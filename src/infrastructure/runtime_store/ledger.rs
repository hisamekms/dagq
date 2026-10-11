//! The ledger's rows in queue.db (`ledger_rows`, [`LedgerStore`]).

use super::*;
use crate::application::LedgerStore;
use crate::domain::ledger::{InputsThrough, LedgerKind, LedgerRow, Measure, Totals};
use crate::domain::run_phase::{Blocker, Holds};

const BLOCKERS: [(Blocker, &str); 7] = [
    (Blocker::Queue, "blocker_queue_ms"),
    (Blocker::Ai, "blocker_ai_ms"),
    (Blocker::Compute, "blocker_compute_ms"),
    (Blocker::Human, "blocker_human_ms"),
    (Blocker::Runtime, "blocker_runtime_ms"),
    (Blocker::External, "blocker_external_ms"),
    (Blocker::Infra, "blocker_infra_ms"),
];

const HOLDS: [(Holds, &str); 3] = [
    (Holds::WorkerSlot, "holds_worker_slot_ms"),
    (Holds::LandingSlot, "holds_landing_slot_ms"),
    (Holds::None, "holds_none_ms"),
];

/// The key a tag's sum is under in [`Totals`].
fn tag(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn millis(map: &std::collections::BTreeMap<String, Measure>, key: String) -> i64 {
    map.get(&key).map_or(0, |measure| measure.duration_ms)
}

fn text(value: &impl serde::Serialize) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

impl LedgerStore for SqliteQueue {
    fn write_ledger_row(&self, row: &LedgerRow) -> Result<bool> {
        let totals = &row.totals;
        let mut columns: Vec<(&str, rusqlite::types::Value)> = vec![
            ("kind", row.kind.as_str().to_owned().into()),
            ("key", row.key.clone().into()),
            ("task_id", row.task_id.map(|task| task.as_i64()).into()),
            ("ledger_v", i64::from(row.ledger_v).into()),
            ("built_at", row.built_at.clone().into()),
            ("inputs_through", text(&row.inputs_through)?.into()),
            ("final", i64::from(row.is_final).into()),
            ("legacy", i64::from(row.is_legacy()).into()),
            (
                "legacy_values",
                row.legacy.as_ref().map(text).transpose()?.into(),
            ),
            ("started_at", row.started_at.into()),
            ("ended_at", row.ended_at.into()),
            ("outcome", row.outcome.clone().into()),
            ("duration_ms", totals.all.duration_ms.into()),
            ("slot_ms", totals.all.slot_ms.into()),
            ("unattributed_ms", totals.unattributed_ms.into()),
            ("attempts", totals.attempts.into()),
            ("totals", text(totals)?.into()),
            ("intervals", text(&row.intervals)?.into()),
            ("attributes", text(&row.attributes)?.into()),
            (
                "after_claim",
                row.after_claim.as_ref().map(text).transpose()?.into(),
            ),
            ("detail", text(&row.detail)?.into()),
        ];
        for (blocker, column) in BLOCKERS {
            columns.push((column, millis(&totals.by_blocker, tag(blocker)).into()));
        }
        for (holds, column) in HOLDS {
            columns.push((column, millis(&totals.by_holds, tag(holds)).into()));
        }
        let names: Vec<&str> = columns.iter().map(|(name, _)| *name).collect();
        let marks: Vec<String> = (1..=names.len()).map(|n| format!("?{n}")).collect();
        let updates: Vec<String> = names
            .iter()
            .filter(|name| !matches!(**name, "kind" | "key"))
            .map(|name| format!("{name}=excluded.{name}"))
            .collect();
        // A legacy row is frozen: the conflict leaves it as it is.
        let written = self.conn.execute(
            &format!(
                "INSERT INTO ledger_rows({}) VALUES ({})
                 ON CONFLICT(kind, key) DO UPDATE SET {} WHERE ledger_rows.legacy = 0",
                names.join(","),
                marks.join(","),
                updates.join(","),
            ),
            rusqlite::params_from_iter(columns.into_iter().map(|(_, value)| value)),
        )?;
        Ok(written == 1)
    }

    fn ledger_row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>> {
        self.conn
            .query_row(
                "SELECT * FROM ledger_rows WHERE kind=?1 AND key=?2",
                params![kind.as_str(), key],
                ledger_row,
            )
            .optional()?
            .transpose()
    }

    fn ledger_rows(&self, kind: LedgerKind) -> Result<Vec<LedgerRow>> {
        let mut statement = self
            .conn
            .prepare("SELECT * FROM ledger_rows WHERE kind=?1 ORDER BY key")?;
        let rows = statement.query_map([kind.as_str()], ledger_row)?;
        rows.map(|row| row?).collect()
    }
}

/// A row of `ledger_rows`; its JSON is read in the outer result, so a row
/// that does not read fails the read rather than reading as a default.
fn ledger_row(r: &Row) -> rusqlite::Result<Result<LedgerRow>> {
    let json = |column: &str| -> rusqlite::Result<String> { r.get(column) };
    let optional = |column: &str| -> rusqlite::Result<Option<String>> { r.get(column) };
    let kind: String = r.get("kind")?;
    let key: String = r.get("key")?;
    let task_id: Option<i64> = r.get("task_id")?;
    let ledger_v: i64 = r.get("ledger_v")?;
    let built_at: String = r.get("built_at")?;
    let inputs_through = json("inputs_through")?;
    let is_final: bool = r.get("final")?;
    let legacy = optional("legacy_values")?;
    let started_at: Option<i64> = r.get("started_at")?;
    let ended_at: Option<i64> = r.get("ended_at")?;
    let outcome: Option<String> = r.get("outcome")?;
    let (totals, intervals, attributes, detail) = (
        json("totals")?,
        json("intervals")?,
        json("attributes")?,
        json("detail")?,
    );
    let after_claim = optional("after_claim")?;
    Ok((|| {
        Ok(LedgerRow {
            kind: LedgerKind::parse(&kind).ok_or_else(|| anyhow!("unknown ledger kind {kind}"))?,
            key,
            task_id: task_id.map(TaskId::new),
            ledger_v: u32::try_from(ledger_v)?,
            built_at,
            inputs_through: serde_json::from_str::<InputsThrough>(&inputs_through)?,
            is_final,
            started_at,
            ended_at,
            outcome,
            totals: serde_json::from_str::<Totals>(&totals)?,
            intervals: serde_json::from_str(&intervals)?,
            attributes: serde_json::from_str(&attributes)?,
            after_claim: after_claim
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
            detail: serde_json::from_str(&detail)?,
            legacy: legacy.as_deref().map(serde_json::from_str).transpose()?,
        })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ledger::{self, LEDGER_V};
    use crate::domain::{EventId, RunEvent, RunId};

    const RUN: &str = "11111111-1111-4111-8111-111111111111";

    fn event(id: i64, kind: &str, payload: Value, secs: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(7)),
            goal_id: None,
            run_id: Some(RunId::new(RUN).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: crate::domain::marks::utc_text((1_900_000_000 + secs) * 1000),
            actor: None,
        }
    }

    fn phase(id: i64, phase: &str, blocker: &str, holds: &str, secs: i64) -> RunEvent {
        event(
            id,
            "run_phase_changed",
            json!({"phase": phase, "blocker": blocker, "holds": holds,
                   "attempt": {"kind": "first", "n": 1}, "cause": "x", "v": 1}),
            secs,
        )
    }

    fn rows(queue: &SqliteQueue) -> i64 {
        queue
            .conn
            .query_row("SELECT count(*) FROM ledger_rows", [], |r| r.get(0))
            .unwrap()
    }

    /// A folded run's row round-trips with its sums in their columns, and
    /// writing it again (or a newer fold of it) keeps one row.
    #[test]
    fn a_run_row_is_written_once_per_run_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let events = vec![
            event(
                1,
                "run_claimed",
                json!({"attributes": {"build": "1", "x.new": "kept"}}),
                0,
            ),
            phase(2, "provisioning", "infra", "worker_slot", 0),
            phase(3, "worker", "ai", "worker_slot", 10),
            phase(4, "landing_answer", "human", "none", 70),
            phase(5, "ended", "runtime", "none", 100),
        ];
        let row = ledger::run_row(RUN, &events, &[], 0, "2030-01-01T00:00:00.000Z");
        assert!(queue.write_ledger_row(&row).unwrap());
        assert!(queue.write_ledger_row(&row).unwrap());
        assert_eq!(rows(&queue), 1);
        let read = queue.ledger_row(LedgerKind::Run, RUN).unwrap().unwrap();
        assert_eq!(read, row);
        assert_eq!(read.inputs_through.events, EventId::new(5));
        assert_eq!(read.attributes["x.new"], "kept");
        let (duration, slot, ai, human, none, legacy, v): (i64, i64, i64, i64, i64, i64, i64) =
            queue
                .conn
                .query_row(
                    "SELECT duration_ms, slot_ms, blocker_ai_ms, blocker_human_ms, holds_none_ms,
                        legacy, ledger_v FROM ledger_rows",
                    [],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                        ))
                    },
                )
                .unwrap();
        assert_eq!(
            (duration, slot, ai, human, none, legacy, v),
            (
                100_000,
                70_000,
                60_000,
                30_000,
                30_000,
                0,
                i64::from(LEDGER_V)
            )
        );
        let mut later = row.clone();
        later.ledger_v = LEDGER_V + 1;
        assert!(queue.write_ledger_row(&later).unwrap());
        assert_eq!(rows(&queue), 1);
        assert_eq!(queue.ledger_rows(LedgerKind::Run).unwrap(), vec![later]);
        assert!(queue.ledger_rows(LedgerKind::Task).unwrap().is_empty());
    }

    /// A legacy row is frozen: a later write of the same run, of any
    /// version, leaves it as first written.
    #[test]
    fn a_legacy_row_is_not_written_over() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let events = vec![
            event(1, "run_claimed", json!({"dagq_version": "0.1"}), 0),
            event(2, "receipt_observed", json!({}), 60),
            event(3, "run_integrated", json!({}), 90),
        ];
        let legacy = ledger::run_row(RUN, &events, &[], 0, "2030-01-01T00:00:00.000Z");
        assert!(legacy.is_legacy());
        assert!(queue.write_ledger_row(&legacy).unwrap());
        let mut rebuilt = legacy.clone();
        rebuilt.legacy = None;
        rebuilt.ledger_v = LEDGER_V + 1;
        assert!(!queue.write_ledger_row(&rebuilt).unwrap());
        assert_eq!(rows(&queue), 1);
        let read = queue.ledger_row(LedgerKind::Run, RUN).unwrap().unwrap();
        assert_eq!(read, legacy);
        assert_eq!(read.legacy.unwrap()["work"], 60);
    }
}
