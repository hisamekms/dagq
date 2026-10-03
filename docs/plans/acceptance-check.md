---
id: plan-acceptance-check
type: plan
title: worker が受け入れ条件を根拠と照合する変更の前の、run の review の差し戻しの基準値と、前後比較の script
status: active
created: 2026-10-03
updated: 2026-10-03
owners:
  - hisamekms
tags:
  - measurement
  - review
related:
  - plan-review-sendback-reasons
  - design-supervisor-lifecycle-review
  - adr-t947-1
  - adr-t980-1
---

# worker が受け入れ条件を根拠と照合する変更の前の、run の review の差し戻しの基準値と、前後比較の script

goal 90 の受け入れ条件 (3)、task 1422。task 1420（worker・resume・revise の prompt に受け入れ条件の各項目を根拠へ対応づける手順を足す）と task 1421（AGENTS.md と planner の参照）の変更の前の、run の review の初回通過率・差し戻し・acceptance_unmet・work・review・wait_to_land を、change・area・複雑さ・review の provider・worker の経路の層で出した。後続の task（goal 90 の受け入れ条件 (4)）が同じ script に後の区間を足して比べる。数字は event log の 1 回の集計で、繰り返しの測定はしていない。率の差はどれも観測で、原因は確かめていない。

**後の区間（task 1423）は暫定**: 観測の締切 C = 2026-10-03T01:25:00Z の時点で、task 1420 の着地 commit を含む binary の supervisor はまだ動いておらず（T は未確定）、後の区間の値は全て『未取得』。前の区間だけを C で出し直した（76 run）。goal 90 の受け入れ条件 (4) を満たすとは判定しない。[後の区間（task 1423、暫定）](#後の区間task-1423暫定)の章。

## 要点

- **主の前の区間（2026-10-02T03:07:27.872Z〜2026-10-03T00:12:09.306Z、run の review が Codex）の reviewed run は 75。初回通過 24（32.0%）、差し戻し 52（69.3%）、acceptance_unmet を含む verdict のある run 39・verdict 59（主の理由が acceptance_unmet の verdict 49）、run あたりの review 2.08 回。**
- 参考の区間（同じ長さの直前、Claude の review）は reviewed run 49、初回通過 32（65.3%）、差し戻し 17（34.7%）、acceptance_unmet 4 run・4 verdict（主 1）、review 1.69 回。2 つの区間では review の provider のほかに worker の経路・時期・task の中身も違うので、この差を review の provider の違いのためと読まない。
- 08:05:36Z（Claude の worker の既定を非対話に）の前は 11 run（全て対話の worker）で初回通過 27.3%・差し戻し 81.8%、後は 64 run（非対話 61）で 32.8%・67.2%。前の区間は 11 run しかない。
- 主の区間の中では、着地 commit の変更行数の上位 3 分の 1（459 行以上）の差し戻しが 87.5%、下位（178 行以下）が 56.0%。change では docs 90.0%・feature 85.7%・measure 87.5% に対し fix 40.0%・test 40.0%。
- 依頼の監査の数（03:07Z 以降、reviewed 35 run、差し戻し 26 run、acceptance_unmet 16 run・27 verdict）は、区間の終わりと観測の締切を goal 90 の登録の時刻（2026-10-02T14:44:46.885Z）にし、acceptance_unmet を「主の理由（`primary_code`）が acceptance_unmet」で数えると 4 つとも一致した（[監査との照合](#監査との照合)）。
- compute の run ごとの work・wait_to_land・land_phases の ask・route・actual_provider・areas は、`dagq stats` の値と 123 run の全てで一致した。食い違いは C の時点で終わっていない 1 run（stats の runs に無い）だけ（[stats との照合](#stats-との照合crosscheck)）。

## 基準

### 区間

時刻は UTC。どの区間も始まりを含み終わりを含まない [始まり, 終わり) で、境界は event の `created_at`（`dagq events` の `--since` が含み `--until` が含まないのと同じ）。run は、その run の最初の `review_finished`（verdict が pass・revise・concern のどれか）の `created_at` で区間に入れる（reviewed run）。`dagq stats` の `--since` / `--until`（run の終わりで選ぶ）では選ばない。`review_failed` だけの run は数えず、件数を別に書く（どの区間も 0。`review_failed` のある run は参考の区間の 1 run だけで、その run は後で verdict を受けたので reviewed run に入る）。

| 名前 | 区間 | reviewed run | 決め方 |
|---|---|---|---|
| S（主の前の区間の始まり） | 2026-10-02T03:07:27.872Z | — | 印 60152（run の review を Codex に） |
| E（主の前の区間の終わり） | 2026-10-03T00:12:09.306Z | — | この task の run（5667034f）の `run_claimed`（event 69700）。task 1420・1421 は C の時点で `run_integrated` が無い（1420 は実行中、1421 は ready）ので、早い方はこの run の claim |
| M（主の区間の中の境） | 2026-10-02T08:05:36Z | — | 印 61916（Claude の worker の既定を非対話に、task 1340） |
| main | [S, E) | 75 | 主の前の区間 |
| main_before_0805 | [S, M) | 11 | 08:05:36Z の前 |
| main_after_0805 | [M, E) | 64 | 08:05:36Z の後 |
| reference | [R, S)、R = 2026-10-01T06:02:46.438Z | 49 | 参考。R = S −（E − S）（E − S = 21 時間 4 分 41.434 秒）。run の review は Claude |
| audit | [S, A)、A = 2026-10-02T14:44:46.885Z、A より前の event だけ | 35 | 依頼の監査との照合用。A は goal 90 の `goal_created`（event 64178） |
| audit_c | [S, A)、C より前の event | 35 | 同じ run を C まで見たもの |

区間の中の印（normalized/ の `mark_recorded` / `mark_retracted`、時刻は payload の `at`）: [R, E) の中は 2 件で、印 60152（2026-10-02T03:07:27.872Z、run の review を Codex に）と印 61916（2026-10-02T08:05:36.000Z、Claude の worker の既定を非対話に）。取り消しは無い。参考に、reference/ の `dagq marks` には同じ範囲に自動更新による `supervisor_started`（handoff）が 72 件と、`derived:claude_version` / `derived:codex_version` が 1 件ずつ（2026-10-01T14:59:11Z）ある（比べる値には使わない）。

### 観測の締切 C

C = **2026-10-03T00:15:00Z**（E 以後で、snapshot を取った時刻の直前）。表と run の一覧の値は、`created_at` が C より前の event と git の commit だけから求める。C の後に増えた review・着地・receipt は数えない。snapshot は `docs/plans/acceptance-check/snapshot/2026-10-03T00:15:00Z/` に取ったが、本番 queue の dump（約 12MB）なので commit せず `.gitignore` で除く（公開の repository に queue の中身を出さないため。2026-10-03 の人の指示）。後の比較に要る area の対応表だけを `docs/plans/acceptance-check/areas-2026-10-03T00:15:00Z.toml` に残す。出力は `docs/plans/acceptance-check/out/2026-10-03T00:15:00Z/`。

### script と snapshot

`docs/plans/acceptance-check/` の下に置いた（python3 の標準ライブラリだけ。host の 3.9 で流した）。

| ファイル | 読むもの | 書くもの |
|---|---|---|
| `fetch.py` | 本番の queue を `dagq` の読むだけのコマンド（`events`・`show`・`stats`・`marks`・`status`）と git だけで。queue の DB は開かない。引数は区間の始まり（`--since`）・終わり（`--until`）・C（`--cutoff`） | `snapshot/<C>/normalized/` と `snapshot/<C>/reference/` |
| `compute.py` | `snapshot/<C>/normalized/` だけ（reference/ と crosscheck.csv は読まない）。引数は snapshot の directory と区間（`--interval 名前=始まり,終わり[,締切]`）と 3 分位を決める区間 | `out/<C>/runs.csv`（区間と run ごとに 1 行）、`out/<C>/table.csv`（区間と層ごとに 1 行）、`out/<C>/meta.json`（3 分位の境と区間） |
| `crosscheck.py` | `out/<C>/runs.csv` と `snapshot/<C>/reference/stats.json.gz` だけ（queue も normalized/ も読まない） | `out/<C>/crosscheck.csv`（compute は読まない） |
| `baseline.sh` | — | 上の 3 本をこの文書の引数で順に流す |

normalized/（比べる対象。compute はここだけを読む）:

- `events.jsonl`: 候補の run（`dagq events --full --kind review_finished --kind review_failed --since R --until E` に出た run。区間に入るかは compute が最初の verdict の時刻で決める）ごとの `dagq events --full --all --run ID --until C` と、その task ごとの `dagq events --full --all --task ID --until C`（`task_edited` と同じ task の他の run のため。task 1420・1421 も `--watch-task` で入れる）、印の `dagq events --full --all --kind mark_recorded --kind mark_retracted --until C`。どれも `--limit 1000 --after <前のページの最後の id>` で尽きるまで読み、event の id で重複を除き、id の昇順に 1 行 1 event、欄は `id`・`kind`・`run_id`・`task_id`・`goal_id`・`created_at`・`actor`・`payload` だけを key の昇順の JSON で書く（`cursor` などの読む時の欄は落とす）。11,367 event。
- `tasks.jsonl`: task ごとの `change` と受け入れ条件の項目数（`items`）。値は `dagq show ID --full` の今の値から、`created_at` がその task の最初の `run_claimed` 以後の `task_edited`（C で切らずに今までの全て）を新しい順に `from` へ戻した、最初の claim の時点の値。`from` に欄が無く `to` にだけある編集は戻せないので『未取得』にする（125 task で 0 件）。項目数は acceptance の中の `(数字)`（半角の括弧）の数で、0 なら 1。
- `commits.jsonl`: `run_integrated` の `commit`（古い payload は `result_commit`）ごとの `git show --numstat --no-renames --diff-merges=first-parent --format=` の追加・削除（binary は 0）とファイルの一覧。121 commit。
- `areas.toml`: `git rev-list -1 --before=C main` の commit（523ae7ff5e6e）の `dagq.toml` の `[areas]` の写し。area は stats を読まずに、commits.jsonl のファイルをこの対応表に `src/domain/scope.rs` と同じ glob の規則（`*` と `?` は 1 階層の中、`**` だけの階層は 0 個以上の階層）で通して求め、どれにも当たらないファイルがあれば `other` を足す。後の比較でもこの対応表を使い、変えるなら前の区間も出し直す。

reference/（参考資料。比べる値を裏づけるのには使わず、compute は読まない。crosscheck だけが `stats.json.gz` を読む）: `dagq stats --full --since 2026-09-30T00:00:00Z --until C`（`stats.json.gz`）、選んだ task の `dagq show ID --full`（`show.json.gz`）、`dagq marks`（`marks.json`）、`dagq status`（`status.json`）の生の出力と fetch の引数（`args.json`）。大きい 2 つは mtime を 0 にした gzip にした。`stats --until C` は event を C で切り詰めた再生ではなく、`running_alerts`・`workspace_check`・slot・open な待ちなどを読む時点の状態から求め、`show --full` は `--until` を持たず今の状態を出すので、どちらも比べる対象から外した。

### 各値の求め方

どれも [stats](../design/supervisor-lifecycle/stats.md) と [Review](../design/supervisor-lifecycle/review.md) の定義を normalized/ の event に当てて compute が求め、stats の出力は読まない。秒はミリ秒の差を 1000 で割った切り捨て（stats と同じ）。run の event は id の順に読む。

| 値 | 求め方 |
|---|---|
| reviewed run・verdict の並び | `review_finished` のうち `verdict` が pass・revise・concern のもの（id の順）。最初のものの時刻で区間に入れる |
| 初回通過 | 最初の verdict が pass の run。率は ÷ n |
| 差し戻し | revise か concern の verdict を 1 回以上受けた run。率は ÷ n |
| verdict の数 | pass・revise・concern 別の verdict の数（pass には差し戻しの後の pass も入る） |
| acceptance_unmet の run / verdict / 主 | `reason_codes`（項目ごとのコードの配列の配列）のどれかに `acceptance_unmet` を含む verdict が 1 つ以上ある run の数 / 含む verdict の数 / `primary_code` が `acceptance_unmet` の verdict の数。table.csv には主が acceptance_unmet の verdict のある run の数（`au_primary_runs`）も出す |
| 主の理由コードの分布 | revise と concern の verdict の `primary_code`（無ければ `reason_codes` の最初、それも無ければ `unlabeled`）ごとの verdict の数 |
| review/run | verdict の数 ÷ n |
| work | 最初の `run_claimed` → 最初の `receipt_observed`。`receipt_observed` の無い run（`receipt_missing` から resume された run など）は『未取得』 |
| work（待ち除く） | work から、`run_waiting_started`〜`run_waiting_ended` の区間のうち work の区間と重なる部分だけを引く（claim→着地の別の値からは引かない。task 1368 の二重の控除を繰り返さない）。終わりの無い待ちが work の終わりより前に始まっていれば『未完』 |
| review | C より前の全ての試行の `review_finished` と `review_failed` の `duration_secs` の合計（Review の 1・3・5） |
| review（待ち除く） | review から、待ちの区間のうち各試行の `review_started` → 同じ `attempt` の `review_finished` / `review_failed` と重なる部分を引く |
| wait_to_land | 最初の `validation_finished` → `run_integrated`。着地していない run は『未着地』 |
| wait_to_land（ask 除く） | wait_to_land − land_phases の `ask`。`ask` は stats の「着地待ちの内訳」の工程の切り替え（`domain::stats::landing::LandClock`）を compute.py の `LandClock` に移したもので、最初の `validation_finished` から、`ask_opened`（`blocked` と `planner_question` を除く）・`review_failed`・`integration_error`・`integration_held` で `ask` に入り、答えや他の工程の event で出るまでの秒 |
| route | 最初の `run_claimed` の `worker_mode` |
| actual_provider | C より前の最後の `provider_switched` の `to`、無ければ最初の `run_claimed` の `provider` |
| review の provider | 最初の `review_started` の `launch.provider`。run の中で provider の違う review があれば `review_providers` に並べる（どの区間も 0 run） |
| 状態 | `landed`（`run_integrated` がある）、`ended`（着地せず、C より前に最後に記録された payload の `status` が `failed` / `interrupted`。resume で取り戻された途中の `failed` は数えない）、`review 中`（どちらでもなく、最後の verdict が revise か concern でその後に review が無い）、`未着地`（それ以外） |
| kpi の first_pass | 参考。着地した run のうち、task の run が 1 つで、その run に `resume_started`・`revise_requested`・`integration_deferred` が無いもの（[kpi](../design/supervisor-lifecycle/kpi.md) の `first_pass_rate`）÷ 着地した run。初回通過率は review の最初の verdict だけを見るので、resume や着地の延期、同じ task の前の run があっても最初の verdict が pass なら数え、逆に kpi は review の無い run も数える（この表では reviewed run に限った） |
| 中央値・p90 | 中央値は偶数個なら中央 2 つの平均の切り捨て、p90 は nearest-rank（昇順で ceil(0.9×n) 番目）。stats と同じ。『未着地』『未取得』『未完』の run はその値の中央値と p90 から除き、『review 中』の run は review の中央値と p90 から除く。どれも件数を列に出す |

### 層

層ごとに表の 1 行。

- **all**
- **change**: task の最初の claim の時点の `change`（7 値。無い task は `unknown`）
- **area**: commits.jsonl を areas.toml に通した名前。1 つの run は持つ全ての area に数えるので、area の行の n の和は all の n を超える（主の区間で 2 つ以上の area を持つ run は 75 のうち 49）。着地 commit の無い run は `unknown`
- **items**: 受け入れ条件の項目数の 1〜3・4〜6・7 以上
- **lines**: 着地 commit の変更行数（追加 + 削除）の 3 分位。境は主の前の区間（main）の着地した 73 run で決め（昇順で ceil(n/3) 番目と ceil(2n/3) 番目）、178 行以下・179〜458 行・459 行以上（`>458`）。後の比較でも同じ境を使う（`out/<C>/meta.json` の `line_terciles`）
- **review_provider**: 最初の `review_started` の `launch.provider`
- **worker**: `route/actual_provider`

### 層ごとの表

`out/2026-10-03T00:15:00Z/table.csv` の値をそのまま写した（秒）。「未着地 / review 中 / 未取得」の未取得は、項目数・route・actual_provider・review の provider・work・review・変更行数のどれかが『未取得』か『未完』の run の数。「kpi first_pass」は kpi の first_pass の run の数 / 着地した run の数。主の理由コードの層ごとの分布と `au_primary_runs` は table.csv の `primary_codes` と `au_primary_runs` にある。run ごとの値（run・task・change・areas・項目数・変更行数・review の provider・worker の経路・verdict の並び・acceptance_unmet の有無・work・review・wait_to_land とその人の待ちを除いた値）は `out/2026-10-03T00:15:00Z/runs.csv`。

人の答えの待ち（`run_waiting_started`）は主の区間の reviewed run には 1 件も無く、work と review の「待ち除く」は含む値と同じになった。待ちのあった run は参考の区間の 1 run（task の change が feature・area が plugin・変更行数が 459 行以上の層の work の p90 だけが 4742 から 4719 になる）。wait_to_land の ask 除く値は land_phases の `ask`（`approve_landing` などの人の答えを待つ工程）を引いたもの。

#### 主の前の区間（main）[S, E)

| 層 | n | 初回通過 | 差し戻し | verdict pass/revise/concern | acceptance_unmet の run / verdict / 主 | review/run | work 中央・p90 | work 待ち除く | review 中央・p90 | review 待ち除く | wait_to_land 中央・p90 | wait_to_land ask 除く | 未着地 / review 中 / 未取得 | kpi first_pass |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| all | 75 | 24（32.0%） | 52（69.3%） | 64/68/24 | 39 / 59 / 49 | 2.08 | 854・2228 | 854・2228 | 144・457 | 144・457 | 1194・8734 | 1164・6047 | 2 / 1 / 2 | 22/73 |
| change=config | 3 | 0（0.0%） | 3（100.0%） | 3/3/0 | 0 / 0 / 0 | 2.00 | 211・373 | 211・373 | 77・92 | 77・92 | 207・395 | 207・395 | 0 / 0 / 0 | 0/3 |
| change=docs | 10 | 1（10.0%） | 9（90.0%） | 9/13/3 | 6 / 10 / 8 | 2.50 | 352・854 | 352・854 | 171・340 | 171・340 | 652・1367 | 652・1367 | 0 / 0 / 0 | 1/10 |
| change=feature | 21 | 3（14.3%） | 18（85.7%） | 16/24/14 | 15 / 21 / 15 | 2.57 | 1408・2326 | 1408・2326 | 252・620 | 252・620 | 2691・10080 | 1916・7339 | 1 / 1 / 1 | 3/20 |
| change=fix | 10 | 6（60.0%） | 4（40.0%） | 9/3/2 | 4 / 5 / 4 | 1.40 | 730・1460 | 730・1460 | 103・247 | 103・247 | 2091・6642 | 1720・4447 | 0 / 0 / 0 | 6/10 |
| change=measure | 8 | 2（25.0%） | 7（87.5%） | 7/10/1 | 7 / 11 / 11 | 2.25 | 690・10932 | 690・10932 | 253・583 | 253・583 | 655・7207 | 655・5692 | 0 / 0 / 0 | 2/8 |
| change=refactor | 9 | 4（44.4%） | 5（55.6%） | 7/5/3 | 4 / 7 / 7 | 1.67 | 1220・2217 | 1220・2217 | 117・486 | 117・486 | 1133・25548 | 1133・10261 | 1 / 0 / 1 | 5/8 |
| change=test | 10 | 6（60.0%） | 4（40.0%） | 10/5/1 | 3 / 5 / 4 | 1.60 | 925・2228 | 925・2228 | 73・141 | 73・141 | 1024・8494 | 1024・5458 | 0 / 0 / 0 | 5/10 |
| change=unknown | 4 | 2（50.0%） | 2（50.0%） | 3/5/0 | 0 / 0 / 0 | 2.00 | 546・4333 | 546・4333 | 134・389 | 134・389 | 846・9380 | 846・1336 | 0 / 0 / 0 | 0/4 |
| area=ci | 1 | 1（100.0%） | 0（0.0%） | 2/0/0 | 0 / 0 / 0 | 2.00 | 500・500 | 500・500 | 147・147 | 147・147 | 4447・4447 | 4447・4447 | 0 / 0 / 0 | 0/1 |
| area=config | 4 | 0（0.0%） | 4（100.0%） | 4/4/0 | 0 / 0 / 0 | 2.00 | 241・373 | 241・373 | 84・133 | 84・133 | 211・395 | 211・395 | 0 / 0 / 0 | 0/4 |
| area=docs | 59 | 16（27.1%） | 44（74.6%） | 50/56/18 | 35 / 51 / 42 | 2.10 | 884・2067 | 884・2067 | 187・486 | 187・486 | 1321・9380 | 1321・6698 | 0 / 0 / 1 | 15/59 |
| area=migrations | 3 | 1（33.3%） | 2（66.7%） | 1/0/3 | 2 / 3 / 2 | 1.33 | 1718・2326 | 1718・2326 | 117・252 | 117・252 | 7280・13397 | 3372・6698 | 0 / 0 / 0 | 1/3 |
| area=plugin | 13 | 3（23.1%） | 10（76.9%） | 12/12/5 | 5 / 9 / 6 | 2.23 | 820・1850 | 820・1850 | 199・620 | 199・620 | 1069・13397 | 1069・7339 | 0 / 0 / 0 | 3/13 |
| area=runtime | 42 | 14（33.3%） | 28（66.7%） | 35/29/15 | 23 / 31 / 24 | 1.88 | 1091・1850 | 1091・1850 | 144・457 | 144・457 | 1741・8734 | 1618・6861 | 0 / 0 / 1 | 14/42 |
| area=tests | 49 | 21（42.9%） | 28（57.1%） | 45/31/13 | 22 / 30 / 24 | 1.82 | 1087・2228 | 1087・2228 | 124・457 | 124・457 | 1494・8734 | 1438・6698 | 0 / 0 / 1 | 17/49 |
| area=unknown | 2 | 0（0.0%） | 2（100.0%） | 0/5/5 | 2 / 4 / 3 | 5.00 | 2756・2756 | 2756・2756 | 297・297 | 297・297 | ・ | ・ | 2 / 1 / 1 | 0/0 |
| items=1-3 | 31 | 11（35.5%） | 21（67.7%） | 27/29/9 | 16 / 21 / 15 | 2.10 | 681・2756 | 681・2756 | 126・255 | 126・255 | 1137・7207 | 1089・5458 | 1 / 1 / 0 | 8/30 |
| items=4-6 | 35 | 11（31.4%） | 24（68.6%） | 30/34/8 | 17 / 30 / 28 | 2.06 | 984・2217 | 984・2217 | 187・620 | 187・620 | 1069・13105 | 1069・6050 | 1 / 0 / 2 | 12/34 |
| items=7+ | 9 | 2（22.2%） | 7（77.8%） | 7/5/7 | 6 / 8 / 6 | 2.11 | 1126・1718 | 1126・1718 | 204・792 | 204・792 | 4274・7339 | 1994・7339 | 0 / 0 / 0 | 2/9 |
| lines=179-458 | 24 | 10（41.7%） | 15（62.5%） | 23/21/7 | 14 / 22 / 18 | 2.12 | 690・1636 | 690・1636 | 149・389 | 149・389 | 1344・7339 | 1243・5692 | 0 / 0 / 0 | 9/24 |
| lines=<=178 | 25 | 11（44.0%） | 14（56.0%） | 25/14/2 | 6 / 6 / 4 | 1.64 | 475・1920 | 475・1920 | 92・199 | 92・199 | 640・5458 | 640・4431 | 0 / 0 / 0 | 9/25 |
| lines=>458 | 24 | 3（12.5%） | 21（87.5%） | 16/28/10 | 17 / 27 / 24 | 2.25 | 1444・2228 | 1444・2228 | 250・620 | 250・620 | 1988・13397 | 1864・7687 | 0 / 0 / 1 | 4/24 |
| lines=未着地 | 2 | 0（0.0%） | 2（100.0%） | 0/5/5 | 2 / 4 / 3 | 5.00 | 2756・2756 | 2756・2756 | 297・297 | 297・297 | ・ | ・ | 2 / 1 / 1 | 0/0 |
| review_provider=codex | 75 | 24（32.0%） | 52（69.3%） | 64/68/24 | 39 / 59 / 49 | 2.08 | 854・2228 | 854・2228 | 144・457 | 144・457 | 1194・8734 | 1164・6047 | 2 / 1 / 2 | 22/73 |
| worker=headless/claude | 61 | 20（32.8%） | 41（67.2%） | 55/54/20 | 31 / 46 / 37 | 2.11 | 854・1718 | 854・1718 | 147・389 | 147・389 | 1164・7280 | 1150・4447 | 2 / 1 / 2 | 20/59 |
| worker=interactive/claude | 14 | 4（28.6%） | 11（78.6%） | 9/14/4 | 8 / 13 / 12 | 1.93 | 1280・8402 | 1280・8402 | 100・620 | 100・620 | 6100・13397 | 4096・7687 | 0 / 0 / 0 | 2/14 |

#### 08:05:36Z の前（main_before_0805）[S, M)

| 層 | n | 初回通過 | 差し戻し | verdict pass/revise/concern | acceptance_unmet の run / verdict / 主 | review/run | work 中央・p90 | work 待ち除く | review 中央・p90 | review 待ち除く | wait_to_land 中央・p90 | wait_to_land ask 除く | 未着地 / review 中 / 未取得 | kpi first_pass |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| all | 11 | 3（27.3%） | 9（81.8%） | 8/11/3 | 6 / 9 / 8 | 2.00 | 2228・8402 | 2228・8402 | 92・620 | 92・620 | 5458・13397 | 5458・7687 | 0 / 0 / 0 | 1/11 |
| change=config | 2 | 0（0.0%） | 2（100.0%） | 2/2/0 | 0 / 0 / 0 | 2.00 | 292・373 | 292・373 | 84・92 | 84・92 | 300・395 | 300・395 | 0 / 0 / 0 | 0/2 |
| change=feature | 3 | 0（0.0%） | 3（100.0%） | 0/4/2 | 2 / 3 / 2 | 2.00 | 2326・2395 | 2326・2395 | 224・620 | 224・620 | 13397・13869 | 7687・8018 | 0 / 0 / 0 | 0/3 |
| change=measure | 3 | 1（33.3%） | 3（100.0%） | 3/2/1 | 3 / 3 / 3 | 2.00 | 8402・10932 | 8402・10932 | 151・583 | 151・583 | 650・7207 | 650・5692 | 0 / 0 / 0 | 1/3 |
| change=test | 2 | 1（50.0%） | 1（50.0%） | 2/3/0 | 1 / 3 / 3 | 2.50 | 1419・2228 | 1419・2228 | 426・775 | 426・775 | 8387・11316 | 5754・6050 | 0 / 0 / 0 | 0/2 |
| change=unknown | 1 | 1（100.0%） | 0（0.0%） | 1/0/0 | 0 / 0 / 0 | 1.00 | 4333・4333 | 4333・4333 | 47・47 | 47・47 | 962・962 | 962・962 | 0 / 0 / 0 | 0/1 |
| area=config | 2 | 0（0.0%） | 2（100.0%） | 2/2/0 | 0 / 0 / 0 | 2.00 | 292・373 | 292・373 | 84・92 | 84・92 | 300・395 | 300・395 | 0 / 0 / 0 | 0/2 |
| area=docs | 8 | 1（12.5%） | 8（100.0%） | 5/8/3 | 5 / 6 / 5 | 2.00 | 2088・10932 | 2088・10932 | 121・620 | 121・620 | 3928・13869 | 3171・8018 | 0 / 0 / 0 | 1/8 |
| area=migrations | 1 | 0（0.0%） | 1（100.0%） | 0/0/1 | 1 / 1 / 0 | 1.00 | 2326・2326 | 2326・2326 | 69・69 | 69・69 | 13397・13397 | 6698・6698 | 0 / 0 / 0 | 0/1 |
| area=plugin | 2 | 0（0.0%） | 2（100.0%） | 0/3/1 | 1 / 1 / 0 | 2.00 | 2088・2326 | 2088・2326 | 344・620 | 344・620 | 13633・13869 | 7192・7687 | 0 / 0 / 0 | 0/2 |
| area=runtime | 3 | 0（0.0%） | 3（100.0%） | 0/4/2 | 2 / 3 / 2 | 2.00 | 2326・2395 | 2326・2395 | 224・620 | 224・620 | 13397・13869 | 7687・8018 | 0 / 0 / 0 | 0/3 |
| area=tests | 6 | 2（33.3%） | 4（66.7%） | 3/7/2 | 3 / 6 / 5 | 2.00 | 2277・4333 | 2277・4333 | 151・775 | 151・775 | 10698・13869 | 6374・8018 | 0 / 0 / 0 | 0/6 |
| items=1-3 | 7 | 3（42.9%） | 5（71.4%） | 7/4/2 | 3 / 4 / 4 | 1.86 | 2395・10932 | 2395・10932 | 78・224 | 78・224 | 962・10080 | 962・8018 | 0 / 0 / 0 | 1/7 |
| items=4-6 | 4 | 0（0.0%） | 4（100.0%） | 1/7/1 | 3 / 5 / 4 | 2.25 | 2039・2326 | 2039・2326 | 601・775 | 601・775 | 12356・13869 | 6374・7687 | 0 / 0 / 0 | 0/4 |
| lines=179-458 | 2 | 1（50.0%） | 2（100.0%） | 2/1/1 | 2 / 2 / 2 | 2.00 | 9667・10932 | 9667・10932 | 109・151 | 109・151 | 3720・7207 | 2962・5692 | 0 / 0 / 0 | 1/2 |
| lines=<=178 | 5 | 2（40.0%） | 3（60.0%） | 6/3/0 | 1 / 1 / 1 | 1.80 | 441・4333 | 441・4333 | 78・583 | 78・583 | 650・5458 | 650・5458 | 0 / 0 / 0 | 0/5 |
| lines=>458 | 4 | 0（0.0%） | 4（100.0%） | 0/7/2 | 3 / 6 / 5 | 2.25 | 2277・2395 | 2277・2395 | 422・775 | 422・775 | 12356・13869 | 7192・8018 | 0 / 0 / 0 | 0/4 |
| review_provider=codex | 11 | 3（27.3%） | 9（81.8%） | 8/11/3 | 6 / 9 / 8 | 2.00 | 2228・8402 | 2228・8402 | 92・620 | 92・620 | 5458・13397 | 5458・7687 | 0 / 0 / 0 | 1/11 |
| worker=interactive/claude | 11 | 3（27.3%） | 9（81.8%） | 8/11/3 | 6 / 9 / 8 | 2.00 | 2228・8402 | 2228・8402 | 92・620 | 92・620 | 5458・13397 | 5458・7687 | 0 / 0 / 0 | 1/11 |

#### 08:05:36Z の後（main_after_0805）[M, E)

| 層 | n | 初回通過 | 差し戻し | verdict pass/revise/concern | acceptance_unmet の run / verdict / 主 | review/run | work 中央・p90 | work 待ち除く | review 中央・p90 | review 待ち除く | wait_to_land 中央・p90 | wait_to_land ask 除く | 未着地 / review 中 / 未取得 | kpi first_pass |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| all | 64 | 21（32.8%） | 43（67.2%） | 56/57/21 | 33 / 50 / 41 | 2.09 | 837・1658 | 837・1658 | 147・389 | 147・389 | 1179・7280 | 1157・4447 | 2 / 1 / 2 | 21/62 |
| change=config | 1 | 0（0.0%） | 1（100.0%） | 1/1/0 | 0 / 0 / 0 | 2.00 | 47・47 | 47・47 | 77・77 | 77・77 | 207・207 | 207・207 | 0 / 0 / 0 | 0/1 |
| change=docs | 10 | 1（10.0%） | 9（90.0%） | 9/13/3 | 6 / 10 / 8 | 2.50 | 352・854 | 352・854 | 171・340 | 171・340 | 652・1367 | 652・1367 | 0 / 0 / 0 | 1/10 |
| change=feature | 18 | 3（16.7%） | 15（83.3%） | 16/20/12 | 13 / 18 / 13 | 2.67 | 1253・2067 | 1253・2067 | 253・704 | 253・704 | 1659・7280 | 1659・6861 | 1 / 1 / 1 | 3/17 |
| change=fix | 10 | 6（60.0%） | 4（40.0%） | 9/3/2 | 4 / 5 / 4 | 1.40 | 730・1460 | 730・1460 | 103・247 | 103・247 | 2091・6642 | 1720・4447 | 0 / 0 / 0 | 6/10 |
| change=measure | 5 | 1（20.0%） | 4（80.0%） | 4/8/0 | 4 / 8 / 8 | 2.40 | 672・745 | 672・745 | 255・379 | 255・379 | 660・6742 | 660・3492 | 0 / 0 / 0 | 1/5 |
| change=refactor | 9 | 4（44.4%） | 5（55.6%） | 7/5/3 | 4 / 7 / 7 | 1.67 | 1220・2217 | 1220・2217 | 117・486 | 117・486 | 1133・25548 | 1133・10261 | 1 / 0 / 1 | 5/8 |
| change=test | 8 | 5（62.5%） | 3（37.5%） | 8/2/1 | 2 / 2 / 1 | 1.38 | 925・2288 | 925・2288 | 61・141 | 61・141 | 715・8494 | 715・4431 | 0 / 0 / 0 | 5/8 |
| change=unknown | 3 | 1（33.3%） | 2（66.7%） | 2/5/0 | 0 / 0 / 0 | 2.33 | 525・567 | 525・567 | 199・389 | 199・389 | 731・9380 | 731・1336 | 0 / 0 / 0 | 0/3 |
| area=ci | 1 | 1（100.0%） | 0（0.0%） | 2/0/0 | 0 / 0 / 0 | 2.00 | 500・500 | 500・500 | 147・147 | 147・147 | 4447・4447 | 4447・4447 | 0 / 0 / 0 | 0/1 |
| area=config | 2 | 0（0.0%） | 2（100.0%） | 2/2/0 | 0 / 0 / 0 | 2.00 | 159・272 | 159・272 | 105・133 | 105・133 | 211・215 | 211・215 | 0 / 0 / 0 | 0/2 |
| area=docs | 51 | 15（29.4%） | 36（70.6%） | 45/48/15 | 30 / 45 / 37 | 2.12 | 854・1640 | 854・1640 | 199・441 | 199・441 | 1321・7339 | 1321・4701 | 0 / 0 / 1 | 14/51 |
| area=migrations | 2 | 1（50.0%） | 1（50.0%） | 1/0/2 | 1 / 2 / 2 | 1.50 | 1441・1718 | 1441・1718 | 184・252 | 184・252 | 4151・7280 | 2197・3372 | 0 / 0 / 0 | 1/2 |
| area=plugin | 11 | 3（27.3%） | 8（72.7%） | 12/9/4 | 4 / 8 / 6 | 2.27 | 783・1641 | 783・1641 | 199・268 | 199・268 | 872・7339 | 872・6047 | 0 / 0 / 0 | 3/11 |
| area=runtime | 39 | 14（35.9%） | 25（64.1%） | 35/25/13 | 21 / 28 / 22 | 1.87 | 1056・1658 | 1056・1658 | 141・457 | 141・457 | 1655・7339 | 1520・5632 | 0 / 0 / 1 | 14/39 |
| area=tests | 43 | 19（44.2%） | 24（55.8%） | 42/24/11 | 19 / 24 / 19 | 1.79 | 1007・1718 | 1007・1718 | 124・293 | 124・293 | 1245・6913 | 1245・4447 | 0 / 0 / 1 | 17/43 |
| area=unknown | 2 | 0（0.0%） | 2（100.0%） | 0/5/5 | 2 / 4 / 3 | 5.00 | 2756・2756 | 2756・2756 | 297・297 | 297・297 | ・ | ・ | 2 / 1 / 1 | 0/0 |
| items=1-3 | 24 | 8（33.3%） | 16（66.7%） | 20/25/7 | 13 / 17 / 11 | 2.17 | 676・1372 | 676・1372 | 141・279 | 141・279 | 1164・6913 | 1110・4431 | 1 / 1 / 0 | 7/23 |
| items=4-6 | 31 | 11（35.5%） | 20（64.5%） | 29/27/7 | 14 / 25 / 24 | 2.03 | 937・2067 | 937・2067 | 133・379 | 133・379 | 1037・3804 | 1037・3482 | 1 / 0 / 2 | 12/30 |
| items=7+ | 9 | 2（22.2%） | 7（77.8%） | 7/5/7 | 6 / 8 / 6 | 2.11 | 1126・1718 | 1126・1718 | 204・792 | 204・792 | 4274・7339 | 1994・7339 | 0 / 0 / 0 | 2/9 |
| lines=179-458 | 22 | 9（40.9%） | 13（59.1%） | 21/20/6 | 12 / 20 / 16 | 2.14 | 672・1253 | 672・1253 | 161・389 | 161・389 | 1344・7339 | 1243・5632 | 0 / 0 / 0 | 8/22 |
| lines=<=178 | 20 | 9（45.0%） | 11（55.0%） | 19/11/2 | 5 / 5 / 3 | 1.60 | 521・1164 | 521・1164 | 97・167 | 97・167 | 632・1823 | 632・1823 | 0 / 0 / 0 | 9/20 |
| lines=>458 | 20 | 3（15.0%） | 17（85.0%） | 16/21/8 | 14 / 21 / 19 | 2.25 | 1318・2067 | 1318・2067 | 250・457 | 250・457 | 1657・6913 | 1618・3482 | 0 / 0 / 1 | 4/20 |
| lines=未着地 | 2 | 0（0.0%） | 2（100.0%） | 0/5/5 | 2 / 4 / 3 | 5.00 | 2756・2756 | 2756・2756 | 297・297 | 297・297 | ・ | ・ | 2 / 1 / 1 | 0/0 |
| review_provider=codex | 64 | 21（32.8%） | 43（67.2%） | 56/57/21 | 33 / 50 / 41 | 2.09 | 837・1658 | 837・1658 | 147・389 | 147・389 | 1179・7280 | 1157・4447 | 2 / 1 / 2 | 21/62 |
| worker=headless/claude | 61 | 20（32.8%） | 41（67.2%） | 55/54/20 | 31 / 46 / 37 | 2.11 | 854・1718 | 854・1718 | 147・389 | 147・389 | 1164・7280 | 1150・4447 | 2 / 1 / 2 | 20/59 |
| worker=interactive/claude | 3 | 1（33.3%） | 2（66.7%） | 1/3/1 | 2 / 4 / 4 | 1.67 | 672・711 | 672・711 | 109・255 | 109・255 | 6742・8734 | 3492・4701 | 0 / 0 / 0 | 1/3 |

#### 参考の区間（reference）[R, S)、Claude の review

| 層 | n | 初回通過 | 差し戻し | verdict pass/revise/concern | acceptance_unmet の run / verdict / 主 | review/run | work 中央・p90 | work 待ち除く | review 中央・p90 | review 待ち除く | wait_to_land 中央・p90 | wait_to_land ask 除く | 未着地 / review 中 / 未取得 | kpi first_pass |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| all | 49 | 32（65.3%） | 17（34.7%） | 57/25/1 | 4 / 4 / 1 | 1.69 | 659・3386 | 659・3386 | 50・267 | 50・267 | 873・2853 | 873・2853 | 1 / 0 / 3 | 15/48 |
| change=config | 3 | 2（66.7%） | 1（33.3%） | 4/1/0 | 0 / 0 / 0 | 1.67 | 235・407 | 235・407 | 43・97 | 43・97 | 138・1105 | 138・1105 | 0 / 0 / 0 | 2/3 |
| change=docs | 6 | 6（100.0%） | 0（0.0%） | 6/0/0 | 0 / 0 / 0 | 1.00 | 398・538 | 398・538 | 23・45 | 23・45 | 205・445 | 205・445 | 0 / 0 / 0 | 6/6 |
| change=feature | 10 | 2（20.0%） | 8（80.0%） | 14/13/1 | 2 / 2 / 0 | 2.80 | 2374・4742 | 2374・4719 | 172・500 | 172・500 | 1749・14434 | 1749・14016 | 0 / 0 / 0 | 2/10 |
| change=fix | 11 | 7（63.6%） | 4（36.4%） | 11/4/0 | 0 / 0 / 0 | 1.36 | 964・2969 | 964・2969 | 151・261 | 151・261 | 821・1257 | 821・1257 | 0 / 0 / 0 | 4/11 |
| change=measure | 2 | 1（50.0%） | 1（50.0%） | 2/1/0 | 0 / 0 / 0 | 1.50 | 290・404 | 290・404 | 77・97 | 77・97 | 58623・116114 | 45931・90729 | 0 / 0 / 0 | 0/2 |
| change=refactor | 2 | 2（100.0%） | 0（0.0%） | 2/0/0 | 0 / 0 / 0 | 1.00 | 1412・2389 | 1412・2389 | 34・44 | 34・44 | 724・944 | 724・944 | 0 / 0 / 0 | 0/2 |
| change=test | 6 | 6（100.0%） | 0（0.0%） | 6/0/0 | 0 / 0 / 0 | 1.00 | 574・2232 | 574・2232 | 26・50 | 26・50 | 635・1330 | 635・1330 | 0 / 0 / 0 | 1/6 |
| change=unknown | 9 | 6（66.7%） | 3（33.3%） | 12/6/0 | 2 / 2 / 1 | 2.00 | 586・884 | 586・884 | 52・1001 | 52・1001 | 1217・2837 | 1217・2837 | 1 / 0 / 3 | 0/8 |
| area=ci | 2 | 2（100.0%） | 0（0.0%） | 2/0/0 | 0 / 0 / 0 | 1.00 | 211・235 | 211・235 | 35・43 | 35・43 | 88・138 | 88・138 | 0 / 0 / 0 | 2/2 |
| area=config | 2 | 0（0.0%） | 2（100.0%） | 4/3/1 | 1 / 1 / 0 | 4.00 | 895・1383 | 895・1383 | 182・267 | 182・267 | 16601・32097 | 16294・31484 | 0 / 0 / 0 | 0/2 |
| area=docs | 41 | 25（61.0%） | 16（39.0%） | 49/21/1 | 3 / 3 / 0 | 1.73 | 676・4144 | 676・4144 | 55・261 | 55・261 | 821・2853 | 821・2853 | 0 / 0 / 2 | 14/41 |
| area=plugin | 12 | 4（33.3%） | 8（66.7%） | 15/13/1 | 3 / 3 / 0 | 2.42 | 820・4742 | 820・4719 | 149・500 | 149・500 | 1037・14434 | 1037・14016 | 0 / 0 / 0 | 2/12 |
| area=runtime | 30 | 16（53.3%） | 14（46.7%） | 37/19/1 | 3 / 3 / 0 | 1.90 | 1212・4446 | 1212・4446 | 111・267 | 111・267 | 1093・2853 | 1093・2853 | 0 / 0 / 2 | 6/30 |
| area=tests | 33 | 19（57.6%） | 14（42.4%） | 39/19/1 | 3 / 3 / 0 | 1.79 | 1004・3386 | 1004・3386 | 72・267 | 72・267 | 1148・2853 | 1148・2853 | 0 / 0 / 2 | 5/33 |
| area=unknown | 1 | 0（0.0%） | 1（100.0%） | 1/4/0 | 1 / 1 / 1 | 5.00 | ・ | ・ | 1001・1001 | 1001・1001 | ・ | ・ | 1 / 0 / 1 | 0/0 |
| items=1-3 | 32 | 22（68.8%） | 10（31.2%） | 38/16/1 | 2 / 2 / 1 | 1.72 | 659・3386 | 659・3386 | 44・211 | 44・211 | 642・1993 | 642・1993 | 1 / 0 / 2 | 11/31 |
| items=4-6 | 14 | 8（57.1%） | 6（42.9%） | 16/8/0 | 2 / 2 / 0 | 1.71 | 616・2389 | 616・2389 | 77・267 | 77・267 | 1209・4533 | 1209・4153 | 0 / 0 / 1 | 3/14 |
| items=7+ | 3 | 2（66.7%） | 1（33.3%） | 3/1/0 | 0 / 0 / 0 | 1.33 | 964・2389 | 964・2389 | 179・261 | 179・261 | 774・926 | 774・926 | 0 / 0 / 0 | 1/3 |
| lines=179-458 | 16 | 13（81.2%） | 3（18.8%） | 18/3/0 | 0 / 0 / 0 | 1.31 | 658・2969 | 658・2969 | 48・152 | 48・152 | 859・1328 | 859・1328 | 0 / 0 / 2 | 4/16 |
| lines=<=178 | 17 | 15（88.2%） | 2（11.8%） | 18/2/0 | 0 / 0 / 0 | 1.18 | 369・853 | 369・853 | 28・57 | 28・57 | 596・1330 | 596・1330 | 0 / 0 / 0 | 8/17 |
| lines=>458 | 15 | 4（26.7%） | 11（73.3%） | 20/16/1 | 3 / 3 / 0 | 2.47 | 1435・4742 | 1435・4719 | 187・500 | 187・500 | 1506・14434 | 1506・14016 | 0 / 0 / 0 | 3/15 |
| lines=未着地 | 1 | 0（0.0%） | 1（100.0%） | 1/4/0 | 1 / 1 / 1 | 5.00 | ・ | ・ | 1001・1001 | 1001・1001 | ・ | ・ | 1 / 0 / 1 | 0/0 |
| review_provider=claude | 49 | 32（65.3%） | 17（34.7%） | 57/25/1 | 4 / 4 / 1 | 1.69 | 659・3386 | 659・3386 | 50・267 | 50・267 | 873・2853 | 873・2853 | 1 / 0 / 3 | 15/48 |
| worker=headless/claude | 10 | 8（80.0%） | 2（20.0%） | 10/2/0 | 0 / 0 / 0 | 1.20 | 605・4144 | 605・4144 | 39・179 | 39・179 | 725・1148 | 725・1148 | 0 / 0 / 0 | 4/10 |
| worker=headless/codex | 4 | 1（25.0%） | 3（75.0%） | 5/6/0 | 1 / 1 / 1 | 2.75 | 404・404 | 404・404 | 98・1001 | 98・1001 | 2537・116114 | 2537・90729 | 1 / 0 / 3 | 0/3 |
| worker=interactive/claude | 35 | 23（65.7%） | 12（34.3%） | 42/17/1 | 3 / 3 / 0 | 1.71 | 688・2969 | 688・2969 | 50・267 | 50・267 | 944・2853 | 944・2853 | 0 / 0 / 0 | 11/35 |

#### 主の理由コード（all）

| 区間 | 主の理由コード（verdict 数） |
|---|---|
| main | acceptance_ambiguous:1、acceptance_infeasible:2、acceptance_unmet:49、adr_conflict:1、adr_design_mismatch:15、code_defect:6、docs_drift:3、out_of_scope_change:1、repo_rule_violation:12、test_gap:2 |
| main_before_0805 | acceptance_infeasible:1、acceptance_unmet:8、code_defect:1、docs_drift:2、repo_rule_violation:2 |
| main_after_0805 | acceptance_ambiguous:1、acceptance_infeasible:1、acceptance_unmet:41、adr_conflict:1、adr_design_mismatch:15、code_defect:5、docs_drift:1、out_of_scope_change:1、repo_rule_violation:10、test_gap:2 |
| reference | acceptance_unmet:1、adr_conflict:1、adr_design_mismatch:1、code_defect:3、docs_drift:13、local_slip:2、repo_rule_violation:1、test_gap:4 |
| audit | acceptance_ambiguous:1、acceptance_infeasible:1、acceptance_unmet:27、adr_conflict:1、adr_design_mismatch:2、code_defect:2、docs_drift:3、out_of_scope_change:1、repo_rule_violation:3、test_gap:1 |
| audit_c | acceptance_ambiguous:1、acceptance_infeasible:1、acceptance_unmet:28、adr_conflict:1、adr_design_mismatch:2、code_defect:2、docs_drift:3、out_of_scope_change:1、repo_rule_violation:3、test_gap:1 |

### 監査との照合

依頼の監査（goal 90 の記述: 03:07Z 以降、reviewed 35 run、差し戻し 26 run、acceptance_unmet 16 run・27 verdict）を同じ script で出した。監査の区間の終わりは記録に無いので、goal 90 を登録した時刻 A = 2026-10-02T14:44:46.885Z を終わりにした。

| 区間 | reviewed run | 差し戻し run | acceptance_unmet を含む verdict のある run / その verdict | 主が acceptance_unmet の run / verdict |
|---|---|---|---|---|
| 監査（goal 90 の記述） | 35 | 26 | — | 16 / 27 |
| audit: [S, A)、A より前の event | 35 | 26 | 20 / 31 | **16 / 27** |
| audit_c: [S, A)、C より前の event | 35 | 26 | 20 / 32 | 16 / 28 |
| main: [S, E)、C より前の event | 75 | 52 | 39 / 59 | 31 / 49 |

- 区間の終わりと観測の締切を A にし、acceptance_unmet を主の理由（`primary_code`）で数えると、4 つの数が監査と一致した。監査の 16 run・27 verdict は「主の理由が acceptance_unmet」の数で、この文書の表の「acceptance_unmet の run / verdict」（`reason_codes` のどこかに含む）より狭い。表には両方を出した（`au_runs`・`au_verdicts` と `au_primary_runs`・`au_primary`）。
- 同じ 35 run を C まで見る（audit_c）と、A の後に task 1334 の run が 2 回目の concern（主の理由 acceptance_unmet）を受け、acceptance_unmet の verdict が 1 つ増えた（31 → 32、主 27 → 28）。task 1389 の run も A の後に着地した。差し戻しの run の数は変わらない。
- 主の区間の終わりは E で、A の後の 40 run が加わる。

### stats との照合（crosscheck）

`crosscheck.py` が `runs.csv` の 124 run（どれかの区間に入った run）の work・wait_to_land・land_phases の ask・route・actual_provider・areas を、reference/ の `dagq stats --full --since 2026-09-30T00:00:00Z --until C` の run ごとの値と比べ、食い違いを `out/2026-10-03T00:15:00Z/crosscheck.csv` に書いた。

- stats の runs にある 123 run は 6 つの欄が全て一致した（compute の『未取得』『未着地』と stats の null は一致と見なす）。
- 食い違いは 1 件で、task 1405 の run 42637110 が stats の runs に無い（compute では『review 中』）。stats の runs は終わった run（着地か `failed` / `interrupted`）だけを出すため。
- crosscheck.csv は表の値にも run の一覧にも戻していない。

### 再現性

- 同じ引数（同じ C）で fetch を 2 回流し（2 回目は `OUT=/tmp/refetch1422 sh baseline.sh fetch`）、`diff -r` で normalized/ の 4 ファイルが byte で同じだった。reference/ は今の状態を含むので比べていない。
- 同じ snapshot で compute を 2 回流し、`runs.csv`・`table.csv`・`meta.json` が `cmp` で同じだった。続けて crosscheck を 2 回流し、`crosscheck.csv` が同じだった。

### 再実行のコマンド

`docs/plans/acceptance-check/` で、固定バイナリ（`~/.local/bin/dagq`）を PATH に置いて、fetch → compute → crosscheck の順に流す。`sh baseline.sh` は 3 つを順に流し、`sh baseline.sh fetch` / `compute` / `crosscheck` は 1 つだけ流す。中身は次と同じ。

```sh
cd docs/plans/acceptance-check
# 1. fetch（queue を読むだけ。区間の始まり R・終わり E・C）
python3 fetch.py --since 2026-10-01T06:02:46.438Z --until 2026-10-03T00:12:09.306Z \
  --cutoff 2026-10-03T00:15:00Z --stats-since 2026-09-30T00:00:00Z \
  --watch-task 1420 --watch-task 1421 --repo ../../.. --out snapshot/2026-10-03T00:15:00Z
# 2. compute（normalized/ だけを読む）
python3 compute.py snapshot/2026-10-03T00:15:00Z \
  --interval main=2026-10-02T03:07:27.872Z,2026-10-03T00:12:09.306Z \
  --interval main_before_0805=2026-10-02T03:07:27.872Z,2026-10-02T08:05:36Z \
  --interval main_after_0805=2026-10-02T08:05:36Z,2026-10-03T00:12:09.306Z \
  --interval reference=2026-10-01T06:02:46.438Z,2026-10-02T03:07:27.872Z \
  --interval audit=2026-10-02T03:07:27.872Z,2026-10-02T14:44:46.885Z,2026-10-02T14:44:46.885Z \
  --interval audit_c=2026-10-02T03:07:27.872Z,2026-10-02T14:44:46.885Z \
  --terciles-from main --changed-task 1420 --changed-task 1421
# 3. crosscheck（runs.csv と reference/ の stats だけを読む）
python3 crosscheck.py snapshot/2026-10-03T00:15:00Z
```

### 限界

- **本数**: 主の区間は 75 run で、08:05:36Z の前は 11 run しかない。層の多くは n が 10 未満で、1 run で率が 10 ポイント以上動く。率の差はどれも観測で、因果は確かめていない。
- **area の重なり**: 1 つの run を持つ全ての area に数えるので、主の区間で 49 run（65%）が 2 つ以上の area に入る（docs と tests と runtime が多い）。area の行どうしは独立の群ではない。
- **review の provider の切り替わり**: 主の区間の 75 run は全て Codex の review、参考の区間の 49 run は全て Claude の review で、run の中で provider の違う review を受けた run は無い。主と参考の差は provider・時期・task の中身・worker の経路の違いを全て含む。Codex の review は理由コードの付け方も違いうる（主の区間は acceptance_unmet が主の理由の revise・concern の verdict 49 / 92、参考の区間は docs_drift が 13 / 26）。
- **worker の経路の切り替わり**: 08:05:36Z の前の 11 run は全て対話の Claude、後の 64 run は非対話の Claude 61・対話の Claude 3。参考の区間は対話の Claude 35・非対話の Claude 10・非対話の Codex 4 で、途中で provider を切り替えた run が 4 ある（主の区間は 0）。
- **task 1420・1421 の着地の後に review か resume を受けた選んだ run**: 0（C の時点で 1420・1421 のどちらも着地していない）。後の比較では、後の区間の始まりを 1420・1421 の最初の `run_integrated` 以後に置く。
- **C と E の近さ**: C は E の 2 分 51 秒後。C の時点で主の区間に未着地が 2 run（『review 中』の task 1405 と、`failed` で終わった task 1334）あり、2 run とも wait_to_land の中央値と p90 に入っていない。review の時間の中央値と p90 から除いたのは『review 中』の 1405 だけで、1334 の review（297 秒）は入っている。
- **受け入れ条件の項目数**: acceptance の中の半角の `(数字)` を数えるので、箇条書き（`-`）や全角の括弧で書いた task は 1 になり、本文で `(1)` を引き直した task は多く数える。参考の区間は項目数 1〜3 が 32 / 49 と多い。
- **change の unknown**: change の無い task（ADR-t980-1 より前の登録か、claim の時点で change が無かったもの）の run が主の区間に 4、参考の区間に 9 ある。
- **『未取得』**: work は `receipt_observed` の無い run（`receipt_missing` から resume された run）で求められず、主の区間で 2 run（task 1334・1372）、参考の区間で 3 run（task 955・979・995）。
- **kpi の first_pass**: reviewed run に限った値で、kpi の `first_pass_rate`（期間に着地した全ての run）とは分母が違う。

## 後の比較の仕方

後続の task（goal 90 の受け入れ条件 (4)）は、1420・1421 の最初の `run_integrated` 以後の同じ本数（75）の reviewed run を後の区間にし、新しい C で fetch して、同じ定義の表を出す。層をそろえるため、area の対応表と変更行数の 3 分位の境はこの基準のものを使う。

```sh
cd docs/plans/acceptance-check
python3 fetch.py --since <後の始まり> --until <後の終わり> --cutoff <新しい C> \
  --areas-from areas-2026-10-03T00:15:00Z.toml --repo ../../.. \
  --out snapshot/<新しい C>
python3 compute.py snapshot/<新しい C> --interval after=<後の始まり>,<後の終わり> \
  --line-terciles 178,458 --changed-task 1420 --changed-task 1421
python3 crosscheck.py snapshot/<新しい C>
```

前の区間の値はこの文書の表と `out/2026-10-03T00:15:00Z/` のまま使い、出し直さない（対応表か境を変えるときだけ、前の区間も同じ引数で出し直す）。

task 1423 は、前の区間の終わりを 1420・1421 の最初の `run_integrated` まで延ばし、前と後を同じ C の snapshot から求めることになったので、この手順の代わりに `after.sh` で前の区間も出し直す（次の章）。

## 後の区間（task 1423、暫定）

goal 90 の受け入れ条件 (4)、task 1423。**暫定の報告**: 観測の締切 C の時点で T（task 1420 の着地 commit を含む binary で supervisor が動き始めた時刻）が無く、後の区間の値は全て『未取得』。前の区間だけを同じ C の snapshot から出し直した。goal 90 の受け入れ条件 (4) を満たすとは判定しない。後の区間が前の区間の本数 N にそろってから、同じ手順で測り直す（receipt の follow_up、category measurement）。

### 要点（暫定）

- **T は未確定**。task 1420 の着地（`run_integrated` event 69870、2026-10-03T00:29:27.268Z、commit `23c4fefd9d31`）の後、C より前に 23c4fefd を祖先に持つ version の `supervisor_started` も `update_installed` も無い。23c4fefd の自動更新は e2e の関門で落ち（`update_failed` event 69933、00:35:36.974Z、stage `e2e`、落ちた test は `two_independent_tasks_run_concurrently_and_a_dependent_follows_integration`）、`update_failed` の ask 352 に `skip` が答えられた。C より前の最後の `supervisor_started` は event 69467（2026-10-02T23:54:59.754Z、`handoff: true`、`0.4.0-dev+e994a99f750b`。e994a99f は 23c4fefd を祖先に持たない）。
- **前の区間の reviewed run は N = 76**（[S, E2)、E2 = 1420 の着地）。初回通過 25（32.9%）、差し戻し 52（68.4%）、acceptance_unmet を含む verdict のある run 39・verdict 60（主 50）、review/run 2.12、work 中央値 884 秒・p90 2228 秒、review 144・486 秒、wait_to_land 1179・8734 秒（ask 除く 1157・6047 秒）。人の答えの待ちは work と review に無く、待ち除く値は含む値と同じ。
- **後の区間は 0 本（未取得）**。初回通過率・差し戻し・acceptance_unmet・work・review・wait_to_land の前後の差は出せない。構成比で重み付けした率、同じ review の provider と worker の経路どうしの行、対応が書かれた run の割合の後の値も『未取得』。
- **前の区間で receipt の summary が受け入れ条件の番号ごとに根拠を挙げていた run** は、番号のある 46 run のうち 5（10.9%）。番号の無い task の run が 30。後の区間の同じ割合と比べる基準になる（判定の規則は[対応が書かれた run の割合](#対応が書かれた-run-の割合)）。
- 参考（C の後、比べる値に使わない）: 01:28:15.711Z に 33cc0410（23c4fefd を祖先に持つ）の `supervisor_started`（event 70602、handoff）と `update_installed`（event 70612）が記録された。測り直すときの T はこの時刻と 1421 の着地（01:09:04.553Z）の遅い方になる見込みだが、測り直しの snapshot の normalized/ から同じ規則で求め直す。

### T の求め方と値

T = 次の 2 つの遅い方。どちらも normalized/events.jsonl（created_at < C）と normalized/versions.jsonl から compute が求め、`out/<C>/t.json` に根拠の event の ID と created_at を書く。

1. `supervisor_started`（version は `payload.dagq_version`、`payload.handoff` が true の自動更新の引き継ぎも false の `up` などの起動も）か `update_installed`（version は `payload.commit`）のうち、version の commit が task 1420 の着地 commit を祖先に持つ（`git merge-base --is-ancestor`。fetch が versions.jsonl に書く）最初のもの
2. task 1421 の最初の `run_integrated`

`mark_recorded` は根拠にしない（人・planner の `dagq mark` の記録で、`dagq marks` の「supervisor handed off to …」の行は `supervisor_started` から作られる表示）。

| 部分 | C の時点の値 | 根拠 |
|---|---|---|
| 1420 の着地 | 2026-10-03T00:29:27.268Z | `run_integrated` event 69870、commit 23c4fefd9d31a1db7ddccc8d23cec94dacaae697 |
| 1420 を含む binary の最初の起動 | **無し** | 1420 の着地以後の `supervisor_started`・`update_installed`・`update_failed` は `update_failed` event 69933（00:35:36.974Z、commit 23c4fefd、stage e2e）だけ |
| 1421 の着地 | 2026-10-03T01:09:04.553Z | `run_integrated` event 70392、commit 3189b7e1 |
| T | **未確定** | 1 が無いため |

未確定の根拠（normalized/ の C より前の event）:

- C より前の最後の `supervisor_started`: event 69467、2026-10-02T23:54:59.754Z、`handoff: true`、`dagq_version` `0.4.0-dev+e994a99f750b3f07aee1e3e4eb26d4dec3bc0afc`（23c4fefd を祖先に持たない）。直後の `update_installed` は event 69479（23:55:02.125Z、e994a99f）
- `update_failed`: event 69587（00:05:10.462Z、e850ad3a）、69788（00:19:51.844Z、523ae7ff）、69933（00:35:36.974Z、23c4fefd）。3 つとも stage `e2e` で、同じ e2e が名前での流し直しでも落ちた（task 1516 の 27b7af0c が 00:51Z に直した）
- 自動更新の ask: `approve_update` の ask は C より前に 0 件。`update_failed` の ask は 350・351・352 で、どれも `skip` で答えられ閉じた（352 は ask_opened event 69932、ask_answered event 69942 の option `skip`、ask_closed event 69944）
- 参考（reference/status.json、fetch の時刻 01:28 ごろの今の状態で、比べる値に使わない）: `supervisors[].binary_version` は `0.4.0-dev+33cc04102ae9`、`auto_update` の `at` は 01:28:17.742Z（event 70612）。C の後に入れ替わったことを示すだけで、T には使わない

### 区間

境界は task 1422 と同じく [始まり, 終わり) で、run は最初の verdict の `review_finished` の時刻で区間に入れる。

| 名前 | 区間 | reviewed run | 決め方 |
|---|---|---|---|
| main（前の区間） | [S, E2)、S = 2026-10-02T03:07:27.872Z、E2 = 2026-10-03T00:29:27.268Z | **N = 76** | E2 は task 1420・1421 の最初の `run_integrated`（1420、event 69870）。task 1422 の E（00:12:09.306Z）が E2 より前なので、[E, E2) の分を同じ定義で足した |
| main_1422 | [S, E)、C = 01:25:00Z | 75 | task 1422 の主の区間を新しい C で出し直したもの（差の確認用） |
| main_before_0805 / main_after_0805 | [S, M) / [M, E2) | 11 / 65 | M は印 61916 |
| reference | [R, S) | 49 | task 1422 と同じ（R は task 1422 の値のまま） |
| audit | [S, A)、A より前の event | 35 | task 1422 と同じ（監査との照合用で、後の区間の本数には使わない） |
| after（後の区間） | `run_claimed` が T 以後で、最初の verdict が [T, C) の run を早い順に N 本 | **0（未取得）** | T が未確定のため |

- 後の区間から除いた run（T より前に claim され T の後に review された run）: T が無いので数えていない。
- 参考に、E2 から C までに最初の verdict を受けた run は 8（task 1516・1422・1421 ×2・1428 ×2・1424・1457）。どれも T の前（1420 を含まない binary の supervisor が claim し prompt を作った）で、後の区間には入らない。
- `review_failed` だけの run はどの区間も 0。
- 区間の中の印（normalized/ の `mark_recorded` / `mark_retracted`、[R, C)）: task 1422 と同じ 2 件（60152、61916）だけ。後の区間は無いので、後の区間の中で review の provider や worker の既定を変える印は無い。

### 観測の締切 C と snapshot

C = **2026-10-03T01:25:00Z**（この task の run の claim 01:22:06Z の後で、snapshot を取った時刻 01:26 ごろの直前）。値は `created_at` が C より前の event と git の commit だけから求めた。snapshot は `docs/plans/acceptance-check/snapshot/2026-10-03T01:25:00Z/` に取ったが commit していない（`.gitignore` 済み、`git add -f` もしない。本番 queue の dump を公開の repository に出さないため）。出力は `docs/plans/acceptance-check/out/2026-10-03T01:25:00Z/`。area は task 1422 の対応表 `areas-2026-10-03T00:15:00Z.toml` を fetch の `--areas-from` に渡し、変更行数の 3 分位は task 1422 の境（178・458）を `--line-terciles` で固定した（どちらも変えていない）。

### script の変更

task 1422 の列・計算式・層・正規化・裏づけの規則は変えず、次を足した。足した列と層は前の区間にも同じに出る（task 1422 の `out/2026-10-03T00:15:00Z/` は作り直していない。前の区間はこの章の C で全て出し直した）。

| ファイル | 足したもの |
|---|---|
| `fetch.py` | queue 全体の `supervisor_started`・`update_installed`・`update_failed`（`--until C`）と、ask の kind が `approve_update` か `update_failed` の `ask_opened`・`ask_answered`・`ask_closed` を、task 1422 と同じ正規化で `normalized/events.jsonl` に入れる。`normalized/versions.jsonl`（それらの version ごとの commit と、`--watch-task` の着地 commit を祖先に持つか。`+` の後の sha、無ければ tag `v<version>` を git で読む）。`tasks.jsonl` に受け入れ条件の番号の一覧 `item_numbers`。reference/ の `marks`・`status` は読み失敗でも止めずに失敗を書く（1 回目の fetch で `dagq status` が一度だけ exit 1 になったため） |
| `compute.py` | `--cutoff`・`--t-task`・`--t-landed-task` で T（`t.json`）、`--after after=main` で後の区間（N は main の本数）、`mapping.csv` と `mapping_items.csv`（番号ごとの根拠の判定）、`--compare main=after` で `compare.csv`（層ごとの前・後・差）と `weighted.csv`（構成比で重み付けした率）。層に `review_worker`（review の provider と worker の経路の組）を足し、`table.csv` に `mapped`・`mapped_judged`・`mapped_rate` の列を足した |
| `after.sh` | この章の引数で fetch → compute → crosscheck を流す |
| `crosscheck.py` | 変えていない |

### 層ごとの前後の表（前の区間 main と後の区間 after）

`out/2026-10-03T01:25:00Z/table.csv`（前）と `compare.csv`（前・後・差の全ての列）の値を写した（秒）。列は task 1422 の表と同じで、「対応の記載」（summary が受け入れ条件の番号ごとに根拠を挙げた run / 番号のある run。[判定の規則](#対応が書かれた-run-の割合)）を足した。後の区間は 0 本なので、「後」と「差（後−前）」は全ての列で『未取得』（compare.csv の `*_after` と `*_diff` も同じ）。

| 層 | n | 初回通過 | 差し戻し | verdict pass/revise/concern | acceptance_unmet の run / verdict / 主 | review/run | work 中央・p90 | work 待ち除く | review 中央・p90 | review 待ち除く | wait_to_land 中央・p90 | wait_to_land ask 除く | 未着地 / review 中 / 未取得 | kpi first_pass | 対応の記載 | 後 | 差（後−前） |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| all | 76 | 25（32.9%） | 52（68.4%） | 68/69/24 | 39 / 60 / 50 | 2.12 | 884・2228 | 884・2228 | 144・486 | 144・486 | 1179・8734 | 1157・6047 | 2 / 0 / 2 | 23/74 | 5/46 | 未取得 | 未取得 |
| change=config | 3 | 0（0.0%） | 3（100.0%） | 3/3/0 | 0 / 0 / 0 | 2.00 | 211・373 | 211・373 | 77・92 | 77・92 | 207・395 | 207・395 | 0 / 0 / 0 | 0/3 | 0/0 | 未取得 | 未取得 |
| change=docs | 10 | 1（10.0%） | 9（90.0%） | 9/13/3 | 6 / 10 / 8 | 2.50 | 352・854 | 352・854 | 171・340 | 171・340 | 652・1367 | 652・1367 | 0 / 0 / 0 | 1/10 | 0/7 | 未取得 | 未取得 |
| change=feature | 22 | 4（18.2%） | 18（81.8%） | 20/25/14 | 15 / 22 / 16 | 2.68 | 1372・2326 | 1372・2326 | 252・704 | 252・704 | 1838・10080 | 1838・7339 | 1 / 0 / 1 | 4/21 | 2/16 | 未取得 | 未取得 |
| change=fix | 10 | 6（60.0%） | 4（40.0%） | 9/3/2 | 4 / 5 / 4 | 1.40 | 730・1460 | 730・1460 | 103・247 | 103・247 | 2091・6642 | 1720・4447 | 0 / 0 / 0 | 6/10 | 0/5 | 未取得 | 未取得 |
| change=measure | 8 | 2（25.0%） | 7（87.5%） | 7/10/1 | 7 / 11 / 11 | 2.25 | 690・10932 | 690・10932 | 253・583 | 253・583 | 655・7207 | 655・5692 | 0 / 0 / 0 | 2/8 | 2/4 | 未取得 | 未取得 |
| change=refactor | 9 | 4（44.4%） | 5（55.6%） | 7/5/3 | 4 / 7 / 7 | 1.67 | 1220・2217 | 1220・2217 | 117・486 | 117・486 | 1133・25548 | 1133・10261 | 1 / 0 / 1 | 5/8 | 1/8 | 未取得 | 未取得 |
| change=test | 10 | 6（60.0%） | 4（40.0%） | 10/5/1 | 3 / 5 / 4 | 1.60 | 925・2228 | 925・2228 | 73・141 | 73・141 | 1024・8494 | 1024・5458 | 0 / 0 / 0 | 5/10 | 0/5 | 未取得 | 未取得 |
| change=unknown | 4 | 2（50.0%） | 2（50.0%） | 3/5/0 | 0 / 0 / 0 | 2.00 | 546・4333 | 546・4333 | 134・389 | 134・389 | 846・9380 | 846・1336 | 0 / 0 / 0 | 0/4 | 0/1 | 未取得 | 未取得 |
| area=ci | 1 | 1（100.0%） | 0（0.0%） | 2/0/0 | 0 / 0 / 0 | 2.00 | 500・500 | 500・500 | 147・147 | 147・147 | 4447・4447 | 4447・4447 | 0 / 0 / 0 | 0/1 | 0/0 | 未取得 | 未取得 |
| area=config | 4 | 0（0.0%） | 4（100.0%） | 4/4/0 | 0 / 0 / 0 | 2.00 | 241・373 | 241・373 | 84・133 | 84・133 | 211・395 | 211・395 | 0 / 0 / 0 | 0/4 | 0/1 | 未取得 | 未取得 |
| area=docs | 60 | 17（28.3%） | 44（73.3%） | 51/56/18 | 35 / 51 / 42 | 2.08 | 914・2067 | 914・2067 | 181・457 | 181・457 | 1283・8734 | 1283・6047 | 0 / 0 / 1 | 16/60 | 5/36 | 未取得 | 未取得 |
| area=migrations | 3 | 1（33.3%） | 2（66.7%） | 1/0/3 | 2 / 3 / 2 | 1.33 | 1718・2326 | 1718・2326 | 117・252 | 117・252 | 7280・13397 | 3372・6698 | 0 / 0 / 0 | 1/3 | 1/3 | 未取得 | 未取得 |
| area=plugin | 13 | 3（23.1%） | 10（76.9%） | 12/12/5 | 5 / 9 / 6 | 2.23 | 820・1850 | 820・1850 | 199・620 | 199・620 | 1069・13397 | 1069・7339 | 0 / 0 / 0 | 3/13 | 0/11 | 未取得 | 未取得 |
| area=runtime | 43 | 15（34.9%） | 28（65.1%） | 36/29/15 | 23 / 31 / 24 | 1.86 | 1108・1850 | 1108・1850 | 141・457 | 141・457 | 1659・8734 | 1577・6861 | 0 / 0 / 1 | 15/43 | 3/28 | 未取得 | 未取得 |
| area=tests | 49 | 21（42.9%） | 28（57.1%） | 45/31/13 | 22 / 30 / 24 | 1.82 | 1087・2228 | 1087・2228 | 124・457 | 124・457 | 1494・8734 | 1438・6698 | 0 / 0 / 1 | 17/49 | 1/32 | 未取得 | 未取得 |
| area=unknown | 2 | 0（0.0%） | 2（100.0%） | 3/6/5 | 2 / 5 / 4 | 7.00 | 2756・2756 | 2756・2756 | 1056・1816 | 1056・1816 | ・ | ・ | 2 / 0 / 1 | 0/0 | 0/1 | 未取得 | 未取得 |
| items=1-3 | 31 | 11（35.5%） | 21（67.7%） | 30/30/9 | 16 / 22 / 16 | 2.23 | 681・2756 | 681・2756 | 129・279 | 129・279 | 1137・7207 | 1089・5458 | 1 / 0 / 0 | 8/30 | 1/1 | 未取得 | 未取得 |
| items=4-6 | 36 | 12（33.3%） | 24（66.7%） | 31/34/8 | 17 / 30 / 28 | 2.03 | 984・2217 | 984・2217 | 160・620 | 160・620 | 1053・13105 | 1053・6050 | 1 / 0 / 2 | 13/35 | 4/36 | 未取得 | 未取得 |
| items=7+ | 9 | 2（22.2%） | 7（77.8%） | 7/5/7 | 6 / 8 / 6 | 2.11 | 1126・1718 | 1126・1718 | 204・792 | 204・792 | 4274・7339 | 1994・7339 | 0 / 0 / 0 | 2/9 | 0/9 | 未取得 | 未取得 |
| lines=179-458 | 24 | 10（41.7%） | 15（62.5%） | 23/21/7 | 14 / 22 / 18 | 2.12 | 690・1636 | 690・1636 | 149・389 | 149・389 | 1344・7339 | 1243・5692 | 0 / 0 / 0 | 9/24 | 2/13 | 未取得 | 未取得 |
| lines=<=178 | 26 | 12（46.2%） | 14（53.8%） | 26/14/2 | 6 / 6 / 4 | 1.62 | 521・1920 | 521・1920 | 92・199 | 92・199 | 645・5458 | 645・4431 | 0 / 0 / 0 | 10/26 | 2/13 | 未取得 | 未取得 |
| lines=>458 | 24 | 3（12.5%） | 21（87.5%） | 16/28/10 | 17 / 27 / 24 | 2.25 | 1444・2228 | 1444・2228 | 250・620 | 250・620 | 1988・13397 | 1864・7687 | 0 / 0 / 1 | 4/24 | 1/19 | 未取得 | 未取得 |
| lines=未着地 | 2 | 0（0.0%） | 2（100.0%） | 3/6/5 | 2 / 5 / 4 | 7.00 | 2756・2756 | 2756・2756 | 1056・1816 | 1056・1816 | ・ | ・ | 2 / 0 / 1 | 0/0 | 0/1 | 未取得 | 未取得 |
| review_provider=codex | 76 | 25（32.9%） | 52（68.4%） | 68/69/24 | 39 / 60 / 50 | 2.12 | 884・2228 | 884・2228 | 144・486 | 144・486 | 1179・8734 | 1157・6047 | 2 / 0 / 2 | 23/74 | 5/46 | 未取得 | 未取得 |
| worker=headless/claude | 62 | 21（33.9%） | 41（66.1%） | 59/55/20 | 31 / 47 / 38 | 2.16 | 884・1658 | 884・1658 | 147・441 | 147・441 | 1157・6913 | 1130・4431 | 2 / 0 / 2 | 21/60 | 4/40 | 未取得 | 未取得 |
| worker=interactive/claude | 14 | 4（28.6%） | 11（78.6%） | 9/14/4 | 8 / 13 / 12 | 1.93 | 1280・8402 | 1280・8402 | 100・620 | 100・620 | 6100・13397 | 4096・7687 | 0 / 0 / 0 | 2/14 | 1/6 | 未取得 | 未取得 |
| review_worker=codex\|headless/claude | 62 | 21（33.9%） | 41（66.1%） | 59/55/20 | 31 / 47 / 38 | 2.16 | 884・1658 | 884・1658 | 147・441 | 147・441 | 1157・6913 | 1130・4431 | 2 / 0 / 2 | 21/60 | 4/40 | 未取得 | 未取得 |
| review_worker=codex\|interactive/claude | 14 | 4（28.6%） | 11（78.6%） | 9/14/4 | 8 / 13 / 12 | 1.93 | 1280・8402 | 1280・8402 | 100・620 | 100・620 | 6100・13397 | 4096・7687 | 0 / 0 / 0 | 2/14 | 1/6 | 未取得 | 未取得 |

#### 前の区間の値の task 1422 との差

C が 00:15:00Z から 01:25:00Z に動き、区間の終わりが E から E2 に延びたため、次が変わった（`main_1422` と task 1422 の `out/2026-10-03T00:15:00Z/table.csv` の main を比べた）。

- 同じ [S, E) でも、task 1405 の run 42637110 が、旧 C（00:15:00Z）の後から新しい C（01:25:00Z）までに review を 4 回追加で受けた（`review_finished` event 69789・69907・70018・70320。verdict は pass・pass・revise・pass で、verdict の列は 8 件から 12 件になった）。その後 `landing_decided`（event 70369）で `needs_session` になり、01:09:08Z の `recovery_requested`（event 70406）からは記録された status が `failed` で、C の時点で復旧の job にかかっていた（compute の状態は `ended`）。all の verdict pass/revise/concern が 64/68/24 → 67/69/24、acceptance_unmet の verdict 59 → 60（主 49 → 50）、review/run 2.08 → 2.13、review の中央値・p90 が 144・457 → 147・486、review 中 1 → 0。初回通過・差し戻し・acceptance_unmet の run の数は同じ。この run を含む層（change=feature、items=1-3、lines=未着地、area=unknown、worker=headless/claude）も同じ分だけ動いた。
- [E, E2) に task 1420 の run d0e77030 が 1 本加わった（最初の verdict は 00:16:51.942Z の pass、change feature、area docs・runtime、項目数 6、151 行）。n 75 → 76、初回通過 24 → 25（32.0% → 32.9%）、差し戻し率 69.3% → 68.4%、work の中央値 854 → 884 秒。

#### 構成比で重み付けした率

`weighted.csv`。change・項目数・変更行数の層それぞれで、Σ（前の区間の層の構成比 × 後の区間の層の率）（両方の区間にある値だけで、『未着地』『未取得』の区分は大きさの区分ではないので使わない。構成比はその値の中で足して 1 に直す）を初回通過率・差し戻し率・acceptance_unmet の run の率について出す。比べる前の値は同じ値の集合で求めた Σ（構成比 × 前の率）。

| 層 | 率 | 前（main 全体） | 後（重み付け） | 差 | 使った値 / 除いた値 |
|---|---|---|---|---|---|
| change | 初回通過率 | 0.329 | 未取得 | 未取得 | 後の区間が 0 本なので両方にある値が無く、前の全ての値（config 3・docs 10・feature 22・fix 10・measure 8・refactor 9・test 10・unknown 4）が除かれる |
| change | 差し戻し率 | 0.684 | 未取得 | 未取得 | 同上 |
| change | acceptance_unmet の run の率 | 0.513 | 未取得 | 未取得 | 同上 |
| 項目数 | 3 つの率 | 0.329 / 0.684 / 0.513 | 未取得 | 未取得 | 前の 1-3 31・4-6 36・7+ 9 が除かれる |
| 変更行数 | 3 つの率 | 0.329 / 0.684 / 0.513 | 未取得 | 未取得 | 前の <=178 26・179-458 24・>458 24・未着地 2 が除かれる |

#### 同じ review の provider と worker の経路どうしの行

層 `review_worker` の行（上の表の `review_worker=` の行と compare.csv）。前の区間は codex|headless/claude 62 run（初回通過 33.9%、差し戻し 66.1%）と codex|interactive/claude 14 run（28.6%、78.6%）。後の区間は未取得なので、同じ組どうしの差も未取得。

#### 対応が書かれた run の割合

判定の規則（`compute.py` の `mapping`・`item_segments`・`evidence_of`。run ごとの結果は `out/2026-10-03T01:25:00Z/mapping.csv`、番号ごとの裏づけは `mapping_items.csv`）:

1. 読む summary: run の最初の verdict の前の最後の `validation_finished` の `payload.receipt.summary`（最初の review が読んだ receipt）。番号は task の最初の claim の時点の受け入れ条件の `(数字)`（`tasks.jsonl` の `item_numbers`）。
2. 番号ごとの区切り: summary の中の `(k)` か `（k）` の最初の出現から、次の番号の印（どの番号でも）か summary の終わりまでを、その番号の記載とする。
3. 番号ごとの根拠: その記載が、空白を除いて 20 字以上あり、次の根拠のどれか 1 つ以上を含めば、その番号に根拠がある（`evidenced` = 1）。path かファイル名（`a/b`、`.md`・`.rs`・`.sql`・`.py`・`.sh`・`.toml`・`.json`・`.jsonl`・`.csv`・`.yml` で終わる名前）、code の名前（`a::b`、backtick、`snake_case`）、commit の sha（16 進 7 字以上）、コマンド（cargo・dagq・git・python3・grep・sh）、test・event・commit・section・ADR・テスト・節・印の語、単位つきの測った値（`%`・秒・分・時間・本・件・回・行・run・s・lines）。どれが当たったかを `mapping_items.csv` の `evidence` に、記載の字数を `chars` に書く。
4. run の判定: 受け入れ条件の全ての番号に根拠があれば 1、1 つでも欠けるか番号が summary に無ければ 0（`mapping.csv` の `numbers_with_evidence` と `numbers_without_evidence` に番号を並べる）。受け入れ条件に番号が無い task は『番号なし』、受け入れ条件か summary が読めなければ『未取得』で、どちらも分母から除く。

根拠が正しいか（その path や test が本当に条件を満たすか）は判定しない。根拠の欄があるかと、その中に検査できる手がかりがあるかだけを見る。

| 区間 | 判定した run | 番号ごとに根拠を挙げた run | 割合 | 番号なし |
|---|---|---|---|---|
| main（前） | 46 | 5 | 10.9% | 30 |
| main_before_0805 | 5 | 1 | 20.0% | 6 |
| main_after_0805 | 41 | 4 | 9.8% | 24 |
| reference | 23 | 1 | 4.3% | 26 |
| after（後） | 未取得 | 未取得 | 未取得 | 未取得 |

前の区間で 1 だった run: task 961・1333・1368・1455・1420（mapping.csv の `mapped` が 1 の行）。番号を全て挙げたが根拠の無い番号があって 0 になった run は task 1161（`(4) See tests.` が 9 字）の 1 つ。番号の一部だけを挙げた run は 1200・1075・1369・1371・1372・1334・627・1391 などで、例えば 1200 は `(1) task 1179 landed and is met.` に根拠が無く `(2)` と `(4)` が無い。

### stats との照合（crosscheck）

`out/2026-10-03T01:25:00Z/crosscheck.csv`。runs.csv の 125 run（どれかの区間に入った run。後の区間は 0 本）の work・wait_to_land・land_phases の ask・route・actual_provider・areas を reference/ の `dagq stats --full --since 2026-09-30T00:00:00Z --until C` と比べ、**食い違いは 0**（前の区間 0、後の区間は run が無く 0）。task 1422 の 1 件（task 1405 の run が stats の runs に無い）は、その run が C までに終わった（`ended`）ので消えた。crosscheck.csv は表の値に使っていない。

### 再現性

- 同じ snapshot で compute を 2 回流し、`runs.csv`・`table.csv`・`meta.json`・`t.json`・`mapping.csv`・`mapping_items.csv`・`compare.csv`・`weighted.csv` が `cmp` で同じだった（review の差し戻しで判定の規則を変えた後にも流し直して確かめた）。
- crosscheck を 2 回流し、`crosscheck.csv` が同じだった。
- 同じ C で fetch し直し（`OUT=/tmp/refetch1423 sh after.sh fetch`）、`diff -r` で normalized/ の 5 ファイル（`events.jsonl`・`tasks.jsonl`・`commits.jsonl`・`areas.toml`・`versions.jsonl`）が同じだった（133 run、130 task、12,599 event、126 commit）。reference/ は今の状態を含むので比べていない。
- `baseline.sh`（task 1422）を今の compute で流し直すと、足した列と層のため table.csv が task 1422 の out/ と同じにはならない（task 1422 の snapshot は手元に無く、流し直していない）。

### 再実行のコマンド

`docs/plans/acceptance-check/` で、固定バイナリ（`~/.local/bin/dagq`）を PATH に置いて、fetch → compute → crosscheck の順に流す。`sh after.sh` は 3 つを順に、`sh after.sh fetch` / `compute` / `crosscheck` は 1 つだけ流す（測り直すときは `C=<新しい C> sh after.sh`）。中身は次と同じ。

```sh
cd docs/plans/acceptance-check
# 1. fetch（queue を読むだけ。始まり R・終わり C・締切 C）
python3 fetch.py --since 2026-10-01T06:02:46.438Z --until 2026-10-03T01:25:00Z \
  --cutoff 2026-10-03T01:25:00Z --stats-since 2026-09-30T00:00:00Z \
  --watch-task 1420 --watch-task 1421 --areas-from areas-2026-10-03T00:15:00Z.toml \
  --repo ../../.. --out snapshot/2026-10-03T01:25:00Z
# 2. compute（normalized/ だけを読む）
python3 compute.py snapshot/2026-10-03T01:25:00Z \
  --interval main=2026-10-02T03:07:27.872Z,2026-10-03T00:29:27.268Z \
  --interval main_1422=2026-10-02T03:07:27.872Z,2026-10-03T00:12:09.306Z \
  --interval main_before_0805=2026-10-02T03:07:27.872Z,2026-10-02T08:05:36Z \
  --interval main_after_0805=2026-10-02T08:05:36Z,2026-10-03T00:29:27.268Z \
  --interval reference=2026-10-01T06:02:46.438Z,2026-10-02T03:07:27.872Z \
  --interval audit=2026-10-02T03:07:27.872Z,2026-10-02T14:44:46.885Z,2026-10-02T14:44:46.885Z \
  --line-terciles 178,458 --changed-task 1420 --changed-task 1421 \
  --cutoff 2026-10-03T01:25:00Z --t-task 1420 --t-landed-task 1421 \
  --after after=main --compare main=after
# 3. crosscheck（runs.csv と reference/ の stats だけを読む）
python3 crosscheck.py snapshot/2026-10-03T01:25:00Z
```

### 同じ期間に入った他の変更

本数が少なく後の区間も無いので、差を因果と言い切らない。前の区間の中にも、run の review・worker の prompt・AGENTS.md を変えた着地が入っている。測り直すときは、後の区間にも同じ種類の変更が重なる。

- 区間の中の印: [R, C) は 60152・61916 の 2 件だけ（上の区間の節）。
- 着地の洗い出し方: normalized/ の `run_integrated` のうち `created_at` が [S, C) のもの（S は印 60152 の時刻で、task 1208 の着地と同じ時刻）について、着地 commit の変更ファイル（`git show --name-only`）を `prompt|review|AGENTS\.md|^dagq\.toml|plugins/.*/(dagq|dagq-planner)/` で選び、内容で分けた。`git log --first-parent main --since=S --until=C` の commit は全て normalized/ にその `run_integrated` があった。着地は main に入った時刻で、本番の supervisor がその commit を含む binary で動き始めるのは自動更新の `supervisor_started` の後になる（上の T の節と同じ）。前の区間の run は、claim の時点の binary によって次の変更のどれを受けたかが違う。

| 種類 | 着地（`run_integrated` の event・時刻） | commit | task | 内容 |
|---|---|---|---|---|
| run の review | 60129・2026-10-02T03:07:27.872Z | 2cb08f4b | 1208 | `dagq.toml` の `[roles.review]` を codex に（S。印 60152） |
| run の review | 63391・2026-10-02T13:29:42.276Z | 79d17986 | 1316 | review の prompt（`src/application/prompt.rs`）に concern の推奨・確信度・`reason_category` を足し、runtime が `high` の land / send_back を適用する（ADR-t451-1 決定 3）。`review.md` |
| run の review | 66645・2026-10-02T17:53:53.080Z | 8a6d9ed1 | 1453 | review の subagent の ADR（文書だけ） |
| run の review | 66880・2026-10-02T18:41:16.793Z | 91f1bc3f | 1454 | `dagq.toml` の review の subagent の設定を読み、必須の agent を選んで review job に渡す |
| run の review | 67277・2026-10-02T19:43:54.379Z | acc7d12e | 1455 | 親の review job が必須の subagent を実行して 1 つの verdict にまとめ、必須の結果が欠けた verdict を supervisor が pass にしない。C の時点の main（523ae7ff・33cc0410）の `dagq.toml` に `[review.subagents.*]` は無く、選ばれる agent は無い |
| run の review | 67733・2026-10-02T20:56:19.715Z | bfe4edf7 | 1392 | 適用した concern の send_back で開く `approve_landing` の ask に concern と推奨を書き、stats の数え方を直す（verdict の判定は変えない） |
| worker の prompt | 61149・2026-10-02T08:00:54.475Z | c05e8644 | 1340 | Claude の worker の既定を非対話に（印 61916）。`prompt.rs` と `AGENTS.md`（1 行） |
| worker の prompt | 64015・2026-10-02T14:15:24.217Z | 112aecdf | 1372 | 答えずに閉じた `worker_question` を worker に伝える文面を足す |
| worker の prompt | 64165・2026-10-02T14:40:20.833Z | e8f05079 | 1290 | Codex の run と job に一時ファイルの置き場所（TMPDIR）を渡す |
| worker の prompt | 69870・2026-10-03T00:29:27.268Z | 23c4fefd | 1420 | 受け入れ条件の対応づけ（この測定の対象。E2） |
| worker の prompt | 70553・2026-10-03T01:21:58.486Z | 33cc0410 | 1428 | receipt の前に、変えた挙動を説明する文書を差分と照合する指示を対応づけに足す（ADR-t1428-1）。C の後に入った binary（33cc0410）はこれも含むので、測り直しの後の区間は 1420 と 1428 の両方の効果を含む |
| AGENTS.md | 60830・2026-10-02T07:24:13.139Z | 051caaea | 1220 | スループットの見直しの job の記述（1 行） |
| AGENTS.md | 66416・2026-10-02T17:34:53.976Z | 52cec2bf | 724 | goal の close を goal review job へ（3 行） |
| AGENTS.md | 69314・2026-10-02T23:37:01.475Z | fd30c185 | 1458 | 「作業中」「起動と停止」「着地と人の判断」を受け持ちごとに正本へ移す（12 行追加・37 行削除） |
| AGENTS.md | 70392・2026-10-03T01:09:04.553Z | 3189b7e1 | 1421 | 受け入れ条件の対応づけの根拠の書き方と、測定の task の規則（この測定の対象） |
| AGENTS.md（C の後） | 2026-10-03T01:30:30Z（commit 時刻） | 2dc9638c | — | 「変更後に必ず通す」「テストの制約」を開発文書へ移す。後の区間の run の base に入りうる |

選んだが上の表から外した着地（run の review・worker の prompt・AGENTS.md の振る舞いを変えないか、別の job のもの）:

- plan review・goal review・スループットの見直し・runtime の planner の prompt や設定: fb8170a8（1219）、e11fff83（1221）、df10d058（1300）、e81fc18f（1317）、9d19e500（1338、`dagq.toml` のコメント）、eae988ff（1320、runtime の planner の prompt）、e994a99f（1334）、8081a8d1（1425）、49286517（1318）、42e70adc（1378）、c7b562d4（1371）、d006c3f2（1215）、433469e9（1418）
- test と refactor だけ（振る舞いを変えない）: 9d74de58（1414、`review.md` の test の記述を含む）、ead1223c（1413、`prompt.rs` の判断を unit test に移す）、7fa18894（1075）、740adf14（1274）、cc50e670（1329）、471bb76b（1023）、8327e76a（1284）
- ADR・文書・コメントだけ: 8ea16a73（1394）、dfdf4456（1404）、5870f5c0（1265、`dagq.toml` の `[e2e]` のコメント）
- plugin の skill の文書: 46640f00（627）、baf6427a（1466）、59461a1d（1323）、523ae7ff（1319）
- E から C の間の 27b7af0c（task 1516、e2e の test の修正）と 92967ea7（task 1422、docs だけ）は選ぶ規則に当たらない

task 1420・1428 自体は、run の review の prompt と判定の基準を変えていない。23c4fefd と 33cc0410 の `review.md` の変更は、revise の依頼で worker が対応づけをやり直す手順の説明。両方の receipt に、`review_prompt` が変わっていないことを unit test で確かめたと書いてある（ADR-t1420-1・ADR-t1428-1）。一方、期間全体では上の表のとおり、run の review の prompt と判定に触れる着地（79d17986・91f1bc3f・acc7d12e・bfe4edf7）が前の区間の中にある。前の区間の中でも review の条件は一様ではない。goal 90 の制約（比べる間は run の review の prompt と判定を変えない）は後の区間を測る間に当てはまる。測り直すときは、T から C までに同じ種類の着地が無いかを同じ洗い出し方で確かめる。

### 限界

- **後の区間が無い**: この章の比べ方は前の区間だけで、goal 90 の受け入れ条件 (4) は満たしていない。
- **N は C で動く**: 前の区間の本数 N はこの C で 76。測り直す C で前の区間を出し直すと、[S, E2) に最初の verdict を持つ run は増えないが、C の後に review が増えた run の verdict の数や review の時間は動く（1405 の例）。後の区間はその C で求めた N にそろえる。
- **対応の判定**: 番号ごとの記載の長さと根拠の手がかりを見る機械的な規則で、根拠の中身の正しさは見ない。手がかりの語（test・節など）だけで根拠ありになる記載も、言い換えで根拠なしになる記載もありうるので、`mapping_items.csv` の番号ごとの `chars` と `evidence` を残した。受け入れ条件に番号の無い task（前の区間で 30 / 76）は判定できない。task 1420 より前の receipt でも、番号ごとに根拠を書く worker が居た（5 run）。
- **後の区間の打ち切り**: 後の区間の run は C の直前に最初の verdict を受けうるので、C までに見える 2 回目以後の verdict と着地が前の区間より少ない。差し戻し率・acceptance_unmet の数・review/run・wait_to_land は後の区間で低く出る向きに偏り、未着地も多くなる（初回通過率は最初の verdict だけなので偏らない）。測り直すときは、後の区間の最後の run の最初の verdict から十分あと（前の区間の review と着地が済む程度）に C を置き、未着地と review 中の件数を両方の区間で並べる。
- **後の区間の `review_failed_only`**: table.csv の後の区間の `review_failed_only` は [T, 終わり) の `review_failed` だけの run を claim の時刻で絞らずに数える（前の区間と同じ数え方）。
- task 1422 の限界（area の重なり、review の provider の切り替わり、項目数の数え方、change の unknown）はそのまま当てはまる。
