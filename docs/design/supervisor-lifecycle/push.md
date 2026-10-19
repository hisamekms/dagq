---
id: design-supervisor-lifecycle-push
type: design
title: "KPIのpush（目標割れの記録とホストのコマンドへの通知）"
status: current
created: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-commands
  - design-supervisor-lifecycle-observer
  - design-supervisor-lifecycle-ask-push
  - adr-0051
---

# KPIのpush（目標割れの記録とホストのコマンドへの通知）

[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)の決定18（目標割れの始まりと解消のevent）・22・23の実装。
supervisorが日次の[レポート](commands.md)を書いた後に、目標割れの始まりと解消をqueueのeventに記録し、ホストの設定に書いたコマンドがあれば、日次・週次のまとめと目標割れの即時通知をそのコマンドのstdinに渡す。
runtimeはntfyやSlackなどのサービスに依存せず、秘密（webhookのURLやtoken）はrepositoryにもeventにも入らない。

ここのpushはホストのコマンドへの通知で、着地したbranchのGitのpushではない。
着地したbranchのpushは`Integrator`だけが行い、remoteがすでに着地commitを含むときの扱いも含めて[`integrate`](integrate.md#着地とpush)と`decide_push`のdoc commentが持つ（[Authorization](../authorization.md#着地とpushintegrator)、ADR-t728-2）。
その扱いはホストのコマンドへのKPI通知には適用しない。
着地したrunの工程の`push`・`push_pending`（`run_phase_changed`）もGitのpushのもので、[`integrate`](integrate.md#着地とpush)が持つ。

## 設定（host.tomlの`[push]`）

- 置き場所は`<queue dir>/host.toml`と、hostの全queueに効く`$XDG_CONFIG_HOME/dagq/host.toml`（無ければ`~/.config/dagq/host.toml`）。
  どちらもrepositoryの外で、commitされる`dagq.toml`には置かない。
  読むprocess（supervisorとinboxのwatch）は自分の`XDG_CONFIG_HOME`で読み、launchd modeのsupervisorには`up`を打ったshellの`XDG_CONFIG_HOME`がplistから渡る（[up / down](up-down.md)のplist）。
  両方に`[push]`があれば、queueのファイルの`[push]`が表ごと優先する（キーごとには混ぜない）。
  queueのファイルの空の`command`は、host全体の`[push]`をそのqueueだけ切る。
- 書式は`infrastructure::push::parse_host_push`で、1行の文字列の配列と決まったキーだけを受け付け、知らないキーは拒む。
  キーと既定は`infrastructure::push`のmodule docと`domain::kpi::push::PushConfig`が持つ。
- `[push]`が無いか`command`が空なら、runtimeはコマンドを呼ばず、pushのeventも記録しない（レポートと目標割れのeventは書く）。
  設定はレポートを書くたびに読み直す。
  読めない`[push]`は`warn`のlogを出してその日のpushをしないだけで、レポートと目標割れの記録は止めない。
- 読むのは日次のレポートを書くsupervisor（`compose`の`ReportPort::push_config`）とinboxのwatchだけで、どれも使うときに読み直す。
  supervisorはレポートを書くときと、watcherの居ないinboxへの知らせを出すと決めたとき（drainの間も。[inboxへの知らせ](ask-push.md#supervisorによるinboxへの知らせ)）に読む。
  watchは始めるときに読む（[人へのaskの通知](ask-push.md)）。
  `dagq report`と`dagq kpi`は`[push]`を読まない。

## 目標割れの記録

- supervisorのレポートのjob（[レポート](commands.md)のsupervisorの日次）が、`write_due`の後に`application::push::check_breaches`で、今の時点の日と週の`kpi`の目標の判定（[kpi](commands.md)）を読む。
  目標が1つも無く、開いた目標割れも無ければ何もしない。
  `dagq kpi`とレポートと同じくhostの負荷の記録も読むので、hostの負荷から作るKPI（`cpu_per_landing`・`load_per_core`）の目標も判定し、記録が無ければそのKPIは`not_judged`になるだけで検査は続く。
- 状態が`breach`の目標（KPIと層）は`kpi_breach_started`を、開いた目標割れのうち今の判定が`ok`か`missed`のもの（判定できる期間が目標を満たした）は解消を、目標の設定から消えたものも解消を`kpi_breach_resolved`で記録する。
  `not_judged`の間は開いたまま。
  どれもtaskの無いqueueのevent。
  payloadの欄と`label`の意味は`domain::kpi::push`の`KPI_BREACH_STARTED`・`KPI_BREACH_RESOLVED`のdoc commentが持つ。
- 記録は、同じ期間・KPI・層の最新のeventを読んで状態が変わるときだけ入れることを1つの`BEGIN IMMEDIATE`の中で行う（`SqliteQueue::record_kpi_breach`）ので、2つのsupervisorが同じ判定をしても記録は1つで、同じ状態を二度記録しない。
  解消した後に再び目標割れになれば、また`kpi_breach_started`になる。
  目標割れは期間ごとに別に扱い、その扱いは`record_kpi_breach`のdoc commentが持つ。
  開いた目標割れは`SqliteQueue::kpi_breaches_open`。
- 即時通知の印は、即時通知を送る設定で、同じlocal dayに印を付けて記録した`kpi_breach_started`が1日の上限に満たないときだけ付く（数え方は`record_kpi_breach`のdoc comment）。
  上限を超えた分と`[push]`が無いときはfalseで、日次のまとめにだけ載る。
- これらのeventはobserverの「変化が無ければ起動しない」の判定で数える（記帳のeventではない。ADR-0051の決定24）。

## 送るもの（stdin）

stdinは1つのJSONオブジェクト（UTF-8、最後に改行）。
受け取るコマンドはJSONを読んでも、`title`と`text`だけを使ってもよい。
環境変数は`DAGQ_PUSH_KIND`（messageの種類で、`kind`と同じ値。定義は`domain::kpi::push::PushKind`）、`DAGQ_QUEUE`（queueのDBのpath）、`DAGQ_REPORT_HTML`・`DAGQ_REPORT_JSON`（まとめのレポートのpath。即時通知では空）。
送るprocessの環境変数をそのまま継ぐ（`ask_opened`はinboxのsessionのwatch、ほかはsupervisor）。
ADR-0051の決定22が挙げる`resolved`の種類は、解消を即時には送らない（決定23）ので使わない。
messageごとの欄と文面は`domain::kpi::push`のそれぞれを作る関数が持つ。

- **目標割れの即時通知**（`breach`、`domain::kpi::push::breach_message`）: そのjobで即時通知の印を付けて記録した`kpi_breach_started`ごとに1通。
- **日次・週次のまとめ**（`daily` / `weekly`、`summary_message`）: そのjobで書いたレポートのうち最新の日と最新の週の分だけ1通ずつ（起動時に遡って書いた古い日の分は送らない）。
  即時通知を上限で送らなかった目標割れもまとめに載る。
- 1つのjobの中では即時通知、日次、週次の順に送る。
- **inboxの見張りの通知**（`inbox_watch`、`inbox_watch_message`）: inboxのwatchが居ないままaskがinboxを待つと、`supervise::host_ops::inbox_nudge`が同じ`ReportPort::push_config`で`[push]`を読み、その不在ごとに1通を返し、ループがこの文書の待ちに入れる。
  送り方・再試行・記録は下の「実行・再試行・失敗」と同じで、まとめと即時通知の切り替えはこのmessageを止めない。
  いつ送るかと`inbox_nudged`の記録は[inboxへの知らせ](ask-push.md#supervisorによるinboxへの知らせ)が持つ。
- **新しいaskの通知**（`ask_opened`）: inboxのwatchが`ask_opened`を返すとき、そのwatchのprocessが同じ`[push]`で1通を送る。
  新しいaskの件数を伝えるだけで、askの中身は載せない。
  送り方・再試行しないこと・失敗の残し方は[人へのaskの通知](ask-push.md)が持つ。
  supervisorの列には入らず、`kpi_push_*`のeventも記録しない。
- レポートを記録した後で目標割れの判定かメッセージづくりが失敗したら、`warn`のlogを出してその回のpushをしない（jobは失敗にせず、レポートは書き直さない）。

## 実行・再試行・失敗

- supervisorはレポートのjobが作ったメッセージをプロセスのメモリの列に積み、周回ごとに期日の来た1通をjob threadで送る（`application::supervise::observation::push`）。
  コマンドは`infrastructure::push::run_push`がshellを通さずに、自分のprocess groupで起動し、stdinにメッセージを書き、stdoutは捨て、stderrの末尾を読む（stderrをpipeで受けない理由は`run_push`のdoc comment）。
  期限を過ぎたらprocess groupごとSIGKILLで止める。
- 終了コード0なら`kpi_push_sent`、0でない・signalで終わった・timeout・起動できなかったら`kpi_push_failed`を記録する（欄は`domain::kpi::push`のdoc comment）。
  同じメッセージを決まった間隔（`RETRY_DELAYS_SECS`）でもう一度送り、最後の試みも失敗したら捨てて`kpi_push_abandoned`を記録する。
  `kpi_push_abandoned`は最新の`kpi_push_sent`より後にまだ無いときだけ記録する（`SqliteQueue::record_kpi_push_abandoned`）ので、失敗が続いても人への通知は1件。
- `kpi_push_abandoned`はinbox宛てのattention（`AttentionNext::FixPush`）になり、`status`の`attention`と`watch`に出る。
  次に`kpi_push_sent`が記録されると消える。
  人はpushのコマンドかその先のサービスを直す。
  直した後も、次のメッセージ（多くは翌日の日次のまとめ）が送れるまでattentionは残る（手で消す操作は無い）。
- 記録（`kpi_push_failed`など）が書けなかったときも、失敗したメッセージは同じ規則で再試行する。
- 秘密を出さない: eventにはコマンドのargv（プログラムのpathを含む）も、環境変数も、メッセージの中身も記録しない。
  `stderr_tail`は末尾だけで、秘密になりうる長さのコマンドの引数（プログラムを除く）と同じ文字列を伏せる（`domain::kpi::push::stderr_tail`）。
  コマンド自身がstderrに秘密を出さないようにするのはコマンドの側。
  logにもargvは出さない。
- pushの失敗はレポートの書き込み・claim・着地を止めない。
  送信中の1通はtimeoutで上限があり、停止（`down`）はその1通の終わりだけを、execの引き継ぎはその1通とレポートのjob（記録した目標割れとメッセージを失わないため）の終わりを待つ。
  列に残った未送信・再試行待ちのメッセージはプロセスとともに捨てる（1回だけ回すsupervisorは再試行も含めて列が空になるまで待つ）。
- `kpi_push_sent` / `kpi_push_failed` / `kpi_push_abandoned`はKPIの記帳のevent（`domain::kpi::BOOKKEEPING_KINDS`）として、observerの起動の判定で数えない（失敗はattentionでinboxに届くため）。

## 設定例

runtimeはどちらにも依存しない。スクリプトはrepositoryの外（例: `~/.local/bin`）に置き、実行権限を付け、host.tomlの`command`に絶対pathで書く。`jq`と`curl`を使う。

ntfy（topicはスクリプトか、commandを起動するprocessの環境変数に置く）:

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

`DAGQ_NTFY_TOPIC`はスクリプトに直接書くか、supervisorを起動する環境（`up`を打つshell）とinboxを開く環境（`dagq inbox`を打つshell）の両方に置く。
supervisorの環境だけに置くと、inboxのwatchが送る`ask_opened`には届かない。

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
```

webhookのURLを`command`の引数に直接書くこともできる（eventの`stderr_tail`では伏せる）が、host.tomlを他人と共有しうるなら上のようにファイルか環境変数に置く。
