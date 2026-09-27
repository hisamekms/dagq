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

どれも`DAGQ_QUEUE`も持つ。`DAGQ_ACTOR_ID`・`DAGQ_RUN_ID`・`DAGQ_TASK_ID`は`[run.env]`で上書きできない（`DAGQ_`の予約）。環境は`actor_env`（`src/application/actor_executor.rs`）の1か所で作り、順は`DAGQ_ROLE`・`DAGQ_QUEUE`・`DAGQ_ACTOR_ID`・`DAGQ_RUN_ID`・`DAGQ_TASK_ID`、inboxとplannerのsessionの区間の種類`DAGQ_SESSION_KIND`、plannerの`DAGQ_PLANNER_ORIGIN`・`DAGQ_PLANNER_ID`・`DAGQ_LAUNCH`で、workerのworkspaceはその後に`[run.env]`を足す。headlessのjobは呼び出し元の変数（reviewの`[run.env]`、observerの`PATH`）の後にこれを足すので、actorの変数は上書きされない。in-cmuxのsupervisorのworkspaceもAI actorではないが同じ形の環境を持つ。

### actorの起動（ActorExecutor）

runtimeが起動するAI actorは全て`ActorExecutor::spawn(ActorExecutionSpec) -> ActorHandle`を通る（goal 55、task 737。`src/application/actor_executor.rs`）。

- `ActorExecutionSpec`: `actor`（`ActorContext`。roleとactor id、workerならrunとtask）、`capabilities`（roleの`grants`。[Authorization](../authorization.md)）、`workspace`（`Write`・`Read`・`Scratch`とそのpath）、`resources`（`timeout`）、`program`（何を起動するか）。`run_id()`・`task_id()`はactorのものか、programのrunのもの。
- `program`: `RunWorkspace`（workerとresumeのworkspaceでsession wrapperを動かす）、`NamedWorkspace`（inboxのagent、plannerのwrapper）、`SessionAgent`（session wrapperがworkspaceの中で起動するworker・resume・plannerのagent）、`Headless`（review・recovery・plan review・goal reviewのjobとobserverのagent。`HeadlessProgram::Review`と`Job`）。
- `spawn`はまず`ActorExecutionSpec::check`で整合を確かめ、合わなければ起動しない（fail closed）: roleの`TrustLevel`が`UntrustedAgent`でない（`user`・`supervisor`・`wrapper`・`integrator`）、programがroleのものでない（`RunWorkspace`はworker、inboxのagentはinbox、`Review`はreview-jobなど）、`capabilities`がroleの`grants`と違う、workerのactorのrunとprogramのrunが違う。
- `ActorHandle`: workspaceのUUID（`Workspace`）かプロセス（`Process`）。
- `HostActorExecutor`がただ1つのbackend（`backend()`は`host`、`enforcement()`は`advisory`）。cmuxの`WorkspaceBackend`、`AgentProvider`（Claude Code）、`Spawner`の上に作り、呼び出し元が持つ部品だけを渡す（supervisorは全部、`up`はcmuxとinboxのprovider、plannerを開くところはcmux、session wrapperはproviderとspawner、`observe`はproviderと`LocalSpawner`）。要る部品の無いprogramは拒む。hostではspecは記録と整合の検査だけで隔離ではなく、プロセスはこのユーザーにできることを全てできる（ADR-t728-1の決定6）。`resources.timeout`もhostではsupervisorのtimerとobserverのdeadlineが守る。sandboxのbackend（goal 38）は同じspecを強制する側になる。
- Claudeのsettingsは`agent_settings(role, planner origin)`の1か所でroleから決まる: workerとruntimeが立てたplannerはsession（`Stop` hook・`UserPromptSubmit` hook・`permissions.deny`、サジェストなし）、人のplannerはサジェストありのsession、review jobはreviewの設定（hookなし）、inbox・recovery・plan review・goal reviewのjobとobserverはdagqの設定を書かない。Claude Code adapterはこれをファイルに書くだけ（[Agent provider lifecycle](../provider-lifecycle.md)）。
- executorの外でClaudeを起動するのはtestのstubだけ。executorの外で起動するもの（`up --in-cmux`のsupervisorのworkspace、session wrapper自身、supervisorが起動する`observe`と`auto-update`のコマンド）はAI actorではなく、信頼する制御側。

### CLIでの解釈

`execute()`はコマンドの前に環境を`ActorContext::from_env`で読む。

