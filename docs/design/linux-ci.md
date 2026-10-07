---
id: design-linux-ci
type: design
title: Linux build and test job in CI
status: current
created: 2026-10-02
scope: operations
tags:
  - testing
  - ci
related:
  - adr-t2034-1
  - design-stress-ci
  - design-slow-tests
  - adr-0076
  - development-testing
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

## docsだけの差分

**予定（未実装）**（[ADR-t2034-1](../adr/2026-10-07-t2034-1-skip-rust-ci-jobs-on-docs-only-changes-and-do-not-read-skipped-runs-as-green.md)）。
`ci.yml` は差分を判定する小さな job と、Rust の要らない doc と script の検査を常に流す macOS の job を持ち、`linux` と macOS の Rust の job（fmt・clippy・`cargo llvm-cov nextest`・JUnit・Slow tests・IT test time gate）は判定の job を `needs` に持って、docs だけのとき job の `if` で飛ぶ。
2 つの Rust の job は今の `name:` を保つ。

- 判定: base（pull request の base か push の `before`）から実行の commit までの `git diff --name-only` が全部 `docs/**` か root の `*.md` なら docs だけ。
  base が無い・0 だけ・履歴に無い、差分が空、判定の job が落ちたときは全部流す。
  外部の action は使わない。
  判定の job と常に流す検査の job は `fetch-depth: 0`（base と release の tag が要る）
- Rust の job の `if` は `!cancelled()` と判定の output が docs だけでないことで書く（`needs` の既定の `success()` のままだと、判定の job が落ちたとき飛んでしまう）
- `on.paths-ignore` にしないのは、実行が起きないと必須の status check が pending で残り、doc の検査も流れないため。
  `if` で飛んだ job は status check では success と報告される（job の API の `conclusion` は `skipped`）
- 飛ばした実行を緑と読まない側: [CI failure issues](ci-failure-issues.md) の「きっかけ」と [CI watch](supervisor-lifecycle/ci-watch.md) の「実行の扱い」。
  Rust の job の `name:` を変えるときは `dagq.toml` の `[ci_watch]` で名指した job の名前も同じ変更で直す。
  Rust の job 以外に job の `if` を足すときは、同じ変更で `ci-failure.yml` も直す（`skipped` の job で閉じないため）

## 失敗を通さない

job は失敗を通さない（`continue-on-error` を持たない）。Linux の build か test が落ちれば workflow 全体（main の CI の結果、[ci-failure の issue](ci-failure-issues.md)）が落ちる。task 1237 で `continue-on-error: true` の形で足し、task 1238 が main の Linux の job の失敗を全部扱ってから外した。macOS に固有の test の書き方は [testの制約](../development/testing.md) の「macOSに固有のtest」が持つ。

## task 1238 で扱った Linux の失敗

task 1237 の後の main の run（例 run 37230895149、`2736 tests run: 2732 passed, 4 failed`）で毎回落ちた 4 件は、どれも移植できる形に直し、macOS に固有として分けたものは無い。

| test | Linux で落ちた理由 | 扱い |
| --- | --- | --- |
| `dagq infrastructure::launchd::tests::uninstall_tolerates_a_missing_plist_but_not_a_relative_install_path` | `launchctl` が無く、`uninstall` と `bootout` が `start "launchctl"` で落ちた | runtime を直した: macOS の外では launchd に何も載り得ないので、`Launchctl` の `state` は載っていない、`bootout` は `false` を返し、`install` は「launchd mode needs macOS」の error にする（`src/infrastructure/launchd.rs`）。test は両方で流し、Linux では `install_off_macos_says_launchd_mode_needs_macos`（`#[cfg(not(target_os = "macos"))]`）が error の文面を確かめる |
| `dagq::it cli_operations::the_user_the_inbox_and_the_planner_keep_their_operations` | supervisor の無い `down` が同じく `launchctl` を起動できずに落ちた | 上と同じ runtime の直しで、Linux の `down` は載っている agent が無いとして成功する |
| `dagq infrastructure::adapters::tests::worktree_status_does_not_write_back_the_index` | file を消して 20ms 後に同じ中身で書き直して index を古くしていたが、Linux の Git は stat を秒単位で比べ、ext4 は消した inode を再び使うので、index の項目が変わらず見えて素の `git status` が index を書き直さなかった | test を直した: mtime を 1 時間前に動かして（`File::set_modified`）、どちらの OS でも stat data が確実に変わる形にした |
| `dagq::it runtime_repair::a_long_process_that_uses_cpu_time_is_not_an_idle_process_alert` | Linux の `ps -o time=` は CPU 時間を秒単位で出すので、約 1 秒 CPU を使う process の進みが 0 に見え、`idle_process` の警報と復旧 job が出た | runtime を直した: Linux では `SystemProcesses::list` が CPU 時間を `/proc/<pid>/stat` の `utime + stime`（clock tick）から読む（読めない process は `ps` の値のまま）。読み取りの判断は unit test（`ps_listings_are_read` の `proc_stat_cpu_ticks`）が確かめる |

