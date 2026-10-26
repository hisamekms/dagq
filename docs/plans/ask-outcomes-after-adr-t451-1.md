---
id: plan-ask-outcomes-after-adr-t451-1
type: plan
title: ADR-t451-1の実装の着地の前後の、askのkindごとの件数・人の答え待ち・AIが決めた件数と、AIが決めたlandの後の手直し
status: completed
created: 2026-10-05
owners:
  - hisamekms
tags:
  - measurement
  - operations
related:
  - adr-t451-1
  - plan-ask-outcomes-2026-09-26
  - adr-0047
---

# ADR-t451-1の実装の着地の前後の、askのkindごとの件数・人の答え待ち・AIが決めた件数と、AIが決めたlandの後の手直し

[ADR-t451-1](../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)の実装（task 1316〜1320）の着地の前後を、[2026-09-26以降のaskの集計](ask-outcomes-2026-09-26.md)（以下「前の集計」）と同じ数え方で比べる（goal 34、task 1321）。

次の節から「読み方と不足」までは後の期間が約1.9日の暫定の結果。後の期間を7日にした再測定と、暫定の結論が確かめられたか変わったかは「後の期間を7日にした再測定（goal 142、task 1805）」の節が持つ。

## 期間と数え方

- 境目: task 1316〜1320 の最後の着地 = task 1319（run 8cf5ae79）の`run_integrated`（event 69686、**2026-10-03T00:11:48.912Z**）。ほかの着地は 1318 が 2026-10-02T11:50Z、1317 が 12:34Z、1316 が 13:29Z、1320 が 17:06Z。依存の task 1389 は 14:50Z、1392 は 20:56Z に着地した。そのため前の期間の終わりの約12時間（2026-10-02T11:50Z〜境目）は、実装の一部がすでに動いている。
- 前の期間: ask 69 の作成（2026-09-25T15:35:47Z、前の集計の始まり）から境目まで（7.36日、ask 69〜350 の 282 件）。前の集計（ask 69〜287）を含み、その後の ask 288〜350 も足した。
- 後の期間: 境目から 2026-10-04T21:12:23Z まで（1.88日、ask 351〜437 の 87 件）。
- 読んだもの: 固定バイナリ`~/.local/bin/dagq`（`0.4.0-dev+06cf5074`。task 1389 の 0f1a6026 と task 1392 の bfe4edf7 を含む。`status`の`supervisors[].binary_version`も同じ）の読み取り専用のコマンドだけ。`dagq asks --all`、`dagq events --all --full --kind concern_decided --kind plan_concern_decided --kind observe_finished --kind follow_up_adopted --kind concern_send_back_escalated --kind ask_opened --kind run_integrated --kind review_finished --since 2026-09-25T15:35:00Z`、`dagq events --all --full --since 2026-10-03T04:00:00Z --kind task_created --kind follow_up_registered --kind task_rework --kind task_reopened --kind finding_recorded --kind follow_up_adopted`、`dagq events --all --full --run RUN --kind review_finished`、`dagq stats --full --since 2026-10-03T00:11:48.912Z --until 2026-10-04T21:12:23Z`。数えるのは`jq`。queue の状態は変えていない。
- 答えまでの時間は前の集計と同じく`answered_at - created_at`（分）。中央値と合計は答えのある ask だけで、runtime が閉じた答え（`answered_by: runtime`）も含む（前の集計と同じ）。測った時点で答えの無い ask は「未回答」に数え、時間には入れない。

## kindごとの件数と答えまでの時間

| kind | 前の件数 | 前の中央値（分） | 前の合計（時間） | 後の件数 | 後の中央値（分） | 後の合計（時間） | 備考 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `planner_question` | 70 | 3.2 | 77.2 | 21（未回答 3） | 42.4 | 37.7 | |
| `approve_landing`（`provider_disabled`を除く） | 53 | 53.8 | 77.1 | 23（未回答 5） | 16.8 | 35.4 | 出どころの内訳は下 |
| `approve_landing`（`provider_disabled`） | 24 | 2,317 | 745.8 | 0 | — | — | `--no-claude`の運転の期間だけ。ADRの対象外 |
| `approve_plan` | 8 | 53.1 | 13.7 | 6（未回答 1） | 3.5 | 1.0 | 前の集計の範囲（〜ask 287）では 0 件。前の 8 件は ask 304〜347 |
| `blocked` | 19 | 159.8 | 46.3 | 1 | 48.7 | 0.8 | |
| `update_failed` | 22 | 21.5 | 35.9 | 10 | 19.7 | 6.7 | 対象外 |
| `queue_hold` | 31 | 2.5 | 2.5 | 6 | 0.6 | 0.4 | 対象外。ほぼ runtime が閉じた |
| `decide` | 17 | 1.5 | 12.8 | 6（未回答 2） | 22.4 | 1.7 | 対象外 |
| `worker_question` | 13 | 118.9 | 69.4 | 5（未回答 1） | 22.2 | 2.9 | 対象外 |
| `approve_update` | 12 | 20.1 | 8.3 | 1 | 11.9 | 0.2 | 対象外 |
| `approve_goal` | 3 | 200.2 | 8.2 | 8（未回答 4） | 9.0 | 0.5 | 対象外 |
| `stuck_exit` / `stalled` / `answer_prompt` | 5 / 4 / 1 | 177.3 / 2.3 / 0.2 | 18.3 / 0.2 / 0 | 0 / 0 / 0 | — | — | 対象外 |
| 計 | 282 | | | 87 | | | |

