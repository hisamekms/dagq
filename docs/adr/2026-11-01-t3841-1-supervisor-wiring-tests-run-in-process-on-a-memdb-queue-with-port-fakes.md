---
id: adr-t3841-1
type: adr
title: supervisorの配線は、memdbのSQLiteのqueueとportのfakeで本物のsuperviseをプロセスの中で回すtestの層で確かめ、unit testともITとも別の種類にする（ADR-t1410-1決定2・6をamends）
status: accepted
created: 2026-11-01
updated: 2026-11-01
accepted_on: 2026-11-01
amends:
  - adr-t1410-1 decision 2
  - adr-t1410-1 decision 6
owners:
  - hisamekms
tags:
  - testing
  - performance
related:
  - adr-t1410-1
  - adr-t3802-1
  - adr-t1707-1
  - plan-in-process-supervisor-test-spike
  - plan-it-reduction
  - development-testing
  - development-task-registration
---

# ADR-t3841-1: supervisorの配線は、memdbのSQLiteのqueueとportのfakeで本物のsuperviseをプロセスの中で回すtestの層で確かめ、unit testともITとも別の種類にする（ADR-t1410-1決定2・6をamends）

## Context

[ADR-t1410-1](2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)決定2は、supervisorの配線（判断の結果がevent・ask・statusに届くこと）をtests/itの境界に数える。
時間の関門の許可の一覧の約380件が理由に「supervisorの配線」を挙げ、配線のITは1本ごとにfixture・実プロセスのsession・実Git・SQLiteのファイルの秒を払う（goal 239）。

[Spike](../plans/in-process-supervisor-test-spike.md)は、本物の`SqliteQueue`をSQLiteのmemdbで開き他のportをfakeにする形で、claim→workerのsession→receipt→検証→review→askの配線が外部プロセス・実Git・SQLiteのファイルなしにsrcの`#[cfg(test)]`で回り、ITと同じeventとaskが出ることを確かめた（同じ前半でITの2〜3倍速い）。
手書きの`Queue`のfakeはqueueの再実装になるので採れなかった。
この層はproductionのloop・heartbeat・wrapperを実時間で回しthreadを起こすので、決定5のunit testには当たらない。

## Decision

決定2のうち「supervisorの配線」をtests/itの境界に置く部分と、決定6の行き先を変える。決定1・3〜5・7は保つ（決定5のunit testの規則は変えず、この層はunit testとは別の種類にする）。SQLite（schema・transaction・migration、ファイルのlockとプロセスの競合）・Git・プロセス・復旧とadoptの境界はtests/itに残る。置き場の名前・URIの形・時間の目安の値・関門のscriptの名前は`docs/development/testing.md`の「判断と境界のtest」が持つ。

1. **層の置き場と形**: 配線を確かめるtestは、applicationとinfrastructureの両方が見えるcomposition rootの`#[cfg(test)]`のmoduleに置く。本物のsuperviseと本物のSQLiteのqueue・同じプロセスで答えるqueueのserviceを使い、queueはプロセスの中で複数の接続が共有できるメモリのSQLite（memdb）で開く。DBはtestごとに別にし、processごとに1回migrationを当てた雛形を写して作る。接続ごとに別のDBになるメモリのSQLiteは、要求ごとに開き直すqueueからは空に見えるので使わない。

2. **規則**: 層のtestは外部プロセス・実Git・SQLiteのファイルを使わない。testの行にsleepと実時間の時計を置かない。productionの実時間の待ちは、上限つきの1つの期限（ITの待ちの上限に当たるもの）で包み、過ぎればtestの名前と待っていた条件を出して落ちる。起こしたthread（sessionのwrapperを含む）はtestの終わりまでにjoinする。process-globalの可変のstaticをtestの間で共有しない（並列とnextestのprocessごとの実行の両方で通るため）。

3. **portのfakeとproductionの口**: Git・session・agent・検証・プロセスの制御などのportは、生成した中身の無いstubでなく状態を持つ少数のfakeにする（Gitはcommit・branch・mainのheadを、agentは台本のturnとreviewの出力を持つ）。productionのevent・payload・ask・文面・閾値の意味は変えない。productionに足してよい口は、testから構成を渡すもの（queueのserviceのtokenの置き場、メモリのrunのファイルの公開、待ちの間隔）だけにする。

4. **流れる場所と時間**: 層のtestはlibのtestとして、着地のunit test全件とCIのcoverageで流れる。tests/itではないので、境界の登録簿・moduleの本数のratchet（ADR-t3802-1）と時間の関門の許可の一覧（ADR-t1707-1）の対象にならない。代わりに1本の時間の目安を持ち、Spikeの測定から決める（値・超えたときの扱い・測り方はtesting.md）。

5. **新しいtestの置き場と移し替え**: 配線を確かめる新しいtestはこの層に置き、tests/itの登録簿に境界の種類「supervisorの配線」のidを新しく足さない。移し替えの間、既存の「supervisorの配線」のidとそのIT・許可の一覧の項目は残す（移すtaskが中身の行き先を対応づけ、同じ変更で外す）。境界の種類「supervisorの配線」の退役（ADR-t3802-1と関門のscriptの境界の種類の値）は、移し替えの後の別のtaskがADR-t3802-1をamendsして行う。

6. **決定6の行き先（ADR-t1410-1決定6を改める）**: integration testを減らすときの行き先は、unit test・この層のtest・残すintegration testのどれかにする。確かめていた中身を行き先に対応づけること、行き先の無いまま消さないこと、testが確かめる中身を弱めないことは変えない。

## Alternatives

- **手書きの`Queue`のfake（Spikeの形(a)）**: 1本の配線がqueueに120種の操作を送り、必須のmethodは350本ある。claim・lease・遷移の意味をメモリで書き直すとproductionのqueueと食い違っても気づけない。
- **配線をITに残し、待ちを縮め続ける**: 1本ごとの実プロセス・Git・ファイルの秒は減らず、ADR-t1410-1のAlternativesの1つ目と同じ理由で採らない。
- **層をunit testとして決定5に入れる**: 層はproductionの実時間の待ちとthreadを持ち、決定5を満たさない。決定5を緩めると判断のunit testにも待ちとthreadが入り得るので、別の種類として規則を置く。
- **`:memory:`のSQLite**: 2つ目の接続から空のDBに見え、serviceの経路が通らない（Spikeで確かめた）。

## Consequences

- supervisorの配線の確かめが、外部プロセス・Git・SQLiteのファイルの秒を払わずにlibのtestで流れ、tests/itの本数と秒が配線の分だけ減る。
- testの層は3つ（unit test・プロセスの中の層・IT）になり、test-rulesのreviewは層のtestをunit testの規則でなく上の規則で判定する。規則の本文は`docs/development/testing.md`の「判断と境界のtest」、受け入れ条件の書き方は`docs/development/task-registration.md`の「受け入れ条件とITの名指し方」が持つ。
- 層のharnessと状態を持つfakeを作るtask、着地まで広げるtask、moduleごとの移し替えのtaskがこの規則を前提にする。
