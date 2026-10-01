//! `run screen` / `run send` and `planner screen` / `planner send`
//! (ADR-t1228-1 decisions 4, 5 and 7): a person or the inbox reads a
//! session's screen, at most 200 lines, and types into it only a key of the
//! set or the answer of an answered ask about it, naming the run (or task)
//! or planner, never the workspace. Other roles are refused and recorded,
//! a headless run has no screen, and each read and send is recorded with
//! its actor. A stub `cmux` shows the screen and keeps the calls.

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
    use std::os::unix::fs::PermissionsExt;
    let dir = db.parent().unwrap().join("screen-cmux");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("screen"), screen).unwrap();
    let stub = dir.join("cmux");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{dir}/calls'\n\
             if [ \"$1\" = read-screen ]; then cat '{dir}/screen'; fi\n",
            dir = dir.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    stub
}

fn calls(cmux: &Path) -> String {
    fs::read_to_string(cmux.parent().unwrap().join("calls")).unwrap_or_default()
}

/// A run of a new ready task claimed and planned, its session's workspace
/// `workspace`; returns its run id.
fn run_in(db: &Path, title: &str, mode: Option<WorkerMode>, workspace: &str) -> String {
    let mut queue = SqliteQueue::open(db).unwrap();
    let mut task = common::queue::new_task(title);
    task.worker_mode = mode;
    let id = queue.add(task).unwrap().id();
    queue.transition(id, TaskAction::BypassReview).unwrap();
    let token = LeaseToken::new(format!("lease-{workspace}"));
    let worker = Worker {
        mode: mode.unwrap_or(WorkerMode::Interactive),
        ..Worker::DEFAULT
    };
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

#[test]
fn a_runs_screen_is_read_by_run_or_task_id_up_to_the_limit_and_recorded() {
    let (_dir, db) = queue();
    let run = run_in(&db, "screen", None, "WS-1");
    let long: String = (1..=300).map(|n| format!("line {n}\n")).collect();
    let cmux = stub_cmux(&db, &format!("{long}\n\n"));
    let cmux = cmux.to_str().unwrap();

    let read = ok(&db, &["run", "screen", &run, "--cmux", cmux]);
    assert_eq!(read["run_id"], run.as_str());
    assert_eq!(read["task_id"], 1);
    assert_eq!(read["lines"], 40);
    assert_eq!(read["truncated"], true);
    let screen = read["screen"].as_str().unwrap();
    assert!(screen.starts_with("line 261\n") && screen.ends_with("line 300"));

    // By the task's id, more lines than the limit are cut to it.
    let read = ok(
        &db,
        &["run", "screen", "1", "--lines", "1000", "--cmux", cmux],
    );
    assert_eq!(read["run_id"], run.as_str());
    assert_eq!(
        (read["lines_requested"].clone(), read["lines_limit"].clone()),
        (1000.into(), 200.into())
    );
    assert_eq!(read["lines"], 200);
    assert!(calls(Path::new(cmux)).contains("read-screen --workspace WS-1"));

    // The inbox reads too; each read is recorded with its actor and
    // without the screen.
    ok_as("inbox", &db, &["run", "screen", &run, "--cmux", cmux]);
    let reads = events(&db, "screen_read");
    assert_eq!(reads.len(), 3);
    assert_eq!(reads[0]["actor"]["role"], "user");
    assert_eq!(reads[2]["actor"]["role"], "inbox");
    assert_eq!(reads[0]["run_id"], run.as_str());
    assert_eq!(reads[0]["payload"]["workspace_id"], "WS-1");
    assert!(!reads[0]["payload"].to_string().contains("line 300"));
}

#[test]
fn a_send_types_only_keys_of_the_set_or_an_answer_of_the_run() {
    let (_dir, db) = queue();
    let run = run_in(&db, "send", None, "WS-1");
    let other = run_in(&db, "other", None, "WS-2");
    let cmux = stub_cmux(&db, READY);
    let cmux = cmux.to_str().unwrap();

    let sent = ok(
        &db,
        &[
            "run", "send", &run, "--key", "down", "--key", "2", "--key", "enter", "--cmux", cmux,
        ],
    );
    assert_eq!(sent["keys"], serde_json::json!(["down", "2", "enter"]));
    let typed = calls(Path::new(cmux));
    assert!(
        typed.contains("send-key --workspace WS-1 -- down\n"),
        "{typed}"
    );
    assert!(
        typed.contains("send-key --workspace WS-1 -- 2\n"),
        "{typed}"
    );
    assert!(
        typed.contains("send-key --workspace WS-1 -- enter\n"),
        "{typed}"
    );

    // The answer of an answered ask on the run, as the supervisor types it.
    let ask = answered_ask(&db, &run, "the left one");
    let sent = ok_as(
        "inbox",
        &db,
        &["run", "send", "1", "--answer", &ask, "--cmux", cmux],
    );
    assert_eq!(sent["input"], "answer");
    assert_eq!(sent["outcome"], "submitted");
    let typed = calls(Path::new(cmux));
    assert!(
        typed.contains(&format!(
            "send --workspace WS-1 -- answer to ask {ask}: the left one\n"
        )),
        "{typed}"
    );

    // Refused: free text, a key outside the set, an ask still open, an ask
    // of another run, both at once, or nothing.
    let open = ok(
        &db,
        &[
            "ask",
            "--run",
            &run,
            "--kind",
            "worker_question",
            "--because",
            "scope",
            "--topic",
            "task_overlap",
            "--question",
            "Again?",
        ],
    )["id"]
        .to_string();
    let elsewhere = answered_ask(&db, &other, "no");
    let before = calls(Path::new(cmux));
    for (args, error) in [
        (vec!["--key", "rm -rf /"], "is not a key"),
        (vec!["--key", "ctrl-c"], "is not a key"),
        (vec!["--key", "0"], "is not a key"),
        (
            vec!["--key", "exit", "--key", "enter"],
            "exit is sent alone",
        ),
        (vec!["--answer", &open], "has no answer yet"),
        (vec!["--answer", &elsewhere], "is not about run"),
        (vec![], "send either --key"),
    ] {
        let mut argv = vec!["run", "send", &run];
        argv.extend(args.iter().copied());
        argv.extend(["--cmux", cmux]);
        let failed = failed_with(&[], &db, &argv);
        assert!(
            failed["error"].as_str().unwrap().contains(error),
            "{argv:?}: {failed}"
        );
    }
    // No free text through any flag.
    assert!(
        !invoke(&db, &["run", "send", &run, "hello", "--cmux", cmux])
            .status
            .success()
    );
    assert!(
        !invoke(&db, &["run", "send", &run, "--text", "hi", "--cmux", cmux])
            .status
            .success()
    );
    assert_eq!(
        calls(Path::new(cmux)),
        before,
        "a refused send typed nothing"
    );

    // Each send is recorded with its actor.
    let sends = events(&db, "screen_input_sent");
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0]["actor"]["role"], "user");
    assert_eq!(sends[0]["payload"]["input"], "keys");
    assert_eq!(sends[1]["actor"]["role"], "inbox");
    assert_eq!(sends[1]["payload"]["ask_id"].to_string(), ask);
}

