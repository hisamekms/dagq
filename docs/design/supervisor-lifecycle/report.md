---
id: design-supervisor-lifecycle-report
type: design
title: "KPIのレポート（`report`）"
status: current
created: 2026-09-27
updated: 2026-09-29
last_verified: 2026-09-29
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-kpi
  - design-supervisor-lifecycle-observer
  - design-supervisor-lifecycle-dependency-diagram
  - adr-0051
  - adr-0077
---

# KPIのレポート（`report`）

[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)の決定20・21の実装（task 431）と、[ADR-0077](../../adr/0077-near-term-dependency-graph-with-d2-tala.md)の決定7の後半の当面の依存図の節（task 534）。supervisorが日に1回、[`kpi`](kpi.md)の結果から日とISO週のレポートをJSONと自己完結のHTMLでqueueのディレクトリに書き、人は`dagq report`で同じものを手で書く。集計は`dagq kpi`と同じ関数で、LLMもrun slotも使わず、queueのディレクトリの外に何も送らない。JSONは集計の写しで、正はrun_events（消しても`dagq kpi` / `dagq report`で作り直せる）。

## 中身

- **JSON**は`dagq kpi --period <day|week> --at <その期間>`（`--last`は既定の7、期間は古い順で最後がレポートの期間）と同じ形に、`report`（`period`・`label`・`partial`・`generated_at`・`build`（書いたバイナリのbuild識別子））、`findings_open`（`open` / `proposed`のfindingの数）、`findings`（そのうち`findings`の順（影響の大きい順）で先頭10件。どれもレポートを書いた時点の値で、遡って書いた日のレポートでもその日の値ではないの`id`・`kind`・`target`・`subject`・`summary`・`impact`・`status`・`occurrences`・`last_seen_at`）と、`diagram`（[当面の依存図](#当面の依存図)の`tasks`（描いたtaskのID、昇順）・`d2_source`（d2のソースを作ったか）・`reason`（HTMLに図が無いときだけ、その理由）。SVGはJSONに入れない）を足したもの。`domain::kpi::report::Report`。
- **HTML**（`domain::kpi::report::render_html`）は1ファイルで、CSSと小さなグラフ（inline SVG）を埋め込み、script・外部のCSS・font・画像・CDNを読まない（JavaScriptを使わない）。上から: 見出し（期間、区間、終わったrunの数、生成時刻、build、`partial`の印）、目標（`breach` → `missed` → `not_judged` → `ok`の順に、KPI・層・stat・目標・最新の値・状態・連続・始まり・設定の出どころ）、推移（`all`の層に値のあるKPIごとに（`landings`・`lead_time`・`phase.*`・手戻りの率・`slot_usage`・`landing_utilization`・`landing_utilization.peak`・`landing_attempt`・`landing_queue_depth`・`asks_per_landing`・`ask_wait`・`max_load_avg`・`cpu_per_landing`・`load_per_core`などを先に、他は名前の順）、並べた期間の棒グラフ、最新・前・差・判定。印のある期間の上に三角、`partial`の期間は薄い棒）、変更の印の一覧、見込みの誤差（ADR-0070の決定5。レポートの期間に終わった対象の答え合わせ（[kpi](kpi.md#完了見込みの答え合わせ)）の標本数・印のある標本数・除いた行の数と、層（`all` → `target=` → `kind=` → `change=` → `band=` → `marks=` → `method=`）ごとの`n`・p50の誤差の中央値（符号つき）・絶対値の中央値・残り時間に対する比の中央値・p90の的中率・遅れと早いの割合・偏り（`late_rate`と`early_rate`の大きい方、同じなら`even`）・`forecast.*`の目標の最も悪い状態（`breach` → `missed` → `not_judged` → `ok`）。標本が0なら表を出さない）、着地slot（「Integration slot」。goal 72、task 991。レポートの期間の`details.landing_utilization`から、試行が占めた時間と期間の長さと使用率、試行の数と着地した数、最も混んだ1時間の始まりと使用率、順番待ちのrunの数の平均と最大）、hostのCPU（「Host CPU」。goal 72、task 992。hostの記録を読んだときだけ。`details.cpu_per_landing`と`kpis`から、期間のCPU秒・着地数・着地1件あたりのCPU秒とプロセスの種類ごとの内訳、`load1` ÷ コア数の中央値・p90・最大。記録が無ければそう書く）、レポートの期間のKPIの表（`all`の層の`n`・`value`・`median`・`p90`・前・差・7日の基準・判定と、`<details>`に他の層（`kind=`、taskが宣言した`change=`（ADR-t980-1、task 982。[kpi](kpi.md#kpiと層)）、`[areas]`のあるqueueでは`area=`（[kpi](kpi.md#area)）、claimの属性の層））、open なfindingの上位、当面の依存図（「Near-term dependencies」。図か、描けなかった理由）、記録の無いKPI（`unavailable`）。値は秒のKPIを`1h 02m`の形、割合を`%`（差は`pt`）で出す。文字列はHTMLのescapeをする。
- HTMLに外部の資源を読むものが無いことはtestで検査する（依存図のSVGを含めて。`domain::kpi::report::external_references`と、SVGの外の部分に`url(`・`@font-face`・`http://`などの文字列が無いこと）。
- `index.html`（`index_html`）は日・週のレポートへの相対リンクを新しい順に並べ、書くたびに書き直す。

## 当面の依存図

- レポートを作る関数（`application::report::make`。supervisorの日次と`dagq report`が同じものを使う）が、`dagq graph --format svg`と同じ関数で当面の依存図を描く: queueの今の`graph`から`application::diagram::near_term`でtaskを選んで配置し、`Diagram::to_d2`のソースを`ReportSetup::diagram`（`compose`が`infrastructure::d2::render_svg`をPATHと`RENDER_TIMEOUT`（60秒）で包んだもの）に渡す（[当面の依存図](dependency-diagram.md)）。PATHは`dagq report`ではそのプロセスの、supervisorでは`up`で固定されたsupervisorのもの（testは`SuperviseOptions::diagram_path`で差し替える）。
- 図はレポートを書いた時点のqueueの姿で、遡って書いた日・週のレポートでもその期間の姿ではない。supervisorの`write_due`は1回の周回で書くレポート（最大で7日と1週）に同じ図を使い、d2は周回ごとに1回だけ実行する。
- SVGは`domain::kpi::report::inline_svg`でHTMLにinlineで埋め込める形にする。読むのはmarkup（tagと`<style>`の中身）だけで、textは読まない（taskのtitleに`url(`や`@import`が含まれていても、labelの文字とその後のmarkupはそのまま）: 最初の`<svg`から最後の`</svg>`までを取り（XMLの宣言を落とす）、次の要素を終わりのtagまで（終わりのtagが無ければ始まりのtagだけ）消す: 外から読み込む・実行する・他へ導く要素（`script`・`link`・`img`・`iframe`・`frame`・`object`・`embed`・`audio`・`video`・`source`・`track`・`portal`・`base`）、属性を時間で変えるSMILの要素（`set`・`animate`・`animateMotion`・`animateTransform`）、すべての`meta`（refreshで他へ導きうる。図には要らない）。tagの属性はHTMLと同じく読み（`=`の前後の空白、引用符の無い値、`/`での区切り、前の値の直後に続く名前も）、`xmlns`と`xmlns:*`を消し（HTMLはsvgとxlinkの名前空間を知っている）、`href`・`xlink:href`・`src`・`srcset`・`action`・`formaction`・`poster`・`background`・`data`で値が`#id`でも`data:`でもないものと、値に`image-set(`を含むものを消し、残る値の中の外を指す`url(...)`（`)`の欠けたものを含む）を値ごとに`none`に置き換える。`<style>`の中身からは`@import`の規則を消し、外を指す`url(...)`を`none`に置き換える。残るのはSVGの中を指す`#id`と、中身を運ぶ`data:`（d2が埋め込むfontなど）だけ。そのあと`external_references`で外を指すもの（上の要素（`meta`は`http-equiv`を持つものだけ）、上の属性、外を指すか`)`の欠けた`url(`、`<style>`に残る`@import`か`image-set(`）が残っていれば、図を載せずに理由にする。
- 描けないとき（選んだtaskが無い、`d2`か`d2plugin-tala`がPATHに無い、d2の失敗・時間切れ・SVGを書かない、SVGが外を指す、queueを読めない）は、図の節を「Not drawn: <理由>」に置き換え、KPIなど他の節はそのまま書く（レポート全体は失敗にしない）。理由はd2の側では`render_svg`のもの（見つからないtool、終了コードとstderrの末尾など）。

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
- 同じjobが、レポートを書いた後に目標割れの始まりと解消を記録し、host.tomlの`[push]`があればpushのメッセージを作る（[push](push.md)。`write_due`は書いたレポートと一緒に作った`Report`を返す）。pushの失敗はレポートの記録を戻さない。
- observerの「自分以外のeventが無ければ起動しない」の判定（`events_besides`）は`report_written`を数えない（ADR-0051の決定24）。

## `dagq report`

`dagq report [--period day|week] [--at <cursor>] [--out <dir>] [--print json]`（`compose::OneShot::report_of`）。`--at`（無ければ今）を含む期間のレポートを、supervisorと同じ関数で`--out`（無ければ`<queue dir>/reports/`）の下に書き、index.htmlと保持も同じく行い、`period`・`label`・`partial`・`html`・`json`・`index`・`removed`を返す。`--print json`はファイルを書かずにレポートのJSONを返す。queueはread-onlyで開き、`report_written`を記録しない（supervisorの日次の記録は変えない）。ファイルを書くので、observerとheadlessのjob（reviewer）には許さない。
