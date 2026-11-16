---
id: adr-t5059-1
type: adr
title: run 1本の空き容量の想定を予測と設定の固定枠から選べるようにする
status: accepted
created: 2026-11-16
updated: 2026-11-16
accepted_on: 2026-11-16
amends:
  - adr-t4473-1 decision 3
owners:
  - hisamekms
tags:
  - architecture
  - disk
related:
  - adr-t4473-1
  - adr-t4542-1
  - adr-t4727-1
  - adr-t2257-1
---

# ADR-t5059-1: runの想定は予測と設定の固定枠から選べる

## Context

[ADR-t4473-1](2026-11-08-t4473-1-disk-needs-follow-a-reserve-and-recent-runs-with-no-factor.md)決定3は、runの想定を直近の標本の分位点とした。
このrepositoryでは大きいrunの標本で想定が上がり、空きslotがあってもclaimが長く控えられた。
人は、予測方式を残し、設定で固定枠に切り替える方式を選んだ。
固定枠を汎用runtimeの既定にするとproject固有のbuildの大きさを持ち込むので、固定枠はprojectが選ぶ。

## Decision

ADR-t4473-1決定3の想定の求め方を、次のように改める。

1. **run 1本の想定の方式を設定で選べる。** 既定は直近の標本からの予測で、期間と件数の窓・少ない標本の最大・分位点の求め方は保つ。固定枠を選ぶと想定は設定の値になり、標本の件数・期間・外れ値によらず、標本が無くても想定を持つ。claimと着地は同じ方式を使う。
2. **固定枠の値はprojectの設定だけが持つ。** projectの`dagq.toml`が値を与え、runtimeは固定枠の既定値を持たない。[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定1を保つ。
3. **想定の出どころ以外の判断は保つ。** ADR-t4473-1決定1の係数なし・決定2の最低ライン・決定5の前進の保証・決定6の着地の先行は両方式に効く。標本が無いとき1本ずつ進めるのは予測方式で想定が無い場合である。[ADR-t4542-1](2026-11-09-t4542-1-moving-runs-are-measured-off-the-loop-for-their-remaining.md)の残りの見込みは、想定から測った大きさを引き負なら0、量れないrunは想定そのままを数える。枠を超えたrunの残りは0だが、実際の空きが最低ラインと新しい1枠の和に届かなければ、動いているrunがあるときのclaimは通らない。
4. **期限つきの緩めは固定枠にも効く。** [ADR-t4727-1](2026-11-11-t4727-1-disk-needs-may-be-relaxed-for-a-while-by-a-person-or-the-inbox.md)の想定の上書きは固定枠の値も上書きし、最低ライン・前進の保証・着地の先行を保つ。

ほかの決定は変えない。
実装中に別の決定を変える必要が分かったら、人に聞いて止める。

## Alternatives

- **予測だけを保つ**: projectが許容する固定枠を選べず、標本による長い控えが続く。
- **固定枠をruntimeの既定にする**: 別のprojectにもこのrepositoryのbuildの大きさを前提にした値を持ち込む。
- **固定方式で必要量の式も変える**: 方式によって前進の保証や着地の順が変わる。想定の値だけを同じ式に入れる。

## Consequences

- 設定を足さないprojectは今の予測のまま進む。
- 固定方式は標本が無くても空き容量に応じて複数のrunをclaimできる。
- 固定枠を超えるrunが重なると容量不足で落ちる可能性は残り、最低ラインと既存のやり直しで受ける。
- projectは固定枠の値を見直す責任を持つ。