1日あたりにすると、ADRの対象の kind（`planner_question`・`provider_disabled`を除く`approve_landing`・`approve_plan`・`blocked`）は前が 150 件 / 7.36日 = 20.4 件/日、後が 51 件 / 1.88日 = 27.2 件/日で、減っていない。全 kind では前が 38.3 件/日、後が 46.4 件/日。

### `approve_landing`の出どころと答え

`send_back: <理由>`の形の答えは`send_back`に数えた。

| 出どころ | 前: land / send_back / ほか | 後: land / send_back / 未回答 |
| --- | --- | --- |
| concern | 25 / 15 / 0（計 40） | 2 / 4 / 3（計 9） |
| revise の上限超え | 5 / 6 / 0（計 11） | 2 / 8 / 2（計 12） |
| pass の後の merge-tree の衝突 | 2 / 0 / 0（計 2） | 0 / 1 / 0（計 1） |
| headless の review が verdict を返さなかった | —（0） | 0 / 1 / 0（計 1） |
| 適用した concern の send_back を session が直さなかった（task 1392 の形） | —（0） | —（0） |
| `provider_disabled` | 20 / 3 / runtime が閉じた 1（計 24） | —（0） |

後の concern の 9 件は、どれも runtime が自分で決めなかったもの（推奨が`land`でも high でない、`scope`に当たる、review と subagent の推奨が分かれた）だった。

## AIが決めてaskにしなかった件数

定義は task 1321 の受け入れ条件 (1) のとおり。

| 項目 | 前 | 後 | 読んだ記録 |
| --- | --- | --- | --- |
| review の concern の適用した`land`（`concern_decided`の`applied: true`） | 0 | 16（14 run） | `concern_decided` |
| review の concern の適用した`send_back`で ask に至らなかったもの | 5 | 7 | `concern_decided`と、同じ run の次の`review_finished` / `approve_landing`の`ask_opened` |
| （除いたもの: 適用した`send_back`の後に session が直さずに`approve_landing`の ask に至ったもの） | 0 | 0 | 下の「除外の照合」 |
| plan review の`plan_concern_decided`の`applied: true` | 7 | 13 | `plan_concern_decided`（どれも推奨`send_back`） |
| observer の finding だけ（`observe_finished`の`findings_without_ask`の和） | 0 | 113 | `observe_finished`（前の 127 件はこの欄を持たない） |
| planner が`planner_question`を経ずに決めたもの（`follow_up_adopted`の`by: planner`・`ask_id: null`） | 366 | 49 | `follow_up_adopted` |
| 計 | 378 | 198 | |

- 前の review・plan review の件数（5・7）は、境目の前の約12時間に task 1316・1317 が先に着地して動いていた分。
- planner の 366 件は、ADR-t451-1 の前から planner が上限に当たらない follow_up を自分で採っていた分で、ADR の決定 5 で始まったものではない。1日あたりでは前 49.7 件、後 26.1 件。
- 後の期間の`stats`の`recommendations.decided_without_ask`（上の`stats`のコマンド）は`approve_landing` 23・`approve_plan` 13・`blocked` 113・`planner_question` 49 で、上の jq の数（16＋7、13、113、49）と一致した。

### 除外の照合

- 適用した`send_back`の`concern_decided`は前 5 件（task 1418・1405・1320・1453・1458）、後 7 件（task 1423・1459・1433・251・655・839・1632）。どれも、同じ run の次の記録は次の review の attempt の`review_finished`（attempt が 1 以上進む）で、その前に`approve_landing`の`ask_opened`は無かった。除いたものは前後とも 0 件。
- task 1392 の着地（2026-10-02T20:56Z）の後の記録も同じ: `concern_send_back_escalated`の event は全期間で 0 件、後の期間の`approve_landing`の ask に task 1392 の形（`which the runtime applied`）の question は 0 件、`stats`の`decided_without_ask.approve_landing`の 23 は除外 0 件の jq の数と同じ。jq・記録・`stats`の食い違いは 0 件。

## 推奨との一致率

前の集計の表の率と、後の期間の`stats`の率を直接比べない。前後を並べるのは、旧記録から同じ定義で作り直せる kind だけ。

### 同じ定義で作り直せる kind

| kind と定義 | 前: 一致 / 母数 | 前の率 | 後: 一致 / 母数 | 後の率 | 母数から外したもの |
| --- | --- | --- | --- | --- | --- |
| `planner_question`（作り手の推奨の option と answer） | 66 / 69 | 96% | 18 / 18 | 100% | 前: 推奨の取れない 1 件（ask 332。question に推奨が無い）。後: 未回答 3 件（ask 423・432・433） |
| `blocked`（見立て = options の第 1 の選択肢と answer） | 14 / 19 | 74% | 1 / 1 | 100% | なし |
| `blocked`（answer が leave it / wait。後は参考） | 11 / 19 | 58% | 0 / 1 | 0%（参考） | なし。後の期間は leave it / wait を ask にしないので比べない（下） |

