//! The execution port on rusqlite: the SQLite adapter's side of
//! [`super::SqlRead`] and [`super::SqlWrite`], the mapping of values and
//! errors to and from rusqlite's, and what keeps a module that has not moved
//! to the port working with what has.
//!
//! A [`WriteTx`], what the named entries hand out, runs the port's statements
//! as its own: a write is counted and held as one through its `execute`. It
//! still derefs to rusqlite's `Transaction` and so to its `Connection`, so a
//! module that has not moved builds as it is. Such a module holds a
//! `Connection` (or a `Transaction`, which derefs to one) and hands it to a
//! moved function with [`port_tx`], and reads a rusqlite `Row` with a moved
//! row helper through [`with_row`], back to rusqlite's errors. A `Connection`
//! is also a [`SqlRead`] as it is: the read connection needs no wrapper.
//!
//! The actor functions are here because registering a function is the
//! driver's call; the state they read is [`super::super::event_actor`]'s.

use rusqlite::{
    Connection, Row,
    functions::FunctionFlags,
    params_from_iter,
    types::{ToSqlOutput, Type, Value},
};

use super::{
    RowValues, SqlDialect, SqlError, SqlRead, SqlResult, SqlRow, SqlValue, SqlWrite,
    TransactionEntries, ValueKind,
};
use crate::domain::actor::ActorContext;
use crate::infrastructure::{
    event_actor::EventActors,
    sqlite::{SqliteQueue, WriteTx, insert_counted},
};

impl rusqlite::ToSql for SqlValue {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Self::Integer(value) => ToSqlOutput::from(*value),
            Self::Real(value) => ToSqlOutput::from(*value),
            Self::Text(value) | Self::Json(value) => ToSqlOutput::from(value.as_str()),
            Self::Bool(value) => ToSqlOutput::from(i64::from(*value)),
            Self::Null => ToSqlOutput::Owned(Value::Null),
        })
    }
}

impl From<Type> for ValueKind {
    fn from(kind: Type) -> Self {
        match kind {
            Type::Null => Self::Null,
            Type::Integer => Self::Integer,
            Type::Real => Self::Real,
            Type::Text => Self::Text,
            Type::Blob => Self::Blob,
        }
    }
}

impl From<ValueKind> for Type {
    fn from(kind: ValueKind) -> Self {
        match kind {
            ValueKind::Null => Self::Null,
            ValueKind::Integer => Self::Integer,
            ValueKind::Real => Self::Real,
            ValueKind::Text => Self::Text,
            ValueKind::Blob => Self::Blob,
        }
    }
}

/// Whether `error` means another connection held the lock (SQLite's busy
/// or locked).
pub(crate) fn is_contention(error: &rusqlite::Error) -> bool {
    error.sqlite_error_code().is_some_and(|code| {
        matches!(
            code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        )
    })
}

/// rusqlite's marker of an error with no column; such an error stays the
/// driver's, whose text says no index.
const UNKNOWN_COLUMN: usize = usize::MAX;

impl From<rusqlite::Error> for SqlError {
    fn from(error: rusqlite::Error) -> Self {
        match error {
            rusqlite::Error::QueryReturnedNoRows => Self::NoRows,
            rusqlite::Error::InvalidColumnName(name) => Self::NoColumn(name),
            rusqlite::Error::InvalidColumnIndex(index) => Self::NoColumnIndex(index),
            rusqlite::Error::InvalidColumnType(index, name, stored) => Self::WrongType {
                index,
                name,
                stored: stored.into(),
            },
            rusqlite::Error::IntegralValueOutOfRange(index, value) if index != UNKNOWN_COLUMN => {
                Self::OutOfRange { index, value }
            }
            rusqlite::Error::FromSqlConversionFailure(index, stored, cause)
                if index != UNKNOWN_COLUMN =>
            {
                Self::Conversion {
                    index,
                    stored: stored.into(),
                    cause,
                }
            }
            error => Self::Driver {
                contention: is_contention(&error),
                source: Box::new(error),
            },
        }
    }
}

