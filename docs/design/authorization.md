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
  どのプロセスも`DAGQ_ROLE`を偽れる（ADR-t728-1の決定6）ので、古いバイナリに新しいroleを教えるコードは足さない
- 気になるときは、戻す前に`down --wait`でjobを終わらせるか、戻した後に走っているjobを止める

## Capability

| 群 | capability | 対応するCLI |
| --- | --- | --- |
| 読み取り | `queue.read` | `locate` `list` `show` `candidates` `graph`（`--out`なし） `status` `asks` `events` `timeline` `stats` `kpi` `forecast` `doctor` `notes` `marks` `findings` `search` `related` `proposal list/show` `planners` `requests` `replans` `lint` `goal list/show` `observe --history` `observe --input` |
| | `queue.watch` | `watch` |
| | `ci.read` | `ci failures`（着地先のbranchで既に落ちているtestの一覧。[CI watch](supervisor-lifecycle/ci-watch.md)） |
| | `queue.export` | `graph --out` `report`（ファイルを書く） |
| 計画 | `goal.write` | `goal add` `goal edit` |
| | `goal.ready` | `goal ready` |
| | `goal.close` | `goal close` |
| | `goal.review_request` | `goal review` |
| | `task.write` | `add` `edit` `draft` `set-goal` `set-paths` `set-priority` `revisit` `dependency add/remove` |
| | `follow_up.judge` | `judge-follow-up` |
| | `task.verify_edit` | 終了runを持つ`in_progress` taskの`edit --verify` / `edit --no-verify` |
| | `task.cancel` | `cancel` |
| | `task.ready` / `task.ready_bypass_review` | `ready` / `ready --bypass-review`・`ready --inherit` |
| | `proposal.submit` / `proposal.withdraw` | `submit` / `proposal withdraw` |
| | `note.write` / `mark.write` | `note` / `mark` |
| workerの通信 | `ask.open` | `ask`（`blocked`をfindingに紐づけるもの以外） |
| | `session.run` | `session`（runのsession wrapper） `planner-session` |
| | `session.record` | `session-event`（`--run`を含む） |
| review / triage | `review.submit` / `triage.submit` | CLIには無い。jobのverdictはデータとしてsupervisorが読み、遷移に写す |
| | `review.prepare` | `review`（review.mdを書く） |
| 観察 | `finding.record` / `finding.resolve` / `finding.dismiss` | `finding record` / `finding resolve` / `finding dismiss` |
| | `finding.ask` | `ask --kind blocked --finding` |
| | `observe.run` | `observe`（`--history`・`--input`を除く） |
| 人との対話 | `ask.answer` / `ask.close` | `answer` / `ask close` |
| | `planner.open` | `plan` |
| | `request.record` / `request.decline` | `request add` / `request decline`（計画の依頼。ADR-t1394-1の決定3・6、[`plan` / `planners`](supervisor-lifecycle/plan-planners.md#inboxからの計画の依頼)） |
| | `replan.*` | `replan request` `replan propose` / runtimeの保留と置換（[再計画](../plans/task-replanning.md)） |
| | `task.hold` | `hold` `release` |
| | `screen.read` / `screen.send` | `run screen` `planner screen` `run log` `planner log` / `run send` `planner send`（sessionの画面を読む・送る。workerのrunには画面が無く、`run screen`はどのrunにも理由とturnのlogのCLI（`run log`）を示して拒み（[ADR-t1433-3](../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）、`run send`はどのrunにも拒む。backgroundのsessionのlogを読む（ADR-t1404-1決定6）。[ADR-t1228-1](../adr/2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)） |
| | `planner.request` | `planner request`（開いている非対話のruntimeのplannerへの続きの依頼を次のturnとして置く。[ADR-t1533-1](../adr/2026-10-03-t1533-1-follow-up-requests-go-to-headless-planners-by-planner-id-and-no-planner-close.md)、[`plan` / `planners`](supervisor-lifecycle/plan-planners.md#続きの依頼と非対話のplannerのcli)）。plannerを閉じるCLIは無い |
| eval | `eval.request` / `eval.request_held_out` / `eval.rerun` / `eval.read` / `eval.cases` | `agent eval`（dev / holdout・production / `--rerun`） / 読み取り / `agent cases` `agent decide`（[Agent eval](agent-eval.md)） |
| schedulerの遷移 | `scheduler.supervise` | `supervise` |
| | `run.recover` | `recover` |
| | `workspace.cleanup` | `run close-workspaces`（認可の後に理由を示して拒む。[ADR-t1433-3](../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)の決定3） |
| | `service.lifecycle` | `up` `down` `inbox` |
| | `service.install` | `install` `auto-update` |
| | `queue.admin` | `init` `migrate` `rebind` |
| | `review.setup` / `review.setup_approve` | `setup-review` / `setup-review --yes`（[setup-review](supervisor-lifecycle/setup-review.md)） |
| 着地 | `landing.request` | `integrate`（Integratorへの依頼。ADR-t728-2） |
| | `landing.land` / `landing.push` | Integratorの内側 |
| 計測 | `telemetry.report` | CLIには無い（queue serviceの`telemetry_report`） |
| 予約 | `reserved.filesystem_read` `reserved.filesystem_write` `reserved.network` `reserved.secret_read` | 後の段（sandbox）。この段では誰にも与えない |

CLIのコマンドからcapabilityとresourceへの写しは`src/main.rs`の`requests`で、matchに既定の枝を置かないので、コマンドを足すと写しを書くまでcompileが通らない。
`queue.read`だけのコマンドはqueueをread-onlyの接続で開く（`reads_only`）。

## Resource

種類と各々が持つ持ち主・状態は`src/domain/authorization.rs`の`Resource`のdoc commentが持つ。
CLIの写し（`requests`）は引数にあるものだけを入れ、持ち主と状態は`None`のまま渡す。
計画系のコマンドはapplicationの層（下の[適用の範囲](#適用の範囲)）がqueueからtaskの状態と所属のproposal、proposalの持ち主を読んで埋めてから判定する。
proposalの持ち主は、`submit`（新しいproposalと、差し戻しの後の出し直し）が出したactorとする（入口は`infrastructure::proposals`の`owner_actor`）。
plan reviewが人を待つproposalをそのまま出し直す`submit --proposal`は持ち主を変えない。
plan reviewが差し戻して`revising`のproposalは、reviseを送ったplannerを持ち主とする。
持ち主を記録していないproposalは、plannerは取り下げられない（fail closed。userとinboxは取り下げられる）。
roleの名前だけでactorを名指さないplannerのidは、どのproposalの持ち主にもならない。
`revising`のproposalのtaskは、plannerはその持ち主のときだけ変えられ、持ち主が記録に無いかroleの名前だけなら拒む（fail closed）。

## Policy

| role | 許すcapability | resourceの規則 |
| --- | --- | --- |
| user | 予約と`review.submit` `triage.submit` `landing.land` `landing.push` `request.decline` `replan.propose` `replan.park` `replan.apply` `telemetry.report`を除く全て | なし |
| inbox | userと同じ（人の言葉での代行。ADR-t728-3の決定1）だが、`review.setup_approve`は持たない | なし。人自身の操作との区別は記録が持つ |
| planner | 読み取り・`queue.watch`・`queue.export`・`ci.read`・`goal.write`・`goal.close`・`task.write`・`follow_up.judge`・`task.cancel`・`proposal.submit`・`proposal.withdraw`・`note.write`・`mark.write`・`ask.open`・`finding.resolve`・`finding.dismiss`・`planner.open`・`request.decline`・`eval.request`・`eval.request_held_out`・`eval.read`・`eval.cases`・`service.lifecycle`・`service.install`・`queue.admin` | runにはnote（`note --run`）だけを書け、ほかは何もできない（runに紐づくaskも開けない）。開けるaskは`planner_question`だけで、依頼に紐づくもの（`ask --request ID`）はその依頼の閉じていないruntimeのplannerのactor idが自分のときだけ（`request.decline`と同じ規則。plannerが分からなければ拒む）。taskの変更（`task.write`・`task.cancel`）はdraft・submitted・readyのものだけ（状態が不明なら拒む）で、`revising`のproposalのtaskは持ち主だけ（不明なら拒む）。`revisit`（draftの再検討の時刻、`task.write`）はdraftだけで（storeがどのroleにも求める）、`draft_planner_exhausted`のあるdraftには付けられない（人・inboxだけが付ける。ADR-t1540-1）。`follow_up.judge`は全状態のfollow_upに記録でき、所属を動かすのはdraft・readyだけ。noteはどの状態のtaskにも書ける。`proposal.withdraw`は自分（actor id）が出したproposalだけ。`request.decline`はそのplannerが立てられた依頼（依頼の閉じていないruntimeのplannerのactor idが自分）だけで、plannerが分からなければ拒む |
| worker | 読み取り・`ask.open`・`note.write`・`replan.propose`・`eval.request`・`eval.read` | evalと`replan.propose`は自分のrunの分だけ。読み取り以外は自分のrun（`DAGQ_RUN_ID`）・そのtask（`DAGQ_TASK_ID`）・自分のrunのaskだけ。開けるaskは`worker_question`だけで、`--run`なら自分のrun、`--task`だけなら自分のtask（`DAGQ_RUN_ID`の無いworkerは何も持たない） |
| review-job | 読み取り・`review.submit` | `review.submit`は自分のrunだけ |
| recovery-job | 読み取り・`triage.submit` | `triage.submit`は自分のrunだけ |
| plan-review-job・goal-review-job・throughput-review-job | 読み取りだけ | throughput-review-jobは[スループットの見直し](supervisor-lifecycle/throughput-review.md)（ADR-t996-1） |
| setup-review-job | 読み取りだけ | なし（[setup-review](supervisor-lifecycle/setup-review.md)） |
| observer | 読み取り・`queue.watch`・`ci.read`・`finding.record`・`finding.resolve`・`finding.ask`・`eval.read` | `finding.resolve`と`finding.ask`はfindingだけ |
| supervisor | 読み取り・`queue.watch`・`queue.export`・`ci.read`・`goal.close`・`task.cancel`・`task.ready`・`note.write`・`ask.open`・`ask.close`・`session.run`・`review.prepare`・`finding.record`・`finding.resolve`・`finding.dismiss`・`observe.run`・`planner.open`・`eval.read`・`scheduler.supervise`・`run.recover`・`service.lifecycle`・`service.install`・`queue.admin`・`landing.request`・`replan.park`・`replan.apply` | なし。`queue.admin`は自動更新が新しいバイナリで`install`と同じ確認（`migrate --check`・`migrate`・使い捨てのqueueの`init`）をするため。`ask.answer`・`task.ready_bypass_review`・`landing.land`・`landing.push`・`workspace.cleanup`（終わったrunに残ったbackgroundのwrapperは自分の掃除で止める）とevalの依頼は持たない |
| wrapper | 読み取り・`session.run`・`session.record` | 自分のrunかplanner（tokenのprincipalのactor id）だけ。queue serviceの`session_run`・`session_record`で判定する（[ADR-t3102-1](../adr/2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)決定4） |
| integrator | 読み取り・`landing.land`・`landing.push` | なし |
| executor-telemetry | `telemetry.report`だけ | tokenの対象に制御側が登録したsessionだけ |

旧値の`DAGQ_ROLE=reviewer`はreview-jobとして読む（[Roles](supervisor-lifecycle/roles.md#cliでの解釈)）。
jobの環境には`DAGQ_RUN_ID`が無いので、`review.submit`・`triage.submit`は持ち主が分からず拒まれる。
verdictはデータとしてsupervisorが読む。

人の出どころの優先度はuserとinboxだけが変える。
例外は`out_of_scope`の判断を記録するAIのactorの移動で、goalから継いだだけの値は移動先を継ぎ、taskに直接付いた値は残る（[ADR-t2373-1](../adr/2026-10-12-t2373-1-out-of-scope-follow-up-move-takes-the-destination-goals-priority.md)）。
他のroleの`set-priority`・`goal edit`と依頼と違う優先度の`goal add`・`add`はstoreが同じtransactionで拒み、`authorization_denied`を記録し、`denied`の無い人の値を名指すerrorを返す。

askのkindはroleごとに決まる（`opens_ask`）。
observerの`ask --kind blocked --finding`は`ask.open`ではなく`finding.ask`で、finding（`Resource::Finding`）に対して判定する。
kindがroleに合わなければ拒む。

`AuthorizationError`はroleと拒んだcapabilityと理由（`DenyReason`）だけを出し、actor id（session idを含みうる）やresourceのid・本文を出さない。

## 適用の範囲

### 計画系のコマンド（application）

計画系のコマンド（下の表）は、`src/application/commands/planning.rs`の`Planning`が全てのroleについて判定してからstoreを呼ぶ。
CLI（`src/main.rs`の`execute()`）はparseと出力だけをし、これらのコマンドでstoreの変更を直接呼ばない。
`Planning`は呼び出し元の`ActorContext`・`Authorizer`・port `PlanningStore`を受け取り、コマンドごとに次のcapabilityとresourceを問う。

| コマンド | capability | resource |
| --- | --- | --- |
| `add` | `task.write` | `--goal`のgoal、無ければqueue |
| `edit` `set-goal` `set-paths` `draft` `dependency add/remove` | `task.write` | task（queueにある状態） |
| `set-priority` | `task.write` | task（queueにある状態）。`in_progress`のtask（再開待ちか復旧を待つrunを持つもの）にも置け・外せ、次の再開と復旧jobの順に効く（[ADR-t1850-1](../adr/2026-10-06-t1850-1-resumes-recovery-jobs-and-claims-share-one-line-by-effective-priority.md)の決定7）。`in_progress`のtaskではplannerは`not on this resource`で拒まれ、user・inboxだけが通る。`completed` / `canceled`は全てのroleでdomainが拒む |
| `revisit ID --at` / `revisit ID --clear` | `task.write` | task（queueにある状態）。draftの再検討の時刻を付け・変え・外す（[ADR-t1540-1](../adr/2026-10-05-t1540-1-a-kept-draft-returns-to-runtime-planners-at-its-revisit-time.md)）。storeは同じtransactionでtaskが`draft`であることを確かめ（draft以外は全てのroleに拒む）、`user` / `inbox`以外が`draft_planner_exhausted`のあるdraftに付けるのを拒む |
| `judge-follow-up` | `follow_up.judge` | task（状態による制限なし。follow_upの出どころと必須の欄・遷移をstoreが同一transactionで検査） |
| `edit --verify` / `edit --no-verify`（`in_progress`のみ） | `task.verify_edit` | task（最新runが終了し、生きているrunが無いことをstoreが同一transactionで検査） |
| `ready` / `ready --bypass-review`・`ready --inherit` | `task.ready` / `task.ready_bypass_review` | task（状態） |
| `cancel`（`--duplicate-of`を含む） | `task.cancel` | task（状態） |
| `submit` / `submit --proposal ID` | `proposal.submit` | queue / proposal（持ち主） |
| `proposal withdraw` | `proposal.withdraw` | proposal（持ち主） |
| `goal add` / `goal edit` / `goal ready` / `goal close` / `goal review` | `goal.write` / `goal.write` / `goal.ready` / `goal.close` / `goal.review_request` | queue / goal |

終了runのverifyだけはuserとinboxが`task.verify_edit`で直せる（[ADR-t883-1](../adr/2026-09-30-t883-1-edit-ended-run-verification-before-inherited-retry.md)）。
planner・worker・jobはこの権限を持たない。
`required_evidence`と`paths`は変更できず、修正は`task_edited`とそのactorに残る。
`edit`のcapabilityは`Planning`がtransactionの外で読んだtaskの状態から選ぶので、`Planning`はその状態と所属のproposal（id・状態・持ち主）をstoreの`edit_task`に渡し、storeは同じtransactionで読んだものと照合する。
食い違えば（例: `ready`で`task.write`を通った後にclaimされ、runが失敗して`in_progress`になった）、taskも`task_edited`も変えずに変わったことを理由に拒む。
taskの状態で認可する他のコマンド（`set-goal`・`set-paths`・`set-priority`・`revisit`・`draft`・`ready`・`cancel`・`dependency add` / `remove`）も同じく、認可に使ったものをstoreに渡し、storeは書く前に同じtransactionで照合して、食い違えばtask・依存・eventを変えずに拒む。
照合が無いと当たる取り違えは`src/domain/authorization.rs`の`TaskAuthorization`のdoc commentが持つ。

`judge-follow-up`は`follow_up.judge`をtask resourceで判定し、user・inbox・plannerだけに許す。
taskの状態による制限は無く、submitted以降の訂正も記録できる（所属を動かすのはdraft/readyだけ。[所属の判断](follow-up-membership.md)）。

`ready`はuserとinbox（人の言葉での代行。区別はeventのactorが持つ）。
worker・headlessのjob（一覧は`src/domain/actor.rs`の`ActorRole::is_headless_job`）・observer・wrapper・integrator・executor-telemetryは計画系を何もできない。
supervisorはCLIからは`ready`・`cancel`・`goal close`だけ。
capabilityをどのresourceにも持たないroleは、storeを読む前に拒む（taskやproposalが無くても拒否になる）。
capabilityを持つroleで、taskやproposalが見つからないときは拒否ではなく、そのerrorになる。

拒んだときは、queueのevent `authorization_denied`（taskにもgoalにも紐づかないqueueのevent。actorの列は拒まれた呼び出し元）を記録し、`AuthorizationError`を返す。
payloadは`src/application/commands/mod.rs`の`Gate`の記録と`Resource::record`が決める。
記録に失敗しても拒否は拒否のまま返す。
CLIのerrorの形は`src/main.rs`の`error_json`が持つ。
observerのこのeventは、observerの次の起動を決める「自分以外のevent」に数えない。

### 対話と記録のコマンド（application）

対話と記録のコマンド（下の表）は、`src/application/commands/dialogue.rs`の`Dialogue`が全てのroleについて判定してからstoreを呼ぶ。
port `DialogueStore`は`src/infrastructure/dialogue.rs`の`DialogueQueue`が実装する。
拒否の記録（`authorization_denied`）とerrorの形は計画系と同じで、判定と記録の共通の部分は`src/application/commands/mod.rs`の`Gate`が持つ。

| コマンド | capability | resource |
| --- | --- | --- |
| `ask --kind blocked --finding ID` | `finding.ask` | finding |
| `ask`（それ以外） | `ask.open` | `NewAsk`（kind、`--run`、`--task`、`--request`とその依頼の閉じていないruntimeのplanner）。`--request`付きは、`ask.open`を持たないroleとkindの合わないroleを依頼を読む前に拒み、plannerはその依頼のplanner自身でなければ`not on this resource`で拒む |
| `answer` / `ask close` | `ask.answer` / `ask.close` | ask（queueにあるrun）。capabilityを持たないroleはaskを読む前に拒む（記録のrunは`null`） |
| `note` | `note.write` | `--task`のtask・`--run`のrun・`--goal`のgoal |
| `mark` / `mark --retract` | `mark.write` | queue |
| `finding record` | `finding.record` | 対象のtask・run・goal、`--queue`ならqueue |
| `finding resolve` / `finding dismiss` | `finding.resolve` / `finding.dismiss` | finding |

計画の依頼（[ADR-t1394-1](../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)の決定3・6）の`request add`と`request decline`は、`src/application/commands/requests.rs`の`Requests`が全てのroleについて判定してからstoreを呼ぶ（拒否は同じ`authorization_denied`に残る）。
`ask --request ID`（`planner_question`）は上の`ask`と同じ`ask.open`で判定し、`request decline`と同じくその依頼のplannerをstoreから読んで、plannerにはその依頼のplanner自身だけを許す。
`requests`は読み取り（`queue.read`）。

`request add`はCLIの入口でも同じ`request.record`（queue）を判定し、ファイルやstdinを読む前に拒む（queue serviceへ送るときは読まずに送り、serviceが断って記録する。[計画のユースケース](queue-service-host-cli.md#計画のユースケース)）。
拒否はそこで記録して直ちに返すため、`authorization_denied`は1回だけになる。

| コマンド | capability | resource |
| --- | --- | --- |
| `request add` | `request.record` | queue。userとinboxだけが持つ（inboxの記録は人の言葉の代行で、`requested_by`とeventのactorが`inbox`） |
| `request decline ID` | `request.decline` | `Request`（id、その依頼の閉じていないruntimeのplanner）。capabilityを持たないroleは依頼を読む前に拒み、plannerはその依頼のplanner自身でなければ`not on this resource`で拒む |
| `replan request` / `replan propose` | `replan.request` / `replan.propose` | `Run`（storeが読むtask）。`Replans`が判定し、proposeは自分のrunだけ |
| `hold` / `release` | `task.hold` | `Task`か`Run`（storeが読むtask）。`Holds`が判定 |

`asked_by`・noteとfindingの`by`・markの`by`は、呼び出し元が渡した値ではなく`Dialogue`がactorから入れる。
answerはuserとinboxだけで（[ADR-t728-3](../adr/2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)の決定4）、`ask.close`はそれにsupervisorを足したroleだけが持つ。

#### answerの権限の出どころと承認の分類

answerは誰の権限で書かれたか（`AnswerAuthority`）を記録する（ADR-t728-3の決定2）。
値はactorの型（`ActorContext::answerer`）から決め、answerの文やinboxのpromptから推さない。
`answered_by`と`asked_by`の値の綴りは変えない。
eventのactorの列（roleとid）も同じ区別を持つ。

承認に当たるask（ADR-t728-3の決定3）はdomainが分類する（`answer_approves`）。
runtimeが自分で閉じる・取り下げる・置き換えるanswerは、kindに関わらず何も承認しない。
この段では承認を人だけに限る強制はしない。

### runtimeの操作系のコマンド（application）

runtimeの操作系のコマンドは、`src/application/commands/operations.rs`の`Operation`が名指すcapabilityとresourceで、全てのroleについてコマンドが何かをする前に判定する。
CLI（`src/main.rs`の`execute()`）はqueueの場所を決めた直後、queueを開く・作る・移す前、ログを開く前に`operations::authorize`を呼び、`requests`もこれらのコマンドの写しを`Operation::request`から取る。
操作そのもの（`up`・`install`など）は今の場所のままで、ここはその入口の判定だけ。
着地はこの入口の後で[Integrator](#着地とpushintegrator)がもう一度判定する。

| コマンド | capability | resource |
| --- | --- | --- |
| `init` `migrate`（`--check`を含む） `rebind` | `queue.admin` | queue |
| `install` / `auto-update` | `service.install` | queue |
| `up` `down` `inbox` `service start` `service stop` `service serve` `queue-broker serve` | `service.lifecycle` | queue |
| `plan`（何も開かず、inboxへの依頼の案内を付けて拒む。ADR-t1394-1） | `planner.open` | queue |
| `supervise` | `scheduler.supervise` | queue |
| `observe`（`--history`・`--input`を除く） | `observe.run` | queue |
| `integrate ID` / `integrate --next` | `landing.request` | task / queue |
| `recover RUN` | `run.recover` | run（読めないidは`Unresolved`） |
| `run close-workspaces` / `run close-workspaces RUN` / `run close-workspaces --task ID`（認可の後に拒む） | `workspace.cleanup` | queue / run（読めないidは`Unresolved`） / task |
| `review ID` | `review.prepare` | task |
| `run screen RUN`（認可の後に拒む） `run log RUN` / `run send RUN` | `screen.read` / `screen.send` | run（数字はtask。読めないidは`Unresolved`） |
| `planner screen ID` `planner log ID` / `planner send ID` | `screen.read` / `screen.send` | planner |
| `planner request ID` | `planner.request` | planner |
| `session-event open/close`（inboxのspan） | `session.record` | queue |

結果（上のPolicyの表から決まる）:

- userとinboxは全てを打てる（inboxは人の言葉での代行で、dagq-recoverの手作業の`integrate`・`recover`・`review`を含み、`setup-review --yes`を除く。区別は記録のactorが持つ）
- `run screen` / `run send` / `planner screen` / `planner send`と、backgroundのsessionのlogを読む`run log` / `planner log`はuserとinboxだけ（ADR-t1228-1の決定7）。
  plannerは自分のplannerのものも拒み（`screen.read`・`screen.send`を持たない）、supervisorも持たない（自分の送信の経路を使う）
- `planner request`（`planner.request`）はuserとinboxだけ（ADR-t1533-1）。
  plannerは自分のplannerへのものも拒み、supervisorも持たない（自分の依頼は`send_to_planner`で置く）。
  成功した依頼は`turn_requested`と`planner_request_handed`を呼び出し元をactorにして記録する
- `run close-workspaces`（`workspace.cleanup`）を通すのはuserとinboxだけ（ADR-t1228-1の決定7）で、plannerとsupervisorを含むほかのroleは`authorization_denied`で拒む。
  通ったuserとinboxにも、runtimeはrunのworkspaceを開かず、runのbackgroundのwrapperは自分で止めることを理由に拒み（引数は受け付けて使わない。ADR-t1433-3の決定3）、何も閉じず記録しない。
  残ったrunのworkspaceは人が自分のterminalで閉じる。
  supervisorは終わったrunに残ったwrapperを自分の掃除（[終わったrunの掃除](supervisor-lifecycle/background-recovery-job.md#終わったrunの掃除)）で止める
- `run screen`は`screen.read`を通った後、どのrunにも画面を読まずに理由と`run log`を示して拒む（ADR-t1433-3の決定4）
- plannerの`service.lifecycle`・`service.install`・`queue.admin`・`planner.open`は、ADR-t728-1の決定7のとおり許す。
  plannerはruntimeだけが立て、人が頼む相手ではないので（[ADR-t1394-1](../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)）、`up`・`down`・`inbox`・`install`は使わない（打つのはinboxか人の`DAGQ_ROLE`の無いterminal。[Roles](supervisor-lifecycle/roles.md)）。
  `plan`は権限の判定を通っても誰が打っても何も開かず、inboxへの依頼の案内（`PLAN_REFUSED`）で失敗する
- `session`・`planner-session`は環境のactorで判定せず、wrapperのprincipalでserviceが判定する（[Queue service](queue-service.md#クライアントモード)）。
  `session-event`はplannerのspanならwrapperに渡してqueueに触れず、runのspanは無く、inboxのspanはinboxとuserだけが記録できる（workerとplannerは`session.record`を持たない）
- 拒否は`authorization_denied`として、拒まれた呼び出し元をactorにしてqueueに記録する（`src/infrastructure/denials.rs`の`QueueDenials`が、判定の後でだけqueueを開く）。
  queueが無い・このバイナリが開けない（`init`の前、`migrate`の前）ときは記録できず、拒否は拒否のまま返す。
  errorの形は計画系と同じ
- sessionへの操作は上の`run` / `planner`のCLIで行う。
  Codexのinboxは同じ趣旨（dagqのCLIを通す）をCodexの手段で持たせ、無ければguardrailが無いと[Security](security.md#判定の場所)に書く（[ADR-t2159-1](../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定5）。
  settingsの場所・中身と`status` / `doctor`の`inbox_guardrail`は[`up` / `down`](supervisor-lifecycle/up-down.md)と[Security](security.md)が持つ

#### 制御側の起動としての`supervise`と`auto-update`

`supervise`と`auto-update`はruntimeの制御側（信頼する制御側。ADR-t728-1の決定2）のプロセスを始めるコマンドで、呼び出し元の環境で判定する:

- `supervise`は`scheduler.supervise`を持つuser・inbox・supervisorだけが打てる。
  launchdのsupervisorは`DAGQ_ROLE`を持たない（user）ので、`up`を打ったのがplannerでも起動は通る（登録済みのin-cmuxのものは`DAGQ_ROLE=supervisor`で動く）。
  exec の引き継ぎは同じ環境のまま自分をexecするので、同じroleで判定される。
  plannerは`supervise`を直接打てない（ADR-t728-1の決定7の表にschedulerは無い）。
  起動したプロセスのeventのactorは、呼び出し元ではなく`supervisor:<pid>`（`event_actor`）
- `auto-update`は`install`と同じ`service.install`で、supervisorが自分の環境（`supervisor:<pid>`）で起動する。
  `install`と`auto-update`は新しいバイナリでの確認と`up`を子プロセスとして同じ環境のまま打つので、それを打つroleは`queue.admin`と`service.lifecycle`も持つ。
  人とinboxとplannerも`install`と同じ権限で打てる（手順の再現と復旧）。
  `observe`（`observe.run`）もsupervisorが自分の環境で起動し、その中のagentだけがobserverになる

host実行ではこれも助言的で、`DAGQ_ROLE`を外せば誰でもuserになる（ADR-t728-1の決定6）。

### 着地とpush（Integrator）

着地（rebase・再検証・squash・mainの更新）とpushは`src/application/integrate.rs`の`Integrator`だけが行う（[ADR-t728-2](../adr/2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)）。
Integratorはrole `integrator`のactor（`integrator:<pid>`）で、`landing.land`と`landing.push`を持つのはこのroleだけ。
`Integrator::acting_as(actor)`は`StaticPolicy`がこの2つを許すactorでなければ拒み、着地の関数はmoduleの外から呼べない。

- **依頼**: supervisorの着地のthreadと、人（とinboxの代行）の`integrate`は`IntegrationRequest`を作り、`Integrator::land`に渡す。
  `integrate`は`Integrator::approve`がrunを選び、依頼者が`landing.request`を持つことを確かめてから（持たなければ何も記録せずに拒む）`integration_approved`を依頼者として記録し、slotを取って依頼を返す。
  supervisorはslotを自分で取り（`integration_started`はsupervisorの記録）、依頼者は`supervisor:<pid>`
- **Integratorの検査**（`landing_refusal`）: 依頼者が`StaticPolicy`でそのrunに`landing.request`を持つこと（user・inbox・supervisor）、runが`integrating`で依頼のtokenがleaseを持つこと、runが承認済み（`integration_approved`）か、最新のreviewが`pass`かその後に収束した`revise_limit_judged`があること（`RunHistory::landable`）。
  どれかが欠ければ着地せず、slotを持っていれば`abort_integration`で返して（`integration_error`）errorにする。
  reviewの`pass`は着地の必要条件で、この後のreceiptの再検査・rebase・検証コマンド・scopeの判定が落ちれば着地しない（ADR-t728-2の決定3）
- **記録**: `Integrator::land`の間、queueのeventのactorはIntegrator（role `integrator`、id `integrator:<pid>`）で、`requested_by`は依頼者のactor id。
  依頼と記録を切り替える入口はport `EventStore::act_as`
- **push**: `MainRemote::push_main`は`PushGrant`を引数に取り、`PushGrant`はIntegratorが`landing.push`を確かめてからしか作れない（fieldがmoduleの外から見えない）ので、`GitRepository`のpushはIntegratorの外から呼べない

host実行ではIntegratorはsupervisorや`integrate`と同じプロセスとユーザーで動き、この境界は論理的なもの（ADR-t728-2の決定4）。

### queue serviceのユースケース（service側）

queue service（[Queue service](queue-service.md)、ADR-t1233-1決定4）は、要求のtokenから決めたprincipalをactorにして、対話と記録のユースケースを上の「対話と記録のコマンド」と同じ`Dialogue`に、`show`を`Gate`の`queue.read`（task）に、`proposal_list`・`proposal_show`を`queue.read`（queue。CLIの`proposal list|show`と同じ）に、`watch_poll`を`queue.watch`に通す。
findingの書き込みもCLIと同じpolicyで判定する（ADR-t1222-1決定2）。
クライアントの名乗るroleは認可に使わずtokenのprincipalで判定し、tokenの無いuserはsocketのpeerで決める（[hostのCLI](queue-service-host-cli.md)）。
拒否は同じ`authorization_denied`（拒まれたprincipalがactor）に残る。
tokenを発行するのは制御側だけで、AI actorにtokenを作るコマンドは無い。
読み取りは狭めず、executor-telemetryのほかの全roleがqueue全体を読める（ADR-t1233-5決定3）。
queue全体の読み取りのユースケース（一覧は[Queue service](queue-service.md#読み取りのユースケース)）は、CLIがそのコマンドに求めるのと同じcapability（`ci failures`は`ci.read`、ほかは`queue.read`）を`Gate`で通して答える。

### ほかのコマンド（CLIの入口）

上の3つ以外（読み取り・`watch`など）は状態を変えない。

- 宛ての環境のあるworker・job・observerはserviceのユースケースだけを送り、ほかはCLIが拒みDBを開かない（[クライアントモード](queue-service.md#クライアントモード)）。
- 宛ての環境の無いuser・inbox・plannerはserviceが答える読み取り・`watch`・`status`をserviceへ送り、serviceが上の節のとおり認可する。
  CLIはDBを開かず、届かなくても開かない（[hostのCLI](queue-service-host-cli.md)）。
- 残り（3つのroleのserviceが答えないものと、ほかのrole）は`check_access`が（default deny）`requests`の全てで`StaticPolicy`に通す。
  拒んだときだけ`QueueDenials`で`authorization_denied`を呼び出し元をactorに記録する。
  通った読み取りはqueueを読み取り専用で開く（ADR-0073決定5・7・18）。

## Claudeのpermissions.deny（guardrail）

多層防御の1枚として、runtimeがClaude Codeの設定を書くactor（worker・planner・review job・inbox。[Roles](supervisor-lifecycle/roles.md#actorの起動actorexecutor)）の`permissions.deny`に、roleのpolicyから作った規則を入れる（`src/application/execution.rs`の`permission_deny(role)`。規則の中身はそのdoc comment）。

- `DAGQ_COMMANDS`はroleによって拒まれうる`dagq`のsubcommandと、その形のどれかが要るcapabilityの表。
  roleの`grants`がどれも持たないcommandを`Bash(dagq <command>:*)`で拒む。
  表と`src/main.rs`の`requests`が食い違わないことはunit testが確かめる。
  clapの全subcommandが`DAGQ_COMMANDS`か、`src/main.rs`のtestの`LEFT_OUT_COMMANDS`のどちらかにあることもunit testが確かめ、どちらにも無いsubcommandを足すと落ちる。
  roleによって拒まれうるsubcommandを足したら、読み取りの形を持つものを除いて表に入れる。
- actorを名指す変数（`IDENTITY_ENV`）の書き換えも全roleで拒む。
- workerとplannerのsession・turnの設定とreview jobの設定では、`SIGNAL_BY_NAME_DENIED`の後に置く（turnの設定ではその間に`AskUserQuestion`と予約の道具を挟む。[非対話のworker](supervisor-lifecycle/headless-worker.md)。review jobの設定はroleの規則だけ）。
  inboxの設定（[`up` / `down`](supervisor-lifecycle/up-down.md)）は`permissions.deny`だけで、roleの規則だけを持ち、`SIGNAL_BY_NAME_DENIED`は入れない。

これはguardrailでenforcementではない。
Claude Codeの規則はコマンドの先頭の形しか見ないので、pathで打つ`~/.local/bin/dagq integrate`、pluginのskillが使う`"$DAGQ" ...`や`${CLAUDE_PLUGIN_ROOT}/bin/dagq ...`、subcommandの前にglobalのflagを置く`dagq --db X integrate`、`sh -c`、scriptの中からの呼び出しは通る。
拒む判定はCLIの`Authorizer`（上の「適用の範囲」）がする。
どちらもhostではadvisoryで、隔離は将来のsandboxのbackend（[Roles](supervisor-lifecycle/roles.md#実行のbackendとenforcement)）が担う。

このすり抜けを塞ぐために、規則をwildcardで広げること（例: `Bash(* integrate*)`や`Bash(*dagq* integrate:*)`）はしない。
理由:

1. 誤検出が大きい。
   subcommandの名前は日常の語なので、workerが作業で打つ`git log --grep integrate`や`rg 'ready'`なども拒まれ、作業が止まる。
2. 広げてもenforcementにならない。
   `sh -c`・script・変数に入れたcommandの名前からは通るので、守れる範囲は少ししか増えない。
3. 拒む判定はCLIの`Authorizer`がすでにする。
   guardrailは「間違いで打つのを先に止める」ためのもので、pathやglobal flagの形で打った呼び出しもCLIが拒み、`authorization_denied`に記録する。

規則の形は`Bash(dagq <command>:*)`と識別の変数の書き換えのままにする。

## CIの見張り（ADR-t1920-1）

[ADR-t1920-1](../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)と[CI watch](supervisor-lifecycle/ci-watch.md)。
読むコマンド`dagq ci failures`のcapabilityは`ci.read`（`Capability::CiRead`）で、誰が持つかはPolicyの表のとおり（一覧はworkerとrunの全体のreviewにはpromptとrun directoryのファイルで渡す）。
`DAGQ_COMMANDS`により、`ci.read`を持たないroleのClaudeの`permissions.deny`に`Bash(dagq ci failures:*)`が入る。
`finding dismiss --covered-by`も`finding.dismiss`で判定する。
