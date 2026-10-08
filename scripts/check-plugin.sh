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
# gopls's first start builds it: 2 x (cold build 95 s on a loaded machine + 7.7 s).
s=$(grep -c '"startupTimeout": 210000' "$m" || true)
[ "$s" = 1 ] || { echo "plugin.json sets the go startupTimeout 210000 $s times, want 1" >&2; fail=1; }
grep -q 'DEFAULT_CEILING: Duration = Duration::from_secs(370)' src/relay.rs || { echo "ceiling is no longer 370 s: update requestTimeout" >&2; fail=1; }
# The session-start guidance stays an index (ADR-0026): at most 6 lines.
h=plugins/fleet-lsp/hooks/code-intelligence.sh
[ -x "$h" ] || { echo "$h is not executable" >&2; fail=1; }
grep -q 'hooks/code-intelligence.sh' plugins/fleet-lsp/hooks/hooks.json || { echo "hooks.json does not run $h" >&2; fail=1; }
stub=$(mktemp -d) && printf '#!/bin/sh\n' >"$stub/fleet-lsp" && chmod +x "$stub/fleet-lsp"
l=$(PATH="$stub:$PATH" "$h" | wc -l | tr -d ' ')
rm -r "$stub"
[ "$l" -ge 1 ] || { echo "$h prints nothing with fleet-lsp on PATH" >&2; fail=1; }
[ "$l" -le 6 ] || { echo "$h prints $l lines, want at most 6" >&2; fail=1; }
[ "$fail" = 0 ] && echo "plugin manifest matches $v"
exit "$fail"
