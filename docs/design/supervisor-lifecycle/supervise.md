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
---

# `supervise`

> supervisorはcmuxを呼ばない（[ADR-t1433-1](../../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)）。
> supervisorはlaunchdで常駐し、in-cmux modeでは新しく起動しない（[ADR-t1433-4](../../adr/2026-10-03-t1433-4-supervisor-resides-without-cmux.md)）。
> 登録済みのin-cmuxのsupervisorは、そのworkspaceに居たまま引き継ぎと`down`だけを受ける。

ユースケースの本体はapplication層の`src/application/supervise/`にある（submoduleの分担は`mod.rs`のmodule doc、watcherの居ないinboxへの知らせが`inbox_nudge.rs`（[inboxへの知らせ](ask-push.md#supervisorによるinboxへの知らせ)）。review・triageのpromptとsessionに渡す依頼文は`src/application/prompt/`）（[ADR-0013](../../adr/0013-layered-architecture-and-type-function-style.md)の方針1と8）。
`supervise(&Ports, &LoopSettings)`が下の起動時の1〜4とループを行い、外のものにはすべて`Ports`のport越しに触れ、pathと子プロセスの環境は`Layout`として値で受ける（portの分担は`mod.rs`のmodule docと`Ports`・`Layout`のdoc comment）。
`runtime::supervise`はadapterを組み立てて呼ぶ入口だけ。
askの登録は`application::ask`（通知はしない。inboxのwatchが知らせる）、doctor・`recover`と同じrunのhealth判定は`application::health`、workspaceの名前付けとshellの引用は`application::naming`にある。

計画管理・観測と分析・host運用は`contexts.rs`（登録は`handoff.rs`）にあり、そのpassだけが変える。
実行と着地は`stages.rs`に工程ごとにあり、工程のsubmoduleとループが変える。
runの工程の移りは`stages.rs`の移す操作だけを通り、移るたびに`run_phase_changed`を書く（[計測](../measurement.md#区間とタグ)）。
slotの集まりは`SlotTable`だけが変え、残りは`Supervisor`が持つ（欄の所有するcontextは`Supervisor`のdoc comment、[Architecture](../architecture.md)のC3）。

`dagq supervise`はrepository内で実行する常駐ループ（通常は`up`がlaunchdで起動する）で、依存が解けたtaskを上限（`parallel`）まで同時に実行する。
人の答えだけを待つrunはslotから外れ、`max_waiting`までslotの外で待つ（[人の答えを待つrun](waiting.md)、[ADR-0071](../../adr/0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)）。
「active runが上限未満」「空いたslot」は、待ちと戻り待ちのrunを除いたslot（`used_slots()`）で数える。
queueはcwdから解決し（[persistence](../persistence.md)のQueue location）、repositoryのcheckoutもcwdを使う。

起動時:

1. DBのpathを正規化し、checkoutのroot、Git common directoryを取得する。DBはworktree外か、common directory配下に置く（ユーザーDIRのqueueは常に満たす）。worktreeの作成元は`repo_path`に記録したcheckout。
2. Claudeのpreflightを行う。
3. queueをrepositoryに束縛する（`bind_repository`）。別repositoryに束縛済みなら開始しない。queue全体の排他はなく、同じqueueに別のsupervisorがいても構わない。
4. supervisorプロセスのtoken（UUID）を作り、`supervisors`表に自分を登録し（`register_supervisor`）、引き継ぎを受けられる印を付ける（`accept_handoff`）。
   引き継ぎのexecで起動されたときは新しく登録せず、そのtokenの登録を同じPIDのまま取り戻し（`resume_registration`）、そのtokenのleaseを持つrunのslotを組み立て直す（[Handoff](handoff.md#handoff)）。
   runを1つも持たない常駐supervisorも、この登録で`status`/`doctor`に並ぶ。
   続けて別スレッドで`HEARTBEAT_INTERVAL`ごとにそのtokenの登録と全leaseのheartbeatを1トランザクションで更新する（`heartbeat(token)`）。
   heartbeatの書き込みが他の接続の書き込みのlockでSQLiteのbusy / locked（busy timeoutの`QUEUE_BUSY_TIMEOUT`を待っても取れない）で失敗したときは止めず、次の間隔で書き直す。
   heartbeatのthreadは失敗のたびにWARNのlogを書き、書き直せたら失敗の回数と時間をINFOのlogと`supervisor_heartbeat_retried`のqueue eventに残す。
   busyのまま書き直さないのは、最後に書けた時刻からの時間に次の間隔と次の書き込みの待ちを足すと、leaseがstaleと見なされる`HEARTBEAT_TIMEOUT_SECS`に届くときで、そこで諦めれば最後に書いたheartbeatがstaleになる前に止まるので、他のsupervisorが引き継ぐ前に自分から退く（`HeartbeatPolicy::retries`）。
   busy / locked でない失敗（書き込みの拒否、I/O errorなど）はその場で止める。
   書き込みが通っても登録の行が無かったとき（PIDが死んだかstaleと見なされ`up`か強制の`down`で消された）はleaseを失ったとして止める（登録を持たない`integrate`のheartbeatはこの判定をしない）。
   runのlease行が他のtokenに移ったことは、下の7の各tickの確認がrunごとに見て、そのrunだけをslotから外す。
   止めるときはループで検知し、全runに`runtime_error`（`lease_lost`。`Heartbeat::check`）を記録してleaseと登録を残したまま終了する（プロセス終了後にstaleになる）。
   `parallel`・`max_waiting`・`runtime_planners`の値と出どころは`SlotLimits::resolve`が決め、登録の行を作るのと同じトランザクションで書く（引き継ぎでは取り戻しの直後に書き直す（`set_slot_limits`）。[Run environment](run-environment.md)の`[supervisor]`）。
   登録の後、`stall_config_loaded`に続けて変更の印`supervisor_started`を1件記録する（`domain::marks::SUPERVISOR_STARTED`）。
   停止要求（SIGINT/SIGTERM）に最初に気づいたpassでは、drainに入ったことをqueueのevent `supervisor_draining`で1回だけ記録する（[Handoff](handoff.md#handoff)の停止要求の記録と引き継ぎの待ち）。
   loopが終わって登録を消すとき（execするときを除く）は`supervisor_stopped`を記録する。
   ループの各passでは、環境の検査に続けてsccacheの監視と再起動を行う。
   監視と待ちの入口は`application::supervise::sccache`、serverをsandboxの外で持つ分担と観測の限界は[Run environment](run-environment.md#sccacheのserver)が持つ。
   続けてmain checkoutの`[run.env]`の正規化したhashがqueueの最新の`run_env_changed`と違えば`run_env_changed`を記録する（`Verifier::run_env_table`、hashの鍵はqueueのsaltの`Verifier::run_env_salt`。
   [変更の印](commands.md)、ADR-0051の決定10・11）。
   続けてmain checkoutの`[conflicts]`を読み直す（`Supervisor::reread_conflicts`、ADR-0080・ADR-t774-1・ADR-t775-1）。
   続けて`[supervisor]`を読み直し（flagで明示した値はそのまま）、値が変われば以後のclaimと待ちとruntimeのplannerを立てる判定に使い、登録に書いて`supervisor_config_changed`を記録する（[Run environment](run-environment.md)の`[supervisor]`）。
   続けて`[provider_fallback]`を読み直し、`workers`が変われば以後のclaimの経路とturnの壁の判定に、`jobs`が変われば以後のheadlessのjobの行き先と失敗の後の扱いに使う（[Run environment](run-environment.md)の`[provider_fallback]`、[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）。
   続けて`[fresh_session]`を読み直し、変わった閾値を新しいsessionの判定（`next_session`）に使う（[Run environment](run-environment.md)）。
   続けて`[ci_watch]`を読み直し、CIの確かめを受け取り・始め（下の「CIの見張り」）、間隔が過ぎていれば今の判定を記録する（[計測](../measurement.md#今の判定の記録)）。
   続けてmainの履歴の動いた分と到達点を記録する（[mainの履歴の記録](../main-history.md)。毎passではGitを読まない）。
   続けて間隔が過ぎていれば、前の周回の後に届いた入力から台帳をjobのthreadで書く（[計測](../measurement.md#台帳を作る係)）。

ループ: 各passの先頭で、待ちの終わったrunを新しい仕事より先にslotへ戻し（drain中も）、各tickの先頭で人の答えを待つrunを待ちに移す（[人の答えを待つrun](waiting.md)）。
待ちのrunのslotは下の監視の代わりにsessionへ何も送らない見張りだけを受ける。
workerのsessionはclaimでもresumeでも非対話で、supervisorは依頼をrun directoryに書き、workerの画面を読まず打ち込まない（[非対話のworker](headless-worker.md)）。

5. **adopt**（[ADR-0039](../../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)の決定1〜4）: active runが上限未満なら（イベントの上で人の答えを待っているrunは、待ちの数が`max_waiting`未満なら空きを要さずに待ちとして。[人の答えを待つrun](waiting.md)）、claimの前に、他のtokenのleaseを持つ`running` / `validating` / `awaiting_integration` / `needs_session`のrunを`runs_leased_by_others`で読み、leaseがstale（pidが死んでいるかheartbeatが`HEARTBEAT_TIMEOUT_SECS`より古い）で、wrapperが生きていてheartbeatが`HEARTBEAT_TIMEOUT_SECS`以内か`exited_at`が記録済みのもの、または`awaiting_integration`のもの（wrapperを問わない。reviewはheadlessでsessionを要らず、wrapperが終了を記録せずに死んだsessionは終わったsessionとして扱う）、またはresumeの途中の`needs_session`のもの（最後のresumeのイベントが`resume_started`でその試行の`workspace_created`が記録済み、wrapperが終了を記録しておらずプロセスが生きている。heartbeatは問わない。[`needs_session`](needs-session.md#supervisorの入れ替わり)）を`adopt_run`で引き継ぐ。
   `adopt_run`は`BEGIN IMMEDIATE`の中でstatusとstaleを再検査し、lease行と`task_runs.supervisor_token`を自分のものにして`run_adopted`を書く（同じrunを2つのsupervisorが取ろうとしても1つしか通らない。負けた方は何もしない）。
   自分がまだslotで進めているrunは、leaseが他のtokenに変わってstaleでも引き継がない（resumeとtriageも取らない）: そのslotは7で外れ、その後のpassでだけ他のrunと同じく引き継ぐ（同じrunを2つのslotで進めない。passはslotを進める7より前にadoptするので、前のpassの7の後にleaseが動くと、leaseを失ったrunを引き継ぎ直して2つのslotで進めてしまう）。
   引き継いだrunのslotはDBから組み立てる: pathは`run_planned`のもの、`receipt_seen`はreceiptファイルと`receipt_observed`イベントの有無、`exit_requested`イベントがあれば終了の依頼を書き直さず、exit timeoutは最後の`exit_requested`から数え、receiptの後の待ち（`resume_timeout`）は最後の`receipt_observed`の時刻から数える（引き継いだ時刻から数え直さない。resumeの途中のrunと`handoff.json`の`exit`の段の終了の依頼も同じくその記録から数える。reviewの後に引き継いだrunの終了の依頼も、reviewの錨か`review_started`より後の記録から数える）。
   記録の時刻は`RunFiles`の壁時計との差を今の`Instant`から引いて戻す、wrapperの登録待ちは持たない。
   `validating`のrunは9の検証をはじめから行う。
   `claimed` / `starting`（wrapperの登録にclaimしたtokenが要る）、leaseのないrun（abandon済み・`recover`済み）、`integrating`、wrapperが死んでいるか黙っている`running` / `validating`のrunは引き継がず、`recover`に残す（leaseのpidが死んでいてprocessが止まっていれば12の自動recoverが拾う。`integrating`のrunは12が着地スロットを解放する）。
   引き継ぎはlogに1行で残す。
   引き継ぐかとreview中のrunの続け方は`adopt.rs`の副作用のない関数が観測と型付きの記録から決める。
   **slotを渡す順**（[ADR-t1850-1](../../adr/2026-10-06-t1850-1-resumes-recovery-jobs-and-claims-share-one-line-by-effective-priority.md)）: `needs_session`のrunの再開（[`needs_session`](needs-session.md)）、`failed` / `interrupted`のrunの復旧job（12）、readyのtaskのclaim（下）は1つの候補の列になり、候補のtaskの効く優先度の降順にslotを受ける（同じ効く優先度の中の順は`domain::slot_order`のmodule doc）。
   効く優先度の求め方は下のclaim順と同じで、`in_progress`のtaskにも同じく当てはめる。
   試行を使い切った再開の`exhaust`と、終わったsessionの`stuck_exit`のaskのcloseは、再開の候補を集めるときにslotに関わらず行い、`exhaust`の引き継ぐretryで`ready`に戻ったtaskも同じpassでclaimの候補になる。
   claimは1つの効く優先度の候補をまとめて下のclaimのloop（その優先度以上の候補だけを取る）で取り、空きが無いか、claimの間隔・着地のbranch・provisioningの失敗でそのpassのclaimを止めたら、以降のclaimを飛ばす。
   再開と復旧jobは通常のslotだけを使い（軽い枠はclaimだけ）、空きが無ければその候補を飛ばす。
   種ごとの保留（再開: 着地のbranch・`[run.env]`のprogram・`[ci_watch]`の読む手段・Claudeを使えない・containerで動かせないworker・生きたleaseかwrapper、復旧job: 行き先の待ち（`Supervisor::recovery_route`）・着地のbranch・`wait`、claim: 下の各保留）は、その種の候補を列から外すだけで、上位の候補のためにslotを空けて待たない。
   走っているrunは止めず、自動のagingは無い。
   人とinboxは`set-priority`で`in_progress`のtaskの順も変えられる（[Authorization](../authorization.md)）。

   **claim**: 続けて（上の列の順で）、`graph_input`から`dependency_graph`でcandidatesを規則の順（`domain::ClaimRank`、[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定4）に並べる。
    効く優先度は`application::effective_priority`、基の優先度とgoal間のrankが無いことは[ADR-t1639-1](../../adr/2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)の決定2・4・8が持つ。
    goalの優先度の変更はpassごとの読み直しで次のclaimから効き（`in_progress`のtaskでは次の再開・復旧jobの順に効く。ADR-t1850-1）、走っているrunは止めない。
    claimの順は、規則の順から下の控えとSpikeの上限で候補を外した残りに、同じ効く優先度の中で区分のagingを当てた順で（[実行区分の上限とaging](claim-defer.md#実行区分の上限とaging)）、`candidates`と`graph`はsupervisorの控えの記録から同じ順を出す。
    空でなければ`refs/heads/<branch>`を読み直してbase commitにし、`claim_for_supervisor_holding`でその順の先頭からまだclaim可能なtaskを取り（上限に達した区分は取らない）、run・`supervisor_token`・lease行を1トランザクションで作る。
    順序は`candidates`・`graph`で再現できるので`claim_reordered`は記録しない。
    ただし、衝突の多いファイル（`conflict_hotspots`のalert）で進行中のrunと予想するファイルが重なる候補は、interruptのものと上限を過ぎたものを除いてそのpassの順から外し、控えた理由を`claim_deferred`に書く（[claimを控える（衝突の多いファイル）](claim-defer.md)、ADR-0080）。
    supervisorのadapterの表に無いworker（taskの`provider`と`worker_mode`の組）と、containerで動かせないworker（[Container runtime](../container-runtime.md#fail-closedの境界)）の候補も順から外し、`claim_deferred`に書く（[workerを動かせないtask](claim-defer.md#workerを動かせないtask)、ADR-t813-2）。
    `wait_for_build`を宣言した候補は、supervisorのbuild識別子のcommitが依存先の着地commitを全て含むまで（interruptでも）順から外し、`claim_deferred`に書く（[依存先の着地を含むbuildを待つtask](claim-defer.md#依存先の着地を含むbuildを待つtask)、ADR-t1632-1）。
    `run_claimed`のpayloadには、遷移とclaimの時点の属性を書く（欄は`domain::measure::ClaimAttributes`のdoc comment、buildは[Build identifier](build-identifier.md)、`rustc`は[Source repository](source-repository.md)、claimの間隔は[claimを控える](claim-hold.md#claimの間隔)）。
    loadの保留が有効なsupervisorは、claimの間隔のあいだ次の新しいclaimをせず、1つのpassで新しくclaimするのは1本になる（同じ節）。
    claimのloopは使っているslot（`used_slots()`。着地の順番を待つだけのrunも含む）が`parallel`未満のあいだ順の先頭からclaimし、それに届いても、着地の順番を待つだけのrun（`parallel`件まで）を除いた数が`parallel`未満で`[supervisor] light_changes`が空でなく戻り待ちのrunが無ければ、順から`light_changes`のchangeでpathsを宣言したtaskだけを残してclaimする（priorityに関わらず軽くないtaskは残さない。[claimを控える](claim-hold.md#着地待ちが空けた軽い枠)、[ADR-t1591-1](../../adr/2026-10-04-t1591-1-landing-queue-leaves-room-for-light-changes.md)）。
    path・pidは載せない（ADR-0032の分類でドメインevent）。
    手の`claim`（supervisorの無いclaim）の`run_claimed`はこれらを持たない。
    `integrate`で依存が解けたtaskは次のループで、先行taskを含む`main`から始まる。
    fill passは始めと終わりにslotの埋まりの変化を`slots_full_*`に書く（[計測](../measurement.md)の「claimの前」）。
   **candidatesの標本**（ADR-0051の決定3）: claimを止めておらず停止要求も無いpassでは、このfill pass（claimを控えたpassも含む）の後に標本（`domain::kpi::CandidatesSample`）を数え、このプロセスが前に記録した標本と違えば`candidates_sampled`をqueueのeventとして1件記録する。
   読み書きの失敗はlogに出すだけで、次のpassでやり直す。
   `kpi`の`candidates`がこれを読む（[`kpi`](commands.md)）。
6. **provision**: run管理領域（DBと同じdirの`runs/<run-id>/`）のpath、branch `dagq/<run-id>`、worktree（`runs/<run-id>/worktree`）、receipt、logのpathを`run_planned`として先にDBへ保存し、ディレクトリ、`prompt.txt`、runtimeバイナリのスナップショット`runner`、worktreeを作り、session wrapperを起動する（起動の仕方は`ActorProgram::RunSession`、環境変数は`domain::actor`のmodule doc。[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)、[非対話のworker](headless-worker.md)）。
   返ったhandleを`workspace_created`として`task_runs.workspace_id`に保存し、`wrapper_launched`を記録する。
   wrapperの環境にqueue DBのpathは無く、起動の前にworkerのqueue serviceのtokenを発行し、wrapperがagentの起動でそのfileとserviceのsocketを渡す（[Queue service](../queue-service.md#クライアントモード)）。
   provisioningの失敗は環境要因とみなし、そのrunをabandonした上で以後のclaimを止め、active runをdrainしてから非0で終了する。
   `prompt.txt`の内容は下記[Prompt](prompt.md#workerのprompt)。
7. **監視**: 各tickの先頭で、そのrunのlease行がまだ自分のtokenであることを確認する。
   なければ（別のsupervisorが引き継いだ、または`recover`された）そのrunをslotから外し、DBには何も書かず結果の`errors`に載せる。
   tickの途中でleaseを失ってlease付きの書き込みが失敗した場合も同じで、abandonしない（`last_error`を書かない）。
   「leaseを失った」はlease行が無いか他のtokenに変わったことで、heartbeatが古いことではない: lease付きの書き込みは同じトランザクションの中で自分のtokenのlease行の`heartbeat_at`を条件付きのUPDATEで更新してから書くので、ホストのsleepやsupervisorの一時停止でheartbeatが`HEARTBEAT_TIMEOUT_SECS`より古くなっていても、誰にも引き継がれていなければそのrunをそのまま続ける（[ADR-0039](../../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)の決定7。引き継がれていれば更新が0行になり、上のとおり退く）。
   検証threadが動いていればそのまま終わらせる（結果は記録されない。引き継いだ側が検証をやり直す）。
   続けてrunごとの`SessionWatch`が、wrapperの登録（`SessionWrappers::registration_timeout`以内）、wrapper heartbeat（`HEARTBEAT_TIMEOUT_SECS`以内）、receiptファイルの出現、idle marker、wrapperの終了を確認する。
   receiptの出現は`receipt_observed`として記録するだけで、セッション終了とは別に扱う。
   supervisorは各tickの先頭でload averageを1回読み、slotにあるrunごとに今の区間のsampleに足す（claimのsampleから始まる。`domain::measure::LoadWindow`）。
   区間の終わりのイベントにその平均と最大（`domain::measure::LoadSummary`）を載せて次の区間を空から始める: `receipt_observed`がwork（claim→receipt）、`validation_finished`がvalidate（receipt→検証の完了）の区間。
   引き継いだrunは引き継いでからのsampleだけを持つ。
   これとは別に、hostの負荷はpassごとに間隔を見てjobのthreadが取り、eventにせずNodeSampleStoreとCSV（写し）に書く（[hostの負荷の連続の記録](host-metrics.md)）。
   receipt観測後にidle markerがreceiptより新しければ`session_idle_observed`を記録し、`exit_requested`を記録してから終了の依頼のfileを一度だけ書く（下記）。
   wrapperが`exited_at`を記録済みのsession（自分で終わった、引き継ぐ前に終わっていた）には終了を要求しない。
8. wrapper終了後に、その時点でrunのreceiptの有無を読んでから（receiptとwrapperの終了を見た順に依らない）、`supervision_finished`でrunを終了コード0なら`validating`、非0でもreceiptがあれば`validating`（receiptを書いた後に失敗したturn。`last_error`は書かず、turnの失敗は`turn_finished`に残る。[ADR-t1594-1](../../adr/2026-10-05-t1594-1-a-receipt-left-by-a-failed-headless-turn-goes-to-validation.md)）、非0でreceiptが無ければ`failed`にする（判定は`domain::run::session_end_status`）。
   `failed`のときは同じトランザクションで`last_error`に終了コードを書き、`show`だけで理由が分かるようにする。
   Taskは`in_progress`のまま残す。
9. `validating`のrunはreceipt検証（下記）をrunごとのthread（専用SQLite接続）で行い、ループは完了を待ちながら他のrunを監視し続ける。完了したら`validation_finished`でrunを`awaiting_integration`、`needs_session`（`evidence_missing` / `scope_violation`など。[Validation](validation.md#validation)）、`failed`のどれかにする。workerのsessionはここでは閉じない。
   `already_satisfied`の受理はrunを`validating`に残す（[Validation](validation.md#already_satisfiedの検証)）。
10. **reviewと着地**（[ADR-0054](../../adr/0054-run-lease-ownership-parallel-supervisors-and-recover.md)の決定1・6、[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定2）: `awaiting_integration`になったrunは、leaseとslotを持ったまま、workerのsessionを開いたままheadlessのreviewにかける（[Review (supervisor)](review.md#review-supervisor)。`integrate`で承認済みのrunはreviewを飛ばして着地へ進む）。
    `revise`は生きているsessionへ差し戻し、書き直したreceiptで9からやり直す（passでもmainとの衝突の事前判定が衝突を見つければ、同じく差し戻す）。
    pass・concern・reviewの失敗・上限を超えたreviseや衝突の依頼のaskでは、sessionに終了を依頼し、sessionが終わってから、まだ動いていればwrapperをhandleの`stop_background`（どう終わったかは`wrapper_stopped`。[非対話のworker](headless-worker.md)の「停止の記録」）で止め（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)の決定3）、`workspace_closed`を記録する。
    ADR-t1433-3より前にworkspaceで開いたsessionは閉じず、人に残す（cmuxを呼ばない）。
    worktreeとbranchは着地まで残す（着地後に`integrate`が削除する）。
    止められなければ`cleanup_failed`イベントと`last_error`に記録し、runのstatusは変えず、`workspace_closed_at`はnullのままにする（後の掃除が止める。[Cleanup and recovery](cleanup-and-recovery.md#cleanup-and-recovery)）。
    passなら着地スロットが空くのをleaseを持ったまま待って着地する。
    `needs_session`になったrunも終了の依頼の後にwrapperを止める（resumeは自分のwrapperを起動する）。
    `failed`のrunは終了を依頼し、worktreeは調査のため残し、まだ動いているwrapperはtriageの後に止める。
11. **leaseの解放**（ADR-0054の決定6）: `awaiting_integration`になった時点でも`failed`になった時点でもleaseとslotは返さず、runがsupervisorの手を離れて次の担い手を待つ時点で返す（`lease_released`をleaseの削除と同じトランザクションで記録する）。
    主なものは、着地が終わって`integrated`になったとき、runを`needs_session`にしたとき（終了の依頼とwrapperの停止の後。resumeはleaseを取り直して行う）、`failed`のrunの終了の依頼の後（sessionが非0で終わったrunはすぐ）、`failed` / `interrupted`のrunのtriage（leaseを取り直して行う）が終わったか失敗したとき、concern・reviewの失敗・上限を超えたreviseや衝突の依頼で`approve_landing`のaskを開いたときと、開いたままの`approve_landing`のaskの答えを待つ`awaiting_integration`のrunが検証をやり直したとき（runは`awaiting_integration`のままleaseの無い状態で答えを待つ。[Landing recheck](landing-recheck.md)）、drainやexecの引き継ぎ（[Handoff](handoff.md#handoff)）で持ち続けられないrunを手放すとき、abandon（`lease_released`は書かず`runtime_error`で記録する）である。
12. **recoverとtriage**（[Triage (supervisor)](triage.md#triage-supervisor)、ADR-0054の決定11、[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定3）: fill passの中で、adoptの後に、`claimed` / `starting` / `running` / `validating`のrunのうち、leaseが無いか、leaseのpidが死んでいて5のadoptの対象にならないもので、processが1つも生きていないもの（`doctor`の`blockers`が空）を`recover`と同じ手順で`interrupted`にする（slotは使わない）。
    持ち主の居ない`integrating`のrun（leaseの行が無いか、leaseがstaleでpidが死んでいる。判定は`landing_release::nobody_lands`）は、`doctor`の`blockers`が空で、worktreeの中で動くプロセス（死んだ着地が残した検証コマンドなど。`run_processes`で、このsupervisor自身の子は除く）も無いときだけ、`recover`と同じ`recover_run`で`awaiting_integration`（leaseなし）に戻して着地スロットを解放し、`auto_repaired`（`repair: landing_released`、欄は`landing_release::released`）を記録する。
    worktreeにプロセスが残っているときの猶予、猶予の後にpidで止めること、止められないときのinboxのattention（`landing_release_stuck`）は`domain::landing_release`のmodule docが持つ。
    持ち主の`integrate`は死んでいるのでそのプロセスの結果を読む者はおらず、着地はやり直され（mainに既にあればIntegratorが`landing_found_on_main`にする）、成果は失われない。
    着地そのものは`start_approved_landings`とIntegratorが行う（ADR-0047の3層、ADR-t728-2）。
    claimを止めてdrainしているsupervisor（fill passを行わない）も、各回でこの解放（`release_dead_landings`）だけは行い、自分の着地を待つrunとdrainが死んだ着地を待たないようにする（列に並んだrunは次のsupervisorが着地させる）。
    承認されたかreviewがpass済みのrun（Integratorが着地させる条件と同じ`RunHistory::landable`）は`landing_queued`を記録して人を待たずに着地の列に並び（[Review](review.md#review-supervisor)の6の`start_approved_landings`）、そうでないrunは`review_recovered_runs`がleaseを取ってvalidationの直後と同じreviewにかける（そのrunの`approve_landing`のaskが開いていれば、その答えを待つ）。
    回答済みのtriageの`decide`のaskを適用し、空いたslotで`failed` / `interrupted`のrunのtriage（復旧job）を、`needs_session`のresume（試行を使い切ったrunは`failed`にして`decide`のaskにする）とclaimと1つの列で、taskの効く優先度の順に始める（5の「slotを渡す順」、ADR-t1850-1）。
    休んでいる`already_satisfied`のrunは回収しない（`satisfied::blocks_recovery`）。
13. **observer**: 停止要求が無くclaimを止めていなければ、observationの期日が来ていて走っているobserverが無いときに`dagq observe`を子プロセスで起動する（[Observer](observer.md#observer)）。run slotは使わない。
14. active runも走っているobserverもなく、`once`か停止要求（下記）か、provisioning失敗でclaimを止めていればループを抜ける（走っているobserverもrunと同じく待つ）。
    それ以外はactive runがない間は間を置いて（runかjobがある間はtickごとに）`candidates`を見る。
    ループを抜けたら（claimやGitのエラーで抜ける場合も含む）自分の登録の削除と`supervisor_stopped`の記録を1つのトランザクションで行う。
    heartbeat失敗で終わるときと記録に失敗したときは消さず、staleとして次の`up`・`down`の掃除（同じく1つのトランザクション）に任せる。

結果の欄の意味は`Supervisor::run_loop`のdoc commentが持つ。
SIGINT/SIGTERMは1回目でclaimを止めてactive runの終了を待ち（graceful drain）、2回目で既定の動作（即終了）になる。
即終了した（killされた）supervisorのleaseはPIDが死んだ時点で（遅くともheartbeatの`HEARTBEAT_TIMEOUT_SECS`で）staleになり、wrapperが生きているrunは次のfill passで別のsupervisorが引き継ぐ（5）。
登録はPIDが死んだ時点から`stale`として`status`/`doctor`に残る。
`status` / `doctor` / `recover` / `integrate`は登録を消さず、次の`up`がPIDの死んだ登録だけを消す（[`up` / `down`](up-down.md#up--down)）。

## CIの見張り（ADR-t1920-1）

[ADR-t1920-1](../../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)と[CI watch](ci-watch.md)。
`SuperviseOptions::ci_watch`を持つsupervisorは、ループの各passで`[provider_fallback]`の後に`[ci_watch]`を読み直して確かめ（[CI watch](ci-watch.md)の「いつ確かめるか」）、答えを持たないか読む手段が無い間はclaim・新しいresume・着地を保留する（同じく「読む手段が無いとき」）。
着地を始めるか・待つか・drainが人に返すかの判断は、`[run.env]`・着地先・diskの保留と合わせて`domain::landing_hold::judge`が持つ。
