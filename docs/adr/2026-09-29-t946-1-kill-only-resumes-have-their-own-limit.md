---
id: adr-t946-1
type: adr
title: 外からのkill（session_killed）で止まったsessionのresumeは試行3回の上限に数えず、killだけの試行の別の上限で止める（ADR-0047決定24をamends）
status: accepted
created: 2026-09-29
updated: 2026-09-29
accepted_on: 2026-09-29
amends:
  - adr-0047 decision 24
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
related:
  - adr-0047
  - adr-0079
  - adr-t598-1
  - design-supervisor-lifecycle-needs-session
---

# ADR-t946-1: 外からのkill（session_killed）で止まったsessionのresumeは試行3回の上限に数えず、killだけの試行の別の上限で止める（ADR-0047決定24をamends）

## Context

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定24は、`needs_session`のrunのresumeをrunごとに通算3回（`MAX_RESUME_ATTEMPTS`）で止め、triageや復旧jobからの`resume`も数えると決めた。例外はreviewがpass済みで理由がrebaseの衝突だけの試行で、これは別の柵（`[resume] conflict_only_limit`）で止める。

observerのfinding 3（`resume_exhausted`を7回）で、task 310のrun af773e13の3回のresumeのうち1回は、外からのSIGTERM（exit 143、`session_killed`）で失敗したrunを復旧jobがresumeしたものだった。killはhostやほかのプロセスが起こしたもので、runが起こした失敗ではない。それでも3回の1つに数えられ、runは本当の失敗（検証の失敗など）に使える試行を1つ失って`resume_exhausted`になった。衝突だけの試行を数えないのと同じ理由（runが起こしていないものでrunの試行を使わない）が当てはまる。

## Decision

1. **外からのkillの後のresumeは、試行3回に数えない。** sessionがsignalで終わって（`session_killed`）runが失敗し、復旧job（か人の`decide`の`resume`）がそのrunを`needs_session`に戻したとき、次の`resume_started`は数える試行ではなく**killだけの試行**とし、`counted: false`で記録する。判定は衝突だけの試行と同じく、runのイベントだけから行う: その`resume`より前のrunの`last_error`がkillであること。runtimeはこれを自動修正として記録する。
2. **killだけの試行は別の柵で止める。** 何度もkillされ続けるrunが回り続けないよう、killだけの試行はrunごとに別の上限で止め、上限に達したrunは試行を使い切ったもの（`resume_exhausted`。決定24の「使い切ったとき」）として扱う。上限の値は設定にせず、runtimeの定数にする（値は[Needs session](../design/supervisor-lifecycle/needs-session.md)に書く）。
3. **killの後のresumeの後に別の理由でparkしたら、その理由で数える。** killだけの試行はkillの後の`resume`の直後の1回だけで、そのsessionが解消せずに次のresumeに回ったなら、間に新しいkillが無い限り数える試行にする。数えるかどうかはresumeの直前の`needs_session`の理由で決まり、killの後のresumeが検証の失敗や`evidence_missing`でparkしたなら次のresumeは数える試行、reviewがpass済みの衝突なら衝突だけの試行になる。

決定24のそれ以外（衝突だけの試行、引き継ぐretry、adopt、復旧jobに回すこと）は変えない。killの後のresumeは同じsessionの段を保つ（[ADR-0079](0079-record-task-weight-predictions-and-trial-model-effort-selection.md)決定5のまま）。

## Alternatives

- **killの後は復旧jobを経ずにruntimeが自動でresumeする**: 復旧jobは画面や記録からkillの原因（人が止めた、hostの再起動など）を読んで、resumeしない判断もできる。数え方だけを変え、判断は今までどおり復旧jobに置く。
- **killだけの試行を上限なしにする**: killし続ける原因（メモリの不足など）があると、runが人の目に触れずに回り続ける。
- **上限を設定にする**: 衝突の柵と違い、値を変えたい運用上の理由が今は無い。必要になったら設定に足す。

## Consequences

- 外からのkillで試行を失って`resume_exhausted`になるrunが減り、復旧jobとinboxに回るものが減る。
- `resume_started`の`counted`、`ResumeCount`、`resume_exhausted`と使い切ったrunのaskや復旧jobのpromptの文面にkillだけの試行の数が加わる。