- `DAGQ_ROLE`が無いか空なら`user`（人）。host実行の互換のためで、助言的（advisory）でありsandboxではない（ADR-t728-1の決定6）。
- `DAGQ_ROLE`が上の表の値（`user`を除く）でなければ`unknown DAGQ_ROLE: <値>`のerrorで止まり、queueを開かない（fail closed）。`DAGQ_RUN_ID`・`DAGQ_TASK_ID`が読めないときも同じ。
- 旧値`reviewer`（入れ替え前のバイナリが起動したjob）は移行の間だけ`review-job`として読む。
- `DAGQ_ACTOR_ID`が無ければ（それより前に開いたsession）actor idは`DAGQ_ROLE`の値。
- 計画系のコマンド（`add`・`submit`・`ready`・`cancel`・`goal close`など）は全てのroleについてapplicationの層が判定し、拒んだ試みを`authorization_denied`に記録する（task 732、[Authorization](../authorization.md#計画系のコマンドapplication)）。対話と記録のコマンド（`ask`・`answer`・`note`・`mark`・`finding`。task 733）と、runtimeの操作系のコマンド（`integrate`・`recover`・`review`・`up`・`down`・`install`・`auto-update`・`supervise`・`observe`・`plan`・`init`・`migrate`・`rebind`と内部の`session`・`session-event`・`planner-session`。task 734、[Authorization](../authorization.md#runtimeの操作系のコマンドapplication)）も同じ。runtimeのwrapperとhookはworkerの環境のまま打つので、workerは自分のrunの`session`と`session-event`だけを打てる。
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

### headlessのjobのverdictと遷移

review・recovery・plan review・goal reviewのjobは、それぞれ`review-job`・`recovery-job`・`plan-review-job`・`goal-review-job`のactorで（1つのroleにまとめない）、出力は型付きのverdict（`ReviewVerdict`・`RecoveryVerdict`・`PlanReviewVerdict`・`GoalReviewVerdict`）として読むデータにすぎない。遷移を決めるのはverdictを受けたsupervisorのRustのコードで（ADR-t728-1、ADR-t728-2の決定3）、写像は次のとおり決定的。

| job | verdict | supervisorが行うこと |
| --- | --- | --- |
| review-job | `pass` | 衝突の事前検査のあと、sessionの`/exit`を待って着地の段（`AfterExit::Land`）へ進めるだけ。着地の関数は呼ばない。着地は別のthreadの`land_integrating`が、rebase後の検証コマンドを含む自分の検査を通したときだけ行い、落ちればrunは`needs_session`になる |
| review-job | `revise` / `concern` | 生きているsessionへの差し戻し（`revise`の上限を使い切ったか、sessionが終わっていれば`approve_landing`のask） / `approve_landing`のask |
| recovery-job | `repair`（`confidence: high`） | `RecoveryVerdict::applies()`を通り、さらにalertごとの前提（終わったrunは`plan_ended`、生きているrunは`check_live`）を今も満たすactionだけを適用する。満たさなければaskにする |
| recovery-job | `repair`（`low`）/ `escalate` | 適用せず、actionを推奨として`decide`などのaskにする |
| plan-review-job | `pass` / `revise` / `concern` | 1つのtransactionで`ready`・plannerへの差し戻し・`approve_plan`のask。`actions`は`pass`のときだけ許された3種を適用し、`reopen`はverdictに関わらず検査して適用する。`pass`では改善のproposalの`high`以上のtaskを`normal`に下げる（KPIの`max_improvement_proposals`） |
| goal-review-job | `achieved` / `gaps` / `ask` | goalを閉じる・gapをdraftにする・`approve_goal`のask（`gaps`の連続が上限を超えたら`ask`） |

- 未知の欄を拒む: 4つのverdictと入れ子の型（reviewのverdict、recoveryのaction、plan reviewのaction・`reopen`、goal reviewの`criteria`・`gaps`）は`#[serde(deny_unknown_fields)]`で、未知の欄・未知のactionを含む出力は読めない出力として扱う。
- 例外は`PlanReviewVerdict`の`predictions`で、`serde_json::Value`のまま持つ。型にするとtaskの重さの見積もりの形が崩れただけでverdict全体が読めず、plan reviewが失敗するが、見積もりは記録するだけで遷移を変えない（ADR-0079決定2）ため。supervisorは`parse_predictions`で型付きの`TaskWeightPrediction`にしてから記録し、読めなければ記録しないだけで、生の値から遷移を決めない。
- 壊れた出力はfail closed: 読めないverdictで何かが着地・retry・`ready`・goalのcloseになることはない。reviewは1回だけ同じ入力で再review（`review_retried`）し、それも読めなければ`review_failed`と`approve_landing`のask。recoveryは`triage_failed`（`triage by hand`）か生きているrunの`recovery_failed`（`recover by hand`）。plan reviewは`plan_review_failed`（`plan review by hand`）でproposalは`submitted`のまま。goal reviewは`goal_review_failed`（`goal review by hand`）でgoalは開いたまま。
- 記録: verdictを適用したeventは上の「eventのactor」のとおりactorがsupervisor、`requested_by`がjobのactor id。例外として、reviewの`concern`（と差し戻せない`revise`）の`approve_landing`のaskは`AfterExit::Ask`でsessionの`/exit`の後に`for_job`の外で開くので、その`ask_opened`は`requested_by`を持たない。jobの失敗のeventはjobの依頼ではないので`requested_by`を持たない（どのjobかはpayloadの`attempt`で分かる）。jobが開くaskの`asked_by`（`plan_review`・`goal_review`など）は変えない。
- test: `tests/it/runtime_job_verdicts.rs`（reviewのpassが検証の落ちるrunを着地させないこと、reviewと終わったrunのrecoveryの壊れた出力）、`tests/it/plan_review.rs`と`tests/it/goal_review.rs`の`..._applied_at_its_jobs_request_and_a_broken_one_fails_closed`。
