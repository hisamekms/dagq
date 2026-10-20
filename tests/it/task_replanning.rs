//! The replanning operations of a run (ADR-t1521-1, ADR-t1521-2) through
//! the CLI and the queue: a person's request and a worker's proposal on
//! its own run open one operation per run with the task's and its goal's
//! conditions as they were, which a reopened queue reads back unchanged;
//! only the trusted runtime moves an operation on; and while one guards
//! its task, `ready`, `edit`, `cancel`, a change of its dependencies, a
//! resume, a landing and the claim are refused, as `status`, `doctor`,
//! `list` and `show` say.

use crate::common::cli::{invoke_with, ok, ok_as, queue};
use crate::common::queue::{base, new_goal, new_task};
use dagq::domain::EventKind;
use dagq::{
    application::{PlanRequestStore, TaskReads, TaskStore, commands::replans::Replans},
    domain::{
        ActorContext, ActorRole, ClaimOutcome, GoalEdit, LeaseToken, ReplanId, RunId, StaticPolicy,
        TaskAction, TaskId,
        task_replan::{ReplanMove, ReplanStatus},
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
