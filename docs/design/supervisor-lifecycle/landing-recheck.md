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
---

# Landing recheck

> [ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)決定3（ADR-0068決定4をamends）のとおり、resumeの後は終了の依頼でbackgroundのwrapperを終わらせ、終わったことを確かめ、残っていれば止めてからleaseを手放す（task 1440。runはworkspaceを開かない）。worker の打鍵と stuck_exit の監視は task 1437 で廃止した。

supervisorが着地させたrunが`run_integrated`で終わるたびと、mainが最後にrecheckを終えたmainと違うとき（直接の`integrate`、handoffや再起動の間の着地、dagqを通さないpush）に、着地待ちのrunをそのmainに対して先回りして確かめる（[ADR-0068](../../adr/0068-recheck-waiting-runs-after-each-landing.md)、きっかけは[ADR-t1310-1](../../adr/2026-10-03-t1310-1-recheck-whenever-main-moves-past-the-last-recheck.md)が広げた）。着地しなくなったrunは、askの答えや`integrate`の順番を待たずにresumeへ回る。きれいに着地すると確かめたrunにもその結果を記録し、runの開いているaskは最新の結果を1つの段落で見せる（[ADR-t1311-1](../../adr/2026-10-05-t1311-1-record-clean-recheck-results-and-keep-one-recheck-paragraph-in-asks.md)）。実装は`src/application/supervise/recheck.rs`（supervisor側）と`src/domain/recheck.rs`（記録の形）。

## 対象と起動

