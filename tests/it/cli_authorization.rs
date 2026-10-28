//! The planning commands are authorized in the application before they
//! change the queue (ADR-t728-1 decision 5, task 732): workers, the headless
//! jobs and the observer run none, the planner keeps its authority on tasks
//! that have not started and on its own proposals, the user and the inbox
//! run them all, and every refusal is recorded as `authorization_denied`.

use crate::common;

use common::cli::*;

use serde_json::{Value, json};
use std::path::Path;

#[test]
fn ended_run_verify_edit_is_delegated_only_to_user_and_inbox() {
    use dagq::application::TaskStore;
    use dagq::infrastructure::sqlite::SqliteQueue;
    let (_dir, db) = queue();
    let id = ok(&db, &["add", "verify", "--verify", "false"])["id"]
        .as_i64()
        .unwrap();
    let id_text = id.to_string();
    ok(&db, &["ready", &id_text, "--bypass-review"]);
    let mut store = SqliteQueue::open(&db).unwrap();
    let run = match store.claim(&crate::common::queue::base()).unwrap() {
        dagq::domain::ClaimOutcome::Claimed { run } => run,
        other => panic!("unexpected claim: {other:?}"),
    };
    assert!(
        !invoke_as(None, &db, &["edit", &id_text, "--verify", "true"])
            .status
            .success()
    );
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE task_runs SET status='failed' WHERE id=?1",
        [run.id().as_str()],
    )
    .unwrap();
    conn.execute(
        "DELETE FROM run_leases WHERE run_id=?1",
        [run.id().as_str()],
    )
    .unwrap();
    for role in ["planner", "worker", "recovery-job", "review-job"] {
        assert!(
            !invoke_as(Some(role), &db, &["edit", &id_text, "--verify", "true"])
                .status
                .success(),
            "{role}"
        );
    }
    assert!(
        !invoke_as(None, &db, &["edit", &id_text, "--paths", "src/**"])
            .status
            .success()
    );
    ok(&db, &["edit", &id_text, "--verify", "true"]);
    ok_as("inbox", &db, &["edit", &id_text, "--no-verify"]);
    let edits: Vec<_> = ok(&db, &["show", &id_text, "--full"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "task_edited")
        .cloned()
        .collect();
    assert_eq!(edits.len(), 2);
    assert_eq!(edits[0]["actor"]["role"], "user");
    assert_eq!(
        edits[0]["payload"]["from"]["verification_commands"],
        serde_json::json!(["false"])
    );
    assert_eq!(
        edits[0]["payload"]["to"]["verification_commands"],
        serde_json::json!(["true"])
    );
    assert_eq!(edits[1]["actor"]["role"], "inbox");
    assert_eq!(
        edits[1]["payload"]["to"]["verification_commands"],
        serde_json::json!([])
    );
}

