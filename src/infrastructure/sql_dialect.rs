//! The dialects' fragments of the queue store's SQL (ADR-t3982-1).
//!
//! Where the dialects differ only in one expression (reading a field of a
//! JSON column, a date and time function, the key of an expression index), a
//! statement of the store takes
//! that expression from a function here instead of spelling it. A function
//! takes what the expression reads (the column and the JSON path, or the
//! time's value) and what it reads it as (the type, the form), and returns
//! the SQL text of the expression; a statement held as a constant becomes a
//! function that builds its text.
//!
//! The SQLite dialect returns SQLite's own function (`json_extract`,
//! `strftime`, `julianday`), character for character the expression the
//! statement spelled: a statement whose text separates the arguments with a
//! comma and a space calls the `_spaced` function, any other the plain one.
//!
//! The JSON type and the time form name what the caller reads; SQLite's
//! `json_extract` returns the stored JSON type whatever the caller asks, so
//! the SQLite text does not depend on [`JsonType`]. A dialect without that
//! looseness reads the field as the named type.
//!
//! The Postgres dialect casts what it reads (`::jsonb`, `::text::timestamptz`),
//! so its text holds whichever type the column has and a bound parameter is
//! bound as text. It ignores the `_spaced` difference, which only keeps
//! SQLite's text the same to the character. It asks for PostgreSQL 16 or
//! later (`pg_input_is_valid`).
//!
//! The rest of the JSON functions and the clock in seconds take the same
//! form. A table of an array's elements ([`SqlDialect::json_elements`]) comes
//! with the reads of its row ([`SqlDialect::json_element`],
//! [`SqlDialect::json_element_index`], [`SqlDialect::json_element_type`]),
//! because `jsonb_array_elements` has no `key` or `type` column and its
//! `value` is a `jsonb`, not SQLite's plain value.
//!
//! Outside test code, no SQL of `src/` but this module's spells the functions
//! the boundary test `src/context_boundaries/dialect.rs` lists, nor `ifnull`,
//! which `src/context_boundaries/shared_sql.rs` checks: they fail on one that
//! does.
//!
//! [`SqlDialect`] is the kind of dialect: a store module on the execution
//! port (`super::sql_exec`) takes the expression from the method of the kind
//! its transaction returns. A free function is the same expression in the
//! SQLite kind, for the modules that are not on the port.

/// The kind of dialect a statement runs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqlDialect {
    Sqlite,
    Postgres,
}

/// What a JSON field is read as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JsonType {
    /// A JSON string.
    Text,
    /// A JSON integer (an ID, a count, a mark).
    Integer,
    /// A JSON boolean as `0` or `1`, compared with `1` or used as a condition.
    /// Postgres takes no integer as a condition: there a statement compares it.
    Boolean,
    /// The field whatever its JSON type: whether it is there, or its value
    /// handed back as it is stored (in Postgres, as text). A JSON `null` is
    /// SQL `NULL`.
    Any,
}

/// The text between two arguments of a function in the expression.
const TIGHT: &str = ",";
const SPACED: &str = ", ";

impl SqlDialect {
    /// The insertion order of `task_runs` or `supervisors`, qualified by
    /// `alias` when nonempty. SQLite keeps its implicit rowid unchanged;
    /// Postgres uses the unique identity `seq`. The single writer assigns
    /// identities in insertion order; local import copies rowid into seq.
    /// A run's text ID does not carry insertion order.
    pub(crate) fn insertion_order(self, alias: &str) -> String {
        let column = match self {
            Self::Sqlite => "rowid",
            Self::Postgres => "seq",
        };
        if alias.is_empty() {
            column.to_owned()
        } else {
            format!("{alias}.{column}")
        }
    }

    /// The larger of two scalar values: SQLite MAX and Postgres GREATEST.
    /// Unlike Postgres's native function, SQLite propagates a NULL operand;
    /// the CASE preserves that result in shared statements.
    pub(crate) fn scalar_max(self, a: &str, b: &str) -> String {
        self.scalar_extreme(a, b, "MAX", "GREATEST")
    }

    /// The smaller of two scalar values, with the same NULL propagation as
    /// [`Self::scalar_max`]: SQLite MIN and Postgres LEAST.
    #[allow(dead_code)] // Paired dialect operation; no shared statement needs MIN yet.
    pub(crate) fn scalar_min(self, a: &str, b: &str) -> String {
        self.scalar_extreme(a, b, "MIN", "LEAST")
    }

    fn scalar_extreme(self, a: &str, b: &str, sqlite: &str, postgres: &str) -> String {
        match self {
            Self::Sqlite => format!("{sqlite}({a},{b})"),
            Self::Postgres => format!(
                "CASE WHEN ({a}) IS NULL OR ({b}) IS NULL THEN NULL ELSE {postgres}({a},{b}) END"
            ),
        }
    }

    /// Whether any non-NULL predicate in the group is true. Empty groups
    /// and all-NULL groups give NULL: SQLite MAX of 0/1, Postgres bool_or.
    pub(crate) fn any_true(self, predicate: &str) -> String {
        match self {
            Self::Sqlite => format!("max({predicate})"),
            Self::Postgres => format!("bool_or({predicate})"),
        }
    }

    /// The SQL expression that reads the JSON field at the literal `path`
    /// (`$.kind`) of `column` (`payload`, `o.payload`) as `ty`.
    pub(crate) fn json_field(self, column: &str, path: &str, ty: JsonType) -> String {
        self.json_expression(column, &path_literal(path), ty, TIGHT)
    }

