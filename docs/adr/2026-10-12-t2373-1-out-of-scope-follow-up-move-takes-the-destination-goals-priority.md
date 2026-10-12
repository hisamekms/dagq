---
id: adr-t2373-1
type: adr
title: AIのactorがfollow_upをout_of_scopeの判断で移すときは、元のgoalから継いでいただけの人の出どころの優先度を残さず移動先のgoalを継がせ、taskに直接付いた人の個別の優先度だけを保つ（ADR-t1975-1決定4をamends）
status: accepted
created: 2026-10-12
updated: 2026-10-12
accepted_on: 2026-10-12
amends:
  - adr-t1975-1 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - plan-review
  - priority
  - goal
related:
  - adr-t1975-1
  - adr-t1639-1
  - adr-t1639-2
  - adr-t1504-1
  - adr-t1504-2
  - adr-t1091-1
  - adr-t598-1
---

# ADR-t2373-1: out_of_scopeの判断で移すfollow_upは、継いでいただけの人の優先度を残さず移動先のgoalを継ぐ（ADR-t1975-1決定4をamends）

## Context

[ADR-t1975-1](2026-10-08-t1975-1-runtime-records-origin-and-person-set-priority-and-ai-actors-cannot-change-it.md)決定4は、AIのactorがtaskのgoalを変え、そのtaskの効いている優先度が人の出どころ（`human`）なら、移動の前の値を`human`の個別の優先度としてtaskに残すとした。AIが移動だけで人の指定を下げる穴を塞ぐためである。決定3でAIのactorはその値を変えられない。

その結果、plannerがfollow_upを`out_of_scope`の判断（[ADR-t1504-1](2026-10-04-t1504-1-follow-ups-belong-to-the-goal-whose-acceptance-needs-them.md)・[ADR-t1504-2](2026-10-04-t1504-2-runtime-records-and-enforces-follow-up-membership-judgements.md)）で受け皿のgoal（[ADR-t1639-2](2026-10-04-t1639-2-defer-improvements-outside-acceptance-to-a-low-goal-per-tag.md)）へ移すと、元のgoalから継いでいただけの値がtaskの個別の値として残り、plan reviewは変えられずに`approve_plan`のaskで止まり、人が`send_back`し、inboxが個別の優先度を外してgoalからの継承に戻す流れが繰り返した（2026-10-12だけで3回）。人の答えはどれも「所属のgoalを継ぐ」だった。2026-10-08以降に値を残した移動はすべてこの経路だった。ADR-t1639-2決定1（要らない改善は受け皿の`low`で走らせ、個別の指定は外す）とも食い違う。

## Decision

1. **follow_upの所属の判断を`out_of_scope`で記録して移すAIのactorの移動は、taskが元のgoalから継いでいただけの`human`の優先度を残さない。** taskは移動先のgoalの優先度と出どころを継ぐ（goalを外す移動なら`normal`）。人がgoalに付けた優先度はそのgoalの受け入れ条件の達成への指定で（[ADR-t1639-1](2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)）、受け入れ条件に要らないと判断したtaskはその指定の外にある。判断はruntimeが記録し、plan reviewが受け入れ条件との対応づけを検査するので（ADR-t1504-1・2）、AIが移動だけで値を下げる穴は記録と検査で塞がれる。
2. **taskに直接付いた人の個別の優先度は、どの移動でも変えない。** 人とinboxの`set-priority`、requestの値をruntimeが付けたもの（ADR-t1975-1決定2）、以前の移動で残ったものを含む。
3. **ほかのAIのactorの移動はADR-t1975-1決定4のまま値を残す。** `set-goal`や重複の整理など、所属の判断の記録を伴わない移動では、記録と検査を伴わない経路で値が下がるのを防ぐ。人とinboxの移動（判断の記録を伴うものも）は今どおり移動先を継ぐ。
4. **人の出どころの値が移動で効かなくなったことはeventに残す。** どの値を、どの判断の移動で外したかが、taskのeventから読める形にする（形はコードのdoc commentが持つ）。
5. **既存の値はmigrationで変えない。** 以前の移動で残った個別の値は人の出どころの値で、外すのは人とinbox（ADR-t1975-1決定6と同じ扱い）。
6. **ADR-t1975-1との関係。** ADR-t1975-1決定4の「follow_upの`out_of_scope`の移動」でも値を残すことを、決定1・2に改める。決定4のほかの部分（`set-goal`などの移動で値を残すこと、移動先が同じ値の`human`なら継ぐこと、人とinboxの移動）と、ADR-t1975-1のほかの6つの決定は変えない。7つの決定のうち1つの一部を変えるので、[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)の選び方でamendsにする。ADR-t1639-2は変えない（決定1の「個別の指定は外して受け皿の`low`を継がせる」を、follow_upの経路ではruntimeが満たす）。

## Alternatives

- **今のまま、inboxが人に確かめて外す**: 人のaskと待ちが移動のたびに出て、人の答えは毎回同じだった。
- **AIのactorのどの移動でも継いでいただけの値を残さない**: 判断の記録とplan reviewの検査を伴わない`set-goal`で、AIが移動だけで人の指定を下げられる穴に戻る。
- **plan reviewに移動で残った値を外す権限を与える**: 人の出どころの値をAIのactorが変えないという決定3を崩す。
- **既存の残った値もmigrationで外す**: 人の出どころの値を人に確かめずに変えることになる。

## Consequences

- plannerの`out_of_scope`の移動で、follow_upは受け皿の`low`などの移動先の優先度で走り、plan reviewが人の値で止まらない。
- 外した値はtaskの優先度の変更のeventに残り、人が後から確かめられる。
- 人が個別に付けた値は、follow_upの移動でも残る。
- 今も以前の移動で残った値を持つ開いたtaskは、外すかを人とinboxが決める。
