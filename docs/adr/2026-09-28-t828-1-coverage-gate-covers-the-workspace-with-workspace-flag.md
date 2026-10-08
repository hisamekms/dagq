---
id: adr-t828-1
type: adr
title: coverageの関門はcargo-llvm-covがdefault-membersを見ないので、関門のコマンドに--workspaceを足してbrokerのcrateも80%の行カバレッジに数える（ADR-t827-1決定3をamends）
status: superseded
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
superseded_by: adr-t2126-1
superseded_on: 2026-10-09
amends:
  - adr-t827-1 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - testing
  - broker
related:
  - adr-t827-1
  - adr-0076
  - design-broker
---

# ADR-t828-1: coverageの関門はcargo-llvm-covがdefault-membersを見ないので、関門のコマンドに--workspaceを足してbrokerのcrateも80%の行カバレッジに数える（ADR-t827-1決定3をamends）

> **置き換え済み（2026-10-09）**: このADRの決定は現在有効ではない。現行の決定は[ADR-t2126-1](2026-10-09-t2126-1-single-package-without-workspace-and-coverage-gate-keeps-workspace-flag.md)を読む。

## Context

[ADR-t827-1](2026-09-28-t827-1-broker-crates-binaries-and-version-alignment.md)の決定3は、workspaceの`default-members`に全てのcrateを入れれば、rootの`cargo test`・`cargo clippy --all-targets`・`cargo llvm-cov nextest --locked --fail-under-lines 80`が`--workspace`なしで新しいcrateも覆うとし、関門のコマンドを変えないと決めた。reportに新しいcrateが載らなければ、関門を弱めずに人に聞くとも決めていた。

task 828がworkspaceを作って確かめたところ、`cargo test`・clippy・nextestのtestの実行は`default-members`の全てのcrateに及ぶ（brokerのcrateのtestが落ちれば関門も落ちる）が、cargo-llvm-cov（0.9.1）は、rootがpackageのworkspaceで`-p`も`--workspace`も無いと`default-members`を見ず、root package（dagq）のファイルだけをreportに入れ、他のmemberのファイルを除外する。envや設定でこれを変える手段は無い。このままでは80%の行カバレッジがdagqだけで計算され、brokerのcrateはcoverageの関門の外になる。人は2026-09-28にask 165で、関門のコマンドを変える案を選んだ。

## Decision

1. **関門のコマンドに`--workspace`を足す。** 関門は`cargo llvm-cov nextest --locked --workspace --fail-under-lines 80`にし、80%の行カバレッジはworkspaceの全てのcrate（dagqとbrokerの3つ）を合わせた全体で守る。CI、AGENTS.mdの関門の記述と推奨のverify、これから登録するruntimeのtaskのverifyはこの形にする。ADR-t827-1決定3の「関門のコマンドに`--workspace`を足さない」はこの点だけ変わり、`default-members`に全てのcrateを入れること（`cargo test`・clippy・workerの手元のコマンドが全てのcrateを覆うこと）はそのまま有効。
2. **登録済みのtaskの旧コマンドは書き換えず、有効なまま。** `cargo llvm-cov nextest --locked --fail-under-lines 80`（と`cargo llvm-cov --locked --fail-under-lines 80`）は、dagqについては今までと同じ関門として働き、brokerのcrateのtestも実行する（落ちれば関門も落ちる）が、brokerのcrateの行はcoverageに数えない。brokerのcrateを変える登録済みのdraft・readyのtaskのverifyを新しい形に直すかはplannerが決める。

## Alternatives

- **コマンドを変えず、brokerのcrateのcoverageを関門に数えない**: 関門を弱める。ADR-t827-1決定3が退けた形。
- **virtual workspaceにしてdagqを`crates/`に移す**（rootにpackageが無ければcargo-llvm-covは全memberをreportする）: ADR-t827-1決定1が既存のパスと走っているrunを崩すとして退けた。
- **cargoのaliasで`llvm-cov`を上書きする**: 外部のsubcommandをaliasで隠す仕組みに頼り、hostやcargoの版で動きが変わる。

## Consequences

- 関門のcoverageの分母にbrokerのcrateが入る。brokerのcrateは、podmanを要らないtest（hostのプロセスで起こしたserverなど）で覆う必要がある（ADR-t827-1決定3のとおり）。
- 旧コマンドのtaskと新コマンドのtaskが並ぶ間、旧コマンドのtaskはbrokerのcrateのcoverageを見ない。CIは新しい形で全体を見る。
