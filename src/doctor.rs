//! `fleet-lsp doctor [--json] [PATH]`: what each language in the repository
//! around PATH (default: the current directory) resolves to, and whether it
//! matches the pin.

use crate::cli::Lang;
use crate::json::Json;
use crate::log::{log_dir, newest, tilde};
use crate::resolve::{self, Fix, Resolution, Verdict};
use std::path::Path;
use std::process::Command;

/// `requestTimeout` in plugin manifests needs this Claude Code.
const CLAUDE_MIN: &str = "2.1.288";
const WIDTH: usize = 80;

/// What doctor prints: the report on stdout, talk on stderr, and the exit code.
pub(crate) struct Report {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) code: u8,
}

pub(crate) fn run(json: bool, path: Option<&Path>) -> Report {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let start = path.map_or_else(|| cwd.clone(), |p| cwd.join(p));
    let Some(git) = resolve::git_root(&start) else {
        let msg = format!("not in a git repository: {}", tilde(&start));
        // Outside every repository with no PATH is where a workspace-mode
        // session starts: say which repository to name.
        let hint = if path.is_none() {
            "\nworkspace mode: doctor checks one repository; run `fleet-lsp doctor <repo>`"
        } else {
            ""
        };
        return if json {
            let j = Json::obj().set("error", msg.as_str());
            Report {
                stdout: format!("{j}\n"),
                stderr: String::new(),
                code: 1,
            }
        } else {
            Report {
                stdout: String::new(),
                stderr: format!("{msg}{hint}\n"),
                code: 1,
            }
        };
    };
    let cwd = start;
    let found: Vec<Resolution> = Lang::ALL
        .into_iter()
        .filter(|l| resolve::present(*l, &cwd))
        .map(|l| resolve::resolve(l, &cwd))
        .map(|mut r| {
            if r.lang == Lang::Go && r.verdict == Verdict::Verified {
                // stderr: progress is not the report.
                eprintln!("fleet-lsp: checking that gopls builds (the first run builds it: about a minute)");
                resolve::warm_gopls(&mut r);
            }
            r
        })
        .collect();
    let code = u8::from(
        found
            .iter()
            .any(|r| matches!(r.verdict, Verdict::Refused { .. })),
    );
    let claude = claude_version();
    let text = if json {
        render_json(&found, &git, claude.as_deref())
    } else {
        render_text(&found, &git, claude.as_deref())
    };
    Report {
        stdout: text,
        stderr: String::new(),
        code,
    }
}

fn rel(path: &Path, git: &Path) -> String {
    match path.strip_prefix(git) {
        Ok(r) if r.as_os_str().is_empty() => ".".into(),
        Ok(r) => r.display().to_string(),
        Err(_) => tilde(path),
    }
}

fn verdict_word(v: &Verdict) -> &'static str {
    match v {
        Verdict::Verified => "verified",
        Verdict::Refused { .. } => "refused",
    }
}

fn render_text(found: &[Resolution], git: &Path, claude: Option<&str>) -> String {
    let mut out = format!("{}\n", tilde(git));
    if found.is_empty() {
        out.push_str("no rust, go, python or typescript project found here\n");
    }
    for r in found {
        let head = format!("{:<11}{:<11}", r.lang.name(), verdict_word(&r.verdict));
        let version = match &r.verdict {
            Verdict::Refused { .. } => String::new(),
            _ => r.version.clone().unwrap_or_default(),
        };
        out.push_str(head.trim_end());
        if !version.is_empty() {
            out.push_str(&format!(" {version}"));
        }
        out.push('\n');
        if let Some(p) = &r.project_root {
            field(&mut out, "project", &rel(p, git));
        }
        if let Some(pin) = &r.pin {
            field(&mut out, "pin", pin);
        }
        if let Some(s) = &r.server {
            field(&mut out, "server", &rel(s, git));
        }
        if r.narrowed {
            field(
                &mut out,
                "note",
                "readiness barrier not measured for this version: an empty answer right after start is not evidence",
            );
        }
        if let Verdict::Refused { reason, fix } = &r.verdict {
            field(&mut out, "reason", reason);
            match fix {
                Fix::Command(c) => out.push_str(&format!("  fix:\n    {c}\n")),
                Fix::Lines(head, lines) => {
                    out.push_str(&format!("  fix: {head}\n"));
                    for l in lines {
                        out.push_str(&format!("    {l}\n"));
                    }
                }
                Fix::None(why) => field(&mut out, "fix", &format!("none — {why}")),
            }
        }
    }
    let dir = log_dir();
    out.push_str(&format!("log: {}/\n", tilde(&dir)));
    if let Some(n) = newest(&dir) {
        field(&mut out, "newest", &tilde(&n));
    }
    out.push_str(&format!(
        "claude: {} (needs {CLAUDE_MIN} or newer)\n",
        claude.unwrap_or("not found")
    ));
    if let Some(v) = claude {
        if !crate::resolve::version_ge(v, CLAUDE_MIN) {
            field(
                &mut out,
                "warning",
                "this Claude Code ignores requestTimeout; upgrade it",
            );
        }
    }
    out
}

/// `  label: text`, wrapped at `WIDTH` with continuation lines indented.
fn field(out: &mut String, label: &str, text: &str) {
    let lead = format!("  {label}: ");
    let indent = " ".repeat(lead.len());
    let mut line = lead.clone();
    for word in text.split(' ') {
        let fresh = line == lead || line == indent;
        if !fresh && line.chars().count() + 1 + word.chars().count() > WIDTH {
            out.push_str(line.trim_end());
            out.push('\n');
            line = indent.clone();
        }
        if !(line == lead || line == indent) {
            line.push(' ');
        }
        line.push_str(word);
    }
    out.push_str(line.trim_end());
    out.push('\n');
}

