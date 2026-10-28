//! Worker screen/send/close-workspaces commands are refused. A planner has
//! no screen to read and takes no keys or answers either (ADR-t1433-2):
//! `planner screen` says so and where its turns are, and `planner send` is
//! refused; who may run them stays the actors' authorization.

use crate::common;

use common::cli::*;

use dagq::{
    application::TaskStore,
    domain::{
        ClaimOutcome, LeaseToken, PlannerOrigin, RunPlan, TaskAction,
        provider_switch::WorkerRoute,
        worker::{Worker, WorkerMode},
    },
    infrastructure::sqlite::SqliteQueue,
};
use serde_json::Value;
use std::path::Path;

/// A headless run of a new ready task claimed and planned, its session's
/// workspace `workspace`, or with `recorded_interactive` a run stored as
/// interactive before task 1437 (a claim no longer makes one); returns its
/// run id.
fn run_in(db: &Path, title: &str, recorded_interactive: bool, workspace: &str) -> String {
    let mut queue = SqliteQueue::open(db).unwrap();
    let mut task = common::queue::new_task(title);
    task.worker_mode = Some(WorkerMode::Headless);
    let id = queue.add(task).unwrap().id();
    queue.transition(id, TaskAction::BypassReview).unwrap();
    let token = LeaseToken::new(format!("lease-{workspace}"));
    let worker = Worker::CLAUDE_HEADLESS;
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor_in_order(
            &common::queue::base(),
            &token,
            &[id],
            None,
            &Default::default(),
            &WorkerRoute::direct(&[worker]),
        )
        .unwrap()
    else {
        panic!("no claim")
    };
    // An already stored historical run; a new claim always runs headless.
    if recorded_interactive {
        rusqlite::Connection::open(db)
            .unwrap()
            .execute(
                "UPDATE task_runs SET worker_mode='interactive' WHERE id=?1",
                [run.id()],
            )
            .unwrap();
    }
    let dir = format!("/runs/{}", run.id());
    queue
        .plan_run(
            run.id(),
            &token,
            &RunPlan {
                repo_path: "/repo".into(),
                run_dir: dir.clone(),
                branch: format!("dagq/{}", run.id()),
                worktree_path: format!("{dir}/worktree"),
                receipt_path: format!("{dir}/receipt.json"),
                log_path: format!("{dir}/log"),
            },
        )
        .unwrap();
    queue
        .workspace_created(run.id(), &token, workspace)
        .unwrap();
    run.id().to_string()
}

fn events(db: &Path, kind: &str) -> Vec<Value> {
    ok(
        db,
        &["events", "--after", "0", "--all", "--full", "--kind", kind],
    )["events"]
        .as_array()
        .unwrap()
        .clone()
}

fn failed_with(env: &[(&str, &str)], db: &Path, args: &[&str]) -> Value {
    let output = invoke_with(env, db, args);
    assert!(!output.status.success(), "{env:?} {args:?} succeeded");
    serde_json::from_slice(&output.stderr).unwrap()
}

/// An answered `worker_question` on `run`; returns the ask's id.
fn answered_ask(db: &Path, run: &str, answer: &str) -> String {
    let opened = ok(
        db,
        &[
            "ask",
            "--run",
            run,
            "--kind",
            "worker_question",
            "--because",
            "scope",
            "--topic",
            "task_overlap",
            "--question",
            "Which side?",
        ],
    );
    let id = opened["id"].to_string();
    ok(db, &["answer", &id, "--text", answer]);
    id
}

/// `run screen` is refused for every run, with the reason and the turns'
/// log CLI that replaced it (ADR-t1433-3 decision 4), by run or task id,
/// for a run recorded as interactive too; `run send` refuses typing; and
/// `run close-workspaces` is refused with its reason (decision 3). None of
/// them reads or types anything.
#[test]
fn runs_refuse_their_screen_and_typing() {
    let (_dir, db) = queue();
    // Include the old stored mode: historical interactive records no
    // longer grant access to a terminal, either by run or task id.
    let interactive = run_in(&db, "historical", true, "WS-I");
    let headless = run_in(&db, "headless", false, "WS-H");
    for (run, task) in [(&interactive, "1"), (&headless, "2")] {
        let ask = answered_ask(&db, run, "continue");
        for target in [run.as_str(), task] {
            let refused = failed_with(
                &[("DAGQ_ROLE", "inbox")],
                &db,
                &["run", "screen", target, "--lines", "1000"],
            );
            let error = refused["error"].as_str().unwrap();
            assert!(
                error.contains(dagq::application::screen::RUN_SCREEN_REFUSED)
                    && error.contains(&format!("`dagq run log {run}`"))
                    && error.contains(&format!("/runs/{run}/turns")),
                "{refused}"
            );
            for input in [
                vec!["--key", "enter"],
                vec!["--key", "exit"],
                vec!["--answer", &ask],
            ] {
                let mut argv = vec!["run", "send", target];
                argv.extend(input);
                let failed = failed_with(&[], &db, &argv);
                let error = failed["error"].as_str().unwrap();
                assert!(
                    error.contains("no interactive input")
                        && error.contains("`answer`")
                        && error.contains("next turn"),
                    "{failed}"
                );
            }
        }
    }
    for args in [
        vec!["run", "close-workspaces", "--apply"],
        vec!["run", "close-workspaces", headless.as_str()],
        vec!["run", "close-workspaces", "--task", "2"],
    ] {
        let refused = failed_with(&[], &db, &args);
        assert_eq!(
            refused["error"],
            dagq::application::screen::CLOSE_WORKSPACES_REFUSED,
            "{args:?}: {refused}"
        );
    }
    assert!(events(&db, "screen_read").is_empty());
    assert!(events(&db, "screen_input_sent").is_empty());
    assert!(
        events(&db, "ask_delivered").is_empty(),
        "refused sends leave answers for the supervisor"
    );
}

