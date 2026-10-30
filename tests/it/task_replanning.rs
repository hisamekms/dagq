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

use crate::common::cli::{ok, ok_as, queue};
use crate::common::queue::{base, new_task};
use crate::common::replans::{args, as_role, claimed, denials, park, request};
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

/// A replanning operation that ends with no planner of the runtime's open for its
/// planning request, before its snapshot (`continuing`, or `withdrawn` out
/// of a `blocked` one) or after it (`continuing_after_park` out of
/// `parked`, `withdrawn` out of a `blocked` one that parked), declines the
/// request in the same move, with the outcome as the reason, as
/// `requests`, `replans` and `show` say: no planner is opened for it and it
/// is not left `open`. One `blocked` before its snapshot that goes on to it
/// keeps its request for the planner. A closed one whose restore failed,
/// back in `blocked`, that goes on to the planner gets a new request and
/// one planner for it; one closed while its planner is open leaves the
/// request to it, and declines it when the planner ends undecided.
#[test]
fn a_request_whose_operation_closes_before_its_planner_is_declined_with_the_outcome() {
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
            "replan operation {first} ended continuing with no planner of the runtime's open \
             for it: the test moves it to continuing"
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
            .contains("ended withdrawn with no planner of the runtime's open for it")
    );
    assert!(!open(&queue, second_request));

    // Blocked before its snapshot, then on to it: the request waits for the
    // planner; closed after the snapshot with no planner opened, it is
    // declined as well.
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
    let generation = moved(
        &mut queue,
        third,
        generation,
        ReplanStatus::ContinuingAfterPark,
        None,
    );
    let continued =
        &ok_as("observer", &db, &["requests", &third_request.to_string()])["requests"][0];
    assert_eq!(continued["status"], "declined");
    assert_eq!(
        continued["status_reason"],
        format!(
            "replan operation {third} ended continuing_after_park with no planner of the \
             runtime's open for it: the test moves it to continuing_after_park"
        )
    );
    assert!(!open(&queue, third_request));
    assert_eq!(
        ok_as("observer", &db, &["replans", &third.to_string()])["replans"][0]["request_status"],
        "declined"
    );
    assert_eq!(event("request_declined").len(), 3);

    // Its restore fails: back in `blocked`, the declined request stays as
    // it is and no planner is opened; through the snapshot again on to the
    // planner, a new request is recorded and gets one planner.
    let generation = moved(&mut queue, third, generation, ReplanStatus::Blocked, None);
    assert!(queue.planner_requests().unwrap().is_empty());
    let generation = moved(
        &mut queue,
        third,
        generation,
        ReplanStatus::Snapshotting,
        None,
    );
    assert_eq!(
        queue
            .replan_operation(third)
            .unwrap()
            .request_id
            .unwrap()
            .as_i64(),
        third_request
    );
    let snapshot = format!("replan-{third}-g{generation}");
    let generation = moved(
        &mut queue,
        third,
        generation,
        ReplanStatus::Parked,
        Some(snapshot),
    );
    let op = queue.replan_operation(third).unwrap();
    let renewed = op.request_id.unwrap();
    assert_ne!(renewed.as_i64(), third_request);
    assert_eq!(
        op.request_status,
        Some(dagq::domain::plan_request::RequestStatus::Open)
    );
    let old = queue
        .plan_request(dagq::domain::RequestId::new(third_request))
        .unwrap();
    assert_eq!(
        old.status,
        dagq::domain::plan_request::RequestStatus::Declined
    );
    let new = queue.plan_request(renewed).unwrap();
    assert_eq!(new.text, old.text);
    assert_eq!(new.refs, old.refs);
    assert_eq!(new.replan_operation, Some(third));
    assert!(
        event("request_recorded")
            .iter()
            .any(|recorded| recorded["request_id"] == renewed.as_i64()
                && recorded["renews_request"] == third_request)
    );
    assert!(
        event("replan_status_changed")
            .iter()
            .any(|changed| changed["to"] == "parked"
                && changed["request_renewed"] == renewed.as_i64())
    );
    let targets: Vec<_> = queue
        .planner_requests()
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(targets, [renewed]);
    let dagq::application::RequestPlannerStart::Opened { planner, .. } =
        queue.open_request_planner(renewed, None).unwrap()
    else {
        panic!("no planner was opened for request {renewed}")
    };
    assert!(matches!(
        queue
            .open_request_planner(dagq::domain::RequestId::new(third_request), None)
            .unwrap(),
        dagq::application::RequestPlannerStart::Skipped
    ));
    assert!(matches!(
        queue.open_request_planner(renewed, None).unwrap(),
        dagq::application::RequestPlannerStart::Skipped
    ));
    assert!(queue.planner_requests().unwrap().is_empty());

    // Closed while its planner is open: the request is its planner's to
    // end, left as it was; the planner ends without deciding it, and its
    // row's close declines it, naming the planner.
    moved(
        &mut queue,
        third,
        generation,
        ReplanStatus::ContinuingAfterPark,
        None,
    );
    assert_eq!(
        queue.plan_request(renewed).unwrap().status,
        dagq::domain::plan_request::RequestStatus::Open
    );
    assert_eq!(event("request_declined").len(), 3);
    assert!(
        queue
            .end_planner(planner.id, &json!({"code": "runtime_exited"}))
            .unwrap()
    );
    let ended = queue.plan_request(renewed).unwrap();
    assert_eq!(
        ended.status,
        dagq::domain::plan_request::RequestStatus::Declined
    );
    assert_eq!(
        ended.status_reason.unwrap(),
        format!(
            "replan operation {third} ended continuing_after_park while planner {} of the \
             runtime was open, and it ended without deciding the request: the test moves it \
             to continuing_after_park",
            planner.id
        )
    );
    let told = event("request_declined");
    assert_eq!(told.len(), 4);
    assert_eq!(told[3]["request_id"], renewed.as_i64());
    assert_eq!(told[3]["by"], "runtime");
    assert_eq!(told[3]["replan_operation_id"], third.as_i64());
    assert_eq!(told[3]["ended_planner_id"], planner.id.as_i64());

    // Parked, blocked and withdrawn with no planner opened: declined.
    let (fourth, fourth_request) = recorded("split it once more");
    let generation = moved(&mut queue, fourth, 1, ReplanStatus::PauseRequested, None);
    let generation = moved(
        &mut queue,
        fourth,
        generation,
        ReplanStatus::Snapshotting,
        None,
    );
    let snapshot = format!("replan-{fourth}-g{generation}");
    let generation = moved(
        &mut queue,
        fourth,
        generation,
        ReplanStatus::Parked,
        Some(snapshot),
    );
    let generation = moved(&mut queue, fourth, generation, ReplanStatus::Blocked, None);
    assert!(!open(&queue, fourth_request));
    assert_eq!(
        queue.replan_operation(fourth).unwrap().request_status,
        Some(dagq::domain::plan_request::RequestStatus::Open)
    );
    moved(
        &mut queue,
        fourth,
        generation,
        ReplanStatus::Withdrawn,
        None,
    );
    let withdrawn = queue
        .plan_request(dagq::domain::RequestId::new(fourth_request))
        .unwrap();
    assert_eq!(
        withdrawn.status,
        dagq::domain::plan_request::RequestStatus::Declined
    );
    assert!(
        withdrawn
            .status_reason
            .unwrap()
            .contains("ended withdrawn with no planner of the runtime's open for it")
    );
    assert!(!open(&queue, fourth_request));
    assert_eq!(
        ok(&db, &["show", &a.to_string()])["replans"]
            .as_array()
            .unwrap()
            .iter()
            .find(|op| op["id"] == fourth.as_i64())
            .unwrap()["request_status"],
        "declined"
    );
    assert_eq!(event("request_declined").len(), 5);
}

