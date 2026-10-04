---
id: adr-t1591-1
type: adr
title: 着地の順番を待つだけのrunをslotの外に数え、それで空いた枠ではrepositoryが軽いと決めたchangeの、--pathsを宣言したtaskだけをclaimする（ADR-0071決定5・7・8・12、ADR-t610-1決定1をamends）
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-0071 decision 5
  - adr-0071 decision 7
  - adr-0071 decision 8
  - adr-0071 decision 12
  - adr-t610-1 decision 1
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - capacity
related:
  - adr-0062
  - adr-0071
  - adr-t610-1
  - adr-t980-1
  - adr-t1479-1
  - adr-t1410-1
  - design-supervisor-lifecycle-waiting
  - design-supervisor-lifecycle-claim-hold
  - design-supervisor-lifecycle-status
  - design-supervisor-lifecycle-run-environment
---

# ADR-t1591-1: 着地の順番を待つだけのrunをslotの外に数え、それで空いた枠ではrepositoryが軽いと決めたchangeの、`--paths`を宣言したtaskだけをclaimする（ADR-0071決定5・7・8・12、ADR-t610-1決定1をamends）

## Context

supervisorは、reviewを終えて着地（`integrating`）の順番を待つだけの`awaiting_integration`のrunもleaseを持ったままslotに数える（[ADR-0071](0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)決定5、[ADR-t610-1](2026-09-27-t610-1-landing-runs-fill-the-slot-in-status-and-stats.md)決定1）。着地はqueue全体で1件ずつの直列なので、2026-10-03 23:36 JSTには3つのslotが着地中1件と着地の順番待ち2件で埋まり、作業中のworkerは0でCPUとメモリに余裕があるのに、interruptのtaskも含めてclaimが止まった（goal 104、request 9）。

一方、`parallel`と`[run.env]`のbuildの並列度は、同時に走る重いbuildの数を前提に決めてある。順番待ちのrunを単にslotの外に出すと、重いtaskの同時数が増えてその前提が崩れる。直近7日の着地では、docsとconfigのtaskは作業の中央値が3〜4分・p90が6〜12分で検証のコマンドも無く、feature・fix・testより桁違いに軽い。

## Decision

1. **着地の順番を待つだけのrunを決める。** `awaiting_integration`で、reviewがpassし（承認済みで）、要るe2eも終わり（要らないものを含む）、別のrunの着地が終わるのだけを待つrunを「着地待ち」と呼ぶ。着地中（`integrating`）のrunと、review・revise・e2e・resumeの途中のrunは、今のままslotに数える。
2. **重いtaskの数え方は変えない。着地待ちが空けた枠は軽いtaskだけに使う。** slotの数は今までどおり着地待ちを含めて数え、重いtaskはそれが`parallel`未満のときだけclaimする。軽いtaskは、着地待ちを除いた数が`parallel`未満ならclaimしてよい。重いtaskの同時数は今より増えない。着地待ちのうちslotの外に数えるのは`parallel`件までとし、それを超える分はslotに数える（着地は直列なので、着地待ちが`parallel`を超えて積み上がってもそれ以上の枠は作らない）。
3. **軽いtaskはrepositoryの設定が決め、runtimeは値の意味を持たない。** 軽いchangeの集合は`dagq.toml`の`[supervisor]`の設定で、値は`[tasks]`の`changes`（[ADR-t980-1](2026-09-29-t980-1-classify-runs-by-declared-change-and-diff-derived-area.md)）の中から選ぶ。既定は空で、空なら今と全く同じに振る舞う。新しいラベルやtaskの欄は足さない。`--paths`を宣言していないtaskは、changeが軽くても軽い枠ではclaimしない（何でも変えられるため）。changeと`--paths`の食い違いを止めるのはplan reviewの規則である。
4. **priorityは軽い枠を広げない。** 軽い枠はpriorityに関わらず軽いtaskだけに使い、interrupt・urgentの重いtaskも入れない（入れると重いbuildの同時数の前提が崩れる）。重いinterruptは次の通常の枠でclaimの順の先頭に来る。軽いtaskどうしは今のclaimの順（priority）に従う。
5. **claimの関門はそのまま効く。** loadとディスクによるclaimの保留（[claimを控える](../design/supervisor-lifecycle/claim-hold.md)）と、[ADR-t1479-1](2026-10-04-t1479-1-space-new-claims-while-the-load-hold-is-on.md)のclaimの間隔は、軽い枠のclaimにも同じに効く。軽い枠のclaimも1つのclaimとして間隔を始め、間隔の中では軽い枠でもclaimしない。
6. **着地待ちは人の待ちの上限に数えない。** 着地待ちは生きているsessionを持たず（sessionを閉じてから着地を待つ）、ADR-0071決定7の上限（`max_waiting`）の根拠であるsessionのメモリを使わない。上限に数えると、人の答えを待つrunが着地待ちのためにslotの外に出られなくなる。着地待ちの上限は決定2の`parallel`件とする。
7. **slotへ戻る規則は今のまま。** 着地待ちのrunが着地に失敗して`needs_session`（衝突のresumeなど）に戻ったときは、ADR-0071決定8の戻り待ちと同じく通常の枠（着地待ちを含めた数が`parallel`未満）を待つ。戻り待ちのrun（人の待ちが終わったもの）がslotの空きを待つあいだは、軽い枠でも新しいclaimをしない（決定8の「新しい仕事より先に戻す」を軽い枠にも当てる）。
8. **見え方はqueueの記録から組み立てる。** 着地待ちに出入りする新しいeventのkindは足さない（着地待ちは`landing_queued`・e2eの終わり・`integration_started`の既存の記録から読める）。軽い枠のclaimは`run_claimed`にそう記録する。`status`はsupervisorごとにslotの数から着地待ちを除いた数と着地待ちの数を分けて出し、runごとの枠の扱いにも着地待ちを出す。`stats`の`idle_slots`と`kpi`の`slot_usage`の数え方は変えない（空きは重いtaskが使える枠で数え、軽い枠は数えない）。欄の名前と形は[status](../design/supervisor-lifecycle/status.md)が持つ。

