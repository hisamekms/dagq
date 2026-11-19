---
id: adr-t5293-1
type: adr
title: drainは止まった待ちを引き継ぎ着地の列は答え待ちを飛ばす
status: accepted
created: 2026-11-19
updated: 2026-11-19
accepted_on: 2026-11-19
amends:
  - adr-t5292-1 decision 3
  - adr-0071 decision 11
  - adr-t5283-1 decision 4
owners:
  - hisamekms
tags:
  - supervisor
  - capacity
related: []
---

# ADR-t5293-1: drainは止まった待ちを引き継ぎ着地の列は答え待ちを飛ばす

## Context

着地を許したrunに答え待ちが残ることがある。
また、taskがholdされていると、待ちが終わったrunもslotに戻れない。
人の操作を待つrunが一つ残るだけで、後続の着地やsupervisorの停止が進まなくなる。

## Decision

1. 答え待ちのaskは、そのentryだけに効く控えとして着地の列が扱う。
   列は今着地できる最初の候補を返し、飛ばしたentryの順番を保つ。
   Admissionは列が選んだ候補の開始とclaimとの兼ね合いを決める。
2. 停止要求を受けたdrainは進行中の工程を終え、今進める待ちからの戻りを扱う。
   hold中のrunは答えが届いてもslotに戻さない。
   holdや答え待ちで止まっているrunは待ちの記録とleaseを保ち、次のsupervisorに渡す。

## Alternatives

- Admissionが答え待ちの候補を飛ばす案では、着地の列を読む経路ごとに控えの判断が分かれる。
- drainが待ちのrunをすべて待つ案では、holdを解除するまで停止できない。

## Consequences

着地を飛ばされたrunは答えが来れば元の順番で選ばれる。
停止のために人へ回答やholdの解除を求めずに済む。
次のsupervisorは待ちの記録から見張りか戻りを再開する。
