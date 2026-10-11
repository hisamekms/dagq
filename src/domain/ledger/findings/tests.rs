use serde_json::{Value, json};

use super::*;
use crate::domain::EventId;
use crate::domain::marks::utc_text;

const T0: i64 = 1_900_000_000;

/// The events of a queue as a test lays them down, one second apart.
#[derive(Default)]
struct Log {
    events: Vec<RunEvent>,
}

impl Log {
    fn push(&mut self, kind: &str, payload: Value) -> &mut Self {
        let n = i64::try_from(self.events.len()).unwrap() + 1;
        self.events.push(RunEvent {
            id: EventId::new(n),
            task_id: None,
            run_id: None,
            goal_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: utc_text((T0 + n) * 1000),
            actor: None,
        });
        self
    }

    fn at(&self, window_end: Option<i64>) -> Findings {
        match fold(&self.events, window_end) {
            Folded::Findings(findings) => findings,
            Folded::BeforeBaseline => panic!("before the baseline"),
        }
    }
}

fn values(id: i64, summary: &str, impact: &str, status: &str, occurrences: i64) -> Value {
    json!({"id": id, "target": "queue", "task_id": null, "run_id": null, "goal_id": null,
        "kind": "conflict_hotspot", "subject": "src/a.rs", "summary": summary,
        "impact": impact, "status": status, "occurrences": occurrences, "last_seen_at": T0})
}

fn record(id: i64, summary: &str, impact: &str, status: &str, occurrences: i64) -> Value {
    json!({"finding_id": id, VALUES: values(id, summary, impact, status, occurrences)})
}

/// A later record wins, a record without the values (a binary before the
/// record) is passed over, and the unsettled findings come in the order
/// `findings` lists them.
#[test]
fn a_later_record_wins_and_the_unsettled_come_larger_impact_first() {
    let mut log = Log::default();
    log.push(
        event_kind::FINDING_RECORDED,
        record(1, "a", "normal", "open", 1),
    )
    .push(
        event_kind::FINDING_RECORDED,
        record(2, "b", "normal", "open", 1),
    )
    .push(
        event_kind::FINDING_UPDATED,
        record(2, "b, often", "high", "open", 2),
    )
    .push(
        event_kind::FINDING_RECORDED,
        record(3, "c", "high", "open", 1),
    )
    .push(
        event_kind::FINDING_STATUS_CHANGED,
        record(3, "c", "high", "dismissed", 1),
    )
    .push(
        event_kind::FINDING_UPDATED,
        json!({"finding_id": 1, "changed": ["summary"]}),
    );
    let findings = log.at(None);
    assert_eq!(findings.findings.len(), 3);
    assert_eq!(findings.findings[&FindingId::new(1)].summary, "a");
    let unsettled: Vec<(i64, &str)> = findings
        .unsettled()
        .iter()
        .map(|finding| (finding.id.as_i64(), finding.impact.as_str()))
        .collect();
    assert_eq!(unsettled, [(2, "high"), (1, "normal")]);
    // A window that ends before the update holds the value before it.
    assert_eq!(
        log.at(Some(T0 + 2)).findings[&FindingId::new(2)].summary,
        "b"
    );
}

/// A window that ends before the first baseline's mark is told apart; one
/// that holds the mark takes its group whole, and a window that ends
/// inside a later group keeps the values from before it.
#[test]
fn a_window_before_the_baseline_is_told_apart_and_holds_a_group_whole() {
    let baseline = |id: i64, summary: &str| json!({"finding_id": id, VALUES: values(id, summary, "normal", "resolved", 4)});
    let mark = json!({"findings": 1});
    let mut log = Log::default();
    log.push(event_kind::FINDING_BASELINE, baseline(1, "older"))
        .push(event_kind::FINDING_BASELINE, mark.clone());
    assert_eq!(fold(&log.events, Some(T0)), Folded::BeforeBaseline);
    assert_eq!(fold(&log.events, Some(T0 + 1)), Folded::BeforeBaseline);
    let first = log.at(Some(T0 + 2));
    assert_eq!(first.findings[&FindingId::new(1)].summary, "older");
    assert!(first.unsettled().is_empty());
    log.push(event_kind::FINDING_BASELINE, baseline(1, "again"))
        .push(event_kind::FINDING_BASELINE, mark);
    assert_eq!(log.at(Some(T0 + 3)), first);
    assert_eq!(
        log.at(Some(T0 + 4)).findings[&FindingId::new(1)].summary,
        "again"
    );
    // Other kinds are not read, whatever they carry.
    log.push(
        event_kind::TASK_CREATED,
        record(1, "not a finding's", "high", "open", 1),
    );
    assert_eq!(log.at(None).findings[&FindingId::new(1)].summary, "again");
}
