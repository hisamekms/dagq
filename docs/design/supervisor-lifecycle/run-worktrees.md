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
worktreeのpathはrun_dirの下の`<runs>/<run-id>/worktree-<run-id>`で、`--repo`のcheckoutとその祖先には触れない。
Gitはworktreeごとの記録（`.git/worktrees/<名前>`）の名前をpathの最後の要素から採り、使われていれば番号を足す。
どのworktreeも同じ名前のpathなら記録は`worktree`・`worktree1`…となり、ある記録がpruneで消えるとdirが残っていても次のaddがその名前を使い回し、元のworktreeの`.git`が別のrunの記録を指す。
`git worktree add`には名前を渡すoptionが無いので、pathの最後の要素をrunごと・scratchごとに一意にして、記録の名前を一意にする（`domain::worktree_name`）。
addはそのpathにそのまま作り、lockもmoveも使わない。
landing recheckのscratch worktreeは`<queue>/recheck/worktree-recheck-<key>`で、`<key>`はqueueのdirのlinkを解いた完全なpathのSHA-256の先頭16桁なので、同じrepositoryの別のqueueはdirの名前が同じでも別の名前になる（`domain::recheck_scratch_name`）。
restoreが作り直すscratchは`<runs>/<run-id>/worktree-<run-id>.restoring`、`already_satisfied`の検証のscratchは`<queue>/satisfied/<run-id>`で、runのworktreeとこの3つのscratchは互いに名前が重ならない。
古い配置のrunのworktree（`<runs>/<run-id>/worktree`）は、queueが記録したpathの最後の要素が`worktree`ならその配置のまま導き（`RunPaths::with_layout_of`）、resume・検証・後片付けが今までどおり見つけて扱う。
着地と後片付けがそのrun自身のworktreeかを決める判断（`is_run_worktree`）も、`worktree-<run-id>`と`worktree`の両方を受ける。
landing recheckは、hostで流すたびにscratchを作る前に、`recheck/`の下の今の名前でないscratch（古い配置の`recheck/worktree`と、queueが動く前のpathから名付けた`worktree-recheck-*`）を記録ごと消す。
古い配置のrunのrestoreは、前のrestoreが古い名前のscratch（`worktree.restoring`）にrunのbranchをcheckoutしたまま止まっていても、新しいscratchを作る前にそれを消す（`replan_snapshot::legacy_scratch_worktree`）。
消すのは、linkでなく、そのrunのbranchかdetachedのworktreeか登録の無いdirだけで（`discard_worktree`）、それ以外（link・file・別のbranchのworktree）はrunのbranchを持たないので残して復元を続ける。
新しいaddは`worktree`・`worktreeN`の名前を使わないので、古い配置の記録の名前を使い回さない。
同じrepositoryで同時に流れる`git worktree add`は、別のGitのコマンドがその時に変えていた記録で落ちることがある（別のaddが書きかけの記録に`commondir`がまだ無い、addが記録のdirを作ってGit自身の`locked`を書くまでの間にpruneがそのdirを消す、addが作った記録を読み直す前にpruneがそれを消す）。
その失敗のときは少し待ち、落ちたaddが残したもの（空か`.git`だけを持つworktreeのdirと、そのdirを指すか、何も指さず作りかけでもない記録）を片付けてから、同じaddを流し直す（`worktree_command`・`clear_failed_add`）。
新しいbranchはaddの前に作り、addが失敗したら消す。
消したものはもう無いので、何度見てもよい。
ただし片付けるものが何も残っていないrunは、同じsupervisorのプロセスの次の掃除からは候補にしない（下の「片付け終わったrun」）。

