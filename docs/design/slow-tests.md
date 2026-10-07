---
id: design-slow-tests
type: design
title: Slow tests summary from nextest output
status: current
created: 2026-09-30
scope: operations
tags:
  - testing
  - ci
related:
  - adr-0076
  - adr-t1707-1
  - design-stress-ci
  - development-testing
---

# Slow tests summary

`scripts/slow-tests.sh` は cargo nextest の出力から、遅い test の上位と、1 秒・5 秒・30 秒を超えた test の本数と合計の秒と、test binary ごとの本数と合計の秒を Markdown の表にする。遅い test が少しずつ増えて suite（と `integrate` の検証の test 段）が遅くなるのを記録から見えるようにし、test の待ちを減らす修正（goal 68）の前後を比べる土台にする。`.config/nextest.toml` の `slow-timeout`（60 秒を超えた test を `SLOW` と出す）は 1 本ずつの目印で、全体の分布は出さない。

## 使い方

```sh
# CI や手元の 1 回の nextest の出力
cargo llvm-cov nextest --locked --workspace --fail-under-lines 80 2>&1 | tee nextest.log
sh scripts/slow-tests.sh nextest.log
# 本番 queue の integrate の検証の log をまとめて（runs_dir は `dagq locate` が出す）
sh scripts/slow-tests.sh <runs_dir>/*/integrate-*-verify-*.log
# stdin から、上位 50 本
cat nextest.log | sh scripts/slow-tests.sh --top 50
# fixture で表の値を確かめる
sh scripts/slow-tests.sh --self-test
```

| 引数 | 意味 |
| --- | --- |
| `LOG...` | nextest の出力のファイル（`-` は stdin）。無ければ stdin を読む |
| `--top N` | 上位に並べる本数（既定 20） |
| `--min-ratio R` | 途中で止まった log を除く割合（既定 0.9）。`Summary [` の行がある log は失敗の本数に依らず採用する。無い log は、成功・失敗を合わせた終了 test の数が、その log の `Starting N tests` の N の R 倍に満たなければ数えない。`Starting` の行も無い log は、log の中で最も多い終了本数の R 倍と比べる |
| `--self-test` | 下の「self-test」の fixture で表を確かめる（ほかの引数と一緒には取らない） |

`sh` と POSIX の `awk` だけを使うので、macOS と ubuntu の host にそのまま使える。読めない log と引数の誤りは exit 2、ほかは表を出して exit 0（数えた test が無くてもそう出して 0）。`--self-test` は合えば 0、違えば 1。

## 読む行

- `PASS [ 秒s] (i/N) <binary> <test>` の秒をその test の時間にする
- 流し直して通った test は、最後の要約に出る `FLKY-FL 2/2 [ 秒s] ...`（古い nextest の `FLAKY`）の秒（通った回の時間）を使う。時間の表には `TRY n PASS`・`TRY n FAIL`・`FAIL`・`SLOW [> 60.000s]`・`SKIP` を入れない（落ちた test は時間に入らない）
- 完走判定の終了本数には、成功と `TRY n PASS` に加え、`FAIL`・`FAIL + LEAK`・`XFAIL`・`LEAK-FAIL`・`TIMEOUT`・`ABORT`・`SIGSEGV` などの signal・`ABORT SIG n` と、retry の短い status（`TRY n FAIL`・`FL+LK`・`LKFAIL`・`TMT`・`SEGV`・`SIG n` など）を使う（`src/domain/verify_failure.rs` と CI の失敗 status と同じ集合）。`TRY n` の途中の `(───)` は終了として数えない。同じ log の同じ test は最後の結果で 1 本とし、Summary 後の再掲も重複させない。`SLOW`・`SKIP` は終了本数に入れない
- 色の制御コード（`CARGO_TERM_COLOR=always`）は取り除いてから読む。同じ log に同じ test が 2 回出たら後の方を使う
- test の名前は nextest の `<binary id> <test 名>`（`dagq::it runtime_resume::...`、`dagq domain::...`、`dagq-broker ...`）

