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
- 送らないのはADR-t3102-2が残すものだけで、今はDBを直接開く: DBの無い所に作る`init`と`service serve`（決定1）、queueを開かないもの、wrapperとhookの経路（決定6の(3)）、runのbranchを動かす`ready --inherit`（ADR-t3102-3決定5）、制御側のプロセス（`supervise`・`up`・`integrate`・`review`・`observe`など。(5)）と`migrate`・`doctor`（(6)）。
  CLIの全ての葉は送るものかこの例外のどれかに当たり、どちらでもない葉は無い（unit testが全ての葉を数えて確かめる）。
  1つのコマンドの中で経路を混ぜない。
- repositoryのqueueの束縛は、CLIが解決したrepositoryを要求に載せ（`ServiceRequest::binding`）、serviceが答える前に照らす。
  serviceは読み取り（tokenのrunとtargetの確かめを含む）を読み取り専用の接続で（floorの内側の古いschemaはmemoryの写しで）、`watch_poll`を今のqueueの読み取り専用の接続で答え、policyの断りは書ける別の接続で残す（`ServiceBackend::open_read`・`open_watch`）。
- 宛てのqueue: socketは解決したDB（linkはその先）のdirのserviceのもので、要求はそのDBの絶対pathを持つ（`ServiceRequest::queue`）。
  serviceはそれが自分の扱うDBと同じfileでなければ（同じdirの別のDB、無いfile）、何も開かず読まずに`wrong_queue`で断る。
- `watch`の待ち・inboxのwatcherの記録・`[push]`はクライアント側に残る（`application::watch::watch_through`）。
  pushが名指すrepositoryは、serviceが見るの答えに載せるqueueの束縛から取り、束縛のあるqueueではwatchを打つ場所に依らない（束縛の無いqueueは打つ場所の名前。`WatchLook::binding`）。
