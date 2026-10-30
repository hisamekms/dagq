//! [`super::window_hotspot_files`] reads the default window's bounds and
//! its conflict events only, and gives what `stats` gives of every event.
use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::*;
use crate::application::EventStore;
use crate::domain::stats::conflicts::{MainChange, MainCommit, MainHistory};
use crate::domain::stats::{
    ConflictConfigReport, DEFAULT_RUNS, LiveSnapshot, SlotSnapshot, StatsQuery, StatsWindow,
    window::default_window,
};
use crate::domain::{EventFilter, EventId, RunEvent, RunId};

/// The time of event `id`: one second apart, on one day.
fn at(id: i64) -> String {
    format!(
        "2027-01-15T{:02}:{:02}:{:02}.000Z",
        id / 3600,
        id % 3600 / 60,
        id % 60
    )
}

/// The unix second of event `id`'s time.
fn secs(id: i64) -> i64 {
    crate::domain::stats::timestamp_millis(&at(id)).unwrap() / 1000
}

/// The events in memory, how often all of them were read, how many events
/// the reads gave and how often the window was read. The window is the
/// store's own range read ([`default_window`] of its events), which gives
/// no event.
#[derive(Default)]
struct Counted {
    events: Vec<RunEvent>,
    all: Cell<usize>,
    read: Cell<usize>,
    windows: Cell<usize>,
}

#[allow(unused_variables)]
impl EventStore for Counted {
    fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        unreachable!()
    }
    fn all_events(&self) -> Result<Vec<RunEvent>> {
        self.all.set(self.all.get() + 1);
        self.read.set(self.read.get() + self.events.len());
        Ok(self.events.clone())
    }
    fn stats_window(&self) -> Result<StatsWindow> {
        self.windows.set(self.windows.get() + 1);
        Ok(default_window(&self.events))
    }
    fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
        unreachable!()
    }
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        unreachable!()
    }
    fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
        unreachable!()
    }
    fn record_runtime_event(&self, id: &RunId, kind: EventKind, payload: Value) -> Result<()> {
        unreachable!()
    }
    fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
        unreachable!()
    }
    fn latest_event_id(&self) -> Result<EventId> {
        unreachable!()
    }
    fn record_backend_failure(&self, run: Option<&RunId>, payload: Value) -> Result<()> {
        unreachable!()
    }
    fn record_queue_event(&self, kind: EventKind, payload: Value) -> Result<EventId> {
        unreachable!()
    }
    fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>> {
        unreachable!()
    }
    fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>> {
        unreachable!()
    }
    fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
        unreachable!()
    }
    fn latest_events_by_supervisor(
        &self,
        kinds: &[&str],
        supervisors: &[&str],
    ) -> Result<Vec<RunEvent>> {
        unreachable!()
    }
    fn events_of_between(
        &self,
        kinds: &[&str],
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        let read: Vec<RunEvent> = self
            .events
            .iter()
            .filter(|e| e.id > after && e.id <= upto && kinds.contains(&e.kind.as_str()))
            .take(limit)
            .cloned()
            .collect();
        self.read.set(self.read.get() + read.len());
        Ok(read)
    }
    fn events_between(
        &self,
        after: EventId,
        upto: EventId,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        unreachable!()
    }
}

/// A queue of events written one after another.
#[derive(Default)]
struct Log {
    events: Vec<RunEvent>,
}

impl Log {
    fn push(&mut self, run: Option<usize>, kind: &str, payload: Value) -> EventId {
        let id = EventId::new(i64::try_from(self.events.len()).unwrap() + 1);
        self.events.push(RunEvent {
            id,
            task_id: run.map(|n| TaskId::new(i64::try_from(n).unwrap() + 1)),
            goal_id: None,
            run_id: run
                .map(|n| RunId::new(format!("{n:08x}-1111-4111-8111-111111111111")).unwrap()),
            kind: kind.into(),
            payload,
            created_at: at(id.as_i64()),
            actor: None,
        });
        id
    }
}

const OPEN: usize = 9_000;
const EARLY: usize = 9_001;

/// `outside` runs that ended before the window, each with a conflict in
/// `old.rs`; a run in flight whose conflicts in `hot.rs` fall on both
/// sides of the window's start; then the window's [`DEFAULT_RUNS`] runs,
/// the first of which started before the run in flight's last conflict,
/// with conflicts in `hot.rs` and `src/a.rs` (renamed on main to
/// `src/b.rs`), some ending failed.
fn queue(outside: usize) -> Vec<RunEvent> {
    let mut log = Log::default();
    for n in 0..outside {
        log.push(Some(n), "run_claimed", json!({}));
        log.push(
            Some(n),
            "conflict_precheck",
            json!({"main": format!("old{n}"), "conflicts": ["old.rs", "hot.rs"]}),
        );
        log.push(None, "host_sampled", json!({}));
        log.push(Some(n), "run_integrated", json!({}));
    }
    log.push(Some(OPEN), "run_claimed", json!({}));
    log.push(
        Some(OPEN),
        "conflict_precheck",
        json!({"main": "m-before", "conflicts": ["hot.rs"]}),
    );
    log.push(Some(EARLY), "run_claimed", json!({}));
    log.push(
        Some(OPEN),
        "integration_deferred",
        json!({"main": "m-after", "conflicts": ["hot.rs"]}),
    );
    log.push(Some(EARLY), "run_integrated", json!({}));
    for k in 1..DEFAULT_RUNS {
        let n = 10_000 + k;
        log.push(Some(n), "run_claimed", json!({}));
        if k % 3 == 0 {
            log.push(
                Some(n),
                "integration_deferred",
                json!({"main": format!("m{k}"), "conflicts": ["hot.rs", "src/a.rs"]}),
            );
            // The same main again is not counted twice.
            log.push(
                Some(n),
                "conflict_precheck",
                json!({"main": format!("m{k}"), "conflicts": ["hot.rs"]}),
            );
        }
        log.push(None, "host_sampled", json!({}));
        if k % 7 == 0 {
            log.push(Some(n), "run_failed", json!({"status": "failed"}));
        } else {
            log.push(Some(n), "run_integrated", json!({}));
        }
    }
    log.events
}

