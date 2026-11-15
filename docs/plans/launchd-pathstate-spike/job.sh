#!/bin/sh
# 使い捨ての agent の job。起きた時刻と pid を "$1/$2.count" に足し、
# "$1/$2.cmd" が現れたらその中身を終了コードにして終わる。
# SIGTERM は "$1/$2.termhold" があれば記録して 90 秒走り続けてから自分で終わり、無ければすぐ終わる。
d=$1
n=$2
f="$d/$n.count"
echo "start $(date +%s) $$" >> "$f"
deadline=
trap 'if [ -f "$d/$n.termhold" ]; then echo "term-held $(date +%s) $$" >> "$f"; deadline=$(( $(date +%s) + 90 )); else echo "term-exit $(date +%s) $$" >> "$f"; exit 0; fi' TERM
while :; do
  if [ -n "$deadline" ] && [ "$(date +%s)" -ge "$deadline" ]; then
    echo "end-after-term $(date +%s) $$" >> "$f"
    exit 0
  fi
  if [ -f "$d/$n.cmd" ]; then
    c=$(cat "$d/$n.cmd")
    rm -f "$d/$n.cmd"
    echo "exit $c $(date +%s) $$" >> "$f"
    exit "$c"
  fi
  sleep 1
done
