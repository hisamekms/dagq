---
id: plan-broker-e2e-image-cache
type: plan
title: broker の image の build のキャッシュの前後の、着地前の e2e と自動更新の関門の e2e の時間・broker の e2e の完了順・e2e の待ちの測定と、broker の e2e を差分で絞る案の判断
status: completed
created: 2026-10-07
owners:
  - hisamekms
tags:
  - testing
  - performance
  - measurement
related:
  - design-measurement
  - plan-landing-it-selection
---

# broker の image の build のキャッシュの前後の e2e の測定と、broker の e2e を差分で絞る案の判断

goal 93 の測定（task 1452）。broker の image の build で依存の crate の compile を使い回す変更（task 1451、commit `e193e335`）の前後で、着地前の e2e（`run_e2e_finished`）の時間と待ち、broker の e2e の完了順と 60 秒超の警告を比べ、着地前の e2e で broker の e2e を差分が broker に触れるときだけ流す案に進むかを決めた。src/・tests/・dagq.toml は変えていない。

## 要点

- **結論: 差分で絞る案には進まない。** broker の e2e を流さない回との差は着地前の e2e 1 本あたり finished in の中央値で約 12 秒（期間をまたぐ比べ方）〜約 30 秒（同じ期間の n=1 の参照）の目安で、e2e を待った回は後B の 30 本中 6 本（待った回の待ちの中央値 63 秒）と少ない。ADR-t963-1・ADR-t1233-2 の amends と broker の e2e の守りを薄める代わりに得るものが小さい（下の「結論」）。
- **1451 の効果（前 → 後A）**: 着地前の e2e の secs の中央値は 141 → 121 秒（−20）、finished in は 117.55 → 88.22 秒（−29.33）。broker の e2e が流れた回だけでは secs 179 → 139 秒（−40）、finished in 153.98 → 104.31 秒（−49.67）。ただし前の broker の e2e が流れた回は主に 15 本で、後A は 11 本なので、この差は 1451 だけの効果ではない（「比べ方の注意」）。
- **841 の増分（後A → 後B）**: broker の e2e が 1 本から 3 本になっても、secs の中央値は 121 → 103.5 秒（−17.5）、finished in は 88.22 → 79.34 秒（−8.88）で増えていない。broker の e2e が流れた回のうち broker の e2e が最後に終わった割合は 7/8 → 12/29 に下がった（60 秒超の警告は 8/8 → 28/29 で、broker の e2e は今も 60 秒を超えるが、他の test より先に終わる回が多い）。
- **timeout は後B の 1 件だけ**（2026-10-05T07:50:02Z、run `379c0da8` の attempt 2、secs 1800）。log では `broker::a_required_queue_claims_nothing_while_its_broker_is_stopped_and_tells_the_inbox` だけが 60 秒超の警告の後に結果を出さず、上限で切られた。この test は測定の期間の他の 89 回の log（着地前の e2e 30・関門の e2e 59）ではどれも ok。

## 母集団と記録の取り方

- **正本**: 本番 queue の `run_e2e_finished` の event（`dagq events --kind run_e2e_finished --full`。`--since`・`--until` で期間を切り、`--after` に前のページの最後の event の id を渡して 500 件ずつページング）。event 由来の列は `created_at`・`run_id`・`task_id`・payload の `attempt`・`secs`・`lock_wait_secs`・`timed_out`・`outcome`・`log`。
- **材料**: event の `log` が指す run dir の e2e の log（`runs/<run>/e2e-<attempt>.log`）。log 由来の列は、最後の `test result:` の行の `finished in`・passed と failed の和（流れた本数）、`test <name> ... ok|FAILED` の行（`ignored` は除く）の broker の e2e（`broker::` で始まる名前）と最後に出た test、`test <name> has been running for over 60 seconds` の行の test。流し直し（`.rerun.log`）は読まない。
- **期間**（UTC。境界は `run_integrated` の時刻）:
  - 前: 2026-10-01T19:03:49.783Z（1451 の着地の 3 日前）から 2026-10-04T19:03:49.783Z（task 1451 の `run_integrated`、commit `e193e335`）まで
  - 後A: 1451 の着地から 2026-10-05T02:54:07.372Z（task 841 の `run_integrated`、commit `be148d59`）まで
  - 後B: 841 の着地から T_end = 2026-10-07T12:00:00Z まで
