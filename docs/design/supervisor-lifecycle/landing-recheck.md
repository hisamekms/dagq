---
id: design-supervisor-lifecycle-landing-recheck
type: design
title: "Landing recheck"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0068
  - adr-t1310-1
  - adr-t1311-1
  - adr-0027
  - adr-0047
  - adr-0049
  - adr-t2032-1
  - adr-t639-1
  - adr-t2424-1
---

# Landing recheck

> [ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)決定3（ADR-0068決定4をamends）のとおり、resumeの後は終了の依頼でbackgroundのwrapperを終わらせ、終わったことを確かめ、残っていれば止めてからleaseを手放す（runはworkspaceを開かない）。

supervisorが着地させたrunが`run_integrated`で終わるたびと、mainが最後にrecheckを終えたmainと違うとき（直接の`integrate`、handoffや再起動の間の着地、dagqを通さないpush）に、着地待ちのrunをそのmainに対して先回りして確かめる（[ADR-0068](../../adr/0068-recheck-waiting-runs-after-each-landing.md)、きっかけは[ADR-t1310-1](../../adr/2026-10-03-t1310-1-recheck-whenever-main-moves-past-the-last-recheck.md)が広げた）。着地しなくなったrunは、askの答えや`integrate`の順番を待たずにresumeへ回る。きれいに着地すると確かめたrunにもその結果を記録し、runの開いているaskは最新の結果を1つの段落で見せる（[ADR-t1311-1](../../adr/2026-10-05-t1311-1-record-clean-recheck-results-and-keep-one-recheck-paragraph-in-asks.md)）。実装は`src/application/supervise/recheck.rs`（supervisor側）と`src/domain/recheck.rs`（記録の形）。

## 対象と起動

- 対象: `awaiting_integration`で`result_commit`を持つrunのうち、leaseの無いもの（`approve_landing`のaskの答えやrecoverを待つもの）と、このsupervisorがslotに着地待ちとして持つもの（着地slotを待つものと、終わったら着地する終了依頼を待つもの）。
  reviewとreviseの途中のrunはpassの時のmerge-treeの判定（ADR-0027の決定4）に任せ、他のprocessがleaseを持つrunは見ない。
- 起動（supervisorの着地）: slotが終わって`integrated`のrunを返したとき、recheckを予定にする（`note_landed`）。
  予定はきっかけだけで、どの着地の後のrecheckかは、始めるときにmainがどの着地のcommitかで決める（下の「着地の名指し」。予定が残る間に直接の`integrate`がmainを動かせば、その着地を名指す）。
  loopの各回の`recheck_pass`が、走っているrecheckが終わっていれば結果を記録し、走っていなければ予定のものを始める。
  予定のものも、このsupervisorが今のmainをすでに確かめていれば（下のmainの動きのrecheckが先に見ていれば）始めない（下の排他の項）。
  同時に走るのは（queueで）1つで、走っている間の着地は最新の1件にまとまる。
  statusが`integrating`のrunがある間と、lockが取れない間は始めず、予定は残して次の回にやり直す。
- 起動（mainの動き、ADR-t1310-1）: 予定が無く何も走っていない回に、`recheck_pass`がlanding branchの先端を読み、最新の`landing_recheck_finished`の`main`と比べる。
  同じで、それがこのsupervisorのrecheckなら確かめ済み。
  同じでもほかのsupervisorのrecheckなら、そのsupervisorから見えなかった、このsupervisorがslotに着地待ちとして持つrunだけをまだ確かめる（下の排他の項）。
  違っていれば対象を集め、headがすでに今のmainを含むrun（`is_ancestor(main, head)`。そのまま着地できる）を除き、残りがあれば今の先端に対してrecheckを始める。
  比べる相手はeventから読むので、execの引き継ぎや起動し直しを越えて残り、止まる前に動いたmainも新しいsupervisorが最初の回で確かめる。
  processは、このsupervisorがrecheckを終えた先端（`Rechecks::checked`。自分のrecheckを記録したときと、最新の記録が自分のものと読んだとき）を覚え、先端が変わらない間はqueueのeventを読まない。
  対象が無い（無いか、全部がmainを含む）と分かった先端は、そのとき見た対象のrunとheadの組と一緒に覚え（`Rechecks::idle`）、同じ先端で同じ組なら見直さないが、後からleaseの無い着地待ちになったrun（reviewや終了を終えてleaseを手放したもの）があるか、headが変われば、同じ先端のままでも見直す（対象が無かったことで、そのmainを確かめ済みにはしない）。
  同じ先端を見直すときは、このsupervisorがslotに着地待ちとして新しく持ったrunは見ない（passしたreviewの衝突のprecheck（ADR-0027の決定4）が今の先端に対して確かめた後なので）。
  どの起動でも、headをすでにそのmainに対して判定されたrun（同じ`main`と`head`の`conflict_precheck`・`integration_deferred`・`landing_recheck_failed`・`landing_recheck_clean`がある）は除く: 見つかったことは、sessionへの依頼・askか・resume・着地のpark・askの段落で扱われていて、確かめ直すとaskの答えを待たずにresumeへ回すか、同じきれいな結果を記録し直すことになる。
  statusが`integrating`のrun（このsupervisorの着地でも直接の`integrate`でも）がある間は始めない（mainを動かした後`run_integrated`を記録する前の着地を、着地のrunの無い動きと取り違えないため）。
  止まったまま`integrating`に残ったrunがあると、それが片付くまでrecheckは始まらない。
  途中の失敗とlockの空き待ちは、何も覚えずに次の回にやり直す。
