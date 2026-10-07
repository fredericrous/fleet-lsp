//! `fleet-lsp serve`: the I/O shell around `core`.
//!
//! Threads: client-stdin reader, child-stdout reader, child-stdin writer,
//! client-stdout writer, and the event loop (this thread). The two directions
//! never share a blocking path:
//!
//! - server → client: the child-stdout reader pushes relayed frames straight
//!   into the client-output queue, drained by the client-stdout writer. The
//!   event loop only hears what those frames *said* (events), so a stalled
//!   client→server direction never stops server output.
//! - client → server: the client-stdin reader fills a byte-bounded queue; the
//!   loop takes a frame only when its outbox to the child is empty, so a
//!   server that stops reading pushes back on the client instead of growing
//!   memory, and is declared hung after `HUNG`.
//!
//! The loop never performs a blocking write. Teardown never waits on a
//! writer: it closes the child's stdin, gives the child `TEARDOWN` to exit,
//! kills it, and returns — process exit ends any writer still blocked.

use crate::cli::{Lang, Version};
use crate::core::{Action, Config, Core, Input, ServerFilter};
use crate::frame::{write_frame_reporting, Frame, FrameReader};
use crate::gate::{Barrier, Signal};
use crate::json::{self, Json};
use crate::log::{tilde, Log};
use crate::queue::{Queue, OWN, SERVER};
use crate::resolve::{self, Resolution, Verdict};
use crate::scan::{scan, Kind};
use std::collections::VecDeque;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const QUEUE_CAP: usize = 16 << 20;
const OWN_CAP: usize = 1 << 20;
const HUNG: Duration = Duration::from_secs(30);
/// rust-analyzer took 7.4 s to exit on stdin EOF (docs/readiness.md).
const TEARDOWN: Duration = Duration::from_secs(10);
/// 2 × the slowest measured cold start (rust-analyzer on lldap, 185 s).
pub(crate) const DEFAULT_CEILING: Duration = Duration::from_secs(370);
/// What the plugin manifest sets as `requestTimeout`: the ceiling + 30 s.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(400);
const TICK: Duration = Duration::from_millis(200);

#[derive(Debug)]
enum Ev {
    /// A client frame is waiting in `client_in`.
    ClientQueued,
    Input(Input),
    ChildStdoutClosed,
    ChildStdinBroken,
    /// A line for the session log from a pipe thread.
    Log(String),
}

