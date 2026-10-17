---
id: adr-t2687-1
type: adr
title: プログラムのreviewの差し戻しをrunを通した自動の手直しの上限に数え、上限の後は収束の判定に通す（ADR-t1895-2決定3をamends）
status: accepted
created: 2026-10-18
updated: 2026-10-18
accepted_on: 2026-10-18
amends:
  - adr-t1895-2 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - review
related:
  - adr-t1895-2
  - adr-t2599-1
  - design-supervisor-lifecycle-review
---

# ADR-t2687-1: プログラムのreviewの差し戻しをrunを通した自動の手直しの上限に数え、上限の後は収束の判定に通す（ADR-t1895-2決定3をamends）

## Context

[ADR-t1895-2](2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)の決定3は、プログラムのreviewが落ちたらagentのjobを起動せずworkerに差し戻し、その差し戻しを「reviseの上限に1回と数える（上限を超えれば人の判断）」と書いた。

[ADR-t2599-1](2026-10-17-t2599-1-run-review-revises-counted-across-the-run-and-judged-by-code-findings-past-the-limit.md)は、決定1でプログラムのreviewの差し戻しをrunを通して数える自動の手直しに含め、決定3〜6で上限の後に差し戻すreviewを理由のコードで判定すると決めた。ところがその`amends`にADR-t1895-2は無く、accepted のADRの`amends`は後から足せないので、ADR-t1895-2決定3から今の決定を辿れない。

## Decision

1. **プログラムのreviewの差し戻しは、ADR-t2599-1決定1のrunを通した自動の手直しに1回と数え、上限の後はADR-t2599-1決定3〜6の収束の判定に通す。** ADR-t1895-2決定3の「reviseの上限に1回と数える（上限を超えれば人の判断）」をこれに改める。数え方と判定の中身はADR-t2599-1が持ち、ここに書き写さない。
2. **それ以外は変えない。** 落ちたらagentのjobを1本も起動せず、落ちたプログラムの名前と出力を理由にworkerへ差し戻すことは、ADR-t1895-2決定3のまま。

## Alternatives

- **ADR-t2599-1の`amends`に後から足す**: acceptedのADRの`amends`はappend-onlyで後から変えられない。
- **ADR-t1895-2決定3の本文を直す**: acceptedのADRの本文はappend-onlyで書き換えない。
- **ADR-t1895-2を丸ごと置き換える**: 決定を複数持つADRの一部だけを変えるので、置き換えでなく`amends`にする。

## Consequences

- ADR-t1895-2決定3から`amended_by`でこのADRへ、このADRからADR-t2599-1へ辿れる。
- 今の姿は[Review](../design/supervisor-lifecycle/review.md)とその下のプログラムのreviewの文書が持つ。
