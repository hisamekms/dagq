---
id: adr-t791-1
type: adr
title: 効く優先度の継承元から、abandonedで閉じたgoalに依存するtaskを除く（ADR-0049決定4をamends）
status: accepted
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
amends:
  - adr-0049 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - graph
related:
  - adr-0049
  - adr-0040
  - adr-0038
  - adr-t598-1
  - design-domain-model
---

# ADR-t791-1: 効く優先度の継承元から、abandonedで閉じたgoalに依存するtaskを除く（ADR-0049決定4をamends）

## Context

[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定4（[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定4を変えずに引き継いだもの）は、taskの**効く優先度**を「自分と、自分を推移的に待っている`ready`のtaskの優先度の最大値」とし、`draft`・`canceled`・`completed`のtaskと、draftのgoalに属するtaskからは継承しないと決めた。理由は、draftに退避したtaskや流す予定の無いtaskが依存元を押し上げないため。

[ADR-0038](0038-task-depends-on-a-goal-until-it-is-achieved.md)のgoal依存は、依存先のgoalが`achieved`で閉じるまでtaskをclaimしない。`abandoned`で閉じたgoalは依存を解かないので、そのgoalに依存する`ready`のtaskは永久にclaimされない（graphの`ready_after`に残る依存先goalとして見える）。決定4の継承元の定義はこの場合を除いていなかったため、そのようなtaskが優先度の高いものだと、別の依存を通して待っている依存元を押し上げ、実際には流れないtaskのためにclaim順が入れ替わっていた。task 304が実装でこれを除いた。

## Decision

1. **効く優先度の継承元から、`abandoned`で閉じたgoalに依存するtaskを除く。** そのtaskは`ready`でも永久にclaimされず、依存元を先に流しても何も解放しないため。`draft`・`canceled`・`completed`のtaskとdraftのgoalのtaskを除くのと同じ理由で、流れる予定の無いtaskが依存元を押し上げてclaim順を乱さないようにする。
   - 除くかどうかは待っている側のtaskの依存先goalの状態で決める。依存先のgoalがまだ開いている（`achieved`で閉じうる）間は今までどおり継承する。
   - 解放数（`unblocks`）の数え方とclaim順の比べ方、その他の継承の規則は変えない。推移的な扱い（`abandoned`のgoalに依存するtaskを経由して待っている`ready`のtaskなど）はこのADRの範囲外。

ADR-0049の決定4の「効く優先度」の継承しないtaskの列挙に、`abandoned`で閉じたgoalに依存するtaskを足す。決定4のそれ以外は変えない。

## Alternatives

- **除かない（決定4のまま）**: 永久にclaimされないtaskの優先度が依存元に効き続け、他の流れるtaskよりその依存元が先にclaimされる。依存元の作業は、待っているtaskが流れない以上その優先度に見合う価値を生まない。そのtaskは人がplannerで依存を外すかcancelするまで残るので、その間ずっとclaim順が乱れる。
- **そのtaskを自動でdraftに戻して継承元から外す**: 継承の規則は変えずに済むが、依存を外すか、cancelするか、続きを新しいgoalで登録し直すかは計画の判断で、runtimeは閉じたgoalに取り残された依存を知らせるだけで状態を変えない（`dependency_stranded`と同じ方針）。継承のためだけにtaskの状態を変えない。

## Consequences

- `abandoned`で閉じたgoalに依存するtaskが優先度を上げていても、その依存元は自分と他の流れるtaskの優先度でclaimされる。
- 人がそのtaskのgoal依存を外してclaimできる形に直せば、その時点から継承が戻る。
- 継承の実装（関数名と入力）は[Domain model](../design/domain-model.md)の`graph`の項が持つ。