impl From<SqlError> for rusqlite::Error {
    fn from(error: SqlError) -> Self {
        match error {
            SqlError::NoRows => Self::QueryReturnedNoRows,
            SqlError::NoColumn(name) => Self::InvalidColumnName(name),
            SqlError::NoColumnIndex(index) => Self::InvalidColumnIndex(index),
            SqlError::WrongType {
                index,
                name,
                stored,
            } => Self::InvalidColumnType(index, name, stored.into()),
            SqlError::OutOfRange { index, value } => Self::IntegralValueOutOfRange(index, value),
            SqlError::Conversion {
                index,
                stored,
                cause,
            } => Self::FromSqlConversionFailure(index, stored.into(), cause),
            SqlError::Driver { source, .. } => match source.downcast::<Self>() {
                Ok(error) => *error,
                Err(source) => Self::ToSqlConversionFailure(source),
            },
        }
    }
}

/// `error` of a moved function, with the port's error it holds outermost as
/// the rusqlite error it stands for, for a caller that has not moved.
pub(crate) fn native_error(error: anyhow::Error) -> anyhow::Error {
    match error.downcast::<SqlError>() {
        Ok(error) => anyhow::Error::new(rusqlite::Error::from(error)),
        Err(error) => error,
    }
}

/// A rusqlite row as the port reads one. Each value comes through `get`, so
/// it fails as rusqlite's own read of that column does, but for one thing:
/// the value is decoded before the type asked for is known, so text that is
/// not UTF-8 fails as text whatever is asked for.
struct RowOf<'a, 'stmt>(&'a Row<'stmt>);

impl RowValues for RowOf<'_, '_> {
    fn column_index(&self, name: &str) -> SqlResult<usize> {
        Ok(self.0.as_ref().column_index(name)?)
    }

    fn column_name(&self, index: usize) -> String {
        self.0
            .as_ref()
            .column_name(index)
            .map_or_else(|_| String::new(), str::to_owned)
    }

    fn value(&self, index: usize) -> SqlResult<SqlValue> {
        let kind = self.0.get_ref(index)?.data_type();
        Ok(match kind {
            Type::Null => SqlValue::Null,
            Type::Integer => SqlValue::Integer(self.0.get(index)?),
            Type::Real => SqlValue::Real(self.0.get(index)?),
            Type::Text => SqlValue::Text(self.0.get(index)?),
            Type::Blob => {
                return Err(SqlError::WrongType {
                    index,
                    name: self.column_name(index),
                    stored: ValueKind::Blob,
                });
            }
        })
    }
}

/// What the moved row helper `read` makes of the rusqlite row `row`, with
/// rusqlite's error: the one thing the helpers' wrappers in
/// `infrastructure::sqlite` do.
pub(crate) fn with_row<T>(
    row: &Row<'_>,
    read: impl FnOnce(&SqlRow<'_>) -> SqlResult<T>,
) -> rusqlite::Result<T> {
    Ok(read(&SqlRow::new(&RowOf(row)))?)
}

fn visit_rows_on(
    conn: &Connection,
    sql: &str,
    params: &[SqlValue],
    visit: &mut dyn FnMut(&SqlRow<'_>) -> SqlResult<bool>,
) -> SqlResult<()> {
    let mut statement = conn.prepare(sql)?;
    let mut rows = statement.query(params_from_iter(params))?;
    while let Some(row) = rows.next()? {
        if !visit(&SqlRow::new(&RowOf(row)))? {
            break;
        }
    }
    Ok(())
}

impl SqlRead for Connection {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Sqlite
    }

    fn visit_rows(
        &self,
        sql: &str,
        params: &[SqlValue],
        visit: &mut dyn FnMut(&SqlRow<'_>) -> SqlResult<bool>,
    ) -> SqlResult<()> {
        visit_rows_on(self, sql, params, visit)
    }
}

impl SqlRead for WriteTx<'_> {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Sqlite
    }

    fn visit_rows(
        &self,
        sql: &str,
        params: &[SqlValue],
        visit: &mut dyn FnMut(&SqlRow<'_>) -> SqlResult<bool>,
    ) -> SqlResult<()> {
        visit_rows_on(self, sql, params, visit)
    }
}

impl SqlWrite for WriteTx<'_> {
    fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<usize> {
        Ok(WriteTx::execute(self, sql, params_from_iter(params))?)
    }

    fn insert_id(&self, sql: &str, params: &[SqlValue]) -> SqlResult<i64> {
        Ok(WriteTx::insert_id(self, sql, params_from_iter(params))?)
    }

    fn insert_id_if_inserted(&self, sql: &str, params: &[SqlValue]) -> SqlResult<Option<i64>> {
        Ok(WriteTx::insert_id_if_inserted(
            self,
            sql,
            params_from_iter(params),
        )?)
    }
}

