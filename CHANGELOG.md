# Changelog

## v0.1.0

First release.

- `fleet-lsp serve <rust|go|python|typescript>`: relays Claude Code's LSP
  traffic to the repository's pinned server — rust-analyzer from the
  `rust-toolchain.toml` toolchain, gopls via `go tool` or a gopls built with
  a new enough Go, pyright from the project's `.venv` matching its
  `pyright==` pin, typescript-language-server with the lockfile's
  TypeScript — and refuses with the cause and fix when it cannot verify one.
- Readiness barriers measured per server (docs/readiness.md): queries wait
  until the server has loaded, never on a timer; rust-analyzer's health is
  honoured in every state.
- `fleet-lsp doctor [--json]`: what each language resolves to, and why.
- The Claude Code plugin, read from the `release` branch.
