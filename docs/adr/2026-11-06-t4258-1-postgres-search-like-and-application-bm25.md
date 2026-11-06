---
id: adr-t4258-1
type: adr
title: Postgresの検索はLIKEで絞り込みアプリのbm25で順位を揃える
status: accepted
created: 2026-11-06
updated: 2026-11-06
accepted_on: 2026-11-06
owners:
  - hisamekms
tags:
  - architecture
  - search
related:
  - adr-t3986-1
  - adr-0063
  - adr-t3982-1
  - plan-team-version-search-spike
---

# ADR-t4258-1: Postgresの検索はLIKEで絞り込みアプリのbm25で順位を揃える

## Context

[Spike](../plans/team-version-search-spike.md)では、pg_trgmとpg_bigmの絞り込みは42件全部でFTS5と一致したが、順位の関数は基準を満たさなかった。
長い語を含む検索の上位10件の共通の割合の平均は0.458・0.485、FTS5の上位10件のbが落ちる組は2組・18組、relatedの上位集合の共通の割合の平均は0.485・0.046だった。
LIKEの結果にアプリでbm25を計算したA3は、全部の問い合わせと組で順位まで一致した。
チームの版は特定のクラウドに縛らない。
pg_bigmはRDSで利用できるが、[Microsoft Q&Aの2026年3月の回答](https://learn.microsoft.com/en-us/answers/questions/5825157/when-can-azure-for-postgresql-support-pg-bigm-exte)ではAzure Database for PostgreSQLでの提供時期は未定なので、必須にはできない。

## Decision

Postgresのstoreの検索は、索引の表の`lower(列) LIKE`で絞り込み、アプリの中の純粋なbm25でFTS5と同じ順位を計算する。
`word_similarity`・`bigm_similarity`は順位に使わない。
GINの索引はpg_bigmが使えるDBでは`gin_bigm_ops`、使えなければpg_trgmの`gin_trgm_ops`で張り、問い合わせの文は同じにする。
どちらでもLIKEの結果は同じで、違うのは短い語の速さだけである（Spikeの[実行計画](../plans/team-version-search-spike/explain.txt)）。
順位はFTS5と同じ列の重み・文書の長さ・全行の統計と、語の出現の数え方に従い、同点の並びを保つため浮動小数の計算順も揃える。
問い合わせの演算子の置き方は木に解釈してFTS5と同じものを拒むが、ローカルのSQLiteは今のFTS5の判定を保つ。
検索はstoreと別のportに保つ。
外の検索serviceは今は採らない。

これはADR-t3986-1の決定6と「決めていないもの」がSpikeに委ねた検索の方式を埋める決定であり、amendsも置き換えもしない。
ADR-0063決定1〜3はローカルのSQLiteの決定として保ち、ADR-t3982-1決定3の検索のportの境界も保つ。

## Alternatives

- pg_trgm・pg_bigmの順位の関数: SpikeでFTS5の順位に届かず、relatedの重複の判定にも影響する。
- pg_bigmを必須にする: 配置できるDBを狭め、特定のクラウドに縛らない制約に反する。
- 外の検索service: 統計の計算量が足りないときの候補だが、A3が順位まで一致しており、今は部品を増やさない。

## Consequences

LC_CTYPEがCのDBでも、絞り込みと順位は語の分け方に依らない（語の分け方に依る順位の関数を使わない）。
ただし`lower()`はDBのLC_CTYPEに従う。
[PostgreSQLのLocale Support](https://www.postgresql.org/docs/18/locale.html)はupper・lowerがlocaleの影響を受け、Cがlocaleの支えの無い動きになるとし、[Collation Support](https://www.postgresql.org/docs/18/collation.html)はbuiltinのC.UTF-8がUnicodeの単純な大文字と小文字の写しを使うとする。
この文書から、LC_CTYPEがCのDBでは非ASCIIの大文字と小文字が畳まれず、Éとéが別の語になり、非ASCIIの絞り込みの結果が変わると判断する。
日本語は大文字と小文字を持たないので、日本語の絞り込みは変わらない。
この限界は文書による判断で、実行では確かめていない。
配置する人にはLC_CTYPEがCでないDBを勧める。
統計の持ち方と候補が多い検索の計算量はadapterの実装で扱う。