    /// A top-level JSON key used by the ledger's earlier seeks and their
    /// expression indexes. Bind numeric IDs as integers in SQLite and text
    /// in Postgres; no cast may wrap this expression in the read.
    #[allow(dead_code)] // The earlier reads will use the indexes before replacing the whole read.
    pub(crate) fn ledger_index_key(self, column: &str, key: &str) -> String {
        debug_assert!(key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        match self {
            Self::Sqlite => self.json_field(column, &format!("$.{key}"), JsonType::Text),
            Self::Postgres => format!("{column}::jsonb ->> '{key}'"),
        }
    }

    /// [`SqlDialect::json_field`] with a space after the comma between the
    /// arguments.
    pub(crate) fn json_field_spaced(self, column: &str, path: &str, ty: JsonType) -> String {
        self.json_expression(column, &path_literal(path), ty, SPACED)
    }

    /// The SQL expression that reads the JSON field of `column` at the path
    /// the SQL expression `path` (a bound parameter such as `?4`) gives, as
    /// `ty`.
    pub(crate) fn json_field_at(self, column: &str, path: &str, ty: JsonType) -> String {
        self.json_expression(column, path, ty, TIGHT)
    }

    /// The SQL expression that reads the time the SQL expression `value`
    /// gives (a column, a bound parameter, `'now'`) in `form`.
    pub(crate) fn time_value(self, value: &str, form: TimeForm) -> String {
        self.time_expression(value, form, TIGHT)
    }

    /// [`SqlDialect::time_value`] with a space after the comma between the
    /// arguments.
    pub(crate) fn time_value_spaced(self, value: &str, form: TimeForm) -> String {
        self.time_expression(value, form, SPACED)
    }

    /// The SQL expression of the DB's clock now, in Unix milliseconds as an
    /// integer: read at the statement, not at the transaction's start, so a
    /// lease's end is judged by the DB's clock when the statement runs
    /// (ADR-t3986-1 decision 4). SQLite's `'now'` is the same everywhere in
    /// one statement. Postgres's is read when the expression is, after the
    /// statement's wait for a row, so a lease extended by a writer that queued
    /// on the leader's row runs from when it got the row.
    pub(crate) fn now_millis(self) -> String {
        self.time_value("'now'", TimeForm::EpochMillis)
    }

    /// The SQL table of the elements of the JSON array the SQL expression
    /// `source` gives (a column, a bound parameter), one row each; a statement
    /// reads a row with [`SqlDialect::json_element`] and gives the table its
    /// alias after this text. `NULL` has no elements in both dialects. An
    /// element is read from an array only: what is not an array is an error in
    /// Postgres (`jsonb_array_elements`) where `json_each` walks it.
    pub(crate) fn json_elements(self, source: &str) -> String {
        match self {
            Self::Sqlite => format!("json_each({source})"),
            Self::Postgres => format!("jsonb_array_elements(({source})::jsonb)"),
        }
    }

    /// [`SqlDialect::json_elements`] of the array at the literal `path` of
    /// `source`; a path that is not there has no elements.
    pub(crate) fn json_elements_at(self, source: &str, path: &str) -> String {
        self.json_elements_at_sep(source, path, TIGHT)
    }

    /// [`SqlDialect::json_elements_at`] with a space after the comma between
    /// the arguments.
    pub(crate) fn json_elements_at_spaced(self, source: &str, path: &str) -> String {
        self.json_elements_at_sep(source, path, SPACED)
    }

    fn json_elements_at_sep(self, source: &str, path: &str, sep: &str) -> String {
        let path = path_literal(path);
        match self {
            Self::Sqlite => format!("json_each({source}{sep}{path})"),
            Self::Postgres => format!("jsonb_array_elements({})", postgres_json_at(source, &path)),
        }
    }

    /// [`SqlDialect::json_elements`] with its alias `alias`, for a statement
    /// that reads an element's index ([`SqlDialect::json_element_index`]):
    /// Postgres numbers the rows only in the table's own alias.
    pub(crate) fn json_elements_numbered(self, source: &str, alias: &str) -> String {
        match self {
            Self::Sqlite => format!("json_each({source}) {alias}"),
            Self::Postgres => format!(
                "jsonb_array_elements(({source})::jsonb) WITH ORDINALITY AS {alias}(value, ordinal)"
            ),
        }
    }

    /// The SQL expression of the element in `column` (`value`, `j.value`) of a
    /// row of [`SqlDialect::json_elements`] as `ty`: SQLite's column as it is,
    /// which is a JSON string's text and a JSON integer's number; in Postgres
    /// the `jsonb` as that text, cast to `ty`. An object or an array reads as
    /// its document with [`JsonType::Text`].
    pub(crate) fn json_element(self, column: &str, ty: JsonType) -> String {
        match self {
            Self::Sqlite => column.to_owned(),
            Self::Postgres => postgres_cast(&format!("({column} #>> '{{}}')"), ty),
        }
    }

    /// The SQL expression of the index from 0 of an element of the table
    /// [`SqlDialect::json_elements_numbered`] made under `alias`.
    pub(crate) fn json_element_index(self, alias: &str) -> String {
        match self {
            Self::Sqlite => format!("{alias}.key"),
            Self::Postgres => format!("({alias}.ordinal - 1)"),
        }
    }

    /// The name of the JSON type of an element of the table under `alias`, in
    /// the names of [`SqlDialect::json_type_of`].
    pub(crate) fn json_element_type(self, alias: &str) -> String {
        match self {
            Self::Sqlite => format!("{alias}.type"),
            Self::Postgres => postgres_json_type(&format!("{alias}.value")),
        }
    }

    /// The SQL expression of the name of the JSON type of the field at the
    /// literal `path` of `column`, in SQLite's names whichever the dialect, so
    /// a statement compares it with one text: `text`, `integer`, `real`,
    /// `true`, `false`, `null`, `object`, `array`; SQL `NULL` when the field is
    /// not there. Spaced, as every statement that reads it spells it. Postgres
    /// has one `number`, so its `integer` is a number whose `jsonb` text has no
    /// fraction (a stored `1e3` is one, where SQLite says `real`).
    pub(crate) fn json_type_of(self, column: &str, path: &str) -> String {
        let path = path_literal(path);
        match self {
            Self::Sqlite => format!("json_type({column}{SPACED}{path})"),
            Self::Postgres => postgres_json_type(&postgres_json_at(column, &path)),
        }
    }

    /// The SQL aggregate of the JSON array of what the SQL expression `value`
    /// gives over a group's rows. It is SQL `NULL` over no rows in Postgres and
    /// `[]` in SQLite, so a statement that reads the empty one says
    /// `COALESCE(.., '[]')`.
    pub(crate) fn json_array_agg(self, value: &str) -> String {
        match self {
            Self::Sqlite => format!("json_group_array({value})"),
            Self::Postgres => format!("jsonb_agg({value})"),
        }
    }

    /// The SQL expression of the length of the JSON array the SQL expression
    /// `value` gives. An array only: what is not one is `0` in SQLite and an
    /// error in Postgres.
    pub(crate) fn json_array_length(self, value: &str) -> String {
        match self {
            Self::Sqlite => format!("json_array_length({value})"),
            Self::Postgres => format!("jsonb_array_length(({value})::jsonb)"),
        }
    }

    /// The SQL expression of the JSON document the SQL expression `value` (a
    /// bound parameter) holds as text, as a value [`SqlDialect::json_set_field`]
    /// sets rather than as a string.
    pub(crate) fn json_document(self, value: &str) -> String {
        match self {
            Self::Sqlite => format!("json({value})"),
            Self::Postgres => format!("({value})::jsonb"),
        }
    }

    /// The SQL expression of the document in `column` with the field the SQL
    /// expression `path` names (a bound `$.a`, of names without a dot or a
    /// quote) set to the JSON value the SQL expression `value` gives, added
    /// when it is not there. A document that is not an object stays as it is
    /// (a row whose body went to the port holds its key, a JSON string): SQLite
    /// does that itself, and `jsonb_set` would fail on it, so Postgres sets
    /// only under an object. Postgres adds only under what is there, so a path
    /// of more than one name asks for its parents to be there.
    pub(crate) fn json_set_field(self, column: &str, path: &str, value: &str) -> String {
        match self {
            Self::Sqlite => format!("json_set({column}{TIGHT}{path}{TIGHT}{value})"),
            Self::Postgres => format!(
                "CASE WHEN jsonb_typeof(({column})::jsonb) = 'object' \
                 THEN jsonb_set(({column})::jsonb, string_to_array(substr(({path})::text, 3), '.'), {value}) \
                 ELSE ({column})::jsonb END"
            ),
        }
    }

    /// The SQL expression of the DB's clock now, in whole Unix seconds as an
    /// integer. The clock is read as [`SqlDialect::now_millis`] reads it.
    pub(crate) fn unix_epoch_now(self) -> String {
        match self {
            Self::Sqlite => "unixepoch()".to_owned(),
            Self::Postgres => postgres_time("'now'", TimeForm::EpochSeconds),
        }
    }

    /// The condition that the time's text `column` is off the form records
    /// write: anything but four, two and two digits, a `T`, two digits each
    /// for the hour, minute and second, a `.` and three digits and a `Z`,
    /// case and all. Only a text in that form orders as its time, so a read
    /// by an index of `created_at` checks first that no event is off it, on
    /// the partial index `events_off_the_record_form` whose condition this
    /// is: `SELECT EXISTS (SELECT 1 FROM run_events WHERE` this `)` reads
    /// only the events off the form. Stricter than
    /// [`crate::domain::ledger::window::RECORD_TIME_FORM`], whose `LIKE`
    /// takes any character for a digit and, in SQLite, a lower-case `t` or
    /// `z`, both of which order apart from the time.
    // The range reads of the ledger's window that check it land after the
    // index; the plan test of the index reads it now.
    #[allow(dead_code)]
    pub(crate) fn off_the_record_form(self, column: &str) -> String {
        match self {
            Self::Sqlite => format!("{column} NOT GLOB '{RECORD_TIME_GLOB}'"),
            Self::Postgres => format!("{column} !~ '{RECORD_TIME_REGEX}'"),
        }
    }

    /// Pin a SQLite lookup to its required index when competing indexes
    /// would otherwise hide the selective key. Postgres has no INDEXED BY
    /// clause and chooses its plan from the unchanged predicates.
    pub(crate) fn indexed_by(self, index: &str) -> String {
        match self {
            Self::Sqlite => format!(" INDEXED BY {index}"),
            Self::Postgres => String::new(),
        }
    }

    /// The key of a unique expression index that takes an absent `column` for
    /// the SQL literal `absent` (`0`, `''`), spelled as the dialect's index is:
    /// SQLite's `asks_open` is on `ifnull` and Postgres's base on `coalesce`.
    /// SQLite matches an index's expression by its function's name, so a
    /// statement that is to seek such an index takes its key from here; one
    /// that only filters by it spells `COALESCE`.
    pub(crate) fn index_key(self, column: &str, absent: &str) -> String {
        match self {
            Self::Sqlite => format!("ifnull({column},{absent})"),
            Self::Postgres => format!("COALESCE({column},{absent})"),
        }
    }

    fn json_expression(self, column: &str, path: &str, ty: JsonType, sep: &str) -> String {
        match self {
            Self::Sqlite => {
                let _ = ty;
                format!("json_extract({column}{sep}{path})")
            }
            Self::Postgres => postgres_json(column, path, ty),
        }
    }

    /// `args` is the function's arguments after the format (the value, and a
    /// shift when there is one), already separated.
    fn time_expression(self, args: &str, form: TimeForm, sep: &str) -> String {
        match self {
            Self::Sqlite => match form {
                TimeForm::Text => format!("strftime('%Y-%m-%dT%H:%M:%fZ'{sep}{args})"),
                TimeForm::EpochSeconds => format!("CAST(strftime('%s'{sep}{args}) AS INTEGER)"),
                TimeForm::EpochMillis => {
                    format!("CAST((julianday({args}) - 2440587.5) * 86400000 AS INTEGER)")
                }
                TimeForm::Ordinal => format!("julianday({args})"),
            },
            Self::Postgres => postgres_time(args, form),
        }
    }
}

/// The field at the path `path` gives (a literal or a bound parameter, in
/// the `$.a.b` form both dialects read) as text, cast to `ty`. A field that
/// is not there and a JSON `null` both read as `NULL`, as in SQLite.
fn postgres_json(column: &str, path: &str, ty: JsonType) -> String {
    postgres_cast(
        &format!("({} #>> '{{}}')", postgres_json_at(column, path)),
        ty,
    )
}

/// The `jsonb` at the path `path` gives of `column`, SQL `NULL` when it is not
/// there.
fn postgres_json_at(column: &str, path: &str) -> String {
    format!("jsonb_path_query_first(({column})::jsonb, ({path})::text::jsonpath)")
}

/// `text`, a JSON value as text, as `ty`.
fn postgres_cast(text: &str, ty: JsonType) -> String {
    match ty {
        JsonType::Text | JsonType::Any => text.to_owned(),
        JsonType::Integer => format!("{text}::BIGINT"),
        JsonType::Boolean => format!("{text}::BOOLEAN::INTEGER"),
    }
}

/// The name in SQLite's names of the JSON type of the `jsonb` `value`.
fn postgres_json_type(value: &str) -> String {
    format!(
        "CASE jsonb_typeof({value}) WHEN 'string' THEN 'text' \
         WHEN 'number' THEN CASE WHEN ({value} #>> '{{}}') ~ '^-?[0-9]+$' THEN 'integer' ELSE 'real' END \
         WHEN 'boolean' THEN ({value} #>> '{{}}') \
         ELSE jsonb_typeof({value}) END"
    )
}

/// The time `value` gives in `form`. `'now'` is the clock as the statement
/// runs: the Postgres text `'now'` would be the transaction's start, and
/// `statement_timestamp()` the time before a wait for a row. Only the
/// [`TimeForm::Ordinal`] promises `NULL` for what is not a time, since a cast
/// of one fails in Postgres where SQLite returns `NULL`.
fn postgres_time(value: &str, form: TimeForm) -> String {
    let now = value == "'now'";
    let instant = if now {
        "clock_timestamp()".to_owned()
    } else {
        format!("({value})::text::timestamptz")
    };
    match form {
        TimeForm::Text => {
            format!("to_char({instant} AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')")
        }
        TimeForm::EpochSeconds => format!("FLOOR(EXTRACT(EPOCH FROM {instant}))::BIGINT"),
        TimeForm::EpochMillis => format!("FLOOR(EXTRACT(EPOCH FROM {instant}) * 1000)::BIGINT"),
        TimeForm::Ordinal if now => format!("EXTRACT(EPOCH FROM {instant})::DOUBLE PRECISION"),
        TimeForm::Ordinal => format!(
            "CASE WHEN pg_input_is_valid(({value})::text, 'timestamptz') \
             THEN EXTRACT(EPOCH FROM {instant})::DOUBLE PRECISION END"
        ),
    }
}

/// The record's form of a time as SQLite's `GLOB` matches it
/// ([`SqlDialect::off_the_record_form`]).
const RECORD_TIME_GLOB: &str =
    "[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9]Z";

/// The same form as a Postgres regular expression, anchored at both ends.
const RECORD_TIME_REGEX: &str =
    "^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}[.][0-9]{3}Z$";

/// [`SqlDialect::json_field`] in the SQLite kind.
// SQLite compatibility entry for store modules migrating independently.
#[allow(dead_code)]
pub(crate) fn json_field(column: &str, path: &str, ty: JsonType) -> String {
    SqlDialect::Sqlite.json_field(column, path, ty)
}

/// [`SqlDialect::json_field_spaced`] in the SQLite kind.
#[cfg(test)]
pub(crate) fn json_field_spaced(column: &str, path: &str, ty: JsonType) -> String {
    SqlDialect::Sqlite.json_field_spaced(column, path, ty)
}

/// [`SqlDialect::json_field_at`] in the SQLite kind.
// No caller here any more, kept for a store module on another branch that
// has not moved (ADR-t3982-1: the moves land in any order).
#[allow(dead_code)]
pub(crate) fn json_field_at(column: &str, path: &str, ty: JsonType) -> String {
    SqlDialect::Sqlite.json_field_at(column, path, ty)
}

fn path_literal(path: &str) -> String {
    debug_assert!(
        path.starts_with('$') && !path.contains('\''),
        "a JSON path literal: {path}"
    );
    format!("'{path}'")
}

/// The form a date and time value is read in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimeForm {
    /// The text of `created_at`: UTC, ISO 8601, milliseconds and `Z`.
    Text,
    /// Whole seconds since the Unix epoch, as an integer.
    EpochSeconds,
    /// Milliseconds since the Unix epoch, as an integer.
    EpochMillis,
    /// A number that orders and compares the instants; `NULL` when the value
    /// is not a time.
    Ordinal,
}