- **除かないもの**: event は timeout・`outcome` が `unavailable`・log の欠損の回も 1 件も除かない。event 由来の値（secs・lock_wait_secs）はその回も集計に入れ、log 由来の列だけを「欠測」と書く。
- **1 回だけ読む測定**: 過去の本番の event と log を期間で分けて 1 回読むだけで、同じ条件の e2e を流し直す実験ではない。周回や交互の実行は過去の期間には当てはまらず、host の e2e を流すのは worker の権限の外なので、周回も交互もしない。

### 件数

| 期間 | event | log が無い | log はあるが test result の行が無い | log が読めた | うち broker の e2e が流れていない |
| --- | --- | --- | --- | --- | --- |
| 前 | 79 | 1 | 0 | 78 | 31 |
| 後A | 9 | 0 | 0 | 9 | 1 |
| 後B | 30 | 0 | 1 | 29 | 0 |

- 前の log が無い 1 件: 2026-10-04T04:05:47Z の run `6357a771` の attempt 1（secs 717、`outcome: unavailable`。cmux が 20 秒で応えず e2e を始められなかった）。
- 後B の test result の行が無い 1 件: 2026-10-05T07:50:02Z の run `379c0da8-57a4-4295-b46f-27e5f524fc4d` の attempt 2（secs 1800・lock_wait_secs 60・timeout）。e2e-2.log に test result が無い。
- broker の e2e が流れていない回: 前の 31 件は 2026-10-03T15:33Z〜2026-10-04T18:18Z で、task 1582 の一時除外（commit `679d2fcf`。broker と cmux に固有の e2e を登録から外した）の run 自身の e2e から、1451（broker の e2e を戻した）の run の前まで。後A の 1 件は 1451 の着地直後の 2026-10-04T19:45:33Z の run `c9c14221`（broker の e2e が戻る前の commit を base にした run とみられる）。

### 自動更新の関門の e2e

関門の e2e の log は run dir ではなく queue の dir の `logs/update-<時刻>-<commit>.e2e.log` に残っていたので、同じ 3 期間で別の表（`update-gates.csv`・`update-gates-summary.csv`）にした。母集団は `update_e2e_passed` と、`stage` が `e2e` の `update_failed`（payload の `log` は build の log を指すので、同じ名前の `.e2e.log` を読む）。関門には `lock_wait_secs` が無い。

| 期間 | event | log が無い | test result の行が無い | log が読めた | うち broker の e2e が流れていない | secs の中央値 | finished in の中央値（broker が流れた回） | broker の e2e が最後（broker が流れた回） |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 前 | 132 | 0 | 1 | 131 | 54 | 181（n=132） | 196.29（n=77） | 73/77 |
| 後A | 17 | 0 | 0 | 17 | 0 | 174（n=17） | 118.52（n=17） | 14/17 |
| 後B | 61 | 1 | 0 | 60 | 1 | 129.5（n=60。secs の無い `update_failed` が 1 件） | 92.89（n=59） | 36/59 |

関門の傾向は着地前の e2e と同じ（broker の e2e が流れた回の finished in は 196 → 119 → 93 秒、最後に終わる割合は下がった）。

## 表の列（回ごと: `runs.csv`）

- event 由来: `time`（`created_at`）・`event_id`・`period`（before・afterA・afterB）・`run_id`・`task_id`・`attempt`・`secs`・`lock_wait_secs`（payload に無ければ空。値を埋めない）・`timeout`（`timed_out` か secs が上限の 1800 以上なら 1）・`outcome`
- log 由来（`log_state` が `read` でない回は「欠測」と書き、値を埋めない）: `log_state`（read・missing・no_test_result）・`finished_in`・`tests_run`・`broker_tests`（流れた broker の e2e の名前）・`broker_count`（本数）・`last_test`（最後に終わった test）・`broker_last`（それが broker の e2e なら 1）・`slow_tests`（60 秒超の警告が出た test）・`broker_slow`（broker の e2e に警告が出たら 1）・`log`（queue の dir からの相対 path）
- **broker の e2e の test ごとの秒数は取得不能**: e2e の log（`src/infrastructure/e2e_gate.rs` が書く）は test ごとの時間を記録しないので欠測とし、全体の finished in や 60 秒超の警告を test ごとの時間として代用しない。

