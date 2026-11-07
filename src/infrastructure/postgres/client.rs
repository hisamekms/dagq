//! The driver under the Postgres store: it moves a statement's values to and
//! from the wire, in the form [`super::exec`] decides.
//!
//! TLS is required unless plaintext is explicitly selected; policy is in
//! [super::tls]. Connection errors discard driver details, which can contain
//! connection parameters or server-supplied text.

use super::exec::{PostgresRow, decode_column, encode_parameter, session_error};
use super::tls::{Transport, TrustRoots, policy};
use super::{PostgresSession, PostgresStore, migrate};
use crate::domain::store_setting::ConnectionString;
use crate::infrastructure::sql_exec::{SqlError, SqlResult, SqlValue};
use crate::infrastructure::sqlite::MigrationReport;
use anyhow::Result;
use bytes::BytesMut;
use postgres::types::{FromSql, IsNull, ToSql, Type, to_sql_checked};
use postgres::{Client, config::SslMode};
use std::cell::RefCell;
use std::error::Error;
use std::rc::Rc;

/// One connection, which the entries' transactions share one at a time.
pub struct PostgresClient(RefCell<Client>);

/// Connect to the store at `connection`. The errors do not print it.
pub fn connect(connection: &ConnectionString) -> Result<PostgresStore<PostgresClient>> {
    let client = open_client(connection)?;
    PostgresStore::open(PostgresClient(RefCell::new(client)))
}

/// Bring the database at `connection` to this binary's schema from what it
/// has, nothing at all included, refusing a breaking migration
/// ([`migrate::migrate`]). What it did, `None` when nothing was pending.
pub fn migrate(connection: &ConnectionString) -> Result<Option<MigrationReport>> {
    let client = open_client(connection)?;
    migrate::migrate(&PostgresClient(RefCell::new(client)))
}

fn open_client(connection: &ConnectionString) -> Result<Client> {
    let (mut config, transport) = connection_config(connection)?;
    let mut roots = rustls::RootCertStore::empty();
    match transport {
        Transport::Plaintext => {
            config.ssl_mode(SslMode::Disable);
        }
        Transport::Tls(trust) => {
            config.ssl_mode(SslMode::Require);
            match trust {
                TrustRoots::Standard => {
                    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned())
                }
                TrustRoots::Custom(certificates) => {
                    for certificate in certificates {
                        roots
                            .add(rustls::pki_types::CertificateDer::from(certificate.clone()))
                            .map_err(|_| anyhow::anyhow!("invalid Postgres CA certificate"))?;
                    }
                }
            }
        }
    }
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    config
        .connect(tokio_postgres_rustls::MakeRustlsConnect::new(tls))
        .map_err(|_| anyhow::anyhow!("connect to Postgres failed"))
}

fn connection_config(connection: &ConnectionString) -> Result<(postgres::Config, Transport<'_>)> {
    let config: postgres::Config = connection
        .expose()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid Postgres connection settings"))?;
    let mode = match config.get_ssl_mode() {
        SslMode::Disable => "disable",
        SslMode::Prefer => "prefer",
        SslMode::Require => "require",
        _ => anyhow::bail!("unsupported Postgres sslmode"),
    };
    let transport = policy(Some(mode), connection.ca_certificates())?;
    Ok((config, transport))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_keyword_strings_use_the_same_secure_policy() {
        for prefix in [
            "postgres://user:secret@localhost/db",
            "host=localhost password=secret",
        ] {
            for mode in [None, Some("prefer"), Some("require"), Some("disable")] {
                let text = match mode {
                    None => prefix.to_owned(),
                    Some(mode) if prefix.starts_with("postgres:") => {
                        format!("{prefix}?sslmode={mode}")
                    }
                    Some(mode) => format!("{prefix} sslmode={mode}"),
                };
                let connection = ConnectionString::new(text);
                let (_, transport) = connection_config(&connection).unwrap();
                assert_eq!(transport == Transport::Plaintext, mode == Some("disable"));
            }
        }
        let error = connection_config(&ConnectionString::new(
            "password=secret sslmode=verify-full",
        ))
        .unwrap_err();
        assert!(!format!("{error:?}").contains("secret"));
    }
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
    let state = error.code().cloned();
    session_error(Box::new(error), state.as_ref().map(|state| state.code()))
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
