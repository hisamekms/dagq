---
id: adr-t3990-1
type: adr
title: チームの版のHTTPのAPIは今のユースケースの要求を1つのHTTPSの要求にそのまま載せ、dagqの短いtokenを標準の認可のheaderで受け、TLSは配置の側で終え、互換は要求ごとのAPIの版の範囲で判定して下限より古いrunnerが仕事を持つ間は下限を上げない
status: accepted
created: 2026-11-04
updated: 2026-11-04
accepted_on: 2026-11-04
owners:
  - hisamekms
tags:
  - architecture
  - runtime
  - security
  - queue-service
related:
  - adr-t3981-1
  - adr-t3985-1
  - adr-t3986-1
  - adr-t3987-1
  - adr-t1233-1
  - adr-t1233-4
  - adr-t3102-2
  - adr-t3102-3
  - adr-t3990-2
  - adr-t3990-3
  - adr-t3990-4
  - adr-t3990-5
  - design-queue-service
  - plan-team-version-control-side
---

# ADR-t3990-1: チームの版のHTTPのAPI（形・認証・版の互換）

## Context

[ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)は、制御側をcontainerのtask 1つとPostgreSQLとS3互換の置き場にし（決定1）、配置は利用するpjが持ってdagqは部品を配る（決定2）と決め、部品の形とHTTPのAPIを後に残した。人とrunnerの識別は[ADR-t3985-1](2026-11-03-t3985-1-team-version-identity-by-oidc-human-roles-on-staticpolicy-runner-registration-per-person-inbox-and-no-delegated-landing-approval.md)、書き込みの係は[ADR-t3986-1](2026-11-03-t3986-1-write-leader-fencing-trigger-placement-postgres-migration-series-archive-local-import-and-store-tests.md)、runnerのpullとleaseは[ADR-t3987-1](2026-11-03-t3987-1-team-version-runners-pull-under-fenced-run-leases-ai-jobs-on-runners-pinned-images-two-github-apps-and-attributed-claude-credentials.md)が決めた。この決定はそれらをHTTPのAPIに写す。

今のqueue serviceは、1つの接続に1つの要求で、要求はユースケースの名前・params・APIの版・宛てのqueueを持つ1つの文書、答えも1つの文書である。serviceはAPIの版の範囲、宛て、token、ユースケースの順に確かめ（[Queue service](../design/queue-service.md)の「API versionと互換」「ユースケース」）、hostの呼び出し元はunix socketで、containerのrunはqueueのbrokerのHTTPで届く。

具体の候補は設計資料の[チームの版の制御側の具体案](../plans/team-version-control-side.md)が持ち、実装のtaskが定義のそばのdoc commentへ移す。

## Decision

[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)（とくに決定2の互換をAPIの版で判定すること、決定3のユースケース単位のAPIとtransactionをクライアントとの往復で分けないこと）、[ADR-t3102-3](2026-10-22-t3102-3-stage-5-outage-notice-and-use-cases-for-multi-step-control-operations.md)（決定3〜5のユースケースの単位とleaseと前提の確かめ、副作用の順）、[ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)（とくに決定1・2）、ADR-t3985-1・ADR-t3986-1・ADR-t3987-1の決定は保つ。この決定はどれもamendsも置き換えもしない。ローカルの既定の配置（unix socketとqueueのbroker）は何も変えない。

