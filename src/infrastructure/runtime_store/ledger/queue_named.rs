//! The queue rows of the kinds a read from a time reads by their range
//! with the views ([`crate::domain::ledger::window::queue_from::BY_VIEWS`]),
//! and what the range names of them before it, sought in the SQL SQLite
//! and Postgres share ([`crate::application::LedgerWindowRead::queue_views_usable`],
//! [`crate::application::LedgerWindowRead::queue_main_before`], [`crate::application::LedgerWindowRead::queue_sought`]):
//! each read a seek of the views' tables, of a key of `ledger_rows` or of
//! an index of `run_events`, the events of the latter judged visible as the
//! whole read sees them in the same snapshot. What is named and which of
//! the events found are kept is the domain's
//! ([`crate::domain::ledger::window::queue_named`]).
use super::*;
use crate::domain::EventKind;
use crate::domain::ledger::queue_views::MainSummary;
use crate::domain::ledger::window::queue_named::{
    self as named, MainBefore, QueueSought, QueueWants, SUMMARIZED_KINDS,
};

/// The rows of a page of a walk down the ids of `run_events`.
const PAGE: i64 = 32;

/// The latest summary of main's record through the event `?1` or before it,
/// by its primary key (the latest of all through the greatest id).
pub(super) const SUMMARY_SQL: &str = "SELECT summary FROM ledger_main_summaries
  WHERE through <= ?1 ORDER BY through DESC LIMIT 1";

/// A branch's earliest or latest CI check (`?1`) at `?2` or before it,
/// folded by the summary through `?3`. Each queue row holds one check;
/// its end is the parsed event time, so noncanonical timestamps work too.
pub(super) fn ci_boundary_sql(dialect: SqlDialect, latest: bool) -> String {
    let order = if latest { "DESC" } else { "ASC" };
    format!(
        "SELECT * FROM ledger_rows{} WHERE kind = 'queue' AND subject_kind = 'ci_checked'
          AND {} = ?1 AND ended_at <= ?2 AND inputs_from <= ?3
          ORDER BY ended_at {order}, inputs_from {order} LIMIT 1",
        dialect.indexed_by("ledger_ci_by_branch_time"),
        dialect.json_field("records", "$[0].payload.branch", JsonType::Text),
    )
}

/// The queue rows of the kinds a summary folds past the event `?1`, by
/// their event, of the partial index of the rows the views are made of
/// (`ledger_rows_queue_views`).
pub(super) fn after_summary_sql(dialect: SqlDialect) -> String {
    let kinds: Vec<String> = SUMMARIZED_KINDS
        .iter()
        .map(|kind| format!("'{}'", kind.as_str()))
        .collect();
    format!(
        "SELECT * FROM {} WHERE {} AND inputs_from > ?1 AND subject_kind IN ({})
          ORDER BY inputs_from",
        queue_views::view_rows_from(dialect),
        queue_views::view_rows_sql(),
        kinds.join(", ")
    )
}

/// The events that record the commit `?1` (`main_commits_by_sha`).
pub(super) const COMMITS_BY_SHA_SQL: &str =
    "SELECT event_id FROM ledger_main_commits WHERE sha = ?1 ORDER BY event_id";

/// The events that record a commit reverting the commit `?1`
/// (`main_commits_by_reverts`).
pub(super) const COMMITS_BY_REVERTS_SQL: &str =
    "SELECT event_id FROM ledger_main_commits WHERE reverts = ?1 ORDER BY event_id";

/// The turns to red of CI on the commit `?1` (`ci_reds_by_sha`).
pub(super) fn reds_by_sha_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT * FROM run_events{} WHERE kind = 'ci_turned_red' AND {} = ?1 ORDER BY id",
        dialect.indexed_by("ci_reds_by_sha"),
        dialect.ledger_index_key("payload", "sha"),
    )
}

