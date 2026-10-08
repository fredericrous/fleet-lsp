# fleet-lsp

Pinned, ready language servers for Claude Code's `LSP` tool.

Claude Code starts whatever language server is first on `PATH`, and asks it
questions the moment it starts. Measured on real repositories
([docs/readiness.md](docs/readiness.md)), three of the four common servers
answer **wrong** in that window — an empty list where there are callers —
and the binary that answers is often not the version the repository pins.

fleet-lsp sits between Claude Code and the server and makes two promises:

- **Pinned.** The server that answers is verified against the repository's
  own pin (`rust-toolchain.toml`, the `pyright==` dev dependency and
  `uv.lock`, the `typescript` and `typescript-language-server` lockfile
  entries, gopls's `tool` directive in `tools/go.mod` or `go.mod`). When it cannot be verified, every query gets an error naming
  the cause and the command that fixes it — never an answer from the wrong
  version.
- **Ready.** A query is answered only after the server has finished loading,
  by a barrier measured per server and version — never by a timer.

| language | server | readiness barrier |
|---|---|---|
| Rust | rust-analyzer from the pinned rustup toolchain | `experimental/serverStatus` quiescent; `health: error` becomes an error answer |
| Go | gopls from the repository's `tool` directive — the nearest `tools/go.mod` (run with `-modfile`) or the module's `go.mod` — built by the machine's Go under `GOTOOLCHAIN=local` | gopls blocks until its workspace is loaded |
| Python | pyright from the repository's `.venv` | pyright's workspace-enumeration message (1.1.411) |
| TypeScript | typescript-language-server from the repository's own `node_modules` (an exact devDependency), with the repository's TypeScript | the end of its "Initializing JS/TS language features" progress (6.0.1) |

A server version whose barrier has not been measured gets a narrowed
promise: answers flow, but an empty answer during the first start-up seconds
is not evidence. `scripts/lsp-probe.py` measures a new version.

Requires Claude Code 2.1.288 or newer (`requestTimeout` in plugin
manifests).

## Install

```sh
brew install fredericrous/tap/fleet-lsp
claude plugin marketplace add fredericrous/fleet-lsp#release
claude plugin install fleet-lsp@fleet-lsp
```

The marketplace is read from the `release` branch, which only the release
workflow moves, after the matching binary is installable from the tap: a
merge to `main` never reaches a session. A plugin newer than the installed
binary refuses every query with `brew upgrade fleet-lsp`.

Then turn off the official LSP plugins it replaces, so each extension has one
server: `gopls-lsp`, `rust-analyzer-lsp`, `typescript-lsp`, `pyright-lsp`.

Check a repository: `cd <repo> && fleet-lsp doctor`.

## What a session sees

- **A query before the server has loaded** waits, up to 370 s (twice the
  slowest cold start measured), then gets `server not ready after 370s`.
- **A server that cannot be verified** answers every query with an error
  naming the cause and the fix, e.g.
  `fleet-lsp: python: stale venv: pyright 1.1.414, pinned 1.1.411; fix: uv sync`.
- **rust-analyzer that could not load the workspace** answers
  `rust-analyzer: workspace did not load: …` with a fix.
- **At session start**, a five-line note
  ([hooks/code-intelligence.sh](plugins/fleet-lsp/hooks/code-intelligence.sh))
  tells the agent to navigate with the LSP tool and keep Grep for text.

## Limits

- Claude Code starts one set of servers per session, rooted where the
  session started. Start the session in the repository (or the project);
  outside one, every query answers `not in a git repository`.
- rust-analyzer reports unfetched dependencies as a *warning*: answers about
  your own code stay right, answers that reach into the missing crates are
  incomplete. fleet-lsp logs it and sends it as `window/showMessage`, but
  Claude Code does not show that message to the agent — run `cargo fetch`.
- A pyright or typescript-language-server version whose readiness signal
  has not been measured gets no barrier (`doctor` says so): an empty answer
  right after start is not evidence. Measure it with `scripts/lsp-probe.py`.
- gopls is built by the machine's Go (`GOTOOLCHAIN=local`), which must be
  at least the `go` line of the tools module and of the project's go.mod;
  it is not the toolchain the module pins (a stated deviation from
  build.toolchain-source). The first start builds it — about a minute,
  more on a loaded machine — and a session shows nothing while it does:
  run `fleet-lsp doctor` once in the repository to build it ahead.
- A `tools/go.mod` cannot serve a project with a `vendor/` directory or a
  `go.work` in scope (`-modfile` cannot run with either): pin gopls in that
  module's go.mod instead.
- One log file per session under `~/.local/state/fleet-lsp/<lang>/`.

## Uninstall

```sh
claude plugin uninstall fleet-lsp@fleet-lsp
claude plugin marketplace remove fleet-lsp
brew uninstall fleet-lsp
rm -rf "${XDG_STATE_HOME:-$HOME/.local/state}/fleet-lsp"
```

The last line removes the only files fleet-lsp writes outside its binary:
one log per session, which it already deletes after 14 days.

## Why no dependencies

fleet-lsp relays every byte between Claude Code and a language server, so it
links nothing it does not own (fleet decision `change.dependency-bar`).
`scripts/check-no-deps.sh` enforces that in CI. For the same reason its
arguments are parsed by hand, not with `clap`: `serve` is invoked by a
plugin manifest with a fixed argv and `doctor` takes one flag, so a parser
library would add a dependency and nothing it could act on (a deliberate
deviation from the `cli.basics.parse-with-a-library` heuristic).

## Development

`make check` runs what CI runs: the pinned toolchain, the no-dependency
check, `fmt`, `clippy -D warnings`, the tests, and a build on the
`rust-version` floor.
