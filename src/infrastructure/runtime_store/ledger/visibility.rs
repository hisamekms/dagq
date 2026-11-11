//! Visibility of small event reads against the saved view, in its snapshot.
use super::*;
use crate::domain::ledger::owner::{Owner, owner};
use std::collections::{BTreeSet, HashMap};

/// The same expression as ledger_rows_by_inputs_through, for both stores.
fn past_mark_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT records FROM ledger_rows WHERE kind=?1 AND {through}>?2",
        through = dialect.json_field("inputs_through", "$.events", JsonType::Integer),
    )
}

fn past_mark(conn: &impl SqlRead, kind: LedgerKind, mark: EventId) -> Result<BTreeSet<EventId>> {
    let records = SqlRead::query_map(
        conn,
        &past_mark_sql(conn.dialect()),
        sql_params![kind.as_str(), mark],
        |r| r.get::<_, String>(0),
    )?;
    let mut ids = BTreeSet::new();
    for records in records {
        let records: Vec<RunEvent> = serde_json::from_str(&records)?;
        ids.extend(records.into_iter().map(|event| event.id));
    }
    Ok(ids)
}

pub(super) fn visible_in(
    conn: &impl SqlRead,
    candidates: &[RunEvent],
) -> Result<Option<Vec<RunEvent>>> {
    let marks = marks_in(conn)?;
    let Some(w) = written::written_through(marks.as_ref().map(|m| &m.sessions), &[]) else {
        return Ok(None);
    };
    let (Some(queue), Some(core)) = (
        written::rows_to_resume(marks.as_ref(), &[], true),
        written::rows_to_resume(marks.as_ref(), &[], false),
    ) else {
        return Ok(None);
    };
    let coverage = held_coverage(conn)?;
    let mut owners = HashMap::new();
    let mut open_runs: Option<BTreeSet<String>> = None;
    let mut session_records = None;
    let mut queue_records = None;
    let mut visible = Vec::new();
    for event in candidates {
        if coverage
            .as_ref()
            .is_some_and(|c| crate::domain::ledger::window::omitted::covered(c, event))
        {
            continue;
        }
        let keep = match owner(event) {
            Owner::Task if event.id <= core.through.events => true,
            Owner::Run | Owner::Task => {
                let (kind, key) = if owner(event) == Owner::Run {
                    (LedgerKind::Run, event.run_id.as_ref().unwrap().to_string())
                } else {
                    (LedgerKind::Task, event.task_id.unwrap().to_string())
                };
                let through = match owners.entry((kind, key.clone())) {
                    std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                    std::collections::hash_map::Entry::Vacant(entry) => *entry
                        .insert(held_row(conn, kind, &key)?.map(|row| row.inputs_through.events)),
                };
                match through {
                    Some(through) => event.id <= through,
                    None if kind == LedgerKind::Run => {
                        if open_runs.is_none() {
                            let rows = SqlRead::query_map(
                                conn,
                                "SELECT * FROM ledger_rows WHERE kind='task' AND ended_at IS NULL",
                                sql_params![],
                                ledger_row,
                            )?
                            .into_iter()
                            .collect::<Result<Vec<_>>>()?;
                            open_runs = Some(
                                rows.iter()
                                    .flat_map(crate::domain::ledger::window::runs_named)
                                    .collect(),
                            );
                        }
                        open_runs.as_ref().unwrap().contains(&key)
                    }
                    None => false,
                }
            }
            owner => {
                let (kind, mark, records) = if owner == Owner::Session {
                    (LedgerKind::Session, w, &mut session_records)
                } else {
                    (LedgerKind::Queue, queue.through.events, &mut queue_records)
                };
                if event.id <= mark {
                    true
                } else {
                    if records.is_none() {
                        *records = Some(past_mark(conn, kind, mark)?);
                    }
                    records.as_ref().unwrap().contains(&event.id)
                }
            }
        };
        if keep {
            visible.push(event.clone());
        }
    }
    Ok(Some(visible))
}

#[cfg(test)]
mod tests;
