---
status: active
branch: feat/language-servers-pinned
repos: [fleet-lsp, decisions, agent-console, application-landscape, customer-vision, duro-app, duro-design-system, duro-lexical-multi, governance-ts, grid, kb-vision, social-planner, ticket-vision, website-builder, vault-transit-unseal-operator, authelia-oidc-operator, homelab-preview-operator, ddns-updater-operator, duro-operator, cluster-vision, amont-pack-homelab, homelab]
adrs: [ADR-0019, ADR-0020, ADR-0012]
---
# Language servers pinned per repository

## Review panel

👉 **Decide:** none now — approve if pinning per repo is worth ~20 small PRs plus fleet-lsp 0.2.0 (TS) and 0.3.0 (Go); each release is asked separately.
📍 fleet-lsp, decisions, 12 TS + 8 Go repos · plan only · next: Phase 1, fleet-lsp 0.2.0 TypeScript. Panel: backend, architect, po, lang go/typescript/python, react, ui-design, ux-research, game-ux.
**Changed by review:** split into TS then Go releases; gopls built by local Go under `GOTOOLCHAIN=local` (stated deviation), vendor/go.work/ignored roots refused; literal refusal texts, rollback and upgrade path.
**Verdicts:** round 1: 10 approve-with-changes; round 2 (architect, backend): approve-with-changes; final body: backend, architect, python, react, typescript approve; po, go, ui-design, ux-research, game-ux approve-with-changes — non-blocking, carried as implementation notes (Full reviews).
📄 Full reviews: [2026-10-07-language-servers-pinned-per-repository.reviews.md](2026-10-07-language-servers-pinned-per-repository.reviews.md)

## Goal

fleet-lsp 0.1.0 verifies the pinned server for Rust (rust-toolchain.toml) and
Python (`pyright==` + uv.lock), but two pieces still come from the machine's
PATH, recorded as deviations `holds-until:` the repositories pin them: the
TypeScript **adapter** (`typescript-language-server`, global npm 6.0.1) and
**gopls** (`~/go/bin`, accepted only as `compatible`). This plan makes every
active TypeScript and Go repository pin its language server, makes fleet-lsp
run only those pins, removes the global copies, and writes the rule into the
fleet corpus. TypeScript ships first (0.2.0), Go second (0.3.0), so each half
lands, and can be rolled back, on its own.

## Non-goals

- Moving other Go tools (controller-gen, golangci-lint, setup-envtest) into
  the tools module.
- Inactive repositories (last commit before 2026-06-01). After the globals
  go they get a refusal with the fix, i.e. no LSP until pinned:
  TypeScript — adequate-guide-react, ap-webcomponents, fredericrous.com,
  frontninja2, poc-gridstack, receipts, thomasgrougi.com, web, and the
  2020–2021 Bangle/Catatonia/jellyfish repos; Go — none active outside the
  list. Accepted by approving this plan.
- homelab's Python services (`services/rag-api`, `services/websearch`,
  `services/playbook-scorer`): not uv projects (requirements.txt, or a
  pyproject.toml with no `[project]` table), so they stay unpinned; fleet-lsp
  refuses there with `fix: none — not a uv project (no [project] table)`
  instead of a `uv add` that cannot work.
- Changing how `typescript` itself is declared (carets, locked exactly in
  each lockfile — fleet-lsp verifies the lockfile).
- Automated bumps (no renovate/dependabot in the fleet): see Decision log.

## Behaviour

### The rule (decisions corpus)

A principle adopting ADR-0019 in `docs/principles/toolchain.md`, with its
scope stated: a language server is not a check-target tool (so
`toolchain.tools-pinned-with-it` does not reach it), but it is pinned in
the repository all the same — never the machine's PATH. Nor does
`toolchain.one-pin-read-by-both` apply: no check target or CI runs a
language server; the pin is read by fleet-lsp alone.
- TypeScript: an exact `typescript-language-server` devDependency at the
  workspace root;
- Go: `tool golang.org/x/tools/gopls` in a separate `tools/go.mod` (own
  `go.sum`) — the default — or in the module's go.mod;
- Python: an exact `pyright==` requirement in pyproject.toml, matched by
  uv.lock when present.
No version numbers in the text. Upgrades: a new adapter or gopls version is
measured with fleet-lsp's `scripts/lsp-probe.py` first, fleet-lsp's measured
constant bumped, then the repositories in one batch.

### fleet-lsp 0.2.0 — TypeScript

