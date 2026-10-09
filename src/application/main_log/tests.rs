use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
};

use serde_json::json;

use super::*;
use crate::domain::{
    EventId, RunEvent,
    marks::utc_text,
    stats::{
        ConflictConfigReport,
        conflicts::{History, HistoryCheck, MainChange, MainHistory, conflict_hotspots},
        main_log::{
            MAIN_LOG_KINDS, MAIN_OBSERVED, MAIN_READ_FAILED, MAIN_READ_RECOVERED,
            MAIN_RECORD_GRACE_SECS, MainLog, MainMissing, fold_main_log,
        },
    },
};

const T: i64 = 1_800_000_000;

/// Main as a list of first-parent commits, read the way Git reads it; a
/// commit dropped by a rewrite is kept as an object with its merge base.
#[derive(Default)]
struct Main {
    line: RefCell<Vec<MainCommit>>,
    /// The commits no longer on the line, with their merge base with it.
    gone: RefCell<HashMap<String, Option<String>>>,
    failing: Cell<bool>,
}

impl Main {
    fn land(&self, sha: &str, at: i64, changes: Vec<MainChange>) {
        self.line.borrow_mut().push(MainCommit {
            sha: sha.to_owned(),
            at,
            changes,
        });
    }

    /// Rewrite the line after `keep` commits, as a force push does.
    fn rewrite(&self, keep: usize) {
        let mut line = self.line.borrow_mut();
        let base = keep.checked_sub(1).map(|at| line[at].sha.clone());
        for gone in line.drain(keep..) {
            self.gone.borrow_mut().insert(gone.sha, base.clone());
        }
    }

    fn check(&self) -> Result<()> {
        if self.failing.get() {
            anyhow::bail!("git failed");
        }
        Ok(())
    }

    /// What the reader of Git `stats` uses gives: the commits since
    /// `since` and the paths of the head.
    fn history(&self, since: i64) -> MainHistory {
        MainHistory {
            commits: self
                .line
                .borrow()
                .iter()
                .filter(|commit| commit.at >= since)
                .cloned()
                .collect(),
            paths: self.paths("").unwrap().into_iter().collect(),
        }
    }
}

impl MainReader for Main {
    fn head(&self) -> Result<String> {
        self.check()?;
        Ok(self
            .line
            .borrow()
            .last()
            .map_or_else(|| "root".to_owned(), |c| c.sha.clone()))
    }
    fn commits(&self, _: &str, after: Option<&str>, since: i64) -> Result<Vec<MainCommit>> {
        self.check()?;
        let line = self.line.borrow();
        let start = after
            .and_then(|after| line.iter().position(|c| c.sha == after))
            .map_or(0, |at| at + 1);
        Ok(line[start..]
            .iter()
            .filter(|commit| commit.at >= since)
            .cloned()
            .collect())
    }
    fn paths(&self, _: &str) -> Result<Vec<String>> {
        self.check()?;
        let mut paths = HashSet::new();
        for commit in self.line.borrow().iter() {
            for change in &commit.changes {
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
        Ok(paths.into_iter().collect())
    }
    fn reaches(&self, commit: &str, _: &str) -> Result<bool> {
        self.check()?;
        Ok(self.line.borrow().iter().any(|c| c.sha == commit))
    }
    fn merge_base(&self, a: &str, _: &str) -> Result<Option<String>> {
        self.check()?;
        Ok(self.gone.borrow().get(a).cloned().flatten())
    }
}

fn change(path: &str) -> MainChange {
    MainChange {
        path: path.to_owned(),
        from: None,
        deleted: false,
    }
}

/// The queue's events, with what a supervisor's pass appends.
struct Queue {
    events: Vec<RunEvent>,
}

impl Queue {
    /// A queue whose earliest event is `at`.
    fn since(at: i64) -> Self {
        let mut queue = Self { events: Vec::new() };
        queue.push("supervisor_started", json!({"supervisor": "s"}), at);
        queue
    }

    fn push(&mut self, kind: &str, payload: Value, at: i64) {
        let id = self.events.len() as i64 + 1;
        self.events.push(RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: utc_text(at * 1000),
            actor: None,
        });
    }

    fn latest(&self, kinds: &[&str]) -> Option<&RunEvent> {
        self.events
            .iter()
            .rev()
            .find(|event| kinds.contains(&event.kind.as_str()))
    }

    /// A pass of the supervisor at `now`: what it recorded.
    fn pass(&mut self, main: &Main, now: i64) -> Vec<&'static str> {
        let recorded = Recorded {
            reach: self.latest(&[MAIN_OBSERVED]).and_then(Reach::of),
            reading: MainReading::of(self.latest(&[MAIN_READ_FAILED, MAIN_READ_RECOVERED])),
        };
        let first = self.events.first().map(|e| e.created_at.clone());
        let earliest = || {
            Ok(first
                .as_deref()
                .and_then(crate::domain::stats::timestamp_millis))
        };
        let records = records(&recorded, now, &earliest, main);
        let kinds = records.iter().map(|(kind, _)| kind.as_str()).collect();
        for (kind, payload) in records {
            self.push(kind.as_str(), payload, now);
        }
        // The supervisor's evidence of life goes on.
        self.push("supervisor_alive", json!({"supervisor": "s"}), now);
        kinds
    }

