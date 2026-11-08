---
id: design-supervisor-lifecycle-needs-session
type: design
title: "`needs_session`"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t639-1
  - adr-t946-1
  - adr-t3255-1
  - design-supervisor-lifecycle
  - adr-0027
  - adr-0026
  - adr-0028
  - adr-0047
  - adr-0068
  - adr-t803-1
  - adr-0079
  - design-supervisor-lifecycle-worker-model
---

# `needs_session`

着地の衝突や変更に由来する検証の失敗を、成果を残したままworkerへ返す待ちの状態。
runはtaskを占有し続け、`ready`/`cancel`はできない。
状態の入口は`domain::run::defer_integration`で、parkする境界は[`integrate`](integrate.md#receiptの検査とrebase)を読む。
hostの失敗はworkerの修正やresumeの試行を使わせず、[hostの失敗のやり直し](integrate.md#hostの失敗のやり直し)へ分ける。

着地前の[e2e](integrate.md#着地の前のe2e)と[landing recheck](integrate.md#landing-recheck)も、変更の修正が要るrunをここへ返す（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)、[ADR-0068](../../adr/0068-recheck-waiting-runs-after-each-landing.md)）。
containerでのrecheckの境界は[ADR-t2424-1](../../adr/2026-10-17-t2424-1-a-container-runs-landing-recheck-command-runs-in-the-verification-container.md)、modelの段上げは[ADR-0079](../../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)決定5を読む。
依頼文の選び方は`application::prompt::requests::ResumeKind`が持つ。
古い記録の`session_gone_parked`もparkとして読めるが、新しくは書かない（`domain::reason::ReasonCode::SessionGone`）。
resumeのmodelとeffortの継承・段上げは[Worker model](worker-model.md#段上げ決定5)を読む。

`integrate`を呼ぶことが着地の承認で、runtimeが自分で着地させるのは承認済みのrun、reviewがpassしたrun（上限の後の差し戻しが収束したものを含む）、人が着地を選んだrunだけ（[ADR-0027](../../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)）。
承認の記録とpushの引き継ぎは`application::integrate::IntegrationRequest`を入口に読む。

**試行の数え方**の入口は`domain::resume`（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定24）。
衝突だけの試行は、待っている間に動いたmainのために成果を捨てないための別枠で、解けない衝突が回り続けることも防ぐ。
上限とprecheckとの共有、使い切りの判定は`domain::resume::ResumeCount`が持つ。
人の差し戻しを衝突だけの上限で止めない約束は[ADR-t2946-1](../../adr/2026-10-22-t2946-1-a-persons-send-back-resumes-past-the-conflict-only-limit.md)を読む。

**killだけの試行**は外から止められたsessionをrun自身の失敗に数えない別枠で、入口は`domain::resume::resume_start_plan`（[ADR-t946-1](../../adr/2026-09-29-t946-1-kill-only-resumes-have-their-own-limit.md)）。
**holdの後の試行**は人の解除で再開するため、試行の上限で止めない（[ADR-t2664-1](../../adr/2026-10-22-t2664-1-resumes-after-a-hold-release-are-not-counted.md)）。
**providerだけの試行**の判定は`domain::resume::finish_provider_only`を読む（[ADR-t3255-1](../../adr/2026-10-24-t3255-1-provider-only-resumes-wait-for-the-hold-and-are-not-counted.md)）。
providerが使えない間の待ちで試行を費やさない。

supervisorだけがresumeのwrapperを起動する。
再開の候補は復旧job・claimと同じ列でslotを受ける（[supervise](supervise.md#slotを渡す順とclaim)、[ADR-t1850-1](../../adr/2026-10-06-t1850-1-resumes-recovery-jobs-and-claims-share-one-line-by-effective-priority.md)）。
人とinboxはtaskの優先度で再開待ちの順を変えられるが、走っている再開は止めない。
同じworktreeで2つ目のsessionを始めず、同じrunを2つのslotで進めない柵は`Supervisor::resume_candidates`が持つ。
containerが使えなければhostで代行しない（[Container runtime](../container-runtime.md#fail-closedの境界)）。

再開の実装への入口は`src/application/supervise/execution/resume.rs`。

| 知りたいこと | コードの入口 |
| --- | --- |
| 前の試行で解消済みならsessionも試行も使わず進める判定 | `Supervisor::resolved_head` |
| 再開のleaseと進行の記録 | `SqliteQueue::begin_resume`（`src/infrastructure/runtime_store/recovery.rs`） |
| backgroundの起動と依頼の保存 | `Supervisor::start_resume`（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)） |
| receiptとturnの終了、wrapperの生存を見張る境界 | `ResumeWatch` |
| 解消したrunを承認に応じて着地かvalidationへ渡す境界 | `Supervisor::finish_resumed_session` |
| 使い切ったrunの引き継ぐretryと復旧jobへの境界 | `Supervisor::exhaust_resumes`（ADR-0047決定24・39） |
| 引き継ぐcommitの選択と保持 | `Supervisor::inherited_head` |
| 引き継ぐ再試行への引き渡し | `Supervisor::retry_inheriting` |
| 引き継ぐ再試行の確定と再検査 | `SqliteQueue::exhaust_resumes`（`src/infrastructure/runtime_store/recovery.rs`） |
| 起動や監視自体のerrorと残したwrapper | `Supervisor::give_up_resume`（`src/application/supervise/execution/slot_end.rs`） |

引き継ぐ再試行はtaskの中身を変えないためplan reviewにはかけない。

再開中も状態は増やさず`needs_session`のままで、進行は`resume_started`と`resume_finished`で読む。
解消済みでskipしたrunも宣言パスの検査を免れず、[Validation](validation.md#validation)か着地で検査する。
開いたままの着地のaskがあるrunは、解消しても人の答えを待つ。
大きなcontextから新しいsessionへ渡す境界は[Agent provider lifecycle](../provider-lifecycle.md#大きなcontextからの新しいsession)、依頼文の切り方は[Prompt](prompt.md#次のturnの文の上限)が持つ。

[放棄](background-recovery-job.md#1-runの放棄abandon)の例外を除き、この状態のrunはruntimeが再開を担い、`status`のattentionは`next: resuming (runtime)`を示す。
attentionの理由の入口は`application::health::attention`。
sessionが変更不要と判断した場合はreceiptを`result: failed`と理由で書き直す。
そのrunのintegrateはmainに触れず失敗し、worktreeは残り、[Triage](triage.md#triage-supervisor)へ渡る。
triageから再開しても、このrunの試行の上限を通算で共有する。

## 再計画で保留したrun

保護中は自動resume・解消済みのskip・使い切りの後始末をせず、再開と差し戻しの回数も引き継ぐ。
継続か撤回の後の照合と復元は`Supervisor::restore_parked`（`src/application/supervise/execution/replan_park.rs`）を入口に、[順と失敗](../../plans/task-replanning.md#実装の状態)を読む。
保存していない操作もworktreeの成果を捨てず、照合に失敗した操作は成果を残して判断を待つ。
保留が終わった柵にslotで出会ったrunも書き手が終わるまで待ち、slotの外で休む（`ParkFor::Rest`）。
解除後の再開は新しいvalidationとreviewから続ける。

## supervisorの入れ替わり

依頼と終了はfileに残し、再構築の入口は`Supervisor::adopt_resume`。
記録したhandleの無いwrapperは監視できず、人がwrapperの生存を確かめる必要があり、生存中は2つ目のsessionを始めない。
依頼を書いてから送信の記録までにsupervisorが死ぬと、依頼が2回届きうる。

## taskのhold

holdはresumeと使い切りの後始末を止め、再計画の保留と重なれば両方が終わるまで再開しない。
工程の終わりでの保留は[非対話のworker](headless-worker.md#再計画の保留)、柵とleaseの引き継ぎは`src/application/supervise/execution/replan_park.rs`のmodule docを読む。
解除が同じrunを続けるか、新しいrunへ成果を渡すかの選択は`domain::task_hold::release_decision`が持つ。
引き継ぐ成果を保存できない解除は、hold・run・taskを変えず拒む。
未消費の回答と保留前の承認の扱いも同じ関数のdoc commentを入口に読む。
