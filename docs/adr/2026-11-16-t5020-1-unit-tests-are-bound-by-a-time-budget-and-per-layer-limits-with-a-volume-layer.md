---
id: adr-t5020-1
type: adr
title: unit testを本数でなく着地の検証のunitの段の時間の予算（壁時計5分・6並列でCPU合計30分）で縛り、1本の上限を層ごとにし、量で約束を確かめるtestを量の層へ移し、SQLiteのtestを性質ごとの軽い形にし、schemaの用意の代金は判定の基準だけを決める（ADR-t4948-1決定2とADR-t4997-1決定3をamends）
status: accepted
created: 2026-11-16
updated: 2026-11-16
accepted_on: 2026-11-16
amends:
  - adr-t4948-1 decision 2
  - adr-t4997-1 decision 3
owners:
  - hisamekms
tags:
  - testing
  - performance
  - integrate
related:
  - adr-t4948-1
  - adr-t4997-1
  - adr-t4979-1
  - adr-t1925-1
  - adr-t1707-1
  - adr-t1410-1
  - adr-t2895-1
  - adr-t3841-1
  - development-testing
---

# ADR-t5020-1: unit testを時間の予算と層ごとの1本の上限で縛り、量のtestを別の層に置く

## Context

人は、リポジトリの規模とコードの性質からunit testの目標の本数か時間を決め、対症療法でなく狙う先を持ちたいと問い、inboxと相談した方針を「この方針で」「推奨どおり」と決めた（request 326、goal 297）。

測った事実（task 4981の着地の検証のrun fb5dc8e1のunitの段、`NEXTEST_TEST_THREADS=6`、他のrunと同時の負荷の下）:

- srcは約72万行・831 file、unit testは約5,800本（domain 2,459・application 1,777・infrastructure 1,165・compose 198）。
- CPU時間の合計は8,392秒で、6並列で割ると1,399秒になり、実測の1,440秒とほぼ等しい。段の時間は遅い1本（尻尾）ではなく合計が決める。
- 1本のp50は0.027秒・p90は0.34秒・p99は49.5秒。1秒を超える460本（8%）がCPU時間の97%を、5秒を超える204本が89%を、60秒を超える36本が46%を占める。
- moduleごとでは`infrastructure::sqlite`が4,361秒（118本、52%）、`application::ports`が1,147秒、`application::supervise`が988秒（738本）、`infrastructure::runtime_store`が610秒、`compose::wiring_tests`が605秒で、domainはわずか。
- 重いtestの多くは、範囲の外の履歴を150〜300単位積む・本番の規模で作る・storeの契約の全caseを同じDBで通すなど、量で約束を確かめる形で、1本ごとにmemdbへ`SqliteQueue::init`でmigration約120本を当てる。

[ADR-t4948-1](2026-11-16-t4948-1-time-gate-for-added-or-changed-lib-tests-and-the-lib-budget.md)は差分で足した・変えた`--lib`のtestを1本5秒の閾値で落とし（決定2）、`--lib`の予算を毎晩mainで見る（決定6）。
[ADR-t4997-1](2026-11-16-t4997-1-landing-unit-stage-leaves-out-path-groups-the-diff-misses-and-ci-runs-them.md)は着地の検証のunitの段から、差分が組のpathに当たらない組を外し（決定1）、組に足すのをgoal 294のtaskと人の決定だけにした（決定3）。

amendsにするのは、変わるのがADR-t4948-1の閾値（決定2）とADR-t4997-1の組に足してよい者（決定3）だけで、両ADRの他の決定（対象・許可の一覧と載せてよい理由・既存の遅いtestを載せないこと・関門の場所・夜の予算・限界、組の外し方・CIが組を流すこと）は保つからである（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。
[ADR-t4979-1](2026-11-15-t4979-1-landing-unit-stage-leaves-out-listed-heavy-unit-tests-and-ci-runs-them.md)、CIを最終の関門とする[ADR-t1925-1](2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)、`tests/it`の関門の[ADR-t1707-1](2026-10-05-t1707-1-time-gate-for-added-or-changed-integration-tests.md)は変えない。

## Decision

