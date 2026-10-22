---
id: adr-t3134-1
type: adr
title: 生きているが黙っているsupervisorの止め方を、同じqueueに生きているsupervisorが他に居るかで分け、居るときはdownを使わずそのプロセスだけを止める（ADR-0073決定16をamends）
status: accepted
created: 2026-10-23
updated: 2026-10-23
accepted_on: 2026-10-23
amends:
  - adr-0073 decision 16
owners:
  - hisamekms
tags:
  - runtime
  - operations
related:
  - adr-0073
  - adr-0014
  - adr-0047
  - design-supervisor-lifecycle
---

# ADR-t3134-1: 生きているが黙っているsupervisorの止め方

## Context

[ADR-0073](0073-kind-additions-are-compatible.md)決定16は、判定の対象をliveな登録だけにし、PIDが生きていてheartbeatの止まった「生きているが黙っている」supervisorには`up`も`install`も自動更新も入れ替えを要求せず、reuseもpruneもkillもしないと決めた。
その後半は、`status`がstaleとして報告するので、人が`down --force`で止めてから`up`をやり直すと無条件に書く。

`down`はそのqueueのsupervisorを全部止める。
黙っているsupervisorの隣に、同じqueueでrunを流している生きている（staleでない）supervisorが居るとき、`down --force`はその生きている方も止め、走っているrunを失わせる。
そのとき`up`は生きている方をreuseするだけなので、`up`のやり直しも要らない。
`status`のattentionは、生きているsupervisorが居るときstaleな登録の案内をsupervisorの再起動にせず、黙っているsupervisorにはそのプロセスを止める案内を出す。

## Decision

1. **決定16の前半は変えない。** 判定の対象はliveな登録だけで、生きているが黙っているsupervisorには`up`も`install`も自動更新も入れ替えを要求せず、reuseもpruneもkillもしない。
2. **人の操作は、同じqueueに生きているsupervisorが他に居るかで分ける。**
   - 居ないとき: 今どおり、人が`down --force`で止めてから`up`をやり直す。
   - 居るとき: `down`を使わず、人がその黙っているsupervisorのプロセスだけを止める。残った登録は生きているsupervisorに片付けさせ、`up`はやり直さない。
   止める前にそのPIDがまだこのqueueのsupervisorであることを確かめることと、案内の文言と手順は`status`のattentionとpluginの復旧の案内が持つ。

## Alternatives

- **今どおり無条件に`down --force`と`up`**: 生きているsupervisorまで止め、そのrunを失わせる。
- **生きているsupervisorが黙っている方を自動でkillする**: PIDの再利用や引き継ぎの途中の登録を取り違えると別のプロセスを止める。決定16の「killもしない」を崩すので、止めるのは人の確認の後に残す。

## Consequences

- 生きているsupervisorが居る間、黙っているsupervisorを止めてもrunは流れ続ける。
- 人は、黙っているsupervisorの隣に生きている方が居るかを`status`のattentionの案内で見分ける。
