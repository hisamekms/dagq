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
            MAIN_RECORD_GRACE_SECS, MAIN_REVERTS_IMPORTED, MAIN_REWRITTEN, MainLog, MainMissing,
            fold_main_log, import_heads,
        },
        revert::RevertOf,
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
    /// The line each rewrite took off, whole, by its head.
    lines: RefCell<Vec<Vec<MainCommit>>>,
    /// The commits Git no longer has (collected as garbage).
    collected: RefCell<HashSet<String>>,
    failing: Cell<bool>,
}

impl Main {
    fn land(&self, sha: &str, at: i64, changes: Vec<MainChange>) {
        self.land_reverting(sha, at, changes, RevertOf::NotRevert);
    }

    /// A commit whose message says it reverts as `reverts` says.
    fn land_reverting(&self, sha: &str, at: i64, changes: Vec<MainChange>, reverts: RevertOf) {
        self.line.borrow_mut().push(MainCommit {
            sha: sha.to_owned(),
            at,
            changes,
            reverts,
        });
    }

    /// Rewrite the line after `keep` commits, as a force push does.
    fn rewrite(&self, keep: usize) {
        let mut line = self.line.borrow_mut();
        self.lines.borrow_mut().push(line.clone());
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
    fn commits(&self, head: &str, after: Option<&str>, since: i64) -> Result<Vec<MainCommit>> {
        self.check()?;
        // The line `head` is on: main's now, or one a rewrite took off.
        let lines = self.lines.borrow();
        let now = self.line.borrow();
        let on = |line: &Vec<MainCommit>| line.iter().position(|c| c.sha == head);
        let line: Vec<MainCommit> = std::iter::once(&*now)
            .chain(lines.iter().rev())
            .find_map(|line| Some(line[..=on(line)?].to_vec()))
            .unwrap_or_else(|| now.clone());
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
    fn has(&self, commit: &str) -> Result<bool> {
        self.check()?;
        Ok(!self.collected.borrow().contains(commit)
            && (self.line.borrow().iter().any(|c| c.sha == commit)
                || self.gone.borrow().contains_key(commit)))
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
            kind: kind.into(),
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
        let recorded = self.recorded();
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

    /// What the queue holds of the record, as the supervisor reads it.
    fn recorded(&self) -> Recorded {
        let reach = self.latest(&[MAIN_OBSERVED]).and_then(Reach::of);
        let lines = if reach.as_ref().is_none_or(|reach| !reach.reverts) {
            import_heads(&self.events)
        } else {
            Vec::new()
        };
        Recorded {
            reach,
            reading: MainReading::of(self.latest(&[MAIN_READ_FAILED, MAIN_READ_RECOVERED])),
            lines,
        }
    }

    fn log(&self) -> MainLog {
        fold_main_log(&self.events)
    }

    fn count(&self, kind: &str) -> usize {
        self.events
            .iter()
            .filter(|event| event.kind.as_str() == kind)
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
                reverts: Default::default(),
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
    cut.events
        .retain(|event| event.kind != EventKind::MainObserved);
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
    assert_eq!(MAIN_LOG_KINDS.len(), 7);
}

/// The record of a queue from before the commits carried the evidence of a
/// revert: its `main_observed` has no `reverts` and its commits no field.
fn recorded_before_the_evidence(main: &Main, queue: &mut Queue, now: i64) {
    queue.pass(main, now);
    for event in &mut queue.events {
        if let Some(object) = event.payload.as_object_mut() {
            object.remove("reverts");
        }
        for commit in event.payload["commits"]
            .as_array_mut()
            .into_iter()
            .flatten()
        {
            commit.as_object_mut().unwrap().remove("reverts");
        }
    }
}

/// (2) A reach recorded before the evidence has the evidence of main's
/// line from the record's start imported by the next pass, before its
/// reach and whether or not the reach is due; no later pass imports again,
/// and a pass that stopped before its reach was written imports again.
#[test]
fn a_record_from_before_the_evidence_is_imported_once() {
    let main = Main::default();
    main.land("a", T + 1, vec![change("a.rs")]);
    main.land_reverting(
        "r",
        T + 2,
        vec![change("a.rs")],
        RevertOf::Commit("a".into()),
    );
    let mut queue = Queue::since(T);
    recorded_before_the_evidence(&main, &mut queue, T + 10);
    assert_eq!(
        Reach::of(queue.latest(&[MAIN_OBSERVED]).unwrap()).map(|r| r.reverts),
        Some(false)
    );
    // The next pass, the head at rest and the reach not due.
    assert_eq!(
        queue.pass(&main, T + 11),
        [MAIN_REVERTS_IMPORTED, MAIN_OBSERVED]
    );
    let imported = queue.latest(&[MAIN_REVERTS_IMPORTED]).unwrap();
    assert_eq!(
        imported.payload,
        json!({"heads": ["r"], "unread": [], "since": T - 1, "at": T + 11,
               "reverts": [{"sha": "r", "at": T + 2, "reverts": "a"}]})
    );
    assert!(
        Reach::of(queue.latest(&[MAIN_OBSERVED]).unwrap())
            .unwrap()
            .reverts
    );
    // Once: neither the next pass nor one with a new commit imports.
    assert!(queue.pass(&main, T + 12).is_empty());
    main.land("b", T + 20, vec![change("b.rs")]);
    assert_eq!(queue.pass(&main, T + 21), [COMMITS, MAIN_OBSERVED]);
    let commits = &queue.latest(&[COMMITS]).unwrap().payload["commits"];
    assert_eq!(commits[0]["reverts"], Value::Null);
    assert_eq!(queue.count(MAIN_REVERTS_IMPORTED), 1);

    // A pass that stopped after its import, before its reach: the next
    // pass imports again.
    let mut stopped = Queue::since(T);
    recorded_before_the_evidence(&main, &mut stopped, T + 10);
    let first = records(&stopped.recorded(), T + 11, &|| Ok(None), &main);
    assert_eq!(first[0].0, EventKind::MainRevertsImported);
    stopped.push(MAIN_REVERTS_IMPORTED, first[0].1.clone(), T + 11);
    assert_eq!(
        stopped.pass(&main, T + 12),
        [MAIN_REVERTS_IMPORTED, MAIN_OBSERVED]
    );
    assert_eq!(stopped.count(MAIN_REVERTS_IMPORTED), 2);
    assert!(stopped.pass(&main, T + 13).is_empty());
}

/// A record begun with the evidence never imports, and its commits carry
/// what each reverts.
#[test]
fn a_record_begun_with_the_evidence_imports_nothing() {
    let main = Main::default();
    main.land("a", T + 1, vec![change("a.rs")]);
    main.land_reverting(
        "r",
        T + 2,
        vec![change("a.rs")],
        RevertOf::Commit("a".into()),
    );
    let mut queue = Queue::since(T);
    queue.pass(&main, T + 10);
    assert!(
        queue
            .pass(&main, T + 10 + MAIN_OBSERVED_INTERVAL_SECS)
            .len()
            == 1
    );
    assert_eq!(queue.count(MAIN_REVERTS_IMPORTED), 0);
    let commits = &queue.latest(&[COMMITS]).unwrap().payload["commits"];
    assert_eq!(commits[0]["reverts"], Value::Null);
    assert_eq!(commits[1]["reverts"], "a");
    assert_eq!(
        queue.log().history.commits[1].reverts,
        RevertOf::Commit("a".into())
    );
}

/// (2) The import covers every line recorded, not only main's now: a
/// revert recorded before the evidence that a force push took off before
/// the first import (in the same pass, or in an earlier rewrite) is
/// imported, each once; a line whose head Git no longer has is named
/// unread. A pass that stopped before its reach imports the same again.
#[test]
fn the_import_covers_the_lines_a_rewrite_took_off() {
    let main = Main::default();
    main.land("a", T + 1, vec![change("a.rs")]);
    main.land_reverting("r1", T + 2, vec![], RevertOf::Commit("a".into()));
    let mut queue = Queue::since(T);
    queue.pass(&main, T + 10);
    // An earlier rewrite took off r1, recorded before the evidence.
    main.rewrite(1);
    main.land("b", T + 20, vec![change("b.rs")]);
    main.land_reverting("r2", T + 21, vec![], RevertOf::Commit("b".into()));
    main.land("x", T + 22, vec![]);
    queue.pass(&main, T + 30);
    assert_eq!(queue.count(MAIN_REWRITTEN), 1);
    // And the line at r2 is taken off before the first import.
    main.rewrite(2);
    main.land("c", T + 40, vec![change("c.rs")]);
    let mut before = Queue {
        events: queue.events.clone(),
    };
    for event in &mut before.events {
        if let Some(object) = event.payload.as_object_mut() {
            object.remove("reverts");
        }
        for commit in event.payload["commits"]
            .as_array_mut()
            .into_iter()
            .flatten()
        {
            commit.as_object_mut().unwrap().remove("reverts");
        }
    }
    let mut stopped = Queue {
        events: before.events.clone(),
    };
    assert_eq!(
        before.pass(&main, T + 50),
        [
            MAIN_REVERTS_IMPORTED,
            MAIN_REWRITTEN,
            COMMITS,
            "main_paths_recorded",
            MAIN_OBSERVED
        ]
    );
    let imported = &before.latest(&[MAIN_REVERTS_IMPORTED]).unwrap().payload;
    assert_eq!(imported["heads"], json!(["x", "r1"]));
    assert_eq!(imported["unread"], json!([]));
    assert_eq!(
        imported["reverts"],
        json!([
            {"sha": "r2", "at": T + 21, "reverts": "b"},
            {"sha": "r1", "at": T + 2, "reverts": "a"},
        ])
    );
    assert!(before.pass(&main, T + 51).is_empty());

    // Stopped after the import, before the reach: the next pass takes the
    // same again; the fold counts each revert once.
    let first = records(&stopped.recorded(), T + 50, &|| Ok(None), &main);
    stopped.push(MAIN_REVERTS_IMPORTED, first[0].1.clone(), T + 50);
    stopped.pass(&main, T + 52);
    assert_eq!(stopped.count(MAIN_REVERTS_IMPORTED), 2);
    let again = &stopped.latest(&[MAIN_REVERTS_IMPORTED]).unwrap().payload;
    assert_eq!(again["reverts"], first[0].1["reverts"]);

    // A head Git collected is named, not read.
    main.collected.borrow_mut().insert("r1".into());
    let mut collected = Queue {
        events: stopped.events[..before.events.len().min(stopped.events.len())].to_vec(),
    };
    collected.events.truncate(queue.events.len());
    for event in &mut collected.events {
        if let Some(object) = event.payload.as_object_mut() {
            object.remove("reverts");
        }
    }
    collected.pass(&main, T + 60);
    let imported = &collected.latest(&[MAIN_REVERTS_IMPORTED]).unwrap().payload;
    assert_eq!(imported["heads"], json!(["x"]));
    assert_eq!(imported["unread"], json!(["r1"]));
}

/// Strip the evidence of a revert from `events`, as a record from before
/// it holds them.
fn without_evidence(events: &mut [RunEvent]) {
    for event in events {
        if let Some(object) = event.payload.as_object_mut() {
            object.remove("reverts");
        }
        for commit in event.payload["commits"]
            .as_array_mut()
            .into_iter()
            .flatten()
        {
            commit.as_object_mut().unwrap().remove("reverts");
        }
    }
}

/// (2) A record a pass from before the evidence left before its reach is
/// imported too: commits recorded after the reach (a revert among them)
/// and taken off main by a rewrite before the first import, and commits
/// recorded with no reach at all. Each is imported once its line is read,
/// and no later pass imports again.
#[test]
fn a_record_left_before_its_reach_is_imported() {
    // After the reach `a`, a pass recorded `r` and `h` and stopped.
    let main = Main::default();
    main.land("a", T + 1, vec![change("a.rs")]);
    let mut queue = Queue::since(T);
    queue.pass(&main, T + 10);
    main.land_reverting("r", T + 11, vec![], RevertOf::Commit("a".into()));
    main.land("h", T + 12, vec![]);
    let left = records(&queue.recorded(), T + 20, &|| Ok(None), &main);
    assert_eq!(left[0].0, EventKind::MainCommitsRecorded);
    queue.push(COMMITS, left[0].1.clone(), T + 20);
    without_evidence(&mut queue.events);
    // Main is rewritten from before `a`.
    main.rewrite(0);
    main.land("b", T + 30, vec![change("b.rs")]);
    assert_eq!(queue.recorded().lines, ["h"]);
    let kinds = queue.pass(&main, T + 40);
    assert_eq!(kinds[0], MAIN_REVERTS_IMPORTED);
    let imported = &queue.latest(&[MAIN_REVERTS_IMPORTED]).unwrap().payload;
    assert_eq!(imported["heads"], json!(["a", "h"]));
    assert_eq!(
        imported["reverts"],
        json!([{"sha": "r", "at": T + 11, "reverts": "a"}])
    );
    assert!(queue.pass(&main, T + 41).is_empty());
    assert_eq!(queue.count(MAIN_REVERTS_IMPORTED), 1);

    // A first record left before any reach, its revert rewritten away.
    let main = Main::default();
    main.land("a", T + 1, vec![change("a.rs")]);
    main.land_reverting("r", T + 2, vec![], RevertOf::Commit("a".into()));
    let mut queue = Queue::since(T);
    let left = records(&queue.recorded(), T + 10, &|| Ok(Some(T * 1000)), &main);
    assert_eq!(left[0].0, EventKind::MainCommitsRecorded);
    queue.push(COMMITS, left[0].1.clone(), T + 10);
    without_evidence(&mut queue.events);
    main.rewrite(1);
    assert_eq!(
        queue.pass(&main, T + 20),
        [
            MAIN_REVERTS_IMPORTED,
            COMMITS,
            "main_paths_recorded",
            MAIN_OBSERVED
        ]
    );
    let imported = &queue.latest(&[MAIN_REVERTS_IMPORTED]).unwrap().payload;
    assert_eq!(imported["heads"], json!(["r"]));
    assert_eq!(imported["since"], T - 1);
    assert_eq!(
        imported["reverts"],
        json!([{"sha": "r", "at": T + 2, "reverts": "a"}])
    );
    assert!(queue.pass(&main, T + 21).is_empty());
}
