---
id: adr-t1981-1
type: adr
title: claimを控える判定の予想するファイルは--pathsのワイルドカードを含むglobを外し、具体的なパスが残らなければrelatedの着地の差分で予想する
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
amends:
  - adr-0080 decision 1
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - performance
related:
  - adr-0029
  - adr-0046
  - design-supervisor-lifecycle-claim-defer
  - design-supervisor-lifecycle-plan-review
---

# ADR-t1981-1: claimを控える判定の予想するファイルは--pathsのワイルドカードを含むglobを外し、具体的なパスが残らなければrelatedの着地の差分で予想する

## Context

[ADR-0080](0080-supervisor-rereads-conflicts-config.md)の決定1は、候補のtaskが触ると予想するファイルを宣言した`--paths`（globのまま）とし、宣言が無いときだけ`dagq related`で最も似た`completed`のtask 3件の着地commitが変えたファイルを使う。plan reviewのpromptの予想するファイルも同じ規則で求める。

request 51（2026-10-06、goal 164）の実測では、9/29以降にhotspotで控えて着地した158件のうち、`--paths`のglobを元にした控えの外れが64/70（91%）、relatedを元にしたものが58/88（66%）、合計の外れが122/158（77%）だった。待った時間（控えが終わったときに記録した控えの時間の和）は外れが約192時間、当たりが約55時間。`--paths`の外れは、docs・configのtaskの`docs/**`のような広いglobがdocsのhotspotの全部に当たるためで、範囲の宣言（変えてよい）を予想（触る予定）に流用していることが原因。

## Decision

1. ADR-0080の決定1を改める。予想するファイルは、宣言した`--paths`のうちワイルドカード（`*`・`**`・`?`）を含まない具体的なパスだけとする。
2. 具体的なパスが1つも残らなければ（`--paths`が無いときと同じく）、`dagq related`で最も似た`completed`のtask 3件の着地commitが変えたファイルで予想する。どちらも無ければ何も予想せず、控えない。
3. 進行中のrunのファイル（ADR-0080の決定2）は、そのtaskの予想するファイルにこの規則を当て、base commitからheadまでの差分は今どおり足す。plan reviewのpromptの予想するファイルも同じ規則で求める。hotspotに当たるかの判定はglobを解釈するまま変えない。
4. `--paths`のscopeの検査（validation・integrateの範囲外の変更の検出）と軽い枠の判定は変えない。globは範囲の宣言としてそこで今どおり使う。

## Alternatives

- **広いglob（`docs/**`など）だけを外す**: 広さの線引きに閾値が要り、狭いglobでも宣言は予想ではないという原因は残るので採らない。
- **plannerが予想ファイルを別の欄に書く**: 欄・CLI・planner の指示の変更が要り、今すぐの小さい対処に収まらない。goal 159の着地後に人が別に依頼する範囲として残す。

## Consequences

- globだけを宣言したdocs・configのtaskはrelatedの着地の差分で予想されるので、docsのhotspotの全部に当たる控えが減る。relatedの外れ（66%）は残る。
- 具体的なパスとglobを混ぜて宣言したtaskは、具体的なパスだけで予想され、globの範囲で触ったファイルとの衝突を控えで避けられないことがある（衝突は着地のrebaseに寄る）。
- 控えの当たり率のstatsへの計測と見逃しの測定は入れない。前後比較の基準値はContextの数字。
- 現在の仕様は[claimを控える（衝突の多いファイル）](../design/supervisor-lifecycle/claim-defer.md)と[Plan review](../design/supervisor-lifecycle/plan-review.md)に置く。
