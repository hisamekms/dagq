---
id: adr-t2126-1
type: adr
title: resource brokerのcrateを外した後、dagqは[workspace]の無い単一のpackageに戻り、coverageの関門は--workspaceの付いたコマンドのまま、登録済みのtaskの旧コマンドも有効なまま引き継ぐ（ADR-t828-1を置き換える）
status: accepted
created: 2026-10-09
updated: 2026-10-09
accepted_on: 2026-10-09
supersedes:
  - adr-t828-1
owners:
  - hisamekms
tags:
  - runtime
  - testing
  - release
related:
  - adr-t2113-1
  - adr-t2125-1
  - adr-t827-1
  - adr-t828-1
  - adr-t1925-1
  - adr-0030
  - development-task-registration
  - development-testing
---

# ADR-t2126-1: resource brokerのcrateを外した後、dagqは`[workspace]`の無い単一のpackageに戻り、coverageの関門は`--workspace`の付いたコマンドのまま引き継ぐ（ADR-t828-1を置き換える）

## Context

[ADR-t2113-1](2026-10-08-t2113-1-remove-the-resource-broker.md)はresource brokerを外すと決め、brokerのcrate（`crates/`の3つ）が無くなった後のworkspaceの形を撤去の実装に任せた。
dagqの側からbrokerを外し、build識別子をdagqに移したのは[ADR-t2125-1](2026-10-08-t2125-1-e2e-gate-checks-only-cmux-after-the-broker-removal.md)で、残るのはcrateとworkspaceの撤去になる。

[ADR-t828-1](2026-09-28-t828-1-coverage-gate-covers-the-workspace-with-workspace-flag.md)は、cargo-llvm-covがrootがpackageのworkspaceで`default-members`を見ないので、coverageの関門に`--workspace`を足してbrokerのcrateも80%に数え（決定1）、`--workspace`の無い登録済みの旧コマンドは書き換えず有効とし、brokerのcrateの行を数えないことを受け入れた（決定2）。
どちらの決定もbrokerのcrateがworkspaceに居ることを前提にし、amendsの元の[ADR-t827-1](2026-09-28-t827-1-broker-crates-binaries-and-version-alignment.md)決定3もADR-t2113-1に置き換えられた。
一方で関門のコマンドの`--workspace`は、CI・推奨のverify・登録済みのtaskのverify・scriptsの`cargo nextest`の行に広く入っている。

memberがdagqだけになるとworkspaceが持つものが無い。
cargoは`[workspace]`の無い単一のpackageをそれ自身のworkspaceとして扱うので、`--workspace`を付けたcargo・nextest・llvm-covのコマンドはそのpackageだけを対象にそのまま動く。
edition 2024の既定のresolverは3で、`resolver`の行を外しても依存の解決は変わらない。

## Decision

1. **dagqは`[workspace]`の無い単一のpackageに戻る。** rootの`Cargo.toml`から`[workspace]`（`members`・`default-members`・`resolver`）を外す。crateを足すtaskが要るときにworkspaceを戻す。
2. **coverageの関門のコマンドは`cargo llvm-cov nextest --locked --workspace --fail-under-lines 80`のまま。** CI、推奨のverify、これから登録するruntimeのtaskのverifyはこの形を使い続ける。単一のpackageでは`--workspace`はdagqだけを指し、80%の行カバレッジはdagqの全体で守る。
3. **登録済みのtaskのverifyは書き換えず、どれも有効なまま。** `--workspace`のある形も、`--workspace`の無い旧コマンド（`cargo llvm-cov nextest --locked --fail-under-lines 80`・`cargo llvm-cov --locked --fail-under-lines 80`）も、単一のpackageでは同じ範囲を測る。着地の検証でこれらを置き換える扱い（[ADR-t1925-1](2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)）は変えない。

ADR-t828-1の決定1・2はこのADRの決定2・3に書き直して引き継ぎ、ADR-t828-1を丸ごと置き換える。

## Alternatives

- **`[workspace]`を`members = ["."]`で残す**: 単一のpackageと同じ動きで、持つものの無い表と、crateが居るかのような見た目だけが残る。
- **virtual workspaceにしてdagqを`crates/`の下に移す**: 既存のpath（`src/`・`tests/`・scripts・`dagq.toml`のglob・走っているrun）を全て崩し、得るものが無い。
- **関門のコマンドから`--workspace`を外す**: 単一のpackageでは範囲が変わらないのに、CI・推奨・登録済みのverify・scriptsの行を書き換えることになる。

## Consequences

- `--workspace`を付けたコマンドと付けないコマンドは同じ範囲を流し、測る。`-p dagq-broker*`を名指すコマンドは対象が無くなり失敗する。
- crates.ioへのpublishとGitHub Releaseのassetはdagqの1本（tarと`SHA256SUMS`）になる。既にcrates.ioに出たbrokerのcrateの版は残る。
- workspaceの要否は、crateを足すtaskがこのADRを置き換えるかamendsして決め直す。
