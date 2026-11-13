//! The store's views of the queue rows ([`views`]): written with each queue
//! row of [`views::VIEW_KINDS`] in its transaction by every write of a
//! ledger row ([`upsert`]), the rows written before them taken in a part at
//! a time by the filling, and their mark kept through the write leader's
//! generations as `ledger_open`'s is, carried on by every write of the
//! ledger whatever it writes (`View::Queue`: [`keep_views`], [`held_view`]).

use super::*;
use crate::domain::ledger::queue_views::{self as views, Ask, MainSummary};
use serde_json::Value;

/// The read of the queue rows the filling takes past its mark, in SQLite
/// naming the partial index it searches: with no statistics the planner
/// takes `ledger_rows_by_subject_end` instead and walks every row of the
/// kinds.
pub(super) fn view_rows_from(dialect: SqlDialect) -> &'static str {
    match dialect {
        SqlDialect::Sqlite => "ledger_rows INDEXED BY ledger_rows_queue_views",
        SqlDialect::Postgres => "ledger_rows",
    }
}

/// The condition of the queue rows the views are made of, as the partial
/// index `ledger_rows_queue_views` holds it.
pub(super) fn view_rows_sql() -> String {
    let kinds: Vec<String> = views::VIEW_KINDS
        .iter()
        .map(|kind| format!("'{}'", kind.as_str()))
        .collect();
    format!("kind = 'queue' AND subject_kind IN ({})", kinds.join(", "))
}

/// Write the views of `row` with it on `conn`, when it is a queue row they
/// are made of: `existed` when the store held the row before this write,
/// `filled` the mark as the write carried it on ([`keep_views`], whatever
/// the row).
pub(super) fn write_views(
    conn: &impl SqlWrite,
    row: &LedgerRow,
    existed: bool,
    filled: &Filled,
) -> Result<()> {
    if !views::takes(row) {
        return Ok(());
    }
    write_row_views(conn, &row.key, &row.records, Some((filled, existed)))
}

/// The views of the queue row `key` of `records`, in place of what they
/// held of it. `written` is a write's mark and whether the row was held
/// before; `None`, the filling's own part.
fn write_row_views(
    conn: &impl SqlWrite,
    key: &str,
    records: &[RunEvent],
    written: Option<(&Filled, bool)>,
) -> Result<()> {
    let queue = LedgerKind::Queue.as_str();
    SqlWrite::execute(
        conn,
        "DELETE FROM ledger_open WHERE kind=?1 AND key=?2",
        sql_params![queue, key],
    )?;
    SqlWrite::execute(
        conn,
        "DELETE FROM ledger_answered_asks WHERE key=?1",
        sql_params![key],
    )?;
    for event in records {
        let id = event.id.as_i64();
        if event.kind == EventKind::ForecastRecorded {
            SqlWrite::execute(
                conn,
                "DELETE FROM ledger_snapshot_targets WHERE snapshot_event_id=?1",
                sql_params![id],
            )?;
            for task in views::snapshot_targets(event) {
                SqlWrite::execute(
                    conn,
                    "INSERT INTO ledger_snapshot_targets(snapshot_event_id, task_id) VALUES (?1, ?2)",
                    sql_params![id, task],
                )?;
            }
        }
        if event.kind == EventKind::MainCommitsRecorded {
            SqlWrite::execute(
                conn,
                "DELETE FROM ledger_main_commits WHERE event_id=?1",
                sql_params![id],
            )?;
            for commit in views::main_commits(event) {
                SqlWrite::execute(
                    conn,
                    "INSERT INTO ledger_main_commits(event_id, sha, reverts) VALUES (?1, ?2, ?3)",
                    sql_params![id, commit.sha, commit.reverts],
                )?;
            }
        }
        match views::ask(event) {
            Some(Ask::Opened(ask)) if !answered(conn, &Left::Ask(ask.clone()))? => {
                SqlWrite::execute(
                    conn,
                    "INSERT INTO ledger_open(kind, key, what) VALUES (?1, ?2, ?3)
                     ON CONFLICT(kind, key, what) DO NOTHING",
                    sql_params![queue, key, Left::Ask(ask).text()],
                )?;
            }
            Some(Ask::Answered(ask)) => {
                let what = Left::Ask(ask).text();
                SqlWrite::execute(
                    conn,
                    "INSERT INTO ledger_answered_asks(key, what) VALUES (?1, ?2)
                     ON CONFLICT(key, what) DO NOTHING",
                    sql_params![key, what],
                )?;
                SqlWrite::execute(
                    conn,
                    "DELETE FROM ledger_open WHERE kind=?1 AND what=?2",
                    sql_params![queue, what],
                )?;
            }
            _ => {}
        }
        if views::summarized(event) {
            summarize(conn, event, written)?;
        }
    }
    Ok(())
}

