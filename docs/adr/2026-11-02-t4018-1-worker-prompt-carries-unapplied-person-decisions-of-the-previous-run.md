---
id: adr-t4018-1
type: adr
title: retryで作り直したrunのworkerのpromptに、前のrunのsessionに届かなかった人の判断を一覧から外さない必須の節として本文のまま入れ、その節を切るときだけworkerが読めるdagqの読むコマンドを名指す（ADR-t2072-1決定2・決定3をamends）
status: accepted
created: 2026-11-02
updated: 2026-11-02
accepted_on: 2026-11-02
amends:
  - adr-t2072-1 decision 2
  - adr-t2072-1 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - worker
related:
  - adr-t2072-1
  - adr-t1566-1
  - adr-t2946-1
  - adr-0047
  - design-supervisor-lifecycle-prompt
---

# ADR-t4018-1: retryで作り直したrunのpromptに前のrunの届かなかった人の判断を必須の節として入れ、切るときだけdagqの読むコマンドで読ませる（ADR-t2072-1決定2・決定3をamends）

## Context

人がapprove_landingに`send_back`で答えても、そのrunが再開を使い切っていればsessionは開かれず、runは復旧のjobに回る。
jobや人がretry・retry_inheritを選ぶと、新しいrunは新しいsessionで始まり、そのpromptには前のreceiptの要約しか入らなかった。
worker_question・decideのaskへの答えも、届く前にrunが終われば同じく失われる。
2026-11-02に、人の`send_back`の理由が新しいrunに届かず、workerが古い方針のまま作業し、reviewが同じ食い違いを再び指摘した。

ADR-t2072-1は、workerのpromptの節を上限の中に収め、切った残りの読む方法をworktree・git・goalのdocに限り、workerが打たない`dagq`の読むコマンドを書かないと決めた（決定2）。
必須で一覧から外さない節はtaskそのものの記述と固定の指示だけだった（決定3）。
人の判断の本文はどのcommitにもworktreeにも無く、queueのeventsにだけある。
workerはclient modeでqueueを読む権限を持ち、queueの読むコマンドでeventsとaskを読める。

## Decision

1. **前のrunの届かなかった人の判断の節は、必須の節にする（決定3に足す）。**
   retry・retry_inherit（runtime・復旧のjob・人の手の引き継ぎのどれでも）で終わった前のrunに、そのsessionに届いた記録の無い人の判断（`send_back`の理由、worker_question・decideのaskへの答え）があれば、新しいrunのpromptにその本文をaskのidとともに、人の判断として必ず従うものとして入れる。
   前のrunのsessionがpromptを受け取った記録が無ければ（起動や登録や最初のturnの前に失敗したrun）、そのpromptが運ぶはずだったさらに前のrunの判断も、retryで終わったrunをたどって同じ節に集める。
   この節はtaskの記述と同じく一覧から外さず、判断がいくつあっても自分の上限の中に収める。
   上限を超えるときだけ本文を切るか判断を節から外し、切ったことを記録に書く。
   外した判断は、その数と、節の上限の中で名指せるだけのaskのidと、外した判断の全てを読める読む方法で示す（idを全て並べると判断の数だけ節が育ち、上限を守れないため）。
   届いたかは送った記録だけで決め、記録の無いものは届いていないとみなす（重ねて渡すほうが落とすより害が小さい）。
   本文を持つ記録が読めないときは、本文を欠いたまま新しいrunを始めない。
2. **その節を切るときに限り、読む方法として`dagq`の読むコマンドを書いてよい（決定2の例外）。**
   切った本文と外した判断には、askのidと、その本文を全文で持つ記録を読むコマンドを名指す。
   判断を記録したeventはtaskと種類で絞って読ませる。
   eventが本文を持たない古い答えはaskそのものが本文を持ち、askをtaskで絞る読むコマンドが無いので、queueのaskを全て読むコマンドを名指し、askのidで探させる。
   workerのtokenはqueueを読む権限を持つので、読めない場所への退避にならない。
   ほかの節の読む方法は決定2のままworktree・git・goalのdocに限る。

ADR-t2072-1の決定1・4・5とADR-t1566-1は変えない。
再開の上限の値と数え方（ADR-t2946-1を含む）も変えない。
節の上限の値・本文を持つeventの種類・読むコマンドの形・promptの文面はコードのdoc commentと[Prompt](../design/supervisor-lifecycle/prompt.md)が持つ。

## Alternatives

- **切らずに上限を外す**: 人の判断の本文には上限が無く、review の指摘を引いた`send_back`の理由は長くなりうる。上限を外せばADR-t2072-1の全体の上限が守れず、promptの大きさを最も大きな入力で確かめるtestが成り立たない。本文を落とさないことは読む方法の名指しで満たせる。
- **worktreeのファイルに書き出す**: worktreeはrunの作業の場所で、書き出したファイルはcommitやscopeの検査に紛れ、containerのrunのcloneにも別の経路で渡す必要がある。本文の正本はqueueのeventsにあり、workerはそれを読めるので、写しを作る理由が無い。
- **復旧のjobの判断の理由に本文を写させる**: jobは判断を名指しても本文を写さないことがあり（今回の理由はaskを名指しただけ）、runtimeがeventsから直接取るほうが確かである。

## Consequences

- retryの後の新しいrunは、前のrunで人が下した判断を本文で受け取り、従う。
- workerのpromptの全体の上限は、この節の分だけ大きくなる。
- workerのpromptに`dagq`の読むコマンドが現れるのはこの節が切られたときだけで、workerの読む範囲の説明（[Prompt](../design/supervisor-lifecycle/prompt.md)）はこの例外を持つ。
- 同じ判断が前のsessionにも届いていた場合（送った記録が書けなかったとき、promptが届いたturnの記録の前に終わったとき）は、重ねて渡る。
