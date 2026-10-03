---
id: development-migrations
type: development
title: このrepositoryのmigrationの規則（足し方・番号・リリース済みのmigrationの不変）
status: current
created: 2026-10-03
updated: 2026-10-03
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

`migrations/`を足す・変えるときの今の規則。読むのは、migrationを足すworkerと、migrationを足すtaskを登録するplanner。migrationを変えたときに手元で流すtestは[手元の検証](local-checks.md)の「全体を比べるtest」、taskのverifyは[taskの登録](task-registration.md)の「推奨の組み合わせ」、schemaとmigrationの仕組みは[Persistence](../design/persistence.md)の「Database setup and migrations」が持つ。

## 足し方

- migrationは`migrations/NNNN_<name>.sql`を置くだけで足す。一覧は`build.rs`が作るので`src/infrastructure/schema.rs`は編集しない。
- testは最新のschemaの番号を数字で書かず、`SqliteQueue::SCHEMA_VERSION`と`MIGRATIONS`から組み立てる。

## 番号

番号はmainの次の空きにする。並行するrunと重なっても、足したmigrationが1つでその番号を他の変更が含まなければ`integrate`が振り直して着地させる。振り直せなければ`migration_number_taken`の`needs_session`でresumeされ、workerが次の空き番号へ振り直す（[ADR-0067](../adr/0067-migrations-are-listed-by-build-and-renumbered-on-landing.md)、仕組みは[integrate](../design/supervisor-lifecycle/integrate.md)）。

## リリース済みは不変

- リリース済みのmigrationは変えない（[ADR-t614-2](../adr/2026-09-27-t614-2-released-migrations-are-immutable.md)）。schemaを直すときは次の番号のmigrationを足す。
- `scripts/check-migration-numbers.sh`は番号の規則に加えて、`v0.3.0`より新しい最新の`v<X.Y.Z>`のtagの`migrations/*.sql`が名前も中身も変わらず残っていることを検査し、変更・改名・削除を名前つきでexit 1にする（tagの無いcloneでは検査しなかったことを出して通す）。CIと`release.yml`が実行する。
