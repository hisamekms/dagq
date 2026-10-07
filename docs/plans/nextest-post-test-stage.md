---
id: plan-nextest-post-test-stage
type: plan
title: cargo llvm-cov nextestのtest段の後（一覧・profrawのmerge・report）の内訳
status: completed
created: 2026-09-28
owners:
  - hisamekms
tags:
  - performance
  - testing
  - measurement
related:
  - adr-0076
  - adr-0078
  - plan-nextest-measurement
---

# cargo llvm-cov nextestのtest段の後（一覧・profrawのmerge・report）の内訳

task 564の測定。[nextest-measurement](nextest-measurement.md)（task 537）は、`cargo llvm-cov nextest`の「test段の後」（段の時間からbuildとnextestの`Summary`を引いた残り）が旧コマンドの約20秒に対し518で63秒・551で109秒と長く、integration testを1つのbinaryにまとめた（[ADR-0078](../adr/0078-one-integration-test-binary.md)）ことでprofrawのmergeが重くなった可能性を挙げた。この文書はその時間を工程に分けて測り、goal 36の並列数の判断とtask 565（`NEXTEST_TEST_THREADS`を上げる）の材料にする。変更はせず、taskも登録しない。

## 要点

- **test段の後は短い。いまのmainで約12秒（nextest）・約5.5秒（旧コマンド）。** nextestでは、testの一覧（build後から`Starting`まで）が3.3秒、testが終わってからnextestが戻るまでが0.02秒、`cargo llvm-cov report`が8.7秒（profrawのmerge 7.4秒、`llvm-cov report` 0.5秒、`llvm-cov export` 0.7秒）。最も時間を使うのは**`llvm-profdata merge`**（report段の85%）。
- **518の63秒・551の109秒は外れ値で、mergeの重さではなかった。** 残っているnextestのintegrateのlog 221件（test段が100秒を超えたもの）では、test段の後の中央値7.1秒・p90 15.5秒・p95 22.3秒で、60秒を超えたのは551（108秒）を含む3件だけ。551はlogの時刻では分けられないが、host のmetricsではtestのprocessが19:02:45（verifyのlogが閉じる約30秒前）まで残っており、Summaryの170秒と合わせると、長かったのはtest段の後ではなくtestの開始より前（build後の一覧かbuildの前）と読める（下の3）。
- **profrawの件数はprocessの数で決まる。** nextestは3,525件・3.0GB、旧コマンドは1,746件・1.35GB。nextestで増えたのはtestのprocessごとの1件（lib 878件・`it` 741件）で、両方に共通する最大の出どころはtestが起動する`dagq`のCLI（約1,740件・1.35GB）。mergeの時間は合計サイズにほぼ比例する（3.0GBで7.4秒、1.35GBで3.9秒）。
- **`NEXTEST_TEST_THREADS`を上げてもtest段の後はほぼ変わらない。** profrawの数はtestとCLIのprocessの数で決まり、並列度に依らない。mergeと一覧はtest段の外で走る。並列度を上げたときの床は、test段の後ではなく**最長のtest**になる。`runtime_stale_receipt::a_stale_receipt_left_during_a_wait_is_unchanged`が直近のintegrateの50件のうち18件で127〜133秒かかり（他は7〜13秒）、今回の測定でもtest段の最後の約60秒はこの1件だけが走っていた（follow-up）。

## 1. 測定の条件

| 項目 | 値 |
| --- | --- |
| commit | `d4a23e4`（mainの最新。run `b4057e07`のworktree） |
| 日時 | 2026-09-28 17:14〜17:34 JST（nextest 17:14:16〜17:22:03、旧コマンド 17:24:55〜17:33:58） |
| host | 8コア（aarch64-apple-darwin）、16GB。他のrunが走っている本番の負荷の下 |
| toolchain | rustc 1.98.1（LLVM 22.1.8） |
| cargo-llvm-cov | 0.9.1 |
| cargo-nextest | 0.9.146 |
| env | main checkoutの`dagq.toml`の`[run.env]`と同じ: `RUSTC_WRAPPER=sccache`、`SCCACHE_IGNORE_SERVER_IO_ERROR=1`、`CARGO_BUILD_JOBS=4`、`RUST_TEST_THREADS=8`、`NEXTEST_TEST_THREADS=8` |
| test | nextest: 1,736件（13 binary、16件skip）。旧コマンド: 17 binary（`it`は738件） |

段ごとのコマンド（各1回、並べずに順に流した）:

- (a) `cargo llvm-cov clean --workspace`
- (b) `cargo llvm-cov nextest --locked --no-report -v`（旧コマンドは`cargo llvm-cov --locked --no-report -v`）
- (c) `target/llvm-cov-target`の`*.profraw`の件数と合計サイズ
- (d) `cargo llvm-cov report --fail-under-lines 80 -v`

