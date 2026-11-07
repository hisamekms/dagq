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
掃除の責務と入口は `application::supervise::host_ops::cleanup` のmodule doc、候補の読み取りは `SqliteQueue::ended_run_worktrees` が持つ。
生きているsupervisorのleaseやslotを持つrunを通常の掃除で奪わず、taskが終わっていなければ次のrunやresumeが引き継ぐworktreeとbranchを残し、ビルド成果物だけを消す。
`--repo`のcheckoutとその祖先、他のworktreeのadmin dirとbranchには触れない。
削除の安全性とrepairの境界を知りたいときは `cleanup::owns_admin` を読む。

Gitはworktreeのpathの最後の要素から管理記録の名前を採り、消えた記録の名前を使い回すので、dirだけ残ったworktreeの`.git`が他のrunを指しうる。
runとscratchの名前は互いに重ならないようにし、新しいaddが古い配置の記録の名前も使い回さないようにする（入口は `domain::worktree_name`）。
scratchの命名は `domain::recheck_scratch_name`、restoreの互換は `replan_snapshot::legacy_scratch_worktree` を読む。
古い配置のrunもresume・検証・後片付けで見つけて扱う（入口は `RunPaths::with_layout_of`）。
同時のaddがGitの記録の競合で落ちたときのやり直しは `infrastructure::adapters::worktree_command` を読む。

消す前にそのrunのworktreeから実行されるプロセスを止める（入口は `HostOpsState::request_cleanup`）。
着地したrunの削除も同じ境界を守るが、Gitの削除に失敗してdirだけ残る場合は、他のworktreeの記録を指しうる`.git`を残さない（入口は `integrate::remove_landed_worktree`）。
失敗した掃除は他の候補を妨げず、次の掃除で再び試す。
消したものはもう無いので何度見てもよく、片付け終わったrunは同じsupervisorの次の掃除で候補にしない（下の「片付け終わったrun」）。

## 終わっていないrunのビルド成果物

