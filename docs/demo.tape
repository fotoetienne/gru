# Gru demo recording
# Run with: just demo (from repo root)
# Requires: vhs (brew install vhs)
# Regenerates docs/demo.gif: file two chained gru:todo issues → gru lab picks
# up #101, then #102 once its blocker merges → gru status

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
Type "PS1='\033[32m\033[1m~/projects/myapp\033[0m \033[90m$\033[0m '" Enter
Type "rm -f /tmp/gru-demo-issue-count" Enter
Type "alias gh='./docs/demo-script.sh gh' gru='./docs/demo-script.sh gru'" Enter
Type "clear" Enter
Show

Sleep 600ms
Type `gh issue create -l gru:todo -t "Add rate-limit headers"`
Sleep 400ms
Enter
Sleep 1.8s

Type `gh issue create -l gru:todo -t "Show limits in CLI" -b "**Blocked by:** #101"`
Sleep 400ms
Enter
Sleep 2s

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
