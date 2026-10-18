---
id: plan-domain-crate-measurement
type: plan
title: domain を crate に切り出した試作（条件 B）と現状（条件 A）の、ビルド時間・テストの時間・依存の制御・変更容易さの測定
status: completed
created: 2026-10-14
owners:
  - hisamekms
tags:
  - architecture
  - build
  - measurement
related:
  - adr-t1545-1
  - adr-0013
  - plan-worktree-seed
  - plan-sccache-measurement
  - plan-architecture-boundaries
---

# domain を crate に切り出した試作（条件 B）と現状（条件 A）の、ビルド時間・テストの時間・依存の制御・変更容易さの測定

goal 121 の判断の材料（task 1721）。
`src/domain` を workspace の新しい crate `crates/dagq-domain` に切り出した試作（条件 B）を作り、同じ commit の現状（条件 A）と並べて、(1) 変更 1 回あたりのビルド時間、(2) `cargo clean` の後の build、(3) domain の単体テスト、(4) 依存の制御、(5) 変更容易さを測った・数えた。
採るかの結論は末尾の「[結論](#結論)」にある（task 1722）。

試作の src・Cargo.toml・crates の変更は着地させず、差分を [domain-crate/prototype.patch](domain-crate/prototype.patch) に残した（`git apply` で測った commit に当たる）。
測定の手順は [domain-crate/measure.sh](domain-crate/measure.sh)、各試行の値は [build-times.csv](domain-crate/build-times.csv)・[clean-builds.csv](domain-crate/clean-builds.csv)・[domain-tests.csv](domain-crate/domain-tests.csv) にある。

## 条件と環境

| 項目 | 値 |
| --- | --- |
| 測った commit | `1a7f887b0e4d4c60ee81892b1d10030faef42c5e`（A はそのまま、B はそれに [prototype.patch](domain-crate/prototype.patch) を当てたもの） |
| host | macOS 14.7.4、Apple M1、`uname -m` = `arm64`、CPU の数 8（`sysctl -n hw.ncpu`。高性能 4・高効率 4） |
| `rustc -V` | `rustc 1.98.1 (48a229cea 2026-09-01)` |
| `cargo nextest --version` | `cargo-nextest 0.9.146 (8af696ddc 2026-09-21)` |
| `cargo llvm-cov --version` | `cargo-llvm-cov 0.9.1` |
| `RUSTC_WRAPPER` | run の `dagq-rustc-wrapper`（run の runner への link。sccache 0.18.0 を包む guard で、`DAGQ_SCCACHE_PROGRAM=sccache`） |
| `CARGO_BUILD_JOBS` | `4`（ほかに `NEXTEST_TEST_THREADS=6`・`RUST_TEST_THREADS=6`） |
| 測定の開始と終了（UTC） | 1 回目 2026-10-14T14:35:06Z〜2026-10-14T15:42:12Z（(3) の全部と (1) の 30 試行）、2 回目 2026-10-16T11:42:40Z〜2026-10-16T14:00:00Z（(1) の残り 30 試行と (2) の全部）。間で run の turn が終わり測定が止まったので、A と B を同じ手順で作り直して続けた |
| 開始時の load average（1・5・15 分） | 1 回目 11.96・14.51・16.33、2 回目 8.14・8.25・8.18（host は他の run と共有） |

A と B の置き場と、そろえたこと:

- A は `git archive HEAD | tar -x -C "$TMPDIR/dagq-a"` で展開し、`git init`・`git add -A`・`git commit` で 1 つの commit の repository にした。
  B は同じものに `git apply --index prototype.patch` と `git commit` を重ねた（`$TMPDIR/dagq-b`）。
  A と B はそれぞれの `target/`（k2 は `target/llvm-cov-target`）を持ち、切り替えで互いの warm な状態を壊さない。
- 両方の `.git/packed-refs` に空の file を置いた。
  dagq の build script は `.git`・`HEAD`・index・`packed-refs`・今の branch の loose な ref を `rerun-if-changed` で見張り、名指した file が無いと cargo が毎回 stale と見て dagq の crate を毎回 compile し直す。
  `.git` の無い展開（`git archive` だけ）や `packed-refs` の無い新しい repository では、変更の無い build でも dagq が compile し直しになり、測りたい差が埋もれるためである。
  この run の worktree も branch の ref が `packed-refs` にしか無く、同じく変更の無い `cargo test --no-run` で dagq を compile し直した。
- 測っている間は A と B で `git` を打たない（index を書き直すと build script が走り直す）。
- 準備の間に取った値（`.git` の無い A と worktree の B で取った 2026-10-14 の 13:10Z までの試行）は使っていない。

## 試作（条件 B）の形と、仮に許したもの

- `src/domain/` を `crates/dagq-domain/src/` に移し（`mod.rs` は `lib.rs`）、root の `Cargo.toml` に `[workspace] members = [".", "crates/dagq-domain"]` と依存 `dagq-domain = { path = "crates/dagq-domain" }` を足した。
- `src/lib.rs` の `pub mod domain;` を `pub use dagq_domain as domain;` にした。
  application・infrastructure・compose・`src/main.rs`・`tests/` の `crate::domain::…`・`dagq::domain::…` の path は変えずに通った（`tests/` の差分は 0）。
- dagq-domain の依存は `serde`（derive）・`serde_json`・`sha2` と、下の仮の `rusqlite`。
  feature `testkit` を持ち、dagq は dev-dependency でそれを有効にする。
- 試作がまともな手間で compile しきれたので、止めた error は残っていない（下の (4) の「切り出しに要った変更」が、途中で出た error を種類ごとに直したもの）。
  `cargo check --workspace --tests` は warning 0 で通り、dagq-domain の単体テスト 1,307 本はすべて通った（(3)）。

仮に許したもの（本当の切り出しでは別の形にするもの）:

| 仮に許したもの | 理由 | 本当の切り出しでの形の候補 |
| --- | --- | --- |
| dagq-domain の通常の依存に `rusqlite` | `src/infrastructure/sql_ids.rs` の `ToSql`・`FromSql` の impl（13 の ID の型 × 2）が orphan rule に当たるので、impl を dagq-domain の `sql_ids` に移した | infrastructure に newtype を置くか、bind と読み出しを関数にする（L2 の `rusqlite` を crate の境界で強制するには、これを先に直す） |
| dagq-domain に `build_id` の 2 つの関数（`named_commit`・`is_prerelease`）の写し | domain が層の外の `crate::build_id` を 3 か所で呼んでいて、`build.rs` がその file を `#[path]` で読むので dagq-domain に依存させられない | I/O の無い部分だけを dagq-domain（か小さな共有の crate）に置き、`build.rs` もそれを読む |
| `src/build_id.rs` の `SOURCES` に `crates` を足した | build 識別子の dirty の判定と rerun に dagq-domain の変更を含めるため | 同じ（切り出すなら要る変更） |
| feature `testkit` と、`#[cfg(test)]` を `#[cfg(any(test, feature = "testkit"))]` にした 9 行 | dagq の test が domain の test 用の部品（`ledger::testkit`・`toolchain::fake`・`transcript::ANALYSED` と、それを数える呼び出し）を使っていて、`cfg(test)` は crate をまたがない | 同じか、部品を dev-dependency の小さな crate に分ける |

## (1) ビルド時間（変更 1 回あたり）

使ったコマンド（`sh measure.sh A B build-times`）:

- k1: `cargo test --locked --no-run --workspace`。
  `--workspace` は B で dagq-domain の単体テストの binary も build するために付けた（B で付けないと root の package だけになり、domain の 1,307 本の binary を作らない）。A では付けても同じ。
- k2: `eval "$(cargo llvm-cov show-env --sh)"` の環境で `CARGO_TARGET_DIR=<tree>/target/llvm-cov-target cargo nextest run --locked --workspace --no-run`。
  cargo-llvm-cov 0.9.1 の `show-env` は target dir に `target/` を出し、instrument の flag を自分の rustc wrapper の中で足すので、cargo の fingerprint には現れない。そのまま k1 と同じ dir で流すと互いの成果物を fresh と取り違えるので、`cargo llvm-cov nextest` と同じ `target/llvm-cov-target` を分けて使った。
- 変更: (a) A は `src/domain/stall.rs`、B は `crates/dagq-domain/src/stall.rs`、(b) `src/application/areas.rs`、(c) `src/lib.rs`。どれも末尾にコメント `// measure: one changed line` を 1 行足す。
- 各試行は、変更を戻した状態で同じコマンドを時間を測らずに 1 回流して warm にし、変更を足し、`/usr/bin/time -p` で包んで測り（real を wall とする）、変更を戻す。
- 1 周は (a)(b)(c) × k1・k2 の 6 つの組を A と B で 1 回ずつ（12 試行）。組ごとに奇数の周は A→B、偶数の周は B→A で、5 周。
- Compiling の crate の数は、そのコマンドの stderr の `^ *Compiling ` の行の数。

周回ごとの値（[build-times.csv](domain-crate/build-times.csv) と同じ列、60 行）と集計:

| round | order | condition | change | command | started_at | ended_at | wall_secs | compiling_crates |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 1 | A | a | k1 | 2026-10-14T14:44:00Z | 2026-10-14T14:45:22Z | 81.90 | 1 |
| 1 | 2 | B | a | k1 | 2026-10-14T14:46:15Z | 2026-10-14T14:47:22Z | 66.69 | 2 |
| 1 | 3 | A | a | k2 | 2026-10-14T14:48:54Z | 2026-10-14T14:50:09Z | 74.22 | 1 |
| 1 | 4 | B | a | k2 | 2026-10-14T14:51:07Z | 2026-10-14T14:52:16Z | 68.51 | 2 |
| 1 | 5 | A | b | k1 | 2026-10-14T14:53:47Z | 2026-10-14T14:54:27Z | 39.93 | 1 |
| 1 | 6 | B | b | k1 | 2026-10-14T14:55:10Z | 2026-10-14T14:55:37Z | 27.03 | 1 |
| 1 | 7 | A | b | k2 | 2026-10-14T14:57:02Z | 2026-10-14T14:58:09Z | 66.36 | 1 |
| 1 | 8 | B | b | k2 | 2026-10-14T14:59:05Z | 2026-10-14T15:00:00Z | 54.20 | 1 |
| 1 | 9 | A | c | k1 | 2026-10-14T15:01:30Z | 2026-10-14T15:03:24Z | 114.28 | 1 |
| 1 | 10 | B | c | k1 | 2026-10-14T15:05:07Z | 2026-10-14T15:07:06Z | 118.76 | 1 |
| 1 | 11 | A | c | k2 | 2026-10-14T15:08:40Z | 2026-10-14T15:10:09Z | 88.49 | 1 |
| 1 | 12 | B | c | k2 | 2026-10-14T15:11:10Z | 2026-10-14T15:12:09Z | 58.95 | 1 |
| 2 | 1 | B | a | k1 | 2026-10-14T15:12:59Z | 2026-10-14T15:13:59Z | 59.61 | 2 |
| 2 | 2 | A | a | k1 | 2026-10-14T15:15:01Z | 2026-10-14T15:16:08Z | 67.03 | 1 |
| 2 | 3 | B | a | k2 | 2026-10-14T15:17:31Z | 2026-10-14T15:18:38Z | 66.77 | 2 |
| 2 | 4 | A | a | k2 | 2026-10-14T15:19:26Z | 2026-10-14T15:19:59Z | 33.58 | 1 |
| 2 | 5 | B | b | k1 | 2026-10-14T15:20:46Z | 2026-10-14T15:21:09Z | 23.70 | 1 |
| 2 | 6 | A | b | k1 | 2026-10-14T15:21:53Z | 2026-10-14T15:22:28Z | 35.59 | 1 |
| 2 | 7 | B | b | k2 | 2026-10-14T15:23:15Z | 2026-10-14T15:23:53Z | 37.31 | 1 |
| 2 | 8 | A | b | k2 | 2026-10-14T15:24:31Z | 2026-10-14T15:25:18Z | 46.41 | 1 |
| 2 | 9 | B | c | k1 | 2026-10-14T15:25:48Z | 2026-10-14T15:26:14Z | 26.59 | 1 |
| 2 | 10 | A | c | k1 | 2026-10-14T15:27:00Z | 2026-10-14T15:27:38Z | 38.11 | 1 |
| 2 | 11 | B | c | k2 | 2026-10-14T15:28:12Z | 2026-10-14T15:28:40Z | 27.38 | 1 |
| 2 | 12 | A | c | k2 | 2026-10-14T15:29:26Z | 2026-10-14T15:30:18Z | 51.21 | 1 |
| 3 | 1 | A | a | k1 | 2026-10-14T15:31:01Z | 2026-10-14T15:31:38Z | 37.69 | 1 |
| 3 | 2 | B | a | k1 | 2026-10-14T15:32:26Z | 2026-10-14T15:33:21Z | 55.04 | 2 |
| 3 | 3 | A | a | k2 | 2026-10-14T15:34:48Z | 2026-10-14T15:35:56Z | 66.56 | 1 |
| 3 | 4 | B | a | k2 | 2026-10-14T15:37:10Z | 2026-10-14T15:38:17Z | 66.93 | 2 |
| 3 | 5 | A | b | k1 | 2026-10-14T15:39:37Z | 2026-10-14T15:40:42Z | 65.02 | 1 |
| 3 | 6 | B | b | k1 | 2026-10-14T15:41:37Z | 2026-10-14T15:42:11Z | 34.19 | 1 |
| 3 | 7 | A | b | k2 | 2026-10-16T11:42:42Z | 2026-10-16T11:43:59Z | 77.12 | 1 |
| 3 | 8 | B | b | k2 | 2026-10-16T11:44:02Z | 2026-10-16T11:45:02Z | 59.00 | 1 |
| 3 | 9 | A | c | k1 | 2026-10-16T11:45:51Z | 2026-10-16T11:46:50Z | 58.84 | 1 |
| 3 | 10 | B | c | k1 | 2026-10-16T11:47:15Z | 2026-10-16T11:48:04Z | 48.75 | 1 |
| 3 | 11 | A | c | k2 | 2026-10-16T11:50:14Z | 2026-10-16T11:51:50Z | 95.00 | 1 |
| 3 | 12 | B | c | k2 | 2026-10-16T11:53:14Z | 2026-10-16T11:54:00Z | 45.66 | 1 |
| 4 | 1 | B | a | k1 | 2026-10-16T11:54:58Z | 2026-10-16T11:56:50Z | 112.41 | 2 |
| 4 | 2 | A | a | k1 | 2026-10-16T11:59:23Z | 2026-10-16T12:00:14Z | 51.21 | 1 |
| 4 | 3 | B | a | k2 | 2026-10-16T12:01:02Z | 2026-10-16T12:01:49Z | 47.07 | 2 |
| 4 | 4 | A | a | k2 | 2026-10-16T12:02:58Z | 2026-10-16T12:03:55Z | 56.84 | 1 |
| 4 | 5 | B | b | k1 | 2026-10-16T12:05:07Z | 2026-10-16T12:06:18Z | 70.80 | 1 |
| 4 | 6 | A | b | k1 | 2026-10-16T12:07:58Z | 2026-10-16T12:09:19Z | 80.50 | 1 |
| 4 | 7 | B | b | k2 | 2026-10-16T12:10:49Z | 2026-10-16T12:11:58Z | 68.34 | 1 |
| 4 | 8 | A | b | k2 | 2026-10-16T12:13:23Z | 2026-10-16T12:17:59Z | 275.91 | 1 |
| 4 | 9 | B | c | k1 | 2026-10-16T12:18:55Z | 2026-10-16T12:19:43Z | 48.64 | 1 |
| 4 | 10 | A | c | k1 | 2026-10-16T12:20:56Z | 2026-10-16T12:22:04Z | 67.99 | 1 |
| 4 | 11 | B | c | k2 | 2026-10-16T12:22:55Z | 2026-10-16T12:23:42Z | 47.29 | 1 |
| 4 | 12 | A | c | k2 | 2026-10-16T12:24:59Z | 2026-10-16T12:25:52Z | 52.81 | 1 |
| 5 | 1 | A | a | k1 | 2026-10-16T12:26:30Z | 2026-10-16T12:27:00Z | 29.97 | 1 |
| 5 | 2 | B | a | k1 | 2026-10-16T12:27:22Z | 2026-10-16T12:28:07Z | 44.87 | 2 |
| 5 | 3 | A | a | k2 | 2026-10-16T12:28:54Z | 2026-10-16T12:31:32Z | 156.83 | 1 |
| 5 | 4 | B | a | k2 | 2026-10-16T12:33:32Z | 2026-10-16T12:35:14Z | 101.14 | 2 |
| 5 | 5 | A | b | k1 | 2026-10-16T12:36:47Z | 2026-10-16T12:41:19Z | 271.40 | 1 |
| 5 | 6 | B | b | k1 | 2026-10-16T12:43:43Z | 2026-10-16T12:45:02Z | 78.15 | 1 |
| 5 | 7 | A | b | k2 | 2026-10-16T12:50:15Z | 2026-10-16T12:52:55Z | 157.85 | 1 |
| 5 | 8 | B | b | k2 | 2026-10-16T12:54:30Z | 2026-10-16T12:55:24Z | 53.44 | 1 |
| 5 | 9 | A | c | k1 | 2026-10-16T12:57:01Z | 2026-10-16T12:58:20Z | 78.75 | 1 |
| 5 | 10 | B | c | k1 | 2026-10-16T12:59:35Z | 2026-10-16T13:01:04Z | 88.64 | 1 |
| 5 | 11 | A | c | k2 | 2026-10-16T13:03:56Z | 2026-10-16T13:05:09Z | 72.41 | 1 |
| 5 | 12 | B | c | k2 | 2026-10-16T13:05:54Z | 2026-10-16T13:07:15Z | 80.73 | 1 |

| 条件 | 変更 | コマンド | n | 中央値 | 最小 | 最大 | 比 B/A | 差 B−A | Compiling |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| A | a | k1 | 5 | 51.21 | 29.97 | 81.90 | — | — | 1 |
| B | a | k1 | 5 | 59.61 | 44.87 | 112.41 | 1.16 | 8.40 | 2 |
| A | a | k2 | 5 | 66.56 | 33.58 | 156.83 | — | — | 1 |
| B | a | k2 | 5 | 66.93 | 47.07 | 101.14 | 1.01 | 0.37 | 2 |
| A | b | k1 | 5 | 65.02 | 35.59 | 271.40 | — | — | 1 |
| B | b | k1 | 5 | 34.19 | 23.70 | 78.15 | 0.53 | -30.83 | 1 |
| A | b | k2 | 5 | 77.12 | 46.41 | 275.91 | — | — | 1 |
| B | b | k2 | 5 | 54.20 | 37.31 | 68.34 | 0.70 | -22.92 | 1 |
| A | c | k1 | 5 | 67.99 | 38.11 | 114.28 | — | — | 1 |
| B | c | k1 | 5 | 48.75 | 26.59 | 118.76 | 0.72 | -19.24 | 1 |
| A | c | k2 | 5 | 72.41 | 51.21 | 95.00 | — | — | 1 |
| B | c | k2 | 5 | 47.29 | 27.38 | 80.73 | 0.65 | -25.12 | 1 |

比 = B の中央値 ÷ A の中央値（1 より小さいと B が速い）、差 = B の中央値 − A の中央値（秒）。
Compiling の crate の数は 5 周とも同じだったので、範囲を書いていない。

同じ周の A と B の組で B が速かった周の数（5 周のうち）: (a) k1 2・k2 3、(b) k1 5・k2 5、(c) k1 3・k2 4。

読み方:

- (a) domain の変更では B が速くならない。B は dagq-domain と dagq の 2 つを compile し直し（Compiling 2）、中央値は k1 で 8.4 秒長く、k2 でほぼ同じだった。dagq が dagq-domain に依るので、domain を変えると dagq も作り直しになる。
- (b) application の変更では、5 周とも B が速かった（k1 で中央値 30.8 秒・k2 で 22.9 秒短い）。B では dagq-domain が fresh のまま残り、dagq の crate は domain の約 13 万行を含まない分小さい。
- (c) `src/lib.rs` の変更も B が速い側に寄った（k1 で 19.2 秒・k2 で 25.1 秒短い）が、周によって逆になった（k1 は 5 周中 3 周）。
- host は他の run と共有で、同じセルの中でも最大が最小の 2〜7 倍に広がった（A の (b) k1 の 271 秒など）。比は中央値どうしで出したが、揺れの大きさに比べて (a) と (c) の差は小さい。
- 試行は 2026-10-14（1〜3 周の途中まで）と 2026-10-16（残り）に分かれた。間で A と B を同じ手順で作り直したが、条件の中身（commit・patch・コマンド）は同じで、どちらの日も同じ周の中で A と B を交互に流している。

## (2) `cargo clean` の後の build

使ったコマンド（`sh measure.sh A B clean-builds`）:

- 3 つの条件: A は `cargo clean -p dagq`、B は `cargo clean -p dagq -p dagq-domain`、B-dagq-only は B の tree で `cargo clean -p dagq` だけ。
- 各試行は、時間を測らずに `cargo test --locked --no-run --workspace` で全部を build 済みにし、clean し、同じコマンドの wall と Compiling の crate の数を測る（`--workspace` の理由は (1) と同じ）。
- 順序は 1 周目 A→B→B-dagq-only、2 周目 B→B-dagq-only→A、3 周目 B-dagq-only→A→B と巡回させ、3 周。

周回ごとの値（[clean-builds.csv](domain-crate/clean-builds.csv) と同じ列、9 行）と集計:

| round | order | condition | change | command | started_at | ended_at | wall_secs | compiling_crates |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 1 | A | none | k1 | 2026-10-16T13:08:04Z | 2026-10-16T13:11:46Z | 221.90 | 1 |
| 1 | 2 | B | none | k1 | 2026-10-16T13:12:45Z | 2026-10-16T13:16:21Z | 215.56 | 2 |
| 1 | 3 | B-dagq-only | none | k1 | 2026-10-16T13:16:23Z | 2026-10-16T13:18:45Z | 142.43 | 1 |
| 2 | 1 | B | none | k1 | 2026-10-16T13:18:47Z | 2026-10-16T13:24:22Z | 334.36 | 2 |
| 2 | 2 | B-dagq-only | none | k1 | 2026-10-16T13:24:23Z | 2026-10-16T13:27:51Z | 208.06 | 1 |
| 2 | 3 | A | none | k1 | 2026-10-16T13:27:55Z | 2026-10-16T13:34:19Z | 383.54 | 1 |
| 3 | 1 | B-dagq-only | none | k1 | 2026-10-16T13:34:23Z | 2026-10-16T13:40:59Z | 395.93 | 1 |
| 3 | 2 | A | none | k1 | 2026-10-16T13:41:02Z | 2026-10-16T13:47:41Z | 399.28 | 1 |
| 3 | 3 | B | none | k1 | 2026-10-16T13:47:44Z | 2026-10-16T14:00:00Z | 736.34 | 2 |

| 条件 | 変更 | コマンド | n | 中央値 | 最小 | 最大 | 比（÷A） | 差 | Compiling |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| A | none | k1 | 3 | 383.54 | 221.90 | 399.28 | — | — | 1 |
| B | none | k1 | 3 | 334.36 | 215.56 | 736.34 | 0.87 | 126.30（B − B-dagq-only） | 2 |
| B-dagq-only | none | k1 | 3 | 208.06 | 142.43 | 395.93 | 0.54 | — | 1 |

比はそれぞれの中央値 ÷ A の中央値（B ÷ A = 0.87、B-dagq-only ÷ A = 0.54）、差 B − B-dagq-only = 126.30 秒は B の中央値 − B-dagq-only の中央値で、dagq-domain を作り直す分の目安。

読み方:

- Compiling の行は、A は 3 周とも `dagq` だけ、B は `dagq-domain` と `dagq`、B-dagq-only は `dagq` だけだった（依存 crate は clean していないので fresh）。
- dagq-domain が残っていれば（B-dagq-only）、dagq の crate だけの作り直しは A の約半分（中央値 208 秒と 384 秒）になった。
  B で両方を作り直すと、中央値では A より 13% 短いが、3 周目の 736 秒のように揺れが大きく、A との差は負荷の揺れに比べて小さい。
- 3 周目は 3 条件とも 1・2 周目より長く、host の負荷が上がった時間に当たった。巡回の順序なので、どの条件も 1 回ずつ遅い位置に入っている。

### worktree seed の前提が B で変わるか

[worktree-seed](worktree-seed.md) は、seed の target を clone しても dagq の crate はいつも作り直しになると測った。
理由は dagq の build script が絶対 path と worktree ごとに違う Git の file（`HEAD`・index・branch の ref）を `rerun-if-changed` で見張り、clone 先で走り直すためで、build script の無い crate（当時の `dagq-broker-protocol`）は mtime をそろえた条件で fresh に残った。
判断:

- dagq-domain は build script を持たない（prototype.patch の `crates/dagq-domain/Cargo.toml` に `build` が無く、`build.rs` も無い）。
  なので、mtime をそろえた seed（その文書の (c)）では dagq-domain は fresh に残せる範囲に入る見込みで、seed の効果の前提（dagq の crate 自身は作り直し）は、作り直す範囲が dagq だけに縮む形で変わる。
- そのとき縮む量の目安が B − B-dagq-only（中央値で約 126 秒、clean からの build）で、dagq の crate だけの作り直しは B-dagq-only の 208 秒（中央値）になる。
  seed から incremental が当たる場合の量は測っていない。
- mtime をそろえない clone（その文書の (b)）では、dagq-domain の source も checkout の時刻を持つので dirty になり、変わらない。
- dagq の build script が Git の file を見張る限り dagq の crate はいつも作り直しになる点は B でも同じで、B では `src/build_id.rs` の見張る範囲に `crates` が加わる。

## (3) domain の単体テスト

### test の数（静的・1 回）

`grep -rE '#\[(tokio::)?test\]' src/domain | wc -l` を A で 1 回数えた: **1,306**。
同じ commit なら値が変わらない静的な数え上げなので、周回は当たらない。

### 時間（周回・交互）

使ったコマンド（`sh measure.sh A B domain-tests`）:

- 各試行は、(a) の変更を戻した状態で build の段のコマンドを時間を測らずに 1 回流して warm にし、(a)（`stall.rs` の末尾にコメントを 1 行）を足し、build の段と run の段を続けて測り、変更を戻す。
- build の段: A は `cargo nextest run --locked --no-run --lib -E 'test(/^domain::/)'`、B は `cargo nextest run --locked --no-run -p dagq-domain`。
- run の段: 同じ絞り込みの `cargo nextest run --locked`（build 済みなので実行だけ）。build と run の区切りは、この 2 つのコマンドの境目。
- 順序は奇数の周 A→B、偶数の周 B→A で 3 周。CSV の `command` は `nextest`、`phase` が段。

周回ごとの値（[domain-tests.csv](domain-crate/domain-tests.csv) と同じ列、12 行）と集計:

| round | order | condition | change | command | phase | started_at | ended_at | wall_secs | compiling_crates |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 1 | A | a | nextest | build | 2026-10-14T14:35:06Z | 2026-10-14T14:36:00Z | 54.08 | 1 |
| 1 | 1 | A | a | nextest | run | 2026-10-14T14:36:00Z | 2026-10-14T14:36:04Z | 4.10 | 0 |
| 1 | 2 | B | a | nextest | build | 2026-10-14T14:38:13Z | 2026-10-14T14:38:33Z | 20.47 | 1 |
| 1 | 2 | B | a | nextest | run | 2026-10-14T14:38:33Z | 2026-10-14T14:38:41Z | 8.16 | 0 |
| 2 | 1 | B | a | nextest | build | 2026-10-14T14:38:59Z | 2026-10-14T14:39:17Z | 17.80 | 1 |
| 2 | 1 | B | a | nextest | run | 2026-10-14T14:39:17Z | 2026-10-14T14:39:26Z | 8.86 | 0 |
| 2 | 2 | A | a | nextest | build | 2026-10-14T14:40:12Z | 2026-10-14T14:40:41Z | 28.52 | 1 |
| 2 | 2 | A | a | nextest | run | 2026-10-14T14:40:41Z | 2026-10-14T14:40:51Z | 9.87 | 0 |
| 3 | 1 | A | a | nextest | build | 2026-10-14T14:41:33Z | 2026-10-14T14:42:06Z | 32.51 | 1 |
| 3 | 1 | A | a | nextest | run | 2026-10-14T14:42:06Z | 2026-10-14T14:42:15Z | 8.48 | 0 |
| 3 | 2 | B | a | nextest | build | 2026-10-14T14:42:49Z | 2026-10-14T14:43:06Z | 16.42 | 1 |
| 3 | 2 | B | a | nextest | run | 2026-10-14T14:43:06Z | 2026-10-14T14:43:13Z | 7.57 | 0 |

| 条件 | 変更 | 段 | n | 中央値 | 最小 | 最大 | 比 B/A | 差 B−A |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| A | a | build | 3 | 32.51 | 28.52 | 54.08 | — | — |
| B | a | build | 3 | 17.80 | 16.42 | 20.47 | 0.55 | -14.71 |
| A | a | run | 3 | 8.48 | 4.10 | 9.87 | — | — |
| B | a | run | 3 | 8.16 | 7.57 | 8.86 | 0.96 | -0.32 |
| A | a | 合計 | 3 | 40.99 | 38.39 | 58.18 | — | — |
| B | a | 合計 | 3 | 26.66 | 23.99 | 28.63 | 0.65 | -14.33 |

合計は周ごとに build + run を足した値の中央値・最小・最大。比 = B の中央値 ÷ A の中央値、差 = B の中央値 − A の中央値（秒）。

実行した test の数（nextest の Summary）:

- A: 3 周とも `1306 tests run: 1306 passed, 1096 skipped`（skipped は `--lib` の binary のうち domain の外の test）。
- B: 3 周とも `1307 tests run: 1307 passed, 0 skipped`。
- 1 本の差は、試作で `src/infrastructure/sql_ids.rs` を dagq-domain に移したため、その test `sql_ids::tests::ids_round_trip_through_sqlite_and_are_checked_on_the_way_back` が B では dagq-domain の test に入ったこと（名前の集合を比べて確かめた。ほかの 1,306 本は同じ名前）。

読み方:

- build の段は B が約半分（中央値で 14.7 秒短い）。A は domain の test を回すにも dagq の lib test の binary（domain の外の 1,096 本を含む全体）を作り直すが、B は dagq-domain の test の binary だけを作り直す。
- run の段は同じ大きさ（どちらも 1,306〜1,307 本で 8 秒前後）。A の 1 周目の 4.10 秒は他より短く、負荷の揺れの範囲とみる。
- tests/it は変わらない: 試作の差分は `tests/` に触れず（prototype.patch に `tests/` の file は無い）、it の binary は B でも dagq（と、それが使う dagq-domain）に対して build される。domain だけを変えた後の it の build は、dagq が dagq-domain に依るので作り直しになる（(1) の (a)）。

## (4) 依存の制御

静的な数え上げで、同じ commit と試作なら何度数えても同じ値になり、実行の順序や host の負荷の揺れが値に入らない。
そのため 1 回だけ数え、周回と交互は当たらない。

### B の dagq-domain の依存

| 依存 | 種類 | 使う所 |
| --- | --- | --- |
| `serde`（`derive`） | 通常 | 値の型の Serialize・Deserialize |
| `serde_json` | 通常 | payload と記録の読み書き |
| `sha2` | 通常 | 指紋（hash） |
| `rusqlite` | 通常（仮） | ID の型の `ToSql`・`FromSql`（上の「仮に許したもの」） |
| feature `testkit` | dagq の dev-dependency が有効にする | dagq の test が使う domain の test の部品 |

dev-dependency は無い（domain の単体テストは std と上の依存だけで通る）。

### `scripts/check-layer-deps.sh` の L1・L2 と crate の境界

| 規則 | 禁止するもの | crate の境界で強制されるか | script が残るか |
| --- | --- | --- | --- |
| L1 | domain から `crate::application`・`crate::infrastructure`・`crate::compose` | される。dagq-domain は dagq に依存できない（循環になる）ので、名指すと compile が通らない。test の中も同じ | 残らない |
| L1 | domain から層の外の `crate::view`・`crate::runtime`・`crate::lifecycle` | される（同上） | 残らない |
| （今の規則に無い） | domain から層の外の `crate::build_id`（今は許されている） | される（同上）。試作は写しで避けた | — |
| L2 | `rusqlite` | 依存に書かなければされる。試作は `sql_ids` のために仮に依存に入れたので、されない | `sql_ids` を domain の外へ戻すまでは残る |
| L2 | `anyhow` | される（依存に無い） | 残らない |
| L2 | `Uuid::new_v4` | される（`uuid` が依存に無い） | 残らない |
| L2 | `std::fs`・`std::process`・`std::net` | されない（std は全ての crate が使える） | 残る（dagq-domain の clippy の `disallowed_methods`・`disallowed_types` に移す道もある） |
| L2 | `SystemTime::now`・`Instant::now` | されない（std） | 残る（同上） |

- 今の script は L2 を test の中で数えないが、crate の依存は test にも効く（dev-dependency に足さない限り）。
- script は `src/domain/` を見るので、B では L1・L2 の検査の対象が無くなり、何も数えずに通る（B の worktree で `sh scripts/check-layer-deps.sh` は ok だった）。
  切り出すなら、std の規則（L2 の残り）を `crates/dagq-domain/src/` に向け直す変更が要る。

### 切り出しに要った変更

試作で `cargo check -p dagq-domain --tests`、続けて `cargo check --workspace --tests` を流し、出た error を種類ごとに直した。
`crate::domain::` の書き換え・`include_str!` の path・`build_id` の写しは compile の前に機械的に直したので error に出ていない。
dagq-domain だけの check は error 0、workspace の check は lib で E0117 が 26、lib test で 34（E0117 26・E0425 6・E0432 1・E0433 1）、その後の check で E0603 が 4（test の部品の可視性）だった。

| 種類 | 数 | 数えたコマンド |
| --- | --- | --- |
| domain から `crate::application` 等への参照（`.config/layer-deps-allow.txt` の L1） | 0（allow list に L1 の行は無い。description が挙げた task 1616 の項目は base で既に無かった） | `git show HEAD:.config/layer-deps-allow.txt \| grep -vE '^#\|^$' \| wc -l` → 0、`sh scripts/check-layer-deps.sh` → ok |
| 　うち doc comment の中の `crate::application`・`crate::infrastructure`（compile は通るが rustdoc の link は切れる） | 4 | `git grep -nE 'crate::(application\|infrastructure\|compose\|view\|runtime\|lifecycle)' HEAD -- src/domain` |
| domain から層の外の `crate::build_id` の呼び出し | 3（ほかに doc comment の link 2） | `git grep -n 'crate::build_id' HEAD -- src/domain` |
| `anyhow`（L2、allow list の L2） | 0（allow list に L2 の行は無い。task 1617 の項目も base で既に無かった） | `git grep -n anyhow HEAD -- src/domain \| wc -l` → 0 |
| domain の外で外部の trait を domain の型に実装していて orphan rule に当たる箇所 | 26（13 の ID の型 × `ToSql`・`FromSql`。全部 `src/infrastructure/sql_ids.rs`） | `git show HEAD:src/infrastructure/sql_ids.rs \| grep -cE '^impl (ToSql\|FromSql) for'`、`cargo check --workspace --tests` の E0117 |
| domain の外から使われていて `pub(crate)` から `pub` にする項目（本番のコード） | 0（domain の `pub(crate)` は test の部品を含めて 49 行。うち次の行の test の部品 16 行を除く 33 行（`kpi/report/tests.rs` の 5 行を含む）は、domain の外から使われていない） | `git grep -c 'pub(crate)' HEAD -- src/domain` の合計 49、test の部品の 16 行を `pub` にした B の compile が `pub(crate)` で落ちないこと |
| 同（test の部品） | 16（`ledger::testkit` の 14 項目と、`testkit`・`toolchain::fake` の 2 module） | `git grep -c 'pub(crate)' HEAD -- src/domain/ledger/testkit.rs` → 14、E0603 4・E0432 1・E0433 1 |
| `#[cfg(test)]` を crate の外の test に見せる（feature `testkit`） | 9 行（5 file） | `grep -c '^+.*feature = "testkit"' docs/plans/domain-crate/prototype.patch` |
| domain の中の `crate::domain::` を `crate::` に書き換え（機械的） | 471 か所・145 file | `git grep -o 'crate::domain' HEAD -- src/domain \| wc -l`、`git grep -l … \| wc -l` |
| `include_str!` の相対 path（深さが 1 段変わる） | 5 | `grep -c '^+.*include_str!("../../../../' docs/plans/domain-crate/prototype.patch` |
| domain の外（application・infrastructure・compose・main・tests）の path の書き換え | 0（`pub use dagq_domain as domain;` で保った） | `grep '^diff --git' docs/plans/domain-crate/prototype.patch` に `src/infrastructure/sql_ids.rs`・`src/lib.rs`・`src/build_id.rs` の 3 file だけ |
| Cargo.toml | root に 3 か所（`[workspace]`・依存・dev-dependency）と、新しい crate の Cargo.toml | prototype.patch |

## (5) 変更容易さ

静的な数え上げで、同じ commit と期間なら何度数えても同じ値になり、実行の順序や host の負荷の揺れが値に入らない。
そのため 1 回だけ数え、周回と交互は当たらない。

数えた期間は 2026-09-01 から測った commit（`1a7f887b`、2026-10-14T10:54:54Z）までの 44 日。
この repository の最初の commit は 2026-09-22、`src/domain` を触った最初の commit は 2026-09-23 なので、中身のある期間は 2026-09-23〜10-14 の 22 日である。

| 指標 | 値 |
| --- | --- |
| `src/domain` を触った commit の数 N | 457 |
| そのうち `src/domain` の外の `src` も触った commit の数 M（B では crate をまたぐ変更） | 419 |
| 割合 M ÷ N | 91.7% |
| `src/domain` だけを触った commit | 38 |

数えたコマンド:

```sh
git log 1a7f887b --since=2026-09-01T00:00:00Z --format='@%H' --name-only -- src \
  | awk '/^@/{if(c!="" && d){n++; if(o) m++} c=$0; d=0; o=0; next}
         /^src\/domain\//{d=1; next} /^src\//{o=1}
         END{if(c!="" && d){n++; if(o) m++} print "N=" n, "M=" m}'
```

B でこうした変更に増える手順:

- domain に足した項目を dagq から使うなら `pub` にする（`pub(crate)` は dagq から見えない）。
  今の domain の `pub(crate)` は test の部品を含めて 49 行で、本番のコードの項目は domain の外から使われていないので、本番のコードで今の時点で `pub` にするものは無い。
  dagq の test が使う test の部品の 16 行（`ledger::testkit` の 14 項目と `testkit`・`toolchain::fake` の 2 module）は、試作で `pub` にした（(4) の「切り出しに要った変更」）。
- 再公開は `src/lib.rs` の `pub use dagq_domain as domain;` の 1 行で済み、項目ごとの再公開は要らない（`crate::domain::…` の path は変わらない）。
- domain に新しい外部 crate を使うときだけ、`crates/dagq-domain/Cargo.toml` に依存を足す（L2 の禁止に当たらないものに限る）。
- domain の test の部品を dagq の test から使うなら、feature `testkit` の下に置く。
- domain に外部の trait（`rusqlite` の `ToSql` など）を実装するなら、orphan rule のため domain の側か newtype に置く。
- 変更は 1 つの commit の中で両方の crate を直せるので、commit や着地の数は増えない。
  dagq-domain の変更は dagq の再 compile も起こす（(1) の (a)）。

衝突の集中（`application/ports.rs`・`main.rs`・`application/supervise`）は dagq の crate の中にあり、domain を切り出しても減らない。
それは goal 100（context ごとの分割）が受け持つ前提で、この測定はそれを数えていない。

## 観点ごとの要約

- **ビルド時間**: application か `src/lib.rs` を変えた後の再 compile は B が短い側に寄った（中央値の比 0.53〜0.72、(b) は 5 周とも B が速い）。domain を変えた後は dagq-domain と dagq の両方を作り直すので速くならない（比 1.16 と 1.01）。
  llvm-cov の instrument の build（k2）も同じ傾向（比 (a) 1.01・(b) 0.70・(c) 0.65）。
  clean の後は、dagq-domain が残れば dagq の作り直しが A の約半分（比 0.54）、両方なら比 0.87。
  build script の無い dagq-domain は mtime をそろえた worktree seed で fresh に残せる見込みで、seed の前提（dagq の crate は作り直し）は作り直す範囲が縮む形で変わる。
  host の負荷の揺れが大きく（同じセルで 2〜7 倍）、(a)(c) と clean の B ÷ A は揺れより小さい差である。
- **依存の制御**: L1（domain から application・infrastructure などへの参照）は crate の境界で compile が強制し、script が要らなくなる。
  L2 のうち `anyhow`・`Uuid::new_v4` は依存に無いので強制され、`rusqlite` は試作で仮に依存に入れた（orphan rule に当たる `sql_ids` の impl 26 個）ので、それを domain の外へ戻すまで強制されない。
  std の `std::fs`・`std::process`・`std::net`・`SystemTime::now`・`Instant::now` は crate の境界では強制できず、script（か dagq-domain の clippy の設定）が残る。
  切り出しに要った手間は、機械的な path の書き換え 471 か所と `include_str!` 5、`build_id` の 2 関数の写し、orphan rule の 26、test の部品の公開 16 と `cfg` 9 行で、本番のコードの `pub(crate)`→`pub` は 0、domain の外の path の書き換えは 0 だった。
- **変更容易さ**: 期間（2026-09-01〜10-14、中身は 22 日）に `src/domain` を触った 457 commit のうち 419（91.7%）が domain の外の `src` も触っていた。
  B ではそうした変更に、dagq から使う項目を `pub` にすること（今は本番のコードで該当 0、dagq の test が使う test の部品で 16）と、新しい外部 crate を使うときの dagq-domain の Cargo.toml が加わる。再公開は `pub use` の 1 行で済む。
  衝突の集中（ports.rs・main.rs・supervise）は crate 分割では減らず、goal 100 が受け持つ前提。
- **テスト**: domain の単体テスト（`#[test]` 1,306）を domain の変更の後に回すと、build の段が B で約半分（中央値 32.5→17.8 秒、比 0.55）、run の段は同じ（比 0.96）、合計で比 0.65。
  B の 1 本の増加は `sql_ids` の test が dagq-domain に移ったため。tests/it は変わらない。
- 測れなかった項目は無い（試作は compile しきれ、(1)〜(5) を全て測った）。

## 結論

**今は domain を crate に切り出さない。** ADR は書かず、ADR-0013 と ADR-t1545-1 の「crate は 1 つのまま」は変えない。
試作の B は compile し、(1)〜(5) は全て測れているので、未確認の観点は無い。
得はあるが、domain を触る変更で消え、残る得が host の負荷の揺れより小さく、依存の制御は今の script が既に違反 0 で守っているものを compile に移すだけで、一度きりの切り出しの手間と、domain を触る変更の 9 割が crate をまたぐことに見合わないと判断した（本番のコードで今 `pub` にする項目は 0 で、commit や着地の数は増えない）。

### 観点ごとの得と手間

| 観点 | 得（測った値） | 手間・得の消える所（測った値） |
| --- | --- | --- |
| ビルド時間 | application を変えた後の再 compile は 5 周とも B が速い（中央値の比 k1 0.53・k2 0.70、差 −30.8 秒・−22.9 秒）。`src/lib.rs` の変更も比 0.72・0.65 | domain を変えた後は B が速くならない（比 k1 1.16・k2 1.01、Compiling 2）。下の重みでは、変更 1 回あたりの短縮の見込みは k1 で約 9 秒（A の約 15%）、k2（llvm-cov の build）で約 10 秒（約 14%）で、同じセルの中の揺れ（最大が最小の 2〜7 倍）より小さい。`src/lib.rs` の変更は 5 周中 k1 で 2 周 B が遅い。clean の後は両方を作り直すと比 0.87 で揺れの範囲 |
| ビルド時間（worktree seed） | dagq-domain は build script を持たないので、mtime をそろえた seed なら fresh に残せる見込みで、dagq だけの作り直しは中央値 208 秒（A は 384 秒、差 B − B-dagq-only 126 秒） | seed は本番に入っておらず（[worktree-seed](worktree-seed.md) の見積もりのまま）、この得は今は出ない |
| 依存の制御 | L1 の全部と L2 の `anyhow`・`Uuid::new_v4` は crate の境界で compile が強制する | 今の allow list は L1・L2 とも 0 行で、`scripts/check-layer-deps.sh` が既に違反 0 で守っている（切り出しで新たに防げる違反は今は無い）。L2 の `rusqlite` は orphan rule に当たる `sql_ids` の impl 26 個を domain の外へ戻すまで強制されず、std の `std::fs`・`std::process`・`std::net`・`SystemTime::now`・`Instant::now` は切り出しても script か clippy の設定が残る |
| 変更容易さ | 再公開は `pub use dagq_domain as domain;` の 1 行で、domain の外の path の書き換えは 0、本番のコードの `pub(crate)`→`pub` も今は 0 | `src/domain` を触った 457 commit のうち 419（91.7%）が domain の外の `src` も触り、B ではどれも crate をまたぐ変更になる（dagq から使う項目の `pub`、test の部品の feature `testkit`、新しい外部 crate の Cargo.toml）。一度きりの手間は path の書き換え 471 か所・145 file、`include_str!` 5、`build_id` の 2 関数の写し、orphan rule の 26、test の部品の公開 16 と `cfg` 9 行。衝突の集中（ports.rs・main.rs・supervise）は減らない |
| テスト | domain の単体テスト（`#[test]` 1,306）を domain の変更の後に回すと、build の段が比 0.55（中央値 32.5→17.8 秒、差 −14.7 秒）、合計で比 0.65（−14.3 秒） | run の段は同じ（比 0.96）。得は domain の test だけを回すときのもので、dagq の lib test と tests/it を build する検証では、domain を変えた後に dagq も作り直す（(1) の (a)）ので残らない。tests/it は変わらない |

ビルド時間の重み: 同じ期間（2026-09-01〜測った commit）に `src` を触った 811 commit のうち、`src/domain` を触ったのが 457（56.4%）、`src/domain` を触らず `src` の他を触ったのが 354（43.6%）。
domain を触る変更を (a)、触らない変更を (b) の中央値で代表させて、短縮の見込み = 0.564 ×（(a) の差）+ 0.436 ×（(b) の差）とした（k1: 0.564 × 8.40 + 0.436 × (−30.83) = −8.7 秒、A の重み付きの中央値 57.2 秒に対し約 15%。k2: 0.564 × 0.37 + 0.436 × (−22.92) = −9.8 秒、71.2 秒に対し約 14%）。
数えたコマンドは (5) と同じ形で、domain を触らない commit も数えた:

```sh
git log 1a7f887b --since=2026-09-01T00:00:00Z --format='@%H' --name-only -- src \
  | awk '/^@/{if(c!=""){t++; if(d) n++; else if(o) x++} c=$0; d=0; o=0; next}
         /^src\/domain\//{d=1; next} /^src\//{o=1}
         END{if(c!=""){t++; if(d) n++; else if(o) x++} print "T=" t, "N=" n, "X=" x}'
```

結果は `T=811 N=457 X=354`（2026-10-18 に数えた）。
このコマンドは測った commit `1a7f887b` に固定してあり、この値を再現するためのものである（いつ流しても同じ値になる）。

### 見直す条件

次のどれかが起きたら、[domain-crate/measure.sh](domain-crate/measure.sh) と [prototype.patch](domain-crate/prototype.patch)（当たらなければ同じ形で作り直す）で (1)〜(3) を測り直し、もう一度決める。

- mtime をそろえた worktree seed（または dagq の crate だけを作り直す seed）が本番に入ったとき。dagq-domain が fresh に残るなら、run ごとの最初の build で B − B-dagq-only（今は中央値 126 秒）が得になり、上の重みの判断が変わる。
- `src` を触る commit のうち `src/domain` を触らない割合 X ÷ T が 70% 以上になったとき（今は 43.6%）。70% では今の中央値で k1 の短縮の見込みが約 19 秒になり、揺れより大きくなりうる。
  見直すときは、上のコマンドの `1a7f887b` を見直す時点の `main` の最新の commit に、`--since` をその commit の時刻の 30 日前に替えて数える（直近の変更の傾向を見るため。今の 43.6% は 2026-09-01〜`1a7f887b` の値で、中身のある期間は 22 日）:

  ```sh
  head=$(git rev-parse main)
  since=$(date -u -r $(( $(git log -1 --format=%ct "$head") - 30*86400 )) +%Y-%m-%dT%H:%M:%SZ)
  git log "$head" --since="$since" --format='@%H' --name-only -- src \
    | awk '/^@/{if(c!=""){t++; if(d) n++; else if(o) x++} c=$0; d=0; o=0; next}
           /^src\/domain\//{d=1; next} /^src\//{o=1}
           END{if(c!=""){t++; if(d) n++; else if(o) x++} print "T=" t, "N=" n, "X=" x}'
  ```

  `date -u -r` は macOS の形で、GNU の `date` では `-d @<秒>` にする。数えた commit・期間・T・N・X を、この文書か測り直しの文書に残す。
- `.config/layer-deps-allow.txt` に L1 か L2 の行が戻る、または script をすり抜けた domain の禁止依存が着地したとき。compile で強制する得が、今の「違反 0 の検査を移すだけ」から変わる。

context ごとの crate はこの結論の範囲外で、決めていない。
