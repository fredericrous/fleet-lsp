---
status: done
plan-id: p-01M7ZWPG42B1QJ5ZPDTZZSAVXC
branch: feat/guidance-on-contact
repos: [fleet-lsp]
adrs: [guidance.session-load]
phases: [A]
---
# fleet-lsp: guidance only where it serves, diagnostics capped

## Review panel
👉 **Decide:** the review grew the plan ×1.37 (mostly fixes for high findings); approve keeps it all.
📍 fleet-lsp · 1 release · next: worktree, then A1. Panel: backend, lang:rust, tui, unix (opus), codex (gpt-6-astra).
**Changed by review:** cap parses past 1 MiB, in one function; the marker is claimed once (O_EXCL); hooks.json passes explicit events, agents looked up by `name`.
**Carried, settled here:** an unknown event exits 2 (A1's exit-0 list is wrong on that); add a 64 KiB per-file byte budget; fast path logs nothing; p95 ≤ 20 ms.
📄 Full reviews: [reviews](2026-11-21-guidance-only-where-it-serves.reviews.md)
**Verdicts:** 3 approve-with-changes, 1 approve; 14 carried; words 1058 → 1451 (×1.37); remove 0, merge 0.

## Picture

```
 today                        after this plan
 -----                        ---------------
 session start -> guidance    session start -+- project -> guidance
 subagent ------> guidance                   `- none ----> (wait)
 server -> all diagnostics    first .rs/.go/.py/.ts touch -> guidance
                              subagent -+- has LSP -> guidance
                                        `- no LSP --> nothing
                              server -> [cap 20 per file] -> diagnostics
```

The guidance used to arrive at every start and every subagent; after this
plan it arrives once, when the session first works on a served language and
only for agents that can use LSP, and diagnostics pass a per-file cap.

## Context
Claude Code loads plugin output into context. fleet-lsp's footprint: the `lspServers` config (0 tokens), the deferred `LSP` tool (a name), the ~1 KB guidance from `plugins/fleet-lsp/hooks/code-intelligence.sh` (SessionStart + every SubagentStart), and the diagnostics it relays after edits (unbounded). The person asked for three changes:
1. **Subagents without the LSP tool get no guidance** (browser-check, plan-review-po/ui-design/unix, claude-code-guide, …).
2. **Guidance loads when the session works on .rs/.go/.py/.ts, not merely starts.** Sessions usually start in `~/Developer/Perso` (no repo) and work across projects, so: inject at SessionStart when the start directory is a project of a served language; otherwise inject once, the first time a tool touches a file with a served extension.
3. **Diagnostics are capped per file**, so a noisy repository cannot flood context. Claude Code documents no cap or filter of its own (plugins-reference, code-intelligence docs: "errors and warnings" are pushed after an edit).

## Approach

### A1. `fleet-lsp hook <event>` — the hook logic moves into the binary
New `src/hook.rs`, a `hook` command in `src/cli.rs` / `src/main.rs`. `HELP` gains `fleet-lsp hook <session-start|post-tool-use|subagent-start>` ("Claude Code runs it from the plugin; a person never does") and its exit codes; an unknown event or a terminal on stdin exits 2 with a usage line instead of blocking on stdin (as `serve` refuses a terminal); a `cli.rs` test covers both. It reads the hook's JSON from stdin with the crate's own `json` module (no new dependency; `scripts/check-no-deps.sh`). The guidance text moves to a `const` there, unchanged. Exit codes: 0 on every input problem (unparseable JSON, unknown event, missing field: logged to the log dir, nothing printed); 2 only for usage errors; stdout carries the context alone.
`code-intelligence.sh` becomes a thin wrapper: no binary → exit 0 (as today); else it runs `fleet-lsp hook "$@" 2>/dev/null || exit 0` (stdin passes through), so an older binary without `hook` stays silent instead of raising a hook error.
`hooks.json` passes explicit wrapper arguments: `session-start` for SessionStart, `subagent-start` for SubagentStart, and `post-tool-use` for the new PostToolUse entry (matcher `Read|Edit|Write|MultiEdit`). Replace the existing empty argument and `subagent` argument. In check-plugin, execute the actual commands extracted from hooks.json against representative inputs and assert their event-specific output.

### A2. SubagentStart: skip agents without LSP
Input carries `agent_type` (hooks reference). Resolve definitions by their frontmatter `name`, not filename, searching `<cwd>/.claude/agents/`, then `~/.claude/agents/` (`plugin:name` types: search the plugin's active installed agents directory). Parse `tools` and `disallowedTools` as comma-separated strings or YAML lists, including block lists; unsupported syntax is unknown and follows the fail-open rule. Skip guidance when a parsed tools allowlist contains neither `LSP` nor `*`, or the parsed denylist excludes LSP. Add fixtures whose filename differs from `name`, with block-list LSP allowance and denial, and with unsupported syntax. Built-ins without LSP (`claude-code-guide`, `statusline-setup`) are a short constant list. Anything unknown or unreadable → inject (fail open: today's behaviour).

### A3. SessionStart and PostToolUse: guidance on first contact
- **SessionStart:** first delete this `session_id`'s marker (a compaction or clear dropped the earlier copy). If `resolve::git_root(cwd)` (`src/resolve.rs:114`) finds a repository and `resolve::project_root` (made `pub(crate)`, `src/resolve.rs:226`; its downward walk is bounded by `DOWN_DEPTH` and `SKIP_DIRS`) finds a project for any of the four languages from `cwd` → print the guidance and write the marker. Otherwise print nothing.
- **PostToolUse:** input carries `agent_id` (a subagent) → exit 0. Marker present → exit 0 at once. Else if `tool_input.file_path` ends in a served extension (the set in `plugin.json`: `.rs .go .py .pyi .ts .tsx .js .jsx .mts .cts .mjs .cjs`) → create the marker with `create_new(true)` (O_EXCL) and print `{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"<guidance>"}}` only if that creation succeeded (parallel tool calls run parallel hooks; they inject once).
- **Marker:** `log::log_dir()`'s sibling `$XDG_STATE_HOME/fleet-lsp/guided/<session_id>` (empty file; a `session_id` outside `[A-Za-z0-9._-]` (as `log.rs` keeps repo names) is refused → no marker, inject). Markers older than 7 days are pruned on write, as `log::prune` does for logs. Each hook decision and each capped push (URI, dropped count) is a line in the fleet-lsp log.

### A4. Diagnostics capped at 20 per file
One function `core::cap_diagnostics(items: Json) -> Json`: drop severity 3/4 (information, hint; a missing severity counts as an error), order errors before warnings (stable), keep the first 20; when more were dropped, append one diagnostic at 0:0, severity 2, source `fleet-lsp`: `N more diagnostics (E errors) not shown: fleet-lsp keeps 20 per file; for the full list run <cmd>`, where <cmd> is the server's language: `cargo check`, `go vet ./...`, `pyright`, `npx tsc --noEmit`; ` (E errors)` is omitted when E is 0.
Applied on both paths:
- **pushed** — `textDocument/publishDiagnostics` from the server in `relay::child_reader` (`src/relay.rs:568`) and the workspace slot reader (`src/workspace.rs:~1606`): parse with `json::parse` up to `frame::MAX_FRAME` (not `parse_small`, which stops at 1 MiB), cap, re-serialise only when the cap removed something (else forward byte for byte; update the `src/json.rs` module doc); an unparseable frame is forwarded unchanged. The cap lives in `relay::observe_notification`, which returns the possibly rewritten body, so both readers share it.
- **pulled** — `core::publish` (`src/core.rs:965`) caps `items` before building the notification.

### A5. Docs, checks, release
- `scripts/check-plugin.sh`: the content checks run with the built `target/debug/fleet-lsp` on PATH (the empty stub prints nothing now); the stub stays only to prove an old binary leaves the wrapper silent with exit 0. The wrapper prints ≤ 6 lines for a SessionStart input in a Rust repo, nothing for one from a non-repository directory; `hooks.json` runs the three events.
- README section on the hook and the cap; CHANGELOG via `tag-release`.
- Release 0.7.0 (plugin.json `version` + 4× `--min-version`), tap bump (`scripts/bump-tap.py`), then `brew upgrade fleet-lsp` and the plugin update on this machine.

## Critical files
`src/hook.rs` (new), `src/cli.rs`, `src/main.rs`, `src/resolve.rs` (visibility), `src/core.rs`, `src/relay.rs`, `src/workspace.rs`, `tests/relay.rs`, `plugins/fleet-lsp/hooks/{code-intelligence.sh,hooks.json}`, `plugins/fleet-lsp/.claude-plugin/plugin.json`, `scripts/check-plugin.sh`, `README.md`.

## Non-goals
- The greenrack SessionStart hook (33.7 KB): a separate plan in greenrack.
- A cap on the total across files: each `publishDiagnostics` names one file, so fleet-lsp can only bound per file.
- Splitting the plugin per language: the `lspServers` config costs no context.

## Code review
- fleet-lsp: backend, lang:rust, unix (tui left out: no layout or prompt changes)

## Verification (Part A)
- `make check` (fmt, clippy, tests, no-deps, check-plugin, msrv) exits 0 in the worktree.
- `tests/relay.rs`: a 2 MiB push of 6,000 diagnostics → the client receives 21; a `hook.rs` test runs two `post-tool-use` calls for one fresh `session_id` in parallel threads → one JSON total.
- 100 `post-tool-use` runs of the wrapper with a marker present → p50/p95 wall time recorded in the plan's decision log.
- Unit tests in `src/hook.rs` with temp dirs, the state dir passed as a parameter (no `set_var`; `main` alone reads `log::log_dir()`): SessionStart from a non-repo dir → empty; from a repo with `Cargo.toml` → guidance + marker; PostToolUse on `/x/a.rs` with no marker → JSON once, then nothing; on `/x/a.md` → nothing; SessionStart (compact) clears the marker; SubagentStart for a definition with `tools: Read, Bash` → nothing, with `tools: Read, LSP` or no `tools:` → JSON, unknown type → JSON, `claude-code-guide` → nothing; a `session_id` of `../x` writes nothing outside the dir. FALSIFY: removing the marker check makes the "then nothing" test fail.
- `core` unit tests for `cap_diagnostics`: 50 errors → 20 + summary; 5 hints + 3 warnings → 3 warnings, no summary; warnings before errors in input → errors first in output; exactly 20 → no summary.
- `tests/relay.rs`: a fake server pushing 50 diagnostics for one URI → the client receives 21 items; a push over 1 MiB is capped too; a 5-item push arrives byte-identical; a pull-mode fixture likewise.
- The real binary, built from the committed tree, with `XDG_STATE_HOME=$(mktemp -d)` so no marker lands in the real state dir: `printf '{"session_id":"t1","cwd":"%s","source":"startup"}' ~/Developer/Perso | fleet-lsp hook session-start` prints nothing; with `cwd` = the fleet-lsp checkout it prints the guidance; `post-tool-use` with a `.rs` `file_path` prints the JSON once for a fresh `session_id`; `subagent-start` with `agent_type: browser-check` prints nothing, `Explore` prints the JSON.
- After release and install: a new Claude Code session started in `~/Developer/Perso` shows no fleet-lsp SessionStart text; after reading a `.rs` file, the guidance arrives once beside that tool result.

## Decision log
- 2026-11-21 — done by hand, not relais: the run waited its full 1800 s admission budget (load ~585, 7 jobs queued on 3 slots) and launched nothing.
- 2026-11-22 — the pushed cap does not parse the frame whole: the first version did, and `sustained_traffic_while_closed…` peaked at 307 MiB (limit 202) on 32 MiB diagnostics frames. `core::cap_published` finds the array's elements by byte spans (`src/spans.rs`), parses only elements up to 64 KiB for their severity (a larger one cannot be kept and counts as an error) and splices the kept bytes; the peak went back to 162 MiB, and kept diagnostics stay byte for byte (no re-sorted keys). `core::cut` is the one selection both paths share.
- 2026-11-22 — carried items settled: an unknown event exits 2 (usage); the 64 KiB byte budget is in; the marker fast path logs nothing; `observe_notification` returns a struct; agents are found by `name` (`<type>.md` first), plugin agents from `installed_plugins.json`'s one `installPath` (several → inject).
- 2026-11-22 — PostToolUse with an unusable `session_id` logs and prints nothing, rather than inject on every call as A3 read.
- 2026-11-22 — the hooks.json-command checks against the real binary live in `tests/hook.rs`, not `scripts/check-plugin.sh`: CI's `deps` job runs the script without a Rust build. The script keeps the wiring checks and the old-binary/no-binary silence.
- 2026-11-22 — deliberate: the p95 ≤ 20 ms bar is not met and the design stays. Measured on this host (load 25–50, debug build): `post-tool-use` with a marker p50 89 ms / p95 148 ms through the wrapper; `sh -c true` alone p50 38 / p95 56 ms, `fleet-lsp --version` alone p50 18 / p95 44 ms. The hook costs one shell plus one process start; the bar was set without a baseline.

## Implementation review
- implementation-review (opus): R1 approve-with-changes (swallowed claim error, banners, names) fixed; 3 deltas, last approve (55k/32 s, 95k/106 s, 108k/110 s, 30k/15 s).
- backend (opus): R1 4 findings (unbounded hook.log, `.` fallback, untested guards) fixed; deltas approve (81k/72 s, 38k/34 s, 30k/14 s, 22k/10 s).
- lang:rust (opus): R1 6 (Forward/ToolAccess enums, Capped, expect) fixed; deltas approve (72k/48 s, 35k/29 s, 29k/9 s, 22k/9 s).
- unix (opus): R1 3 (marker kept on lost write, `hook -x`) fixed; delta approve (55k/24 s, 35k/22 s); deliberate: `hook --help` already prints help (`asks_for_help` runs first).

## Verification record
- `make check` parts: fmt, clippy `-D warnings`, `cargo test` (all suites, run by the commit gate), `check-no-deps.sh`, `check-plugin.sh` → all pass. Falsified: dropping `post-tool-use` from hooks.json fails `check-plugin.sh`.
- Cap unit tests (`core::tests`): 50 errors → 20 + summary; hints/info → dropped, no summary; errors first; exactly 20 and empty → unchanged; 1 warning dropped → "1 more diagnostic not shown … `npx tsc --noEmit`"; huge messages → ≤ 64 KiB, valid JSON → pass.
- `tests/relay.rs`: 50-item push → 21 with "(5 errors)"; 6,000 × 300 B push (≈2.4 MiB) → 21, frame < 70 KiB, next frame reads and is forwarded byte-identical; workspace push → 21; tsgo pull with 31 errors → 21 → pass. Overload test peak RSS 162 MiB (limit 202).
- `src/hook.rs` + `tests/hook.rs` (hooks.json commands via `sh -c`, temp state): non-project start silent, repo start guides (≤ 6 lines); first `.rs` touch guides once, `.md` nothing, second touch nothing and no log line; compaction clears the marker; `agent_id` touch silent, no marker; 8 parallel first touches → 1 output; unsafe ids write nothing; non-JSON silent; agents by `name`, comma/flow/block lists, deny list, unsupported syntax → inject; plugin agent from its one install → pass.
- Real binary, temp `XDG_STATE_HOME`, this machine's `~/.claude/agents`: start in `~/Developer/Perso` → 0 lines; in the fleet-lsp checkout → guidance; `post-tool-use` on `fleet-lsp/src/main.rs` → JSON, then nothing; `browser-check`, `plan-review-po` → nothing; `Explore`, `implementation-review`, `relais:relais-research` → JSON (1165 bytes); `hook bogus` → exit 2 → as expected.
- After release and install (fleet-lsp 0.7.0, #40 + #41, tag `v0.7.0`; release run 45074776454: four platform archives, tap published and install-tested; `brew upgrade` and `claude plugin update` here → 0.7.0): a headless Claude Code session started in `~/Developer/Perso` (session ee630dfb) logged no SessionStart guidance, then on its Read of `fleet-lsp/src/main.rs` the hook log has `PostToolUse ee630dfb…: guided on …/fleet-lsp/src/main.rs`, once → as expected.

<!-- panel: repos=fleet-lsp adds= reviewers=backend,lang:rust,tui,unix,codex body-sha=d8411fd59f74 -->
