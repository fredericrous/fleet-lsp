//! Workspace mode: a session started outside any git repository serves
//! every repository it touches. Each request is routed by the document it
//! names (`route`) to the child of that document's project root, started
//! on its first request and gated by its own `Core`, exactly as a
//! single-root session gates its one child.
//!
//! The shell answers `initialize` and `shutdown` itself and owns `exit`;
//! a child's own `initialize` and `shutdown` carry fleet-lsp's ids, whose
//! replies never reach the client. Threads: the client's reader and writer
//! (shared with `relay`), and per child a stdin writer, a stdout reader and
//! a stderr reader that writes the child's own log.

use crate::cli::Lang;
use crate::core::{self, Action, ChildScope, Config, Core, Input, ServerFilter, REQUEST_FAILED};
use crate::frame::{Frame, FrameReader};
use crate::json::{self, Json};
use crate::log::{tilde, Log};
use crate::queue::{Queue, OWN, SERVER};
use crate::relay::{self, Ev, HUNG, OWN_CAP, QUEUE_CAP, TEARDOWN, TICK};
use crate::resolve::{self, Located, Resolution};
use crate::route::{self, refusal, Applied, Documents, Recent};
use crate::scan::{scan, Id, Kind};
use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Live children per language when `FLEET_LSP_MAX_CHILDREN` is unset:
/// rust-analyzer alone can hold over 1 GB per repository.
pub(crate) const DEFAULT_MAX_CHILDREN: usize = 4;
/// A root whose child died this often (the first start and the plugin's
/// `maxRestarts` of 2) is refused; an eviction is not a death.
const MAX_DEATHS: u32 = 3;
/// How long the client's `shutdown` waits for every child's reply.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(10);

/// What the loop hears.
enum Event {
    Client(Ev),
    Slot(u64, SlotEvent),
}

#[derive(Debug)]
enum SlotEvent {
    Input(Input),
    /// The reply to one of fleet-lsp's own requests, with its body.
    OwnReply(Id, Vec<u8>),
    StdoutClosed,
    StdinBroken,
    Log(String),
}

/// Where a child is in its life.
#[derive(Debug)]
enum Phase {
    /// fleet-lsp's `initialize` is out: what the child is sent waits until
    /// its reply, as the protocol requires.
    Starting {
        init: Id,
        waiting: Vec<Vec<u8>>,
    },
    Live,
    /// Shut down by fleet-lsp (an eviction); killed after `TEARDOWN`.
    Closing {
        since: Instant,
    },
}

struct Slot {
    root: PathBuf,
    repo: String,
    core: Core,
    child: Child,
    child_in: Arc<Queue>,
    outbox: Vec<Vec<u8>>,
    phase: Phase,
    stdout_closed: bool,
    stdin_broken: bool,
    log_path: String,
}

/// What is known about a project root.
// holds-until: a pin or a server is fixed mid-session (`uv sync`); until
// the next session the refusal stands, as a single-root session's does.
enum RootState {
    Refused(String),
    Resolved { res: Box<Resolution>, deaths: u32 },
}

/// The client's `shutdown`, waiting on the children's replies.
struct Shutdown {
    client_id: Option<Id>,
    waiting: HashMap<u64, Id>,
    deadline: Instant,
}

pub(crate) struct Settings {
    pub(crate) lang: Lang,
    pub(crate) ceiling: Duration,
    pub(crate) max_children: usize,
}

/// `FLEET_LSP_MAX_CHILDREN`, validated: the cap, and a note for the log
/// when the value was not usable.
pub(crate) fn max_children_from_env() -> (usize, Option<String>) {
    match std::env::var("FLEET_LSP_MAX_CHILDREN") {
        Err(_) => (DEFAULT_MAX_CHILDREN, None),
        Ok(v) => match v.trim().parse::<usize>() {
            Ok(n) if n > 0 => (n, None),
            _ => (
                DEFAULT_MAX_CHILDREN,
                Some(format!(
                    "FLEET_LSP_MAX_CHILDREN={v:?} is not a positive number; using {DEFAULT_MAX_CHILDREN}"
                )),
            ),
        },
    }
}

pub(crate) fn serve(
    settings: Settings,
    log: Log,
    client: FrameReader<io::Stdin>,
    first: Vec<u8>,
) -> u8 {
    Workspace::new(settings, log).run(client, first)
}

