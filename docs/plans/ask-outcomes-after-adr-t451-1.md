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

**結論は暫定。** 後の期間は境目から約1.9日で、目安の3日に足りない（AIの判断の件数は足りている）。再測定は task 1321 の receipt の follow_up（measurement）に出した。

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
