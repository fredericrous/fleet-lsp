#!/bin/sh
# Session-start guidance: an index, one line per item (decisions ADR-0026,
# guidance.always-on-is-an-index). The operations and their arguments are in
# the LSP tool's own description, so they are not repeated here. Without the
# binary every query fails, so the guidance would point at a dead tool.
command -v fleet-lsp >/dev/null 2>&1 || exit 0
cat <<'TXT'
Code intelligence (fleet-lsp, .rs .go .py .ts/.js):
- Navigate with the LSP tool, not Grep: workspaceSymbol to find a name, then goToDefinition, findReferences, hover, incomingCalls. Grep/Glob are for text: comments, strings, config.
- Before a rename or a signature change: findReferences for the call sites, then Grep for what LSP cannot see (strings, docs, dynamic calls).
- After an edit, diagnostics arrive on their own: fix each type error or missing import before moving on.
- Started outside a repository, each repository you query gets its own server; workspaceSymbol searches the repository of the file used most recently.
- A slow first answer is the server loading. An error that names a fix (`fleet-lsp doctor` explains it): apply the fix or tell the person; do not fall back to Grep without saying so.
TXT
