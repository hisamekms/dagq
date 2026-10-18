---
id: adr-t2751-1
type: adr
title: 1回のplan reviewがreopenできるreadyのtaskに上限を置き、超えたら1件もreopenせず検査中のproposalを互換を守るよう差し戻す（ADR-0047決定14をamends）
status: accepted
created: 2026-10-18
updated: 2026-10-18
accepted_on: 2026-10-18
amends:
  - adr-0047 decision 14
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - planner
related:
  - adr-0047
  - adr-0041
  - design-supervisor-lifecycle-plan-review
---

# ADR-t2751-1: 1回のplan reviewがreopenできるreadyのtaskに上限を置き、超えたら1件もreopenせず検査中のproposalを差し戻す

## Context

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定14（ADR-0041決定14を引き継ぐ）は、plan reviewがverdictの`reopen`に書いたreadyのtaskを、件数に関わらず1件ずつ`submitted`に戻し、そのtaskだけのproposalを作ってruntimeのplannerに直させる。

2026-10-18、あるproposalのplan reviewがreadyのtask 163件をreopenし、そのうち154件は同じ1行の理由だった。
それぞれが自分のproposalでruntimeのplannerを待ち、plannerの枠が2つなのでreviseは1時間を超えて待ち（`planner_unresponsive`が134件）、claimの候補は139件から5件に減った。
人の判断は「互換は壊す側で守る、個別のreviseは回さない」だった。

ふつうのreopenは1〜2件で、同じ件の163件のうち中身のある理由は9件だった。

## Decision

1. **1回のplan reviewがreopenできるreadyのtaskは数件（上限）までにする。** 上限以下のreopenは今までどおり1件ずつ適用する。
   上限はdagqの汎用の既定（外部のprojectでも同じ理屈）で、projectの設定にしない。値と理由はコードの定義のそばに置く。
2. **上限を超えたreopenは1件も適用せず、検査中のproposalを差し戻す。** 多くの登録済みのtaskを壊す変更は、壊す側のproposalが吸収する（登録済みのtaskが動き続けるよう互換を自分の側で守るか、少しのtaskのreopenで済むよう変更を分ける）。
   - 各項目はreopenしなかった理由（件数と上限）とともに記録する。
   - `pass`は`revise`にし、判定を変えたことを記録する。この`revise`もreviseの回数（ADR-0047決定11）に数え、その上限を過ぎていれば`concern`にする。確信度の高い`concern`を`pass`として適用するときも同じで、その`concern`は差し戻しとして適用した（上限を過ぎていれば人に残した）と記録する。`revise`は理由を保ったまま差し戻しの理由を1つ足す。`concern`は`concern`のままで、差し戻しの理由をaskの文に足す。
3. **plan review jobには上限を伝える。** 変える必要のある少しのtaskだけをreopenし、多くのtaskが壊れるときはreopenせずこのproposalを互換を守るよう差し戻す。

上限以下のreopen・`pass`・`revise`・`concern`のほかの適用（eventの種類と順、1つのtransactionで適用すること）は変えない。

## Alternatives

- **上限を`dagq.toml`の設定にする**: 理屈がprojectに依らず、利用者が決める根拠を持たない。
- **上限までだけreopenし、残りを捨てる**: どれを選ぶかの根拠が無く、壊れたまま残るtaskがclaimされうる。一括の書き換えの兆しは検査中のproposalの問題なので、そこへ返す。
- **同じ理由のreopenを1つのproposalにまとめる**: 壊す側が互換を守れば要らない作業を、なおplannerに回すことになる。

## Consequences

- 一括のreopenでplannerの枠とclaimの候補が長く塞がることがなくなる。
- 本当に多くのtaskを直す必要がある変更は、分けて出し直すか、人がconcernで判断する。
- 上限で差し戻したproposalがまた上限を超えるreopenで`pass`を受け続けても、reviseの回数の上限で人に上がり、差し戻しが繰り返されない。
