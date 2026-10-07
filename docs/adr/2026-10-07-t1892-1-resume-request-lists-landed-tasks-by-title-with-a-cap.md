---
id: adr-t1892-1
type: adr
title: resumeの解消依頼は着地したtaskをIDとtitleだけで新しい順に上限まで並べ、receipt summaryを載せず、残りは件数とgitで読む方法を示す
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
amends:
  - adr-0047 decision 24
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - performance
related:
  - adr-0027
  - design-supervisor-lifecycle-needs-session
---

# ADR-t1892-1: resumeの解消依頼は着地したtaskをIDとtitleだけで新しい順に上限まで並べ、receipt summaryを載せず、残りは件数とgitで読む方法を示す

## Context

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定24は、`needs_session`のrunのsessionへ送る定型の解消依頼に、runのbaseから今のmainまでに着地したtaskのtitleとreceipt summaryを含めるとした。件数と長さに上限は無く、resumeのたびにbase以降の全件を送り直す。reviewがpassしたrunのsessionへ送る衝突の依頼（[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)の決定4）も同じ節を使う。

request 41（2026-10-06）の調査では、ピークのcontextが200kを超えたworkerの60 sessionの284件のresumeのうち125件にこの節があり、長さは中央値19k字・p90 63k字・最大99k字、合計3.5M字で、resumeのuser promptの大半を占めた。人はこれをurgentで直すよう指示した。

## Decision

1. ADR-0047の決定24の「着地したtaskのtitleとreceipt summary」を改める。解消依頼（衝突の依頼も同じ）の着地したtaskの節は、1行にtask IDとtitleだけを書き、receipt summaryを載せない。
2. 並べるのは新しく着地したものから上限の行数まで。上限を超えた分は、省いた件数と、全部を`git log`（runのbaseからmainまで）で読めることを1行で書く。節の見出しは、各taskが何を変えたかを`git log`・`git show`で読めることを示す。
3. 着地したtaskが無ければ、無いことを1行で書く（今どおり）。

## Alternatives

- **runの変更pathと重なるtaskだけsummaryを残す**: pathの照合が要り、長さの上限も別に要る。衝突の解消に要る情報はgitの差分のほうが正確なので採らない。
- **summaryを字数で切る**: 件数に上限が無いままで、resumeごとに全件を送り直す増え方が残るので採らない。

## Consequences

- 解消依頼の長さは件数の上限とtitleの長さで決まり、resumeを重ねてもこの節は一定の大きさに収まる。
- workerは着地したtaskの中身をsummaryで読めなくなり、要るときは`git log`・`git show`で読む（workerは`dagq show`を打たない）。
- 上限の値と節の文言はコード（解消依頼を組み立てる箇所のdoc comment）が持つ。
