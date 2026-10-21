---
id: plan-measurement-ledger-parallel
type: plan
title: 台帳と今の stats を 7 日並走させた結果（unattributed の割合・run ごとの差の理由・照合）と、段 2 の基準の判定
status: completed
created: 2026-10-21
owners:
  - hisamekms
tags:
  - measurement
related:
  - design-measurement
---

# 台帳と今の stats を 7 日並走させた結果（unattributed の割合・run ごとの差の理由・照合）と、段 2 の基準の判定

goal 71 の段 2 の完了条件（unattributed < 2%、差がすべて説明できる）を確かめる 1 回きりの測定（task 1668）。
台帳を作る係と比較の読み取り（`dagq stats --ledger`）を足した task 1667 の build が本番で動き始めてから 168 時間の期間を、本番の固定バイナリの状態を変えないコマンドだけで読んだ。
測って書くだけで、段 3 を解放するかは関門の task 1678 が決める。
src/・tests/ は変えていない。

## 要点

- **境目は event 180803（`supervisor_started`、build `5090da6a`、2026-10-13T12:00:52.847Z）、期間は [2026-10-13T12:00:52Z, 2026-10-20T12:00:52Z)。**
  終端の event から数えた期間に終わった run は 253 件、task は 527 件で、どれも台帳の行を持ち、行の無い対象・final でない対象・push_pending・dirty はどれも 0 件。
  比較の読み取りの照合（`reconcile.kinds`）は run 261 件・task 539 件と多く出るが、差の 8 run・12 task はどれも legacy の行で、終わりが期間の外なのに最後の入力（後片付けなど）が期間にあるため数えられたもの。
- **unattributed の割合は run が 1.5 秒 ÷ 4,859,700 秒 = 0.00003%、task が 51,501 秒 ÷ 93,998,709 秒 = 0.055%。**
  どちらも 2% を大きく下回る。
  run の unattributed は 253 行のうち 249 行にあるが、どれも 0.1 秒未満（記録の間の数ミリ秒の隙間）。
  task の unattributed のほぼ全部（6 件・51,500 秒）は「run が終わった後に task が `in_progress` のまま run を持たない時間」。
- **比較の読み取りが `unexplained` とした run の値の差は 224 run・735 個。**
  timeline と events を読み直し、今の `LandClock` の区間と台帳の区間を突き合わせると、735 個すべてが「どの区間を今と台帳が別の名前に数えたか」で説明できた（説明の残りは各 3 秒以内）。
  最大は台帳の `awaiting_slot` を台帳の読み（`read::land_phase`）が `exit` に数え、今の `LandClock` が `landing_queue` に数える分（217 run・218,270 秒）。
  次は今の `LandClock` が開いている `approve_landing` の ask を忘れる誤り（`exit`・`landing_queue`・`resume` に入る。合わせて 212,460 秒）。
- **run・task を持たない欄は 13 欄のうち 4 欄が違う（理由はどれも `records`）。**
  `draft_flow`・`auto_repairs`・`planner_routes` の差は、終わっていない task が台帳の行を持たない（行は終わった task だけに作る）ことで全部説明できた。
  `sessions` の active（transcript の turn の時間）の差は、期間の終わりに開いていた 5 span（worker 1・revise 2・runtime_planner 2）の、終わりの後に記録された turn で説明できた（この読みの時点の比較の読み取りは `records` に数え、今は理由の分類 `turns_after_end` に分ける）。
- **session の行 1 件（`inbox//session/23`）が final でない。**
  inbox と planner の span は閉じてから 7 日の猶予の後にしか final にならない（設計どおり）ためで、行の欠けではない。
- **host の欄は期間の 8 日（JST の日を期間で切ったもの）のどれも、全列の差が 0（標本は全部両方にある）。**
- **段 2 の基準は 5 つのうち 3 つを満たし、2 つ（不明の差 0 件・push_pending 以外の final でない対象 0 件）を満たさない。**
  直す打ち手は receipt の follow_ups（improvement）に書いた。
- 読んだ時刻: 主な読み取りは 2026-10-21T02:59Z の固定バイナリ `79077fa1`（`LEDGER_V` 3）。
  2026-10-21T03:14:13Z に本番が `b31d8470` に自動更新され、`LEDGER_V` が 4 に上がった。
  その後の読み（host の日ごとの読み）では台帳の行が古い版として dirty に出る（作り直しの途中）。
  照合の数（dirty 0 件）は更新の前の読みのもの。

## 境目の決め方

1. task 1667 の着地の commit: この run の起点の origin/main（`b31d8470fc273dbf47933b40ca1cc8d678631c51`）で
   `git log origin/main --format='%H%x09%(trailers:key=Dagq-Task,valueonly,separator=%x2C)'` を読み、値をカンマで分けた要素が `1667` と完全一致する commit を集めた。
   一致は 1 件で `5090da6a870fa444ab513d5bbe649d639ab3c357`（commit の時刻 2026-10-12T20:45:03Z）。
2. 候補の event: `dagq events --full --kind update_installed --kind supervisor_started --limit 500 --after <cursor>` を cursor 0 から返った `cursor` で進め、空のページまで 4 ページ読んだ（1,382 件）。
   build commit は `update_installed` が `payload.commit`、`supervisor_started` が `payload.dagq_version` の「+」の後ろ。
   取れない event は 0 件。
3. 各候補に `git merge-base --is-ancestor 5090da6a… <build commit>` を打った: exit 0 が 300 件、exit 1 が 1,081 件、それ以外が 1 件。
   それ以外の 1 件は event 105641（2026-10-05T22:09:15Z）の `a8056a3a….dirty` で、`git fetch` の後も object 名として読めない（exit 128）。
   「.dirty」を外した `a8056a3a…` は exit 1 で、2026-10-05 の build なので 1667 を含まない。
4. exit 0 のうち `created_at` の最も早いものは **event 180803（`supervisor_started`、2026-10-13T12:00:52.847Z、build `5090da6a…`）**。
   この build の `update_installed` は無く、次は同じ build の event 180832（`supervisor_started`、12:00:59.993Z）。
5. 境目から測った時刻（2026-10-21T02:58:10Z）までの経過は 182.9 時間で、168 時間を過ぎている。

## 期間と読み取り

- 期間: [2026-10-13T12:00:52Z, 2026-10-20T12:00:52Z)（UTC、168 時間。境目の 0.847 秒は秒に切り捨てた）。
  `--since` は「その後に終わったもの」、`--until` は「その時刻まで」を取るので、両端の 1 秒未満の含み方は半開区間と少し違う。
  終わりが両端の 1 秒に入る run と task は無かった（下の再計算で確かめた）。
- 主な読み取り（2026-10-21T02:59Z、固定バイナリ `79077fa1`）:
  `dagq stats --ledger --full --since 2026-10-13T12:00:52Z --until 2026-10-20T12:00:52Z`。
  出力の `window_ms` は [1791892852000, 1792497652000]。
  `runs`（253 run）・`fields`（13 欄）・`host`（期間全体と kpi の直近 7 日）・`reconcile` を使った。
- 今の stats: `dagq stats --full --since 2026-10-13T12:00:52Z --until 2026-10-20T12:00:52Z`（253 run。比較の読み取りの `runs` と同じ集合）。
- 比較の読み取りは run ごとの unattributed（整数秒）しか出さず、task の unattributed と blocker・holds・slot の内訳を出す読み取りは固定バイナリに無い。
  そこで台帳の畳む関数（`domain::ledger::fold` の `fold_run`・`fold_task`）を写した script で、同じ events から run と task の区間を畳み直した（[fold.py](measurement-ledger-parallel/fold.py)）。
  入力は [fetch.py](measurement-ledger-parallel/fetch.py) が打つ `dagq events --full --all --task <ID> --limit 1000 --after <cursor>`（task 543 件。ID は主な読み取りの `reconcile.rows` の task の key、`runs[].task_id`、照合にだけある legacy の run 8 件の task。(1) の対象の task はすべて含む（`dagq events --full --all --run <ID> --limit 3` の `task_id`）を合わせたもの。期間に終わった task 539 件と、期間に終わった run の task。run の events を含む）と、`dagq events --full --kind push_finished --kind push_failed --since 2026-10-01 --limit 5000`（915 件）。
  畳み直した run の unattributed は、比較の読み取りが出す台帳の値（秒）と 253 run のすべてで一致した（どれも整数秒では 0 なので、この照合は弱い）。
  task の値と blocker・holds の内訳を台帳の出力と突き合わせる読み取りは無く、畳む関数を写した script の値として読む。
- 今の `land_phases` の区間: 今の `stats::landing::LandClock` を写した script（[landclock.py](measurement-ledger-parallel/landclock.py)）で、各 run の区間を作った。
  区間の合計は今の stats の `land_phases`（exit・review・revise・ask・resume・landing_queue・conflict・e2e_wait）と 232 run のすべてで 2 秒以内に一致した（[check_lc.py](measurement-ledger-parallel/check_lc.py)）。
- 作り直せる範囲: script は [measurement-ledger-parallel/](measurement-ledger-parallel/) にあり、上の読み取りの出力（`ledger.json`・`tasks/<ID>.jsonl`・`pushes.jsonl`）を置いた directory で `python3 -I <script>` として動く。
  出力は大きい（`ledger.json` 4.9 MB、events 54,583 行）ので commit していない。
  上の読み取りを打ち直せば作り直せるが、本番の EventStore の保持と、台帳の版の変化（`LEDGER_V`）で値は変わりうる。

## (1) 対象と台帳の行の照合

