---
id: design-supervisor-lifecycle-stall-thresholds
type: design
title: "Stall thresholds"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0043
  - adr-t803-1
---

# Stall thresholds

> **予定（goal 92）**: 対話の経路の廃止（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）で、画面・idleの印・送った文の確認・`/exit`に当たる閾値は対象が無くなり、非対話のturnの閾値（ADR-t813-1決定9）だけが残る。workerのrunはtask 1437で対話でなくなり（claimとresumeが非対話に変えて`worker_mode_converted`を記録する）、workerにはもう送った文の確認・`/exit`・画面の閾値を使わない。runtimeのplannerはtask 1441で非対話のturnだけになり、画面の閾値を使わない。inboxの画面の閾値（`screen_idle_secs`）は後続のtask（1442）が実装するまでの今の姿である（plannerの画面は読まない）。

`dagq.toml`の`[stall]`（[ADR-0043](../../adr/0043-detect-stalled-worker-sessions-nudge-once-then-ask.md)の決定4）が、止まったworkerのsessionの検知の閾値を秒で持つ。読み込みは`[run.env]`と同じ`src/infrastructure/run_env.rs`（`parse_config`と、fileを読む`load_stall_config`）で、型と既定値は`src/domain/stall.rs`の`StallConfig`。

| 設定名 | 既定値 | 意味 |
| --- | --- | --- |
| `idle_without_receipt_secs` | 1200（20分） | 対話のworkerでreceiptの無いidleが促しまで続く時間と、促しの後にaskまで続く時間だった（task 1437で対話のworkerとともに廃止）。非対話のturnは待たずに促し（閾値0。[receiptの無いidleの検知](idle-without-receipt.md)）、この値は`stall_nudged`などの`threshold_secs`に記録するだけ。`stats`の`idle_without_receipt`の閾値 |
| `send_confirm_secs` | 60 | 対話のworkerに送った文が処理されるのを待つ時間と、Enterの送り直しの後に待つ時間だった（task 1437で廃止）。keyは今も受け付けて`stall_config_loaded`と`stats`の`stall_thresholds`に載るが、workerにもplannerとinboxへの送信にも使わない |
| `background_alert_secs` | 1800（30分） | `stats`の`long_background`の閾値（過去の記録の判定）。対話のworkerのidle markerの`background_tasks`から`long_background`の復旧jobを起動する閾値だったが、task 1437でそのalertを撤去し、supervisorは使わない |
| `idle_process_secs` | 1800（30分） | runのプロセスがCPU時間をほとんど使わないまま生きている時間が、supervisorが`idle_process`の[復旧job](background-recovery-job.md#cpu時間が伸びないプロセスidle_process)を起動する閾値（task 469）。`stats`の`stall_thresholds`の`idle_process_secs`の閾値（task 645） |
| `turn_silence_secs` | 900（15分） | 非対話のworkerのturnが出力の行を出さないまま続いたら、wrapperがturnを止める時間（heartbeatのあるproviderだけ。[非対話のworker](headless-worker.md#wrapperがturnを止めるとき)、task 815）。supervisorが`turns/limits.json`でwrapperに渡す |
| `turn_limit_secs` | 14400（4時間） | 非対話のworkerの1 turnの時間の上限。超えたらwrapperがturnを止める |
| `screen_idle_secs` | 120（2分） | inbox の入力待ちの画面から idle と推定する時間（planner の画面は読まない）。worker の run は画面で推定しない（task 1437）。stats の stall_thresholds に項目を持たない |

- 書式は`KEY = 秒`（正の整数。`_`の桁区切りと`#`以降のcommentを許す）。未知のkey、整数でない値、0以下、重複したkeyはエラー。表もkeyも無ければ既定値。
- `stats`は、supervisorが起動時に記録した最新の`stall_config_loaded`（payloadは`[stall]`の各値。task 469で`idle_process_secs`が加わった）があればその値を使い（出力の`stall_config.source`が`supervisor`）、無ければmain checkoutの`dagq.toml`（`file`）、それも無ければ既定値（`default`）で判定する。main checkoutはqueueが束縛されたrepositoryのmain worktreeで（[Run environment](run-environment.md#main-checkoutの決め方)）、無ければ既定値。supervisorは起動時（登録の直後）にmain checkoutの`dagq.toml`の`[stall]`を読み（fileが無ければ既定値。書式の誤りは起動のエラー）、各値と自分の`supervisor` tokenをpayloadにしたtaskの無い`stall_config_loaded`を記録して、その値で[receiptの無いidleの検知](idle-without-receipt.md#receiptの無いidleの検知)を行う（task 288。値を変えたら`down --wait` → `up`）。`send_confirm_secs`は、対話のworkerに送った文の確認（`StartCheck`）が処理された印を待つ時間と、supervisorの送信の後に入力のmarkerを人の入力に数えない幅（その2倍 + 10秒）に使っていた（ADR-0043の決定2・3、task 409）。task 1437でその確認を撤去し、今は値を読んで記録するだけである（[Session send](session-send.md)）。
- **testから秒未満を入れる経路**: StallConfig::with_millis / set_millis は test の閾値を Duration で比較する。event の秒は切り上げた threshold_secs と経過の秒を使う。worker の turn_silence_secs / turn_limit_secs は turns/limits.json の silence_ms / limit_ms で wrapper に渡る。idle_process の CPU 観測と、inbox の screen_idle の区間もミリ秒の閾値で比較する。対話 worker の送信確認と long_background の閾値は過去の設定・記録として読めるが監視には使わない。
