//! `fleet-lsp doctor [--json]`: what each language in the repository around
//! the current directory resolves to, and whether it matches the pin.

use crate::cli::Lang;
use crate::json::Json;
use crate::log::{log_dir, newest, tilde};
use crate::resolve::{self, Fix, Resolution, Verdict};
use std::path::Path;
use std::process::Command;

/// `requestTimeout` in plugin manifests needs this Claude Code.
const CLAUDE_MIN: &str = "2.1.288";
const WIDTH: usize = 80;

pub(crate) fn run(json: bool) -> (String, u8) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let Some(git) = resolve::git_root(&cwd) else {
        let msg = format!("not in a git repository: {}", tilde(&cwd));
        return if json {
            let j = Json::obj().set("error", msg.as_str());
            (format!("{j}\n"), 1)
        } else {
            (format!("{msg}\n"), 1)
        };
    };
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
    (text, code)
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
}
