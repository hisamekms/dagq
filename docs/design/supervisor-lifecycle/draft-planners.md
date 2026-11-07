---
id: design-supervisor-lifecycle-draft-planners
type: design
title: "Draft planners (supervisor)"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-0072
  - adr-t1704-1
  - adr-t1394-1
  - adr-t1394-2
  - adr-t451-1
  - design-supervisor-lifecycle
  - adr-t807-1
  - adr-0044
  - adr-0037
  - design-persistence
  - adr-t803-1
  - adr-t947-3
  - adr-t1504-1
  - adr-t1504-2
  - adr-t1540-1
  - adr-t2352-1
---

# Draft planners (supervisor)

[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定13・16と[ADR-t807-1](../../adr/2026-09-28-t807-1-bundle-drafts-of-one-piece-of-work-for-one-runtime-planner.md)。
runtimeやjobが作ったdraftを、同じきっかけで作られ同時に待っているものの束にし、束ごとにsupervisorがruntimeのplannerを1つ立て、plannerがdraftごとに採用・不採用・askのどれかを選ぶ。
対象はfollow_upに限らず、goalの判断jobのgapと定期の提案も含む。

## 入口と境界

出どころと束の意味は`src/domain/follow_up.rs`、対象の条件は`src/infrastructure/draft_planners.rs`の`targets_in`を読む。
出どころの登録は同moduleの`insert_draft_origin`が入口で、receiptとreviewのfollow_upは`DraftPlannerStore::register_follow_ups`を通る。
passは同じreceiptまたはreviewのdraftを全件見るか1件も見ない。
同じきっかけのdraftが作業中に増えても、開いている束のplannerが終わるまで待つ。
reopenを経たdraftをreadyに直接戻す経路は作らない: plan reviewが見つけた食い違いをgateを通さずにclaimさせないためである。

起動の入口は`src/application/supervise/planning/draft_planner.rs`の`PlanningState::open_draft_planners`。
run slotを使わず、revise・finding・計画の依頼のplannerと枠を共有する。
[Plan review](plan-review.md#reviseの配送)のreviseの配送と[依頼のplanner](plan-planners.md#inboxからの計画の依頼)がdraftより優先し、drain中は立てない。
束の対象の再検査と登録は同じtransactionなので、2つのsupervisorが同じdraftに立てない。
Spikeのfollow_upもここで扱い、その依頼のplannerは計画しない（[Spikeの再計画の依頼](plan-planners.md#spikeの再計画の依頼)）。

初期promptの材料と採否の指示は`prompt::draft_planner_prompt`が持つ。
重複と実装済みのdraftを重複先を記録して閉じる責任もこのplannerが持つ（[ADR-0063](../../adr/0063-full-text-search-related-with-mentions-and-search-strength-and-duplicate-of.md)決定5）。
promptの上限は[Prompt](prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)、repositoryの規則の読み込みは[読む順](prompt.md#repositoryの規則を読む順)を参照する。

決めずに終わった回数と例外は`MAX_DRAFT_PLANNERS`のdoc commentを読む。
使い切ったdraftはattentionになり、人の言葉を受けたinboxの参照付きの計画の依頼か、人のterminalでの判断を待つ（[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)決定8）。
人の答えだけを待って終わったplannerは枠を空け、決めずに終わった回数に数えない（[ADR-t1704-1](../../adr/2026-10-05-t1704-1-human-answer-wait-releases-runtime-planner-slots.md)決定5）。

answerの行き先は`PlannerAnswerRoute`、配送は`PlanningState::deliver_planner_answers`が入口。
束でも問いはdraftごとに開き、answerはworkerに送らない。
配送のclaimによって、引き継ぎ中にsupervisorが重なってもanswerを二重に送らない。
届けたanswerを読み終えるまで終了の依頼を送らない（[非対話の経路](#予定-人のplannerの廃止と非対話の経路)）。

採用の検査と記録は`draft_planners.rs`の`check_adoptions`が入口。
runtimeのplannerは自動採用の上限のdraftを人のadopt無しに出せない（[ADR-t808-1](../../adr/2026-09-28-t808-1-runtime-planners-submit-follow-ups-up-to-depth-two.md)）。
束の1件が人の判断を要しても、他のdraftの採用を妨げない。
人のterminalからのsubmitは人の判断として深さを0に戻す。

終了の依頼と片付けは[Plan review](plan-review.md#runtimeのplannerの終わり)、生存・時間切れ・providerが使えないときの境界は[runtimeのplannerの経路](plan-planners.md#runtimeのplannerの経路)を参照する。
idleの判定は`src/application/planner.rs`の`planner_view`が入口。
結末の記録は`draft_planners.rs`の`settle_bundle`、意味は`DraftOutcome`を読む。
元のrunから各draftの結末までeventsで引ける。
読み取りの形は`domain::views::TaskOrigin`、束の集計は`domain::stats::drafts`が入口。

## 再検討の時刻

[ADR-t1540-1](../../adr/2026-10-05-t1540-1-a-kept-draft-returns-to-runtime-planners-at-its-revisit-time.md)。
`keep_draft`で残したdraftを「この時刻に決め直す」約束をqueueが持ち、前回の判断を初期promptに載せる。
時刻の意味は`domain::DraftRevisit`、付け替えは`SqliteQueue::revisit_draft`、promptの材料は`prompt::revisit_section`を読む。
権限の境界は[Authorization](../authorization.md)の計画系のコマンドを参照する。
使った時刻では二度と立たないので、また残すなら新しい時刻を付ける。
時刻の無いkeep_draftは参照付きの依頼を待ち、再検討の経路はその依頼と並ぶ。
同じdraftに依頼と再検討のplannerを同時に立てない。
依頼が開いている間はdraftのanswerも待ち、時刻は残る（`draft_planners.rs`の`named_by_open_request`、依頼側は`plan_requests.rs`の`targets`）。

## follow_upの種類と判断の集計<a id="follow_upの種類と判断の集計未実装"></a>

[ADR-t947-3](../../adr/2026-09-28-t947-3-follow-ups-carry-category-codes.md)。
種類のpromptへの提示は`prompt::follow_up_category_line`、判断との突き合わせは`domain::stats::follow_up_categories`を読む。

## 推奨が出せればplannerが決める（ADR-t451-1）<a id="推奨が出せればplannerが決める未実装"></a>

[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)決定5。
基本方針はdagq-planner skillの「Basic policy」に揃え、要約は`prompt::DECIDE_YOURSELF`を読む。
推奨が出せれば自分で決めて理由を記録し、人に聞くのは材料で決めきれないscope・discard、低い確信度、自動採用しない上限に当たるdraftに限る。
問いの境界は`prompt::RUNTIME_PLANNER_ASK`、実際の採否の指示は`draft_planner_prompt`が持つ。
verify・paths・evidenceがrepositoryの規則で決まらないこと自体は、人に聞く理由にならない。
判断を経たかの集計は[種類と判断の集計](#follow_upの種類と判断の集計未実装)を参照する。

## 人のplannerの廃止と非対話の経路<a id="予定-人のplannerの廃止と非対話の経路"></a>

[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)と[ADR-t1394-2](../../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)。
人が開くplannerは無く、計画はinboxへの依頼を通し、runtimeのplannerは常に非対話のbackgroundのwrapperで動く。
古い対話のplannerの行は閉じた行として読む（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定5、[終わりと片付け](plan-planners.md#終わりと片付け)）。
answerは次のturnの依頼として届き、読み終えた判定は`PlanningState::headless_answer_taken`、依頼の記録が無い場合は`answer_taken`を読む。
落とし穴: askを閉じた時刻と比べると、配送後close前にidleになったplannerが枠をふさぐ。
providerの壁で失敗したturnを読み終えたとするとanswerが失われるので、retryのturnまで追う。
非対話の状態と互換の設定の境界は[runtimeのplannerの経路](plan-planners.md#runtimeのplannerの経路)を参照する。

## 所属の判断

`judge-follow-up`は採用・priorityとは別に元goalのacceptanceとの関係を記録する。
required/out_of_scopeの所属変更は判断と同じtransactionで行い、set-goalによる食い違う変更は拒む。
登録時の出どころ・状態と深さは移動しても変わらないので、開いたgoalへの移動で人のadoptを迂回できない。
元goalのあるfollow_upのdraftは、今の判断が無い・undecided・要再確認ならsubmitとdraft / submittedのreadyのbypassが拒む。
所属の判断と人のadoptは別の検査で、判断を記録してもadoptにはならない。
lintと[Plan review](plan-review.md)もこの判断を材料に検査する。
判断と訂正の規則は[所属の判断](../follow-up-membership.md)、plannerへの指示は`prompt::follow_up_membership_step`を読む。
follow_upを外すためにacceptanceを弱めず、人に聞く（[ADR-t1504-1](../../adr/2026-10-04-t1504-1-follow-ups-belong-to-the-goal-whose-acceptance-needs-them.md)）。

## recurringのdraft<a id="予定-recurringのdraft未実装"></a>

[ADR-0072](../../adr/0072-recurring-drafts-from-dagq-toml.md)決定2〜4。
[定期設定](run-environment.md#定期draft)から登録したdraftも同じplannerが扱う。
登録の入口は`src/infrastructure/recurring_drafts.rs`、周期と重なりの規則は`domain::recurring`、採否の材料は`prompt::recurring_origin`を読む。
登録はtransactionで再検査し、引き継ぎ中にsupervisorが重なっても二重に登録しない。
上限やkeep_draftで残ったdraftも、次の周期で迂回しない。
不要な定期の提案をcancelすることだけで人に聞かない。
