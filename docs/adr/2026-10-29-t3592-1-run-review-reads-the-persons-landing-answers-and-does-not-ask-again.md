---
id: adr-t3592-1
type: adr
title: runのreviewは同じrunのapprove_landingへの人の答えを入力に持ち、受け入れたconcernを範囲が広がらない限り人に聞き直さない
status: accepted
created: 2026-10-29
updated: 2026-10-29
accepted_on: 2026-10-29
owners:
  - hisamekms
tags:
  - runtime
  - review
related:
  - adr-t451-1
  - adr-t2259-1
  - adr-0027
---

# ADR-t3592-1: runのreviewは同じrunのapprove_landingへの人の答えを入力に持ち、受け入れたconcernを範囲が広がらない限り人に聞き直さない

## Context

runのreviewは回ごとに独立で、前の回の何も渡さない。前の回の指摘や直しの差分を渡さないのは、reviewが直しの差分だけに狭まらず毎回差分の全体を読むための方針で、保つ。
しかし人の判断も届かない。人が`approve_landing`のaskの答え（`send_back: <理由>`など）で、ある権限の追加や書き込み先を設計どおりとして受け入れても、次の回のreviewは同じ拡張を`scope`のconcernで返し、人に同じ確認を求め直す。
2026-10の実例では、人が答えで受け入れたsecurityのconcernを次の回のsecurityのagentが同じ拡張のconcernとして返し、新しいaskを開いた。

## Decision

1. **人の答えを入力に持つ。** runのreview（全体のreviewと各agentのjob）は、そのrunの答えの済んだ`approve_landing`のaskへの人の答えの本文を、askのIDと開いた回とともに上限付きで資料（`review.md`）とpromptに持つ。runtimeが自分で閉じたaskの答えは人の判断ではないので持たない。答えの無いrunには何も足さない。前の回のreviewの指摘と直しの差分は今どおり渡さない（渡すのは人の判断だけ）。
2. **受け入れたものを聞き直さない。** 答えが受け入れたconcernは、答えの後の差分がその範囲を広げていなければ（新しい権限・書き込み先・外への接続などを足していなければ）`scope`のconcernにせず、summaryで人が受け入れたものとして名指す。広げていれば広がった部分だけをconcernにする。
3. **判断は人に残す。** [ADR-t451-1](2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)決定1・3（scopeとdiscardは人に残す）を保つ。人が既に決めたものを聞き直さないだけで、答えが決めていない新しいscopeの判断は人に上げる。全体のreviewがinstructionsを読まずpjの規則をlanding branchから読むこと（[ADR-t2259-1](2026-10-12-t2259-1-builtin-review-agents-and-programs-configured-by-builtin-names.md)）も変えない。

文言は特定のpjの権限名を持たない一般の文にする。上限の数値と関数名はコードのdoc commentが持つ。

## Alternatives

- **runtimeがconcernの文を照合して自動でlandにする**: 同じconcernかの判定が文の照合では当てにならず、範囲の広がりを見落とすので採らない。同じかと広がったかはreviewが差分を読んで判断する。
- **前の回のreviewの指摘をまとめて渡す**: reviewを前の回の指摘と直しに狭め、差分の全体を読まなくするので採らない。
- **答えを渡さず人が同じ答えを繰り返す**: 人の手間と待ちが残るので採らない。

## Consequences

- 人の答えで受け入れたconcernが同じrunで繰り返しaskになることが減る。
- reviewは答えの範囲と差分を突き合わせる判断を持つ。広がっていないと誤って判断すると、広がった部分が人に聞かれずに着地しうる。その危険は、runtimeが文を照合して自動で着地させる案より小さいと判断した（reviewは差分を読む）。
- promptの固定の文が増え、全体のreviewの固定の文のdigestが変わる。
