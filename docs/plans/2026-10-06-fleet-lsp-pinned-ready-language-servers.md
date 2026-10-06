---
status: active
branch: feat/fleet-lsp
repos: [fleet-lsp, dotfiles, homebrew-tap]
adrs: [ADR-0008, ADR-0012, ADR-0017, ADR-0019, ADR-0020, ADR-0021]
---
# fleet-lsp: pinned, ready language servers

## Review panel

👉 **Decide:** none now — approve if Phase 0 should prove each server's barrier before code; per-server promises and v0.1.0 are asked later.
📍 fleet-lsp (new), dotfiles PR #17, homebrew-tap · plan only · next: Phase 0 readiness/pin investigation. Panel: backend, lang:rust, tui, unix, platform.
**Changed by review:** timer gate removed for proven per-server barriers; pins verified against declared sources (uv/pnpm workspace roots); output always drains, memory bounded at 202 MiB.
**Verdicts:** rounds 1–2: 5 approve-with-changes; your review: request-changes, applied; backend deltas ×4, final approve (two implementation notes in Full reviews).
📄 Full reviews: [2026-10-06-fleet-lsp-pinned-ready-language-servers.reviews.md](2026-10-06-fleet-lsp-pinned-ready-language-servers.reviews.md)

## Goal

Claude Code's `LSP` tool, which the review agents now carry (dotfiles PR #17),
answers from whatever server binary is first on PATH and answers *before the
server has finished indexing*. Measured 2026-10-06 in headless sessions:
rust-analyzer returned "No call hierarchy item" and 0 symbols in the first
seconds, then 7 correct callers after 90 s; pyright ran the global 1.1.414
while every Python repo pins 1.1.411 (`uv run pyright` in CI and pre-commit);
gopls was v0.18.1 built with go1.24.2 against modules at go 1.25/1.26.

fleet-lsp is one small binary between Claude Code and each server. Its two
promises, each only as strong as Phase 0 proves it can be:

- **Pinned:** the server that answers is *verified* against the
  repository's pin, not merely found in the repository; when it cannot be
  verified, every query gets an error naming the cause and the fix.
- **Ready:** a query is answered only after a *proven* readiness barrier for
  that server and version. Where Phase 0 finds no barrier, the promise for
  that server is narrowed in writing — never replaced by a timer.

## Non-goals

- No semantic search, call graph or index of its own (Brigade Code
  Intelligence and Code Search API were considered and set aside).
- No editor support beyond Claude Code; no Windows build in v0.1.
- Not fixing Claude Code's single workspace root per session (rooted at the
  launch cwd, one server set per session shared by subagents — community
  report, issue 62904). fleet-lsp makes that case *say so*.
- No language beyond Rust, Go, Python, TypeScript/JavaScript.
- No time-based readiness: no quiet window, no "probably loaded".

## Behaviour

**Commands** (clig.dev, `cli.conventions`). No colour anywhere (aval
precedent). Argv is parsed by hand: a few fixed shapes invoked by a
manifest, in a zero-dependency crate — a deviation from the
`cli.basics.parse-with-a-library` heuristic, argued in the README beside
`change.dependency-bar`.

- `fleet-lsp serve <rust|go|python|typescript> --min-version <x.y.z>` —
  speaks LSP on stdio; Claude Code launches it. `--min-version` is checked
  first, before any other argument: an older binary runs as the refusal stub
  with `fleet-lsp <own> is older than the plugin requires (<x.y.z>)` and fix
  `brew upgrade fleet-lsp`. **In `serve`, stdout carries framed LSP only.**
  Run with stdin on a TTY, it prints two lines to stderr and exits 2:
  `fleet-lsp serve speaks LSP and is run by Claude Code. Try:` /
  `  fleet-lsp doctor`.
- `fleet-lsp doctor [--json]` — from the current directory: the git root
  once as a header, then one block per language found (Root, below), each
  with its project root, pin, resolved server and **verdict class**:
  `verified` (installed version equals the pin), `compatible` (gopls from
  PATH, build Go ≥ the module — the one non-pin acceptance), or
  `refused` (reason + fix). Paths are relative to the git root or
  `~`-prefixed; reasons carry no paths; lines ≤ 80 columns; a fix command
  stands alone on its line; a refusal with no command says
  `fix: none — <why>`. Also prints the log dir, its newest session log, and
  `claude --version` against the minimum (2.1.288, needed for
  `requestTimeout`). Outside a git repository:
  `not in a git repository: <cwd>`, exit 1. `--json`: an array of
  `{lang, git_root, project_root, pin, server, version, verdict, reason,
  fix}` plus `{log_dir, claude_version, claude_min}`.
