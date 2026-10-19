---
id: design-supervisor-lifecycle-claim-hold
type: design
title: "claimを控える"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-0047
  - adr-0080
  - adr-t813-2
  - adr-t1063-1
  - adr-t1207-1
  - adr-t1479-1
  - adr-t1484-1
  - adr-t1487-1
  - adr-t1487-2
  - adr-t1489-1
  - adr-t1591-1
  - adr-t1632-1
  - adr-t1634-1
  - adr-t1857-1
  - adr-t1879-1
  - adr-t1981-1
  - adr-t1992-1
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-commands
  - design-supervisor-lifecycle-queue-hold
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-run-worktrees
  - design-supervisor-lifecycle-host-metrics
---

# claimを控える

## 目的

supervisorは新しいrunをclaimする前に、今claimしてよいかを決める。
hostの負荷・空き容量・providerの壁で止まるときはqueue全体のclaimを控え、衝突やbuildの待ちのように1つのtaskだけが進めないときはそのtaskだけを飛ばして次の候補を取る。
走っているrun（sessionの監視・validation・review・resume・triage・着地）には触れない。
空き容量は着地の検証の前にも確かめ、足りなければ着地も控える。

## 流れ

```text
passの初め（[run.env]の検査の次）: 空き容量 → 足りなければ掃除のjob → それでも足りなければinboxにask
fill pass（adopt・answerの適用・resume・triage・sweepの後）
  └─ claimの前: queue全体の控え（provider → 空き容量 → load average）→ 控えるならこのpassはclaimしない
       └─ claimのloop: claimの間隔 → 軽い枠か → 候補ごとの控え（worker → build → hotspot）
            └─ 残りの候補を実行区分の上限とagingで並べる → claim → 進行中のrunを読み直して次の周
```

## 責務と境界

- queue全体の控えの判定は`domain::claim_hold::ClaimHold::judge`で、supervisorの側は`Supervisor::hold_claims`である。
- 候補ごとの控えの判定は`domain::claim_defer`（hotspot・worker）と`domain::build_wait`、supervisorの側は`application/supervise/execution/claim_defer.rs`である。
- claimの間隔は`domain::claim_spacing`、軽い枠は`domain::light_slots`、実行区分の上限とagingは`domain::claim_order`、調査中の計画は`domain::investigation`が持つ。
- 空き容量の閾値は`domain::disk`、確かめと掃除と着地の控えは`application/supervise/host_ops/disk.rs`が持つ。
- 認証と利用上限のaskの検知・answerの適用・askを閉じることは[認証と利用上限のaskの待ちとanswer](queue-hold.md)が持ち、ここはその控えだけを持つ。
- taskを1つずつ待たせるholdは別の層で、下の「taskのhold」。

## 不変条件

- 控えが止めるのは新しいclaimと、空き容量では着地の開始、providerの控えではそのproviderで始めるheadless jobとrunのreviewだけで、走っているrunとleaseを持つrunには触れない。
- queue全体の控えの記録は判定が前回の記録と変わったときだけ書き、同じqueueの2つのsupervisorが交互に書き直さない。
- 候補ごとの控えは始まりと終わりをtaskのeventに残し、`candidates`と`graph`はその記録だけを読むので、表示と実際のclaimが一致する。
- 控えたtaskも、効く優先度が`interrupt`ならhotspotでは控えないが、workerとbuildと実行区分の上限では控える。

## queue全体の控え

- 理由の順はprovider（認証・利用上限のask）→ 空き容量 → load averageで、最初に当たった理由で控える（`ClaimHold::judge`）。
- load averageはhostの1分の値が上限を超えるときだけ控え、等しいときと読めないときは控えない。
- 上限の既定はCLIの`supervise`だけが起動時に`resolve_max_load`で解決し、libraryの`SuperviseOptions::new`の既定は無効である。
  `up`がsupervisorに渡す値は`application::lifecycle`の`max_load`のdoc commentが持つ。
