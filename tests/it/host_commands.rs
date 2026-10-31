//! The host's dialogue and operations through the queue service
//! (ADR-t3102-2 decision 6 (4), ADR-t3102-3 decision 3): `answer`, `ask
//! close`, `hold`, `release`, `replan request`, `replan resolve`,
//! `rebind`, `recover`, `run screen|send|log`, `planner
//! screen|send|log|request`, `report` and `graph --out` of the user and
//! the inbox go to the queue's service as the caller's principal and
//! print what the command line printed on the
//! DB, which its own function (`compose::run_command`) answers on a twin
//! of the queue opened directly; the service refuses a worker's
//! client-mode `dagq` each of them and records the refusal; and each write
//! is recorded as the caller's principal.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use crate::common::{
    self, Bounded, WithoutActor,
    cli::invoke_with,
    service::{Queue, events, queue, start},
};
use dagq::{
    application::{
        ProcessControl, TaskStore, queue_commands as commands, queue_commands::QueueCommand,
        session_log,
    },
    domain::{
        ActorContext, EventKind, PlannerOrigin, PlannerRoute, TaskAction,
        background_wrapper::BackgroundHandle,
        queue_service::{CREDENTIAL_FILE_ENV, Principal, SOCKET_ENV, SessionProcess},
    },
    infrastructure::{
        adapters::SystemProcesses, location::planners_dir, queue_service as service,
        run_files::LocalRunFiles, sqlite::SqliteQueue,
    },
};
use serde_json::{Value, json};

/// The JSON `value` is made of, its values replaced by their kinds: what
/// the command line printed, whatever ids and times it named.
fn shape(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), shape(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(shape).collect()),
        Value::Null => json!("null"),
        Value::Bool(_) => json!("bool"),
        Value::Number(_) => json!("number"),
        Value::String(_) => json!("string"),
    }
}

