---
id: design-queue-service-host-cli
type: design
title: Queue serviceのhostのCLI
status: current
created: 2026-10-23
scope: runtime
tags:
  - security
  - runtime
related:
  - adr-t3102-1
  - adr-t3102-2
  - adr-t3102-3
  - adr-t3105-1
  - design-queue-service
  - design-authorization
---

# Queue serviceのhostのCLI

人・inbox・plannerがhostで打つ`dagq`が、queue serviceへ送るものと送らないものの地図。
[Queue service](queue-service.md)の[クライアントモード](queue-service.md#クライアントモード)（workerとjob）の続きで、決定の理由は[ADR-t3102-1](../adr/2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)決定1・2、[ADR-t3102-2](../adr/2026-10-22-t3102-2-only-the-queue-service-opens-the-db-schema-by-the-service-build-and-migration-order.md)決定5〜7、[ADR-t3102-3](../adr/2026-10-22-t3102-3-stage-5-outage-notice-and-use-cases-for-multi-step-control-operations.md)決定2、[ADR-t3105-1](../adr/2026-10-23-t3105-1-reopen-the-inbox-once-after-the-host-cli-reaches-the-queue-service.md)。

## 送る範囲

- 宛ての環境が無い`dagq`のうちuser・inbox・plannerのもの（`host_client_role`）は、serviceが答えるコマンド（`client_request`）と`watch`の各回の見る（`watch_poll`）をそのsocketへ送り、DBを開かない。
  serviceが一度も動いていないqueueも同じで、DBが無ければ送る前に`no queue at`で終わる（ADR-t3105-1決定3）。
- それ以外のコマンド（ユースケースの無い書き込みと、制御側の診断とfileを書く読み取り）は今はDBを直接開く。
  1つのコマンドの中で経路を混ぜない。
- repositoryのqueueの束縛は、CLIが解決したrepositoryを要求に載せ（`ServiceRequest::binding`）、serviceが答える前に照らす。
  serviceは読み取り（tokenのrunとtargetの確かめを含む）を読み取り専用の接続で（floorの内側の古いschemaはmemoryの写しで）、`watch_poll`を今のqueueの読み取り専用の接続で答え、policyの断りは書ける別の接続で残す（`ServiceBackend::open_read`・`open_watch`）。
- 宛てのqueue: socketは解決したDB（linkはその先）のdirのserviceのもので、要求はそのDBの絶対pathを持つ（`ServiceRequest::queue`）。
  serviceはそれが自分の扱うDBと同じfileでなければ（同じdirの別のDB、無いfile）、何も開かず読まずに`wrong_queue`で断る。
- `watch`の待ち・inboxのwatcherの記録・`[push]`はクライアント側に残る（`application::watch::watch_through`）。
  pushが名指すrepositoryは、serviceが見るの答えに載せるqueueの束縛から取り、束縛のあるqueueではwatchを打つ場所に依らない（束縛の無いqueueは打つ場所の名前。`WatchLook::binding`）。
- `graph --format svg`はd2のsourceをserviceから読み、hostのCLIが自分のd2で描く（serviceは読み取りでprogramを起動しない）。

## principal

- userはtokenを持たない。
  tokenもroleも無い要求を、serviceは同じOSのユーザーのpeer（serviceが接続から読む）ならuserとする。
- 環境がroleを名指してtokenが無い要求はそのroleを名乗り、serviceは`missing_token`で断る（`QueueService::authenticate`）。
  inboxとplannerには理由と次の手を返す（`missing_token_next`）。
- inboxとplannerは環境が名指すtokenのfileで送る（queueのpathも残す）。
  fileがそのqueueの`service/credentials/`に無ければ（`--db`が別のqueueを名指すなど）、tokenを送らずに`wrong_queue`で終わる（`issued_by`）。
  fileはserviceのdirに置くので、sessionが続くかぎり自動更新とserviceの起動し直しをまたいで使える。
- inboxのtokenは`dagq inbox`が、agentに場所を譲る自分のprocess（pidと、環境のzoneに依らずUTCで読むstart）をsessionとして発行する（`Principal::inbox`）。
  fileはsessionごとに別で（`credential_path_of`）、開き直しの発行がqueueのlockの下で前のsessionのtokenとfileを消してから書くので（同時に開いても残るのは1つ）、前のsessionのCLIは新しいtokenを読めず`no_credential`で終わる。
  sessionのprocessが終われば（pidを別のprocessが取っても）serviceが`session_ended`で断り、supervisorのsweepが消す（startが読めないときは残す。`session_token_ended`）。
- plannerのtokenはplannerのturnを起動するとき、無いときだけ発行する（`HostActorExecutor`）。
  plannerが閉じた後はserviceが`target_ended`で断り、supervisorのsweepが消す（`planner_token_ended`）。
- 発行は今は制御側がfileに直接書く。
  親のprincipalの起動の権限と対象の前提をserviceが発行と同じtransactionで確かめる経路（ADR-t3102-1決定2）はまだ無い（ADR-t3105-1決定5）。
- hostのCLIの要求は`HOST_CLIENT_API_VERSION`からの版で送る。
  queueとsessionを読まない前の版のserviceは、tokenを読む前に`api_version_mismatch`で断る（[API versionと互換](queue-service.md#api-versionと互換)）。
  serviceを見る`hello`（`probe`とsupervisorの起動の確かめ）は`MIN_API_VERSION`で送るので（`request_version`）、前の版のserviceも届かないではなく別のbuildとして見え、supervisorが入れ替える。
- `planners`は制御側の状態を見る読み取りで、hostのCLIだけが送る（クライアントモードのworkerとjobは`control_side_command`で断る）。

## 計画のユースケース

planner・inbox・人が打つ計画の書き込みと`mark`は、serviceのユースケースとして送る（ADR-t3102-3決定3・4）。
一覧は`application::queue_writes::QueueWrite`（ユースケースの名前はコマンドの名前で、サブコマンドは`_`でつなぐ）で、paramsはそのコマンドのoption（名前はlong optionの名前）、答えはそのコマンドの出力。
CLIのDBの経路とserviceは同じ`compose::write_queue`（repositoryの規則をqueueに載せてから`queue_writes::answer`）で答え、認可と拒否の記録はCLIと同じapplicationの`Planning`・`Requests`がprincipalのactorで行う。
1つのユースケースはapplicationの1回の呼び出しで、その書き込みはstoreの1つのtransactionで終わる。

- `submit`のproposalの持ち主は、環境ではなくprincipalから決める（plannerならその記録のoriginとbackgroundのhandle、ほかは人。`queue_writes::principal_owner`）
- `request add`の言葉と`submit --replacement`の置き換えの計画は、クライアントがファイルかstdinから読んで送る。
  環境のroleが`request.record`を持たなければ読まずに送り、serviceがprincipalで断って記録する
- `ready --inherit`はrunのbranchを動かす制御側のGitの操作なので（ADR-t3102-3決定5）ユースケースにせず、今はDBを直接開く

## 届かないとき

`status`・`watch`も`unreachable`で直ちに終わり、errorの文が次の手（`dagq service status`・`dagq up`）を言う。
空の答えや待ちに替えず、DBを開くことへ戻らない。
`watch`の1回の見るは`watch`の残りの時間（`LOOK_FLOOR`から`LOOK_LIMIT`の間）を接続・要求・答えの全体の1つの期限にするので、backlogが一杯か、答えないか、答えを少しずつ送るserviceでも、`watch`は時間切れから`LOOK_FLOOR`の内に`unreachable`で終わる（`application::watch::look_limit`と`infrastructure::queue_service::call`）。
`status`はserviceが`hello`に`PROBE_TIMEOUT`の内に答えてから送るので、そのようなserviceでは`PROBE_TIMEOUT`の内に`unreachable`で終わる。
`hello`に答えて`status`に答えないserviceでは、`status`の答えを`LOOK_LIMIT`まで待ってから`unreachable`で終わる（`Client::look_within`）。
supervisorが記録した停止（[落ちたときの知らせ](queue-service.md#落ちたときの知らせadr-t3102-3決定1)）があれば、errorの`queue_service.outage`に載せる。
`watch`はroleごとに同じ停止を1回だけ返し（返した停止はqueueのdirのroleごとの`OUTAGE_TOLD_FILE`）、そのroleの次の`watch`は停止の間は待ち、時間切れではeventを空にして`outage_since`を返す（`application::watch::look_through`）。

## 段の後の一度だけの開き直し

この段を含むbuildへの入れ替えの後に一度だけ、前のbuildで開いたinboxは開き直し、そのとき開いていたruntimeのplannerは閉じるのを待つ（ADR-t3105-1決定1）。
前のbuildのsessionはtokenのfileを持たず、そのagentの`dagq`は`missing_token`で断られる。
名乗るroleだけで認めることも、DBへ戻ることもしない。
手順はpluginの`dagq-recover`の`reference/update.md`。
