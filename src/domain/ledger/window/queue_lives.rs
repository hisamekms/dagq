//! Names used by the queue's range read. Collecting them has no store effects.
use super::{RowsFrom, WindowRows};
use crate::domain::{EventId, EventKind};
use std::collections::BTreeSet;

/// Subject kinds whose range needs the named queue seeks as well as promise 6.
pub const KINDS: &[&str] = &[
    "slots_full",
    "claim_hold",
    "landing_hold",
    "supervisor_started",
    "supervisor_alive",
    "supervisor_stopped",
    "mark_recorded",
    "mark_retracted",
    "forecast_recorded",
    "candidates_sampled",
    "update_failed",
    "backend_call_failed",
    "auto_repaired",
    "stall_config_loaded",
    "ask_opened",
    "ask_answered",
];

/// Each moved event kind may supply a named supervisor's last evidence
/// before S, including evidence other than a heartbeat. Other kinds are
/// still read whole and supply their evidence themselves.
pub const LIFE_KINDS: &[&str] = &[
    "supervisor_started",
    "supervisor_alive",
    "supervisor_stopped",
    "slots_full_started",
    "slots_full_ended",
    "claim_held",
    "claim_resumed",
    "landing_held",
    "landing_resumed",
    "mark_recorded",
    "mark_retracted",
    "forecast_recorded",
    "candidates_sampled",
    "update_failed",
    "backend_call_failed",
    "auto_repaired",
    "stall_config_loaded",
    "ask_opened",
    "ask_answered",
];

/// Keys the records and task rows of the range name.
#[derive(Debug, Default)]
pub struct Names {
    pub supervisors: BTreeSet<String>,
    pub asks: BTreeSet<Option<String>>,
    pub marks: BTreeSet<EventId>,
    pub tasks: BTreeSet<i64>,
}

/// Match AskRef's key after an indexed lookup of its SQL aliases. SQL
/// booleans and numbers can have the same scalar representation on SQLite.
pub fn ask_matches(event: &crate::domain::RunEvent, key: &Option<String>) -> bool {
    &crate::domain::run::restore_payload::<crate::domain::run::AskRef>(&event.payload).key() == key
}

impl Names {
    /// Include the latest start before S in the life keys sought.
    pub fn supervisors_with(&self, events: &[crate::domain::RunEvent]) -> BTreeSet<String> {
        self.supervisors
            .iter()
            .cloned()
            .chain(
                events
                    .iter()
                    .filter_map(|event| event.payload["supervisor"].as_str().map(str::to_owned)),
            )
            .collect()
    }
    /// Include retractable marks supplied by the snapshot's second stage.
    pub fn marks_with(&self, events: &[crate::domain::RunEvent]) -> BTreeSet<EventId> {
        self.marks
            .iter()
            .copied()
            .chain(
                events
                    .iter()
                    .filter(|e| e.kind.is_in(&crate::domain::marks::RETRACTABLE_KINDS))
                    .map(|e| e.id),
            )
            .collect()
    }
}

impl WindowRows {
    /// Take sought queue records through the same bounds as saved queue rows.
    pub fn take_queue_records(&mut self, records: Vec<crate::domain::RunEvent>) {
        let mut kept = Vec::new();
        for mut event in records {
            if event.kind == EventKind::RunClaimed {
                // Claims used to derive a forecast's marks are not proof
                // of an occupied slot: a closed run's row may be left out.
                self.earlier.claims.push(event);
                continue;
            }
            if event
                .kind
                .known()
                .is_some_and(|kind| crate::domain::forecast::score::TASK_MARK_KINDS.contains(&kind))
            {
                event.run_id = None;
            }
            if crate::domain::ledger::owner::owner(&event)
                != crate::domain::ledger::owner::Owner::Queue
            {
                // These are forecast plan marks, not queue records: they
                // must not advance queue_before past the saved queue rows.
                self.earlier_records.push(event);
            } else {
                kept.push(event);
            }
        }
        let mut row = super::LedgerRow::new(super::LedgerKind::Queue, String::new(), None, "");
        row.records = kept;
        self.take(row);
    }

    /// Names for the queue seeks, from the records held and the task rows
    /// that reach S. The last sample before S also names its supervisor.
    pub fn queue_names(&self, from: &RowsFrom) -> Names {
        let mut names = Names::default();
        for row in &self.rows {
            if row.kind == super::LedgerKind::Task && !from.leaves_out(row) {
                names.tasks.extend(row.task_id.map(|task| task.as_i64()));
            }
        }
        for event in &self.records {
            let reaches = crate::domain::stats::timestamp_millis(&event.created_at)
                .is_none_or(|at| at >= from.ended_before_ms);
            if reaches || event.kind == EventKind::CandidatesSampled {
                names
                    .supervisors
                    .extend(event.payload["supervisor"].as_str().map(str::to_owned));
            }
            if reaches {
                if event.kind == EventKind::AskAnswered {
                    names.asks.insert(
                        crate::domain::run::restore_payload::<crate::domain::run::AskRef>(
                            &event.payload,
                        )
                        .key(),
                    );
                } else if let Some(id) = event.payload.get("ask_id") {
                    names.asks.insert(Some(
                        id.as_str().map_or_else(|| id.to_string(), str::to_owned),
                    ));
                }
            }
            if event.kind.is_in(&crate::domain::marks::RETRACTABLE_KINDS) {
                names.marks.insert(event.id);
            }
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ledger::testkit::{Log, ms};
    use crate::domain::ledger::window::WindowQuery;

    #[test]
    fn names_include_answer_applications_and_the_last_sample_s_supervisor() {
        let mut log = Log::default();
        log.add(
            None,
            None,
            "supervisor_alive",
            serde_json::json!({"supervisor":"old"}),
            1,
        )
        .add(
            None,
            None,
            "candidates_sampled",
            serde_json::json!({"supervisor":"sample"}),
            2,
        )
        .add(
            None,
            None,
            "mark_recorded",
            serde_json::json!({"label":"m"}),
            3,
        )
        .add(
            None,
            None,
            "auto_repaired",
            serde_json::json!({"ask_id":81,"supervisor":"range"}),
            11,
        )
        .add(
            None,
            None,
            "ask_answered",
            serde_json::json!({"ask_id":"82"}),
            12,
        );
        let mut read = WindowRows::new(
            &WindowQuery {
                read_from_ms: Some(ms(10)),
                ..WindowQuery::default()
            },
            0,
        );
        read.take_queue_records(log.events.clone());
        let names = read.queue_names(&RowsFrom {
            ended_before_ms: ms(10),
            inputs_through: EventId::new(3),
        });
        assert_eq!(
            names.supervisors,
            BTreeSet::from(["sample".into(), "range".into()])
        );
        assert_eq!(
            names.asks,
            BTreeSet::from([Some("81".into()), Some("82".into())])
        );
        assert_eq!(names.marks, BTreeSet::from([log.events[2].id]));
    }
    #[test]
    fn forecast_claims_stay_out_of_slot_occupancy() {
        let mut log = Log::default();
        log.add(
            Some(1),
            Some(&crate::domain::ledger::testkit::run_id(1)),
            "run_claimed",
            serde_json::json!({"parallel":2}),
            1,
        );
        let mut read = WindowRows::new(
            &WindowQuery {
                read_from_ms: Some(ms(10)),
                ..WindowQuery::default()
            },
            0,
        );
        read.take_queue_records(log.events.clone());
        let window = read.into_window(&WindowQuery {
            now_ms: ms(20),
            ..WindowQuery::default()
        });
        assert!(window.events().is_empty());
        assert_eq!(window.earlier.claims, log.events);
    }
}
