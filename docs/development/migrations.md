---
id: development-migrations
type: development
title: このrepositoryのmigrationの規則（足し方・番号・リリース済みのmigrationの不変）
status: current
created: 2026-10-03
updated: 2026-10-07
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

## persistence.mdに書くこと

migrationを足すtaskは、[Persistence](../design/persistence.md)の、変えた表・列の今の姿を書く箇所だけを直す: 冒頭のschemaの木のその表の行（表の役割と、列についてコードから読めない約束）と、その表・列を説明する本文の節。列の一覧・既定値・null可の書き写しは足さず、意味が要れば定義のそばのdoc commentかmigrationのコメントに書く（[文書の規則](documents.md)の「design」、[ADR-t1942-1](../adr/2026-10-07-t1942-1-design-docs-in-four-layers-with-size-budgets.md)）。足した表は木に1行と、要れば本文の節を足す。

- 冒頭の段落にmigrationごとの経緯（どのmigrationが何を足したか、schemaの版）を足さず、木にmigrationごとの行（`-- 00NN: …`）を足さない。経緯は`migrations/*.sql`（先頭行の互換の宣言とコメント）とgitの履歴が持つ。
- 本文で根拠を名指すときは、migrationの番号（`integrate`が振り直すと古くなる）でなくADRかmigrationのファイル名の`<name>`の部分を名指す。既にある番号の言及は書き直さなくてよい。
