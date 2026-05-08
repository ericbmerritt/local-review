#!/usr/bin/env bash
# Create a three-change jj stack fixture at the path given as $1.
# Used by integration tests; assumes jj is on PATH.
#
# The resulting stack walks oldest-to-newest: "first", "second", "third".
# `@` ends up on the "third" change.

set -euo pipefail

if [ $# -ne 1 ]; then
    echo "usage: three_change_stack.sh <repo-path>" >&2
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

# Isolate the test user from any global jj config (see single_change.sh for
# the full caveat about OS-level config layering).
export JJ_CONFIG="$config"

cd "$repo"
jj git init --quiet

# First change.
echo "one" >file.txt
jj describe -m "first" --quiet

# Second change on top.
jj new --quiet -m "second"
echo "two" >>file.txt

# Third change on top.
jj new --quiet -m "third"
echo "three" >>file.txt
