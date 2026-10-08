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
  - design-supervisor-lifecycle-status
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle-host-metrics
---

# 空き容量を確かめる（claimと着地の検証の前）

2026-09-25に、終わったrunの`target/`が残って空きが1.6GiBまで減り、task 276の`integrate`の検証（`cargo llvm-cov`）が`No space left on device (os error 28)`で落ちた。supervisorはclaimの前と着地の検証の前に、queueのdirectory（run worktreeを置く`runs/`）のファイルシステムの空きを確かめ、足りなければclaimと着地を控えて掃除し、それでも足りないときだけinboxに1件知らせる（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定44、goal 34、task 377）。検証が容量不足で落ちる代わりに、容量不足という理由で止まる。

控えの仕組みと記録は[load averageによるclaimの控え](claim-hold.md)（task 327）と同じもので、理由`disk_space`を足し、着地の検証の控えにも同じ形の記録を使う。

## 閾値

`domain::disk::DiskConfig::needs`が、直近のrunのビルドの大きさから決める。

- 直近のrunの大きさ: `build_outputs_removed`（runのworktreeから`target/`などを消したときの記録。終わったrunのものも、終わっていないrunのもの（task 1289）も`reason`を問わず数える）の新しい順に`sample_runs`件（既定20）の`bytes`の最大値と、`scratchpad_removed`（taskの終わったrunのClaude Codeのscratchpadを消したときの記録。[Run worktrees](run-worktrees.md)、task 1100）の新しい順に`sample_runs`件の`bytes`の最大値と、`run_tmp_removed`（taskの終わったrunの、runtimeがCodexのworkerのturnに`TMPDIR`として渡したrun_dirの`tmp`を消したときの記録。[Run worktrees](run-worktrees.md)、task 1290）の新しい順に`sample_runs`件の`bytes`の最大値の和（`domain::disk::run_size`、`application::recent_run_sizes`）。runの作業はworktreeのビルドとscratchpadか一時ファイルのdirを同時にdiskに置くので、一部だけでは見積もりが小さく出る。runごとには足さない: `build_outputs_removed`はtaskが続くrunだけに、`scratchpad_removed`はtaskが終わった後にだけ記録され（着地したrunのビルドはworktreeごと`worktree_removed`で消える）、同じrunに両方がそろうことはまれなので、それぞれの最大値を足す（`run_tmp_removed`も同じ）。一部だけ記録があればその最大値の和。`$CLAUDE_CODE_TMPDIR`が別のファイルシステムでも区別しない
- claimの前: その和 × `claim_factor`（既定2）
- 着地の検証の前: その和 × `integrate_factor`（既定1.5）
- どちらも`min_free_bytes`（既定なし）を下限にする。記録がまだ無く`min_free_bytes`も無ければ確かめない

値は`dagq.toml`の`[disk]`（`sample_runs`・`min_free_bytes`は正の整数、`claim_factor`・`integrate_factor`は正の数。書式は[Run environment](run-environment.md)）で変える。読むのはmain checkoutの作業ファイルで、supervisorの起動時に読む（読めなければwarnを出して既定値）。libraryの`SuperviseOptions::disk`で上書きできる（testが使う）。直近のrunの大きさは60秒ごとに読み直す。

空きは`statvfs(3)`の`f_bavail × f_frsize`（一般のprocessが使える量。`infrastructure::adapters::free_disk_bytes`）で、supervisorのpassごとに読む。読めなければ控えない。

## 判定と掃除

supervisorはpassの初め（`[run.env]`のprogramの検査の次、drainやhandoffの途中も）に`HostOpsState::check_disk`で確かめる。

