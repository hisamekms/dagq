---
id: design-supervisor-lifecycle-ask-push
type: design
title: "人へのaskの通知（`[push]`）"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-push
  - adr-0022
  - adr-t906-1
  - adr-t1433-5
  - adr-t2159-1
---

# 人へのaskの通知（`[push]`）

新しいaskを人に知らせるのは、inboxのsessionの中で動く`watch --role inbox`だけである（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定4）。
supervisor・queue service・observerは知らせず、askを開く経路（`ask`のCLI、runtimeのask、storeが判断と同じトランザクションで開くask）はどれも何も知らせない。

- 送り先はhostの`host.toml`の`[push]`のcommandで、読み方（queueの`host.toml`がhost全体のものに優先する）とcommandの起動はKPIの[push](push.md)と同じ。
  inboxのwatchは始めるたびに`[push]`を読み直し、無ければ何も送らない（`application::watch::push_notifier`）。
- inboxのwatchがaskの`ask_opened`を返すとき、そのwatchごとに1通を送る。
  inbox以外のroleのwatch、roleの無いwatch、時間切れで何も返さないwatchは送らない。
  次のwatchは返したcursorの後から読むので、同じaskを2度知らせない。
- messageの種類は`ask_opened`で、新しいaskの件数を伝えるだけで、askの中身（question・option・task・ID）は送らない。
  stdinの形と環境変数の名前はKPIの[push](push.md)の「送るもの」と同じで、commandが継ぐ環境変数はsupervisorのものではなく、watchを回すinboxのsessionのものである。
- 送るのはwatchのprocessで、1回だけ試し、再試行もeventの記録もしない（watchはqueueを読み取り専用で開く）。
  送れなかったとき（commandの失敗・時間切れ・`[push]`が読めない）は、watchを止めずに出力の`ask_push`に理由を残し、warnのlogを出す。
  理由にはcommandの引数を出さない（KPIのpushの`stderr_tail`と同じ置き換え）。
- 認証とコストのaskを1件にまとめるとき（ADR-0047決定42）、runが加わるだけでは`ask_opened`が無いので知らせない。
- runの遷移・`answer`・`ask close`は知らせない（[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)決定5）。
  inboxはattentionを`watch`で受ける。
  `watch`はqueueを読むだけで、`integrate`などの操作を呼ばない。

通知は人への知らせで、inboxのClaudeのsessionを起こさない（sessionはwatchの返りで起きる）。
watchが張られていないあいだは通知も出ないので、その後ろ盾は[ADR-t1433-5](../../adr/2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)の層が持つ: watcherの記録（[`events` / `watch`](commands.md)）、pluginのSessionStart / Stop hook（[plugin integration](../plugin-integration.md)）、watcherが居ないまま閾値を超えてaskが待つときのsupervisorの`inbox_nudged`と`[push]`（下の「supervisorによるinboxへの知らせ」）。
supervisorはinboxの画面を読まず、どのterminalにも打ち込まない。

## supervisorによるinboxへの知らせ<a id="supervisorによるinboxへの知らせ"></a>

[ADR-t1433-5](../../adr/2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)決定1の(3)。
入口は`application::supervise::inbox_nudge`（閾値とeventのpayloadはそのdoc comment）で、supervisorは毎pass、drainの間も行う（drainはaskの答えを待つため）。

- inboxのwatcherが`absent`（`application::inbox_watcher::judge_with`）で、inbox宛てのopenなaskが閾値より長く待つときだけ知らせる。
  待ちは、askを開いた時刻とwatcherを最後に見た時刻の遅い方から数え、watcherが見ていたあいだは数えない。
- 知らせはwatcherが居ない区間（`last_seen_at`で識別する）ごとに1回で、居ないことと待つaskの件数だけを伝え、askとattentionの中身は送らない。
- 知らせる前に`inbox_nudged`を1つのwrite transactionで書き（`claim_inbox_nudge`）、書けたsupervisorだけが知らせるので、複数のsupervisorもexecの後のprocessも二度知らせない。
- `[push]`があればKPIのpushと同じcommandにmessageを1つ渡し、送り方と記録はKPIの[push](push.md)と同じで、`daily`・`breach`の設定はこのmessageを止めない。
  無いか読めなければeventだけを残す。
  messageは待ちに入れるだけなので、送る前にsupervisorがexecか停止で終われば送られず、同じ区間には二度入れない。
