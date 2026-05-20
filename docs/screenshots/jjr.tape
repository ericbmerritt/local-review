# jjr README hero capture.
#
# Renders docs/screenshots/jjr.gif. Run from the workspace root inside the
# Nix dev shell so vhs, jjr, and jj are on PATH:
#
#   nix develop --command vhs docs/screenshots/jjr.tape
#
# The Hide block bootstraps a throwaway fixture stack at /tmp/jjr-demo;
# the recorded portion starts after `clear` so viewers see only the jjr
# session, not the setup commands.

Output docs/screenshots/jjr.gif

Set Shell "zsh"
Set FontSize 14
Set Width 1300
Set Height 720
Set Padding 30
Set Theme "Dracula"
Set Framerate 30
Set TypingSpeed 30ms

Hide
Type "export PATH=$PWD/target/release:$PATH"
Enter
Sleep 200ms
Type "bash docs/screenshots/setup-jjr-demo.sh /tmp/jjr-demo"
Enter
Sleep 1500ms
Type "cd /tmp/jjr-demo && export JJ_CONFIG=$PWD/.jjconfig.toml && clear"
Enter
Sleep 300ms
Show

Type "jjr"
Enter
Sleep 2000ms

# Advance to the second change (the retry-policy one — has the most
# visually informative diff: edits an existing function rather than
# scaffolding from nothing).
Type "n"
Sleep 600ms

# Tab off the description page into the file diff.
Tab
Sleep 800ms

# Scroll a couple of lines so the +/- block is mid-frame, not pinned to
# the top edge.
Type "j"
Sleep 300ms
Type "j"
Sleep 300ms
Type "j"
Sleep 1800ms

Type "q"
Sleep 400ms
