//! A person's planner the supervisor closes once its agent exited
//! (ADR-t1300-1): past the grace, its workspace and row are closed and
//! `planner_closed` is recorded; within the grace, a lost wrapper, a live
//! planner, or a listing cmux fails to give, closes nothing.

use crate::common;
use crate::runtime_support::*;

use dagq::domain::{PlannerId, PlannerOrigin};
use serde_json::Value;

/// Move planner `id`'s recorded exit `secs` back, as if its agent exited
/// that long ago.
fn exited_ago(db: &std::path::Path, id: PlannerId, secs: i64) {
    rusqlite::Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE planners SET exited_at = exited_at - ?2 WHERE id = ?1",
            rusqlite::params![id.as_i64(), secs],
        )
        .unwrap();
}

/// Supervisor options for one pass with no sweep in it, so only the pass
/// closes a planner.
fn options() -> dagq::runtime::SuperviseOptions {
    dagq::runtime::SuperviseOptions {
        sweep_interval: std::time::Duration::from_secs(3600),
        ..supervise_options(4, true)
    }
}

#[test]
fn the_supervisor_closes_a_persons_planner_once_its_agent_exited_past_the_grace() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let dead_pid = {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    };
    let me = std::process::id();
    let record = |workspace: &str, pid| {
        let planner = queue.open_planner(PlannerOrigin::Person, None).unwrap();
        queue
            .planner_workspace_created(planner.id, workspace)
            .unwrap();
        queue.register_planner_wrapper(planner.id, pid).unwrap();
        queue.register_planner_agent(planner.id, pid, pid).unwrap();
        planner.id
    };
    let past = record("W-PAST", me);
    let fresh = record("W-FRESH", me);
    let lost = record("W-LOST", dead_pid);
    let alive = record("W-ALIVE", me);
    queue.planner_exited(past, me, 0).unwrap();
    queue.planner_exited(fresh, me, 0).unwrap();
    exited_ago(&db, past, 61);
    let open = || -> Vec<PlannerId> {
        queue
            .planners(false)
            .unwrap()
            .into_iter()
            .map(|planner| planner.id)
            .collect()
    };
    let closes = || -> Vec<Value> {
        queue
            .latest_events_of("planner_closed", 10)
            .unwrap()
            .into_iter()
            .map(|event| event.payload)
            .collect()
    };
    let listing = ["W-PAST", "W-FRESH", "W-LOST", "W-ALIVE"].map(String::from);

    // cmux cannot list its workspaces: nothing is closed.
    let mut failing = TestWorkspace::new(&db, false, "exit 0");
    failing.exists_fails = true;
    failing.listed.lock().unwrap().extend(listing.clone());
    supervise_with(&db, &repo, &failing, &options()).unwrap();
    assert_eq!(open(), [past, fresh, lost, alive]);
    assert!(failing.closed.lock().unwrap().is_empty());
    assert!(closes().is_empty());

    // Past the grace, its workspace (unpinned first by the adapter) and
    // row close, once; the others stay.
    let backend = TestWorkspace::new(&db, false, "exit 0");
    backend.listed.lock().unwrap().extend(listing);
    supervise_with(&db, &repo, &backend, &options()).unwrap();
    assert_eq!(open(), [fresh, lost, alive]);
    assert_eq!(*backend.closed.lock().unwrap(), ["W-PAST"]);
    let recorded = closes();
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0]["planner_id"], past.as_i64());
    assert_eq!(recorded[0]["origin"], "person");
    assert_eq!(recorded[0]["code"], "person_exited");
    assert_eq!(recorded[0]["workspace_id"], "W-PAST");
    assert_eq!(recorded[0]["workspace_closed"], true);
    assert_eq!(recorded[0]["exit_code"], 0);
    assert!(recorded[0]["exited_at"].is_i64());
    supervise_with(&db, &repo, &backend, &options()).unwrap();
    assert_eq!(closes().len(), 1);

    // `events --kind` reads it.
    let read = common::cli::ok(&db, &["events", "--full", "--kind", "planner_closed"]);
    let events = read["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "{read}");
    assert_eq!(events[0]["payload"]["planner_id"], past.as_i64());
    assert_eq!(events[0]["payload"]["code"], "person_exited");
}

/// A workspace cmux fails to close keeps the planner's row open, for the
/// next pass to try again.
#[test]
fn a_persons_planner_whose_workspace_does_not_close_stays_open() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let me = std::process::id();
    let planner = queue.open_planner(PlannerOrigin::Person, None).unwrap();
    queue
        .planner_workspace_created(planner.id, "W-STUCK")
        .unwrap();
    queue.register_planner_wrapper(planner.id, me).unwrap();
    queue.register_planner_agent(planner.id, me, me).unwrap();
    queue.planner_exited(planner.id, me, 0).unwrap();
    exited_ago(&db, planner.id, 61);

    let mut backend = TestWorkspace::new(&db, false, "exit 0");
    backend.close_times_out = true;
    backend.listed.lock().unwrap().push("W-STUCK".into());
    supervise_with(&db, &repo, &backend, &options()).unwrap();
    assert!(queue.planner(planner.id).unwrap().closed_at.is_none());
    assert!(
        queue
            .latest_events_of("planner_closed", 10)
            .unwrap()
            .is_empty()
    );

    backend.close_times_out = false;
    supervise_with(&db, &repo, &backend, &options()).unwrap();
    assert!(queue.planner(planner.id).unwrap().closed_at.is_some());
    assert_eq!(
        queue.latest_events_of("planner_closed", 10).unwrap().len(),
        1
    );
}
