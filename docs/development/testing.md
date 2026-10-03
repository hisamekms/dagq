---
id: development-testing
type: development
title: このrepositoryのtestの制約（coverageの関門・test binary・置き場所・書き方・ファイルの行数・待ちの上限・e2eとその印・手動スモーク）
status: current
created: 2026-10-03
updated: 2026-10-04
owners:
  - hisamekms
tags:
  - testing
  - conventions
related:
  - adr-t1453-2
  - adr-t1582-1
  - development-local-checks
  - development-task-registration
  - development-migrations
  - plan-local-checks-history
---

# このrepositoryのtestの制約

testを書く・置く・直すときの今の規則。読むのは、`tests/`・`crates/*/tests/`・`src/`の`#[cfg(test)]`・`.config/e2e-quarantine.toml`を変えるworkerと、それらを変えるtaskを登録するplanner。手元でどのtestを流すかは[手元の検証](local-checks.md)、taskのverifyの選び方は[taskの登録](task-registration.md)、migrationの規則は[migration](migrations.md)、経緯は[手元の検証とtestの規則の経緯](../plans/local-checks-history.md)とADRが持つ。

## coverage

- 行カバレッジの合計を80%以上に保つ（`cargo-llvm-cov`、行基準、全体）。下回る変更は着地しない。
- 関門のコマンドは`cargo llvm-cov nextest --locked --workspace --fail-under-lines 80`。cargo-nextestがtestを1件ずつ別processで、binaryをまたいで並列に流す（[ADR-0076](../adr/0076-run-the-coverage-gate-tests-with-nextest.md)）。`--workspace`は、cargo-llvm-covがrootがpackageのworkspaceで`default-members`を見ずroot package（dagq）だけをreportに入れるため、`crates/`のbrokerのcrateも80%に数えるために付ける（[ADR-t828-1](../adr/2026-09-28-t828-1-coverage-gate-covers-the-workspace-with-workspace-flag.md)）。
- 門番はtaskの`verification_commands`（`integrate`がrebase後にworkerのreceiptを信用せず1回だけ実行する。validatingでは実行しない）とCI。workerが手元で流すものは[手元の検証](local-checks.md)。登録済みのtaskの旧コマンドの扱いは[taskの登録](task-registration.md)の「coverageの関門」。

## test binary

`cargo llvm-cov nextest`（と旧コマンドの`cargo llvm-cov`）は`cargo test`と同じtest binary群を全部実行し、1件でも落ちれば失敗する: `src/lib.rs`のunit testと`tests/it`・`tests/e2e.rs`・`tests/plugin.rs`、`crates/`のbrokerのcrateのunit testと`crates/<crate>/tests/`。rootの`Cargo.toml`の`[workspace]`の`default-members`が全てのcrateを入れるので、`cargo test`とnextestは`--workspace`なしで全てのcrateのtestを流す（[ADR-t827-1](../adr/2026-09-28-t827-1-broker-crates-binaries-and-version-alignment.md)決定3）。coverageの80%は関門の`--workspace`で全てのcrateを合わせた全体で見る。このrepositoryのcrateにdoctestは無く、nextestはdoctestを流さない。

## testの置き場所

- e2e（`tests/e2e.rs`）とplugin（`tests/plugin.rs`）以外のintegration testは1つのtest binary `it`（`tests/it/main.rs`）にまとめ、testは機能ごとのファイル`tests/it/<機能>.rs`に足す。新しいファイルは`tests/it/main.rs`に`mod <機能>;`を足す。`Cargo.toml`は`autotests = false`で`[[test]]`は`it`・`e2e`・`plugin`の3本（[ADR-0078](../adr/0078-one-integration-test-binary.md)）。
- 複数のファイルで使うhelperは`tests/common`（`it`と`plugin`の両方が読む。`tests/it`からは`crate::common`）、runtimeのtestだけが使うfixtureとhelperは`tests/it/runtime_support`（`crate::runtime_support`）に置く。
- `crates/`のbrokerのcrateのtestは各crateの`src/`のunit testと`crates/<crate>/tests/`に置き、podmanを要るtestはe2eと同じく`#[ignore]`にする（関門とCIに数えない）。

## testの書き方

- 関門ではtestは1件ずつ別processになるので、binaryの中の他のtestとprocessの状態（staticや一度だけの初期化）を共有することに頼るtestを書かない。
- `cargo test`では同じprocessのthreadで走るので、process全体の状態（env・cwd）を変えるtestを書かない。

## testファイルの行数

- `tests/*.rs`・`tests/common/*.rs`・`tests/it/**/*.rs`（tests/の下の全`.rs`）と`crates/*/tests/**/*.rs`はどれも3,000行以下に保つ（1つのbinaryにまとめてもファイル単位のまま）。末尾にtestを足す形の大きなファイルは別々のtaskの追記が同じ場所で衝突するため。
- `scripts/check-test-file-lines.sh`が超えたファイルを名前と行数つきで出してexit 1にし、CIも実行する。
- 超えそうなら、testを足す前に機能ごとのファイル（`tests/it/runtime_*.rs`・`tests/it/lifecycle_*.rs`のように）へ分け、複数のファイルで使うhelperは`tests/common`に、runtime系だけのものは`tests/it/runtime_support`に寄せる。
- `src/`はまだ対象外。

## 待ちの上限

