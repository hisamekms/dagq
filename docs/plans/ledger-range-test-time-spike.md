---
id: plan-ledger-range-test-time-spike
type: plan
title: スパイク：台帳の範囲の読みの test を 30 と 300 にし、しきい値を引数にした後でも、その test が着地の検証と CI の時間を重く延ばすか
status: completed
created: 2026-11-16
owners:
  - hisamekms
tags:
  - planning
  - measurement
  - testing
related:
  - plan-kpi-read-memory-spike
  - plan-it-coverage-map-linux-spike
---

# スパイク：台帳の範囲の読みの test は着地の検証と CI の時間をまだ重く延ばすか

問いは「30 と 300 にする task 4942、しきい値を引数にする task 4943、goal 271 の task 4133・4134 が着地した後でも、台帳の範囲の読みと作りの test（以下「範囲の test」）は着地の検証と CI の時間をまだ重く延ばすか。延ばすなら、台帳の読みのコードを変えた run の着地と CI だけで流す仕組みが要り、どの形か」。

判定は **holds**（下の「判定と根拠」）。3 成分（着地・CI の macOS・CI の Linux）のどれも、2 回とも下限で基準に当たった。

production と test のコードは変えていない。手元で流した test は下の対応表の範囲の testcase と混在の testcase だけで、unit test 全件・全体の `cargo test`・`cargo llvm-cov` は流していない。全件の値は main の CI（ci.yml）の JUnit から取った。

## 対象の対応表

範囲は次の source の test の case: `src/infrastructure/runtime_store/ledger/**`、`src/infrastructure/store_contract/ledger_*.rs`、`src/domain/ledger/**`、`src/domain/kpi/**`、`src/application/supervise/observation/ledger/**`、`src/application/ports/observation/**`、`src/application/marks.rs`。
JUnit の testcase は呼び出し元の名前で現れるので、表は呼び出し元の testcase で作った。

対象の commit（`ddc07d9e5`）で作った。行番号もこの commit のもの。
4 task の着地の commit（`18b9f802d`・`3753724d4`・`f2b4cd6de`・`a17fe0a12`）はどれもこの commit の祖先である（`git merge-base --is-ancestor`）。

