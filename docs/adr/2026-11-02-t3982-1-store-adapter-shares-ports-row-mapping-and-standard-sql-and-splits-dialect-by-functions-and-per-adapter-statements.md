---
id: adr-t3982-1
type: adr
title: queueのstoreのadapterは、portのtrait・domainの行の写し・標準のSQLの文をSQLiteとPostgresで共有し、接続・transactionの始め方・方言を分け、方言の違うSQLは式の断片を方言の関数に、文の形が違うものをadapterごとの文にし、大きな本文の置き場のportに閾値を超えるeventの本文を通す
status: accepted
created: 2026-11-02
updated: 2026-11-02
accepted_on: 2026-11-02
owners:
  - hisamekms
tags:
  - architecture
  - persistence
amended_by:
  - adr-t4098-1
related:
  - adr-t3981-1
  - adr-0013
  - adr-0063
  - adr-t3102-2
  - design-persistence
  - plan-team-version-sqlite-dialect
---

# ADR-t3982-1: queueのstoreのadapterの形（共有と分離・方言の切り出し方・大きな本文の置き場のport）

## Context

[ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)は、ローカルのSQLiteを既定の1級に残してPostgreSQLをadapterとして足すこと（決定3）と、書き込みの直列化をアプリの係とleaseとfencingで守りSQLiteとPostgresで同じ仕組みにすること（決定4）を決め、storeのadapterの形を後のADRに残した。段1（goal 249）はPostgresのcrateを足さず、schemaもローカルの動きも変えずに、足せる形へ分ける。

今のstoreはSQLite（rusqlite）だけで、applicationはportのtraitで読み、SQLとrusqliteは`src/infrastructure/`にある（[ADR-0013](0013-layered-architecture-and-type-function-style.md)の層）。SQLiteに固有の書き方の数とファイルは[棚卸し](../plans/team-version-sqlite-dialect.md)が持つ。要点は次のとおり。

- transactionの始め方の指定（rusqliteの種類）が34ファイル・219行にあり、portの外のstoreの関数が直接名指す。
- JSONの関数がsrcのSQLに213行あり、うち`json_extract`が180行で、大半がeventの本文（`payload`の列）を読む。日時の関数がsrcに13行、挿入した行のIDをrusqliteから取る所が17ファイル・26行ある。
- FTS5の検索（`dagq search`と`related`、[ADR-0063](0063-full-text-search-related-with-mentions-and-search-strength-and-duplicate-of.md)）は計画の記録のportに混ざり、索引はmigrationのtriggerが保つ。
- trigger・外部キー・AUTOINCREMENTの大半はmigrationsにある。
- ADR-t3981-1のContextのとおり本文が64 KBを超えるeventがあり、ADR-t3981-1決定1はlog・本文・大きなevent・古いeventと台帳をS3互換の置き場に置くとした。

## Decision

ADR-t3981-1の決定3（SQLiteを既定の1級に残しPostgresをadapterとして足す）と決定4（書き込みの係・lease・fencingをSQLiteとPostgresで同じ仕組みにし、DBには原子性と耐久性だけを求める）を保つ。このADRはどのADRもamendsも置き換えもしない。

