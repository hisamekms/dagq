//! An executor's telemetry through the queue service on the host
//! (docs/design/queue-service.md, ADR-t1662-3 decisions 2, 4, 9 to 11): a
//! token the control side issued for one target sends the steps and the
//! resource samples of a session the control side registered to that
//! target, through the client mode's TelemetrySink into the stores, and
//! reaches no other use case; another target's steps, a session registered
//! to no target or to another, an agent's token, a version the service
//! does not read, a report past its bounds and a revoked token are
//! refused, and the largest report within its bounds is recorded. The
//! token of a target of no run is issued only to the parent that started
//! it.

use crate::common;
use common::service::{call, events, queue, start, token};

use dagq::{
    application::{
        SessionOwners, SessionResourceStore, SessionStepStore, TelemetrySink,
        queue_service::TelemetryTokens,
    },
    domain::{
        ActorContext,
        queue_service::{Principal, UseCase},
        session_step::{ResourceSample, Step, StepKind, StepTarget, StepsRecorded, TargetKind},
        telemetry::{
            MAX_ID_BYTES, MAX_NAME_BYTES, MAX_REPORT_BYTES, MAX_SHAPE_BYTES, MAX_STEPS_PER_REPORT,
            ReportRecorded, TELEMETRY_VERSION, TelemetryReport,
        },
    },
    infrastructure::{
        queue_service::{self as service, ServiceTelemetrySink, SystemServiceAccess},
        sqlite::SqliteQueue,
    },
};
use serde_json::{Value, json};

fn steps() -> Vec<Step> {
    let mut tool = Step::new(2, StepKind::ToolCall, 5, 3);
    tool.tool = Some("Bash".into());
    vec![Step::new(1, StepKind::ModelTurn, 0, 5), tool]
}

fn sample() -> ResourceSample {
    let mut sample = ResourceSample::new(1, 0, 30_000);
    sample.cpu_seconds = Some(4.5);
    sample.memory_max_bytes = Some(1 << 30);
    sample.node_id = Some("host".into());
    sample
}

fn held(db: &std::path::Path, target: &StepTarget) -> Vec<(String, i64)> {
    SqliteQueue::open(db)
        .unwrap()
        .target_steps(target)
        .unwrap()
        .into_iter()
        .map(|step| (step.session_id, step.step.seq))
        .collect()
}

/// The control side registers `session` to `target`, as it does before
/// the executor reports.
fn register(db: &std::path::Path, target: &StepTarget, session: &str) {
    SqliteQueue::open(db)
        .unwrap()
        .register_session(target, session, 1)
        .unwrap();
}

const fn recorded(added: usize, duplicates: usize) -> StepsRecorded {
    StepsRecorded { added, duplicates }
}

