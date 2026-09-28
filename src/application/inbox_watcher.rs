//! Whether the inbox has a watcher (ADR-t906-1): the one judgment `status`,
//! `doctor`, the plugin's Stop hook (through `status`) and the supervisor
//! share. A `watch --role inbox` leaves a record of itself in a file under
//! the queue's directory (`crate::infrastructure::inbox_watchers`) and
//! renews its heartbeat at every read of the queue; the judgment reads only
//! those records and the time it is given, never the processes, so a watch
//! that hangs or cannot read the queue counts as absent though its process
//! is still there.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use super::RunFiles;

/// The directory of the records, under the queue's directory.
pub const DIR: &str = "inbox-watchers";

/// The records' directory for the queue at `db`.
pub fn dir(db: &Path) -> PathBuf {
    match db.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(DIR),
        _ => PathBuf::from(DIR),
    }
}

/// Every record in `dir` read through `files`, skipping those that cannot
/// be read or parsed (a missing directory is no record): what the
/// supervisor judges by.
pub fn read(files: &dyn RunFiles, dir: &Path) -> Vec<WatcherRecord> {
    files
        .read_dir(dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| files.read(&path).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect()
}

/// A running watch whose heartbeat is older than this many `--interval`s
/// (plus [`HEARTBEAT_SLACK_SECS`]) is not watching: it renews the heartbeat
/// at every read, one `--interval` apart.
pub const HEARTBEAT_INTERVALS: i64 = 3;

/// Seconds added to the heartbeat and deadline limits, for a read of the
/// queue that takes a while and a slow write of the record.
pub const HEARTBEAT_SLACK_SECS: i64 = 10;

/// Seconds after a watch returned during which the inbox still counts as
/// watched: the session reports what the watch brought and starts the next
/// one in between.
pub const END_GRACE_SECS: i64 = 120;

/// One `watch --role inbox` as its record file holds it; times are unix
/// seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatcherRecord {
    pub pid: u32,
    pub started_at: i64,
    pub heartbeat_at: i64,
    #[serde(default)]
    pub ended_at: Option<i64>,
    /// The watch's `--timeout`; `None` (`null`) for a `--until-attention`
    /// watch, which has none. A record from before the mode holds a number.
    #[serde(default)]
    pub timeout_secs: Option<i64>,
    pub interval_secs: i64,
}

impl WatcherRecord {
    /// How old the heartbeat may be while the watch still counts as
    /// watching.
    pub fn stale_after_secs(&self) -> i64 {
        HEARTBEAT_INTERVALS
            .saturating_mul(self.interval_secs.max(1))
            .saturating_add(HEARTBEAT_SLACK_SECS)
    }

    /// Whether the watch is watching at `now`: it has not ended, its
    /// heartbeat is fresh, and it has not outlived its own `--timeout` (a
    /// watch past its deadline is stuck however fresh its heartbeat). A watch
    /// without a timeout is judged by its heartbeat alone.
    pub fn watching(&self, now: i64) -> bool {
        let stale = self.stale_after_secs();
        self.ended_at.is_none()
            && now - self.heartbeat_at <= stale
            && self.timeout_secs.is_none_or(|timeout| {
                now <= self
                    .started_at
                    .saturating_add(timeout.max(0))
                    .saturating_add(stale)
            })
    }

    /// The last time the watch was seen watching: when it ended, else its
    /// last heartbeat.
    pub fn last_seen_at(&self) -> i64 {
        self.ended_at.unwrap_or(self.heartbeat_at)
    }
}

/// `alive` or `absent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WatcherState {
    Alive,
    Absent,
}

/// The inbox's watcher at a moment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InboxWatcher {
    /// `alive` while a watch is watching or one returned less than
    /// [`END_GRACE_SECS`] ago; `absent` otherwise.
    pub state: WatcherState,
    /// The watches watching now, without the grace: the Stop hook wants
    /// one running before the turn ends.
    pub watching: usize,
    /// The last time any watch was seen; `None` when no watch ever left a
    /// record.
    pub last_seen_at: Option<i64>,
    /// Seconds since `last_seen_at` while `absent`; `None` while `alive` or
    /// when never seen.
    pub absent_secs: Option<i64>,
    pub grace_secs: i64,
}

impl InboxWatcher {
    pub fn to_json(&self) -> Value {
        json!(self)
    }
}

