//! The session wrappers and their hook reach the queue only through its
//! service (ADR-t3102-1 decision 4, docs/design/queue-service.md): `dagq
//! session` and `dagq planner-session` read and record as the wrapper's
//! principal, whose token the control side issued, and never open the
//! queue, whatever actor their environment names; one whose service does
//! not answer ends with that error; a planner's hook leaves its report for
//! its wrapper; and the worker's token is refused the wrapper's use cases.
//!
//! The wrappers here are given a queue path in a directory that holds no
//! queue, only the queue's `service/` (a link to the real one): a wrapper
//! that opened the queue would find none there.

use crate::common::{self, WithoutActor};
use common::service::{call, events, queue, start, token};

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use dagq::{
    application::{ProcessControl, SessionRegistry},
    domain::{
        EventKind, PlannerOrigin,
        background_wrapper::BackgroundHandle,
        queue_service::{Principal, ServiceErrorCode, UseCase},
        session_step::{StepTarget, TargetKind},
    },
    infrastructure::{adapters::SystemProcesses, sqlite::SqliteQueue},
};
use serde_json::{Value, json};

/// The variables the wrapper inherits from the worker it runs the session
/// of: the worker's, which it does not act as.
const WORKER_ENV: [(&str, &str); 3] = [
    ("DAGQ_ROLE", "worker"),
    ("DAGQ_ACTOR_ID", "worker:x"),
    ("DAGQ_TASK_ID", "1"),
];

/// A queue path beside `dir`'s queue whose directory holds the queue's
/// `service/` only, by a link: the service's socket and the tokens are
/// found there, and no queue is.
fn without_queue(dir: &Path) -> PathBuf {
    let alias = dir.join("a");
    std::fs::create_dir(&alias).unwrap();
    std::os::unix::fs::symlink(dir.join("service"), alias.join("service")).unwrap();
    alias.join("queue.db")
}

