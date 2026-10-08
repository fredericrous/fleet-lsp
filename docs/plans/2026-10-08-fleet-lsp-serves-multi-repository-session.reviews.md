# Full reviews: fleet-lsp serves a multi-repository session

**TUI (round 1), approve-with-changes, 34k tokens, 41 s.**
- (high) Refusals point to `doctor`, which is useless outside a repository → `doctor [PATH]`.
- (high) Reused refusal fixes say "start the session in…" → workspace texts.
- (medium) New texts not written out.
- (medium) `workspaceSymbol` scope invisible → session log and hook line.
- (medium) Log name collisions and doctor's newest log.
- (low) Log evictions.

**Unix (round 1), approve-with-changes, 34k, 38 s.**
- (high) Serial teardown 4×10 s → shared deadline.
- (high) Eviction and restart need non-blocking reaping.
- (medium) Inherited stderr mixes children → per-child pipe.
- (medium) No restart budget → 2 per root.
- (medium) Log name collisions.
- (low) doctor stdout and exit code.

**Backend (round 1), approve-with-changes, 38k, 63 s.**
- (high) Core's direct client replies need merging.
- (high) Exit and finish(1) kill the session.
- (medium) Server-request id renumber order and the pyright guard.
- (medium) Double didOpen, and notifications spawning children.
- (medium) RSS numbers.
- (low) Rollback.
- (low) Live check expectations.

**Rust (round 1), rework, 50k, 80 s.**
- (blocking) finish(1) on child death.
- (blocking) Server output bypasses the loop: replies leak, nowhere to renumber.
- (high) Inherited stderr.
- (high) Fake server cannot assert roots.
- (medium) Double didOpen.
- (medium) Shared output lane flood.

**Rust (round 2), approve-with-changes, 48k, 46 s.** All six round-1 findings resolved. New:
- (high) `fail_everything` leaks own ids.
- (high) Eviction and shutdown through Core end the session.
- (medium) Cross-slot id collision → `s<slot>:<orig>`.
- (low) Client-id→slot cancel map.

All applied.

**Backend (round 2), approve-with-changes, 42k, 51 s.** Remaining:
- (high) Id table cannot live in the `Copy` ServerFilter.
- (high) `OwnReply` instead of dropping.
- (low) Live check ordering.

All applied.

**Backend (re-bind), approve-with-changes, 38k, 35 s.**
- (blocking) The bypass skipped the initialize injections → `prepare_initialize` and `adopt_filter`.
- (low) 23 s worst case.

Both applied.

**Backend (final bind), approve-with-changes, 36k, 34 s.** Both resolved. Carried into implementation:
- (medium) Fixture flags and initialize logging.
- (low) Parse-failure, id and rootUri order before `prepare_initialize`.
- (low) JSON-equal params comparison.
