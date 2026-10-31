---
status: done
branch: chore/relais-profile-trim
repos: [fleet-lsp]
canonical: decisions:docs/plans/2026-11-01-one-verdict-build.md
phases: [P1]
---
# One verdict per tree: build (pointer, P1)

The canonical plan lives in decisions at `docs/plans/2026-11-01-one-verdict-build.md`. This branch is phase **P1** for fleet-lsp: profile trims and `cache_baseline`.

## Verification record (P1)

- relais.toml profile, read with tomllib → expected setup `make toolchain`, commands `make deps`, `make lint`, `make test`, no msrv, `cache_baseline = true` → actual: matches.
- Makefile targets → expected `toolchain`, `deps`, `lint` and `test` all exist (`check: toolchain deps lint test msrv`) → actual: present; `deps` runs the local `check-no-deps.sh` and `check-plugin.sh`.
- `relais plan --task <noop contract> --json` → expected exit 0 → actual: exit 0, policy_hash `fd910d73…`.

## Implementation review

- Approve-with-changes, round 1.
- Already present: the verification record (input → expected → actual) is this pointer's "Verification record (P1)". The review brief carries only the canonical plan, so the reviewer could not see it.
- Deliberate: the fleet pack refresh is its own commit, the worktree-task start step (`aval add --check` returned 1), not part of P1's change.
- Deliberate: `[verification.profiles.default]` sits after its command arrays. TOML 1.0 allows it, and all three P1 repositories keep the same layout.
