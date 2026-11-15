---
id: adr-t4979-1
type: adr
title: 着地の検証のunitの段は、一覧に理由と戻すtaskつきで載ったunit testだけを外し、CIのmacOS・Linuxのjobは流し続ける（ADR-t1925-1決定1をamends）
status: accepted
created: 2026-11-15
updated: 2026-11-15
accepted_on: 2026-11-15
amends:
  - adr-t1925-1 decision 1
owners:
  - hisamekms
tags:
  - testing
  - integrate
  - ci
related:
  - adr-t1925-1
  - adr-t1920-1
  - plan-landing-it-selection
  - design-supervisor-lifecycle-validation
---

# ADR-t4979-1: 着地の検証のunitの段から一覧のunit testを外し、CIで流し続ける

## Context

[ADR-t1925-1](2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)決定1は、着地の検証でunit testを全件流すと決めた。
速く、判断の多くをunit testが確かめるためだった。
2026-11-15に着地した3本のrunでは、着地の検証は1本37〜41分で、unitの段だけでnextestのSummaryが1,918〜2,131秒だった。
段の最後まで残るのは同じ6本の重いunit testで、1本で135〜1,303秒かかり、1回目の着地が3600秒の上限で時間切れになったrunもある。
人は、これらを取り急ぎ着地の検証のunitの段でだけ外して速度を戻し、軽くする直しは後の別のtaskにすると決めた（request 319、goal 291）。
外したtestの壊れでmainが壊れるリスクは人が受け入れた。

amendsにするのは、ADR-t1925-1の決定のうち決定1のunit testの範囲だけが変わり、他の決定（絞ったIT・既に落ちているtestの扱い・設定での切り替え・CIが最終関門）と、それをamendsした[ADR-t2892-1](2026-10-20-t2892-1-landing-it-leaves-added-sources-and-broad-entries-to-ci.md)・[ADR-t3920-1](2026-11-01-t3920-1-landing-it-keeps-using-an-old-table-and-widens-the-files-it-does-not-know.md)・[ADR-t3926-1](2026-11-06-t3926-1-it-coverage-map-is-built-on-a-schedule-by-parallel-linux-jobs.md)は変わらないため（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。

## Decision

1. **着地の検証のunitの段は、外す一覧に理由と戻すtaskつきで載ったunit testを流さない。他のunit testは今どおり全件流す。**
   - 一覧はrepositoryの設定のファイルに置き、testごとに理由・載せたtask・戻すtaskかgoalを持つ。戻すtaskが軽くするか分割した後に一覧から外し、unitの段に戻す。
   - 着地の検証の出力は、unitの段の前に外したtestの名前と件数と、CIが流すことを名指す。
   - 一覧が無いか空なら全件を流す。読めない一覧では着地の検証を止め、黙って全件にも全部を外すことにもしない。
   - ITの段の選び方は一覧を読まない。
2. **CIのmacOS・Linuxのjobは、一覧に載ったtestも流し続ける。外したtestの壊れはCIが最終関門として拾う（ADR-t1925-1決定5）。**
   - `#[ignore]`のようにCIでも外れる印は使わない。
3. **一覧に足すのは、goal 291のtaskと人が決めたときだけにする。**
   - 日々の遅いtestの受け皿にしない。遅いunit testは時間の関門と、軽くするか分割する直しで扱う。

## Alternatives

- `#[ignore]`で外す: CIでも流れなくなり、最終関門が失われる。
- 既に落ちているtestの一覧（ADR-t1920-1）に載せる: 落ちていないtestをCIの見張りの一覧に混ぜ、修正taskの見分けを狂わせる。
- 着地の上限の時間を延ばす: 着地の数の上限が下がったまま戻らない。
- 一覧を置かずにscriptの中に名前を書く: 理由と戻す先を持てず、外したことが残り続けやすい。

## Consequences

- 着地の検証のunitの段の時間が、外した6本の分だけ短くなる。
- 外したtestが確かめるstoreの契約・台帳の約束の壊れは、着地では見えず、mainのCIで初めて見える。CIの見張りがfindingにし、修正taskで直す。
- 一覧が空になれば、unitの段はADR-t1925-1決定1のとおり全件に戻る（仕組みは残してよい）。
