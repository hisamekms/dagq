//! The views of the queue rows a read from a time looks up instead of
//! walking the settled history (docs/plans/ledger-window-earlier-visibility-spike.md,
//! "追試 (ii) の残り（Spike 4811）" "判定"): the tasks each forecast's
//! snapshot names, main's commits by their sha and the commit each reverts,
//! the summaries of main's record up to an event, and the run-less asks no
//! answer closed.
//!
//! Each is made of a queue row's records alone, but for an ask (whether a
//! row of the ledger answers it) and a summary (the summary before it), and
//! the store writes them with the row in its transaction each time the row
//! is written. They are views of the events (ADR-t1662-2 decisions 1, 7 and
//! 8): rebuilt of the rows alone, they change no row and no `ledger_v`. The
//! store's mark of them is kept as that of [`super::open`]: through the
//! generations of the write leader whose writer kept the views, so that an
//! older binary that wrote a row without them leaves the mark behind and a
//! reader reads the whole pages again until the filling took every row in.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::Value;

use serde::{Deserialize, Serialize};

use super::open::Left;
use super::post_landing::branch_name;
use super::{LedgerKind, LedgerRow};
use crate::domain::run::{AskRef, restore_payload};
use crate::domain::stats::main_log::{MAIN_LOG_KINDS, MainReading, Reach, reverts_of};
use crate::domain::stats::timestamp_millis;
use crate::domain::{EventId, EventKind, RunEvent};

/// The kinds of the queue rows the views are made of, one event each.
pub const VIEW_KINDS: [EventKind; 11] = [
    EventKind::ForecastRecorded,
    EventKind::MainCommitsRecorded,
    EventKind::MainRevertsImported,
    EventKind::MainPathsRecorded,
    EventKind::MainObserved,
    EventKind::MainReadFailed,
    EventKind::MainReadRecovered,
    EventKind::MainRewritten,
    EventKind::CiChecked,
    EventKind::AskOpened,
    EventKind::AskAnswered,
];

/// The events of main's record and of CI's a summary folds: those
/// [`crate::domain::stats::main_log::fold_main_log`] reads and the checks
/// of CI ([`super::post_landing`] reads per branch).
pub fn summarized(event: &RunEvent) -> bool {
    event.kind == EventKind::CiChecked || MAIN_LOG_KINDS.iter().any(|kind| event.kind == *kind)
}

/// Whether `row` is a queue row the views are made of.
pub fn takes(row: &LedgerRow) -> bool {
    row.kind == LedgerKind::Queue
        && row
            .subject
            .as_ref()
            .is_some_and(|subject| VIEW_KINDS.iter().any(|kind| subject.kind == kind.as_str()))
}

/// The tasks a `forecast_recorded` names (its `tasks`' ids), none of any
/// other event.
pub fn snapshot_targets(event: &RunEvent) -> BTreeSet<i64> {
    if event.kind != EventKind::ForecastRecorded {
        return BTreeSet::new();
    }
    event.payload["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|task| task["id"].as_i64())
        .collect()
}

/// One commit a `main_commits_recorded` records: its sha and the commit
/// its message says it reverts (none for a commit that reverts none, or
/// recorded before the record carried it).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Commit {
    pub sha: String,
    pub reverts: Option<String>,
}

/// The commits a `main_commits_recorded` records, each once, none of any
/// other event.
pub fn main_commits(event: &RunEvent) -> BTreeSet<Commit> {
    if event.kind != EventKind::MainCommitsRecorded {
        return BTreeSet::new();
    }
    event.payload["commits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|commit| {
            Some(Commit {
                sha: commit["sha"].as_str()?.to_owned(),
                reverts: reverts_of(commit).target().map(str::to_owned),
            })
        })
        .collect()
}

/// Which side of a run-less ask an event is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    Opened(Option<String>),
    Answered(Option<String>),
}

/// The side of a run-less ask `event` is, by [`AskRef::key`]; `None` of
/// another event and of an ask of a run (its run row's, [`super::open`]).
pub fn ask(event: &RunEvent) -> Option<Ask> {
    if event.run_id.is_some() {
        return None;
    }
    let key = || restore_payload::<AskRef>(&event.payload).key();
    match event.kind.known() {
        Some(EventKind::AskOpened) => Some(Ask::Opened(key())),
        Some(EventKind::AskAnswered) => Some(Ask::Answered(key())),
        _ => None,
    }
}