| 範囲の case | 呼び出し元の testcase の名前 | 別と理由 |
|---|---|---|
| `src/infrastructure/runtime_store/ledger/**` と module の file `ledger.rs` の `#[test]`（104 本。うち `ledger.rs` の `mod tests` が 31 本） | `infrastructure::runtime_store::ledger::…` | (1) 範囲の file の test |
| `src/domain/ledger/**` の `#[test]`（276 本） | `domain::ledger::…` | (1) |
| `src/domain/kpi/**` の `#[test]`（153 本） | `domain::kpi::…` | (1) |
| `src/application/supervise/observation/ledger/**` の `#[test]`（240 本。`ledger.rs` 自身は `mod tests;` を宣言するだけ） | `application::supervise::observation::ledger::…` | (1) |
| `src/application/ports/observation/**` の `#[test]`（35 本: `earlier_visibility_spike` 33、`queue_rows_spike` 2） | `application::ports::observation::…` | (1) |
| `src/application/marks.rs` の `#[test]`（8 本） | `application::marks::tests::…` | (1) |
| `src/infrastructure/store_contract/ledger_*.rs`（`#[test]` は無い。case は下の wrapper と `runtime_store/ledger/*_tests.rs` から呼ばれる） | 下の (2) と (3)、範囲の中の (1) | — |
| `store_contract::ledger_*` の case のうち `runtime_store/ledger/{earlier,queue_from,queue_named}_tests.rs` から呼ばれるもの | `infrastructure::runtime_store::ledger::{earlier,queue_from,queue_named}_tests::…` | (1)（呼び出し元が範囲の file） |
| `store_contract::ledger_earlier::ledger_earlier_answers_as_every_row` | `infrastructure::sqlite::store_contract_tests::ledger_earlier_answers_as_every_row`（sqlite.rs:3060） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_earlier::ledger_earlier_past_an_old_writer` | `infrastructure::sqlite::store_contract_tests::ledger_earlier_past_an_old_writer`（sqlite.rs:3064） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_first_executions::ledger_first_executions_generations` | `infrastructure::sqlite::store_contract_tests::ledger_first_executions_generations`（sqlite.rs:3241） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_first_executions::ledger_first_executions_parts` | `infrastructure::sqlite::store_contract_tests::ledger_first_executions_parts`（sqlite.rs:3245） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_open::ledger_open_written_with_its_rows` | `infrastructure::sqlite::store_contract_tests::ledger_open_written_with_its_rows`（sqlite.rs:3249） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_open::ledger_open_counts_the_runs_left_open` | `infrastructure::sqlite::store_contract_tests::ledger_open_counts_the_runs_left_open`（sqlite.rs:3253） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_open::ledger_open_filled_in_parts` | `infrastructure::sqlite::store_contract_tests::ledger_open_filled_in_parts`（sqlite.rs:3257） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_open::ledger_open_parts_bounded_by_bytes` | `infrastructure::sqlite::store_contract_tests::ledger_open_parts_bounded_by_bytes`（sqlite.rs:3261） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_open::ledger_open_kept_through_generations` | `infrastructure::sqlite::store_contract_tests::ledger_open_kept_through_generations`（sqlite.rs:3265） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_queue_views::ledger_queue_views_written_with_their_rows` | `infrastructure::sqlite::store_contract_tests::ledger_queue_views_written_with_their_rows`（sqlite.rs:3269） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_queue_views::ledger_queue_views_count_what_a_read_names` | `infrastructure::sqlite::store_contract_tests::ledger_queue_views_count_what_a_read_names`（sqlite.rs:3273） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_queue_views::ledger_queue_views_filled_in_parts` | `infrastructure::sqlite::store_contract_tests::ledger_queue_views_filled_in_parts`（sqlite.rs:3277） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_queue_views::ledger_queue_views_pair_asks_whichever_comes_first` | `infrastructure::sqlite::store_contract_tests::ledger_queue_views_pair_asks_whichever_comes_first`（sqlite.rs:3281） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_queue_views::ledger_queue_views_kept_through_generations` | `infrastructure::sqlite::store_contract_tests::ledger_queue_views_kept_through_generations`（sqlite.rs:3285） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_range::ledger_range_answers_as_every_row` | `infrastructure::sqlite::store_contract_tests::ledger_range_answers_as_every_row`（sqlite.rs:3289） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_queue_named::ledger_queue_named_answers_as_every_row` | `infrastructure::sqlite::store_contract_tests::ledger_queue_named_answers_as_every_row`（sqlite.rs:3293） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_queue_named::ledger_queue_named_goes_back_whole` | `infrastructure::sqlite::store_contract_tests::ledger_queue_named_goes_back_whole`（sqlite.rs:3297） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_queue_named::ledger_queue_named_past_an_old_writer` | `infrastructure::sqlite::store_contract_tests::ledger_queue_named_past_an_old_writer`（sqlite.rs:3301） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_queue_named::ledger_queue_named_past_a_clock_set_back` | `infrastructure::sqlite::store_contract_tests::ledger_queue_named_past_a_clock_set_back`（sqlite.rs:3305） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_queue_named::ledger_queue_named_with_only_ci_after_a_future_main_event` | `infrastructure::sqlite::store_contract_tests::ledger_queue_named_with_only_ci_after_a_future_main_event`（sqlite.rs:3309） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_report::ledger_report_answers_as_every_row` | `infrastructure::sqlite::store_contract_tests::ledger_report_answers_as_every_row`（sqlite.rs:3315） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_report::ledger_report_past_the_queue_and_the_clocks` | `infrastructure::sqlite::store_contract_tests::ledger_report_past_the_queue_and_the_clocks`（sqlite.rs:3319） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_report::ledger_report_keeps_an_old_snapshot` | `infrastructure::sqlite::store_contract_tests::ledger_report_keeps_an_old_snapshot`（sqlite.rs:3323） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_report::ledger_report_past_a_named_task_s_history` | `infrastructure::sqlite::store_contract_tests::ledger_report_past_a_named_task_s_history`（sqlite.rs:3327） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::ledger_report::ledger_report_past_a_target_s_ends` | `infrastructure::sqlite::store_contract_tests::ledger_report_past_a_target_s_ends`（sqlite.rs:3331） | (2) wrapper: 本体は `contract` に名指しの case を 1 つ渡すだけ |
| `store_contract::run`（store_contract/mod.rs:132）が順に流す 61 の case のうち台帳の 30（`ledger_open_*`・`ledger_first_executions_*`・`ledger_earlier_*`・`ledger_queue_views_*`・`ledger_range_*`・`ledger_queue_named_*`・`ledger_queue_lives_*`・`ledger_report_*`・`ledger_queue_named::kpi_compare_groups_read_by_their_range`。store_contract/mod.rs:169〜288） | `infrastructure::sqlite::store_contract_tests::all_cases_run_in_order_on_the_same_database`（sqlite.rs:3335） | (3) 混在: 同じ memdb の上で範囲の外の case（leader・goal・delegates・search など）と台帳の case を続けて流す。JUnit では分けられない |

