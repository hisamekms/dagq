---
id: design-supervisor-lifecycle-source-repository
type: design
title: "Source repository"
status: current
created: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-integrate
  - design-supervisor-lifecycle-install
  - design-supervisor-lifecycle-auto-update
  - design-supervisor-lifecycle-stats
  - design-persistence
  - adr-t614-1
  - adr-t614-2
  - adr-t2268-1
  - design-measurement
---

# Source repository

dagqの開発でだけ要る機能は、queueのrepositoryが「dagqのソース」かの判定1つで有効にする（[ADR-t614-1](../../adr/2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)）。設定・flag・環境変数で判定を上書きする手段は無い。

**実装状況**: 判定と表の4行はどれも実装済み。

## 判定

- 読むのはqueueが束縛されたrepositoryのroot（main checkout。`dagq.toml`と同じ場所）の`Cargo.toml`の作業ファイル。`--from`なしの`install`はqueueを開く前に動き、queueが無くても動くので、buildしようとするcheckout（cwdのrepositoryのmain checkout）の`Cargo.toml`を読む。
- `[package]`の表があり、その`name`が文字列`"dagq"`ならソース。
- ファイルが無い、読めない、TOMLとして読めない、`[package]`が無い（`[workspace]`だけ）、`name`が`"dagq"`でない、のどれかならソースではない。
- 機能を使うたびにその時点のファイルで判定し、DBにもsupervisorの登録にも保存しない。
- 実装: 判定は`domain::source_repository::is_source`（`Cargo.toml`の文字列を受け、`[package]`の表の`name = "dagq"`（basic・literalの文字列、行末のコメント可）だけを真にする）。TOMLを完全には解釈せず行単位で読むので、`[ package ]`のように空白を含む表の見出しや、dotted keyの`package.name`はソースでない側に倒れる。ファイルを読むのは`infrastructure::adapters::is_dagq_source(dir)`で、`GitRepository::is_dagq_source`（port`Repository::is_dagq_source`）はmain checkout（`dagq.toml`を読むのと同じ`checkout`）を渡す。`up`は`RepositoryPaths::dagq_source`で受け取る。

## 対象

新しくdagqの開発でだけ要る機能を足すときは、この判定を使い、この表に行を足す。
この判定に置くのはdagq自身の配布の事情（表の上の3行と、そのinstallと自動更新の前のe2eの関門）だけで、言語・道具の知識は置かない（[ADR-t2268-1](../../adr/2026-10-12-t2268-1-toolchain-knowledge-behind-declared-profiles.md)決定2）。
言語・道具の知識は`dagq.toml`で宣言したtoolchainのprofile、projectの値は`dagq.toml`、hostの値は`host.toml`、言語に依らない知識（hostの分類の汎用の印、待ち・dagq・gitのコマンドの分類）はruntimeの共通の既定に置く。
**実装状況**: profileのportと`[toolchains.<名前>]`の宣言はある（[Toolchain profiles](../toolchain-profiles.md)）。
検証の失敗の分類・落ちたtestとflakyのtestの抜き出し・flakyのやり直しのenvは、宣言したprofileを通して働く。
宣言の無いrepositoryではRustのprofileが暗黙に当たり、`doctor`と`status`が警告する（ADR-t2268-1決定9）。
buildの出力の置き場と掃除・計測・CIの見張りの読み方・processの種類はまだprofileに移っておらず、表の4行目はこの判定で分け、下の「言語・道具の知識の今の場所」はcargoの前提でどのrepositoryでも働く。
移行の間も、宣言の無いrepositoryは今のRustの振る舞いのままで、旧い分け方を外すのはこのrepositoryの`dagq.toml`に宣言と規則が入った後である（ADR-t2268-1決定9）。