#[test]
fn a_telemetry_token_records_its_targets_steps_and_samples_and_reaches_nothing_else() {
    let queue = queue();
    assert_eq!(start(&queue)["service"]["state"], "running");
    let run = queue.run.id();
    let target = StepTarget::run(run);
    let file = SystemServiceAccess
        .issue_telemetry(&queue.db, &target, Some(queue.run.task_id()), None)
        .unwrap();
    let telemetry_token = service::read_token(&file).unwrap();
    let sink = ServiceTelemetrySink::new(service::socket_path(queue.dir()), file.clone());

    // Its own target's steps and samples go in, once each.
    register(&queue.db, &target, "s1");
    let report = TelemetryReport::new(target.clone(), "s1", steps()).with_resources(vec![sample()]);
    assert_eq!(
        sink.report(&report).unwrap(),
        ReportRecorded {
            steps: recorded(2, 0),
            resources: recorded(1, 0),
        }
    );
    assert_eq!(
        sink.report(&report).unwrap(),
        ReportRecorded {
            steps: recorded(0, 2),
            resources: recorded(0, 1),
        }
    );
    assert_eq!(
        held(&queue.db, &target),
        [("s1".to_owned(), 1), ("s1".to_owned(), 2)]
    );
    let samples = SqliteQueue::open(&queue.db)
        .unwrap()
        .target_resources(&target)
        .unwrap();
    assert_eq!(
        samples.iter().map(|row| &row.sample).collect::<Vec<_>>(),
        [&sample()]
    );

    // Another run's, a planner's or a job's target, of a session
    // registered to it, is refused, and nothing of it is written.
    let others = [
        StepTarget::run(&dagq::domain::RunId::new("another-run").unwrap()),
        StepTarget {
            kind: TargetKind::Planner,
            id: "3".into(),
        },
        StepTarget {
            kind: TargetKind::PlanReview,
            id: "4".into(),
        },
        StepTarget {
            kind: TargetKind::GoalReview,
            id: "5".into(),
        },
        StepTarget {
            kind: TargetKind::ThroughputReview,
            id: "6".into(),
        },
    ];
    for (at, other) in others.iter().enumerate() {
        let session = format!("other-{at}");
        register(&queue.db, other, &session);
        let refused = sink
            .report(&TelemetryReport::new(other.clone(), session, steps()))
            .unwrap_err()
            .to_string();
        assert!(refused.starts_with("authorization_denied"), "{refused}");
        assert!(held(&queue.db, other).is_empty(), "{other:?}");
    }

    // The same token reaches no other use case, a read included.
    let task = queue.run.task_id();
    for (use_case, params) in [
        (UseCase::Status, Value::Null),
        (UseCase::Show, json!({"id": task})),
        (UseCase::Events, json!({"run": run})),
        (
            UseCase::Ask,
            json!({"kind": "worker_question", "because": "scope", "topics": ["design_choice"],
                   "question": "q", "run_id": run}),
        ),
        (UseCase::Note, json!({"run": run, "text": "x"})),
    ] {
        let refused = call(&queue, Some(&telemetry_token), use_case, params);
        assert_eq!(
            refused["error"]["code"], "authorization_denied",
            "{use_case:?}: {refused}"
        );
    }
    let denials = events(&queue.db, "authorization_denied");
    assert_eq!(denials.len(), others.len() + 5, "{denials:?}");
    assert!(
        denials
            .iter()
            .all(|denial| denial["payload"]["role"] == "executor-telemetry"),
        "{denials:?}"
    );
    assert!(events(&queue.db, "note_added").is_empty());
    assert!(events(&queue.db, "ask_opened").is_empty());

    // An agent's token does not report, of its own run included.
    register(&queue.db, &target, "s3");
    let planner = SqliteQueue::open(&queue.db)
        .unwrap()
        .open_planner(dagq::domain::PlannerOrigin::Runtime, None)
        .unwrap();
    for principal in [
        Principal::worker(run, task),
        Principal::of(&ActorContext::review_job(run, 1)),
        Principal::planner(planner.id.as_i64()),
    ] {
        let agent = token(&queue, &principal);
        let refused = call(
            &queue,
            Some(&agent),
            UseCase::TelemetryReport,
            serde_json::to_value(TelemetryReport::new(target.clone(), "s3", steps())).unwrap(),
        );
        assert_eq!(
            refused["error"]["code"], "authorization_denied",
            "{refused}"
        );
    }

    // A version the service does not read and a report past its bounds
    // are refused; a field it does not know is dropped.
    register(&queue.db, &target, "s4");
    let mut unknown =
        serde_json::to_value(TelemetryReport::new(target.clone(), "s4", steps())).unwrap();
    unknown["version"] = json!(TELEMETRY_VERSION + 1);
    let over = TelemetryReport::new(
        target.clone(),
        "s4",
        (1..=i64::try_from(MAX_STEPS_PER_REPORT).unwrap() + 1)
            .map(|seq| Step::new(seq, StepKind::Wait, 0, 1))
            .collect(),
    );
    for params in [unknown, serde_json::to_value(over).unwrap()] {
        let refused = call(
            &queue,
            Some(&telemetry_token),
            UseCase::TelemetryReport,
            params,
        );
        assert_eq!(refused["error"]["code"], "bad_request", "{refused}");
    }
    let mut later = serde_json::to_value(
        TelemetryReport::new(target.clone(), "s4", steps()).with_resources(vec![sample()]),
    )
    .unwrap();
    later["sent_by"] = json!("a later executor");
    later["steps"][0]["later"] = json!(true);
    later["resources"][0]["later"] = json!(true);
    let answered = call(
        &queue,
        Some(&telemetry_token),
        UseCase::TelemetryReport,
        later,
    );
    assert_eq!(
        answered["result"],
        json!({"steps": {"added": 2, "duplicates": 0},
               "resources": {"added": 1, "duplicates": 0}}),
        "{answered}"
    );
    assert_eq!(held(&queue.db, &target).len(), 4);

    // Revoked, the token is no token.
    assert!(
        SystemServiceAccess
            .revoke_telemetry(&queue.db, &target, None)
            .unwrap()
    );
    let refused = call(
        &queue,
        Some(&telemetry_token),
        UseCase::TelemetryReport,
        serde_json::to_value(TelemetryReport::new(target.clone(), "s1", steps())).unwrap(),
    );
    assert_eq!(refused["error"]["code"], "unauthenticated", "{refused}");
    assert!(sink.report(&report).is_err());
    assert_eq!(held(&queue.db, &target).len(), 4);
}