fn wrapper(db: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command
        .without_actor_env()
        .envs(WORKER_ENV)
        .arg("--db")
        .arg(db)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn finished(command: &mut Command) -> Output {
    let _waiting = common::within(common::STEP_LIMIT, "the session wrapper to end");
    command.output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Acceptance (1): a run's wrapper reads the run and waits for its start
/// through the service, and its registration under a lease it does not
/// hold is refused by the queue behind the service, recorded nowhere,
/// without its process opening the queue (there is none where it looks).
/// With the service gone, it ends at once with the service's error.
#[test]
fn a_runs_wrapper_reaches_its_run_only_through_the_service() {
    let queue = queue();
    let run = queue.run.clone();
    start(&queue);
    token(
        &queue,
        &Principal::wrapper(&StepTarget::run(run.id()), Some(run.task_id())),
    );
    let alias = without_queue(queue.dir());
    let args = [
        "session",
        "--run",
        run.id().as_str(),
        "--lease",
        "not-the-lease",
        "--claude",
        "/nonexistent/claude",
        "--codex",
        "/nonexistent/codex",
        "--background",
    ];
    let child = wrapper(&alias, &args).spawn().unwrap();
    // The supervisor's record of the wrapper's start, which it waits for.
    let handle = BackgroundHandle::new(
        child.id(),
        &SystemProcesses.start_identity(child.id()).unwrap(),
    );
    SqliteQueue::open(&queue.db)
        .unwrap()
        .record_runtime_event(
            run.id(),
            EventKind::WrapperLaunched,
            json!({"pid": handle.pid, "start": handle.start, "workspace_id": handle.to_string()}),
        )
        .unwrap();
    let output = {
        let _waiting = common::within(common::STEP_LIMIT, "the refused wrapper to end");
        child.wait_with_output().unwrap()
    };
    assert!(!output.status.success());
    let error = stderr(&output);
    assert!(error.contains("register_wrapper"), "{error}");
    assert!(error.contains("run lease is missing"), "{error}");
    assert!(!alias.exists(), "{error}");
    assert!(
        SqliteQueue::open(&queue.db)
            .unwrap()
            .run_events(run.id())
            .unwrap()
            .iter()
            .all(|event| event.kind != EventKind::WrapperStarted),
    );

    // The service stopped: the wrapper says so and ends.
    common::cli::ok(&queue.db, &["service", "stop"]);
    let before = SqliteQueue::open(&queue.db).unwrap().all_events().unwrap();
    let output = finished(&mut wrapper(&alias, &args));
    assert!(!output.status.success());
    let error = stderr(&output);
    assert!(error.contains("queue service"), "{error}");
    assert!(error.contains("does not open the queue itself"), "{error}");
    assert_eq!(
        SqliteQueue::open(&queue.db).unwrap().all_events().unwrap(),
        before
    );
}

/// Acceptance (1): a planner's wrapper registers, finds its planner and
/// records its exit through the service as the wrapper, without opening
/// the queue; one without its token is refused before anything.
#[test]
fn a_planners_wrapper_records_through_the_service_as_the_wrapper() {
    let queue = queue();
    start(&queue);
    let sqlite = SqliteQueue::open(&queue.db).unwrap();
    let planner = sqlite.open_planner(PlannerOrigin::Runtime, None).unwrap();
    let id = planner.id.to_string();
    let alias = without_queue(queue.dir());
    let args = [
        "planner-session",
        "--planner",
        &id,
        "--claude",
        "/nonexistent/claude",
        "--headless",
        "--background",
    ];
    // No token issued: its first request is refused, and nothing written.
    let output = finished(&mut wrapper(&alias, &args));
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("no_credential"),
        "{}",
        stderr(&output)
    );
    assert_eq!(sqlite.planner(planner.id).unwrap().wrapper_pid, None);

    token(
        &queue,
        &Principal::wrapper(
            &StepTarget::of(TargetKind::Planner, planner.id.as_i64()),
            None,
        ),
    );
    // The planner's prompt is not where the wrapper looks: its agent never
    // starts, and the wrapper records that exit.
    let output = finished(&mut wrapper(&alias, &args));
    assert!(!output.status.success());
    let recorded = sqlite.planner(planner.id).unwrap();
    assert_eq!(recorded.exit_code, Some(127), "{}", stderr(&output));
    assert!(recorded.wrapper_pid.is_some());
    // The planner's wrapper writes as itself.
    let actors: Vec<Value> = events(&queue.db, "planner_exited")
        .into_iter()
        .map(|event| event["actor"]["id"].clone())
        .collect();
    assert!(
        actors
            .iter()
            .all(|actor| *actor == json!(format!("wrapper:planner:{id}"))),
        "{actors:?}"
    );
    assert!(!alias.exists());
}

/// Acceptance (1): a planner's hook opens no queue and reaches no service:
/// it leaves its report in the planner's directory, which only the
/// planner's wrapper records.
#[test]
fn a_planners_hook_leaves_its_report_for_its_wrapper() {
    let queue = queue();
    let alias = without_queue(queue.dir());
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command
        .without_actor_env()
        .envs([
            ("DAGQ_ROLE", "planner"),
            ("DAGQ_ACTOR_ID", "planner:3"),
            ("DAGQ_PLANNER_ORIGIN", "runtime"),
            ("DAGQ_PLANNER_ID", "3"),
        ])
        .arg("--db")
        .arg(&alias)
        .args(["session-event", "open"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            json!({"session_id": "s1", "source": "startup"})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
    let output = {
        let _waiting = common::within(common::STEP_LIMIT, "the hook to end");
        child.wait_with_output().unwrap()
    };
    assert!(output.status.success(), "{}", stderr(&output));
    let answer: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(answer["relayed_by"], "wrapper", "{answer}");
    let spool = alias.parent().unwrap().join("planners/3/session-hooks");
    let reports: Vec<_> = std::fs::read_dir(&spool).unwrap().collect();
    assert_eq!(reports.len(), 1);
    assert!(!alias.exists());
    assert!(events(&queue.db, "session_opened").is_empty());
    assert!(
        SqliteQueue::open(&queue.db)
            .unwrap()
            .open_hook_session_spans()
            .unwrap()
            .is_empty()
    );
}

/// Acceptance (3), through the service: the worker's token is refused the
/// wrapper's use cases on its own run (the records and reads of its turn's
/// container's parts among them), recorded as `authorization_denied`,
/// while the wrapper's token is answered.
#[test]
fn the_workers_token_is_refused_the_wrappers_use_cases() {
    let queue = queue();
    start(&queue);
    let run = queue.run.clone();
    let worker = token(&queue, &Principal::worker(run.id(), run.task_id()));
    let wrapper = token(
        &queue,
        &Principal::wrapper(&StepTarget::run(run.id()), Some(run.task_id())),
    );
    let heartbeat = json!({"op": "heartbeat_wrapper", "run": run.id(), "pid": 1});
    let record = json!({"op": "runtime_event", "run": run.id(), "kind": "turn_started",
                        "payload": {"by": "test"}});
    let owner = json!({"kind": "run", "id": run.id()});
    let image = json!({"op": "image_event", "owner": owner, "kind": "container_image_built",
                       "payload": {"reference": "localhost/q:ab"}});
    let machine_user = json!({"op": "machine_user_run", "owner": owner, "run": run.id()});
    for (use_case, params, capability) in [
        (UseCase::SessionRun, &heartbeat, "session.run"),
        (UseCase::SessionRecord, &record, "session.record"),
        (UseCase::SessionRecord, &image, "session.record"),
        (UseCase::SessionRun, &machine_user, "session.run"),
    ] {
        let refused = call(&queue, Some(&worker), use_case, params.clone());
        assert_eq!(
            refused["error"]["code"],
            ServiceErrorCode::AuthorizationDenied.as_str(),
            "{refused}"
        );
        assert_eq!(refused["error"]["denied"]["capability"], capability);
    }
    let denied = events(&queue.db, "authorization_denied");
    assert_eq!(denied.len(), 4, "{denied:?}");
    assert!(
        denied
            .iter()
            .all(|event| event["actor"]["role"] == "worker")
    );
    let read = call(
        &queue,
        Some(&wrapper),
        UseCase::SessionRun,
        json!({"op": "run", "run": run.id()}),
    );
    assert_eq!(read["result"]["value"]["id"], json!(run.id()), "{read}");
    let recorded = call(&queue, Some(&wrapper), UseCase::SessionRecord, record);
    assert_eq!(recorded["ok"], true, "{recorded}");
    let started = events(&queue.db, "turn_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["actor"]["id"], format!("wrapper:{}", run.id()));
    // A planner's wrapper's token is not the run's.
    let planner = SqliteQueue::open(&queue.db)
        .unwrap()
        .open_planner(PlannerOrigin::Runtime, None)
        .unwrap();
    let other = token(
        &queue,
        &Principal::wrapper(
            &StepTarget::of(TargetKind::Planner, planner.id.as_i64()),
            None,
        ),
    );
    let refused = call(&queue, Some(&other), UseCase::SessionRun, heartbeat);
    assert_eq!(
        refused["error"]["code"],
        ServiceErrorCode::AuthorizationDenied.as_str(),
        "{refused}"
    );
}

/// The records and reads of the container's parts a run's wrapper readies
/// for its turn (the image's builds and failures, the runs that own users
/// of the dagq machine) go through the service as the wrapper's, while its
/// process refuses to open the queue; another queue's runs are not
/// reached, and the image's last record is not the wrapper's to read.
/// Once the service stops, a record fails as out of reach and a run reads
/// as unknown, still without opening the queue.
#[test]
fn a_runs_wrapper_readies_its_containers_parts_through_the_service_alone() {
    use dagq::application::container_image::ImageRecords;
    use dagq::compose::WrapperContainerRecords;
    use dagq::domain::broker_relay::RelayRun;
    use dagq::infrastructure::{podman_machine::OwnerRuns, queue_service::ServiceWrapperStore};
    let queue = queue();
    let run = queue.run.clone();
    start(&queue);
    let target = StepTarget::run(run.id());
    token(&queue, &Principal::wrapper(&target, Some(run.task_id())));
    let refused = dagq::infrastructure::sqlite::refuse_opening(&queue.db);
    let records =
        WrapperContainerRecords::new(ServiceWrapperStore::of_queue(&queue.db, target), &queue.db);

    records
        .record(
            EventKind::ContainerImageBuilt,
            json!({"reference": "localhost/dagq-q:ab"}),
        )
        .unwrap();
    assert!(records.latest().is_err());
    assert_eq!(
        records.run(&queue.db, run.id().as_str()),
        Some(RelayRun::Found {
            status: run.status(),
            task_in_progress: true,
        })
    );
    assert_eq!(
        records.run(&queue.db, "no-such-run"),
        Some(RelayRun::Missing)
    );
    assert_eq!(
        records.run(&queue.dir().join("other").join("queue.db"), "r2"),
        None
    );

    common::cli::ok(&queue.db, &["service", "stop"]);
    let error = records
        .record(EventKind::ContainerImageFailed, json!({}))
        .unwrap_err();
    assert!(format!("{error:#}").contains("queue service"), "{error:#}");
    assert_eq!(
        records.run(&queue.db, run.id().as_str()),
        Some(RelayRun::Unknown)
    );
    drop(refused);
    let built = events(&queue.db, "container_image_built");
    assert_eq!(built.len(), 1, "{built:?}");
    assert_eq!(built[0]["actor"]["id"], format!("wrapper:{}", run.id()));
    assert!(events(&queue.db, "container_image_failed").is_empty());
}
