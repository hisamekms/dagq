//! CI-only boundary runner. The supplied database must be empty; migration
//! applies the real Postgres series before any case opens its own connection.
//! No default connection and no successful skip when configuration is absent.
use anyhow::Result;
use dagq::domain::store_setting::ConnectionString;
use dagq::infrastructure::postgres::client;
use dagq::infrastructure::store_contract;

fn main() -> Result<()> {
    let connection = std::env::var("DAGQ_TEST_POSTGRES_URL").ok();
    store_contract::run_configured(connection.as_deref(), |url| {
        let connection = ConnectionString::new(url);
        client::migrate(&connection)?;
        store_contract::run(&|| client::connect(&connection))
    })
}