- `graph --format svg`はd2のsourceをserviceから読み、hostのCLIが自分のd2で描く（serviceは読み取りでprogramを起動しない）。
- `stats`（`--ledger`を含む）と`kpi`はまず`stats_inputs`で、main checkout・着地したcommit・mainの履歴を読み始める秒（衝突のeventの無いqueueでは無し）・衝突したfileと、それらの版を読む。
  main checkoutはserviceが束縛したGitのcommon directoryからGitなしで、`main_checkout_of`と同じものに決め（[main checkoutの決め方](supervisor-lifecycle/run-environment.md#main-checkoutの決め方)）、クライアントが名指すpathは受けない。
  クライアントはそのcheckoutのGitで各commitのareaと、`stats`ならその秒からのmainの履歴を衝突したfileと改名先に絞って読み、版と一緒に送る（`application::stats::call_read`）。
  読めなかった履歴は理由の文で送り、serviceはDBを開く経路と同じ「読めない」にする。
  serviceは集計するeventから同じ入力と版が出るときだけ受けたものを使い、Gitを起動しない。
  間に着地か衝突が来て版が違えば断り、クライアントは読み直して送り直す（3回まで）。
  Gitで読んだものを送らない要求（serviceより古いクライアント）は、少なく答えずに断る。
  areaは集合の表とその番号、履歴はcommitの時刻の差と名前の表の番号で送り、それでも要求の大きさを超えれば送らずに断る
- `service status`はserviceに届かなくても答える。
  socketとserviceの記録はhostのCLIが見て、届けばqueueのattentionだけを`service_status`（`queue.read`）で読み、届かなければ[落ちたときの知らせ](queue-service.md#落ちたときの知らせadr-t3102-3決定12)の記録と次の手を返す（`application::queue_service::host_status`）。
  どちらでもDBを開かない。

## 対話と操作

人とinboxがhostで打つ対話と操作のコマンド（ADR-t3102-2決定6の(4)）。
一覧は`domain::queue_service::UseCase`、paramsと認可は`application::queue_commands::QueueCommand`。
CLIのDBの経路とserviceは同じ`compose::run_command`で走らせるので、答えはCLIの出力と同じになる。
書き込みは1つのコマンドにつき1つのtransactionでserviceの中で終わり（`planner request`の2つのeventも一緒に確定する）、eventのactorは呼び出し元のprincipal。
Gitとprogramはserviceでなくクライアントがユースケースの外で走らせる（ADR-t3102-3決定5）。

- 認可: runtimeの操作（`rebind`・`recover`・screen・`planner request`）と`report`・`graph --out`（`queue.export`）は走らせる前にCLIと同じcapabilityを`Gate`で通し、`answer`・`ask close`・`hold`・`release`・`replan request`はapplicationがqueueを読んで通す
- paramsのpathは絶対path（serviceは別のdirectoryで動く）。
  `planner request`の言葉はクライアントが読み、環境が名指すroleがpolicyで`planner.request`を持たなければ読まずに送るので、断られる呼び出し元のfileとstdinは読まれない
- `run log`・`planner log`の答えはlogを読むbackgroundのsessionで、fileを出し`--follow`で追うのはクライアント。
  `graph --out`の答えはd2のsourceと載るtaskで、描く（svg）のと書くのはクライアント
- `report`はまず`report_inputs`で近い依存図のd2のsourceと束縛したrepositoryを読み、main checkoutの`dagq.toml`が`[areas]`を持てばそのcheckoutを載せてもう一度聞いて、reportが読む期間（並べる期間と、その前のbaselineとtargetの連続の分）に着地したcommitだけを読む。
  クライアントはmain checkoutのGitでそのcommitのareaを決め、自分のd2で描いた図と一緒に送る。
  要求がserviceの受ける大きさを超えれば図を描かなかったものとして理由と送り、それでも超えれば送らずに断る
  serviceはそれとserviceの環境（`TZ`・`host.toml`）でreportを作り、`--out`の無いものはqueueの`reports/`に書く。
  SVGはd2の出力と同じく整ったXMLの`<svg>`であることだけを確かめ（整っていないものは図を載せない）、pageはそのbytesの`data:` URLの`<img>`として載せるので、browserは画像として描いてscriptを動かさず外を読まず、そのstyleはpageに及ばない。
  serviceが`dagq.toml`を読むcheckoutは、`stats`・`kpi`と同じく束縛からGitなしで決めたmain checkoutだけで、クライアントが名指すcheckoutはそれとreal pathが同じときだけ受け、違えばその中を読まずに断る（`application::report::accept_checkout`）。
  serviceが読む`dagq.toml`は大きさに上限があり、通常のfileでないもの（deviceやFIFOへのsymlinkなど）は読まずに断る
- `release`はまず`release_read`で成果を読むrunを聞き、runを終える解除ならクライアントが成果を読んで保存し、headを`refs/dagq/runs/<run-id>`に残してから送る。
  serviceは読んだ時のholdと版を確定の中で照らし直し、食い違えば何も変えずに断る。
  hostの`dagq`はcontainerのrunのfacts-onlyのturnを自分で走らせ、そのimageの記録はDBを開かずに`release_images`で送る（成果が読めたかに依らず、失敗の時刻はserviceが記録する時のもの）。
  queueのdirを知らないクライアントモードの`dagq`はcontainerを起こさずに読むので、factsが最後のturnの終わりのものでないcontainerのrun（facts-onlyのturnが要る）は断り、hostの`dagq`に任せる
- `rebind`は別のrepositoryに束縛されたqueueで打つものなので束縛を載せずに送り、`logs/`の記録はクライアントがserviceの答えから書く。
  checkoutのGitのcommon dirと、クライアントのdata homeがそのrepositoryに与えるqueueのdir（`move_to`の先）はクライアントが読んで送り、答えが挙げるrunのworktreeはクライアントがGitで直す。
  生きているsupervisorが居ると拒まれ、`down`はserviceも止めるので、`up`でなく`service start`でserviceだけを起こしてから打つ（手順はpluginの`dagq`の`reference/locate.md`）

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
supervisorが記録した停止（[落ちたときの知らせ](queue-service.md#落ちたときの知らせadr-t3102-3決定12)）があれば、errorの`queue_service.outage`に載せる。
`watch`はroleごとに同じ停止を1回だけ返し（返した停止はqueueのdirのroleごとの`OUTAGE_TOLD_FILE`）、そのroleの次の`watch`は停止の間は待ち、時間切れではeventを空にして`outage_since`を返す（`application::watch::look_through`）。

## 段の後の一度だけの開き直し

この段を含むbuildへの入れ替えの後に一度だけ、前のbuildで開いたinboxは開き直し、そのとき開いていたruntimeのplannerは閉じるのを待つ（ADR-t3105-1決定1）。
前のbuildのsessionはtokenのfileを持たず、そのagentの`dagq`は`missing_token`で断られる。
名乗るroleだけで認めることも、DBへ戻ることもしない。
手順はpluginの`dagq-recover`の`reference/update.md`。
