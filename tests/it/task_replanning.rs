//! The replanning operations of a run (ADR-t1521-1, ADR-t1521-2) through
//! the CLI and the queue: a person's request and a worker's proposal on
//! its own run open one operation per task (on its latest run until it
//! guards the task, then on the run it fenced), with the task's and its
//! goal's conditions as they were, which a reopened queue reads back
//! unchanged;
//! only the trusted runtime moves an operation on; and while one guards
//! its task, `ready`, `edit`, `cancel`, a change of its dependencies, a
//! resume, a landing and the claim are refused, as `status`, `doctor`,
//! `list` and `show` say. The condition mappings' writer deletes an
//! acceptance item at most once per operation.

use crate::common::cli::{invoke_with, ok, ok_as, queue};
use crate::common::queue::{base, new_goal, new_task};
use dagq::domain::EventKind;
use dagq::{
    application::{PlanRequestStore, TaskReads, TaskStore, commands::replans::Replans},
    domain::{
        ActorContext, ActorRole, ClaimOutcome, GoalEdit, LeaseToken, ReplanId, RunId, StaticPolicy,
        TaskAction, TaskId,
        task_replan::{ConditionMapping, NewReplan, ReplanMove, ReplanOrigin, ReplanStatus},
    },
    infrastructure::sqlite::SqliteQueue,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::Path;

/// A goal with task A (two acceptance items) claimed as a run, and task B
/// that waits for A: the queue, A, B and A's run.
fn claimed(db: &Path) -> (SqliteQueue, TaskId, TaskId, RunId) {
    let mut queue = SqliteQueue::open(db).unwrap();
    let goal = queue.add_goal(new_goal("replanning")).unwrap().id();
    let a = queue
        .add(dagq::domain::NewTask {
            acceptance: "the API behaves\nthe operations are documented".into(),
            goal_id: Some(goal),
            ..new_task("A")
        })
        .unwrap()
        .id();
    let b = queue
        .add(dagq::domain::NewTask {
            dependencies: vec![a],
            goal_id: Some(goal),
            ..new_task("B")
        })
        .unwrap()
        .id();
    for task in [a, b] {
        queue.transition(task, TaskAction::BypassReview).unwrap();
    }
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!("claim")
    };
    assert_eq!(run.task_id(), a);
    let run = run.id().clone();
    (queue, a, b, run)
}

/// `dagq` as `role` (with a worker's run and task; a person at a plain
/// terminal for `user`), its JSON and whether it succeeded.
fn as_role(
    db: &Path,
    role: &str,
    worker: Option<(&RunId, TaskId)>,
    args: &[&str],
) -> (bool, Value) {
    let task = worker.map(|(_, task)| task.to_string());
    let mut env = Vec::new();
    if role != "user" {
        env.push(("DAGQ_ROLE", role));
    }
    if let (Some((run, _)), Some(task)) = (worker, task.as_deref()) {
        env.extend([
            ("DAGQ_RUN_ID", run.as_str()),
            ("DAGQ_TASK_ID", task),
            ("DAGQ_ACTOR_ID", run.as_str()),
        ]);
    }
    let output = invoke_with(&env, db, args);
    let bytes = if output.status.success() {
        &output.stdout
    } else {
        &output.stderr
    };
    (
        output.status.success(),
        serde_json::from_slice(bytes).unwrap_or(Value::Null),
    )
}

fn request(run: &RunId, reason: &str) -> Vec<String> {
    [
        "replan",
        "request",
        "--run",
        run.as_str(),
        "--reason",
        reason,
        "--evidence",
        "1",
    ]
    .map(str::to_owned)
    .to_vec()
}

fn args(words: &[String]) -> Vec<&str> {
    words.iter().map(String::as_str).collect()
}

fn denials(db: &Path, capability: &str) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM run_events WHERE kind='authorization_denied'
             AND json_extract(payload,'$.capability')=?1",
            [capability],
            |r| r.get(0),
        )
        .unwrap()
}

/// Move operation `id` as the trusted runtime does (the supervisor).
fn park(queue: &mut SqliteQueue, id: ReplanId, generation: i64, to: ReplanStatus) -> i64 {
    let supervisor = ActorContext::instance(ActorRole::Supervisor, 1);
    Replans::new(queue, &supervisor, &StaticPolicy)
        .advance(
            id,
            generation,
            &ReplanMove {
                to,
                reason: format!("diagnosed: {}", to.as_str()),
                snapshot_id: None,
                diagnosis: None,
            },
        )
        .unwrap()
        .generation
}

