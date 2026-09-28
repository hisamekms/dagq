//! Whether the inbox has a watcher (ADR-t906-1): the one judgment `status`,
//! `doctor`, the plugin's Stop hook (through `status`) and the supervisor
//! share. A `watch --role inbox` leaves a record of itself in a file under
//! the queue's directory (`crate::infrastructure::inbox_watchers`) and
//! renews its heartbeat at every read of the queue. A watch counts as
//! watching only while its heartbeat is fresh, so one that hangs or cannot
//! read the queue counts as absent though its process is still there. The
//! process is a further condition on the other side only (task 927): a
//! watch killed by SIGKILL (Claude Code's `/clear` or KillShell) writes no
//! end, and its record still has a fresh heartbeat for a while, so a record
//! whose process is gone, or whose pid now runs a process started at
//! another time (the pid reused), is not watching however fresh its
//! heartbeat. [`judge`] never looks at the processes itself: [`processes`]
//! asks a [`ProcessControl`] what runs under each record's pid and hands
//! the answers over, and a process whose start cannot be read leaves the
//! judgment to the heartbeat alone.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use super::{ProcessControl, RunFiles};

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

/// Seconds a watch's process may have started before its record's
/// `started_at`: the watch writes the record after it has opened the
/// queue, which a queue copied to read an older schema can take a while.
pub const PROCESS_START_LEAD_SECS: i64 = 60;

/// Seconds a watch's process may seem to have started after its record's
/// `started_at`: the system gives the process's age to the second, and it
/// is read a moment after the time it is subtracted from. A process started
/// later than this runs under a pid reused after the watch ended.
pub const PROCESS_START_LAG_SECS: i64 = 5;

/// What runs under a record's pid, as the system says at the judgment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherProcess {
    /// A process started at this unix second.
    Running { started_at: i64 },
    /// No process has the pid.
    Gone,
    /// Not known: the process's start could not be read, or the record was
    /// not looked up because its heartbeat alone rules it out.
    Unknown,
}

impl WatcherProcess {
    /// Whether this says the record's watch is no longer running: its pid
    /// runs nothing, or a process that started outside the record's start
    /// by more than the allowance.
    pub fn ended(&self, record: &WatcherRecord) -> bool {
        match *self {
            Self::Gone => true,
            Self::Running { started_at } => {
                started_at < record.started_at.saturating_sub(PROCESS_START_LEAD_SECS)
                    || started_at > record.started_at.saturating_add(PROCESS_START_LAG_SECS)
            }
            Self::Unknown => false,
        }
    }
}

