//! What the Postgres side of the execution port ([`super::super::sql_exec`])
//! makes of a statement of the shared SQL: its parameters' spelling, its
//! values on the wire and its rows. All of it is pure and in every build, so
//! the unit tests see it without a DB; the driver ([`super::client`]) only
//! moves the bytes.
//!
//! The values are the binary format of Postgres's types. A parameter's type
//! is the one the server infers from the statement and a column's is the one
//! it describes, so the same [`SqlValue`] goes to what the column is. The base
//! schema's columns are `bigint`, `double precision`, `text`, `boolean` and
//! `jsonb`.
//!
//! - An integer binds to `bigint`, and to the narrower integers when it fits
//!   (a `CAST(x AS INTEGER)` is an `integer`). It binds to `double precision`
//!   too, as SQLite stores one in a real column.
//! - A real binds to `double precision` and reads back as the same `f64`.
//! - A boolean binds to `boolean`, and to `integer` as `0` or `1`, which is
//!   what the dialect reads a JSON boolean as, and to `bigint` the same way, as
//!   SQLite stores one. An integer does not bind to `boolean`.
//! - Text and a JSON document are the same text: a store module often binds a
//!   document as a plain string, so both bind to `text` and to `jsonb`, whose
//!   server checks the document.
//! - `NULL` binds to any type and reads as [`SqlValue::Null`].
//!
//! A pair not listed, such as text for a `bigint` or a column of `numeric`
//! (what `SUM(bigint)` returns), fails with both named: SQLite would convert
//! or store it, and a statement has to say so with a cast here.

use std::rc::Rc;

use crate::infrastructure::sql_exec::{RowValues, SqlError, SqlResult, SqlValue};

/// A value that does not convert; the driver reports it with the statement.
pub type WireResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// The version byte in front of a `jsonb`'s text in the binary format.
const JSONB_VERSION: u8 = 1;

/// `sql` with the parameters spelled as the shared SQL spells them (`?1`,
/// `?`) spelled as Postgres does (`$1`), and a `?` taking the number after
/// the highest so far as SQLite's does. What is quoted is left as it is.
pub(super) fn numbered_parameters(sql: &str) -> String {
    let mut numbered = String::with_capacity(sql.len());
    let mut highest = 0_u64;
    let mut quote = None;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some(open), _) if open == c => quote = None,
            (None, '?') => {
                let mut digits = String::new();
                while let Some(digit) = chars.next_if(char::is_ascii_digit) {
                    digits.push(digit);
                }
                let number = digits.parse().unwrap_or(highest + 1);
                highest = highest.max(number);
                numbered.push('$');
                numbered.push_str(&number.to_string());
                continue;
            }
            _ => {}
        }
        numbered.push(c);
    }
    numbered
}

/// The insert `sql` with `RETURNING id`, how Postgres answers the ID of the
/// row it inserted (ADR-t3982-1 decision 3).
pub(super) fn returning_id(sql: &str) -> String {
    let statement = sql.trim_end().trim_end_matches(';').trim_end();
    format!("{statement} RETURNING id")
}

/// `value` in the binary format of the parameter type `type_name` (Postgres's
/// name of it, such as `int8`), or `None` for `NULL`.
pub fn encode_parameter(value: &SqlValue, type_name: &str) -> WireResult<Option<Vec<u8>>> {
    let bytes = match (value, type_name) {
        (SqlValue::Null, _) => return Ok(None),
        (SqlValue::Integer(value), "int8") => value.to_be_bytes().to_vec(),
        (SqlValue::Integer(value), "int4") => i32::try_from(*value)?.to_be_bytes().to_vec(),
        (SqlValue::Integer(value), "int2") => i16::try_from(*value)?.to_be_bytes().to_vec(),
        (SqlValue::Integer(value), "float8") => (*value as f64).to_be_bytes().to_vec(),
        (SqlValue::Real(value), "float8") => value.to_be_bytes().to_vec(),
        (SqlValue::Bool(value), "bool") => vec![u8::from(*value)],
        (SqlValue::Bool(value), "int8") => i64::from(*value).to_be_bytes().to_vec(),
        (SqlValue::Bool(value), "int4") => i32::from(*value).to_be_bytes().to_vec(),
        (SqlValue::Text(text) | SqlValue::Json(text), "text" | "varchar" | "json") => {
            text.as_bytes().to_vec()
        }
        (SqlValue::Text(text) | SqlValue::Json(text), "jsonb") => {
            let mut bytes = Vec::with_capacity(text.len() + 1);
            bytes.push(JSONB_VERSION);
            bytes.extend_from_slice(text.as_bytes());
            bytes
        }
        (value, type_name) => {
            return Err(format!("cannot bind a {} to a {type_name}", value.kind()).into());
        }
    };
    Ok(Some(bytes))
}

