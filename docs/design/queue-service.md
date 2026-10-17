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
---

# Queue service

hostで動き、queue DBを開いてユースケース単位のAPIを、service側で認可して提供するプロセス。
決定の理由は[ADR-t1233-1](../adr/2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)（制御側と実行側の分け方・serviceとAPIと認可・unix socket・段）、[ADR-t1233-4](../adr/2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)（起動・停止の責任・落ちたときの知らせ方・tokenによるprincipalの認証）、[ADR-t1233-5](../adr/2026-10-02-t1233-5-read-use-cases-read-scope-by-role-and-codex-sandbox-reach.md)（読み取りの範囲）。

今の段（段(2)・(3)）: serviceは下の[ユースケース](#ユースケース)（findingの書き込みはCodexのobserverの経路、[ADR-t1222-1](../adr/2026-10-02-t1222-1-codex-observer-writes-through-the-queue-service.md)）と、読み取りのroleのjobとworkerが打つqueue全体の読み取り（[読み取りのユースケース](#読み取りのユースケース)）を答える。
**worker（resumeを含む）・headlessのjob・observerのdagqはクライアントモードで動き、そのプロセスにはqueue DBのpathを渡さない**（[クライアントモード](#クライアントモード)）。
supervisor・session wrapperとhook・inbox・planner・人のCLIはDBを直接開く（段(5)まで。ADR-t1233-4決定6）。

名前: この文書の「broker」はqueueのbroker（[Queue broker](queue-broker.md)）のこと。
brokerはrun tokenを検証した要求をそのtokenのままserviceのsocketへ転送するので、serviceの認証と認可は変わらない。
fs・process・git・packageを仲介するresource brokerは置かない（[ADR-t2113-1](../adr/2026-10-08-t2113-1-remove-the-resource-broker.md)）。

## 置き場所と形

queueのディレクトリ（`dagq locate`の`db`のあるディレクトリ）の`service/`に置く（ファイルとmodeは`infrastructure::queue_service`のmodule doc）。

| ファイル | 中身 |
| --- | --- |
| `queue.sock` | unix socket。pathが長すぎるqueueで`/tmp`の下に置くときは、そのdirがlinkでないことを確かめる。どの呼び出し元もqueueのディレクトリから同じpathを求める（`socket_path`） |
| `lock` | serviceが生きている間`flock`の排他で持ち、2つ目のserviceは取れずに止まる。見る側（status・supervisor）は共有で試し、取れれば「居ない」と判定する（見る側どうしは互いを妨げない）。止めるときにsignalを送るのは、`hello`が答えたpidか、記録のpidが今このqueueの`service serve`を動かしている（`ps`で確かめる）ときだけ |
| `state.json` | 動いているserviceの記録。止まるときに消す |
| `service.log` | `up`・supervisor・`service start`が起動したserviceのstdoutとstderr |
| `tokens/<値のsha256>.json` | tokenのprincipalと発行の時刻 |
| `credentials/<actor idのsha256>` | tokenの値。呼び出し元にはこのfileのpathを渡し、値をenvにもargvにも置かない（ADR-t1233-4決定4）。worktreeとrun dirの外 |

1つの接続に1つの要求で、要求は1行のJSON、答えも1行のJSONで、答えた後に閉じる。
transactionはserviceの中で完結し、クライアントとの往復で分けない（ADR-t1233-1決定3）。
要求ごとにDBへの接続を開き、principalのactorで書く（eventの`actor`は呼び出し元のprincipal）。
要求と答えの形は`domain::queue_service`の`ServiceRequest`と`ServiceResponse`が持つ。

## API versionと互換

互換はqueueのschemaではなくAPIのversionで判定する（ADR-t1233-1決定2）。
`API_VERSION`と`MIN_API_VERSION`を`src/domain/queue_service.rs`が持つ。

- serviceは要求の`api_version`が`MIN_API_VERSION`〜`API_VERSION`の外なら、tokenも読まずに`api_version_mismatch`で断る
- 答えは常に`api_version`と`min_api_version`を持ち、クライアントは自分の版がその範囲に入るか（`understands`）で、知らない版のserviceを使わない
- 古い版の呼び出し元が読めない変更（欄の意味の変更・ユースケースの削除）で`API_VERSION`を上げ、新しい版の呼び出し元を断るまでの間は`MIN_API_VERSION`を据え置く

## principalとtoken

serviceは呼び出しのprincipal（`Principal`）をtokenから決め、クライアントが名乗るrole（`DAGQ_ROLE`）は認可に使わない（ADR-t1233-1決定4、ADR-t1233-4決定4）。

- 発行: 制御側だけが`queue_service::issue`で発行する。
  AI actorにtokenを作るコマンドは無い。
  発行できるprincipalはAI actorだけで、人と制御側（user・supervisor・wrapper・integrator）は段(5)まで今のままDBを直接開く。
  同じactor idに発行し直すと前のtokenは失効する（resumeの発行し直し）。
  発行するのはactorの起動の1か所（applicationのport `ServiceAccess`）:
  - workerのtoken: supervisorがclaimとresumeでrunのsession wrapperをbackgroundで起動するときに発行する。
    wrapperの環境には入れず、session wrapperがagent（非対話のturn。最初のsessionもresumeもturnで動き、対話のsessionは起動しない）を起動するときにそのfileのpathを渡す。
    fileが無いとき（claimの発行を経ずに起動したwrapper）だけwrapperが発行する（wrapperも制御側）
  - jobのtoken: jobを起動するプロセス（supervisor、`observe`・`throughput-review`・`setup-review`のコマンド）が起動のときに発行し、jobのプロセスが終わったとき（それを見たwaitか、handleを捨てたとき）に失効させる（`ServiceAccess::revoke_on_exit`）
- 失効: `queue_service::revoke`（actor id）が値とprincipalの記録を消す。
  加えてserviceは、principalがrunを名指すtokenを、そのrunが終わったときに断る（runの終わりで失効。`run_holds_token`）。
  workerのtokenのfileはresumeの発行し直しまで残るが、使えない
- 断り: 認証できない要求は`unauthenticated`で断り、queueのevent `queue_service_unauthenticated`（tokenの値は書かない）に残す（理由は`QueueService::authenticate`のdoc comment）。
- hostの呼び出し元に渡すenvはsocketのpathとtokenのfileのpathで、containerのrunにはsocketの代わりにbrokerのURLを渡す（[クライアントモード](#クライアントモード)、`domain::queue_service::CLIENT_ENV`）。
  envの名前の落とし穴は`CREDENTIAL_FILE_ENV`のdoc comment
- host構成では同じユーザーのプロセスが他のrunのtokenのfileもDBも読めるので、tokenは誤りを止めて記録を正しくするためのもので、security boundaryではない（ADR-t1233-4決定5、[Security](security.md#host実行は助言的advisory)）

## ユースケース

`src/application/queue_service.rs`の`QueueService::handle`が、versionの検査、tokenの検査、ユースケースの順に行う。
認可はCLIと同じapplicationの境界（`Dialogue`・`Gate`と`StaticPolicy`）を、principalのactorでservice側で通す。
拒否はCLIと同じ`authorization_denied`（拒まれたprincipalがactor）に記録し、`authorization_denied`のcodeで返す。
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
| 読み取り | そのコマンド | `queue.read`（queue）。[読み取りのユースケース](#読み取りのユースケース) |

observerのfindingに紐づく`blocked`のaskは`ask`のユースケース（capabilityは`finding.ask`）で送る。
findingの書き込みとそのaskはCLIと同じ`Dialogue`をprincipalのactorで通るので、eventの書き手は`observer`、eventのactorはjobのactor idで、拒否の`authorization_denied`もobserverのものとして残る。
そのため`observe_finished`の件数と、observer自身のeventを数えない判定（[Observer](supervisor-lifecycle/observer.md)の0、`SqliteQueue::events_besides`）は、CLIで書いたときと同じに成り立つ（ADR-t1222-1決定4）。

`ask`は誰にも知らせず、inboxの`watch`が人に知らせる（[人への通知](supervisor-lifecycle/ask-push.md)）。

## 読み取りのユースケース

queue全体の読み取り（ADR-t1233-5決定1〜3）。
1つの読み取りのコマンドに1つのユースケースで、paramsはそのコマンドのoption（名前はlong optionの名前、既定値はCLIの既定値）、答えはそのコマンドが出すJSONと同じ。
CLIとserviceは同じ`application::queue_reads::answer`で答える。
serviceは起動部分が注入する読み取り（`ServiceReads`）で呼ぶので、`infrastructure::queue_service`は起動部分を参照しない。
paramsは`application::queue_reads::QueueRead`が読み、CLIのparserが拒む値は`bad_request`。
認可はCLIがそのコマンドに求めるのと同じ`queue.read`（queue）を`Gate`でprincipalに通す。
全role（workerとjobを含む）がqueue全体を読める（ADR-t1233-5決定3）。
`stats`・`kpi`・`forecast`・`status`・`candidates`の「今」はserviceが要求を受けた時刻。

ユースケースの一覧は`domain::queue_service::UseCase`、CLIと違うparamsは`application::queue_reads`の各optionの型のdoc commentが持つ。

paramsは書き出すファイルも実行するprogramも受け取らず、読み取りでserviceは別のprogramを起動しない（cmuxも`graph --format svg`のd2も起動しない）。
`domain::queue_service::UseCase::of_command`が、dagqのコマンドの引数（サブコマンドから）から行き先のユースケースを決める。
クライアントモードは、引数の文字列ではなく読んだコマンドから行き先を決める（[クライアントモード](#クライアントモード)の`client_request`。どちらも同じ行き先になる）。

### 読み取りのroleとworkerが打つコマンド

promptとskillが打たせるdagqのコマンドは、それぞれのpromptとskillの本文が持つ。
jobのpromptに現れる`` `dagq …` ``が全てユースケースに行くことと、それぞれの読み取りがserviceでもCLIと同じJSONを返すことはtestが確かめる。

| 呼び出し元 | どこが言うか | 注 |
| --- | --- | --- |
| plan review job（`ReadFilesAndQueueCli`） | `prompt::plan_review_prompt`（[Plan review](supervisor-lifecycle/plan-review.md)の4）、AGENTS.mdのplan reviewの節 | 読み取りだけ |
| goal review job（`ReadFilesAndQueueCli`） | `prompt::goal_review_prompt`（[Prompt](supervisor-lifecycle/prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)） | 読み取りだけ |
| observer（`QueueCli`） | `observer::observer_prompt` | 読み取りと、findingの書き込みとそのask |
| スループットの見直しのjob（`QueueCli`） | `throughput_review::review_prompt`と、それが載せるdagq skillの`reference/kpi.md`の手順 | 読み取りだけ |
| review job・復旧job（`ReadFiles`） | `prompt::review_prompt`・`prompt::recovery_prompt` | なし（Bashを持たず、dagqを打たない） |
| runtimeが立てるplanner | `RECORD_READING` | 読み取り（plannerは今もDBを直接開く） |
| worker | runtimeのprompt（`WORKER_READING`） | `ask`（書き込み）。`list`・`show`は作業の初めに打たないよう言う（権限の範囲ではない。ADR-t1233-5決定3） |
| worker（`measure`のtask） | [taskの登録](../development/task-registration.md)の「change」の`measure`、measureのtaskのdescription | 読み取りだけ |
| worker・job | dagq skillの`SKILL.md`と`reference/` | 読み取りだけ |

ユースケースの無いコマンド（クライアントモードではserviceに届かない）:

- `watch`（`queue.watch`。inboxのもの）、`report`と`graph --out`（`queue.export`。ファイルを書く）: jobとworkerのpolicyに無い操作で、今のCLIでも拒まれる
- `graph --format svg`: hostのd2を起動するので、serviceは読み取りとして受け取らない（`json`と`d2`は答える）。
  promptとskillはsvgを名指さない
- `locate`（DBのpathを返す）・`doctor`（hostとDBのファイルの診断）・`service status`・`planners`: 制御側の状態を見るもので、jobのpromptは名指さない。
  dagq skillは`doctor`を名指すが、AGENTS.mdはworkerの実queueでの`doctor`の確認を人かinboxに任せる。
  クライアントモードでは`locate`がDBのpathを出さずにsocketを答え、ほかは`no_use_case`で断る（[クライアントモード](#クライアントモード)）

## クライアントモード

ADR-t1233-1決定7、ADR-t1233-5決定1・2・5、ADR-t2114-3決定3。
`dagq`（`src/main.rs`の`execute`）は、環境が宛てを名指せば、`DAGQ_ROLE`もqueueの解決（cwdや`--db`）も読まずにクライアントモードで動く（`infrastructure::queue_service::Client`）。

- 宛ての種類（`ClientRoute`）: hostのworkerとjobはserviceのsocketへ、containerのrunはqueueのbroker（containerの中の転送、[Container runtime](container-runtime.md#brokerへの経路)）へ[Queue broker](queue-broker.md#入口)の要求の形で送る。
  envの意味は`domain::queue_service`の定数のdoc comment
- 両方があれば設定の誤りとして全てのコマンドを断り、どちらも無ければqueueを自分で開く
- コマンドを引数のとおりに読んで（clapの検査はそのまま）、ユースケースとparamsに写す（`client_request`）。
  読み取りはCLIの読み取りと同じ`QueueRead`をparamsにし、serviceがそれを同じ読み取りに読み戻す。
- tokenはcredentialのfileから読み、要求ごとに送る。
  答えの`result`をCLIと同じに出す（`graph --format d2`の本文もそのまま）。
  答えを待つ時間と読む長さの上限はどちらの宛ても同じ
- 断り: serviceとbrokerの断りもクライアントの断りも同じ形のerrorで終了コード1になり、codeの意味は`ClientFailure`のdoc comment。
  どの断りでもDBを開くことに戻らない（fail closed）
- `no_use_case`になるのはserviceのユースケースでない全てのコマンド。
  `locate`はserviceに送らずに、DBのpathを出さずにsocket（brokerの宛てではbroker）を答え、DBの有無を答えない。
  pluginの`--resolve`が最初に打ち、dagq skillはDBが無いと分かったときだけ`init`を言うので、workerとjobが`init`を試みない
- workerとjobのpromptとskillのコマンド（`dagq ask`・`dagq show`など）は変えない。
  observerとスループットの見直しのpromptは、`dagq --db <db>`ではなく`dagq`を名指す（promptはagentの引数なので、DBのpathを含めない）
- session wrapper（`session`）とhook（`session-event`）は制御側で、`--db`でDBを開く。
  runのsession wrapperの環境にはserviceの変数を入れないので、wrapperはクライアントモードにならない（agentを起動するときに足す）

### Codexのsandboxからの到達（ADR-t1233-5決定4）

- workerのturn（workspace-write、networkを開ける。ADR-t813-3決定4）: そのままsocketに届く
- 読み取りだけのjob（goal reviewなど）: `--sandbox read-only`の代わりに、`:read-only`を継いでnetworkをCodexのnetwork proxy経由で開き、serviceのsocket（実path）だけを許すpermission profileで動かす（`AgentProvider::reach_queue_service`、設定は`codex::job_service_config`）。
  実Codexでのjobの確認は手動スモークに任せる
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
- supervisor: `up`が起動したsupervisor（`--once`でないもの）は一定の間隔でserviceを見て、居ない・答えないものは起動し直し、別のbuildのもの（`install`や自動更新の引き継ぎの前のバイナリが残したもの）は入れ替える（`queue_service_started`）。
  起動し直しは窓の中で回数に上限がある（`application::supervise::queue_service`の定数）。
  別のbuildの入れ替えは数えない。
  制限: 起動の成功は自分のbuildの答えだけなので（`SystemQueueService::start`）、`install`が新しいバイナリを置いた後でexecの前のsupervisorの起動は失敗して`queue_service_down`を書き、次の見張りでまた入れ替える。
  drainと引き継ぎの間は見ない（execはserviceを次のプロセスに残し、次のプロセスがbuildの違いで入れ替える）
- serviceが居ない間、supervisorは新しいclaimと、queueのjob（plan review・goal review・observer・スループットの見直し）の起動を控える。
  走っているrunとそのreview・復旧は止めない（ADR-t1233-4決定2）
- `down`: supervisorが居なければserviceを止めて`queue_service_stopped`に残す（`application::lifecycle::down`）。
  drainを待たない`down`は、signalの前に`queue_service_stop_requested`を書き、drainを終えた最後のsupervisorが止める（`queue_service_stopped`）
- `install`と`install --allow-breaking`: `install`のdrain（`down --wait`）の後の`up`が新しいバイナリで起動する。
  drainしない引き継ぎでは、新しいバイナリのsupervisorの最初の見張りが入れ替える
- 人の手: `dagq service start`（`service.lifecycle`。`ensure`と同じ）・`dagq service stop`（supervisorが動いていれば次の見張りで起動し直す）・`dagq service status`（`queue.read`）

## 落ちたときの知らせ（ADR-t1233-4決定3）

supervisorが起動し直せなかったか、上限に達したときは、queueのattention `queue_service_down`をDBに直接書く（serviceを通らない）。
inboxの`watch`と`status`はDBを直接読むので、serviceが落ちても届く。
1つの失敗につき1回で、serviceがまた動けば（`queue_service_started`・`queue_service_running`）消える。
attentionの次の手は`dagq service status`。
人は`dagq service status`と`service.log`を見て、`up`か`service start`を打ち直す。
supervisorも居ないときは`restart supervisor`が出る。

クライアントモードのdagqは、serviceに届かないときに`unreachable`のerrorを返し、DBを直接開くことへ戻らない（ADR-t1233-1決定7、[クライアントモード](#クライアントモード)）。

## statusとdoctor

`status`・`doctor`・`dagq service status`はserviceの状態を出す（`application::queue_service::view`。欄の意味は`ServiceProbe`と`view`、`status`の節は`StatusSections`のdoc comment）。
見るだけで、serviceを起動も停止もしない（socketへの`hello`を1回打つ）。

## event

queueのevent（`EventKind::is_queue`）で、種類と欄の意味は`domain::queue_service`の`QUEUE_SERVICE_*`の定数のdoc comment。

## 権限

`service start`・`service stop`・`service serve`は`up`・`down`と同じ`service.lifecycle`（`Operation::QueueService`）、`service status`は`queue.read`。
workerとjobのClaudeの`permissions.deny`には`dagq service`の制御のコマンドが入る（`DAGQ_COMMANDS`）。

## まだ無いもの

- runの終わりでのworkerのtokenのfileの片付け（serviceはrunの終わったtokenを断るので使えないが、fileは次のresumeの発行し直しか手の片付けまで残る）
- 実Codexでの読み取りだけのjobのsocketへの到達の確認（`codex sandbox`とstubのCodexまで。上の「Codexのsandboxからの到達」）
- `dagq ci failures`（`ci.read`。[CI watch](supervisor-lifecycle/ci-watch.md)）の読み取りのユースケース。
  clientのmodeでは`no_use_case`で断る
- 段(4)〜(6)の残り: brokerの残りの宛て（[Queue broker](queue-broker.md#まだ無いもの)）、supervisor・wrapper・hook・CLIのservice経由化、integrateのverificationの隔離

## CIの見張り（ADR-t1920-1）

[ADR-t1920-1](../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)と[CI watch](supervisor-lifecycle/ci-watch.md)。
`finding_dismiss`の任意の引数`covered_by`（CLIは与えたときだけkeyを送る。意味はparamsのdoc comment）を知らない古いserviceは引数を読めず`bad_request`で拒む（「API versionと互換」）。
