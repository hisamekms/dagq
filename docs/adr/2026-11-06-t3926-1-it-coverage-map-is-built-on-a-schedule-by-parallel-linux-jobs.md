---
id: adr-t3926-1
type: adr
title: 着地のITを絞る対応表を、夜間のmacOSの1 jobから、定時のLinuxの並列jobで作る（ADR-t1925-1決定2をamends）
status: accepted
created: 2026-11-06
updated: 2026-11-06
accepted_on: 2026-11-06
amends:
  - adr-t1925-1 decision 2
owners:
  - hisamekms
tags:
  - testing
  - integrate
  - ci
related:
  - adr-t1925-1
  - adr-t2892-1
  - adr-t3920-1
  - adr-t3924-1
  - adr-t1091-1
  - plan-landing-it-selection
  - plan-it-coverage-map-linux-spike
  - design-supervisor-lifecycle-validation
---

# ADR-t3926-1: 着地のITを絞る対応表を、定時のLinuxの並列jobで作る

## Context

[ADR-t1925-1](2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)決定2は、ITを「CIの夜間の対応表」と差分で選ぶ。
表は夜間にmacOSの1 jobで全部のITを流して作っていた。

表が古いほど「表に無いIT」（表の後に足されたtest）が増え、着地の検証はそれを必ず流す。
夜間に1本の表では、1回の失敗で表が次の夜までさらに古くなり、macOSのrunnerの都合で実行が止まると[ADR-t3920-1](2026-11-01-t3920-1-landing-it-keeps-using-an-old-table-and-widens-the-files-it-does-not-know.md)の古い表の段に入る。

人の決定（request 210）は、macOSの枠を使わず、Linuxのrunnerで全件の作り直しを並列にして周期を縮め、「表に無いIT」を減らすこと。
[スパイク](../plans/it-coverage-map-linux-spike.md)で、対象のITはLinuxでも安定して通り、同じ名前の集合で、方式はLinuxで壊れず、短い周期に収まる所要だと確かめた。

amendsにするのは、ADR-t1925-1の決定のうち決定2の表の作り方の前提だけが変わるため（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。

## Decision

1. **表は、定時のLinuxの並列jobで作る。**
   ADR-t1925-1決定2（ADR-t2892-1・ADR-t3920-1のamendsの後の今の決定）の「CIの夜間の対応表」のうち、周期と作るOSだけを変える。
   表はLinuxのrunnerで、ITをpartitionに分けたjobが流し、最後のjobが1つに合わせる。
   夜間に限らない短い定時で作り直し、macOSのrunnerを使わない。
2. **欠けた表を出さない。**
   partitionの結果が欠ける、作ったcommitが揃わない、流れたtestの集合が一覧と合わないときは、表を出さず、実行を失敗にする。
   落ちたtestだけでは失敗にせず、表に状態を記録する。落ちたtestで失敗にすると、mainに落ちているtestがある間は成功の実行が無くなり、最新の成功した表を読む着地の検証と古さの見張りが止まるため。
3. **表は、作ったOSとOSの条件を持つファイルを記録する。**
   表がLinuxから作られたことと、macOSとの違いが出うるファイルを、表だけから読めるようにする。

保つもの: CIが作る最新の表で差分から選ぶこと、必ず含める2つ（差分のtestのファイルが定めるITと表に無いIT）、ITを全部流す条件、古さの2段（ADR-t3920-1決定1）。

周期とpartitionの数はworkflowの定義に、表のkeyの名前は表を作るscriptの冒頭に置く。

## Alternatives

- **macOSのままpushごとに作る**: 表は最も新しく、macOSだけのコードの結びつきも残るが、長いjobがmacOSの枠を着地ごとに占め、人の決定に反する。
- **夜間のまま**: 変更は要らないが、「表に無いIT」と古さの問題が残る。
- **差分だけの作り直し**: 1回は軽いが、差分に当たるtestの決め方と部分の表の継ぎ足しという新しい誤りの元を持ち込む。全件の作り直しが短い周期に収まるので今は作らず、入れた後の測定で要るかを改めて見る。

## Consequences

- 表が新しくなり、着地で流す「表に無いIT」が減る。macOSの枠を占めない。
- macOSだけのコードの結びつきが表から欠ける。macOSの分岐だけを通るtestは、そのファイルに結びつかないことがある。
- その変更でのITの広げ方は別のADR（ADR-t3927-1）が決める。表が記録するファイルの一覧はその材料になる。
- macOSのCIのjobが最終関門として残り、macOSだけのコードの壊れはそこで拾う。
- 周期が短くなり、走っている実行を止めないので、実行が重なると次の1本が待つ。溜まらないのは所要が周期に収まる間で、所要の実測はjobの記録で読む。
- 1つのpartitionの失敗で表が出ない実行は、古さの見張り（[ADR-t3924-1](2026-11-03-t3924-1-ci-watch-measures-declared-workflows-age-by-latest-success-and-dispatches-once-before-the-limit.md)）と次の定時で拾う。