- queueのsupervisorの間の排他（ADR-t1310-1）: recheckはqueue dirの`recheck/lock`の排他の`flock`（`RunFiles::try_lock`。待たずに取る）を取ってからだけ始まり、lockは結果を記録した後（`landing_recheck_finished`の後）に手放す。
  supervisorの着地の予定もmainの動きも同じlockを取るので、supervisorが複数でもscratch worktreeとbuildの出力のdirを使うrecheckはqueueで同時に1つ。
  取れなければ始めず、予定は残して次の回にやり直す。
  予定がlockの空きを待つ間は、slotが空でもloopは終わらない（走っているrecheckと同じ扱い。待つのはほかのsupervisorのrecheckが終わるまで）。
  lockを取った後に、最近の`landing_recheck_finished`（`CHECKED_LOOKBACK`）から同じmainを誰が確かめたかを読む: このsupervisorなら何もしない。
  ほかのsupervisorなら、このsupervisorがslotに持つrun（ほかのsupervisorからは見えない）だけを確かめる。
  だれも確かめていなければ対象の全部を確かめる。
  mainの動きを見るsupervisorを1つに選ぶことはしない（drainやhandoffのsupervisorはrecheckを始めないので、生きているほかのsupervisorがmainの動きを確かめる）。
  空き容量のための掃除も同じlockを待たずに取れたときだけ`recheck/`の下の宣言ごとのbuildの出力のdirの中身を消し、消し終わるまで持つ（[空き容量を確かめる](disk-space.md)の「queueのdirのビルドのキャッシュ」）。
  recheckが走る間は掃除がそのdirに触れず、掃除が消す間はrecheckがlockを取れないので始めず、次の回にやり直す。
  - 着地の名指し: 始めるときのmainが最新の`run_integrated`の`result_commit`なら、そのrunを着地として名指し、`landing_recheck_finished`もそのrunに記録する（予定を作ったのがこのsupervisorの別の着地でも）。
    そうでなければ着地のrunの無い動きとして扱う（下の項）。
  - 着地のrunの無いmainの動き（dagqを通さないpushなど）では、着地のrunを名指さず、`landing_recheck_finished`は確かめた最初のrunに記録する（queue自体のeventは使わない）。
    `stats`のrecheckの数はその記録のtaskで数える。
    drain・handoffの回は新しく始めない。
    handoffは走っているrecheckが終わるのを待ってからexecする（commandが次のprocessのscratch worktreeで走り続けないように。予定だけのrecheckは捨てる）。
    handoffやadoptで作り直したrunが、recheckがresumeさせて`approve_landing`の答えを待つrun（最新のparkが`landing_recheck_failed`で、閉じていない`approve_landing`のaskがある）なら、reviewをやり直さずに答えを待つ（`adopt_review`）。
    対象が無ければ何も記録しない。
- recheckが走っている間と、結果を記録した回は、slotが空でもloopは終わらない（observerやplan reviewと同じ扱い。記録した回の次の回が、parkしたrunをresumeする）。

## 確かめ方

recheckのthreadが対象を1件ずつ確かめる（`recheck_runs`）。

1. `git merge-tree`でmainとheadを合わせる（`Repository::merged_tree`）。
   衝突すれば`rebase_conflict`で、衝突したpathを持つ。
