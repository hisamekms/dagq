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
  - design-authorization
  - adr-t3985-1
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
  OIDCの発行元のある配置ではapprove_landingの着地の承認を書けず、人が自分の最近のloginの資格で答え、inboxの権限は親の人の役割の範囲に狭まる。
  発行元の無い配置では今どおり全てのaskのanswerを書き戻す（[Authorization](../authorization.md#policy)、[ADR-t3985-1](../../adr/2026-11-03-t3985-1-team-version-identity-by-oidc-human-roles-on-staticpolicy-runner-registration-per-person-inbox-and-no-delegated-landing-approval.md)）。
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

runtimeのactorの型と信頼の区分は`src/domain/actor.rs`が入口で、信頼はroleだけから決め、promptや名前から推し量らない（[ADR-t728-1](../../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)決定2・4）。

### 環境変数

AI actorは全てroleとactor idを環境に持ち、組み立ては`src/application/actor_executor.rs`の`actor_env`が入口になる。
actorごとの起動元、queueの宛てと変数を足す順序もそのdoc commentが持つ。
`[run.env]`では`DAGQ_`の予約された名前を上書きできない。
runのsession wrapper自体にはagentのqueueの宛てを渡さず、制御側のprincipalで動かす（[Queue service](../queue-service.md#クライアントモード)）。
登録済みのin-cmuxのsupervisorのworkspaceはAI actorではないが同じ形の環境を持つ。

### actorの起動（ActorExecutor）

runtimeが起動するAI actorは全て`src/application/actor_executor.rs`の`ActorExecutor`を通り、整合しないspecでは起動しない（fail closed）。
整合の検査は`ActorExecutionSpec::check`、hostで使う部品と拒否は`HostActorExecutor`が入口になる。
hostのspecは記録と整合の検査だけで隔離ではなく、プロセスはこのユーザーにできることを全てできる（ADR-t728-1決定6）。
制限時間もhostでは呼び出し元のtimerとdeadlineが守る。
Claudeのsettingsをroleから決める入口は`src/infrastructure/adapters.rs`の`agent_settings`で、provider固有の仕組みはadapterが持つ（[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）。
executorの外で起動する制御側のprocessとtestのstubはAI actorの起動とは区別する（actor_executorのmodule doc）。
登録済みのin-cmuxのsupervisorのworkspaceも制御側に当たる。

### 実行のbackendとenforcement

実行場所と強制の型、status・doctorの表示を知る入口は`src/application/execution.rs`で、値と欄の意味は各定義のdoc commentが持つ。
隔離はsandboxだけで、hostのCodex workerの書き込みとsignalの制限も隔離ではない（[ADR-t813-3](../../adr/2026-09-28-t813-3-codex-worker-permissions.md)決定7）。
containerへ置いたものをhostへ黙って戻さず、起動できなければ拒む（fail closed）。
actorとしてcontainerへ置くのはworkerのturnだけで、session wrapperはhostの制御側に残る（[Container runtime](../container-runtime.md#workerの実行)）。
verificationのコマンドはactorではない（[Container runtime](../container-runtime.md#verificationの実行)）。
statusとdoctorはhost実行がsandboxでないことを実行時にも示し、その行の互換とprovider別の強制の読み方は`ExecutionConfig::profiles`が持つ。

runnerのcapabilityと所有の規則は[Authorization](../authorization.md#capability)のCapability・Policyが入口になる。
人のClaudeの資格情報の設定の`credential.preference`はuserだけが持ち、inbox・planner・job・runnerは拒まれる（同じPolicy）。

### CLIでの解釈

認証・認可とqueueを開く境界の分岐は`src/main.rs`の`execute`が入口になる。
環境のactorを読まずtokenのprincipalで動く経路も、そのdoc commentが案内する。
環境からのactorの読み方は`src/domain/actor.rs`の`ActorContext::from_env`で、読めない環境ではfail closedにする。
queue serviceのprincipalだけのrole（runner・executor-telemetry・image-publisher）は`DAGQ_ROLE`の値として受けない。
roleが無い環境を人として扱うのはhost実行の互換の約束であり、sandboxではない（ADR-t728-1決定6）。
宛ての無いworker・job・observerはqueueを開かずに断る（[Queue service](../queue-service.md#クライアントモード)）。
headlessのjobはCLIの読み取りだけを許し、書き込みを与えない（[Authorization](../authorization.md)）。
wrapperのprincipalによるsessionの記録はagentの権限ではない。

### eventのactor

eventは全て書いたactorを持ち、askとanswerの書き手の欄とは別に記録する（ADR-t728-1決定4）。
接続とSQLへの受け渡しは`src/infrastructure/event_actor.rs`、制御側のeventをsessionのactorにしない境界は`src/main.rs`の`event_actor`が入口になる。
着地のactorと依頼元は[Authorization](../authorization.md#着地とpushintegrator)と`IntegrationRequest`のdoc commentが持つ。
verdictの適用中はactorがsupervisor、依頼元がjobで、範囲を抜けると適用が失敗しても入る前の依頼元に戻す（`Supervisor::for_job`）。
recoveryのescalationの依頼元は`Supervisor::for_escalation`、reviewの終了後のaskと引き継ぎは`AfterExit`のdoc commentが入口になる。
jobの失敗と、判定の適用後のsupervisor自身の判断にはjobの依頼元を付けない。
古い行の`RunEvent.actor`は`None`で、JSONには欄を出さない。
actorにはroleとidだけを入れ、secretやpromptの全文を入れない。

### headlessのjobのverdictと遷移

jobはそれぞれ別のroleのactorで、verdictはデータにすぎず、遷移を決めるのはsupervisorのRustのコードである（ADR-t728-1、[ADR-t728-2](../../adr/2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)決定3）。
判定から適用への入口は[Review](review.md)、[Triage](triage.md)、[Plan review](plan-review.md)、[Goal review](goal-review.md)が案内し、場面ごとの写像は適用する定義のdoc commentが持つ。
再計画の診断の写像は`src/application/supervise/execution/replan_diagnosis.rs`の`apply_verdict`、already_satisfiedの主張の判定は`ClaimReviewWatch`が入口になる（[ADR-t1818-1](../../adr/2026-10-05-t1818-1-worker-claims-already-satisfied-and-runtime-closes-on-a-verified-snapshot.md)）。
通常のreview・recovery・plan review・goal reviewのverdictは、未知の欄やactionを含む出力を拒む。
読めないverdictで着地・retry・ready・goalのcloseが起こることはない（fail closed）。
見積もりの形が壊れたために判定全体を失敗させない例外は`PlanReviewVerdict::predictions`のdoc commentが持つ（[ADR-0079](../../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)決定2）。
