# Full reviews: fleet-lsp first-contact guidance in process

## Full reviews (reference)
- **backend (opus):** R1 approve-with-changes, 7 (2 high: parallel guide, compaction order); 5 accepted, 2 rejected (cost arithmetic, picture) (28k, 41 s). R2 tui seat + stale critical files (35k, 35 s). Codex delta: stale regex/compact text, applied (39k, 32 s). Final: resolved (40k, 5 s).
- **lang:rust (opus):** R1 approve-with-changes, 6 (high: drift test can't see `pub(crate)`), all accepted (25k, 35 s). R2 approve (31k, 28 s). Codex delta approve; carried: core-origin mirror test, `--locked` (33k, 28 s). Final resolved (33k, 6 s).
- **lang:typescript (opus):** R1 approve-with-changes, 5 (2 high: parallel guide, compact twice), all accepted (49k, 94 s). R2 approve (51k, 18 s). Codex delta approve: `next.origin.tier` confirmed in the types (58k, 36 s). Final approve (59k, 13 s).
- **tui (opus):** R1 approve-with-changes, 2 (README/log, HELP), accepted (30k, 42 s). R2 approve-with-changes (33k, 16 s). Codex delta resolved (37k, 59 s). Final resolved (37k, 8 s).
- **unix (opus):** R1 approve-with-changes, 5 (high: hook log can't prove no spawn → PATH shim), accepted (31k, 58 s). R2 mutation-test assertions (36k, 23 s). Codex delta (36k, 48 s). Final approve; carried: Drop guard (37k, 9 s).
- **codex (gpt-6-astra):** approve-with-changes. High: plugin-origin Read consumes the guide (applied); release omits the Cargo bump (applied). Medium: precompute compaction; regex accepts `.mtsx` (both applied in the final pass).
