---
id: design-linux-ci
type: design
title: Linux build and test job in CI
status: current
created: 2026-10-02
updated: 2026-10-02
last_verified: 2026-10-02
scope: operations
tags:
  - testing
  - ci
related:
  - design-stress-ci
  - design-slow-tests
  - adr-0076
---

# Linux build and test job in CI

この repository を Linux で build と test を通すため（goal 83。先に Linux で通し、その後にコンテナで動かす）、`.github/workflows/ci.yml` に ubuntu の job `linux`（表示名 `linux build / test`）を置く。macOS の job `checks`（fmt・各 `scripts/` の検査・clippy・coverage の関門の `cargo llvm-cov nextest` と slow tests）は変えない。

## 流すもの

| step | 中身 |
| --- | --- |
| `actions/checkout@v4` | checkout（履歴は既定の浅いもの。`scripts/` の検査は macOS の job が流す） |
| Install Rust | `rustup toolchain install --no-self-update`。`rust-toolchain.toml` の channel と components を入れる |
| Install cargo-nextest | `taiki-e/install-action@cargo-nextest` |
| `Swatinem/rust-cache@v2` | 依存の build の cache |
| cargo build | `cargo build --locked`（workspace の `default-members` の全ての crate） |
| cargo nextest run | `cargo nextest run --locked --color never` の出力を `$RUNNER_TEMP/nextest.log` に `tee` する。macOS の関門と同じ test binary（`src/` の unit test、`tests/it`・`tests/plugin.rs`・`tests/e2e.rs`、`crates/` の test）を流し、e2e は `#[ignore]` のまま流さない。`.config/nextest.toml` の `fail-fast = false` で全部の test を流しきり、`retries = 1` も macOS と同じく効く |
| Failed tests | 前の step が落ちても（`!cancelled()`）走り、job の summary（`$GITHUB_STEP_SUMMARY`）に落ちた test を書く |

runner は `ubuntu-24.04`（x86_64）。`rust-toolchain.toml` の `targets` は `aarch64-apple-darwin` だけを挙げるが、rustup は挙げた target に加えて host（`x86_64-unknown-linux-gnu`）の std を必ず入れるので、Linux の job のために `rust-toolchain.toml` は変えない。rusqlite は `bundled` で、C compiler は runner に入っている。

## 失敗を通す期間

job は `continue-on-error: true` で、Linux の build か test が落ちても workflow 全体の結果（main の CI の結果）は落ちない。job 自体は失敗として表示され、pull request でもこの job の check は失敗と出るが、required の check にしない限り merge を止めない。macOS に固有の test を理由付きで分ける（`#[cfg(target_os = "macos")]` など）まで Linux の失敗を見えるだけにしておくための一時の措置で、goal 83 の受け入れ条件は main で Linux の job が通り、`continue-on-error` を外すこと。

## summary の書式

`Failed tests` の step は summary に `## Linux: failed tests` の見出しと次のどれかを書く。

- 落ちた test があるとき: `<N> failed:` と、コードブロックに 1 行 1 本の `<binary id> <test name>`（例 `dagq::it runtime_claim::name`）。log の最後の `Summary [` の行より後の結果の行（`FAIL`・`TRY <n> FAIL`・`TIMEOUT`・`SIG*`・`ABORT`・`LEAK-FAIL`・`FLKY-FL`）から、最後の 2 つの欄を取って重複を除く。`FLKY-FL` は落ちて流し直しで通った test で、これも載る
- 落ちた test が無いとき: `none`
- どちらでも、その後に nextest の `Summary [` の行（流した数・通った数・落ちた数）
- log が無いか `Summary [` の行が無いとき（build か準備の失敗）: `no nextest summary (the build or the setup failed; see the log)`

次の task はこの summary から、Linux で落ちる test を読んで macOS に固有のものを分ける。
