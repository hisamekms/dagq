---
id: adr-t1332-1
type: adr
title: watcherが居ないあいだに開いたaskのask_seen_waitは、watcherが戻るかaskが終わるかの早い方までの秒で、その時刻の期間に数え、runtimeが閉じたaskも外さない（ADR-0051決定1をamends）
status: accepted
created: 2026-10-20
updated: 2026-10-20
accepted_on: 2026-10-20
amends:
  - adr-0051 decision 1
owners:
  - hisamekms
tags:
  - runtime
  - operations
related:
  - adr-0051
  - adr-t1433-5
  - adr-t1091-1
---

# ADR-t1332-1: ask_seen_waitは、watcherが戻るかaskが終わるかの早い方までの秒で数える（ADR-0051決定1をamends）

## Context

KPIの`ask_seen_wait`は、askがinboxのwatcherに見られるまでの秒を数える。
watcherが居るあいだに開いたaskは0秒で、居ないあいだに開いたaskは、答えたか閉じたかに依らず次にwatcherが戻るまでの秒で数えていた。
watcherが居ないあいだにruntimeが閉じたaskや、人が別のterminalから答えたaskも不在の長さで数えるので、数時間の不在では、その間に開いたaskの数だけ不在の時間が重ねて数えられ、askが開いたまま見られなかった時間より大きく出る。
2026-09-30以降に開いたaskは51件、そのうち閉じた記録の付いたものは31件で、終わり方の大半はruntimeのcloseだった。

## Decision

watcherが居ないあいだに開いたaskの`ask_seen_wait`は、次にwatcherが戻った時刻と、そのaskの終わり（最初の答え。runtimeが閉じたaskはruntimeの答え。答えの無いまま閉じたaskはその閉じた時刻）の早い方までの秒とし、その時刻の期間に数える。
runtimeが閉じたaskも外さず、同じく打ち切って数える。
期間の終わりまでにwatcherが戻らずaskも終わっていないものは標本にしない。
watcherが居るあいだに開いたaskを0秒にすることと、watcherの記録の無い期間を標本にしないことは変えない。

## Alternatives

- **watcherが戻る前に終わったaskを標本から外す**: watcherが居るあいだに開いたaskはruntimeが閉じたものも0秒で数えたまま、居ないあいだのaskだけを落とすことになり、分布が小さい側に偏る。
- **データが溜まるまで決めない**: KPIはrun_eventsから読むたびに導き直すので、定義を変える時期で前後の比較が崩れない。待つ理由が無い。

## Consequences

- 過去の期間も新しい定義で数え直される。印や移行は要らない。
- `ask_wait`（人の答えの待ち）はruntimeが閉じたaskを外し、`ask_seen_wait`（inboxが見るまでの時間）は外さない。2つは母数が違うので、件数を並べて比べない。
