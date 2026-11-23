//! `fleet-lsp hook <event>`: the plugin's context for the agent, given only
//! where it serves (guidance.session-load: an index, loaded on demand).
//!
//! - `session-start`: the guidance when the session starts inside a project
//!   of a served language; else nothing, and the first contact gives it.
//! - `post-tool-use`: the guidance once per session, the first time a tool
//!   touches a file of a served language. A marker under
//!   `<state>/guided/<session_id>`, claimed with `create_new`, makes it once
//!   even when parallel tool calls run parallel hooks.
//! - `subagent-start`: the guidance, unless the agent's definition shows it
//!   has no LSP tool.
//!
//! Claude Code passes the hook's JSON on stdin. Every input problem exits 0
//! with nothing printed and a line in `<state>/hook/<date>.log`: a hook never
//! raises an error in the person's session. Only usage errors exit 2.

use crate::cli::Lang;
use crate::json::{self, Json};
use crate::resolve;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The session-start index (ADR-0026): at most 6 lines.
pub(crate) const GUIDANCE: &str = "\
Code intelligence (fleet-lsp, .rs .go .py .ts/.js), wherever the LSP tool is available (named only as a deferred tool? load it first: ToolSearch select:LSP):
- Navigate with the LSP tool, not grep/rg/cat or a whole-file Read: workspaceSymbol to find a name, then goToDefinition, findReferences, hover, incomingCalls; Read only the range LSP points to. Grep/Glob are for text: comments, strings, config.
- Before a rename or a signature change: findReferences for the call sites, then Grep for what LSP cannot see (strings, docs, dynamic calls).
- After an edit, diagnostics arrive on their own: fix each type error or missing import before moving on.
- Each repository you query gets its own server (worktrees included); workspaceSymbol searches the repository of the file opened or used most recently, so open a file (documentSymbol, hover) first when the session did not start in a repository.
- A slow first answer is the server loading. An error that names a fix (`fleet-lsp doctor` explains it): apply the fix or tell the person; do not fall back to Grep without saying so.
";

/// The extensions the plugin's `lspServers` serve (`plugin.json`).
pub(crate) const SERVED: [&str; 12] = [
    "rs", "go", "py", "pyi", "ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs",
];

/// Built-in agents with no LSP tool; they have no definition file to read.
const BUILTIN_WITHOUT_LSP: [&str; 2] = ["claude-code-guide", "statusline-setup"];

/// How long a marker outlives its session.
const MARKER_KEEP: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    SessionStart,
    PostToolUse,
    SubagentStart,
}

impl Event {
    pub(crate) const NAMES: &'static str = "session-start|post-tool-use|subagent-start";

    pub(crate) fn parse(s: &str) -> Option<Event> {
        match s {
            "session-start" => Some(Event::SessionStart),
            "post-tool-use" => Some(Event::PostToolUse),
            "subagent-start" => Some(Event::SubagentStart),
            _ => None,
        }
    }

    fn hook_event_name(self) -> &'static str {
        match self {
            Event::SessionStart => "SessionStart",
            Event::PostToolUse => "PostToolUse",
            Event::SubagentStart => "SubagentStart",
        }
    }
}

/// Where a hook looks, passed in so tests never touch the real state.
pub(crate) struct Env<'a> {
    /// `log::log_dir()`: markers and the hook log live under it.
    pub(crate) state: &'a Path,
    /// `$HOME`, for `~/.claude/agents` and the installed plugins.
    pub(crate) home: Option<&'a Path>,
}

/// What a hook prints, and the line it logs.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub(crate) stdout: String,
    pub(crate) log: Option<String>,
    /// The marker this call claimed: the caller removes it when `stdout`
    /// cannot be written, so a lost write does not use up the session.
    pub(crate) claimed: Option<PathBuf>,
}

impl Outcome {
    fn silent() -> Outcome {
        Outcome::default()
    }

