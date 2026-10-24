#!/bin/sh
# usage: round.sh <run no> <cond: H|N> <limit>
# snapshot before launch, launch in tmux, save screens, two turns (the 2nd crosses the limit), quit, snapshot after exit
S=$(cd "$(dirname "$0")/.." && pwd -P); T=$S/bin/t; n=$1; cond=$2; limit=$3
mkdir -p $S/screens
echo "=== run $n start $(date +%H:%M:%S)" >> $S/hooks.log
echo "=== run $n start $(date +%H:%M:%S)" >> $S/notify.log
$S/bin/snap.sh before-run$n
$T new-session -d -s r$n -x 160 -y 45 "sh $S/bin/launch.sh $cond $limit; echo EXIT=\$?; sleep 600"
sleep 8
$T capture-pane -p -t r$n > $S/screens/run$n-start.txt
if grep -q 'Hooks need review' $S/screens/run$n-start.txt; then $T send-keys -t r$n 3; sleep 1; $T send-keys -t r$n Enter; sleep 3; fi
wait_turn() { i=0; while [ $i -lt 60 ]; do sleep 3; i=$((i+1)); $T capture-pane -p -t r$n | grep -q 'esc to interrupt\|Working\|ompacting' || { [ $i -gt 2 ] && break; }; done; }
$T send-keys -t r$n -l 'Reply with the word ALPHA followed by a 300-word paragraph about tide pools.'; sleep 1; $T send-keys -t r$n Enter; wait_turn
$T send-keys -t r$n -l 'Reply with the word BETA only.'; sleep 1; $T send-keys -t r$n Enter; wait_turn
$T capture-pane -p -t r$n > $S/screens/run$n-end.txt
$T send-keys -t r$n -l '/quit'; sleep 1; $T send-keys -t r$n Enter; sleep 5
$T capture-pane -p -t r$n | grep -q 'EXIT=0' && echo exited >> $S/screens/run$n-end.txt
$T kill-session -t r$n
$S/bin/snap.sh after-run$n