/// The value of a column of the type `type_name` from its binary format
/// `raw`, or `NULL` when there is none.
pub fn decode_column(type_name: &str, raw: Option<&[u8]>) -> WireResult<SqlValue> {
    let Some(raw) = raw else {
        return Ok(SqlValue::Null);
    };
    Ok(match type_name {
        "int8" => SqlValue::Integer(i64::from_be_bytes(raw.try_into()?)),
        "int4" => SqlValue::Integer(i64::from(i32::from_be_bytes(raw.try_into()?))),
        "int2" => SqlValue::Integer(i64::from(i16::from_be_bytes(raw.try_into()?))),
        "float8" => SqlValue::Real(f64::from_be_bytes(raw.try_into()?)),
        "bool" => SqlValue::Bool(<[u8; 1]>::try_from(raw)? != [0]),
        "text" | "varchar" | "bpchar" | "name" => SqlValue::Text(std::str::from_utf8(raw)?.into()),
        "json" => SqlValue::Json(std::str::from_utf8(raw)?.to_owned()),
        "jsonb" => match raw.split_first() {
            Some((&JSONB_VERSION, text)) => SqlValue::Json(std::str::from_utf8(text)?.to_owned()),
            _ => return Err("a jsonb of an unknown version".into()),
        },
        other => return Err(format!("no value reads from a column of type {other}").into()),
    })
}

/// Whether the SQLSTATE `code` means another connection held the lock: a
/// `lock_timeout` ran out or the server broke a deadlock.
pub fn is_contention_state(code: &str) -> bool {
    matches!(code, "55P03" | "40P01")
}

/// A row a statement returned, in the port's values.
pub struct PostgresRow {
    columns: Rc<[String]>,
    values: Vec<SqlValue>,
}

impl PostgresRow {
    /// The row of `values`, one for each of the statement's `columns`.
    pub fn new(columns: Rc<[String]>, values: Vec<SqlValue>) -> Self {
        Self { columns, values }
    }
}

impl RowValues for PostgresRow {
    /// A name matches whatever its case, as rusqlite's does: Postgres folds
    /// an unquoted alias to lower case.
    fn column_index(&self, name: &str) -> SqlResult<usize> {
        self.columns
            .iter()
            .position(|column| column.eq_ignore_ascii_case(name))
            .ok_or_else(|| SqlError::NoColumn(name.to_owned()))
    }

    fn column_name(&self, index: usize) -> String {
        self.columns.get(index).cloned().unwrap_or_default()
    }