run と task の対象は、比較の読み取りの照合に頼らず、終端の event から数え直した（[targets.py](measurement-ledger-parallel/targets.py) `ledger.json targets.json`）。
終端は ADR-t1662-1 決定 16 のとおり。

- 着地した run: `run_integrated` の後の最初の `push_finished` か `push_skipped`。
  その前に `push_failed` があり、どちらも後に無ければ push_pending（`push_failed` の時刻で別に数える）。
- 着地しない run（失敗・取り消し・interrupted・閉じた `already_satisfied`）: 最後の `run_unfinished_changed` の `unfinished: false`（後に未完了へ戻る `unfinished: true` も着地も無いもの）。
- task: 最後の `task_status_changed` の `completed` か `canceled`（後にそこから出る動きが無いもの）。

読んだコマンド（2026-10-21T03:55Z〜04:05Z、固定バイナリ `b31d8470`、どれも状態を変えない）:
`dagq events --full --kind push_finished --kind push_skipped --kind push_failed --since 2026-10-13T12:00:52Z --until 2026-10-20T12:00:52Z`（232 件、すべて `push_finished`）、
`dagq events --full --kind run_unfinished_changed --since 2026-10-13T12:00:52Z --until 2026-10-20T12:00:52Z`（257 件）、
`dagq events --full --kind task_status_changed --since 2026-10-13T12:00:52Z --until 2026-10-20T12:00:52Z`（1,781 件）を `--limit 1000 --after <cursor>` で最後のページまで。
候補の run ごとに `dagq events --full --all --run <ID>`、task ごとに `dagq events --full --all --task <ID> --kind task_status_changed` で後の動きを確かめた。
`--since` は「その時刻から」、`--until` は「その時刻より前」なので、event の数え方は期間の半開区間と同じ。
行の有無・final・push_pending・dirty は、その対象ごとに主な読み取り（2026-10-21T02:59Z）の `reconcile.rows` を引いた。

| 種類 | 終端の event から数えた対象 | 台帳の行 | 行の無い対象 | final でない（push_pending を除く） | push_pending | dirty の残る対象 |
|---|---:|---:|---:|---:|---:|---:|
| run | 253（着地 232・着地しない 21） | 253 | 0 | 0 | 0 | 0 |
| task | 527 | 527 | 0 | 0 | 0 | 0 |

- 対象の 253 run は、supervisor の `ended` の記録（`run_phase_changed` の `phase: ended`）が期間にある run の集合と同じで、比較の読み取りの `runs`（253 run）とも同じ。
  着地しない run の終端の event と台帳の終わり（`ended` の記録、復旧の job の後）は最大 5.4 時間ずれるが（`516c0bc4-682c-4627-addc-72398f8039a1`）、どちらで数えても期間の中。
- 期間の後に未完了へ戻った run と、期間の後に `completed`・`canceled` から出た task は無かった。
- 比較の読み取りの照合（`reconcile.kinds`）は run 261 件・task 539 件を出すが、終端の event から数えた対象より 8 run・12 task 多い。
  多い分はどれも legacy の行で、照合は legacy の行の終わりを最後の入力の event の時刻とするため（`compare::ended_at`）、終わりが期間の外でも期間に後片付けの event があれば数える。
  例: task 32 の `completed` は event 1068（2026-09-22T12:32:14Z）、run `b053101e-08f5-4511-be06-524e63f685ca` の期間の event は event 248079（2026-10-19T20:39:25Z、`worktree_removed`）。
  - run（8）: `0ce0d8a2-a1d4-49e1-a173-3fc90086c562`・`20c909a5-4ed9-4006-b9ba-6f02ad9bfc33`・`2f664dd2-991a-4584-848f-cd18a7b5cf9c`・`8e2e0094-956a-4bb9-9ec2-b96e67d44f90`・`b053101e-08f5-4511-be06-524e63f685ca`・`c1cf5b09-a068-4e1e-a7bd-c6f70f5e34bd`・`dcf0765c-fffc-4a16-8eeb-7a824dd3cbf4`・`f2ab82e0-20be-4efd-bcbb-af9084d0817b`
  - task（12）: 32・428・708・714・765・800・1394・1715・1818・1847・1920・1942
- 対象のうち legacy（切り替えの前の run を持ち、区間を持たず値を凍らせた行）は task 6 件（631・661・700・784・903・955）で、run は 0 件。
  legacy の行は final で dirty でないが区間を持たないので、(a) と (c) の値に入らない。

session・queue・node は run と task のような legacy の行を持たず、終わりは行そのものの終わり（session の span の閉じ・queue の event・node の 1 時間の区切り）なので、主な読み取りの `reconcile.kinds` をそのまま使う。

| 種類 | 期間に終わった対象 | 台帳の行 | 行の無い対象 | final でない（push_pending を除く） | push_pending | dirty の残る対象 |
|---|---:|---:|---:|---:|---:|---:|
| session | 1,784 | 1,784 | 0 | 1（`inbox//session/23`） | 0 | 0 |
| queue | 16,128 | 16,128 | 0 | 0 | 0 | 0 |
| node | 159 | 159 | 0 | 0 | 0 | 0 |

- `inbox//session/23` が final でないのは、inbox と planner の span の猶予が閉じてから 7 日（`TOKEN_CUT_CLOSED_WINDOW_MS`、`settle::grace`）で、期間の後半に閉じた inbox の span はまだ猶予の中にあるため。
  期間に閉じた inbox の span は 2026-10-15T22:57:32Z（event 197884、`reason: logout`）で、読み取りの時点で閉じてから 5 日。
- 更新の後の状態: 2026-10-21T03:14:13Z の自動更新（`b31d8470`、`LEDGER_V` 4）の後に打った日ごとの読み（下の (e)）では、node の行がすべて `up_to_date: false`（`folded` と `reach` は等しく、版だけが古い）で dirty に数えられた。
  新しい版で作り直し終わるまで、照合の dirty は版の差を数える。

## (a) unattributed の割合

対象の行は (1) の対象（run 253 件・task 527 件）の行のうち final で dirty でなく legacy でない行: run 253 行、task 521 行（対象の数との差は legacy の task 6 行。dirty の残る行は 0 件）。
所要時間は run が claim から終わりまで、task が `task_created` から終わりまで（区間の和で、畳む関数の区間は重ならず全体を覆う）。
人の答え待ちは引いていない（(c) の blocker: human に別に出す）。

| 種類 | 行の件数 | unattributed の秒（分子） | 所要時間の秒（分母） | 割合 |
|---|---:|---:|---:|---:|
| run | 253 | 1.545 | 4,859,699.862 | 0.00003% |
| task | 521 | 51,501.070 | 93,998,709.483 | 0.055% |

- run の unattributed: 253 行のうち 249 行で 0 秒より大きく、最大は 0.095 秒（`1f329f87-24d7-4b61-b033-37b1d16ae8b5`）。
  claim と最初の `run_phase_changed` の間の数ミリ秒で、比較の読み取りの整数秒ではどれも 0。
- task の unattributed の 99.99% は 6 件の「`in_progress` で run を持たない時間」（畳む関数の `Composer::status` の既定の枝）。
  run が終わってから、人か planner が task を `ready` に戻すか取り消すまで。

| task | 区間（UTC） | 秒 | 終わらせた動き |
|---:|---|---:|---|
| 2394 | 2026-10-19T15:53:22Z〜21:53:17Z | 21,595 | `in_progress` → `ready` |
| 1672 | 2026-10-17T00:02:13Z〜03:27:12Z | 12,298 | `in_progress` → `ready` |
| 2163 | 2026-10-17T00:02:13Z〜03:27:12Z | 12,298 | `in_progress` → `ready` |
| 2138 | 2026-10-20T10:07:32Z〜11:11:00Z | 3,808 | `in_progress` → `canceled` |
| 2270 | 2026-10-14T05:01:11Z〜05:22:35Z | 1,284 | `in_progress` → `ready` |
| 1821 | 2026-10-19T09:17:52Z〜09:21:27Z | 216 | `in_progress` → `canceled` |

コマンド: [fold.py](measurement-ledger-parallel/fold.py) `tasks pushes.jsonl ledger.json fold.json` の後に [agg.py](measurement-ledger-parallel/agg.py) `fold.json ledger.json`、区間の中身は [why.py](measurement-ledger-parallel/why.py) `2394 1672 2163 2138 2270 1821`。

## (b) run ごとの差

比較の読み取りの `runs[].fields`（253 run）を、値の名前と理由の分類ごとに数えた（差が 0 で理由の無い値は除く。両方が無い値は差を持たないので除く）。
`push` は今の `wait_to_land` に無い（台帳の値で別に出す）ので差に数えない（232 run・合計 1,260 秒）。
コマンド: [diffs.py](measurement-ledger-parallel/diffs.py) `ledger.json`。