## 値の式

- 中央値: 昇順に並べ、件数 n が奇数なら真ん中、偶数なら中の 2 つの平均。p90: 昇順の ceil(0.9×n) 番目。最大: 最大の値。
- secs・lock_wait_secs の中央値・p90・最大の分母は期間の event の全件（timeout・log の欠損の回を含む）。
- finished in の中央値・p90・最大、broker の e2e が最後に終わった割合（分子 = `broker_last` が 1 の回、分母 = log が読めた回）、broker の e2e に 60 秒超の警告が出た割合（分子 = `broker_slow` が 1 の回、分母 = log が読めた回）は log が読めた回だけ。
- 待った回の割合: 分子 = lock_wait_secs が 1 以上の回、分母 = lock_wait_secs のある event（無い event は欠測として分母と待ちの中央値・p90・最大から外す）。待ちの中央値は待った回だけの中央値（全件の中央値も並べる）。
- 前後の差 = 後の中央値 − 前の中央値。1451 の効果は前と後A、841 の増分は後A と後B で比べ、前と後B を直接比べない。例外は「結論」の broker の e2e を流さない回との差の目安だけで（同じ期間の回が n=1 しか無いため）、前後の差としては使わず、同じ期間の n=1 の参照と並べる。
- 部分集合（`summary.csv` の `subset`）: `all` = 期間の event の全件、`broker_ran` = log が読めて broker の e2e が 1 本以上流れた回、`no_broker` = log が読めて broker の e2e が流れなかった回。

## 期間ごとの要約（着地前の e2e: `summary.csv`）

| 期間（部分集合） | event | secs 中央値 / p90 / 最大 | timeout | 欠測（log 無し・結果無し） | finished in 中央値 / p90 / 最大（分母） | broker が最後 | broker に 60 秒超 | 待った回 | 待ちの中央値（待った回） / 全件の p90 / 最大 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 前（all） | 79 | 141 / 288 / 717 | 0 | 1・0 | 117.55 / 246.76 / 522.59（78） | 47/78 | 47/78 | 5/78（欠測 1） | 170 / 0 / 1186 |
| 前（broker_ran） | 47 | 179 / 293 / 566 | 0 | — | 153.98 / 264.54 / 522.59（47） | 47/47 | 47/47 | 5/47 | 170 / 121 / 1186 |
| 前（no_broker） | 31 | 80 / 151 / 220 | 0 | — | 66.97 / 102.35 / 198.14（31） | 0/31 | 0/31 | 0/31 | — / 0 / 0 |
| 後A（all） | 9 | 121 / 327 / 327 | 0 | 0・0 | 88.22 / 144.55 / 144.55（9） | 7/9 | 8/9 | 2/9 | 85 / 128 / 128 |
| 後A（broker_ran） | 8 | 139 / 327 / 327 | 0 | — | 104.31 / 144.55 / 144.55（8） | 7/8 | 8/8 | 1/8 | 42 / 42 / 42 |
| 後B（all） | 30 | 103.5 / 270 / 1800 | 1 | 0・1 | 79.34 / 188.62 / 212.85（29） | 12/29 | 28/29 | 6/30 | 63 / 60 / 209 |
| 後B（broker_ran） | 29 | 103 / 270 / 367 | 0 | — | 79.34 / 188.62 / 212.85（29） | 12/29 | 28/29 | 5/29 | 66 / 66 / 209 |

全件の lock_wait_secs の中央値はどの期間も 0。前の lock_wait_secs の欠測 1 件と secs の最大 717 は、cmux が応えず e2e を始められなかった同じ回（run `6357a771` の attempt 1。payload に `lock_wait_secs` が無い）で、secs は除かずに入れている。timeout の 1 件（後B）の理由は「要点」のとおり broker の e2e の 1 本が結果を出さずに上限の 1800 秒で切られたこと。

### 比べ方の注意