/// What `args` printed as the caller of `env`, which must have succeeded.
fn printed(db: &Path, env: &[(&str, &str)], args: &[&str]) -> Value {
    let output = invoke_with(env, db, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The error `args` printed as the caller of `env`, which must have failed.
fn failed(db: &Path, env: &[(&str, &str)], args: &[&str]) -> String {
    let output = invoke_with(env, db, args);
    assert!(!output.status.success(), "{args:?} succeeded");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    error["error"].as_str().unwrap().to_owned()
}

/// `command` run on the DB of `twin` as `actor`, by the command line's own
/// function, a `report` with the diagram drawn here first.
fn direct(twin: &Queue, actor: &ActorContext, command: &QueueCommand) -> anyhow::Result<Value> {
    let mut queue = SqliteQueue::open(&twin.db)
        .unwrap()
        .with_actor(actor.clone());
    let command = dagq::compose::prepare_command(command.clone(), Some(&twin.db), |diagram| {
        dagq::compose::run_command(&mut queue, &twin.db, diagram)
    })?;
    dagq::compose::run_command(&mut queue, &twin.db, &command)
}

/// The session of this test's process, as `dagq inbox` binds its token
/// and the tests' `dagq` of the inbox is given one.
fn this_session() -> SessionProcess {
    let pid = std::process::id();
    SessionProcess {
        pid,
        start: SystemProcesses.session_start(pid).unwrap(),
    }
}

/// The background handle of this test's process.
fn live_handle() -> String {
    let pid = std::process::id();
    BackgroundHandle::new(pid, &SystemProcesses.start_identity(pid).unwrap()).to_string()
}

/// The queue's task 2, ready, a headless planner of the runtime's at work
/// in this process with its `session.log`, the run's background session
/// with its log, and an ask to answer and one answered to close: the same
/// on each queue it readies.
fn ready(queue: &Queue) -> (String, PathBuf) {
    let mut sqlite = SqliteQueue::open(&queue.db).unwrap();
    let task = sqlite.add(common::queue::new_task("held")).unwrap().id();
    sqlite.transition(task, TaskAction::BypassReview).unwrap();
    let planner = sqlite
        .open_planner(PlannerOrigin::Runtime, None)
        .unwrap()
        .id;
    let pid = std::process::id();
    sqlite
        .set_planner_route(planner, PlannerRoute::Headless)
        .unwrap();
    sqlite
        .planner_workspace_created(planner, &live_handle())
        .unwrap();
    sqlite.register_planner_wrapper(planner, pid).unwrap();
    sqlite.register_planner_agent(planner, pid, pid).unwrap();
    let planner_dir = planners_dir(&queue.db).join(planner.to_string());
    fs::create_dir_all(&planner_dir).unwrap();
    fs::write(
        planner_dir.join("session.log"),
        "[dagq] the planner's turn\n",
    )
    .unwrap();
    let log = queue.dir().join("run-session.log");
    fs::write(&log, "[dagq] the run's turn\n").unwrap();
    sqlite
        .record_runtime_event(
            queue.run.id(),
            EventKind::WrapperLaunched,
            json!({"workspace_id": live_handle(), "log": log.display().to_string()}),
        )
        .unwrap();
    drop(sqlite);
    for kind in ["decide", "approve_landing"] {
        printed(
            &queue.db,
            &[],
            &[
                "ask",
                "--kind",
                kind,
                "--task",
                "2",
                "--question",
                "which?",
                "--because",
                "scope",
            ],
        );
    }
    printed(&queue.db, &[], &["answer", "2", "--text", "this"]);
    let repo = queue.dir().join("checkout");
    fs::create_dir(&repo).unwrap();
    common::template::repository(&repo, "host-commands");
    (task.to_string(), repo)
}

/// A command as its caller of `env` runs it through the service, and as
/// the actor runs it on the DB.
type Case<'a> = (
    &'a [(&'a str, &'a str)],
    Vec<&'a str>,
    &'a ActorContext,
    QueueCommand,
);

/// `dagq args` in client mode (the service's socket and a token, no
/// queue's path) as `principal`, claiming `role`, from a directory that is
/// no repository.
fn in_client_mode(queue: &Queue, principal: &Principal, role: &str, args: &[&str]) -> Output {
    let credential = service::issue(queue.dir(), principal, 1).unwrap().file;
    let elsewhere = queue.dir().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .current_dir(elsewhere)
        .env(SOCKET_ENV, service::socket_path(queue.dir()))
        .env(CREDENTIAL_FILE_ENV, credential)
        .env("DAGQ_ROLE", role)
        .args(args)
        .bounded_output()
        .unwrap()
}

/// `dagq args` in client mode as the worker of the queue's run.
fn as_worker(queue: &Queue, args: &[&str]) -> Output {
    in_client_mode(
        queue,
        &Principal::worker(queue.run.id(), queue.run.task_id()),
        "worker",
        args,
    )
}

/// Acceptance (1) and (3): each command, through the service, prints what
/// its function prints on the DB, and is refused a worker; each write is
/// the caller's principal's.
#[test]
fn each_host_command_runs_through_the_service_as_on_the_db_and_is_refused_a_worker() {
    let served = queue();
    let twin = queue();
    let (task, repo) = ready(&served);
    let (_, twin_repo) = ready(&twin);
    let user = ActorContext::user();
    let inbox_principal = Principal::inbox(this_session());
    let inbox = inbox_principal.actor();
    let as_inbox = [("DAGQ_ROLE", "inbox")];
    let run = served.run.id().to_string();
    let twin_run = twin.run.id().to_string();
    let run_params = |run: &str| commands::RunParams {
        run: run.to_owned(),
    };
    let planner = commands::PlannerParams { planner: 1 };

    // The service refuses the worker of the run, before it ends, each of
    // them, and records it as the worker's.
    let refusals: &[(&[&str], &str)] = &[
        (&["answer", "1", "--text", "no"], "ask.answer"),
        (&["ask", "close", "1"], "ask.close"),
        (&["hold", "1", "--reason", "mine"], "task.hold"),
        (&["release", "1"], "task.hold"),
        (
            &["replan", "request", "--run", &run, "--reason", "r"],
            "replan.request",
        ),
        (
            &[
                "replan",
                "resolve",
                "1",
                "--generation",
                "1",
                "--continue",
                "--reason",
                "r",
            ],
            "replan.resolve",
        ),
        (&["rebind", "--repo", "/nowhere"], "queue.admin"),
        (&["recover", &run], "run.recover"),
        (&["run", "screen", &run], "screen.read"),
        (&["run", "send", &run], "screen.send"),
        (&["run", "log", &run], "screen.read"),
        (&["planner", "screen", "1"], "screen.read"),
        (&["planner", "send", "1"], "screen.send"),
        (&["planner", "log", "1"], "screen.read"),
        (
            &["planner", "request", "1", "--text", "t"],
            "planner.request",
        ),
        // Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
        #[cfg(any())]
        (&["report"], "queue.export"),
        (
            &["graph", "--out", "g.d2", "--format", "d2"],
            "queue.export",
        ),
    ];
    let before = events(&served.db, "authorization_denied").len();
    for (args, capability) in refusals {
        let output = as_worker(&served, args);
        assert!(!output.status.success(), "{args:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(
            error["queue_service"]["code"], "authorization_denied",
            "{args:?}"
        );
        assert_eq!(error["denied"]["capability"], *capability, "{args:?}");
    }
    let denied = events(&served.db, "authorization_denied");
    assert_eq!(denied.len(), before + refusals.len());
    assert!(
        denied[before..]
            .iter()
            .all(|event| event["actor"]["role"] == "worker"),
        "{denied:?}"
    );
    assert!(!served.dir().join("elsewhere").join("g.d2").exists());

    // Each command's output on the served queue, and the same command on
    // the twin's DB, compared by their shape.
    let twin_common_dir = dagq::compose::repository_common_dir(&twin_repo).unwrap();
    let cases: Vec<Case<'_>> = vec![
        (
            &as_inbox,
            vec!["answer", "1", "--text", "yes"],
            &inbox,
            QueueCommand::Answer(commands::AnswerParams {
                id: 1,
                text: "yes".to_owned(),
            }),
        ),
        (
            &[],
            vec!["ask", "close", "2"],
            &user,
            QueueCommand::AskClose(commands::AskCloseParams { id: 2 }),
        ),
        (
            &[],
            vec!["hold", &task, "--reason", "wait"],
            &user,
            QueueCommand::Hold(commands::HoldParams {
                target: task.clone(),
                reason: "wait".to_owned(),
                until: None,
            }),
        ),
        (
            &as_inbox,
            vec!["release", &task],
            &inbox,
            QueueCommand::Release(commands::ReleaseParams {
                target: task.clone(),
                provider: None,
                model: None,
                work: None,
            }),
        ),
        (
            &[],
            vec!["replan", "request", "--run", &run, "--reason", "split it"],
            &user,
            QueueCommand::ReplanRequest(commands::ReplanRequestParams {
                run: twin_run.clone(),
                reason: "split it".to_owned(),
                evidence: Vec::new(),
            }),
        ),
        (
            &as_inbox,
            vec!["planner", "screen", "1"],
            &inbox,
            QueueCommand::PlannerScreen(planner.clone()),
        ),
        (
            &as_inbox,
            vec!["planner", "request", "1", "--text", "and the docs"],
            &inbox,
            QueueCommand::PlannerRequest(commands::PlannerRequestParams {
                planner: 1,
                text: "and the docs".to_owned(),
            }),
        ),
        // Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
        #[cfg(any())]
        (
            &[],
            vec!["report", "--print", "json"],
            &user,
            QueueCommand::Report(commands::ReportParams {
                period: "day".to_owned(),
                at: None,
                print: true,
                files: false,
                diagram: None,
                checkout: None,
                areas: None,
            }),
        ),
        (
            &[],
            vec!["recover", &run],
            &user,
            QueueCommand::Recover(run_params(&twin_run)),
        ),
        (
            &[],
            vec!["rebind", "--repo", repo.to_str().unwrap()],
            &user,
            QueueCommand::Rebind(commands::RebindParams {
                git_common_dir: Some(twin_common_dir.clone()),
                repository_queue_dir: dagq::compose::repository_queue_dir(&twin_common_dir),
            }),
        ),
    ];
    for (env, args, actor, command) in &cases {
        let answer = printed(&served.db, env, args);
        let expected = direct(&twin, actor, command).unwrap();
        // The caller repairs the worktrees the queue's part lists.
        let expected = match command {
            QueueCommand::Rebind(_) => {
                dagq::compose::repair_rebound_worktrees(&twin_repo, expected).unwrap()
            }
            _ => expected,
        };
        assert_eq!(shape(&answer), shape(&expected), "{args:?}: {answer}");
    }

    // The screens and the input no session has are refused with the same
    // reason as on the DB.
    for (args, command, ids) in [
        (
            vec!["run", "screen", run.as_str()],
            QueueCommand::RunScreen(run_params(&twin_run)),
            true,
        ),
        (
            vec!["run", "send", run.as_str()],
            QueueCommand::RunSend(run_params(&twin_run)),
            true,
        ),
        (
            vec!["planner", "send", "1"],
            QueueCommand::PlannerSend(planner.clone()),
            false,
        ),
    ] {
        let error = failed(&served.db, &as_inbox, &args);
        let expected = format!("{:#}", direct(&twin, &inbox, &command).unwrap_err());
        let expected = if ids {
            expected.replace(twin_run.as_str(), &run)
        } else {
            expected
        };
        assert_eq!(error, expected, "{args:?}");
    }

    // The logs are printed from their files, as the command line printed
    // them; the diagram is drawn from the service's source and written.
    for (args, log) in [
        (
            vec!["run", "log", run.as_str()],
            served.dir().join("run-session.log"),
        ),
        (
            vec!["planner", "log", "1", "--lines", "1"],
            planners_dir(&served.db).join("1").join("session.log"),
        ),
    ] {
        let output = invoke_with(&as_inbox, &served.db, &args);
        assert!(output.status.success(), "{args:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            fs::read_to_string(&log).unwrap()
        );
    }
    let session = direct(&twin, &inbox, &QueueCommand::PlannerLog(planner)).unwrap();
    assert!(
        session["session"]["log"]
            .as_str()
            .unwrap()
            .ends_with("planners/1/session.log"),
        "{session}"
    );
    let out = served.dir().join("graph.d2");
    let written = printed(
        &served.db,
        &[],
        &["graph", "--format", "d2", "--out", out.to_str().unwrap()],
    );
    let diagram = direct(
        &twin,
        &user,
        &QueueCommand::GraphExport(commands::GraphExportParams { goal: None }),
    )
    .unwrap();
    assert_eq!(written["format"], "d2");
    assert_eq!(written["out"], out.display().to_string());
    assert_eq!(written["tasks"], diagram["tasks"]);
    assert_eq!(fs::read_to_string(&out).unwrap(), diagram["source"]);

    // Each write is the caller's principal's.
    let actor_of =
        |kind: &str| -> Value { events(&served.db, kind).last().unwrap()["actor"]["id"].clone() };
    assert_eq!(actor_of("ask_answered"), json!(inbox.actor_id()));
    assert_eq!(actor_of("task_held"), json!(user.actor_id()));
    assert_eq!(actor_of("task_released"), json!(inbox.actor_id()));
    assert_eq!(actor_of("planner_request_handed"), json!(inbox.actor_id()));
    assert_eq!(actor_of("run_recovered"), json!(user.actor_id()));
}