/// [`SqlDialect::json_type_of`] in the SQLite kind.
#[cfg(test)]
pub(crate) fn json_type_of(column: &str, path: &str) -> String {
    SqlDialect::Sqlite.json_type_of(column, path)
}

/// [`SqlDialect::time_value`] in the SQLite kind.
// SQLite compatibility entry for store modules migrating independently.
#[allow(dead_code)]
pub(crate) fn time_value(value: &str, form: TimeForm) -> String {
    SqlDialect::Sqlite.time_value(value, form)
}

/// [`SqlDialect::time_value_spaced`] in the SQLite kind.
#[cfg(test)]
pub(crate) fn time_value_spaced(value: &str, form: TimeForm) -> String {
    SqlDialect::Sqlite.time_value_spaced(value, form)
}

/// The SQL expression that reads, in `form`, the time the SQL expression
/// `value` gives moved by the SQL expression `shift` (a bound parameter
/// holding a SQLite modifier such as `-100 seconds`); only the tests move
/// the times they record.
#[cfg(test)]
pub(crate) fn time_value_shifted(value: &str, shift: &str, form: TimeForm) -> String {
    SqlDialect::Sqlite.time_expression(&format!("{value}{TIGHT}{shift}"), form, TIGHT)
}

/// The SQL expression of the text the format the SQL expression `format` (a
/// bound parameter, in SQLite's `strftime` format) makes of the time the SQL
/// expression `value` gives, its arguments spaced; only a test that stands
/// in for SQLite's own `strftime` reads it.
#[cfg(test)]
pub(crate) fn formatted_time(format: &str, value: &str) -> String {
    format!("strftime({format}{SPACED}{value})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn an_index_hint_is_written_only_for_sqlite() {
        assert_eq!(
            SqlDialect::Sqlite.indexed_by("events_by_opened_event"),
            " INDEXED BY events_by_opened_event"
        );
        assert_eq!(
            SqlDialect::Postgres.indexed_by("events_by_opened_event"),
            ""
        );
    }

    #[test]
    fn an_index_key_is_spelled_as_each_dialect_s_index_is() {
        assert_eq!(
            SqlDialect::Sqlite.index_key("task_id", "0"),
            "ifnull(task_id,0)"
        );
        assert_eq!(
            SqlDialect::Postgres.index_key("run_id", "''"),
            "COALESCE(run_id,'')"
        );
    }

    /// The kind's form returns, character for character, what the SQLite
    /// form returns, and the SQLite kind is what the statements spell.
    #[test]
    fn the_sqlite_kind_returns_the_text_of_the_sqlite_form() {
        let kind = SqlDialect::Sqlite;
        let ty = JsonType::Integer;
        assert_eq!(
            kind.json_field("o.payload", "$.kind", ty),
            "json_extract(o.payload,'$.kind')"
        );
        assert_eq!(
            kind.json_field_spaced("payload", "$.kind", ty),
            "json_extract(payload, '$.kind')"
        );
        assert_eq!(
            kind.json_field_at("payload", "?4", ty),
            "json_extract(payload,?4)"
        );
        assert_eq!(
            kind.json_field("o.payload", "$.kind", ty),
            json_field("o.payload", "$.kind", ty)
        );
        assert_eq!(
            kind.json_field_spaced("p", "$.k", ty),
            json_field_spaced("p", "$.k", ty)
        );
        assert_eq!(
            kind.json_field_at("p", "?1", ty),
            json_field_at("p", "?1", ty)
        );
        for (form, tight, spaced) in [
            (
                TimeForm::Text,
                "strftime('%Y-%m-%dT%H:%M:%fZ',at)",
                "strftime('%Y-%m-%dT%H:%M:%fZ', at)",
            ),
            (
                TimeForm::EpochSeconds,
                "CAST(strftime('%s',at) AS INTEGER)",
                "CAST(strftime('%s', at) AS INTEGER)",
            ),
            (
                TimeForm::EpochMillis,
                "CAST((julianday(at) - 2440587.5) * 86400000 AS INTEGER)",
                "CAST((julianday(at) - 2440587.5) * 86400000 AS INTEGER)",
            ),
            (TimeForm::Ordinal, "julianday(at)", "julianday(at)"),
        ] {
            assert_eq!(kind.time_value("at", form), tight);
            assert_eq!(kind.time_value_spaced("at", form), spaced);
            assert_eq!(kind.time_value("at", form), time_value("at", form));
            assert_eq!(
                kind.time_value_spaced("at", form),
                time_value_spaced("at", form)
            );
        }
    }

    #[test]
    fn the_postgres_kind_reads_a_json_field_as_the_named_type() {
        let kind = SqlDialect::Postgres;
        let text = "(jsonb_path_query_first((o.payload)::jsonb, \
                    ('$.a.b')::text::jsonpath) #>> '{}')";
        for (ty, expected) in [
            (JsonType::Text, text.to_owned()),
            (JsonType::Any, text.to_owned()),
            (JsonType::Integer, format!("{text}::BIGINT")),
            (JsonType::Boolean, format!("{text}::BOOLEAN::INTEGER")),
        ] {
            assert_eq!(kind.json_field("o.payload", "$.a.b", ty), expected);
            assert_eq!(kind.json_field_spaced("o.payload", "$.a.b", ty), expected);
        }
        assert_eq!(
            kind.json_field_at("c.payload", "?4", JsonType::Any),
            "(jsonb_path_query_first((c.payload)::jsonb, (?4)::text::jsonpath) #>> '{}')"
        );
    }

    #[test]
    fn the_postgres_kind_reads_a_time_in_the_named_form() {
        let kind = SqlDialect::Postgres;
        let at = "(created_at)::text::timestamptz";
        for (form, expected) in [
            (
                TimeForm::Text,
                format!("to_char({at} AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')"),
            ),
            (
                TimeForm::EpochSeconds,
                format!("FLOOR(EXTRACT(EPOCH FROM {at}))::BIGINT"),
            ),
            (
                TimeForm::EpochMillis,
                format!("FLOOR(EXTRACT(EPOCH FROM {at}) * 1000)::BIGINT"),
            ),
            (
                TimeForm::Ordinal,
                format!(
                    "CASE WHEN pg_input_is_valid((created_at)::text, 'timestamptz') \
                     THEN EXTRACT(EPOCH FROM {at})::DOUBLE PRECISION END"
                ),
            ),
        ] {
            assert_eq!(kind.time_value("created_at", form), expected);
            assert_eq!(kind.time_value_spaced("created_at", form), expected);
        }
    }

    /// Each of the JSON functions and the clock in seconds in both kinds. The
    /// SQLite text is what the statements spelled, character for character.
    #[test]
    fn each_kind_returns_its_own_json_function_and_clock() {
        type Fragment = fn(SqlDialect) -> String;
        let cases: [(Fragment, &str, &str); 16] = [
            (
                |d| d.json_elements("q.refs"),
                "json_each(q.refs)",
                "jsonb_array_elements((q.refs)::jsonb)",
            ),
            (
                |d| d.json_elements("?2"),
                "json_each(?2)",
                "jsonb_array_elements((?2)::jsonb)",
            ),
            (
                |d| d.json_elements_at("e.payload", "$.asks"),
                "json_each(e.payload,'$.asks')",
                "jsonb_array_elements(jsonb_path_query_first((e.payload)::jsonb, ('$.asks')::text::jsonpath))",
            ),
            (
                |d| d.json_elements_at_spaced("t.detail", "$.runs"),
                "json_each(t.detail, '$.runs')",
                "jsonb_array_elements(jsonb_path_query_first((t.detail)::jsonb, ('$.runs')::text::jsonpath))",
            ),
            (
                |d| d.json_elements_numbered("r.records", "j"),
                "json_each(r.records) j",
                "jsonb_array_elements((r.records)::jsonb) WITH ORDINALITY AS j(value, ordinal)",
            ),
            (
                |d| d.json_element("value", JsonType::Text),
                "value",
                "(value #>> '{}')",
            ),
            (
                |d| d.json_element("j.value", JsonType::Integer),
                "j.value",
                "(j.value #>> '{}')::BIGINT",
            ),
            (|d| d.json_element_index("j"), "j.key", "(j.ordinal - 1)"),
            (
                |d| d.json_element_type("j"),
                "j.type",
                "CASE jsonb_typeof(j.value) WHEN 'string' THEN 'text' \
                 WHEN 'number' THEN CASE WHEN (j.value #>> '{}') ~ '^-?[0-9]+$' THEN 'integer' ELSE 'real' END \
                 WHEN 'boolean' THEN (j.value #>> '{}') ELSE jsonb_typeof(j.value) END",
            ),
            (
                |d| d.json_type_of("payload", "$.a"),
                "json_type(payload, '$.a')",
                "CASE jsonb_typeof(jsonb_path_query_first((payload)::jsonb, ('$.a')::text::jsonpath)) WHEN 'string' THEN 'text' \
                 WHEN 'number' THEN CASE WHEN (jsonb_path_query_first((payload)::jsonb, ('$.a')::text::jsonpath) #>> '{}') ~ '^-?[0-9]+$' THEN 'integer' ELSE 'real' END \
                 WHEN 'boolean' THEN (jsonb_path_query_first((payload)::jsonb, ('$.a')::text::jsonpath) #>> '{}') \
                 ELSE jsonb_typeof(jsonb_path_query_first((payload)::jsonb, ('$.a')::text::jsonpath)) END",
            ),
            (
                |d| d.json_array_agg("x"),
                "json_group_array(x)",
                "jsonb_agg(x)",
            ),
            (|d| d.json_document("?2"), "json(?2)", "(?2)::jsonb"),
            (
                |d| d.json_set_field("payload", "?3", "v"),
                "json_set(payload,?3,v)",
                "CASE WHEN jsonb_typeof((payload)::jsonb) = 'object' \
                 THEN jsonb_set((payload)::jsonb, string_to_array(substr((?3)::text, 3), '.'), v) \
                 ELSE (payload)::jsonb END",
            ),
            (
                |d| d.json_set_field("payload", "?3", &d.json_document("?2")),
                "json_set(payload,?3,json(?2))",
                "CASE WHEN jsonb_typeof((payload)::jsonb) = 'object' \
                 THEN jsonb_set((payload)::jsonb, string_to_array(substr((?3)::text, 3), '.'), (?2)::jsonb) \
                 ELSE (payload)::jsonb END",
            ),
            (
                |d| d.json_array_length("x"),
                "json_array_length(x)",
                "jsonb_array_length((x)::jsonb)",
            ),
            (
                SqlDialect::unix_epoch_now,
                "unixepoch()",
                "FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::BIGINT",
            ),
        ];
        for (fragment, sqlite, postgres) in cases {
            assert_eq!(fragment(SqlDialect::Sqlite), sqlite);
            assert_eq!(fragment(SqlDialect::Postgres), postgres);
        }
        assert_eq!(json_type_of("p", "$.a"), "json_type(p, '$.a')");
    }

    /// What the SQLite text of a table and of the reads of its row gives,
    /// which is what the names and the doc comments promise of both kinds.
    #[test]
    fn the_sqlite_elements_read_what_their_functions_name() {
        let kind = SqlDialect::Sqlite;
        let conn = Connection::open_in_memory().unwrap();
        let elements: Vec<(i64, String, String)> = conn
            .prepare(&format!(
                "SELECT {}, {}, {} FROM {}",
                kind.json_element_index("j"),
                kind.json_element_type("j"),
                kind.json_element("j.value", JsonType::Text),
                kind.json_elements_numbered("?1", "j"),
            ))
            .unwrap()
            .query_map(["[\"a\", 2]"], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get::<_, rusqlite::types::Value>(2).map(|v| match v {
                        rusqlite::types::Value::Text(text) => text,
                        rusqlite::types::Value::Integer(number) => number.to_string(),
                        other => panic!("{other:?}"),
                    })?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            elements,
            [
                (0, "text".to_owned(), "a".to_owned()),
                (1, "integer".to_owned(), "2".to_owned())
            ]
        );
        let (set, none, ty, missing, aggregate, length): (
            String,
            i64,
            String,
            Option<String>,
            String,
            i64,
        ) = conn
            .query_row(
                &format!(
                    "SELECT {}, (SELECT count(*) FROM {}), {}, {}, (SELECT {} FROM {}), {}",
                    kind.json_set_field("'{}'", "?1", &kind.json_document("?2")),
                    kind.json_elements_at("'{}'", "$.none"),
                    kind.json_type_of("'{\"a\":\"x\"}'", "$.a"),
                    kind.json_type_of("'{}'", "$.a"),
                    kind.json_array_agg("value"),
                    kind.json_elements_at_spaced("'{\"r\":[1,2]}'", "$.r"),
                    kind.json_array_length("'[1,2,3]'"),
                ),
                ["$.a", "{\"b\":1}"],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(set, "{\"a\":{\"b\":1}}");
        assert_eq!(none, 0);
        assert_eq!(ty, "text");
        assert_eq!(missing, None);
        assert_eq!(aggregate, "[1,2]");
        assert_eq!(length, 3);
    }

    /// The DB's clock as the statement runs: `now()`, `'now'` and
    /// `statement_timestamp()` are fixed before a writer queues on the
    /// leader's row, so its lease could already have ended when it commits.
    #[test]
    fn the_postgres_clock_now_is_read_as_the_statement_runs_in_milliseconds() {
        let kind = SqlDialect::Postgres;
        assert_eq!(
            kind.now_millis(),
            "FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT"
        );
        assert_eq!(
            kind.time_value("'now'", TimeForm::Ordinal),
            "EXTRACT(EPOCH FROM clock_timestamp())::DOUBLE PRECISION"
        );
    }

    #[test]
    fn the_db_clock_now_is_sqlites_julian_day_in_milliseconds() {
        assert_eq!(
            SqlDialect::Sqlite.now_millis(),
            "CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)"
        );
    }

    /// Every expression the queue store's statements spelled before they
    /// took it from here, each next to the call that now returns it: the
    /// text is the same to the character, with and without a space after
    /// the comma.
    #[test]
    fn each_call_returns_the_expression_its_statement_spelled() {
        let cases = [
            (
                time_value("s.created_at", TimeForm::EpochMillis),
                r#"CAST((julianday(s.created_at) - 2440587.5) * 86400000 AS INTEGER)"#,
            ),
            (
                json_field("c.payload", "$.active_unavailable", JsonType::Text),
                r#"json_extract(c.payload,'$.active_unavailable')"#,
            ),
            (
                json_field("c.payload", "$.kind", JsonType::Text),
                r#"json_extract(c.payload,'$.kind')"#,
            ),
            (
                json_field("c.payload", "$.opened_event_id", JsonType::Integer),
                r#"json_extract(c.payload,'$.opened_event_id')"#,
            ),
            (
                json_field("c.payload", "$.reason", JsonType::Text),
                r#"json_extract(c.payload,'$.reason')"#,
            ),
            (
                json_field_at("c.payload", "?4", JsonType::Any),
                r#"json_extract(c.payload,?4)"#,
            ),
            (
                json_field("e.payload", "$.ask_id", JsonType::Integer),
                r#"json_extract(e.payload,'$.ask_id')"#,
            ),
            (
                json_field("e.payload", "$.planner_id", JsonType::Integer),
                r#"json_extract(e.payload,'$.planner_id')"#,
            ),
            (
                json_field("e.payload", "$.subject", JsonType::Text),
                r#"json_extract(e.payload,'$.subject')"#,
            ),
            (
                json_field("e.payload", "$.workspace_id", JsonType::Text),
                r#"json_extract(e.payload,'$.workspace_id')"#,
            ),
            (
                json_field_spaced("f.payload", "$.dir", JsonType::Text),
                r#"json_extract(f.payload, '$.dir')"#,
            ),
            (
                json_field("f.payload", "$.eval_id", JsonType::Integer),
                r#"json_extract(f.payload,'$.eval_id')"#,
            ),
            (
                json_field_spaced("inputs_through", "$.events", JsonType::Integer),
                r#"json_extract(inputs_through, '$.events')"#,
            ),
            (
                json_field_spaced("inputs_through", "$.supervisor", JsonType::Integer),
                r#"json_extract(inputs_through, '$.supervisor')"#,
            ),
            (
                json_field_spaced("j.value", "$.id", JsonType::Integer),
                r#"json_extract(j.value, '$.id')"#,
            ),
            (
                json_field_spaced("j.value", "$.kind", JsonType::Text),
                r#"json_extract(j.value, '$.kind')"#,
            ),
            (
                json_field("o.payload", "$.goal_id", JsonType::Integer),
                r#"json_extract(o.payload,'$.goal_id')"#,
            ),
            (
                json_field("o.payload", "$.goal_review_id", JsonType::Integer),
                r#"json_extract(o.payload,'$.goal_review_id')"#,
            ),
            (
                json_field("o.payload", "$.kind", JsonType::Text),
                r#"json_extract(o.payload,'$.kind')"#,
            ),
            (
                json_field("o.payload", "$.plan_review_id", JsonType::Integer),
                r#"json_extract(o.payload,'$.plan_review_id')"#,
            ),
            (
                json_field("o.payload", "$.planner_id", JsonType::Integer),
                r#"json_extract(o.payload,'$.planner_id')"#,
            ),
            (
                json_field("o.payload", "$.proposal_id", JsonType::Integer),
                r#"json_extract(o.payload,'$.proposal_id')"#,
            ),
            (
                json_field("o.payload", "$.route", JsonType::Text),
                r#"json_extract(o.payload,'$.route')"#,
            ),
            (
                json_field_spaced("payload", "$.mode", JsonType::Text),
                r#"json_extract(payload, '$.mode')"#,
            ),
            (
                json_field_spaced("payload", "$.period", JsonType::Text),
                r#"json_extract(payload, '$.period')"#,
            ),
            (
                json_field_spaced("payload", "$.pid", JsonType::Integer),
                r#"json_extract(payload, '$.pid')"#,
            ),
            (
                json_field_spaced("payload", "$.plugin_only", JsonType::Boolean),
                r#"json_extract(payload, '$.plugin_only')"#,
            ),
            (
                json_field_spaced("payload", "$.session_id", JsonType::Text),
                r#"json_extract(payload, '$.session_id')"#,
            ),
            (
                json_field_spaced("payload", "$.supervisor", JsonType::Text),
                r#"json_extract(payload, '$.supervisor')"#,
            ),
            (
                json_field("payload", "$.absent_since", JsonType::Integer),
                r#"json_extract(payload,'$.absent_since')"#,
            ),
            (
                json_field("payload", "$.asked_by", JsonType::Text),
                r#"json_extract(payload,'$.asked_by')"#,
            ),
            (
                json_field("payload", "$.at", JsonType::Integer),
                r#"json_extract(payload,'$.at')"#,
            ),
            (
                json_field("payload", "$.attempt", JsonType::Integer),
                r#"json_extract(payload,'$.attempt')"#,
            ),
            (
                json_field("payload", "$.by", JsonType::Text),
                r#"json_extract(payload,'$.by')"#,
            ),
            (
                json_field("payload", "$.dir", JsonType::Text),
                r#"json_extract(payload,'$.dir')"#,
            ),
            (
                json_field("payload", "$.eval_id", JsonType::Integer),
                r#"json_extract(payload,'$.eval_id')"#,
            ),
            (
                json_field("payload", "$.final", JsonType::Boolean),
                r#"json_extract(payload,'$.final')"#,
            ),
            (
                json_field("payload", "$.finding_id", JsonType::Integer),
                r#"json_extract(payload,'$.finding_id')"#,
            ),
            (
                json_field("payload", "$.hold_id", JsonType::Integer),
                r#"json_extract(payload,'$.hold_id')"#,
            ),
            (
                json_field("payload", "$.kept_id", JsonType::Integer),
                r#"json_extract(payload,'$.kept_id')"#,
            ),
            (
                json_field("payload", "$.kind", JsonType::Text),
                r#"json_extract(payload,'$.kind')"#,
            ),
            (
                json_field("payload", "$.label", JsonType::Text),
                r#"json_extract(payload,'$.label')"#,
            ),
            (
                json_field("payload", "$.mode", JsonType::Text),
                r#"json_extract(payload,'$.mode')"#,
            ),
            (
                json_field("payload", "$.opened_event_id", JsonType::Integer),
                r#"json_extract(payload,'$.opened_event_id')"#,
            ),
            (
                json_field("payload", "$.outcome", JsonType::Text),
                r#"json_extract(payload,'$.outcome')"#,
            ),
            (
                json_field("payload", "$.period", JsonType::Text),
                r#"json_extract(payload,'$.period')"#,
            ),
            (
                json_field("payload", "$.planner_id", JsonType::Integer),
                r#"json_extract(payload,'$.planner_id')"#,
            ),
            (
                json_field("payload", "$.proposal_id", JsonType::Integer),
                r#"json_extract(payload,'$.proposal_id')"#,
            ),
            (
                json_field("payload", "$.reason", JsonType::Text),
                r#"json_extract(payload,'$.reason')"#,
            ),
            (
                json_field("payload", "$.reviewed_proposal_id", JsonType::Integer),
                r#"json_extract(payload,'$.reviewed_proposal_id')"#,
            ),
            (
                json_field("payload", "$.session_id", JsonType::Text),
                r#"json_extract(payload,'$.session_id')"#,
            ),
            (
                json_field("payload", "$.set_id", JsonType::Integer),
                r#"json_extract(payload,'$.set_id')"#,
            ),
            (
                json_field("payload", "$.source_run_id", JsonType::Text),
                r#"json_extract(payload,'$.source_run_id')"#,
            ),
            (
                json_field("payload", "$.step", JsonType::Text),
                r#"json_extract(payload,'$.step')"#,
            ),
            (
                json_field("payload", "$.subject", JsonType::Text),
                r#"json_extract(payload,'$.subject')"#,
            ),
            (
                json_field("payload", "$.supervisor", JsonType::Text),
                r#"json_extract(payload,'$.supervisor')"#,
            ),
            (
                json_field("payload", "$.to", JsonType::Text),
                r#"json_extract(payload,'$.to')"#,
            ),
            (
                json_field("payload", "$.to", JsonType::Integer),
                r#"json_extract(payload,'$.to')"#,
            ),
            (
                json_field("payload", "$.workspace_id", JsonType::Text),
                r#"json_extract(payload,'$.workspace_id')"#,
            ),
            (
                json_field_spaced("s.payload", "$.dir", JsonType::Text),
                r#"json_extract(s.payload, '$.dir')"#,
            ),
            (
                json_field_spaced("s.payload", "$.pid", JsonType::Integer),
                r#"json_extract(s.payload, '$.pid')"#,
            ),
            (
                json_field_spaced("s.payload", "$.timeout_secs", JsonType::Integer),
                r#"json_extract(s.payload, '$.timeout_secs')"#,
            ),
            (
                json_field("s.payload", "$.eval_id", JsonType::Integer),
                r#"json_extract(s.payload,'$.eval_id')"#,
            ),
            (
                json_field("s.payload", "$.kept_id", JsonType::Integer),
                r#"json_extract(s.payload,'$.kept_id')"#,
            ),
            (
                json_field("t.payload", "$.final", JsonType::Boolean),
                r#"json_extract(t.payload,'$.final')"#,
            ),
            (
                json_field("t.payload", "$.opened_event_id", JsonType::Integer),
                r#"json_extract(t.payload,'$.opened_event_id')"#,
            ),
            (
                json_field("d.material", "$.proposal_id", JsonType::Integer),
                r#"json_extract(d.material,'$.proposal_id')"#,
            ),
            (
                json_field_spaced("e.payload", "$.ask_id", JsonType::Integer),
                r#"json_extract(e.payload, '$.ask_id')"#,
            ),
            (
                json_field_spaced("e.payload", "$.duplicate_of", JsonType::Integer),
                r#"json_extract(e.payload, '$.duplicate_of')"#,
            ),
            (
                json_field_spaced("e.payload", "$.reason", JsonType::Text),
                r#"json_extract(e.payload, '$.reason')"#,
            ),
            (
                json_field_spaced("e.payload", "$.runtime_delivers", JsonType::Boolean),
                r#"json_extract(e.payload, '$.runtime_delivers')"#,
            ),
            (
                json_field_spaced("e.payload", "$.to", JsonType::Text),
                r#"json_extract(e.payload, '$.to')"#,
            ),
            (
                json_field("e.payload", "$.by", JsonType::Text),
                r#"json_extract(e.payload,'$.by')"#,
            ),
            (
                json_field("e.payload", "$.duplicate_of", JsonType::Integer),
                r#"json_extract(e.payload,'$.duplicate_of')"#,
            ),
            (
                json_field("e.payload", "$.finding_id", JsonType::Integer),
                r#"json_extract(e.payload,'$.finding_id')"#,
            ),
            (
                json_field("e.payload", "$.marked_at", JsonType::Integer),
                r#"json_extract(e.payload,'$.marked_at')"#,
            ),
            (
                json_field("e.payload", "$.replan_operation_id", JsonType::Integer),
                r#"json_extract(e.payload,'$.replan_operation_id')"#,
            ),
            (
                json_field("e.payload", "$.request_id", JsonType::Integer),
                r#"json_extract(e.payload,'$.request_id')"#,
            ),
            (
                json_field("j.value", "$.id", JsonType::Integer),
                r#"json_extract(j.value,'$.id')"#,
            ),
            (
                json_field("j.value", "$.kind", JsonType::Text),
                r#"json_extract(j.value,'$.kind')"#,
            ),
            (
                json_field_spaced("material", "$.source_task_id", JsonType::Integer),
                r#"json_extract(material, '$.source_task_id')"#,
            ),
            (
                json_field("material", "$.key", JsonType::Text),
                r#"json_extract(material,'$.key')"#,
            ),
            (
                json_field_spaced("o.material", "$.source_goal_id", JsonType::Integer),
                r#"json_extract(o.material, '$.source_goal_id')"#,
            ),
            (
                json_field("o.material", "$.index", JsonType::Integer),
                r#"json_extract(o.material,'$.index')"#,
            ),
            (
                json_field("o.material", "$.key", JsonType::Text),
                r#"json_extract(o.material,'$.key')"#,
            ),
            (
                json_field("o.material", "$.source_goal_id", JsonType::Integer),
                r#"json_extract(o.material,'$.source_goal_id')"#,
            ),
            (
                json_field("o.material", "$.source_run_id", JsonType::Text),
                r#"json_extract(o.material,'$.source_run_id')"#,
            ),
            (
                json_field("o.material", "$.source_task_id", JsonType::Integer),
                r#"json_extract(o.material,'$.source_task_id')"#,
            ),
            (
                json_field_spaced("payload", "$.planner_id", JsonType::Integer),
                r#"json_extract(payload, '$.planner_id')"#,
            ),
            (
                json_field_spaced("payload", "$.task_id", JsonType::Integer),
                r#"json_extract(payload, '$.task_id')"#,
            ),
            (
                json_field("payload", "$.ask_id", JsonType::Integer),
                r#"json_extract(payload,'$.ask_id')"#,
            ),
            (
                json_field("payload", "$.claimed_at", JsonType::Integer),
                r#"json_extract(payload,'$.claimed_at')"#,
            ),
            (
                json_field("payload", "$.continue_run", JsonType::Boolean),
                r#"json_extract(payload,'$.continue_run')"#,
            ),
            (
                json_field("payload", "$.correction_ask_id", JsonType::Integer),
                r#"json_extract(payload,'$.correction_ask_id')"#,
            ),
            (
                json_field("payload", "$.from", JsonType::Integer),
                r#"json_extract(payload,'$.from')"#,
            ),
            (
                json_field("payload", "$.goal_id", JsonType::Integer),
                r#"json_extract(payload,'$.goal_id')"#,
            ),
            (
                json_field("payload", "$.id", JsonType::Integer),
                r#"json_extract(payload,'$.id')"#,
            ),
            (
                json_field("payload", "$.marked_at", JsonType::Integer),
                r#"json_extract(payload,'$.marked_at')"#,
            ),
            (
                json_field("payload", "$.operation_id", JsonType::Integer),
                r#"json_extract(payload,'$.operation_id')"#,
            ),
            (
                json_field("payload", "$.origin", JsonType::Text),
                r#"json_extract(payload,'$.origin')"#,
            ),
            (
                json_field("payload", "$.plan_review_id", JsonType::Integer),
                r#"json_extract(payload,'$.plan_review_id')"#,
            ),
            (
                json_field("payload", "$.predecessor_id", JsonType::Integer),
                r#"json_extract(payload,'$.predecessor_id')"#,
            ),
            (
                json_field("payload", "$.request_id", JsonType::Integer),
                r#"json_extract(payload,'$.request_id')"#,
            ),
            (
                json_field("payload", "$.run_id", JsonType::Integer),
                r#"json_extract(payload,'$.run_id')"#,
            ),
            (
                json_field("payload", "$.source", JsonType::Text),
                r#"json_extract(payload,'$.source')"#,
            ),
            (
                json_field("payload", "$.source_goal_id", JsonType::Integer),
                r#"json_extract(payload,'$.source_goal_id')"#,
            ),
            (
                json_field("payload", "$.text", JsonType::Text),
                r#"json_extract(payload,'$.text')"#,
            ),
            (
                json_field("payload", "$.workflow", JsonType::Text),
                r#"json_extract(payload,'$.workflow')"#,
            ),
            (
                json_field_at("payload", "?3", JsonType::Text),
                r#"json_extract(payload,?3)"#,
            ),
            (
                json_field("r.material", "$.proposal_id", JsonType::Integer),
                r#"json_extract(r.material,'$.proposal_id')"#,
            ),
            (time_value("?7", TimeForm::Ordinal), r#"julianday(?7)"#),
            (time_value("?8", TimeForm::Ordinal), r#"julianday(?8)"#),
            (
                time_value("created_at", TimeForm::Ordinal),
                r#"julianday(created_at)"#,
            ),
            (
                time_value("'now'", TimeForm::Text),
                r#"strftime('%Y-%m-%dT%H:%M:%fZ','now')"#,
            ),
            (
                time_value_shifted("'now'", "?1", TimeForm::Text),
                r#"strftime('%Y-%m-%dT%H:%M:%fZ','now',?1)"#,
            ),
            (
                time_value_spaced("?1", TimeForm::EpochSeconds),
                r#"CAST(strftime('%s', ?1) AS INTEGER)"#,
            ),
            (
                time_value("created_at", TimeForm::EpochSeconds),
                r#"CAST(strftime('%s',created_at) AS INTEGER)"#,
            ),
            (formatted_time("?1", "?2"), r#"strftime(?1, ?2)"#),
        ];
        for (returned, spelled) in cases {
            assert_eq!(returned, spelled);
        }
    }

    #[test]
    fn a_json_field_reads_with_json_extract_whatever_the_type() {
        for ty in [
            JsonType::Text,
            JsonType::Integer,
            JsonType::Boolean,
            JsonType::Any,
        ] {
            assert_eq!(
                json_field("o.payload", "$.kind", ty),
                "json_extract(o.payload,'$.kind')"
            );
            assert_eq!(
                json_field_spaced("payload", "$.kind", ty),
                "json_extract(payload, '$.kind')"
            );
        }
    }

    #[test]
    fn the_expressions_read_what_their_form_names() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t(created_at TEXT, payload TEXT);
             INSERT INTO t VALUES ('2026-11-02T00:00:01.250Z',
               '{\"kind\":\"k\",\"id\":7,\"final\":true}');",
        )
        .unwrap();
        let sql = format!(
            "SELECT {}, {}, {}, {}, {}, {} FROM t",
            json_field("payload", "$.kind", JsonType::Text),
            json_field("payload", "$.id", JsonType::Integer),
            json_field("payload", "$.final", JsonType::Boolean),
            time_value("created_at", TimeForm::EpochSeconds),
            time_value("created_at", TimeForm::EpochMillis),
            time_value("created_at", TimeForm::Ordinal),
        );
        let row: (String, i64, i64, i64, i64, f64) = conn
            .query_row(&sql, [], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .unwrap();
        assert_eq!(&row.0, "k");
        assert_eq!((row.1, row.2), (7, 1));
        assert_eq!((row.3, row.4), (1_793_577_601, 1_793_577_601_250));
        assert!(row.5 > 2_461_000.0);
        let text: String = conn
            .query_row(
                &format!(
                    "SELECT {}",
                    time_value_shifted("'2026-11-02T00:00:00.000Z'", "?1", TimeForm::Text)
                ),
                ["-1 seconds"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(text, "2026-11-01T23:59:59.000Z");
    }
}

#[cfg(test)]
mod value_tests {
    use super::SqlDialect::{Postgres, Sqlite};

    #[test]
    fn insertion_order_names_the_same_sqlite_row_and_the_postgres_identity() {
        for (alias, sqlite, postgres) in [
            ("", "rowid", "seq"),
            ("r", "r.rowid", "r.seq"),
            ("s", "s.rowid", "s.seq"),
        ] {
            assert_eq!(Sqlite.insertion_order(alias), sqlite);
            assert_eq!(Postgres.insertion_order(alias), postgres);
        }
    }

    #[test]
    fn extrema_and_boolean_aggregates_have_both_dialects_and_keep_nulls() {
        assert_eq!(Sqlite.scalar_max("a", "b"), "MAX(a,b)");
        assert_eq!(Sqlite.scalar_min("a", "b"), "MIN(a,b)");
        assert_eq!(
            Postgres.scalar_max("a", "b"),
            "CASE WHEN (a) IS NULL OR (b) IS NULL THEN NULL ELSE GREATEST(a,b) END"
        );
        assert_eq!(
            Postgres.scalar_min("a", "b"),
            "CASE WHEN (a) IS NULL OR (b) IS NULL THEN NULL ELSE LEAST(a,b) END"
        );
        assert_eq!(Sqlite.any_true("a=b"), "max(a=b)");
        assert_eq!(Postgres.any_true("a=b"), "bool_or(a=b)");
    }

    #[test]
    fn sqlite_extrema_and_boolean_aggregates_read_values_and_nulls() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for (a, b, max, min) in [
            ("2", "7", Some(7), Some(2)),
            ("NULL", "7", None, None),
            ("2", "NULL", None, None),
        ] {
            let actual: (Option<i64>, Option<i64>) = conn
                .query_row(
                    &format!(
                        "SELECT {}, {}",
                        Sqlite.scalar_max(a, b),
                        Sqlite.scalar_min(a, b)
                    ),
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(actual, (max, min));
        }
        for (values, expected) in [
            ("(0),(1),(NULL)", Some(true)),
            ("(0),(NULL)", Some(false)),
            ("(NULL)", None),
        ] {
            let actual: Option<bool> = conn
                .query_row(
                    &format!(
                        "WITH t(v) AS (VALUES {values}) SELECT {} FROM t",
                        Sqlite.any_true("v=1")
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(actual, expected);
        }
    }
}