fn render_json(found: &[Resolution], git: &Path, claude: Option<&str>) -> String {
    let opt_path = |p: &Option<std::path::PathBuf>| p.as_ref().map(|p| p.display().to_string());
    let langs: Vec<Json> = found
        .iter()
        .map(|r| {
            let (reason, fix) = match &r.verdict {
                Verdict::Refused { reason, fix } => (
                    Some(reason.clone()),
                    Some(match fix {
                        Fix::Command(c) => c.clone(),
                        Fix::Lines(h, l) => format!("{h} {}", l.join(", ")),
                        Fix::None(why) => format!("none — {why}"),
                    }),
                ),
                _ => (None, None),
            };
            Json::obj()
                .set("lang", r.lang.name())
                .set("git_root", git.display().to_string())
                .set_opt("project_root", opt_path(&r.project_root))
                .set_opt("pin", r.pin.clone())
                .set_opt("server", opt_path(&r.server))
                .set_opt("version", r.version.clone())
                .set("verdict", verdict_word(&r.verdict))
                .set_opt("reason", reason)
                .set_opt("fix", fix)
                .set("narrowed", r.narrowed)
        })
        .collect();
    let j = Json::obj()
        .set("languages", langs)
        .set("log_dir", log_dir().display().to_string())
        .set_opt("claude_version", claude.map(str::to_string))
        .set("claude_min", CLAUDE_MIN);
    format!("{j}\n")
}

fn claude_version() -> Option<String> {
    let claude = crate::resolve::which("claude")?;
    let out = Command::new(claude).arg("--version").output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_wrap_at_80_columns() {
        let mut out = String::new();
        field(&mut out, "reason", &"word ".repeat(40));
        assert!(out.lines().all(|l| l.chars().count() <= WIDTH), "{out}");
        assert!(out.lines().nth(1).unwrap().starts_with("          word"));
    }

    fn effect_tsgo(verdict: Verdict, narrowed: bool, version: &str) -> Resolution {
        let root = std::path::PathBuf::from("/r/cv");
        Resolution {
            lang: Lang::TypeScript,
            git_root: Some(root.clone()),
            project_root: Some(root.clone()),
            pin: Some("pnpm-lock.yaml @effect/tsgo 0.64.1".into()),
            server: matches!(verdict, Verdict::Verified)
                .then(|| root.join("node_modules/.bin/effect-tsgo")),
            version: Some(version.into()),
            verdict,
            spawn: None,
            barrier: crate::gate::Barrier::None,
            narrowed,
            tsserver_path: None,
            pull_diagnostics: true,
        }
    }

    /// The language block of `render_text`: what follows the repository
    /// line, up to the log lines, which depend on the machine.
    fn block(found: &[Resolution]) -> String {
        let text = render_text(found, Path::new("/r/cv"), None);
        assert!(text.lines().all(|l| l.chars().count() <= WIDTH), "{text}");
        text.lines()
            .skip(1)
            .take_while(|l| !l.starts_with("log: "))
            .map(|l| format!("{l}\n"))
            .collect()
    }

    /// FALSIFY: print the adapter's version layout for effect-tsgo.
    #[test]
    fn effect_tsgo_renders_its_compiler_pin_and_server() {
        let v = "effect-tsgo 0.64.1 (typescript 7.0.2, node 24.14.0)";
        assert_eq!(
            block(&[effect_tsgo(Verdict::Verified, false, v)]),
            "typescript verified effect-tsgo 0.64.1 (typescript 7.0.2, node 24.14.0)\n\
             \x20 project: .\n\
             \x20 pin: pnpm-lock.yaml @effect/tsgo 0.64.1\n\
             \x20 server: node_modules/.bin/effect-tsgo\n"
        );
        let beta = "effect-tsgo 0.64.1 (typescript 7.1.0-beta, node 24.14.0)";
        let narrowed = block(&[effect_tsgo(Verdict::Verified, true, beta)]);
        assert!(
            narrowed.contains("\n  note: readiness barrier not measured"),
            "{narrowed}"
        );
    }

    #[test]
    fn effect_tsgo_refusals_name_it_and_their_fix() {
        let refused = |reason: &str, fix: Fix| {
            block(&[effect_tsgo(
                Verdict::Refused {
                    reason: reason.into(),
                    fix,
                },
                false,
                "",
            )])
        };
        let install = Fix::Command("pnpm install --frozen-lockfile".into());
        assert_eq!(
            refused("@effect/tsgo is not installed", install.clone()),
            "typescript refused\n\
             \x20 project: .\n\
             \x20 pin: pnpm-lock.yaml @effect/tsgo 0.64.1\n\
             \x20 reason: @effect/tsgo is not installed\n\
             \x20 fix:\n\
             \x20   pnpm install --frozen-lockfile\n"
        );
        let stale = refused(
            "stale node_modules: @effect/tsgo 0.63.0, pinned 0.64.1",
            install,
        );
        assert!(
            stale.contains("  reason: stale node_modules: @effect/tsgo 0.63.0, pinned 0.64.1\n"),
            "{stale}"
        );
        let node = refused(
            "node is not on PATH; @effect/tsgo runs on it",
            Fix::None("install Node".into()),
        );
        assert!(
            node.ends_with(
                "  reason: node is not on PATH; @effect/tsgo runs on it\n  fix: none — install Node\n"
            ),
            "{node}"
        );
    }
}