1. **共有するものと分けるもの。** SQLiteとPostgresのadapterで共有するのは、applicationのportのtrait、domainの集約と行の間の写し（行から記録を組んで復元し、集約から書く値を作る）、両方の方言で同じ意味になるSQLの文の3つである。adapterごとに分けるのは、接続とその設定、transactionの始め方（決定2）、方言（決定3）、schemaとmigrationの系列（どう持つかは決定7のとおり後のADR）、検索の実装の5つである。
2. **transactionは名前付きの入口から始め、始め方はadapterが持つ。** portの側には書き込みと読み取りの名前付きの入口だけを置き、入口は始め方の種類を引数に取らない。SQLiteのadapterは書き込みをimmediateで、読み取りをdeferredで始め、rusqliteの始め方の種類を名指すのはその入口の中だけにする。ADR-t3981-1決定4の書き込みの係のleaseと世代の番号の確かめは、SQLiteとPostgresのどちらの書き込みの入口も始めた直後に持ち、確かめのSQLは共有のSQLにする。leaseの行はschemaを足すので、確かめを入れるのは段1でなくチームの版の段である。段1は名前付きの入口を入れ、入口の外のsrcがrusqliteの始め方の種類を名指さないようにする。
3. **方言の違うSQLは3つの形のどれかで扱う。**
   - 両方の方言で同じ構文が書けるもの（`ON CONFLICT`、外部キー、日時の文字列の比較など）は共有のSQLにし、方言の違う書き方から共有の書き方へ書き直してよい。
   - 違いが式の断片に収まるもの（JSONの欄の読み出し、日時の関数）は方言の関数にし、共有のSQLに埋める。方言の関数は読む欄と読む型（文字列・整数など）を受け、SQLiteの方言は今と同じSQLの文字列を返す。
   - 文の形か実行の仕方が違うもの（FTS5の検索と関連の問い合わせ、schemaの版と接続の設定の読み書き、挿入した行のIDの取り方）はadapterごとの文か関数にし、adapterの中に置く。挿入した行のIDは、SQLiteではrusqliteの呼び出しで、Postgresでは`RETURNING`で読むので、adapterの「挿入してIDを返す」関数にする。検索はstoreと別のportにし、検索のadapterがその文を持つ。
4. **段1で切り出すもの。** 段1は、srcのSQLの`json_extract`によるJSONの欄の読み出しと、srcのSQLの日時の関数（`datetime`・`strftime`・`julianday`）の2つを方言の関数に、挿入した行のIDの取り方をadapterの関数に切り出す。SQLiteの方言の関数は今と同じSQLを返し、IDの関数は今と同じrusqliteの呼び出しをするので、DBに流れるSQLの文字列とローカルの動きは変わらない。段1は決定2の名前付きの入口と、検索と関連を計画の記録のportから分けた検索のportとそのSQLiteのadapter（FTS5を読むSQLの置き場）も入れる。ほかのJSONの関数（`json_each`・`json_type`・`json_set`など）、`INSERT OR`の書き直し、rowidの並び順、`IS`で値を比べる所、型の緩さに頼るその他の所、migrationsの中の方言は段1で切り出さず、Postgresのadapterを足す段で決定3の形に当てる。FTS5の検索は方言の関数にはしない。
5. **大きな本文の置き場のportの形。** 大きな本文の置き場はstoreと別のportで、本文を鍵で置き・読み・消すだけを持つ。storeの行が持つのは本文そのものか本文の鍵で、行が鍵を持つのは置き終えた本文だけにする。本文はそれを参照する行のtransactionが確定する前に置き、確定しなかった本文（孤児）は残ってよく、後で片付ける。portはstoreのtransactionに加わらない。
6. **段1で大きな本文の置き場のportに通すもの。** 段1でportに通すのは、本文が閾値（ADR-t3981-1のContextの64 KBを目安にし、値はコードの定義のそばに置く）を超えるeventの本文である。portが置き場の外に置いたときだけ行は鍵を持ち、置かなかったときは本文をそのまま行に入れるよう返す。ローカルの既定の実装は外に置かず、本文を今と同じ場所（eventの行の本文の列）にそのまま入れさせるので、transactionに加わらず、行の形も読める値も変えない。runのdirectoryのファイル（log・prompt・receipt・review）は段1では今のrunのファイルのportに残し、チームの版でこのportに通すかとその時点は、runnerの仕組みを決める後のADRが決める。台帳の行と古いeventはSQLが集計して読む行なのでこのportに通さず、S3互換の置き場へ移す条件と読み戻し方はADR-t3981-1が後のADRに残したとおりにする。
7. **triggerの移し先とPostgresのmigrationの系列は決めない。** 検索の索引を保つtrigger、記録の変更を禁じるtrigger、その他のtriggerを、DBのtriggerのまま方言ごとに書くか書き込みのコードへ移すかと、Postgresのschemaとmigrationの系列（SQLiteの系列とどう対応させるか、型の選び方）は、このADRが決めず後のADRが決める。

