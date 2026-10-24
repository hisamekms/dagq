---
id: design-queue-service
type: design
title: Queue service
status: current
created: 2026-10-02
scope: runtime
tags:
  - security
  - runtime
related:
  - adr-t1233-1
  - adr-t1233-4
  - adr-t1233-5
  - adr-t1222-1
  - design-supervisor-lifecycle-observer
  - adr-t728-1
  - design-security
  - design-authorization
  - design-persistence
  - design-supervisor-lifecycle-commands
  - adr-t1662-3
  - design-measurement
  - adr-t3102-1
  - adr-t3102-2
  - adr-t3102-3
  - adr-t3105-1
  - design-queue-service-host-cli
---

# Queue service

hostで動き、queue DBを開いてユースケース単位のAPIを、service側で認可して提供するプロセス。
決定の理由は[ADR-t1233-1](../adr/2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)（制御側と実行側の分け方・serviceとAPIと認可・unix socket・段）、[ADR-t1233-4](../adr/2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)（起動・停止の責任・落ちたときの知らせ方・tokenによるprincipalの認証）、[ADR-t1233-5](../adr/2026-10-02-t1233-5-read-use-cases-read-scope-by-role-and-codex-sandbox-reach.md)（読み取りの範囲）。

今の段（段(2)・(3)）: serviceは下の[ユースケース](#ユースケース)（findingの書き込みはCodexのobserverの経路、[ADR-t1222-1](../adr/2026-10-02-t1222-1-codex-observer-writes-through-the-queue-service.md)）と、読み取りのroleのjobとworkerが打つqueue全体の読み取り（[読み取りのユースケース](#読み取りのユースケース)）を答える。
**worker（resumeを含む）・headlessのjob・observerのdagqはクライアントモードで動き、そのプロセスにはqueue DBのpathを渡さない**（[クライアントモード](#クライアントモード)）。
session wrapperとhookはwrapperのprincipalでserviceを通り、人・inbox・plannerのCLIは[hostのCLI](queue-service-host-cli.md)。
supervisorはclaim・lease・runの状態とevent・tokenをserviceで行い、着地とjobの結果はDBを開く（[`supervise`](supervisor-lifecycle/supervise.md#queue-serviceの経路)）。
例外はexecutorのsessionのstepだけで、制御側が対象ごとに発行するtelemetry専用のprincipalで[`telemetry_report`](#telemetry_report)を通る（[ADR-t1662-3](../adr/2026-10-04-t1662-3-executors-send-telemetry-and-commands-are-classified-by-shape.md)決定13）。

名前: この文書の「broker」はqueueのbroker（[Queue broker](queue-broker.md)）のこと。
brokerはrun tokenを検証した要求をそのtokenのままserviceのsocketへ転送するので、serviceの認証と認可は変わらない。

## 置き場所と形

queueのディレクトリ（`dagq locate`の`db`のあるディレクトリ）の`service/`に置く（ファイルとmodeは`infrastructure::queue_service`のmodule doc）。

| ファイル | 中身 |
| --- | --- |
| `queue.sock` | unix socket。pathが長すぎるqueueで`/tmp`の下に置くときは、そのdirがlinkでないことを確かめる。どの呼び出し元もqueueのディレクトリから同じpathを求める（`socket_path`） |
| `lock` | serviceが生きている間`flock`の排他で持ち、2つ目のserviceは取れずに止まる。見る側（status・supervisor）は共有で試し、取れれば「居ない」と判定する（見る側どうしは互いを妨げない）。止めるときにsignalを送るのは、`hello`が答えたpidか、記録のpidが今このqueueの`service serve`を動かしている（`ps`で確かめる）ときだけ |
| `state.json` | 動いているserviceの記録。止まるときに消す |
| `service.log` | `up`・supervisor・`service start`が起動したserviceのstdoutとstderr |
| `tokens/<値のsha256>.json` | tokenのprincipalと発行の時刻 |
| `credentials/<keyのsha256>` | tokenの値（keyとinboxのsessionごとのfileは`Principal::credential_key`）。呼び出し元にはこのfileのpathを渡し、値をenvにもargvにも置かない（ADR-t1233-4決定4）。worktreeとrun dirの外 |

1つの接続に1つの要求で、要求は1行のJSON、答えも1行のJSONで、答えた後に閉じる。
上限（`MAX_ANSWER_BYTES`）を超える答えは送らず`response_too_large`で断る（ADR-t1233-5）。
transactionはserviceの中で完結し、クライアントとの往復で分けない（ADR-t1233-1決定3）。
要求ごとにDBへの接続を開き、principalのactorで書く（eventの`actor`は呼び出し元のprincipal）。
要求と答えの形は`domain::queue_service`の`ServiceRequest`と`ServiceResponse`が持つ。

## API versionと互換

互換はqueueのschemaではなくAPIのversionで判定する（ADR-t1233-1決定2）。
`API_VERSION`と`MIN_API_VERSION`を`src/domain/queue_service.rs`が持つ。

- serviceは要求の`api_version`が`MIN_API_VERSION`〜`API_VERSION`の外なら、tokenも読まずに`api_version_mismatch`で断る
- 答えは常に`api_version`と`min_api_version`を持ち、クライアントは自分の版がその範囲に入るか（`understands`）で、知らない版のserviceを使わない（判定は`answer_of`のdoc comment）
- 古い版の呼び出し元が読めない変更（欄の意味の変更・ユースケースの削除）で`API_VERSION`を上げ、新しい版の呼び出し元を断るまでの間は`MIN_API_VERSION`を据え置く

## principalとtoken

serviceは呼び出しのprincipal（`Principal`）をtokenから決め（tokenの無い人は[hostのCLI](queue-service-host-cli.md#principal)）、クライアントが名乗るrole（`DAGQ_ROLE`）は認可に使わない（ADR-t1233-1決定4、ADR-t1233-4決定4）。

- 発行: 制御側だけが`queue_service::issue`で発行する。
  AI actorにtokenを作るコマンドは無い。
  発行できるprincipalはAI actorと、executorのtelemetry専用のprincipal（下の「executorのtelemetryのprincipal」）・登録を対象に持つsupervisor・session wrapper・inbox・plannerだけで、残りの制御側（integrator）は段(5)まで今のままDBを直接開く（`Principal::check`）。
  同じactor idに発行し直すと前のtokenは失効する（resumeの発行し直し）。
  発行するのはactorの起動の1か所（applicationのport `ServiceAccess`）:
  - workerのtoken: supervisorがclaimとresumeでrunのsession wrapperをbackgroundで起動するときに、serviceにrunのleaseを確かめさせて発行する。
    wrapperの環境には入れず、session wrapperがagentのturnを起動するときにそのfileのpathを渡す。
  - wrapperのtoken: 制御側がrunかplannerのwrapperを起動するときに、その対象のrole `wrapper`のprincipalに発行する。
    wrapperはfileを自分のactor idから引くので、envとargvに値もpathも無い
  - jobのtoken: supervisorかjobを起動するコマンドが起動のときに発行し、jobのプロセスが終わったときに失効させる（`ServiceAccess::revoke_on_exit`）
  - inboxとplannerのtoken: [hostのCLI](queue-service-host-cli.md#principal)
- 失効: `queue_service::revoke`（`Principal::credential_key`）が値とprincipalの記録を消す。
  inboxのtokenはsessionの終わりと開き直しで、plannerとそのwrapperのものはplannerを閉じたときに失効し（ADR-t3102-1決定2）、serviceは消える前でも断る。
  sweepはtokenを置く名前で消すので、開き直したinboxのものは残る（`swept_token_key`）。
  加えてserviceは、principalがrunを名指すtokenを、そのrunが終わったときに断る（runの終わりで失効。`run_holds_token`）。
  workerのtokenは、supervisorがrunの終わり（slotのrunが着地・failed・interruptedで休むとき）に`ServiceAccess::revoke`で消す。
  そこで消さなかったもの（cancelやsupervisorが居ない間に終わったrunなど）は、supervisorが起動の最初のpassと終わったrunのsweepのたびに`ServiceAccess::sweep`で消す（`run_token_ended`。wrapperのtokenも）。
  resumeを待つrun（`needs_session`）はtokenを保つので消さず、resumeは今までどおり発行し直す
- executorのtelemetryのprincipal（ADR-t1662-3決定9〜11・13）: role `executor-telemetry`で、対象を1つだけ持つ（runの対象は`run_id`、runを持たない対象は種類とid。`domain::session_step::StepTarget`）。
  actor idは対象から作り、runの対象のtokenはworkerのものと同じくrunの終わりで断る。
  発行と失効の口は制御側だけが呼ぶapplicationのport `TelemetryTokens`で、AI actorのコマンドからは呼べない。
  runの対象のtoken: supervisorだけが自分のleaseの下で、claim・resume・runの引き継ぎとexecutorの起動の前に発行し、runの終わりとleaseの喪失（そのleaseのものだけ）で失効させる（runを持たない対象のものとeventは`application::telemetry_tokens`のmodule doc）。
  executorは`telemetry_credential`で場所を引き、agentのenv・argv・worktree・run dirには渡さない
- 断り: 認証できない要求は`unauthenticated`で断り、queueのevent `queue_service_unauthenticated`（tokenの値は書かない）に残す（理由は`QueueService::authenticate`のdoc comment）。
- hostの呼び出し元に渡すenvはsocketのpathとtokenのfileのpathで、containerのrunにはsocketの代わりにbrokerのURLを渡す（[クライアントモード](#クライアントモード)、`domain::queue_service::CLIENT_ENV`）。
  envの名前の落とし穴は`CREDENTIAL_FILE_ENV`のdoc comment
- host構成では同じユーザーのプロセスが他のrunのtokenのfileもDBも読めるので、tokenは誤りを止めて記録を正しくするためのもので、security boundaryではない（ADR-t1233-4決定5、[Security](security.md#host実行は助言的advisory)）

## ユースケース

`src/application/queue_service.rs`の`QueueService::handle`が、versionの検査、宛てのqueueの検査、tokenの検査、ユースケースの順に行う。
認可はCLIと同じapplicationの境界（`Dialogue`・`Gate`と`StaticPolicy`）を、principalのactorでservice側で通す。
拒否はCLIと同じ`authorization_denied`（拒まれたprincipalがactor）に記録し、`authorization_denied`のcodeで返す。
そのerrorはeventと同じrole・capability・reasonを`denied`に持ち（`ServiceDenial`）、知らない欄を読み飛ばす古いクライアントはcodeとmessageだけを読むので、API versionを上げない。
codeの意味は`ServiceErrorCode`のdoc comment。

paramsはそのコマンドのoption、答えはそのコマンドの出力（paramsの形は`src/application/queue_service.rs`の各params型）。

| use_case | コマンド | capabilityとresource |
| --- | --- | --- |
| `hello` | なし | tokenを要らない（生存と版の確認だけ） |
| `ask` | `dagq ask` | `ask.open`（new ask）、findingに紐づく`blocked`は`finding.ask`（下） |
| `show` | `dagq show` | `queue.read`（task） |
| `note` | `dagq note` | `note.write` |
| `proposal_list` | `dagq proposal list` | `queue.read`（queue） |
| `proposal_show` | `dagq proposal show` | `queue.read`（queue） |
| `finding_record` | `dagq finding record` | `finding.record`（対象）。findingへの合流と根拠の検査は、CLIと同じ`record_finding`の1つのtransaction |
| `finding_resolve` | `dagq finding resolve` | `finding.resolve` |
| `finding_dismiss` | `dagq finding dismiss`（下の「CIの見張り」） | `finding.dismiss`（ADR-t1222-1決定2） |
| `agent_`の各ユースケース | `dagq agent`の同名のサブコマンド | `eval.*`。[Agent eval](agent-eval.md) |
| `replan_propose` | `dagq replan propose` | `replan.propose`（run） |
| `telemetry_report` | なし（executorのTelemetrySink） | `telemetry.report`（tokenの対象）。[telemetry_report](#telemetry_report) |
| `session_run`・`session_record` | なし（wrapper。`SessionOp`） | `session.run`・`session.record`（wrapperの対象） |
| `watch_poll` | hostの`dagq watch` | `queue.watch` |
| 読み取り | そのコマンド | CLIと同じcapability。[読み取りのユースケース](#読み取りのユースケース) |
| 計画の書き込みと`mark` | 同名のコマンド | CLIと同じ。[hostのCLI](queue-service-host-cli.md#計画のユースケース) |

observerのfindingに紐づく`blocked`のaskは`ask`のユースケース（capabilityは`finding.ask`）で送る。
findingの書き込みとそのaskはCLIと同じ`Dialogue`をprincipalのactorで通るので、eventの書き手は`observer`、eventのactorはjobのactor idで、拒否の`authorization_denied`もobserverのものとして残る。
そのため`observe_finished`の件数と、observer自身のeventを数えない判定（[Observer](supervisor-lifecycle/observer.md)の0、`SqliteQueue::events_besides`）は、CLIで書いたときと同じに成り立つ（ADR-t1222-1決定4）。

### telemetry_report

executorがsessionのstepと、supervisorが外から測れない環境で中から測った資源のsampleを送るユースケース（ADR-t1662-3決定2・4・9・10、[計測](measurement.md)の「送る口」）。
送る側のportは`application::TelemetrySink`で、今のアダプタは`infrastructure::queue_service::ServiceTelemetrySink`（クライアントモードのsocketの宛てで送る）。
paramsの形は`domain::telemetry::TelemetryReport`（1つのsessionの、1つの対象のstepと資源のsample）。

- paramsの版: paramsは要求の`api_version`とは別に自分の`version`を持つ。
  serviceは他の欄より先に版を読み、`MIN_TELEMETRY_VERSION`〜`TELEMETRY_VERSION`の外（版の無いものを含む）は何も記録せず`bad_request`で断る。
  知らない欄は、上の段でもstepとsampleの中でも捨てて受ける。
  前の版のserviceが一部だけ読んではいけない変更は版を上げ、他の欄と並ぶ欄を足すだけなら上げない
- 1回の上限: executorは30秒ごとにまとめて送る（`REPORT_INTERVAL_SECS`）。
  上限はparamsのJSONとしての大きさ（`MAX_REPORT_BYTES`）で、要求の1行の上限から要求の残りの欄の分を引いたものなので、文字のescapeで膨らんでも上限の内の要求は1行に収まる。
  ほかにstepとsampleの数・idと名前の長さ・コマンドのshapeの大きさにも上限があり（値は`domain::telemetry`の定数）、超えたものは何も記録せず`bad_request`で断る。
  溜まったstep（serviceが答えなかった後など）は何回かに分けて送る
- 認可: `telemetry.report`を、paramsが名指す対象と、そのsessionを制御側が登録した対象（`Resource::Telemetry`）でprincipalに通す。
  持つのはexecutor-telemetryだけで、tokenの対象と違う対象（他のrun・他のplanner・他のjob）と、tokenの対象に登録されていないsession（未登録も）は`authorization_denied`（記録は他のユースケースと同じ）。
  sessionの持ち主は制御側が`SessionOwners::register_session`でDBに直接登録し（時点は[計測](measurement.md#送る口)）、serviceのユースケースからは登録できないので、別の対象のtokenが他の対象のsessionを先に取れない
  agentのtoken（worker・各job・observer・planner）では、自分のrunの対象でも、paramsを読む前に`authorization_denied`
- 書き込み: 受けたstepとsampleを受けた時刻とともに書き（`SessionStepStore::record_steps`、`SessionResourceStore::record_resources`の順）、答えはそれぞれの足した数と既に持っていた数。
  間で失敗すればstepだけが残り、送り直しは同じstepを足さない。
  成り立たないもの（seqが1より前・負の長さや値）と、別の対象で受けたことのあるsessionは`bad_request`で、何も記録しない
- 読み取りのユースケースの除外: `telemetry_report`は読み取りではなく、対応するコマンドも無い（`UseCase::of_command`は行き先にしない）。
  executor-telemetryのtokenは読み取り（`queue.read`）を含め他の全てのユースケースを、paramsを読む前に`authorization_denied`で拒まれる
- 終わったrunのtokenは断るので、runの対象の最後のstepはrunが終わる前に送る

## 読み取りのユースケース

queue全体の読み取り（ADR-t1233-5決定1〜3）。
1つの読み取りのコマンドに1つのユースケースで、paramsはそのコマンドのoption（名前はlong optionの名前、既定値はCLIの既定値）、答えはそのコマンドが出すJSONと同じ。
CLIとserviceは同じ`application::queue_reads::answer`で答える。
serviceは起動部分が注入する読み取り（`ServiceReads`）で呼ぶので、`infrastructure::queue_service`は起動部分を参照しない。
paramsは`application::queue_reads::QueueRead`が読み、CLIのparserが拒む値は`bad_request`。
認可はCLIがそのコマンドに求めるのと同じcapability（queue）を`Gate`でprincipalに通す。
executor-telemetryのほかの全role（workerとjobを含む）がqueue全体を読める（ADR-t1233-5決定3）。
`stats`・`kpi`・`forecast`・`status`・`candidates`の「今」はserviceが要求を受けた時刻。

ユースケースの一覧は`domain::queue_service::UseCase`、CLIと違うparamsは`application::queue_reads`の各optionの型のdoc commentが持つ。

paramsは書き出すファイルも実行するprogramも受け取らず、読み取りでserviceは別のprogramを起動しない（`graph --format svg`のd2も起動しない）。
`domain::queue_service::UseCase::of_command`が、dagqのコマンドの引数（サブコマンドから）から行き先のユースケースを決める。
クライアントモードは、引数の文字列ではなく読んだコマンドから行き先を決める（[クライアントモード](#クライアントモード)の`client_request`。どちらも同じ行き先になる）。

### 読み取りのroleとworkerが打つコマンド

promptとskillが打たせるdagqのコマンドは、それぞれのpromptとskillの本文が持つ。
jobのpromptに現れる`` `dagq …` ``が全てユースケースに行くことと、それぞれの読み取りがserviceでもCLIと同じJSONを返すことはtestが確かめる。

| 呼び出し元 | どこが言うか | 注 |
| --- | --- | --- |
| plan review job（`ReadFilesAndQueueCli`） | `prompt::plan_review_prompt`（[Plan review](supervisor-lifecycle/plan-review.md#入口)）、AGENTS.mdのplan reviewの節 | 読み取りだけ |
| goal review job（`ReadFilesAndQueueCli`） | `prompt::goal_review_prompt`（[Prompt](supervisor-lifecycle/prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)） | 読み取りだけ |
| observer（`QueueCli`） | `observer::observer_prompt` | 読み取りと、findingの書き込みとそのask |
| スループットの見直しのjob（`QueueCli`） | `throughput_review::review_prompt`と、それが載せるdagq skillの`reference/kpi.md`の手順 | 読み取りだけ |
| review job・復旧job（`ReadFiles`） | `prompt::review_prompt`・`prompt::recovery_prompt` | なし（Bashを持たず、dagqを打たない） |
| runtimeが立てるplanner | `RECORD_READING` | 読み取り（[hostのCLI](queue-service-host-cli.md)） |
| worker | runtimeのprompt（`WORKER_READING`） | `ask`（書き込み）。`list`・`show`は作業の初めに打たないよう言う（権限の範囲ではない。ADR-t1233-5決定3） |
| worker（`measure`のtask） | [taskの登録](../development/task-registration.md)の「change」の`measure`、measureのtaskのdescription | 読み取りだけ |
| worker・job | dagq skillの`SKILL.md`と`reference/` | 読み取りだけ |

ユースケースの無いコマンド（クライアントモードではserviceに届かない）:

- `watch`（`queue.watch`。hostのCLIは上の`watch_poll`）、`report`と`graph --out`（`queue.export`。ファイルを書く）: jobとworkerのpolicyに無い操作で、今のCLIでも拒まれる
- `graph --format svg`: hostのd2を起動するので、serviceは読み取りとして受け取らない（`json`と`d2`は答える）。
  promptとskillはsvgを名指さない
- `locate`（DBのpathを返す）・`doctor`（hostとDBのファイルの診断）・`service status`: 制御側の状態を見るもので、jobのpromptは名指さない。
  クライアントモードでの扱いは[クライアントモード](#クライアントモード)

## クライアントモード

ADR-t1233-1決定7、ADR-t1233-5決定1・2・5、ADR-t2114-3決定3。
`dagq`（`src/main.rs`の`execute`）は、環境が宛てを名指せば、`DAGQ_ROLE`もqueueの解決（cwdや`--db`）も読まずにクライアントモードで動く（`infrastructure::queue_service::Client`）。

- 宛ての種類（`ClientRoute`）: hostのworkerとjobはserviceのsocketへ、containerのrunはqueueのbroker（containerの中の転送、[Container runtime](container-runtime.md#brokerへの経路)）へ[Queue broker](queue-broker.md#入口)の要求の形で送る。
  envの意味は`domain::queue_service`の定数のdoc comment
- 両方があれば設定の誤りとして全てのコマンドを断る
- 宛ての環境が無いhostの`user`・`inbox`・`planner`の`dagq`も、serviceの答えるコマンドは解決したqueueのserviceへ送り（ADR-t3102-2決定5・6(2)、ADR-t3105-1決定3）、届かなければDBを開くことに戻らない。
  DBを自分で開くのはserviceを通らない残りのコマンドだけで、`check_access`の後に開く（分岐の順は[Roles](supervisor-lifecycle/roles.md#cliでの解釈)）
- コマンドを引数のとおりに読んで（clapの検査はそのまま）、ユースケースとparamsに写す（`client_request`）。
- tokenはcredentialのfileから読み、要求ごとに送る。
  答えの`result`をCLIと同じに出す
- 断り: serviceとbrokerの断りもクライアントの断りも同じ形のerror（`queue_service`の`code`）で終了コード1になり、codeの意味は`ClientFailure`のdoc comment。
  serviceの`authorization_denied`は、codeに加えてローカルのCLIの断りと同じ`denied`（role・capability・reason）とerrorの文（observerとjobには各々の決まった文）を出す。
  クライアントで断るもの（`no_use_case`・`queue_named`など）は`denied`を持たない
  どの断りでもDBを開くことに戻らない（fail closed）
- `doctor`・`service status`・`planners`は制御側（人かinboxがhostで打つ）のコマンドとして`control_side_command`で断り、errorの文がコマンド名とそれを言う。
  DBもserviceも触らない。
  `no_use_case`になるのはそのほかのserviceのユースケースでない全てのコマンド。
- `locate`はDBのpathを出さずにsocket（brokerの宛てではbroker）を答え、DBの有無を答えない。
  serviceへの到達は`hello`（statusの見る待ちと同じ短い待ち）で確かめて足し、brokerの宛てで読めるtokenが無ければbrokerに送らない（足す形は`reach_of`のdoc comment）。
  どちらでも`locate`は成功し、DBを開くことに戻らない。
  pluginの`--resolve`が最初に打ち、dagq skillはDBが無いと分かったときだけ`init`を言うので、workerとjobが`init`を試みない
- workerとjobのpromptとskillのコマンド（`dagq ask`・`dagq show`など）は変えない。
  observerとスループットの見直しのpromptは、`dagq --db <db>`ではなく`dagq`を名指す（promptはagentの引数なので、DBのpathを含めない）
- session wrapper（`session`・`planner-session`）は継いだ環境のactorでなくwrapperのtokenで送り、DBを開かず（containerのrunのturnの準備を除く）、届かなければそのerrorで終わる（`ServiceWrapperStore`）。
  plannerのhook（`session-event`）は報告をwrapperに渡し（`session::spool_hook`）、inboxのhookはinboxとして書く

### Codexのsandboxからの到達（ADR-t1233-5決定4）

- workerのturn（workspace-write、networkを開ける。ADR-t813-3決定4）: そのままsocketに届く
- 読み取りだけのjob（goal reviewなど）: `--sandbox read-only`の代わりに、`:read-only`を継いでnetworkをCodexのnetwork proxy経由で開き、serviceのsocket（実path）だけを許すpermission profileで動かす（`AgentProvider::reach_queue_service`、設定は`codex::job_service_config`）。
- Claude Codeのagentはsandboxを持たないので何も足さない
- Codexのworkerの`dagq ask`はserviceのユースケースとして送る（ADR-t1233-5決定5）

## 起動と停止（ADR-t1233-4決定1・2）

serviceは`dagq --db <db> service serve`で、固定バイナリ（`up`かsupervisorを動かしているもの）から起動する。
起動するときはactorを名指すenvを外し、`setsid`で起動元のsessionから離し、もう1度forkして起動元の子でなくし（execで入れ替わるsupervisorにzombieを残さない）、`service.log`に出力を足す。
起動したものは、このbuildのserviceが答えれば（競った別の起動元のものでも）成功とする。
`SIGINT`・`SIGTERM`で受け付けを止めて終わる。
queueのDBが消えれば（使い捨てのqueueの片付け）自分で終わる。
起動元のenvがownerのpidを名指していれば、そのprocessが居なくなってからも終わる（`domain::queue_service::OWNER_PID_ENV`）。
これを付けるのはtestだけで、本番の起動元（`up`・supervisor・`service start`）は付けないので、本番のserviceの寿命（supervisorのexecの引き継ぎやdrainの間も動き続けること）は変わらない。

- `up`: preflightの後、supervisorより先に`application::queue_service::ensure`を打つ。
  起動できなければ`up`はsupervisorを起動せずに止まる。
  結果は`up`の出力の`queue_service`で、起動か入れ替えはevent `queue_service_started`に残す
- supervisor: CLIのsupervisor（`--once`も）は登録とsign inの前に、居ないか別のbuild（`install`や自動更新の前のバイナリ）のserviceを起動か入れ替えし、最初の見張りで`queue_service_started`に残す（[`supervise`](supervisor-lifecycle/supervise.md#queue-serviceの経路)）。
  その後も一定の間隔で見て、同じように起動し直すか入れ替える。
  起動し直しの上限と、別のbuildの入れ替えとexecの前の起動の失敗の扱いは`application::supervise::host_ops::queue_service`のmodule doc。
  drainと引き継ぎの間は見ない（execはserviceを次のプロセスに残す）
- serviceが居ない間、supervisorは新しいclaimと、queueのjob（plan review・goal review・observer・スループットの見直し）の起動を控える。
  走っているrunとそのreview・復旧は止めない（ADR-t1233-4決定2）
- `down`: supervisorが居なければserviceを止めて`queue_service_stopped`に残す（`application::lifecycle::down`）。
  drainを待たない`down`は、signalの前に`queue_service_stop_requested`を書き、drainを終えた最後のsupervisorが止める（`queue_service_stopped`）
- `install`と`install --allow-breaking`: `install`のdrain（`down --wait`）の後の`up`が新しいバイナリで起動する。
  drainしない引き継ぎでは上のsupervisorが入れ替える
- 人の手: `dagq service start`（`service.lifecycle`。`ensure`と同じ）・`dagq service stop`（supervisorが動いていれば次の見張りで起動し直す）・`dagq service status`（`queue.read`）

## 落ちたときの知らせ（ADR-t3102-3決定1・2）

supervisorが起動し直せないか、上限に達したか、届かないときは、不在をqueueのディレクトリのDBの外に1回記録して`[push]`で1回送り、serviceが戻ったらserviceを通してattention `queue_service_down`に残し、続く`queue_service_started`か`queue_service_running`で閉じる（[`supervise`](supervisor-lifecycle/supervise.md#queue-serviceの経路)）。
不在の間はqueueにattentionが無い。
attentionの次の手は`dagq service status`。
人はそれと`service.log`を見て、`up`か`service start`を打ち直す。
生きているsupervisorも居ないときは`restart supervisor`が出る。

`status`と`watch`は届かないとき記録をDBを開かずに読んで添え、`watch`はroleごとに1回だけ返してserviceが戻るか新しい記録まで待つ（[hostのCLI](queue-service-host-cli.md#届かないとき)）。

## statusとdoctor

`status`・`doctor`・`dagq service status`はserviceの状態を出す（`application::queue_service::view`。欄の意味は`ServiceProbe`と`view`、`status`の節は`StatusSections`のdoc comment）。
見るだけで、serviceを起動も停止もしない（socketへの`hello`を1回打つ）。

## event

queueのevent（`EventKind::is_queue`）で、種類と欄の意味は`domain::queue_service`の`QUEUE_SERVICE_*`の定数のdoc comment。

## 権限

`service start`・`service stop`・`service serve`は`up`・`down`と同じ`service.lifecycle`（`Operation::QueueService`）、`service status`は`queue.read`。
workerとjobのClaudeの`permissions.deny`には`dagq service`の制御のコマンドが入る（`DAGQ_COMMANDS`）。

## まだ無いもの

- 実Codexでの読み取りだけのjobのsocketへの到達の確認（`codex sandbox`とstubのCodexまで。上の「Codexのsandboxからの到達」）
- containerの中からbrokerを通す`telemetry_report`（[Queue broker](queue-broker.md)の`TokenRefusal`）。
  containerのrunのstepはhostのwrapperが送る
- 段(4)〜(6)の残り: brokerの残りの宛て（[Queue broker](queue-broker.md#まだ無いもの)）、integrateのverificationの隔離
- 段(5)の残り: supervisorの着地とjobの結果・書き込みのCLIのservice経由化、親の依頼によるtokenの発行（決定はADR-t3102-1〜3。分け方は[ADR-t3102-1](../adr/2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)のContext）

## CIの見張り（ADR-t1920-1）

[ADR-t1920-1](../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)と[CI watch](supervisor-lifecycle/ci-watch.md)。
`finding_dismiss`の任意の引数`covered_by`（CLIは与えたときだけkeyを送る。意味はparamsのdoc comment）を知らない古いserviceは引数を読めず`bad_request`で拒む（「API versionと互換」）。
