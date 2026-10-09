//! The record of main's first-parent history that the supervisor keeps in
//! the events (docs/design/main-history.md), and the fold that
//! gives back from those events alone what `conflict_hotspots` and the
//! areas read from Git: the commits with the paths each changed, the paths
//! main has, the head last observed and how far the record reaches.
//!
//! Three times are kept apart: a commit's time (its committer's, part of
//! the history), the head observed (the last head of main the supervisor
//! read) and the reach (`recorded_through`: when the supervisor last read
//! Git and found the history up to that head recorded, which moves while
//! the head does not).
use std::collections::{BTreeSet, HashMap, HashSet};

use serde_json::{Value, json};

use super::{
    conflicts::{History, MainChange, MainCommit, MainHistory, window_span},
    timestamp_millis,
};
use crate::domain::{
    EventId, RunEvent, event_kind::EventKind, marks::utc_text, supervisor_life::supervisor_life_end,
};

/// How often a supervisor that reads main fine records its reach while the
/// head does not move: far shorter than [`MAIN_RECORD_GRACE_SECS`], and
/// rare enough to keep the records small.
pub const MAIN_OBSERVED_INTERVAL_SECS: i64 = 600;
/// How long after the reach a window may end and still be read from the
/// record: three reaches missed.
pub const MAIN_RECORD_GRACE_SECS: i64 = 3 * MAIN_OBSERVED_INTERVAL_SECS;
/// The most commits one `main_commits_recorded` holds.
pub const COMMITS_PER_EVENT: usize = 100;
/// The most changes one `main_commits_recorded` holds; a commit with more
/// is never split and has an event of its own.
pub const CHANGES_PER_EVENT: usize = 2000;
/// The most paths one `main_paths_recorded` holds.
pub const PATHS_PER_EVENT: usize = 1000;

/// The commits main's first-parent line gained after `after` (the commit
/// the record had reached, `None` for the first), oldest first.
pub const MAIN_COMMITS_RECORDED: &str = EventKind::MainCommitsRecorded.as_str();
/// One part of the paths main has at `head`: the base the later commits'
/// changes are applied to. `since` is the unix second the record of the
/// history starts at.
pub const MAIN_PATHS_RECORDED: &str = EventKind::MainPathsRecorded.as_str();
/// The reach: the history up to `head` is recorded, as read `at`.
pub const MAIN_OBSERVED: &str = EventKind::MainObserved.as_str();
/// Git could not be read from `at`, for `reason`.
pub const MAIN_READ_FAILED: &str = EventKind::MainReadFailed.as_str();
/// Git could be read again `at`, ending the failure that began `since`.
pub const MAIN_READ_RECOVERED: &str = EventKind::MainReadRecovered.as_str();
/// The commit the record had reached (`from`) is no longer on main: the
/// record goes on from `base`, their merge base (`None`: none), to `head`.
pub const MAIN_REWRITTEN: &str = EventKind::MainRewritten.as_str();

/// The kinds of the record, which [`fold_main_log`] reads.
pub const MAIN_LOG_KINDS: [&str; 6] = [
    MAIN_COMMITS_RECORDED,
    MAIN_PATHS_RECORDED,
    MAIN_OBSERVED,
    MAIN_READ_FAILED,
    MAIN_READ_RECOVERED,
    MAIN_REWRITTEN,
];

/// The payloads of `main_commits_recorded` for `commits` after `after`,
/// each holding at most [`COMMITS_PER_EVENT`] commits and, but for a
/// single larger commit, [`CHANGES_PER_EVENT`] changes; each names the
/// commit it continues from.
pub fn commits_payloads(after: Option<&str>, commits: &[MainCommit]) -> Vec<Value> {
    let mut payloads = Vec::new();
    let mut after = after.map(str::to_owned);
    let mut start = 0;
    while start < commits.len() {
        let mut end = start;
        let mut changes = 0;
        while end < commits.len()
            && end - start < COMMITS_PER_EVENT
            && (end == start || changes + commits[end].changes.len() <= CHANGES_PER_EVENT)
        {
            changes += commits[end].changes.len();
            end += 1;
        }
        let part = &commits[start..end];
        payloads.push(json!({
            "after": after,
            "commits": part.iter().map(commit_json).collect::<Vec<_>>(),
        }));
        after = part.last().map(|commit| commit.sha.clone());
        start = end;
    }
    payloads
}

