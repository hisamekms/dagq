//! The planning commands are authorized in the application before they
//! change the queue (ADR-t728-1 decision 5, task 732): workers, the headless
//! jobs and the observer run none, the planner keeps its authority on tasks
//! that have not started and on its own proposals, the user and the inbox
//! run them all, and every refusal is recorded as `authorization_denied`.

use crate::common;

use common::cli::*;

use serde_json::Value;
use std::path::Path;

fn planner(id: &str) -> [(&'static str, String); 2] {
    [
        ("DAGQ_ROLE", "planner".into()),
        ("DAGQ_ACTOR_ID", id.into()),
    ]
}

fn run_as(env: &[(&str, String)], db: &Path, args: &[&str]) -> std::process::Output {
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    invoke_with(&env, db, args)
}

fn allowed_as(env: &[(&str, String)], db: &Path, args: &[&str]) -> Value {
    let output = run_as(env, db, args);
    assert!(
        output.status.success(),
        "{env:?} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The error JSON `args` is refused with.
fn denied_as(env: &[(&str, String)], db: &Path, args: &[&str]) -> Value {
    let output = run_as(env, db, args);
    assert!(!output.status.success(), "{env:?} {args:?} was allowed");
    serde_json::from_slice(&output.stderr).unwrap()
}

fn denials(db: &Path) -> Vec<Value> {
    ok(
        db,
        &[
            "events",
            "--after",
            "0",
            "--full",
            "--kind",
            "authorization_denied",
        ],
    )["events"]
        .as_array()
        .unwrap()
        .clone()
}

fn set_status(db: &Path, task: i64, status: &str) {
    rusqlite::Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE tasks SET status=?1 WHERE id=?2",
            rusqlite::params![status, task],
        )
        .unwrap();
}

#[test]
fn workers_jobs_and_the_observer_change_no_plan_and_each_refusal_is_recorded() {
    let (_dir, db) = queue();
    ok(&db, &["goal", "add", "open goal"]);
    ok(&db, &["add", "existing", "--goal", "1"]);
    let worker = vec![
        ("DAGQ_ROLE", "worker".to_owned()),
        ("DAGQ_ACTOR_ID", "worker:r1".to_owned()),
        ("DAGQ_RUN_ID", "r1".to_owned()),
        ("DAGQ_TASK_ID", "1".to_owned()),
    ];
    let mut actors = vec![("worker", worker)];
    for role in [
        "review-job",
        "recovery-job",
        "plan-review-job",
        "goal-review-job",
        "observer",
    ] {
        actors.push((role, vec![("DAGQ_ROLE", role.to_owned())]));
    }
    let commands: [&[&str]; 8] = [
        &["add", "more"],
        &["submit", "1"],
        &["ready", "1"],
        &["ready", "1", "--bypass-review"],
        &["cancel", "1"],
        &["goal", "close", "1", "--verdict", "abandoned"],
        &["set-priority", "1", "high"],
        &["dependency", "add", "1", "--goal", "1"],
    ];
    let mut refused = 0;
    for (role, env) in &actors {
        for args in commands {
            let error = denied_as(env, &db, args);
            let expected = match *role {
                "worker" => format!(
                    "worker may not {}",
                    error["denied"]["capability"].as_str().unwrap()
                ),
                "observer" => "observer may not change queue state".to_owned(),
                _ => "reviewer may not change queue state".to_owned(),
            };
            assert!(
                error["error"].as_str().unwrap().starts_with(&expected),
                "{role} {args:?}: {error}"
            );
            assert_eq!(error["denied"]["role"], *role, "{error}");
            assert_eq!(error["denied"]["reason"], "not granted", "{error}");
            refused += 1;
        }
    }
    // Nothing changed, and every refusal was recorded as its actor.
    assert_eq!(ok(&db, &["list"])["total"], 1);
    assert_eq!(ok(&db, &["show", "1"])["task"]["status"], "draft");
    assert_eq!(ok(&db, &["goal", "show", "1"])["goal"]["status"], "open");
    let recorded = denials(&db);
    assert_eq!(recorded.len(), refused);
    assert_eq!(recorded[0]["actor"]["role"], "worker");
    assert_eq!(recorded[0]["actor"]["id"], "worker:r1");
    assert_eq!(recorded[0]["payload"]["capability"], "task.write");
    assert_eq!(
        recorded[0]["payload"]["resource"],
        serde_json::json!({"kind": "queue"})
    );
    // `cancel` names the task; a role without the capability is refused
    // before its status is read.
    let cancel = recorded
        .iter()
        .find(|event| event["payload"]["capability"] == "task.cancel")
        .unwrap();
    assert_eq!(
        cancel["payload"]["resource"],
        serde_json::json!({"kind": "task", "id": 1, "status": null})
    );
}

#[test]
fn the_planner_keeps_its_authority_on_tasks_that_have_not_started() {
    let (_dir, db) = queue();
    let me = planner("planner:1");
    allowed_as(&me, &db, &["goal", "add", "planned"]);
    allowed_as(&me, &db, &["goal", "edit", "1", "--title", "planned again"]);
    allowed_as(&me, &db, &["add", "first", "--goal", "1"]);
    allowed_as(&me, &db, &["add", "second"]);
    allowed_as(&me, &db, &["edit", "2", "--title", "second again"]);
    // Ready tasks (a person made them ready) stay the planner's to change.
    ok(&db, &["ready", "1", "--bypass-review"]);
    ok(&db, &["ready", "2", "--bypass-review"]);
    allowed_as(&me, &db, &["set-paths", "1", "--paths", "docs/**"]);
    allowed_as(&me, &db, &["set-priority", "1", "high"]);
    allowed_as(&me, &db, &["dependency", "add", "2", "1"]);
    allowed_as(&me, &db, &["dependency", "remove", "2", "1"]);
    allowed_as(&me, &db, &["set-goal", "2", "1"]);
    allowed_as(&me, &db, &["cancel", "2"]);
    assert_eq!(ok(&db, &["show", "2"])["task"]["status"], "canceled");
    allowed_as(&me, &db, &["cancel", "1"]);
    allowed_as(&me, &db, &["goal", "close", "1", "--verdict", "abandoned"]);
    assert_eq!(ok(&db, &["goal", "show", "1"])["closed"], true);
    assert!(denials(&db).is_empty());
}

#[test]
fn the_planner_may_not_ready_a_task_or_touch_one_that_started() {
    let (_dir, db) = queue();
    let me = planner("planner:1");
    allowed_as(&me, &db, &["add", "first"]);
    allowed_as(&me, &db, &["add", "second"]);
    for args in [&["ready", "1"][..], &["ready", "1", "--bypass-review"]] {
        let error = denied_as(&me, &db, args);
        assert_eq!(error["denied"]["reason"], "not granted", "{error}");
    }
    set_status(&db, 2, "in_progress");
    for args in [
        &["cancel", "2"][..],
        &["draft", "2"],
        &["set-priority", "2", "high"],
        &["dependency", "add", "2", "1"],
    ] {
        let error = denied_as(&me, &db, args);
        assert_eq!(
            error["error"],
            format!(
                "planner may not {} (not on this resource)",
                error["denied"]["capability"].as_str().unwrap()
            ),
            "{args:?}"
        );
    }
    assert_eq!(ok(&db, &["show", "1"])["task"]["status"], "draft");
    assert_eq!(ok(&db, &["show", "2"])["task"]["status"], "in_progress");
    let recorded = denials(&db);
    assert_eq!(recorded.len(), 6);
    assert_eq!(
        recorded[2]["payload"]["resource"],
        serde_json::json!({"kind": "task", "id": 2, "status": "in_progress"})
    );
    assert_eq!(recorded[2]["actor"]["id"], "planner:1");
}

#[test]
fn a_planner_withdraws_only_its_own_proposal() {
    let (_dir, db) = queue();
    let mine = planner("planner:1");
    let other = planner("planner:2");
    allowed_as(&mine, &db, &["add", "first"]);
    allowed_as(&mine, &db, &["add", "second"]);
    let first = allowed_as(&mine, &db, &["submit", "1"])["id"].to_string();
    let second = allowed_as(&other, &db, &["submit", "2"])["id"].to_string();

    let error = denied_as(&other, &db, &["proposal", "withdraw", &first]);
    assert_eq!(
        error["error"],
        "planner may not proposal.withdraw (not on this resource)"
    );
    assert_eq!(
        denials(&db)[0]["payload"]["resource"],
        serde_json::json!({"kind": "proposal", "id": 1, "owner": "planner:1"})
    );
    let withdrawn = allowed_as(&mine, &db, &["proposal", "withdraw", &first]);
    assert_eq!(withdrawn["status"], "canceled");
    // A person withdraws any.
    assert_eq!(
        ok(&db, &["proposal", "withdraw", &second])["status"],
        "canceled"
    );
}

#[test]
fn the_inbox_readies_at_a_persons_word_and_the_record_tells_it_from_the_user() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    ok(&db, &["add", "second"]);
    let inbox = vec![
        ("DAGQ_ROLE", "inbox".to_owned()),
        ("DAGQ_ACTOR_ID", "inbox".to_owned()),
    ];
    allowed_as(&inbox, &db, &["ready", "1", "--bypass-review"]);
    ok(&db, &["ready", "2", "--bypass-review"]);
    let events = ok(
        &db,
        &[
            "events",
            "--after",
            "0",
            "--all",
            "--full",
            "--kind",
            "task_status_changed",
        ],
    )["events"]
        .clone();
    let readied_by = |task: i64| {
        events
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["task_id"] == task && event["payload"]["to"] == "ready")
            .unwrap_or_else(|| panic!("task {task} was not readied: {events}"))["actor"]["role"]
            .clone()
    };
    assert_eq!(readied_by(1), "inbox");
    assert_eq!(readied_by(2), "user");
}