- Adapter = `<project root>/node_modules/.bin/typescript-language-server`,
  never PATH. Pin = the lockfile entry named exactly
  `typescript-language-server` (`lockfile_typescript` becomes
  `lockfile_version(kind, text, name)`, exact-name lookup, root importer
  first). Installed = `node_modules/typescript-language-server/package.json`.
- Node: the `.bin` shim runs PATH `node`; fleet-lsp checks `node --version`
  against the adapter's `engines.node` and refuses if lower; `doctor` prints
  the Node version.
- Barrier: still keyed to the measured adapter (6.0.1); TypeScript 6.x is
  checked live in Phase 1 (agent-console, social-planner) — if the
  selection report or answers differ, the measured key becomes (adapter,
  TypeScript major).
- 0.1.0 keeps working on pinned repositories (it ignores the devDependency
  and still uses the PATH adapter), so repositories can be pinned first.

### fleet-lsp 0.3.0 — Go

- Pin = `tool golang.org/x/tools/gopls` in the module's go.mod, or in the
  nearest `tools/go.mod` searched from the project root up to the git root
  (homelab's nested modules share one at the repository root). A project
  root that git ignores (homelab's gitignored sibling copies of the
  operators) is refused, so a stale copy is never served as `verified`.
- Run as `<verified go> tool -modfile=<absolute tools/go.mod> gopls` with
  cwd = the project root and `GOTOOLCHAIN=local` (no silent toolchain
  download). gopls is therefore built by the local Go, which must be ≥ the
  tools module's `go` line (gopls v0.23.0 requires go 1.26.0) and ≥ the
  project go.mod's `go`/`toolchain` line (`GOTOOLCHAIN=local` also governs
  gopls's own `go list` on the workspace) — else refused.
  This is a stated deviation from build.toolchain-source: gopls's toolchain
  is the machine's Go, not the module's ADR-0019 pin; `doctor` prints the
  Go that builds it.
- Refused: a `vendor/` directory at the project root (`-modfile` would read
  it), or a `go.work` in scope (`-modfile` errors in workspace mode — such a
  repository uses the go.mod `tool` form).
- The `compatible` verdict and the PATH gopls go; `verdict: "compatible"`
  leaves `--json` (fleet consumers grepped first; CHANGELOG lists the new
  verdict set — a 0.x minor under ADR-0012).
- First run builds gopls (cached after): `doctor` prints `building gopls
  (first run only)` and warms the cache; a build failure is its own refusal.
  The plugin's `startupTimeout` is set from Phase 5's measurement:
  ≥ 2 × (cold build + 7.7 s ready).

### Refusal texts (both releases; one unit test each asserts the text and its `fix:`)

| case | reason | fix |
|---|---|---|
| adapter not in lockfile | `typescript-language-server is not pinned` | `npm install -D -E typescript-language-server@6.0.1` (pnpm: `pnpm add -D -E -w typescript-language-server@6.0.1`) |
| adapter installed ≠ locked | `stale node_modules: typescript-language-server <i>, pinned <p>` | the lockfile's install command |
| Node too old | `node <v> is older than typescript-language-server needs (<engines>)` | `fix: none — upgrade Node` |
| Go not pinned | `gopls is not pinned` | `mkdir -p tools && cd tools && go mod init tools && go get -tool golang.org/x/tools/gopls@v0.23.0` |
| local Go too old | `go <v> is older than tools/go.mod's go <w>` (or `… than go.mod's go <w>`) | `fix: none — upgrade Go` |
| ignored project root | `<dir> is ignored by git` | `fix: none — open the real repository` |
| vendor / go.work | `-modfile cannot run with vendor/ (or go.work)` | `fix: none — pin gopls in go.mod instead` |
| Python not a uv project | `not a uv project (no [project] table)` | `fix: none — …` |

### Repositories

- **TypeScript** (12: agent-console, application-landscape, customer-vision,
  duro-app, duro-design-system, duro-lexical-multi, governance-ts, grid,
  kb-vision, social-planner, ticket-vision, website-builder): the exact
  devDependency at the workspace root (pnpm `-w` for duro-design-system,
  website-builder); lockfile regenerated with CI's Node/npm major, then `npm
  ci` (or the repo's install) run locally; lint and dependency gates run
  (allowlist entry where an unused-dependency check flags it); for the
  published libraries (duro-design-system, duro-lexical-multi,
  governance-ts, grid) `npm pack --dry-run` file lists unchanged; for repos
  with a Dockerfile, the runtime stage installs `--omit=dev`/copies only
  production deps (checked by grep). The dependency argument goes in each PR.
- **Go** (8: vault-transit-unseal-operator, authelia-oidc-operator,
  homelab-preview-operator, ddns-updater-operator, duro-operator,
  cluster-vision, amont-pack-homelab, homelab): `tools/go.mod` + `tools/go.sum`
  created inside `tools/` (the fix command above). `tools/` has its own
  go.mod, so it is a separate module and stays out of the parent's `./...`.

## Phases

- [x] Phase 1 — fleet-lsp 0.2.0 TypeScript (branch): adapter from
      node_modules, exact-name lockfile lookup, Node check, refusal texts,
      README/CHANGELOG; live checks incl. TypeScript 6.0.3.
- [x] Phase 2 — the rule in `decisions` (one PR).
- [x] Phase 3 — 12 TypeScript PRs (`aval add` refresh, devDependency,
      checks above, doctor 0.2.0-local exit 0 `verified (adapter 6.0.1)`,
      CI green).
- [ ] 🧑 decision: release fleet-lsp 0.2.0.
- [ ] Phase 4 — release 0.2.0, `brew upgrade`; `npm rm -g
      typescript-language-server`; `which typescript-language-server`
      must find nothing.
- [ ] Phase 5 — fleet-lsp 0.3.0 Go (branch): tools-module discovery,
      `GOTOOLCHAIN=local` spawn, refusals, `compatible` removed; measure the
      cold gopls build and set `startupTimeout`.
- [ ] Phase 6 — 8 Go PRs (`aval add` refresh, tools module, doctor
      0.3.0-local exit 0 `verified`, CI green).
- [ ] 🧑 decision: release fleet-lsp 0.3.0.
- [ ] Phase 7 — release 0.3.0, `brew upgrade`; remove `~/go/bin/gopls`;
      `which gopls` must find nothing.

## Decision log

- 2026-10-07 — Scope TypeScript + Go (person's choice); gopls in a separate
  `tools/go.mod` per module, not the product go.mod (person's choice).
- 2026-10-07 — Review round 1: split into 0.2.0 (TS) and 0.3.0 (Go);
  `GOTOOLCHAIN=local` and the local-Go deviation stated instead of a
  `go`-line check that always passed; vendor/go.work refused; literal
  refusal texts; Node check; homelab Python services excluded as non-uv;
  rollback and upgrade path written down.
- 2026-10-07 — Upgrades: measure with `scripts/lsp-probe.py`, bump the
  measured constant in fleet-lsp, then one batch of PRs.
- Rollback (each release): reinstall the global copy
  (`npm i -g -E typescript-language-server@6.0.1` /
  `go install golang.org/x/tools/gopls@v0.23.0`) and the previous fleet-lsp:
  `gh release download v<prev> -R fredericrous/fleet-lsp -p
  'fleet-lsp-<prev>-x86_64-apple-darwin.tar.gz'`, extract, and put its
  binary first on PATH (the tap carries only the latest). Trigger: a
  `refused` in a session log under `~/.local/state/fleet-lsp/` within 7
  days, in a repository `doctor` verified on release day, other than
  `stale node_modules` (which the person fixes with an install).

## Verification

- Phase 1 unit: adapter pinned+installed → verified; not in lockfile →
  refused + fix; installed ≠ locked → refused; pnpm root importer and
  package-lock v3 with both `typescript` and `typescript-language-server`
  (exact names, no prefix match); Node below engines → refused; adapter
  6.0.2 → verified + narrowed. Integration: a fake PATH adapter with another
  version is never spawned (spawn program = `<root>/node_modules/.bin/…`).
  One test per refusal text.
- Phase 1 live (local build): fresh `claude -p` in duro-app
  `incomingCalls useCopyFeedback.ts:9` → 8 callers; in social-planner
  (TypeScript 6.0.3) a non-empty `incomingCalls` and the adapter's
  selection report naming 6.0.3.
- Phase 3, per repo: doctor exit 0 typescript `verified … (adapter 6.0.1)`;
  `npm pack --dry-run` unchanged (published libs); runtime image without the
  adapter (Dockerfile repos); CI `success`.
- Phase 4: `which typescript-language-server` empty; a loop of `fleet-lsp
  doctor` over the 12 repos → 12 × exit 0; duro-app live call → 8 callers.
- Phase 5 unit: tools module at project root / git root; go.mod `tool` form;
  vendor/ and go.work refused; local Go older than the tools `go` line →
  refused; local Go older than the project go.mod's `go` line → refused;
  a gitignored project root → refused; spawn args carry
  `-modfile=<absolute>` and `GOTOOLCHAIN=local`
  (asserted), via an offline fixture tool module with a `replace` to a path,
  run through the real `go tool -modfile`.
- Phase 5 live: `go clean -cache` then the first `doctor` in
  authelia-oidc-operator — cold build time recorded; fresh session
  `incomingCalls assembler.go:30` → 3 callers; homelab
  `wasm/subsonic-auth` (go 1.22.12) answers through the shared tools module.
- Phase 7: `which gopls` empty; doctor loop over the 8 Go repos → 8 × exit
  0, verdict `verified`.
- 7 days after each release: no `refused` (other than `stale
  node_modules`) in session logs for the repositories verified on release
  day — else rollback.

Observed 2026-10-07:
- Phase 1: `make check` — 97 unit + 20 integration tests, clippy clean, no
  dependencies, msrv 1.74, plugin manifest 0.2.0. New unit tests: exact-name
  lockfile lookups in pnpm/package-lock/yarn/bun with both packages; adapter
  verified from `node_modules/.bin` (spawn program asserted — PATH is not
  read at all); 6.0.2 → verified + narrowed; not pinned / not installed /
  stale / Node too old / Python not a uv project — one test per refusal
  text. Found while testing: pnpm's root-importer lookup never worked (a
  blank line after `importers:` reset it; the unique-version fallback hid
  it) — fixed. Live (0.2.0 debug build first on PATH, duro-app worktree
  pinned): `doctor` → `typescript verified 5.9.3 (adapter 6.0.1, node
  24.14.0)`, exit 0; the unpinned live checkout → `refused`, `typescript-
  language-server is not pinned`, exit 1; a fresh `claude -p`
  `incomingCalls useCopyFeedback.ts:9` → 8 callers, served by
  `duro-app-wt-tsls/node_modules/.bin/typescript-language-server`, gate
  open. The TypeScript 6.0.3 live check (social-planner) runs when that
  repository is pinned in Phase 3.
- Phase 2: decisions branch `feat/language-servers-pinned` — `aval check`
  → 27 records, no findings; `aval rule toolchain.language-servers-pinned`
  → constraint, adopts ADR-0019, active. Implementation review: approve-
  with-changes (source line, gopls exception, broker role) → fixed; delta
  approve-with-changes (record this here) → this entry.
- Phase 3: 12 PRs merged, each CI `success` (governance-ts has no CI:
  build + 14 tests run locally, green): agent-console #9, customer-vision
  #19, kb-vision #39, ticket-vision #18, application-landscape #323,
  social-planner #54, grid #34, governance-ts #2, website-builder #198,
  duro-lexical-multi #20 (Forgejo); duro-app #141 (replaced #139 after a
  conflict, rebased without a force-push), duro-design-system #73 (GitHub).
  `doctor` (0.2.0 local build) → exit 0, `typescript verified … (adapter
  6.0.1, node 24.14.0)` in all 12, and inside a workspace member
  (duro-design-system `packages/cli`, website-builder `apps/builder-api`)
  it resolves the workspace root. Every lockfile diff adds only the
  adapter. Published libs: `npm pack --dry-run` file lists identical
  (grid 202, governance-ts 6, duro-lexical-multi 148, duro-design-system 7
  packages); only `package.json` grows by the devDependency line.
  TypeScript 6.0.3 live (social-planner): `documentSymbol` answered, gate
  open after ~88 s, verdict verified — the barrier key stays the adapter.
  Differences from the plan, recorded not fixed:
  - Runtime images: agent-console, customer-vision, social-planner,
    duro-app install prod-only; kb-vision, ticket-vision,
    application-landscape (tsx relay, deliberate) and four website-builder
    images (builder-api, builder-admin, workerd-runtime, sync-bridge) copy
    the builder's full `node_modules`, so they already ship every
    devDependency and now the adapter too (~2.4 MB, ~3.6 MB with pnpm's
    extras). Pre-existing; a prod-only install there is separate work.
  - pnpm repos: pnpm's built-in compatibility table adds
    `vscode-jsonrpc`/`vscode-languageserver-protocol` to the adapter
    (~1.2 MB) — "zero dependencies" holds for npm only. Turning it off is
    repo-wide; left on.
  - duro-lexical-multi: main's lockfile is pnpm 10 while CI runs pnpm 9;
    locked with pnpm 10 (pnpm 9 `--frozen-lockfile` accepts it).
  - No `.adr.yaml` (the rule cannot resolve there, the pin still holds):
    agent-console, ticket-vision, grid, governance-ts, duro-lexical-multi.

## Outcome

<!-- panel: repos=fleet-lsp,decisions,duro-app,authelia-oidc-operator,homelab reviewers=backend,architect,po,lang:go,lang:typescript,lang:python,react,ui-design,ux-research,game-ux body-sha=5c9210e5f402 -->
