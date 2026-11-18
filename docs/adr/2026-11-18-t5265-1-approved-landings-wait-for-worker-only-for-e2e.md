---
id: adr-t5265-1
type: adr
title: 承認された着地はe2eの開始だけworkerの空きを待つ
status: accepted
created: 2026-11-18
updated: 2026-11-18
accepted_on: 2026-11-18
amends:
  - adr-t5107-1 decision 5
amended_by:
  - adr-t5283-4
  - adr-t5283-1
owners:
  - hisamekms
tags:
  - architecture
  - capacity
related:
  - adr-t5107-1
---

# ADR-t5265-1: 承認された着地はe2eの開始だけworkerの空きを待つ

## Context

着地の列をworkerのslotから分けても、着地の答えのadoptがworkerの空きを待つと、e2eが要らないrunまで着地を始められない。
e2eはworkerの枠を使うが、着地は着地の側の枠を使う。

## Decision

1. ADR-t5107-1決定5の着地の答えのadoptについて、e2eの要らないrunの着地の開始はworkerのslotの空きを見ない。e2eの要るrunのe2eの開始だけがworkerのslotの空きを待つ。停止時のdrainも同じ判定とする。
2. 承認の順に候補を選ぶ。workerのslotが無い間はe2eの要るrunを飛ばし、次のe2eの要らないrunを着地させる。workerのslotが空いていれば最古のe2eの要るrunが先にleaseを取る。e2eの終了は承認順とは限らないため、着地順も承認順とは限らない。
3. ADR-t5107-1決定1・2の着地の列とworkerのslotの数え方、着地はqueue全体で1本ずつという直列、着地のほかの控えを保つ。決定5のclaim・resume・復旧・adopt・取り残されたrun・待ちからの戻りの判定は引き続き同じworkerのslotの数で判じる。

## Alternatives

- 最古の承認がe2eの空きを待つ間は後続も待つ: workerを使わない着地が止まる。
- e2eもworkerの空きを見ずに始める: hostの重いbuildの同時数がworkerの上限を超える。

## Consequences

workerが全て埋まっていても、e2eの要らない承認済みrunは着地の側の枠で進む。
着地の直列と既存の控えは変わらず、e2eの同時数もworkerの上限を保つ。
