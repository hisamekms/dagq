---
id: adr-t1984-1
type: adr
title: runのreviewのjobが非0で終わったときは、手動reviewに渡す前に同じ入力で1回だけ自動でやり直し、その回数は読めないverdictのやり直しと合わせてreviewごとに1回にし、どちらの原因のやり直しかを区別して記録する
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
amends:
  - adr-t1207-1 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - review
related:
  - adr-t1207-1
  - adr-t1453-1
  - design-supervisor-lifecycle-review
---

# ADR-t1984-1: 非0で終わったrunのreviewを1回だけ自動でやり直してから手動reviewに渡す

## Context

[ADR-t1207-1](2026-09-30-t1207-1-codex-run-review.md)決定3は、使えないproviderからの切り替えと分けて、一般の失敗（verdictが読めない・非0の終了・時間の上限）を切り替えずに手動review（`approve_landing`）に渡すと決めた。そのうちverdictが読めないreviewは、それより前から同じ入力で1回だけやり直してから手動reviewに渡している（task 328。必須のsubagentの結果が欠けたverdictも読めないものとして扱う、[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)）。

非0の終了はやり直さないので、一時的な失敗でも人の判断を待つ。2026-10-04と10-05に、reviewのjobが標準エラーに何も残さず終わったことだけを理由に手動reviewのaskが開いた。人は、一時的なjobの失敗は人のaskにする前にruntimeが1回やり直すと決めた（request 53）。

## Decision

1. ADR-t1207-1決定3の「一般の失敗は手動reviewに渡す」のうち、非0の終了の扱いだけを変える。jobが非0で終わったrunのreviewは、手動reviewに渡す前に同じ入力（同じreviewの材料とprompt）で1回だけ自動でやり直し、やり直しも失敗したら今までどおり手動reviewに渡す。
2. やり直しの回数は読めないverdictのやり直しと合わせてreviewごとに1回にする。読めないverdictのやり直しが非0で終わっても、非0のやり直しが読めないverdictで終わっても、2回目はやり直さずに手動reviewに渡す。同じ入力のreviewを最大2回に保ち、人へのaskが遅れすぎないためである。
3. 時間の上限で止まったreviewはやり直さない。やり直すと時間の上限をもう1度費やし、止まったjobは一時的な失敗とは言えないためである。起動できなかったreview（Claudeを禁じた運転で切り替え先の無いもの、subagentを渡せないもの、promptや入力を渡せないもの）はjobが走る前の失敗で、やり直しても同じく失敗するので、これもやり直さない。
4. やり直しは、読めないverdictのやり直しと非0の終了のやり直しのどちらかを区別して記録する。2つは原因が違い（jobの出力の問題か、jobの実行の問題か）、件数と傾向を分けて見るためである。
5. ADR-t1207-1決定3の残り（使えないproviderからはもう一方に切り替えること、Codexの控えはaskを開かないこと、一般の失敗ではproviderを切り替えないこと）は変えない。providerが使えないと分かった失敗は今までどおり切り替えか控えの待ちが先に扱い、この決定のやり直しはそれ以外の非0の終了だけに当てる。

## Consequences

一時的なjobの失敗で開く手動reviewのaskが減る代わりに、そのrunの着地はreview 1回分遅れる。2回とも非0で終わるreviewは、今までより1回分遅れて手動reviewに渡る。ADR-t1453-1の読めないverdictのやり直しはそのままで、回数の上限をこのADRの非0の終了と分け合う。