(1) は 816 本、(2) は 25 本、(3) は 1 本で、範囲の testcase（(1)＋(2)）は 841 本、計 842 本。
(1) の 816 本の名前は、下の filterset の module の path の部分で `cargo nextest list` に出る名前と一致する。
(2) の 25 本は、sqlite.rs の `mod store_contract_tests` の `ledger_*` の wrapper の全部である。
wrapper の本体はどれも名指しの case を `contract` に渡すだけなので、どれも (3) でなく (2) にした。
範囲の case を呼ばず、台帳のコードを使うだけの範囲の外の test（`application::kpi`・`application::stats` の test など）は入れていない。
Postgres の契約の example（ADR-t3986-1 決定10）は unit test の段の外の job（ci.yml の `postgres store contracts`）なので、集計に入れていない。
module の file `runtime_store/ledger.rs` の `mod tests`（31 本）は、testcase の名前が範囲の module の path で始まるので (1) に入れた。

表を作ったコマンド:

```sh
grep -rnE 'store_contract::(ledger_[a-z_]+|ledger_[a-z_]+::)' src | grep -v '^src/infrastructure/store_contract/ledger_'
#   → sqlite.rs の store_contract_tests の 25 の wrapper と、範囲の中の runtime_store/ledger/{earlier,queue_from,queue_named}_tests.rs
grep -rn 'store_contract::run' src            # → sqlite.rs:3336 の all_cases_run_in_order_on_the_same_database だけ
grep -rln 'use crate::infrastructure::store_contract\|store_contract::{' src | grep -v store_contract/
#   → sqlite.rs と範囲の中の 2 file だけ（別名で呼ぶ wrapper は無い）
grep -rn '#\[path' src                        # → 範囲の file を別の module に置くものは無い
cargo nextest list --locked --workspace --lib --bins --message-format oneline   # (1) の名前を module の path で数えた
```

## 環境

- 対象の commit: `ddc07d9e5b86f452c2b7c63c7018412d74df08e9`（この run の base。4 task の着地を含む）。
- 道具: rustc・cargo 1.99.1、cargo-nextest 0.9.150（d225b1daf 2026-10-26）、cargo-llvm-cov 0.9.1（手元。CI の macOS の job も同じ版）、gh 2.103.3。
- host: Apple M1、8 コア、16 GiB、macOS 27.2.1。`NEXTEST_TEST_THREADS=6`（dagq.toml の `[run.env]`）。
  他の run の build と test が同時に走り、負荷平均は 8〜14 だった。
- 経路: この run の worker（Claude、headless、background）。host には何も入れていない。
- CI の run（ci.yml、main の push）:

| 区分 | run の id | head の commit | 作成 | 4942・4943・4133・4134 を含む |
|---|---|---|---|---|
| after 1 | 43721344022 | `183bdfd1c` | 2026-11-15T13:53Z | 全部 |
| after 2 | 43698907887 | `3753724d4` | 2026-11-15T09:22Z | 全部 |
| before 1 | 43552653627 | `a17fe0a12` | 2026-11-14T03:38Z | 4134 だけ（4942 を含まない） |
| before 2 | 43514982812 | `5befd0775` | 2026-11-13T21:55Z | どれも含まない |

- runner: macOS は `macos-26`（3 vCPU）で `cargo llvm-cov nextest`、Linux は `ubuntu-24.04`（4 vCPU）で `cargo nextest run`。
  T は runner の CPU 数から取った。job の log の nextest の行（`Starting 7501 tests across 5 binaries`）は並列度を出さない。
  ci.yml と `.config/nextest.toml` は test-threads を決めず、nextest の既定は CPU 数である。
- 選んだ run の「成功」について: 2026-11-12T13:35Z の後の ci.yml の main の run は、どれも結論が `failure` か `cancelled` だった。
  known の失敗（`postgres store contracts` の job、`dagq::it` の lifecycle_install、docs の job）が毎回落とすためである。
  まだ走っていた 43730026423 も `postgres store contracts` がすでに `failure` で、60 分待っても結論の成功する run はそろわない。
  そこで、unit test の 2 つの testsuite（`dagq`、`dagq::bin/dagq`）が failures 0 で全件の testcase をそろえた run を「成功した」とみなした。
  選んだ 4 本はどれも、lib と bin の testsuite が failures 0 だった。落ちたのは `dagq::it` だけである。
  集計に使うのは unit test の testcase だけなので、IT の失敗は値に入らない。
  この読み替えに頼らなくても、着地の成分（(A)）だけで holds になる（下の「判定と根拠」）。

