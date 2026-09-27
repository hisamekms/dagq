---
id: adr-t808-1
type: adr
title: runtimeのplannerが人の判断を経ずにsubmitできないfollow_upの深さを2以上から3以上に上げる（ADR-0047決定16・20をamends）
status: accepted
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
amends:
  - adr-0047 decision 16
  - adr-0047 decision 20
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - follow-up
related:
  - adr-0047
  - adr-t598-1
  - design-domain-model
  - design-supervisor-lifecycle-draft-planners
  - design-persistence
---

# ADR-t808-1: runtimeのplannerが人の判断を経ずにsubmitできないfollow_upの深さを2以上から3以上に上げる（ADR-0047決定16・20をamends）

## Context

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定16は、runtimeが立てたplannerが人の判断（`planner_question`の`adopt`）を経ずにsubmitできないdraftとして、閉じたgoalのfollow_upと「深さ2以上のfollow_up」を挙げ、決定20はこの上限をfollow_upの連鎖を止める柵として残すと書いた。深さは人の判断を経ずに続いたfollow_upの段数で、元のtaskのrunが出したfollow_upは1、そのfollow_upのrunがさらに出したものは2になる。

2026-09-27に人がplannerに、人の確認が必須になる段数を1つ増やすよう指示した。深さ2のfollow_upは、元のtaskの手当ての続きであることが多く、plan reviewの検査を通れば人を待たずに進めてよいという判断である。

## Decision

1. **runtimeのplannerが人の`adopt`を経ずにsubmitできないfollow_upを、深さ3以上にする。** 深さ2までのfollow_upのdraftは、goalが開いていればruntimeのplannerが人を経ずにsubmitでき、plan reviewを通ってreadyになる。ADR-0047決定16の「深さ2以上のfollow_up」と、決定20の「決定16の自動で採用しない上限（閉じたgoalのfollow_up、深さ2以上）」の「深さ2以上」を「深さ3以上」と読み替える。
2. **それ以外は変えない。** goalが無いか閉じているfollow_upは、深さに関わらず今までどおり人の`adopt`が要る。深さの数え方（`add`は0、`integrate`の登録は元のtask + 1、人の`adopt`・人が開いたplannerのsubmit・bypassで0に戻す）と、人が開いたplannerのsubmitを拒否しないことも変えない。上限は連鎖を止める柵として残す。

## Consequences

- 深さ2のfollow_upで`planner_question`のaskがinboxに届かなくなり、人の答えを待つ時間が減る。深さ3以上になる連鎖は今までどおり人が止める。
- 閾値の定数・errorの文言・plannerのpromptの説明は[Domain model](../design/domain-model.md#draft-planners)・[Draft planners](../design/supervisor-lifecycle/draft-planners.md)・[Persistence](../design/persistence.md)が持つ。
