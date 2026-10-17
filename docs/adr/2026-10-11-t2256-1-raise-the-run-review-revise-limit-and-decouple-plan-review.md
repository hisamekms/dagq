---
id: adr-t2256-1
type: adr
title: runのreviewのreviseを生きているsessionに返す回ごとの上限を1つ上げ、plan reviewの差し戻しの上限はその値と数え方のままrunのreviewの上限と連動させない（ADR-0027決定2・ADR-0050決定1・3・4・ADR-0047決定11をamends）
status: superseded
created: 2026-10-11
updated: 2026-10-11
accepted_on: 2026-10-11
superseded_by: adr-t2599-1
superseded_on: 2026-10-17
amends:
  - adr-0027 decision 2
  - adr-0050 decision 1
  - adr-0050 decision 3
  - adr-0050 decision 4
  - adr-0047 decision 11
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
related:
  - adr-0027
  - adr-0050
  - adr-0047
  - adr-t451-1
  - adr-t1091-1
  - adr-t1942-1
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-plan-review
---

# ADR-t2256-1: runのreviewのreviseの回ごとの上限を1つ上げ、plan reviewの上限と連動させない（ADR-0027決定2・ADR-0050決定1・3・4・ADR-0047決定11をamends）

> **置き換え済み（2026-10-17）**: このADRの決定は現在有効ではない。現行の決定は[ADR-t2599-1](2026-10-17-t2599-1-run-review-revises-counted-across-the-run-and-judged-by-code-findings-past-the-limit.md)を読む。

## Context

runのreviewは、`revise`のverdictと、concernの`high`・`reason_category`無しの`send_back`（reviseと同じく上限に数える）を、reviewの回（ADR-0050決定2の区切りの後）ごとに上限の回数まで生きているworkerのsessionに返す（ADR-0027決定2、回ごとに数えることはADR-0050決定1）。上限を使い切った後のreviseとconcernの`send_back`は`approve_landing`のask（`escalated_because: revise_limit`）になる。

2026-10-11に、「review still asks for changes after 2 revises」の`approve_landing`のaskが4件（ask 716・720・721・722、task 1820・1685・2172・1905）人に上がった。残っていた指摘は文書のずれやtestの不足で、人は4件とも`send_back`と答えた。上限の後の判断が人に要らなかったので、人はrunのreviewの自動の手直しの上限を3にしたいと言った（request 91）。

plan reviewの差し戻しの上限（`MAX_PLAN_REVISES`）は、ADR-0047決定11（ADR-0041決定11をADR-0044経由で引き継いだもの）が「runのreviewの`MAX_REVISE_ATTEMPTS`と同じ」とし、ADR-0050決定4がその値と結び付きを変えないとしたので、コードでもrunのreviewの上限から作られていた。人の言葉はrunのreviewについてのもので、plan reviewに触れていない。

## Decision

1. **runのreviewのreviseの回ごとの上限を1つ上げる。** 値の置き場は`MAX_REVISE_ATTEMPTS`（定義のそばのdoc comment）で、数値はそこが持つ（ADR-t1942-1）。ADR-0027決定2の上限の値と、ADR-0050決定1の「上限の値（2）はADR-0027の決定2のまま変えない」はこの値に置き換わり、ADR-0050決定3の「reviseの総数は回の数の2倍を超えない」の倍数もこの上限になる。
2. **変えるのは上限の値だけにする。** 上限に達した後のreviseとconcernの`send_back`が`approve_landing`のaskになること、concernの`land`・`scope`・`discard`・`low`・推奨なしの判断がreviseの残数に依らないこと（ADR-t451-1決定3。`high`・`reason_category`無しの`land`は上限の後も適用する）、回の区切り（ADR-0050決定2）、verdictの3値は変えない。
3. **plan reviewの差し戻しの上限は、値もproposalの全期間で数えることも変えず、runのreviewの上限と連動させない。** 値の置き場は`MAX_PLAN_REVISES`で、`MAX_REVISE_ATTEMPTS`から作らない。ADR-0047決定11の「runのreviewの`MAX_REVISE_ATTEMPTS`と同じ」と、ADR-0050決定4の`MAX_PLAN_REVISES`と`MAX_REVISE_ATTEMPTS`の結び付きを保つとした部分だけを外し、ほかは変えない。

## Alternatives

- **plan reviewも同じだけ上げる（連動を保つ）**: ADR-0050のAlternativesは、plannerの書き直しは計画の意図がずれやすいのでreviseの往復を短く保つ方を選び、plan reviewで同じ問題が数字で見えたら別のADRで決めるとした。今回の4件はどれもrunのreviewで、plan reviewの上限で人に上がる件が増えたという材料は無い。人の言葉もrunのreviewについてのものなので、連動を外して値を保つ。
- **上限を置かずに人へ上げない**: 上限は人の知らないまま往復が続くのを止める柵で、回の区切りとresumeの上限と合わせて往復の総数を抑える（ADR-0050決定3）。外す材料は無い。
- **ADR-0027・ADR-0050・ADR-0047を丸ごと置き換える**: どれも番号付きの決定を複数持ち、変えるのはその一部なのでamendsにする（ADR-t1091-1）。

## Consequences

- 回の中で人に上がる前のreviseの往復が1回増え、文書のずれやtestの不足のような指摘は人を経ずに直りやすくなる。
- 往復が1回増える分、runがslot（とworkerのsession）を占める時間が伸びうる。reviseの総数の上限は回の数の倍数として同じだけ増える。
- runのreviewとplan reviewの上限は別々に変えられる。片方を変えるときにもう片方に触れない。
