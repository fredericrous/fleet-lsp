//! fleet-lsp: pinned, ready language servers for Claude Code's LSP tool.
//!
//! The main component is the dirtiest one (architecture.main-is-a-plugin):
//! it reads argv, picks a command, and maps the outcome to an exit code.

mod cli;
mod out;

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
        Ok(Command::Doctor { .. }) | Ok(Command::Serve { .. }) => {
            eprintln!("fleet-lsp: not implemented yet");
            ExitCode::from(1)
        }
        Err(Usage(msg)) => {
            eprintln!("fleet-lsp: {msg}\nTry: fleet-lsp --help");
            ExitCode::from(2)
        }
    }
}
