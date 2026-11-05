//! The queue rows the store holds, each written by a pass at some position
//! `P`, cut to an earlier position `at` (docs/design/measurement.md
//! "台帳を作る係"): what builds the ledger makes its queue book and its
//! books of the waits and the lives again at `at` from them, so that the
//! events after `at` it reads again go into them once. A row is as it was
//! written unless something of it is past `at` (a record, its last input);
//! such a row is cut by its kind, from its head and the reads [`Needs`]
//! names, each bounded by the row and not by the history:
//!
//! - **One event, an alert, a deferral, a hold**: the head alone. An end
//!   past `at` (its record, or the event of no record of its own its
//!   `inputs_through.events` names) is undone: the row is open, reading up
//!   to its start. A hold its supervisor's stop ended by `at` reads that
//!   supervisor's life up to `at`.
//! - **A finding**: its records with their bodies (its row's,
//!   `LedgerStore::ledger_row`); its values are those of its last record by
//!   `at`.
//! - **A stream**: one its next baseline replaced by `at` is as written;
//!   another is open at `at`, its records (with their bodies when one is
//!   past `at`) cut there, reading up to its supervisor's last event by
//!   `at` and ended by that life.
//! - **A slots full** not freed by `at`: open at `at`, reading up to its
//!   supervisor's last event by `at` and ended by that life.
//! - **A landing slot**: whether its attempt ended by `at` is told by the
//!   next slot's key (the heads') or its run's events by `at`
//!   (`LedgerStore::ledger_run_events`). One open at `at` reads up to `at`;
//!   one ended keeps the through its row has (`inputs_through.events`):
//!   the greatest id timed by its end may be past `at`, and finding the one
//!   by `at` takes a scan of the events after its end. The events after
//!   `at` read again set it as the fold of every event does, so once they
//!   are all read the row is that fold's.
//!
//! The supervisors' lives at `at` ([`lives_at`]) are of the last event
//! naming each supervisor by `at` (`LedgerStore::last_event_naming_supervisor`)
//! and the stops by `at`.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};

use serde_json::json;

use super::QueueHead;
use crate::domain::{
    EventId, RunEvent, RunId, claim_hold,
    ledger::findings,
    live_alerts::StreamRecord,
    marks::utc_text,
    pre_claim::{ENDED_FREED, PreClaimInterval},
    stats::{
        landing_utilization::{Attempt, Attempts},
        timestamp_millis,
    },
    supervisor_life::{LifeEnd, Lives, supervisor_of},
};

const LANDING_SLOT: &str = "landing_slot";
const STREAM: &str = "live_alert_stream";
const SLOTS_FULL: &str = "slots_full";

/// What cutting the queue rows of some heads to a position reads besides
/// the heads: each bounded by a row, a run or a supervisor, none by the
/// history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Needs {
    /// The keys of the rows whose records are read with their bodies (a
    /// finding's or a stream's with a record past the position).
    pub rows: Vec<String>,
    /// The runs whose events are read (an attempt ended by the heads whose
    /// end the next slot does not tell by the position).
    pub runs: Vec<String>,
    /// The supervisors whose last event by the position is read: those
    /// whose lives reach a row at the position whose end their lives do
    /// not tell yet (a slots full or a stream open), and of a row past the
    /// position a slots full not freed, a stream not replaced or a hold a
    /// stop ended by then.
    pub supervisors: BTreeSet<String>,
}

