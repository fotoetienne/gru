# Gru demo recording
# Run with: just demo (from repo root)
# Requires: vhs (brew install vhs)
# Regenerates docs/demo.gif showing `gru do <issue>` → agent works → PR opened

Output docs/demo.gif

Set Shell "bash"
Set FontSize 14
Set Width 900
Set Height 550
Set Theme "Catppuccin Mocha"
Set Padding 20
Set TypingSpeed 70ms
Set WindowBar Rings
Set BorderRadius 8
Set Framerate 30

Hide
Type "PS1='\033[32m\033[1m~/projects/myapp\033[0m \033[90m$\033[0m '" Enter
Type "alias gru='./docs/demo-script.sh'" Enter
Type "clear" Enter
Show

Sleep 800ms
Type "gru do 47"
Sleep 600ms
Enter
# demo-script.sh accumulates ~12.3s of sleep; total GIF is ~16s (0.8s + 0.6s + 15s)
Sleep 15s
