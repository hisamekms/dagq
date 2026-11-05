//! The driver under the Postgres store: it moves a statement's values to and
//! from the wire, in the form [`super::exec`] decides.
//!
//! The connection string is `postgres`'s (a URL or `key=value` pairs) and
//! this client connects without TLS: `sslmode=require` fails at connecting
//! rather than going on in plain text.

use super::exec::{PostgresRow, decode_column, encode_parameter, is_contention_state};
use super::{PostgresSession, PostgresStore};
use crate::domain::store_setting::ConnectionString;
use crate::infrastructure::sql_exec::{SqlError, SqlResult, SqlValue};
use anyhow::{Context, Result};
use bytes::BytesMut;
use postgres::types::{FromSql, IsNull, ToSql, Type, to_sql_checked};
use postgres::{Client, NoTls};
use std::cell::RefCell;
use std::error::Error;
use std::rc::Rc;

/// One connection, which the entries' transactions share one at a time.
pub struct PostgresClient(RefCell<Client>);

/// Connect to the store at `connection`. The errors do not print it.
pub fn connect(connection: &ConnectionString) -> Result<PostgresStore<PostgresClient>> {
    let client = Client::connect(connection.expose(), NoTls).context("connect to Postgres")?;
    PostgresStore::open(PostgresClient(RefCell::new(client)))
}

/// A parameter of whatever type the server inferred.
#[derive(Debug)]
struct Parameter<'v>(&'v SqlValue);

impl ToSql for Parameter<'_> {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        Ok(match encode_parameter(self.0, ty.name())? {
            Some(bytes) => {
                out.extend_from_slice(&bytes);
                IsNull::No
            }
            None => IsNull::Yes,
        })
    }

    /// Whether the value fits the type is [`encode_parameter`]'s to say.
    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

/// A column of whatever type the server described.
struct Column(SqlValue);

impl<'r> FromSql<'r> for Column {
    fn from_sql(ty: &Type, raw: &'r [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        decode_column(ty.name(), Some(raw)).map(Self)
    }

    fn from_sql_null(ty: &Type) -> Result<Self, Box<dyn Error + Sync + Send>> {
        decode_column(ty.name(), None).map(Self)
    }

    /// Whether the type reads is [`decode_column`]'s to say.
    fn accepts(_: &Type) -> bool {
        true
    }
}

fn driver_error(error: postgres::Error) -> SqlError {
    SqlError::Driver {
        contention: error
            .code()
            .is_some_and(|state| is_contention_state(state.code())),
        source: Box::new(error),
    }
}

/// `params` as the driver takes them, handed to `run`.
fn with_parameters<T>(
    params: &[SqlValue],
    run: impl FnOnce(&[&(dyn ToSql + Sync)]) -> Result<T, postgres::Error>,
) -> SqlResult<T> {
    let parameters: Vec<Parameter<'_>> = params.iter().map(Parameter).collect();
    let references: Vec<&(dyn ToSql + Sync)> = parameters
        .iter()
        .map(|parameter| parameter as &(dyn ToSql + Sync))
        .collect();
    run(&references).map_err(driver_error)
}

fn port_row(row: &postgres::Row, columns: &Rc<[String]>) -> SqlResult<PostgresRow> {
    let values = (0..row.len())
        .map(|index| row.try_get::<_, Column>(index).map(|column| column.0))
        .collect::<Result<_, _>>()
        .map_err(driver_error)?;
    Ok(PostgresRow::new(Rc::clone(columns), values))
}

impl PostgresSession for PostgresClient {
    fn run(&self, sql: &str) -> Result<()> {
        self.0.borrow_mut().batch_execute(sql)?;
        Ok(())
    }

    fn query(&self, sql: &str, params: &[SqlValue]) -> SqlResult<Vec<PostgresRow>> {
        let rows = with_parameters(params, |parameters| {
            self.0.borrow_mut().query(sql, parameters)
        })?;
        let Some(first) = rows.first() else {
            return Ok(Vec::new());
        };
        let columns: Rc<[String]> = first
            .columns()
            .iter()
            .map(|column| column.name().to_owned())
            .collect();
        rows.iter().map(|row| port_row(row, &columns)).collect()
    }

    fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<u64> {
        with_parameters(params, |parameters| {
            self.0.borrow_mut().execute(sql, parameters)
        })
    }
}
