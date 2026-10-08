//! The token cuts of the interactive sessions (ADR-t1486-1 decision 3):
//! the supervisor cuts the tokens of each inbox and person's planner span
//! ([`TOKEN_CUT_KINDS`]) every [`TOKEN_CUT_INTERVAL_MS`] while it is open
//! and once at its close, one `session_tokens` per cut, so that a span
//! open for days is counted on the day of each cut rather than all on the
//! day it closed. Where the last cut ended is in the queue (its `at` and
//! its `tokens_total`), so a supervisor that took over, or started again,
//! goes on from it. The transcript is read and analysed before the write
//! lock ([`prepare`]); under it a cut only checks the span's cuts again
//! and subtracts recorded numbers ([`write`]). A hook's close reads no
//! transcript: its cut at the close is made here afterwards.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use tracing::{debug, info};

use super::{
    sessions::{
        HOOK_INTAKE_PENDING, insert_at, now, open_hook_spans, read, span_closed, span_task,
    },
    sqlite::json_col,
};
use crate::domain::{
    EventId,
    event_kind::EventKind,
    sessions::{
        HEADLESS_ROUTE, INFERRED, OpenSpan, SESSION_CLOSED, SESSION_OPENED, SESSION_TOKENS,
        TOKEN_CUT_CLOSED_WINDOW_MS, TOKEN_CUT_KINDS, token_cut_due,
    },
    stats::rfc3339_millis,
    tokens::{ExecutionTokens, SpanTotals, TokenUsage},
    transcript::millis_text,
};

/// Make the cuts due now: of each span of [`TOKEN_CUT_KINDS`] still open
/// whose last cut (or open) is [`token_cut_due`], and of each one closed
/// in the last [`TOKEN_CUT_CLOSED_WINDOW_MS`] without its cut at the close.
/// A transcript of an open span that cannot be read or counted now is left
/// for the next time; one of a closed span is cut as not measured, once. One span
/// that fails holds back no other. Returns the cuts recorded.
pub(super) fn record_session_tokens(conn: &Connection) -> Result<usize> {
    let now = now(conn)?;
    record_at(conn, &now)
}

/// [`record_session_tokens`] at `now` (in the form of `created_at`).
fn record_at(conn: &Connection, now: &str) -> Result<usize> {
    let now = rfc3339_millis(now).context("the time now does not parse")?;
    let mut cuts = Vec::new();
    for span in open_hook_spans(conn)? {
        let Some(start) = span
            .opened_ms
            .filter(|_| TOKEN_CUT_KINDS.contains(&span.kind()))
        else {
            continue;
        };
        let since = last_cut_at(conn, span.opened_event_id)?.unwrap_or(start);
        if token_cut_due(since, now) {
            cuts.push((span, start, now, false));
        }
    }
    for (span, end) in closed_without_final_cut(conn, now - TOKEN_CUT_CLOSED_WINDOW_MS)? {
        let Some(start) = span.opened_ms else {
            continue;
        };
        // Never before a cut made while it was open (an inferred close
        // moved back to its transcript's last record): the totals do not
        // go back.
        let since = last_cut_at(conn, span.opened_event_id)?.unwrap_or(start);
        cuts.push((span, start, end.max(since), true));
    }
    let mut recorded = 0;
    for (span, start, at, last) in cuts {
        let Some(cut) = prepare(conn, span, start, at, last) else {
            continue;
        };
        let (opened, kind) = (cut.span.opened_event_id, cut.span.kind().to_owned());
        match write(conn, &cut) {
            Ok(true) => recorded += 1,
            Ok(false) => {}
            Err(error) => info!("session span {opened} ({kind}): tokens not cut now: {error:#}"),
        }
    }
    Ok(recorded)
}

