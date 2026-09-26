---
id: design-supervisor-lifecycle-source-repository
type: design
title: "Source repository"
status: current
created: 2026-09-27
updated: 2026-09-27
last_verified: 2026-09-27
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
---

# Source repository

dagqの開発でだけ要る機能は、queueのrepositoryが「dagqのソース」かの判定1つで有効にする（[ADR-t614-1](../../adr/2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)）。設定・flag・環境変数で判定を上書きする手段は無い。

**実装状況**: 判定と下の表の振る舞いはまだ実装していない（goal 52の後続のtask）。実装が入るまでは、表の機能はどのrepositoryのqueueでも動く。

## 判定

- 読むのはqueueが束縛されたrepositoryのroot（main checkout。`dagq.toml`と同じ場所）の`Cargo.toml`の作業ファイル。`--from`なしの`install`はqueueを開く前に動き、queueが無くても動くので、buildしようとするcheckout（cwdのrepositoryのmain checkout）の`Cargo.toml`を読む。
- `[package]`の表があり、その`name`が文字列`"dagq"`ならソース。
- ファイルが無い、読めない、TOMLとして読めない、`[package]`が無い（`[workspace]`だけ）、`name`が`"dagq"`でない、のどれかならソースではない。
- 機能を使うたびにその時点のファイルで判定し、DBにもsupervisorの登録にも保存しない。

## 対象

新しくdagqの開発でだけ要る機能を足すときは、この判定を使い、この表に行を足す。

| 機能 | 決定 | dagqのソース | ソースでないrepository |
| --- | --- | --- | --- |
| `integrate`のmigrationの番号の振り直し（[integrate](integrate.md)の5） | ADR-0067決定3 | 今までどおり（`migration_renumbered`、振り直せなければ`migration_number_taken`の`needs_session`） | 番号を見ない。振り直さず、`migration_number_taken`にもしない。`migrations/`の変更は他のファイルと同じに扱う |
| `--from`なしの`dagq install`（main checkoutからの`cargo build --release --locked`。[install](install.md)の1） | ADR-0073決定14 | 今までどおり | buildせずにerror。`cargo install dagq`で入れ替えるか、`--from`でバイナリかcheckoutを指すよう案内する。`--from`付きと`--rollback`は判定に関係なく動く |
| `up --auto-update`のsource build（[Auto-update](auto-update.md)） | ADR-0073決定17 | 今までどおり | `up`はerrorで止め、supervisorを起動も引き継ぎもしない。自動更新の設定を持つsupervisorもbuildに進まない。外部のprojectの更新は別のADR |
| `stats`・KPI・worktimeのcargo専用の計測（[stats](stats.md)） | — | 今までどおり | 記録も出力もしない（下の一覧） |

### cargo専用の計測

ソースでないrepositoryのqueueで記録も出力もしないもの。

- worktime（`src/domain/worktime.rs`）: commandの分類のうち`e2e`（`--test e2e`）・`llvm_cov`・`test`（`cargo test`と`cargo nextest`）の判定と、`full_tests`（全体の`cargo test`）・`llvm_cov_runs`・`verification_repeats`（integrateと重なるllvm-cov・全体のtest・e2e）の数。
- `stats`の`work_breakdown`の`test_with_llvm_cov`（`src/domain/stats/work.rs`）。
- claimの属性の`rustc_release`・`rustc_host`（`run_claimed`）と、`stats`の`versions.rustc`、[`kpi`](kpi.md)の`toolchain=`の層。

## リリース済みのmigration

リリース済み（最新の`v*`のtagに含まれる）の`migrations/*.sql`は中身も名前も変えず消さない（[ADR-t614-2](../../adr/2026-09-27-t614-2-released-migrations-are-immutable.md)）。検査の場所と今の状態は[Persistence](../persistence.md)のmigrationの項にある。
