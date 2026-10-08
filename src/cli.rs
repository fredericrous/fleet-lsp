//! Argument parsing for the few fixed argv shapes fleet-lsp accepts.
//!
//! Hand-rolled rather than `clap` (README, "Why no dependencies"): `serve` is
//! invoked by a plugin manifest with a fixed argv, `doctor` takes one flag.
//! What a library would add — bundling, abbreviations — has nothing to act on.

use std::fmt;

/// The four language families fleet-lsp fronts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lang {
    Rust,
    Go,
    Python,
    TypeScript,
}

impl Lang {
    pub(crate) const ALL: [Lang; 4] = [Lang::Rust, Lang::Go, Lang::Python, Lang::TypeScript];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Go => "go",
            Lang::Python => "python",
            Lang::TypeScript => "typescript",
        }
    }

    fn parse(s: &str) -> Option<Lang> {
        Lang::ALL.into_iter().find(|l| l.name() == s)
    }
}

impl fmt::Display for Lang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A `major.minor.patch` version, compared numerically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version(pub(crate) u64, pub(crate) u64, pub(crate) u64);

impl Version {
    pub(crate) fn parse(s: &str) -> Option<Version> {
        let mut it = s.split('.');
        let v = Version(
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
        );
        it.next().is_none().then_some(v)
    }

    pub(crate) fn own() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo.toml version is x.y.z")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    /// `serve <lang> --min-version <x.y.z>`. `min_version` is `Err` with the
    /// raw text when it does not parse, so the caller can refuse through the
    /// stub rather than crash-loop the plugin.
    Serve {
        lang: Lang,
        min_version: Option<Result<Version, String>>,
    },
    Doctor {
        json: bool,
        /// The directory to check, else the current one.
        path: Option<std::path::PathBuf>,
    },
    Help,
    Version,
}

/// A usage error: the message to print, and the process exits 2.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Usage(pub(crate) String);

pub(crate) const HELP: &str = "\
fleet-lsp — pinned, ready language servers for Claude Code's LSP tool

Usage:
  fleet-lsp doctor [--json] [PATH]
  fleet-lsp serve <rust|go|python|typescript> --min-version <x.y.z>
  fleet-lsp --help | --version

doctor  Show, for the repository around PATH (default: the current
        directory), which server each language resolves to, and whether
        it matches the pin.
serve   Speak LSP on stdin/stdout in front of the pinned server. Claude
        Code runs it from the fleet-lsp plugin; a person never does.
        Started outside any git repository, it serves every repository
        whose files it is asked about, one server each.

Exit codes:
  doctor  0 every language verified
          1 a language refused, or PATH is in no git repository
          2 usage error
  serve   0 exit after shutdown, 1 otherwise, 2 usage error or a terminal

Docs: https://github.com/fredericrous/fleet-lsp
";

/// Parses argv without the program name.
pub(crate) fn parse(args: &[String]) -> Result<Command, Usage> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        [] => Err(Usage("missing command".into())),
        ["-h" | "--help"] | ["help"] => Ok(Command::Help),
        ["-V" | "--version"] => Ok(Command::Version),
        ["doctor", rest @ ..] => parse_doctor(rest),
        ["serve", rest @ ..] => parse_serve(rest),
        [other, ..] => Err(Usage(format!("unknown command `{other}`"))),
    }
}

fn parse_doctor(rest: &[&str]) -> Result<Command, Usage> {
    let mut json = false;
    let mut operands = Vec::new();
    let mut options_done = false;
    for arg in rest {
        match *arg {
            "--" if !options_done => options_done = true,
            "--json" if !options_done => json = true,
            a if !options_done && a.starts_with('-') => {
                return Err(Usage(format!("doctor: unexpected {a}")))
            }
            a => operands.push(a),
        }
    }
    match operands.as_slice() {
        [] => Ok(Command::Doctor { json, path: None }),
        [path] => Ok(Command::Doctor {
            json,
            path: Some(std::path::PathBuf::from(path)),
        }),
        [_, extra, ..] => Err(Usage(format!("doctor: unexpected `{extra}`"))),
    }
}

