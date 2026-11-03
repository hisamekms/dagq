//! The values a statement of the execution port binds and a row hands back,
//! and their mapping to the store's Rust types. Binding and reading are
//! pure: the adapters only move a [`SqlValue`] to and from their driver.
//!
//! What is read mirrors rusqlite's `FromSql`, so a store module keeps its
//! types and its errors: an integer column does not read as text, a real
//! reads only as `f64` (an integer reads as `f64` too), `NULL` reads only
//! as `None`, and a real stays an `f64` both ways. A boolean is the
//! dialect's to store: SQLite stores `0` and `1` ([`SqlValue::Bool`] binds
//! so), and any non-zero integer reads as true.

use crate::domain::{
    AskId, CommitSha, EventId, FindingId, GoalId, HoldId, LeaseToken, PlannerId, ProposalId,
    ReplanId, RequestId, RunId, TaskId,
};

/// A value bound to a statement, or read from a column.
#[derive(Clone, Debug, PartialEq)]
pub enum SqlValue {
    Integer(i64),
    Real(f64),
    Text(String),
    Bool(bool),
    Null,
    /// The text of a JSON document: text in SQLite, and what a dialect with
    /// a JSON type binds as one.
    Json(String),
}

/// The stored type of a value, as an error reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Null,
    Integer,
    Real,
    Text,
    /// A stored blob: the port neither binds nor reads one, so reading the
    /// column is an error of this kind.
    Blob,
}

impl std::fmt::Display for ValueKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Null => "Null",
            Self::Integer => "Integer",
            Self::Real => "Real",
            Self::Text => "Text",
            Self::Blob => "Blob",
        })
    }
}

impl SqlValue {
    /// The kind an error reports for this value: a boolean as an integer
    /// and a JSON document as text, as SQLite stores them.
    pub fn kind(&self) -> ValueKind {
        match self {
            Self::Integer(_) | Self::Bool(_) => ValueKind::Integer,
            Self::Real(_) => ValueKind::Real,
            Self::Text(_) | Self::Json(_) => ValueKind::Text,
            Self::Null => ValueKind::Null,
        }
    }
}

/// Why a value does not read as the type asked for; the row adds the column.
#[derive(Debug)]
pub enum ValueError {
    /// The stored type is not one this type reads.
    WrongType,
    /// An integer outside this type's range.
    OutOfRange(i64),
    /// The value's own check refused it (a malformed run ID or commit).
    Other(Box<dyn std::error::Error + Send + Sync>),
}

/// A Rust value as a statement binds it.
pub trait ToSqlValue {
    fn to_sql_value(&self) -> SqlValue;
}

/// A Rust value as a column reads into it.
pub trait FromSqlValue: Sized {
    fn from_sql_value(value: SqlValue) -> Result<Self, ValueError>;
}

impl ToSqlValue for SqlValue {
    fn to_sql_value(&self) -> SqlValue {
        self.clone()
    }
}

impl<T: ToSqlValue + ?Sized> ToSqlValue for &T {
    fn to_sql_value(&self) -> SqlValue {
        (**self).to_sql_value()
    }
}

impl<T: ToSqlValue> ToSqlValue for Option<T> {
    fn to_sql_value(&self) -> SqlValue {
        self.as_ref().map_or(SqlValue::Null, T::to_sql_value)
    }
}

impl<T: FromSqlValue> FromSqlValue for Option<T> {
    fn from_sql_value(value: SqlValue) -> Result<Self, ValueError> {
        match value {
            SqlValue::Null => Ok(None),
            value => T::from_sql_value(value).map(Some),
        }
    }
}

impl ToSqlValue for i64 {
    fn to_sql_value(&self) -> SqlValue {
        SqlValue::Integer(*self)
    }
}

impl FromSqlValue for i64 {
    fn from_sql_value(value: SqlValue) -> Result<Self, ValueError> {
        match value {
            SqlValue::Integer(value) => Ok(value),
            _ => Err(ValueError::WrongType),
        }
    }
}

macro_rules! integers {
    ($($int:ty),*) => {$(
        impl FromSqlValue for $int {
            fn from_sql_value(value: SqlValue) -> Result<Self, ValueError> {
                let value = i64::from_sql_value(value)?;
                <$int>::try_from(value).map_err(|_| ValueError::OutOfRange(value))
            }
        }
    )*};
}
integers!(i32, u32, u64, usize);

impl ToSqlValue for i32 {
    fn to_sql_value(&self) -> SqlValue {
        SqlValue::Integer(i64::from(*self))
    }
}

impl ToSqlValue for u32 {
    fn to_sql_value(&self) -> SqlValue {
        SqlValue::Integer(i64::from(*self))
    }
}

