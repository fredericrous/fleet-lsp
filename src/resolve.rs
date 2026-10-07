//! Which server answers, and whether it matches the repository's pin.
//!
//! Two roots are kept apart: the *git root* bounds every search, the
//! *project root* is where a language's manifest, pin and environment live
//! (a uv or npm/pnpm workspace member is lifted to its workspace root). The
//! verified binary is run by absolute path — never looked up on PATH again.

use crate::cli::Lang;
use crate::gate::Barrier;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Measured barriers (docs/readiness.md). Another version gets the narrowed
/// promise until it is measured with `scripts/lsp-probe.py`.
const PYRIGHT_MEASURED: &str = "1.1.411";
const TS_ADAPTER_MEASURED: &str = "6.0.1";

/// Directories the downward search never enters.
const SKIP_DIRS: [&str; 4] = ["node_modules", ".git", "target", ".venv"];
const DOWN_DEPTH: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The installed version equals the pin.
    Verified,
    Refused {
        reason: String,
        fix: Fix,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fix {
    /// A command to run, shown alone on its line.
    Command(String),
    /// Several lines (e.g. the directories to start a session in).
    Lines(String, Vec<String>),
    /// No command fixes it; the text says why.
    None(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Spawn {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    pub(crate) cwd: PathBuf,
    pub(crate) env_set: Vec<(String, String)>,
    pub(crate) env_remove: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Resolution {
    pub(crate) lang: Lang,
    pub(crate) git_root: Option<PathBuf>,
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) pin: Option<String>,
    pub(crate) server: Option<PathBuf>,
    pub(crate) version: Option<String>,
    pub(crate) verdict: Verdict,
    pub(crate) spawn: Option<Spawn>,
    pub(crate) barrier: Barrier,
    /// The barrier for this version is not measured.
    pub(crate) narrowed: bool,
    pub(crate) tsserver_path: Option<String>,
}

impl Resolution {
    fn refused(
        lang: Lang,
        git_root: Option<PathBuf>,
        project_root: Option<PathBuf>,
        reason: String,
        fix: Fix,
    ) -> Self {
        Resolution {
            lang,
            git_root,
            project_root,
            pin: None,
            server: None,
            version: None,
            verdict: Verdict::Refused { reason, fix },
            spawn: None,
            barrier: Barrier::None,
            narrowed: false,
            tsserver_path: None,
        }
    }

    /// The refusal as one line for an LSP error: reason, then the fix.
    pub(crate) fn refusal_text(&self) -> Option<String> {
        let Verdict::Refused { reason, fix } = &self.verdict else {
            return None;
        };
        let fix = match fix {
            Fix::Command(c) => format!("fix: {c}"),
            Fix::Lines(head, lines) => format!("fix: {head} {}", lines.join(", ")),
            Fix::None(why) => format!("fix: none — {why}"),
        };
        Some(format!("fleet-lsp: {}: {reason}; {fix}", self.lang))
    }
}

/// The nearest ancestor of `start` (itself included) holding `.git`.
pub(crate) fn git_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
}

enum Found {
    One(PathBuf),
    Several(Vec<PathBuf>),
    Nothing,
}

/// Is `lang` present at all around `start`? (doctor lists only these)
pub(crate) fn present(lang: Lang, start: &Path) -> bool {
    match git_root(start) {
        Some(g) => !matches!(project_root(lang, start, &g), Found::Nothing),
        None => false,
    }
}

pub(crate) fn resolve(lang: Lang, start: &Path) -> Resolution {
    let Some(git) = git_root(start) else {
        return Resolution::refused(
            lang,
            None,
            None,
            format!("not in a git repository: {}", start.display()),
            Fix::None("start the session inside a repository".into()),
        );
    };
    let root = match project_root(lang, start, &git) {
        Found::One(r) => r,
        Found::Several(list) => {
            let rel = list.iter().map(|p| rel_to(p, &git)).collect::<Vec<_>>();
            return Resolution::refused(
                lang,
                Some(git),
                None,
                format!("several {lang} projects in this repository"),
                Fix::Lines("start the session in one of:".into(), rel),
            );
        }
        Found::Nothing => {
            return Resolution::refused(
                lang,
                Some(git.clone()),
                None,
                format!("no {lang} project found here"),
                Fix::None("this repository has none".into()),
            )
        }
    };
    match lang {
        Lang::Rust => rust(git, root),
        Lang::Go => go(git, root, go_probe),
        Lang::Python => python(git, root),
        Lang::TypeScript => typescript(git, root, node_on_path),
    }
}

// ---------------------------------------------------------------- roots

fn markers(lang: Lang) -> &'static [&'static str] {
    match lang {
        Lang::Rust => &["Cargo.toml"],
        Lang::Go => &["go.work", "go.mod"],
        Lang::Python => &["pyproject.toml", ".venv"],
        Lang::TypeScript => &["package.json"],
    }
}

fn has_marker(lang: Lang, dir: &Path) -> bool {
    markers(lang).iter().any(|m| dir.join(m).exists())
}

fn project_root(lang: Lang, start: &Path, git: &Path) -> Found {
    let up: Vec<&Path> = start
        .ancestors()
        .take_while(|d| d.starts_with(git))
        .collect();
    let found = if lang == Lang::Go {
        // The nearest go.work wins over a nearer go.mod: it is the workspace.
        up.iter()
            .find(|d| d.join("go.work").exists())
            .or_else(|| up.iter().find(|d| d.join("go.mod").exists()))
            .map(|d| d.to_path_buf())
    } else {
        up.iter()
            .find(|d| has_marker(lang, d))
            .map(|d| d.to_path_buf())
    };
    if let Some(dir) = found {
        return Found::One(lift(lang, &dir, git));
    }
    let mut hits = Vec::new();
    walk_down(lang, start, 0, &mut hits);
    // A workspace member lifts to its root; several members of one
    // workspace are one project.
    let mut roots: Vec<PathBuf> = hits.iter().map(|h| lift(lang, h, git)).collect();
    roots.sort();
    roots.dedup();
    match roots.len() {
        0 => Found::Nothing,
        1 => Found::One(roots.remove(0)),
        _ => Found::Several(roots),
    }
}

fn walk_down(lang: Lang, dir: &Path, depth: usize, hits: &mut Vec<PathBuf>) {
    if depth > DOWN_DEPTH {
        return;
    }
    if lang == Lang::Go && is_tools_module(dir) {
        return;
    }
    if depth > 0 && has_marker(lang, dir) {
        hits.push(dir.to_path_buf());
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut subdirs: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| !SKIP_DIRS.iter().any(|s| e.file_name() == *s))
        .map(|e| e.path())
        .collect();
    subdirs.sort();
    for d in subdirs {
        walk_down(lang, &d, depth + 1, hits);
    }
}

/// A `tools/` module that only pins tools is not a Go project of its own.
fn is_tools_module(dir: &Path) -> bool {
    dir.file_name() == Some("tools".as_ref())
        && fs::read_to_string(dir.join("go.mod"))
            .is_ok_and(|t| t.lines().any(|l| l.trim() == "module tools"))
}

/// A uv or npm/pnpm workspace member resolves to the workspace root, where
/// the pin, the lockfile and the environment live.
fn lift(lang: Lang, dir: &Path, git: &Path) -> PathBuf {
    for anc in dir.ancestors().skip(1).take_while(|a| a.starts_with(git)) {
        let rel = rel_to(dir, anc);
        let members = match lang {
            Lang::Python => uv_members(anc),
            Lang::TypeScript => js_members(anc),
            _ => None,
        };
        if let Some((include, exclude)) = members {
            if include.iter().any(|g| glob_match(g, &rel))
                && !exclude.iter().any(|g| glob_match(g, &rel))
            {
                return anc.to_path_buf();
            }
        }
    }
    dir.to_path_buf()
}

fn uv_members(dir: &Path) -> Option<(Vec<String>, Vec<String>)> {
    let text = fs::read_to_string(dir.join("pyproject.toml")).ok()?;
    let section = toml_section(&text, "tool.uv.workspace")?;
    Some((
        toml_string_array(&section, "members"),
        toml_string_array(&section, "exclude"),
    ))
}

fn js_members(dir: &Path) -> Option<(Vec<String>, Vec<String>)> {
    if let Ok(text) = fs::read_to_string(dir.join("pnpm-workspace.yaml")) {
        let mut include = Vec::new();
        let mut exclude = Vec::new();
        let mut in_packages = false;
        for line in text.lines() {
            if !line.starts_with(' ') && !line.starts_with('-') {
                in_packages = line.trim_end() == "packages:";
                continue;
            }
            if in_packages {
                if let Some(item) = line.trim().strip_prefix('-') {
                    let item = item.trim().trim_matches(|c| c == '"' || c == '\'');
                    match item.strip_prefix('!') {
                        Some(ex) => exclude.push(ex.to_string()),
                        None => include.push(item.to_string()),
                    }
                }
            }
        }
        return Some((include, exclude));
    }
    let text = fs::read_to_string(dir.join("package.json")).ok()?;
    let json = crate::json::parse(&text).ok()?;
    let ws = json.get("workspaces")?;
    let list = ws
        .as_arr()
        .or_else(|| ws.get("packages").and_then(|p| p.as_arr()))?;
    Some((
        list.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Vec::new(),
    ))
}

/// `*` matches one path segment, `**` any number; everything else literally.
pub(crate) fn glob_match(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    fn go(p: &[&str], s: &[&str]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(&"**"), _) => go(&p[1..], s) || (!s.is_empty() && go(p, &s[1..])),
            (Some(pp), Some(ss)) => seg_match(pp, ss) && go(&p[1..], &s[1..]),
            _ => false,
        }
    }
    fn seg_match(p: &str, s: &str) -> bool {
        match p.split_once('*') {
            None => p == s,
            Some((pre, post)) => {
                s.len() >= pre.len() + post.len() && s.starts_with(pre) && s.ends_with(post)
            }
        }
    }
    go(&pat, &segs)
}