## 出力の読み方

- 冒頭の行: 終了した test のある log の本数と、そのうち採用した本数。失敗だけの log も終了 status があれば数え、時間の表に成功した test が無ければその旨を出す（`fmt` や `clippy` の verify の log のように test の出ない log は数に入らない）
- 失敗を除いた本数の行: 採用した log の終了 status で失敗だった test の本数（同じ test でも log ごとに 1 本）。途中の retry の失敗や、採用しなかった log の失敗は含めない。失敗の秒はどの時間の表にも入れない
- 範囲の表: `全体` は数えた test の本数と秒の合計、`N 秒を超える` はその test の時間が N 秒より長い本数・その秒の合計・全体の合計に占める割合。合計は test の時間の和で、並列に流れた実時間ではない。`integrate` の test 段の実時間はおおむね「合計 ÷ `NEXTEST_TEST_THREADS`」なので、合計の変化が test 段の変化の目安になる
- 上位の表: 時間の長い順に N 本
- test binary ごとの表（`### test binary ごと`、最後の表）: test の名前の先頭の語（nextest の binary id。`dagq::it`、lib の unit test の `dagq`、`dagq::plugin`、`dagq-broker` などの crate）ごとの本数・秒の合計・全体の合計に占める割合を、合計の長い順に並べる。log を複数渡したときの合計は test ごとの中央値の和。throughput-review の日次の見直しが `dagq::it` と `dagq` の行を日ごとに並べる（`.claude/skills/throughput-review/reference/daily.md`）。前の 3 つの表は足す前と同じで、この表は末尾に足しただけなので、CI の job summary の読み方は変わらない
- log を複数渡すと、test ごとの秒は数えた log をまたいだ中央値（偶数本なら中の 2 つの平均）で、表は中央値で数える。その test が出た log だけで中央値を取るので、期間中に名前の変わった・足された test は別の test として並び、全体の本数が 1 回の実行より多くなることがある。前後を比べるときは、同じ手順で期間を分けて渡し、host の load average を併記する（goal 68 の制約）

## self-test

`sh scripts/slow-tests.sh --self-test` は `scripts/slow-tests-fixtures/` の nextest の出力（`PASS`・`TRY n FAIL`/`TRY n PASS`・最後の要約の `FLKY-FL` と古い `FLAKY`・`FAIL`・`SLOW`・色の付いた行・`dagq::it`・`dagq`・`dagq-broker` の 3 つの binary）を script に通し、出力の全体を同じ dir の期待の Markdown（`one.md`・`two.md`・`stopped.md`・`failures.md`・`summary.md`・`retry-stopped.md`）と比べる。1 本の log、2 本の log の中央値（`--top 3`）、stdin、失敗の多い完走した log（Summary ありと終了本数による判定）、少ない終了行でも Summary があれば採用すること、Summary の無い途中で止まった log（retry の途中を含む）を除くこと、失敗の本数と成功だけの時間の表、と引数の誤り（`--self-test` とほかの引数の組を含む）と読めない log の exit 2 を確かめ、全部が合えば exit 0、どれかが違えば差分を stderr に出して exit 1。期待の Markdown は、表を変えるときに fixture の値から手で確かめて書き直す。

## CI

`.github/workflows/ci.yml` の `cargo llvm-cov nextest` の step は、出力を `tee` で `$RUNNER_TEMP/nextest.log` にも残す（`shell: bash` の `-o pipefail` で、step の成否はコマンドの exit status のまま）。続く `Slow tests` の step が script に通して `$GITHUB_STEP_SUMMARY` に書くので、実行ごとの job summary に同じ表が出る。この step は test の step が落ちても（cancel のときを除き）走り、`continue-on-error: true` で自分の失敗では CI を落とさない。関門のコマンドと失敗の条件は変えていない。

## itのtestの時間の関門

