---
name: throughput-review
description: この repository の本番 queue の着地の流れ（スループット）を見る手順。人が「流れ見て」「着地どう？」「hourly集計」「毎時の確認」と言ったら毎時の確認（JST の時ごとの着地数と 3h・6h 移動平均・24h 平均、slot・待ち・ask・attention）を行い、平常から外れていれば増減の理由の分析まで続ける。「なぜ？」「なんで減った/増えた？」には増減の理由の分析、「日次の見直し」には日次レポートと kpi の外れ値と目標割れ、「週次の見直し」には毎週の見直し、「mark N の効果を見て」には印の前後比較を行う。利用者向けではなく、この repository を開発・運用する session（inbox・planner・人が開いた session）用。
---

# 着地の流れを見る（throughput review）

根拠は plugin の dagq skill の [reference/kpi.md](../../../plugins/claude-dagq/skills/dagq/reference/kpi.md)（KPI の意味と「Raising throughput: the weekly review」）と、[operations.md](../../../docs/development/operations.md) の「KPIの読み方と印」と「`[run.env]`とtestの並列度の置き場」。この skill はそれをこの repository とこの host に当てはめ、session ごとに手順が揺れないように同じコマンドに固定する（以前は `--limit` の付け忘れで過少に数えたり、`date -j` で時がずれたりした）。

共通の決まり:

- 使うのは固定バイナリ `~/.local/bin/dagq` の読み取りコマンド（`events`・`status`・`stats`・`kpi`・`timeline`・`report --print json`・`locate`）だけ。状態を変えるコマンドは打たない（`mark` は人の指示があるときだけで、この skill の範囲の外）
- コマンドは repository の中（どの worktree でもよい）で打つ。外で打つと queue を解決できない
- `events` の `--limit` を必ず付ける（既定は 100 件で、30 時間ぶんの `run_integrated` は超えうる）
- `created_at` は UTC。JST への変換は jq の `sub("\\.[0-9]+Z$";"Z") | fromdateiso8601 + 9*3600 | strftime(...)` で行い、`date -j` では変換しない（`date` は `--since` の UTC の時刻を作るのにだけ使う）
- 出力は日本語。時間の値は、人の答え待ちを含む値と、除いた値を並べる（除く分: 着地待ちの中の `land_phases.ask`、作業中の答え待ちは `timeline RUN` の `waiting_ask` の区間、全体は kpi の `ask_wait`）
- 1 時間の上下には反応しない。見るのは移動平均で、手を打つかどうかは週次の見直しで決める

## 1. 毎時の確認（「流れ見て」「着地どう？」「hourly集計」）

(a) 着地数の移動平均と分析を始める条件の判定、(b) 今の slot などを出す。コマンドと読み方（6h 平均からのずれの比べ方、前回からの増減の書き方）は [reference/hourly.md](reference/hourly.md)。

報告は表と (b) を数行にまとめ、最後に「平常」か「分析する（当たった条件）」を書く。判定がどれも `false` なら分析はせず「平常」とだけ書く。

## 2. 増減の理由の分析

人に言われなくても、毎時の確認の判定が次のどれかに当たれば続けて行う。人が「なぜ？」と聞いたときも行う。

- 直近の確定した 1 時間が 6h 平均から ±50% 以上、かつ 3 件以上ずれた
- 3h 平均が 24h 平均を 30% 以上下回る状態が 3 時間続いた
- 着地 0 件の時間があった（直近 6 時間）

対象の時間を `FROM`・`TO` にして、(a) その時間に着地した run の中身、(b) claim の保留と resume と ask、(トークン) トークン消費を並べ、(c) 結論を分ける。対象の時間の決め方、コマンドと読み方、結論の分け方は [reference/analysis.md](reference/analysis.md)。

分析は報告だけにする。手を打つ（設定を変える、task を足す）かどうかは週次の見直しで、1 つだけ決める。

## 3. 日次の見直し（「日次の見直し」）

目標割れと外れ値と、本番の関門の `dagq::it` の本数と時間を見る。コマンドと読み方は [reference/daily.md](reference/daily.md)。

報告は目標割れの一覧、外れ値の run と理由、前日の着地数と 7 日の中央値との比、`dagq::it` の本数と合計の前の 7 日との並び。

## 4. 毎週の見直し（「週次の見直し」）

plugin の [reference/kpi.md](../../../plugins/claude-dagq/skills/dagq/reference/kpi.md) の「Raising throughput: the weekly review」の 1〜5 を、`~/.local/bin/dagq kpi --period week --last 4`（週）と `--period day`（日）で、この host の前提に当てはめて行う。

この host の前提と当てはめの 1〜4（算数の確認・制約を 1 つ特定・枠の時間のパレート・外れ値）は [reference/weekly.md](reference/weekly.md)。

5. **1 つだけ手を打ち、印を打つ提案をする**: 候補を 1 つに絞って人に提案する（設定・運用・host の変更は人が決め、効いた時点で人か planner が `dagq mark '<label>' --note '...'` を打つ。build・`--parallel`・Claude Code・`[run.env]` の変化は runtime が印にする）。`[run.env]` の値を変えるなら [operations.md](../../../docs/development/operations.md) の「`[run.env]`とtestの並列度の置き場」の理由と合わせて見直す

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