/// A target of no run (an open planner of the runtime's) has its own
/// token, and a session is recorded only for the target the control side
/// registered it to: a token of another target takes neither a registered
/// session nor one registered to none, which its owner still reports once
/// registered.
#[test]
fn a_session_is_recorded_only_for_the_target_the_control_side_registered_it_to() {
    let queue = queue();
    start(&queue);
    let opened = SqliteQueue::open(&queue.db)
        .unwrap()
        .open_planner(dagq::domain::PlannerOrigin::Runtime, None)
        .unwrap();
    let planner = StepTarget::of(TargetKind::Planner, opened.id.as_i64());
    let run = StepTarget::run(queue.run.id());
    let socket = service::socket_path(queue.dir());
    let planner_sink = ServiceTelemetrySink::new(
        socket.clone(),
        SystemServiceAccess
            .issue_telemetry(&queue.db, &planner, None, None)
            .unwrap(),
    );
    let run_sink = ServiceTelemetrySink::new(
        socket,
        SystemServiceAccess
            .issue_telemetry(&queue.db, &run, Some(queue.run.task_id()), None)
            .unwrap(),
    );
    register(&queue.db, &planner, "p1");
    assert_eq!(
        planner_sink
            .report(&TelemetryReport::new(planner.clone(), "p1", steps()))
            .unwrap()
            .steps,
        recorded(2, 0)
    );
    // The run's token on the planner's session, naming its own target,
    // and on a session registered to none: refused, nothing taken.
    for session in ["p1", "free"] {
        let refused = run_sink
            .report(&TelemetryReport::new(run.clone(), session, steps()))
            .unwrap_err()
            .to_string();
        assert!(refused.starts_with("authorization_denied"), "{refused}");
    }
    assert!(held(&queue.db, &run).is_empty());
    let store = SqliteQueue::open(&queue.db).unwrap();
    assert_eq!(store.session_owner("free").unwrap(), None);
    assert!(
        store
            .register_session(&run, "p1", 2)
            .unwrap_err()
            .to_string()
            .contains("another target")
    );
    register(&queue.db, &planner, "free");
    assert_eq!(
        planner_sink
            .report(&TelemetryReport::new(planner.clone(), "free", steps()))
            .unwrap()
            .steps,
        recorded(2, 0)
    );
    assert_eq!(held(&queue.db, &planner).len(), 4);
}

/// A report exactly [`MAX_REPORT_BYTES`] long as JSON, of steps whose model
/// and tool escape to six bytes a character, so its text is the longest
/// any report within the bounds can make.
fn largest(target: StepTarget, session: &str) -> TelemetryReport {
    fn size<T: serde::Serialize>(value: &T) -> usize {
        serde_json::to_vec(value).unwrap().len()
    }
    // Seqs of four digits, so every full step is as long.
    let full = |seq| {
        let mut step = Step::new(seq, StepKind::ToolCall, i64::MAX, i64::MAX);
        step.model = Some("\u{1}".repeat(MAX_NAME_BYTES));
        step.tool = Some("\u{1}".repeat(MAX_NAME_BYTES));
        step.command_shape = Some(json!("x".repeat(MAX_SHAPE_BYTES - 2)));
        step.exit_code = Some(i64::MIN);
        step.background = Some(false);
        step.output_bytes = Some(i64::MAX);
        step
    };
    let mut report = TelemetryReport::new(target, session, vec![]);
    let empty = size(&report);
    let one = size(&full(1000)) + 1;
    let mut last = Step {
        model: Some(String::new()),
        tool: Some(String::new()),
        command_shape: Some(json!("")),
        ..full(1000)
    };
    let count = (MAX_REPORT_BYTES - empty - size(&last)) / one;
    report.steps = (0..count).map(|at| full(1000 + at as i64)).collect();
    last.seq = 1000 + count as i64;
    let budget = MAX_REPORT_BYTES - empty - count * one;
    let chars = ((budget - size(&last)) / 6).min(2 * MAX_NAME_BYTES);
    last.model = Some("\u{1}".repeat(chars.min(MAX_NAME_BYTES)));
    last.tool = Some("\u{1}".repeat(chars.saturating_sub(MAX_NAME_BYTES)));
    let rest = budget - size(&last);
    last.command_shape = Some(json!("x".repeat(rest)));
    report.steps.push(last);
    assert_eq!(size(&report), MAX_REPORT_BYTES);
    report
}

