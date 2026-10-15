---
id: design-supervisor-lifecycle-disk-space
type: design
title: "空き容量を確かめる（claimと着地の検証の前）"
status: current
created: 2026-09-27
scope: runtime
related:
  - adr-t639-1
  - adr-0047
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-claim-hold
  - design-supervisor-lifecycle-run-worktrees
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-commands
  - design-supervisor-lifecycle-host-metrics
---

# 空き容量を確かめる（claimと着地の検証の前）

supervisorはclaimの前と着地の検証の前に、queueのdirectory（run worktreeを置く`runs/`）のファイルシステムの空きを確かめ、足りなければclaimと着地を控えて掃除し、それでも足りないときだけinboxに1件知らせる（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定44）。
検証が容量不足で落ちる代わりに、容量不足という理由で止まる。

控えの仕組みと記録は[load averageによるclaimの控え](claim-hold.md)と同じもので、理由`disk_space`を足し、着地の検証の控えにも同じ形の記録を使う。

## 閾値

`domain::disk::DiskConfig::needs`が、直近のrunのビルドの大きさから決める。

- 直近のrunの大きさ: 直近の`build_outputs_removed`・`scratchpad_removed`・`run_tmp_removed`のそれぞれの`bytes`の最大値の和（`domain::disk::run_size`）。
  `build_outputs_removed`は終わっていないrunのものも`reason`を問わず数える。
  `$CLAUDE_CODE_TMPDIR`が別のファイルシステムでも区別しない。
  queueのdirのビルドのキャッシュ（`build_cache_removed`、下の「queueのdirのビルドのキャッシュ」）は数えない。
  見積もりは新しいrunと着地の検証がdiskに新しく書く量で、共有のキャッシュはrunごとに増えないため。
- claimの前: その和 × `claim_factor`
- 着地の検証の前: その和 × `integrate_factor`
- どちらも`min_free_bytes`を下限にする。
  記録がまだ無く`min_free_bytes`も無ければ確かめない

値は`dagq.toml`の`[disk]`（書式は[Run environment](run-environment.md)）で変える。
読むのはmain checkoutの作業ファイルで、supervisorの起動時に読む（読めなければwarnを出して既定値）。
直近のrunの大きさは毎passではなく間隔をおいて読み直す。

空きは`statvfs(3)`の一般のprocessが使える量（`infrastructure::adapters::free_disk_bytes`）で、supervisorのpassごとに読む。
読めなければ控えない。

## 判定と掃除

supervisorはpassの初め（`[run.env]`のprogramの検査の次、drainやhandoffの途中も）に`HostOpsState::check_disk`で確かめる。

