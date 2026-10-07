#!/bin/sh
# fleet-lsp relays every byte between Claude Code and a language server, so
# it links nothing it does not own (change.dependency-bar). This check is what
# keeps a convenience dependency from quietly arriving later.
set -eu
extra=$(grep '^name = ' Cargo.lock | sed 's/name = //; s/"//g' | grep -v '^fleet-lsp$' || true)
if [ -n "$extra" ]; then
    echo "fleet-lsp must have no external dependencies, found:" >&2
    echo "$extra" >&2
    exit 1
fi
echo "no external dependencies"
