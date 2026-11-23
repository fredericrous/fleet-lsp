//! The plugin's hooks as Claude Code runs them: each `command` taken from
//! `hooks.json`, through `sh -c`, with `CLAUDE_PLUGIN_ROOT` set and the
//! binary under test first on PATH. What each event prints, and that it
//! prints it only where it serves.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// The `command` of `event` in hooks.json, unescaped. PostToolUse is the
/// command an older plugin copy (before 0.7.1) still runs: the plugin's own
/// first-contact guidance is the in-process mod now, and the binary keeps the
/// event for those copies.
fn command(event: &str) -> String {
    if event == "PostToolUse" {
        return r#""${CLAUDE_PLUGIN_ROOT}/hooks/code-intelligence.sh" post-tool-use"#.to_string();
    }
    let text =
        fs::read_to_string(Path::new(ROOT).join("plugins/fleet-lsp/hooks/hooks.json")).unwrap();
    let at = text
        .find(&format!("\"{event}\""))
        .unwrap_or_else(|| panic!("no {event}"));
    let rest = &text[at..];
    let start = rest.find("\"command\": \"").unwrap() + "\"command\": \"".len();
    let mut out = String::new();
    let mut chars = rest[start..].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next().unwrap()),
            '"' => break,
            c => out.push(c),
        }
    }
    out
}

struct Box_ {
    dir: PathBuf,
}

impl Box_ {
    fn new(name: &str) -> Box_ {
        let dir =
            std::env::temp_dir().join(format!("fleet-lsp-hooktest-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir); // a leftover scratch dir is replaced below
        fs::create_dir_all(dir.join("home")).unwrap();
        Box_ { dir }
    }

    fn run(&self, event: &str, input: &str) -> String {
        let bin = Path::new(env!("CARGO_BIN_EXE_fleet-lsp"))
            .parent()
            .unwrap()
            .to_path_buf();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command(event))
            .env(
                "CLAUDE_PLUGIN_ROOT",
                Path::new(ROOT).join("plugins/fleet-lsp"),
            )
            .env("PATH", path)
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("HOME", self.dir.join("home"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{event}: {out:?}");
        assert!(
            out.stderr.is_empty(),
            "{event}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for Box_ {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir); // a leftover scratch dir does not fail a test
    }
}

fn start(session: &str, cwd: &Path) -> String {
    format!(
        r#"{{"session_id":"{session}","cwd":"{}","source":"startup"}}"#,
        cwd.display()
    )
}

fn touch(session: &str, file: &str) -> String {
    format!(
        r#"{{"session_id":"{session}","tool_name":"Read","tool_input":{{"file_path":"{file}"}},"tool_response":{{"content":"x"}}}}"#
    )
}

/// FALSIFY: print the guidance unconditionally in `hook::session_start`.
#[test]
fn session_start_guides_only_inside_a_project() {
    let b = Box_::new("start");
    assert_eq!(b.run("SessionStart", &start("s1", &b.dir)), "");
    let out = b.run("SessionStart", &start("s2", Path::new(ROOT)));
    assert!(out.starts_with("Code intelligence (fleet-lsp"), "{out}");
    assert!(out.lines().count() <= 6, "an index (ADR-0026): {out}");
}

#[test]
fn post_tool_use_guides_on_the_first_served_file_only() {
    let b = Box_::new("touch");
    assert_eq!(b.run("PostToolUse", &touch("t1", "/x/README.md")), "");
    let out = b.run("PostToolUse", &touch("t1", "/x/src/main.rs"));
    assert!(out.contains(r#""hookEventName":"PostToolUse""#), "{out}");
    assert!(out.contains("Code intelligence (fleet-lsp"), "{out}");
    assert_eq!(b.run("PostToolUse", &touch("t1", "/x/src/lib.rs")), "");
    let logs: Vec<PathBuf> = fs::read_dir(b.dir.join("state/fleet-lsp/hook"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(logs.len(), 1, "one dated log: {logs:?}");
    let log = fs::read_to_string(&logs[0]).unwrap();
    assert!(
        log.contains("PostToolUse t1: guided on /x/src/main.rs"),
        "{log}"
    );
    assert_eq!(log.lines().count(), 1, "the fast path logs nothing: {log}");
}

#[test]
fn subagent_start_skips_an_agent_without_lsp() {
    let b = Box_::new("agents");
    let agents = b.dir.join("home/.claude/agents");
    fs::create_dir_all(&agents).unwrap();
    fs::write(
        agents.join("browser-check.md"),
        "---\nname: browser-check\ntools: Read, Bash\n---\n",
    )
    .unwrap();
    let input = |agent: &str| {
        format!(
            r#"{{"session_id":"s","agent_id":"a1","agent_type":"{agent}","cwd":"{}"}}"#,
            b.dir.display()
        )
    };
    assert_eq!(b.run("SubagentStart", &input("browser-check")), "");
    let out = b.run("SubagentStart", &input("Explore"));
    assert!(out.contains(r#""hookEventName":"SubagentStart""#), "{out}");
}

#[test]
fn input_that_is_not_json_is_silent() {
    let b = Box_::new("garbage");
    for event in ["SessionStart", "PostToolUse", "SubagentStart"] {
        assert_eq!(b.run(event, "not json"), "", "{event}");
    }
}

/// FALSIFY: use `log_dir()` instead of `state_dir()` in `run_hook`.
#[test]
fn without_a_state_directory_the_hook_writes_nothing() {
    let b = Box_::new("nostate");
    let cwd = b.dir.join("project");
    fs::create_dir_all(&cwd).unwrap();
    for xdg in [None, Some("relative/state")] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_fleet-lsp"));
        cmd.args(["hook", "post-tool-use"])
            .current_dir(&cwd)
            .env_remove("HOME")
            .env_remove("XDG_STATE_HOME");
        if let Some(relative) = xdg {
            cmd.env("XDG_STATE_HOME", relative);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = touch("n1", "/x/a.rs");
        let _ = child.stdin.take().unwrap().write_all(input.as_bytes()); // it may exit before reading
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{xdg:?}");
        assert!(out.stdout.is_empty(), "{xdg:?}");
        assert_eq!(
            fs::read_dir(&cwd).unwrap().count(),
            0,
            "{xdg:?}: wrote into the project"
        );
    }
}

#[test]
fn a_typed_hook_with_a_bad_event_is_a_usage_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_fleet-lsp"))
        .args(["hook", "bogus"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}

/// The cost a PostToolUse hook adds to every Read, Edit and Write once the
/// session holds the guidance: the wrapper, the binary, the marker check.
#[test]
fn the_marker_fast_path_is_cheap() {
    let b = Box_::new("timing");
    b.run("PostToolUse", &touch("p1", "/x/a.rs"));
    let mut times: Vec<u128> = (0..50)
        .map(|_| {
            let t = Instant::now();
            assert_eq!(b.run("PostToolUse", &touch("p1", "/x/a.rs")), "");
            t.elapsed().as_micros()
        })
        .collect();
    times.sort_unstable();
    let (p50, p95) = (times[25], times[47]);
    eprintln!(
        "post-tool-use with a marker: p50 {} ms, p95 {} ms",
        p50 / 1000,
        p95 / 1000
    );
    // Generous for a loaded CI runner; the measured figures go in the plan.
    assert!(p95 < 500_000, "p95 {} ms", p95 / 1000);
}
