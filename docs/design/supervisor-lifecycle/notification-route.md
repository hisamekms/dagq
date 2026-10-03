---
id: design-supervisor-lifecycle-notification-route
type: design
title: "人への通知経路（ADR-0016で決定、ADR-0022とADR-0024で改めた）"
status: current
created: 2026-09-26
updated: 2026-10-03
last_verified: 2026-10-03 # task 1418
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0022
  - adr-t906-1
  - adr-0025
  - design-domain-model
---

# 人への通知経路（ADR-0016で決定、ADR-0022とADR-0024で改めた）

ADR-0016で次を決めた。`status`のattentionとcursor、`events --after`、`watch`は実装済み（[`status`](status.md#status)、[`events` / `watch`](events-watch.md#events--watch)）。`review`は実装済み（下記「`review`」）。`cmux notify`は[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)で送る条件と宛先が改められ、`dagq ask`が新しいaskのときにinboxへ送る形で実装済み（[人への通知](cmux-notify.md#人への通知cmux-notify)）。圧縮出力と`--full`はtask 59、短い初期promptとpluginの`SessionStart` hookはtask 65で実装済み。ADR-0016の受け手だった常駐sessionはADR-0024で退役し、受け手はinboxになった（task 100）。

- inboxとplannerは状態を持たない使い捨てのsessionで、compaction・`/clear`・再起動からの起き直しは`status`の1コマンドで行う。`status`はsupervisorの健全性、未完了run、attention、次のcursorを上限のある大きさで返す。
- `watch --after <cursor>`はcursorより後のattentionイベントかsupervisor健全性の変化までblockし、attentionと新しいcursorを返して終わる。`--role inbox`では、知らせるだけの`update_installed`と毎時の`throughput_review_reported`は単独では起こさず、次に起きたときの`events`にまとめて載る（[ADR-t1418-1](../../adr/2026-10-03-t1418-1-quiet-notices-do-not-wake-the-inbox-watch.md)、[`events` / `watch`](events-watch.md#events--watch)）。inboxはこれをbackgroundで走らせて終了で起きる。`doctor`は診断専用で、pollingには使わない。
- attentionはrun_eventsのkind（公開契約。既存のkind名とpayloadは変えず追加だけ）からdomainが判定する: runの`awaiting_integration`・`needs_session`・`failed`（のちにtriageに回るものはattentionでなくなった）、`exit_request_timed_out`（task 104からは`stuck_exit`のaskに乗せ替え）、`prompt_waiting`（ADR-0019。task 100からは`answer_prompt`のaskに乗せ替え）、supervisorの停止/stale（のちに[ADR-0025](../../adr/0025-leaseless-unfinished-run-is-a-recover-run-attention.md)でsupervisorが手放した未完了runの`recover run`が加わった）。supervisorの状態はrun_eventsに載せず`supervisors`表から導出し、schemaは変えない。
- runtimeは人のsessionのterminalに`cmux send`で打ち込まない（workerへの`/exit`と回答の送信は従来どおり）。例外として[ADR-t906-1](../../adr/2026-09-28-t906-1-guarantee-the-inbox-watch.md)がADR-0016の決定3とADR-0022の決定2をinboxについて改め、inboxのwatcherが居ないまま閾値を超えてaskが開いているとき、supervisorがinboxのworkspaceが画面でidleのときに1行の知らせを打ち込む（下記「supervisorによるinboxへの知らせ」。人が入力中のinboxには打ち込まない）。watchが張られていることは、watcherの生存の記録と`status` / `doctor`の`inbox_watcher`（[`events` / `watch`](events-watch.md#inboxのwatcherの記録adr-t906-1)）とpluginのSessionStart / Stop hook（[plugin integration](../plugin-integration.md)）で保証する。`integrate`は`watch`からもイベントの副作用としても呼ばない。
- 人の経路のコマンドは既定で圧縮し（既存キー名を変えずに省く・切り詰める）、全文は`--full`。`show`・`goal show`・`doctor`は実装済み（`doctor`は上、`show`と`goal show`は[domain-model](../domain-model.md)）。手でのレビューは`review ID`が`<run_dir>/review.md`を書き、subagentにpathを渡す（`review`は実装済み。下記「`review`」）。

## supervisorによるinboxへの知らせ（ADR-t906-1）

> **予定（goal 92）**: ADR-t906-1は[ADR-t1433-5](../../adr/2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)に置き換えられた。supervisorはinboxの画面を読まず、inboxのterminalに打ち込まない。watcherが居ないまま閾値を超えてaskが開いているときは、eventに残し、`host.toml`の`[push]`があればそれで1回送る。この節の画面のidleの推定と打ち込みは、後続のtaskが実装するまでの今の姿である。askの`cmux notify`はinboxのwatchが出す形に移る（[ADR-t1433-1](../../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)決定2）。

[ADR-t906-1](../../adr/2026-09-28-t906-1-guarantee-the-inbox-watch.md)の決定1の(3)。supervisorは毎pass（drain中も。drainはaskの答えを待つため）、`src/application/supervise/inbox_nudge.rs`で次を行う（`--no-claude`のsupervisorは知らせない）。判定の後、知らせるかどうかの前に、watcherの状態の変わり目を`inbox_watcher_absent` / `inbox_watcher_returned`に記録する（`--no-claude`でも。[`events` / `watch`](events-watch.md#watcherの変わり目の記録)）。

- **判定**: inboxのwatcherを`application::inbox_watcher::judge_with`（supervisorの`ProcessControl`で記録のpidのprocessも見る。[`events` / `watch`](events-watch.md#inboxのwatcherの記録adr-t906-1)。記録は`RunFiles`で`inbox-watchers/`から読む）で見て、`absent`のときだけ進む。居ない区間（absence）は`last_seen_at`（一度も記録が無ければ0）で識別する。区間のあいだ記録は変わらないのでこの値は一定で、watchが張られれば`alive`になって区間が閉じ、次に居なくなれば新しい`last_seen_at`の区間になる。
- **閾値**: inbox宛て（`waits_for`がinbox）のopenなaskのうち、`max(created_at, last_seen_at)`から`NUDGE_AFTER_SECS`（300秒）以上たったものが1件でもあれば知らせる。watcherが見ていたあいだの時間は数えない。
- **回数**: 同じ区間で、1回目を打ち、`NUDGE_AGAIN_SECS`（600秒）たっても`absent`ならもう1回打ち（`TYPED_NUDGES` = 2）、さらに600秒たっても戻らなければinboxのworkspaceへ`cmux notify`（[人への通知](cmux-notify.md#人への通知cmux-notify)）を1回送り、それ以上は何もしない。2回目を打てないまま（画面がidleにならない・入力欄が空にならない・1回目の文が欄に残った）前回から1200秒（間隔の2倍）たったら、2回目を飛ばしてnotify（`attempt` 3）にする。1回目を打てないあいだは何もしない。記録が一度も無いqueueの区間は`absent_since` 0で、最初のwatchが記録を書くまで同じ区間として数える。
- **打つ条件**: `session_workspaces`にinboxのworkspaceが記録され、cmuxの`exists`が真であること（無い・閉じられていれば打たず、notifyもしない）。打ち込み（1・2回目）は、画面がidleと推定でき（[ADR-t803-1](../../adr/2026-09-27-t803-1-infer-idle-from-the-screen-when-the-idle-marker-is-missing-or-stale.md)の`ScreenProbe`。inboxにはidleの印が無いので`MarkerState::Missing`で、`[stall].screen_idle_secs`以上離れた2回以上のcaptureが同じtranscriptのinput box・作業なし・ダイアログなし）、かつ最後のcaptureで入力欄が空（`AgentSignals::input_empty`。Claude Codeでは`❯`の後が空か、空の欄のplaceholderの`Try "`だけ。shell modeの`!`や人が打ちかけた文は空でない。既定は`false`で打たない）のときだけ。Claudeが作業中・ダイアログ・人が入力中なら打たず次のpassで見る。captureの span と supervisorの打ち込みの印（`supervisor-input.json`）は、queueのディレクトリの`inbox/`（印の`idle.json`は名前だけで書かれない）に置く。notify（3回目）は画面を見ない。
- **文**: `dagq: <N> open ask(s) wait for the inbox and no \`dagq watch --role inbox\` is running. Run \`dagq status --role inbox\`, then start the watch in the background as the dagq-inbox skill says.`の1行（`<N>`は閾値を超えて待ったaskの数）に、`status`の`language.instruction`と同じ言語の指示を`with_instruction`で付けて、plannerへのreviseと同じ`submit_input`で打つ。answerやattentionの中身は打たない。
- **記録と排他**: 知らせる前に、queue eventの`inbox_nudged`を`claim_inbox_nudge`で書く。payloadは`{absent_since, attempt（1から）, action（typed / notified）, at, workspace_id, open_asks, waiting_asks, absent_secs}`で、同じ`absent_since`と`attempt`のeventがあれば書かずに`false`を返す（1つのwrite transaction）。書けたsupervisorだけが知らせるので、同じqueueの複数のsupervisorも、execの引き継ぎの後のprocessも同じ知らせを二度しない。回数と前回の時刻もこのeventから組み立てる（プロセスのメモリに持たない）。
- **失敗**: 打ち込み（`submit_input`のerrorか、`Submitted`以外の結果）とnotifyの失敗はwarnを出し、queue eventの`inbox_nudge_failed`（`{absent_since, attempt, action, workspace_id, error}`）に残す。supervisorは止めず、その回は済んだものと数える（次の回は間隔の後）。