/// A rusqlite `Connection` or `Transaction` an unmoved module holds, as a
/// transaction of the port ([`port_tx`]). It writes on the transaction the
/// connection is in, as that module's own statements do.
pub(crate) struct SqliteTx<'c>(&'c Connection);

/// `conn` as a transaction of the port, for an unmoved module to call what
/// has moved; a `&Transaction` coerces. Its writes are not counted as a
/// [`WriteTx`]'s, so one who holds a `WriteTx` hands that on as it is. It
/// begins nothing, and
/// `SqliteQueue::conn` is not handed to it to write outside a transaction
/// (`context_boundaries::write_tx`).
pub(crate) fn port_tx(conn: &Connection) -> SqliteTx<'_> {
    SqliteTx(conn)
}

impl SqlRead for SqliteTx<'_> {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Sqlite
    }

    fn visit_rows(
        &self,
        sql: &str,
        params: &[SqlValue],
        visit: &mut dyn FnMut(&SqlRow<'_>) -> SqlResult<bool>,
    ) -> SqlResult<()> {
        visit_rows_on(self.0, sql, params, visit)
    }
}

impl SqlWrite for SqliteTx<'_> {
    fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<usize> {
        Ok(self.0.execute(sql, params_from_iter(params))?)
    }

    fn insert_id(&self, sql: &str, params: &[SqlValue]) -> SqlResult<i64> {
        let (_, id) = insert_counted(self.0, sql, params_from_iter(params))?;
        Ok(id)
    }

    fn insert_id_if_inserted(&self, sql: &str, params: &[SqlValue]) -> SqlResult<Option<i64>> {
        let (rows, id) = insert_counted(self.0, sql, params_from_iter(params))?;
        Ok((rows > 0).then_some(id))
    }
}

/// Register `dagq_actor_role()`, `dagq_actor_id()` and `dagq_requested_by()`
/// on `conn`, writing as `actor`, and return the state they read.
pub(in crate::infrastructure) fn attach_event_actors(
    conn: &Connection,
    actor: ActorContext,
) -> anyhow::Result<EventActors> {
    let actors = EventActors::new(actor);
    // Not deterministic: the value changes with `set`.
    let flags = FunctionFlags::SQLITE_UTF8;
    let state = actors.clone();
    conn.create_scalar_function("dagq_actor_role", 0, flags, move |_| Ok(state.role()))?;
    let state = actors.clone();
    conn.create_scalar_function("dagq_actor_id", 0, flags, move |_| Ok(state.actor_id()))?;
    let state = actors.clone();
    conn.create_scalar_function("dagq_requested_by", 0, flags, move |_| {
        Ok(state.requested_by())
    })?;
    Ok(actors)
}

impl TransactionEntries for SqliteQueue {
    type Write<'store> = WriteTx<'store>;
    type Read<'store> = WriteTx<'store>;

    fn write_entry(&self, name: &'static str) -> anyhow::Result<WriteTx<'_>> {
        self.write_tx(name)
    }

    fn read_entry(&self, name: &'static str) -> anyhow::Result<WriteTx<'_>> {
        self.read_tx(name)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rusqlite::{OpenFlags, params};

    use super::*;
    use crate::infrastructure::sql_exec::{OptionalRow, sql_params};
    use crate::infrastructure::sqlite::WriteIdentity;

    /// A connection to the test's own memdb.
    fn memdb(name: &str) -> (String, Connection) {
        let uri = format!("file:/sql-exec-{name}-{}?vfs=memdb", uuid::Uuid::new_v4());
        (uri.clone(), connect(&uri))
    }

    fn connect(uri: &str) -> Connection {
        Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap()
    }

    fn table(name: &str) -> Connection {
        let (_, conn) = memdb(name);
        conn.execute_batch(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT UNIQUE, r REAL, b INTEGER)",
        )
        .unwrap();
        conn
    }

    fn read_tx(conn: &Connection) -> WriteTx<'_> {
        WriteTx::read(conn, "sql_exec_test", Duration::from_secs(60)).unwrap()
    }

    /// A bool is stored as the integers 0 and 1, and reads back as a bool.
    #[test]
    fn a_bool_is_stored_as_zero_or_one_and_reads_as_a_bool() {
        let conn = table("bool");
        let tx = port_tx(&conn);
        for value in [false, true] {
            SqlWrite::execute(&tx, "INSERT INTO t(b) VALUES (?1)", sql_params![value]).unwrap();
        }
        let rows = SqlRead::query_map(&tx, "SELECT b, typeof(b), b FROM t ORDER BY id", &[], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })
        .unwrap();
        assert_eq!(
            rows,
            [
                (0, "integer".to_owned(), false),
                (1, "integer".to_owned(), true)
            ]
        );
    }

