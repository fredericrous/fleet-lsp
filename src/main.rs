//! fleet-lsp: pinned, ready language servers for Claude Code's LSP tool.
//!
//! The main component is the dirtiest one (architecture.main-is-a-plugin):
//! it reads argv, picks a command, and maps the outcome to an exit code.

mod cli;
mod core;
mod doctor;
mod frame;
mod gate;
mod hook;
mod json;
mod log;
mod out;
mod queue;
mod relay;
mod resolve;
mod route;
mod scan;
mod spans;
mod workspace;

use cli::{Command, Usage};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args) {
        Ok(Command::Help) => {
            out::stdout(cli::HELP);
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            out::stdout(&format!("fleet-lsp {}\n", cli::Version::own()));
            ExitCode::SUCCESS
        }
        Ok(Command::Doctor { json, path }) => {
            let report = doctor::run(json, path.as_deref());
            out::stdout(&report.stdout);
            if !report.stderr.is_empty() {
                eprint!("{}", report.stderr);
            }
            ExitCode::from(report.code)
        }
        Ok(Command::Serve { lang, min_version }) => ExitCode::from(relay::serve(lang, min_version)),
        Ok(Command::Hook(event)) => ExitCode::from(run_hook(event)),
        Err(Usage(msg)) => {
            eprintln!("fleet-lsp: {msg}\nTry: fleet-lsp --help");
            ExitCode::from(2)
        }
    }
}

/// `fleet-lsp hook <event>`: reads the hook's JSON on stdin, prints the
/// context for the agent. 0 on every input problem (logged), 2 for a
/// terminal on stdin: a person who typed it would wait on nothing.
fn run_hook(event: hook::Event) -> u8 {
    use std::io::{IsTerminal, Read};
    if std::io::stdin().is_terminal() {
        eprintln!(
            "fleet-lsp: hook reads a Claude Code hook's JSON on stdin\nTry: fleet-lsp --help"
        );
        return 2;
    }
    // No absolute state directory: the fallback would be the working
    // directory, the person's project. A hook writes nothing there.
    let Some(state) = log::state_dir() else {
        return 0;
    };
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        hook::log_line(&state, &format!("hook: cannot read stdin: {e}"));
        return 0;
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let env = hook::Env {
        state: &state,
        home: home.as_deref(),
    };
    let outcome = hook::run(event, &input, &env);
    hook::deliver(&outcome, &mut std::io::stdout().lock(), &state);
    0
}
