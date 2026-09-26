---
id: adr-t614-2
type: adr
title: リリース済み（最新のv*のtagに含まれる）のmigrationは中身も名前も変えず消さず、それを検査のscriptとCIとreleaseで止める
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
owners:
  - hisamekms
tags:
  - runtime
  - persistence
  - release
related:
  - adr-0067
  - adr-0073
  - adr-t614-1
  - design-persistence
---

# ADR-t614-2: リリース済み（最新のv*のtagに含まれる）のmigrationは中身も名前も変えず消さず、それを検査のscriptとCIとreleaseで止める

## Context

queueのschemaは、dagqのバイナリに組み込まれた`migrations/*.sql`を番号順に適用し、DBには適用した番号（`user_version`）だけを残す。適用済みのmigrationの中身は二度と読まれない。

これまでdagqの利用者はこのrepositoryだけで、固定バイナリはmainから作られてきた。migrationを後から直しても、影響するのはこのqueueだけで、運用で合わせられた。実際、v0.2.0の後に0001〜0010の先頭に互換の宣言行（[ADR-0073](0073-kind-additions-are-compatible.md)）を足している。

goal 52で、外部のprojectは`cargo install dagq`でリリースを導入する。リリースの後にそのリリースのmigrationの中身を変えると、同じ番号を適用済みのDBと、新しい中身で作ったDBとでschemaが食い違い、どちらのDBかは`user_version`からは見分けられない。名前を変える・消すと、番号の欠けや重なりになるか、適用済みのDBのschemaの意味が変わる。

[ADR-0067](0067-migrations-are-listed-by-build-and-renumbered-on-landing.md)決定3の`integrate`の振り直しは、runが足した未着地のmigrationだけを動かすので、リリース済みの番号はもともと変わらない。しかし、runや人がリリース済みのmigrationの中身を書き換える変更を止める検査は無い。人は2026-09-27に「リリースの後はリリース済みのものを変えない仕組みが要る」と整理した。

## Decision

1. **リリース済みのmigrationは不変にする。** 最新の`v*`のtagに含まれる`migrations/*.sql`は、中身をbyte単位で変えず、名前を変えず、消さない。コメントや互換の宣言行も例外にしない。schemaを直したいときは、次の番号の新しいmigrationを足す。
2. **検査はmigrationの検査のscriptとCIとreleaseで行う。** migrationの番号を検査するscriptが、最新の`v*`のtagと作業treeを比べ、リリース済みのmigrationの変更・改名・削除を見つけたらファイルを名指してexit 1にする。CIはこのscriptを実行する。releaseは新しいtagを出す前に、前の`v*`のtagと比べて同じ検査を行い、通らなければ出さない。migrationを足すtaskのverificationにこのscriptを含めれば、`integrate`がrebase後に走らせるので、mainに着地する前にも止まる（含めないtaskではCIとreleaseが止める）。
3. **規則はこのADRの後の最初のリリースから効く。** v0.2.0とその後のmainの差（0001〜0010の宣言行）はそのまま受け入れ、検査の基準はこのADRの後に出る最初の`v*`のtagから始める。それより前のtagを基準にしない。
4. **dagqのソースのrepositoryだけの規則である。** 対象はdagqに組み込まれたqueueのschemaのmigrationで、他のrepositoryの`migrations/`には触れない（[ADR-t614-1](2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)）。

この規則はADR-0067決定3の振り直しの規則を変えない（振り直しはリリース済みの番号を動かさない）ので、他のADRをamendsしない。検査の実装（scriptの比べ方、tagの探し方、CIとrelease.ymlの段）は別のtaskで行い、[Persistence](../design/persistence.md)に書く。

## Alternatives

- **DBにmigrationの中身のhashを記録して、openで食い違いを検出する**: 食い違いは見つかるが、すでに配ったDBは直せない。変更そのものを出す前に止める方が安い。schemaの変更も要る。
- **リリース済みのmigrationの修正を互換の宣言やコメントに限って許す**: 宣言は互換の判定（ADR-0073）に効き、例外の線引きを検査で持つと複雑になる。例外を置かない。
- **検査をCIだけで行う**: releaseはCIの結果を待たずにtagから走りうる。releaseでも比べることで、出す直前に止まる。
- **規則をv0.2.0から効かせる**: 0001〜0010の宣言行がすでに違うので、検査が最初から落ちる。戻すと互換の判定が変わる。

## Consequences

- リリース済みのmigrationを直す変更は、CI・`integrate`・releaseで止まり、新しいmigrationを足す形に直すことになる。
- 検査はgitのtagを読むので、tagの無いcheckout（shallow clone、tagをfetchしないCI）では基準が見つからない。その扱い（tagのfetch、基準が無いときの扱い）は実装のtaskで決めてdesignに書く。
- 未リリースのmigrationは、次のリリースまで今までどおり直してよい。
