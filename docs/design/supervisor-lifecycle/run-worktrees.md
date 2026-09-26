---
id: design-supervisor-lifecycle-run-worktrees
type: design
title: "Run worktrees"
status: current
created: 2026-09-26
updated: 2026-09-27
last_verified: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
---

# Run worktrees

終わったrunのworktreeが占めるディスクを、supervisorが自動で空ける（task 376。goal 34のconstraintsの人の決定（2026-09-25）。2026-09-25に、消されずに残ったrunの`target/`（1 runあたり0.6〜3.4GB、合計20GB超）で空きが尽き、integrateの検証が`No space left on device`で落ちた）。対象は、slotに居ないrunのうち、生きているsupervisorのleaseが無く`integrated`・`succeeded`・`failed`・`interrupted`で終わったもの（staleなleaseは無いものとして扱う。[Run workspaces](run-workspaces.md#run-workspaces)の掃除と同じ。task 396）と、leaseが無くtaskが`completed` / `canceled`になったもの（runの状態を問わない）（`ended_run_worktrees`）。worktreeのpathはrun_dirの下の`<runs>/<run-id>/worktree`で、`--repo`のcheckoutとその祖先には触れない。消したものはもう無いので、何度見てもよい。

- **taskが`completed` / `canceled`**: worktreeとbranchを消す（`git worktree remove --force`、branchがあれば`git branch -D`）。`worktree_removed`（`path`、`branch`、`bytes`、`by: supervisor`、`reason`: `task_completed` / `task_canceled`）を記録する。前のrunのworktreeも、着地した（`integrate`がworktreeを消せなかった）runのものも同じ。run_dirとその記録、着地したcommitの`refs/dagq/runs/<run-id>`は残す
  - `git worktree remove`が失敗したら（queueの`rebind`の後などで、worktreeの`.git`が古いcommon dirを指している）、`git worktree repair <worktree>`で直してからもう一度removeする。そうして消せたら`worktree_removed`に`repaired: true`を足す。repairか2回目のremoveも失敗すれば、最初の失敗と合わせて`cleanup_failed`にする（task 405）
  - worktreeのディレクトリがもう無いのにbranch `dagq/<run-id>`が残っていれば、`git worktree prune`（Gitはpruneするまで消えたworktreeを覚えていて、そこにcheckoutされたbranchを消させない）の後に`git branch -D`で消し、`worktree_removed`（`bytes: 0`、`worktree_missing: true`、他の欄は同じ）を記録する。branchが無ければ何もしない（task 405。branchの一覧は1回の掃除で1回だけ読む）
- **それ以外（taskがまだ`in_progress` / `ready`など）**: 次のrunやresumeが引き継ぐかもしれないのでworktreeとbranchは残し、worktree直下のビルド成果物（`target/`と`llvm-cov-target/`。`cargo llvm-cov`は既定で`target/llvm-cov-target`に作る）だけを消す。Gitがその下のfileをtrackしていれば消さず、linkはたどらない。`build_outputs_removed`（`paths`、`bytes`、`by: supervisor`）を記録する。ソース、commit、run_dirは残る。triageを待つrunも対象で、resumeされたら作り直す。worktreeが無ければ何もしない

`bytes`は消したものがディスクで占めていた量（blocks × 512、hard linkは1回だけ数える）。どちらのeventも人の手の代わりにruntimeが直したもので、goal 34の自動修正の件数に数える。

空き容量がclaimか着地の検証に足りないときも、supervisorは同じ掃除と`git worktree prune`を走らせ、何か消えれば`auto_repaired`（`repair: disk_cleanup`）を記録する（[空き容量を確かめる](disk-space.md)、task 377）。その閾値は直近の`build_outputs_removed`の`bytes`の最大値から決める。

## loopの外で掃除する

数GBの`target/`の計測（`tree_size`）と削除はloopの外のthread（掃除のjob）が行い、loopは完了を待たない（task 405。入れ替え後の最初の掃除が過去のrunを全部歩いてloopが長く止まり、生きているrunのidle・receipt・stallの検知が遅れたため）。周回ごとの予算で区切る方式にしなかったのは、1つのworktreeの`target/`の削除だけで数十秒かかりうり、件数や時間の予算では1件の途中で止められないから。

- 下の契機は掃除を頼むだけ（`request_cleanup`）。jobが無ければloopがその場で候補（`ended_run_worktrees`からslotのrunを除いたもの、taskの指定があればそのtaskのrun）を選んでjobを起こし、jobが走っていれば頼みを溜めて（全runか、taskの集合）、jobが終わった後の周回で次のjobにする。jobは同時に1つだけなので、同じworktreeを二重に掃除しない
- jobが選んだrunは、jobがそのrunを通り過ぎるまで予約される。loopは終わったrunにleaseを取る前（triageの`begin_triage`、resumeの`begin_resume`）に予約のlockを取り、予約されたrunはその周回は取らない（loopの中で同期に掃除していたときと同じく、掃除がtriageより先になる）。そうして取らなかったrunがあれば、loopはそのjobをrunと同じく待つ（`supervise --once`がjobの後にtriageやresumeをしてから終わるように）。jobは候補ごとに、同じlockの中で消す前にqueueを読み直し（自分の接続で`ended_run_worktrees`）、選ばれたときと同じ状態で候補に残っているものだけを掃除する。選ばれた後にleaseが付いた（別のsupervisorがclaimした）run、状態が変わったrunやtaskは飛ばし、次の掃除が選び直す。どちらが先でも、掃除中のworktreeのrunがclaimされることはない
- eventはloopが記録する。loopは周回の最初にjobが終わっていればjoinし、jobが返した結果から`build_outputs_removed`・`worktree_removed`・`cleanup_failed`をtask 376と同じpayloadで記録する（`cleanup_failed`はプロセスごとにworktreeあたり1回）
- 空き容量のための掃除（[空き容量を確かめる](disk-space.md)）も同じjobに乗る。そのjobは最後に`git worktree prune`を行い、終わった周回で空きを読み直して`auto_repaired`を記録する。空きが足りずにそのjobを待つあいだ、claimと着地は控えるが`claim_held` / `landing_held`もaskも記録せず、終わった後の周回で読み直した空きで判定する。loopはこのjobだけはrunと同じく待つ（`supervise --once`がそのjobの後の周回でclaimできるように）。空き容量のための掃除を頼んだときに別のjobが走っていれば、そのjobを空き容量のための掃除として扱い（claimと着地はそのjobだけを待ち、消した分を`auto_repaired`に数える）、残り（そのjobが選ばなかったrunと`git worktree prune`）は控えずに次のjobで行う
- stopかhandoffでは、jobは今のworktreeを終えたところで止まり、溜めた頼みは捨てる（残りは次のsupervisorの最初の掃除が拾う）。handoffのexecはjobの終わりを待つ。loopが終わるときは、走っているjobと溜めた頼みのjobを待ってeventを記録する（errorで終わったときは、jobは今のworktreeを終えたところで止まり、それを待って記録する）

消す契機は、supervisorのslotが終わったとき（`integrated`・`failed`・`interrupted`）、triageが終わったとき、triageの`decide` askとlandingの`approve_landing` askのanswer（`cancel`を含む）を適用したとき（そのtaskのrunだけ）と、上の掃除と同じ回（`sweep_ended_runs`、workspaceを閉じた後、全runを見直す）。手での`integrate`は着地したrunのworktreeを自分で消し、手での`recover`・人の`ready` / `cancel`・supervisorの外で終わったrunは次の掃除が拾う。失敗は`cleanup_failed`（`path`、`message`、`by: supervisor`）にして残りを続け、次の掃除で再び試す（supervisorのプロセスごとにworktreeあたり1回だけ記録する）。

closeの成否は`task_runs.workspace_closed_at`で表す。nullは「閉じたことを確認していない」で、closeの失敗だけでなく、cmuxが閉じた後にDBへ書けなかった場合も含む。closeの失敗は`cleanup_failed`イベントと`last_error`に残るが、run状態は変えない。閉じていないworkspaceをcleaned扱いにせず、再試行は`doctor`/`recover`で扱う。