## 打ったもの

```sh
# (A) 着地の検証の形（build は測らない）
cargo nextest run --locked --workspace --lib --bins --no-run                 # 15:55:37Z〜16:02:24Z、6 分 43 秒
NEXTEST_PROFILE=ci sh scripts/cargo-brief.sh --log $TMPDIR/runA1.log \
  cargo nextest run --locked --workspace --lib --bins -E "$FS"               # 1 回目 16:25:45Z〜17:13:53Z
NEXTEST_PROFILE=ci sh scripts/cargo-brief.sh --log $TMPDIR/runA2.log \
  cargo nextest run --locked --workspace --lib --bins -E "$FS"               # 2 回目 17:13:53Z〜17:48:29Z
# 各回の後に target/nextest/ci/junit.xml を $TMPDIR/junitA1.xml・junitA2.xml へ写した

# (B) CI の材料
gh run list --workflow ci.yml --branch main --limit 40 --json databaseId,conclusion,status,headSha,createdAt,event
git merge-base --is-ancestor <4 task の commit> <head>                      # 40 本の各 head に当てた
gh run view <id> --json jobs                                                # job の結論
gh run download <id> -n nextest-junit-macos -D <tmp>/<id>/macos              # 4 本
gh run download <id> -n nextest-junit-linux -D <tmp>/<id>/linux              # 4 本
gh run view --job <id> --log                                                # nextest の行（並列度は出ない）
```

filterset（`$FS`）は対応表の (1)〜(3) を名指す:

```text
kind(lib) & (test(/^(infrastructure::runtime_store::ledger::|infrastructure::store_contract::ledger_|domain::ledger::|domain::kpi::|application::supervise::observation::ledger::|application::ports::observation::|application::marks::)/) | test(/^infrastructure::sqlite::store_contract_tests::(ledger_[a-z_]+|all_cases_run_in_order_on_the_same_database)$/))
```

1 つ目の `test(...)` は (1) の module の path、2 つ目は (2) の 25 の wrapper と (3) の混在の testcase を名前で足す。
2 つ目の正規表現は、`store_contract_tests` の中で `ledger_` で始まる名前と、`all_cases_run_in_order_on_the_same_database` だけに当たる。
`store_contract_tests` の中で `ledger_` で始まるのは対応表の (2) の 25 本だけである。
`cargo nextest list -E "$FS"` は 842 本を出した。2 回の JUnit の testcase の名前の集合は、どちらも対応表の 842 本と一致した。

16:02:51Z に 1 度流し始めたが、harness が background の command を 10 分で止めたので test の途中で切れた。
842 本のうち 840 本が通り、2 本が signal で止まった（`all_cases_run_in_order_on_the_same_database` が 660 秒超、`ledger_report_past_the_queue_and_the_clocks` が 420 秒超）。
この回は JUnit が無く、数えていない。filterset は同じで、2 回の上限は完走した 2 回で数えた。

集計は `$TMPDIR` の使い捨ての python（`xml.etree` で各 testcase の `time` を読む）で行った。どの JUnit にも同じ式を当てた。

## 測った表

式（どの JUnit にも同じ）:

- unit test は classname が `dagq`（lib）と `dagq::bin/dagq` の testcase。`dagq::it`・`dagq::plugin`・`dagq::e2e` は除く。
- 範囲の testcase は対応表の (1)＋(2)、混在の testcase は (3)。
- before の run には、名前の変わった test（`…_with_3000_settled_units` など）があり、まだ無い wrapper もある。
  そこで before では、(1) は module の path、(2) は `store_contract_tests::ledger_` の接頭辞で、その run の JUnit の中から同じ規則で選んだ。
- C は (3) の time の和、C_max はその最大。M は範囲の testcase の time の最大、S_r はその和、S_u は unit test の time の和、share は S_r / S_u。
- E = max(M, S_r / T)。M_hi = max(M, C_max)、S_r_hi = S_r + C、E_hi = max(M_hi, S_r_hi / T)。
- 1 回が「基準に当たる」のは、下限で M ≥ 30 秒か E ≥ 60 秒のとき。
  「当たらない」のは、上限で M_hi < 30 秒かつ E_hi < 60 秒のとき。
