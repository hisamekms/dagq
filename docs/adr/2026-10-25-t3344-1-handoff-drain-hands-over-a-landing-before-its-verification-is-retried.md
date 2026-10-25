---
id: adr-t3344-1
type: adr
title: 自動更新のhandoffのdrainは、既に始まっていた着地の検証がやり直しになるときやり直さず、runをleaseを持ったまま着地slotの待ちとして次のprocessに引き継ぐ（ADR-t3077-1決定1・ADR-t639-1決定2・ADR-t1039-1決定1をamends）
status: accepted
created: 2026-10-25
updated: 2026-10-25
accepted_on: 2026-10-25
amends:
  - adr-t3077-1 decision 1
  - adr-t639-1 decision 2
  - adr-t1039-1 decision 1
owners:
  - hisamekms
tags:
  - runtime
  - integrate
related:
  - adr-t3077-1
  - adr-t3048-1
  - adr-t639-1
  - adr-t768-1
  - adr-t1039-1
  - adr-t632-1
  - adr-t1091-1
  - design-supervisor-lifecycle-handoff
  - design-supervisor-lifecycle-integrate
---

# ADR-t3344-1: handoffのdrainは着地の検証をやり直さず、runを着地slotの待ちとして引き継ぐ

## Context

handoffのdrainは新しい着地を始めず、要求の時点で既に始まっていた着地の終わりを待つ（ADR-t3077-1決定1）。
着地の検証は、hostの分類の失敗（ADR-t639-1決定2）とflakyだけの失敗（ADR-t1039-1決定1）を、同じ試行・同じ着地の枠と同じheadでもう1回流す。
着地の検証は1回15〜20分かかるので、drainのあいだにやり直しが入ると、drainは検証2回分を待ってhandoffの上限（1800秒）を超える。
2026-10-24には、要求とほぼ同時に始まった着地の1回目の検証がflakyで落ちてやり直しになり、handoffが1800秒で失敗して前のbinaryに戻った。
上限を延ばすと、やり直しの回数だけ待ちが延び続ける。
走っている検証を次のprocessに引き継ぐことは、子processの検証をexecをまたいで持てないのでできない。
方針は人が決めた（ask 1199）。

## Decision

1. **handoffのdrainのあいだに、既に始まっていた着地の検証がやり直しになるときは、やり直しの前で区切り、runをleaseを持ったまま着地slotの待ちとして次のprocessに引き継ぐ。**
   drainするprocessはやり直しの検証を始めず、runを着地を始めた時の状態に戻し（着地slotは空く）、leaseは返さない。
   区切った理由（何がどう落ち、logがどこにあるか）をrunのeventに記録する。
   次のprocessは、ADR-t3077-1決定1の着地slotを待つrunと同じ経路で、reviewもresumeもやり直さずにそのrunを着地の列に戻し、着地を最初から（rebaseから）やり直す。
   区切りで終わった着地は、runを着地の列から外したものとして数えない（着地の列に並べた記録はそのまま効く）。
   これでdrainが待つ着地の時間は検証1回分を超えない。
2. **ADR-t3077-1決定1の「drainは要求の前に始まっていた着地の終わりを待つ」に、決定1の例外を足す。**
   着地を始めないこと、着地slotを待つrunを保留の有無に依らずleaseを持ったまま引き継ぐこと、止めるdrain（`down`・provisioningの失敗）の扱いは保つ。
3. **ADR-t639-1決定2の「hostの分類の失敗は同じ`integrate`の試行の中で1回だけやり直す」に、handoffのdrainのあいだに限り、やり直しをその試行で行わず次のprocessの新しい試行に回す例外を足す。**
   handoff以外（drainしていないとき・止めるdrain・人の`integrate`）は同じ試行でのやり直しを保つ。
   止める要求はhandoffに勝つので、handoffの要求の後に止める要求が来れば、同じ試行でやり直す。
   決定1・3〜5（分類の集合、やり直しても落ちたら`held`とinbox、時間切れの扱い、集合を広げないこと）は変えない。
   区切りの前に空き容量が足りずやり直さないと分かった`disk_full`は、今どおり決定3で`held`にする。
4. **ADR-t1039-1決定1の「flakyだけの失敗のやり直しは同じ着地の枠・同じheadで着地の試行ごとに1回」に、handoffのdrainのあいだに限り、やり直しをその枠で行わず次のprocessの新しい着地で（rebase後のheadに対して）行う例外を足す。**
   「1回」は次のprocessの新しい着地で数え直す。
   handoff以外は同じ枠・同じheadでのやり直しを保ち、決定2〜4は変えない。

3つのADRは番号付きの決定を複数持ち、変えるのは各1つの一部なので、丸ごと置き換えずamendsにする（ADR-t1091-1）。
handoffの上限そのもの、前のbinaryに戻す規則（ADR-t632-1）、着地の検証そのものの長さは変えない。

## Alternatives

- **handoffの上限を延ばす**: やり直しの回数だけ待ちが延び続け、上限が有界にならない。
- **走っている検証を次のprocessに引き継ぐ**: 検証は子processなので、execをまたいで持てない。
- **やり直しを諦めて人に上げる（`held`）・resumeする**: コードの問題でもhostの恒常的な問題でもないものに人かsessionを使う。次のprocessに回せば自動のまま済む。

## Consequences

- handoffをまたいだ着地は、次のprocessで最初からやり直すので、rebaseと検証の1回目をもう一度流す。やり直しの1回分より長くなることがある。
- flakyのやり直しの「着地ごとに1回」は、handoffで区切った着地と次の着地で合わせて2回のやり直しの機会になりうる。
- 区切られた着地はやり直しの記録を残さず、区切りの記録だけを残す。やり直しの集計はそれを数えない。