/// A replanning operation whose request's planner ended without deciding
/// it (the request still `open`, under the planners' limit) and that then
/// closes (`continuing_after_park` out of `planning`, `withdrawn` out of a
/// `blocked` one) declines the request in the same move, with the outcome
/// as the reason, as `requests`, `replans` and `show` say. Its restore
/// failing, back in `blocked` and on to the planner again, a new request is
/// recorded and gets one planner.
#[test]
fn a_request_whose_planner_ended_undecided_is_declined_when_its_operation_closes() {
    use dagq::application::RequestPlannerStart;
    use dagq::domain::{RequestId, plan_request::RequestStatus};
    let (_dir, db) = queue();
    let (mut queue, a, _, run) = claimed(&db);
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
    // Recorded, parked, with the planner: a planner is opened and ends
    // without deciding the request, which stays `open`.
    let planned = |queue: &mut SqliteQueue, words: &str| {
        let recorded = ok_as("inbox", &db, &args(&request(&run, words)));
        let id = ReplanId::new(recorded["operation"]["id"].as_i64().unwrap());
        let request = RequestId::new(recorded["request"]["id"].as_i64().unwrap());
        let generation = moved(queue, id, 1, ReplanStatus::PauseRequested, None);
        let generation = moved(queue, id, generation, ReplanStatus::Snapshotting, None);
        let snapshot = format!("replan-{id}-g{generation}");
        let generation = moved(queue, id, generation, ReplanStatus::Parked, Some(snapshot));
        let RequestPlannerStart::Opened { planner, .. } =
            queue.open_request_planner(request, None).unwrap()
        else {
            panic!("no planner was opened for request {request}")
        };
        // The planner's opening moved the operation on to it.
        let op = queue.replan_operation(id).unwrap();
        assert_eq!(op.status, ReplanStatus::Planning);
        assert_eq!(op.generation, generation);
        assert!(
            queue
                .end_planner(planner.id, &json!({"code": "runtime_exited"}))
                .unwrap()
        );
        assert_eq!(
            queue.plan_request(request).unwrap().status,
            RequestStatus::Open
        );
        (id, request, generation)
    };
    let declined = |queue: &SqliteQueue, id: ReplanId, request: RequestId, outcome: &str| {
        let listed = &ok_as("observer", &db, &["requests", &request.to_string()])["requests"][0];
        assert_eq!(listed["status"], "declined");
        assert_eq!(
            listed["status_reason"],
            format!(
                "replan operation {id} ended {outcome} with no planner of the runtime's open \
                 for it: the test moves it to {outcome}"
            )
        );
        assert!(
            !queue
                .planner_requests()
                .unwrap()
                .iter()
                .any(|r| r.id == request)
        );
        assert_eq!(
            ok_as("observer", &db, &["replans", &id.to_string()])["replans"][0]["request_status"],
            "declined"
        );
        assert_eq!(
            ok(&db, &["show", &a.to_string()])["replans"]
                .as_array()
                .unwrap()
                .iter()
                .find(|op| op["id"] == id.as_i64())
                .unwrap()["request_status"],
            "declined"
        );
    };

    // Continued after the park: declined.
    let (first, first_request, generation) = planned(&mut queue, "split it");
    let generation = moved(
        &mut queue,
        first,
        generation,
        ReplanStatus::ContinuingAfterPark,
        None,
    );
    declined(&queue, first, first_request, "continuing_after_park");

    // Its restore fails: back in `blocked`, no planner; on to the planner
    // again, a new request gets exactly one planner.
    let generation = moved(&mut queue, first, generation, ReplanStatus::Blocked, None);
    assert!(queue.planner_requests().unwrap().is_empty());
    let generation = moved(
        &mut queue,
        first,
        generation,
        ReplanStatus::Snapshotting,
        None,
    );
    let snapshot = format!("replan-{first}-g{generation}");
    let generation = moved(
        &mut queue,
        first,
        generation,
        ReplanStatus::Parked,
        Some(snapshot),
    );
    let renewed = queue.replan_operation(first).unwrap().request_id.unwrap();
    assert_ne!(renewed, first_request);
    assert_eq!(
        queue.plan_request(renewed).unwrap().replan_operation,
        Some(first)
    );
    let targets: Vec<_> = queue
        .planner_requests()
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(targets, [renewed]);
    let RequestPlannerStart::Opened {
        planner, attempt, ..
    } = queue.open_request_planner(renewed, None).unwrap()
    else {
        panic!("no planner was opened for request {renewed}")
    };
    assert_eq!(attempt, 1);
    for request in [renewed, first_request] {
        assert!(matches!(
            queue.open_request_planner(request, None).unwrap(),
            RequestPlannerStart::Skipped
        ));
    }
    // That planner submits nothing either; withdrawn after it ended, the
    // new request is declined too.
    assert_eq!(
        queue.replan_operation(first).unwrap().status,
        ReplanStatus::Planning
    );
    assert!(
        queue
            .end_planner(planner.id, &json!({"code": "runtime_exited"}))
            .unwrap()
    );
    let generation = moved(&mut queue, first, generation, ReplanStatus::Blocked, None);
    moved(&mut queue, first, generation, ReplanStatus::Withdrawn, None);
    declined(&queue, first, renewed, "withdrawn");

    // Blocked with the planner after its planner ended, then withdrawn:
    // declined.
    let (second, second_request, generation) = planned(&mut queue, "split it again");
    let generation = moved(&mut queue, second, generation, ReplanStatus::Blocked, None);
    moved(
        &mut queue,
        second,
        generation,
        ReplanStatus::Withdrawn,
        None,
    );
    declined(&queue, second, second_request, "withdrawn");
    assert!(queue.planner_requests().unwrap().is_empty());
}