差分で足した・変えた`tests/it`のtestの1本の時間を閾値と比べ、超えたものを許可の一覧の項目が無ければ落とす関門（[ADR-t1707-1](../adr/2026-10-05-t1707-1-time-gate-for-added-or-changed-integration-tests.md)）の今の値と形。許可の一覧に載せてよい理由と関門の場所の規則は[testの制約](../development/testing.md)の「判断と境界のtest」が持つ。

### 閾値

既定は1本**5秒**。本番の関門の`dagq::it`の1本の平均（goal 118の起点の直近10本で約2.6秒、task 1707の初期値を取った10本で約2.35秒＝1,222本・2,876.9秒）の2倍程度で、goal 68と`scripts/slow-tests.sh`の「5 秒を超える」の帯と同じ。test の秒は log の`PASS`と`FLKY-FL`（`FLAKY`）の行から上の「読む行」と同じに読み、1本のtestの行が複数あれば（複数のlog、stressの周回）その中央値と比べる。閾値ちょうどは超えない。

CIのmacOSのrunner（`macos-14`）はnextestの既定（CPU数）の並列で流し、本番の関門は`dagq.toml`の`[run.env]`の`NEXTEST_TEST_THREADS`で絞って流すので、同じtestの秒が違う。今の値は本番の秒で決めたもので、CIで超えて本番で超えない（またはその逆の）testがありうる。CIで落ちたtestが本番では閾値の内なら、本番の関門のlogの秒を添えて直すか項目を足すかをplannerが決める。workerの手元の秒もhostの負荷で揺れる。

### 許可の一覧

`.config/it-slow-allow.toml`。1つの項目は`[[test]]`で、3つのkeyを全部持つ（書式は`.config/e2e-quarantine.toml`の流儀）。

```toml
[[test]]
name = "runtime_handoff::review_verdicts::reviews_that_ended_before_the_exec_are_applied_not_run_again"
reason = "R 移し替えの予定（代表がある、goal 119 か 120）: same review/handoff boundary"
task = 1707
```

| key | 意味 |
| --- | --- |
| `name` | test binary `dagq::it`の中の完全なnextestの名前（下の「名前」）。末尾のfnの名前だけの項目は何も許さない |
| `reason` | 守る境界か、移し替えの予定と行き先のtask・goal |
| `task` | 項目を足したtaskのID |

keyの欠けた項目・知らないkey・1つの項目の中で2回書いたkey・同じ`name`の2つの項目は読めない一覧として扱う（exit 2）。初期値（task 1707）は、本番の関門の直近10本（2026-10-04T20:32〜2026-10-05T00:06 +0900、Summaryを含む`integrate-*-verify-*.log`）を`scripts/slow-tests.sh`で集計し、中央値が5秒を超えた`dagq::it`のtest 129本。理由は[it-reduction](../plans/it-reduction.md)の`it-tests.tsv`のclassとreasonで、完全な名前の先頭の要素がTSVの`module`、末尾の要素が`test`と一致する行が1つのときだけ使い、0行か2行以上ならclassを`unknown`にする（TSVの`module`は入れ子のmodを落とし秒も当てにならないので、名前と秒はlogから取る）。各項目の上のcommentに中央値とclassを書く。

HEADの`tests/it`に同じ完全な名前の`#[test]`が無い項目は、差分の対象と同じ名前の求め方で検出し、名前ごとにstderrへ警告する。警告だけでexit statusは変えないので、CIやworkerの手元でほかのtaskを止めない。testを消す・移すtaskは、消えた完全な名前の項目を同じ変更で外す。残したままだと、後で同じ名前のtestを足したときに関門を素通りする。task 1736で初期値のうち消えた23項目を外した。

### 名前

対象のtestと許可の一覧とlogは、同じ完全なnextestの名前で照合する。

