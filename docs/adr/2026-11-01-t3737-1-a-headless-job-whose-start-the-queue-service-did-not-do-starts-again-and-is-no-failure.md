---
id: adr-t3737-1
type: adr
title: headless jobの起動の途中でqueue serviceが要求をしなかった（undone）ための失敗はjobの失敗にせず、人に回る記録を書かず、起動し直せる状態のまま残して次の周回か次のsupervisorが起動し直す（ADR-0047決定2・17・43をamends、ADR-t3102-3決定5の延長）
status: accepted
created: 2026-11-01
updated: 2026-11-01
accepted_on: 2026-11-01
amends:
  - adr-0047 decision 2
  - adr-0047 decision 17
  - adr-0047 decision 43
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - queue-service
related:
  - adr-t3102-3
  - adr-0041
  - adr-0047
  - adr-0048
  - design-supervisor-lifecycle-plan-review
  - design-supervisor-lifecycle-goal-review
---

# ADR-t3737-1: queue serviceが起動の要求をしなかったheadless jobは失敗にせず起動し直す（ADR-0047決定2・17・43をamends）

## Context

plan reviewとgoal reviewのjobは、queueに行を作って（`begin_plan_review`・`begin_goal_review`）から、jobのdirectoryとpromptを用意し（promptの材料をqueueから読む）、providerの処理を起動する。今は行を作った後の起動のどこで失敗しても、原因を問わずjobの失敗（`plan_review_failed`・`goal_review_failed`）にし、届かなければkeptの記録に残して次のsupervisorが書く。失敗したproposalは保留（plan review by hand）で人を待つ。

2026-10-29、queue DBの書き込みの詰まりでheartbeatが失敗して終わりの期限を過ぎたsupervisorは、plan reviewの準備の読み（`events_between`）をqueue serviceに送らず（serviceに届かない、要求をしていない）、それだけでproposalのplan reviewが失敗になり人を待った。jobの中身にもproviderにも失敗は無く、やり直せば済む。

[ADR-t3102-3](2026-10-22-t3102-3-stage-5-outage-notice-and-use-cases-for-multi-step-control-operations.md)決定5は、serviceに届かない間は制御側がjobの起動を始めず、DBを直接開かないとしている。起動を始めた後に届かなくなったときの扱いは決めていない。

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)は、決定2でjobが起動できないことをheadless jobの失敗に数えて対象を動かさずinboxのattentionかaskにし、決定17で失敗したplan reviewを`plan_review_failed`、決定43で失敗したgoal reviewを`goal_review_failed`のattentionにする。ADR-0047は番号付きの決定を多く持ち、変えるのはその3つの決定の起動の失敗の範囲だけなので、amendsにする。serviceが要求をしていないことは、`ServiceUnreachable`の`maybe_done`が偽であること（`supervisor_service::undone`）で分かる。読みは何も変えないので、答えの来ない読みもundoneに当たる。

## Decision

1. **undoneの起動の失敗はjobの失敗にしない。** headless jobの起動の途中（行を作った後の準備とproviderの起動）でqueue serviceが要求をしなかった（undone）ための失敗は、jobの失敗にせず、人に回る記録（`*_failed`、jobの失敗のask）を書かず、keptの記録にも残さない。logに残し、jobを起動し直せる状態のまま残す。
2. **起動し直しは自分の次の周回か次のsupervisorが行う。** 残したjobは、同じsupervisorの次の周回か、そのsupervisorが居なくなった後の次のsupervisorが、同じ対象のjobとして起動し直す。やり直しは失敗の回に数えない。残したjobのtelemetryのtokenと用意したdirectoryは、残すときに後始末する。
3. **maybe_doneとserviceに届いた失敗は今のとおり。** serviceがしたかもしれない要求（`maybe_done`）、serviceが拒んだ要求、jobの用意の失敗（promptが作れないなど）、providerが起動できない失敗は、これまでどおりjobの失敗として扱う。maybe_doneは要求が行われたかもしれないので、undoneと同じに扱わない。
4. **ADR-t3102-3決定5を保つ。** serviceに届かない間は新しい起動を始めずDBを直接開かない、という決定5はそのままで、この決定はその延長として、起動の途中で届かなくなったjobを始めなかったjobと同じに扱う。
5. **ADR-0047決定2・17・43の起動の失敗から除く。** 決定2の「起動できない」失敗、決定17の`plan_review_failed`、決定43の`goal_review_failed`は、6で当てはめたjobについて、1のundoneの起動の失敗を含まない。それ以外の起動の失敗と、起動した後の失敗の扱いは変えない。
6. **当てはめる範囲。** plan reviewとgoal reviewに当てはめる。行は`finished`でないまま残し、次の`begin_*_review`が自分のtokenの行も持ち主の死んだ行も`interrupted`で閉じて同じ候補のreviewを始め、attemptは`interrupted`を数えない。runの側のjob（runのreview・満たし済みのreview・復旧job・triage・再計画の診断）は状態の持ち方が違うので、同じgoalの後続のtaskが同じ決定を当てはめる。

## Alternatives

- **今のまま失敗にして人を待つ**: jobにもproviderにも失敗が無いのに人の手が要り、queueの一時の詰まりのたびに人が保留を解く。
- **undoneの起動の失敗を回数に数えてやり直す**: やり直しが失敗の上限を使い、詰まりが続くと本当の失敗と見分けられない。
- **maybe_doneもundoneと同じにやり直す**: serviceが要求をしたかもしれず、同じ起動が2度記録されうる。

## Consequences

- queueの一時の詰まりやsupervisorの終わりの期限の超過で起動できなかったplan review・goal reviewは、人を待たずに次の周回か次のsupervisorが起動し直す。
- serviceに届かない状態が続くと、起動の試みごとにjobの行と開始のevent（閉じた行は`interrupted`）とwarnのlogが周回ごとに増える。やり直しの間隔は周回のままで、人に回る記録は増えない。
- 残した行は次の起動か引き継ぎが閉じるので、その前に対象が候補でなくなり他の候補も無いと、行は開いたまま残る。
- 分ける判定は`supervise::planning::JobStart::of`、残し方は`leave_for_restart`のdoc commentが持つ。
