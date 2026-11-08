---
id: design-supervisor-lifecycle-goal-review
type: design
title: "Goal review (supervisor)"
status: current
created: 2026-09-27
scope: runtime
related:
  - adr-t1504-2
  - adr-t3737-1
  - design-follow-up-membership
  - adr-t1566-1
  - design-supervisor-lifecycle-prompt
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-plan-review
  - design-supervisor-lifecycle-draft-planners
  - design-supervisor-lifecycle-actor-model
  - design-provider-lifecycle
  - adr-0047
  - adr-t1063-1
---

# Goal review (supervisor)

所属taskがすべて終わったopenのgoalを、supervisorが起動するheadlessのjobがacceptanceと照合し、そのverdictと人のanswerをruntimeが適用する（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定43）。
plannerはgoalの完了を見てcloseする役を持たず、CLIでのcloseは人の判断のために残る。

## 記録と引き継ぎ

loopからの入口は`src/application/supervise/planning/goal_review.rs`の`goal_review_pass`。
run slotは使わず、`--once`は走っているjobを待つ。
drain中は新しく始めず、走っているjobとanswerの適用は続ける。

queue全体で同時に1つという境界はplan reviewとは別で、DBの制約でなく`GoalReviewStore::begin_goal_review`が守る。
行とeventの記録は`src/infrastructure/goal_reviews.rs`、生きているsupervisorの判定は`domain::job_supervisor_lives`が持つ。
execの回収と止め方は[引き継ぎ](handoff.md)、queueが取らなかった終わりを先に引き取る約束は[Supervise](supervise.md#queue-serviceの経路)を参照する。
中断はattemptに数えず、候補のままならreviewをやり直す。

## 起動の条件と入力

候補の選択は`GoalReviewStore::goal_review_candidates`、起動できるかの判断は`domain::goal_review::reviewable`が入口。
入力が変わらなければ起動せず、人の`goal review ID`だけが同じ入力を取り直させる。
入力の同一性は`domain::goal_review::review_fingerprint`が持つ。
範囲外と判断して別のgoalへ移したfollow_upの未完了は起動を妨げず、そのstatusの変化でもreviewを無効にしない。
所属の判断だけで待つgoalのattentionは[Follow-up membership judgements](../follow-up-membership.md#閉じる条件)を参照する。

置換先は所属を問わず再置換の先まで待つが、置換元を完了実績には数えない（[ADR-t1521-2](../../adr/2026-10-05-t1521-2-trusted-runtime-parks-snapshots-and-atomically-replaces-tasks.md)決定5）。
置換先の記録が空・欠落・循環なら解決済みにせず、自動reviewも達成としてのcloseも拒み、定期taskのkeyも保つ。
壊れた記録は手で直さず、終了は人が判断する。
判定の入口は`domain::replan_apply::unresolved_replacements`。

jobへ渡す材料は`PlanningState::goal_review_material`、promptは`prompt::goal_review_prompt`が持つ。
範囲外のfollow_upは達成の判定から外し、requiredは所属taskと同じく含める（[ADR-t1504-2](../../adr/2026-10-04-t1504-2-runtime-records-and-enforces-follow-up-membership-judgements.md)）。
promptの渡し方・上限・選び方・省略の明示は[Prompt](prompt.md#headlessのjobのprompt)と[上限の節](prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)を参照する（[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)）。

## 起動とprovider

起動とその失敗の入口は`PlanningState::start_goal_review`、行き先の判断は`goal_review_route_of`。
jobはqueue serviceを通して読むだけで、権限の境界は[Roles](roles.md#actors)を参照する。
役割の設定は[Actor model](actor-model.md)、providerの切り替えと控えは[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)が持つ（[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)、[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）。
queue serviceが起動の要求をしなかったと確定した場合は失敗にせず、次の周回かsupervisorで取り直す（[ADR-t3737-1](../../adr/2026-11-01-t3737-1-a-headless-job-whose-start-the-queue-service-did-not-do-starts-again-and-is-no-failure.md)）。

## verdictの適用と失敗

verdictの解釈は`domain::goal_review::GoalReviewVerdict`、適用の入口は`GoalReviewStore::finish_goal_review`。
適用は1トランザクションで所有と入力と閉じる条件を再検査し、review中に入力が変われば使わない。
queueの状態だけで再検査するので、execの引き継ぎで回収したverdictもsupervisorの再起動後も同じ結果になる。

gapはdraftになり、[Draft planners](draft-planners.md)の経路でruntimeのplannerが採否を決める。
人に聞くaskは通知を送らず、inboxがwatchで受ける。
失敗の後は入力が変わるか人が取り直させるまで待ち、inboxのattentionでgoalを示す。
再開・終了は人が判断する。
ただし使えないproviderによる中断は人のgoal reviewを待たず候補に戻る。
切り替えを止める設定では控えが解けるまで同じproviderを待つ。
失敗の分類と控えは`supervise::execution::jobs::job_failure_route`が入口。
復旧の操作は[dagq-recoverのgoal review by hand](../../../plugins/claude-dagq/skills/dagq-recover/reference/goal-review-by-hand.md)が持つ。

## answerと達成の訂正

通常の答えの条件は`domain::goal_review::GoalAnswer`、適用は`GoalReviewStore::decide_goal`が持つ。
inboxに渡したanswerを、後で条件がそろってからruntimeが適用することはない。
attentionも回答時に決めた配送先を読み、現在のgoalの状態で判定し直さない。

達成の後でrequiredと判断されたfollow_upは、自動でgoalを開き直さず、人がaskで訂正を決める（[ADR-t1504-2](../../adr/2026-10-04-t1504-2-runtime-records-and-enforces-follow-up-membership-judgements.md)決定9）。
askを開く判断は`domain::follow_up::opens_correction`、質問の材料は同moduleの`correction_question`、答えの適用は`infrastructure::follow_up_membership::decide_correction`が持つ。
再開しても走っている依存taskは止めず、過去のeventは書き換えない。
再開したgoalを待つtaskのうち未claimのものは、もう一度閉じるまで待つ。
所属の訂正の境界は[Follow-up membership judgements](../follow-up-membership.md#achievedの後の訂正)を参照する。
