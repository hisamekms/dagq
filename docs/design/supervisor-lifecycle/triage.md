---
id: design-supervisor-lifecycle-triage
type: design
title: "Triage (supervisor)"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-background-recovery-job
  - adr-0039
  - adr-0047
  - adr-t1818-2
---

# Triage (supervisor)

終わった run の復旧 job は failed / interrupted / resume_exhausted を扱い、生きている worker の stalled と非対話の idle_process は [生きているsessionの復旧job](background-recovery-job.md) が扱う。
復旧jobは判断を返す読み取りだけのjobで、状態を動かす責務はsupervisorにある。
workerのsessionが容量不足で落ちたrun（最後のturnの出力に検証の`disk_full`と同じerrnoの印があり、復旧がまだ始まっていない）は復旧jobに回らずaskも開かない。
空き容量がclaimを控える間は`failed`か`interrupted`のまま置き、控えが解けたpassで同じrunのままresumeする。
resumeは通常の回数に数え、使い切れば`resume_exhausted`で復旧jobに回る（`domain::full_disk`、`Supervisor::resume_full_disks`）。

## 候補と開始

孤児runの判定は `Supervisor::recover_dead_runs`（`src/application/supervise/execution/triage.rs`）が持つ。
死んだrunはinterruptedにし、readyへ直さない（孤児runを自動で再実行しない）。