出力の各行にperlで受け取った時刻（ミリ秒）を付け、`-v`が出す`Running`の行（`llvm-profdata merge`・`llvm-cov report`・`llvm-cov export`）の時刻の差で(d)を工程に分けた。関門のコマンドはいま`--workspace`を付けるが（ADR-t828-1）、518・551の数字と並べるため、taskの指定どおり`--workspace`なしのreportで測った（どちらもreportの`-object`はworkspaceの全てのbinaryを渡しており、mergeの入力は同じ）。

## 2. 段ごとの所要時間

| 段 | nextest | 旧コマンド |
| --- | --- | --- |
| (a) clean | 0.24秒 | 1.48秒 |
| (b) build（cargoの`Finished`） | 54.5秒（sccacheあり） | 40.1秒 |
| (b) testの一覧（`Finished`→`Starting`） | 3.3秒 | —（binaryごとに順に起動） |
| (b) test段 | 400.1秒（`Summary`） | 495.5秒（`Finished`→最後の`test result`。binaryごとの`finished in`の合計は490.7秒） |
| (b) testの後にcargo-llvm-covが戻るまで | 0.02秒 | 0.02秒 |
| (b) 合計 | 458.3秒 | 535.7秒 |
| (c) profrawの件数・合計サイズ | 3,525件・3.00GB（`llvm-cov-target`全体4.6GB） | 1,746件・1.35GB（同3.1GB） |
| (d) report合計 | 8.70秒 | 5.45秒 |
| (d) うちcargo-llvm-covの起動・objectの一覧 | 0.14秒 | 0.16秒 |
| (d) うち`llvm-profdata merge -sparse` | 7.36秒 | 3.91秒 |
| (d) うち`llvm-cov report` | 0.47秒 | 0.52秒 |
| (d) うち`llvm-cov export`（`--fail-under-lines`の判定） | 0.73秒 | 0.86秒 |
| **test段の後（一覧＋testの後＋report）** | **約12.0秒** | **約5.5秒** |

load average（1分・5分・15分）:

| 時点 | nextest | 旧コマンド |
| --- | --- | --- |
| (a)の前 | 8.24・16.21・18.32 | 2.28・8.91・14.05 |
| (b)の後・(d)の前 | 7.68・14.71・16.93 | 30.60・26.96・20.34 |
| (d)の後 | 7.18・14.37・16.78 | 28.63・26.61・20.25 |

旧コマンドは測定の途中で他のrunのbuildが重なりloadが30まで上がったので、test段の時間（495.5秒対400.1秒）はnextestと公平に比べられない。test段の後の比較には影響が小さい（mergeとreportは数秒）。

profrawの内訳（ファイル名`worktree-<pid>-<binaryの識別子>_<N>.profraw`の識別子ごと。binaryは件数から判断した）:

| 出どころ | nextest | 旧コマンド |
| --- | --- | --- |
| testが起動する`dagq`のCLI（`CARGO_BIN_EXE_dagq`） | 1,741件・1,365MB | 1,722件・1,350MB |
| libのunit test（testごとのprocess） | 878件・756MB | 1件・1MB未満 |
| `it`（testごとのprocess） | 741件・873MB | 3件・4MB |
| その他（broker・plugin・e2eなど） | 165件・数MB | 20件・1MB未満 |

1件あたりは中央値約0.78MB（最大1.18MB）。mergeの出力（`worktree.profdata`）はどちらも3.3MB。

## 3. integrateの518・551との対応

task 537の「test段の後」は、verifyのlogの作成から最後の書き込みまで（段の時間）からcargoの`Finished`と`Summary`を引いた残りで、buildの前（cargo-llvm-covの起動・cargoのlockの待ち）と一覧も含む。

残っているnextestのintegrateのlog（2026-09-26〜28、`runs/*/integrate-*-verify-*.log`）を同じ定義で集計した（test段が100秒を超えたもの、task 537が別に扱った518と550を除く221件）:

| 範囲 | 件数 | test段の後 中央値 | p90 | 最大 |
| --- | --- | --- | --- | --- |
| 全体 | 221 | 7.1秒 | 15.5秒 | 701秒（1件） |
| 2026-09-28に始まったverify（1,385〜1,736件のtest） | 65 | 10.1秒 | 20.6秒 | — |
| 12〜13 binary（brokerのcrateが入った後、2026-09-28 12:40以降） | 19 | 13.1秒 | — | 28.9秒 |

