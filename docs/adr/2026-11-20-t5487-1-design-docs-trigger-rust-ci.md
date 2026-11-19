---
id: adr-t5487-1
type: adr
title: docs/designの差分でCIのRustのjobを流す
status: accepted
created: 2026-11-20
updated: 2026-11-20
accepted_on: 2026-11-20
amends:
  - adr-t2034-1 decision 1
owners:
  - hisamekms
tags:
  - ci
related:
  - adr-t2034-1
  - adr-t2134-1
  - adr-t1925-1
---

# ADR-t5487-1: docs/designの差分でCIのRustのjobを流す

## Context

libのunit testが`docs/design/`の文書を読み、生成した集約とBCの一覧の鮮度と、識別子や参照先を検査する。
文書だけが変わるpushでRustのjobを飛ばすと、次にRustのjobが流れるまで違反を見つけられない。
[ADR-t1925-1](2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)はmainのCIを着地の最終関門とする。

## Decision

[ADR-t2034-1](2026-10-07-t2034-1-skip-rust-ci-jobs-on-docs-only-changes-and-do-not-read-skipped-runs-as-green.md)決定1のpathの範囲を改め、docsだけの判定から`docs/design/**`を外す。
`plugins/**`を範囲から外すのと同じく、`src/`のtestがその文書を読むためである。
将来`src/`のtestが`docs/`以下の別の文書を読むようにする変更では、同じ変更でそのpathをdocsだけの判定から外す。
ADR-t2034-1決定2〜8と、baseの取り方を定める[ADR-t2134-1](2026-10-20-t2134-1-push-docs-only-base-is-the-latest-run-whose-rust-checks-ran.md)はそのまま効く。
この範囲でdocsだけと判定する差分は決定7の前提どおりRustのbuildとtestの入力を変えない。

## Alternatives

designの差分だけで流すjobを足す案では、平常の実行でも飛ぶjobが増えるため、決定6の規則のままではCIが通っても失敗のissueが閉じないことがある。

## Consequences

`docs/design/`の文書だけを変えたpushでもRustのjobが流れ、文書を入力にするtestが着地後のCIで働く。
その分CIの実行時間は伸びる。