- 対象: `awaiting_integration`で`result_commit`を持つrunのうち、leaseの無いもの（`approve_landing`のaskの答えやrecoverを待つもの）と、このsupervisorがslotに着地待ちとして持つもの（`AwaitingSlot`、`AfterExit::Land`の終了依頼を待つもの）。reviewとreviseの途中のrunはpassの時のmerge-treeの判定（ADR-0027の決定4）に任せ、他のprocessがleaseを持つrunは見ない。
- 起動（supervisorの着地）: slotが`Step::Done`で`integrated`のrunを返したとき（`note_landed`）、recheckを予定にする（`Rechecks::due`）。予定はきっかけだけで、どの着地の後のrecheckかは、始めるときにmainがどの着地のcommitかで決める（下の「着地の名指し」。予定が残る間に直接の`integrate`がmainを動かせば、その着地を名指す）。loopの各回の`recheck_pass`（`try_recheck`）が、走っているrecheckが終わっていれば結果を記録し、走っていなければ予定のものを始める。予定のものも、このsupervisorが今のmainをすでに確かめていれば（下のmainの動きのrecheckが先に見ていれば）始めない（下の排他の項）。同時に走るのは（queueで）1つで、走っている間の着地は最新の1件にまとまる。statusが`integrating`のrunがある間と、lockが取れない間は始めず、予定は残して次の回にやり直す。
- 起動（mainの動き、ADR-t1310-1）: 予定が無く何も走っていない回に、`try_recheck`がlanding branchの先端（`Repository::main_head`）を読み、最新の`landing_recheck_finished`の`main`と比べる。同じで、それがこのsupervisorのrecheck（payloadの`supervisor`が自分のtoken）なら確かめ済み。同じでもほかのsupervisorのrecheckなら、そのsupervisorから見えなかった、このsupervisorがslotに着地待ちとして持つrunだけをまだ確かめる（下の排他の項の`ByOther`）。違っていれば対象を集め、headがすでに今のmainを含むrun（`is_ancestor(main, head)`。そのまま着地できる）を除き、残りがあれば今の先端に対してrecheckを始める。比べる相手はeventから読むので、execの引き継ぎや起動し直しを越えて残り、止まる前に動いたmainも新しいsupervisorが最初の回で確かめる。processは、このsupervisorがrecheckを終えた先端（`Rechecks::checked`。自分のrecheckを記録したときと、最新の記録が自分のものと読んだとき）を覚え、先端が変わらない間はqueueのeventを読まない。対象が無い（無いか、全部がmainを含む）と分かった先端は、そのとき見た対象のrunとheadの組と一緒に覚え（`Rechecks::idle`）、同じ先端で同じ組なら見直さないが、後からleaseの無い着地待ちになったrun（reviewや終了を終えてleaseを手放したもの）があるか、headが変われば、同じ先端のままでも見直す（対象が無かったことで、そのmainを確かめ済みにはしない）。同じ先端を見直すときは、このsupervisorがslotに着地待ちとして新しく持ったrunは見ない（passしたreviewの衝突のprecheck（ADR-0027の決定4）が今の先端に対して確かめた後なので）。どの起動でも、headをすでにそのmainに対して判定されたrun（同じ`main`と`head`の`conflict_precheck`・`integration_deferred`・`landing_recheck_failed`・`landing_recheck_clean`がある）は除く: 見つかったことは、sessionへの依頼・askか・resume・着地のpark・askの段落で扱われていて、確かめ直すとaskの答えを待たずにresumeへ回すか、同じきれいな結果を記録し直すことになる。statusが`integrating`のrun（このsupervisorの着地でも直接の`integrate`でも）がある間は始めない（mainを動かした後`run_integrated`を記録する前の着地を、着地のrunの無い動きと取り違えないため）。止まったまま`integrating`に残ったrunがあると、それが片付くまでrecheckは始まらない。途中の失敗とlockの空き待ちは、何も覚えずに次の回にやり直す。
- queueのsupervisorの間の排他（ADR-t1310-1）: recheckはqueue dirの`recheck/lock`の排他の`flock`（`RunFiles::try_lock`。待たずに取る）を取ってからだけ始まり、lockは結果を記録した後（`landing_recheck_finished`の後）に手放す。supervisorの着地の予定もmainの動きも同じlockを取るので、supervisorが複数でもscratch worktreeとtargetを使うrecheckはqueueで同時に1つ。取れなければ始めず、予定は残して次の回にやり直す。予定がlockの空きを待つ間は、slotが空でもloopは終わらない（走っているrecheckと同じ扱い。待つのはほかのsupervisorのrecheckが終わるまで）。lockを取った後に、最近の`landing_recheck_finished`（最新の20件）から同じmainを誰が確かめたかを読む: このsupervisor（payloadの`supervisor`が自分のtoken）なら何もしない。ほかのsupervisorなら、このsupervisorがslotに持つrun（ほかのsupervisorからは見えない）だけを確かめる。だれも確かめていなければ対象の全部を確かめる。mainの動きを見るsupervisorを1つに選ぶことはしない（drainやhandoffのsupervisorはrecheckを始めないので、生きているほかのsupervisorがmainの動きを確かめる）。
  - 着地の名指し: 始めるときのmainが最新の`run_integrated`の`result_commit`なら、そのrunとtaskを`landed_run_id` / `landed_task_id`にし、`landing_recheck_finished`もそのrunに記録する（予定を作ったのがこのsupervisorの別の着地でも）。そうでなければ着地のrunの無い動きとして扱う（下の項）。
  - 着地のrunの無いmainの動き（dagqを通さないpushなど）では、`landing_recheck_failed`と`landing_recheck_finished`の`landed_run_id` / `landed_task_id`をnullにし、`reason`を「after main moved without a dagq landing, the landing recheck found ...」にし、`landing_recheck_finished`は確かめた最初のrunに記録する（queue自体のeventは使わない）。`stats`の`landing_rechecks`はその記録のtaskで数える。drain・handoffの回は新しく始めない。handoffは走っているrecheckが終わるのを待ってからexecする（commandが次のprocessのscratch worktreeで走り続けないように。予定だけのrecheckは捨てる）。handoffやadoptで作り直したrunが、recheckがresumeさせて`approve_landing`の答えを待つrun（最新のparkが`landing_recheck_failed`で、閉じていない`approve_landing`のaskがある）なら、reviewをやり直さずに答えを待つ（`adopt_review`）。対象が無ければ何も記録しない。
- recheckが走っている間と、結果を記録した回は、slotが空でもloopは終わらない（observerやplan reviewと同じ扱い。記録した回の次の回が、parkしたrunをresumeする）。

## 確かめ方

recheckのthreadが対象を1件ずつ確かめる（`recheck_runs`）。

