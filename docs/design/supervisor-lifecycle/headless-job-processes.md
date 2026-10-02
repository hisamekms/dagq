---
id: design-supervisor-lifecycle-headless-job-processes
type: design
title: "Headless job processes"
status: current
created: 2026-09-27
updated: 2026-10-02
last_verified: 2026-10-02
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-handoff
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-triage
  - design-supervisor-lifecycle-background-recovery-job
  - design-supervisor-lifecycle-plan-review
  - design-supervisor-lifecycle-goal-review
  - design-persistence
---

# Headless job processes

task 443。supervisorが起動するheadlessのjob（runのreview、終わったrunと生きているsessionの復旧job、plan review、goal review）の`claude -p`のプロセスをqueueに記録し、そのsupervisorが死んだ後に引き継ぐsupervisorが、同じjobを立て直す前に前のjobを止める。同じ入力のjobが二重に走ること、その費用、古いjobが出力ファイルを書くことを防ぐ。use caseは`src/application/supervise/jobs.rs`、表は`src/infrastructure/headless_jobs.rs`、判定は`src/domain/headless_job.rs`。observerの子プロセスはここに記録せず、時間切れとsupervisorのループのerrorでの終了のときに子孫ごとkillする（task 245、[Observer](observer.md)）。

## 記録

- jobのプロセスを起動した直後に、`Supervisor::headless_job`が`headless_jobs`（schema v43、[Persistence](../persistence.md)）に1行書く: `kind`（`review` / `recovery` / `plan_review` / `goal_review`）、`label`（復旧jobのalert）、`run_id` / `proposal_id` / `goal_id`、`attempt`、`pid`、`process_start`（`ProcessControl::start_identity`。`LC_ALL=C`の`ps -o lstart= -p <pid>`の秒までの起動時刻）、`supervisor_token`、`started_at`、`provider`（jobを起動したprovider（`JobSubject::provider`）。schema v55、task 1062。goal review（task 1065）・runのreview（ADR-t1207-1）・plan review（task 1218）は行き先で実際に起動したprovider（launchの`provider`。使えないproviderから切り替えた後ならその先）、他のjob（復旧）は`claude`（`domain::actor_model::ROLE_PROVIDER`）で、列の既定も`claude`なので、それより前の行と古いバイナリが書く行は`claude`と読める。値の集合はworkerのrunの`requested_provider` / `actual_provider`と同じ）。書けなくてもjobはそのまま走る（logだけ）。
- jobの終わりで`ended_at`と`outcome`を書く: 自分で終わったjob（exitを読んだ）は`ended`、自分のsupervisorが止めたjob（timeout、見るのをやめたslot、handoffの前）は`stopped`。jobが終わる場所はqueueを持たないので、終わりは`JobEnds`に積み、次のpassの先頭（と、ループを抜けた直後）に書く。終わりを読む前に捨てられたjob（途中のerror、失敗したループ）は`Drop`で止めて`stopped`にするので、走ったまま見られなくなるjobは無い。
- timeoutの停止（`HeadlessJob::stop`）は、killの前に`ProcessControl::descendants`（`ps -U <uid> -o pid=,ppid=,…`の親子の連なり）でjobの子孫を集め、jobをSIGKILLしてwaitした後、子孫もSIGKILLする。`claude -p`のBashとその子がjobより長く残らない。

## 起動と終わりのinterface

jobは[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)のinterfaceでproviderにつながる（task 1064）: 権限は意図（`JobAccess`）で渡し、`HeadlessJob::poll`はjobを起動したprovider（`HeadlessJob::provider`の`Supervisor::job_agent`: Claudeはsupervisorの`reviewer`、Codexは`codex_jobs`。task 1065）の`job_reply`でstdoutから取り出した最終の返答を返し（失敗なら理由の文）、失敗したjobの分類は`Supervisor::job_failure`（Claudeは`AgentSignals::job_failure`、Codexは`AgentProvider::job_failure`。共通の`JobFailure`）で読む。時間の上限の止め方（子孫をpidで集めてから止める）はproviderによらず同じ。

## 引き継ぎ

- supervisorはループの各passの先頭で（claim・adopt・triage・plan review・goal reviewがjobを立てる前に）、`orphaned_headless_jobs(token, own)`を読む。自分以外のtokenの未完了の行のうち、そのsupervisorが登録に無いか、heartbeatが`HEARTBEAT_TIMEOUT_SECS`（30秒）より古いものが対象。プロセスの起動の直後（execの後なら`rebuild_own_runs`の前）の1回だけは、自分のtokenの未完了の行も対象にする（execされたプロセスは前のbinaryのjobを知らない。handoffはexecの前にjobを止めるので、普通はもう居ない）。
- 登録が残っていてheartbeatだけが古く、そのpidが生きているsupervisor（sleepから戻った直後のhostでは全員のheartbeatが古い）の行は、そのpassでは触らない（登録の無い行と、自分と同じpidの登録（in-processのtest）はこの検査をしない）。supervisorのpidが別のプロセスに再利用されていると、その行は閉じられないまま残る。
- 行ごとに`domain::headless_job::takeover`で決める。pidが生きていなければ`gone`で閉じるだけ。生きていて、今の`start_identity`が記録した`process_start`と同じなら、そのjobのプロセスとして止める。違う（pidが別のプロセスに再利用された）か、どちらかの起動時刻が読めなければ、signalを送らずに`not_the_job`で閉じる。
- 止める前に行を`taken_over`で閉じる（`end_headless_job`が閉じたときだけ進むので、同時に引き継ぐ2つのsupervisorの片方だけがsignalを送る）。止め方（`stop_tree`）: 子孫とそれぞれの起動時刻を集め、jobと子孫にSIGTERMを送り、5秒（`TAKEOVER_GRACE`）まで消えるのを待ち（自分の子なら`reap`する）、残ったものにSIGKILLを送る。どちらのsignalも、そのpidの起動時刻が集めたときと同じときだけ送る（待つ間にpidが再利用されても触らない）。待ちはループの中で行うので、1行ごとに最大5秒passが止まる。`headless_job_stopped`（`headless_job_id`、`kind`、`label`、`run_id`、`proposal_id`、`goal_id`、`attempt`、`pid`、`process_start`、`descendants`、`killed`、`supervisor`、`started_at`）を、runのjobならそのrunに、plan review / goal reviewならqueueのevent（`EventKind::is_queue`）に記録する（記録の失敗はlogだけ）。
- その後で、adoptしたrunのreviewや復旧job、`begin_plan_review` / `begin_goal_review`が`interrupted`にした行のやり直しが新しいjobを立てるので、同じ入力のjobは1つだけが走る。

## test

`tests/it/runtime_headless_jobs.rs`: 死んだsupervisorのreviewの途中のrun（終わらないstubのjobとその子を`headless_jobs`に記録）を別のsupervisorがadoptすると、自分のreviewを立てる前に前のjobとその子を止めて`headless_job_stopped`を残し、reviewは1回だけ走って着地すること、pidが別のプロセスになった行はsignalを送らずに`not_the_job`、プロセスの無い行は`gone`で閉じ、heartbeatが古くてもpidの生きているsupervisorの行は触らないこと。timeoutしたreviewのbackgroundの子が残らず、行が`stopped`になること。表の読み書きは`src/infrastructure/headless_jobs.rs`、判定と子孫の辿り方は`src/domain/headless_job.rs`、起動時刻の読み方は`src/infrastructure/adapters.rs`のunit test。
