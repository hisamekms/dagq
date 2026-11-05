//! Which store a deployment's queue lives on (ADR-t3981-1 decision 3): the
//! setting of the deployment and not of the repository, so it is read from
//! the process's environment and never from `dagq.toml`.
//!
//! - [`STORE_KIND_ENV`] is `sqlite` or `postgres`. Without it the store is
//!   the local SQLite queue.
//! - [`STORE_CONNECTION_ENV`] is the Postgres connection string, which is
//!   given only with `postgres`.
//!
//! A setting that cannot be used refuses ([`StoreRefusal`]) and never falls
//! back to SQLite, so a deployment that meant its shared store does not
//! start on an empty local queue. `postgres` refuses in every build until
//! the store's ports run on Postgres: choosing a backend that is not there
//! would look like it worked.

use std::{ffi::OsString, fmt};

/// The name of the variable with the store's kind.
pub const STORE_KIND_ENV: &str = "DAGQ_STORE";
/// The name of the variable with the store's connection string, which may
/// hold a password.
pub const STORE_CONNECTION_ENV: &str = "DAGQ_DATABASE_URL";

/// The store a deployment chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreSetting {
    Sqlite,
    Postgres {
        connection: Option<ConnectionString>,
    },
}

/// A connection string: no `Debug` or error prints it, since it may hold a
/// password.
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectionString(String);

impl ConnectionString {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The text, for the driver alone.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ConnectionString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConnectionString(<redacted>)")
    }
}

/// Why a process does not start on the setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreRefusal {
    /// The kind is neither `sqlite` nor `postgres`. The value is not kept: a
    /// connection string given in the wrong variable holds a password.
    UnknownKind,
    /// The variable of this name is not text. It is not read as unset, which
    /// would mean SQLite.
    NotText(&'static str),
    /// A connection string without the kind that reads it.
    ConnectionWithoutPostgres,
    /// `postgres`, in a build without the feature.
    NoPostgresFeature,
    /// `postgres`, without a connection string.
    NoConnection,
    /// `postgres`, in a build with the feature, while the store's ports do
    /// not run on Postgres.
    PostgresNotUsableYet,
}

impl fmt::Display for StoreRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKind => {
                write!(f, "{STORE_KIND_ENV} takes `sqlite` or `postgres`")
            }
            Self::NotText(name) => write!(f, "{name} is not UTF-8 text"),
            Self::ConnectionWithoutPostgres => write!(
                f,
                "{STORE_CONNECTION_ENV} is set but {STORE_KIND_ENV} is not `postgres`"
            ),
            Self::NoPostgresFeature => write!(
                f,
                "{STORE_KIND_ENV} is `postgres` but this build has no Postgres store: \
                 it is built without the cargo feature `postgres`"
            ),
            Self::NoConnection => write!(
                f,
                "{STORE_KIND_ENV} is `postgres` but {STORE_CONNECTION_ENV} is not set"
            ),
            Self::PostgresNotUsableYet => write!(
                f,
                "{STORE_KIND_ENV} is `postgres` but the Postgres store cannot be used yet: \
                 the store's ports do not run on it"
            ),
        }?;
        f.write_str("; no queue was opened")
    }
}

impl std::error::Error for StoreRefusal {}