/// Whether a run-less `ask_answered` row of the ledger answers `ask`: its
/// key ([`crate::domain::run::AskRef::key`], made of the answer's payload
/// when its row was written, [`Left::text`]) is in `ledger_answered_asks`.
fn answered(conn: &impl SqlRead, ask: &Left) -> Result<bool> {
    Ok(SqlRead::query_row(
        conn,
        "SELECT 1 FROM ledger_answered_asks WHERE what=?1 LIMIT 1",
        sql_params![ask.text()],
        |_| Ok(()),
    )
    .optional()?
    .is_some())
}

/// The latest summary the store keeps.
/// Whether the latest summary is one a binary before them wrote, holding no
/// record of what a reader takes of it ([`MainSummary::holds_record`]): the
/// views are then made again, whatever their mark says.
pub(in crate::infrastructure::runtime_store) fn older_summaries(
    conn: &impl SqlRead,
) -> Result<bool> {
    Ok(latest_summary(conn)?.is_some_and(|summary| !summary.holds_record()))
}

fn latest_summary(conn: &impl SqlRead) -> Result<Option<MainSummary>> {
    SqlRead::query_row(
        conn,
        "SELECT summary FROM ledger_main_summaries ORDER BY through DESC LIMIT 1",
        sql_params![],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|text| Ok(serde_json::from_str(&text)?))
    .transpose()
}

/// Fold `event` on the latest summary. A write takes it while the mark
/// says the filling took the rows up to it in, and leaves a later one to
/// the filling. An event a summary is past already is folded once: one the
/// store held before is none to fold again, and one it did not hold came
/// out of the order of the ids (a row a pass put back, a commit of a later
/// id first), so the summaries past the last kept one before it are folded
/// again from the rows up to where they reached ([`refold`]).
fn summarize(
    conn: &impl SqlWrite,
    event: &RunEvent,
    written: Option<(&Filled, bool)>,
) -> Result<()> {
    let id = event.id.as_i64();
    if let Some((filled, _)) = written {
        let taken = match filled {
            Filled::Done => true,
            Filled::Through(through) => through.parse::<i64>().is_ok_and(|through| id <= through),
            Filled::Not => false,
        };
        if !taken {
            return Ok(());
        }
    }
    let last = latest_summary(conn)?;
    if let Some(reached) = last
        .as_ref()
        .and_then(|summary| summary.through)
        .filter(|through| through.as_i64() >= id)
    {
        if written.is_some_and(|(_, existed)| !existed) {
            refold(conn, id, reached.as_i64())?;
        }
        return Ok(());
    }
    advance(conn, &last.unwrap_or_default(), event)?;
    Ok(())
}

/// Write the summary after `event` folded on `last`, the latest the store
/// keeps, in place of `last` unless it is kept; the next. When an event
/// crosses a main time, including a CI check following a future main event,
/// the writer builds missing time alternatives from the prefix once;
/// ordinary writes advance the existing ones. This rebuild is never done
/// by the window reader.
fn advance(conn: &impl SqlWrite, last: &MainSummary, event: &RunEvent) -> Result<MainSummary> {
    let id = event.id.as_i64();
    // The commits past a base's head its completion applies: their
    // changes, read of the row that records each.
    let mut changes = std::collections::HashMap::new();
    for sha in last.path_changes(event) {
        if let Some(change) = recorded_changes(conn, &sha, id)? {
            changes.insert(sha, change);
        }
    }
    let mut next = last.then(event, &|sha| changes.get(sha).cloned());
    if last.clock_went_back(event)
        && let Some(now) = crate::domain::stats::timestamp_millis(&event.created_at)
    {
        // Reuse the latest prefix valid at the new time. Only the crossed
        // clock span and the unsummarized tail need replay, not all settled
        // history before that prefix.
        let (base, _) =
            super::queue_named::summary_by(conn, last.through.unwrap_or(EventId::new(0)), now)?;
        let after = base
            .as_ref()
            .and_then(|base| base.through)
            .map_or(0, |id| id.as_i64());
        let texts: Vec<String> = SqlRead::query_map(
            conn,
            &format!(
                "SELECT records FROM {} WHERE {} AND inputs_from > ?1 AND inputs_from <= ?2 ORDER BY inputs_from",
                view_rows_from(SqlRead::dialect(conn)),
                view_rows_sql()
            ),
            sql_params![after, id],
            |r| r.get(0),
        )?;
        let mut events = Vec::new();
        for text in texts {
            events.extend(serde_json::from_str::<Vec<RunEvent>>(&text)?);
        }
        if let Some(base) = &base {
            let needed: BTreeSet<String> = events
                .iter()
                .flat_map(|event| base.path_changes(event))
                .collect();
            for sha in needed {
                if let Some(value) = recorded_changes(conn, &sha, id)? {
                    changes.insert(sha, value);
                }
            }
        }
        changes.extend(views::commit_changes(&events));
        next = next.with_clock_alternatives(base.as_ref(), &events, now, &|sha| {
            changes.get(sha).cloned()
        });
    }
    if let Some(through) = last.through.filter(|_| !last.kept()) {
        SqlWrite::execute(
            conn,
            "DELETE FROM ledger_main_summaries WHERE through=?1",
            sql_params![through.as_i64()],
        )?;
    }
    SqlWrite::execute(
        conn,
        "INSERT INTO ledger_main_summaries(through, at_ms, summary) VALUES (?1, ?2, ?3)",
        sql_params![id, next.at_ms, serde_json::to_string(&next)?],
    )?;
    Ok(next)
}