    fn logged(line: String) -> Outcome {
        Outcome {
            log: Some(line),
            ..Outcome::default()
        }
    }
}

pub(crate) fn run(event: Event, input: &str, env: &Env) -> Outcome {
    let Ok(msg) = json::parse(input) else {
        return Outcome::logged(format!(
            "{}: input is not JSON ({} bytes)",
            event.hook_event_name(),
            input.len()
        ));
    };
    match event {
        Event::SessionStart => session_start(&msg, env),
        Event::PostToolUse => post_tool_use(&msg, env),
        Event::SubagentStart => subagent_start(&msg, env),
    }
}

fn str_at<'a>(msg: &'a Json, path: &[&str]) -> Option<&'a str> {
    let mut cur = msg;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str()
}

/// `<state>/guided/<session_id>`, for an id of `[A-Za-z0-9._-]` only (as
/// `log.rs` keeps repository names) that is not `.` or `..`.
fn marker(env: &Env, session: &str) -> Option<PathBuf> {
    let ok = !session.is_empty()
        && session != "."
        && session != ".."
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    ok.then(|| env.state.join("guided").join(session))
}

/// Claims `path`: true only for the one call that created it.
fn claim(path: &Path) -> io::Result<bool> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a marker with no directory"))?;
    fs::create_dir_all(dir)?;
    match File::options().write(true).create_new(true).open(path) {
        Ok(_) => {
            prune_markers(dir, SystemTime::now());
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

fn prune_markers(dir: &Path, now: SystemTime) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > MARKER_KEEP);
        if old {
            let _ = fs::remove_file(entry.path()); // best effort: the next claim prunes again
        }
    }
}

fn context_json(event: Event) -> String {
    let out = Json::obj().set(
        "hookSpecificOutput",
        Json::obj()
            .set("hookEventName", event.hook_event_name())
            .set("additionalContext", GUIDANCE),
    );
    format!("{out}\n")
}

fn session_start(msg: &Json, env: &Env) -> Outcome {
    let session = str_at(msg, &["session_id"]).unwrap_or("");
    let mark = marker(env, session);
    // A start, a resume, a clear or a compaction: whatever guidance the
    // session held may be gone, so the next contact may give it again.
    if let Some(m) = &mark {
        let _ = fs::remove_file(m); // absent is the state this wants
    }
    let Some(cwd) = str_at(msg, &["cwd"]) else {
        return Outcome::logged(format!("SessionStart {session}: no cwd"));
    };
    let Some(lang) = project_here(Path::new(cwd)) else {
        return Outcome::silent();
    };
    let (claimed, unmarked) = match mark.as_deref().map(claim) {
        Some(Ok(true)) => (mark, String::new()),
        Some(Ok(false)) | None => (None, String::new()),
        Some(Err(e)) => (None, format!("; marker not written: {e}")),
    };
    Outcome {
        stdout: GUIDANCE.to_string(),
        log: Some(format!(
            "SessionStart {session}: guided ({} project in {cwd}){unmarked}",
            lang.name()
        )),
        claimed,
    }
}

/// The first served language with a project at or under `cwd`, inside its
/// git repository.
fn project_here(cwd: &Path) -> Option<Lang> {
    let git = resolve::git_root(cwd)?;
    Lang::ALL
        .into_iter()
        .find(|&lang| resolve::has_project(lang, cwd, &git))
}

fn post_tool_use(msg: &Json, env: &Env) -> Outcome {
    // A subagent's tool call: SubagentStart already decided its guidance, and
    // its contact must not use up the main conversation's.
    if msg.get("agent_id").is_some() {
        return Outcome::silent();
    }
    let session = str_at(msg, &["session_id"]).unwrap_or("");
    let Some(mark) = marker(env, session) else {
        return Outcome::logged(format!("PostToolUse: unusable session_id {session:?}"));
    };
    if mark.exists() {
        return Outcome::silent();
    }
    let Some(file) = str_at(msg, &["tool_input", "file_path"]) else {
        return Outcome::silent();
    };
    if !is_served(file) {
        return Outcome::silent();
    }
    match claim(&mark) {
        Ok(true) => Outcome {
            stdout: context_json(Event::PostToolUse),
            log: Some(format!("PostToolUse {session}: guided on {file}")),
            claimed: Some(mark),
        },
        Ok(false) => Outcome::silent(),
        Err(e) => Outcome::logged(format!(
            "PostToolUse {session}: cannot write {}: {e}",
            mark.display()
        )),
    }
}