同じ期間に一度だけ落ちたものは扱いが別: `dagq::it planner_headless_turns::a_headless_request_planners_answer_reaches_a_new_one_and_undecided_ends_exhaust_the_request` は 2026-10-04 の 2 回の run で timeout し、後の run では落ちていない。`dagq::it runtime_slot_limits::the_supervisor_table_sets_parallel_and_max_waiting_and_is_read_again` は流し直しで通った（`FLKY-FL`、job を落とさない）。

## macOSに固有として分けたtest

| test | 場所 | macOS に固有として分けた理由 |
| --- | --- | --- |
| `infrastructure::adapters::tests::proc_pidinfo_reads_a_childs_directory_and_none_for_a_pid_that_runs_nothing` | `src/infrastructure/adapters.rs` | macOS 固有の `proc_pidinfo(PROC_PIDVNODEPATHINFO)` による process の作業ディレクトリの読み取りを検証するため、`#[cfg(target_os = "macos")]` で分ける。子 process の起動時の作業ディレクトリが読め、終了した process や無効な pid では `None` になることを確かめる。Linux は `/proc/<pid>/cwd` を読む別の実装 |

runtime の実装だけを分ける cfg（`src/infrastructure/adapters.rs` の `process_cwd` / `process_executable`、`crates/dagq-broker/src/backends/fs.rs` の `set_errno`、`src/infrastructure/launchd.rs` の `HAS_LAUNCHD`）は test を分けていないので、この一覧に含めない。`tests/it/runtime_headless.rs` と `tests/it/runtime_support/thread_stacks.rs` の `cfg!(target_os = "macos")` は test とその helper 内の分岐で、test 自体は両方の OS で流れるため、同じく含めない。

macOS の外だけで流れる逆向きの test は上の一覧とは別: `src/infrastructure/launchd.rs` の `install_off_macos_says_launchd_mode_needs_macos` は `#[cfg(not(target_os = "macos"))]` で、launchd mode が使えないと分かる error を確かめる（上の「task 1238 で扱った Linux の失敗」）。

足すときは [testの制約](../development/testing.md) の「macOSに固有のtest」に従い、test の名前・場所と、何が macOS にしか無いかをここに書く。

## summary の書式

`Failed tests` の step は summary に `## Linux: failed tests` の見出しと次のどれかを書く。

- 落ちた test があるとき: `<N> failed:` と、コードブロックに 1 行 1 本の `<binary id> <test name>`（例 `dagq::it runtime_claim::name`）。log の最後の `Summary [` の行より後の結果の行のうち、status が [Integrate](supervisor-lifecycle/integrate.md) の「nextestの失敗のstatusの集合」の (A)(B)(C)（`FAIL`・`FAIL + LEAK`・`XFAIL`・`LEAK-FAIL`・`TIMEOUT`・`ABORT`・`SIG<name>`・`ABORT SIG <n>`、`TRY <n>` の後の `FAIL`・`FL+LK`・`XFAIL`・`LKFAIL`・`TMT`・`ABORT`・signal の名前・`SIG <n>`、`FLKY-FL n/m`・`FLAKY n/m`）の行から、最後の 2 つの欄を取って重複を除く（status が複数語でも最後の 2 語）。`FLKY-FL` は落ちて流し直しで通った test で、これも載る。`LEAK`・`SLOW`・`TRY <n> SLOW` などの失敗でない行は載らない。集合は `src/domain/verify_failure.rs` と `scripts/stress-recent-tests.sh` と同じで、正規表現は stress の script と同じ文字列（task 1272）
- 落ちた test が無いとき: `none`
- どちらでも、その後に nextest の `Summary [` の行（流した数・通った数・落ちた数）
- log が無いか `Summary [` の行が無いとき（build か準備の失敗）: `no nextest summary (the build or the setup failed; see the log)`

Linux で新しく落ちた test は、この summary から読んで移植できる形に直すか、macOS に固有として分けて上の節に足す。