/// Main's history of `events`' time: a landing on `hot.rs` every 20th
/// event, `src/a.rs` renamed to `src/b.rs` by the one before the last, `old.rs` deleted; as
/// Git reads it, only the commits since the second asked.
fn history(events: &[RunEvent]) -> impl Fn(i64) -> Result<MainHistory> + use<> {
    let last = events.last().unwrap().id.as_i64();
    let landed: Vec<i64> = (1..=last).step_by(20).collect();
    let mut commits = Vec::new();
    for (i, &id) in landed.iter().enumerate() {
        let mut changes = vec![MainChange {
            path: "hot.rs".into(),
            from: None,
            deleted: false,
        }];
        if i + 2 == landed.len() {
            changes.push(MainChange {
                path: "src/b.rs".into(),
                from: Some("src/a.rs".into()),
                deleted: false,
            });
        }
        commits.push(MainCommit {
            sha: format!("{id:040}"),
            at: secs(id),
            changes,
        });
    }
    let paths: HashSet<String> = ["hot.rs", "src/b.rs"].map(String::from).into();
    move |since: i64| {
        Ok(MainHistory {
            commits: commits.iter().filter(|c| c.at >= since).cloned().collect(),
            paths: paths.clone(),
        })
    }
}

/// What `stats` gives of every event for the files.
fn of_every_event(
    events: &[RunEvent],
    read: &dyn Fn(i64) -> Result<MainHistory>,
) -> Vec<ConflictHotspot> {
    let live = LiveSnapshot {
        history: crate::application::stats::conflict_history(events, read),
        conflicts: ConflictConfigReport::default(),
        ..LiveSnapshot::default()
    };
    crate::domain::stats::stats(
        events,
        &HashMap::new(),
        secs(events.last().unwrap().id.as_i64()),
        SlotSnapshot::default(),
        &StatsQuery::default(),
        &live,
    )
    .conflict_hotspots
    .files
}

/// The files read from the window are those `stats` gives of every event,
/// with the conflict in flight on both sides of the window's start counted
/// on its side only, and a renamed file under its new name; however many
/// runs ended before the window, every event is read never, the window
/// once, and only its conflict events after it.
#[test]
fn the_hotspots_are_read_from_the_window_s_conflicts_as_stats_counts_them() {
    let mut reads = Vec::new();
    for outside in [2, 3 * DEFAULT_RUNS] {
        let events = queue(outside);
        let read = history(&events);
        let expected = of_every_event(&events, &read);
        let store = Counted {
            events: events.clone(),
            ..Counted::default()
        };
        let files = window_hotspot_files(&store, &read, ConflictConfigReport::default()).unwrap();
        assert_eq!(files, expected, "{outside} runs before the window");
        assert_eq!(store.all.get(), 0);
        assert_eq!(store.windows.get(), 1);
        reads.push(store.read.get());

        let hot = files.iter().find(|file| file.path == "hot.rs").unwrap();
        let in_window = 1 + (1..DEFAULT_RUNS).filter(|k| k % 3 == 0).count();
        assert_eq!(hot.conflicts, i64::try_from(in_window).unwrap());
        assert!(hot.alert);
        let renamed = files.iter().find(|file| file.path == "src/a.rs").unwrap();
        assert_eq!(renamed.renamed_to.as_deref(), Some("src/b.rs"));
        assert!(!files.iter().any(|file| file.path == "old.rs"));
    }
    let conflicts = 1 + 2 * (1..DEFAULT_RUNS).filter(|k| k % 3 == 0).count();
    assert_eq!(reads, [conflicts, conflicts]);
}

/// Without a conflict in the window no history is read, and none is
/// found, as `stats` finds none; a queue with no event has none either.
#[test]
fn a_window_without_a_conflict_has_no_hotspot_and_reads_no_history() {
    let mut log = Log::default();
    log.push(Some(1), "run_claimed", json!({}));
    log.push(
        Some(1),
        "conflict_precheck",
        json!({"main": "m", "conflicts": ["hot.rs"]}),
    );
    log.push(Some(1), "run_integrated", json!({}));
    for k in 0..DEFAULT_RUNS {
        log.push(Some(100 + k), "run_claimed", json!({}));
        log.push(Some(100 + k), "run_integrated", json!({}));
    }
    let never = |_: i64| -> Result<MainHistory> { unreachable!() };
    let store = Counted {
        events: log.events.clone(),
        ..Counted::default()
    };
    let files = window_hotspot_files(&store, &never, ConflictConfigReport::default()).unwrap();
    assert!(files.is_empty());
    let read = history(&log.events);
    assert!(of_every_event(&log.events, &read).is_empty());
    let empty = Counted::default();
    assert!(
        window_hotspot_files(&empty, &never, ConflictConfigReport::default())
            .unwrap()
            .is_empty()
    );
}