- `planner_question`の推奨は、前の期間は question の「推奨: …」（「提案: 採用」も`adopt`に読んだ。ask 147）、後の期間は ask の`recommendation`の欄（無ければ question）から取った。前の外れは ask 96・195（前の集計と同じ）と ask 331（推奨の option 1 に対し、人は自由文で「このプランは一旦廃止」と答えた）。前の期間の ask 331・332・334 は`recommendation`の欄も持つ（task 1318 の着地の後）が、定義どおり question から取った。後の期間の 18 件はどれも欄と question の両方に推奨があり、両者の食い違いは 0 件。
- `blocked`の後の 1 件（ask 401、`scope`）は task 1520 の着地の後の自動更新の e2e の失敗で、人は第 1 の選択肢（直す task を今すぐ計画する）を選んだ。leave it / wait の見立ては task 1319 の後は ask にならず finding に残る（上の 113 件）ので、後の期間の leave it / wait の率は前と同じ意味を持たない。参考: 後の 1 件は`recommendation`の欄と answer も一致した。

### 比較できない kind

| kind | 前の件数（land / send_back など） | 後の件数 | 比較できない理由 |
| --- | --- | --- | --- |
| `approve_landing` | 77（上の出どころの表） | 23 | 実装の前の review の verdict は推奨を持たず、旧記録から推奨を作り直せない。前の期間の ask 330〜349 の 7 件は欄を持つが、境目の前の途中の版の記録 |
| `approve_plan` | 8（`ready` 4・`send_back` 4） | 6（`send_back` 5・未回答 1） | 実装の前の plan review の concern は推奨を持たない。前の 8 件のうち欄を持つのは task 1317 の着地の後の 4 件だけ |
| ほか（`update_failed`・`decide`・`worker_question`・`approve_update`・`stuck_exit`・`stalled`・`answer_prompt`・`approve_goal`・`queue_hold`） | 上の件数の表 | 上の件数の表 | ADR の対象外（前の集計と同じ） |

### 後の期間の`stats`の率（参考。後の期間だけ）

task 1389 と 1392 を含む固定バイナリ（06cf5074）で読んだ`stats --full --since 2026-10-03T00:11:48.912Z --until 2026-10-04T21:12:23Z`の`recommendations.by_kind`。

| kind | answered | matched | rate |
| --- | --- | --- | --- |
| `planner_question` | 18 | 18 | 1.0 |
| `approve_landing` | 2 | 2 | 1.0 |
| `approve_plan` | 5 | 5 | 1.0 |
| `blocked` | 1 | 1 | 1.0 |

`approve_landing`の 2 件は欄を持つ答えのある ask だけ（ask 354・379）。後の concern の ask の多くは review と subagent の推奨が分かれて欄を持たない。

## AIがlandを選んだrunの着地の後

後の期間に`concern_decided`の`applied: true`・推奨`land`は 16 件、14 run（task 251 と 1713 は 2 回）で、14 run とも着地した。前の期間は 0 件。

| task | run | 着地 | 着地の後 |
| --- | --- | --- | --- |
| 793 | a22cd69e | 2026-10-03T04:39Z | なし |
| 1435 | ae4bbfa9 | 2026-10-03T14:43Z | なし |
| 1461 | 3a8484c5 | 2026-10-03T17:19Z | なし |
| 1402 | 2dcbe739 | 2026-10-03T18:17Z | なし |
| 251 | 64727486 | 2026-10-03T20:27Z | なし |
| 1462 | fcbdd009 | 2026-10-04T00:06Z | なし |
| 1520 | e9536c58 | 2026-10-04T03:05Z | **直す task が出た**（下） |
| 1505 | 0de6cd72 | 2026-10-04T03:49Z | なし |
| 1583 | 6357a771 | 2026-10-04T04:25Z | なし |
| 1506 | 280d9606 | 2026-10-04T05:27Z | なし |
| 1591 | 3c5a55c1 | 2026-10-04T08:35Z | なし |
| 1570 | a1b7032b | 2026-10-04T10:30Z | なし |
| 1712 | 44ad0456 | 2026-10-04T15:06Z | なし |
| 1713 | 853c5ae1 | 2026-10-04T16:03Z | なし |

調べたこと:

- revert: main の 2026-10-02 以降の commit に revert は無く、14 run の着地の commit はどれも今の main に残っている（`git merge-base --is-ancestor`）。
- 差し戻し: 14 task のどれにも`task_reopened`は無い。
- 直す task: 後の commit の message で「task <ID>」と 14 task を名指すものは task 1658（commit bfeada1d）だけ。後の期間の`follow_up_registered`の title で 14 task を名指すのは、その run 自身の receipt のもの（1520・1583）だけ。observer の flaky_test の finding（39 本）の test は、どれも 14 run が足した test の関数ではない。
- task 1520: 着地の後、自動更新の前の全 e2e の関門が`killed_supervisor_run_is_adopted_by_the_next_supervisor_and_lands`と`two_independent_tasks_run_concurrently_and_a_dependent_follows_integration`で落ち続け（observer の ask 401）、task 1658（2026-10-04T04:24Z に登録）が tests/e2e.rs の期待値を 1520 の`status`・`doctor`の runs の一覧の範囲に合わせた。1520 の diff は`dagq.toml`の`[e2e]`の paths に触れず、着地の前の e2e は流れていない。AI が land を選んだ concern（attempt 2）は plugin の参照などの小さな残りを挙げただけで e2e には触れておらず、同じ concern を人が見ても同じ材料しかない。

**誤った自動の land（着地の後に直す task が出たもの）は 14 run のうち 1 件（task 1520）。** 原因は concern の判断ではなく、着地の前に e2e を流す範囲（`[e2e]`の paths）の外の変更が e2e の期待値を変えたこと。