1. **HTTPは今のユースケースの要求を運ぶ経路で、APIの単位を変えない。** チームの版の制御側の入口は、今のユースケースの要求1つをHTTPSの要求1つにそのまま載せ、答え1つを返す。ユースケースの集まり・params・答え・認可（serviceの確かめの順とStaticPolicy）は今の経路と同じコードを共有し、HTTPの層は運ぶことと大きさの上限と認証のheaderを読むことだけを持つ。資源ごとの読み書き（行のCRUD）の形にも、transactionを開いて閉じる要求にもしない。ユースケースの名前はHTTPの層から見える形に置き、logと上限の単位に使ってよい（形はplan）。runnerのpullも同じ形の1つの要求で、長く繋ぎっぱなしにしない（ADR-t3987-1決定1）。例外は、チームのAPI keyのClaudeの要求を運ぶ制御側の差し替え（ADR-t3987-1決定8）で、同じHTTPSの受けの別の入口に置き、ユースケースではなく上流へ流す中継として応答をstreamで返し、ユースケースの大きさの上限を当てない。その入口はrunnerのtokenとleaseの世代で認証し、今の世代のleaseのものだけを送り、使用量の記録だけを書き込みの列に並べる。
2. **認証はdagqの短いtokenをHTTPの標準の認可のheaderで受け、TLSは配置の側で終える。** 人・runner・制御側が割り当てで発行したjobとworkerのtokenは、どれもHTTPの標準の認可のheader（bearerの形）で送り、URL・query・bodyには置かない。発行元のOIDCのJWTを受けるのは、それをdagqのtokenに替えるユースケース（ADR-t3985-1決定1）だけである。制御側のcontainerは配置の側のTLSの終端（参考の構成ではALB）の後ろで受け、クライアントはloopbackでないHTTPSでない宛てを拒む。終端からcontainerまでの区間は配置の側のnetworkの中で、containerへの受けを終端からだけに絞ることを配置する側が守る（ADR-t3990-5決定1）。終端からcontainerまでも暗号にしたい配置のために、制御側は配置の設定で渡した証明書でTLSを自分で受けることもできる。識別と認可は送り元のIPと転送のheaderに依らない（IPの許可リストを前提にしない。ADR-t3981-1決定1）。
3. **版の互換は要求ごとのAPIの版の範囲で判定し、pathに版を入れない。** 今と同じく要求はAPIの版を持ち、制御側は自分の範囲（下限から今の版）の外を認証の前に断り、答えは範囲を持つ。HTTPのpathに版の番号を入れて版ごとの入口を並べることはしない。APIの版はdagqのreleaseの版と別の数で、古い呼び出し元が読めない変更のときだけ上げる。範囲の外と断られた古いCLIは、要る範囲と入れ替えの案内を出して止まり、DBやほかの経路へ戻らない（ADR-t1233-1決定7）。範囲の外と断られた古いrunnerは、断られた要求でleaseを返すこともできないので、新しい仕事を取らず、走っているcontainerを止めて理由をlogに出して終わる（そのleaseは期限で失われる）。これを起こさないよう、下限は[ADR-t3102-2](2026-10-22-t3102-2-only-the-queue-service-opens-the-db-schema-by-the-service-build-and-migration-order.md)決定4のとおり、下限より古い版のrunnerが仕事を持っている間は上げない: runnerの登録はrunnerの版を記録し、下限を上げた新しいbuildの制御側は、下限を下回るrunnerがleaseを持っている間は係を引き継がずに理由を示して待つ。配置する側は、その前に下限を下回るrunnerの仕事を返させる（drain）。上げる順は、制御側を先に新しい版に替えて範囲を広げ（下限は据え置く）、runnerとCLIが替わってから下限を上げる。

## Alternatives

- **資源ごとのREST（行の読み書き）にする**: leaseと前提の確かめがクライアントとの往復に割れる（ADR-t1233-1のAlternatives）。ユースケースをそのまま運ぶ（決定1）。
- **gRPCか常時の双方向の接続にする**: 別の層と道具が要り、間のproxyとload balancerで切れやすい（ADR-t3987-1のAlternatives）。HTTPSの1往復で足りる（決定1）。
- **発行元のJWTを毎回の要求で受ける**: ADR-t3985-1決定1が退けた。
- **送り元のIPやmTLSで識別する**: IPは配置の形（NATの有無）で変わり、許可リストを前提にしない人の決定に反する。mTLSはTLSの終端を配置の側に置く形と合わない。tokenで識別する（決定2）。
- **pathに版を入れて版ごとの入口を並べる**: 同じユースケースを版の数だけ持ち、今のAPIの版の範囲の判定と二重になる。要求の版で判定する（決定3）。
- **releaseの版の一致で互換を判定する**: releaseのたびに全てのrunnerとCLIを同時に替えることになる。APIの版を別に持つ（決定3、ADR-t3990-3決定2）。

## Consequences

- HTTPの層はユースケースを運ぶだけなので、ユースケースを足す変更はsocketとHTTPの両方で同じに効き、HTTPだけの欠けは生まれにくい。代わりに、資源の形のAPIを期待する外の道具はdagqのCLIを通す。
- 下限を上げる入れ替えは、下限を下回るrunnerの仕事が返るまで係を引き継げないので、配置する側に先のdrainが要る。
- 今の姿を書く`docs/design/`は、実装のtaskが着地と同じ変更で直す（この決定はまだ実装されていない）。
- この決定はPRとの連携のadapter（ADR-t3981-1決定7）を決めていない。
