//! Readiness barriers, one per server and verified version (docs/readiness.md).
//!
//! Pure: the relay feeds it what the server said, it answers what changed.
//! No timer ever opens a gate; the ceiling lives in `core` and answers a held
//! request with an error, it does not release it.

/// Which barrier a session uses, chosen by `resolve` from the verified server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Barrier {
    /// Nothing held: gopls (self-barrier, it blocks requests until loaded),
    /// or a server version whose barrier is not measured (narrowed promise).
    None,
    /// rust-analyzer `experimental/serverStatus`, all `health` states.
    RustAnalyzer,
    /// pyright 1.1.411: the workspace enumerator's completion log line.
    PyrightEnumeration,
    /// typescript-language-server 6.0.1: the selection report must name the
    /// verified TypeScript, then the `Initializing JS/TS language features…`
    /// progress must end.
    TypeScript {
        version: String,
        tsserver_js: String,
    },
}

pub(crate) const TS_PROGRESS_TITLE: &str = "Initializing JS/TS language features";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum State {
    Open,
    Closed,
    /// Every request is answered with this message until the state changes.
    Failed(String),
}

/// What the server reported, as far as a barrier cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Signal {
    Status {
        health: String,
        quiescent: bool,
        message: Option<String>,
    },
    ProgressBegin {
        token: String,
        title: String,
    },
    ProgressEnd {
        token: String,
    },
    Log(String),
}

/// A change the relay must act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    /// The gate state changed to this.
    Now(State),
    /// Show this warning to the client once (rust-analyzer `health: warning`).
    Warn(String),
}

#[derive(Debug)]
pub(crate) struct Gate {
    barrier: Barrier,
    state: State,
    ts_report_ok: bool,
    ts_token: Option<String>,
    ts_loaded: bool,
    warned: Vec<String>,
}

impl Gate {
    pub(crate) fn new(barrier: Barrier) -> Gate {
        let state = match barrier {
            Barrier::None => State::Open,
            Barrier::RustAnalyzer | Barrier::PyrightEnumeration | Barrier::TypeScript { .. } => {
                State::Closed
            }
        };
        Gate {
            barrier,
            state,
            ts_report_ok: false,
            ts_token: None,
            ts_loaded: false,
            warned: Vec::new(),
        }
    }

    pub(crate) fn state(&self) -> &State {
        &self.state
    }

    pub(crate) fn on(&mut self, signal: &Signal) -> Vec<Effect> {
        let mut effects = Vec::new();
        let next = match (&self.barrier, signal) {
            (
                Barrier::RustAnalyzer,
                Signal::Status {
                    health,
                    quiescent,
                    message,
                },
            ) => self.rust_status(health, *quiescent, message.as_deref(), &mut effects),
            (Barrier::PyrightEnumeration, Signal::Log(text)) if pyright_enumerated(text) => {
                Some(State::Open)
            }
            (Barrier::TypeScript { .. }, _) => self.typescript(signal),
            _ => None,
        };
        if let Some(next) = next {
            if next != self.state {
                self.state = next.clone();
                effects.insert(0, Effect::Now(next));
            }
        }
        effects
    }

    fn rust_status(
        &mut self,
        health: &str,
        quiescent: bool,
        message: Option<&str>,
        effects: &mut Vec<Effect>,
    ) -> Option<State> {
        if health == "error" {
            // Answer at once only when the server has settled on it; while it
            // is still working, an error may clear.
            return Some(if quiescent {
                State::Failed(format!(
                    "rust-analyzer: workspace did not load: {}{}",
                    message.unwrap_or("no detail given").trim(),
                    rust_fix(message.unwrap_or(""))
                ))
            } else {
                State::Closed
            });
        }
        if !quiescent {
            return Some(State::Closed);
        }
        if health == "warning" {
            if let Some(m) = message {
                let m = m.trim().to_string();
                if !self.warned.contains(&m) {
                    self.warned.push(m.clone());
                    effects.push(Effect::Warn(format!("rust-analyzer: {m}{}", rust_fix(&m))));
                }
            }
        }
        Some(State::Open)
    }

    fn typescript(&mut self, signal: &Signal) -> Option<State> {
        let Barrier::TypeScript {
            version,
            tsserver_js,
        } = &self.barrier
        else {
            return None;
        };
        match signal {
            Signal::Log(text) => {
                let report = parse_ts_report(text)?;
                if report.source == "user-setting"
                    && report.version == *version
                    && report.path == *tsserver_js
                {
                    self.ts_report_ok = true;
                } else {
                    return Some(State::Failed(format!(
                        "typescript-language-server selected TypeScript {} ({}) from {}, \
                         not the repository's {} from {}",
                        report.version, report.source, report.path, version, tsserver_js
                    )));
                }
            }
            Signal::ProgressBegin { token, title } if title.starts_with(TS_PROGRESS_TITLE) => {
                self.ts_token = Some(token.clone());
            }
            Signal::ProgressEnd { token } if self.ts_token.as_deref() == Some(token) => {
                self.ts_loaded = true;
            }
            _ => return None,
        }
        if matches!(self.state, State::Failed(_)) {
            return None;
        }
        (self.ts_report_ok && self.ts_loaded).then_some(State::Open)
    }
}

/// pyright 1.1.411 `_finish()`: `Found <n> source file(s)` or
/// `No source files found.` (docs/readiness.md).
fn pyright_enumerated(text: &str) -> bool {
    let text = text.trim();
    if text == "No source files found." {
        return true;
    }
    let Some(rest) = text.strip_prefix("Found ") else {
        return false;
    };
    let Some((n, tail)) = rest.split_once(' ') else {
        return false;
    };
    !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_digit())
        && matches!(tail, "source file" | "source files")
}