- 判定はclaimするpassごとに1回で、空きslotの有無に依らず、drainやhandoffの途中のpassでは判定しない。
- `--once`のsupervisorは、runが無く控えているpassで終わる（claimできるtaskが無いのと同じ扱い）。
- 落とし穴: 前回の記録はqueueの最新の`claim_held` / `claim_resumed`で、`claim_held`は書いたsupervisorが自分自身か今動いている間だけ続いているとみなす（`domain::claim_hold::transition`）。
  止まったか死んだsupervisorの控えは、loadが高いまま起動したsupervisorが自分のtokenで書き直し、下がっていれば`claim_resumed`で終える。
- 2つのeventはclaimの前の待ちの区間も兼ね（[計測](../measurement.md)の「claimの前」）、区間の終わり方は`domain::claim_hold::hold_spans`が持つ。

## providerごとの控え

- 控えの単位はproviderで、Claudeの控えは認証と利用上限の`queue_hold`のask、Codexの控えはaskを開かない`ProviderHold`である（[ADR-t813-2](../../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)の決定6、[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)）。
- 毎passの読み直しは`Supervisor::check_queue_hold`で、どのaskが控えるかは`domain::queue_hold::reason_of`と`hold_of`が決める。
- 控えがqueue全体のclaimを止めるのは、どのworkerにも経路が無いときだけである（`Supervisor::routes`）。
  一方のproviderが控えられても、もう一方で動かせるtaskは下の「workerを動かせないtask」のとおりそちらでclaimする。
- headless jobの行き先は`Supervisor::start_route`（復旧job・observer・スループットの見直し）、runのreviewは`review_route`、plan reviewとgoal reviewは`JobDesk::held`が控えを読む。
  `provider`を書いた役割のjobは控えの間も使えるproviderで起動する（[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)の決定5）。
  切り替えを止める設定の扱いは[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)。