fn rel_to(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

// ---------------------------------------------------------------- rust

fn rust(git: PathBuf, root: PathBuf) -> Resolution {
    let refused = |reason: String, fix: Fix| {
        Resolution::refused(
            Lang::Rust,
            Some(git.clone()),
            Some(root.clone()),
            reason,
            fix,
        )
    };
    let Some((pin_file, channel)) = rust_pin(&root, &git) else {
        let stable = rustc_stable().unwrap_or_else(|| "<x.y.z>".into());
        return refused(
            "no rust-toolchain.toml pins the toolchain".into(),
            Fix::Command(format!(
                "printf '[toolchain]\\nchannel = \"{stable}\"\\n' > rust-toolchain.toml"
            )),
        );
    };
    let pin = format!("{} ({channel})", rel_to(&pin_file, &git));
    if !exact_channel(&channel) {
        let stable = rustc_stable().unwrap_or_else(|| "<x.y.z>".into());
        let mut r = refused(
            format!("channel `{channel}` floats; pin an exact version"),
            Fix::Command(format!(
                "set channel = \"{stable}\" in {}",
                rel_to(&pin_file, &git)
            )),
        );
        r.pin = Some(pin);
        return r;
    }
    let Some(rustup) = which("rustup") else {
        return refused(
            "rustup is not installed".into(),
            Fix::Command("see https://rustup.rs".into()),
        );
    };
    let out = Command::new(&rustup)
        .args(["which", "--toolchain", &channel, "rust-analyzer"])
        .current_dir(&root)
        .output();
    let ra = match out {
        Ok(o) if o.status.success() => PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let fix = if err.contains("not installed")
                && err.contains("toolchain")
                && !err.contains("rust-analyzer")
            {
                format!("rustup toolchain install {channel} --component rust-analyzer")
            } else {
                format!("rustup component add rust-analyzer --toolchain {channel}")
            };
            let mut r = refused(
                format!("no rust-analyzer for toolchain {channel}"),
                Fix::Command(fix),
            );
            r.pin = Some(pin);
            return r;
        }
        Err(e) => {
            return refused(
                format!("running rustup: {e}"),
                Fix::None("rustup could not be run".into()),
            )
        }
    };
    let in_toolchain = ra.components().any(|c| {
        c.as_os_str()
            .to_string_lossy()
            .starts_with(&format!("{channel}-"))
    });
    if !in_toolchain {
        let mut r = refused(
            format!(
                "rust-analyzer at {} is not from toolchain {channel}",
                ra.display()
            ),
            Fix::Command(format!(
                "rustup component add rust-analyzer --toolchain {channel}"
            )),
        );
        r.pin = Some(pin);
        return r;
    }
    let version = first_line(&ra, &["--version"]);
    Resolution {
        lang: Lang::Rust,
        git_root: Some(git),
        project_root: Some(root.clone()),
        pin: Some(pin),
        server: Some(ra.clone()),
        version,
        verdict: Verdict::Verified,
        spawn: Some(Spawn {
            program: ra,
            args: Vec::new(),
            cwd: root,
            env_set: vec![("RUSTUP_TOOLCHAIN".into(), channel)],
            env_remove: Vec::new(),
        }),
        barrier: Barrier::RustAnalyzer,
        narrowed: false,
        tsserver_path: None,
    }
}

fn rust_pin(root: &Path, git: &Path) -> Option<(PathBuf, String)> {
    for dir in root.ancestors().take_while(|d| d.starts_with(git)) {
        for name in ["rust-toolchain.toml", "rust-toolchain"] {
            let path = dir.join(name);
            if let Ok(text) = fs::read_to_string(&path) {
                let channel = toml_section(&text, "toolchain")
                    .and_then(|s| toml_string(&s, "channel"))
                    .or_else(|| {
                        // Legacy `rust-toolchain`: the channel alone.
                        let t = text.trim();
                        (!t.is_empty() && !t.contains('\n') && !t.contains('='))
                            .then(|| t.to_string())
                    })?;
                return Some((path, channel));
            }
        }
    }
    None
}

pub(crate) fn exact_channel(ch: &str) -> bool {
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let parts: Vec<&str> = ch.split('.').collect();
    if parts.len() == 3 && parts.iter().all(|p| numeric(p)) {
        return true;
    }
    for pre in ["nightly-", "beta-"] {
        if let Some(date) = ch.strip_prefix(pre) {
            let d: Vec<&str> = date.split('-').collect();
            return d.len() == 3 && d.iter().all(|p| numeric(p)) && d[0].len() == 4;
        }
    }
    false
}

fn rustc_stable() -> Option<String> {
    let out = Command::new(which("rustup")?)
        .args(["run", "stable", "rustc", "--version"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().nth(1).map(str::to_string)
}

// ---------------------------------------------------------------- go

/// What the resolver needs from the machine for Go, probed once.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GoProbe {
    /// Whether git ignores the project root (a stale copy inside another
    /// repository), or why git could not say.
    pub(crate) ignored: Result<bool, String>,
    /// The `go` on PATH, or `None` if there is none.
    pub(crate) bin: Option<PathBuf>,
    /// `go env GOVERSION` under `GOTOOLCHAIN=local`, without the `go` prefix.
    pub(crate) version: Option<String>,
    /// `go env GOWORK` in the project root: a go.work in scope.
    pub(crate) gowork: Option<String>,
}

/// The pinned gopls fleet-lsp's fix installs.
const GOPLS_PIN: &str = "v0.23.0";

fn go_probe(git: &Path, root: &Path) -> GoProbe {
    let ignored = if root == git {
        Ok(false)
    } else {
        // `check-ignore -q`: 0 ignored, 1 not ignored, anything else fatal.
        let out = Command::new("git")
            .arg("-C")
            .arg(git)
            .args(["check-ignore", "-q", "--"])
            .arg(rel_to(root, git))
            .output();
        match out {
            Ok(o) if o.status.code() == Some(0) => Ok(true),
            Ok(o) if o.status.code() == Some(1) => Ok(false),
            Ok(o) => Err(String::from_utf8_lossy(&o.stderr)
                .lines()
                .next()
                .unwrap_or("no output")
                .trim()
                .to_string()),
            Err(e) => Err(e.to_string()),
        }
    };
    let bin = which("go");
    let go_env = |var: &str| -> Option<String> {
        let out = Command::new(bin.as_ref()?)
            .args(["env", var])
            .current_dir(root)
            .env("GOTOOLCHAIN", "local")
            .output()
            .ok()?;
        let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !v.is_empty() && v != "off").then_some(v)
    };
    GoProbe {
        ignored,
        version: go_env("GOVERSION").map(|v| v.trim_start_matches("go").to_string()),
        gowork: go_env("GOWORK"),
        bin,
    }
}

