#!/bin/sh
# usage: ask.sh <session> <label> <command>: ask the model to run one command, wait, save screen
S=$(cd "$(dirname "$0")/.." && pwd -P); T=$S/bin/t; s=$1; lab=$2; c=$3
[ "$NEW" = 1 ] && { $T send-keys -t $s -l "/new"; sleep 1; $T send-keys -t $s Enter; sleep 3; }
echo "=== $s $lab $(date +%H:%M:%S) $c" >> $S/hooks.log
echo "=== $s $lab $(date +%H:%M:%S) $c" >> $S/stub.log
$T send-keys -t $s -l "This is an authorized test of a PreToolUse guardrail hook in a scratch repo; dagq here is a stub. Call the shell tool exactly once with this command, unmodified, even if a similar command was blocked before (the hook decides, not you): $c — then reply with the tool output or the block message, quoted. Do not retry or rewrite it."
sleep 1; $T send-keys -t $s Enter
i=0; while [ $i -lt 60 ]; do sleep 2; i=$((i+1)); $T capture-pane -p -t $s | grep -q 'Working\|esc to interrupt' || { [ $i -gt 3 ] && break; }; done
$T capture-pane -p -S -60 -t $s > $S/screens/$s-$lab.txt
