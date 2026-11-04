---
id: adr-t3990-5
type: adr
title: チームの版の配置する側が守ることの文書と参考の構成は、releaseと同じ版で読める配置する人向けの文書の種類としてこのrepositoryに置き、参考の構成はNATなしを既定にしてNATと固定IPの形も並べ、IPの許可リストを前提にせず、IaCを持たない
status: accepted
created: 2026-11-04
updated: 2026-11-04
accepted_on: 2026-11-04
owners:
  - hisamekms
tags:
  - architecture
  - documentation
  - security
related:
  - adr-t3981-1
  - adr-t3986-1
  - adr-t3987-1
  - adr-t1233-1
  - adr-t3102-3
  - adr-t3990-1
  - adr-t3990-2
  - adr-t3990-3
  - adr-t3990-4
  - plan-team-version-control-side
---

# ADR-t3990-5: チームの版の配置の文書と参考の構成の置き場

## Context

[ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)は、制御側をcontainerのtask 1つとPostgreSQLとS3互換の置き場にし（決定1）、配置は利用するpjが持ってdagqは部品を配る（決定2）と決め、部品の形とHTTPのAPIを後に残した。部品には配置する側が守ることの文書と参考の構成が含まれ、ADR-t3981-1決定1はNATなしを既定にし、NATと固定IPの形も書き、IPの許可リストを前提にしないと決めた。goalの制約は、dagqのrepositoryに特定の配置のIaCを持たないことである。この決定はその文書の置き場と中身の境を決める。

今の文書は、ADR・design・development・plansの4つの種類で、どれもこのrepositoryを開発する人とagent向けである。

具体の候補は設計資料の[チームの版の制御側の具体案](../plans/team-version-control-side.md)が持ち、実装のtaskが定義のそばのdoc commentへ移す。

## Decision

[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)（とくに決定2の互換をAPIの版で判定すること、決定3のユースケース単位のAPIとtransactionをクライアントとの往復で分けないこと）、[ADR-t3102-3](2026-10-22-t3102-3-stage-5-outage-notice-and-use-cases-for-multi-step-control-operations.md)（決定3〜5のユースケースの単位とleaseと前提の確かめ、副作用の順）、[ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)（とくに決定1・2）、ADR-t3985-1・ADR-t3986-1・ADR-t3987-1の決定は保つ。この決定はどれもamendsも置き換えもしない。ローカルの既定の配置は何も変えない。

1. **配置する側が守ることの文書と参考の構成は、IaCを持たない配る文書の種類としてこのrepositoryに置く。** `docs/design/`（このrepositoryの今の実装の地図）にもpluginのskill（repositoryでdagqを使うagentの手順）にも置かず、releaseと同じ版で読める、配置する人向けの文書の種類を足して置く（置き場の名前とfrontmatterの種類はplan）。中身は、守ること（TLSの終端・制御側のtaskを1つにし入れ替えは止めてから起動すること・secretの置き場・migrateを先に流すこと・DBのbackup・置き場の写しに期限を付けないこと（ADR-t3986-1決定8）・healthの監視・2つのGitHub Appとruleset（ADR-t3987-1決定6）・logを集めること）と、参考の構成である。
2. **参考の構成はNATなしを既定にし、NATと固定IPの形も並べ、IaCを持たない。** 参考の構成はNATなしを既定にし（制御側のtaskはpublicなsubnetでALBからだけ受け、外へはinternet gatewayで出る）、NATと固定IPを置く形も並べ、どちらもIPの許可リストを前提にしない（ADR-t3981-1決定1）。参考の構成は文と図と要る値の表で書き、TerraformやCloudFormationなどの特定の配置のIaCを持たない。

## Alternatives

- **参考の構成をIaCとして配る**: goalの制約（dagqのrepositoryに特定の配置のIaCを持たない）に反し、特定のクラウドに縛られる（決定2）。
- **pluginのskillか`docs/design/`に置く**: skillはrepositoryの中のagentの手順で配置の人に届かず、designは今の実装の地図でreleaseの版と揃わない。別の文書の種類にする（決定1）。
- **別のrepositoryに置く**: releaseの版とimageの版とずれる。同じrepositoryで同じtagにする（決定1）。

## Consequences

- 配置する人向けの文書の種類が1つ増え、文書の規則と検査（frontmatterの種類）を同じ変更で足す。
- backupと復元の手順の細目は配置する側の文書が書き、この決定は決めていない。
- 今の姿を書く`docs/design/`は、実装のtaskが着地と同じ変更で直す（この決定はまだ実装されていない）。
