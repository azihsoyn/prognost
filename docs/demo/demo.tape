# The README's demo. Record it with:
#   docs/demo/make-demo-repo.sh /tmp/prognost-demo
#   PATH="$PWD/target/release:$PATH" vhs docs/demo/demo.tape
Output docs/demo/demo.gif

Set Shell "bash"
Set FontSize 13
Set Width 1600
Set Height 880
Set Padding 24
Set Theme "Catppuccin Mocha"
Set TypingSpeed 45ms
Set CursorBlink false

Hide
Type "cd /tmp/prognost-demo && export PS1='\[\e[38;5;117m\]feature/pool-drain\[\e[0m\] $ ' && clear"
Enter
Show

Sleep 600ms
Type "prognost plan --base main"
Sleep 400ms
Enter
Sleep 3.5s

Type "prognost plan --base main --json | prognost assess -"
Sleep 400ms
Enter
Sleep 5s

Type "prognost graph --base main"
Sleep 400ms
Enter
Sleep 3s
# down to the shop's database client, then up its callers to the web page
Type "j"
Sleep 500ms
Type "j"
Sleep 500ms
Type "j"
Sleep 900ms
Type "h"
Sleep 1.3s
Type "h"
Sleep 1.3s
Type "h"
Sleep 2s
# back down the chain to the changed library function, and open its diff
Type "l"
Sleep 500ms
Type "l"
Sleep 500ms
Type "l"
Sleep 500ms
Type "l"
Sleep 1.2s
Enter
Sleep 3.5s
Type "q"
Sleep 600ms
Type "q"
Sleep 500ms
