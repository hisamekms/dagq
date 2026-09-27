---
id: adr-0077
type: adr
title: 当面のtaskの依存図を、dagqが組み立てたd2のソースからhostのd2（TALA）で描き、dagq graphと日次・週次レポートで出す
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
owners:
  - hisamekms
tags:
  - runtime
  - operations
  - observability
related:
  - adr-0040
  - adr-0049
  - adr-0051
  - design-domain-model
---

# ADR-0077: 当面のtaskの依存図を、dagqが組み立てたd2のソースからhostのd2（TALA）で描き、dagq graphと日次・週次レポートで出す

## Context

`dagq graph`（[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定4）は未完了のtaskの依存をJSONで返すだけで、人が「いま何が流れていて、何が何を塞いでいるか」を一目で見る絵が無い。未完了のtaskは数百あり、全部を描くと読めない。

2026-09-26、plannerのsessionで人がd2とTALAのlayoutで当面の依存図を描かせ、左から右へ流れる順・goalの枠・色の意味を確かめた。そのうえで、同じ絵をKPIの日次レポート（[ADR-0051](0051-kpi-time-series-report-and-push.md)の決定20、task 431）に載せ、単体でも出せるようにすると決めた。描画はd2＋TALAを呼ぶこと、出し方はレポートと単体の両方にすることを人が選んだ。

試した中で分かったこと: goalをd2のcontainerにすると、TALAはcontainerの中の配置を自分で決め直し、`top` / `left`で与えた座標を無視して列が崩れる。

## Decision

1. **「当面」の選び方**: 未完了のtaskのうち、次のどれかに当たるものを描く。(i) `in_progress`、(ii) 実効優先度（`effective_priority`）が`interrupt` / `urgent` / `high`、(iii) `graph`の`critical`の鎖に載るtask、(iv) (i)〜(iii)の`ready_after`に残る前提（未完了の依存元のtaskと、閉じていない依存先のgoal）。(iv)は1段だけ足し、前提の前提は辿らない（辿ると結局ほぼ全体になる）。`normal`以下で上のどれにも当たらないtaskは描かない。選ぶ規則は決まった関数で、LLMを使わない。
2. **配置**: 依存の深さを列にし、左から右へ流れる順（前提が左、それを待つtaskが右）に並べる。goalを横長の帯として行に詰め、依存でつながるgoalどうしは近い行に、他のgoalとつながらない単独のgoalは下の行に置く。座標はdagqが計算し、d2の`top` / `left`で固定する。goalの枠はcontainerにせず、taskの箱の背後に置く矩形（と見出し）として描く。goalの見出しは枠の幅で切る（はみ出さない）。
3. **見た目の意味**: 箱の色は4つに分ける: `in_progress`、`interrupt`と`urgent`、`high`、`normal`以下（(iv)で入った前提）。`critical`の鎖の辺は赤の太線にする。依存が残っておらず今すぐ着手できるtaskには印を付ける。色と印の凡例を図に添える。
4. **描画はhostのd2に任せる**: dagqはd2のソースを自分で組み立て、SVGはhostの`d2`を`--layout=tala`で実行して得る（2026-09-26の人の決定）。runtimeは自前でSVGのlayoutを描かない。`d2`とTALAのplugin（`d2plugin-tala`）はsupervisorのPATH（`up`で固定される）から解決する。人がmiseで入れ、`~/.local/bin`にmiseのshimへのlinkを置く（sccacheとcargo-nextestと同じ運用。[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定7）。workerはhostにツールを入れない。
5. **描けないときはSVGを作らず、理由を返す**: `d2`か`d2plugin-tala`が見つからない、実行が失敗する、時間の上限を超えるときは、SVGを作らず理由（どれが見つからないか、終了コードとstderrの要約）を返す。`dagq graph`の描画は失敗として終わる。レポートは依存図の節を「描けなかった理由」に置き換え、KPIなど他の節はそのまま書く（依存図が描けないことでレポート全体を止めない）。d2のソースを出すだけなら`d2`は要らない。
6. **TALAのライセンス**: TALAはTerrastructの商用ライセンスの製品で、d2本体（オープンソース）とは別に配られる。ライセンスの無いTALAも描画はでき、SVGに未ライセンスの透かしが入る。dagqはライセンスの有無を検査せず、透かし入りのSVGもそのまま使う（人が見る社内の図なので、描けないよりよい）。ライセンスのkeyはsecretとしてrepositoryに入れず、`dagq.toml`にも置かない。使うなら人がsupervisorを起動するshellの環境か、hostのファイルに置く。ライセンスの取得と更新は人の判断で、runtimeは関わらない。
7. **出し方**: 単体では`dagq graph --format d2|svg [--out PATH]`で、d2のソースかSVGを出す（`--out`が無ければ標準出力）。既定のJSONの出力は変えない。レポートでは、ADR-0051の決定20の日次・週次レポートのHTMLに「当面の依存図」の節を足し、SVGをHTMLにinlineで埋め込む。ADR-0051の決定20の「外部の資源を読まない」「queueのディレクトリの外に何も送らない」はそのまま守る（d2の実行はhostの中で完結し、SVGは埋め込む）。ADR-0051は書き換えず、この節はこのADRが足す。JSONのレポートには描いたtaskの集合と、描けなかったときの理由を載せる。

色の値、列と行の間隔、時間の上限、eventの名前は実装のtaskが[docs/design/](../design/)に書く。

## Alternatives

- **runtimeが自前でSVGを描く**（Rustで座標を決め、SVGの文字列を書く）: 外部のツールが要らず、どのhostでも描ける。しかし辺の経路決め（箱を避ける線、交差の少ない曲げ）を自前で持つのは大きく、人が試してよいと確かめたTALAの見た目に届かない。座標は自分で決めるが、線の引き回しをd2＋TALAに任せる形にした。
- **goalをd2のcontainerにする**: d2の素直な書き方で、枠と中身の関係がソースに残る。しかしTALAがcontainerの中を並べ直して`top` / `left`を無視し、左から右の列が崩れる（2026-09-26に確かめた）。枠は背後の矩形で描く。
- **d2の既定のlayout（dagre / ELK）を使う**: 無償で入れやすいが、座標の固定が効かないか、列の揃い方が人の確かめた絵と違う。人がTALAを選んだ。
- **未完了のtask全部を描く**: 数百の箱になり、当面の流れが読めない。選ぶ規則（決定1）で絞る。
- **ツールが無ければレポートを失敗にする**: 依存図は補助で、KPIの日次の記録を止める理由にならない。理由に置き換えて他を書く。

## Consequences

- 人は日次レポートを開けば、KPIと並んで当面の流れと塞いでいるtaskを見られる。単体の`dagq graph`でplannerのsessionからも同じ絵を出せる。
- hostに`d2`と`d2plugin-tala`を入れる手間が増える。入っていないhostでもレポートは書け、依存図の節が理由になるだけ。
- TALAが未ライセンスなら図に透かしが入る。ライセンスを買うかは人が決める。
- 座標をdagqが持つので、配置の規則（決定2）を変えるのはdagqの変更になる。TALAの版が変わっても列の並びは崩れにくいが、線の引き回しは変わりうる。
- d2を子プロセスで呼ぶので、supervisorのレポートの周回に外部の実行時間が入る。時間の上限で打ち切る。