1. `git merge-tree --write-tree --name-only --no-messages -z <main> <head>`（`Repository::merged_tree`）。衝突すれば`rebase_conflict`で、衝突したpathを持つ。
2. 衝突が無く、main checkoutの`dagq.toml`の`[recheck] command`（[Run environment](run-environment.md)）があれば、mergeした木を`commit-tree`でmainの上の1 commitにし（refは作らない）、queue dirの`recheck/worktree`に`Repository::checkout_scratch`で出す（worktreeでなければ`git worktree prune`の後に`git worktree add --detach --force`、worktreeなら`checkout --detach --force`と`clean -ffdxq`）。そこで`/bin/sh`でcommandを実行する。envはそのrunの`[run.env]`（`${DAGQ_RUN_DIR}`はそのrunのrun dir）に、`CARGO_TARGET_DIR=<queue dir>/recheck/target`を上書きしたもの。出力はrun dirの`recheck-<mainの先頭12桁>.log`。非0の終了は`verification_failed`で、`command`・`exit_code`・`log_path`・`output_tail`（末尾2000文字）を持つ。
   `[recheck] paths`があれば、2はmergeした木とmainの差分のpath（`Repository::changed_paths(main, tree)`）がどれかのglobに当たるrunにだけ行う（`recheck::runs_command`、[ADR-t2032-1](../../adr/2026-10-07-t2032-1-run-the-recheck-command-only-on-runs-touching-recheck-paths.md)）。
   当たらないrunは1だけを見て、scratch worktreeも出さない。
3. `[recheck]`が無いか、`[run.env]`のprogramが見つからない間（ADR-0049の決定9）は1だけを見る。Gitやcommandが実行できなかったrunは`errors`に数え、runには何も記録しない。

この repositoryの`dagq.toml`には`[recheck]`の`command = "cargo check --locked --all-targets"`がある。task 529が、固定バイナリにtask 462の実装が入った後に足した。target（`recheck/target`）は1つで、recheckは直列なので、同時にそれを使うのは1本だけ。worktreeとtargetはqueue dirの`recheck/`に残り、次のrecheckが使い回す。

## 見つかったとき

記録はloopのthreadで行う（`apply_recheck`）。runのheadかstatusがrecheckの後に変わっていれば何もしない。

- **leaseの無いrun**: `park_rechecked`が1 transactionでrunを`awaiting_integration` → `needs_session`にし（`last_error`は`reason`）、`landing_recheck_failed`（`code`、`main`、`head`、`landed_run_id`、`landed_task_id`、`conflicts`か`command`・`exit_code`・`log_path`・`output_tail`、`reason`、`action: resumed`、`status: needs_session`）を記録する。次のpassの`resume_candidates`（[`needs_session`](needs-session.md)）が、ふつうのresumeとして拾い、効く優先度の順で空いたslotを受ける。依頼文は`ResumeKind::Recheck`（「waiting to land … the supervisor's landing recheck found that it no longer lands」）で、手順は着地の延期と同じrebaseと、commandの失敗ならrebase後にそのcommandを手元で流して直すこと。
- **このsupervisorがslotに持つrun**: `landing_recheck_failed`を`action: held`（`reason`付き）で記録するだけにする。そのrunが`AwaitingSlot`で着地slotを取る直前に`park_held_by_recheck`が、最新の`landing_recheck_failed`が`held`で`main`と`head`が今のmainと`result_commit`に一致するかを見て、一致すれば`park_rechecked`（このsupervisorのtoken）でleaseを手放して`needs_session`にし、同じ内容を`action: resumed`、`repeat: true`でもう1度記録する（`lease_released`の`reason`は`landing_recheck_failed`）。mainがさらに動いていれば着地を試みる。
- **きれいなrun**（ADR-t1311-1）: merge-treeが衝突せず、`[recheck] command`があればそれも通ったrunのうち、leaseが無いかこのsupervisorがslotに持つものに`landing_recheck_clean`（`main`、`head`、`command`（merge-treeだけならnull）、`landed_run_id`、`landed_task_id`（着地のrunの無いmainの動きならnull））を記録する（`record_recheck_clean`、payloadは`recheck::clean_payload`）。statusは変えない。
  - `paths`で飛ばしたrunもきれいなrunで、`command`はnullにし、`command_skipped: "paths"`を足す。
  - この印は、commandが無い・`[run.env]`のprogramが見つからずに流せなかったnullと、`paths`で流す要が無かったnullを分ける。
    commandを流さなかったきれいな結果を確かめ直すかを決めるときは、後者を確かめ済みのまま数える。
