---
id: design-stress-ci
type: design
title: Scheduled stress of recently changed tests in GitHub Actions
status: current
created: 2026-09-28
updated: 2026-10-03
last_verified: 2026-10-02
scope: operations
tags:
  - testing
  - ci
related:
  - adr-t920-1
  - adr-t768-1
  - adr-0076
---

# Scheduled stress of recently changed tests

main に直近で足した・変えた test を GitHub Actions で 1 日 1 回、多い周回と高い並列度で繰り返し、落ちた test を GitHub の issue で人に知らせる。worker の手元の stress（[手元の検証](../development/local-checks.md)の「stress」。5 周の軽い見張り）が止めきれない、稀にしか落ちない不安定な test を着地の後に見つけるためのもので、host の load も queue の slot も使わない（なぜそうしたかは [ADR-t920-1](../adr/2026-09-28-t920-1-light-worker-stress-and-heavy-repetition-in-scheduled-ci.md) 決定 2）。

dagq の finding は observer しか書けないので、結果は queue ではなく issue に残り、planner が issue から直す task を登録する。

## 部品

| 部品 | 役割 |
| --- | --- |
| `scripts/stress-recent-tests.sh` | commit の範囲の diff から対象の test を選び、`cargo nextest run --stress-count` で繰り返す。手元でも CI でも同じものを使う |
| `.github/workflows/stress.yml` | 定時（毎日 18:00 UTC = 03:00 JST）と `workflow_dispatch` で script を流し、落ちた test ごとに issue を開くか追記する |

## 対象の test の選び方

worker の stress の項と同じ選び方にする。範囲の diff（`git diff -U0 <base> <head>`）で追加・変更された行が、`#[test]` の属性から関数の閉じ括弧までの行に 1 行でも掛かる関数を選ぶ（行の削除だけの hunk は、その前後の行を変わった行として数える）。関数の範囲は、文字列（複数行と raw string を含む）・文字 literal・コメントを除いた括弧の深さで数える。nextest の出力は `--color never` で読む。

| ファイル | nextest の binary | test の名前 |
| --- | --- | --- |
| `tests/it/**`（`it`） | `dagq::it` | `<ファイルの module>::<inline module>::<name>`（`tests/it/runtime_claim.rs` なら `runtime_claim::<name>`） |
| `src/**`（`src/main.rs` を除く） | `dagq`（lib） | `<module のパス>::tests::<name>`（`src/domain/exit.rs` なら `domain::exit::tests::<name>`） |
| `src/main.rs` | `dagq::bin/dagq` | `tests::<name>` など |
| `crates/<crate>/src/**` | `<crate>`（`main.rs` は `<crate>::bin/<crate>`） | lib と同じ |
| `crates/<crate>/tests/<file>.rs` | `<crate>::<file>` | `<name>` |

`tests/e2e.rs`（と `tests/e2e/`）と `tests/plugin.rs` は対象外（e2e は cmux を要り `#[ignore]`、plugin は文書を読む test）。`tests/common` と `tests/it/runtime_support` の helper の変更は、それを使う test を選ばない（helper に `#[test]` は無い）。

script は選んだ test を `binary_id(<binary>) & test(=<name>)` の和の filter にし、build の後に `cargo nextest list` で名前が解決できるかを確かめる。解決できない名前（parser の読み違いや、checkout に無い test）は warning に出し、1 つも解決できなければ流さずに exit 0 にする。対象が無い範囲も流さずにそう出して exit 0。

## 繰り返し方