#[test]
fn a_headless_run_has_no_screen_and_takes_nothing() {
    let (_dir, db) = queue();
    let run = run_in(&db, "headless", Some(WorkerMode::Headless), "WS-H");
    let cmux = stub_cmux(&db, READY);
    let cmux = cmux.to_str().unwrap();
    let read = ok(&db, &["run", "screen", &run, "--cmux", cmux]);
    assert_eq!(read["screen"], Value::Null);
    assert!(
        read["turns"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/runs/{run}/turns")),
        "{read}"
    );
    let failed = failed_with(
        &[],
        &db,
        &["run", "send", &run, "--key", "enter", "--cmux", cmux],
    );
    assert!(
        failed["error"].as_str().unwrap().contains("is headless"),
        "{failed}"
    );
    assert_eq!(calls(Path::new(cmux)), "");
}

#[test]
fn a_planners_screen_is_read_and_sent_keys_by_its_id() {
    let (_dir, db) = queue();
    let queue = SqliteQueue::open(&db).unwrap();
    let planner = queue.open_planner(PlannerOrigin::Person, None).unwrap();
    queue.planner_workspace_created(planner.id, "PW-1").unwrap();
    drop(queue);
    let run = run_in(&db, "asked", None, "WS-1");
    let cmux = stub_cmux(&db, READY);
    let cmux = cmux.to_str().unwrap();
    let id = planner.id.to_string();

    let read = ok_as("inbox", &db, &["planner", "screen", &id, "--cmux", cmux]);
    assert_eq!(
        read["planner_id"],
        planner.id.to_string().parse::<i64>().unwrap()
    );
    assert!(read["screen"].as_str().unwrap().contains("Done."));
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
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0]["actor"]["role"], "inbox");
    assert_eq!(reads[0]["payload"]["target"], "planner");
    assert_eq!(reads[0]["payload"]["workspace_id"], "PW-1");
    assert_eq!(events(&db, "screen_input_sent").len(), 1);
}

#[test]
fn a_planner_a_worker_and_the_jobs_are_refused_and_recorded() {
    let (_dir, db) = queue();
    let run = run_in(&db, "guarded", None, "WS-1");
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
