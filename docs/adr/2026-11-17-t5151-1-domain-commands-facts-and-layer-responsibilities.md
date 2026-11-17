---
id: adr-t5151-1
type: adr
title: 集約のコマンド・事実・eventをdomainに揃え、applicationとstoreの責務を分ける
status: accepted
created: 2026-11-17
updated: 2026-11-17
accepted_on: 2026-11-17
amends:
  - adr-0013 decision 3
  - adr-0013 decision 8
  - adr-0013 decision 9
owners:
  - hisamekms
tags:
  - architecture
  - domain
  - persistence
related:
  - adr-t1410-1
  - adr-t3986-1
  - design-domain-model
  - design-persistence
  - design-architecture
---

# ADR-t5151-1: 集約のコマンド・事実・eventをdomainに揃え、applicationとstoreの責務を分ける

## Context

[ADR-0013](0013-layered-architecture-and-type-function-style.md)は、3層と「型＋関数」、集約のカプセル化、transactionの維持を決めている。
しかし、集約を保存するSQLが遷移の可否を確定したり、storeがeventのkindとpayloadを作ったりすると、domainの関数だけでは業務の規則を確かめられない。
他の行や履歴が要る判断も、値を渡す境界が曖昧なままではapplicationとstoreに残る。

このADRはADR-0013の決定3の返り値と、追補の決定8の「完了か追加情報が必要」、決定9のtransactionの述語の意味を具体化する。
層の分け方と型＋関数のstyle、新規作成と復元の区別、時刻・IDの注入は保つ。
CLIの出力・exit code・JSONの欄名、eventのkind・順・payload、理由のコードとschemaの意味は変えない。

## Decision

### 1. 集約のコマンドは新しい状態と記録するeventを返す

Task・Goal・TaskRun・Proposal・Askなどを変えるdomainのコマンドは、集約の所有権と入力を受け、成功時に新しい集約と記録するeventの列、拒否時にdomainのエラーを返す。
返り値は現在の`run::Recorded`に倣う「集約とeventの列」の組を基本とし、集約ごとのdomainの型で表す。
eventの型はdomainが所有し、kindとpayloadと順序はコマンドが決める。
記録しない成功は空の列で表し、storeが記録の要否を補わない。
storeは返った集約とeventを同じtransactionで保存するだけで、kindを選ばず、payloadを組み立てたり追記したりしない。
保存に要る事実もコマンドに先に渡す。
既存のeventの契約を保つための形であり、新しいevent体系や汎用の実行フレームワークを導入するものではない。

### 2. storeは同じtransactionで事実を読みdomainに渡す

判断に要る他の行（runの状態・lease・最新のrun・保留の印など）は、storeが対象の集約と同じtransactionで読み、domainが定義する事実のstructに詰めてコマンドへ渡す。
事実の型は操作に必要な観測を表し、「許可された」のような業務判断の結果に置き換えない。
domainはI/Oをせず、集約・入力・事実の型に無い条件を暗黙に仮定して判断しない。
行が無いことも事実として表し、取得の失敗を「無い」に読み替えない。

まずは判断に要る事実を一度に渡す。
先読みのコストが問題になる場合だけ、domainが完了または追加情報の要求を型で返し、applicationがportを通して取得を実行し、domainを再開する。
途中の状態は非公開の中間型に閉じ、必要な情報と次の業務操作をapplicationが推測しない。
永続化する判断に使う事実の取得と再開は同じtransaction内に置き、途中でtransactionを閉じて古い観測に基づく確定をしない。
この形で扱う集約の判断をapplicationへ残す理由に、追加情報の必要性を使わない。

### 3. SQLは候補の絞り込みと並行の変更の検出を担う