1. 空きがclaimかlandingの閾値の大きい方を下回れば、掃除を自動で走らせる（頼み直しの間隔は下のとおり）: 終わったrunのビルド成果物の消し残しと、誰も作業していない終わっていないrun（着地の列のrun、resumeを待つrunなど。leaseもslotも生きているsessionも無いもの）のビルド成果物（`build_outputs_removed`の`reason: disk_space`。人の答えを待つrunのものは空きによらず通常の周回で消える。[Run worktrees](run-worktrees.md#終わっていないrunのビルド成果物)、task 1289）と、`completed` / `canceled`のtaskのworktreeとClaude Codeのscratchpadとrunの一時ファイルのdir（task 1290）の消し残し（[Run worktrees](run-worktrees.md)の掃除と同じ）、`git worktree prune`。掃除はloopの外のjobが行い（[Run worktrees](run-worktrees.md#loopの外で掃除する)、task 405）、そのjobが終わるまでclaimと着地は控えるが、控えのeventもaskも記録しない。走っている通常の掃除jobに乗ったときは、そのjobと、乗った後に続く残り（`Request::counted`。そのjobが選ばなかったrunと`Idle`のrunのビルド成果物と`git worktree prune`）のjobの両方を空き容量のための掃除とし、残りのjobが終わるまで同じく控えのeventもaskも記録しない（task 1478。`CleanupWatch::for_disk`が残りの待ちと残りのjobを含み、`disk.cleaning`が続く）。そのあいだもclaimは空きがclaimの閾値を下回るときだけ、着地は着地の閾値を下回るときだけ控え、閾値を満たす側は掃除を待たずに進む（2と3）。jobが終われば、何か消えればqueueイベント`auto_repaired`（`repair: disk_cleanup`、`layer: runtime`、`bytes`、`conditions: {free_bytes, needed_bytes, free_bytes_after}`、`detail: {bytes, runs}`、`supervisor`）を記録する。`bytes`は消したworktree・ビルド成果物・scratchpad・runの一時ファイルのdirの`bytes`の合計で、`runs`は何か消えたrunを1回ずつ並べる。その後の周回で読み直した空きで判定する。空き容量のための掃除（乗った通常のjob、残りの待ちと残りのjobを含む。`CleanupWatch::for_disk`）が待つか走るあいだは、空きが足りなくても次の空き容量の掃除を頼まない（task 1627）。次の頼みは、空き容量のための掃除のjob（残りのjobを含む）が終わって回収した時刻から60秒（`CLEANUP_INTERVAL`。testは`SuperviseOptions::disk_cleanup_interval`で短くする）経つまで出さない（判断は`asks_for_cleanup`、積み方は`add_disk_request`）。このため、jobが60秒より長く走っても、終わった後の周回は新しい頼みに抑えられずに読み直した空きで判定し、掃除の連なりは「乗った通常のjob → 残りのjob」の1回までになる。頼みが受けられなかったとき（drainのあいだ）は間隔に数えない
2. claim: `fill_slots`の`hold_claims`が、空きとclaimの閾値を`ClaimHold::judge`に渡す。足りなければ理由`disk_space`で控え（load averageより先に判定する）、`claim_held`（`value`は空きbytes、`threshold`は要るbytes）を記録する。空きが戻れば`claim_resumed`
3. 着地: 着地slotを待つrun（`Phase::AwaitingSlot`）は、空きが着地の閾値を下回る間`begin_integration`をしない。runは`awaiting_integration`のままleaseを持ち、rebaseも検証も始めない。`approve_landing`の`land`のanswerは足りない間もその場で適用してaskを閉じ、runを着地の列に並べる。列のrunの着地の開始（`start_approved_landings`）だけが、空きが戻るまで待つ（task 949、[Review](review.md#review-supervisor)の6）。着地を待つrunがあり足りない間はqueueイベント`landing_held`（payloadは`claim_held`と同じ。`message`は着地の文）を、空きが戻るか待つrunが無くなれば`landing_resumed`を記録する（`domain::claim_hold::LANDINGS`、`transition_of`）。着地の控えはhostのものではなく記録したsupervisorのslotのものなので、別の生きているsupervisorは`landing_resumed`で終えない（そのsupervisorが止まれば終えてよい）。drainするsupervisorも（stop、handoff、provisioningの失敗でclaimを止めたもの（`claiming`が`false`）のどれでも）、空き容量のための掃除jobが走っている間はleaseを持って待つ（task 648）。通常の掃除jobに空き容量の掃除が乗ったときは、その残り（`Request::counted`）のjobが終わるまでも待つ（task 1426。`CleanupWatch::for_disk`は残りの待ちと残りのjobを含み（1）、drainの間も`disk.cleaning`が続く。handoffの頼みは周回の中で掃除のpollと空きの読み取りの後に読むので、handoffを初めて読んだ周回では、runを進める前に掃除をdrainの扱いにして`disk.cleaning`を読み直す（`end_cleanup_for_handoff`）。このとき終わったjobは回収しない。空きを読んだ後に終わったjobは次の周回で回収し、その後に読み直した空きで判定する）。provisioningの失敗のdrainはstopやhandoffと違って掃除をdrainの扱いにせず（`poll_cleanup`の`ending`は立たない）、新しい掃除の頼みを受け通常のjobも止めないが、`for_disk`は`ending`によらず残りのjobを含むので、同じく残りのjobが終わるまで待つ（task 1482）。jobが最後のworktreeまで処理し、結果を記録した後の容量で判定し、足りれば着地へ進む。なお足りなければleaseを返してrunを`awaiting_integration`のまま人に残す。待ちの時間上限は設けず、既に選んだ有限の候補の処理を待つ。`[run.env]`のprogramが無い場合と着地のbranchが解決しない場合は従来どおり掃除を待たずleaseを返す

走っているrun（session・validation・review・resume・triage）と、leaseを持つrun（ADR-0071の待ちを含む）には触れない。

## 人が打つ`integrate`

人が打つ`dagq integrate`（`dagq-recover`の手順で打つものも）は、supervisorの着地と同じ着地の閾値で空きを確かめる（task 638）。`application::integrate::begin`が、runを選び、leaseを確かめた後、`integration_approved`を記録してintegrationのslotを取る前に確かめる。

- 閾値: supervisorと同じ`DiskConfig::needs`の着地の側。`[disk]`はmain checkoutの`dagq.toml`をcommandのたびに読み（読めなければwarnを出して既定値）、直近のrunの大きさはqueueの`build_outputs_removed`・`scratchpad_removed`・`run_tmp_removed`から上と同じく読む。libraryの`OneShot::disk`で上書きできる（testが使う）
- 空き: queueの`runs/`（無ければDBのあるdirectory）のファイルシステムの空き（`free_disk_bytes`。`OneShot::free_space`でtestが差し替える）
- 足りなければ、人が目の前にいるので控えて待つのではなく断る: `integration_approved`を記録せず、runの状態もleaseも変えず、空き・必要量・直近のrunの大きさ（ビルド成果物の最大値とscratchpadの最大値とrunの一時ファイルのdirの最大値の和。`largest_build_bytes`。名前は前のまま）と、掃除の手がかり（`dagq doctor`でrunとworktreeを見る、見なくなったrunのworktreeや他のファイルを消す）を含むエラーで非0終了する。eventもaskも記録しない
- 閾値が決まらない（記録も`min_free_bytes`も無い）・空きが読めないときは確かめない（supervisorと同じ）

supervisorの着地（`land_integrating`）は上の「判定と掃除」の控えを使い、この確かめはしない。

## 検証がディスク満杯で落ちたとき

事前の確かめをすり抜けて着地の検証コマンドが`disk_full`で落ちたら（supervisorの着地も人の`integrate`も）、integrateはやり直す前に空きを着地の閾値で確かめる（task 639。[`integrate`](integrate.md#integrate)の5の「hostの失敗のやり直し」）。閾値と空きの読み方は上と同じで、supervisorは起動時の`[disk]`と`free_space`、人の`integrate`は`OneShot`の`disk`と`free_space`を、そのときに読む（`Integration::retry_disk`）。

- 足りれば同じ試行で1回やり直す
- 足りなければやり直さず、runを`awaiting_integration`に戻して`integration_held`（`retried: false`、`disk`に`{free_bytes, needed_bytes, largest_build_bytes}`。`largest_build_bytes`は上の直近のrunの大きさ）を記録し、inboxの`review and integrate`になる。resumeはしない。supervisorの次のpassの`check_disk`は、上の「判定と掃除」のとおり掃除し、足りなければ`cost`のaskを開く。空けた後に人が`dagq integrate <task>`を打てば、上の「人が打つ`integrate`」の確かめを通って検証からやり直す
- 閾値が決まらない・空きが読めないときは確かめずにやり直す

## inboxに知らせる

掃除の後も（別のjobに乗った空き容量の掃除なら、その残りのjobの後も。task 1478）claimか着地の閾値を下回れば、そのjobが終わった後の周回で（次の掃除の頼みはjobの終わりから60秒経つまで出ないので、判定が先送りされない。task 1627）、queueで1件の`cost`のask（`kind: queue_hold`、`reason_category: cost`、`subject: disk`、optionsは`done` / `wait`）を開く（ADR-0047の決定42）。着地を待つrunは`affected`に足され（`ask_updated`）、questionの末尾の`Affected runs:`に並ぶ。ログインのaskと違い、sessionを止めるものではないので、`hold_of`（sessionの待ちと停滞の見張りがログインの控えを見る）には当たらない。claimだけが足りないときはrunを持たない（`NewHold::run_id`が`None`）。
askを開いても誰にも通知を送らず、人にはinboxのwatchが`ask_opened`として知らせる（[人への通知](cmux-notify.md)）。
questionは、掃除したもの（終わったrunと、答え・着地・resumeを待つ誰も作業していないrunのビルド成果物、`completed` / `canceled`のtaskのrunのworktreeとClaude Codeのscratchpadとrunの一時ファイルのdir（task 1290）、`git worktree prune`）と、閾値が直近のrunの大きさ（ビルド成果物の最大値とscratchpadの最大値とrunの一時ファイルのdirの最大値の和）に係数を掛けたものであることを書く。人の`integrate`が断るときのエラーも同じ言い方で大きさを書く（task 1100、task 1290）

- 同じ不足の間は開き直さない。supervisorはこの不足でaskを開いたことを覚え、空きが戻るまで新しく開かない
- `done`（人が空けた）: supervisorがaskを閉じ、すぐ掃除し直して確かめる（空き容量のための掃除が待つか走るあいだは、新しく頼まずにその終わりを待つ）。まだ足りなければもう1件開く
- `wait`: supervisorがaskを閉じ、空きが戻るまで開き直さない
- 空きが戻れば、supervisorは開いているdiskのaskにruntimeとして答えて閉じ（`ask_answered`の`runtime_closed: true`、`answered_by: runtime`）、控えを解く

supervisorを起動し直すと、覚えていた「開いた」は消えるので、足りないままなら開いているaskに合流するか、無ければ1件開く。

## `status`と`stats`

- `status`: claimを控えているsupervisorの項目の`claim_hold`（理由`disk_space`を含む）と同じ形で、着地を控えているsupervisorは`landing_hold`（最新の`landing_held`のpayloadと`since`）を持つ（[`status`](status.md)）
- 空きの推移: supervisorはhostの負荷の記録と同じ行に`runs/`のファイルシステムの空きを30秒ごとに記録し（task 1371、[hostの負荷の連続の記録](host-metrics.md#ディスクの空き)）、`stats`の`host.metrics.disk_free_bytes`・`disk_free_pct`と`kpi`の各期間の`health.disk`が最小値と中央値を出す
- `stats`: `claim_holds.by_reason.disk_space`がclaimの控え、`landing_holds`（`claim_holds`と同じ`{count, secs, by_reason, held}`）が着地の控え。load averageの控え（`load_average`）とは理由で区別できる。掃除は`auto_repairs`の`by_layer.runtime.by_repair.disk_cleanup`に数える（[`stats`](stats.md)）
