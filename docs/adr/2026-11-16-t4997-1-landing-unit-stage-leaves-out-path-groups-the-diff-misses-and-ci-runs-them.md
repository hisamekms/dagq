---
id: adr-t4997-1
type: adr
title: 着地の検証のunitの段は、設定に置いたtestの組を、runの差分が組のpathに1つも当たらないときだけ外し、CIのmacOS・Linuxのjobは流し続ける（ADR-t4979-1決定1・3とADR-t1925-1決定1をamends）
status: accepted
created: 2026-11-16
updated: 2026-11-16
accepted_on: 2026-11-16
amends:
  - adr-t4979-1 decision 1
  - adr-t4979-1 decision 3
  - adr-t1925-1 decision 1
owners:
  - hisamekms
tags:
  - testing
  - integrate
  - ci
related:
  - adr-t1925-1
  - adr-t4979-1
  - adr-t2032-1
  - plan-ledger-range-test-time-spike
  - plan-landing-it-selection
  - design-supervisor-lifecycle-validation
---

# ADR-t4997-1: 着地の検証のunitの段から、差分が当たらないtestの組を外し、CIで流し続ける

## Context

[ADR-t1925-1](2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)決定1は着地の検証でunit testを全件流すと決め、[ADR-t4979-1](2026-11-15-t4979-1-landing-unit-stage-leaves-out-listed-heavy-unit-tests-and-ci-runs-them.md)決定1はそこから一覧に名指しで載ったunit testだけを外し、決定3は一覧に足すのをgoal 291のtaskと人の決定だけにした。
[Spike 4944](../plans/ledger-range-test-time-spike.md)は、台帳の読みと作りの範囲のtest（対応表の842本）が着地の検証のunitの段でEで814〜1,407秒を占め、ADR-t4979-1の6本を除いても488〜1,004秒が残ると測った（判定holds）。
人はrequest 312で、軽くした後も重ければ台帳の読みのコードを変えたrunだけで流す仕組みを考えると決めた。
plannerは、2026-11-09以降のmainの着地136本のうち差分が範囲のpathに当たったのは67本（49%）と数えた。

amendsにするのは、変わるのが着地のunitの段の範囲（ADR-t1925-1決定1・ADR-t4979-1決定1）と、外すものを足してよい者（ADR-t4979-1決定3）だけだからである（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。
ADR-t1925-1の他の決定（絞ったIT・既に落ちているtestの扱い・設定での切り替え・CIが最終関門）、それをamendsした[ADR-t2892-1](2026-10-20-t2892-1-landing-it-leaves-added-sources-and-broad-entries-to-ci.md)・[ADR-t3920-1](2026-11-01-t3920-1-landing-it-keeps-using-an-old-table-and-widens-the-files-it-does-not-know.md)・[ADR-t3926-1](2026-11-06-t3926-1-it-coverage-map-is-built-on-a-schedule-by-parallel-linux-jobs.md)の決定、ADR-t4979-1決定2（CIは一覧のtestも流す）は変わらない。

## Decision

1. **着地の検証のunitの段は、設定のファイルに置いたtestの組を、runの差分（`git diff $DAGQ_LANDING_BASE..HEAD`のfile）が組のpathに1つも当たらないときだけ流さない。差分が当たるとき、差分が取れないときは流す。**
   - 組は名前・外す理由・載せたtask・testを名指すnextestのfilterset・差分を照らすpathのglobの組を持つ。globは`[recheck]`の`paths`と同じ形にする（[ADR-t2032-1](2026-10-07-t2032-1-run-the-recheck-command-only-on-runs-touching-recheck-paths.md)）。
   - 置き場はADR-t4979-1決定1の一覧と別のファイルにする。一覧は名指しのtestを戻すまでの仮の置き場で、組は差分で外す恒常の形なので混ぜない。
   - 一覧のtest・既に落ちているtestの除外と同じ形で、unitの段のfiltersetから引く。他のunit testはADR-t4979-1決定1のとおり流す。
   - 着地の検証の出力は、unitの段の前に、外した組の名前とCIが流すこと、または流す理由（当たったpathの先頭の数件か、差分が取れないこと）を出す。
   - ファイルが無いか組が無ければ、unitの段は組が無いときと同じ。読めないファイル（知らないkey・欠け・空・名前の重複）では着地の検証を止め、黙って流すことにも外すことにもしない。
   - ITの段の選び方は組を読まない。
2. **CIのmacOS・Linuxのjobは、組のtestも毎回流す。外したrunでの壊れは、着地の後のmainのCIが最終関門として拾う（ADR-t1925-1決定5、ADR-t4979-1決定2）。**
   - 組のpathの外の差分（eventの型など）が組のtestを壊す経路は、組のpathに足さずCIに任せる。足すと`src/domain`の大半に当たって効かなくなり、request 43とADR-t4979-1は着地の後にCIで見つかる事故を許しているため。
3. **組に足す（組・test・pathを足す）のは、goal 294のtaskと人が決めたときだけにする。ADR-t4979-1決定1の一覧に足すのは、今どおりgoal 291のtaskと人の決定だけにする。**
   - どちらも日々の遅いtestの受け皿にしない。遅いunit testは時間の関門と、軽くするか分割する直しで扱う。

## Alternatives

- ADR-t4979-1の一覧の書式を広げてpathを持たせる: 名指しの6本を戻す仮の一覧と、恒常の組が混ざり、goal 291で一覧を空に戻すときに組を巻き込む。
- CIでも差分で外す: mainのpushごとのCIが全部を流すことが、外したrunの壊れを1回のCIで拾う根拠なので、それが失われる。
- eventの型などを組のpathに足す: ほとんどのrunの差分が当たり、効く量が無くなる。

## Consequences

- 差分が組のpathに当たらないrun（2026-11-09以降の着地の約半分）は、着地の検証のunitの段が組の分（Spike 4944のEで8〜17分）短くなる。
- 組のpathの外の差分で組のtestが壊れたときは、着地では見えず、mainのCIで初めて見える。CIの見張りがfindingにし、修正taskで直す。
- 組に当たるrunの着地は軽くならない。重いtestを軽くする直し（goal 291）はそのために残る。