/// A page of the events of the goal `?1` below the id `?2`, down the ids,
/// at most `?3` (`events_by_goal`).
pub(super) fn goal_down_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT * FROM run_events{} WHERE goal_id = ?1 AND id < ?2 ORDER BY id DESC LIMIT ?3",
        dialect.indexed_by("events_by_goal"),
    )
}

/// A page of the `goal_submitted` of the goal `?1` below the id `?2`, down
/// the ids, at most `?3` (`goal_submissions`).
pub(super) fn submissions_down_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT * FROM run_events{} WHERE kind = 'goal_submitted' AND goal_id = ?1 AND id < ?2
          ORDER BY id DESC LIMIT ?3",
        dialect.indexed_by("goal_submissions"),
    )
}

/// A page of the `goal_closed` of the goal `?1` past the id `?2`, up the
/// ids, at most `?3` (`goal_closes`).
pub(super) fn closes_up_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT * FROM run_events{} WHERE kind = 'goal_closed' AND goal_id = ?1 AND id > ?2
          ORDER BY id LIMIT ?3",
        dialect.indexed_by("goal_closes"),
    )
}

/// The condition of the marks of `kind`'s baselines, as the partial index
/// of them holds it.
fn marks_condition(dialect: SqlDialect, kind: EventKind) -> String {
    match kind {
        EventKind::FindingBaseline => format!(
            "kind = 'finding_baseline' AND {} IS NULL",
            dialect.ledger_index_key("payload", "finding")
        ),
        _ => "kind = 'attributes_baseline' AND task_id IS NULL AND goal_id IS NULL".to_owned(),
    }
}

/// The partial index of the marks of `kind`'s baselines.
fn marks_index(kind: EventKind) -> &'static str {
    match kind {
        EventKind::FindingBaseline => "finding_baseline_marks",
        _ => "attribute_baseline_marks",
    }
}

/// A page of the marks of `kind`'s baselines below the id `?1`, down the
/// ids, at most `?2`, of the marks alone.
pub(super) fn marks_down_sql(dialect: SqlDialect, kind: EventKind) -> String {
    format!(
        "SELECT * FROM run_events{} WHERE {} AND id < ?1 ORDER BY id DESC LIMIT ?2",
        dialect.indexed_by(marks_index(kind)),
        marks_condition(dialect, kind),
    )
}

/// A page of the marks of `kind`'s baselines past the id `?1`, up the ids,
/// at most `?2`: the mark that closes a baseline's group is the first.
pub(super) fn marks_up_sql(dialect: SqlDialect, kind: EventKind) -> String {
    format!(
        "SELECT * FROM run_events{} WHERE {} AND id > ?1 ORDER BY id LIMIT ?2",
        dialect.indexed_by(marks_index(kind)),
        marks_condition(dialect, kind),
    )
}

/// The changes of the findings that name the proposal `?1`
/// (`finding_changes_by_proposal`).
pub(super) fn links_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT * FROM run_events{} WHERE kind = 'finding_status_changed' AND {} = ?1
          ORDER BY id",
        dialect.indexed_by("finding_changes_by_proposal"),
        dialect.ledger_index_key("payload", "proposal_id"),
    )
}

/// The row of the finding `?1` (`ledger_rows_findings`).
pub(super) fn finding_row_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT * FROM ledger_rows{} WHERE kind = 'queue' AND subject_kind = 'finding'
          AND subject_id = ?1",
        dialect.indexed_by("ledger_rows_findings"),
    )
}

fn events(conn: &impl SqlRead, sql: &str, values: &[SqlValue]) -> Result<Vec<RunEvent>> {
    Ok(SqlRead::query_map(conn, sql, values, event_row)?)
}

/// The events of `events` the whole read sees, in their order (none when
/// that cannot be judged; a read seeks only once it can).
fn visible(conn: &impl SqlRead, events: &[RunEvent]) -> Result<Vec<RunEvent>> {
    Ok(visibility::visible_in(conn, events)?.unwrap_or_default())
}

