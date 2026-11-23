---
status: done
plan-id: p-01M830BR59896ZBW1G44MMJ4HM
branch: feat/guidance-mod
repos: [fleet-lsp]
adrs: [guidance.session-load]
phases: [P0, A]
---
# fleet-lsp: first-contact guidance in process, no spawn per call

## Review panel
👉 **Decide:** the review grew the plan ×1.44 (fixes for 6 high findings: races, compaction, origin, release); approve keeps it all.
📍 fleet-lsp · 1 release · next: P0, then the mod. Panel: backend, lang:rust, lang:typescript, tui, unix (opus), codex (gpt-6-astra).
**Changed by review:** one guide via `$.state` compare-and-set; marker re-checked, compaction reset only when applied; plugin-origin Reads skipped; Cargo bumped with the plugin.
**Carried, settled here:** session ids `.`/`..` rejected; the PATH shim is checked to be reached; core-origin mirror test; temp copies cleaned by a Drop guard (no new crate); canary command on the release checklist.
📄 Full reviews: [reviews](2026-11-23-guidance-in-process-no-spawn.reviews.md)
**Verdicts:** 4 approve, 1 approve-with-changes; ~12 carried; words 1028 → 1483 (×1.44); remove 0, merge 0.

## Picture

```
 today (0.7.0)                    after this plan
 -------------                    ---------------
 Read/Edit/Write any file         Read/Edit/Write a.md
   -> sh -> wrapper -> fleet-lsp    -> matcher: no match, nothing runs
      (~80-150 ms, every call)    Read/Edit/Write x.rs (main loop)
                                    -> mod hook, in process:
                                       guided? -> result as is
                                       first   -> result + guidance
 SessionStart / SubagentStart     SessionStart / SubagentStart
   -> fleet-lsp hook (1 spawn)      -> unchanged (1 spawn each)
```

The tool call used to start two processes on every Read, Edit and Write;
after this plan it starts none, and the first served file still brings
the guidance once.

## Context
fleet-lsp 0.7.0 gives the code-intelligence guidance on first contact through a PostToolUse command hook (matcher `Read|Edit|Write|MultiEdit`). Measured with the installed release build (load ~98): `sh -c` 49 ms + the wrapper 23 ms + the binary 32 ms, paid on every such call for the whole session, even after the guidance was given. The cost is process start-up, not code. The person chose (2026-11-23) to replace the PostToolUse hook with a mod: an in-process hooks module, which Claude Code runs without spawning anything.

Facts from the mod API (Claude Code 2.1.330 types, `plugin-authoring` reference):
- `on('tool.call', matcher, hook)`: the matcher is a partial of `e` (array = any-of, RegExp tests a string), so `{ tool: ['Read','Edit','Write'], file_path: /…/ }` keeps the hook off every other call. The tool input fields are on `e` (`e.file_path`); `e.agentId` is set only inside a subagent's or fork's loop.
- `await next(e)` resolves the tool's result; returning `{ ...r, context: [...(r.context ?? []), text] }` adds text the model reads after the result and the user never sees (the PostToolUse `additionalContext` equivalent).
- `on('session.compact', …)`: `e.agentId` absent for the main conversation.
- `$.state` keeps values across a module reload (contract in `types/index.d.ts`); `$.session.id()`, `$.env.get`, `$.fs.exists`.
- `MultiEdit` does not exist on this build; the tools are Read, Edit, Write.

## Approach

