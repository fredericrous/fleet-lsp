#!/bin/sh
# The plugin's hooks: `code-intelligence.sh <session-start|post-tool-use|
# subagent-start>`, the hook's JSON on stdin. `fleet-lsp hook` decides where
# the guidance serves (a project of a served language, the first touch of
# such a file, a subagent with the LSP tool) and prints it; this wrapper only
# keeps a hook from ever failing the person's session. Without the binary
# every LSP query fails, so there is no guidance to give; a binary older than
# `hook` exits 2 here, and its silence is the right answer too. The binary
# logs its own failures to $XDG_STATE_HOME/fleet-lsp/hook/<date>.log.
command -v fleet-lsp >/dev/null 2>&1 || exit 0
fleet-lsp hook "$@" 2>/dev/null || exit 0
