# ggr README hero capture.
#
# Renders docs/screenshots/ggr.gif. Run from the workspace root inside the
# Nix dev shell:
#
#   cargo build --release
#   nix develop --command vhs docs/screenshots/ggr.tape
#
# Targets a recent PR in this repo (auto-detected from the local git remote).
# Requires `gh` to be authenticated against github.com.
# --no-graph skips the /tmp clone so recording doesn't wait for a network
# fetch; entity lists and diffs still render fully.

Output docs/screenshots/ggr.gif

Set Shell "zsh"
Set FontSize 13
Set Width 1400
Set Height 760
Set Padding 24
Set Theme "Dracula"
Set Framerate 30
Set TypingSpeed 30ms

Hide
Type "export PATH=$PWD/target/release:$PATH && clear"
Enter
Sleep 300ms
Show

# Open a PR — lands on the description page (entry 0).
Type "ggr --no-graph 60"
Enter
# Wait for PR fetch + draft re-anchor.
Sleep 4000ms

# The description page shows PR title, body, PR-level comments.
# Press e to switch to the aggregated entity pane.
Type "e"
# Wait for entity extraction across all commits.
Sleep 6000ms

# Entity pane: every changed entity across the whole PR, topo-sorted.
# Move down to show a few rows.
Type "j"
Sleep 300ms
Type "j"
Sleep 300ms
Type "j"
Sleep 1200ms

# Switch to per-commit view: n advances to commit 1.
Type "n"
Sleep 4000ms

# Now on commit 1's entity list. Open an entity diff.
Type "j"
Sleep 300ms
Enter
Sleep 800ms

# Scroll down to see the diff body.
Type "j"
Sleep 250ms
Type "j"
Sleep 250ms
Type "j"
Sleep 1600ms

# Return to entity list.
Escape
Sleep 500ms

Type "q"
Sleep 400ms