fn parse_serve(rest: &[&str]) -> Result<Command, Usage> {
    // `--min-version` is read first, wherever it sits, so a plugin newer than
    // this binary gets the upgrade refusal even if it also passes an argument
    // this version does not know yet.
    let mut min_version = None;
    let mut positional = Vec::new();
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        if *arg == "--min-version" {
            let v = it
                .next()
                .ok_or_else(|| Usage("serve: --min-version needs a value".into()))?;
            min_version = Some(Version::parse(v).ok_or_else(|| v.to_string()));
        } else if let Some(v) = arg.strip_prefix("--min-version=") {
            min_version = Some(Version::parse(v).ok_or_else(|| v.to_string()));
        } else {
            positional.push(*arg);
        }
    }
    if let Some(Ok(min)) = min_version {
        if min > Version::own() {
            // Accept whatever else is there: the stub will refuse with the
            // upgrade command, which is the answer the person needs.
            let lang = positional
                .first()
                .and_then(|s| Lang::parse(s))
                .unwrap_or(Lang::Rust);
            return Ok(Command::Serve { lang, min_version });
        }
    }
    match positional.as_slice() {
        [lang] => match Lang::parse(lang) {
            Some(lang) => Ok(Command::Serve { lang, min_version }),
            None => Err(Usage(format!(
                "serve: unknown language `{lang}` (rust, go, python, typescript)"
            ))),
        },
        [] => Err(Usage("serve: missing language".into())),
        [_, extra, ..] => Err(Usage(format!("serve: unexpected `{extra}`"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Result<Command, Usage> {
        parse(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn doctor_and_json() {
        let doctor = |json, path: Option<&str>| {
            Ok(Command::Doctor {
                json,
                path: path.map(std::path::PathBuf::from),
            })
        };
        assert_eq!(p(&["doctor"]), doctor(false, None));
        assert_eq!(p(&["doctor", "--json"]), doctor(true, None));
        assert!(p(&["doctor", "--jsn"]).is_err());
        assert_eq!(p(&["doctor", "relais"]), doctor(false, Some("relais")));
        assert_eq!(
            p(&["doctor", "relais", "--json"]),
            doctor(true, Some("relais"))
        );
        assert_eq!(p(&["doctor", "--", "-odd"]), doctor(false, Some("-odd")));
        assert!(p(&["doctor", "a", "b"]).is_err());
    }

    #[test]
    fn serve_with_min_version_anywhere() {
        let want = Ok(Command::Serve {
            lang: Lang::Go,
            min_version: Some(Ok(Version(0, 1, 0))),
        });
        assert_eq!(p(&["serve", "go", "--min-version", "0.1.0"]), want);
        assert_eq!(p(&["serve", "--min-version=0.1.0", "go"]), want);
    }

    #[test]
    fn newer_min_version_wins_over_unknown_arguments() {
        let r = p(&["serve", "rust", "--future-flag", "--min-version", "99.0.0"]);
        assert_eq!(
            r,
            Ok(Command::Serve {
                lang: Lang::Rust,
                min_version: Some(Ok(Version(99, 0, 0)))
            })
        );
    }

    #[test]
    fn malformed_min_version_is_carried_not_fatal() {
        let r = p(&["serve", "python", "--min-version", "one"]);
        assert_eq!(
            r,
            Ok(Command::Serve {
                lang: Lang::Python,
                min_version: Some(Err("one".into()))
            })
        );
    }

    #[test]
    fn usage_errors() {
        assert!(p(&[]).is_err());
        assert!(p(&["serve"]).is_err());
        assert!(p(&["serve", "cobol"]).is_err());
        assert!(p(&["serve", "rust", "extra"]).is_err());
        assert!(p(&["frobnicate"]).is_err());
    }

    #[test]
    fn version_ordering_is_numeric() {
        assert!(Version(0, 10, 0) > Version(0, 9, 9));
        assert_eq!(Version::parse("1.2"), None);
        assert_eq!(Version::parse("1.2.3.4"), None);
    }
}