impl StoreSetting {
    /// The setting `env` holds, where an empty variable is one not set.
    pub fn from_env(env: impl Fn(&str) -> Option<OsString>) -> Result<Self, StoreRefusal> {
        let set = |name: &'static str| {
            env(name)
                .filter(|value| !value.is_empty())
                .map(|value| value.into_string().map_err(|_| StoreRefusal::NotText(name)))
                .transpose()
        };
        let connection = set(STORE_CONNECTION_ENV)?.map(ConnectionString::new);
        match set(STORE_KIND_ENV)?.as_deref() {
            None | Some("sqlite") if connection.is_some() => {
                Err(StoreRefusal::ConnectionWithoutPostgres)
            }
            None | Some("sqlite") => Ok(Self::Sqlite),
            Some("postgres") => Ok(Self::Postgres { connection }),
            Some(_) => Err(StoreRefusal::UnknownKind),
        }
    }

    /// Whether a build with (`postgres_built`) or without the feature can
    /// run on this setting.
    pub fn check_usable(&self, postgres_built: bool) -> Result<(), StoreRefusal> {
        match self {
            Self::Sqlite => Ok(()),
            Self::Postgres { .. } if !postgres_built => Err(StoreRefusal::NoPostgresFeature),
            Self::Postgres { connection: None } => Err(StoreRefusal::NoConnection),
            Self::Postgres {
                connection: Some(_),
            } => Err(StoreRefusal::PostgresNotUsableYet),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setting(kind: Option<&str>, connection: Option<&str>) -> Result<StoreSetting, StoreRefusal> {
        StoreSetting::from_env(|name| match name {
            STORE_KIND_ENV => kind.map(OsString::from),
            STORE_CONNECTION_ENV => connection.map(OsString::from),
            _ => None,
        })
    }

    const URL: &str = "postgres://dagq:hunter2@db/queue";

    #[test]
    fn no_setting_is_the_local_sqlite_queue_in_every_build() {
        for kind in [None, Some(""), Some("sqlite")] {
            let found = setting(kind, None).unwrap();
            assert_eq!(found, StoreSetting::Sqlite);
            assert_eq!(found.check_usable(false), Ok(()));
            assert_eq!(found.check_usable(true), Ok(()));
        }
        assert_eq!(setting(None, Some("")), Ok(StoreSetting::Sqlite));
    }

    /// The reason names what would let it through, by build.
    #[test]
    fn postgres_refuses_in_every_build_with_the_reason_of_that_build() {
        let chosen = setting(Some("postgres"), Some(URL)).unwrap();
        assert_eq!(
            chosen,
            StoreSetting::Postgres {
                connection: Some(ConnectionString::new(URL))
            }
        );
        let without = chosen.check_usable(false).unwrap_err();
        assert_eq!(without, StoreRefusal::NoPostgresFeature);
        assert!(without.to_string().contains("cargo feature `postgres`"));
        let with = chosen.check_usable(true).unwrap_err();
        assert_eq!(with, StoreRefusal::PostgresNotUsableYet);
        assert!(with.to_string().contains("cannot be used yet"));
        let no_url = setting(Some("postgres"), None).unwrap();
        assert_eq!(
            no_url.check_usable(false),
            Err(StoreRefusal::NoPostgresFeature)
        );
        assert_eq!(no_url.check_usable(true), Err(StoreRefusal::NoConnection));
    }

    /// What was not meant for SQLite does not land on it.
    #[test]
    fn a_setting_that_reads_two_ways_refuses_and_does_not_mean_sqlite() {
        for kind in ["mysql", "Postgres"] {
            assert_eq!(setting(Some(kind), None), Err(StoreRefusal::UnknownKind));
        }
        for kind in [None, Some("sqlite")] {
            assert_eq!(
                setting(kind, Some(URL)),
                Err(StoreRefusal::ConnectionWithoutPostgres)
            );
        }
    }

    #[test]
    fn a_variable_that_is_not_text_refuses_and_is_not_read_as_unset() {
        use std::os::unix::ffi::OsStringExt;
        for name in [STORE_KIND_ENV, STORE_CONNECTION_ENV] {
            let found = StoreSetting::from_env(|asked| {
                (asked == name).then(|| OsString::from_vec(vec![0xff]))
            });
            assert_eq!(found, Err(StoreRefusal::NotText(name)));
        }
    }

    #[test]
    fn no_refusal_or_debug_prints_the_connection_string() {
        let chosen = setting(Some("postgres"), Some(URL)).unwrap();
        assert!(!format!("{chosen:?}").contains("hunter2"));
        for built in [false, true] {
            let refusal = chosen.check_usable(built).unwrap_err();
            assert!(!refusal.to_string().contains("hunter2"));
            assert!(refusal.to_string().ends_with("no queue was opened"));
        }
        let misplaced = setting(None, Some(URL)).unwrap_err();
        assert!(!misplaced.to_string().contains("hunter2"));
        let in_the_wrong_variable = setting(Some(URL), None).unwrap_err();
        assert!(!format!("{in_the_wrong_variable:?}").contains("hunter2"));
        assert!(!in_the_wrong_variable.to_string().contains("hunter2"));
    }
}
