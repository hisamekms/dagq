---
id: adr-t1172-1
type: adr
title: 毎時のスループットの見直しは規則に関わらず毎時agentで行い、規則の判定はレポートで強調し、inboxには規則に当たった時間だけ知らせる（ADR-t996-1決定2・3をamends）
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
amends:
  - adr-t996-1 decision 2
  - adr-t996-1 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - kpi
  - operations
related:
  - adr-t996-1
  - adr-t1418-1
  - adr-t1942-1
  - design-supervisor-lifecycle-throughput-review
  - design-supervisor-lifecycle-events-watch
---

# ADR-t1172-1: 毎時のスループットの見直しは規則に関わらず毎時agentで行い、規則の判定はレポートで強調し、inboxには規則に当たった時間だけ知らせる（ADR-t996-1決定2・3をamends）

## Context

[ADR-t996-1](2026-09-29-t996-1-supervisor-runs-throughput-review-jobs-and-reports-to-inbox.md)決定2は、毎時の見直しをruntimeの規則（6時間の平均からのずれ・続く低下・着地の無い時間）に当たった時間だけagentで行い、平常の時間はagentを起動しないと決めた。
そのため2026-09-30は02時・03時台だけがレポートになり、04〜07時台は`reports/reviews/`に何も残らなかった。
平常の時間に何が起きていたかを後から読む材料が無い。

2026-09-30に人は、平常の時間もagentが分析することを決めた。
「平常は runtime の要約だけ」の案と比べて選び、毎時のheadless jobの費用とhostの負荷が増えることは承知している。

ADR-t996-1決定3は、見直しの結果を`reports/`に残して知らせるだけのattentionでinboxに届けると決めた。
平常の毎時までattentionにすると1日24件でinboxの一覧が埋まる。
dagq skillの`reference/kpi.md`の「Cadence」は1時間の増減に反応しないとしているので、人に届けるのは規則に当たった時間で足りる。

ADR-t996-1は番号付きの決定を5つ持ち、変えるのは決定2と3だけなので、[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)に従ってamendsで直す。

## Decision

1. **(a) 毎時の見直しは、規則の判定に関わらず毎回agentを起動し、レポートを残す。** 平常の時間もagentが見直し、日次・週次と同じく`reports/reviews/`にレポートを置く。
2. **(b) 規則の判定は捨てずにjobの入力と記録に残し、当たった規則をレポートの冒頭で強調する。** 判定は起動を決めず、レポートの書き方と知らせを決める。当たった時間は当たった規則を結論の冒頭に挙げて理由を分析し、当たらなかった時間はCadenceのとおり短く、何が平常で、続けば気にすべき兆しがあるかを書く。手順はpromptに二重に持たない（ADR-t996-1決定5のまま）。
3. **(c) inboxに知らせるだけのattentionは、規則に当たった毎時と、日次・週次の見直しだけにする。** 平常の毎時の結論は、eventと`reports/reviews/`に残すだけでattentionにしない。見直しが結論に届かなかった知らせは、今までどおり頻度と規則を問わずattentionにする。規則に当たった毎時の結論がinboxのwatchを単独では起こさないこと（[ADR-t1418-1](2026-10-03-t1418-1-quiet-notices-do-not-wake-the-inbox-watch.md)）は変えない。
4. **(d) 平常の毎時をやめる設定は設けない。** 毎時に起動しない経路は無くす。人が毎時の分析を決めたので、既定と違う経路を残すとtestと文書が2通りになる。費用が見合わなければ、導入の前後比較を材料に人が決め直し、別のtaskで戻す。見直しを止めるだけなら`supervise --throughput-review false`がある。

ADR-t996-1決定2の「当たったときだけjobを起動する」「平常時はagentを起動しない」を1と4に、規則とその閾値はそのまま判定として2に改める。
決定3の「結果をinbox宛ての知らせるだけのattentionで届ける」を、毎時については規則に当たった時間だけに3で改める。
判定の閾値（2026-09-28に人が決めた初期値）とjobのprovider・modelは変えない。
閾値の値・欄名・eventの形は定義のそばのコードのdoc commentが持ち、流れと不変条件は[スループットの見直し](../design/supervisor-lifecycle/throughput-review.md)と[events / watch](../design/supervisor-lifecycle/events-watch.md)が持つ（[ADR-t1942-1](2026-10-07-t1942-1-design-docs-in-four-layers-with-size-budgets.md)）。

## Alternatives

- **平常は runtime の要約だけ（agentを使わない）**: 費用は小さいが、平常の時間に長かったrunや待ちの兆しを読むには`timeline`とrunの中身の解釈が要り、規則と数値の要約では書ききれない。人はこの案と比べて毎時のagentを選んだ。
- **平常も attention で知らせる**: 1日24件の知らせでinboxの一覧が埋まり、規則に当たった時間の知らせが埋もれる。Cadenceのとおり1時間の増減には反応しないので、平常の時間を人に届けても打つ手が無い。
- **設定で規則だけの起動に戻せるようにする**: 既定と違う経路を残すと、testと文書が2通りになり、どちらが本番の姿かが読みにくくなる。戻すかどうかは前後比較の後に人が決めることで、決めたときに別のtaskで戻せばよい。止めるだけなら既存の`supervise --throughput-review false`で足りる。

## Consequences

- 毎時のagentの起動は1日24回になり、見直しのjobの時間とhostの負荷が増える。導入の前後比較（見直しのjobの件数・失敗率・所要時間、hostのload、着地）は着地と自動更新の後にplannerかinboxが行う。
- 平常の毎時の見直しが失敗した時間も`check the failed review`でinboxに届くので、失敗の知らせは平常の時間の分だけ増えうる。
- 平常の毎時の結論はattentionにならないので、`attentions_per_landing`などattentionを数えるKPIには入らない。
- 過去の記録に残る平常の毎時の`skipped`の終わりは、今までどおりattentionではない。