#[test]
fn one_operation_per_run_keeps_the_conditions_it_began_with_across_a_reopen() {
    let (_dir, db) = queue();
    let (mut queue, a, b, run) = claimed(&db);
    let goal = queue.show(a).unwrap().task.goal_id().unwrap();

    let first = ok_as(
        "inbox",
        &db,
        &args(&request(&run, "the review keeps sending it back")),
    );
    assert_eq!(first["created"], true);
    let op = &first["operation"];
    let id = op["id"].as_i64().unwrap();
    assert_eq!(op["status"], "candidate");
    assert_eq!(op["origin"], "manual");
    assert_eq!(op["generation"], 1);
    assert_eq!(op["task_id"], a.as_i64());
    assert_eq!(op["requested_by"], "inbox");
    assert_eq!(op["diagnosis"]["evidence"], json!([1]));
    let items: Vec<&str> = op["conditions"]["acceptance"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["text"].as_str().unwrap())
        .collect();
    assert_eq!(items, ["the API behaves", "the operations are documented"]);
    assert_eq!(op["conditions"]["successors"], json!([b.as_i64()]));
    assert_eq!(op["goal_acceptance_version"], 1);
    let request_id = op["request_id"].as_i64().unwrap();
    assert_eq!(first["request"]["id"], request_id);
    assert_eq!(first["request"]["replan_operation_id"], id);
    let version = op["task_version"].clone();

    // A second request, a person's or a worker's on the same run, opens
    // nothing new and records no second planning request.
    let again = ok(&db, &args(&request(&run, "again")));
    assert_eq!(again["created"], false);
    assert_eq!(again["operation"]["id"], id);
    assert_eq!(again["request"]["id"], request_id);
    let (proposed, worker) = as_role(
        &db,
        "worker",
        Some((&run, a)),
        &[
            "replan",
            "propose",
            "--run",
            run.as_str(),
            "--reason",
            "two independent parts",
        ],
    );
    assert!(proposed, "{worker}");
    assert_eq!(worker["created"], false);
    assert_eq!(worker["operation"]["id"], id);
    let counts: (i64, i64) = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT (SELECT count(*) FROM replan_operations),
                    (SELECT count(*) FROM plan_requests WHERE replan_operation_id IS NOT NULL)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(counts, (1, 1));

    // The request's planner waits for the operation's snapshot.
    assert!(
        queue
            .planner_requests()
            .unwrap()
            .iter()
            .all(|request| request.id.as_i64() != request_id)
    );

    // The goal's acceptance changes after: the operation keeps the version
    // and the items it began with.
    queue
        .edit_goal(
            goal,
            GoalEdit {
                acceptance: Some("a new acceptance".into()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
    drop(queue);
    let reopened = SqliteQueue::open(&db).unwrap();
    let read = reopened.replan_operation(ReplanId::new(id)).unwrap();
    assert_eq!(read.status, ReplanStatus::Candidate);
    assert_eq!(read.generation, 1);
    assert_eq!(read.goal_acceptance_version, Some(1));
    assert_eq!(
        read.conditions.goal.as_ref().unwrap().acceptance[0].text,
        "Every task landed and the feature works end to end"
    );
    assert_eq!(json!(read.task_version), version);
    assert_eq!(read.conditions.version(), read.task_version);
    let listed = ok_as("observer", &db, &["replans"]);
    assert_eq!(listed["replans"][0]["id"], id);
    let one = ok(&db, &["replans", &id.to_string()]);
    assert_eq!(
        one["replans"][0]["conditions"]["successors"],
        json!([b.as_i64()])
    );
    let events: Vec<(String, Value)> = Connection::open(&db)
        .unwrap()
        .prepare("SELECT kind, payload FROM run_events WHERE kind='replan_requested' ORDER BY id")
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                serde_json::from_str(&r.get::<_, String>(1)?).unwrap(),
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let created: Vec<(Value, Value)> = events
        .iter()
        .map(|(_, payload)| (payload["origin"].clone(), payload["created"].clone()))
        .collect();
    assert_eq!(
        created,
        [
            (json!("manual"), json!(true)),
            (json!("manual"), json!(false)),
            (json!("worker"), json!(false)),
        ]
    );
}

/// The mappings' key does not compare a deletion's NULL child task, so
/// the writer itself refuses a second deletion of one item: in the same
/// write, and in a later one, which leaves the mappings as they were.
#[test]
fn an_operation_deletes_an_acceptance_item_at_most_once() {
    let (_dir, db) = queue();
    let (mut queue, a, _b, run) = claimed(&db);
    let op = queue
        .record_replan(
            &NewReplan {
                run,
                reason: "two independent parts".into(),
                evidence: Vec::new(),
            },
            ReplanOrigin::Runtime,
            "supervisor",
            "supervisor",
        )
        .unwrap()
        .operation;
    let mapping = |ordinal: usize, child: Option<TaskId>| ConditionMapping {
        ordinal,
        hash: op.conditions.acceptance[ordinal].hash.clone(),
        child_task_id: child,
        child_ordinal: child.map(|_| 0),
        disposition: if child.is_some() { "moved" } else { "deleted" }.into(),
        authority: child.is_none().then(|| json!({"answer": 1})),
    };

    let repeated = queue
        .record_replan_mappings(op.id, &[mapping(1, None), mapping(1, None)])
        .unwrap_err();
    assert!(
        repeated
            .to_string()
            .contains("already deletes acceptance item 1"),
        "{repeated:#}"
    );
    assert!(queue.replan_operation(op.id).unwrap().mappings.is_empty());

    let written = queue
        .record_replan_mappings(op.id, &[mapping(0, Some(a)), mapping(1, None)])
        .unwrap();
    assert_eq!(written.mappings, [mapping(0, Some(a)), mapping(1, None)]);
    let again = queue
        .record_replan_mappings(op.id, &[mapping(1, None)])
        .unwrap_err();
    assert!(
        again
            .to_string()
            .contains("already deletes acceptance item 1"),
        "{again:#}"
    );
    let rows: i64 = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM replan_mappings WHERE operation_id=?1 AND child_task_id IS NULL",
            [op.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1);
}

#[test]
fn an_operation_does_not_both_delete_an_item_and_map_it_to_a_child() {
    let (_dir, db) = queue();
    let (mut queue, a, b, run) = claimed(&db);
    let op = queue
        .record_replan(
            &NewReplan {
                run,
                reason: "two independent parts".into(),
                evidence: Vec::new(),
            },
            ReplanOrigin::Runtime,
            "supervisor",
            "supervisor",
        )
        .unwrap()
        .operation;
    let mapping = |ordinal: usize, child: Option<TaskId>| ConditionMapping {
        ordinal,
        hash: op.conditions.acceptance[ordinal].hash.clone(),
        child_task_id: child,
        child_ordinal: child.map(|_| 0),
        disposition: if child.is_some() { "moved" } else { "deleted" }.into(),
        authority: child.is_none().then(|| json!({"answer": 1})),
    };
    let refused = |err: anyhow::Error| {
        assert!(
            err.to_string()
                .contains("both deletes acceptance item 0 and maps it to a child task"),
            "{err:#}"
        );
    };

    refused(
        queue
            .record_replan_mappings(op.id, &[mapping(0, Some(a)), mapping(0, None)])
            .unwrap_err(),
    );
    assert!(queue.replan_operation(op.id).unwrap().mappings.is_empty());

    // Two children holding the item together pass; a later deletion of it
    // is refused and leaves the rows as they were.
    let shared = [mapping(0, Some(a)), mapping(0, Some(b))];
    queue.record_replan_mappings(op.id, &shared).unwrap();
    refused(
        queue
            .record_replan_mappings(op.id, &[mapping(0, None), mapping(1, None)])
            .unwrap_err(),
    );
    assert_eq!(queue.replan_operation(op.id).unwrap().mappings, shared);
}

#[test]
fn a_deleted_item_is_not_mapped_to_a_child_afterwards() {
    let (_dir, db) = queue();
    let (mut queue, a, _b, run) = claimed(&db);
    let op = queue
        .record_replan(
            &NewReplan {
                run,
                reason: "two independent parts".into(),
                evidence: Vec::new(),
            },
            ReplanOrigin::Runtime,
            "supervisor",
            "supervisor",
        )
        .unwrap()
        .operation;
    let deletion = ConditionMapping {
        ordinal: 0,
        hash: op.conditions.acceptance[0].hash.clone(),
        child_task_id: None,
        child_ordinal: None,
        disposition: "deleted".into(),
        authority: Some(json!({"answer": 1})),
    };
    let moved = ConditionMapping {
        child_task_id: Some(a),
        child_ordinal: Some(0),
        disposition: "moved".into(),
        authority: None,
        ..deletion.clone()
    };
    queue
        .record_replan_mappings(op.id, std::slice::from_ref(&deletion))
        .unwrap();
    let err = queue.record_replan_mappings(op.id, &[moved]).unwrap_err();
    assert!(
        err.to_string()
            .contains("both deletes acceptance item 0 and maps it to a child task"),
        "{err:#}"
    );
    assert_eq!(queue.replan_operation(op.id).unwrap().mappings, [deletion]);
}

#[test]
fn each_role_requests_proposes_or_moves_only_what_it_may() {
    let (_dir, db) = queue();
    let (mut queue, a, _, run) = claimed(&db);
    let other = RunId::new("00000000-0000-4000-8000-000000000000").unwrap();

    // A worker proposes on no run but its own, and requests nothing.
    let (on_other, refused) = as_role(
        &db,
        "worker",
        Some((&other, a)),
        &[
            "replan",
            "propose",
            "--run",
            run.as_str(),
            "--reason",
            "not mine",
        ],
    );
    assert!(!on_other);
    assert_eq!(refused["denied"]["capability"], "replan.propose");
    let (requested, _) = as_role(&db, "worker", Some((&run, a)), &args(&request(&run, "x")));
    assert!(!requested);
    // The planner, the observer, the jobs and the supervisor request
    // nothing and propose nothing.
    for role in ["planner", "observer", "supervisor"] {
        let (requested, refusal) = as_role(&db, role, None, &args(&request(&run, "x")));
        assert!(!requested, "{role}");
        assert_eq!(refusal["denied"]["capability"], "replan.request", "{role}");
        let (proposed, _) = as_role(
            &db,
            role,
            None,
            &["replan", "propose", "--run", run.as_str(), "--reason", "x"],
        );
        assert!(!proposed, "{role}");
    }
    assert_eq!(denials(&db, "replan.propose"), 4);
    assert_eq!(denials(&db, "replan.request"), 4);
    assert!(queue.replan_operations(true).unwrap().is_empty());

    // The worker's own proposal opens the run's operation.
    let (proposed, recorded) = as_role(
        &db,
        "worker",
        Some((&run, a)),
        &[
            "replan",
            "propose",
            "--run",
            run.as_str(),
            "--reason",
            "two independent parts",
        ],
    );
    assert!(proposed, "{recorded}");
    assert_eq!(recorded["operation"]["origin"], "worker");
    assert_eq!(recorded["request"], Value::Null);
    let id = ReplanId::new(recorded["operation"]["id"].as_i64().unwrap());

    // Only the trusted runtime moves it on; nobody reaches the replacement.
    let step = ReplanMove {
        to: ReplanStatus::PauseRequested,
        reason: "diagnosed".into(),
        snapshot_id: None,
        diagnosis: None,
    };
    for actor in [
        ActorContext::user(),
        ActorContext::instance(ActorRole::Inbox, "inbox"),
        ActorContext::instance(ActorRole::Planner, 3),
        ActorContext::worker(&run, a),
    ] {
        let error = Replans::new(&mut queue, &actor, &StaticPolicy)
            .advance(id, 1, &step)
            .unwrap_err();
        assert!(error.to_string().contains("replan.park"), "{error}");
    }
    assert_eq!(park(&mut queue, id, 1, ReplanStatus::PauseRequested), 2);
    let supervisor = ActorContext::instance(ActorRole::Supervisor, 1);
    let stale = Replans::new(&mut queue, &supervisor, &StaticPolicy)
        .advance(
            id,
            1,
            &ReplanMove {
                to: ReplanStatus::Quiescing,
                ..step.clone()
            },
        )
        .unwrap_err();
    assert!(stale.to_string().contains("generation 2"), "{stale}");
}

#[test]
fn a_guarding_operation_refuses_the_ordinary_steps_and_the_claim() {
    let (_dir, db) = queue();
    let (mut queue, a, b, run) = claimed(&db);
    let recorded = ok(&db, &args(&request(&run, "split it")));
    let id = ReplanId::new(recorded["operation"]["id"].as_i64().unwrap());
    // A candidate leaves the run going: its task still takes a dependency.
    queue.remove_dependency(b, a).unwrap();
    queue.add_dependency(b, a).unwrap();
    let generation = park(&mut queue, id, 1, ReplanStatus::PauseRequested);
    park(&mut queue, id, generation, ReplanStatus::Quiescing);

    let replanning = |error: anyhow::Error| {
        let text = format!("{error:#}");
        assert!(
            text.contains(&format!("task {a} is being replanned")),
            "{text}"
        );
    };
    replanning(queue.transition(a, TaskAction::Ready).unwrap_err());
    replanning(queue.transition(a, TaskAction::Cancel).unwrap_err());
    replanning(queue.remove_dependency(b, a).unwrap_err());
    replanning(queue.add_dependency(a, b).unwrap_err());
    replanning(
        queue
            .begin_integration(&run, &LeaseToken::new("t"), &base())
            .unwrap_err(),
    );
    // The command line refuses it too, a person's included.
    for words in [
        vec!["cancel", &*a.to_string(), "--reason", "stale"],
        vec!["edit", &*a.to_string(), "--title", "renamed"],
    ] {
        let (done, error) = as_role(&db, "user", None, &words);
        assert!(!done, "{words:?}");
        assert!(
            error["error"]
                .as_str()
                .unwrap()
                .contains("is being replanned"),
            "{error}"
        );
    }

    // `status`, `doctor`, `list` and `show` say where it is and why.
    for report in [ok(&db, &["status"]), ok(&db, &["doctor"])] {
        let replans = &report["replans"];
        assert_eq!(replans["guarding"], 1, "{replans}");
        assert_eq!(replans["open"][0]["id"], id.as_i64());
        assert_eq!(replans["open"][0]["status"], "quiescing");
        assert_eq!(replans["open"][0]["guards"], true);
        assert_eq!(replans["open"][0]["reason"], "diagnosed: quiescing");
        assert_eq!(replans["problems"], json!([]));
    }
    let listed = ok(&db, &["list"])["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == a.as_i64())
        .unwrap()["replan"]
        .clone();
    assert_eq!(
        listed,
        json!({"operation": id.as_i64(), "status": "quiescing", "guards": true,
               "reason": "diagnosed: quiescing"})
    );
    let shown = ok(&db, &["show", &a.to_string()]);
    assert_eq!(shown["replans"][0]["status"], "quiescing");
    assert_eq!(shown["replans"][0]["generation"], 2);

    // The run stops; its task waits as it would for a retry, but neither
    // the retry, a resume nor a claim takes it while the operation guards
    // it. Each is checked again with the operation set back to a
    // candidate (by hand, for the check only): the guard alone held it.
    // Awaiting its landing, it is not the next to land: it holds back no
    // other run.
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration' WHERE id=?1",
            [run.as_str()],
        )
        .unwrap();
    assert!(queue.next_awaiting_integration().unwrap().is_none());
    queue.recover_run(&run, 0, json!({})).unwrap();
    replanning(queue.transition(a, TaskAction::Ready).unwrap_err());
    let set = |table: &str, status: &str, id: &dyn rusqlite::ToSql| {
        Connection::open(&db)
            .unwrap()
            .execute(
                &format!("UPDATE {table} SET status=?1 WHERE id=?2"),
                rusqlite::params![status, id],
            )
            .unwrap();
    };
    set("task_runs", "needs_session", &run.as_str());
    let resume = |queue: &mut SqliteQueue, token: &str| {
        queue
            .begin_resume(
                &run,
                &LeaseToken::new(token),
                &base(),
                None,
                dagq::domain::resume::ResumeConfig::default(),
            )
            .unwrap()
    };
    assert!(resume(&mut queue, "guarded").is_none());
    set("replan_operations", "candidate", &id.as_i64());
    assert!(resume(&mut queue, "released").is_some());
    // The resumed session is gone, its lease with it, and the run ended.
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases WHERE run_id=?1", [run.as_str()])
        .unwrap();
    set("task_runs", "interrupted", &run.as_str());
    queue.transition(a, TaskAction::Ready).unwrap();
    set("replan_operations", "blocked", &id.as_i64());
    assert!(
        queue
            .candidates()
            .unwrap()
            .iter()
            .all(|task| task.id() != a)
    );
    assert!(matches!(
        queue.claim(&base()).unwrap(),
        ClaimOutcome::NoReadyTask
    ));
    assert_eq!(ok(&db, &["candidates"])["candidates"], json!([]));
    set("replan_operations", "candidate", &id.as_i64());
    assert_eq!(queue.candidates().unwrap()[0].id(), a);
}

/// A run whose resumes are used up is not given up (failed for the
/// recovery job, or retried with its branch) while a replanning guards its
/// task, though the operation came after the supervisor read it: the
/// check is the transaction's. With the operation set back to a candidate
/// (by hand, for the check only), the guard alone held it.
#[test]
fn a_run_out_of_resumes_is_not_given_up_while_a_replanning_guards_it() {
    let (_dir, db) = queue();
    let (mut queue, _, _, run) = claimed(&db);
    let conn = Connection::open(&db).unwrap();
    conn.execute("DELETE FROM run_leases WHERE run_id=?1", [run.as_str()])
        .unwrap();
    conn.execute(
        "UPDATE task_runs SET status='needs_session' WHERE id=?1",
        [run.as_str()],
    )
    .unwrap();
    for attempt in 1..=dagq::domain::MAX_RESUME_ATTEMPTS {
        queue
            .record_runtime_event(
                &run,
                dagq::domain::EventKind::ResumeStarted,
                json!({"attempt": attempt}),
            )
            .unwrap();
    }
    let recorded = ok(&db, &args(&request(&run, "split it")));
    let id = ReplanId::new(recorded["operation"]["id"].as_i64().unwrap());
    park(&mut queue, id, 1, ReplanStatus::PauseRequested);
    let exhaust = |queue: &mut SqliteQueue| {
        queue
            .exhaust_resumes(
                &run,
                &dagq::domain::resume::Exhaustion::Recover,
                "used up",
                dagq::domain::resume::ResumeConfig::default(),
            )
            .unwrap()
    };
    assert!(exhaust(&mut queue).is_none());
    let kept = queue.run(&run).unwrap();
    assert_eq!(kept.status(), dagq::domain::RunStatus::NeedsSession);
    assert!(
        queue
            .run_events(&run)
            .unwrap()
            .iter()
            .all(|event| event.kind != EventKind::RecoveryRequested)
    );
    conn.execute(
        "UPDATE replan_operations SET status='candidate' WHERE id=?1",
        [id.as_i64()],
    )
    .unwrap();
    let failed = exhaust(&mut queue).expect("given up once nothing guards it");
    assert_eq!(failed.status(), dagq::domain::RunStatus::Failed);
}

/// The restore of a parked run's work is one supervisor's from its start to
/// its recorded result (ADR-t1521-2): the first to start it holds the run,
/// and a second is turned away and records nothing, not even a failure that
/// would block the operation. A holder whose lease went stale loses the run
/// to a takeover, and its late result records nothing. A lease a holder
/// could not give back is its own to take up again. The owner's failure
/// blocks the operation and gives the run back; a result for an operation
/// that moved meanwhile records nothing and gives it back as well; the
/// owner's restore is recorded once, after which a start finds the work
/// restored.
#[test]
fn two_supervisors_restore_a_parked_run_one_at_a_time_and_a_loser_writes_nothing() {
    use dagq::domain::replan_snapshot::RestoreStart;
    let (_dir, db) = queue();
    let (mut queue, _, _, run) = claimed(&db);
    let recorded = ok(&db, &args(&request(&run, "split it")));
    let id = ReplanId::new(recorded["operation"]["id"].as_i64().unwrap());
    let mut generation = park(&mut queue, id, 1, ReplanStatus::PauseRequested);
    generation = park(&mut queue, id, generation, ReplanStatus::Quiescing);
    generation = park(&mut queue, id, generation, ReplanStatus::Snapshotting);
    let snapshot = format!("replan-{id}-g{generation}");
    let moved = |queue: &mut SqliteQueue, from: i64, to: ReplanStatus, snapshot: Option<String>| {
        queue
            .move_replan(
                id,
                from,
                &ReplanMove {
                    to,
                    reason: format!("the test moves it to {}", to.as_str()),
                    snapshot_id: snapshot,
                    diagnosis: None,
                },
            )
            .unwrap()
            .generation
    };
    generation = moved(&mut queue, generation, ReplanStatus::Parked, Some(snapshot));
    generation = moved(
        &mut queue,
        generation,
        ReplanStatus::ContinuingAfterPark,
        None,
    );
    // Parked as the park leaves it: waiting for a session, nobody leasing.
    let conn = Connection::open(&db).unwrap();
    conn.execute("DELETE FROM run_leases WHERE run_id=?1", [run.as_str()])
        .unwrap();
    conn.execute(
        "UPDATE task_runs SET status='needs_session' WHERE id=?1",
        [run.as_str()],
    )
    .unwrap();
    let (first, second) = (LeaseToken::new("first"), LeaseToken::new("second"));
    let begin = |queue: &mut SqliteQueue, token: &LeaseToken, generation: i64| {
        queue.begin_restore(&run, token, id, generation).unwrap()
    };
    let finish = |queue: &mut SqliteQueue,
                  token: &LeaseToken,
                  generation: i64,
                  restored: Result<Value, String>| {
        queue
            .finish_restore(&run, token, id, generation, restored)
            .unwrap()
    };
    let count = |queue: &SqliteQueue, kind: &str| {
        queue
            .run_events(&run)
            .unwrap()
            .iter()
            .filter(|event| event.kind.as_str() == kind)
            .count()
    };
    let status = |queue: &SqliteQueue| queue.replan_operation(id).unwrap().status;
    let holder = |queue: &SqliteQueue| {
        queue
            .run_lease(&run)
            .unwrap()
            .map(|lease| lease.token.as_str().to_owned())
    };
    let restored = || Ok(json!({"operation_id": id.as_i64(), "action": "intact"}));

    assert_eq!(begin(&mut queue, &first, generation), RestoreStart::Owned);
    assert_eq!(
        begin(&mut queue, &second, generation),
        RestoreStart::Elsewhere
    );
    // The one turned away cannot block the operation with a failure, nor
    // give the first's lease back.
    assert!(!finish(
        &mut queue,
        &second,
        generation,
        Err("changed".into())
    ));
    assert_eq!(status(&queue), ReplanStatus::ContinuingAfterPark);
    assert_eq!(count(&queue, "replan_restore_failed"), 0);
    assert_eq!(holder(&queue).as_deref(), Some("first"));
    // A lease the first still holds is its own to take up again.
    assert_eq!(begin(&mut queue, &first, generation), RestoreStart::Owned);

    // The first stops heartbeating: the second takes the run over, and the
    // first's late result records nothing and leaves the second's lease.
    conn.execute(
        "UPDATE run_leases SET heartbeat_at=0 WHERE run_id=?1",
        [run.as_str()],
    )
    .unwrap();
    assert_eq!(begin(&mut queue, &second, generation), RestoreStart::Owned);
    assert!(!finish(&mut queue, &first, generation, restored()));
    assert_eq!(count(&queue, "replan_restored"), 0);
    assert_eq!(holder(&queue).as_deref(), Some("second"));

    // The owner's failure blocks the operation and gives the run back.
    assert!(finish(
        &mut queue,
        &second,
        generation,
        Err("changed".into())
    ));
    assert_eq!(status(&queue), ReplanStatus::Blocked);
    assert_eq!(count(&queue, "replan_restore_failed"), 1);
    assert_eq!(holder(&queue), None);

    // A result for an operation that moved meanwhile records nothing and
    // gives the run back as well.
    generation = queue.replan_operation(id).unwrap().generation;
    generation = moved(&mut queue, generation, ReplanStatus::Withdrawn, None);
    assert_eq!(begin(&mut queue, &first, generation), RestoreStart::Owned);
    let reopened = moved(&mut queue, generation, ReplanStatus::Blocked, None);
    assert!(!finish(&mut queue, &first, generation, restored()));
    assert_eq!(count(&queue, "replan_restored"), 0);
    assert_eq!(holder(&queue), None);
    generation = moved(&mut queue, reopened, ReplanStatus::Withdrawn, None);

    // The owner's restore is recorded once; a start then finds it done.
    assert_eq!(begin(&mut queue, &second, generation), RestoreStart::Owned);
    assert!(finish(&mut queue, &second, generation, restored()));
    assert_eq!(count(&queue, "replan_restored"), 1);
    assert_eq!(holder(&queue), None);
    assert_eq!(status(&queue), ReplanStatus::Withdrawn);
    assert_eq!(begin(&mut queue, &first, generation), RestoreStart::Done);
    assert_eq!(holder(&queue), None);
}

/// A replanning operation that ends before its snapshot (`continuing`, or
/// `withdrawn` out of a `blocked` one) declines its planning request in the
/// same move, with the outcome as the reason, as `requests`, `replans` and
/// `show` say: no planner is opened for it and it is not left `open`. One
/// `blocked` before its snapshot that goes on to it keeps its request for
/// the planner, and one closed after its snapshot leaves the request as
/// it was.
#[test]
fn a_request_whose_operation_ends_before_its_snapshot_is_declined_with_the_outcome() {
    let (_dir, db) = queue();
    let (mut queue, a, _, run) = claimed(&db);
    let open = |queue: &SqliteQueue, request: i64| {
        queue
            .planner_requests()
            .unwrap()
            .iter()
            .any(|r| r.id.as_i64() == request)
    };
    let recorded = |words: &str| {
        let recorded = ok_as("inbox", &db, &args(&request(&run, words)));
        assert_eq!(recorded["created"], true);
        (
            ReplanId::new(recorded["operation"]["id"].as_i64().unwrap()),
            recorded["request"]["id"].as_i64().unwrap(),
        )
    };
    let moved = |queue: &mut SqliteQueue,
                 id: ReplanId,
                 from: i64,
                 to: ReplanStatus,
                 snapshot: Option<String>| {
        queue
            .move_replan(
                id,
                from,
                &ReplanMove {
                    to,
                    reason: format!("the test moves it to {}", to.as_str()),
                    snapshot_id: snapshot,
                    diagnosis: None,
                },
            )
            .unwrap()
            .generation
    };

    // The run lands first, before the park: the operation continues and
    // its request is declined with that outcome.
    let (first, first_request) = recorded("the review keeps sending it back");
    let generation = moved(&mut queue, first, 1, ReplanStatus::PauseRequested, None);
    moved(
        &mut queue,
        first,
        generation,
        ReplanStatus::Continuing,
        None,
    );
    let requests = ok_as("observer", &db, &["requests", &first_request.to_string()]);
    let declined = &requests["requests"][0];
    assert_eq!(declined["status"], "declined");
    assert_eq!(
        declined["status_reason"],
        format!(
            "replan operation {first} ended continuing before its snapshot: \
             the test moves it to continuing"
        )
    );
    assert!(!open(&queue, first_request));
    let replans = ok_as("observer", &db, &["replans", &first.to_string()]);
    assert_eq!(replans["replans"][0]["request_status"], "declined");
    let shown = ok(&db, &["show", &a.to_string()]);
    assert_eq!(shown["replans"][0]["request_status"], "declined");
    let conn = Connection::open(&db).unwrap();
    let event = |kind: &str| -> Vec<Value> {
        conn.prepare("SELECT payload FROM run_events WHERE kind=?1 ORDER BY id")
            .unwrap()
            .query_map([kind], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
            .collect()
    };
    let told = event("request_declined");
    assert_eq!(told.len(), 1);
    assert_eq!(told[0]["request_id"], first_request);
    assert_eq!(told[0]["replan_operation_id"], first.as_i64());
    assert_eq!(told[0]["replan_status"], "continuing");
    assert_eq!(told[0]["by"], "runtime");
    assert!(event("replan_status_changed").iter().any(
        |changed| changed["to"] == "continuing" && changed["request_declined"] == first_request
    ));

    // Blocked before the park, then withdrawn: declined as well.
    let (second, second_request) = recorded("split it");
    let generation = moved(&mut queue, second, 1, ReplanStatus::Diagnosing, None);
    let generation = moved(&mut queue, second, generation, ReplanStatus::Blocked, None);
    assert!(!open(&queue, second_request));
    assert_eq!(
        queue.replan_operation(second).unwrap().request_status,
        Some(dagq::domain::plan_request::RequestStatus::Open)
    );
    moved(
        &mut queue,
        second,
        generation,
        ReplanStatus::Withdrawn,
        None,
    );
    let withdrawn = queue
        .plan_request(dagq::domain::RequestId::new(second_request))
        .unwrap();
    assert_eq!(
        withdrawn.status,
        dagq::domain::plan_request::RequestStatus::Declined
    );
    assert!(
        withdrawn
            .status_reason
            .unwrap()
            .contains("ended withdrawn before its snapshot")
    );
    assert!(!open(&queue, second_request));

    // Blocked before its snapshot, then on to it: the request waits for the
    // planner, and closing after the snapshot leaves it as it was.
    let (third, third_request) = recorded("split it again");
    let generation = moved(&mut queue, third, 1, ReplanStatus::PauseRequested, None);
    let generation = moved(&mut queue, third, generation, ReplanStatus::Blocked, None);
    let generation = moved(
        &mut queue,
        third,
        generation,
        ReplanStatus::Snapshotting,
        None,
    );
    let snapshot = format!("replan-{third}-g{generation}");
    let generation = moved(
        &mut queue,
        third,
        generation,
        ReplanStatus::Parked,
        Some(snapshot),
    );
    assert!(open(&queue, third_request));
    moved(
        &mut queue,
        third,
        generation,
        ReplanStatus::ContinuingAfterPark,
        None,
    );
    assert_eq!(
        queue
            .plan_request(dagq::domain::RequestId::new(third_request))
            .unwrap()
            .status,
        dagq::domain::plan_request::RequestStatus::Open
    );
    assert_eq!(event("request_declined").len(), 2);
}

/// A task has one open operation, on its latest run while it does not
/// guard the task: when the run it was opened on ends and the task is
/// claimed again before the operation moved on, the new run takes it over
/// (with its planning request) at the claim,
/// `replans`, `list`, `show`, `status` and `doctor` show it on the new run,
/// a request on the new run joins it, and one on the ended run is refused.
#[test]
fn a_new_run_of_the_task_takes_over_its_open_operation() {
    let (_dir, db) = queue();
    let (mut queue, a, _, first) = claimed(&db);
    let opened = ok(
        &db,
        &args(&request(&first, "the review keeps sending it back")),
    );
    let id = opened["operation"]["id"].as_i64().unwrap();
    let request_id = opened["request"]["id"].clone();

    // The run ends while the operation is a candidate, and the task is
    // claimed again.
    let conn = Connection::open(&db).unwrap();
    conn.execute("DELETE FROM run_leases WHERE run_id=?1", [first.as_str()])
        .unwrap();
    conn.execute(
        "UPDATE task_runs SET status='failed' WHERE id=?1",
        [first.as_str()],
    )
    .unwrap();
    conn.execute("UPDATE tasks SET status='ready' WHERE id=?1", [a.as_i64()])
        .unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!("claim")
    };
    assert_eq!(run.task_id(), a);
    let second = run.id().clone();
    assert_ne!(second, first);

    let carried = queue.replan_operation(ReplanId::new(id)).unwrap();
    assert_eq!(carried.run_id, second);
    assert_eq!(carried.status, ReplanStatus::Candidate);
    assert_eq!(carried.generation, 2);
    assert_eq!(json!(carried.request_id), request_id);
    let refs: Value = conn
        .query_row(
            "SELECT refs FROM plan_requests WHERE id=?1",
            [request_id.as_i64().unwrap()],
            |r| Ok(serde_json::from_str(&r.get::<_, String>(0)?).unwrap()),
        )
        .unwrap();
    for run in [&first, &second] {
        assert!(
            refs.as_array()
                .unwrap()
                .contains(&json!({"id": run.as_str(), "kind": "run"})),
            "{refs}"
        );
    }
    let moved: Value = conn
        .query_row(
            "SELECT payload FROM run_events WHERE kind=?1 AND run_id=?2",
            [EventKind::ReplanCarried.as_str(), second.as_str()],
            |r| Ok(serde_json::from_str(&r.get::<_, String>(0)?).unwrap()),
        )
        .unwrap();
    assert_eq!(
        moved,
        json!({"operation_id": id, "from_run": first.as_str(), "to_run": second.as_str(),
               "generation": 2})
    );

    // A request on the new run joins it; one on the ended run is refused.
    let again = ok(&db, &args(&request(&second, "still long")));
    assert_eq!(again["created"], false);
    assert_eq!(again["operation"]["id"], id);
    assert_eq!(again["request"]["id"], request_id);
    let (refused, error) = as_role(&db, "user", None, &args(&request(&first, "late")));
    assert!(!refused, "{error}");
    let open: i64 = conn
        .query_row(
            "SELECT count(*) FROM replan_operations WHERE task_id=?1 AND closed_at IS NULL",
            [a.as_i64()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(open, 1);

    // Every reading shows the task's one operation, on the new run.
    let listed = ok(&db, &["replans"]);
    assert_eq!(listed["replans"].as_array().unwrap().len(), 1);
    assert_eq!(listed["replans"][0]["id"], id);
    assert_eq!(listed["replans"][0]["run_id"], second.as_str());
    let task = ok(&db, &["list"])["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == a.as_i64())
        .unwrap()["replan"]
        .clone();
    assert_eq!(task["operation"], id);
    let shown = ok(&db, &["show", &a.to_string()]);
    assert_eq!(shown["replans"].as_array().unwrap().len(), 1);
    assert_eq!(shown["replans"][0]["run_id"], second.as_str());
    for report in [ok(&db, &["status"]), ok(&db, &["doctor"])] {
        let open = report["replans"]["open"].as_array().unwrap();
        assert_eq!(open.len(), 1, "{report}");
        assert_eq!(open[0]["id"], id);
    }
}

/// Two operations open on one task, one per run as the rule before kept
/// them, the earlier one `blocked` with its run's work: a request on the
/// latest run closes the later candidate as superseded and joins the
/// blocked one, which stays on the run it parked and is what the guard,
/// `list` and `replans` name.
#[test]
fn a_request_closes_a_candidate_beside_a_blocked_operation_and_joins_it() {
    let (_dir, db) = queue();
    let (queue, a, _, first) = claimed(&db);
    let blocked = ok(&db, &args(&request(&first, "split it")))["operation"]["id"]
        .as_i64()
        .unwrap();
    // The state the rule of one per run could leave (by hand): the first
    // run's operation blocked, the task claimed again, and a candidate on
    // the second run.
    let conn = Connection::open(&db).unwrap();
    let second = RunId::new("00000000-0000-4000-8000-000000000002").unwrap();
    conn.execute_batch(&format!(
        "DELETE FROM run_leases WHERE run_id='{first}';
         UPDATE task_runs SET status='failed' WHERE id='{first}';
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('{second}',{a},'running','claude','claude','{}');
         UPDATE replan_operations SET status='blocked', generation=2 WHERE id={blocked};
         INSERT INTO replan_operations(task_id, run_id, goal_id, origin, status, generation,
           reason, conditions, task_version, goal_acceptance_version, diagnosis,
           requested_by, requested_by_id, created_at, updated_at)
         SELECT task_id, '{second}', goal_id, 'worker', 'candidate', 1, 'again', conditions,
           task_version, goal_acceptance_version, diagnosis, requested_by, requested_by_id,
           created_at, updated_at FROM replan_operations WHERE id={blocked};",
        "a".repeat(40),
    ))
    .unwrap();
    let candidate = conn.last_insert_rowid();

    let joined = ok(&db, &args(&request(&second, "still long")));
    assert_eq!(joined["created"], false);
    assert_eq!(joined["operation"]["id"], blocked);
    let closed = queue.replan_operation(ReplanId::new(candidate)).unwrap();
    assert_eq!(closed.status, ReplanStatus::Continuing);
    assert!(closed.closed_at.is_some());
    let kept = queue.replan_operation(ReplanId::new(blocked)).unwrap();
    assert_eq!(kept.status, ReplanStatus::Blocked);
    assert_eq!(kept.run_id, first);
    assert_eq!(kept.generation, 2);
    assert_eq!(
        queue.replan_guard(a).unwrap().unwrap().operation.as_i64(),
        blocked
    );
    let listed = ok(&db, &["replans"]);
    assert_eq!(listed["replans"].as_array().unwrap().len(), 1);
    assert_eq!(listed["replans"][0]["id"], blocked);
    let task = ok(&db, &["list"])["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == a.as_i64())
        .unwrap()["replan"]
        .clone();
    assert_eq!(task["operation"], blocked);
}

/// Two candidates open on one task, one per run as the rule before kept
/// them: the runtime fences neither while the other is open, so two never
/// guard one task; a request on the latest run closes the earlier one as
/// superseded (its planning request declined), after which the task's one
/// operation is fenced.
#[test]
fn no_second_operation_of_a_task_is_fenced_and_a_request_leaves_one() {
    let (_dir, db) = queue();
    let (mut queue, a, _, first) = claimed(&db);
    let earlier = ok(&db, &args(&request(&first, "split it")))["operation"]["id"]
        .as_i64()
        .unwrap();
    let conn = Connection::open(&db).unwrap();
    let second = RunId::new("00000000-0000-4000-8000-000000000003").unwrap();
    conn.execute_batch(&format!(
        "DELETE FROM run_leases WHERE run_id='{first}';
         UPDATE task_runs SET status='failed' WHERE id='{first}';
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('{second}',{a},'running','claude','claude','{}');
         INSERT INTO replan_operations(task_id, run_id, goal_id, origin, status, generation,
           reason, conditions, task_version, goal_acceptance_version, diagnosis,
           requested_by, requested_by_id, created_at, updated_at)
         SELECT task_id, '{second}', goal_id, 'worker', 'candidate', 1, 'again', conditions,
           task_version, goal_acceptance_version, diagnosis, requested_by, requested_by_id,
           created_at, updated_at FROM replan_operations WHERE id={earlier};",
        "a".repeat(40),
    ))
    .unwrap();
    let later = conn.last_insert_rowid();
    let fence = ReplanMove {
        to: ReplanStatus::PauseRequested,
        reason: "split it".into(),
        snapshot_id: None,
        diagnosis: None,
    };
    for id in [earlier, later] {
        let refused = queue
            .move_replan(ReplanId::new(id), 1, &fence)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("has operation"), "{refused}");
        assert_eq!(
            queue.replan_operation(ReplanId::new(id)).unwrap().status,
            ReplanStatus::Candidate
        );
    }
    assert_eq!(queue.replan_guard(a).unwrap(), None);

    let joined = ok(&db, &args(&request(&second, "still long")));
    assert_eq!(joined["created"], false);
    assert_eq!(joined["operation"]["id"], later);
    let closed = queue.replan_operation(ReplanId::new(earlier)).unwrap();
    assert_eq!(closed.status, ReplanStatus::Continuing);
    let request = closed.request_id.unwrap();
    assert_eq!(
        queue.plan_request(request).unwrap().status,
        dagq::domain::plan_request::RequestStatus::Declined
    );
    assert_eq!(
        ok(&db, &["replans"])["replans"].as_array().unwrap().len(),
        1
    );

    let fenced = queue.move_replan(ReplanId::new(later), 1, &fence).unwrap();
    assert_eq!(fenced.status, ReplanStatus::PauseRequested);
    assert_eq!(
        queue.replan_guard(a).unwrap().unwrap().operation.as_i64(),
        later
    );
    for report in [ok(&db, &["status"]), ok(&db, &["doctor"])] {
        let open = report["replans"]["open"].as_array().unwrap();
        assert_eq!(open.len(), 1, "{report}");
        assert_eq!(open[0]["id"], later);
    }
}
