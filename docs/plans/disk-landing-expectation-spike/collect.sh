#!/bin/sh
# 読むだけ: disk-landing-expectation-spike の event を kind ごとに JSONL へ頁送りで読む。
# 使い方: collect.sh DATA_DIR UNTIL
# 大きさの標本と run の始まりと終わりの kind は T（2026-11-08T16:25:44Z）の 14 日前から（想定と期間の窓の温め）、ほかは T の 2 日前から
# （T に動いていた run の状態と claim）読み、UNTIL まで（期間内に claim した run の終わりと大きさ）読む。
# 期間 [T, E) の切り分けは replay.py が行う。
set -eu
data=$1
until=$2
mkdir -p "$data"
read_kind() {
    kind=$1
    since=$2
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
}
for kind in build_outputs_removed worktree_removed scratchpad_removed run_tmp_removed run_claimed session_closed run_integrated; do
    read_kind "$kind" 2026-10-25T16:25:44Z
done
for kind in run_phase_changed claim_held claim_resumed landing_held landing_resumed \
    supervisor_stopped auto_repaired verification_command integration_started; do
    read_kind "$kind" 2026-11-06T16:25:44Z
done
rm -f "$data/page.json"