- 対象は、HEADの`tests/it`の下の`.rs`のうち`#[test]`の付いたfnで、`git diff --unified=0 <base>...HEAD -- tests/it`が足した・変えた行が、その`#[test]`の行から閉じる`}`までに入るもの。消しただけの行は、消した位置の前後の行が両方その範囲に入るときだけ数える（testの直後のhelperを消してもそのtestは対象にならない）。
- 名前は、fileの`tests/it`からのpathから`.rs`（と`/mod`）を除いて`/`を`::`にしたもの（`tests/it/runtime_handoff.rs`なら`runtime_handoff`、`tests/it/main.rs`なら無し）に、fnを囲む`mod NAME {`の入れ子とfnの名前を`::`でつなぐ。`tests/it/main.rs`に`#[path = "..."] mod NAME;`（1行でも2行に分けても）があってそのfileを指すなら、pathの代わりに`NAME`を使う。
- signatureの`()`と`[]`の中の`;`（`-> [u8; 2]`）はitemの終わりと見ない。括弧の対応は、comment（入れ子の`/* */`を含む）・文字列・raw文字列・char literalの中の`{`と`}`を数えない。macroが展開するtestは見ない。
- logの行の名前は、色を除いた`PASS [ 秒s] [i/N] (j/M) dagq::it <名前>`の末尾の2語（binaryと名前）で、binaryが`dagq::it`の行だけを読む。

### script

`scripts/check-it-test-time.sh`。`sh`とPOSIXの`awk`と`git`だけを使う。

```sh
sh scripts/check-it-test-time.sh --base REV [--threshold SECS] [--allow FILE] [LOG...]
sh scripts/check-it-test-time.sh --self-test
```

| 引数 | 意味 |
| --- | --- |
| `--base REV` | 比べるcommit（必須、既定は無し）。`git diff REV...HEAD -- tests/it` |
| `--threshold SECS` | 閾値の秒（既定5） |
| `--allow FILE` | 許可の一覧（既定`.config/it-slow-allow.toml`） |
| `LOG...` | nextestの出力のfile（複数可、`-`はstdin）。無ければstdin |
| `--self-test` | `scripts/check-it-test-time-fixtures/`のbaseとheadから一時dirに2つのcommitを作り、超過・許可・変えていない遅いtest・logに無いtest・色の付いた行・入れ子のmodの中の既存のtestの本文の変更（と2行と1行の`#[path]`、signatureの`;`、testの直後のhelperを消すこと、閾値の内のexit 0、引数の誤り・読めないlog・理由の無い項目・2回書いたkeyのexit 2、このrepositoryの許可の一覧が読めること、HEADに無い許可項目の名前つき警告とexit statusの不変（0/1/2）、入れ子のmodの完全な名前の項目には警告しないこと）を確かめる |

exit statusは、閾値を超え許可の一覧に無い対象が無ければ0、あれば1（1本ずつ名前・秒・file:行をstderrに出す）、引数の誤り・読めないlogか許可の一覧・書式の誤り・gitの失敗は2。logに秒の無い対象（流さなかったtest、`#[ignore]`）と、HEADの`tests/it`に同じ完全な名前の`#[test]`が無い許可項目は警告をstderrに出すだけ（上の「許可の一覧」）。最後の1行（stdout）に対象・超過・閾値の内か許可・秒の無いものの本数を出す。

### CI

`.github/workflows/ci.yml`のmacOSのjobの`Slow tests`の後の`IT test time gate`のstepが、同じ`$RUNNER_TEMP/nextest.log`をscriptに渡す。`--base`はpushでは`github.event.before`、pull_requestでは`github.event.pull_request.base.sha`。`before`が無い（新しいbranchの最初のpushで0の列）か、そのcommitが無いときは理由を出してskipする。checkoutは`fetch-depth: 0`なので`REV...HEAD`のmerge baseが求まる。nextestのstepが落ちても（cancelのときを除き）走り、`continue-on-error`を付けないので、超えればjobが落ち、mainへのpushではci-failureのissueが開く（[ci-failure-issues](ci-failure-issues.md)）。
