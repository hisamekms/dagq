---
id: adr-t3656-1
type: adr
title: runのreviewは人が受け入れたconcernの範囲の広がりを、答えの後の差分ではなくrunの差分の全体と答えが受け入れた範囲の突き合わせで判断する（ADR-t3592-1決定2をamends）
status: accepted
created: 2026-10-29
updated: 2026-10-29
accepted_on: 2026-10-29
amends:
  - adr-t3592-1 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - review
related:
  - adr-t3592-1
  - adr-t451-1
  - adr-t2259-1
---

# ADR-t3656-1: runのreviewは人が受け入れたconcernの範囲の広がりを、答えの後の差分ではなくrunの差分の全体と答えが受け入れた範囲の突き合わせで判断する

## Context

[ADR-t3592-1](2026-10-29-t3592-1-run-review-reads-the-persons-landing-answers-and-does-not-ask-again.md)決定2は、答えが受け入れたconcernを「答えの後の差分」がその範囲を広げない限り聞き直さないとした。
しかしreviewに渡る答えはaskのIDと開いた回だけで、その回のreviewが読んだheadのcommitを持たない。reviewはcommitの履歴から答えの起点を推し量るしかなく、起点を誤ると広がった部分を見落としうる。
起点のheadを渡せば推し量りは要らないが、reviewが起点からの直しの差分に狭まりうるので、前の回の結果を渡さず毎回差分の全体を読むという決定1の方針に反する。

## Decision

1. **差分の全体と答えの範囲を突き合わせる。** 答えが受け入れたconcernは、runの差分の全体（reviewが毎回読むもの）が答えの受け入れた範囲（答えの本文が名指す権限・書き込み先・外への接続など）を超えていなければ`scope`のconcernにせず、summaryで人が受け入れたものとして名指す。超えていれば超えた部分だけをconcernにする。答えがどこまでを受け入れるかを言わないときは、名指す種類の範囲だけを受け入れたと読み、範囲の内か確かでない部分はconcernにする。答えの後に何が変わったかでは判断しないので、起点は要らない。
2. **起点を渡さない。** 答えの回のreviewが読んだheadのcommitも、答えの後の直しの差分も、reviewに渡さない。

ADR-t3592-1の決定1・3、[ADR-t451-1](2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)決定1・3、[ADR-t2259-1](2026-10-12-t2259-1-builtin-review-agents-and-programs-configured-by-builtin-names.md)は変えない。文言は特定のpjの権限名を持たない一般の文にする。

## Alternatives

- **答えの回のheadのcommitを渡す**: 起点の推し量りは消えるが、reviewが起点からの直しの差分だけを読む方へ狭まりうるので、決定1の方針と衝突するので採らない。
- **今のまま答えの後の差分で判断する**: 起点の推し量りを誤ると広がった部分が人に聞かれずに着地しうるので採らない。

## Consequences

- reviewは起点を推し量らず、毎回読む差分の全体で判断するので、起点の誤りによる見落としが無くなる。
- 判断は答えの本文がどこまでを受け入れたかの読みに依る。答えが範囲を明確に名指さないときは決定1のとおり狭く読むので、同じ種類の拡張でも確かでない部分は聞き直しになりうる。
- promptの固定の文が変わり、全体のreviewの固定の文のdigestが変わる。
