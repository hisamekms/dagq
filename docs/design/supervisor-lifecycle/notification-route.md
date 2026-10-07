---
id: design-supervisor-lifecycle-notification-route
type: design
title: "人への通知経路（ADR-0016で決定、ADR-0022とADR-0024で改めた）"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0022
  - adr-t906-1
  - adr-t1433-1
  - adr-t1433-5
  - adr-0025
  - design-domain-model
---

# 人への通知経路（ADR-0016で決定、ADR-0022とADR-0024で改めた）

対話 worker の prompt_waiting / answer_prompt / stuck_exit / send_unconfirmed / intervene は廃止した処理の過去の記録を読むために残る。以下の該当する集計・attention・既存 ask の扱いは過去の記録の互換を含む。新しい worker の監視は turn を使う（task 1437、[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）。

ADR-0016で次を決めた。`status`のattentionとcursor、`events --after`、`watch`は実装済み（[`status`](status.md#status)、[`events` / `watch`](events-watch.md#events--watch)）。`review`は実装済み（下記「`review`」）。`cmux notify`は[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)で送る条件と宛先が改められ、[ADR-t1433-1](../../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)決定2で、inboxの`watch --role inbox`が`ask_opened`を見て送る形になった（[人への通知](cmux-notify.md#人への通知cmux-notify)）。圧縮出力と`--full`はtask 59、短い初期promptとpluginの`SessionStart` hookはtask 65で実装済み。ADR-0016の受け手だった常駐sessionはADR-0024で退役し、受け手はinboxになった（task 100）。

- inboxとplannerは状態を持たない使い捨てのsessionで、compaction・`/clear`・再起動からの起き直しは`status`の1コマンドで行う。`status`はsupervisorの健全性、未完了run、attention、次のcursorを上限のある大きさで返す。
- `watch --after <cursor>`はcursorより後のattentionイベントかsupervisor健全性の変化までblockし、attentionと新しいcursorを返して終わる。`--role inbox`では、知らせるだけの`update_installed`と規則に当たった毎時の`throughput_review_reported`は単独では起こさず、次に起きたときの`events`にまとめて載る（[ADR-t1418-1](../../adr/2026-10-03-t1418-1-quiet-notices-do-not-wake-the-inbox-watch.md)、[`events` / `watch`](events-watch.md#events--watch)）。inboxはこれをbackgroundで走らせて終了で起きる。`doctor`は診断専用で、pollingには使わない。
- attentionはrun_eventsのkind（公開契約。既存のkind名とpayloadは変えず追加だけ）からdomainが判定する: runの`awaiting_integration`・`needs_session`・`failed`（のちにtriageに回るものはattentionでなくなった）、`exit_request_timed_out`（task 104からは`stuck_exit`のaskに乗せ替え）、`prompt_waiting`（ADR-0019。task 100からは`answer_prompt`のaskに乗せ替え）、supervisorの停止/stale（のちに[ADR-0025](../../adr/0025-leaseless-unfinished-run-is-a-recover-run-attention.md)でsupervisorが手放した未完了runの`recover run`が加わった）。supervisorの状態はrun_eventsに載せず`supervisors`表から導出し、schemaは変えない。
- runtimeはどのsessionのterminalにも打ち込まない（[ADR-t1433-5](../../adr/2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)決定2）。
  workerとplannerへの終了と回答は次のturnの依頼として書き、inboxのwatcherが居ないときの後ろ盾も画面を読まず打ち込まない（下記「supervisorによるinboxへの知らせ」）。
  watchが張られていることは、watcherの生存の記録と`status` / `doctor`の`inbox_watcher`（[`events` / `watch`](events-watch.md#inboxのwatcherの記録adr-t906-1)）とpluginのSessionStart / Stop hook（[plugin integration](../plugin-integration.md)）で保証する。
  `integrate`は`watch`からもイベントの副作用としても呼ばない。
- 人の経路のコマンドは既定で圧縮し（既存キー名を変えずに省く・切り詰める）、全文は`--full`。`show`・`goal show`・`doctor`は実装済み（`doctor`は上、`show`と`goal show`は[domain-model](../domain-model.md)）。手でのレビューは`review ID`が`<run_dir>/review.md`を書き、subagentにpathを渡す（`review`は実装済み。下記「`review`」）。

## supervisorによるinboxへの知らせ（ADR-t1433-5）

[ADR-t1433-5](../../adr/2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)決定1の(3)。
入口は`src/application/supervise/inbox_nudge.rs`で、supervisorは毎pass（drain中も。drainはaskの答えを待つため）次を行う。
supervisorはinboxの画面を読まず、どのterminalにも打ち込まず、cmuxを呼ばない。
判定の前に、watcherの状態の変わり目を`inbox_watcher_absent` / `inbox_watcher_returned`に記録する（[`events` / `watch`](events-watch.md#watcherの変わり目の記録)）。

- **判定**: inboxのwatcherを`application::inbox_watcher::judge_with`で見て、`absent`のときだけ進む。
  居ない区間（absence）は`last_seen_at`（一度も記録が無ければ0）で識別し、watchが張られれば区間が閉じる。
- **閾値**: inbox宛てのopenなaskのうち、開いた時刻とwatcherを最後に見た時刻の遅い方から一定の時間（`supervise::inbox_nudge`のdoc comment）たったものが1件でもあれば知らせる。
  watcherが見ていたあいだの時間は数えない。
- **回数**: 同じ区間で1回だけ。
  人に届けるのは、inboxのwatchが居ないまま開いているaskがあることと件数だけで、askやattentionの中身は送らない。
- **記録と排他**: 知らせる前に、queue eventの`inbox_nudged`を`claim_inbox_nudge`で書く。
  eventは居ない区間を識別し、知らせ方（`action`）と待つaskの件数を持ち、askの中身は持たない（payloadは`supervise::inbox_nudge`のdoc comment）。
  同じ区間のeventがあれば書かずに`false`を返す（1つのwrite transaction）。
  書けたsupervisorだけが知らせるので、複数のsupervisorもexecの後のprocessも二度知らせない。
  `action`は、`host.toml`の`[push]`があれば`pushed`（messageをpushの待ちに入れた。送った記録は`kpi_push_sent`で、送る前にsupervisorがexecや停止で終われば送られず、同じ不在には二度入れない）、無ければ`recorded`（eventだけ）。
  `[push]`を読めなければ`recorded`にして`push_error`に理由を書く。
  過去の記録の`typed`・`notified`と`inbox_nudge_failed`（画面への打ち込みと`cmux notify`）はそのまま読める。
- **`[push]`**: `pushed`のとき、KPIのpushと同じ`[push]`のcommandに`kind`が`inbox_watch`のmessage（`domain::kpi::push::inbox_watch_message`。titleと件数だけ）を1つ渡す。
  送り方・やり直し・記録（`kpi_push_sent` / `kpi_push_failed`、`push_kind`は`inbox_watch`）はKPIのpushと同じ（[KPI](kpi.md)）。
  `[push]`の`daily`・`breach`はこのmessageを止めない。
