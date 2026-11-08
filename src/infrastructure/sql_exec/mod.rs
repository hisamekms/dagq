//! The execution port of the store's shared SQL (ADR-t3982-1 decision 1): a
//! thin layer between the SQL both adapters run and the driver under it.
//! A store module written against [`SqlRead`] and [`SqlWrite`] names no
//! driver and runs on whichever transaction its adapter's entry hands it.
//!
//! The shape follows the rusqlite calls it replaces, so moving a module is a
//! change of types and of how its arguments are written and not of its SQL:
//! `execute`, `query_row` and `query_map` take the same statement, a row's
//! `get` takes a column's index or name with the same turbofish, and
//! `.optional()` reads as before ([`OptionalRow`]). `params![a, b]` becomes
//! [`sql_params!`]`[a, b]`. `query_map` collects its rows.
//!
//! A statement spells its parameters as SQLite does (`?1`, `?`); an adapter
//! for another dialect maps that spelling to its own. An expression whose
//! spelling differs comes from the transaction's [`SqlRead::dialect`]
//! ([`SqlDialect`]).
//!
//! An error says what went wrong with a row in the port's terms
//! ([`SqlError`]) and carries a failure of the driver as it is, with whether
//! it means another connection held the lock. Its text is the text of the
//! rusqlite error it stands for, so a moved module's messages do not change.
//!
//! While modules move one by one, [`sqlite`] keeps them working together:
//! the transaction of an entry still derefs to rusqlite's, and
//! [`sqlite::port_tx`] hands a `Connection` an unmoved module holds to what
//! has moved.

pub(crate) mod commit_actions;
#[cfg(test)]
mod run_events_read_plans;
pub mod sqlite;
use commit_actions::CommitState;
pub(crate) use commit_actions::{CommitActions, flush_after_commit};
mod values;

pub use super::sql_dialect::SqlDialect;
pub use values::{FromSqlValue, SqlValue, ToSqlValue, ValueError, ValueKind};

/// A failure of the execution port.
#[derive(Debug)]
pub enum SqlError {
    /// [`SqlRead::query_row`] found no row; see [`OptionalRow`].
    NoRows,
    /// No column of this name in the row.
    NoColumn(String),
    /// No column at this index in the row.
    NoColumnIndex(usize),
    /// The column's stored type is not one the type asked for reads.
    WrongType {
        index: usize,
        name: String,
        stored: ValueKind,
    },
    /// An integer outside the range of the type asked for.
    OutOfRange { index: usize, value: i64 },
    /// The column read, but its value does not convert: a stored value the
    /// domain refuses, with the refusal as `cause`.
    Conversion {
        index: usize,
        stored: ValueKind,
        cause: Box<dyn std::error::Error + Send + Sync>,
    },
    /// A failure of the driver. `contention` is whether another connection
    /// held the lock the statement needed, a failure that passes.
    Driver {
        contention: bool,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl SqlError {
    /// Whether the statement failed because another connection held the
    /// lock.
    pub fn is_contention(&self) -> bool {
        matches!(
            self,
            Self::Driver {
                contention: true,
                ..
            }
        )
    }
}

impl std::fmt::Display for SqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRows => write!(f, "Query returned no rows"),
            Self::NoColumn(name) => write!(f, "Invalid column name: {name}"),
            Self::NoColumnIndex(index) => write!(f, "Invalid column index: {index}"),
            Self::WrongType {
                index,
                name,
                stored,
            } => write!(
                f,
                "Invalid column type {stored} at index: {index}, name: {name}"
            ),
            Self::OutOfRange { index, value } => {
                write!(f, "Integer {value} out of range at index {index}")
            }
            Self::Conversion {
                index,
                stored,
                cause,
            } => write!(
                f,
                "Conversion error from type {stored} at index: {index}, {cause}"
            ),
            Self::Driver { source, .. } => source.fmt(f),
        }
    }
}

