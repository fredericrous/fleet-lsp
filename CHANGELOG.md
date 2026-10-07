# Changelog

## v0.2.0

TypeScript's adapter is pinned per repository.

- The adapter is `typescript-language-server` from the repository's own
  `node_modules/.bin`, verified against its lockfile entry (an exact
  devDependency at the workspace root). The PATH copy is no longer used.
  Not pinned → `fix: npm install -D -E typescript-language-server@6.0.1`
  (pnpm workspace: `pnpm add -D -E -w …`); installed ≠ locked → the
  lockfile's install command.
- Node: the adapter's `.bin` shim runs PATH `node`; a Node older than the
  adapter's `engines.node` floor is refused, and so are a missing `node`
  (`node is not on PATH`) and one whose `--version` fails. `doctor` prints
  the Node version.
- `--json`: for TypeScript, `pin` now also names the adapter
  (`…, typescript-language-server 6.0.1`) and `version` the adapter and
  Node (`5.9.3 (adapter 6.0.1, node 24.14.0)`). Both are display strings;
  the field set is unchanged.
- Lockfile lookups match package names exactly; pnpm's root importer is
  read past the blank lines pnpm writes (it fell back before).
- Python: a pyproject.toml without a `[project]` table is refused as `not a
  uv project` instead of suggesting a `uv add` that cannot work; a
  missing or unreadable pyproject.toml is refused with its own reason.
- The plugin requires fleet-lsp 0.2.0. Rolling back to 0.1.0 means the
  plugin too: point the `fleet-lsp` marketplace at `ref: v0.1.0` and
  update the plugin, then put the 0.1.0 binary first on PATH.

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
