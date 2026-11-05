use serde_json::{Value, json};

use super::*;
use crate::domain::GoalId;
use crate::domain::ledger::{
    LedgerKind, LedgerRow, build,
    testkit::{Log, T0, ms, run_id},
    window::{LedgerWindow, WindowQuery, select},
};
use crate::domain::stats::Cursor;

/// Run A (task 1) lands `c1` at 900 s (its main before it `m0`), run B
/// (task 2) lands `c2` at 2900 s with an older payload of `commit` only.
fn landings() -> Log {
    let mut log = Log::default();
    log.landed(1, &run_id(1), 0).landed(2, &run_id(2), 2000);
    for event in &mut log.events {
        if event.kind.as_str() != "run_integrated" {
            continue;
        }
        if event.run_id.as_ref().map(RunId::as_str) == Some(run_id(1).as_str()) {
            event.payload["result_commit"] = json!("c1");
            event.payload["commit"] = json!("c1");
            event.payload["main_before"] = json!("m0");
        } else {
            event.payload["commit"] = json!("c2");
        }
    }
    log
}

fn commit(sha: &str, secs: i64, reverts: Option<Option<&str>>) -> Value {
    let mut commit = json!({"sha": sha, "at": T0 + secs, "changes": []});
    if let Some(reverts) = reverts {
        commit["reverts"] = json!(reverts);
    }
    commit
}

/// `commits` recorded at `secs`, and the reach to the last of them.
fn recorded(log: &mut Log, commits: Vec<Value>, secs: i64) -> &mut Log {
    let evidence = commits.iter().all(|c| c.get("reverts").is_some());
    let head = commits.last().map(|c| c["sha"].clone());
    log.add(
        None,
        None,
        "main_commits_recorded",
        json!({"after": null, "commits": commits}),
        secs,
    );
    let mut observed = json!({"head": head, "at": T0 + secs, "since": T0 - 1});
    if evidence {
        observed["reverts"] = json!(true);
    }
    log.add(None, None, "main_observed", observed, secs)
}

fn run(n: u8) -> RunId {
    RunId::new(run_id(n)).unwrap()
}

/// The window of `log` (its rows built then) to `secs`, its `--until`
/// and now, with `query`'s filters.
fn filtered(log: &Log, secs: i64, query: WindowQuery) -> LedgerWindow {
    let now_ms = ms(secs);
    let rows = build(&log.events, now_ms, "2030-01-01T00:00:00.000Z");
    select(
        rows,
        &WindowQuery {
            until: Some(Cursor::Time(now_ms)),
            now_ms,
            ..query
        },
    )
}

fn window(log: &Log, secs: i64) -> LedgerWindow {
    filtered(log, secs, WindowQuery::default())
}

/// The common read's reverts and failures of `log`'s window to `secs`.
fn book_at(log: &Log, secs: i64) -> PostLandingBook {
    window(log, secs).post_landing_book()
}

/// What befell run `n` in `log`'s window to `secs`.
fn read_at(log: &Log, secs: i64, n: u8) -> PostLanding {
    book_at(log, secs).of_run(&run(n))
}

/// (3) A revert falls on the run that landed what it reverts, by
/// `result_commit` or by `commit` of an older payload; one of the main
/// before a landing falls on no run, and neither does one of a commit no
/// run landed (on the record) or one before the record.
#[test]
fn a_revert_falls_on_the_run_that_landed_its_target() {
    let mut log = landings();
    recorded(
        &mut log,
        vec![
            commit("m0", -10, Some(None)),
            commit("c1", 900, Some(None)),
            commit("p", 1000, Some(None)),
            commit("c2", 2900, Some(None)),
            commit("r1", 3000, Some(Some("c1"))),
            commit("r2", 3100, Some(Some("c2"))),
            commit("rm", 3200, Some(Some("m0"))),
            commit("rp", 3300, Some(Some("p"))),
            commit("ro", 3400, Some(Some("old"))),
        ],
        3500,
    );
    let book = book_at(&log, 4000);
    let reasons: Vec<(&str, Option<&str>, Option<&str>)> = book
        .reverts
        .iter()
        .map(|fact| {
            (
                fact.sha.as_str(),
                fact.target.as_ref().map(|t| t.run_id.as_str()),
                fact.reason,
            )
        })
        .collect();
    assert_eq!(
        reasons,
        [
            ("r1", Some(run_id(1).as_str()), None),
            ("r2", Some(run_id(2).as_str()), None),
            ("rm", None, Some(NOT_A_LANDING)),
            ("rp", None, Some(NOT_A_LANDING)),
            ("ro", None, Some(BEFORE_RECORD)),
        ]
    );
    assert_eq!(
        book.reverts[0].target.as_ref().unwrap().task_id,
        Some(TaskId::new(1))
    );
    assert_eq!(book.reverts[0].names, "c1");
    let a = book.of_run(&run(1));
    assert_eq!(a.commit.as_deref(), Some("c1"));
    assert_eq!(a.reverts.count, 1);
    assert_eq!(a.reverts.first_at, Some(ms(3000)));
    assert_eq!(a.reverts.coverage, Coverage::Recorded);
    assert_eq!(book.of_run(&run(2)).reverts.count, 1);
}

