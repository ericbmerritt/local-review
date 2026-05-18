#!/usr/bin/env bash
# Create a three-change jj stack fixture at the path given as $1, then
# pre-populate the comment store with:
#   - one pending comment for a change in the stack
#   - one comment file for a change_id NOT in the stack (orphan)
# Used by integration tests; assumes jj is on PATH.

set -euo pipefail

if [ $# -ne 1 ]; then
    echo "usage: stack_with_orphan.sh <repo-path>" >&2
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

export JJ_CONFIG="$config"

cd "$repo"
jj git init --quiet

# Match the runtime: jjr writes /.jj-review to .gitignore before creating any
# state files. Without this, jj snapshots the .jj-review/ files below and the
# startup precheck (`store::check_review_files_untracked`) rejects them.
echo '/.jj-review' >.gitignore

echo "one" >file.txt
jj describe -m "first" --quiet
change_id=$(jj log -r @ --no-graph -T 'change_id ++ "\n"' | head -1 | tr -d '[:space:]')

jj new --quiet -m "second"
echo "two" >>file.txt

mkdir -p "$repo/.jj-review/comments"

# Pending comment for the first (in-stack) change.
cat >>"$repo/.jj-review/comments/${change_id}.jsonl" <<EOF
{"schema_version":"diff-comment/v2","scope":"change","change_id":"${change_id}","repo_root":"${repo}","revset":"trunk()..@","comment":"in-stack comment","severity":"note","created_at":"2026-04-29T10:00:00Z","status":"pending"}
EOF

# Orphan file: a change_id that does not appear in the jj history.
# Use a well-formed but non-existent change_id that passes ChangeId::parse.
orphan_id="aabbccddeeff0011"
cat >>"$repo/.jj-review/comments/${orphan_id}.jsonl" <<EOF
{"schema_version":"diff-comment/v2","scope":"change","change_id":"${orphan_id}","repo_root":"${repo}","revset":"trunk()..@","comment":"orphan comment","severity":"note","created_at":"2026-04-29T10:01:00Z","status":"pending"}
EOF