pub(crate) fn serve(lang: Lang, min_version: Option<Result<Version, String>>) -> u8 {
    if io::stdin().is_terminal() {
        eprintln!("fleet-lsp serve speaks LSP and is run by Claude Code. Try:\n  fleet-lsp doctor");
        return 2;
    }
    let mut log = Log::open(lang.name());
    let mut client = FrameReader::new(io::stdin());
    // The first frame is `initialize`: it names the session root.
    let first = match client.next_frame() {
        Ok(Some(Frame::Body(b))) => b,
        _ => {
            log.line("client closed before initialize");
            return 1;
        }
    };
    let session_root = session_root(&first);
    let (ceiling, ceiling_note) = ceiling_from_env();
    if let Some(note) = ceiling_note {
        log.line(&note);
    }
    let refusal = version_refusal(min_version);
    let res = if refusal.is_some() {
        None
    } else {
        Some(resolve::resolve(lang, &session_root))
    };
    let mut refusal = refusal.or_else(|| res.as_ref().and_then(Resolution::refusal_text));
    header(&mut log, lang, &session_root, res.as_ref(), ceiling);

    let mut child = None;
    if refusal.is_none() {
        let spawn = res
            .as_ref()
            .and_then(|r| r.spawn.clone())
            .expect("verified implies a spawn");
        let mut cmd = Command::new(&spawn.program);
        cmd.args(&spawn.args)
            .current_dir(&spawn.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for k in &spawn.env_remove {
            cmd.env_remove(k);
        }
        for (k, v) in &spawn.env_set {
            cmd.env(k, v);
        }
        match cmd.spawn() {
            Ok(c) => child = Some(c),
            Err(e) => {
                refusal = Some(format!(
                    "fleet-lsp: {lang}: could not start {}: {e}; fix: none — reinstall the server",
                    spawn.program.display()
                ));
            }
        }
    }
    if let Some(r) = &refusal {
        log.line(&format!("refusing: {r}"));
    }
    let res_barrier = res.as_ref().map_or(Barrier::None, |r| r.barrier.clone());
    let cfg = Config {
        lang: lang.name(),
        barrier: if refusal.is_some() {
            Barrier::None
        } else {
            res_barrier
        },
        refusal,
        ceiling,
        log_path: tilde(log.path()),
        tsserver_path: res.as_ref().and_then(|r| r.tsserver_path.clone()),
        guard_pyright_log_level: lang == Lang::Python,
    };
    Shell::new(cfg, child, client, log).run(first)
}

struct Shell {
    core: Core,
    child: Option<Child>,
    log: Log,
    client_in: Arc<Queue>,
    child_in: Arc<Queue>,
    client_out: Arc<Queue>,
    filter: Arc<Mutex<ServerFilter>>,
    rx: mpsc::Receiver<Ev>,
    tx: Sender<Ev>,
    outbox_child: VecDeque<Vec<u8>>,
    outbox_client: VecDeque<Vec<u8>>,
    outbox_client_bytes: usize,
    pending_client: usize,
    child_stdout_closed: bool,
    exit: Option<u8>,
    client_reader: Option<FrameReader<io::Stdin>>,
}

impl Shell {
    fn new(cfg: Config, child: Option<Child>, client: FrameReader<io::Stdin>, log: Log) -> Shell {
        let (tx, rx) = mpsc::channel();
        Shell {
            core: Core::new(cfg),
            child,
            log,
            client_in: Arc::new(Queue::single(QUEUE_CAP)),
            child_in: Arc::new(Queue::single(QUEUE_CAP)),
            client_out: Arc::new(Queue::two_lane(OWN_CAP, QUEUE_CAP)),
            filter: Arc::new(Mutex::new(ServerFilter::default())),
            rx,
            tx,
            outbox_child: VecDeque::new(),
            outbox_client: VecDeque::new(),
            outbox_client_bytes: 0,
            pending_client: 0,
            child_stdout_closed: false,
            exit: None,
            client_reader: Some(client),
        }
    }

    fn run(mut self, first: Vec<u8>) -> u8 {
        self.start_threads();
        let s = scan(&first);
        self.step(Input::Client {
            body: first,
            scan: s,
        });
        loop {
            self.flush_outboxes();
            if let Some(code) = self.exit {
                return self.teardown(code);
            }
            while self.pending_client > 0 && self.outbox_child.is_empty() && self.exit.is_none() {
                self.pending_client -= 1;
                if let Some(p) = self.client_in.try_pop() {
                    self.client_in.release(p.lane, p.body.len());
                    let s = scan(&p.body);
                    self.step(Input::Client {
                        body: p.body,
                        scan: s,
                    });
                    self.flush_outboxes();
                }
            }
            match self.rx.recv_timeout(TICK) {
                Ok(Ev::ClientQueued) => self.pending_client += 1,
                Ok(Ev::Input(i)) => self.step(i),
                Ok(Ev::ChildStdoutClosed) => self.child_stdout_closed = true,
                Ok(Ev::ChildStdinBroken) => self.child_stdout_closed = true,
                Ok(Ev::Log(line)) => self.log.line(&line),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.step(Input::ClientGone),
            }
            self.housekeeping();
        }
    }

    fn housekeeping(&mut self) {
        let now = Instant::now();
        self.step(Input::Tick);
        if self.child_stdout_closed {
            if let Some(c) = &mut self.child {
                if let Ok(Some(status)) = c.try_wait() {
                    self.child = None;
                    self.step(Input::ChildExited(status.to_string()));
                }
            }
        }
        let child_stuck = self.child_in.stalled_for(now).is_some_and(|d| d >= HUNG);
        if child_stuck && self.exit.is_none() {
            self.step(Input::ChildHung);
        }
        // The client is not reading when no byte has reached it for `HUNG`
        // (the queue's clock counts every chunk written, so a slow reader of
        // one large frame is still reading). Replies of our own piling up
        // past the queue cap behind a full reserved share are the same case.
        let client_stuck = self.outbox_client_bytes > QUEUE_CAP
            || self.client_out.stalled_for(now).is_some_and(|d| d >= HUNG);
        if client_stuck && self.exit.is_none() {
            self.log.line("the client stopped reading its input");
            self.step(Input::ClientGone);
        }
    }

    fn step(&mut self, input: Input) {
        for action in self.core.step(input, Instant::now()) {
            match action {
                Action::ToChild(b) => {
                    if self.child.is_some() {
                        self.outbox_child.push_back(b);
                    }
                }
                Action::ToClient(b) => {
                    self.outbox_client_bytes += b.len();
                    self.outbox_client.push_back(b);
                }
                Action::SetFilter(f) => *self.filter.lock().unwrap_or_else(|p| p.into_inner()) = f,
                Action::Log(s) => self.log.line(&s),
                // After a forwarded `exit` the child should leave on its own;
                // either way teardown gives it the same grace, then kills it.
                Action::Exit(code) | Action::AwaitChildThenExit(code) => self.exit = Some(code),
            }
        }
    }

    fn flush_outboxes(&mut self) {
        while let Some(b) = self.outbox_child.pop_front() {
            if let Err(b) = self.child_in.try_push(0, b) {
                self.outbox_child.push_front(b);
                break;
            }
        }
        while let Some(b) = self.outbox_client.pop_front() {
            let len = b.len();
            if let Err(b) = self.client_out.try_push(OWN, b) {
                self.outbox_client.push_front(b);
                break;
            }
            self.outbox_client_bytes -= len;
        }
    }

    fn teardown(&mut self, code: u8) -> u8 {
        self.log.line(&format!("teardown, exit {code}"));
        // What is already queued for the child (an `exit`) goes first; a
        // full outbox is abandoned — the child is going away.
        let deadline = Instant::now() + Duration::from_secs(1);
        while !self.outbox_child.is_empty() && Instant::now() < deadline {
            self.flush_outboxes();
            thread::sleep(Duration::from_millis(10));
        }
        self.child_in.close();
        if let Some(mut c) = self.child.take() {
            let deadline = Instant::now() + TEARDOWN;
            loop {
                match c.try_wait() {
                    Ok(Some(status)) => {
                        self.log.line(&format!("server exited: {status}"));
                        break;
                    }
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(50))
                    }
                    _ => {
                        self.log.line("server did not exit; killed");
                        let _ = c.kill();
                        let _ = c.wait();
                        break;
                    }
                }
            }
        }
        // Give our own last replies (a shutdown result, refusals) a moment.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && (!self.outbox_client.is_empty() || !self.client_out.is_drained())
        {
            self.flush_outboxes();
            thread::sleep(Duration::from_millis(10));
        }
        code
    }

    fn start_threads(&mut self) {
        // client stdin → client_in
        let reader = self.client_reader.take().expect("started once");
        let (q, tx) = (Arc::clone(&self.client_in), self.tx.clone());
        thread::spawn(move || client_reader(reader, &q, &tx));
        // client_out → client stdout
        let (q, tx) = (Arc::clone(&self.client_out), self.tx.clone());
        thread::spawn(move || client_writer(&q, &tx));
        if let Some(c) = &mut self.child {
            let stdin = c.stdin.take().expect("piped");
            let stdout = c.stdout.take().expect("piped");
            let (q, tx) = (Arc::clone(&self.child_in), self.tx.clone());
            thread::spawn(move || child_writer(stdin, &q, &tx));
            let (q, tx, f) = (
                Arc::clone(&self.client_out),
                self.tx.clone(),
                Arc::clone(&self.filter),
            );
            thread::spawn(move || child_reader(stdout, &q, &tx, &f));
        }
    }
}