pub(crate) fn is_served(file: &str) -> bool {
    Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| SERVED.contains(&e))
}

fn subagent_start(msg: &Json, env: &Env) -> Outcome {
    let agent = str_at(msg, &["agent_type"]).unwrap_or("");
    let cwd = str_at(msg, &["cwd"]).map(Path::new);
    if lsp_access(agent, cwd, env.home) == ToolAccess::Denied {
        return Outcome::logged(format!("SubagentStart {agent}: no LSP tool, skipped"));
    }
    Outcome {
        stdout: context_json(Event::SubagentStart),
        ..Outcome::default()
    }
}

/// Whether an agent may call the LSP tool, as its definition says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolAccess {
    Allowed,
    Denied,
    /// No definition found, or a syntax this does not read: treated as
    /// allowed, today's behaviour.
    Unknown,
}

pub(crate) fn lsp_access(agent: &str, cwd: Option<&Path>, home: Option<&Path>) -> ToolAccess {
    if BUILTIN_WITHOUT_LSP.contains(&agent) {
        return ToolAccess::Denied;
    }
    definition_of(agent, cwd, home).map_or(ToolAccess::Unknown, |front| allows_lsp(&front))
}

/// The front matter of agent `agent`'s definition.
fn definition_of(agent: &str, cwd: Option<&Path>, home: Option<&Path>) -> Option<Vec<String>> {
    let (dirs, name): (Vec<PathBuf>, &str) = match agent.split_once(':') {
        Some((plugin, name)) => (vec![plugin_agents(plugin, home?)?], name),
        None => {
            let mut dirs = Vec::new();
            if let Some(cwd) = cwd {
                dirs.push(cwd.join(".claude/agents"));
            }
            if let Some(home) = home {
                dirs.push(home.join(".claude/agents"));
            }
            (dirs, agent)
        }
    };
    dirs.iter().find_map(|d| definition(d, name))
}

/// `agents/` of the one installed plugin named `plugin`, from Claude Code's
/// `installed_plugins.json`; `None` unless exactly one install path names it.
fn plugin_agents(plugin: &str, home: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(home.join(".claude/plugins/installed_plugins.json")).ok()?;
    let doc = json::parse(&text).ok()?;
    let Some(Json::Obj(plugins)) = doc.get("plugins") else {
        return None;
    };
    let mut paths: Vec<&str> = plugins
        .iter()
        .filter(|(key, _)| key.split('@').next() == Some(plugin))
        .flat_map(|(_, installs)| installs.as_arr().unwrap_or(&[]))
        .filter_map(|i| i.get("installPath")?.as_str())
        .collect();
    paths.sort_unstable();
    paths.dedup();
    match paths.as_slice() {
        [one] => Some(Path::new(one).join("agents")),
        _ => None,
    }
}

/// The front matter of the definition in `dir` whose `name` is `name`:
/// `<name>.md` first, then every other `.md`.
fn definition(dir: &Path, name: &str) -> Option<Vec<String>> {
    let named = |path: &Path| {
        let text = fs::read_to_string(path).ok()?;
        let front = front_matter(&text)?;
        (scalar(&front, "name").as_deref() == Some(name)).then_some(front)
    };
    let first = dir.join(format!("{name}.md"));
    if let Some(front) = named(&first) {
        return Some(front);
    }
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md") && *p != first)
        .collect();
    files.sort();
    files.iter().find_map(|p| named(p))
}

