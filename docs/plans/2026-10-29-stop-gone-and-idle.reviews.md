# Reviews: fleet-lsp stops a server whose root is gone or that sits idle

## Full reviews (reference)

**Round 1, backend** (approve-with-changes, 42k, 90 s): high, `did*` notifications never restart an idle-stopped child (applied: restart on didOpen/didChange); medium, `begin_shutdown` name taken (`close_slot`); medium, no `Core::is_quiet()` (added); medium, restart cost unmeasured (measure + log restarts); low, no Idle during the client's shutdown (unit case).

**Round 1, lang:rust** (approve-with-changes, 37k, 102 s): high, fake server logs no `shutdown` (fixture gains it); high, tests started children with notifications (a request now); high, MAX_DEATHS check could pass if counted (3 idle cycles then a 4th request); medium, `Core::is_quiet`; medium, `exists()` swallows errors (`try_exists`, Err keeps the server); medium, sweep during client shutdown (gated); low, stale doc comments; low, `Instant` stops during macOS sleep (README note).

**Round 2, backend** (approve-with-changes, 31k, 43 s): high, edit-after-idle expected didOpen then didChange, which would apply an incremental edit twice (now one didOpen carrying the edit); medium, didClose must not restart, set cleanup on Err; low, HTML entities; low, numbering.

**Bind, backend** (approve-with-changes, 23k, 22 s): low, the didClose check proves nothing on A (carried: use root B).

**Final review, codex** (approve-with-changes, high effort): high, idle marker cleared on the reap of the idle-stopped child (now survives, generations protected); high, eviction sends `exit` without awaiting the `shutdown` reply (real handshake, bounded); high, `Starting` has no deadline, a notification-triggered restart could hang (init deadline, counted as a death). All applied.

**Bind after codex, backend** (approve-with-changes, 31k, 54 s): blocking, `flush_outboxes` closes a Closing child's stdin once the outbox is empty, before `exit` (fixed: close only after `exit`); low, init-timeout deaths never reset (carried: README note). **Final bind** (approve, 33k, 103 s; a resumed agent the hook does not count): no new findings. **Fresh bind, backend** (approve-with-changes, 31k, 49 s): medium, Changes item 2 lags Design (Closing shape, idle set, init deadline, stdin rule, fixture); medium, Verification lacks the handshake, missing-reply, init-deadline and generation tests. Both carried into implementation.