- **askの段落**: 失敗でもきれいでも、runの閉じていないask（答えの有無を問わない）のquestionのrecheckの段落を最新の結果1つに置き換え、`ask_updated`（`ask_id`、`kind`、`why`）を記録する（`AskStore::note_on_asks`、置き換えは`recheck::noted_question`）。recheckの段落は`Landing recheck: `で始まる段落（空行で区切ったもの）で、置くときは前のものを全部除いてからquestionの末尾に置く（ADR-t1311-1以前のバイナリが積み重ねた段落も除く）。置き換えた結果が前のquestionと同じなら書き直さず、`ask_updated`も記録しない。askは閉じない。
  - 失敗（`why: landing_recheck_failed`、`recheck::ask_note`）: `Landing recheck: <reason>. The supervisor resumes the run …`（`held`なら`… parks the run for a resume instead of landing it once its session has exited.`）。
  - きれい（`why: landing_recheck_clean`、`recheck::clean_ask_note`）: `Landing recheck: after task <task> (run <run>) landed, the landing recheck found that the run still lands cleanly on main <mainの先頭12桁>: git merges it without a conflict and "<command>" passes on main with the run merged in.`。commandを流さなかったときは`…: git merges it without a conflict (no command was run).`、着地のrunの無い動きなら冒頭は`after main moved without a dagq landing, …`。
- **resumeの後**: resumeが解決して`validating`を通ったrunに閉じていない`approve_landing`のaskがあれば、reviewをやり直さず、sessionに終了を依頼し（`turns/exit`）、wrapperが終わったことを確かめて残りをhandleで止め（`stop_run_session`。ADR-t1433-3より前にworkspaceで開いたsessionのIDは閉じずに人に任せる）、leaseを手放して`awaiting_integration`で答えを待つ（`AfterExit::Rest { close: true }`）。答えは`apply_landing_answers`がこれまでどおり適用する。`integration_approved`のあるrunはreviewを経ずに着地へ進む。
- **resumeの数え方**（ADR-0068の決定5、`domain::resume`）: `action: resumed`の`landing_recheck_failed`はparkのイベントで、code `rebase_conflict`ならreviewのverdictを問わず衝突だけのresume（`MAX_RESUME_ATTEMPTS`に数えず`[resume] conflict_only_limit`（既定5）で止める）、`verification_failed`なら数えるresume。使い切ったときの引き継ぐretryは、reviewがpassしたかapproveされたrunだけ。

## 記録

- 待つrunごとの結果は、着地しなくなったrunの`landing_recheck_failed`（上の「見つかったとき」）と、きれいなrunの`landing_recheck_clean`（ADR-t1311-1）がrunに持つ。
- recheckが終わるたびに、mainを動かした着地のrun（着地のrunの無いmainの動きなら確かめた最初のrun）に`landing_recheck_finished`（`main`、`landed_run_id`、`landed_task_id`、`command`、`checked`、`clean`、`conflicts`、`check_failed`、`errors`、`resumed`、`held`、`failed_runs`（`run_id`・`code`・`action`）、`duration_secs`、`supervisor`）を記録する。
- [`status`](status.md)は最新の`landing_recheck_finished`のpayloadを`landing_recheck`（`at`付き。まだ無ければnull）として出す。
- [`stats`](stats.md)の`landing_rechecks`は、`backend_failures`と同じ窓の`rechecks`、`runs_checked`、`conflicts`、`check_failures`、`resumed`と、findingごとの`runs`（`task_id`、`run_id`、`code`、`action`、`landed_task_id`）。`repeat`は新しいfindingに数えず、`resumed`には数える。
- [`stats`](stats.md)の`landing_waits`は、着地したrunをreviewの後に人かrecoverを待ったかで分け、群ごとにこのrecheckが着地の前に見つけた衝突と着地のrebaseの衝突のrunを数える（goal 39、task 1312）。
- `landing_recheck_failed`はcode（ADR-0034）を持つので、`needs_session`のrunの`last_error_code`にも出る。[Conflict thresholds](conflict-thresholds.md)の`conflict_hotspots`はこのイベントを数えない（着地のrebaseとpassの時のprecheckの衝突だけ）。
