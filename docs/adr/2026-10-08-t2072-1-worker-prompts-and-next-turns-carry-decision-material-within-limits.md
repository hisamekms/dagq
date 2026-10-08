---
id: adr-t2072-1
type: adr
title: workerの初期promptとsupervisorがworkerのsessionに送る次のturnの文にも、ADR-t1566-1の決定2〜6（判断の材料だけ、読める経路だけ、節ごとと全体の上限と決まった順、省いた件数と読む方法、byte数の記録とtest）を当て、読む方法はworkerが読める経路（worktreeのファイル・git・goalのdoc）に限る（ADR-t1566-1決定7をamends）
status: accepted
created: 2026-10-08
updated: 2026-10-08
accepted_on: 2026-10-08
amends:
  - adr-t1566-1 decision 7
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - worker
related:
  - adr-t1566-1
  - adr-t1892-1
  - adr-t598-1
  - adr-t813-1
  - design-supervisor-lifecycle-prompt
---

# ADR-t2072-1: workerの初期promptと次のturnの文にもADR-t1566-1の上限の方針を当て、読む方法はworkerが読める経路に限る（ADR-t1566-1決定7をamends）

## Context

ADR-t1566-1は、headlessのjobとruntimeのplannerのpromptに、判断の材料だけを節ごとと全体の上限の中に決まった順で載せ、省いた件数と読む方法を書き、promptのbyte数を記録して上限をtestで確かめることを決めた。決定7はその適用の範囲をjobとplannerに限り、workerの`prompt.txt`と、supervisorがworkerのsessionに次のturnとして送る文（resume・revise・食い違い・古いreceipt・receiptの無い促し・askの答え・復旧jobの指示・閉じたaskの知らせなど）は範囲の外だった。

2026-10-06の調査で、workerの初期promptは中央値22k字・最大82k字、2026-10-08のhostに残るrunの`prompt.txt`は中央値16KB・最大125KBで、大きいものは依存元のsummary（最大82KB）と引き継いだrunのsummary（最大68KB）が占めていた。どの節も上限を持たず、入力が増える方向にしか検査が無い。workerのピークのcontextは中央値153k・最大962kで、初期promptと次のturnの文はその土台になる。

workerはqueueのCLI（`dagq show`など）を打たない（workerのpromptが読むものを限っている）ので、jobのように省いた中身をqueueのCLIで取りに行かせることはできない。

## Decision

1. **ADR-t1566-1の決定2〜6を、workerの初期promptと、supervisorがworkerのsessionに送る次のturnの文の全てに当てる。** 判断の材料だけを載せ、節ごとの件数かbyteの上限と全体の上限を持ち、上限を超えたときに残す順は決まった規則で決め（LLMに選ばせない）、省いた件数と読む方法を書き、promptのbyte数をeventに記録し、最も大きな入力でも上限に収まることをtestで確かめる。ADR-t1566-1決定7の適用の範囲（headlessのjobとruntimeのplanner）にこれらを足す。
2. **workerに書く読む方法は、workerが実際に読める経路に限る。** worktreeのファイル、`git log`・`git show <commit>`のようなgitの読むコマンド（依存元の全文は着地のcommitのmessageにある）、goalのdocのpathだけを書き、workerが打たない`dagq`の読むコマンドを書かない。goalの記述の切った残りはgoalのdocで読ませ、docの無いgoalのものは読む方法が無いと書く。どの経路にも無いもの（taskの記述の切った残り、兄弟task、着地していないrunのreceiptのsummaryなど）は、読む方法が無いと書く（ADR-t1566-1決定3と同じく、読めない場所に退避しない）。
3. **taskそのものの記述は省かない。** taskのtitle・description・acceptance・verification commandsとpathsは必須の節で、一覧から外さない。それぞれの自分の上限を超えるときだけ切り、切ったことを記録に書く。固定の指示（receiptの契約・askの手順など）も省かない。
4. **渡し方は変えない。** ADR-t1566-1決定1（stdinかファイルで渡す）はworkerに当てない。workerの初期promptは今のとおり`prompt.txt`に書き、次のturnの文は今の非対話のturnの渡し方（ADR-t813-1）のままにする。
5. **上限の中の文面は今と同じにする。** 上限に当たらない入力では、promptの節の中身と形は上限を持つ前と変わらず、省いたことの注記は何かを省いたときだけ載る。

節ごとの上限の値・選ぶ順・byte数を記録するeventと欄は[Prompt](../design/supervisor-lifecycle/prompt.md)の表とコードのdoc commentが持つ（ADR-t598-1決定2・3）。次のturnの「Tasks landed」の節の形と件数の上限はADR-t1892-1が持ち、この決定はそれを変えない。

## Alternatives

- **workerを範囲の外のままにする**: 入力が増えるほどworkerのcontextの土台が育ち、jobで起きたのと同じ伸びを見張れない。
- **省いた中身を`dagq show`で読ませる**: workerはqueueを読まずに始める約束で、読む権限と手順を広げることになる。依存元の全文は着地のcommitのmessageにあり、gitで同じものが読める。
- **jobと同じくstdinで渡す**: workerのturnは既にファイルと非対話のturnで渡しており、`ARG_MAX`の問題は無い。経路を変える理由が無い。
- **taskの記述も他の節と同じく省いてよいとする**: workerの作業の正本で、他に読む経路が無い。省けば作業が成り立たない。

## Consequences

- workerの初期promptは上限の中に収まり、大きな依存元や引き継ぎのsummaryは決まった順で絞られ、省いた分はgitで読める。
- promptのbyte数がworkerのturnの開始のeventで見え、jobとplannerと同じ形で比べられる。
- 次のturnの文の上限は後続のtaskが同じ方針で足す。上限が切ったtaskの記述は記録で見える。
