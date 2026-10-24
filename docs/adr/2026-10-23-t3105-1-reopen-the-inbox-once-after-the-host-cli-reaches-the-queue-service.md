---
id: adr-t3105-1
type: adr
title: 人・inbox・plannerのCLIをqueue service経由にする段の着地の後に一度だけ、前のbuildで開いたinboxを開き直し、そのとき開いていたruntimeのplannerは閉じるのを待つ。名乗るだけでinbox・plannerと認める経路とDBへの戻りは作らず、serviceが一度も動いていないqueueでもDBを開かずserviceが居ないことを返す。inbox・plannerのtokenはserviceが親と対象を検査する発行ができるまで制御側が発行する（ADR-t3102-2決定7・ADR-t3102-1決定2をamends）
status: accepted
created: 2026-10-23
updated: 2026-10-23
accepted_on: 2026-10-23
amends:
  - adr-t3102-2 decision 7
  - adr-t3102-1 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - security
  - architecture
related:
  - adr-t3102-1
  - adr-t3102-2
  - adr-t3102-3
  - adr-t2159-1
  - design-queue-service
---

# ADR-t3105-1: host CLIの段の後の一度だけの開き直しと、serviceが一度も動いていないqueue

## Context

[ADR-t3102-2](2026-10-22-t3102-2-only-the-queue-service-opens-the-db-schema-by-the-service-build-and-migration-order.md)決定6の(2)の段で、人・inbox・plannerのhostのCLIは、serviceが答えるコマンドをqueue serviceへ送る。
inboxとplannerは、[ADR-t3102-1](2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)決定1のとおり、環境がroleを名指してtokenが無ければuserにならず断られる。
tokenはinboxを開くときとplannerのturnを起動するときに渡すので、この段のbuildの前に開いたinboxとplannerのsessionはtokenのfileを持たない。
固定バイナリがこのbuildに入れ替わると、それらのagentが打つ`dagq`（新しいbuild）は、読み取りやnoteを`missing_token`で断られる。
ADR-t3102-2決定7は、固定バイナリの入れ替えを移行の各段で壊さないとした。
この段の着地の後の入れ替えだけは、前のbuildのsessionを壊さずに済ます経路を作ると、ADR-t3102-1の認証か、fail closedのどちらかを崩す。

## Decision