- **taskが`completed` / `canceled`**: worktreeとbranchを消し、`worktree_removed`を記録する（payloadと残すものは`HostOpsState::request_cleanup`）。
  前のrunのworktreeも、着地した（`integrate`がworktreeを消せなかった）runのものも同じ。
  - 消す前に（下の`.git`の壊れたdirを消す前とも）、worktreeがそのrun自身の`<runs>/<run-id>`の下のworktree（`--repo`のcheckoutとその祖先でない）なら、実行ファイルのpathがその下にあるプロセスを止める（止めるものは`domain::disk::worktree_executables`）。
    一覧を取れなければ止めずに消す。
    着地の削除（`integrate`の`remove_landed_worktree`）も、hostのrunのworktreeがそのrun自身のもの（linkを解決してもそのrunのdirの下の同じ名前のまま）なら、消す前に同じようにプロセスを止める（判断は`own_worktree`）。
    着地の削除の`git worktree remove`が失敗したら（Gitはworktreeのdirを消し切れなくてもadmin dirを消すことがあり、空いた名前を次のworktreeが使う）、そのrun自身のworktreeから`.git`を先に消してからdirを消し、`.git`が他のworktreeのadmin dirを指しうる形で残らないようにする。
    失敗は今までどおり`cleanup_failed`に残す。
  - `git worktree remove`が失敗したら（queueの`rebind`の後などで、worktreeの`.git`が古いcommon dirを指している）、`git worktree repair <worktree>`で直してからもう一度removeし、repairか2回目のremoveも失敗すれば、最初の失敗と合わせて`cleanup_failed`にする。
    repairは`.git`が名指すadmin dirを名前（`worktrees/<name>`）だけで探すので、repairの前に、repositoryのその名前のadmin dirがそのworktreeのものかを確かめる（判断は`owns_admin`）。
    admin dirの`gitdir`がそのworktreeの`.git`を指すか、`HEAD`が消すrunのbranchならそのworktreeのもので、上のとおりrepairする。
    admin dirが無いか、`gitdir`が別のpathを指し`HEAD`も別のbranchなら、名前を使い回した他のworktreeのものなので、repairも2回目の`git worktree remove`もせず、そのrun自身のworktreeならadminを持たないdirとしてdirだけを消し、消すrunのbranchを消す（`worktree_removed`に`no_admin: true`）。
    そのrun自身のworktreeでなければ消さずに`cleanup_failed`にする。
    他のworktreeのadmin dirとbranchには触れない。
    `.git`がadmin dirを名指すgitfileでなければ、上のとおりrepairに任せる。
  - repairも失敗し、その理由が`.git`の壊れ（Gitがそのdirをworktreeとして扱えない）で、worktreeがそのrun自身のものなら、dirをfilesystemで消し、`git worktree prune`の後にbranchを消す（判断は`may_remove_broken`）。
    別の理由の失敗と、dirかbranchを消せなかったときは上と同じく`cleanup_failed`にする（taskが終わっていないrunはworktreeを消さないので、この経路に来ない）。
  - worktreeのディレクトリがもう無いのにbranch `dagq/<run-id>`が残っていれば、`git worktree prune`（Gitはpruneするまで消えたworktreeを覚えていて、そこにcheckoutされたbranchを消させない）の後にbranchを消す。
    branchが無ければ何もしない。
- **それ以外（taskがまだ`in_progress` / `ready`など）**: 次のrunやresumeが引き継ぐかもしれないのでworktreeとbranchは残し、worktree直下のビルド成果物だけを消して`build_outputs_removed`を記録する。
  ビルド成果物のdirの名前は、landing recheckと同じ解決の結果に入っている宣言したprofileが返すもので（[Toolchain profiles](../toolchain-profiles.md)の「buildの出力の置き場」）、宣言が無ければ消さない。
  Gitがファイルを追跡するdirは消さない。
  worktreeが無ければ何もしない。

## 終わっていないrunのビルド成果物

