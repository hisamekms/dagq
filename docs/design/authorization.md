---
id: design-authorization
type: design
title: Authorization
status: current
created: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle-task-hold
  - adr-t1394-1
  - adr-t1533-1
  - adr-t728-1
  - adr-t728-2
  - adr-t728-3
  - adr-t1228-1
  - adr-t1228-2
  - design-supervisor-lifecycle-roles
  - design-security
---

# Authorization

actorが何をしてよいかは、`src/domain/authorization.rs`の`Authorizer`（`authorize(actor, capability, resource) -> Result<(), AuthorizationError>`）が決める（[ADR-t728-1](../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)の決定5）。実装は静的なpolicyの`StaticPolicy`で、roleごとの許可の一覧（`grants(role)`）と、いくつかのroleのresourceの規則でできている。一覧に無い組み合わせは拒む（default deny）。resourceの持ち主や状態を規則が要るのに呼び出し元が知らない（`None`、`Resource::Unresolved`）ときも拒む（fail closed）。判定はClaudeを起動せずにunit testできる。

host実行ではこの判定は助言的（advisory）で、sandboxでも隔離でもない。どのプロセスも`DAGQ_ROLE`を偽れる（ADR-t728-1の決定6）。信頼の区分・actorごとのcapabilityの要約・迂回できる経路・Podmanとqueue serviceへの道筋は[Security](security.md)。actorの型と環境変数は[Roles](supervisor-lifecycle/roles.md#actors)。

## 固定バイナリを戻したときの窓

判定は、呼ばれた`dagq`のバイナリが持つpolicyで決まる。固定バイナリを前のものに戻すと（[`install --rollback`](supervisor-lifecycle/install.md)、自動更新の見張りの`restore`（[Auto-update](supervisor-lifecycle/auto-update.md)））、新しいバイナリが起動してまだ走っているjobの`dagq`の呼び出しは戻したバイナリに届く。戻したバイナリが`DAGQ_ROLE`の未知の値をfail closedにするparseを持たず、新しいroleを知らなければ、そのroleは制限を受けない。task 729より前のバイナリがこれに当たり、`DAGQ_ROLE`のうち`reviewer`だけを読み取りだけに制限し、`review-job`・`recovery-job`・`plan-review-job`・`goal-review-job`を知らないので、これらのjobの状態を変える呼び出しを拒まない（task 729より後のバイナリは知らないroleを`unknown DAGQ_ROLE`で拒む）。

- 窓は、戻したrenameから、新しいバイナリが起動したjobが居なくなるまで。supervisorの引き継ぎはexecの前にheadlessのjobを止めるが（[Handoff](supervisor-lifecycle/handoff.md)、[Headless job processes](supervisor-lifecycle/headless-job-processes.md)）、それは検証と着地の進行中のslotを進め終えた区切りなので、それまでjobは走り続ける。引き継ぎを受けなかったsupervisor（`not_handed_off`）のjobと、supervisorが死んで起動し直すまでに残ったjob（引き継ぐsupervisorが`orphaned_headless_jobs`で止める）は、終わるか止められるまで走る
- これは新しい弱点ではなく、host実行が助言的（advisory）であることの既知の限界の一例として受け入れる。どのプロセスも`DAGQ_ROLE`を偽れる（ADR-t728-1の決定6）ので、古いバイナリに新しいroleを教えるコードは足さない
- 気になるときは、戻す前に`down --wait`でjobを終わらせるか、戻した後に走っているjobを止める

## Capability

| 群 | capability | 対応するCLI |
| --- | --- | --- |
| 読み取り | `queue.read` | `locate` `list` `show` `candidates` `graph`（`--out`なし） `status` `asks` `events` `timeline` `stats` `kpi` `forecast` `doctor` `broker status` `broker logs` `broker audit` `notes` `marks` `findings` `search` `related` `proposal list/show` `planners` `requests` `lint` `goal list/show` `observe --history` `observe --input` |
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
| | `request.record` / `request.decline` | `request add` / `request decline`（計画の依頼。[ADR-t1394-1](../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)の決定3・6、[`plan` / `planners`](supervisor-lifecycle/plan-planners.md#inboxからの計画の依頼)） |
| | `screen.read` / `screen.send` | `run screen` `planner screen` `run log` `planner log` / `run send` `planner send`（sessionの画面を読む・送る。workerのrunには画面が無く、`run screen`はどのrunにも理由とturnのlogのCLI（`run log`）を示して拒み（[ADR-t1433-3](../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）、`run send`はどのrunにも拒む。backgroundのsessionのlogを読む（ADR-t1404-1決定6）。[ADR-t1228-1](../adr/2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)、[sessionへの送信と確認](supervisor-lifecycle/session-send.md#人とinboxの画面の読み取りと送信)） |
| | `planner.request` | `planner request`（開いている非対話のruntimeのplannerへの続きの依頼を次のturnとして置く。[ADR-t1533-1](../adr/2026-10-03-t1533-1-follow-up-requests-go-to-headless-planners-by-planner-id-and-no-planner-close.md)、[`plan` / `planners`](supervisor-lifecycle/plan-planners.md#続きの依頼と非対話のplannerのcli)）。plannerを閉じるCLIは無い |
| schedulerの遷移 | `scheduler.supervise` | `supervise` |
| | `run.recover` | `recover` |
| | `workspace.cleanup` | `run close-workspaces`（終わったrunの残ったworkspaceの片付けだったが、runtimeがrunのworkspaceを開かなくなったので、認可の後に理由を示して拒む。[ADR-t1433-3](../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)の決定3。認可はもとの[ADR-t1228-1](../adr/2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)の決定6・7のまま） |
| | `service.lifecycle` | `up` `down` `broker start` `broker stop` |
| | `service.install` | `install` `auto-update` |
| | `queue.admin` | `init` `migrate` `rebind` |
| 着地 | `landing.request` | `integrate`（Integratorへの依頼。[ADR-t728-2](../adr/2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)） |
| | `landing.land` / `landing.push` | Integratorの内側 |
| 予約 | `reserved.filesystem_read` `reserved.filesystem_write` `reserved.network` `reserved.secret_read` | 後の段（sandbox）。この段では誰にも与えない |

CLIのコマンドからcapabilityとresourceへの写しは`src/main.rs`の`requests`で、matchに既定の枝を置かないので、コマンドを足すと写しを書くまでcompileが通らない。`queue.read`だけのコマンドはqueueをread-onlyの接続で開く（`reads_only`）。

## Resource

`Queue`・`Goal`・`Task`（idと、分かれば状態）・`Run`（idと、分かればtask）・`Ask`（idと、分かればrun）・`NewAsk`（開くaskのkindと、コマンドが名指すrunとtask）・`Proposal`（idと、分かれば出したplannerのactor id）・`Finding`・`Planner`・`Request`（計画の依頼のidと、分かればその依頼に開いているruntimeのplanner）・`Unresolved`（読めなかったid）。CLIの写し（`requests`）は引数にあるものだけを入れ、持ち主と状態は`None`のまま渡す。計画系のコマンドはapplicationの層（下の[適用の範囲](#適用の範囲)）がqueueからtaskの状態とproposalの持ち主を読んで埋めてから判定する。proposalの持ち主は`proposals.owner_actor_id`（`0046_proposal_owner_actor.sql`）で、`submit`（新しいproposalと、差し戻しの後の出し直し）が出したactorのid（`planner:<id>`など）を書く。plan reviewが人を待つproposalをそのまま出し直す`submit --proposal`は持ち主を変えない（古いバイナリの出し直しも列を書かない）。plan reviewが差し戻して`revising`のproposalは、reviseを送ったplanner（`revise_planner_id`。持ち主が閉じていたときにruntimeが立てたplannerや、reopenしたproposalを受け取ったplanner）を持ち主とする。migrationより前に出したproposalは`NULL`で、plannerは取り下げられない（fail closed。userとinboxは取り下げられる）。`DAGQ_ACTOR_ID`より前に開いたplannerのid `planner`（roleの名前だけでactorを名指さない）は、どのproposalの持ち主にもならない。

## Policy

| role | 許すcapability | resourceの規則 |
| --- | --- | --- |
| user | 予約と`review.submit` `triage.submit` `landing.land` `landing.push` `request.decline`を除く全て | なし |
| inbox | userと同じ（人の言葉での代行。[ADR-t728-3](../adr/2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)の決定1） | なし。人自身の操作との区別は記録が持つ |
| planner | 読み取り・`queue.watch`・`queue.export`・`ci.read`・`goal.write`・`goal.close`・`task.write`・`follow_up.judge`・`task.cancel`・`proposal.submit`・`proposal.withdraw`・`note.write`・`mark.write`・`ask.open`・`session.run`・`session.record`・`finding.resolve`・`finding.dismiss`・`planner.open`・`request.decline`・`service.lifecycle`・`service.install`・`queue.admin` | runにはnote（`note --run`）だけを書け、ほかは何もできない（runに紐づくaskも開けない）。開けるaskは`planner_question`だけで、依頼に紐づくもの（`ask --request ID`）はその依頼の閉じていないruntimeのplannerのactor idが自分のときだけ（`request.decline`と同じ規則。plannerが分からなければ拒む）。taskの変更（`task.write`・`task.cancel`）はdraft・submitted・readyのものだけ（状態が不明なら拒む）。`revisit`（draftの再検討の時刻、`task.write`）はdraftだけで（storeがどのroleにも求める）、`draft_planner_exhausted`のあるdraftには付けられない（人・inboxだけが付ける。[ADR-t1540-1](../adr/2026-10-05-t1540-1-a-kept-draft-returns-to-runtime-planners-at-its-revisit-time.md)）。`follow_up.judge`は全状態のfollow_upに記録でき、所属を動かすのはdraft・readyだけ。noteはどの状態のtaskにも書ける。`proposal.withdraw`は自分（actor id）が出したproposalだけ。`planner-session`は自分のplannerだけ。`request.decline`はそのplannerが立てられた依頼（依頼の閉じていないruntimeのplannerのactor idが自分）だけで、plannerが分からなければ拒む |
| worker | 読み取り・`ask.open`・`note.write`・`session.run`・`session.record` | 読み取り以外は自分のrun（`DAGQ_RUN_ID`）・そのtask（`DAGQ_TASK_ID`）・自分のrunのaskだけ。開けるaskは`worker_question`だけで、`--run`なら自分のrun、`--task`だけなら自分のtask（`DAGQ_RUN_ID`の無いworkerは何も持たない）。`session`と`session-event`は自分のrunのものだけ（runtimeのwrapperとhookはworkerの環境のまま打つ） |
| review-job | 読み取り・`review.submit` | `review.submit`は自分のrunだけ |
| recovery-job | 読み取り・`triage.submit` | `triage.submit`は自分のrunだけ |
| plan-review-job・goal-review-job・throughput-review-job | 読み取りだけ | throughput-review-jobは[スループットの見直し](supervisor-lifecycle/throughput-review.md)（ADR-t996-1） |
| observer | 読み取り・`queue.watch`・`ci.read`・`finding.record`・`finding.resolve`・`finding.ask` | `finding.resolve`と`finding.ask`はfindingだけ |
| supervisor | 読み取り・`queue.watch`・`queue.export`・`ci.read`・`goal.close`・`task.cancel`・`task.ready`・`note.write`・`ask.open`・`ask.close`・`session.run`・`review.prepare`・`finding.record`・`finding.resolve`・`finding.dismiss`・`observe.run`・`planner.open`・`scheduler.supervise`・`run.recover`・`service.lifecycle`・`service.install`・`queue.admin`・`landing.request` | なし。`queue.admin`は自動更新が新しいバイナリで`install`と同じ確認（`migrate --check`・`migrate`・使い捨てのqueueの`init`）をするため。`ask.answer`・`task.ready_bypass_review`・`landing.land`・`landing.push`・`workspace.cleanup`（終わったrunに残ったbackgroundのwrapperは自分の掃除で止める）は持たない |
| wrapper | 読み取り・`session.run`・`session.record` | なし |
| integrator | 読み取り・`landing.land`・`landing.push` | なし |

旧値の`DAGQ_ROLE=reviewer`はreview-jobとして読む（[Roles](supervisor-lifecycle/roles.md#cliでの解釈)）。jobの環境には今`DAGQ_RUN_ID`が無いので、`review.submit`・`triage.submit`は持ち主が分からず拒まれる。verdictは今までどおりデータとしてsupervisorが読む。

askのkindはroleごとに決まる（`opens_ask`）: userとinboxとsupervisorはCLIが開けるどのkindも、workerは`worker_question`、plannerは`planner_question`だけを開ける。observerの`ask --kind blocked --finding`は`ask.open`ではなく`finding.ask`で、finding（`Resource::Finding`）に対して判定する。kindがroleに合わなければ理由`not of this kind`で拒む。

`AuthorizationError`はroleと拒んだcapabilityと理由（`not granted`・`reserved`・`not on this resource`・`not of this kind`）だけを出し、actor id（session idを含みうる）やresourceのid・本文を出さない。

## 予定: taskのhold

taskのholdと解除（ADR-t1879-1、[taskのhold](supervisor-lifecycle/task-hold.md)）は、まだ実装していない。
userとinboxだけに与えるcapabilityを1つ足し、ほかのroleは拒む。
実装が、上の表・[Security](security.md)・pluginの権限の表を直す。

## 適用の範囲

### 計画系のコマンド（application）

計画系のコマンド（`add`・`edit`・`submit`・`draft`・`ready`（`--bypass-review`を含む）・`cancel`・`dependency add/remove`・`goal add/edit/ready/close/review`・`set-goal`・`judge-follow-up`・`set-paths`・`set-priority`・`revisit`・`proposal withdraw`）は、`src/application/commands/planning.rs`の`Planning`が全てのroleについて判定してからstoreを呼ぶ。CLI（`src/main.rs`の`execute()`）はparseと出力だけをし、これらのコマンドでstoreの変更を直接呼ばない。`Planning`は呼び出し元の`ActorContext`・`Authorizer`（`StaticPolicy`）・port `PlanningStore`（`SqliteQueue`が`src/infrastructure/planning.rs`で実装する）を受け取り、コマンドごとに次のcapabilityとresourceを問う。

| コマンド | capability | resource |
| --- | --- | --- |
| `add` | `task.write` | `--goal`のgoal、無ければqueue |
| `edit` `set-goal` `set-paths` `draft` `dependency add/remove` | `task.write` | task（queueにある状態） |
| `set-priority` | `task.write` | task（queueにある状態）。`in_progress`のtask（再開待ちか復旧を待つrunを持つもの）にも置け・外せ、次の再開と復旧jobの順に効く（[ADR-t1850-1](../adr/2026-10-06-t1850-1-resumes-recovery-jobs-and-claims-share-one-line-by-effective-priority.md)の決定7）。plannerは今の規則（in_progress以降のtaskの`task.write`は`not on this resource`）で拒まれ、user・inboxだけが通る。新しいcapabilityは作らない。`completed` / `canceled`は全てのroleでdomainが拒む |
| `revisit ID --at` / `revisit ID --clear` | `task.write` | task（queueにある状態）。draftの再検討の時刻を付け・変え・外す（[ADR-t1540-1](../adr/2026-10-05-t1540-1-a-kept-draft-returns-to-runtime-planners-at-its-revisit-time.md)）。開始前のtaskの変更と同じroleの集合（user・inbox・planner。worker・observer・job・supervisorは`not granted`）なので、新しいcapabilityを作らず`task.write`に含めた。storeは同じtransactionでtaskが`draft`であることを確かめ（認可に使った状態は渡さない。draft以外は全てのroleに拒む）、`user` / `inbox`以外が`draft_planner_exhausted`のあるdraftに付けるのを拒む |
| `judge-follow-up` | `follow_up.judge` | task（状態による制限なし。follow_upの出どころと必須の欄・遷移をstoreが同一transactionで検査） |
| `edit --verify` / `edit --no-verify`（`in_progress`のみ） | `task.verify_edit` | task（最新runが終了し、生きているrunが無いことをstoreが同一transactionで検査） |
| `ready` / `ready --bypass-review`・`ready --inherit` | `task.ready` / `task.ready_bypass_review` | task（状態） |
| `cancel`（`--duplicate-of`を含む） | `task.cancel` | task（状態） |
| `submit` / `submit --proposal ID` | `proposal.submit` | queue / proposal（持ち主） |
| `proposal withdraw` | `proposal.withdraw` | proposal（持ち主） |
| `goal add` / `goal edit` / `goal ready` / `goal close` / `goal review` | `goal.write` / `goal.write` / `goal.ready` / `goal.close` / `goal.review_request` | queue / goal |

終了runのverifyだけはuserとinboxが`task.verify_edit`で直せる（[ADR-t883-1](../adr/2026-09-30-t883-1-edit-ended-run-verification-before-inherited-retry.md)）。planner・worker・jobはこの権限を持たない。`required_evidence`と`paths`は変更できず、`task_edited`の`from`/`to`とactorに修正が残る。`edit`のcapabilityは`Planning`がtransactionの外で読んだtaskの状態から選ぶので、`Planning`はその状態をstoreの`edit_task`に渡し、storeは同じtransactionで読んだ状態と照合する。食い違えば（例: `ready`で`task.write`を通った後にclaimされ、runが失敗して`in_progress`になった）、taskも`task_edited`も変えずに状態が変わったことを理由に拒む。taskの状態で認可する他のコマンド（`set-goal`・`set-paths`・`set-priority`・`draft`・`ready`（`--bypass-review`を含む）・`cancel`（`--duplicate-of`を含む）・`dependency add` / `remove`）も同じく、認可に使った状態をstoreに渡し、storeは書く前に同じtransactionで照合して、食い違えばtask・依存・eventを変えずに拒む。domainの遷移はrunの終わった`in_progress`からの`draft`と`cancel`を許すので、照合が無いと`ready`で認可したplannerの`cancel` / `draft`が、その間にclaimされてrunが失敗した`in_progress`のtaskに当たる。

`judge-follow-up`は`follow_up.judge`をtask resourceで判定し、user・inbox・plannerだけに許す。taskの状態による制限は無く、submitted以降の訂正も記録できる（所属を動かすのはdraft/readyだけ。[所属の判断](follow-up-membership.md)）。

policyは上の表のまま: plannerは今の権限（draft・submitted・readyのtaskの変更と`cancel`、goalの追加・編集・close、自分のproposalの取り下げ）を持ち、`ready`（`--bypass-review`を含む）・`goal ready`・`goal review`とin_progress以降のtaskの変更（follow_upの所属判断の記録を除く。`in_progress`のtaskの`set-priority`もuserとinboxだけ）は持たない。`ready`はuserとinbox（人の言葉での代行。区別はeventのactorが持つ）。worker・4つのjob・observer・wrapper・integratorは計画系を何もできない。supervisorはCLIからは`ready`・`cancel`・`goal close`だけ。capabilityをどのresourceにも持たないroleは、storeを読む前に拒む（taskやproposalが無くても拒否になり、記録のresourceは状態と持ち主が`null`）。capabilityを持つroleで、taskやproposalが見つからないときは拒否ではなく、そのerror（`task N does not exist`など）になる。

拒んだときは、queueのevent `authorization_denied`（taskにもgoalにも紐づかないqueueのevent。actorの列は拒まれた呼び出し元）を記録し、`AuthorizationError`を返す。payloadは`role`・`capability`・`reason`（`not granted`・`reserved`・`not on this resource`）・`resource`（`kind`と`id`、taskなら`status`、proposalなら`owner`、依頼なら`planner`。開くask（`new_ask`）はidの代わりに`ask_kind`・`run`・`task`を持ち、`--request`付きなら`request`と依頼の`planner`も持つ）。記録に失敗しても拒否は拒否のまま返す。observerのこのeventは、observerの次の起動を決める「自分以外のevent」に数えない。

CLIのerrorは`{"error": ..., "denied": {"role", "capability", "reason"}}`で、`error`はobserverなら`observer may not change queue state`、4つのjob（と`reviewer`）なら`reviewer may not change queue state`（今までの文言）、それ以外は`<role> may not <capability> (<reason>)`。

### 対話と記録のコマンド（application）

`ask`・`ask close`・`answer`・`note`・`mark`（`--retract`を含む）・`finding record/resolve/dismiss`は、`src/application/commands/dialogue.rs`の`Dialogue`が全てのroleについて判定してからstoreを呼ぶ。port `DialogueStore`は`src/infrastructure/dialogue.rs`の`DialogueQueue`（`SqliteQueue`を包む）が実装する。拒否の記録（`authorization_denied`）とerrorの形は計画系と同じで、判定と記録の共通の部分は`src/application/commands/mod.rs`の`Gate`が持つ。

| コマンド | capability | resource |
| --- | --- | --- |
| `ask --kind blocked --finding ID` | `finding.ask` | finding |
| `ask`（それ以外） | `ask.open` | `NewAsk`（kind、`--run`、`--task`、`--request`とその依頼の閉じていないruntimeのplanner）。`--request`付きは、`ask.open`を持たないroleとkindの合わないroleを依頼を読む前に拒み、plannerはその依頼のplanner自身でなければ`not on this resource`で拒む |
| `answer` / `ask close` | `ask.answer` / `ask.close` | ask（queueにあるrun）。capabilityを持たないroleはaskを読む前に拒む（記録のrunは`null`） |
| `note` | `note.write` | `--task`のtask・`--run`のrun・`--goal`のgoal |
| `mark` / `mark --retract` | `mark.write` | queue |
| `finding record` | `finding.record` | 対象のtask・run・goal、`--queue`ならqueue |
| `finding resolve` / `finding dismiss` | `finding.resolve` / `finding.dismiss` | finding |

計画の依頼（[ADR-t1394-1](../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)の決定3・6）の`request add`と`request decline`は、`src/application/commands/requests.rs`の`Requests`が全てのroleについて判定してからstoreを呼ぶ（port `RequestStore`は`SqliteQueue`が実装し、拒否は同じ`authorization_denied`に残る）。`ask --request ID`（`planner_question`）は上の`ask`と同じ`ask.open`で判定し、`request decline`と同じくその依頼のplannerをstoreから読んで（port `DialogueStore::request_planner`）、plannerにはその依頼のplanner自身だけを許す。`requests`は読み取り（`queue.read`）。

`request add`はCLIの`check_access`でも同じ`request.record`（queue）を`Gate`に通し、`request_words`でファイルやstdinを読む前に拒む。拒否はそこで記録して直ちに返すため、`Requests::record`の検査は残しても`authorization_denied`は1回だけになる。権限を持つuserとinboxには今までどおり入力の検査を行う。

| コマンド | capability | resource |
| --- | --- | --- |
| `request add` | `request.record` | queue。userとinboxだけが持つ（inboxの記録は人の言葉の代行で、`requested_by`とeventのactorが`inbox`）。planner・worker・observer・全てのjob・supervisor・wrapper・integratorは拒む |
| `request decline ID` | `request.decline` | `Request`（id、その依頼の閉じていないruntimeのplanner）。capabilityを持たないroleは依頼を読む前に拒み、plannerはその依頼のplanner自身でなければ`not on this resource`で拒む |

`asked_by`・noteとfindingの`by`・markの`by`は、呼び出し元が渡した値ではなく`Dialogue`がactorから入れる（roleの名前、userは`human`。綴りは今までどおり）。answerはuserとinboxだけで（[ADR-t728-3](../adr/2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)の決定4）、worker・4つのjob・observer・planner・supervisor・wrapper・integratorは`ask.answer`を、worker・4つのjob・observer・planner・wrapper・integratorは`ask.close`を持たない（supervisorは閉じられる）。

#### answerの権限の出どころと承認の分類

answerは誰の権限で書かれたか（`AnswerAuthority`）を記録する（ADR-t728-3の決定2）。値はactorの型（`ActorContext::answerer`）から決め、answerの文やinboxのpromptから推さない。

| 値 | 書くもの | `answered_by` |
| --- | --- | --- |
| `user` | `DAGQ_ROLE`の無い呼び出し（人自身） | `person` |
| `delegated` | inbox（人の言葉での代行） | `inbox` |
| `runtime` | runtimeが自分で閉じる・取り下げる・置き換えるask | `runtime` |

askの行には`answer_authority`と`answer_approval`（`0047_answer_authority.sql`）を、`ask_answered`のpayloadには`authority`と`approval`を書く。`answered_by`と`asked_by`の値の綴りは変えない（jobのaskの`asked_by`の改名とhumanからuserへの改名はgoal 48のtask 502が持つ）。eventのactorの列（roleとid）も同じ区別を持つ。migrationより前のanswerと古いバイナリのanswerは両方とも`NULL`で、JSONには出ない。

承認に当たるask（後のgoalで人だけに限るときの土台。ADR-t728-3の決定3）はdomainが分類する（`AskKind::is_approval`と`answer_approves`）: kindが`approve_landing`・`decide`・`approve_plan`・`approve_goal`・`correct_goal`・`approve_update`・`update_failed`のaskのanswer、または`blocked`・`stalled`のaskがoptionに出した`propose` / `dismiss`（runtimeがfindingに適用する答え）。そのanswerは`answer_approval`が`1`、payloadの`approval`が`true`になる。runtimeが自分で閉じる・取り下げる・置き換えるanswer（`authority`が`runtime`）は何も承認しないので、kindに関わらず`0` / `false`にする。この段では承認を人だけに限る強制はしない。

### runtimeの操作系のコマンド（application）

runtimeの操作系のコマンドは、`src/application/commands/operations.rs`の`Operation`が名指すcapabilityとresourceで、全てのroleについてコマンドが何かをする前に判定する。CLI（`src/main.rs`の`execute()`）はqueueの場所を決めた直後、queueを開く・作る・移す前、ログを開く前に`operations::authorize`を呼び、`requests`もこれらのコマンドの写しを`Operation::request`から取る。操作そのもの（`up`・`install`など）は今の場所のままで、ここはその入口の判定だけ。着地はこの入口の後で[Integrator](#着地とpushintegrator)がもう一度判定する。

| コマンド | capability | resource |
| --- | --- | --- |
| `init` `migrate`（`--check`を含む） `rebind` | `queue.admin` | queue |
| `install` / `auto-update` | `service.install` | queue |
| `up` `down` `broker start` `broker stop` `service start` `service stop` `service serve` | `service.lifecycle` | queue |
| `plan`（何も開かず、inboxへの依頼の案内を付けて拒む。ADR-t1394-1） | `planner.open` | queue |
| `supervise` | `scheduler.supervise` | queue |
| `observe`（`--history`・`--input`を除く） | `observe.run` | queue |
| `integrate ID` / `integrate --next` | `landing.request` | task / queue |
| `recover RUN` | `run.recover` | run（読めないidは`Unresolved`） |
| `run close-workspaces` / `run close-workspaces RUN` / `run close-workspaces --task ID`（認可の後に拒む） | `workspace.cleanup` | queue / run（読めないidは`Unresolved`） / task |
| `review ID` | `review.prepare` | task |
| `session --run RUN` | `session.run` | run（読めないidは`Unresolved`） |
| `planner-session --planner ID` | `session.run` | planner |
| `run screen RUN`（認可の後に拒む） `run log RUN` / `run send RUN` | `screen.read` / `screen.send` | run（数字はtask。読めないidは`Unresolved`） |
| `planner screen ID` `planner log ID` / `planner send ID` | `screen.read` / `screen.send` | planner |
| `planner request ID` | `planner.request` | planner |
| `session-event open/close` | `session.record` | hookが記録するspan: inboxのspanはqueue、plannerのspanは`DAGQ_PLANNER_ID`のplanner（無いか、`DAGQ_ACTOR_ID`より前に開いたworkspaceならqueue）、spanの無いsession（run）は`--run`、無ければ`DAGQ_RUN_ID`のrun（どちらも無ければ`Unresolved`） |

結果（上のPolicyの表から決まる）:

- userとinboxは全てを打てる（inboxは人の言葉での代行で、dagq-recoverの手作業の`integrate`・`recover`・`review`を含む。区別は記録のactorが持つ）
- `run screen` / `run send` / `planner screen` / `planner send`と、backgroundのsessionのlogを読む`run log` / `planner log`はuserとinboxだけ（ADR-t1228-1の決定7）。plannerは自分のplannerのものも拒み（`screen.read`・`screen.send`を持たない）、supervisorも持たない（自分の送信の経路を使う）
- `planner request`（`planner.request`）はuserとinboxだけ（ADR-t1533-1）。plannerは自分のplannerへのものも拒み、supervisorも持たない（自分の依頼は`send_to_planner`で置く）。成功した依頼は`turn_requested`と`planner_request_handed`を呼び出し元をactorにして記録する
- `run close-workspaces`（`workspace.cleanup`）を通すのはuserとinboxだけ（ADR-t1228-1の決定7）で、plannerとsupervisorを含むほかのroleは`authorization_denied`で拒む。通ったuserとinboxにも、runtimeはrunのworkspaceを開かず、runのbackgroundのwrapperは自分で止めることを理由に拒み（引数は受け付けて使わない。ADR-t1433-3の決定3）、何も閉じず記録しない。過去に作られて残ったrunのworkspaceは人が自分のterminalで閉じる。supervisorは終わったrunに残ったwrapperを自分の掃除（[Run workspaces](supervisor-lifecycle/run-workspaces.md)）で止める
- `run screen`は`screen.read`を通った後、どのrunにも画面を読まずに理由と`run log RUN [--follow]`を示して拒む（ADR-t1433-3の決定4）
- plannerの権限は`up`・`down`・`install`・`init`・`migrate`・`rebind`・`plan`と、自分のplannerの`planner-session`と`session-event`を許す（ADR-t728-1の決定7のとおり今の権限のまま）。plannerはruntimeだけが立て、人が頼む相手ではないので（[ADR-t1394-1](../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)）、`up`・`down`・`install`は使わない（打つのはinboxか人の`DAGQ_ROLE`の無いterminal。[Roles](supervisor-lifecycle/roles.md)）。`plan`は権限の判定を通っても誰が打っても何も開かず、inboxへの依頼の案内（`PLAN_REFUSED`）で失敗する。`integrate`・`recover`・`review`・`supervise`・`observe`・`session`は拒む
- worker・4つのjob・observerは`integrate`・`recover`・`install`・`auto-update`・`up`・`down`・`init`・`migrate`・`rebind`・`plan`・`supervise`・`observe`・`review`を拒む。workerは自分のrunの`session`と`session-event`だけを打てる。別のrunのもの、inboxやplannerのspanを名乗るもの（`DAGQ_SESSION_KIND`）、`planner-session`は拒む
- 拒否は`authorization_denied`として、拒まれた呼び出し元をactorにしてqueueに記録する（`src/infrastructure/denials.rs`の`QueueDenials`が、判定の後でだけqueueを開く）。queueが無い・このバイナリが開けない（`init`の前、`migrate`の前）ときは記録できず、拒否は拒否のまま返す。errorの形は計画系と同じ
- 生の`cmux`はこのpolicyの外にある（dagqのCLIを通らない）。inboxとplannerには、Claudeのsettingsの`permissions.deny`の`Bash(cmux:*)`（`permission_deny(Inbox|Planner)`の最後の規則）を置き、上の`run` / `planner`のCLIを使わせる（[ADR-t1228-2](../adr/2026-10-02-t1228-2-deny-raw-cmux-to-inbox-and-planner-as-a-guardrail.md)）。これは**guardrailであってenforcementではない**: hostの判定は助言的なまま（ADR-t728-1決定6）で、絶対pathやscriptからの`cmux`は通り、`up`が`reused`で使い続けるinboxと`claude`を打ち直したsessionには効かない。拒むのはあくまでCLIの判定で、cmuxを拒んだことでCLIの権限は変わらない。workerとjobの`cmux`は拒まない（隔離が扱う）。Codexのinboxは同じ趣旨をCodexの手段で持たせ、無ければguardrailが無いと[Security](security.md#判定の場所)に書く（決定6。今のinboxはClaudeだけ）。settingsの場所・中身と`status` / `doctor`の`inbox_guardrail`は[`up` / `down`](supervisor-lifecycle/up-down.md)と[Security](security.md)が持つ

#### 制御側の起動としての`supervise`と`auto-update`

`supervise`と`auto-update`はruntimeの制御側（信頼する制御側。ADR-t728-1の決定2）のプロセスを始めるコマンドで、呼び出し元の環境で判定する:

- `supervise`は`scheduler.supervise`を持つuser・inbox・supervisorだけが打てる。`up`はin-cmuxのsupervisorのworkspaceを`DAGQ_ROLE=supervisor`の環境で開き、launchdの登録は`DAGQ_ROLE`を持たない（user）ので、`up`を打ったのがplannerでも起動は通る。exec の引き継ぎ（`--handoff-token`）は同じ環境のまま自分をexecするので、同じroleで判定される。plannerは`supervise`を直接打てない（ADR-t728-1の決定7の表にschedulerは無い）。起動したプロセスのeventのactorは、呼び出し元ではなく`supervisor:<pid>`（`event_actor`）
- `auto-update`は`install`と同じ`service.install`で、supervisorが自分の環境（`supervisor:<pid>`）で起動する。`install`と`auto-update`は新しいバイナリで`migrate --check`・`migrate`・使い捨てのqueueの`init`と`list`・`up`を子プロセスとして同じ環境のまま打つので、それを打つroleは`queue.admin`と`service.lifecycle`も持つ（supervisorに`queue.admin`を足したのはこのため）。人とinboxとplannerも`install`と同じ権限で打てる（手順の再現と復旧）。`observe`（`observe.run`）もsupervisorが自分の環境で起動し、その中のagentだけがobserverになる

host実行ではこれも助言的で、`DAGQ_ROLE`を外せば誰でもuserになる（ADR-t728-1の決定6）。wrapperとhookをworkerの環境から分けて、信頼する制御側のwrapperのactor（`ActorRole::Wrapper`）として判定することは、queue service / brokerのgoal 38の後続にする。

### 着地とpush（Integrator）

着地（rebase・再検証・squash・mainの更新）とpushは`src/application/integrate.rs`の`Integrator`だけが行う（[ADR-t728-2](../adr/2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)）。Integratorはrole `integrator`のactor（`integrator:<pid>`）で、`landing.land`と`landing.push`を持つのはこのroleだけ。`Integrator::acting_as(actor)`は`StaticPolicy`がこの2つを許すactorでなければ拒み（integrator以外の全てのroleが拒まれることをunit testが確かめる）、着地の関数（`begin`・`land_integrating`・`push_main`）はmoduleの外から呼べない。

- **依頼**: supervisorの着地のthreadと、人（とinboxの代行）の`integrate`は`IntegrationRequest`（`requester`＝依頼者のactor、run、戻り先のstatus、着地先のmain、integrationのslotのtoken）を作り、`Integrator::land`に渡す。`integrate`は`Integrator::approve`がrunを選び、依頼者が`landing.request`を持つことを確かめてから（持たなければ何も記録せずに拒む）`integration_approved`を依頼者として記録し、slotを取って依頼を返す。supervisorはslotを自分で取り（`integration_started`はsupervisorの記録）、依頼者は`supervisor:<pid>`
- **Integratorの検査**（`landing_refusal`）: 依頼者が`StaticPolicy`でそのrunに`landing.request`を持つこと（user・inbox・supervisor）、runが`integrating`で依頼のtokenがleaseを持つこと、runが着地を承認されている（`integration_approved`。人の`integrate`か`approve_landing`の`land`）か最新のreviewのverdictが`pass`であること。どれかが欠ければ着地せず、slotを持っていれば`abort_integration`で返して（`integration_error`、`code: other`）errorにする。reviewの`pass`は着地の必要条件で、この後のreceiptの再検査・rebase・検証コマンド・scopeの判定が落ちれば着地しない（ADR-t728-2の決定3）
- **記録**: `Integrator::land`の間、queueのeventのactorはIntegrator（role `integrator`、id `integrator:<pid>`）で、`requested_by`は依頼者のactor id（人は`user`、inboxは`inbox`、supervisorは`supervisor:<pid>`）。`verification_command`・`integration_rebased`・`integration_deferred`・`run_integrated`・`push_finished` / `push_skipped` / `push_failed`などがこれにあたる。依頼と記録を切り替えるのはport `RunLog::act_as`と`RunLog::request_as`（抜けるときに`RunLog::restore_request`で入る前の依頼者に戻す）
- **push**: `MainRemote::push_main`は`PushGrant`を引数に取り、`PushGrant`はIntegratorが`landing.push`を確かめてからしか作れない（fieldがmoduleの外から見えない）ので、`GitRepository`のpushはIntegratorの外から呼べない

host実行ではIntegratorはsupervisorや`integrate`と同じプロセスとユーザーで動き、この境界は論理的なもの（ADR-t728-2の決定4）。

### queue serviceのユースケース（service側）

queue service（[Queue service](queue-service.md)、ADR-t1233-1決定4）は、要求のtokenから決めたprincipal（role・actor id・run・task）をactorにして、`ask`・`note`・`finding_record`・`finding_resolve`・`finding_dismiss`を上の「対話と記録のコマンド」と同じ`Dialogue`に、`show`を`Gate`の`queue.read`（task）に、`proposal_list`・`proposal_show`を`queue.read`（queue。CLIの`proposal list|show`と同じ）に通す。findingの書き込みもCLIと同じpolicyで判定する（ADR-t1222-1決定2）: observerは`finding.record`・`finding.resolve`と、findingに紐づく`blocked`のaskの`finding.ask`だけを持ち、`finding.dismiss`・`note.write`・findingに紐づかないaskの`ask.open`は拒まれる。workerとjobはfindingを書けない。inboxとplannerは上の表のとおり（inboxは全て、plannerは`finding.resolve`・`finding.dismiss`）。クライアントが名乗るroleは使わない（workerとjobのクライアントモードのdagqは`DAGQ_ROLE`を読まず、そのtokenのprincipalで判定される。[クライアントモード](queue-service.md#クライアントモード)）。拒否は同じ`authorization_denied`（拒まれたprincipalがactor）に残る。tokenを発行するのは制御側だけで、AI actorにtokenを作るコマンドは無い。goal 82では読み取りを狭めず、全roleがqueue全体を読める（ADR-t1233-5決定3）。queue全体の読み取りのユースケース（`list`・`events`・`timeline`・`stats`・`kpi`・`forecast`・`marks`・`search`・`related`・`findings`・`goal_show`など。一覧は[Queue service](queue-service.md#読み取りのユースケース)）は、CLIがその読み取りのコマンドに求めるのと同じ`queue.read`（queue）を`Gate`でprincipalに通してから答えるので、workerとjob（review・復旧・plan review・goal review・スループットの見直し・observer）も読める。CLIで`queue.watch`の`watch`と`export.file`の`report`・`graph --out`はユースケースが無く（`bad_request`）、paramsは書き出すファイルも実行するprogram（`stats`の`--cmux`など）も受け取らず、hostのd2を起動する`graph --format svg`も受け取らない。

### ほかのコマンド（CLIの入口）

上の3つ以外のコマンド（読み取り・`watch`・`graph --out`・`report`・`ci failures`）は状態を変えない。これらはroleを問わず（default deny）`src/main.rs`の`check_access`が`requests`の全てで`StaticPolicy`に通す。拒んだときだけ、runtimeの操作と同じ`src/infrastructure/denials.rs`の`QueueDenials`（判定の後でだけqueueを開く）で`authorization_denied`を、拒まれた呼び出し元をactorにして記録する（payloadは計画系と同じ`role`・`capability`・`reason`・`resource`）。queueが無い・このバイナリが開けないときは記録せず、拒否は拒否のまま返す。通った読み取りは今までどおりqueueを読み取り専用で開き、書かない（ADR-0073決定5・7・18）。読み取り（`queue.read`）は全roleが持つ。`watch`（`queue.watch`）はuser・inbox・planner・supervisor・observer、`graph --out`と`report`（`queue.export`）はuser・inbox・planner・supervisorだけが持ち、`ci failures`（`ci.read`）はuser・inbox・planner・observer・supervisorだけが持ち、worker・wrapper・integratorと4つのjob（と旧値`reviewer`）のそれらは拒まれる。observerとjobの拒否のerrorの文は以前の`observer_access` / `reviewer_access`のもの（`observer may not change queue state`など）のまま。

## Claudeのpermissions.deny（guardrail）

多層防御の1枚として、runtimeがClaude Codeの設定を書くactor（worker・planner・review job・inbox。[Roles](supervisor-lifecycle/roles.md#actorの起動actorexecutor)）の`permissions.deny`に、roleのpolicyから作った規則を入れる（`src/application/execution.rs`の`permission_deny(role)`）。

- `DAGQ_COMMANDS`はroleによって拒まれうる`dagq`のsubcommand（状態を変えるものと`watch`（`queue.watch`）・`report`（`queue.export`）・`ci failures`（`ci.read`））と、その形のどれかが要るcapabilityの表（`ready`は`task.ready`と`task.ready_bypass_review`、`ask`は`ask.open`・`finding.ask`・`ask.close`など）。roleの`grants`がどれも持たないcommandを`Bash(dagq <command>:*)`で拒む（例: workerは`Bash(dagq integrate:*)`・`Bash(dagq answer:*)`・`Bash(dagq ready:*)`・`Bash(dagq ask close:*)`、plannerは`integrate`・`answer`・`ready`・`recover`・`supervise`・`run screen`・`run send`・`planner screen`・`planner send`）。読み取りの形を持つcommand（`observe`は`--history`が読み取りなので`observe`ごと、`graph`）は表に入れない。表と`src/main.rs`の`requests`が食い違わないことはunit test（`the_denied_commands_need_what_the_table_says`）が確かめる。clapの全subcommand（`goal close`のような入れ子を含む。`DAGQ_COMMANDS`の親の項目（`dependency`）はその下を覆い、subcommandを必ず取る親（`goal`・`finding`・`proposal`）は自分の項目が要らない。自分の形を持つ親（`ask`）は自分の項目が要る）が`DAGQ_COMMANDS`か、`src/main.rs`のtestの`LEFT_OUT_COMMANDS`（読み取り（`broker status`・`broker logs`・`broker audit`を含む）、読み取りの形を持つ`graph`・`observe`）のどちらかにあることもunit test（`every_subcommand_is_denied_or_left_out_on_purpose`）が確かめ、どちらにも無いsubcommandを足すと落ちる。roleによって拒まれうるsubcommandを足したら、読み取りの形を持つものを除いて表に入れる。
- actorを名指す変数（`DAGQ_ROLE`・`DAGQ_ACTOR_ID`・`DAGQ_RUN_ID`・`DAGQ_TASK_ID`）の書き換え（`<名前>=...`・`export`・`env <名前>=`・`env -u`・`unset`）も全roleで拒む。
- inboxとplannerには最後に`Bash(cmux:*)`（`RAW_CMUX_DENIED`、`RAW_CMUX_DENIED_ROLES`）も足す（[ADR-t1228-2](../adr/2026-10-02-t1228-2-deny-raw-cmux-to-inbox-and-planner-as-a-guardrail.md)）。workerとjobには足さない（決定7）。
- workerとplannerのsession・turnの設定とreview jobの設定では、`SIGNAL_BY_NAME_DENIED`（`pkill`・`killall`）の後に置く（turnの設定（`claude-headless-settings.json`）ではその間に`AskUserQuestion`と予約の道具（`PRINT_MODE_DENIED_TOOLS`）を挟む。[非対話のworker](supervisor-lifecycle/headless-worker.md)。`[broker] mode = "required"`のrunのturnでは、それらの後に組み込みのファイルの道具（`BROKER_REQUIRED_DENIED_TOOLS`: `Read`・`Edit`・`Write`・`MultiEdit`・`NotebookEdit`・`Glob`・`Grep`・`LS`）を挟み、`permissions.allow`に`mcp__dagq-broker`と`Bash(dagq:*)`を置き、permission modeを`dontAsk`にする。`Bash`はdenyに入れない（denyがallowに勝ち、workerの`dagq ask`まで拒むため）。これもguardrailで、[Broker](broker.md#required)。review jobの設定はroleの規則だけ）。inboxの設定（`up`が書く`claude-inbox-settings.json`、[`up` / `down`](supervisor-lifecycle/up-down.md)）は`permissions.deny`だけで、roleの規則だけを持ち、`SIGNAL_BY_NAME_DENIED`は入れない。

これはguardrailでenforcementではない。Claude Codeの規則はコマンドの先頭の形しか見ないので、pathで打つ`~/.local/bin/dagq integrate`、pluginのskillが使う`"$DAGQ" ...`や`${CLAUDE_PLUGIN_ROOT}/bin/dagq ...`、subcommandの前にglobalのflagを置く`dagq --db X integrate`、`sh -c`、scriptの中からの呼び出しは通る。拒む判定はCLIの`Authorizer`（上の「適用の範囲」）がする。どちらもhostではadvisoryで、隔離は将来のsandboxのbackend（[Roles](supervisor-lifecycle/roles.md#実行のbackendとenforcement)）が担う。

このすり抜けを塞ぐために、規則をwildcardで広げること（例: `Bash(* integrate*)`や`Bash(*dagq* integrate:*)`で`"$DAGQ" integrate`・`${CLAUDE_PLUGIN_ROOT}/bin/dagq integrate`・`dagq --db X integrate`も拒む）はしない（2026-09-28 plannerの判断）。理由:

1. 誤検出が大きい。subcommandの名前は日常の語なので、workerが作業で打つ`cargo test --locked --test it integrate::`・`git log --grep integrate`・`rg integrate`・`rg 'ready'`なども拒まれ、作業が止まる。
2. 広げてもenforcementにならない。`sh -c`・script・変数に入れたcommandの名前からは今までどおり通るので、守れる範囲は少ししか増えない。
3. 拒む判定はCLIの`Authorizer`がすでにする。guardrailは「間違いで打つのを先に止める」ためのもので、pathやglobal flagの形で打った呼び出しもCLIが拒み、`authorization_denied`に記録する。

規則の形は`Bash(dagq <command>:*)`と識別の変数の書き換えのままにする。

## CIの見張り（ADR-t1920-1）

[ADR-t1920-1](../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)と[CI watch](supervisor-lifecycle/ci-watch.md)。読むコマンド`dagq ci failures`のcapabilityは`ci.read`（`Capability::CiRead`）で、user・inbox・planner・observer・supervisorが持ち、worker・wrapper・integratorとjob（review・recovery・plan review・goal review・throughput review）には許さない（一覧はworkerにはpromptで渡す予定）。`DAGQ_COMMANDS`の`("ci failures", &[C::CiRead])`により、workerとjobのClaudeの`permissions.deny`に`Bash(dagq ci failures:*)`が入る。`finding dismiss --covered-by`は今の`finding.dismiss`のままで、表は変えない。
