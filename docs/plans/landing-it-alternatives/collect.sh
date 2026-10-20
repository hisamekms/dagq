#!/bin/sh
# Collects the inputs of the comparison of the landing IT selection's
# alternatives (docs/plans/landing-it-alternatives.md) into OUT, a directory
# outside the repository (events, tables and CI logs are materials; the CSVs
# and JSON in this directory are what the document keeps), and computes them.
#
# Usage: sh collect.sh OUT
#
# It reads, once (records, not a measurement on the host: one pass):
#   1. integration_started, integration_rebased and verification_command of
#      the after window, and verification_command of the before window, 12
#      hours at a time -> events_after.json, events_before.json
#   2. run_integrated from 2026-10-06 (a range of CI may start before the
#      window) -> landings.json
#   3. the IT coverage map runs and their tables -> itmap_runs.json, itmap/
#   4. main's CI runs from 2026-10-06 (from landing-verify-switch/ci_main_runs.csv),
#      each job's test step and the failed tests of each failed run, as
#      landing-it-selection/collect.sh steps 3 and 6 read them -> ci/
#   5. replay.py, stages.py and stage_summary.py -> OUT/out
# The logs of the verification commands are read where the events'
# log_path names them (the run directories on the host).
# It needs dagq (read-only commands), git, python3 and an authenticated gh.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
repo=$(git -C "$here" rev-parse --show-toplevel)
out=$1
T0=2026-09-30T10:27:40.972Z T=2026-10-07T10:27:40.972Z T1=2026-10-14T10:27:40.972Z
mkdir -p "$out/itmap" "$out/ci/logs" "$out/out"
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
fetch $T $T1 integration_started integration_rebased verification_command >events_after.json
fetch $T0 $T verification_command >events_before.json
dagq events --full --kind run_integrated --since 2026-10-06T00:00:00Z --until $T1 --limit 100000 >landings.json
gh run list -R hisamekms/dagq --workflow "IT coverage map" --created 2026-10-05..2026-10-15 \
  --json databaseId,conclusion,createdAt,updatedAt,headSha,event --limit 100 >itmap_runs.json
for id in $(python3 -c 'import json,sys; [print(r["databaseId"]) for r in json.load(open(sys.argv[1])) if r["conclusion"] == "success" and r["createdAt"] < "2026-10-14T10:27:40Z"]' itmap_runs.json); do
  [ -d itmap/t$id ] || gh run download $id -R hisamekms/dagq -n it-coverage-map -D itmap/t$id
done
python3 - "$here/../landing-verify-switch/ci_main_runs.csv" <<'PY' >ci/ci-runs.json
import csv, json, sys
json.dump([dict(databaseId=int(x['run_id']), headSha=x['head_sha'], conclusion=x['conclusion'], createdAt=x['created_at'], url=x['url'])
           for x in csv.DictReader(open(sys.argv[1])) if '2026-10-06T00:00:00Z' <= x['created_at'] < '2026-10-14T10:27:41Z'], sys.stdout)
PY
# steps 3 and 6 of landing-it-selection/collect.sh, the runs read 4 at a time
STEPS="3 6" JOBS=4 sh "$here/../landing-it-selection/collect.sh" $T1 ci
python3 "$here/replay.py" . "$repo" out >/dev/null
python3 "$here/stages.py" events_before.json events_after.json "$here/../landing-verify-switch/choice.csv" >out/stages.csv
python3 "$here/stage_summary.py" out/stages.csv events_before.json events_after.json >out/stage-summary.json
