---
id: adr-t3154-1
type: adr
title: resumes_per_runは上限に数えるresumeだけの平均にし、数えないresumeは目標の無いuncounted_resumes_per_runに分ける（ADR-0051決定1をamends）
status: accepted
created: 2026-10-23
updated: 2026-10-23
accepted_on: 2026-10-23
amends:
  - adr-0051 decision 1
owners:
  - hisamekms
tags:
  - runtime
  - observer
related:
  - adr-0051
  - adr-0047
  - adr-0068
  - design-supervisor-lifecycle-commands
---

# ADR-t3154-1: resumes_per_runは上限に数えるresumeだけの平均にし、数えないresumeは目標の無いuncounted_resumes_per_runに分ける（ADR-0051決定1をamends）

## Context

[ADR-0051](0051-kpi-time-series-report-and-push.md)決定1の表は、手戻りのKPI `resumes_per_run`を「終わったrunのresumeの数の平均」と決めた。
resumeには、runtimeがresumeの上限（[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定24）に数えないものがある。
他のtaskの着地の後にlanding recheckが見つけた衝突だけのresume（[ADR-0068](0068-recheck-waiting-runs-after-each-landing.md)決定5）や、holdの解除の後のresumeで、runが起こした手戻りではなく必要な作業である。
2026-10-21〜22のresume 64件の約半数がこれで、`resumes_per_run`は2026-10-01から目標を割り続けた（finding 137）。
また`stats`のresumeの理由ごとの内訳は、landing recheckがparkしたrunのresumeの理由を読めず不明にしていた。

## Decision

1. **`resumes_per_run`は上限に数えるresumeだけの平均にする（ADR-0051決定1の表の`resumes_per_run`の行をamends）。** 終わったrunのresumeのうち、runtimeが上限に数えたものの数の平均とし、分母は終わったrunのまま。上限に数えるかを記録する前のresumeは数える。理由ごとの内訳は数えないresumeも含めて出す。
2. **数えないresumeは`uncounted_resumes_per_run`として出し、目標を置かない。** 上限に数えないresumeの数の平均を、`resumes_per_run`と同じ分母で手戻りのKPIの隣に出す。必要な衝突の作業の量を見えるままにするための値で、良い向きを持たず、目標割れと悪化の判定とKPIのfindingの対象にしない。
3. **landing recheckがparkしたrunのresumeは、その失敗の理由に帰属させ、不明にしない。** 失敗が理由を持たなければlanding recheckそのものを理由にする。理由に帰属させるのはrunをresumeへ回した失敗だけで、何もparkしなかった失敗は帰属先にしない（supervisorがresumeの理由を選ぶ規則と同じ）。
4. 変えないもの: `resumes_per_run`の目標値、runtimeのresumeの上限の数え方、`first_pass_rate`（resumeの数えるか否かに依らずresumeの無いrunだけを一度で通ったとする）。

## Alternatives

- **目標値を緩める**: 数え方の誤りを残し、必要な作業が増減すると手戻りの値も動く。
- **数えないresumeをKPIから消す**: 衝突の作業が増えたことが見えなくなる。
- **`uncounted_resumes_per_run`にも目標を置く**: 衝突の量は他のtaskの着地の重なりで決まり、runの手戻りを測る目標にならない。

## Consequences

- `resumes_per_run`は決定の前の期間とも同じ規則で再導出され（ADR-0051の原則）、過去の期間の値も下がる。
- 残る数えるresumeは主にreviewの差し戻し（人の差し戻しを含む）で、差し戻しを減らす計画が扱う。
- どのeventのどの欄で数えるか・帰属させるかの今の姿は、runの集計とKPIの導出のコードのdoc commentが持つ。