fn front_matter(text: &str) -> Option<Vec<String>> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let mut out = Vec::new();
    for line in lines {
        if line.trim_end() == "---" {
            return Some(out);
        }
        out.push(line.to_string());
    }
    None
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            s.strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .unwrap_or(s)
}

fn scalar(front: &[String], key: &str) -> Option<String> {
    front.iter().find_map(|l| {
        let rest = l.strip_prefix(key)?.strip_prefix(':')?;
        Some(unquote(rest).to_string())
    })
}

/// A front-matter list field, read.
#[derive(Debug, PartialEq, Eq)]
enum ToolList {
    Absent,
    /// A comma string, a flow list (`[a, b]`) or a block list (`- a` lines).
    Listed(Vec<String>),
    /// A syntax this does not read.
    Unreadable,
}

fn list(front: &[String], key: &str) -> ToolList {
    let Some(at) = front.iter().position(|l| {
        l.strip_prefix(key)
            .is_some_and(|rest| rest.starts_with(':'))
    }) else {
        return ToolList::Absent;
    };
    let inline = front[at][key.len() + 1..].trim();
    let split = |s: &str| -> Vec<String> {
        s.split(',')
            .map(unquote)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    };
    if inline.is_empty() {
        let items: Vec<&String> = front[at + 1..]
            .iter()
            .take_while(|l| l.starts_with(' ') || l.starts_with('\t') || l.starts_with('-'))
            .collect();
        let mut out = Vec::new();
        for l in items {
            let Some(entry) = l.trim_start().strip_prefix('-') else {
                return ToolList::Unreadable;
            };
            out.push(unquote(entry).to_string());
        }
        return ToolList::Listed(out);
    }
    if let Some(inner) = inline.strip_prefix('[') {
        return inner
            .strip_suffix(']')
            .map_or(ToolList::Unreadable, |i| ToolList::Listed(split(i)));
    }
    if inline.starts_with(['{', '|', '>', '&', '*']) {
        return ToolList::Unreadable;
    }
    ToolList::Listed(split(inline))
}

/// What the `tools` / `disallowedTools` front matter says of LSP.
fn allows_lsp(front: &[String]) -> ToolAccess {
    let is_lsp = |t: &String| t == "LSP" || t.starts_with("LSP(");
    match list(front, "disallowedTools") {
        ToolList::Unreadable => return ToolAccess::Unknown,
        ToolList::Listed(deny) if deny.iter().any(is_lsp) => return ToolAccess::Denied,
        ToolList::Listed(_) | ToolList::Absent => {}
    }
    match list(front, "tools") {
        ToolList::Unreadable => ToolAccess::Unknown,
        ToolList::Absent => ToolAccess::Allowed,
        ToolList::Listed(tools) if tools.iter().any(|t| is_lsp(t) || t == "*") => {
            ToolAccess::Allowed
        }
        ToolList::Listed(_) => ToolAccess::Denied,
    }
}

/// Writes `outcome.stdout` for Claude Code, then logs `outcome.log`, marked
/// when the write failed. A failed write gives the claimed marker back, so a
/// lost delivery does not use up the session's guidance.
pub(crate) fn deliver(outcome: &Outcome, out: &mut impl Write, state: &Path) {
    let written = out
        .write_all(outcome.stdout.as_bytes())
        .and_then(|()| out.flush());
    let line = match (&outcome.log, &written) {
        (Some(line), Ok(())) => Some(line.clone()),
        (Some(line), Err(e)) => Some(format!("{line} (not delivered: {e})")),
        (None, Ok(())) => None,
        (None, Err(e)) => Some(format!("hook: guidance not delivered: {e}")),
    };
    let kept = match (&written, &outcome.claimed) {
        (Err(_), Some(mark)) => match fs::remove_file(mark) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Some(e),
            Ok(()) | Err(_) => None,
        },
        _ => None,
    };
    let line = match (line, kept) {
        (Some(line), Some(e)) => Some(format!("{line} (marker kept: {e})")),
        (line, _) => line,
    };
    if let Some(line) = line {
        log_line(state, &line);
    }
}

