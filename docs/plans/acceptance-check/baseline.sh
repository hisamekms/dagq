#!/bin/sh
# The baseline of docs/plans/acceptance-check.md (task 1422), in the order
# fetch -> compute -> crosscheck. Run from this directory with the fixed
# binary (~/.local/bin/dagq) on PATH. `sh baseline.sh fetch` reads the
# production queue (read-only commands and git); `compute` and `crosscheck`
# read only the snapshot. Without an argument, all three in order.
set -eu
cd "$(dirname "$0")"

S=2026-10-02T03:07:27.872Z       # mark 60152: the run review moves to Codex
M=2026-10-02T08:05:36Z           # mark 61916: Claude workers default to headless
E=2026-10-03T00:12:09.306Z       # run_claimed of this task's run (task 1422)
R=2026-10-01T06:02:46.438Z       # S - (E - S): the reference interval of the same length
A=2026-10-02T14:44:46.885Z       # goal 90 created: the end of the audit it quotes
C=2026-10-03T00:15:00Z           # observation cutoff
SNAP=snapshot/$C

step=${1:-all}

if [ "$step" = fetch ] || [ "$step" = all ]; then
  python3 fetch.py --since "$R" --until "$E" --cutoff "$C" \
    --stats-since 2026-09-30T00:00:00Z --watch-task 1420 --watch-task 1421 \
    --repo ../../.. --out "${OUT:-$SNAP}"
fi

if [ "$step" = compute ] || [ "$step" = all ]; then
  python3 compute.py "$SNAP" \
    --interval "main=$S,$E" \
    --interval "main_before_0805=$S,$M" \
    --interval "main_after_0805=$M,$E" \
    --interval "reference=$R,$S" \
    --interval "audit=$S,$A,$A" \
    --interval "audit_c=$S,$A" \
    --terciles-from main --changed-task 1420 --changed-task 1421
fi

if [ "$step" = crosscheck ] || [ "$step" = all ]; then
  python3 crosscheck.py "$SNAP"
fi
