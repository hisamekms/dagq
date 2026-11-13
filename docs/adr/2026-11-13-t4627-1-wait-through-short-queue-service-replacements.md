---
id: adr-t4627-1
type: adr
title: clientの要求とwatchはqueue serviceの入れ替えの短い不在を上限30秒まで待つ
status: accepted
created: 2026-11-13
updated: 2026-11-13
accepted_on: 2026-11-13
amends:
  - adr-t3102-3 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - reliability
related:
  - adr-t3102-2
  - design-queue-service-host-cli
---

# ADR-t4627-1: queue serviceの入れ替えの短い不在を待つ

## Context

自動更新でqueue serviceを入れ替える数秒の間、socketが無い・接続が拒まれる・答えがEOFになることがある。
[ADR-t3102-3](2026-10-22-t3102-3-stage-5-outage-notice-and-use-cases-for-multi-step-control-operations.md)決定2の「直ちに返る」では、不在の記録が無いこの短い間にもinboxのwatchが失敗して終わる。
入れ替えの起動が答えを待つ上限10秒と、停止から起動までの間を覆う待ちが要る。

## Decision

1. **決定2の直ちに返る部分を、入れ替えの短い不在を覆う上限30秒まで待ってから返る形にamendsする。** clientの要求は、届かなかったと分かる失敗なら読み書きとも間を置いて送り直し、答えを読めず届いたか分からない失敗なら読み取りだけを送り直す。届いたか分からない書き込みを黙って送り直さない。statusもこの読み取りの待ちを受ける。watchは不在の記録が無い届かないlookを上限の内は待ってやり直す。watchにtimeoutがあれば残りの時間で頭打ちにする。上限を過ぎれば、空の答えや通常の時間切れに替えず、今と同じ届かない返りにする。
2. **記録された不在の扱いとDBの境界は保つ。** 不在の記録があれば次の手とともに添え、watchは同じ不在をroleごとに一度だけ知らせ、その後は戻るまで待つ。どの失敗でもclientはqueue DBを直接開かない。

## Alternatives

- 直ちに返る: 正常な入れ替えでもwatchが止まる。
- 上限なく待つ: 長い不在を人へ知らせられない。
- 書き込みも全て送り直す: 既に行った操作を重ねる恐れがある。

## Consequences

短い入れ替えは読み取りとwatchの中で吸収する。
本当の不在を返すまでには限られた待ちが増える。
通信の判断と待ちの境界は[hostのCLI](../design/queue-service-host-cli.md#届かないとき)から辿る。
