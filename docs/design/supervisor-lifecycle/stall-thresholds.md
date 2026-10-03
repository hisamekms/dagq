---
id: design-supervisor-lifecycle-stall-thresholds
type: design
title: "Stall thresholds"
status: current
created: 2026-09-26
updated: 2026-10-03
last_verified: 2026-09-29
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0043
  - adr-t803-1
---

# Stall thresholds

> **予定（goal 92）**: 対話の経路の廃止（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）で、画面・idleの印・送った文の確認・`/exit`に当たる閾値は対象が無くなり、非対話のturnの閾値（ADR-t813-1決定9）だけが残る。後続のtaskが実装するまでの今の姿である。

`dagq.toml`の`[stall]`（[ADR-0043](../../adr/0043-detect-stalled-worker-sessions-nudge-once-then-ask.md)の決定4）が、止まったworkerのsessionの検知の閾値を秒で持つ。読み込みは`[run.env]`と同じ`src/infrastructure/run_env.rs`（`parse_config`と、fileを読む`load_stall_config`）で、型と既定値は`src/domain/stall.rs`の`StallConfig`。

| 設定名 | 既定値 | 意味 |
| --- | --- | --- |
| `idle_without_receipt_secs` | 1200（20分） | receiptの無いidleが促しまで続く時間と、促しの後にaskまで続く時間。`stats`の`idle_without_receipt`の閾値 |
| `send_confirm_secs` | 60 | 送った文が処理されるのを待つ時間と、Enterの送り直しの後に待つ時間 |
| `background_alert_secs` | 1800（30分） | `stats`の`long_background`の閾値と、supervisorが[復旧job](background-recovery-job.md#backgroundの処理が終わらないときの復旧job)を起動する閾値 |
| `idle_process_secs` | 1800（30分） | runのプロセスがCPU時間をほとんど使わないまま生きている時間が、supervisorが`idle_process`の[復旧job](background-recovery-job.md#cpu時間が伸びないプロセスidle_process)を起動する閾値（task 469）。`stats`の`stall_thresholds`の`idle_process_secs`の閾値（task 645） |
| `turn_silence_secs` | 900（15分） | 非対話のworkerのturnが出力の行を出さないまま続いたら、wrapperがturnを止める時間（heartbeatのあるproviderだけ。[非対話のworker](headless-worker.md#wrapperがturnを止めるとき)、task 815）。supervisorが`turns/limits.json`でwrapperに渡す |
| `turn_limit_secs` | 14400（4時間） | 非対話のworkerの1 turnの時間の上限。超えたらwrapperがturnを止める |
| `screen_idle_secs` | 120（2分） | idleの印が無いか最後の入力より古いsessionを、画面から`idle`と推定するまでに、入力待ち・作業中でない・ダイアログ無しの画面が続く時間（[ADR-t803-1](../../adr/2026-09-27-t803-1-infer-idle-from-the-screen-when-the-idle-marker-is-missing-or-stale.md)、[画面からのidleの推定](receipt-and-session-exit.md#画面からのidleの推定)）。検知ではないので`stats`の`stall_thresholds`には項目を持たない（`stall_config`には出る） |

- 書式は`KEY = 秒`（正の整数。`_`の桁区切りと`#`以降のcommentを許す）。未知のkey、整数でない値、0以下、重複したkeyはエラー。表もkeyも無ければ既定値。
- `stats`は、supervisorが起動時に記録した最新の`stall_config_loaded`（payloadは`[stall]`の各値。task 469で`idle_process_secs`が加わった）があればその値を使い（出力の`stall_config.source`が`supervisor`）、無ければmain checkoutの`dagq.toml`（`file`）、それも無ければ既定値（`default`）で判定する。main checkoutはqueueが束縛されたrepositoryのmain worktreeで（[Run environment](run-environment.md#main-checkoutの決め方)）、無ければ既定値。supervisorは起動時（登録の直後）にmain checkoutの`dagq.toml`の`[stall]`を読み（fileが無ければ既定値。書式の誤りは起動のエラー）、各値と自分の`supervisor` tokenをpayloadにしたtaskの無い`stall_config_loaded`を記録して、その値で[receiptの無いidleの検知](idle-without-receipt.md#receiptの無いidleの検知)を行う（task 288。値を変えたら`down --wait` → `up`）。`send_confirm_secs`は、送った文の確認（`StartCheck`、[送信と確認](session-send.md#sessionへの送信と確認)の3）が処理された印を待つ時間と、supervisorの送信の後に入力のmarkerを人の入力に数えない幅（その2倍 + 10秒）に使う（ADR-0043の決定2・3、task 409）。
- **testから秒未満を入れる経路**（goal 68、task 1045）: `dagq.toml`の書式と既定値は上のとおり（正の整数の秒）で変わらない。testは`SuperviseOptions.stall`に渡す`StallConfig`の閾値を`StallConfig::with_millis(key, ミリ秒)`（`set_millis`）でミリ秒で入れられる（例: `StallConfig::default().with_millis("idle_without_receipt_secs", 200)`）。入れた値は`StallConfig`の`millis`（serializeしない）に持ち、`*_secs`の欄は切り上げた秒（200ミリ秒なら1）になるので、eventの`threshold_secs`と`stall_config_loaded`は秒のまま。`set`で秒を入れ直すとミリ秒は消える。検知は閾値を`StallConfig::threshold(key)`（と`idle_without_receipt()`・`send_confirm()`・`background_alert()`・`idle_process()`・`screen_idle()`）の`Duration`で読み、経過もミリ秒の精度で比べる（receiptの無いidleと閾値の手前の入力、送信の確認、`long_background`、`idle_process`（`CpuWatch::idle`）、Escで止めたturn、画面からのidleの推定の区間（`screen-idle.json`の`first_seen_ms` / `last_seen_ms`はUnixミリ秒。秒で書いた古いfileは読まずに区間をやり直す））。`turn_silence_secs`・`turn_limit_secs`のミリ秒は`turns/limits.json`の`silence_ms` / `limit_ms`でwrapperに渡る。秒で入れた閾値の発火の条件は、1秒未満の丸めを除いて前と同じ。eventに記録する経過の秒（`idle_secs`、`observed_secs`など）は切り捨ての秒のままで、送信の確認の`waited_secs`は待った時間を切り上げた秒。画面の推定の`idle_inferred`には`since_ms`と`observed_ms`も記録する。画面のcaptureの間隔（`screen_idle_secs`の半分を秒に切り捨て、上限60秒）は秒の単位のままで、2秒未満の閾値では毎tickになる。プロセスの見本の間隔（閾値の10分の1を秒に切り捨て、1秒〜1分）は閾値より長くしないので、1秒未満の閾値ではその閾値ごとになる。