1. **この段の着地の後に一度だけ、前のbuildで開いたinboxを開き直す。**
   人は`DAGQ_ROLE`の無いterminalで`dagq inbox`を打って開き直す（[ADR-t2159-1](2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定2）。
   そのとき開いていたruntimeのplannerは、閉じるのを待つ（次に開くplannerはtokenを持つ）。
   開き直した後のsessionはtokenのfileを持つので、以後の入れ替えでは壊れない。
   これはADR-t3102-2決定7の「固定バイナリの入れ替えを壊さない」の、この段の1回だけの例外にする。
   人はこの例外をこの1回だけ認めた（ask 1113の(C)、ask 1138）。
2. **前のbuildのsessionのための経路は作らない。**
   tokenの無いinbox・plannerをDBの経路に戻すことも、名乗るroleだけでinbox・plannerと認めることもしない。
   断るときは、理由と次の手（inboxは開き直し、plannerは閉じるのを待つ）を返す。
3. **serviceが一度も動いていないqueueでもDBを開かない。**
   hostのCLIは、serviceが答えるコマンドを、queueのserviceが動いたことがあるかに依らずserviceへ送る。
   serviceが居なければ（一度も動いていないqueueも）、DBを開かずserviceが居ないことと次の手（`dagq up`か`dagq service start`）を返す（[ADR-t3102-3](2026-10-22-t3102-3-stage-5-outage-notice-and-use-cases-for-multi-step-control-operations.md)決定2）。
   DBがそこに無いときは、送る前にqueueが無いことを返す。
   この段で並存の期間にDBを直接開くのは、ユースケースの無いコマンド（書き込みと、`doctor`や`run log`のような制御側の診断とfileを書く読み取り）だけになる。
   CLIがDBを開いて行っていた読み取りの前の確かめはserviceが行う: repositoryのqueueの束縛は、CLIが解決したrepositoryを要求に載せてserviceが答える前に照らし、floorの内側の古いschemaのqueueの読み取りは、serviceが読み取り専用で開いて答える（[ADR-0045](0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)決定18）。
4. **ADR-t3102-2決定7を変える範囲。** 決定1の1回の例外を足し、決定3で並存の期間にDBを直接開くコマンドをユースケースの無いものに限る。fail closedと、1つのコマンドの中で経路を混ぜないことは変えない。
5. **inbox・plannerのtokenは、serviceが発行のユースケースを持つまで制御側が発行する（ADR-t3102-1決定2の例外）。**
   ADR-t3102-1決定2は、inboxとplannerのtokenを親（userとsupervisor）の依頼でserviceが発行し、親が子を起動してよいかと対象の前提を発行と同じtransactionで確かめるとした。
   その発行のユースケースができるまでは、`dagq inbox`とplannerのturnを起動するsupervisorが、workerのtokenと同じくserviceのdirのfileに直接書いて発行する。
   その間も、inboxのtokenはそのsessionのprocessに結び付けてsessionごとに別のfileに置き、開き直しで前のsessionのfileとtokenを消し、sessionの終わりで失効させる。plannerのtokenは対象のplannerに結び付け、閉じたときに失効させる。
   親の権限と対象の前提の検査を発行と同じtransactionで行う形には、その発行のユースケースで戻す。
   ADR-t3102-1決定2の表の発行者・時機・対象・失効と、tokenの置き場の規則は変えない。
   人はこの例外を、serviceの側の親の認可と同じtransactionでの発行を後続のtask（3125）で入れるまでの間として認めた（ask 1126、ask 1129）。

判定のfileの名前、断りの文言、運用の手順は[Queue service](../design/queue-service.md)とpluginの`dagq-recover`の`reference/update.md`、[運用](../development/operations.md)が持つ。

## Alternatives

- **tokenの無いinbox・plannerは今のままDBを開く**: 名乗るroleだけでDBの経路を選べ、移行の後も同じ穴が残る。fail closedの規則（ADR-t3102-2決定7）と合わない。
- **serviceかwrapperが前のbuildのsessionにtokenを渡す**: 前のbuildのwrapperとagentの環境は後から変えられず、渡す先が無い。名乗るroleに応じてserviceが発行すれば、名乗るだけでinbox・plannerになれる。
- **serviceが一度も動いていないqueue（serviceのlockのfileが無い）だけはDBを開く**: lockのfileの有無で経路を選ぶと、止まったserviceと一度も動いていないserviceを分け、届かないときにDBを開く経路が残る。人はこの例外を認めなかった（ask 1138）。

## Consequences

- この段を含む更新の後、人はinboxを一度開き直す。それまでinboxの読み取り・`watch`・noteは、開き直しを促す断りで終わる。
- runtimeのplannerは、閉じるまで読み取りとnoteを断られ、ユースケースの無い書き込み（`add`・`submit`など）はDBで続く。
- `init`の直後と`up`の前のqueueの読み取り（`status`・`list`など）は、serviceを起動するまでserviceが居ないことを返す。`install`は新しいbuildを使い捨てのqueueで確かめるとき、そのbuildのserviceを起動して読む。
- serviceを起動しないtestは、人のCLIの読み取りの前にtestのserviceを起動する。
- 決定5の間、inboxとplannerのtokenの発行では、親が子を起動してよいかをserviceのpolicyが確かめない。発行できるのは制御側のprocess（`dagq inbox`とsupervisor）だけで、AI actorのコマンドには発行の手段が無い（ADR-t1233-4決定4）。