    fn log(&self) -> MainLog {
        fold_main_log(&self.events)
    }

    fn count(&self, kind: &str) -> usize {
        self.events
            .iter()
            .filter(|event| event.kind == kind)
            .count()
    }
}

const COMMITS: &str = "main_commits_recorded";

/// (1) The first pass takes the history; a pass after a landing and a
/// person's own commit records both once, after the head it reached; a
/// pass with the head at rest records no commit.
#[test]
fn a_pass_records_the_commits_since_its_reach_once() {
    let main = Main::default();
    main.land("a", T + 1, vec![change("a.rs")]);
    let mut queue = Queue::since(T);
    assert_eq!(
        queue.pass(&main, T + 10),
        [COMMITS, "main_paths_recorded", MAIN_OBSERVED],
        "the first record"
    );
    // A landing by dagq, and a person's push past dagq.
    main.land("b", T + 20, vec![change("b.rs")]);
    main.land("c", T + 25, vec![change("c.rs")]);
    assert_eq!(queue.pass(&main, T + 30), [COMMITS, MAIN_OBSERVED]);
    let recorded = queue.latest(&[COMMITS]).unwrap();
    assert_eq!(recorded.payload["after"], "a");
    let shas: Vec<&str> = recorded.payload["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["sha"].as_str().unwrap())
        .collect();
    assert_eq!(shas, ["b", "c"]);
    // The head rests: nothing is recorded until the reach is due.
    assert!(queue.pass(&main, T + 31).is_empty());
    assert!(
        queue
            .pass(&main, T + 30 + MAIN_OBSERVED_INTERVAL_SECS - 1)
            .is_empty()
    );
    assert_eq!(queue.count(COMMITS), 2);
    let log = queue.log();
    assert_eq!(log.history, main.history(T - 1));
    assert_eq!(log.head.as_deref(), Some("c"));
}

/// (1) A force push that leaves the head recorded off main is recorded,
/// and the record goes on from the merge base (or from its start without
/// one), folding to what Git reads.
#[test]
fn a_rewritten_main_is_recorded_again_from_the_merge_base() {
    let main = Main::default();
    main.land("a", T + 1, vec![change("a.rs")]);
    main.land("b", T + 2, vec![change("b.rs")]);
    let mut queue = Queue::since(T);
    queue.pass(&main, T + 10);
    main.rewrite(1);
    main.land("b2", T + 20, vec![change("b2.rs")]);
    assert_eq!(
        queue.pass(&main, T + 30),
        [
            "main_rewritten",
            COMMITS,
            "main_paths_recorded",
            MAIN_OBSERVED
        ]
    );
    let rewritten = queue.latest(&["main_rewritten"]).unwrap();
    assert_eq!(rewritten.payload["from"], "b");
    assert_eq!(rewritten.payload["base"], "a");
    assert_eq!(queue.log().history, main.history(T - 1));
    // No merge base: the whole line again.
    main.rewrite(0);
    main.land("z", T + 40, vec![change("z.rs")]);
    queue.pass(&main, T + 50);
    assert_eq!(
        queue.latest(&["main_rewritten"]).unwrap().payload["base"],
        Value::Null
    );
    assert_eq!(queue.log().history, main.history(T - 1));
}

/// (2) A head at rest read fine moves the reach at its interval without a
/// commit, so a window ending after the latest commit is still covered.
#[test]
fn the_reach_moves_at_its_interval_while_the_head_rests() {
    let main = Main::default();
    main.land("a", T + 1, vec![change("a.rs")]);
    let mut queue = Queue::since(T);
    queue.pass(&main, T + 10);
    let mut now = T + 10;
    while now < T + 10 + 3 * MAIN_OBSERVED_INTERVAL_SECS {
        now += 60;
        queue.pass(&main, now);
    }
    assert_eq!(queue.count(COMMITS), 1);
    assert_eq!(queue.count(MAIN_OBSERVED), 4);
    let log = queue.log();
    assert_eq!(log.head.as_deref(), Some("a"));
    assert_eq!(
        log.recorded_through,
        Some(T + 10 + 3 * MAIN_OBSERVED_INTERVAL_SECS)
    );
    // A window far after the latest commit, ending now.
    assert_eq!(
        log.covers(&queue.events, Some((T + 600) * 1000), Some(now * 1000)),
        Ok(())
    );
}

/// (2) While Git cannot be read the reach stops and the failure's start is
/// recorded once; past the grace a window has no record for that reason,
/// the supervisor alive. Read again, the commits landed meanwhile are
/// recorded, the reach moves and the window's history is Git's.
#[test]
fn a_failure_stops_the_reach_and_its_recovery_catches_up() {
    let main = Main::default();
    main.land("a", T + 1, vec![change("a.rs")]);
    let mut queue = Queue::since(T);
    queue.pass(&main, T + 10);
    main.failing.set(true);
    let mut now = T + 10;
    let mut failed = Vec::new();
    while now < T + 10 + 2 * MAIN_RECORD_GRACE_SECS {
        now += 60;
        failed.extend(queue.pass(&main, now));
        if now == T + 130 {
            main.line.borrow_mut().push(MainCommit {
                sha: "b".into(),
                at: now,
                changes: vec![change("b.rs")],
            });
        }
    }
    assert_eq!(failed, ["main_read_failed"]);
    let log = queue.log();
    assert_eq!(log.recorded_through, Some(T + 10));
    let window = (Some((T + 20) * 1000), Some(now * 1000));
    assert!(matches!(
        log.covers(&queue.events, window.0, window.1),
        Err(MainMissing::GitUnreadable { since, .. }) if since == T + 70
    ));
    main.failing.set(false);
    now += 60;
    assert_eq!(
        queue.pass(&main, now),
        [MAIN_READ_RECOVERED, COMMITS, MAIN_OBSERVED]
    );
    let log = queue.log();
    assert_eq!(log.recorded_through, Some(now));
    assert_eq!(log.covers(&queue.events, window.0, window.1), Ok(()));
    assert_eq!(log.history, main.history(T - 1));
    assert_eq!(log.reading, MainReading::Readable);
}

/// (3) The first record takes, once, the range the readers of Git read
/// (from a second before the earliest event, conflict or not), and the
/// fold gives their history; a first record cut short is taken again and
/// folds the same. With commits that changed a file before its first
/// conflict, `conflict_hotspots` counts the same landings and ratio from
/// the fold as from Git.
#[test]
fn the_first_record_takes_the_readers_range_once() {
    let main = Main::default();
    main.land("old", T - 50, vec![change("hot.rs")]);
    main.land("a", T + 1, vec![change("hot.rs")]);
    main.land("b", T + 5, vec![change("hot.rs"), change("cold.rs")]);
    let mut queue = Queue::since(T);
    queue.pass(&main, T + 10);
    // A first record whose reach was never written is taken again.
    let mut cut = Queue::since(T);
    cut.pass(&main, T + 10);
    cut.events.retain(|event| event.kind != MAIN_OBSERVED);
    cut.pass(&main, T + 11);
    assert_eq!(cut.count(COMMITS), 2);
    assert_eq!(cut.log().history, queue.log().history);
    assert!(queue.pass(&main, T + 12).is_empty(), "taken once");
    main.land("c", T + 30, vec![change("hot.rs")]);
    queue.pass(&main, T + 31);
    for (at, run) in [(T + 40, "r1"), (T + 50, "r2")] {
        queue.push(
            "conflict_precheck",
            json!({"main": run, "conflicts": ["hot.rs"]}),
            at,
        );
        queue.events.last_mut().unwrap().run_id =
            Some(crate::domain::RunId::new(format!("{run}-run")).unwrap());
    }
    let log = queue.log();
    let git = main.history(T - 1);
    assert_eq!(log.history, git);
    let mut changed: Vec<&str> = log.changed.keys().map(String::as_str).collect();
    changed.sort_unstable();
    assert_eq!(changed, ["a", "b", "c"]);
    assert_eq!(log.changed["b"], ["cold.rs", "hot.rs"]);
    let upto = EventId::new(queue.events.len() as i64);
    let hotspots = |history: &History| {
        conflict_hotspots(
            &queue.events,
            EventId::new(0),
            upto,
            |_| true,
            history,
            ConflictConfigReport::default(),
        )
    };
    let from_git = hotspots(&History::Read(git));
    let from_record = hotspots(&log.history_for(&queue.events, EventId::new(0), upto));
    assert_eq!(from_record, from_git);
    assert_eq!(from_git.history, HistoryCheck::Checked { landings: 3 });
    assert_eq!(from_git.files[0].landings, Some(3));
    assert_eq!(MAIN_LOG_KINDS.len(), 6);
}
