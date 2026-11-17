---
id: adr-t5155-1
type: adr
title: Goalのdomainが受け入れ条件の本文と版を一緒に変え、版のtriggerを非互換のmigrationで除く
status: accepted
created: 2026-11-17
updated: 2026-11-17
accepted_on: 2026-11-17
amends:
  - adr-t3986-1 decision 6
owners:
  - hisamekms
tags:
  - domain
  - persistence
  - migrations
related:
  - adr-t5151-1
  - adr-t1504-1
  - adr-t1504-2
  - design-follow-up-membership
  - development-migrations
---

# ADR-t5155-1: Goalのdomainが受け入れ条件の本文と版を一緒に変える

## Context

受け入れ条件の版は、follow-upの所属の判断が今の本文に対するものか、再計画が今も有効かを決める業務の状態である。
[ADR-t1504-1決定7](2026-10-04-t1504-1-follow-ups-belong-to-the-goal-whose-acceptance-needs-them.md)は、本文が変われば判断を確かめ直すまでgoalを閉じないことを決めている。
しかし、Goalのdomainは本文だけを変え、版を進める規則はSQLiteのtriggerとPostgresの検索の書き込みに分かれている。
[ADR-t5151-1](2026-11-17-t5151-1-domain-commands-facts-and-layer-responsibilities.md)の、新しい集約と記録するeventをdomainのコマンドが返す形に揃える。

[ADR-t3986-1決定6](2026-11-03-t3986-1-write-leader-fencing-trigger-placement-postgres-migration-series-archive-local-import-and-store-tests.md)はSQLiteのtriggerを残し、Postgresではadapterが版を進めると決めている。
そのAlternativesは、互換のmigrationの後に前のbuildへ戻すと、triggerを頼る書き込みが壊れるためSQLiteのtriggerをアプリへ移す案を退けた。
版のtriggerを消したqueueでは、その書き込みは本文だけを変え、古い所属の判断が同じ版のままcurrentに見え、確かめ直さずgoalを閉じうる。
この危険を互換性の宣言と下限で防ぐ必要がある。

## Decision

1. **本文と版を一緒に変える規則はGoalのdomainのコマンドが持つ。** 本文が変わったときだけ現在の版を1進め、同じ本文や本文以外の編集では版を保つ。
   コマンドはGoalの所有権と入力・必要な事実を受け、新しいGoalと記録するeventの列を返す（ADR-t5151-1決定1・2・5）。
   本文と版を非公開の状態として持ち、検証付きの入口と保存済みの値を保つrestoreを分け、呼び出し元が本文と版を別々に変える公開境界にしない。
2. **SQLiteの版のtriggerとPostgresの検索の書き込みによる版の更新をなくす。** SQLiteは新しいmigrationで版のtriggerを消し、同じ名前のPostgresのmigrationは変更不要と宣言する1本にする（[migrationの規則](../development/migrations.md)）。
   Postgresの検索の書き込みにある版の比較と加算も除き、両方のstoreは同じtransactionで読んだGoalをdomainへ渡し、返された本文・版・eventを同じtransactionで書く。
   版のtriggerは残さない。
   ADR-t3986-1決定6のうち受け入れ条件の版の部分だけをamendsし、SQLiteの検索の索引のtriggerを残すことを含む他の部分と、他の決定は保つ。
3. **triggerを除くmigrationは非互換と宣言し、その版まで下限を上げる。** SQLiteのmigrationの先頭行で非互換と宣言し、両方のstoreに共通の互換の宣言と下限により、この変更より前のbuildが移行後のqueueを開くことを拒む。
   `up`と`install`が非互換のmigrationをdrain後に適用する今の仕組みに乗り、互換のmigrationとして前のbuildを動かしたまま適用しない。
   得失を比べる対象は次の2案とし、(i)を採る。
   - **(i) 非互換と宣言して下限を上げる。** 前のbuildへのそのままのrollbackと稼働を続けた更新はできず、drainが要る代わりに、版を進めない書き込みを拒み、所属の再確認と再計画の有効性の判断を保てる。
     rollbackが要る場合も下限を下げて前のbuildを開かせず、このschemaとdomainの規則を扱える修正版を用意する。
   - **(ii) 互換と宣言し、前のbuildの書き込みが版を進めない期間を受け入れる。** 前のbuildへのrollbackを許せる代わりに、SQLiteでは本文を何度変えても所属の判断や再計画が古い版のまま有効に見える。
     誤った達成や再計画の適用まで起こりうるため影響は表示だけに限らない。
     新しいbuildへ戻しても次の編集だけでは取りこぼした変更を回復できず、履歴からの検出と関係する判断の再確認などの補いが要る。
     全行の版を一律に進める補いは決定4に反し、履歴からの補いも別の規則と検証が要るため、この案は採らない。
4. **既存の行の版を変えない。** migrationはtriggerを除くのみで、本文・版・判断の行を更新せず、storeの復元も保存された版をそのまま使う。
   CLIの出力・exit code・JSONの欄名、eventのkind・payload・順、理由のコードとschemaの意味、follow-upの所属の判断の結果を変えない。
   状態とeventの原子性、並行の変更を見つけるtransactionの述語も保つ。

## Alternatives

- **版のtriggerを残し、domainも同じ版を書く。** 前のbuildの書き込みを救えるが、規則の二重の実装が残り、版のtriggerとPostgres側の加算をなくす目的と決定2を満たさないため採らない。
- **両方のstoreが本文を比較して版を進める。** transactionの原子性は守れるが、同じ業務の規則をadapterごとに持ち、domainのunit testだけで確かめられないため採らない。
- **互換のmigrationとして版を進めないrollbackを許す。** 決定3(ii)の利点より、古い判断がcurrentに見える危険と補いの負担が大きいので採らない。

## Consequences

規則をGoalのunit testで確かめ、同じ編集で同じ版になること、状態とeventの原子性を両方のstoreの共通のtestで確かめる。
移行の境界では既存の版が変わらないことと、下限に満たないbuildの拒否を確かめる。
このADRはコードとmigrationを変えず、実装を適用するときに最終のdesignの説明も揃える。
designは今の実装を説明するので、この変更では版を進める現在の2つの経路だけを補い、[follow-upの版の説明](../design/follow-up-membership.md#acceptanceの版と読み取り)からこの決定を参照する。
