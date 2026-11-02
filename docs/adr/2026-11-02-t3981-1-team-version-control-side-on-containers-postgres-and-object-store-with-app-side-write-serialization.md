---
id: adr-t3981-1
type: adr
title: チームの版は、制御側をcontainerのtask 1つとPostgreSQLとS3互換の置き場にし、配置は利用するpjが持ちdagqは部品を配り、ローカルのSQLiteを既定のまま残し、書き込みの直列化をアプリの係とleaseとfencingで守り、runnerはpullし、integratorを検証とmainを進めることに分け、人の関門をapprove_landingで、ownerと資格情報を足す（2026-11-02の人の決定）
status: accepted
created: 2026-11-02
updated: 2026-11-02
accepted_on: 2026-11-02
owners:
  - hisamekms
tags:
  - architecture
  - runtime
  - security
  - queue-service
related:
  - adr-t1233-1
  - adr-t3102-1
  - adr-t3102-2
  - design-queue-service
  - design-persistence
---

# ADR-t3981-1: チームの版の構成（制御側・配置の分担・store・書き込みの係・runner・integrator・人の関門・owner・資格情報）

## Context

dagqは今、1人の人が1台のhostで使う形で動く。queue DBはローカルのSQLiteで、queue serviceがそれを開く唯一のプロセスであり（[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)決定2、[ADR-t3102-2](2026-10-22-t3102-2-only-the-queue-service-opens-the-db-schema-by-the-service-build-and-migration-order.md)決定1）、制御側と実行側を分け、実行側はclient modeでserviceに届く（ADR-t1233-1決定1・7）。人はhostのsocketのpeerの資格で表す（[ADR-t3102-1](2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)決定1）。ownerは1人、askは1つのinboxに届き、着地はmainへ直接で、runnerはsupervisorと同じプロセスにいる。

人は、物理的に別の場所とデバイスでオンラインに作業するチームで使う版を求めた（request 213）。最初の段はECSとRDSのinstanceに置くこと、配置（containerのimage・ALB・RDS・S3）は利用するpjが持つこと、今lockで守っていることをDBでなくアプリの責務にすること、taskとgoalにownerを決めてaskをownerへ向けること、integrationに人の関門を入れることが人の問いにある。2026-11-02に人はaskへの答えで次を決めた: 範囲は制御側の置き場と書き込みの責務の移し替え・ownerとaskの宛先をやり、人の関門はPRでなくdagqの承認で入れる。優先度はhighでgoal 182・38の後。試行は別の試行用repositoryで行い後で移す。Claudeの資格情報は両方を選べる。優先度の裁定は管理者の役。関門はowner本人が承認してもよい（コードを書くのはAI）。IPの許可リストは前提にせず両方の形を文書に書く。この決定はその答えと、人と対話したinboxが添えた前提を決定にしたもので、選ぶ余地の残る論点は後のADRが決める。

参考の数字（2026-11-02の本番queue。決定の根拠の目安で、決定の値ではない）:

- 本番のqueue.dbは2.3 GBで、約0.4 GB/週で増える。表ではledger_rowsが1.29 GB、run_eventsが0.74 GB。
- 本文が64 KBを超えるeventは13件。
- 2026-10-31の46件の着地の検証は、中央値240秒、p90約17分。slotの3〜6割を検証が使っている。

## Decision

ADR-t1233-1（制御側と実行側を分ける・queue service・client mode）、ADR-t3102-1（principalの表し方）、ADR-t3102-2（DBを開くのはqueue serviceだけ・schemaはserviceのbuildが決める）の決定は保つ。この決定はどれもamendsも置き換えもせず、その上にチームの版の配置を足す。