/// What runs under each record's pid, in the order of `records`, asked of
/// `control` only for the records that would count as watching at `now` by
/// their record alone (the others are [`WatcherProcess::Unknown`]): a
/// queue's directory keeps records for days, and a record past its
/// heartbeat does not need its process looked up.
pub fn processes(
    records: &[WatcherRecord],
    control: &dyn ProcessControl,
    now: i64,
) -> Vec<WatcherProcess> {
    records
        .iter()
        .map(|record| {
            if !record.watching(now) {
                return WatcherProcess::Unknown;
            }
            if !control.alive(record.pid) {
                return WatcherProcess::Gone;
            }
            match control.started_at(record.pid) {
                Some(started_at) => WatcherProcess::Running { started_at },
                // It may have ended between the two looks.
                None if !control.alive(record.pid) => WatcherProcess::Gone,
                None => WatcherProcess::Unknown,
            }
        })
        .collect()
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

/// The inbox's watcher at `now` from every watch's record and what runs
/// under its pid ([`processes`], in the same order; a record without one
/// counts as [`WatcherProcess::Unknown`]). A record whose process ended
/// without writing its end is not watching, and its last sighting stays
/// its last heartbeat: the end is not known, so no grace starts from it.
pub fn judge(records: &[WatcherRecord], processes: &[WatcherProcess], now: i64) -> InboxWatcher {
    let watching = records
        .iter()
        .enumerate()
        .filter(|(i, r)| {
            r.watching(now)
                && !processes
                    .get(*i)
                    .copied()
                    .unwrap_or(WatcherProcess::Unknown)
                    .ended(r)
        })
        .count();
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

/// [`judge`] of `records` with their processes looked up through
/// `control`: what `status`, `doctor` and the supervisor call.
pub fn judge_with(
    records: &[WatcherRecord],
    control: &dyn ProcessControl,
    now: i64,
) -> InboxWatcher {
    judge(records, &processes(records, control, now), now)
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
        let watcher = judge(std::slice::from_ref(&fresh), &[], later);
        assert_eq!(watcher.state, WatcherState::Alive);
        assert_eq!(watcher.watching, 1);
        // A heartbeat older than the limit is absent, the process or not.
        let stale = WatcherRecord {
            heartbeat_at: later - 17,
            ..record
        };
        let watcher = judge(std::slice::from_ref(&stale), &[], later);
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
        let watcher = judge(&[], &[], 1_000);
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
        let watcher = judge(std::slice::from_ref(&record), &[], 1_026);
        assert_eq!(watcher.state, WatcherState::Alive);
        assert_eq!(watcher.watching, 1);
        assert_eq!(watcher.last_seen_at, Some(1_026));
        assert_eq!(watcher.absent_secs, None);
        // One second past the limit, with its process possibly still there.
        let watcher = judge(&[record], &[], 1_027);
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
        assert_eq!(judge(&[record], &[], 1_627).state, WatcherState::Absent);
    }

    #[test]
    fn an_ended_watch_is_alive_through_the_grace_then_absent() {
        let record = WatcherRecord {
            ended_at: Some(1_100),
            ..running(1_000, 1_100)
        };
        let watcher = judge(std::slice::from_ref(&record), &[], 1_100 + END_GRACE_SECS);
        assert_eq!(watcher.state, WatcherState::Alive);
        assert_eq!(watcher.watching, 0);
        assert_eq!(watcher.last_seen_at, Some(1_100));
        let watcher = judge(&[record], &[], 1_101 + END_GRACE_SECS);
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
        let watcher = judge(&[old.clone(), stuck.clone(), live], &[], 1_090);
        assert_eq!(watcher.state, WatcherState::Alive);
        assert_eq!(watcher.watching, 1);
        let watcher = judge(&[old, stuck], &[], 1_090);
        assert_eq!(watcher.last_seen_at, Some(950));
        assert_eq!(watcher.absent_secs, Some(140));
    }

    /// Processes by pid: a start, `None` for a process whose start cannot
    /// be read; a pid not listed runs nothing.
    struct Fake(Vec<(u32, Option<i64>)>);

    impl ProcessControl for Fake {
        fn alive(&self, pid: u32) -> bool {
            self.0.iter().any(|(p, _)| *p == pid)
        }
        fn terminate(&self, _: u32) -> anyhow::Result<()> {
            unreachable!()
        }
        fn interrupt(&self, _: u32) -> anyhow::Result<()> {
            unreachable!()
        }
        fn kill(&self, _: u32) -> anyhow::Result<()> {
            unreachable!()
        }
        fn started_at(&self, pid: u32) -> Option<i64> {
            self.0.iter().find(|(p, _)| *p == pid).and_then(|(_, s)| *s)
        }
    }

    #[test]
    fn a_fresh_record_whose_process_is_gone_or_another_is_not_watching() {
        // Heartbeat 2 s old, well inside the 16 s: watching by the record.
        let record = running(1_000, 1_008);
        let now = 1_010;
        assert!(record.watching(now));
        let one = std::slice::from_ref(&record);
        // Its pid runs nothing: killed without writing its end.
        let gone = judge_with(one, &Fake(vec![]), now);
        assert_eq!(gone.watching, 0);
        assert_eq!(gone.state, WatcherState::Absent);
        // The last sighting is the last heartbeat, not the judgment.
        assert_eq!(gone.last_seen_at, Some(1_008));
        assert_eq!(gone.absent_secs, Some(2));
        // Its pid runs a process started after the watch: the pid reused.
        let reused = judge_with(one, &Fake(vec![(42, Some(1_006))]), now);
        assert_eq!((reused.watching, reused.state), (0, WatcherState::Absent));
        // Or long before it.
        let older = judge_with(
            one,
            &Fake(vec![(42, Some(1_000 - PROCESS_START_LEAD_SECS - 1))]),
            now,
        );
        assert_eq!((older.watching, older.state), (0, WatcherState::Absent));
        // Another watch that ended within the grace still keeps it alive.
        let ended = WatcherRecord {
            pid: 7,
            ended_at: Some(990),
            ..running(900, 990)
        };
        let both = [record.clone(), ended];
        let watcher = judge_with(&both, &Fake(vec![]), now);
        assert_eq!((watcher.watching, watcher.state), (0, WatcherState::Alive));
    }

    #[test]
    fn a_process_started_with_the_record_or_of_unknown_start_leaves_it_to_the_heartbeat() {
        let record = running(1_000, 1_008);
        let one = std::slice::from_ref(&record);
        for fake in [
            Fake(vec![(42, Some(1_000))]),
            Fake(vec![(42, Some(1_000 - PROCESS_START_LEAD_SECS))]),
            Fake(vec![(42, Some(1_000 + PROCESS_START_LAG_SECS))]),
            Fake(vec![(42, None)]),
        ] {
            let watcher = judge_with(one, &fake, 1_010);
            assert_eq!(watcher, judge(one, &[], 1_010));
            assert_eq!((watcher.watching, watcher.state), (1, WatcherState::Alive));
            // A live process does not make a stale heartbeat watching.
            let stale = judge_with(one, &fake, 1_025);
            assert_eq!(stale, judge(one, &[], 1_025));
            assert_eq!((stale.watching, stale.state), (0, WatcherState::Absent));
        }
    }

    #[test]
    fn only_records_watching_by_themselves_are_looked_up() {
        let stale = running(980, 990);
        let ended = WatcherRecord {
            ended_at: Some(1_005),
            ..running(1_000, 1_005)
        };
        let fresh = running(1_000, 1_008);
        let found = processes(&[stale, ended, fresh], &Fake(vec![]), 1_010);
        assert_eq!(
            found,
            [
                WatcherProcess::Unknown,
                WatcherProcess::Unknown,
                WatcherProcess::Gone
            ]
        );
    }
}
