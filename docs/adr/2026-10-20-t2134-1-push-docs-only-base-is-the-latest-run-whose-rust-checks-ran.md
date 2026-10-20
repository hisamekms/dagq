---
id: adr-t2134-1
type: adr
title: mainへのpushのdocsだけの判定のbaseは、pushのbeforeではなく、このworkflowのmainの実行でRustのjobが流れて終わった最新の実行のcommitにする（ADR-t2034-1決定1をamends）
status: accepted
created: 2026-10-20
updated: 2026-10-20
accepted_on: 2026-10-20
amends:
  - adr-t2034-1 decision 1
owners:
  - hisamekms
tags:
  - ci
related:
  - adr-t2034-1
  - adr-t1925-1
---

# ADR-t2134-1: pushのdocsだけの判定のbaseを、Rustのjobが流れた最新の実行のcommitにする（ADR-t2034-1決定1をamends）

## Context

[ADR-t2034-1](2026-10-07-t2034-1-skip-rust-ci-jobs-on-docs-only-changes-and-do-not-read-skipped-runs-as-green.md)決定1は、pushのdocsだけの判定のbaseをpushの`before`（直前のcommit）にした。
`ci.yml`のmainの`concurrency`は`cancel-in-progress`がfalseでも、GitHubは同じgroupでpendingの実行を新しい実行が来たときに取り消す。
pendingの実行が取り消されたcommitは自分の実行でRustのjobを流さず、次の実行は取り消されたcommitとの差分だけを見るので、その差分がdocsだけならRustのjobを飛ばす。
こうしてcodeを変えたcommitが、どの実行でもtestとcoverageの関門を通らずにmainに残る。
2026-10-08にff5006b0（`src/`と`tests/it`を変えた）の後のbdec1841とd903bb5eの実行が取り消され、41dfa2d4の実行がd903bb5eとの差分をdocsだけと判定してRustのjobを飛ばした。
CIを着地の最終関門にした[ADR-t1925-1](2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)では、この穴は関門の抜けになる。

## Decision

1. **pushのbase**: mainへのpushのbaseは、このworkflowのmainへのpushの終わった実行のうち、Rustのjob（`checks`）の`conclusion`が`success`か`failure`の最新の実行のhead commitとする（飛ばされた・取り消された・成否の決まらない終わり方の実行は読み飛ばす）。`failure`も含めるのは、そのcommitまでのcodeはRustのjobが流れて結果が見張りに届いており、赤の後をdocsだけの変更で緑と読むのはADR-t2034-1決定4・6が防ぐため。
2. **読み方と安全側**: 判定のjobはGITHUB_TOKEN（`actions: read`）でGitHubのAPIから実行とjobを読み、外部のactionを足さない。見つけたcommitは実行のcommitの祖先でなければ使わない（force push）。APIを読めない・該当の実行が見つからない・祖先でないときは、ADR-t2034-1決定1の残りと同じくdocsだけとせず全部流す。jobのlogに使ったbaseとその出どころ（どの実行か）を出す。
3. **変えないもの**: pull requestのbase、docsだけのpathの範囲、差分が空のときと判定のjobが落ちたときに全部流すこと、ADR-t2034-1の他の決定。`concurrency`は変えない（ADR-t2034-1決定8）。

## Alternatives

- **`concurrency`でmainの実行を取り消さない・並べる**: GitHubのconcurrencyはpendingを1つしか持てず、groupを分けるとRustのjobが重なって待ちが増える。取り消し自体は「最新のcommitまで流せばよい」ので害が無く、穴はbaseの取り方だけにある。
- **beforeのまま、取り消された実行の分を後から流すjobを足す**: 実行と待ちが増え、取り消しを見張る仕組みが要る。
- **見つからないときに`before`へ戻る**: 戻った先が取り消された実行のcommitなら同じ穴が残る。

## Consequences

- 判定のjobはpushの実行ごとにAPIを数回呼ぶ（docsだけの実行が続いた分だけjobの一覧を読む）。読む範囲を超えて見つからなければ全部流す。
- 判定の書き方は`ci.yml`の`changes`のjobの冒頭のコメントが持つ。Rustのjobの`name:`を変えるときは、`dagq.toml`の`required_jobs`と同じく判定のjobも同じ変更で直す。