/// A request whose planner ended waiting for the answer of its
/// `planner_question` and that the runtime declines as its operation closes
/// has that question closed by the runtime in the same move: before the
/// close with no planner open (`decline_undecided`, an answered question
/// `ask_closed`) and at the close of the row of a planner open at the
/// operation's close (`decline_after_planner`, an open one answered by the
/// runtime), each named by `request_declined`. While the operation is open,
/// or once the request ended otherwise, the question stays open.
#[test]
fn a_request_declined_as_its_operation_closes_closes_its_planner_question() {
    use dagq::application::RequestPlannerStart;
    use dagq::domain::{AskKind, AskReason, NewAsk, RequestId, plan_request::RequestStatus};
    let (_dir, db) = queue();
    let (mut queue, _, _, run) = claimed(&db);
    let moved = |queue: &mut SqliteQueue, id: ReplanId, from: i64, to: ReplanStatus| {
        queue
            .move_replan(
                id,
                from,
                &ReplanMove {
                    to,
                    reason: format!("the test moves it to {}", to.as_str()),
                    snapshot_id: (to == ReplanStatus::Parked)
                        .then(|| format!("replan-{id}-g{from}")),
                    diagnosis: None,
                },
            )
            .unwrap()
            .generation
    };
    // Recorded, parked, in planning: its planner asks a question about the
    // request and is marked to wait for the answer.
    let asked = |queue: &mut SqliteQueue, words: &str| {
        let recorded = ok_as("inbox", &db, &args(&request(&run, words)));
        let id = ReplanId::new(recorded["operation"]["id"].as_i64().unwrap());
        let request = RequestId::new(recorded["request"]["id"].as_i64().unwrap());
        let generation = moved(queue, id, 1, ReplanStatus::PauseRequested);
        let generation = moved(queue, id, generation, ReplanStatus::Snapshotting);
        let generation = moved(queue, id, generation, ReplanStatus::Parked);
        let RequestPlannerStart::Opened { planner, .. } =
            queue.open_request_planner(request, None).unwrap()
        else {
            panic!("no planner was opened for request {request}")
        };
        // The planner's opening moved the operation on to it.
        let op = queue.replan_operation(id).unwrap();
        assert_eq!(op.status, ReplanStatus::Planning);
        assert_eq!(op.generation, generation);
        let ask = queue
            .ask(NewAsk {
                recommendation: None,
                confidence: None,
                topics: Vec::new(),
                kind: AskKind::PlannerQuestion,
                task_id: None,
                run_id: None,
                question: "split it how?".into(),
                options: vec!["in two".into(), "in three".into()],
                asked_by: "planner".into(),
                reason_category: AskReason::Scope,
                finding_id: None,
                request_id: Some(request),
            })
            .unwrap()
            .ask;
        assert!(
            queue
                .planner_answer_wait(planner.id, &[ask.id], &json!({"planner_id": planner.id}))
                .unwrap()
        );
        (id, request, planner.id, ask.id, generation)
    };
    let conn = Connection::open(&db).unwrap();
    let events = |kind: &str| -> Vec<Value> {
        conn.prepare("SELECT payload FROM run_events WHERE kind=?1 ORDER BY id")
            .unwrap()
            .query_map([kind], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
            .collect()
    };
    let open_asks = || -> Vec<i64> {
        ok_as("inbox", &db, &["asks"])["asks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|ask| ask["id"].as_i64().unwrap())
            .collect()
    };
    let in_inbox = |ask: i64| {
        ok_as("inbox", &db, &["status", "--role", "inbox"])
            .to_string()
            .contains(&format!("\"ask_id\":{ask}"))
    };
    let declined_naming = |request: RequestId| -> Value {
        events("request_declined")
            .into_iter()
            .find(|told| told["request_id"] == request.as_i64())
            .unwrap_or_else(|| panic!("request {request} was not declined"))
    };

    // Its planner ends while the operation is open: the request and its
    // question stay open. The person answers; then the operation closes
    // with no planner open, and the answered question is closed with the
    // declined request.
    let (first, first_request, planner, first_ask, generation) = asked(&mut queue, "split it");
    queue
        .end_planner(planner, &json!({"code": "answer_wait"}))
        .unwrap();
    assert_eq!(
        queue.plan_request(first_request).unwrap().status,
        RequestStatus::Open
    );
    assert!(open_asks().contains(&first_ask.as_i64()));
    queue.answer(first_ask, "in two").unwrap();
    assert!(queue.read_ask(first_ask).unwrap().closed_at.is_none());
    assert!(in_inbox(first_ask.as_i64()));
    moved(
        &mut queue,
        first,
        generation,
        ReplanStatus::ContinuingAfterPark,
    );
    assert_eq!(
        queue.plan_request(first_request).unwrap().status,
        RequestStatus::Declined
    );
    let closed = queue.read_ask(first_ask).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(closed.answer.as_deref(), Some("in two"));
    assert!(!open_asks().contains(&first_ask.as_i64()));
    assert!(!in_inbox(first_ask.as_i64()));
    assert!(
        events("ask_closed")
            .iter()
            .any(|e| e["ask_id"] == first_ask.as_i64())
    );
    let told = declined_naming(first_request);
    assert_eq!(told["by"], "runtime");
    assert_eq!(told["replan_operation_id"], first.as_i64());
    assert_eq!(told["closed_asks"], json!([first_ask.as_i64()]));

    // The operation closes while its planner, waiting for the answer, is
    // open: the request and its question stay open until the planner's row
    // closes, which declines the request and answers the question by the
    // runtime with the reason.
    let (second, second_request, planner, second_ask, generation) =
        asked(&mut queue, "split it again");
    moved(
        &mut queue,
        second,
        generation,
        ReplanStatus::ContinuingAfterPark,
    );
    assert_eq!(
        queue.plan_request(second_request).unwrap().status,
        RequestStatus::Open
    );
    assert!(open_asks().contains(&second_ask.as_i64()));
    queue
        .end_planner(planner, &json!({"code": "answer_wait"}))
        .unwrap();
    let declined = queue.plan_request(second_request).unwrap();
    assert_eq!(declined.status, RequestStatus::Declined);
    let closed = queue.read_ask(second_ask).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(closed.answer, declined.status_reason);
    assert!(!open_asks().contains(&second_ask.as_i64()));
    assert!(!in_inbox(second_ask.as_i64()));
    assert!(
        events("ask_answered")
            .iter()
            .any(|e| e["ask_id"] == second_ask.as_i64() && e["runtime_closed"] == true)
    );
    let told = declined_naming(second_request);
    assert_eq!(told["ended_planner_id"], planner.as_i64());
    assert_eq!(told["closed_asks"], json!([second_ask.as_i64()]));

    // A request declined otherwise before its operation closes: the
    // operation's close leaves the request and its question as they are.
    let (third, third_request, planner, third_ask, generation) =
        asked(&mut queue, "split it once more");
    queue
        .end_planner(planner, &json!({"code": "answer_wait"}))
        .unwrap();
    queue
        .decline_request(third_request, "not now", "user", None)
        .unwrap();
    moved(
        &mut queue,
        third,
        generation,
        ReplanStatus::ContinuingAfterPark,
    );
    assert_eq!(
        queue
            .plan_request(third_request)
            .unwrap()
            .status_reason
            .as_deref(),
        Some("not now")
    );
    assert!(queue.read_ask(third_ask).unwrap().closed_at.is_none());
    assert!(open_asks().contains(&third_ask.as_i64()));
    assert!(in_inbox(third_ask.as_i64()));
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

/// Each diagnosis job, and each ask of one, claims its operation under a
/// new generation: of two supervisors that read the operation alike only
/// one claims it, and one restarted claims it from what it reads again.
/// The move that applies a diagnosis checks its premise in its own
/// transaction: a review, a turn or a receipt after the last event it
/// read, the run's landing begun, an edit of the task or a hold open on
/// it refuses it (a hold also refuses the claim); a premise that holds
/// lets it through.
#[test]
fn a_diagnosis_is_claimed_once_and_applied_only_on_its_premise() {
    use dagq::domain::task_replan::{Diagnosis, DiagnosisPremise};
    let (_dir, db) = queue();
    let (mut queue, a, _, run) = claimed(&db);
    let opened = ok(
        &db,
        &args(&request(&run, "the review keeps sending it back")),
    );
    let id = ReplanId::new(opened["operation"]["id"].as_i64().unwrap());
    let read = queue.replan_operation(id).unwrap().generation;
    let claim = |queue: &mut SqliteQueue, generation: i64| {
        queue.move_replan(
            id,
            generation,
            &ReplanMove {
                to: ReplanStatus::Diagnosing,
                reason: "its diagnosis starts".into(),
                snapshot_id: None,
                diagnosis: None,
            },
        )
    };
    let first = claim(&mut queue, read).unwrap();
    assert_eq!(first.generation, read + 1);
    assert!(
        claim(&mut queue, read).is_err(),
        "a second supervisor's claim of the same read"
    );
    let claimed = claim(&mut queue, first.generation).unwrap();
    assert_eq!(claimed.generation, read + 2);

    let conn = Connection::open(&db).unwrap();
    let newest = |conn: &Connection| -> i64 {
        conn.query_row(
            "SELECT MAX(id) FROM run_events WHERE task_id=?1",
            [a.as_i64()],
            |r| r.get(0),
        )
        .unwrap()
    };
    let status = queue.show(a).unwrap().runs[0].status();
    let diagnosed = |read_until: i64| ReplanMove {
        to: ReplanStatus::Continuing,
        reason: "diagnosed: it goes on".into(),
        snapshot_id: None,
        diagnosis: Some(Diagnosis {
            premise: Some(DiagnosisPremise {
                run_status: status,
                read_until: Some(dagq::domain::EventId::new(read_until)),
                head: None,
                manual: None,
                max_auto_parks: None,
            }),
            ..Diagnosis::default()
        }),
    };
    let refused = |queue: &mut SqliteQueue, read_until: i64, why: &str| {
        let error = queue
            .move_replan(id, claimed.generation, &diagnosed(read_until))
            .unwrap_err();
        assert!(format!("{error:#}").contains(why), "{error:#}");
        assert_eq!(
            queue.replan_operation(id).unwrap().status,
            ReplanStatus::Diagnosing
        );
    };

    // A review came while it was diagnosed.
    let read_until = newest(&conn);
    queue
        .record_runtime_event(&run, EventKind::ReviewFinished, json!({"verdict": "pass"}))
        .unwrap();
    refused(&mut queue, read_until, "review_finished event");

    // A turn ended, or a receipt came, while it was diagnosed.
    for (kind, name) in [
        (EventKind::TurnFinished, "turn_finished event"),
        (EventKind::ReceiptObserved, "receipt_observed event"),
    ] {
        let read_until = newest(&conn);
        queue.record_runtime_event(&run, kind, json!({})).unwrap();
        refused(&mut queue, read_until, name);
    }

    // The landing began before the move was written.
    let read_until = newest(&conn);
    conn.execute(
        "UPDATE task_runs SET status='integrating' WHERE id=?1",
        [run.as_str()],
    )
    .unwrap();
    refused(&mut queue, read_until, "integrating");
    conn.execute(
        "UPDATE task_runs SET status=?1 WHERE id=?2",
        [status.as_str(), run.as_str()],
    )
    .unwrap();

    // The task's conditions changed.
    conn.execute(
        "UPDATE tasks SET acceptance=acceptance || '\nmore' WHERE id=?1",
        [a.as_i64()],
    )
    .unwrap();
    refused(&mut queue, read_until, "was edited");
    conn.execute(
        "UPDATE tasks SET acceptance='the API behaves\nthe operations are documented' WHERE id=?1",
        [a.as_i64()],
    )
    .unwrap();

    // The task was held: the move of the verdict and a new claim are
    // refused in their transactions until the hold is released.
    conn.execute(
        "INSERT INTO task_holds(task_id, reason, by_role, by_id, created_at, settled_at)
         VALUES (?1, 'the login expired', 'user', 'user', 0, 0)",
        [a.as_i64()],
    )
    .unwrap();
    refused(&mut queue, read_until, "is held (hold");
    let error = claim(&mut queue, claimed.generation).unwrap_err();
    assert!(format!("{error:#}").contains("is held"), "{error:#}");
    conn.execute(
        "UPDATE task_holds SET released_at=1 WHERE task_id=?1",
        [a.as_i64()],
    )
    .unwrap();

    let moved = queue
        .move_replan(id, claimed.generation, &diagnosed(read_until))
        .unwrap();
    assert_eq!(moved.status, ReplanStatus::Continuing);
}

/// The ask of a diagnosis, the operation claimed again for it and the
/// `replan_diagnosed` that ties them are written in one transaction, or
/// none of them when another claim came first or the task is held. A move from the diagnosis
/// is refused when a person's request joined an operation the decision
/// read as no person's, and a park without a person when the lineage's
/// parks reached the limit since the decision read them.
#[test]
fn a_diagnosis_asks_atomically_and_applies_only_under_its_request_and_park_limit() {
    use dagq::domain::task_replan::{Diagnosis, DiagnosisChoice, DiagnosisPremise};
    use dagq::domain::{AskKind, AskReason, NewAsk};
    let (_dir, db) = queue();
    let (mut queue, a, _, run) = claimed(&db);
    let detect = |queue: &mut SqliteQueue| {
        queue
            .record_replan(
                &NewReplan {
                    run: run.clone(),
                    reason: "the reviews keep failing".into(),
                    evidence: Vec::new(),
                },
                ReplanOrigin::Runtime,
                "supervisor",
                "supervisor",
            )
            .unwrap()
            .operation
    };
    let step = |to: ReplanStatus, premise: Option<DiagnosisPremise>| ReplanMove {
        to,
        reason: format!("diagnosed: {}", to.as_str()),
        snapshot_id: None,
        diagnosis: Some(Diagnosis {
            choice: Some(if to == ReplanStatus::Continuing {
                DiagnosisChoice::Continue
            } else {
                DiagnosisChoice::Replan
            }),
            decided_by: Some("runtime".into()),
            premise,
            ..Diagnosis::default()
        }),
    };
    let status = queue.show(a).unwrap().runs[0].status();
    let premise = |manual: bool, max_auto_parks: Option<usize>| DiagnosisPremise {
        run_status: status,
        read_until: None,
        head: None,
        manual: Some(manual),
        max_auto_parks,
    };
    let claim = |queue: &mut SqliteQueue, id: ReplanId, generation: i64| {
        queue
            .move_replan(
                id,
                generation,
                &ReplanMove {
                    to: ReplanStatus::Diagnosing,
                    reason: "its diagnosis starts".into(),
                    snapshot_id: None,
                    diagnosis: None,
                },
            )
            .unwrap()
            .generation
    };

    // One park of the lineage without a person, which ends withdrawn.
    let first = detect(&mut queue);
    let generation = claim(&mut queue, first.id, first.generation);
    let generation = queue
        .move_replan(
            first.id,
            generation,
            &step(ReplanStatus::PauseRequested, None),
        )
        .unwrap()
        .generation;
    let generation = park(&mut queue, first.id, generation, ReplanStatus::Blocked);
    park(&mut queue, first.id, generation, ReplanStatus::Withdrawn);

    let second = detect(&mut queue);
    assert_ne!(second.id, first.id);
    let read = claim(&mut queue, second.id, second.generation);

    // The ask and its event in one transaction, under a new claim.
    let ask = NewAsk {
        kind: AskKind::Decide,
        task_id: Some(a),
        run_id: None,
        question: "continue or replan?".into(),
        options: vec!["continue".into(), "replan".into()],
        asked_by: "replan-diagnosis".into(),
        reason_category: AskReason::Scope,
        topics: Vec::new(),
        recommendation: None,
        confidence: None,
        finding_id: None,
        request_id: None,
    };
    let asks = |queue: &SqliteQueue| queue.asks(Default::default()).unwrap().len();
    let before = asks(&queue);
    // Only the diagnosis's own decide ask about the task opens here.
    for other in [
        NewAsk {
            asked_by: "someone".into(),
            ..ask.clone()
        },
        NewAsk {
            kind: AskKind::Blocked,
            ..ask.clone()
        },
    ] {
        assert!(
            queue
                .ask_replan_diagnosis(
                    &other,
                    (second.id, read),
                    "a person is asked",
                    &run,
                    (json!({"operation_id": second.id}), None),
                )
                .is_err()
        );
    }
    assert_eq!(asks(&queue), before);
    // The ask of a job's verdict checks the job's premise in its
    // transaction: a turn requested or a receipt seen after the last event
    // the job read makes the verdict stale, and nothing is written (the
    // supervisor records it as discarded and diagnoses again).
    let newest = || -> i64 {
        Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT MAX(id) FROM run_events WHERE task_id=?1",
                [a.as_i64()],
                |r| r.get(0),
            )
            .unwrap()
    };
    for (kind, name) in [
        (EventKind::TurnRequested, "turn_requested event"),
        (EventKind::ReceiptObserved, "receipt_observed event"),
    ] {
        let read_until = newest();
        queue.record_runtime_event(&run, kind, json!({})).unwrap();
        let stale = DiagnosisPremise {
            read_until: Some(dagq::domain::EventId::new(read_until)),
            ..premise(false, None)
        };
        let error = queue
            .ask_replan_diagnosis(
                &ask,
                (second.id, read),
                "a person is asked",
                &run,
                (json!({"operation_id": second.id}), Some(&stale)),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains(name), "{error:#}");
        assert_eq!(asks(&queue), before);
        assert_eq!(queue.replan_operation(second.id).unwrap().generation, read);
    }
    // A hold open on the task refuses the ask, with a verdict's premise
    // or without one, and nothing is written until it is released.
    let holds = Connection::open(&db).unwrap();
    holds
        .execute(
            "INSERT INTO task_holds(task_id, reason, by_role, by_id, created_at, settled_at)
             VALUES (?1, 'the login expired', 'user', 'user', 0, 0)",
            [a.as_i64()],
        )
        .unwrap();
    let fresh = DiagnosisPremise {
        read_until: Some(dagq::domain::EventId::new(newest())),
        ..premise(false, None)
    };
    for premise in [Some(&fresh), None] {
        let error = queue
            .ask_replan_diagnosis(
                &ask,
                (second.id, read),
                "a person is asked",
                &run,
                (json!({"operation_id": second.id}), premise),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("is held"), "{error:#}");
        assert_eq!(asks(&queue), before);
        assert_eq!(queue.replan_operation(second.id).unwrap().generation, read);
    }
    holds
        .execute(
            "UPDATE task_holds SET released_at=1 WHERE task_id=?1",
            [a.as_i64()],
        )
        .unwrap();
    let (outcome, asked) = queue
        .ask_replan_diagnosis(
            &ask,
            (second.id, read),
            "a person is asked",
            &run,
            (json!({"operation_id": second.id}), None),
        )
        .unwrap();
    assert_eq!(asked.generation, read + 1);
    assert_eq!(asks(&queue), before + 1);
    let diagnosed: Vec<Value> = queue
        .show(a)
        .unwrap()
        .events
        .iter()
        .filter(|e| e.kind == EventKind::ReplanDiagnosed)
        .map(|e| e.payload.clone())
        .collect();
    assert_eq!(diagnosed.len(), 1);
    assert_eq!(diagnosed[0]["ask_id"], json!(outcome.ask.id));
    assert_eq!(diagnosed[0]["generation"], asked.generation);
    assert_eq!(diagnosed[0]["outcome"], "ask");
    // A claim read before it writes nothing at all.
    assert!(
        queue
            .ask_replan_diagnosis(
                &ask,
                (second.id, read),
                "a person is asked",
                &run,
                (json!({"operation_id": second.id}), None),
            )
            .is_err()
    );
    assert_eq!(asks(&queue), before + 1);
    let generation = asked.generation;

    // A person's request joins after the decision read the operation.
    let joined = ok(&db, &args(&request(&run, "split it, please")));
    assert_eq!(joined["operation"]["id"], json!(second.id));
    let error = queue
        .move_replan(
            second.id,
            generation,
            &step(ReplanStatus::Continuing, Some(premise(false, None))),
        )
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("a person's request joined"),
        "{error:#}"
    );

    // The lineage's one park reached a limit of one since it was read.
    let error = queue
        .move_replan(
            second.id,
            generation,
            &step(ReplanStatus::PauseRequested, Some(premise(true, Some(1)))),
        )
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("parks without a person are used up (1 of 1)"),
        "{error:#}"
    );
    assert_eq!(
        queue.replan_operation(second.id).unwrap().status,
        ReplanStatus::Diagnosing
    );
    let moved = queue
        .move_replan(
            second.id,
            generation,
            &step(ReplanStatus::PauseRequested, Some(premise(true, Some(2)))),
        )
        .unwrap();
    assert_eq!(moved.status, ReplanStatus::PauseRequested);
}

