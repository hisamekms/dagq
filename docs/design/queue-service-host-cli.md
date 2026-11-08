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
  - adr-t3745-1
  - design-queue-service
  - design-authorization
---

# Queue serviceのhostのCLI

人・inbox・plannerがhostで打つ`dagq`とqueue serviceの責務の境界。
workerとjobの経路は[Queue serviceのクライアントモード](queue-service.md#クライアントモード)、決定の理由は[ADR-t3102-1](../adr/2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)・[ADR-t3102-2](../adr/2026-10-22-t3102-2-only-the-queue-service-opens-the-db-schema-by-the-service-build-and-migration-order.md)・[ADR-t3102-3](../adr/2026-10-22-t3102-3-stage-5-outage-notice-and-use-cases-for-multi-step-control-operations.md)が持つ。

## 送る範囲

hostのCLIはserviceが答えるコマンドをsocketへ送り、queueのDBを開かない。
serviceが一度も動いていないqueueも同じで、DBが無ければ送る前に拒む。
送るコマンドと例外の境界を知りたいときは`main.rs`の`client_request`を読む。
CLIの全ての葉は送るものか例外のどれかに当たり、1つのコマンドの中で経路を混ぜない。

DBの無い所に作る初期化とservice自身の起動は[DBを開く経路](queue-service.md#dbを開く経路)で扱う。
既存のqueueの初期化・migration・診断もserviceを通る。
入れ替えの前に新しいbuildでschemaを確認してもDBへ戻らず、前のserviceが答えられないときの判定は入れ替えの起動に任せる（`domain::queue_service::schema_unanswered`、[install](supervisor-lifecycle/install.md#install)）。
wrapperとhookは[クライアントモード](queue-service.md#クライアントモード)の専用のprincipalで送り、プロセスを扱うコマンドは下の「制御側のコマンド」の経路を使う。

CLIが自分で拒んだものも、DBを開かずに拒否の記録だけを送る。
届かなくても拒否は拒否のままで、記録の送り方は`infrastructure::denials`、受け取れる拒否の条件は`application::queue_service::record_own_denial`を読む。

repositoryの束縛はクライアントが解決したものをserviceが照らす。
宛てのDBはlinkの先を解決したdirのsocketで扱い、同じdirの別のDBも何も開かず拒む。
要求の宛てと束縛の入口は`domain::queue_service::ServiceRequest`、読み取り専用の接続と拒否の記録の境界は`application::queue_service::ServiceBackend`。

watchの待ちとpushはクライアント側に残る（`application::watch::watch_through`）。
pushが名指すrepositoryはqueueの束縛から決めるので、束縛のあるqueueではwatchを打つ場所に依らない（`WatchLook::binding`）。
束縛の無いqueueでは打つ場所のrepositoryの名前になる。
serviceは読み取りでGitもprogramも起動しない。
図を描くのはhostのCLIで、台帳の統計には両側ともGitを使わない（`application::stats`）。
supervisorの全eventの読み取りを拒む境界は`application::supervisor_service::SupervisorQueue`を読む。

serviceに届かなくても、serviceとbrokerのstatusはDBを開かずに答える。
答えの組み立てはそれぞれ`application::queue_service::host_status`と`application::queue_broker::host_status`、停止の記録と次の手は[落ちたときの知らせ](queue-service.md#落ちたときの知らせadr-t3102-3決定12)を読む。

## 対話と操作

対話と操作の入口は`application::queue_commands::QueueCommand`。
serviceとCLIのDBの経路は同じapplicationを通るので、答えと認可はCLIと同じになる。
書き込みは1つのコマンドにつき1つのtransactionでserviceの中で終わり、eventのactorは呼び出し元のprincipal。
Gitとprogramはクライアントがユースケースの外で走らせる。
認可をGateで通すかapplicationで通すかと、複数回の要求にまたがる読み取りの照合は`application::queue_commands`のdoc commentを読む。

paramsのpathはserviceのdirectoryに依らない絶対pathで送る。
`request add`と`planner request`は、policyに権限の無いroleのfileとstdinを読まずにserviceへ送り、principalで拒む。
logを追うことと図の描画・出力はクライアントの責務で、境界は`application::queue_commands`を読む。

reportの入力・描画・出力の受け渡しは`application::report`を読む。
serviceがreportを書き消すのはqueueのreportsの下だけで、利用者が名指した出力先へ書くのはクライアントの権限。
serviceが設定を読むのは束縛からGitなしで決めたmain checkoutだけで、別のcheckoutの中は読まずに断る。
SVGは画像として埋め込むので、scriptも外への読み取りも動かず、styleもpageに及ばない（`domain::kpi::report::svg`）。

releaseの成果を読むのと保存はクライアントが担い、serviceは読んだholdと版を確定で照らす（`application::commands::holds::prepare_release`）。
containerのfacts-onlyのturnが要る成果はhostのCLIに任せる。
queueのdirを知らないクライアントモードはそのturnを起こさない。

rebindのクライアントとserviceの境界は`application::rebind`を読む。
生きたsupervisorが居ると拒まれる。
repositoryの移動時の操作手順は[pluginのlocate reference](../../plugins/claude-dagq/skills/dagq/reference/locate.md)を読む。

## 制御側のコマンド

Git・検証・agentや自分のプロセスを扱う制御側のコマンドは、queueの読み書きをstepとしてserviceへ送り、DBを開かない。
hostのCLIもクライアントモードも同じ経路だが、brokerを通る宛てからは送らない（`ControlCaller`）。
コマンドごとの起動・停止・送り直しの扱いは`application::supervisor_service::control`、stepの定義は`domain::queue_service::ControlParams`を読む。
入れ替えの口のserviceは引き継ぎ前にrepositoryの束縛を確かめ、違えば入れ替えもmigrationもしない（`ServeOptions::binding`）。

認可はCLIと同じで、最初のstepで拒まれるprincipalは何も始めない。
queueの読み取りに加え、書けるのはコマンド自身の記録だけで、範囲の入口は`control::scope`。
呼び出し元は依頼者を名乗れず、supervisorだけの資源・負荷・台帳の操作も使えない。
upの引き継ぎで渡せるbinaryはsocketのpeerの実行ファイルだけで、他のpathは拒否として記録する（`control::handoff_refusal`）。
更新のjobは起動したsupervisorのprincipalで送る（[Auto-update](supervisor-lifecycle/auto-update.md#queueへの経路)）。

着地の承認はserviceの時計とプロセスでleaseを確かめ、記録と同じtransactionで再検査する（`control::APPROVE_INTEGRATION`）。
着地のslotとtokenは同じtransactionで発行し、接続から確かめたコマンド自身のプロセスだけが取得・失効できる。
着地のeventはintegratorのもので、依頼者は呼び出し元になる（[ADR-t3102-1](../adr/2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)決定2）。
telemetryはコマンド自身の対象に限る（`control::own_target`）。
共有するmachineの他のqueueのrunもserviceを通して読み、届かないqueueを無いrunとは数えず、DBへ戻らない（`podman_machine::OwnerRuns`）。

結果の記録はserviceが取るまで同じidentityで送り直す。
呼び出し元のprincipalで送れるのはそのプロセスだけなので、期限の後に記録を残して終わることはしない（`control::resending`）。
記録でないstepが届かないときは新しいstepを始めずに終わる。
起動・停止のコマンドには送り直しの例外があり、停止前の記録の失敗は警告して止めるが、serviceが呼び出し元を拒んだときは止めない（`application::queue_service::stop_record`）。
brokerの起動・停止の例外は`application::queue_broker::start_by_hand`と同moduleの停止のdoc commentを読む。

## principal

serviceはtokenもroleも無い要求を、同じOSのuserのsocket peerならuserとして扱う。
roleを名乗るだけの要求は認めず、tokenが無ければ拒む（`QueueService::authenticate`）。
inboxとplannerの次の手は`domain::queue_service::missing_token_next`。
互換の決定と開き直しの理由は[ADR-t3105-1](../adr/2026-10-23-t3105-1-reopen-the-inbox-once-after-the-host-cli-reaches-the-queue-service.md)、運用の手順はpluginの`dagq-recover`の`reference/update.md`を読む。

inboxとplannerのtokenはserviceのdirのfileから送り、別のqueueへは送らない（`infrastructure::queue_service::issued_by`）。
sessionが続くかぎり自動更新とserviceの起動し直しをまたいで使える。
inboxを開き直したときは前のsessionが新しいtokenを読めず、同時に開いても残るのは1つだけ。
inboxはserviceが答えなければ開かない。
発行・sessionごとのfile・掃除は`infrastructure::queue_service::issue`、plannerの発行側は`HostActorExecutor`を読む。
発行は親の権限と対象の前提を同じtransactionで確かめる（[Queue service](queue-service.md#principalとtoken)）。
APIの互換は[API versionと互換](queue-service.md#api-versionと互換)、要求の版を選ぶ入口は`domain::queue_service::request_version`。

## 計画のユースケース

計画の書き込みとmarkの入口は`application::queue_writes::QueueWrite`。
CLIとserviceは同じapplicationの呼び出しで答え、認可と拒否の記録はprincipalのactorで行い、書き込みは1つのtransactionで終わる。
proposalの持ち主は環境の名乗りでなくprincipalから決める（`queue_writes::principal_owner`）。

人の手での引き継ぎの入口は`application::commands::planning::ready_inherit`、refを先に固定する約束は[ADR-t3294-1](../adr/2026-10-25-t3294-1-carry-over-by-hand-keeps-the-ref-before-the-record-and-puts-it-back-on-refusal.md)が持つ。
次のrunは記録されたheadから始まり、refの今の値には拠らない（[triageの人の手での引き継ぎ](supervisor-lifecycle/triage.md#人の手での引き継ぎ)）。

## 届かないとき

statusとwatchは届かないとき空の答えに替えず、DBへ戻らずに失敗する。
停止の記録があれば次の手とともにerrorへ添える（[落ちたときの知らせ](queue-service.md#落ちたときの知らせadr-t3102-3決定12)）。
watchは同じ停止をroleごとに1回だけ返し、その後は停止を待つので、停止中の待ちを通常の空の答えと混同しない（`application::watch::look_through`）。
待ちの時間の境界は`application::watch::look_limit`、statusの接続と応答の期限は`infrastructure::queue_service::Client::look_within`を読む。
