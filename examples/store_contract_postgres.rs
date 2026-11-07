//! CI-only boundary runner. The supplied database must be empty; migration
//! applies the real Postgres series before any case opens its own connection.
//! No default connection and no successful skip when configuration is absent.
use anyhow::{Context, Result, ensure};
use dagq::application::WriteLeaderStore;
use dagq::domain::store_setting::ConnectionString;
use dagq::infrastructure::postgres::client;
use dagq::infrastructure::postgres::{PostgresSession, PostgresStore};
use dagq::infrastructure::sql_exec::{SqlRead, SqlSavepoints, SqlWrite, TransactionEntries};
use dagq::infrastructure::sqlite::WriteIdentity;
use dagq::infrastructure::store_contract;

fn main() -> Result<()> {
    let connection = std::env::var("DAGQ_TEST_POSTGRES_URL").ok();
    store_contract::run_configured(connection.as_deref(), |url| {
        let connection = ConnectionString::new(url);
        client::migrate(&connection)?;
        store_contract::run(&|| client::connect(&connection))?;
        store_contract::postgres_search_writes(&|| client::connect(&connection))?;
        let store = client::connect(&connection)?;
        let generation = store
            .take_write_leader("commit-cases", std::time::Duration::from_secs(300), false)?
            .context("take leader for Postgres commit cases")?;
        let store = store.with_write_identity(WriteIdentity {
            holder: Some("commit-cases".into()),
            generation,
        });
        swallowed_statement_error_rolls_back(&store)
            .context("Postgres swallowed statement error")?;
        savepoint_rollback_allows_commit(&store).context("Postgres savepoint recovery")?;
        ensure!(store.release_write_leader("commit-cases", generation)?);
        Ok(())
    })
}

// Postgres-specific: SQLite permits commit after this statement error.
fn swallowed_statement_error_rolls_back(store: &PostgresStore<impl PostgresSession>) -> Result<()> {
    let tx = store.write_entry("swallowed-error")?;
    tx.execute(
        "CREATE TEMP TABLE commit_probe (id BIGINT PRIMARY KEY)",
        &[],
    )?;
    tx.commit()?;
    let tx = store.write_entry("swallowed-error")?;
    tx.execute("INSERT INTO commit_probe VALUES (1)", &[])?;
    ensure!(
        tx.execute("INSERT INTO commit_probe VALUES (1)", &[])
            .is_err()
    );
    let error = tx
        .commit()
        .err()
        .context("a rejected statement must prevent commit")?;
    ensure!(error.to_string().contains("swallowed-error"));
    let tx = store.read_entry("verify-rollback")?;
    ensure!(
        tx.query_row("SELECT count(*) FROM commit_probe", &[], |r| r
            .get::<_, i64>(0))?
            == 0
    );
    tx.commit()?;
    Ok(())
}

fn savepoint_rollback_allows_commit(store: &PostgresStore<impl PostgresSession>) -> Result<()> {
    let mut tx = store.write_entry("savepoint-recovery")?;
    tx.execute("INSERT INTO commit_probe VALUES (1)", &[])?;
    {
        let savepoint = tx.savepoint()?;
        savepoint.execute("INSERT INTO commit_probe VALUES (2)", &[])?;
        ensure!(
            savepoint
                .execute("INSERT INTO commit_probe VALUES (2)", &[])
                .is_err()
        );
        // Dropping the guard recovers the aborted transaction.
    }
    tx.commit()?;
    let tx = store.read_entry("verify-savepoint")?;
    let ids = tx.query_map("SELECT id FROM commit_probe ORDER BY id", &[], |r| {
        r.get::<_, i64>(0)
    })?;
    ensure!(
        ids == [1],
        "only the row before the savepoint must remain: {ids:?}"
    );
    tx.commit()?;
    Ok(())
}
