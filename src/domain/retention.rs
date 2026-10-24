//! How long the queue directory keeps what ended sessions and jobs left
//! (ADR-t697-1): the directory of a closed planner, of a finished plan
//! review and of an observation goes once it has been over for
//! [`RetentionConfig::days`]; `backups/` keeps the newest
//! [`RetentionConfig::backups`] copies. The run directories are not
//! among them.

use super::run::payload::{restore, typed};
use super::{EventKind, RunEvent};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The days [`RetentionConfig::days`] is by default.
pub const DEFAULT_RETENTION_DAYS: u64 = 7;

/// The copies [`RetentionConfig::backups`] is by default.
pub const DEFAULT_BACKUP_GENERATIONS: usize = 2;

/// `[retention]` of `dagq.toml` (ADR-t697-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionConfig {
    /// `days`: how long, in whole days above 0, the directory of a closed
    /// planner (`planners/<id>`), a finished plan review
    /// (`plan-reviews/<id>`) and an observation (`observer/<started_at>`)
    /// is kept after it was closed, finished or last written.
    pub days: u64,
    /// `backups`: how many of the copies a breaking `migrate` made in
    /// `backups/` are kept, the newest first; a whole number above 0.
    pub backups: usize,
}

impl RetentionConfig {
    /// The keys of `[retention]`.
    pub const KEYS: [&'static str; 2] = ["days", "backups"];

    /// The time (UNIX seconds) at or before which something that ended is
    /// past its retention at `now`.
    pub fn cutoff(&self, now: i64) -> i64 {
        let secs = i64::try_from(self.days.saturating_mul(86_400)).unwrap_or(i64::MAX);
        now.saturating_sub(secs)
    }

    /// Whether something that ended at `ended_at` is past its retention at
    /// `now`; something not ended (`None`) never is.
    pub fn expired(&self, ended_at: Option<i64>, now: i64) -> bool {
        ended_at.is_some_and(|ended| ended <= self.cutoff(now))
    }

    /// The names among `names` (the files of `backups/`) to remove: the
    /// copies `migrate` names `queue-<version>-<unix>[-<n>].sqlite3`
    /// ([`backup_order`]) past the newest [`Self::backups`]. A name of
    /// another shape is kept.
    pub fn backups_to_remove<'a>(&self, names: &[&'a str]) -> Vec<&'a str> {
        let mut copies: Vec<(&str, (i64, u64))> = names
            .iter()
            .filter_map(|name| backup_order(name).map(|order| (*name, order)))
            .collect();
        copies.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(a.0)));
        copies
            .into_iter()
            .skip(self.backups)
            .map(|(name, _)| name)
            .collect()
    }
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            days: DEFAULT_RETENTION_DAYS,
            backups: DEFAULT_BACKUP_GENERATIONS,
        }
    }
}

/// Where a copy `migrate` made sits among the others: the UNIX second it
/// was made and the suffix a second copy in the same second took
/// (`queue-<version>-<unix>[-<n>].sqlite3`); `None` for a name of another
/// shape.
pub fn backup_order(name: &str) -> Option<(i64, u64)> {
    let stem = name.strip_prefix("queue-")?.strip_suffix(".sqlite3")?;
    let mut parts = stem.split('-');
    parts.next()?.parse::<u64>().ok()?;
    let made: i64 = parts.next()?.parse().ok()?;
    let suffix = match parts.next() {
        Some(n) => n.parse().ok()?,
        None => 0,
    };
    parts.next().is_none().then_some((made, suffix))
}

/// Whether `name` is an observation's directory under `observer/`
/// (`<started_at>` or `<started_at>-<n>`), not its cursor or a
/// temporary file.
pub fn observation_name(name: &str) -> bool {
    let mut parts = name.split('-');
    let started = parts.next().is_some_and(|s| s.parse::<i64>().is_ok());
    let suffix = parts.next().is_none_or(|n| n.parse::<u64>().is_ok());
    started && suffix && parts.next().is_none()
}

/// What one removal past the retention did: the paths it removed, and the
/// first failure among its reads and removals (`E`), which did not stop
/// the others.
#[derive(Debug)]
pub struct Swept<E> {
    pub removed: Vec<PathBuf>,
    pub failure: Option<E>,
}

