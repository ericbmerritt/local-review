#!/usr/bin/env bash
# Create a one-change jj fixture repository at the path given as $1, then
# pre-populate the comment store with one stale and one pending comment.
# Used by integration tests; assumes jj is on PATH.
#
# If XDG_DATA_HOME is set in the environment the comments are written to
# $XDG_DATA_HOME/jjr/repos/<canonical-repo>/comments/ (new XDG layout).

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

# Resolve comment storage location based on XDG_DATA_HOME (mirrors store::comments_dir).
canonical_repo=$(realpath "$repo")
relative_repo="${canonical_repo#/}"
if [ -n "${XDG_DATA_HOME:-}" ]; then
    comments_dir="$XDG_DATA_HOME/jjr/repos/$relative_repo/comments"
else
    comments_dir="$repo/.jj-review/comments"
fi
mkdir -p "$comments_dir"

cat >>"$comments_dir/${change_id}.jsonl" <<EOF
{"schema_version":"diff-comment/v2","scope":"line","change_id":"${change_id}","repo_root":"${repo}","revset":"@","file":"hello.txt","side":"new","new_line":1,"hunk_header":"@@ -0,0 +1,2 @@","target_text":"hello","context_before":[],"context_after":["world"],"comment":"stale comment","severity":"note","created_at":"2026-04-29T10:00:00Z","status":"stale","mismatch_reason":"target_text changed"}
{"schema_version":"diff-comment/v2","scope":"line","change_id":"${change_id}","repo_root":"${repo}","revset":"@","file":"hello.txt","side":"new","new_line":2,"hunk_header":"@@ -0,0 +1,2 @@","target_text":"world","context_before":["hello"],"context_after":[],"comment":"pending comment","severity":"suggestion","created_at":"2026-04-29T10:01:00Z","status":"pending"}
EOF