struct Workspace {
    settings: Settings,
    log: Log,
    /// The client's `initialize`, which each child gets with its own root.
    init: Json,
    client_in: Arc<Queue>,
    client_out: Arc<Queue>,
    rx: mpsc::Receiver<Event>,
    tx: Sender<Event>,
    pending_client: usize,
    outbox_client: Vec<Vec<u8>>,
    outbox_client_bytes: usize,
    slots: BTreeMap<u64, Slot>,
    /// Project root → the slot serving it (starting or live).
    serving: HashMap<PathBuf, u64>,
    roots: HashMap<PathBuf, RootState>,
    // holds-until: a session's repositories keep their layout; a project
    // added under an already-located directory mid-session is seen by the
    // next session (the cache is per process).
    located: HashMap<PathBuf, Located>,
    /// Project root → its latest child's log, named when it died too often.
    last_log: HashMap<PathBuf, String>,
    documents: Documents,
    recent: Recent<PathBuf>,
    /// Client request id → the slot it went to, for `$/cancelRequest`.
    client_ids: HashMap<Id, u64>,
    next_slot: u64,
    next_own: u64,
    shutdown: Option<Shutdown>,
    shutdown_seen: bool,
    exit_seen: bool,
    exit: Option<u8>,
}

impl Workspace {
    fn new(settings: Settings, log: Log) -> Workspace {
        let (tx, rx) = mpsc::channel();
        Workspace {
            settings,
            log,
            init: Json::obj(),
            client_in: Arc::new(Queue::single(QUEUE_CAP)),
            client_out: Arc::new(Queue::two_lane(OWN_CAP, QUEUE_CAP)),
            rx,
            tx,
            pending_client: 0,
            outbox_client: Vec::new(),
            outbox_client_bytes: 0,
            slots: BTreeMap::new(),
            serving: HashMap::new(),
            roots: HashMap::new(),
            located: HashMap::new(),
            last_log: HashMap::new(),
            documents: Documents::default(),
            recent: Recent::default(),
            client_ids: HashMap::new(),
            next_slot: 1,
            next_own: 1,
            shutdown: None,
            shutdown_seen: false,
            exit_seen: false,
            exit: None,
        }
    }

