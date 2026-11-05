//! The driver under the Postgres store.
//!
//! The connection string is `postgres`'s (a URL or `key=value` pairs) and
//! this client connects without TLS: `sslmode=require` fails at connecting
//! rather than going on in plain text.

use super::{PostgresSession, PostgresStore};
use crate::domain::store_setting::ConnectionString;
use anyhow::{Context, Result};
use postgres::{Client, NoTls};
use std::cell::RefCell;

/// One connection, which the entries' transactions share one at a time.
pub struct PostgresClient(RefCell<Client>);

/// Connect to the store at `connection`. The errors do not print it.
pub fn connect(connection: &ConnectionString) -> Result<PostgresStore<PostgresClient>> {
    let client = Client::connect(connection.expose(), NoTls).context("connect to Postgres")?;
    PostgresStore::open(PostgresClient(RefCell::new(client)))
}

impl PostgresSession for PostgresClient {
    fn run(&self, sql: &str) -> Result<()> {
        self.0.borrow_mut().batch_execute(sql)?;
        Ok(())
    }

    fn leader_identity(&self, sql: &str) -> Result<(Option<String>, i64)> {
        let row = self.0.borrow_mut().query_one(sql, &[])?;
        Ok((row.try_get(0)?, row.try_get(1)?))
    }

    fn fence(
        &self,
        sql: &str,
        holder: Option<&str>,
        generation: i64,
        lease_ms: i64,
    ) -> Result<u64> {
        Ok(self
            .0
            .borrow_mut()
            .execute(sql, &[&holder, &generation, &lease_ms])?)
    }
}
