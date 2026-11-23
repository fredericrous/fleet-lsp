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
# The hooks: SessionStart and SubagentStart run one wrapper, each event named
# on its command line (what `fleet-lsp hook` prints is tested against the real
# binary in tests/hook.rs); first-contact guidance is the in-process mod
# `modules` names (hooks/register.ts, tested by `claude plugin test`), so no
# PostToolUse command spawns per tool call. Here: the wiring, and that the
# wrapper never fails a session.
h=plugins/fleet-lsp/hooks/code-intelligence.sh
j=plugins/fleet-lsp/hooks/hooks.json
[ -x "$h" ] || { echo "$h is not executable" >&2; fail=1; }
for e in session-start subagent-start; do
	n=$(grep -cF 'hooks/code-intelligence.sh\" '"$e"'"' "$j" || true)
	[ "$n" = 1 ] || { echo "hooks.json runs $h $e $n times, want 1" >&2; fail=1; }
done
! grep -q '"PostToolUse"' "$j" || { echo "hooks.json has a PostToolUse hook; remove it: hooks/register.ts gives the first-contact note" >&2; fail=1; }
# The first `modules` entry, "./" optional, on one line or several.
m=$(tr -d '\n' <"$j" | sed -n 's/.*"modules":[[:space:]]*\[[[:space:]]*"\(\.\/\)\{0,1\}\([^"]*\)".*/\2/p')
if [ -z "$m" ]; then
	echo 'hooks.json names no module: add "modules": ["./register.ts"]' >&2; fail=1
elif [ ! -f "plugins/fleet-lsp/hooks/$m" ]; then
	echo "hooks.json modules names $m, which is not in plugins/fleet-lsp/hooks" >&2; fail=1
fi
# A binary older than `hook` (it exits 2), and none at all: silent, exit 0.
stub=$(mktemp -d) && printf '#!/bin/sh\necho usage >&2\nexit 2\n' >"$stub/fleet-lsp" && chmod +x "$stub/fleet-lsp"
o=$(echo '{}' | PATH="$stub:$PATH" "$h" post-tool-use) && [ -z "$o" ] || { echo "$h is not silent with an old binary" >&2; fail=1; }
rm -r "$stub"
o=$(echo '{}' | PATH=/nonexistent /bin/sh "$h" session-start) && [ -z "$o" ] || { echo "$h is not silent without the binary" >&2; fail=1; }
[ "$fail" = 0 ] && echo "plugin manifest matches $v"
exit "$fail"