/// A cut read and analysed before the write lock.
#[derive(Debug)]
struct Cut {
    span: OpenSpan,
    /// The time it cuts at (unix milliseconds): now for an open span, the
    /// close for a closed one.
    at: i64,
    /// Whether it is the span's cut at its close.
    last: bool,
    /// The span's totals from its open to `at`, or why they could not be
    /// counted.
    totals: Result<SpanTotals, &'static str>,
}

/// Read and analyse the transcript of `span` from `start` to `at`, never
/// under a write lock. `None` when an open span's transcript cannot be read
/// now.
fn prepare(conn: &Connection, span: OpenSpan, start: i64, at: i64, last: bool) -> Option<Cut> {
    let totals = match read(conn, &span) {
        Ok(transcript) => match SpanTotals::of(&transcript.records, start, at) {
            // Its totals run from its start, so this would recur hourly:
            // only its close records why.
            Err(reason) if !last => {
                debug!(
                    "session span {} ({}): tokens not cut now, {reason}",
                    span.opened_event_id,
                    span.kind()
                );
                return None;
            }
            totals => totals,
        },
        Err(unreadable) if !last => {
            debug!(
                "session span {} ({}): tokens not cut now, {}: {}",
                span.opened_event_id,
                span.kind(),
                unreadable.code,
                unreadable.detail
            );
            return None;
        }
        Err(unreadable) => Err(unreadable.code),
    };
    Some(Cut {
        span,
        at,
        last,
        totals,
    })
}

/// Record `cut` unless the queue moved past it while the transcript was
/// read: the span has its cut at the close already, an open span's cut
/// closed since, or another cut reached its time. Its own tokens are its
/// totals less those of the span's last cut that recorded some. An open
/// span's cut that adds nothing is not recorded (an idle inbox writes no
/// event each hour), so the next pass tries again. `false` when nothing was
/// recorded.
fn write(conn: &Connection, cut: &Cut) -> Result<bool> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let opened = cut.span.opened_event_id;
    let earlier = recorded_cuts(&tx, opened)?;
    let stale = if cut.last {
        earlier.iter().any(|payload| payload["final"] == true)
    } else {
        span_closed(&tx, opened)?
            || earlier
                .last()
                .and_then(cut_at)
                .is_some_and(|at| at >= cut.at)
    };
    if stale {
        return Ok(false);
    }
    let before = earlier.iter().rev().find_map(SpanTotals::from_payload);
    let mut payload = json!({
        "opened_event_id": opened,
        "kind": cut.span.kind(),
        "session_id": cut.span.session_id(),
        "at": millis_text(cut.at),
        "final": cut.last,
    });
    match &cut.totals {
        Ok(totals) => {
            let own = totals.since(before.as_ref());
            if !cut.last && own.tokens.as_ref().is_some_and(TokenUsage::is_zero) {
                return Ok(false);
            }
            own.record(&mut payload);
            totals.record(&mut payload);
        }
        Err(reason) => ExecutionTokens::unmeasured(reason).record(&mut payload),
    }
    let task = span_task(&tx, &cut.span)?;
    insert_at(
        &tx,
        task,
        None,
        EventKind::SessionTokens,
        &payload,
        &now(&tx)?,
    )?;
    tx.commit()?;
    Ok(true)
}

/// The time a cut's payload cuts at.
fn cut_at(payload: &Value) -> Option<i64> {
    payload["at"].as_str().and_then(rfc3339_millis)
}

/// The query of the cuts of the span opened by `?1`, by
/// `events_by_opened_event`.
fn recorded_cuts_sql() -> String {
    format!(
        "SELECT payload FROM run_events WHERE kind='{SESSION_TOKENS}'
           AND json_extract(payload,'$.opened_event_id')=?1 ORDER BY id"
    )
}

/// The payloads of the cuts of the span opened by `opened`, oldest first.
fn recorded_cuts(conn: &Connection, opened: EventId) -> Result<Vec<Value>> {
    Ok(conn
        .prepare(&recorded_cuts_sql())?
        .query_map([opened], |r| json_col(r, "payload"))?
        .collect::<rusqlite::Result<_>>()?)
}

