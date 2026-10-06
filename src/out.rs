//! Human output to stdout that never panics on a closed pipe.
//!
//! Rust ignores SIGPIPE, so `println!` into `fleet-lsp doctor | head -1`
//! panics once `head` exits. Every human-facing write goes through here and
//! treats `BrokenPipe` as "the reader is done": a silent exit 0.
//! (`serve` never uses this: its stdout is framed LSP, see `relay`.)

use std::io::{self, Write};

pub(crate) fn stdout(text: &str) {
    let mut lock = io::stdout().lock();
    if let Err(e) = lock.write_all(text.as_bytes()).and_then(|()| lock.flush()) {
        if e.kind() == io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("fleet-lsp: writing to stdout: {e}");
        std::process::exit(1);
    }
}
