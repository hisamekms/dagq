---
name: throughput-review
description: この repository の本番 queue の着地の流れ（スループット）を見る手順。人が「流れ見て」「着地どう？」「hourly集計」「毎時の確認」と言ったら毎時の確認（JST の時ごとの着地数と 3h・6h 移動平均・24h 平均、slot・待ち・ask・attention）を行い、平常から外れていれば増減の理由の分析まで続ける。「なぜ？」「なんで減った/増えた？」には増減の理由の分析、「日次の見直し」には日次レポートと kpi の外れ値と目標割れ、「週次の見直し」には毎週の見直し、「mark N の効果を見て」には印の前後比較を行う。利用者向けではなく、この repository を開発・運用する session（inbox・planner・人が開いた session）用。
---

# 着地の流れを見る（throughput review）

根拠は plugin の dagq skill の [reference/kpi.md](../../../plugins/claude-dagq/skills/dagq/reference/kpi.md)（KPI の意味と「Raising throughput: the weekly review」）と、AGENTS.md の「作業中」の `dagq.toml` と KPI の項。この skill はそれをこの repository とこの host に当てはめ、session ごとに手順が揺れないように同じコマンドに固定する（以前は `--limit` の付け忘れで過少に数えたり、`date -j` で時がずれたりした）。

共通の決まり:

- 使うのは固定バイナリ `~/.local/bin/dagq` の読み取りコマンド（`events`・`status`・`stats`・`kpi`・`timeline`・`report --print json`・`locate`）だけ。状態を変えるコマンドは打たない（`mark` は人の指示があるときだけで、この skill の範囲の外）
- コマンドは repository の中（どの worktree でもよい）で打つ。外で打つと queue を解決できない
- `events` の `--limit` を必ず付ける（既定は 100 件で、30 時間ぶんの `run_integrated` は超えうる）
- `created_at` は UTC。JST への変換は jq の `sub("\\.[0-9]+Z$";"Z") | fromdateiso8601 + 9*3600 | strftime(...)` で行い、`date -j` では変換しない（`date` は `--since` の UTC の時刻を作るのにだけ使う）
- 出力は日本語。時間の値は、人の答え待ちを含む値と、除いた値を並べる（除く分: 着地待ちの中の `land_phases.ask`、作業中の答え待ちは `timeline RUN` の `waiting_ask` の区間、全体は kpi の `ask_wait`）
- 1 時間の上下には反応しない。見るのは移動平均で、手を打つかどうかは週次の見直しで決める

## 1. 毎時の確認（「流れ見て」「着地どう？」「hourly集計」）

(a) 着地数の表。直近 6 時間の完了した時と今の途中の時、3h・6h の移動平均、完了した 24 時間の平均、分析を始める条件の判定を出す:

```sh
since=$(date -u -v-30H +%Y-%m-%dT%H:%M:%SZ)   # GNU date なら: date -u -d '30 hours ago' +%Y-%m-%dT%H:%M:%SZ
~/.local/bin/dagq events --kind run_integrated --since "$since" --limit 10000 | jq -r '
  def hour: floor | . - (. % 3600);
  def jst: sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601 + 9*3600;
  def avg(a): if (a|length) == 0 then null else ((a | map(.n) | add) / (a|length) * 10 | round / 10) end;
  (now + 9*3600 | hour) as $cur
  | ([.events[].created_at | jst | hour | tostring] | group_by(.) | map({key: .[0], value: length}) | from_entries) as $c
  | [range(29; -1; -1) | ($cur - . * 3600) as $h | {h: $h, n: ($c[$h|tostring] // 0)}] as $hs
  | ($hs[:-1]) as $done
  | avg($done[-24:]) as $d24
  | [range(3) | (($done | length) - 1 - .) as $i | avg($done[$i-2:$i+1])] as $ma3
  | ($done[-1].n) as $last | avg($done[-7:-1]) as $prev6
  | "時(JST)\t着地\t3h平均\t6h平均",
    ($done | to_entries[-6:][] | .key as $i
      | "\(.value.h | strftime("%m-%d %H時"))\t\(.value.n)\t\(avg($done[$i-2:$i+1]))\t\(avg($done[$i-5:$i+1]))"),
    "\($hs[-1].h | strftime("%m-%d %H時"))（途中）\t\($hs[-1].n)\t-\t-",
    "完了した24時間の平均\t\($d24)\t件/時",
    "判定: 直近の時が6h平均から外れた=\((($last - $prev6) | fabs) >= ([$prev6 * 0.5, 3] | max)) 3h平均が24h平均の7割未満で3時間=\(all($ma3[]; . < $d24 * 0.7)) 着地0件の時=\(any($done[-6:][]; .n == 0))"'
```