- W は (A) の nextest の `Summary` の行の wall time（test の段だけ）。
- (A) の JUnit は filterset の 842 本だけを持つので、(A) の S_u と share は全件の unit test に対する値ではない（S_u は範囲と混在の和）。

値は秒。

| 成分・回 | M | S_r | S_u | share | T | E | C（=C_max） | M_hi | S_r_hi | E_hi | W | 1 回の判定 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 着地 (A) 1 回目 | 819.7 | 8,439.6 | 10,409.2 | （81.1%） | 6 | 1,406.6 | 1,969.6 | 1,969.6 | 10,409.2 | 1,969.6 | 2,884.6 | 当たる |
| 着地 (A) 2 回目 | 433.2 | 4,886.5 | 6,412.0 | （76.2%） | 6 | 814.4 | 1,525.5 | 1,525.5 | 6,412.0 | 1,525.5 | 2,075.0 | 当たる |
| CI macOS after 1（43721344022） | 325.6 | 3,133.3 | 5,397.8 | 58.0% | 3 | 1,044.4 | 998.5 | 998.5 | 4,131.8 | 1,377.3 | — | 当たる |
| CI macOS after 2（43698907887） | 416.3 | 3,586.8 | 6,193.0 | 57.9% | 3 | 1,195.6 | 1,292.2 | 1,292.2 | 4,879.0 | 1,626.3 | — | 当たる |
| CI Linux after 1（43721344022） | 243.0 | 2,718.2 | 4,367.5 | 62.2% | 4 | 679.5 | 769.1 | 769.1 | 3,487.3 | 871.8 | — | 当たる |
| CI Linux after 2（43698907887） | 300.9 | 3,555.5 | 5,580.4 | 63.7% | 4 | 888.9 | 982.4 | 982.4 | 4,537.9 | 1,134.5 | — | 当たる |
| CI macOS before 1（43552653627） | 400.9 | 4,927.2 | 7,015.5 | 70.2% | 3 | 1,642.4 | 920.9 | 920.9 | 5,848.2 | 1,949.4 | — | 当たる |
| CI macOS before 2（43514982812） | 286.6 | 4,557.1 | 6,177.4 | 73.8% | 3 | 1,519.0 | 345.3 | 345.3 | 4,902.5 | 1,634.2 | — | 当たる |
| CI Linux before 1（43552653627） | 472.2 | 7,823.5 | 10,304.2 | 75.9% | 4 | 1,955.9 | 1,050.0 | 1,050.0 | 8,873.5 | 2,218.4 | — | 当たる |
| CI Linux before 2（43514982812） | 254.9 | 3,663.2 | 4,807.7 | 76.2% | 4 | 915.8 | 251.1 | 254.9 | 3,914.4 | 978.6 | — | 当たる |

参考（判定には使わない）: 混在の testcase の中の台帳の分を、(2) の wrapper の time の和で見積もった値。
(A) は 2,680.9 と 1,469.0、CI after の macOS は 925.7 と 1,232.2、Linux は 875.8 と 1,120.4 だった。
どれも C と同じ桁で、混在の testcase の大半は台帳の case と見られる。

参考（判定には使わない）: `.config/landing-unit-skip.toml`（ADR-t4979-1）が着地の unit の段から外す 6 本を除いた値。
6 本はどれも範囲か混在の testcase で、混在の 1 本も含む。

| 成分・回 | M | S_r | share | E |
|---|---|---|---|---|
| 着地 (A) 1 回目 | 214.5 | 6,025.5 | — | 1,004.2 |
| 着地 (A) 2 回目 | 119.6 | 3,500.8 | — | 583.5 |
| CI macOS after 1 / 2 | 68.3 / 83.2 | 2,222.2 / 2,511.1 | 63.7% / 65.7% | 740.7 / 837.0 |
| CI Linux after 1 / 2 | 59.8 / 78.7 | 1,950.1 / 2,568.9 | 68.9% / 71.1% | 487.5 / 642.2 |

6 本を外しても、残る範囲の testcase の最大は 1 本 60〜215 秒で、E は 488〜1,004 秒である。
今の着地の検証の形でも、基準を大きく越える。

範囲の test の遅い順の上位 10 本（秒。(A) 1 回目・CI macOS after 2・CI Linux after 2）:

