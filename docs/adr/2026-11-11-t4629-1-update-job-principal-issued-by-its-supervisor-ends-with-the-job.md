---
id: adr-t4629-1
type: adr
title: 更新のjob（自動更新とrelease更新）に、起動元のsupervisorがjobの起動の前に依頼してserviceが発行し、jobの終わりとjobのprocessが居なくなった後の掃除で失効して起動元のsupervisorのsign_outと登録の終わりでは失効しない専用のprincipalを与え、jobはそのprincipalで送る（ADR-t3102-1決定2をamends）
status: accepted
created: 2026-11-11
updated: 2026-11-11
accepted_on: 2026-11-11
amends:
  - adr-t3102-1 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - security
related:
  - adr-t3102-1
  - adr-t1233-4
  - adr-t728-1
  - adr-0073
  - design-authorization
  - design-security
  - design-queue-service
---

# ADR-t4629-1: 更新のjobのprincipal

## Context

更新のjob（自動更新とrelease更新）は、supervisorのexecと終了に巻き込まれない独立のsessionで動き、handoffを受けた新しいsupervisorが見張りの期限の内に継ぐかを見届ける。
ところがjobは起動元のsupervisorの資格のfileで送るので、資格だけがsupervisorの寿命に縛られている。
[ADR-t3102-1](2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)決定2の表ではsupervisorのtokenは登録の終わりで失効するので、handoffを受けた後に起動元が終わってsign_outが届くと、jobのsupervisorsの読みと戻しの段は認証で失敗する。
2026-11-08の事故ではsign_outが届かずtokenが残ったので、jobは記録と戻しまで進んだが、それは偶然だった。

## Decision

1. **更新のjobに専用のprincipalを足し、決定2の表に行を足す。**

   | principal | 親（発行を依頼する） | 発行の時機 | 対象 | 失効 |
   | --- | --- | --- | --- | --- |
   | 更新のjob | 起動元のsupervisor（登録が生きている） | jobの起動の前 | そのjob | jobの終わり・jobのprocessが居なくなった後の掃除 |

   親と対象の検査と発行は決定2のとおり同じtransactionで行い、子の権限は親を越えない（[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)決定5）。
   起動元のsupervisorのsign_outと登録の終わりでは失効しない。
   同じ対象に同じ親が発行し直すと前のtokenは失効し、別の親の依頼はjobが終わるまで拒む。
   jobはAIを起動しない制御側の処理なので、このprincipalは信頼する制御側に入れ、[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)決定2が更新の処理をAI actorとして起動するときのために予約したroleとは別のroleにする。
2. **権限はjobが今送っている操作だけにする。** 読み取り、更新の記録とask、installの引き継ぎの要求と見張り、queueの管理（`queue.admin`）、serviceの起動と停止（`service.lifecycle`）、置き換え（`service.install`）を持たせる。claim・runのlease・着地・tokenの発行・supervisorの起動（`up`を含む）とそれ以外の状態を変える操作はdefault denyで拒む。`DAGQ_ROLE`はこのroleを名指さず、制御側が発行するprincipalだけにする。
3. **jobはこのprincipalで送る。** jobのコマンドは資格のfileを引数で受け、jobと、jobが同じ環境で打つ新しいbuildの確認・installの段はその資格で送り、jobの終わりに自分で返す。checkout・ビルド・`cargo install`の子へ資格を渡さない規則は保つ。

値はenv・argvに置かずfileに置く規則（[ADR-t1233-4](2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)決定4）と、hostでは認可が助言的であること（ADR-t3102-1決定5）はそのまま保つ。

## Consequences

- jobの資格はjobの寿命に結びつき、起動元のsupervisorが継ぎ手に代わった後も、継ぎ手の判定と戻した後の引き継がせまでjobが自分の資格で進められる。
- 受け付ける側（principal・policy・発行と失効・jobのコマンドの引数）を先に入れる。supervisorがjobの起動でこの資格を発行して渡す切り替えと、それを受け付けないbuildへ戻さない判定は後続のtaskが[ADR-0073](0073-kind-additions-are-compatible.md)のamendsとして書く。切り替えまでjobは今どおり起動元のsupervisorの資格で送る。
- 発行とjobのprocessとの結び付けの処理と順序は、定義のそばのdoc commentが持つ。

## Alternatives

- (i) jobが生きる間は起動元のsupervisorのtokenを失効させない（資格の寿命を延ばす）: 登録の終わったsupervisorのprincipalが残り、表の「失効は登録の終わり」を崩すので退けた。
- (ii) jobが継ぎ手のsupervisorのtokenのfileを読む: 別のprincipalへの成り代わりで、自動更新の設計が退けているので退けた。
- (iii) 今のまま認証の失敗でjobを終える: 継ぎ手の判定と、戻した後の引き継がせが成り立たないので退けた。
- (iv) 拒まれたら別のprincipalで送る: 成り代わりで、自動更新の設計が退けているので退けた。
