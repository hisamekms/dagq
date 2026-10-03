---
id: docs-index
type: design
title: Documentation guide
status: current
created: 2026-09-21
updated: 2026-10-03
last_verified: 2026-10-03
tags:
  - documentation
---

# Documentation guide

dagqの文書は、決定、現在の設計、このrepositoryの開発の規則、実装計画と測定を分けて管理する。タスクの経過と状態はdagqのキュー（`dagq show ID`のrun履歴とreceipt）が持つ。repo rootの[AGENTS.md](../AGENTS.md)はこのrepositoryの開発の短い案内で、規則の本文は`development/`などの正本を指す（移し終えるまではAGENTS.mdが持つ規則もある）。

## 文書の種類

- `adr/`: なぜその決定をしたか。ADRの書き方・ID・置き換えとamendsの規則は[文書の規則](development/documents.md)の「ADR」「ADRのID」、欄と状態は[frontmatter仕様](frontmatter.md)、索引は[adr/README.md](adr/README.md)。
- `design/`: 現在の実装がどうなっているか。コードを読む前のショートカットとして保守する。
- `development/`: このrepositoryの開発の今の規則（plannerのverify・paths・evidence・changeの選び方、workerのtestの範囲、testの制約、文書の規則など）。書き換えてよく、経緯はADRとplansに残す。AGENTS.md・plugin・reviewのsubagent定義は規則の本文を写さず、ここを参照する（[ADR-t1453-2](adr/2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)）。
- `plans/`: これから何を作るか。ステップの順序と完了条件を持つ（更新と状態の規則は[文書の規則](development/documents.md)の「plans」）。

人の判断の残し方は[文書の規則](development/documents.md)の「判断の記録」。

## Frontmatter

frontmatterの欄は[frontmatter仕様](frontmatter.md)が持つ（従う規則は[文書の規則](development/documents.md)の「frontmatter」）。

## 更新ルール

ADRを実装より先に作る決定、designの内容と`updated` / `last_verified`の更新、計画の依存関係・完了条件・状態の更新の規則は[文書の規則](development/documents.md)の「ADR」「design」「plans」が持つ。