/// Two supervisors on their own connections that start a diagnosis job
/// at once, of one operation or of two tasks' operations, start one job
/// queue-wide: the claim, the queue-wide count of the jobs that run and
/// the `replan_diagnosis_started` that names the owner are one
/// transaction, and the other start writes nothing. A job runs until the
/// end tied to its start, whatever claims its operation meanwhile: a job
/// whose start ended in a failure, or whose owner is gone, leaves the
/// queue to the next start, and a start read before another claim is
/// refused by its generation.
#[test]
fn a_diagnosis_starts_once_queue_wide_and_is_taken_over_after_its_start() {
    use dagq::domain::replan_detect::{JobOwner, PlanningPayload};
    use dagq::domain::task_replan::ReplanOperation;
    use std::sync::{Arc, Barrier};
    // A supervisor of this pid: its registration's token, its process's
    // start.
    let sv = |pid: u32| JobOwner {
        pid,
        token: Some(format!("sv-{pid}")),
        process_start: Some("Thu Oct 29 10:00:00 2026".into()),
    };
    let (_dir, db) = queue();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let tasks: Vec<TaskId> = ["A", "B"]
        .into_iter()
        .map(|title| queue.add(new_task(title)).unwrap().id())
        .collect();
    let mut runs = Vec::new();
    for task in &tasks {
        queue.transition(*task, TaskAction::BypassReview).unwrap();
    }
    for _ in &tasks {
        let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
            panic!("claim")
        };
        runs.push(run.id().clone());
    }
    let ops: Vec<ReplanOperation> = runs
        .iter()
        .map(|run| {
            queue
                .record_replan(
                    &NewReplan {
                        run: run.clone(),
                        reason: "the reviews keep failing".into(),
                        evidence: Vec::new(),
                    },
                    ReplanOrigin::Runtime,
                    "supervisor",
                    "supervisor",
                )
                .unwrap()
                .operation
        })
        .collect();
    let started = |queue: &SqliteQueue, task: TaskId| -> Vec<Value> {
        queue
            .task_events(task)
            .unwrap()
            .iter()
            .filter(|e| e.kind == EventKind::ReplanDiagnosisStarted)
            .map(|e| e.payload.clone())
            .collect()
    };
    // Each supervisor `owner` starts `starts[owner]` at once.
    let race = |starts: [(ReplanId, i64, RunId); 2]| {
        let barrier = Arc::new(Barrier::new(2));
        let racers: Vec<_> = starts
            .into_iter()
            .zip([101_u32, 102])
            .map(|((id, generation, run), owner)| {
                let barrier = barrier.clone();
                let mut queue = SqliteQueue::open(&db).unwrap();
                std::thread::spawn(move || {
                    barrier.wait();
                    queue.start_replan_diagnosis(
                        (id, generation),
                        "its diagnosis 1 starts",
                        &run,
                        (json!({"attempt": 1}), &sv(owner), &[]),
                    )
                })
            })
            .collect();
        let _waiting = crate::common::within(crate::common::STEP_LIMIT, "the two starts to return");
        racers
            .into_iter()
            .map(|racer| racer.join().unwrap())
            .collect::<Vec<_>>()
    };

    let ended = |queue: &mut SqliteQueue, op: &ReplanOperation, run: &RunId| {
        queue
            .record_runtime_event(
                run,
                EventKind::ReplanDiagnosisFailed,
                json!({"operation_id": op.id, "started_generation": op.generation,
                       "error": "the job could not start"}),
            )
            .unwrap();
    };
    let won = |outcomes: Vec<anyhow::Result<ReplanOperation>>| {
        assert_eq!(
            outcomes.iter().filter(|o| o.is_ok()).count(),
            1,
            "{outcomes:?}"
        );
        for lost in outcomes.iter().filter_map(|o| o.as_ref().err()) {
            let lost = format!("{lost:#}");
            assert!(
                lost.contains("one runs at a time in the queue") || lost.contains("generation"),
                "{lost}"
            );
        }
        outcomes.into_iter().find_map(Result::ok).unwrap()
    };
    let owner_of = |queue: &SqliteQueue, task: TaskId| -> JobOwner {
        let records = started(queue, task);
        PlanningPayload::from_value(records.last().unwrap())
            .owner()
            .unwrap()
    };
    let owners = |queue: &SqliteQueue| -> Vec<JobOwner> {
        queue
            .running_diagnosis_owners()
            .unwrap()
            .into_iter()
            .map(|o| o.owner)
            .collect()
    };

    // Two tasks' operations raced: one job queue-wide, the other's
    // operation untouched.
    let (a, b) = (&ops[0], &ops[1]);
    let first = won(race([
        (a.id, a.generation, runs[0].clone()),
        (b.id, b.generation, runs[1].clone()),
    ]));
    assert_eq!(first.status, ReplanStatus::Diagnosing);
    let (winner, loser) = if first.id == a.id { (0, 1) } else { (1, 0) };
    assert_eq!(started(&queue, tasks[winner]).len(), 1);
    assert!(started(&queue, tasks[loser]).is_empty());
    let other = queue.replan_operation(ops[loser].id).unwrap();
    assert_eq!(other.generation, ops[loser].generation);
    assert_eq!(other.status, ReplanStatus::Candidate);
    let records = started(&queue, tasks[winner]);
    assert_eq!(records[0]["generation"], first.generation);
    // The start names its owner by its pid, token and process's start.
    let owner = owner_of(&queue, tasks[winner]);
    assert!([sv(101), sv(102)].contains(&owner), "{owner:?}");
    assert_eq!(records[0]["supervisor_token"], json!(owner.token));
    assert_eq!(
        records[0]["supervisor_process_start"],
        "Thu Oct 29 10:00:00 2026"
    );

    // Its owner gone (or a supervisor that runs no job of its own), the
    // other task's job is the queue's one.
    let next = queue
        .start_replan_diagnosis(
            (other.id, other.generation),
            "its diagnosis 1 starts",
            &runs[loser],
            (json!({"attempt": 1}), &sv(103), &[owner]),
        )
        .unwrap();
    assert_eq!(next.generation, other.generation + 1);
    assert_eq!(owner_of(&queue, tasks[loser]), sv(103));

    // A start that failed ends its job; one operation raced by two
    // supervisors then: one claim and one start under one owner.
    ended(&mut queue, &next, &runs[loser]);
    ended(&mut queue, &first, &runs[winner]);
    let again = won(race([
        (first.id, first.generation, runs[winner].clone()),
        (first.id, first.generation, runs[winner].clone()),
    ]));
    assert_eq!(again.generation, first.generation + 1);
    let records = started(&queue, tasks[winner]);
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[1]["generation"], again.generation);
    // A start read before that claim is refused by its generation.
    assert!(
        queue
            .start_replan_diagnosis(
                (first.id, first.generation),
                "its diagnosis 2 starts",
                &runs[winner],
                (json!({"attempt": 2}), &sv(104), &[sv(101), sv(102)]),
            )
            .is_err()
    );
    assert_eq!(started(&queue, tasks[winner]).len(), 2);

    // A later claim of the operation (an ask's, or a new run's that takes
    // it over) leaves its job running until that job's own end; a record
    // of another attempt, or an ask's that names no start, ends it not.
    let asked = queue
        .move_replan(
            again.id,
            again.generation,
            &ReplanMove {
                to: ReplanStatus::Diagnosing,
                reason: "a person is asked".into(),
                snapshot_id: None,
                diagnosis: None,
            },
        )
        .unwrap();
    for record in [
        json!({"operation_id": again.id, "generation": asked.generation, "outcome": "discarded"}),
        json!({"operation_id": again.id, "started_generation": first.generation,
               "outcome": "discarded"}),
    ] {
        queue
            .record_runtime_event(&runs[winner], EventKind::ReplanDiagnosed, record)
            .unwrap();
    }
    let owner = owner_of(&queue, tasks[winner]);
    assert_eq!(owners(&queue), [owner]);
    let error = queue
        .start_replan_diagnosis(
            (next.id, next.generation),
            "its diagnosis 2 starts",
            &runs[loser],
            (json!({"attempt": 2}), &sv(105), &[]),
        )
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("one runs at a time in the queue"),
        "{error:#}"
    );
    // Its own end lets the next start in.
    ended(&mut queue, &again, &runs[winner]);
    assert!(owners(&queue).is_empty());
    let after = queue
        .start_replan_diagnosis(
            (next.id, next.generation),
            "its diagnosis 2 starts",
            &runs[loser],
            (json!({"attempt": 2}), &sv(105), &[]),
        )
        .unwrap();
    assert_eq!(after.generation, next.generation + 1);

    // An older binary's records name no start's generation in their ends:
    // its job's failure or verdict ends the start of its attempt, and its
    // ask's own record without a verdict ends none.
    let legacy = |queue: &mut SqliteQueue, kind: EventKind, payload: Value| {
        queue
            .record_runtime_event(&runs[loser], kind, payload)
            .unwrap();
    };
    ended(&mut queue, &after, &runs[loser]);
    legacy(
        &mut queue,
        EventKind::ReplanDiagnosisStarted,
        json!({"operation_id": after.id, "attempt": 3, "generation": after.generation,
               "supervisor_pid": 106}),
    );
    legacy(
        &mut queue,
        EventKind::ReplanDiagnosed,
        json!({"operation_id": after.id, "attempt": 3, "window_end": 1, "outcome": "ask"}),
    );
    let legacy_owner = JobOwner {
        pid: 106,
        token: None,
        process_start: None,
    };
    assert_eq!(owners(&queue), [legacy_owner]);
    legacy(
        &mut queue,
        EventKind::ReplanDiagnosed,
        json!({"operation_id": after.id, "attempt": 3, "verdict": {"choice": "continue"},
               "duration_secs": 4, "outcome": "continue"}),
    );
    assert!(owners(&queue).is_empty());
}