人の答えを待つrunなどがビルド成果物を持ち続けると、空き容量が足りずclaimと着地が控えられる（[空き容量](claim-hold.md#空き容量)）。
誰も作業していないrunの答え待ちなら通常の掃除で、他の待ちなら空き容量のための掃除でビルド成果物を消す。
対象と区別を知りたいときは `WorktreeCleanup` を読む。
宣言したprofileのビルド成果物だけを消し、Gitが追跡するソースは残す（入口は `cleanup::remove_build_outputs`、宣言の責務は[Toolchain profiles](../toolchain-profiles.md#buildの出力の置き場)）。

leaseやslotを持つrunには通常は触れない。
例外は、何も動いていない答え待ちと着地slot待ちのrunを、leaseを持つsupervisor自身が掃除に渡す場合である。
答え待ちの受け渡しは[人の答えを待つrun](waiting.md#空き容量のための掃除)を読む。
**着地slotを待つrun**の受け渡しは `Supervisor::landing_worktrees`、予約と着地の排他は `cleanup::Cleaning` を読む。
着地が始まったrunのビルド成果物は残し、掃除中は着地もe2eもparkも始めない。
人が打つ`integrate`はこの予約を見ない。
消した後に着地やresumeが始まれば、検証とworkerがsccacheでビルドし直して進む。

全runを見直す通常の掃除では、buildの止まっているrunの古いcacheだけを消す（入口は `cleanup::remove_stale_entries`）。
rustcのlockを取らないので、作業中・検証・review・e2eの最中のrunには触れない。
次のbuildで戻るので、空くのはbuildの止まっている間だけである。
消したrunを再び候補にする条件は `cleanup::leave_pruned` を読む。
古いcacheを消したbytesはrunの大きさの見積もりに数えない（[claimを控える](claim-hold.md#閾値)）。
空き容量のための掃除は、これらのrunのビルド成果物を丸ごと消す。

終わったrunに付随するファイルの掃除は `cleanup` のmodule docから辿れる。
run_dirの記録は残し、scratchpadは求めたdirの外や人のsessionのものに触れない。
一時ファイルの置き場所は `application::RUN_TMP_DIR`、hostの検証の短いdirの理由は `application::host_verification_tmp_path` を読む。
workerのpromptはproviderによらず一時ファイル・使い捨てのrepository・worktreeの外のbuildを`$TMPDIR`の下に置くよう案内し、他のbuildの場所はrepositoryの指示に委ねる（このrepositoryは[手元の検証](../../development/local-checks.md#workerの手元の検証)）。
終わっていないrunの一時dirは、次のturnが使い続けるか今のprocessからは判断できないので、ビルド成果物の掃除でも残す。
runtimeが決めたdirの外にworkerが作ったものは対象外で、人かinboxが消す。

queue serviceのクライアントモードはDBのpathを渡さないだけで、置き場所と片付けの境界を変えない。
containerの`/tmp`はcontainerと一緒に消えるが、run_dirは同じpathでmountされ、Claudeのworkerのturnにも同じ`TMPDIR`が届くので、run_dir側から片付ける。

## loopの外で掃除する

数GBのビルド成果物の計測と削除はloopの外のthreadが行い、loopは通常は完了を待たないので、生きているrunのidle・receipt・stallの検知が遅れない。
1件の削除だけで数十秒かかりうり、件数や時間の予算では途中で止められないため、周回ごとの予算では区切らない。
jobは同時に1つだけで、予約のlockと削除前の読み直しにより、掃除とleaseの取得のどちらが先でも掃除中のworktreeのrunはclaimされない（入口は `cleanup::CleanupWatch`）。
予約で先送りしたrunと空き容量のためのjobはloopがrunと同じく待つので、`supervise --once`も掃除後のtriage・resume・claimへ進める。
頼み方とeventの記録は `HostOpsState::request_cleanup`、掃除を頼む契機は `Supervisor::request_cleanup` と `Supervisor::sweep_ended_runs` を読む。
空き容量のjobと残りのjobの待ち・claimと着地の控え・queueのbuild cacheの責務は[claimを控える](claim-hold.md#空き容量)が持つ。

**片付け終わったrun**を候補から外す条件は `cleanup::pick_candidates` を読む。
この印はqueueではなくプロセスの中で持つ。
runが再びworktreeを持ちうるleaseと状態の変化を、候補の読み取りとlease取得のlockで確かめられ、新しいsupervisorは最初の掃除で全runを1回ずつ確かめ直せる。
queueに印を置くと、leaseを取るすべての書き込みが印を消す責務を持ってしまう。
残る隙として、別のsupervisorが2回の掃除の間にworktreeを作り直して同じ状態で返すと、このプロセスが入れ替わるまで候補に戻らない。
このrepositoryではqueueごとの常駐supervisorは1つで、handoffの間だけ重なり、新しいプロセスは印を持たないので、この隙を受け入れる。

stopとhandoffでも空き容量のjobは選んだ分を終える。
通常のjobと溜まった頼みの扱いは `cleanup` のmodule doc、handoffの取り消しは[Handoff](handoff.md)を読む。
計測は削除の可否を決めるものではなく、計測の失敗で掃除が毎周回止まることはない（入口は `cleanup::Size`）。

sessionの停止の成否は `TaskRun::workspace_closed_at` を読む。
止めていないsessionをcleaned扱いにせず、停止の再試行は`doctor`と`recover`が扱う（停止の入口は[後始末](background-recovery-job.md#後片付け)）。

## containerのrunのclone

containerで動くrunのclone（[Container runtime](../container-runtime.md#git)）はworktreeと同じ契機で、gitを走らせずsymlinkを辿らずに消え、ビルド成果物の掃除は当てない（[ADR-t2179-1](../../adr/2026-10-12-t2179-1-control-side-runs-no-git-in-a-container-runs-clone-and-lands-in-its-own-worktree.md)決定7）。
作りかけのcloneは、workerの起動をqueueが記録する前なら消し作り直す。