| # | testcase | (A) 1 回目 | macOS after 2 | Linux after 2 |
|---|---|---|---|---|
| 1 | `infrastructure::sqlite::store_contract_tests::ledger_report_past_the_queue_and_the_clocks` | 819.7 | 416.3 | 300.9 |
| 2 | `infrastructure::sqlite::store_contract_tests::ledger_queue_named_goes_back_whole` | 545.9 | 242.2 | 246.8 |
| 3 | `application::supervise::observation::ledger::tests::frozen::rebuilding_each_frozen_input_keeps_every_value_and_publishes_the_full_fold` | 498.7 | 198.7 | 210.2 |
| 4 | `application::supervise::observation::ledger::tests::omitting::the_set_s_builder_taking_over_the_ledger_of_now_covers_from_where_it_reads` | 353.6 | 134.0 | 141.8 |
| 5 | `infrastructure::runtime_store::ledger::queue_named_tests::the_report_s_reads_return_as_many_with_300_settled_units` | 214.5 | 81.5 | 75.0 |
| 6 | `application::ports::observation::earlier_visibility_spike::rest::the_final_shapes_of_the_waits_lives_samples_and_failures_answer_as_every_row_with_many_units` | 196.2 | 84.5 | 86.8 |
| 7 | `application::supervise::observation::ledger::tests::frozen::the_ledger_without_the_omitted_rows_gives_the_values_fixed` | 174.7 | 74.1 | 78.7 |
| 8 | `infrastructure::sqlite::store_contract_tests::ledger_report_answers_as_every_row` | 170.1 | 83.2 | 78.6 |
| 9 | `infrastructure::sqlite::store_contract_tests::ledger_queue_named_past_a_clock_set_back` | 166.2 | 上位 10 の外 | 64.5 |
| 10 | `infrastructure::sqlite::store_contract_tests::ledger_queue_named_past_an_old_writer` | 165.1 | 65.5 | 上位 10 の外 |

CI の上位 10 本には、ほかに `ledger_report_past_a_target_s_ends`（macOS 70.2・Linux 66.0）が入る。
混在の `all_cases_run_in_order_on_the_same_database` は (A) 1 回目で 1,969.6、CI after で 769〜1,292 だった。

before と前の値との比較:

- CI の S_r は、macOS が before の 4,557〜4,927 から after の 3,133〜3,587 に、平均で約 29% 減った。
  Linux は before の 3,663〜7,823 から after の 2,718〜3,555 に減った。before の 1 本目は、遅い回だった。
- 減ったのは 3000 単位の比較の test だった。
  before で 1 本 104〜472 秒だった `…_with_3000_settled_units` と `earlier_visibility_spike` の 3000 単位の test は、after の上位から消えた。
  300 単位になった同じ test は 58〜87 秒（CI）である。
- M はほとんど減っていない。before の 255〜472 に対し、after は 243〜416 である。
  after の最大は、どの回も `ledger_report_past_the_queue_and_the_clocks`（4942 が 3000 から下げた対象の外）だった。
- request 312 の inbox の補足の値は、着地の検証のログで 3000 単位の test が 1 本 29〜631 秒、着地を 6〜10 分延ばすというものだった。
  after の着地の形（(A)）でも、1 本の最大は 433〜820 秒、E は 814〜1,407 秒（約 14〜23 分）、W は 2,075〜2,885 秒である。
  負荷平均 8〜14 の host での値で、CI after の M の 1.0〜3.4 倍、S_r の 1.4〜3.1 倍に当たる（(A) の 2 回の間でも 1.7 倍の差がある）。
  ADR-t4979-1 の 6 本を外した今の着地の形でも、E は 583〜1,004 秒になる。
  補足の値より減ったとは言えず、着地を延ばす量は補足の 6〜10 分と同じかそれ以上である。

## 判定と根拠

判定: **holds**。

当てた規則: 1 回の測定は、下限で M ≥ 30 秒か E ≥ 60 秒なら基準に当たる。上限で M_hi < 30 秒かつ E_hi < 60 秒なら当たらない。上限だけ当たる回は、混在で決まらない。成分ごとに 2 回をそろえ、どれかの成分の 2 回とも当たれば holds とする。

- 着地 (A): 1 回目は下限で M = 819.7 ≥ 30、E = 1,406.6 ≥ 60（上限は M_hi = E_hi = 1,969.6）。2 回目は下限で M = 433.2 ≥ 30、E = 814.4 ≥ 60（上限は M_hi = E_hi = 1,525.5）。2 回とも当たる。
- CI macOS: after 1 は下限で M = 325.6、E = 1,044.4、after 2 は M = 416.3、E = 1,195.6。2 回とも当たる。
- CI Linux: after 1 は下限で M = 243.0、E = 679.5、after 2 は M = 300.9、E = 888.9。2 回とも当たる。