人の答えを待つrunなどはleaseを返したまま`target/`を持ち続け、空きが閾値を下回るとclaimと着地が控えられるので（[空き容量](claim-hold.md#空き容量)）、同じjobが、終わっていないrunのうち誰も作業していないものの、上の「それ以外」と同じビルド成果物も消す。

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

- **runの`already_satisfied`のscratch worktree**: 終わったrunについて、同じjobが、検証の途中で死んだsupervisorが残したqueueの`satisfied/`のそのrunのscratch worktreeを、そこにあるものを信用せずにGitの登録ごと消す（`satisfied_scratch_of`、[Validation](validation.md#already_satisfiedの検証)）。
  eventは記録せず、消せなければ`cleanup_failed`にして、次の掃除で再び試す。

- **runのClaude Codeのscratchpad**: taskが`completed` / `canceled`のrunについて、同じjobが、そのrunのsession（worker）がClaude Codeのscratchpadを置くディレクトリを消す。
  workerは使い捨てのrepositoryや計測用の`target/`をそこに置くので、1 runで数GBになりうる。
  - 名前: runのsessionのcwdはrunのworktreeなので、runtimeはDBのworktreeのpathから、Claude Codeが付けるディレクトリの名前を求める（`scratchpad_dir_name`）。
  - 親ディレクトリ: Claude Codeは`$TMPDIR`ではなくsystemの`/tmp`の下の`claude-<uid>`に置き、runtimeが見る親は`infrastructure::run_files::claude_scratchpad_roots`が求める。
  - 消し方: 求めたディレクトリの外（同じ親の他のproject、`$TMPDIR`の他の一時ファイル、人が開いたplanner・inbox・supervisorのsession（cwdがmain checkoutでrunに紐づかない）のscratchpad）には触れない。
  - 消す条件は上のworktreeを消すのと同じ（slotに無く、生きているleaseが無く、taskが`completed` / `canceled`）で、worktreeがもう無くても、branchがもう無くても消す。
  - 何か消せば`scratchpad_removed`をrunに記録し、消せなければ`cleanup_failed`にして、次の掃除で再び試す。

- **runの一時ファイルのdir**: runtimeは、runのworkerのturn（ClaudeとCodex、最初のturnとresumeのたび、新しいsessionへの切り替えを含む）と、hostで流す着地の検証のコマンド（flakyの流し直しを含む）に、run_dirの下の`tmp`（`application::RUN_TMP_DIR`）を`TMPDIR`として渡す。
  渡す前にdirを作り、workerがその場所に置いたlinkやファイルは辿らずに作り直す（`application::run_tmp_dir`）。
  Codexでは、run_dirがsandboxの書いてよい場所なので（ADR-t813-3決定2）、このdirも書ける。
  元の`$TMPDIR`は`writable_roots`に足して書けるままにし、`/tmp`も既定のまま書ける（[Provider lifecycle](../provider-lifecycle.md)のCodexの節）。
  Claudeでは、turnがrun_dirを`--add-dir`で足すので、このdirも書ける。
  workerのpromptは、どちらのproviderでも、一時ファイル・使い捨てのrepository・worktreeの外に置くbuildの出力を`$TMPDIR`の下に置き`/tmp`や`/private/tmp`に直接置かないことを案内し、それ以外のbuildの場所はrepositoryの指示（AGENTS.mdかCLAUDE.md）に委ねる（このrepositoryの規則は[手元の検証](../../development/local-checks.md)の「workerの手元の検証」）。
  Claude Codeのscratchpadは`$TMPDIR`ではなくsystemの`/tmp`に置かれるので、上の項がこれまでどおり片付ける。
  - 渡さないもの: containerの検証（containerの後片付けが残したものを消す）、landing recheck（queueのscratchのworktreeで、確かめるrunとmainだけの確認が共有し、どのrunのものでもない）、着地の前のe2e（関門が1回ごとの`TMPDIR`を自分で作り、後で消す）、runtimeのplannerのturn（plannerのdirには片付けが無い）。
  - 終わっていないrunのビルド成果物の掃除（空き容量のための掃除を含む）は、このdirを消さない。
    次のturnが同じsessionでその中の使い捨てのrepositoryやbuildを使い続けうり、今動いているprocessを見てもそれを確かめられないためである。
  - 消す条件はscratchpadと同じで、消すのは`<runs>/<run-id>/tmp`だけ（`run_tmp_dir`）。
  - 何か消せば`run_tmp_removed`（`scratchpad_removed`と同じ形）をrunに記録し、消せなければ`cleanup_failed`にして、次の掃除で再び試す。
  - runtimeが決めたdirの外にworkerが作ったものは対象外で、人かinboxが消す。
  - Codexのheadlessのjob（review・goal review・plan reviewなどを`[roles.<role>]`の`provider = "codex"`で動かすもの）は対象外にする: jobはread-onlyのsandbox（`JOB_SANDBOX`か、queue serviceに届くときはそれを広げたjobの権限）で動き、どこにも書けないので、一時ファイルのdirを渡しても使えず、残るものも無い。
    例外の`setup-review`の生成のjobは、runに属さず、`/tmp`と`$TMPDIR`を書けず自分の一時のworktreeだけに書けるsandboxで動き、その一時の場所はコマンドが成否に依らず消すので、これも対象外にする（[Setup review](setup-review.md)）。
  - queue serviceのクライアントモードとの関係: worker・jobの`dagq`のクライアントモードはDBのpathを渡さないだけでworkerはhostのままなので、置き場所と片付けはこのまま変わらない。
    実行側のコンテナの隔離ではworkerの`/tmp`はコンテナと一緒に消えるが、run_dirはコンテナに同じpathでmountされるので、コンテナで動くClaudeのworkerのturnにも同じ`TMPDIR`が届き、run_dirの側から片付けるこの形をそのまま引き継ぐ（コンテナの中の`/tmp`を片付ける仕組みは要らない）。

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
大きさ（`tree_size`）は消した`bytes`を記録するためだけに測り、消すかどうかは測る前の条件で決まっているので、測れなくても消し、消せればその`bytes`に数えず測れなかった理由を添えて今の`*_removed`を記録する（`Size`）。
ただしworktree・clone以外のdirは、測れずに何であるか（linkでないdir）も読めなければ消さずに失敗にする。
`cleanup_failed`になるのは消すこと自体の失敗で、計測の失敗が掃除を毎周回止めることはない。

runのsessionの停止（backgroundのwrapperのhandleの`stop_background`。停止ごとに`wrapper_stopped`も記録する。[後始末](background-recovery-job.md#後片付け)）の成否は`task_runs.workspace_closed_at`で表す。
nullは「止めたことを確かめていない」で、停止の失敗だけでなく、wrapperを止めた後にDBへ書けなかった場合も含む。
停止の失敗は`cleanup_failed`イベントと`last_error`に残るが、run状態は変えない。
止めていないsessionをcleaned扱いにせず、再試行は`doctor`/`recover`で扱う。

## containerのrunのclone

containerで動くrunのclone（[Container runtime](../container-runtime.md#git)）はworktreeと同じ契機で、gitを走らせずsymlinkを辿らずに消え、ビルド成果物の掃除は当てない（[ADR-t2179-1](../../adr/2026-10-12-t2179-1-control-side-runs-no-git-in-a-container-runs-clone-and-lands-in-its-own-worktree.md)決定7）。
作りかけのcloneは、workerの起動をqueueが記録する前なら消し作り直す。
