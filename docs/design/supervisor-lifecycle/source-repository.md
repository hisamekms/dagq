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
  - design-supervisor-lifecycle-commands
  - design-persistence
  - adr-t614-1
  - adr-t614-2
  - adr-t2268-1
  - design-measurement
---

# Source repository

dagqの開発でだけ要る機能は、queueのrepositoryが「dagqのソース」かの判定1つで有効にする（[ADR-t614-1](../../adr/2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)）。設定・flag・環境変数で判定を上書きする手段は無い。

**実装状況**: 判定と表の3行はどれも実装済み。

## 判定

- 読むのはqueueが束縛されたrepositoryのroot（main checkout。`dagq.toml`と同じ場所）の`Cargo.toml`の作業ファイル。`--from`なしの`install`はqueueを開く前に動き、queueが無くても動くので、buildしようとするcheckout（cwdのrepositoryのmain checkout）の`Cargo.toml`を読む。
- `[package]`の表があり、その`name`が文字列`"dagq"`ならソース。
- ファイルが無い、読めない、TOMLとして読めない、`[package]`が無い（`[workspace]`だけ）、`name`が`"dagq"`でない、のどれかならソースではない。
- 機能を使うたびにその時点のファイルで判定し、DBにもsupervisorの登録にも保存しない。
- 実装: 判定は`domain::source_repository::is_source`、ファイルを読むのは`infrastructure::adapters::is_dagq_source(dir)`で、main checkout（`dagq.toml`を読むのと同じ`checkout`）を渡す。

## 対象

新しくdagqの開発でだけ要る機能を足すときは、この判定を使い、この表に行を足す。
この判定に置くのはdagq自身の配布の事情（表の上の3行と、そのinstallと自動更新の前のe2eの関門）だけで、言語・道具の知識は置かない（[ADR-t2268-1](../../adr/2026-10-12-t2268-1-toolchain-knowledge-behind-declared-profiles.md)決定2）。
言語・道具の知識は`dagq.toml`で宣言したtoolchainのprofile、projectの値は`dagq.toml`、hostの値は`host.toml`、言語に依らない知識（hostの分類の汎用の印、待ち・dagq・gitのコマンドの分類）はruntimeの共通の既定に置く。
**実装状況**: profileのportと`[toolchains.<名前>]`の宣言はある（[Toolchain profiles](../toolchain-profiles.md)）。
検証の失敗の分類・落ちたtestとflakyのtestの抜き出し・flakyのやり直しのenv、landing recheckと自動更新のbuildの出力の置き場と掃除、CIの見張りのJUnitのtestの名前、hostの負荷のprocessの種類、hostの`rustc`とtoolchainの記録とKPIの`toolchain`の軸と`stats`の`rustc`の欄は、profileを通して働く。
宣言の無いrepositoryでは、dagqのソースでもどのprofileも働かない（ADR-t2268-1決定3）。
計測のコマンドの分類と回数はこの判定で分けず、効く規則の重ね合わせで決める（下の「cargo専用の計測」）。
着地の前のe2eを流すかもこの判定で分け、`dagq.toml`の宣言に移すことはADR-t2257-1決定4が決めている。