1. **本数の目標は置かず、時間の予算で縛る。** 予算は着地の検証のunitの段の壁時計5分以内、`NEXTEST_TEST_THREADS=6`でCPU時間の合計30分（1,800秒）以内とする。
   - 根拠: Contextのとおり段の時間はCPU合計を並列度で割った値で決まり、合計の97%は1秒を超える8%の本数が持つ。本数を減らしても1秒未満の本数は合計をほとんど動かさず、目標にすると軽いunit testを書かない方へ働く。
   - 予算が数えるのはunitの層（決定3の量の層を除いた`--lib`のtest）で、比べる秒は本番の着地の検証のログ（nextestのSummaryの秒と、1本ごとの秒の和）の負荷の下の秒とする。量の層を流したrunは、量の層の分を除いて比べる。
   - 予算は着地を止める関門にしない（ADR-t4948-1決定6とAlternativesの「予算で着地を止める」と同じ理由）。予算へ近づける手段は、決定2の1本の上限・決定3の量の層・決定4の軽い形・決定5の判定に従う直しである。
   - ADR-t4948-1決定6の夜の予算との関係: 夜の天井はCIのrunnerで`--lib`を測り、今の値を超えないための天井で、この予算そのものではない。天井は直しが着地するごとに今の値からこの予算の方へ下げていく。夜の天井を超えたfindingは増え方を拾い、この予算との差は人かinboxが着地の検証のログで見る。

2. **1本の上限を層ごとにし、それを`--lib`の時間の関門の閾値にする（ADR-t4948-1決定2をamends）。**
   - 上限: domain（副作用の無い判断）50 ms、application（fakeを使う）200 ms、infrastructureとcompose（実のSQLite・配線のtest）1秒。
   - 層はnextestのtestの名前の先頭のmoduleで決める: `domain::`・`application::`・`infrastructure::`・`compose::`。どれでもないもの（binのtestを含む）はinfrastructureと同じ1秒にする。
   - 値はADR-t4948-1決定2と同じく本番の着地の検証のunitの段のログの負荷の下の1本の秒と比べ、等しいものは超えない。
   - ADR-t4948-1の対象（決定1）・許可の一覧と載せてよい理由（決定3）・既存の遅いtestを載せないこと（決定4）・関門の場所（決定5）・限界（決定7）は保つ。上限を超えたtestは、直すか、決定3の理由（守る境界か、縮める・移す予定のtaskかgoal）を書いた項目を足す。
   - 量の層のtestも層は先頭のmoduleで決まり、関門の対象である。ADR-t4948-1決定3の理由は変えず、その読み方だけを定める: 量の層のtestは同じ約束の小さい版をunitの層に持つときだけ「件数を大きくしただけのtest」に当たらず、項目は(a)の理由（実物のSQLiteで確かめる約束）に、unitの層の小さい版のtestの名前を添える。小さい版の無い量のtestは「件数を大きくしただけのtest」に当たり、載せられない。
   - 根拠: 5秒はContextの分布で合計の89%を見逃す側にあり、層の性質（副作用の無い判断は速い、fakeは中くらい、実のSQLiteと配線は重い）ごとの上限なら、層を取り違えたtest（判断のcaseをSQLiteで並べたものなど）を早く捕まえられる。

3. **量で約束を確かめるtestは、unitの層の外の「量の層」に置く。**
   - 当たるもの: 範囲の外の履歴を150〜300単位積むtest、本番の規模で作るtest、storeの契約の全caseを同じDBで通すtest。
   - 印: 量の層のtestは、名前でfiltersetが拾えるように`mod volume`の中に置く（testの全名に`::volume::`が入る）。
   - 小さい版: 同じ約束の小さい版（決定4の形）をunitの層に残す。量の層へ移すことで確かめる約束を弱めない。
   - 流れる場所: 着地の検証のunitの段では、goal 294のpathの組（[ADR-t4997-1](2026-11-16-t4997-1-landing-unit-stage-leaves-out-path-groups-the-diff-misses-and-ci-runs-them.md)、`.config/landing-unit-path-groups.toml`）で、差分が組のpathに当たるrunだけで流す。CIのmacOS・Linuxのjobは毎回全部流す（ADR-t1925-1決定5のCIを最終の関門とする決定、ADR-t4997-1決定2を保つ）。
   - 組の規則: unitの層の版の組への出入りは、goal 294の組のfilterset（moduleのpathと`store_contract_tests`の名前）のまま変えない。goal 297は組を狭めも広げもしない。量の層は`mod volume`のtestを組に足すだけとする。
   - ADR-t4997-1決定3の「組に足すのはgoal 294のtaskと人の決定だけ」を、量の層の印（`mod volume`）を持つtestについてamendsし、そのtestを組のfiltersetに足すことをgoal 297のtaskにも許す。組のpath・組そのものを足すこと、量の層の印を持たないtestを足すことは今どおりgoal 294のtaskと人の決定だけにする。