- `--help`, `--version`. Output through one helper that treats `BrokenPipe`
  as a silent exit 0.

**Exit codes** (in `--help` and the README; public interface,
`cli.compat.interface-is-public`): `doctor` 0 all verified/compatible, 1 any
refused or not in a repository, 2 usage. `serve` 0 when `exit` follows
`shutdown`, 1 otherwise, 2 usage or TTY. The `serve` argv, `FLEET_LSP_*`
variables and refusal-message shape are public interface too: a plugin
release that passes something new raises `--min-version` with it.

**Root.** Two roots, kept apart:

- *git root* — nearest ancestor of the session root (`rootUri` /
  `workspaceFolders`, else cwd) holding `.git`; the upper bound of every
  search. None → refused.
- *project root* — per language, found by walking **up** from the session
  root to the git root: rust the nearest `Cargo.toml`; go the nearest
  `go.work`, else `go.mod`; python the nearest dir holding `pyproject.toml`
  or `.venv`, **then lifted to the uv workspace root** when an ancestor's
  `pyproject.toml` has `[tool.uv.workspace]` whose `members` match it
  (trade-agents: a session in `packages/<m>` resolves to the repo root,
  where the pin and `.venv` live); typescript the nearest `package.json`,
  then lifted to the package-manager workspace root (`pnpm-workspace.yaml`,
  or a `package.json` `workspaces` that matches it), where the lockfile
  and hoisted `node_modules/typescript` live. If the walk up finds nothing,
  one walk **down** (depth ≤ 3, skipping `node_modules`, `.git`, `target`,
  `.venv`): exactly one candidate → it; several → refused, fix
  `start the session in one of:` + the list, each line relative to the git
  root. Starting a session in a listed directory resolves it by the walk up
  — tested for every fix fleet-lsp suggests.
- Child cwd = project root. Resolution happens once, at `initialize`.

**Pinned — accepted pin sources and verification:**