| 値の名前 | 理由の分類 | run の件数 | 差の秒の合計 | 差の秒の中央値 |
|---|---|---:|---:|---:|
| land_phases.ask | boundary | 3 | 4 | 1 |
| land_phases.ask | exit_held_ask | 12 | 151744 | 8995 |
| land_phases.ask | unexplained | 43 | -2424 | 48 |
| land_phases.conflict | boundary | 1 | -2 | -2 |
| land_phases.conflict | unexplained | 12 | -722 | -27 |
| land_phases.e2e | boundary | 10 | -10 | -1 |
| land_phases.e2e_wait | unexplained | 6 | -624 | -125 |
| land_phases.exit | boundary | 28 | 43 | 2 |
| land_phases.exit | exit_held_ask | 12 | -112072 | -6039.5 |
| land_phases.exit | unexplained | 191 | 231148 | 61 |
| land_phases.landing_queue | boundary | 98 | -134 | -1 |
| land_phases.landing_queue | unexplained | 133 | -253917 | -50 |
| land_phases.rebase | boundary | 80 | -91 | -1 |
| land_phases.rebase | unexplained | 24 | -91 | -3 |
| land_phases.resume | boundary | 4 | -8 | -2 |
| land_phases.resume | unexplained | 53 | -55740 | -104 |
| land_phases.review | boundary | 39 | 19 | 1 |
| land_phases.review | unexplained | 15 | -12 | -3 |
| land_phases.revise | boundary | 22 | -38 | -2 |
| land_phases.revise | unexplained | 142 | 41668 | -47.5 |
| land_phases.verify | boundary | 117 | 143 | 1 |
| land_phases.verify | unexplained | 35 | 1218 | 3 |
| wait_to_land | boundary | 77 | -80 | -1 |
| wait_to_land | unexplained | 3 | -11 | -4 |
| work | boundary | 36 | 40 | 1 |
| work | unexplained | 78 | 3364 | 14.5 |

- `startup` は 253 run のすべてで一致した（差 0）。
- `work` の `unexplained` 78 件のうち 2 件は今の値が無い（差を 0 として合計した）。
- `exit_held_ask` は今の `exit` が `approve_landing` の答え待ちを持ち、台帳が `ask` に持つ分で、12 run の `exit` と `ask` に逆の符号で出る（`ask` +151,744 秒と `exit` −112,072 秒の差 39,672 秒は、同じ run の今の `resume`（33,704 秒）と `landing_queue`（5,262 秒）などに入った分。分類は両側がそれぞれ境目の幅より動いたことだけを見る）。

### 理由が不明の差の由来

`unexplained` の 735 個（224 run）を、今の区間（[landclock.py](measurement-ledger-parallel/landclock.py)）と台帳の区間（[fold.py](measurement-ledger-parallel/fold.py)）の重なりで説明した。
`land_phases` の値は、今の工程 S と台帳の工程 R（その `land_phases` の名前 B）が重なる時間のうち S と B が違うものが、S から B へ移った時間になる。
これに今と台帳の始まりの差（今は最初の `validation_finished`、台帳は最初の `validating` の終わり）を足すと、735 個のすべてで比較の読み取りの差と 3 秒以内で一致した（[perrun.py](measurement-ledger-parallel/perrun.py) の検査）。
`work` と `wait_to_land` は値の区切りの違い（下の W1・W2・T）で説明した（[work.py](measurement-ledger-parallel/work.py)）。

今と台帳が別の名前に数えた区間（1 秒以上の重なりを持つ run の件数と秒の合計。[matrix.py](measurement-ledger-parallel/matrix.py) `ledger.json`）:

| 由来 | 今（LandClock）の工程 | 台帳の工程 | 台帳の land_phases | run の件数 | 秒 |
|---|---|---|---|---:|---:|
| A | landing_queue | awaiting_slot | exit | 217 | 218,270 |
| B | exit | landing_answer | ask | 16 | 119,666 |
| D | ask | revise | revise | 6 | 58,514 |
| B | landing_queue | landing_answer | ask | 56 | 51,471 |
| B | resume | landing_answer | ask | 16 | 41,323 |
| F | resume | landing_queue | landing_queue | 5 | 15,779 |
| C | revise | validating | exit | 164 | 8,879 |
| G | revise | returning | exit | 7 | 6,028 |
| H | ask | resume | resume | 5 | 4,509 |
| C | resume | validating | exit | 52 | 2,337 |
| G | revise | exiting | exit | 5 | 1,794 |
| F | resume | landing | verify | 12 | 822 |
| C | conflict | validating | exit | 13 | 725 |
| A | e2e_wait | awaiting_slot | exit | 5 | 689 |
| Z | ask | landing | verify | 1 | 285 |
| R | rebase | landing | verify | 91 | 188 |
| K | revise | waiting | ask | 7 | 183 |
| G | landing_queue | exiting | exit | 3 | 121 |
| J | exit | review | review | 25 | 112 |
| G | review | exiting | exit | 13 | 98 |
| Z | landing_queue | awaiting_e2e | e2e_wait | 1 | 66 |
| C | ask | validating | exit | 4 | 8 |
| C | review | validating | exit | 1 | 4 |
| Z | rebase | landing_queue | landing_queue | 1 | 1 |

由来の分類（どちらの数え方が誤りかを含む）:

- **A（台帳の読みの名前の抜け）**: 着地の slot を待つ間、supervisor は `awaiting_slot`（blocker: queue）を記録するが、台帳の読みの `read::land_phase` は `awaiting_slot` を名前に持たず `exit` に数える。
  今の `LandClock` は `landing_queued` の後なので `landing_queue` に数える。
  意味は今の `landing_queue` が合っている。
- **B（今の LandClock の誤り）**: `approve_landing` の答え待ちを台帳は `landing_answer`（`ask`）に持つが、今の `LandClock` は ask の間に他の工程の event（`landing_queued`・`needs_session` など）が来ると開いている ask を忘れ、`exit`・`landing_queue`・`resume` に数える。
  goal 71 が直す誤り（`exit_held_ask` と同じ型で、`exit` 以外に入った分と `exit` でも分類に掛からなかった分）。
- **C（再検証）**: revise・resume・conflict の後の 2 回目以降の検証を台帳は `validating`（読みでは `exit`）に持つが、今の `LandClock` は次の `review_started` まで直前の工程に数える。
- **D（台帳の記録の遅れと抜け）**: revise の間に worker_question が開くと、今の `LandClock` は `ask_opened` で `ask` に入るが、supervisor の `waiting` の記録は遅れるか無い。
  台帳は `waiting` の記録までを `revise` に持つ（`609270cf-…` の 2 回目の質問は 16 分後に `waiting`、`43229830-…` の 2 回目と `609270cf-…` の 1 回目の質問は答えまで `waiting` の記録が無い。6 run で合わせて約 16 時間）。
  ここは今の `LandClock` が合っており、台帳の記録が人の答え待ちを取りこぼしている。
- **F（今の LandClock の resume）**: `needs_session` の後の着地の待ちと着地を、今の `LandClock` は次の工程の event まで `resume` に数え、台帳は `landing_queue`・`landing` に持つ。
- **G（引き渡し）**: session の終わり（`exiting`）と戻り（`returning`）を台帳は工程に持ち、読みは `exit` に数える（`read::land_phase` の既定）。
  今の `LandClock` は直前の工程に数える。
- **H**: 今の `LandClock` が開いている ask の間、台帳は `resume` に持つ（resume の session が動いている）。
- **J**: review の始まりの記録（`review` の `run_phase_changed`）が `review_started` より先に来る分（25 run・合計 112 秒）。
- **K**: revise の間の worker の質問を台帳は `waiting`（`ask`）に持ち、今の `LandClock` は `revise` に数える（今の `LandClock` は ask の後に `revise` に戻す）。
- **R（着地は 1 工程）**: 台帳の着地は `landing` 1 つで、読みは `rebase` を 0 にして `verify` に含める（`read::land_phase` の約束）。
- **Z（各 1 run）**: `ask`→`landing`（285 秒）、`landing_queue`→`awaiting_e2e`（66 秒）、`rebase`→`landing_queue`（1 秒）の小さな食い違い。
- **W1（work の区切り）**: 今の `work` は claim から最初の `receipt_observed` まで、台帳の `work` は最初の `validating` の記録まで。
  76 run の差はすべて receipt から `validating` までの台帳の `worker` の区間（中央値 14.5 秒、最大 1,063 秒）。
- **W2**: `receipt_observed` の無い run（`177a0afe-af84-4676-adfd-b94b7dc571dc`・`266cda8b-6f22-4f04-a39a-d2f017ca5310`。中断と復旧から検証に進んだ）は、今の `work` が無く台帳だけが持つ。
- **T（境目の幅）**: `wait_to_land` の 3 run（`3b0d2b77-…`・`ce07f96e-…`・`098c6d9f-…`）は、台帳の `validating` の終わりが `validation_finished` より 2.7〜3.8 秒遅く、比較の読み取りの境目の幅（`BOUNDARY_SECS` 2 秒）を少し超えた。

run ID ごとの不明の差と由来の一覧は末尾の「付録: 理由が不明の差の run ごとの一覧」。

## (c) blocker・holds ごとの所要時間と slot を握った時間（参考）

対象の行は (a) と同じ（run 253 行・task 521 行）。
割合 = その blocker・holds の区間の秒の合計 ÷ 同じ行の所要時間の秒の合計。
slot は holds が `worker_slot` か `landing_slot` の区間で、run の行だけに出す。
区間は重ならないが、blocker と holds は同じ区間を別の軸で数えるので、軸をまたいで足すと 100% を超える。
unattributed の区間は blocker・holds を持たない（(a) の分子）。