/// A diagnosis job's owner is told by its registration and its process's
/// start, not its pid alone: a job whose owner is gone while another
/// process took its pid is started again by a later supervisor, so the
/// queue's one job does not hold the diagnosis of every operation; and a
/// job of a supervisor whose registration is fresh, of another pid
/// namespace with the later supervisor's pid, is not started twice.
#[test]
fn a_diagnosis_owner_is_told_by_its_registration_and_start_not_its_pid() {
    use dagq::application::ProcessControl;
    use dagq::domain::replan_detect::{self, JobOwner, OwnerSeen, PlanningPayload};
    use dagq::infrastructure::adapters::SystemProcesses;
    let (_dir, db) = queue();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let task = queue.add(new_task("A")).unwrap().id();
    queue.transition(task, TaskAction::BypassReview).unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!("claim")
    };
    let run = run.id().clone();
    let op = queue
        .record_replan(
            &NewReplan {
                run: run.clone(),
                reason: "the reviews keep failing".into(),
                evidence: Vec::new(),
            },
            ReplanOrigin::Runtime,
            "supervisor",
            "supervisor",
        )
        .unwrap()
        .operation;
    // The process that has the gone owner's pid now, started after it.
    let mut taker = crate::common::KillOnDrop::new(
        std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .unwrap(),
        "the process that took the gone owner's pid",
    );
    let gone_owner = JobOwner {
        pid: taker.child().id(),
        token: Some("gone".into()),
        process_start: Some("Thu Jan  1 00:00:00 2026".into()),
    };
    let first = queue
        .start_replan_diagnosis(
            (op.id, op.generation),
            "its diagnosis 1 starts",
            &run,
            (json!({"attempt": 1}), &gone_owner, &[]),
        )
        .unwrap();
    let started = |queue: &SqliteQueue| -> Vec<Value> {
        queue
            .task_events(task)
            .unwrap()
            .iter()
            .filter(|e| e.kind == EventKind::ReplanDiagnosisStarted)
            .map(|e| e.payload.clone())
            .collect()
    };
    let records = started(&queue);
    assert_eq!(
        PlanningPayload::from_value(&records[0]).owner(),
        Some(gone_owner.clone())
    );

    // A supervisor `me` (its token, this process's pid) splits the owners
    // of the jobs the queue runs as the runtime does.
    let me_pid = std::process::id();
    let split = |queue: &SqliteQueue, token: &str| {
        let owners = queue.running_diagnosis_owners().unwrap();
        let starts: Vec<(u32, bool, Option<String>)> = owners
            .iter()
            .map(|o| {
                let pid = o.owner.pid;
                (
                    pid,
                    SystemProcesses.alive(pid),
                    SystemProcesses.start_identity(pid),
                )
            })
            .collect();
        replan_detect::split_owners(&owners, |owner| {
            let (_, alive, start) = starts.iter().find(|(pid, ..)| *pid == owner.pid).unwrap();
            OwnerSeen {
                me: (token, me_pid),
                pid_alive: *alive,
                pid_start: start.as_deref(),
            }
        })
    };
    let later = JobOwner {
        pid: me_pid,
        token: Some("later".into()),
        process_start: SystemProcesses.start_identity(me_pid),
    };
    // Its pid alive in a process of another start: the owner is gone. Not
    // told so, the queue refuses a second job; told so, the later
    // supervisor takes the operation over as its next attempt.
    let (live, gone) = split(&queue, "later");
    assert!(live.is_empty(), "{live:?}");
    assert_eq!(gone, std::slice::from_ref(&gone_owner));
    let refused = queue
        .start_replan_diagnosis(
            (first.id, first.generation),
            "its diagnosis 2 starts",
            &run,
            (json!({"attempt": 2}), &later, &[]),
        )
        .unwrap_err();
    assert!(
        format!("{refused:#}").contains("one runs at a time in the queue"),
        "{refused:#}"
    );
    let second = queue
        .start_replan_diagnosis(
            (first.id, first.generation),
            "its diagnosis 2 starts",
            &run,
            (json!({"attempt": 2}), &later, &gone),
        )
        .unwrap();
    assert_eq!(second.generation, first.generation + 1);
    assert_eq!(started(&queue).len(), 2);

    // `later` registered with a fresh heartbeat: a supervisor of another
    // pid namespace with the same pid sees its job running and starts none.
    Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO supervisors(token, pid, parallel, heartbeat_at)
             VALUES ('later', ?1, 1, unixepoch())",
            [me_pid],
        )
        .unwrap();
    let (live, gone) = split(&queue, "twin");
    assert_eq!(
        (live, gone.clone()),
        (vec![later.clone()], vec![gone_owner.clone()])
    );
    let twin = JobOwner {
        token: Some("twin".into()),
        ..later.clone()
    };
    assert!(
        queue
            .start_replan_diagnosis(
                (second.id, second.generation),
                "its diagnosis 3 starts",
                &run,
                (json!({"attempt": 3}), &twin, &gone),
            )
            .is_err()
    );
    // `later` itself after an exec (its own token) runs none: it starts
    // the next attempt.
    let (live, gone) = split(&queue, "later");
    assert_eq!(
        (live, gone.clone()),
        (vec![], vec![gone_owner, later.clone()])
    );
    let third = queue
        .start_replan_diagnosis(
            (second.id, second.generation),
            "its diagnosis 3 starts",
            &run,
            (json!({"attempt": 3}), &later, &gone),
        )
        .unwrap();
    assert_eq!(third.generation, second.generation + 1);
    drop(taker);
}

