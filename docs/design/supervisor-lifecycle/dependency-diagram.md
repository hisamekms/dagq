---
id: design-supervisor-lifecycle-dependency-diagram
type: design
title: "当面の依存図（`graph --format d2|svg`）"
status: current
created: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-domain-model
  - design-supervisor-lifecycle-doctor
  - design-supervisor-lifecycle-report
  - adr-0077
---

# 当面の依存図（`graph --format d2|svg`）

[ADR-0077](../../adr/0077-near-term-dependency-graph-with-d2-tala.md)の決定1〜5・7の単体の出し方の実装（task 533）。`dagq graph`の結果（[Domain model](../domain-model.md)の`graph`）から当面のtaskを選び、座標を固定したd2のソースを組み立て、SVGはhostの`d2 --layout=tala`で描く。日次・週次レポートへの埋め込み（決定7の後半、task 534）は[report](report.md#当面の依存図)に書く（同じ`near_term`と`render_svg`を使う）。

## コマンド

`dagq graph [--goal ID] [--format json|d2|svg] [--out PATH]`。既定の`json`は依存の見取り図で、`candidates`はsupervisorが記録した控えのtaskを`deferred`に分けたもの（[claimを控える](claim-defer.md)）。
図は控えを分けない`DependencyGraph`から描く。
`d2`はd2のソースを、`svg`はSVGを標準出力にそのまま書く（JSONで包まない。`main.rs`の`RAW_STDOUT`）。`--out`があればそのファイルに書き、`{"format", "out", "tasks"}`（`tasks`は描いたtaskのID）のJSONを返す。`--format json`に`--out`は付けられない。`--goal`は、`json`では`graph`と同じく`tasks`・`candidates`・`critical`の起点を絞る。`d2` / `svg`では選ぶ起点だけをgoalに絞り、goalの外の前提と`critical`の鎖も描く（下の選び方。task 764）。queueの状態を変えず、queueはread-onlyで開く。observerとheadlessのjob（`DAGQ_ROLE=observer` / `review-job`などのjobのrole）は`--out`の無い`graph`だけを打てる（`--out`はファイルを書くので許可の一覧から外す）。

## 選び方（`application::diagram::select`）

`DependencyGraph`の`tasks`のうち、(i) `in_progress`、(ii) `effective_priority`が`high`以上、(iii) `critical`の鎖に載る、のどれかに当たるtaskと、それらの`ready_after`（1段だけ）: 未完了のtask、と`{"goal": ID}`のgoalの未完了のtask全部。前提の前提は辿らない。理由（`in_progress` / `priority` / `critical` / `prerequisite`）はこの順で最初に当たったもの。

`--goal G`の`d2` / `svg`（`near_term_in_goal`、`select_in`）: (i)・(ii)はGのtaskに絞り、(iii)は`graph --goal G`の`critical`の鎖（Gのtaskから始まり、Gの外へ続きうる）の全部のtask、(iv)の前提は絞らない`graph`（`dependency_graph(..., None)`）の全部のtaskから引く。Gの外のtaskは自分のgoalの帯・枠に置く（goalの無いtaskは`no goal`の帯）。`critical`の鎖をGの外まで描くのは、ADR-0077の決定1の(iii)が「`graph`の`critical`の鎖に載るtask」で、`--goal`の`graph`の`critical`がGの外まで続く鎖だから。鎖の外に出たtaskの前提も(iv)として1段だけ描く。Gの外のtaskは`in_progress`や`high`でも(i)・(ii)の起点にしない（鎖か前提として入ったときだけ描き、理由は`critical` / `prerequisite`、色は`Tone`のまま）。着手可の印は絞らない`graph`の`candidates`で付ける。`--goal`の無いときは`select`と同じ。

## 配置（`application::diagram::layout`）

純粋な関数で、同じ`DependencyGraph`とgoalの題から同じ結果になる（`BTreeMap`の順）。

- **列**: 描いたtaskの間の前提（`ready_after`のtaskと、`ready_after`のgoalの描いたtask）の最長の鎖の長さ（`depths`）。前提が左。
- **帯**: goalごと（goalの無いtaskは`no goal`の帯）。帯の中は列ごとにtaskをIDの順に行へ積み、帯の高さは最も多い列の行数で決まる。
- **帯の順**: 描いた前提でgoalどうしをつないだ連結成分のうち、2つ以上のgoalのものを先に（成分の中は最初の列、次にgoalのID）、他とつながらない帯を後に並べる。
- **lane**: 帯は順に、直前の帯と同じlaneの右に置けるなら（最初の列が、そのlaneの最後の帯の最後の列より右）そこへ、置けなければ下に新しいlaneを開く。
- **座標**（px）: 列の間隔`COLUMN_WIDTH` 300、箱`NODE_WIDTH` 240 × `NODE_HEIGHT` 64、行の間隔`ROW_HEIGHT` 88、枠の見出し`HEADER_HEIGHT` 36、枠の余白`FRAME_PADDING` 16、laneの間`LANE_GAP` 40、外周`MARGIN` 20。
- **見出しと題**: 枠の見出し`goal ID: 題`は枠の幅に、箱の題は箱の幅に切り、切ったら`…`で終える（`fit`。ASCIIを1、U+1100以上を2の幅とし、1の幅を8px）。

## d2のソース（`Diagram::to_d2`）

枠（`goal_<ID>` / `goal_none`。containerにせず、`label.near: top-left`の見出しを持つ矩形）を先に書いて箱の背後にし、次に箱（`t<ID>`、label `#ID`（Spikeは`#ID spike`）と題の2行）、辺、凡例（`legend_0`〜`legend_3`と`legend_note_0` / `legend_note_1`。凡例と`no goal`の見出しは英語の固定の文字列で、taskとgoalの題は利用者が書いたまま）を書く。どの形も`top` / `left`（枠と箱は`width` / `height`も）で固定する。文字列はd2のdouble-quoteで、`"`・`\`・`$`をescapeし、制御文字を落とす。

- **色**（`Tone`、fill / stroke）: `in_progress` `#dbeafe` / `#1d4ed8`、`interrupt`と`urgent` `#fee2e2` / `#b91c1c`、`high` `#fef3c7` / `#b45309`、`normal`以下 `#f3f4f6` / `#6b7280`。判定は`in_progress`を先に、次に`effective_priority`。枠は`#fafafa` / `#9ca3af`の破線。
- **辺**: taskの前提は`t<前> -> t<後>`、goalの前提は`goal_<ID> -> t<後>`。`critical`の鎖の隣り合う2つを結ぶ辺（goalの辺は、そのgoalの描いたtaskから鎖が続くとき）は`#dc2626`の太さ4、他は`#6b7280`の太さ1。
- **着手可**: `candidates`に入るtaskは題の前に`▶ `を付け、二重枠にする。

## 描画（`infrastructure::d2`）

`d2`と`d2plugin-tala`を`dagq`のプロセスのPATHから探し（実行できるファイル）、どちらかが無ければ描かずに`cannot draw the dependency diagram: d2 and d2plugin-tala not found on PATH ...`の形で失敗する。あれば`d2 --layout=tala - -`を自分のprocess groupで起動してd2のソースを標準入力に渡し、標準出力のSVGを受ける。`RENDER_TIMEOUT`（60秒）を過ぎたとき、または待てなかったときはgroupをkillして`did not finish within 60s`の形、0でない終了は終了コードとstderrの末尾2000文字、`<svg`を含まない出力は`wrote no SVG`で失敗する。どれもSVGを出さない。TALAのライセンスは検査しない（未ライセンスなら透かし入りのSVGのまま）。

`doctor`は`d2`の欄に、同じPATHでの`d2`と`tala`の解決（`path`と、linkならその先の`resolved`。無ければnull）と、無いものの`error`を出す（[doctor](doctor.md)）。

## test

配置・選び方（`--goal`の`select_in` / `near_term_in_goal`を含む）・d2の生成は`application::diagram`のunit test、描画の失敗の経路は`infrastructure::d2`のunit test（stubの`d2`）、CLIは`tests/it/cli_graph.rs`（PATHをstubの`d2` / `d2plugin-tala`の入ったディレクトリとsystemのものだけにして、d2・svg・`--out`・ツールの無いとき・失敗・`doctor`と、goalをまたぐ前提を持つqueueでの`--goal`のd2を確かめる）。本物のTALAを使う描画は自動化しない。
