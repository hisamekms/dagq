---
id: adr-t1662-2
type: adr
title: 記録を論理ストア（SSOTのStateStore・EventStore・SessionStepStore・NodeSampleStore、ビューのLedgerStore・ReportStore）と消えてよい材料（run dir・log・CSV・transcript）に分け、今のアダプタはqueue.dbの表にし、StateStoreとEventStoreは今どおり同時に確定して計測はStateStoreを読まず、sessionのstepは90日だけ重複を除き抜けを検出して持ち、hostの負荷はNodeSampleStoreを恒久の記録にしてCSVを写しにし、RunLog・EventReads・TaskStoreをEventStore・StateStoreにまとめ直す（ADR-0049決定5をamends）
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-0049 decision 5
owners:
  - hisamekms
tags:
  - runtime
  - persistence
  - observability
related:
  - adr-0040
  - adr-0048
  - adr-0049
  - adr-0032
  - adr-t1545-1
  - adr-t1662-1
  - adr-t1662-3
  - design-measurement
  - design-persistence
  - design-architecture
  - design-supervisor-lifecycle-host-metrics
---

# ADR-t1662-2: 計測のストアの抽象化（SSOTとビュー）

## Context

計測の作り直し（request 13、2026-10-04に人が承認。モデルは[ADR-t1662-1](2026-10-04-t1662-1-runs-are-covering-phase-intervals-folded-into-a-ledger.md)）は、記録から台帳を作り、統計は台帳だけを読む層にする。今の記録は、queue.dbの状態の表とrun_events、run dir・log・hostの負荷のCSV（`host/metrics-YYYYMMDD.csv`）・Claude Codeのtranscriptに散っていて、どれが消えてはいけない正本（SSOT）で、どれが作り直せるビューか、どれが消えてよい材料かが決まっていない。statsはrun_eventsに加えてhostのCSVも読む。

[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定5（[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定5を引き継いだ現行の決定。ADR-0040はsupersededで現行の決定を持たない）は「集計はrun_eventsから再導出し、新しい表は持たない」と決めた。台帳を持つことと、sessionのstepとhostの負荷を恒久に持つことは、この文言に当たる。人は台帳を持ってよいと決めた（request 13のL・ストアの節）。

読み取りのportは`RunLog`・`EventReads`・`TaskStore`が重なりながら分かれていて、計測のために別の抽象を足すとさらに増える。goal 100はportとmoduleをcontextごとに分け直している（[ADR-t1545-1](2026-10-04-t1545-1-split-the-runtime-by-layer-and-context.md)）。

## Decision

1. **論理ストアと材料。** 計測が扱う記録を次の6つの論理ストアと材料に分ける。各々の中身と今のアダプタの表は[計測の設計](../design/measurement.md)の「SSOTとビュー」が持つ。
   - SSOT: **StateStore**（今の状態。task・run・ask・goalなど）・**EventStore**（起きたことのappend-onlyの記録）・**SessionStepStore**（sessionのstep。ADR-t1662-1決定7）・**NodeSampleStore**（nodeの資源の連続の値）。
   - ビュー: **LedgerStore**（台帳。SSOTから作り直せる）・**ReportStore**（日次のレポートなど統計の出力）。
   - 材料: run dir・log・CSV・transcript。消えてよく、台帳と統計は読まない。
2. **今のアダプタはqueue.dbの表。** 6つの論理ストアの今のアダプタはqueue.dbの表（ReportStoreは今のqueueのdirのレポートのファイルと`report_written`）にする。別のDBや外のサービスに替えるときはこのアダプタを替える。
3. **D1 StateStoreとEventStoreは今どおり同時に確定する。** 状態の変更とそのeventは今と同じ1つのtransactionで書く。計測の層はEventStore・SessionStepStore・NodeSampleStoreだけを読み、StateStoreを読まない。eventだけをSSOTにする移行（状態をeventから導く）はこのADRの外。
4. **D2 SessionStepStoreは90日だけ持つ。** stepは`(session_id, seq)`で重複を除き、seqの抜けを検出して抜けとして残す（推して埋めない）。90日より古いstepは消してよい。runに属するstepは台帳のrunの行に畳んだ後に消えても台帳は保つ。
5. **NodeSampleStoreはhostの負荷の恒久のストア。** goal 110の環境の口を待たず、今のhostの記録（supervisorが間隔ごとに取るsample）から作る。連続の値をeventにしない今の決め方は保ち、表に持つ。今の`host/metrics-*.csv`は同じsampleから書く人が読む写しになり、統計は読まない。保持の期間にあるCSVは一度だけ取り込む（task 1685）。
6. **D6 portをまとめ直す。** `RunLog`・`EventReads`・`TaskStore`はEventStore・StateStoreのportにまとめ直し、それと並ぶ別の抽象を足さない。新しいport（EventStore・SessionStepStore・NodeSampleStore・LedgerStore・ReportStore）と台帳の係は、goal 100のmodule（task 1554の観測と分析のport、task 1552の観測と分析の状態、task 1556の観測と分析の組み立て）に置き、`Supervisor`の本体に状態を足さない。
7. **ADR-0049決定5をamendsする。** 「集計はrun_eventsから再導出し、新しい表は持たない」に次の2つの例外を足す。(a) SSOTから作り直せる派生のビュー（台帳）は表に持ってよい。(b) 恒久の入力のストア（SessionStepStore・NodeSampleStore）は表に持ってよい。集計がSSOT（run_eventsを含むEventStore）から再導出できることは変えない。決定5のほかの部分（閾値超え・`--since`・observerの入力）はこのADRでは変えない。ADR-0040は変えない。
8. **旧方式の行は作り直せないので捨てない。** LedgerStoreはビューだが、切り替え前のrunの行のlegacyのJSON（ADR-t1662-1決定18）は一度だけ書いて凍結するので、台帳を捨てて作り直すときも残す。
9. **区分を設計書に書く。** 新しいストアやビューを足すtaskは、同じ変更で計測の設計の「SSOTとビュー」の節に区分（SSOT・ビュー・材料）と今のアダプタを書く。

## Alternatives

- **今のまま（run_eventsとCSVを読み手が直接読む）**: 台帳が無いと読み手ごとに畳み方が割れ、CSVは消えると過去の統計が変わる。
- **eventだけをSSOTにして状態をeventから導く**: 計測の作り直しに要らず、状態の書き込みの全てを変える大きな移行になる。別のADRで決める。
- **stepとhostの負荷をeventにする**: 連続の値と細かいstepでrun_eventsが膨らみ、`events`・`watch`の読み手を遅くする。
- **計測用に別の抽象（`MeasureStore`など）を足す**: `RunLog`・`EventReads`・`TaskStore`とさらに重なる。
- **台帳を別のDBに置く**: 今はqueue.dbの表で足り、アダプタを替えれば後から移せる。

## Consequences

- queue.dbに台帳・step・nodeのsampleの表が増える（migrationは後続のtask）。台帳はいつでも捨てて作り直せる（旧方式の行を除く）。
- hostのCSVを消しても統計は変わらない。CSVは人が読む写しとして残る。
- stepは90日を過ぎると消え、runを持たない対象のstepは読めなくなる（ADR-t1662-3）。
- portのまとめ直しはgoal 100のmoduleの分け方に合わせて進み、その間は今のportと並ぶ。
