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

- 宛ての環境が無い`dagq`のうちuser・inbox・plannerのもの（`host_client_role`）は、serviceが答えるコマンド（`client_request`）と`watch`の各回の見る（`watch_poll`）と制御側のコマンドのstep（`control_call`、下の「制御側のコマンド」）をそのsocketへ送り、DBを開かない。
  serviceが一度も動いていないqueueも同じで、DBが無ければ送る前に`no queue at`で終わる（ADR-t3105-1決定3）。
- 送らないのはADR-t3102-2が残すものだけで、今はDBを直接開く: DBの無い所に作る`init`と`service serve`（決定1）、queueを開かないもの、制御側のプロセス（`supervise`・`install`など。(5)）と`migrate`・`doctor`（(6)）。
  `up`・`down`・`inbox`・`service start`・`service stop`は自分のプロセスに残るが、queueの読み書きは下の「制御側のコマンド」のstepとして送る。
  wrapperとhookの経路（決定6の(3)）は`client_request`でなく、wrapperはwrapperのtokenで（runのturnのcontainerの準備を含む）、inboxの`session-event`はspanをinboxのtokenで自分で`session_record`に送り、DBを開かない（[クライアントモード](queue-service.md#クライアントモード)）。
  CLIの全ての葉は送るものかこの例外のどれかに当たり、どちらでもない葉は無い（unit testが全ての葉を数えて確かめる）。
  1つのコマンドの中で経路を混ぜない。
- 送らないコマンドでもCLIが自分で拒んだもの（`check_access`とruntimeの操作の判定）は、その拒否だけを`denial_record`で呼び出し元のprincipal（tokenか、socketのpeerのuser）として送り、serviceがそのprincipalの`authorization_denied`に記録する（`infrastructure::denials::ServiceDenials`）。
  serviceは自分のroleと違うroleの拒否を`authorization_denied`で断り、そのroleが持つcapabilityを`not granted`とする拒否と、resourceの記録の形でないものは記録しない。
  tokenが無くroleを名乗る呼び出しはserviceが`queue_service_unauthenticated`に残す。
  短い待ちの内に届かないか、DBが無ければ記録せずに拒否を返し、DBを開かない。
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
- `queue-broker status`も同じくserviceに届かなくても答え、brokerの記録とhealthをhostのCLIが見て、attentionは`service_status`の答えから読む（`application::queue_broker::host_status`）。

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
  `--out`のreportは、`graph --out`と同じくserviceが作ったものを答え（JSONとHTMLと保持の設定）、`--out`の下に書いて古いreportを消し索引を書くのはクライアントが自分の権限で行う。
  serviceがreportを書き消すのはqueueの`reports/`の下だけで、`out`を名指すparamsは何も書かずに断る
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

## 制御側のコマンド

人とinboxがhostで打つ`integrate`・`review`・`observe`（`--history`と`--input`は読み取り）・`throughput-review`・`setup-review`は、自分のプロセスでGit・検証・agentを走らせ、queueの操作を1つずつそのコマンドのstepとして`control`のユースケースで送る（ADR-t3102-3決定3〜5）。
hostの`dagq`（`--db`）もクライアントモード（socketとtoken）も同じ経路で、brokerを通る宛てからは送らない（`ControlCaller`）。
stepの形はsupervisorの操作と同じ名前と引数（`ControlParams`）で、serviceの側は`application::supervisor_service::control`。
`up`・`down`・`dagq inbox`・`service start`・`service stop`・`queue-broker start|stop`も、launchdとプロセスとserviceかbrokerのプロセスを自分で扱い、登録・event・runの読み書きをstepとして送る（`OneShot::lifecycle_route`）。
`up`と`service start`はserviceを起動してから、`down`と`service stop`はserviceを止める前に送り、`service start`は動いているserviceを再利用するときも最初のstepを送る。
`up`が起動する入れ替えの口のserviceは、引き継ぐ前にqueueのrepositoryの束縛を確かめ、違えば何も入れ替えもmigrateもせずに断る（`ServeOptions::binding`）。

- 認可: serviceは各stepで、そのコマンドのoperationをCLIと同じcapabilityでprincipalに通し、断りを記録する（`control::operation`）。
  CLIと同じく、断られるprincipalは最初のstep（queueとactorを聞く`control_queue`）で断られ、何も始まらない
- 範囲: 答えるのは`queue.read`の読み取りと、そのコマンドが自分で書く記録だけで、eventはコマンドごとの種類に限る（`control::scope`）。
  runのeventは着地するtask（`--next`ではどのtaskでも）の着地を待つか着地中のrunだけで、`requested_by`は名乗れない。
  runの資源とhostの負荷のsampleの読みと記録、台帳を作る係の読み書きはsupervisorだけのもので、どのコマンドにも`authorization_denied`で断り、断りを記録する。
  `up`・`down`はどのsupervisorの登録にも、刈り取り・引き継ぎ・modeと自動更新の設定・sessionの記録を送れる（`control::lifecycle_ops`。CLIがDBで書くのと同じ）
  引き継ぎで渡すbinaryは、接続のpeerのプロセスの実行ファイルだけで、他のpathは`authorization_denied`として断って記録する（`control::handoff_refusal`）
  eventのactorは呼び出し元のprincipalで、CLIがDBで書くのと同じ
- 着地の承認: `integration_approved`は自分のstep（`approve_integration`）だけで書き、serviceはrunの状態が呼び出し元の読んだまま着地かsessionを待つこと、生きたleaseが持たないこと（serviceのプロセスと時計で見る）、承認がまだ無いことを、記録と同じwrite transactionで確かめる（ADR-t3102-3決定4）。
  承認済みのrunには何も書かない
- コンテナの検証: 着地の検証が読むrelayの注意と、作るimageの記録もstepで送り、eventは呼び出し元のもの。
  dagq machineを共有する利用者のrunは、そのqueueのものをserviceで読み、届かない他のqueueのものは分からないものとして数えたまま残し（DBのfileが無いqueueのものは無いrunとして数えない）、どのqueueのDBも開かない（`podman_machine::OwnerRuns`）
- 観測の壁: `observe`のagentが認証切れか利用上限で止まったときのqueue_holdのask（observerのjobで、runなし）もstepで開くか加わり、`auth_required`・`usage_limited`は呼び出し元のeventになる（`control::own_hold`）
- 着地: 呼び出し元は自分で作ったleaseでslotを取り、serviceはrunがコマンドの名指すtaskのものであること、そのleaseを持つrunとsupervisorが無いことを確かめ、slotと着地のintegratorのtoken（`integrator:<pid>`、pidはコマンドのプロセス）を同じtransactionで発行する（ADR-t3102-1決定2）。
  着地の残りとleaseのheartbeatはそのtokenでintegratorとして送り、eventはintegratorのもので`requested_by`が呼び出し元。
  tokenは着地が`integrating`を出たときに呼び出し元が失効させ、serviceは別のtaskのrun、続いている着地、supervisorの着地のものは失効させない
  slotとtokenの発行と失効は、stepの名乗るコマンドのプロセスが接続のpeerと同じときだけ受け、別のプロセスを名乗るか接続がプロセスを伝えないstepは、何も作らず失効させずに`authorization_denied`として断って記録する
- telemetry: agentと検証のsessionの登録、runを持たない対象のtokenの発行と失効（leaseなし）、対象の事実もstepで送る。
  対象はコマンド自身のものだけ（着地するrun、呼び出し元が始めを記録した観測とスループットの見直し。`control::own_target`）
  agentのjobのtokenは呼び出し元のprincipalの依頼でqueue serviceの`issue_token`が発行する（[Queue service](queue-service.md#principalとtoken)）
- 結果の記録（eventと着地）は、serviceが取るまでこのプロセスが期限なく送り直し、同じ記録はidentityで1回だけ取る（`control::resending`）。
  呼び出し元のprincipalで送れるのはこのプロセスだけなので、supervisorのjobのコマンドと違い、期限の後に記録を残して終わることはしない
  記録でないstepが届かないときは新しいstepを始めず、DBを開かずに`unreachable`で終わる
  `up`・`down`・`inbox`・`service start|stop`・`queue-broker start|stop`は送り直さず、届かないstepはそのerrorでコマンドが失敗する。
  例外は`down`と`service stop`の止める前の記録で、届かないか記録だけが失敗すれば警告して止め（`service stop`は`unrecorded`）、`service stop`の呼び出し元をserviceが最初のstepで断れば（認証・認可・束縛）止めずに失敗する（`queue_service::stop_record`）。
  `queue-broker start|stop`は最初のstepを止めるか起動する前に送り、断られれば何もしない。
  startはserviceに届かなければ起動せず、起動の記録が失敗すればその失敗で終わる（brokerは動いたまま）。
  stopはserviceに届かないか止めた後の記録が失敗しても止めて`unrecorded`を返す（`queue_broker::start_by_hand`・`stop_by_hand`）。
  `down --wait`は、drainを終えた最後のsupervisorがserviceを止めて届かなくなれば、supervisorのプロセスだけで終わりを見る

## principal

- userはtokenを持たない。
  tokenもroleも無い要求を、serviceは同じOSのユーザーのpeer（serviceが接続から読む）ならuserとする。
- 環境がroleを名指してtokenが無い要求はそのroleを名乗り、serviceは`missing_token`で断る（`QueueService::authenticate`）。
  inboxとplannerには理由と次の手を返す（`missing_token_next`）。
- inboxとplannerは環境が名指すtokenのfileで送る（queueのpathも残す）。
  fileがそのqueueの`service/credentials/`に無ければ（`--db`が別のqueueを名指すなど）、tokenを送らずに`wrong_queue`で終わる（`issued_by`）。
  fileはserviceのdirに置くので、sessionが続くかぎり自動更新とserviceの起動し直しをまたいで使える。
- inboxのtokenは`dagq inbox`がuserとしてserviceに依頼し、agentに場所を譲る自分のprocess（pidと、環境のzoneに依らずUTCで読むstart）をsessionとする（`Principal::inbox`、`RequestedServiceAccess`）。
  serviceが答えなければinboxを開かない。
  fileはsessionごとに別で（`credential_path_of`）、開き直しの発行がqueueのlockの下で前のsessionのtokenとfileを消してから書くので（同時に開いても残るのは1つ）、前のsessionのCLIは新しいtokenを読めず`no_credential`で終わる。
  sessionのprocessが終われば（pidを別のprocessが取っても）serviceが`session_ended`で断り、supervisorのsweepが消す（startが読めないときは残す。`session_token_ended`）。
- plannerのtokenは、supervisorがplannerのwrapperを起動する前にserviceに依頼し、wrapperはturnにそのfileを渡すだけで発行を依頼しない（`HostActorExecutor`）。
  plannerが閉じた後はserviceが`target_ended`で断り、supervisorのsweepが消す（`planner_token_ended`）。
- 発行はserviceの`issue_token`が、親の権限と対象の前提を発行と同じtransactionで確かめて行う（ADR-t3102-1決定2、[Queue service](queue-service.md#principalとtoken)）。
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
- `ready --inherit`はまず`ready_inherit_read`でtaskの状態と最新のrunを読み、クライアントが引き継ぐheadを選んで`refs/dagq/runs/<run-id>`に書いてから（refが読んだ値のままのときだけ書いて確定する）、`ready_inherit`で記録を送る（ADR-t3102-3決定5、[ADR-t3294-1](../adr/2026-10-25-t3294-1-carry-over-by-hand-keeps-the-ref-before-the-record-and-puts-it-back-on-refusal.md)。`application::commands::planning::ready_inherit`、`application::inherit::keep_head`）。
  refを書けなければ記録を送らずに拒む。
  serviceは読んだ時のtaskの状態とrunの状態・検証済みのcommitを確定の中で照らし直し、食い違えば何も変えずに断り、クライアントはrefがまだ自分の書いたheadを持つときだけ書く前の値に戻す。
  答えが届かず記録したかが分からないときは、headを残す。
  次のrunは記録されたheadから始まり、refの今の値には拠らない（[triage](supervisor-lifecycle/triage.md)の「人の手での引き継ぎ」）。
  `ready_inherit_read`の答えはserviceのqueueのDBのpathも返し、クライアントはそのqueueのcontainerで最後のturnが切れたcontainerのrunのfacts-onlyのturnを走らせる（DBは開かない）。
  そのturnが作ったimageの記録は`ready_inherit_images`で送る

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