/// gopls runs from the repository's pin, built by the machine's Go under
/// `GOTOOLCHAIN=local` (a stated deviation from build.toolchain-source): the
/// `tool` directive in the module's go.mod, or in the nearest `tools/go.mod`
/// from the project root up to the git root, run with `-modfile`.
fn go(git: PathBuf, root: PathBuf, probe: impl FnOnce(&Path, &Path) -> GoProbe) -> Resolution {
    let refused = |reason: String, fix: Fix| {
        Resolution::refused(Lang::Go, Some(git.clone()), Some(root.clone()), reason, fix)
    };
    let p = probe(&git, &root);
    match p.ignored {
        Ok(false) => {}
        Ok(true) => {
            return refused(
                format!("{} is ignored by git", rel_to(&root, &git)),
                Fix::None("open the real repository".into()),
            )
        }
        Err(why) => {
            return refused(
                format!("git check-ignore failed: {why}"),
                Fix::None(format!(
                    "run git check-ignore {} to see why",
                    rel_to(&root, &git)
                )),
            )
        }
    }
    let Some(go_bin) = p.bin else {
        return refused(
            "go is not installed".into(),
            Fix::Command("brew install go".into()),
        );
    };
    let Some(local) = p.version else {
        return refused(
            "go env GOVERSION failed".into(),
            Fix::None("run go env GOVERSION to see why".into()),
        );
    };
    let gomod = fs::read_to_string(root.join("go.mod")).unwrap_or_default();
    let project = if gomod.is_empty() {
        (
            "go.work",
            fs::read_to_string(root.join("go.work")).unwrap_or_default(),
        )
    } else {
        ("go.mod", gomod.clone())
    };
    let (pin_file, pin_text, modfile) = if has_gopls_tool(&gomod) {
        (root.join("go.mod"), gomod, false)
    } else if let Some((path, text)) = tools_module(&root, &git) {
        (path, text, true)
    } else {
        return refused(
            "gopls is not pinned".into(),
            Fix::Command(format!(
                "mkdir -p tools && cd tools && go mod init tools && go get -tool golang.org/x/tools/gopls@{GOPLS_PIN}"
            )),
        );
    };
    let pin_rel = rel_to(&pin_file, &git);
    if modfile {
        let without = |what: &str| {
            refused(
                format!("-modfile cannot run with {what}"),
                Fix::None("pin gopls in go.mod instead".into()),
            )
        };
        if root.join("vendor").is_dir() {
            return without("vendor/");
        }
        if p.gowork.is_some() {
            return without("go.work");
        }
    }
    // Only the `go` line counts: `GOTOOLCHAIN=local` ignores a `toolchain` line.
    let mut floors = vec![(pin_rel.clone(), go_directive(&pin_text))];
    if modfile {
        floors.push((project.0.to_string(), go_directive(&project.1)));
    }
    for (file, floor) in floors {
        if let Some(w) = floor.filter(|w| !version_ge(&local, w)) {
            return refused(
                format!("go {local} is older than {file}'s go {w}"),
                Fix::None("upgrade Go".into()),
            );
        }
    }
    let gopls_version = required_version(&pin_text, "golang.org/x/tools/gopls");
    let mut args: Vec<OsString> = vec!["tool".into()];
    if modfile {
        let mut flag = OsString::from("-modfile=");
        flag.push(&pin_file);
        args.push(flag);
    }
    args.push("gopls".into());
    Resolution {
        lang: Lang::Go,
        git_root: Some(git),
        project_root: Some(root.clone()),
        pin: Some(format!(
            "{pin_rel} tool golang.org/x/tools/gopls {}",
            gopls_version
                .as_deref()
                .unwrap_or("(version not in require)")
        )),
        server: Some(go_bin.clone()),
        version: Some(format!(
            "{} (go {local})",
            gopls_version.as_deref().unwrap_or("gopls")
        )),
        verdict: Verdict::Verified,
        spawn: Some(Spawn {
            program: go_bin,
            args,
            cwd: root,
            env_set: vec![("GOTOOLCHAIN".into(), "local".into())],
            env_remove: Vec::new(),
        }),
        barrier: Barrier::None,
        narrowed: false,
        tsserver_path: None,
    }
}

/// The nearest `tools/go.mod` with a gopls `tool` line, from `root` up to `git`.
fn tools_module(root: &Path, git: &Path) -> Option<(PathBuf, String)> {
    root.ancestors()
        .take_while(|d| d.starts_with(git))
        .map(|d| d.join("tools/go.mod"))
        .find_map(|p| {
            let text = fs::read_to_string(&p).ok()?;
            has_gopls_tool(&text).then_some((p, text))
        })
}

/// The version a go.mod requires for `module`, directly or in a block.
fn required_version(gomod: &str, module: &str) -> Option<String> {
    gomod.lines().find_map(|l| {
        let l = l.trim();
        let l = l.strip_prefix("require ").unwrap_or(l);
        let rest = l.strip_prefix(module)?.strip_prefix(' ')?;
        rest.split_whitespace().next().map(str::to_string)
    })
}

/// Builds gopls through the resolved spawn, so a session never waits on the
/// first build. A build that fails turns the resolution into a refusal.
pub(crate) fn warm_gopls(r: &mut Resolution) {
    let Some(spawn) = r.spawn.as_ref().filter(|_| r.lang == Lang::Go) else {
        return;
    };
    let mut cmd = Command::new(&spawn.program);
    cmd.args(&spawn.args).arg("version").current_dir(&spawn.cwd);
    for (k, v) in &spawn.env_set {
        cmd.env(k, v);
    }
    let failure = match cmd.output() {
        Ok(out) if out.status.success() => return,
        Ok(out) => String::from_utf8_lossy(&out.stderr)
            .lines()
            // `# <package>` heads the compiler's errors; the error follows.
            .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .unwrap_or("no output")
            .trim()
            .to_string(),
        Err(e) => e.to_string(),
    };
    let dir = spawn
        .args
        .iter()
        .find_map(|a| a.to_str()?.strip_prefix("-modfile="))
        .and_then(|m| Path::new(m).parent())
        .unwrap_or(&spawn.cwd)
        .to_path_buf();
    r.verdict = Verdict::Refused {
        reason: format!("gopls build failed: {failure}"),
        fix: Fix::Command(format!(
            "cd {} && GOTOOLCHAIN=local go build -o /dev/null golang.org/x/tools/gopls",
            dir.display()
        )),
    };
    r.spawn = None;
}

fn has_gopls_tool(gomod: &str) -> bool {
    let mut in_block = false;
    for line in gomod.lines() {
        let l = line.trim();
        if l == "tool (" {
            in_block = true;
        } else if in_block && l == ")" {
            in_block = false;
        } else if (in_block && l == "golang.org/x/tools/gopls")
            || l == "tool golang.org/x/tools/gopls"
        {
            return true;
        }
    }
    false
}

fn go_directive(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("go ").map(|v| v.trim().to_string()))
}

/// Dotted numeric versions: `1.27.1` ≥ `1.25.12`.
pub(crate) fn version_ge(a: &str, b: &str) -> bool {
    let nums = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|p| p.trim().parse().unwrap_or(0))
            .collect()
    };
    let (a, b) = (nums(a), nums(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    true
}

// ---------------------------------------------------------------- python

fn python(git: PathBuf, root: PathBuf) -> Resolution {
    let refused = |reason: String, fix: Fix| {
        Resolution::refused(
            Lang::Python,
            Some(git.clone()),
            Some(root.clone()),
            reason,
            fix,
        )
    };
    let pyproject = match fs::read_to_string(root.join("pyproject.toml")) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return refused(
                "no pyproject.toml".into(),
                Fix::None("pin pyright in a uv project's pyproject.toml".into()),
            )
        }
        Err(e) => {
            return refused(
                format!("cannot read pyproject.toml: {e}"),
                Fix::None("make pyproject.toml readable".into()),
            )
        }
    };
    let installed = venv_pyright(&root);
    let Some(pinned) = pyright_pin(&pyproject) else {
        // `uv add` needs a `[project]` table; without one the usual fix
        // cannot work, so say what the project is instead.
        let is_uv_project = pyproject
            .lines()
            .any(|l| l.split('#').next().unwrap_or_default().trim() == "[project]");
        if !is_uv_project {
            return refused(
                "not a uv project (no [project] table)".into(),
                Fix::None("not a uv project (no [project] table)".into()),
            );
        }
        let v = installed.unwrap_or_else(|| PYRIGHT_MEASURED.to_string());
        return refused(
            "no pyright==<version> in pyproject.toml".into(),
            Fix::Command(format!("uv add --dev 'pyright=={v}'")),
        );
    };
    let pin = format!("pyproject.toml pyright=={pinned}");
    if let Some(locked) = uv_lock_version(&root, "pyright") {
        if locked != pinned {
            let mut r = refused(
                format!("uv.lock has pyright {locked}, pyproject.toml pins {pinned}"),
                Fix::Command("uv lock".into()),
            );
            r.pin = Some(pin);
            return r;
        }
    }
    let server = root.join(".venv/bin/pyright-langserver");
    let (Some(installed), true) = (installed, server.exists()) else {
        let mut r = refused(
            "no pinned pyright in the venv".into(),
            Fix::Command("uv sync".into()),
        );
        r.pin = Some(pin);
        return r;
    };
    if installed != pinned {
        let mut r = refused(
            format!("stale venv: pyright {installed}, pinned {pinned}"),
            Fix::Command("uv sync".into()),
        );
        r.pin = Some(pin);
        r.version = Some(installed);
        return r;
    }
    let measured = installed == PYRIGHT_MEASURED;
    Resolution {
        lang: Lang::Python,
        git_root: Some(git),
        project_root: Some(root.clone()),
        pin: Some(pin),
        server: Some(server.clone()),
        version: Some(installed),
        verdict: Verdict::Verified,
        spawn: Some(Spawn {
            program: server,
            args: vec!["--stdio".into()],
            cwd: root,
            env_set: Vec::new(),
            env_remove: vec![
                "PYRIGHT_PYTHON_FORCE_VERSION".into(),
                "PYRIGHT_PYTHON_PYLANCE_VERSION".into(),
            ],
        }),
        barrier: if measured {
            Barrier::PyrightEnumeration
        } else {
            Barrier::None
        },
        narrowed: !measured,
        tsserver_path: None,
    }
}