    /// A real column keeps the f64 it was given and NULL is None: nothing
    /// is rounded to an integer or text on the way.
    #[test]
    fn a_real_column_reads_back_the_f64_it_was_given() {
        let conn = table("real");
        let tx = read_tx(&conn);
        let values = [Some(0.1), Some(2.5), Some(-3.75), None];
        for value in values {
            SqlWrite::execute(&tx, "INSERT INTO t(r) VALUES (?1)", sql_params![value]).unwrap();
        }
        let read = SqlRead::query_map(&tx, "SELECT r, typeof(r) FROM t ORDER BY id", &[], |r| {
            Ok((r.get::<_, Option<f64>>("r")?, r.get::<_, String>(1)?))
        })
        .unwrap();
        assert_eq!(
            read,
            [
                (Some(0.1), "real".to_owned()),
                (Some(2.5), "real".to_owned()),
                (Some(-3.75), "real".to_owned()),
                (None, "null".to_owned()),
            ]
        );
    }

    #[test]
    fn each_value_binds_as_its_type_in_sqlite() {
        let conn = table("kinds");
        let read = SqlRead::query_row(
            &conn,
            "SELECT typeof(?1), typeof(?2), typeof(?3), typeof(?4), typeof(?5), typeof(?6)",
            &[
                SqlValue::Integer(1),
                SqlValue::Real(0.5),
                SqlValue::Text("a".to_owned()),
                SqlValue::Bool(true),
                SqlValue::Null,
                SqlValue::Json("{}".to_owned()),
            ],
            |r| {
                (0..6)
                    .map(|index| r.get::<_, String>(index))
                    .collect::<SqlResult<Vec<_>>>()
            },
        )
        .unwrap();
        assert_eq!(read, ["integer", "real", "text", "integer", "null", "text"]);
    }