fn commit_json(commit: &MainCommit) -> Value {
    let changes: Vec<Value> = commit
        .changes
        .iter()
        .map(|change| {
            let mut value = json!({"path": change.path});
            if let Some(from) = &change.from {
                value["from"] = json!(from);
            }
            if change.deleted {
                value["deleted"] = json!(true);
            }
            value
        })
        .collect();
    json!({"sha": commit.sha, "at": commit.at, "changes": changes})
}

fn commit_of(value: &Value) -> Option<MainCommit> {
    let changes = value
        .get("changes")?
        .as_array()?
        .iter()
        .filter_map(|change| {
            Some(MainChange {
                path: change.get("path")?.as_str()?.to_owned(),
                from: change
                    .get("from")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                deleted: change.get("deleted").and_then(Value::as_bool) == Some(true),
            })
        })
        .collect();
    Some(MainCommit {
        sha: value.get("sha")?.as_str()?.to_owned(),
        at: value.get("at")?.as_i64()?,
        changes,
    })
}

/// The payloads of `main_paths_recorded` for `paths`, main's at `head`, in
/// parts of at most [`PATHS_PER_EVENT`] (one, empty, for no path).
pub fn paths_payloads(head: &str, since: i64, paths: &[String]) -> Vec<Value> {
    let sorted: BTreeSet<&str> = paths.iter().map(String::as_str).collect();
    let sorted: Vec<&str> = sorted.into_iter().collect();
    let parts: Vec<&[&str]> = if sorted.is_empty() {
        vec![&[]]
    } else {
        sorted.chunks(PATHS_PER_EVENT).collect()
    };
    let count = parts.len();
    parts
        .into_iter()
        .enumerate()
        .map(|(part, paths)| {
            json!({"head": head, "since": since, "part": part, "parts": count, "paths": paths})
        })
        .collect()
}

/// The payload of `main_observed`: the history up to `head` recorded, as
/// read `at`, the record starting `since`.
pub fn observed_payload(head: &str, at: i64, since: i64) -> Value {
    json!({"head": head, "at": at, "since": since})
}

/// The payload of `main_read_failed`.
pub fn read_failed_payload(reason: &str, at: i64) -> Value {
    json!({"reason": reason, "at": at})
}

/// The payload of `main_read_recovered`: the failure of `reason` that
/// began `since` ended `at`.
pub fn read_recovered_payload(reason: &str, since: i64, at: i64) -> Value {
    json!({"reason": reason, "since": since, "at": at})
}

/// The payload of `main_rewritten`.
pub fn rewritten_payload(from: &str, head: &str, base: Option<&str>, at: i64) -> Value {
    json!({"from": from, "head": head, "base": base, "at": at})
}

/// The reach last recorded, as the supervisor goes on from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reach {
    /// The head observed: the history up to it is recorded.
    pub head: String,
    /// When (unix seconds).
    pub at: i64,
    /// Where the record of the history starts (unix seconds).
    pub since: i64,
}

impl Reach {
    /// The reach of the latest `main_observed`.
    pub fn of(event: &RunEvent) -> Option<Self> {
        Some(Self {
            head: event.payload.get("head")?.as_str()?.to_owned(),
            at: event.payload.get("at")?.as_i64()?,
            since: event.payload.get("since")?.as_i64()?,
        })
    }
}

/// Whether Git can be read, as the latest change recorded says.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MainReading {
    #[default]
    Readable,
    /// It could not from `since` (unix seconds), for `reason`.
    Failing { since: i64, reason: String },
}

