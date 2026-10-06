---
id: adr-t1428-1
type: adr
title: 挙動や仕様を変えるtaskは変更する時点で更新する文書を決め、plannerが関連文書を書き、workerがreceiptの前に差分と照合してsummaryに更新したpath・節か不要の理由を書き、reviewが照合する。Task・receipt・schemaは変えない
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
amended_by:
  - adr-t1688-1
  - adr-t1942-2
owners:
  - hisamekms
tags:
  - runtime
  - worker
  - planner
  - review
related:
  - adr-t1420-1
  - adr-t947-3
  - adr-0029
  - design-supervisor-lifecycle-prompt
  - plan-review-sendback-reasons
  - plan-follow-up-kinds
---

# ADR-t1428-1: 挙動や仕様を変えるtaskは変更する時点で更新する文書を決め、plannerが関連文書を書き、workerがreceiptの前に差分と照合してsummaryに更新したpath・節か不要の理由を書き、reviewが照合する。Task・receipt・schemaは変えない

> **一部変更（2026-10-07）**: 決定4・5は[ADR-t1942-2](2026-10-07-t1942-2-document-check-and-review-in-both-directions.md)がamendsした（変えた名前で候補を探し探した名前をsummaryに書くが、designを書くのは流れ・境界・不変条件・約束が変わったときだけで、reviewは書き写しと経緯の混入も指摘する。決定4を先にamendsした[ADR-t1688-1](2026-10-04-t1688-1-worker-searches-documents-by-changed-names.md)は置き換え済み）。

## Context

2026-10-03の人の依頼（goal 91、開発速度を上げる施策「コードを変更する時点で、更新する設計文書を決める」）。コードの変更と設計文書のずれ（docs_drift）がrunのreviewの差し戻しになり、更新先を探す作業と直しの往復が増えている。

- 2026-10-02T03:07Z以降のrunのreview（`dagq stats --since 2026-10-02T03:07:27Z`のreview_reasons）で、docs_driftは72 verdict中6件（主の理由3件、reviseの手直しの合計615秒）。
- [review-sendback-reasons](../plans/review-sendback-reasons.md)では、文書・コメントの取り残し（`docs_out_of_sync`）がreviseの主の理由の2番目（主8・全9）。
- [follow-up-kinds](../plans/follow-up-kinds.md)では、receiptのfollow_upのうち`docs_drift`が72件。

workerのpromptは変えた挙動を説明する文書の照合を求めておらず、[ADR-t1420-1](2026-10-03-t1420-1-worker-maps-each-acceptance-criterion-before-the-receipt.md)の対応づけも受け入れ条件が名指す文書しか見ない。

## Decision

1. **Task・receipt・queue DBの構造とschemaは変えず、文書専用の欄もevidenceの種類も足さない。** 文書の扱いは今のdescription・context・acceptance・paths・summaryで書く。
2. **plannerは、挙動や仕様を変えるtaskのdescriptionかcontextに、関連する文書のrepository相対のpath・節と更新が要る理由を書き、acceptanceを変更後の挙動と関連文書の整合を確かめられる形で書く。**
3. **pathsは変えてよい範囲で、要る文書を許可として含める。** 更新必須の一覧としては使わず、文書の一覧に合わせてコードの許可範囲を狭めない。
4. **workerはreceiptの前に、taskが名指す文書と作業中に見つけた関連文書を実際の差分と照合し、pathsの中の古い文書を直し、summaryに更新したpath・節か更新が要らないと判断した理由を短く書く。** pathsの外の文書のずれは`docs_drift`のfollow_upにpathと節を書く。文書の変更が要らないtaskに、日付だけの差分を含む無意味な文書の差分を強いない。
5. **reviewはtaskの記述・実装の差分・関連文書・summaryの根拠を照合する。** 文書の差分があることだけで正しいとはせず、taskが名指したかどうかに関わらず、見つけたずれは今までどおり指摘する。
6. **登録の手順とworker・reviewへの指示の小さな変更から始める。** 重い検査の工程や全体のtestをworkerに足さない。

### ADR-t1420-1との関係

文書の照合はADR-t1420-1の受け入れ条件の対応づけの手順の一部として、その直後に行う。足すのは、受け入れ条件が名指さない関連文書も照合の対象にし、その結果をsummaryに残す点だけで、別の手順を作らない。受け入れ条件が求める文書を直せないときは、ADR-t1420-1のとおり`worker_question`（`--because scope`）かfailedのreceiptにする。resumeとreviseの後のreceiptの書き直しでも、直した項目の対応の書き直しに文書の照合の記録を含める。

### 実装の順

workerの指示（決定4）はtask 1428、AGENTS.mdとplannerの登録の参照（決定2・3）はtask 1430が入れる。reviewの側（決定5）は、goal 90が比べるあいだrunのreviewのpromptと判定の基準を変えないと決めているので、task 1429がgoal 90のclose（achieved）の後に入れる。文面・定数の名前・文字数は[Prompt](../design/supervisor-lifecycle/prompt.md#文書の照合)が持つ。

## Alternatives

- **Taskかreceiptに文書専用の欄を足す**: schema・validation・reviewの変更が要る。人が2026-10-03に採らないと決めた。
- **evidenceにdocsの種類を足す**: validatingがrunを止め、要らない文書の差分を強いる。
- **文書の差分の有無を機械で検査する**: 示すためだけの無意味な差分を促し、名指されない古い文書を見落とす。
- **workerにgrepなどの検査のコマンドを課す**: 重い検査を足さない方針（決定6）に反する。

## Consequences

- docs_driftとdocs_out_of_syncによるreviewの差し戻しと、`docs_drift`のfollow_upが減ることを期待する。効果はこのgoalの後に、`dagq stats`のreview_reasonsのdocs_driftとgoal 90のtask 1422のscriptで読む。
- workerのsummaryに文書の照合の結果が1句増える。
