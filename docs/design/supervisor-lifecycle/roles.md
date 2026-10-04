---
id: design-supervisor-lifecycle-roles
type: design
title: "Roles"
status: current
created: 2026-09-26
updated: 2026-10-05 # task 839: a worker run with the broker's tools gets the PreToolUse hooks counting the built-in tools; task 1437
last_verified: 2026-10-05 # task 839; task 1437
scope: runtime
related:
  - adr-t1228-2
  - adr-t1394-1
  - design-supervisor-lifecycle
  - adr-0044
  - adr-t728-1
  - design-security
---

# Roles

- **supervisor**: runtimeの`supervise`プロセス。taskをclaimし、runごとにworktreeとworkspaceを作って監視し、receiptを検証する。`up`がlaunchdのLaunchAgentとして常駐させるか（既定）、`--in-cmux`なら`[<repo>]supervisor` workspaceの中で動かす。
- **worker**: run session。runごとのcmux workspace `[<repo>]worker#<task-id> - <task title>`（descriptionは`dagq role=worker queue=<queue hash> run=<run-id> task=<id>`）で動くClaude session。`needs_session`のrunをresumeするworkspaceも同じtitleで、descriptionは`run <run-id> resume`。
- **planner**: goal / taskを書いてproposalとしてsubmitするsession。proposalごとのオンデマンドのworkspaceで、常駐しない（[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定1・6）。runtimeが立てる（差し戻し先のplannerが閉じていたproposal・draft・finding・inboxからの計画の依頼。複数同時に開ける）。人が開く`dagq plan`は廃止し、案内を付けて拒む（[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)、task 1399）。workspaceは`[<repo>]planner#<planner-id>`（`DAGQ_ROLE=planner`）で、session wrapper `planner-session`がClaude sessionをそのplannerの初期prompt（`runtime_planner_prompt`・`draft_planner_prompt`・`finding_planner_prompt`・`request_planner_prompt`）付きで起動する（[`plan` / `planners`](plan-planners.md#plan--planners)）。画面を見る人は居ない。依頼・draft・findingは自分で決め、決めきれないものだけ`planner_question`でinboxに上げる。`up` / `down` / `install`は打たない（CLIは許すが、inboxか人の`DAGQ_ROLE`の無いterminalが打つ）。
- **inbox**: 人に届くものすべての窓口になるsession。openなask（worker・supervisor・job・observerの質問）を人に見せてanswerを書き戻し、それ以外のattentionを人に知らせ、人の指示があるときだけ`dagq-recover` skillの手順（`up` / `down`、手でのreviewと`integrate`、`recover`、対話のplannerへのキーと`/exit`（`planner send`）。workerのrunへの`run send`はどのrunにも拒まれ、workerへの答えは`answer`で記録する）を実行する。人が頼んだ計画は人の言葉のまま`request add`で計画の依頼として記録し、runtimeのplannerに移譲する（[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)、[inboxからの計画の依頼](plan-planners.md#inboxからの計画の依頼)）。唯一の常駐sessionで、`up`が`[<repo>]inbox`のworkspace（`DAGQ_ROLE=inbox`）に、`inbox_prompt`付きで起動する（[Session prompts](session-prompts.md#session-prompts)）。
- **observer**: supervisorのtimerが起動するheadlessのjob（`DAGQ_ROLE=observer`、workspaceは持たない）。stats・閉じていないfinding・直近のnote・openなask・graphを読み、findingの記録と更新（`finding record` / `finding resolve`）と、findingに紐づく`blocked`のask（`ask --kind blocked --finding`）だけを書く。note・goal（draftを含む）・taskは書かず、run・task・goalの状態も変えない（[Observer](observer.md#observer)、[Overview](../overview.md)の用語集）。

役割はこの5つ（ADR-0044の決定1。ADR-0024の決定1を引き継ぐ）で、review・recovery（triage）・plan review・goal reviewのjobはsupervisorが起動するheadlessのjob（workspaceは持たない）。runtimeの中で人が打つ`/exit`や復旧は「人（person）」と書き、inboxのsessionか人の`DAGQ_ROLE`の無いterminalから打つ（`src/`に`operator`も退役した役割名も残らない）。

## Actors

信頼の区分、actorごとのcapabilityの要約、host実行が助言的で隔離ではないこと、Podmanとqueue serviceへの道筋は[Security](../security.md)にまとめる。

runtimeはactorを型で表す（[ADR-t728-1](../../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)の決定2・4。`src/domain/actor.rs`）。

- `ActorRole`: `user`・`inbox`・`planner`・`worker`・`review-job`・`recovery-job`・`plan-review-job`・`goal-review-job`・`throughput-review-job`・`observer`・`supervisor`・`wrapper`・`integrator`（`desk`はgoal 48のtask 504が作るときに足す）。workspaceを持つ役割の`SessionRole`（`supervisor`・`worker`・`planner`・`inbox`・`observer`）は`SessionRole::actor_role`で`ActorRole`に写る。
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
| throughput review job | 毎時・日次・週次のスループットの見直し | `throughput-review-job` | `throughput-review-job:<mode>:<期間>` | 読むだけ。結論はsupervisorの`throughput-review`コマンドが保存しinboxに知らせる（[スループットの見直し](throughput-review.md)） |
| observer | `dagq observe`が起動するagent | `observer` | `observer:<session id>` | |

inboxとplanner（とin-cmuxのsupervisor）は`DAGQ_QUEUE`（queue DBのpath）も持つ。workerとresume、headlessのjob、observerは持たず（`domain::queue_service::client_role`）、agentのプロセスはqueue serviceのsocketの`DAGQ_SERVICE_SOCKET`とtokenのfileの`DAGQ_SERVICE_CREDENTIAL_FILE`を持ち、その`dagq`はクライアントモードで動く（goal 82の段(3)、[Queue service](../queue-service.md#クライアントモード)）。runのworkspace自体（session wrapperが動く）はどちらも持たない。`DAGQ_ACTOR_ID`・`DAGQ_RUN_ID`・`DAGQ_TASK_ID`は`[run.env]`で上書きできない（`DAGQ_`の予約）。環境は`actor_env`（`src/application/actor_executor.rs`）の1か所で作り、順は`DAGQ_ROLE`・`DAGQ_QUEUE`（inboxとplannerだけ）・`DAGQ_ACTOR_ID`・`DAGQ_RUN_ID`・`DAGQ_TASK_ID`、inboxとplannerのsessionの区間の種類`DAGQ_SESSION_KIND`、plannerの`DAGQ_PLANNER_ORIGIN`・`DAGQ_PLANNER_ID`・`DAGQ_LAUNCH`で、workerのworkspaceはその後に`[run.env]`を足す。headlessのjobは呼び出し元の変数（reviewの`[run.env]`、observerの`PATH`）の後にこれを足すので、actorの変数は上書きされない。in-cmuxのsupervisorのworkspaceもAI actorではないが同じ形の環境を持つ。

### actorの起動（ActorExecutor）

runtimeが起動するAI actorは全て`ActorExecutor::spawn(ActorExecutionSpec) -> ActorHandle`を通る（goal 55、task 737。`src/application/actor_executor.rs`）。

- `ActorExecutionSpec`: `actor`（`ActorContext`。roleとactor id、workerならrunとtask）、`capabilities`（roleの`grants`。[Authorization](../authorization.md)）、`workspace`（`Write`・`Read`・`Scratch`とそのpath）、`resources`（`timeout`）、`program`（何を起動するか）。`run_id()`・`task_id()`はactorのものか、programのrunのもの。
- `program`: `RunWorkspace`（workerとresumeのworkspaceでsession wrapperを動かす）、`NamedWorkspace`（inboxのagent、plannerのwrapper）、`SessionAgent`（session wrapperがworkspaceの中で起動するworker・resume・plannerのagent）、`Headless`（review・recovery・plan review・goal reviewのjobとobserverのagent。`HeadlessProgram::Review`と`Job`）。
- `spawn`はまず`ActorExecutionSpec::check`で整合を確かめ、合わなければ起動しない（fail closed）: roleの`TrustLevel`が`UntrustedAgent`でない（`user`・`supervisor`・`wrapper`・`integrator`）、programがroleのものでない（`RunWorkspace`はworker、inboxのagentはinbox、`Review`はreview-jobなど）、`capabilities`がroleの`grants`と違う、workerのactorのrunとprogramのrunが違う。
- `ActorHandle`: workspaceのUUID（`Workspace`）かプロセス（`Process`）。
- `HostActorExecutor`がただ1つのbackend（`backend()`は`host`、`enforcement()`は`advisory`。下の「実行のbackendとenforcement」）。cmuxの`WorkspaceBackend`、`AgentProvider`（Claude Code）、`Spawner`の上に作り、呼び出し元が持つ部品だけを渡す（supervisorは全部、`up`はcmuxとinboxのprovider、plannerを開くところはcmux、session wrapperはproviderとspawner、`observe`はproviderと`LocalSpawner`）。要る部品の無いprogramは拒む。hostではspecは記録と整合の検査だけで隔離ではなく、プロセスはこのユーザーにできることを全てできる（ADR-t728-1の決定6）。`resources.timeout`もhostではsupervisorのtimerとobserverのdeadlineが守る。sandboxのbackend（goal 38）は同じspecを強制する側になる。
- Claudeのsettingsは、Claude Code adapter（`src/infrastructure/adapters.rs`）の`agent_settings(role, planner origin)`の1か所でroleから決まる（Claude Codeの実装のものなのでadapterが持つ。ADR-t1063-1の決定3、task 1064。headless jobは権限の意図`JobAccess`だけを渡す。[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）: workerとruntimeが立てたplannerはsession（`Stop` hook・`UserPromptSubmit` hook・`permissions.deny`、サジェストなし。brokerの道具を渡したworkerのrun（`<run dir>/broker/mcp.json`がある）だけ、`write_settings`が組み込みの道具を数える`PreToolUse`のhookを足す。[Broker](../broker.md#組み込みの道具の数)）、廃止前に人が`dagq plan`で開いたplannerはサジェストありのsession、review jobはreviewの設定（hookなし）、recovery・plan review・goal reviewのjobとobserverはdagqの設定を書かない（`AgentSettings::None`）。inboxも`agent_settings`では`None`だが、`up`が開くときに`AgentProvider::inbox_command`が`permissions.deny`だけの設定（queueのディレクトリの`claude-inbox-settings.json`。hookもサジェストの設定も無い）を書いて`--settings`で渡す（[ADR-t1228-2](../../adr/2026-10-02-t1228-2-deny-raw-cmux-to-inbox-and-planner-as-a-guardrail.md)、[`up` / `down`](up-down.md)）。設定を書くrole（worker・planner・review job・inbox）の`permissions.deny`には、roleのpolicyが拒むコマンドの規則（`permission_deny(role)`。inboxとplannerは`Bash(cmux:*)`も。[Authorization](../authorization.md#claudeのpermissionsdenyguardrail)）が入る。Claude Code adapterはこれをファイルに書くだけ（[Agent provider lifecycle](../provider-lifecycle.md)）。
- executorの外でClaudeを起動するのはtestのstubだけ。executorの外で起動するもの（`up --in-cmux`のsupervisorのworkspace、session wrapper自身、supervisorが起動する`observe`と`auto-update`のコマンド）はAI actorではなく、信頼する制御側。

### 実行のbackendとenforcement

actorをどこで動かし、specをどこまで守らせるかは`src/application/execution.rs`の型が持つ（task 738）。

- `ExecutorBackend`: `host`（このユーザーのhost上のプロセス）と`podman`（goal 38のための予約の名前）。`podman`は未実装で、`ensure_implemented`がerrorにする。選ばれたactorは起動せず、hostに黙って戻さない（fail closed）。知らない名前も`ExecutorBackend::parse`がerrorにする。
- `EnforcementLevel`: `advisory`（runtimeのコードが記録と検査をするだけで隔離ではない。hostの値）、`confined`（OSのsandboxが書き込みと他のプロセスへのsignalを止めるが、同じユーザーで動き、読み取りとnetworkは開いていて隔離ではない。hostで動くCodexのworkerの値。[ADR-t813-3](../../adr/2026-09-28-t813-3-codex-worker-permissions.md)の決定7）、`sandbox`（プロセスが抜けられない隔離。`podman`が実装されたときの値）。`is_sandbox`（`sandboxed`の欄）は`sandbox`だけがtrueで、`confined`はfalse。workerのproviderごとの値は`EnforcementLevel::of_worker`が決める（hostではClaudeが`advisory`、Codexが`confined`。隔離するbackendでは両方がそのbackendの値）。
- `ExecutionConfig`: 全actorの既定の`backend`と、roleごとの上書き`actors`（`(ActorRole, ExecutorBackend)`の並び）。将来の設定ファイルの次の形と対応させる。この段では設定ファイルを読まず、既定（全actorが`host`）だけを使う。

  ```toml
  [security]
  backend = "host"        # 全actorの既定。今は host だけ

  [actors.worker]
  backend = "host"        # roleごとの上書き。podman は未実装で起動を拒む
  ```

- `HostActorExecutor`は`ExecutionConfig`を持ち（`with_config`、既定は全actorが`host`）、`spawn`でactorのroleの`backend`が`host`でなければ起動を拒む。
- `status`と`doctor`（要約と`--full`）は`actors`に、AI actor（`TrustLevel`が`untrusted_agent`のrole: inbox・planner・worker・review-job・recovery-job・plan-review-job・goal-review-job・throughput-review-job・observer）ごとの`role`・`backend`・`enforcement`・`sandboxed`を出す（`actor_executions`）。今は全て`host`・`advisory`・`false`で、host実行がsandboxではないことをここでも明示する。roleの行の`enforcement`はそのactorへの最も弱い強制で、workerの行も`advisory`のまま（Claudeのworkerがいつでも動きうるため）。
  - 生きている（pidが生きている）登録されたsupervisorのどれかの`providers`（`supervisors[].providers`）でCodexが使える（`found`で、動かすmodeがある）ときだけ、workerの行に`providers`の配列を足す: 使えるproviderごとの`provider`・`modes`（supervisorをまたいだmodeの和）・`enforcement`・`sandboxed`で、Claudeは`advisory`、Codexは`confined`、`sandboxed`はどちらも`false`（ADR-t813-3の決定7）。例: `{"role": "worker", "backend": "host", "enforcement": "advisory", "sandboxed": false, "providers": [{"provider": "claude", "modes": ["interactive", "headless"], "enforcement": "advisory", "sandboxed": false}, {"provider": "codex", "modes": ["headless"], "enforcement": "confined", "sandboxed": false}]}`。
  - Codexが使えない（生きているsupervisorが無い、`codex`が見つからない、動かすmodeが無い）ときは`providers`の欄ごと省き、行は前と同じ形になる。roleの行を分けずに欄を足す形にしたのは、`role`の並びと各行の`enforcement`を読む既存の読み手（testとpluginのskill）を変えないため。

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
- 着地（rebase・検証・squash・mainの更新・push）のeventはactorがIntegrator（`integrator:<pid>`）で、`requested_by`が依頼者（supervisorの着地は`supervisor:<pid>`、人が打つ`integrate`は`user`、inboxの代行は`inbox`）。依頼の`integration_approved`は依頼者、slotを取る`integration_started`はsupervisorの記録（[Authorization](../authorization.md#着地とpushintegrator)、task 736）。
- review・recovery（終わったrunと生きているrun）・plan review・goal reviewのverdictをsupervisorが適用する間（`Supervisor::for_job`）、書かれるeventはactorがsupervisorのまま、`requested_by`にjobのactor id（`review-job:<run>:<attempt>`など、上の表と同じ）を持つ。portでは`RunLog::request_as`（入る前の`requested_by`を返す）と`RunLog::restore_request`。抜けるときは（適用が失敗しても）入る前の値に戻すので、`for_job`が入れ子になっても内側を抜けた後の外側のeventは外側のjobの`requested_by`を持ち、外側を抜けると元の値（ふつうは無し）に戻る（task 783。Integratorの着地も同じく依頼者を入る前の値に戻す）。生きているrunのrecovery jobのverdictが適用されずにescalateするとき（`escalate`・confidence lowの`repair`・今は成り立たない`repair`）も、そのescalationの`recovery_finished`と開く ask（`stalled`。過去の `stuck_exit`・`answer_prompt` の記録も同じ actor の読み方）の`ask_opened`は`Supervisor::for_escalation`の中で書かれ、`requested_by`にjobのactor id（`recovery-job:<run>:<alert>:<attempt>`）を持つ（task 782。`Escalation::requester`）。jobの失敗と、alertのjobを使い切ったとき（`UsedUp`）のescalationはjobの依頼ではないので`requested_by`を持たない。review passの後の着地のeventは上のとおりIntegratorのactorで、`requested_by`が依頼したsupervisor。
- 読み方: `RunEvent.actor`（`role`・`id`・`requested_by`）。migrationより前の行と古いバイナリが書いた行は`None`で、JSONでは`actor`の欄が無い。`events --full`と`show --full`は`actor`を出し、`show`の要約のeventも`actor`を持つ。`events`の要約（`--full`なし）と`watch`は変えない。secretやpromptの全文は入れない（入るのはroleとidだけ）。

### headlessのjobのverdictと遷移

review・recovery・plan review・goal reviewのjobは、それぞれ`review-job`・`recovery-job`・`plan-review-job`・`goal-review-job`のactorで（1つのroleにまとめない）、出力は型付きのverdict（`ReviewVerdict`・`RecoveryVerdict`・`PlanReviewVerdict`・`GoalReviewVerdict`）として読むデータにすぎない。遷移を決めるのはverdictを受けたsupervisorのRustのコードで（ADR-t728-1、ADR-t728-2の決定3）、写像は次のとおり決定的。

| job | verdict | supervisorが行うこと |
| --- | --- | --- |
| review-job | `pass` | 衝突の事前検査のあと、sessionの終了を待って着地の段（`AfterExit::Land`）へ進めるだけ。着地の関数は呼ばない。着地は別のthreadの`land_integrating`が、rebase後の検証コマンドを含む自分の検査を通したときだけ行い、落ちればrunは`needs_session`になる |
| review-job | `revise` / `concern` | 生きているsessionへの差し戻し（`revise`の上限を使い切ったか、sessionが終わっていれば`approve_landing`のask） / `approve_landing`のask |
| recovery-job | `repair`（`confidence: high`） | `RecoveryVerdict::applies()`を通り、さらにalertごとの前提（終わったrunは`plan_ended`、生きているrunは`check_live`）を今も満たすactionだけを適用する。満たさなければaskにする |
| recovery-job | `repair`（`low`）/ `escalate` | 適用せず、actionを推奨として`decide`などのaskにする |
| plan-review-job | `pass` / `revise` / `concern` | 1つのtransactionで`ready`・plannerへの差し戻し・`approve_plan`のask。`concern`の`high`で`reason_category`が`null`の`ready` / `send_back`はpass / reviseとして適用する（[Plan review](plan-review.md#aiが決めるconcern未実装)、ADR-t451-1決定4）。`actions`は`pass`（と適用した`ready`）のときだけ許された3種を適用し、`reopen`はverdictに関わらず検査して適用する。`pass`では改善のproposalの`high`以上のtaskを`normal`に下げる（KPIの`max_improvement_proposals`） |
| goal-review-job | `achieved` / `gaps` / `ask` | goalを閉じる・gapをdraftにする・`approve_goal`のask（`gaps`の連続が上限を超えたら`ask`） |

- 未知の欄を拒む: 4つのverdictと入れ子の型（reviewのverdict、recoveryのaction、plan reviewのaction・`reopen`、goal reviewの`criteria`・`gaps`）は`#[serde(deny_unknown_fields)]`で、未知の欄・未知のactionを含む出力は読めない出力として扱う。
- 例外は`PlanReviewVerdict`の`predictions`で、`serde_json::Value`のまま持つ。型にするとtaskの重さの見積もりの形が崩れただけでverdict全体が読めず、plan reviewが失敗するが、見積もりは記録するだけで遷移を変えない（ADR-0079決定2）ため。supervisorは`parse_predictions`で型付きの`TaskWeightPrediction`にしてから記録し、読めなければ記録しないだけで、生の値から遷移を決めない。
- 壊れた出力はfail closed: 読めないverdictで何かが着地・retry・`ready`・goalのcloseになることはない。reviewは1回だけ同じ入力で再review（`review_retried`）し、それも読めなければ`review_failed`と`approve_landing`のask。recoveryは`triage_failed`（`triage by hand`）。生きているrunのjobの失敗はそのalertのask（`reason_category: recovery_failed`。ADR-t609-1）。plan reviewは`plan_review_failed`（`plan review by hand`）でproposalは`submitted`のまま。goal reviewは`goal_review_failed`（`goal review by hand`）でgoalは開いたまま。
- 記録: verdictを適用したeventは上の「eventのactor」のとおりactorがsupervisor、`requested_by`がjobのactor id。reviewの`concern`と差し戻せない`revise`（上限を使い切った・sessionが終わっていた・切り替えられなかった・送れなかった）と、`pass`の後の衝突の事前検査が上限で人に聞く`approve_landing`のaskは、sessionの終了の後に開くが、`AfterExit::Ask`がverdictを返したreview jobのactor（`requested_by`）を持ち、askを開くときだけ`for_job`で包むので、その`ask_opened`もactorがsupervisor、`requested_by`が`review-job:<run>:<attempt>`になる（task 798）。askの後にleaseを返すなどのeventは`for_job`の外で、`requested_by`を持たない。supervisorの引き継ぎ（`adopt.rs`）が履歴から`AfterExit::Ask`を組み立て直すときは、引き継ぐ（か直前の）`review_finished`のpayloadの`attempt`から同じjobを復元し、`attempt`が無ければ別のjobを名指ししないよう`requested_by`を付けない。引き継いだpassのrunの衝突の事前検査も同じjobの`for_job`の中で行うので、その`conflict_precheck`も生きている経路と同じく`requested_by`を持つ。差し戻しや衝突の依頼を生きているsessionが果たさなかった後（sessionが終わった・receiptの直しを送れなかった）のaskは、verdictを適用し終えた後のsupervisor自身の判断なので`requested_by`を持たない。jobの失敗のeventはjobの依頼ではないので`requested_by`を持たない（どのjobかはpayloadの`attempt`で分かる）。jobが開くaskの`asked_by`（`plan_review`・`goal_review`など）は変えない。
- test: `tests/it/runtime_job_verdicts.rs`（reviewのpassが検証の落ちるrunを着地させないこと、reviewと終わったrunと生きているrunの各alertのrecoveryの壊れた出力（alertごとに代表の1つ。形ごとのerrorは`RecoveryVerdict::parse`と`ReviewVerdict::parse`のunit test）、`concern`・上限を超えた`revise`・引き継いだ`concern`のaskが`requested_by`にreview jobを持ち、失敗したreviewのaskは持たないこと）、`tests/it/plan_review.rs`と`tests/it/goal_review.rs`の`..._applied_at_its_jobs_request_and_a_broken_one_fails_closed`。