- runのreviewの待ちは`Phase::ReviewHeld`で、プログラムのreviewは控えの間も流れる（[プログラムのreview](review-programs.md)）。
  始まった後のjobが止まったときは、そのjobだけが他のjobの答えを保ってreviewの中で待つ（[Review](review.md#reviewのprovider)）。
- `--no-claude`のsupervisorは控えを持たず、reviewは`provider`を書いて使えるCodexで動くjobなら起動し、そうでなければ人の`approve_landing`に回る（[ADR-t1207-1](../../adr/2026-09-30-t1207-1-codex-run-review.md)の決定4）。
- 着地（`integrate`）はこの控えで止めない。
- 落とし穴: 回答済みのaskは控えないので、claimとheadless jobはaskが閉じるのを待たず答えた時点で再開する。

## 空き容量

supervisorはqueueのdirectory（run worktreeを置く`runs/`）のファイルシステムの空きを確かめ、足りなければclaimと着地を控えて掃除し、それでも足りないときだけinboxに1件知らせる（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定44）。
検証が容量不足で落ちる代わりに、容量不足という理由で止まる。

### 閾値

- 閾値は`domain::disk::DiskConfig::needs`が、直近のrunの大きさ（`domain::disk::run_size`）にclaimと着地のそれぞれの係数を掛けて決め、下限を設定の最小の空きにする。
- 直近のrunの大きさは新しいrunと着地の検証がdiskに新しく書く量の見積もりで、runごとに増えない共有のキャッシュ（下の「queueのdirのビルドのキャッシュ」）は数えない。
  runの一時ファイルのdirが別のファイルシステムにあっても区別しない。
- 記録も最小の空きも無いときと、空き（`statvfs(3)`の一般のprocessが使える量）が読めないときは確かめず、控えない。
- `[disk]`はsupervisorの起動時にmain checkoutの作業ファイルから読み（[Run environment](run-environment.md)）、読めなければwarnを出して既定値で動く。

### 判定と掃除

- 入口は`HostOpsState::check_disk`で、passの初め（drainやhandoffの途中も）に確かめる。
- 空きがclaimか着地の閾値の大きい方を下回れば掃除を頼み、loopの外のjobが行う（[Run worktrees](run-worktrees.md#loopの外で掃除する)）。
  消すのは終わったrunと誰も作業していないrunのビルド成果物・終わったtaskのworktreeとscratchpadと一時ファイルの消し残しで、leaseかslotか生きているsessionを持つrunには触れない（[Run worktrees](run-worktrees.md#終わっていないrunのビルド成果物)）。
- 掃除のjobが終わるまでclaimと着地は控えるが、控えのeventもaskも記録しない。
  そのあいだもclaimはclaimの閾値を、着地は着地の閾値を下回るときだけ控え、満たす側は掃除を待たずに進む。
- 走っている通常の掃除jobに乗ったときは、そのjobと続く残りのjobの両方を空き容量の掃除とし、残りのjobが終わるまで待つ（`CleanupWatch::for_disk`）。
  掃除の連なりは「乗った通常のjob → 残りのjob」の1回までで、次の頼みは回収から一定の間隔が経つまで出さない（`asks_for_cleanup`）。
  頼みが受けられなかったとき（drainのあいだ）は間隔に数えない。
- claim: 空きとclaimの閾値を`ClaimHold::judge`に渡し、足りなければ理由`disk_space`で控える。
- 着地: 着地slotを待つrunは、空きが着地の閾値を下回る間integrationを始めず、leaseを持ったままrebaseも検証もしない。
  `approve_landing`の`land`のanswerは足りない間もその場で適用して列に並べ、列の着地の開始（`start_approved_landings`）だけが待つ（[Review](review.md#review-supervisor)）。
- 落とし穴: 着地の控えはhostのものでなく記録したsupervisorのslotのものなので、別の生きているsupervisorはその控えを終えない（`domain::claim_hold::LANDINGS`）。
- drainするsupervisorも、空き容量の掃除と残りのjobが終わるまでleaseを持って待ち、その後の容量で足りれば着地へ進み、足りなければleaseを返してrunを`awaiting_integration`のまま人に残す。
  待ちの時間上限は設けず、既に選んだ有限の候補の処理を待つ。
  handoffを初めて読んだ周回では、runを進める前に掃除をdrainの扱いにして読み直す（`end_cleanup_for_handoff`）。
  provisioningの失敗のdrainは掃除をdrainの扱いにしないが、同じく残りのjobが終わるまで待つ。
  `[run.env]`のprogramが無いときと着地のbranchが解決しないときは、掃除を待たずにleaseを返す。
- 人の答えを待つrunのうちturnが終わり何も動いていないものは、空き容量の掃除だけがビルド成果物を消す（[人の答えを待つrun](waiting.md)）。

### queueのdirのビルドのキャッシュ

- 空き容量の掃除（残りのjobを含む）だけが、runの後片付けの後に`update/target`とlanding recheckの宣言ごとのbuildの出力のdirの中身を消し、通常の掃除は作り直しの時間を払わないよう消さない。
- recheckのdirは宣言を変えても前の宣言のdirが残らないよう、今の宣言に依らずそれに当たるdirを全て見る（`recheck::build_dirs`）。
- 使っている間は消さない: recheckのlockと更新のlock（`update::hold_target`）を待たずに取れたときだけ消し、取れなければ何も記録せず次の空き容量の掃除で試す。
- 消している間は使う側が待つ: recheckは次の回にやり直し（[landing recheck](integrate.md#landing-recheck)）、自動更新・release更新・`install --release`は更新のlockを待つ（[Auto-update](auto-update.md)・[Release update](release-update.md)・[install](install.md)）。
  どのsupervisorの掃除でもCLIの`install`でも同じlockのfileなので、queueの中で安全である。
- 消すのはtarget dirの中身だけで、dir自体は空で作り直し、linkはたどらない。
- 消せなければプロセスごとにpathあたり1回だけ`cleanup_failed`を記録する。

### 人が打つ`integrate`

- 人が打つ`dagq integrate`は、`application::integrate::begin`がleaseを確かめた後、承認を記録してslotを取る前に、supervisorと同じ着地の閾値で確かめる。
  `[disk]`はcommandのたびにmain checkoutの`dagq.toml`から読む。
- 足りなければ人が目の前にいるので控えて待たずに断り、承認もrunの状態もleaseも変えず、eventもaskも記録しない。
  エラーは空き・必要量・直近のrunの大きさと掃除の手がかりを、inboxのaskと同じ言い方で書く。
- supervisorの着地はこの確かめをせず、上の「判定と掃除」の控えを使う。

### 検証がディスク満杯で落ちたとき

- 着地の検証コマンドが`disk_full`で落ちたら、integrateはやり直す前に着地の閾値で確かめる（`Integration::retry_disk`、[`integrate`](integrate.md#hostの失敗のやり直し)）。
- 足りれば同じ試行で1回やり直し、足りなければrunを`awaiting_integration`に戻して`integration_held`を記録しinboxの`review and integrate`にする（resumeしない）。
  閾値が決まらないか空きが読めないときは確かめずにやり直す。

### inboxに知らせる

- 掃除の後も（乗った掃除なら残りのjobの後も）足りなければ、jobが終わった後の周回でqueueで1件のdiskの`cost`のaskを開く（ADR-0047の決定42）。
- 着地を待つrunは控えが止めるrunとしてaskに加わり、claimだけが足りないときはrunを持たない。
- ログインのaskと違いsessionを止めないので、sessionの待ちと停滞の見張りが読む`hold_of`には当たらない。
- 同じ不足の間は開き直さず、`done`は閉じてすぐ掃除し直して確かめ、`wait`は閉じて空きが戻るまで開き直さない。
  空きが戻れば、supervisorは開いているdiskのaskにruntimeとして答えて閉じ、控えを解く。
- 落とし穴: 「開いた」の記憶はprocessの中だけなので、起動し直したsupervisorは足りないままなら開いているaskに合流するか1件開く。
- 空きの推移は[hostの負荷の連続の記録](host-metrics.md#ディスクの空き)が持つ。

## claimの間隔

- loadの上限が有効なsupervisorは、新しいclaimの間を空ける（[ADR-t1479-1](../../adr/2026-10-04-t1479-1-space-new-claims-while-the-load-hold-is-on.md)）。
  1分のload averageはclaimしたばかりのrunのbuildをまだ含まないので、空いたslotを続けて埋めると数分後に上限を大きく越えるためである。
- 判定は`domain::claim_spacing`で、queueの最新の`run_claimed`（どのsupervisorのものでも）から測るので、起動し直し・execの引き継ぎ・別のsupervisorのclaimで数え直しにならない。
- 間隔が効くのは、loadの上限が有効で間隔が0でないときだけで、条件は`claim_spacing::in_effect`のdoc commentが持つ。
  効かないときは1つのpassで空いたslotを埋める。
- 間隔が効くときは1つのpassで新しくclaimするのは1本で、間隔が過ぎた後のpassはqueue全体の控えでloadを判定し直す。
- 対象は新しいclaimだけで、resume・待ちからの戻り・着地・triage・headless jobは間隔を見ない。
- 間隔の待ちは`claim_held` / `claim_resumed`にせず、observerと`stats`の控えの数え方に混ぜない。
  待ちの区間は`spaced_since`のcomment、`run_claimed`に載る値は`domain::measure::ClaimAttributes`が持つ。
- `--once`のsupervisorは、runが無く次のclaimが間隔を待つpassで終わる。

## 着地待ちが空けた軽い枠

- reviewとe2eを終えて着地の順番を待つだけのrunが空けた枠では、repositoryが軽いと決めたchangeのtaskだけをclaimする（[ADR-t1591-1](../../adr/2026-10-04-t1591-1-landing-queue-leaves-room-for-light-changes.md)）。
- 着地待ちの印は`Slot::landing_turn`で、見るたびに付け直し、着地待ちに出入りするeventは無い。
- 判定は`domain::light_slots::claim_room`で、軽い枠の周は`--paths`を宣言した軽いchangeのtaskだけを残し、interrupt・urgentでも軽くないtaskは残さない。
- queue全体の控えはloopの前に効くので軽い枠でもclaimせず、claimの間隔も同じに効く（`domain::light_slots::gated`）。
- parkしたrunのresume・待ちからの戻り・triage・adoptは軽い枠を使わない。
- 軽い枠の周で取れない候補は`light_room`の控えとして記録し、agingは軽いtaskだけに当てる（`claim_order::light_room_deferred`）。
- `stats`の空きslotと`kpi`のslotの使用率は軽い枠を数えず、着地待ちを含めて数える。

## 調査中の計画の上限

Spikeがclaimされたgoal（goalの無いSpikeはそれ自体）は、未完了のSpikeか`open`のSpike起点の依頼が残るあいだ調査中の計画で、`[supervisor] max_investigations`がその同時の数を絞る（[ADR-t1487-1](../../adr/2026-10-04-t1487-1-spike-is-an-execution-class-with-a-judged-result-and-a-durable-replanning-request.md)決定7）。
数え方・猶予・判定の順・控えの理由は`domain::investigation`のdoc commentが持つ。

- 控えるのは新しい調査を始めるSpikeのclaimだけで、submitは拒まず、plannerの枠も使わない（待つのはreadyのSpikeで、plannerは結果を待たずに終わる）。
- Spikeの上限が先に効き、両方が効くあいだの控えはSpikeの上限の理由で、それが外れたら調査中の計画の上限の理由に替わる。
- どちらの上限で控えても、空いた枠は実装のtaskが取る。
- claimのトランザクションがqueueの今の姿で数え直すので、別のsupervisorのclaimや順の外のtaskでも上限を超えない。

## 衝突の多いファイル

候補のtaskが触ると予想するファイルと進行中のrunのファイルが衝突の多いファイル（hotspot）で重なるとき、supervisorはそのpassでそのtaskを飛ばして次の候補をclaimする（[ADR-0080](../../adr/0080-supervisor-rereads-conflicts-config.md)）。
他にclaimできるtaskが無くslotが空いていても控えたまま待ち、直列にする。

### 判定の入力

- hotspotは`stats`の衝突の多いファイルで`alert`のもので、plan reviewのpromptと同じ計算（`plan_review::conflict_hotspot_files`）である。
- 予想するファイル（`domain::claim_defer::expected_files`）は`--paths`のうちワイルドカードを含まない具体的なpathで、globは範囲の宣言で予想ではないので外す（[ADR-t1981-1](../../adr/2026-10-07-t1981-1-expected-files-leave-out-path-globs.md)）。
  具体的なpathが残らなければ、`dagq related`で似た`completed`のtaskが着地させたcommitの変えたファイルを使う。
- 進行中のrun（`InFlight`）は`in_progress`のtaskの最新のrun（着地待ち・`needs_session`を含む）で、base commitからheadまでの差分のファイルとそのtaskの予想するファイルを持ち、1件claimするたびに読み直す。
- hotspotが無いか進行中のrunがどのhotspotも触らなければ、候補のファイルは読まない。

### 数えない進行中のrun

- 人の答えだけを待つrunは、待ちの始まりから猶予を過ぎたら数えない（[ADR-t1484-1](../../adr/2026-10-04-t1484-1-runs-waiting-only-for-a-person-stop-holding-claims-past-a-grace.md)、`domain::claim_defer::owner_waiting_since`・`counted`）。
  人の答えまでrunを止めるask（`waits_for_owner`）があり、待ちにあるかどのsupervisorのleaseも持たないrunがそれで、leaseを持ち待ちに居ないrunはaskがあっても動くので数える。
  猶予の経過はpassの時刻で判定し、askへの答えや待ちの終わりの反映は進行中のrunの読み直しまで遅れうる。
- 自分のcommitを持たないfailedのrunは、猶予を待たずに数えない（[ADR-t1634-1](../../adr/2026-10-05-t1634-1-failed-runs-without-their-own-commit-hold-no-claims.md)、`failed_without_commit`）。
  validationは落ちたreceiptのcommitも残すので、commitの有無でなくbaseからの差分で見て、差分が読めなければwarnを出して数える。
- 動き出したrunは読み直しの後にまた数え、重なれば新しく控える。

### 判定

- 入口は`domain::claim_defer::decide`で、重なれば控えて最初に控えたときだけ`claim_deferred`を書く。
- 効く優先度が`interrupt`のtaskは控えない。
- 最初の`claim_deferred`から設定の上限の時間を過ぎたtaskは重なっていてもclaimし、次にclaimされるまで控え直さない。
  読み直した新しい上限は進行中の控えにも効き、控えを無効にする値は無い。
- 数えないrunを除くと重ならなくなったtaskは、上限を待たずにclaimする（`left_out`）。
- 落とし穴: 控えの状態はsupervisorが持ち、起動して最初の判定でtaskごとの最新の記録から組み立て直す（`deferrals_in_place`）ので、supervisorが入れ替わっても上限は最初の`claim_deferred`から数える。
  hotspotの控えが期限切れのまま順の記録（上限か軽い枠）を受けたtaskは、その記録が期限切れの始まりを持つ（`claim_defer::HOT_EXPIRED_SINCE`）。

## workerを動かせないtask

- どのproviderでも動かせないworkerと、`dagq.toml`が選んだruntimeで動かせないworkerのtaskはclaimせずに飛ばす（[ADR-t813-2](../../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)）。
- hotspotより先に、候補の`worker`にこのpassの経路（`domain::provider_switch::routes`）があるかを見て、頼んだproviderが使えなくてももう一方で動かせるtaskはそちらでclaimする。
- 切り替えを止める設定のときは、頼んだproviderの控えが解けるのを待ち、実行ファイルが無いときはそれを持つsupervisorが動くまで控えたままである（[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）。
  例外は`--no-claude`で、Claudeを頼んだtaskも設定に依らずCodexでclaimする。
- containerで動かすときはproviderの経路より先にcontainerを見て（`Supervisor::container_deferral`、[Container runtime](../container-runtime.md)）、hostでは動かさない。
- 理由の値と意味は`domain::claim_defer::worker_deferred`と各理由の定数、理由の変わり方は`worker_step`が持つ。
- 両方のproviderが使えないpassはqueue全体の控えがclaimを止めるが、その前に候補をこの判定にかけて控えを記録する。
- 上限は無く、そのworkerを動かせるようになったsupervisorが控えを終えてclaimする。
- claimの後戻り（`claim_for_supervisor_holding`）にも経路の一覧を渡すので、経路の無いworkerのtaskは取らない。

## 依存先の着地を含むbuildを待つtask

- `wait_for_build`を宣言したtaskは、supervisor自身のbuild識別子（[Build identifier](build-identifier.md)）のcommitが直接の依存先の着地commitを全て含むまでclaimしない（[ADR-t1632-1](../../adr/2026-10-05-t1632-1-claim-waits-for-a-build-that-contains-the-dependencies-landings.md)）。
- 判定は`domain::build_wait`（`judge`・`claim_step`・`decide`）で、workerの判定の後・hotspotの前に行い、待つtaskはhotspotの判定にかけない。
- 効く優先度が`interrupt`でも待つ（宣言は落ちる関門を避けるためで、急ぎでも同じに落ちる）。
- 含むかを読めない（gitのerror）ならclaimしない。
  commitを名乗らないbuild（リリースの版、`+unknown`）は判定できず、来ないかもしれないbuildを待ち続けないよう待たずにclaimしてwarnを出す。
  `.dirty`のbuildはそのcommitで判定する。
- 上限は無い。
  自動更新はruntimeのpathを変える着地でだけbuildする（[Auto-update](auto-update.md)の「きっかけ」）ので、依存先の着地がruntimeのpathを変えなければ、後でruntimeを変える別の着地のbuildまで待つ。
- 含むかの答えは着地commitごとにprocessの中でcacheし（`DeferWatch::in_build`）、execの引き継ぎ（[Handoff](handoff.md)）の後は空から全ての待ちを判定し直す。
  固定バイナリが入れ替わっても、引き継がなかったsupervisorのbuildと待ちは変わらない。
- 落とし穴: claimの後戻りも順を渡さない`TaskStore::claim`も`wait_for_build`のtaskを取らず、判定したsupervisorが順に名指したときだけclaimされる（hotspotで控えたtaskは後戻りで取られうる）。
- hotspotで控えている途中のtaskがbuildを待ち始めたら、hotspotの控えはbuildの控えに置き換わる。

## 実行区分の上限とaging

Spikeは実装とworkerの総枠を共有し、枠を予約しない（[ADR-t1487-2](../../adr/2026-10-04-t1487-2-spikes-share-the-worker-slots-under-a-cap-with-cross-class-aging.md)、agingは[ADR-t1489-1](../../adr/2026-10-11-t1489-1-class-aging-reorders-only-within-one-effective-priority.md)）。
判定は純粋関数`domain::claim_order::arrange`で、上の控え（worker・build・hotspot）を通った候補にだけ当てる。

- Spikeの上限の数のSpikeのrunがslotを持つあいだ、Spikeの候補を順から外し、空いた枠は実装の候補が取る。
  人の答えを待ってslotの外にいるrunは数えず、interruptも上限を越えない。
  走っているrunは止めず、slotの外から戻るSpikeで一時的に超えても新しいSpikeをclaimしないだけである。
- 上限に達している区分は、claimの後戻りもその区分を取らない。
- agingは効く優先度の中だけで並べ替え、優先度を越えず、区分の中の順とinterruptの順を変えない。
  控えや上限で除かれた候補は前に出さない。
- claimは再開・復旧jobとの列（`slot_order`）で各自の効く優先度で比べ、`run_claimed`の区分とagingの記録は`domain::claim_facts`が持つ。

## 記録と見え方

- queue全体の控えは`claim_held` / `claim_resumed`（queueのevent）、着地の控えは`landing_held` / `landing_resumed`、候補ごとの控えは`claim_deferred` / `claim_deferral_ended`（taskのevent）で、欄は各判定のdoc commentが持つ。
- 候補ごとの控えの区間は`domain::claim_defer::deferral_spans`で、どのsupervisorの記録でも終わる。
- `candidates`と`graph`は記録を`application::claim_view`で読み、開いている控えのtaskを順から除いて別に出す（[ADR-t1992-1](../../adr/2026-10-07-t1992-1-candidates-show-the-order-as-claimed-from-the-supervisor-s-deferral-records.md)）。
  CLIは控えを判定し直さず、記録を閉じたり足したりもしない。
- 落とし穴: 控えが閉じるのはsupervisorが次に判定したときだけで、slotが無いpassとclaimを止めているpass（両方のproviderが使えないpassを除く）では判定せず、hotspotの入力もcacheするので、その間は前の判定の控えが開いたまま出る（遅れの上限は無い）。
- `show --full`は控えを出さず、控えはeventsで読む。
- `stats`は空きslotがあり控えているとき、空きslotのalertの代わりに控えのalertを出し、控えによる空きと依存の詰まりによる空きを区別する（[`status`と`stats`](commands.md)）。

## taskのhold

- taskを1つずつ待たせるhold（[ADR-t1879-1](../../adr/2026-10-07-t1879-1-hold-a-task-at-the-turn-boundary-and-release-it-explicitly.md)）は、fill passの列（resume・復旧job・claim）に入る前に外れ、外したことを工程ごとに1回残す（`supervise::execution::task_hold`）。
- 列を作った後にかかったholdも、claim・resume・triageの開始がトランザクションの中で読み直すので、その開始は起きない。
- queue全体の控えではないので`claim_held`を書かず、`stats`の控えにも数えない。
- 走っているrunは工程の終わりで再計画と同じ保留を通って待ちに入り（[needs_session](needs-session.md#taskのhold)）、それまでの解除は拒んでholdの印を変えない。
- hold中のtaskは`status`（`--role inbox`を含む）の`held`に理由とともに出る。
  期限を過ぎたか、期限が無く既定の時間を超えたholdはattentionになり、`watch`には同じholdにつき一定の間隔ごとに1回だけ知らせる。
  holdは自動では解かれない。
  欄と判定の詳細は定義のそばのdoc commentと`--help`、[taskのhold](../../plans/task-hold.md)が持つ。
- 解除の続け方の選択は予定で、[taskのhold](../../plans/task-hold.md)が持つ。
