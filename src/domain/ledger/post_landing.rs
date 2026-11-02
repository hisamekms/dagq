//! What befell a landed run after its landing (ADR-t3957-1 decision 5,
//! docs/design/measurement.md "共通の読み取り関数"): the reverts of its
//! landed commit and the turns to red of the landing branch's CI on it,
//! read from the ledger's rows alone, as the common read of a window gives
//! them ([`super::window::LedgerWindow::post_landing`]): every row's
//! records up to the window's end, whatever its filters, for the runs the
//! window chose.
//!
//! Both arrive after the run's row is final, so they are never folded into
//! it: each is one event of the queue as a whole and so the record of a
//! queue row of its own ([`super::queue`]): a revert in the commits of a
//! `main_commits_recorded` (or of a `main_reverts_imported`, for those
//! recorded before the record carried the evidence), a failure in a
//! `ci_turned_red`. [`fold`] matches them to the runs by the commit the run
//! landed, read from the run row's `run_integrated` (`result_commit`, or
//! `commit` of a payload without it); the commit before the landing
//! (`main_before`) is never a run's. A revert or a failure is counted once
//! by its commit (a revert) or its event (a failure), however many rows
//! hold it (a record again after a rewrite, an import of what a commit's
//! own record already carried). A failure falls on a run only when CI
//! watched the branch the run landed on (the `branch` of its push), and
//! CI's record of a run is the checks of that branch alone: a run before
//! its push has none.
//!
//! What arrives late is read as of the window's end, with how the record
//! stands to it ([`Counted`]): how long before the window's end the run
//! landed and the record last reached, so that a reader tells a settled
//! count from one still open, and a later window reads what arrived after.
//! A run that landed before its record began is
//! [`Coverage::BeforeRecord`]: a revert or failure before the record began
//! is not in it. One that landed before an import that could not read a
//! line Git no longer had (`unread`) is [`Coverage::Incomplete`]: a revert
//! on that line is unknown.
//!
//! Limits: a revert a force push later took off main still counts, and a
//! revert of a revert names the first revert's commit, so it falls on no
//! run ([`NOT_A_LANDING`]) and the run stays reverted.
//!
//! Reads nothing but the rows' records it is given: no Git, StateStore or
//! run dir.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;
use serde_json::Value;

use crate::domain::{
    EventKind, RunEvent, RunId, TaskId,
    stats::{main_log::reverts_of, revert::RevertOf, timestamp_millis},
};

/// Why a revert or a failure names no run: the commit it names is on the
/// record of main but no run of the rows landed it (a person's push, a
/// landing outside the rows read).
pub const NOT_A_LANDING: &str = "not_a_landing";
/// Why a revert names no run: the commit it reverts is not on the record
/// of main (recorded before the record began).
pub const BEFORE_RECORD: &str = "before_record";

/// The run a revert or a failure falls on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Target {
    pub run_id: RunId,
    pub task_id: Option<TaskId>,
}

/// One revert or one failure, matched to the run that landed the commit
/// it names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Fact {
    /// A revert's own commit; the commit a failure's CI ran on.
    pub sha: String,
    /// The commit it names: the one a revert reverts, the one CI ran on.
    pub names: String,
    /// When (unix milliseconds): a revert's commit time, a failure's
    /// record.
    pub at: i64,
    /// A failure's branch, the one CI watched; none for a revert.
    pub branch: Option<String>,
    /// The run that landed `names`; `None` with `reason` when none did.
    pub target: Option<Target>,
    pub reason: Option<&'static str>,
}

/// Whether a record speaks for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    /// The record ran from before the run's landing.
    Recorded,
    /// The run landed before the record began: what came before is not in
    /// it.
    BeforeRecord,
    /// The run landed before an import of the record that could not read
    /// a line Git no longer had: a revert on it is unknown.
    Incomplete,
    /// There is no record yet.
    NotRecorded,
    /// The run has not landed in the rows read.
    NotLanded,
}

/// The reverts or the failures of one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Counted {
    pub coverage: Coverage,
    pub count: usize,
    /// The earliest (unix milliseconds).
    pub first_at: Option<i64>,
    /// When the record last reached by the window's end (unix
    /// milliseconds): main's history up to its head recorded, CI's last
    /// check of the run's branch read.
    pub reach_at: Option<i64>,
    /// The window's end the values are read at (unix milliseconds).
    pub window_end: i64,
    /// From the run's landing to the window's end, in milliseconds: how
    /// long a revert or a failure had to arrive in the window.
    pub landed_before_end_ms: Option<i64>,
    /// From the record's reach to the window's end, in milliseconds: how
    /// long before the window's end the record stopped speaking (0 when it
    /// reached the end).
    pub reach_before_end_ms: Option<i64>,
}

/// What befell one run after its landing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PostLanding {
    pub run_id: RunId,
    /// Its landing (unix milliseconds) and the commit it landed; none
    /// before it landed.
    pub landed_at: Option<i64>,
    pub commit: Option<String>,
    pub reverts: Counted,
    pub ci_failures: Counted,
}

