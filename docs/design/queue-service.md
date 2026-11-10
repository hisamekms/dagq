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
決定の理由は[ADR-t1233-1](../adr/2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)（制御側と実行側・APIと認可・段）、[ADR-t1233-4](../adr/2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)（起動・停止・tokenの認証）、[ADR-t1233-5](../adr/2026-10-02-t1233-5-read-use-cases-read-scope-by-role-and-codex-sandbox-reach.md)（読み取りの範囲）。

ADR-t3102-2の境界: queue DBを開くのはserviceと[その例外](#dbを開く経路)だけで、serviceは下の[ユースケース](#ユースケース)（findingの書き込みはCodexのobserverの経路、[ADR-t1222-1](../adr/2026-10-02-t1222-1-codex-observer-writes-through-the-queue-service.md)）と、読み取りのroleのjobとworkerが打つqueue全体の読み取り（[読み取りのユースケース](#読み取りのユースケース)）を答える。
**worker（resumeを含む）・headlessのjob・observerのdagqはクライアントモードで動き、そのプロセスにはqueue DBのpathを渡さない**（[クライアントモード](#クライアントモード)）。
session wrapperとhookはwrapperのprincipalでserviceを通り（plannerとinboxのhookは例外、[クライアントモード](#クライアントモード)）、人・inbox・plannerのCLIは[hostのCLI](queue-service-host-cli.md)。
supervisorとそれが起動する`observe`・`throughput-review`もserviceを通る（[`supervise`](supervisor-lifecycle/supervise.md#queue-serviceの経路)）。
例外はexecutorのsessionのstepだけで、制御側が対象ごとに発行するtelemetry専用のprincipalで[`telemetry_report`](#telemetry_report)を通る（[ADR-t1662-3](../adr/2026-10-04-t1662-3-executors-send-telemetry-and-commands-are-classified-by-shape.md)決定13）。

名前: この文書の「broker」はqueueのbroker（[Queue broker](queue-broker.md)）のこと。

## 置き場所と形

queueのディレクトリ（`dagq locate`の`db`のあるディレクトリ）の`service/`に置く（ファイルとmodeは`infrastructure::queue_service`のmodule doc）。

ファイルの配置と排他、生存の判定は`infrastructure::queue_service`のmodule doc、socketの実pathの決め方は同moduleの`socket_path`。
止めるsignalはserviceが答えたpidか、同moduleの`serves`が確かめた記録のpidにだけ送る。
hostのtokenはworktreeとrun dirの外に置き、呼び出し元にはfileのpathを渡し、値はenvにもargvにも置かない（ADR-t1233-4決定4）。

tokenの記録`tokens/<値のsha256>.json`の欄と、人のtokenの期限は同moduleの`TokenRecord`のdoc comment。

要求のcallerは呼び手の自己申告であり、認可にも答えの内容にも使わない。

交換後のHTTPの受けはsocketと同じ確かめの順を通り、識別はbearerのtokenだけで、peerにも送り元のIPにも転送のheaderにも依らない。

接続と要求・応答の形と上限は`domain::queue_service::MAX_REQUEST_BYTES`のdoc comment。
上限を超えてもsupervisorのpassは止めず、育つ読み取りは区切る。
重い読み取りの同時実行の制限は`application::heavy_reads`が持ち、書き込みとsupervisorの周期の転送を妨げない。

transactionはserviceの中で完結し、クライアントとの往復で分けない（ADR-t1233-1決定3）。
書き込みの列への入口は`on_write_line`と`ServiceWriteLine`。
serviceの書き込みは1本の列で1つずつ確定し、読み取りは列を待たない。
書き込みはprincipalのactorで書く（eventの`actor`は呼び出し元のprincipal）。

## API versionと互換

互換はqueueのschemaではなくAPIのversionで判定する（ADR-t1233-1決定2）。
`API_VERSION`と`MIN_API_VERSION`を`src/domain/queue_service.rs`が持つ。

- serviceは要求の`api_version`が`MIN_API_VERSION`〜`API_VERSION`の外なら、tokenも読まずに`api_version_mismatch`で断る
- 答えは常に`api_version`と`min_api_version`を持ち、クライアントは自分の版がその範囲に入るか（`understands`）で、知らない版のserviceを使わない（判定は`answer_of`のdoc comment）
- 古い版の呼び出し元が読めない変更（欄の意味の変更・ユースケースの削除）で`API_VERSION`を上げ、新しい版の呼び出し元を断るまでの間は`MIN_API_VERSION`を据え置く

## principalとtoken

人の役割と有効性は呼び出しごとに登録簿から引き、tokenには役割を写さない。

入口はHTTPの`exchange_token`（`application::queue_service::exchange_human`）。
runnerの登録・交換・更新・失効の入口はHTTPの`application::queue_service::runner_http`。
通常の登録の経路では、runnerの資格は人が発行した1回きりの値から始まり、値はハッシュだけを持つ。
例外として、GitHub Actionsの控えのrunnerは登録の値の代わりに配置の設定が縛るOIDCのJWTで識別され、短いtokenだけを得て更新の資格を持たない。
申告は割り当てにだけ使い、権限は登録の印が決める。
外向きの入口だけが発行元のJWTを人の短いtokenに替え、hostのsocketの入口は人のtokenを発行も受けもしない。
人のtokenの記録は制御側の手元のファイルだけにあり、制御側の入れ替えで失われると期限の内でもunauthenticatedになる。

serviceは呼び出しのprincipal（`Principal`）をtokenから決め（tokenの無い人は[hostのCLI](queue-service-host-cli.md#principal)）、クライアントが名乗るrole（`DAGQ_ROLE`）は認可に使わない（ADR-t1233-1決定4、ADR-t1233-4決定4）。

tokenの発行・失効の入口と、actorの起動・終了との対応は`application::queue_service::ServiceAccess`のdoc comment。
親になれるroleと発行時の前提は[Authorization](authorization.md#queue-serviceのユースケースservice側)が持つ。
発行してservice dirに書くのはserviceだけで、AI actorにtokenを作るコマンドは無い。
発行のCOMMITが失敗したtokenは失効し、再発行の扱いは`domain::queue_service::reissue`が持つ。
終わったrunのtokenはfileが残っていてもserviceが断る（`domain::queue_service::run_holds_token`）。

executorのtelemetryの対象・発行・失効は`application::telemetry_tokens`のmodule docが持つ（ADR-t1662-3）。
制御側だけが発行し、agentのenv・argv・worktree・run dirには渡さない。
認証の拒否と記録は`QueueService::authenticate`、hostとcontainerへ渡す環境は`domain::queue_service::CLIENT_ENV`のdoc comment。
環境の名前の落とし穴は同moduleの`CREDENTIAL_FILE_ENV`を参照する。

hostではtokenは誤りを止めるもので、security boundaryではない（ADR-t1233-4決定5、[Security](security.md#host実行は助言的advisory)）

## ユースケース

runnerの割り当てと報告の入口は`application::queue_service::runner_work_http`。
runnerの仕事は制御側が選んで渡し、runnerはrunを指定できない。

検査の順と拒否の記録・応答は`application::queue_service::QueueService::handle`、storeの型付き拒否の応答は同moduleの`respond`が持つ。
認可はCLIと同じapplicationの境界をprincipalのactorでservice側で通す。

ユースケースの一覧とCLIからの対応は`domain::queue_service::UseCase::of_command`、paramsと答えは`application::queue_service`の各params型を入口に読む。
認可は[Authorization](authorization.md#queue-serviceのユースケースservice側)、hostのコマンドの範囲は[hostのCLI](queue-service-host-cli.md)を参照する。
imageのdigestの受け取りの入口はHTTPの`application::queue_service::image_publisher_exchange`と`image_publish`。
imageのdigestは、配置の設定で縛ったmainのCIの識別からだけ受ける。

observerのfindingに紐づく`blocked`のaskは`ask`のユースケース（capabilityは`finding.ask`）で送る。
findingの書き込みとそのaskはCLIと同じ`Dialogue`をprincipalのactorで通るので、eventの書き手は`observer`、eventのactorはjobのactor idで、拒否の`authorization_denied`もobserverのものとして残る。
そのため`observe_finished`の件数とobserver自身のeventを数えない判定は、CLIで書いたときと同じに成り立つ（[Observer](supervisor-lifecycle/observer.md)、ADR-t1222-1決定4）。

### telemetry_report

executorがsessionのstepと、supervisorが外から測れない環境で中から測った資源のsampleを送るユースケース（ADR-t1662-3決定2・4・9・10、[計測](measurement.md)の「送る口」）。
形・版・上限は`domain::telemetry`のmodule docと`TelemetryReport`、認可は`application::queue_service::run`、書き込みは同moduleの`ServiceQueue::record_report`が持つ。
受けるのはtokenの対象に制御側が登録したsessionだけで、agentのtokenでは自分の対象にも送れない。
終わったrunのtokenは断るので、最後のstepはrunが終わる前に送る。

## 読み取りのユースケース

queue全体の読み取り（ADR-t1233-5決定1〜3）。
CLIとserviceは同じ`application::queue_reads::answer`で答える。
paramsの解釈と受け付ける値は同moduleの`QueueRead`、注入する読み取りの境界は`infrastructure::queue_service::ServiceReads`が持つ。
認可はCLIがそのコマンドに求めるのと同じcapabilityをprincipalに通す。
executor-telemetry・runner・image-publisher以外の全role（workerとjobを含む）がqueue全体を読める（ADR-t1233-5決定3）。

ユースケースの一覧は`domain::queue_service::UseCase`、CLIと違うparamsは`application::queue_reads`の各optionの型のdoc commentが持つ。

paramsは書き出すファイルも実行するprogramも受け取らず、読み取りでserviceは別のprogramを起動しない（`graph --format svg`のd2も起動しない）。
`domain::queue_service::UseCase::of_command`が、dagqのコマンドの引数（サブコマンドから）から行き先のユースケースを決める。

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

コマンドにserviceの行き先があるかは`domain::queue_service::UseCase::of_command`、CLIから要求への写し方は`src/main.rs`の`client_request`が持つ。

## クライアントモード

ADR-t1233-1決定7、ADR-t1233-5決定1・2・5、ADR-t2114-3決定3。
`dagq`（`src/main.rs`の`execute`）は、環境が宛てを名指せば、`DAGQ_ROLE`もqueueの解決（cwdや`--db`）も読まずにクライアントモードで動く（`infrastructure::queue_service::Client`）。

`login`・`logout`・`whoami`はクライアントモードで実行できず、actorの判定より前にCLIが`control_side_command`で拒む。
この拒否では資格の置き場を開かず、発行元にもqueue serviceにも通信せず、拒否の記録も送らない。

宛てと転送は`infrastructure::queue_service::ClientRoute`、断りの形と意味は同moduleの`ClientFailure`、到達の答えは`reach_of`のdoc commentが持つ。
containerの経路は[Queue broker](queue-broker.md#入口)と[Container runtime](container-runtime.md#brokerへの経路)を参照する。
どの断りでもDBを開くことに戻らない（fail closed）。
宛ての無いhostのCLIの範囲は[hostのCLI](queue-service-host-cli.md#送る範囲)、roleとの分岐の順は[Roles](supervisor-lifecycle/roles.md#cliでの解釈)。

workerとjobのpromptとskillはDBのpathを含めず、`dagq`を名指す。
session wrapperとhookのprincipalの切り替えは`ServiceAccess`のdoc commentが持つ。
wrapperもDBを開かず、届かなければそのerrorで終わる。

### Codexのsandboxからの到達（ADR-t1233-5決定4）

- workerのturn（workspace-write、networkを開ける。ADR-t813-3決定4）: そのままsocketに届く
- 読み取りだけのjob（goal reviewなど）: `--sandbox read-only`の代わりに、`:read-only`を継いでnetworkをCodexのnetwork proxy経由で開き、serviceのsocket（実path）だけを許すpermission profileで動かす（`AgentProvider::reach_queue_service`、設定は`codex::job_service_config`）。
- Claude Codeのagentはsandboxを持たないので何も足さない
- Codexのworkerの`dagq ask`はserviceのユースケースとして送る（ADR-t1233-5決定5）

## 起動と停止（ADR-t1233-4決定1・2）

配置の設定の入口は`infrastructure::deployment_config`で、serviceの起動がHTTPと任意のTLSを開く。
配置の設定が欠ければ起動せず、OIDCの発行元が無ければHTTPの入口を開かず、配置の設定はdagq.tomlにもhost.tomlにもDBにも置かない。
チーム制御側はmainのdagq.tomlをrunner用Appの読み取り専用tokenで読む。

起動するprocessとbinaryは`infrastructure::queue_service::SystemQueueService`、終了条件は同moduleの`serve`が持つ。
入れ替えの口は`application::queue_service::ensure_replaced`、supervisorの見張りは`application::supervise::host_ops::queue_service`のmodule doc、停止とdrainの境界は`application::lifecycle::down`を入口に読む。
手で起動・停止する入口は`application::queue_service::start_by_hand`と同moduleの`stop_by_hand`。
ローカルではserviceの出力を`service.log`に足すが、配置の設定でstoreを選んだ制御側はlogのファイルを作らず標準出力と標準エラーに出し、集めて保つのは配置の側である（[計測のOpenTelemetryとの対応](measurement-otel.md)）。
更新の境界は[install](supervisor-lifecycle/install.md)を参照する。

serviceが居ない間、supervisorは新しいclaimとqueueのjobの起動を控える。
走っているrunとそのreview・復旧は止めない（ADR-t1233-4決定2）。
queueのbinary以外からはserviceを起動しない（ADR-t3102-2決定3）。

migrationを適用するのは入れ替えの口で起動したserviceの受け付けの前（ADR-t3102-2決定2。順は`serve`のdoc comment）と`migrate`のユースケースだけ。
引き継ぐ前の拒否は動いているserviceを残し、引き継いだ後の使用中の再検査か適用の失敗はserviceを残さず、見張りか`up`が起動し直す。

## 落ちたときの知らせ（ADR-t3102-3決定1・2）

supervisorが起動し直せないか、上限に達したか、届かないときは、不在をqueueのディレクトリのDBの外に1回記録して`[push]`で1回送り、serviceが戻ったらserviceを通してattention `queue_service_down`に残し、続く`queue_service_started`か`queue_service_running`で閉じる（[`supervise`](supervisor-lifecycle/supervise.md#queue-serviceの経路)）。
不在の間はqueueにattentionが無い。
attentionの次の手は`dagq service status`。
人はそれと`service.log`を見て、`up`か`service start`を打ち直す。
`service.log`はローカルの動きで、配置の設定でstoreを選んだ制御側は標準出力と標準エラーに出すので、配置の側が集めたlogを見る（[計測のOpenTelemetryとの対応](measurement-otel.md)）。
生きているsupervisorも居ないときは`restart supervisor`が出る。

`status`と`watch`は届かないとき記録をDBを開かずに読んで添える（返し方は[hostのCLI](queue-service-host-cli.md#届かないとき)）。

## statusとdoctor

`status`・`doctor`・`dagq service status`はserviceの状態を出す（`application::queue_service::view`。欄の意味は`ServiceProbe`と`view`、`status`の節は`StatusSections`のdoc comment）。
`doctor`はserviceが作り、PATHで探すものだけをクライアントが足す（`doctor_on_this_host`）。
見るだけで、serviceを起動も停止もしない（socketへ`hello`を打つ）。

## event

queueのevent（`EventKind::is_queue`）で、種類と欄の意味は`domain::queue_service`の`QUEUE_SERVICE_*`の定数のdoc comment。

## 権限

serviceの認可は[Authorization](authorization.md)、agentのコマンドの制限は`application::execution::DAGQ_COMMANDS`のdoc commentを入口に読む。

## DBを開く経路

DBを開くのはserviceのプロセスと、DBの無い所に作る`init`だけで、`context_boundaries::queue_opens`のtestがほかのopenを落とす（ADR-t3102-2決定1・8）。

## まだ無いもの

- 実Codexでの読み取りだけのjobのsocketへの到達の確認（`codex sandbox`とstubのCodexまで。上の「Codexのsandboxからの到達」）
- containerの中からbrokerを通す`telemetry_report`（[Queue broker](queue-broker.md)の`TokenRefusal`）。
  containerのrunのstepはhostのwrapperが送る

## CIの見張り（ADR-t1920-1）

[ADR-t1920-1](../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)と[CI watch](supervisor-lifecycle/ci-watch.md)。
`finding_dismiss`の任意の引数`covered_by`（CLIは与えたときだけkeyを送る。意味はparamsのdoc comment）を知らない古いserviceは引数を読めず`bad_request`で拒む（「API versionと互換」）。