/// A proposal plan review sent back to a planner of the runtime's (its
/// owner had closed) is that planner's to withdraw, and a session from
/// before `DAGQ_ACTOR_ID` owns nothing.
#[test]
fn the_planner_a_revise_was_sent_to_may_withdraw_it() {
    let (_dir, db) = queue();
    let owner = planner("planner:1");
    allowed_as(&owner, &db, &["add", "first"]);
    let id = allowed_as(&owner, &db, &["submit", "1"])["id"].to_string();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE proposals SET status='revising', revise_planner_id=5 WHERE id=?1",
            [&id],
        )
        .unwrap();
    let legacy = vec![("DAGQ_ROLE", "planner".to_owned())];
    assert_eq!(
        denied_as(&legacy, &db, &["proposal", "withdraw", &id])["denied"]["reason"],
        "not on this resource"
    );
    denied_as(&owner, &db, &["proposal", "withdraw", &id]);
    let taken_up = planner("planner:5");
    assert_eq!(
        allowed_as(&taken_up, &db, &["proposal", "withdraw", &id])["status"],
        "canceled"
    );
}

/// A role without the capability is refused as always, whether or not
/// the task exists.
#[test]
fn a_refusal_does_not_depend_on_the_task_existing() {
    let (_dir, db) = queue();
    let observer = vec![("DAGQ_ROLE", "observer".to_owned())];
    let error = denied_as(&observer, &db, &["cancel", "9999"]);
    assert_eq!(error["error"], "observer may not change queue state");
    assert_eq!(denials(&db).len(), 1);
}