3 成分とも、2 回とも下限で当たった。混在の testcase を 0 と数えても当たるので、混在で決まらない回（unresolved の (u4)）は無い。
2 回の間で判定が分かれた成分も無い。
CI の run の選び方（環境の節の「成功」の読み替え）を認めなくても、着地の成分だけで holds の条件を満たす。
W（2,884.6 と 2,075.0）は E と同じく 60 秒を大きく越え、食い違いは無い。

計画への意味: 4942・4943・4133・4134 の後でも、範囲の test は unit test の時間の 58〜64%（CI）を占める。
着地の検証では 1 本で最大 7〜14 分（負荷のある host）かかり、並列 6 でも段を約 14〜23 分延ばす見積もりになる。
3000 単位を 300 にした分は減ったが、最も重い 1 本と、300 単位でも 1 本 1 分を超える 10 本前後が残る。
そのため、台帳の読みのコードを変えた run だけで流す仕組みが要る。

## 残りの大きい件数の test

範囲にかけたコマンド:

```sh
grep -rnE '\b(3000|3_000|5000|5_000)\b|0\.\.(1000|1_000|2000|2_000|3000|3_000|5000|5_000)' \
  src/infrastructure/runtime_store/ledger src/infrastructure/runtime_store/ledger.rs \
  src/infrastructure/store_contract/ledger_*.rs src/domain/ledger src/domain/kpi \
  src/application/supervise/observation/ledger src/application/supervise/observation/ledger.rs \
  src/application/ports/observation src/application/ports/observation.rs src/application/marks.rs
```

162 件に当たった。範囲の外の確定した履歴を 300 単位より多くして比べる test は無かった。
本番の定数を越えるために、件数の literal で 300 件より多く作る test も無かった。外したものと理由:

- 時刻と時間（ms・秒）: `ms(3000)`、`T0 + 5000`、`NEAR + 3000`、`E + 3000`、`now - DAY - 3000`、`5_000 * MIN`、`log.fold(5000)` と `assert_covers(&life, 5000)` は畳む時刻、`interval("work", 1_000, 3_000)`、event の作成時刻の引数の `3000,` と `5000,` など。これらが 162 件の大半で、件数ではない。
- payload と期待値: `json!({... "at": T0 + 3000 ...})`、`land.secs[...] == 3000`（秒の期待値）、`(3, Some(1_000.0), Some(3_000.0))`、`denied(&mut log, None, 3_000)`。
- 本番の定数の定義: `EVENTS_PER_READ`・`STEPS_PER_READ`・`RESOURCES_PER_READ` = 5_000（`domain/ledger/builder.rs`）。test ではない。
- 繰り返しの上限: `store_contract/ledger_queue_lives.rs` の `for _ in 0..10000` と `ledger_first_executions.rs` の `for _ in 0..1000`。fill が終わるまで回す上限で、行の数ではない。
- comment と doc comment: `earlier_visibility_spike/rest.rs` の「3000 は run of record」（既定は 300、`DAGQ_SPIKE_4811_LARGE` で変える）、`earlier_visibility_spike.rs` の「debug build で 3000 は数分」、`queue_views/tests.rs`・`compare/tests.rs`・`kpi/tests/tokens.rs` の comment。
- id の offset と 300 以下の繰り返し: `runtime_store/ledger/visibility/tests.rs` の `1000..1000 + settled`（settled は 30 か 300）、`supervise/observation/ledger/tests.rs` と `tests/start.rs` の `1_000..1_000 + WRITES_PER_PASS`（200）、`tests/start.rs` の `5_000 + n`（id）。
- 件数として残るもの: `src/application/marks.rs` の `busy_queue` の `for step in 0..3000`。
  `CURSOR_SPAN = 64` の先のページを読ませる 3000 event で、task 4942 が残した。
  これを使う `application::marks::tests` の 8 本は、CI で 1 本 0.6 秒以下で、時間の問題は無い。

grep に当たらない形（件数を本番の定数の式で書くもの）で task 4943 が理由をつけて残した 4 本を、JUnit で確かめた（CI after の秒）:

- `application::supervise::observation::ledger::tests::sweep_reads::a_sweep_from_the_cursors_start_sends_a_request_a_share_not_a_row`（9.3〜13.3）
- `…::sweep_reads::the_rows_read_in_shares_sweep_and_write_as_the_rows_read_one_by_one`（2.6〜3.8）
- `…::oldest_first::the_oldest_mark_holding_r_is_taken_whatever_the_rank_restored`（17.7〜21.9）
- `…::oldest_first::the_oldest_mark_holding_each_cursor_of_r_is_taken_whatever_the_rank_restored`（18.2〜22.2）