/// (4) A revert recorded again (after a rewrite) and imported too counts
/// once, by its commit.
#[test]
fn a_revert_recorded_many_times_counts_once() {
    let mut log = landings();
    let line = || {
        vec![
            commit("c1", 900, Some(None)),
            commit("r1", 3000, Some(Some("c1"))),
        ]
    };
    recorded(&mut log, line(), 3500);
    log.add(
        None,
        None,
        "main_rewritten",
        json!({"from": "r1", "head": "x", "base": null, "at": T0 + 3600}),
        3600,
    );
    recorded(&mut log, line(), 3600);
    log.add(
        None,
        None,
        "main_reverts_imported",
        json!({"through": "r1", "since": T0 - 1, "at": T0 + 3700,
               "reverts": [{"sha": "r1", "at": T0 + 3000, "reverts": "c1"}]}),
        3700,
    );
    let book = book_at(&log, 4000);
    assert_eq!(book.reverts.len(), 1);
    assert_eq!(book.of_run(&run(1)).reverts.count, 1);
}

/// (2) Commits recorded before the evidence leave the record not begun
/// until it is imported; the import's start is where the record speaks
/// from, and a run landed before it is before the record.
#[test]
fn an_import_begins_the_record_at_its_start() {
    let mut log = landings();
    recorded(
        &mut log,
        vec![commit("c1", 900, None), commit("r1", 1000, None)],
        1100,
    );
    let before = read_at(&log, 1200, 1);
    assert_eq!(before.reverts.coverage, Coverage::NotRecorded);
    assert_eq!(before.reverts.count, 0);
    assert_eq!(before.reverts.reach_at, None);
    log.add(
        None,
        None,
        "main_reverts_imported",
        json!({"through": "r1", "since": T0 + 950, "at": T0 + 3000,
               "reverts": [{"sha": "r1", "at": T0 + 1000, "reverts": "c1"}]}),
        3000,
    );
    log.add(
        None,
        None,
        "main_observed",
        json!({"head": "c2", "at": T0 + 3000, "since": T0 + 950, "reverts": true}),
        3000,
    );
    let book = book_at(&log, 3100);
    let a = book.of_run(&run(1));
    assert_eq!(a.reverts.coverage, Coverage::BeforeRecord);
    assert_eq!(a.reverts.count, 1);
    let b = book.of_run(&run(2));
    assert_eq!(b.reverts.coverage, Coverage::Recorded);
    assert_eq!(b.reverts.count, 0);
    assert_eq!(b.reverts.reach_at, Some(ms(3000)));
    assert_eq!(b.reverts.window_end, ms(3100));
    assert_eq!(b.reverts.landed_before_end_ms, Some(ms(3100) - ms(2900)));
    assert_eq!(b.reverts.reach_before_end_ms, Some(ms(3100) - ms(3000)));
    // A record begun with the evidence speaks from its start, before any
    // commit carries it.
    let mut fresh = landings();
    fresh.add(
        None,
        None,
        "main_observed",
        json!({"head": "c2", "at": T0 + 3000, "since": T0 - 1, "reverts": true}),
        3000,
    );
    let fresh = read_at(&fresh, 3100, 1);
    assert_eq!(fresh.reverts.coverage, Coverage::Recorded);
    assert_eq!(fresh.reverts.count, 0);
    let none = book.of_run(&run(9));
    assert_eq!(none.reverts.coverage, Coverage::NotLanded);
    assert_eq!(none.landed_at, None);
}

/// (5) A turn to red of CI on a landed commit falls on its run, one on a
/// commit no run landed on none; the checks give where the record speaks
/// from and its reach.
#[test]
fn a_ci_failure_falls_on_the_run_that_landed_its_commit() {
    let mut log = landings();
    log.add(
        None,
        None,
        "ci_checked",
        json!({"sha": "c0", "branch": "main"}),
        500,
    );
    log.add(
        None,
        None,
        "ci_turned_red",
        json!({"sha": "c1", "branch": "main"}),
        1000,
    );
    log.add(
        None,
        None,
        "ci_turned_red",
        json!({"sha": "m0", "branch": "main"}),
        1100,
    );
    log.add(
        None,
        None,
        "ci_checked",
        json!({"sha": "c2", "branch": "main"}),
        3000,
    );
    let book = book_at(&log, 3100);
    assert_eq!(book.ci_failures.len(), 2);
    assert_eq!(book.ci_failures[1].reason, Some(NOT_A_LANDING));
    let a = book.of_run(&run(1));
    assert_eq!(a.ci_failures.count, 1);
    assert_eq!(a.ci_failures.first_at, Some(ms(1000)));
    assert_eq!(a.ci_failures.coverage, Coverage::Recorded);
    assert_eq!(a.ci_failures.reach_at, Some(ms(3000)));
    assert_eq!(a.ci_failures.landed_before_end_ms, Some(ms(3100) - ms(900)));
    assert_eq!(book.of_run(&run(2)).ci_failures.count, 0);
    // No revert record: not recorded, not zero of a record.
    assert_eq!(a.reverts.coverage, Coverage::NotRecorded);
}