/// The inbox's watcher at `now` from every watch's record.
pub fn judge(records: &[WatcherRecord], now: i64) -> InboxWatcher {
    let watching = records.iter().filter(|r| r.watching(now)).count();
    let in_grace = records
        .iter()
        .filter_map(|r| r.ended_at)
        .any(|ended| now - ended <= END_GRACE_SECS);
    let last_seen_at = if watching > 0 {
        Some(now)
    } else {
        records.iter().map(WatcherRecord::last_seen_at).max()
    };
    let state = if watching > 0 || in_grace {
        WatcherState::Alive
    } else {
        WatcherState::Absent
    };
    InboxWatcher {
        state,
        watching,
        last_seen_at,
        absent_secs: match state {
            WatcherState::Alive => None,
            WatcherState::Absent => last_seen_at.map(|seen| (now - seen).max(0)),
        },
        grace_secs: END_GRACE_SECS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(started_at: i64, heartbeat_at: i64) -> WatcherRecord {
        WatcherRecord {
            pid: 42,
            started_at,
            heartbeat_at,
            ended_at: None,
            timeout_secs: Some(600),
            interval_secs: 2,
        }
    }

    #[test]
    fn a_watch_without_a_timeout_is_judged_by_its_heartbeat_alone() {
        let record = WatcherRecord {
            timeout_secs: None,
            ..running(1_000, 1_000)
        };
        // Long past any timeout, a fresh heartbeat is watching.
        let later = 1_000 + 30 * 24 * 3600;
        let fresh = WatcherRecord {
            heartbeat_at: later - 16,
            ..record.clone()
        };
        assert!(fresh.watching(later));
        let watcher = judge(std::slice::from_ref(&fresh), later);
        assert_eq!(watcher.state, WatcherState::Alive);
        assert_eq!(watcher.watching, 1);
        // A heartbeat older than the limit is absent, the process or not.
        let stale = WatcherRecord {
            heartbeat_at: later - 17,
            ..record
        };
        let watcher = judge(std::slice::from_ref(&stale), later);
        assert_eq!(watcher.state, WatcherState::Absent);
        assert_eq!(watcher.absent_secs, Some(17));
    }

    #[test]
    fn records_with_a_timeout_a_null_one_or_none_are_read() {
        let old: WatcherRecord = serde_json::from_str(
            r#"{"pid":1,"started_at":10,"heartbeat_at":12,"ended_at":null,"timeout_secs":600,"interval_secs":2}"#,
        )
        .unwrap();
        assert_eq!(old.timeout_secs, Some(600));
        let until: WatcherRecord = serde_json::from_str(
            r#"{"pid":1,"started_at":10,"heartbeat_at":12,"timeout_secs":null,"interval_secs":2}"#,
        )
        .unwrap();
        assert_eq!(until.timeout_secs, None);
        let missing: WatcherRecord = serde_json::from_str(
            r#"{"pid":1,"started_at":10,"heartbeat_at":12,"interval_secs":2}"#,
        )
        .unwrap();
        assert_eq!(missing.timeout_secs, None);
        assert_eq!(
            serde_json::to_value(&until).unwrap()["timeout_secs"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn no_record_is_absent_and_never_seen() {
        let watcher = judge(&[], 1_000);
        assert_eq!(watcher.state, WatcherState::Absent);
        assert_eq!(watcher.watching, 0);
        assert_eq!(watcher.last_seen_at, None);
        assert_eq!(watcher.absent_secs, None);
        assert_eq!(watcher.to_json()["state"], "absent");
    }

    #[test]
    fn a_fresh_heartbeat_is_alive_and_a_stale_one_is_absent_though_not_ended() {
        let record = running(1_000, 1_010);
        // 3 intervals of 2 s plus the slack: 16 s.
        assert_eq!(record.stale_after_secs(), 16);
        let watcher = judge(std::slice::from_ref(&record), 1_026);
        assert_eq!(watcher.state, WatcherState::Alive);
        assert_eq!(watcher.watching, 1);
        assert_eq!(watcher.last_seen_at, Some(1_026));
        assert_eq!(watcher.absent_secs, None);
        // One second past the limit, with its process possibly still there.
        let watcher = judge(&[record], 1_027);
        assert_eq!(watcher.state, WatcherState::Absent);
        assert_eq!(watcher.watching, 0);
        assert_eq!(watcher.last_seen_at, Some(1_010));
        assert_eq!(watcher.absent_secs, Some(17));
    }

    #[test]
    fn a_watch_that_outlives_its_timeout_is_stuck() {
        let record = running(1_000, 1_620);
        assert!(record.watching(1_616));
        assert!(!record.watching(1_627));
        assert_eq!(judge(&[record], 1_627).state, WatcherState::Absent);
    }

    #[test]
    fn an_ended_watch_is_alive_through_the_grace_then_absent() {
        let record = WatcherRecord {
            ended_at: Some(1_100),
            ..running(1_000, 1_100)
        };
        let watcher = judge(std::slice::from_ref(&record), 1_100 + END_GRACE_SECS);
        assert_eq!(watcher.state, WatcherState::Alive);
        assert_eq!(watcher.watching, 0);
        assert_eq!(watcher.last_seen_at, Some(1_100));
        let watcher = judge(&[record], 1_101 + END_GRACE_SECS);
        assert_eq!(watcher.state, WatcherState::Absent);
        assert_eq!(watcher.absent_secs, Some(END_GRACE_SECS + 1));
    }

    #[test]
    fn any_watching_record_makes_it_alive() {
        let old = WatcherRecord {
            ended_at: Some(500),
            ..running(400, 500)
        };
        let stuck = running(900, 950);
        let live = WatcherRecord {
            interval_secs: 30,
            ..running(1_000, 1_000)
        };
        let watcher = judge(&[old.clone(), stuck.clone(), live], 1_090);
        assert_eq!(watcher.state, WatcherState::Alive);
        assert_eq!(watcher.watching, 1);
        let watcher = judge(&[old, stuck], 1_090);
        assert_eq!(watcher.last_seen_at, Some(950));
        assert_eq!(watcher.absent_secs, Some(140));
    }
}
