---
id: adr-t632-1
type: adr
title: 引き継ぎ（install・up・auto-updateのhand_offと見張り）で、binaryを前に戻すのは引き継がせた全員が失敗したときだけにし、一部の失敗では新しいbinaryを残して失敗したsupervisorを人に知らせる（ADR-0073決定13・14・15・17をamends）
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
amends:
  - adr-0073 decision 13
  - adr-0073 decision 14
  - adr-0073 decision 15
  - adr-0073 decision 17
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
related:
  - adr-0073
  - adr-t598-1
  - design-supervisor-lifecycle-install
  - design-supervisor-lifecycle-auto-update
  - design-supervisor-lifecycle-up-down
  - design-supervisor-lifecycle-handoff
---

# ADR-t632-1: 引き継ぎで、binaryを前に戻すのは引き継がせた全員が失敗したときだけにし、一部の失敗では新しいbinaryを残して失敗したsupervisorを人に知らせる（ADR-0073決定13・14・15・17をamends）

## Context

[ADR-0073](0073-kind-additions-are-compatible.md)は、`install`・`up`・自動更新がliveなsupervisorのすべてに新しいbinaryのexecを要求し（決定10・14・15・17）、引き継ぎや見張りが失敗すれば`.previous`に戻す（決定13）と決めた。queueのsupervisorが複数あるとき、その一部だけが失敗した場合の扱いは決めていなかった。

実装は、引き継ぎの待ちが最初に失敗したsupervisorで止まり、`install`はそのとき固定バイナリを`.previous`に戻していた。そのため、既に新しいbinaryをexecした他のsupervisorが、戻ったfileと食い違ったまま動いていた（次にlaunchdが起動し直すと別のbuildになる、`status`のbuild識別子とfileが合わない）。一方、task 497は引き継ぎの後の見張りで「全員が失敗したときだけ戻し、一部の失敗では新しいbinaryを残して`update_failed`のaskを開く（`kept: true`）」にしていた。planner（task 632）は、見張りと引き継ぎで扱いが分かれると運用が読みにくいことから、引き継ぎも同じ扱いに揃えると決めた。

## Decision

1. **binaryを前に戻すのは、引き継がせた全員が失敗したときだけにする。** 固定バイナリはqueueのsupervisorの全員で1つのfileなので、1つでも新しいbinaryで動いているsupervisorがあれば戻さない。`install`・`up`・自動更新の引き継ぎと、自動更新の引き継ぎの後の見張りに同じ規則を当てる。
2. **引き継ぎは最初の失敗で止まらず、引き継がせた全員の結果を待つ。** 待ちの上限は全員で1つ。supervisorごとに成功（今のtoken）か失敗（理由）を決め、要求の取り消しは失敗したsupervisorにだけ行う。成功したsupervisorの要求は、他の失敗で取り消さない。
3. **一部の失敗は、新しいbinaryを残したまま人に知らせる。** 失敗したsupervisorは前のbinaryのprocessのまま動いているか止まっているので、人は`down --force`と`up`で新しいbinaryに揃えるか、`install --rollback`で全員を前のbinaryに戻すかを選ぶ。
   - `install`は失敗したsupervisorを名前と理由つきで示し、結果（`kept: true`とsupervisorごとの結果）を出して非0で終わる。
   - 自動更新は、見張りの一部の失敗と同じく`update_failed`のask（`kept`とsupervisorごとの結果）を開く。
   - `up`はfileを差し替えないので戻すものは無く、全員を待ってから失敗したsupervisorを示して非0で終わる。

ADR-0073の決定13のうち「引き継ぎや見張りが失敗すれば戻す」を「全員が失敗したときだけ戻す」に、決定14・15・17の引き継ぎの失敗の扱いをこれに改める。確認・差し替え・`.previous`の扱い・見張りの期限・非互換のmigrationのdrainは変えない。

## Alternatives

- **一部の失敗でも戻す（これまでの実装）**: 既にexecしたsupervisorが戻ったfileと食い違う。成功したsupervisorをもう一度引き継がせ直す手順も要る。
- **一部の失敗は成功として扱う**: 前のbinaryのまま動くsupervisorが残ることを人が知らないまま、queueに2つのbuildが混ざる。
- **失敗したsupervisorを自動で止めて新しいbinaryで起動し直す**: 前のbinaryで動き続けているsupervisorはrunを進めており、止めるかどうかは人が決める（`down --force`は走っている処理を切る）。見張りで止まったsupervisorを起動し直す既存の動き（決定13）は変えない。

## Consequences

- 引き継ぎの一部の失敗の後も、固定バイナリのfileと、引き継いだsupervisorのbuildが一致する。
- 人は、`install`の出力か`update_failed`のaskで、どのsupervisorが前のbinaryのまま残ったかを見て、揃えるか戻すかを選ぶ。
- 結果の欄・error の文面・test は[`install`](../design/supervisor-lifecycle/install.md)、[Auto-update](../design/supervisor-lifecycle/auto-update.md)、[`up` / `down`](../design/supervisor-lifecycle/up-down.md)が持つ。