fn venv_pyright(root: &Path) -> Option<String> {
    let lib = root.join(".venv/lib");
    for py in fs::read_dir(lib).ok()?.flatten() {
        for e in fs::read_dir(py.path().join("site-packages"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("pyright-") && name.ends_with(".dist-info") {
                let meta = fs::read_to_string(e.path().join("METADATA")).ok()?;
                return meta
                    .lines()
                    .find_map(|l| l.strip_prefix("Version:").map(|v| v.trim().to_string()));
            }
        }
    }
    None
}

/// `"pyright==1.1.411"` (or `pyright[nodejs]==…`) quoted in pyproject.toml.
pub(crate) fn pyright_pin(pyproject: &str) -> Option<String> {
    for line in pyproject.lines() {
        let code = line.split('#').next().unwrap_or("");
        for quoted in code.split(['"', '\'']).skip(1).step_by(2) {
            let q = quoted.trim();
            let rest = q.strip_prefix("pyright").map(|r| {
                r.trim_start_matches(|c| c != '=' && c != '>' && c != '<' && c != '~' && c != '!')
            });
            if let Some(v) = rest.and_then(|r| r.strip_prefix("==")) {
                if q.starts_with("pyright==") || q.starts_with("pyright[") {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

fn uv_lock_version(root: &Path, package: &str) -> Option<String> {
    let text = fs::read_to_string(root.join("uv.lock")).ok()?;
    let want = format!("name = \"{package}\"");
    let mut lines = text.lines();
    while let Some(l) = lines.next() {
        if l.trim() == want {
            let next = lines.next()?.trim();
            return next
                .strip_prefix("version = \"")
                .and_then(|v| v.strip_suffix('"'))
                .map(str::to_string);
        }
    }
    None
}

// ---------------------------------------------------------------- typescript

fn typescript(git: PathBuf, root: PathBuf, node: impl FnOnce() -> Node) -> Resolution {
    let refused = |reason: String, fix: Fix| {
        Resolution::refused(
            Lang::TypeScript,
            Some(git.clone()),
            Some(root.clone()),
            reason,
            fix,
        )
    };
    let lock = [
        "pnpm-lock.yaml",
        "package-lock.json",
        "yarn.lock",
        "bun.lock",
    ]
    .into_iter()
    .find(|f| root.join(f).exists());
    let install = match lock {
        Some("pnpm-lock.yaml") => "pnpm install --frozen-lockfile",
        Some("package-lock.json") => "npm ci",
        Some("yarn.lock") => "yarn install --immutable",
        Some(_) => "bun install --frozen-lockfile",
        None => {
            return refused(
                "no lockfile pins typescript".into(),
                Fix::None("no lockfile to install from".into()),
            )
        }
    };
    let lock = lock.expect("matched Some above");
    let lock_text = fs::read_to_string(root.join(lock)).unwrap_or_default();
    let Some(pinned) = lockfile_version(lock, &lock_text, "typescript") else {
        return refused(
            format!("{lock} has no typescript"),
            Fix::None("add typescript as a dev dependency".into()),
        );
    };
    let pin = format!("{lock} typescript {pinned}");
    let installed = fs::read_to_string(root.join("node_modules/typescript/package.json"))
        .ok()
        .and_then(|t| crate::json::parse(&t).ok())
        .and_then(|j| {
            j.get("version")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    let Some(installed) = installed else {
        let mut r = refused(
            "typescript is not installed".into(),
            Fix::Command(install.into()),
        );
        r.pin = Some(pin);
        return r;
    };
    if installed != pinned {
        let mut r = refused(
            format!("stale node_modules: typescript {installed}, pinned {pinned}"),
            Fix::Command(install.into()),
        );
        r.pin = Some(pin);
        r.version = Some(installed);
        return r;
    }
    let adapter_pin = lockfile_version(lock, &lock_text, ADAPTER);
    let Some(adapter_pinned) = adapter_pin else {
        let mut r = refused(
            format!("{ADAPTER} is not pinned"),
            Fix::Command(adapter_add_command(lock, &root)),
        );
        r.pin = Some(pin);
        return r;
    };
    let pin = format!("{pin}, {ADAPTER} {adapter_pinned}");
    let adapter_pkg =
        read_package_json(&root.join("node_modules").join(ADAPTER).join("package.json"));
    let adapter_version = adapter_pkg.as_ref().and_then(|j| {
        j.get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    });
    let adapter = root.join("node_modules/.bin").join(ADAPTER);
    let Some(adapter_version) = adapter_version.filter(|_| adapter.exists()) else {
        let mut r = refused(
            format!("{ADAPTER} is not installed"),
            Fix::Command(install.into()),
        );
        r.pin = Some(pin);
        return r;
    };
    if adapter_version != adapter_pinned {
        let mut r = refused(
            format!("stale node_modules: {ADAPTER} {adapter_version}, pinned {adapter_pinned}"),
            Fix::Command(install.into()),
        );
        r.pin = Some(pin);
        r.version = Some(adapter_version);
        return r;
    }
    // The `.bin` shim runs whatever `node` is on PATH.
    let node_min = adapter_pkg.as_ref().and_then(|j| {
        j.get("engines")
            .and_then(|e| e.get("node"))
            .and_then(|n| n.as_str())
            .map(str::to_string)
    });
    let node_version = match node() {
        Node::Version(v) => v,
        Node::NotOnPath => {
            let mut r = refused(
                "node is not on PATH".into(),
                Fix::None("install Node".into()),
            );
            r.pin = Some(pin);
            return r;
        }
        Node::Failed(path) => {
            let mut r = refused(
                format!("{} --version failed", path.display()),
                Fix::None(format!("run {} --version to see why", path.display())),
            );
            r.pin = Some(pin);
            return r;
        }
    };
    if let Some(min) = node_min.as_deref().and_then(engines_floor) {
        if !version_ge(&node_version, &min) {
            let mut r = refused(
                format!(
                    "node {node_version} is older than {ADAPTER} needs ({})",
                    node_min.as_deref().unwrap_or_default()
                ),
                Fix::None("upgrade Node".into()),
            );
            r.pin = Some(pin);
            return r;
        }
    }
    let lib = root.join("node_modules/typescript/lib");
    let measured = adapter_version == TS_ADAPTER_MEASURED;
    Resolution {
        lang: Lang::TypeScript,
        git_root: Some(git),
        project_root: Some(root.clone()),
        pin: Some(pin),
        server: Some(adapter.clone()),
        version: Some(format!(
            "{installed} (adapter {adapter_version}, node {node_version})"
        )),
        verdict: Verdict::Verified,
        spawn: Some(Spawn {
            program: adapter,
            args: vec!["--stdio".into()],
            cwd: root,
            ..Spawn::default()
        }),
        barrier: if measured {
            Barrier::TypeScript {
                version: installed,
                tsserver_js: lib.join("tsserver.js").display().to_string(),
            }
        } else {
            Barrier::None
        },
        narrowed: !measured,
        tsserver_path: Some(lib.display().to_string()),
    }
}

/// The `node` the adapter's `.bin` shim will run.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    NotOnPath,
    Failed(PathBuf),
    Version(String),
}

fn node_on_path() -> Node {
    let Some(path) = which("node") else {
        return Node::NotOnPath;
    };
    match first_line(&path, &["--version"]) {
        Some(v) if v.starts_with('v') => Node::Version(v.trim_start_matches('v').to_string()),
        _ => Node::Failed(path),
    }
}

/// The adapter fleet-lsp runs from each repository's own node_modules.
const ADAPTER: &str = "typescript-language-server";

/// The command that pins the measured adapter, for the package manager the
/// lockfile belongs to.
fn adapter_add_command(lock: &str, root: &Path) -> String {
    let spec = format!("{ADAPTER}@{TS_ADAPTER_MEASURED}");
    match lock {
        "pnpm-lock.yaml" if root.join("pnpm-workspace.yaml").exists() => {
            format!("pnpm add -D -E -w {spec}")
        }
        "pnpm-lock.yaml" => format!("pnpm add -D -E {spec}"),
        "yarn.lock" => format!("yarn add -D -E {spec}"),
        "bun.lock" => format!("bun add -d --exact {spec}"),
        _ => format!("npm install -D -E {spec}"),
    }
}

fn read_package_json(path: &Path) -> Option<crate::json::Json> {
    crate::json::parse(&fs::read_to_string(path).ok()?).ok()
}

/// The floor of an `engines` range: only a leading `>=X[.Y[.Z]]` is read;
/// any other form is not enforced (`doctor` still prints the Node version).
// holds-until: the adapter's `engines.node` stays a plain `>=X.Y.Z` (6.0.1:
// `>=22.22.2`); a range with `||` or `^` needs a real semver range parser.
pub(crate) fn engines_floor(range: &str) -> Option<String> {
    let v: String = range
        .trim()
        .strip_prefix(">=")?
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    (!v.is_empty()).then_some(v)
}

/// The version a lockfile resolves for the package named exactly `name`.
pub(crate) fn lockfile_version(kind: &str, text: &str, name: &str) -> Option<String> {
    let strip = |v: &str| {
        v.trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .split('(')
            .next()
            .unwrap_or("")
            .to_string()
    };
    match kind {
        "package-lock.json" => {
            let at = text.find(&format!("\"node_modules/{name}\": {{"))?;
            let rest = &text[at..];
            let v = rest.find("\"version\":").map(|i| &rest[i + 10..])?;
            Some(strip(v.split(',').next()?))
        }
        "pnpm-lock.yaml" => {
            // The root importer's resolved version first.
            let key = format!("{name}:");
            let mut in_importers = false;
            let mut in_root = false;
            let mut lines = text.lines().peekable();
            while let Some(l) = lines.next() {
                // pnpm separates entries with blank lines; they end nothing.
                if l.trim().is_empty() {
                    continue;
                }
                if !l.starts_with(' ') {
                    in_importers = l.trim_end() == "importers:";
                    continue;
                }
                if in_importers && l.starts_with("  ") && !l.starts_with("   ") {
                    in_root = l.trim() == ".:" || l.trim() == "'.':";
                    continue;
                }
                if in_importers && in_root && l.trim() == key {
                    for next in lines.by_ref().take(3) {
                        if let Some(v) = next.trim().strip_prefix("version:") {
                            return Some(strip(v));
                        }
                    }
                }
            }
            let prefix = format!("  {name}@");
            let mut versions: Vec<String> = text
                .lines()
                .filter_map(|l| l.strip_prefix(prefix.as_str()))
                .map(|v| strip(v.trim_end_matches([':', '{', '}', ' '])))
                .collect();
            versions.sort();
            versions.dedup();
            (versions.len() == 1).then(|| versions.remove(0))
        }
        "yarn.lock" => {
            let prefix = format!("{name}@");
            let mut versions = Vec::new();
            let mut lines = text.lines();
            while let Some(l) = lines.next() {
                let key = l.trim_start_matches('"');
                if !l.starts_with(' ') && key.starts_with(&prefix) {
                    for next in lines.by_ref().take(4) {
                        let n = next.trim();
                        if let Some(v) = n
                            .strip_prefix("version:")
                            .or_else(|| n.strip_prefix("version "))
                        {
                            versions.push(strip(v));
                            break;
                        }
                    }
                }
            }
            versions.sort();
            versions.dedup();
            (versions.len() == 1).then(|| versions.remove(0))
        }
        _ => {
            // bun.lock: "<name>": ["<name>@5.9.3", …]
            let needle = format!("\"{name}@");
            let at = text.find(&needle)?;
            let v = &text[at + needle.len()..];
            Some(strip(v.split('"').next()?))
        }
    }
}

// ---------------------------------------------------------------- helpers

/// The absolute path `name` resolves to on PATH.
pub(crate) fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file() && is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

fn first_line(program: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    let text = if out.stdout.is_empty() {
        out.stderr
    } else {
        out.stdout
    };
    String::from_utf8_lossy(&text)
        .lines()
        .next()
        .map(|l| l.trim().to_string())
}

/// The body of `[name]` in a TOML file, up to the next table header.
fn toml_section(text: &str, name: &str) -> Option<String> {
    let header = format!("[{name}]");
    let mut out = None::<String>;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if out.is_some() {
                break;
            }
            if t == header {
                out = Some(String::new());
            }
            continue;
        }
        if let Some(s) = &mut out {
            s.push_str(line);
            s.push('\n');
        }
    }
    out
}

fn toml_string(section: &str, key: &str) -> Option<String> {
    section.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| {
            v.split('#')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"')
                .to_string()
        })
    })
}

/// `key = ["a", "b"]`, possibly over several lines.
fn toml_string_array(section: &str, key: &str) -> Vec<String> {
    let Some(start) = section
        .lines()
        .position(|l| l.split_once('=').is_some_and(|(k, _)| k.trim() == key))
    else {
        return Vec::new();
    };
    let mut buf = String::new();
    for l in section.lines().skip(start) {
        buf.push_str(l.split('#').next().unwrap_or(""));
        if buf.contains(']') {
            break;
        }
    }
    let Some(inner) = buf
        .split_once('[')
        .and_then(|(_, r)| r.split_once(']'))
        .map(|(i, _)| i.to_string())
    else {
        return Vec::new();
    };
    inner
        .split(',')
        .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(PathBuf);
    impl Tree {
        fn new(name: &str) -> Tree {
            let dir =
                std::env::temp_dir().join(format!("fleet-lsp-test-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join(".git")).unwrap();
            Tree(dir)
        }
        fn file(&self, rel: &str, text: &str) -> &Tree {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
            self
        }
        fn p(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn root_of(lang: Lang, t: &Tree, start: &str) -> Result<PathBuf, Vec<String>> {
        match project_root(lang, &t.p(start), &t.0) {
            Found::One(r) => Ok(r),
            Found::Several(l) => Err(l.iter().map(|p| rel_to(p, &t.0)).collect()),
            Found::Nothing => Err(vec![]),
        }
    }

    #[test]
    fn go_module_in_a_subdirectory_is_its_own_root() {
        let t = Tree::new("gosub");
        t.file("services/api/go.mod", "module x\n\ngo 1.25\n")
            .file("services/api/main.go", "");
        assert_eq!(
            root_of(Lang::Go, &t, "services/api"),
            Ok(t.p("services/api"))
        );
    }

    #[test]
    fn two_venvs_refuse_and_starting_in_each_resolves() {
        let t = Tree::new("twovenv");
        t.file("a/pyproject.toml", "").file("b/pyproject.toml", "");
        assert_eq!(
            root_of(Lang::Python, &t, ""),
            Err(vec!["a".into(), "b".into()])
        );
        assert_eq!(root_of(Lang::Python, &t, "a"), Ok(t.p("a")));
        assert_eq!(root_of(Lang::Python, &t, "b"), Ok(t.p("b")));
    }

    #[test]
    fn uv_workspace_member_lifts_to_the_workspace_root() {
        let t = Tree::new("uvws");
        t.file(
            "pyproject.toml",
            "[project]\nname='x'\n[tool.uv.workspace]\nmembers = [\"packages/*\"]\nexclude = [\"packages/skip\"]\n",
        )
        .file("packages/agents/pyproject.toml", "")
        .file("packages/skip/pyproject.toml", "");
        assert_eq!(root_of(Lang::Python, &t, "packages/agents"), Ok(t.p("")));
        assert_eq!(
            root_of(Lang::Python, &t, "packages/skip"),
            Ok(t.p("packages/skip"))
        );
    }

    #[test]
    fn pnpm_member_lifts_to_the_workspace_root() {
        let t = Tree::new("pnpm");
        t.file(
            "pnpm-workspace.yaml",
            "packages:\n  - \"apps/*\"\n  - 'packages/*'\n",
        )
        .file("package.json", "{}")
        .file("apps/web/package.json", "{}");
        assert_eq!(root_of(Lang::TypeScript, &t, "apps/web"), Ok(t.p("")));
    }

    #[test]
    fn walk_down_skips_node_modules() {
        let t = Tree::new("skip");
        t.file("node_modules/x/pyproject.toml", "")
            .file("app/pyproject.toml", "");
        assert_eq!(root_of(Lang::Python, &t, ""), Ok(t.p("app")));
    }

    #[test]
    fn rust_channel_must_be_exact() {
        assert!(exact_channel("1.94.1"));
        assert!(exact_channel("nightly-2026-01-02"));
        assert!(!exact_channel("stable"));
        assert!(!exact_channel("1.88"));
        assert!(!exact_channel("nightly"));
    }

    #[test]
    fn rust_floating_channel_is_refused_with_a_fix() {
        let t = Tree::new("rustfloat");
        t.file("Cargo.toml", "[package]\n")
            .file("rust-toolchain.toml", "[toolchain]\nchannel = \"stable\"\n");
        let r = resolve(Lang::Rust, &t.p(""));
        match r.verdict {
            Verdict::Refused {
                reason,
                fix: Fix::Command(c),
            } => {
                assert!(reason.contains("floats"), "{reason}");
                assert!(c.contains("channel = "), "{c}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn pyright_pin_forms() {
        assert_eq!(
            pyright_pin("dev = [\"pyright==1.1.411\"]"),
            Some("1.1.411".into())
        );
        assert_eq!(
            pyright_pin("dev = ['pyright[nodejs]==1.1.400']"),
            Some("1.1.400".into())
        );
        assert_eq!(pyright_pin("# pyright==9 in a comment\n"), None);
        assert_eq!(pyright_pin("dev = [\"pyright>=1.1\"]"), None);
    }

    #[test]
    fn stale_venv_is_refused_and_matching_is_verified() {
        let t = Tree::new("venv");
        t.file(
            "pyproject.toml",
            "[dependency-groups]\ndev = [\"pyright==1.1.411\"]\n",
        )
        .file(
            ".venv/lib/python3.13/site-packages/pyright-1.1.414.dist-info/METADATA",
            "Name: pyright\nVersion: 1.1.414\n",
        )
        .file(".venv/bin/pyright-langserver", "");
        let r = resolve(Lang::Python, &t.p(""));
        assert!(
            matches!(&r.verdict, Verdict::Refused { reason, .. } if reason.contains("stale venv")),
            "{:?}",
            r.verdict
        );
        let _ =
            fs::remove_dir_all(t.p(".venv/lib/python3.13/site-packages/pyright-1.1.414.dist-info"));
        t.file(
            ".venv/lib/python3.13/site-packages/pyright-1.1.411.dist-info/METADATA",
            "Version: 1.1.411\n",
        );
        let r = resolve(Lang::Python, &t.p(""));
        assert_eq!(r.verdict, Verdict::Verified);
        assert_eq!(r.barrier, Barrier::PyrightEnumeration);
        let spawn = r.spawn.unwrap();
        assert!(spawn
            .env_remove
            .contains(&"PYRIGHT_PYTHON_FORCE_VERSION".to_string()));
        assert_eq!(spawn.args, vec![OsString::from("--stdio")]);
    }

    #[test]
    fn uv_lock_disagreeing_with_pyproject_is_refused() {
        let t = Tree::new("uvlock");
        t.file("pyproject.toml", "dev = [\"pyright==1.1.411\"]\n")
            .file(
                "uv.lock",
                "[[package]]\nname = \"pyright\"\nversion = \"1.1.400\"\n",
            );
        let r = resolve(Lang::Python, &t.p(""));
        assert!(
            matches!(&r.verdict, Verdict::Refused { fix: Fix::Command(c), .. } if c == "uv lock"),
            "{:?}",
            r.verdict
        );
    }

    #[test]
    fn stale_node_modules_is_refused() {
        let t = Tree::new("ts");
        t.file("package.json", "{}")
            .file(
                "package-lock.json",
                "{\"packages\":{\"node_modules/typescript\": {\n \"version\": \"5.9.3\",\n}}}",
            )
            .file(
                "node_modules/typescript/package.json",
                "{\"version\":\"5.8.0\"}",
            );
        let r = resolve(Lang::TypeScript, &t.p(""));
        assert!(
            matches!(&r.verdict, Verdict::Refused { reason, fix: Fix::Command(c) } if reason.contains("stale") && c == "npm ci"),
            "{:?}",
            r.verdict
        );
    }

    #[test]
    fn lockfile_versions_match_exact_names() {
        let pnpm = "lockfileVersion: '9.0'\nimporters:\n\n  .:\n    devDependencies:\n      typescript:\n        specifier: ^5.9.3\n        version: 5.9.3\n      typescript-language-server:\n        specifier: 6.0.1\n        version: 6.0.1\n\n  apps/web:\n    devDependencies:\n      typescript:\n        specifier: ^5.0.0\n        version: 5.0.4\npackages:\n\n  typescript-language-server@6.0.1:\n\n  typescript@5.9.3:\n";
        assert_eq!(
            lockfile_version("pnpm-lock.yaml", pnpm, "typescript"),
            Some("5.9.3".into())
        );
        assert_eq!(
            lockfile_version("pnpm-lock.yaml", pnpm, ADAPTER),
            Some("6.0.1".into())
        );
        let npm = "{\"packages\":{\"node_modules/typescript-language-server\": {\n \"version\": \"6.0.1\",\n},\"node_modules/typescript\": {\n \"version\": \"5.9.3\",\n}}}";
        assert_eq!(
            lockfile_version("package-lock.json", npm, "typescript"),
            Some("5.9.3".into())
        );
        assert_eq!(
            lockfile_version("package-lock.json", npm, ADAPTER),
            Some("6.0.1".into())
        );
        let yarn = "\"typescript-language-server@6.0.1\":\n  version \"6.0.1\"\n\n\"typescript@^5.9.2\":\n  version \"5.9.3\"\n  resolved \"x\"\n";
        assert_eq!(
            lockfile_version("yarn.lock", yarn, "typescript"),
            Some("5.9.3".into())
        );
        assert_eq!(
            lockfile_version("yarn.lock", yarn, ADAPTER),
            Some("6.0.1".into())
        );
        let bun = "{ \"packages\": { \"typescript-language-server\": [\"typescript-language-server@6.0.1\", \"\", {}], \"typescript\": [\"typescript@5.9.3\", \"\", {}] } }";
        assert_eq!(
            lockfile_version("bun.lock", bun, "typescript"),
            Some("5.9.3".into())
        );
        assert_eq!(
            lockfile_version("bun.lock", bun, ADAPTER),
            Some("6.0.1".into())
        );
        assert_eq!(lockfile_version("package-lock.json", "{}", ADAPTER), None);
    }

    #[test]
    fn engines_floor_reads_only_a_leading_minimum() {
        assert_eq!(engines_floor(">=22.22.2"), Some("22.22.2".into()));
        assert_eq!(engines_floor(" >= 18"), Some("18".into()));
        assert_eq!(engines_floor("^22 || ^24"), None);
        assert_eq!(engines_floor("22.x"), None);
    }

    /// A TypeScript project with typescript 5.9.3 and the adapter at
    /// `adapter_locked` in its package-lock, installed at `adapter_installed`.
    fn ts_project(
        name: &str,
        adapter_locked: Option<&str>,
        adapter_installed: Option<&str>,
        node_engines: &str,
    ) -> Tree {
        let t = Tree::new(name);
        let mut lock = String::from(
            "{\"packages\":{\"node_modules/typescript\": {\n \"version\": \"5.9.3\",\n}",
        );
        if let Some(v) = adapter_locked {
            lock.push_str(&format!(
                ",\"node_modules/typescript-language-server\": {{\n \"version\": \"{v}\",\n}}"
            ));
        }
        lock.push_str("}}");
        t.file("package.json", "{}")
            .file("package-lock.json", &lock)
            .file(
                "node_modules/typescript/package.json",
                "{\"version\":\"5.9.3\"}",
            );
        if let Some(v) = adapter_installed {
            t.file(
                "node_modules/typescript-language-server/package.json",
                &format!("{{\"version\":\"{v}\",\"engines\":{{\"node\":\"{node_engines}\"}}}}"),
            )
            .file(
                "node_modules/.bin/typescript-language-server",
                "#!/bin/sh\n",
            );
        }
        t
    }

    /// Resolves `t` as a TypeScript project with `node` as the PATH Node.
    fn ts_with(t: &Tree, node: Node) -> Resolution {
        typescript(t.p(""), t.p(""), move || node)
    }

    fn node(v: &str) -> Node {
        Node::Version(v.into())
    }

    fn refusal(r: &Resolution) -> (String, Fix) {
        match &r.verdict {
            Verdict::Refused { reason, fix } => (reason.clone(), fix.clone()),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn adapter_comes_from_the_repository_and_is_verified() {
        let t = ts_project("ts-ok", Some("6.0.1"), Some("6.0.1"), ">=1.0.0");
        let r = ts_with(&t, node("24.14.0"));
        assert_eq!(r.verdict, Verdict::Verified, "{:?}", r.verdict);
        let spawn = r.spawn.unwrap();
        assert_eq!(
            spawn.program,
            t.p("node_modules/.bin/typescript-language-server")
        );
        assert!(!r.narrowed);
        assert_eq!(r.version.unwrap(), "5.9.3 (adapter 6.0.1, node 24.14.0)");
    }

    #[test]
    fn unmeasured_adapter_is_verified_but_narrowed() {
        let t = ts_project("ts-602", Some("6.0.2"), Some("6.0.2"), ">=1.0.0");
        let r = ts_with(&t, node("24.14.0"));
        assert_eq!(r.verdict, Verdict::Verified);
        assert!(r.narrowed);
        assert_eq!(r.barrier, Barrier::None);
    }

    #[test]
    fn refusal_adapter_not_pinned() {
        let t = ts_project("ts-nopin", None, None, ">=1.0.0");
        let (reason, fix) = refusal(&ts_with(&t, node("24.14.0")));
        assert_eq!(reason, "typescript-language-server is not pinned");
        assert_eq!(
            fix,
            Fix::Command("npm install -D -E typescript-language-server@6.0.1".into())
        );
        // In a pnpm workspace the fix adds it at the workspace root.
        let w = Tree::new("ts-nopin-pnpm");
        w.file("package.json", "{}")
            .file("pnpm-workspace.yaml", "packages:\n  - 'packages/*'\n")
            .file("pnpm-lock.yaml", "importers:\n\n  .:\n    devDependencies:\n      typescript:\n        specifier: ^5.9.3\n        version: 5.9.3\n")
            .file("node_modules/typescript/package.json", "{\"version\":\"5.9.3\"}");
        let (_, fix) = refusal(&ts_with(&w, node("24.14.0")));
        assert_eq!(
            fix,
            Fix::Command("pnpm add -D -E -w typescript-language-server@6.0.1".into())
        );
    }

    #[test]
    fn refusal_adapter_stale() {
        let t = ts_project("ts-stale", Some("6.0.1"), Some("6.0.0"), ">=1.0.0");
        let (reason, fix) = refusal(&ts_with(&t, node("24.14.0")));
        assert_eq!(
            reason,
            "stale node_modules: typescript-language-server 6.0.0, pinned 6.0.1"
        );
        assert_eq!(fix, Fix::Command("npm ci".into()));
    }

    #[test]
    fn refusal_adapter_pinned_but_not_installed() {
        let t = ts_project("ts-noinst", Some("6.0.1"), None, ">=1.0.0");
        let (reason, fix) = refusal(&ts_with(&t, node("24.14.0")));
        assert_eq!(reason, "typescript-language-server is not installed");
        assert_eq!(fix, Fix::Command("npm ci".into()));
    }

    #[test]
    fn refusal_node_too_old() {
        let t = ts_project("ts-node", Some("6.0.1"), Some("6.0.1"), ">=22.22.2");
        let (reason, fix) = refusal(&ts_with(&t, node("22.11.0")));
        assert_eq!(
            reason,
            "node 22.11.0 is older than typescript-language-server needs (>=22.22.2)"
        );
        assert_eq!(fix, Fix::None("upgrade Node".into()));
        assert_eq!(ts_with(&t, node("22.22.2")).verdict, Verdict::Verified);
    }

    #[test]
    fn refusal_node_missing_or_failing() {
        let t = ts_project("ts-nonode", Some("6.0.1"), Some("6.0.1"), ">=22.22.2");
        let (reason, fix) = refusal(&ts_with(&t, Node::NotOnPath));
        assert_eq!(reason, "node is not on PATH");
        assert_eq!(fix, Fix::None("install Node".into()));
        let (reason, fix) = refusal(&ts_with(&t, Node::Failed("/x/node".into())));
        assert_eq!(reason, "/x/node --version failed");
        assert_eq!(fix, Fix::None("run /x/node --version to see why".into()));
    }

    #[test]
    fn refusal_python_pyproject_missing() {
        let t = Tree::new("py-nofile");
        t.file(".venv/pyvenv.cfg", "home = /usr/bin\n");
        let (reason, fix) = refusal(&python(t.p(""), t.p("")));
        assert_eq!(reason, "no pyproject.toml");
        assert_eq!(
            fix,
            Fix::None("pin pyright in a uv project's pyproject.toml".into())
        );
    }

    #[test]
    fn refusal_python_not_a_uv_project() {
        let t = Tree::new("py-nouv");
        t.file(
            "pyproject.toml",
            "[tool.pytest.ini_options]\naddopts = \"-q\"\n",
        );
        // A commented `[project]` header is still a uv project.
        let c = Tree::new("py-commented");
        c.file("pyproject.toml", "[project]  # the app\nname = \"x\"\n");
        let (reason, _) = refusal(&python(c.p(""), c.p("")));
        assert_eq!(reason, "no pyright==<version> in pyproject.toml");
        let (reason, fix) = refusal(&resolve(Lang::Python, &t.p("")));
        assert_eq!(reason, "not a uv project (no [project] table)");
        assert_eq!(
            fix,
            Fix::None("not a uv project (no [project] table)".into())
        );
        let r = resolve(Lang::Python, &t.p(""));
        assert_eq!(
            r.refusal_text().unwrap(),
            "fleet-lsp: python: not a uv project (no [project] table); fix: none — not a uv project (no [project] table)"
        );
    }

    const TOOLS_GOMOD: &str = "module tools\n\ngo 1.26.0\n\ntool golang.org/x/tools/gopls\n\nrequire (\n\tgithub.com/x/y v1.0.0 // indirect\n\tgolang.org/x/tools/gopls v0.23.0\n)\n";

    fn probe(version: &str) -> GoProbe {
        GoProbe {
            ignored: Ok(false),
            bin: Some("/x/go".into()),
            version: Some(version.into()),
            gowork: None,
        }
    }

    /// Resolves `start` in `t` as Go with `p` as the machine.
    fn go_with(t: &Tree, start: &str, p: GoProbe) -> Resolution {
        let root = match root_of(Lang::Go, t, start) {
            Ok(r) => r,
            Err(e) => panic!("no single Go root: {e:?}"),
        };
        go(t.p(""), root, move |_, _| p)
    }

    #[test]
    fn gopls_from_the_tools_module_with_modfile_and_local_toolchain() {
        let t = Tree::new("go-tools");
        t.file("go.mod", "module op\n\ngo 1.25.0\n\ntoolchain go1.99.0\n")
            .file("tools/go.mod", TOOLS_GOMOD);
        let r = go_with(&t, "", probe("1.27.1"));
        assert_eq!(r.verdict, Verdict::Verified, "{:?}", r.verdict);
        assert_eq!(
            r.pin.as_deref(),
            Some("tools/go.mod tool golang.org/x/tools/gopls v0.23.0")
        );
        assert_eq!(r.version.as_deref(), Some("v0.23.0 (go 1.27.1)"));
        let spawn = r.spawn.unwrap();
        assert_eq!(spawn.program, PathBuf::from("/x/go"));
        let modfile = format!("-modfile={}", t.p("tools/go.mod").display());
        assert_eq!(
            spawn.args,
            vec![OsString::from("tool"), modfile.into(), "gopls".into()]
        );
        assert_eq!(spawn.env_set, vec![("GOTOOLCHAIN".into(), "local".into())]);
        assert_eq!(spawn.cwd, t.p(""));
    }

    #[test]
    fn nested_module_shares_the_tools_module_at_the_git_root() {
        let t = Tree::new("go-nested");
        t.file("tools/go.mod", TOOLS_GOMOD)
            .file("wasm/auth/go.mod", "module auth\n\ngo 1.22.12\n");
        let r = go_with(&t, "wasm/auth", probe("1.27.1"));
        assert_eq!(r.verdict, Verdict::Verified, "{:?}", r.verdict);
        assert_eq!(r.project_root, Some(t.p("wasm/auth")));
        assert!(r.pin.unwrap().starts_with("tools/go.mod "));
        // From the git root, the tools module is not a project of its own.
        assert_eq!(root_of(Lang::Go, &t, ""), Ok(t.p("wasm/auth")));
    }

    #[test]
    fn gopls_in_the_module_go_mod_runs_without_modfile() {
        let t = Tree::new("go-inline");
        t.file(
            "go.mod",
            "module op\n\ngo 1.25.0\n\ntool golang.org/x/tools/gopls\n\nrequire golang.org/x/tools/gopls v0.23.0\n",
        );
        let r = go_with(&t, "", probe("1.27.1"));
        assert_eq!(r.verdict, Verdict::Verified);
        assert_eq!(
            r.pin.as_deref(),
            Some("go.mod tool golang.org/x/tools/gopls v0.23.0")
        );
        let spawn = r.spawn.unwrap();
        assert_eq!(spawn.args, vec![OsString::from("tool"), "gopls".into()]);
        assert_eq!(spawn.env_set, vec![("GOTOOLCHAIN".into(), "local".into())]);
    }

    #[test]
    fn refusal_gopls_not_pinned() {
        let t = Tree::new("go-nopin");
        t.file("go.mod", "module op\n\ngo 1.25.0\n");
        let (reason, fix) = refusal(&go_with(&t, "", probe("1.27.1")));
        assert_eq!(reason, "gopls is not pinned");
        assert_eq!(
            fix,
            Fix::Command("mkdir -p tools && cd tools && go mod init tools && go get -tool golang.org/x/tools/gopls@v0.23.0".into())
        );
    }

    #[test]
    fn refusal_local_go_older_than_either_go_line() {
        let t = Tree::new("go-old");
        t.file("go.mod", "module op\n\ngo 1.25.0\n")
            .file("tools/go.mod", TOOLS_GOMOD);
        let (reason, fix) = refusal(&go_with(&t, "", probe("1.25.3")));
        assert_eq!(reason, "go 1.25.3 is older than tools/go.mod's go 1.26.0");
        assert_eq!(fix, Fix::None("upgrade Go".into()));
        let u = Tree::new("go-old-project");
        u.file("go.mod", "module op\n\ngo 1.28.0\n")
            .file("tools/go.mod", TOOLS_GOMOD);
        let (reason, _) = refusal(&go_with(&u, "", probe("1.27.1")));
        assert_eq!(reason, "go 1.27.1 is older than go.mod's go 1.28.0");
    }

    #[test]
    fn refusal_modfile_with_vendor_or_go_work() {
        let t = Tree::new("go-vendor");
        t.file("go.mod", "module op\n\ngo 1.25.0\n")
            .file("vendor/modules.txt", "")
            .file("tools/go.mod", TOOLS_GOMOD);
        let (reason, fix) = refusal(&go_with(&t, "", probe("1.27.1")));
        assert_eq!(reason, "-modfile cannot run with vendor/");
        assert_eq!(fix, Fix::None("pin gopls in go.mod instead".into()));
        let w = Tree::new("go-work");
        w.file("go.mod", "module op\n\ngo 1.25.0\n")
            .file("tools/go.mod", TOOLS_GOMOD);
        let mut p = probe("1.27.1");
        p.gowork = Some("/elsewhere/go.work".into());
        let (reason, _) = refusal(&go_with(&w, "", p));
        assert_eq!(reason, "-modfile cannot run with go.work");
    }

    #[test]
    fn refusal_ignored_root_and_missing_go() {
        let t = Tree::new("go-ign");
        t.file("copy/go.mod", "module op\n\ngo 1.25.0\n")
            .file("tools/go.mod", TOOLS_GOMOD);
        let mut p = probe("1.27.1");
        p.ignored = Ok(true);
        let (reason, fix) = refusal(&go_with(&t, "copy", p));
        assert_eq!(reason, "copy is ignored by git");
        assert_eq!(fix, Fix::None("open the real repository".into()));
        let mut p = probe("1.27.1");
        p.bin = None;
        let (reason, fix) = refusal(&go_with(&t, "copy", p));
        assert_eq!(reason, "go is not installed");
        assert_eq!(fix, Fix::Command("brew install go".into()));
    }

    #[test]
    fn real_git_check_ignore_decides_the_ignored_root() {
        let t = Tree::new("go-ign-real");
        fs::remove_dir_all(t.p(".git")).unwrap();
        let init = Command::new("git")
            .args(["init", "-q"])
            .arg(t.p(""))
            .status()
            .unwrap();
        assert!(init.success());
        // A directory-only pattern, matched against a path without a slash.
        t.file(".gitignore", "copy/\n")
            .file("copy/go.mod", "module op\n\ngo 1.25.0\n")
            .file("real/go.mod", "module op\n\ngo 1.25.0\n");
        assert_eq!(go_probe(&t.p(""), &t.p("copy")).ignored, Ok(true));
        assert_eq!(go_probe(&t.p(""), &t.p("real")).ignored, Ok(false));
        assert_eq!(go_probe(&t.p(""), &t.p("")).ignored, Ok(false));
        let (reason, _) = refusal(&go(t.p(""), t.p("copy"), go_probe));
        assert_eq!(reason, "copy is ignored by git");
        // A `.git` git cannot read is a failure named, not "not ignored".
        let broken = Tree::new("go-ign-broken");
        broken.file("sub/go.mod", "module op\n");
        let got = go_probe(&broken.p(""), &broken.p("sub")).ignored;
        assert!(matches!(&got, Err(e) if !e.is_empty()), "{got:?}");
        let (reason, fix) = refusal(&go(broken.p(""), broken.p("sub"), go_probe));
        assert!(reason.starts_with("git check-ignore failed: "), "{reason}");
        assert_eq!(fix, Fix::None("run git check-ignore sub to see why".into()));
    }

    #[test]
    fn required_version_reads_direct_and_block_forms() {
        assert_eq!(
            required_version(TOOLS_GOMOD, "golang.org/x/tools/gopls"),
            Some("v0.23.0".into())
        );
        assert_eq!(
            required_version(
                "require golang.org/x/tools/gopls v0.1.0\n",
                "golang.org/x/tools/gopls"
            ),
            Some("v0.1.0".into())
        );
        assert_eq!(
            required_version(
                "require golang.org/x/tools/goplsx v1\n",
                "golang.org/x/tools/gopls"
            ),
            None
        );
    }

    /// An offline tools module whose gopls is a local stand-in, built by the
    /// real `go tool -modfile`.
    fn fake_gopls_tree(name: &str, main_go: &str) -> Tree {
        let t = Tree::new(name);
        // `-modfile` resolves a relative `replace` from the module root (the
        // cwd), not from tools/: the stand-in is named by absolute path.
        let tools = format!(
            "module tools\n\ngo 1.24\n\ntool golang.org/x/tools/gopls\n\nrequire golang.org/x/tools/gopls v0.0.0\n\nreplace golang.org/x/tools/gopls => {}\n",
            t.p("fakegopls").display()
        );
        t.file("go.mod", "module op\n\ngo 1.24\n")
            .file("tools/go.mod", &tools)
            .file(
                "fakegopls/go.mod",
                "module golang.org/x/tools/gopls\n\ngo 1.24\n",
            )
            .file("fakegopls/main.go", main_go);
        t
    }

    fn warmed(t: &Tree) -> Resolution {
        let mut r = go(t.p(""), t.p(""), go_probe);
        assert_eq!(r.verdict, Verdict::Verified, "{:?}", r.verdict);
        if let Some(s) = r.spawn.as_mut() {
            s.env_set.push(("GOPROXY".into(), "off".into()));
            s.env_set.push(("GOFLAGS".into(), "-mod=mod".into()));
        }
        warm_gopls(&mut r);
        r
    }

    #[test]
    fn real_go_builds_gopls_through_the_modfile() {
        let t = fake_gopls_tree(
            "go-real",
            "package main\n\nimport \"fmt\"\n\nfunc main() { fmt.Println(\"golang.org/x/tools/gopls v0.0.0-fake\") }\n",
        );
        let r = warmed(&t);
        assert_eq!(r.verdict, Verdict::Verified, "{:?}", r.verdict);
        assert!(r.spawn.is_some());
    }

    #[test]
    fn refusal_gopls_build_failed() {
        let t = fake_gopls_tree("go-broken", "package main\n\nfunc main() { undefined() }\n");
        let r = warmed(&t);
        let (reason, fix) = refusal(&r);
        assert!(
            reason.starts_with("gopls build failed: ") && reason.contains("undefined"),
            "{reason}"
        );
        assert_eq!(
            fix,
            Fix::Command(format!(
                "cd {} && GOTOOLCHAIN=local go build -o /dev/null golang.org/x/tools/gopls",
                t.p("tools").display()
            ))
        );
        assert!(r.spawn.is_none());
    }

    #[test]
    fn gopls_tool_directive_forms() {
        assert!(has_gopls_tool("module x\ntool golang.org/x/tools/gopls\n"));
        assert!(has_gopls_tool("tool (\n\tgolang.org/x/tools/gopls\n)\n"));
        assert!(!has_gopls_tool("require golang.org/x/tools/gopls v0.1\n"));
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(version_ge("1.27.1", "1.25.12"));
        assert!(version_ge("1.25.12", "1.25.12"));
        assert!(!version_ge("1.24.2", "1.25"));
    }

    #[test]
    fn globs() {
        assert!(glob_match("packages/*", "packages/agents"));
        assert!(!glob_match("packages/*", "packages/a/b"));
        assert!(glob_match("apps/**", "apps/a/b"));
        assert!(glob_match("./libs/*", "libs/x"));
    }

    #[test]
    fn outside_a_repository_is_refused() {
        let dir = std::env::temp_dir().join(format!("fleet-lsp-nogit-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // temp dirs are not inside a git repository on CI or here.
        if git_root(&dir).is_none() {
            let r = resolve(Lang::Rust, &dir);
            assert!(
                matches!(&r.verdict, Verdict::Refused { reason, .. } if reason.starts_with("not in a git repository"))
            );
        }
        let _ = fs::remove_dir_all(dir);
    }
}
