---
id: design-authorization
type: design
title: Authorization
status: current
created: 2026-09-27
updated: 2026-09-29
last_verified: 2026-09-29
scope: runtime
related:
  - adr-t728-1
  - adr-t728-2
  - adr-t728-3
  - design-supervisor-lifecycle-roles
  - design-security
---

# Authorization

actorが何をしてよいかは、`src/domain/authorization.rs`の`Authorizer`（`authorize(actor, capability, resource) -> Result<(), AuthorizationError>`）が決める（[ADR-t728-1](../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)の決定5）。実装は静的なpolicyの`StaticPolicy`で、roleごとの許可の一覧（`grants(role)`）と、いくつかのroleのresourceの規則でできている。一覧に無い組み合わせは拒む（default deny）。resourceの持ち主や状態を規則が要るのに呼び出し元が知らない（`None`、`Resource::Unresolved`）ときも拒む（fail closed）。判定はClaudeを起動せずにunit testできる。

host実行ではこの判定は助言的（advisory）で、sandboxでも隔離でもない。どのプロセスも`DAGQ_ROLE`を偽れる（ADR-t728-1の決定6）。信頼の区分・actorごとのcapabilityの要約・迂回できる経路・Podmanとqueue serviceへの道筋は[Security](security.md)。actorの型と環境変数は[Roles](supervisor-lifecycle/roles.md#actors)。

## Capability

| 群 | capability | 対応するCLI |
| --- | --- | --- |
| 読み取り | `queue.read` | `locate` `list` `show` `candidates` `graph`（`--out`なし） `status` `asks` `events` `timeline` `stats` `kpi` `forecast` `doctor` `broker status` `notes` `marks` `findings` `search` `related` `proposal list/show` `planners` `lint` `goal list/show` `observe --history` |
| | `queue.watch` | `watch` |
| | `queue.export` | `graph --out` `report`（ファイルを書く） |
| 計画 | `goal.write` | `goal add` `goal edit` |
| | `goal.ready` | `goal ready` |
| | `goal.close` | `goal close` |
| | `goal.review_request` | `goal review` |
| | `task.write` | `add` `edit` `draft` `set-goal` `set-paths` `set-priority` `dependency add/remove` |
| | `task.cancel` | `cancel` |
| | `task.ready` / `task.ready_bypass_review` | `ready` / `ready --bypass-review` |
| | `proposal.submit` / `proposal.withdraw` | `submit` / `proposal withdraw` |
| | `note.write` / `mark.write` | `note` / `mark` |
| workerの通信 | `ask.open` | `ask`（`blocked`をfindingに紐づけるもの以外） |
| | `session.run` | `session`（runのsession wrapper） `planner-session` |
| | `session.record` | `session-event`（`--run`を含む） |
| review / triage | `review.submit` / `triage.submit` | CLIには無い。jobのverdictはデータとしてsupervisorが読み、遷移に写す |
| | `review.prepare` | `review`（review.mdを書く） |
| 観察 | `finding.record` / `finding.resolve` / `finding.dismiss` | `finding record` / `finding resolve` / `finding dismiss` |
| | `finding.ask` | `ask --kind blocked --finding` |
| | `observe.run` | `observe`（`--history`を除く） |
| 人との対話 | `ask.answer` / `ask.close` | `answer` / `ask close` |
| | `planner.open` | `plan` |
| schedulerの遷移 | `scheduler.supervise` | `supervise` |
| | `run.recover` | `recover` |
| | `service.lifecycle` | `up` `down` `broker start` `broker stop` |
| | `service.install` | `install` `auto-update` |
| | `queue.admin` | `init` `migrate` `rebind` |
| 着地 | `landing.request` | `integrate`（Integratorへの依頼。[ADR-t728-2](../adr/2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)） |
| | `landing.land` / `landing.push` | Integratorの内側 |
| 予約 | `reserved.filesystem_read` `reserved.filesystem_write` `reserved.network` `reserved.secret_read` | 後の段（sandbox）。この段では誰にも与えない |

CLIのコマンドからcapabilityとresourceへの写しは`src/main.rs`の`requests`で、matchに既定の枝を置かないので、コマンドを足すと写しを書くまでcompileが通らない。`queue.read`だけのコマンドはqueueをread-onlyの接続で開く（`reads_only`）。

## Resource

`Queue`・`Goal`・`Task`（idと、分かれば状態）・`Run`（idと、分かればtask）・`Ask`（idと、分かればrun）・`NewAsk`（開くaskのkindと、コマンドが名指すrunとtask）・`Proposal`（idと、分かれば出したplannerのactor id）・`Finding`・`Planner`・`Unresolved`（読めなかったid）。CLIの写し（`requests`）は引数にあるものだけを入れ、持ち主と状態は`None`のまま渡す。計画系のコマンドはapplicationの層（下の[適用の範囲](#適用の範囲)）がqueueからtaskの状態とproposalの持ち主を読んで埋めてから判定する。proposalの持ち主は`proposals.owner_actor_id`（`0046_proposal_owner_actor.sql`）で、`submit`（新しいproposalと、差し戻しの後の出し直し）が出したactorのid（`planner:<id>`など）を書く。plan reviewが人を待つproposalをそのまま出し直す`submit --proposal`は持ち主を変えない（古いバイナリの出し直しも列を書かない）。plan reviewが差し戻して`revising`のproposalは、reviseを送ったplanner（`revise_planner_id`。持ち主が閉じていたときにruntimeが立てたplannerや、reopenしたproposalを受け取ったplanner）を持ち主とする。migrationより前に出したproposalは`NULL`で、plannerは取り下げられない（fail closed。userとinboxは取り下げられる）。`DAGQ_ACTOR_ID`より前に開いたplannerのid `planner`（roleの名前だけでactorを名指さない）は、どのproposalの持ち主にもならない。

## Policy

| role | 許すcapability | resourceの規則 |
| --- | --- | --- |
| user | 予約と`review.submit` `triage.submit` `landing.land` `landing.push`を除く全て | なし |
| inbox | userと同じ（人の言葉での代行。[ADR-t728-3](../adr/2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)の決定1） | なし。人自身の操作との区別は記録が持つ |
| planner | 読み取り・`queue.watch`・`queue.export`・`goal.write`・`goal.close`・`task.write`・`task.cancel`・`proposal.submit`・`proposal.withdraw`・`note.write`・`mark.write`・`ask.open`・`session.run`・`session.record`・`finding.resolve`・`finding.dismiss`・`planner.open`・`service.lifecycle`・`service.install`・`queue.admin` | runにはnote（`note --run`）だけを書け、ほかは何もできない（runに紐づくaskも開けない）。開けるaskは`planner_question`だけ。taskの変更（`task.write`・`task.cancel`）はdraft・submitted・readyのものだけ（状態が不明なら拒む）。noteはどの状態のtaskにも書ける。`proposal.withdraw`は自分（actor id）が出したproposalだけ。`planner-session`は自分のplannerだけ |
| worker | 読み取り・`ask.open`・`note.write`・`session.run`・`session.record` | 読み取り以外は自分のrun（`DAGQ_RUN_ID`）・そのtask（`DAGQ_TASK_ID`）・自分のrunのaskだけ。開けるaskは`worker_question`だけで、`--run`なら自分のrun、`--task`だけなら自分のtask（`DAGQ_RUN_ID`の無いworkerは何も持たない）。`session`と`session-event`は自分のrunのものだけ（runtimeのwrapperとhookはworkerの環境のまま打つ） |
| review-job | 読み取り・`review.submit` | `review.submit`は自分のrunだけ |
| recovery-job | 読み取り・`triage.submit` | `triage.submit`は自分のrunだけ |
| plan-review-job・goal-review-job・throughput-review-job | 読み取りだけ | throughput-review-jobは[スループットの見直し](supervisor-lifecycle/throughput-review.md)（ADR-t996-1） |
| observer | 読み取り・`queue.watch`・`finding.record`・`finding.resolve`・`finding.ask` | `finding.resolve`と`finding.ask`はfindingだけ |
| supervisor | 読み取り・`queue.watch`・`queue.export`・`goal.close`・`task.cancel`・`task.ready`・`note.write`・`ask.open`・`ask.close`・`session.run`・`review.prepare`・`finding.record`・`finding.resolve`・`finding.dismiss`・`observe.run`・`planner.open`・`scheduler.supervise`・`run.recover`・`service.lifecycle`・`service.install`・`queue.admin`・`landing.request` | なし。`queue.admin`は自動更新が新しいバイナリで`install`と同じ確認（`migrate --check`・`migrate`・使い捨てのqueueの`init`）をするため。`ask.answer`・`task.ready_bypass_review`・`landing.land`・`landing.push`は持たない |
| wrapper | 読み取り・`session.run`・`session.record` | なし |
| integrator | 読み取り・`landing.land`・`landing.push` | なし |

旧値の`DAGQ_ROLE=reviewer`はreview-jobとして読む（[Roles](supervisor-lifecycle/roles.md#cliでの解釈)）。jobの環境には今`DAGQ_RUN_ID`が無いので、`review.submit`・`triage.submit`は持ち主が分からず拒まれる。verdictは今までどおりデータとしてsupervisorが読む。

askのkindはroleごとに決まる（`opens_ask`）: userとinboxとsupervisorはCLIが開けるどのkindも、workerは`worker_question`、plannerは`planner_question`だけを開ける。observerの`ask --kind blocked --finding`は`ask.open`ではなく`finding.ask`で、finding（`Resource::Finding`）に対して判定する。kindがroleに合わなければ理由`not of this kind`で拒む。

`AuthorizationError`はroleと拒んだcapabilityと理由（`not granted`・`reserved`・`not on this resource`・`not of this kind`）だけを出し、actor id（session idを含みうる）やresourceのid・本文を出さない。

## 適用の範囲

### 計画系のコマンド（application）

計画系のコマンド（`add`・`edit`・`submit`・`draft`・`ready`（`--bypass-review`を含む）・`cancel`・`dependency add/remove`・`goal add/edit/ready/close/review`・`set-goal`・`set-paths`・`set-priority`・`proposal withdraw`）は、`src/application/commands/planning.rs`の`Planning`が全てのroleについて判定してからstoreを呼ぶ（task 732）。CLI（`src/main.rs`の`execute()`）はparseと出力だけをし、これらのコマンドでstoreの変更を直接呼ばない。`Planning`は呼び出し元の`ActorContext`・`Authorizer`（`StaticPolicy`）・port `PlanningStore`（`SqliteQueue`が`src/infrastructure/planning.rs`で実装する）を受け取り、コマンドごとに次のcapabilityとresourceを問う。

| コマンド | capability | resource |
| --- | --- | --- |
| `add` | `task.write` | `--goal`のgoal、無ければqueue |
| `edit` `set-goal` `set-paths` `set-priority` `draft` `dependency add/remove` | `task.write` | task（queueにある状態） |
| `ready` / `ready --bypass-review` | `task.ready` / `task.ready_bypass_review` | task（状態） |
| `cancel`（`--duplicate-of`を含む） | `task.cancel` | task（状態） |
| `submit` / `submit --proposal ID` | `proposal.submit` | queue / proposal（持ち主） |
| `proposal withdraw` | `proposal.withdraw` | proposal（持ち主） |
| `goal add` / `goal edit` / `goal ready` / `goal close` / `goal review` | `goal.write` / `goal.write` / `goal.ready` / `goal.close` / `goal.review_request` | queue / goal |

policyは上の表のまま: plannerは今の権限（draft・submitted・readyのtaskの変更と`cancel`、goalの追加・編集・close、自分のproposalの取り下げ）を持ち、`ready`（`--bypass-review`を含む）・`goal ready`・`goal review`とin_progress以降のtaskの変更は持たない。`ready`はuserとinbox（人の言葉での代行。区別はeventのactorが持つ）。worker・4つのjob・observer・wrapper・integratorは計画系を何もできない。supervisorはCLIからは`ready`・`cancel`・`goal close`だけ。capabilityをどのresourceにも持たないroleは、storeを読む前に拒む（taskやproposalが無くても拒否になり、記録のresourceは状態と持ち主が`null`）。capabilityを持つroleで、taskやproposalが見つからないときは拒否ではなく、そのerror（`task N does not exist`など）になる。

拒んだときは、queueのevent `authorization_denied`（taskにもgoalにも紐づかないqueueのevent。actorの列は拒まれた呼び出し元）を記録し、`AuthorizationError`を返す。payloadは`role`・`capability`・`reason`（`not granted`・`reserved`・`not on this resource`）・`resource`（`kind`と`id`、taskなら`status`、proposalなら`owner`）。記録に失敗しても拒否は拒否のまま返す。observerのこのeventは、observerの次の起動を決める「自分以外のevent」に数えない。

CLIのerrorは`{"error": ..., "denied": {"role", "capability", "reason"}}`で、`error`はobserverなら`observer may not change queue state`、4つのjob（と`reviewer`）なら`reviewer may not change queue state`（今までの文言）、それ以外は`<role> may not <capability> (<reason>)`。

### 対話と記録のコマンド（application）

`ask`・`ask close`・`answer`・`note`・`mark`（`--retract`を含む）・`finding record/resolve/dismiss`は、`src/application/commands/dialogue.rs`の`Dialogue`が全てのroleについて判定してからstoreを呼ぶ（task 733）。port `DialogueStore`は`src/infrastructure/dialogue.rs`の`DialogueQueue`（`SqliteQueue`と、askの通知に使うcheckoutとcmux）が実装する。拒否の記録（`authorization_denied`）とerrorの形は計画系と同じで、判定と記録の共通の部分は`src/application/commands/mod.rs`の`Gate`が持つ。

| コマンド | capability | resource |
| --- | --- | --- |
| `ask --kind blocked --finding ID` | `finding.ask` | finding |
| `ask`（それ以外） | `ask.open` | `NewAsk`（kind、`--run`、`--task`） |
| `answer` / `ask close` | `ask.answer` / `ask.close` | ask（queueにあるrun）。capabilityを持たないroleはaskを読む前に拒む（記録のrunは`null`） |
| `note` | `note.write` | `--task`のtask・`--run`のrun・`--goal`のgoal |
| `mark` / `mark --retract` | `mark.write` | queue |
| `finding record` | `finding.record` | 対象のtask・run・goal、`--queue`ならqueue |
| `finding resolve` / `finding dismiss` | `finding.resolve` / `finding.dismiss` | finding |

`asked_by`・noteとfindingの`by`・markの`by`は、呼び出し元が渡した値ではなく`Dialogue`がactorから入れる（roleの名前、userは`human`。綴りは今までどおり）。answerはuserとinboxだけで（[ADR-t728-3](../adr/2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)の決定4）、worker・4つのjob・observer・planner・supervisor・wrapper・integratorは`ask.answer`を、worker・4つのjob・observer・planner・wrapper・integratorは`ask.close`を持たない（supervisorは閉じられる）。

#### answerの権限の出どころと承認の分類

answerは誰の権限で書かれたか（`AnswerAuthority`）を記録する（ADR-t728-3の決定2）。値はactorの型（`ActorContext::answerer`）から決め、answerの文やinboxのpromptから推さない。

| 値 | 書くもの | `answered_by` |
| --- | --- | --- |
| `user` | `DAGQ_ROLE`の無い呼び出し（人自身） | `person` |
| `delegated` | inbox（人の言葉での代行） | `inbox` |
| `runtime` | runtimeが自分で閉じる・取り下げる・置き換えるask | `runtime` |

askの行には`answer_authority`と`answer_approval`（`0047_answer_authority.sql`）を、`ask_answered`のpayloadには`authority`と`approval`を書く。`answered_by`と`asked_by`の値の綴りは変えない（jobのaskの`asked_by`の改名とhumanからuserへの改名はgoal 48のtask 502が持つ）。eventのactorの列（roleとid）も同じ区別を持つ。migrationより前のanswerと古いバイナリのanswerは両方とも`NULL`で、JSONには出ない。

承認に当たるask（後のgoalで人だけに限るときの土台。ADR-t728-3の決定3）はdomainが分類する（`AskKind::is_approval`と`answer_approves`）: kindが`approve_landing`・`decide`・`approve_plan`・`approve_goal`・`approve_update`・`update_failed`のaskのanswer、または`blocked`・`stalled`のaskがoptionに出した`propose` / `dismiss`（runtimeがfindingに適用する答え）。そのanswerは`answer_approval`が`1`、payloadの`approval`が`true`になる。runtimeが自分で閉じる・取り下げる・置き換えるanswer（`authority`が`runtime`）は何も承認しないので、kindに関わらず`0` / `false`にする。この段では承認を人だけに限る強制はしない。

### runtimeの操作系のコマンド（application）

runtimeの操作系のコマンドは、`src/application/commands/operations.rs`の`Operation`が名指すcapabilityとresourceで、全てのroleについてコマンドが何かをする前に判定する（task 734）。CLI（`src/main.rs`の`execute()`）はqueueの場所を決めた直後、queueを開く・作る・移す前、ログを開く前に`operations::authorize`を呼び、`requests`もこれらのコマンドの写しを`Operation::request`から取る。操作そのもの（`up`・`install`など）は今の場所のままで、ここはその入口の判定だけ。着地はこの入口の後で[Integrator](#着地とpushintegrator)がもう一度判定する（task 736）。

| コマンド | capability | resource |
| --- | --- | --- |
| `init` `migrate`（`--check`を含む） `rebind` | `queue.admin` | queue |
| `install` / `auto-update` | `service.install` | queue |
| `up` `down` `broker start` `broker stop` | `service.lifecycle` | queue |
| `plan` | `planner.open` | queue |
| `supervise` | `scheduler.supervise` | queue |
| `observe`（`--history`を除く） | `observe.run` | queue |
| `integrate ID` / `integrate --next` | `landing.request` | task / queue |
| `recover RUN` | `run.recover` | run（読めないidは`Unresolved`） |
| `review ID` | `review.prepare` | task |
| `session --run RUN` | `session.run` | run（読めないidは`Unresolved`） |
| `planner-session --planner ID` | `session.run` | planner |
| `session-event open/close` | `session.record` | hookが記録するspan: inboxのspanはqueue、plannerのspanは`DAGQ_PLANNER_ID`のplanner（無いか、`DAGQ_ACTOR_ID`より前に開いたworkspaceならqueue）、spanの無いsession（run）は`--run`、無ければ`DAGQ_RUN_ID`のrun（どちらも無ければ`Unresolved`） |

結果（上のPolicyの表から決まる）:

- userとinboxは全てを打てる（inboxは人の言葉での代行で、dagq-recoverの手作業の`integrate`・`recover`・`review`を含む。区別は記録のactorが持つ）
- plannerは人に頼まれた`up`・`down`・`install`と、`init`・`migrate`・`rebind`・`plan`、自分のplannerの`planner-session`と`session-event`を打てる（ADR-t728-1の決定7のとおり今の権限のまま）。`integrate`・`recover`・`review`・`supervise`・`observe`・`session`は拒む
- worker・4つのjob・observerは`integrate`・`recover`・`install`・`auto-update`・`up`・`down`・`init`・`migrate`・`rebind`・`plan`・`supervise`・`observe`・`review`を拒む。workerは自分のrunの`session`と`session-event`だけを打てる。別のrunのもの、inboxやplannerのspanを名乗るもの（`DAGQ_SESSION_KIND`）、`planner-session`は拒む
- 拒否は`authorization_denied`として、拒まれた呼び出し元をactorにしてqueueに記録する（`src/infrastructure/denials.rs`の`QueueDenials`が、判定の後でだけqueueを開く）。queueが無い・このバイナリが開けない（`init`の前、`migrate`の前）ときは記録できず、拒否は拒否のまま返す。errorの形は計画系と同じ

#### 制御側の起動としての`supervise`と`auto-update`

`supervise`と`auto-update`はruntimeの制御側（信頼する制御側。ADR-t728-1の決定2）のプロセスを始めるコマンドで、呼び出し元の環境で判定する:

- `supervise`は`scheduler.supervise`を持つuser・inbox・supervisorだけが打てる。`up`はin-cmuxのsupervisorのworkspaceを`DAGQ_ROLE=supervisor`の環境で開き、launchdの登録は`DAGQ_ROLE`を持たない（user）ので、`up`を打ったのがplannerでも起動は通る。exec の引き継ぎ（`--handoff-token`）は同じ環境のまま自分をexecするので、同じroleで判定される。plannerは`supervise`を直接打てない（ADR-t728-1の決定7の表にschedulerは無い）。起動したプロセスのeventのactorは、呼び出し元ではなく`supervisor:<pid>`（`event_actor`）
- `auto-update`は`install`と同じ`service.install`で、supervisorが自分の環境（`supervisor:<pid>`）で起動する。`install`と`auto-update`は新しいバイナリで`migrate --check`・`migrate`・使い捨てのqueueの`init`と`list`・`up`を子プロセスとして同じ環境のまま打つので、それを打つroleは`queue.admin`と`service.lifecycle`も持つ（supervisorに`queue.admin`を足したのはこのため）。人とinboxとplannerも`install`と同じ権限で打てる（手順の再現と復旧）。`observe`（`observe.run`）もsupervisorが自分の環境で起動し、その中のagentだけがobserverになる

host実行ではこれも助言的で、`DAGQ_ROLE`を外せば誰でもuserになる（ADR-t728-1の決定6）。wrapperとhookをworkerの環境から分けて、信頼する制御側のwrapperのactor（`ActorRole::Wrapper`）として判定することは、queue service / brokerのgoal 38の後続にする。

### 着地とpush（Integrator）

着地（rebase・再検証・squash・mainの更新）とpushは`src/application/integrate.rs`の`Integrator`だけが行う（[ADR-t728-2](../adr/2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)、task 736）。Integratorはrole `integrator`のactor（`integrator:<pid>`）で、`landing.land`と`landing.push`を持つのはこのroleだけ。`Integrator::acting_as(actor)`は`StaticPolicy`がこの2つを許すactorでなければ拒み（integrator以外の全てのroleが拒まれることをunit testが確かめる）、着地の関数（`begin`・`land_integrating`・`push_main`）はmoduleの外から呼べない。

- **依頼**: supervisorの着地のthreadと、人（とinboxの代行）の`integrate`は`IntegrationRequest`（`requester`＝依頼者のactor、run、戻り先のstatus、着地先のmain、integrationのslotのtoken）を作り、`Integrator::land`に渡す。`integrate`は`Integrator::approve`がrunを選び、依頼者が`landing.request`を持つことを確かめてから（持たなければ何も記録せずに拒む）`integration_approved`を依頼者として記録し、slotを取って依頼を返す。supervisorはslotを自分で取り（`integration_started`はsupervisorの記録）、依頼者は`supervisor:<pid>`
- **Integratorの検査**（`landing_refusal`）: 依頼者が`StaticPolicy`でそのrunに`landing.request`を持つこと（user・inbox・supervisor）、runが`integrating`で依頼のtokenがleaseを持つこと、runが着地を承認されている（`integration_approved`。人の`integrate`か`approve_landing`の`land`）か最新のreviewのverdictが`pass`であること。どれかが欠ければ着地せず、slotを持っていれば`abort_integration`で返して（`integration_error`、`code: other`）errorにする。reviewの`pass`は着地の必要条件で、この後のreceiptの再検査・rebase・検証コマンド・scopeの判定が落ちれば着地しない（ADR-t728-2の決定3）
- **記録**: `Integrator::land`の間、queueのeventのactorはIntegrator（role `integrator`、id `integrator:<pid>`）で、`requested_by`は依頼者のactor id（人は`user`、inboxは`inbox`、supervisorは`supervisor:<pid>`）。`verification_command`・`integration_rebased`・`integration_deferred`・`run_integrated`・`push_finished` / `push_skipped` / `push_failed`などがこれにあたる。依頼と記録を切り替えるのはport `RunLog::act_as`と`RunLog::request_as`（抜けるときに`RunLog::restore_request`で入る前の依頼者に戻す）
- **push**: `MainRemote::push_main`は`PushGrant`を引数に取り、`PushGrant`はIntegratorが`landing.push`を確かめてからしか作れない（fieldがmoduleの外から見えない）ので、`GitRepository`のpushはIntegratorの外から呼べない

host実行ではIntegratorはsupervisorや`integrate`と同じプロセスとユーザーで動き、この境界は論理的なもの（ADR-t728-2の決定4）。

### ほかのコマンド（CLIの入口）

上の3つ以外のコマンド（読み取り・`watch`・`graph --out`・`report`）は状態を変えない。これらは今もobserverと4つのjob（と旧値`reviewer`）だけを`src/main.rs`の`check_access`が`requests`の全てで`StaticPolicy`に通す（記録はしない）。observerに許すもの（読み取り、`watch`）とjobに許すもの（読み取りだけ）は、以前の`observer_access` / `reviewer_access`の一覧と同じ。

## Claudeのpermissions.deny（guardrail）

多層防御の1枚として、runtimeがClaude Codeの設定を書くactor（worker・planner・review job。[Roles](supervisor-lifecycle/roles.md#actorの起動actorexecutor)）の`permissions.deny`に、roleのpolicyから作った規則を入れる（task 738。`src/application/execution.rs`の`permission_deny(role)`）。

- `DAGQ_COMMANDS`は状態を変える`dagq`のsubcommandと、その形のどれかが要るcapabilityの表（`ready`は`task.ready`と`task.ready_bypass_review`、`ask`は`ask.open`・`finding.ask`・`ask.close`など）。roleの`grants`がどれも持たないcommandを`Bash(dagq <command>:*)`で拒む（例: workerは`Bash(dagq integrate:*)`・`Bash(dagq answer:*)`・`Bash(dagq ready:*)`・`Bash(dagq ask close:*)`、plannerは`integrate`・`answer`・`ready`・`recover`・`supervise`）。読み取りの形を持つcommand（`observe`は`--history`が読み取りなので`observe`ごと、`graph`）は表に入れない。表と`src/main.rs`の`requests`が食い違わないことはunit test（`the_denied_commands_need_what_the_table_says`）が確かめる。clapの全subcommand（`goal close`のような入れ子を含む。`DAGQ_COMMANDS`の親の項目（`dependency`）はその下を覆い、subcommandを必ず取る親（`goal`・`finding`・`proposal`）は自分の項目が要らない。自分の形を持つ親（`ask`）は自分の項目が要る）が`DAGQ_COMMANDS`か、`src/main.rs`のtestの`LEFT_OUT_COMMANDS`（読み取り（`broker status`を含む）、`watch`、ファイルを書き出す`report`、読み取りの形を持つ`graph`・`observe`）のどちらかにあることもunit test（`every_subcommand_is_denied_or_left_out_on_purpose`、task 851）が確かめ、どちらにも無いsubcommandを足すと落ちる。状態を変えるsubcommandを足したら表に入れる（task 851の列挙で、隠しcommandの`release-update`（`service.install`）を足した）。
- actorを名指す変数（`DAGQ_ROLE`・`DAGQ_ACTOR_ID`・`DAGQ_RUN_ID`・`DAGQ_TASK_ID`）の書き換え（`<名前>=...`・`export`・`env <名前>=`・`env -u`・`unset`）も全roleで拒む。
- 順は`SIGNAL_BY_NAME_DENIED`（`pkill`・`killall`）の後。

これはguardrailでenforcementではない。Claude Codeの規則はコマンドの先頭の形しか見ないので、pathで打つ`~/.local/bin/dagq integrate`、pluginのskillが使う`"$DAGQ" ...`や`${CLAUDE_PLUGIN_ROOT}/bin/dagq ...`、subcommandの前にglobalのflagを置く`dagq --db X integrate`、`sh -c`、scriptの中からの呼び出しは通る。拒む判定はCLIの`Authorizer`（上の「適用の範囲」）がする。どちらもhostではadvisoryで、隔離は将来のsandboxのbackend（[Roles](supervisor-lifecycle/roles.md#実行のbackendとenforcement)）が担う。

このすり抜けを塞ぐために、規則をwildcardで広げること（例: `Bash(* integrate*)`や`Bash(*dagq* integrate:*)`で`"$DAGQ" integrate`・`${CLAUDE_PLUGIN_ROOT}/bin/dagq integrate`・`dagq --db X integrate`も拒む）はしない（2026-09-28 plannerの判断、task 851）。理由:

1. 誤検出が大きい。subcommandの名前は日常の語なので、workerが作業で打つ`cargo test --locked --test it integrate::`・`git log --grep integrate`・`rg integrate`・`rg 'ready'`なども拒まれ、作業が止まる。
2. 広げてもenforcementにならない。`sh -c`・script・変数に入れたcommandの名前からは今までどおり通るので、守れる範囲は少ししか増えない。
3. 拒む判定はCLIの`Authorizer`がすでにする。guardrailは「間違いで打つのを先に止める」ためのもので、pathやglobal flagの形で打った呼び出しもCLIが拒み、`authorization_denied`に記録する。

規則の形は`Bash(dagq <command>:*)`と識別の変数の書き換えのままにする。
