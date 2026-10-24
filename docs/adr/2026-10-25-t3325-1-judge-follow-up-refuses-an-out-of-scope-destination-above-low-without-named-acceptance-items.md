---
id: adr-t3325-1
type: adr
title: judge-follow-upは、out_of_scopeの移動先のgoalの優先度がlowより高いとき、移動先の受け入れ条件のうちそのfollow_upを要る項目の名指しを求め、無ければ同じラベルの受け皿へ案内して拒む。runtimeは名指しの有無と優先度だけを機械で検査し、意味の判定と自動の移動はしない（ADR-t1639-2決定5をamends）
status: accepted
created: 2026-10-25
updated: 2026-10-25
accepted_on: 2026-10-25
amends:
  - adr-t1639-2 decision 5
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - follow-up
  - goal
  - priority
related:
  - adr-t1639-2
  - adr-t1504-1
  - adr-t1504-2
  - adr-t1639-1
  - design-follow-up-membership
  - design-persistence
---

# ADR-t3325-1: out_of_scopeの移動先がlowより高いgoalなら、移動先の受け入れ条件の項目の名指しを求める（ADR-t1639-2決定5をamends）

## Context

[ADR-t1639-2](2026-10-04-t1639-2-defer-improvements-outside-acceptance-to-a-low-goal-per-tag.md)は、受け入れ条件に要らない改善を同じラベルの受け皿のgoal（`low`）に集め、決定5で判定と移動をplannerの手順にし、runtimeは自動で移さないと決めた。
pluginの登録の手順は、受け入れ条件がそのfollow_upを要らず優先度が受け皿より高い候補は受け皿に負けると書いている。

それでもplan reviewは同じ理由でfollow_upの所属を差し戻し続けた（2026-10-18〜22に4回）。
どれもout_of_scopeの判定は妥当で、移動先に受け入れ条件がそれを要らない`normal`〜`high`の既存のgoalを選び、`low`の受け皿へ直された。
文書の規則は既にあり、文書を足しても再発は止まらない。
所属の判断（`judge-follow-up`）は移動と同じtransactionで記録されるので（[ADR-t1504-2](2026-10-04-t1504-2-runtime-records-and-enforces-follow-up-membership-judgements.md)）、記録の時に止めればsubmitとplan reviewの前に直せる。

## Decision

1. **lowより高い移動先には、移動先の受け入れ条件の項目の名指しを求める。**
   out_of_scopeの判断で、移動先のgoalの優先度が`low`より高いとき、判断する者は移動先の受け入れ条件のうちそのfollow_upを要る項目を名指す。
   名指しが無ければruntimeは判断を拒み、移動もしない。
   拒むときは、受け入れ条件に要らない高い優先度の候補は同じラベルの受け皿に負けることと、受け皿の探し方・作り方を案内する。
   移動先が`low`のgoal（受け皿）なら名指しは要らない。
2. **runtimeは名指しの有無と移動先の優先度だけを検査する。**
   項目が移動先の受け入れ条件に本当にあるか、そのfollow_upを要るかの意味は判定しない（[ADR-t1504-1](2026-10-04-t1504-1-follow-ups-belong-to-the-goal-whose-acceptance-needs-them.md)決定5）。
   名指した項目は判断の記録に残り、その当否はplannerが決め、plan reviewが移動先の受け入れ条件と照らす。
   runtimeは受け皿を選ばず、follow_upを自動で移さない。ADR-t1639-2決定5の、判定と移動をplannerの手順にすることはこの検査のほかは変えない。
3. **記録の仕組みはADR-t1504-2のまま。**
   名指した項目は追記の判断の行に判断の記録として残り、判断の行と移動と同じtransactionで書く。
   別の記録は作らない。required・undecidedの判断は変えない。

人が付けた優先度と受け入れ条件は変えない。

## Alternatives

- **lintの警告にする**: submitまで気づかれず、移動は判断の記録の時に済んでいる。記録の時に止める方が早く確実である。
- **runtimeが受け皿へ自動で移す**: ラベルが複数あると行き先が一意でなく、要るか要らないかの意味の判断をruntimeが持つことになる（ADR-t1639-2の退けた案と同じ）。
- **文書の規則を強める**: 規則は既にあり、それで再発は止まらなかった。
- **項目が移動先の受け入れ条件の文にあるかを文字列で照合する**: 項目の書き方（番号・要約）は揃わず、一致しても要るとは言えない。意味の照合はplan reviewが持つ。

## Consequences

- plannerは`normal`以上のgoalを移動先にする前に、その受け入れ条件のどの項目が要るかを書くことになり、書けなければ受け皿を選ぶ。
- plan reviewは名指された項目を移動先の受け入れ条件と照らせ、名指しの無い高い移動先は届かない。
- 移動先の受け入れ条件に要るものを書かずに通す道は、`low`の受け皿を選ぶことだけになる。