/// Open the runtime's planners of the queue at `db` through id `through`:
/// the planners whose actor ids the tests name, each a principal of its
/// own on the queue service.
fn open_planners(db: &Path, through: i64) {
    let queue = dagq::infrastructure::sqlite::SqliteQueue::open(db).unwrap();
    while queue
        .open_planner(dagq::domain::PlannerOrigin::Runtime, None)
        .unwrap()
        .id
        .as_i64()
        < through
    {}
}

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
        "throughput-review-job",
        "setup-review-job",
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
    open_planners(&db, 5);
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
    assert!(denials(&db).is_empty());
    // A person's priority (ADR-t1975-1 decisions 3 and 4): the planner may
    // change neither the person's goal's nor the one its task takes, and
    // each refusal is recorded; moving the task to the planner's normal
    // goal keeps the person's value as the task's own.
    ok(
        &db,
        &["goal", "add", "a person's", "--priority", "interrupt"],
    );
    ok(&db, &["add", "third", "--goal", "2"]);
    let error = denied_as(&me, &db, &["set-priority", "3", "low"]);
    assert!(
        error["error"].as_str().unwrap().starts_with(
            "a person set the priority of task 3 (interrupt): planner may not change it"
        ),
        "{error}"
    );
    let error = denied_as(&me, &db, &["goal", "edit", "2", "--priority", "low"]);
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("goal 2 (interrupt)"),
        "{error}"
    );
    let recorded = denials(&db);
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    for (event, resource) in recorded.iter().zip([
        json!({"kind": "task", "id": 3, "status": "draft"}),
        json!({"kind": "goal", "id": 2}),
    ]) {
        assert_eq!(event["actor"]["id"], "planner:1");
        assert_eq!(event["payload"]["reason"], "a person's priority");
        assert_eq!(event["payload"]["resource"], resource);
    }
    assert_eq!(
        ok(&db, &["goal", "show", "2"])["goal"]["priority"],
        "interrupt"
    );
    allowed_as(&me, &db, &["set-goal", "3", "1"]);
    let moved = &ok(&db, &["show", "3"])["task"];
    assert_eq!(
        (
            &moved["goal_id"],
            &moved["priority"],
            &moved["priority_source"],
            &moved["priority_by"]
        ),
        (
            &json!(1),
            &json!("interrupt"),
            &json!("task"),
            &json!("human")
        )
    );
    // The inbox changes it, and it stays the person's.
    let high = ok_as("inbox", &db, &["set-priority", "3", "high"]);
    assert_eq!(
        (&high["priority"], &high["priority_by"]),
        (&json!("high"), &json!("human"))
    );
    allowed_as(&me, &db, &["cancel", "2", "--reason", "stale"]);
    assert_eq!(ok(&db, &["show", "2"])["task"]["status"], "canceled");
    allowed_as(&me, &db, &["cancel", "1", "--reason", "stale"]);
    allowed_as(&me, &db, &["goal", "close", "1", "--verdict", "abandoned"]);
    assert_eq!(ok(&db, &["goal", "show", "1"])["closed"], true);
}

#[test]
fn the_planner_may_not_ready_a_task_or_touch_one_that_started() {
    let (_dir, db) = queue();
    open_planners(&db, 5);
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

/// ADR-t1850-1 decision 7: the user and the inbox set and clear the
/// priority of an in-progress task, recorded as `task_priority_changed`
/// like any other; the planner's set-priority on it is refused and
/// recorded as `authorization_denied`, and a finished task keeps its own.
#[test]
fn only_user_and_inbox_set_the_priority_of_a_task_in_progress() {
    let (_dir, db) = queue();
    open_planners(&db, 5);
    ok(&db, &["add", "parked"]);
    ok(&db, &["ready", "1", "--bypass-review"]);
    set_status(&db, 1, "in_progress");
    let low = ok(&db, &["set-priority", "1", "low"]);
    assert_eq!(low["priority"], "low");
    assert_eq!(low["status"], "in_progress");
    let high = ok_as("inbox", &db, &["set-priority", "1", "high"]);
    assert_eq!(high["priority"], "high");
    let inherited = ok_as("inbox", &db, &["set-priority", "1", "--inherit"]);
    assert_eq!(inherited["priority"], "normal");
    assert_eq!(inherited["priority_source"], "default");
    let changes: Vec<_> = ok(&db, &["show", "1", "--full"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "task_priority_changed")
        .map(|e| (e["actor"]["role"].clone(), e["payload"]["to"].clone()))
        .collect();
    assert_eq!(
        changes,
        [
            (json!("user"), json!("low")),
            (json!("inbox"), json!("high")),
            (json!("inbox"), json!("normal")),
        ]
    );
    let error = denied_as(&planner("planner:1"), &db, &["set-priority", "1", "low"]);
    assert_eq!(error["denied"]["reason"], "not on this resource", "{error}");
    assert_eq!(ok(&db, &["show", "1"])["task"]["priority"], "normal");
    let recorded = denials(&db);
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0]["payload"]["resource"],
        json!({"kind": "task", "id": 1, "status": "in_progress"})
    );
    set_status(&db, 1, "completed");
    assert_eq!(
        refused(&db, &["set-priority", "1", "low"]),
        "task 1 is completed; the priority can only be changed for draft, submitted, ready or in_progress tasks"
    );
}