2. 衝突が無く、main checkoutの`dagq.toml`の`[recheck] command`（[Run environment](run-environment.md)）があれば、mergeした木を`commit-tree`でmainの上の1 commitにし（refは作らない）、queue dirの`recheck/worktree`に`Repository::checkout_scratch`で出す。
   そこで`/bin/sh`でcommandを実行する。
   envはそのrunの`[run.env]`に、mergeした木に当たる宣言のbuildの出力（`recheck/`の下の宣言ごとのdirとそれを指すenv）を上書きしたもの（`recheck_env`、[Toolchain profiles](../toolchain-profiles.md)の「buildの出力の置き場」）。
   runtimeはその宣言のdirだけを作り、`[toolchains]`を宣言しないか当たる宣言が無ければenvを足さずdirを作らない。
   同じenvの宣言が複数当たれば、commandを宣言ごとのdirで順に流し直し（毎回mergeした木を出し直す）、落ちた回で止めてその回の失敗をfindingにする。
   出力はrun dirのlogに書く（`recheck_runs`）。
   非0の終了は`verification_failed`である。
   失敗はintegrateの検証と同じ分類（[integrate](integrate.md)）で、hostの失敗とコードの失敗に分ける。
   verifierの上限での打ち切りは実行できなかったものとせず、時間切れのhostの失敗として扱う。
   `[recheck] paths`があれば、2はmergeした木とmainの差分のpathがどれかのglobに当たるrunにだけ行う（`recheck::runs_command`、[ADR-t2032-1](../../adr/2026-10-07-t2032-1-run-the-recheck-command-only-on-runs-touching-recheck-paths.md)）。
   当たらないrunは1だけを見て、scratch worktreeも出さない。
   workerがcontainerで動くrunでは、mergeした木のcommitをscratch worktreeに出さず、commandをintegrateのverificationと同じ形のcontainer（[Container runtime](../container-runtime.md)の「verificationの実行」）で流す（[ADR-t2424-1](../../adr/2026-10-17-t2424-1-a-container-runs-landing-recheck-command-runs-in-the-verification-container.md)）。
   そのcontainerを使えないとき（verificationのruntimeがhost、containerが使えない、用意できない、podmanがcontainerを起動しない）はhostにもどこにも流さない（下の「見つかったとき」）。
3. `[recheck]`が無いか、`[run.env]`のprogramが見つからない間（ADR-0049の決定9）は1だけを見る。Gitやcommandが実行できなかったrunは`errors`に数え、runには何も記録しない。

buildの出力のdirは宣言ごとに1つで、recheckは直列なので、同時にそれを使うのは1本だけ。
worktreeとbuildの出力のdirはqueue dirの`recheck/`に残り、次のrecheckが使い回す。
buildの出力のdirの中身は、空き容量のための掃除だけがrecheckの走っていない間に消し、次のrecheckが作り直す。

## 見つかったとき

記録はloopのthreadで行う（`apply_recheck`）。runのheadかstatusがrecheckの後に変わっていれば何もしない。

- **hostの失敗**（[ADR-t639-1](../../adr/2026-09-27-t639-1-retry-verification-failures-of-the-host-once.md)をrecheckにも当てはめる）: commandがhostの分類（`FailureClass::is_environmental`）で落ちたfindingは、コードが着地するかを何も言わない。
  leaseの無いrunもこのsupervisorがslotに持つrunも、parkせず（resumeを使わず）`held`にもせず、`awaiting_integration`のまま置く。
  `landing_recheck_failed`はparkもholdもしなかった失敗として残り、`park_held_by_recheck`はこれを読まないので、slotのrunはそのまま着地へ進む。
  integrateがhostの失敗で控えたrun（`integration_held`）は、控えとinboxのattentionを保つ。
  containerのrunのcommandを流すcontainerが無かったfindingも同じに扱い、`landing_recheck_failed`に流さなかった理由を残す（ADR-t2424-1）。
  衝突とコードの分類の失敗だけが次の2つに進む。
- **leaseの無いrun**: `park_rechecked`が1 transactionでrunを`awaiting_integration` → `needs_session`にし、parkした失敗として`landing_recheck_failed`を記録する。
  次のpassの`resume_candidates`（[`needs_session`](needs-session.md)）が、ふつうのresumeとして拾い、効く優先度の順で空いたslotを受ける。
  依頼文は`ResumeKind::Recheck`で、手順は着地の延期と同じrebaseと、commandの失敗ならrebase後にそのcommandを手元で流して直すこと。
- **このsupervisorがslotに持つrun**: `landing_recheck_failed`をslotが持つ失敗として記録するだけにする。
  そのrunが着地slotを待って取る直前に`park_held_by_recheck`が、最新の`landing_recheck_failed`がslotが持つ失敗で、確かめたmainとheadが今のmainとrunのcommitに一致するかを見て、一致すればleaseの無いrunと同じ遷移（このsupervisorのtoken）でleaseを手放して`needs_session`にし、同じ内容をparkした失敗としてもう1度記録する。
  mainがさらに動いていれば着地を試みる。
