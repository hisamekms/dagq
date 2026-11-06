//! The dialects' fragments of the queue store's SQL (ADR-t3982-1).
//!
//! Where the dialects differ only in one expression (reading a field of a
//! JSON column, a date and time function), a statement of the store takes
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
//! Outside test code, no SQL of `src/` but this module's spells
//! `json_extract`, `datetime`, `strftime` or `julianday`: the boundary test
//! `src/context_boundaries/dialect.rs` fails on one that does.
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
    /// The SQL expression that reads the JSON field at the literal `path`
    /// (`$.kind`) of `column` (`payload`, `o.payload`) as `ty`.
    pub(crate) fn json_field(self, column: &str, path: &str, ty: JsonType) -> String {
        self.json_expression(column, &path_literal(path), ty, TIGHT)
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
    let text =
        format!("(jsonb_path_query_first(({column})::jsonb, ({path})::text::jsonpath) #>> '{{}}')");
    match ty {
        JsonType::Text | JsonType::Any => text,
        JsonType::Integer => format!("{text}::BIGINT"),
        JsonType::Boolean => format!("{text}::BOOLEAN::INTEGER"),
    }
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

/// [`SqlDialect::json_field`] in the SQLite kind.
pub(crate) fn json_field(column: &str, path: &str, ty: JsonType) -> String {
    SqlDialect::Sqlite.json_field(column, path, ty)
}

/// [`SqlDialect::json_field_spaced`] in the SQLite kind.
pub(crate) fn json_field_spaced(column: &str, path: &str, ty: JsonType) -> String {
    SqlDialect::Sqlite.json_field_spaced(column, path, ty)
}

/// [`SqlDialect::json_field_at`] in the SQLite kind.
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

/// [`SqlDialect::time_value`] in the SQLite kind.
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