着地の時に、その run の receipt の follow_up として残りを登録した run はある（task 251 の throughput_review の移動、1713 の provider-lifecycle.md の残りの文、1583 の host での A/B と stress、1520・1570・1461 の docs_drift など）。これは review の concern が挙げて着地の前に分かっていた残りで、着地の後に見つかった手直しではないので上の件数に入れていない。

## 読み方と不足

- 後の期間は 1.88日で、目安の 3日に足りない。AI の判断は 198 件（review の land 16・send_back 7、plan review 13、observer 113、planner 49）で 10 件以上あるが、着地の後の手直しは着地から日が経って見つかるもの（e2e の関門・flaky・後の task の review）があり、後の期間の終わりに近い着地（1712・1713 など）はまだ見られていない。
- ADR の対象の ask は 1日あたり減っていない（20.4 → 27.2 件/日）。後の期間は revise の上限超えの`approve_landing`（12 件）と`planner_question`（21 件）が多く、`planner_question`の答えまでの中央値も 3.2 分から 42.4 分に延びた。期間が短く、件数の多い日の偏りを除けないので、増減の理由はここでは決めない。
- 再測定: 境目を同じ（2026-10-03T00:11:48.912Z）にして、後の期間を 3日以上（目安 7日）にしたうえで、この文書と同じ表と、AI が land を選んだ run の着地の後を数え直す。task 1321 の receipt の follow_up（measurement）に出した。

## 後の期間を7日にした再測定（goal 142、task 1805）

上の節は後の期間が1.88日の暫定の結果。同じ境目と同じ前の期間で、後の期間を境目から**7日（2026-10-10T00:11:48.912Z まで、168時間）**にして、上の表を同じ数え方で測り直した。目安の72時間（2026-10-06T00:11:48.912Z）の時点の値も並べる。

### 期間と数え方（上の節との違い）

- 境目: 上と同じ event 69686（task 1319 の`run_integrated`、2026-10-03T00:11:48.912Z）。前の期間も上と同じ（ask 69〜350）で、前の値は測り直して上の表と一致した。
- 後の期間: 境目から 2026-10-10T00:11:48.912Z まで（7.00日、ask 351〜713 の 363 件）。72時間の時点は 2026-10-06T00:11:48.912Z まで（3.00日、ask 351〜507 の 157 件）。測った時点（2026-10-26T13:42Z ごろ）の記録は 2026-10-26T09:38Z の ask 1260 まであり、境目から 23 日を超えている。
- 読んだもの: 固定バイナリ`~/.local/bin/dagq`（`0.4.0-dev+fb928779`）の読み取り専用のコマンドだけ。`dagq asks --all`、`dagq events --all --full --kind concern_decided --kind plan_concern_decided --kind observe_finished --kind follow_up_adopted --kind concern_send_back_escalated --kind ask_opened --kind run_integrated --kind review_finished --since 2026-09-25T15:35:00Z --until 2026-10-10T00:11:48.912Z --limit 100000`、`dagq events --all --full --since 2026-10-03T00:00:00Z --kind task_created --kind follow_up_registered --kind task_rework --kind task_reopened --kind finding_recorded --kind follow_up_adopted --kind landing_recheck_failed --limit 200000`（着地の後の照合は測った時点まで）、`dagq findings --all`、`dagq stats --full --since 2026-10-03T00:11:48.912Z --until 2026-10-06T00:11:48.912Z`と`--until 2026-10-10T00:11:48.912Z`。数えるのは`jq`。queue の状態は変えていない。
- 時刻の区切り: ask は`created_at`（秒）が境目の秒より後（`>= 1790986309`）で、終わりの秒より前（72時間は`< 1791245509`、7日は`< 1791591109`）。event は`created_at`の文字列で境目より後・終わりより前。前の期間は ask が`>= 1790350547`・`< 1790986309`。
- 列と計算式は上と同じ: 答えまでの時間は`answered_at - created_at`（分）、中央値と合計は答えのある ask だけ（runtime が閉じた答えも含む）、未回答は測った時点で答えの無いもの。1日あたりは件数を期間の日数（前 7.36、後 3.00 と 7.00）で割る。
- 測った時点では後の期間の ask はすべて答えがある（未回答 0）。そのため上の節の後の期間（〜2026-10-04T21:12:23Z）を今読み直すと、当時の未回答の分が答えまでの時間に入って値が変わる（`approve_landing`の中央値 16.8→128.2 分、`planner_question` 42.4→65.4 分）。答えの時刻を 2026-10-04T21:12:23Z で切ると上の表の値がそのまま出ることを確かめた（数え方は変えていない）。
- `approve_landing`の出どころは question の文で分けた。上の表の「revise の上限超え」には revise の後に session が黙ったもの（前の期間の ask 291）が入っていたので、後の期間も同じく入れた。後の期間に出た新しい形（pass の後に session が黙ったもの）は別の行にした。

### kindごとの件数と答えまでの時間（7日）

