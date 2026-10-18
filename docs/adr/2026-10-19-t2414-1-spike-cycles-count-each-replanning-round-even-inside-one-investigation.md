---
id: adr-t2414-1
type: adr
title: goalのSpikeの周期を再計画の回ごとに数え、同じ調査中の計画の中で追加のSpikeを連鎖させても周期の上限が止めるようにする（ADR-t1487-1決定8をamends）
status: accepted
created: 2026-10-19
updated: 2026-10-19
accepted_on: 2026-10-19
amends:
  - adr-t1487-1 decision 8
owners:
  - hisamekms
tags:
  - runtime
  - planner
related:
  - adr-t1487-1
  - adr-t808-1
  - design-supervisor-lifecycle-plan-planners
---

# ADR-t2414-1: goalのSpikeの周期を再計画の回ごとに数え、同じ調査中の計画の中で追加のSpikeを連鎖させても周期の上限が止めるようにする

## Context

[ADR-t1487-1](2026-10-04-t1487-1-spike-is-an-execution-class-with-a-judged-result-and-a-durable-replanning-request.md)決定8は、goalのSpikeの周期を「goalが調査中の計画になった回数（決定7の取るから解放までを1回）」で数える。
決定7の表は、依頼が`proposed`で追加のSpikeがsubmit〜readyにあるあいだ枠を保持する。
そのため依頼のplannerが再計画のたびに追加のSpikeを出すと、調査中の計画は閉じず、周期は1のまま続く。
上限が2以上だと連鎖は一度も止まらず、上限が効くのは1のときだけになる。
goal 96の「Spikeの繰り返しは回数の上限を持ち、自動で延長・連鎖しない」に反する。

## Decision

1. **周期はSpikeの再計画の回で数える。** goalのSpikeがclaimされて調査中の計画が始まったときが1回目で、その結果を読む依頼のplannerが追加のSpikeをsubmitしたら次の回に入る。
   次の回に入るのは、goalのSpikeの依頼が`open`（結果を再計画している）あいだのruntimeのplannerのsubmitで、依頼の前（Spikeが走っているあいだ）に足したSpikeはその回に加わる。
   同時に走った複数のSpikeは今までどおり1回で、1回のsubmitに入る複数の追加のSpikeも1回にする。
   plan reviewが差し戻した追加のSpikeを出し直しても回は増えない。回はsubmitで数え、後でplan reviewが退けてもSpikeが取り消されても戻さない（数え直しの判断をruntimeに持ち込まない）。
2. **調査中の計画の枠は決定7のまま。** 追加のSpikeのsubmitで次の回に入っても、調査中の計画は閉じず枠を保持する。
   同時数の上限（決定7）と回数の上限（この決定）は別の柵で、前者は調査の広がりを、後者は1つのgoalの調査の総量を絞る。
3. **上限の判定と人の扱いは決定8のまま。** 上限は追加のSpikeのsubmitで判定し、上限に達したgoalの追加のSpikeはruntimeのplannerのsubmitを拒んで`planner_question`の案内を出す。
   人のsubmitは拒まず、回にも数えない（人が範囲を広げる判断をした後も、次の回へ進むにはまた人を経る）。
   上限の前にsubmitしたSpikeの完了が依頼を残すこと、追加のSpikeをgoalに属させること、follow_upの深さの上限（[ADR-t808-1](2026-09-28-t808-1-runtime-planners-submit-follow-ups-up-to-depth-two.md)）と別の柵であることは変えない。

回の記録の形とどの経路が回に入るかは`domain::investigation`のdoc commentが持つ。

## Alternatives

- **連鎖を1周期とADRに書く（字句どおりの実装を追認する）**: 上限が2以上のとき連鎖を一度も止めず、上限が1つのgoalの調査の総量を絞らないので、未解決を理由に延長・連鎖しない柵にならない。
- **調査中の計画を追加のSpikeのsubmitで閉じ、開き直す**: 回は数えられるが、決定7の枠が一度空き、上限で控えていた他のgoalのSpikeが割り込んで同時数の柵と混ざる。
- **人のsubmitも回に数える**: 上限に達した後の判断は人がしており、数えてもruntimeのplannerを止める結果は変わらない。人の判断の前の回だけが柵の対象である。

## Consequences

- 上限は1つのgoalで依頼のplannerが続けて出せる追加のSpikeの回数を絞り、超える延長は人の判断を経る。
- 上限が1のときの振る舞い（最初の回の後の追加のSpikeを拒む）は変わらない。
- 回の記録は今の周期の記録に足すので、schemaは変えない。
