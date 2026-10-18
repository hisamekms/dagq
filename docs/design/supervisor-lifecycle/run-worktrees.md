---
id: design-supervisor-lifecycle-run-worktrees
type: design
title: "Run worktrees"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
---

# Run worktrees

終わったrunのworktreeが占めるディスクを、supervisorが自動で空ける。
対象は、slotに居ないrunのうち、生きているsupervisorのleaseが無く`integrated`・`succeeded`・`failed`・`interrupted`で終わったもの（staleなleaseは無いものとして扱う。[掃除](background-recovery-job.md#終わったrunの掃除)と同じ）と、leaseが無くtaskが`completed` / `canceled`になったもの（runの状態を問わない）（`ended_run_worktrees`）。
worktreeのpathはrun_dirの下の`<runs>/<run-id>/worktree`で、`--repo`のcheckoutとその祖先には触れない。
消したものはもう無いので、何度見てもよい。
ただし片付けるものが何も残っていないrunは、同じsupervisorのプロセスの次の掃除からは候補にしない（下の「片付け終わったrun」）。

- **taskが`completed` / `canceled`**: worktreeとbranchを消し、`worktree_removed`を記録する（payloadと残すものは`HostOpsState::request_cleanup`）。
  前のrunのworktreeも、着地した（`integrate`がworktreeを消せなかった）runのものも同じ。
  - 消す前に（下の`.git`の壊れたdirを消す前とも）、worktreeがそのrun自身の`<runs>/<run-id>/worktree`（`--repo`のcheckoutとその祖先でない）なら、実行ファイルのpathがその下にあるプロセスを止める（止めるものは`domain::disk::worktree_executables`）。
    一覧を取れなければ止めずに消す。
  - `git worktree remove`が失敗したら（queueの`rebind`の後などで、worktreeの`.git`が古いcommon dirを指している）、`git worktree repair <worktree>`で直してからもう一度removeし、repairか2回目のremoveも失敗すれば、最初の失敗と合わせて`cleanup_failed`にする。
  - repairも失敗し、その理由が`.git`の壊れ（Gitがそのdirをworktreeとして扱えない）で、worktreeがそのrun自身のものなら、dirをfilesystemで消し、`git worktree prune`の後にbranchを消す（判断は`may_remove_broken`）。
    別の理由の失敗と、dirかbranchを消せなかったときは上と同じく`cleanup_failed`にする（taskが終わっていないrunはworktreeを消さないので、この経路に来ない）。
  - worktreeのディレクトリがもう無いのにbranch `dagq/<run-id>`が残っていれば、`git worktree prune`（Gitはpruneするまで消えたworktreeを覚えていて、そこにcheckoutされたbranchを消させない）の後にbranchを消す。
    branchが無ければ何もしない。
- **それ以外（taskがまだ`in_progress` / `ready`など）**: 次のrunやresumeが引き継ぐかもしれないのでworktreeとbranchは残し、worktree直下のビルド成果物だけを消して`build_outputs_removed`を記録する。
  ビルド成果物のdirの名前は、landing recheckと同じ解決の結果に入っている宣言したprofileが返すもので（[Toolchain profiles](../toolchain-profiles.md)の「buildの出力の置き場」）、宣言が無ければ消さない。
  Gitがファイルを追跡するdirは消さない。
  worktreeが無ければ何もしない。

## 終わっていないrunのビルド成果物

人の答えを待つrunなどはleaseを返したまま`target/`を持ち続け、空き容量が尽きると`queue_hold`（`cost`）でclaimと着地が止まるので、同じjobが、終わっていないrunのうち誰も作業していないものの、上の「それ以外」と同じビルド成果物も消す。

- 対象: run（`awaiting_integration`か`needs_session`）のtaskが`completed` / `canceled`でなく、slotに無く、leaseが1つも無く（staleなleaseも有るものとして扱い、触らない）、生きているsessionが無いもの（`ended_run_worktrees`が返す）。
  例えば、着地の列に並んだrun、`approve_landing`を待つrun、resumeを待つ`needs_session`のrun、`integration_held`で人を待つrunである。
- **人の答えを待つrun**（`AwaitingAnswer`）: runに開いていて答えの無いaskがあり、答えまで何時間も待つので、空きの多少によらず、jobの通常の周回で消す。
- **それ以外**（`Idle`）: すぐ使われうるので通常は残し、空き容量のための掃除（[claimを控える](claim-hold.md)の「判定と掃除」）のjobのときだけ消して、そのjobの`auto_repaired`に数える。
- 消さないもの: leaseかslotを持つrun（作業中・validating・review・resume・integrate・着地の前のe2e）。
  ただしADR-0071の待ちのrun（leaseとsessionを持ったまま答えを待つ）のうちturnの終わったものは、空き容量のための掃除だけが、leaseを持つsupervisorから受け取ってビルド成果物を消す（[人の答えを待つrun](waiting.md)の「空き容量のための掃除」）。
  worktree・branch・commit・receipt・run_dir・未commitの変更（ソース）は残し、`runner`も消さない（下の`runner`の項は終わったrunだけ）。
- 消している最中: jobが通り過ぎるまでrunは予約され、loopはleaseを取る前に予約のlockを取り、予約されたrunはその周回は取らない（下の「loopの外で掃除する」）。
  消した後に着地やresumeが始まれば、検証とworkerがsccacheでビルドし直して進む。
  人が打つ`integrate`はこの予約を見ない。

- **runの`runner`**: 上のどちらでも、同じjobがrun_dirの`runner`（claimとresumeのたびに写すバイナリの写し）を消し、消してよい理由と失敗のやり直しは`remove_run_runner`が持つ。
  worktreeの有無を問わず、run_dirの他のfile（`receipt.json`・検証のlogなど）は残す。

- **runのClaude Codeのscratchpad**: taskが`completed` / `canceled`のrunについて、同じjobが、そのrunのsession（worker）がClaude Codeのscratchpadを置くディレクトリを消す。
  workerは使い捨てのrepositoryや計測用の`target/`をそこに置くので、1 runで数GBになりうる。
  - 名前: runのsessionのcwdはrunのworktreeなので、runtimeはDBのworktreeのpathから、Claude Codeが付けるディレクトリの名前を求める（`scratchpad_dir_name`）。
  - 親ディレクトリ: Claude Codeは`$TMPDIR`ではなくsystemの`/tmp`の下の`claude-<uid>`に置き、runtimeが見る親は`infrastructure::run_files::claude_scratchpad_roots`が求める。
  - 消し方: 求めたディレクトリの外（同じ親の他のproject、`$TMPDIR`の他の一時ファイル、人が開いたplanner・inbox・supervisorのsession（cwdがmain checkoutでrunに紐づかない）のscratchpad）には触れない。
  - 消す条件は上のworktreeを消すのと同じ（slotに無く、生きているleaseが無く、taskが`completed` / `canceled`）で、worktreeがもう無くても、branchがもう無くても消す。
  - 何か消せば`scratchpad_removed`をrunに記録し、消せなければ`cleanup_failed`にして、次の掃除で再び試す。

- **runの一時ファイルのdir**: runtimeは、Codexの非対話のworkerのturn（execとresumeのたび）に、run_dirの下の`tmp`（`application::RUN_TMP_DIR`）を`TMPDIR`として渡す（[Provider lifecycle](../provider-lifecycle.md)のCodexの節）。
  run_dirはCodexのsandboxの書いてよい場所なので（ADR-t813-3決定2）、このdirも書ける。
  元の`$TMPDIR`は`writable_roots`に足して書けるままにし、`/tmp`も既定のまま書ける。
  workerのpromptは、一時ファイル・使い捨てのrepository・worktreeの外に置くbuildの出力を`$TMPDIR`の下に置き`/tmp`や`/private/tmp`に直接置かないことを案内し、それ以外のbuildの場所はrepositoryの指示（AGENTS.mdかCLAUDE.md）に委ねる（このrepositoryの規則は[手元の検証](../../development/local-checks.md)の「workerの手元の検証」）。
  Claudeのworkerには渡さず、promptも変えない（Claude Codeのscratchpadは上の項が片付ける）。
  - 消す条件はscratchpadと同じで、消すのは`<runs>/<run-id>/tmp`だけ（`run_tmp_dir`）。
  - 何か消せば`run_tmp_removed`（`scratchpad_removed`と同じ形）をrunに記録し、消せなければ`cleanup_failed`にして、次の掃除で再び試す。
  - runtimeが決めたdirの外にworkerが作ったものは対象外で、人かinboxが消す。
  - Codexのheadlessのjob（review・goal review・plan reviewなどを`[roles.<role>]`の`provider = "codex"`で動かすもの）は対象外にする: jobはread-onlyのsandbox（`JOB_SANDBOX`か、queue serviceに届くときはそれを広げたjobの権限）で動き、どこにも書けないので、一時ファイルのdirを渡しても使えず、残るものも無い
  - queue serviceのクライアントモードとの関係: worker・jobの`dagq`のクライアントモードはDBのpathを渡さないだけでworkerはhostのままなので、置き場所と片付けはこのまま変わらない。
    実行側のコンテナの隔離ではworkerの`/tmp`はコンテナと一緒に消えるが、ビルドの出力とrun_dirはコンテナにmountされるhostのdirに残るので、`TMPDIR`をそのmountの中の同じdirに向ければ、run_dirの側から片付けるこの形をそのまま引き継げる（コンテナの中の`/tmp`を片付ける仕組みは要らない）。

空き容量がclaimか着地の検証に足りないときも、supervisorは同じ掃除と`git worktree prune`を走らせる（閾値と`auto_repaired`は[claimを控える](claim-hold.md#空き容量)）。

## loopの外で掃除する

数GBの`target/`の計測（`tree_size`）と削除はloopの外のthread（掃除のjob）が行い、loopは完了を待たないので、生きているrunのidle・receipt・stallの検知が遅れない。
周回ごとの予算で区切る方式にしなかったのは、1つのworktreeの`target/`の削除だけで数十秒かかりうり、件数や時間の予算では1件の途中で止められないから。

- 下の契機は掃除を頼むだけ（`request_cleanup`）で、jobが無ければloopがその場で候補を選んでjobを起こし、jobが走っていれば頼みを溜めて、jobが終わった後の周回で次のjobにする。
  jobは同時に1つだけなので、同じworktreeを二重に掃除しない。
- jobが選んだrunは、jobがそのrunを通り過ぎるまで予約される。
  loopはleaseの無いrunにleaseを取る前（triage・resume・着地の列・着地の前のe2e・reviewし直し）に予約のlockを取り、予約されたrunはその周回は取らない（掃除がtriageより先になる）。
  そうして取らなかったrunがあれば、loopはそのjobをrunと同じく待つ（`supervise --once`がjobの後にtriageやresumeをしてから終わるように）。
  jobは候補ごとに、同じlockの中で消す前にそのrunだけをqueueから読み直し（`ended_run_worktree`）、選ばれたときと同じ状態で候補に残っているものだけを掃除する。
  選ばれた後にleaseが付いた（別のsupervisorがclaimした）run、状態が変わったrunやtaskは飛ばし、次の掃除が選び直す。
  どちらが先でも、掃除中のworktreeのrunがclaimされることはない。
- **片付け終わったrun**: jobは候補を通り過ぎた後、何も残っていなければ、そのrunを片付け済み（`Cleaning::settled`）として、選ばれたときの値と一緒に、予約を外すのと同じlockの中でプロセスの中で覚える。
  loopは次からの掃除の候補（`pick_candidates`）から片付け済みのrunを外し、そのrunにはgitもfilesystemの処理もしない。
  `ended_run_worktrees`がそのrunを返さないか違う値で返す、slotがそのrunを持つ、loopがそのrunのleaseを取る、のどれかで片付け済みを忘れ、再び候補にする。
  片付けに失敗したrunと、stopで通り過ぎなかったrunは覚えないので、次の掃除で再び試す。
  - プロセスの中で覚え、queueに記録しない理由: 候補から外すには、runが再びworktreeを持ちうる契機（leaseと状態の変化）で忘れる必要がある。queueに印を足すとmigrationと、leaseを取るすべての書き込みで印を消すことが要るが、プロセスの中なら、loopが毎回の掃除で読む`ended_run_worktrees`の値と、loopのleaseの取り方（上の予約のlock）でそれを確かめられる。新しいsupervisor（入れ替えやhandoffの後）は覚えていないので、最初の掃除で全runを1回ずつ確かめ直す（全件の読み直しはしないので、候補N件に比例する）
  - 残る隙: 別のsupervisorがこのプロセスの2回の掃除の間にleaseを取ってworktreeを作り直し、同じ状態で返したrunは、このプロセスが入れ替わるまで候補に戻らない。このrepositoryの運用ではqueueごとに常駐するsupervisorは1つで（入れ替えのhandoffの間だけ重なる）、新しいプロセスは覚えていないので、この隙は受け入れる
- eventはloopが記録する。
  loopは周回の最初にjobが終わっていればjoinし、jobが返した結果からeventを記録する（`cleanup_failed`はプロセスごとにworktreeあたり1回）。
- 空き容量のための掃除（[claimを控える](claim-hold.md#空き容量)）も同じjobに乗り、そのjobは最後に`git worktree prune`を行い、loopはこのjobだけはrunと同じく待つ（`supervise --once`がそのjobの後の周回でclaimできるように）。
  別のjobに乗ったときの残りのjob（`Request::counted`）、そのあいだのclaimと着地の控え、次の頼みまでの間隔は[claimを控える](claim-hold.md)の「判定と掃除」が持ち、`supervise --once`のloopは残りのjobも待つ。
- 空き容量のためのjobと残りのjobは、runの後片付けの後に、queueのdirのビルドのキャッシュ（`update/`と`recheck/`の下の宣言ごとのbuildの出力）の中身も、それぞれのlockを待たずに取れたときだけ消して`build_cache_removed`（queueのevent）を記録する（[claimを控える](claim-hold.md)の「queueのdirのビルドのキャッシュ」）。
- stopかhandoffでは、空き容量のためのjobは選んだ最後のworktreeまで処理し、通常のjobは今のworktreeを終えたところで止まる。
  溜めた頼みは捨て、新しいjobは始めない（残りは次のsupervisorの最初の掃除が拾う）。
  ただし、空き容量のための掃除を別のjobに乗せた残り（`Request::counted`）だけは捨てず、走っているjobの後に1回だけjobとして始め、停止の印（`cleanup.stop`）で止めずに選んだ最後のworktreeまで処理する。
  stopかhandoffの後に来た新しい頼みは受けない。
  handoffのexecはjobの終わりを、残りのjobがあればその終わりも待つ。
  loopが終わるときは、走っているjobと溜めた頼みのjobを待ってeventを記録する（errorで終わったときも同じ規則でjobを待って記録する）。
  handoffの要求がdrain中に取り消されたときに掃除を通常に戻すことは[Handoff](handoff.md)が持つ。
- provisioningの失敗でclaimを止めたsupervisor（`claiming`が`false`）もdrainするが、stopやhandoffと違って掃除はdrainの扱いにせず、頼みを受け、通常のjobも止めず、溜めた頼みのjobも始める（空き容量の待ちは[claimを控える](claim-hold.md#空き容量)の着地）。
  loopは`for_disk`のjobを待ってから終わる。

消す契機は、supervisorのslotが終わったとき（`integrated`・`failed`・`interrupted`）、triageが終わったとき、triageの`decide` askとlandingの`approve_landing` askのanswer（`cancel`を含む）を適用したとき（そのtaskのrunだけ）と、上の掃除と同じ回（`sweep_ended_runs`。終わったrunに残ったbackgroundのwrapperを止めるのと同じ回で、全runを見直す）。
手での`integrate`は着地したrunのworktreeを自分で消し、手での`recover`・人の`ready` / `cancel`・supervisorの外で終わったrunは次の掃除が拾う。
失敗は`cleanup_failed`にして残りを続け、次の掃除で再び試す（supervisorのプロセスごとにworktreeあたり1回だけ記録する）。

runのsessionの停止（backgroundのwrapperのhandleの`stop_background`。停止ごとに`wrapper_stopped`も記録する。cmuxを呼ばない。[後始末](background-recovery-job.md#後片付け)）の成否は`task_runs.workspace_closed_at`で表す。
nullは「止めたことを確かめていない」で、停止の失敗だけでなく、wrapperを止めた後にDBへ書けなかった場合も含む。
停止の失敗は`cleanup_failed`イベントと`last_error`に残るが、run状態は変えない。
止めていないsessionをcleaned扱いにせず、再試行は`doctor`/`recover`で扱う。

## containerのrunのclone

containerで動くrunのclone（[Container runtime](../container-runtime.md#git)）はworktreeと同じ契機で、gitを走らせずsymlinkを辿らずに消え、ビルド成果物の掃除は当てない（[ADR-t2179-1](../../adr/2026-10-12-t2179-1-control-side-runs-no-git-in-a-container-runs-clone-and-lands-in-its-own-worktree.md)決定7）。
作りかけのcloneは、workerの起動をqueueが記録する前なら消し作り直す。