impl<E> Swept<E> {
    /// Nothing removed, nothing failed.
    pub fn new() -> Self {
        Self {
            removed: Vec::new(),
            failure: None,
        }
    }

    /// Keep `error` unless an earlier one is kept.
    pub fn failed(&mut self, error: E) {
        self.failure.get_or_insert(error);
    }
}

impl<E> Default for Swept<E> {
    fn default() -> Self {
        Self::new()
    }
}

/// A recorded `observe_started` or `observe_finished` restored as the
/// typed values the retention decides by (C6 of
/// docs/design/architecture.md). It never refuses a payload: a missing
/// key, `null` or a value of another type (an older writer) reads as
/// absent.
#[derive(Debug, Default, Deserialize)]
pub struct RecordedObservation<'a> {
    /// Whether the event is the observation's start; else its finish.
    #[serde(skip)]
    pub started: bool,
    /// Its directory as recorded; a finish of an observation that ran no
    /// agent records none.
    #[serde(default, borrow, deserialize_with = "typed")]
    pub dir: Option<&'a str>,
    /// The process that ran it, on its start.
    #[serde(default, deserialize_with = "typed")]
    pub pid: Option<u32>,
}

impl<'a> RecordedObservation<'a> {
    /// `event` restored; `None` for an event of another kind.
    pub fn restore(event: &'a RunEvent) -> Option<Self> {
        let started = event.kind == EventKind::ObserveStarted;
        if !started && event.kind != EventKind::ObserveFinished {
            return None;
        }
        Some(Self {
            started,
            ..restore(&event.payload)
        })
    }

    /// The name of its directory under `observer/`: the queue directory
    /// may have moved since it was recorded.
    pub fn name(&self) -> Option<&'a str> {
        self.dir
            .map(Path::new)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
    }
}

