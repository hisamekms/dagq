#!/bin/sh
# Collects the inputs of the landing verification switch comparison
# (docs/plans/landing-verify-switch.md) into OUT, a directory outside the
# repository (the events and the nightly tables are materials; the CSVs
# compute.py writes are what the document keeps), and computes them.
#
# Usage: sh collect.sh OUT
#
# It reads, once (records, not a measurement on the host: one pass):
#   1. dagq stats --full of each window -> stats_before.json, stats_after.json
#   2. dagq kpi --compare 123128 --window 7 -> kpi_compare.json
#   3. every event of both windows, 6 hours at a time (fetch_events.py) -> events.json
#   4. main's push runs of the CI workflow, day by day -> ci_<day>.json
#   5. the ci-failure issues (all states) -> ci_issues.json
#   6. the IT coverage map runs and the tables the landings could use
#      -> itmap_runs.json, itmap/t<run id>/it-coverage-map.json
#   7. the replay of landing-it's choice (replay_choice.py) -> choice.csv
#   8. compute.py -> the CSVs and summary.json in OUT/out
# It needs dagq (read-only commands; a worker reads through the queue
# service), git, python3 and an authenticated gh.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
repo=$(git -C "$here" rev-parse --show-toplevel)
out=$1
T0=2026-09-30T10:27:40.972Z T=2026-10-07T10:27:40.972Z T1=2026-10-14T10:27:40.972Z
mkdir -p "$out/itmap" "$out/out"
cd "$out"
dagq stats --full --since $T0 --until $T >stats_before.json
dagq stats --full --since $T --until $T1 >stats_after.json
dagq kpi --compare 123128 --window 7 >kpi_compare.json
python3 "$here/fetch_events.py" $T0 $T1 >events.json
for d in 2026-09-30 2026-10-01 2026-10-02 2026-10-03 2026-10-04 2026-10-05 2026-10-06 \
         2026-10-07 2026-10-08 2026-10-09 2026-10-10 2026-10-11 2026-10-12 2026-10-13 2026-10-14; do
  gh run list -R hisamekms/dagq --workflow CI --branch main --event push --created $d \
    --json databaseId,headSha,conclusion,status,createdAt,updatedAt,url --limit 1000 >ci_$d.json
done
gh issue list -R hisamekms/dagq --label ci-failure --state all --limit 500 \
  --json number,title,createdAt,closedAt,state >ci_issues.json
gh run list -R hisamekms/dagq --workflow "IT coverage map" --created 2026-10-05..2026-10-15 \
  --json databaseId,conclusion,createdAt,updatedAt,headSha,event --limit 100 >itmap_runs.json
for id in $(python3 -c 'import json,sys; [print(r["databaseId"]) for r in json.load(open(sys.argv[1])) if r["conclusion"] == "success" and r["createdAt"] < "2026-10-14T10:27:40Z"]' itmap_runs.json); do
  [ -d itmap/t$id ] || gh run download $id -R hisamekms/dagq -n it-coverage-map -D itmap/t$id
done
python3 "$here/replay_choice.py" events.json itmap_runs.json itmap "$repo" >choice.csv
python3 "$here/compute.py" . out >out/summary.json
cp choice.csv out/
