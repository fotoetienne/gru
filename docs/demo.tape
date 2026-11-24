# Gru demo recording
# Run with: just demo (from repo root)
# Requires: vhs (brew install vhs)
# Regenerates docs/demo.gif: an agent files two gru:todo issues, #102 blocked by #101 → gru lab
# picks up #101, then #102 once its blocker merges → gru status

Output docs/demo.gif

Set Shell "bash"
Set FontSize 13
Set Width 1100
Set Height 480
Set Theme "Catppuccin Mocha"
Set Padding 20
Set TypingSpeed 45ms
Set WindowBar Rings
Set BorderRadius 8
Set Framerate 30

Hide
Type "PS1='\[\033[32m\033[1m\]~/projects/myapp\[\033[0m\] \[\033[90m\]$\[\033[0m\] '" Enter
Type "alias claude='./docs/demo-script.sh claude' gru='./docs/demo-script.sh gru'" Enter
Type "clear" Enter
Show

Sleep 600ms
Type `claude -p "File gru:todo issues for API rate-limit headers, then CLI display (blocked by the first)"`
Sleep 500ms
Enter
Sleep 4.5s

Type "clear" Enter
Sleep 300ms
Type "gru lab"
Sleep 400ms
Enter
Sleep 7s
Ctrl+C
Sleep 1.5s

Type "gru status"
Sleep 400ms
Enter
Sleep 4s
