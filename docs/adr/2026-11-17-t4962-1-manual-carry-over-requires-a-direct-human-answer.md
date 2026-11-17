---
id: adr-t4962-1
type: adr
title: 手での引き継ぎはplan reviewのbypassとしてinboxの中継を拒み人の直接の答えだけを受ける
status: accepted
created: 2026-11-17
updated: 2026-11-17
accepted_on: 2026-11-17
amends:
  - adr-t1962-1 decision 1
owners:
  - hisamekms
tags:
  - recovery
  - authorization
  - audit
related:
  - adr-t3294-1
  - adr-t4840-3
  - design-supervisor-lifecycle-triage
---

# ADR-t4962-1: 手での引き継ぎは人の直接の答えだけを受ける

## Context

[ADR-t1962-1](2026-10-07-t1962-1-retry-carrying-the-branch-over-by-hand.md)決定1は、手での引き継ぎをuserとinbox（人の言葉で）のcapabilityとした。
[ADR-t4840-3](2026-11-12-t4840-3-protected-paths-and-weakening-operations-need-a-direct-human-answer.md)決定4は、plan reviewのbypassを弱める操作としてinboxの中継を拒む。
手での引き継ぎもplan reviewを通らずにtaskをreadyに戻すため、この境界に含める必要がある。

## Decision

1. 手での引き継ぎ（`ready --inherit`。引き継ぐ成果のread・imagesも含む）は、ADR-t4840-3決定4の弱める操作のうちplan reviewのbypassに数える。
   ローカルの既定の配置でもinboxの中継と代行を拒み、決定5の人が直接答える経路の答えだけをhuman_attestedとして受ける。
   hostの配置の段0では、人がDAGQ_ROLEの無い自分のterminalで直接コマンドを打ち、制御端末（TTY）で対象を確かめる。
2. ADR-t1962-1決定1のcapabilityの文だけを、この範囲でuserの直接の判断に狭めるamendsとし、ADR全体を置き換えない。
   ADR-t1962-1の他の決定と、[ADR-t3294-1](2026-10-25-t3294-1-carry-over-by-hand-keeps-the-ref-before-the-record-and-puts-it-back-on-refusal.md)によるamendsは保つ。
   他の決定にあるinboxの記述は、この操作をinboxに許す根拠にはしない。
   ADR-t4840-3決定2・4〜7は保ち、普通の着地の承認の中継、チームの版の直接の経路、hostのadvisoryの限界も変えない。

## Alternatives

- 人の言葉ならinboxが手での引き継ぎを中継できるとする: plan reviewを通らずにreadyへ戻す操作に、弱める操作の直接の判断を求める境界が効かなくなるので採らない。
- ADR-t1962-1を丸ごと置き換える: 変えるのは決定1のcapabilityの範囲だけで、引き継ぎの前提・競合の扱い・次のrunの始まり方は変えないため採らない。

## Consequences

inboxは手での引き継ぎを代行せず、人に自分のterminalでの操作を案内する。
人の直接の確認が必要になる運用の重さを、このplan reviewのbypassにも適用する。
今の流れは[triageの人の手での引き継ぎ](../design/supervisor-lifecycle/triage.md#人の手での引き継ぎ)が持つ。
