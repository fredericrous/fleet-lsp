# fleet-lsp: pinned, ready language servers — full reviews


**Round 1** (body 102217e8a88b)
- backend — approve-with-changes (32k, 46 s): [blocking] undeclared ops/repos; [high] FIFO deadlock on workspace/configuration/shutdown; [high] resolve from file dir before any file; child crash; no numbers / raw relay; stderr-only logs; PATH vs ADR-0019; uncheckable "same results".
- lang:rust — approve-with-changes (37k, 65 s): [high] FIFO holds replies/shutdown; [high] aval Json re-serialisation alters frames; fake-server [[bin]] ships; RUSTUP_TOOLCHAIN + cwd; heuristic gate unverified cold; threads/timing; rust-version.
- tui — approve-with-changes (30k, 32 s): gate invisible; doctor > 80 cols; refusals without commands; python "file's dir"; doctor contract; serve on TTY.
- unix — approve-with-changes (31k, 32 s): held shutdown + orphan child; stdout discipline + SIGPIPE; exit codes; override precedence; serve on TTY; PATH deviation.

**Round 2** (body c1081a1b76f7)
- backend — approve-with-changes (34k, 51 s): queued-notification release rule; shared log interleaving; unbounded .venv walk; ceiling/RSS pass limits.
- lang:rust — approve-with-changes (33k, 41 s): top-level-only scanner; expected child exit after `exit`; bounded wait w/o wait_timeout; log O_APPEND + pid.
- tui — approve-with-changes (32k, 35 s): 80 cols with long roots; fix lines for every refusal; log attribution/rotation; ceiling error names log; TTY hint width.
- unix — approve-with-changes (32k, 29 s): SIGTERM teardown; serve EPIPE; ceiling message uses value in effect; child-exit wording.
- platform (round 1) — approve-with-changes (42k, 54 s): plugin must follow the tag, not main; two-commit dotfiles switch; separate tap deploy key; verify-brew for v0.1.0; 4 targets; log cap.

**Backend deltas**
- body 59011b2ad180 — approve-with-changes (35k, 43 s): release branch as the only pin path; vacuous doctor gate; `${pipestatus[1]}`; serve exit wording; requestTimeout vs ceiling; reason text.
- body d32b3bf6b7e8 — approve-with-changes (34k, 29 s): `doctor` cannot see the manifest env → `serve` logs the ceiling warning (now in Behaviour).

**Person's review** (body d32b3bf6b7e8) — request changes: false-ready heuristic; RA `health` ignored; installed ≠ pinned (python/TS/rust channel, adapter fallback); blocking writes vs ceiling/teardown and unbounded queues; git root vs project root makes fixes ineffective; stub capabilities/lifecycle; plugin/binary compatibility + Claude 2.1.288 minimum; raw-spelling ids; O_APPEND logging.

**Deltas after the person's review**
- body 9af46b9931a8 — backend approve-with-changes (42k, 93 s): [blocking] uv workspace member root; [high] which pyright binary is run (`PYRIGHT_PYTHON_FORCE_VERSION`); [high] shared `sync_channel` could falsely declare a busy server hung; memory bound unbounded by frame size; escape example lost. All applied.
- body 9f07a4b3cb19 — backend approve-with-changes (39k, 42 s): serverStatus vs output bypass and the loop's own replies; 202 MiB bound; go by absolute path; empty pyright-python cache; "ready" defined. All applied.
- body 3f0368b55cb4 — backend **approve** (39k, 34 s). Implementation notes, no body edit: charge a frame's bytes to its queue until its write completes (else writers add 2 × 32 MiB); allocate bodies at exact `Content-Length`, discard oversize server bodies in chunks, scan an oversize client request's id in chunks; decide whether the 32 MiB limit counts headers, and unit-test 32 MiB passes / 32 MiB + 1 refused.
