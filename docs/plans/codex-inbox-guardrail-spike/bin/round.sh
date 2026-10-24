#!/bin/sh
# usage: [NEW=1] round.sh <session>: snapshot (NEW=1: /new before each form), launch, ask forms a..h in order, quit, snapshot
S=$(cd "$(dirname "$0")/.." && pwd -P); T=$S/bin/t; s=$1
sh $S/bin/snap.sh before-$s
$T new-session -d -s $s -x 180 -y 50 "sh $S/bin/launch.sh $s; echo EXIT=\$?; sleep 600"
sleep 8
$T capture-pane -p -t $s > $S/screens/$s-start.txt
X='dagq request decline'
sh $S/bin/ask.sh $s a "$X"
sh $S/bin/ask.sh $s b "true && $X"
sh $S/bin/ask.sh $s c "true; $X"
sh $S/bin/ask.sh $s d "echo | $X"
sh $S/bin/ask.sh $s e "sh -c '$X'"
sh $S/bin/ask.sh $s f 'DAGQ_ROLE=worker dagq status'
sh $S/bin/ask.sh $s g 'export DAGQ_ROLE=worker; dagq status'
sh $S/bin/ask.sh $s h 'dagq status'
$T send-keys -t $s -l '/quit'; sleep 1; $T send-keys -t $s Enter; sleep 4
$T capture-pane -p -t $s > $S/screens/$s-end.txt
$T kill-session -t $s
sh $S/bin/snap.sh after-$s
