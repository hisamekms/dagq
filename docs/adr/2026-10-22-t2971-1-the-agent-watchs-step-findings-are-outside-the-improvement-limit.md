---
id: adr-t2971-1
type: adr
title: supervisorのagentの見張りが記録した手順のfinding（agent_eval_watch_due）も改善の上限に数えずdraftと改善のfindingより先にplannerを立て、finding recordはどの役割でも見張りのfindingの種類を拒む（ADR-t2928-1決定3・4、ADR-0051決定25をamends）
status: accepted
created: 2026-10-22
updated: 2026-10-22
accepted_on: 2026-10-22
amends:
  - adr-t2928-1 decision 3
  - adr-t2928-1 decision 4
  - adr-0051 decision 25
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - security
related:
  - adr-t2928-1
  - adr-0051
  - adr-t1728-1
  - design-supervisor-lifecycle-finding-planners
  - design-agent-eval
---

# ADR-t2971-1: agentの見張りの手順のfindingも改善の上限の外に置き、finding recordは見張りの種類を拒む（ADR-t2928-1決定3・4、ADR-0051決定25をamends）

## Context

supervisorは採用したagentの定義を新しい本番のreviewで見張り（[ADR-t1728-1](2026-10-06-t1728-1-agent-definitions-cases-and-eval-as-a-queue-service-use-case.md)）、測り直す時期と手順（ケースの集合の依頼・hold-outの依頼・拒まれた周）を印付きのfindingとして記録する。
見張りはそのfindingに立つruntimeのplannerだけで次の手順に進む。
このplannerはproposalを出さず、集合かhold-outを依頼してfindingをresolveするか、理由を書いてdismissするだけである。

[ADR-t2928-1](2026-10-20-t2928-1-only-the-ci-watchs-ci-failure-findings-are-repairs-outside-the-improvement-limit.md)は、改善の上限（[ADR-0051](0051-kpi-time-series-report-and-push.md)決定25）に数えずdraftより先にplannerを立てるfindingを、supervisorのCIの見張りが記録した`ci_failure`だけに狭めた。
見張りの手順のfindingは改善として上限で待つ。
本番の上限は2026-10-08から達したままなので、見張りは最初のfindingから先に進まない。
上限が守るのは改善の提案がqueueを埋めて人の計画を押しのけないことで、proposalを出さない手順のplannerはそれに当たらない。

種類だけで免除すると、observerが`finding record`でその種類を名乗り、上限と順番を飛ばすplannerを開ける（ADR-t2928-1のContextと同じ穴）。
見張りのfindingはsupervisorの見張りが記録者supervisorとして直接書き、`finding record`を通らない。

## Decision

1. **`finding record`は見張りのfindingの種類を拒む。** `finding record`（queue serviceのクライアントモードも同じuse case）は、ADR-t2928-1決定1の`ci_failure`の拒否を残したまま、supervisorの見張りだけが記録する手順のfinding（`agent_eval_watch_due`）と成績のfinding（`agent_eval_below_threshold`）の種類を、findingを記録できるどの役割（observer・supervisor・人のCLIも）からも拒む。その種類はsupervisorの見張りだけが記録すると分かる理由で断り、何も記録しない。見張りの記録は変わらない。
2. **見張りの手順のfindingは改善ではない（ADR-t2928-1決定3をamends）。** ADR-t2928-1決定3の「修理」と同じく、種類が`agent_eval_watch_due`で記録者がsupervisorのfindingのplannerとproposalは改善の上限とKPIの改善のproposalの数に数えず、上限で待たせない。種類だけでは免除しない: 決定1の前に記録された行や他の経路の行が`agent_eval_watch_due`を名乗っても、改善として上限に数えて待たせる。成績のfindingは定義の改善のtaskを出す改善なので、記録者に依らず上限に数えたままにする。
3. **順番と枠は`ci_failure`の修理と同じにする（ADR-t2928-1決定4をamends）。** 決定2のfindingのplannerは、修理と同じく計画の依頼のplannerの後、draftと改善のfindingのplannerより前に、印の古い順に立つ。runtimeのplannerの枠は共有のままで別の枠を作らない。上限に数えるか、上限で待つか、どの順に立つかは1つの判定で決め、KPIと順番も同じ判定に従う。
4. 変えないもの: CIの記録の制限と修理の免除の条件（ADR-t2928-1決定1・2）、修正taskの優先度とplannerの回数の上限（同決定5）、見張りがfindingを記録する条件と手順（ADR-t1728-1）。既存のfindingは書き換えない。

## Alternatives

- **種類だけで免除する**: observerが`finding record`で`agent_eval_watch_due`を名乗って印を付ければ上限と順番を飛ばせる。
- **上限の外に置くが順番は改善のfindingの後にする**: 印の古い改善のfindingが上限で止まるたびに後ろのfindingも立たない順番に並び、見張りがまた待つ。
  判定を2つに分けることにもなる。
  見張りの手順のfindingは定義ごとに1件ずつしか開かず、draftより先に枠を取る量は小さい。
- **見張りの手順のfindingに別の枠を作る**: 枠の設定と数え方が増える。手順のplannerは短く、共有の枠で足りる。

## Consequences

- 改善の上限に達していても、見張りは手順のfindingごとにplannerが立って進む。
- observerや人の`finding record`は見張りの種類を記録できない。見張りの対象の定義の改善を提案したいときは別の種類で記録し、改善の上限に従う。
- 決定1の前にsupervisor以外が記録した`agent_eval_watch_due`のfindingは、改善として上限で待つ。
- 上限・順番・数え方の今の姿は[Finding planners](../design/supervisor-lifecycle/finding-planners.md)の3と11、見張りは[agentのeval](../design/agent-eval.md#採用した定義の見張り)が持つ。
