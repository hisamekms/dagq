#!/bin/sh
# Collects the inputs of the landing IT selection measurement
# (docs/plans/landing-it-selection.md) into OUT, a directory outside the
# repository (the inputs are materials: the map alone is 13 MB, and
# analyze.py's CSVs are what the document keeps).
#
# Usage: sh collect.sh T_END OUT
#   T_END   the end of the period, a UTC midnight (e.g. 2026-10-06T00:00:00Z)
#
# It reads, once:
#   1. the run_integrated events from 2026-09-29T00:00:00Z to T_END with
#      dagq events --full, paging with --after <cursor> until events is empty
#      (one page holds at most --limit 100) -> landings.tsv
#   2. main's push CI runs created from 2026-09-29 to the day before T_END with
#      gh run list --limit 1000; if it returns 1000 runs it reads the period
#      again day by day -> ci-runs.json
#   3. the failed tests of each failed run: the FAIL rows of the nextest
#      summary in gh run view --log-failed (both the macOS and the linux job;
#      the linux job's Failed tests step prints the same rows) -> ci-failed-tests.tsv
#   4. the ci-failure issues (all states) with their comments -> ci-failure-issues.json
#   5. the latest it-coverage-map artifact (and every one still kept, for the
#      staleness comparison) -> maps/<run id>.json
#   6. each job of each run that was not cancelled and the conclusion of its
#      test step (cargo llvm-cov nextest / cargo nextest run): success means
#      the job's tests ran and passed, failure with nextest's Summary means
#      they ran and the rows of 3 are the failures, anything else (or a job the
#      run did not have, like the linux job before it was added) means the
#      job gave no test result -> ci-jobs.tsv
# STEPS (default "1 2 3 4 5 6") runs only the named steps, e.g. STEPS="3 4 5".
# It needs dagq (read-only commands), git and an authenticated gh.
set -eu

T_END=${1:?usage: sh collect.sh T_END OUT}
OUT=${2:?usage: sh collect.sh T_END OUT}
SINCE=2026-09-29T00:00:00Z
REPO=${REPO:-hisamekms/dagq}
mkdir -p "$OUT/maps"
STEPS=${STEPS:-1 2 3 4 5 6}
want() { case " $STEPS " in *" $1 "*) return 0 ;; esac; return 1; }

if want 1; then
# 1. landings
: > "$OUT/landings.tsv"
after=0
while :; do
  page=$(dagq events --full --kind run_integrated --since "$SINCE" --until "$T_END" --after "$after" --limit 100)
  n=$(printf '%s' "$page" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["events"]))')
  [ "$n" -eq 0 ] && break
  printf '%s' "$page" | python3 -c '
import json, sys
d = json.load(sys.stdin)
for e in d["events"]:
    p = e["payload"]
    print("\t".join(str(x) for x in [e["id"], e["created_at"], e["task_id"], e["run_id"], p["main_before"], p["commit"], str(p.get("verification_skipped", False)).lower()]))
' >> "$OUT/landings.tsv"
  after=$(printf '%s' "$page" | python3 -c 'import json,sys; print(json.load(sys.stdin)["cursor"])')
done
echo "landings: $(wc -l < "$OUT/landings.tsv")"

fi

if want 2; then
# 2. CI runs (created is inclusive on both ends; the last day is T_END - 1 day)
last_day=$(python3 -c "import datetime,sys; t=datetime.datetime.strptime(sys.argv[1][:10],'%Y-%m-%d'); print((t-datetime.timedelta(days=1)).strftime('%Y-%m-%d'))" "$T_END")
fields=databaseId,headSha,conclusion,createdAt,url
gh run list -R "$REPO" --workflow CI --branch main --event push --created "2026-09-29..$last_day" --json "$fields" --limit 1000 > "$OUT/ci-runs.json"
if [ "$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "$OUT/ci-runs.json")" -ge 1000 ]; then
  echo "gh run list hit --limit 1000: reading day by day"
  python3 - "$last_day" <<'PY' > "$OUT/days.txt"
import datetime, sys
d = datetime.date(2026, 9, 29)
end = datetime.date.fromisoformat(sys.argv[1])
while d <= end:
    print(d.isoformat()); d += datetime.timedelta(days=1)
PY
  echo '[]' > "$OUT/ci-runs.json"
  while read -r day; do
    gh run list -R "$REPO" --workflow CI --branch main --event push --created "$day" --json "$fields" --limit 1000 > "$OUT/ci-day.json"
    python3 -c 'import json,sys; a=json.load(open(sys.argv[1])); a+=json.load(open(sys.argv[2])); json.dump(a, open(sys.argv[1],"w"))' "$OUT/ci-runs.json" "$OUT/ci-day.json"
  done < "$OUT/days.txt"
  rm -f "$OUT/ci-day.json" "$OUT/days.txt"