impl MainReading {
    /// The state the latest `main_read_failed` or `main_read_recovered`
    /// left.
    pub fn of(event: Option<&RunEvent>) -> Self {
        match event {
            Some(event) if event.kind == MAIN_READ_FAILED => Self::Failing {
                since: event.payload["at"]
                    .as_i64()
                    .or_else(|| timestamp_millis(&event.created_at).map(|ms| ms / 1000))
                    .unwrap_or_default(),
                reason: event.payload["reason"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            },
            _ => Self::Readable,
        }
    }
}

/// What the record of main gives back, folded from its events alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MainLog {
    /// The commits since the record's start, oldest first, and the paths
    /// main has at the head observed (empty before the first base).
    pub history: MainHistory,
    /// The paths each recorded commit changed against its first parent,
    /// both names of a rename, by the commit's sha: what the areas read.
    pub changed: HashMap<String, Vec<String>>,
    /// The head last observed.
    pub head: Option<String>,
    /// The reach: when (unix seconds) the history up to `head` was last
    /// found recorded.
    pub recorded_through: Option<i64>,
    /// Where the record starts (unix seconds); `None` before the first
    /// record is complete.
    pub since: Option<i64>,
    pub reading: MainReading,
    /// Whether the paths are known: a base was recorded and no rewrite
    /// since waits for its own.
    pub paths_known: bool,
}

/// Fold the record of main from `events` (ascending id; other kinds are
/// skipped). A commit recorded twice counts once; after a rewrite the
/// record goes on from the merge base, the later record winning. Reads
/// neither Git nor the state.
pub fn fold_main_log(events: &[RunEvent]) -> MainLog {
    let mut log = MainLog::default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut paths: Option<HashSet<String>> = None;
    // The parts of a base by its head: its count and the parts seen.
    let mut bases: HashMap<String, (u64, BTreeSet<u64>, HashSet<String>)> = HashMap::new();
    for event in events {
        let payload = &event.payload;
        match event.kind.as_str() {
            MAIN_COMMITS_RECORDED => {
                for commit in payload["commits"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(commit_of)
                {
                    if !seen.insert(commit.sha.clone()) {
                        continue;
                    }
                    if let Some(paths) = &mut paths {
                        apply(paths, &commit.changes);
                    }
                    log.changed
                        .insert(commit.sha.clone(), changed_files(&commit.changes));
                    log.history.commits.push(commit);
                }
            }
            MAIN_PATHS_RECORDED => {
                let (Some(head), Some(part), Some(parts)) = (
                    payload["head"].as_str(),
                    payload["part"].as_u64(),
                    payload["parts"].as_u64(),
                ) else {
                    continue;
                };
                let base = bases
                    .entry(head.to_owned())
                    .or_insert_with(|| (parts, BTreeSet::new(), HashSet::new()));
                base.1.insert(part);
                base.2.extend(
                    payload["paths"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned),
                );
                if base.1.len() as u64 >= base.0 {
                    let (_, _, mut complete) = bases.remove(head).unwrap_or_default();
                    // The commits already folded past the base's head (another
                    // supervisor's, read later) apply to it.
                    if let Some(at) = log.history.commits.iter().position(|c| c.sha == head) {
                        for commit in &log.history.commits[at + 1..] {
                            apply(&mut complete, &commit.changes);
                        }
                    }
                    paths = Some(complete);
                    if log.since.is_none() {
                        log.since = payload["since"].as_i64();
                    }
                }
            }
            MAIN_REWRITTEN => {
                let base = payload["base"].as_str();
                let keep = base
                    .and_then(|base| log.history.commits.iter().position(|c| c.sha == base))
                    .map_or(0, |at| at + 1);
                log.history.commits.truncate(keep);
                seen = log.history.commits.iter().map(|c| c.sha.clone()).collect();
                paths = None;
            }
            MAIN_OBSERVED => {
                if let Some(reach) = Reach::of(event) {
                    log.head = Some(reach.head);
                    log.recorded_through = Some(reach.at);
                }
            }
            MAIN_READ_FAILED | MAIN_READ_RECOVERED => {
                log.reading = MainReading::of(Some(event));
            }
            _ => {}
        }
    }
    log.paths_known = paths.is_some();
    log.history.paths = paths.unwrap_or_default();
    log
}

/// `paths` after `changes`.
fn apply(paths: &mut HashSet<String>, changes: &[MainChange]) {
    for change in changes {
        if let Some(from) = &change.from {
            paths.remove(from);
        }
        if change.deleted {
            paths.remove(&change.path);
        } else {
            paths.insert(change.path.clone());
        }
    }
}

/// The files `changes` touched, both names of a rename, sorted.
fn changed_files(changes: &[MainChange]) -> Vec<String> {
    let files: BTreeSet<&str> = changes
        .iter()
        .flat_map(|change| std::iter::once(change.path.as_str()).chain(change.from.as_deref()))
        .collect();
    files.into_iter().map(str::to_owned).collect()
}

/// Why a window has no record of main's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MainMissing {
    /// No record is complete yet.
    NotRecorded,
    /// The window starts before the record does (unix seconds).
    BeforeRecord { since: i64 },
    /// Git could not be read from `since`, and the reach stopped.
    GitUnreadable { since: i64, reason: String },
    /// No supervisor ran when the reach fell overdue (`through` + grace).
    SupervisorStopped { through: i64 },
    /// The reach is older than the grace for another cause.
    Stale { through: i64 },
}

