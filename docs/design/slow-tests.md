---
id: design-slow-tests
type: design
title: Slow tests summary from nextest output
status: current
created: 2026-09-30
updated: 2026-09-30
last_verified: 2026-09-30
scope: operations
tags:
  - testing
  - ci
related:
  - adr-0076
  - design-stress-ci
---

# Slow tests summary

`scripts/slow-tests.sh` は cargo nextest の出力から、遅い test の上位と、1 秒・5 秒・30 秒を超えた test の本数と合計の秒を Markdown の表にする。遅い test が少しずつ増えて suite（と `integrate` の検証の test 段）が遅くなるのを記録から見えるようにし、test の待ちを減らす修正（goal 68）の前後を比べる土台にする。`.config/nextest.toml` の `slow-timeout`（60 秒を超えた test を `SLOW` と出す）は 1 本ずつの目印で、全体の分布は出さない。

## 使い方

```sh
# CI や手元の 1 回の nextest の出力
cargo llvm-cov nextest --locked --workspace --fail-under-lines 80 2>&1 | tee nextest.log
sh scripts/slow-tests.sh nextest.log
# 本番 queue の integrate の検証の log をまとめて（runs_dir は `dagq locate` が出す）
sh scripts/slow-tests.sh <runs_dir>/*/integrate-*-verify-*.log
# stdin から、上位 50 本
cat nextest.log | sh scripts/slow-tests.sh --top 50
```

| 引数 | 意味 |
| --- | --- |
| `LOG...` | nextest の出力のファイル（`-` は stdin）。無ければ stdin を読む |
| `--top N` | 上位に並べる本数（既定 20） |
| `--min-ratio R` | 途中で止まった log を除く割合（既定 0.9）。時間の出た test の数が、その log の `Starting N tests` の N の R 倍に満たない log を数えない。`Starting` の行が無い log は、log の中で最も多い数の R 倍と比べる |

`sh` と POSIX の `awk` だけを使うので、macOS と ubuntu の host にそのまま使える。読めない log と引数の誤りは exit 2、ほかは表を出して exit 0（数えた test が無くてもそう出して 0）。

## 読む行

- `PASS [ 秒s] (i/N) <binary> <test>` の秒をその test の時間にする
- 流し直して通った test は、最後の要約に出る `FLKY-FL 2/2 [ 秒s] ...`（古い nextest の `FLAKY`）の秒（通った回の時間）を使う。`TRY n PASS`・`TRY n FAIL`・`FAIL`・`SLOW [> 60.000s]`・`SKIP` は数えない（落ちた test は時間に入らない）
- 色の制御コード（`CARGO_TERM_COLOR=always`）は取り除いてから読む。同じ log に同じ test が 2 回出たら後の方を使う
- test の名前は nextest の `<binary id> <test 名>`（`dagq::it runtime_resume::...`、`dagq domain::...`、`dagq-broker ...`）

## 出力の読み方

- 冒頭の行: test の時間の出た log の本数と、そのうち数えた本数（`fmt` や `clippy` の verify の log のように test の出ない log は数に入らない）
- 範囲の表: `全体` は数えた test の本数と秒の合計、`N 秒を超える` はその test の時間が N 秒より長い本数・その秒の合計・全体の合計に占める割合。合計は test の時間の和で、並列に流れた実時間ではない。`integrate` の test 段の実時間はおおむね「合計 ÷ `NEXTEST_TEST_THREADS`」なので、合計の変化が test 段の変化の目安になる
- 上位の表: 時間の長い順に N 本
- log を複数渡すと、test ごとの秒は数えた log をまたいだ中央値（偶数本なら中の 2 つの平均）で、表は中央値で数える。その test が出た log だけで中央値を取るので、期間中に名前の変わった・足された test は別の test として並び、全体の本数が 1 回の実行より多くなることがある。前後を比べるときは、同じ手順で期間を分けて渡し、host の load average を併記する（goal 68 の制約）

## CI

`.github/workflows/ci.yml` の `cargo llvm-cov nextest` の step は、出力を `tee` で `$RUNNER_TEMP/nextest.log` にも残す（`shell: bash` の `-o pipefail` で、step の成否はコマンドの exit status のまま）。続く `Slow tests` の step が script に通して `$GITHUB_STEP_SUMMARY` に書くので、実行ごとの job summary に同じ表が出る。この step は test の step が落ちても（cancel のときを除き）走り、`continue-on-error: true` で自分の失敗では CI を落とさない。関門のコマンドと失敗の条件は変えていない。
