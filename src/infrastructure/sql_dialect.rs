//! The SQLite dialect's fragments of the queue store's SQL (ADR-t3982-1).
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

/// What a JSON field is read as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JsonType {
    /// A JSON string.
    Text,
    /// A JSON integer (an ID, a count, a mark).
    Integer,
    /// A JSON boolean, compared with `1` or used as a condition.
    Boolean,
    /// The field whatever its JSON type: whether it is there, or its value
    /// handed back as it is stored.
    Any,
}

/// The text between two arguments of a function in the expression.
const TIGHT: &str = ",";
const SPACED: &str = ", ";

/// The SQL expression that reads the JSON field at the literal `path`
/// (`$.kind`) of `column` (`payload`, `o.payload`) as `ty`.
pub(crate) fn json_field(column: &str, path: &str, ty: JsonType) -> String {
    json_expression(column, &path_literal(path), ty, TIGHT)
}

/// [`json_field`] with a space after the comma between the arguments.
pub(crate) fn json_field_spaced(column: &str, path: &str, ty: JsonType) -> String {
    json_expression(column, &path_literal(path), ty, SPACED)
}

/// The SQL expression that reads the JSON field of `column` at the path the
/// SQL expression `path` (a bound parameter such as `?4`) gives, as `ty`.
pub(crate) fn json_field_at(column: &str, path: &str, ty: JsonType) -> String {
    json_expression(column, path, ty, TIGHT)
}

fn path_literal(path: &str) -> String {
    debug_assert!(
        path.starts_with('$') && !path.contains('\''),
        "a JSON path literal: {path}"
    );
    format!("'{path}'")
}

fn json_expression(column: &str, path: &str, ty: JsonType, sep: &str) -> String {
    let _ = ty;
    format!("json_extract({column}{sep}{path})")
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

/// The SQL expression that reads the time the SQL expression `value` gives
/// (a column, a bound parameter, `'now'`) in `form`.
pub(crate) fn time_value(value: &str, form: TimeForm) -> String {
    time_expression(value, form, TIGHT)
}

/// [`time_value`] with a space after the comma between the arguments.
pub(crate) fn time_value_spaced(value: &str, form: TimeForm) -> String {
    time_expression(value, form, SPACED)
}

/// The SQL expression that reads, in `form`, the time the SQL expression
/// `value` gives moved by the SQL expression `shift` (a bound parameter
/// holding a SQLite modifier such as `-100 seconds`); only the tests move
/// the times they record.
#[cfg(test)]
pub(crate) fn time_value_shifted(value: &str, shift: &str, form: TimeForm) -> String {
    time_expression(&format!("{value}{TIGHT}{shift}"), form, TIGHT)
}

/// The SQL expression of the text the format the SQL expression `format` (a
/// bound parameter, in SQLite's `strftime` format) makes of the time the SQL
/// expression `value` gives, its arguments spaced; only a test that stands
/// in for SQLite's own `strftime` reads it.
#[cfg(test)]
pub(crate) fn formatted_time(format: &str, value: &str) -> String {
    format!("strftime({format}{SPACED}{value})")
}

/// `args` is the function's arguments after the format (the value, and a
/// shift when there is one), already separated.
fn time_expression(args: &str, form: TimeForm, sep: &str) -> String {
    match form {
        TimeForm::Text => format!("strftime('%Y-%m-%dT%H:%M:%fZ'{sep}{args})"),
        TimeForm::EpochSeconds => format!("CAST(strftime('%s'{sep}{args}) AS INTEGER)"),
        TimeForm::EpochMillis => {
            format!("CAST((julianday({args}) - 2440587.5) * 86400000 AS INTEGER)")
        }
        TimeForm::Ordinal => format!("julianday({args})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

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
                json_field_spaced("inputs_through", "$.supervisor", JsonType::Text),
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