/// The time the last cut of the span opened by `opened` cuts at.
fn last_cut_at(conn: &Connection, opened: EventId) -> Result<Option<i64>> {
    let payload: Option<Value> = conn
        .query_row(
            &format!("{} DESC LIMIT 1", recorded_cuts_sql()),
            [opened],
            |r| json_col(r, "payload"),
        )
        .optional()?;
    Ok(payload.as_ref().and_then(cut_at))
}

/// The query of the spans of [`TOKEN_CUT_KINDS`] closed since `?1` without
/// their cut at the close, with when and why each closed. Its `NOT EXISTS` finds
/// the cuts by `events_by_opened_event`.
fn closed_without_final_cut_sql() -> String {
    let kinds = TOKEN_CUT_KINDS.map(|kind| format!("'{kind}'")).join(",");
    format!(
        "SELECT o.id, o.payload, o.created_at, c.created_at AS closed_at,
                json_extract(c.payload,'$.reason') AS reason,
                json_extract(c.payload,'$.active_unavailable') AS unavailable
             FROM run_events c JOIN run_events o
               ON o.id=json_extract(c.payload,'$.opened_event_id')
             WHERE c.kind='{SESSION_CLOSED}' AND c.run_id IS NULL AND c.created_at>=?1
               AND o.kind='{SESSION_OPENED}'
               AND json_extract(o.payload,'$.kind') IN ({kinds})
               AND NOT EXISTS (SELECT 1 FROM run_events t WHERE t.kind='{SESSION_TOKENS}'
                 AND json_extract(t.payload,'$.opened_event_id')=+o.id
                 AND json_extract(t.payload,'$.final')=1)
             ORDER BY o.id"
    )
}

