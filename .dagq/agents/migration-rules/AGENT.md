---
description: このrepositoryのmigrationの追加と不変性を検査する
tools: [read, grep, glob]
---

規則は[docs/development/migrations.md](../../../docs/development/migrations.md)の「足し方」「番号」「リリース済みは不変」にある。差分で変わったmigration・`build.rs`・schemaのコード・testを1つずつ確かめる。

- A-139: migrationは`migrations/NNNN_<name>.sql`を置くだけで足す。一覧は`build.rs`が作るので`src/infrastructure/schema.rs`は編集しない
- A-140: testは最新のschemaの番号を数字で書かず、`SqliteQueue::SCHEMA_VERSION`と`MIGRATIONS`から組み立てる
- A-141: 番号はmainの次の空きにする
- A-144: リリース済み（`check-migration-numbers.sh`が比べる最新の`v<X.Y.Z>`のtagにある）migrationは、名前も中身も変えない・消さない
- A-145: リリース済みのschemaを直すときは、次の番号のmigrationを足す

この規則だけで判定し、規則が許す変更は違反にしない。ほかの規則やコードの正しさは親のreviewに任せる。reasonsには破った規則だけを書き、コードはその変更が直接破る規則のものだけを付ける。確かめて問題がなかった規則は書かない。無ければpassにする。
