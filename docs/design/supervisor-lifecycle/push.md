---
id: design-supervisor-lifecycle-push
type: design
title: "KPIのpush（目標割れの記録とホストのコマンドへの通知）"
status: current
created: 2026-09-27
updated: 2026-10-01
last_verified: 2026-10-01
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-kpi
  - design-supervisor-lifecycle-report
  - design-supervisor-lifecycle-observer
  - adr-0051
---

# KPIのpush（目標割れの記録とホストのコマンドへの通知）

[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)の決定18（目標割れの始まりと解消のevent）・22・23の実装（task 432）。supervisorが日次の[レポート](report.md)を書いた後に、目標割れの始まりと解消をqueueのeventに記録し、ホストの設定に書いたコマンドがあれば、日次・週次のまとめと目標割れの即時通知をそのコマンドのstdinに渡す。runtimeはntfyやSlackなどのサービスに依存せず、秘密（webhookのURLやtoken）はrepositoryにもeventにも入らない。

ここのpushはホストのコマンドへの通知で、着地したbranchのGitのpushではない。着地したbranchのpushは`Integrator`だけが行う（[`integrate`](integrate.md)の9、[Authorization](../authorization.md#着地とpushintegrator)、ADR-t728-2）。Gitのpushが失敗しても、remoteのbranchが着地commitをすでに含むと確認できたときは`push_finished`（payloadの`already_delivered: true`）として扱う。この規則はホストのコマンドへのKPI通知には適用しない。

## 設定（host.tomlの`[push]`）

- 置き場所は`<queue dir>/host.toml`と、hostの全queueに効く`$XDG_CONFIG_HOME/dagq/host.toml`（無ければ`~/.config/dagq/host.toml`）。どちらもrepositoryの外で、commitされる`dagq.toml`には置かない。supervisorは自分のprocessの`XDG_CONFIG_HOME`で読み、launchd modeでは`up`を打ったshellの`XDG_CONFIG_HOME`がplistから渡る（[up / down](up-down.md)のplist）。両方に`[push]`があれば、queueのファイルの`[push]`が表ごと優先する（キーごとには混ぜない）。queueのファイルの`command = []`は、host全体の`[push]`をそのqueueだけ切る。
- 書式（`infrastructure::push::parse_host_push`。1行の文字列の配列と、下のキーだけを受け付け、知らないキーは拒む）:

  ```toml
  [push]
  command = ["/Users/me/.local/bin/dagq-push-ntfy"]   # argvの配列。shellを通さない。必須
  timeout_secs = 30          # 既定30
  daily = true               # 日次・週次のまとめを送る。既定true
  breach = true              # 目標割れの即時通知を送る。既定true
  max_breach_per_day = 3     # 即時通知の1日（hostのlocal timezone）の上限。既定3
  ```

- `[push]`が無いか`command`が空なら、runtimeはコマンドを呼ばず、pushのeventも記録しない（レポートと目標割れのeventは書く）。設定はレポートを書くたびに読み直す。読めない`[push]`は`warn`のlogを出してその日のpushをしないだけで、レポートと目標割れの記録は止めない。
- 読むのは`supervise --report-daily`のsupervisorだけ（`compose`の`ReportPort::push_config`）。`dagq report`と`dagq kpi`は`[push]`を読まない。ライブラリの`SuperviseOptions::host_config`はhost全体のファイルの場所を、`push_retry`は再試行の間隔を変える（testが使う）。

## 目標割れの記録

- supervisorのレポートのjob（[レポート](report.md)のsupervisorの日次）が、`write_due`の後に`application::push::check_breaches`で、今の時点の日と週の`kpi`（`--last 2`相当）の目標の判定（[kpi](kpi.md#目標targets)）を読む。目標が1つも無く、開いた目標割れも無ければ何もしない。
- 状態が`breach`の目標（KPIと層）は`kpi_breach_started`を、開いた目標割れのうち今の判定が`ok`か`missed`のもの（判定できる期間が目標を満たした）は`kpi_breach_resolved`（`reason: met`）を、目標の設定から消えたものは`kpi_breach_resolved`（`reason: target_removed`）を記録する。`not_judged`の間は開いたまま。どれもtaskの無いqueueのevent。
  - `kpi_breach_started`のpayload: `period`（`day` / `week`）・`kpi`・`stratum`・`stat`・`min`・`max`・`source`・`since`（目標割れの始まりの期間）・`streak`・`label`と`value`（最後に判定できた期間とその値）・`pushed`（即時通知を送るか）・`day`（`pushed`を数えたhostのlocal day）。
  - `kpi_breach_resolved`のpayload: `period`・`kpi`・`stratum`・`label`（最後に判定できた期間。`ok`なら目標を満たした期間、`missed`なら連続が切れた後にまた外れた期間）・`reason`。
- 目標割れは`period`ごとに別に扱う。同じKPIと層が日と週の両方で目標割れになれば、それぞれ`kpi_breach_started`になり、それぞれ即時通知の対象になる（日の連続と週の連続は別の判定で、始まりの時期も違うため。どちらも1日の上限に数える）。
- 記録は、同じ`period`・`kpi`・`stratum`の最新のeventを読んで状態が変わるときだけ入れることを1つの`BEGIN IMMEDIATE`の中で行う（`SqliteQueue::record_kpi_breach`）ので、2つのsupervisorが同じ判定をしても記録は1つで、同じ状態を二度記録しない。解消した後に再び目標割れになれば、また`kpi_breach_started`になる。開いた目標割れは`SqliteQueue::kpi_breaches_open`。
- `pushed`は、`[push]`があり`breach = true`で、同じlocal dayに`pushed: true`で記録した`kpi_breach_started`が`max_breach_per_day`に満たないときだけtrue（同じトランザクションで数える。日と週の目標割れを合わせて数える）。上限を超えた分と`[push]`が無いときはfalseで、日次のまとめにだけ載る。
- これらのeventはobserverの「変化が無ければ起動しない」の判定で数える（記帳のeventではない。ADR-0051の決定24）。

## 送るもの（stdin）

stdinは1つのJSONオブジェクト（UTF-8、最後に改行）。受け取るコマンドはJSONを読んでも、`title`と`text`だけを使ってもよい。環境変数は`DAGQ_PUSH_KIND`（`daily` / `weekly` / `breach`）、`DAGQ_QUEUE`（queueのDBのpath）、`DAGQ_REPORT_HTML`・`DAGQ_REPORT_JSON`（まとめのレポートのpath。即時通知では空）。supervisorの環境変数はそのまま継ぐ。ADR-0051の決定22が挙げる`resolved`の種類は、解消を即時には送らない（決定23）ので使わない。

- **目標割れの即時通知**（`breach`、`domain::kpi::push::breach_message`）: そのjobで記録した`pushed: true`の`kpi_breach_started`ごとに1通。`kind`・`queue`・`period`（最後に判定できた期間）・`title`（`<repositoryの名前> target breach: <KPI> (<層>)`。固定の文字列は英語。task 625）・`text`（値・stat・目標・続いた期間の数・始まり）・`breaches`（1件: `kpi`・`stratum`・`stat`・`value`・`min`・`max`・`periods`・`since`）・`resolved`（空）・`report_html` / `report_json`（null）。
- **日次・週次のまとめ**（`daily` / `weekly`、`summary_message`）: そのjobで書いたレポートのうち最新の日と最新の週の分だけ1通ずつ（起動時に遡って書いた古い日の分は送らない）。`title`（`<名前> <期間>: landings N, breaches M`）、`text`（着地の数と`lead_time`・`phase.work`・`first_pass_rate`・`revise_rate`・`asks_per_landing`の`all`の層の値、目標割れ、1期間の外れ（`missed`）、そのjobで記録した同じ周期の解消、open なaskの数、レポートのpath）、`breaches`（そのレポートの目標のうち`breach`のもの全部。即時通知を上限で送らなかった分もここに載る）、`missed`、`resolved`、`open_asks`、`report_html` / `report_json`。値の単位はレポートのHTMLと同じ（秒は`1h 02m`、割合は`%`）。
- 1つのjobの中では即時通知、日次、週次の順に送る。
- レポートを記録した後で目標割れの判定かメッセージづくりが失敗したら、`warn`のlogを出してその回のpushをしない（jobは失敗にせず、レポートは書き直さない）。

## 実行・再試行・失敗

- supervisorはレポートのjobが作ったメッセージをプロセスのメモリの列に積み、周回ごとに期日の来た1通をjob threadで送る（`application::supervise::push`）。コマンドは`infrastructure::push::run_push`がshellを通さずに、自分のprocess groupで起動し、stdinにメッセージを書き、stdoutは捨て、stderrの末尾を読む。`timeout_secs`を過ぎたらprocess groupごとSIGKILLで止める。
- 終了コード0なら`kpi_push_sent`（`push_kind`・`period`・`attempt`）。0でない・signalで終わった・timeout・起動できなかったら`kpi_push_failed`（`push_kind`・`period`・`attempt`・`exit_code`・`signal`・`timed_out`・`error`・`stderr_tail`・`gave_up`）。同じメッセージを1分後と5分後（`RETRY_DELAYS_SECS`）にもう一度送り、3回とも失敗したら捨てて`gave_up: true`にし、`kpi_push_abandoned`（`push_kind`・`period`・`attempts`・`reason_category: recovery_failed`・`message`）を記録する。`kpi_push_abandoned`は最新の`kpi_push_sent`より後にまだ無いときだけ記録する（`SqliteQueue::record_kpi_push_abandoned`）ので、失敗が続いても人への通知は1件。
- `kpi_push_abandoned`はinbox宛てのattention `fix the push command`（`AttentionNext::FixPush`、`reason_category: recovery_failed`）になり、`status`の`attention`と`watch`に出る。次に`kpi_push_sent`が記録されると消える。人はpushのコマンドかその先のサービスを直す。直した後も、次のメッセージ（多くは翌日の日次のまとめ）が送れるまでattentionは残る（手で消す操作は無い）。
- 記録（`kpi_push_failed`など）が書けなかったときも、失敗したメッセージは同じ規則で再試行する。
- 秘密を出さない: eventにはコマンドのargv（プログラムのpathを含む。起動できないときの`error`は`could not start the push command: <OSのエラー>`）も、環境変数も、メッセージの中身も記録しない。`stderr_tail`は末尾2000バイトで、コマンドの引数（プログラムを除く8文字以上のもの）と同じ文字列を`[argument]`に置き換える。コマンド自身がstderrに秘密を出さないようにするのはコマンドの側。logにもargvは出さない。
- pushの失敗はレポートの書き込み・claim・着地を止めない。送信中の1通はtimeoutで上限があり、停止（`down`）はその1通の終わりだけを、execの引き継ぎはその1通とレポートのjob（記録した目標割れとメッセージを失わないため）の終わりを待つ。列に残った未送信・再試行待ちのメッセージはプロセスとともに捨てる（`--once`は再試行も含めて列が空になるまで待つ）。
- `kpi_push_sent` / `kpi_push_failed` / `kpi_push_abandoned`はKPIの記帳のevent（`domain::kpi::BOOKKEEPING_KINDS`）として、observerの起動の判定で数えない（失敗はattentionでinboxに届くため）。

## 設定例

runtimeはどちらにも依存しない。スクリプトはrepositoryの外（例: `~/.local/bin`）に置き、実行権限を付け、host.tomlの`command`に絶対pathで書く。`jq`と`curl`を使う。

ntfy（topicはスクリプトか、supervisorの環境変数に置く）:

```sh
#!/bin/sh
# dagq-push-ntfy: ntfyにtitleとtextを送る
json=$(cat)
title=$(printf '%s' "$json" | jq -r .title)
printf '%s' "$json" | jq -r .text |
  curl -fsS -H "Title: $title" --data-binary @- "https://ntfy.sh/${DAGQ_NTFY_TOPIC:?}"
```

```toml
# ~/.config/dagq/host.toml
[push]
command = ["/Users/me/.local/bin/dagq-push-ntfy"]
```

`DAGQ_NTFY_TOPIC`はsupervisorを起動する環境（`up`を打つshell）に置くか、スクリプトに直接書く。

Slack の Incoming Webhook（URLはrepositoryの外のファイルから読む）:

```sh
#!/bin/sh
# dagq-push-slack: SlackのIncoming Webhookに送る
url=$(cat "$HOME/.config/dagq/slack-webhook-url")
jq '{text: (.title + "\n" + .text)}' |
  curl -fsS -H 'Content-Type: application/json' --data-binary @- "$url"
```

```toml
# <queue dir>/host.toml
[push]
command = ["/Users/me/.local/bin/dagq-push-slack"]
timeout_secs = 20
max_breach_per_day = 2
```

webhookのURLを`command`の引数に直接書くこともできる（eventの`stderr_tail`では伏せる）が、host.tomlを他人と共有しうるなら上のようにファイルか環境変数に置く。