fi
echo "ci runs: $(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "$OUT/ci-runs.json")"

fi

if want 3; then
# 3. failed tests of each failed run
: > "$OUT/ci-failed-tests.tsv"
for id in $(python3 -c 'import json,sys; [print(r["databaseId"]) for r in json.load(open(sys.argv[1])) if r["conclusion"]=="failure"]' "$OUT/ci-runs.json"); do
  gh run view -R "$REPO" "$id" --log-failed 2>/dev/null > "$OUT/log.txt" || true
  python3 - "$id" "$OUT/log.txt" >> "$OUT/ci-failed-tests.tsv" <<'PY'
import re, sys
run_id, path = sys.argv[1], sys.argv[2]
# gh's log rows are "<job>\t<step>\t<timestamp> <text>"; the nextest rows are
# "<status> [ 1.234s] (n/m) <binary id> <test name>" after the Summary line
# (a TRY n row is a retry). Read the rows after each job's first Summary
# (the linux job's Failed tests step prints the Summary line again at its end).
st = re.compile(r'^\s*(FAIL|FAIL \+ LEAK|LEAK-FAIL|TIMEOUT|ABORT|SIG[A-Z]+|ABORT SIG \d+|TRY \d+ \S+|FLKY-FL \d+/\d+|FLAKY \d+/\d+|XFAIL)\s+\[\s*[\d.]+s\]\s+(\(\d+/\d+\)\s+)?(\S+)\s+(\S+)\s*$')
seen = set()
jobs = {}
for raw in open(path, errors="replace"):
    parts = raw.rstrip("\n").split("\t", 2)
    if len(parts) < 3:
        continue
    job, text = parts[0], parts[2]
    text = re.sub(r'^\S+Z ', '', text)
    text = re.sub(r'\x1b\[[0-9;]*m', '', text)
    jobs.setdefault(job, []).append(text)
for job, rows in jobs.items():
    idx = min([i for i, t in enumerate(rows) if re.match(r'^\s+Summary \[', t)] or [-1])
    if idx < 0:
        continue
    for t in rows[idx + 1:]:
        m = st.match(t)
        if not m:
            continue
        status, binary, name = m.group(1), m.group(3), m.group(4)
        flaky = status.startswith("FLKY") or status.startswith("FLAKY")
        key = (job, binary, name)
        if key in seen:
            continue
        seen.add(key)
        print("\t".join([run_id, "linux" if "linux" in job.lower() else "macos", binary + "::" + name, "flaky" if flaky else "fail"]))
PY
done
rm -f "$OUT/log.txt"
echo "failed test rows: $(wc -l < "$OUT/ci-failed-tests.tsv")"

fi

if want 4; then
# 4. ci-failure issues
gh issue list -R "$REPO" --label ci-failure --state all --limit 200 --json number,title,state,createdAt,closedAt,body,comments > "$OUT/ci-failure-issues.json"

fi

if want 5; then
# 5. the coverage maps still kept
gh run list -R "$REPO" --workflow it-coverage-map.yml --status success --json databaseId,headSha,createdAt --limit 100 > "$OUT/map-runs.json"
for id in $(python3 -c 'import json,sys; [print(r["databaseId"]) for r in json.load(open(sys.argv[1]))]' "$OUT/map-runs.json"); do
  [ -s "$OUT/maps/$id.json" ] && continue
  rm -rf "$OUT/maps/tmp"
  if gh run download -R "$REPO" "$id" -n it-coverage-map -D "$OUT/maps/tmp" 2>/dev/null; then
    mv "$OUT/maps/tmp/it-coverage-map.json" "$OUT/maps/$id.json"
  fi
  rm -rf "$OUT/maps/tmp"
done
echo "maps: $(ls "$OUT/maps" | wc -l | tr -d ' ')"
fi

if want 6; then
# 6. the jobs and their test step
: > "$OUT/ci-jobs.tsv"
for id in $(python3 -c 'import json,sys; [print(r["databaseId"]) for r in json.load(open(sys.argv[1])) if r["conclusion"]!="cancelled"]' "$OUT/ci-runs.json"); do
  gh run view -R "$REPO" "$id" --json jobs --jq '.jobs[] | [.name, ([.steps[] | select(.name == "cargo llvm-cov nextest" or .name == "cargo nextest run") | .conclusion] | first // "none")] | @tsv' |
    sed "s/^/$id\t/" >> "$OUT/ci-jobs.tsv"
done
echo "ci jobs: $(wc -l < "$OUT/ci-jobs.tsv")"
fi
