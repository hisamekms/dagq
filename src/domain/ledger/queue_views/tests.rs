//! The views of the queue rows hold what the whole read of the same events
//! gives: the tasks each snapshot names, main's commits and their reverts'
//! targets, main's record and CI's checks up to each summary, and the
//! run-less asks no answer closed.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::*;
use crate::domain::ledger::build;
use crate::domain::marks::utc_text;
use crate::domain::stats::conflicts::{MainChange, MainCommit};
use crate::domain::stats::main_log::{commits_payloads, fold_main_log, paths_payloads};
use crate::domain::stats::revert::RevertOf;

const T0_MS: i64 = 1_900_000_000_000;

/// The run-less events of a queue, ascending id and time.
#[derive(Default)]
pub(crate) struct Queue(pub(crate) Vec<RunEvent>);

impl Queue {
    pub(crate) fn add(&mut self, kind: &str, payload: Value) -> EventId {
        let id = EventId::new(i64::try_from(self.0.len()).unwrap() + 1);
        self.0.push(RunEvent {
            id,
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: utc_text(T0_MS + id.as_i64() * 1000),
            actor: None,
        });
        id
    }

    pub(crate) fn snapshot(&mut self, tasks: &[i64]) -> EventId {
        let tasks: Vec<Value> = tasks.iter().map(|id| json!({"id": id})).collect();
        self.add(
            "forecast_recorded",
            json!({"supervisor": "s", "at_secs": 1, "tasks": tasks, "goals": []}),
        )
    }

    /// Commits after `after`, each `(sha, file added, file deleted,
    /// reverts)`.
    pub(crate) fn commits(
        &mut self,
        after: Option<&str>,
        commits: &[(&str, &str, Option<&str>, Option<&str>)],
    ) {
        let commits: Vec<MainCommit> = commits
            .iter()
            .enumerate()
            .map(|(n, (sha, added, deleted, reverts))| MainCommit {
                sha: (*sha).to_owned(),
                at: 1_900_000_000 + i64::try_from(n).unwrap(),
                changes: std::iter::once(MainChange {
                    path: (*added).to_owned(),
                    from: None,
                    deleted: false,
                })
                .chain(deleted.map(|path| MainChange {
                    path: path.to_owned(),
                    from: None,
                    deleted: true,
                }))
                .collect(),
                reverts: reverts.map_or(RevertOf::NotRevert, |sha| RevertOf::Commit(sha.into())),
            })
            .collect();
        for payload in commits_payloads(after, &commits) {
            self.add("main_commits_recorded", payload);
        }
    }

    pub(crate) fn paths(&mut self, head: &str, paths: &[&str]) {
        let paths: Vec<String> = paths.iter().map(|path| (*path).to_owned()).collect();
        for payload in paths_payloads(head, 1_800_000_000, &paths) {
            self.add("main_paths_recorded", payload);
        }
    }

    pub(crate) fn observed(&mut self, head: &str, at: i64) {
        self.add(
            "main_observed",
            json!({"head": head, "at": at, "since": 1_800_000_000, "reverts": true}),
        );
    }

    pub(crate) fn ci(&mut self, branch: &str, sha: &str) {
        self.add(
            "ci_checked",
            json!({"branch": branch, "sha": sha, "conclusion": "success"}),
        );
    }

    pub(crate) fn red(&mut self, branch: &str, sha: &str) {
        self.add("ci_turned_red", json!({"branch": branch, "sha": sha}));
    }

    pub(crate) fn rows(&self) -> Vec<LedgerRow> {
        let now = T0_MS + i64::try_from(self.0.len()).unwrap() * 1000 + 60_000;
        build(&self.0, now, "2030-01-01T00:00:00.000Z")
            .into_iter()
            .filter(|row| row.kind == LedgerKind::Queue)
            .collect()
    }
}