    /// The statement's own `?` and `?N` spellings bind as they are.
    #[test]
    fn a_statement_keeps_its_parameter_spelling() {
        let conn = table("spelling");
        let numbered: (i64, i64) =
            SqlRead::query_row(&conn, "SELECT ?2, ?1", sql_params![1_i64, 2_i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(numbered, (2, 1));
        let anonymous: (i64, i64) =
            SqlRead::query_row(&conn, "SELECT ?, ?", sql_params![1_i64, 2_i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(anonymous, (1, 2));
    }

    /// A blob is outside the port's values: reading it is the error rusqlite
    /// gives for a text column read from one, whatever the type asked for.
    #[test]
    fn a_stored_blob_does_not_read() {
        let conn = table("blob");
        let error = SqlRead::query_row(&conn, "SELECT x'00' AS b", &[], |r| {
            r.get::<_, Option<String>>("b")
        })
        .unwrap_err();
        let rusqlite = conn
            .query_row("SELECT x'00' AS b", [], |r| r.get::<_, Option<String>>("b"))
            .unwrap_err();
        assert_eq!(error.to_string(), rusqlite.to_string());
    }

    /// The trait's insert returns the ID rusqlite's own read gives, through
    /// the entry's transaction (counting it as a write) and the wrapper.
    #[test]
    fn the_port_s_insert_returns_the_id_of_the_row_and_counts_it() {
        let conn = table("insert");
        conn.execute("INSERT INTO t(id, v) VALUES (41, 'a')", [])
            .unwrap();
        let tx = read_tx(&conn);
        let id =
            SqlWrite::insert_id(&tx, "INSERT INTO t(v) VALUES (?1)", sql_params!["b"]).unwrap();
        assert_eq!(id, 42);
        assert_eq!(id, conn.last_insert_rowid());
        assert_eq!(tx.rows_written(), 1);
        let if_new = "INSERT INTO t(v) SELECT ?1 WHERE NOT EXISTS(SELECT 1 FROM t WHERE v=?1)";
        assert_eq!(
            SqlWrite::insert_id_if_inserted(&tx, if_new, sql_params!["c"]).unwrap(),
            Some(43)
        );
        assert_eq!(
            SqlWrite::insert_id_if_inserted(&tx, if_new, sql_params!["c"]).unwrap(),
            None
        );
        assert_eq!(tx.rows_written(), 2);
        assert_eq!(
            SqlWrite::execute(&tx, "UPDATE t SET v='z' WHERE id=?1", sql_params![42_i64]).unwrap(),
            1
        );
        assert_eq!(tx.rows_written(), 3);
        tx.commit().unwrap();

        let wrapped = port_tx(&conn);
        assert_eq!(
            SqlWrite::insert_id(&wrapped, "INSERT INTO t(v) VALUES (?1)", sql_params!["d"])
                .unwrap(),
            44
        );
        assert_eq!(
            SqlWrite::insert_id_if_inserted(&wrapped, if_new, sql_params!["e"]).unwrap(),
            Some(45)
        );
        assert_eq!(
            SqlWrite::insert_id_if_inserted(&wrapped, if_new, sql_params!["e"]).unwrap(),
            None
        );
    }

    /// The entries hand a transaction of the port: the write entry's has
    /// passed the fence and writes through it (counted, in the transaction),
    /// and the read entry's reads the same store. A stale identity never
    /// gets one. The trait's entries are these entries.
    #[test]
    fn the_entries_hand_a_transaction_of_the_port() {
        let uri = format!("file:/sql-exec-entries-{}?vfs=memdb", uuid::Uuid::new_v4());
        let queue = SqliteQueue::init(uri).unwrap();
        let sql = "UPDATE goals SET title='x' WHERE id=?1";
        let tx = queue.write_tx("sql_exec_write_entry").unwrap();
        assert_eq!(SqlRead::dialect(&tx), SqlDialect::Sqlite);
        assert_eq!(SqlWrite::execute(&tx, sql, sql_params![1_i64]).unwrap(), 0);
        let generation: i64 =
            SqlRead::query_row(&tx, "SELECT generation FROM write_leader", &[], |r| {
                r.get(0)
            })
            .unwrap();
        tx.commit().unwrap();

        let reader = queue.read_tx("sql_exec_read_entry").unwrap();
        let read: i64 =
            SqlRead::query_row(&reader, "SELECT generation FROM write_leader", &[], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(read, generation);
        assert_eq!(SqlRead::dialect(&reader), SqlDialect::Sqlite);
        drop(reader);
        let reader = queue.read_entry("sql_exec_read_entry").unwrap();
        assert_eq!(SqlRead::dialect(&reader), SqlDialect::Sqlite);
        drop(reader);
        queue
            .write_entry("sql_exec_write_entry")
            .unwrap()
            .commit()
            .unwrap();

        let stale = WriteIdentity {
            holder: Some("gone".to_owned()),
            generation: generation + 7,
        };
        let queue = queue.with_write_identity(stale);
        assert!(queue.write_tx("sql_exec_stale_entry").is_err());
    }

    /// What is not moved keeps its rusqlite calls: a `WriteTx` still derefs,
    /// and what it holds hands itself to moved code with [`port_tx`].
    #[test]
    fn an_unmoved_function_and_a_moved_one_share_the_transaction() {
        fn unmoved(conn: &Connection) -> rusqlite::Result<usize> {
            conn.execute("INSERT INTO t(v) VALUES (?1)", params!["unmoved"])
        }
        fn moved(tx: &impl SqlWrite) -> SqlResult<usize> {
            tx.execute("INSERT INTO t(v) VALUES (?1)", sql_params!["moved"])
        }
        let conn = table("compatibility");
        let tx = read_tx(&conn);
        assert_eq!(unmoved(&tx).unwrap(), 1);
        assert_eq!(moved(&tx).unwrap(), 1);
        assert!(
            moved(&port_tx(&tx))
                .unwrap_err()
                .to_string()
                .contains("UNIQUE")
        );
        let inner: &rusqlite::Transaction<'_> = &tx;
        assert_eq!(
            SqlRead::query_row(&port_tx(inner), "SELECT count(*) FROM t", &[], |r| r
                .get::<_, i64>(0))
            .unwrap(),
            2
        );
        drop(tx);
        // Rolled back with the transaction: the wrapper began nothing.
        let left: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }

    /// Each error of the port says what rusqlite's says, and comes back as
    /// the same error for a caller that has not moved.
    #[test]
    fn an_error_keeps_rusqlite_s_text_both_ways() {
        let unknown = usize::MAX;
        let errors = || {
            vec![
                rusqlite::Error::QueryReturnedNoRows,
                rusqlite::Error::InvalidColumnName("c".to_owned()),
                rusqlite::Error::InvalidColumnIndex(3),
                rusqlite::Error::InvalidColumnType(1, "c".to_owned(), Type::Blob),
                rusqlite::Error::InvalidColumnType(1, "c".to_owned(), Type::Null),
                rusqlite::Error::InvalidColumnType(1, "c".to_owned(), Type::Real),
                rusqlite::Error::InvalidColumnType(1, "c".to_owned(), Type::Integer),
                rusqlite::Error::InvalidColumnType(1, "c".to_owned(), Type::Text),
                rusqlite::Error::IntegralValueOutOfRange(1, 9),
                rusqlite::Error::IntegralValueOutOfRange(unknown, 9),
                rusqlite::Error::FromSqlConversionFailure(1, Type::Text, "bad".into()),
                rusqlite::Error::FromSqlConversionFailure(unknown, Type::Text, "bad".into()),
                rusqlite::Error::ExecuteReturnedResults,
            ]
        };
        for error in errors() {
            let text = error.to_string();
            let port = SqlError::from(error);
            assert_eq!(port.to_string(), text);
            assert_eq!(rusqlite::Error::from(port).to_string(), text);
        }
        let other = SqlError::Driver {
            contention: false,
            source: "another driver".into(),
        };
        assert_eq!(rusqlite::Error::from(other).to_string(), "another driver");
    }

    /// A moved function's error reads as the rusqlite error for an unmoved
    /// caller; any other error is left as it is.
    #[test]
    fn a_moved_function_s_error_comes_back_as_rusqlite_s() {
        let error = native_error(anyhow::Error::new(SqlError::NoRows));
        assert!(matches!(
            error.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::QueryReturnedNoRows)
        ));
        assert_eq!(native_error(anyhow::anyhow!("other")).to_string(), "other");
    }

    /// A statement refused because another connection holds the lock is
    /// told apart, on the port as on rusqlite, and tagged as busy.
    #[test]
    fn a_held_lock_is_contention_and_tags_as_busy() {
        let (uri, holder) = memdb("busy");
        holder.execute_batch("CREATE TABLE t(v)").unwrap();
        let other = connect(&uri);
        other.busy_timeout(Duration::ZERO).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let error =
            SqlWrite::execute(&port_tx(&other), "INSERT INTO t VALUES (1)", &[]).unwrap_err();
        assert!(error.is_contention(), "{error}");
        let tagged = crate::infrastructure::sqlite::tag_busy(anyhow::Error::new(error));
        assert!(tagged.is::<crate::application::QueueBusy>());
        let native = other.execute("INSERT INTO t VALUES (1)", []).unwrap_err();
        assert!(is_contention(&native));
        let text = native.to_string();
        let port = SqlError::from(native);
        assert!(port.is_contention());
        assert_eq!(port.to_string(), text);
        let back = rusqlite::Error::from(port);
        assert!(is_contention(&back));
        holder.execute_batch("ROLLBACK").unwrap();

        let refused = other
            .execute("INSERT INTO nothing VALUES (1)", [])
            .unwrap_err();
        assert!(!is_contention(&refused));
        assert!(!SqlError::from(refused).is_contention());
    }

    /// The port's reads of no row and of the first row, on a real statement.
    #[test]
    fn a_query_reads_in_order_and_stops_at_the_first_row() {
        let conn = table("query");
        conn.execute_batch("INSERT INTO t(v) VALUES ('a'), ('b')")
            .unwrap();
        let sql = "SELECT v FROM t ORDER BY id";
        let first: String = SqlRead::query_row(&conn, sql, &[], |r| r.get("v")).unwrap();
        assert_eq!(first, "a");
        assert_eq!(
            SqlRead::query_map(&conn, sql, &[], |r| r.get::<_, String>(0)).unwrap(),
            ["a", "b"]
        );
        assert_eq!(
            SqlRead::query_row(&conn, "SELECT v FROM t WHERE id=99", &[], |r| r
                .get::<_, String>(0))
            .optional()
            .unwrap(),
            None
        );
        assert!(SqlRead::query_row(&conn, "SELECT nothing", &[], |r| r.get::<_, i64>(0)).is_err());
    }
}
