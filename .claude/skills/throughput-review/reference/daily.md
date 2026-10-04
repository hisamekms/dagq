# 日次の見直しのコマンドと読み方

[SKILL.md](../SKILL.md) の「3. 日次の見直し」のコマンドと読み方。

```sh
db=$(~/.local/bin/dagq locate | jq -r .db); echo "$(dirname "$db")/reports/index.html"   # 人がブラウザで開くもの（session からは開かない）
kpi=$(mktemp); ~/.local/bin/dagq kpi --period day --last 7 > "$kpi"
jq -r '.targets[] | select(.state != "ok") | "\(.kpi)\t\(.stratum)\t\(.state)\tsince=\(.breach_since)"' "$kpi"
jq -r '.periods[-2] as $p | ["landings","slot_usage","landing_utilization","load_per_core","cpu_per_landing","phase.work","phase.validate","phase.wait_to_land","land_phase.verify","first_pass_rate","ask_wait"][] as $k
  | $p.kpis[$k].all as $v | $p.comparison[$k].all as $c
  | "\($k)\tvalue=\($v.value) median=\($v.median) p90=\($v.p90) max=\($v.max) n=\($v.n)\t前日=\($c.previous) 前日比=\($c.verdict)\t7日中央値=\($c.baseline_7d) 7日中央値比=\(if $c.baseline_7d and (($v.value // $v.median) != null) and $c.baseline_7d != 0 then (($v.value // $v.median) / $c.baseline_7d * 100 | round / 100) else null end)"' "$kpi"
```

- `.periods[-1]` は今日（`partial`）なので、見直すのは完了した前日（`.periods[-2]`）。`.periods[-2].comparison` の `verdict`・`ratio`・`delta` は前日（`previous`）との比で、7 日の中央値は `baseline_7d`（比は上の jq が `value`（無ければ `median`）÷ `baseline_7d` で求める）
- 見るのは目標割れ（`state` が `ok` 以外）と外れ値（`p90`・`max`）。外れ値は `~/.local/bin/dagq stats --since <前日の 00:00 JST を UTC で> --until <今日の 00:00 JST を UTC で> --full` の長い run を `timeline RUN` で読み、人の答え待ちを除いた長さも書く
