---
id: adr-t3524-1
type: adr
title: supervisorが始めた着地がruntimeのerror（code other）でmainを動かす前に止まったleaseの無いrunは、別のbuildのsupervisorがbuildごとに1回だけ拾い直す（ADR-t3049-1決定3をamends）
status: accepted
created: 2026-10-28
updated: 2026-10-28
accepted_on: 2026-10-28
amends:
  - adr-t3049-1 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - lifecycle
related:
  - adr-t3049-1
  - adr-0039
  - adr-0054
  - adr-0045
---

# ADR-t3524-1: supervisorが始めた着地がruntimeのerror（code other）でmainを動かす前に止まったleaseの無いrunは、別のbuildのsupervisorがbuildごとに1回だけ拾い直す（ADR-t3049-1決定3をamends）

## Context

着地がmainを動かす前に止まると、着地はslotを返して`integration_error`を記録し、runはleaseの無い`awaiting_integration`に残る。
[ADR-t3049-1](2026-10-21-t3049-1-supervisor-takes-up-a-passed-run-left-awaiting-integration-without-lease-or-ask.md)決定3は、最後のreview以降に着地の失敗を記録したrunを拾い直しの対象の外にし、人の着地（`review and integrate`）を待たせる。
errorの原因がruntime自身の不具合（queue serviceの拒否など、codeは`other`）のときは、修正の入ったバイナリに入れ替わっても誰も拾わない。
2026-10-26〜27に、着地のtokenがqueue serviceに拒まれる不具合で18本がそう残り、修正が自動更新で入った後もinboxが人の言葉で1本ずつ`integrate`した。
runtimeはerrorの原因と修正の対応を判定できない。
自動更新の一部の失敗で別のbuildのsupervisorが残る経路もある（[Auto-update](../design/supervisor-lifecycle/auto-update.md)）ので、buildが往復しうる。

## Decision

1. **記録する。** 着地の`integration_error`は、その着地を動かしたバイナリのbuild識別子（supervisorの登録のbuildと同じもの、[ADR-0045](0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)）を持つ。
2. **別のbuildが拾い直す。** ADR-t3049-1決定3の「最後のreview以降に着地の決定・控え・失敗を記録した」から、次のすべてに当たるrunを外す。
   - 最後のreview以降に着地を止めた記録が`integration_error`だけで、その最新のものがcode `other`である。
   - その着地をsupervisorが始めた（eventのactorの依頼元がsupervisor）。人の`integrate`のerrorは人が続きを持つので外さない。
   - そのerrorがbuildを持ち、判定するsupervisorのbuildと違う。
   - 判定するsupervisorのbuildが、最後のreview以降のこのrunの履歴にまだ現れていない（そのbuildの`integration_error`も、そのbuildによるこの拾い直しの記録も無い）。
   - 最新の`integration_error`の後に、どのbuildによるこの拾い直しの記録も無い。
   外したrunも、決定3のほかの項目（ask・runtimeのerrorでleaseを手放した・hold・再計画など）は今のまま当てはめる。
   buildの無いerror・同じbuildのerror・ほかのcode・着地の保留・控え・失敗・決定は今のまま人を待つ。
3. **buildごとに1回。** 上限は最新のerrorのbuildとの比較でなく、最後のreview以降の履歴で判定する。
   A→B→Aのように前に試したbuildへ戻っても拾わず、試せる回数は最後のreview以降に現れたbuildの数で上限がある。
   1つの`integration_error`を拾い直すのも1回で、拾い直したsupervisorが新しいerrorを残さずにleaseを返したrunは、どのbuildも拾わず人を待つ。
   判定はeventだけから決まり、同じbuildのsupervisorが再起動して判定し直しても同じ結果になる。
4. **leaseのトランザクションで判定し直す。** ADR-t3049-1決定6のleaseを取るトランザクションは、leaseを取るsupervisorの登録のbuildで同じ判定をし直し、当たらなければleaseを取らない。
   判定の後に別のbuildのsupervisorが拾い直したrunも、同じbuildの別のsupervisorが先に取ったrunも、トランザクションが最新のerrorの後の拾い直しの記録を読んで拾わない。
5. **拾い直しを記録する。** buildの違いで拾い直したときは、拾い直しの記録に、そのこと、拾い直したerrorのeventとそのbuild、拾い直した側のbuildを残す（形は記録を書く操作のdoc comment）。
   3の判定はこの記録の拾い直した側のbuildを読む。
   拾い直したrunは、ADR-t3049-1のほかの拾い直しと同じくruntimeの着地として出す。

ADR-t3049-1の決定1・2・4〜7、[ADR-0039](0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)決定1、[ADR-0054](0054-run-lease-ownership-parallel-supervisors-and-recover.md)決定12の残りは変えない。

## Alternatives

- **最新のerrorのbuildとだけ比べる**: A→B→Aの往復で同じerrorを繰り返すloopになる。
- **`update_installed`のeventの後かで判定する**: 人の`install`や`up`での入れ替えには`update_installed`が無く取りこぼし、queue全体のeventを判定に足すことになる。
- **人の`integrate`のerrorも拾う**: 人が始めた着地の続きは人が持つので、runtimeが重ねて動かさない。
- **errorの原因で判定する**: runtimeは原因と修正の対応を知らない。

## Consequences

- runtimeの不具合で止まった着地は、修正の入ったbuildに入れ替わった後にsupervisorが着地させる。修正の入っていない別のbuildでも1回試し、同じerrorならそのbuildの記録が残って人を待つ。
- 拾い直したsupervisorが着地の前にleaseを返したrun（止めるdrainなど）は、どのbuildも拾い直さず人を待つ。
- 修正を運ぶtaskのrunのように、変更が人の手で既にmainにあるrunを拾い直すと、rebaseの後に何も残らず、今の空のrebaseの経路のとおり着地を控えてsessionに戻し、sessionが失敗のreceiptを書けばrunは`failed`になってtriageと人に届く。mainは動かず、同じbuildで繰り返さない。この経路はこのADRでは変えない。
- この決定の前に記録されたbuildの無い`integration_error`のrunは対象の外のまま残る。
- ADR-t3049-1決定7の工程とattentionは、記録・表示する側のbuildで拾い直しの対象かを判定する。着地を止めたbuildのsupervisorが記録する工程は人の答えを待つ工程で、別のbuildのsupervisorが拾えばslotに戻る。決定7は変えない。
