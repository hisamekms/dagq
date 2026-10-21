---
id: adr-t3048-1
type: adr
title: 自動更新のhandoffは着地の保留で待つrunのleaseを返さず、leaseを持ったまま次のprocessに引き継ぐ
status: accepted
created: 2026-10-21
updated: 2026-10-21
accepted_on: 2026-10-21
owners:
  - hisamekms
tags:
  - runtime
related:
  - adr-0045
  - adr-0039
  - adr-0049
  - design-supervisor-lifecycle-claim-hold
  - design-supervisor-lifecycle-handoff
---

# ADR-t3048-1: 自動更新のhandoffは着地の保留で待つrunのleaseを返さず、leaseを持ったまま次のprocessに引き継ぐ

## Context

reviewを通って着地slotを待つrunは、`[run.env]`のprogramが無い・検証のcontainerが使えない・CIが読めないか最初の返事が無い・着地のbranchが解決しない・空き容量が足りないときに着地を保留し、leaseを持ったまま待つ。止まるsupervisor（`down`・provisioningの失敗）は待ち続けられないので、task 648・1426から「drainは掃除の後も足りなければleaseを返し、runを`awaiting_integration`のまま人に残す」としてきた。

この規則は止めるdrainを前提にしていたが、判定はdrainとhandoffを区別していなかった。自動更新のhandoffは20〜40分ごとに起き、次のprocessが同じtokenで続けるのに、保留に当たったrunはleaseを返されてleaseもaskも無い`awaiting_integration`になり、誰にも拾われなかった（10/20〜21に3 run、最長13.8時間）。また着地slotを待つrunは組み立て直せないslotとして扱われ、handoffは着地の始まりを待つので、保留が解けるまでexecも待たされうる。

## Decision

1. **handoffは着地の保留でleaseを返さない。** 時間で解ける保留（空き容量の不足・CIの最初の返事待ち）に限らず、時間で解けない保留（programが無い・containerが使えない・CIが読めない・着地のbranchが解決しない）も同じく、runはleaseを持って待つ。次のprocessは止まらないsupervisorと同じ判定をし、止まらないsupervisorはどの保留でもleaseを持って待つので、handoffの前後で扱いを変える理由が無い。新しいbinaryがそもそもの原因（判定の不具合など）を直していることもある。
2. **止めるdrainの規則は変えない。** handoffでないdrainは今どおり、掃除が治している不足とCIの最初の返事待ちを待ち、それ以外の保留でleaseを返してrunを人に残す。
3. **保留で待つrunは引き継ぎに数え、handoffを待たせない。** execを決めるpassの読み取りで着地が保留なら、着地slot待ちのrunは着地が始まりかけていないので、leaseを持ったままslotから外して次のprocessに渡す。次のprocessはreview待ちのrunとしてadoptの組み立て直し（ADR-0039）に乗せ、記録済みのpassは`/exit`の後に着地の列に戻る（reviewをやり直さない）。人の`land`の答えで列に並んだrunは、adoptがその答えを読まずに聞き直すかreviewし直すので、次のprocessがleaseを返し、承認の着地の経路がleaseの無いrunとして拾う。保留の無い着地slot待ちのrunはすぐ着地を始めるので、今どおりhandoffがその着地を待つ。

## Alternatives

- **時間で解けない保留はhandoffでも人に返す**: 止まらないsupervisorは同じ保留でleaseを持って待つので、更新のたびにrunを人の仕事に変えるだけになる。人に知らせる経路は保留そのもの（askや記録）が持つ。
- **handoffが保留の解けるまで待つ**: 更新が数時間止まり、その間の新しいclaimも止まる。
- **着地slot待ちのrunを保留の有無に依らず引き継ぐ**: 保留の無いrunはすぐ着地を始めるので、待っても短い。adoptの`/exit`と列の記録を挟む分だけ着地が遅れる。

## Consequences

- handoffをまたいだ保留のrunは、次のprocessでadoptの経路（`/exit`→着地の列）を通り、着地の列に並んだ記録がもう1回残る。保留が解ければ、その後は止まらないsupervisorの着地と同じに進む。
- leaseもaskも無いまま残ったrunを拾い直す安全網はこの決定の範囲の外で、同じgoalの別のtaskが持つ。
