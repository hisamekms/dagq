---
id: adr-t2951-1
type: adr
title: findingに紐づいたproposalが走っているか・終わったかは、proposalのtaskとproposalが持つgoalの今のtaskの和で判断し、和が空のproposalは改善の上限に数えず、acceptedで和が空ならfindingを印を外してopenに戻す（ADR-0047決定18・ADR-0051決定25をamends）
status: accepted
created: 2026-10-22
updated: 2026-10-22
accepted_on: 2026-10-22
amends:
  - adr-0047 decision 18
  - adr-0051 decision 25
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - observer
related:
  - adr-0047
  - adr-0051
  - adr-t2928-1
  - design-supervisor-lifecycle-finding-planners
  - design-supervisor-lifecycle-observer
---

# ADR-t2951-1: findingのproposalはproposalのtaskとgoalの今のtaskの和で走り、終わる（ADR-0047決定18・ADR-0051決定25をamends）

## Context

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定18は、findingに紐づいたproposalのtaskがすべて終わったらfindingを`resolved`か`open`にするとし、[ADR-0051](0051-kpi-time-series-report-and-push.md)決定25は、taskがすべて終わっていないproposalを改善の上限に数えるとした。どちらも「proposalのtask」しか読まない。

proposalはtaskだけでなくgoalも持てる。goalだけを持つproposalはplan reviewを通る正当な形で、そのgoalのtaskは後で別のproposalに入って走る。taskを1件も持たないproposalも残りうる（持っていたtaskが別のproposalに移った）。実装はtaskの一覧が空のとき「まだ終わっていない」とし、そのproposalは改善の上限を永遠に埋め、findingも`proposed`のまま終わらなかった。本番では、goalだけを持つproposalとtaskもgoalも持たないacceptedのproposalの2件が上限を埋め、以後、印の付いた改善のfindingにruntimeのplannerが1件も立たなかった。

## Decision

1. **proposalが直すtaskは、proposalのtaskとproposalが持つgoalのtaskの和。** goalのtaskは今の所属で読み、同じtaskは1回だけ数える。下の2・3はこの和を「proposalのtask」として読む。
2. **改善の上限に数えるのは、和に終わっていない（`completed` / `canceled`以外の）taskがあるproposalだけ（ADR-0051決定25の数え方を変える）。** `canceled`のproposalと、和が空か全部終わったproposalは、proposalの状態（`submitted` / `revising` / `accepted`）によらず数えない。KPIの改善のproposalの数と、上限で待つfindingの一覧も同じ数え方にする。KPIは期間の終わりまでの記録から組み立て直すので、goalがどのproposalに入ったかはその記録で読み、goalのtaskは今の所属のうち期間の終わりまでに記録のあるtaskだけを数える（読み方はKPIの数え方のdoc commentが持つ）。
3. **findingのsettleも同じ和で判断する（ADR-0047決定18の状態の遷移を広げる）。** 和が全部終わり1件以上`completed`なら`resolved`、全部`canceled`かproposalが`canceled`なら`open`に戻して印を外す（今のまま）。和が空の`accepted`のproposalは、直すtaskが無いので`open`に戻して印を外す。同じ問題が続けばobserverがまた印を付ける。和が空でplan reviewがまだ決めていない（`submitted` / `revising`）proposalのfindingは`proposed`のまま待つ。
4. 変えないもの: ADR-0047決定18の残り（findingの持つもの・一致の規則・observerと人とplannerの遷移）、ADR-0051決定25の上限の値・超えた分の扱い・数えないもの（人のproposal、follow_upの採否）、[ADR-t2928-1](2026-10-20-t2928-1-only-the-ci-watchs-ci-failure-findings-are-repairs-outside-the-improvement-limit.md)の修理のfindingの扱い。

## Alternatives

- **taskの一覧が空のproposalを数えないだけにする**: goalだけを持つproposalは、そのgoalのtaskが走っている間も改善が続いているのに上限から外れ、findingも`resolved`にできない（直したtaskがproposalのtaskでないため）。
- **goalのtaskを、goalがproposalに入った時点の所属で読む**: その時点の所属を残すものが無く、goalに後から足したtaskも改善の続きなので、今の所属で読む。
- **和が空のacceptedのproposalのfindingを`resolved`にする**: 何も直していないのに解決したことになり、手当てが効かなかったことが残らない。`open`に戻せばobserverが再び見て判断できる。

## Consequences

- goalだけを持つproposalのfindingは、そのgoalのtaskが全部終わると`resolved`か`open`になる。goalに終わっていないtaskが残る間（後から足したtaskも含む）は上限に数える。
- taskを持たないacceptedのproposalのfindingは、次のsupervisorのpassで印を外して`open`に戻り、上限が空く。
- 数え方とsettleの今の姿は[Finding planners](../design/supervisor-lifecycle/finding-planners.md)の7と11が持つ。
