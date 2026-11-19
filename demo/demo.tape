# Demo recording for the README, made with VHS (https://github.com/charmbracelet/vhs).
#
# Render from the repo root:
#     cargo build --release
#     vhs demo/demo.tape
#
# If VHS fails with "could not start browser" (Ubuntu blocks Chromium's
# sandbox), run it as: VHS_NO_SANDBOX=1 vhs demo/demo.tape
#
# It runs asf against a fake home folder with invented sessions (see
# make-demo-home.py), so no real sessions end up in the recording.

Output demo/asf.gif

Require python3

Set Shell "bash"
Set FontSize 16
Set Width 1600
Set Height 860
Set Padding 20
Set TypingSpeed 90ms
Set Theme "TokyoNight"

# Setup, not recorded: fake home, repo build on PATH, plain prompt.
Hide
Type `export HOME=/tmp/asf-demo XDG_CONFIG_HOME=/tmp/asf-demo/.config XDG_CACHE_HOME=/tmp/asf-demo/.cache PATH="$PWD/target/release:$PATH" PS1='$ '`
Enter
Type "python3 -I demo/make-demo-home.py $HOME > /dev/null && clear"
Enter
Sleep 1s
Show

# Start asf: all sessions, newest first, with a preview of the selected one.
Type "asf"
Sleep 400ms
Enter
Sleep 3s

# Browse the list.
Down
Sleep 900ms
Down
Sleep 900ms
Down
Sleep 1.5s

# Folder tree view.
Tab
Sleep 3s
Tab
Sleep 1s

# Search through the conversations, jump between matches.
Type "cache"
Sleep 3s
Ctrl+N
Sleep 1.5s
Ctrl+N
Sleep 1.5s
Ctrl+N
Sleep 2s

# Next result, its preview centers on the first match.
Down
Sleep 2.5s

# Clear the search. The top session is open in another terminal, so asf asks first.
Backspace 5
Sleep 1.5s
Enter
Sleep 2s
Left
Sleep 1s
Right
Sleep 1s
Enter
Sleep 1.5s

Escape
Sleep 500ms