/// What the fold of every event up to `upto` gives of what a summary
/// holds: the start, reach, head, reading, paths, last commit, and CI's
/// first and last check of each branch as [`super::super::post_landing`]
/// keys them.
fn whole(events: &[RunEvent], upto: EventId) -> MainSummary {
    let upto: Vec<RunEvent> = events
        .iter()
        .filter(|event| event.id <= upto)
        .cloned()
        .collect();
    let log = fold_main_log(&upto);
    let mut ci: BTreeMap<String, Checks> = BTreeMap::new();
    let at = |event: &RunEvent| timestamp_millis(&event.created_at).unwrap();
    for event in upto
        .iter()
        .filter(|event| event.kind == EventKind::CiChecked)
    {
        let branch = branch_name(event.payload["branch"].as_str().unwrap());
        let checks = ci.entry(branch).or_insert_with(|| Checks {
            earliest: event.clone(),
            latest: event.clone(),
        });
        // The least and the greatest time, as post_landing's record takes
        // them: the first of the least, the last of the greatest in id.
        if at(event) < at(&checks.earliest) {
            checks.earliest = event.clone();
        }
        if at(event) >= at(&checks.latest) {
            checks.latest = event.clone();
        }
    }
    MainSummary {
        since: log.since,
        head: log.head,
        recorded_through: log.recorded_through,
        commits: log
            .history
            .commits
            .iter()
            .map(|commit| commit.sha.clone())
            .collect(),
        paths: log
            .paths_known
            .then(|| log.history.paths.into_iter().collect()),
        reading: upto
            .iter()
            .rev()
            .find(|event| {
                matches!(
                    event.kind.known(),
                    Some(EventKind::MainReadFailed | EventKind::MainReadRecovered)
                )
            })
            .cloned(),
        ci,
        ..MainSummary::default()
    }
}

/// `summary` with only the values [`whole`] gives.
fn values(summary: &MainSummary) -> MainSummary {
    MainSummary {
        since: summary.since,
        head: summary.head.clone(),
        recorded_through: summary.recorded_through,
        commits: summary.commits.clone(),
        paths: summary.paths.clone(),
        reading: summary.reading.clone(),
        ci: summary.ci.clone(),
        ..MainSummary::default()
    }
}