| 機能 | 決定 | dagqのソース | ソースでないrepository |
| --- | --- | --- | --- |
| `integrate`のmigrationの番号の振り直し（[integrate](integrate.md#migrationの番号の振り直し)） | ADR-0067決定3 | 動く（`migration_renumbered`、振り直せなければ`migration_number_taken`の`needs_session`） | 番号を見ない。振り直さず、`migration_number_taken`にもしない。`migrations/`の変更は他のファイルと同じに扱う |
| `--from`なしの`dagq install`（main checkoutからの`cargo build --release --locked`。[install](install.md)の1） | ADR-0073決定14 | 動く | buildせずにerror。`cargo install dagq`で入れ替えるか、`--from`でバイナリかcheckoutを指すよう案内する。`--from`付きと`--rollback`は判定に関係なく動く |
| `up --auto-update`のsource build（[Auto-update](auto-update.md)） | ADR-0073決定17 | 動く | `up`はerrorで止め、supervisorを起動も引き継ぎもしない。自動更新の設定を持つsupervisorもbuildに進まない。外部のprojectの更新は[Release update](release-update.md)（ADR-t618-1） |

### cargo専用の計測

コマンドの分類と回数は`dagq.toml`の規則・宣言したprofileの既定の規則・runtimeの共通の既定の規則の重ね合わせ（[計測のコマンドの形と分類](../measurement-commands.md)）で決め、hostの`rustc`とtoolchainの記録と分け方は宣言したRustのprofileが持つ（ADR-t2268-1決定2・5）。
どれもdagqのソースかの判定では分けない。

- worktime（`src/domain/worktime.rs`）: 区間を閉じるときの分類と数は、どのrepositoryも`domain::command_rules::work_rules`の効く規則で決める。
  `integrate`と重なる検証の数は、効く規則のどれかが`full_test`・`llvm_cov`・`e2e`の分類を与えるときだけ数える。
  宣言も規則も無ければ（dagqのソースでも）共通の規則だけで、cargoのコマンドは未分類、数は出さない。
  規則は区間を閉じるたびに`infrastructure::sessions::work_rules`が、queueの束縛からmain checkoutを求めて`dagq.toml`で決め、束縛の無いqueueと読めない`dagq.toml`は宣言も規則も無い。
- `stats`のcargo専用の欄: 効く規則が`integrate`の検証を数えなければ`runs`と全ての群で欄ごと出さず、欄は`domain::stats::without_cargo_measures`のdoc commentが持つ。
- claimのRustのtoolchainの属性（`run_claimed`）と[`kpi`](commands.md)の`toolchain=`の層と`stats`の`rustc`の欄: 宣言したprofileにRustのprofileがあるかで分ける（`Toolchains::records_rust_toolchain`）。
  当たらなければ（宣言の無いdagqのソースも）supervisorはRustのprofileの`rustc -vV`を実行せず欄ごと書かず、`kpi`は`--by toolchain`でも層を出さず、`stats`は`rustc`の欄を`runs`と`versions`で出さない（`domain::stats::without_rustc`）。
- 出力の判定は`stats`・`kpi`・日次のレポート・observerの入力を出すたびに`compose`がmain checkoutの`dagq.toml`で行う。

### 言語・道具の知識の今の場所

この判定で分けず、道具の知識はRustのprofileの実装が持ち、宣言したrepositoryでだけ働くもの（ADR-t2268-1決定1・2）。
言語に依らない部分はruntimeの共通の既定に、このrepositoryに固有の部分は`dagq.toml`に置く（ADR-t2268-1決定2・4）。

- 検証の失敗の分類と落ちたtest・flakyのtestの抜き出し、flakyのやり直しのenvと受け継いだ値を落とす扱いは、profileに移った（汎用の印は共通の分類、cargo・nextest・rustfmtの印とenvはRustのprofile。[Toolchain profiles](../toolchain-profiles.md)）。
  `tests/common`の`within`の時間切れの行はprojectに固有の印で、共通の分類にもRustのprofileにも置かず、`dagq.toml`の`[verify_marks.<name>]`で宣言する（`domain::verify_marks`）。
- landing recheckと自動更新のbuildの出力の置き場と掃除はprofileに移った（`ToolchainProfile::build_output`）。
  自動更新のsource buildは宣言に依らずRustのprofileのものを使う。
  workerとintegrateのbuildの出力の置き場は`[run.env]`のprojectの値を`infrastructure::run_env`が読むだけで、runtimeに雛形は無い（決まった形ではprofileの推奨の案として出す。ADR-t2268-1決定1・7）。
- CIの見張りのJUnitのtestの名前とprocessのcargo・rustcの種類はprofileに移った（`ToolchainProfile`）。
  jobとstepの名前は道具に依らずGitHubのものを読み、claude・dagq・そのほかのprocessの種類はruntimeの共通の既定（`domain::host_metrics::process_kind`）に残す。
- e2eの関門（`e2e_gate`）が`CARGO_TARGET_DIR`を自前で入れることと、`--from`なしの`install`（`LocalBinaries::build`）が`CARGO_TARGET_DIR`と`<checkout>/target`を読むことはprofileに移さず、dagq自身のcheckoutからのinstallと自動更新とそのe2eの関門というruntimeの自前の事情として、dagqのソースかの判定の側に残す（ADR-t2268-1決定2）。

## テスト

- 振り直しのintegration testは`[package] name = "dagq"`のfixtureで動く。
- cargo専用の計測を記録するtestは、Rustのprofileを宣言した`dagq.toml`を持つrepositoryに束縛したqueueで動く。

## リリース済みのmigration

リリース済み（最新の`v*`のtagに含まれる）の`migrations/*.sql`は中身も名前も変えず消さない（[ADR-t614-2](../../adr/2026-09-27-t614-2-released-migrations-are-immutable.md)）。検査の場所と今の状態は[Persistence](../persistence.md)のmigrationの項にある。
