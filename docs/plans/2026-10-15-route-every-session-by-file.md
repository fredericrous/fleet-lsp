---
status: done
branch: feat/route-every-session-by-file
repos: [fleet-lsp]
adrs: []
---

# fleet-lsp routes every session by file

## Review panel
👉 **Decide:** approve to reverse the 2026-10-08 non-goal "no change for a session inside a repository": route every session by file.
📍 fleet-lsp · implemented and verified live · next: release (person's call), then the 10-18 journal check. Panel: backend, lang:rust, tui, unix (sonnet-5-5).
**Changed by review:** single-root tests pinned via `start_args` + `env_remove`; rollback is an exported env var; workspace teardown twins and warm-start refusal tests.
📄 Full reviews: [2026-10-15-route-every-session-by-file.reviews.md](2026-10-15-route-every-session-by-file.reviews.md)
**Verdicts:** 4 approve-with-changes (round 1), backend approve-with-changes (round 2); 5 findings carried into implementation.

## Context

A session's server never learns about a worktree, or about any repository
other than its first. Claude Code starts one server per language with
`rootUri` = the cwd it has when the LSP first starts. When that directory is in
a git repository, `relay::serve` (`src/relay.rs:81`) runs **single-root
mode**: one child, pinned to that repository, for the whole session. Every
request about a file elsewhere goes to the wrong rust-analyzer, which answers
`unlinked-file` / empty results. The journal records those as successful
`lsp` calls, so agents conclude "LSP returns nothing" and go back to grep.

Measured on 2026-10-15 from `~/.local/state/fleet-lsp/*/` session headers:

| lang | single-root sessions | workspace sessions |
| --- | --- | --- |
| rust | 58 | 1 |
| typescript | 29 | 1 |
| go | 7 | 1 |
| python | 16 | 6 |

The session roots are mostly worktrees (`relais-wt-*`, `amont-agent-wt-*`,
even `…/crates/relais`). The person launches from `~/Developer/Perso` on
purpose (2026-10-08 decision log), but the agent's first `cd` before its
first LSP call decides the mode. This session reproduced it: launched in
`~/Developer/Perso`, the server started at `~/Developer/Perso/fleet-lsp`
(log `rust/20261015T082933Z-53412.log`). Then every call on
`fleet-lsp-wt-dead-root` came back unlinked or empty.

Workspace mode (`src/workspace.rs`) already does the right thing per file:
`root_of` → `resolve::locate` → one pinned child per project root, LRU cap,
death budget. It is simply not used when the session root is in a repo.

## Non-goals

- No cross-repository answers (unchanged): `references` and `workspace/symbol`
  still search one repository's server.
- No change to pinning (`toolchain.language-servers-pinned`) or to the
  readiness barriers.
- No change to the session-level stub. A newer plugin than the binary
  (`version_refusal`) still answers every request with the upgrade refusal
  through the existing single-root stub.
- No new memory policy. The cap stays `FLEET_LSP_MAX_CHILDREN`, default 4.

## Behaviour

1. **Mode at `initialize`** (`relay::serve`):
   - With a version refusal, the single-root stub runs, as today.
   - Else, `FLEET_LSP_MODE=single` with a session root in a git repo runs
     single-root mode, as today. This is the rollback switch, and the
     existing single-root tests set it.
   - Else (the default), workspace mode runs, whether or not the session
     root is in a repository.
2. **Warm start.** In workspace mode with a session root inside a project
   (`resolve::locate` → `Located::Project`), the shell starts that root's
   slot at `initialize` (`slot_for`). The session's first repository loads at
   once, as in single-root mode today. The root is also seeded into `recent`,
   so a URI-less `workspace/symbol` before any request goes there. When the
   session root is outside any project, nothing is pre-started (today's
   workspace behaviour).
3. **Recency follows reading.** `didOpen` touches `recent` for its document's
   root (`root_of`, no child started). `workspace/symbol` then searches the
   repository of the file the agent opened last, not only the last file it
   *queried*. This matches what the plugin note already promises.
4. **Header line.** The workspace header reads `workspace mode (default; FLEET_LSP_MODE=single restores one server)` whether or not the session root is in a repository (replacing "no git repository here", `relay.rs:87`). It names the session root and, when one was pre-started, `first project <root>`. A test asserts the text. Each LRU eviction and cold start is logged with its repository and slot (tui#3). `doctor` is unchanged.
5. **Plugin note** (`plugins/fleet-lsp/hooks/code-intelligence.sh:18`):
   "Each repository you query gets its own server (worktrees included);
   workspaceSymbol searches the repository of the file opened or used most
   recently." It stays ≤ 6 lines (check-plugin.sh).

## Files

- `src/relay.rs`: the mode choice (`serve`, near `:81`), and
  `FLEET_LSP_MODE` read like `ceiling_from_env` / `max_children_from_env`:
  valid values `single` and `workspace` (default); any other value is logged
  (`… is not single|workspace; using workspace`) and ignored, and `single`
  with no git repository at the session root is logged as falling back to
  workspace.
- `src/workspace.rs`:
  - `Settings` gains `session_root: PathBuf`.
  - `initialize` (`:298`) stores `self.init` first (`start` builds the child's initialize from it, `:629`), then pre-starts and seeds through the existing
    `root_of`-style locate (a directory variant beside `root_of`, `:474`, which locates on the directory itself, not its `parent()`), and
    `slot_for` (`:497`).
  - `document` (`:347`) touches `recent` on `didOpen`.
  - A failed warm start (`slot_for` → `Err`) is logged, and `initialize` is still answered. The cached `RootState::Refused` answers later requests. Test: `a_refused_session_repository_still_initializes`.
- `src/route.rs`: `Recent::touch` is reused as is.
- `tests/relay.rs`:
  - Every single-root test sets `FLEET_LSP_MODE=single` through one place, `Session::start_args` (tests/relay.rs:226), so their expectations stay byte-identical. That covers `start_rust`, the readiness, ceiling, configuration, cancel, teardown, overload and `doctor_*` tests (lines 361-1000).
  - `Session::start_in` does `env_remove("FLEET_LSP_MODE")`, so an exported rollback switch cannot turn the `Workspace` tests into single-root ones.
  - New tests, using the existing `Workspace` fixture, `answered_by` and
    `fake_env`:
    - `a_session_inside_a_repository_serves_a_worktree_of_it`: rootUri in
      repo A; `git worktree add` A-wt; a definition on A-wt's file is
      answered by A-wt's child; one on A's file by A's child. FALSIFY:
      choose single-root whenever the session root has a git root.
    - `the_session_repository_starts_at_initialize`: A's child logs
      `root …A` before any request, and a `symbol` sent first is answered by A. FALSIFY: drop the pre-start; drop the `recent` seed. The rootUri is a sub-directory of A.
    - `workspace_symbol_follows_the_last_opened_file`: open B after
      querying A, then symbol → B. FALSIFY: drop the `didOpen` touch.
    - Workspace twins of the teardown tests now pinned to single mode
      (`stdin_eof_while_held…`, `sigterm_while_held…`,
      `a_client_that_stops_reading…`): two live children, and every child
      pid is gone within 6 s. FALSIFY: skip the slot teardown.
- `plugins/fleet-lsp/hooks/code-intelligence.sh`, `README.md` (mode
  section), and `CHANGELOG.md`.
- `docs/plans/2026-10-08-…multi-repository-session.md`: one Decision-log
  line pointing at this plan, which reverses its first non-goal.

## Risks

- **Memory.** Single-root sessions held one rust-analyzer. Now a session that
  walks 4 worktrees holds 4 (~800 MiB each) until LRU. Bounded by the
  existing cap: 4 × 800 MiB = 3.2 GiB per language per session, ~9.6 GiB for 3 rust sessions (800 MiB to be measured in Verification). If the measured sum exceeds half of `sysctl hw.memsize`, lower `FLEET_LSP_MAX_CHILDREN` in this PR.
- **Capabilities.** Workspace mode answers `initialize` with the fixed set
  (`core::capabilities`) instead of the child's own. Claude Code's LSP tool
  uses only operations in that set, so this needs no change.
- **Cold start from reading.** After a `didOpen` in a repository with no child, a URI-less `workspace/symbol` starts that child (up to 185 s, `relay.rs:43`) and, at the cap, evicts a live one. The log line names the reason (`most recently opened`).
- **Death semantics.** A crashing child now retires its slot (3 deaths →
  refused) instead of ending the session for the plugin to restart. That is
  workspace mode's tested behaviour.

## Verification

- `make check` (fmt, clippy -D warnings, msrv, unit and relay tests). Every
  single-root test passes unchanged under `FLEET_LSP_MODE=single`, and the
  three new tests pass and fail when falsified.
- `make check`: the three new tests fail when each is falsified (FAIL output recorded), and pass with the change.
- Live, with `PATH=<worktree>/target/release:$PATH claude -p …` (the session header shows the branch version; the brew binary is untouched):
  - A headless `claude -p` from `~/Developer/Perso` whose first command is
    `cd relais`, then LSP `hover` on a `relais-wt-*` file and on a `relais`
    file. Each is answered by its own child, as shown in the per-child logs
    `<session>-<repo>-<slot>.log`. Record the rust-analyzer RSS of both.
  - Same with a session launched inside `fleet-lsp`, touching a fresh
    `git worktree add` of it: no `unlinked-file`, and `findReferences`
    non-empty.
- Before release: record the count of `unlinked-file`/empty `lsp` answers on worktree paths for 10-08..10-15, and the new header string.
- After release (person's call, `tag-release`): the 10-18 journal check
  should show `lsp` calls on worktree paths with non-empty answers, plus the
  new `answered <method>: …` log lines from #16.

## Rollback

`FLEET_LSP_MODE=single` exported in the environment Claude Code is launched from (plugin.json has no `env` key; its servers inherit Claude Code's environment), which restores today's behaviour without a reinstall. The README mode section gives the exact `export` line. Verified live: with it set, the session header says single-root. Or a marketplace `ref` to the previous tag plus
that binary first on PATH.

## Observed (2026-10-15, before push)

| check | input | expected | actual |
| --- | --- | --- | --- |
| make check | relais runs run-65ddfd20d22e7-48b6, run-65de00792c8cc-11f28; each commit's gate | green | green (25 checks at every commit) |
| falsify mode | `!single && !in_repo` in `relay::serve` | `a_session_inside_a_repository_serves_a_worktree_of_it` fails | FAIL at tests/relay.rs:1649 `answered_by(…, &wt)` |
| falsify warm start | drop `self.warm_start()` | `the_session_repository_starts_at_initialize` fails | FAIL at tests/relay.rs:1662 |
| falsify didOpen touch | drop `recent.touch` on didOpen | `workspace_symbol_follows_the_last_opened_file` fails | FAIL at tests/relay.rs:1683 `answered_by(…, &b)` |
| restored | the three tests | pass | pass |
| live, launched in ~/Developer/Perso, first `cd relais` | documentSymbol on relais and relais-wt-resource-admission | one child each | header `workspace mode (default; FLEET_LSP_MODE=single restores one server)`, `first project ~/Developer/Perso/relais`, slot 1 relais, slot 2 the worktree, both answered |
| live, discriminating | findReferences relais-wt-resource-admission procs.rs:30 | non-empty by default, empty with `FLEET_LSP_MODE=single` | 17 references vs "No references found" |
| live, launched inside fleet-lsp | findReferences on this worktree's `warm_start` | own child, non-empty | slot 2 `fleet-lsp-wt-route-by-file`, 2 references |
| rollback header | `FLEET_LSP_MODE=single` exported | no `workspace mode` | `serve rust: session root ~/Developer/Perso/relais; git root …; project root …` |
| memory | summed rust-analyzer RSS, all sessions, sampled every 3 s | under half of 32 GiB | peak 2.8 GiB; children 350–650 MiB; cap stays 4 |
| baseline | `unlinked-file`/empty answers 10-08..10-15 | a count | not measurable: the journal records `lsp`/`lsp-error`, not empty answers; baseline is the session table in Context (58/1 rust single/workspace) |

Not done here: the 10-18 post-release journal check (needs a release, the person's call).

## Decision log

- 2026-10-15: `relais.toml` lands on this branch: relais onboarding committed it when this plan's implementation was routed through relais (one check, `make check`).
- 2026-10-15: a `didOpen` in a refused or restart-exhausted root leaves `recent` unchanged (`root_is_closed`), so a URI-less `workspace/symbol` keeps going to a serving repository; tested by `workspace_symbol_ignores_a_didopen_in_a_refused_repository`.
- 2026-10-15: `mode_from_env` returns a `bool`, like its sibling `*_from_env` helpers — deliberate (two reviews noted it as low).

## Implementation review

approve-with-changes, round 1 (37k tokens, 22 s). Fixed: falsification FAIL output, live RSS and the fleet-lsp-launched worktree run recorded above; `relais.toml` and the refused-root refinement put in the Decision log. Deliberate: `mode_from_env` bool. No code changed after the review.

<!-- panel: repos=fleet-lsp reviewers=backend,lang:rust,tui,unix body-sha=b4853f08f5f9 -->
