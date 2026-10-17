---
description: このrepositoryの手元の検証とreceiptの根拠を検査する
tools: [read, grep, glob]
---

手元の検証の実行は、先に資料`review.md`の記録の節（「Local check records」のreceiptのcommitのtreeと一致した記録と、一致しない・壊れた記録の件数）と関門の節（「Local gates」の関門ごとの要否）で見る — [docs/development/local-checks.md「実行の記録とreceiptのevidence」](../../../docs/development/local-checks.md#実行の記録とreceiptのevidence)。
記録と判定で示せる事項（流したコマンド・exit status・stressの周回と結果・時間の関門の結果・変えたtestが無いので関門を省いたこと）は、その節が示していればevidenceに無いことでは差し戻さない。節が無いか、節が示さない事項はevidenceで見る。
受け入れ条件の対応づけと、testの範囲の選び方の妥当さ（範囲を選んだ理由・流したtestの件数）は引き続き見る。要る関門の記録が無いか失敗しているときは、evidenceの流せなかった理由と合わせて判じる（ADR-t2589-1決定5・7）。

- A-057・A-058・A-066・A-093・A-219: workerの手元の検証の適合性 — [docs/development/local-checks.md「workerの手元の検証」](../../../docs/development/local-checks.md#workerの手元の検証)。fmt・clippyの実行の有無と根拠は審査しない（関門は着地の検証）。
- A-059・A-060・A-061・A-062・A-065・A-220: testの範囲の適合性 — [docs/development/local-checks.md「testの範囲」](../../../docs/development/local-checks.md#testの範囲)。
- A-063: 全体を比べるtestの適合性 — [docs/development/local-checks.md「全体を比べるtest」](../../../docs/development/local-checks.md#全体を比べるtest)。
- A-067・A-068・A-218・A-225: e2eを流さないの適合性 — [docs/development/local-checks.md「e2eを流さない」](../../../docs/development/local-checks.md#e2eを流さない)。
- A-071・A-073・A-075・A-076・A-077・A-078・A-223: stressの適合性 — [docs/development/local-checks.md「stress」](../../../docs/development/local-checks.md#stress)。
- A-080・A-081・A-221: resumeでの再現の適合性 — [docs/development/local-checks.md「resumeでの再現」](../../../docs/development/local-checks.md#resumeでの再現)。
- A-257: 受け入れ条件の対応づけの適合性 — [docs/development/local-checks.md「受け入れ条件の対応づけ」](../../../docs/development/local-checks.md#受け入れ条件の対応づけ)。
