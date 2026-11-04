---
id: adr-t3990-4
type: adr
title: チームの版の制御側のlogは構造化して標準出力に出し、dagqに固有の指標はOTLPで出し、KPI・stats・eventsは今のSSOTとコマンドを制御側のAPIで読み、sessionのlogは署名つきの許しで置き場に置き、見直しはrunnerのjobとして動かして結論を当番に届ける
status: accepted
created: 2026-11-04
updated: 2026-11-04
accepted_on: 2026-11-04
owners:
  - hisamekms
tags:
  - architecture
  - runtime
  - measurement
related:
  - adr-t3981-1
  - adr-t3986-1
  - adr-t3987-1
  - adr-t3989-1
  - adr-t1662-2
  - adr-0033
  - adr-t1233-1
  - adr-t3102-3
  - adr-t3990-1
  - adr-t3990-2
  - adr-t3990-3
  - adr-t3990-5
  - design-measurement
  - plan-team-version-control-side
---

# ADR-t3990-4: チームの版の制御側の観測（log・指標・KPIとstats・sessionのlog・見直し）

## Context

[ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)は、制御側をcontainerのtask 1つとPostgreSQLとS3互換の置き場にし（決定1）、配置は利用するpjが持ってdagqは部品を配る（決定2）と決め、部品の形とHTTPのAPIを後に残した。[ADR-t3987-1](2026-11-03-t3987-1-team-version-runners-pull-under-fenced-run-leases-ai-jobs-on-runners-pinned-images-two-github-apps-and-attributed-claude-credentials.md)は、AIのjobを全てrunnerで動かし（決定3）、制御側のcontainerの指標を観測の決定に残した（決定7）。[ADR-t3986-1](2026-11-03-t3986-1-write-leader-fencing-trigger-placement-postgres-migration-series-archive-local-import-and-store-tests.md)は古いeventと台帳を置き場へ移す（決定8）。結論の宛ての当番は[ADR-t3989-1](2026-11-03-t3989-1-team-version-goal-owner-ask-routing-to-owner-delegate-and-on-call-admin-only-high-priority-and-shared-store-for-roadmap.md)が決めた。

今のlogはqueueのディレクトリのファイル、KPIとstatsはhostのCLIが読むSSOTとビュー、毎時・日次・週次の見直しはsupervisorが起動するjobで、結論はinboxに届く。

具体の候補は設計資料の[チームの版の制御側の具体案](../plans/team-version-control-side.md)が持ち、実装のtaskが定義のそばのdoc commentへ移す。

## Decision

[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)（とくに決定2の互換をAPIの版で判定すること、決定3のユースケース単位のAPIとtransactionをクライアントとの往復で分けないこと）、[ADR-t3102-3](2026-10-22-t3102-3-stage-5-outage-notice-and-use-cases-for-multi-step-control-operations.md)（決定3〜5のユースケースの単位とleaseと前提の確かめ、副作用の順）、[ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)（とくに決定1・2）、ADR-t3985-1・ADR-t3986-1・ADR-t3987-1の決定は保つ。この決定はどれもamendsも置き換えもしない。ローカルの既定の配置（queueのディレクトリのlog、inboxへの結論）は何も変えない。

1. **logは構造化して標準出力に出し、dagqに固有の指標はOTLPで出す。** 制御側のlogは1行1つの構造化の形で標準出力と標準エラーに出し、集めて保つのは配置の側の役目にする（チームの版ではqueueのディレクトリのlogのファイルを持たない）。tokenとsecretの値はlogに出さない。tracingの外への送り出しは[ADR-0033](0033-one-tracing-pipeline-with-local-json-lines-and-optional-otlp.md)の任意のOTLPの経路に乗せ、宛ては配置の設定に置く。containerの資源の指標（CPU・memory）は配置の側の基盤から取り、dagqは測らない。dagqに固有の制御側の指標（書き込みの列の長さと待ち・係の世代と交代・交代中とDBに届かない断りの数・runnerの数）は、同じOTLPの経路のmetricと構造化のlogに出し、取りに来させる指標の入口は持たない。
2. **KPI・stats・eventsは今のSSOTとコマンドを制御側のAPIで読む。** KPI・stats・eventsの正本は今どおりstoreのSSOTで（[ADR-t1662-2](2026-10-04-t1662-2-measurement-stores-ssot-and-views.md)）、人は今と同じコマンドを自分のtokenで読み取りのユースケースとして打ち、制御側は表示の画面を持たない。置き場へ移した古い区切りは置き場の写しを外の集計で読む（ADR-t3986-1決定8）。
3. **sessionのlogは一つの物に限った署名つきの許しで置き場に置く。** runとjobのsessionのlogはrunnerのマシンで生まれ、runnerは制御側がleaseに限って発行するS3互換の一つの物への書き込みの許し（S3互換のAPIの署名つきの要求）で置き場に置き、置き場の資格をrunnerに渡さない。`run log`などは制御側が置き場から読んで答える。
4. **見直しはrunnerのjobとして動かし、制御側の不在の知らせは配置する側が持つ。** 毎時・日次・週次の見直しは、supervisorのループの時計が始めるAIのjobとしてrunnerで動き（ADR-t3987-1決定3）、時間帯は配置の設定で決め、結論は当番に届ける（ADR-t3989-1）。制御側そのものが落ちた知らせはdagqの中から出せないので、healthの監視と通知は配置する側が持つ（ADR-t3990-5決定1の文書に書く）。

## Alternatives

- **dagqが表示の画面や指標の入口を持つ**: 配置の側の監視と二重になり、読み取りの認可の経路が増える。今のコマンドと標準出力のlogにする（決定1・2）。
- **runnerに置き場の資格を渡す、か全てのlogを制御側のAPIで運ぶ**: 前者は他人の物を書けるrunnerを作り、後者は制御側の帯域を大きなlogで埋める。一つの物に限った署名つきの許しにする（決定3）。

## Consequences

- 制御側は表示の画面を持たないので、KPIの画面が欲しい配置は置き場の写しや外の集計から自分で作る。
- 今の姿を書く`docs/design/`は、実装のtaskが着地と同じ変更で直す（この決定はまだ実装されていない）。
