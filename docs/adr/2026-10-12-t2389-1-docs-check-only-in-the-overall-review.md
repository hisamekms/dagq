---
id: adr-t2389-1
type: adr
title: runのreviewの文書の照合は全体のreview（builtin:acceptance）だけが持ち、builtin:docsを同梱せず、このrepositoryの.dagq/agents/design-consistencyを退役させてその項目をpjの規則のfileに移す（ADR-t2259-1決定1・5・10・11・12、ADR-t1942-2決定4をamends）
status: accepted
created: 2026-10-12
updated: 2026-10-12
accepted_on: 2026-10-12
amends:
  - adr-t2259-1 decision 1
  - adr-t2259-1 decision 5
  - adr-t2259-1 decision 10
  - adr-t2259-1 decision 11
  - adr-t2259-1 decision 12
  - adr-t1942-2 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - review
  - documentation
related:
  - adr-t2259-1
  - adr-t1942-1
  - adr-t1942-2
  - adr-t2287-1
  - adr-t1453-2
  - adr-t1091-1
  - design-supervisor-lifecycle-review
---

# ADR-t2389-1: runのreviewの文書の照合は全体のreviewだけが持ち、builtin:docsを同梱せず、design-consistencyを退役させる

## Context

[ADR-t2259-1](2026-10-12-t2259-1-builtin-review-agents-and-programs-configured-by-builtin-names.md)は、全体のreviewのpromptから文書の照合（`REVIEW_DOCS_CHECK`）を外して`builtin:docs`（既定off、このrepositoryはon）に移し、コードの書き写しと経緯の指摘はこのrepositoryの`.dagq/agents/design-consistency`に残すと決めた（決定1・5・10・11・12）。

人はrequest 106でこれを見直した。人の言葉: 「ドキュメント単独でみてわかることは少ないので全体のレビューに統合する。統合案で。折衷案は一旦不要。将来似た方式をとるにしても全体のレビューの設定項目の一部である方が自然（全体のレビューはレビューサブエージェントと横並びのメンタルモデル）」。

本番queueの2026-10-05〜10-12のeventsの集計（inboxが添えた根拠）:

- 全体のreviewのpromptは`REVIEW_DOCS_CHECK`と`REVIEW_RULES`を常に持ち、design-consistencyと観点が重なる。
- Codexの並列のjobの時期（10/11〜）にdesign-consistencyが指摘した64回のうち47回（73%）で、全体のreviewも同じ回に文書を指摘した。design-consistencyがA（実害）を出した27回のうち、全体のreviewがAを出していなかったのは3回だけだった。
- design-consistencyのjobはreviewのsubagentの中で最も重い（中央値でpeak context 84k・cache read 361k・94秒、122 jobで計3.4h）。
- runのreviewの実害のある指摘の57%が`docs_drift`で、差し戻しの往復の主因である。

文書の照合は差分・taskの記述・receiptと突き合わせて初めて判断でき、文書だけを見るjobは全体のreviewと同じ材料を読み直す。

## Decision

1. **文書の照合は全体のreviewだけが行う。** runのreviewの文書の照合（変えた挙動に対する文書の古さ・欠けを`docs_drift`にすること）は全体のreviewのjob（`builtin:acceptance`）だけが行い、別のagentのjob（built-inでもpjの`.dagq/agents/`でも）で同じ照合を重ねない。`builtin:docs`は同梱しない。同梱のagentは`builtin:acceptance`・`builtin:security`・`builtin:tests`で、`builtin:docs`は知らないbuiltinの名前として設定の誤りになる（ADR-t2259-1決定4）。
2. **汎用の部分はruntime、dagqの文書の方針はpjの規則。** 変えた挙動に対する文書の古さ・欠けを指摘すること、文書の差分だけで正しいとしないこと、名前の欠けだけをずれにしないことは汎用の照合として全体のreviewのruntimeのpromptに残す。コードの書き写しと経緯の混入を`docs_drift`にする部分はdagqの文書の方針（[ADR-t1942-2](2026-10-07-t1942-2-document-check-and-review-in-both-directions.md)・[ADR-t1942-1](2026-10-07-t1942-1-design-docs-in-four-layers-with-size-budgets.md)）なのでruntimeに固定せず、このrepositoryではpjの規則の`.dagq/review/builtin-acceptance.md`が持つ（goal 188の制約「dagq自身の開発の事情をbuilt-inに固定しない」、ADR-t1453-2決定5）。
3. **このrepositoryのdesign-consistencyは退役する。** その項目（ADR-t1942-2決定1〜4のsummaryの探した名前の確かめ、名前の欠けだけをずれにしないことのこのrepositoryの文書の規則での当て方、書き写しと経緯の指摘、A-153・A-261・A-260）は`.dagq/review/builtin-acceptance.md`に移す。`.dagq/review/builtin-docs.md`は`builtin-acceptance.md`に統合して消す。1文1行・大きさの予算・本文のtask番号は今どおり`[review.programs]`のプログラムが見る。
4. **切りたい要望は全体のreviewの設定にする。** 将来、外部のpjが文書の照合を切りたいと言っても、別のbuilt-inの名前にせず、全体のreview（`builtin:acceptance`）の設定項目の一つにする。全体のreviewはreviewのsubagentと横並びのものと扱う。今は設定を足さない。