| lang | pin source (only these) | installed version read from | verdict |
|---|---|---|---|
| rust | `channel` in the nearest `rust-toolchain.toml` / `rust-toolchain` up to the git root (rustup's own search), and it must be exact: `x.y.z` or `nightly-YYYY-MM-DD` — `stable`/`beta`/`nightly` are refused (fix: pin the version `rustc +stable --version` shows) | `rustup which --toolchain <ch> rust-analyzer`, path must lie under that toolchain's dir; child gets `RUSTUP_TOOLCHAIN=<ch>` | verified / refused |
| go | `tool golang.org/x/tools/gopls` in `go.mod` (version from `go.mod`/`go.sum`, run as `go tool gopls`) | `go tool gopls version` | verified |
| go (no tool directive) | — | PATH `gopls`, build Go from `go version -m` ≥ the `go` directive | compatible (deviation from `build.toolchain-source`, `holds-until:` the repository pins gopls) / refused |
| python | `pyright==X` in the project root's `pyproject.toml` (`[dependency-groups]`, `[project.optional-dependencies]`, `[tool.uv] dev-dependencies`), cross-checked with the `pyright` entry in `uv.lock` | `.venv/lib/python*/site-packages/pyright-*.dist-info/METADATA` `Version:` | verified if equal; installed ≠ pin → refused `stale venv`, fix `uv sync`; no pin → refused, fix: add `pyright==<x>` to the dev group |
| typescript | the `typescript` version resolved in the workspace root's lockfile (`pnpm-lock.yaml`, `package-lock.json`, `yarn.lock`, `bun.lock`) | `node_modules/typescript/package.json` `version` | verified if equal, *and* the adapter's own report matches (below); stale → refused, fix the lockfile's install command |

**What is run** — always the verified binary by absolute path, never a PATH
lookup after verification: rust `<toolchain>/bin/rust-analyzer`; go
`<abs path of the go resolved during verification> tool gopls`, or the
verified PATH gopls by its resolved absolute path;
python `<project root>/.venv/bin/pyright-langserver --stdio`, with
`PYRIGHT_PYTHON_FORCE_VERSION` (and any `PYRIGHT_PYTHON_*` that selects a
version) removed from the child's environment, since the pyright-python
wrapper would otherwise run another version; typescript the adapter's
resolved absolute path. The session log header names the exact path run.

TypeScript adapter fallback: `typescript-language-server` (PATH, deviation
recorded) may fall back to another TypeScript when the configured one is
invalid. fleet-lsp passes `tsserver.path`, then reads the adapter's own
report of the TypeScript it selected (version and path, as the adapter
announces it after `initialize` — the exact message is fixed in Phase 0 from
the pinned adapter's source). The gate stays closed until that report
arrives and names the verified path and version; a different one switches
the session to refusal (`adapter selected TypeScript <v> from <path>, not the
repository's <v>`).

**Ready — barrier per server.** Phase 0 establishes, per server and pinned
version, one of:

- *signal barrier* — an exact, documented server message marks the load
  complete (rust-analyzer: `experimental/serverStatus`). fleet-lsp holds
  requests until it.
- *self-barrier* — the server itself blocks a request until the data it
  needs is loaded, shown by the adversarial tests below; fleet-lsp does not
  hold, and records the evidence in `docs/readiness.md`.
- *none* — neither holds. The README and refusal-free behaviour state the
  narrowed promise for that server: "reduces startup races; an empty answer
  in the first N s is not evidence", N being that server's slowest cold
  ready time measured in Phase 0. No timer stands in for a barrier.

rust-analyzer, all `health` states (`experimental/serverStatus`):

| status | action |
|---|---|
| `quiescent: false` | hold (gate re-closes whenever it goes non-quiescent) |
| `quiescent: true, health: "ok"` | release |
| `quiescent: true, health: "warning"` | release; the status `message` goes to the session log and, once per message, to the client as `window/showMessage` (Warning); `doctor` shows the newest session's last status |
| `quiescent: true, health: "error"` | do not release: every held and later request is answered with `rust-analyzer: workspace did not load: <message>`, fix from the message class (e.g. `cargo metadata` in the project root, or `cargo fetch` for missing dependencies); a later `ok`/`warning` re-opens |

fleet-lsp adds `experimental.serverStatusNotification` to the client
capabilities and does not forward those notifications (the client never
asked for them); the information reaches the client only as above.

**What is held** (signal barriers only): client→server requests other than
`initialize` and `shutdown`, plus the notifications behind a held request.
Never held: `initialize`, `initialized`, `shutdown`, `exit`,
`$/cancelRequest`, replies to server requests. When the head request leaves
the queue (forwarded, cancelled, failed), the notifications behind it go out
in order up to the next held request. A held request cancelled → answered
`RequestCancelled` (-32800). On `shutdown`/`exit`: held notifications
flushed, held requests failed (-32800), then `shutdown`/`exit` pass. A
request held past the ceiling is answered
`fleet-lsp: <lang>: server not ready after <N>s; log: <path>`; the ceiling
default is ≥ 2× the slowest cold ready time measured in Phase 0; env
`FLEET_LSP_CEILING_MS` overrides, a malformed value is logged and ignored;
`serve` logs a warning at `initialize` when the ceiling is ≥ the
`requestTimeout` it was built for (default ceiling + 30 s).

**Identity.** Frames are relayed byte for byte; a frame is scanned only for
its top-level `id` / `method` (nested values and strings, escapes included,
are skipped, not substring-searched). For tracking — cancel, response
matching, held queue — ids are **decoded** to a typed `Id::Int(i64) |
Id::Str(String)` with full JSON string unescaping, so the string id `abc`
and the same id spelled with a backslash-u escape for its `a` (code point
0061) are one id; the bytes sent on are the originals. Only
`initialize` is re-serialised (capability injection).

**Concurrency and backpressure.** Threads: client-stdin reader, child-stdout
reader, child-stdin writer, client-stdout writer, and the event loop. The
two directions never share a blocking path: **server→client output always
drains**, whatever client→server traffic is stalled. The child-stdout reader
puts frames into its own byte-bounded queue (16 MiB) that the
client-stdout writer drains directly; the event loop only observes those
frames (ids, status, progress) through a non-blocking copy of their
top-level scan and never sits between them. The child-stdout reader itself
**drops `experimental/serverStatus` frames after scanning them, before
queueing** (the loop still receives their content as an event), so the
client never sees them. Replies fleet-lsp writes itself (overload, ceiling,
refusal errors, `window/showMessage`) go into a **reserved 1 MiB share** of
the client-stdout queue that server frames cannot use; the loop never blocks
on it — that share full counts as "client not reading", and the 30 s
teardown below applies. The client-stdin reader feeds the event loop
through a byte-bounded queue (16 MiB); the loop hands client→server frames
to the child-stdin writer through another (16 MiB). The event loop never
performs a blocking write; it waits on one `mpsc` of events with
`recv_timeout` (readers post "frame queued" events, not frames). The held
queue is byte-bounded too (16 MiB). **Maximum frame size 32 MiB**, either
direction. Client frame over it: request → error `frame too large`,
notification → teardown, exit 1 (dropping it would desync the server).
Server frame over it: response → replaced by an error reply for its id,
server request → error reply to the server, notification → dropped and
logged. A frame larger than a queue's bound but within 32 MiB is admitted
only into an empty queue. Worst case: each of the four queues holds one
max frame and each of the two readers holds one in hand, so peak memory ≤
6 × 32 MiB + 10 MiB = 202 MiB.
Overload rules:

- held queue full → every held request is failed
  (`fleet-lsp: overload: <N> MiB held before the server was ready`) and the
  notifications behind them go to the child-stdin queue in order (none is
  dropped: the server's file state stays exact);
- child-stdin queue full for 30 s (child alive but not reading) → the child
  is declared hung: pending requests failed, child killed, `serve` exits 1;
- client-stdout queue full for 30 s (client not reading) → same teardown,
  exit 1.

Teardown never waits on a writer: on stdin EOF, stdout EPIPE, `exit`, or a
hung peer, the event loop closes the child's stdin, polls `try_wait` until a
5 s deadline, then `kill` + `wait`, then `std::process::exit` — which ends a
writer blocked on the client. If fleet-lsp itself is killed (std has no
signal API) the child's stdin closes and each server must exit on EOF —
proved per server in Phase 0. After `exit` is forwarded the child's exit is
expected (serve exits 0 if `shutdown` preceded, else 1). A child that exits
on its own before `shutdown` → every in-flight and held request gets an
error naming the exit status, `serve` exits 1 (`maxRestarts: 2` applies).
Child stderr is inherited.

**Refusal stub.** When resolution or verification refuses — at start, or
mid-session (adapter mismatch, rust-analyzer `health: error` stays in the
real-server path above) — fleet-lsp answers `initialize` with the same
static capabilities a real server of that language advertises for the LSP
tool's operations (`definitionProvider`, `referencesProvider`,
`hoverProvider`, `documentSymbolProvider`, `workspaceSymbolProvider`,
`implementationProvider`, `callHierarchyProvider`), so Claude sends its
requests instead of rejecting them locally. Every request then gets a
JSON-RPC error carrying the refusal and its fix, except `shutdown` → `null`
result; `exit` → exit 0 after `shutdown`, else 1; notifications are ignored.

**Log.** One file per session:
`$XDG_STATE_HOME/fleet-lsp/<lang>/<utc-timestamp>-<pid>.log` (default
`~/.local/state`) — no shared file, so no interleaving and no rotation
race. Lines: `<rfc3339> <event>`; the header line names git root, project
root, pin, server and version. At start a session deletes files of its own
language older than 14 days (unlinking a file another session holds open is
harmless on Unix).

**Plugin.** The repository is its own marketplace:
`.claude-plugin/marketplace.json` → `plugins/fleet-lsp/.claude-plugin/plugin.json`
declaring four `lspServers` (`command: "fleet-lsp"`,
`args: ["serve", "<lang>", "--min-version", "<this release>"]`, the extension
maps of the official plugins, `requestTimeout` = default ceiling + 30 s,
`maxRestarts: 2`). README and marketplace description state the minimum
Claude Code, 2.1.288. **The plugin follows the release tag, not `main`**
(`release.trigger`): the marketplace reads the `release` branch, which the
release job fast-forwards to the tag it just built — never a push to `main`
— with `contents: write` granted only inside the `release` environment. A
plugin newer than the installed binary meets `--min-version` and refuses
with the upgrade command, instead of running a mismatched pair.

## Phases

- [x] Phase 0 — **readiness and pin investigation, before any shipping
      code** (scratch harness outside the crate). For rust-analyzer (1.94.1,
      1.91.0), gopls v0.23.0, pyright 1.1.411, typescript-language-server +
      duro-app's TypeScript, in a real repo each:
      (a) a request sent at t=0 of a cold start — blocks, answers partial,
      answers empty, or errors? — "ready" is the time until a known query
      returns its known answer (the 90 s rust-analyzer figure was measured
      this way); for pyright, once also with an empty pyright-python cache
      (its first run downloads the npm build); (b) the first `didOpen` sent 30 s after
      `initialized`, then a workspace-wide request at once; (c) a delayed
      first progress (fake-delayed `workspace/configuration` reply, forcing
      a slow load) and a workspace with no progress at all; (d) the exact
      signal, if any, that marks load complete; (e) rust-analyzer `health`
      on a broken `Cargo.toml` and on missing dependencies (offline, empty
      `~/.cargo/registry` copy); (f) the adapter's TypeScript-selection
      report, and its fallback when `tsserver.path` is invalid; (g) each
      server's exit on stdin EOF. Result: `docs/readiness.md`, one row per
      server/version: barrier kind, evidence, cold ready times (sets the
      ceiling).
- [x] 🧑 decision: per server, accept the barrier Phase 0 found, or the
      narrowed promise where it found none.
- [ ] Phase 1 — repo skeleton: `gh repo create fredericrous/fleet-lsp --public`,
      single crate from attest's layout and aval's `Makefile`, `[lints]`,
      `rust-toolchain.toml` 1.94.1, `rust-version` measured with
      `cargo +<v> check --locked`, `.adr.yaml` + pack, CI (`ci.yaml`,
      `adr.yaml`), `check-no-deps.sh`, README with install and, right below,
      uninstall naming the state dir (`cli.dist.single-binary-easy-uninstall`).
- [ ] Phase 2 — transport + relay: framing, top-level scanner, typed ids,
      byte-bounded queues and writer threads, overload rules, teardown, stub,
      stdout discipline, exit codes, `--min-version`.
- [ ] Phase 3 — roots, resolvers and pin verification (table above),
      `doctor`, session logs.
- [ ] Phase 4 — barriers exactly as decided after Phase 0 (rust-analyzer
      status machine with all health states; adapter TypeScript check;
      self-barrier servers pass through).
- [ ] Phase 5 — plugin manifests, `release.yaml` + `bump-tap.py` from aval,
      four targets for the tap's four url/sha pairs (`aarch64-apple-darwin`,
      `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`,
      `aarch64-unknown-linux-gnu`); a **separate** homebrew-tap deploy key in
      a `release` environment limited to `v*` tags; the `release`-branch
      fast-forward.
- [ ] 🧑 decision: cut v0.1.0 (`work.release-on-request`)? Seed
      `Formula/fleet-lsp.rb` by hand, then set `PUBLISH_TAP` and re-run the
      release on `v0.1.0` by `workflow_dispatch` so `publish-tap`,
      `verify-brew` and the first `release` fast-forward run; `gh run view
      --json conclusion` must read `success`.
- [ ] Phase 6 — dotfiles, on PR #17's branch (pointer plan there), two
      commits: (a) Brewfile `fredericrous/tap/fleet-lsp` +
      `typescript-language-server`, `extraKnownMarketplaces` fleet-lsp;
      (b) once `fleet-lsp doctor` exits 0 in relais and
      authelia-oidc-operator on the machine, `enabledPlugins`
      `fleet-lsp@fleet-lsp` on, official `gopls/rust-analyzer/typescript/pyright-lsp`
      off, agent note: "fleet-lsp answers only from the pinned server once it
      is ready; an error names its cause and fix" (plus the narrowed promise
      for any server Phase 0 left without a barrier). Rollback = revert (b).
