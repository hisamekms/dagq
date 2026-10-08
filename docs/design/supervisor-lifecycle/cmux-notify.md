---
id: design-supervisor-lifecycle-cmux-notify
type: design
title: "人への通知（`cmux notify`）"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0022
  - adr-t906-1
  - adr-t1433-1
  - adr-t1433-5
---

# 人への通知（`cmux notify`）

新しいaskを人に知らせるのは、inboxのsessionの中で動く`watch --role inbox`だけである（[ADR-t1433-1](../../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)決定2）。
supervisor・queue service・observerは`cmux notify`を呼ばず、askを開く経路（`ask`のCLI、`application::ask::ask` / `hold`、storeが判断と同じトランザクションで開くask）はどれも何も知らせない。
`ask`の出力に`notified`・`notify_error`は無く、`ask --cmux`は受け付けて無視する。

- 判断は`application::watch`にある。
  watchが返す`ask_opened`ごとに1回、`AskNotifier::notify`を呼ぶ（`notify_opened`）。
  inbox以外のroleのwatchとroleの無いwatchは知らせず、時間切れで何も返さないwatchも知らせない。
  次のwatchは返したcursorの後から読むので、同じaskを2度知らせない。
  知らせの失敗はwarnだけで、watchは止まらない。
- 文面は`watch::ask_notice`: titleは`[<repo>] ask #<id> <kind>`（`naming::ask_notification_title`）、bodyはquestionの先頭（超えれば`…`）と、taskのあるaskなら改行の後の`task <id>`（runのaskは` run <run-id>`を続ける）。
  `<repo>`はqueueが束縛されたrepositoryのmain checkoutの名前。
- 呼び出しは`application::watch::InboxNotifier`（portの`AskNotifier`の実装）がaskを読み、inboxのsessionの`Cmux`の`WorkspaceBackend::notify`（`cmux notify --title --body [--workspace]`）で送る。
- `compose::watch_in`は組み立てるだけ。
  宛先は`session_workspaces`のinboxのworkspace（`up`が記録する）で、記録が無ければ`--workspace`なしで送る。
  cmuxは`watch --cmux`（既定はPATHの`cmux`）で、見つからなければ誰にも知らせない。
  watchはcmuxのterminalの子なのでsocketのpasswordが要らない。
- 認証とコストのaskを1件にまとめるとき（ADR-0047決定42）、runが加わるだけでは`ask_opened`が無いので知らせない。
- runの遷移・`answer`・`ask close`は知らせない（[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)決定5）。
  inboxはattentionを`watch`で受ける。

通知は人への知らせで、inboxのClaudeのsessionを起こさない。
watchが張られていないあいだは通知も出ないので、その後ろ盾は[ADR-t1433-5](../../adr/2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)の層が持つ: watcherの記録（[`events` / `watch`](events-watch.md#inboxのwatcherの記録adr-t906-1)）、pluginのSessionStart / Stop hook（[plugin integration](../plugin-integration.md)）、watcherが居ないまま閾値を超えてaskが待つときのsupervisorの`inbox_nudged`と`[push]`（[inboxへの知らせ](notification-route.md#supervisorによるinboxへの知らせadr-t1433-5)）。
supervisorはinboxの画面を読まず、どのterminalにも打ち込まず、`cmux notify`も送らない。
supervisorの中でcmuxに触れるのは、着地の前のe2eの関門が打つ`ping`（答えなければ実cmuxを要るe2eを外す）と、cmuxが答えたときのe2eの後始末だけ（[着地の前のe2e](landing-e2e.md)）。
