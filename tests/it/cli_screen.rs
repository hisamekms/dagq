//! Worker screen/send/close-workspaces commands are refused without
//! touching cmux. Planner screen/send commands keep their screen, key,
//! answer delivery and actor authorization behavior.

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
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Claude Code at rest: the input box empty, nothing at work.
const READY: &str = "\
⏺ Done.

──────────────────────────────────────────────────────────────────────
❯
──────────────────────────────────────────────────────────────────────
  ? for shortcuts
";

/// A stub `cmux` beside `db`: `read-screen` prints the `screen` file, and
/// every call is appended to `calls`. Returns its path.
fn stub_cmux(db: &Path, screen: &str) -> PathBuf {
    let dir = db.parent().unwrap().join("screen-cmux");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("screen"), screen).unwrap();
    let stub = dir.join("cmux");
    common::template::script(
        &stub,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${0%/*}/calls\"\n\
         if [ \"$1\" = read-screen ]; then cat \"${0%/*}/screen\"; fi\n",
    );
    stub
}

fn calls(cmux: &Path) -> String {
    fs::read_to_string(cmux.parent().unwrap().join("calls")).unwrap_or_default()
}

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
/// them reads, types or needs cmux.
#[test]
fn runs_refuse_their_screen_and_typing_without_cmux() {
    let (_dir, db) = queue();
    // Include the old stored mode: historical interactive records no
    // longer grant access to a terminal, either by run or task id.
    let interactive = run_in(&db, "historical", true, "WS-I");
    let headless = run_in(&db, "headless", false, "WS-H");
    let cmux = stub_cmux(&db, READY);
    let cmux = cmux.to_str().unwrap();
    for (run, task) in [(&interactive, "1"), (&headless, "2")] {
        let ask = answered_ask(&db, run, "continue");
        // Asking can notify the inbox; compare only screen/send calls.
        let before = calls(Path::new(cmux));
        for target in [run.as_str(), task] {
            let refused = failed_with(
                &[("DAGQ_ROLE", "inbox")],
                &db,
                &["run", "screen", target, "--lines", "1000", "--cmux", cmux],
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
                argv.extend(["--cmux", cmux]);
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
        assert_eq!(
            calls(Path::new(cmux)),
            before,
            "worker commands must not read or type"
        );
    }
    // None needs cmux: a host without it gets the same replies.
    let missing = "/nonexistent/cmux";
    let refused = failed_with(
        &[("DAGQ_ROLE", "inbox")],
        &db,
        &["run", "screen", &headless, "--cmux", missing],
    );
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains(dagq::application::screen::RUN_SCREEN_REFUSED),
        "{refused}"
    );
    for args in [
        vec!["run", "close-workspaces", "--apply", "--cmux", missing],
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
    let failed = failed_with(
        &[],
        &db,
        &[
            "run", "send", &headless, "--key", "enter", "--cmux", missing,
        ],
    );
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .contains("no interactive input"),
        "{failed}"
    );
    assert!(events(&db, "screen_read").is_empty());
    assert!(events(&db, "screen_input_sent").is_empty());
    assert!(
        events(&db, "ask_delivered").is_empty(),
        "refused sends leave answers for the supervisor"
    );
}

#[test]
fn a_planners_screen_is_read_and_sent_keys_by_its_id() {
    let (_dir, db) = queue();
    let queue = SqliteQueue::open(&db).unwrap();
    let planner = queue.open_planner(PlannerOrigin::Person, None).unwrap();
    queue.planner_workspace_created(planner.id, "PW-1").unwrap();
    drop(queue);
    let run = run_in(&db, "asked", false, "WS-1");
    let long: String = (1..=300).map(|n| format!("line {n}\n")).collect();
    let cmux = stub_cmux(&db, &format!("{long}\n\n"));
    let cmux = cmux.to_str().unwrap();
    let id = planner.id.to_string();

    let read = ok_as("inbox", &db, &["planner", "screen", &id, "--cmux", cmux]);
    assert_eq!(
        read["planner_id"],
        planner.id.to_string().parse::<i64>().unwrap()
    );
    assert_eq!(read["lines"], 40);
    assert_eq!(read["truncated"], true);
    let screen = read["screen"].as_str().unwrap();
    assert!(screen.starts_with("line 261\n") && screen.ends_with("line 300"));
    let read = ok(
        &db,
        &["planner", "screen", &id, "--lines", "1000", "--cmux", cmux],
    );
    assert_eq!(read["lines_requested"], 1000);
    assert_eq!(read["lines_limit"], 200);
    assert_eq!(read["lines"], 200);
    assert_eq!(read["screen"].as_str().unwrap().lines().count(), 200);
    ok(
        &db,
        &["planner", "send", &id, "--key", "escape", "--cmux", cmux],
    );
    assert!(calls(Path::new(cmux)).contains("send-key --workspace PW-1 -- escape\n"));
    // A worker's answer is no answer for a planner.
    let ask = answered_ask(&db, &run, "yes");
    let failed = failed_with(
        &[],
        &db,
        &["planner", "send", &id, "--answer", &ask, "--cmux", cmux],
    );
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .contains("is not a question"),
        "{failed}"
    );
    assert!(
        failed_with(&[], &db, &["planner", "screen", "99", "--cmux", cmux])["error"]
            .as_str()
            .is_some()
    );
    let reads = events(&db, "screen_read");
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[0]["actor"]["role"], "inbox");
    assert_eq!(reads[0]["payload"]["target"], "planner");
    assert_eq!(reads[0]["payload"]["workspace_id"], "PW-1");
    assert_eq!(events(&db, "screen_input_sent").len(), 1);
}

#[test]
fn a_planner_a_worker_and_the_jobs_are_refused_and_recorded() {
    let (_dir, db) = queue();
    let run = run_in(&db, "guarded", false, "WS-1");
    let cmux = stub_cmux(&db, READY);
    let cmux = cmux.to_str().unwrap();
    let planner = [
        ("DAGQ_ROLE", "planner"),
        ("DAGQ_ACTOR_ID", "planner:1"),
        ("DAGQ_PLANNER_ID", "1"),
    ];
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
            let mut argv = args.clone();
            argv.extend(["--cmux", cmux]);
            let error = failed_with(env, &db, &argv);
            assert_eq!(
                error["denied"]["capability"], capability,
                "{env:?} {argv:?}"
            );
            assert_eq!(error["denied"]["reason"], "not granted");
            refused += 1;
        }
    }
    assert_eq!(calls(Path::new(cmux)), "", "nothing was read or typed");
    let denied = events(&db, "authorization_denied");
    assert_eq!(denied.len(), refused);
    assert_eq!(denied[0]["actor"]["role"], "planner");
    assert_eq!(denied[0]["payload"]["capability"], "screen.read");
    assert!(events(&db, "screen_read").is_empty());
}

/// The answer of a `planner_question` about a task of the proposal a
/// planner of the runtime's works on is typed into that planner, once: the
/// ask is claimed and closed, so the supervisor does not type it again.
#[test]
fn a_planner_question_is_answered_into_its_planner_once() {
    let (_dir, db) = queue();
    ok(&db, &["add", "drafted"]);
    let proposal = ok(&db, &["submit", "1"])["id"].as_i64().unwrap();
    let queue = SqliteQueue::open(&db).unwrap();
    let planner = queue
        .open_planner(
            PlannerOrigin::Runtime,
            Some(dagq::domain::ProposalId::new(proposal)),
        )
        .unwrap();
    queue.planner_workspace_created(planner.id, "PW-R").unwrap();
    drop(queue);
    let cmux = stub_cmux(&db, READY);
    let cmux = cmux.to_str().unwrap();
    let ask = ok(
        &db,
        &[
            "ask",
            "--task",
            "1",
            "--kind",
            "planner_question",
            "--because",
            "scope",
            "--question",
            "Which goal?",
        ],
    )["id"]
        .to_string();
    ok(&db, &["answer", &ask, "--text", "goal 3"]);
    let id = planner.id.to_string();
    let sent = ok_as(
        "inbox",
        &db,
        &["planner", "send", &id, "--answer", &ask, "--cmux", cmux],
    );
    assert_eq!(sent["outcome"], "submitted");
    assert!(
        calls(Path::new(cmux)).contains(&format!(
            "send --workspace PW-R -- answer to ask {ask}: goal 3\n"
        )),
        "{}",
        calls(Path::new(cmux))
    );
    assert_eq!(events(&db, "ask_delivered").len(), 1);
    // Delivered and closed, it is typed no more.
    let failed = failed_with(
        &[],
        &db,
        &["planner", "send", &id, "--answer", &ask, "--cmux", cmux],
    );
    assert!(failed["error"].as_str().is_some(), "{failed}");
}
