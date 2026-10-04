---
id: adr-t1639-2
type: adr
title: goalの残りのtaskとfollow-upを元のgoalの受け入れ条件の達成に要るかで判定し、要るものは元のgoalに残してgoalの優先度を継ぎ、要らない改善は同じラベルのテーマごとの受け皿のgoal（優先度low、ラベルごとに1つ）に移し、元のgoalが閉じた後の後回しのfollow-upも受け皿へ移し、移しても採用・優先にせず、判定と移動はplannerの手順にする（ADR-t1504-1決定3をamends）
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-t1504-1 decision 3
owners:
  - hisamekms
tags:
  - planner
  - follow-up
  - goal
  - priority
related:
  - adr-t1639-1
  - adr-t1504-1
  - adr-t1504-2
  - adr-t808-1
  - adr-0009
  - adr-0047
  - design-supervisor-lifecycle-draft-planners
  - design-supervisor-lifecycle-goal-review
---

# ADR-t1639-2: 受け入れ条件に要らない後回しの改善を、同じラベルのテーマごとの受け皿のgoal（low）に集める（ADR-t1504-1決定3をamends）

## Context

[ADR-t1639-1](2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)でtaskの優先度は所属のgoalから継ぐ。すると`high`以上のgoalに残った、受け入れ条件には要らない後回しの改善（2026-10-04の時点でgoal 87のfollow-upの1563・1564・1565・1576・1577・1585など）は、goalの優先度で走り、goalも閉じない。2026-10-04に人がrequest 11で、後回しにするtaskの判定が要ると決め、受け入れ条件に要る修正は元のgoalに、要らない改善はテーマごとの受け皿の`low`のgoal（同じラベル）に入れる提案を採用した。

follow-upの所属の基準は[ADR-t1504-1](2026-10-04-t1504-1-follow-ups-belong-to-the-goal-whose-acceptance-needs-them.md)決定1が、所属の判断の記録・移動・人の`adopt`の上限は[ADR-t1504-2](2026-10-04-t1504-2-runtime-records-and-enforces-follow-up-membership-judgements.md)が決めている。このADRは同じ基準と記録の仕組みを使い、範囲外の置き先の選び方（ADR-t1504-1決定3）に受け皿のgoalの規則を足す。

## Decision

