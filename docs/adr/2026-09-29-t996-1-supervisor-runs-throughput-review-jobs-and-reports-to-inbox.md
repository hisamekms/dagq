---
id: adr-t996-1
type: adr
title: supervisorが毎時・日次・週次のスループットの見直しをheadlessのjobで行い、結果をinboxに知らせるだけのattentionで届ける（ADR-0047決定1・2・17をamends）
status: accepted
created: 2026-09-29
updated: 2026-10-03
accepted_on: 2026-09-29
amends:
  - adr-0047 decision 1
  - adr-0047 decision 2
  - adr-0047 decision 17
amended_by:
  - adr-t1418-1
  - adr-t1172-1
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - kpi
  - operations
related:
  - adr-0047
  - adr-0051
  - design-supervisor-lifecycle-observer
  - design-supervisor-lifecycle-report
  - design-supervisor-lifecycle-auto-update
  - design-supervisor-lifecycle-kpi
---

# ADR-t996-1: supervisorが毎時・日次・週次のスループットの見直しをheadlessのjobで行い、結果をinboxに知らせるだけのattentionで届ける（ADR-0047決定1・2・17をamends）

## Context

goal 72で、スループットの制約（着地の直列処理とCPU）を常時見る数値（`landing_utilization`・`cpu_per_landing`）と、pluginのdagq skillの`reference/kpi.md`の毎週の見直しの手順（制約の特定 → 枠の時間のパレート → 外れ値 → 1つだけ手を打ち印を打つ → 層をそろえた前後比較）ができた。手順の「Cadence」は、毎時は止まっていないかだけ、日次は外れ値と目標割れ、週次は全部、と頻度ごとに見るものを分けている。

ところが手順を回す者が決まっていない。日次レポート（[ADR-0051](0051-kpi-time-series-report-and-push.md)）は数字を書くだけで理由を読まず、observer（[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)がADR-0044から引き継いだもの）は問題をfindingにするjobで、着地数の増減の理由を人に説明しない。plannerはオンデマンドで、常に開いているのはinboxだけである。2026-09-28に人は「hourlyの分析はinboxで見たい。日次・週次も自動化したい」と決めた。runtimeの版が着地するまで、inboxがCronCreateで同じ手順を回している（host側の`PROCEDURE.md`）。この橋渡しはinboxのsessionのcontextを毎時消費し、`/clear`や再起動で止まる。

## Decision

1. **supervisorがtimerでスループットの見直しのheadlessのjobを起動する。** observerと同じく、cmux workspaceを持たない`claude -p`で、MCPを読まない。頻度は毎時・日次・週次の3つ。supervisorが居ないときは動かない。
2. **毎時はruntimeが規則で判定し、当たったときだけjobを起動する。** runtimeが直近の確定した1時間の着地数と移動平均を集計し、次のどれかに当たったときだけjobに理由を分析させる。平常時はagentを起動しない。
   - 直近の確定した1時間の着地数が6時間の平均から±50%以上、かつ3件以上ずれた
   - 3時間の平均が24時間の平均を30%以上下回る状態が3時間続いた
   - 着地が0件の時間があった

   日次と週次は、平常かどうかに関わらず毎回jobを起動する。
3. **結果はqueueのdirの`reports/`の下に残し、inbox宛ての知らせるだけのattentionで届ける。** `update_installed`と同じく、人に知らせるだけで答えを求めないattentionにする（askにしない）。inboxが結論を人に見せる。
4. **jobは状態を変えない。** review / 復旧 / plan review / goal reviewのjobと同じく読むコマンドだけを許すroleで動き（`kpi`・`stats`・`timeline`・`events`など）、結論はデータとして返し、supervisorが保存と知らせを行う。週次の見直しの「次の一手」は、jobがその出力に載せてよく、supervisorがそれをproposalを求める印の付いたfindingとして記録する（ADR-0047決定19の経路(a)と同じ印）。findingはruntimeのplannerの既存の経路（finding → planner → plan review）でproposalになる。jobが自分でfinding・task・goal・印（`mark`）を書くことはない。
5. **見直しの手順はpluginのdagq skillの`reference/kpi.md`の毎週の見直しと同じものを使う。** runtimeのpromptは手順を二重に持たず、頻度ごとにその「Cadence」の範囲を当てる。判定の閾値・起動の時刻・保存のファイル名・eventとattentionの名前・欄名は`docs/design`に書く（上の2の数値は2026-09-28に人が決めた初期値で、変えるのはdesignの変更として扱う）。

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の3つの決定を改める。決定1のsupervisorが起動するjobと決定2の「jobは次の5種類」に、6つ目としてこのスループットの見直しのjobを足す（役割は増やさない。supervisorが起動するheadlessのjobの1つ）。決定17の「inboxに届くものは人の操作が要るもの」に、人に知らせるだけで操作を求めないattentionとしてこのjobの結論を足す（`update_installed`と同じ扱い）。askの人が要る理由（決定41）とその他の決定は変えない。

## Alternatives

- **inboxがcronで回し続ける（今の橋渡し）**: 常駐のinboxのcontextを毎時使い、`/clear`・再起動・compactionで止まる。inboxは人に届くものの窓口で、分析の担い手にすると本来の応答が遅れる。
- **observerに見直しを足す**: observerはfindingを記録するjobで、平常時の日次・週次の説明や毎時の増減の理由は役割が違う。起動の条件（自分以外のeventが無ければ起動しない）も合わない。
- **毎時も毎回jobを起動する**: 平常時の1時間ごとの増減は雑音で、手順自身が「1時間の増減に反応しない」としている。24回/日のagentの起動はhostの負荷とcostに見合わない。
- **askで届ける**: 人が選ぶものが無い。答えを待つaskが積もるとinboxの一覧が埋まる（ADR-0047決定41の人が要る理由にも当たらない）。
- **dagqのreportに規則の分析だけを足す（agentを使わない）**: 増減の理由（どのrunが長かったか、何が詰まったか）を読むには`timeline`とrunの中身の解釈が要り、規則では書ききれない。

## Consequences

- inboxのCronCreateの橋渡しは、runtimeの版が着地して本番で動いたことを確かめてから止める。
- 毎時の判定はruntimeの集計だけなので平常時のcostはほぼ無い。当たった時間と日次・週次にだけagentの時間とhostの負荷がかかる。
- 週次のfindingは`[kpi] max_improvement_proposals`（ADR-0051）の上限に従うので、改善のproposalが増えすぎない。
- 閾値・時刻・欄名・event名は[Report](../design/supervisor-lifecycle/report.md)・[KPI](../design/supervisor-lifecycle/kpi.md)の並びに置くdesignの文書が持つ。どれもこのADRの時点では未実装。