    fn lang(&self) -> &'static str {
        self.settings.lang.name()
    }

    fn run(mut self, client: FrameReader<io::Stdin>, first: Vec<u8>) -> u8 {
        let (q, tx) = (Arc::clone(&self.client_in), self.tx.clone());
        thread::spawn(move || {
            relay::client_reader(client, &q, |e| tx.send(Event::Client(e)).is_ok())
        });
        let (q, tx) = (Arc::clone(&self.client_out), self.tx.clone());
        thread::spawn(move || relay::client_writer(&q, |e| tx.send(Event::Client(e)).is_ok()));
        self.on_client(first);
        loop {
            self.flush_outboxes();
            if let Some(code) = self.exit {
                return self.teardown(code);
            }
            // A client frame is taken only once every child has taken what
            // it was sent: a child that stops reading pushes back on the
            // client instead of growing memory.
            while self.pending_client > 0
                && self.slots.values().all(|s| s.outbox.is_empty())
                && self.exit.is_none()
            {
                self.pending_client -= 1;
                if let Some(p) = self.client_in.try_pop() {
                    self.client_in.release(p.lane, p.body.len());
                    self.on_client(p.body);
                    self.flush_outboxes();
                }
            }
            match self.rx.recv_timeout(TICK) {
                Ok(Event::Client(ev)) => self.on_client_event(ev),
                Ok(Event::Slot(slot, ev)) => self.on_slot_event(slot, ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.client_gone(),
            }
            self.housekeeping();
        }
    }

    // ---------------------------------------------------------------- client

    fn on_client_event(&mut self, ev: Ev) {
        match ev {
            Ev::ClientQueued => self.pending_client += 1,
            Ev::Input(Input::ClientOversize { len, scan }) => {
                let msg = format!(
                    "fleet-lsp: frame too large ({} MiB, limit {} MiB)",
                    len >> 20,
                    crate::frame::MAX_FRAME >> 20
                );
                self.log.line(&msg);
                if scan.kind() == Kind::Request {
                    self.send_to_client(core::error(scan.id.as_ref(), REQUEST_FAILED, &msg));
                } else {
                    // Dropping a notification would desync a server's view
                    // of a file, as in single-root mode.
                    self.exit = Some(1);
                }
            }
            Ev::Input(Input::ClientGone) => self.client_gone(),
            Ev::Input(other) => self.log.line(&format!("unexpected client event {other:?}")),
            Ev::ChildStdoutClosed | Ev::ChildStdinBroken => {}
            Ev::Log(line) => self.log.line(&line),
        }
    }

    fn client_gone(&mut self) {
        if self.exit.is_none() {
            self.exit = Some(if self.exit_seen && self.shutdown_seen {
                0
            } else {
                1
            });
        }
    }

    fn on_client(&mut self, body: Vec<u8>) {
        let s = scan(&body);
        let method = s.method.clone().unwrap_or_default();
        match s.kind() {
            Kind::Request => match method.as_str() {
                "initialize" => self.initialize(&body, s.id.as_ref()),
                "shutdown" => self.begin_shutdown(s.id),
                _ => self.request(body, s.id.as_ref()),
            },
            Kind::Notification => match method.as_str() {
                "exit" => self.exit_now(),
                // Each child gets its own, after its own `initialize`.
                "initialized" => {}
                "$/cancelRequest" => self.cancel(body),
                m if m.starts_with("textDocument/did") => self.document(body, m),
                _ => self.broadcast(&body),
            },
            Kind::Response => self.client_response(&body, s.id.as_ref()),
            Kind::Other => self.broadcast(&body),
        }
    }

    fn initialize(&mut self, body: &[u8], id: Option<&Id>) {
        match parse(body) {
            Some(msg) => self.init = msg,
            None => self
                .log
                .line("the client's initialize is not JSON; children get an empty one"),
        }
        self.send_to_client(core::result(
            id,
            &core::capabilities("fleet-lsp (workspace)"),
        ));
    }

    fn request(&mut self, body: Vec<u8>, id: Option<&Id>) {
        let msg = parse(&body);
        let root = match msg.as_ref().and_then(route::document_uri) {
            Some(uri) => self.root_of(uri),
            None => {
                let latest = self.recent.latest().cloned();
                if let Some(root) = &latest {
                    self.log.line(&format!(
                        "{} → {} (most recently used)",
                        scan(&body).method.unwrap_or_default(),
                        tilde(root)
                    ));
                }
                latest.ok_or_else(|| refusal::no_document(self.lang()))
            }
        };
        let slot = root.and_then(|root| {
            let slot = self.slot_for(&root)?;
            self.recent.touch(&root);
            Ok(slot)
        });
        match slot {
            Ok(slot) => {
                if let Some(id) = id {
                    self.client_ids.insert(id.clone(), slot);
                }
                let s = scan(&body);
                self.step_slot(slot, Input::Client { body, scan: s });
            }
            Err(text) => self.send_to_client(core::error(id, REQUEST_FAILED, &text)),
        }
    }

    /// A `didOpen`/`didChange`/`didClose`: always folded into the store; a
    /// live child of its root also gets it. A notification never starts a
    /// child, and one starting gets the store replayed once it is up.
    fn document(&mut self, body: Vec<u8>, method: &str) {
        let Some(msg) = parse(&body) else {
            self.log.line(&format!("{method}: not JSON; dropped"));
            return;
        };
        if let Applied::Unfollowed(why) = self.documents.apply(method, &msg) {
            self.log.line(&format!("document store: {why}"));
        }
        let Some(uri) = route::document_uri(&msg).map(str::to_string) else {
            return;
        };
        let Ok(root) = self.root_of(&uri) else { return };
        let live = self
            .serving
            .get(&root)
            .copied()
            .filter(|slot| matches!(self.slots.get(slot).map(|s| &s.phase), Some(Phase::Live)));
        if let Some(slot) = live {
            let s = scan(&body);
            self.step_slot(slot, Input::Client { body, scan: s });
        }
    }

    fn cancel(&mut self, body: Vec<u8>) {
        let target = parse(&body).and_then(|m| {
            m.get("params")
                .and_then(|p| p.get("id"))
                .and_then(route::id_of)
        });
        let slot = target.and_then(|id| self.client_ids.get(&id).copied());
        if let Some(slot) = slot {
            let s = scan(&body);
            self.step_slot(slot, Input::Client { body, scan: s });
        }
    }

    /// The client answering a child's request: back to that child, under
    /// the child's own id.
    fn client_response(&mut self, body: &[u8], id: Option<&Id>) {
        let Some((slot, original)) = id.and_then(route::from_client_facing) else {
            self.log
                .line("a client response to no request of a child; dropped");
            return;
        };
        let Some(msg) = parse(body) else {
            self.log.line("a client response that is not JSON; dropped");
            return;
        };
        if !self.slots.contains_key(&slot) {
            return;
        }
        let body = route::with_id(msg, &original).to_string().into_bytes();
        let s = scan(&body);
        self.step_slot(slot, Input::Client { body, scan: s });
    }

    /// A notification about no document (configuration, trace): every child
    /// that is up hears it.
    fn broadcast(&mut self, body: &[u8]) {
        let live: Vec<u64> = self
            .slots
            .iter()
            .filter(|(_, s)| matches!(s.phase, Phase::Live))
            .map(|(id, _)| *id)
            .collect();
        for slot in live {
            let body = body.to_vec();
            let s = scan(&body);
            self.step_slot(slot, Input::Client { body, scan: s });
        }
    }

    fn begin_shutdown(&mut self, client_id: Option<Id>) {
        self.shutdown_seen = true;
        let mut waiting = HashMap::new();
        let slots: Vec<u64> = self.slots.keys().copied().collect();
        for slot in slots {
            let drained = self
                .slots
                .get_mut(&slot)
                .map(|s| s.core.drain_for_shutdown());
            self.apply(slot, drained.unwrap_or_default());
            if matches!(self.slots.get(&slot).map(|s| &s.phase), Some(Phase::Live)) {
                let id = self.own_request(slot, "shutdown");
                waiting.insert(slot, id);
            }
        }
        self.shutdown = Some(Shutdown {
            client_id,
            waiting,
            deadline: Instant::now() + SHUTDOWN_WAIT,
        });
        self.answer_shutdown_when_done(Instant::now());
    }

    fn answer_shutdown_when_done(&mut self, now: Instant) {
        let done = self
            .shutdown
            .as_ref()
            .is_some_and(|s| s.waiting.is_empty() || now >= s.deadline);
        if done {
            if let Some(s) = self.shutdown.take() {
                if !s.waiting.is_empty() {
                    self.log.line(&format!(
                        "shutdown answered after {}s without {} child(ren)'s reply",
                        SHUTDOWN_WAIT.as_secs(),
                        s.waiting.len()
                    ));
                }
                self.send_to_client(core::result(s.client_id.as_ref(), "null"));
            }
        }
    }

    fn exit_now(&mut self) {
        self.exit_seen = true;
        let exit = notification("exit");
        for slot in self.slots.values_mut() {
            slot.outbox.push(exit.clone());
        }
        self.exit = Some(if self.shutdown_seen { 0 } else { 1 });
    }

    // ---------------------------------------------------------------- roots

    /// The project root a document belongs to, or the refusal that says why
    /// it has none.
    fn root_of(&mut self, uri: &str) -> Result<PathBuf, String> {
        let lang = self.lang();
        let Some(path) = relay::file_uri_path(uri) else {
            return Err(refusal::no_repository(lang, uri));
        };
        let dir = path.parent().unwrap_or(&path).to_path_buf();
        let settings_lang = self.settings.lang;
        let located = self
            .located
            .entry(dir.clone())
            .or_insert_with(|| resolve::locate(settings_lang, &dir))
            .clone();
        match located {
            Located::Project { project_root, .. } => Ok(project_root),
            Located::NoRepository => Err(refusal::no_repository(lang, &tilde(&path))),
            Located::Several { git_root, projects } => {
                Err(refusal::several(lang, &tilde(&git_root), &projects))
            }
            Located::Nothing { git_root } => Err(refusal::nothing(lang, &tilde(&git_root))),
        }
    }

    /// The slot serving `root`, started if need be.
    fn slot_for(&mut self, root: &Path) -> Result<u64, String> {
        if let Some(slot) = self.serving.get(root) {
            return Ok(*slot);
        }
        let lang = self.settings.lang;
        let state = self.roots.entry(root.to_path_buf()).or_insert_with(|| {
            let res = resolve::resolve(lang, root);
            let repo = tilde(res.git_root.as_deref().unwrap_or(root));
            match res.refusal_text() {
                Some(text) => RootState::Refused(refusal::of_repository(lang.name(), &repo, &text)),
                None => RootState::Resolved {
                    res: Box::new(res),
                    deaths: 0,
                },
            }
        });
        let res = match state {
            RootState::Refused(text) => return Err(text.clone()),
            RootState::Resolved { res, deaths } if *deaths >= MAX_DEATHS => {
                let repo = tilde(res.git_root.as_deref().unwrap_or(root));
                let log = self
                    .last_log
                    .get(root)
                    .cloned()
                    .unwrap_or_else(|| tilde(self.log.path()));
                return Err(refusal::restarts_spent(lang.name(), &repo, *deaths, &log));
            }
            RootState::Resolved { res, .. } => (**res).clone(),
        };
        self.make_room_for(root);
        self.start(root, &res)
    }

    /// Over the cap, the least recently used child is shut down.
    fn make_room_for(&mut self, root: &Path) {
        if self.serving.len() < self.settings.max_children {
            return;
        }
        // A child still starting is never evicted: what waits for its
        // `initialize` would have nowhere to go.
        // holds-until: children start one per request and answer
        // `initialize` within seconds, so the cap is exceeded by at most the
        // children still starting (one per repository asked about at once);
        // a session that opens many repositories in the same seconds would
        // need starts queued behind the cap instead.
        let (serving, slots) = (&self.serving, &self.slots);
        let live = |r: &PathBuf| {
            serving
                .get(r)
                .and_then(|slot| slots.get(slot))
                .is_some_and(|s| matches!(s.phase, Phase::Live))
        };
        let Some(victim) = self.recent.evictable(live, &root.to_path_buf()).cloned() else {
            return;
        };
        let Some(slot) = self.serving.remove(&victim) else {
            return;
        };
        self.log.line(&format!(
            "evicted {} (cap {})",
            tilde(&victim),
            self.settings.max_children
        ));
        let drained = self
            .slots
            .get_mut(&slot)
            .map(|s| s.core.drain_for_shutdown());
        self.apply(slot, drained.unwrap_or_default());
        if matches!(self.slots.get(&slot).map(|s| &s.phase), Some(Phase::Live)) {
            self.own_request(slot, "shutdown");
            if let Some(s) = self.slots.get_mut(&slot) {
                s.outbox.push(notification("exit"));
            }
        }
        if let Some(s) = self.slots.get_mut(&slot) {
            s.phase = Phase::Closing {
                since: Instant::now(),
            };
        }
    }

    fn start(&mut self, root: &Path, res: &Resolution) -> Result<u64, String> {
        let lang = self.settings.lang;
        let git_root = res.git_root.clone().unwrap_or_else(|| root.to_path_buf());
        let repo = tilde(&git_root);
        let spawn = res.spawn.clone().expect("verified implies a spawn");
        let slot = self.next_slot;
        self.next_slot += 1;
        let name = git_root
            .file_name()
            .map_or_else(|| "repo".to_string(), |n| n.to_string_lossy().into_owned());
        let child_log = self.log.child(&name, slot);
        let log_path = tilde(child_log.path());
        self.last_log.insert(root.to_path_buf(), log_path.clone());
        let mut cmd = Command::new(&spawn.program);
        cmd.args(&spawn.args)
            .current_dir(&spawn.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for k in &spawn.env_remove {
            cmd.env_remove(k);
        }
        for (k, v) in &spawn.env_set {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().map_err(|e| {
            let text = format!(
                "fleet-lsp: {}: could not start {}: {e}; fix: none — reinstall the server",
                lang.name(),
                spawn.program.display()
            );
            self.log.line(&text);
            refusal::of_repository(lang.name(), &repo, &text)
        })?;
        let cfg = Config {
            lang: lang.name(),
            barrier: res.barrier.clone(),
            refusal: None,
            ceiling: self.settings.ceiling,
            log_path: log_path.clone(),
            tsserver_path: res.tsserver_path.clone(),
            guard_pyright_log_level: lang == Lang::Python,
            scope: ChildScope::Slot,
        };
        let child_in = Arc::new(Queue::single(QUEUE_CAP));
        let filter = Arc::new(Mutex::new(ServerFilter::default()));
        self.start_threads(slot, &mut child, &child_in, &filter, child_log);
        let init_id = route::own_id(self.next_own);
        self.next_own += 1;
        let (init, decided) =
            core::prepare_initialize(&cfg, child_init(&self.init, &init_id, root));
        *filter.lock().unwrap_or_else(|p| p.into_inner()) = decided;
        let mut core = Core::new(cfg);
        core.adopt_filter(decided);
        self.log.line(&format!(
            "start {repo} (slot {slot}): project {}; server {} {}; log {log_path}",
            tilde(root),
            spawn.program.display(),
            res.version.as_deref().unwrap_or("-"),
        ));
        self.slots.insert(
            slot,
            Slot {
                root: root.to_path_buf(),
                repo,
                core,
                child,
                child_in,
                outbox: vec![init.to_string().into_bytes()],
                phase: Phase::Starting {
                    init: init_id,
                    waiting: Vec::new(),
                },
                stdout_closed: false,
                stdin_broken: false,
                log_path,
            },
        );
        self.serving.insert(root.to_path_buf(), slot);
        Ok(slot)
    }

    fn start_threads(
        &self,
        slot: u64,
        child: &mut Child,
        child_in: &Arc<Queue>,
        filter: &Arc<Mutex<ServerFilter>>,
        child_log: Log,
    ) {
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let (q, tx) = (Arc::clone(child_in), self.tx.clone());
        thread::spawn(move || {
            relay::child_writer(stdin, &q, |e| {
                let ev = match e {
                    Ev::ChildStdinBroken => SlotEvent::StdinBroken,
                    other => SlotEvent::Log(format!("unexpected writer event {other:?}")),
                };
                tx.send(Event::Slot(slot, ev)).is_ok()
            })
        });
        let (q, tx, f) = (
            Arc::clone(&self.client_out),
            self.tx.clone(),
            Arc::clone(filter),
        );
        thread::spawn(move || {
            slot_reader(slot, stdout, &q, &f, |ev| {
                tx.send(Event::Slot(slot, ev)).is_ok()
            })
        });
        thread::spawn(move || stderr_to_log(stderr, child_log));
    }

    /// Sends one of fleet-lsp's own requests to a slot's child.
    fn own_request(&mut self, slot: u64, method: &str) -> Id {
        let id = route::own_id(self.next_own);
        self.next_own += 1;
        let body = route::with_id(Json::obj().set("jsonrpc", "2.0").set("method", method), &id)
            .to_string()
            .into_bytes();
        if let Some(s) = self.slots.get_mut(&slot) {
            s.outbox.push(body);
        }
        id
    }

    // ---------------------------------------------------------------- slots

    fn on_slot_event(&mut self, slot: u64, ev: SlotEvent) {
        match ev {
            SlotEvent::Input(input) => {
                if let Input::ServerResponse(id) = &input {
                    self.client_ids.remove(id);
                }
                self.step_slot(slot, input);
            }
            SlotEvent::OwnReply(id, body) => self.own_reply(slot, &id, &body),
            SlotEvent::StdoutClosed => {
                if let Some(s) = self.slots.get_mut(&slot) {
                    s.stdout_closed = true;
                }
            }
            SlotEvent::StdinBroken => {
                if let Some(s) = self.slots.get_mut(&slot) {
                    s.stdin_broken = true;
                }
            }
            SlotEvent::Log(line) => {
                let repo = self
                    .slots
                    .get(&slot)
                    .map_or("?", |s| s.repo.as_str())
                    .to_string();
                self.log.line(&format!("[{repo}] {line}"));
            }
        }
    }

    fn own_reply(&mut self, slot: u64, id: &Id, body: &[u8]) {
        let answered_init = matches!(
            self.slots.get(&slot).map(|s| &s.phase),
            Some(Phase::Starting { init, .. }) if init == id
        );
        if answered_init {
            self.child_initialized(slot, body);
            return;
        }
        if let Some(s) = &mut self.shutdown {
            if s.waiting.get(&slot) == Some(id) {
                s.waiting.remove(&slot);
            }
        }
        self.answer_shutdown_when_done(Instant::now());
    }

    /// The child answered fleet-lsp's `initialize`: `initialized`, the open
    /// documents of its root, then what waited.
    fn child_initialized(&mut self, slot: u64, body: &[u8]) {
        let failure = parse(body).and_then(|m| {
            m.get("error").map(|e| {
                e.get("message")
                    .and_then(Json::as_str)
                    .unwrap_or("no message")
                    .to_string()
            })
        });
        let Some(root) = self.slots.get(&slot).map(|s| s.root.clone()) else {
            return;
        };
        if let Some(message) = failure {
            let repo = self
                .slots
                .get(&slot)
                .map(|s| s.repo.clone())
                .unwrap_or_default();
            let text = refusal::of_repository(
                self.lang(),
                &repo,
                &format!("fleet-lsp: {}: the server refused initialize: {message}; fix: none — read its log", self.lang()),
            );
            self.log.line(&text);
            self.roots.insert(root, RootState::Refused(text.clone()));
            self.retire(slot, &format!("it refused initialize: {message}"));
            return;
        }
        let uris: Vec<String> = self.documents.uris().map(str::to_string).collect();
        let mine: Vec<String> = uris
            .into_iter()
            .filter(|uri| self.root_of(uri).as_ref() == Ok(&root))
            .collect();
        let replay = self.documents.replay(|uri| mine.iter().any(|m| m == uri));
        if let Some(s) = self.slots.get_mut(&slot) {
            let waiting = match std::mem::replace(&mut s.phase, Phase::Live) {
                Phase::Starting { waiting, .. } => waiting,
                other => {
                    s.phase = other;
                    Vec::new()
                }
            };
            s.outbox.push(notification("initialized"));
            s.outbox
                .extend(replay.iter().map(|m| m.to_string().into_bytes()));
            s.outbox.extend(waiting);
        }
    }

    fn step_slot(&mut self, slot: u64, input: Input) {
        let actions = match self.slots.get_mut(&slot) {
            Some(s) => s.core.step(input, Instant::now()),
            None => return,
        };
        self.apply(slot, actions);
    }

    fn apply(&mut self, slot: u64, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::ToChild(b) => {
                    if let Some(s) = self.slots.get_mut(&slot) {
                        match &mut s.phase {
                            Phase::Starting { waiting, .. } => waiting.push(b),
                            Phase::Live | Phase::Closing { .. } => s.outbox.push(b),
                        }
                    }
                }
                Action::ToClient(b) => {
                    if let Some(id) = scan(&b).id {
                        self.client_ids.remove(&id);
                    }
                    // A refusal from a slot's Core names its repository.
                    let repo = self.slots.get(&slot).map(|s| s.repo.clone());
                    let b = repo
                        .and_then(|repo| route::name_repository(&b, self.lang(), &repo))
                        .unwrap_or(b);
                    self.send_to_client(b);
                }
                Action::SetFilter(_) => {
                    unreachable!("a slot's initialize is the shell's; its Core never sees one")
                }
                Action::Log(line) => {
                    let repo = self
                        .slots
                        .get(&slot)
                        .map_or("?", |s| s.repo.as_str())
                        .to_string();
                    self.log.line(&format!("[{repo}] {line}"));
                }
                Action::Exit(_) | Action::AwaitChildThenExit(_) => {
                    unreachable!(
                        "shutdown, exit and the client's end are the shell's in workspace mode"
                    )
                }
                Action::ChildDead => {
                    self.count_death(slot);
                    self.retire(slot, "its server died");
                }
            }
        }
    }

    /// The slot is done: its child is killed if still there, its root
    /// forgotten until the next request starts it again.
    fn retire(&mut self, slot: u64, why: &str) {
        // What it still held or had in flight is answered, whatever ended it.
        let msg = format!("fleet-lsp: {}: the server was stopped ({why})", self.lang());
        let failed = match self.slots.get_mut(&slot) {
            Some(s) => s.core.fail_all(&msg),
            None => return,
        };
        self.apply(slot, failed);
        let Some(mut s) = self.slots.remove(&slot) else {
            return;
        };
        if self.serving.get(&s.root) == Some(&slot) {
            self.serving.remove(&s.root);
        }
        self.client_ids.retain(|_, v| *v != slot);
        if let Some(sd) = &mut self.shutdown {
            sd.waiting.remove(&slot);
        }
        s.child_in.close();
        let _ = s.child.kill();
        let _ = s.child.wait();
        self.log.line(&format!(
            "retired {} (slot {slot}): {why}; log {}",
            s.repo, s.log_path
        ));
        self.answer_shutdown_when_done(Instant::now());
    }

    /// A child died: one more against its root's budget.
    fn count_death(&mut self, slot: u64) {
        let root = self.slots.get(&slot).map(|s| s.root.clone());
        if let Some(RootState::Resolved { deaths, .. }) = root.and_then(|r| self.roots.get_mut(&r))
        {
            *deaths += 1;
        }
    }

    // ---------------------------------------------------------------- loop

    fn housekeeping(&mut self) {
        let now = Instant::now();
        let slots: Vec<u64> = self.slots.keys().copied().collect();
        for slot in slots {
            let Some(s) = self.slots.get_mut(&slot) else {
                continue;
            };
            if let Phase::Closing { since } = s.phase {
                let cap = self.settings.max_children;
                match s.child.try_wait() {
                    Ok(Some(status)) => {
                        self.retire(slot, &format!("evicted at cap {cap}, {status}"))
                    }
                    Ok(None) if now.saturating_duration_since(since) < TEARDOWN => {}
                    Ok(None) => self.retire(slot, "evicted, did not exit after shutdown; killed"),
                    Err(e) => self.retire(
                        slot,
                        &format!("evicted, its status is unreadable: {e}; killed"),
                    ),
                }
                continue;
            }
            if s.stdout_closed || s.stdin_broken {
                if let Ok(Some(status)) = s.child.try_wait() {
                    self.step_slot(slot, Input::ChildExited(status.to_string()));
                    continue;
                }
            }
            let stuck = s.child_in.stalled_for(now).is_some_and(|d| d >= HUNG);
            self.step_slot(slot, Input::Tick);
            if stuck {
                self.step_slot(slot, Input::ChildHung);
            }
        }
        self.answer_shutdown_when_done(now);
        let client_stuck = self.outbox_client_bytes > QUEUE_CAP
            || self.client_out.stalled_for(now).is_some_and(|d| d >= HUNG);
        if client_stuck && self.exit.is_none() {
            self.log.line("the client stopped reading its input");
            self.client_gone();
        }
    }

    fn send_to_client(&mut self, body: Vec<u8>) {
        self.outbox_client_bytes += body.len();
        self.outbox_client.push(body);
    }

    fn flush_outboxes(&mut self) {
        for s in self.slots.values_mut() {
            let mut sent = 0;
            for b in &s.outbox {
                if s.child_in.try_push(0, b.clone()).is_err() {
                    break;
                }
                sent += 1;
            }
            s.outbox.drain(..sent);
            // An evicted child gets its `shutdown` and `exit`, then EOF.
            if matches!(s.phase, Phase::Closing { .. }) && s.outbox.is_empty() {
                s.child_in.close();
            }
        }
        let mut sent = 0;
        for b in &self.outbox_client {
            if self.client_out.try_push(OWN, b.clone()).is_err() {
                break;
            }
            self.outbox_client_bytes -= b.len();
            sent += 1;
        }
        self.outbox_client.drain(..sent);
    }

    fn teardown(&mut self, code: u8) -> u8 {
        self.log.line(&format!("teardown, exit {code}"));
        // What is queued for the children (an `exit`) goes first; a full
        // outbox is abandoned — the children are going away.
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.slots.values().any(|s| !s.outbox.is_empty()) && Instant::now() < deadline {
            self.flush_outboxes();
            thread::sleep(Duration::from_millis(10));
        }
        for s in self.slots.values() {
            s.child_in.close();
        }
        // One deadline for all children, not one each.
        let deadline = Instant::now() + TEARDOWN;
        let mut exited = std::collections::HashSet::new();
        loop {
            for (slot, s) in &mut self.slots {
                if exited.contains(slot) {
                    continue;
                }
                match s.child.try_wait() {
                    Ok(Some(status)) => {
                        exited.insert(*slot);
                        self.log.line(&format!("{} exited: {status}", s.repo));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        exited.insert(*slot);
                        self.log
                            .line(&format!("{}: status unreadable: {e}; killed", s.repo));
                        let _ = s.child.kill();
                    }
                }
            }
            if exited.len() == self.slots.len() || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        for (slot, s) in &mut self.slots {
            if !exited.contains(slot) {
                self.log.line(&format!("{} did not exit; killed", s.repo));
                let _ = s.child.kill();
            }
            let _ = s.child.wait();
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
}

/// The client's `initialize`, for one child: fleet-lsp's own id, and the
/// project root as the root and the only workspace folder.
fn child_init(client: &Json, id: &Id, root: &Path) -> Json {
    let mut msg = route::with_id(client.clone(), id);
    let uri = route::file_uri(root);
    let name = root
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    {
        let top = msg.obj_at(&[]);
        top.insert("jsonrpc".into(), Json::Str("2.0".into()));
        top.insert("method".into(), Json::Str("initialize".into()));
    }
    let params = msg.obj_at(&["params"]);
    params.insert("rootUri".into(), Json::Str(uri.clone()));
    params.insert(
        "rootPath".into(),
        Json::Str(root.to_string_lossy().into_owned()),
    );
    params.insert(
        "workspaceFolders".into(),
        Json::Arr(vec![Json::obj().set("uri", uri).set("name", name)]),
    );
    msg
}

fn notification(method: &str) -> Vec<u8> {
    Json::obj()
        .set("jsonrpc", "2.0")
        .set("method", method)
        .to_string()
        .into_bytes()
}

fn parse(body: &[u8]) -> Option<Json> {
    json::parse(std::str::from_utf8(body).ok()?).ok()
}

/// A child's stdout: replies to fleet-lsp's own requests stay inside, the
/// child's own requests reach the client under a slot-qualified id, and
/// what the gate needs is reported as in single-root mode.
fn slot_reader(
    slot: u64,
    stdout: ChildStdout,
    out: &Queue,
    filter: &Mutex<ServerFilter>,
    emit: impl Fn(SlotEvent) -> bool,
) {
    let mut reader = FrameReader::new(stdout);
    loop {
        let body = match reader.next_frame() {
            Ok(Some(Frame::Body(b))) => b,
            Ok(Some(Frame::Oversize { scan, .. })) => {
                match scan.id.clone().filter(route::is_own) {
                    Some(id) => emit(SlotEvent::OwnReply(id, Vec::new())),
                    None => emit(SlotEvent::Input(Input::ServerOversize(scan))),
                };
                continue;
            }
            Ok(None) | Err(_) => {
                emit(SlotEvent::StdoutClosed);
                return;
            }
        };
        let s = scan(&body);
        let f = *filter.lock().unwrap_or_else(|p| p.into_inner());
        let method = s.method.as_deref().unwrap_or("");
        let mut forward = Some(body);
        match s.kind() {
            Kind::Notification => {
                let body = forward.take().expect("set above");
                let (seen, keep) = relay::observe_notification(method, &body, f);
                match seen {
                    Some(Ev::Input(input)) => emit(SlotEvent::Input(input)),
                    Some(Ev::Log(line)) => emit(SlotEvent::Log(line)),
                    Some(_) | None => true,
                };
                forward = keep.then_some(body);
            }
            Kind::Request => {
                let body = forward.take().expect("set above");
                let id = s.id.clone().expect("a request has an id");
                let keep = (method == "workspace/configuration").then(|| body.clone());
                emit(SlotEvent::Input(Input::ServerRequest {
                    id: id.clone(),
                    method: method.to_string(),
                    body: keep,
                }));
                if !(method == "window/workDoneProgress/create" && f.own_progress) {
                    forward = match parse(&body) {
                        Some(msg) => Some(
                            route::with_id(msg, &route::client_facing_id(slot, &id))
                                .to_string()
                                .into_bytes(),
                        ),
                        None => {
                            emit(SlotEvent::Log(format!(
                                "a {method} request that is not JSON ({} bytes); not relayed",
                                body.len()
                            )));
                            None
                        }
                    };
                }
            }
            Kind::Response => match s.id.clone() {
                Some(id) if route::is_own(&id) => {
                    let body = forward.take().expect("set above");
                    emit(SlotEvent::OwnReply(id, body));
                }
                Some(id) => {
                    emit(SlotEvent::Input(Input::ServerResponse(id)));
                }
                None => {}
            },
            Kind::Other => {}
        }
        if let Some(body) = forward {
            if out.push(SERVER, body).is_err() {
                return;
            }
        }
    }
}

/// A child's stderr, line by line, into its own log.
fn stderr_to_log(stderr: ChildStderr, mut log: Log) {
    for line in BufReader::new(stderr).lines() {
        match line {
            Ok(line) => log.line(&line),
            Err(_) => return,
        }
    }
}