fn client_reader(mut reader: FrameReader<io::Stdin>, q: &Queue, tx: &Sender<Ev>) {
    loop {
        match reader.next_frame() {
            Ok(Some(Frame::Body(b))) => {
                if q.push(0, b).is_err() || tx.send(Ev::ClientQueued).is_err() {
                    return;
                }
            }
            Ok(Some(Frame::Oversize { len, scan })) => {
                if tx
                    .send(Ev::Input(Input::ClientOversize { len, scan }))
                    .is_err()
                {
                    return;
                }
            }
            Ok(None) | Err(_) => {
                let _ = tx.send(Ev::Input(Input::ClientGone));
                return;
            }
        }
    }
}

fn client_writer(q: &Queue, tx: &Sender<Ev>) {
    let stdout = io::stdout();
    while let Some(p) = q.pop() {
        let mut lock = stdout.lock();
        let ok = write_frame_reporting(&mut lock, &p.body, || q.progress()).is_ok();
        drop(lock);
        q.release(p.lane, p.body.len());
        if !ok {
            let _ = tx.send(Ev::Input(Input::ClientGone));
            return;
        }
    }
}

fn child_writer(mut stdin: ChildStdin, q: &Queue, tx: &Sender<Ev>) {
    while let Some(p) = q.pop() {
        let ok = write_frame_reporting(&mut stdin, &p.body, || q.progress()).is_ok();
        q.release(p.lane, p.body.len());
        if !ok {
            let _ = tx.send(Ev::ChildStdinBroken);
            return;
        }
    }
    // Closed and drained: dropping stdin is the child's EOF.
}

