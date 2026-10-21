---
id: design-supervisor-lifecycle-plan-review
type: design
title: "Plan review (supervisor)"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1985-1
  - adr-t1704-1
  - adr-t1566-1
  - adr-t451-1
  - design-supervisor-lifecycle
  - adr-0041
  - adr-0044
  - adr-0079
  - design-supervisor-lifecycle-actor-model
  - adr-t947-1
  - adr-t1300-1
  - adr-t1504-2
  - adr-t1981-1
  - adr-t1971-1
  - adr-t1975-1
  - adr-t2751-1
---

# Plan review (supervisor)

## 目的

submitされたproposalを、taskがreadyになる前にheadlessのjobが検査し、そのverdictと人のanswerをruntimeが適用する（[ADR-0041](../../adr/0041-on-demand-planners-proposals-submitted-and-plan-review-job.md)の決定9〜15・17、[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定10〜15・17）。
reviseをplannerへ届けることと、そのためのruntimeのplannerの立て方と終わり方もここが持つ。

## 流れ

```text
submit -> 候補の順序 -> job（読むだけ） -> verdict -+- pass -----> taskがready
  ^                                                +- revise ---> plannerへ配送 -> 出し直し --+
  |                                                +- concern --> AIが決める / approve_planのask -> 人のanswer
  +-------------------------------------------------------------------------------------------+
失敗 -> 保留（plan review by hand）    reopen -> proposalの外のreadyのtaskをruntimeのproposalへ（上限を超えれば1件も動かさず、検査中のproposalは最終の判定でreviseかconcernのaskへ）
```

## 責務と境界

- supervisorのloopの各passで進め、run slotを使わない（observerと同じ）。
- jobは読むだけで、`dagq`の書き込むコマンドはCLIが役割で拒む（[Queue service](../queue-service.md#クライアントモード)）。
- repositoryの規則をruntimeに埋め込まず、jobがrepositoryの指示と文書から読む（[Prompt](prompt.md#repositoryの規則を読む順)）。
- plannerの起動・行・終わりの記録は[`plan` / `planners`](plan-planners.md)が持つ。
  計画の依頼・draft・findingのplannerは[inboxからの計画の依頼](plan-planners.md#inboxからの計画の依頼)・[Draft planners](draft-planners.md)・[Finding planners](finding-planners.md)が持ち、reviseの配送の後に同じ上限の中で立つ。

## 不変条件

- 未完了のplan reviewはqueue全体で同時に1つ。
- verdictとanswerの適用はそれぞれ1トランザクションで、actionが1つでも検査に外れればverdictの何も適用しない。
- jobの開始より後に検査中のproposalのtaskが編集されたら、verdictも失敗も適用せず、次のpassで編集後の内容をかけ直す。
- 失敗で保留したproposalはsubmittedのまま動かさず、supervisorは自分では取り直さない。
- plannerには何も打ち込まない。
  runtimeのplannerへの指摘・答え・終了は次のturnの依頼として置き、古い記録のworkspaceの行には何も送らない（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定3・5）。
- runtimeがplannerを立てるのは`runtime_planners`の上限の中だけ。

## 入口

- passの全体とplannerの世話: `src/application/supervise/planning/plan_review.rs`の`PlanningState::plan_review_pass`。
- verdictの型・runtimeの決め方・順序・人の答え: `src/domain/plan_review.rs`。
- 記録と適用: port `PlanReviewStore`（`src/application/ports/planning.rs`）の各操作のdoc commentと、storeの`src/infrastructure/plan_reviews.rs`。
  行と欄の意味は`migrations/0027_plan_review.sql`のコメントが持つ。
- promptの材料・検査の指示・上限と選ぶ順: `src/application/prompt/plan_review.rs`の`plan_review_prompt`と各定数。
  方針は[Prompt](prompt.md#headlessのjobのprompt)と[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)。
- 境界のtestは`tests/it/`のplan reviewのmoduleで、stubのheadless providerと止めたplannerのwrapperを使う。

## 候補と開始

- 次に取るproposal: `domain::plan_review::next_to_review`と`PlanReviewStore::plan_review_candidates`。
- 開始の記録、死んだsupervisorの行の引き継ぎ、attemptの数え方: `PlanReviewStore::begin_plan_review`。
- 起動のmodel / effortは[Actor model](actor-model.md)の役割の表から決める。
- jobの起動: `PlanningState::start_plan_review_job`（promptの渡し方は[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）。
- 予想するファイルは[claimを控える](claim-hold.md#判定の入力)と同じ規則で求める（[ADR-t1981-1](../../adr/2026-10-07-t1981-1-expected-files-leave-out-path-globs.md)）。
- 依存は中身の前提のときだけで、同じファイルを触ることはclaimの控えに任せる（[ADR-t1985-1](../../adr/2026-10-07-t1985-1-dependencies-only-for-content-prerequisites-and-conflicts-left-to-claim-deferral.md)）。
- 落とし穴: run_eventsはtaskかgoalを要るので、proposalのeventはproposalの最初のtaskに付く。

## verdictと適用

- verdictの形と、予測が未知のフィールドの拒否の対象外であること: `PlanReviewVerdict`。
- runtimeがverdictから決めること: `domain::plan_review::decide_verdict`。
  reviseの上限はrunのreviewとは別に数える（[ADR-t2256-1](../../adr/2026-10-11-t2256-1-raise-the-run-review-revise-limit-and-decouple-plan-review.md)）。
- 適用（検査中の編集、passでの由来と優先度、reopen、重さの予測）: `PlanReviewStore::finish_plan_review`。
- reopenの上限と超えたときの扱い: `domain::plan_review::REOPEN_LIMIT`と`decide_reopen`（[ADR-t2751-1](../../adr/2026-10-18-t2751-1-plan-review-reopens-at-most-a-few-ready-tasks-and-sends-back-a-change-that-breaks-many.md)）。
  上限を超えたreopenは1件も適用せず、多くの登録済みのtaskを壊す変更は検査中のproposalの側で互換を守るか分けるよう求める。
  passはrevise（reviseの上限を過ぎていればconcern）になり、reviseとconcernは判定を保って理由を足す（reviseは差し戻しの理由に、concernはaskの文に）。
- 由来と優先度の出どころによる扱い: `domain::plan_review::priority_refusal`と`priorities_to_inherit`（ADR-t1971-1、[ADR-t1975-1](../../adr/2026-10-08-t1975-1-runtime-records-origin-and-person-set-priority-and-ai-actors-cannot-change-it.md)決定7）。
- 約束: jobが自分でしてよい修正は`PlanReviewAction`と`reopen`だけで、それ以外はreviseでplannerが直す。
- 重さの予測は記録だけでverdictを変えない（[ADR-0079](../../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)の決定2）。
  claimは予測を作らず、予測をworkerのmodel / effortに使うのは[Worker model](worker-model.md)の限定の試しだけ。

## 失敗と保留

- 失敗の扱いと保留: `PlanReviewStore::fail_plan_review`と`domain::plan_review::failed_end`。
- providerが使えなかった失敗: 下の[Codexで動かす](#codexで動かす)。
- 保留中のattention: `PlanReviewStore::plan_review_holds`。
- submittedのtaskが無くなったproposalの片付け: `PlanReviewStore::settle_proposals`。
- 約束: 出し直し（`submit --proposal`）は保留とreviseの記録を全て消してplan reviewにかけ直す。

## 人のanswer

- `approve_plan`のaskへの答えの適用: `PlanReviewStore::decide_plan`と`domain::plan_review::PlanAnswer`。
- concernのaskとその文面: use caseの`plan_ask`。

## reviseの配送

- 配送、plannerの立て方、期限と知らせ: `PlanningState::tend_planners`と`revise_watch`。
- 上限で待つreviseのための枠の空け方: `PlanningState::release_runtime_planner`。
- plannerを終わらせない理由: `PlannerBusy`と`PlanningState::busy_reasons`。
- 届ける依頼の文: `prompt::plan_revise_request`。
- 持ち主が居ないreviseは、回答済みの`planner_question`とそれを出したplannerごとの引き継ぎ材料を同じ新しいplannerに運び、`plan_revise_sent`と運んだanswerのclose（`ask_delivered`）を1トランザクションで記録する（[ADR-t1704-1](../../adr/2026-10-05-t1704-1-human-answer-wait-releases-runtime-planner-slots.md)決定2〜4、`PlanReviewStore::revise_sent`）。
- 差し戻しで開き直すplannerのeffortは1段上げ、生きているsessionのmodel / effortは切り替えない（[Actor model](actor-model.md#差し戻しで開き直すplannerの段上げ)）。
- 落とし穴: sessionが見つからないだけではplannerが終わった証拠にせず、wrapperで判定する（workerと同じ。`PlanningState::planner_gone`）。
- 落とし穴: plannerはClaudeのsessionなので、Claudeの控えの間はruntimeのplannerへの配送を止める。
- 上限・期限・pluginの設定は`supervise`の`--help`、`runtime_planners`の優先順と読み直しは[Run environment](run-environment.md)の`[supervisor]`。

## runtimeのplannerの終わり

- 終わったplannerの片付けと終了の依頼: `PlanningState::end_runtime_planners`。
- 人の答えだけを待つplannerの終わり: `PlanningState::end_for_answer_wait`（[ADR-t1704-1](../../adr/2026-10-05-t1704-1-human-answer-wait-releases-runtime-planner-slots.md)）。
- 届けた答えを読み終えたかの判定: `PlanningState::question_wait`と`domain::turn::request_read`。
- 落とし穴: 壁（ログイン切れ・利用上限・起動の失敗）で失敗したturnを読み終えたとすると、answerが読まれずに失われる。
- 人が開いたplannerの行: `PlanningState::close_person_planners`（[`plan` / `planners`](plan-planners.md)の「人のplannerの終わり」）。
- 終わりの記録: [`plan` / `planners`](plan-planners.md)の「plannerの終わりの記録」（[ADR-t1300-1](../../adr/2026-10-02-t1300-1-runtime-closes-exited-person-planners-after-a-grace.md)）。

## 引き継ぎ

- execの[引き継ぎ](handoff.md)の前後: `PlanningState::stop_for_handoff`・`interrupt_for_handoff`と`PlanReviewStore::interrupt_plan_reviews_for_handoff`。
- 約束: 引き継ぎを待つあいだに終わったjobのverdictは捨てずに適用し、新しいjobは始めない。

## Codexで動かす

- 役割ごとのproviderはgoal reviewと同じで（[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)の決定1〜6、[Actor model](actor-model.md#provider)）、`--no-claude`との関係はrunのreviewと同じ（[ADR-t1207-1](../../adr/2026-09-30-t1207-1-codex-run-review.md)）。
- 行き先: use caseの`plan_review_route`。
  切り替えを止める設定は[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)。
- 起動と権限の意図: `prompt::PLAN_REVIEW_ACCESS`と[Agent provider lifecycle](../provider-lifecycle.md#codexのheadless-job)、sandboxからqueue serviceへの到達は[Queue service](../queue-service.md#codexのsandboxからの到達adr-t1233-5決定4)。
- 約束: promptとverdictの読み方はClaudeと同じで、Claudeのplugin・skill・hookに頼らない。
- 落とし穴: Codexはthreadを自分で名付けるので、session IDと実際のmodelは終わりのeventと区間の`session_closed`が持つ（`JobSession::record`）。
- 使えないproviderで失敗したjobは保留せずもう一方のproviderでやり直し、それ以外の失敗は保留してClaudeに戻さない（`PlanReviewFailure::unusable`）。
- このrepositoryの設定は`dagq.toml`の`[roles.plan_review]`が持つ。

## 差し戻しの分類コード<a id="差し戻しの分類コード未実装"></a>

- 決定は[ADR-t947-1](../../adr/2026-09-28-t947-1-review-verdicts-carry-reason-codes.md)。
- 一覧と定義と主の選び方: `domain::review_reason::PLAN_REVIEW_CODES`（promptがそのまま載せる）、読み方と記録の規則はそのmoduleのdoc。
- 約束: コードはverdictの判定と適用に使わない。
- runのreviewと共通のコードは[Review](review.md#差し戻しの分類コード)と同じ定義で使う（plan reviewで止められなかったものが後でreviewのconcernやworkerの問いになった割合を追うため）。
  `task_overlap`はworkerの問い（[ask](ask.md#worker_questionの分類コード)）と同じ定義。
- 人の答えからの補い: storeの`record_plan_outcome`と`domain::review_reason::answer_outcome`。
- `reopen`の上限を超えた差し戻しで足す理由には、jobでなくruntimeが`domain::review_reason::REOPEN_OVER_LIMIT`を付ける（promptの一覧に無い）。
  記録の並び: `PlanReviewVerdict::recorded_reasons`。

## AIが決めるconcern<a id="aiが決めるconcern未実装"></a>

- 決定は[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)の決定4（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定11をamends）。
- 判定: `PlanReviewVerdict::decide_concern`、欄の意味は`PlanRecommendation`・`PlanConcernReason`・`PlanConcernEscalation`。
- 確信度が高く理由の分類の無い`ready` / `send_back`はpass / reviseとして適用し、verdictは`concern`のまま残る。
- 人に残すaskの理由の分類: `PlanConcernDecision::ask_reason`。
- 約束: jobが自分でしてよい修正（actionsと`reopen`）は変わらず、askを作らないので`plan_review_outcome`は記録しない。
- 決めた数の読み方は[Stats](commands.md)。