- 「6h 平均からのずれ」は、直近の完了した時を、その前の 6 時間（その時を含めない）の平均と比べる
- 表の後に前回の確認（この session の直前の報告）からの増減を 1 行で書く（例: 「3h 平均 4.0 → 5.3」）。前回が無ければ書かない

(b) 今の slot・待ち・ask・attention・claim の保留:

```sh
~/.local/bin/dagq status | jq '{
  slots: [.supervisors[] | .slots | "\(.used)/\(.parallel)"],
  waiting: [.supervisors[] | .waiting | "\(.count)/\(.limit)（戻り待ち \(.returning)）"],
  asks: (.asks | length),
  attention: [.attention[] | .kind // .reason // .] ,
  claim_deferrals: (.claim_deferrals | group_by(.reason) | map({(.[0].reason): length}) | add)}'
```

報告は表と (b) を数行にまとめ、最後に「平常」か「分析する（当たった条件）」を書く。判定がどれも `false` なら分析はせず「平常」とだけ書く。

## 2. 増減の理由の分析

人に言われなくても、毎時の確認の判定が次のどれかに当たれば続けて行う。人が「なぜ？」と聞いたときも行う。

- 直近の確定した 1 時間が 6h 平均から ±50% 以上、かつ 3 件以上ずれた
- 3h 平均が 24h 平均を 30% 以上下回る状態が 3 時間続いた
- 着地 0 件の時間があった（直近 6 時間）

対象の時間（外れた時、下回りが続いた 3 時間など）を UTC の `FROM`・`TO` にして（JST から 9 時間引く）、次を並べる。比べる相手は同じ手順で出した直前の 6〜24 時間。

(a) その時間に着地した run の中身。task の種類と、claim→着地・作業・検証・着地待ちの時間、人の答え待ち、resume:

```sh
FROM=2026-09-28T13:00:00Z; TO=2026-09-28T14:00:00Z
~/.local/bin/dagq stats --since "$FROM" --until "$TO" --full | jq -r '
  "task\tchange\tareas\tclaim→着地(分)\t人待ち除く\t作業\t検証\t着地待ち\tverify\t人の答え待ち\tresume\ttitle",
  (.runs[] | select(.status == "integrated")
   | def m: ./60 | floor;
     def t: sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601;
     [.task_id, (.change // "-"), ((.areas // []) | join(",") | if . == "" then "-" else . end),
      ((.landed_at|t) - (.claimed_at|t) | m), ((.landed_at|t) - (.claimed_at|t) - .land_phases.ask | m), (.work|m), (.validate|m),
      (.wait_to_land|m), (.land_phases.verify|m), (.land_phases.ask|m), .resumes, .title[0:40]] | @tsv)'
```

- 時間はどれも分。「人の答え待ち」は着地待ちの中の ask で、作業中の答え待ちが疑わしい run は `~/.local/bin/dagq timeline RUN` の `waiting_ask` の区間を足し、除いた値も並べる
- 種類の層: task が宣言した `change`（`dagq.toml` の `[tasks] changes` の値。change の無い task と古いバイナリの記録では `-`）と、`dagq.toml` の `[areas]` から求める `areas`（着地 commit の差分から。`[areas]` が無い間と古いバイナリの記録では `-`）で分ける

(b) claim の保留と resume と ask（同じ `FROM`・`TO`）:

```sh
~/.local/bin/dagq events --since "$FROM" --until "$TO" --kind claim_deferred --limit 10000 --full \
  | jq -c '[.events[].payload.reason] | group_by(.) | map({(.[0]): length}) | add'
~/.local/bin/dagq stats --since "$FROM" --until "$TO" --full | jq -c '.claim_holds'
~/.local/bin/dagq events --since "$FROM" --until "$TO" --kind resume_started --kind ask_opened --limit 10000 --full \
  | jq -r '.events[] | [.created_at, .kind, .task_id, ((.payload.reason // .payload.kind // "") | tostring | .[0:80])] | @tsv'
```

- `claim_deferred`（衝突の多いファイルでの保留。reason は `hot_files`）の数と、`stats` の `claim_holds`（load などによる claim の保留。`by_reason.load_average` など、件数と秒）で、slot が空いていたのに claim されなかった時間を見る
- resume と ask は、作業のやり直しと人待ちで slot が埋まっていたかを見る

(c) 結論を分ける。書くのは次のどれか（重なるなら全部）と、その根拠の数字:

