---
id: adr-t876-1
type: adr
title: スキーマが安定するまでSQLiteのCHECK制約を使わず、不変条件はdomainの型とapplication・書き込みのportで守り、壊れた値を読んだらkindの列だけ寛容に、それ以外はfail closedにする（ADR-0073決定6・8・19・22をamends）
status: accepted
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
amends:
  - adr-0073 decision 6
  - adr-0073 decision 8
  - adr-0073 decision 19
  - adr-0073 decision 22
owners:
  - hisamekms
tags:
  - runtime
  - persistence
related:
  - adr-0073
  - adr-t614-2
  - adr-t598-1
  - design-persistence
  - design-domain-model
---

# ADR-t876-1: スキーマが安定するまでSQLiteのCHECK制約を使わず、不変条件はdomainの型とapplication・書き込みのportで守り、壊れた値を読んだらkindの列だけ寛容に、それ以外はfail closedにする（ADR-0073決定6・8・19・22をamends）

## Context

[ADR-0073](0073-kind-additions-are-compatible.md)の決定19〜23は、askとeventの`kind`を名指すCHECKだけを外し、規則を書き込みのportに移した。kindを名指さないCHECKは決定19・22でDBに残した。2026-09-28の時点で、空のqueueのschemaには約60のCHECKが20前後の表にある。種類は、`status`・`role`・`origin`・`target`・`verdict`などの値の一覧、`json_valid`と`json_type`、NULLの組の整合（`(answer IS NULL) = (answered_at IS NULL)`など）、数値の範囲、空文字の禁止、singletonなどである。

SQLiteはCHECKを変えられないので、CHECKのある列に値を1つ足すたびに表を作り直す必要がある。作り直しはADR-0073決定6で非互換（`-- dagq-schema: breaking`）になり、下限を上げ、本番の入れ替えはdrainに落ちる。ADR-0073の背景にあったこの問題は、kind以外のCHECKでも続いている（例: taskごとのworkerのproviderの列）。schemaはまだ安定しておらず、値の追加と列の意味の調整が続く見込みである。一方で、DBに書くのはdagqのRustの書き込み口だけで、同じ規則はdomainの型（値の一覧の列挙など）とapplicationの検査がすでに多くを持っている。

人は2026-09-28に、schemaが安定するまでSQLiteのCHECKを使わず、不変条件はdomainと書き込みのportだけで守ると決めた。

## Decision

1. **スキーマが安定するまでCHECKを置かない。** 新しいmigrationにCHECKを書かず、既存の表のCHECKもすべて外す。NOT NULL・UNIQUE（部分UNIQUE indexを含む）・主キー・外部キー・DEFAULTは対象外で、今までどおりDBが守る。
2. **不変条件はdomainの型とapplication・書き込みのportで守る。** CHECKが持っていた規則（値の一覧、jsonの形、NULLの組の整合、範囲、空文字の禁止、singletonなど）は、domainの型が表せないものを作れないようにし、表せないものはapplicationか書き込みのport（queueのportとその実装）が書く前に検査して、破れていれば書かずにerrorにする。規則ごとに違反を拒むtestを置く。CHECKを外すのは、同じ規則がdomainかportにあることをtestで固定した後にする（外してから足す順にしない）。ADR-0073決定22の「kindを名指さないCHECKはDBに残す」はこれで変わり、kindに結び付いた規則と同じくportが守る。
3. **壊れた値を読んだときの扱いは、kindの列だけ寛容、それ以外はfail closedにする。** ADR-0073決定21のとおり、askとeventの`kind`（と今後の同種の列、repositoryが名付けるlabelの列）は知らない値を知らない値として読み、errorにしない。それ以外の列（`status`などの状態や人の判断の分類、jsonの形、NULLの組、範囲など）に規則の外の値があれば、その行の読み込みは推測で埋めずにerrorで止める（知らない値を既定値や近い値に読み替えない）。状態遷移や人の判断の分類は、知らない値を寛容に読んでも正しく動けないからである（ADR-0073決定19の理由と同じ）。DBの行をその場で直すことはせず、直すのはmigrationか人の判断で行う。
4. **値を足すことの互換は、CHECKの有無ではなく読む側で決める（ADR-0073決定6を変える）。** CHECKが無いので、列に値を足しても表の作り直しは要らない。ただし古いバイナリがその値をfail closedで読む列（決定3のkind以外の列）に値を足す変更は、今までどおり非互換で、下限を上げる非互換のmigrationを伴い、適用はADR-0073決定8・14のdrainを経る（決定8のdrainが要る変更の「CHECKで値を列挙した列への値の追加」は、この「fail closedで読む列への値の追加」と読む）。表を作り直さずに下限だけを上げる形でよい。kindの列への値の追加は、ADR-0073決定6のとおりmigrationを要さない。
5. **見直しはschemaが安定したと人が判断したときにする。** そのときにCHECKを戻すか、どの規則をDBにも置くかを新しいADRで決める。それまでは、この決定を理由に新しいmigrationのCHECKを検査で拒む。

CHECKを外すmigrationの番号、表の作り直しの手順、検査のscriptの名前とerrorの文言、規則ごとの関数とtestの名前は[Persistence](../design/persistence.md)が持つ。

## Alternatives

- **kindを名指さないCHECKを残す（ADR-0073のまま）**: 値の追加のたびに表の作り直しとdrainが続く。taskごとのworkerのproviderの列のように、kind以外の列にも値は足される。
- **値の一覧のCHECKだけを外し、json・NULLの組・範囲のCHECKは残す**: 値の一覧以外のCHECKも列の意味の調整で書き換えが要り、そのたびに表を作り直す。規則の置き場所が2つに割れ、どちらが正か読む側が迷う。
- **CHECKの代わりにtriggerの`RAISE`で守る**: triggerは作り直さずに差し替えられるが、規則が2か所（RustとSQL）に分かれて食い違いうるうえ、`RAISE`を含むtriggerは互換のmigrationの範囲の外である。
- **読むときも寛容にする（全ての列で知らない値を読み飛ばす）**: 状態や判断の分類を知らないまま動くと、claimや着地を誤る。kindの列だけに限る。

## Consequences

- CHECKが無くなった後は、DBを直接書き換えた場合や規則を持たない古いバイナリが書いた場合に、規則の外の行が入りうる。書き込み口はRustだけで、DBは手で直さない運用なので、そのような行は読み込みのerrorとして見つかる。
- 値の追加は表の作り直しを要さなくなる。kind以外の列への追加の非互換とdrainは残るが、表の作り直しの手順（id・index・trigger・viewの作り直し）とその誤りの余地は無くなる。
- 全てのCHECKを外すmigrationは表を作り直すので、1回だけ非互換で、本番の入れ替えは`approve_update`のaskと人の`install --allow-breaking`になる。
- 実装（規則ごとのtest、CHECKを外すmigration、新しいmigrationのCHECKの検査）はgoal 60の別のtaskで行う。実装が着地するまで、既存のCHECKはDBに残る。