| kind | 前の件数 | 前の中央値（分） | 前の合計（時間） | 72時間の件数 | 72時間の中央値（分） | 72時間の合計（時間） | 7日の件数 | 7日の中央値（分） | 7日の合計（時間） |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `planner_question` | 70 | 3.2 | 77.2 | 26 | 71.8 | 67.5 | 44 | 42.4 | 91.5 |
| `approve_landing`（`provider_disabled`を除く） | 53 | 53.8 | 77.1 | 67 | 200.8 | 210.7 | 124 | 194.6 | 435.1 |
| `approve_landing`（`provider_disabled`） | 24 | 2,317 | 745.8 | 0 | — | — | 0 | — | — |
| `approve_plan` | 8 | 53.1 | 13.7 | 8 | 26.4 | 13.8 | 22 | 48.3 | 39.8 |
| `blocked` | 19 | 159.8 | 46.3 | 1 | 48.7 | 0.8 | 2 | 28.0 | 0.9 |
| `update_failed` | 22 | 21.5 | 35.9 | 12 | 19.7 | 11.9 | 16 | 18.5 | 15.3 |
| `queue_hold` | 31 | 2.5 | 2.5 | 16 | 0.1 | 5.1 | 104 | 0.3 | 67.2 |
| `decide` | 17 | 1.5 | 12.8 | 11 | 42.6 | 12.1 | 21 | 153.3 | 63.7 |
| `worker_question` | 13 | 118.9 | 69.4 | 5 | 33.8 | 5.4 | 12 | 146.0 | 29.4 |
| `approve_update` | 12 | 20.1 | 8.3 | 2 | 7.1 | 0.2 | 8 | 18.0 | 4.0 |
| `approve_goal` | 3 | 200.2 | 8.2 | 9 | 11.8 | 26.5 | 10 | 198.2 | 35.2 |
| `stuck_exit` / `stalled` / `answer_prompt` | 5 / 4 / 1 | 177.3 / 2.3 / 0.2 | 18.3 / 0.2 / 0 | 0 / 0 / 0 | — | — | 0 / 0 / 0 | — | — |
| 計 | 282 | | | 157 | | | 363 | | |

未回答は 72時間・7日とも 0 件（測った時点）。`queue_hold`の 104 件のうち 95 件は runtime が閉じた。

1日あたりの ADR の対象の kind（`planner_question`・`provider_disabled`を除く`approve_landing`・`approve_plan`・`blocked`）は、前 150 件 / 7.36日 = 20.4 件/日、72時間 102 件 / 3日 = 34.0 件/日、7日 192 件 / 7日 = 27.4 件/日。全 kind では前 38.3、72時間 52.3、7日 51.9 件/日。

境目からの日ごと（境目から 24 時間ずつ）の ADR の対象の件数:

| 日 | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| ADR の対象 | 27 | 26 | 49 | 16 | 43 | 15 | 16 |
| うち headless の review が verdict を返さなかった`approve_landing` | 0 | 1 | 36 | 2 | 11 | 0 | 0 |
| `planner_question` | 12 | 10 | 4 | 2 | 10 | 3 | 3 |
| `planner_question`の答えまでの中央値（分） | 228 | 33.4 | 256.6 | 260.2 | 2.2 | 220 | 22.6 |

### `approve_landing`の出どころと答え（7日）

| 出どころ | 前: land / send_back / ほか | 72時間: land / send_back / ほか | 7日: land / send_back / ほか |
| --- | --- | --- | --- |
| concern | 25 / 15 / 0（計 40） | 4 / 5 / cancel 1（計 10） | 13 / 19 / cancel 1・runtime が閉じた 1（計 34） |
| revise の上限超え（revise の後に session が黙ったものを含む） | 5 / 6 / 0（計 11） | 5 / 14 / 0（計 19） | 12 / 25 / cancel 1（計 38） |
| pass の後の merge-tree の衝突 | 2 / 0 / 0（計 2） | 0 / 1 / 0（計 1） | 0 / 1 / 0（計 1） |
| headless の review が verdict を返さなかった | —（0） | 0 / 37 / 0（計 37） | 0 / 50 / 0（計 50） |
| pass の後に session が黙った | —（0） | —（0） | runtime が閉じた 1（計 1） |
| 適用した concern の send_back を session が直さなかった（task 1392 の形） | —（0） | —（0） | —（0） |
| `provider_disabled` | 20 / 3 / runtime が閉じた 1（計 24） | —（0） | —（0） |

headless の review が verdict を返さなかった 50 件の内訳は、`subagents_unsupported` 36 件（ask 457〜552。35 件が 2026-10-05、1 件が 10-06）、600 秒の時間切れ 11 件、exit status で終わった 3 件。どれも人は`send_back`（そのまま出し直して review を受け直す）で答えた。review の材料が無い ask で、推奨の出る判断ではない。

### AIが決めてaskにしなかった件数（7日）

| 項目 | 前 | 上の節（〜10-04T21:12Z） | 72時間 | 7日 |
| --- | --- | --- | --- | --- |
| review の concern の適用した`land` | 0 | 16（14 run） | 23（20 run） | 58（52 run） |
| review の concern の適用した`send_back`で ask に至らなかったもの | 5 | 7 | 9 | 17 |
| （除いたもの: 適用した`send_back`の後に session が直さずに`approve_landing`の ask に至ったもの） | 0 | 0 | 0 | 0 |
| plan review の`plan_concern_decided`の`applied: true` | 7 | 13 | 16 | 64 |
| observer の finding だけ（`findings_without_ask`の和） | 0 | 113 | 152 | 925 |
| planner が`planner_question`を経ずに決めたもの | 366 | 49 | 71 | 181 |
| 計 | 378 | 198 | 271 | 1,245 |