| 欄 | run か task | 行の件数 | 分子の秒 | 分母の秒 | 割合 |
|---|---|---:|---:|---:|---:|
| blocker: queue | run | 253 | 1,804,686 | 4,859,700 | 37.1% |
| blocker: ai | run | 253 | 1,371,606 | 4,859,700 | 28.2% |
| blocker: human | run | 253 | 1,365,321 | 4,859,700 | 28.1% |
| blocker: compute | run | 253 | 168,134 | 4,859,700 | 3.5% |
| blocker: runtime | run | 253 | 147,941 | 4,859,700 | 3.0% |
| blocker: external | run | 253 | 1,379 | 4,859,700 | 0.0% |
| blocker: infra | run | 253 | 632 | 4,859,700 | 0.0% |
| holds: none | run | 253 | 3,020,933 | 4,859,700 | 62.2% |
| holds: worker_slot | run | 253 | 1,691,580 | 4,859,700 | 34.8% |
| holds: landing_slot | run | 253 | 147,185 | 4,859,700 | 3.0% |
| slot | run | 253 | 1,838,765 | 4,859,700 | 37.8% |
| blocker: queue | task | 521 | 84,488,039 | 93,998,709 | 89.9% |
| blocker: human | task | 521 | 6,556,189 | 93,998,709 | 7.0% |
| blocker: ai | task | 521 | 2,608,464 | 93,998,709 | 2.8% |
| blocker: compute | task | 521 | 164,683 | 93,998,709 | 0.2% |
| blocker: runtime | task | 521 | 127,892 | 93,998,709 | 0.1% |
| blocker: external | task | 521 | 1,332 | 93,998,709 | 0.0% |
| blocker: infra | task | 521 | 610 | 93,998,709 | 0.0% |
| holds: none | task | 521 | 92,177,957 | 93,998,709 | 98.1% |
| holds: worker_slot | task | 521 | 1,625,299 | 93,998,709 | 1.7% |
| holds: landing_slot | task | 521 | 143,953 | 93,998,709 | 0.2% |

- 分母が 0 の欄は無い。
- task の blocker: queue の大半は `ready`（前提が終わった後に claim を待つ時間）と `dependency_wait` で、task の所要時間の 90% を占める。
- run の blocker: human（28.1%）は人の答え待ち（`waiting`・`landing_answer` など）で、引かずに所要時間に含めている。
- コマンド: [agg.py](measurement-ledger-parallel/agg.py) `fold.json ledger.json`。

## (d) run・task を持たない欄の差と session・queue の行の照合

主な読み取りの `fields`（13 欄）。
今の値は EventStore の上の今の関数、台帳の値は同じ関数を台帳の行が返す events の上で作ったもの。
session・queue の行の照合は (1) の表（行の無い対象 0、final でない session 1、dirty 0）。

| 欄 | 理由の分類 | 違う値の数 | 差 | 理由 |
|---|---|---:|---|---|
| jobs・updates・landing_utilization・claim_holds・landing_holds・claim_deferrals・execution_tokens・backend_failures・reason_codes | — | 0 | なし | — |
| draft_flow | records | 11 | `registered` 445→356（−89）、`adopted` 163→79（−84）、`backlog` 14→3（−11）、`drafts_per_landing` 1.92→1.53 など（11 個の全部は下の「比較で違った値の全部」） | 期間に登録された follow_up の draft 442 件のうち 89 件は読んだ時点で終わっていない（completed・canceled でない）。台帳は task の行を終わった task にだけ作るので、その `task_created` などの記録が行に無い |
| auto_repairs | records | 8 | 10-16・10-17 の `asks_opened` と `asks_by_reason.scope` が −1、10-18・10-20 が −2 | その 6 個の ask はどれも run を持たない `planner_question`（task 2584・2654・2723・2754・2906・2916）で、task が終わっていないので行が無い |
| planner_routes | records | 1 | `headless.planner_questions` 9→3（−6） | auto_repairs と同じ 6 個の `planner_question` |
| sessions | records | 20 | `by_kind` と `by_route` の active: worker の count −1・total −1,307 秒、revise の count −1・total −873 秒、runtime_planner の count −2・total −112 秒、runtime_planner と worker の active の median（+1 秒・−2 秒）と revise・runtime_planner の active_ratio（−0.001）（20 個の全部は下の「比較で違った値の全部」） | 期間の終わり（`--until`）に開いていた span の、終わりの後に記録された `session_turns`（turn は終わった後に記録されるので、終わりをまたぐ turn は終わりの後に届く）。今の stats は EventStore の全部の event を読んで窓に重なる turn を数え、台帳の窓は終わりまでの event だけを返すので turn が少なく、終わりの前に turn の無い span は `active.count` から外れる。終わりの後の `session_turns` で窓に重なる turn を持つのは 5 span（worker 1 は run `42f92bfb-…`、revise 2 は run `3d06e8a0-…`・`12bbaa9e-…` でうち 1 つは終わりの前に turn がある、runtime_planner 2 は run を持たない）で、件数の差と合う。各 span の秒の差は、窓に重なる turn の時間を窓の終わり（窓の最後の event の時刻で、`--until` より約 7 秒前）で切ったものと合う。6 run のうち他の 3 run は、終わりの後に窓に重なる turn を記録した span を持たない。今の比較の読み取りは span ごとの差を `fields[].spans` に出し、理由の分類を `records` でなく `turns_after_end` にする |

### 比較で違った値の全部

主な読み取りの `fields[].differences` の全部（4 欄・40 個。どの欄も `differing` と列挙の数が等しく、切られた値は無い）。
差は 台帳 − 今 で、片方に値が無いものは差を書かない。
理由の分類はどれも `records` で、各欄の理由は上の表。
コマンド: `jq -r '.fields[]|select(.differing>0)|.field as $f|.differences[]|[$f,.path,.stats,.ledger,.diff]|@tsv' ledger.json`。

| 欄 | path | 今 | 台帳 | 差 |
|---|---|---:|---:|---:|
| sessions | `sessions.by_kind.revise.active.count` | 304 | 303 | -1 |
| sessions | `sessions.by_kind.revise.active.total` | 256500 | 255627 | -873 |
| sessions | `sessions.by_kind.revise.active_ratio` | 0.547 | 0.546 | -0.001 |
| sessions | `sessions.by_kind.runtime_planner.active.count` | 444 | 442 | -2 |
| sessions | `sessions.by_kind.runtime_planner.active.median` | 69 | 70 | 1 |
| sessions | `sessions.by_kind.runtime_planner.active.total` | 38299 | 38187 | -112 |
| sessions | `sessions.by_kind.runtime_planner.active_ratio` | 0.392 | 0.391 | -0.001 |
| sessions | `sessions.by_kind.worker.active.count` | 251 | 250 | -1 |
| sessions | `sessions.by_kind.worker.active.median` | 1176 | 1174 | -2 |
| sessions | `sessions.by_kind.worker.active.total` | 492273 | 490966 | -1307 |
| sessions | `sessions.by_route.revise.headless.active.count` | 304 | 303 | -1 |
| sessions | `sessions.by_route.revise.headless.active.total` | 256500 | 255627 | -873 |
| sessions | `sessions.by_route.revise.headless.active_ratio` | 0.547 | 0.546 | -0.001 |
| sessions | `sessions.by_route.runtime_planner.headless.active.count` | 444 | 442 | -2 |
| sessions | `sessions.by_route.runtime_planner.headless.active.median` | 69 | 70 | 1 |
| sessions | `sessions.by_route.runtime_planner.headless.active.total` | 38299 | 38187 | -112 |
| sessions | `sessions.by_route.runtime_planner.headless.active_ratio` | 0.392 | 0.391 | -0.001 |
| sessions | `sessions.by_route.worker.headless.active.count` | 251 | 250 | -1 |
| sessions | `sessions.by_route.worker.headless.active.median` | 1176 | 1174 | -2 |
| sessions | `sessions.by_route.worker.headless.active.total` | 492273 | 490966 | -1307 |
| planner_routes | `planner_routes.headless.planner_questions` | 9 | 3 | -6 |
| auto_repairs | `auto_repairs.by_day.2026-10-16.asks_by_reason.scope` | 21 | 20 | -1 |
| auto_repairs | `auto_repairs.by_day.2026-10-16.asks_opened` | 27 | 26 | -1 |
| auto_repairs | `auto_repairs.by_day.2026-10-17.asks_by_reason.scope` | 16 | 15 | -1 |
| auto_repairs | `auto_repairs.by_day.2026-10-17.asks_opened` | 44 | 43 | -1 |
| auto_repairs | `auto_repairs.by_day.2026-10-18.asks_by_reason.scope` | 26 | 24 | -2 |
| auto_repairs | `auto_repairs.by_day.2026-10-18.asks_opened` | 40 | 38 | -2 |
| auto_repairs | `auto_repairs.by_day.2026-10-20.asks_by_reason.scope` | 9 | 7 | -2 |
| auto_repairs | `auto_repairs.by_day.2026-10-20.asks_opened` | 20 | 18 | -2 |
| draft_flow | `draft_flow.adopted` | 163 | 79 | -84 |
| draft_flow | `draft_flow.backlog` | 14 | 3 | -11 |
| draft_flow | `draft_flow.by_origin.follow_up.adopted` | 160 | 76 | -84 |
| draft_flow | `draft_flow.by_origin.follow_up.backlog` | 13 | 3 | -10 |
| draft_flow | `draft_flow.by_origin.follow_up.registered` | 442 | 353 | -89 |
| draft_flow | `draft_flow.by_origin.goal_gap.backlog` | 1 | 0 | -1 |
| draft_flow | `draft_flow.by_origin.goal_gap.oldest_backlog_secs` | 1165320 | （値なし） | — |
| draft_flow | `draft_flow.by_origin.goal_gap.oldest_backlog_task_id` | 2009 | （値なし） | — |
| draft_flow | `draft_flow.drafts_per_landing` | 1.92 | 1.53 | -0.39 |
| draft_flow | `draft_flow.inflow_per_outflow` | 1.01 | 1 | -0.01 |
| draft_flow | `draft_flow.registered` | 445 | 356 | -89 |

- コマンド: 欄の差は主な読み取りの `.fields[]`。
  draft_flow の確かめは `dagq events --full --kind follow_up_registered --since 2026-10-13T12:00:52Z --until 2026-10-20T12:00:52Z --limit 5000` の task と `dagq events --full --kind task_status_changed --since 2026-10-13T12:00:52Z --limit 20000` の completed・canceled の差。
  auto_repairs と planner_routes の確かめは `dagq events --full --kind ask_opened --since 2026-10-13T12:00:52Z --until 2026-10-20T12:00:52Z --limit 5000` のうち終わっていない task の ask。

