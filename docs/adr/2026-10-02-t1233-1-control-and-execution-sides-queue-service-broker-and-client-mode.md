---
id: adr-t1233-1
type: adr
title: 制御側（host）と実行側（隔離環境）を分け、queue serviceをqueue DBを開く唯一のプロセスにしてユースケース単位のAPIとservice側の認可を持たせ、hostの呼び出し元はunix socketで使い、brokerを実行側から制御側への唯一の出口にし、dagq CLIはserviceの宛先があればクライアントモードで動く
status: accepted
created: 2026-10-02
updated: 2026-10-02
accepted_on: 2026-10-02
amended_by:
  - adr-t1433-1
  - adr-t2113-3
  - adr-t2114-1
  - adr-t2114-3
owners:
  - hisamekms
tags:
  - runtime
  - security
  - architecture
related:
  - adr-t728-1
  - adr-t728-2
  - adr-t813-3
  - adr-t827-4
  - adr-t1063-1
  - adr-t1233-2
  - adr-t1233-3
  - adr-t1233-4
  - adr-t1233-5
  - design-security
  - design-authorization
  - design-persistence
---

# ADR-t1233-1: 制御側と実行側を分け、queue serviceとbrokerとクライアントモードで実行側からqueueへの経路を限る

## Context

今のworker・resume・headlessのjobはhostで動き、queue DBのpathを知り、DBを直接開ける。`DAGQ_ROLE`によるCLIの制限はenvの判定なので偽装でき、[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)決定6はhost実行を助言的と明記した。cargoのbuild.rs・proc-macro・testのように、workerが書いたコードもhostで動く（integrateのverificationを含む）。

goal 38（draft）は2026-09-26に人とplannerが方針を合意したが、未決の項目を残して着手していなかった。2026-10-01に人が、段(1)〜(3)を先に切り出し（goal 82）、e2eの置き場所とこのrepositoryのOSを決めた（[ADR-t1233-2](2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)・[ADR-t1233-3](2026-10-02-t1233-3-this-repository-passes-on-linux-before-containers.md)）。このADRはgoal 38の方針の文を決定にする。goal 38の未決のうち残りは、[ADR-t1233-4](2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)（serviceの起動・停止の責任、落ちたときの知らせ方、brokerより前のprincipalの認証）と[ADR-t1233-5](2026-10-02-t1233-5-read-use-cases-read-scope-by-role-and-codex-sandbox-reach.md)（読み取りのroleの読み方とroleごとの範囲、workerの読める範囲、Codexのsandboxからの到達）が決める。

## Decision

