---
id: design-supervisor-lifecycle-ask
type: design
title: "`ask` / `answer` / `asks`"
status: current
created: 2026-09-26
updated: 2026-10-02
last_verified: 2026-10-02
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

- askのkindにはほかに`stuck_exit`（schema v17）があり、CLIからは作れない。`follow_up`（schema v22）は退役したfollow-up triageのaskで、もう作られない（残ったものは人が読む）。`planner_question`（schema v28）はruntimeのplannerが`ask --kind planner_question --task ID`で作り、supervisorがanswerをそのplannerに届ける（[Draft planners (supervisor)](draft-planners.md#draft-planners-supervisor)）。findingのために立ったplannerは`--finding ID`で作り、taskもrunも無くてよい（[Finding planners (supervisor)](finding-planners.md)）。findingに紐づけた`blocked`のaskにはruntimeが`propose`と`dismiss`を、`stalled`のaskには`propose`をoptionsに足し、その答えは`answer`がfindingに適用する（同じ文書の5）。`/exit`のtimeoutでsupervisorが作り、sessionの終了でsupervisorが閉じる（[Receipt and session exit](receipt-and-session-exit.md#receipt-and-session-exit)）。
- `dagq ask --kind <approve_landing|answer_prompt|decide|worker_question|planner_question|blocked> --because <scope|discard|recovery_failed> --question <text> [--option <text>]... [--task ID | --run RUN_ID]`は相談を登録してaskを返す（`Ask`の全列と`created`）。workerとjob（Codexの非対話のworkerを含む）の`dagq ask`はクライアントモードでqueue serviceに送られ、serviceが呼び出しのtokenのprincipalで同じ`Dialogue`の判定を通してから開く（`asked_by`はprincipalのrole。task 1236、[Queue service](../queue-service.md#クライアントモード)）。queue serviceより前に起動したCodexのturn（env `DAGQ_ASK_REQUESTS`を持ち、クライアントモードでない）の`dagq ask`だけは、queueを開かずに、queueを要らない検査（optionとpolicy）に通してからrun dirの`ask-requests/`へ要求を書いて`{"requested": true, "request", "path", "message"}`を返し（通らなければすぐにerror）、supervisorがそれを同じ判定と検査に通してaskを開く（ADR-t813-3の決定3。[provider-lifecycle](../provider-lifecycle.md#codexの非対話のworker)）。`--run`はそのrunのtaskも決める。`--task` / `--run`のどちらかは必須で、例外は`blocked`（observerが上げる閾値超え。ADR-0044の決定4）だけ: taskにもrunにも紐づかない閾値（`idle_slots`、`backend_failures`）は`task_id` / `run_id`の無いaskにし、その`ask_opened` / `ask_answered`もtaskの無いrun_eventsになる（schema v16）。`--finding ID`（`blocked`だけ、schema v30、ADR-0044の決定23）はaskを根拠のfindingに紐づけ、observerのblocked askには必須にする。同じ（task、run、kind、finding）でopenなaskがあれば登録せずにそれを`created: false`で返す（`--question`と`--option`は捨てる。taskの無い`blocked`はfindingごとに1件）。新しいaskは`ask_opened`（payloadは`ask_id`、`kind`、`asked_by`）を同じトランザクションで書き、commitの後にinboxへ`cmux notify`を1回送る（observerの`blocked`も同じ。出力に`notified`、失敗なら`notify_error`。[人への通知](cmux-notify.md#人への通知cmux-notify)）。`asked_by`はsessionの`DAGQ_ROLE`（無ければnoteの`by`と同じく`human`）。observer（`DAGQ_ROLE=observer`）は読み取りの`asks`と`status`と、`ask --kind blocked`だけを使え、それ以外の`ask`・`answer` / `ask close`は拒否される。`ask`・`answer`・`ask close`はapplicationの層（`Dialogue`）が呼び出し元のroleで判定してから書く（task 733）: workerは自分のrunか自分のtaskの`worker_question`だけ、plannerはrunに紐づかない`planner_question`だけを開け、answerはuserとinboxだけ、`ask close`はuser・inbox・supervisorだけ（[Authorization](../authorization.md#対話と記録のコマンドapplication)）。answerはaskの行の`answer_authority`（`user`・`delegated`・`runtime`）と`answer_approval`、`ask_answered`のpayloadの`authority`と`approval`に、権限の出どころと承認に当たるかを残す。
- **人が要る理由**（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定41、task 361、schema v29）: askは`reason_category`（`authentication` / `cost` / `scope` / `discard` / `recovery_failed`）を必ず持つ。CLIの`ask`は`--because <reason>`が必須で、無いか一覧に無い値、`authentication` / `cost`（下のqueue_holdだけのもの）は拒否する。当てはまらない相談はaskにせず、noteかreceiptに書く。runtimeが作るaskはkindごとに決まった理由を書く: `approve_landing` / `approve_plan`は`scope`、`approve_goal`（[goal review](goal-review.md)）はverdictが`discard`ならそれ、ほかは`scope`、`decide`（triageとresumeの使い切り）・`stuck_exit`・`answer_prompt`・`stalled`は`recovery_failed`。`worker_question` / `planner_question` / `blocked`は作る者が`--because`で付ける。v29のmigrationは既存のaskをkindから埋める（`approve_landing` / `approve_plan` / `planner_question` / `worker_question` / `blocked` / `follow_up`は`scope`、ほかは`recovery_failed`）。`ask_opened` / `ask_answered`のpayload、`status`の`asks`とask由来のattention、`events` / `watch`のcompactな行に`reason_category`が載る。
- **認証のaskは1件**（ADR-0047の決定42）: kind `queue_hold`のaskはtaskにもrunにも紐づかず、理由（`authentication` / `cost`）と`subject`ごとにopenなものが1件だけで、止めているrunとheadless jobを`affected`（run IDと、`review job of run <id>`のようなjobの項目の配列）に持つ。supervisorはworkerの画面にログインの切れ（最後の行のどれかが、枠と`⎿`を除いて`API Error: 401` / `Invalid API key` / `OAuth token has expired` / `OAuth token revoked`で始まり`/login`を含む。文言を含むだけの作業の出力は数えない。`infrastructure::claude::auth_required`）か利用上限（`usage limit reached`、`5-hour limit reached ∙ resets 3pm`など。`usage_limited`、task 438）を見つけると、dialogの確認（`watch_prompt`）でも受領の無いidleの判定（`StallWatch`、促しとstalledのaskの前）でも待ちの確認でも、そのrunをこのask（ログインは`authentication`、利用上限は`cost`で`subject: usage_limit`）に足す（無ければ開く。optionsは`done` / `cancel_affected`、通知は開いたときの1回だけ）。足したrunには`auth_required` / `usage_limited`（`workspace_id`、`excerpt`、`screen_hash`、`ask_id`）を、2件目以降には`ask_updated`（`ask_id`、`reason_category`、`affected`、`joined`）をそのrunに（runの無いjobならqueueに）書き、questionの末尾の`Affected:`（`run <id>`とjobの項目を並べる）を書き直す。失敗したheadless job（review・復旧job・plan review・goal review・observer）の出力にその文言があれば、jobの失敗を通常の`*_failed`のattentionにせずaskに足す（詳しくは[認証と利用上限のaskの待ちとanswer](queue-hold.md)の「検知」）。askにいるrunは促しもstalledのaskも受けない。ログインや上限の後も画面にはエラーが残るので、直近の同じ種類のeventと同じ`screen_hash`の画面で、そのaskがもうopenでなければ上げ直さない（`done`の後はruntimeが決まった文面で続きを頼み、送れなかったrunや別のsupervisorのrunはstallの促しが頼む）。openな間のclaimとheadless jobの控えと、`done` / `cancel_affected`のruntimeによる適用（`runtime_delivers: true`）は[認証と利用上限のaskの待ちとanswer](queue-hold.md)（task 437）、ディスクの`cost`のaskは[空き容量を確かめる](disk-space.md)（task 377）。
- `dagq answer ASK_ID --text <text>`はopenなaskに`answer`と`answered_at`を書き、`ask_opened`と同じtask / runに`ask_answered`（`ask_id`、`kind`）を書く。回答済みかcloseされたaskにはerror。task 325（schema v38）から、回答した者`answered_by`（sessionの`DAGQ_ROLE`、無ければ`person`）と、回答が`options`のどれかと（前後の空白を除いて）一致したときのその番号`option_index`（一致しなければnull）も行に書き、`ask_answered`のpayloadに`answered_by` / `option_index`と選んだ選択肢の文字列`option`を足す。runtimeが自分で閉じる回答（`runtime_closed: true`の`ask_answered`と、supervisorの取り下げ）は`answered_by: runtime`。出力の`Ask`には`answered_by` / `option_index`が常に載り、未回答とv38より前の回答ではnull。集計は[`stats`](stats.md)の`asks`。
- `dagq ask close ASK_ID`は回答済みのaskに`closed_at`を書き、`ask_closed`（`ask_id`、`kind`）を記録する。inboxが回答を読んで従った印（supervisorが答えを適用して閉じるときも同じ`close_ask`）で、`stats`の`asks.times`は回答の適用として読む（task 468）。runtimeが回答済みのaskを適用せずに閉じるとき（回答が当たらなくなった`approve_plan` / `approve_goal`、proposalの取り下げで閉じる`approve_plan`）も同じ`ask_closed`をcloseと同じトランザクションで書く（task 568）。attentionではない。未回答のaskはcloseできず（error）、取り下げは`answer`で取り下げた旨を書いてからcloseする。run_eventsでaskを終えるのは`ask_answered`だけで（`worker_question`の送信の`ask_delivered` / `ask_delivery_failed`は後から足したkindで、`stats`の対には使わない）、`stats`はこの2つを`ask_id`で対にして未回答のaskを数えるため、closeだけで閉じたaskが`stats`に残り続けないようにする。回答した時点で同じ（task、run、kind）の新しいaskを登録できる。
- `dagq asks [--open] [--role <role>] [--all]`はaskを古い順に`{asks}`で返す。既定はcloseされていないもの、`--all`はcloseされたものも、`--open`は未回答のものだけ、`--role`はそのroleが今動かすもの（`Ask::waits_for`: 未回答も回答済みでcloseされていないものもinbox、plannerは無し）。

## worker_questionの分類コード<a id="worker_questionの分類コード未実装"></a>

[ADR-t947-2](../../adr/2026-09-28-t947-2-worker-questions-carry-topic-codes.md)の決定（task 953で実装）。一覧と定義は`domain::worker_question::WORKER_QUESTION_TOPICS`（重い順）が持ち、workerのprompt（`application::prompt`の`worker_question_topics_line`）とCLIの`ask --topic`のhelpがそのまま載せる。一覧はtask 950の分析（[worker-question-topics](../../plans/worker-question-topics.md#ラベル)）を元に、runのreview（[Review](review.md#差し戻しの分類コード)）と同じ種類の問題の名前を揃えた。

- **CLI**: `dagq ask --kind worker_question --because <scope|discard> --topic <code> [--topic <code>]...`。`--topic`は1つ以上必須で、先頭が主、残りが副。前後の空白を除き、空の値は捨て、同じコードは最初の1つだけ残す。一覧に無い値は拒まずにそのまま記録する。`--topic`の無い`worker_question`（`a worker_question needs --topic`とコードの一覧）と、`worker_question`以外のkindに付けた`--topic`（ADR-t947-2決定6）は`NewAsk::validate`が拒む。runtimeは`worker_question`を作らない。
- **記録**: askの行の`topics`（JSONの配列、先頭が主。schema v54、`0054_ask_topics.sql`。他のkindと、それより前のaskはnull）に書き、`ask_opened`のpayload・`ask`と`asks`の出力（`Ask`の`topics`）・`status`の`asks`の各項目に`topics`を載せる（無いaskは欄ごと省く）。`reason_category`（上の「人が要る理由」）とは別の欄で、一方から他方を推さず、食い違っても拒まない。`topics`の無い過去のaskは書き換えず、集計では`unlabeled`として数える（`domain::UNLABELED_TOPIC`）。
- **集計**: [`stats`](stats.md#worker_questionの分類コードごとの集計)の`worker_question_topics`・`asks.times.by_topic`・`waiting.waited_by_topic`と、[`kpi`](kpi.md#worker_questionの分類コードごとの系列)の`ask.worker_question_wait`。
- **主の選び方**: 主はworkerが止まったきっかけ（最初に満たせなくなったもの）、副はそれを解くのに一緒に決める必要があるもの。きっかけが2つ同時で決められないときだけ、重い方を主にする。重い順: `discard_work` > `adr_conflict` > `acceptance_conflict` > `acceptance_infeasible` > `out_of_scope_change` > `task_overlap` > `precondition_missing` > `host_environment` > `design_choice` > `other`。

| コード | 定義 | task 950の例 | `reason_category`の目安 |
|---|---|---|---|
| `adr_conflict` | taskの受け入れ条件かdescription、またはそれを満たす唯一のやり方が、acceptedのADR・design・人の決定（goalのconstraints、AGENTS.mdのユーザー決定）と食い違い、両方は満たせない（[Review](review.md#差し戻しの分類コード)と同じ定義） | task 392: ADR-0040決定3の「dagq.tomlを置かない」を変える | `scope` |
| `acceptance_conflict` | 同じtaskの受け入れ条件どうし、または条件とdescriptionが両立しない | task 128: newtype化と「tests/cli.rsを変更なしで通す」 | `scope` |
| `acceptance_infeasible` | 条件が、調べた事実（ツールの挙動・再現しない現象・権限）のためにそのままでは満たせない | task 770: hangを再現できず「原因の特定」を満たせない | `scope` |
| `out_of_scope_change` | 条件を満たすのに、taskのpaths・description・verifyに無い変更が要る | task 252: tests/plugin.rsがpathsの外 | `scope` |
| `task_overlap` | 並行する他のtaskや、直前に着地した変更と重なる・衝突する | task 209: 同じ番号とREADMEを書き換えたtaskが着地 | `scope` |
| `precondition_missing` | 着手の前提（測る対象のデータ、先行のtaskの着地、必要な件数）がまだ揃っていない | task 460: 導入後の着地が10 runに満たない | `scope` |
| `host_environment` | hostのツール・設定・版が作業か着地を妨げ、workerはhostに手を入れられない | task 486: globalのmiseのrustの設定 | `scope` |
| `design_choice` | 条件・ADR・範囲に触れない実装の選び方。AGENTS.mdではaskにせずworkerが決めるもので、付いたaskはpromptの直しどころを示す（ADR-t947-2決定3） | この期間は無し | （人が要る理由が無い） |
| `discard_work` | できた成果を捨てるか、やり直すか | この期間は無し | `discard` |
| `other` | どれにも当たらない。問いの文で説明する | この期間は無し | — |

## 今後の姿: AIの推奨と確信度をaskに持たせる（未実装、ADR-t451-1）<a id="aiの推奨と確信度未実装"></a>

[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)の決定1。**未実装**で、今の`ask`はこの欄を持たない。実装は後続のtaskが行う。

- **欄**: AIが作るask（`planner_question`・`blocked`・`approve_landing`・`approve_plan`）に`recommendation`（推奨のoptionの文。無ければ`null`）と`confidence`（`high` / `low` / `null`）を足し、`ask_opened`のpayloadと`asks`の出力に載せる。inboxは推奨と確信度を人に見せる。`dagq ask`は`--recommend <option>`と`--confidence <high|low>`を受ける（`worker_question`には求めない）。
- **AIが決めたものの記録**: AIが推奨を適用してaskを作らなかった判断は、kindごとの記録（[Review](review.md#aiが決めるconcern未実装)の`concern_decided`、[Plan review](plan-review.md#aiが決めるconcern未実装)の`plan_concern_decided`、[Observer](observer.md#人が要る見立てだけをblockedにする未実装)のfinding、[Draft planners](draft-planners.md#推奨が出せればplannerが決める未実装)のnoteと`follow_up_adopted`）に残す。
- **集計**: [Stats](stats.md)にaskのkindごとの「answerが`recommendation`と一致した割合」と、AIが決めてaskにしなかった件数を並べる（ADR-t451-1のContextの数え方を、question・optionsの文でなく欄から再導出する）。
