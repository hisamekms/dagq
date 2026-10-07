//! `candidates` and `graph`'s `candidates` as claimed (ADR-t1992-1): the
//! claim order less the deferrals the supervisor recorded, the deferred
//! tasks apart, and what holds the claims in `held`.

use crate::common;
use dagq::application::DraftPlannerStore;
use dagq::domain::{EventKind, LeaseToken, TaskId};
use dagq::infrastructure::sqlite::SqliteQueue;
use serde_json::{Value, json};
use std::path::Path;

use common::cli::*;

fn ids(tasks: &Value) -> Vec<i64> {
    tasks
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["id"].as_i64().unwrap())
        .collect()
}

fn deferred_ids(view: &Value) -> Vec<i64> {
    view["deferred"]
        .as_array()
        .unwrap()
        .iter()
        .map(|deferral| deferral["task_id"].as_i64().unwrap())
        .collect()
}

fn reasons(view: &Value) -> Vec<String> {
    view["held"]
        .as_array()
        .unwrap()
        .iter()
        .map(|held| held["reason"].as_str().unwrap().to_owned())
        .collect()
}

/// Three ready tasks; the second is high, so the rule's order is 2, 1, 3.
fn three_ready(db: &Path) {
    ok(db, &["add", "first"]);
    ok(db, &["add", "second", "--priority", "high"]);
    ok(db, &["add", "third"]);
    for id in ["1", "2", "3"] {
        ok(db, &["ready", id, "--bypass-review"]);
    }
}

fn record_task(db: &Path, task: i64, kind: EventKind, payload: Value) {
    SqliteQueue::open(db)
        .unwrap()
        .record_task_event(TaskId::new(task), kind, payload)
        .unwrap();
}

fn record_queue(db: &Path, kind: EventKind, payload: Value) {
    SqliteQueue::open(db)
        .unwrap()
        .record_queue_event(kind, payload)
        .unwrap();
}

/// A task with an open `claim_deferred` leaves `candidates` and `graph`'s
/// `candidates` and stands in `deferred` with what the supervisor recorded;
/// `--ignore-deferrals` keeps it in the rule's order; once the deferral
/// ended it is back in place. With no supervisor, `held` says so.
#[test]
fn a_recorded_deferral_leaves_the_order_until_it_ends() {
    let (_dir, db) = queue();
    three_ready(&db);
    let before = ok(&db, &["candidates"]);
    assert_eq!(ids(&before["candidates"]), [2, 1, 3]);
    assert_eq!(before["deferred"], json!([]));
    assert_eq!(reasons(&before), ["no_supervisor"]);

    record_task(
        &db,
        2,
        EventKind::ClaimDeferred,
        json!({"reason": "hot_files", "files": ["docs/hot.md"],
               "runs": [{"run_id": "r9", "task_id": 9}], "max_secs": 3600,
               "supervisor": "tok"}),
    );
    let view = ok(&db, &["candidates"]);
    assert_eq!(ids(&view["candidates"]), [1, 3]);
    let deferred = &view["deferred"][0];
    assert_eq!(deferred_ids(&view), [2]);
    assert_eq!(deferred["reason"], "hot_files");
    assert_eq!(deferred["files"], json!(["docs/hot.md"]));
    assert_eq!(deferred["runs"], json!([{"run_id": "r9", "task_id": 9}]));
    assert!(deferred["since"].is_string(), "{view}");
    assert_eq!(view["candidates"][0]["effective_priority"], "normal");

    let graph = ok(&db, &["graph"]);
    assert_eq!(graph["candidates"], json!([1, 3]));
    assert_eq!(graph["deferred"], view["deferred"]);
    assert_eq!(graph["tasks"].as_array().unwrap().len(), 3, "{graph}");

    let rule = ok(&db, &["candidates", "--ignore-deferrals"]);
    assert_eq!(ids(&rule["candidates"]), [2, 1, 3]);
    assert_eq!(rule["deferred"], view["deferred"]);
    assert_eq!(rule["held"], view["held"]);

    record_task(
        &db,
        2,
        EventKind::ClaimDeferralEnded,
        json!({"reason": "hot_files", "why": "cleared", "deferred_secs": 5,
               "supervisor": "tok"}),
    );
    let after = ok(&db, &["candidates"]);
    assert_eq!(ids(&after["candidates"]), [2, 1, 3]);
    assert_eq!(after["deferred"], json!([]));
    assert_eq!(ok(&db, &["graph"])["candidates"], json!([2, 1, 3]));
}