/// The run-less asks `rows` leave with no answer, by the key of the queue
/// row that opens each, as the whole read of the queue rows pairs them by
/// [`AskRef::key`] whichever comes first: what the store's table of what
/// is left open holds of the queue rows.
pub fn asks_left_open(rows: &[LedgerRow]) -> BTreeMap<String, BTreeSet<Left>> {
    let records = || {
        rows.iter()
            .filter(|row| row.kind == LedgerKind::Queue)
            .flat_map(|row| row.records.iter().map(move |event| (row, event)))
    };
    let answered: HashSet<Option<String>> = records()
        .filter_map(|(_, event)| match ask(event)? {
            Ask::Answered(key) => Some(key),
            Ask::Opened(_) => None,
        })
        .collect();
    let mut left: BTreeMap<String, BTreeSet<Left>> = BTreeMap::new();
    for (row, event) in records() {
        if let Some(Ask::Opened(key)) = ask(event)
            && !answered.contains(&key)
        {
            left.entry(row.key.clone())
                .or_default()
                .insert(Left::Ask(key));
        }
    }
    left
}

/// The events of main's record and CI's checks between two summaries the
/// store keeps, at most: a reader reads no more of them past the latest
/// summary at its time or before.
pub const SUMMARY_EVERY: u32 = 256;

/// A base of main's paths in parts, not all of them recorded yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Base {
    pub parts: u64,
    pub seen: BTreeSet<u64>,
    pub paths: BTreeSet<String>,
}

/// CI's earliest and latest check of a branch by the time it was
/// written, whatever the order of their ids: where CI's record of the
/// branch begins and how far it reaches, as [`super::post_landing::fold`]
/// takes them (the least and the greatest time of its checks).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checks {
    pub earliest: RunEvent,
    pub latest: RunEvent,
}

impl Checks {
    /// The checks after `event`, a check of the branch written `at` (unix
    /// milliseconds): the earlier of two at the same time stays earliest,
    /// the later in id becomes the latest.
    fn and(&mut self, event: &RunEvent, at: i64) {
        let time = |event: &RunEvent| timestamp_millis(&event.created_at).unwrap_or(at);
        if at < time(&self.earliest) {
            self.earliest = event.clone();
        }
        if at >= time(&self.latest) {
            self.latest = event.clone();
        }
    }
}

/// What main's record and CI's checks up to an event leave: the values a
/// reader of a window from a time takes of what is before it
/// ([`crate::domain::stats::main_log::MainLog`]'s start, reach, head,
/// reading and paths, the last commit recorded, CI's earliest and latest
/// check of each branch by time), each the value the fold of every event up to it gives.
/// The commits' own changes are read by their sha.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MainSummary {
    /// The last event folded, and its time (unix milliseconds).
    pub through: Option<EventId>,
    pub at_ms: Option<i64>,
    /// The events folded since the summary the store keeps before it.
    pub folded: u32,
    pub since: Option<i64>,
    pub head: Option<String>,
    pub recorded_through: Option<i64>,
    /// Main's first-parent history since the record's start, by sha,
    /// oldest first, as the fold keeps it: a commit recorded again counts
    /// once, and a rewrite cuts it back to the merge base.
    pub commits: Vec<String>,
    /// The paths main has, `None` while they are not known.
    pub paths: Option<BTreeSet<String>>,
    pub bases: BTreeMap<String, Base>,
    /// The last `main_read_failed` or `main_read_recovered`.
    pub reading: Option<RunEvent>,
    /// CI's checks by branch ([`branch_name`]).
    pub ci: BTreeMap<String, Checks>,
}

impl MainSummary {
    /// Whether the store keeps it when the next is written, rather than
    /// writing the next in its place.
    pub const fn kept(&self) -> bool {
        self.folded >= SUMMARY_EVERY
    }

    /// Whether Git can be read, as the last change recorded says.
    pub fn reading(&self) -> MainReading {
        MainReading::of(self.reading.as_ref())
    }