impl Needs {
    /// What cutting `heads` to `at` reads ([`Cut::heads`]).
    pub fn of(heads: &[QueueHead], at: EventId) -> Self {
        let mut needs = Self::default();
        let slots = slot_keys(heads);
        for head in heads {
            let Some(key) = key_of(head) else {
                continue;
            };
            if key > at {
                continue;
            }
            let through = head.head.inputs_through.events;
            let past = past(head, at);
            let records_past = head.head.records.iter().any(|id| *id > at);
            match head.subject_kind.as_str() {
                LANDING_SLOT if past && head.ended_at.is_some() => {
                    if next_slot(&slots, key).is_none_or(|next| next > at) {
                        needs.runs.push(head.subject_id.clone());
                    }
                }
                STREAM => {
                    let replaced = head.outcome.as_deref() == Some("replaced") && through <= at;
                    // Open at the position, its life reaches it; one of
                    // nothing past it whose life ended tells its end.
                    let reached = if past {
                        !replaced
                    } else {
                        head.outcome.is_none()
                    };
                    if reached {
                        needs.supervisors.insert(head.subject_id.clone());
                    }
                    if records_past && !replaced {
                        needs.rows.push(head.head.key.clone());
                    }
                }
                "finding" if records_past => needs.rows.push(head.head.key.clone()),
                kind if is_wait(kind) => {
                    let Some(interval) = interval_of(head) else {
                        continue;
                    };
                    let end = interval.end.as_deref();
                    let reached = if !past {
                        kind == SLOTS_FULL && end.is_none()
                    } else if kind == SLOTS_FULL {
                        end != Some(ENDED_FREED) || through > at
                    } else {
                        end == Some(claim_hold::ENDED_SUPERVISOR_GONE) && through <= at
                    };
                    if reached && let Some(supervisor) = interval.supervisor {
                        needs.supervisors.insert(supervisor);
                    }
                }
                _ => {}
            }
        }
        needs
    }
}

/// What [`Needs`] named, read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reads {
    /// The records of each row by its key, with their bodies.
    pub records: HashMap<String, Vec<RunEvent>>,
    /// The events of each run by its id.
    pub runs: HashMap<String, Vec<RunEvent>>,
}

/// What the queue rows are cut to a position `at` against: the time the
/// rows were folded at (`now_ms`, the time a life ends a row by), the
/// supervisors' lives at `at` ([`lives_at`]) and what [`Needs`] read.
#[derive(Debug, Clone, Copy)]
pub struct Cut<'a> {
    pub at: EventId,
    pub now_ms: i64,
    pub lives: &'a Lives,
    pub reads: &'a Reads,
}

/// The supervisors' lives at `at`: of each event of `last` (the last event
/// naming a supervisor by `at`), its supervisor's life up to there, its
/// time the latest of that life (the clocks are not set back), and the
/// stops of `stops` by `at`.
pub fn lives_at(last: &[RunEvent], stops: &[RunEvent], at: EventId) -> Lives {
    let mut lives = Lives::default();
    for event in last.iter().filter(|event| event.id <= at) {
        if let Some(supervisor) = supervisor_of(event) {
            lives.go_on(supervisor, event.id, timestamp_millis(&event.created_at));
        }
    }
    let mut stops: Vec<&RunEvent> = stops.iter().filter(|stop| stop.id <= at).collect();
    stops.sort_by_key(|stop| stop.id);
    for stop in stops {
        lives.add(stop);
    }
    lives
}

/// Whether something of the row of `head` is past `at`: a record, or the
/// last input it read.
pub fn past(head: &QueueHead, at: EventId) -> bool {
    head.head.inputs_through.last() > at || head.head.records.iter().any(|id| *id > at)
}

fn key_of(head: &QueueHead) -> Option<EventId> {
    head.head.key.parse().ok().map(EventId::new)
}

fn is_wait(kind: &str) -> bool {
    matches!(
        kind,
        "slots_full" | "claim_hold" | "claim_deferral" | "landing_hold"
    )
}

fn interval_of(head: &QueueHead) -> Option<PreClaimInterval> {
    serde_json::from_value(head.detail.get("interval")?.clone()).ok()
}

/// The keys of the landing slot's rows of `heads`, ascending.
fn slot_keys(heads: &[QueueHead]) -> Vec<EventId> {
    let mut keys: Vec<EventId> = heads
        .iter()
        .filter(|head| head.subject_kind == LANDING_SLOT)
        .filter_map(key_of)
        .collect();
    keys.sort_unstable();
    keys
}

fn next_slot(slots: &[EventId], key: EventId) -> Option<EventId> {
    slots
        .get(slots.partition_point(|slot| *slot <= key))
        .copied()
}

