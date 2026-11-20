---
id: adr-t5111-1
type: adr
title: statusとstatsとkpiをworkerのslotで数え着地の列を分ける
status: accepted
created: 2026-11-17
updated: 2026-11-17
accepted_on: 2026-11-17
amends:
  - adr-t1591-1 decision 8
  - adr-t610-1 decision 1
owners:
  - hisamekms
tags:
  - architecture
  - measurement
related:
  - adr-t5107-1
  - adr-t5283-4
---

# ADR-t5111-1: statusとstatsとkpiをworkerのslotで数え着地の列を分ける

## Context

workerの枠と着地の列を分けても、表示と計測が着地中をworkerに数えると、空きがあるのに埋まって見える。
人は表示と計測もworkerのslotに揃えると決めた。

## Decision

1. **表示と空きは判定と同じ集合で数える。** ADR-t1591-1決定8とADR-t610-1決定1を改め、statusの使用数とstatsの空きを、ADR-t5107-1とADR-t5283-4のworkerのslotで数える。着地中・着地側のe2e・着地の順番待ちを別に出し、上限を超えてworkerに数えた分も分かるようにする。共通の着地の列が生む背圧も、着地を進められる候補と控えの既存の記録から数える。runごとの枠の扱いにも着地中を着地の列として出す。claimと戻りの判定と同じ集合で数える原則、重いtaskも使える枠を空きとする原則、ADR-t610-1決定2の登録の無いtokenのleaseを数えないことは保つ。
2. **計測もworkerの保持で数える。** kpiのslotの使用率と、その占有数を着地数と保持時間で照合する両辺から、着地の列の区間を同じように除く。過去の記録から順番待ちの超過を確定しにくいときは、着地の列を全て除く近似を許し、その範囲を定義のそばに明記する。
3. **既存の記録から組み立てる。** ADR-t1591-1決定8の新しいeventのkindを足さないことを保つ。reviewはworkerとして数え、e2eは記録された予算の区分に従う。欄の名前・形・意味と近似の詳細は定義のそばのdoc commentが持つ。

## Alternatives

- 着地中もworkerの使用数に含める: 表示からclaimの空きを読めない。
- 過去の超過分を新しい記録で補う: 過去に遡れず、記録を増やさない方針にも反する。

## Consequences

表示と空きからworkerの詰まりと着地の詰まりを区別できる。
近似の計測は順番待ちが上限を超えた期間のworkerの保持を少なく数える。