#[test]
fn a_planner_withdraws_only_its_own_proposal() {
    let (_dir, db) = queue();
    open_planners(&db, 5);
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
    open_planners(&db, 5);
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

/// A planner changes no task of a revising proposal another planner owns,
/// nor of one whose owner the queue does not name as an actor: each change
/// is refused as `not on this resource`, recorded with the proposal, and
/// leaves the task, its dependencies and its events as they were. The
/// planner the revise went to, the user and the inbox change it as before.
#[test]
fn a_planner_changes_no_task_of_another_planners_revising_proposal() {
    let (_dir, db) = queue();
    open_planners(&db, 5);
    let replaced = planner("planner:1");
    allowed_as(&replaced, &db, &["goal", "add", "planned"]);
    allowed_as(&replaced, &db, &["add", "held"]);
    allowed_as(&replaced, &db, &["add", "predecessor"]);
    allowed_as(&replaced, &db, &["submit", "1"]);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let send_back = |owner: Option<&str>, reviser: Option<i64>| {
        conn.execute(
            "UPDATE proposals SET status='revising', owner_actor_id=?1, revise_planner_id=?2
             WHERE id=1",
            rusqlite::params![owner, reviser],
        )
        .unwrap();
    };
    set_status(&db, 1, "draft");
    send_back(Some("planner:1"), Some(5));
    let changes: [&[&str]; 9] = [
        &["edit", "1", "--description", "replaced"],
        &["set-goal", "1", "1"],
        &["set-paths", "1", "--paths", "docs/**"],
        &["set-priority", "1", "high"],
        &["revisit", "1", "--at", "2099-01-01T00:00:00Z"],
        &["draft", "1"],
        &["dependency", "add", "1", "2"],
        &["dependency", "remove", "1", "2"],
        &["cancel", "1", "--reason", "stale"],
    ];
    let task_events = || {
        conn.query_row(
            "SELECT COUNT(*) FROM run_events WHERE task_id=1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
    };
    let before = (
        ok(&db, &["show", "1", "--full"])["task"].clone(),
        task_events(),
    );
    let refused = |actor: &[(&str, String)], owner: Value| {
        let recorded = denials(&db).len();
        for args in changes {
            let error = denied_as(actor, &db, args);
            assert_eq!(
                error["denied"]["reason"], "not on this resource",
                "{args:?}"
            );
        }
        let recorded = &denials(&db)[recorded..];
        assert_eq!(recorded.len(), changes.len());
        for event in recorded {
            assert_eq!(
                event["payload"]["resource"],
                json!({"kind": "task", "id": 1, "status": "draft",
                       "proposal": {"id": 1, "status": "revising", "owner": owner}}),
            );
        }
        assert_eq!(
            (
                ok(&db, &["show", "1", "--full"])["task"].clone(),
                task_events()
            ),
            before
        );
        assert!(
            ok(&db, &["show", "1"])["dependencies"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    };
    refused(&replaced, json!("planner:5"));
    // The owner unrecorded, or a role's name that names no one actor.
    send_back(None, None);
    refused(&planner("planner:5"), Value::Null);
    send_back(Some("planner"), None);
    refused(&[("DAGQ_ROLE", "planner".to_owned())], json!("planner"));

    // The planner the revise went to, the user and the inbox change it.
    send_back(Some("planner:1"), Some(5));
    let owner = planner("planner:5");
    allowed_as(&owner, &db, &["edit", "1", "--description", "kept"]);
    allowed_as(&owner, &db, &["dependency", "add", "1", "2"]);
    allowed_as(&owner, &db, &["dependency", "remove", "1", "2"]);
    ok(&db, &["set-paths", "1", "--paths", "docs/**"]);
    ok_as("inbox", &db, &["set-priority", "1", "high"]);
    // With no reviser, the planner that submitted it owns it.
    send_back(Some("planner:1"), None);
    allowed_as(&replaced, &db, &["edit", "1", "--title", "held again"]);
    let task = &ok(&db, &["show", "1"])["task"];
    assert_eq!(
        (
            &task["description"],
            &task["paths"],
            &task["priority"],
            &task["title"]
        ),
        (
            &json!("kept"),
            &json!(["docs/**"]),
            &json!("high"),
            &json!("held again")
        )
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

/// The CLI's refusal of a watch and of an export, recorded as the refused
/// caller (task 1151): who may not watch and export is
/// `domain::authorization::tests::workers_wrappers_the_integrator_and_the_jobs_neither_watch_nor_export`,
/// which capability each command asks `main`'s
/// `tests::watching_and_exporting_ask_their_own_capability_and_reads_ask_to_read`
/// (task 1709). A refused export writes no file, a read goes through and
/// writes nothing, and the user watches and exports as before.
#[test]
fn a_worker_is_refused_watch_and_export_and_each_refusal_is_recorded() {
    let (dir, db) = queue();
    ok(&db, &["goal", "add", "open goal"]);
    ok(&db, &["add", "existing", "--goal", "1"]);
    let graph = dir.path().join("graph.d2");
    let graph = graph.to_str().unwrap();
    let reports = dir.path().join("reports-out");
    let reports = reports.to_str().unwrap();
    let watch: &[&str] = &["watch", "--after", "0", "--timeout", "1"];
    let export: [&[&str]; 2] = [
        &["graph", "--format", "d2", "--out", graph],
        &["report", "--out", reports],
    ];
    let worker = vec![
        ("DAGQ_ROLE", "worker".to_owned()),
        ("DAGQ_ACTOR_ID", "worker:r1".to_owned()),
        ("DAGQ_RUN_ID", "r1".to_owned()),
        ("DAGQ_TASK_ID", "1".to_owned()),
    ];
    let before = denials(&db).len();
    let mut refused = Vec::new();
    for (args, capability) in std::iter::once((watch, "queue.watch"))
        .chain(export.iter().map(|args| (*args, "queue.export")))
    {
        let error = denied_as(&worker, &db, args);
        assert_eq!(error["denied"]["role"], "worker", "{args:?}: {error}");
        assert_eq!(error["denied"]["capability"], capability, "{error}");
        assert_eq!(error["denied"]["reason"], "not granted", "{error}");
        assert!(
            error["error"]
                .as_str()
                .unwrap()
                .starts_with(&format!("worker may not {capability}")),
            "{error}"
        );
        refused.push(capability);
    }
    // Reading stays open, and writes nothing.
    let written = event_count(&db);
    for args in [&["show", "1"][..], &["graph"]] {
        allowed_as(&worker, &db, args);
    }
    assert_eq!(event_count(&db), written);
    // Each refusal went to the queue's service, which takes a worker named
    // with no token for no principal: recorded as unauthenticated, never as
    // the worker's `authorization_denied` (cli_denials has the refusals of
    // a principal).
    assert_eq!(denials(&db).len(), before);
    let unauthenticated = common::service::events(&db, "queue_service_unauthenticated");
    assert_eq!(unauthenticated.len(), refused.len(), "{unauthenticated:?}");
    for event in &unauthenticated {
        assert_eq!(event["payload"]["use_case"], "denial_record", "{event}");
        assert_eq!(event["payload"]["reason"], "missing_token", "{event}");
    }
    let events = event_count(&db);
    assert!(!Path::new(graph).exists());
    assert!(!Path::new(reports).exists());
    // The user watches and exports as before.
    allowed_as(&[], &db, watch);
    for args in export {
        allowed_as(&[], &db, args);
    }
    assert!(Path::new(graph).is_file());
    assert!(Path::new(reports).is_dir());
    // Allowed watches and exports record nothing.
    assert_eq!(event_count(&db), events);
}

fn event_count(db: &Path) -> usize {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM run_events", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap() as usize
}

#[test]
fn a_refused_watch_with_no_queue_is_refused_unrecorded() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let env = vec![("DAGQ_ROLE", "worker".to_owned())];
    let error = denied_as(&env, &db, &["watch", "--after", "0", "--timeout", "1"]);
    assert_eq!(error["denied"]["role"], "worker", "{error}");
    assert_eq!(error["denied"]["capability"], "queue.watch", "{error}");
    assert_eq!(error["denied"]["reason"], "not granted", "{error}");
    assert!(!db.exists());
}

/// ADR-t1504-2 decision 10: a planner's set-goal of a goal review's
/// goal_gap draft out of its goal (to another goal or out of any goal) is
/// refused and recorded as `authorization_denied` after the store's
/// transaction rolled back; a person's move passes unrecorded, and the
/// planner still cancels it as a duplicate or as done.
#[test]
fn a_planner_does_not_move_a_goal_gap_draft_out_of_its_goal() {
    use dagq::domain::{DraftOrigin, TaskId};
    let (_dir, db) = queue();
    open_planners(&db, 5);
    let me = planner("planner:1");
    ok(&db, &["goal", "add", "reviewed"]);
    ok(&db, &["goal", "add", "another"]);
    let mut queue = dagq::infrastructure::sqlite::SqliteQueue::open(&db).unwrap();
    for title in ["gap", "duplicate", "done", "found"] {
        let id = ok(&db, &["add", title, "--goal", "1"])["id"]
            .as_i64()
            .unwrap();
        if title != "found" {
            queue
                .record_draft_origin(TaskId::new(id), DraftOrigin::GoalGap, &json!({}))
                .unwrap();
        }
    }
    drop(queue);
    for (to, args) in [
        (json!(2), ["set-goal", "1", "2"]),
        (json!(null), ["set-goal", "1", "--none"]),
    ] {
        let error = denied_as(&me, &db, &args);
        let message = error["error"].as_str().unwrap();
        assert!(message.contains("goal's acceptance needs it"), "{error}");
        assert_eq!(ok(&db, &["show", "1"])["task"]["goal_id"], 1);
        let recorded = denials(&db);
        let last = recorded.last().unwrap();
        assert_eq!(last["actor"]["role"], "planner");
        assert_eq!(last["payload"]["role"], "planner");
        assert_eq!(last["payload"]["capability"], "task.write");
        assert_eq!(last["payload"]["reason"], "a goal review's goal_gap draft");
        assert_eq!(last["payload"]["rule"], "goal_gap draft stays in its goal");
        assert_eq!(
            last["payload"]["resource"],
            json!({"kind": "task", "id": 1, "status": "draft"})
        );
        assert_eq!(last["payload"]["from_goal_id"], 1);
        assert_eq!(last["payload"]["to_goal_id"], to);
    }
    assert_eq!(denials(&db).len(), 2);
    // A planner's set-goal of a draft of another origin, and to the goal
    // it is in, passes.
    allowed_as(&me, &db, &["set-goal", "4", "2"]);
    allowed_as(&me, &db, &["set-goal", "1", "1"]);
    allowed_as(&me, &db, &["cancel", "2", "--duplicate-of", "4"]);
    allowed_as(&me, &db, &["cancel", "3", "--reason", "already done"]);
    // A person moves it, and nothing is recorded.
    ok(&db, &["set-goal", "1", "2"]);
    ok_as("inbox", &db, &["set-goal", "1", "1"]);
    assert_eq!(ok(&db, &["show", "1"])["task"]["goal_id"], 1);
    assert_eq!(denials(&db).len(), 2);
}