復旧ラウンドの候補は同じmoduleの `Supervisor::triage_candidates`、eventからの判定は `domain::triage_state` が入口になる。
同じrunを2つのslotで進めないため、このsupervisorのslotにあるrunは、leaseが他へ移ってstaleになっていても候補にしない。
slotを外してjobを止める境界は [supervise](supervise.md#provisionと監視) が持つ。

空きslotを渡す列は [supervise](supervise.md#slotを渡す順とclaim) と [ADR-t1850-1](../../adr/2026-10-06-t1850-1-resumes-recovery-jobs-and-claims-share-one-line-by-effective-priority.md)、ラウンドの開始は `Supervisor::triage_run` が持つ。
復旧jobはrun slotを1つ使い、開始の確定は `SqliteQueue::begin_triage`（`src/infrastructure/runtime_store/recovery.rs`）で競合を解くので、同じrunを2つのsupervisorが取れない。

alertの回数を使い切った後はjobを起動せずescalateするので、providerの待ちにも止められない。
例外は、人がverifyを直してから回答したときの追加のラウンドである（[ADR-t883-1](../../adr/2026-09-30-t883-1-edit-ended-run-verification-before-inherited-retry.md)、判定は `domain::recovery::verify_fix_round`）。

## prompt

promptの資料と収め方は `prompt::recovery_prompt` のdoc comment、固定バイナリの共通の節は [復旧job](background-recovery-job.md#起動とprompt) が入口になる。
上限と切った資料を読む方法は [Prompt](prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限) が持つ。

## verdictの適用

判定から適用への入口は `Supervisor::act_on_recovery`、操作の前提は同じmoduleの `Supervisor::plan_ended` のdoc commentが持つ。
適用の確信度が高く、操作が1つで、今も前提が成り立つときだけruntimeが適用する。
適用時にleaseを失っていれば何も書かずに手放す。
失敗の原因の判定は適用の成否と独立に残る（[復旧job](background-recovery-job.md#適用)）。

commitのあるrunを最初からやり直すのは成果の破棄なので、自動では適用しない。
引き継ぐcommitの読み方は `Supervisor::own_commits`、登録の壊れたhost worktreeの判定は `run_git::host_head`、containerの事実の読み直しは [Container runtime](../container-runtime.md#git) が入口になる。

resumeは適用のtransactionでも再検査し、後のrunがあれば戻さない（`domain::telemetry::run_revivable`）。
終わったrunの未送信の計測をsweepが消すので、戻すとsessionの番号を数え直すという落とし穴がある（[計測](../measurement.md#送る口)）。
resumeの試行回数と新しいsessionへの引き継ぎは [needs_session](needs-session.md#needs_session) と共有する。

hand_offはbaseがすでに受け入れ条件を満たすことの [検証の経路](validation.md#already_satisfiedの検証) へ渡すだけで、job自身は閉じる判断をしない（[ADR-t1818-2](../../adr/2026-10-05-t1818-2-recovery-job-options-map-to-runtime-options-or-pass-back.md)決定3）。
前提の入口は `domain::recovery::hand_off_refusal`、適用後の休みの判定は `domain::satisfied::handed_off` のdoc commentが持つ。

## escalateとaskの回答

前提が崩れた判定や自動で扱えない判定は人のdecide askにし、runは動かさない。
askの組み立ては `Supervisor::escalate_ended`、jobのoptionの寄せ方と外し方は `domain::recovery::sort_job_options` のdoc commentが持つ（[ADR-t1818-2](../../adr/2026-10-05-t1818-2-recovery-job-options-map-to-runtime-options-or-pass-back.md)）。
jobのoptionを選ぶのは復旧jobに渡し直す依頼であり、runtimeの操作や完了の承認ではない。
人への通知はinboxのwatchが担う。

回答の適用は `Supervisor::apply_triage_answers` と、競合を解く `SqliteQueue::decide_triage` が入口になる。
人のretryは成果の破棄も選べる（[ADR-0039](../../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)決定7）。
自由な回答は人が読む。
optionの原文と一致しない操作名だけの回答は、適用されない自由な回答として閉じて答え直せなくなるのを防ぐため、何も書かずに断る（`asks::refuse_bare_action`）。
引き継ぐretryを直接選びたいときは下の [人の手での引き継ぎ](#人の手での引き継ぎ) を使う。

## ラウンドの終了と失敗

適用の記録は `SqliteQueue::finish_triage`、終了処理は `Supervisor::end_round`、終了したrunのaskの整理は `Supervisor::note_triaged` が入口になる。
verdictを適用済みのラウンドの終了処理が失敗しても、supervisor logに残すだけでtriage_failedにしない。
wrapperをbackgroundで止める境界は [ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)決定3、legacyのworkspaceを人が閉じる互換は [ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md) が持つ。

jobの失敗は `Supervisor::fail_recovery` が持つ。
triage_failedはattentionで、人の指示でinboxか人が復旧を決める（plannerはreadyを打てない）。
resumeされるか、下の [providerが使えない場合](#codexで動かす) を除き、同じrunをもう一度ラウンドにかけない。

## 再計画の診断

長期化したtaskの再計画の診断は、同じ復旧jobの経路を使う読み取りだけのjobで、run slotの外で動きrunのturnを止めない。
診断と回答を適用する責務はtrusted runtimeにある。
入口は `src/application/supervise/execution/replan_diagnosis.rs` のmodule docと [taskの再計画](../../plans/task-replanning.md) の「実装の状態」。

## 人の手での引き継ぎ

userとinbox（人の言葉で）は、`in_progress`のtaskの最新runが`failed`か`interrupted`のとき、`dagq ready ID --inherit --reason ...`でそのrunのbranchを次のrunに引き継いでtaskを`ready`に戻せる（[ADR-t1962-1](../../adr/2026-10-07-t1962-1-retry-carrying-the-branch-over-by-hand.md)）。
decideのaskの答えに無い引き継ぎを、人の判断として直接打つための操作で、decideの答えの集合（ADR-0047決定3）は変えない。
確定の前提と記録は `SqliteQueue::inherit_by_hand` のdoc commentが持つ。
認可は計画管理の`ready`が行い、引き継ぎの確定は実行と着地が公開するportが行う（境界をまたぐtransactionのT10、`src/context_boundaries/ownership.rs`）。

引き継ぐheadの選び方とrefの保存・戻しは `inherit::keep_head` と `inherit::KeptHead::settle` のdoc commentが持つ（[ADR-t3294-1](../../adr/2026-10-25-t3294-1-carry-over-by-hand-keeps-the-ref-before-the-record-and-puts-it-back-on-refusal.md)）。
確定の間はrefのlockを持たず、確定が通った時のrefが記録と食い違うことも、断った呼び出しの戻しが同じheadの別の書き手のrefを消すこともある。
保証するのは次のrunが記録されたheadから始まることで、refの値ではない。
containerのcloneもpromptも記録されたheadをidで名指す。
競った書き手や確定の前後に止まったCLIは、次のrunの始まりを変えない。
次のrunへの引き渡しは `inherit::keep_inherited_head`、containerへの取り込みは [Container runtime](../container-runtime.md#git) が入口になる。

復旧jobのラウンドとの競合は、確定のtransactionでrunのleaseを見て解くので、止まっていたラウンドからの古い適用も防ぎ、どちらの順でも引き継ぎは高々1回になる。
手での引き継ぎは自動のretry_inheritのtaskごとに1回の制限を使わず、その回数にも数えない。

## Codexで動かす

復旧jobのproviderは役割が選ぶ（[Actor model](actor-model.md#provider)、[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)、[ADR-t1204-1](../../adr/2026-09-30-t1204-1-explicit-no-claude-operation.md)）。
jobは状態を変えず読むだけで、どのproviderでも上の適用と前提の再確認を通る。
設定はjobを始めるたびに読むので、main checkoutに設定の変更が反映されればsupervisorの再起動は要らない。

行き先は `Supervisor::recovery_route` のdoc commentが持つ。
ClaudeのholdはCodexのjobを止めず、Claudeを使わない運転ではClaudeに回さない。
fallbackを止める約束は [ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md) が持つ。

起動環境と出力の入口は `recovery::start_job`、provider固有の読み取りは `AgentProvider::job_reply` のdoc commentが持つ（[Provider lifecycle](../provider-lifecycle.md)）。
jobはworkerのworktreeとrun directoryに書けず、processを止める操作はruntimeがpidで行う。
Codexのjobの失敗をそのままClaudeに回さない。
providerが使えない失敗だけは人のattentionにせず、次のラウンドの行き先を選び直す（`Supervisor::fail_recovery`）。

## taskのhold

holdは復旧jobの開始と結果・回答の適用を止める境界でもある。
解除の後に適用できるかの入口は `SqliteQueue::settle_kept_result`、新しいrunへ引き継ぐ境界とretryの数え方は [taskのhold](../../plans/task-hold.md) が持つ。