- plan review の 64 件はどれも推奨`send_back`。observer の 925 件は`observe_finished` 144 回の和。
- planner の 1日あたりは前 49.7 件、7日 25.9 件。
- `stats`の`recommendations.decided_without_ask`は、72時間が`approve_landing` 32・`approve_plan` 16・`blocked` 152・`planner_question` 71、7日が 75・64・925・181 で、jq の数（23＋9、58＋17 ほか）と一致した。
- 除外の照合: 7日の適用した`send_back`は 17 件（task 1423・1459・1433・251・655・839・1632・1225・1680・2016・2008・2072・2068・1552・1847・2158・1869）。15 件は同じ run の次の記録が次の attempt の`review_finished`。task 1225（ask 456）と 2072（ask 599）は次の記録が`approve_landing`の`ask_opened`だが、その前に`revise_finished`があり（session は直した）、ask は直した後の review が verdict を返さなかったもの（上の表の headless の行）で、除外の形ではない。`concern_send_back_escalated`は 0 件、question に`which the runtime applied`を持つ ask も 0 件。除いたものは 0 件。

### 推奨との一致率（7日）

| kind と定義 | 前: 一致 / 母数 | 前の率 | 72時間: 一致 / 母数 | 72時間の率 | 7日: 一致 / 母数 | 7日の率 | 母数から外したもの |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `planner_question`（作り手の推奨の option と answer） | 66 / 69 | 96% | 26 / 26 | 100% | 43 / 44 | 98% | 前: ask 332（上と同じ）。後: なし |
| `blocked`（見立て = options の第 1 の選択肢と answer） | 14 / 19 | 74% | 1 / 1 | 100% | 2 / 2 | 100% | なし |
| `blocked`（answer が leave it / wait。後は参考） | 11 / 19 | 58% | 0 / 1 | 0%（参考） | 0 / 2 | 0%（参考） | なし |

- `planner_question`の 7日の外れは ask 536（推奨`read-time`に対し、人は自由文で「README.md とは別の索引ファイルに生成する」別案を答えた）。後の 44 件はどれも`recommendation`の欄と question の両方に推奨があり、食い違いは 0 件。
- `blocked`の後の 2 件は ask 401（上と同じ）と ask 594（goal 79 の draft 2009 の host での確認。`scope`）で、どちらも人は第 1 の選択肢を選んだ。
- `stats`の`recommendations.by_kind`（参考。後の期間だけ）: 72時間は`planner_question` 26 / 26、`approve_landing` 3 / 4、`approve_plan` 8 / 8、`blocked` 1 / 1。7日は`planner_question` 43 / 44（0.977）、`approve_landing` 11 / 16（0.688）、`approve_plan` 22 / 22、`blocked` 2 / 2。7日の`approve_landing`の外れは ask 506（推奨`land`、答え`cancel`）、568・582（推奨`send_back`、答え`land`）、598・621（推奨`land`、答え`send_back`）。欄を持つ 17 件のうち ask 648 は runtime が閉じたので母数に入らない。`approve_plan`の 22 件の答えはどれも`send_back`。
- 比較できない kind（`approve_landing`・`approve_plan`の前後の率、ADR の対象外の kind）は上の節と同じ理由で比べない。

### AIがlandを選んだrunの着地の後（7日）

7日に`concern_decided`の`applied: true`・推奨`land`は 58 件、52 run で、52 run とも着地した（72時間までの着地は 20 run）。下の表は AI が land を選んだ順に並べた。着地の後の照合は測った時点（2026-10-26）までで、最後の着地（task 2160、2026-10-09T07:50Z）からも 16 日を超えている。