書き込み対象を選ぶSQLのWHEREは候補を絞る条件と、読んだ状態からの並行の変更を見つける条件に限る。
どの候補を選び、どの遷移を許すかの確定はdomainのコマンドまたはドメインサービスが持ち、SQLが選んだことだけを許可の根拠にしない。
`WHERE status=<読んだstatus>`・lease・tokenの条件付き更新は保ち、更新件数が合わなければ状態とeventをともに書かずに競合として扱う。
transactionの述語を外へ出さず、`renew_lease`・`BEGIN IMMEDIATE`と、contextをまたぐtransactionの所有と公開する書き込み口も保つ（[architectureのX1〜X3](../design/architecture.md#transactionの規則)）。

claimのready条件などSQLの絞り込みとdomainが同じ規則を表す所では、domainを正とする。
絞り込みはdomainが許す候補を落とさず、domainは取得した候補を確定前に判定する。
両方が表す適格性の答えが一致することは、同じ事実をDBの行とdomainの入力にした境界のtestで比べる。
SQL側の結果をdomain側の入力や期待値に流用せず、SQLに落とされる候補も含めて比べ、取りこぼしも検出する。
判断の条件の組み合わせはunit test、SQLとの対応・競合時に何も残らないこと・状態とeventの原子性は代表の境界のtestで確かめる（[ADR-t1410-1](2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)）。

### 4. 集約をまたぐ規則はcontextごとのドメインサービスに置く

依存グラフ・claim・優先度の伝播など、1つの集約に閉じない規則は、値を受け値を返すI/Oの無いdomainのmoduleに置く。
置き場は`src/domain/`の所有するcontextに対応するmoduleの下を基本とし、既存の配置を使う場合もcontextの所有を保つ。
名前は業務の概念と規則の目的を表すものにし、汎用の判断・policy・serviceという名だけの共有の巨大moduleを作らない。
1つのmoduleは一緒に変わる規則に絞り、複数のcontextの方針を1つの分岐に集めない。
例えばAsk自身は回答と終了の規則を持ち、回答をどの操作に適用するかは各contextのdomainが自分の方針を持つ。
contextをまたぐ結果の永続化は既存の所有とtransactionの境界を通し、ドメインサービスの追加でその境界を広げない。

### 5. 公開境界は検証付きの生成と復元に分ける

集約の欄は非公開とし、作る入口はdomainの検証付きconstructor、変える入口はコマンドに限る。
不正な組み合わせを後から検査するより、enumなどで不正な状態を型として作れなくすることを優先する。
保存済みの行はrestoreから復元し、復元に必要な不変条件を確かめるが、作成時の規則を当て直さず、作成のeventも再発行しない。
行の変換と保存形式はinfrastructureが持ち、公開の欄やデシリアライズでdomainの入口を迂回させない。

### 6. applicationは取得・実行・永続化と生成の注入を担う

applicationはportを通した事実の取得、domainが返す操作の実行と永続化、時刻とIDの生成の注入を担う。
履歴の解釈、次の業務操作の選択、遷移の確定はdomainへ置き、applicationが同じ規則を再判定しない。
Generators・Clockなど注入された仕組みから得た値をdomainへ渡し、1つの業務操作で必要な基準時刻を揃える。
storeがtransaction内でコマンドを呼ぶ場合も、applicationのportによる操作の内側で決定2・3を守り、transactionの外で判断を済ませない。
配線とI/Oの失敗の扱いはapplication、DB固有の復元・書き込み・競合検出はinfrastructureが担う。

## Alternatives

- **storeに判断を残し、domainの関数を検査だけに使う。** SQLとstoreが実際の操作を決めるままでは、domainのunit testが通っても実行経路の規則を保証できず、eventの内容も別の判断になるため採らない。
- **DBのtrigger・CHECKに業務の規則を置く。** 判断がDBの言語とadapterごとに分かれ、domainだけで確かめられなくなるため採らない。
  [ADR-t3986-1決定6](2026-11-03-t3986-1-write-leader-fencing-trigger-placement-postgres-migration-series-archive-local-import-and-store-tests.md)による既存のSQLiteのtriggerの維持とPostgresへの移し先は変えず、新しい業務判断の置き場にはしない。
  [persistenceの「CHECK制約を使わない」](../design/persistence.md#check制約を使わない)を保ち、NOT NULL・UNIQUE・主キー・外部キーなど既存の整合の制約や一意の索引も除かない。

## Consequences

判断とeventが同じdomainのコマンドに揃い、事実を値で与えるunit testで業務の規則を確かめられる。
storeには事実の変換とdomainの返り値を保存する処理が要り、SQLの絞り込みにはdomainとの対応を確かめる境界のtestが要る。
実装への適用は段階的に行い、単に関数を移すだけでなく集約の状態の表し方と公開境界も見直す。
このADR自体はコード・schema・外部仕様を変えない。
