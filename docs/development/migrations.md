---
id: development-migrations
type: development
title: このrepositoryのmigrationの規則（足し方・Postgresの系列・番号・リリース済みのmigrationの不変）
status: current
created: 2026-10-03
owners:
  - hisamekms
tags:
  - persistence
  - conventions
related:
  - adr-t1453-2
  - development-testing
  - development-task-registration
---

# このrepositoryのmigrationの規則

`migrations/`と`migrations-postgres/`を足す・変えるときの今の規則。読むのは、migrationを足すworkerと、migrationを足すtaskを登録するplanner。migrationを変えたときに手元で流すtestは[手元の検証](local-checks.md)の「全体を比べるtest」、taskのverifyは[taskの登録](task-registration.md)の「推奨の組み合わせ」、schemaとmigrationの仕組みは[Persistence](../design/persistence.md)の「Database setup and migrations」が持つ。

## 足し方

- migrationは`migrations/NNNN_<name>.sql`を置くだけで足す（Postgresの側の1本は「Postgresの系列」）。一覧は`build.rs`が作るので`src/infrastructure/schema.rs`は編集しない。
- testは最新のschemaの番号を数字で書かず、`SqliteQueue::SCHEMA_VERSION`と`MIGRATIONS`から組み立てる。

## Postgresの系列

`migrations-postgres/`はPostgresのstoreの系列で、`migrations/`の番号の規則の外にある（[ADR-t3986-1](../adr/2026-11-03-t3986-1-write-leader-fencing-trigger-placement-postgres-migration-series-archive-local-import-and-store-tests.md)決定7）。版・互換の宣言・下限は`migrations/`の側の1本が持ち、両方のstoreで同じ版の数が同じschemaの意味になる。

- 基点の`base.sql`は、先頭行に書いた版のSQLiteのschemaを一度に作る。型の写しはそのファイルの先頭のコメントが持つ。基点より前のSQLiteのmigrationをPostgresに書き直さない。
- SQLiteのmigrationを足すときは、`migrations/NNNN_<name>.sql`の`<name>`と同じ名前の`migrations-postgres/<name>.sql`を同じ変更で足す。Postgresで要らない変更は`-- postgres: none`だけの1本にする。
- 対応は名前で取るので、`integrate`が番号を振り直してもPostgresの側は書き換えない。Postgresの側のファイルにはコメントでも4桁の番号を書かない（書くと`integrate`が振り直せず`needs_session`になる）。別のmigrationを名指すときは`<name>`を書く。
- `build.rs`は、基点より後のSQLiteのmigrationに対応のファイルが無い、対応の無いファイルがある、基点より後の名前が重なる（`base`を含む）、ファイルが4桁の番号や対応のmigrationの番号を書いている、のどれでもbuildを失敗させる。
- triggerを置かない（決定6）。triggerがSQLiteで行う書き込みは、そのPostgresのadapterの書き込みに置く。
- CHECK制約を置かない（[ADR-t876-1](../adr/2026-09-28-t876-1-no-sqlite-check-constraints-until-schema-is-stable.md)）。`scripts/check-migration-numbers.sh`が全てのファイルを検査する。

## 番号

番号はmainの次の空きにする。並行するrunと重なっても、足したmigrationが1つでその番号を他の変更が含まなければ`integrate`が振り直して着地させる。振り直せなければ`migration_number_taken`の`needs_session`でresumeされ、workerが次の空き番号へ振り直す（[ADR-0067](../adr/0067-migrations-are-listed-by-build-and-renumbered-on-landing.md)、仕組みは[integrate](../design/supervisor-lifecycle/integrate.md)）。

## リリース済みは不変

- リリース済みのmigrationは変えない（[ADR-t614-2](../adr/2026-09-27-t614-2-released-migrations-are-immutable.md)）。schemaを直すときは次の番号のmigrationを足す。
- `scripts/check-migration-numbers.sh`は番号の規則に加えて、`v0.3.0`より新しい最新の`v<X.Y.Z>`のtagの`migrations/*.sql`と`migrations-postgres/*.sql`が名前も中身も変わらず残っていることを検査し、変更・改名・削除を名前つきでexit 1にする（tagの無いcloneでは検査しなかったことを出して通す）。CIと`release.yml`が実行する。

## persistence.mdに書くこと

migrationを足すtaskは、[Persistence](../design/persistence.md)の、変えた表・列の今の姿を書く箇所だけを直す: 冒頭のschemaの木のその表の行（表の役割と、列についてコードから読めない約束）と、その表・列を説明する本文の節。列の一覧・既定値・null可の書き写しは足さず、意味が要れば定義のそばのdoc commentかmigrationのコメントに書く（[文書の規則](documents.md)の「design」、[ADR-t1942-1](../adr/2026-10-07-t1942-1-design-docs-in-four-layers-with-size-budgets.md)）。足した表は木に1行と、要れば本文の節を足す。

- 冒頭の段落にmigrationごとの経緯（どのmigrationが何を足したか、schemaの版）を足さず、木にmigrationごとの行（`-- 00NN: …`）を足さない。経緯は`migrations/*.sql`（先頭行の互換の宣言とコメント）とgitの履歴が持つ。
- 本文で根拠を名指すときは、migrationの番号（`integrate`が振り直すと古くなる）でなくADRかmigrationのファイル名の`<name>`の部分を名指す。既にある番号の言及は書き直さなくてよい。