/// The holds a live supervisor recorded stand in `held`, with their
/// records, and change neither `candidates` nor `deferred`; a hold that
/// resumed is gone.
#[test]
fn the_recorded_holds_stand_in_held_and_leave_the_order_alone() {
    let (_dir, db) = queue();
    three_ready(&db);
    let token = LeaseToken::new("live");
    SqliteQueue::open(&db)
        .unwrap()
        .register_supervisor(&token, std::process::id(), 2, "0.0.1")
        .unwrap();
    assert_eq!(reasons(&ok(&db, &["candidates"])), Vec::<String>::new());

    record_queue(
        &db,
        EventKind::ClaimHeld,
        json!({"reason": "load_average", "value": 40.0, "threshold": 16.0,
               "supervisor": "live"}),
    );
    record_queue(
        &db,
        EventKind::BrokerClaimsHeld,
        json!({"reason": "not_ready", "message": "the broker starts"}),
    );
    record_queue(
        &db,
        EventKind::RunEnvProgramMissing,
        json!({"missing": ["sccache"], "supervisor": "live"}),
    );
    let view = ok(&db, &["candidates"]);
    assert_eq!(
        reasons(&view),
        [
            "claim_held",
            "broker_claims_held",
            "run_env_program_missing"
        ]
    );
    assert_eq!(view["held"][0]["record"]["reason"], "load_average");
    assert!(view["held"][0]["since"].is_string(), "{view}");
    assert_eq!(ids(&view["candidates"]), [2, 1, 3]);
    assert_eq!(view["deferred"], json!([]));

    record_queue(&db, EventKind::ClaimResumed, json!({"supervisor": "live"}));
    assert_eq!(
        reasons(&ok(&db, &["candidates"])),
        ["broker_claims_held", "run_env_program_missing"]
    );
}

/// A live supervisor's hold for the CI watch and for a landing branch that
/// does not resolve stand in `held` with their records until it records
/// their end, whatever another supervisor records after it; a gone
/// supervisor's hold holds nothing.
#[test]
fn the_ci_watch_and_landing_branch_holds_stand_in_held() {
    let (_dir, db) = queue();
    three_ready(&db);
    for token in ["live", "other"] {
        SqliteQueue::open(&db)
            .unwrap()
            .register_supervisor(&LeaseToken::new(token), std::process::id(), 2, "0.0.1")
            .unwrap();
    }
    record_queue(
        &db,
        EventKind::CiWatchHeld,
        json!({"reason": "pending", "workflow": "ci.yml", "supervisor": "gone"}),
    );
    record_queue(
        &db,
        EventKind::LandingBranchUnresolved,
        json!({"reason": "unresolved", "error": "no branch", "supervisor": "gone"}),
    );
    assert_eq!(reasons(&ok(&db, &["candidates"])), Vec::<String>::new());

    record_queue(
        &db,
        EventKind::CiWatchHeld,
        json!({"reason": "unreadable", "workflow": "ci.yml", "supervisor": "live"}),
    );
    record_queue(
        &db,
        EventKind::LandingBranchUnresolved,
        json!({"reason": "unresolved", "error": "no branch", "supervisor": "live"}),
    );
    // The other supervisor's hold and its end, after the live one's.
    record_queue(
        &db,
        EventKind::CiWatchHeld,
        json!({"reason": "pending", "workflow": "ci.yml", "supervisor": "other"}),
    );
    record_queue(
        &db,
        EventKind::CiWatchResumed,
        json!({"reason": "pending", "supervisor": "other"}),
    );
    let view = ok(&db, &["candidates"]);
    assert_eq!(
        reasons(&view),
        ["ci_watch_held", "landing_branch_unresolved"]
    );
    assert_eq!(view["held"][0]["record"]["reason"], "unreadable");
    assert_eq!(view["held"][1]["record"]["error"], "no branch");
    assert!(view["held"][1]["since"].is_string(), "{view}");
    assert_eq!(ids(&view["candidates"]), [2, 1, 3]);
    assert_eq!(view["deferred"], json!([]));
    // `status` shows each on its supervisor only.
    let status = ok(&db, &["status"]);
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 2, "{status}");
    let holding: Vec<&Value> = supervisors
        .iter()
        .filter(|supervisor| supervisor.get("ci_watch_hold").is_some())
        .collect();
    assert_eq!(holding.len(), 1, "{status}");
    assert_eq!(
        holding[0]["ci_watch_hold"]["reason"], "unreadable",
        "{status}"
    );
    assert_eq!(
        holding[0]["landing_branch_hold"]["error"], "no branch",
        "{status}"
    );

    record_queue(
        &db,
        EventKind::CiWatchResumed,
        json!({"reason": "unreadable", "supervisor": "live"}),
    );
    record_queue(
        &db,
        EventKind::LandingBranchResolved,
        json!({"reason": "unresolved", "supervisor": "live"}),
    );
    assert_eq!(reasons(&ok(&db, &["candidates"])), Vec::<String>::new());
}