/// Appends `line` to `<state>/hook/<utc date>.log`, one file a day so the
/// session logs' pruning (`log::prune`, 14 days) bounds it; a failure is
/// dropped (logging never fails a hook).
pub(crate) fn log_line(state: &Path, line: &str) {
    let dir = state.join("hook");
    let now = SystemTime::now();
    let stamp = crate::log::rfc3339(now);
    let day = stamp.get(..10).unwrap_or("undated");
    let written = fs::create_dir_all(&dir).and_then(|()| {
        crate::log::prune(&dir, now);
        let mut log_file = File::options()
            .create(true)
            .append(true)
            .open(dir.join(format!("{day}.log")))?;
        writeln!(log_file, "{stamp} {}", line.replace('\n', " | "))
    });
    drop(written); // nowhere left to report a log that cannot be written
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory: `state/`, `home/`, and repositories under it.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir =
                std::env::temp_dir().join(format!("fleet-lsp-hook-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir); // a leftover scratch dir is replaced below
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn file(&self, rel: &str, text: &str) -> PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, text).unwrap();
            p
        }

        fn run(&self, event: Event, input: &str) -> Outcome {
            let state = self.0.join("state");
            let home = self.0.join("home");
            run(
                event,
                input,
                &Env {
                    state: &state,
                    home: Some(&home),
                },
            )
        }

        fn marker(&self, session: &str) -> PathBuf {
            self.0.join("state/guided").join(session)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0); // a leftover scratch dir does not fail a test
        }
    }

    fn start(session: &str, cwd: &Path) -> String {
        Json::obj()
            .set("session_id", session)
            .set("cwd", cwd.display().to_string())
            .set("source", "startup")
            .to_string()
    }

    fn touch(session: &str, file: &str) -> String {
        Json::obj()
            .set("session_id", session)
            .set("tool_name", "Read")
            .set("tool_input", Json::obj().set("file_path", file))
            .to_string()
    }

    fn subagent(agent: &str, cwd: &Path) -> String {
        Json::obj()
            .set("session_id", "s")
            .set("agent_id", "agent-1")
            .set("agent_type", agent)
            .set("cwd", cwd.display().to_string())
            .to_string()
    }

    fn injects(o: &Outcome, event: &str) -> bool {
        o.stdout.contains(&format!(r#""hookEventName":"{event}""#))
            && o.stdout.contains("Code intelligence (fleet-lsp")
    }

    #[test]
    fn a_start_outside_any_project_is_silent_and_one_inside_guides() {
        let s = Scratch::new("start");
        let o = s.run(Event::SessionStart, &start("s1", &s.0));
        assert_eq!(o.stdout, "");
        assert!(!s.marker("s1").exists());
        s.file("repo/.git/HEAD", "");
        s.file("repo/Cargo.toml", "[package]\n");
        let o = s.run(Event::SessionStart, &start("s2", &s.0.join("repo")));
        assert_eq!(o.stdout, GUIDANCE);
        assert!(GUIDANCE.lines().count() <= 6);
        assert!(
            s.marker("s2").exists(),
            "the start used up the first contact"
        );
    }

    /// FALSIFY: drop the `mark.exists()` / `claim` checks in `post_tool_use`.
    #[test]
    fn the_first_served_file_guides_once() {
        let s = Scratch::new("touch");
        assert_eq!(
            s.run(Event::PostToolUse, &touch("t1", "/x/a.md")).stdout,
            ""
        );
        let o = s.run(Event::PostToolUse, &touch("t1", "/x/a.rs"));
        assert!(injects(&o, "PostToolUse"), "{o:?}");
        assert!(o.log.is_some());
        let again = s.run(Event::PostToolUse, &touch("t1", "/x/b.ts"));
        assert_eq!(again, Outcome::silent(), "the marker path logs nothing");
    }

    #[test]
    fn a_restart_or_compaction_clears_the_marker() {
        let s = Scratch::new("compact");
        assert!(injects(
            &s.run(Event::PostToolUse, &touch("c1", "/x/a.go")),
            "PostToolUse"
        ));
        let compact = start("c1", &s.0).replace("startup", "compact");
        assert_eq!(s.run(Event::SessionStart, &compact).stdout, "");
        assert!(!s.marker("c1").exists());
        assert!(injects(
            &s.run(Event::PostToolUse, &touch("c1", "/x/a.go")),
            "PostToolUse"
        ));
    }

    #[test]
    fn a_subagents_tool_call_never_uses_up_the_session() {
        let s = Scratch::new("sub-touch");
        let input = touch("u1", "/x/a.py").replace("{", r#"{"agent_id":"agent-9","#);
        assert_eq!(s.run(Event::PostToolUse, &input), Outcome::silent());
        assert!(!s.marker("u1").exists());
    }

    #[test]
    fn parallel_first_contacts_guide_once() {
        let s = Scratch::new("parallel");
        let state = s.0.join("state");
        let outs: Vec<Outcome> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        run(
                            Event::PostToolUse,
                            &touch("p1", "/x/a.rs"),
                            &Env {
                                state: &state,
                                home: None,
                            },
                        )
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(outs.iter().filter(|o| !o.stdout.is_empty()).count(), 1);
    }

    #[test]
    fn an_unsafe_session_id_writes_nothing() {
        let s = Scratch::new("unsafe");
        for id in ["../x", "a/b", "..", ""] {
            let o = s.run(Event::PostToolUse, &touch(id, "/x/a.rs"));
            assert_eq!(o.stdout, "", "{id}");
        }
        assert!(!s.0.join("state/x").exists());
        assert!(!s.0.join("x").exists());
    }

    struct Closed;

    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// FALSIFY: drop the `remove_file` in `deliver`.
    #[test]
    fn a_lost_delivery_gives_the_marker_back() {
        let s = Scratch::new("lost");
        let outcome = s.run(Event::PostToolUse, &touch("l1", "/x/a.rs"));
        assert!(s.marker("l1").exists());
        deliver(&outcome, &mut Closed, &s.0.join("state"));
        assert!(!s.marker("l1").exists(), "the next contact guides");
        let log: String = fs::read_dir(s.0.join("state/hook"))
            .unwrap()
            .flatten()
            .map(|e| fs::read_to_string(e.path()).unwrap())
            .collect();
        assert!(log.contains("guided on /x/a.rs (not delivered:"), "{log}");
        assert_eq!(log.lines().count(), 1, "{log}");
    }

    #[test]
    fn input_that_is_not_json_is_logged_not_printed() {
        let s = Scratch::new("garbage");
        let o = s.run(Event::PostToolUse, "not json");
        assert_eq!(o.stdout, "");
        assert!(o.log.unwrap().contains("not JSON"));
    }

    #[test]
    fn subagents_without_lsp_get_nothing() {
        let s = Scratch::new("agents");
        let agents = "home/.claude/agents";
        s.file(
            &format!("{agents}/no-lsp.md"),
            "---\nname: no-lsp\ntools: Read, Bash\n---\nx\n",
        );
        s.file(
            &format!("{agents}/lsp.md"),
            "---\nname: lsp\ntools: Read, LSP\n---\nx\n",
        );
        s.file(
            &format!("{agents}/all.md"),
            "---\nname: all\nmodel: inherit\n---\nx\n",
        );
        // The file name is not the agent's name.
        s.file(
            &format!("{agents}/renamed.md"),
            "---\nname: other-name\ntools: Grep\n---\nx\n",
        );
        s.file(
            &format!("{agents}/block.md"),
            "---\nname: block\ntools:\n  - Read\n  - LSP\n---\n",
        );
        s.file(
            &format!("{agents}/flow.md"),
            "---\nname: flow\ntools: [Read, Grep]\n---\n",
        );
        s.file(
            &format!("{agents}/deny.md"),
            "---\nname: deny\ndisallowedTools: LSP\n---\n",
        );
        s.file(
            &format!("{agents}/odd.md"),
            "---\nname: odd\ntools: {Read: true}\n---\n",
        );
        let run = |agent: &str| s.run(Event::SubagentStart, &subagent(agent, &s.0));
        for skipped in ["no-lsp", "other-name", "flow", "deny", "claude-code-guide"] {
            let o = run(skipped);
            assert_eq!(o.stdout, "", "{skipped}");
            assert!(o.log.is_some(), "{skipped}");
        }
        for guided in ["lsp", "all", "block", "odd", "Explore", "unknown-agent"] {
            assert!(injects(&run(guided), "SubagentStart"), "{guided}");
        }
    }

    #[test]
    fn a_plugin_agent_is_read_from_its_one_install() {
        let s = Scratch::new("plugin-agents");
        let install = s.0.join("cache/tools/1.0.0");
        s.file(
            "cache/tools/1.0.0/agents/browser.md",
            "---\nname: browser\ntools: Read\n---\n",
        );
        let installed = Json::obj().set(
            "plugins",
            Json::obj().set(
                "tools@market",
                Json::Arr(vec![
                    Json::obj().set("installPath", install.display().to_string())
                ]),
            ),
        );
        s.file(
            "home/.claude/plugins/installed_plugins.json",
            &installed.to_string(),
        );
        let o = s.run(Event::SubagentStart, &subagent("tools:browser", &s.0));
        assert_eq!(o.stdout, "");
        let o = s.run(Event::SubagentStart, &subagent("absent:browser", &s.0));
        assert!(injects(&o, "SubagentStart"));
    }

    /// The mod (`plugins/fleet-lsp/hooks/register.ts`) gives the first-contact
    /// note itself: its single-quoted `GUIDANCE_LINES` must be this text, in
    /// order. FALSIFY: change one word on either side.
    #[test]
    fn the_mod_gives_the_same_guidance() {
        let module = include_str!("../plugins/fleet-lsp/hooks/register.ts");
        let start = module
            .find("const GUIDANCE_LINES = [")
            .expect("register.ts declares GUIDANCE_LINES");
        let end = start + module[start..].find("\n]").expect("its array closes");
        let lines: Vec<&str> = module[start..end]
            .lines()
            .skip(1)
            .map(|line| {
                let quoted = line.trim().strip_suffix(',').unwrap_or(line.trim());
                let inner = quoted
                    .strip_prefix('\'')
                    .and_then(|rest| rest.strip_suffix('\''))
                    .unwrap_or_else(|| panic!("not one single-quoted line: {line}"));
                assert!(
                    !inner.contains(['\\', '\'']),
                    "an escape this test does not read: {line}"
                );
                inner
            })
            .collect();
        assert_eq!(lines, GUIDANCE.lines().collect::<Vec<_>>());
    }

    /// The mod's `SERVED` pattern names exactly these extensions (its own test
    /// runs the pattern over them and over near misses).
    #[test]
    fn the_mod_serves_the_same_extensions() {
        let module = include_str!("../plugins/fleet-lsp/hooks/register.ts");
        let pattern = r"/\.(rs|go|pyi?|[jt]sx?|[mc][jt]s)$/";
        assert!(
            module.contains(&format!("export const SERVED = {pattern}")),
            "register.ts SERVED is no longer {pattern}: update this list"
        );
        // That pattern, spelled out.
        let mut spelled = vec!["rs", "go", "py", "pyi", "js", "jsx", "ts", "tsx"];
        spelled.extend(["mjs", "mts", "cjs", "cts"]);
        spelled.sort_unstable();
        let mut served = SERVED.to_vec();
        served.sort_unstable();
        assert_eq!(spelled, served);
    }
}