/// The values a payload's id is bound as against
/// [`SqlDialect::ledger_index_key`]: SQLite's `json_extract` gives a
/// number and its text apart, Postgres' `->>` gives one text.
fn id_values(dialect: SqlDialect, id: i64) -> Vec<SqlValue> {
    match dialect {
        SqlDialect::Sqlite => vec![id.to_sql_value(), id.to_string().to_sql_value()],
        SqlDialect::Postgres => vec![id.to_string().to_sql_value()],
    }
}

fn summary_of(text: Option<String>) -> Result<Option<MainSummary>> {
    text.map(|text| Ok(serde_json::from_str(&text)?))
        .transpose()
}

/// [`crate::application::LedgerWindowRead::queue_views_usable`] of `conn`.
pub(super) fn usable_in(conn: &impl SqlRead) -> Result<bool> {
    for table in [
        "ledger_main_summaries",
        "ledger_main_commits",
        "ledger_queue_views_filled",
    ] {
        if !earlier::has_table(conn, table)? {
            return Ok(false);
        }
    }
    if !held_view(conn, View::Queue)?.is_done() {
        return Ok(false);
    }
    let latest = summary_of(
        SqlRead::query_row(conn, SUMMARY_SQL, sql_params![i64::MAX], |r| r.get(0)).optional()?,
    )?;
    if latest.is_some_and(|summary| !summary.holds_record()) {
        return Ok(false);
    }
    Ok(visibility::visible_in(conn, &[])?.is_some())
}

/// The latest summary through `queue_before` or before it whose events
/// every window from `from_ms` holds ([`MainSummary::folds_by`]): a walk
/// down only while the eligible state is later than the time. A clock
/// regression's alternative stays in the same row, so a future event
/// before settled history never makes this walk cross that history.
pub(super) fn summary_by(
    conn: &impl SqlRead,
    queue_before: EventId,
    from_ms: i64,
) -> Result<(Option<MainSummary>, usize)> {
    let mut through = queue_before.as_i64();
    let mut read = 0;
    loop {
        let Some(summary) = summary_of(
            SqlRead::query_row(conn, SUMMARY_SQL, sql_params![through], |r| r.get(0)).optional()?,
        )?
        else {
            return Ok((None, read));
        };
        read += 1;
        let previous = summary.through.map_or(0, |id| id.as_i64()) - 1;
        if let Some(summary) = summary.for_read_from(from_ms) {
            return Ok((Some(summary), read));
        }
        through = previous;
    }
}

/// [`crate::application::LedgerWindowRead::queue_main_before`] of `conn`.
pub(super) fn main_before_in(
    conn: &impl SqlRead,
    queue_before: EventId,
    from_ms: i64,
) -> Result<MainBefore> {
    let (mut summary, summaries_read) = summary_by(conn, queue_before, from_ms)?;
    let through = summary
        .as_ref()
        .and_then(|summary| summary.through)
        .map_or(0, |through| through.as_i64());
    let mut rows = SqlRead::query_map(
        conn,
        &after_summary_sql(SqlRead::dialect(conn)),
        sql_params![through],
        ledger_row,
    )?
    .into_iter()
    .collect::<Result<Vec<_>>>()?;
    if let Some(summary) = &mut summary {
        for branch in summary.take_late_ci(from_ms) {
            for latest in [false, true] {
                rows.extend(
                    SqlRead::query_row(
                        conn,
                        &ci_boundary_sql(SqlRead::dialect(conn), latest),
                        sql_params![branch, from_ms, through],
                        ledger_row,
                    )
                    .optional()?
                    .transpose()?,
                );
            }
        }
    }
    Ok(MainBefore {
        summary,
        summaries_read,
        rows,
    })
}

/// The records of the queue rows of the events `ids` (a row of one event
/// each, keyed by its id).
fn queue_records(conn: &impl SqlRead, ids: &BTreeSet<i64>) -> Result<Vec<RunEvent>> {
    let mut records = Vec::new();
    for id in ids {
        if let Some(row) = held_row(conn, LedgerKind::Queue, &id.to_string())? {
            records.extend(row.records);
        }
    }
    Ok(records)
}