- **きれいなrun**（ADR-t1311-1）: merge-treeが衝突せず、`[recheck] command`があればそれも通ったrunのうち、leaseが無いかこのsupervisorがslotに持つものに`landing_recheck_clean`を記録する（payloadは`recheck::clean_payload`）。
  statusは変えない。
  - `paths`で飛ばしたrunもきれいなrunで、記録はcommandが無いか`[run.env]`のprogramが見つからずに流せなかったものと分ける。
    commandを流さなかったきれいな結果を確かめ直すかを決めるときは、`paths`で飛ばしたものを確かめ済みのまま数える。
- **askの段落**: 失敗でもきれいでも、runの閉じていないask（答えの有無を問わない）のquestionのrecheckの段落を最新の結果1つに置き換え、`ask_updated`を記録する（置き換えは`recheck::noted_question`）。
  recheckの段落は`Landing recheck: `で始まる段落（空行で区切ったもの）で、置くときは前のものを全部除いてからquestionの末尾に置く（ADR-t1311-1以前のバイナリが積み重ねた段落も除く）。
  置き換えた結果が前のquestionと同じなら書き直さず、`ask_updated`も記録しない。
  askは閉じない。
  - hostの失敗（`recheck::skipped_ask_note`）: hostがcommandにコードを判断させなかったので、runはresumeせず着地を待ち続けることを伝える。
    containerが無かったfinding（`recheck::refused_ask_note`）は、流さなかった理由と、何も判定せずに着地を待ち続けることを伝える。
  - 失敗（`recheck::ask_note`）: 見つかったことと、答えを待たずにrunをresumeすること（`held`ならsessionが終わったときに着地の代わりにresumeへparkすること）を伝える。
  - きれい（`recheck::clean_ask_note`）: 確かめたmainの短いcommitでrunがまだきれいに着地することと、commandを流したかを伝える。
- **resumeの後**: resumeが解決して`validating`を通ったrunに閉じていない`approve_landing`のaskがあれば、reviewをやり直さず、sessionに終了を依頼し（`turns/exit`）、wrapperが終わったことを確かめて残りをhandleで止め（`stop_run_session`。ADR-t1433-3より前にworkspaceで開いたsessionのIDは閉じずに人に任せる）、leaseを手放して`awaiting_integration`で答えを待つ。
  答えは`apply_landing_answers`が適用する。
  `integration_approved`のあるrunはreviewを経ずに着地へ進む。
- **resumeの数え方**（ADR-0068の決定5、`domain::resume`）: parkした失敗の`landing_recheck_failed`はparkのイベントで、code `rebase_conflict`ならreviewのverdictを問わず衝突だけのresume（数えるresumeの上限に数えず`[resume] conflict_only_limit`で止める）、`verification_failed`なら数えるresume。
  使い切ったときの引き継ぐretryは、reviewがpassしたかapproveされたrunだけ。

## 記録

- 待つrunごとの結果は、着地しなくなったrunとhostの失敗で判断できなかったrunの`landing_recheck_failed`（上の「見つかったとき」）と、きれいなrunの`landing_recheck_clean`（ADR-t1311-1）がrunに持つ。
- recheckが終わるたびに、mainを動かした着地のrun（着地のrunの無いmainの動きなら確かめた最初のrun）に`landing_recheck_finished`を記録する（欄は`supervise::recheck::Tally`）。
  hostの失敗はコードの失敗とは別に数え、resumeにもheldにも数えない（`supervise::recheck::Tally`）。
- [`status`](commands.md)は最新の`landing_recheck_finished`の中身を出す。
- [`stats`](commands.md)は、窓のrecheckの件数とfindingごとのrunを出す（`domain::stats::LandingRechecks`）。
  slotが持つrunのparkは新しいfindingに数えず、resumeには数える。
  hostの失敗は、コードの失敗にもresumeにも数えず別に数える。
- [`stats`](commands.md)は、着地したrunをreviewの後に人かrecoverを待ったかで分け、群ごとにこのrecheckが着地の前に見つけた衝突と着地のrebaseの衝突のrunを数える。
- `landing_recheck_failed`はcode（ADR-0034）を持つので、`needs_session`のrunのerrorのcodeにも出る。
  `stats`のhotspotはこのイベントを数えない（着地のrebaseとpassの時のprecheckの衝突だけ）。
