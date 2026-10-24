#!/bin/sh
# usage: round.sh <run no> <launcher> <cond> [project]
# snapshot before launch, launch in tmux, save screens, run one turn, quit, snapshot after exit
S=$(cd "$(dirname "$0")/.." && pwd -P); T=$S/bin/t; n=$1; L=$2; cond=$3
mkdir -p $S/screens
if [ "$4" = project ]; then mkdir -p $S/repo/.codex; printf '[[hooks.SessionStart]]\n[[hooks.SessionStart.hooks]]\ntype = "command"\ncommand = "sh %s/bin/hook.sh project SessionStart"\n' "$S" > $S/repo/.codex/config.toml; else rm -rf $S/repo/.codex; fi
echo "=== run $n start $(date +%H:%M:%S)" >> $S/hooks.log
$S/bin/snap.sh before-run$n
$T new-session -d -s r$n -x 160 -y 45 "sh $S/bin/$L $cond; echo EXIT=\$?; sleep 600"
sleep 8
$T capture-pane -p -t r$n > $S/screens/run$n-start.txt
if grep -q 'Hooks need review' $S/screens/run$n-start.txt; then $T send-keys -t r$n 3; sleep 1; $T send-keys -t r$n Enter; sleep 3; fi
sh $S/bin/turn.sh r$n > $S/screens/run$n-end.txt
$T capture-pane -p -t r$n | grep -q 'EXIT=0' && echo exited >> $S/screens/run$n-end.txt
$T kill-session -t r$n
$S/bin/snap.sh after-run$n
grep -v '^\s*$' $S/screens/run$n-start.txt | head -8