- **軽い task の偏り**: 増えた時間の着地が `docs`・`plugin`（作業が短い）に偏り、`runtime` の claim→着地の中央値は前と変わらない → 本当に速くなったのではない
- **本当に速くなった / 遅くなった**: 同じ種類（`runtime` どうし）で claim→着地・作業・検証の中央値が動いた。動いた相では `land_phases`（verify・landing_queue・review など）まで割る
- **入口で詰まった**: slot が空いていた（`slots` の used < parallel）のに `claim_deferred` か `claim_holds`（load）が多い、候補が無い
- **人待ち**: 答え待ちを除くと時間が戻る。ask と attention が溜まっていた
- **着地の直列で詰まった**: 着地待ちの中の `landing_queue` が伸びた、verify が長い
- **一時的な事象**: supervisor の停止・入れ替え（`update_installed` など）、resume の集中

分析は報告だけにする。手を打つ（設定を変える、task を足す）かどうかは週次の見直しで、1 つだけ決める。

## 3. 日次の見直し（「日次の見直し」）

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
- 報告は目標割れの一覧、外れ値の run と理由、前日の着地数と 7 日の中央値との比

## 4. 毎週の見直し（「週次の見直し」）

plugin の [reference/kpi.md](../../../plugins/claude-dagq/skills/dagq/reference/kpi.md) の「Raising throughput: the weekly review」の 1〜5 を、`~/.local/bin/dagq kpi --period week --last 4`（週）と `--period day`（日）で、この host の前提に当てはめて行う。

この host の前提（変わったらここも直す）:

- 8 コア / 16GB。supervisor は `--parallel 3`（AGENTS.md の `up` のコマンド）
- `dagq.toml` の `[run.env]`: `CARGO_BUILD_JOBS = "4"`（worker 3 本と integrate 1 本で合計 16 並列 = コア数の 2 倍を目安）、`RUST_TEST_THREADS = "8"`、`NEXTEST_TEST_THREADS = "8"`、`RUSTC_WRAPPER = "sccache"`
- 着地は 1 本ずつ直列で、runtime の task の検証（`cargo llvm-cov nextest`）が 1 件あたり数分〜10 分かかる

当てはめ:

1. **算数の確認**: 着地数/時 ≈ `slot_usage` × 3 ÷ 1 件あたりの枠の時間（`phase.work` + `phase.validate` + `phase.wait_to_land` の平均から人の答え待ちを引いたもの）。両辺が大きくずれたら、先に記録か読み方を疑う
2. **制約を 1 つ特定**: `slot_usage` が 1 に張り付き、`load_per_core` の p90 が 1 以上（8 コアで load 8 以上）なら CPU が制約。`--parallel` を上げず、`cpu_per_landing`（`.rustc`・`.cargo`・`.claude` の内訳）を下げる手を探す。`landing_utilization` が 1 に近いか `.peak` が 1 で `landing_queue_depth` > 0 なら着地の直列が制約（1 件の検証が約 6〜10 分なら 6〜10 件/時で頭打ち）。`ask_wait` が大きければ人待ち
3. **枠の時間のパレート**: `phase.work`・`phase.validate`・`phase.wait_to_land` を中央値 × 件数の大きい順に並べ、一番上だけを割る（`land_phase.*`、`phase.startup` は `work` の内側）
4. **外れ値**: p90・max と、`stats --full` の長い run を `timeline RUN` で読む。`first_pass_rate` と `verification_failed_rate` でやり直しの量を見る
5. **1 つだけ手を打ち、印を打つ提案をする**: 候補を 1 つに絞って人に提案する（設定・運用・host の変更は人が決め、効いた時点で人か planner が `dagq mark '<label>' --note '...'` を打つ。build・`--parallel`・Claude Code・`[run.env]` の変化は runtime が印にする）。`[run.env]` の値を変えるなら AGENTS.md の「作業中」の理由と合わせて見直す

報告は 1〜4 の数字（人の答え待ちを含む値と除いた値）、特定した制約、提案する 1 つの手とそれを確かめる KPI。

## 5. 印の効果を見る（「mark N の効果を見て」）

```sh
~/.local/bin/dagq kpi --compare N --area runtime            # --window 7（日）が既定
~/.local/bin/dagq kpi --compare N --change <値>             # change で絞るとき
~/.local/bin/dagq kpi --compare N --area <名前>              # 別の area で読むとき
```

- 前後比較は `--area runtime`（か `--change`）の層で読み、`all` では読まない（task の種類の混ざり方で動く）
- `confounders`（間の他の印）を名指し、`split.separable` が `false` なら結論を出さない。`[kpi] min_samples` に満たない、または `partial` の区間は判定しない
- 見る KPI は印の目的のもの（例: `[run.env]` の変更なら `cpu_per_landing`・`load_per_core`・`phase.work`・`land_phase.verify`）と、着地数/時・`slot_usage`
