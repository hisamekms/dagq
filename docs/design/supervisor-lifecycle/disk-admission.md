---
id: design-supervisor-lifecycle-disk-admission
type: design
title: "claimと着地のディスクの開始判断"
status: current
created: 2026-11-19
scope: runtime
related:
  - adr-t5292-1
  - design-supervisor-lifecycle-claim-hold
---

# claimと着地のディスクの開始判断

必要量は同じ回の空きと予約から決める。
[控えと掃除](claim-hold.md#空き容量)は、単独の開始に要る容量と最低ラインを使う。

## 予約と単独の開始

- phaseの性質・候補ごとの必要量と可否を一度決め、開始・控え・askが同じ結果を読む入口は`domain::disk::DiskState::decide`。
- 方式の違いはWとIの出どころだけで、係数を掛けない。
  標本の入口は`domain::disk::StagePeaks`、見積もりの入口は`domain::disk::stage_budgets`。
- 動くrunが0本なら、空きが最低ライン以上あれば1本は進める。
  予測方式で標本が無ければqueueは1本ずつ進む（ADR-t5059-1）。
  対象は[今進めるrun](claim-hold.md#workerのslotと着地の列)。
- 予約と今の大きさの数え方は`domain::disk::DiskState::needs`を読む。

## claimと着地の同時開始

- 同時開始に要る容量の入口は`domain::disk::DiskNeeds::claims_with_landing`。
  選ばれた着地がe2eから始まるときも、e2e用のleaseを取る回に同じ容量で判じる。
- 空きが同時開始の必要量に足りなければ効く優先度の高い方を先にし、同じ優先度なら着地を先にする。
  動くrunが無いときに最低ラインだけで通す前進保証は、この順で選ぶ1本に使う。
- 着地の番が埋まっている回や、着地候補が控えで始められない回は、着地待ちを理由にclaimを止めない。