- 流れた本数（`tests_run`）が期間で違う: 前の broker が流れた回は主に 15 本（38/47）、除外の間は 10 本、後A は主に 11 本（7/8。10 本＋broker 1 本）、後B は 13 本（10 本＋broker 3 本）。前（broker_ran）→ 後A の差には broker 以外の test が 14 本から 10 本に減った分（1582 の一時除外とその間の e2e の変更）が混ざる。
- 除外の間の 10 本は後A・後B の broker 以外の test と同じ組なので、`no_broker`（前）は「broker の e2e を流さなかったら」の参照になる。ただし時期が違い（2026-10-03〜04）、他の変更の影響は分けられない。同じ期間の broker を流さない回は後A に 1 回（finished in 70.29 秒・secs 72）と後B の関門に 1 回（finished in 64.17 秒・secs 94）しかない。
- 後A は 9 件（broker が流れた回は 8 件）と少なく、1451 の効果と 841 の増分の中央値は 1 件で動きうる。

## 結論: broker の e2e を差分で絞る案には進まない

- **縮む時間が小さい**: 期間をまたぐ比べ方の例外として（前後の差は混ぜない規則の外で、目安にだけ使う）、後B（broker の 3 本を含む 13 本）の finished in の中央値 79.34 秒と、broker の e2e を流さない同じ 10 本の組（前の no_broker）の 66.97 秒の差は 12.37 秒、secs では 103 − 80 = 23 秒（secs は cargo の compile の時間も含み、期間で揺れる）。同じ期間の broker を流さない回（n=1 が 2 つ、上の「比べ方の注意」）との差はこれより大きく、後A の着地前の e2e で 104.31 − 70.29 ≈ 34 秒、後B の関門の e2e で 92.89 − 64.17 ≈ 29 秒で、約 30 秒を示す。どちらも 1 件なので、差は約 12〜30 秒の幅で見る。p90 では finished in 188.62 − 102.35 ≈ 86 秒と開くので、遅い回では縮み方が大きいことがある。broker の e2e が最後に終わる回は後B で 12/29 に下がり、その他の回では test の完了順の上は broker の e2e が全体の時間を決めていない（CPU・podman を通じて他の test を遅らせる分は、test ごとの時間が無いので分けられない）。
- **e2e の待ちは少なく短い**: 後B で lock_wait_secs が 1 秒以上の回は 6/30、待った回の中央値は 63 秒、全件の中央値は 0。後B の 30 件の待ちの合計は 477 秒（約 8 分、約 2.4 日の間）で、1 本あたり約 30 秒縮んでも、縮む待ちはこの合計を超えない。待たなかった 24 件では、縮むのは e2e の工程そのものの数十秒だけになる。差が約 30 秒でも、ADR の amends と守りを薄めることに見合わない。
- **失うもの**: ADR-t963-1・ADR-t1233-2 の amends と、`dagq.toml` の `[e2e] paths` を test ごとに持つ仕組みが要る。broker の e2e は crate の外の runtime（claim・起動・着地）も通るので、「差分が broker に触れる」の線引きを誤ると守りが薄くなる。
- **test ごとの時間が無くても決められる**: broker の e2e を流さない同じ組の回との全体の時間の差（約 12〜30 秒）で判断でき、待ちが稀で合計も小さいので、test ごとの時間で差の内訳が分かっても結論は変わらない。test ごとの時間の計装は要らない。計装の follow_up は出さない。
- **見直す合図**: broker の e2e が最後に終わる割合が前（47/47）の水準に戻るか、着地前の e2e の待った回の割合や待ちの中央値が今より大きく増えたら、この表を作り直して決め直す。
- image の tag・health の照合・fail closed（ADR-t827-1）を変える案は推奨に含めない。

## 再計算の手順

本番 queue のある repository の中（どの worktree でもよい）で、読み取りの `dagq events` が使える状態で次を打つ。T_end と期間の境界は script の冒頭の定数。

```sh
python3 docs/plans/broker-e2e-image-cache/measure.py docs/plans/broker-e2e-image-cache
```

出力は `docs/plans/broker-e2e-image-cache/` の `runs.csv`（着地前の e2e の回ごと）・`summary.csv`（期間と部分集合ごとの要約）・`update-gates.csv`（関門の e2e の回ごと）・`update-gates-summary.csv`。この文書の値は 2026-10-07T12:09Z に固定バイナリ `~/.local/bin/dagq` で実行したもの。event は queue に残るが、run dir と `logs/` の e2e の log（材料）が消えると log 由来の列は同じ表では作り直せない。
