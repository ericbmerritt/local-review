# jjr README hero capture.
#
# Renders docs/screenshots/jjr.gif. Run from the workspace root inside the
# Nix dev shell so vhs, jjr, and jj are on PATH:
#
#   cargo build --release
#   nix develop --command vhs docs/screenshots/jjr.tape
#
# The Hide block bootstraps a throwaway fixture stack at /tmp/jjr-demo;
# the recorded portion starts after `clear` so viewers see only the jjr
# session, not the setup commands.

Output docs/screenshots/jjr.gif

Set Shell "zsh"
Set FontSize 13
Set Width 1400
Set Height 760
Set Padding 24
Set Theme "Dracula"
Set Framerate 30
Set TypingSpeed 30ms

Hide
Type "export PATH=$PWD/target/release:$PATH"
Enter
Sleep 200ms
Type "bash docs/screenshots/setup-jjr-demo.sh /tmp/jjr-demo"
Enter
Sleep 2000ms
Type "cd /tmp/jjr-demo && export JJ_CONFIG=$PWD/.jjconfig.toml && clear"
Enter
Sleep 300ms
Show

# Open jjr — lands on Screen::Extracting while tree-sitter runs.
Type "jjr"
Enter
# Wait for entity extraction on both changes (async, ~2-3s each).
Sleep 4000ms

# Now on the entity list for change 1 (oldest first).
# Entity list shows: login, verify_credentials, create_session, check_rate_limit,
# hash_password, generate_token — sorted deepest-dependency-first.

# Move down to highlight a row mid-list.
Type "j"
Sleep 300ms
Type "j"
Sleep 300ms
Type "j"
Sleep 1200ms

# Open the entity diff for the focused entity.
Enter
Sleep 800ms

# Scroll a few lines so the diff body is visible.
Type "j"
Sleep 250ms
Type "j"
Sleep 250ms
Type "j"
Sleep 1500ms

# Return to entity list.
Escape
Sleep 600ms

# Advance to change 2 to show a different entity list.
Type "n"
Sleep 4000ms

# Move cursor down past the description row to an entity.
Type "j"
Sleep 300ms
Type "j"
Sleep 1500ms

Type "q"
Sleep 400ms
