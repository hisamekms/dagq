---
id: design-supervisor-lifecycle-report
type: design
title: "KPIのレポート（`report`）"
status: current
created: 2026-09-27
updated: 2026-09-27
last_verified: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-kpi
  - design-supervisor-lifecycle-observer
  - adr-0051
---

# KPIのレポート（`report`）

[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)の決定20・21の実装（task 431）。supervisorが日に1回、[`kpi`](kpi.md)の結果から日とISO週のレポートをJSONと自己完結のHTMLでqueueのディレクトリに書き、人は`dagq report`で同じものを手で書く。集計は`dagq kpi`と同じ関数で、LLMもrun slotも使わず、queueのディレクトリの外に何も送らない。JSONは集計の写しで、正はrun_events（消しても`dagq kpi` / `dagq report`で作り直せる）。

## 中身

- **JSON**は`dagq kpi --period <day|week> --at <その期間>`（`--last`は既定の7、期間は古い順で最後がレポートの期間）と同じ形に、`report`（`period`・`label`・`partial`・`generated_at`・`build`（書いたバイナリのbuild識別子））、`findings_open`（`open` / `proposed`のfindingの数）、`findings`（そのうち`findings`の順（影響の大きい順）で先頭10件。どれもレポートを書いた時点の値で、遡って書いた日のレポートでもその日の値ではないの`id`・`kind`・`target`・`subject`・`summary`・`impact`・`status`・`occurrences`・`last_seen_at`）を足したもの。`domain::kpi::report::Report`。
- **HTML**（`domain::kpi::report::render_html`）は1ファイルで、CSSと小さなグラフ（inline SVG）を埋め込み、script・外部のCSS・font・画像・CDNを読まない（JavaScriptを使わない）。上から: 見出し（期間、区間、終わったrunの数、生成時刻、build、`partial`の印）、目標（`breach` → `missed` → `not_judged` → `ok`の順に、KPI・層・stat・目標・最新の値・状態・連続・始まり・設定の出どころ）、推移（`all`の層に値のあるKPIごとに、並べた期間の棒グラフ、最新・前・差・判定。印のある期間の上に三角、`partial`の期間は薄い棒）、変更の印の一覧、レポートの期間のKPIの表（`all`の層の`n`・`value`・`median`・`p90`・前・差・7日の基準・判定と、`<details>`に他の層）、open なfindingの上位、記録の無いKPI（`unavailable`）。値は秒のKPIを`1h 02m`の形、割合を`%`（差は`pt`）で出す。文字列はHTMLのescapeをする。
- `index.html`（`index_html`）は日・週のレポートへの相対リンクを新しい順に並べ、書くたびに書き直す。

## 場所と保持

- `<queue dir>/reports/`の`daily/YYYY-MM-DD.{json,html}`と`weekly/YYYY-Www.{json,html}`、`index.html`。まだ終わっていない今日・今週のレポート（`partial`）は`YYYY-MM-DD.partial.*`と名前を分け、完結した日のファイルを上書きしない。各ファイルは同じディレクトリの一時ファイル（`.<名前>.<pid>.tmp`）に書いてからrenameする（`application::report::write`）。
- 保持は書くたびに適用する: 日は今日より前の`keep_daily_days`日（既定90）、週は今週より前の`keep_weekly_weeks`週（既定104）を残し、それより古いものと、同じ期間の完結したレポートがある`partial`のレポートを消す（`domain::kpi::report::expired`）。レポートの名前の形でないファイルは触らない。ただし書き手が途中で終わって残した一時ファイル（`.`で始まり`.tmp`で終わる）は、1時間より古ければ消す（引き継ぎのexecはjobを待たないため）。日数はhost.tomlの`[report]`（`<queue dir>/host.toml`が`$XDG_CONFIG_HOME/dagq/host.toml`（無ければ`~/.config/dagq/host.toml`）にキーごとに優先。`infrastructure::report_config`）:

  ```toml
  [report]
  keep_daily_days = 90
  keep_weekly_weeks = 104
  ```

## supervisorの日次

- `supervise --report-daily`（既定true。ライブラリの`SuperviseOptions::report_daily`は既定false）。`application::supervise::report`が、周回ごとにhostのlocal timezoneの今日（`local_day`）を見て、前に書き終えた日と違えば（起動直後の最初の周回を含む）job threadで`application::report::write_due`を走らせる。書くのは、今日より前の7日（`BACKFILL_DAYS`）と先週（ISO週）のうち`report_written`が記録されていないものを古い順に（保持の日数が7日より短ければ、保持がすぐ消す日は書かない）。設定（dagq.tomlの`[kpi]`、host.tomlの`[kpi]`と`[report]`、timezone）は書くたびに読み直す。
- 書いたレポートごとにqueueのevent `report_written`（taskの無いqueueのevent。payloadは`period`・`label`・`html`・`json`・`removed`（保持で消したファイルの数）・`build`・`supervisor`）を記録する。記録は同じ`period`と`label`の`report_written`が無いことを同じ書き込みのトランザクションで確かめてから入れる（`SqliteQueue::record_report_written`）ので、2つのsupervisorが同じレポートを書いても記録は1つで、同じ日は二度書かない（ファイルはどちらが書いても同じ中身をrenameで置く）。
- 始めるのはclaimを続けているsupervisorだけ（drain中・停止中・引き継ぎの待ちでは始めず、走っているjobの終わりだけを拾う）。loopはjobを待たないが、`--once`で終わる前には待つ。失敗は`warn`のlogだけで、10分後に書いていない分をもう一度試す。claimと着地は止めない。引き継ぎのexecで途中のjobが切れても、記録の無いレポートは次のプロセスが書く。
- observerの「自分以外のeventが無ければ起動しない」の判定（`events_besides`）は`report_written`を数えない（ADR-0051の決定24）。

## `dagq report`

`dagq report [--period day|week] [--at <cursor>] [--out <dir>] [--print json]`（`compose::OneShot::report_of`）。`--at`（無ければ今）を含む期間のレポートを、supervisorと同じ関数で`--out`（無ければ`<queue dir>/reports/`）の下に書き、index.htmlと保持も同じく行い、`period`・`label`・`partial`・`html`・`json`・`index`・`removed`を返す。`--print json`はファイルを書かずにレポートのJSONを返す。queueはread-onlyで開き、`report_written`を記録しない（supervisorの日次の記録は変えない）。ファイルを書くので、observerとheadlessのjob（reviewer）には許さない。