impl Cut<'_> {
    /// The rows of `heads` as they were at the position: those that start
    /// after it are none, those of nothing past it as written, the rest cut
    /// by their kind (the module's doc), settled none of them.
    pub fn heads(&self, heads: &[QueueHead]) -> Vec<QueueHead> {
        self.heads_held(heads)
            .into_iter()
            .map(Cow::into_owned)
            .collect()
    }

    /// [`Self::heads`], those of nothing past it borrowed: a start holds
    /// the heads it read, and no second copy of what each holds on the heap.
    pub(super) fn heads_held<'h>(&self, heads: &'h [QueueHead]) -> Vec<Cow<'h, QueueHead>> {
        let slots = slot_keys(heads);
        heads
            .iter()
            .filter_map(|head| self.head(head, &slots))
            .collect()
    }

    /// The row of `head` at the position, `slots` the keys of the landing
    /// slot's rows; none for one that starts after it.
    fn head<'h>(&self, head: &'h QueueHead, slots: &[EventId]) -> Option<Cow<'h, QueueHead>> {
        let Some(key) = key_of(head) else {
            return Some(Cow::Borrowed(head));
        };
        if key > self.at {
            return None;
        }
        if !past(head, self.at) {
            return Some(Cow::Borrowed(head));
        }
        let mut cut = head.clone();
        cut.head.is_final = false;
        cut.head.records.retain(|id| *id <= self.at);
        match head.subject_kind.as_str() {
            LANDING_SLOT => self.landing_slot(head, key, next_slot(slots, key), &mut cut),
            STREAM => self.stream(head, key, &mut cut),
            "live_alert" => {
                if head.head.records.iter().any(|id| *id > self.at) {
                    open(&mut cut, key);
                }
            }
            "finding" => self.finding(head, &mut cut),
            kind if is_wait(kind) => self.wait(head, key, kind == SLOTS_FULL, &mut cut),
            _ => {}
        }
        Some(Cow::Owned(cut))
    }

    /// A wait or a hold: as written when an event of its own ended it by
    /// the position (a hold a stop ended reading its supervisor's life up
    /// to there), else open there; a slots full not freed reads up to its
    /// supervisor's last event and is ended by that life.
    fn wait(&self, head: &QueueHead, key: EventId, slots_full: bool, cut: &mut QueueHead) {
        let Some(mut interval) = interval_of(head) else {
            return;
        };
        let gone = interval.end.as_deref() == Some(claim_hold::ENDED_SUPERVISOR_GONE);
        let ended_by_event = interval.end.is_some() && !(slots_full && gone);
        if ended_by_event && head.head.inputs_through.events <= self.at {
            if gone {
                cut.head.inputs_through.supervisor = interval
                    .supervisor
                    .as_deref()
                    .and_then(|supervisor| self.lives.last_of(supervisor));
            }
            return;
        }
        open(cut, key);
        interval.ended_at = None;
        interval.end = None;
        interval.ended_by = None;
        if slots_full && let Some(supervisor) = interval.supervisor.clone() {
            let last = self.lives.last_of(&supervisor);
            cut.head.inputs_through.events = last.unwrap_or(key).max(key);
            if let Some(end) = self.lives.end(&supervisor, self.now_ms) {
                let started = timestamp_millis(&interval.started_at).unwrap_or(end.at_ms());
                let ended = end.at_ms().max(started);
                interval.ended_at = Some(utc_text(ended));
                interval.end = Some(claim_hold::ENDED_SUPERVISOR_GONE.to_owned());
                interval.ended_by = match end {
                    LifeEnd::Stopped { .. } => interval.supervisor.clone(),
                    LifeEnd::Silent { .. } => None,
                };
                cut.ended_at = Some(ended);
                cut.outcome = Some(claim_hold::ENDED_SUPERVISOR_GONE.to_owned());
                cut.head.inputs_through.supervisor = last;
            }
        }
        cut.detail = json!({ "interval": interval });
    }

    /// A stream: one its next baseline replaced by the position is as
    /// written; another is open there, its records and version cut there,
    /// reading up to its supervisor's last event and ended by that life.
    fn stream(&self, head: &QueueHead, key: EventId, cut: &mut QueueHead) {
        let through = head.head.inputs_through.events;
        if head.outcome.as_deref() == Some("replaced") && through <= self.at {
            return;
        }
        if head.head.records.iter().any(|id| *id > self.at)
            && let Some(records) = self.reads.records.get(&head.head.key)
        {
            let mut detail =
                json!({ "baseline": head.detail["baseline"].as_bool().unwrap_or(false) });
            if let Some(version) = records
                .iter()
                .filter(|event| event.id <= self.at)
                .rev()
                .find_map(|event| StreamRecord::of(event).and_then(|record| record.version))
            {
                detail["version"] = json!(version);
            }
            cut.detail = detail;
        }
        let supervisor = head.subject_id.as_str();
        let last = cut.head.records.last().copied().unwrap_or(key);
        let life = self.lives.last_of(supervisor);
        cut.head.inputs_through.events = last.max(life.unwrap_or(last));
        cut.head.inputs_through.supervisor = None;
        cut.ended_at = None;
        cut.outcome = None;
        if let Some(end) = self.lives.end(supervisor, self.now_ms) {
            let started = head.started_at.unwrap_or(end.at_ms());
            cut.ended_at = Some(end.at_ms().max(started));
            cut.outcome = Some(claim_hold::ENDED_SUPERVISOR_GONE.to_owned());
            cut.head.inputs_through.supervisor = life;
        }
    }

    /// A finding with a record past the position: its values are those of
    /// its last record by then.
    fn finding(&self, head: &QueueHead, cut: &mut QueueHead) {
        let Some(records) = self.reads.records.get(&head.head.key) else {
            return;
        };
        let records: Vec<&RunEvent> = records.iter().filter(|event| event.id <= self.at).collect();
        let Some(last) = records.last() else {
            return;
        };
        let values = findings::values_of(last);
        cut.outcome = values.as_ref().map(|v| v.status.as_str().to_owned());
        cut.ended_at = values
            .as_ref()
            .filter(|v| !v.status.is_unsettled())
            .and_then(|_| timestamp_millis(&last.created_at));
        cut.detail = last
            .payload
            .get(findings::VALUES)
            .map_or(json!({}), |values| json!({ "finding": values }));
        cut.task_id = records.iter().find_map(|event| event.task_id);
        cut.head.inputs_through.events = last.id;
        cut.head.records = records.iter().map(|event| event.id).collect();
    }

    /// A landing slot's attempt: open at the position unless the next
    /// slot started by then or its run's events by then end it; one ended
    /// keeps its row's through.
    fn landing_slot(
        &self,
        head: &QueueHead,
        key: EventId,
        next: Option<EventId>,
        cut: &mut QueueHead,
    ) {
        let ended = head.ended_at.is_some()
            && (next.is_some_and(|next| next <= self.at)
                || head.head.inputs_through.events <= self.at
                || self.run_ends(head, key));
        if !ended {
            cut.ended_at = None;
            cut.outcome = None;
            cut.head.inputs_through.events = self.at;
        }
    }

    /// Whether the run of the attempt `key` began ended it by the
    /// position, by its events read.
    fn run_ends(&self, head: &QueueHead, key: EventId) -> bool {
        let Some(events) = self.reads.runs.get(&head.subject_id) else {
            return false;
        };
        let Ok(run) = RunId::new(head.subject_id.clone()) else {
            return false;
        };
        let start = head.started_at.unwrap_or_default();
        let mut attempts = Attempts::from_parts(
            vec![Attempt {
                task_id: head.task_id,
                start,
                end: start,
                landed: false,
                open: true,
            }],
            Some((run, 0)),
        );
        events
            .iter()
            .filter(|event| event.id > key && event.id <= self.at)
            .any(|event| {
                let (Some(run), Some(at)) = (&event.run_id, timestamp_millis(&event.created_at))
                else {
                    return false;
                };
                attempts.add(event, run, at).ended.is_some()
            })
    }
}

/// Open the row `cut` started at `key`: no end, its start its only record
/// and its last input.
fn open(cut: &mut QueueHead, key: EventId) {
    cut.ended_at = None;
    cut.outcome = None;
    cut.head.records.retain(|id| *id == key);
    cut.head.inputs_through.events = key;
    cut.head.inputs_through.supervisor = None;
}

#[cfg(test)]
mod tests;