/// Acceptance (one settlement of one verdict): the failure of a verdict's
/// apply is recorded with a claim of its operation under the job's
/// generation, the guard of every move and ask of the verdict. Of a
/// supervisor that applies a verdict a hold kept and another that records
/// its failure, the one the queue takes first wins and the other is
/// refused and writes nothing: the failure first (a move, or the ask a
/// refused park makes under the job's generation), the apply first, and
/// both at once. A failure that lost is no failed job. A hold that came
/// before the failure refuses it too, and nothing is written (the
/// supervisor keeps the verdict for the release).
#[test]
fn the_apply_and_the_failure_of_one_verdict_are_not_both_settled() {
    use dagq::application::supervisor_service::rejected;
    use dagq::domain::replan_detect::{JobOwner, failed_jobs};
    use dagq::domain::task_replan::ReplanOperation;
    use dagq::domain::{AskKind, AskReason, NewAsk};
    use std::sync::{Arc, Barrier};
    let (_dir, db) = queue();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let task = queue.add(new_task("A")).unwrap().id();
    queue.transition(task, TaskAction::BypassReview).unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!("claim")
    };
    let run = run.id().clone();
    let op = queue
        .record_replan(
            &NewReplan {
                run: run.clone(),
                reason: "the reviews keep failing".into(),
                evidence: Vec::new(),
            },
            ReplanOrigin::Runtime,
            "supervisor",
            "supervisor",
        )
        .unwrap()
        .operation;
    let owner = JobOwner {
        pid: 101,
        token: Some("sv-101".into()),
        process_start: Some("Thu Oct 29 10:00:00 2026".into()),
    };
    let start = |queue: &mut SqliteQueue, read: &ReplanOperation, attempt: u64| {
        queue
            .start_replan_diagnosis(
                (read.id, read.generation),
                "its diagnosis starts",
                &run,
                (
                    json!({"attempt": attempt}),
                    &owner,
                    std::slice::from_ref(&owner),
                ),
            )
            .unwrap()
    };
    let failure = |generation: i64| {
        json!({"operation_id": op.id, "started_generation": generation,
               "verdict": {"choice": "ask"}, "error": "the ask could not be written"})
    };
    let claim = |to: ReplanStatus| ReplanMove {
        to,
        reason: "the verdict is applied".into(),
        snapshot_id: None,
        diagnosis: None,
    };
    let events = |queue: &SqliteQueue| queue.task_events(task).unwrap();
    let failed = |queue: &SqliteQueue| failed_jobs(&events(queue), op.id);

    // The failure first: the apply under the job's generation is refused.
    let first = start(&mut queue, &op, 1);
    let after = queue
        .fail_replan_diagnosis(
            (op.id, first.generation),
            "the apply failed",
            &run,
            failure(first.generation),
        )
        .unwrap();
    assert_eq!(after.generation, first.generation + 1);
    let error = queue
        .move_replan(op.id, first.generation, &claim(ReplanStatus::Continuing))
        .unwrap_err();
    assert!(rejected(&error), "{error:#}");
    // The ask a park refused at the lineage's limit makes claims the
    // operation under the job's generation too, and is refused as well.
    let asks = |queue: &SqliteQueue| queue.asks(Default::default()).unwrap().len();
    let before = asks(&queue);
    let ask = NewAsk {
        kind: AskKind::Decide,
        task_id: Some(task),
        run_id: None,
        question: "continue or replan?".into(),
        options: vec!["continue".into(), "replan".into()],
        asked_by: "replan-diagnosis".into(),
        reason_category: AskReason::RecoveryFailed,
        topics: Vec::new(),
        recommendation: Some("replan".into()),
        confidence: None,
        finding_id: None,
        request_id: None,
    };
    let error = queue
        .ask_replan_diagnosis(
            &ask,
            (op.id, first.generation),
            "a person is asked",
            &run,
            (
                json!({"operation_id": op.id, "verdict": {"choice": "replan"}}),
                None,
            ),
        )
        .unwrap_err();
    assert!(rejected(&error), "{error:#}");
    assert_eq!(asks(&queue), before);
    assert_eq!(failed(&queue), 1);

    // Both at once: one wins.
    let second = start(&mut queue, &after, 2);
    let barrier = Arc::new(Barrier::new(2));
    let racers: Vec<_> = [true, false]
        .into_iter()
        .map(|fails| {
            let barrier = barrier.clone();
            let mut queue = SqliteQueue::open(&db).unwrap();
            let (run, payload) = (run.clone(), failure(second.generation));
            std::thread::spawn(move || {
                barrier.wait();
                if fails {
                    queue.fail_replan_diagnosis(
                        (op.id, second.generation),
                        "the apply failed",
                        &run,
                        payload,
                    )
                } else {
                    queue.move_replan(op.id, second.generation, &claim(ReplanStatus::Diagnosing))
                }
            })
        })
        .collect();
    let outcomes: Vec<anyhow::Result<ReplanOperation>> = {
        let _waiting =
            crate::common::within(crate::common::STEP_LIMIT, "the apply and the failure");
        racers
            .into_iter()
            .map(|racer| racer.join().unwrap())
            .collect()
    };
    assert_eq!(
        outcomes.iter().filter(|o| o.is_ok()).count(),
        1,
        "{outcomes:?}"
    );
    let lost = outcomes.iter().find_map(|o| o.as_ref().err()).unwrap();
    assert!(rejected(lost), "{lost:#}");
    let failure_won = outcomes[0].is_ok();
    assert_eq!(failed(&queue), if failure_won { 2 } else { 1 });
    let now = queue.replan_operation(op.id).unwrap();
    assert_eq!(now.generation, second.generation + 1);

    // A hold before the failure refuses it and writes nothing, the
    // operation left under the job's generation for the release.
    let third = start(&mut queue, &now, 3);
    let holds = Connection::open(&db).unwrap();
    holds
        .execute(
            "INSERT INTO task_holds(task_id, reason, by_role, by_id, created_at, settled_at)
             VALUES (?1, 'the login expired', 'user', 'user', 0, 0)",
            [task.as_i64()],
        )
        .unwrap();
    let written = events(&queue).len();
    let error = queue
        .fail_replan_diagnosis(
            (op.id, third.generation),
            "the apply failed",
            &run,
            failure(third.generation),
        )
        .unwrap_err();
    assert!(rejected(&error), "{error:#}");
    assert!(format!("{error:#}").contains("is held"), "{error:#}");
    assert_eq!(events(&queue).len(), written);
    assert_eq!(
        queue.replan_operation(op.id).unwrap().generation,
        third.generation
    );
    holds
        .execute(
            "UPDATE task_holds SET released_at=1 WHERE task_id=?1",
            [task.as_i64()],
        )
        .unwrap();

    // The apply first: the failure under the job's generation is refused
    // and writes nothing.
    let written = events(&queue).len();
    queue
        .move_replan(op.id, third.generation, &claim(ReplanStatus::Continuing))
        .unwrap();
    let written = written + 1;
    let error = queue
        .fail_replan_diagnosis(
            (op.id, third.generation),
            "the apply failed",
            &run,
            failure(third.generation),
        )
        .unwrap_err();
    assert!(rejected(&error), "{error:#}");
    assert_eq!(events(&queue).len(), written);
    assert_eq!(failed(&queue), if failure_won { 2 } else { 1 });
    assert_eq!(
        queue.replan_operation(op.id).unwrap().status,
        ReplanStatus::Continuing
    );
}