- testの待ちには上限を付ける。pollのloopはdeadlineを持ち、上限の無い待ち（threadのjoin、stubのsessionの終了、`dagq`の子プロセスの`output()`）は`tests/common/mod.rs`の`within`（fixtureが持つtest全体の`common::test()`と、1つの待ちの`STEP_LIMIT`）で包む。
- 上限を過ぎるとtest binaryがtestの名前と待っていた条件をstderrに出してexit 101で失敗する（`cargo test --test it`では同じbinaryの残りのtestもそこで止まる。関門のnextestはtestごとのprocessなので止まるのはそのtestだけ）。`cargo test | tail`が戻らなくなることはない。

## e2e

- ハッピーパスを`tests/e2e.rs`に置く。実バイナリ・実Git・実cmuxを使い、Claudeの代わりに受け入れ条件どおりcommitとreceiptを書くstubスクリプトをproviderにする。cmuxが必要なので`#[ignore]`とし、integrateでは流さない（1本ずつの着地の上限が下がるため。[ADR-t963-1](../adr/2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定4）。着地の rebase の後にも流し直さない。
- e2eを流すのはworkerでなくruntimeがhostで、2つの場所: (1) 差分が`dagq.toml`の`[e2e] paths`に触れるrunと`--evidence e2e`のtaskのrunで、reviewのpassの後・着地の前（[Review](../design/supervisor-lifecycle/review.md)の「着地の前のe2e」、要否は[Validation](../design/supervisor-lifecycle/validation.md)の「runtimeが流すe2e」）。(2) 自動更新とsourceのcheckoutからの`install`が固定バイナリを入れ替える前（[Auto-update](../design/supervisor-lifecycle/auto-update.md)・[install](../design/supervisor-lifecycle/install.md)）。workerが流さないことと`e2e_failed`のresumeでの再現は[手元の検証](local-checks.md)の「e2eを流さない」。
- `[e2e] paths`の外の変更が実cmuxとの組み合わせを壊しても本番のバイナリは(2)の関門が守るので、落ちたらplannerが直すtaskを作る。
- inboxもplannerもe2eを自分では再実行しない（落ちたときの汎用の手順はpluginの`dagq-recover`の`reference/review-by-hand.md`）。
- 期間限定で登録から外したケース（[ADR-t1582-1](../adr/2026-10-04-t1582-1-temporarily-leave-broker-and-cmux-only-e2e-cases-out.md)）: 次の4ケースは本文を残したまま`#[cfg(any())]`で`tests/e2e.rs`の登録から外れ、`--ignored`でも、(1)の着地の前のe2eでも(2)の関門でも流れない（podmanに繋がらないときの`broker::`の省略とは別で、4ケースは結果の`skipped`には出ない。podmanの確認はこの間も動き、繋がらなければ`skipped`に`broker::`が書かれるが、省くtestは登録に無い）。外したケースだけが使うhelperにも同じcfgと、一緒に消す・戻すtaskのIDのcommentを付け、ケースを削除・復帰するtaskは同じ変更でそのhelperのcfgも削除するか外す。crate全体の`dead_code`の許可は付けない。早期returnで成功に見せたり、`#[ignore]`だけで外したりしない。この4ケースの外に広げない。
  - `broker::a_preferred_worker_does_its_task_through_the_broker_and_lands`: task 1451が自分の変更でcfgと一時のcommentを外して復帰し、着地の前に実podmanでbrokerのe2eを流す。
  - `the_sweep_closes_workspaces_left_in_any_window_after_their_fixture_dir_is_gone`: task 1440が旧ケースとcfgを削除する。
  - `planner::the_runtime_opens_planners_side_by_side_that_submit_go_idle_and_exit`: task 1441が削除するか非対話のケースに置き換え、置き換えたらcfgを外して復帰する（先にtask 1399で置き換わっていればその実装に合わせる）。
  - `up_in_cmux_starts_a_supervisor_in_a_workspace_that_down_wait_stops_and_closes`: task 1443が旧ケースとcfgを削除する。

## `[e2e] paths`

`[e2e] paths`（一覧と理由は`dagq.toml`のコメント）を変えるのは、実物の境目のファイルが増えた・分かれたとき。`dagq.toml`なので着地してmain checkoutに反映されてから効く。

## e2eの印

関門が落ちたe2eを流し直し、`.config/e2e-quarantine.toml`の印を効かせる仕組み・書式・上限・効かない印は[Auto-update](../design/supervisor-lifecycle/auto-update.md)、着地の前のe2eで印をどのcommitから読み、どの印を外すかは[Review](../design/supervisor-lifecycle/review.md)の「着地の前のe2e」が持つ（[ADR-t1165-1](../adr/2026-09-30-t1165-1-e2e-gate-reruns-failed-e2e-once-and-records-quarantined-failures.md)、[ADR-t1233-2](../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)決定5）。

- 印は不安定なe2eが直るまでの一時の措置で、`dagq.toml`には置かない。
- 印を付けるのは、直すtaskのIDと期限を持ちplan reviewを通ったtaskだけ。inboxやplannerが手で付けない。
- 印の付いたtestを直すtaskは、同じ変更で自分の印を外す。
- 印を変えるtaskのverifyには`sh scripts/check-e2e-quarantine.sh`を付ける（[taskの登録](task-registration.md)の「推奨の組み合わせ」）。
- 印で通した回数とflakyの回数は`stats`の関門のtestごとの数え方（[stats](../design/supervisor-lifecycle/stats.md)）で見て、直すtaskの優先と印の期限を決める。

## 手動スモーク

実Claudeを含む経路は自動化せず、手動スモーク（[manual-smoke](../design/manual-smoke.md)）で確認する。