- 60秒を超えたのは3件（551の108秒、`fd1e1682`の70秒（buildが18分30秒かかったrun）、`c41d440e`の701秒）だけで、どれもlogに手がかりが無い。今回の手元の測定の約12秒と合わせ、ふだんのtest段の後は10〜20秒で、testとbinaryの数に合わせて少しずつ伸びている（5 binaryで7秒前後、13 binaryで13秒前後）。
- 551（2026-09-26 18:58:13〜19:03:14）: host のmetrics（`~/.local/share/dagq-hostmetrics/metrics.csv`、30秒ごと）では、`target/…/deps/`のprocess（testの一覧とtestのprocess）が18:58:55から19:02:45まで3〜4本あり、19:03:17に0になった。18:58:55〜19:01:11はmetricsの採取自体が止まっている。testが19:02:45過ぎまで走っていたなら、`Summary`の170秒からtestの開始は19:00頃で、buildの`Finished`（約18:58:36）からの約80秒はtestの開始より前にある。今回の測定のmerge（3.0GBで7.4秒）から見ても、551（test 739件、profrawは今回の半分程度と見込まれる）のmergeが100秒かかったとは考えにくい。原因はtestの一覧かhostの一時的な停止と見るが、logに時刻が無く確定できない。
- 518（46 binary、63秒）: nextestはbinaryごとに一覧を取るので、binaryの多い構成では一覧が長い。同じ47 binaryの550は22秒、旧コマンドの47 binary（2026-09-26）は17〜21秒だった。518はload1が12.5まで上がっていた。
- 旧コマンドの残っているlog（88件）の同じ残りは、47 binaryの期間で18〜21秒、5 binaryになった後（551以降）で3〜14秒。1 binaryへのまとめはtest段の後を縮めこそすれ、伸ばしてはいない。

## 4. 削る手の候補と見立て（変更はしない）

test段の後はいま約12秒で、関門の段（400秒前後）の3%程度にすぎない。削る価値はtest段より小さい。候補を効く順に挙げる。

1. **`dagq`のCLIのprofrawを減らす（mergeの入力の約半分）。** testが`dagq`を1,700回余り起動し、1回ごとに約0.8MBのprofrawを書く。`LLVM_PROFILE_FILE`をpidを含まない`%Nm`だけの書式（N個のfileを共有してonline mergeする）にすれば件数は数十件に、合計は数十MBに減り、mergeは1秒前後になる見込み。ただしcargo-llvm-covが`LLVM_PROFILE_FILE`を自分で決めるので上書きの手段が要ること、同時に書くprocessが多いとfileのlockで待つことがあり、test段を延ばしうる。効果は最大でも約7秒。
2. **`llvm-profdata merge`の並列度。** cargo-llvm-covは`-num-threads`を渡しておらず、`llvm-profdata`の既定（入力が多ければhardwareの並列度）で走る。loadの高いhostでは並列度を上げても縮みにくく、下げてもほかのrunに譲れる時間はわずか。手を入れる価値は小さい。
3. **reportの対象を絞る。** `llvm-cov report`と`export`は合わせて約1.2秒で、`--ignore-filename-regex`などで対象を減らしても削れるのは1秒未満。
4. **testの一覧。** 13 binaryで3.3秒。binaryの数を増やさないこと（ADR-0078の方針）で抑えられている。

**`NEXTEST_TEST_THREADS`との関係の見立て**: test段の後はtestとCLIのprocessの数と合計サイズで決まり、並列度では変わらない（profrawはprocessごとに1件で、mergeはtestが全部終わってから1回走る）。並列度を上げても、mergeと同時に走るtestは無いので競合もしない。上げたときに効くのはtest段だけで、その床は最長のtestになる。今回の測定（並列度8）では、testの合計2,749秒÷8＝344秒に対し`Summary`は400秒で、差の大部分は最後に1件だけ走っていた`runtime_stale_receipt::a_stale_receipt_left_during_a_wait_is_unchanged`（128秒）による。このtestは直近のintegrateの50件のうち18件で127〜133秒（他の32件は7〜13秒）と二つの山に分かれており、何かの約120秒の待ちに当たっていると見られる。並列度を上げる前に、このtestの長い側を無くすほうが関門の段を確実に縮める（follow-up）。

## 5. 集計の再現

- 手元の測定: 上の(a)〜(d)をworktreeで順に流し、各行に時刻を付けた。profrawは(b)の直後に`find target/llvm-cov-target -name '*.profraw'`で数えた。
- integrateのlog: `runs/*/integrate-*-verify-*.log`のうち`Nextest run ID`を含むものについて、ファイルの作成時刻と最終更新時刻の差から、cargoの`Finished … in`（`1m 50s`の形を含む）と`Summary`の秒を引いた。旧コマンドは`Summary`の代わりに`finished in`の合計を引いた。 百分位は昇順の順位（n×p番目）で取った。
