#!/bin/sh
# send one prompt that runs a shell command, wait, then /quit
S=$(cd "$(dirname "$0")/.." && pwd -P); T=$S/bin/t; s=$1
$T send-keys -t $s -l 'Run the shell command `echo probe` and reply with its output only.'
sleep 1; $T send-keys -t $s Enter
i=0; while [ $i -lt 60 ]; do sleep 2; i=$((i+1)); $T capture-pane -p -t $s | grep -q 'Working\|esc to interrupt' || { [ $i -gt 3 ] && break; }; done
$T capture-pane -p -t $s | grep -v '^\s*$' | tail -25
$T send-keys -t $s -l '/quit'; sleep 1; $T send-keys -t $s Enter; sleep 4
$T capture-pane -p -t $s | grep -v '^\s*$' | tail -6