    /// The summary after `event` ([`summarized`]; another changes nothing
    /// but where it is), the next after this one, as
    /// [`crate::domain::stats::main_log::fold_main_log`] folds it:
    /// `changes` gives the changes of a commit of [`Self::commits`] by its
    /// sha (those its `main_commits_recorded` records), which a base whose
    /// paths complete applies past its head.
    #[must_use]
    pub fn then(&self, event: &RunEvent, changes: &dyn Fn(&str) -> Option<Value>) -> Self {
        let mut next = self.clone();
        next.through = Some(event.id);
        next.at_ms = timestamp_millis(&event.created_at);
        next.folded = if self.through.is_none() || self.kept() {
            1
        } else {
            self.folded + 1
        };
        let payload = &event.payload;
        match event.kind.known() {
            Some(EventKind::MainCommitsRecorded) => {
                for commit in payload["commits"].as_array().into_iter().flatten() {
                    let (Some(sha), Some(_), Some(_)) = (
                        commit["sha"].as_str(),
                        commit["at"].as_i64(),
                        commit["changes"].as_array(),
                    ) else {
                        continue;
                    };
                    if next.commits.iter().any(|known| known == sha) {
                        continue;
                    }
                    if let Some(paths) = &mut next.paths {
                        apply(paths, &commit["changes"]);
                    }
                    next.commits.push(sha.to_owned());
                }
            }
            Some(EventKind::MainPathsRecorded) => {
                let (Some(head), Some(part), Some(parts)) = (
                    payload["head"].as_str(),
                    payload["part"].as_u64(),
                    payload["parts"].as_u64(),
                ) else {
                    return next;
                };
                let base = next.bases.entry(head.to_owned()).or_insert_with(|| Base {
                    parts,
                    ..Base::default()
                });
                base.seen.insert(part);
                base.paths.extend(
                    payload["paths"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|path| path.as_str())
                        .map(str::to_owned),
                );
                if base.seen.len() as u64 >= base.parts {
                    let mut complete = next.bases.remove(head).unwrap_or_default().paths;
                    // The commits already folded past the base's head
                    // (another supervisor's, read later) apply to it.
                    if let Some(at) = next.commits.iter().position(|sha| sha == head) {
                        for sha in &next.commits[at + 1..] {
                            if let Some(changes) = changes(sha) {
                                apply(&mut complete, &changes);
                            }
                        }
                    }
                    next.paths = Some(complete);
                    if next.since.is_none() {
                        next.since = payload["since"].as_i64();
                    }
                }
            }
            Some(EventKind::MainRewritten) => {
                let keep = payload["base"]
                    .as_str()
                    .and_then(|base| next.commits.iter().position(|sha| sha == base))
                    .map_or(0, |at| at + 1);
                next.commits.truncate(keep);
                next.paths = None;
            }
            Some(EventKind::MainObserved) => {
                if let Some(reach) = Reach::of(event) {
                    next.head = Some(reach.head);
                    next.recorded_through = Some(reach.at);
                }
            }
            Some(EventKind::MainReadFailed | EventKind::MainReadRecovered) => {
                next.reading = Some(event.clone());
            }
            Some(EventKind::CiChecked) => {
                if let (Some(at), Some(branch)) = (
                    timestamp_millis(&event.created_at),
                    payload["branch"].as_str(),
                ) {
                    next.ci
                        .entry(branch_name(branch))
                        .and_modify(|checks| checks.and(event, at))
                        .or_insert_with(|| Checks {
                            earliest: event.clone(),
                            latest: event.clone(),
                        });
                }
            }
            _ => {}
        }
        next
    }

    /// The last commit of main's history recorded.
    pub fn last_commit(&self) -> Option<&str> {
        self.commits.last().map(String::as_str)
    }
}

/// `paths` after a recorded commit's `changes`, as the fold applies them.
fn apply(paths: &mut BTreeSet<String>, changes: &Value) {
    for change in changes.as_array().into_iter().flatten() {
        let Some(path) = change["path"].as_str() else {
            continue;
        };
        if let Some(from) = change["from"].as_str() {
            paths.remove(from);
        }
        if change["deleted"].as_bool() == Some(true) {
            paths.remove(path);
        } else {
            paths.insert(path.to_owned());
        }
    }
}

/// The summaries the store keeps of `events` (ascending id; those not
/// [`summarized`] skipped), each event folded on the one before it: the
/// kept ones and the last. What the store's writes and its filling leave
/// of the same events.
pub fn summaries(events: &[RunEvent]) -> Vec<MainSummary> {
    let changes = commit_changes(events);
    let mut kept: Vec<MainSummary> = Vec::new();
    for event in events.iter().filter(|event| summarized(event)) {
        let last = kept.last().cloned().unwrap_or_default();
        let next = last.then(event, &|sha| changes.get(sha).cloned());
        if last.through.is_some() && !last.kept() {
            kept.pop();
        }
        kept.push(next);
    }
    kept
}

/// The changes each commit of `events`' `main_commits_recorded` records,
/// by its sha (the last record of it).
pub fn commit_changes(events: &[RunEvent]) -> HashMap<String, Value> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::MainCommitsRecorded)
        .flat_map(|event| event.payload["commits"].as_array().into_iter().flatten())
        .filter_map(|commit| {
            Some((
                commit["sha"].as_str()?.to_owned(),
                commit["changes"].clone(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests;
