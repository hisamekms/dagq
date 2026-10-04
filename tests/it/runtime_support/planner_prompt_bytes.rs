//! What the prompt of a planner of the runtime's took, as
//! `planner_prompt_written` records it (task 1571, ADR-t1566-1 decision 6).

use crate::plan_review::planner_prompt;
use rusqlite::Connection;
use serde_json::Value;
use std::path::Path;

/// The `planner_prompt_written` of `planner` (task 1571, ADR-t1566-1
/// decision 6): one, of `kind`, whose `prompt_bytes` are its prompt's as
/// written, within `limit`, section by section.
pub(crate) fn assert_planner_prompt_bytes(
    db: &Path,
    planner: dagq::domain::PlannerId,
    kind: &str,
    limit: usize,
) {
    let prompt = planner_prompt(db, planner);
    let written: Vec<Value> = Connection::open(db)
        .unwrap()
        .prepare(
            "SELECT payload FROM run_events WHERE kind = 'planner_prompt_written'
               AND json_extract(payload, '$.planner_id') = ?1",
        )
        .unwrap()
        .query_map([planner.as_i64()], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
        .collect();
    assert_eq!(written.len(), 1, "{written:?}");
    let written = &written[0];
    assert_eq!(written["subject"], "planner");
    assert_eq!(written["prompt"], kind, "{written}");
    let bytes = &written["prompt_bytes"];
    assert_eq!(bytes["total"], prompt.len(), "{written}");
    assert_eq!(bytes["limit"], limit, "{written}");
    let sections: u64 = bytes["sections"]
        .as_object()
        .unwrap()
        .values()
        .map(|bytes| bytes.as_u64().unwrap())
        .sum();
    assert_eq!(sections, prompt.len() as u64, "{written}");
}