/// A planner in a workspace (a person's, opened before `dagq plan` was
/// abolished) has its screen read no more and takes no keys or answers
/// (ADR-t1433-2): `planner screen` says so, with where a planner's turns
/// are, and `planner send` is refused with what to do instead; neither
/// reads, types or records anything. An unknown planner is the queue's
/// error.
#[test]
fn a_planner_in_a_workspace_has_no_screen_read_and_takes_no_keys_or_answers() {
    let (_dir, db) = queue();
    let queue = SqliteQueue::open(&db).unwrap();
    let planner = queue.open_planner(PlannerOrigin::Person, None).unwrap();
    queue.planner_workspace_created(planner.id, "PW-1").unwrap();
    drop(queue);
    let run = run_in(&db, "asked", false, "WS-1");
    let id = planner.id.to_string();

    for args in [
        vec!["planner", "screen", &id],
        vec!["planner", "screen", &id, "--lines", "1000"],
    ] {
        let read = ok_as("inbox", &db, &args);
        assert_eq!(read["planner_id"], planner.id.as_i64());
        assert_eq!(read["screen"], Value::Null, "{read}");
        assert_eq!(read["route"], "interactive");
        assert!(
            read["reason"]
                .as_str()
                .unwrap()
                .contains("is not read any more"),
            "{read}"
        );
        assert!(
            read["turns"]
                .as_str()
                .unwrap()
                .ends_with(&format!("planners/{id}/turns")),
            "{read}"
        );
    }
    let ask = answered_ask(&db, &run, "yes");
    for args in [
        vec!["planner", "send", &id, "--key", "escape"],
        vec!["planner", "send", &id, "--key", "exit"],
        vec!["planner", "send", &id, "--answer", &ask],
    ] {
        let failed = failed_with(&[], &db, &args);
        let error = failed["error"].as_str().unwrap();
        assert!(error.contains("takes no keys or answers"), "{failed}");
        assert!(error.contains("`answer`"), "{failed}");
    }
    assert!(
        failed_with(&[], &db, &["planner", "screen", "99"])["error"]
            .as_str()
            .is_some()
    );
    assert!(events(&db, "screen_read").is_empty());
    assert!(events(&db, "screen_input_sent").is_empty());
    assert!(events(&db, "ask_delivered").is_empty());
}

#[test]
fn a_planner_a_worker_and_the_jobs_are_refused_and_recorded() {
    let (_dir, db) = queue();
    let run = run_in(&db, "guarded", false, "WS-1");
    // A planner of the runtime's, with its token.
    let planner = [("DAGQ_ROLE", "planner")];
    let worker = [
        ("DAGQ_ROLE", "worker"),
        ("DAGQ_ACTOR_ID", "worker:x"),
        ("DAGQ_RUN_ID", run.as_str()),
        ("DAGQ_TASK_ID", "1"),
    ];
    let actors: Vec<Vec<(&str, &str)>> = vec![
        planner.to_vec(),
        worker.to_vec(),
        vec![("DAGQ_ROLE", "recovery-job")],
        vec![("DAGQ_ROLE", "observer")],
    ];
    let mut refused = 0;
    for env in &actors {
        for (args, capability) in [
            (vec!["run", "screen", run.as_str()], "screen.read"),
            (
                vec!["run", "send", run.as_str(), "--key", "enter"],
                "screen.send",
            ),
            // Not even a planner its own session.
            (vec!["planner", "screen", "1"], "screen.read"),
            (
                vec!["planner", "send", "1", "--key", "enter"],
                "screen.send",
            ),
        ] {
            let argv = args.clone();
            let error = failed_with(env, &db, &argv);
            assert_eq!(
                error["denied"]["capability"], capability,
                "{env:?} {argv:?}"
            );
            assert_eq!(error["denied"]["reason"], "not granted");
            refused += 1;
        }
    }
    // The planner's refusals reach the service with its token, which
    // records them as the planner's; the others' are sent to it with no
    // token, and recorded as unauthenticated.
    let denied = events(&db, "authorization_denied");
    assert_eq!(denied.len(), 4);
    assert!(
        denied
            .iter()
            .all(|event| event["actor"]["role"] == "planner")
    );
    assert_eq!(denied[0]["payload"]["capability"], "screen.read");
    let unauthenticated = events(&db, "queue_service_unauthenticated");
    assert_eq!(unauthenticated.len(), refused - 4);
    assert!(
        unauthenticated
            .iter()
            .all(|event| event["payload"]["use_case"] == "denial_record")
    );
    assert!(events(&db, "screen_read").is_empty());
}
