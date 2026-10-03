#!/bin/sh
# The after comparison of docs/plans/acceptance-check.md (task 1423), in the
# order fetch -> compute -> crosscheck, with the baseline's script (task
# 1422), area table and line terciles. Run from this directory with the
# fixed binary (~/.local/bin/dagq) on PATH. `sh after.sh fetch` reads the
# production queue (read-only commands and git); `compute` and `crosscheck`
# read only the snapshot. Without an argument, all three in order. To measure
# again, set C (and E2 if task 1420 or 1421 landed again) and run it.
set -eu
cd "$(dirname "$0")"

S=2026-10-02T03:07:27.872Z       # mark 60152: the run review moves to Codex
M=2026-10-02T08:05:36Z           # mark 61916: Claude workers default to headless
E=2026-10-03T00:12:09.306Z       # the baseline's end (task 1422)
E2=2026-10-03T00:29:27.268Z      # the first run_integrated of tasks 1420 and 1421 (1420, event 69870)
R=2026-10-01T06:02:46.438Z       # the baseline's reference interval start
A=2026-10-02T14:44:46.885Z       # goal 90 created: the end of the audit it quotes
C=${C:-2026-10-03T01:25:00Z}     # observation cutoff
SNAP=snapshot/$C

step=${1:-all}

if [ "$step" = fetch ] || [ "$step" = all ]; then
  python3 fetch.py --since "$R" --until "$C" --cutoff "$C" \
    --stats-since 2026-09-30T00:00:00Z --watch-task 1420 --watch-task 1421 \
    --areas-from areas-2026-10-03T00:15:00Z.toml --repo ../../.. --out "${OUT:-$SNAP}"
fi

if [ "$step" = compute ] || [ "$step" = all ]; then
  python3 compute.py "$SNAP" \
    --interval "main=$S,$E2" \
    --interval "main_1422=$S,$E" \
    --interval "main_before_0805=$S,$M" \
    --interval "main_after_0805=$M,$E2" \
    --interval "reference=$R,$S" \
    --interval "audit=$S,$A,$A" \
    --line-terciles 178,458 --changed-task 1420 --changed-task 1421 \
    --cutoff "$C" --t-task 1420 --t-landed-task 1421 \
    --after after=main --compare main=after
fi

if [ "$step" = crosscheck ] || [ "$step" = all ]; then
  python3 crosscheck.py "$SNAP"
fi
