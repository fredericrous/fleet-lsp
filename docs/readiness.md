# Readiness and pins — Phase 0 findings (2026-10-06)

Measured with a scratch LSP client (`scripts/lsp-probe.py`, outside the crate; rerun it to measure a new server version)
on this machine (macOS, x86_64). Raw transcripts and summaries:
`~/.local/state/fleet-lsp-phase0/*.jsonl` / `*.summary.json`.
"Ready" = the time until a known query returns its known answer
(`textDocument/references`, a cross-file caller expected in the result).

## Per server

| server / version | repo, query | false answers before ready? | barrier | ready (correct answer) | exit on stdin EOF |
|---|---|---|---|---|---|
| rust-analyzer 1.94.1 | relais, `reconcile` → `main.rs` | **yes**: `[]` at 0.6 s, `null`, `[]`, `content modified` | **signal**: `experimental/serverStatus` `quiescent: true` (30.9 s); next query correct (33.4 s) | 33.4 s | 3.9 s |
| rust-analyzer 1.91.0 | lldap, `setup_default_schema` → `ldap/src/handler.rs` | **yes**: `[]` for ~60 s, then `content modified` | **signal**, conservative: correct at 125 s, `quiescent: true` at 185 s (warm run); cold: correct at 167 s | 167 s cold | 7.4 s |
| gopls v0.23.0 (go1.27.1) | authelia-oidc-operator, `NewAssembler` → `oidcclient_controller.go` | **no**: the first request waits for the workspace load | **self-barrier**: request at 0.4 s answered correctly at 7.7 s; with `workspace/configuration` reply delayed 20 s, answered correctly at 24.4 s; `didOpen` 30 s late, correct in 0.5 s | 7.7 s | 5.3 s |
| pyright 1.1.411 (pyright-python wheel, bundled build — no download even with an empty cache) | trade-agents, `DuckDBFeatureLoader` → `backtest_runner/worker.py` | **yes**: a request pending when the configuration is applied is answered `[]` at that instant (7.1 s; 46.4 s with config delayed 20 s); the next one is correct | **signal (version-keyed)**: `window/logMessage` from the workspace enumerator's `_finish()` — `Found <n> source file(s)` or `No source files found.` (fixed strings in `pyright-internal.js` 1.1.411); every request sent after it was correct | 7.8 s (46.8 s config delayed) | 0.12 s |
| typescript-language-server 6.0.1 + TypeScript 5.9.3 | duro-app, `useCopyFeedback` → `ApiKeysSection` | **yes**: `[]` for 44 s | **signal (adapter-version-keyed)**: `$/progress` token titled `Initializing JS/TS language features…` — begins at the first `didOpen`, its `end` (44.5 s) precedes the first correct answer (45.6 s); late `didOpen` (30 s): begins on open, correct after its end | 45.6 s cold, 8.4 s warm | 0.05 s |

## rust-analyzer health

| workspace | status sequence | answers |
|---|---|---|
| `Cargo.toml` syntax error | `error, quiescent: false` → `error, quiescent: true` "Failed to load workspaces." | `[]` throughout |
| dependency not fetched (offline, empty `CARGO_HOME`) | `warning` "Failed to read Cargo metadata with dependencies … no matching package named `serde`" → `warning, quiescent: true` | local answers correct (`[1]`) |

Missing dependencies are a **warning**, not an error: local answers stay
right, answers that cross into the missing crates are incomplete.

## TypeScript selection report

`window/logMessage` right after `initialize`:
- pinned path accepted: `Using Typescript version (user-setting) 5.9.3 from path "<root>/node_modules/typescript/lib/tsserver.js"`
- invalid `tsserver.path`: `[lspserver] Typescript specified through user setting ignored due to invalid path "<path>"`, then `Using Typescript version (workspace) …` — the `(user-setting)` source marker and the path tell the two apart.

## Consequences for the plan

- Every server exits on stdin EOF; rust-analyzer can take 7.4 s, so the 5 s
  teardown deadline kills it on big workspaces — raise to 10 s.
- Ceiling: slowest cold ready is rust-analyzer on lldap, 185 s to
  quiescence → ceiling ≥ 370 s, `requestTimeout` ≈ 400 s.
- pyright's barrier is a log line at Information level: fleet-lsp must keep
  the client's `python.analysis.logLevel` reply at ≥ Information (it sits in
  the `workspace/configuration` reply it already relays), or the barrier
  never arrives and the ceiling answers.
- The pyright and TypeScript barriers are tied to the verified version;
  an unlisted version gets the narrowed promise until it is measured.
- Not covered: a second TypeScript project (another `tsconfig`) opened later
  in the same session — the progress token was observed for the first
  project load only.
