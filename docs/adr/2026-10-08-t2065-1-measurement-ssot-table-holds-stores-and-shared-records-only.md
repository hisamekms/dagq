---
id: adr-t2065-1
type: adr
title: 計測の設計書の「SSOTとビュー」に行を足すのは計測の層の論理ストアと、runtime・CI・scripts/が続けて書き他の仕組みが読む共有の記録に限り、docs/plansの1回きりや週次の見直しの測定の出力はそのplansの文書が区分と作り直せる範囲を持つ（ADR-t1662-2決定9をamends）
status: accepted
created: 2026-10-08
updated: 2026-10-08
accepted_on: 2026-10-08
amends:
  - adr-t1662-2 decision 9
owners:
  - hisamekms
tags:
  - observability
  - documentation
related:
  - adr-t1662-1
  - adr-t1662-2
  - adr-t1942-1
  - design-measurement
---

# ADR-t2065-1: 「SSOTとビュー」の表に足す範囲を計測の層のストアと共有の記録に限る

## Context

[ADR-t1662-2](2026-10-04-t1662-2-measurement-stores-ssot-and-views.md)決定9は「新しいストアやビューを足すtaskは、同じ変更で計測の設計の『SSOTとビュー』の節に区分（SSOT・ビュー・材料）と今のアダプタを書く」と決めた。
決定9は、どのストアやビューが対象かを限っていない。
そのため`docs/plans/`の1回きりの測定（scriptとCSV）を足すtaskも、[文書の規則](../development/documents.md)の「design」とplan reviewに求められて表に行を足し続けた。
それらの出力は計測の層（台帳と統計）が読み書きせず、他の仕組みも読まない。
行は測定ごとの入力・材料・作り直せない条件の細部を持ち、1行の予算（[ADR-t1942-1](2026-10-07-t1942-1-design-docs-in-four-layers-with-size-budgets.md)）を超える行も出た。
設計書は計測の層の今の予定を持つ地図で、測定ごとの細部はその測定の`docs/plans/`の文書が既に持つ。

## Decision

1. **ADR-t1662-2決定9だけをamendsする。** 「SSOTとビュー」の節に行（中身・区分・今のアダプタ）を足すか直すのは、次のどちらかを足すか変えるtaskに限る。
   - 計測の層の論理ストア（StateStore・EventStore・SessionStepStore・NodeSampleStore・LedgerStore・ReportStore）。
   - runtime・CI・`scripts/`が続けて書き、他の仕組みが読む共有の記録（例: 統合テストのcoverageの対応表、着地の検証の対応表のcache）。
2. **`docs/plans/`の1回きりや週次の見直しの測定の出力（scriptとCSV）は設計書に行を足さない。** その測定のplansの文書が、入力と出力の区分（SSOT・ビュー・材料）と、作り直せる範囲（材料が消えると同じ表では作り直せない条件を含む）を持つ。
3. 決定9の「区分を書く」こと自体と、ADR-t1662-2の決定1〜8は変えない。

## Alternatives

- **今のまま全ての測定の行を足す**: 測定のたびに設計書が太り、計測の層の予定が細部に埋もれる。同じ内容をplansの文書と二重に持ち、食い違いの元になる。
- **設計書を消すか凍結する**: 設計書は計測の作り直しのacceptedの決定（ADR-t1662-1〜3）の今の予定を持ち、後続のtaskが直す先なので取らない。
- **測定の行を別の索引の文書に集める**: 読み手は各測定のplansの文書から入るので、索引は写しになるだけで新しく持つものが無い。
- **共有の記録も表から外す**: CIや着地の検証が続けて書き読む記録は、計測の層と同じく他の仕組みが頼る区分なので、設計書で見えるのがよい。

## Consequences

- 設計書の表は論理ストアと共有の記録だけになり、測定のtaskは設計書を変えない。
- 測定のplansの文書は区分と作り直せる範囲を自分で持つ。plan reviewは測定のtaskに設計書の行を求めない。
- 測定の出力が他の仕組みに続けて読まれるようになったら、そのときに共有の記録として表に行を足す。
