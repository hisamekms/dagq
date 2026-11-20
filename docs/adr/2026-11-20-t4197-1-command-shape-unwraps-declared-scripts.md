---
id: adr-t4197-1
type: adr
title: 計測のコマンドの形は宣言したwrapperが流すコマンドを持つ
status: accepted
created: 2026-11-20
updated: 2026-11-20
accepted_on: 2026-11-20
amends:
  - adr-t1673-1 decision 5
owners:
  - hisamekms
tags:
  - runtime
  - observability
related:
  - adr-t1673-1
  - adr-t2257-1
  - adr-t3201-1
  - design-measurement-commands
---

# ADR-t4197-1: 計測のコマンドの形は宣言したwrapperが流すコマンドを持つ

## Context

計測ではshellが走らせるscriptとその後の語がdigestになる。コマンドをそのまま流すwrapperを通すと、直接実行したときの部分の形が失われ、同じ規則で作業の内訳を数えられない。

## Decision

1. `dagq.toml`で宣言したwrapperで中のコマンドがあるときは、[ADR-t1673-1](2026-10-21-t1673-1-command-shape-keeps-only-known-subcommands-in-plain.md)決定5の例外として、そのコマンドの部分として形にする。展開した形にwrapperのpathとflagの値は残さない。宣言の無いscriptと、中のコマンドが無い呼び方には決定5をそのまま当てる。

## Alternatives

- **scriptの引数から自動で推測する**。`sh x.sh git`のような呼び方で、scriptが受け取るデータをコマンドと誤読する。
- **runtimeがwrapperの名前を持つ**。repository固有の値は設定から受けるという[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定1に反する。

## Consequences

- 同じコマンドに対して、直接実行したときとwrapperを通したときで同じ形と分類が得られる。保存済みのstepの形は作り直さない。
- ADR-t1673-1決定1・3と[ADR-t3201-1](2026-10-24-t3201-1-command-shape-keeps-only-listed-flags-and-variables-in-plain.md)決定1を保つ。wrapperの中でも、一覧に無いプログラムの語はdigestで持つ。
- 今の姿は[計測のコマンドの形と分類](../design/measurement-commands.md)と`domain::command_shape`のmodule docが持つ。
