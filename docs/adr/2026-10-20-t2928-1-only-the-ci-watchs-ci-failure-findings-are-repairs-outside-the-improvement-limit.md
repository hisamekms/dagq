---
id: adr-t2928-1
type: adr
title: 改善のproposalの上限に数えずdraftと改善のfindingより先にplannerを立てるのはsupervisorのCIの見張りが記録したci_failureのfindingだけとし、finding recordはどの役割でもci_failureを拒む（ADR-t2893-1を置き換え、ADR-t1920-1決定4・ADR-0051決定25をamends）
status: accepted
created: 2026-10-20
updated: 2026-10-20
accepted_on: 2026-10-20
supersedes:
  - adr-t2893-1
amends:
  - adr-t1920-1 decision 4
  - adr-0051 decision 25
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - ci
  - security
related:
  - adr-t2893-1
  - adr-t1920-1
  - adr-0051
  - adr-0044
  - design-supervisor-lifecycle-ci-watch
  - design-supervisor-lifecycle-finding-planners
---

# ADR-t2928-1: CIの見張りが記録したci_failureのfindingだけを改善の上限の外に置き、finding recordはci_failureを拒む（ADR-t2893-1を置き換え、ADR-t1920-1決定4・ADR-0051決定25をamends）

## Context

[ADR-t2893-1](2026-10-20-t2893-1-ci-failure-findings-are-repairs-outside-the-improvement-limit.md)は、mainの赤の修理が改善の上限（[ADR-0051](0051-kpi-time-series-report-and-push.md)決定25）で待ち続け、修正taskが登録されない不具合を直すため、`ci_failure`のfindingを改善の上限に数えず、runtimeのplannerの枠が空いた回にdraftと改善のfindingより先にplannerを立てるとした。その判定はfindingの種類が`ci_failure`かどうかだけで決まる。

ADR-t2893-1を入れたrunのreview（ask 1017）のsecurityの指摘: findingの種類は自由な文字列で、`finding record`はfindingを記録できるどの役割（observer・supervisor・人など）の種類も受ける。observerが種類を`ci_failure`としてproposalの印を付けて記録すれば、改善の上限に数えられないruntimeのplannerを、draftと改善より先に開ける。observerの見つけた改善が上限と順番を飛ばせることになり、改善の提案がqueueを埋めて人の計画を押しのけないという上限の目的（ADR-0051決定25、[ADR-0044](0044-findings-proposals-from-findings-and-quiet-observer.md)の分担）が崩れる。supervisorのCIの見張りは`finding record`を通らず、CIの確認の記録と同じ書き込みで、記録者がsupervisorのfindingを直接書く。

人はask 1017でADR-t2893-1のrunをlandとし、`ci_failure`をsupervisorのCIの見張りの書き込みに限る直しを後で入れると選んだ（挙げた案はauthorizationの検査と、findingの記録者の検査）。記録の側で拒むだけでは拒む前に書かれた行や別の経路の行が残り、判定の側で記録者を見るだけではobserverが紛らわしい`ci_failure`のfindingを記録できてしまうので、両方を入れる。ADR-t2893-1の主要な決定（決定1・2）の対象をともに狭めるので、amendsではなく丸ごと置き換える（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。

## Decision

1. **`finding record`は`ci_failure`を拒む。** `finding record`（queue serviceのクライアントモードも同じuse caseを通る）は、種類が`ci_failure`のfindingを、findingを記録できるどの役割（人・supervisorのCLIも）からも拒み、その種類はsupervisorのCIの見張りだけが記録すると分かる理由で断り、何も記録しない。CIの見張りの記録はこの経路を通らないので変わらない。
2. **修理として扱うのはCIの見張りの`ci_failure`だけ。** 下の決定3・4の「修理」は、種類が`ci_failure`で記録者がsupervisorのfindingだけに当たる。決定1の前に記録された行や他の経路の行が`ci_failure`を名乗っても、改善として上限に数え、上限で待たせる。既存のfindingは書き換えない。
3. **修理は改善ではない（ADR-t2893-1決定1を狭めて引き継ぐ）。** 改善の上限は、修理のfindingに立てたruntimeのplannerと、修理のfindingだけに紐づいたproposalを数えず、上限に達していても修理のfindingのplannerを止めない。改善のfindingと同じproposalに紐づいたときは、改善の方でそのproposalを数える。KPIの改善のproposalの数も、findingを記録したときの種類と記録者で同じ規則に従って数え、上限で待つfindingの一覧に修理のfindingを載せない。
4. **枠が空いた回にdraftと改善のfindingより先に立てる（ADR-t2893-1決定2を狭めて引き継ぐ）。** supervisorはrevise・答え・計画の依頼（人が待つもの）のplannerの後、draftと改善のfindingのplannerより前に、印付きの修理のfindingのplannerを印の古い順に立てる。runtimeのplannerの枠は共有のままで、枠が埋まっていれば次に空いた回に立つ。
5. 変えないもの（ADR-t2893-1決定3を引き継ぐ）: 修正taskの優先度（ADR-0051決定26の`normal`以下、ADR-t1920-1決定4の「急ぐときは人かinboxが上げる」）、1件あたりのplannerの回数の上限、plannerを立てる書き込みの中での再検査、plan reviewを通ること。ADR-t1920-1決定4の残り（findingの記録・鍵・持つもの）と他の決定。

## Alternatives

- **記録の側で拒むだけ**: 拒む前に記録された行と、`finding record`を通らない別の経路の行が、`ci_failure`を名乗って上限を飛ばし続ける。
- **判定の側で記録者を見るだけ**: 上限は守られるが、observerが`ci_failure`のfindingを記録でき、findingの一覧やCIの見張りの`ci_failure`のfindingを読む仕組み（taskが覆うものとしての退け方など）に紛らわしい行が混ざる。
- **authorizationの資源にfindingの種類を足し、`ci_failure`の記録をsupervisorだけに許す**: supervisorのCLIの`finding record`も同じ記録者で書けてしまい、CIの見張りの書き込みと区別できない。CIの見張りは`finding record`を使わないので、どの役割にも許さない方が狭い。

## Consequences

- observerや人の`finding record`は`ci_failure`を記録できない。CIの失敗を改善として提案したいときは別の種類で記録し、改善の上限に従う。
- 拒む前にsupervisor以外が記録した`ci_failure`のfindingは、改善として上限に数えられ上限で待つ。
- 上限・順番・数え方の今の姿は[Finding planners](../design/supervisor-lifecycle/finding-planners.md)の「CIの見張りの`ci_failure`のfinding」と11、記録は[CIの見張り](../design/supervisor-lifecycle/ci-watch.md)が持つ。