/// (6) What arrives after the run's row is final (its end long past) is
/// read from the queue's rows: the run's row is the same before and after
/// (no input of it, so not built again), and the read gives
/// the late revert and failure with their record's reach.
#[test]
fn what_arrives_after_the_final_row_leaves_it_as_it_was() {
    let mut log = landings();
    recorded(&mut log, vec![commit("c1", 900, Some(None))], 950);
    let early = window(&log, 100_000).rows;
    let row = |rows: &[LedgerRow]| {
        rows.iter()
            .find(|row| row.kind == LedgerKind::Run && row.key == run_id(1))
            .cloned()
            .unwrap()
    };
    // Its run ended long before: what arrives later is no input of it.
    assert!(row(&early).ended_at.is_some());
    assert_eq!(read_at(&log, 100_000, 1).reverts.count, 0);
    let mut late = log.clone();
    late.add(
        None,
        None,
        "ci_checked",
        json!({"sha": "c1", "branch": "main"}),
        100_100,
    );
    late.add(
        None,
        None,
        "ci_turned_red",
        json!({"sha": "c1", "branch": "main"}),
        100_100,
    );
    recorded(
        &mut late,
        vec![commit("r1", 100_200, Some(Some("c1")))],
        100_300,
    );
    let later = window(&late, 100_400).rows;
    assert_eq!(row(&later), row(&early));
    let read = read_at(&late, 100_400, 1);
    assert_eq!(read.reverts.count, 1);
    assert_eq!(read.reverts.first_at, Some(ms(100_200)));
    assert_eq!(read.reverts.reach_at, Some(ms(100_300)));
    assert_eq!(read.ci_failures.count, 1);
    assert_eq!(read.ci_failures.reach_at, Some(ms(100_100)));
    assert!(
        build(&late.events, ms(100_400), "2030-01-01T00:00:00.000Z")
            .iter()
            .filter(|row| row.kind == LedgerKind::Queue)
            .any(|row| row
                .records
                .iter()
                .any(|e| e.kind.as_str() == "ci_turned_red"))
    );
}

/// (5) CI's failures and record are the branch the run landed on (its
/// push's): a turn to red of the same commit on another branch falls on no
/// run, and checks of another branch neither begin nor move its record;
/// once the watch moves to another branch, the run's record stays where
/// the checks of its branch left it.
#[test]
fn a_ci_failure_counts_only_on_the_branch_the_run_landed_on() {
    let mut log = landings();
    let ci = |log: &mut Log, kind: &str, sha: &str, branch: &str, secs: i64| {
        log.add(
            None,
            None,
            kind,
            json!({"sha": sha, "branch": branch}),
            secs,
        );
    };
    ci(&mut log, "ci_checked", "c0", "release", 500);
    ci(&mut log, "ci_turned_red", "c1", "release", 1000);
    let book = book_at(&log, 1100);
    assert_eq!(book.ci_failures[0].target, None);
    assert_eq!(book.ci_failures[0].reason, Some(NOT_A_LANDING));
    assert_eq!(book.ci_failures[0].branch.as_deref(), Some("release"));
    let a = book.of_run(&run(1));
    assert_eq!(a.ci_failures.count, 0);
    assert_eq!(a.ci_failures.coverage, Coverage::NotRecorded);
    assert_eq!(a.ci_failures.reach_at, None);
    // The watch on main, then moved to `release` again.
    ci(&mut log, "ci_checked", "c0", "refs/heads/main", 600);
    ci(&mut log, "ci_turned_red", "c1", "main", 1200);
    ci(&mut log, "ci_checked", "c2", "release", 5000);
    let a = read_at(&log, 5100, 1);
    assert_eq!(a.ci_failures.count, 1);
    assert_eq!(a.ci_failures.first_at, Some(ms(1200)));
    assert_eq!(a.ci_failures.coverage, Coverage::Recorded);
    assert_eq!(a.ci_failures.reach_at, Some(ms(600)));
}