ADR-0071決定5の`used_slots`（claimとresumeとtriageと戻りの判定に使うslotの数）はそのまま着地待ちを含め、軽い枠の判定だけが着地待ちを除いた数を使う。決定12の`slots.used`は着地待ちを除いた数に改め、ADR-t610-1決定1の「着地の順番を待つものも埋まったslotに数える」は、`status`では着地待ちを別に数えるように改める。

## Alternatives

- **着地待ちを単にslotの外に出し、空いた枠を何にでも使う**: 重いbuildの同時数が着地待ちの分だけ増え、`parallel`と`[run.env]`の並列度の前提が崩れる。
- **着地（着地待ちと`integrating`）をworkerのslotから分け、別の着地の枠にする（task 962）**: 範囲が広く、進めるかを人が決めていない。この決定は着地待ちだけを外し、空いた分を軽いtaskに限るので、その判断を先取りしない。task 962が進むならこのADRをamendsする。
- **軽いchangeをruntimeに固定する（docs・config）**: changeの集合はrepositoryごとの`[tasks] changes`で、runtimeが値の意味を持たない（ADR-t980-1）。
- **ADR-0079の大きさ（size）で軽いtaskを決める**: sizeは順位にしか使わないと決めてある。
- **interruptの重いtaskも軽い枠に入れる**: 重いbuildの同時数の前提が崩れる。interruptは次の通常の枠の先頭で十分早い。
- **着地待ちを`max_waiting`に数える**: 人の答えを待つrunがslotの外に出られなくなり、上限の根拠（sessionのメモリ）にも合わない。

## Consequences

- 着地待ちで`parallel`が埋まっても、軽いtaskは進む。重いtaskの同時数とloadの保留は今のまま。
- 軽い枠のtaskが走るあいだ、slotの数（着地待ちを含む）は`parallel`を超えうる。その分重いtaskのclaimは遅れうるが、軽いtaskは短い。
- 軽いchangeを`dagq.toml`に足すのは、本番の固定バイナリがその設定を読めるようになってから（旧バイナリは未知のkeyで`dagq.toml`全体を読めない）。この変更ではこのrepositoryの`dagq.toml`に値を足さない。
- 効果は着地後の印の前後で`kpi --compare`の`slot_usage`・着地数/時・`landing_utilization`・`load_per_core`をchangeで層別に比べて見る（goalの受け入れ条件の外）。