## (e) host の欄の日ごとの差と node の行の照合

期間を JST の日（kpi の日の区切り）で切り、日ごとに `dagq stats --ledger --since <日の始め> --until <日の終わり>` を打った（2026-10-21T03:15〜03:20Z、固定バイナリ `b31d8470`）。
区切りは 2026-10-13T12:00:52Z・10-13T15:00:00Z・10-14T15:00:00Z・…・10-19T15:00:00Z・10-20T12:00:52Z。
出力の `host` のうち `window: "stats"`（その日の node の行の標本の要約と今の CSV の要約）を比べた。
差のある列はどの日も 0 なので、列・理由の分類・差の表は行を持たず、日ごとの差のある列の数と標本の内訳だけを書く。
kpi の `cpu_per_landing`・`load_per_core`・`health.disk` は同じ host の要約（`cpu_secs`・`metrics.load1`・`metrics.disk_free_*`）から作るので、要約の差が 0 ならそれらの host の部分も差が 0。

| 日（JST、期間で切る） | 標本（両方・CSV だけ・取り込み漏れ・保持・ストアだけ） | 差のある列 | 理由の分類 |
|---|---|---:|---|
| 10-13（21:00:52 から） | 177・0・0・0・0 | 0 | — |
| 10-14 | 710・0・0・0・0 | 0 | — |
| 10-15 | 2,078・0・0・0・0 | 0 | — |
| 10-16 | 1,339・0・0・0・0 | 0 | — |
| 10-17 | 875・0・0・0・0 | 0 | — |
| 10-18 | 891・0・0・0・0 | 0 | — |
| 10-19 | 1,178・0・0・0・0 | 0 | — |
| 10-20（21:00:52 まで） | 988・0・0・0・0 | 0 | — |

- 期間全体（主な読み取りの `window: "stats"`）も標本 8,236 がすべて両方にあり、差のある列は 0。
- 主な読み取りの kpi の 2026-10-21 の窓（期間の外）は CSV だけの標本が 18 あり列が違うが、まだ取り込まれていない直近の標本で、期間に関わらない。
- node の行の照合: 期間全体は (1) の表のとおり 159 行で、行の無い・final でない・dirty の区切りは 0。
  日ごとの読みでは区切りの数は 3・20・23・25・23・25・25・22 で、行の無い・final でない区切りは 0。
  dirty はすべての区切りに出たが、2026-10-21T03:14:13Z の更新で `LEDGER_V` が 4 に上がり、作り直しの前の行が古い版に数えられたため（`folded` は `reach` と等しい）。

## (6) 段 2 の基準の判定

| 基準 | 結果 | 判定 |
|---|---|---|
| run と task の unattributed がともに 2% 未満 | run 0.00003%、task 0.055% | 満たす |
| 不明の差が 0 件（session・queue・node の行と run・task を持たない欄と host の欄を含む） | 比較の読み取りの分類では run の値の `unexplained` が 735 個、欄の `records` が 4 欄。timeline と events を読んで run の 735 個と欄の 3 欄（draft_flow・auto_repairs・planner_routes）は説明できたが、`sessions` の active の差（20 個の値）が説明できない | 満たさない |
| 対象が run と task ともに 1 件以上 | 終端の event から数えて run 253、task 527 | 満たす |
| 行の無い対象と push_pending 以外の final でない対象が 0 件 | 行の無い対象は 0（run・task は終端の event から数えた対象で）。final でない対象は session の 1 件（`inbox//session/23`、閉じてから 7 日の猶予の中） | 満たさない |
| dirty の残る対象が 0 件 | 主な読み取り（更新の前）の行で、終端の event から数えた run・task と session・queue・node のどれも 0 | 満たす |

満たさない 2 つと、説明はできたが読み取りの分類に掛からない差について、直す打ち手を receipt の follow_ups（improvement）に書いた。
- 比較の読み取りの理由の分類を増やし、ここで説明した由来（A・C・G・R の台帳の読みの名前、B・D・F・H・K の今の `LandClock`、W1・W2 の work の区切り）を `unexplained` から分ける。
- 台帳の読み（`read::land_phase`）が `awaiting_slot` を `landing_queue` に数えるかを決める（A。今の値との差の最大）。
- `sessions` の差を span ごとに名指す読みを足し、active の差の理由を確かめる。
- 照合で、猶予の中の行（inbox・planner の span）を push_pending と同じく final でない対象と分けて数える。
- 終わっていない task の記録（`draft_flow`・run を持たない ask）を段 3 の読み手が台帳から作れるよう、行か queue の記録に持たせる（今のままでは段 3 で値が欠ける）。
- 比較の読み取りが task の unattributed と blocker・holds・slot の内訳を出すようにする（この測定は events から畳み直した）。
- 照合の対象を終端の event から数える（今は legacy の行の終わりを最後の入力の時刻とし、終わりが期間の外の run 8 件・task 12 件を数える）。

D（revise の間の worker_question の答え待ちを `waiting` に記録しない・遅れる）は台帳の記録の不具合として、receipt の follow_ups（defect）に書いた。

## 付録: 理由が不明の差の run ごとの一覧

比較の読み取りが `unexplained` とした値を持つ 224 run。
「不明の差」は `unexplained` の値（台帳 − 今、秒）、「由来」は上の「理由が不明の差の由来」の分類で、その run の不明の差を説明する区間の分類をすべて挙げる。
コマンド: [perrun.py](measurement-ledger-parallel/perrun.py)（`matrix.py` の出力 `matrix.json` を読む）。

