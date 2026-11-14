//! What the rows a read from a time leaves out still give, sought by what
//! the range names ([`LedgerStore::ledger_earlier_named`]): the seeks of
//! the shared SQL, each by a key of `ledger_rows` or an index of
//! `run_events`, and the parts read whole again when what they stand on
//! cannot be trusted. Which events are kept is the domain's
//! ([`crate::domain::ledger::window::named`]).
use super::*;
use crate::domain::ledger::window::named::member_end;
use crate::domain::ledger::window::named::{
    GOAL_TASK_KINDS, MEMBER_END_KINDS, MEMBER_KINDS, Named, PROPOSAL_KINDS, goal_task,
};

/// Which parts of the read were sought by what is named (true) and which
/// were read whole as [`super::earlier_in`] reads them (false).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sought {
    /// The task and session rows' records.
    pub(crate) records: bool,
    /// The run rows' records.
    pub(crate) runs: bool,
    /// The first Executions.
    pub(crate) executions: bool,
}

/// The values an id is bound as against [`SqlDialect::ledger_index_key`]:
/// a payload holds it as a number or as text, which SQLite's
/// `json_extract` gives apart and Postgres' `->>` gives as one text.
fn key_values(dialect: SqlDialect, id: i64) -> Vec<SqlValue> {
    match dialect {
        SqlDialect::Sqlite => vec![id.to_sql_value(), id.to_string().to_sql_value()],
        SqlDialect::Postgres => vec![id.to_string().to_sql_value()],
    }
}

/// The events of the kind `kind` (a literal, so that a partial index of
/// the kind serves it) whose payload's `key` is `?1`.
pub(super) fn by_payload_sql(dialect: SqlDialect, kind: Option<EventKind>, key: &str) -> String {
    let field = dialect.ledger_index_key("payload", key);
    let kind = kind
        .map(|kind| format!("kind = '{}' AND ", kind.as_str()))
        .unwrap_or_default();
    format!("SELECT * FROM run_events WHERE {kind}{field} = ?1 ORDER BY id")
}

/// The events of the task `?1` of the kind `kind` past the id `?2`, by
/// `events_by_task_kind`.
pub(super) fn by_task_kind_sql(kind: EventKind) -> String {
    format!(
        "SELECT * FROM run_events WHERE task_id = ?1 AND kind = '{}' AND id > ?2 ORDER BY id",
        kind.as_str()
    )
}

/// A page of the events of the task `?1` of the kind `kind` below the id
/// `?2`, down the ids by `events_by_task_kind`, at most `?3`.
pub(super) fn by_task_kind_down_sql(kind: EventKind) -> String {
    format!(
        "SELECT * FROM run_events WHERE task_id = ?1 AND kind = '{}' AND id < ?2
          ORDER BY id DESC LIMIT ?3",
        kind.as_str()
    )
}

/// Every event of the task `?1`, by `events_by_task`.
pub(super) const BY_TASK_SQL: &str = "SELECT * FROM run_events WHERE task_id = ?1 ORDER BY id";

