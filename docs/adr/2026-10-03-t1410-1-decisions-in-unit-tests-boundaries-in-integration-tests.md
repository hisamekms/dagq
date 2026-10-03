---
id: adr-t1410-1
type: adr
title: 状態の判断はsrcの副作用のない関数のunit testで確かめ、tests/itのintegration testとe2eは境界の確認に絞る
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
owners:
  - hisamekms
tags:
  - testing
  - performance
related:
  - adr-0076
  - adr-0078
  - adr-t920-1
  - adr-t598-1
  - plan-slow-test-waits
  - design-slow-tests
---

# ADR-t1410-1: 状態の判断はsrcの副作用のない関数のunit testで確かめ、tests/itのintegration testとe2eは境界の確認に絞る

## Context

goal 68は、着地の検証のtest段とworkerの手元のtestを縮める。test段の実時間はおおむね「testの時間の合計 ÷ 並列数」で決まり（[遅いtestの待ちの内訳](../plans/slow-test-waits.md)）、最長のtestは律速していない。縮めるには合計を減らすしかない。

2026-10-02に、goal 68の待ちを縮める修正（F1〜F5・G1〜G4）が着地した後の本番のcoverageの関門のlogを見た（6並列）。testの時間の合計の大半はtests/itのintegration testで、unit testはその数%しか使っていない。重いのはruntimeのresume・integrate・reviewなどの群だった。integration testの1本は、fixture・in-processのsupervisor・stubのsession・gitのrebaseで1〜5秒かかる。状態の判断（resumeを数えるか・skipするか、pushの結果、verdictやanswerから操作への対応、askの文面）だけを確かめるtestも、caseごとにparked_conflict・awaiting_runのような状態のfixtureとsupervisorを起動し直して、その秒を払っている。

待ち（閾値・秒の境界・fixture）を縮める修正は効いたが、残りの大半は1本ごとの起動と処理で、待ちを縮め続けても減らない。判断を確かめるのに境界を毎回通す形をやめないと、合計は減らない。

閾値や設定の値・関数名・test名・flagはこのADRに書かない（上の秒と並列数は決定の理由になった観測で、決定の値ではない。[ADR-t598-1](2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)決定2・3）。具体はAGENTS.mdの「テストの制約」・`docs/development/testing.md`とgoal 68のtaskが持つ。

## Decision

1. **状態の判断はsrcの副作用のない関数にし、`#[cfg(test)]`のunit testで確かめる。** 状態の判断とは、状態の遷移、回数と上限、時刻を値で受けた時間の判定、verdictやanswerから操作への対応、askやerrorの文面、次の一手の選び方を指す。判断の入力は値で受け、結果を値で返す。
2. **tests/itのintegration testは境界を確かめる少数に絞る。** 境界とは、SQLite（schema・transaction・migration）、Git（rebase・push・worktree）、プロセス（起動・終了・signal・stubのsession）、supervisorの配線（判断の結果がevent・ask・statusに届くこと）、復旧とadopt、cmux（テスト用のworkspaceと実cmux）を指す。
3. **e2eは実バイナリ・実Git・実cmuxのハッピーパスと境界だけにする。** 判断のcaseをe2eで回さない。
4. **同じ境界で判断のcaseを回すときは、caseごとにfixtureとsupervisorを起動し直さない。** caseはunit testにし、境界は代表の1 caseのintegration testで確かめる。
5. **unit testは外部プロセス・git・SQLiteのファイル・sleep・実時間の時計を使わない。** 時刻は値で渡す。使う必要があるものは境界なので、integration testで確かめる。
6. **integration testを減らすときは、確かめていた中身を行き先に対応づける。** 行き先はunit testか、残すintegration testのどちらか。行き先の無いまま消さず、testが確かめる中身を弱めない。
7. **変えないもの。** coverageの関門と、integrateが全部のtestを流すこと、1つのtest binary（[ADR-0078](0078-one-integration-test-binary.md)）、関門をnextestで流すこと（[ADR-0076](0076-run-the-coverage-gate-tests-with-nextest.md)）、workerのstressと定時の重い繰り返し（[ADR-t920-1](2026-09-28-t920-1-light-worker-stress-and-heavy-repetition-in-scheduled-ci.md)）は変えない。productionのtimeoutと閾値の値と意味も変えない。

## Alternatives

- **全部をintegration testのまま、待ちだけを縮め続ける（goal 68のF・Gの延長）**: 今のtestの形を保てるが、待ちを縮めた後に残る1本ごとのfixture・supervisor・stubのsession・gitの起動と処理は減らない。判断のcaseが増えるたびに境界の秒を払い続け、合計は増え続ける。待ちを短くしすぎると負荷の下でflakyも増える。
- **pathによってintegrateで流すtestを選ぶ**: 変更に関係するtestだけを流せば速いが、関係の判定を誤ると壊れたまま着地し、coverageの関門の意味も変わる。goal 68で採らないと決めた。
- **判断をunit testに移すだけで、integration testをそのまま残す**: 速くならない。移した判断の分のintegration testを、境界の代表に減らして初めて合計が減る。

## Consequences

- 判断のcaseを足しても、testの時間の合計がほとんど増えない。境界のintegration testの本数は境界の数に比例する。
- 判断を関数に切り出すため、srcのruntimeのコードに副作用のない関数の層が増える。その関数がsupervisorから正しく呼ばれることは、境界の代表のintegration testが確かめる（決定2の配線）。
- 移し替えはgoal 68の重い群のtaskが行い、移した・減らしたtestの行き先の対応づけと、外部プロセスの起動・固定の待ち・fixtureの実時間が減ったことの数をそのtaskが示す。前後の比較はgoal 68の測定のtaskが`docs/plans/`に書く。
- runtimeのtaskのworkerとplannerは、新しく書くtestもこの方針に従う。規則はAGENTS.mdの「テストの制約」が持つ。
