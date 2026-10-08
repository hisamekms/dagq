---
id: adr-t1847-1
type: adr
title: 必須のsubagentを動かせるproviderが控え中なだけのときは、runのreviewを失敗させずに控えが解けるまで待たせ、恒久的に動かせるproviderが無いときだけ手動reviewに渡す（ADR-t1453-1決定8をamends）
status: accepted
created: 2026-10-08
updated: 2026-10-08
accepted_on: 2026-10-08
amends:
  - adr-t1453-1 decision 8
owners:
  - hisamekms
tags:
  - runtime
  - review
  - provider
related:
  - adr-t1453-1
  - adr-t1063-1
  - adr-t1207-1
  - adr-t1857-1
  - adr-t1895-1
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-queue-hold
---

# ADR-t1847-1: 必須のsubagentを動かせるproviderが控え中なだけのときは、runのreviewを待たせる

## Context

[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)決定8は、reviewの行き先のproviderが必須のsubagentを動かせないとき、もう一方のproviderで起動し、「切り替え先が無い（`--no-claude`など）ときは待たずにpassにせず決定6の手動reviewに渡す」と決めた。

Claudeの利用上限でqueueの控えのask（`queue_hold`）がClaudeを控えたあいだ、`[roles.review]`が切り替えを許すrunのreviewはCodexに回り、Codexは必須のsubagentを動かせず、動かせるClaudeは控え中なので、reviewは`review_failed`（`subagents_unsupported`）と`approve_landing`のaskになった。
控えは時間が経つか人が答えれば解けるので、失敗にすると人が全てのaskに答え、workerのturnが無駄に走る。
人は、控えのあいだは待ち、`--no-claude`のように恒久的に動かせないときは今のまま失敗にすることを求めた（request 30）。

## Decision

1. **控え中なだけなら待つ。** 必須のsubagentを持つreviewで、行き先のproviderが動かせず、もう一方のproviderがreviewの役割を動かせ、そのagentがこのsupervisorにあり、控え（queueの控えのask・`ProviderHold`）で今使えないだけのときは、`review_started`も`review_failed`も記録せずに、sessionを開いたまま控えが解けるのを待つ。
   控えが解けたら同じ入力でreviewを起動し、起動は決定8のとおり切り替えの理由を記録する。
   待つあいだ、passごとに起動と待ちを往復して記録を積まない。
2. **恒久的に動かせないときだけ失敗にする。** 動かせるproviderのagentがこのsupervisorに無い（`--no-claude`など）か、reviewの役割を動かせないときは、決定8のとおり待たずに決定6の手動reviewに渡す。
3. **控えのaskとの関係。** subagentを動かせるproviderを待つrunは、控えのaskの影響（affected）にrunとして載せない。
   askを開くのは控えを作ったjobで、このrunは控えを読んで待つだけだからである。
   そのaskに`cancel_affected`と答えてもこのrunは手放されず、askが閉じて控えが解けた後のpassでreviewを起動する（`done`と同じ）。
   控えの答えをrunに当てる規則（affectedに載ったrunだけを処理する）は変えない。
4. **変えないもの。** 必須のsubagentを飛ばしてreviewを通す経路は持たない（決定8）。
   決定8の他の部分（providerごとの渡し方、起動の前に理由を記録して切り替えること、控え（`ProviderHold`）にしないこと、起動の後の失敗の扱い）とADR-t1453-1の他の決定は変えない。

## Alternatives

- 待つrunを控えのaskのaffectedに足し、`cancel_affected`で手放せるようにする: 人が手放す手段はできるが、askの登録の仕方とjobとして待つreviewとの扱いの差を決める必要がある。要るとわかったら別に決める。
- 今のまま失敗にし、`approve_landing`にreviewのやり直しを足すだけにする: 控えのあいだに開いたaskの数だけ人が答える必要が残る（やり直しの選択肢は同じgoalの別のtaskが足す）。

## Consequences

- 控えのあいだのreviewは失敗にならず、控えが解けると人の答え無しで進む。控えが長いと、そのあいだrunはslotとsessionを持ち続ける。
- `cancel_affected`で手放せるのは、affectedに載ったrunだけである。待つrunを止めるには、控えが解けた後のreviewの結果か、人の手での操作による。
- ADR-t1453-1は番号付きの決定を10持ち、変えるのは決定8の一部だけなので、置き換えずamendsにする（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。
  [ADR-t1895-1](2026-10-06-t1895-1-review-stage-runs-agent-and-program-jobs-in-a-fixed-shape.md)決定7がsubagentによる切り替えを無くした後は、各agentのjobの行き先が控えと待ちの規則に従うので、この決定はそれまでの今の形に当てる。
