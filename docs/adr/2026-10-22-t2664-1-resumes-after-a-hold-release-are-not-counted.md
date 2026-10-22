---
id: adr-t2664-1
type: adr
title: holdでsessionを残して待ったrunの解除の後のresumeは数える試行の上限に数えず、数える試行を使い切ったrunでもresumeする（ADR-0047決定24をamends）
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
  - hold
related:
  - adr-0047
  - adr-t946-1
  - adr-t1879-1
  - design-supervisor-lifecycle-needs-session
---

# ADR-t2664-1: holdでsessionを残して待ったrunの解除の後のresumeは数える試行の上限に数えず、数える試行を使い切ったrunでもresumeする（ADR-0047決定24をamends）

## Context

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定24は、sessionを待つrunのresumeをrunごとに通算の数える試行の上限で止め（「試行の上限」）、使い切ったrunを引き継ぐretryか復旧jobに回す（「使い切ったとき」）。例外は、reviewがpass済みの衝突だけの試行（決定24）と、外からのkillの後の試行（[ADR-t946-1](2026-09-29-t946-1-kill-only-resumes-have-their-own-limit.md)）で、どちらもrunが起こしていないものでrunの試行を使わないための別の数え方と別の上限を持つ。

[ADR-t1879-1](2026-10-07-t1879-1-hold-a-task-at-the-turn-boundary-and-release-it-explicitly.md)のholdは、走っているrunをturnの境界で止め、sessionを残した待ちに置き、解除の後は今のresumeの経路で同じsessionから続ける。ところがこのresumeも数える試行の1つに数えていたので、holdされたrunは自分の失敗でない保留で試行を1つ失い、既に数える試行を使い切っていたrunは解除してもresumeされず、引き継ぐretryか復旧jobに回った。これは「sessionがあればresume」というholdの解除の約束（goal 150の受け入れ条件）に反する。

## Decision

1. **holdの解除の後のresumeは、数える試行の上限に数えない。** runをsessionの待ちにした最新のparkがholdの待ちなら、解除の後の次のresumeはholdのresumeとして数える試行と別に扱い、runtimeは数えなかったことを自動修正として記録する。判定はほかの例外と同じくrunのイベントだけから行う。
2. **数える試行を使い切ったrunでも、holdの解除の後はresumeする。** holdの待ちにあるrunは試行を使い切ったものとして扱わず、「使い切ったとき」の引き継ぐretryにも復旧jobにも回さない。数える試行の残りはholdの前と同じままにする。
3. **holdのresumeに別の上限を設けない。** holdは人（かinbox）の明示の操作で、holdのresumeは解除ごとに1回しか起きない。runの失敗の繰り返しではないので、人の目に触れずに回り続けることがない。
4. **holdのresumeの後に別の理由でparkしたら、その理由で数える。** holdのresumeはholdの待ちの後の最初の1回だけで、そのsessionが検証の失敗などでparkしたなら、次のresumeはその理由で数える（reviewがpass済みの衝突なら衝突だけの試行）。holdの待ちの後、resumeの前に別の理由でparkし直したrunも、その理由で数える。

決定24のそれ以外と、再計画の保留の後のresumeの数え方は変えない。

## killの例外と分ける理由

ADR-t946-1のkillの後のresumeも数えないが、killだけの試行は別の上限で止める。killは外の原因（メモリの不足、hostの再起動）で繰り返し起こりうるうえ、復旧jobのverdictで自動に回るので、柵が無いとrunが回り続ける。holdの解除は人の操作でしか起きず、回数は人の操作の回数を超えないので、同じ柵は要らない。またkillの上限を共有すると、killで止まったことのあるrunのholdの解除がresumeされず、決定2の約束を破る。

## Alternatives

- **数えたままにする**: 試行を使い切ったrunが解除でresumeされず、引き継ぐretryか復旧jobに回る。人がsessionを残すために選んだholdが、その目的を果たさない。
- **killだけの試行と同じく別の上限で止める**: 上限に達したrunの解除が同じ問題を起こし、上限が守るもの（人の目に触れない繰り返し）がholdには無い。

## Consequences

- holdは解除のたびにrunの試行を使わず、使い切ったrunもsessionのまま続く。
- resumeの記録、使い切ったrunの文面と復旧jobのpromptが、数える試行とholdのresumeを分けて示す（記録の形は[Needs session](../design/supervisor-lifecycle/needs-session.md)とコードのdoc commentが持つ）。