/// The bound of a report is its size as JSON: the largest one within it
/// goes through the service's request line and is recorded, and one a byte
/// larger is not sent, and is refused when sent all the same.
#[test]
fn the_largest_report_is_recorded_and_one_byte_more_is_refused() {
    let queue = queue();
    start(&queue);
    let target = StepTarget::run(queue.run.id());
    let file = SystemServiceAccess
        .issue_telemetry(&queue.db, &target, Some(queue.run.task_id()), None)
        .unwrap();
    let telemetry_token = service::read_token(&file).unwrap();
    let sink = ServiceTelemetrySink::new(service::socket_path(queue.dir()), file);
    let session = "s".repeat(MAX_ID_BYTES);
    register(&queue.db, &target, &session);
    let report = largest(target.clone(), &session);
    let steps = report.steps.len();
    assert!(steps <= MAX_STEPS_PER_REPORT);
    assert_eq!(sink.report(&report).unwrap().steps, recorded(steps, 0));
    assert_eq!(held(&queue.db, &target).len(), steps);

    let mut over = report;
    let last = over.steps.last_mut().unwrap();
    let shape = format!(
        "{}x",
        last.command_shape.as_ref().unwrap().as_str().unwrap()
    );
    last.command_shape = Some(json!(shape));
    let refused = sink.report(&over).unwrap_err().to_string();
    assert!(refused.contains("bytes as JSON"), "{refused}");
    let refused = call(
        &queue,
        Some(&telemetry_token),
        UseCase::TelemetryReport,
        serde_json::to_value(&over).unwrap(),
    );
    assert_eq!(refused["error"]["code"], "bad_request", "{refused}");
    assert_eq!(held(&queue.db, &target).len(), steps);
}

/// The token of the executor of a job a command started (an observation,
/// a throughput review) goes only to the parent that started it, asking
/// from the process its start names (ADR-t3102-1 decision 2), as the
/// issue's transaction reads the start. The user, the inbox and another
/// supervisor naming the observation a supervisor's command started are
/// refused as `authorization_denied` (on the resource), recorded and
/// taking no token, so that supervisor's own request is then issued, not
/// refused as held already. The person's command of another process, and
/// a start that names no process, are refused too.
#[test]
fn only_the_parent_that_started_a_job_is_issued_its_executors_token() {
    use dagq::domain::queue_service::SessionProcess;
    use dagq::domain::{ActorRole, EventKind, LeaseToken};
    let queue = queue();
    start(&queue);
    let pid = std::process::id();
    let (lease, other) = (LeaseToken::new("it-started"), LeaseToken::new("it-other"));
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    db.register_supervisor(&lease, pid, 1, "test").unwrap();
    // Another supervisor, of another process.
    db.register_supervisor(&other, 1, 1, "test").unwrap();
    let started = |db: SqliteQueue, by: ActorContext, kind, payload: Value| {
        let db = db.with_actor(by);
        let id = db.record_queue_event(kind, payload).unwrap().as_i64();
        (db, id)
    };
    let supervisor_actor = ActorContext::instance(ActorRole::Supervisor, pid);
    // The supervisor's observe command, this process.
    let (db, observed) = started(
        db,
        supervisor_actor,
        EventKind::ObserveStarted,
        json!({"dir": "/o/1", "timeout_secs": 600, "pid": pid}),
    );
    // The person's throughput-review command of another process, and one
    // whose start names none.
    let (db, elsewhere) = started(
        db,
        ActorContext::user(),
        EventKind::ThroughputReviewStarted,
        json!({"dir": "/r/1", "timeout_secs": 600, "pid": 1}),
    );
    let (db, unnamed) = started(
        db,
        ActorContext::user(),
        EventKind::ThroughputReviewStarted,
        json!({"dir": "/r/2", "timeout_secs": 600}),
    );
    drop(db);
    let observed = StepTarget::of(TargetKind::Observer, observed);
    let supervisor = token(&queue, &Principal::supervisor(&lease, pid));
    let another = token(&queue, &Principal::supervisor(&other, 1));
    let inbox = token(
        &queue,
        &Principal::inbox(SessionProcess {
            pid,
            start: dagq::infrastructure::adapters::session_start(pid).unwrap(),
        }),
    );
    let issue = |token: Option<&str>, target: &StepTarget| {
        call(
            &queue,
            token,
            UseCase::IssueToken,
            json!({"principal": Principal::telemetry(target, None)}),
        )
    };
    let refused = |token: Option<&str>, target: &StepTarget, why: &str| {
        let before = events(&queue.db, "authorization_denied").len();
        let answer = issue(token, target);
        assert_eq!(answer["error"]["code"], "authorization_denied", "{answer}");
        assert_eq!(
            answer["error"]["denied"]["reason"], "not on this resource",
            "{answer}"
        );
        let denied = events(&queue.db, "authorization_denied");
        assert_eq!(denied.len() - before, 1, "{answer}");
        let last = &denied[denied.len() - 1]["payload"];
        assert_eq!(last["capability"], "token.issue", "{last}");
        assert!(last["why"].as_str().unwrap().contains(why), "{last}");
        assert!(
            SystemServiceAccess
                .telemetry_credential(&queue.db, target)
                .is_none(),
            "{target:?}"
        );
    };

    for caller in [None, Some(inbox.as_str()), Some(another.as_str())] {
        refused(caller, &observed, "started by another caller");
    }
    let issued = issue(Some(&supervisor), &observed);
    assert_eq!(issued["ok"], true, "{issued}");
    let file: std::path::PathBuf =
        serde_json::from_value(issued["result"]["file"].clone()).unwrap();
    assert_eq!(
        service::lookup(queue.dir(), &service::read_token(&file).unwrap())
            .unwrap()
            .unwrap(),
        Principal::telemetry(&observed, None)
    );

    refused(
        None,
        &StepTarget::of(TargetKind::ThroughputReview, elsewhere),
        "started by another caller",
    );
    refused(
        None,
        &StepTarget::of(TargetKind::ThroughputReview, unnamed),
        "names no caller",
    );
}

