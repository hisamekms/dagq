---
id: design-supervisor-lifecycle-finding-planners
type: design
title: "Finding planners (supervisor)"
status: current
created: 2026-09-27
scope: runtime
related:
  - adr-t1920-1
  - adr-t1704-1
  - adr-t1394-1
  - adr-t1394-2
  - adr-t451-1
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-draft-planners
  - design-supervisor-lifecycle-observer
  - adr-0047
  - design-persistence
  - adr-0051
---

# Finding planners (supervisor)

[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定18〜20。
proposalを求める印の付いたfindingごとに、supervisorがruntimeのplannerを1つ立て、plannerがproposalを作ってplan reviewに出すか、findingを`dismissed`にするか、人に聞く。
人の承認は一律には求めず、人に聞くのはplannerが`planner_question`にしたときとplan reviewが`concern`にしたときだけ。
use caseは`src/application/supervise/finding_planner.rs`、storeは`src/infrastructure/finding_planners.rs`（port `DraftPlannerStore`に足したメソッド）、規則は`src/domain/finding.rs`、promptは`application::prompt::finding_planner_prompt`。
run slotは使わず、[Plan review (supervisor)](plan-review.md#plan-review-supervisor)の`tend_planners`の中で進める。

1. **印**: (a) observerが`finding record --propose`で付ける（[Observer](observer.md)）。
   (b) 人がaskに`propose`と答えると、`answer`の同じトランザクションでruntimeが付ける（下の5）。
   印は`open`のfindingにだけ付き、`proposed`のfindingには付かない。
2. **対象**（`planner_findings`）: statusが`open`で印があり、閉じていないruntimeのplannerが無く、closeされていない`planner_question`が無く、今の印の後に立てたplannerを使い切っていないfinding。
3. **立てる**（`open_finding_planners`）: runtimeが立てたplannerで生きているものが上限`runtime_planners`（[Run environment](run-environment.md)の`[supervisor]`。revise・draft・計画の依頼（[inboxからの計画の依頼](plan-planners.md#inboxからの計画の依頼)）のplannerと共有、run slotとは別、人が開いたplannerは数えない）未満のあいだ、`open_finding_planner`で立てる。
   storeは`BEGIN IMMEDIATE`で対象の条件を再検査してから`planners`の行と`finding_planner_opened`を書くので、2つのsupervisorや同じsupervisorの2つのpassが同じfindingに立てない。
   同じトランザクションで改善の数を数え、上限に達していれば何も書かずに`FindingPlannerStart::AtLimit`を返し、そのpassでは後のfindingにも立てない（下の11）。
   eventはfindingの対象（runならrunとtask、task、goal、queueなら何にも付かないqueueのevent）に付く。
   plannerはworkspaceを持たず、非対話のwrapperがbackgroundで起動する（[runtimeのplannerの経路](plan-planners.md#runtimeのplannerの経路)）。
4. **初期prompt**（`finding_planner_prompt`、載せるものは`FindingPlannerMaterial`）: findingとその根拠、根拠と記録を読むCLI、findingに紐づくaskとそのanswer、対象のgoalとそのtask、手順を載せる。
   手順は、`dagq search` / `related`で既存のtaskと実装済みを確かめてから、(1) 既存のopenなgoalにtaskを足してfindingを名指してsubmit、(2) 範囲に入るgoalが無ければdraftのgoalとtaskを書いてsubmit（新しいgoalでも人の承認は要らない）、(3) 手当てしない（既存のtaskで足りる、もう起きていない、割に合わない）なら`dagq finding dismiss`で理由を書く、(4) 自分で決めきれないもの（[推奨が出せればplannerが決める](#推奨が出せればplannerが決める未実装)の条件）だけ`planner_question`で聞いて止まる、のどれか1つ。
   verify・paths・evidenceは`prompt::repository_rules`の順で決めさせる（[Prompt](prompt.md#repositoryの規則を読む順)）。
   人のanswerを運ぶplannerには元の質問とanswerを載せる。
   promptは全体と節ごとの上限の中に収め（値と理由は[Prompt](prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)）、根拠のeventは新しい順に選び、省いたものはその件数と読むだけの`dagq`のコマンドを書く。
5. **askの「提案にする」**（`answer`の中、`finding_planners::apply_answer`）: findingに紐づいた`blocked`のaskには、askを作るときにruntimeがoptionsの末尾に`propose`と`dismiss`を足し、`stalled`のaskには`propose`を足す（復旧jobが足すoptionの後）。
   askが載せたoptionへの答えだけを適用する。
   `propose`はfindingに印を付け、`resolved` / `dismissed`のfindingは`open`に戻す。
   findingの無い`stalled`のaskでは、先にaskのrun（無ければtask、queue）を対象に、askの`ask_opened`を根拠にしたfindingを記録し（同じ種類・対象・subjectの閉じていないfindingがあればその更新）、askに結ぶ。
   `dismiss`はaskのfindingを`dismissed`にする。
   適用したanswerはattentionにならない。
   `blocked`のaskはその場で閉じ、`stalled`のaskはrunのstallの見張りが閉じる。
   それ以外の答えと`planner_question`の答えはここで適用せず、ふだんの行き先に送る。
6. **submitでの紐づけ**（`submit_linking`、`submit`と同じトランザクション）: `submit --finding`で名指したfindingと、submitしたruntimeのplannerが立てられたfindingを、proposalに紐づける。
   `open`のfindingは`proposed`になりproposalを持ち、同じproposalで既に`proposed`（resubmit）は変えない。
   名指したfindingが紐づけられない（他のproposalで`proposed`、`resolved` / `dismissed`）ならsubmit全体が失敗する。
   plannerの自分のfindingは`open`のときだけ紐づける。
   紐づいたproposalはplan review（ADR-0047の決定10〜15）を通る。
   改善のproposalのtaskは個別の優先度を持たずgoalから継ぐ（ADR-t1971-1決定4）。
   plannerの初期promptは`--priority`を付けず、受け入れ条件に要るときだけ`normal`以下の既存のgoalに入れ、ほかは新しいgoal（改善は`normal`以下）かgoalの無いtaskにするよう書き、上限はtaskの値でなくgoalと所属で守る（plan reviewが見て外れれば`revise`）。
   follow_upの自動採用の上限（決定16）はfindingのproposalには掛からない（draftの出どころが無いので`check_adoptions`の対象外）。
7. **終わったproposal**（`settle_findings`、各passで立てる前）: `proposed`のfindingの紐づいたproposalが`canceled`になったか、そのtaskがすべて`completed` / `canceled`になったら、1つ以上`completed`なら`resolved`、1つも無ければ`open`に戻す。
   `open`に戻すときはproposalと印を外すので、もう一度proposalにするには新しい印が要る。
8. **回数の上限**: plannerがfindingを決めずに終わった（sessionが終わった、idleで終了の依頼を受けて終わった）findingは次のpassで再び対象になる。
   今の印の後に立てたplannerのうち、人の答えだけを待って終わったもの（10）を除いた数が`MAX_FINDING_PLANNERS`に達したfindingは（人のanswerを運ぶとき以外は）立てずに`finding_planner_exhausted`を記録する。
   attentionは`AttentionNext::DecideFinding`で、findingの対象のtask / runに付き、印のある`open`のあいだ`status`に出る（`watch`も同じ文面）。
   人の言葉を受けたinboxがそのfindingを参照にした計画の依頼（[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)の決定8）を記録し、依頼のruntimeのplanner（[inboxからの計画の依頼](plan-planners.md#inboxからの計画の依頼)）が決めるか、人が`DAGQ_ROLE`の無い自分のterminalで`submit --finding`するか`finding dismiss`する。
9. **planner_question**: `planner_question`はfindingを持てる（taskもrunも無くてよい）。
   answerの行き先（`planner_answer_route`）はfindingで決める。
   runtimeのplannerが一度も立っていないfindingなら人（runtimeのplanner以外の質問）。
   閉じていないそのfindingのruntimeのplannerがあれば、answerはそのplannerに届く。
   無ければ、findingがまだ対象（`open`、印あり、使い切っていない）なら上限の中で新しいplannerがanswerを持って立ち、そうでなければsupervisorが`planner_answer_closed`で閉じる。
   送れなかったときは`ask_delivery_failed`を残し、plannerは人の答え待ちと同じく終わり、answerは新しいplannerが運ぶ（[Draft planners](draft-planners.md)の5）。
   配送のeventはfindingの対象に付き、queueが対象のfindingではqueueのeventになる（`EventKind::is_queue`）。
10. **終わり**: runtimeのplannerは、自分のfindingの`planner_question`のanswerを届ける前と、送ったanswerをまだturnで読み終えていないあいだ（`headless_answer_taken`。[Draft planners](draft-planners.md)の5と同じ）は終了の依頼を受けない。
    それ以外でidleになれば終了の依頼を受けてwrapperがsessionを終え、行は閉じられ、まだ動いているbackgroundのwrapperは止められる（`end_runtime_planners`）。
    runtimeのplannerにcmuxのworkspaceは無く、画面も読まない。
    idleはwrapperがturnの終わりに書くidle markerで判じ、時間は`--planner-timeout`でなくturnの`[stall]`の上限で見る。
    wrapperがturnを`[stall]`の上限で止めたときは、plannerごとに1回`planner_unresponsive`がinboxに届き、止めたturnでsessionが終わるので、決めずに終わったplannerに数える（`tell_of_stopped_planner_turns`）。
    最後のturnがClaudeのログイン切れ・利用上限・起動の失敗で終わったplannerは、Claudeを待って終了の依頼を受けず、決めずに終わったplannerにも数えない（`provider_wall`）。
    どちらも[runtimeのplannerの経路](plan-planners.md#runtimeのplannerの経路)の「状態」と「使えないとき」。
    未回答の`planner_question`のほかに待つものが無いidleのplannerは、人の答えだけを待つとして終了の依頼を受け、行は`runtime_answer_wait`で閉じ、askは開いたまま残る（[ADR-t1704-1](../../adr/2026-10-05-t1704-1-human-answer-wait-releases-runtime-planner-slots.md)決定1。[runtimeのplannerの経路](plan-planners.md#runtimeのplannerの経路)の「人の答えだけを待つplannerの終わり」）。
    findingは2のとおりそのaskが閉じるまで対象にならず、answerで9の新しいplannerが続ける。
11. **改善の上限**（ADR-0051の決定25）: 動いている改善（`SqliteQueue::improvements`）は、findingが指す別々のproposalで持ち主がruntimeのplannerのもののうち`finding::improvement_running`のものと、閉じていないruntimeのplannerのうちfindingが`open`のもの（submitすればfindingが`proposed`になり、そのproposalの方を数える）の合計。
    ADR-0051の決定25は数え方をproposalで書くが、runtimeのplannerも数えるのは、同時に立ったplannerがそれぞれsubmitして上限を超えないため（決定25の「plannerがすでに立った後に上限を超えることはない」）。
    上限は`dagq.toml`の`[kpi]`の`max_improvement_proposals`で、supervisorは毎回読み直す。
    合計が上限以上のあいだ、`open_finding_planner`は（人のanswerを運ぶとき以外は）plannerを立てず、findingは`open`のまま待つ。
    eventは書かない。
    人が`submit --finding`したproposal（持ち主が`person`）とfollow_upのdraftの採否は数えず止めない。
    `dagq findings`は改善の数と上限と待つfindingを返し、observerの入力にも同じものが載る。
12. **test**: `tests/it/finding_planner.rs`がplan reviewのbackendとstubのplan reviewで上の流れを確かめる。
    runtimeのplannerのwrapperはbackgroundに停めてtestが演じ、cmuxのdouble・画面・打ち込みは使わない。
    規則は`src/domain/finding.rs`のunit test。

## 推奨が出せればplannerが決める（ADR-t451-1）<a id="推奨が出せればplannerが決める未実装"></a>

[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)の決定5。
findingのplannerも、[Draft planners](draft-planners.md#推奨が出せればplannerが決める未実装)と同じく、dagq-planner skillの「Basic policy」に従う（`finding_planner_prompt`の「What to do」、`prompt::DECIDE_YOURSELF`）。
推奨が出せる判断（既存のgoalへのtask、新しいgoal、`finding dismiss`）は自分で決め、理由をproposalのtaskの`--context`（`from finding N (kind)`に続けて、その手当てを選んだ理由）かfindingの`--reason`に書く。
`planner_question`は、ADR-0047決定41の`scope`・`discard`に当たりqueue・repository・ADR・人の先例で決めきれないものと確信度が`low`のものだけにし（repositoryの規則でverify・paths・evidenceが決まらないこと自体は理由にしない。`prompt::RUNTIME_PLANNER_ASK`）、推奨と確信度を載せる。

## 人のplannerの廃止と非対話の経路<a id="予定-人のplannerの廃止と非対話の経路"></a>

[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)と[ADR-t1394-2](../../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)。
人が開くplannerは無く、`dagq plan`は何も開かずにinboxへの計画の依頼の案内を付けて拒む（ADR-t1394-1）。
人が開いたplannerの閉じていない行は、supervisorがcmuxを呼ばずに閉じた行として扱い（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定5。[`plan` / `planners`](plan-planners.md)の「人のplannerの終わり」）、その持ち主のdraftへのanswerは新しいruntimeのplannerが受ける。
非対話の経路（ADR-t1394-2の決定1・2・3・5）では、answerは次のturnの依頼として届き、読み終えたかとClaudeを待つplannerの扱いは[runtimeのplannerの経路](plan-planners.md#runtimeのplannerの経路)の「状態」と「使えないとき」が持つ。
残りの予定は[予定: inboxからの依頼と非対話のplanner](plan-planners.md#予定-inboxからの依頼と非対話のplanner)にある。

- 9の「runtimeのplannerが一度も立っていないfindingのanswerは人」は、人が開いたplannerの質問だけに当たる。
  findingを決める経路は、inboxがそのfindingを参照にした計画の依頼を記録し、依頼のplannerが`submit --finding`か`finding dismiss`で決めるものになる。
  人が`DAGQ_ROLE`の無いterminalで打つ`submit --finding` / `finding dismiss`は残る。
  依頼のplannerがsubmitしたproposalは、依頼とfindingの両方に結ばれる。
- findingのplannerは経路を選ばず、いつも非対話でbackgroundのwrapperだけで動く（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定3・[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)決定1）。
  `[roles.runtime_planner]`の`route`は受け付けて無視する（[runtimeのplannerの経路](plan-planners.md#runtimeのplannerの経路)）。
  起動は3、submitの持ち主は6、answerの配送は9、終わりは10で（`headless::send_to_planner`）、runtimeのplannerにはcmuxのworkspace・画面・打ち込み・`/exit`が無い。
  11の改善の上限は依頼のplannerを、findingを持たないかぎり数えない。

## CIの見張りの`ci_failure`のfinding

[ADR-t1920-1](../../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)決定4。
`[ci_watch]`で有効にしたrepositoryでは、supervisorが着地先のbranchのCIの赤から`ci_failure`のfindingを印付きで記録し（observerでなくruntimeの書き込み）、上の1〜11の経路でplannerが立つ。
上限（11）と優先度（6のgoalから継ぐ形）は変えない。
`finding_planner_prompt`は`ci_failure`のときに節を足し、findingの`detail`の各項を修正taskの説明に写すこと、同じtestを直す閉じていないtaskを探して、あればtaskを足さずにfindingをそのtaskが覆うものとして`finding dismiss`すること、修正taskに改善の規則どおり優先度を付けないこと（6）を指示する。
findingの項目が全部CIで通れば、そのfindingが`open`で閉じていないplannerが無いときに限りsupervisorが`resolved`にする。
findingの欄と鍵は[CI watch](ci-watch.md)の「finding（修正taskの材料）」。
