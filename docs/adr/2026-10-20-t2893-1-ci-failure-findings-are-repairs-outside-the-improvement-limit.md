---
id: adr-t2893-1
type: adr
title: CIの見張りのci_failureのfindingは着地先のbranchの修理で改善ではないとし、改善のproposalの上限に数えず待たせず、runtimeのplannerの枠が空いた回にdraftと改善のfindingより先にplannerを立てる（ADR-t1920-1決定4・ADR-0051決定25をamends）
status: superseded
created: 2026-10-20
updated: 2026-10-20
accepted_on: 2026-10-20
superseded_by: adr-t2928-1
superseded_on: 2026-10-20
amends:
  - adr-t1920-1 decision 4
  - adr-0051 decision 25
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - ci
related:
  - adr-t1920-1
  - adr-0051
  - adr-0047
  - design-supervisor-lifecycle-ci-watch
  - design-supervisor-lifecycle-finding-planners
---

# ADR-t2893-1: CIの見張りのfindingは改善の上限に数えず待たせない（ADR-t1920-1決定4・ADR-0051決定25をamends）

> **置き換え済み（2026-10-20）**: このADRの決定は現在有効ではない。現行の決定は[ADR-t2928-1](2026-10-20-t2928-1-only-the-ci-watchs-ci-failure-findings-are-repairs-outside-the-improvement-limit.md)を読む。

## Context

[ADR-t1920-1](2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)決定4は、mainのCIの赤から`ci_failure`のfindingを印付きで記録し、runtimeのfindingのplannerとplan reviewで修正taskにするとした。修正taskは改善のproposalとして[ADR-0051](0051-kpi-time-series-report-and-push.md)決定25の上限にも掛かり、帰結は「改善の上限で待つことがある」とした。goal 157の受け入れ条件1は、mainが赤くなったら修正taskが登録されることを求める。

本番のqueueでは、2026-10-08T05:53Zの`finding_planner_opened`（event 134773、finding 101）を最後に、どのfindingにもplannerが立っていない。2026-10-07〜10-14の窓に見張りは印付きの`ci_failure`のfindingを19件記録し、2026-10-20も印付きの`ci_failure`のfindingが20件余り`open`のまま、proposalを持たない（finding 336は31回見た）。原因は改善の上限である。

- 上限は既定の2で、runtimeのplannerが持ち主のproposalのうち紐づいたfindingがあり、taskがすべて終わっていないものを数える。proposal 885（finding 101、goal 180だけを持ちtaskが無い）とproposal 696（finding 70、taskもgoalも無い）は、taskが無いのでいつまでも「動いている」に数えられ、10-08から上限の2に達したままである。
- 上限に達していれば、`open_finding_planners`は印の古いfindingから順に見て最初の1件で止まる。印の古い改善のfinding（finding 102〜104など）が先頭に並ぶので、後から印の付いた`ci_failure`のfindingは上限が空いても順番が来ない。plannerはdraftと計画の依頼の後に残りの枠で立つので、枠が埋まっていても待つ。

壊れたmainを直すことは、観測から足す改善ではない。改善の上限（決定25）は、改善の提案がqueueを埋めて人の計画を押しのけないためのもので、壊れの修理を同じ枠で待たせる理由にならない。一覧のtestが着地を守るので着地は止まらないが、修理が登録されないまま一覧は伸び続ける（10-14の1件で277件）。

## Decision

1. **`ci_failure`のfindingは改善ではない。** 改善の上限（ADR-0051決定25）は`ci_failure`のfindingに立てたruntimeのplannerと、`ci_failure`のfindingだけに紐づいたproposalを数えず、上限に達していても`ci_failure`のfindingのplannerを止めない。改善のfindingと同じproposalに紐づいたときは、改善の方でそのproposalを数える。KPIの`improvement_proposals`も同じ規則で数え、上限で待つfindingの一覧に`ci_failure`のfindingを載せない。
2. **枠が空いた回にdraftと改善のfindingより先に立てる。** supervisorはrevise・答え・計画の依頼（人が待つもの）のplannerの後、draftと改善のfindingのplannerより前に、印付きの`ci_failure`のfindingのplannerを印の古い順に立てる。runtimeのplannerの枠（`runtime_planners`）は共有のままで、枠が埋まっていれば次に空いた回に立つ。
3. 変えないもの: 修正taskの優先度（ADR-0051決定26の`normal`以下、ADR-t1920-1決定4の「急ぐときは人かinboxが上げる」）、1件あたりのplannerの回数の上限、`BEGIN IMMEDIATE`での再検査、plan reviewを通ること。ADR-t1920-1決定4の残り（findingの記録・鍵・持つもの）と他の決定。

## Alternatives

- **taskの無いproposalを「動いている」に数えない形に直すだけ**: 改善のfindingの待ちは解けるが、改善が上限まで動いていれば`ci_failure`のfindingは改善と同じ列で待ち続け、受け入れ条件1が改善の流れに左右される。数え方の欠けは別に直す。
- **`ci_failure`専用の上限を設ける**: 同じ鍵の失敗は1件にまとまり、plannerは同じtestを直すtaskを探して重ねないので、数を絞る理由が無い。
- **修正taskを`high`にする**: 上限の外し方とは別の決定で、ADR-t1920-1が段4の前後比較の後に決めるとしたもの。ここでは決めない。

## Consequences

- mainが赤になれば、枠が空いた最初の回に修正のplannerが立つ。改善の上限に達していても改善の提案は増えず、`ci_failure`のfindingのplannerがその分だけ枠を使う。
- 同時に多くの赤が続けば`ci_failure`のfindingのplannerが枠を埋め、draftと改善のfindingのplannerが待つ。枠の数（`runtime_planners`）で抑える。
- 上限・順番・数え方の今の姿は[Finding planners](../design/supervisor-lifecycle/finding-planners.md)の「CIの見張りの`ci_failure`のfinding」と11が持つ。
