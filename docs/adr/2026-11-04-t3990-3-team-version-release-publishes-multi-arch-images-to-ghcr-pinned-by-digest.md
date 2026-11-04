---
id: adr-t3990-3
type: adr
title: チームの版の制御側のimageとrunのbaseはdagqのreleaseがarm64とamd64の1つのmanifestでGHCRに出し、tagは版だけにして配置の側がdigestで固定し、版の互換はAPIの版とschemaの版で判定する
status: accepted
created: 2026-11-04
updated: 2026-11-04
accepted_on: 2026-11-04
owners:
  - hisamekms
tags:
  - architecture
  - runtime
  - release
related:
  - adr-t3981-1
  - adr-t3987-1
  - adr-t2257-1
  - adr-t1233-1
  - adr-t3102-3
  - adr-t3990-1
  - adr-t3990-2
  - adr-t3990-4
  - adr-t3990-5
  - plan-team-version-control-side
---

# ADR-t3990-3: チームの版のimageの出し方と版の互換

## Context

[ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)は、制御側をcontainerのtask 1つとPostgreSQLとS3互換の置き場にし（決定1）、配置は利用するpjが持ってdagqは部品を配る（決定2）と決め、部品の形とHTTPのAPIを後に残した。部品にはarm64とamd64のimageが含まれる。[ADR-t3987-1](2026-11-03-t3987-1-team-version-runners-pull-under-fenced-run-leases-ai-jobs-on-runners-pinned-images-two-github-apps-and-attributed-claude-credentials.md)は、dagqが配るrunのbaseをreleaseが公開のOCIのregistryに出し、制御側がdigestで固定すること（決定4）と、制御側がagentのCLIとgitを持たないこと（決定3）を決めた。この決定は、どこへどう出すかと版の互換を決める。

具体の候補は設計資料の[チームの版の制御側の具体案](../plans/team-version-control-side.md)が持ち、実装のtaskが定義のそばのdoc commentへ移す。

## Decision

[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)（とくに決定2の互換をAPIの版で判定すること、決定3のユースケース単位のAPIとtransactionをクライアントとの往復で分けないこと）、[ADR-t3102-3](2026-10-22-t3102-3-stage-5-outage-notice-and-use-cases-for-multi-step-control-operations.md)（決定3〜5のユースケースの単位とleaseと前提の確かめ、副作用の順）、[ADR-t3981-1](2026-11-02-t3981-1-team-version-control-side-on-containers-postgres-and-object-store-with-app-side-write-serialization.md)（とくに決定1・2）、ADR-t3985-1・ADR-t3986-1・ADR-t3987-1の決定は保つ。この決定はどれもamendsも置き換えもしない。ローカルの既定（固定バイナリ、制御側がmainのcheckoutからimageをbuildする形）は何も変えない。

1. **imageはdagqのreleaseがarm64とamd64でGHCRに出し、版のtagとdigestで固定させる。** 配るimageは、制御側のimage（dagqのbinaryだけで、agentのCLIもgitも持たない。ADR-t3987-1決定3）と、runのbase（ADR-t3987-1決定4）の2つで、どちらもdagqのrelease（binaryとcrates.ioと同じtagのcommit）の同じworkflowがbuildし、arm64とamd64をまとめた1つのmanifestとして、dagqのGitHubのrepositoryに付くcontainer registry（GHCR）に出す。tagはreleaseの版だけで、動くtag（最新を指すもの）を置かず、releaseの記録にdigestを書き、buildの出どころの証明を添える。配置の側はdigestで固定し、mainの途中のcommitからimageを出さない。
2. **版の互換はAPIの版とschemaの版で判定し、releaseの版の数では判定しない。** 制御側のimageの版とrunnerとCLIの版の間はAPIの版の範囲（ADR-t3990-1決定3）、DBとの間はschemaの版と下限（ADR-t3990-2決定4）で判定する。

## Alternatives

- **binaryだけを配り、imageは配置の側がbuildする**: 配置ごとに中身が変わり、digestでの照合と出どころの証明ができない（決定1）。
- **mainのcommitごとにimageを出す、か最新を指すtagを置く**: releaseしていない中身が配置に入り、何が動いているか分からなくなる。releaseの版だけにする（決定1）。
- **Docker Hubなど別のregistryに出す**: releaseと別の資格と出どころになり、pullの回数の上限を受ける。releaseと同じGitHubに置く（決定1）。
- **releaseの版の一致で互換を判定する**: releaseのたびに全てのrunnerとCLIを同時に替えることになる。APIの版を別に持つ（決定2、ADR-t3990-1決定3）。

## Consequences

- releaseのworkflowはimageのbuildとmulti-archの公開と出どころの証明を持つことになり、releaseの時間が延びる。
- 今の姿を書く`docs/design/`は、実装のtaskが着地と同じ変更で直す（この決定はまだ実装されていない）。
