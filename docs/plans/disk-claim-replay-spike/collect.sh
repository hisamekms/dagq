#!/bin/sh
# 読むだけ: disk-claim-replay-spike の event を kind ごとに JSONL へ頁送りで読む。
# 使い方: collect.sh DATA_DIR
# 固定の期間の前（窓の温め）と後（期間内に claim した run の終わりと大きさ）も読み、期間の切り分けは replay.py が行う。
set -eu
data=$1
since=2026-09-11T00:00:00Z
until=2026-11-07T04:00:00Z
mkdir -p "$data"
for kind in build_outputs_removed scratchpad_removed run_tmp_removed run_claimed session_closed \
    run_integrated claim_held claim_resumed landing_held landing_resumed supervisor_stopped \
    auto_repaired verification_command; do
    out="$data/$kind.jsonl"
    : >"$out"
    after=0
    while :; do
        dagq events --full --limit 5000 --kind "$kind" --since "$since" --until "$until" --after "$after" >"$data/page.json"
        next=$(python3 -I - "$data/page.json" "$out" <<'PY'
import json, sys
page = json.load(open(sys.argv[1]))
with open(sys.argv[2], "a") as out:
    for event in page["events"]:
        out.write(json.dumps(event) + "\n")
print(page["cursor"] if page["events"] else "")
PY
)
        [ -n "$next" ] || break
        after=$next
    done
    echo "$kind $(wc -l <"$out")"
done
rm -f "$data/page.json"