/// Acceptance (1) in client mode: the inbox's client-mode `dagq` (the
/// service's socket and its token, no queue's path) runs each command
/// through the service and prints what its function prints on the DB (a
/// log, the file of the session it answers), is refused what no session
/// has with the DB's reason, and each write is the inbox's.
#[test]
fn each_host_command_runs_in_client_mode_as_on_the_db() {
    let served = queue();
    let twin = queue();
    let (task, repo) = ready(&served);
    let (_, twin_repo) = ready(&twin);
    let principal = Principal::inbox(this_session());
    let inbox = principal.actor();
    let run = served.run.id().to_string();
    let twin_run = twin.run.id().to_string();
    let twin_common_dir = dagq::compose::repository_common_dir(&twin_repo).unwrap();
    let run_params = |run: &str| commands::RunParams {
        run: run.to_owned(),
    };
    let planner = commands::PlannerParams { planner: 1 };
    let cases: Vec<(Vec<&str>, QueueCommand)> = vec![
        (
            vec!["answer", "1", "--text", "yes"],
            QueueCommand::Answer(commands::AnswerParams {
                id: 1,
                text: "yes".to_owned(),
            }),
        ),
        (
            vec!["ask", "close", "2"],
            QueueCommand::AskClose(commands::AskCloseParams { id: 2 }),
        ),
        (
            vec!["hold", &task, "--reason", "wait"],
            QueueCommand::Hold(commands::HoldParams {
                target: task.clone(),
                reason: "wait".to_owned(),
                until: None,
            }),
        ),
        (
            vec!["replan", "request", "--run", &run, "--reason", "split it"],
            QueueCommand::ReplanRequest(commands::ReplanRequestParams {
                run: twin_run.clone(),
                reason: "split it".to_owned(),
                evidence: Vec::new(),
            }),
        ),
        (
            vec!["planner", "request", "1", "--text", "and the docs"],
            QueueCommand::PlannerRequest(commands::PlannerRequestParams {
                planner: 1,
                text: "and the docs".to_owned(),
            }),
        ),
        // Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
        #[cfg(any())]
        (
            vec!["report", "--print", "json"],
            QueueCommand::Report(commands::ReportParams {
                period: "day".to_owned(),
                at: None,
                print: true,
                files: false,
                diagram: None,
                checkout: None,
                areas: None,
            }),
        ),
        (
            vec!["recover", &run],
            QueueCommand::Recover(run_params(&twin_run)),
        ),
        (
            vec!["rebind", "--repo", repo.to_str().unwrap()],
            QueueCommand::Rebind(commands::RebindParams {
                git_common_dir: Some(twin_common_dir.clone()),
                repository_queue_dir: dagq::compose::repository_queue_dir(&twin_common_dir),
            }),
        ),
    ];
    for (args, command) in &cases {
        let output = in_client_mode(&served, &principal, "inbox", args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let answer: Value = serde_json::from_slice(&output.stdout).unwrap();
        let expected = direct(&twin, &inbox, command).unwrap();
        let expected = match command {
            QueueCommand::Rebind(_) => {
                dagq::compose::repair_rebound_worktrees(&twin_repo, expected).unwrap()
            }
            _ => expected,
        };
        assert_eq!(shape(&answer), shape(&expected), "{args:?}: {answer}");
    }

    // The planner's screen is what its function answers on the DB, but
    // for the queue's directory its turns are under, which the service
    // names as it resolves it.
    let output = in_client_mode(&served, &principal, "inbox", &["planner", "screen", "1"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let answer: Value = serde_json::from_slice(&output.stdout).unwrap();
    let expected = direct(&twin, &inbox, &QueueCommand::PlannerScreen(planner.clone())).unwrap();
    let expected: Value = serde_json::from_str(&expected.to_string().replace(
        &twin.dir().display().to_string(),
        &served.dir().canonicalize().unwrap().display().to_string(),
    ))
    .unwrap();
    assert_eq!(answer, expected);

    // The run's screen and the input no session has are refused with the
    // same reason as on the DB.
    for (args, command) in [
        (
            vec!["run", "screen", run.as_str()],
            QueueCommand::RunScreen(run_params(&twin_run)),
        ),
        (
            vec!["run", "send", run.as_str()],
            QueueCommand::RunSend(run_params(&twin_run)),
        ),
        (
            vec!["planner", "send", "1"],
            QueueCommand::PlannerSend(planner.clone()),
        ),
    ] {
        let output = in_client_mode(&served, &principal, "inbox", &args);
        assert!(!output.status.success(), "{args:?} succeeded");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        let expected = format!("{:#}", direct(&twin, &inbox, &command).unwrap_err());
        assert_eq!(
            error["error"],
            expected.replace(twin_run.as_str(), &run),
            "{args:?}"
        );
    }

    // The logs are printed from the files the service names, as the
    // command line prints the session its function answers on the DB: the
    // run's whole, and the planner's last line of two.
    for queue in [&served, &twin] {
        let log = planners_dir(&queue.db).join("1").join("session.log");
        let mut text = fs::read_to_string(&log).unwrap();
        text.push_str("[dagq] the planner's next turn\n");
        fs::write(&log, text).unwrap();
    }
    for (args, command, lines) in [
        (
            vec!["run", "log", run.as_str()],
            QueueCommand::RunLog(run_params(&twin_run)),
            None,
        ),
        (
            vec!["planner", "log", "1", "--lines", "1"],
            QueueCommand::PlannerLog(planner),
            Some(1),
        ),
    ] {
        let output = in_client_mode(&served, &principal, "inbox", &args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let session = direct(&twin, &inbox, &command).unwrap()["session"].clone();
        let mut expected = Vec::new();
        session_log::print(
            &session_log::LogPorts {
                files: &LocalRunFiles,
                processes: &SystemProcesses,
            },
            &session_log::SessionLog {
                session: serde_json::from_value(session).unwrap(),
            },
            lines,
            None,
            &mut expected,
        )
        .unwrap();
        let expected = String::from_utf8(expected).unwrap();
        assert!(!expected.is_empty(), "{args:?}");
        if let Some(lines) = lines {
            assert_eq!(expected.lines().count(), lines, "{expected}");
        }
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            expected,
            "{args:?}"
        );
    }

    // `graph --out` writes, on the client's side, the source the service
    // answers, as the command line writes it.
    let out = served.dir().join("graph.d2");
    let output = in_client_mode(
        &served,
        &principal,
        "inbox",
        &["graph", "--format", "d2", "--out", out.to_str().unwrap()],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let written: Value = serde_json::from_slice(&output.stdout).unwrap();
    let diagram = direct(
        &twin,
        &inbox,
        &QueueCommand::GraphExport(commands::GraphExportParams { goal: None }),
    )
    .unwrap();
    assert_eq!(written["format"], "d2");
    assert_eq!(written["out"], out.display().to_string());
    assert_eq!(written["tasks"], diagram["tasks"]);
    assert_eq!(fs::read_to_string(&out).unwrap(), diagram["source"]);

    // Each write is the inbox's.
    let actor_of =
        |kind: &str| -> Value { events(&served.db, kind).last().unwrap()["actor"]["id"].clone() };
    for kind in [
        "ask_answered",
        "task_held",
        "planner_request_handed",
        "run_recovered",
    ] {
        assert_eq!(actor_of(kind), json!(inbox.actor_id()), "{kind}");
    }
}

/// The inbox's `answer` to the queue's Claude hold (`queue_hold`) goes
/// through the service as the inbox's principal, whatever provider the
/// inbox runs on (ADR-t1181-1 decision 4): it is recorded as the inbox's
/// delegated answer for the runtime to deliver, and the ask stays open
/// for the runtime, which applies `done` to every answered hold whoever
/// answered it (`runtime_queue_hold::an_open_usage_limit_ask_holds_claims_and_reviews_until_done`).
#[test]
fn the_inbox_answers_the_queue_hold_through_the_service_for_the_runtime_to_apply() {
    let served = queue();
    let hold = SqliteQueue::open(&served.db)
        .unwrap()
        .hold(dagq::domain::NewHold {
            reason_category: dagq::domain::AskReason::Cost,
            subject: Some(dagq::domain::queue_hold::USAGE_LIMIT_SUBJECT.to_owned()),
            run_id: None,
            job: None,
            question: "Claude's usage limit".into(),
            options: dagq::domain::HOLD_OPTIONS
                .iter()
                .map(|option| (*option).to_owned())
                .collect(),
            asked_by: "supervisor".into(),
        })
        .unwrap()
        .ask;
    start(&served);
    let output = in_client_mode(
        &served,
        &Principal::inbox(this_session()),
        "inbox",
        &["answer", &hold.id.to_string(), "--text", "done"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let answered: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(answered["answered_by"], "inbox", "{answered}");
    assert_eq!(answered["answer_authority"], "delegated", "{answered}");
    let events = events(&served.db, "ask_answered");
    let event = events
        .iter()
        .find(|event| event["payload"]["ask_id"] == json!(hold.id))
        .unwrap();
    assert_eq!(event["payload"]["runtime_delivers"], true, "{event}");
    assert_eq!(event["actor"]["role"], "inbox", "{event}");
    let ask = SqliteQueue::open(&served.db)
        .unwrap()
        .read_ask(hold.id)
        .unwrap();
    assert_eq!(ask.answer.as_deref(), Some("done"));
    assert!(ask.closed_at.is_none());
}