/// The changes the last `main_commits_recorded` before the event `before`
/// records of the commit `sha`, read of its queue row.
fn recorded_changes(conn: &impl SqlRead, sha: &str, before: i64) -> Result<Option<Value>> {
    let Some(event_id) = SqlRead::query_row(
        conn,
        "SELECT event_id FROM ledger_main_commits WHERE sha=?1 AND event_id<?2
          ORDER BY event_id DESC LIMIT 1",
        sql_params![sha, before],
        |r| r.get::<_, i64>(0),
    )
    .optional()?
    else {
        return Ok(None);
    };
    let Some(text) = SqlRead::query_row(
        conn,
        "SELECT records FROM ledger_rows WHERE kind=?1 AND key=?2",
        sql_params![LedgerKind::Queue.as_str(), event_id.to_string()],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    else {
        return Ok(None);
    };
    let changes = views::commit_changes(&serde_json::from_str::<Vec<RunEvent>>(&text)?);
    Ok(changes.get(sha).cloned())
}

/// Fold again the summaries past the last kept one before the event `id`
/// that came out of order, from the queue rows the views are made of up to
/// `reached` (where the summaries reached): the summaries after it are
/// removed and written again in the order of the ids, so they are those
/// of the rows whichever order the rows came in. It reads the rows since
/// that summary, about [`views::SUMMARY_EVERY`] events and the gap.
fn refold(conn: &impl SqlWrite, id: i64, reached: i64) -> Result<()> {
    let before = SqlRead::query_row(
        conn,
        "SELECT summary FROM ledger_main_summaries WHERE through < ?1
          ORDER BY through DESC LIMIT 1",
        sql_params![id],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|text| serde_json::from_str::<MainSummary>(&text))
    .transpose()?;
    // The last kept summary: one in place of which the next is written
    // goes too, and the fold starts from the one kept before it.
    let mut last = match before {
        Some(summary) if !summary.kept() => SqlRead::query_row(
            conn,
            "SELECT summary FROM ledger_main_summaries WHERE through < ?1
              ORDER BY through DESC LIMIT 1",
            sql_params![summary.through.map_or(0, |through| through.as_i64())],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|text| serde_json::from_str::<MainSummary>(&text))
        .transpose()?
        .unwrap_or_default(),
        Some(summary) => summary,
        None => MainSummary::default(),
    };
    let from = last.through.map_or(0, |through| through.as_i64());
    SqlWrite::execute(
        conn,
        "DELETE FROM ledger_main_summaries WHERE through > ?1",
        sql_params![from],
    )?;
    let texts: Vec<String> = SqlRead::query_map(
        conn,
        &format!(
            "SELECT records FROM {} WHERE {} AND inputs_from > ?1 AND inputs_from <= ?2
              ORDER BY inputs_from",
            view_rows_from(SqlRead::dialect(conn)),
            view_rows_sql()
        ),
        sql_params![from, reached],
        |r| r.get::<_, String>(0),
    )?;
    for text in texts {
        let records: Vec<RunEvent> = serde_json::from_str(&text)?;
        for event in records.iter().filter(|event| views::summarized(event)) {
            last = advance(conn, &last, event)?;
        }
    }
    Ok(())
}

/// Take into the views a part of the queue rows past the mark, by their
/// first event, and move the mark past those taken, in one write
/// transaction, bounded as `ledger_open`'s filling is
/// ([`open::ROWS_PER_FILL`], [`open::part_spent`]). A mark that counts as
/// none begins again from the first row, the views removed first. A row
/// written meanwhile past the mark has its summary left to the filling, so
/// that the summaries fold the events in the order of their ids. A mark
/// done over summaries that hold no record (an older binary's) counts as
/// none.
pub(super) fn fill_views(conn: &impl SqlWrite) -> Result<Filled> {
    let mut held = kept_view(conn, View::Queue)?;
    // Summaries a binary before them wrote hold no record of what a reader
    // takes of them ([`MainSummary::holds_record`]): the views are made
    // again from the first row, as of a mark of none.
    if held.is_done() && older_summaries(conn)? {
        write_filled(conn, View::Queue, &Filled::Not)?;
        held = Filled::Not;
    }
    let Some(after) = held.after() else {
        return Ok(held);
    };
    if after.is_none() {
        for table in [
            "ledger_snapshot_targets",
            "ledger_main_commits",
            "ledger_main_summaries",
            "ledger_answered_asks",
        ] {
            SqlWrite::execute(conn, &format!("DELETE FROM {table}"), sql_params![])?;
        }
        SqlWrite::execute(
            conn,
            "DELETE FROM ledger_open WHERE kind=?1",
            sql_params![LedgerKind::Queue.as_str()],
        )?;
    }
    let after: i64 = after.and_then(|after| after.parse().ok()).unwrap_or(0);
    let started = std::time::Instant::now();
    let limit = i64::try_from(open::ROWS_PER_FILL).unwrap_or(i64::MAX);
    let rows: Vec<(String, i64)> = SqlRead::query_map(
        conn,
        &format!(
            "SELECT key, inputs_from FROM {} WHERE {} AND inputs_from > ?1
              ORDER BY inputs_from LIMIT ?2",
            view_rows_from(SqlRead::dialect(conn)),
            view_rows_sql()
        ),
        sql_params![after, limit],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    )?;
    let (mut bytes, mut taken) = (0, 0_usize);
    for (key, _) in &rows {
        let text: String = SqlRead::query_row(
            conn,
            "SELECT records FROM ledger_rows WHERE kind=?1 AND key=?2",
            sql_params![LedgerKind::Queue.as_str(), key],
            |r| r.get(0),
        )?;
        bytes += text.len();
        let records: Vec<RunEvent> = serde_json::from_str(&text)?;
        write_row_views(conn, key, &records, None)?;
        taken += 1;
        if open::part_spent(bytes, started.elapsed()) {
            break;
        }
    }
    let last = taken.checked_sub(1).map(|at| rows[at].1.to_string());
    let filled = Filled::next(rows.len(), taken, last.as_deref());
    write_filled(conn, View::Queue, &filled)?;
    Ok(filled)
}

/// What the views hold, whatever their mark: the snapshots' targets as
/// (snapshot, task), the commits as (event, sha, reverts), the summaries
/// ascending, and the run-less asks left open by their row's key.
#[cfg(any(test, feature = "store-contract"))]
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Views {
    pub(crate) targets: BTreeSet<(i64, i64)>,
    pub(crate) commits: BTreeSet<(i64, String, Option<String>)>,
    pub(crate) summaries: Vec<MainSummary>,
    pub(crate) asks: BTreeMap<String, BTreeSet<Left>>,
}

/// What the store contracts drive of the views on a transaction of either
/// adapter (ADR-t3986-1 decision 10): the same functions the store's
/// [`LedgerStore`] operations run in theirs, and the reads that look them
/// up by what a read from a time names.
#[cfg(any(test, feature = "store-contract"))]
pub(crate) mod views_in {
    pub(crate) use super::Views;
    use super::*;

    /// Write `row` as every write of a ledger row does, its views with it.
    pub(crate) fn write_row(tx: &impl SqlWrite, row: &LedgerRow) -> Result<()> {
        upsert(tx, row).map(|_| ())
    }

    /// Write `row` as a binary that keeps no views of the queue rows does:
    /// the row and the views of the run rows alone.
    pub(crate) fn write_row_alone(tx: &impl SqlWrite, row: &LedgerRow) -> Result<()> {
        upsert_with(tx, row, false).map(|_| ())
    }

    /// Write `rows` as a part of the queue's rows in the middle of a pass
    /// is ([`LedgerStore::write_session_rows_marked`] with no marks): each
    /// read again first, R untouched.
    pub(crate) fn write_chunk(tx: &impl SqlWrite, rows: &[LedgerRow]) -> Result<bool> {
        let rows: Vec<(LedgerRow, Option<LedgerRow>)> = rows
            .iter()
            .map(|row| Ok((row.clone(), held_row(tx, row.kind, &row.key)?)))
            .collect::<Result<_>>()?;
        write_rows_marked(tx, &rows, None)
    }

    /// Write `marks` (W and R) as a pass's last write does, no row with
    /// them.
    pub(crate) fn write_marks(
        tx: &impl SqlWrite,
        marks: &crate::domain::ledger::session::written::Marks,
    ) -> Result<bool> {
        write_rows_marked(tx, &[], Some(marks))
    }

    /// The marks (W and R) the store holds.
    pub(crate) fn marks(
        tx: &impl SqlRead,
    ) -> Result<Option<crate::domain::ledger::session::written::Marks>> {
        marks_in(tx)
    }

    /// A part of the filling ([`LedgerStore::fill_ledger_queue_views`]).
    pub(crate) fn fill(tx: &impl SqlWrite) -> Result<Filled> {
        fill_views(tx)
    }

    /// The mark as a reader takes it.
    pub(crate) fn held(tx: &impl SqlRead) -> Result<Filled> {
        held_view(tx, View::Queue)
    }

    /// Whether a read from a time reads by the views
    /// ([`crate::application::LedgerWindowRead::queue_views_usable`]).
    pub(crate) fn usable(tx: &impl SqlRead) -> Result<bool> {
        super::super::queue_named::usable_in(tx)
    }

    /// Remove what the views and their mark hold.
    pub(crate) fn clear_views(tx: &impl SqlWrite) -> Result<()> {
        for table in [
            "ledger_snapshot_targets",
            "ledger_main_commits",
            "ledger_main_summaries",
            "ledger_answered_asks",
            "ledger_queue_views_filled",
        ] {
            SqlWrite::execute(tx, &format!("DELETE FROM {table}"), sql_params![])?;
        }
        Ok(())
    }

    /// What the views hold, whatever their mark.
    pub(crate) fn views(tx: &impl SqlRead) -> Result<Views> {
        let targets = SqlRead::query_map(
            tx,
            "SELECT snapshot_event_id, task_id FROM ledger_snapshot_targets",
            sql_params![],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )?
        .into_iter()
        .collect();
        let commits = SqlRead::query_map(
            tx,
            "SELECT event_id, sha, reverts FROM ledger_main_commits",
            sql_params![],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )?
        .into_iter()
        .collect();
        let summaries = SqlRead::query_map(
            tx,
            "SELECT summary FROM ledger_main_summaries ORDER BY through",
            sql_params![],
            |r| r.get::<_, String>(0),
        )?
        .into_iter()
        .map(|text| Ok(serde_json::from_str(&text)?))
        .collect::<Result<_>>()?;
        let mut asks: BTreeMap<String, BTreeSet<Left>> = BTreeMap::new();
        for (key, what) in SqlRead::query_map(
            tx,
            "SELECT key, what FROM ledger_open WHERE kind=?1",
            sql_params![LedgerKind::Queue.as_str()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )? {
            if let Some(what) = Left::of_text(&what) {
                asks.entry(key).or_default().insert(what);
            }
        }
        Ok(Views {
            targets,
            commits,
            summaries,
            asks,
        })
    }

    /// The snapshots that name `task`.
    pub(crate) fn snapshots_naming(tx: &impl SqlRead, task: i64) -> Result<Vec<i64>> {
        Ok(SqlRead::query_map(
            tx,
            "SELECT snapshot_event_id FROM ledger_snapshot_targets WHERE task_id=?1
              ORDER BY snapshot_event_id",
            sql_params![task],
            |r| r.get::<_, i64>(0),
        )?)
    }

    /// The events that record `sha`, and those that record a commit
    /// reverting it.
    pub(crate) fn commits_of(tx: &impl SqlRead, sha: &str) -> Result<(Vec<i64>, Vec<i64>)> {
        let of = |column: &str| -> Result<Vec<i64>> {
            Ok(SqlRead::query_map(
                tx,
                &format!(
                    "SELECT event_id FROM ledger_main_commits WHERE {column}=?1 ORDER BY event_id"
                ),
                sql_params![sha],
                |r| r.get::<_, i64>(0),
            )?)
        };
        Ok((of("sha")?, of("reverts")?))
    }

    /// The latest summary through `id` or before.
    pub(crate) fn summary_through(tx: &impl SqlRead, id: i64) -> Result<Option<MainSummary>> {
        SqlRead::query_row(
            tx,
            "SELECT summary FROM ledger_main_summaries WHERE through <= ?1
              ORDER BY through DESC LIMIT 1",
            sql_params![id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|text| Ok(serde_json::from_str(&text)?))
        .transpose()
    }
}
