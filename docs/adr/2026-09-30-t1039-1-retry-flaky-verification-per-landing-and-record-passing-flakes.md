---
id: adr-t1039-1
type: adr
title: flakyだけの着地の検証は試行ごとに1回やり直し、やり直しのFLAKYは記録して着地させる
status: accepted
created: 2026-09-30
updated: 2026-09-30
accepted_on: 2026-09-30
amends:
  - adr-t768-1 decision 2
  - adr-t768-1 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - testing
related:
  - adr-0076
  - adr-t639-1
  - adr-t920-1
  - design-supervisor-lifecycle-integrate
  - design-supervisor-lifecycle-stats
---

# ADR-t1039-1: flakyだけの検証を着地の試行ごとにやり直し、FLAKYを記録して着地させる

## Context

ADR-t768-1決定2・3は、不安定なtestだけで落ちた着地の検証をrunごとに1回やり直し、なお落ちればworkerをresumeすると決めた。finding 48のtask 845は、やり直しとresume後の着地がそれぞれFLAKYだけで落ち、数えられるresumeを2回使った。sessionは1107秒、着地待ちは4163秒で、statsは無関係な799と829の着地を壊したものとして名指した。既存の不安定なtestを直すのはobserverのfindingから立つtaskであり、そのrunのworkerを再開しても変更は進まない。

## Decision

1. **flakyだけの失敗に対する検証全体のやり直しは、着地の試行ごとに1回にする。** rebase後の同じheadに対して同じ着地の枠内で行う。run全体の過去のやり直しは制限に数えず、resumeや復旧の後の新しい着地にも1回を認める。
2. **やり直しに限り、nextestの流し直しで通ったtestは検証の成功として扱う。** 1回目は引き続きFLAKYを失敗とし、判別して記録する。やり直しでFLAKYだけならcoverageの関門を含む残りの検証へ進み、全て通れば着地する。FLAKYの名前は成功した検証でも記録し、statsとobserverのfindingの材料にする。これによりADR-0076決定2の「不安定なtestを隠さない」は記録・集計・findingで保つ。変更自身が入れた不安定さはworkerのstressとCIの定時実行（ADR-t920-1）が見る。CI・人の手元・workerのstressの設定は変えない。
3. **やり直しで本当のtestの失敗やbuildの失敗が出れば、従来どおりworkerをresumeする。** hostの分類はADR-t639-1の扱いを保つ。flakyだけならsessionも数えられるresumeも使わない。
4. **flakyの分類による検証の延期は、過去の記録も含め、rebase先の着地を壊したものとして名指さず、壊したrunの数にも数えない。** rebase先の情報自体は残す。

## Alternatives

- flakyだけのやり直しを何度も繰り返す: 着地の検証は直列なので、後ろのrunを待たせる（finding 56）。1回で区切る。
- resumeを数えずにsessionに戻す: 上限を使わなくてもsessionと時間を使い、変更は進まない。
- holdして人に知らせる: hostの故障と違い、人の判断は要らない。
- 最初からFLAKYを成功として扱う: まず失敗として判別・記録し、1回検証をやり直す境界を保つため採らない。

## Consequences

flakyだけの失敗はworkerのresume回数や無関係な着地の責任を増やさない。追加の検証は試行ごとに1回までで、継続して落ちるtestやcoverage不足は通さない。event・payload・envの綴りと集計は[Integrate](../design/supervisor-lifecycle/integrate.md)と[Stats](../design/supervisor-lifecycle/stats.md)が持つ。