impl std::error::Error for SqlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Conversion { cause, .. } => Some(&**cause),
            // Transparent: the driver's error is this error.
            Self::Driver { source, .. } => source.source(),
            _ => None,
        }
    }
}

pub type SqlResult<T> = Result<T, SqlError>;

/// `.optional()` of a [`SqlRead::query_row`]: no row is `None`.
pub trait OptionalRow<T> {
    fn optional(self) -> SqlResult<Option<T>>;
}

impl<T> OptionalRow<T> for SqlResult<T> {
    fn optional(self) -> SqlResult<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(SqlError::NoRows) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// What an adapter gives of the row its driver is on.
pub trait RowValues {
    /// The index of the column `name`, or [`SqlError::NoColumn`].
    fn column_index(&self, name: &str) -> SqlResult<usize>;
    /// The name of the column at `index`, for an error.
    fn column_name(&self, index: usize) -> String;
    /// The value of the column at `index`, or [`SqlError::NoColumnIndex`].
    fn value(&self, index: usize) -> SqlResult<SqlValue>;
}

/// A column of a row: its index, or its name.
pub trait ColumnRef {
    fn index_in(&self, row: &dyn RowValues) -> SqlResult<usize>;
}

impl ColumnRef for usize {
    fn index_in(&self, _: &dyn RowValues) -> SqlResult<usize> {
        Ok(*self)
    }
}

impl ColumnRef for &str {
    fn index_in(&self, row: &dyn RowValues) -> SqlResult<usize> {
        row.column_index(self)
    }
}

/// The row a statement is on, read by column.
#[derive(Clone, Copy)]
pub struct SqlRow<'r> {
    values: &'r dyn RowValues,
}

impl<'r> SqlRow<'r> {
    pub fn new(values: &'r dyn RowValues) -> Self {
        Self { values }
    }

    /// The value of `column` as `T`; see [`FromSqlValue`].
    pub fn get<C: ColumnRef, T: FromSqlValue>(&self, column: C) -> SqlResult<T> {
        let index = column.index_in(self.values)?;
        let value = self.values.value(index)?;
        let stored = value.kind();
        T::from_sql_value(value).map_err(|error| match error {
            ValueError::WrongType => SqlError::WrongType {
                index,
                name: self.values.column_name(index),
                stored,
            },
            ValueError::OutOfRange(value) => SqlError::OutOfRange { index, value },
            ValueError::Other(cause) => SqlError::Conversion {
                index,
                stored,
                cause,
            },
        })
    }

    /// The index of the column `name`.
    pub fn column_index(&self, name: &str) -> SqlResult<usize> {
        self.values.column_index(name)
    }

    /// Whether the query selected a column `name`.
    pub fn has_column(&self, name: &str) -> bool {
        self.values.column_index(name).is_ok()
    }
}

/// The transaction or connection a store module reads through. An adapter
/// implements [`SqlRead::dialect`] and [`SqlRead::visit_rows`].
pub trait SqlRead {
    /// The dialect the statements of this transaction are run in.
    fn dialect(&self) -> SqlDialect;

    /// Reserve the ID used to construct an aggregate before its explicit-ID
    /// insert in this writer transaction. SQLite predicts AUTOINCREMENT
    /// without consuming it; Postgres consumes the identity sequence. Like
    /// insert_id, this adapter operation owns how an ID is obtained.
    /// `table` is an internal table name, never user input.
    fn next_id(&self, table: &str) -> SqlResult<i64> {
        let sql = match self.dialect() {
            SqlDialect::Sqlite => sqlite::next_id_sql(table),
            SqlDialect::Postgres => "SELECT nextval(pg_get_serial_sequence(?1, 'id'))".to_owned(),
        };
        self.query_row(&sql, sql_params![table], |row| row.get(0))
    }

    /// Whether this connection is inside a write transaction. Readers may
    /// perform transcript I/O only when this is false.
    fn in_write_transaction(&self) -> bool {
        false
    }