impl ToSqlValue for f64 {
    fn to_sql_value(&self) -> SqlValue {
        SqlValue::Real(*self)
    }
}

impl FromSqlValue for f64 {
    fn from_sql_value(value: SqlValue) -> Result<Self, ValueError> {
        match value {
            SqlValue::Real(value) => Ok(value),
            SqlValue::Integer(value) => Ok(value as f64),
            _ => Err(ValueError::WrongType),
        }
    }
}

impl ToSqlValue for bool {
    fn to_sql_value(&self) -> SqlValue {
        SqlValue::Bool(*self)
    }
}

impl FromSqlValue for bool {
    fn from_sql_value(value: SqlValue) -> Result<Self, ValueError> {
        match value {
            SqlValue::Bool(value) => Ok(value),
            SqlValue::Integer(value) => Ok(value != 0),
            _ => Err(ValueError::WrongType),
        }
    }
}

impl ToSqlValue for str {
    fn to_sql_value(&self) -> SqlValue {
        SqlValue::Text(self.to_owned())
    }
}

impl ToSqlValue for String {
    fn to_sql_value(&self) -> SqlValue {
        SqlValue::Text(self.clone())
    }
}

impl FromSqlValue for String {
    fn from_sql_value(value: SqlValue) -> Result<Self, ValueError> {
        match value {
            SqlValue::Text(value) | SqlValue::Json(value) => Ok(value),
            _ => Err(ValueError::WrongType),
        }
    }
}

impl ToSqlValue for serde_json::Value {
    fn to_sql_value(&self) -> SqlValue {
        SqlValue::Json(self.to_string())
    }
}

macro_rules! integer_ids {
    ($($id:ident),*) => {$(
        impl ToSqlValue for $id {
            fn to_sql_value(&self) -> SqlValue {
                SqlValue::Integer(self.as_i64())
            }
        }

        impl FromSqlValue for $id {
            fn from_sql_value(value: SqlValue) -> Result<Self, ValueError> {
                i64::from_sql_value(value).map($id::new)
            }
        }
    )*};
}
integer_ids!(
    HoldId, ReplanId, TaskId, GoalId, ProposalId, FindingId, RequestId, PlannerId, AskId, EventId
);

impl ToSqlValue for LeaseToken {
    fn to_sql_value(&self) -> SqlValue {
        self.as_str().to_sql_value()
    }
}

impl FromSqlValue for LeaseToken {
    fn from_sql_value(value: SqlValue) -> Result<Self, ValueError> {
        String::from_sql_value(value).map(LeaseToken::new)
    }
}

impl ToSqlValue for RunId {
    fn to_sql_value(&self) -> SqlValue {
        self.as_str().to_sql_value()
    }
}

impl FromSqlValue for RunId {
    fn from_sql_value(value: SqlValue) -> Result<Self, ValueError> {
        let text = String::from_sql_value(value)?;
        RunId::new(&text).map_err(|error| ValueError::Other(Box::new(error)))
    }
}

impl ToSqlValue for CommitSha {
    fn to_sql_value(&self) -> SqlValue {
        self.as_str().to_sql_value()
    }
}