| run | task | 不明の差（秒） | 由来 |
|---|---:|---|---|
| 000eacb5-489c-410c-82f6-602e2860ee99 | 955 | exit +64, revise -32, conflict -31 | ACJ |
| 01abbedb-dcd8-407c-8f4a-d784b08f252f | 2869 | work 32, resume -2639, landing_queue -18, rebase -3, verify +3 | BCRW1 |
| 03f1dab6-7143-4ffb-91a7-00ea0f475917 | 2076 | exit +20, landing_queue -21 | A |
| 05c1db91-6d97-4a72-a3b4-728fea086a02 | 2341 | work 11, exit +4 | ACW1 |
| 0784ebe1-4ece-425d-aa91-4eb907a87a02 | 2458 | exit +416, revise -341, resume -25, landing_queue -68, verify +20 | ACFR |
| 08ec2f1c-6878-4b11-bad0-a2d5e1d2194d | 2793 | exit +46, revise -44 | AC |
| 098c6d9f-75ca-4d78-98ee-ce341c1255c2 | 2272 | wait_to_land -4, exit +216, revise -203, ask +18, resume -14, landing_queue -19 | ABCGT |
| 099497da-ae7e-4abe-bae6-4a6721b0ed2b | 2346 | exit +9, revise -6, ask +29, landing_queue -31 | ABC |
| 0b008184-314a-4150-af65-85f6719d8b02 | 2867 | work 7, exit +65, revise -63, landing_queue -3 | ACW1 |
| 0da679cd-7326-4a78-8e0f-38b8737e3560 | 1808 | exit +50090, revise -289, ask +39, resume -156, landing_queue -49550, e2e_wait -139, verify +7 | ABCFJR |
| 0e4eb30c-98ab-468d-bd3c-bbf43fb118fe | 2515 | exit +435, revise -124, resume -90, landing_queue -307, rebase -6, verify +93 | ACFR |
| 0fb9a8f7-3129-4d28-977e-4ad0dd2e4a31 | 1326 | exit +75, revise -18, conflict -17, landing_queue -41, rebase -3, verify +4 | ACGR |
| 101f5051-e7ce-43af-b6fc-4872a612c22b | 2127 | exit +10, revise -8 | AC |
| 10e3fcec-0324-4394-9739-6ea5a7413696 | 2751 | work 8, exit +5, revise -5 | ACW1 |
| 11399516-2f35-4499-ad1a-2a7e70b55482 | 2634 | exit +116, landing_queue -116, rebase -3, verify +3 | AR |
| 139e7c7f-287d-42c0-bc1a-abeb554e9d7b | 2720 | work 118, exit +122, landing_queue -121 | AW1 |
| 13f0a089-9555-4c64-86e9-16816339776e | 2312 | work 15 | W1 |
| 156dede0-0dbc-4721-acbc-dc0d76ce5786 | 2274 | exit +3054, revise -47, landing_queue -3008 | ACJ |
| 16d84f37-9152-42af-80ab-2a656b13de0f | 2596 | work 72, exit +56, ask +6, resume -54, landing_queue -7 | ABCW1 |
| 173df6d4-1e92-4998-bf1e-1ff9a634405a | 2493 | exit +7, revise -6 | AC |
| 177a0afe-af84-4676-adfd-b94b7dc571dc | 2394 | work stats なし, exit +247, conflict -117, landing_queue -127 | ACW2 |
| 1b069cda-dfd0-4b21-9aa6-4b0c6dc84212 | 2334 | work 56, exit +45883, ask +82, landing_queue -45960 | ABCW1 |
| 1c8b3b7c-68ce-46c0-b26d-f90ef1224edd | 2625 | exit +146, landing_queue -146 | A |
| 1e383227-f8ea-47ee-b7d0-2d727f97619b | 2182 | exit +28771, revise -51, ask +305, resume -3, landing_queue -29022 | ABC |
| 1f329f87-24d7-4b61-b033-37b1d16ae8b5 | 2333 | exit +7, landing_queue -3 | AC |
| 1f6707f3-46ed-4514-b660-71b8538a6bfd | 2485 | work 8, exit +8, revise -5 | ACW1 |
| 2095cfda-b31c-4fa8-8b83-886fe73c5274 | 2180 | work 19, resume -8677, landing_queue -518, verify +287 | ABCRW1Z |
| 209bcedf-276f-4b8c-9fbd-2a8b8c93803e | 1524 | work 28 | W1 |
| 2189537f-f974-4a9c-b80e-66f3912f9111 | 2454 | exit +357, revise -353, resume -3 | ACGJ |
| 22d2fb17-9274-4434-a68e-15f86a80f2ff | 2836 | work 9, exit +44, revise -42 | ACW1 |
| 23e818ce-ced9-460d-a050-62a5a20f9502 | 2314 | exit +104, revise -103 | AC |
| 25914307-08b4-4edc-b9f3-64848295c760 | 2265 | exit +256, revise -250, ask +86, resume -4, landing_queue -89 | ABCGJ |
| 2629cdd0-63bb-4224-9273-336cd224e01a | 2772 | exit +257, landing_queue -257 | AG |
| 266cda8b-6f22-4f04-a39a-d2f017ca5310 | 1721 | work stats なし, exit +341, revise -342, landing_queue -3 | BCW2 |
| 2810d946-39ef-422a-930e-05bc7dd6e2eb | 2888 | exit +10, landing_queue -10 | A |
| 28bbf72b-1db3-449b-bde8-c509de70205b | 2692 | exit +9, resume -3, landing_queue -3 | AC |
| 28eb1e37-2aa9-4ac6-b73e-91e19ce6b0da | 1260 | exit +43, revise -42, rebase -3, verify +3 | ACR |
| 2a9ee446-ed05-4893-9bf8-2d7874b3dde4 | 2313 | exit +126, landing_queue -125 | AC |
| 2b51d67d-2fe3-405d-9782-80c6e8b7368e | 2330 | exit +62, revise -61, landing_queue -3 | AC |
| 2cc84ae8-fd32-4b36-a2a2-44dd5390f309 | 1720 | work 5 | W1 |
| 2d525447-430e-404a-a8cc-32c3e44981ab | 2470 | work 26 | W1 |
| 2e4f880a-ece9-45c2-8c1a-a381fc2db813 | 2470 | work 11, exit +469, revise -300, resume -44, landing_queue -165, rebase -3, verify +44 | ACFRW1 |
| 2edc1303-5883-4dac-bfe4-69387519efd0 | 2394 | work 37 | W1 |
| 2f1e8290-8192-4264-ba5e-157d82b46029 | 2137 | exit +47, revise -45 | AC |
| 2fcc73b2-8a09-425c-8368-8ba340acda19 | 2273 | exit +136, revise -131, landing_queue -5 | AC |
| 3158f4fe-8a50-4001-8454-2424a9ab2814 | 2395 | exit +120, revise -20, ask +29, landing_queue -129, rebase -3, verify +3 | ABCR |
| 31b6b8fb-07b7-4be1-a135-631903c4502b | 2748 | work 37 | W1 |
| 324e8936-474e-44fc-801f-9f58b1fb69ef | 2595 | ask +216, landing_queue -216, verify +3 | BR |
| 3489c2d8-b93a-4bd1-8fb8-913efa8bfa4e | 2603 | exit +31, landing_queue -29 | AC |
| 34948123-29e1-48bc-84c2-639d57d5cb35 | 2563 | work 1063 | W1 |
| 35f516ee-97e6-447b-9496-48cddb6502e5 | 1822 | exit +122, revise -119 | AC |
| 368b2426-209a-47a0-bedd-e8c580b70169 | 2344 | exit +14, revise -10, ask +3, resume -3, landing_queue -4 | ABC |
| 36bf03e8-7bfd-4f79-9682-abf2887f289b | 2348 | exit +186, review +3, revise -46, conflict -23, resume -116, landing_queue -4 | ABCJ |
| 3784cb5b-4caa-4aef-aeac-7cfd00e8d5d4 | 2375 | work 7 | W1 |
| 379670ff-00b9-4b6e-ac4b-a878668bf3f2 | 2593 | exit +20, revise -19 | AC |
| 38be94e7-b463-4237-b6db-7c40e792c22d | 2317 | exit +7, revise -6 | AC |
| 396b10f4-a8f8-4d90-9734-d65697705c40 | 2521 | exit +525, review +59, conflict -7, landing_queue -577 | ACJ |
| 3a22c3fa-5519-4500-8ba9-db176a8d8598 | 2306 | work 5, exit +337, revise -81, resume -338, landing_queue -184, rebase -6, verify +272 | ACFRW1 |
| 3b0d2b77-d544-4094-84a6-a7fd9c682669 | 1498 | wait_to_land -3, exit +44, revise -43 | ACT |
| 3c6c9b5a-617e-4633-92b8-87fca5879b76 | 2134 | exit +48, landing_queue -48 | A |
| 3d42b312-3a8c-4044-a697-65856fa13294 | 2834 | exit +4, landing_queue -3 | A |
| 3dcb17e1-6f2b-46bb-984b-4efec440e150 | 2796 | exit +4, revise -3 | AC |
| 3e32493d-86db-432c-acff-10a5aa5a3443 | 1524 | exit +1217, review +3, revise -1113, ask +243, resume -104, landing_queue -247 | ABCGJ |
| 3ea64e98-6eba-4030-a459-094c0a009422 | 1881 | work 72, exit +1690, review +4, revise -356, ask +2914, resume -1832, landing_queue -2347, e2e_wait -75 | ABCHJW1 |
| 42f3d124-ba16-4d22-af82-8e7a5ccc2890 | 2510 | exit +238, review -4, revise -161, ask +86, resume -73, landing_queue -85 | BCG |
| 43229830-0377-4acb-97d4-c90163f91661 | 2326 | exit +1144, review -3, revise +43163, ask -43675, landing_queue -630 | BCDGK |
| 44793a6f-db0a-4ead-a6fd-f788fb857473 | 2746 | work 86, revise -4, resume -3202, landing_queue -66 | BCW1 |
| 450133c7-9eb3-4cdb-b8a2-045b66808f82 | 2719 | exit +143, revise -5, ask -17, resume -47, landing_queue -72, rebase -4, verify +4 | ABCHR |
| 453b30c7-d3a0-4d0d-98e9-712465b12a4f | 2550 | work 15, exit +42, conflict -4, landing_queue -39 | ACJW1 |
| 465b389e-d39e-40e2-ae81-6618716706be | 2343 | exit +58, revise -48, landing_queue -9 | AC |
| 47f0b7f2-d3ef-4c2f-91c6-60969d4780e9 | 784 | exit +195, revise -147, landing_queue -48 | AC |
| 48bf5c0e-b8fc-4d80-a8f0-f0a61ad70822 | 2315 | exit +183, revise -85, landing_queue -98, rebase -3, verify +4 | ACR |
| 48ccb8d1-507b-44e7-976b-9c6a11e6392e | 2321 | exit +62, landing_queue -62 | A |
| 490a2191-6569-4c7f-9f8a-7a40ea5b2708 | 2414 | work 10 | W1 |
| 4cbc9bff-76d5-4fbb-83ea-bd5f53002c6d | 2364 | work 116, exit +4, landing_queue -4, rebase -3, verify +3 | ARW1 |
| 4cecf706-3e81-4bec-a6cd-bf8e7a814cb7 | 2782 | exit +3 | AC |
| 4de8bdc2-607a-4051-9474-6d71bd1eaba7 | 2357 | exit +21, revise -19 | AC |
| 4ed90328-f68d-4d51-adda-07a960056db4 | 2136 | exit +46, revise -44 | AC |
| 4f5d2c97-309d-4da7-b3e7-5d59202a6983 | 2791 | work 15, exit +18, ask +33, resume -16, landing_queue -35 | ABCW1 |
| 535c3205-7b1f-4a71-a844-5905b4cf208a | 2350 | exit +31, revise -6, ask +3, resume -24, landing_queue -4 | ABC |
| 5574366d-7ae9-4a78-bdee-bc227744240d | 2499 | exit +229, review -11, revise -121, ask +48, resume -28, landing_queue -183, e2e_wait +66 | ABCGJZ |
| 560135dd-a532-44eb-8f2b-5155015cfb7f | 2457 | work 11, exit +70, revise -67, landing_queue -4 | ACW1 |
| 569d8507-83b4-4cf1-b29f-b85e6545b17b | 2342 | work 21, exit +29, revise -27 | ACW1 |
| 5761b0e6-2c85-4322-8999-ed6d147d9565 | 2522 | exit +1933, revise -13, landing_queue -1919 | AC |
| 57aba7c6-3493-4a86-9fbc-027f60ae5007 | 2484 | work 5, exit +3, revise -3 | ACW1 |
| 587d2737-2d83-4c6f-93d7-a1cf3d89278d | 2329 | exit +190, revise -52, conflict -126, landing_queue -12 | ACJ |
| 59ba807f-6608-415c-afa0-d783aa4c68bc | 2753 | work 4, exit +232, revise -4, ask +80, resume -228, landing_queue -79 | BCW1 |
| 5acb58ad-9990-4a55-8e04-e0586e4be6d4 | 2428 | work 14, revise -96, resume -2810, landing_queue -676 | ABCW1 |
| 5c93082b-2f83-4831-ba4c-d84a1768ddf2 | 2110 | exit +98, revise -92, landing_queue -6 | AC |
| 5cc91014-dfc9-4293-9dc6-377f8a8be159 | 631 | work 6, exit +9, landing_queue -9 | AW1 |
| 5d212fdf-b1b1-4369-8b5b-442d56706c4c | 1880 | work 21, exit +476, ask +45689, resume -291, landing_queue -46163, rebase -7, verify +297 | ABFRW1Z |
| 5d217a0c-0fc7-441a-a6c9-d8c619783e00 | 2349 | exit +31, revise -7, resume -22 | AC |
| 609270cf-0ecf-4135-b463-38b94d333424 | 2332 | exit +369, review -4, revise +13604, ask -13839, resume -7258, landing_queue +7127 | ABCDFGK |
| 62e7bed6-7ef4-4897-863c-d671697bb198 | 2640 | exit +3 | A |
| 63f2d964-4b49-4d46-8597-4829ea1c02b5 | 2747 | resume -1202, landing_queue -393 | ABC |
| 651643eb-0c26-48d0-a1c5-60471e51d424 | 2263 | exit +153, revise -90, landing_queue -63 | AC |
| 660de816-8453-4a98-bb6e-9ae8ab1edf85 | 2698 | exit +46, landing_queue -46 | A |
| 686bb6d5-3913-401a-870a-47dd55555bcc | 2690 | work 73, exit +4 | ACW1 |
| 691f7f0b-4d36-4d48-9068-e2580363c765 | 2447 | exit +4, revise -3 | AC |
| 693ddf17-3ec5-42a8-b705-e21024017076 | 2387 | exit +35, revise -30, landing_queue -5 | AC |
| 6acfb21f-03d1-4cb9-b7fc-1aff47f6f59a | 2792 | revise -143, resume +510, landing_queue -39 | ABCH |
| 6febcadd-817c-4a21-859a-966b8fc9d7ed | 2138 | work 9 | W1 |
| 707e9470-6bd9-4a4b-bc06-011f096b97d5 | 2833 | work 16, exit +95, ask +478, resume -90, landing_queue -481 | ABCW1 |
| 70836771-607e-4c73-9df6-76462a702483 | 1888 | work 45, exit +26, revise -25 | ACW1 |
| 71ba5f7c-e47b-4169-ae66-7657a434724b | 2261 | revise -124, resume -3499 | BC |
| 7274b548-c3a9-41ae-8105-6d4c0b754d39 | 2164 | exit +3 | A |
| 745b8a71-f73e-452b-9e3a-b46b4e344174 | 2083 | exit +51, landing_queue -50 | A |
| 7586248b-fb9d-4427-8260-160c3bb3019a | 1804 | exit +4, landing_queue -4 | A |
| 76638eee-cf63-4759-ac39-761d5e09f9f3 | 700 | exit +40, revise -8, conflict -32 | ACJ |
| 76a3c8e5-e4ba-4cd8-b616-cf52f5ea35a5 | 2135 | work 48, exit +5, landing_queue -4, rebase -3, verify +3 | ARW1 |
| 77974fba-213e-468c-915c-ef5e27d75589 | 2660 | exit +76, landing_queue -76, rebase -3, verify +3 | AR |
| 786966d3-df7c-4936-b44b-6da9e7d7477c | 2393 | revise -58, resume -1255, landing_queue -114 | ABCH |
| 798e0ab9-6592-48bc-b72e-2443daa8167b | 1969 | exit +6, landing_queue -7 | A |
| 79c5d61e-e5d1-4f49-b2b5-1ffe4f377f4a | 1882 | work 5 | W1 |
| 7a80d697-0034-4e45-8ed2-11fec5a78bc4 | 2507 | exit +733, review -43, revise -71, ask +48, resume -178, landing_queue -283, e2e_wait -206 | ABCG |
| 7b5149a0-be9c-47fd-bf9e-8259447b1ef4 | 2167 | exit +5, revise -3 | AC |
| 7d2648b9-bc51-410a-8f16-9238700887ae | 2724 | exit +370, revise -177, landing_queue -194, rebase -3, verify +3 | ACR |
| 7e00d636-55f0-47f2-9d15-638f97aa2e83 | 2832 | exit +92, revise -90 | AC |
| 7e9aa263-2648-4e43-a1c9-8f42cb033f4f | 2424 | exit +7, revise -4, landing_queue -3 | AC |
| 7ef12d10-f2c7-4619-a102-8c2030d01cfe | 2310 | exit +4, landing_queue -3 | A |
| 818e6c1e-704d-449c-9e8a-8a87335d2b82 | 2594 | exit +68, revise -67 | AC |
| 819261e7-8a36-4499-a020-af87d3851304 | 2265 | work 14 | W1 |
| 8233da91-8296-4b9d-86e3-3af4115abc19 | 2331 | work 88, exit +1540, revise -1221, ask -99, resume -889, landing_queue +671 | ABCDFGKW1 |
| 837ba707-b6eb-45f6-83df-9abb4efadd33 | 2524 | exit +78, landing_queue -78 | A |
| 84b1e4ab-e7e0-4cd4-8fef-5d2bf3c26ba3 | 2347 | work 12, exit +5, revise -5 | CW1 |
| 8540ec64-aaf8-4be7-af55-8356cf11ffe3 | 1883 | exit +160, revise -131, ask +31, resume -27, landing_queue -33 | ABCG |
| 8a376640-979f-41dd-b1ee-c046e7333efb | 2513 | exit +146, revise -68, landing_queue -78 | AC |
| 8bf5cc3d-c663-4083-a791-9546e26f5892 | 2835 | exit +112, revise -110, rebase -3, verify +3 | ACR |
| 8e38b232-956d-41a1-b59e-340fd1fd5383 | 2213 | exit +63, ask +71, resume -62, landing_queue -72 | ABC |
| 8ea1f93d-3a18-41f3-b00c-eebb15f5c312 | 2741 | exit +169, revise -169, ask +41, landing_queue -40, rebase -4, verify +3 | BCR |
| 8fc31a49-699c-4566-bfd1-76e425b6b181 | 661 | exit +7, revise -4 | AC |
| 930be273-9a98-4e64-940b-7ebc053a06ad | 2489 | exit +160, landing_queue -160 | A |
| 971d749f-2908-4bfc-8fdf-2592bf779459 | 2802 | work 12, exit +3, revise -3 | ACW1 |
| 974d845c-f150-452d-aabf-8a9596e9923f | 1807 | exit +114, landing_queue -114 | A |
| 990a09e8-8554-4c5f-8dbd-38ff22ff7532 | 2163 | work 12, resume -5658, landing_queue -55 | ABCW1 |
| 9a187c4a-5133-4bec-95db-4ac45d60c768 | 2892 | work 47 | W1 |
| 9a7a3d0d-a940-40f3-84b5-097c00df0f3a | 2319 | exit +6, revise -5 | AC |
| 9aa1ecde-a0e6-4a3c-be18-410c4be45608 | 2338 | work 4, exit +12, revise -8, landing_queue -4 | ACW1 |
| 9ab2560c-8ce8-4994-b612-416c563b3572 | 2345 | exit +283, revise -223, ask +87, resume -60, landing_queue -87 | ABCJ |
| 9aeca547-f1a2-4a46-82ac-adde8980b16b | 2322 | exit +71, revise -69 | AC |
| 9c095c7c-1002-4754-8a8e-f5052abe1e5d | 2852 | exit +240, revise -76, ask -106, resume -46, landing_queue -14 | ABCH |
| 9e3215e6-4d47-451b-8c9d-96810a97de15 | 1831 | work 11 | W1 |
| 9ebf2ec0-3fa3-487b-91e2-86bf3cda5892 | 2600 | revise -57, resume -4580, rebase -3, verify +3 | BCR |
| 9f40ee36-f066-460f-810d-d294250dbaed | 2316 | exit +31, revise -26, conflict -3 | AC |
| a385eca0-6f1b-4341-95fd-f5a0ffb4929b | 1464 | work 46 | W1 |
| a3e4f858-550b-45ed-8bf6-96dab9813018 | 2566 | exit +14, landing_queue -14, rebase -3, verify +3 | AR |
| a3eb4738-50e4-4c8a-b94f-5b95a22e4481 | 2853 | work 6, exit +42, landing_queue -39 | ACW1 |
| a44a30d1-c10e-4b07-81fc-90a4e07bd791 | 2790 | exit +123, revise -61, ask +19, resume -3, landing_queue -79 | ABC |
| a50ecc7f-000c-4161-9288-2805f6d6bbd2 | 2536 | exit +4 | AC |
| a6226bae-fd93-4b85-968d-dc2001ddc14b | 2511 | work 47, exit +34, review -5, revise -23, ask +48, landing_queue -51 | ABCGW1 |
| a7e26edf-59de-4803-b538-8bbe5e688184 | 2745 | exit +72, revise -71 | AC |
| a7e45912-fed5-47bb-8d7c-3110cbdea23f | 2376 | ask +32, landing_queue -32 | B |
| a82f2469-ec69-4d66-aa3a-af13a626a49c | 2624 | work 15, exit +212, revise -61, ask +72, resume -2503, landing_queue +2244, rebase -4, verify +40 | ABCFJRW1 |
| a9c84d4b-348a-43ba-b580-06c96a5b2e49 | 1722 | exit +6, landing_queue -4 | AC |
| ac26d225-a2a5-4dc2-82c9-05afc0262f11 | 1957 | exit +131, revise -130 | AC |
| adb17cd9-b20c-4466-9eed-de5008c152b2 | 2262 | work 10, exit +4, revise -3 | ACW1 |
| aeabe686-106e-4349-bd19-64cd650d91f5 | 2392 | exit +31, revise -30 | AC |
| af136f02-5a92-439d-901f-bea91c9855a7 | 2362 | work 27, exit +8, revise -5 | ACW1 |
| b08d2656-55a5-4352-89bc-34b1ff9a0bdb | 2448 | work 6, exit +44, revise -42 | ACW1 |
| b23dcf0d-7c73-4c33-87f7-ed9509e3cfb7 | 2337 | work 9, exit +86, revise -24, landing_queue -61 | ACW1 |
| b2779a1f-18f6-46d3-96d7-20b0721239c9 | 1324 | work 10, exit +4, landing_queue -4 | AW1 |
| b37d637b-ae7a-49a5-a73a-f4fee8d3f691 | 2502 | exit +293, review -3, revise -27, ask +48, resume -114, landing_queue -214, verify +18 | ABCFGJ |
| b740677a-6124-4f84-840b-3a419c0e39fe | 2691 | exit +30, revise -29 | AC |
| ba60547e-22fe-4538-b125-0668ded8737a | 2352 | exit +515, revise -317, ask +210, landing_queue -409 | ABCJ |
| bb3e1950-17c3-4e22-b0a4-aab6f95867f0 | 2129 | work 98, exit +47971, revise -66, landing_queue -47906 | ACW1 |
| bdb57693-d134-4898-be9a-7d85d27aae27 | 2335 | exit +10, revise -10 | AC |
| be907aeb-21aa-4b74-b3b9-74e2533fe013 | 1332 | exit +64, revise -62 | AC |
| bf3d2771-e9d3-4590-a167-c90364850462 | 2742 | exit +6, revise -5 | AC |
| bf446ec6-49a4-47e2-ad1d-12a7c3c95296 | 1819 | exit +81, revise -78 | AC |
| c1918aa7-d45c-44c0-af3e-53893810872f | 2363 | exit +1863, revise -52, landing_queue -1810 | AC |
| c26e7bf7-5af3-427d-9f08-79f440d720d8 | 2588 | exit +8, revise -6 | AC |
| c2d60dbc-35f1-4ba1-8398-f8a2852ee6c5 | 2684 | exit +168, revise -93, landing_queue -75 | AC |
| c3f2e1dd-8012-4074-aa16-a3d18a4e7d8f | 2590 | exit +4 | AC |
| c5322712-8d98-4a6b-9388-a9dc852bee9e | 2798 | work 11, exit +5, revise -3, landing_queue -3 | ACW1 |
| c5d3b204-fffd-419a-8faa-63781608e97b | 2325 | exit +1379, revise -1345, ask +255, resume -4473, landing_queue +4185, verify +3 | ABCDFGK |
| c6a02254-544e-40a2-b154-d2c5b394396e | 2494 | exit +154, revise -154 | ACJ |
| cc32abea-b0ac-4178-9d43-e2a594fcca82 | 2626 | work 9 | W1 |
| cd4e8344-77c3-48ee-9f0a-224fc9c08cb2 | 2339 | work 41, exit +10, revise -9 | ACW1 |
| ce07f96e-ac8f-49d6-b367-6ecefff91946 | 2453 | work 9, wait_to_land -4, exit +34, revise -32 | ACTW1 |
| cfceaaa1-d8bd-4345-836d-9dcdf58fdd42 | 2323 | work 10, exit +120, landing_queue -120, verify +3 | ARW1 |
| d18d0a48-2305-4c0b-9c03-0ca39574f998 | 2398 | work 5, exit +56, revise -56, ask +17, landing_queue -16, rebase -8, verify +7 | BCRW1 |
| d3010f10-00a1-4c79-ad91-7b749c35b933 | 2336 | exit +23, revise -22 | AC |
| d33b873a-478e-4d9a-bc8d-242572b81d9a | 2585 | exit +190, revise -130, landing_queue -60 | AC |
| d3c6985d-3f57-4871-aea2-33dc34a815db | 2779 | exit +3, landing_queue -3 | A |
| d3e72862-f3a4-42b5-a206-3445ad50a28d | 2592 | work 99, exit +45, revise -19, landing_queue -26 | ACW1 |
| d428571c-a100-4b2e-b7b7-1e052ea185af | 2601 | exit +108, landing_queue -108 | A |
| d5b695cc-5116-495c-a3fd-82b96d05804b | 2512 | work 78, exit +54, revise -50, landing_queue -4 | ACW1 |
| d5d5271b-8f80-416e-8f91-7bc65965565b | 2607 | work 10, exit +160, revise -33, resume -56, landing_queue -125, verify +56 | ACFRW1 |
| d65eeb19-439b-4032-a2ce-d38ffc743dc5 | 2301 | exit +21, revise -19 | AC |
| d93d4738-af0f-44a7-bbc2-bc8f093eec0b | 2768 | exit +3, rebase -4, verify +4 | AR |
| d96b99ec-67b3-4c10-aeaf-fcf20ae117e8 | 2320 | exit +5 | AC |
| d9ada5c7-7fd4-43d0-83c5-7e5b1c7096a2 | 2794 | exit +39, revise -38 | AC |
| dc5239c3-6232-4da5-8266-0fff1b195a9e | 1004 | exit +13, revise -11 | AC |
| df6cc5b7-97ac-49d7-ae01-6edd44e09a62 | 2112 | exit +52, revise -47, landing_queue -5 | AC |
| e0149b7d-7c7d-462b-a552-b457b7b5d9cf | 1871 | work 168, exit +3 | ACW1 |
| e04aa5a1-d975-4b22-8b08-160874a05245 | 2504 | exit +320, review -9, revise -168, ask +48, resume -3, landing_queue -188 | ABCGJ |
| e092087d-2551-4f1c-971e-2a5d39966927 | 2353 | exit +127, review -3, revise -6, ask +47, resume -117, landing_queue -49 | ABCG |
| e5afb2c5-15ba-4a46-a277-48699dc2d9c3 | 2396 | work 27, exit +102, review -4, revise -263, ask +168, rebase -3, verify +3 | BCDGKRW1 |
| e643689f-8808-4ef0-a892-739c74f0c708 | 2270 | exit +264, revise -222, landing_queue -43 | ACJ |
| e8a71509-40e6-4cc6-878d-0bba6bd65eb7 | 2893 | work 5, revise -6, resume -492, landing_queue -38 | BCW1 |
| e97eeed2-fd88-4caa-b31b-159c108092eb | 2599 | revise -200, conflict -147, resume -200, landing_queue -3342, e2e_wait -159, verify +4 | ABCFGKR |
| eab366c4-c842-44e0-8668-d78915af0d42 | 2308 | exit +61, revise -63 | ACJ |
| ebaa7baf-0ef1-4dd5-b73e-6d8a1a09f724 | 2165 | exit +167, revise -117, landing_queue -50 | AC |
| ed908a2f-234a-440e-b4c3-06444f7c62c1 | 2340 | exit +6, revise -4 | AC |
| ed90af54-67b6-4f53-8d6d-1871aeb64902 | 2328 | exit +1502, revise -1480, ask +467, resume -704, landing_queue +215, verify +3 | ABCDFGK |
| eed95ac4-36ee-49b6-bab3-562f70395e48 | 2309 | exit +3, landing_queue -3 | AC |
| efc08206-7924-45c2-b0ba-ba6800786ae4 | 2505 | exit +320, revise -79, landing_queue -241 | AC |
| efeb5564-1d13-4d57-adbd-59d72cf63245 | 2318 | exit +217, revise -5, conflict -210 | AC |
| f15e4e68-c859-4ef1-8f0a-1e058f810ed2 | 2525 | exit +44, revise -42 | AC |
| f277eb06-6d61-48ec-a043-bb3c3b1e172e | 2509 | exit +46, ask +15, resume -43, landing_queue -17 | ABC |
| f2b418b7-721f-45eb-99fc-839888ee36b1 | 2271 | work 18, exit +25547, revise -185, ask +3201, resume -1892, landing_queue -26560, e2e_wait -111 | ABCW1 |
| f3a94d2a-39f2-416a-85e0-07dd7d14d2fd | 2480 | work 105, exit +107, revise -52, landing_queue -54 | ACW1 |
| f4007aa6-e457-46e6-a992-635c114a0312 | 1403 | work 14, exit +71, landing_queue -68, rebase -3, verify +3 | ACRW1 |
| f4cc14fb-0a2b-41a5-894e-5aa1e09e32c9 | 2864 | verify +3 | R |
| f5c71df2-0d67-495d-b5d6-d114ddc34bb2 | 2526 | exit +4, landing_queue -4 | A |
| f62bc27a-eebb-419b-8629-45c4df23d2b5 | 1821 | work 31 | W1 |
| f87a71a5-de6d-4bf0-b92c-9987016b4afa | 2307 | work 63, exit +5, landing_queue -5 | AW1 |
| f8864968-44d1-40b5-91f1-cf0f5914ae4e | 2831 | exit +4, landing_queue -3 | A |
| f8f2a85a-6af2-4ca1-89dc-102dc9a6c644 | 1906 | exit +59, revise -58 | AC |
| fa2bf7e9-5b3d-40e0-8c11-612621f26106 | 2413 | exit +6, revise -3, landing_queue -3 | AC |
| fb02c9f3-9a3d-4751-aae8-3bb6fef663af | 2623 | work 15, exit +241, revise -31, landing_queue -210 | ACW1 |
| fb701575-4c3c-46eb-964a-73d19e142f8b | 2650 | work 6 | W1 |
| fd78c9a8-f3b9-4d04-ab03-705dc7679c63 | 2481 | exit +126, revise -51, landing_queue -76 | ACJ |
| fe3e4160-43a0-4c8d-b212-d7fcbc38433d | 2351 | exit +105, review +8, revise -105, conflict -5 | ACJ |

