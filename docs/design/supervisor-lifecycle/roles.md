---
id: design-supervisor-lifecycle-roles
type: design
title: "Roles"
status: current
created: 2026-09-26
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

- **supervisor**: runtimeの`supervise`プロセス。
  taskをclaimし、runごとにworktreeを作ってsession wrapperをbackgroundで起動して監視し、receiptを検証する。
  `up`がlaunchdのLaunchAgentとして常駐させる（[ADR-t1433-4](../../adr/2026-10-03-t1433-4-supervisor-resides-without-cmux.md)）。
  前のbinaryがin-cmux modeで起動して登録したsupervisorは、引き継ぎと`down`を受けるだけで、新しくは起動しない。
- **worker**: run session。
  runごとのsession wrapperがworkspaceなしのsupervisorから切り離したbackgroundのprocessとして動き、turnごとに非対話のagent（ClaudeかCodex）を起動する（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)、[非対話のworker](headless-worker.md)）。
  `needs_session`のrunのresumeも同じくbackgroundのwrapperで動く。
- **planner**: goal / taskを書いてproposalとしてsubmitするsession。
  proposalごとのオンデマンドのbackgroundのwrapperで、workspaceを持たず、常駐しない（[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定1・6）。
  runtimeが立てる（差し戻し先のplannerが閉じていたproposal・draft・finding・inboxからの計画の依頼。複数同時に開ける）。
  人が開く`dagq plan`は廃止し、案内を付けて拒む（[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)）。
  session wrapper `planner-session`がbackgroundのprocessとして動き、turnごとにClaudeを非対話で起動する（最初のturnはそのplannerの初期prompt付き。経路は非対話だけ。ADR-t1433-2決定3。[`plan` / `planners`](plan-planners.md#plan--planners)）。
  画面を見る人は居ない。
  依頼・draft・findingは自分で決め、決めきれないものだけ`planner_question`でinboxに上げる。
  `up` / `down` / `install`は打たない（CLIは許すが、inboxか人の`DAGQ_ROLE`の無いterminalが打つ）。
- **inbox**: 人に届くものすべての窓口になるsession。
  openなask（worker・supervisor・job・observerの質問）を人に見せてanswerを書き戻し、それ以外のattentionを人に知らせ、人の指示があるときだけ`dagq-recover` skillの手順を実行する（workerのrunへの`run send`とplannerへの`planner send`はどれにも拒まれ（ADR-t1433-2）、workerとplannerへの答えは`answer`で記録する）。
  人が頼んだ計画は人の言葉のまま`request add`で計画の依頼として記録し、runtimeのplannerに移譲する（[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)、[inboxからの計画の依頼](plan-planners.md#inboxからの計画の依頼)）。
  唯一の常駐sessionで、人が自分のterminalで打つ`dagq inbox`が`DAGQ_ROLE=inbox`で、`inbox_prompt`付きでその前面に起動する（`up`は開かない。[`dagq inbox`](up-down.md#dagq-inbox)）。
  初期promptは短く、CLIの手順を持たず（手順はskillが持つ）、skillを変えてもpromptは変わらない。
- **observer**: supervisorのtimerが起動するheadlessのjob（`DAGQ_ROLE=observer`、workspaceは持たない）。
  stats・閉じていないfinding・直近のnote・openなask・graphを読み、findingの記録と更新と、findingに紐づく`blocked`のaskだけを書く。
  note・goal（draftを含む）・taskは書かず、run・task・goalの状態も変えない（[Observer](observer.md#observer)、[Overview](../overview.md)の用語集）。

役割はこの5つ（ADR-0044の決定1。ADR-0024の決定1を引き継ぐ）で、reviewなどのjob（一覧は`src/domain/actor.rs`の`ActorRole::is_headless_job`）はsupervisorが起動するheadlessのjob（workspaceは持たない）。
例外はreviewの設定の生成のjob（`setup-review-job`）で、supervisorでなく人かinboxが打つ`dagq setup-review`のコマンドが起動する（[setup-review](setup-review.md)）。
runtimeの中で人が打つ`/exit`や復旧は「人（person）」と書き、inboxのsessionか人の`DAGQ_ROLE`の無いterminalから打つ。

## Actors

信頼の区分、actorごとのcapabilityの要約、host実行が助言的で隔離ではないこと、containerとqueue serviceへの道筋は[Security](../security.md)にまとめる。

runtimeはactorを型で表す（[ADR-t728-1](../../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)の決定2・4。`src/domain/actor.rs`）。

- `ActorRole`: workspaceを持つ役割の`SessionRole`は`SessionRole::actor_role`で`ActorRole`に写る。
- `TrustLevel`: roleだけから決まり（`ActorRole::trust`）、promptや名前から推し量らない。

### 環境変数

runtimeが起動するAI actorは全て、環境にroleとactor idを持つ。

`DAGQ_ROLE`はroleの値（`ActorRole::env_value`）、`DAGQ_ACTOR_ID`は`ActorContext`のconstructor（`ActorContext::instance`の`<role>:<instance>`など）が作る。

| actor | 起動するところ | ほか |
| --- | --- | --- |
| worker（resumeも） | runのbackgroundのsession wrapper | `DAGQ_RUN_ID`・`DAGQ_TASK_ID` |
| planner（runtime・draft・finding） | runtimeのplannerのbackgroundのwrapper（`planner-session`）とそのturn | |
| inbox | 人のterminalの`dagq inbox` | |
| supervisor（前のbinaryが登録したin-cmuxのもの） | 前のbinaryのin-cmux mode（新しくは起動しない） | |
| review job | runのreview | |
| recovery job | 終わったrunと生きているrunの復旧 | |
| plan review job | proposalのplan review | |
| goal review job | goalのgoal review | |
| throughput review job | 毎時・日次・週次のスループットの見直し | 読むだけ。結論はsupervisorの`throughput-review`コマンドが保存しinboxに知らせる（[スループットの見直し](throughput-review.md)） |
| setup review job | 人かinboxの`dagq setup-review` | queueは読むだけで、書けるのは一時の場所のfileだけ（[setup-review](setup-review.md)） |
| observer | `dagq observe`が起動するagent | actor idは`observer_actor`（`src/application/observer/mod.rs`）が決める |

inboxとplanner（と登録済みのin-cmuxのsupervisor）は`DAGQ_QUEUE`（queue DBのpath）も持つ。
workerとresume、headlessのjob、observerは持たず（`domain::queue_service::client_role`）、agentのプロセスはqueue serviceのsocketの`DAGQ_SERVICE_SOCKET`とtokenのfileの`DAGQ_SERVICE_CREDENTIAL_FILE`を持ち、その`dagq`はクライアントモードで動く（[Queue service](../queue-service.md#クライアントモード)）。
runのsession wrapperのprocess自体はどちらも持たない。
`DAGQ_ACTOR_ID`・`DAGQ_RUN_ID`・`DAGQ_TASK_ID`は`[run.env]`で上書きできない（`DAGQ_`の予約）。
環境は`actor_env`（`src/application/actor_executor.rs`）の1か所で作り、runのsession wrapperはその後に`[run.env]`を足す。
headlessのjobは呼び出し元の変数（reviewの`[run.env]`、observerの`PATH`）の後にこれを足すので、actorの変数は上書きされない。
登録済みのin-cmuxのsupervisorのworkspaceもAI actorではないが同じ形の環境を持つ。

### actorの起動（ActorExecutor）

runtimeが起動するAI actorは全て`ActorExecutor::spawn(ActorExecutionSpec) -> ActorHandle`を通る（`src/application/actor_executor.rs`）。

- `spawn`はまず`ActorExecutionSpec::check`で整合を確かめ、合わなければ起動しない（fail closed）: roleの`TrustLevel`が`UntrustedAgent`でない、programがroleのものでない、`capabilities`がroleの`grants`と違う、workerのactorのrunとprogramのrunが違う。
- `HostActorExecutor`がただ1つのbackend（下の「実行のbackendとenforcement」）。
  backgroundのwrapperの`SessionWrappers`、`AgentProvider`、`Spawner`の上に作り、呼び出し元が持つ部品だけを渡す。
  要る部品の無いprogramは拒む。
  hostではspecは記録と整合の検査だけで隔離ではなく、プロセスはこのユーザーにできることを全てできる（ADR-t728-1の決定6）。
  `resources.timeout`もhostではsupervisorのtimerとobserverのdeadlineが守る。
  sandboxのbackendは同じspecを強制する側になる。
- Claudeのsettingsは、Claude Code adapter（`src/infrastructure/adapters.rs`）の`agent_settings(role)`の1か所でroleから決まる（Claude Codeの実装のものなのでadapterが持つ。ADR-t1063-1の決定3。headless jobは権限の意図`JobAccess`だけを渡す。[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）。
  workerとruntimeのplannerは非対話のturnで動き、turnのsettingsは`turn_command`が書く（hookなし、`permissions.deny`、サジェストの設定なし）。
  inboxも`agent_settings`では`None`だが、`dagq inbox`が開くときに`AgentProvider::inbox_command`が`permissions.deny`だけの設定（hookもサジェストの設定も無い）を書いて渡す（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定5、[`up` / `down`](up-down.md)）。
  設定を書くroleの`permissions.deny`には、roleのpolicyが拒むコマンドの規則（`permission_deny(role)`。[Authorization](../authorization.md#claudeのpermissionsdenyguardrail)）が入る。
  Claude Code adapterはこれをファイルに書くだけ（[Agent provider lifecycle](../provider-lifecycle.md)）。
- executorの外でClaudeを起動するのはtestのstubだけ。executorの外で起動するもの（登録済みのin-cmuxのsupervisorのworkspace、session wrapper自身、supervisorが起動する`observe`と`auto-update`のコマンド）はAI actorではなく、信頼する制御側。

### 実行のbackendとenforcement

actorをどこで動かし、specをどこまで守らせるかは`src/application/execution.rs`の型が持つ。

値と欄の意味は各定義のdoc commentが持つ。

- `ExecutorBackend`はactorを動かす場所、`EnforcementLevel`はそこでの強制の強さで、隔離は`sandbox`だけである。
  hostで動くCodexのworkerはOSのsandboxが書き込みと他のプロセスへのsignalを止めるが、隔離ではない（[ADR-t813-3](../../adr/2026-09-28-t813-3-codex-worker-permissions.md)の決定7）。
  workerのproviderごとの強制は`EnforcementLevel::of_worker`が決める。
- containerで起動するactorはworkerのturnだけで、runのsession wrapperが`run_container::PodmanActorExecutor`で起動する（[Container runtime](../container-runtime.md#workerの実行)）。
  integrateのverificationのコマンドはactorでなく、`integrate`がcontainerで流す（[Container runtime](../container-runtime.md#verificationの実行)）。
  hostのexecutorはcontainerに置いたactorを起動せず`ensure_implemented`がerrorにし、hostに黙って戻さない（fail closed）。
  知らない名前も`ExecutorBackend::parse`がerrorにする。
- `HostActorExecutor`は`ExecutionConfig`を持ち（`with_config`）、actorのroleのbackendがhostでなければ起動を拒む。
  例外はcontainerに置いたworkerのrunのsession wrapperで、制御側としてhostで起動する。
  `status`・`doctor`とsupervisor・wrapperのactorの起動は`ExecutionConfig::of_runtimes`でworkerを`dagq.toml`のruntimeに置き、他のactorはhostのまま（[Container runtime](../container-runtime.md)）。
- `status`と`doctor`は`actors`にAI actorごとのbackendと強制を出す（`ExecutionConfig::profiles`）。
  `dagq.toml`がworkerをcontainerにしない限り全てhostで、host実行がsandboxではないことを実行時にも明示する。
  roleの行の強制はそのactorへの最も弱い強制で、hostのworkerの行はClaudeのworkerがいつでも動きうるので`advisory`のまま。
  生きているsupervisorのどれかでCodexが使えるときだけ、workerの行にproviderごとの強制を足す。
  roleの行を分けずに足すのは、行の並びと各行の強制を読む既存の読み手（testとpluginのskill）を変えないため。
  適用先ごとのruntimeと強制は`runtimes`が出す（`runtime_executions`）。

### CLIでの解釈

`execute()`は環境のactorを読む前に、次の経路へ順に分岐し、これらでは環境のactor（`ActorContext::from_env`）を決めない。

- relayとreview program: queueを開かずに動く。
- 宛ての環境（serviceのsocketかqueueのbroker）があればクライアントモード（`client_mode`）で、tokenのprincipalをserviceが認可する（[Queue service](../queue-service.md#クライアントモード)）。
- plannerのhook（`session-event`）は報告をwrapperに渡す（spool）。
  inboxのhookはこの分岐に入らず、下の経路でinboxとして判定されてqueueを開き、eventはinboxのsessionのwrapperのactorで書く（[eventのactor](#eventのactor)）。
- session wrapper（`session`・`planner-session`）はwrapperのprincipalで動く。

それ以外のコマンドは`ActorContext::from_env`でactorを決める。
そのactorが`user`・`inbox`・`planner`でserviceの答えるコマンドならqueue serviceへ送り、残りは`check_access`の判定の後にqueueを開く（[Queue service](../queue-service.md#クライアントモード)）。
`ActorContext::from_env`の読み方は次のとおり。

- `DAGQ_ROLE`が無いか空なら`user`（人）。host実行の互換のためで、助言的（advisory）でありsandboxではない（ADR-t728-1の決定6）。
- `DAGQ_ROLE`がroleの値（`user`と、queue serviceのprincipalだけのrole `executor-telemetry`を除く）でなければerrorで止まり、queueを開かない（fail closed）。
  `DAGQ_RUN_ID`・`DAGQ_TASK_ID`が読めないときも同じ。
- 計画系のコマンドは全てのroleについてapplicationの層が判定し、拒んだ試みを`authorization_denied`に記録する（[Authorization](../authorization.md#計画系のコマンドapplication)）。
  対話と記録のコマンドと、runtimeの操作系のコマンド（[Authorization](../authorization.md#runtimeの操作系のコマンドapplication)）も同じ。
  runtimeのwrapperとhookは継いだworkerやplannerの環境のactorでなく、wrapperのprincipalでqueue serviceを通るので、workerとplannerはsessionを記録する権限を持たない（[Queue service](../queue-service.md#クライアントモード)）。
- headlessのjobは読み取りだけの制限を受け、状態を変えるコマンドは拒まれる。
  判定は`StaticPolicy`（[Authorization](../authorization.md)）で、どのjobにもCLIの書き込みを与えない。

### eventのactor

queueに書かれるeventは全て、書いたactorと、headless jobのverdictを適用したときの依頼元を`run_events`の列に持つ（ADR-t728-1の決定4。列は[SQLite persistence](../persistence.md)の`0044_event_actors.sql`）。
askとanswerが持つ書き手の欄とは別で、それらの値は変えない。

- 仕組み: `SqliteQueue`の接続ごとに`EventActors`（`src/infrastructure/event_actor.rs`）がactorを持ち、SQLの関数として登録する。
  `INSERT INTO run_events`は全てこの関数で列を埋めるので、書く場所にactorを渡さず、登録の無い接続での挿入は失敗する。
- 既定のactorはプロセスのactor（`set_process_actor`。`main`がコマンドの前に1回決める）で、決めていなければ（ライブラリを直接使うtest）`user`。
  `SqliteQueue::with_actor`で接続ごとに変えられる。
- プロセスのactorは呼び出し元の`ActorContext`（`DAGQ_ROLE`が無ければ`user`）。
  例外は制御側のプロセス（`supervise`、runとplannerのsession wrapper、session hook）で、eventはsessionのactorでなくsupervisorかwrapperのactorになる（`main.rs`の`event_actor`）。
  supervisorが起動する`observe`と`auto-update`には`supervisor:<pid>`の環境を渡すので、そのeventもsupervisorになる（observerのagentはobserverのactorのまま）。
- 着地（rebase・検証・squash・mainの更新・push）のeventはactorがIntegratorで、`requested_by`が依頼者。
  依頼と`integration_started`の記録の主は[Authorization](../authorization.md#着地とpushintegrator)と`IntegrationRequest`のdoc commentが持つ。
- review・recovery（終わったrunと生きているrun）・plan review・goal reviewのverdictをsupervisorが適用する間（`Supervisor::for_job`）、書かれるeventはactorがsupervisorのまま、`requested_by`にjobのactor idを持つ。
  抜けるときは（適用が失敗しても）入る前の値に戻すので、`for_job`が入れ子になっても内側を抜けた後の外側のeventは外側のjobの`requested_by`を持ち、外側を抜けると元の値（ふつうは無し）に戻る（Integratorの着地も同じく依頼者を入る前の値に戻す）。
  生きているrunのrecovery jobのverdictが適用されずにescalateするとき（`escalate`・confidence lowの`repair`・適用の時点で前提を満たさない`repair`）も、そのescalationの`recovery_finished`と開く ask（`stalled`）の`ask_opened`は`Supervisor::for_escalation`の中で書かれ、`requested_by`にjobのactor id（`recovery-job:<run>:<alert>:<attempt>`）を持つ（`Escalation::requester`）。
  jobの失敗と、alertのjobを使い切ったとき（`UsedUp`）のescalationはjobの依頼ではないので`requested_by`を持たない。
  review passの後の着地のeventは上のとおりIntegratorのactorで、`requested_by`が依頼したsupervisor。
- 読み方: `RunEvent.actor`で、migrationより前の行と古いバイナリが書いた行は`None`（JSONでは`actor`の欄が無い）。
  secretやpromptの全文は入れない（入るのはroleとidだけ）。

### headlessのjobのverdictと遷移

review・recovery・plan review・goal reviewのjobは、jobごとに別のroleのactorで（1つのroleにまとめない）、出力は型付きのverdict（`ReviewVerdict`など）として読むデータにすぎない。
遷移を決めるのはverdictを受けたsupervisorのRustのコードで（ADR-t728-1、ADR-t728-2の決定3）、写像は次のとおり決定的。

| job | verdict | supervisorが行うこと |
| --- | --- | --- |
| review-job（runの差分の判定） | `pass` | 衝突の事前検査のあと、sessionの終了を待って着地の段（`AfterExit::Land`）へ進めるだけ。着地の関数は呼ばない。着地は別のthreadの`land_integrating`が、rebase後の検証コマンドを含む自分の検査を通したときだけ行い、落ちればrunは`needs_session`になる |
| review-job（runの差分の判定） | `revise` / `concern` | 生きているsessionへの差し戻し（sessionが終わっていれば`approve_landing`のask） / `approve_landing`のask。runを通した`revise`の上限の後の差し戻しは収束の判定（`revise_limit_judged`）に通し、コードの指摘が無ければaskを開かずにpassの道で着地して理由をfollow_upのdraftにし、あれば`approve_landing`のask（[Review](review.md)の4a） |
| review-job（`already_satisfied`の主張の判定） | `pass`（`confidence: high`） | 着地の段へは進めない。閉じる時点のmainのheadを判定したsnapshotと確かめ直し、同じなら人に聞かずに[ADR-t1818-1](../../adr/2026-10-05-t1818-1-worker-claims-already-satisfied-and-runtime-closes-on-a-verified-snapshot.md)の閉じ方（runは`succeeded`、taskはcommitなしの`completed`）で閉じ、着地しない。進んでいれば古い判定では閉じずに新しいheadでverifyと判定をやり直し、やり直しの上限を超えたら`approve_close`のask（`satisfied_judgment::decide`、[Review](review.md#already_satisfiedの主張の判定)） |
| review-job（`already_satisfied`の主張の判定） | `pass`（`low`・確信度の欠落か未知の値）/ `revise` / `concern` | `approve_close`のask（理由は`scope`）にverdict・確信度・推奨・理由を載せて人に上げるだけで、taskを閉じない |
| review-job（`already_satisfied`の主張の判定） | jobの失敗（答えが無い） | 1回だけやり直し、それも答えが無ければ理由`recovery_failed`の`approve_close`のask（下の「壊れた出力はfail closed」） |
| recovery-job | `repair`（`confidence: high`） | `RecoveryVerdict::applies()`を通り、さらにalertごとの前提（終わったrunは`plan_ended`、生きているrunは`check_live`）を今も満たすactionだけを適用する。満たさなければaskにする |
| recovery-job | `repair`（`low`）/ `escalate` | 適用せず、actionを推奨として`decide`などのaskにする |
| plan-review-job | `pass` / `revise` / `concern` | 1つのtransactionで`ready`・plannerへの差し戻し・`approve_plan`のask。`concern`の`high`で`reason_category`が`null`の`ready` / `send_back`はpass / reviseとして適用する（[Plan review](plan-review.md#aiが決めるconcern未実装)、ADR-t451-1決定4）。`actions`は`pass`（と適用した`ready`）のときだけ許された3種を適用し、`reopen`はverdictに関わらず検査して適用し、上限を超えれば1件も適用せず、最終の判定でproposalを差し戻すかconcernのaskにする（passはrevise、reviseの上限を過ぎていればconcern）。`pass`での優先度の扱いは各taskの記録の由来と優先度の出どころで分かれる（[Plan review](plan-review.md#verdictと適用)） |
| goal-review-job | `achieved` / `gaps` / `ask` | goalを閉じる・gapをdraftにする・`approve_goal`のask（`gaps`の連続が上限を超えたら`ask`） |

`already_satisfied`の主張の判定のverdictは専用の入口（`ClaimJudgment::parse`）で読み、`pass`でも確信度を保つ。
通常のrunのreviewのverdictの読み取りはこの入口で変わらない（[Review](review.md)の「verdictの欄」）。

- 未知の欄を拒む: 4つのverdictと入れ子の型は`#[serde(deny_unknown_fields)]`で、未知の欄・未知のactionを含む出力は読めない出力として扱う。
- 例外は`PlanReviewVerdict`の`predictions`で、`serde_json::Value`のまま持つ。型にするとtaskの重さの見積もりの形が崩れただけでverdict全体が読めず、plan reviewが失敗するが、見積もりは記録するだけで遷移を変えない（ADR-0079決定2）ため。supervisorは`parse_predictions`で型付きの`TaskWeightPrediction`にしてから記録し、読めなければ記録しないだけで、生の値から遷移を決めない。
- 壊れた出力はfail closed: 読めないverdictで何かが着地・retry・`ready`・goalのcloseになることはない。
  reviewは答えの無いjobをそのjobだけ1回同じ入力でやり直し（`review_job_finished`）、それも答えが無ければ`review_failed`と`approve_landing`のask。
  `already_satisfied`の主張の判定も1回だけやり直し、それも答えが無ければ`recovery_failed`の`approve_close`のask。
  recoveryは`triage_failed`（`triage by hand`）。
  生きているrunのjobの失敗はそのalertのask（`reason_category: recovery_failed`。ADR-t609-1）。
  plan reviewは`plan_review_failed`（`plan review by hand`）でproposalは`submitted`のまま。
  goal reviewは`goal_review_failed`（`goal review by hand`）でgoalは開いたまま。
- 記録: verdictを適用したeventは上の「eventのactor」のとおりactorがsupervisor、`requested_by`がjobのactor id。
  reviewの`concern`と差し戻せない`revise`（上限の後に収束していない・sessionが終わっていた・切り替えられなかった・送れなかった）と、`pass`の後の衝突の事前検査が上限で人に聞く`approve_landing`のaskは、sessionの終了の後に開くが、`AfterExit::Ask`がverdictを返したreview jobのactor（`requested_by`）を持ち、askを開くときだけ`for_job`で包むので、その`ask_opened`もactorがsupervisor、`requested_by`が`review-job:<run>:<attempt>`になる。
  askの後にleaseを返すなどのeventは`for_job`の外で、`requested_by`を持たない。
  supervisorの引き継ぎ（`adopt.rs`）が履歴から`AfterExit::Ask`を組み立て直すときは、引き継ぐ（か直前の）`review_finished`のpayloadの`attempt`から同じjobを復元し、`attempt`が無ければ別のjobを名指ししないよう`requested_by`を付けない。
  引き継いだpassのrunの衝突の事前検査も同じjobの`for_job`の中で行うので、その`conflict_precheck`も生きている経路と同じく`requested_by`を持つ。
  差し戻しや衝突の依頼を生きているsessionが果たさなかった後（sessionが終わった・receiptの直しを送れなかった）のaskは、verdictを適用し終えた後のsupervisor自身の判断なので`requested_by`を持たない。
  jobの失敗のeventはjobの依頼ではないので`requested_by`を持たない（どのjobかはpayloadの`attempt`で分かる）。
  jobが開くaskの`asked_by`（`plan_review`・`goal_review`など）は変えない。
