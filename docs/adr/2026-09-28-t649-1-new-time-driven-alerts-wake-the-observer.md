---
id: adr-t649-1
type: adr
title: eventを出さずに時間だけで閾値を超えたalertが前回のobservationに無かったものなら、observerの変化とみなして起動する（ADR-0047決定21をamends）
status: accepted
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
amends:
  - adr-0047 decision 21
owners:
  - hisamekms
tags:
  - runtime
  - observer
related:
  - adr-0047
  - adr-t598-1
  - design-supervisor-lifecycle-observer
  - design-supervisor-lifecycle-stats
---

# ADR-t649-1: eventを出さずに時間だけで閾値を超えたalertが前回のobservationに無かったものなら、observerの変化とみなして起動する（ADR-0047決定21をamends）

## Context

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定21は、observerが変化の無いときに何もせずに終わると決めた。task 294はこれを「前回のobservationが読んだeventより後に、observer自身のもの以外のeventが無ければagentを起動しない」と実装した。

この判定は、eventを出さずに時間だけで閾値を超える検出を見落とす。止まったsessionはsupervisorが催促や復旧jobのeventを出すのでskipが破れるが、`stats`のalertのうち経過時間やその時点のsnapshotだけから作るもの（答えられないask、空いたslot、など）はeventを伴わない。前回は閾値の手前でskipが続くあいだに閾値を超えても、observerは起動せず、findingにも`blocked`のaskにもならない（task 294のreviewの指摘）。

planner（2026-09-27）は、回数や時間の上限を付けること、openなrunやaskがある間はskipしないことを採らなかった。runやaskは常に開いていることが多く、変化の無い時間の起動を無くす決定21の意図を崩すからである。

## Decision

1. **前回のobservationに無かったalertが今あれば、それを変化とみなしてagentを起動する。**
   - 成功したobservationは、その回に見た`stats`のalertの識別（種類と対象。時間とともに伸びる値は含めない）の一覧を記録に持つ。
   - skipの判定は、自分以外のeventの有無に加えて、今の`stats`のalertの識別を前回の成功したobservationの一覧と比べる。前回に無かったものが1つでもあれば、自分以外のeventが無くても起動する。
   - eventが無く、alertの集合が前回と同じか減っただけなら、今までどおり起動せずにskippedを記録する。続いているだけのalertでは起こさない。
   - 一覧を持たない古いobservationの後は、alertが1つでもあれば起動する（何を見たか分からないので、見ていないとみなす）。

ADR-0047の決定21のうち「変化の無いときは起動しない」の変化の定義に、新しく出たalertを足す。決定21のその他（書き直さないこと、MCPを読み込まないこと、間隔、入力）は変えない。

## Alternatives

- **回数や時間の上限でskipを破る（N回skipしたら起動する）**: 変化が無くても定期的に起動するので、決定21が無くしたかった変化の無い時間の起動が戻る。
- **openなrunやaskがある間はskipしない**: runやaskは常に開いていることが多く、ほぼ毎回起動する。
- **時間由来のalertが閾値を超えたときにruntimeがeventを記録する**: 検出の側に閾値ごとの記帳が要り、`stats`を読んだ時点のsnapshotで判定するalert（空いたslotなど）には超えた時刻が無い。observerの判定で`stats`を読めば足りる。

## Consequences

- 時間だけで閾値を超えたalertも、次のobservationでfindingか`blocked`のaskになりうる。
- skipの判定のために、起動しない回も`stats`を読む。observationは3時間ごとなので負担は小さい。
- 識別の組み立て方、記録の欄名、対象に含める欄は[Observer](../design/supervisor-lifecycle/observer.md)が持つ。