1. **goalの残りのtaskとfollow-upを、元のgoalの受け入れ条件の達成に要るかで判定する。** 基準はADR-t1504-1決定1と同じ（実施しなくても元のgoalのacceptanceを満たしたと言えるか）で、影響の大小やpriorityでは決めない。要るものは元のgoalに残し、goalの優先度を継ぐ（ADR-t1639-1決定2）。要らない改善は元のgoalから出し、同じラベルのテーマごとの受け皿のgoalに移す。受け皿のgoalの優先度は`low`にする。移すとき、taskの個別の優先度の指定（ADR-t1639-1決定2・5）は外して受け皿の`low`を継がせ、残すときは理由をtaskに書く（個別の指定が残ると後回しの改善が元の優先度で走るため）。移せるのはADR-0009の付け替えの規則（`draft` / `ready`のtaskだけ）の範囲で、`draft`・`ready`以外の閉じていない状態（`submitted`と`in_progress`以降）のtaskは元のgoalに残り、そのgoalの閉鎖を待たせる（follow-upはADR-t1504-2決定6のとおり判断だけを記録する）。
2. **goalごとに積み残しのgoalを作らず、受け皿はラベルごとに1つを基本にし、無ければ作る。** ADR-t1504-1決定3の「既存の適切なgoalを先に探し、無ければ作る」の範囲外の置き先の選び方に、次を足す: 範囲外の改善で、その改善に固有の適切な既存のgoalが無いときの置き先は、元のgoalと同じラベルの受け皿のgoalにする。受け皿はラベルで束ねたテーマの範囲に限るので、決定3の「無関係な大きな保守のgoalにまとめて詰めない」と両立する（ラベルの無いgoalのものは今までどおり決定3で選ぶ）。元のgoalごとの積み残しのgoalは、閉じないgoalを増やすだけなので作らない。受け皿は閉じずにテーマの後回しの改善を受け続けるgoalになりうるが、ラベルごとに1つに限るので数が増えず、`low`で他のgoalの枠を取らず、他のgoalの受け入れ条件に要るものを持たない（要るものは元のgoalに残る）ので、閉じないことで他のgoalを止めない。ADR-t1504-1決定3が避けた「閉じないgoal」は、受け入れ条件に要る作業と要らない改善が混ざって元のgoalが閉じないことで、受け皿はその混ざりを元のgoalから出す置き先である。
3. **元のgoalが閉じた後に届いた後回しのfollow-upも、同じラベルの受け皿へ移す。** 複数のラベルを持つgoalのものは、plannerが主なラベルを選んでその受け皿へ移す。人の`adopt`の要否は移しても変わらない: 登録時に元のgoalが閉じていたfollow_upは、受け皿へ移しても人の`adopt`が要る（[ADR-t808-1](2026-09-28-t808-1-runtime-planners-submit-follow-ups-up-to-depth-two.md)決定2の閉じたgoalのfollow_upの人の`adopt`を、ADR-t1504-2決定5が登録時の事実で決め、`set-goal`による迂回を塞いだもの。受け皿への移動もこの迂回に当たらない）。元のgoalが`achieved`で閉じた後の判断で要ると分かったもの（範囲外からの訂正を含む）は受け皿へ移さず、ADR-t1504-2決定9の訂正の経路に乗せる。
4. **受け皿へ移しても採用・優先にはしない。** ADR-t1504-1決定2の所属・採用・priorityの分離のまま、採否は今までどおりruntimeのplannerが決め、不要なものは根拠を残してcancelできる。follow-upの所属の判断の記録はADR-t1504-2の仕組み（判断の行・所属先のgoal・acceptanceの版、判断と同じtransactionの移動）を使い、受け皿のための別の記録は作らない。follow-upでない（plannerが直接登録した）taskはADR-t1504-2の記録の対象でないので、今までどおり`set-goal`で移し、判定の根拠（該当しないacceptanceの項目と理由）はplannerの計画の記録（proposalやtaskの`context`）に残す。移しても出どころと深さは残る（ADR-t1504-2決定4）。
5. **判定と移動はplannerの手順にし、runtimeは自動で移さない。** 受け皿の行き先は元のgoalのラベルが複数あると一意に決まらず、要るか要らないかは意味の判断で、ADR-t1504-1決定5のとおりruntimeは判定しない。手順（判定の問い、受け皿の探し方と作り方、主なラベルの選び方）は[ADR-t1453-2](2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)決定1に従いpluginのskillが持ち、このrepositoryのラベルの語彙は`dagq.toml`と開発文書が持つ。

## Alternatives

- **元のgoalごとに積み残しのgoalを作る**: 積み残しのgoalがgoalの数だけ増え、どれも閉じない。テーマで束ねた受け皿なら、同じテーマの後回しの改善を1か所で見比べて採否を決められる。
- **1つの大きな保守のgoalに集める**: ADR-t1504-1の退けた案と同じで、閉じなくなり、判断が「とりあえず保守へ」に流れる。
- **受け皿の優先度を元のgoalから継ぐ**: 後回しの改善が元のgoalの優先度で走り、この判定の目的（`high`以上のgoalの枠を受け入れ条件に要る作業に使う）が果たせない。
- **runtimeが閉じたgoalのfollow-upを受け皿へ自動で移す**: ラベルが複数あると行き先が決まらず、要るか要らないかの意味の判断をruntimeが持つことになる。
- **受け皿への移動に別の記録を作る**: ADR-t1504-2の所属の判断の記録と二重になり、どちらが今の判断かがずれる。

## Consequences

- `high`以上のgoalには受け入れ条件に要る作業と、移せない状態（`draft`・`ready`以外の閉じていない状態）のtaskだけが残り、goalが閉じやすくなる。後回しの改善はラベルごとの`low`の受け皿に集まり、人がgoal listの`--tag`で見られる。
- 受け皿のgoalのacceptanceの書き方と、受け皿の改善を優先して取り出すときの手順はpluginのskillが持つ（このADRは決めない）。
- 既存の`high`以上のgoalに残るtaskの判定は、goal 106の棚卸しのtaskが根拠と受け皿つきの案にし、適用は権限のあるplannerか人が行う。