1. **制御側はcontainerのtask 1つとPostgreSQLとS3互換の置き場。** チームの版の制御側は、queue serviceとsupervisorのループを動かすcontainerのtask 1つ、PostgreSQL、S3互換の置き場（log・本文・大きなevent・古いeventと台帳）で成る。参考の構成はECS Fargate＋RDS for PostgreSQL（Multi-AZ）で、taskはpublicなsubnetに置いてALBからだけ受け、NATなしを既定にする。NATと固定IPを置く形も文書に書き、IPの許可リストは前提にしない。
2. **配置は利用するpjが持ち、dagqは部品を配る。** ECS・ALB・RDS・S3の配置は利用するpjが持つ。dagqが配るのは、arm64とamd64のimage、設定の入口（DB・S3互換・OIDCの発行元・GitHub App）、health check、SIGTERMでの安全な停止、migrateを別に流すコマンド、配置する側が守ることの文書、参考の構成である。特定のクラウドに縛らず、DBはPostgresの接続の文字列、置き場はS3互換のAPI、人の識別はOIDCのJWTで受け、JWTの検証はdagqが行う。
3. **ローカルのSQLiteを既定の1級のまま残し、Postgresはadapterとして足す。** ローカルの既定の動き（ownerは1人、askは1つのinbox、着地はmainへ直接、runnerは同じプロセス）は変えない。storeの種類と接続先は`dagq.toml`でなく配置ごとの設定に置く。
4. **書き込みの直列化はアプリの責務。** queue serviceに書き込みの係を1つ置き、全部の書き込みのtransactionをその列に並べる。係が1つであることは、DBのリーダーの行（lease）と世代の番号（fencing）を、書き込みのtransactionの最初に条件付きの更新で確かめて守る。SQLiteとPostgresで同じ仕組みにし、advisory lockは使わない。大事な不変条件は条件付きの一意の索引で二重に守ってよい。DBに求めるのは原子性と耐久性だけである。
5. **runnerは外向きのHTTPSでpullする。** runnerは制御側へ外向きのHTTPSで仕事を取りに行き、制御側からrunnerへは繋がない。他の人のtaskを受けるrunnerはcontainerを必須にする。gitはGitHubのremoteを正にし、runごとにcloneする。
6. **integratorを検証とmainを進めることに分ける。** 検証（rebase・pathsの検査・migrationの番号の振り直し・reviewのプログラム・着地の検証・squash）は選ばれたマシンが行う。選ぶ順は常設のマシンと登録したチームのマシン、控えにself-hostedのrunner、最後にGitHub Actionsである。mainを進めるのは制御側で、GitHub Appによるfast-forwardのときだけ成功するrefの更新で行う。制御側はマシンの報告（S・M・treeのhash・imageのdigest）を照合してから進める。着地は当面直列にし、slotの使われる割合が7割を超え始めたら、merge trainと、関係のないrunの並列の着地を検討する。
7. **人の関門はPRを使わずapprove_landingを広げる。** 関門を開く条件（宣言したpath・reviewの確信度low・goalの印）は`dagq.toml`で宣言する。承認できるのはOIDCで識別した人だけで、inboxの代行は弾く。owner本人の承認でよく、two-person ruleは既定にせず設定で足せる。askにはGitHubの比較の画面のURLとreviewの要約を添える。承認を持ち越すのは差分の中身（patch-id）が同じときだけにする。承認した人とcommitを、eventとsquash commitのmessageに残す。PRとの連携は後のadapterの選択肢とする。CIは今どおりmainの後の最終関門である。
8. **ownerと優先度の裁定。** goalのownerを必須にし、taskはgoalのownerを継ぐ。ownerの代わりに答える人と、答えが無いときの繰り上げを持つ。運用のaskは当番へ向ける。high以上の優先度は管理者の役だけが付け、各自はnormalまで付けられる（優先度の裁定は管理者の役）。ownerをまたぐ依存は、plannerが推奨を付けて依存される側のownerに知らせる。
9. **Claudeの資格情報は2つから選べる。** チームのAPI key（brokerが差し替える）と各自の契約の両方を選べる。どのjobをどの契約で動かしたかの記録、予算の上限、各自の契約を他人のtaskに使わない線引きを持つ。
10. **順番と試行の場。** 段1の下ごしらえ（ローカルのSQLiteだけで完結する、goal 249）を先に行い、残りはgoal 182・38の後に行う。試行は人が選ぶ別の試行用repositoryで行い、dagqの本番queueはSQLiteのまま残し、安定した後に移すかを決める。

## Alternatives

- **PRを人の関門にする**: 人は、人の関門が付けばPRである必然は無いと答えた。dagqの承認で入れ、PRとの連携は後のadapterの選択肢に残す（決定7）。
- **DBのlock（advisory lockなど）で書き込みを直列にする**: SQLiteとPostgresで守り方が分かれ、DBの種類ごとに不変条件の守り方を持つことになる。人の問いのとおりアプリの責務にし、DBには原子性と耐久性だけを求める（決定4）。
- **dagqが配置（クラウドの資源）まで持つ**: 特定のクラウドに縛られ、利用するpjの組織の決まりと衝突する。配置は利用するpjが持つ（決定2）。
- **Postgresに一本化してSQLiteを外す**: 1人のローカルの使い方を重くし、今の本番queueを動かせなくなる。SQLiteを既定の1級に残す（決定3）。
- **制御側からrunnerへ繋ぐ**: runnerの置き場所ごとに入口の穴が要る。外向きのpullにする（決定5）。
- **IPの許可リストを前提にする**: 人は前提にしないと答えた。NATなしを既定にし、NATと固定IPの形も文書に書く（決定1）。

## Consequences

- 段1（goal 249）はローカルのSQLiteだけで完結し、Postgresのadapterを足せる下ごしらえを先に進められる。チームの版の本体は後のgoalで、goal 182・38の後に流れる。
- dagqの本番queueは試行の間もSQLiteのままで、チームの版の不具合が本番queueに及ばない。移すかは安定した後に人が決める。
- 書き込みの係とfencingはSQLiteとPostgresで同じ仕組みなので、ローカルでもleaseと世代の番号を確かめる手間が書き込みのtransactionごとに掛かる。
- 人の関門の承認はOIDCの識別を要するので、inboxが人の代わりに着地を承認する今の運用はチームの版の関門には使えない。
- この決定は次の論点を決めていない。どれも後のADRが決める。
  - storeのadapterの形: SQLiteとPostgresで共有するものと分けるもの、方言の違うSQLの切り出し方、大きな本文の置き場のportを通すもの。
  - Postgresのschemaとmigrationの持ち方、ローカルのSQLiteのqueueをチームの版へ移す手順。
  - 古いeventと台帳をS3互換の置き場へ移す条件と、読み戻し方。
  - 書き込みの係のleaseの期限と引き継ぎ、制御側のtaskが入れ替わるときの止まり方。
  - OIDCの発行元の設定の形、人のprincipalとtokenの発行、管理者と当番の役の表し方。
  - runnerの登録と認証、仕事の割り当て方、containerのimageの配り方。
  - 検証をするマシンの選び方の細目、マシンの報告の照合の形、GitHub Appに与える権限の範囲。
  - 関門の宣言の書式、two-person ruleの設定の形、承認の持ち越しの細目。
  - ownerの代わりに答える人の繰り上げの時間、当番の決め方、ownerをまたぐ依存の知らせ方。
  - 資格情報の予算の単位と上限の超え方、brokerの差し替えの仕組み。
  - チームの版のinbox（人ごとか共有か）とplannerの置き場所。
  - backupと復元、監視と指標。
