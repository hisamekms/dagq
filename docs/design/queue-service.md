---
id: design-queue-service
type: design
title: Queue service
status: current
created: 2026-10-02
updated: 2026-10-02
last_verified: 2026-10-02
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
  - design-supervisor-lifecycle-status
  - design-supervisor-lifecycle-doctor
---

# Queue service

hostで動き、queue DBを開いてユースケース単位のAPIを、service側で認可して提供するプロセス。決定の理由は[ADR-t1233-1](../adr/2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)（制御側と実行側の分け方・serviceとAPIと認可・unix socket・段）、[ADR-t1233-4](../adr/2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)（起動・停止の責任・落ちたときの知らせ方・tokenによるprincipalの認証）、[ADR-t1233-5](../adr/2026-10-02-t1233-5-read-use-cases-read-scope-by-role-and-codex-sandbox-reach.md)（読み取りの範囲）。

今の段（goal 82の段(2)、task 1234・1235・1242）: serviceがあり、`hello`・`ask`・`show`・`note`（task 1234）と、`proposal_list`・`proposal_show`・`finding_record`・`finding_resolve`・`finding_dismiss`（task 1235。goal 80のCodexのobserverの書き込みの経路、[ADR-t1222-1](../adr/2026-10-02-t1222-1-codex-observer-writes-through-the-queue-service.md)）と、読み取りのroleのjobとworkerが打つqueue全体の読み取り（task 1242。[読み取りのユースケース](#読み取りのユースケース)）のユースケースを答える。**supervisor・inbox・planner・人のCLI・workerとjobのdagqは、今までどおりDBを直接開く。** workerとjobのdagqをクライアントモードにし、DBのpathを渡さないのは段(3)で、tokenの発行と受け渡し（claim・resume・jobの起動）もそこで足す。

名前: この文書の「broker」はqueueのbroker（段(4)）のこと。fs・process・gitを仲介するresource broker（[Resource broker](broker.md)）とは別。

## 置き場所と形

queueのディレクトリ（`dagq locate`の`db`のあるディレクトリ）の`service/`（mode 0700）に置く（`src/infrastructure/queue_service.rs`）。

| ファイル | 中身 |
| --- | --- |
| `queue.sock` | unix socket（mode 0600）。このpathがmacOSの`sun_path`の上限（103 bytes）を超えるqueue（長い一時ディレクトリの下のもの）では、代わりに`/tmp/dagq-<uid>/<queue dirのsha256の先頭16桁>.sock`（そのdirはこのユーザーのもので mode 0700、linkでないことを確かめる）に置く。どの呼び出し元もqueueのディレクトリから同じpathを求める（`socket_path`） |
| `lock` | serviceが生きている間`flock`の排他（`LOCK_EX`）で持つ。2つ目のserviceは取れずに`a queue service already runs for this queue`で止まる。見る側（status・supervisor）は共有（`LOCK_SH`）で試し、取れれば「居ない」と判定する（見る側どうしは互いを妨げない）。止めるときにsignalを送るのは、`hello`が答えたpidか、記録のpidが今このqueueの`service serve`を動かしている（`ps`で確かめる）ときだけ |
| `state.json` | 動いているserviceの記録（`pid`・`build`・`api_version`・`socket`・`started_at`）。止まるときに消す |
| `service.log` | `up`・supervisor・`service start`が起動したserviceのstdoutとstderr |
| `tokens/<値のsha256>.json` | tokenのprincipalと発行の時刻 |
| `credentials/<actor idのsha256>` | tokenの値（mode 0600）。呼び出し元にはこのfileのpathを渡し、値をenvにもargvにも置かない（ADR-t1233-4決定4）。worktreeとrun dirの外 |

1つの接続に1つの要求で、要求は1行のJSON、答えも1行のJSONで、答えた後に閉じる（`MAX_REQUEST_BYTES`は1 MiB）。transactionはserviceの中で完結し、クライアントとの往復で分けない（ADR-t1233-1決定3）。要求ごとにDBへの接続を開き、principalのactorで書く（eventの`actor`は呼び出し元のprincipal）。

```json
{"api_version": 1, "token": "<64 hex>", "use_case": "ask", "params": {...}}
{"api_version": 1, "min_api_version": 1, "ok": true, "result": {...}}
{"api_version": 1, "min_api_version": 1, "ok": false, "error": {"code": "authorization_denied", "message": "..."}}
```

## API versionと互換

互換はqueueのschemaではなくAPIのversionで判定する（ADR-t1233-1決定2）。`API_VERSION`（今は1）と`MIN_API_VERSION`（今は1）を`src/domain/queue_service.rs`が持つ。

- serviceは要求の`api_version`が`MIN_API_VERSION`〜`API_VERSION`の外なら、tokenも読まずに`api_version_mismatch`で断る
- 答えは常に`api_version`と`min_api_version`を持ち、クライアントは自分の版がその範囲に入るか（`understands`）で、知らない版のserviceを使わない
- 古い版の呼び出し元が読めない変更（欄の意味の変更・ユースケースの削除）で`API_VERSION`を上げ、新しい版の呼び出し元を断るまでの間は`MIN_API_VERSION`を据え置く

## principalとtoken

serviceは呼び出しのprincipal（`role`・`actor_id`・workerなら`run_id`と`task_id`）をtokenから決め、クライアントが名乗るrole（`DAGQ_ROLE`）は認可に使わない（ADR-t1233-1決定4、ADR-t1233-4決定4）。

- 発行: 制御側（supervisor）だけが`queue_service::issue`で発行する。AI actorにtokenを作るコマンドは無い。tokenは32 bytesの乱数のhex。発行できるprincipalはAI actor（`TrustLevel::UntrustedAgent`）だけで、人と制御側（user・supervisor・wrapper・integrator）は段(5)まで今のままDBを直接開く。同じactor idに発行し直すと前のtokenは失効する（resumeの発行し直し）
- 失効: `queue_service::revoke`（actor id）が値とprincipalの記録を消す。加えてserviceは、principalがrunを名指すtokenを、そのrunの状態が`integrated`・`succeeded`・`failed`・`interrupted`のとき、またはqueueに無いときに断る（runの終わりで失効。`run_holds_token`）
- 断り: tokenが無い（`missing_token`）・発行していない値か失効した（`unknown_token`）・runが終わった（`run_ended`）要求は`unauthenticated`で断り、queueのevent `queue_service_unauthenticated`（`use_case`・`reason`。tokenの値は書かない）に残す。actorはservice自身（role `supervisor`、id `queue-service:<pid>`。制御側の一部）
- 段(3)で呼び出し元に渡すenvの名前は`DAGQ_SERVICE_SOCKET`（socketのpath）と`DAGQ_SERVICE_TOKEN_FILE`（tokenのfileのpath）。今はどちらも誰も設定しない
- host構成では同じユーザーのプロセスが他のrunのtokenのfileもDBも読めるので、tokenは誤りを止めて記録を正しくするためのもので、security boundaryではない（ADR-t1233-4決定5、[Security](security.md#host実行は助言的advisory)）

## ユースケース

`src/application/queue_service.rs`の`QueueService::handle`が、versionの検査、tokenの検査、ユースケースの順に行う。認可はCLIと同じapplicationの境界（`Dialogue`・`Gate`と`StaticPolicy`）を、principalのactorでservice側で通す。拒否はCLIと同じ`authorization_denied`（拒まれたprincipalがactor）に記録し、`authorization_denied`のcodeで返す。paramsが読めなければ`bad_request`、ユースケースの失敗（無いtask、読めないoptionなど）は`failed`。

| use_case | params | 答え | capabilityとresource |
| --- | --- | --- | --- |
| `hello` | なし | `service`・`build`・`pid`・`api_version`・`min_api_version` | tokenを要らない（生存と版の確認だけ） |
| `ask` | `dagq ask`と同じ: `kind`・`question`・`options`・`because`・`topics`・`task_id`・`run_id`・`finding_id` | `dagq ask`の出力 | `ask.open`（new ask）、`blocked`の`finding_id`つきは`finding.ask`。workerは自分のrunとtaskだけ、`worker_question`だけ。`asked_by`はprincipalのもの |
| `show` | `id`・`full`（既定false）・`events`（既定5） | `dagq show`（`--full`）の出力 | `queue.read`（task）。goal 82では全roleがqueue全体を読める（ADR-t1233-5決定3） |
| `note` | `task`・`run`・`goal`のどれか1つと`text`・`kind` | `dagq note`の出力 | `note.write`。workerは自分のtaskとrunだけ。`by`はprincipalのもの |
| `proposal_list` | `all`（既定false） | `dagq proposal list`（`--all`）の出力（`{"proposals": [...]}`） | `queue.read`（queue）。全role |
| `proposal_show` | `id` | `dagq proposal show`の出力 | `queue.read`（queue）。全role。無いproposalは`failed` |
| `finding_record` | `dagq finding record`と同じ: `kind`・対象を1つ（`task`・`run`・`goal`か`queue: true`）・`subject`（既定は空）・`summary`・`detail`・`impact`・`evidence`（eventのidの配列）・`propose` | `dagq finding record`の出力（findingと`created`・`changed`） | `finding.record`（対象）。tokenを発行するAI actorのうち今のpolicyで持つのはobserverとinboxで、plannerとworkerとjobには無い。同じ種類・対象・subjectのopenかproposedのfindingへの合流（新しい根拠で回数と根拠を足し、新しいものが無ければ何も書かない）と根拠の検査は、CLIと同じ`record_finding`の1つのtransaction。`by`はprincipalのもの |
| `finding_resolve` | `id`・`reason` | `dagq finding resolve`の出力 | `finding.resolve`。今のpolicyで持つのはobserver・inbox・plannerで、workerとjobには無い。`by`はprincipalのもの |
| `finding_dismiss` | `id`・`reason` | `dagq finding dismiss`の出力 | `finding.dismiss`。observerには無い（ADR-t1222-1決定2）。今のpolicyでこれを持つのはinboxとplannerで、workerとjobには無い |
| 読み取り（`list`・`events`・`timeline`・`stats`・`kpi`・`forecast`・`marks`・`search`・`related`・`findings`・`goal_show`など） | そのコマンドのoption | そのコマンドの出力 | `queue.read`（queue）。全role。[読み取りのユースケース](#読み取りのユースケース) |

observerのfindingに紐づく`blocked`のaskは`ask`のユースケース（`kind: blocked`と`finding_id`。capabilityは`finding.ask`）で送る。findingの書き込みとそのaskはCLIと同じ`Dialogue`をprincipalのactorで通るので、`finding_recorded` / `finding_updated` / `finding_status_changed`の`by`と`ask_opened`の`asked_by`は`observer`、eventのactorはjobのactor idで、拒否の`authorization_denied`もobserverのものとして残る。そのため`observe_finished`の件数と、observer自身のeventを数えない判定（[Observer](supervisor-lifecycle/observer.md)の0、`SqliteQueue::events_besides`）は、CLIで書いたときと同じに成り立つ（ADR-t1222-1決定4）。

`ask`はCLIと同じく、新しいaskをinboxにcmuxで知らせる（serviceの`--cmux`。見つからなくても知らせが失敗するだけでaskは開く）。

## 読み取りのユースケース

queue全体の読み取り（ADR-t1233-5決定1〜3、task 1242）。1つの読み取りのコマンドに1つのユースケースで、paramsはそのコマンドのoption（名前はlong optionの名前、既定値はCLIの既定値）、答えはそのコマンドが出すJSONと同じ。CLIとserviceは同じ`compose::read_queue`（`src/compose.rs`）で答え、paramsは`application::queue_reads::QueueRead`が読む（CLIのparserが拒む値、例えば`limit: 0`・知らない`format`・`role`・`kind`・`change`・`area`・`status`、読めないrun id、2つの対象は`bad_request`）。認可はCLIがそのコマンドに求めるのと同じ`queue.read`（queue）を`Gate`でprincipalに通す。goal 82では全role（workerとjobを含む）がqueue全体を読める（ADR-t1233-5決定3）。`stats`・`kpi`・`forecast`・`status`の「今」はserviceが要求を受けた時刻で、CLIと同じ。

| use_case | CLI | params（既定） |
| --- | --- | --- |
| `list` | `list` | `status`（配列）・`all`・`goal`・`limit`（20）・`before`・`full` |
| `candidates` | `candidates` | なし |
| `graph` | `graph`（`--out`なし） | `goal`・`format`（`json`か`d2`。`d2`は`{"__dagq_raw_stdout": <本文>}`で、CLIはこれをそのまま出す。`svg`はhostのd2を起動するので受け取らない） |
| `status` | `status` | `role`（`inbox`・`planner`） |
| `asks` | `asks` | `open`・`role`・`all` |
| `events` | `events` | `after`（0）・`limit`（100）・`all`・`full`・`run`・`task`・`goal`・`kind`（配列）・`since`・`until` |
| `timeline` | `timeline RUN` | `run`・`gap`（`DEFAULT_GAP_SECS`）・`full` |
| `stats` | `stats`（`--cmux`なし） | `since`・`until`（cursorの文字列かevent id）・`goal`・`full`。`workspace_mismatch`はserviceの`--cmux`でworkspaceを見る |
| `kpi` | `kpi` | `period`（`day`）・`last`（7）・`at`・`since`・`until`・`change`・`area`・`by`（配列）・`cross`・`compare`・`window`（`DEFAULT_WINDOW_DAYS`）・`goal` |
| `forecast` | `forecast` | `task`・`goal`・`parallel`・`trials`（`DEFAULT_TRIALS`） |
| `notes` | `notes` | `goal`・`task`・`since`・`limit`（20） |
| `marks` | `marks` | `since`・`until` |
| `findings` | `findings` | `id`・`all`・`status`・`kind`（配列）・対象を高々1つ（`task`・`run`・`goal`か`queue: true`）・`full` |
| `search` | `search QUERY` | `query`・`status`・`kind`・`goal`・`limit`（20）・`full` |
| `related` | `related TASK` | `task`・`status`・`limit`（10） |
| `goal_list` | `goal list` | なし |
| `goal_show` | `goal show ID` | `id`・`full` |
| `lint` | `lint` | `tasks`・`proposals`（どちらかは要る） |
| `observe_history` | `observe --history` | `limit`（`observer::HISTORY_LIMIT`） |

`show`・`proposal_list`・`proposal_show`は上の表。paramsは書き出すファイルも実行するprogramも受け取らず（`stats`と`planners`の`--cmux`、`graph`の`--out`）、読み取りでserviceが別のprogramを起動するのは`stats`の`workspace_mismatch`のためのserviceの`--cmux`だけ（`graph --format svg`のd2は起動しない）。serviceの`--cmux`が見つからなければ、CLIと同じくworkspaceを見ない。`domain::queue_service::UseCase::of_command`が、dagqのコマンドの引数（サブコマンドから）から行き先のユースケースを決める（段(3)のクライアントモードが使う。`graph --out`・`ask close`・`--history`の無い`observe`は`None`）。

### 読み取りのroleとworkerが打つコマンド

promptとskillが打たせるdagqのコマンドと、行き先のユースケース。jobのpromptに現れる`` `dagq …` ``が全てユースケースに行くことを`application::prompt`のunit test（`every_dagq_command_a_job_s_prompt_names_is_a_use_case_of_the_queue_service`）が、それぞれの読み取りがserviceでもCLIと同じJSONを返すことを`tests/it/queue_service_reads.rs`が確かめる。

| 呼び出し元 | どこが言うか | コマンド |
| --- | --- | --- |
| plan review job（`ReadFilesAndQueueCli`） | `prompt::plan_review_prompt`と`RECORD_READING`、AGENTS.mdのplan reviewの節 | `show ID`・`proposal show ID`・`search`・`related`・`findings`・`stats`・`lint`・`events --full`・`timeline RUN` |
| goal review job（`ReadFilesAndQueueCli`） | `prompt::goal_review_prompt` | `show ID`・`goal show ID --full`・`findings`・`events --goal ID --full`・`search` |
| observer（`QueueCli`） | `observer::observer_prompt` | 読み取り: `findings [ID] [--full]`・`stats`・`kpi`・`marks`・`notes`・`show ID`・`asks`・`graph`・`forecast`・`goal show ID`・`events --full`・`timeline RUN`・`observe --history`。書き込み: `finding record`・`finding resolve`・`ask --kind blocked --finding ID` |
| スループットの見直しのjob（`QueueCli`） | `throughput_review::review_prompt`と、それが載せるdagq skillの`reference/kpi.md`の手順 | `kpi [--period] [--last] [--area] [--change]`・`stats [--since] [--until] [--full]`・`timeline RUN`・`events --full --kind --since --until`・`asks`・`marks`・`findings`・`show ID`・`forecast` |
| review job・復旧job（`ReadFiles`） | `prompt::review_prompt`・`prompt::recovery_prompt` | なし（Bashを持たず、dagqを打たない） |
| runtimeが立てるplanner | `RECORD_READING` | `events --full`・`timeline RUN`（plannerは段(3)でもDBを直接開く） |
| worker | runtimeのprompt（`WORKER_READING`） | `ask`（書き込み）。`list`・`show`は作業の初めに打たないよう言う（権限の範囲ではない。ADR-t1233-5決定3） |
| worker（`measure`のtask） | AGENTS.mdの「テストの制約」のchangeの`measure`、measureのtaskのdescription（commit 2ae2c673のtask、task 1205・1114・1034・1026・601・1200） | `stats --full`・`events --full`・`timeline RUN`・`kpi`（`--compare`・`--area`・`--change`・`--by`）・`marks`・`forecast` |
| worker・job | dagq skillの`SKILL.md`と`reference/`（`inspect.md`・`kpi.md`・`register.md`・`scope.md`・`provider.md`・`goal-close.md`・`observer.md`） | `list`・`show`・`graph`・`candidates`・`status`・`asks`・`events`・`timeline`・`stats`・`kpi`・`forecast`・`notes`・`marks`・`findings`・`search`・`related`・`proposal list`・`proposal show`・`goal list`・`goal show`・`lint`・`observe --history` |

ユースケースの無いコマンド（クライアントモードではserviceに届かない）:

- `watch`（`queue.watch`。inboxのもの）、`report`と`graph --out`（`export.file`。ファイルを書く）: jobとworkerのpolicyに無い操作で、今のCLIでも拒まれる
- `graph --format svg`: hostのd2を起動するので、serviceは読み取りとして受け取らない（`json`と`d2`は答える）。promptとskillはsvgを名指さない
- `locate`（DBのpathを返す）・`doctor`（hostとDBのファイルの診断）・`service status`・`broker status|logs|audit`・`planners`: 制御側の状態を見るもので、jobのpromptは名指さない。dagq skillは`doctor`を名指すが、AGENTS.mdはworkerの実queueでの`doctor`の確認を人かinboxに任せる。クライアントモードでどう答えるか（DBのpathを出さずに答えるか、届かないことの分かるerrorにするか）は段(3)が決める

## 起動と停止（ADR-t1233-4決定1・2）

serviceは`dagq --db <db> service serve --cmux <cmux>`で、固定バイナリ（`up`かsupervisorを動かしているもの）から起動する。起動するときはactorを名指すenv（`DAGQ_ROLE`など）を外し、`setsid`で起動元のsessionから離し、もう1度forkして起動元の子でなくし（execで入れ替わるsupervisorにzombieを残さない）、`service.log`に出力を足す。起動したものは、このbuildのserviceが答えれば（競った別の起動元のものでも）成功とする。`SIGINT`・`SIGTERM`で受け付けを止めて終わる。queueのDBが消えれば（使い捨てのqueueの片付け）2秒以内に自分で終わる（`outcome: queue_gone`）。

- `up`: preflightの後、supervisorより先に`application::queue_service::ensure`を打つ。このbuildのserviceが答えれば`reused`、居なければ`started`、別のbuildか答えないものが居れば止めて`replaced`。起動できなければ`up`はsupervisorを起動せずに止まる。結果は`up`の出力の`queue_service`（`outcome`・`service`・`replaced`）で、`started` / `replaced`はevent `queue_service_started`（`by: up`）に残す。`UpOptions::queue_service`がfalse（`up`の他の段のtest）なら何もしない
- supervisor: `up`が起動したsupervisor（`--once`でないもの）は`QUEUE_SERVICE_INTERVAL`（10秒）ごとにserviceを見て、居ない・答えないものは起動し直し、別のbuildのもの（`install`や自動更新の引き継ぎの前のバイナリが残したもの）は入れ替える（`queue_service_started`、`by: supervisor`、2回目からは`restart: true`、入れ替えなら`replaced`）。起動し直しは`QUEUE_SERVICE_RESTART_WINDOW`（600秒）に`QUEUE_SERVICE_RESTARTS`（3回）まで。別のbuildの入れ替えは数えず、入れ替えて動いたbuildはそれ以後そのまま受け入れる（`install`が新しいバイナリを置いた後でexecの前のsupervisorは、自分のbuildと違う新しいserviceを入れ替え続けない）。drainと引き継ぎの間は見ない（execはserviceを次のプロセスに残し、次のプロセスがbuildの違いで入れ替える）
- serviceが居ない間、supervisorは新しいclaimと、queueのjob（plan review・goal review・observer・スループットの見直し）の起動を控える。走っているrunとそのreview・復旧は止めない（ADR-t1233-4決定2）
- `down`: supervisorが居なければ（`--force`の後、`--wait`のdrainの後も）serviceを止めて`queue_service_stopped`（`by: down`）に残し、出力の`queue_service`に`{"outcome": "stopped", "pid"}`を出す。drainを待たない`down`は、signalの前に`queue_service_stop_requested`（`supervisors`・`by: down`）を書いて`{"outcome": "left_to_the_drain"}`を出し、drainを終えた最後のsupervisorが止める（`queue_service_stopped`、`by: supervisor`）。serviceが動いていなければ`queue_service`の欄を出さない
- `install`と`install --allow-breaking`: `install`のdrain（`down --wait`）の後の`up`が新しいバイナリで起動する。drainしない引き継ぎでは、新しいバイナリのsupervisorの最初の見張りが入れ替える
- 人の手: `dagq service start [--cmux]`（`service.lifecycle`。`ensure`と同じで`by: service start`）・`dagq service stop`（`by: service stop`。supervisorが動いていれば次の見張りで起動し直す）・`dagq service status`（`queue.read`）

## 落ちたときの知らせ（ADR-t1233-4決定3）

supervisorが起動し直せなかった（`start_failed`）か、上限に達した（`restart_limit`）ときは、queueのattention `queue_service_down`（`reason`・`message`・`supervisor`）をDBに直接書く（serviceを通らない）。inboxの`watch`と`status`はDBを直接読むので、serviceが落ちても届く。1つの失敗につき1回で、serviceがまた動けば（`queue_service_started`・`queue_service_running`）消える。attentionは`kind: queue_service_down`・`status`にその`reason`・`next: dagq service status`・`last_error`に`message`（`run_id` / `task_id`はnull）。人は`dagq service status`と`service.log`を見て、`up`か`service start`を打ち直す。supervisorも居ないときは今までどおり`restart supervisor`が出る。

段(3)以降のクライアントモードのdagqは、serviceに届かないときに届かないことの分かるerrorを返し、DBを直接開くことへ戻らない（ADR-t1233-1決定7）。

## statusとdoctor

`status`（`--role`なしと`--role inbox`）・`doctor`・`dagq service status`は`queue_service`を出す（`compose::queue_service_view`）。

```json
{"state": "running", "socket": "<queue dir>/service/queue.sock", "pid": 4242, "build": "0.4.0-dev+abc", "api_version": 1, "min_api_version": 1, "build_matches": true, "started_at": 1790000000, "client_api_version": 1, "attention": false}
```

- `state`: `running`（`hello`に答えた）・`stopped`（lockを誰も持たない）・`unreachable`（lockは持たれているが答えない。`error`に理由）
- `pid`・`build`・`api_version`・`min_api_version`は`hello`の答え、答えなければ`state.json`の記録。`build_matches`はこのバイナリのbuild識別子と同じか
- `client_api_version`はこのバイナリが話すAPIの版
- `attention`は`queue_service_down`が立っているか（queueが読めなければnull）

見るだけで、serviceを起動も停止もしない（socketへの`hello`を1回打つ）。

## event

queueのevent（`EventKind::is_queue`）: `queue_service_started`（`by`（`up`・`supervisor`・`service start`）・`pid`・`build`・`api_version`・`socket`・`restart`・`replaced`、supervisorが書くものは`supervisor`）・`queue_service_stopped`（`pid`・`by`（`down`・`supervisor`・`service stop`））・`queue_service_stop_requested`（`supervisors`・`by: down`）・`queue_service_down`（attention。`reason`・`message`）・`queue_service_running`（答えがまた返った）・`queue_service_unauthenticated`（`use_case`・`reason`）。

## 権限

`service start`・`service stop`・`service serve`は`up`・`down`と同じ`service.lifecycle`（`Operation::QueueService`。user・inbox・planner・supervisor）、`service status`は`queue.read`。workerとjobのClaudeの`permissions.deny`には`Bash(dagq service start:*)`などが入る（`DAGQ_COMMANDS`）。

## まだ無いもの

- 段(3): workerとjobのdagqのクライアントモード、claim・resume・jobの起動でのtokenの発行と受け渡し、runの終わりでのtokenのfileの片付け、Codexのsandboxからsocketへの到達（ADR-t1233-5決定4）
- 段(3): クライアントモードの`locate`・`doctor`をどう答えるか（[読み取りのユースケース](#読み取りのユースケース)の「ユースケースの無いコマンド」）
- 段(4)〜(6)（goal 38）: queueのbroker、supervisorとCLIのservice経由化、integrateのverificationの隔離
