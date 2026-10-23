---
id: adr-t3240-1
type: adr
title: reviewのプログラムが差分のtreeで流して通したverifyのコマンドは、workerの手元の実行の根拠をreceiptに求めず、reviewはその欠けで差し戻さない。プログラムが流さなかったverifyは今どおり手元で流す（ADR-t2589-1決定5をamends）
status: accepted
created: 2026-10-24
updated: 2026-10-24
accepted_on: 2026-10-24
amends:
  - adr-t2589-1 decision 5
owners:
  - hisamekms
tags:
  - review
  - testing
  - conventions
related:
  - adr-t2589-1
  - adr-t2588-1
  - adr-t1895-1
  - adr-t1895-2
  - adr-t1925-1
  - development-local-checks
---

# ADR-t3240-1: reviewのプログラムが通したverifyはworkerの手元の根拠に求めない

## Context

workerは手元で、taskのverifyのうちcoverageの関門と全体の`cargo test --locked`以外を流す（[手元の検証](../development/local-checks.md#workerの手元の検証)）。
receipt-evidenceのagentと全体のreviewは、その実行の根拠（記録か`tests`のevidence）を見て、無ければ差し戻していた。

docsを変えるtaskのverifyに付く文書の検査は、`dagq.toml`の`[review.programs]`にも挙がっている。
差分がそのprogramの`paths`に当たるrunでは、programのreviewがagentのjobより前にreviewするcommitのtreeでそのscriptを流し、落ちればagentを起動せずにworkerへ差し戻す（[ADR-t1895-1](2026-10-06-t1895-1-review-stage-runs-agent-and-program-jobs-in-a-fixed-shape.md)、[ADR-t1895-2](2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)決定2・3）。
agentが動いている時点で、そのコマンドは同じtreeで通っている（差分がそのscriptを変えたときを除く。決定3）。

それでもreceipt-evidenceは、これらの手元の実行の根拠が無いことだけ（か主に）で差し戻していた（finding 242、1つのtaskで3回を含む）。
手元の根拠はprogramのreviewの結果と重複し、差し戻しは着地の品質を上げず待ちとreviseの回数だけを積む。

## Decision

1. **reviewのプログラムが流して通したverifyは、workerの手元の根拠に求めない。**
   taskのverifyのコマンドが`[review.programs]`のprogramと同じscriptを同じ引数で流すもので、runの差分（programのreviewが選ぶのと同じ、receiptの範囲の変えたpath）がそのprogramの`paths`に当たるときは、そのverifyの手元の実行の根拠をreceiptに求めない。
   receipt-evidenceのagentと全体のreviewは、そのverifyの記録もevidenceも無いことを差し戻しの理由にしない。
   workerはそのverifyを手元で流さなくてよい（流してもよい）。落ちる変更は、programのreviewがagentの前に差し戻す。
2. **プログラムが流さなかったverifyは今どおり。**
   programに無いverifyと、programに挙がっていても差分が`paths`に当たらず流れなかったverifyは、今どおりworkerが手元で流し、根拠は記録かevidenceで見る（[ADR-t2589-1](2026-10-17-t2589-1-local-check-runs-are-recorded-by-project-scripts-and-matched-to-the-receipt-tree.md)決定5・7）。
3. **どれが流れたかは設定と差分から判じる。**
   reviewの資料は、programのreviewでどのprogramが流れて通ったかを今は載せない。
   agentは、landing branch（main）の`dagq.toml`の`[review.programs]`の`script`・`args`・`paths`とrunの差分のpathから判じる。
   programと設定とscriptは着地したcommitから読まれる（ADR-t1895-2決定2）。
   差分がそのprogramの`[review.programs]`の設定かそのscriptを変えたrunでは、programが流したのは変える前のscriptで、verifyが流す差分のscriptは着地まで流れないので、決定1に当てず決定2の扱いにする。
   runtimeは変えず、資料にprogramの結果を載せることはこのADRでは決めない。

ADR-t2589-1決定5のうち変わるのは、workerの手書きのevidenceに求めずreviewが欠けで差し戻さない事項に、決定1のverifyを足すことだけである。
同ADRの決定3（記録の欠けでreceiptの受理を変えず新しい関門を作らない）・決定7（要る関門の記録の欠けの扱いはLLMが判じる）は変えない。

## Alternatives

- **reviewの資料に流れて通ったprogramの一覧を載せる**: agentが判じなくて済むが、runtimeの変更が要る。設定と差分から判じる形は今の資料で足り、判じ違いはprogramが流れなかった側（今どおり根拠を求める側）に倒れる。
- **手元の根拠を求め続け、記録を書く指示を強める**: programのreviewが同じtreeで既に通しているので、根拠の書き漏れの差し戻しは待ちを積むだけになる。
- **verifyから文書の検査を外す**: `integrate`の検証はrebase後のtreeで流し直し、programのreviewが通っても省かれない（ADR-t1895-2決定1）。着地の関門は保つ。

## Consequences

- 差分が`paths`に当たる文書の検査の、手元の根拠の欠けによる差し戻しが無くなることを期待する。`paths`の狭いprogram（`docs/design/`だけを見るものなど）は、そこに触れないtaskでは流れないので、今どおり手元の根拠が要る。
- 着地の関門は変わらず、`integrate`がrebase後に1回verifyを流す。
- workerが手元で流さなかった文書の検査が落ちれば、programのreviewの差し戻しになり、runを通した自動の手直しの上限に1回と数える。
- [ADR-t1925-1](2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)決定6（着地の検証が軽くなってもworkerの手元の検証は変えない）はamendsしない。あの決定は着地の検証の変更とworkerの規則を切り離したもので、このADRは別の理由（programのreviewとの重複）で手元の範囲を狭める。
- [ADR-t2588-1](2026-10-17-t2588-1-worker-local-fmt-and-clippy-are-not-receipt-evidence-and-not-reviewed.md)はfmt・clippyの根拠を求めないことを決めたもので、このADRとは対象が別で食い違わない。
