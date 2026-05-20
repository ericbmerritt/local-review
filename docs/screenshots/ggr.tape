# ggr README hero capture.
#
# Renders docs/screenshots/ggr.gif. Run from the workspace root inside the
# Nix dev shell:
#
#   nix develop --command vhs docs/screenshots/ggr.tape
#
# Targets PR #28 in this repo (auto-detected from the local git remote).
# Requires `gh` to be authenticated against github.com.

Output docs/screenshots/ggr.gif

Set Shell "zsh"
Set FontSize 14
Set Width 1300
Set Height 720
Set Padding 30
Set Theme "Dracula"
Set Framerate 30
Set TypingSpeed 30ms

Hide
Type "export PATH=$PWD/target/release:$PATH && clear"
Enter
Sleep 300ms
Show

Type "ggr 28"
Enter
Sleep 3500ms

# n advances PR description -> first commit. Tab walks the commit's files.
Type "n"
Sleep 600ms
Tab
Sleep 800ms

# Scroll a few lines so the diff body is mid-frame.
Type "j"
Sleep 250ms
Type "j"
Sleep 250ms
Type "j"
Sleep 250ms
Type "j"
Sleep 1800ms

Type "q"
Sleep 400ms
