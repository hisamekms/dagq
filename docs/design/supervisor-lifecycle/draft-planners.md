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
---

# Draft planners (supervisor)

[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定13・16と[ADR-t807-1](../../adr/2026-09-28-t807-1-bundle-drafts-of-one-piece-of-work-for-one-runtime-planner.md)。
runtimeやjobが作ったdraftを、同じきっかけで作られ同時に待っているものの束にし、束ごとにsupervisorがruntimeのplannerを1つ立て、plannerがdraftごとに採用・不採用・askのどれかを選ぶ。
対象はfollow_upのdraftに限らず、runtimeやjobが作ったdraft全般（follow_up、goalの判断jobのgap）とする。
再検討の時刻（[ADR-t1540-1](../../adr/2026-10-05-t1540-1-a-kept-draft-returns-to-runtime-planners-at-its-revisit-time.md)）は下の[再検討の時刻](#再検討の時刻)。
use caseは`src/application/supervise/draft_planner.rs`、storeは`src/infrastructure/draft_planners.rs`（port `DraftPlannerStore`）、規則と型は`src/domain/follow_up.rs`、promptは`application::prompt::draft_planner_prompt`。
run slotは使わず、[Plan review (supervisor)](plan-review.md#plan-review-supervisor)の`tend_planners`の中で、reviseの配送の後に進める（drain中は立てない）。

1. **出どころ**: runtimeやjobがdraftを作るときに、出どころと材料を`record_draft_origin`で`draft_origins`に書く。
   `integrate`の`register_follow_ups`はfollow_upのdraftに、workerが付けた種類（[follow_upsの分類コード](receipt-and-session-exit.md#follow_upsの分類コード)、ADR-t947-3）と所属の提案（[follow_upsの所属の提案](receipt-and-session-exit.md#follow_upsの所属の提案)）を材料として書く。
   材料に所属の提案の無いdraftはpromptが`(none)`として、種類の無いdraftはpromptと集計が`unlabeled`として扱う。
   `goal_gap`は、[goal review](goal-review.md)のjobの`gaps`のverdict（と人の`approve_goal`の`gaps`のanswer）からruntimeが登録するdraftの出どころとする。
   人が`add`で作ったdraftとobserverのdraft goalのtaskには行が無く、plannerは立たない（再検討の時刻が来たときだけ、出どころ`revisit`として立つ。[再検討の時刻](#再検討の時刻)）。
   出どころ`reopened`は、plan reviewがreadyから戻して自分のproposalに入れたtask（ADR-0044の決定14）が、そのproposalのwithdrawで`draft`に戻ったときにwithdrawが`draft_reopens`に書く。
   人が作ったtaskで`draft_origins`の行が無くても、これで誰かが拾う。
   行が効くのはtaskがまだそのproposalのものであるあいだだけで、後で別のproposalに入ったtaskのwithdrawには古い行が効かない。
   同じproposalの他のmember（plannerがsubmitしたdraft）には書かず、`draft`に戻るだけとする。
   readyに直接戻す経路は作らない（reopenはplan reviewが食い違いを見つけた結果なので、gateを通さずにreadyに戻すと食い違ったままclaimされうる）。
   withdrawとreopenの繰り返しは下の4の上限（`draft_planner_exhausted`）で止まる。
2. **対象**（`planner_drafts`）: 条件はstoreの`targets`のdoc commentが持ち、要点は次のとおり。
   statusが`draft`でproposalに入っておらず、出どころか（有効な）reopenの行があるか再検討の時刻が来ていて、閉じていないruntimeのplannerも、closeされていない`planner_question`のaskも無く、`keep_draft`のanswerで残されておらず、まだ来ない再検討の時刻を持たず、`open`の計画の依頼の参照（`task:N`）に無く、使い切っていないdraft（時刻が来たときの例外は[再検討の時刻](#再検討の時刻)）。
   ただし閉じていないplannerの束と同じ束の鍵を持つdraftは、そのplannerが終わるまで待つ（同じきっかけのdraftが作業中に増えたとき）。
   integrateは1つのreceiptの未登録follow_upsを1つの`BEGIN IMMEDIATE` transactionで登録するので、passはそのreceiptのdraftを全件見るか1件も見ない。
   束のplannerが`planner_unresponsive`で開いたまま人を待つあいだも、同じ鍵のdraftは待つ。
   - **束**（ADR-t807-1）: 対象を束の鍵（`BundleKey::of`）で分ける。
     鍵は同じintegrate・同じgoal review・同じwithdrawが作ったことを表す。
     作成時刻の近さでは束ねず、材料に鍵の無いdraftは1件で1つの束とする。
     束の順は束の最も古いdraftのID順。
   - Spikeのfollow_upにも立ち、その依頼のplannerは計画しない（[依頼](plan-planners.md#spikeの再計画の依頼)）。
3. **立てる**（`open_draft_planners`）: runtimeが立てたplannerで生きているものが上限`runtime_planners`（[Run environment](run-environment.md)の`[supervisor]`）未満のあいだ、束ごとに`open_draft_planner`で立てる（束1つで1と数える）。
   上限はrevise・finding・計画の依頼（[inboxからの計画の依頼](plan-planners.md#inboxからの計画の依頼)）のplannerと共有し、run slotとは別とする。
   依頼のplannerはdraftより先に立てる。
   storeは`BEGIN IMMEDIATE`で束の全てのdraftの対象の条件を再検査し、条件を外れたdraftは束から落として残りで、`planners`の行、束の行、draftごとの`draft_planner_opened`を書く（[Persistence](../persistence.md#draft-plannersdraft_plannersrs)）ので、2つのsupervisorが同じdraftに立てない。
   plannerはworkspaceを持たず、非対話のwrapperがbackgroundで起動する（[runtimeのplannerの経路](plan-planners.md#runtimeのplannerの経路)）。
   初期promptは`draft_planner_prompt`（`prompt::repository_rules`を含む。[Prompt](prompt.md#repositoryの規則を読む順)）で、draftごとの中身、出どころ（1回だけ）、束のdraftのgoalと同じgoalの束の外のtask、手順を載せる。
   束が2件以上なら、束の中の重複・依存と、採用するものを1つのproposalで出すことを先に考えさせる。
   draftごとの3択は、採用（出自を補い、依存を足し、lintしてからsubmit）、不採用（cancelとnoteで理由）、自分で決めきれないときの`planner_question`（[推奨が出せればplannerが決める](#推奨が出せればplannerが決める未実装)の3つの条件）で、answerに従う。
   reopenedのdraftは、taskの意図を保ってreasonの指す所を直す。
   他のtaskの重複か、completedのtaskで実装済みのdraftを重複先を記録して閉じるのはこのplanner（[ADR-0063](../../adr/0063-full-text-search-related-with-mentions-and-search-strength-and-duplicate-of.md)の決定5）。
   記録を読むCLIは`prompt::RECORD_READING`（ADR-0044の決定22）が持つ。
   promptは全体と節ごとの上限の中に収める（値と理由は[Prompt](prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)）。
   draftの節は束の古い順に選び（descriptionは必須で、超えたら切って`over_limit`に書く）、入らないdraftとgoalは件数と読むCLIを書く。
   出どころの節は丸ごとと欄ごとに切り、receiptを読むCLIを書く。
   promptのbyte数は`planner_prompt_written`に記録する。
4. **上限まで**（`MAX_DRAFT_PLANNERS`）: plannerがdraftを決めずに終わった（sessionが終わった、idleで終了の依頼を受けて終わった）draftは次のpassで再び対象になり、残りのdraft（とその間に増えた同じきっかけのdraft）で束を作り直して次のplannerが立つ。
   回数はdraftごとに数える（`draft_bundle_members.attempt`）。
   再検討の時刻で立つplannerも、answerを運ぶplannerも1回と数え、作業中の失敗・時間切れも数えるが、人の答えだけを待って終わった（結末`answer_wait`、5）ものは数えない（[ADR-t1704-1](../../adr/2026-10-05-t1704-1-human-answer-wait-releases-runtime-planner-slots.md)決定5）。
   この数が`MAX_DRAFT_PLANNERS`に達したdraftは（人のanswerを運ぶときと、人・inboxが付けた再検討の時刻が来たとき以外は）束から外して`draft_planner_exhausted`を記録し、束の残りで立てる。
   attentionは`AttentionNext::DecideDraft`で、draftのまま残っているあいだ`status`と`watch`にも出る。
   人の言葉を受けたinboxがそのdraftを参照（`task:N`）にした計画の依頼（[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)の決定8）を記録し、依頼のruntimeのplanner（[inboxからの計画の依頼](plan-planners.md#inboxからの計画の依頼)）が決めるか、人が`DAGQ_ROLE`の無い自分のterminalで決める。
5. **planner_question**（ADR-0044の決定13）: askのkind`planner_question`は、束の中でも`--task ID`でdraftごとに開く。
   `answer`は`planner_answer_route`で行き先を決め、`Person`以外なら`ask_answered`に`runtime_delivers`を書いてattentionにしない。
   行き先は次の4つ。
   - (a) そのtaskで働く閉じていないruntimeのplanner（draftを持つ束のplannerか、taskのproposalのplanner）: supervisorがplannerの`idle`（wrapperがturnの終わりに書くidle markerがaskより新しく、`turns/`に待っている依頼が無い）を待つ。
     送る前に`claim_planner_answer`で配送をclaimし（`BEGIN IMMEDIATE`で、askが未closeで行き先が今もそのplannerであり、同じaskの`planner_answer_claimed`が`PLANNER_ANSWER_CLAIM_SECS`以内に無いことを確かめ、`planner_answer_claimed`をtaskに書く。claimできなければ送らない）、answerをplannerの次のturnの依頼として書き、`ask_delivered`でaskを閉じる。
     plannerには何も打ち込まない。
     これで引き継ぎの間に同じqueueを扱う2つのsupervisorが重なっても、answerは1回しか送られない。
     送れなかったときは`ask_delivery_failed`を残してそのplannerには送り直さず、人の答えだけを待つplannerと同じく終わらせ、answerは(b)が運ぶ（[runtimeのplannerの経路](plan-planners.md#runtimeのplannerの経路)）。
     claimの後、送る前にsupervisorが落ちたaskは、claimが`PLANNER_ANSWER_CLAIM_SECS`より古くなった後の周回で別のsupervisorがclaimし直して送る（依頼を書くのは数秒で終わるので、生きているsupervisorのclaimを奪わない）。
   - (b) plannerが居ないdraft: 上限の中で新しいplannerを立て（そのdraftを先頭に、同じ束の鍵で対象のdraftを束に加える。同じ鍵の束のplannerが開いていれば、それが終わるまで待つ）、初期promptに元の質問とanswerを載せ、askを閉じる。
     依頼のplannerが作り由来もproposalも無いdraftは依頼のplannerに届け、居なければ依頼の状態に依らず答えを載せたplannerを依頼に立てる（ADR-t2015-1）。
   - (c) `keep_draft`のanswerでplannerが居ない、またはdraftが先に進んだ（submit、cancel）: supervisorが`planner_answer_closed`でaskを閉じる。
     `keep_draft`で残したdraftにはruntimeのplannerを立て直さず、人の言葉を受けたinboxがそのdraftを参照にした計画の依頼を記録するまでdraftのまま待つ（`PLANNER_QUESTION_OPTIONS`のdoc comment）。
     ただし再検討の時刻を付けたdraftは、その時刻にruntimeのplannerの対象に戻る（依頼の経路と並ぶ。[再検討の時刻](#再検討の時刻)）。
   - (d) それ以外（依頼のplanner以外が作った出どころの無いtask、使い切ったdraft）: inboxが受け、workerには送らない。
     draftなら人の答えを計画の依頼（taskとaskの参照付き）にしてaskを閉じ、他はanswerを読んで閉じる。
   送ったanswerをまだturnで読み終えていないplannerと、answerを届ける前のplannerには終了の依頼を送らない（送らずに閉じたaskは引き止めない。`question_wait`）。
   読み終えたかはturnの記録で決める（`headless_answer_taken`、[下](#予定-人のplannerの廃止と非対話の経路)）。
   idle markerの時刻やaskの`closed_at`（送った後に`ask_delivered`が書く）とは比べない: answerを読んで仕事を終えたplannerはaskのcloseより前にidleになりうるので、比べると答えの処理中とみて終わらせず、plannerの枠をふさぐ。
   plannerが自分で作ったaskはplannerがaskの後に止まってから、他の人が作ったaskはplannerがidleなら送る。
   未回答の`planner_question`だけを待つplannerは終了の依頼を受け（[runtimeのplannerの経路](plan-planners.md#runtimeのplannerの経路)の「人の答えだけを待つplannerの終わり」）、その後のanswerは行が閉じて束の鍵が空いてから(b)が前のplannerのnote・draftと運ぶ。
   質問したdraftは結末`answer_wait`（8）、質問していないものは先に決めさせ、残れば`undecided`。
6. **採用の記録と上限**（`submit`の中、[persistence](../persistence.md)）: submitが出どころのあるdraftを初めて出すとき`follow_up_adopted`（goal_gapとreopenedは`draft_adopted`）を書き、plannerが決めたか人が決めたか（`by`）を残す。
   上限はdraftごとで、束の中の1件が人の`adopt`を要しても、他のdraftは出せる。
   runtimeのplanner（`DAGQ_PLANNER_ORIGIN=runtime`）は、登録時の元goalが無い・閉じていた・不明か、現在のgoalが閉じているか無い、または`follow_up_depth`が`FOLLOW_UP_ASK_DEPTH`（[ADR-t808-1](../../adr/2026-09-28-t808-1-runtime-planners-submit-follow-ups-up-to-depth-two.md)）以上のfollow_upのdraftを、そのdraftへの`planner_question`に人が`adopt`と答えていなければsubmitできない（errorで`planner_question`を促す）。
   それより浅い深さは、登録時の元goalと現在のgoalがともに開いていれば人を経ずにsubmitできる。
   人のsubmit（人が`DAGQ_ROLE`の無い自分のterminalで打った`submit`）、人の`adopt`を経たsubmit、`ready --bypass-review`は深さを0に戻す。
7. **終わり**: runtimeのplannerは束のdraftをsubmitしたか、cancelしたか、`keep_draft`で（answerか、まだ来ない再検討の時刻で）残したら（決めずに残したものがあっても）、idleになったところで終了の依頼（`turns/exit`。`planner_exits`に記録）を受け、wrapperがsessionを終える。
   sessionの終わったplanner（exitした、見失った、backgroundのwrapperが居ない）の行は閉じられ、まだ動いているwrapperは止められる。
   終了の依頼から`SessionWrappers::exit_timeout`を過ぎても終わらなければ、wrapperとその子孫を止めて行を閉じる（[Plan review (supervisor)](plan-review.md#plan-review-supervisor)の`end_runtime_planners`）。
   runtimeのplannerにcmuxのworkspaceは無いので、閉じるworkspaceも無い。
   idleは、wrapperがturnの終わりに書くidle markerが最後の入力より新しく、`turns/`に待っている依頼が無いことで判じ、画面は読まない。
   `planner_question`のanswerを送られたplannerも、それをturnで読み終えれば（5）、人の答えだけを待つplannerも（5、`runtime_answer_wait`）終了の依頼を受ける。
   持ち主の居ないreviseが上限で`--planner-timeout`を過ぎて待つときは、人の答えを待っていないidleのplannerが終了の依頼を受けて枠を空ける（`planner_released`。[Plan review (supervisor)](plan-review.md)の9）。
   runtimeのplannerの時間は`--planner-timeout`でなくturnの`[stall]`の上限で見る: wrapperがturnを`[stall]`の上限で止めたときに、plannerごとに1回`planner_unresponsive`がinboxに届き、止めたturnでsessionが終わるので、plannerは閉じられ、決めずに終わったplannerに数える（`tell_of_stopped_planner_turns`）。
   最後のturnがClaudeのログイン切れ・利用上限・起動の失敗で終わったplannerは、Claudeを待って終了の依頼を受けず、決めずに終わったplannerにも数えない（`provider_wall`）。
   どちらも[runtimeのplannerの経路](plan-planners.md#runtimeのplannerの経路)の「状態」と「使えないとき」。
   submitしたproposalはplan reviewを通り、reviseは持ち主（閉じていれば新しいruntimeのplanner）に返る。
8. **記録と読み方**（ADR-t807-1）: plannerの行が閉じるとき（`close_planner`）、束のdraftごとの結末（`DraftOutcome`）を`draft_bundle_members`に書き、draftに`draft_planner_settled`を記録する。
   結末`answer_wait`（人の答えだけを待って終わった）は4で数えない。
   出どころのあるdraftのeventは束の鍵と出どころの欄を載せ、`events`のtaskとrunの絞り込み（runはpayloadの`source_run_id`も見る）で、元のrunから各draftの結末まで引ける。
   `dagq show ID`（`--full`でなくても）はtaskの`origin`と`revisit`（[再検討の時刻](#再検討の時刻)の「記録」）と、元のtaskなら`follow_up_drafts`（そのrunのreceiptが出したdraftと今の状態）を出す。
   人が`add`したtaskとobserverのdraft goalのtaskの`origin`はnull（再検討の時刻で立ったplannerがあってもnull）で、出どころの行があればどのdraftも出す。
   `dagq planners`はruntimeのplannerの束を、`dagq stats`の`draft_flow`は窓の中の束の数と大きさを出す。

## 再検討の時刻

[ADR-t1540-1](../../adr/2026-10-05-t1540-1-a-kept-draft-returns-to-runtime-planners-at-its-revisit-time.md)。
draftに再検討の時刻を1つ持たせ、時刻が来たらruntimeのplannerの対象に戻し、前回の判断を初期promptに載せる。
`keep_draft`で残したdraftを「この時刻に決め直す」約束をqueueが持つ。

- **持ち方**: 表`draft_revisits`にdraftごとに1行持ち、型は`domain::DraftRevisit`。
  条件（taskの着地の後など）は持たない（ADR-t1540-1の代替案）。
- **付ける**（`dagq revisit`、`SqliteQueue::revisit_draft`）: draftだけが持てる。
  付け直せば置き換え（使った時刻も新しい時刻で置き換わる）、消すと行を消す（無ければerror）。
  権限は`task.write`（[Authorization](../authorization.md)の計画系のコマンド）: user・inbox・plannerに許し（plannerは開始前のtaskだけ）、worker・observer・job・supervisorは拒む。
  plannerは`draft_planner_exhausted`のあるdraftに付けられない（`revisit_refusal`）。
  runtimeのplannerはpromptの3の`keep_draft`の説明に従い、決められる時が分かれば（taskの着地の後、測る期間の後）、`planner_question`の答えの後でも、推奨が`keep_draft`なら聞かずにでも、`keep_draft`で残すときに時刻を付ける。
- **対象に戻す**: 2の対象の条件のうち、時刻が来ていて使っていないdraftは、`keep_draft`のanswerによる除外を受けず、`draft_origins`の行が無い人のdraftも出どころ`revisit`（`planned_origin`）として対象になる。
  まだ来ない時刻を持つdraftは`keep_draft`のanswerが無くても対象にしない（時刻付きで残したplannerの後に次がすぐ立たないように）。
  `draft_planner_exhausted`のあるdraftは、人・inboxが付けた時刻のときだけ対象になる。
  時刻が来たdraftは1件で1つの束とする（`DraftTarget::bundle_key`）。
- **立てる**: `open_draft_planner`が束の再検査の後に、時刻が来たdraftの時刻を使い切り（`draft_revisit_due`）、`draft_planner_opened`に時刻と付けたroleを足す。
  plannerが付けた時刻のdraftが上限に達していれば、`draft_planner_exhausted`を記録し、時刻も使い切る。
  人・inboxが付けた時刻は上限を越えて1回立つ。
  そのplannerがまた残すなら新しい時刻を付けさせる（使った時刻では二度と立たない）。
- **prompt**（`draft_planner_prompt`の`revisit_section`）: 時刻・付けたroleとactor・note、前回の判断（そのdraftの`planner_question`の質問・推奨・確信度・answerと、draftのnoteの本文を新しい順に）を載せる。
  supervisorが`show`から集める（`RevisitHistory`）。
  節には上限がある。
  出どころ`revisit`のdraftは「Where it came from」に人がaddしたことを書く。
- **依頼の経路と並べる**: `keep_draft`で残したdraftの行き先（inboxの参照付きの依頼か人のterminal。5の(c)）はそのままで、時刻の経路を並べる。
  `open`の計画の依頼が`task:N`で参照するdraftは対象にしない（依頼のplannerが決める。時刻は残る）。
  そのdraftへの`planner_question`のanswerを運ぶ新しいplannerも立てず、answerは依頼が`open`でなくなるまで待つ（`named_by_open_request`）。
  依頼のplanner（[inboxからの計画の依頼](plan-planners.md#inboxからの計画の依頼)）は、参照する`task:N`に開いているruntimeのdraftのplanner（再検討のものを含む）がある間は立たない（`plan_requests`の`TARGETS`）。
- **時刻の無い`keep_draft`**: 対象から外れ、依頼を待つ（ADR-t1540-1決定6）。
- **記録**: 付ける・消す・使い切るのはどれもtaskのevent（`draft_revisit_set`・`draft_revisit_cleared`・`draft_revisit_due`）で、actorはそのCLIを打ったrole（`draft_revisit_due`はsupervisor）。
  `show`は`revisit`（無ければnull）を出す。
  束のplannerの結末（8）は、まだ使っていない時刻を持つdraftを`keep_draft`にする。

## follow_upの種類と判断の集計<a id="follow_upの種類と判断の集計未実装"></a>

[ADR-t947-3](../../adr/2026-09-28-t947-3-follow-ups-carry-category-codes.md)の決定。
新しいeventは足さず、既存のeventから引く。

- **plannerへの材料**: 2の対象のdraftをplannerに渡すとき、出どころの材料の種類（[Receipt and session exit](receipt-and-session-exit.md#follow_upsの分類コード)）とその定義を、promptのdraftごとの節に載せる（一覧に無い値と`unlabeled`はその旨を添える。`follow_up_category_line`）。
- **判断との突き合わせ**: 8の束のdraftごとの結末と、plannerがそのdraftについて開いた`planner_question`とその答えを、draftの種類で並べる。
  不採用（`canceled`）の理由は[cancelの理由の分類コード](../domain-model.md#cancelの理由の分類コード未実装)（ADR-t947-4）で読む。
- **集計**: [Stats](commands.md)の`follow_up_categories`。

## 推奨が出せればplannerが決める（ADR-t451-1）<a id="推奨が出せればplannerが決める未実装"></a>

[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)の決定5（ADR-0047決定13・16・20をamends）。
基本方針の文言はdagq-planner skillの「Basic policy」に揃え、promptは`prompt::DECIDE_YOURSELF`でその要約を持つ。

- **prompt**（`draft_planner_prompt`の「What to do」）: runtimeのplannerは、推奨が出せるdraftの採否を自分で決める（採用はsubmit、不採用はcancel）。
  理由は、採用ならtaskのcontextに、不採用（重複を含む）・`keep_draft`ならdraftのnoteに書く。
  `keep_draft`で残すとき、決められる時が分かれば再検討の時刻を付け、推奨が`keep_draft`なら聞かずに時刻付きで残してよい（[再検討の時刻](#再検討の時刻)）。
  `planner_question`にするのは、(a) ADR-0047決定41の`scope`・`discard`に当たりqueue・repository・ADR・人の先例で決めきれないもの、(b) 確信度が`low`のもの、(c) [ADR-t808-1](../../adr/2026-09-28-t808-1-runtime-planners-submit-follow-ups-up-to-depth-two.md)の自動で採用しない上限（6）に当たるdraftだけとする。
  repositoryの規則でverify・paths・evidenceが決まらないこと自体は理由にならず、source・ADR・人の先例から決め、それでも(a)か(b)に当たるときだけ聞く（`prompt::RUNTIME_PLANNER_ASK`）。
  問いには推奨と確信度を載せ（[ask](ask.md#aiの推奨と確信度未実装)）、理由の分類は`scope`（成果を捨てるかどうかの問いは`discard`）とする。
- **上限**: CLIの`submit`が上限のdraftを人の`adopt`なしに拒む規則（6）は変えない。
- **集計**: [follow_upの種類と判断の集計](#follow_upの種類と判断の集計未実装)の結末で、plannerが自分で決めたものと`planner_question`を経たものは`follow_up_adopted`の`by`と、draftの`planner_question`の有無で分かる。

## 人のplannerの廃止と非対話の経路<a id="予定-人のplannerの廃止と非対話の経路"></a>

[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)と[ADR-t1394-2](../../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)。
人が開くplannerは無い（ADR-t1394-1）: `dagq plan`は何も開かず、inboxへの計画の依頼の案内を付けて拒む。
人のplannerの行が開いたまま残っていれば、supervisorがcmuxを呼ばずに閉じた行として扱い（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定5。[`plan` / `planners`](plan-planners.md)の「人のplannerの終わり」）、その持ち主のdraftへのanswerは新しいruntimeのplannerが受ける。
非対話の経路（ADR-t1394-2の決定1・2・3・5）では、answerは次のturnの依頼として届く。
読み終えたかは、そのanswerの依頼を取ったturnが終わり、そのturnがClaudeのログイン切れ・利用上限・起動の失敗で失敗していなければ読み終えたとする（`headless_answer_taken`、判断は`src/domain/turn.rs`の`request_read`）。
壁で失敗したturnだけでは読み終えたとせず、wrapperが次に取る`provider retry`のturn（そのanswerを載せる）が壁で失敗せずに終わるまで追う（壁で失敗したturnの後にplannerを終わらせると、answerが読まれずに失われるため）。
また、最後のturnがClaudeのログイン切れ・利用上限・起動の失敗で終わってClaudeを待つplanner（`busy`の`provider_wall`）は終わらせず、控えが解けて続きの`provider retry`のturnが終わるまでanswerを置かない（[runtimeのplannerの経路](plan-planners.md#runtimeのplannerの経路)）。
残りの予定は[予定: inboxからの依頼と非対話のplanner](plan-planners.md#予定-inboxからの依頼と非対話のplanner)にある。

- **draftの行き先**: 4の`draft_planner_exhausted`のattentionと5の`keep_draft`で残したdraftの行き先は、inboxの参照付きの依頼か人の自分のterminalとする（4と5）。
  再検討の時刻を付けたdraftは、これと並んでその時刻にruntimeのplannerの対象に戻る（[再検討の時刻](#再検討の時刻)）。
  人が開いたplannerからのsubmitに代わるのは、依頼のruntimeのplannerのsubmitか、人が`DAGQ_ROLE`の無い自分のterminalで打つ`submit`とする。
  依頼のplannerはruntimeのplannerなので、6の自動で採用しない上限は効き、上限のdraftは`planner_question`の`adopt`を経る。
  人のterminalからのsubmitは人の判断を経たものとして扱う（`by: "person"`、深さを0に戻す）。
- **非対話の経路**（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定3・[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)決定1）: draftのplannerは経路を選ばず、いつも非対話でbackgroundのwrapperだけで動く（`[roles.runtime_planner]`の`route`は受け付けて無視する）。
  起動はbackgroundのwrapper（3）、answerの配送は次のturnの依頼（5）、終わりは終了の依頼とturnの終わり（7）で（`headless::send_to_planner`）、idleはturnの終わりのidle markerで判断する。
  runtimeのplannerにはcmuxのworkspace・画面・打ち込み・`/exit`が無い（workspaceで開いた対話の行は閉じた行として読み、cmuxを呼ばない）。
  束・上限・結末の記録（8）は同じ（[runtimeのplannerの経路](plan-planners.md#runtimeのplannerの経路)）。

## 所属の判断

`judge-follow-up`は採用・priorityとは別に元goalのacceptanceとの関係を記録する。
required/out_of_scopeの所属変更は判断と同じtransactionで行い、set-goalによる食い違う変更は拒む。
登録時の出どころ・状態と深さは移動しても変わらないので、開いたgoalへの移動で人のadoptを迂回できない。
元goalのあるfollow_upのdraftは、今の判断が無い・`undecided`・要再確認ならsubmit（runtimeのplannerも人も）が拒み、人の`ready --bypass-review`もdraft / submittedのものを拒むので、plannerは採用するdraftの判断を先に記録する（人のadoptの上限とは別の検査で、判断を記録してもadoptにはならない）。
`lint`は同じ欠けを`follow_up_membership_unjudged`で出し、plan reviewは判断の行を材料に検査する（[Plan review](plan-review.md)）。
CLI、必須の欄、訂正とacceptanceの版、旧材料の移行は[所属の判断](../follow-up-membership.md)。

- **prompt**（ADR-t1504-1決定1〜3・5・6）: follow_upのdraftの節は`follow_up_proposal_line`で、材料の登録時の元goalとその状態（今のgoalではなくこのgoalのacceptanceに照らすこと）とworkerの所属の提案を載せる。
  「What to do」は3択の前に`follow_up_membership_step`の1段落を置く。
  元goalがnoneでないfollow_upのdraftは、採否・priorityと別に、workerの提案を起点に意味を自分で判断する（そのdraftを実施しなくても元goalのacceptanceを満たせるか。満たせなければrequiredで元goal、満たせればout_of_scopeで別goal）。
  別goalは`dagq search`で既存の適切なgoalを先に探し、無いときだけ作り、無関係な大きなgoalに詰めない。
  adopt・drop・askの前に`dagq judge-follow-up`で判断を記録する。
  前の判断は`dagq show ID`の`membership_judgements`で読み、まだ成り立てば記録し直さず、required・out_of_scopeの間の変更は訂正として行い、undecidedへは戻さない。
  dropするdraftは、記録に新しいgoalが要るなら記録を省いてよい（cancelしたfollow-upはgoalの閉鎖を止めない）。
  決まらなければundecidedを理由つきで記録し、所属の問いを含めて3のaskにし、answerの後にrequiredかout_of_scopeを記録してから従う。
  別goalへの移動は採用でも優先でもない。
  follow_upを外すためにacceptanceを弱めず、人に聞く（`--because scope`）。
  goal_gapとreopenedのdraftにはこの段落も提案の行も出ない（ADR-t1504-2決定10）。

## 予定: recurringのdraft（未実装）

[ADR-0072](../../adr/0072-recurring-drafts-from-dagq-toml.md)の決定2〜4。supervisorが[定期設定](run-environment.md#予定-定期draft未実装)から登録したdraftも、上の2の対象に加える。現在の出どころの列挙と登録処理にはまだ`recurring`は無い。

- **登録と記録**: `draft_origins.origin = recurring`とし、`material`に`key`、`previous_task_id`、`previous_registered_at`（初回はいずれもnull）、登録時の設定のsnapshotを持たせる。今の登録時刻は`draft_origins.created_at`に残す。queueの同じkeyの登録履歴から前回を引き、未完了のdraft/taskが無いことと期限を`BEGIN IMMEDIATE`で再検査して、draftと出どころを同じtransactionで書く。取り戻しも新規の登録も同じ経路を使う。
- **planner**: recurringは各登録を1件の束（task IDの鍵）として扱い、別keyや別周期を時刻の近さで束ねない。
  初期promptにkey・前回の登録・設定のsnapshotとgoalの材料を載せる。
  同時数`runtime_planners`、同じdraftへの二重起動の防止、上限と使い切った後の経路は既存のものを使う。
  上限や`keep_draft`で残ったdraftも非終端なので、次の周期を登録して迂回しない。
- **採否**: 必要性を確認し、採用はedit・lint・submit、不採用はcancelとnote。採用の根拠はcontextに残し、通常のplan reviewを通る。Rustでは公式stableの版と確認日・今の固定版を記録し、更新が無ければcancelのnoteに根拠を残す。goalが未指定なら採用するplannerが適切な開いたgoalを探すか作り、閉じたgoalへの追加や再開は既存のgoalの規則に従う。
- **深さと柵**: recurringのdraftは`follow_up_depth = 0`で、採用の記録は`draft_adopted`とする。recurring自体にはfollow_upの元goal・深さによるadoptの柵を当てない。派生するfollow_upには通常どおり深さ+1と登録時の元goalの記録・所属の判断・自動採用の柵を適用する。
- **人への問い**: 上の「推奨が出せればplannerが決める」とADR-0047決定41を適用する。判断できる採否は自分で決め、材料で決めきれないscope/discard、低い確信度、既存の柵が求める判断だけを理由の分類・推奨・確信度つきの`planner_question`にする。不要な定期の提案をcancelすることだけで人に聞かない。
