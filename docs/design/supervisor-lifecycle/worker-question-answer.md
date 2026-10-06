---
id: design-supervisor-lifecycle-worker-question-answer
type: design
title: "workerの質問への回答の送信"
status: current
created: 2026-09-26
updated: 2026-10-04
last_verified: 2026-10-04
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0022
---

# workerの質問への回答の送信

[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定2。runtimeがworkerのsessionに渡すのは終了の依頼・resumeの解消依頼・reviseの依頼と、このworkerへの回答だけで、どれも次のturnの依頼としてrun dirの`turns/`に書く（[非対話のworker](headless-worker.md#run-dirのturns)、[Session send](session-send.md)。ADR-0016の決定3の例外）。task 1437より前の対話のworkerには、これらをterminalに打ち込み、入力欄に残った文面や`/exit`をEnterだけで送り直していた。

- **条件**: `SessionWatch::poll`のたびに、wrapperが生きていて終了をまだ依頼していないrunについて、そのrunの`worker_question`のうち回答済みでcloseされていないask（`SqliteQueue::undelivered_answers`）を古い順に見る。idle markerが最後に始まったturn（`turn_started`の`turn`の最大）の終わりを示していれば、turnの間にある（`headless::between_turns`）とみなし、markerの時刻とaskの`created_at`を比べずに回答を送る（次のturnの依頼になる）。Codexのworkerのaskは今はqueue serviceがturnの中で開く（task 1236、[Queue service](../queue-service.md#クライアントモード)）が、queue serviceより前に起動したturnのaskはrun dirへの要求で、supervisorが問いたturnの終わった後のpassで開くので（ADR-t813-3の決定3、[provider-lifecycle](../provider-lifecycle.md#codexの非対話のworker)）、markerがaskより古い秒になりうるため。turnが走っている間は`idle.json`のmtime（unix秒）がaskの`created_at`以上のaskだけを送り、markerが無ければ送らない（次のpollで見直す）。画面からidleを推定しない（task 1437で対話のworkerとともに撤去した。[workerのsession](receipt-and-session-exit.md#workerのsession)）。
- **送信**: turn が終わり次の依頼が無いときに `answer to ask <id>: <answer>` を次の turn の依頼として書く。成功すれば ask_delivered と closed_at を同じ transaction で記録する。書き込みの失敗では ask_delivery_failed だけを記録し、ask は閉じない。打鍵・Enter の確認・StartCheck は廃止した。
- **失敗**: 送信の失敗はrunを手放さず、`ask_delivery_failed`（`ask_id`、`workspace_id`、`error`）を記録してlogに書き、askはcloseしない。送信は1回だけで、`ask_delivery_failed`のあるaskは（引き継いだsupervisorも）再送しない。送信とcloseの間でsupervisorが死んだときだけ、引き継いだsupervisorがもう一度送りうる。
- **attention**: `answer`は`worker_question`の`ask_answered`のpayloadに`runtime_delivers`（回答した時点でrunが`running`か、leaseを持ち`domain::session_takes_answers`が真: revise中の`awaiting_integration`（[Review](review.md#review-supervisor)の8）かresume中の`needs_session`（[`needs_session`](needs-session.md#needs_session)の4、ADR-0071の決定17））を足す。`true`ならattentionイベントにしない（inboxの`watch`を起こさない）、`false`なら`send the answer of ask <id> to the worker and close it`のattentionイベント。`status`は回答済みの`worker_question`を、runが`running`でstaleでないleaseがあり送信に失敗していなければ`next: delivering the answer of ask <id> (runtime)`（何もしなくてよい）、`ask_delivery_failed`があれば`kind: ask_delivery_failed`で、runが`running`でないかleaseが無いかstaleなら（誰も送らない）`kind: ask_answered`で、どちらも`next: send the answer of ask <id> to the worker and close it`として出す。`ask_delivery_failed`はattentionイベント（inbox宛て）。`ask_delivered`はattentionではない。
- **過去の記録**: worker のダイアログ待ちは廃止した。過去の prompt_waiting / answer_prompt は読めるが、worker_question の配送は turn の終了で判断する。
- **答えずに閉じたask**（task 1372）: 回答が配送されないまま閉じた`worker_question`（`ask_delivered`の無い`ask_closed`。`ask close`は回答済みのaskしか閉じないので、配送の前に人かinboxが閉じたもの）は、`deliver_answers`が送らない。sessionがそのcloseの後にturnを終えていなければ、supervisorは次のreceiptの無いidleで、促しの代わりに閉じたことと次にすべきことを伝える文を1回だけ送る（[receiptの無いidleの検知](idle-without-receipt.md)の「答えずに閉じた`worker_question`」）。待ちの最中にsessionが終わっても`worker_question`は閉じない（[非対話のworker](headless-worker.md#待ちの最中に失ったsessionの開き直し)）。
- **inbox**: `status`の`asks`に`worker_question`が出る。inboxが人に見せ、人の答えを`answer`で書く（それまで退役した常駐sessionがworktreeの中の判断に自分で答えていたのはやめた。task 100）。