    /// Run the query `sql` with `params` and call `visit` on each row until
    /// it returns `false` or the rows end.
    fn visit_rows(
        &self,
        sql: &str,
        params: &[SqlValue],
        visit: &mut dyn FnMut(&SqlRow<'_>) -> SqlResult<bool>,
    ) -> SqlResult<()>;

    /// Whether the query yields at least one row.
    fn exists(&self, sql: &str, params: &[SqlValue]) -> SqlResult<bool> {
        Ok(self
            .query_row(sql, params, |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// What `read` makes of the query's first row, or [`SqlError::NoRows`].
    fn query_row<T>(
        &self,
        sql: &str,
        params: &[SqlValue],
        read: impl FnOnce(&SqlRow<'_>) -> SqlResult<T>,
    ) -> SqlResult<T> {
        let mut read = Some(read);
        let mut found = None;
        self.visit_rows(sql, params, &mut |row| {
            if let Some(read) = read.take() {
                found = Some(read(row)?);
            }
            Ok(false)
        })?;
        found.ok_or(SqlError::NoRows)
    }

    /// What `read` makes of each row of the query, in order.
    fn query_map<T>(
        &self,
        sql: &str,
        params: &[SqlValue],
        mut read: impl FnMut(&SqlRow<'_>) -> SqlResult<T>,
    ) -> SqlResult<Vec<T>> {
        let mut found = Vec::new();
        self.visit_rows(sql, params, &mut |row| {
            found.push(read(row)?);
            Ok(true)
        })?;
        Ok(found)
    }
}

/// The transaction a store module writes through.
pub trait SqlWrite: SqlRead {
    /// Register an action for this transaction's successful commit. A rollback
    /// or failed commit discards it. A savepoint owns its registrations until
    /// release; rolling it back discards them too. SQLite defers actions until
    /// the caller has left the commit statement (see `flush_after_commit`).
    fn after_commit(&self, action: Box<dyn FnOnce()>) -> SqlResult<()>;

    /// Run the write statement `sql` and return the rows it changed.
    fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<usize>;

    /// Run the insert `sql` and return the ID of the row it inserted: the
    /// one way a store module reads a new row's ID (ADR-t3982-1 decision 4).
    /// An insert that may insert no row takes
    /// [`SqlWrite::insert_id_if_inserted`].
    fn insert_id(&self, sql: &str, params: &[SqlValue]) -> SqlResult<i64>;

    /// Run the insert `sql` and return the ID of the row it inserted, or
    /// `None` when it inserted none.
    fn insert_id_if_inserted(&self, sql: &str, params: &[SqlValue]) -> SqlResult<Option<i64>>;
}

/// A write transaction that can isolate a group of writes. The mutable borrow
/// keeps the outer transaction open and unused until the guard ends.
pub trait SqlSavepoints: SqlWrite {
    type Savepoint<'tx>: SqlSavepoint
    where
        Self: 'tx;

    /// Begin a savepoint. Dropping its guard without committing rolls back
    /// only its writes; the outer transaction can keep writing and commit.
    fn savepoint(&mut self) -> anyhow::Result<Self::Savepoint<'_>>;
}

/// A savepoint uses the same execution port as its outer transaction.
pub trait SqlSavepoint: SqlWrite {
    /// Keep these writes in the outer transaction, which still must commit
    /// for them to persist. Consumes the guard as rusqlite's savepoint does.
    fn commit(self) -> anyhow::Result<()>;
}

/// The named entries of a store's transactions (ADR-t3982-1 decision 2): the
/// one way a transaction begins, whichever the adapter. A write entry fences
/// the caller as its first statement and fails with
/// [`crate::application::NotLeader`] when the caller is not the leader. How a
/// transaction begins is the adapter's and no caller's to choose.
pub trait TransactionEntries {
    /// The transaction of a write entry.
    type Write<'store>: SqlSavepoints
    where
        Self: 'store;
    /// The transaction of a read entry, which reads one snapshot.
    type Read<'store>: SqlRead
    where
        Self: 'store;

    /// Begin the write transaction `name`.
    fn write_entry(&self, name: &'static str) -> anyhow::Result<Self::Write<'_>>;

    /// Begin the read transaction `name`.
    fn read_entry(&self, name: &'static str) -> anyhow::Result<Self::Read<'_>>;
}

/// A transaction whose owner can finish it after writing shared SQL.
pub(crate) trait CommitTransaction: SqlWrite {
    fn commit(self) -> anyhow::Result<()>;
}

/// The connection used by transcript intake: read before opening each short
/// fenced write, and commit that write before reading the next transcript.
/// The adapter owns the concrete transaction and its lock accounting.
pub(crate) trait SessionConnection: SqlRead {
    type Write<'c>: CommitTransaction
    where
        Self: 'c;
    fn session_write(
        &self,
        name: &'static str,
        warn: std::time::Duration,
        identity: &crate::infrastructure::write_leader::WriteIdentity,
    ) -> anyhow::Result<Self::Write<'_>>;
}

/// The arguments of a statement of the port, each bound as its
/// [`ToSqlValue`]: what `rusqlite::params!` is to rusqlite's.
macro_rules! sql_params {
    ($($value:expr),* $(,)?) => {
        &[$($crate::infrastructure::sql_exec::ToSqlValue::to_sql_value(&$value)),*]
            as &[$crate::infrastructure::sql_exec::SqlValue]
    };
}
pub(crate) use sql_params;

#[cfg(test)]
mod tests {
    use super::*;

    /// A row of `(name, value)` columns, which is all a row of the port is.
    struct Columns(Vec<(&'static str, SqlValue)>);

    impl RowValues for Columns {
        fn column_index(&self, name: &str) -> SqlResult<usize> {
            self.0
                .iter()
                .position(|(column, _)| *column == name)
                .ok_or_else(|| SqlError::NoColumn(name.to_owned()))
        }

        fn column_name(&self, index: usize) -> String {
            self.0[index].0.to_owned()
        }

        fn value(&self, index: usize) -> SqlResult<SqlValue> {
            self.0
                .get(index)
                .map(|(_, value)| value.clone())
                .ok_or(SqlError::NoColumnIndex(index))
        }
    }

    /// A transaction whose every query returns the rows of [`Columns`].
    struct Rows(Vec<Vec<(&'static str, SqlValue)>>);

    impl SqlRead for Rows {
        fn dialect(&self) -> SqlDialect {
            SqlDialect::Sqlite
        }

        fn visit_rows(
            &self,
            _: &str,
            _: &[SqlValue],
            visit: &mut dyn FnMut(&SqlRow<'_>) -> SqlResult<bool>,
        ) -> SqlResult<()> {
            for row in &self.0 {
                let row = Columns(row.clone());
                if !visit(&SqlRow::new(&row))? {
                    break;
                }
            }
            Ok(())
        }
    }

    #[test]
    fn next_id_is_an_adapter_operation_with_a_postgres_sequence_reservation() {
        struct Capture(std::cell::RefCell<String>);
        impl SqlRead for Capture {
            fn dialect(&self) -> SqlDialect {
                SqlDialect::Postgres
            }
            fn visit_rows(
                &self,
                sql: &str,
                params: &[SqlValue],
                visit: &mut dyn FnMut(&SqlRow<'_>) -> SqlResult<bool>,
            ) -> SqlResult<()> {
                *self.0.borrow_mut() = sql.to_owned();
                assert_eq!(params, sql_params!["tasks"]);
                visit(&SqlRow::new(&Columns(vec![(
                    "nextval",
                    SqlValue::Integer(42),
                )])))?;
                Ok(())
            }
        }
        let conn = Capture(std::cell::RefCell::new(String::new()));
        assert_eq!(conn.next_id("tasks").unwrap(), 42);
        assert_eq!(
            *conn.0.borrow(),
            "SELECT nextval(pg_get_serial_sequence(?1, 'id'))"
        );
    }

    #[test]
    fn a_row_reads_a_column_by_index_or_by_name() {
        let row = Columns(vec![
            ("id", SqlValue::Integer(4)),
            ("load", SqlValue::Real(0.5)),
            ("done", SqlValue::Integer(1)),
            ("note", SqlValue::Null),
        ]);
        let row = SqlRow::new(&row);
        assert_eq!(row.get::<_, i64>(0).unwrap(), 4);
        assert_eq!(row.get::<_, i64>("id").unwrap(), 4);
        assert_eq!(row.get::<_, f64>("load").unwrap(), 0.5);
        assert!(row.get::<_, bool>("done").unwrap());
        assert_eq!(row.get::<_, Option<String>>("note").unwrap(), None);
        assert!(row.has_column("note"));
        assert!(!row.has_column("nothing"));
        assert_eq!(row.column_index("load").unwrap(), 1);
    }

    #[test]
    fn a_column_that_does_not_read_names_itself_in_the_error() {
        let row = Columns(vec![
            ("id", SqlValue::Integer(-4)),
            ("run", SqlValue::Text(String::new())),
        ]);
        let row = SqlRow::new(&row);
        let wrong = row.get::<_, String>("id").unwrap_err().to_string();
        assert_eq!(wrong, "Invalid column type Integer at index: 0, name: id");
        let range = row.get::<_, u32>("id").unwrap_err().to_string();
        assert_eq!(range, "Integer -4 out of range at index 0");
        assert!(matches!(
            row.get::<_, crate::domain::RunId>("run"),
            Err(SqlError::Conversion {
                index: 1,
                stored: ValueKind::Text,
                ..
            })
        ));
        assert!(matches!(
            row.get::<_, i64>("nothing"),
            Err(SqlError::NoColumn(_))
        ));
        assert!(matches!(
            row.get::<_, i64>(9),
            Err(SqlError::NoColumnIndex(9))
        ));
    }

    #[test]
    fn query_row_takes_the_first_row_and_no_row_is_optional() {
        let rows = Rows(vec![
            vec![("n", SqlValue::Integer(1))],
            vec![("n", SqlValue::Integer(2))],
        ]);
        assert_eq!(rows.query_row("q", &[], |r| r.get::<_, i64>(0)).unwrap(), 1);
        assert_eq!(
            rows.query_map("q", &[], |r| r.get::<_, i64>(0)).unwrap(),
            [1, 2]
        );
        let none = Rows(Vec::new());
        assert!(matches!(
            none.query_row("q", &[], |r| r.get::<_, i64>(0)),
            Err(SqlError::NoRows)
        ));
        assert_eq!(
            none.query_row("q", &[], |r| r.get::<_, i64>(0))
                .optional()
                .unwrap(),
            None
        );
        assert!(
            none.query_map("q", &[], |r| r.get::<_, i64>(0))
                .unwrap()
                .is_empty()
        );
        let failed: SqlResult<i64> = Err(SqlError::NoColumnIndex(1));
        assert!(failed.optional().is_err());
    }

    #[test]
    fn the_macro_binds_each_argument_as_its_value() {
        assert_eq!(
            sql_params![1_i64, "a", Some(0.5_f64), None::<i64>, true],
            [
                SqlValue::Integer(1),
                SqlValue::Text("a".to_owned()),
                SqlValue::Real(0.5),
                SqlValue::Null,
                SqlValue::Bool(true),
            ]
        );
        assert!(sql_params![].is_empty());
    }

    #[test]
    fn only_a_driver_s_failure_on_a_lock_is_contention() {
        let driver = |contention| SqlError::Driver {
            contention,
            source: "locked".into(),
        };
        assert!(driver(true).is_contention());
        assert!(!driver(false).is_contention());
        assert!(!SqlError::NoRows.is_contention());
        assert_eq!(driver(true).to_string(), "locked");
    }
}
