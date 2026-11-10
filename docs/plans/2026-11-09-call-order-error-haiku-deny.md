---
canonical: amont-agent:docs/plans/2026-11-09-call-order-error-haiku-deny.md
phases: [A]
status: active
---
Part of [Fix fleet-lsp's call-order error, deny symbol-grep-not-lsp for haiku workers](../../../amont-agent/docs/plans/2026-11-09-call-order-error-haiku-deny.md).

## Code review

- fleet-lsp: backend, lang:rust, unix (tui left out: one refusal sentence changes, no layout)

## Verification (Part A)
- The real `fleet-lsp serve python` binary, rebuilt from the committed source, driven with `initialize` then `workspace/symbol`: in a Rust-only repository the answer is `fleet-lsp: python: no python project found in <repo>; fix: none`; from a directory outside any repository it is `<dir> is in no git repository; fix: open a file inside a repository`. Before the change the same call answered `no repository chosen yet` (seen on a build with the fallback reverted).
- `tests/relay.rs`: the four new tests and the neighbouring refusal and gone-root tests pass. Dropping the fallback hunk fails the first two; dropping both clears fails the stale-reason test; the didOpen test has no FALSIFY by design.
- `plugins/fleet-lsp/hooks/code-intelligence.sh` prints 6 lines in session mode and one JSON line in `subagent` mode.

## Decision log
- 2026-11-10 — the saved warm-start failure is cleared whenever the recent list gains a root. The plan said it was never cleared; backend review showed the sweep can empty the list without the request path seeing a gone root, which replayed a stale "fix: none".
- 2026-11-10 — deliberate: the stale-reason test asserts the old text is absent, not that the generic `no repository chosen yet` text is present, so it could pass vacuously if a child's death were handled before the sweep. Two seats suggested the extra assertion; it is a one-line follow-up, deferred to avoid another tree change and review pass.
- 2026-11-10 — known limit: a reason the person can fix (for example an unpinned server) is still replayed to requests with no document until a `didOpen` or a new session.
- 2026-11-10 — the release step adds the `CHANGELOG.md` entry (`tag-release`); `decisions.pack` and `AGENTS.md` in the diff are the required fleet-pack refresh and generated-block regeneration.
- 2026-11-10 — the Part A relais run was blocked twice by contention (baseline at load 20-30, then an 1800 s admission wait with nothing launched), so Part A was done by hand.

## Implementation review
- implementation-review: round 1 approve-with-changes (three lows, fixed or noted); delta approve-with-changes, test strength deliberate (54k, 31 s; 33k, 54 s).
- backend: round 1 approve-with-changes (stale reason replayed after the sweep, fixed with a test); delta approve (55k, 32 s; 23k, 18 s).
- lang:rust: approve, no findings (48k, 37 s).
- unix: approve, no findings (63k, 37 s).
