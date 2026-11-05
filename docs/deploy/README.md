---
id: deploy-index
type: deploy
title: チームの版の制御側を配置する人向けの文書の案内
status: current
created: 2026-11-05
owners:
  - hisamekms
tags:
  - documentation
related:
  - adr-t3990-5
  - adr-t3981-1
  - development-documents
---

# チームの版の制御側を配置する人向けの文書

チームの版の制御側（containerのtask 1つ・PostgreSQL・S3互換の置き場）を自分のクラウドに配置する人が読む文書の入口。
dagqは部品を配り、配置そのものは利用するpjが持つ（[ADR-t3981-1](../adr/2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)）。

ローカルのSQLiteと1台のhostで動かすだけなら、この文書は要らない。

## 読む版

配置するreleaseと同じtagのこのdirectoryを読む。
文書と部品は同じrepositoryの同じtagで揃う（[ADR-t3990-5](../adr/2026-11-04-t3990-5-team-version-deploy-documents-and-reference-configurations-without-iac.md)決定1）。

## ここに置く文書

| 文書 | 持つもの |
| --- | --- |
| この文書 | 目的と文書の一覧。部品の一覧と版の互換の表もここに足す |
| `requirements.md` | 配置する側が守ること（ADR-t3990-5決定1が挙げるもの）とbackupと復元・DBの監視 |
| `github-apps.md` | 2つのGitHub Appの権限とruleset |
| `reference-aws.md` | 参考の構成（NATなしを既定に、NATと固定IPの形も並べる） |

この文書のほかの行のうち、リンクの無い文書はまだ置かれていない。

## ここに置かないもの

- 特定の配置のIaC（TerraformやCloudFormationなど）。参考の構成は文と図と要る値の表で書く。
- dagqの今の実装の説明。それは[design](../design/README.md)が持つ。
