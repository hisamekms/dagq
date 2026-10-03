---
id: adr-t1233-4
type: adr
title: queue serviceはsupervisorの起動・停止と同じ`up`・`down`・引き継ぎで起動・停止し、生きている間はsupervisorが見張って起動し直し、落ちたときの知らせはserviceを通らない経路で届け、brokerより前のhost構成では実行側のprincipalを制御側がrun・jobごとに発行して失効させるtokenで認証する
status: accepted
created: 2026-10-02
updated: 2026-10-02
accepted_on: 2026-10-02
amended_by:
  - adr-t1433-5
owners:
  - hisamekms
tags:
  - runtime
  - security
  - supervisor
related:
  - adr-t1233-1
  - adr-t1233-5
  - adr-t728-1
  - adr-t827-2
  - adr-t906-1
  - adr-0073
  - design-security
  - design-authorization
---

# ADR-t1233-4: queue serviceの起動・停止の責任、落ちたときの知らせ方、brokerより前のprincipalの認証

## Context

[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)はqueue serviceをqueue DBを開く唯一のプロセスにし、実行側のdagqをクライアントモードにすると決めた（goal 82の段(2)〜(3)）。goal 38はserviceの起動・停止の責任と、serviceが落ちたときの通知の経路（inboxの`watch`自体がserviceに依存しうる）を未決にしていた。また段(3)ではworkerとjobからDBのpathを外すが、queueのbroker（段(4)）はまだ無く、実行側はhostのunix socketでserviceを呼ぶ。socketにつないだプロセスは全て同じユーザーなので、socketのpeerからはどのrunのworkerかを区別できない。今のrole（`DAGQ_ROLE`）は偽装できる。

## Decision

1. **serviceの起動・停止はsupervisorの生き死にの手順に乗せる。** `up`がsupervisorより先にserviceを起動し（生きていればreuseし、build識別子が違えば入れ替える）、`down`はsupervisorを止めた後にserviceを止める。`install`と自動更新の引き継ぎはsupervisorと同じ規則でserviceも新しいバイナリに入れ替える（[ADR-0073](0073-kind-additions-are-compatible.md)の固定バイナリの運用。serviceは固定バイナリだけで動き、開発中のバイナリは本番のserviceを起動しない）。serviceは1つのqueueに1つで、同じqueueに2つ目を起動しない。
2. **生きている間はsupervisorが見張る。** supervisorは各passでserviceの生存を確かめ、居なければ上限つきで起動し直し、起動し直したことを記録する。serviceが居ない間、supervisorは新しいclaimとjobの起動を控える（クライアントモードのworkerとjobがqueueに届かないため）。走っているrunは止めず、届かなかった操作はクライアントのerrorとして実行側に返る。supervisorも居ないときの起動し直しは人が`up`で行う（今のsupervisorと同じ）。
3. **落ちたときの知らせはserviceを通らない経路で届ける。**
   - 段(2)〜(4)では、supervisor・inbox・人のCLIはDBを直接開くので、supervisorはserviceの停止と起動し直しの失敗を、DBにattentionとして直接書いてinboxに届ける。inboxの`watch`はserviceに依存しない。
   - 段(5)でinboxの`watch`と`status`がservice経由になったら、それらはserviceに届かないこと自体を知らせとして返し、黙って空を返したり待ち続けたりしない。supervisorがinboxのterminalに1行の知らせを打つ経路（[ADR-t906-1](2026-09-28-t906-1-guarantee-the-inbox-watch.md)決定1の(3)）を、serviceに届かないときの後ろ盾にも使う。
   - どの段でも、クライアントモードのdagqがserviceに届かないときは、届かないことを分かるerrorで返し、DBを直接開くことへ戻らない。
4. **brokerより前の実行側のprincipalは、制御側が発行するtokenで認証する。** supervisor（信頼する制御側）は、workerのrun（claimとresumeのとき）とheadlessのjob（起動のとき）ごとに、principal（roleとactor idと対象のrunかjob）に結びついたtokenを発行し、serviceはtokenからprincipalを決める。AI actorにtokenを作るコマンドは持たせない。tokenはrun・jobの終わり、leaseの喪失、resumeでの発行し直しで失効する。tokenの値はenvに置かずファイルの場所を渡す（envの値はcmuxのargvに出るため）。tokenのファイルは実行側のworktreeとrun dirの外に置く。実行側のtokenの無いクライアントモードの呼び出しは拒む（fail closed）。
5. **hostでは助言的のまま。** host構成では同じユーザーのプロセスが他のrunのtokenのファイルもDBのファイルも読めるので、tokenは誤りを止め記録を正しくするためのもので、security boundaryではない（[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)決定6）。段(4)のqueueのbrokerは同じtokenを実行側から受け取り、隔離環境ではtokenのファイルをそのrun・jobにだけ見せることで境界になる。
6. **hostの制御側と人のprincipal。** 段(2)〜(4)でDBを直接開くsupervisor・wrapper・inbox・planner・人は今のactorの決め方（ADR-t728-1）のまま。段(5)でそれらをservice経由にするとき、wrapperとhooks（session・session-event・planner-session）には自分の信頼するprincipalを与え、inbox・plannerにもtokenを発行する。人（user）をtokenで表すかsocketの別の資格で表すかを含め、その形はgoal 38の段(5)が決める。

tokenの書式・置き場・期限の値・envの名前・attentionのkind・起動し直しの上限と間隔は実装のtaskが`docs/design/`に書く。

## Alternatives

- **supervisorのプロセスの中でserviceを動かす**: supervisorの入れ替えや停止でserviceも止まり、supervisorが居なくても人のCLI（段(5)）がqueueを使えなくなる。並列のsupervisorでは誰が持つかも決まらない。
- **launchdなどOSのservice managerだけに任せる**: in-cmux modeの運用（ADR-0011）で使えず、入れ替えの手順が`install`と別になる。
- **serviceが落ちたらクライアントがDBを直接開く**: DBのpathを実行側から外す段(3)の目的が崩れる。
- **socketのpeerの資格（uid・pid）で実行側を見分ける**: 全て同じユーザーで、pidからrunを辿るのは子孫のプロセスで崩れる。
- **`DAGQ_ROLE`を信じ続ける**: 偽装を防げず、段(4)のbrokerとも認証の形が揃わない。
- **resource brokerの署名つきtoken（ADR-t827-2）を流用する**: queue serviceはDBを持つので、失効と照合を自分で行える。形を揃えるかは実装のtaskが決める。

## Consequences

- serviceが落ちても、段(4)までは人とinboxに届く経路が残る。段(5)の`watch`は届かないことを知らせるので、「何も来ない」と「届かない」を見分けられる。
- claimの控えでserviceの停止中にrunが増えない。走っているrunのworkerはaskやnoteがerrorになるので、そのerrorを読んで待つかreceiptに書く。
- tokenの発行と失効の分、claim・resume・jobの起動に手順が増える。
