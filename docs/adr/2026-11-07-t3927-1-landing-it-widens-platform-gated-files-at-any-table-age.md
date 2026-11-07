---
id: adr-t3927-1
type: adr
title: 着地の絞ったITは、表が記録したmacOSだけのコードを持つファイルの変更を表の古さに依らず広げる（ADR-t3920-1決定2をamends）
status: accepted
created: 2026-11-07
updated: 2026-11-07
accepted_on: 2026-11-07
amends:
  - adr-t3920-1 decision 2
owners:
  - hisamekms
tags:
  - testing
  - integrate
  - ci
related:
  - adr-t3920-1
  - adr-t3926-1
  - adr-t2892-1
  - adr-t1925-1
  - plan-landing-it-selection
  - design-supervisor-lifecycle-validation
---

# ADR-t3927-1: OSの条件による表の欠けは古さに依らず広げる

## Context

[ADR-t3926-1](2026-11-06-t3926-1-it-coverage-map-is-built-on-a-schedule-by-parallel-linux-jobs.md)の結果、表はLinuxで作られる。
[スパイク](../plans/it-coverage-map-linux-spike.md)の(d)が示すとおり、OSに依らない部分の結びつきが残っていても、macOSだけの関数・分岐を通るITの結びつきは欠ける。
この欠けは古さでなく作るOSから来るので、新しい表でも埋まらない。
人の決定（goal 244の受け入れ(2)、request 210）は、そのファイルの変更ではITを広めに流すこと。

[ADR-t3920-1](2026-11-01-t3920-1-landing-it-keeps-using-an-old-table-and-widens-the-files-it-does-not-know.md)の4つの決定のうち、変えるのは決定2の広げる条件だけなので、置き換えでなくamendsにする（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。

## Decision

1. **表が記録したmacOSだけのコードを持つファイルは、表の古さに依らず広げる。**
   ADR-t3920-1決定2の「古い表では」と広げる対象の定義を変え、表のtreeに無いファイルと表のcommitの後にmainで変わったファイルに加え、表が記録したmacOSだけのコードを持つファイルを対象にする。
   そのファイルは、表に載っていても自分の項目のITに加えて広げる。
   unit testだけのファイルと広い項目は広げない。
   Decision末尾の「古くない表（短い方の段以内）の扱いは変えない」という一文も、この対象については広げるものに改める。
   該当ファイルの記録が無い旧形式の表では、この追加の広げ方を行わない。

保つもの: 決定2の広げる先（同じdirectoryと宣言するmoduleの項目、広い項目を除く）、決定3（広げたITも閾値に数える）、決定4（出力に理由と数を名指す）。
ADR-t2892-1決定1（古くない表で表のtreeに無いファイルは何も選ばない）は上の追加対象を除いて保ち、決定2（広い項目は選ばずCIに任せる）とADR-t1925-1決定2の閾値は保つ。

## Alternatives

- **古い表のときだけ広げる**: OSによる欠けは新しい表にもあり、更新しても埋まらないため、変更を確かめる範囲が足りない。
- **広げずCIに任せる**: 着地は速いが、人の「広めに流す」という決定を満たさない。
- **そのファイルの変更でITを全部流す**: 欠けを最も広く拾えるが、既存の近くの項目への広げ方と閾値で範囲を決められるので、一律の全件は採らない。

## Consequences

- 新しい表でもOSによる欠けを補うために選択が広がる。広げる規則は既存のものを使い、別の範囲を持たない。
- src/infrastructureの該当ファイルは隣が多く、閾値で全件に落ちる着地が増えうる。入れた後の時間と「表に無いIT」の割合はtask 3928が測る。
- 別のdirectoryから使われる経路の漏れは残り、macOSのCIが最終関門として拾う。
- 選び方の今の姿は[Validation](../design/supervisor-lifecycle/validation.md)、根拠と広げる先は[測定の「決めたこと」](../plans/landing-it-selection.md)が持つ。