fn rust_fix(message: &str) -> &'static str {
    if message.contains("no matching package") || message.contains("failed to download") {
        "; fix: cargo fetch"
    } else if message.contains("Failed to load workspaces") || message.contains("cargo metadata") {
        "; fix: run `cargo metadata` in the project root and fix what it reports"
    } else {
        ""
    }
}

#[derive(Debug, PartialEq, Eq)]
struct TsReport {
    source: String,
    version: String,
    path: String,
}

/// `Using Typescript version (user-setting) 5.9.3 from path "/…/tsserver.js"`
fn parse_ts_report(text: &str) -> Option<TsReport> {
    let rest = text.trim().strip_prefix("Using Typescript version (")?;
    let (source, rest) = rest.split_once(") ")?;
    let (version, rest) = rest.split_once(" from path \"")?;
    let path = rest.strip_suffix('"')?;
    Some(TsReport {
        source: source.to_string(),
        version: version.to_string(),
        path: path.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(health: &str, quiescent: bool, message: Option<&str>) -> Signal {
        Signal::Status {
            health: health.into(),
            quiescent,
            message: message.map(str::to_string),
        }
    }

    #[test]
    fn no_barrier_is_open_from_the_start() {
        assert_eq!(Gate::new(Barrier::None).state(), &State::Open);
    }

    #[test]
    fn rust_every_health_row() {
        let mut g = Gate::new(Barrier::RustAnalyzer);
        assert_eq!(g.state(), &State::Closed);
        assert!(g.on(&status("ok", false, None)).is_empty(), "still closed");
        assert_eq!(
            g.on(&status("ok", true, None)),
            vec![Effect::Now(State::Open)]
        );
        // Re-closes when it goes non-quiescent again.
        assert_eq!(
            g.on(&status("ok", false, None)),
            vec![Effect::Now(State::Closed)]
        );
        // Warning, quiescent: opens and warns once per message.
        let w = g.on(&status(
            "warning",
            true,
            Some("no matching package named `serde`"),
        ));
        assert_eq!(w[0], Effect::Now(State::Open));
        assert!(matches!(&w[1], Effect::Warn(m) if m.contains("cargo fetch")));
        assert!(g
            .on(&status(
                "warning",
                true,
                Some("no matching package named `serde`")
            ))
            .is_empty());
        // Error while working: closed, not failed.
        assert_eq!(
            g.on(&status("error", false, Some("x"))),
            vec![Effect::Now(State::Closed)]
        );
        // Error, quiescent: failed with the message.
        let f = g.on(&status("error", true, Some("Failed to load workspaces.")));
        assert!(
            matches!(&f[0], Effect::Now(State::Failed(m)) if m.contains("workspace did not load: Failed to load workspaces."))
        );
        // A later ok re-opens.
        assert_eq!(
            g.on(&status("ok", true, None)),
            vec![Effect::Now(State::Open)]
        );
    }

    #[test]
    fn pyright_opens_on_either_enumeration_line_only() {
        for line in [
            "Found 589 source files",
            "Found 1 source file",
            "No source files found.",
        ] {
            let mut g = Gate::new(Barrier::PyrightEnumeration);
            assert_eq!(
                g.on(&Signal::Log(line.into())),
                vec![Effect::Now(State::Open)],
                "{line}"
            );
        }
        let mut g = Gate::new(Barrier::PyrightEnumeration);
        for line in [
            "Loading configuration file at /x",
            "Found many source files",
            "Searching for source files",
        ] {
            assert!(g.on(&Signal::Log(line.into())).is_empty(), "{line}");
        }
    }

    fn ts() -> Gate {
        Gate::new(Barrier::TypeScript {
            version: "5.9.3".into(),
            tsserver_js: "/r/node_modules/typescript/lib/tsserver.js".into(),
        })
    }

    #[test]
    fn typescript_needs_matching_report_and_progress_end() {
        let mut g = ts();
        let ok = r#"Using Typescript version (user-setting) 5.9.3 from path "/r/node_modules/typescript/lib/tsserver.js""#;
        assert!(g.on(&Signal::Log(ok.into())).is_empty());
        assert!(g
            .on(&Signal::ProgressBegin {
                token: "t".into(),
                title: "Initializing JS/TS language features…".into()
            })
            .is_empty());
        assert!(g
            .on(&Signal::ProgressEnd {
                token: "other".into()
            })
            .is_empty());
        assert_eq!(
            g.on(&Signal::ProgressEnd { token: "t".into() }),
            vec![Effect::Now(State::Open)]
        );
    }

    #[test]
    fn typescript_fallback_is_a_refusal() {
        let mut g = ts();
        let fell_back = r#"Using Typescript version (workspace) 5.9.3 from path "/r/node_modules/typescript/lib/tsserver.js""#;
        let e = g.on(&Signal::Log(fell_back.into()));
        assert!(matches!(&e[0], Effect::Now(State::Failed(m)) if m.contains("(workspace)")));
        // Progress ending later does not reopen a refused session.
        g.on(&Signal::ProgressBegin {
            token: "t".into(),
            title: TS_PROGRESS_TITLE.into(),
        });
        assert!(g.on(&Signal::ProgressEnd { token: "t".into() }).is_empty());
        assert!(matches!(g.state(), State::Failed(_)));
    }

    #[test]
    fn typescript_without_report_stays_closed() {
        let mut g = ts();
        g.on(&Signal::ProgressBegin {
            token: "t".into(),
            title: TS_PROGRESS_TITLE.into(),
        });
        assert!(g.on(&Signal::ProgressEnd { token: "t".into() }).is_empty());
        assert_eq!(g.state(), &State::Closed);
    }
}