## Amendsの判断

変える決定はADR-t2259-1の13個のうち5個と、ADR-t1942-2の4個のうち1個で、どちらも大半ではないので[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)によりamendsにする。

- **ADR-t2259-1決定1**（同梱するbuilt-in）: agentの一覧から`builtin:docs`を除く。`builtin:acceptance`・`builtin:security`・`builtin:tests`とprogramの`builtin:secrets`、agentとprogramで名前を重ねないことは保つ。
- **ADR-t2259-1決定5**（既定）: 既定offの一覧から`builtin:docs`を除く（既定offは`builtin:tests`だけ）。既定on・既定の`paths`・置き換えの規則は保つ。
- **ADR-t2259-1決定10**（eval）: 同梱のケースとevalの対象から`builtin:docs`を除き、対象は`builtin:security`・`builtin:tests`にする。ケースの形（baselineとpatch、独立の一時のrepository）・digest・pjの規則とprogramを当てないこと・`builtin:acceptance`を対象にしないことは保つ。
- **ADR-t2259-1決定11**（`REVIEW_DOCS_CHECK`の移し先）: 全体のreviewのpromptから外さず、汎用の部分を全体のreviewに残し、書き写しと経緯の部分の置き場をdesign-consistencyから`.dagq/review/builtin-acceptance.md`に変える（決定2）。書き写しと経緯をbuilt-inに入れずdagqの文書の方針として扱うことは保つ。このrepositoryで`builtin:docs`を有効にすることは除く。
- **ADR-t2259-1決定12**（このrepositoryの設定）: 残すpjのagentからdesign-consistencyを除き（7つ）、`builtin:docs`のonと`.dagq/review/builtin-docs.md`の手書きを除く（決定3）。他のpjのagentを残し重なる部分をpjの側に置くこと、`builtin:tests`のoff、`builtin:security`・`builtin:secrets`の既定のon、`builtin-acceptance.md`を実装の着地の前に手で書くこと、新しいkeyを固定バイナリが読めるようになってから足すことは保つ。
- **ADR-t1942-2決定4**（runのreviewは両方向に照合する）: ADR-t2259-1が定めた行き先（汎用の部分は`builtin:docs`、書き写しと経緯はdesign-consistency）を、汎用の部分は全体のreview、書き写しと経緯と探した名前の確かめはこのrepositoryの`.dagq/review/builtin-acceptance.md`に変える。全体のreviewは全ての差分で走るので、照合の当たるpathをrepositoryの設定で絞ることはなくなる。両方向に照合すること、文書の差分だけで正しいとしないこと、名前の欠けだけでは指摘しないこと、差分が入れたものだけを指摘すること、ADR-t2287-1決定3が足した境界の記録の漏れを指摘しないことは保つ。決定1〜3は変えない。

## Alternatives

- **ADR-t2259-1の`builtin:docs`の形**（別のjobが同じ照合をする）: 文書だけを見るjobは差分・taskの記述・receiptを全体のreviewと同じく読み直し、指摘の73%が全体のreviewと同じ回に重なり、Aの見落としを補ったのは27回のうち3回だった。最も重いjobの時間とtokenに見合わない。
- **折衷案**（`builtin:docs`のon/offを残し、onなら全体のreviewのpromptに文書の節を足す）: 人が「一旦不要」と退けた。文書の照合の有無を別のbuilt-inの名前で切り替えると、全体のreviewの中身を外の名前が変える形になり、全体のreviewとsubagentを横並びにする見方に合わない。要望が出れば決定4の形にする。
- **design-consistencyを全体のreviewと重ならない項目に絞って残す**: 項目は全てが文書の照合で、重ならない項目が残らない。1文1行・予算・task番号は既に`[review.programs]`が見る。

## Consequences

- 実装はgoal 188の後続のtaskが行う: `builtin:docs`を同梱しない実装とeval、`REVIEW_DOCS_CHECK`の書き写しと経緯の部分をruntimeから外すこと、`.dagq/agents/design-consistency`の退役と`.dagq/review/`の統合、[Review](../design/supervisor-lifecycle/review.md)の今の姿の記述。実装の着地までは今の実装が動く。
- runのreviewのjobが1本減り、このrepositoryの文書の照合の時間とtokenが減ることを期待する。全体のreviewのpromptとpjの規則のfileは書き写しと経緯の項目の分だけ長くなる。
- 外部のpjの全体のreviewは汎用の文書の照合を常に行い、今は切れない。
