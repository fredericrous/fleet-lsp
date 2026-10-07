//! `fleet-lsp serve` end to end, against a fake server reached through the
//! real python resolver (tests/fixtures/fake_server.py installed as a fixture
//! repository's `.venv/bin/pyright-langserver`). The shipped binary carries
//! no test mode.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_fleet-lsp");

/// The tests that move tens of MiB through a Python fake are CPU-bound;
/// beside them a light test's timing bound (e.g. 100 ms) is measured on a
/// starved machine. Heavy tests take this lock exclusively, light ones
/// shared: light tests run together, never beside a heavy one, and heavy
/// tests run one at a time.
static LOAD: std::sync::RwLock<()> = std::sync::RwLock::new(());

fn heavy() -> std::sync::RwLockWriteGuard<'static, ()> {
    LOAD.write().unwrap_or_else(|p| p.into_inner())
}

fn light() -> std::sync::RwLockReadGuard<'static, ()> {
    LOAD.read().unwrap_or_else(|p| p.into_inner())
}

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str, with_venv: bool) -> Fixture {
        let dir = std::env::temp_dir().join(format!("fleet-lsp-it-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(
            dir.join("pyproject.toml"),
            "[dependency-groups]\ndev = [\"pyright==1.1.411\"]\n",
        )
        .unwrap();
        if with_venv {
            let info = dir.join(".venv/lib/python3.13/site-packages/pyright-1.1.411.dist-info");
            fs::create_dir_all(&info).unwrap();
            fs::write(info.join("METADATA"), "Name: pyright\nVersion: 1.1.411\n").unwrap();
            let bin = dir.join(".venv/bin");
            fs::create_dir_all(&bin).unwrap();
            let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_server.py");
            let script = bin.join("pyright-langserver");
            fs::write(
                &script,
                format!("#!/bin/sh\nexec python3 -u '{}' \"$@\"\n", fake.display()),
            )
            .unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Fixture { dir }
    }

    /// A Cargo project whose `rustup` (first on the session's PATH) points
    /// at the fake server in rust-analyzer flavour, under a toolchain path,
    /// so the real rust resolver verifies it.
    fn rust(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("fleet-lsp-it-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            dir.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.94.1\"\n",
        )
        .unwrap();
        let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_server.py");
        let ra_dir = dir.join("toolchains/1.94.1-x86_64-fake/bin");
        fs::create_dir_all(&ra_dir).unwrap();
        let ra = ra_dir.join("rust-analyzer");
        let script = |p: &Path, body: String| {
            fs::write(p, body).unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
        };
        script(
            &ra,
            format!("#!/bin/sh\nexec python3 -u '{}' \"$@\"\n", fake.display()),
        );
        fs::create_dir_all(dir.join("fakebin")).unwrap();
        script(
            &dir.join("fakebin/rustup"),
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  which) echo '{}' ;;\n  run) echo 'rustc 1.94.1 (fake)' ;;\n  *) exit 1 ;;\nesac\n",
                ra.display()
            ),
        );
        Fixture { dir }
    }

    /// PATH with this fixture's fake tools first.
    fn path_env(&self) -> String {
        format!(
            "{}:{}",
            self.dir.join("fakebin").display(),
            std::env::var("PATH").unwrap()
        )
    }

    fn events(&self) -> PathBuf {
        self.dir.join("events.log")
    }

    fn event_lines(&self) -> Vec<String> {
        fs::read_to_string(self.events())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn fake_pid(&self) -> Option<u32> {
        self.event_lines().iter().find_map(|l| {
            l.split_once(" pid ")
                .map(|(_, p)| p.trim().parse().unwrap())
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // A failing test may have SIGKILLed fleet-lsp before its teardown; a
        // fake that stopped reading would then outlive it and hold the test
        // runner's stderr open.
        if let Some(pid) = self.fake_pid() {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .stderr(Stdio::null())
                .status();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

struct Session {
    proc: Child,
    stdin: Option<ChildStdin>,
    rx: Option<Receiver<(Instant, String)>>,
}

fn frame(body: &str) -> Vec<u8> {
    let mut v = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    v.extend_from_slice(body.as_bytes());
    v
}

fn read_frames(mut out: impl Read + Send + 'static) -> Receiver<(Instant, String)> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = vec![0u8; 1 << 16];
        loop {
            let n = match out.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
            while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_string();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                if buf.len() < end + 4 + len {
                    break;
                }
                let body = String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).to_string();
                buf.drain(..end + 4 + len);
                if tx.send((Instant::now(), body)).is_err() {
                    return;
                }
            }
        }
    });
    rx
}

impl Session {
    fn start(fx: &Fixture, env: &[(&str, &str)], read_stdout: bool) -> Session {
        Session::start_args(
            fx,
            env,
            read_stdout,
            &["serve", "python", "--min-version", "0.1.0"],
        )
    }

    fn start_args(fx: &Fixture, env: &[(&str, &str)], read_stdout: bool, args: &[&str]) -> Session {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .current_dir(&fx.dir)
            .env("XDG_STATE_HOME", fx.dir.join("state"))
            .env("FAKE_EVENTS", fx.events())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut proc = cmd.spawn().unwrap();
        let stdin = proc.stdin.take();
        let stdout = proc.stdout.take().unwrap();
        let rx = if read_stdout {
            Some(read_frames(stdout))
        } else {
            // Keep the pipe open and never read it.
            std::mem::forget(stdout);
            None
        };
        Session { proc, stdin, rx }
    }

    fn send(&mut self, body: &str) {
        let s = self.stdin.as_mut().unwrap();
        s.write_all(&frame(body)).unwrap();
        s.flush().unwrap();
    }

    fn try_send(&mut self, body: &str) -> bool {
        let Some(s) = self.stdin.as_mut() else {
            return false;
        };
        s.write_all(&frame(body)).and_then(|()| s.flush()).is_ok()
    }

    fn recv_until(
        &self,
        timeout: Duration,
        pred: impl Fn(&str) -> bool,
    ) -> Option<(Instant, String)> {
        let rx = self.rx.as_ref().unwrap();
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            match rx.recv_timeout(left) {
                Ok((t, body)) if pred(&body) => return Some((t, body)),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }

    fn initialize(&mut self, root: &Path) {
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","id":0,"method":"initialize","params":{{"rootUri":"file://{}","capabilities":{{"window":{{"workDoneProgress":true}}}}}}}}"#,
            root.display()
        ));
        self.recv_until(Duration::from_secs(30), |b| b.contains(r#""id":0"#))
            .expect("initialize answered");
        self.send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(st) = self.proc.try_wait().unwrap() {
                return st.code();
            }
            thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.proc.kill();
        let _ = self.proc.wait();
    }
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn gone_within(pid: u32, d: Duration) -> bool {
    let deadline = Instant::now() + d;
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_for(fx: &Fixture, needle: &str, d: Duration) -> bool {
    let deadline = Instant::now() + d;
    while Instant::now() < deadline {
        if fx.event_lines().iter().any(|l| l.contains(needle)) {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

const REFS: &str = r#"{"jsonrpc":"2.0","id":7,"method":"textDocument/references","params":{}}"#;
const CHANGE: &str = r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{}}"#;

#[test]
fn a_request_waits_for_readiness_and_keeps_its_order() {
    let _light = light();
    let fx = Fixture::new("ready", true);
    let mut s = Session::start(&fx, &[("FAKE_READY_DELAY", "3")], true);
    // t = 0 is `initialized`: the fake becomes ready 3 s after it.
    let t0 = Instant::now();
    s.initialize(&fx.dir);
    s.send(REFS);
    s.send(CHANGE);
    let (t, body) = s
        .recv_until(Duration::from_secs(20), |b| b.contains(r#""id":7"#))
        .expect("answered");
    assert!(
        t.duration_since(t0) >= Duration::from_secs(3),
        "answered before ready"
    );
    assert!(body.contains("file:///answer"), "{body}");
    let ev = fx.event_lines();
    let pos = |needle: &str| {
        ev.iter()
            .position(|l| l.contains(needle))
            .unwrap_or(usize::MAX)
    };
    assert!(pos(" ready") < pos("textDocument/references"), "{ev:?}");
    assert!(
        pos("textDocument/references") < pos("textDocument/didChange"),
        "{ev:?}"
    );
}

#[test]
fn a_configuration_reply_is_never_held_and_keeps_pyright_logging() {
    let _light = light();
    let fx = Fixture::new("config", true);
    let mut s = Session::start(&fx, &[("FAKE_CONFIG_FIRST", "1")], true);
    s.initialize(&fx.dir);
    s.send(REFS); // held: not ready until the configuration reply arrives
    let (_, req) = s
        .recv_until(Duration::from_secs(10), |b| {
            b.contains("workspace/configuration")
        })
        .expect("server request relayed");
    assert!(req.contains(r#""id":900"#));
    let t = Instant::now();
    s.send(r#"{"jsonrpc":"2.0","id":900,"result":[{"logLevel":"Error"}]}"#);
    assert!(
        wait_for(&fx, "config-reply", Duration::from_secs(1)),
        "reply held: {:?}",
        fx.event_lines()
    );
    assert!(t.elapsed() < Duration::from_secs(1));
    let ev = fx.event_lines();
    let reply = ev.iter().find(|l| l.contains("config-reply")).unwrap();
    assert!(
        reply.contains("Information") && !reply.contains("Error"),
        "{reply}"
    );
    assert!(s
        .recv_until(Duration::from_secs(10), |b| b.contains(r#""id":7"#))
        .is_some());
}

#[test]
fn cancelling_a_held_request_releases_the_notification_behind_it() {
    let _light = light();
    let fx = Fixture::new("cancel", true);
    let mut s = Session::start(&fx, &[("FAKE_NEVER_READY", "1")], true);
    s.initialize(&fx.dir);
    s.send(REFS);
    s.send(CHANGE);
    thread::sleep(Duration::from_millis(200));
    assert!(
        !fx.event_lines().iter().any(|l| l.contains("didChange")),
        "held"
    );
    let t = Instant::now();
    s.send(r#"{"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":7}}"#);
    let (_, body) = s
        .recv_until(Duration::from_secs(5), |b| b.contains(r#""id":7"#))
        .unwrap();
    assert!(body.contains("-32800"), "{body}");
    assert!(
        wait_for(&fx, "didChange", Duration::from_millis(100)),
        "{:?}",
        fx.event_lines()
    );
    assert!(t.elapsed() < Duration::from_secs(1));
}

#[test]
fn ceiling_from_the_environment_answers_with_an_error() {
    let _light = light();
    let fx = Fixture::new("ceiling", true);
    let mut s = Session::start(
        &fx,
        &[("FAKE_NEVER_READY", "1"), ("FLEET_LSP_CEILING_MS", "2000")],
        true,
    );
    s.initialize(&fx.dir);
    let t0 = Instant::now();
    s.send(REFS);
    let (t, body) = s
        .recv_until(Duration::from_secs(10), |b| b.contains(r#""id":7"#))
        .unwrap();
    let after = t.duration_since(t0);
    assert!(
        after >= Duration::from_millis(1900) && after < Duration::from_secs(4),
        "{after:?}"
    );
    assert!(body.contains("not ready after 2s"), "{body}");
}

#[test]
fn shutdown_then_exit_is_exit_0_and_the_server_is_reaped() {
    let _light = light();
    let fx = Fixture::new("shutdown", true);
    let mut s = Session::start(&fx, &[], true);
    s.initialize(&fx.dir);
    s.send(r#"{"jsonrpc":"2.0","id":9,"method":"shutdown"}"#);
    let (_, body) = s
        .recv_until(Duration::from_secs(5), |b| b.contains(r#""id":9"#))
        .unwrap();
    assert!(body.contains(r#""result":null"#), "{body}");
    s.send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    assert_eq!(s.wait_exit(Duration::from_secs(12)), Some(0));
    assert!(gone_within(fx.fake_pid().unwrap(), Duration::from_secs(1)));
}

#[test]
fn stdin_eof_while_held_tears_down_within_6s() {
    let _light = light();
    let fx = Fixture::new("eof", true);
    let mut s = Session::start(&fx, &[("FAKE_NEVER_READY", "1")], true);
    s.initialize(&fx.dir);
    s.send(REFS);
    thread::sleep(Duration::from_millis(200));
    s.stdin = None;
    assert_eq!(s.wait_exit(Duration::from_secs(6)), Some(1));
    assert!(gone_within(fx.fake_pid().unwrap(), Duration::from_secs(1)));
}

#[test]
fn sigterm_while_held_leaves_no_server_behind() {
    let _light = light();
    let fx = Fixture::new("term", true);
    let mut s = Session::start(&fx, &[("FAKE_NEVER_READY", "1")], true);
    s.initialize(&fx.dir);
    s.send(REFS);
    thread::sleep(Duration::from_millis(200));
    let pid = fx.fake_pid().unwrap();
    Command::new("kill")
        .args(["-TERM", &s.proc.id().to_string()])
        .status()
        .unwrap();
    assert!(
        gone_within(pid, Duration::from_secs(6)),
        "server outlived fleet-lsp"
    );
}

#[test]
fn a_server_that_exits_on_its_own_fails_held_requests() {
    let _light = light();
    let fx = Fixture::new("selfexit", true);
    let mut s = Session::start(
        &fx,
        &[("FAKE_NEVER_READY", "1"), ("FAKE_EXIT_AFTER", "1")],
        true,
    );
    s.initialize(&fx.dir);
    s.send(REFS);
    let (_, body) = s
        .recv_until(Duration::from_secs(10), |b| b.contains(r#""id":7"#))
        .unwrap();
    assert!(body.contains("the server exited"), "{body}");
    assert_eq!(s.wait_exit(Duration::from_secs(12)), Some(1));
}

#[test]
fn no_venv_is_a_refusal_the_client_can_read() {
    let _light = light();
    let fx = Fixture::new("stub", false);
    let mut s = Session::start(&fx, &[], true);
    s.send(&format!(
        r#"{{"jsonrpc":"2.0","id":0,"method":"initialize","params":{{"rootUri":"file://{}","capabilities":{{}}}}}}"#,
        fx.dir.display()
    ));
    let (_, init) = s
        .recv_until(Duration::from_secs(5), |b| b.contains(r#""id":0"#))
        .unwrap();
    assert!(
        init.contains("referencesProvider") && init.contains("callHierarchyProvider"),
        "{init}"
    );
    s.send(REFS);
    let (_, body) = s
        .recv_until(Duration::from_secs(5), |b| b.contains(r#""id":7"#))
        .unwrap();
    assert!(
        body.contains("uv sync") && body.contains("-32803"),
        "{body}"
    );
    s.send(r#"{"jsonrpc":"2.0","id":9,"method":"shutdown"}"#);
    let (_, body) = s
        .recv_until(Duration::from_secs(5), |b| b.contains(r#""id":9"#))
        .unwrap();
    assert!(body.contains(r#""result":null"#));
    s.send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    assert_eq!(s.wait_exit(Duration::from_secs(5)), Some(0));
}

#[test]
fn a_newer_plugin_gets_the_upgrade_refusal() {
    let _light = light();
    let fx = Fixture::new("minver", true);
    let mut s = Session::start_args(
        &fx,
        &[],
        true,
        &[
            "serve",
            "python",
            "--min-version",
            "99.0.0",
            "--from-the-future",
        ],
    );
    s.initialize(&fx.dir);
    s.send(REFS);
    let (_, body) = s
        .recv_until(Duration::from_secs(5), |b| b.contains(r#""id":7"#))
        .unwrap();
    assert!(body.contains("brew upgrade fleet-lsp"), "{body}");
}

#[test]
fn a_server_that_stops_reading_is_torn_down_after_30s() {
    let _heavy = heavy();
    let fx = Fixture::new("noread", true);
    let mut s = Session::start(&fx, &[("FAKE_STOP_READING", "1")], true);
    s.initialize(&fx.dir);
    assert!(wait_for(&fx, "stopped-reading", Duration::from_secs(5)));
    thread::sleep(Duration::from_millis(300)); // the readiness line opens the gate
    let pad = "x".repeat(20 << 20);
    let big =
        format!(r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"t":"{pad}"}}}}"#);
    let t0 = Instant::now();
    // Writes may block once every buffer is full; do them off-thread.
    let mut stdin = s.stdin.take().unwrap();
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(&frame(&big));
        let _ = stdin.flush();
        stdin
    });
    // 30 s to declare the server hung, 10 s grace for it to exit, up to 2 s
    // for fleet-lsp's own last replies.
    let code = s.wait_exit(Duration::from_secs(50));
    assert_eq!(code, Some(1), "torn down");
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_secs(29) && took <= Duration::from_secs(45),
        "{took:?}"
    );
    assert!(gone_within(fx.fake_pid().unwrap(), Duration::from_secs(1)));
    drop(writer);
}

#[test]
fn a_client_that_stops_reading_is_torn_down_after_30s() {
    let _heavy = heavy();
    let fx = Fixture::new("noclient", true);
    let mut s = Session::start(&fx, &[("FAKE_FLOOD_MIB", "64")], false);
    s.send(&format!(
        r#"{{"jsonrpc":"2.0","id":0,"method":"initialize","params":{{"rootUri":"file://{}","capabilities":{{}}}}}}"#,
        fx.dir.display()
    ));
    s.send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);
    let t0 = Instant::now();
    let code = s.wait_exit(Duration::from_secs(50));
    assert_eq!(code, Some(1));
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_secs(29) && took <= Duration::from_secs(45),
        "{took:?}"
    );
    assert!(gone_within(fx.fake_pid().unwrap(), Duration::from_secs(1)));
}

#[test]
fn a_slow_but_reading_server_keeps_its_output_flowing() {
    let _heavy = heavy();
    let fx = Fixture::new("slow", true);
    let mut s = Session::start(
        &fx,
        &[("FAKE_SLOW_READ", "1048576"), ("FAKE_FLOOD_MIB", "8")],
        true,
    );
    s.initialize(&fx.dir);
    thread::sleep(Duration::from_millis(300));
    // 35 MiB the server reads at 1 MiB/s: the child queue stays busy, but
    // writes keep moving, so it is never "hung". Waits below are on events
    // with generous deadlines: a slow runner reads slower, it does not fail.
    let mut stdin = s.stdin.take().unwrap();
    let started = Instant::now();
    let writer = thread::spawn(move || {
        // One frame at the 32 MiB limit first: at 1 MiB/s it takes over 30 s
        // to write, and must not read as a server that stopped reading.
        let overhead =
            r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"t":""}}"#.len();
        let huge = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"t":"{}"}}}}"#,
            "w".repeat((32 << 20) - overhead)
        );
        if stdin.write_all(&frame(&huge)).is_err() {
            return None;
        }
        let pad = "y".repeat((1 << 20) - 100);
        for _ in 0..3 {
            let body = format!(
                r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"t":"{pad}"}}}}"#
            );
            if stdin.write_all(&frame(&body)).is_err() {
                return None;
            }
        }
        // Returned, not dropped: closing stdin is the client leaving.
        Some(stdin)
    });
    // Diagnostics reach the client while client→server is backed up.
    assert!(s
        .recv_until(Duration::from_secs(20), |b| b
            .contains("publishDiagnostics"))
        .is_some());
    assert!(wait_for(&fx, "flooded", Duration::from_secs(60)));
    // The 32 MiB frame arrives only after > 30 s of writing; fleet-lsp must
    // still be there when it does.
    assert!(
        wait_for(&fx, "textDocument/didChange", Duration::from_secs(240)),
        "the 32 MiB frame never arrived"
    );
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(30),
        "the write was not slow: {took:?}"
    );
    assert!(
        s.proc.try_wait().unwrap().is_none(),
        "torn down while the server was reading"
    );
    let count = || {
        fx.event_lines()
            .iter()
            .filter(|l| l.contains("didChange"))
            .count()
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while count() < 4 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(count(), 4, "every notification delivered");
    assert!(s.proc.try_wait().unwrap().is_none(), "torn down at the end");
    let _stdin = writer.join().unwrap().expect("every write accepted");
}

fn rss_kib(pid: u32) -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

#[test]
fn sustained_traffic_while_closed_overloads_without_losing_notifications() {
    let _heavy = heavy();
    let fx = Fixture::new("overload", true);
    let mut s = Session::start(
        &fx,
        &[
            ("FAKE_NEVER_READY", "1"),
            ("FAKE_FLOOD_MIB", "96"),
            ("FAKE_FLOOD_FRAME", "33554432"),
        ],
        true,
    );
    s.initialize(&fx.dir);
    let pid = s.proc.id();
    let peak = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let p2 = std::sync::Arc::clone(&peak);
    let sampler = thread::spawn(move || {
        for _ in 0..600 {
            let r = rss_kib(pid);
            p2.fetch_max(r, std::sync::atomic::Ordering::Relaxed);
            thread::sleep(Duration::from_millis(50));
        }
    });
    let mut stdin = s.stdin.take().unwrap();
    let writer = thread::spawn(move || {
        for i in 0..1000 {
            let r = format!(
                r#"{{"jsonrpc":"2.0","id":{},"method":"textDocument/hover","params":{{}}}}"#,
                1000 + i
            );
            stdin.write_all(&frame(&r)).unwrap();
        }
        // Five notifications of exactly the frame limit: 160 MiB.
        let overhead =
            r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"t":""}}"#.len();
        let pad = "z".repeat((32 << 20) - overhead);
        for _ in 0..5 {
            let body = format!(
                r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"t":"{pad}"}}}}"#
            );
            assert_eq!(body.len(), 32 << 20);
            stdin.write_all(&frame(&body)).unwrap();
        }
        stdin.flush().unwrap();
        stdin
    });
    let mut overloads = 0;
    let deadline = Instant::now() + Duration::from_secs(60);
    while overloads < 1000 && Instant::now() < deadline {
        if s.recv_until(Duration::from_secs(5), |b| b.contains("overload"))
            .is_some()
        {
            overloads += 1;
        }
    }
    assert_eq!(overloads, 1000, "every held request answered");
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline
        && fx
            .event_lines()
            .iter()
            .filter(|l| l.contains("textDocument/didChange"))
            .count()
            < 5
    {
        thread::sleep(Duration::from_millis(100));
    }
    let n = fx
        .event_lines()
        .iter()
        .filter(|l| l.contains("textDocument/didChange"))
        .count();
    assert_eq!(n, 5, "no notification lost");
    let _stdin = writer.join().unwrap();
    drop(sampler);
    let peak_mib = peak.load(std::sync::atomic::Ordering::Relaxed) / 1024;
    assert!(peak_mib <= 202, "peak RSS {peak_mib} MiB");
    eprintln!("peak RSS {peak_mib} MiB");
}

#[test]
fn doctor_into_a_closed_pipe_exits_0_without_a_panic() {
    let _light = light();
    let fx = Fixture::new("pipe", true);
    let mut child = Command::new(BIN)
        .arg("doctor")
        .current_dir(&fx.dir)
        .env("XDG_STATE_HOME", fx.dir.join("state"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take()); // the reader is gone before doctor writes
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked"), "{err}");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn doctor_reports_the_verified_fixture() {
    let _light = light();
    let fx = Fixture::new("doctor", true);
    let out = Command::new(BIN)
        .arg("doctor")
        .current_dir(&fx.dir)
        .env("XDG_STATE_HOME", fx.dir.join("state"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("python     verified 1.1.411"), "{text}");
    // Every line fits 80 columns, except the ones naming the fixture's long
    // temporary path (the header and the log dir).
    assert!(
        text.lines()
            .filter(|l| !l.contains("fleet-lsp-it-"))
            .all(|l| l.chars().count() <= 80),
        "{text}"
    );
    assert_eq!(out.status.code(), Some(0));
    let fx2 = Fixture::new("doctor-refused", false);
    let out = Command::new(BIN)
        .args(["doctor", "--json"])
        .current_dir(&fx2.dir)
        .env("XDG_STATE_HOME", fx2.dir.join("state"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(r#""verdict":"refused""#) && text.contains(r#""fix":"uv sync""#),
        "{text}"
    );
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn closed_stdin_before_initialize_is_exit_1() {
    let _light = light();
    let fx = Fixture::new("noinit", true);
    let mut s = Session::start(&fx, &[], true);
    s.stdin = None;
    // An exit code, not a timing check: generous under parallel test load.
    assert_eq!(s.wait_exit(Duration::from_secs(15)), Some(1));
    let _ = s.try_send("{}");
}

fn start_rust(fx: &Fixture, env: &[(&str, &str)]) -> Session {
    let path = fx.path_env();
    let mut all: Vec<(&str, &str)> = vec![("FAKE_FLAVOR", "ra"), ("PATH", &path)];
    all.extend_from_slice(env);
    Session::start_args(fx, &all, true, &["serve", "rust", "--min-version", "0.1.0"])
}

#[test]
fn rust_waits_for_quiescence_and_never_shows_the_status() {
    let _light = light();
    let fx = Fixture::rust("ra-ready");
    let mut s = start_rust(&fx, &[("FAKE_READY_DELAY", "3")]);
    let t0 = Instant::now();
    s.initialize(&fx.dir);
    s.send(REFS);
    let rx = s.rx.take().unwrap();
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut answered = None;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok((t, body)) => {
                if body.contains(r#""id":7"#) {
                    answered = Some((t, body.clone()));
                }
                seen.push(body);
            }
            Err(_) if answered.is_some() => break,
            Err(_) => {}
        }
    }
    let (t, body) = answered.expect("answered");
    assert!(
        t.duration_since(t0) >= Duration::from_secs(3),
        "answered before quiescent"
    );
    assert!(body.contains("file:///answer"), "{body}");
    assert!(
        !seen.iter().any(|b| b.contains("experimental/serverStatus")),
        "a serverStatus frame reached the client"
    );
}

#[test]
fn rust_health_error_refuses_with_the_message() {
    let _light = light();
    let fx = Fixture::rust("ra-error");
    let mut s = start_rust(
        &fx,
        &[
            ("FAKE_RA_HEALTH", "error"),
            ("FAKE_RA_MESSAGE", "Failed to load workspaces."),
        ],
    );
    s.initialize(&fx.dir);
    s.send(REFS);
    let (_, body) = s
        .recv_until(Duration::from_secs(10), |b| b.contains(r#""id":7"#))
        .unwrap();
    assert!(
        body.contains("workspace did not load: Failed to load workspaces."),
        "{body}"
    );
    assert!(body.contains("cargo metadata"), "{body}");
}

#[test]
fn rust_health_warning_answers_and_shows_the_warning() {
    let _light = light();
    let fx = Fixture::rust("ra-warning");
    let mut s = start_rust(
        &fx,
        &[
            ("FAKE_RA_HEALTH", "warning"),
            ("FAKE_RA_MESSAGE", "no matching package named `serde` found"),
        ],
    );
    s.initialize(&fx.dir);
    let (_, warn) = s
        .recv_until(Duration::from_secs(10), |b| {
            b.contains("window/showMessage")
        })
        .expect("warning shown");
    assert!(warn.contains("cargo fetch"), "{warn}");
    s.send(REFS);
    let (_, body) = s
        .recv_until(Duration::from_secs(10), |b| b.contains(r#""id":7"#))
        .unwrap();
    assert!(body.contains("file:///answer"), "{body}");
}
