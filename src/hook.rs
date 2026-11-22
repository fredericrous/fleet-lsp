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
//! with nothing printed and a line in `<state>/hook.log`: a hook never
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
}

impl Outcome {
    fn silent() -> Outcome {
        Outcome::default()
    }

    fn logged(line: String) -> Outcome {
        Outcome {
            stdout: String::new(),
            log: Some(line),
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
    let dir = path.parent().expect("a marker has a directory");
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
    for f in fs::read_dir(dir).into_iter().flatten().flatten() {
        let old = f
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > MARKER_KEEP);
        if old {
            let _ = fs::remove_file(f.path());
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

// ------------------------------------------------------------ session start

fn session_start(msg: &Json, env: &Env) -> Outcome {
    let session = str_at(msg, &["session_id"]).unwrap_or("");
    let mark = marker(env, session);
    // A start, a resume, a clear or a compaction: whatever guidance the
    // session held may be gone, so the next contact may give it again.
    if let Some(m) = &mark {
        let _ = fs::remove_file(m);
    }
    let Some(cwd) = str_at(msg, &["cwd"]) else {
        return Outcome::logged(format!("SessionStart {session}: no cwd"));
    };
    let Some(lang) = project_here(Path::new(cwd)) else {
        return Outcome::silent();
    };
    if let Some(m) = &mark {
        let _ = claim(m);
    }
    Outcome {
        stdout: GUIDANCE.to_string(),
        log: Some(format!(
            "SessionStart {session}: guided ({} project in {cwd})",
            lang.name()
        )),
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

// ------------------------------------------------------------ post tool use

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

// ----------------------------------------------------------- subagent start

fn subagent_start(msg: &Json, env: &Env) -> Outcome {
    let agent = str_at(msg, &["agent_type"]).unwrap_or("");
    let cwd = str_at(msg, &["cwd"]).map(Path::new);
    if has_lsp(agent, cwd, env.home) == Some(false) {
        return Outcome::logged(format!("SubagentStart {agent}: no LSP tool, skipped"));
    }
    Outcome {
        stdout: context_json(Event::SubagentStart),
        log: None,
    }
}

/// Whether agent `agent` may call the LSP tool: `None` when that cannot be
/// told (no definition found, or a syntax this does not read), which the
/// caller treats as yes.
pub(crate) fn has_lsp(agent: &str, cwd: Option<&Path>, home: Option<&Path>) -> Option<bool> {
    if BUILTIN_WITHOUT_LSP.contains(&agent) {
        return Some(false);
    }
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
    let front = dirs.iter().find_map(|d| definition(d, name))?;
    allows_lsp(&front)
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
        .and_then(|r| r.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
        .unwrap_or(s)
}

fn scalar(front: &[String], key: &str) -> Option<String> {
    front.iter().find_map(|l| {
        let rest = l.strip_prefix(key)?.strip_prefix(':')?;
        Some(unquote(rest).to_string())
    })
}

/// A list field: absent → `Ok(None)`; a comma string, a flow list
/// (`[a, b]`) or a block list (`- a` lines) → its items; anything else →
/// `Err(())`, which the caller reads as "cannot tell".
fn list(front: &[String], key: &str) -> Result<Option<Vec<String>>, ()> {
    let Some(at) = front
        .iter()
        .position(|l| l.strip_prefix(key).is_some_and(|r| r.starts_with(':')))
    else {
        return Ok(None);
    };
    let value = front[at][key.len() + 1..].trim();
    let split = |s: &str| -> Vec<String> {
        s.split(',')
            .map(unquote)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    };
    if value.is_empty() {
        let items: Vec<&String> = front[at + 1..]
            .iter()
            .take_while(|l| l.starts_with(' ') || l.starts_with('\t') || l.starts_with('-'))
            .collect();
        let mut out = Vec::new();
        for l in items {
            let item = l.trim_start().strip_prefix('-').ok_or(())?;
            out.push(unquote(item).to_string());
        }
        return Ok(Some(out));
    }
    if let Some(inner) = value.strip_prefix('[') {
        let inner = inner.strip_suffix(']').ok_or(())?;
        return Ok(Some(split(inner)));
    }
    if value.starts_with(['{', '|', '>', '&', '*']) {
        return Err(());
    }
    Ok(Some(split(value)))
}

/// What the `tools` / `disallowedTools` front matter says of LSP.
fn allows_lsp(front: &[String]) -> Option<bool> {
    let is_lsp = |t: &String| t == "LSP" || t.starts_with("LSP(");
    if let Some(deny) = list(front, "disallowedTools").ok()? {
        if deny.iter().any(is_lsp) {
            return Some(false);
        }
    }
    match list(front, "tools").ok()? {
        None => Some(true),
        Some(tools) => Some(tools.iter().any(|t| is_lsp(t) || t == "*")),
    }
}

/// Appends `line` to `<state>/hook.log`; a failure is dropped.
pub(crate) fn log_line(state: &Path, line: &str) {
    let _ = fs::create_dir_all(state).and_then(|()| {
        let mut f = File::options()
            .create(true)
            .append(true)
            .open(state.join("hook.log"))?;
        writeln!(
            f,
            "{} {}",
            crate::log::rfc3339(SystemTime::now()),
            line.replace('\n', " | ")
        )
    });
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
            let _ = fs::remove_dir_all(&dir);
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
            let _ = fs::remove_dir_all(&self.0);
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
}