1. **制御側と実行側を分ける。** 制御側（supervisor・runner session（wrapper）・inbox・planner・cmux・Gitのpushの権限・queue service）はhostに置く。実行側（worker・resume・headlessのjob・integrateのverification）は隔離環境（まずsandbox、次にコンテナ）に置き、見せるのはworktreeとrun dirだけにする。例外はe2eで、実cmuxを使うのでreviewのpassの後に制御側がhostで流す（ADR-t1233-2）。cmuxを使うのは制御側だけで、実行側のagentは使わない。runner sessionが起動するagentをコンテナの起動に替えても、cmuxのTTY・キー送信・capture・Stop hook（run dirのmount）はそのまま動く。
2. **queue serviceをqueue DBを開く唯一のプロセスにする。** queue serviceはhostで動く制御側のプロセスで、最終の姿（段(5)）では他の全てのプロセスがserviceを通してqueueを使う。互換はqueueのschemaではなくAPIのversionで判定し、開発中のバイナリがqueueを開いてschemaを上げる問題を構造的に無くす。
3. **APIはユースケース単位。** メソッド単位のRPC（行の読み書き）ではなく、`ask`を開く・`note`を書くといったユースケース単位にし、leaseの検査を含むtransactionはserviceの中で完結させる。transactionをクライアントとの往復で分けない。
4. **認可はservice側。** serviceは呼び出しのprincipal（roleと対象のrun・job）を自分で確かめ、ADR-t728-1のdefault denyの静的なpolicyで判定する。クライアントが名乗るrole（`DAGQ_ROLE`）は認可に使わない。拒否と状態を変えるeventのactorは今までどおり記録する。
5. **hostの呼び出し元はunix socketで使う。** hostで動く呼び出し元（段(3)からはhost構成の実行側のクライアントモードのdagq、段(5)からはsupervisor・runner session・hostのCLIも）はqueue dirのunix socketでserviceを呼ぶ。段(5)までsupervisor・inbox・planner・人のCLIはDBを直接開く。socketのpathは信頼の境界ではなく、principalはADR-t1233-4のtokenで決める。
6. **brokerは実行側から制御側への唯一の出口。** コンテナの実行側はbrokerだけを通して制御側に届く。brokerはrun・jobごとのtokenを認証してprincipalを付けてserviceに転送し、egressのallowlist（crates.io・GitHubなど）も兼ねる。コンテナからはloopbackのTCPとtokenで使う（macOSのコンテナではunix socketのbind mountが安定しないため）。このbrokerは、fs・process・gitを仲介するresource broker（dagq-broker、[ADR-t827-4](2026-09-28-t827-4-worker-mcp-tools-audit-mode-and-relations.md)決定6）とは別物で、文書では「queueのbroker」と書いて区別する。
7. **dagq CLIのクライアントモード。** dagqは、serviceの宛先（socketかbroker）とtokenを環境で与えられればクライアントモードで動き、queue DBを開かずに同じコマンドをserviceのユースケースに写す。promptとskillが打つ`dagq ask`・`dagq show`・`dagq note`などのコマンドと出力の形は変えない。クライアントモードのdagqは、serviceに届かないときにDBを直接開くことへ戻らない（fail closed）。
8. **段と置き場所。** 段は (1) ADR、(2) queue serviceと少数のAPI（ask・show・proposal・note・finding、と読み取りのroleのjobが使う読み取り。supervisorはまだDBを直接開く）、(3) workerとjobのdagqをクライアントモードにしてDBのpathを渡さない、(4) queueのbroker（TCPとtoken、egressのallowlist）と実行環境のport（host → sandbox → コンテナ）、(5) supervisor・wrapper・CLIも全てservice経由にしてDBを直接開く経路を無くす、(6) integrateのverificationの隔離。どこで止めても価値が出る順にする。(1)〜(3)はgoal 82、(4)〜(6)はgoal 38が持つ。(2)〜(3)はhost構成のままで着地できる形にする。e2eの置き場所（goal 38の段(6)にあったもの）はADR-t1233-2が決める。

決定ごとの置き場所: 起動・停止の責任、落ちたときの知らせ方、principalの認証はADR-t1233-4、読み取りのユースケースとroleごとの範囲・workerの範囲・Codexのsandboxからの到達はADR-t1233-5。APIのpath・欄名・versionの綴り・socketの場所・envの名前は実装のtaskが`docs/design/`に書く。

## Alternatives

- **今のまま（envのroleとCLIの判定）**: 偽装とDBの直接の操作で迂回でき、隔離に進めない。
- **実行側にもDBを読み取り専用で見せる**: 読み取りは止められても、schemaの互換の問題とpathの露出が残り、コンテナに移すときにmountが要る。
- **メソッド単位のRPC**: leaseの検査とtransactionがクライアントとの往復に割れ、壊れたクライアントや悪意のあるクライアントが途中の状態を作れる。
- **brokerとqueue serviceを1つにする**: hostの呼び出し元（unix socket）と実行側（TCPとtoken、egress）では認証と露出が違い、分けた方が段ごとに入れられる。
- **段(5)まで1つのgoalにする**: 人は(1)〜(3)を先に切り出すと決めた（2026-10-01）。(3)だけでもhost構成で改ざんの経路の1つが閉じる。

## Consequences

- 段(3)の後、workerとjobのdagqはDBを開かず、roleの判定はserviceが行う。ただしhost構成では同じユーザーのプロセスがDBのファイルとtokenのファイルを探して読めるので、助言的であることは変わらない（ADR-t728-1決定6）。閉じるのはCLIを通る経路で、隔離は段(4)以降が足す。
- 段(5)までは、supervisor・inbox・planner・人のCLIはDBを直接開き続ける。serviceとsupervisorの両方がDBを開くあいだの書き込みの競合は今のSQLiteのtransactionとleaseの規則で扱う。
- 固定バイナリの運用（入れ替えは`install`と自動更新だけ）はserviceのバイナリにも当てはまる。