/// The commits of `wants` by the table of main's commits: the records of
/// the commits of `wants.shas` and of those reverting one of
/// `wants.landed`, read of their rows.
fn commits(conn: &impl SqlRead, wants: &QueueWants) -> Result<Vec<RunEvent>> {
    let mut ids = BTreeSet::new();
    let of = |sql: &str, sha: &str| -> Result<Vec<i64>> {
        Ok(SqlRead::query_map(conn, sql, sql_params![sha], |r| {
            r.get::<_, i64>(0)
        })?)
    };
    for sha in &wants.shas {
        ids.extend(of(COMMITS_BY_SHA_SQL, sha)?);
    }
    for sha in &wants.landed {
        ids.extend(of(COMMITS_BY_REVERTS_SQL, sha)?);
    }
    Ok(queue_records(conn, &ids)?
        .into_iter()
        .filter(|event| named::records_commit(event, &wants.shas, &wants.landed))
        .collect())
}

/// CI's turns to red the whole read sees on the commits of `wants.landed`.
fn reds(conn: &impl SqlRead, wants: &QueueWants) -> Result<Vec<RunEvent>> {
    let sql = reds_by_sha_sql(SqlRead::dialect(conn));
    let mut found = Vec::new();
    for sha in &wants.landed {
        found.extend(events(conn, &sql, sql_params![sha.as_str()])?);
    }
    visible(conn, &found)
}

/// The first mark of the attributes' baselines past `id` the whole read
/// sees: a walk up the marks past those no row holds yet.
fn mark_after(conn: &impl SqlRead, id: i64) -> Result<Option<RunEvent>> {
    let sql = marks_up_sql(SqlRead::dialect(conn), EventKind::AttributesBaseline);
    let mut after = id;
    loop {
        let page = events(conn, &sql, sql_params![after, PAGE])?;
        let Some(last) = page.last().map(|event| event.id.as_i64()) else {
            return Ok(None);
        };
        if let Some(mark) = visible(conn, &page)?.into_iter().next() {
            return Ok(Some(mark));
        }
        after = last;
    }
}

/// The records of the goal `goal` the whole read sees, down the ids to the
/// last that replaces it before `from_ms` ([`named::stops_goal_walk`]),
/// with the mark of each baseline among them.
fn goal_records(conn: &impl SqlRead, goal: i64, from_ms: i64) -> Result<Vec<RunEvent>> {
    let sql = goal_down_sql(SqlRead::dialect(conn));
    let mut records = Vec::new();
    let mut below = i64::MAX;
    loop {
        let page = events(conn, &sql, sql_params![goal, below, PAGE])?;
        let Some(last) = page.last().map(|event| event.id.as_i64()) else {
            return Ok(records);
        };
        for event in visible(conn, &page)? {
            if !named::goal_record(&event) {
                continue;
            }
            let mark = if event.kind == EventKind::AttributesBaseline {
                mark_after(conn, event.id.as_i64())?
            } else {
                None
            };
            let stops = named::stops_goal_walk(&event, mark.as_ref(), from_ms);
            records.push(event);
            records.extend(mark);
            if stops {
                return Ok(records);
            }
        }
        below = last;
    }
}

/// The `goal_closed` of the goal `goal` the whole read sees past the
/// snapshot's event `snapshot`, up the ids to the first before `from_ms`
/// ([`named::before`]): a forecast's score takes the first close after the
/// snapshot that a window holds, and every window holds that one, so the
/// first it holds is among these or among the range's (a close of the time
/// or after it is the range's). As many as the range's closes and one.
fn closes_after(
    conn: &impl SqlRead,
    goal: i64,
    snapshot: i64,
    from_ms: i64,
) -> Result<Vec<RunEvent>> {
    let sql = closes_up_sql(SqlRead::dialect(conn));
    let mut found = Vec::new();
    let mut after = snapshot;
    loop {
        let page = events(conn, &sql, sql_params![goal, after, PAGE])?;
        let Some(last) = page.last().map(|event| event.id.as_i64()) else {
            return Ok(found);
        };
        for event in visible(conn, &page)? {
            if !named::goal_record(&event) {
                continue;
            }
            let stops = named::before(&event, from_ms);
            found.push(event);
            if stops {
                return Ok(found);
            }
        }
        after = last;
    }
}

