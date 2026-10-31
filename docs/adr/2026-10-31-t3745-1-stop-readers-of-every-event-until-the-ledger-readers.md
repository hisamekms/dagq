---
id: adr-t3745-1
type: adr
title: queueの全eventを読む読み手を台帳の読み手が入るまで止めて断り、段2の並走比較を取り下げ、止めた間のintegration testを#[cfg(any())]で外す
status: accepted
created: 2026-10-31
updated: 2026-10-31
accepted_on: 2026-10-31
owners:
  - hisamekms
tags:
  - runtime
  - observability
  - stats
  - testing
related:
  - adr-t1662-1
  - adr-0051
  - adr-t1582-1
  - design-measurement
  - design-queue-service-host-cli
---

# ADR-t3745-1: queueの全eventを読む読み手を台帳の読み手が入るまで止めて断り、段2の並走比較を取り下げ、止めた間のintegration testを#[cfg(any())]で外す

## Context

queue service（`service serve`）のメモリが13 GBまで膨らみ、hostのswapを使い切りかけ、`status`などが30秒で時間切れになった。
主因は、queueの全event（約109万件）をJSONの木としてメモリに載せる読み（`EventStore::all_events`）を、serviceの中で毎回・並んで行うことだった。
clientが諦めても計算は続き、やり直しで重なる。
全eventを読む本番の入口は`stats`（`--ledger`を含む）・`kpi`（`--compare`を含む）・`report`（`report_inputs`）・`stats_inputs`（observerのjobの入力を含む）・`forecast`・`marks`・`mark`（`--retract`を含む）と、supervisorのuse caseの`all_events`だった。

[ADR-t1662-1](2026-10-04-t1662-1-runs-are-covering-phase-intervals-folded-into-a-ledger.md)は統計を台帳から読むと決め、そのConsequencesは段2で今のstatsと台帳を1週間並走で比べてから段3で読み手を移すとした。
並走の比較は全eventの読みを続けることを前提にしている。

人は2026-10-30にrequest 190で、全eventの読みを即時に止め、台帳が入るまで該当の機能が止まることを認め、段2の関門を取り下げると決めた。

## Decision

1. **全eventを読む読み手を、台帳の読み手が入るまで止め、止めた間は読む前に断る。**
   止めるのは入口（composeの配線、queue serviceの操作、CLIの依頼の前）で、断りは止めた理由と戻すものを名指す誤りにする。
   断りの文と`--help`は、dagqが外部のrepositoryで動くのでdagq自身のtask番号とADRを引かず、戻すもの（台帳の読み手）を平文で名指し、戻すtaskの番号は定義のそばのcommentとこのADRが持つ。
   入口の後ろの集計の関数（domainとapplication）とそのunit testは、台帳の読み手が今の値と同じ答えを出すことを確かめる基準として残す。
   全eventを読む配線は、全eventを読んでいた所で断りを読む形で残し、台帳の読み手が差し替える。
   `timeline`と`status`はrunごとのeventだけを読むので止めない。
   [ADR-0051](0051-kpi-time-series-report-and-push.md)のKPI・report・pushと、ADR-t1662-1の段3の読み手の決定は変えない。
   それらは台帳の読み手の着地で台帳から戻る。
   止める範囲は全eventの読みで、projectの設定の値ではないので、外部のprojectでも同じに止まる。
2. **ADR-t1662-1のConsequencesの段2の1週間の並走比較と、その関門を取り下げる。**
   段3は並走の確認なしに進める。
   失うもの: 台帳の値の正しさを並走で確かめず、切り替えた後は比べる手段が無い。
   ADR-t1662-1の決定1〜16は保つ。
3. **止めた間、止めた入口の値を確かめるintegration testは消さず、本文を残して`#[cfg(any())]`で外し、戻すtaskのIDをcommentに書く。**
   戻すtaskが同じ変更でcfgを外す。
   外したtestだけが使うhelperにも同じcfgとcommentを付け、crate全体の`dead_code`の許可は付けない。
   [ADR-t1582-1](2026-10-04-t1582-1-temporarily-leave-broker-and-cmux-only-e2e-cases-out.md)のe2eの外し方をintegration testに広げるもので、このgoal（全eventの読み手の停止と戻し）の間に限る。
   止めた入口ごとに、断ることを確かめるtestを足す。

## Alternatives

- **設定のkeyで止める（既定で止め、keyで戻せる）**: 人の言葉は即時の廃止で、戻せるkeyは同じメモリの読みを外部のprojectにも残す。
  止めるのは仕組みでprojectの値ではない。
- **全eventの読みを残してページで読む**: serviceの答えの大きさは抑えられるが、clientか読み手の側で全eventを木に載せる重さは変わらず、並んだ読みとやり直しの重なりも残る。
- **集計の関数とunit testも消す**: 台帳の読み手が今の値と同じJSONを出すことを確かめる基準を失う。
- **止めた入口のintegration testを消す**: 戻すtaskが値の確かめを書き直すことになり、外す間の値の基準も失う。

## Consequences

- 戻るまで、throughput reviewの数値・KPIと目標割れ・日次レポート・goalの見通し・observerの入力の一部・印の前後比較が使えない。
- supervisorのpass（日次レポート・pushの目標割れ・`forecast_recorded`）が全eventを読まずに止まることは、同じgoalの別のtaskが持つ。
- observerは`stats`の断りを入力の`error`にして観察を続け、毎時のwindowを最新のeventで閉じるので、止めている間に終わったrunは`stats`が戻った後のobservationも読まない。
- 外したintegration testの間は、止めた入口の値をintegration testで確かめない。
  値の基準は残したunit testが持つ。
- 今の止め方と戻す先は[計測](../design/measurement.md)の「並走の比較と照合」、止めた入口のdoc commentと`--help`が持つ。
