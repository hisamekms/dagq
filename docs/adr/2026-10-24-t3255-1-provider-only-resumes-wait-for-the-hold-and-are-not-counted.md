---
id: adr-t3255-1
type: adr
title: providerの控えの間は衝突だけのresumeを始めず、providerの失敗だけで修理なしに終わったresumeは数える試行にも衝突だけの試行にも数えず別の上限で止める（ADR-0047決定24をamends）
status: accepted
created: 2026-10-24
updated: 2026-10-24
accepted_on: 2026-10-24
amends:
  - adr-0047 decision 24
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - provider
related:
  - adr-0047
  - adr-t946-1
  - adr-t2664-1
  - adr-t2946-1
  - adr-t1857-1
  - design-supervisor-lifecycle-needs-session
---

# ADR-t3255-1: providerの控えの間は衝突だけのresumeを始めず、providerの失敗だけで修理なしに終わったresumeは数える試行にも衝突だけの試行にも数えず別の上限で止める（ADR-0047決定24をamends）

## Context

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定24は、sessionを待つrunのresumeを数える試行の上限で止め、reviewがpass済みの衝突だけの試行を別の上限（衝突だけの上限）で止める。runが起こしていない終わりで試行を使わないための例外として、外からのkillの後の試行（[ADR-t946-1](2026-09-29-t946-1-kill-only-resumes-have-their-own-limit.md)）とholdの解除の後の試行（[ADR-t2664-1](2026-10-22-t2664-1-resumes-after-a-hold-release-are-not-counted.md)）がある。

finding 474のrunは、reviewのpassの後のlanding recheckの衝突で衝突だけのresumeを繰り返し、衝突だけの上限を修理なしで使い切ってretryに回った。そのうち1回のresumeは、turnがClaudeの週のusage limitで失敗し、`[provider_fallback] workers = false`（[ADR-t1857-1](2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）のため自分のproviderを待つ間にhostのsleepでwrapperのheartbeatが切れて未解消で終わった。次のresumeはproviderの控えが解ける前に始まり、何もできないまま同じ終わり方をした。どちらもrunが起こした失敗ではなく、衝突を解く機会でもなかった。

## Decision

1. **runのproviderに控えが掛かっている間、supervisorは衝突だけのresume（とprecheckの依頼）を始めず、控えが解けるか、runがもう一方のproviderに移れるまで待つ。** 移れるのはADR-t1857-1の設定で切り替えが許され、もう一方が使えるとき。待ちは記録し、試行に数えない。ADR-t1857-1の決定2（切り替えを止めたときは控えが解けるのを待って同じproviderで続ける）は保ち、この決定はその待ちをresumeの開始に当てる。
2. **resumeのsessionのturnがproviderの失敗（控えの理由になるusage limit・loginなど）だけで終わり、そのresumeで修理（新しいcommitかreceiptの書き直し）が無いまま終わった試行を「providerだけの試行」とし、数える試行にも衝突だけの試行にも数えない。** turnの失敗の後にsessionがどう終わったか（heartbeatが切れた後の`/exit`、wrapperの沈黙、自分で終わる）には依らない。判定はADR-t946-1・ADR-t2664-1と同じくrunのイベントだけから行い、resumeの記録に数えなかったことを残し、数えなかったことを自動修正として記録する。
3. **無限に回らない条件。** (i) providerだけの試行の後の次のresumeは、決定1のとおり控えが解けた後（か移れるとき）にだけ始める。(ii) providerだけの試行はrunごとに別の上限で止め、上限に達したrunは決定24の「使い切ったとき」に従う。上限は設定にせずruntimeの定数にする。
4. **providerだけの試行の後に別の理由でparkしたら、その理由で数える**（ADR-t946-1決定3・ADR-t2664-1決定4と同じ）。

ADR-0047決定24のそれ以外と、ADR-t946-1・ADR-t2664-1・ADR-t2946-1の決定は変えない。

## Alternatives

- **待ちの間もwrapperにheartbeatを書き続けさせる**: 待ちの間のheartbeatはすでに書かれており、切れた原因はhostのsleepだった。直しても控えの間に始まったresumeは何もできず、試行を使う。
- **控えの間のresumeも数えたままにし、衝突だけの上限を上げる**: 上限は同じ衝突が解けないまま回り続けないための柵で、providerの控えの長さに合わせて上げると柵の意味を失う。
- **providerだけの試行に上限を設けない**: 控えの解除と失敗を繰り返すproviderで、人の目に触れずに回り続ける。holdの解除（ADR-t2664-1）と違い、控えの解除は人の操作に限らない。

## Consequences

- providerの控えでrunの試行が減らず、控えの間に何もできないresumeを始めない。
- providerだけの試行の数と上限は、resumeの記録・使い切ったrunの文面・復旧jobのpromptに数える試行と分けて出る（記録の形と上限の値はコードのdoc comment、流れは[Needs session](../design/supervisor-lifecycle/needs-session.md)が持つ）。
- 衝突だけのresume以外のresume（検証の失敗など）は、providerだけの試行の後でなければ控えの間も今どおり始まり、sessionの中でproviderを待つ。