fn child_reader(stdout: ChildStdout, out: &Queue, tx: &Sender<Ev>, filter: &Mutex<ServerFilter>) {
    let mut reader = FrameReader::new(stdout);
    loop {
        let body = match reader.next_frame() {
            Ok(Some(Frame::Body(b))) => b,
            Ok(Some(Frame::Oversize { scan, .. })) => {
                let _ = tx.send(Ev::Input(Input::ServerOversize(scan)));
                continue;
            }
            Ok(None) | Err(_) => {
                let _ = tx.send(Ev::ChildStdoutClosed);
                return;
            }
        };
        let s = scan(&body);
        let f = *filter.lock().unwrap_or_else(|p| p.into_inner());
        let method = s.method.as_deref().unwrap_or("");
        let mut forward = true;
        match s.kind() {
            Kind::Notification => match method {
                "experimental/serverStatus" => {
                    match status_signal(&body) {
                        Some(sig) => {
                            let _ = tx.send(Ev::Input(Input::ServerSignal(sig)));
                        }
                        None => {
                            let _ = tx.send(Ev::Log(format!(
                                "unreadable experimental/serverStatus frame ({} bytes); the gate did not see it",
                                body.len()
                            )));
                        }
                    }
                    forward = !f.swallow_status;
                }
                "$/progress" => {
                    if let Some(sig) = progress_signal(&body) {
                        let _ = tx.send(Ev::Input(Input::ServerSignal(sig)));
                    }
                    forward = !f.own_progress;
                }
                "window/logMessage" => {
                    if let Some(text) = param_str(&body, "message") {
                        let _ = tx.send(Ev::Input(Input::ServerSignal(Signal::Log(text))));
                    }
                }
                _ => {}
            },
            Kind::Request => {
                let id = s.id.clone().expect("a request has an id");
                let keep = (method == "workspace/configuration").then(|| body.clone());
                let _ = tx.send(Ev::Input(Input::ServerRequest {
                    id,
                    method: method.to_string(),
                    body: keep,
                }));
                forward = !(method == "window/workDoneProgress/create" && f.own_progress);
            }
            Kind::Response => {
                if let Some(id) = s.id.clone() {
                    let _ = tx.send(Ev::Input(Input::ServerResponse(id)));
                }
            }
            Kind::Other => {}
        }
        if forward && out.push(SERVER, body).is_err() {
            return;
        }
    }
}

fn parse_small(body: &[u8]) -> Option<Json> {
    if body.len() > (1 << 20) {
        return None;
    }
    json::parse(std::str::from_utf8(body).ok()?).ok()
}

fn param_str(body: &[u8], key: &str) -> Option<String> {
    let m = parse_small(body)?;
    m.get("params")?.get(key)?.as_str().map(str::to_string)
}

fn status_signal(body: &[u8]) -> Option<Signal> {
    let m = parse_small(body)?;
    let p = m.get("params")?;
    Some(Signal::Status {
        health: p
            .get("health")
            .and_then(Json::as_str)
            .unwrap_or("ok")
            .to_string(),
        quiescent: p.get("quiescent") == Some(&Json::Bool(true)),
        message: p.get("message").and_then(Json::as_str).map(str::to_string),
    })
}

fn progress_signal(body: &[u8]) -> Option<Signal> {
    let m = parse_small(body)?;
    let p = m.get("params")?;
    let token = match p.get("token")? {
        Json::Str(s) => s.clone(),
        other => other.to_string(),
    };
    let value = p.get("value")?;
    match value.get("kind").and_then(Json::as_str)? {
        "begin" => Some(Signal::ProgressBegin {
            token,
            title: value
                .get("title")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string(),
        }),
        "end" => Some(Signal::ProgressEnd { token }),
        _ => None,
    }
}

