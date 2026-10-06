---
id: design-supervisor-lifecycle-cmux-notify
type: design
title: "人への通知（`cmux notify`）"
status: current
created: 2026-09-26
updated: 2026-10-04
last_verified: 2026-10-04
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0022
  - adr-t906-1
---

# 人への通知（`cmux notify`）

> **予定（goal 92）**: [ADR-t1433-1](../../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)決定2で、supervisorとqueue serviceは`cmux notify`を呼ばなくなり、`ask_opened`の通知はinboxのsessionの中で動く`watch --role inbox`が出す（cmuxのterminalの子なのでsocket passwordが要らない）。この文書のsupervisorとqueue serviceからの通知は、後続のtaskが実装するまでの今の姿である。

`WorkspaceBackend::notify(title, body, workspace)`は人への通知の操作で、cmux adapterは`cmux notify --title <title> --body <body> [--workspace <id>]`を実行する（`workspace`が`None`なら`--workspace`を付けない。失敗はcmuxの非0終了をエラーにして返す）。terminalへの打ち込みではないのでinboxやworkerのUI状態に干渉しない。

`cmux notify`を送るのはaskの登録だけで、新しいaskを登録したとき（`ask_opened`を書いたとき）に1回送る（[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定5、task 87）。askを作るのはworker、observer（`blocked`）、手でreviewする人のプロセスと、supervisor（stalled、reviewの`approve_landing`、triageと試行を使い切ったresumeの`decide`）で、どれも`runtime::ask`と同じ経路（supervisorは開いているqueueで`runtime::ask_in`）で送る。宛先は`session_workspaces`に記録されたinboxのworkspace UUID（`up`が記録する）で、記録が無ければ`--workspace`なしで送る。titleは`[<repo>] ask #<id> <kind>`（`<repo>`はqueueが束縛されたrepositoryのmain checkoutのディレクトリ名。束縛の無い`--db` queueは作業ディレクトリの名前）、bodyはquestionの先頭200文字（超えれば`…`）と、改行の後の`task <id>`（runのaskなら` run <run-id>`を続ける。observerの`blocked`でtaskの無いaskはこの行を省きquestionだけ）。同じ（task、run、kind）のopenなaskを返しただけ（`created: false`）なら送らない。例外として、`correct_goal`のask（ADR-t1504-2決定9。[Goal review](goal-review.md)の9）は`judge-follow-up`の判断の記録と同じトランザクションの中でstoreが開くので`cmux notify`を送らず、inboxは`watch --role inbox`の`ask_opened`だけで受ける（通知をinboxの`watch`に移す[ADR-t1433-1](../../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)決定2の向きに合わせる）。`answer`と`ask close`も送らない。cmuxは`ask --cmux <path>`（既定は`cmux`をPATHで解決）。通知の失敗でaskは失敗しない: askは登録済みのまま、出力の`notified`をfalseにして`notify_error`に理由を書く（成功なら`notified: true`）。`backend_call_failed`には記録しない（runにもsupervisorにも属さない呼び出しで、inboxはaskを`watch --role inbox`で受けるので通知は補助）。

supervisorはrunの遷移を通知しない。ADR-0016の契約(3)はattentionのたびに当時の常駐sessionのworkspaceへ送るとしていたが、ADR-0022の決定5でrunの遷移（`awaiting_integration`・`needs_session`・`failed`・`exit_request_timed_out`など）は通知しないことになった。過去の exit_request_timed_out は stuck_exit の ask で通知していたが、対話 worker の終了待ちとその ask は廃止した。`integrate`の`push_failed`（task 70）も通知しない。inboxはattentionを`watch`で受ける。

`cmux notify`は人への知らせで、inboxのClaudeのsessionを起こさない。inboxが起きるのは`watch --role inbox`の終了だけなので、watchが張られていないあいだに開いたaskは人に届かない（2026-09-28に約3時間）。これを[ADR-t906-1](../../adr/2026-09-28-t906-1-guarantee-the-inbox-watch.md)の3層で防ぐ: watcherの生存の記録と`status` / `doctor`の`inbox_watcher`（[`events` / `watch`](events-watch.md#inboxのwatcherの記録adr-t906-1)）、pluginのSessionStart / Stop hook（[plugin integration](../plugin-integration.md)）、watcherが居ないまま閾値を超えてaskが開いているときにsupervisorがidleのinboxの画面へ1行の知らせを打ち込むこと（[inboxへの知らせ](notification-route.md#supervisorによるinboxへの知らせadr-t906-1)）。その知らせを2回打っても（2回目を打てないまま間隔の2倍たったときも）watcherが戻らなければ、supervisorはinboxのworkspaceへ`cmux notify`（title `[<repo>] inbox has no watch`、bodyは閾値を超えて待ったaskの件数）を1回送る。askの登録以外で`cmux notify`を送るのはこれだけ。