/// The `goal_submitted` of the goal `goal` the whole read sees down the ids
/// to the last before `from_ms`: the proposal that holds the goal at each
/// window's end is among theirs.
fn submissions(conn: &impl SqlRead, goal: i64, from_ms: i64) -> Result<Vec<RunEvent>> {
    let sql = submissions_down_sql(SqlRead::dialect(conn));
    let mut found = Vec::new();
    let mut below = i64::MAX;
    loop {
        let page = events(conn, &sql, sql_params![goal, below, PAGE])?;
        let Some(last) = page.last().map(|event| event.id.as_i64()) else {
            return Ok(found);
        };
        for event in visible(conn, &page)? {
            let stops = named::before(&event, from_ms);
            found.push(event);
            if stops {
                return Ok(found);
            }
        }
        below = last;
    }
}

/// The latest mark of `kind`'s baselines before `from_ms` the whole read
/// sees: a walk down the marks past those of the range and those no row
/// holds yet.
fn latest_mark(conn: &impl SqlRead, kind: EventKind, from_ms: i64) -> Result<Option<RunEvent>> {
    let sql = marks_down_sql(SqlRead::dialect(conn), kind);
    let mut below = i64::MAX;
    loop {
        let page = events(conn, &sql, sql_params![below, PAGE])?;
        let Some(last) = page.last().map(|event| event.id.as_i64()) else {
            return Ok(None);
        };
        let found = visible(conn, &page)?
            .into_iter()
            .find(|mark| named::is_mark(kind, mark) && named::before(mark, from_ms));
        if found.is_some() {
            return Ok(found);
        }
        below = last;
    }
}

/// The rows of the findings a change the whole read sees links to one of
/// `proposals`.
fn finding_rows(conn: &impl SqlRead, proposals: &BTreeSet<i64>) -> Result<Vec<LedgerRow>> {
    let dialect = SqlRead::dialect(conn);
    let (links, row_sql) = (links_sql(dialect), finding_row_sql(dialect));
    let mut findings = BTreeSet::new();
    for &proposal in proposals {
        let mut found = Vec::new();
        for value in id_values(dialect, proposal) {
            found.extend(events(conn, &links, &[value])?);
        }
        findings.extend(
            visible(conn, &found)?
                .iter()
                .filter_map(|event| named::linked_finding(event, proposals)),
        );
    }
    let mut rows = Vec::new();
    for finding in findings {
        let held =
            SqlRead::query_map(conn, &row_sql, sql_params![finding.to_string()], ledger_row)?;
        for row in held {
            rows.push(row?);
        }
    }
    Ok(rows)
}

/// [`crate::application::LedgerWindowRead::queue_sought`] of `conn`.
pub(super) fn sought_in(conn: &impl SqlRead, wants: &QueueWants) -> Result<QueueSought> {
    let mut records = commits(conn, wants)?;
    records.extend(reds(conn, wants)?);
    let mut proposals = wants.proposals.clone();
    for &goal in &wants.goals {
        records.extend(goal_records(conn, goal, wants.from_ms)?);
        let submitted = submissions(conn, goal, wants.from_ms)?;
        proposals.extend(named::submitted_proposals(&submitted));
    }
    for &(goal, snapshot) in &wants.closes {
        records.extend(closes_after(conn, goal, snapshot, wants.from_ms)?);
    }
    if wants.marks {
        for kind in [EventKind::AttributesBaseline, EventKind::FindingBaseline] {
            records.extend(latest_mark(conn, kind, wants.from_ms)?);
        }
    }
    records.sort_by_key(|event| event.id);
    records.dedup_by_key(|event| event.id);
    Ok(QueueSought {
        records,
        rows: finding_rows(conn, &proposals)?,
    })
}
