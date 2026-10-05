---
id: adr-t1811-1
type: adr
title: 自分の優先度を持たないtaskは状態によらず所属のgoalの今の優先度に追随し、claimの後も値を凍らせず、claimの順に効くのはreadyのtaskの次のclaimだけにし、claimの時の優先度はrun_claimedの記録で読む（ADR-t1639-1決定3をamends）
status: accepted
created: 2026-10-05
updated: 2026-10-05
accepted_on: 2026-10-05
amends:
  - adr-t1639-1 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - goal
  - priority
related:
  - adr-t1639-1
  - adr-0049
  - design-domain-model
---

# ADR-t1811-1: 自分の優先度を持たないtaskは状態によらずgoalの今の優先度に追随し、claimの後も凍らせない（ADR-t1639-1決定3をamends）

## Context

[ADR-t1639-1](2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)決定2は、taskの優先度を個別の指定があればそれ、無ければ所属のgoalの優先度にした。決定3は、goalの優先度の変更が個別の指定の無い未着手（`draft`・`submitted`・`ready`）のtaskの次のclaimに効き走っているrunを止めないとしたうえで、「claimされた後（`in_progress`以降）のtaskの優先度は変えない」と書いた。

決定2を実装したtask 1640は、基の優先度を読むたびに個別の指定と所属のgoalの今の優先度から求める（1か所の純粋関数）。そのため`in_progress`以降のtaskも、goalの優先度を変えれば表示の値が変わる。task 1640のreviewがこの食い違いを挙げ（ask 427）、2026-10-05に人がrequest 25で「goalの優先度を変えたら、自分の優先度を持たないtaskはgoalに追随するのが実務上自然。この動きで行きたい」と決めた。このADRはそれを決定にし、決定3のうちclaimの後のtaskの優先度の扱いだけを改める。決定3の残り（効くのは次のclaimだけ、走っているrunは止めない）は変えない。

## Decision

1. **自分の優先度（個別の指定）を持たないtaskの基の優先度は、状態（`draft`・`submitted`・`ready`・`in_progress`・`completed`・`canceled`）によらず、読むたびに所属のgoalの今の優先度から求める。** claimの時やgoalの変更の時に値をtaskへ写して凍らせない。ADR-t1639-1決定3の「claimされた後（`in_progress`以降）のtaskの優先度は変えない」はこれに置き換える。
2. **goalの優先度の変更がclaimの順に効くのは、今どおり`ready`のtaskの次のclaimだけで、走っているrunは止めない。** [ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定4の「次のclaimにだけ効く」「割り込みで止めない」を保つ。`in_progress`以降のtaskの`show`・`goal show`・`graph`・`list`が出す優先度はgoalの今の値で、retryで`ready`に戻ったtaskは新しい値で並ぶ。
3. **そのrunがどの優先度でclaimされたかは、taskの表示ではなく`run_claimed`の記録で読む。** claimの時の値と出どころの記録はtask 1664が足す（欄の形は`docs/design/`）。

## Alternatives

- **claimの時かgoalの変更の時に、その時の値を個別の指定としてtaskに写す（task 1640のworkerのfollow-upが挙げた案）**: retryで`ready`に戻った後も、taskが自分で置いていない値を持ち続け、goalの優先度を変えても追随しない。個別の指定は「goalの中の一部のtaskだけを上げ下げする」ためのもの（ADR-t1639-1決定2）で、その意味が崩れる。claimの時の値を残したいだけなら、runの記録（3）で足りる。
- **claimの後の表示だけ凍らせた値を別に持つ**: 値の置き場が増え、retryの後にどちらで並べるかの規則が要る。表示はgoalの今の値、claimの時の値はrunの記録と分ければ要らない。

## Consequences

- runtimeは変わらない（task 1640の実装がすでに1・2のとおり動く）。`docs/design/domain-model.md`はこのADRを引いて1・2を書く。
- `in_progress`以降のtaskの優先度の表示は、claimの時の値ではなくgoalの今の値になる。走っているrunがどの値でclaimされたかを知りたいときは`run_claimed`を見る。
