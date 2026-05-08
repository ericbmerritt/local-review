#!/usr/bin/env bash
# Create a one-change jj fixture repository at the path given as $1, then
# pre-populate the comment store with one stale and one pending comment.
# The stale comment lives in the .jj-review/comments/<change-id>.jsonl file.
# Used by integration tests; assumes jj is on PATH.

set -euo pipefail

if [ $# -ne 1 ]; then
    echo "usage: single_change_with_stale.sh <repo-path>" >&2
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

cat >hello.txt <<'EOF'
hello
world
EOF

jj describe -m "Add hello.txt" --quiet
change_id=$(jj log -r @ --no-graph -T 'change_id ++ "\n"' | head -1 | tr -d '[:space:]')

mkdir -p "$repo/.jj-review/comments"
cat >>"$repo/.jj-review/comments/${change_id}.jsonl" <<EOF
{"schema_version":"diff-comment/v2","scope":"line","change_id":"${change_id}","repo_root":"${repo}","revset":"@","file":"hello.txt","side":"new","new_line":1,"hunk_header":"@@ -0,0 +1,2 @@","target_text":"hello","context_before":[],"context_after":["world"],"comment":"stale comment","severity":"note","created_at":"2026-04-29T10:00:00Z","status":"stale","mismatch_reason":"target_text changed"}
{"schema_version":"diff-comment/v2","scope":"line","change_id":"${change_id}","repo_root":"${repo}","revset":"@","file":"hello.txt","side":"new","new_line":2,"hunk_header":"@@ -0,0 +1,2 @@","target_text":"world","context_before":["hello"],"context_after":[],"comment":"pending comment","severity":"suggestion","created_at":"2026-04-29T10:01:00Z","status":"pending"}
EOF