impl FromSqlValue for CommitSha {
    fn from_sql_value(value: SqlValue) -> Result<Self, ValueError> {
        let text = String::from_sql_value(value)?;
        CommitSha::try_from(text.as_str()).map_err(|error| ValueError::Other(Box::new(error)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read<T: FromSqlValue>(value: SqlValue) -> Result<T, ValueError> {
        T::from_sql_value(value)
    }

    #[test]
    fn each_rust_value_binds_as_its_value() {
        assert_eq!(7_i64.to_sql_value(), SqlValue::Integer(7));
        assert_eq!((-7_i32).to_sql_value(), SqlValue::Integer(-7));
        assert_eq!(7_u32.to_sql_value(), SqlValue::Integer(7));
        assert_eq!(0.1_f64.to_sql_value(), SqlValue::Real(0.1));
        assert_eq!(true.to_sql_value(), SqlValue::Bool(true));
        assert_eq!("a".to_sql_value(), SqlValue::Text("a".to_owned()));
        assert_eq!(
            "a".to_owned().to_sql_value(),
            SqlValue::Text("a".to_owned())
        );
        assert_eq!(None::<f64>.to_sql_value(), SqlValue::Null);
        assert_eq!(Some(2.5_f64).to_sql_value(), SqlValue::Real(2.5));
        assert_eq!(
            serde_json::json!({"a": 1}).to_sql_value(),
            SqlValue::Json("{\"a\":1}".to_owned())
        );
        assert_eq!(TaskId::new(4).to_sql_value(), SqlValue::Integer(4));
        assert_eq!(
            RunId::new("r").unwrap().to_sql_value(),
            SqlValue::Text("r".to_owned())
        );
        assert_eq!((&&SqlValue::Null).to_sql_value(), SqlValue::Null);
    }

    #[test]
    fn a_stored_zero_or_one_reads_as_a_bool() {
        assert!(!read::<bool>(SqlValue::Integer(0)).unwrap());
        assert!(read::<bool>(SqlValue::Integer(1)).unwrap());
        assert!(read::<bool>(SqlValue::Bool(true)).unwrap());
        assert!(matches!(
            read::<bool>(SqlValue::Text("1".to_owned())),
            Err(ValueError::WrongType)
        ));
    }

    /// A real is not rounded to an integer or text, and `NULL` is `None`.
    #[test]
    fn a_real_reads_as_the_same_f64() {
        for value in [0.1, 2.5, -3.75] {
            assert_eq!(read::<f64>(SqlValue::Real(value)).unwrap(), value);
            assert_eq!(
                read::<Option<f64>>(SqlValue::Real(value)).unwrap(),
                Some(value)
            );
            assert!(matches!(
                read::<i64>(SqlValue::Real(value)),
                Err(ValueError::WrongType)
            ));
            assert!(matches!(
                read::<String>(SqlValue::Real(value)),
                Err(ValueError::WrongType)
            ));
        }
        assert_eq!(read::<f64>(SqlValue::Integer(2)).unwrap(), 2.0);
        assert_eq!(read::<Option<f64>>(SqlValue::Null).unwrap(), None);
        assert!(matches!(
            read::<f64>(SqlValue::Null),
            Err(ValueError::WrongType)
        ));
    }

    #[test]
    fn an_integer_reads_within_its_type_s_range() {
        assert_eq!(read::<i32>(SqlValue::Integer(-3)).unwrap(), -3);
        assert_eq!(read::<usize>(SqlValue::Integer(3)).unwrap(), 3);
        assert_eq!(read::<u64>(SqlValue::Integer(3)).unwrap(), 3);
        assert!(matches!(
            read::<u32>(SqlValue::Integer(-1)),
            Err(ValueError::OutOfRange(-1))
        ));
        assert!(matches!(
            read::<i64>(SqlValue::Text("1".to_owned())),
            Err(ValueError::WrongType)
        ));
    }

    #[test]
    fn text_and_json_read_as_a_string_and_null_only_as_none() {
        assert_eq!(read::<String>(SqlValue::Text("a".into())).unwrap(), "a");
        assert_eq!(read::<String>(SqlValue::Json("{}".into())).unwrap(), "{}");
        assert_eq!(read::<Option<String>>(SqlValue::Null).unwrap(), None);
        assert!(matches!(
            read::<String>(SqlValue::Null),
            Err(ValueError::WrongType)
        ));
    }

    /// An ID reads through the check that creates it.
    #[test]
    fn ids_read_through_their_check() {
        assert_eq!(
            read::<TaskId>(SqlValue::Integer(4)).unwrap(),
            TaskId::new(4)
        );
        assert_eq!(
            read::<Option<GoalId>>(SqlValue::Integer(2)).unwrap(),
            Some(GoalId::new(2))
        );
        assert_eq!(
            read::<RunId>(SqlValue::Text("r".into())).unwrap().as_str(),
            "r"
        );
        assert!(matches!(
            read::<RunId>(SqlValue::Text(String::new())),
            Err(ValueError::Other(_))
        ));
        assert!(matches!(
            read::<CommitSha>(SqlValue::Text("abc".into())),
            Err(ValueError::Other(_))
        ));
        assert!(matches!(
            read::<RunId>(SqlValue::Integer(1)),
            Err(ValueError::WrongType)
        ));
        assert_eq!(
            read::<LeaseToken>(SqlValue::Text("t".into()))
                .unwrap()
                .as_str(),
            "t"
        );
        assert_eq!(
            LeaseToken::new("t".to_owned()).to_sql_value(),
            SqlValue::Text("t".to_owned())
        );
    }

    #[test]
    fn a_value_reports_the_kind_sqlite_stores_it_as() {
        assert_eq!(SqlValue::Bool(true).kind(), ValueKind::Integer);
        assert_eq!(SqlValue::Json("{}".into()).kind(), ValueKind::Text);
        assert_eq!(SqlValue::Real(0.5).kind(), ValueKind::Real);
        assert_eq!(SqlValue::Null.kind(), ValueKind::Null);
        assert_eq!(SqlValue::Integer(1).kind().to_string(), "Integer");
        assert_eq!(ValueKind::Blob.to_string(), "Blob");
    }
}
