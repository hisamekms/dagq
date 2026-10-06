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

## `dagq::it` の本数と時間（goal 118）

前日（JST）の本番の関門の log の test の本数と時間の合計を、前の 7 日の各日と並べる。log は `<runs_dir>/*/integrate-*-verify-*.log` のうち `Summary` を含むもの（test 段の log）で、日は log の最終更新の JST の日付。`runs_dir` は固定バイナリの `~/.local/bin/dagq locate` の JSON の値から取る（`dagq --resolve` は plugin の wrapper の機能で、固定バイナリには無い）。`runs_dir` が無い（client mode の session）と読めないので、inbox か人が流す。repository の root で打つ（`scripts/slow-tests.sh` の表の読み方は [Slow tests](../../../../docs/design/slow-tests.md)）。1 分ほどかかる。zsh では `$logs` が語に分かれないので、`sh` に渡して流す。

```sh
sh -s <<'SH'
runs=$(~/.local/bin/dagq locate | jq -r '.runs_dir // empty')
[ -d "$runs" ] || { echo "runs_dir が無い（client mode）" >&2; exit 1; }
idx=$(mktemp)
for f in "$runs"/*/integrate-*-verify-*.log; do
  grep -q 'Summary \[' "$f" && TZ=Asia/Tokyo stat -f '%Sm %N' -t %F "$f"
done | sort > "$idx"
printf 'day\tlogs\tit本数\tit合計\tlib本数\tlib合計\tSummary中央値\t失敗本数\n'
for day in $(cut -d' ' -f1 "$idx" | uniq | tail -9); do
  logs=$(awk -v d="$day" '$1 == d { print $2 }' "$idx")
  md=$(sh scripts/slow-tests.sh --min-ratio 0.9 $logs)
  failures=$(printf '%s\n' "$md" | awk '/^採用した log の失敗 [0-9]+ 本/ { print $4 }')
  row() { printf '%s\n' "$md" | awk -F'|' -v b="\`$1\`" '$2 == " " b " " { gsub(/ /, "", $3); gsub(/ /, "", $4); printf "%s\t%s", $3, $4; found = 1 } END { if (!found) printf "-\t-" }'; }
  med=$(for f in $logs; do grep 'Summary \[' "$f" | tail -1 | sed 's/^[^[]*\[ *\([0-9.]*\)s\].*/\1/'; done | sort -n | awk '{ v[NR] = $1 } END { if (NR) print (NR % 2) ? v[(NR + 1) / 2] : (v[NR / 2] + v[NR / 2 + 1]) / 2 }')
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$day" "$(printf '%s\n' "$logs" | wc -l | tr -d ' ')" "$(row 'dagq::it')" "$(row dagq)" "$med" "$failures"
done
rm -f "$idx"
SH
```

- 列は日・log の本数・`dagq::it` の本数と合計（秒）・`dagq`（lib の unit test）の本数と合計・test 段の `Summary` の秒の中央値・時間の表から除いた失敗本数（全 binary、log ごとに数えた合計）。その日の表に binary の行が無ければ `-`。本数と合計は、その日の log をまたいだ test ごとの中央値で数える（その日に名前の変わった・足された test は別の test として数えるので、本数は 1 回の実行より多くなりうる）
- `Summary` のある log は `--min-ratio 0.9` に依らず採用される（Summary の無い log だけ終了本数で判定する）。binary の本数と秒は成功した test だけで、失敗の多い日は減りうる。日ごとの比較では TSV の「失敗本数」列も読み、失敗を除いた影響を報告する
- 最後の行が今日（途中）なら読まない。読むのは前日の行と、その前の 7 日の行（log の少ない日は揺れが大きいので本数も書く）。報告には前日の値と 7 日の中央値との比を並べる
- 読み方: `dagq::it` の本数が増えずに合計が増えたら、遅い test が足されたか既存の test が遅くなった（`slow-tests.sh` の上位の表を前の日と比べる）。本数と合計が同じ比で増えたら機能の追加に伴う test の増え方。lib の本数が増えて it の本数が減るのは移し替え（goal 68 の流れ）。`Summary` だけが増えたら test でなく host の負荷か並列度を疑う
- 増え直しを見つけたら: `dagq::it` の合計が前の 7 日の中央値より 10% 以上多い日が 2 日続いたら、両日の値・上位の表で増えた test・その間に着地した run を添えて、planner への request（`dagq request add`）にする。1 日だけの増えは報告に書くだけにする
