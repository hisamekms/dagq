---
id: adr-t1480-1
type: adr
title: workerは負荷の下で落ちるtestの再現のためにhostに負荷を足さず（負荷だけを作るprocess・同時の複数のtestのprocess・[run.env]の並列度を超える-jを禁じる）、記録と決定的な遅れと上限つきの繰り返しで確かめ、稀な失敗はfollow_upとCIの定時実行に任せる。plannerは高い負荷の下の再現をacceptanceに求めず、人がapprove_planで認めた例外だけを保つ
status: accepted
created: 2026-10-05
updated: 2026-10-05
accepted_on: 2026-10-05
owners:
  - hisamekms
tags:
  - testing
  - worker
  - planning
  - capacity
related:
  - adr-t920-1
  - adr-t598-1
  - adr-t1453-2
  - development-local-checks
  - development-task-registration
---

# ADR-t1480-1: workerは負荷の下で落ちるtestの再現のためにhostに負荷を足さない

## Context

このhostは8コアで、worker 3本と着地の検証（coverageの関門）とsupervisorの見張りが同じhostを使う。負荷の下で落ちるtestを直すtaskのworkerが、再現のためにhostに負荷を足していた（finding 36、capacity・load_average）。

- task 886のworkerは、nextestの`-j 16`のloopを3〜6本同時に1時間以上流した。loadは70〜150になり、cmuxの時間切れでsupervisorのstallの見張りが見えなくなり（finding 44）、他のrunのcoverageの関門が時間切れで殺されてresumeを1回数えた。
- task 1274のworkerは`-j 16`のstressを4本同時に流した（load 73〜113）。task 1058のworkerは、stressと12本の`yes`で負荷を作った（[load spikeの調査](../plans/load-spike-2026-09-27.md)の6.4節）。
- 計画の側も負荷を求めていた。task 1360・1361のacceptanceは`-j 16`の`--stress-count`で同時に流す再現を例に挙げ、task 1008のacceptanceは「load average 20以上の負荷（他のrunか、負荷をかける処理）」を求めた。

1本のrunが作った負荷が、他のrunの検証とsupervisorの見張りを壊していた。負荷の山の大きいもの（load 70〜150）はworkerが意図して作った負荷で、それを作らないことが最も直接の手当てになる。

[ADR-t920-1](2026-09-28-t920-1-light-worker-stress-and-heavy-repetition-in-scheduled-ci.md)は、稀な失敗を引き出す重い繰り返しをhostの外（GitHub Actionsの定時実行）に移したが、不具合の再現のためにworkerがhostで負荷を作ることは決めていない。この決定はADR-t920-1を変えない。workerのstress（足した・変えたtestを自然な負荷の下で軽く繰り返し、負荷が下がるのを待たない）もそのまま。

ただし、task 1360・1361（plan reviewのask 323、event 62021・62046で承認と適用）とtask 1344（ask 304）は、高い負荷の下での確認をacceptanceに持つtaskを、人がapprove_planのaskにreadyと答えて認めたもの。初回のplan review（proposal 531）は、一律の禁止がこの承認と両立しないと差し戻した。

## Decision

1. **(a) workerはhostに負荷を足す処理を起動しない。** 負荷を足す処理とは、testと別に負荷だけを作るprocess（`yes`・busy loop・stressの道具など）、同時に2本以上の`cargo test` / `cargo nextest`のprocess、`[run.env]`が渡すtestの並列度（`NEXTEST_TEST_THREADS`・`RUST_TEST_THREADS`）より大きい`-j` / `--test-threads`。
2. **(b) 負荷の下で落ちるtestは、負荷を作らずに再現し原因を確かめる。** 記録（eventのdump・log）を読む、待っている条件と上限を確かめる、testの中で遅れを決定的に作る（stubの遅延、上限を縮めるなど）、1本のprocessでの上限つきの繰り返し、のどれかによる。
3. **(c) それで再現しない稀な失敗は、直せる範囲を直して任せる。** 失敗のときの出力を増やし、receiptのfollow_ups（`flaky_test`）とCIの定時実行（ADR-t920-1決定2）に任せる。
4. **(d) plannerはtaskのacceptanceに高い負荷の下での再現を求めない。** load の値の指定、`-j`の引き上げ、同時の複数のstress、負荷をかける処理がそれに当たる。plan reviewはそれを求めるtaskを`revise`にする。
5. **(e) 人が明示して認めた例外は保つ。** plan reviewのapprove_planのaskに人がreadyと答えて、高い負荷の下での確認をacceptanceに持つtaskを認めたもの（ask 323のtask 1360・1361、ask 304のtask 1344）は(a)〜(d)の対象外で、workerはそのacceptanceどおりに確かめてよい。例外のtaskのworkerも、acceptanceが求める範囲（周回・並列度）を超えて負荷を足さない。新しい例外は人の判断（approve_planのaskへの答え）を経てだけ認められ、plannerもplan reviewも自分では例外を作らない。この決定は既存の例外を取り消さない（取り消すなら、人の判断を得てから1360・1361を計画し直す）。

同時のprocessの数・`-j`の上限・繰り返しの周回と時間の上限の数値は、このADRに書かず開発文書が持つ（ADR-t598-1決定3、置き場はADR-t1453-2）。

## Alternatives

- **`--max-load`を越えたら再現を止める**: workerのstressの「負荷が下がるのを待たない」と食い違い、loadが高いままだとworkerが止まり続ける。負荷を足さない形の方が単純で、他のrunを守る効果も直接。
- **他のrunの検証中に重いtestを待たせる（1032系）**: hostの共有の調整を要し、goal 36のtask 1281の測定が決め直す。この決定とは別に扱う。
- **例外も含めて一律に禁じる**: 人がapprove_planで認めた判断を、人に聞かずに覆すことになる。取り消すなら人の判断を先に得る。
- **負荷の再現を別のhostに移す**: 今は使える別のhostが無い。重い繰り返しはすでにCIの定時実行が受け持つ（ADR-t920-1）。

## Consequences

- workerが作る大きなloadの山（70〜150）が無くなり、他のrunの検証とsupervisorの見張りが負荷で壊れにくくなる見込み。
- 負荷の下でしか出ない稀な失敗は、手元で再現できないまま着地することがある。失敗のときの出力を増やしておき、follow_upとCIの定時実行のissueから直すtaskにする。
- 負荷に依らない再現（決定的な遅れ・上限を縮める）を書く手間が増えるが、その形のtestはそのまま回帰testとして残る。
- 例外のtask（1360・1361・1344）の間は、それらのrunが負荷を作ることがある。範囲はacceptanceが求めるものに限る。
