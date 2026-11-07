---
id: adr-t4098-1
type: adr
title: 実行のportは書き込みのtransactionの判定とcommit後の通知を持ち、通知の仕組みはadapterが持つ
status: accepted
created: 2026-11-06
updated: 2026-11-06
accepted_on: 2026-11-06
owners:
  - hisamekms
tags:
  - architecture
  - persistence
amends:
  - adr-t3982-1 decision 1
related:
  - adr-t3981-1
  - design-provider-lifecycle
---

# ADR-t4098-1: transactionの判定とcommit後の通知

## Context

sessionの区間を閉じる処理は、書き込みのlockの下でtranscriptを読まず、閉じた区間の稼働時間をtransactionがcommitしたときだけファイルに書く。
共有のSQLを実行する処理が接続の状態やSQLiteのhookに直接頼ると、Postgresのtransactionでこの約束を保てない。

## Decision

[ADR-t3982-1](2026-11-02-t3982-1-store-adapter-shares-ports-row-mapping-and-standard-sql-and-splits-dialect-by-functions-and-per-adapter-statements.md)の決定1をamendsし、adapterごとに分けるものにcommit後の通知の仕組み（hookかcommitの結果か）を足す。
実行のportのtransactionは、書き込みのtransactionの中かを答える操作と、そのtransactionがcommitしたときだけ行う処理を登録する操作を持つ。
rollbackとcommitの失敗では登録した処理を捨てる。
savepointの巻き戻しでは、その中で登録した操作も捨てる。
savepointをcommitした場合は外側のtransactionのcommitで処理を行う。

ADR-t3982-1の決定2・3とADR-t3981-1の決定3を保つ。
操作の名前と細部は実行のportの定義のdoc commentが持つ。

## Alternatives

- SQLiteのhookをportに出す: driverの仕組みを共有の処理へ持ち込み、Postgresにも同じhookを要求する。
- commit前にファイルへ書く: rollbackされたcloseの稼働時間が残る。
- savepoint内の登録を外側のtransactionに残す: closeの行だけが巻き戻されても、外側のcommitでファイルに記録される。

## Consequences

共有のsession処理は、driverを名指さずにtranscriptの読みと稼働時間の記録の約束を保てる。
各adapterは成功したcommitの通知とsavepointの登録の破棄を保証する。