| task | run | 着地 | 着地の後 |
| --- | --- | --- | --- |
| 793 | a22cd69e | 2026-10-03T04:39Z | なし |
| 1435 | ae4bbfa9 | 2026-10-03T14:43Z | なし |
| 1461 | 3a8484c5 | 2026-10-03T17:19Z | なし |
| 1402 | 2dcbe739 | 2026-10-03T18:17Z | なし |
| 251 | 64727486 | 2026-10-03T20:27Z | なし |
| 1462 | fcbdd009 | 2026-10-04T00:06Z | なし |
| 1520 | e9536c58 | 2026-10-04T03:05Z | **直す task が出た**（下） |
| 1505 | 0de6cd72 | 2026-10-04T03:49Z | なし |
| 1583 | 6357a771 | 2026-10-04T04:25Z | なし |
| 1506 | 280d9606 | 2026-10-04T05:27Z | なし |
| 1591 | 3c5a55c1 | 2026-10-04T08:35Z | なし |
| 1570 | a1b7032b | 2026-10-04T10:30Z | なし |
| 1712 | 44ad0456 | 2026-10-04T15:06Z | なし |
| 1713 | 853c5ae1 | 2026-10-04T16:03Z | なし |
| 1660 | b826f03c | 2026-10-04T23:50Z | なし |
| 1633 | 857eea61 | 2026-10-05T01:12Z | なし |
| 1818 | 7cac1bf7 | 2026-10-05T03:06Z | なし |
| 1550 | 947e89a1 | 2026-10-05T05:59Z | なし |
| 1225 | f1d2111a | 2026-10-05T22:33Z | なし |
| 1661 | 659ddb93 | 2026-10-05T23:26Z | なし |
| 1829 | b1ac7c48 | 2026-10-06T00:20Z | なし |
| 1836 | e66aa82f | 2026-10-06T00:54Z | なし |
| 1887 | 191414fa | 2026-10-06T01:55Z | なし |
| 1846 | a906f17a | 2026-10-06T02:23Z | なし |
| 1911 | f6f34026 | 2026-10-06T02:25Z | なし |
| 1521 | aa50261c | 2026-10-06T02:38Z | なし |
| 1657 | 1ef538f4 | 2026-10-06T03:22Z | なし |
| 1866 | d451df26 | 2026-10-06T11:05Z | なし |
| 1557 | 98b5d449 | 2026-10-06T12:07Z | なし |
| 1867 | b7e5759b | 2026-10-06T11:32Z | なし |
| 1855 | 4cf887af | 2026-10-06T22:51Z | なし |
| 1875 | 38397795 | 2026-10-07T03:10Z | なし |
| 1944 | 83432f1b | 2026-10-07T03:45Z | なし |
| 1974 | 69a3ad72 | 2026-10-07T03:49Z | なし |
| 1965 | 67b5a04f | 2026-10-07T04:19Z | なし |
| 1922 | 31172cf2 | 2026-10-07T09:12Z | なし |
| 1927 | 38e2de9e | 2026-10-07T10:27Z | なし |
| 1962 | e2f02d00 | 2026-10-07T14:24Z | なし |
| 2079 | 8f3514c0 | 2026-10-07T16:22Z | **直す task が出た**（下） |
| 2013 | ab87d3b9 | 2026-10-07T22:20Z | **直す task が出た**（下） |
| 2107 | 75bb765a | 2026-10-07T22:59Z | なし |
| 1863 | c5891fad | 2026-10-08T01:42Z | **直す task が出た**（下） |
| 2117 | 11e6ddd4 | 2026-10-08T03:17Z | なし |
| 2122 | dc342ddf | 2026-10-08T06:10Z | なし |
| 2125 | 19d68ea5 | 2026-10-08T07:01Z | なし |
| 2066 | bb84ed30 | 2026-10-08T07:17Z | なし |
| 1552 | 45f67d1f | 2026-10-08T11:14Z | なし |
| 1553 | dd06164e | 2026-10-08T13:56Z | なし |
| 1554 | 26972e1a | 2026-10-08T13:12Z | なし |
| 1556 | 30f12d7f | 2026-10-08T14:30Z | なし |
| 2155 | c8fbf81d | 2026-10-08T15:43Z | なし |
| 2160 | 916ef878 | 2026-10-09T07:50Z | なし |

調べたこと（上の節の照合に、2 つを足した）:

- revert: main の 2026-10-02 以降の commit（1,106 本）に revert は無く、52 run の着地の commit はどれも今の main に残っている（`git merge-base --is-ancestor`）。
- 差し戻し: 52 task の`task_reopened`は task 1875・1552（2 回）・1927 の 4 件で、どれも plan review が着地の前に proposal を差し戻したもの。着地の後の`task_reopened`は 0 件。`landing_recheck_failed`（task 1462・1520・1962）も着地の前の rebase の衝突。
- 直す task（上の節と同じ照合）: 後の commit の message で「task <ID>」と 52 task を名指すものは 68 本（task との組で 77）あり、1 本ずつ読んだ。直すものは task 1658（1520 を直す）だけで、ほかは着地の後に予定していた設定の切り替え（1225・1591 など）、測定、rebase の記録、receipt の follow_up の残り。後の`follow_up_registered`の title で 52 task を名指すのは、その run 自身の receipt のもの（1520・1583）だけ。
- 足した照合 1（CI と flaky の finding）: 上の照合は直す commit が task を名指さないと見つけられない。そこで、52 run の着地の commit が足した・変えた`fn`の名前を、`dagq findings --all`の`flaky_test`の subject と`ci_failure`の`detail.tests`の test の名前（518 件）と突き合わせた。当たったのは task 2079 と 1863 の足した test（下）と、2026-10-14 の 1 回で 277 本が同時に落ちた`ci_failure`（finding 617）に入った test（1225・1633・1712・2079 のもの）だけで、617 は 1 つの run に帰せないので数えない。
- 足した照合 2（文書の検査）: task 2013 の後の commit の message に「main は review.md が予算を超えている（after task 2013）」とあったので、2013 の着地の commit とその親で`scripts/check-design-docs.sh`を流した。

**直す task が出た 4 run:**

