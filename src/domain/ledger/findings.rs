//! The findings the report counts and lists (`findings_open` and its
//! lines), folded from the EventStore alone (ADR-t1662-2,
//! docs/design/attribute-records.md "findingの属性"): every change of a finding records its
//! attributes after the change ([`FindingAttributes`]) as `finding` in
//! `finding_recorded`, `finding_updated` or `finding_status_changed`, a
//! later record wins, and a finding no record gives is known from its
//! `finding_baseline` on. Measurement only: no decision of the runtime
//! reads it.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::super::{
    FindingAttributes, FindingId, FindingStatus, GoalId, Impact, RunEvent, RunId, TaskId,
    event_kind,
};

/// The kinds [`fold`] reads: a reader may give it only these.
pub const KINDS: &[&str] = &[
    event_kind::FINDING_BASELINE,
    event_kind::FINDING_RECORDED,
    event_kind::FINDING_UPDATED,
    event_kind::FINDING_STATUS_CHANGED,
];

/// The key of a record's attributes in its payload.
pub const VALUES: &str = "finding";

/// Every finding known from the records, by id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Findings {
    pub findings: BTreeMap<FindingId, FindingAttributes>,
}

impl Findings {
    /// The unsettled findings (`open`, `proposed`) larger impact first:
    /// what `findings` lists by default, which the report counts and lists.
    pub fn unsettled(&self) -> Vec<FindingAttributes> {
        let mut unsettled: Vec<FindingAttributes> = self
            .findings
            .values()
            .filter(|finding| finding.status.is_unsettled())
            .cloned()
            .collect();
        unsettled.sort_by(FindingAttributes::by_impact);
        unsettled
    }
}

/// What [`fold`] returns: the findings, or that the window ends before
/// the first baseline, when the records do not hold them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Folded {
    BeforeBaseline,
    Findings(Findings),
}

/// The findings at `window_end` (unix seconds; none for every event) from
/// `events` (ascending id) alone. A baseline is one group: the findings'
/// `finding_baseline` and, after them, the queue's mark that closes it
/// (one without `finding`); a window takes a group's records only when it
/// holds the mark. A window that ends before the first mark is
/// [`Folded::BeforeBaseline`]. A record without the values (a binary
/// before the record) is passed over. Reads nothing but `events`.
pub fn fold(events: &[RunEvent], window_end: Option<i64>) -> Folded {
    fold_within(events, super::WindowEnd::secs(window_end))
}

/// [`fold`] for a window that ends at `end`: an event, a time or both.
pub fn fold_within(events: &[RunEvent], end: super::WindowEnd) -> Folded {
    let is_mark =
        |event: &RunEvent| event.kind == event_kind::FINDING_BASELINE && values(event).is_none();
    let is_record =
        |event: &RunEvent| event.kind == event_kind::FINDING_BASELINE && values(event).is_some();
    let Some(taken) = super::baseline::taken(events, end, is_mark, is_record) else {
        return Folded::BeforeBaseline;
    };
    let mut findings = Findings::default();
    for (event, _) in events.iter().zip(&taken).filter(|(_, taken)| **taken) {
        if !KINDS.contains(&event.kind.as_str()) {
            continue;
        }
        if let Some(finding) = values(event).and_then(restore) {
            findings.findings.insert(finding.id, finding);
        }
    }
    Folded::Findings(findings)
}

/// The values a record of a finding holds, restored; none for a record
/// without them (a binary before the record, a baseline's mark).
pub fn values_of(event: &RunEvent) -> Option<FindingAttributes> {
    values(event).and_then(restore)
}

fn values(event: &RunEvent) -> Option<&serde_json::Value> {
    event.payload.get(VALUES)
}

/// [`FindingAttributes`] as a record holds them.
#[derive(Deserialize)]
struct Recorded {
    id: FindingId,
    target: String,
    task_id: Option<TaskId>,
    run_id: Option<String>,
    goal_id: Option<GoalId>,
    kind: String,
    subject: String,
    summary: String,
    impact: Impact,
    status: FindingStatus,
    occurrences: i64,
    last_seen_at: i64,
}

/// The attributes a record holds; none for a value that is not one.
fn restore(value: &serde_json::Value) -> Option<FindingAttributes> {
    let recorded = Recorded::deserialize(value).ok()?;
    Some(FindingAttributes {
        id: recorded.id,
        target: recorded.target,
        task_id: recorded.task_id,
        run_id: recorded.run_id.map(RunId::new).transpose().ok()?,
        goal_id: recorded.goal_id,
        kind: recorded.kind,
        subject: recorded.subject,
        summary: recorded.summary,
        impact: recorded.impact,
        status: recorded.status,
        occurrences: recorded.occurrences,
        last_seen_at: recorded.last_seen_at,
    })
}

#[cfg(test)]
mod tests;
