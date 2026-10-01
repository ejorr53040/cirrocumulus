# M5's exit: `cirro run` into `cirro top` with live graphs, about 30 s.
# Recorded by scripts/demo/record-top.sh (it needs vhs, ttyd, the cirro
# group and a Node agent built from this checkout).

Output docs/cirro-top.gif

Set Shell "bash"
Set FontSize 15
Set Width 1300
Set Height 760
Set TypingSpeed 40ms
Set Theme "Builtin Dark"

Hide
Type `export PATH="$PWD/target/release:$PATH" PS1='$ '; clear`
Enter
Show

Type "cirro run --name web nginx:alpine"
Sleep 300ms
Enter
Sleep 3s
Type `cirro run --name busy alpine -- sh -c 'while :; do :; done'`
Sleep 300ms
Enter
Sleep 3s

# Some traffic for the network graphs, off screen.
Hide
Type `(for i in $(seq 4000); do curl -s http://$(cirro ps | awk '$1=="web"{print $2}')/ >/dev/null; done &) 2>/dev/null; clear`
Enter
Show

Type "cirro top"
Sleep 300ms
Enter
Sleep 8s
Down
Sleep 3s
Tab
Sleep 3s
Up
Sleep 1s
Type "l"
Sleep 4s
Type "q"
Sleep 1s

Hide
Type "pkill -f 'seq 4000' ; for vm in web busy; do cirro stop --force $vm; cirro rm $vm; done; clear"
Enter
Sleep 3s
