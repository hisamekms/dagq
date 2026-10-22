---
id: adr-t3102-1
type: adr
title: 段(5)のqueue serviceのprincipalは、人（user）をhostのsocketのpeerの資格で表してtokenを持たせず、supervisor・integrator・session wrapperとhook・inbox・plannerには、起動する親のprincipalの依頼でserviceが発行して親と対象の終わりで失効させるtokenを与え、wrapperとhookはagentの環境から分けた専用のprincipalで動いてworkerのpolicyからsession.runとsession.recordを外す（ADR-t1233-4決定6・ADR-t1233-1決定5・ADR-t728-1決定6をamends）
status: accepted
created: 2026-10-22
updated: 2026-10-22
accepted_on: 2026-10-22
amends:
  - adr-t1233-4 decision 6
  - adr-t1233-1 decision 5
  - adr-t728-1 decision 6
owners:
  - hisamekms
tags:
  - runtime
  - security
  - architecture
related:
  - adr-t1233-1
  - adr-t1233-4
  - adr-t1233-5
  - adr-t3102-2
  - adr-t3102-3
  - adr-t728-1
  - adr-t728-2
  - adr-t728-3
  - adr-t1662-3
  - adr-t2114-1
  - design-queue-service
  - design-security
  - design-authorization
---

# ADR-t3102-1: 段(5)のprincipalと認証（人はsocketのpeer、制御側・inbox・plannerは親の依頼で発行するtoken）

## Context

[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)の段(5)（goal 182）は、supervisor・session wrapperとhook・人とinboxとplannerのCLIもqueue service経由にする。[ADR-t1233-4](2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)決定6は、そのときのwrapperとhook（session・session-event・planner-session）・inbox・plannerのprincipalと、人をtokenで表すかsocketの別の資格で表すかを段(5)に残した。いまserviceがtokenのprincipalとして受けるのはAI actorと、[ADR-t1662-3](2026-10-04-t1662-3-executors-send-telemetry-and-commands-are-classified-by-shape.md)のexecutorのtelemetry専用のprincipalだけで（`Principal::check`）、tokenはsupervisorなど制御側がservice dirのファイルに直接書いて発行する。runのwrapperとhookはworkerの環境のまま打つので、workerのpolicyが`session.run`と`session.record`を持つ。

goal 182の決定は3本に分けた。このADRはprincipalと認証、[ADR-t3102-2](2026-10-22-t3102-2-only-the-queue-service-opens-the-db-schema-by-the-service-build-and-migration-order.md)はDBを開くプロセスの例外・schemaを上げる経路・API versionの範囲・移行の順、[ADR-t3102-3](2026-10-22-t3102-3-stage-5-outage-notice-and-use-cases-for-multi-step-control-operations.md)はserviceが落ちたときの知らせとsupervisor・integrate・wrapperの操作のユースケースの単位を決める。

## Decision

1. **人（user）はhostのunix socketのpeerの資格で表し、tokenを持たせない。** tokenを持たずactorを名指さない呼び出しが、hostのserviceのsocketに、serviceと同じOSのユーザーとしてつないだときだけuserとして扱う。peerの資格はOSが接続に付けるもので、serviceはそれを自分で読む。actorを名指す環境があってtokenが無い呼び出しは、今までどおりuserに落とさず拒む（fail closed）。queueのbrokerを通る呼び出し（containerの実行側）はpeerがbrokerなのでuserにならず、brokerはtokenの無い要求をserviceへ送らない（[ADR-t2114-1](2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)決定2のまま）。tokenはuserの役割に発行しない（serviceはuserのtokenを受けない）。hostでは呼び出し元は全て同じOSのユーザーなので、host上でこの判定がAI actorと人を分けるのは「tokenもactorの環境も無い」ことで、今の`DAGQ_ROLE`が無ければuserという規則（ADR-t728-1決定6）と同じ強さにとどまる。peerの資格が足すのは、hostのsocketの外（brokerを通る経路）からuserになれないことである。
2. **制御側・inbox・plannerのtokenは、それを起動する親のprincipalの依頼でserviceが発行する。** 発行はserviceのユースケースにし、親が子を起動してよいかをADR-t728-1のdefault denyのpolicyで判定し、子の権限は親を越えない（[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)決定5）。親と対象に結びつく前提（leaseを持つこと、登録が生きていること）の検査と発行は同じtransactionで行う。AI actorにtokenを作るコマンドは持たせない（ADR-t1233-4決定4）。AI actorが親になるのは、そのroleにpolicyが許す操作に付くtokenだけで、inboxが`integrate`で着地を始めるときのintegratorのtokenがそれに当たる（着地を始めるユースケースが依頼者の`landing.request`を確かめ、その処理の一部として発行する）。

   | principal | 親（発行を依頼する） | 発行の時機 | 対象 | 失効 |
   | --- | --- | --- | --- | --- |
   | supervisor | user（`up`・launchd・人の`supervise`） | supervisorの登録 | その登録 | 登録の終わり（停止・staleの掃除）。execの引き継ぎは同じ登録を継ぐ |
   | runのsession wrapperとそのhook | supervisor（そのrunのleaseを持つ） | claim・resume・引き継ぎでwrapperを起動する前 | そのrun | runの終わり・leaseの喪失・resumeでの発行し直し |
   | plannerのsession wrapper（planner-session）とそのhook | supervisor | runtimeのplannerを開く前 | そのplanner | plannerを閉じたとき |
   | integrator | 着地を依頼するprincipal（`landing.request`を持つsupervisor・user・inbox） | 着地のslotを取るユースケースの中 | そのrun（leaseつき） | `integrating`から出たとき |
   | inbox | user（人が打つinboxを開くコマンド） | inboxのsessionを開く前 | そのinboxのsession | sessionの終わり・開き直し |
   | planner | supervisor | runtimeのplannerのturnを起動する前 | そのplanner | plannerを閉じたとき |

   置き場は今のworker・jobのtokenと同じ規則にする: 値は呼び出し元のenvとargvに置かずファイルの場所を渡し、ファイルはagentのworktree・run dir・plannerのdirの外に置く（ADR-t1233-4決定4）。制御側のtokenはcontainerにmountせず、brokerは受けない。同じ対象に発行し直すと前のtokenは失効する。workerとjob（`observe`などのコマンドが起動するものを含む）とexecutorのtelemetryのtokenも、発行者・時機・失効は今のまま（ADR-t1233-4決定4、[ADR-t1662-3](2026-10-04-t1662-3-executors-send-telemetry-and-commands-are-classified-by-shape.md)決定11）で、同じ発行のユースケースを通す。
