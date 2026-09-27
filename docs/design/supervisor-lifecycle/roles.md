---
id: design-supervisor-lifecycle-roles
type: design
title: "Roles"
status: current
created: 2026-09-26
updated: 2026-09-27
last_verified: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0044
  - adr-t728-1
---

# Roles

- **supervisor**: runtimeの`supervise`プロセス。taskをclaimし、runごとにworktreeとworkspaceを作って監視し、receiptを検証する。`up`がlaunchdのLaunchAgentとして常駐させるか（既定）、`--in-cmux`なら`[<repo>]supervisor` workspaceの中で動かす。
- **worker**: run session。runごとのcmux workspace `[<repo>]worker#<task-id> - <task title>`（descriptionは`dagq role=worker queue=<queue hash> run=<run-id> task=<id>`）で動くClaude session。`needs_session`のrunをresumeするworkspaceも同じtitleで、descriptionは`run <run-id> resume`。
- **planner**: goal / taskを書いてproposalとしてsubmitするsession。proposalごとのオンデマンドのworkspaceで、常駐しない（[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定1・6）。人が`dagq plan`で開くもの（何度打っても新しいworkspaceが開き、複数同時に開ける）と、runtimeが立てるもの（差し戻し先のplannerが閉じていたproposalなど）がある。workspaceは`[<repo>]planner#<planner-id>`（`DAGQ_ROLE=planner`）で、session wrapper `planner-session`がClaude sessionを`planner_prompt`（runtimeが立てたものは`runtime_planner_prompt`）付きで起動する（[`plan` / `planners`](plan-planners.md#plan--planners)）。人に頼まれれば`up` / `down`も打つ。
- **inbox**: 人に届くものすべての窓口になるsession。openなask（worker・supervisor・job・observerの質問）を人に見せてanswerを書き戻し、それ以外のattentionを人に知らせ、人の指示があるときだけ`dagq-recover` skillの手順（`up` / `down`、手でのreviewと`integrate`、`recover`、runのworkspaceへのキーと`/exit`）を実行する。唯一の常駐sessionで、`up`が`[<repo>]inbox`のworkspace（`DAGQ_ROLE=inbox`）に、`inbox_prompt`付きで起動する（[Session prompts](session-prompts.md#session-prompts)）。
- **observer**: supervisorのtimerが起動するheadlessのjob（`DAGQ_ROLE=observer`、workspaceは持たない）。stats・note・openなask・graphを読み、note・`blocked`のask・draftのgoalだけを書く（[Observer](observer.md#observer)）。

役割はこの5つ（ADR-0044の決定1。ADR-0024の決定1を引き継ぐ）で、review・recovery（triage）・plan review・goal reviewのjobはsupervisorが起動するheadlessのjob（workspaceは持たない）。runtimeの中で人が打つ`/exit`や復旧は「人（person）」と書き、inboxかplannerのsessionから打つ（`src/`に`operator`も退役した役割名も残らない）。

## Actors

runtimeはactorを型で表す（[ADR-t728-1](../../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)の決定2・4。`src/domain/actor.rs`）。

- `ActorRole`: `user`・`inbox`・`planner`・`worker`・`review-job`・`recovery-job`・`plan-review-job`・`goal-review-job`・`observer`・`supervisor`・`wrapper`・`integrator`（`desk`はgoal 48のtask 504が作るときに足す）。workspaceを持つ役割の`SessionRole`（`supervisor`・`worker`・`planner`・`inbox`・`observer`）は`SessionRole::actor_role`で`ActorRole`に写る。
- `TrustLevel`: roleだけから決まる。`user`は`Human`、`supervisor`・`wrapper`・`integrator`は`TrustedControlPlane`、それ以外（AI actor）は`UntrustedAgent`。promptや名前から推し量らない。
- `ActorContext`: `actor_id`・`role`・`trust`・`run_id`・`task_id`。

### 環境変数

runtimeが起動するAI actorは全て、環境にroleとactor idを持つ。

| actor | 起動するところ | `DAGQ_ROLE` | `DAGQ_ACTOR_ID` | ほか |
| --- | --- | --- | --- | --- |
| worker（resumeも） | runのworkspace | `worker` | `worker:<run id>` | `DAGQ_RUN_ID`・`DAGQ_TASK_ID` |
| planner（人・runtime・draft・finding） | plannerのworkspace | `planner` | `planner:<planner id>` | |
| inbox | `up` | `inbox` | `inbox` | |
| supervisor（in-cmux） | `up --in-cmux` | `supervisor` | `supervisor` | |
| review job | runのreview | `review-job` | `review-job:<run id>:<attempt>` | |
| recovery job | 終わったrunと生きているrunの復旧 | `recovery-job` | `recovery-job:<run id>:<alert>:<attempt>` | |
| plan review job | proposalのplan review | `plan-review-job` | `plan-review-job:<proposal id>:<attempt>` | |
| goal review job | goalのgoal review | `goal-review-job` | `goal-review-job:<goal id>:<attempt>` | |
| observer | `dagq observe`が起動するagent | `observer` | `observer:<session id>` | |

どれも`DAGQ_QUEUE`も持つ。`DAGQ_ACTOR_ID`・`DAGQ_RUN_ID`・`DAGQ_TASK_ID`は`[run.env]`で上書きできない（`DAGQ_`の予約）。

### CLIでの解釈

`execute()`はコマンドの前に環境を`ActorContext::from_env`で読む。

- `DAGQ_ROLE`が無いか空なら`user`（人）。host実行の互換のためで、助言的（advisory）でありsandboxではない（ADR-t728-1の決定6）。
- `DAGQ_ROLE`が上の表の値（`user`を除く）でなければ`unknown DAGQ_ROLE: <値>`のerrorで止まり、queueを開かない（fail closed）。`DAGQ_RUN_ID`・`DAGQ_TASK_ID`が読めないときも同じ。
- 旧値`reviewer`（入れ替え前のバイナリが起動したjob）は移行の間だけ`review-job`として読む。
- `DAGQ_ACTOR_ID`が無ければ（それより前に開いたsession）actor idは`DAGQ_ROLE`の値。
- 4つのjob（と`reviewer`）は今までのreviewerと同じ読み取りだけの制限を受け、状態を変えるコマンドは`reviewer may not change queue state`で拒まれる。observerの制限（findingの記録・解決と、findingに紐づく`blocked`のaskだけ）も変わらない。判定は`StaticPolicy`（[Authorization](../authorization.md)）で、どのjobにもCLIの書き込みを与えない。
- noteの`by`・markの`by`・findingの`recorded_by`と状態変更の`by`・askの`asked_by`は`ActorContext::written_by`（roleの名前、userは`human`）、answerの`answered_by`は`ActorContext::answered_by`（roleの名前、userは`person`）から作り、書かれる値は型を入れる前と同じ。

### eventのactor

queueに書かれるeventは全て、書いたactorを`run_events`の列`actor_role`・`actor_id`と、headless jobのverdictを適用したときの依頼元`requested_by`に持つ（ADR-t728-1の決定4、task 730。列は[SQLite persistence](../persistence.md)の`0044_event_actors.sql`）。askの`asked_by`やanswerの`answered_by`とは別の欄で、それらの値は変えない（改名はgoal 48のtask 502）。

- 仕組み: `SqliteQueue`の接続ごとに`EventActors`（`src/infrastructure/event_actor.rs`）がactorを持ち、SQLの関数`dagq_actor_role()`・`dagq_actor_id()`・`dagq_requested_by()`として登録する。`INSERT INTO run_events`は全てこの関数で列を埋めるので、書く場所にactorを渡さず、登録の無い接続での挿入は失敗する。
- 既定のactorはプロセスのactor（`set_process_actor`。`main`がコマンドの前に1回決める）で、決めていなければ（ライブラリを直接使うtest）`user`。`SqliteQueue::with_actor`と`SqliteOpener.actor`で接続ごとに変えられる。
- プロセスのactorは呼び出し元の`ActorContext`（`DAGQ_ROLE`が無ければ`user`）。例外は制御側のプロセスで、`supervise`は`supervisor:<pid>`、runのsession wrapper（`session`）は`wrapper:<run id>`、plannerのwrapper（`planner-session`）は`wrapper:planner:<planner id>`、session hook（`session-event`）はworkerのsessionなら`wrapper:<run id>`、それ以外は`wrapper:<sessionのactor id>`（plannerなら`wrapper:planner:<id>`でwrapperと同じ）。supervisorが起動する`observe`と`auto-update`には`supervisor:<pid>`の環境を渡すので、そのeventもsupervisorになる（observerのagentは`observer:<session id>`のまま）。
- 着地（supervisorの中の`integrate`）はintegratorの文脈ができるまで（ADR-t728-2の後のtask）supervisorのactorで記録する。人が打つ`integrate`はその人（`user`）など呼び出し元になる。
- review・recovery（終わったrunと生きているrun）・plan review・goal reviewのverdictをsupervisorが適用する間（`Supervisor::for_job`）、書かれるeventはactorがsupervisorのまま、`requested_by`にjobのactor id（`review-job:<run>:<attempt>`など、上の表と同じ）を持つ。portでは`RunLog::request_as`。入れ子にはせず、抜けるときに`None`に戻す。生きているrunのrecovery jobのverdictが適用されずにescalateするとき（`recovery_finished`とaskの作成は`apply_live`の外で書かれる）と、review passの後に別のthreadで行う着地のeventは`requested_by`を持たない。
- 読み方: `RunEvent.actor`（`role`・`id`・`requested_by`）。migrationより前の行と古いバイナリが書いた行は`None`で、JSONでは`actor`の欄が無い。`events --full`と`show --full`は`actor`を出し、`show`の要約のeventも`actor`を持つ。`events`の要約（`--full`なし）と`watch`は変えない。secretやpromptの全文は入れない（入るのはroleとidだけ）。
