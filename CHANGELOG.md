# Changelog

## v0.4.0

- The plugin adds a five-line note at session start: navigate code with the
  LSP tool, check call sites with `findReferences` before a rename, fix the
  diagnostics that arrive after an edit, and do not fall back to Grep without
  saying so. It is printed only when `fleet-lsp` is on PATH.
- The plugin requires fleet-lsp 0.4.0 (the binary is unchanged otherwise:
  plugin and binary versions move together). Rolling back to 0.3.0: point
  the marketplace at `ref: v0.3.0`, update the plugin, put the 0.3.0 binary
  first on PATH.

## v0.3.0

Go's gopls is pinned per repository.

- gopls runs from the repository's `tool golang.org/x/tools/gopls`: the
  nearest `tools/go.mod` from the project root up to the git root, run as
  `go tool -modfile=<tools/go.mod> gopls`, or the module's own go.mod
  (`go tool gopls`). Both run with `GOTOOLCHAIN=local`, so gopls is built by
  the machine's Go, never a downloaded toolchain. The PATH `gopls` is no
  longer used.
- Refused, each with its fix: `gopls is not pinned` (`mkdir -p tools && cd
  tools && go mod init tools && go get -tool golang.org/x/tools/gopls@v0.23.0`);
  `go <v> is older than <file>'s go <w>` (the tools module's or the
  project's `go` line; a `toolchain` line is not read); `-modfile cannot
  run with vendor/` or `go.work`; `<dir> is ignored by git`; `gopls build
  failed: <error>`.
- `doctor` builds gopls (`checking that gopls builds …` on stderr; only
  the first run builds), so the first session does not wait on the build.
- **`--json`: the `compatible` verdict is gone**; verdicts are `verified`
  and `refused`. `doctor` exits 0 only when every language is verified.
- The Go server's `startupTimeout` is 210 s: twice a cold gopls build
  measured on a loaded machine (95 s) plus its 7.7 s start.
- A `tools/` module (`module tools`) is not discovered as a Go project.
- The plugin requires fleet-lsp 0.3.0. Rolling back to 0.2.0: point the
  marketplace at `ref: v0.2.0`, update the plugin, put the 0.2.0 binary
  first on PATH and `go install golang.org/x/tools/gopls@v0.23.0`.

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