## Alternatives

- **portのtraitだけを共有し、adapterごとにSQLを全部書く**（決定1）: 方言を気にせずに書けるが、SQLが2本になり、2つのadapterのずれを両方に流すtestでしか見つけられず、SQLを変えるtaskが2か所を直す。共有のSQLにすればずれは方言の関数とadapterごとの文の中に閉じる。
- **ORMかquery builderの上にSQLを書き直す**（決定1・3）: 方言の差を道具に任せられるが、全部のSQLを書き直すことになり、段1の動きを変えない制約のもとで同じSQLが流れることを確かめられない。crateを足す判断も要る。
- **SQLの文ごとに方言の版を持つ（文の表を2つ）**（決定3）: 違いが式の断片だけの文（JSONの欄の読み出しを持つ文は180行ある）まで2本になり、上の「SQLを全部書く」と同じ欠点を持つ。文の形が違うものだけをadapterごとの文にする。
- **PostgresにSQLite互換の関数（`json_extract`など）を作り、SQLを変えない**（決定3）: srcのSQLをそのまま流せるが、JSONを文字列で持つ型の緩さをPostgresへ持ち込み、索引の効く形と型の違いが見えなくなる。互換の関数はPostgresのmigrationの系列にも依るので、その系列を決めていない今は選べない。
- **段1で全部の方言を切り出す**（決定4）: 段1の差分が大きくなり、Postgresのadapterが無いまま形だけを決めることになる。最も数の多いJSONの欄の読み出しと、式に収まる日時の関数と、呼び出し1つに収まるIDの取り方だけを先に切り出し、残りは実際のPostgresのSQLと並べて当てる。
- **大きな本文の置き場をstoreのtransactionに入れる（2相のcommit）**（決定5）: 行と本文が必ず揃うが、S3互換の置き場はtransactionに加われない。行が置き終えた本文だけを参照し、孤児を許して片付ける。
- **段1で台帳の行・古いevent・runのファイルもportに通す**（決定6）: 本番のqueue.dbの大きさの大半（台帳の行とevent）を減らせるが、台帳の行と古いeventはSQLが集計して読み、置き場へ移す条件はADR-t3981-1が後のADRに残した。runのファイルはrunの間にプロセスが書き続けるので、置き場へ移す時点がrunnerの仕組みに依る。段1は形だけを入れられる閾値を超えるeventの本文に限る。

## Consequences

- 段1の後、SQLiteの方言の関数とadapterごとの文がPostgresの方言を足す場所になり、共有のSQLは両方のadapterで流せる形のまま残る。Postgresのadapterを足す段は、方言の関数のPostgresの版と、adapterごとの文と、まだ切り出していない方言（決定4の残り）を書く。
- 方言の関数のSQLiteの版は今と同じSQLを返すので、段1は今のtestと`EXPLAIN QUERY PLAN`の期待のまま確かめられる。
- 大きな本文の置き場のportはローカルでは今と同じ列に置くので、段1ではqueue.dbの大きさも、eventを読むSQLも変わらない。チームの版で本文を置き場へ出すとき、行に残す欄（SQLが読むeventの欄）を決める必要が残る。
- このADRは次の論点を決めていない。どれも後のADRが決める。
  - triggerの移し先（検索の索引の維持・記録の変更の禁止・その他）。
  - Postgresのschemaとmigrationの系列、列の型（JSON・日時・真偽）の選び方。
  - チームの版でeventの本文を置き場へ出すときに行に残す欄、runのファイルをportに通す時点、台帳の行と古いeventを移す条件。