### P0. Can `hooks.json` hold command hooks and `modules` together?
Undocumented. In the worktree, a `hooks.json` with both, checked by `claude plugin validate plugins/fleet-lsp` and a headless `claude -p --plugin-dir plugins/fleet-lsp` run (SessionStart output in a served repo, plus the mod's guidance on a `.rs` Read). If the engine refuses the mix: the SessionStart and SubagentStart command hooks move to `hooks/hooks.json` siblings the engine accepts (decided then, recorded in the Decision log), never dropped.

### A1. The mod: `plugins/fleet-lsp/hooks/register.ts`
- `on('tool.call', { tool: ['Read','Edit','Write'], file_path: SERVED }, hook)`, `SERVED` the extension set of `plugin.json`'s `lspServers` as one RegExp (`/\.(rs|go|pyi?|[jt]sx?|[mc][jt]s)$/`, rejecting `.mtsx`, `.cjsx`, `.rsx`, `.json`, `.pyc`, `.tsbuildinfo` — written out and tested against the exact list).
- The hook: `const r = await next(e)`; return `r` unchanged when `e.agentId` is set (SubagentStart decided a subagent's guidance), when `next.origin.tier !== 'core'` (plugin-initiated calls do not deliver context), when `r.deny` or `r.isError`, or when the session is already guided; else claim `guided` with `$.state.set(ref, true, { ifVersion: held.version })` and, only when `isSet`, return `{ ...r, context: [...(r.context ?? []), GUIDANCE] }` (parallel Reads: one wins, as the binary's `create_new` claim did).
- "Already guided" = `$.state` key `guided` (`{ plugin: 'fleet-lsp', key: 'guided' }`, boolean) OR the binary's SessionStart marker exists (`<XDG_STATE_HOME or HOME/.local/state>/fleet-lsp/guided/<session id>`, absolute paths only, as `log::state_dir()`; a session id outside `[A-Za-z0-9._-]+`, empty included, counts as no marker, as `hook::marker`), checked with `$.fs.exists` on every call where `guided` is not true (at most once per guidance cycle), and a present marker writes `guided` true.
- `on('session.compact', …)` with no `agentId`: `const r = await next(e)`, then set `guided` false only when `r.skip` is undefined and `e.trigger !== 'precompute'` (a precompute installs nothing) (the compaction drops the earlier copy; the binary's SessionStart(compact) already deletes its marker).
- A failure in the hook is skipped by the engine and the tool result passes unchanged (no `.catch` guard: nothing here refuses anything).
- `types/index.d.ts` declares `PluginState['fleet-lsp'] = { guided: boolean }`; `plugin.json` gains `"types"`.

### A2. One guidance text, checked for drift
`GUIDANCE` exists in `src/hook.rs` (SessionStart, SubagentStart) and now in `register.ts`. A Rust unit test in the `#[cfg(test)]` module of `src/hook.rs` (`GUIDANCE` is `pub(crate)`, invisible to `tests/hook.rs`) reads `register.ts` through `include_str!`, extracts the guidance as an array of 6 single-quoted lines, and asserts it equals `GUIDANCE.lines()` in order. A second unit test checks that the regex literal in `register.ts` accepts `x.<ext>` for every entry of `SERVED`. An edit to one side fails until the other follows.

### A3. Plugin wiring
- `hooks.json`: drop the `PostToolUse` entry; add `"modules": ["./register.ts"]`.
- `plugin.json` description names the floor as relais's does: "Tested on Claude Code >= 2.1.330". On an engine without mods the session still gets SessionStart and SubagentStart guidance; first-contact guidance needs the mod.
- `fleet-lsp hook post-tool-use` stays in the binary (an older plugin copy still calls it; its wrapper swallows a usage exit 2, so removing it would silently drop that copy's first-contact guidance) and is documented as unused by the plugin: in `src/cli.rs` HELP's `hook` paragraph ("`post-tool-use` is kept for plugin copies older than 0.7.1"), with the help test updated to match; usage line and exit codes unchanged.
- `scripts/check-plugin.sh`: the event loop (line 25) checks session-start and subagent-start only, the `MultiEdit` matcher check (line 29) becomes a check that there is no `PostToolUse`; `modules` names an existing file; `claude plugin validate` is not run there (CI has no Claude Code). A test (`tests/check_plugin.rs`) runs the script in a full copy of the repository root in a temp directory removed on exit, on each mutation (PostToolUse restored; module file missing; plugin.json version differing from Cargo.toml), expecting exit 1 and stderr holding that mutation's own message and no other line. `register.test.ts` and `tsc` run only locally, so the release checklist lists them.
- README.md (the note, lines 64-73): the hook log records SessionStart and SubagentStart decisions only; first-contact guidance comes from the mod and shows only as a `context` row in the session transcript; on Claude Code older than 2.1.330 there is no first-contact guidance (also in the CHANGELOG).

### A4. Release
0.7.1 (plugin behaviour change plus binary docs/tests/version): bump the package version in Cargo.toml and its Cargo.lock entry alongside CHANGELOG, plugin.json version and all 4× `--min-version`. Publish the matching binary through the existing v0.7.1 release workflow, including its tap verification and release-branch advancement, before `brew upgrade` and `claude plugin update`. Verify the installed binary reports 0.7.1 and the installed plugin requires that version.

## Non-goals
- SessionStart/SubagentStart as mods: one spawn per session or subagent is not the cost in question.
- Removing the binary's `post-tool-use` event.

## Critical files
`plugins/fleet-lsp/hooks/register.ts` (new), `plugins/fleet-lsp/types/index.d.ts` (new), `plugins/fleet-lsp/hooks/hooks.json`, `plugins/fleet-lsp/.claude-plugin/plugin.json`, `plugins/fleet-lsp/hooks/register.test.ts` (new), `src/hook.rs` (drift and regex unit tests), `src/cli.rs` (HELP `hook` paragraph and its test), `tests/check_plugin.rs` (new), `Cargo.toml`, `Cargo.lock`, `scripts/check-plugin.sh`, `README.md`, `CHANGELOG.md`.

## Code review
- fleet-lsp: backend, lang:typescript, lang:rust, unix, tui

## Verification
- `claude plugin validate plugins/fleet-lsp` reports the module, its two hooks and the `$.state` key, and nothing refused; `tsc -p plugins/fleet-lsp` passes.
- `claude plugin test plugins/fleet-lsp` (`register.test.ts`): a Read of `x.rs` on the main loop → result plus one `context` entry holding the guidance; a second `.rs` Read → no context; a Read of `a.md` → the hook never runs; a Read with `agentId` → no context; the plugin-origin case is not testable in the kit (Decision log 2026-11-23) and rests on the canary; after `session.compact` → the next `.rs` Read guides again; a marker present for the session → no context; compact, then a marker present → no context; an errored Read → no context; two concurrent `.rs` Reads → exactly one context entry between them. FALSIFY: dropping the `guided` check makes the second-Read test fail.
- `make check` (fmt, clippy, tests including the drift test, no-deps, check-plugin) exits 0. FALSIFY: one changed word in `register.ts`'s guidance fails the drift test.
- Canary: a headless session shows exactly one guidance `context` entry; repeat after each Claude Code upgrade, because a failing mod is skipped silently.
- P0 recorded: what `validate` and the headless run showed for the mixed `hooks.json`.
- Cost: in a headless session with the plugin loaded, 20 Reads of `.md` files and 20 of `.rs` files after the first: no `fleet-lsp` process is started (counted by a `fleet-lsp` shim first on PATH that appends one line per call to a file, then execs the real binary: the count equals the SessionStart/SubagentStart events; the hook log cannot show this, since the guided path logs nothing, src/hook.rs:604) — the per-call cost is the engine's own matcher.
- After release and install: a headless session in `~/Developer/Perso` gets no start text, and the guidance once beside its first `.rs` Read (the transcript `context` row is the pass condition; the hook log's lack of `PostToolUse` lines is only a side note, since a mod that never ran leaves it empty too); `fleet-lsp --version` prints `fleet-lsp 0.7.1`.

## Decision log
- 2026-11-23 — done by hand, not relais: 5–10 jobs were queued for 3 host slots; the mod API work needed the types already in this session.
- 2026-11-23 — P0 answered: `hooks.json` holds `modules` and `hooks` together. `claude plugin validate` passes, and a headless session in a served repo ran the SessionStart command hook (hook log `SessionStart …: guided`) with the mod loaded.
- 2026-11-23 — the PATH shim is reached only through the session's `env` (`--settings '{"env":{"PATH":…}}'`): Claude Code builds a hook's PATH itself, so a shim first on the caller's PATH counted nothing.
- 2026-11-23 — the plugin-origin case (another plugin's `$.tool.call` Read) is not in `register.test.ts`: a test's `$.tool.call` is the engine's own (core origin), and the kit gives no way to raise one with a plugin origin. The guard rests on the types (`PluginTier` excludes `core`; origin is set by the host alone) and the canary.
- 2026-11-23 — `register.test.ts` answers `$` calls beneath the plugin as `{ value: … }`; a compaction needs at least one kept message (with `toolUses`).

## Implementation review
- implementation-review (opus): R1 approve-with-changes (0.7.1 bump missing, high; stale Verification bullet; help test) fixed; 2 deltas, approve.
- backend (opus): R1 3 (CAS proof, compaction-reset race, no mod log); CAS falsified, reset race fixed with a test; deliberate: no log line (canary). Deltas approve.
- lang:typescript, lang:rust, tui (opus): R1 approve-with-changes, all fixed; deliberate: `as never` in test replies (kit types not public), `FleetLspGuided` export (a contract needs one). Delta approve.
- unix (opus): approve (regex tolerance fixed anyway).

## Verification record
- `claude plugin validate plugins/fleet-lsp` → passed: hooks `tool.call{tool=Read|Edit|Write, file_path=?}`, `session.compact`; calls `$.env.get`, `$.fs.exists`, `$.session.id`, `$.state.get/set`; state `fleet-lsp.guided`.
- `tsc` (TypeScript 5.9, the types header's tsconfig, hooks/ and types/ with this build's `claude-code.d.ts`) → exit 0. Run at `nice -n 19` outside the slot queue after a 1800 s wait freed no slot.
- `claude plugin test plugins/fleet-lsp` → 11 pass (after review: + `markerPath` cases, + a note claimed during a compaction survives it — FALSIFY: an unconditional reset fails it): first served file guides once; `.md` never and keeps the note; subagent Read nothing; errored Read nothing then guides; SessionStart marker → nothing; precompute keeps, manual compaction brings it back; fresh marker after compaction → nothing; 3 parallel first Reads → 1; pattern accepts the 12 extensions and rejects 8 near misses. FALSIFY: dropping the `guided` check fails 2 tests.
- `src/hook.rs` unit tests: the mod's `GUIDANCE_LINES` equal `GUIDANCE.lines()`; its `SERVED` pattern spells exactly `SERVED`. FALSIFY: one changed word in register.ts fails the first.
- `tests/check_plugin.rs` → 4 pass: as-is passes; a PostToolUse entry, a missing module, a plugin.json version apart from Cargo.toml each exit 1 with only their own stderr line. `tests/hook.rs` → 7 pass (the `post-tool-use` path an older plugin copy runs).
- Headless, worktree plugin via `--plugin-dir`, installed 0.7.0 off for the run: from `~/Developer/Perso`, 6 Reads (4 served) → the note arrived once, as the `tool.call` hook's context (session 1578824f); from the worktree, the session-start note was present (the model quoted it).
- Cost: shim through the session env, 6 Reads (4 served, 2 not) → the binary started once, `hook session-start` (session 90b06d4e). 0.7.0 would have started a shell and the binary on each of the 6 calls as well.

- After release (#43, tag `v0.7.1`, release run 45193878855: four archives, tap published and install-tested; `brew upgrade` → `fleet-lsp 0.7.1`, `claude plugin update` → 0.7.1): canary, a headless session in `~/Developer/Perso` with the installed plugin (session c53d9a99), 6 Reads → one note, right after the first served file (`src/main.rs`; `README.md` before it got none); the shim through the session env counted one binary start, `hook session-start` → as expected.

<!-- panel: repos=fleet-lsp adds=lang:typescript reviewers=backend,lang:rust,lang:typescript,tui,unix,codex body-sha=a3e48daefbb0 -->
