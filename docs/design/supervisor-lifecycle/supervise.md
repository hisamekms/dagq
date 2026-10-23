---
id: design-supervisor-lifecycle-supervise
type: design
title: "`supervise`"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-t1639-1
  - adr-t1591-1
  - adr-t774-1
  - adr-0013
  - adr-0054
  - adr-0053
  - adr-0039
  - adr-0049
  - adr-0047
  - design-persistence
  - adr-0071
  - design-supervisor-lifecycle-waiting
  - design-queue-service
  - adr-t3102-1
  - adr-t3102-3
---

# `supervise`

> supervisorはlaunchdで常駐し、in-cmux modeでは新しく起動しない（[ADR-t1433-4](../../adr/2026-10-03-t1433-4-supervisor-resides-without-cmux.md)）。
> 前のbinaryが登録したin-cmuxのsupervisorは、引き継ぎと`down`だけを受ける。

## 目的

`dagq supervise`は常駐ループ（通常は`up`がlaunchdで起動する）で、依存が解けたtaskを上限まで同時に実行し、検証とreviewの後に1つずつ着地させ、失敗したrunを復旧jobに、止まったrunを再開に回す。
人の答えだけを待つrunはslotから外れ、slotの外で待つ（[人の答えを待つrun](waiting.md)、[ADR-0071](../../adr/0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)）。

## 流れ

```text
起動 ── 登録・heartbeat ──► pass: 読み直し → adopt → slotを渡す順（再開・復旧job・claim）
claim ──► provision ──► 監視（tick）──► sessionの終わり ──► 検証 ──► review ──► 着地
                                                     └──► needs_session / failed ──► 再開 / 復旧job
```

## 責務と境界