| 機能 | 決定 | dagqのソース | ソースでないrepository |
| --- | --- | --- | --- |
| `integrate`のmigrationの番号の振り直し（[integrate](integrate.md)の5） | ADR-0067決定3 | 今までどおり（`migration_renumbered`、振り直せなければ`migration_number_taken`の`needs_session`） | 番号を見ない。振り直さず、`migration_number_taken`にもしない。`migrations/`の変更は他のファイルと同じに扱う |
| `--from`なしの`dagq install`（main checkoutからの`cargo build --release --locked`。[install](install.md)の1） | ADR-0073決定14 | 今までどおり | buildせずにerror。`cargo install dagq`で入れ替えるか、`--from`でバイナリかcheckoutを指すよう案内する。`--from`付きと`--rollback`は判定に関係なく動く |
| `up --auto-update`のsource build（[Auto-update](auto-update.md)） | ADR-0073決定17 | 今までどおり | `up`はerrorで止め、supervisorを起動も引き継ぎもしない。自動更新の設定を持つsupervisorもbuildに進まない。外部のprojectの更新は[Release update](release-update.md)（ADR-t618-1） |
| `stats`・KPI・worktimeのcargo専用の計測（[stats](stats.md#cargo専用の計測)） | ADR-t614-1決定2(d)（ADR-t1662-3・ADR-t2268-1がamends） | 今までどおり | 記録も出力もしない（下の一覧）。決まった形ではこの判定から外す（下の「cargo専用の計測」） |

### cargo専用の計測

ソースでないrepositoryのqueueで記録も出力もしないもの。
決まった形では、コマンドの分類と回数は`dagq.toml`の規則・宣言したprofileの既定の規則・runtimeの共通の既定の規則の重ね合わせ（[計測](../measurement.md#コマンドの形と分類)）、hostの`rustc`とtoolchainの記録と分け方は宣言したRustのprofileが持ち、この判定を使わない（ADR-t2268-1決定2・5）。
今はどれも次のとおりこの判定で分ける。

- worktime（`src/domain/worktime.rs`）: commandの分類のうち`e2e`（`--test e2e`）・`llvm_cov`・`test`（`cargo test`と`cargo nextest`）の判定と、`full_tests`（全体の`cargo test`。filterの無い`--test it`も含む）・`llvm_cov_runs`・`verification_repeats`（integrateと重なるllvm-cov・全体のtest・e2e）の数。ソースでなければ、区間を閉じるときにこの3つの分類を付けず（そのコマンドは他の規則に落ちる）、3つの数を`work`に書かない（落ちたtestの名前も、testを流す分類のコマンドが無いので読まない）。判定は区間を閉じるたびに`infrastructure::sessions::dagq_source`が、queueの束縛（`queue_repository`）からmain checkoutを求めて行う（書き込みトランザクションの中で閉じる区間は、transcriptと同じく`read_before`がトランザクションの前に判定しておく）。束縛の無いqueueはソースでない。
- `stats`の`work_breakdown`の`verification_repeats`・`runs_with_repeats`・`test_with_llvm_cov`（`src/domain/stats/work.rs`）: ソースでなければ`runs`と全ての群で欄ごと出さない（`domain::stats::without_cargo_measures`）。
- claimの属性の`rustc_release`・`rustc_host`（`run_claimed`）: ソースでなければsupervisorは`rustc -vV`を実行せず、欄ごと書かない（`Ports::host_versions`のcheckoutを`None`にする）。`stats`の`runs`の`rustc_release`・`rustc_host`と`versions.rustc`はソースでなければ欄ごと出さない。[`kpi`](kpi.md)の`toolchain=`の層はソースでなければ`--by toolchain`でも出さない。
- 出力の判定は`stats`・`kpi`・日次のレポート・observerの入力を出すたびに`compose`がmain checkoutの`Cargo.toml`で行う（`stats`は`StatsSources::dagq_source`、KPIは`application::kpi::Host::dagq_source`）。

### 言語・道具の知識の今の場所

この判定で分けず、cargoの前提でどのrepositoryでも働くもの。
決まった形では、道具の知識はRustのprofileの実装に移り、宣言したrepositoryでだけ働く（ADR-t2268-1決定1・2）。
言語に依らない部分はruntimeの共通の既定に、このrepositoryに固有の部分は`dagq.toml`に置く（ADR-t2268-1決定2・4）。

- 検証の失敗の分類と落ちたtest・flakyのtestの抜き出し、flakyのやり直しのenvと受け継いだ値を落とす扱いは、profileに移った（汎用の印は`domain::verify_failure`の共通の分類、cargo・nextest・rustfmtの印とenvはRustのprofileの`domain::toolchain::rust`。[Toolchain profiles](../toolchain-profiles.md)）。
  残るのは`tests/common`の`within`の時間切れの行の印で、projectに固有の印として`domain::verify_failure`の共通の分類が宣言に依らず読み、`dagq.toml`のprojectの印の口ができたらそこへ移す。
- buildの出力の置き場と掃除: landing recheckの`application::supervise::recheck`と自動更新の`application::update`。
  workerとintegrateのbuildの出力の置き場は`[run.env]`のprojectの値を`infrastructure::run_env`が読むだけで、runtimeに雛形は無い（決まった形ではprofileの推奨の案として出す。ADR-t2268-1決定1・7）。
- CIの見張りの読み方: `domain::ci_watch`。
- processの種類: `domain::host_metrics`。
  cargo・rustcの種類はRustのprofileに移し、claude・dagq・そのほかの種類はruntimeの共通の既定に残す。

## テスト

- `src/domain/source_repository.rs`の単体test: `name = "dagq"`の`[package]`だけがソースで、ファイル無し・別の名前・`[workspace]`だけ・別の表の`name`・引用の無い値はソースでない。
- `src/application/integrate.rs`の`migrations_without_a_collision_are_left_alone`: dagqのソースでないrepositoryでは、mainが取った番号のmigrationを足したrunも振り直さない（`plan_renumber`の判断。振り直しのintegration testは`[package] name = "dagq"`のfixtureで動く）。
- `tests/e2e/other_repository.rs`の`a_task_lands_on_master_of_a_repository_without_origin_cargo_toml_or_agents_md`（e2e）: `Cargo.toml`の無いrepositoryで、mainに`migrations/0001_x.sql`があるときにrunが足した`migrations/0001_e2e.sql`が番号を変えずに着地し、`migration_renumbered`も`migration_number_taken`も記録されない。
- `tests/it/source_repository.rs`: `--from`なしの`install`が`Cargo.toml`の無いrepositoryと`migrations/`を持つ別のpackageのrepositoryでbuildの前にerrorになり、dagqのソースでは判定を通ってbuildに進むこと。`up --auto-update`がソースでないrepositoryでsupervisorを起動せず、ソースでは起動すること。
- `tests/it/cli_version.rs`の`auto_update_installs_each_runtime_landing_and_puts_a_broken_build_back`: `Cargo.toml`の名前を変えるとruntimeの着地でjobが起動せず、戻すとまたbuildして入れ替える。
- cargo専用の計測: `src/domain/worktime.rs`の`without_the_cargo_rules_no_cargo_only_kind_or_count_is_kept`（分類と数）、`src/infrastructure/sessions.rs`の`outside_dagqs_source_the_work_breakdown_keeps_no_cargo_measure`（別のpackageと束縛の無いqueueで区間の`work`に残らない。`a_run_session_records_its_work_breakdown_at_its_close`はdagqのソースに束縛して今までどおり記録する）、`src/infrastructure/adapters.rs`の`host_versions_come_from_the_claude_path_and_rustc`（checkoutが無ければ`rustc`を読まない）、`src/domain/kpi/tests.rs`の`the_toolchain_axis_splits_only_dagqs_source`、`src/domain/stats/cargo.rs`と`src/domain/stats/work.rs`の単体test。`tests/it/cli_read.rs`の`timeline_and_stats_show_the_work_breakdown`はdagqのソースに束縛したqueueの`stats`と`kpi --by toolchain`に出て、別のpackageに束縛し直すと出ないこと。`tests/it/runtime_integrate.rs`の`integrate_renumbers_a_migration_whose_number_main_took`（ソース）と`conflict_free_run_lands_as_one_squash_commit_and_releases_dependents`（ソースでない）はclaimの`rustc_release`の有無を見る。

## リリース済みのmigration

リリース済み（最新の`v*`のtagに含まれる）の`migrations/*.sql`は中身も名前も変えず消さない（[ADR-t614-2](../../adr/2026-09-27-t614-2-released-migrations-are-immutable.md)）。検査の場所と今の状態は[Persistence](../persistence.md)のmigrationの項にある。
