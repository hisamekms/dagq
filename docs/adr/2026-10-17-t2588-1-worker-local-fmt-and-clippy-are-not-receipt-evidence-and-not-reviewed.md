---
id: adr-t2588-1
type: adr
title: fmt・clippyの関門は着地の検証のままにし、workerが手元でfmt・clippyを流したかをreceiptの根拠に求めずreviewで審査しない。reviewの段でfmt・clippyを機械で流すものは今は足さない
status: accepted
created: 2026-10-17
updated: 2026-10-17
accepted_on: 2026-10-17
owners:
  - hisamekms
tags:
  - review
  - testing
  - conventions
related:
  - adr-t1925-1
  - adr-t1895-2
  - development-local-checks
---

# ADR-t2588-1: workerの手元のfmt・clippyはreceiptの根拠に求めずreviewで審査せず、関門は着地の検証のままにする

## Context

workerは手元で`cargo fmt --all --check`と`cargo clippy`を流す指示を[手元の検証](../development/local-checks.md#workerの手元の検証)の「workerの手元の検証」で受け、receipt-evidenceのsubagentと全体のreviewは、receiptの`tests`のevidenceにその実行の根拠があるかを審査していた。

2026-10-11からrunのreviewの1回で通った率が0.2〜0.5から0〜0.15に落ちた。
10/16以降のreview 113回のうちsend_backが71回、そのうちreceipt-evidenceの指摘を含むものが38回（単独11・混在27）だった。
指摘の大半は書式（fmt・clippyの根拠が無い、stressやitの時間の関門を省いた旨と理由が無い）で、docsだけ・コメントだけの差分にも付く。

一方、10/03以降の約2週間で着地の検証の失敗からresumeになったのは、`landing-it.sh` 12回・`check-design-docs.sh` 8回・llvm-cov 5回に対し、clippy 1回・fmt 0回だった。
fmt・clippyは着地の検証（`integrate`がtaskの`--verify`のfmt・clippyをrebaseの後に流す）で足りている。

## Decision

1. **fmt・clippyの関門は着地の検証のままにする。** `integrate`がtaskの`--verify`のfmt・clippyをrebaseの後に流すことを関門とし、新しい関門を作らない。
2. **workerが手元でfmt・clippyを流したかは確かめない。** receiptにその実行の根拠を求めず、review（receipt-evidenceのsubagentと全体のreview）はその有無を審査せず、根拠が無いことで差し戻さない。
   workerが手元でfmt・clippyを流す規則は指示として残す（着地の検証で落ちてresumeになる往復を減らすため）。
3. **reviewの段でfmt・clippyを機械で流すものは今は足さない。** 着地の検証でfmt・clippyが落ちる件数が増えたら見直す。

testの範囲・stress・itの時間の関門・e2e・resumeでの再現・受け入れ条件の対応づけの根拠と審査は、このADRでは変えない。

## Alternatives

- **reviewの段でfmt・clippyを機械で流す（review programなど）**: review programはbuild・testを流さない速い形式の検査に限る約束（[ADR-t1895-2](2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)決定1）に当たり、cargoを流す検査は挙げる検査の時間の目安（3回の最大が10秒以下）を超える。fmt・clippyは着地の検証で足りており、関門を2つにする利点が無い。
- **根拠の審査を続け、書式を整える指示を足す**: 着地の検証が同じことを確かめるので、根拠の書き漏れで差し戻す往復は着地の品質を上げず待ちだけを積む。

## Consequences

- receipt-evidenceのfmt・clippyの根拠の指摘による差し戻しと、その待ちが減ることを期待する。
- workerが手元でfmt・clippyを流さなかったときは、着地の検証で落ちてresumeになる。その件数が増えれば決定3を見直す。
- [ADR-t1925-1](2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)決定6（着地の検証が軽くなってもworkerの手元の検証は変わらない）とは食い違わない。workerが流す指示は残り、変わるのはreceiptの根拠とreviewの審査だけである。
