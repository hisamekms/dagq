---
id: design-supervisor-lifecycle-headless-job-processes
type: design
title: "Headless job processes"
status: current
created: 2026-09-27
scope: runtime
related:
  - adr-t1566-1
  - design-supervisor-lifecycle-prompt
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

task 443。supervisorが起動するheadlessのjob（runのreview、終わったrunと生きているsessionの復旧job、plan review、goal review）の`claude -p`か`codex exec --json`（`[roles.<role>] provider = "codex"`。[Actor model](actor-model.md#provider)）のプロセスをqueueに記録し、そのsupervisorが死んだ後に引き継ぐsupervisorが、同じjobを立て直す前に前のjobを止める。同じ入力のjobが二重に走ること、その費用、古いjobが出力ファイルを書くことを防ぐ。use caseは`src/application/supervise/jobs.rs`、表は`src/infrastructure/headless_jobs.rs`、判定は`src/domain/headless_job.rs`。observerの子プロセスはここに記録せず、時間切れとsupervisorのループのerrorでの終了のときに子孫ごとkillする（task 245、[Observer](observer.md)）。

## 記録

- runのreviewの段のjobは2種（`domain::headless_job::JobKind`。[ADR-t1895-1](../../adr/2026-10-06-t1895-1-review-stage-runs-agent-and-program-jobs-in-a-fixed-shape.md)決定1）: providerのheadlessのsessionの`agent`と、決まったプログラムの子processの`program`。
  復旧job・plan review・goal reviewは`agent`のjobである。
  どちらの種類も、起動の記録・時間の上限・timeoutの停止・終わりの記録・引き継ぎは下の同じ経路（`record_job`と`HeadlessJob`）を通る。
  `program`のjobは`start_program_job`が自分のprocess groupで起動し（`CommandSpec::new_session`）、stdoutとstderrをファイルに受ける。
- jobのプロセスを起動した直後に、`record_job`が`headless_jobs`（schema v43、[Persistence](../persistence.md)）に1行書く: `kind`（`review` / `recovery` / `plan_review` / `goal_review`）、`label`（復旧jobのalert）、`run_id` / `proposal_id` / `goal_id`、`attempt`、`pid`、`process_start`（`ProcessControl::start_identity`。`LC_ALL=C`の`ps -o lstart= -p <pid>`の秒までの起動時刻）、`supervisor_token`、`started_at`、`provider`（jobを起動したprovider（`JobSubject::provider`）。schema v55、task 1062。goal review（task 1065）・runのreview（ADR-t1207-1）・plan review（task 1218）・終わったrunと生きているsessionの復旧job（task 1225）は行き先で実際に起動したprovider（launchの`provider`。使えないproviderから切り替えた後ならその先）で、列の既定は`claude`なので、それより前の行と古いバイナリが書く行は`claude`と読める。値の集合はworkerのrunの`requested_provider` / `actual_provider`と同じ）。書けなくてもjobはそのまま走る（logだけ）。
  supervisorが起動したjobは`Supervisor::headless_job`から、`program`のjobは`start_program_job`から`record_job`に入る。
  `program`のjobは`kind`を`review_program`、`label`をプログラムの名前にする。
  `program`のjobはproviderが動かさないので、`provider`に`none`（`headless_job::NO_PROVIDER`）を書く。
  `kind`も`provider`もCHECKの無いTEXTなので、値を足してもmigrationは要らない。
  行はjobのstdoutの置き場（`stdout`）も持ち、止めた側がjobのExecutionを読む（下の引き継ぎ）。
  古い行と古いバイナリが書く行はnullで、そのExecutionは未計測になる。
- agentのevalのjob（[Agent eval](../agent-eval.md)）は`agent`のjobで、runのreviewの段のagentのjobと同じ時間の上限とやり直しで動き、`kind`を`agent_eval`にしてrunのreviewのjobと取り違えない。
  runを持たず、`label`がagent・周・ケース・回を示す（`headless_job::AGENT_EVAL`のdoc comment）。
  その前にケースに流すprogramは`program`のjobで、`kind`を`agent_eval_program`にし、runのreviewのprogramのjobと同じ時間の上限で動く。
- jobの時間の上限（`Supervisor::job_timeout`）: runのreviewの段のjobは種類ごとの`[review.jobs]`の値（[Run environment](run-environment.md)）、無ければ`agent`はjobを動かすproviderの`AgentProvider::review_timeout`、`program`はreviewerのそれ。
  ほかのjob（復旧job・plan review・goal review）は`[review.jobs]`を読まず、reviewerの`review_timeout`。
- jobの終わりで`ended_at`と`outcome`を書く: 自分で終わったjob（exitを読んだ）は`ended`、自分のsupervisorが止めたjob（timeout、見るのをやめたslot、handoffの前）は`stopped`。jobが終わる場所はqueueを持たないので、終わりは`JobEnds`に積み、次のpassの先頭（と、ループを抜けた直後）に書く。終わりを読む前に捨てられたjob（途中のerror、失敗したループ）は`Drop`で止めて`stopped`にするので、走ったまま見られなくなるjobは無い。
- 終わりのeventを書かずに止めるjob（handoffの前、見るのをやめたslot、ループの終わりに残ったjob、`Drop`）は`HeadlessJob::abandon`で止め、その終わりを`JobEnds`に積む。
  agentは走ったので、書くときに出力を読み、`headless_job_stopped`をExecution（[Executionのトークン数](../execution-tokens.md#記録の形)）と共に下の引き継ぎと同じ置き場に記録する。
  止めた側の書く終わりのeventがExecutionを持つjob（見張りが止めた復旧のjob、timeout）は`HeadlessJob::stop`で止め、これを書かない。
  `program`のjobはagentが動かずExecutionでないので、止めても積まず、行の`stopped`だけで終わる。
- timeoutの停止（`HeadlessJob::stop`）は、killの前に`ProcessControl::descendants`（`ps -U <uid> -o pid=,ppid=,…`の親子の連なり）でjobの子孫を集め、jobをSIGKILLしてwaitした後、子孫もSIGKILLする。`claude -p`のBashとその子がjobより長く残らない。
  jobのSIGKILLは`Spawned::kill_group`で、自分のprocess groupを持つ`program`のjobはgroupごと止まる（ADR-t1895-2決定4）。
  groupを持たない`agent`のjobはjobのprocessだけで、子孫は上の一覧で止める。
- この`ps`の一覧（`ProcessControl::list`・`descendants`・`executables`が共有する`user_ps_listing`）はstdoutをbytesで受け、`parse_ps`が`String::from_utf8_lossy`で読む（task 1794）。hostのどれか1つのprocessのcommandにUTF-8でないbyteがあっても一覧全体は失敗せず、その行もpid・ppidのまま残り（commandのそのbyteだけが置換文字になる）、子孫の連なりが切れない。行を飛ばさないのは、飛ばすとその下の子孫が停止と孤児の検出から漏れるため。
- やり直すかどうかは種類で決める（`JobKind::retries`）: `agent`のjobの非0のexitは同じ入力で1回だけやり直してよい失敗で、`program`のjobの非0のexitは検査の結果なのでやり直さない。
  timeoutはどちらもやり直さない。

## 起動と終わりのinterface

jobは[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)のinterfaceでproviderにつながる（task 1064）: 権限は意図（`JobAccess`）で渡し、`HeadlessJob::poll`はjobを起動したprovider（`HeadlessJob::provider`の`Supervisor::job_agent`: Claudeはsupervisorの`reviewer`、Codexは`codex_jobs`。task 1065）の`job_reply`でstdoutから取り出した最終の返答を返し（失敗なら理由の文）、失敗したjobの分類は`Supervisor::job_failure`（Claudeは`AgentSignals::job_failure`、Codexは`AgentProvider::job_failure`。共通の`JobFailure`）で読む。時間の上限の止め方（子孫をpidで集めてから止める）はproviderによらず同じ。
Claudeのjobは、後のpromptを予約する道具を`--disallowedTools`で拒む。
予約したwakeupは1回きりの`claude -p`を返答の後も終わらせず、jobは時間の上限で殺されるため。
一覧と理由は`src/infrastructure/adapters.rs`の`PRINT_MODE_DENIED_TOOLS`のdoc commentが持つ。

## promptの渡し方と大きさ

jobのpromptの渡し方（大きさに関係なくファイルかstdin）、載せる材料、節ごとと全体の上限、省いたことの明示、byte数の記録は、[Prompt](prompt.md#headlessのjobのprompt)の「headlessのjobのprompt」の節が正本で、jobごとの今の渡し方・節・上限をその表が持つ（[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)）。今はどのjobもpromptをstdinで渡し（task 1560。[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)の「promptの渡し方」）、promptの大きさで起動が`ARG_MAX`に当たらない。前は引数で渡し、hostの`ARG_MAX`を超えると起動できなかった。引数・envの大きさによる起動の失敗（`E2BIG`）とstdinの一時ファイルを用意できない失敗（`StdinUnprepared`）は、そのjobの失敗にだけ数えてproviderを控えない。

## 引き継ぎ

- supervisorはループの各passの先頭で（claim・adopt・triage・plan review・goal reviewがjobを立てる前に）、`orphaned_headless_jobs(token, own)`を読む。自分以外のtokenの未完了の行のうち、そのsupervisorが登録に無いか、heartbeatが`HEARTBEAT_TIMEOUT_SECS`（30秒）より古いものが対象。プロセスの起動の直後（execの後なら`rebuild_own_runs`の前）の1回だけは、自分のtokenの未完了の行も対象にする（execされたプロセスは前のbinaryのjobを知らない。handoffはexecの前にjobを止めるので、普通はもう居ない）。
- 登録が残っていてheartbeatだけが古く、そのpidが生きているsupervisor（sleepから戻った直後のhostでは全員のheartbeatが古い）の行は、そのpassでは触らない（登録の無い行と、自分と同じpidの登録（in-processのtest）はこの検査をしない）。supervisorのpidが別のプロセスに再利用されていると、その行は閉じられないまま残る。
- 行ごとに`domain::headless_job::takeover`で決める。pidが生きていなければsignalを送らずに`gone`で閉じる。生きていて、今の`start_identity`が記録した`process_start`と同じなら、そのjobのプロセスとして止める。違う（pidが別のプロセスに再利用された）か、どちらかの起動時刻が読めなければ、signalを送らずに`not_the_job`で閉じる。
- 止める前に行を`taken_over`で閉じる（`end_headless_job`が閉じたときだけ進むので、同時に引き継ぐ2つのsupervisorの片方だけがsignalを送る）。止め方（`stop_tree`）: 子孫とそれぞれの起動時刻を集め、jobと子孫にSIGTERMを送り、5秒（`TAKEOVER_GRACE`）まで消えるのを待ち（自分の子なら`reap`する）、残ったものにSIGKILLを送る。どちらのsignalも、そのpidの起動時刻が集めたときと同じときだけ送る（待つ間にpidが再利用されても触らない）。待ちはループの中で行うので、1行ごとに最大5秒passが止まる。`headless_job_stopped`（`headless_job_id`、`kind`、`label`、`run_id`、`proposal_id`、`goal_id`、`attempt`、`pid`、`process_start`、`descendants`、`killed`、`supervisor`、`started_at`、`agent`のjobは`provider`とExecution）を、runのjobならそのrunに、plan review / goal reviewならqueueのevent（`EventKind::is_queue`）に記録する（記録の失敗はlogだけ）。
- `agent`のjobの`headless_job_stopped`は、止めた後に行の`stdout`を行の`provider`で読んだExecutionを持つ（`Supervisor::taken_over_execution`。[Executionのトークン数](../execution-tokens.md#記録の形)）。
  読めなければ未計測で、同じjobの終わりのeventが既にExecutionを持てば足さない。
  `gone`と`not_the_job`も、行を閉じた後に同じくExecutionを読み、記録するものがあるときだけ止めずに`headless_job_stopped`（`descendants`と`killed`は空）を書く（`close_taken_over`）。
- 判断は`kind`と`provider`を見ないので、`program`のjob（`review_program`）も`agent`のjobと同じ判断で止め、`headless_job_stopped`の`kind`と`label`がその種類とプログラムを示す。
- evalのjob（`agent_eval`・`agent_eval_program`）も同じ判断で止め、runを持たないので`headless_job_stopped`をqueueのeventに記録する。
  止めた後は、そのsupervisorが周をeventから読み直し、終わっていない実行を起動し直す（[Agent eval](../agent-eval.md#evalが本番と共有する起動経路)）。
- その後で、adoptしたrunのreviewや復旧job、`begin_plan_review` / `begin_goal_review`が`interrupted`にした行のやり直しが新しいjobを立てるので、同じ入力のjobは1つだけが走る。

## test

`tests/it/runtime_headless_jobs.rs`: 死んだsupervisorのreviewの途中のrun（終わらないstubのjobとその子を`headless_jobs`に記録）を別のsupervisorがadoptすると、自分のreviewを立てる前に前のjobとその子を止めて`headless_job_stopped`を残し、reviewは1回だけ走って着地すること、pidが別のプロセスになった行はsignalを送らずに`not_the_job`、プロセスの無い行は`gone`で閉じて、どちらもagentのjobはExecutionを持つ`headless_job_stopped`を残し、heartbeatが古くてもpidの生きているsupervisorの行は触らないこと。timeoutしたreviewのbackgroundの子が残らず、行が`stopped`になること。表の読み書きは`src/infrastructure/headless_jobs.rs`、判定と子孫の辿り方は`src/domain/headless_job.rs`、起動時刻の読み方は`src/infrastructure/adapters.rs`のunit test。
timeoutしたreviewの上限は`[review.jobs] agent_timeout_secs`から読む（providerの上限より短く置く）。
死んだsupervisorの`review_program`の行も、同じadoptで`agent`のjobと同じく止めること。
`program`のjobが`review_program`・プログラムの名前・`provider`の`none`で記録され、終われば`ended`、timeoutなら子孫から外れた孫も含めてprocess groupごと止まって`stopped`になること。