どれも 30 秒未満である。

重いのは件数の literal ではなかった。300 単位の比較と store_contract の台帳の case（上の上位 10 本）が、件数を減らした後も 1 本 1〜7 分かかっている。

## 後続の計画への推奨

判定は holds なので、範囲の test を台帳の読みのコードを変えた run の着地だけで流す仕組みを、再計画の planner が計画する。推奨する形:

1. 置き場は、ADR-t4979-1 の着地の unit の段の外し方（`scripts/landing-it.sh` と `.config/landing-unit-skip.toml`）の隣にする。
   今の名指しの 6 本の一覧とは別に、「差分が範囲の path に当たらない run の着地の unit の段では外す」test の組を置く。
   組は、この文書の対応表の (1)〜(3) を nextest の filterset で書く。中身は上の `$FS` を使える。
   今の一覧はこの形を持てない。一覧は `name`・`reason`・`task`・`restore` だけを受け、知らない key は exit 2 で止まる。`landing-it.sh` は各行を `test(=name)` にする。
   そのため、一覧の書式と script の両方を変える。
   また、ADR-t4979-1 は次を決めている。決定 1 は、一覧の外の unit test を今どおり全件流すこと。決定 3 は、一覧に足すのを goal 291 の task と人の決定だけにし、日々の遅い test の受け皿にしないこと。
   差分で外す組はこの 2 つに当たる。ADR-t1925-1 決定 1（着地の unit の段は全件）にも当たる。
   そこで、仕組みの task は ADR を書く task にし、これらの決定を amends する新しい ADR から始める。
2. 差分の path の組は、対応表の範囲の path に、範囲の test が直に頼る次を足したものから始める。
   - `migrations/**`・`migrations-postgres/**`。
   - wrapper と `store_contract::run` の置き場の `src/infrastructure/sqlite.rs` と `src/infrastructure/store_contract/mod.rs`。
   - 範囲の test が使う testkit と fixture（`src/domain/ledger/testkit*` は範囲の中）。
   判定は `[recheck]` の `paths` と同じ glob の形にする（ADR-t2032-1）。
3. CI（ci.yml の macOS と Linux）は、今どおり毎回全部を流し、最終の関門に残す（request 43、ADR-t4979-1）。
   CI の時間を縮めたいときも、差分で外すのでなく、範囲の test を別の job か `--partition` に分けて並列に流す。
   main の push ごとの CI が全部を流すことが、下の見逃さない根拠の前提だからである。
4. 外した run でこの範囲の不具合を見逃さない根拠:
   - 範囲の test が見るのは、台帳の読み・作り・store の SQL・kpi の畳み方で、そのコードは 2 の path の組の中にある。
   - 組の外の差分がこの範囲を壊す経路は、event の kind・payload・queue の行の形が変わり、台帳の畳み方が誤ることである。
     その差分は event の型（`src/domain/` の event と queue の行）に触れる。
     planner は、それを 2 の組に足すか、外して CI で拾うかを決める。
   - 組の外の差分で壊れた場合も、着地の後の main の CI が全部を流すので、1 回の CI で見つかる。
     ADR-t4979-1 と request 43 は、着地の後に CI で見つかる事故を許すと決めている。
   - 確かめ方として、仕組みの task の受け入れに次を入れる。
     差分が組に当たる run では範囲の testcase が着地の unit の段で流れ、当たらない run では流れないこと（`landing-it.sh --self-test` の形）。
     加えて、着地の後の CI の JUnit で範囲の testcase が毎回流れていること。
5. 仕組みと別に、重い 1 本を軽くする直し（goal 291 (2)）は残す。
   仕組みは、組に当たる run（台帳の読みのコードを変える run）の着地を軽くしない。
   その run では、着地は今の E（今の着地の形で 583〜1,004 秒）を払い続ける。
   上位 10 本（特に `ledger_report_past_the_queue_and_the_clocks`・`ledger_queue_named_goes_back_whole`・frozen と omitting の 2 本）が対象になる。
6. 判断の材料として、仕組みを入れる前に、直近の着地のうち差分が 2 の組に当たった run の割合を数えると、効く量が分かる。
   goal 271 と 291 の間は割合が高いと見込む。
   数えるのは、`git log --name-only` を main の着地の commit に当てるだけで、worker でもできる。
