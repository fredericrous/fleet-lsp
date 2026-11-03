---
status: active
branch: feat/effect-tsgo-server
repos: [fleet-lsp]
adrs: [ADR-0056, ADR-0044]
---
# Plan: fleet-lsp serves TypeScript through effect-tsgo where a repository pins it

## Review panel

👉 **Decide:** none — approve if a Phase 0 probe deciding push vs pull is the right first step before fleet-lsp launches effect-tsgo.
📍 fleet-lsp · plan reviewed, nothing built · next: Phase 0 probe. Panel: backend, lang:rust, unix, tui (opus), codex (gpt, high).
**Changed by review:** Phase 0 probe gate (push vs pull, Loading trigger, harness changes); compiler identity and platform package verified; workspace-mode bridge and gate with freshness generations.
📄 Full reviews: [2026-11-03-effect-tsgo-server.reviews.md](2026-11-03-effect-tsgo-server.reviews.md)
**Verdicts:** round 1 four approve-with-changes; Codex rework, applied; backend approve-with-changes on the final body; 7 medium/low findings carried into implementation.

## Context

customer-vision now type-checks with TypeScript 7 through `@effect/tsgo` (customer-vision#34): CI runs `effect-tsgo tsc` with the Effect rules, tests and a few modules exempt through the tsconfig plugin's `overrides`. fleet-lsp 0.5.6, Claude Code's language server, always launches `typescript-language-server` on the repository's TypeScript 5 (`src/resolve.rs:997-1180`, `ADAPTER` at `:1185`). There the `@effect/language-service` 0.87.5 plugin ignores `overrides` and adds rules CI does not run: on customer-vision it flagged nine `strictEffectProvide` errors in `worker/gtm-writeback.test.ts` that CI exempts, plus `multipleEffectProvide`, which CI never reports. So the agent's diagnostics and CI disagree.

A probe of `node_modules/.bin/effect-tsgo tsc --lsp --stdio` on customer-vision (`scripts/lsp-probe.py`, 2026-11-03) showed:
- it answers go-to-definition correctly, ready about 2.4 s after start;
- its readiness signal is a `$/progress` begin titled `Loading` (message `Project 'tsconfig.json'`) then its end; it sends no tsserver "Using Typescript version" log line, so today's `Barrier::TypeScript` would never open (`src/gate.rs:~156-190`);
- it requests `workspace/configuration`, `client/registerCapability` and `window/workDoneProgress/create`, which fleet-lsp already answers or forwards (`src/core.rs:547-550`);
- on the exempt test file it publishes 0 diagnostics, where TypeScript 5 published 9.

`@effect/tsgo` 0.64.1 is a Node wrapper (`dist/effect-tsgo.cjs`, so `engines.node` applies) whose platform package (`@effect/tsgo-<os>-<cpu>`) bundles the TypeScript 7 builds it runs; nothing is downloaded, but the wrapper pin fixes a set of bundled compilers, not one compiler. Bare `tsc` selects a supported installed native TypeScript version, otherwise the bundled `latest`; compiler verification must account for that selection.

The person chose (2026-11-03): fleet-lsp runs `effect-tsgo` when a repository pins `@effect/tsgo`.

## Goal

In a TypeScript project whose lockfile pins `@effect/tsgo`, fleet-lsp launches the project's own `node_modules/.bin/effect-tsgo tsc --lsp --stdio` instead of `typescript-language-server`, verified the same way (lockfile pin equals the installed version, the bin exists, Node satisfies `engines`), with a readiness gate that fits tsgo. The agent then sees exactly CI's diagnostics. Projects without `@effect/tsgo` are unchanged.

## Non-goals

- No plain TypeScript 7 (`@typescript/native-preview`, `typescript@7`) server: only `@effect/tsgo`, the one a fleet repository uses. A later plan can add `tsgo` the same way.
- No change to how Rust, Go or Python are served.
- No change in customer-vision: it already pins `@effect/tsgo`.

## Code review

- fleet-lsp: backend, lang:rust, unix (tui left out: doctor's text layout is unchanged, only the version and pin strings differ)

## Behaviour

1. **Selection** (`src/resolve.rs`, `fn typescript`). After the lockfile is found (`:1003`), read `@effect/tsgo` with `lockfile_version` (`:1222`).
   - Pinned → the effect-tsgo path:
     - the installed `node_modules/@effect/tsgo/package.json` version must equal the pin, else refuse "stale node_modules" with the lockfile's install command (as for `typescript`, `:1036`);
     - `node_modules/.bin/effect-tsgo` must exist, else refuse with the install command;
     - the platform package that holds the compiler, `@effect/tsgo-<os>-<cpu>` (an optional dependency, so `--no-optional` or a lockfile from another OS can skip it), resolved beside the real path of `node_modules/@effect/tsgo` (pnpm keeps it under `.pnpm`), must exist at the pinned version, else refuse with the install command;
     - `node` must be on PATH (`node_on_path` `:1170`): the bin is a Node wrapper. `@effect/tsgo` 0.64.1 declares no `engines.node`, so no floor is checked; the Node refusal takes the package name as a parameter instead of the `ADAPTER` constant (`:1126`);
     - `typescript` and `typescript-language-server` are not required on this path. Resolve the effective compiler using the wrapper's selection rules, including its optional `typescript`/`@typescript/native` lookup. Verify any installed selector package against the applicable lockfile resolution; refuse an unverified or stale selector rather than reporting verified. Record the effective compiler version in doctor. Test absent selectors, TypeScript 5, supported native selectors, and stale selectors that change the selected compiler;
     - each refusal on this path names `@effect/tsgo` and sets `pin`, so doctor's `pin:` line shows why this path was taken: `@effect/tsgo is not installed` (missing package, bin or platform package; fix: the lockfile's install command), `stale node_modules: @effect/tsgo <installed>, pinned <pin>` (fix: the install command), `node is not on PATH` (fix: none — install Node);
     - `Spawn { program: .bin/effect-tsgo, args: ["tsc", "--lsp", "--stdio"], cwd: root }`, `env_remove` empty (the wrapper reads no variable that swaps its binary; checked in `dist/effect-tsgo.cjs`), `tsserver_path: None` (so `prepare_initialize`, `src/core.rs:658-660`, sends no `tsserver.path`);
     - `pin`: `"<lockfile> @effect/tsgo <v>"`; `version`: `"effect-tsgo <v> (node <n>)"`; `barrier`: `Barrier::Tsgo` only when the verified wrapper/compiler pair equals the measured pair `(0.64.1, 7.0.2)`, else `Barrier::None` with `narrowed: true`; add a test that another compiler bundled with wrapper 0.64.1 receives the narrowed promise, so doctor prints its `note:` line only for an unmeasured version (`:1136-1161`).
   - Not pinned → today's path, unchanged.
2. **Scoped names in `lockfile_version`.** Only the pnpm branch fails on a scoped name today (`src/resolve.rs:1240,1265`: unquoted `{name}:` and `  {name}@`); npm (`:1233`) and yarn (`:1280`) already match. Change pnpm to accept the quoted `'@effect/tsgo':` importer key and `'@effect/tsgo@0.64.1':` package key, and test all four formats.
3. **Gate** (`src/gate.rs`). A new `Barrier::Tsgo`: it opens when a `$/progress` token that began with title `Loading` ends. It never opens on a report from another server, and the existing ceiling still answers an error if it never comes. The relay already observes `$/progress` (`relay.rs:~480-507`), including when fleet-lsp owns progress. Phase 0 records what starts `Loading` (spawn or the first `didOpen`) and how many rounds a session sees; if loading waits for a `didOpen`, the gate opens on `initialized` instead and the readiness doc says so.
4. **Diagnostics** (decided by Phase 0). If effect-tsgo pushes `textDocument/publishDiagnostics` for opened files, they are forwarded as today. If it only answers pulls (it advertises `diagnosticProvider`), fleet-lsp sends `textDocument/diagnostic` after each `didOpen`/`didChange` it relays for that server and turns each answer into a `publishDiagnostics` notification to the client (`src/core.rs`, a per-server flag set by the effect-tsgo resolution), so the agent sees the same diagnostics either way. Bridge rules: pulls use a reserved id space that is never forwarded to the client and never answered by `fail_everything` (`src/core.rs:620-627`); one pull per URI at a time; associate each pull with the document lifetime and a diagnostic invalidation generation, not just its document version. Cross-file changes invalidate affected pending pulls even when their target document versions stay unchanged. Coalesce invalidations while a pull runs, discard obsolete responses, and issue the pending replacement when it completes. Closing/reopening a URI starts a new lifetime. Test A's pending pull followed by an edit to B, and close/reopen with a reused document version; `kind: "unchanged"` keeps the last published set; `didClose` publishes an empty set; because the server declares `interFileDependencies: true`, a change re-pulls every open document of that server. Each rule has a unit test in `src/core.rs`. Implement the bridge in both relay modes: carry its configuration into workspace slots, deliver internal diagnostic response bodies to the owning Core before workspace lifecycle reply dispatch, and keep their IDs distinct from initialize/shutdown IDs. Make workspace startup and restart replay observable by Core so both the selected gate trigger and initial diagnostic pulls run exactly once, preserving initialized/didOpen/request ordering. Add a workspace integration test with an effect-tsgo repository and a legacy TypeScript repository, checking initial diagnostics without a subsequent edit, restart replay, slot isolation, and no internal response leakage.
5. **Doctor** prints the effect-tsgo resolution with the existing layout: `typescript verified effect-tsgo 0.64.1 (node 24.x)`, `pin: pnpm-lock.yaml @effect/tsgo 0.64.1`, `server: node_modules/.bin/effect-tsgo`. JSON fields unchanged.
6. **Docs and release.** README's TypeScript row and the lockfile paragraph name the effect-tsgo path; `docs/readiness.md` gains the measured row (2.4 s, the `Loading` progress); CHANGELOG entry; `plugins/fleet-lsp/.claude-plugin/plugin.json` `--min-version` moves to the release. Released as 0.6.0 (new behaviour): the `tag-release` skill, brew formula verified.

## Phases

- [x] Phase 0 — extend the probe harness before production code: add separate initialize-only, didOpen-without-query, and query-triggered scenarios, plus explicit textDocument/diagnostic requests and diagnostic collection that does not end on a successful references response. Record which scenario first triggers Loading, correlate diagnostics with the requested URI, and require an explicit diagnostic report to establish an empty result. Run the extended `scripts/lsp-probe.py` on customer-vision with a planted `Date.now()` in `app/lib/crm/domain.ts` (reverted after) must show whether a `globalDate` diagnostic is pushed for the opened file, and that `worker/gtm-writeback.test.ts` gets none, pushed or pulled; records whether `Loading` starts at spawn or on `didOpen`, and its rounds. Its answer picks Behaviour 3's gate trigger and Behaviour 4's push-or-pull branch.
- [ ] Phase 1 — `lockfile_version` scoped names, the effect-tsgo selection path, the platform-package check, `Barrier::Tsgo`, the pull bridge if Phase 0 needs it, doctor strings; unit tests.
- [ ] Phase 2 — integration test, docs, CHANGELOG, plugin min-version; release 0.6.0; install it here (rollback: reinstall 0.5.6 through brew; no state to migrate); verify in Claude Code on customer-vision.

## Verification

- **Unit, resolve:** fixtures for pnpm (root importer and package keys, quoted scoped names), npm, yarn and bun with `@effect/tsgo` → `lockfile_version` returns 0.64.1; a project pinning it → verified, spawn `.bin/effect-tsgo tsc --lsp --stdio`, `tsserver_path` None, `Barrier::Tsgo`; another version → `Barrier::None`, `narrowed`; stale `node_modules`, missing bin, missing or stale platform package, no `node` → each refusal naming `@effect/tsgo`, with its fix and `pin`; a project without it → today's `typescript-language-server` resolution, byte-identical (existing tests stay green).
- **Unit, doctor:** `render_text` on an effect-tsgo resolution matches a golden: head `typescript verified effect-tsgo 0.64.1 (node 24.14.0)`, `pin:`, `server:`, no `note:`, every line at most 80 columns; on each refusal, `reason:` names `@effect/tsgo` and `fix:` is the install command or `none — install Node`.
- **Unit, gate:** a `Loading` begin then end opens `Barrier::Tsgo`; an end without its begin, or a progress with another title, does not; a typescript-language-server report never opens it.
- **Integration** (`tests/relay.rs`): a fake server flavour that emits the tsgo progress shape (and, if Phase 0 chose pull, answers `textDocument/diagnostic`, checked to arrive as `publishDiagnostics`), with a fake `node` in the fixture's `fakebin/` first on PATH (`tests/relay.rs:54-73`) so the test does not depend on CI's Node; `fleet-lsp serve typescript` on a fixture repo pinning `@effect/tsgo` launches it and answers a definition request once the gate opens.
- **Real server:** `fleet-lsp doctor ~/Developer/Perso/customer-vision` → `typescript verified effect-tsgo 0.64.1`; through Claude Code's LSP tool on customer-vision: a planted `Date.now()` in `app/lib/crm/domain.ts` shows a `globalDate` error, `worker/gtm-writeback.test.ts` shows no Effect diagnostic (TypeScript 5 showed 9), go-to-definition works; a `workspaceSymbol` sent before any `didOpen` is answered before the ceiling; the plant reverted. Diagnostics on lower-cased URIs (effect-tsgo lower-cases some, probe `:48`) must still attach to the file.
- **No orphan:** after a `shutdown` and after a forced retire on customer-vision, `pgrep -f tsgo` finds no process left from the session (the kill reaches only the Node wrapper, `src/relay.rs:336`); if one is left, the spawn starts the wrapper in its own process group and teardown signals the group.
- **A repository without `@effect/tsgo`** (duro-app or relais's TypeScript plugin, whichever is a TS project here): `fleet-lsp doctor` unchanged.
- `make check` (the repo's suite) passes; readiness re-measured with `scripts/lsp-probe.py` and recorded.

## Decision log

- 2026-11-03 — The person chose fleet-lsp running effect-tsgo where a repository pins `@effect/tsgo`, over fixing the 91 test findings or leaving the editor a superset of CI.

## Outcome

**Phase 0 (observed 2026-11-03, effect-tsgo 0.64.1 on customer-vision, `scripts/lsp-probe.py` extended with `--scenario`, `--pull`, `--diag-cap`, `--open-also`):**

| scenario | `Loading` progress | pushed | pulled (`textDocument/diagnostic`) |
|---|---|---|---|
| A: initialize only, 8 s | none | — | — |
| B: open `domain.ts` (planted `Date.now()`) and `worker/gtm-writeback.test.ts` | begin 0.38 s after `didOpen`, end 0.13 s later | only `tsconfig.json`, 0 diagnostics | — |
| C: as B, client advertises pull, pull 3 s after open | as B | as B | `domain.ts`: `full`, 1 (`377066`, globalDate); test file: `full`, 0 |
| D: as C without advertising pull | as B | as B | same as C |

- **Loading starts on `didOpen`**, not at spawn: the gate opens on `initialized` (Behaviour 3's fallback); `Loading` is one round per project.
- **A pull sent at `didOpen` waits for loading** (3 runs, pull 0 s after `didOpen`: answered 0.6–0.9 s later, `full`, the planted `globalDate`), so the bridge pulls at once and needs no retry.
- **Diagnostics are pull-only**: effect-tsgo pushes nothing for opened files, advertised or not, and answers pulls with the plugin's rules and `overrides` applied. Behaviour 4's pull bridge is built.


<!-- panel: repos=fleet-lsp adds= reviewers=backend,lang:rust,unix,tui,codex body-sha=3c689ace007f -->