/// How far a record reaches: from when (unix milliseconds) it speaks, and
/// to when it was last read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Record {
    from: Option<i64>,
    reach: Option<i64>,
}

impl Record {
    fn begins(&mut self, at: i64) {
        self.from = Some(self.from.map_or(at, |from| from.min(at)));
    }

    fn reaches(&mut self, at: i64) {
        self.reach = Some(self.reach.map_or(at, |reach| reach.max(at)));
    }
}

/// A run's landing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Landing {
    commit: String,
    at: Option<i64>,
    /// The branch it landed on, as its push (`push_finished`,
    /// `push_skipped` or `push_failed`) names it; none before its push.
    branch: Option<String>,
}

/// The reverts and failures of the rows read, matched to their runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostLandingBook {
    /// Each revert once, by its commit, oldest first.
    pub reverts: Vec<Fact>,
    /// Each turn to red once, oldest first.
    pub ci_failures: Vec<Fact>,
    landings: HashMap<RunId, Landing>,
    reverts_record: Record,
    /// CI's record per branch it watched.
    ci_records: HashMap<String, Record>,
    /// The heads of the lines an import could not read, and the latest
    /// such import (unix milliseconds): a run landed before it is
    /// [`Coverage::Incomplete`].
    pub unread: Vec<String>,
    unread_until: Option<i64>,
    /// The window's end the events were read to (unix milliseconds).
    end_ms: i64,
}

/// A branch as one name: a full ref and its short name alike.
fn branch_name(branch: &str) -> String {
    branch
        .strip_prefix("refs/heads/")
        .unwrap_or(branch)
        .to_owned()
}

/// Fold `events` (the ledger rows' records up to a window's end at
/// `end_ms`, ascending id, each once) into the reverts and failures
/// matched to the runs that landed what they name.
pub fn fold(events: &[RunEvent], end_ms: i64) -> PostLandingBook {
    let mut book = PostLandingBook {
        end_ms,
        ..PostLandingBook::default()
    };
    let mut landed: HashMap<String, Target> = HashMap::new();
    let mut recorded: HashSet<String> = HashSet::new();
    let mut reverts: BTreeMap<String, (i64, String)> = BTreeMap::new();
    let mut failures: BTreeMap<i64, (i64, String, Option<String>)> = BTreeMap::new();
    let mut branches: HashMap<RunId, String> = HashMap::new();
    for event in events {
        match event.kind.known() {
            Some(EventKind::RunIntegrated) => {
                let (Some(commit), Some(run_id)) =
                    (landed_commit(&event.payload), event.run_id.clone())
                else {
                    continue;
                };
                landed.insert(
                    commit.clone(),
                    Target {
                        run_id: run_id.clone(),
                        task_id: event.task_id,
                    },
                );
                book.landings.insert(
                    run_id,
                    Landing {
                        commit,
                        at: timestamp_millis(&event.created_at),
                        branch: None,
                    },
                );
            }
            Some(EventKind::PushFinished | EventKind::PushSkipped | EventKind::PushFailed) => {
                if let (Some(run), Some(branch)) =
                    (event.run_id.clone(), event.payload["branch"].as_str())
                {
                    branches.insert(run, branch_name(branch));
                }
            }
            Some(EventKind::MainCommitsRecorded) => {
                for commit in commits(&event.payload) {
                    let Some(sha) = commit["sha"].as_str() else {
                        continue;
                    };
                    recorded.insert(sha.to_owned());
                    let reverts_of = reverts_of(commit);
                    let at = commit["at"].as_i64().unwrap_or_default() * 1000;
                    if reverts_of != RevertOf::Unrecorded {
                        book.reverts_record.begins(at);
                    }
                    if let Some(target) = reverts_of.target() {
                        reverts
                            .entry(sha.to_owned())
                            .or_insert((at, target.to_owned()));
                    }
                }
            }
            Some(EventKind::MainRevertsImported) => {
                if let Some(since) = event.payload["since"].as_i64() {
                    book.reverts_record.begins(since * 1000);
                }
                let unread: Vec<&str> = commits_of(&event.payload, "unread")
                    .filter_map(Value::as_str)
                    .collect();
                if !unread.is_empty() {
                    let at = event.payload["at"].as_i64().map(|at| at * 1000);
                    book.unread_until = book.unread_until.max(at);
                    for head in unread {
                        if !book.unread.iter().any(|known| known == head) {
                            book.unread.push(head.to_owned());
                        }
                    }
                }
                for revert in commits_of(&event.payload, "reverts") {
                    if let (Some(sha), Some(target)) =
                        (revert["sha"].as_str(), revert["reverts"].as_str())
                    {
                        recorded.insert(sha.to_owned());
                        let at = revert["at"].as_i64().unwrap_or_default() * 1000;
                        reverts
                            .entry(sha.to_owned())
                            .or_insert((at, target.to_owned()));
                    }
                }
            }
            Some(EventKind::MainObserved) if event.payload["reverts"].as_bool() == Some(true) => {
                // Every commit from its start carries the evidence.
                if let Some(since) = event.payload["since"].as_i64() {
                    book.reverts_record.begins(since * 1000);
                }
                if let Some(at) = event.payload["at"].as_i64() {
                    book.reverts_record.reaches(at * 1000);
                }
            }
            Some(EventKind::CiChecked) => {
                if let (Some(at), Some(branch)) = (
                    timestamp_millis(&event.created_at),
                    event.payload["branch"].as_str(),
                ) {
                    let record = book.ci_records.entry(branch_name(branch)).or_default();
                    record.begins(at);
                    record.reaches(at);
                }
            }
            Some(EventKind::CiTurnedRed) => {
                if let (Some(sha), Some(at)) = (
                    event.payload["sha"].as_str(),
                    timestamp_millis(&event.created_at),
                ) {
                    let branch = event.payload["branch"].as_str().map(branch_name);
                    failures.insert(event.id.as_i64(), (at, sha.to_owned(), branch));
                }
            }
            _ => {}
        }
    }
    for (run, branch) in branches {
        if let Some(landing) = book.landings.get_mut(&run) {
            landing.branch = Some(branch);
        }
    }
    let landings = &book.landings;
    // A failure falls on a run only on the branch the run landed on.
    let matched = |sha: String, names: String, at: i64, branch: Option<String>| {
        let revert = branch.is_none();
        let target = landed.get(&names).cloned().filter(|target| {
            revert
                || landings
                    .get(&target.run_id)
                    .and_then(|landing| landing.branch.as_ref())
                    == branch.as_ref()
        });
        let reason = match &target {
            Some(_) => None,
            None if revert && !recorded.contains(&names) => Some(BEFORE_RECORD),
            None => Some(NOT_A_LANDING),
        };
        Fact {
            sha,
            names,
            at,
            branch,
            target,
            reason,
        }
    };
    let reverts: Vec<Fact> = reverts
        .into_iter()
        .map(|(sha, (at, names))| matched(sha, names, at, None))
        .collect();
    let failures: Vec<Fact> = failures
        .into_values()
        .map(|(at, sha, branch)| matched(sha.clone(), sha, at, Some(branch.unwrap_or_default())))
        .collect();
    book.reverts = reverts;
    book.reverts
        .sort_by(|a, b| (a.at, &a.sha).cmp(&(b.at, &b.sha)));
    book.ci_failures = failures;
    book
}

