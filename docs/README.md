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

- `adr/`: なぜその決定をしたか。重要な決定は必ず追加し、既存のADRを書き換えない。`accepted`のADRだけが現在の決定で、`superseded`なら`superseded_by`を辿り、`deprecated`は後継なしの廃止（日付は`deprecated_on`）。ADRのIDは書くtaskのIDと枝番（`adr-t<task ID>-<N>`）で、1 ADRに決定1つ、今の姿は`design/`が持つ。決定を変えるときは古いADRを丸ごと置き換え、決定の多い既存のADRは`amends`で一部を直す（[ADR-t598-1](adr/2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)、索引は[adr/README.md](adr/README.md)）。
- `design/`: 現在の実装がどうなっているか。コードを読む前のショートカットとして保守する。
- `development/`: このrepositoryの開発の今の規則（plannerのverify・paths・evidence・changeの選び方、workerのtestの範囲、testの制約、文書の規則など）。書き換えてよく、経緯はADRとplansに残す。AGENTS.md・plugin・reviewのsubagent定義は規則の本文を写さず、ここを参照する（[ADR-t1453-2](adr/2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)）。
- `plans/`: これから何を作るか。ステップの順序と完了条件を持つ。完了した計画は`status: completed`にして残す。

人の判断はADR、Goalの記述、`Task.context`、receiptの`summary`に残す。

## Frontmatter

全てのdocs文書は [frontmatter仕様](frontmatter.md) に従う。

## 更新ルール

アーキテクチャ全体に影響し、手戻りが大きい決定は先にADRを作る。実装を変更したら関連するdesign文書の`updated`と`last_verified`を確認する。計画は実際の依存関係と完了条件を更新する。
