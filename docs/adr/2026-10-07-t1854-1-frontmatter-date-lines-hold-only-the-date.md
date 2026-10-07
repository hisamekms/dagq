---
id: adr-t1854-1
type: adr
title: 文書のfrontmatterのcreated・updated・last_verifiedはYYYY-MM-DDの日付だけを持ち、行末にどのtaskが何を変えたかのコメントを書かない。どのtaskが変えたかはgitの履歴（commitのDagq-Task trailer）が持つ
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
amended_by:
  - adr-t1964-1
owners:
  - hisamekms
tags:
  - documentation
  - conventions
related:
  - adr-t1428-1
  - adr-t598-1
  - development-documents
  - docs-frontmatter
---

# ADR-t1854-1: frontmatterの日付の行は日付だけを持ち、変えたtaskはgitの履歴に任せる

> **一部変更（2026-10-07）**: 決定1のうちADR以外の文書が`updated`・`last_verified`を持つ部分は[ADR-t1964-1](2026-10-07-t1964-1-non-adr-docs-drop-updated-and-last-verified.md)がamendsした（ADR以外の文書はこの2つの欄を持たない。ADRの`updated`とどのtypeの`created`は日付だけのまま）。

## Context

goal 147（finding 7のconflict_hotspot）。`docs/design/persistence.md`は10着地で3回衝突し（task 1437・1540・1640、task 1440のlanding recheck）、`git log -p`で見ると衝突のたびに変わっていたのは、frontmatterの`updated` / `last_verified`の行末の「`# task N; task M; …`」の列だった。finding 158の着地待ちのrunがmainの動くたびにresumeする原因も同じ行だった。

文書を変えるtaskは、[ADR-t1428-1](2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)と[文書の規則](../development/documents.md)の「design」に従い、本文を変えた文書の`updated` / `last_verified`を必ず書き換える。行末にtaskの番号や変えた内容を足していくと、同じ文書を変える2つのtaskは本文が重ならなくてもこの1行で必ずrebaseの衝突になる。2026-10-05の時点で、`docs/design`・`docs/development`・`docs/plans`の83文書が日付の行にコメントを持っていた。

## Decision

1. **`created`・`updated`・`last_verified`の値は`YYYY-MM-DD`の日付だけにし、行末にコメント（どのtaskが変えたか・何を変えたか）を書かない。** 日付だけなら、同じ日に同じ文書を変える2つの変更は同じ行になり衝突しない。日付を変える条件（`updated`は本文の最後の変更、日付だけの差分を作らない）は[frontmatter仕様](../frontmatter.md)と[ADR-t1428-1](2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)のままにする。
2. **どのtaskが文書を変えたかはgitの履歴が持つ。** 着地のcommitは`Dagq-Task` / `Dagq-Run` trailerでtaskとrunに結び付くので、`git log -- <path>`で辿る。変えた理由と内容はcommitのmessageとreceiptが持ち、文書には写さない。
3. **既存の文書の日付の行のコメントは、日付の値と本文を変えずにコメントだけを消す。** 日付の値を動かさないので、ADR-t1428-1の「日付だけの差分を作らない」には当たらない。ADRのfrontmatterの日付の行のコメントも同じく消す。[ADR-t598-1](2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)決定9（本文はappend-onlyで、後から変えてよいのは`status`・`accepted_on`・`superseded_by`・`superseded_on`・`deprecated_on`・`amended_by`と注記だけ）は決定と欄の値を守る規則で、行末のコメントは欄の値でも本文でもない。値と本文を変えずにコメントだけを消すのはその制限の外にあり、決定9を変えないのでamendsにしない。この扱いは[文書の規則](../development/documents.md)の「ADR」のappend-onlyの項にも書く。
4. **この形はCIのscriptの検査が守る**（goal 147の後続のtaskが足す）。

## Alternatives

- **コメントを残し、merge driverで日付の行を機械的に解く**: runtimeの`integrate`かgitの設定の変更が要り、この規則を使う他のrepositoryに広がらない。行を書き換え続ける形も残る。
- **taskの番号の列を別のファイルに移す**: 衝突する場所が別のファイルに移るだけで、読み手が日付と変更の経緯を探す場所が増える。gitの履歴が既に同じことを持つ。
- **日付の行をやめる**: `updated`・`last_verified`は読み手が文書の新しさを知る手がかりで、frontmatter仕様の必須の欄なので残す。

## Consequences

- 文書を変えるtaskは日付の値だけを書き換える。同じ日に同じ文書を変えるtaskどうしは日付の行で衝突しない。日付の違う変更どうしは衝突しても、新しい方の日付を取れば解ける。
- 文書のどの変更をどのtaskが入れたかを知るには`git log`か`git blame`を見る。