impl std::fmt::Display for MainMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let utc = |secs: &i64| utc_text(secs * 1000);
        match self {
            Self::NotRecorded => write!(f, "main's history is not recorded yet"),
            Self::BeforeRecord { since } => write!(
                f,
                "the window starts before main's history is recorded (from {})",
                utc(since)
            ),
            Self::GitUnreadable { since, reason } => write!(
                f,
                "main's history is not recorded: Git could not be read since {}: {reason}",
                utc(since)
            ),
            Self::SupervisorStopped { through } => write!(
                f,
                "main's history is not recorded: no supervisor ran after {}",
                utc(through)
            ),
            Self::Stale { through } => write!(
                f,
                "main's history is recorded only through {}",
                utc(through)
            ),
        }
    }
}

impl MainLog {
    /// Whether the record covers a window from `start_ms` to `end_ms`
    /// (unix milliseconds): it must start before the window and reach to
    /// within [`MAIN_RECORD_GRACE_SECS`] of its end. `events` tell whether
    /// a supervisor ran when the reach fell overdue.
    pub fn covers(
        &self,
        events: &[RunEvent],
        start_ms: Option<i64>,
        end_ms: Option<i64>,
    ) -> Result<(), MainMissing> {
        let (Some(since), Some(through), true) =
            (self.since, self.recorded_through, self.paths_known)
        else {
            return Err(MainMissing::NotRecorded);
        };
        if start_ms.is_some_and(|start| start < since * 1000) {
            return Err(MainMissing::BeforeRecord { since });
        }
        let overdue = (through + MAIN_RECORD_GRACE_SECS) * 1000;
        if end_ms.is_none_or(|end| end <= overdue) {
            return Ok(());
        }
        if let MainReading::Failing { since, reason } = &self.reading {
            return Err(MainMissing::GitUnreadable {
                since: *since,
                reason: reason.clone(),
            });
        }
        if !supervisor_alive_at(events, overdue) {
            return Err(MainMissing::SupervisorStopped { through });
        }
        Err(MainMissing::Stale { through })
    }

    /// Main's history for the window of `events` with `after < id <=
    /// upto` as `conflict_hotspots` spans it, or why the record has none.
    pub fn history_for(&self, events: &[RunEvent], after: EventId, upto: EventId) -> History {
        let (start, end) = window_span(events, after, upto);
        match self.covers(events, start, end) {
            Ok(()) => History::Read(self.history.clone()),
            Err(missing) => History::Unavailable(missing.to_string()),
        }
    }
}

/// Whether a supervisor lived at `at_ms`: one started by then whose life
/// had not ended ([`supervisor_life_end`]).
fn supervisor_alive_at(events: &[RunEvent], at_ms: i64) -> bool {
    let started: BTreeSet<&str> = events
        .iter()
        .filter(|event| event.kind == EventKind::SupervisorStarted)
        .filter(|event| timestamp_millis(&event.created_at).is_some_and(|ms| ms <= at_ms))
        .filter_map(|event| event.payload.get("supervisor").and_then(Value::as_str))
        .collect();
    started.into_iter().any(|token| {
        supervisor_life_end(events, token, at_ms).is_none_or(|end| end.at_ms() >= at_ms)
    })
}

#[cfg(test)]
mod tests;
