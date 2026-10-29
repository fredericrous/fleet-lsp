---
status: active
branch: feat/stop-gone-and-idle
repos: [fleet-lsp]
adrs: []
---
# Plan: fleet-lsp stops a server whose root is gone or that sits idle

## Review panel

📄 Full reviews: [2026-10-29-stop-gone-and-idle.reviews.md](2026-10-29-stop-gone-and-idle.reviews.md)
👉 **Decide:** none. Approve if stopping a server ~5 s after its root is deleted, and gracefully after 30 min idle (restarted on the next request or edit), is the fix you want, released as fleet-lsp 0.5.5.
📍 fleet-lsp · reviewed, nothing built · next: worktree from origin/main, land this plan, then one relais run. Panel: backend (opus-5-5), lang:rust (opus-5-5), codex (gpt).
**Changed by review:** an idle-stopped root restarts on `didOpen`/`didChange` (not `didClose`), its marker survives the old child's reap; a real shutdown handshake (`exit` after the `shutdown` reply, stdin kept open until then, bounded); `Starting` gets an init deadline; `try_exists` keeps a server on a stat error.
**Verdicts:** backend approve-with-changes twice then approve; lang:rust approve-with-changes; codex approve-with-changes (3 high, applied). 4 carried into implementation: the `didClose` check runs on a separate root B; README notes that an init timeout counts toward `MAX_DEATHS`, never reset; the relais contract builds from Design (the full `Phase::Closing { why, shutdown_id, stage, deadline }`, `idle_stopped`, init deadline, stdin rule, fake server's delayed/omitted reply), not the shorter Changes list; and its acceptance adds the handshake, missing-reply, init-deadline and overlapping-generation tests.

## Context

On 2026-10-29 the machine ran out of memory: swap 20.4 of 21.5 GB, about 17 MB of RAM free, load near 900. The largest avoidable consumers were 12 rust-analyzers started by fleet-lsp for worktrees that had already been deleted. Stopping them freed about 3 GB. dotfiles#52 taught `worktree-sweep` to stop such servers, but a worktree deleted outside the sweep (relais retiring a run worktree, `rm -rf`, `git worktree remove` by hand) keeps its server until a sweep runs. The person asked for the real fix in fleet-lsp.

Today (v0.5.4, #20) fleet-lsp notices a deleted root only when a request arrives for a file under it (`slot_for`, `workspace.rs:586`) or when the child dies (`ChildDead`, `:948`). A root that is deleted and never asked about again keeps its child for the rest of the session. Sessions last days (`fleet-lsp serve` processes from 2026-10-18 were still running on 10-29), and each holds up to `DEFAULT_MAX_CHILDREN` = 4 children per language, rust-analyzer "over 1 GB per repository" (`workspace.rs:33`).

The person also chose (2026-10-29) to stop servers that sit idle while their worktree still exists, after 30 minutes, configurable, a graceful LSP stop that is not counted as a crash.

Intended outcome: a child whose root is gone is stopped within a few seconds without any request; a child with no request for 30 minutes is shut down gracefully and started again, with its open documents, on the next request.

## Design

1. **Where.** `Workspace::housekeeping` (`workspace.rs:1003`) already runs after every iteration of the event loop (`recv_timeout(TICK)`, 200 ms). Both checks go there; no new thread, no new event.

2. **Gone roots.** At most every `ROOT_CHECK_EVERY` = 5 s (a `last_root_check: Instant` on `Workspace`; one `exists()` per served root, bounded by the cap, and throttled so a slow filesystem stat cannot stall the loop every tick), each `(root, slot)` in `serving` whose `root.try_exists()` is `Ok(false)` is retired (an `Err` is logged and the server kept: `exists()` would read a transient EACCES/EIO as gone) with the existing `retire(slot, "its project root no longer exists")` (`:964`), and the root is forgotten from `recent` (`self.recent.forget(&root)`, as the URI-less request path does at `:351`). `retire` already answers everything held or in flight, kills and reaps the child, does not touch `deaths`, and logs `retired <repo> (slot N): <why>`. A later request for a file under that root keeps getting the existing `root_gone` refusal.

3. **Idle children.** `Slot` gains `last_used: Instant`, set at start and refreshed whenever a client request or notification is routed to that slot: in `slot_for` (`:585`, the one route every request takes to a slot) and in `document()` (`:394`, notifications for open files), never by the child's own traffic. The setting is parsed beside `FLEET_LSP_MAX_CHILDREN` (`:109`). A `Live` slot whose `now - last_used >= settings.idle`, with nothing held or in flight in its `Core`, is shut down gracefully:
   - extract `close_slot(slot, why)` from `make_room_for` and reuse it for eviction and idle stop, but correct the shutdown handshake: remove from `serving`, drain the core, send an own `shutdown` request, and retain its ID. Send `exit` exactly once after that request's response. Bound the shutdown-response wait by `SHUTDOWN_WAIT` and the subsequent exit wait by `TEARDOWN`; force termination on timeout and log the forced outcome;
   - `Phase::Closing` carries its reason, pending shutdown ID and current stage deadline; `own_reply` advances the matching slot from waiting for shutdown to waiting for exit. `flush_outboxes` (`workspace.rs:1061-1064`) today closes a `Closing` child's stdin as soon as its outbox is empty, which would cut stdin before `exit` is sent: close it only in the waiting-for-exit stage, after `exit` is flushed, and assert in the delayed-response test that the child sees no EOF before `exit`. Update the doc comments at `workspace.rs:67` and `core.rs:264` and retain reason-specific reap messages. Extend the fake server to delay or omit shutdown responses: assert that no `exit` arrives before the delayed response, and that missing responses or ignored exits still cause bounded termination without spending the crash budget;
   - not a death (eviction is the precedent; `retire` does not touch `deaths`);
   - the next request starts a new child through `slot_for`, and `child_initialized` (`:853`) replays the root's open documents from `Workspace.documents`, which a stop never touches.
   - the root goes into `idle_stopped: HashSet<PathBuf>`; a `didOpen` or `didChange` for such a root (`document()`, `:394`, which today never starts a child, `:391-393`) starts it again through `slot_for`, so diagnostics for open, edited files resume without waiting for a request; a `didClose` only updates the store (starting a 1 GB server to close a file is waste). The idle marker survives normal retirement of the idle-stopped child. Remove it when a new child successfully starts, when `slot_for` returns `Err` (root gone, refused, restarts spent), or when the root is confirmed gone; on failure the notification stays in the store only. Retirement of an older closing slot must not clear a newer generation's idle marker. Test notification-only restart after the old slot's retirement is confirmed, and test overlapping generations so an older slot's late retirement cannot erase the newer idle state.
   A `Starting` child is never idle-stopped (what waits for its `initialize` would have nowhere to go, as in eviction). A child with requests in flight is not idle by definition.

4. **Decision as a calculation.** Which slots to stop is a pure function, `sweep(now, slots: &[SlotView], settings) -> Vec<(u64, Stop)>` with `Stop::RootGone | Stop::Idle`, where `SlotView` carries `root_exists: Option<bool>` (None when this tick does not check), `last_used`, phase and whether its core is empty. `housekeeping` gathers the facts (the stats, `Instant::now()`) and applies the result, and runs the sweep only while `self.shutdown.is_none() && !self.shutdown_seen`. Unit tests drive it with constructed `Instant`s; no sleeping (`effects.no-ambient-access`, `purity.calculations-from-actions`).

5. **Restart cost and initialization bound.** The existing request ceiling expires Core-held requests; it does not bound `Phase::Starting`. Give `Starting` an explicit initialization deadline based on `settings.ceiling`, enforced by housekeeping even when a didOpen/didChange triggered the restart without a request. On expiry, log initialization timeout, account for a failed start through the existing death budget, fail pending work through `retire`, and reap the child. Ignore late replies to retired slots. Test a fake child that consumes initialize but never answers, both after request-triggered and notification-triggered idle restarts; verify bounded retirement and exactly one failure per pending request. Record rust-analyzer's measured cold start on the largest worktree beside the ceiling, and log `restarted <repo> after idle stop` so thrashing can be counted in session logs.

6. **Setting.** `FLEET_LSP_IDLE_SECONDS`, default 1800 (30 min); `0` turns idle stops off. Parsed beside `FLEET_LSP_MAX_CHILDREN` with the same rule for a bad value (a logged note, the default kept). Seconds, not minutes, so the integration test can use a short value and a person can still write 1800. `ROOT_CHECK_EVERY` is a constant.

7. **Out of scope.** `FLEET_LSP_MODE=single` (`relay.rs`, one child per process, an opt-out) keeps today's behaviour; its child's root is the process's own project. Stopping a whole `fleet-lsp serve` process is the plugin host's job.

## Changes

1. Branch `feat/stop-gone-and-idle` from `origin/main` in a worktree; land this plan as the first commit (`docs/plans/2026-10-29-stop-gone-and-idle.md`, reviews in `.reviews.md`).
2. `src/workspace.rs`: `last_used` on `Slot` and its refresh points; `Phase::Closing { since, why }`; `close_slot(slot, why)` extracted from `make_room_for`; `src/core.rs`: `Core::is_quiet()` (held, inflight and config_requests empty) with a unit test; the pure `sweep` and its application in `housekeeping` with `last_root_check`; `FLEET_LSP_IDLE_SECONDS` in the settings. Through relais, one bounded run (`task_type: feature`, write scope `src/**`, `tests/**`, `README.md`, `CHANGELOG.md`).
3. Tests (same run): unit tests of `sweep`; integration tests in `tests/relay.rs` with the fake server (see Verification).
4. `README.md` Limits section: the two stops and `FLEET_LSP_IDLE_SECONDS` (idle time counts while the machine is awake: `Instant` stops during sleep on macOS); `CHANGELOG.md` `## Unreleased` bullets.
5. Implementation review, PR, merge on green; release fleet-lsp 0.5.5 (`chore(release)` PR, tag, `scripts/bump-tap.py` for the Homebrew tap), `brew upgrade`. Running sessions keep their old `fleet-lsp serve` until they restart; new sessions get it.

## Verification

- **Unit (`sweep`):** a gone root is stopped as `RootGone` whatever its idleness; a root that exists and was used 29 min ago is kept, at 30 min is `Idle`; a `Starting` slot and a slot with requests in flight are never `Idle`; `idle = 0` never yields `Idle`; no `Idle` while the client's `shutdown` is under way (`shutdown_seen`); `root_exists: None` (not this tick) never yields `RootGone`.
- **Integration, gone root** (`tests/relay.rs`, fake server, real binary): open a file under root A and send one request for it so its child starts (a notification never starts a child, `workspace.rs:392`); `fs::remove_dir_all(A)`; with no further request, within 10 s the child's pid is gone (`pids(events.log)` + `gone_within`) and the session log has `retired … its project root no longer exists`; a later request for a file under A gets the `root_gone` refusal. Restoring A (`fs::rename` back, as the existing deleted-root test does) and asking again starts a child: the stop was not counted toward `MAX_DEATHS`.
- **Integration, idle** (`FLEET_LSP_IDLE_SECONDS=2`): open a document under A and send one request so its child starts, then wait past 2 s with no request: the child receives `shutdown` then `exit` (fake server's event log; `tests/fixtures/fake_server.py` gains `event("shutdown")` in its `shutdown` branch) and exits; the session log names the idle stop. A new request for A starts a second child (`count_kind(&a, "root") == 2`), which receives the replayed `didOpen` for the open document, and answers. Repeated for `MAX_DEATHS` (3) idle cycles with `FLEET_LSP_IDLE_SECONDS=1`, a 4th request is still answered: idle stops are not deaths.
- **Integration, edit after idle stop:** after the idle stop above, send only a `didChange` for the open document under A: within 5 s a new child starts (`count_kind(&a, "root") == 2`) and receives exactly one `didOpen` for the document whose text carries the edit (the store folds the change before routing, `workspace.rs:399`, and a `Starting` slot is not sent it, `:412-420`), and no separate `didChange`: forwarding it too would apply an incremental edit twice. A `didClose` sent after an idle stop starts no child (`count_kind(&a, "root")` unchanged after 5 s).
- **Not idle while busy:** a request held in the core past the idle time does not trigger a stop (unit test on `sweep` with a non-empty core).
- **Gates:** `make check` (fmt, clippy `-D warnings`, tests, `scripts/check-no-deps.sh`) and CI green.
- **Live, after `brew upgrade`:** in a fresh session, query a throwaway worktree, delete it, and confirm with `ps`/`lsof` that its rust-analyzer is gone within 10 s and `worktree-sweep --lsp` finds nothing.

<!-- panel: repos=fleet-lsp reviewers=backend,lang:rust,codex body-sha=3ae75be02d75 -->