- ユースケースの本体はapplication層の`src/application/supervise/`で、contextごとのmoduleは`supervise.rs`、ループとsubmoduleの分担は`execution.rs`のmodule docが持つ（[ADR-0013](../../adr/0013-layered-architecture-and-type-function-style.md)の方針1と8）。
- `supervise(&Ports, &LoopSettings)`が起動とループを行い、外のものには`Ports`のport越しにだけ触れ、pathと子プロセスの環境は`Layout`として値で受ける。
- queueの操作はqueue serviceを通り、serviceがまだ答えない操作だけがDBを開く（下の「queue serviceの経路」）。
- `runtime::supervise`はadapterを組み立てて呼ぶ入口だけ。
- promptと依頼文は`src/application/prompt/`、askの登録は`application::ask`（通知はせずinboxのwatchが知らせる）。
- watcherの居ないinboxへの知らせは`inbox_nudge.rs`（[inboxへの知らせ](ask-push.md#supervisorによるinboxへの知らせ)）。
- 計画管理・観測と分析・host運用の状態は各contextのmodule（`planning.rs`・`observation.rs`・`host_ops.rs`）にあり、欄はそのmoduleの中からだけ見え、そのpassだけが変える。
  登録（`host_ops/handoff.rs`）も同じで、ループはhandoffと停止の段をそのmethodで進める。
- 実行と着地の状態は`execution/stages.rs`に工程ごとにあり、工程のsubmoduleとループが変える（欄の所有するcontextは`Supervisor`のdoc comment、[Architecture](../architecture.md)のC3）。

## 不変条件

- queue全体の排他は無く、同じqueueに複数のsupervisorが居てよい。
  1つのrunを進めるのはleaseを持つ1つのsupervisorの1つのslotだけである（[ADR-0054](../../adr/0054-run-lease-ownership-parallel-supervisors-and-recover.md)）。
- runの工程の移りは`stages.rs`の移す操作だけを通り、移るたびに記録する（[計測](../measurement.md#区間とタグ)）。
- slotの集まりは`SlotTable`だけが変える。
- 走っているrunは優先度の変更でも止めない。

## 起動とheartbeat

- 起動の順（登録の後に変更の印を記録することを含む）は`supervise`のdoc commentが持つ。
- 登録の前に、queue serviceに届くことを確かめ（届かなければ起動し）、登録のtokenのsupervisorとしてsign inする（下の「queue serviceの経路」）。
- 引き継ぎのexecで起動したときは新しく登録せず、同じtokenとPIDで登録を取り戻し、slotを組み立て直す（[Handoff](handoff.md#handoff)、`handoff.rs`のmodule doc）。
- heartbeatは`Heartbeat`と`HeartbeatPolicy`が持ち、登録と全leaseを1つの書き込みで更新する。
  busyで書けないときに止めずに書き直す範囲と、leaseがstaleになる前に自分から退く理由は`Heartbeat`と`HeartbeatPolicy`のdoc commentにある。
- heartbeatが止まったsupervisorはleaseと登録を残して終わり、stale になった後に他のsupervisorが引き継ぐ（`Heartbeat::check`）。
- slotの上限の値と出どころは`SlotLimits::resolve`が決め、登録と同じ書き込みで残す（[Run environment](run-environment.md)の`[supervisor]`）。

## passの始め

- 各passの検査と設定の読み直しの順と理由は`Supervisor::drive`のコメントが持ち、設定の変更は再起動なしに効く。
- sccacheの監視の入口は`application::supervise::host_ops::sccache`、serverをsandboxの外で持つ分担は[Run environment](run-environment.md#sccacheのserver)。
- 待ちの終わったrunは新しい仕事より先に（drain中も）slotへ戻す（`return_waiting_runs`、[人の答えを待つrun](waiting.md)）。
- workerのsessionはclaimでも再開でも非対話で、supervisorはworkerの画面を読まず打ち込まない（[非対話のworker](headless-worker.md)）。

## adopt

- 死んだsupervisorのrunを引き継ぐかは`adopt.rs`の`adoptable`と`resume_adoptable`が決め、引き継ぎは`adopt_stale_runs`が行う（[ADR-0039](../../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)の決定1〜4）。
- 同じrunを2つのsupervisorが取ろうとしても`adopt_run`が1つだけを通す。
- 自分がslotで進めているrunは、leaseが他のtokenに移っていても引き継がず、slotが外れた後のpassでだけ取る（理由は`Supervisor::in_slot`のdoc comment）。
- 引き継いだrunのslotはDBとrun fileから組み立て、待ちの時間は引き継いだ時刻から数え直さない（`Supervisor::resume`、review中のrunは`adopt_review`）。
- 引き継がないrunは`recover`と下の自動のrecoverに残る。

## slotを渡す順とclaim

- `needs_session`の再開、`failed` / `interrupted`の復旧job、readyのtaskのclaimは1つの候補の列になり、taskの効く優先度の降順にslotを受ける（`domain::slot_order`のmodule doc、[ADR-t1850-1](../../adr/2026-10-06-t1850-1-resumes-recovery-jobs-and-claims-share-one-line-by-effective-priority.md)）。
- 列を組むのは`Supervisor::fill_slots`で、種ごとの保留はその種を列から外すだけで、上位の候補のためにslotを空けて待たない。
- 効く優先度は`application::effective_priority`、基の優先度とgoal間のrankが無いことは[ADR-t1639-1](../../adr/2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)の決定2・4・8。
- 人とinboxは`set-priority`で`in_progress`のtaskの順も変えられる（[Authorization](../authorization.md)）。
- claimの規則の順は`domain::ClaimRank`（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定4）、区分の上限とagingは[claimを控える](claim-hold.md#実行区分の上限とaging)。
- claimの順は控えの記録から`candidates`・`graph`で再現できるので、順の入れ替えは記録しない。
- readyでも順の手前で落ちるtask（goalがdraft・依存がcompletedでない・goal依存がachievedで閉じていない）は控えの記録を持たないので、`candidates`の`excluded`と`status`の`claim_excluded`が理由ごとの件数と先頭のIDで出す（`application::claim_view::ClaimExclusions`）。
  claimはこの数を読まない。
- 候補を控える理由（衝突の多いファイル・動かせないworker・依存先を含むbuildの待ち）とclaimの間隔と着地待ちが空けた軽い枠は[claimを控える](claim-hold.md)。
- claimはbase commitに着地のbranchをclaimごとに読み直すので、`integrate`で依存が解けたtaskは先行taskを含むmainから始まる。
- claimの時点の属性は`domain::measure::ClaimAttributes`、claimできる候補の標本は`domain::kpi::CandidatesSample`のdoc comment。

## provisionと監視

- provisionは`Supervisor::provision`（`session.rs`）が行い、pathを先に記録してからdirectoryとworktreeを作り、wrapperをbackgroundで起動する（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)、[非対話のworker](headless-worker.md)）。
- wrapperの環境にqueue DBのpathは無く、workerはqueue serviceのtokenでつながる（[Queue service](../queue-service.md#クライアントモード)）。
- provisioningの失敗は環境要因とみなし、以後のclaimを止め、active runをdrainしてから非0で終わる。
- 各tickの先頭で、そのrunのleaseがまだ自分のものかを確かめ、他に移っていればDBに何も書かずslotを外す（`Supervisor::disown`）。
- leaseを失うとはlease行が無いか他のtokenに移ることで、heartbeatが古いことではないので、sleepの後も誰にも引き継がれていなければそのまま続ける（[ADR-0039](../../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)の決定7）。
- sessionの見張り（wrapperの登録とheartbeat・receipt・idle marker・終了）はrunごとの`SessionWatch`、終了の依頼は`exit.rs`（[receiptとsessionの終わり](headless-worker.md#receiptとsessionの終わり)）。
- runごとのload averageの区間は`domain::measure::LoadWindow`（引き継いだrunの区間は`ClaimState::loads`）、hostの負荷は[hostの負荷の連続の記録](host-metrics.md)。

## 検証とreviewと着地

- sessionの終わりでrunが`validating`か`failed`のどちらになるかは`domain::run::session_end_status`（[ADR-t1594-1](../../adr/2026-10-05-t1594-1-a-receipt-left-by-a-failed-headless-turn-goes-to-validation.md)）。
- receiptの検証はrunごとのthreadで動き、ループは待たずに他のrunを見続ける（[Validation](validation.md#validation)）。
- `awaiting_integration`のrunはleaseとslotとworkerのsessionを持ったままheadlessのreviewにかかる（[Review (supervisor)](review.md#review-supervisor)、ADR-0054の決定1・6）。
- `revise`は生きているsessionへ差し戻し、それ以外の結果ではsessionが終わってからwrapperを止める（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)の決定3、[runの復旧の「後片付け」](background-recovery-job.md#後片付け)）。
- 着地は着地スロットが空くのをleaseを持ったまま待つ（[integrate](integrate.md)）。

## leaseの解放

- leaseとslotは`awaiting_integration`や`failed`になった時点でなく、runがsupervisorの手を離れて次の担い手を待つ時点で返す（ADR-0054の決定6）。
- 返す時点の一覧は`release_lease`（`src/infrastructure/runtime_store/coordination.rs`）のdoc commentが持つ。
- drainするsupervisorが着地の保留でleaseを返したrunのように、lease行も閉じていないaskも無い`awaiting_integration`のrunで最後のreviewが着地を許すものは、fill passが拾い直してreviewをやり直さずに着地の列に戻す（`take_up_stranded_landings`、[ADR-t3049-1](../../adr/2026-10-21-t3049-1-supervisor-takes-up-a-passed-run-left-awaiting-integration-without-lease-or-ask.md））。
  拾うか人に残すかの判定は`domain::stranded_landing`で、人の判断を待つ理由（review以降のask・`integration_held`・runtimeのerrorでleaseを手放したこと・holdや再計画・cancelなど）のあるrunは今までどおり人の着地を待つrunとして出る。
  拾い直しの続きが失敗したrunもそのerrorで人に移り、次のpassは拾わない。
  leaseは`lease_stranded_landing`の1つのトランザクションがlease行の無いことと判定をその時点のeventで検査し直して作るので、adopt（staleなlease行の引き継ぎ）とは別の経路で、2つのsupervisorが同じrunを取らず、判定の後に人に保留されたrunも取らない。
  落とし穴: 人を待たせるために新しくleaseを返す経路は、その理由をreview以降のeventかqueueの状態に残さないと拾い直される。

## recoverとtriage

- 持ち主の居ないrunの自動のrecoverと復旧jobは[Triage (supervisor)](triage.md#triage-supervisor)（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定3）。
- 持ち主の居ない着地の着地スロットの解放と、worktreeに残ったプロセスの猶予は`domain::landing_release`のmodule doc。
- drainしているsupervisorも死んだ着地の解放だけは各回で行い、自分の着地を待つrunとdrainが止まらないようにする。
- 着地してよいrunが着地の列に並ぶか、もう一度reviewにかかるかは`RunHistory::landable`と`review_recovered_runs`。

## ループの終わり

- ループがいつ終わるかは`supervise`、ownerが居なくなったときの止まり方は`LoopSettings::owner`、結果の欄と登録の削除は`Supervisor::run_loop`のdoc comment。
- SIGINT/SIGTERMの1回目はclaimを止めてactive runを待ち（graceful drain）、2回目で即終了する（`install_stop_signal`）。
- 即終了したsupervisorのleaseはstaleになり、wrapperが生きているrunは次のpassで別のsupervisorが引き継ぐ（上の「adopt」）。
- 登録を消すのは自分のループの終わり、次の`up`・`down`の掃除と、同じqueueの生きているsupervisorによる死んだ登録の片付けだけで、どれも同じtransactionで`supervisor_stopped`を書く。
  `status` / `doctor` / `recover` / `integrate`は消さない（片付けの条件は[Persistence](../persistence.md#runtime-ownership)、`up` / `down`の掃除は[`up` / `down`](up-down.md#up--down)）。

## queue serviceの経路

supervisorはqueue DBを開かずに、claim・lease・runの状態とevent・登録・判断に使う読み取り（候補・依存・goal）・workerとsession wrapperとexecutorのtelemetryのtoken・landing recheckとresumeの記録・host運用のqueueのevent（`record_queue_event`）をqueue serviceのユースケースとして行う（[ADR-t3102-3](../../adr/2026-10-22-t3102-3-stage-5-outage-notice-and-use-cases-for-multi-step-control-operations.md)決定3〜5、principalは[ADR-t3102-1](../../adr/2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)）。
入口は`application::supervisor_service`のmodule docで、ループと各threadの`Queue`は`SupervisorQueue`、serviceの側はuse case `supervisor`の`answer`。

```text
pass の queue の操作 ── SupervisorQueue ─┬─ serviceが答える操作 ──► socket ──► QueueService::handle ──► queueの同じ操作（1つのtransaction）
                                         └─ まだ答えない操作（着地・jobの結果・計画・観測・askとhold・plannerとsessionとinboxの見張りの記録・evalのround）──► その時に開くDBの接続
```

- 1つの要求は1つのqueueの操作で、`op`でその名を名指し、引数はその操作の順に並べる。
  serviceの側は同じ操作を同じtransactionで行うので、leaseの検査と前提の再検査はその操作のまま（[SQLite persistence](../persistence.md#runtime-ownership)）。
  transactionを開く・閉じる操作や行を読み書きする操作は無い。
- serviceが答える操作は`dispatch`の一覧で、着地とjobのverdictの適用をserviceへ移すのは移行の順の残り（[ADR-t3102-2](../../adr/2026-10-22-t3102-2-only-the-queue-service-opens-the-db-schema-by-the-service-build-and-migration-order.md)決定6）。
- 結果は`Wire`の形で渡り、taskとgoalは`Serialize`（CLIのJSON）ではなく欄を落とさない形で渡る。
- principalは`Principal::supervisor`で、actor idは自分のeventのactorと同じ`supervisor:<pid>`、対象は登録のtoken。
  serviceは要求が名指すleaseのtokenが全てその登録のtokenであることを操作の前に確かめ、違えば何もせずに断るので、他のsupervisorのleaseでは書けない（例外はheartbeatの切れた他の登録の片付け）。
  `scheduler.supervise`の無いprincipalは`authorization_denied`で断られ、記録される。
- eventのactorを名指すのは着地のintegratorとして書くときと、jobのverdictを適用するとき（`requested_by`）だけで、名指せるactorは制御側だけ。
- tokenはhelloの後、登録の前に発行し（`ServiceCaller::sign_in`）、終わりで失効させる。
  1つのプロセスが複数のsupervisorを動かすときも別のtokenになるよう、tokenのfileは登録ごとに分ける（`Principal::credential_key`）。
  親のprincipalの依頼でserviceが発行する形（ADR-t3102-1決定2）はuserのprincipalが入るまで無く、今はsupervisorがserviceのdirに自分で書く。
- workerとsession wrapperのtokenとrunの対象のtelemetryのtokenの発行と失効は`ServiceTokens`がserviceに頼み、serviceはrunのものをrunのleaseを確かめた同じtransactionの中で発行する。
  tokenのfileの置き場とsocketのpathは、DBを読まずにfileから求める。
- 届かない呼び出しは`ServiceUnreachable`で、DBを開いて代わりに行わない（fail closed）。
  起動のときに届かないか別のbuildのものが答えれば、serviceを持つsupervisor（CLIの`supervise`は`--once`でも持つ）は起動か入れ替えをし、それでも届かなければ何もclaimせずに終わる（`reach_at_start`）。
  passの途中で届かなければ、そのpassを捨ててhost運用にserviceを見直させ、届くまでやり直し、その間は新しいclaimとjobを始めない（`Supervisor::run_loop`）。
  heartbeatは届かない間もbusyと同じくleaseがstaleになる前まで書き直し、それを過ぎれば自分から退く（`HeartbeatPolicy::retries`）。
- serviceの不在はqueueのディレクトリのDBの外のファイル（`domain::queue_service::OUTAGE_FILE`、形は`ServiceOutage`）に1つの不在につき1回書き、`host.toml`の`[push]`があれば1回送る（ADR-t3102-3決定1、`host_ops::queue_service`のmodule doc）。
  serviceが答えたら、その記録をserviceを通して`queue_service_down`（始まりと終わりの時刻を持つ）に残してファイルを消し、続く起動か再開のeventでattentionが閉じる。
  eventの時刻は戻った時刻で、不在の時刻はpayloadが持つ。
- libraryとして呼ぶsupervisor（testの）は`SuperviseOptions::service_route`の`InProcess`で、serviceのユースケースを同じプロセスで、要求と答えをsocketと同じ形にして通す。
  CLIの`supervise`は常にsocketを通る。

## CIの見張り（ADR-t1920-1）

[ADR-t1920-1](../../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)と[CI watch](ci-watch.md)。
CIの見張りを持つsupervisor（`SuperviseOptions::ci_watch`）だけが、passごとに`[ci_watch]`を読み直して確かめ（[CI watch](ci-watch.md)の「いつ確かめるか」）、答えを持たないか読む手段が無い間はclaim・新しい再開・着地を保留する（同じく「読む手段が無いとき」）。
着地を始めるか・待つか・drainが人に返すかの判断は、他の保留と合わせて`domain::landing_hold::judge`が持つ。