    fn value(&self, index: usize) -> SqlResult<SqlValue> {
        self.values
            .get(index)
            .cloned()
            .ok_or(SqlError::NoColumnIndex(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::sql_exec::SqlRow;

    fn encoded(value: SqlValue, type_name: &str) -> Vec<u8> {
        encode_parameter(&value, type_name).unwrap().unwrap()
    }

    /// `value` read back from what it binds as in the column type `type_name`.
    fn round_trip(value: &SqlValue, type_name: &str) -> SqlValue {
        let bytes = encode_parameter(value, type_name).unwrap();
        decode_column(type_name, bytes.as_deref()).unwrap()
    }

    #[test]
    fn parameters_are_numbered_as_sqlite_numbers_them_outside_quotes() {
        assert_eq!(
            numbered_parameters("a = ?2 AND b = ?2"),
            "a = $2 AND b = $2"
        );
        assert_eq!(numbered_parameters("(?, ?4, ?)"), "($1, $4, $5)");
        assert_eq!(
            numbered_parameters("x = '?1' AND \"y?\" = ?1"),
            "x = '?1' AND \"y?\" = $1"
        );
        assert_eq!(
            numbered_parameters("x = 'it''s?' AND y = ?"),
            "x = 'it''s?' AND y = $1"
        );
    }

    #[test]
    fn an_insert_returns_its_id_whatever_ends_the_statement() {
        let returning = "INSERT INTO asks (run) VALUES (?1) RETURNING id";
        assert_eq!(
            returning_id("INSERT INTO asks (run) VALUES (?1)"),
            returning
        );
        assert_eq!(
            returning_id("INSERT INTO asks (run) VALUES (?1) ;\n"),
            returning
        );
    }

    /// The bytes are the server's binary format, which no driver checks here.
    #[test]
    fn each_value_binds_in_the_binary_format_of_its_type() {
        assert_eq!(
            encoded(SqlValue::Integer(258), "int8"),
            [0, 0, 0, 0, 0, 0, 1, 2]
        );
        assert_eq!(
            encoded(SqlValue::Real(1.5), "float8"),
            [0x3F, 0xF8, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(encoded(SqlValue::Text("é".into()), "text"), "é".as_bytes());
        assert_eq!(encoded(SqlValue::Bool(true), "bool"), [1]);
        assert_eq!(encoded(SqlValue::Bool(false), "bool"), [0]);
        assert_eq!(
            encoded(SqlValue::Json("{}".into()), "jsonb"),
            [1, b'{', b'}']
        );
        for type_name in ["int8", "float8", "text", "bool", "jsonb"] {
            assert_eq!(encode_parameter(&SqlValue::Null, type_name).unwrap(), None);
        }
    }

    #[test]
    fn each_value_reads_back_the_same_from_the_type_it_binds_to() {
        for (value, type_name) in [
            (SqlValue::Integer(i64::MIN), "int8"),
            (SqlValue::Integer(-1), "int8"),
            (SqlValue::Real(0.1), "float8"),
            (SqlValue::Real(-3.75), "float8"),
            (SqlValue::Real(f64::MIN_POSITIVE), "float8"),
            (SqlValue::Text("a?b".into()), "text"),
            (SqlValue::Bool(true), "bool"),
            (SqlValue::Bool(false), "bool"),
            (SqlValue::Json(r#"{"a":1}"#.into()), "jsonb"),
            (SqlValue::Json(r#"{"a":1}"#.into()), "json"),
            (SqlValue::Null, "int8"),
            (SqlValue::Null, "float8"),
            (SqlValue::Null, "text"),
            (SqlValue::Null, "bool"),
            (SqlValue::Null, "jsonb"),
        ] {
            assert_eq!(round_trip(&value, type_name), value, "{type_name}");
        }
    }

    /// A store module's types read what the base schema's columns hand back,
    /// and a real and its `NULL` come back through `f64` and `None`.
    #[test]
    fn a_row_of_the_base_types_reads_as_the_store_s_types() {
        let names = ["n", "load", "none", "name", "done", "payload"];
        let columns: Rc<[String]> = names.map(str::to_owned).into();
        let row = PostgresRow::new(
            columns,
            vec![
                round_trip(&SqlValue::Integer(4), "int8"),
                round_trip(&SqlValue::Real(0.1), "float8"),
                round_trip(&SqlValue::Null, "float8"),
                round_trip(&SqlValue::Text("a".into()), "text"),
                round_trip(&SqlValue::Bool(true), "bool"),
                round_trip(&SqlValue::Text("{}".into()), "jsonb"),
            ],
        );
        let row = SqlRow::new(&row);
        assert_eq!(row.get::<_, i64>("n").unwrap(), 4);
        assert_eq!(row.get::<_, f64>("load").unwrap(), 0.1);
        assert_eq!(row.get::<_, Option<f64>>("load").unwrap(), Some(0.1));
        assert_eq!(row.get::<_, Option<f64>>("none").unwrap(), None);
        assert_eq!(row.get::<_, String>("name").unwrap(), "a");
        assert!(row.get::<_, bool>("done").unwrap());
        assert_eq!(row.get::<_, String>("payload").unwrap(), "{}");
        assert_eq!(row.get::<_, i64>("N").unwrap(), 4);
        assert!(matches!(
            row.get::<_, i64>("nothing"),
            Err(SqlError::NoColumn(_))
        ));
        assert!(matches!(
            row.get::<_, i64>(9),
            Err(SqlError::NoColumnIndex(9))
        ));
        assert_eq!(
            row.get::<_, String>("n").unwrap_err().to_string(),
            "Invalid column type Integer at index: 0, name: n"
        );
    }

    /// A value goes to what the column is, where SQLite stores either.
    #[test]
    fn a_value_binds_to_the_other_types_that_hold_it() {
        assert_eq!(
            encoded(SqlValue::Text("{}".into()), "jsonb"),
            [1, b'{', b'}']
        );
        assert_eq!(encoded(SqlValue::Json("{}".into()), "text"), b"{}");
        assert_eq!(encoded(SqlValue::Integer(2), "int4"), [0, 0, 0, 2]);
        assert_eq!(encoded(SqlValue::Integer(2), "int2"), [0, 2]);
        assert_eq!(
            encoded(SqlValue::Integer(2), "float8"),
            2.0_f64.to_be_bytes()
        );
        assert_eq!(encoded(SqlValue::Bool(true), "int8"), 1_i64.to_be_bytes());
        assert_eq!(encoded(SqlValue::Bool(true), "int4"), 1_i32.to_be_bytes());
        assert_eq!(encoded(SqlValue::Bool(false), "int4"), 0_i32.to_be_bytes());
    }

    #[test]
    fn a_value_the_type_does_not_hold_is_refused_with_both_named() {
        for (value, type_name, message) in [
            (
                SqlValue::Text("1".into()),
                "int8",
                "cannot bind a Text to a int8",
            ),
            (
                SqlValue::Integer(1),
                "text",
                "cannot bind a Integer to a text",
            ),
            (
                SqlValue::Integer(1),
                "bool",
                "cannot bind a Integer to a bool",
            ),
            (SqlValue::Real(0.5), "int8", "cannot bind a Real to a int8"),
        ] {
            let error = encode_parameter(&value, type_name).unwrap_err();
            assert_eq!(error.to_string(), message);
        }
        assert!(encode_parameter(&SqlValue::Integer(i64::MAX), "int4").is_err());
        assert!(encode_parameter(&SqlValue::Integer(i64::MAX), "int2").is_err());
    }

    #[test]
    fn the_narrower_numbers_read_as_the_port_s_values() {
        assert_eq!(
            decode_column("int4", Some(&(-2_i32).to_be_bytes())).unwrap(),
            SqlValue::Integer(-2)
        );
        assert_eq!(
            decode_column("int2", Some(&(-2_i16).to_be_bytes())).unwrap(),
            SqlValue::Integer(-2)
        );
    }

    #[test]
    fn a_column_that_does_not_read_is_an_error_not_a_guess() {
        let error = decode_column("numeric", Some(&[0])).unwrap_err();
        assert_eq!(
            error.to_string(),
            "no value reads from a column of type numeric"
        );
        assert!(decode_column("int8", Some(&[0])).is_err());
        assert!(decode_column("bool", Some(&[])).is_err());
        assert!(decode_column("text", Some(&[0xFF])).is_err());
        assert!(decode_column("jsonb", Some(&[2, b'{', b'}'])).is_err());
        assert!(decode_column("jsonb", Some(&[])).is_err());
    }

    #[test]
    fn only_a_lock_s_sqlstate_is_contention() {
        assert!(is_contention_state("55P03"));
        assert!(is_contention_state("40P01"));
        assert!(!is_contention_state("23505"));
    }
}
