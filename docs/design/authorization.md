---
id: design-authorization
type: design
title: Authorization
status: current
created: 2026-09-27
scope: runtime
related:
  - adr-t1394-1
  - adr-t1533-1
  - adr-t728-1
  - adr-t728-2
  - adr-t728-3
  - adr-t1228-1
  - adr-t1228-2
  - design-supervisor-lifecycle-roles
  - design-security
  - adr-t1662-3
---

# Authorization

actorが何をしてよいかは、`src/domain/authorization.rs`の`Authorizer`が決める（[ADR-t728-1](../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)の決定5）。
実装は静的なpolicyの`StaticPolicy`で、roleごとの許可の一覧（`grants(role)`）と、いくつかのroleのresourceの規則でできている。
一覧に無い組み合わせは拒む（default deny）。
resourceの持ち主や状態を規則が要るのに呼び出し元が知らない（`None`、`Resource::Unresolved`）ときも拒む（fail closed）。
判定はClaudeを起動せずにunit testできる。

host実行ではこの判定は助言的（advisory）で、sandboxでも隔離でもない。
どのプロセスも`DAGQ_ROLE`を偽れる（ADR-t728-1の決定6）。
信頼の区分・actorごとのcapabilityの要約・迂回できる経路・Podmanとqueue serviceへの道筋は[Security](security.md)。
actorの型と環境変数は[Roles](supervisor-lifecycle/roles.md#actors)。

## 固定バイナリを戻したときの窓

判定は、呼ばれた`dagq`のバイナリが持つpolicyで決まる。
固定バイナリを前のものに戻すと（[`install --rollback`](supervisor-lifecycle/install.md)、自動更新の見張りの`restore`（[Auto-update](supervisor-lifecycle/auto-update.md)））、新しいバイナリが起動してまだ走っているjobの`dagq`の呼び出しは戻したバイナリに届く。
戻したバイナリが`DAGQ_ROLE`の未知の値をfail closedにするparseを持たず、新しいroleを知らなければ、そのroleは制限を受けない。
jobのroleを知らない古いバイナリは`reviewer`だけを読み取りに制限し、ほかのjobの状態を変える呼び出しを拒まない（今のバイナリは知らないroleを拒む）。

- 窓は、戻したrenameから、新しいバイナリが起動したjobが居なくなるまで。
  supervisorの引き継ぎはexecの前にheadlessのjobを止めるが（[Handoff](supervisor-lifecycle/handoff.md)、[Headless job processes](supervisor-lifecycle/headless-job-processes.md)）、それは検証と着地の進行中のslotを進め終えた区切りなので、それまでjobは走り続ける。
  引き継ぎを受けなかったsupervisorのjobと、supervisorが死んで起動し直すまでに残ったjob（引き継ぐsupervisorが止める）は、終わるか止められるまで走る
- これは新しい弱点ではなく、host実行が助言的（advisory）であることの既知の限界の一例として受け入れる。
  `DAGQ_ROLE`は偽れるので、古いバイナリに新しいroleを教えるコードは足さない
- 気になるときは、戻す前に`down --wait`でjobを終わらせるか、戻した後に走っているjobを止める

## Capability

CLIからcapabilityとresourceへの入口は`src/main.rs`の`requests`。
定義は`src/domain/authorization.rs`の`Capability`、端末の認証は`application::login`。
予約のcapabilityは誰にも与えない。
CLIに無いjobの判定結果はデータとしてsupervisorが読み、遷移に写す（[Review](supervisor-lifecycle/review.md)）。

## Resource

種類と各々が持つ持ち主・状態は`src/domain/authorization.rs`の`Resource`のdoc commentが持つ。
持ち主の解決の入口は`infrastructure::proposals`の`owner_actor`。
持ち主が不明か、plannerのidがroleの名前だけなら所有を認めない（fail closed）。
CLIの引数だけで所有や状態を信頼せず、applicationがqueueから読んで判定する。

## Policy

外向きの人もuserで、grantsの後・resourceの前に登録簿で狭め、inboxに親の人の許可も適用する。

| 人の役割 | userの許可からの狭め |
| --- | --- |
| 管理者 | なし |
| member | `service.install`・`queue.admin`・`disk.relax`・`review.setup`・`review.setup_approve`・`task.ready_bypass_review`・`workspace.cleanup`・`run.recover`・`service.lifecycle`・`runner.share`を拒む |
| 当番（管理者かmemberに重ねる） | 重ねた役割に`run.recover`を戻す。grantsに無い許可は足さない |
| 役割なし（hostのsocketのpeer） | なし |

同じroleの行を併読する。

| role | 許すcapability | resourceの規則 |
| --- | --- | --- |
| user | 予約と`review.submit` `triage.submit` `landing.land` `landing.push` `request.decline` `replan.propose` `replan.park` `replan.apply` `telemetry.report` runnerの行と`image.publish`を除く全て | なし |
| user（続き） | `runner.register`・`runner.revoke`・`goal.landing_mark`・`credential.preference` | OIDCの人。登録と設定は自分、失効とgoalの印は持ち主か管理者（印はhostのuserも人のownerの無いgoalに） |
| user（続き） | `runner.share`・`runner.list`・`delegate.write` | 共有は管理者、一覧はmemberが自分、管理者が全て。代わりに答える人はOIDCの人の自分の分だけ |
| runner | `runner.declare`・`runner.claim`・`runner.heartbeat`・`runner.report` | 申告・pullは自分、報告・返却は割り当てrun |
| image-publisher | `image.publish`だけ | なし |
| inbox（続き） | `runner.list`・`delegate.write` | どちらも親の人の分だけ |
| inbox | userと同じだが、`review.setup_approve`・`queue_broker.record`・`runner.list`以外のrunnerの行・`identity.login`・`goal.landing_mark`・`credential.preference`は持たない | 人の役割を持つOIDCの発行元のある配置だけ、親の人の役割で狭め着地承認を拒む |
| planner | 読み取り・`queue.watch`・`queue.export`・`ci.read`・`goal.write`・`goal.close`・`task.write`・`follow_up.judge` | runはnoteだけでask不可。askは`planner_question`だけ、依頼のaskは担当planner本人だけ（`request.decline`と同じ所有の規則）。 |
| planner（続き） | `task.cancel`・`proposal.submit`・`proposal.withdraw`・`note.write`・`mark.write`・`ask.open`・`finding.resolve`・`finding.dismiss` | taskの変更（`task.write`・`task.cancel`）はdraft・submitted・readyのものだけ（状態が不明なら拒む）で、`revising`のproposalのtaskは持ち主だけ（不明なら拒む）。 |
| planner（続き） | `planner.open`・`request.decline`・`eval.request`・`eval.request_held_out`・`eval.read`・`eval.cases`・`service.lifecycle`・`service.install` | `revisit`（`task.write`）はdraftだけで、`draft_planner_exhausted`のあるdraftには付けられない（人・inboxだけが付ける。ADR-t1540-1）。 |
| planner（続き） | `queue.admin` | `follow_up.judge`は全状態に記録でき、移動はdraft・readyだけ。noteは全状態に書ける。`proposal.withdraw`は自分（actor id）が出したproposalだけ。`request.decline`はそのplannerが立てられた依頼（閉じていないplanner本人）だけで、plannerが分からなければ拒む |
| worker | 読み取り・`ask.open`・`note.write`・`replan.propose`・`eval.request`・`eval.read` | evalと`replan.propose`は自分のrunの分だけ。読み取り以外は自分のrun（`DAGQ_RUN_ID`）・そのtask（`DAGQ_TASK_ID`）・自分のrunのaskだけ。開けるaskは`worker_question`だけで、`--run`なら自分のrun、`--task`だけなら自分のtask（`DAGQ_RUN_ID`の無いworkerは何も持たない） |
| review-job | 読み取り・`review.submit` | `review.submit`は自分のrunだけ |
| recovery-job | 読み取り・`triage.submit` | `triage.submit`は自分のrunだけ |
| plan-review-job・goal-review-job・throughput-review-job | 読み取りだけ | throughput-review-jobは[スループットの見直し](supervisor-lifecycle/throughput-review.md) |
| setup-review-job | 読み取りだけ | なし（[setup-review](supervisor-lifecycle/setup-review.md)） |
| observer | 読み取り・`queue.watch`・`ci.read`・`finding.record`・`finding.resolve`・`finding.ask`・`eval.read` | `finding.resolve`と`finding.ask`はfindingだけ |
| supervisor | 読み取り・`queue.watch`・`queue.export`・`ci.read`・`goal.close`・`task.cancel`・`task.ready`・`note.write`・`ask.open`・`ask.close`・`session.run`・`review.prepare`・`finding.record`・`finding.resolve`・`finding.dismiss` | なし。`queue.admin`は自動更新の確認のため（下の`auto-update`）。`ask.answer`・`task.ready_bypass_review`・`landing.land`・`landing.push`・`workspace.cleanup`とevalの依頼は持たない |
| supervisor（続き） | `observe.run`・`planner.open`・`eval.read`・`scheduler.supervise`・`run.recover`・`service.lifecycle`・`service.install`・`queue.admin`・`landing.request`・`replan.park`・`replan.apply`・`token.issue`・`queue_broker.record` | 同上 |
| wrapper | 読み取り・`session.run`・`session.record` | 自分のrunかplanner（tokenのprincipalのactor id）だけ（ADR-t3102-1決定4） |
| integrator | 読み取り・`landing.land`・`landing.push` | なし |
| executor-telemetry | `telemetry.report`だけ | tokenの対象に制御側が登録したsessionだけ |

旧値の`DAGQ_ROLE=reviewer`はreview-jobとして読む（[Roles](supervisor-lifecycle/roles.md#cliでの解釈)）。

人の出どころの優先度はuserとinboxだけが変える。
例外は`out_of_scope`の判断を記録するAIのactorの移動で、goalから継いだだけの値は移動先を継ぎ、taskに直接付いた値は残る（[ADR-t2373-1](../adr/2026-10-12-t2373-1-out-of-scope-follow-up-move-takes-the-destination-goals-priority.md)）。
人の優先度を守る判定の入口は`check_priority_change`、所属を動かす制約は[所属の判断](follow-up-membership.md)。
askのkindをroleに制限する入口は`opens_ask`。

拒否の情報の制限は`AuthorizationError`を読む。

## 適用の範囲

端末のlogin・logout・whoamiの認可は`identity.login`。
入力不備・資格不在は認可の拒否ではなく、queueへ送らない。

### 計画系のコマンド（application）

計画系の判定の入口は`src/application/commands/planning.rs`の`Planning`で、全てのroleについてstoreの変更より先に判定する。
CLIはparseと出力を担い、storeの変更を直接呼ばない。
認可に使った状態・proposal・goalのowner（`goal landing-mark`）はstoreが同じtransactionで照合し、食い違えば何も変えずに拒む。
照合が防ぐ取り違えは`TaskAuthorization`、editの制約は`PlanningStore::edit_task`のdoc commentが持つ。
roleがcapabilityを持たないときの、storeを読む前の拒否は`Planning::refuse_ungranted`が持つ。
終了runのverifyを人とinboxが直せる約束は[ADR-t883-1](../adr/2026-09-30-t883-1-edit-ended-run-verification-before-inherited-retry.md)。
再開・復旧を待つtaskの優先度は[ADR-t1850-1](../adr/2026-10-06-t1850-1-resumes-recovery-jobs-and-claims-share-one-line-by-effective-priority.md)、draftの再検討は[ADR-t1540-1](../adr/2026-10-05-t1540-1-a-kept-draft-returns-to-runtime-planners-at-its-revisit-time.md)。

拒んだときは、queueのevent `authorization_denied`（taskにもgoalにも紐づかないqueueのevent。actorの列は拒まれた呼び出し元）を記録し、`AuthorizationError`を返す。
payloadは`src/application/commands/mod.rs`の`Gate`の記録と`Resource::record`が決める。
記録に失敗しても拒否は拒否のまま返す。
CLIのerrorの形は`src/main.rs`の`error_json`が持つ。
observerのこのeventは、observerの次の起動を決める「自分以外のevent」に数えない。

### 対話と記録のコマンド（application）

対話と記録の入口は`src/application/commands/dialogue.rs`の`Dialogue`で、全てのroleについてstoreの変更より先に判定する。
portとadapterの境界は`DialogueStore`と`infrastructure::dialogue::DialogueQueue`。
拒否の記録とerrorの共通部分は`Gate`が持つ。
記録の書き手は呼び出し元の値を信頼せずactorから入れる。
answerの代行の境界は[ADR-t728-3](../adr/2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)。

計画の依頼の入口は`commands::requests::Requests`（[ADR-t1394-1](../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)）。
入力を読む前の拒否とserviceへ送る境界は[計画のユースケース](queue-service-host-cli.md#計画のユースケース)、CLI側の入口は`main.rs`の`check_access`。
再計画の判定は`commands::replans::Replans`、保留の判定は`commands::holds::Holds`が持つ。

#### answerの権限の出どころと承認の分類

answerの権限は`AnswerAuthority`（ADR-t728-3決定2）。
値はactorの型（`ActorContext::answerer`）から決め、answerの文やinboxのpromptから推さない。

記録の綴り・runtimeの非承認は`Answerer`のdoc comment、承認の分類は`answer_approves`。
OIDCの発行元のある配置のapprove_landingの着地の承認は、最近のloginの人のtokenだけを受け、inboxとtokenの無いuserを拒む。
policyが配置を取得し、不明なら拒む。
着地を承認しないanswerと他のaskはこの制限を受けず、発行元の無い配置の許否と記録も変わらない。

### runtimeの操作系のコマンド（application）

操作系のcapabilityとresourceの入口は`src/application/commands/operations.rs`の`Operation::request`。
CLIはqueueの場所を決めた直後、queueやログを開く前に`operations::authorize`で全てのroleを判定する。
着地はこの入口の後で[Integrator](#着地とpushintegrator)がもう一度判定する。

workspaceを閉じる手順の入口は[dagq-recoverのsession](../../plugins/claude-dagq/skills/dagq-recover/reference/session.md#the-commands)。
終わったrunのwrapperの停止はsupervisorの[自分の掃除](supervisor-lifecycle/background-recovery-job.md#終わったrunの掃除)が担う。
plannerはruntimeだけが立て、人が頼む相手ではないので、起動・停止と更新は人かinboxが担う（[Roles](supervisor-lifecycle/roles.md)）。
続きの依頼の境界は[ADR-t1533-1](../adr/2026-10-03-t1533-1-follow-up-requests-go-to-headless-planners-by-planner-id-and-no-planner-close.md)。

拒否はserviceが呼び出し元のprincipalで記録し、CLIはqueueを開かない（`CommandDenials`と[hostのCLI](queue-service-host-cli.md#送る範囲)）。
queueが無い・serviceが答えないときは記録できず、拒否は拒否のまま返す。
sessionへの操作はdagqのCLIを通す（[ADR-t1228-1](../adr/2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)）。
Codexのinboxのhookもこの約束を持つ（[ADR-t2159-1](../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定5）。
設定とguardrailの見え方は[`up` / `down`](supervisor-lifecycle/up-down.md)と[Security](security.md)。

#### 制御側の起動としての`supervise`と`auto-update`

`supervise`と`auto-update`はruntimeの制御側（信頼する制御側。ADR-t728-1の決定2）のプロセスを始めるコマンドで、呼び出し元の環境で判定する:

- `supervise`は`scheduler.supervise`で判定する。
  launchdのsupervisorは`DAGQ_ROLE`を持たない（user）ので、`up`を打ったのがplannerでも起動は通る（登録済みのin-cmuxのものは`DAGQ_ROLE=supervisor`で動く）。
  exec の引き継ぎは同じ環境のまま自分をexecするので、同じroleで判定される。
  起動したプロセスのeventのactorは、呼び出し元ではなく`supervisor:<pid>`（`event_actor`）
- `auto-update`と`release-update`は、supervisorが自分の環境（`supervisor:<pid>`）とtokenで起動する。
  `install`とjobは新しいbuildの確認を同じ環境の子として打つので、roleは`queue.admin`と`service.lifecycle`も持つ。
  子の`up`は`install`だけが呼び出し元のprincipalで打ち、更新のjobは打たずlaunchdに任せるか打つ`up`をaskで人とinboxに知らせる（[supervisorを戻す](supervisor-lifecycle/auto-update.md#supervisorを戻す)）。
  人とinboxとplannerも`install`と同じ権限で打てる（手順の再現と復旧）。
  `observe`（`observe.run`）もsupervisorが自分の環境で起動し、その中のagentだけがobserverになる

### 着地とpush（Integrator）

着地（rebase・再検証・squash・mainの更新）とpushは`src/application/integrate.rs`の`Integrator`だけが行う（[ADR-t728-2](../adr/2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)）。
Integratorはrole `integrator`のactor（`integrator:<pid>`）。
`Integrator::acting_as(actor)`は`StaticPolicy`がこの2つを許すactorでなければ拒み、着地の関数はmoduleの外から呼べない。

着地の依頼と記録の境界は`IntegrationRequest`、着地前の検査は`landing_refusal`、pushの許可の証拠は`PushGrant`のdoc commentが持つ。
reviewのpassは着地の必要条件で、この後のreceipt・rebase・検証・scopeの関門を免除しない（ADR-t728-2の決定3）。

host実行ではIntegratorはsupervisorや`integrate`と同じプロセスとユーザーで動き、この境界は論理的なもの（ADR-t728-2の決定4）。

### queue serviceのユースケース（service側）

serviceはtokenのprincipalをactorとして認可し、一覧に無い操作を拒む（default deny）。
ユースケースの入口は`src/application/queue_service.rs`の`QueueService`で、対話にはCLIと同じapplicationの判定を使う（ADR-t1233-1決定4）。
token発行の前提と発行が同じtransactionに属する約束は`issue_token`のdoc commentと[ADR-t3102-1](../adr/2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)。
supervisorの操作の判定は`supervisor_service::answer`、integratorが自分の着地だけを操作する制約は`integrator_scope`。
wrapperの操作と制御側のコマンドの判定は`QueueService`のdoc commentと[制御側のコマンド](queue-service-host-cli.md#制御側のコマンド)。

### ほかのコマンド（CLIの入口）

queueの読み取り・`watch`などはqueueの状態を変えない。

- 宛ての環境のあるworker・job・observerはserviceのユースケースだけを送り、ほかはCLIが拒みDBを開かない（[クライアントモード](queue-service.md#クライアントモード)）。
- 宛ての環境の無いuser・inbox・plannerはserviceが答える読み取り・`watch`・`status`・`report`・`graph --out`をserviceへ送り、serviceがprincipalで認可する（読み取りは上の節、後の2つは`Gate`の`queue.export`）。
  CLIは届かなくてもDBを開かない（[hostのCLI](queue-service-host-cli.md)）。
- 残り（3つのroleのserviceが答えないものと、ほかのrole）は`check_access`が（default deny）`requests`の全てで`StaticPolicy`に通す。
  拒んだときだけ`ServiceDenials`でserviceが呼び出し元のprincipalの`authorization_denied`に記録する。
  通った後、supervisorが起動した`observe`・`throughput-review`はsupervisorのtokenでserviceを通り、送る経路の無いものはCLIが拒みDBを開かない（制御側のコマンドとDBの例外を含め[CLIでの解釈](supervisor-lifecycle/roles.md#cliでの解釈)）。

## Claudeのpermissions.deny（guardrail）

規則を作る入口は`src/application/execution.rs`の`permission_deny`。
guardrailが迂回できる限界と、規則をwildcardで広げない約束は[Authorization guardrail](authorization-guardrail.md)。

## CIの見張り（ADR-t1920-1）

[ADR-t1920-1](../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)と[CI watch](supervisor-lifecycle/ci-watch.md)。
読むコマンド`dagq ci failures`のcapabilityは`ci.read`で、誰が持つかはPolicyの表のとおり（一覧はworkerとrunの全体のreviewにはpromptとrun directoryのファイルで渡す）。
guardrailの対応は`DAGQ_COMMANDS`、findingの判断の入口は`Dialogue`。