- task 1520（着地 2026-10-04T03:05Z）: 上の節のとおり。task 1658（commit bfeada1d、10-04T05:12Z）が tests/e2e.rs の期待値を直した。原因は着地の前に e2e を流す範囲（`[e2e]`の paths）の外の変更。
- task 2013（着地 2026-10-07T22:20Z）: review.md に 2 行を足し、`check-design-docs`は親（22f32922~1）では通り、2013 の commit（22f32922）では「review.md: 30983 bytes without the frontmatter, over the budget of 30720 bytes」で落ちる。task 2122（commit 04929da0、10-08T06:10Z。task 2105 の receipt の follow_up で 10-08T02:11Z に登録）が review.md の節を分けて直し、その run は follow_up の defect「着地が check-design-docs を通らずに main を壊す」（task 2144）を出した。2013 の concern（attempt 2）は queue-hold.md の古い 1 行を挙げただけで予算には触れていない。原因は concern の判断ではなく、着地の検証がこの検査を流さなかったこと。task 2122 は 2013 を名指さずに登録されたが、壊れを直したのはこの task なので数える。
- task 2079（着地 2026-10-07T16:22Z）: 足した IT`provider_executables::the_supervisors_entry_keeps_a_link_and_finds_a_gone_claude_again_on_path`が main の CI で落ち（`ci_failure` finding 298、10-07T21:15Z。`flaky_test` finding 374、10-08T12:47Z。finding 784、10-17）、task 2991（commit f40b3d2c、10-22T06:45Z）が直した。原因は supervisor の登録（`register_supervisor`）と providers の記録（`set_supervisor_providers`）が別の write で、test がその間を読みうること。2 つの write は 2079 の前からある（2026-09-28 の c7150542）。2079 の concern（attempt 2）は決定 4 の範囲の絞り込みだけを挙げ、この競合は review の材料に無い。原因は concern の判断ではなく、負荷の下で出る時間の競合。
- task 1863（着地 2026-10-08T01:42Z）: 足した IT`planner_headless::a_headless_draft_planner_ends_while_its_question_waits_and_a_new_one_goes_on_from_the_answer`と`…a_headless_request_planner_ends_while_its_question_waits_and_the_answer_opens_one_that_declines`が main の CI で繰り返し落ち（`ci_failure` finding 332、10-08T04:31Z から finding 1008、10-21 まで 10 件以上）、task 2966（commit 699bc0bc、10-22T04:05Z）が直した。原因は 1863 が足した新しい planner の prompt で運ぶ answer の経路の時間の判断（`plan_review.rs`の`answer_unread`）で、最初の turn が planner の作成の秒の中に終わると planner が待ちから抜けない。1863 の concern（attempt 7）は手元の証拠の小さな抜けと design の欄の一覧だけを挙げ、この判断は review の材料に無い。原因は concern の判断ではなく、1 秒に満たない turn でだけ出る runtime の時間の不具合。

**誤った自動の land（着地の後に直す task が出たもの）は 7日の 52 run のうち 4 件（7.7%）。** 上の節と同じ照合（revert・`task_reopened`・task を名指す commit と follow_up）だけで数えると 1 件（task 1520）で、足した照合の 2 つで 3 件（2013・2079・1863）が増えた。72時間までに着地した 20 run では 1 件（1520）。4 件とも原因は concern の判断ではない（e2e の paths、着地の検証の範囲、時間の競合 2 件）。4 件とも同じ concern を人が見ても同じ材料しかなく、人なら着地を止めたかは材料から決められない。

上の節の 14 run の調べ直し: 14 run のうち直す task が出たのは今も task 1520 の 1 件だけ。1712 が変えた test`a_review_without_required_agents_is_unchanged`は finding 617（277 本が同時に落ちた回）にだけ出ており、1712 の着地から 10 日後で、1712 に帰せない。ほかの 12 run は revert・着地の後の差し戻し・直す task・足した test の CI の失敗のどれも無い。

着地の時にその run の receipt の follow_up として残りを登録した run（2013 の queue-hold.md の docs_drift など）は、上の節と同じく着地の前に分かっていた残りなので数えていない。

### 暫定の結論の確かめ

- **ADR の対象の ask の 1日あたりの件数（20.4 → 27.2）: 件数の増えは確かめられた（72時間 34.0、7日 27.4 件/日）が、増えの中身は ADR の対象の判断ではない。** 7日の増えの大半は headless の review が verdict を返さなかった`approve_landing`（50 件、うち 36 件は 2026-10-05 の`subagents_unsupported`の一度の集中）で、review の材料が無く推奨の出ない ask。これを除くと 72時間 21.7、7日 20.3 件/日で前（20.4）と変わらない。日ごとでも 4・6・7 日目は 15〜16 件で前より少なく、3・5 日目の集中が平均を上げた。一方、ADR-t451-1 で減ると見込んだ concern と revise の上限超えの ask は、前 51 件（6.9 件/日）に対し 7日 72 件（10.3 件/日）で減っていない。concern は 5.4 → 4.9 件/日とほぼ同じで、増えたのは revise の上限超え（1.5 → 5.4 件/日）。revise の上限超えが増えた原因は、この記録からは決めきれない（期間の task の構成と review の厳しさを分けられない）。
- **`planner_question`の答えまでの中央値（3.2 → 42.4 分）: 延びは確かめられた（72時間 71.8 分、7日 42.4 分）。** 件数は 9.5 → 6.3 件/日に減り、10 分以内に答えたものが 43 件 → 15 件に減った一方、60 分を超えたものは 21 件（2.9 件/日）→ 20 件（2.9 件/日）で変わらない。planner が ask にせず決めるようになった分（`follow_up_adopted`の`ask_id: null`）がすぐ答えの出る ask だったとすれば、残った ask の中央値が延びるのと合う。ただし inbox と人の答えの都合（日ごとの中央値が 2.2〜260 分と大きく揺れる）を分けて測っていないので、原因はこの記録からは決めきれない。
- **誤った自動の land（14 run のうち 1 件）: 変わった。** 後の期間を 7日にして照合を足すと 52 run のうち 4 件（7.7%）。上の節の 14 run の中では今も 1 件。4 件とも concern の判断ではなく、着地の前の検証の範囲（e2e の paths・`check-design-docs`）と、review の材料に出ない時間の不具合が原因だった。concern の判断による誤った land は 7日でも 0 件。
- 推奨との一致率は`planner_question` 96% → 98%（7日）で変わらず、AI が決めて ask にしなかった件数は 7日で 1,245 件（review の land 58・send_back 17、plan review 64、observer 925、planner 181）。