/// The observations over by the queue's records (ADR-t697-1 decision 2),
/// by the name of their directory under `observer/`: of the starts and
/// finishes in `records` (oldest first), each directory a finish was
/// recorded for after its latest start, and whose start's `pid` no longer
/// runs (`alive`); a finish with no start among `records` counts as over,
/// with no process to look at. Only these may go once their files are past the
/// retention: one whose finish is not recorded (still running quietly
/// past the retention with `observe --timeout`, fallen before it recorded
/// it, or named by no record at all) is kept however old its last write,
/// as is one whose process still runs.
pub fn ended_observations(
    records: &[RecordedObservation<'_>],
    alive: impl Fn(u32) -> bool,
) -> BTreeSet<String> {
    // Each directory's pid of its latest start, and whether a finish came
    // after it.
    let mut latest: BTreeMap<&str, (Option<u32>, bool)> = BTreeMap::new();
    for record in records {
        let Some(name) = record.name() else {
            continue;
        };
        if record.started {
            latest.insert(name, (record.pid, false));
        } else {
            latest.entry(name).or_insert((None, true)).1 = true;
        }
    }
    latest
        .into_iter()
        .filter(|(_, (pid, finished))| *finished && !pid.is_some_and(&alive))
        .map(|(name, _)| name.to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn observe(id: i64, kind: EventKind, dir: &str, pid: u32) -> RunEvent {
        RunEvent {
            id: super::super::EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload: json!({"dir": dir, "pid": pid}),
            created_at: String::new(),
            actor: None,
        }
    }

    #[test]
    fn an_observation_is_over_once_its_finish_is_recorded_and_its_process_gone() {
        use EventKind::{ObserveFinished, ObserveStarted};
        let events = [
            // Running: started, its process alive, no finish.
            observe(1, ObserveStarted, "/q/observer/100", 7),
            // Fell before its finish: its process dead, no finish.
            observe(2, ObserveStarted, "/q/observer/200", 8),
            // Over: finished and its process gone.
            observe(3, ObserveStarted, "/q/observer/300", 9),
            observe(4, ObserveFinished, "/q/observer/300", 9),
            // Finished, its process not gone yet.
            observe(5, ObserveStarted, "/elsewhere/observer/400", 7),
            observe(6, ObserveFinished, "/elsewhere/observer/400", 7),
            // A finish whose start is not among the events.
            observe(7, ObserveFinished, "/q/observer/500", 8),
            // Started again after a finish: its latest start has none.
            observe(8, ObserveStarted, "/q/observer/600", 9),
            observe(9, ObserveFinished, "/q/observer/600", 9),
            observe(10, ObserveStarted, "/q/observer/600", 9),
        ];
        let records: Vec<_> = events
            .iter()
            .filter_map(RecordedObservation::restore)
            .collect();
        let ended = ended_observations(&records, |pid| pid == 7);
        // A directory no record names ("700") is not among them either.
        assert_eq!(ended.into_iter().collect::<Vec<_>>(), ["300", "500"]);
    }

    #[test]
    fn a_recorded_observation_reads_what_older_writers_left_as_absent() {
        let event = |kind: EventKind, payload| RunEvent {
            payload,
            ..observe(1, kind, "/q/observer/1", 1)
        };
        let started = event(
            EventKind::ObserveStarted,
            json!({"dir": "/q/observer/1-2", "pid": 42, "mode": "hourly"}),
        );
        let restored = RecordedObservation::restore(&started).unwrap();
        assert!(restored.started);
        assert_eq!((restored.name(), restored.pid), (Some("1-2"), Some(42)));
        for payload in [
            json!({"dir": null, "pid": "42"}),
            json!({"dir": 5, "pid": -1}),
            json!({}),
            json!("not an object"),
        ] {
            let finished = event(EventKind::ObserveFinished, payload);
            let restored = RecordedObservation::restore(&finished).unwrap();
            assert!(!restored.started);
            assert_eq!((restored.name(), restored.pid), (None, None));
        }
        let other = event(EventKind::CiWatchHeld, json!({"dir": "/q/observer/1"}));
        assert!(RecordedObservation::restore(&other).is_none());
    }

    #[test]
    fn something_ended_is_expired_only_past_the_days() {
        let config = RetentionConfig::default();
        let now = 100 * 86_400;
        assert!(config.expired(Some(now - 7 * 86_400), now));
        assert!(!config.expired(Some(now - 7 * 86_400 + 1), now));
        assert!(!config.expired(None, now));
        let one = RetentionConfig { days: 1, ..config };
        assert!(one.expired(Some(now - 86_400), now));
        assert!(!one.expired(Some(now - 3_600), now));
    }

    #[test]
    fn backups_keep_the_newest_copies() {
        let names = [
            "queue-40-100.sqlite3",
            "queue-41-300.sqlite3",
            "queue-41-300-1.sqlite3",
            "queue-39-50.sqlite3",
            "notes.txt",
            "queue-x-1.sqlite3",
        ];
        let config = RetentionConfig::default();
        let mut removed = config.backups_to_remove(&names);
        removed.sort_unstable();
        assert_eq!(removed, ["queue-39-50.sqlite3", "queue-40-100.sqlite3"]);
        let one = RetentionConfig {
            backups: 1,
            ..config
        };
        let mut removed = one.backups_to_remove(&names);
        removed.sort_unstable();
        assert_eq!(
            removed,
            [
                "queue-39-50.sqlite3",
                "queue-40-100.sqlite3",
                "queue-41-300.sqlite3"
            ]
        );
        assert!(config.backups_to_remove(&names[..2]).is_empty());
    }

    #[test]
    fn backup_names_are_read_as_migrate_writes_them() {
        assert_eq!(backup_order("queue-41-300.sqlite3"), Some((300, 0)));
        assert_eq!(backup_order("queue-41-300-2.sqlite3"), Some((300, 2)));
        assert_eq!(backup_order("queue-41-300-2-3.sqlite3"), None);
        assert_eq!(backup_order("queue-41.sqlite3"), None);
        assert_eq!(backup_order("queue-41-300.db"), None);
    }

    #[test]
    fn observation_names_leave_the_cursor() {
        assert!(observation_name("1700000000"));
        assert!(observation_name("1700000000-1"));
        assert!(!observation_name("cursor"));
        assert!(!observation_name(".cursor.12.tmp"));
        assert!(!observation_name("1700000000-x"));
    }
}