/// The spans of [`TOKEN_CUT_KINDS`] closed at or after `after` (unix
/// milliseconds) without their cut at the close, each with the end its
/// totals run to, the one its tokens at the close were counted to: its
/// close, or just past it for an `inferred` close the supervisor made from
/// the transcript, which ends at its last record and counts that record, as
/// its `session_closed` does. A close the hook made (its intake pending,
/// even an `inferred` one) is counted to its close, as its final
/// `session_turns` is.
fn closed_without_final_cut(conn: &Connection, after: i64) -> Result<Vec<(OpenSpan, i64)>> {
    type Row = (OpenSpan, String, Option<String>, Option<String>);
    let rows: Vec<Row> = conn
        .prepare(&closed_without_final_cut_sql())?
        .query_map(params![millis_text(after)], |r| {
            Ok((
                OpenSpan {
                    opened_event_id: r.get("id")?,
                    payload: json_col(r, "payload")?,
                    opened_ms: rfc3339_millis(&r.get::<_, String>("created_at")?),
                },
                r.get("closed_at")?,
                r.get("reason")?,
                r.get("unavailable")?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .filter(|(span, ..)| span.payload["route"] != HEADLESS_ROUTE)
        .filter_map(|(span, closed, reason, unavailable)| {
            let closed = rfc3339_millis(&closed)?;
            let through_last = reason.as_deref() == Some(INFERRED)
                && unavailable.as_deref() != Some(HOOK_INTAKE_PENDING);
            Some((span, if through_last { closed + 1 } else { closed }))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{
            RunEvent,
            sessions::{HookEvent, INBOX, PLANNER, SessionHook},
            tokens::span_usage,
            transcript::Transcript,
        },
        infrastructure::{
            sessions::{record_hook, tests::READS},
            sqlite::{SqliteQueue, event_row},
            transcripts::ClaudeTranscripts,
        },
    };

    /// The hook's report of `session` of `kind` starting (`start`) or
    /// ending, its transcript under `dir`.
    fn hook(dir: &std::path::Path, start: bool, kind: &'static str, session: &str) -> SessionHook {
        SessionHook {
            event: if start {
                HookEvent::Start {
                    source: "startup".into(),
                }
            } else {
                HookEvent::End {
                    reason: "prompt_input_exit".into(),
                }
            },
            kind,
            session_id: session.into(),
            transcript_path: Some(dir.join(format!("{session}.jsonl")).display().to_string()),
            cwd: Some("/repo".into()),
            workspace_id: Some(format!("W-{session}")),
            planner_id: None,
            launch: None,
        }
    }

    /// Append to the transcript of `session` an assistant record of message
    /// `id` at `at` with `input` and `output` tokens.
    fn assistant(dir: &std::path::Path, session: &str, id: &str, at: &str, counts: (i64, i64)) {
        use std::io::Write;
        let line = json!({"type": "assistant", "timestamp": at, "sessionId": session,
            "version": "2.1.283",
            "message": {"id": id, "model": "claude-opus-5-5", "content": [{"type": "text"}],
                        "usage": {"input_tokens": counts.0, "output_tokens": counts.1}}});
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{session}.jsonl")))
            .unwrap();
        writeln!(file, "{line}").unwrap();
    }

    fn set_time(conn: &Connection, id: i64, at: &str) {
        conn.execute(
            "UPDATE run_events SET created_at=?1 WHERE id=?2",
            params![at, id],
        )
        .unwrap();
    }

    fn cuts(conn: &Connection) -> Vec<RunEvent> {
        conn.prepare("SELECT * FROM run_events WHERE kind=?1 ORDER BY id")
            .unwrap()
            .query_map([SESSION_TOKENS], event_row)
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn io(payload: &Value) -> (i64, i64) {
        let tokens = &payload["tokens"];
        (
            tokens["input"].as_i64().unwrap(),
            tokens["output"].as_i64().unwrap(),
        )
    }

    /// An inbox open across midnight is cut hourly, each cut at its own
    /// time, from where the last one recorded: a message whose records a
    /// cut split adds only what it grew by, and a supervisor that took over
    /// (the queue opened again) goes on from the recorded cut and makes no
    /// second cut at the same time. Its close by the hook reads no
    /// transcript and makes no cut; the supervisor makes the cut at the
    /// close once, and the cuts together are the span's tokens.
    #[test]
    fn an_inbox_is_cut_hourly_across_midnight_and_once_at_its_hooks_close() {
        let dir = tempfile::tempdir().unwrap();
        ClaudeTranscripts::use_config_dir_in_test(&dir.path().join("config"));
        let db = dir.path().join("q.db");
        let queue = SqliteQueue::init(&db).unwrap();
        let conn = &queue.conn;
        let opened = record_hook(conn, &hook(dir.path(), true, INBOX, "s")).unwrap()["opened"]
            .as_i64()
            .unwrap();
        set_time(conn, opened, "2026-10-01T22:00:00.000Z");
        assistant(dir.path(), "s", "m0", "2026-10-01T21:50:00.000Z", (99, 99));
        assistant(dir.path(), "s", "m1", "2026-10-01T23:10:00.000Z", (10, 1));
        assistant(dir.path(), "s", "m2", "2026-10-01T23:20:00.000Z", (5, 2));
        assistant(dir.path(), "s", "m2", "2026-10-02T00:20:00.000Z", (5, 30));
        assistant(dir.path(), "s", "m3", "2026-10-02T00:40:00.000Z", (7, 7));
        // Not an hour since it opened.
        assert_eq!(record_at(conn, "2026-10-01T22:59:59.999Z").unwrap(), 0);
        assert_eq!(record_at(conn, "2026-10-01T23:30:00.000Z").unwrap(), 1);
        drop(queue);
        // The supervisor that takes over reads where the last cut ended
        // from the queue.
        let queue = SqliteQueue::init(&db).unwrap();
        let conn = &queue.conn;
        assert_eq!(record_at(conn, "2026-10-02T00:00:00.000Z").unwrap(), 0);
        assert_eq!(record_at(conn, "2026-10-02T00:45:00.000Z").unwrap(), 1);
        assert_eq!(record_at(conn, "2026-10-02T00:45:00.000Z").unwrap(), 0);
        // A cut prepared at a time a cut reached already writes nothing.
        let span = open_hook_spans(conn).unwrap().remove(0);
        let start = span.opened_ms.unwrap();
        let at = rfc3339_millis("2026-10-02T00:45:00.000Z").unwrap();
        let stale = prepare(conn, span, start, at, false).unwrap();
        assert!(!write(conn, &stale).unwrap());
        let recorded = cuts(conn);
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0].payload["at"], "2026-10-01T23:30:00.000Z");
        assert_eq!(recorded[1].payload["at"], "2026-10-02T00:45:00.000Z");
        assert_eq!(io(&recorded[0].payload), (15, 3));
        assert_eq!(io(&recorded[1].payload), (7, 35));
        assert_eq!(recorded[0].payload["kind"], INBOX);
        assert_eq!(recorded[0].payload["final"], false);
        assert_eq!(recorded[0].payload["tokens_source"], "transcript");
        assert_eq!(
            recorded[1].payload["tokens_by_model"][0]["model"],
            "claude-opus-5-5"
        );
        assert_eq!(recorded[1].payload["tokens_total"]["output"], 38);
        // The hook closes it without reading the transcript or cutting.
        assistant(dir.path(), "s", "m4", "2026-10-02T00:50:00.000Z", (1, 1));
        READS.set(0);
        let analysed = crate::domain::transcript::ANALYSED.get();
        record_hook(conn, &hook(dir.path(), false, INBOX, "s")).unwrap();
        assert_eq!(READS.get(), 0);
        assert_eq!(crate::domain::transcript::ANALYSED.get(), analysed);
        assert_eq!(cuts(conn).len(), 2);
        let closed: i64 = conn
            .query_row(
                "SELECT max(id) FROM run_events WHERE kind='session_closed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        set_time(conn, closed, "2026-10-02T01:00:00.000Z");
        assistant(dir.path(), "s", "m5", "2026-10-02T01:05:00.000Z", (50, 50));
        // The cut at the close is due at once, and only once.
        assert_eq!(record_at(conn, "2026-10-02T01:01:00.000Z").unwrap(), 1);
        assert_eq!(record_at(conn, "2026-10-02T03:00:00.000Z").unwrap(), 0);
        let recorded = cuts(conn);
        assert_eq!(recorded.len(), 3);
        assert_eq!(recorded[2].payload["final"], true);
        assert_eq!(recorded[2].payload["at"], "2026-10-02T01:00:00.000Z");
        assert_eq!(io(&recorded[2].payload), (1, 1));
        let text = std::fs::read_to_string(dir.path().join("s.jsonl")).unwrap();
        let records = Transcript::parse(&text, "s").unwrap().records;
        let whole = span_usage(
            &records,
            start,
            rfc3339_millis("2026-10-02T01:00:00.000Z").unwrap(),
        )
        .unwrap();
        let sum = recorded
            .iter()
            .map(|cut| io(&cut.payload))
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        assert_eq!(sum, (whole.input, whole.output));
        // The span's tokens at its close, which the supervisor's intake of
        // the hook's close writes in its final `session_turns`, are the
        // same as the cuts together.
        assert_eq!(final_turns_tokens(conn), vec![sum]);
    }

    /// The tokens of the final `session_turns` the supervisor's intake of
    /// the hook's closes writes, in order.
    fn final_turns_tokens(conn: &Connection) -> Vec<(i64, i64)> {
        crate::infrastructure::sessions::record_open_turns(conn).unwrap();
        conn.prepare(
            "SELECT * FROM run_events WHERE kind='session_turns'
               AND json_extract(payload,'$.final')=1 ORDER BY id",
        )
        .unwrap()
        .query_map([], event_row)
        .unwrap()
        .map(|event| io(&event.unwrap().payload))
        .collect()
    }

    /// A cut's transcript is read and analysed before its write
    /// transaction; under it the cut only reads the queue and writes.
    #[test]
    fn a_cut_analyses_its_transcript_before_its_write_transaction() {
        let dir = tempfile::tempdir().unwrap();
        ClaudeTranscripts::use_config_dir_in_test(&dir.path().join("config"));
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let conn = &queue.conn;
        record_hook(conn, &hook(dir.path(), true, PLANNER, "p")).unwrap();
        assistant(dir.path(), "p", "m1", "2099-01-01T00:00:00.000Z", (3, 4));
        let span = open_hook_spans(conn).unwrap().remove(0);
        let start = span.opened_ms.unwrap();
        let at = rfc3339_millis("2099-01-01T01:00:00.000Z").unwrap();
        READS.set(0);
        let analysed = crate::domain::transcript::ANALYSED.get();
        let cut = prepare(conn, span, start, at, false).unwrap();
        assert_eq!(READS.get(), 1);
        let before = crate::domain::transcript::ANALYSED.get();
        assert!(before > analysed);
        assert!(write(conn, &cut).unwrap());
        assert_eq!(READS.get(), 1);
        assert_eq!(crate::domain::transcript::ANALYSED.get(), before);
        assert_eq!(io(&cuts(conn)[0].payload), (3, 4));
        assert_eq!(cuts(conn)[0].payload["kind"], PLANNER);
        // An hour later with nothing new: no cut.
        let span = open_hook_spans(conn).unwrap().remove(0);
        let idle = prepare(conn, span, start, at + 3_600_000, false).unwrap();
        assert!(!write(conn, &idle).unwrap());
        assert_eq!(cuts(conn).len(), 1);
    }

    /// An open span whose transcript cannot be read is left for the next
    /// time; a closed one is cut once as not measured, with why. A span
    /// closed before the window gets no cut.
    #[test]
    fn an_unreadable_transcript_waits_while_open_and_is_unmeasured_at_the_close() {
        let dir = tempfile::tempdir().unwrap();
        ClaudeTranscripts::use_config_dir_in_test(&dir.path().join("config"));
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let conn = &queue.conn;
        let opened = record_hook(conn, &hook(dir.path(), true, INBOX, "gone")).unwrap()["opened"]
            .as_i64()
            .unwrap();
        set_time(conn, opened, "2026-10-01T00:00:00.000Z");
        assert_eq!(record_at(conn, "2026-10-01T02:00:00.000Z").unwrap(), 0);
        record_hook(conn, &hook(dir.path(), false, INBOX, "gone")).unwrap();
        let closed: i64 = conn
            .query_row("SELECT max(id) FROM run_events", [], |r| r.get(0))
            .unwrap();
        set_time(conn, closed, "2026-10-01T03:00:00.000Z");
        // A week and more after the close: no cut.
        assert_eq!(record_at(conn, "2026-10-08T03:00:00.001Z").unwrap(), 0);
        assert_eq!(record_at(conn, "2026-10-01T04:00:00.000Z").unwrap(), 1);
        assert_eq!(record_at(conn, "2026-10-01T05:00:00.000Z").unwrap(), 0);
        let recorded = cuts(conn);
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].payload["tokens"], Value::Null);
        assert_eq!(recorded[0].payload["tokens_reason"], "transcript_missing");
        assert_eq!(recorded[0].payload["final"], true);
    }

    /// The cut at an `inferred` close the supervisor made from the
    /// transcript, which ends at its last record, counts that record as its
    /// `session_closed` does, and is never before a cut made while the span
    /// was open. An `inferred` close the hook made (at the next start of
    /// its kind) is counted to its close, as its final `session_turns` is.
    #[test]
    fn an_inferred_close_is_cut_through_its_last_record_and_never_back() {
        let dir = tempfile::tempdir().unwrap();
        ClaudeTranscripts::use_config_dir_in_test(&dir.path().join("config"));
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let conn = &queue.conn;
        let mut closes = Vec::new();
        for session in ["a", "b", "c"] {
            let opened =
                record_hook(conn, &hook(dir.path(), true, INBOX, session)).unwrap()["opened"]
                    .as_i64()
                    .unwrap();
            set_time(conn, opened, "2026-10-01T00:00:00.000Z");
            assistant(
                dir.path(),
                session,
                "m1",
                "2026-10-01T00:10:00.000Z",
                (1, 1),
            );
            assistant(
                dir.path(),
                session,
                "m2",
                "2026-10-01T02:30:00.000Z",
                (2, 2),
            );
        }
        // "b" is cut at 03:00, after its last record.
        let b = open_hook_spans(conn).unwrap().remove(1);
        let start = b.opened_ms.unwrap();
        let at = rfc3339_millis("2026-10-01T03:00:00.000Z").unwrap();
        assert!(write(conn, &prepare(conn, b, start, at, false).unwrap()).unwrap());
        for session in ["a", "b", "c"] {
            record_hook(conn, &hook(dir.path(), false, INBOX, session)).unwrap();
            closes.push(
                conn.query_row("SELECT max(id) FROM run_events", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
            );
        }
        for (at, close) in closes.into_iter().enumerate() {
            conn.execute(
                "UPDATE run_events SET created_at='2026-10-01T02:30:00.000Z',
                   payload=json_set(payload,'$.reason','inferred') WHERE id=?1",
                [close],
            )
            .unwrap();
            // "a" and "b" as the supervisor closes them, from the
            // transcript; "c" as the hook does.
            if at < 2 {
                conn.execute(
                    "UPDATE run_events SET payload=json_remove(payload,'$.active_unavailable')
                     WHERE id=?1",
                    [close],
                )
                .unwrap();
            }
        }
        assert_eq!(record_at(conn, "2026-10-01T04:00:00.000Z").unwrap(), 3);
        let recorded = cuts(conn);
        let finals: Vec<&RunEvent> = recorded
            .iter()
            .filter(|cut| cut.payload["final"] == true)
            .collect();
        assert_eq!(finals[0].payload["session_id"], "a");
        assert_eq!(io(&finals[0].payload), (3, 3));
        assert_eq!(finals[0].payload["at"], "2026-10-01T02:30:00.001Z");
        assert_eq!(finals[1].payload["session_id"], "b");
        assert_eq!(io(&finals[1].payload), (0, 0));
        assert_eq!(finals[1].payload["at"], "2026-10-01T03:00:00.000Z");
        assert_eq!(finals[1].payload["tokens_total"]["input"], 3);
        assert_eq!(finals[2].payload["session_id"], "c");
        assert_eq!(io(&finals[2].payload), (1, 1));
        assert_eq!(finals[2].payload["at"], "2026-10-01T02:30:00.000Z");
        assert_eq!(final_turns_tokens(conn), vec![(1, 1)]);
    }

    /// The searches of a span's cuts use `events_by_opened_event`.
    #[test]
    fn the_searches_of_the_cuts_use_the_index_by_the_opened_event() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        for sql in [recorded_cuts_sql(), closed_without_final_cut_sql()] {
            let mut statement = queue
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let nulls = vec![rusqlite::types::Null; statement.parameter_count()];
            let plan: Vec<String> = statement
                .query_map(rusqlite::params_from_iter(nulls), |r| r.get("detail"))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert!(
                plan.iter()
                    .any(|step| step.contains("INDEX events_by_opened_event (kind=? AND <expr>=?)")),
                "{sql}\n{plan:#?}"
            );
        }
    }
}
