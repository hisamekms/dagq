---
id: adr-t2946-1
type: adr
title: 人の明示のsend_backで差し戻したrunは、衝突だけの試行が上限に達していても、数える試行が残っていればresumeする（ADR-0047決定24をamends）
status: accepted
created: 2026-10-22
updated: 2026-10-22
accepted_on: 2026-10-22
amends:
  - adr-0047 decision 24
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
related:
  - adr-0047
  - adr-t946-1
  - adr-0068
  - design-supervisor-lifecycle-needs-session
---

# ADR-t2946-1: 人の明示のsend_backで差し戻したrunは、衝突だけの試行が上限に達していても、数える試行が残っていればresumeする（ADR-0047決定24をamends）

## Context

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定24は、`needs_session`のrunのresumeを数える試行の上限で止め、reviewがpass済みで理由がrebaseの衝突だけの試行は数えずに別の柵（衝突だけの試行の上限）で止めると決めた。衝突だけの柵は、同じ衝突を解けないまま回り続けないためのものである。

ところが、衝突だけの試行が上限に達したrunは、parkの理由に依らず試行を使い切ったものとして扱われていた。run efa84a8f（ask 1016）は着地の衝突の解消で衝突だけの試行を上限まで使った後、人が`approve_landing`に`send_back`と答えて`needs_session`になり、数える試行は1回しか使っていなかったのに、直後に試行を使い切ったものとして失敗した。人の差し戻しの理由は一度もworkerに届かなかった。人の明示の差し戻しは同じ衝突の繰り返しではなく、柵が止めるものではない。

## Decision

1. **人の明示の`send_back`で`needs_session`になったrunは、衝突だけの試行が上限に達していても、数える試行が残っていればresumeする。** `approve_landing`への`send_back`（理由なしも理由付きも）でparkしたrunの次のresumeだけが対象で、そのresumeは今どおり数える試行に数える。
2. **変えない境界。** 次は今のままにする。
   - 数える試行の上限とその数え方。人の`send_back`の後のresumeは数える試行に数え、数える試行が上限に達していれば人の`send_back`でも試行を使い切ったものとして扱う。
   - killだけの試行の上限（[ADR-t946-1](2026-09-29-t946-1-kill-only-resumes-have-their-own-limit.md)）。
   - 衝突でparkしたrun（parkの理由がrebaseの衝突）が衝突だけの上限に達したときの使い切りと、引き継ぐretry。人の`send_back`の後のresumeが再び衝突や別の理由でparkしたなら、衝突だけの上限は今どおりそのrunを止める。

決定24のそれ以外（衝突だけの試行を数えないこと、adopt、復旧jobに回すこと）は変えない。

## Alternatives

- **人の`send_back`の後のresumeを数える試行に数えない**: 人が何度も差し戻すrunが数える試行の柵の外で回り続ける。差し戻しの指摘への対応はrunの仕事で、数えるのが正しい。
- **`send_back`で衝突だけの試行の数を0に戻す**: 解けない衝突がsend_backのたびに柵の全量をもう一度使える。数は履歴から読むもので、答えで書き換えない。

## Consequences

- 衝突の解消で衝突だけの試行を使い切ったrunでも、人の差し戻しの理由がworkerに届く。
- 人の`send_back`の直後のresumeの後にrunが再びparkすれば、衝突だけの上限で今どおり使い切りになり、引き継ぐretryか復旧jobに進む。