- 周回は既定で 20（`--count` / `STRESS_COUNT`）。`STRESS_DURATION`（例 `30m`）を置くと周回の代わりに時間の上限になる
- nextest の `--test-threads` は既定で CPU 数の 2 倍（`STRESS_TEST_THREADS`）で、同じ filter の nextest を `STRESS_JOBS`（既定 2）本同時に流し、test どうしが CPU を取り合う負荷を作る。build は先に 1 回だけ行う
- `--no-fail-fast` で全ての周回を流し切り、1 回でも落ちた test を `<STRESS_OUT>/failed-tests.txt`（1 行に `<binary> <test>`、既定の `STRESS_OUT` は `target/stress`）に書いて exit 1 にする。log は `<STRESS_OUT>/stress-<N>.log`
- 1 回でも落ちた test は、log の結果の行の status が [Integrate](supervisor-lifecycle/integrate.md) の「nextestの失敗のstatusの集合」の (A)(B)(C) に当たる行（`FAIL`・`FAIL + LEAK`・`XFAIL`・`LEAK-FAIL`・`TIMEOUT`・`ABORT`・`SIG<name>`・`ABORT SIG <n>`、`TRY <n>` の後の `FAIL`・`FL+LK`・`XFAIL`・`LKFAIL`・`TMT`・`ABORT`・signal の名前・`SIG <n>`、`FLKY-FL n/m`・`FLAKY n/m`）の最後の 2 語で読む（task 1272。script の `failed_status`）。`.config/nextest.toml` の `retries = 1` の下で、1 回目に落ちて流し直しで通った test は `TRY 1 <短い形>` と `FLKY-FL n/m` の行しか出さないが、これが定時の stress が知らせたい不安定な test なので載せる。`LEAK`（子プロセスを残したが通った test）は載せない: nextest は通ったと数えて stress の終了コードを失敗にせず、載せると通った test で issue を開くことになる。leak で落ちた test は `FAIL + LEAK`・`LEAK-FAIL`・`FL+LK`・`LKFAIL` で載る（task 1272 より前は `LEAK` も載せていた）。`SLOW`・`TRY <n> SLOW`・`START` などの失敗でない行も載せない。集合は `src/domain/verify_failure.rs` と `.github/workflows/ci.yml` の Linux の job と同じで、正規表現は ci.yml と同じ文字列

## 範囲

script は `--base REV`（`STRESS_BASE`）があればその commit（含まない）から、無ければ `--since DATE`（`STRESS_SINCE`、既定 `24 hours ago`）より前の最後の first-parent の commit から、`--head`（`STRESS_HEAD`、既定 `HEAD`）までを見る。名前は `--head` の内容から読むので、流す checkout は `--head` にしておく。

workflow は `main` を checkout し、範囲の始まりを次の順で決める。

1. `workflow_dispatch` の `base` の入力
2. この workflow の main での最後の成功した実行の commit（`gh run list --status success`。main の祖先でなければ使わない）
3. `since` の入力（定時実行では `24 hours ago`）

失敗した実行は 2 の始まりを動かさないので、落ちた test は直るまで次の日も範囲に残り、同じ issue に追記が続く。`workflow_dispatch` では `base`・`since`・`count` を入力で上書きできる。

## 落ちたとき

stress の step が落ちると、`target/stress/` を artifact `stress-logs` に上げ、`failed-tests.txt` の 1 行ごとに次を行う。

- label `flaky-test` を（無ければ）作る
- title `flaky test: <binary> <test>` の開いた `flaky-test` の issue があれば、実行の URL・commit・再現のコマンドをコメントで足す
- 無ければ同じ本文で issue を開く

build の失敗など、test の名前が出ない失敗では issue を作らず、実行の失敗（GitHub の通知）だけになる。workflow の権限は `contents: read`・`actions: read`（最後の成功した実行を読む）・`issues: write` で、書くのは issue だけ。

## 手元で流す

```sh
# 範囲の対象だけを見る
sh scripts/stress-recent-tests.sh --base <rev> --head HEAD --list
# 短い周回で流す
sh scripts/stress-recent-tests.sh --base <rev> --count 2
```

cargo-nextest が要る（[ADR-0076](../adr/0076-run-the-coverage-gate-tests-with-nextest.md) 決定 3）。host で流すと host の load を上げるので、dagq の queue が走っている host では短い周回にとどめる。
