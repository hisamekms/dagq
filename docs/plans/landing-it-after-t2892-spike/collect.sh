#!/bin/sh
# Collects the inputs of the measurement of the landing IT selection after
# ADR-t2892-1 (docs/plans/landing-it-after-t2892-spike.md) into OUT, a
# directory outside the repository, and computes them with measure.py.
#
# Usage: sh collect.sh OUT
#
# It reads, once (records, not a measurement on the host: one pass):
#   1. verification_command of the before and the after window, 12 hours at a
#      time -> events_before.json, events_after.json
#   2. run_integrated from 2026-10-19 to the end of the after window (a range
#      of CI may start before the window) -> landings.json
#   3. main's push CI runs from 2026-10-19, the failed tests of each failed
#      run and each job's test step, with steps 2, 3 and 6 of
#      landing-it-selection/collect.sh -> ci/
#   4. measure.py -> OUT/out
#   5. classify.py: for the narrowed landings that did not run a newly red IT,
#      the table each used (downloaded into OUT/tables) and its diff give why
#      -> the it_reason column of OUT/out/new-failures.csv
# The logs of the verification commands are read where the events'
# log_path names them (the run directories on the host).
# It needs dagq (read-only commands), git, python3 and an authenticated gh.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
repo=$(git -C "$here" rev-parse --show-toplevel)
out=$1
T0=2026-10-13T02:12:03.000Z T=2026-10-20T02:12:03.000Z T1=2026-10-27T02:12:03.000Z
mkdir -p "$out/ci" "$out/out"
cd "$out"
fetch() { # FROM TO KIND... -> stdout: the events, 12 hours at a time
  python3 - "$@" <<'PY'
import datetime as D, json, subprocess, sys
def z(x): return x.strftime('%Y-%m-%dT%H:%M:%S.') + f'{x.microsecond // 1000:03d}Z'
f, t = (D.datetime.fromisoformat(a.replace('Z', '+00:00')) for a in sys.argv[1:3])
kinds = [a for k in sys.argv[3:] for a in ('--kind', k)]
out, s = [], f
while s < t:
    u = min(s + D.timedelta(hours=12), t)
    out += json.loads(subprocess.run(['dagq', 'events', '--full', *kinds, '--since', z(s), '--until', z(u), '--limit', '200000'],
                                     capture_output=True, text=True, check=True).stdout)['events']
    s = u
json.dump(out, sys.stdout)
PY
}
fetch $T0 $T verification_command >events_before.json
fetch $T $T1 verification_command >events_after.json
fetch 2026-10-19T00:00:00.000Z $T1 run_integrated >landings.json
SINCE=2026-10-19T00:00:00Z STEPS="2 3 6" JOBS=4 sh "$here/../landing-it-selection/collect.sh" 2026-10-28T00:00:00Z ci
python3 "$here/measure.py" . "$repo" out
mkdir -p tables
python3 "$here/classify.py" . "$repo" tables out
