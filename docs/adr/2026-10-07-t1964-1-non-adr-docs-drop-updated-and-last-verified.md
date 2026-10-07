---
id: adr-t1964-1
type: adr
title: ADR以外の文書（design・development・plan）のfrontmatterはupdatedとlast_verifiedを持たない。ADRのupdatedとどのtypeのcreatedは日付だけでコメントなしのまま（ADR-t1854-1決定1をamends）
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
amends:
  - adr-t1854-1 decision 1
owners:
  - hisamekms
tags:
  - documentation
  - conventions
related:
  - adr-t1854-1
  - adr-t1428-1
  - adr-t1091-1
  - development-documents
  - docs-frontmatter
---

# ADR-t1964-1: ADR以外の文書はupdatedとlast_verifiedを持たない（ADR-t1854-1決定1をamends）

## Context

[ADR-t1854-1](2026-10-07-t1854-1-frontmatter-date-lines-hold-only-the-date.md)決定1は、`created`・`updated`・`last_verified`の値を日付だけにし、行末にコメントを書かないと決めた。
文書を変えるtaskはこの行を書き換えるので、日付だけにしても、日の違う2つの変更は内容が重ならなくてもこの1行で衝突する。

request 46の実測（2026-09-29〜10-06）では、docsだけの衝突135件のうち72件（53%）がfrontmatterの日付の行だけの衝突だった。
記録のmainとheadを`git merge-file --diff3`で再現して分類した（定義は[docsの4指標](../plans/docs-slim.md)）。

ADR-t1854-1のAlternativesは「日付の行をやめる」を、読み手が文書の新しさを知る手がかりで、frontmatter仕様の必須の欄だからと退けた。
新しさはgitの履歴が持つ（ADR-t1854-1決定2）。
必須の欄はこの変更でfrontmatter仕様から外す。
2026-10-06に確かめた時点で、この欄を読むのはrun reviewのsubagent（design-consistency）の項だけで、runtimeは読まない。

ADRはappend-onlyで新しいファイルとして足され、状態の欄だけの変更では`updated`を動かさない（[文書の規則](../development/documents.md)の「ADR」）ので、この衝突の原因にならない。

ADR-0013の「この repository での適用」の進め方（実装を変えたtaskはdesignの`updated` / `last_verified`を更新する）は、番号付きの決定でなく、当時のレイヤー化のtaskの進め方なのでamendsしない。
[ADR-t1428-1](2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)は決定4で日付だけの差分を強いないと決めただけで、`updated`の更新は決めていないのでamendsしない。

## Decision

1. **`docs/`のADR以外の文書（typeが`design`・`development`・`plan`の全て。`docs/README.md`・`docs/frontmatter.md`・`docs/adr/README.md`も含む）のfrontmatterは`updated`と`last_verified`を持たない。**
   文書の新しさと変えたtaskはgitの履歴で辿る。
2. **ADR-t1854-1決定1の残る部分は変えない。**
   ADRの`updated`とどのtypeの`created`は、ADR-t1854-1決定1のとおり`YYYY-MM-DD`の日付だけを持ち、行末にコメントを書かない。
   ADR-t1854-1の決定2〜4は有効なまま。

## Alternatives

- **日付だけに揃える（ADR-t1854-1決定1のまま）**: 日の違う2つの変更はなお衝突する。
- **merge driverで日付の行を解く**: 着地前の検査（`git merge-tree`）で`.gitattributes`のdriverが効くか確かめていない。一般化の案は別にある。
- **ADRの`updated`も外す**: ADRは衝突の原因にならず、約195本のADRのfrontmatterを書き換えることになる。
- **`created`も外す**: 書き換わらないので衝突しない。

## Consequences

- ADR以外の文書を変えるtaskは日付の行を書き換えず、この行で衝突しない。
- 文書がいつ変わったかを知るには`git log -- <path>`を見る。
- ADR以外の文書に行が戻らないことの検査は後続のtaskが足す。
