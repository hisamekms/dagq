---
id: adr-t768-1
type: adr
title: 着地の検証のnextestは落ちたtestを1回だけ流し直してFLAKYを見分けるが検証は失敗のままにし、落ちたtestが全てFLAKYならworkerをresumeせずに着地を1回やり直す（ADR-0076決定2をamends）
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
amended_by:
  - adr-t1039-1
amends:
  - adr-0076 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - integrate
  - testing
related:
  - adr-0076
  - adr-t639-1
  - adr-t598-1
  - design-supervisor-lifecycle-integrate
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle-integrate-errors
---

# ADR-t768-1: 着地の検証で落ちたtestを1回流し直してFLAKYを見分け、落ちたtestが全てFLAKYならresumeせずに着地を1回やり直す（ADR-0076決定2をamends）

## Context

[ADR-0076](0076-run-the-coverage-gate-tests-with-nextest.md)の決定2は、`.config/nextest.toml`のretries（落ちたtestの再実行）を既定の0のままにし、不安定なtestをretryで隠さないと決めた。そのため、変更と無関係な不安定なtestが`integrate`の検証（`cargo llvm-cov nextest`）で1回落ちると、コードの失敗（`test_failure`）と区別できず、runは`needs_session`になってworkerのsessionがresumeされる。2026-09-27は着地の検証48回のうち3回がこれで落ち、resumeから着地のやり直しまで1件7〜16分かかった（goal 37）。

[ADR-t639-1](2026-09-27-t639-1-retry-verification-failures-of-the-host-once.md)はhostの分類（disk_full・killed・timeout）をresumeせずに1回やり直すと決めたが、不安定なtestの失敗は`test_failure`で、その外にある。

cargo-nextest（0.9.146）は`retries`と`flaky-result`を持ち、`flaky-result = "fail"`なら流し直して通ったtestをFLAKY（`FLKY-FL`）と出したうえで、run全体は失敗のままにする。

## Decision

1. **着地の検証のnextestは、落ちたtestを1回だけ流し直す。流し直して通ったtestはFLAKYと判定するが、検証の結果は失敗のままにする（`flaky-result = "fail"`）。** 不安定なtestをretryで隠さないというADR-0076決定2の趣旨は保ち、見分けるためだけに流し直す。設定は`.config/nextest.toml`に置き、CI・人の手元・workerのstressにも同じく効く（どこでも不安定なtestは失敗として見え、FLAKYの印が付くだけ）。
2. **落ちたtestが全てFLAKYのときは、判定をworkerに返さず、runtimeが扱う。** 検証の失敗の分類（task 467）に`flaky`を足して記録し、FLAKYのtestの名前を残し、不安定なtestの候補（statsの`flaky_candidates`）に1回目から数える。workerのsessionはresumeせず、同じrunの着地をもう1回やり直す（同じ着地の枠の中で、rebase後の同じcommitに対して検証を最初から流し直す）。FLAKYでない失敗が1つでもあれば今までどおりresumeする。
3. **やり直しはrunごとに1回まで。** やり直した着地がまた落ちたら（FLAKYだけでも）、今までどおり`needs_session`にしてresumeする。同じ不安定なtestが続けて落ちるなら、それを直すのはworkerではなく不安定なtestを直すtaskで、observerの`flaky_test`のfinding（task 642）がそれを拾う。

## Alternatives

- **retriesを0のままにする（これまで）**: 不安定なtestの失敗がコードの失敗と区別できず、1件7〜16分のresumeを使い続ける。
- **`flaky-result = "pass"`で通す**: 着地は速いが、不安定なtestが記録にも関門にも残らず、直されない。ADR-0076決定2の懸念そのもの。
- **retriesを2回以上にする／やり直しを何度でも行う**: 落ちたときの延びが増え、続けて落ちるtestは結局直すべきものなので、1回で見分ければ足りる。
- **flakyもhostの分類（ADR-t639-1）に入れ、人に知らせる**: 不安定なtestは人がhostを直すものではなく、1回のやり直しで通る見込みが高い。人を呼ぶ前にやり直す。

## Consequences

- 検証が通るとき（約94%）は時間が増えない。落ちたときは落ちたtest 1本分の流し直しだけ延び、全てFLAKYなら着地のやり直しの分（検証をもう1周）が増えるが、resumeの7〜16分は使わない。
- FLAKYのtestは1回目から名前と印付きで記録され、observerが`flaky_test`のfindingにできる。
- eventの名前・payloadの欄・reasonのcode・statsの欄・logの読み方は[`integrate`](../design/supervisor-lifecycle/integrate.md)・[`stats`](../design/supervisor-lifecycle/stats.md)・[integrateのerror](../design/supervisor-lifecycle/integrate-errors.md)・[Domain model](../design/domain-model.md)が持つ。