/// (6) The common read is the window's: the records are read to its end,
/// so a revert and a failure after `--until` are not counted (a later
/// window counts them), and the elapsed are to the window's end.
#[test]
fn the_read_stops_at_the_windows_end() {
    let mut log = landings();
    log.add(
        None,
        None,
        "ci_checked",
        json!({"sha": "c0", "branch": "main"}),
        500,
    );
    recorded(&mut log, vec![commit("c1", 900, Some(None))], 1000);
    recorded(&mut log, vec![commit("r1", 5000, Some(Some("c1")))], 5000);
    log.add(
        None,
        None,
        "ci_turned_red",
        json!({"sha": "c1", "branch": "main"}),
        5000,
    );
    let early = read_at(&log, 4000, 1);
    assert_eq!(early.reverts.count, 0);
    assert_eq!(early.ci_failures.count, 0);
    assert_eq!(early.reverts.reach_at, Some(ms(1000)));
    assert_eq!(early.reverts.landed_before_end_ms, Some(ms(4000) - ms(900)));
    assert_eq!(early.reverts.reach_before_end_ms, Some(ms(4000) - ms(1000)));
    let later = read_at(&log, 6000, 1);
    assert_eq!(later.reverts.count, 1);
    assert_eq!(later.ci_failures.count, 1);
    assert_eq!(later.reverts.reach_before_end_ms, Some(ms(6000) - ms(5000)));
}

/// (6) The window's filters choose the runs, never the records: under
/// `--goal` the reverts of the goal's run are counted from the queue's
/// rows the filter leaves out, and the other goal's run is not given.
#[test]
fn the_filters_choose_the_runs_and_keep_the_records() {
    let mut log = Log::default();
    log.add(
        None,
        None,
        "attributes_baseline",
        json!({"tasks": 0, "goals": 0}),
        -20,
    );
    for (task, goal) in [(1, 3), (2, 4)] {
        log.add(
            Some(task),
            None,
            "task_created",
            json!({"goal_id": goal, "status": "ready", "title": "t", "change": "runtime",
                   "execution_class": "implementation", "priority": null, "priority_by": null}),
            -10,
        );
    }
    let landed = landings();
    log.events
        .extend(landed.events.into_iter().map(|mut event| {
            event.id = crate::domain::EventId::new(event.id.as_i64() + 3);
            event
        }));
    recorded(
        &mut log,
        vec![
            commit("c1", 900, Some(None)),
            commit("c2", 2900, Some(None)),
            commit("r1", 3000, Some(Some("c1"))),
            commit("r2", 3100, Some(Some("c2"))),
        ],
        3200,
    );
    let goal = filtered(
        &log,
        4000,
        WindowQuery {
            goal_id: Some(GoalId::new(3)),
            ..WindowQuery::default()
        },
    );
    assert!(
        goal.rows.iter().all(|row| row
            .records
            .iter()
            .all(|event| event.kind.as_str() != "main_commits_recorded")),
        "the filter leaves the record of main's rows out"
    );
    let read = goal.post_landing();
    assert_eq!(read.len(), 1, "{read:?}");
    assert_eq!(read[0].run_id, run(1));
    assert_eq!(read[0].reverts.count, 1);
    assert_eq!(read[0].reverts.coverage, Coverage::Recorded);
    assert_eq!(window(&log, 4000).post_landing().len(), 2);
}

/// (2) An import that could not read a line keeps the reverts of the runs
/// landed before it unknown, apart from a complete record; a run landed
/// after it is recorded.
#[test]
fn an_unread_line_leaves_the_runs_before_it_incomplete() {
    let mut log = landings();
    let import = |log: &mut Log, unread: Value| {
        log.add(
            None,
            None,
            "main_reverts_imported",
            json!({"heads": ["c1"], "unread": unread, "since": T0 - 1, "at": T0 + 2000,
                   "reverts": []}),
            2000,
        );
        log.add(
            None,
            None,
            "main_observed",
            json!({"head": "c1", "at": T0 + 2000, "since": T0 - 1, "reverts": true}),
            2000,
        );
    };
    let mut complete = log.clone();
    import(&mut complete, json!([]));
    let book = book_at(&complete, 4000);
    assert_eq!(book.unread, Vec::<String>::new());
    assert_eq!(book.of_run(&run(1)).reverts.coverage, Coverage::Recorded);
    import(&mut log, json!(["gone"]));
    let book = book_at(&log, 4000);
    assert_eq!(book.unread, ["gone"]);
    let a = book.of_run(&run(1));
    assert_eq!(a.reverts.coverage, Coverage::Incomplete);
    assert_eq!(a.reverts.count, 0);
    // CI's record is apart from main's.
    assert_ne!(a.ci_failures.coverage, Coverage::Incomplete);
    assert_eq!(book.of_run(&run(2)).reverts.coverage, Coverage::Recorded);
}