/// `rootUri`, else the first workspace folder, else the process cwd.
fn session_root(initialize: &[u8]) -> PathBuf {
    let from_init = parse_small(initialize).and_then(|m| {
        let p = m.get("params")?;
        p.get("rootUri")
            .and_then(Json::as_str)
            .map(str::to_string)
            .or_else(|| {
                p.get("workspaceFolders")?
                    .as_arr()?
                    .first()?
                    .get("uri")?
                    .as_str()
                    .map(str::to_string)
            })
            .and_then(|u| file_uri_path(&u))
            .or_else(|| p.get("rootPath").and_then(Json::as_str).map(PathBuf::from))
    });
    from_init
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

fn ceiling_from_env() -> (Duration, Option<String>) {
    match std::env::var("FLEET_LSP_CEILING_MS") {
        Err(_) => (DEFAULT_CEILING, None),
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(ms) if ms > 0 => {
                let d = Duration::from_millis(ms);
                let note = (d >= REQUEST_TIMEOUT).then(|| {
                    format!(
                        "warning: FLEET_LSP_CEILING_MS={ms} reaches the plugin's requestTimeout ({}s); Claude Code may give up first",
                        REQUEST_TIMEOUT.as_secs()
                    )
                });
                (d, note)
            }
            _ => (
                DEFAULT_CEILING,
                Some(format!(
                    "FLEET_LSP_CEILING_MS={v:?} is not a positive number; using {}s",
                    DEFAULT_CEILING.as_secs()
                )),
            ),
        },
    }
}

fn version_refusal(min: Option<Result<Version, String>>) -> Option<String> {
    let own = Version::own();
    match min? {
        Ok(min) if min > own => Some(format!(
            "fleet-lsp {own} is older than the plugin requires ({min}); fix: brew upgrade fleet-lsp"
        )),
        Ok(_) => None,
        Err(raw) => Some(format!(
            "the plugin passed --min-version {raw:?}, which is not x.y.z; fix: none — reinstall the fleet-lsp plugin"
        )),
    }
}

fn header(
    log: &mut Log,
    lang: Lang,
    session_root: &std::path::Path,
    res: Option<&Resolution>,
    ceiling: Duration,
) {
    let mut line = format!(
        "fleet-lsp {} serve {lang}: session root {}",
        env!("CARGO_PKG_VERSION"),
        tilde(session_root)
    );
    if let Some(r) = res {
        let opt = |p: &Option<PathBuf>| p.as_deref().map_or_else(|| "-".into(), tilde);
        line.push_str(&format!(
            "; git root {}; project root {}; pin {}; server {}; version {}; verdict {}; barrier {:?}{}; ceiling {}s",
            opt(&r.git_root),
            opt(&r.project_root),
            r.pin.as_deref().unwrap_or("-"),
            opt(&r.server),
            r.version.as_deref().unwrap_or("-"),
            match &r.verdict {
                Verdict::Verified => "verified".to_string(),
                Verdict::Refused { reason, .. } => format!("refused ({reason})"),
            },
            r.barrier,
            if r.narrowed { " (narrowed: barrier not measured for this version)" } else { "" },
            ceiling.as_secs()
        ));
    }
    log.line(&line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_decode() {
        assert_eq!(
            file_uri_path("file:///Users/a/b%20c"),
            Some(PathBuf::from("/Users/a/b c"))
        );
        assert_eq!(
            file_uri_path("file:///x/%C3%A9"),
            Some(PathBuf::from("/x/é"))
        );
        assert_eq!(file_uri_path("http://x"), None);
    }

    #[test]
    fn session_root_prefers_root_uri_then_folders() {
        let a = br#"{"id":0,"method":"initialize","params":{"rootUri":"file:///r/a","workspaceFolders":[{"uri":"file:///r/b"}]}}"#;
        assert_eq!(session_root(a), PathBuf::from("/r/a"));
        let b = br#"{"id":0,"method":"initialize","params":{"rootUri":null,"workspaceFolders":[{"uri":"file:///r/b"}]}}"#;
        assert_eq!(session_root(b), PathBuf::from("/r/b"));
    }

    #[test]
    fn newer_plugin_is_an_upgrade_refusal() {
        let r = version_refusal(Some(Ok(Version(99, 0, 0)))).unwrap();
        assert!(r.contains("brew upgrade fleet-lsp"));
        assert!(version_refusal(Some(Ok(Version(0, 0, 1)))).is_none());
        assert!(version_refusal(None).is_none());
    }
}