/// The commit a `run_integrated` landed: `result_commit`, or `commit` of
/// an older payload.
fn landed_commit(payload: &Value) -> Option<String> {
    ["result_commit", "commit"]
        .iter()
        .find_map(|key| payload[*key].as_str().filter(|sha| !sha.is_empty()))
        .map(str::to_owned)
}

fn commits(payload: &Value) -> impl Iterator<Item = &Value> {
    commits_of(payload, "commits")
}

fn commits_of<'a>(payload: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    payload[key].as_array().into_iter().flatten()
}

impl PostLandingBook {
    /// What befell `run` after its landing.
    pub fn of_run(&self, run: &RunId) -> PostLanding {
        let landing = self.landings.get(run);
        let counted = |facts: &[Fact], record: Record, unread_until: Option<i64>| {
            let mine: Vec<&Fact> = facts
                .iter()
                .filter(|fact| fact.target.as_ref().is_some_and(|t| &t.run_id == run))
                .collect();
            let landed_at = landing.and_then(|landing| landing.at);
            let coverage = match (landing, record.from) {
                (None, _) => Coverage::NotLanded,
                (Some(_), None) => Coverage::NotRecorded,
                (Some(_), Some(_)) if landed_at.is_none() => Coverage::NotRecorded,
                (Some(_), Some(from)) if landed_at.is_some_and(|at| at < from) => {
                    Coverage::BeforeRecord
                }
                (Some(_), Some(_))
                    if unread_until
                        .zip(landed_at)
                        .is_some_and(|(until, landed)| landed < until) =>
                {
                    Coverage::Incomplete
                }
                (Some(_), Some(_)) => Coverage::Recorded,
            };
            let before_end = |at: i64| (self.end_ms - at).max(0);
            Counted {
                coverage,
                count: mine.len(),
                first_at: mine.iter().map(|fact| fact.at).min(),
                reach_at: record.reach,
                window_end: self.end_ms,
                landed_before_end_ms: landed_at.map(before_end),
                reach_before_end_ms: record.reach.map(before_end),
            }
        };
        PostLanding {
            run_id: run.clone(),
            landed_at: landing.and_then(|landing| landing.at),
            commit: landing.map(|landing| landing.commit.clone()),
            reverts: counted(&self.reverts, self.reverts_record, self.unread_until),
            ci_failures: counted(
                &self.ci_failures,
                landing
                    .and_then(|landing| landing.branch.as_ref())
                    .and_then(|branch| self.ci_records.get(branch))
                    .copied()
                    .unwrap_or_default(),
                None,
            ),
        }
    }
}

#[cfg(test)]
mod tests;