1. 空きがclaimかlandingの閾値の大きい方を下回れば、掃除を自動で走らせる。
   消すのは、終わったrunのビルド成果物の消し残し、誰も作業していない終わっていないrun（着地の列のrun、resumeを待つrunなど。leaseもslotも生きているsessionも無いもの）のビルド成果物（[Run worktrees](run-worktrees.md#終わっていないrunのビルド成果物)）、`completed` / `canceled`のtaskのworktreeとClaude Codeのscratchpadとrunの一時ファイルのdirの消し残し（[Run worktrees](run-worktrees.md)の掃除と同じ）と、`git worktree prune`。
   掃除はloopの外のjobが行い（[Run worktrees](run-worktrees.md#loopの外で掃除する)）、そのjobが終わるまでclaimと着地は控えるが、控えのeventもaskも記録しない。
   走っている通常の掃除jobに乗ったときは、そのjobと、乗った後に続く残りのjobの両方を空き容量のための掃除とし、残りのjobが終わるまで同じく控えのeventもaskも記録しない（`CleanupWatch::for_disk`）。
   そのあいだもclaimは空きがclaimの閾値を下回るときだけ、着地は着地の閾値を下回るときだけ控え、閾値を満たす側は掃除を待たずに進む（2と3）。
   jobが終われば、何か消えればqueueイベント`auto_repaired`を記録し、その後の周回で読み直した空きで判定する。
   空き容量のための掃除が待つか走るあいだは、空きが足りなくても次の空き容量の掃除を頼まない。
   次の頼みは、空き容量のための掃除のjob（残りのjobを含む）が終わって回収した時刻から一定の間隔が経つまで出さない（判断は`asks_for_cleanup`）。
   このため、jobが間隔より長く走っても、終わった後の周回は新しい頼みに抑えられずに読み直した空きで判定し、掃除の連なりは「乗った通常のjob → 残りのjob」の1回までになる。
   頼みが受けられなかったとき（drainのあいだ）は間隔に数えない。
   空き容量のための掃除のjob（乗った通常のjobでなく、自分で起こしたjobか残りのjob）は、使っていないqueueのdirのビルドのキャッシュも消し（下の「queueのdirのビルドのキャッシュ」）、消したキャッシュとその`bytes`も`auto_repaired`に記録する。
2. claim: fill passのclaimの控えの判定が、空きとclaimの閾値を`ClaimHold::judge`に渡す。
   足りなければ理由`disk_space`で控え（load averageより先に判定する）、`claim_held`を記録する。
   空きが戻れば`claim_resumed`
3. 着地: 着地slotを待つrun（`Phase::AwaitingSlot`）は、空きが着地の閾値を下回る間integrationを始めない。
   runは`awaiting_integration`のままleaseを持ち、rebaseも検証も始めない。
   `approve_landing`の`land`のanswerは足りない間もその場で適用してaskを閉じ、runを着地の列に並べる。
   列のrunの着地の開始（`start_approved_landings`）だけが、空きが戻るまで待つ（[Review](review.md#review-supervisor)の6）。
   着地を待つrunがあり足りない間はqueueイベント`landing_held`を、空きが戻るか待つrunが無くなれば`landing_resumed`を記録する（`domain::claim_hold::LANDINGS`）。
   着地の控えはhostのものではなく記録したsupervisorのslotのものなので、別の生きているsupervisorは`landing_resumed`で終えない（そのsupervisorが止まれば終えてよい）。
   drainするsupervisorも（stop、handoff、provisioningの失敗でclaimを止めたもののどれでも）、空き容量のための掃除jobと、それが通常のjobに乗ったときの残りのjobが終わるまで、leaseを持って待つ（`CleanupWatch::for_disk`。drainの間も`disk.cleaning`が続く）。
   handoffの頼みは周回の中で掃除のpollと空きの読み取りの後に読むので、handoffを初めて読んだ周回では、runを進める前に掃除をdrainの扱いにして`disk.cleaning`を読み直す（`end_cleanup_for_handoff`）。
   このとき終わったjobは回収しない。
   空きを読んだ後に終わったjobは次の周回で回収し、その後に読み直した空きで判定する。
   provisioningの失敗のdrainはstopやhandoffと違って掃除をdrainの扱いにしないが、`for_disk`はそれによらず残りのjobを含むので、同じく残りのjobが終わるまで待つ。
   jobが最後のworktreeまで処理し、結果を記録した後の容量で判定し、足りれば着地へ進む。
   なお足りなければleaseを返してrunを`awaiting_integration`のまま人に残す。
   待ちの時間上限は設けず、既に選んだ有限の候補の処理を待つ。
   `[run.env]`のprogramが無い場合と着地のbranchが解決しない場合は掃除を待たずleaseを返す

走っているrun（session・validation・review・resume・triage）と、leaseを持つrunには触れない。
ただしADR-0071の待ちのrunのうちturnが終わり何も動いていないものは、1の掃除だけがビルド成果物を消す（[人の答えを待つrun](waiting.md)の「空き容量のための掃除」）。

### queueのdirのビルドのキャッシュ

空き容量のための掃除（乗った通常のjobの残りのjobを含む）は、runの後片付けの後に、queueのdirのビルドのキャッシュ2つの中身を消す。
1つは`update/target`で、自動更新のビルドとe2eの関門、release更新のjobと`install --release`の`cargo install`が`CARGO_TARGET_DIR`に使う。
もう1つはlanding recheckの`recheck/target`。
どちらも消して作り直せるキャッシュで、sccacheがcompileを共有するので作り直しは速い。
空きが足りる通常の掃除では消さず、作り直しの時間を払わない。

- 使っている間は消さない: 掃除のjobは`recheck/target`をrecheckのlock（`recheck/lock`）、`update/target`を更新のlock（`update/lock`、`update::hold_target`）を待たずに取れたときだけ消し、消し終わるまで持つ。
  取れなければ飛ばし、何も記録せず、次の空き容量のための掃除でまた試す。
- 消している間は使う側が待つ: recheckは同じlockを待たずに取るので始めず、次の回にやり直す（[Landing recheck](landing-recheck.md)）。
  自動更新のjob、release更新のjob、`install --release`は`update/lock`を待って取るので、消し終わってから始める（[Auto-update](auto-update.md)・[Release update](release-update.md)・[install](install.md)）。
  どのsupervisorの掃除でも、CLIの`install`でも同じlockのfileなので、queueの中で安全。
- 消すのはtarget dirの中身だけで、dir自体は空で作り直す。
  `update/release`・`update/staged`・`update/checkout`・`update/e2e`・`recheck/worktree`は残す。
  target dirがlinkならたどらず消さない（中のlinkもたどらない）。
  消す前に`tree_size`で測る。
- 記録: 消せばキャッシュごとにqueueイベント`build_cache_removed`を記録する。
  runに紐づかないので、runのeventでなくqueueのeventにする。
  消した大きさも`auto_repaired`に数える（上の1）。
  消せなければqueueイベント`cleanup_failed`にし、プロセスごとにpathあたり1回だけ記録する。
- 閾値の見積もり（直近のrunの大きさ）には数えない（上の「閾値」）。

## 人が打つ`integrate`

人が打つ`dagq integrate`（`dagq-recover`の手順で打つものも）は、supervisorの着地と同じ着地の閾値で空きを確かめる。
`application::integrate::begin`が、runを選び、leaseを確かめた後、`integration_approved`を記録してintegrationのslotを取る前に確かめる。

- 閾値: supervisorと同じ`DiskConfig::needs`の着地の側。
  `[disk]`はmain checkoutの`dagq.toml`をcommandのたびに読み（読めなければwarnを出して既定値）、直近のrunの大きさはqueueから上と同じく読む
- 空き: queueの`runs/`（無ければDBのあるdirectory）のファイルシステムの空き（`free_disk_bytes`）
- 足りなければ、人が目の前にいるので控えて待つのではなく断る: `integration_approved`を記録せず、runの状態もleaseも変えず、空き・必要量・直近のrunの大きさと、掃除の手がかり（`dagq doctor`でrunとworktreeを見る、見なくなったrunのworktreeや他のファイルを消す）を含むエラーで非0終了する。
  eventもaskも記録しない
- 閾値が決まらない（記録も`min_free_bytes`も無い）・空きが読めないときは確かめない（supervisorと同じ）

supervisorの着地（`land_integrating`）は上の「判定と掃除」の控えを使い、この確かめはしない。

## 検証がディスク満杯で落ちたとき

事前の確かめをすり抜けて着地の検証コマンドが`disk_full`で落ちたら（supervisorの着地も人の`integrate`も）、integrateはやり直す前に空きを着地の閾値で確かめる（[`integrate`](integrate.md#integrate)の5の「hostの失敗のやり直し」）。
閾値と空きの読み方は上と同じで、そのときに読む（`Integration::retry_disk`）。

- 足りれば同じ試行で1回やり直す
- 足りなければやり直さず、runを`awaiting_integration`に戻して`integration_held`を記録し、inboxの`review and integrate`になる。
  resumeはしない。
  supervisorの次のpassの`check_disk`は、上の「判定と掃除」のとおり掃除し、足りなければ`cost`のaskを開く。
  空けた後に人が`dagq integrate <task>`を打てば、上の「人が打つ`integrate`」の確かめを通って検証からやり直す
- 閾値が決まらない・空きが読めないときは確かめずにやり直す

## inboxに知らせる

掃除の後も（別のjobに乗った空き容量の掃除なら、その残りのjobの後も）claimか着地の閾値を下回れば、そのjobが終わった後の周回で、queueで1件のdiskの`cost`のaskを開く（ADR-0047の決定42）。
次の掃除の頼みはjobの終わりから`CLEANUP_INTERVAL`経つまで出ないので、判定が先送りされない。
着地を待つrunは控えが止めるrunとしてaskに足され（`ask_updated`）、questionの末尾に並ぶ。
ログインのaskと違い、sessionを止めるものではないので、`hold_of`（sessionの待ちと停滞の見張りがログインの控えを見る）には当たらない。
claimだけが足りないときはrunを持たない（`NewHold::run_id`が`None`）。
askを開いても誰にも通知を送らず、人にはinboxのwatchが`ask_opened`として知らせる（[人への通知](ask-push.md)）。
questionは、掃除したものと、閾値が直近のrunの大きさに係数を掛けたものであることを書く。
人の`integrate`が断るときのエラーも同じ言い方で大きさを書く。

- 同じ不足の間は開き直さない。supervisorはこの不足でaskを開いたことを覚え、空きが戻るまで新しく開かない
- `done`（人が空けた）: supervisorがaskを閉じ、すぐ掃除し直して確かめる（空き容量のための掃除が待つか走るあいだは、新しく頼まずにその終わりを待つ）。まだ足りなければもう1件開く
- `wait`: supervisorがaskを閉じ、空きが戻るまで開き直さない
- 空きが戻れば、supervisorは開いているdiskのaskにruntimeとして答えて閉じ、控えを解く

supervisorを起動し直すと、覚えていた「開いた」は消えるので、足りないままなら開いているaskに合流するか、無ければ1件開く。

## `status`と`stats`

- `status`: claimを控えているsupervisorの項目の控え（理由`disk_space`を含む）と同じ形で、着地を控えているsupervisorは着地の控えを持つ（[`status`](commands.md)）
- 空きの推移: supervisorはhostの負荷の記録と同じ行に`runs/`のファイルシステムの空きを記録し（[hostの負荷の連続の記録](host-metrics.md#ディスクの空き)）、`stats`と`kpi`がその最小値と中央値を出す
- `stats`: claimの控えと着地の控えを数え、理由で空き容量の控えを分ける。
  掃除はruntimeの自動の修復として数える（[`stats`](commands.md)）
