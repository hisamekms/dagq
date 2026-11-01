---
id: adr-t2895-1
type: adr
title: src/infrastructureのSQLiteのadapterのtestは、adapter自身を確かめるためにtestごとのtempdirのファイルか名前つきのmemdbでSQLiteをその場で開いてよい（ADR-t1410-1決定5をamends）
status: accepted
created: 2026-11-01
updated: 2026-11-01
accepted_on: 2026-11-01
amends:
  - adr-t1410-1 decision 5
owners:
  - hisamekms
tags:
  - testing
related:
  - adr-t1410-1
  - adr-t3841-1
  - adr-t3802-1
  - adr-t1707-1
  - adr-t1925-1
  - development-testing
---

# ADR-t2895-1: src/infrastructureのSQLiteのadapterのtestは、adapter自身を確かめるためにtestごとのtempdirのファイルか名前つきのmemdbでSQLiteをその場で開いてよい（ADR-t1410-1決定5をamends）

## Context

[ADR-t1410-1](2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)決定5は「unit testは外部プロセス・git・SQLiteのファイル・sleep・実時間の時計を使わない」とだけ書く。
一方、`src/infrastructure`の`#[cfg(test)]`には、testごとのtempdirにSQLiteを開いてadapterのSQL・schema・transaction・index・SQLの関数を確かめるtestが多い。これらはfakeでは確かめられず、決定5のどこにも当たらない。
規則が触れないので、runのreviewのtest-rulesが新しいadapterのtestのたびにconcernを出し、人の着地の承認のaskになった。

## Decision

決定5のunit testの規則に、次の例外と範囲を足す。決定5の他の部分と、ADR-t1410-1の他の決定、[ADR-t3841-1](2026-11-01-t3841-1-supervisor-wiring-tests-run-in-process-on-a-memdb-queue-with-port-fakes.md)の決定は変えない。置き場のpathの形・URIの形・関門のscriptの名前は`docs/development/testing.md`の「判断と境界のtest」が持つ。

1. **例外**: `src/infrastructure`の`#[cfg(test)]`で、SQLiteのadapter自身（SQL・schema・transaction・index・SQLの関数）を確かめるtestは、SQLiteをその場で開いてよい。開くのはtestごとのtempdirのファイルか、testごとに別の名前のmemdbにする。接続ごとに別のDBになるメモリのSQLiteは、複数の接続を開くadapterでは使わない。
2. **種類**: このtestはadapterの境界のtestで、libのtestとして着地のunit test全件とCIで流れる。tests/itではないので、境界の登録簿・moduleの本数のratchet（[ADR-t3802-1](2026-10-31-t3802-1-integration-test-boundaries-in-a-named-registry-with-a-module-ratchet.md)）と時間の関門（[ADR-t1707-1](2026-10-05-t1707-1-time-gate-for-added-or-changed-integration-tests.md)）の対象にならない。
3. **例外の外は従来どおり**: domain・applicationの`#[cfg(test)]`はSQLiteのファイル・git・外部プロセスを使わない。判断のcaseはdomain・applicationの副作用の無い関数のunit testに置き、adapterのtestに並べない。supervisorの配線はADR-t3841-1のプロセスの中の配線のtestの層に置く。外部プロセス・sleep・実時間の時計を使わないことはadapterのtestにも当たる。
4. **扱わないもの**: gitのadapterのtestは、このADRでは決めない。

## Alternatives

- **adapterのtestをtests/itへ移す**: 数十か所のadapterのtestがITになり、ADR-t1410-1のITを境界の代表に絞る方針、ITの登録簿と本数の予算、着地の検証をunit全件と絞ったITにすること（[ADR-t1925-1](2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)）に逆らう。
- **プロセスの中の配線のtestの層に寄せる**: その層はsuperviseの1周を回す配線の層で、adapterのSQLの確認は配線ではない。
- **規則を変えず、reviewのconcernを人が都度承認する**: 同じ形のtestのたびに人のaskが出て、規則がreviewと人の判断を食い違わせたままになる。

## Consequences

- SQLiteのadapterのtestは今の置き場のまま規則に適い、test-rulesのreviewは同じ形のtestをunit testの規則で差し戻さずconcernにもしない。
- 機械の検査（testの行の形を見るscript）はSQLiteのファイルを見ないままで、例外の当てはめ（adapter自身を確かめているか、判断のcaseを並べていないか、開き方）はtest-rulesのreviewが見る。
- 既存のtestは動かさない。