/// Main's record with a base in two parts, commits that add and delete
/// files, a revert, a commit recorded twice, CI checks of two branches
/// (a full ref and its short name alike), a failure of Git and its
/// recovery, and CI turning red on one sha and again on another.
pub(crate) fn main_log(units: usize) -> Queue {
    let mut queue = Queue::default();
    let many: Vec<String> = (0..1500).map(|n| format!("src/f{n}.rs")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    queue.paths("c0", &many);
    queue.observed("c0", 1_900_000_000);
    queue.ci("refs/heads/main", "c0");
    queue.ci("release", "c0");
    queue.commits(Some("c0"), &[("c1", "a.rs", Some("src/f1.rs"), None)]);
    queue.red("main", "c1");
    queue.commits(Some("c1"), &[("c2", "b.rs", None, Some("c1"))]);
    // c2 recorded again: counted once, its change applied once.
    queue.commits(Some("c1"), &[("c2", "b.rs", None, Some("c1"))]);
    queue.red("main", "c2");
    queue.add("main_read_failed", json!({"reason": "git", "at": 1}));
    queue.ci("main", "c2");
    queue.add(
        "main_read_recovered",
        json!({"reason": "git", "since": 1, "at": 2}),
    );
    for k in 0..units {
        let sha = format!("u{k}");
        let before = if k == 0 {
            "c2".to_owned()
        } else {
            format!("u{}", k - 1)
        };
        let added = format!("u{k}.rs");
        let deleted = (k > 0).then(|| format!("u{}.rs", k - 1));
        queue.commits(
            Some(&before),
            &[(
                &sha,
                &added,
                deleted.as_deref(),
                (k % 2 == 1).then_some(before.as_str()),
            )],
        );
        queue.observed(&sha, 1_900_000_100 + i64::try_from(k).unwrap());
        if k % 2 == 0 {
            queue.ci("release", &sha);
        }
    }
    queue.ci("main", "last");
    queue
}

#[test]
fn each_summary_holds_what_the_fold_of_every_event_up_to_it_gives() {
    let queue = main_log(3);
    let mut summary = MainSummary::default();
    let changes = commit_changes(&queue.0);
    let mut compared = 0;
    for event in queue.0.iter().filter(|event| summarized(event)) {
        summary = summary.then(event, &|sha| changes.get(sha).cloned());
        assert_eq!(
            values(&summary),
            whole(&queue.0, event.id),
            "at {}",
            event.id
        );
        assert_eq!(summary.through, Some(event.id));
        compared += 1;
    }
    assert!(compared > 15, "{compared}");
    // The base in two parts is known once both are recorded, c1's delete
    // and c2's add applied once; CI's record keeps both branches.
    let paths = summary.paths.as_ref().unwrap();
    assert!(!paths.contains("src/f1.rs") && paths.contains("b.rs"));
    assert_eq!(summary.ci.keys().collect::<Vec<_>>(), ["main", "release"]);
    assert_eq!(summary.ci["main"].latest.payload["sha"], "last");
    assert_eq!(summary.ci["release"].earliest.payload["sha"], "c0");
    assert_eq!(summary.ci["release"].latest.payload["sha"], "u2");
    assert_eq!(summary.reading(), MainReading::Readable);
    assert_eq!(summary.last_commit(), Some("u2"));
}

#[test]
fn the_summaries_kept_are_spaced_and_the_last_is_that_of_every_event() {
    let units = usize::try_from(SUMMARY_EVERY).unwrap();
    let queue = main_log(units);
    let kept = summaries(&queue.0);
    let folded = queue.0.iter().filter(|event| summarized(event)).count();
    assert_eq!(kept.len(), folded.div_ceil(units));
    for summary in &kept[..kept.len() - 1] {
        assert!(summary.kept());
    }
    let last = kept.last().unwrap();
    assert_eq!(last.through, queue.0.last().map(|event| event.id));
    assert_eq!(values(last), whole(&queue.0, last.through.unwrap()));
    for summary in &kept {
        assert_eq!(values(summary), whole(&queue.0, summary.through.unwrap()));
    }
}

#[test]
fn a_rewrite_leaves_the_paths_unknown_until_the_next_base() {
    let mut queue = main_log(1);
    queue.add(
        "main_rewritten",
        json!({"from": "u0", "head": "x1", "base": "c2", "at": 3}),
    );
    let rewritten = queue.0.last().unwrap().id;
    queue.paths("x1", &["x.rs"]);
    let kept = summaries(&queue.0);
    let mut summary = MainSummary::default();
    let changes = commit_changes(&queue.0);
    for event in queue.0.iter().filter(|event| summarized(event)) {
        summary = summary.then(event, &|sha| changes.get(sha).cloned());
        if event.id == rewritten {
            assert_eq!(summary.paths, None);
            assert_eq!(values(&summary), whole(&queue.0, event.id));
        }
    }
    assert_eq!(
        values(kept.last().unwrap()),
        whole(&queue.0, queue.0.last().unwrap().id)
    );
}

#[test]
fn snapshots_name_their_tasks_and_commits_their_reverts() {
    let mut queue = Queue::default();
    let first = queue.snapshot(&[1, 2, 3]);
    let later = queue.snapshot(&[2]);
    queue.commits(
        None,
        &[("a", "a.rs", None, None), ("b", "b.rs", None, Some("a"))],
    );
    let rows = queue.rows();
    let event = |id: EventId| queue.0.iter().find(|event| event.id == id).unwrap();
    assert_eq!(snapshot_targets(event(first)), BTreeSet::from([1, 2, 3]));
    assert_eq!(snapshot_targets(event(later)), BTreeSet::from([2]));
    let commits: BTreeSet<Commit> = rows
        .iter()
        .flat_map(|row| row.records.iter().flat_map(main_commits))
        .collect();
    assert_eq!(
        commits,
        BTreeSet::from([
            Commit {
                sha: "a".into(),
                reverts: None
            },
            Commit {
                sha: "b".into(),
                reverts: Some("a".into())
            },
        ])
    );
    assert!(rows.iter().all(takes));
}

#[test]
fn the_run_less_asks_left_open_are_those_no_answer_of_the_rows_closes() {
    let mut queue = Queue::default();
    queue.add("ask_opened", json!({"ask_id": 1}));
    queue.add("ask_opened", json!({"ask_id": "two"}));
    queue.add("ask_answered", json!({"ask_id": 1}));
    // An answer before its ask closes it too.
    queue.add("ask_answered", json!({"id": 3}));
    queue.add("ask_opened", json!({"id": 3}));
    let open = queue.add("ask_opened", json!({"ask_id": 4}));
    let rows = queue.rows();
    let left = asks_left_open(&rows);
    let two = queue.0[1].id.to_string();
    assert_eq!(
        left,
        BTreeMap::from([
            (two, BTreeSet::from([Left::Ask(Some("two".into()))])),
            (
                open.to_string(),
                BTreeSet::from([Left::Ask(Some("4".into()))])
            ),
        ])
    );
    // A later pass answers ask 4: it leaves the table.
    queue.add("ask_answered", json!({"ask_id": 4}));
    let left = asks_left_open(&queue.rows());
    assert_eq!(left.len(), 1);
    // An ask of a run is its run row's.
    let mut asked = queue.0[0].clone();
    asked.run_id = Some(crate::domain::RunId::new("00000001-4444-4444-8444-444444444444").unwrap());
    assert_eq!(ask(&asked), None);
}

#[test]
fn a_reader_takes_the_views_only_for_a_mark_done_through_the_leader_s_generation() {
    use crate::domain::ledger::open::{Filled, Kept};
    let leader = 7;
    let through = || Filled::Through("40".into());
    for (mark, held) in [
        (None, Filled::Not),
        (Some((Filled::Done, None)), Filled::Not),
        (Some((Filled::Done, Some(6))), Filled::Not),
        (Some((Filled::Done, Some(8))), Filled::Not),
        (Some((through(), Some(7))), through()),
        (Some((Filled::Done, Some(7))), Filled::Done),
    ] {
        assert_eq!(Filled::held(mark.clone(), leader), held, "{mark:?}");
    }
    assert!(!Filled::held(Some((through(), Some(7))), leader).is_done());
    for (mark, kept) in [
        (None, Kept::Held(Filled::Not)),
        (Some((Filled::Done, Some(7))), Kept::Held(Filled::Done)),
        (Some((Filled::Done, Some(6))), Kept::CarriedOn(Filled::Done)),
        (Some((through(), Some(6))), Kept::CarriedOn(through())),
        (Some((Filled::Done, Some(5))), Kept::Behind),
        (Some((Filled::Done, None)), Kept::Behind),
    ] {
        assert_eq!(Filled::kept(mark.clone(), leader), kept, "{mark:?}");
    }
}

/// Each summary of `queue` against the fold of every event up to it.
fn every_summary_as_the_fold(queue: &Queue) {
    let changes = commit_changes(&queue.0);
    let mut summary = MainSummary::default();
    for event in queue.0.iter().filter(|event| summarized(event)) {
        summary = summary.then(event, &|sha| changes.get(sha).cloned());
        assert_eq!(
            values(&summary),
            whole(&queue.0, event.id),
            "at {}",
            event.id
        );
    }
    let last = summaries(&queue.0).pop().unwrap();
    assert_eq!(values(&last), values(&summary));
}

#[test]
fn a_commit_a_rewrite_took_off_and_recorded_again_counts_again() {
    let mut queue = Queue::default();
    queue.paths("c0", &["a.rs"]);
    queue.commits(Some("c0"), &[("c1", "b.rs", Some("a.rs"), None)]);
    queue.add(
        "main_rewritten",
        json!({"from": "c1", "head": "c0", "base": "c0", "at": 3}),
    );
    queue.paths("c0", &["a.rs"]);
    // c1 again after the rewrite: its change and its place as the last
    // commit come back, as the fold of every event has them.
    queue.commits(Some("c0"), &[("c1", "b.rs", Some("a.rs"), None)]);
    every_summary_as_the_fold(&queue);
    let last = summaries(&queue.0).pop().unwrap();
    assert_eq!(last.last_commit(), Some("c1"));
    assert_eq!(last.paths, Some(BTreeSet::from(["b.rs".to_owned()])));
    // A rewrite to a base taken off before: no commit is left.
    queue.add(
        "main_rewritten",
        json!({"from": "c1", "head": "x", "base": "gone", "at": 4}),
    );
    every_summary_as_the_fold(&queue);
    assert_eq!(summaries(&queue.0).pop().unwrap().last_commit(), None);
}

#[test]
fn a_base_completed_after_commits_past_its_head_takes_their_changes() {
    let mut queue = Queue::default();
    // c0 recorded, then c1 past it, then c0's base (another supervisor's,
    // read later) completes in two parts.
    queue.commits(None, &[("c0", "a.rs", None, None)]);
    queue.commits(Some("c0"), &[("c1", "b.rs", Some("x0.rs"), None)]);
    let many: Vec<String> = (0..1200).map(|n| format!("x{n}.rs")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    queue.paths("c0", &many);
    every_summary_as_the_fold(&queue);
    let paths = summaries(&queue.0).pop().unwrap().paths.unwrap();
    assert!(paths.contains("b.rs") && !paths.contains("x0.rs"));
}

/// CI's record of a run's branch read of the summary's checks gives what
/// [`crate::domain::ledger::post_landing::fold`] gives of every check, when
/// the checks' times go back and forth against their ids: the record
/// begins at the least time and reaches the greatest, not those of the
/// first and the last check in id.
#[test]
fn ci_s_record_of_a_branch_read_of_the_summary_is_that_of_every_check() {
    use crate::domain::ledger::post_landing;
    use crate::domain::ledger::testkit::{Log, T0, run_id};
    let mut log = Log::default();
    log.landed(1, &run_id(1), 0);
    for event in &mut log.events {
        if event.kind == EventKind::RunIntegrated {
            event.payload["result_commit"] = json!("c1");
        }
    }
    // Checks of main (the run's push's branch) in id order at 2000, 1000,
    // 4000 and 3000 s, one of another branch, and a turn to red on c1.
    for secs in [2000, 1000, 4000, 3000] {
        log.add(
            None,
            None,
            "ci_checked",
            json!({"sha": "c1", "branch": "refs/heads/main"}),
            secs,
        );
    }
    log.add(
        None,
        None,
        "ci_checked",
        json!({"sha": "x", "branch": "release"}),
        2500,
    );
    log.add(
        None,
        None,
        "ci_turned_red",
        json!({"sha": "c1", "branch": "main"}),
        3500,
    );
    let summary = summaries(&log.events).pop().unwrap();
    let at = |event: &RunEvent| timestamp_millis(&event.created_at).unwrap();
    let main = &summary.ci["main"];
    assert_eq!(
        (at(&main.earliest), at(&main.latest)),
        ((T0 + 1000) * 1000, (T0 + 4000) * 1000)
    );
    // Every event but the checks, and the summary's checks in their place.
    let mut read: Vec<RunEvent> = log
        .events
        .iter()
        .filter(|event| event.kind != EventKind::CiChecked)
        .cloned()
        .chain(
            summary
                .ci
                .values()
                .flat_map(|checks| [checks.earliest.clone(), checks.latest.clone()]),
        )
        .collect();
    read.sort_by_key(|event| event.id);
    read.dedup_by_key(|event| event.id);
    let run = crate::domain::RunId::new(run_id(1)).unwrap();
    let end = (T0 + 5000) * 1000;
    let whole = post_landing::fold(&log.events, end).of_run(&run);
    assert_eq!(post_landing::fold(&read, end).of_run(&run), whole);
    assert_eq!(whole.ci_failures.reach_at, Some((T0 + 4000) * 1000));
    assert_eq!(whole.ci_failures.count, 1);
}

/// A run-less ask pairs with an answer by [`AskRef::key`], whichever comes
/// first: `ask_id` when the key is there whatever its value (text as it
/// is, a number, `true`, `null`, an object or an array as its JSON text),
/// else `id`; none of either pairs with none. Keys alike in text but not in
/// value do not pair.
#[test]
fn a_run_less_ask_pairs_by_its_key_of_any_value() {
    let keys = [
        (json!({"ask_id": "abc"}), Some("abc")),
        (json!({"ask_id": 12}), Some("12")),
        (json!({"ask_id": 1.5}), Some("1.5")),
        (json!({"ask_id": true}), Some("true")),
        (json!({"ask_id": null, "id": 5}), Some("null")),
        (json!({"id": 7}), Some("7")),
        (json!({}), None),
        (
            json!({"ask_id": {"b": 1, "a": [1, "x"]}}),
            Some(r#"{"a":[1,"x"],"b":1}"#),
        ),
        (
            json!({"ask_id": [2, {"c": null}]}),
            Some(r#"[2,{"c":null}]"#),
        ),
    ];
    let mut queue = Queue::default();
    for (payload, key) in &keys {
        queue.add("ask_answered", payload.clone());
        assert_eq!(
            ask(queue.0.last().unwrap()),
            Some(Ask::Answered(key.map(str::to_owned))),
            "{payload}"
        );
    }
    for (payload, _) in &keys {
        queue.add("ask_opened", payload.clone());
    }
    // Of another key: `null`'s answer is not `id` 5's, `1` is not `true`,
    // and an object or an array of other members is another.
    let others = [
        json!({"id": 5}),
        json!({"ask_id": 1}),
        json!({"ask_id": "12x"}),
        json!({"ask_id": {"b": 1, "a": [1]}}),
        json!({"ask_id": [2]}),
    ];
    let first_other = queue.0.len() + 1;
    for payload in &others {
        queue.add("ask_opened", payload.clone());
    }
    let left = asks_left_open(&queue.rows());
    let open: BTreeSet<i64> = left.keys().map(|key| key.parse().unwrap()).collect();
    let expected: BTreeSet<i64> = (first_other..first_other + others.len())
        .map(|id| i64::try_from(id).unwrap())
        .collect();
    assert_eq!(open, expected, "{left:?}");
}

/// Main's fold and what befell the landings, read from each summary with
/// the events a read from a time gives after it (those of the kinds the
/// views read after the summary, the kinds read whole, and every commit's
/// record before it as the seeks by sha may give them), are those of every
/// event: the commits with their times and changes, the paths, the reach,
/// the reading, the evidence of the reverts and CI's record of each
/// branch, with a rewrite and an import of the reverts among them.
#[test]
fn the_folds_from_each_summary_and_the_events_after_it_are_those_of_every_event() {
    use crate::domain::ledger::post_landing;
    use crate::domain::ledger::window::queue_named::SUMMARIZED_KINDS;
    use crate::domain::stats::main_log::fold_main_log_from;
    let mut queue = main_log(5);
    queue.add(
        "main_reverts_imported",
        json!({"since": 1_799_000_000, "at": 3, "heads": ["u4"], "reverts": [], "unread": []}),
    );
    queue.add(
        "main_rewritten",
        json!({"from": "u4", "head": "u3", "base": "u3", "at": 4}),
    );
    queue.paths("u3", &["u3.rs", "a.rs"]);
    queue.commits(Some("u3"), &[("w1", "w1.rs", Some("u3.rs"), Some("c2"))]);
    queue.observed("w1", 1_900_000_900);
    queue.ci("main", "w1");
    // A run that landed c2 on main, read in the range.
    let id = EventId::new(i64::try_from(queue.0.len()).unwrap() + 1);
    let run = crate::domain::RunId::new("00000001-1111-4111-8111-111111111111").unwrap();
    for (n, (kind, payload)) in [
        ("run_integrated", json!({"result_commit": "c2"})),
        ("push_finished", json!({"branch": "main"})),
    ]
    .into_iter()
    .enumerate()
    {
        let id = EventId::new(id.as_i64() + i64::try_from(n).unwrap());
        queue.0.push(RunEvent {
            id,
            task_id: None,
            goal_id: None,
            run_id: Some(run.clone()),
            kind: kind.into(),
            payload,
            created_at: utc_text(T0_MS + id.as_i64() * 1000),
            actor: None,
        });
    }
    let events = &queue.0;
    let end = T0_MS + 1_000_000_000;
    let (log, book) = (fold_main_log(events), post_landing::fold(events, end));
    assert!(book.of_run(&run).reverts.count > 0);
    let changes = commit_changes(events);
    let mut summary = MainSummary::default();
    for event in events.iter().filter(|event| summarized(event)) {
        summary = summary.then(event, &|sha| changes.get(sha).cloned());
        assert!(summary.holds_record());
        let through = event.id;
        let read_after = |e: &RunEvent| {
            e.id > through
                || e.kind == EventKind::MainCommitsRecorded
                || !SUMMARIZED_KINDS.iter().any(|kind| e.kind == *kind)
        };
        let given: Vec<RunEvent> = events.iter().filter(|e| read_after(e)).cloned().collect();
        assert_eq!(
            fold_main_log_from(Some(&summary), &given),
            log,
            "at {through}"
        );
        assert_eq!(
            post_landing::fold_from(Some(&summary), &given, end),
            book,
            "at {through}"
        );
    }
    // A summary of a binary before the record holds none, and neither
    // does the next folded on it.
    let mut old = summary.clone();
    old.record = None;
    assert!(!old.holds_record());
    let next = old.then(&events[0], &|_| None);
    assert!(!next.holds_record());
    let mut old = serde_json::to_value(&summary).unwrap();
    old["record"].as_object_mut().unwrap().remove("ci_by_time");
    let old: MainSummary = serde_json::from_value(old).unwrap();
    assert!(!old.holds_record());
    assert!(!old.then(&events[0], &|_| None).holds_record());
    assert!(MainSummary::default().holds_record());
}

#[test]
fn ci_s_clock_does_not_move_main_s_summary_back() {
    let mut queue = Queue::default();
    queue.ci("refs/heads/release", "late");
    queue.0[0].created_at = utc_text(T0_MS + 10_000);
    queue.observed("base", 1_900_000_002);
    queue.ci("release", "earlier");
    queue.ci("main", "base");
    let mut summary = summaries(&queue.0).pop().unwrap();
    assert!(summary.folds_by(T0_MS + 4_000));
    assert!(!summary.folds_by(T0_MS + 1_000));
    assert_eq!(
        summary.take_late_ci(T0_MS + 4_000),
        BTreeSet::from(["release".to_owned(), "refs/heads/release".to_owned()])
    );
    assert!(!summary.ci.contains_key("release"));
    assert!(summary.ci.contains_key("main"));
    assert!(summary.take_late_ci(T0_MS + 4_000).is_empty());
    // A summary of checks alone has no main clock bound.
    let summary = summaries(&queue.0[..1]).pop().unwrap();
    assert!(summary.folds_by(T0_MS));
}

/// The first check after a future main event supplies the missing main
/// state. Further checks advance it without rebuilding main's prefix.
#[test]
fn ci_alone_after_a_future_main_event_keeps_a_state_before_it() {
    use crate::domain::ledger::post_landing;
    use crate::domain::stats::main_log::fold_main_log_from;
    let mut queue = main_log(2);
    queue.observed("future", 1_900_001_000);
    let future = T0_MS + 1_000_000;
    queue.0.last_mut().unwrap().created_at = utc_text(future);
    let before_checks = queue.0.len();
    for _ in 0..3 {
        queue.ci("release", "base");
    }
    let mut summary = summaries(&queue.0[..before_checks]).pop().unwrap();
    for (n, check) in queue.0[before_checks..].iter().enumerate() {
        assert_eq!(summary.clock_went_back(check), n == 0);
        summary = summaries(&queue.0[..before_checks + n + 1]).pop().unwrap();
        let end = timestamp_millis(&check.created_at).unwrap();
        let seed = summary.clone().at_time(end).unwrap();
        assert_eq!(seed.through, Some(check.id));
        assert_eq!(seed.ci["release"].latest.id, check.id);
        let eligible: Vec<RunEvent> = queue.0[..before_checks + n + 1]
            .iter()
            .filter(|event| timestamp_millis(&event.created_at).unwrap() <= end)
            .cloned()
            .collect();
        assert_eq!(
            fold_main_log_from(Some(&seed), &eligible),
            fold_main_log(&eligible)
        );
        assert_eq!(
            post_landing::fold_from(Some(&seed), &eligible, end),
            post_landing::fold(&eligible, end)
        );
    }
}

/// Every main kind can carry a future timestamp before ordinary history.
/// Alternatives must fold the eligible id subsequence, including paths
/// and rewrites, rather than just repair the last observation's fields.
#[test]
fn a_main_clock_regression_keeps_the_eligible_state_of_every_main_kind() {
    use crate::domain::ledger::post_landing;
    use crate::domain::stats::main_log::fold_main_log_from;
    let kinds = [
        "main_commits_recorded",
        "main_paths_recorded",
        "main_observed",
        "main_read_failed",
        "main_read_recovered",
        "main_rewritten",
    ];
    for kind in kinds {
        let mut queue = main_log(4);
        queue.add("main_rewritten", json!({"base": "c1", "head": "new"}));
        queue.paths("c1", &["new.rs"]);
        queue.observed("new", 1_900_000_999);
        let future = T0_MS + 1_000_000;
        queue
            .0
            .iter_mut()
            .find(|event| event.kind.as_str() == kind)
            .unwrap()
            .created_at = utc_text(future);
        let summary = summaries(&queue.0).pop().unwrap();
        assert!(!summary.folds_by(future - 1), "{kind}");
        assert!(!summary.before_times.is_empty(), "{kind}");
        let seed = summary.clone().at_time(future - 1).unwrap();
        let eligible: Vec<RunEvent> = queue
            .0
            .iter()
            .filter(|event| timestamp_millis(&event.created_at).unwrap() < future)
            .cloned()
            .collect();
        assert_eq!(
            fold_main_log_from(Some(&seed), &eligible),
            fold_main_log(&eligible),
            "{kind}"
        );
        assert_eq!(
            post_landing::fold_from(Some(&seed), &eligible, future - 1),
            post_landing::fold(&eligible, future - 1),
            "{kind}"
        );
        assert_eq!(
            summary.at_time(future).unwrap().commits,
            summaries(&queue.0).pop().unwrap().commits
        );
    }
}

/// The commits past a base's head and those of a window's times are those
/// of the summary's history.
#[test]
fn a_summary_names_its_commits_past_a_head_and_from_a_time() {
    let queue = main_log(3);
    let last = summaries(&queue.0).pop().unwrap();
    assert_eq!(last.commits_past("u0"), ["u1", "u2"]);
    assert!(last.commits_past("gone").is_empty());
    // main_log's commits are each at 1_900_000_000 (the first of a record).
    assert_eq!(
        last.commits_from(1_900_000_000_000).len(),
        last.commits.len()
    );
    assert!(last.commits_from(1_900_000_001_000).is_empty());
}