/// The supervisor's `issue_telemetry` under no lease, which the job command
/// a supervisor starts as its own asks through, issues the token of a
/// target of no run only to the supervisor whose command started it
/// (ADR-t3102-1 decision 2): another live supervisor naming it is refused
/// and recorded, and takes nothing the starter is then refused over.
#[test]
fn a_supervisor_is_issued_no_telemetry_token_of_a_target_another_started() {
    use dagq::domain::{ActorRole, EventKind, LeaseToken};
    let queue = queue();
    start(&queue);
    let pid = std::process::id();
    let (lease, other) = (LeaseToken::new("it-starter"), LeaseToken::new("it-other"));
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    db.register_supervisor(&lease, pid, 1, "test").unwrap();
    // Another supervisor, of another process, still there.
    db.register_supervisor(&other, 1, 1, "test").unwrap();
    // Supervisor A's observe and throughput-review commands, this process.
    let db = db.with_actor(ActorContext::instance(ActorRole::Supervisor, pid));
    let observed = db
        .record_queue_event(
            EventKind::ObserveStarted,
            json!({"dir": "/o/1", "timeout_secs": 600, "pid": pid}),
        )
        .unwrap()
        .as_i64();
    let reviewed = db
        .record_queue_event(
            EventKind::ThroughputReviewStarted,
            json!({"dir": "/r/1", "timeout_secs": 600, "pid": pid}),
        )
        .unwrap()
        .as_i64();
    drop(db);
    let a = token(&queue, &Principal::supervisor(&lease, pid));
    let b = token(&queue, &Principal::supervisor(&other, 1));
    let issue = |token: &str, target: &StepTarget| {
        call(
            &queue,
            Some(token),
            UseCase::Supervisor,
            json!({"op": "issue_telemetry", "args": [target, null, null]}),
        )
    };
    for target in [
        StepTarget::of(TargetKind::Observer, observed),
        StepTarget::of(TargetKind::ThroughputReview, reviewed),
    ] {
        let before = events(&queue.db, "authorization_denied").len();
        let refused = issue(&b, &target);
        assert_eq!(
            refused["error"]["code"], "authorization_denied",
            "{refused}"
        );
        assert_eq!(
            refused["error"]["denied"]["reason"], "not on this resource",
            "{refused}"
        );
        let denied = events(&queue.db, "authorization_denied");
        assert_eq!(denied.len() - before, 1, "{refused}");
        let last = &denied[denied.len() - 1]["payload"];
        assert_eq!(last["capability"], "token.issue", "{last}");
        assert!(
            last["why"]
                .as_str()
                .unwrap()
                .contains("started by another caller"),
            "{last}"
        );
        assert!(
            SystemServiceAccess
                .telemetry_credential(&queue.db, &target)
                .is_none(),
            "{target:?}"
        );

        let issued = issue(&a, &target);
        assert_eq!(issued["ok"], true, "{issued}");
        let file: std::path::PathBuf = serde_json::from_value(issued["result"].clone()).unwrap();
        assert_eq!(
            service::lookup(queue.dir(), &service::read_token(&file).unwrap())
                .unwrap()
                .unwrap(),
            Principal::telemetry(&target, None)
        );
    }
}
