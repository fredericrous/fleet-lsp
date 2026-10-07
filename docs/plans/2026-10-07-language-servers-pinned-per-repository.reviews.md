# Language servers pinned per repository — full reviews


**Round 1** (body 4394d979e90b) — all approve-with-changes
- backend (40k, 68 s): rule vs tools-pinned-with-it/one-pin scope; go1.22 + -modfile toolchain + vendor; no update path; no rollback; cold-start number; exact-name lockfile tests.
- architect (40k, 99 s): adapter removal place (brew vs npm) + `which` check; which go runs -modfile / silent download; homelab subsonic-auth; name both Go forms; upgrade cost.
- po (39k, 77 s): no post-release measurement / rollback trigger; ship TS first (split releases); inactive repos listed; unmeasured adapter → narrowed test; go-line check reason.
- lang:go (42k, 131 s): gopls v0.23.0 declares go 1.26.0 so the ≥ check always passed; -modfile root/vendor semantics and fix run inside tools/; go.work; offline -modfile fixture; `./...` reason.
- lang:typescript (35k, 41 s): TS 6.0.3 unmeasured; lockfile lookup by exact name; Node engines check; fake PATH adapter test; install before removing the global.
- lang:python (35k, 34 s): homelab's 3 Python services unpinned and not uv projects (uv add fails); rule wording vs what fleet-lsp checks.
- react (33k, 21 s): runtime-stage Dockerfile check; lockfile with CI npm major; npm pack --dry-run for published libs; unused-dependency gates.
- ui-design (27k, 11 s), ux-research (30k, 22 s), game-ux (27k, 12 s): literal refusal texts with a `fix:` each and tests; distinct reasons; gopls first-build feedback and threshold; `compatible` consumers; fleet-wide progress view; inactive-repo wording.

**Round 2** (body 3017e35dff9e) — architect (35k, 72 s) and backend (36k, 82 s), approve-with-changes: local Go vs the project go.mod's go line; gitignored operator copies in homelab; stale Phase cross-reference; `go mod init tools`; narrower rollback trigger + exact downgrade; one-pin waiver sentence. All applied.

**Backend on the final body** (5c9210e5f402) — approve (31k, 22 s). Implementation notes, no body edit: `GOTOOLCHAIN=local` likely ignores a higher `toolchain` line — compare only the `go` line unless Phase 6 shows otherwise, and record each repo's `toolchain` line next to `go version`; check the rollback asset name exists in the release; the refusal table's "both releases" heading means one release per row.

**All reviewers on the final body** (5c9210e5f402) — approve: backend, architect, lang:python, react, lang:typescript; approve-with-changes, none blocking: po, lang:go, ui-design, ux-research, game-ux. Implementation notes (applied in the code and the per-phase checks, not as body edits):
- Go: compare local Go with the project go.mod's `go` line only (`GOTOOLCHAIN=local` ignores a higher `toolchain` line) + a test where a higher `toolchain` line stays verified; detect workspace mode with `go env GOWORK` in the spawn's cwd/env (catches `GOWORK` and a go.work above the git root); run the offline fixture with `GOPROXY=off GOFLAGS=-mod=mod`; a project outside any git repository is refused (existing behaviour) with a test.
- TypeScript: parse only a leading `>=X[.Y.Z]` of `engines.node`, print other ranges in doctor without refusing; yarn.lock and bun.lock fixtures with both package names.
- Refusal table: write out the Python row in full (`fix: none — not a uv project (no [project] table)`); add `gopls build failed: <first stderr line>` → `fix: run go build in tools/ to see the full error`; heading reads TS rows ship in 0.2.0, Go rows in 0.3.0; inactive Go repos get "Go not pinned".
- Rollout: doctor loops print `n/N <repo> <verdict>` and a summary naming every non-zero repo (that summary is the Phase 4/7 acceptance); Go PRs are safe under 0.2.0 (the tools module is ignored until 0.3.0); a startup timeout counts as a rollback trigger like a refusal; if the TS 6.x live check changes the barrier key, re-run Phase 3's doctor loop; the rollback asset name is checked against the release before relying on it.
- Cold gopls build: if it exceeds 120 s, 0.3.0 is not released without the person; the README documents `fleet-lsp doctor` as the warm-up; in a session nothing shows the build (Claude Code surfaces no progress) — stated in the README.
