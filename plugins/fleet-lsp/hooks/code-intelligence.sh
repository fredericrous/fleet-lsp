#!/bin/sh
# Session-start guidance: an index, one line per item (decisions ADR-0026,
# guidance.always-on-is-an-index). The operations and their arguments are in
# the LSP tool's own description, so they are not repeated here. Without the
# binary every query fails, so the guidance would point at a dead tool.
#
# `code-intelligence.sh subagent` prints the same index for SubagentStart,
# which takes context only as JSON: SessionStart reaches the main
# conversation alone, and subagents (Explore, workers, reviewers) navigated
# with whole-file Reads and shell grep without it.
command -v fleet-lsp >/dev/null 2>&1 || exit 0
guidance() {
	cat <<'TXT'
Code intelligence (fleet-lsp, .rs .go .py .ts/.js), wherever the LSP tool is available:
- Navigate with the LSP tool, not grep/rg/cat or a whole-file Read: workspaceSymbol to find a name, then goToDefinition, findReferences, hover, incomingCalls; Read only the range LSP points to. Grep/Glob are for text: comments, strings, config.
- Before a rename or a signature change: findReferences for the call sites, then Grep for what LSP cannot see (strings, docs, dynamic calls).
- After an edit, diagnostics arrive on their own: fix each type error or missing import before moving on.
- Started outside a repository, each repository you query gets its own server; workspaceSymbol searches the repository of the file used most recently.
- A slow first answer is the server loading. An error that names a fix (`fleet-lsp doctor` explains it): apply the fix or tell the person; do not fall back to Grep without saying so.
TXT
}
if [ "${1:-}" = subagent ]; then
	# The text holds no control characters; escape \ and " and join the lines.
	printf '{"hookSpecificOutput":{"hookEventName":"SubagentStart","additionalContext":"%s"}}\n' \
		"$(guidance | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' | awk 'NR>1{printf "\\n"} {printf "%s", $0}')"
else
	guidance
fi
