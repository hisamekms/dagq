---
id: adr-t5107-1
type: adr
title: workerのslotから着地の列を外しchangeによらず空きを使う
status: accepted
created: 2026-11-17
updated: 2026-11-17
accepted_on: 2026-11-17
amends:
  - adr-t1591-1 decision 1
  - adr-t1591-1 decision 2
  - adr-t1591-1 decision 3
  - adr-t1591-1 decision 4
  - adr-t1591-1 decision 7
  - adr-0071 decision 5
amended_by:
  - adr-t5265-1
owners:
  - hisamekms
tags:
  - architecture
  - capacity
related:
  - adr-t5064-1
  - adr-t610-1
---

# ADR-t5107-1: workerのslotから着地の列を外しchangeによらず空きを使う

## Context

着地の列がworkerの枠を埋め、重いinterruptのtaskが数時間claimされなかった。
人は着地の列をworkerから分け、空いたworkerの枠で重いtaskも進めると決めた。
着地は直列なので、作業だけ速めても着地は早まらず、順番待ちのworktreeとbuildを無制限に積むべきではない。

## Decision

1. **workerと着地の列を分ける。** ADR-t1591-1決定1とADR-0071決定5を改める。leaseを持つrunのうち、人の待ちの印も無く着地の列でもないrunをworkerのslotに数える。着地の列は着地中と、review・e2eを終えて着地の順番だけを待つrunである。worker・revise・resume・review・e2eの途中はworkerに残す。reviewはsessionを、e2eはhostの重いbuildを使うためである。着地はqueue全体で1本ずつのままにする。
2. **順番待ちに背圧を保ち、重いtaskも空きを使う。** ADR-t1591-1決定2を改める。着地中はworkerのslotに数えず、着地の順番待ちを外に数えるのはparallel本までで、超過分はworkerのslotに数える。parallelはworkerのslotの上限である。claimの可否はこの数がparallel未満かで決まり、changeに依らない。
3. **軽さの分類と枠を切り離す。** ADR-t1591-1決定3の、repositoryの設定が軽いchangeを選び、値はtasksのchangesから選ぶこと、runtimeが値の意味を持たないこと、pathsの宣言が無いtaskは軽くないことは保つ。light_changesはclaimの枠を開かない。空でも着地の列はworkerのslotの外に数えるので、空なら従来どおりという約束を改める。
4. **軽い枠を無くす。** ADR-t1591-1決定4の軽い枠とpriorityの規則は効く場面が無い。interrupt・urgentの重いtaskも、空いたworkerのslotでclaimの順の先頭に来る。軽い枠の周と、そのための新しい控えは起きない。
5. **戻りもworkerの空きを待つ。** ADR-t1591-1決定7の通常の枠を、着地の列を除いたworkerのslotがparallel未満という意味に改める。着地に失敗してneeds_sessionに戻ったrunは着地の列を出てworkerに数え、そのresumeも同じ空きを待つ。人の待ちからの戻りを新しいclaimより先にする規則は保つ。claim・resume・復旧・adopt・取り残されたrun・e2eの開始・着地の答えのadopt・待ちからの戻りは同じworkerのslotの数で判じる。ADR-t1591-1末尾のused_slotsの扱いもこれに従う。
6. **負荷と空き容量の関門を保つ。** ADR-t1591-1決定5のload・ディスク・claimの間隔は全てのclaimに効き、着地はloadで止めない。run.envの並列度はworker3本とintegrate1本が同時にcargoを回す前提なので、着地を外すとその前提に合う。着地の列はsessionを持たず、決定6の人の待ちの上限に数えないことを保ち、新しいメモリの関門は足さない。空き容量はADR-t5064-1の段階と同じく着地をINTEGRATORに分け、順番待ちは今の大きさだけを占める。常に取るINTEGRATORの予約と式は変えず、claim本数の並列度の上限はworkerの上限として読む。

ADR-t1591-1決定8とADR-t610-1決定1の表示・集計はこのADRでは改めず、後続の判断に委ねる。

## Alternatives

- 着地の列が空ける枠を軽いtaskに限る: 重いinterruptが着地の列のために止まる。
- 順番待ちを全て無制限に外す: 着地を早めず、worktreeと待ちのbuildだけを積み上げる。
- reviewとe2eも外す: sessionとhostのbuildの同時数がworkerの上限を超える。

## Consequences

着地の列があってもworkerの空きで重いtaskが始まり、順番待ちの超過はclaimに背圧を掛ける。
表示と集計の変更は判定と分けて行うため、その更新までは読み手が両者の違いを補う。