/// A live supervisor's hold is read from its own latest record however
/// many records of the same kinds another supervisor wrote after it, in
/// `held` and on its supervisor in `status`, until it records the end.
#[test]
fn a_supervisor_s_hold_outlasts_any_number_of_another_s_records() {
    let (_dir, db) = queue();
    three_ready(&db);
    for token in ["live", "other"] {
        SqliteQueue::open(&db)
            .unwrap()
            .register_supervisor(&LeaseToken::new(token), std::process::id(), 2, "0.0.1")
            .unwrap();
    }
    record_queue(
        &db,
        EventKind::CiWatchHeld,
        json!({"reason": "unreadable", "workflow": "ci.yml", "supervisor": "live"}),
    );
    record_queue(
        &db,
        EventKind::LandingBranchUnresolved,
        json!({"reason": "unresolved", "error": "no branch", "supervisor": "live"}),
    );
    let sqlite = SqliteQueue::open(&db).unwrap();
    for _ in 0..70 {
        for (kind, payload) in [
            (
                EventKind::CiWatchHeld,
                json!({"reason": "pending", "supervisor": "other"}),
            ),
            (
                EventKind::CiWatchResumed,
                json!({"reason": "pending", "supervisor": "other"}),
            ),
            (
                EventKind::LandingBranchUnresolved,
                json!({"reason": "unresolved", "error": "flaky", "supervisor": "other"}),
            ),
            (
                EventKind::LandingBranchResolved,
                json!({"reason": "unresolved", "supervisor": "other"}),
            ),
        ] {
            sqlite.record_queue_event(kind, payload).unwrap();
        }
    }
    let view = ok(&db, &["candidates"]);
    assert_eq!(
        reasons(&view),
        ["ci_watch_held", "landing_branch_unresolved"],
        "{view}"
    );
    assert_eq!(view["held"][0]["record"]["supervisor"], "live");
    assert_eq!(view["held"][1]["record"]["supervisor"], "live");
    assert_eq!(ids(&view["candidates"]), [2, 1, 3]);
    let held_on = |status: &Value, key: &str| -> Vec<Value> {
        status["supervisors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|supervisor| supervisor.get(key).cloned())
            .collect()
    };
    let status = ok(&db, &["status"]);
    for key in ["ci_watch_hold", "landing_branch_hold"] {
        let held = held_on(&status, key);
        assert_eq!(held.len(), 1, "{key}: {status}");
        assert_eq!(held[0]["supervisor"], "live", "{key}: {status}");
    }

    record_queue(
        &db,
        EventKind::CiWatchResumed,
        json!({"reason": "unreadable", "supervisor": "live"}),
    );
    record_queue(
        &db,
        EventKind::LandingBranchResolved,
        json!({"reason": "unresolved", "supervisor": "live"}),
    );
    assert_eq!(reasons(&ok(&db, &["candidates"])), Vec::<String>::new());
    let status = ok(&db, &["status"]);
    for key in ["ci_watch_hold", "landing_branch_hold"] {
        assert!(held_on(&status, key).is_empty(), "{key}: {status}");
    }
}