/// The events of the task `?1` a row other than its task row may own, by
/// `events_by_task`: those of its runs, and those of no run of a kind or
/// with a key a session's are ([`owner::SESSION_KINDS`],
/// [`owner::SESSION_KEYS`]; the caller keeps those [`owner::owner`] gives
/// to no task). Of a task whose row the read holds, whose own records are
/// that row's.
pub(super) fn by_task_held_sql(dialect: SqlDialect) -> String {
    use crate::domain::ledger::owner;
    let kinds = owner::SESSION_KINDS
        .iter()
        .map(|kind| format!("'{}'", kind.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let keys = owner::SESSION_KEYS
        .iter()
        .map(|key| {
            format!(
                "{} IS NOT NULL",
                dialect.json_field("payload", &format!("$.{key}"), JsonType::Text)
            )
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    format!(
        "SELECT * FROM run_events WHERE task_id = ?1 AND (run_id IS NOT NULL OR kind IN ({kinds}) OR {keys}) ORDER BY id"
    )
}

/// A page of the `integration_started` below the id `?1`, down the ids by
/// `events_by_kind`, at most `?2`.
pub(super) const STARTS_DOWN_SQL: &str = "SELECT * FROM run_events
  WHERE kind = 'integration_started' AND id < ?1 ORDER BY id DESC LIMIT ?2";

/// The session rows of the keys in `[?1, ?2)` the read leaves out, on the
/// primary key.
pub(super) fn session_range_sql(dialect: SqlDialect) -> String {
    let key = dialect.by_bytes("r.key");
    format!(
        "SELECT r.records FROM ledger_rows r
          WHERE r.kind = 'session' AND {key} >= ?1 AND {key} < ?2 AND {}",
        rows_left_out_sql(dialect, 3, 4)
    )
}

/// What [`earlier_run_record_sql`] keeps of the run row `?1` if the read
/// leaves it out, on the primary key.
pub(super) fn run_row_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT {record} FROM ledger_rows r, {records} j
          WHERE r.kind = 'run' AND r.key = ?1 AND {left_out} AND {kept}",
        record = dialect.json_element("j.value", JsonType::Text),
        records = dialect.json_elements("r.records"),
        left_out = rows_left_out_sql(dialect, 2, 3),
        kept = earlier_run_record_sql(dialect),
    )
}

fn events(conn: &impl SqlRead, sql: &str, values: &[SqlValue]) -> Result<Vec<RunEvent>> {
    Ok(SqlRead::query_map(conn, sql, values, event_row)?)
}

fn by_payload(
    conn: &impl SqlRead,
    kind: Option<EventKind>,
    key: &str,
    id: i64,
) -> Result<Vec<RunEvent>> {
    let dialect = SqlRead::dialect(conn);
    let sql = by_payload_sql(dialect, kind, key);
    let mut found = Vec::new();
    for value in key_values(dialect, id) {
        found.extend(events(conn, &sql, &[value])?);
    }
    Ok(found)
}

/// The events the seeks by `named` give of the EventStore, unjudged, and
/// `named` with the planners and plan reviews of the sessions' records
/// among them that name what it names.
fn sought(conn: &impl SqlRead, named: &Named) -> Result<(Vec<RunEvent>, Named)> {
    let mut taken = Vec::new();
    for &task in named.tasks() {
        // A task row the read holds already gives the task's own records:
        // reading them again would grow with its history. Its runs' and
        // the sessions' events that name it (a planner's revision of its
        // proposal) are of other rows, sought still.
        if named.holds_task(task) {
            let held = events(
                conn,
                &by_task_held_sql(SqlRead::dialect(conn)),
                sql_params![task],
            )?;
            taken.extend(held.into_iter().filter(|event| {
                crate::domain::ledger::owner::owner(event)
                    != crate::domain::ledger::owner::Owner::Task
            }));
        } else {
            taken.extend(events(conn, BY_TASK_SQL, sql_params![task])?);
        }
        taken.extend(by_payload(
            conn,
            Some(EventKind::FollowUpRegistered),
            "task_id",
            task,
        )?);
    }
    for task in named.snapshot_tasks() {
        if named.names_task(task) {
            continue;
        }
        for kind in crate::domain::forecast::score::TASK_MARK_KINDS {
            taken.extend(events(
                conn,
                &by_task_kind_sql(kind),
                sql_params![task, 0_i64],
            )?);
        }
    }
    for &review in named.reviews() {
        taken.extend(by_payload(conn, None, "plan_review_id", review)?);
    }
    let mut submitted = Vec::new();
    for proposal in named.goal_proposals() {
        submitted.extend(by_payload(
            conn,
            Some(EventKind::GoalSubmitted),
            "proposal_id",
            proposal,
        )?);
    }
    // A proposal a finding links that holds a goal is sought as one named.
    let named = &named.with_goal_proposals_of(&submitted);
    let goals: BTreeSet<i64> = submitted
        .iter()
        .filter_map(|event| named.goal_of(event))
        .collect();
    let mut members = BTreeSet::new();
    for &proposal in named.proposals() {
        for kind in MEMBER_KINDS {
            for event in by_payload(conn, Some(kind), "proposal_id", proposal)? {
                members.extend(named.member_of(&event));
                taken.push(event);
            }
        }
    }
    for &goal in &goals {
        for (kind, key) in GOAL_TASK_KINDS {
            for event in by_payload(conn, Some(kind), key, goal)? {
                members.extend(goal_task(&event, &goals).filter(|task| !named.names_task(*task)));
            }
        }
    }
    // A session's record a seek gives that names what the range names
    // (a planner's revision of a proposal named) comes with its row.
    let sessions = named.with_sessions_of(&taken);
    let kept = taken.into_iter().filter(|event| named.takes(event));
    let mut taken: Vec<RunEvent> = kept.collect();
    for task in members {
        taken.extend(member_ends(conn, task)?);
        for kind in PROPOSAL_KINDS {
            let named_proposal = events(conn, &by_task_kind_sql(kind), sql_params![task, 0_i64])?
                .into_iter()
                .filter(|event| named.names(event));
            taken.extend(named_proposal);
        }
    }
    Ok((taken, sessions))
}

/// The last event of the task `task` of the kind `kind` the whole read
/// sees: a walk down its ids past those no row holds yet (as many as the
/// builder is behind).
fn last_visible(conn: &impl SqlRead, task: i64, kind: EventKind) -> Result<Option<RunEvent>> {
    const PAGE: i64 = 16;
    let sql = by_task_kind_down_sql(kind);
    let mut below = i64::MAX;
    loop {
        let page = events(conn, &sql, sql_params![task, below, PAGE])?;
        let Some(last) = page.last().map(|event| event.id.as_i64()) else {
            return Ok(None);
        };
        if let Some(seen) = visible(conn, &page)?.into_iter().next() {
            return Ok(Some(seen));
        }
        below = last;
    }
}

/// The events of `events` the whole read sees, in their order; none when
/// that cannot be judged (the read has gone back to every row's records
/// before it seeks).
fn visible(conn: &impl SqlRead, events: &[RunEvent]) -> Result<Vec<RunEvent>> {
    Ok(visibility::visible_in(conn, events)?.unwrap_or_default())
}

/// The ends of the member task `task` ([`member_end`]) the whole read sees:
/// the last of each kind, and the reopenings after the last change of
/// status.
fn member_ends(conn: &impl SqlRead, task: i64) -> Result<Vec<RunEvent>> {
    let mut ends = Vec::new();
    for kind in MEMBER_END_KINDS {
        if kind != EventKind::TaskReopened {
            ends.extend(last_visible(conn, task, kind)?);
        }
    }
    let status = ends
        .iter()
        .filter(|event| event.kind == EventKind::TaskStatusChanged)
        .map(|event| event.id.as_i64())
        .max()
        .unwrap_or(0);
    let reopened = events(
        conn,
        &by_task_kind_sql(EventKind::TaskReopened),
        sql_params![task, status],
    )?;
    ends.extend(visible(conn, &reopened)?);
    let read = ends.clone();
    Ok(ends
        .into_iter()
        .filter(|event| member_end(event, &read))
        .collect())
}

/// The session rows' records of the planners and plan reviews `named`
/// names that the read leaves out, by the ranges of their keys.
fn session_records(conn: &impl SqlRead, from: &RowsFrom, named: &Named) -> Result<Vec<RunEvent>> {
    let sql = session_range_sql(SqlRead::dialect(conn));
    let mut records = Vec::new();
    for (low, high) in named.session_key_ranges() {
        for row in SqlRead::query_map(
            conn,
            &sql,
            sql_params![
                low.as_str(),
                high.as_str(),
                from.ended_before_ms,
                from.inputs_through.as_i64(),
                from.time_text().as_str()
            ],
            |r| r.get::<_, String>(0),
        )? {
            records.extend(serde_json::from_str::<Vec<RunEvent>>(&row)?);
        }
    }
    Ok(records)
}

/// What [`earlier_run_record_sql`] keeps of the run row `key` if the read
/// leaves it out.
fn run_records(conn: &impl SqlRead, from: &RowsFrom, key: &str) -> Result<Vec<RunEvent>> {
    let texts = SqlRead::query_map(
        conn,
        &run_row_sql(SqlRead::dialect(conn)),
        sql_params![
            key,
            from.ended_before_ms,
            from.inputs_through.as_i64(),
            from.time_text().as_str()
        ],
        |r| r.get::<_, String>(0),
    )?;
    texts
        .iter()
        .map(|text| Ok(serde_json::from_str::<RunEvent>(text)?))
        .collect()
}

/// What [`earlier_run_record_sql`] keeps of the run row of the last
/// `integration_started` of a time at or before the read's, by id, among
/// those the run rows it leaves out hold: a walk down the ids of the kind
/// past those timed after it, those of a row read (of the range, as many as
/// it holds) or of no row, and those no row holds yet (as many as the
/// builder is behind), judged in the same snapshot.
fn last_start(conn: &impl SqlRead, from: &RowsFrom) -> Result<Vec<RunEvent>> {
    const PAGE: i64 = 64;
    let mut below = i64::MAX;
    loop {
        let page = events(conn, STARTS_DOWN_SQL, sql_params![below, PAGE])?;
        let Some(last) = page.last().map(|event| event.id.as_i64()) else {
            return Ok(Vec::new());
        };
        for start in visible(conn, &page)? {
            let before = crate::domain::stats::timestamp_millis(&start.created_at)
                .is_some_and(|at| at <= from.ended_before_ms);
            if let (true, Some(run)) = (before, start.run_id) {
                let records = run_records(conn, from, run.as_str())?;
                if !records.is_empty() {
                    return Ok(records);
                }
            }
        }
        below = last;
    }
}

/// Whether the store holds the table `name` (one of a schema before it
/// does not).
pub(super) fn has_table(conn: &impl SqlRead, name: &str) -> Result<bool> {
    Ok(!SqlRead::query_map(
        conn,
        &SqlRead::dialect(conn).table_exists_sql(),
        sql_params![name],
        |r| r.get::<_, i64>(0),
    )?
    .is_empty())
}

/// The first Executions of the table of every run row's.
fn table_executions(conn: &impl SqlRead) -> Result<Vec<FirstExecution>> {
    Ok(SqlRead::query_map(
        conn,
        "SELECT actor, provider, at FROM ledger_first_executions ORDER BY actor, provider",
        sql_params![],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    )?
    .into_iter()
    .filter_map(|(actor, provider, at)| FirstExecution::of(&actor, provider, at))
    .collect())
}

/// [`LedgerStore::ledger_earlier_named`] in `conn`'s snapshot, with which
/// parts were sought.
pub(crate) fn earlier_named_in(
    conn: &impl SqlRead,
    from: &RowsFrom,
    named: &Named,
) -> Result<(EarlierRecords, Sought)> {
    let mut records = Vec::new();
    let judged = visibility::visible_in(conn, &[])?.is_some();
    let open = has_table(conn, "ledger_open")? && has_table(conn, "ledger_open_filled")?;
    let firsts = has_table(conn, "ledger_first_executions")?
        && has_table(conn, "ledger_first_executions_ready")?;
    let runs = judged && open && held_filled(conn)?.is_done();
    if judged {
        let (candidates, sessions) = sought(conn, named)?;
        records.extend(visible(conn, &candidates)?);
        records.extend(session_records(conn, from, &sessions)?);
    } else {
        records.extend(whole_records_in(conn, from)?);
    }
    if runs {
        for key in left_open_entries(conn)?.into_keys() {
            records.extend(run_records(conn, from, &key)?);
        }
        records.extend(last_start(conn, from)?);
    } else {
        records.extend(whole_run_records_in(conn, from)?);
    }
    let executions = firsts && held_view(conn, View::First)?.is_done();
    let mut executions_read = if executions {
        table_executions(conn)?
    } else {
        whole_executions_in(conn, from)?
    };
    // The session rows left out but those named give their first
    // Executions here: their records are not read (`judged`), and the table
    // holds the run rows' alone.
    if judged {
        executions_read.extend(left_out_executions_in(conn, from, LedgerKind::Session)?);
    }
    records.sort_by_key(|event| event.id);
    records.dedup_by_key(|event| event.id);
    Ok((
        EarlierRecords {
            records,
            executions: executions_read,
        },
        Sought {
            records: judged,
            runs,
            executions,
        },
    ))
}