3. **serviceがtokenのprincipalとして受ける範囲を、roleごとの対象の形で広げる。** 受けるのは、今のAI actor（worker・job・observer）とexecutorのtelemetryの形に加えて、決定2の表のsupervisor・wrapper・integrator・inbox・plannerで、表の対象を1つ持つものだけにする（inboxとplannerは対象を持つ形になる）。userと、対象の形がroleに合わないものは拒む。telemetryはADR-t1662-3決定9〜11のまま別のprincipalで送り、wrapperのprincipalに`telemetry.report`を足さない。
4. **wrapperとhookはagentの環境から分けた専用のprincipalで動き、workerのpolicyから`session.run`と`session.record`を外す。** runのwrapperは自分のrunの、planner-sessionは自分のplannerの`session.run`と`session.record`を、wrapperのprincipalで呼ぶ。wrapperの資格はagentのenv・argv・worktree・run dirに渡さないので、agentの子として起動されるもの（agentのpluginのhookを含む）はwrapperとして呼べない。hookの記録は全て、wrapperが受け取って自分のprincipalで送る形にする（受け渡しの形は実装）。inboxのsessionのhookの記録はinboxのprincipalのまま（inboxはuserと同じ権限。ADR-t728-3）。wrapperのprincipalの`session.run`と`session.record`は自分の対象だけに限る。workerのpolicyからこの2つを外すのは、runのwrapperとhookがwrapperのprincipalで動く変更と同じ着地で行う。同じ理由でplannerのpolicyのこの2つも、planner-sessionがwrapperのprincipalで動く変更と同じ着地で外す。
5. **hostでは助言的のまま。** host構成では同じユーザーのプロセスが、tokenのファイルを読むことも、環境を外してsocketにつなぎuserとして呼ぶこともできる。peerの資格もtokenも、誤りを止めて記録を正しくするためのもので、security boundaryではない（ADR-t1233-4決定5、ADR-t728-1決定6）。境界になるのは、containerの実行側がhostのsocketに届かず、brokerがrunのtokenだけを通す範囲である。
6. **ADR-t1233-4決定6・ADR-t1233-1決定5・ADR-t728-1決定6を変える範囲。** ADR-t1233-4決定6の未決（wrapperとhook・inbox・plannerのprincipalの形、userの表し方）をこの決定1〜4で決める。ADR-t1233-1決定5の「principalはADR-t1233-4のtokenで決める」は、userだけはsocketのpeerの資格で決めると改める。socketのpathが信頼の境界でないこと、hostの呼び出し元がunix socketで使うことは変えない。ADR-t728-1決定6の「`DAGQ_ROLE`が無い呼び出しをuserと扱う」は、serviceでは決定1の判定に改める。hostが助言的であることは変えない。

tokenとpeerの資格の書式、発行のユースケースの名前、置き場のpath、envの名前、role名の綴りは実装のtaskが[docs/design/](../design/)（[Queue service](../design/queue-service.md)・[Authorization](../design/authorization.md)）に書く。

## Alternatives

- **userもtokenで表す（`up`か`init`がuserのtokenをqueue dirに置く）**: hostでは同じユーザーなら誰でも読めて、peerの資格と同じく助言的なのに、発行・置き換え・漏れたときの失効の手順が増える。tokenのファイルを読めればuserになれるので、AI actorの環境から外しても強さは変わらない。人の端末からの承認など、人だけが持つ資格が要るときに別のADRで決める（[Security](../design/security.md)の将来の道筋の4）。
- **userを`DAGQ_ROLE`が無いことだけで決め続ける**: hostでの強さは決定1と同じだが、brokerを通る経路と区別する手がかりをserviceが持てず、`DAGQ_ROLE`を認可に使わない規則（ADR-t1233-1決定4）とも合わない。peerの資格はserviceが接続から自分で読める。
- **制御側のtokenを今のように制御側がservice dirに直接書いて発行する**: 発行がleaseと登録の検査と別のtransactionになり、leaseを失ったsupervisorがwrapperのtokenを作れる。
- **wrapperとhookをworkerのtokenで動かし続ける**: agentもwrapperと同じ権限で`session`と`session-event`を打てる穴が残る（goal 38のnote 30899）。
- **wrapperにtelemetryも持たせて1つのprincipalにする**: ADR-t1662-3決定9が分けた狭いprincipalを広げ、そのADRを変えることになる。

## Consequences

- 人のCLIはtokenの準備なしにserviceを使え、AI actorが環境を偽らない限りuserにならない。偽ればなれるのは今と同じ（助言的）。
- supervisorの登録・wrapperの起動・着地・inboxとplannerを開く手順に、発行と失効のユースケースの呼び出しが加わる。
- workerとplannerは自分のsessionの記録を書けなくなり、記録の書き手はwrapperのactorになる。
