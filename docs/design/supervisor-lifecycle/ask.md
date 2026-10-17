---
id: design-supervisor-lifecycle-ask
type: design
title: "`ask` / `answer` / `asks`"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t451-1
  - design-supervisor-lifecycle
  - adr-0022
  - adr-0047
  - design-persistence
  - adr-t947-2
---

# `ask` / `answer` / `asks`

[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定1。人の判断を要する相談を`asks`表の行にする（[persistence](../persistence.md#asks)）。

- runtime は `stuck_exit` と `answer_prompt` を新しく開かない（対話の worker を廃止した [ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）。
  過去の ask は読める。
  権限の判定は[Authorization](../authorization.md#対話と記録のコマンドapplication)のとおり（worker は worker_question だけ）。
  `follow_up`はfollow-up triageのaskで、新しく作られない（残ったものは人が読む）。
  `planner_question`はruntimeのplannerがtaskについて作り、supervisorがanswerをそのplannerに届ける（[Draft planners (supervisor)](draft-planners.md#draft-planners-supervisor)）。
  `correct_goal`はachievedで閉じたgoalのfollow_upにrequiredが記録されたときにruntimeがそのfollow_upのtaskについて作り、supervisorがanswerを適用する（[Goal review](goal-review.md)の9、ADR-t1504-2決定9）。
  CLIの`ask`の`--kind`には無い（ほかのaskと同じく、inboxは`watch`の`ask_opened`で受ける。[人への通知](ask-push.md)）。
  findingのために立ったplannerは`--finding ID`で作り、taskもrunも無くてよい（[Finding planners (supervisor)](finding-planners.md)）。
  計画の依頼のために立ったplannerは`--request ID`で作り（`planner_question`だけ。開けるのはその依頼のplanner自身だけで、他のplannerは拒まれる）、taskもrunも無くてよい（[inboxからの計画の依頼](plan-planners.md#inboxからの計画の依頼)の7）。
  findingに紐づけた`blocked`のaskにはruntimeが`propose`と`dismiss`を、`stalled`のaskには`propose`をoptionsに足し、その答えは`answer`がfindingに適用する（同じ文書の5）。
- `dagq ask`は相談を登録してaskを返す（入口は`Dialogue::ask`）。
  workerとjob（Codexの非対話のworkerを含む）の`dagq ask`はクライアントモードでqueue serviceに送られ、serviceが呼び出しのtokenのprincipalで同じ`Dialogue`の判定を通してから開く（`asked_by`はprincipalのrole。[Queue service](../queue-service.md#クライアントモード)）。
  runに紐づけたaskはそのrunのtaskにも紐づく。
  taskかrunのどちらかは必須で、例外は`blocked`（observerが上げる閾値超え。ADR-0044の決定4）だけ: taskにもrunにも紐づかない閾値はtask・runの無いaskにし、その`ask_opened` / `ask_answered`もtaskの無いrun_eventsになる。
  observerのblocked askは根拠のfindingに紐づけるのが必須（ADR-0044の決定23）。
  同じ（task、run、kind、finding）でopenなaskがあれば登録せずにそれを`created: false`で返す（問いとoptionsは捨てる。taskの無い`blocked`はfindingごとに1件）。
  新しいaskは`ask_opened`を同じトランザクションで書き、知らせはinboxの`watch`が出す（[人への通知](ask-push.md)）。
  `asked_by`はsessionの`DAGQ_ROLE`（無ければnoteの`by`と同じく`human`）。
  observer（`DAGQ_ROLE=observer`）は読み取りの`asks`と`status`と、`ask --kind blocked`だけを使え、それ以外の`ask`・`answer` / `ask close`は拒否される。
  `ask`・`answer`・`ask close`はapplicationの層（`Dialogue`）が呼び出し元のroleで判定してから書く: workerは自分のrunか自分のtaskの`worker_question`だけ、plannerはrunに紐づかない`planner_question`だけを開け（`--request`付きはその依頼の閉じていないruntimeのplanner自身だけ）、answerはuserとinboxだけ、`ask close`はuser・inbox・supervisorだけ（[Authorization](../authorization.md#対話と記録のコマンドapplication)）。
  answerはaskの行と`ask_answered`のpayloadに、権限の出どころと承認に当たるかを残す（`Ask`の`answer_authority`・`answer_approval`）。
- **人が要る理由**（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定41）: askは人が要る理由`reason_category`を必ず持つ。
  CLIの`ask`は`--because`が必須で、無いか一覧に無い値、`authentication` / `cost`（下のqueue_holdだけのもの）は拒否する。
  当てはまらない相談はaskにせず、noteかreceiptに書く。
  runtime の ask の理由は主に kind で決まり（`AskReason`）、worker_question / planner_question / blocked は作る者が --because を付ける。
  reason_category は ask と event と表示に残る。
- **認証のaskは1件**: queue_hold は task と run の無い ask で、authentication / cost と subject ごとに open なものは1件。
  失敗した worker の turn と headless job の出力から provider の壁を検知し、affected に run と job を足す。
  人への通知は、新しく開いたaskの`ask_opened`にだけinboxのwatchが出す。
  claim と job の保留、done の続行の依頼、cancel_affected の適用は [queue-hold](queue-hold.md) の共通処理を使う。
- `dagq answer`はopenなaskに答えを書き、`ask_opened`と同じtask / runに`ask_answered`を書く。
  回答済みかcloseされたaskにはerror。
  復旧jobの`decide`のask（`asked_by: supervisor`、runが`failed` / `interrupted`）に、optionsのどれとも一致しないruntimeの操作の名前だけを答えると、何も書かずにerrorで断り、askは開いたまま残る（名前を含むoptionの原文と、`retry_inherit`なら手での引き継ぎをmessageに示す。[Triage](triage.md)の9）。
  回答した者・選んだ選択肢と、`approve_plan`と`approve_landing`の`<option>: <理由>`の形の答えが名指す選択肢の残し方は、`Ask`の欄のdoc commentと`domain::reasoned_option`が持つ。
  その2つのkindでsupervisorは`send_back: <理由>`を`send_back`として適用し人の理由を差し戻し先に渡す（`approve_plan`は`PlanAnswer`、`approve_landing`は`LandingAnswer`）ので、`approve_landing`の`ask_answered`の`runtime_delivers`は、runが`awaiting_integration`で答えが`land` / `send_back` / `cancel`か`send_back: <理由>`、またはreviewが失敗してverdictの無いaskへの`retry_review`のときtrueになる（`land: x`・`cancel: x`と、verdictから開いたaskへの`retry_review`などはfalseのままinboxが読む。受け付けの判断は適用と`status`の表示と同じ1つ。[Review](review.md#review-supervisor)の6、[ADR-t2521-1](../../adr/2026-10-17-t2521-1-retry-review-answer-reviews-a-failed-review-again-on-the-same-run.md)）。
  runtimeが自分で閉じる回答（`runtime_closed: true`の`ask_answered`と、supervisorの取り下げ）は`answered_by: runtime`。
  集計は[`stats`](commands.md)の`asks`。
- `dagq ask close`は回答済みのaskに`closed_at`を書き、`ask_closed`を記録する。
  inboxが回答を読んで従った印（supervisorが答えを適用して閉じるときも同じ`close_ask`）で、`stats`の`asks.times`は回答の適用として読む。
  runtimeが回答済みのaskを適用せずに閉じるとき（回答が当たらなくなった`approve_plan` / `approve_goal`、proposalの取り下げで閉じる`approve_plan`、適用の時にgoalがもうachievedで閉じていない`correct_goal`、`correct_goal`の`reopen`が閉じる同じgoalのほかの`correct_goal`と残った`approve_goal`、runが`failed`になったかtaskが`completed`・`canceled`になったrunの`approve_landing`（[Review](review.md#review-supervisor)の6））も同じ`ask_closed`をcloseと同じトランザクションで書く。
  `reopen`が閉じるそれらのうち未回答のものは、runtimeがgoalを開き直した旨を答えて（`ask_answered`の`runtime_closed: true`）閉じる（[Goal review](goal-review.md)の9）。
  attentionではない。
  未回答のaskはcloseできず（error）、取り下げは`answer`で取り下げた旨を書いてからcloseする。
  run_eventsでaskを終えるのは`ask_answered`だけで（`worker_question`の送信の`ask_delivered` / `ask_delivery_failed`は`stats`の対には使わない）、`stats`はこの2つを`ask_id`で対にして未回答のaskを数えるため、closeだけで閉じたaskが`stats`に残り続けないようにする。
  回答した時点で同じ（task、run、kind）の新しいaskを登録できる。
- `dagq asks`はaskを古い順に返し、roleで絞るときに誰が動かすかは`Ask::waits_for`が決める（plannerは無し）。

## worker_questionの分類コード<a id="worker_questionの分類コード未実装"></a>

[ADR-t947-2](../../adr/2026-09-28-t947-2-worker-questions-carry-topic-codes.md)の決定。
一覧と定義は`domain::worker_question::WORKER_QUESTION_TOPICS`（重い順）が持ち、workerのprompt（`application::prompt`の`worker_question_topics_line`）とCLIの`ask --topic`のhelpがそのまま載せる。
runのreview（[Review](review.md#差し戻しの分類コード)）と同じ種類の問題には同じ名前を使う。

- **CLI**: `worker_question`は`--topic`で主（先頭）と副のコードを付け、無いときと`worker_question`以外のkindに付けたとき（ADR-t947-2決定6）は`NewAsk::validate`が拒む。
  一覧に無い値は拒まずにそのまま記録する。
  runtimeは`worker_question`を作らない。
- **記録**: askの行の`topics`に書き、`ask_opened`のpayloadとaskの出力に載せる（無いaskは欄ごと省く）。
  `reason_category`（上の「人が要る理由」）とは別の欄で、一方から他方を推さず、食い違っても拒まない。
  `topics`の無い過去のaskは書き換えず、集計では`domain::UNLABELED_TOPIC`として数える。
- **集計**: [`stats`](commands.md)と[`kpi`](commands.md)が分類ごとに出す。
- **主の選び方**: 主はworkerが止まったきっかけ（最初に満たせなくなったもの）、副はそれを解くのに一緒に決める必要があるもの。
  きっかけが2つ同時で決められないときだけ、重い方を主にする（重い順は`WORKER_QUESTION_TOPICS`の並び）。

| コード | 定義 | `reason_category`の目安 |
|---|---|---|
| `adr_conflict` | taskの受け入れ条件かdescription、またはそれを満たす唯一のやり方が、acceptedのADR・design・人の決定（goalのconstraints、AGENTS.mdのユーザー決定）と食い違い、両方は満たせない（[Review](review.md#差し戻しの分類コード)と同じ定義） | `scope` |
| `acceptance_conflict` | 同じtaskの受け入れ条件どうし、または条件とdescriptionが両立しない | `scope` |
| `acceptance_infeasible` | 条件が、調べた事実（ツールの挙動・再現しない現象・権限）のためにそのままでは満たせない | `scope` |
| `out_of_scope_change` | 条件を満たすのに、taskのpaths・description・verifyに無い変更が要る | `scope` |
| `task_overlap` | 並行する他のtaskや、直前に着地した変更と重なる・衝突する | `scope` |
| `precondition_missing` | 着手の前提（測る対象のデータ、先行のtaskの着地、必要な件数）がまだ揃っていない | `scope` |
| `host_environment` | hostのツール・設定・版が作業か着地を妨げ、workerはhostに手を入れられない | `scope` |
| `design_choice` | 条件・ADR・範囲に触れない実装の選び方。dagqのrepositoryでは`docs/development/local-checks.md`の「askにしないもの」のとおりaskにせずworkerが決めるもので、付いたaskはpromptの直しどころを示す（ADR-t947-2決定3） | （人が要る理由が無い） |
| `discard_work` | できた成果を捨てるか、やり直すか | `discard` |
| `other` | どれにも当たらない。問いの文で説明する | — |

## AIの推奨と確信度<a id="aiの推奨と確信度未実装"></a>

[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)の決定1。
AIが作るask（`planner_question`・`blocked`・`approve_landing`・`approve_plan`）に推奨と確信度を載せる。
それぞれのaskに推奨を載せるのはreview・plan review・observer・plannerで、この節の欄と集計を使う。

- **欄**: askの行の`recommendation`と`confidence`（`domain::AskConfidence`）。
- **CLI**: `--recommend`がaskのoptions（runtimeが足すものを含む。`NewAsk::offered_options`）のどれでもなければ`NewAsk::validate`が理由（推奨とoptionsの一覧）つきで拒む。
  queue serviceのクライアントモードの`ask`も同じ欄を運ぶ。
  同じ（task、run、kind、finding）のopenなaskを返すとき（`created: false`）は、既存のaskの推奨を書き換えない。
  `blocked`のaskは推奨が無い（空白だけを含む）と、`Dialogue::ask`（CLIとqueue serviceの`ask`が通る）が`NewAsk::check_asker`で理由つきで拒み、askを書かない（拒否は権限の判定の後で、`authorization_denied`には記録しない）。
  推奨がleave it / waitに当たるかはCLIでは判定しない（[Observer](observer.md#人が要る見立てだけをblockedにする未実装)のpromptの規則。ADR-t451-1決定2）。
- **表示**: 推奨と確信度はaskの出力・`ask_opened`・`status`と`watch`のaskの行に載る（無ければnull）。
  `show ID`の`asks`は`TaskDetail::asks`で、`--full`なしでは新しい方から`view::DEFAULT_ASKS`件に絞って長い文を切る。
  openか閉じたかは`answered_at`・`closed_at`で分かる。
  queue serviceのクライアントモードの`show`も同じview。
  inboxは人にaskを見せるとき、推奨と確信度があれば尋ねたAIのものとして添え、自分の推奨は足さない（`dagq-inbox` skill）。
- **AIが決めたものの記録**: AIが推奨を適用してaskを作らなかった判断は、kindごとの記録に残し、数える記録は`stats::recommendations::DECIDED_WITHOUT_ASK`が持つ（[Draft planners](draft-planners.md)・[Plan review](plan-review.md#aiが決めるconcern未実装)・[Review](review.md#aiが決めるconcern未実装)・[Observer](observer.md#人が要る見立てだけをblockedにする未実装)）。
  適用した`send_back`をsessionが直さずに`approve_landing`のaskになったものは`concern_send_back_escalated`を記録して数えない。
- **集計**: [Stats](commands.md)の`recommendations`。
  askのkindごとの「推奨を持つaskの答えのうち、answerが`recommendation`と一致した割合」と、AIが決めてaskにしなかった件数を並べる（ADR-t451-1のContextの数え方を、question・optionsの文でなく欄から再導出する）。
