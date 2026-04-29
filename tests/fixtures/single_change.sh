#!/usr/bin/env bash
# Create a one-change jj fixture repository at the path given as $1.
# Used by integration tests; assumes jj is on PATH.

set -euo pipefail

if [ $# -ne 1 ]; then
    echo "usage: single_change.sh <repo-path>" >&2
    exit 2
fi

repo="$1"
mkdir -p "$repo"

config="$repo/.jjconfig.toml"
cat >"$config" <<'TOML'
[user]
name = "Test"
email = "test@example.com"
TOML

# Isolation note: JJ_CONFIG points at a repo-local config file so the test
# user.name and user.email are used rather than any global config. This does
# not suppress all global config layering (jj may still read OS-level config
# paths), but in practice CI runs with no global jj config so the fixture is
# deterministic there. For full isolation, each jj invocation could pass
# --config user.name="Test" --config user.email="test@example.com" directly;
# JJ_CONFIG is sufficient for the current test needs.
export JJ_CONFIG="$config"

cd "$repo"
jj git init --quiet

cat >hello.txt <<'EOF'
hello
world
EOF

jj describe -m "Add hello.txt" --quiet
jj log -r @ --no-graph -T 'change_id ++ "\n"'
