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
  - adr-t1487-1
  - adr-t3321-1
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
失敗 -> 保留（plan review by hand）    reopen -> 上限の中でproposalの外のreadyのtaskをruntimeのproposalへ
```

## 責務と境界

- supervisorのloopの各passで進め、run slotを使わない（observerと同じ）。
- jobは読むだけで、`dagq`の書き込むコマンドはCLIが役割で拒む（[Queue service](../queue-service.md#クライアントモード)）。
- repositoryの規則をruntimeに埋め込まず、jobがrepositoryの指示と文書から読む（[Prompt](prompt.md#repositoryの規則を読む順)）。
- `lint`のhintsはplannerがsubmitの前に読むもので、submitもjobも読まない（[ADR-t3321-1](../../adr/2026-10-24-t3321-1-lint-hints-related-doc-sections-without-refusing-submit.md)）。
- plannerの起動・行・終わりの記録は[`plan` / `planners`](plan-planners.md)、draft・findingのplannerは[Draft planners](draft-planners.md)・[Finding planners](finding-planners.md)が持ち、それらはreviseの配送の後に同じ上限の中で立つ。

## 不変条件

- 未完了のplan reviewはqueue全体で同時に1つ。
- verdictとanswerの適用はそれぞれ1トランザクションで、actionが1つでも検査に外れればverdictの何も適用しない。
- jobの開始より後に検査中のproposalのtaskが編集されたら、verdictも失敗も適用せず、次のpassで編集後の内容をかけ直す。
- 失敗で保留したproposalはsubmittedのまま動かさず、supervisorは自分では取り直さない。
- 再計画の操作のtaskを置き換えるproposalは、jobに置換計画と元条件・snapshotを渡して意味上の対応を検査させ、passと人の`ready`は同じtransactionで今の版で検査し直す（`domain::plan_review::refuse_replacement`）。
  passでも子は`submitted`に残り、readyにするのはtrusted runtimeの適用だけ（[taskの再計画](../../plans/task-replanning.md#実装の状態)）。
- plannerには何も打ち込まない。
  runtimeのplannerへの指摘・答え・終了は次のturnの依頼として置き、古い記録のworkspaceの行には何も送らない（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定3・5）。
- runtimeがplannerを立てるのは`runtime_planners`の上限の中だけ。

## 入口

- passの全体とplannerの世話: `supervise::planning::plan_review`の`PlanningState::plan_review_pass`。
- verdictの型・runtimeの決め方・順序・人の答え: `domain::plan_review`。
- 記録と適用: port `PlanReviewStore`の各操作のdoc commentと、storeの`infrastructure::plan_reviews`。
  行と欄の意味は`migrations/0027_plan_review.sql`のコメントが持つ。
- promptの材料・検査の指示（前提とSpikeの確認を含む）・上限: `prompt::plan_review`の`plan_review_prompt`と各定数。
  方針は[Prompt](prompt.md#headlessのjobのprompt)と[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)、前提とSpikeの確認は[ADR-t1487-1](../../adr/2026-10-04-t1487-1-spike-is-an-execution-class-with-a-judged-result-and-a-durable-replanning-request.md)の決定2・3・6・8。
- `lint`: `queue_reads::planning::lint`、規則は`domain::lint`、hintsは`domain::doc_hints::hints`。

## 候補と開始

- 次に取るproposalと開始: `domain::plan_review::next_to_review`・`PlanReviewStore::begin_plan_review`・`PlanningState::start_plan_review_job`。
- 予想するファイルは[claimを控える](claim-hold.md#判定の入力)と同じ規則で求める（[ADR-t1981-1](../../adr/2026-10-07-t1981-1-expected-files-leave-out-path-globs.md)）。
- 依存は中身の前提のときだけで、同じファイルを触ることはclaimの控えに任せる（[ADR-t1985-1](../../adr/2026-10-07-t1985-1-dependencies-only-for-content-prerequisites-and-conflicts-left-to-claim-deferral.md)）。
- 落とし穴: run_eventsはtaskかgoalを要るので、proposalのeventはproposalの最初のtaskに付く。

## verdictと適用

- verdictの形: `PlanReviewVerdict`、runtimeの決め方: `domain::plan_review::decide_verdict`、適用: `PlanReviewStore::finish_plan_review`。
  reviseの上限はrunのreviewとは別に数える（[ADR-t2256-1](../../adr/2026-10-11-t2256-1-raise-the-run-review-revise-limit-and-decouple-plan-review.md)）。
- reopenの上限: `domain::plan_review::decide_reopen`（[ADR-t2751-1](../../adr/2026-10-18-t2751-1-plan-review-reopens-at-most-a-few-ready-tasks-and-sends-back-a-change-that-breaks-many.md)）。
  上限を超えたreopenは1件も適用せず、多くの登録済みのtaskを壊す変更は検査中のproposalの側で互換を守るか分けるよう求める。
- 由来と優先度の出どころによる扱い: `domain::plan_review::priority_refusal`と`priorities_to_inherit`（ADR-t1971-1、[ADR-t1975-1](../../adr/2026-10-08-t1975-1-runtime-records-origin-and-person-set-priority-and-ai-actors-cannot-change-it.md)決定7）。
- 約束: jobが自分でしてよい修正は`PlanReviewAction`と`reopen`だけで、それ以外はreviseでplannerが直す。
- 重さの予測は記録だけでverdictを変えない（[ADR-0079](../../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)の決定2）。
  claimは予測を作らず、予測をworkerのmodel / effortに使うのは[Worker model](worker-model.md)の限定の試しだけ。

## 失敗・人のanswer・引き継ぎ

- 失敗と保留: `PlanReviewStore::fail_plan_review`・`plan_review_holds`・`settle_proposals`と`domain::plan_review::failed_end`。
- 約束: 出し直し（`submit --proposal`）は保留とreviseの記録を全て消してplan reviewにかけ直す。
- `approve_plan`のaskへの答え: `PlanReviewStore::decide_plan`と`domain::plan_review::PlanAnswer`、concernのaskはuse caseの`plan_ask`。
- execの[引き継ぎ](handoff.md)の前後: `PlanningState::stop_for_handoff`・`interrupt_for_handoff`。
- 約束: 引き継ぎを待つあいだに終わったjobのverdictは捨てずに適用し、新しいjobは始めない。

## reviseの配送

- 配送・plannerの立て方・期限: `PlanningState::tend_planners`と`revise_watch`、文は`prompt::plan_revise_request`。
- 上限で待つreviseのための枠の空け方: `PlanningState::release_runtime_planner`。
- 約束: 終了の依頼を置く直前に`turns/`を読み直し、終わっていない依頼があれば終わらせない（`PlanningState::request_outstanding`、ほかの理由は`busy_reasons`）。
- 持ち主が居ないreviseは、回答済みの`planner_question`と引き継ぎ材料を同じ新しいplannerに運ぶ（[ADR-t1704-1](../../adr/2026-10-05-t1704-1-human-answer-wait-releases-runtime-planner-slots.md)決定2〜4、`PlanReviewStore::revise_sent`）。
- 持ち主が居ない置換proposalのreviseは、再計画の材料を新しいplannerに渡して`--replacement`を付けた出し直しを指示する（[ADR-t1521-2](../../adr/2026-10-05-t1521-2-trusted-runtime-parks-snapshots-and-atomically-replaces-tasks.md)決定3・4、`PlanningState::open_planner_for`）。
  保留したtaskについてのplannerの問いの答えの行き先は`draft_planners::route_of`。
- 開き直すplannerのeffortの段上げは[Actor model](actor-model.md#差し戻しで開き直すplannerの段上げ)、同じgoalを再計画する依頼の待ちは[Spikeの再計画の依頼](plan-planners.md#spikeの再計画の依頼)。
- 落とし穴: sessionが見つからないだけではplannerが終わった証拠にせず、wrapperで判定する（`PlanningState::planner_gone`）。
- 落とし穴: plannerはClaudeのsessionなので、Claudeの控えの間はruntimeのplannerへの配送を止める。

## runtimeのplannerの終わり

- 片付けと終了の依頼: `PlanningState::end_runtime_planners`、人の答えだけを待つplannerは`end_for_answer_wait`（[ADR-t1704-1](../../adr/2026-10-05-t1704-1-human-answer-wait-releases-runtime-planner-slots.md)）。
- 届けた答えを読み終えたかの判定: `PlanningState::question_wait`、`answer_taken`、`domain::turn::request_read`（[Draft planners](draft-planners.md#予定-人のplannerの廃止と非対話の経路)）。
- 落とし穴: 壁（ログイン切れ・利用上限・起動の失敗）で失敗したturnを読み終えたとすると、answerが読まれずに失われる。
- 人が開いたplannerの行と終わりの記録: `PlanningState::close_person_planners`と[`plan` / `planners`](plan-planners.md)（[ADR-t1300-1](../../adr/2026-10-02-t1300-1-runtime-closes-exited-person-planners-after-a-grace.md)）。

## Codexで動かす

- 役割ごとのprovider（[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)、[Actor model](actor-model.md#provider)）の行き先はuse caseの`plan_review_route`、切り替えは[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)。
- 起動と権限の意図: `prompt::PLAN_REVIEW_ACCESS`と[Agent provider lifecycle](../provider-lifecycle.md#codexのheadless-job)。
- 約束: promptとverdictの読み方はClaudeと同じで、Claudeのplugin・skill・hookに頼らない。
- 落とし穴: Codexはthreadを自分で名付けるので、session IDと実際のmodelは終わりのeventと区間の`session_closed`が持つ（`JobSession::record`）。
- 使えないproviderで失敗したjobは保留せずもう一方のproviderでやり直し、それ以外の失敗は保留してClaudeに戻さない（`domain::plan_review::failed_end`）。

## 差し戻しの分類コード<a id="差し戻しの分類コード未実装"></a>

- 一覧と定義と主の選び方: `domain::review_reason::PLAN_REVIEW_CODES`とそのmoduleのdoc（[ADR-t947-1](../../adr/2026-09-28-t947-1-review-verdicts-carry-reason-codes.md)）。
  runtimeが付けるコードは`REOPEN_OVER_LIMIT`。
- 約束: コードはverdictの判定と適用に使わない。
- runのreview（[Review](review.md#差し戻しの分類コード)）・workerの問い（[ask](ask.md#worker_questionの分類コード)）と共通のコードは同じ定義で使う。
- 人の答えからの補い: storeの`record_plan_outcome`と`domain::review_reason::answer_outcome`。

## AIが決めるconcern<a id="aiが決めるconcern未実装"></a>

- 決定は[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)の決定4（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定11をamends）。
- 判定: `PlanReviewVerdict::decide_concern`、人に残すaskの理由は`PlanConcernDecision::ask_reason`。
- AIが決めたconcernもverdictは`concern`のまま残る。
- 約束: jobが自分でしてよい修正（actionsと`reopen`）は変わらず、askを作らないので`plan_review_outcome`は記録しない。
- 決めた数の読み方は[Stats](commands.md)。
