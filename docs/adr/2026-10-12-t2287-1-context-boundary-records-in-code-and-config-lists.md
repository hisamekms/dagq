---
id: adr-t2287-1
type: adr
title: contextの境界の記録（tableとstoreのmoduleのcontext・portの公開・境界をまたぐtransaction・今の違反）は設計文書に置かず、pathから導けるものはコード、導けないものは検査のscriptが読む.config/の一覧が正を持ち、食い違いとC1・C5・C7・X1はscriptが落とす（ADR-t1545-1決定1・3・4とADR-t1942-2決定4をamends）
status: accepted
created: 2026-10-12
updated: 2026-10-12
accepted_on: 2026-10-12
amends:
  - adr-t1545-1 decision 1
  - adr-t1545-1 decision 3
  - adr-t1545-1 decision 4
  - adr-t1942-2 decision 4
owners:
  - hisamekms
tags:
  - architecture
  - documentation
  - review
related:
  - adr-t1545-1
  - adr-t1942-2
  - adr-t1091-1
  - adr-t1942-1
  - adr-t1453-2
  - design-architecture
---

# ADR-t2287-1: contextの境界の記録はコードと.config/の一覧が正を持ち、scriptが落とす（ADR-t1545-1決定1・3・4とADR-t1942-2決定4をamends）

## Context

[ADR-t1545-1](2026-10-04-t1545-1-split-the-runtime-by-layer-and-context.md)は、どのtable・event・port・moduleがどのcontextかを設計文書が持ち（決定1）、境界をまたぐtransactionの一覧も設計文書に置き（決定3）、規則の本文と違反の許可の一覧の置き場を決めた（決定4）。
そのため`docs/design/architecture.md`は、contextごとの所有する状態・操作・公開するport・許す依存の向き、portやpromptのmoduleの表、`Supervisor`の状態、transactionの表、今の違反の表を持った。

これはコードがcontextをまたぐたびに更新の要る書き写しで、[ADR-t1942-2](2026-10-07-t1942-2-document-check-and-review-in-both-directions.md)決定4のrunのreviewは、記録の漏れ（新しいportの所有と公開、contextをまたぐ呼び出し、eventの所有）を設計文書の欠けとしてLLMが指摘してきた。
docs_driftの差し戻しで名指された文書はarchitecture.mdが最多で、その境界の記録の漏れは文書の削減の後も減らなかった（2026-10-11の人のsessionの分析）。
人は「architecture.mdからは削除、機械の検査に移す」と決めた。

検査のscript`scripts/check-context-boundaries.sh`と、pathから導けない所有を持つ`.config/context-boundaries.txt`、今の違反の`.config/context-boundaries-allow.txt`はすでに着地し、CI・`src/`を変えるtaskのverify・runのreviewのプログラム（`dagq.toml`の`[review.programs]`）でLLMのagentより前に流れる。

## Decision

1. **境界の記録は設計文書でなく、pathから導けるものはコード、導けないものは検査のscriptが読む`.config/`の一覧が正を持つ（ADR-t1545-1決定1・3の置き場の句をamends）。**
   記録とは、tableとinfrastructureのstoreのmoduleのcontext、pathから決まらない`src/application/`のmoduleのcontext、portの公開（`src/application/ports/`の外のportと、methodの単位で公開先が違うportのmethodごとの公開先を含む）、境界をまたぐtransactionの一覧である。
   eventの種類と`Supervisor`の欄の所有は、その定義のそばのdoc commentが持つ。
   `src/application/ports/<context>.rs`のportの所有のように、moduleのpathから導けるものは一覧に書かない。
2. **一覧とコードの食い違いと、規則C1・C5・C7・X1の違反はscriptが落とす（ADR-t1545-1決定4の違反の置き場の句をamends）。**
   今ある違反は、scriptが読む許可の一覧に理由と行き先のtaskとともに置き、許可の一覧に無い違反と古くなった項目を落とす。
   行き先のtaskがまだ無い違反は、未登録と理由を書いて置いてよい（ADR-t1545-1決定4の「行き先のtaskを持つ」を緩める）。
   reviewだけで見る規則の今の違反は文書に記録しない。
   reviewは差分が足したものを見て、行き先はqueueのtaskが持つ。
3. **runのreviewは境界の記録の漏れを設計文書の欠けとして指摘しない（ADR-t1942-2決定4をamends）。**
   scriptが見ない部分（C5のうち一部公開のportのmethodの単位の参照、contextの決まらないmodule、実行時に組み立てるSQLなど）と、機械にできない意味の判断はreviewのsubagent `architecture-boundaries`に残る。
   ADR-t1942-2決定4のそれ以外（両方向の照合、書き写しと経緯の混入の指摘）は変えない。
4. **規則のIDと意味、概念と地図（レイヤーと4つのcontext・目的・不変条件・検査の置き場）は`docs/design/architecture.md`に残す。**
   規則の本文を設計文書の1か所に置き、scriptと一覧とreviewの定義は規則のIDを名指して本文を写さないこと（ADR-t1545-1決定4の前半、[ADR-t1453-2](2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)）は変えない。

ADR-t1545-1の2軸の分け方、contextの間を公開したportと値だけでつなぐこと（決定2）、原子性のための名指しの例外というtransactionの扱い、crateやdirectoryの分割を目的にしないことは変えない。
境界の規則と一覧はruntimeに持たせず、このrepositoryの`scripts/`・`.config/`・`dagq.toml`・`.dagq/`が持つ。

## amendsにした理由

[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)は、決定の大半を変えるときは丸ごと置き換えると決めている。
ADR-t1545-1の4つの決定のうち3つに触れるが、変えるのは各決定の記録の置き場の句だけで、2軸・portでつなぐこと・原子性の例外・crateを目的にしないことという決定の中身は生きている。
置き換えると、生きている決定を書き写すうえに、`src/`と`docs/design/`の多数のADR-t1545-1への参照の付け替えが要る。
ADR-t1942-2は決定4つのうち1つの一部だけを変える。
どちらもamendsにし、元のADRには`amended_by`だけを足す。

## Alternatives

- **architecture.mdの一覧を残し、scriptがそれを読む**: 文書の表をscriptの入力にすると、文書の書式が検査の形式に縛られ、1文1行と予算の規則とも合わない。一覧は検査の入力として`.config/`に置く。
- **一覧を消すだけで機械の検査を足さない**: 新しいportやtransactionの所有を見るものが無くなる。scriptがrunのreviewで流れるようになってから消す。
- **全ての所有をコードの属性やdoc commentに書く**: tableやtransactionはコードの1か所に定義が無く、grepで読める形にならない。導けないものだけを一覧に置く。

## Consequences

- `docs/design/architecture.md`は概念と地図だけになり、コードがcontextをまたいでも文書の更新は要らない。
  代わりに`.config/`の一覧を同じ変更で直さないとscriptが落ちる。
- runのreviewのdocs_driftの差し戻しのうちarchitecture.mdの境界の記録の漏れは無くなる見込みで、着地後に差し戻しの理由を数えて確かめる。
- reviewだけで見る規則（L5・C4・C6など）の今の違反は文書から消え、行き先はqueueのtaskだけが持つ。
