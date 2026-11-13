---
id: adr-t4638-1
type: adr
title: 更新のjobが入れる先と戻す先をjobのprincipalを受け付けるbuildに限る
status: accepted
created: 2026-11-13
updated: 2026-11-13
accepted_on: 2026-11-13
amends:
  - adr-0073 decision 12
  - adr-0073 decision 13
owners:
  - hisamekms
tags:
  - runtime
  - security
related:
  - adr-0073
  - adr-t4629-1
---

# ADR-t4638-1: 更新のjobが置くbuildの資格の互換性

## Context

[ADR-t4629-1](2026-11-11-t4629-1-update-job-principal-issued-by-its-supervisor-ends-with-the-job.md)の更新jobのprincipalは起動元のsupervisorの終了後も使えるが、そのprincipalを知らないbuildのserviceはtokenを認めない。
binaryを戻すと、戻したbuildのsupervisorの起動がserviceもそのbuildのものへ替え得る。
launchdも戻したbinaryでsupervisorをいつでも起動し直すので、jobは戻した後の終端の記録と資格の返却が通ることを要求の順では保証できない。

## Decision

1. [ADR-0073](0073-kind-additions-are-compatible.md)決定12の入れ替え前の確認に、更新のjobが入れるbinaryはjobのprincipalを受け付けるbuildであることを加える。
   受け付けなければ何も入れ替えず確認の段の失敗とする。
2. 同決定13の戻しは、入れ替え前のbuildとの一致に加え、更新のjobのprincipalを受け付けるbuildであることを条件にする。
   引き継ぎの全員の失敗でinstall自身が戻すときにも同じ条件を課す。
   受け付けなければbinaryを置いたまま、戻さなかった理由を失敗の記録とaskに残し、人に委ねる。
   戻したbinaryへの引き継ぎと起動し直しも行わず、置いてあるbinaryのままの復旧は保つ。
3. 人のinstallとrollbackは呼び出し元のprincipalで送るので、この条件を課さない。

## Alternatives

- (i) 資格をsupervisorの寿命に縛ったままにする: 起動元の終了後の見張りを続けられないので退ける。
- (ii) 戻すときだけ人のprincipalへ替える: jobの権限の境界を破るので退ける。
- (iii) 戻した後の認証の失敗で終える: 終端の記録と資格の返却を保証できないので退ける。
- (iv) 戻した後の要求の順を工夫し、戻したbuildのserviceが立つ前に終端の記録とtokenの返却を済ませる: launchdが戻したbinaryで起動し直す時をjobは決められないので保証にならない。
- (v) jobのprincipalを知らないbuildに互換の受け付けを先に入れる: 戻す先はtargetに前にあった任意のbuildで、先に入れた版より前のbuildを除けない。

## Consequences

jobがtargetに置くbuildのserviceは、どの段でもjobのprincipalを知り、serviceの入れ替え後も終端の記録と資格の返却を受け付ける。
受け付けない前のbuildへの戻しは、人が自分のprincipalで行う。
