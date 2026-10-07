#!/bin/sh
# The plugin manifest and the binary must agree on the version: plugin.json's
# `version` and every `--min-version` it passes are the Cargo.toml version, so
# a release ships a manifest that requires exactly the binary built with it.
# The four `requestTimeout`s stay the ceiling (370 s) + 30 s.
set -eu
v=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
m=plugins/fleet-lsp/.claude-plugin/plugin.json
fail=0
grep -q "\"version\": \"$v\"" "$m" || { echo "plugin.json version is not $v" >&2; fail=1; }
n=$(grep -c "\"--min-version\", \"$v\"" "$m" || true)
[ "$n" = 4 ] || { echo "plugin.json passes --min-version $v $n times, want 4" >&2; fail=1; }
t=$(grep -c '"requestTimeout": 400000' "$m" || true)
[ "$t" = 4 ] || { echo "plugin.json sets requestTimeout 400000 $t times, want 4" >&2; fail=1; }
grep -q 'DEFAULT_CEILING: Duration = Duration::from_secs(370)' src/relay.rs || { echo "ceiling is no longer 370 s: update requestTimeout" >&2; fail=1; }
[ "$fail" = 0 ] && echo "plugin manifest matches $v"
exit "$fail"