4. **SQLiteのtestは確かめる性質ごとに軽い形にする。**
   - 範囲の読みと全件の読みの等価: 小さな履歴（数〜数十単位）と境目（範囲の端・時計の戻り・古い書き手）で比べる。
   - 読む量が範囲の外の履歴に比例しないこと: 読んだ行数（読みのportの行数）か、`EXPLAIN QUERY PLAN`の索引の範囲検索（`SEARCH ... USING INDEX`と範囲の条件）で確かめ、履歴は10単位と20単位でよい。前例は`src/infrastructure/runtime_store/ledger/queue_from_tests.rs`の`assert_seeks`。
   - 同じDBで順に流す干渉の検査: unitの層では小さい件数で通し、全caseの通しは決定3の量の層に置く。
   - 根拠: 比例しないことは量を積まずに読みの形で確かめられ、等価と干渉は境目と組み合わせが確かめるので、件数を大きくしても約束は強くならず時間だけが増える。

5. **schemaの用意の代金は、判定の基準だけをここで決め、形はSpikeの後のtaskが書く。**
   - 対象: 1本ごとにmemdbへ`SqliteQueue::init`でmigration約120本を当てる用意。
   - 判定の基準: 1本の内訳でschemaの用意が占める割合と、unitの段のCPU合計の削減の見込みで決め、削減の見込みがunitの段のCPU合計の5%以上なら入れる（同じgoalのSpike 5021の判定の値と同じ）。
   - 入れるときの形の候補: 最新schemaのDBを1回作り、SQLiteのbackupで1本ごとに写す（プロセスの中の配線のtestの層が既に使う形、[ADR-t3841-1](2026-11-01-t3841-1-supervisor-wiring-tests-run-in-process-on-a-memdb-queue-with-port-fakes.md)）。形の決定と実装はSpikeの判定の後のtaskが持つ。
   - Spikeの結果をこのADRが待つとgoalが直列に延びるので、基準だけを先に決める。

## Alternatives

- **本数の目標を置く（例: 層ごとに何本まで）**: 合計を決めるのは1秒を超える8%で、本数は段の時間をほとんど動かさない。軽いunit testを書かない方へ働くので採らない。
- **予算で着地を止める**: 既存のtestの合計で関係の無いrunの着地が止まる。ADR-t4948-1と同じく採らない。
- **閾値を一律の5秒のままにする**: 1〜5秒のtestは合計の8%ほどを持ち、層を取り違えたtestを見逃す。層ごとの上限にする。
- **量のtestを消す**: 範囲の外の履歴に比例しないことや全caseの通しの約束が、CIからも消える。量の層に置いてCIで毎回流す。
- **量のtestを`tests/it`へ移す**: `tests/it`はgoal 293・239が持ち、境界の登録簿と時間の関門の対象になる。量のtestは境界ではなく量の約束なので、libのtestのまま別の層にする。
- **量の層に別の組を作る**: 組を足すのはgoal 294と人の決定に残し、量の層は既存の組へ`mod volume`のtestを足すだけにする。unitの層の版の組への当たり方が組ごとに食い違わないため。

## Consequences

- 重いunit testを足す・変えるrunは、層の上限を超えると着地の前に名前と秒を返され、直すか理由を書いた項目を足す。domainの判断のtestは50 msを超えると捕まる。
- プロセスの中の配線のtest（ADR-t3841-1、Spikeで1本1.6〜3.0秒）は、足す・変えるとcomposeの1秒を超えやすい。待ちの間隔の口で縮めるか1本の配線を分け、分けられないものは決定3の(a)（守る配線の境界）を書いた項目を足す。
- 量のtestは差分が組に当たるrunとCIだけで流れ、組に当たらないrunの着地の検証では流れない。組のpathの外の差分で量のtestが壊れたときはmainのCIで初めて見える（ADR-t4997-1のConsequencesと同じ）。
- SQLiteのtestは等価を小さな履歴と境目で、比例しないことを行数か`EXPLAIN QUERY PLAN`で確かめる形になり、件数を大きくするtestは量の層にだけ現れる。
- 予算と今の値の差（CPU合計8,392秒と1,800秒）は、この方針の直しが着地した後に人かinboxが本番の着地の検証のログで見て記録する。
- 規則の本文は[testの制約](../development/testing.md)の「判断と境界のtest」と「unitの段の時間の予算と量の層」が持つ。関門のscript（`scripts/check-lib-test-time.sh`）の閾値を層ごとにすることと、量の層の`mod volume`のtestを組に足すことは、同じgoalの後続のtaskが行う。
