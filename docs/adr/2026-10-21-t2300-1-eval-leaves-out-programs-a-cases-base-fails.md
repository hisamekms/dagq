---
id: adr-t2300-1
type: adr
title: evalはケースのprogramをまずケースのbase_commitのtreeで流し、そこで0以外で終わるprogramはそのケースで流さず、差分が足した違反だけでケースを止める（ADR-t1728-1決定13をamends）
status: accepted
created: 2026-10-21
updated: 2026-10-21
accepted_on: 2026-10-21
amends:
  - adr-t1728-1 decision 13
owners:
  - hisamekms
tags:
  - runtime
  - review
  - eval
related:
  - adr-t1728-1
  - adr-t1895-2
  - design-agent-eval
---

# ADR-t2300-1: evalはケースのbaseで落ちるprogramをそのケースで流さず、差分が足した違反だけでケースを止める（ADR-t1728-1決定13をamends）

## Context

[ADR-t1728-1](2026-10-06-t1728-1-agent-definitions-cases-and-eval-as-a-queue-service-use-case.md)決定13は、evalが各ケースに本番のreviewと同じ段を当て、landing branchのcommitのprogramをケースのtreeに対して先に流し、落ちたケースをagentに渡さず成績の分母から外すとした。

ケースの`base_commit`は古い（Spikeのケースはf46a7963など）。そのbaseには、後から入った規則（docsのfrontmatter、AGENTS.mdの大きさなど）に既に違反するfileが残る。programはtree全体を検査するので、docsを変えるケースは差分と関係なくその違反で止まる。task 1919の測定では、adr-rulesのdevの53件全部・hold-outの11件全部・productionの29件全部と、migration-rulesのproductionの14件中13件が止まり、agentに渡るケースがほぼ無くなった。着地の前のdevの閾値とhold-outの採点が空の周になる。

本番のrunのreviewはlanding branchをbaseにし、CIがmainでprogramを流すので、baseに既存の違反は無く、runを止めるのは差分が足した違反だけである。evalをこれに揃える。

## Decision

1. **programはまずケースのbaseのtreeで流す。** ケースに選ばれたprogram（決定13のまま、pathがケースの差分に当たるもの）を、ケースのtreeより先に、ケースの`base_commit`のtree（ケースのtreeと同じく、evalが隠すケースの一覧とpatchを除く）で設定の順に流す。
2. **baseで0以外のprogramはそのケースで流さない。** baseで0以外で終わったprogramは、そのケースの規則が変更の前から破れているので、そのケースのtreeで流さず、ケースを止めない。除いたprogramはケースごとに名前を記録し、周を引き取ったsupervisorが読み直せる。
3. **baseで0のprogramだけで決定13のとおり止める。** 残りのprogramをケースのtreeで設定の順に流し、最初に0以外で終わったものでケースを止め、agentに渡さず分母から外して記録する（決定13のまま）。
4. **baseでの起動の失敗・時間切れは周をincompleteにする。** ケースのtreeでの失敗と同じく、違反の有無が分からないので周をincompleteにし、閾値との比較でpassに数えない。
5. 汎用の振る舞いにする。外部のpjでも古いbaseのケースは同じ問題を持つので、runtimeの既定の振る舞いとし、設定で切り替えない。決定13のほか（programの読み出し元のcommit、env・backend、built-inのprogramを当てないこと、費用に数えないこと）は変えない。

## Alternatives

- **ケースのbaseを進める（patchを作り直す）**: ケースのラベルの変更は人・inbox・plannerに限られ（ADR-t1728-1 (e)）、Spike由来の約100件のpatchの付け替えは重く、規則が変わるたびに繰り返しになる。
- **dagq.tomlの設定にする**: 古いbaseのケースはどのpjにもあり、切った状態で得るものが無い。
- **差分の行だけを検査するようprogramを変える**: programはpjのscriptで、evalの都合で形を決められない。本番のreviewと同じscriptを流す決定13とも合わない。

## Consequences

- baseに既存の違反があり差分がそのpathを変えるケースはagentに渡り、採点される。差分が新しく足した違反のケースは今と同じく止まる。
- 除いたprogramの規則に差分がさらに違反しても、そのケースはprogramで止まらない（baseで既に落ちるので区別できない）。その規則をagentの定義が受け持つなら、agentが判定する。
- programはケースごとに最大2回流れる。同じ周の同じbaseのprogramの結果は使い回す。
- 今の姿は[agent eval](../design/agent-eval.md)の「programのreviewの当て方」が持つ。
