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
    /// gopls from PATH, built with a Go new enough for the module: the one
    /// accepted non-pin (deviation from build.toolchain-source).
    Compatible,
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
        Lang::Go => go(git, root),
        Lang::Python => python(git, root),
        Lang::TypeScript => typescript(git, root),
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

fn go(git: PathBuf, root: PathBuf) -> Resolution {
    let refused = |reason: String, fix: Fix| {
        Resolution::refused(Lang::Go, Some(git.clone()), Some(root.clone()), reason, fix)
    };
    let Some(go_bin) = which("go") else {
        return refused(
            "go is not installed".into(),
            Fix::Command("brew install go".into()),
        );
    };
    let gomod = fs::read_to_string(root.join("go.mod")).unwrap_or_default();
    let gowork = fs::read_to_string(root.join("go.work")).unwrap_or_default();
    if has_gopls_tool(&gomod) {
        let version = first_line(&go_bin, &["tool", "gopls", "version"])
            .map(|v| v.replace("golang.org/x/tools/gopls ", ""));
        return Resolution {
            lang: Lang::Go,
            git_root: Some(git),
            project_root: Some(root.clone()),
            pin: Some("go.mod tool golang.org/x/tools/gopls".into()),
            server: Some(go_bin.clone()),
            version,
            verdict: Verdict::Verified,
            spawn: Some(Spawn {
                program: go_bin,
                args: vec!["tool".into(), "gopls".into()],
                cwd: root,
                ..Spawn::default()
            }),
            barrier: Barrier::None,
            narrowed: false,
            tsserver_path: None,
        };
    }
    let need = go_directive(&gomod).or_else(|| go_directive(&gowork));
    // holds-until: the repository pins gopls with `tool golang.org/x/tools/gopls`
    // in go.mod. Until then the PATH gopls is accepted only as `compatible`
    // (built with a Go at least the module's), a recorded deviation from
    // build.toolchain-source.
    let Some(gopls) = which("gopls") else {
        return refused(
            "no gopls on PATH and no `tool golang.org/x/tools/gopls` in go.mod".into(),
            Fix::Command("go install golang.org/x/tools/gopls@latest".into()),
        );
    };
    let built = first_line(&go_bin, &["version", &gopls.display().to_string()]).and_then(|l| {
        l.rsplit(' ')
            .next()
            .map(|v| v.trim_start_matches("go").to_string())
    });
    let version =
        first_line(&gopls, &["version"]).map(|v| v.replace("golang.org/x/tools/gopls ", ""));
    let pin = need
        .as_ref()
        .map(|n| format!("go.mod go {n} (gopls built with go ≥ it)"));
    match (&built, &need) {
        (Some(b), Some(n)) if version_ge(b, n) => Resolution {
            lang: Lang::Go,
            git_root: Some(git),
            project_root: Some(root.clone()),
            pin,
            server: Some(gopls.clone()),
            version: version.map(|v| format!("{v} (go{b})")),
            verdict: Verdict::Compatible,
            spawn: Some(Spawn {
                program: gopls,
                args: Vec::new(),
                cwd: root,
                ..Spawn::default()
            }),
            barrier: Barrier::None,
            narrowed: false,
            tsserver_path: None,
        },
        (Some(b), Some(n)) => {
            let mut r = refused(
                format!("gopls was built with go{b}, older than the module's go {n}"),
                Fix::Command("go install golang.org/x/tools/gopls@latest".into()),
            );
            r.pin = pin;
            r.server = Some(gopls);
            r
        }
        _ => refused(
            "could not read the module's go version or gopls's build version".into(),
            Fix::None("go.mod needs a `go` line".into()),
        ),
    }
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
    let pyproject = fs::read_to_string(root.join("pyproject.toml")).unwrap_or_default();
    let installed = venv_pyright(&root);
    let Some(pinned) = pyright_pin(&pyproject) else {
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

fn typescript(git: PathBuf, root: PathBuf) -> Resolution {
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
    let Some(pinned) = lockfile_typescript(lock, &lock_text) else {
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
    // holds-until: the repository pins typescript-language-server. Until then
    // the adapter comes from PATH (installed by the Brewfile); the TypeScript
    // that answers is still the repository's, checked by the gate against the
    // adapter's own selection report.
    let Some(adapter) = which("typescript-language-server") else {
        let mut r = refused(
            "typescript-language-server is not installed".into(),
            Fix::Command("brew install typescript-language-server".into()),
        );
        r.pin = Some(pin);
        return r;
    };
    let adapter_version = first_line(&adapter, &["--version"]).unwrap_or_default();
    let lib = root.join("node_modules/typescript/lib");
    let measured = adapter_version == TS_ADAPTER_MEASURED;
    Resolution {
        lang: Lang::TypeScript,
        git_root: Some(git),
        project_root: Some(root.clone()),
        pin: Some(pin),
        server: Some(adapter.clone()),
        version: Some(format!("{installed} (adapter {adapter_version})")),
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

pub(crate) fn lockfile_typescript(kind: &str, text: &str) -> Option<String> {
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
            let at = text.find("\"node_modules/typescript\": {")?;
            let rest = &text[at..];
            let v = rest.find("\"version\":").map(|i| &rest[i + 10..])?;
            Some(strip(v.split(',').next()?))
        }
        "pnpm-lock.yaml" => {
            // The root importer's resolved version first.
            let mut in_importers = false;
            let mut in_root = false;
            let mut lines = text.lines().peekable();
            while let Some(l) = lines.next() {
                if !l.starts_with(' ') {
                    in_importers = l.trim_end() == "importers:";
                    continue;
                }
                if in_importers && l.starts_with("  ") && !l.starts_with("   ") {
                    in_root = l.trim() == ".:" || l.trim() == "'.':";
                    continue;
                }
                if in_importers && in_root && l.trim() == "typescript:" {
                    for next in lines.by_ref().take(3) {
                        if let Some(v) = next.trim().strip_prefix("version:") {
                            return Some(strip(v));
                        }
                    }
                }
            }
            let mut versions: Vec<String> = text
                .lines()
                .filter_map(|l| l.strip_prefix("  typescript@"))
                .map(|v| strip(v.trim_end_matches([':', '{', '}', ' '])))
                .collect();
            versions.sort();
            versions.dedup();
            (versions.len() == 1).then(|| versions.remove(0))
        }
        "yarn.lock" => {
            let mut versions = Vec::new();
            let mut lines = text.lines();
            while let Some(l) = lines.next() {
                let key = l.trim_start_matches('"');
                if !l.starts_with(' ') && key.starts_with("typescript@") {
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
            // bun.lock: "typescript": ["typescript@5.9.3", …]
            let at = text.find("\"typescript@")?;
            let v = &text[at + 12..];
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
    fn lockfile_typescript_versions() {
        let pnpm = "lockfileVersion: '9.0'\nimporters:\n\n  .:\n    devDependencies:\n      typescript:\n        specifier: ^5.9.3\n        version: 5.9.3\n\n  apps/web:\n    devDependencies:\n      typescript:\n        specifier: ^5.0.0\n        version: 5.0.4\npackages:\n\n  typescript@5.9.3:\n";
        assert_eq!(
            lockfile_typescript("pnpm-lock.yaml", pnpm),
            Some("5.9.3".into())
        );
        let yarn = "\"typescript@^5.9.2\":\n  version \"5.9.3\"\n  resolved \"x\"\n";
        assert_eq!(lockfile_typescript("yarn.lock", yarn), Some("5.9.3".into()));
        let bun = "{ \"packages\": { \"typescript\": [\"typescript@5.9.3\", \"\", {}] } }";
        assert_eq!(lockfile_typescript("bun.lock", bun), Some("5.9.3".into()));
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
