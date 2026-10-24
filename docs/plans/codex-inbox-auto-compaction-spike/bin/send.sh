#!/bin/sh
# usage: send.sh <session> <text> ; send one prompt and wait for the turn to end
S=$(cd "$(dirname "$0")/.." && pwd -P); T=$S/bin/t; s=$1
$T send-keys -t $s -l "$2"; sleep 1; $T send-keys -t $s Enter
i=0; while [ $i -lt 90 ]; do sleep 2; i=$((i+1)); $T capture-pane -p -t $s | grep -q 'Working\|esc to interrupt\|ompact' || { [ $i -gt 3 ] && break; }; done