- [ ] Phase 7 — after a two-week soak: `npm rm -g pyright typescript
      typescript-language-server`, delete `~/go/bin/gopls.v0.18.1.bak`.

## Decision log

- 2026-10-06 — Public GitHub repo, GitHub Actions, Homebrew via
  fredericrous/tap (person's choice; `ci.system`).
- 2026-10-06 — No pin → refuse with the cause, never fall back to a global
  server (person's choice).
- 2026-10-06 — Zero dependencies like aval/attest; JSON copied from aval-core,
  used only on `initialize`.
- 2026-10-06 — Review rounds 1–2 and two backend deltas (see Full reviews).
- 2026-10-06 — Person's review: the quiet-window heuristic is dropped (it can
  declare ready before a late first progress, and never corrects itself);
  readiness becomes per-server and proven in Phase 0 before any code, or the
  promise is narrowed. Supersedes "readiness gate on all four servers" as a
  timer. Also: rust-analyzer `health` handled in all states; pins verified
  against declared pin sources, not merely found; byte-bounded,
  non-blocking writes; git root vs project root; stub capabilities and
  lifecycle; `--min-version` compatibility; typed ids; per-session logs.
- 2026-10-06 — gopls without a `tool` directive and the TS adapter come from
  PATH: verdict `compatible` / adapter report checked, recorded deviations
  from `build.toolchain-source`, `holds-until:` the repositories pin them.

- 2026-10-07 — Phase 0 done (`docs/readiness.md`). Person's decisions:
  barriers accepted for all four — rust-analyzer `serverStatus`
  `quiescent: true`; gopls self-barrier (no hold); pyright 1.1.411 the
  enumerator's `Found <n> source file(s)` / `No source files found.` log
  line; typescript-language-server 6.0.1 the end of the `Initializing JS/TS
  language features…` progress token — the last two keyed to the measured
  version, any other version gets the narrowed promise until measured.
  rust-analyzer `health: warning` for missing dependencies → answer + warn
  (as planned). Ceiling 370 s (2 × lldap's 185 s to quiescence),
  `requestTimeout` 400 s. Consequences adopted: teardown deadline 10 s
  (rust-analyzer took 7.4 s to exit on EOF); fleet-lsp keeps
  `python.analysis.logLevel` ≥ Information in the `workspace/configuration`
  replies it relays, or pyright's barrier line never arrives.

## Verification

- Phase 0: `docs/readiness.md` holds, per server/version, the observed
  answer to (a)–(g) with the raw transcript paths outside the worktree; the
  🧑 decision is recorded under it.
- Unit: framing (split headers, two messages in one read, a body cut at a
  read boundary); scanner (`"method"` inside `params`, `"id"` inside a string
  with an escaped quote, a quote written as a backslash-u escape, `}{` in a
  body); typed ids (the id `abc` and the same id with its `a` written as a
  backslash-u escape match for cancel and response matching; `1` vs `"1"`
  distinct); frame size (32 MiB + 1 from the client refused; from the server
  a response replaced by an error reply, a notification dropped; 20 MiB
  admitted only into an empty queue); a `serverStatus` frame never reaches
  the client queue; fleet-lsp's own replies use the reserved share, and that
  share full starts the 30 s teardown;
  rust status machine (every row of the health table; re-close; error →
  refusal text; error → ok re-opens); ceiling with injected `Instant`;
  overload rules; pin verification (rust `stable` refused; venv 1.1.414 vs
  pin 1.1.411 refused `stale venv`; `node_modules/typescript` ≠ lockfile
  refused; gopls build Go < module refused); roots (session in
  `services/api` of `/repo` with `services/api/go.mod` → project root
  `services/api`; two venvs → refused, and a session started in each listed
  directory resolves; a session in trade-agents' `packages/<m>` → the uv
  workspace root, pyright 1.1.411 verified; a pnpm workspace member → the
  workspace root's lockfile; `PYRIGHT_PYTHON_FORCE_VERSION` set in the
  parent → absent in the child).
- Integration (fake servers = script fixtures reached through the real
  resolvers; no hidden mode in the binary): rust-analyzer-shaped fake
  delays quiescence 3 s → a request at t=0 answered at t≥3 s, a following
  `didChange` arrives after it; fake reports `health: error` → the request
  gets the workspace-did-not-load error, `warning` → answered plus one
  `window/showMessage`; fake sends `workspace/configuration` before ready →
  answered within 1 s; held request cancelled → its trailing `didChange`
  reaches the fake within 100 ms; **fake alive but not reading stdin** while
  a 20 MiB `didOpen` is sent → teardown within 36 s, exit 1, fake gone;
  **client not reading stdout** while the fake floods diagnostics →
  teardown within 36 s; **fake reading stdin slowly (1 MiB/s) while
  flooding diagnostics** → its output keeps reaching the client and it is
  not torn down for 60 s; **sustained traffic while the gate is closed**
  (1,000 requests + 150 MiB of `didChange` in 32 MiB frames, the fake
  flooding 32 MiB diagnostics) → overload errors, no notification lost
  (fake counts them), peak RSS ≤ 202 MiB; stdin
  EOF and SIGTERM while a request is held → fake gone within 6 s;
  `shutdown` then `exit` → exit 0; stub: `initialize` advertises the
  capability set, a request gets the refusal, `shutdown` → `null`, `exit`
  → 0; plugin args with a newer `--min-version` → stub with the upgrade fix;
  `doctor | head -1; echo ${pipestatus[1]}` in duro-app → 0.
- Live, through a real Claude LSP call (`claude -p`, session started in the
  repo, first call of the session):
  - relais `incomingCalls` `resume.rs:169` → 7 callers incl. `main.rs:3986 reconcile_run`;
  - duro-app `incomingCalls` `useCopyFeedback.ts:9:17` → the 8 callers of 2026-10-06 (ApiKeysSection, UnsupportedBrowser, ClaimLinkDialog, GitKeysSection, InvitePasswordReveal, the test module, InviteQrDialog, PasswordCard);
  - authelia-oidc-operator `incomingCalls` `pkg/assembler/assembler.go:30:6` → SetupWithManager, TestAssemble, TestAssembleConfidentialClientWithoutSecretRef;
  - trade-agents `documentSymbol` `ops/backfills/jobs/lake-phase0-duckdb-probe.py` → BUCKET, GLOBS, CONFIGS, time_wait, run, name/s;
  - openwebui-tts-proxy (no `.venv`) → the agent's reply quotes the refusal and `uv sync`;
  - `~/Developer/Perso` (no repo) on a `.rs` file → the agent quotes `not in a git repository`;
  - a Cargo workspace with a broken `Cargo.toml` → the agent quotes `workspace did not load`;
  - a Cargo project whose dependencies are not fetched (offline, `CARGO_HOME` pointing at an empty dir) → the agent quotes the health message and `cargo fetch`;
  - trade-agents, session started in `packages/<m>` → `fleet-lsp doctor` there shows pyright 1.1.411 verified, and an LSP call answers.
- Live, versions: `fleet-lsp doctor` in relais (rust 1.94.1 verified), lldap
  (1.91.0 verified), sre-agent (pyright 1.1.411 verified), duro-app
  (TypeScript verified, adapter report matching), authelia-oidc-operator
  (gopls compatible) → exit 0, widest line ≤ 80; `doctor --json | jq .`
  parses.
- Measured and recorded: per server cold ready time (sets the ceiling);
  `workspace/configuration` reply delay through fleet-lsp (< 50 ms); no
  non-framed byte on stdout over one live session; two parallel sessions
  write two separate logs.
- `make check` green locally and in CI; release conclusion `success`
  including the `workflow_dispatch` re-run; `brew install
  fredericrous/tap/fleet-lsp && fleet-lsp --version` → 0.1.0; after a no-op
  merge to main, a fresh plugin install resolves the `v0.1.0` tag commit.
- Phase 6: `chezmoi apply` of (a) alone leaves the official plugins on; (b)
  only after `doctor` exits 0 in relais and authelia-oidc-operator.

## Outcome

<!-- panel: repos=fleet-lsp,dotfiles,homebrew-tap adds=lang:rust,cli,ops reviewers=backend,lang:rust,tui,unix,platform body-sha=3f0368b55cb4 -->
