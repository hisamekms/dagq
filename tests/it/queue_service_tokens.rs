//! The tokens of the queue service on the host (docs/design/queue-service.md,
//! ADR-t3102-1): the service issues them at the request of their parent,
//! under the lease or registration they belong to, and revokes them with
//! the end of what they belong to, whichever binary left them.

use crate::common;
use crate::runtime_support::service::{Running, idle_queue, request_stop, supervisor_token};
use common::service::{call, events, queue, start, token};

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use common::cli::invoke;
use common::lifecycle::{FakeLaunchd, FakeProcesses};
use dagq::{
    domain::{
        ActorContext,
        queue_service::{API_VERSION, Principal, ServiceErrorCode, ServiceRequest, UseCase},
    },
    infrastructure::{queue_service as service, sqlite::SqliteQueue},
};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);

/// The issue use case on the service's socket (ADR-t3102-1 decision 2):
/// the user (no token, a peer of the service's own OS user) is issued the
/// supervisor's token of a registration and the inbox's of a session, and
/// that supervisor a planner's and its wrapper's, each written by the
/// service, not by the caller. Refused as `authorization_denied`, recorded
/// and leaving no token: a parent not of the table (a worker for a wrapper,
/// the supervisor for the inbox, the user for a worker), the integrator's
/// asked for by anyone (the landing's use case issues it), and what is not
/// held in the issue's transaction (a run another supervisor holds the
/// lease of, a closed planner, a registration not in the queue or
/// expired). A principal named as another is a bad request.
#[test]
fn tokens_are_issued_by_the_service_at_the_request_of_their_parent() {
    use dagq::domain::{LeaseToken, PlannerOrigin, queue_service::SessionProcess};
    let queue = queue();
    start(&queue);
    let (lease, gone) = (LeaseToken::new("it-supervisor"), LeaseToken::new("gone"));
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    db.register_supervisor(&lease, std::process::id(), 1, "test")
        .unwrap();
    let planner = db.open_planner(PlannerOrigin::Runtime, None).unwrap().id;
    let closed = db.open_planner(PlannerOrigin::Runtime, None).unwrap().id;
    db.close_planner(closed, None).unwrap();
    drop(db);
    let issue = |token: Option<&str>, principal: &Principal| {
        call(
            &queue,
            token,
            UseCase::IssueToken,
            json!({"principal": principal}),
        )
    };
    let issued_for = |answer: &Value| {
        assert_eq!(answer["ok"], true, "{answer}");
        let file: std::path::PathBuf =
            serde_json::from_value(answer["result"]["file"].clone()).unwrap();
        assert!(
            file.starts_with(service::service_dir(queue.dir())),
            "{file:?}"
        );
        let value = service::read_token(&file).unwrap();
        (
            service::lookup(queue.dir(), &value).unwrap().unwrap(),
            value,
        )
    };
    let supervisor = Principal::supervisor(&lease, std::process::id());
    let (principal, supervisor_token) = issued_for(&issue(None, &supervisor));
    assert_eq!(principal, supervisor);
    let inbox = Principal::inbox(SessionProcess {
        pid: std::process::id(),
        start: "now".into(),
    });
    assert_eq!(issued_for(&issue(None, &inbox)).0, inbox);
    let planner_target = |id: i64| {
        dagq::domain::session_step::StepTarget::of(
            dagq::domain::session_step::TargetKind::Planner,
            id,
        )
    };
    for child in [
        Principal::planner(planner.as_i64()),
        Principal::wrapper(&planner_target(planner.as_i64()), None),
    ] {
        assert_eq!(issued_for(&issue(Some(&supervisor_token), &child)).0, child);
    }

    let worker = Principal::worker(queue.run.id(), queue.run.task_id());
    let worker_token = token(&queue, &worker);
    // A supervisor whose registration is not in the queue.
    let gone_token = token(&queue, &Principal::supervisor(&gone, std::process::id()));
    let wrapper = Principal::wrapper(
        &dagq::domain::session_step::StepTarget::run(queue.run.id()),
        Some(queue.run.task_id()),
    );
    let integrator = Principal::integrator(queue.run.id(), &lease, std::process::id());
    let observer = Principal::of(&ActorContext::instance(
        dagq::domain::ActorRole::Observer,
        "s1",
    ));
    let not_held = "what it is issued under is not held";
    let refusals: Vec<(Option<&str>, &Principal, &str, Option<&str>)> = vec![
        (Some(worker_token.as_str()), &wrapper, "not granted", None),
        (
            Some(supervisor_token.as_str()),
            &inbox,
            "not on this resource",
            None,
        ),
        (None, &worker, "not on this resource", None),
        (None, &integrator, "not on this resource", None),
        (
            Some(supervisor_token.as_str()),
            &integrator,
            "not on this resource",
            None,
        ),
        // The run's lease is the claim's, not this supervisor's.
        (
            Some(supervisor_token.as_str()),
            &worker,
            not_held,
            Some("not leased"),
        ),
        (
            Some(gone_token.as_str()),
            &observer,
            not_held,
            Some("not registered"),
        ),
    ];
    let closed_planner = Principal::planner(closed.as_i64());
    let before = events(&queue.db, "authorization_denied").len();
    for (token, child, reason, why) in refusals.iter().copied().chain([(
        Some(supervisor_token.as_str()),
        &closed_planner,
        not_held,
        Some("not open"),
    )]) {
        let refused = issue(token, child);
        assert_eq!(
            refused["error"]["code"],
            json!(ServiceErrorCode::AuthorizationDenied.as_str()),
            "{child:?}: {refused}"
        );
        assert_eq!(refused["error"]["denied"]["reason"], reason, "{refused}");
        let last = events(&queue.db, "authorization_denied").pop().unwrap();
        assert_eq!(last["payload"]["capability"], "token.issue", "{last}");
        assert_eq!(
            last["payload"]["resource"],
            json!({"kind": "principal", "role": child.role.as_str()})
        );
        if let Some(why) = why {
            let recorded = last["payload"]["not_held"].as_str().unwrap();
            assert!(recorded.contains(why), "{recorded}");
        }
    }
    assert_eq!(
        events(&queue.db, "authorization_denied").len() - before,
        refusals.len() + 1
    );
    for child in [&wrapper, &integrator, &closed_planner] {
        assert!(
            !service::credential_path_of(queue.dir(), child).exists(),
            "{child:?}"
        );
    }
    // The registration's heartbeat expires: its supervisor is refused a
    // job's token, though its own sign-in (an exec's) is not.
    rusqlite::Connection::open(&queue.db)
        .unwrap()
        .execute(
            "UPDATE supervisors SET heartbeat_at = heartbeat_at - ?1 WHERE token = 'it-supervisor'",
            [dagq::domain::HEARTBEAT_TIMEOUT_SECS + 60],
        )
        .unwrap();
    let refused = issue(Some(&supervisor_token), &observer);
    assert_eq!(refused["error"]["denied"]["reason"], not_held, "{refused}");
    // A principal named as another is a bad request, and the other's token
    // stands.
    let mut named = observer.clone();
    named.actor_id = Principal::planner(planner.as_i64()).actor_id;
    let refused = issue(None, &named);
    assert_eq!(
        refused["error"]["code"],
        json!(ServiceErrorCode::BadRequest.as_str()),
        "{refused}"
    );
    assert!(
        service::credential_path_of(queue.dir(), &Principal::planner(planner.as_i64())).exists()
    );
}

/// A run's tokens under its lease (ADR-t3102-1 decision 2): the worker's,
/// its wrapper's and its executor's telemetry's are issued only to the
/// supervisor whose registration holds the run's lease and lives, both
/// found in the issue's transaction. Its registration expired or gone, or
/// another supervisor's lease, is refused as `authorization_denied`,
/// recorded with why, and leaves no token; once its heartbeat is written
/// again they are issued. No supervisor is issued another's sign-in.
#[test]
fn a_runs_tokens_are_issued_only_under_its_lease_by_a_live_registration() {
    use dagq::domain::LeaseToken;
    use dagq::domain::session_step::{StepTarget, TargetKind};
    let queue = queue();
    start(&queue);
    let (owner, other) = (LeaseToken::new("it-owner"), LeaseToken::new("it-other"));
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    for lease in [&owner, &other] {
        db.register_supervisor(lease, std::process::id(), 1, "test")
            .unwrap();
    }
    drop(db);
    let conn = rusqlite::Connection::open(&queue.db).unwrap();
    conn.execute(
        "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,?2,?3,unixepoch())
         ON CONFLICT(run_id) DO UPDATE SET token=excluded.token",
        rusqlite::params![queue.run.id().as_str(), owner.as_str(), std::process::id()],
    )
    .unwrap();
    let signed_in = |lease: &LeaseToken| {
        let answer = call(
            &queue,
            None,
            UseCase::IssueToken,
            json!({"principal": Principal::supervisor(lease, std::process::id())}),
        );
        let file: std::path::PathBuf =
            serde_json::from_value(answer["result"]["file"].clone()).unwrap();
        service::read_token(&file).unwrap()
    };
    let (owner_token, other_token) = (signed_in(&owner), signed_in(&other));
    let supervisor = |token: &str, op: &str, args: Value| {
        call(
            &queue,
            Some(token),
            UseCase::Supervisor,
            json!({"op": op, "args": args}),
        )
    };
    let worker = Principal::worker(queue.run.id(), queue.run.task_id());
    let wrapper = Principal::wrapper(&StepTarget::run(queue.run.id()), Some(queue.run.task_id()));
    let target = StepTarget::run(queue.run.id());
    let telemetry = Principal::telemetry(&target, Some(queue.run.task_id()));
    let issues = |token: &str, lease: &LeaseToken| {
        [
            supervisor(token, "issue_token", json!([worker])),
            supervisor(token, "issue_token", json!([wrapper])),
            supervisor(
                token,
                "issue_telemetry",
                json!([target, queue.run.task_id(), lease]),
            ),
        ]
    };
    let refused_all = |token: &str, lease: &LeaseToken, why: &str| {
        let before = events(&queue.db, "authorization_denied").len();
        for refused in issues(token, lease) {
            assert_eq!(
                refused["error"]["code"], "authorization_denied",
                "{refused}"
            );
        }
        let denials = events(&queue.db, "authorization_denied");
        assert_eq!(denials.len() - before, 3);
        for denial in &denials[before..] {
            assert_eq!(denial["payload"]["capability"], "token.issue", "{denial}");
            let recorded = denial["payload"]["not_held"].as_str().unwrap();
            assert!(recorded.contains(why), "{recorded}");
        }
        for child in [&worker, &wrapper, &telemetry] {
            assert!(
                !service::credential_path_of(queue.dir(), child).exists(),
                "{child:?}"
            );
        }
    };
    // Another supervisor's lease.
    refused_all(&other_token, &other, "not leased");
    // Its lease named by another supervisor: refused before the issue, and
    // recorded.
    let before = events(&queue.db, "authorization_denied").len();
    let named = supervisor(
        &other_token,
        "issue_telemetry",
        json!([target, queue.run.task_id(), owner]),
    );
    assert_eq!(named["error"]["code"], "authorization_denied", "{named}");
    let denials = events(&queue.db, "authorization_denied");
    assert_eq!(denials.len() - before, 1);
    assert!(
        denials[denials.len() - 1]["payload"]["not_held"]
            .as_str()
            .unwrap()
            .contains("not the supervisor principal's registration"),
        "{named}"
    );
    assert!(!service::credential_path_of(queue.dir(), &telemetry).exists());
    // The owner's registration expired, then gone.
    let heartbeat = |at: &str| {
        conn.execute(
            &format!("UPDATE supervisors SET heartbeat_at = {at} WHERE token = ?1"),
            [owner.as_str()],
        )
        .unwrap();
    };
    heartbeat(&format!(
        "unixepoch() - {}",
        dagq::domain::HEARTBEAT_TIMEOUT_SECS + 60
    ));
    refused_all(&owner_token, &owner, "expired");
    // The executor's telemetry of a target of no run, under the registration
    // it names: refused while it is expired, issued once it lives.
    let planner = StepTarget::of(TargetKind::Planner, 1);
    let planners = || {
        supervisor(
            &owner_token,
            "issue_telemetry",
            json!([planner, null, owner]),
        )
    };
    let expired = planners();
    assert_eq!(
        expired["error"]["code"], "authorization_denied",
        "{expired}"
    );
    let denial = events(&queue.db, "authorization_denied").pop().unwrap();
    assert!(
        denial["payload"]["not_held"]
            .as_str()
            .unwrap()
            .contains("expired"),
        "{denial}"
    );
    heartbeat("unixepoch()");
    for issued in issues(&owner_token, &owner) {
        assert_eq!(issued["ok"], true, "{issued}");
    }
    let issued = planners();
    assert_eq!(issued["ok"], true, "{issued}");
    for child in [&worker, &wrapper, &telemetry] {
        let file = service::credential_path_of(queue.dir(), child);
        let value = service::read_token(&file).unwrap();
        assert_eq!(
            service::lookup(queue.dir(), &value).unwrap().as_ref(),
            Some(child)
        );
    }
    // A run's executor's token is never issued under no lease.
    let before = events(&queue.db, "authorization_denied").len();
    let leaseless = supervisor(
        &owner_token,
        "issue_telemetry",
        json!([target, queue.run.task_id(), null]),
    );
    assert_eq!(
        leaseless["error"]["code"], "authorization_denied",
        "{leaseless}"
    );
    assert_eq!(events(&queue.db, "authorization_denied").len() - before, 1);
    // Nor a sign-in of another's registration, which would write under its
    // leases.
    let another = supervisor(
        &owner_token,
        "issue_token",
        json!([Principal::supervisor(&other, std::process::id())]),
    );
    assert_eq!(
        another["error"]["code"], "authorization_denied",
        "{another}"
    );
    conn.execute("DELETE FROM supervisors WHERE token = ?1", [owner.as_str()])
        .unwrap();
    let gone = supervisor(&owner_token, "issue_token", json!([worker]));
    assert_eq!(gone["error"]["code"], "authorization_denied", "{gone}");
    let denial = events(&queue.db, "authorization_denied").pop().unwrap();
    assert!(
        denial["payload"]["not_held"]
            .as_str()
            .unwrap()
            .contains("not registered"),
        "{denial}"
    );
}

/// The executor's telemetry of a job a command starts (`observe`,
/// `throughput-review`; ADR-t3102-1 decision 2): its token is issued by the
/// service, under its own lock, at the request of the command's caller,
/// the user (no token) or the supervisor (its registration live), while its
/// target is open and holds no token yet. Refused as
/// `authorization_denied`, recorded and leaving no token: a target that
/// ended, one of a run or of a job no command starts (the user's), a
/// caller that starts no executor (a worker), a target another caller's
/// command started, and a target whose executor holds its token already
/// (its job's, which the issue would revoke).
#[test]
fn a_commands_job_telemetry_token_is_issued_at_its_callers_request() {
    use dagq::domain::session_step::{StepTarget, TargetKind};
    use dagq::domain::{EventKind, LeaseToken};
    let queue = queue();
    start(&queue);
    let lease = LeaseToken::new("it-observing");
    let pid = std::process::id();
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    db.register_supervisor(&lease, pid, 1, "test").unwrap();
    // Each started by a command of this process, as its caller.
    let observation = |db: SqliteQueue, by: ActorContext, dir: &str| {
        let db = db.with_actor(by);
        let id = db
            .record_queue_event(
                EventKind::ObserveStarted,
                json!({"dir": dir, "timeout_secs": 600, "pid": pid}),
            )
            .unwrap();
        (db, StepTarget::of(TargetKind::Observer, id.as_i64()))
    };
    let (db, open) = observation(db, ActorContext::user(), "/o/1");
    let (db, by_supervisor) = observation(
        db,
        ActorContext::instance(dagq::domain::ActorRole::Supervisor, pid),
        "/o/2",
    );
    let (db, ended) = observation(db, ActorContext::user(), "/o/3");
    db.record_queue_event(EventKind::ObserveFinished, json!({"dir": "/o/3"}))
        .unwrap();
    drop(db);
    let issue = |token: Option<&str>, principal: &Principal| {
        call(
            &queue,
            token,
            UseCase::IssueToken,
            json!({"principal": principal}),
        )
    };
    let issued = |answer: &Value| {
        assert_eq!(answer["ok"], true, "{answer}");
        let file: std::path::PathBuf =
            serde_json::from_value(answer["result"]["file"].clone()).unwrap();
        assert!(
            file.starts_with(service::service_dir(queue.dir())),
            "{file:?}"
        );
        service::lookup(queue.dir(), &service::read_token(&file).unwrap())
            .unwrap()
            .unwrap()
    };
    let telemetry = |target: &StepTarget| Principal::telemetry(target, None);
    // The user's command's.
    assert_eq!(issued(&issue(None, &telemetry(&open))), telemetry(&open));
    // The supervisor's command's.
    let supervisor = token(&queue, &Principal::supervisor(&lease, std::process::id()));
    assert_eq!(
        issued(&issue(Some(&supervisor), &telemetry(&by_supervisor))),
        telemetry(&by_supervisor)
    );

    let worker = token(
        &queue,
        &Principal::worker(queue.run.id(), queue.run.task_id()),
    );
    let of_run = Principal::telemetry(&StepTarget::run(queue.run.id()), Some(queue.run.task_id()));
    let plan_review = telemetry(&StepTarget::of(TargetKind::PlanReview, 1));
    let refusals: Vec<(Option<&str>, Principal, &str, &str)> = vec![
        (
            None,
            telemetry(&ended),
            "what it is issued under is not held",
            "has ended",
        ),
        (None, of_run, "not on this resource", "lease"),
        (
            None,
            plan_review,
            "not on this resource",
            "not of a job a command starts",
        ),
        (Some(worker.as_str()), telemetry(&open), "not granted", ""),
        // A command of another caller's started it.
        (
            None,
            telemetry(&by_supervisor),
            "not on this resource",
            "started by another caller",
        ),
        // The job's executor holds it: its own caller asking again does
        // not take it over.
        (
            Some(supervisor.as_str()),
            telemetry(&by_supervisor),
            "not on this resource",
            "holds its token already",
        ),
    ];
    for (token, child, reason, why) in &refusals {
        let before = events(&queue.db, "authorization_denied").len();
        let refused = issue(*token, child);
        assert_eq!(
            refused["error"]["code"],
            json!(ServiceErrorCode::AuthorizationDenied.as_str()),
            "{child:?}: {refused}"
        );
        assert_eq!(refused["error"]["denied"]["reason"], *reason, "{refused}");
        let denied = events(&queue.db, "authorization_denied");
        assert_eq!(denied.len() - before, 1, "{child:?}");
        let last = &denied[denied.len() - 1]["payload"];
        assert_eq!(last["capability"], "token.issue", "{last}");
        assert_eq!(
            last["resource"],
            json!({"kind": "principal", "role": "executor-telemetry"})
        );
        let recorded = last["not_held"]
            .as_str()
            .or(last["why"].as_str())
            .unwrap_or("");
        assert!(recorded.contains(why), "{recorded}");
    }
    assert!(
        dagq::application::queue_service::TelemetryTokens::telemetry_credential(
            &dagq::infrastructure::queue_service::SystemServiceAccess,
            &queue.db,
            &ended,
        )
        .is_none()
    );
}

/// Two requests at once of the caller whose command started a job for
/// the token of that job's executor, which holds none yet (a caller that
/// did not start it is refused before): the service looks at what the target
/// holds in the transaction that issues it, so one of them is issued and
/// the other is refused as `authorization_denied` and recorded, and the
/// token issued is the one that holds (no later issue revoked it).
#[test]
fn of_two_callers_asking_at_once_for_a_jobs_executor_token_one_is_issued() {
    use dagq::domain::EventKind;
    use dagq::domain::session_step::{StepTarget, TargetKind};
    let queue = queue();
    start(&queue);
    let db = SqliteQueue::open(&queue.db)
        .unwrap()
        .with_actor(ActorContext::user());
    for round in 0..5 {
        let id = db
            .record_queue_event(
                EventKind::ObserveStarted,
                json!({"dir": format!("/o/{round}"), "timeout_secs": 600, "pid": std::process::id()}),
            )
            .unwrap();
        let telemetry =
            Principal::telemetry(&StepTarget::of(TargetKind::Observer, id.as_i64()), None);
        let before = events(&queue.db, "authorization_denied").len();
        let barrier = std::sync::Barrier::new(2);
        // The barrier and the joins end within the limit however a caller
        // ends, one that never reaches the barrier included.
        let _waiting = common::within(
            common::STEP_LIMIT,
            "the two callers asking at once to be answered",
        );
        let answers: Vec<Value> = std::thread::scope(|scope| {
            let asks: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        call(
                            &queue,
                            None,
                            UseCase::IssueToken,
                            json!({"principal": telemetry}),
                        )
                    })
                })
                .collect();
            asks.into_iter().map(|ask| ask.join().unwrap()).collect()
        });
        let issued: Vec<&Value> = answers
            .iter()
            .filter(|answer| answer["ok"] == true)
            .collect();
        assert_eq!(issued.len(), 1, "round {round}: {answers:?}");
        let refused = answers.iter().find(|answer| answer["ok"] != true).unwrap();
        assert_eq!(
            refused["error"]["code"], "authorization_denied",
            "{refused}"
        );
        let denied = events(&queue.db, "authorization_denied");
        assert_eq!(denied.len() - before, 1, "round {round}");
        assert!(
            denied[denied.len() - 1]["payload"]["why"]
                .as_str()
                .unwrap()
                .contains("holds its token already")
        );
        let file: std::path::PathBuf =
            serde_json::from_value(issued[0]["result"]["file"].clone()).unwrap();
        assert_eq!(
            service::lookup(queue.dir(), &service::read_token(&file).unwrap())
                .unwrap()
                .as_ref(),
            Some(&telemetry),
            "round {round}"
        );
    }
}

/// The landing slot and the integrator's token (ADR-t3102-1 decision 2):
/// the supervisor that holds a run's lease takes its slot through the
/// service's `begin_landing`, which issues the integrator's token in the
/// same transaction, under the lease and its registration live. A
/// supervisor whose run another holds the lease of, that names a lease not
/// its registration, or whose registration expired or is not in the queue,
/// is refused as `authorization_denied`, recorded, and takes no slot and
/// leaves no token. The token is kept with the supervisor that asked, the
/// generic issue use case issues it to no one, and `end_landing` revokes
/// it.
#[test]
fn the_landing_slot_issues_the_integrators_token_under_its_lease() {
    use dagq::domain::{LeaseToken, RunStatus};
    let queue = queue();
    start(&queue);
    let (lease, other) = (LeaseToken::new("it-lander"), LeaseToken::new("it-other"));
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    for token in [&lease, &other] {
        db.register_supervisor(token, std::process::id(), 1, "test")
            .unwrap();
    }
    drop(db);
    let conn = rusqlite::Connection::open(&queue.db).unwrap();
    conn.execute(
        "UPDATE task_runs SET status='awaiting_integration' WHERE id=?1",
        [queue.run.id().as_str()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,?2,?3,unixepoch())
         ON CONFLICT(run_id) DO UPDATE SET token=excluded.token",
        rusqlite::params![queue.run.id().as_str(), lease.as_str(), std::process::id()],
    )
    .unwrap();
    let sign_in = |lease: &LeaseToken| {
        let answer = call(
            &queue,
            None,
            UseCase::IssueToken,
            json!({"principal": Principal::supervisor(lease, std::process::id())}),
        );
        let file: std::path::PathBuf =
            serde_json::from_value(answer["result"]["file"].clone()).unwrap();
        service::read_token(&file).unwrap()
    };
    let supervisor = |token: &str, op: &str, args: Value| {
        call(
            &queue,
            Some(token),
            UseCase::Supervisor,
            json!({"op": op, "args": args}),
        )
    };
    let main = "a".repeat(40);
    let integrator = Principal::integrator(queue.run.id(), &lease, std::process::id());
    let file = service::credential_path_of(queue.dir(), &integrator);
    let begin = |token: &str, lease: &LeaseToken| {
        supervisor(
            token,
            "begin_landing",
            json!([queue.run.id(), lease, main, false]),
        )
    };

    let denied = || events(&queue.db, "authorization_denied").len();
    let last_denial = || events(&queue.db, "authorization_denied").pop().unwrap();
    let status = |queue: &crate::common::service::Queue| {
        SqliteQueue::open(&queue.db)
            .unwrap()
            .run(queue.run.id())
            .unwrap()
            .status()
    };

    let other_token = sign_in(&other);
    // The run's lease is another supervisor's: found in the slot's
    // transaction, refused and recorded.
    let before = denied();
    let refused = begin(&other_token, &other);
    assert_eq!(
        refused["error"]["code"], "authorization_denied",
        "{refused}"
    );
    assert_eq!(denied() - before, 1);
    let denial = last_denial();
    assert_eq!(denial["payload"]["capability"], "token.issue", "{denial}");
    assert_eq!(
        denial["payload"]["resource"],
        json!({"kind": "principal", "role": "integrator"})
    );
    assert!(
        denial["payload"]["not_held"]
            .as_str()
            .unwrap()
            .contains("still leased"),
        "{denial}"
    );
    assert!(!file.exists());
    // Another's lease named: refused before the slot, and recorded.
    let before = denied();
    let named = begin(&other_token, &lease);
    assert_eq!(named["error"]["code"], "authorization_denied", "{named}");
    assert_eq!(denied() - before, 1);
    let denial = last_denial();
    assert_eq!(
        denial["payload"]["capability"], "landing.request",
        "{denial}"
    );
    assert!(
        denial["payload"]["not_held"]
            .as_str()
            .unwrap()
            .contains("not the supervisor principal's registration"),
        "{denial}"
    );
    assert!(!file.exists());
    assert_eq!(status(&queue), RunStatus::AwaitingIntegration);

    let lander = sign_in(&lease);
    // Its registration expired, or not in the queue: no slot and no token,
    // the refusal recorded.
    let gone = LeaseToken::new("it-gone");
    let gone_token = sign_in(&gone);
    conn.execute(
        "UPDATE supervisors SET heartbeat_at = heartbeat_at - ?2 WHERE token = ?1",
        rusqlite::params![lease.as_str(), dagq::domain::HEARTBEAT_TIMEOUT_SECS + 60],
    )
    .unwrap();
    for (token, lease, why) in [
        (&lander, &lease, "expired"),
        (&gone_token, &gone, "not registered"),
    ] {
        let before = denied();
        let refused = begin(token, lease);
        assert_eq!(
            refused["error"]["code"], "authorization_denied",
            "{refused}"
        );
        assert_eq!(denied() - before, 1);
        let denial = last_denial();
        assert_eq!(
            denial["payload"]["resource"],
            json!({"kind": "principal", "role": "integrator"})
        );
        assert!(
            denial["payload"]["not_held"]
                .as_str()
                .unwrap()
                .contains(why),
            "{denial}"
        );
        assert!(!file.exists());
        assert_eq!(status(&queue), RunStatus::AwaitingIntegration);
    }
    conn.execute(
        "UPDATE supervisors SET heartbeat_at = unixepoch() WHERE token = ?1",
        [lease.as_str()],
    )
    .unwrap();
    drop(conn);
    let begun = begin(&lander, &lease);
    assert_eq!(begun["ok"], true, "{begun}");
    assert_eq!(status(&queue), RunStatus::Integrating);
    let issued: std::path::PathBuf =
        serde_json::from_value(begun["result"]["integrator"].clone()).unwrap();
    assert_eq!(issued, file);
    let token = service::read_token(&file).unwrap();
    let started = events(&queue.db, "integration_started").pop().unwrap();
    assert_eq!(
        service::lookup(queue.dir(), &token).unwrap(),
        Some(
            integrator
                .clone()
                .of_landing(dagq::domain::EventId::new(started["id"].as_i64().unwrap()))
        )
    );
    // Kept with the supervisor that asked.
    let kept = dagq::infrastructure::queue_service::held(queue.dir(), &integrator.credential_key())
        .unwrap()
        .unwrap();
    assert_eq!(
        kept.issued_by.map(|issuer| issuer.parent),
        Some(Principal::supervisor(&lease, std::process::id()).credential_key())
    );
    // No requester is issued it by the generic use case.
    let generic = call(
        &queue,
        Some(&lander),
        UseCase::IssueToken,
        json!({"principal": integrator}),
    );
    assert_eq!(
        generic["error"]["code"], "authorization_denied",
        "{generic}"
    );
    // The landing's end revokes it.
    let ended = supervisor(&lander, "end_landing", json!([queue.run.id()]));
    assert_eq!(ended["result"], true, "{ended}");
    assert!(!file.exists());
    assert_eq!(service::lookup(queue.dir(), &token).unwrap(), None);
}

/// A job's token (ADR-t3102-1 decision 2) is replaced only at the request
/// of the parent that asked for it, once the process that asked, which
/// the job ran under, has ended. The inbox, the user and another
/// supervisor asking for the token of a job a supervisor started, whose
/// process runs, are refused as `authorization_denied`, recorded, and the
/// job's token still holds; the other supervisor may not revoke it
/// either. Its own supervisor asking again is handed the same token. Once
/// that process ended, another parent is still refused, its supervisor
/// replaces it, and the sweep revokes a left one but never a running
/// job's.
#[test]
fn the_inbox_cannot_take_the_token_of_a_job_another_parent_started() {
    use dagq::domain::LeaseToken;
    use dagq::domain::queue_service::{SessionProcess, TokenIssuer};
    let queue = queue();
    start(&queue);
    let pid = std::process::id();
    let (lease, other) = (LeaseToken::new("it-jobs"), LeaseToken::new("it-other"));
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    for lease in [&lease, &other] {
        db.register_supervisor(lease, pid, 1, "test").unwrap();
    }
    drop(db);
    let sign_in = |lease: &LeaseToken| {
        let answer = call(
            &queue,
            None,
            UseCase::IssueToken,
            json!({"principal": Principal::supervisor(lease, pid)}),
        );
        let file: std::path::PathBuf =
            serde_json::from_value(answer["result"]["file"].clone()).unwrap();
        service::read_token(&file).unwrap()
    };
    let (supervisor, another) = (sign_in(&lease), sign_in(&other));
    let inbox = token(
        &queue,
        &Principal::inbox(SessionProcess {
            pid,
            start: dagq::infrastructure::adapters::session_start(pid).unwrap(),
        }),
    );
    let op = |token: &str, op: &str, args: Value| {
        call(
            &queue,
            Some(token),
            UseCase::Supervisor,
            json!({"op": op, "args": args}),
        )
    };
    let asked = |token: Option<&str>, job: &Principal| {
        call(
            &queue,
            token,
            UseCase::IssueToken,
            json!({"principal": job}),
        )
    };
    let value_of = |job: &Principal| {
        service::read_token(&service::credential_path_of(queue.dir(), job)).unwrap()
    };
    let refused = |answer: &Value, why: &str| {
        assert_eq!(answer["error"]["code"], "authorization_denied", "{answer}");
        let denial = events(&queue.db, "authorization_denied").pop().unwrap();
        assert_eq!(denial["payload"]["capability"], "token.issue", "{denial}");
        assert!(
            denial["payload"]["why"].as_str().unwrap().contains(why),
            "{denial}"
        );
    };

    // The supervisor's job, its process (this one, which asked) running.
    let job = Principal::of(&ActorContext::throughput_review_job("hourly", "it"));
    let first = op(&supervisor, "issue_token", json!([job]));
    assert_eq!(first["ok"], true, "{first}");
    assert_eq!(first["result"]["held"], false, "{first}");
    let value = value_of(&job);
    for who in [
        "inbox",
        "user",
        "another supervisor",
        "another supervisor's revocation",
    ] {
        let before = events(&queue.db, "authorization_denied").len();
        let answer = match who {
            "inbox" => asked(Some(&inbox), &job),
            "user" => asked(None, &job),
            "another supervisor" => op(&another, "issue_token", json!([job])),
            _ => op(&another, "revoke_token", json!([job.actor_id])),
        };
        refused(&answer, "another parent's");
        assert_eq!(
            events(&queue.db, "authorization_denied").len() - before,
            1,
            "{who}"
        );
        assert_eq!(value_of(&job), value, "{who}");
        assert_eq!(
            service::lookup(queue.dir(), &value).unwrap(),
            Some(job.clone()),
            "{who}"
        );
    }
    // Its own supervisor is handed the same token again, not a new one,
    // and told so: a start it was handed to gives back nothing.
    let again = op(&supervisor, "issue_token", json!([job]));
    assert_eq!(again["ok"], true, "{again}");
    assert_eq!(again["result"]["held"], true, "{again}");
    assert_eq!(again["result"]["file"], first["result"]["file"], "{again}");
    assert_eq!(value_of(&job), value);
    // Not swept while its process runs.
    assert_eq!(
        op(&supervisor, "revoke_principal", json!([job]))["result"],
        false
    );
    assert_eq!(
        service::lookup(queue.dir(), &value).unwrap(),
        Some(job.clone())
    );

    // A job whose process that asked has ended: its parent alone replaces
    // it, and a left one is swept.
    let gone = TokenIssuer::of(
        &Principal::supervisor(&lease, pid),
        Some(SessionProcess {
            pid,
            start: "a process ended".into(),
        }),
    );
    let left = Principal::of(&ActorContext::throughput_review_job("hourly", "left"));
    service::issue_by(queue.dir(), &left, &gone, 1).unwrap();
    let old = value_of(&left);
    refused(&asked(Some(&inbox), &left), "another parent's");
    assert_eq!(
        service::lookup(queue.dir(), &old).unwrap(),
        Some(left.clone())
    );
    assert_eq!(op(&supervisor, "issue_token", json!([left]))["ok"], true);
    assert_eq!(service::lookup(queue.dir(), &old).unwrap(), None);
    let swept = Principal::of(&ActorContext::throughput_review_job("hourly", "swept"));
    service::issue_by(queue.dir(), &swept, &gone, 1).unwrap();
    assert_eq!(
        op(&another, "revoke_principal", json!([swept]))["result"],
        true
    );
    assert!(!service::credential_path_of(queue.dir(), &swept).exists());
}

/// A job's token whose parent cannot be judged (ADR-t3102-1 decision 2): one
/// an older binary left with no parent kept, as a throughput review whose
/// command fell before its finish leaves, is refused over while the queue
/// tells that job may still run (its command's process runs), recorded,
/// and neither replaced, revoked on request nor swept. Once that process
/// is gone, the period's review started again (its own start recorded
/// first, under its own process) is issued a new token in its place, and
/// the sweep takes a left one. A run's job (a recovery job) is told by its
/// `headless_jobs` row; one the inbox asked for from a process the service
/// could not read is swept once its job's finish is recorded; a
/// supervisor's whose process was not read, once that supervisor's
/// registration went.
#[test]
fn a_job_token_an_older_binary_left_goes_once_the_queue_tells_its_job_ended() {
    use dagq::application::{HeadlessJobStore, NewHeadlessJob};
    use dagq::domain::{
        EventKind, LeaseToken, RunId,
        queue_service::{SessionProcess, TokenIssuer},
    };
    let queue = queue();
    start(&queue);
    let pid = std::process::id();
    let (lease, other) = (LeaseToken::new("it-runs"), LeaseToken::new("it-other"));
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    for lease in [&lease, &other] {
        db.register_supervisor(lease, pid, 1, "test").unwrap();
    }
    let db = db;
    let supervisor = token(&queue, &Principal::supervisor(&lease, pid));
    let op = |op: &str, args: Value| {
        call(
            &queue,
            Some(&supervisor),
            UseCase::Supervisor,
            json!({"op": op, "args": args}),
        )
    };
    let value_of = |job: &Principal| {
        service::read_token(&service::credential_path_of(queue.dir(), job)).unwrap()
    };
    let refused = |answer: &Value| {
        assert_eq!(answer["error"]["code"], "authorization_denied", "{answer}");
        let denial = events(&queue.db, "authorization_denied").pop().unwrap();
        assert!(
            denial["payload"]["why"]
                .as_str()
                .unwrap()
                .contains("no parent that can be judged"),
            "{denial}"
        );
    };
    // The start of a review of `period` by the command of process `pid`.
    let started = |period: &str, pid: u32| {
        db.record_queue_event(
            EventKind::ThroughputReviewStarted,
            json!({"mode": "hourly", "period": period, "dir": format!("/reviews/{period}/{pid}"),
                   "pid": pid, "timeout_secs": 3600}),
        )
        .unwrap();
    };
    let job = |period: &str| Principal::of(&ActorContext::throughput_review_job("hourly", period));

    // An older binary's review of period 10, whose command still runs.
    let mut command = common::KillOnDrop::new(
        std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap(),
        "the older binary's review command",
    );
    let gone = command.child().id();
    let ten = job("10");
    service::issue(queue.dir(), &ten, 1).unwrap();
    started("10", gone);
    let left = value_of(&ten);
    let before = events(&queue.db, "authorization_denied").len();
    refused(&op("issue_token", json!([ten])));
    assert_eq!(events(&queue.db, "authorization_denied").len() - before, 1);
    // Nor revoked on request, which would let the issue take it over.
    refused(&op("revoke_token", json!([ten.actor_id])));
    assert_eq!(events(&queue.db, "authorization_denied").len() - before, 2);
    assert_eq!(op("revoke_principal", json!([ten]))["result"], false);
    assert_eq!(value_of(&ten), left);
    refused(&op("issue_token", json!([ten])));
    assert_eq!(value_of(&ten), left);
    // Its command is gone with no finish; the period is reviewed again,
    // whose start, recorded first, names the process that asks.
    drop(command);
    started("10", pid);
    let issued = op("issue_token", json!([ten]));
    assert_eq!(issued["ok"], true, "{issued}");
    assert_ne!(value_of(&ten), left);
    assert_eq!(service::lookup(queue.dir(), &left).unwrap(), None);
    // One left by a command gone is swept.
    let eleven = job("11");
    service::issue(queue.dir(), &eleven, 1).unwrap();
    started("11", gone);
    let value = value_of(&eleven);
    assert_eq!(op("revoke_principal", json!([eleven]))["result"], true);
    assert_eq!(service::lookup(queue.dir(), &value).unwrap(), None);

    // A run's recovery job, by its row, which a live supervisor runs.
    let run = RunId::new("it-run").unwrap();
    let recovery = Principal::of(&ActorContext::recovery_job(&run, "idle", 1));
    service::issue(queue.dir(), &recovery, 1).unwrap();
    let row = db
        .record_headless_job(&NewHeadlessJob {
            kind: dagq::domain::headless_job::RECOVERY,
            label: Some("idle".into()),
            run_id: Some(run.clone()),
            proposal_id: None,
            goal_id: None,
            attempt: 1,
            provider: None,
            pid,
            process_start: None,
            supervisor_token: other.clone(),
            stdout: None,
        })
        .unwrap();
    refused(&op("issue_token", json!([recovery])));
    db.end_headless_job(row, "ended").unwrap();
    assert_eq!(op("issue_token", json!([recovery]))["ok"], true);

    // The inbox's, asked for from a process the service could not read.
    let unread = job("12");
    let inbox = Principal::inbox(SessionProcess {
        pid,
        start: dagq::infrastructure::adapters::session_start(pid).unwrap(),
    });
    service::issue_by(queue.dir(), &unread, &TokenIssuer::of(&inbox, None), 1).unwrap();
    started("12", pid);
    assert_eq!(op("revoke_principal", json!([unread]))["result"], false);
    db.record_queue_event(
        EventKind::ThroughputReviewFinished,
        json!({"mode": "hourly", "period": "12", "dir": format!("/reviews/12/{pid}")}),
    )
    .unwrap();
    assert_eq!(op("revoke_principal", json!([unread]))["result"], true);

    // A supervisor's whose process was not read, by its registration.
    let registered = job("13");
    service::issue_by(
        queue.dir(),
        &registered,
        &TokenIssuer::of(&Principal::supervisor(&other, pid), None),
        1,
    )
    .unwrap();
    assert_eq!(op("revoke_principal", json!([registered]))["result"], false);
    db.deregister_supervisor(&other).unwrap();
    assert_eq!(op("revoke_principal", json!([registered]))["result"], true);
}

/// A job's token an older binary left with no parent kept, whose job the
/// queue keeps no record of (ADR-t3102-1 decision 2): a recovery job's
/// whose supervisor stopped after its token and before its `headless_jobs`
/// row. While the supervisor registered by its issue runs, another
/// supervisor's issue over it and its revocation are refused and recorded,
/// it is not swept, and it holds. Once that process ended, the same
/// actor's start is issued a new token in its place, and one left so is
/// swept. The one supervisor that alone may have asked for one is handed
/// it again. A setup review's, of which the queue keeps no record, goes
/// once no `dagq setup-review` that started by its issue runs, whatever
/// supervisor runs.
#[test]
fn a_job_token_an_older_binary_left_with_no_record_goes_once_no_process_that_may_have_asked_runs() {
    use dagq::domain::{LeaseToken, RunId};
    let queue = queue();
    start(&queue);
    let pid = std::process::id();
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    let mut stopped = common::KillOnDrop::new(
        std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap(),
        "the process of the supervisor that asked",
    );
    let (old, new) = (LeaseToken::new("it-old"), LeaseToken::new("it-new"));
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    db.register_supervisor(&old, stopped.child().id(), 1, "test")
        .unwrap();
    db.register_supervisor(&new, pid, 1, "test").unwrap();
    drop(db);
    // The old one registered before the tokens, the new one after.
    rusqlite::Connection::open(&queue.db)
        .unwrap()
        .execute(
            "UPDATE supervisors SET started_at = ?1 WHERE token = ?2",
            rusqlite::params![now - 200, old.as_str()],
        )
        .unwrap();
    let supervisor = token(&queue, &Principal::supervisor(&new, pid));
    let op = |op: &str, args: Value| {
        call(
            &queue,
            Some(&supervisor),
            UseCase::Supervisor,
            json!({"op": op, "args": args}),
        )
    };
    let value_of = |job: &Principal| {
        service::read_token(&service::credential_path_of(queue.dir(), job)).unwrap()
    };
    let denials = || events(&queue.db, "authorization_denied");
    let refused = |answer: &Value| {
        assert_eq!(answer["error"]["code"], "authorization_denied", "{answer}");
        let denial = denials().pop().unwrap();
        assert!(
            denial["payload"]["why"]
                .as_str()
                .unwrap()
                .contains("no record of its job"),
            "{denial}"
        );
    };
    let run = RunId::new("it-run").unwrap();
    let job = |attempt| Principal::of(&ActorContext::recovery_job(&run, "idle", attempt));
    let (first, second) = (job(1), job(2));
    for job in [&first, &second] {
        service::issue(queue.dir(), job, now - 100).unwrap();
    }
    let left = value_of(&first);
    let before = denials().len();
    refused(&op("issue_token", json!([first])));
    refused(&op("revoke_token", json!([first.actor_id])));
    assert_eq!(denials().len() - before, 2);
    assert_eq!(op("revoke_principal", json!([second]))["result"], false);
    assert_eq!(value_of(&first), left);
    assert!(service::credential_path_of(queue.dir(), &second).exists());

    // The process of the supervisor that asked ended.
    drop(stopped);
    let issued = op("issue_token", json!([first]));
    assert_eq!(issued["ok"], true, "{issued}");
    assert_ne!(value_of(&first), left);
    assert_eq!(service::lookup(queue.dir(), &left).unwrap(), None);
    assert_eq!(op("revoke_principal", json!([second]))["result"], true);
    assert!(!service::credential_path_of(queue.dir(), &second).exists());

    // One issued since the new one registered, which alone may have asked
    // for it: handed the same token again.
    let third = job(3);
    service::issue(queue.dir(), &third, now + 100).unwrap();
    let held = value_of(&third);
    let again = op("issue_token", json!([third]));
    assert_eq!(again["ok"], true, "{again}");
    assert_eq!(value_of(&third), held);

    // A setup review's, issued before any `dagq` that runs started, which
    // a person starts again. A live supervisor registered before it does
    // not keep it: only its command starts a setup review.
    let ancient = LeaseToken::new("it-ancient");
    SqliteQueue::open(&queue.db)
        .unwrap()
        .register_supervisor(&ancient, pid, 1, "test")
        .unwrap();
    rusqlite::Connection::open(&queue.db)
        .unwrap()
        .execute(
            "UPDATE supervisors SET started_at = 0 WHERE token = ?1",
            rusqlite::params![ancient.as_str()],
        )
        .unwrap();
    let setup = Principal::of(&ActorContext::setup_review_job("it-place"));
    service::issue(queue.dir(), &setup, 1).unwrap();
    let left = value_of(&setup);
    let issued = call(
        &queue,
        None,
        UseCase::IssueToken,
        json!({"principal": setup}),
    );
    assert_eq!(issued["ok"], true, "{issued}");
    assert_ne!(value_of(&setup), left);
}

/// The executor's telemetry of a target of no run (a plan review's), which
/// a supervisor asks for under its registration (ADR-t3102-1 decision 2), is
/// given back only by that supervisor while it is there: another
/// supervisor's revocation is refused as `authorization_denied` and
/// recorded, whether it names the first one's registration as the lease or
/// no lease, and the token still holds. Once the first's registration
/// went, it is gone though its process runs on.
#[test]
fn another_supervisor_revokes_no_live_supervisors_telemetry_of_a_target_of_no_run() {
    use dagq::domain::LeaseToken;
    use dagq::domain::session_step::{StepTarget, TargetKind};
    let queue = queue();
    start(&queue);
    let pid = std::process::id();
    let (first, second) = (LeaseToken::new("it-first"), LeaseToken::new("it-second"));
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    for lease in [&first, &second] {
        db.register_supervisor(lease, pid, 1, "test").unwrap();
    }
    drop(db);
    let (first_token, second_token) = (
        token(&queue, &Principal::supervisor(&first, pid)),
        token(&queue, &Principal::supervisor(&second, pid)),
    );
    let op = |token: &str, op: &str, args: Value| {
        call(
            &queue,
            Some(token),
            UseCase::Supervisor,
            json!({"op": op, "args": args}),
        )
    };
    let target = StepTarget::of(TargetKind::PlanReview, 1);
    let held = || {
        dagq::application::queue_service::TelemetryTokens::telemetry_credential(
            &dagq::infrastructure::queue_service::SystemServiceAccess,
            &queue.db,
            &target,
        )
        .is_some()
    };
    let issued = op(
        &first_token,
        "issue_telemetry",
        json!([target, null, first]),
    );
    assert_eq!(issued["ok"], true, "{issued}");
    assert!(held());
    for lease in [json!(first), Value::Null] {
        let before = events(&queue.db, "authorization_denied").len();
        let refused = op(&second_token, "revoke_telemetry", json!([target, lease]));
        assert_eq!(
            refused["error"]["code"], "authorization_denied",
            "{lease}: {refused}"
        );
        let denied = events(&queue.db, "authorization_denied");
        assert_eq!(denied.len() - before, 1, "{lease}");
        let last = &denied[denied.len() - 1]["payload"];
        assert_eq!(last["capability"], "token.issue", "{last}");
        assert!(
            last["why"].as_str().unwrap().contains("another parent's"),
            "{last}"
        );
        assert!(held(), "{lease}");
    }
    let given_back = op(&first_token, "revoke_telemetry", json!([target, first]));
    assert_eq!(given_back["result"], true, "{given_back}");
    assert!(!held());

    // Once the first's registration went (it stopped), the first is gone
    // though its process runs on: the second gives back the telemetry the
    // first asked for, and is issued another's over the first's.
    let other = StepTarget::of(TargetKind::PlanReview, 2);
    for target in [&target, &other] {
        let issued = op(
            &first_token,
            "issue_telemetry",
            json!([target, null, first]),
        );
        assert_eq!(issued["ok"], true, "{issued}");
    }
    SqliteQueue::open(&queue.db)
        .unwrap()
        .deregister_supervisor(&first)
        .unwrap();
    let before = events(&queue.db, "authorization_denied").len();
    let revoked = op(&second_token, "revoke_telemetry", json!([target, null]));
    assert_eq!(revoked["result"], true, "{revoked}");
    assert!(!held());
    let taken = op(
        &second_token,
        "issue_telemetry",
        json!([other, null, second]),
    );
    assert_eq!(taken["ok"], true, "{taken}");
    assert_eq!(events(&queue.db, "authorization_denied").len(), before);
}

/// The supervisor's records of the container's parts (the image's builds
/// and failures, the relays' attention, the runs that own users of the
/// dagq machine) are operations of its use case on the service's side:
/// its principal is answered, and any other (a worker, a review job, the
/// user) is refused with `authorization_denied`, the refusal recorded,
/// and nothing is written.
#[test]
fn the_container_parts_records_are_answered_to_the_supervisor_alone() {
    use dagq::domain::LeaseToken;
    let queue = queue();
    start(&queue);
    let lease = LeaseToken::new("it-containers");
    SqliteQueue::open(&queue.db)
        .unwrap()
        .register_supervisor(&lease, std::process::id(), 1, "test")
        .unwrap();
    let supervisor = token(&queue, &Principal::supervisor(&lease, std::process::id()));
    let ops = [
        (
            "record_queue_event",
            json!(["container_image_built", {"reference": "localhost/dagq-q:ab"}]),
        ),
        (
            "latest_queue_event",
            json!([["container_image_built", "container_image_failed"]]),
        ),
        (
            "latest_queue_event",
            json!([["queue_relay_down", "queue_relay_running"]]),
        ),
        ("latest_runs_in_progress", json!([])),
        ("run", json!([queue.run.id()])),
    ];
    let op = |token: Option<&str>, op: &str, args: &Value| {
        call(
            &queue,
            token,
            UseCase::Supervisor,
            json!({"op": op, "args": args}),
        )
    };
    let worker = token(
        &queue,
        &Principal::worker(queue.run.id(), queue.run.task_id()),
    );
    let review = token(
        &queue,
        &Principal::of(&ActorContext::review_job(queue.run.id(), 1)),
    );
    for other in [Some(worker.as_str()), Some(&review), None] {
        for (name, args) in &ops {
            let refused = op(other, name, args);
            assert_eq!(
                refused["error"]["code"], "authorization_denied",
                "{name}: {refused}"
            );
        }
    }
    assert!(events(&queue.db, "container_image_built").is_empty());
    let denials = events(&queue.db, "authorization_denied");
    // The worker's, the review job's and the user's refusals, each as its
    // actor.
    assert_eq!(denials.len(), 3 * ops.len(), "{denials:?}");
    assert_eq!(
        denials
            .iter()
            .filter(|denial| denial["actor"]["role"] == "user")
            .count(),
        ops.len(),
        "{denials:?}"
    );
    assert!(
        denials
            .iter()
            .all(|denial| denial["payload"]["capability"] == "scheduler.supervise"),
        "{denials:?}"
    );

    for (name, args) in &ops {
        let answered = op(Some(&supervisor), name, args);
        assert_eq!(answered["ok"], true, "{name}: {answered}");
    }
    let built = events(&queue.db, "container_image_built");
    assert_eq!(built.len(), 1, "{built:?}");
    assert_eq!(built[0]["actor"]["role"], "supervisor", "{built:?}");
}

/// The answer of the queue's service at `queue_dir` to a `show` with
/// `token`, from this test's process.
fn show_with(queue_dir: &Path, token: &str) -> Value {
    let response = service::call(
        &service::socket_path(queue_dir),
        &ServiceRequest {
            api_version: API_VERSION,
            token: Some(token.to_owned()),
            env_role: None,
            queue: None,
            binding: None,
            use_case: UseCase::Show,
            params: json!({"id": 1}),
        },
        TIMEOUT,
    )
    .unwrap();
    serde_json::to_value(response).unwrap()
}

/// The token of a supervisor's registration goes with the registration's
/// end (ADR-t3102-1 decision 2): one a live supervisor's sweep prunes as
/// left behind (its process gone, its heartbeat expired) at the prune, and
/// the supervisor's own as it stops. Each is refused by the service after
/// (`unauthenticated`, `unknown_token`) and its file is gone, while the
/// live supervisor's token holds until its stop. The service revokes the
/// stopping one's at its `sign_out` request: a supervisor whose request
/// never reaches the service keeps its token, which its process does not
/// remove itself.
#[test]
fn a_supervisors_token_goes_with_the_end_of_its_registration() {
    use dagq::application::supervisor_service::ServiceCaller;
    use dagq::domain::{HEARTBEAT_TIMEOUT_SECS, LeaseToken};

    let (_fixture, repo, db) = idle_queue();
    let dagq = common::service::owned_executable(db.parent().unwrap());
    let queue_dir = db.parent().unwrap().to_path_buf();
    let credential = |token: &str, pid: u32| {
        service::credential_path(
            &queue_dir,
            &Principal::supervisor(&LeaseToken::new(token), pid).credential_key(),
        )
    };
    let refused = |token: &str| {
        let answer = show_with(&queue_dir, token);
        assert_eq!(answer["error"]["code"], "unauthenticated", "{answer}");
    };
    let supervisor = Running::with_options(
        &db,
        &repo,
        dagq::compose::SuperviseOptions {
            queue_service: Some(dagq::compose::QueueServiceOptions {
                executable: dagq.clone(),
                interval: Duration::ZERO,
                start_timeout: TIMEOUT,
                control: None,
            }),
            sweep_interval: Duration::ZERO,
            ..crate::runtime_support::supervise_options(1, false)
        },
    );
    let own = supervisor_token(&db);
    let own_file = credential(&own, std::process::id());
    let own_token = service::read_token(&own_file).unwrap();

    // A supervisor that signed in as the user's and registered, and whose
    // process is gone without pruning its registration.
    let (left, dead) = ("left-behind", common::lifecycle::dead_pid());
    service::SocketCaller::new(&db)
        .sign_in(&LeaseToken::new(left), dead)
        .unwrap();
    let left_file = credential(left, dead);
    let left_token = service::read_token(&left_file).unwrap();
    SqliteQueue::open(&db)
        .unwrap()
        .register_supervisor(&LeaseToken::new(left), dead, 1, dagq::VERSION)
        .unwrap();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE supervisors SET heartbeat_at=?1 WHERE token=?2",
            rusqlite::params![
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64
                    - HEARTBEAT_TIMEOUT_SECS
                    - 60,
                left
            ],
        )
        .unwrap();
    {
        let _waiting = common::within(TIMEOUT * 3, "the left registration to be pruned");
        while left_file.exists()
            || SqliteQueue::open(&db)
                .unwrap()
                .supervisors()
                .unwrap()
                .iter()
                .any(|registration| registration.token == left)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    refused(&left_token);
    // The live one's holds through its sweeps.
    supervisor.passes(2);
    assert!(own_file.exists());
    let answer = show_with(&queue_dir, &own_token);
    assert_ne!(answer["error"]["code"], "unauthenticated", "{answer}");

    supervisor.drain();
    assert!(!own_file.exists(), "{}", own_file.display());
    refused(&own_token);
    let reasons: Vec<Value> = events(&db, "queue_service_unauthenticated")
        .iter()
        .map(|event| event["payload"]["reason"].clone())
        .collect();
    assert_eq!(reasons, [json!("unknown_token"), json!("unknown_token")]);

    let (unsent, sign_outs) = crate::runtime_support::service::KeepsDown::wrap("sign_out");
    let supervisor = Running::with_options(
        &db,
        &repo,
        dagq::compose::SuperviseOptions {
            service_route: dagq::compose::ServiceRoute::InProcessThrough(unsent),
            ..crate::runtime_support::supervise_options(1, false)
        },
    );
    let kept = supervisor_token(&db);
    supervisor.passes(1);
    let kept_file = credential(&kept, std::process::id());
    let kept_token = service::read_token(&kept_file).unwrap();
    supervisor.drain();
    assert_eq!(sign_outs.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(kept_file.exists(), "{}", kept_file.display());
    let answer = show_with(&queue_dir, &kept_token);
    assert_ne!(answer["error"]["code"], "unauthenticated", "{answer}");
}

/// `up` and `down` revoke, through the service's `revoke_token` use case,
/// the token of each registration they prune (ADR-t3102-1 decision 2): a
/// supervisor that signed in, registered and died without giving it back.
/// The service refuses the token after (`unauthenticated`,
/// `unknown_token`) and its file is gone.
#[test]
fn up_and_down_revoke_the_tokens_of_the_registrations_they_prune() {
    use dagq::application::supervisor_service::ServiceCaller;
    use dagq::domain::LeaseToken;

    let mut fixture = common::lifecycle::fixture();
    fixture.options.queue_service = true;
    fixture.environment.current_exe =
        common::service::owned_executable(fixture.location.db.parent().unwrap());
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let db = fixture.location.db.clone();
    let queue_dir = db.parent().unwrap().to_path_buf();
    struct Down(PathBuf);
    impl Drop for Down {
        fn drop(&mut self) {
            let _ = invoke(&self.0, &["service", "stop"]);
        }
    }
    let _down = Down(db.clone());
    let first = common::lifecycle::up(&fixture, &launchd, &processes);
    assert_eq!(first["queue_service"]["outcome"], "started", "{first}");
    // A supervisor that signed in as the user's, registered and died.
    let died = |token: &str| {
        let pid = common::lifecycle::dead_pid();
        processes.dead.lock().unwrap().insert(pid);
        service::SocketCaller::new(&db)
            .sign_in(&LeaseToken::new(token), pid)
            .unwrap();
        SqliteQueue::open(&db)
            .unwrap()
            .register_supervisor(&LeaseToken::new(token), pid, 1, dagq::VERSION)
            .unwrap();
        let file = service::credential_path(
            &queue_dir,
            &Principal::supervisor(&LeaseToken::new(token), pid).credential_key(),
        );
        let value = service::read_token(&file).unwrap();
        (file, value)
    };
    let refused = |token: &str| {
        let answer = show_with(&queue_dir, token);
        assert_eq!(answer["error"]["code"], "unauthenticated", "{answer}");
    };

    let (up_file, up_token) = died("pruned-by-up");
    let again = common::lifecycle::up(&fixture, &launchd, &processes);
    assert_eq!(
        again["pruned_supervisors"][0]["token"], "pruned-by-up",
        "{again}"
    );
    assert!(!up_file.exists(), "{}", up_file.display());
    refused(&up_token);

    // `down --force` prunes before it stops the service, which is started
    // again to be asked.
    let (down_file, down_token) = died("pruned-by-down");
    let forced = common::lifecycle::down(&fixture, &launchd, &processes, false, true);
    assert_eq!(forced["queue_service"]["outcome"], "stopped", "{forced}");
    assert!(!down_file.exists(), "{}", down_file.display());
    common::service::serve(&db);
    refused(&down_token);
    let reasons: Vec<Value> = events(&db, "queue_service_unauthenticated")
        .iter()
        .map(|event| event["payload"]["reason"].clone())
        .collect();
    assert_eq!(reasons, [json!("unknown_token"), json!("unknown_token")]);
}

/// The service revokes as it starts a supervisor's token left over from a
/// registration that ended, as one is when the supervisor stopped the
/// service itself at the end of a `down`'s drain and so could not give it
/// back (ADR-t3102-1 decision 2): its file is gone and the service refuses
/// it. One whose registration is kept, and one of a supervisor not
/// registered yet whose process runs, stay.
#[test]
fn the_service_revokes_at_its_start_the_tokens_of_ended_registrations() {
    use dagq::application::ProcessControl;
    use dagq::domain::LeaseToken;
    use dagq::domain::queue_service::{SessionProcess, TokenIssuer};

    let queue = queue();
    let queue_dir = queue.dir().to_path_buf();
    let dead = common::lifecycle::dead_pid();
    let own = std::process::id();
    let process = |pid: u32| SessionProcess {
        pid,
        start: dagq::infrastructure::adapters::SystemProcesses
            .session_start(pid)
            .unwrap_or_else(|| "Thu Oct 23 10:00:00 2026".to_owned()),
    };
    let signed_in = |token: &str, pid: u32, asked: SessionProcess| {
        let issuer = TokenIssuer {
            parent: "user".to_owned(),
            process: Some(asked),
            registration: None,
        };
        let principal = Principal::supervisor(&LeaseToken::new(token), pid);
        let file = service::issue_by(&queue_dir, &principal, &issuer, 1)
            .unwrap()
            .file;
        let value = service::read_token(&file).unwrap();
        (file, value)
    };
    let (ended_file, ended) = signed_in("ended", dead, process(dead));
    let (kept_file, kept) = signed_in("kept", dead, process(dead));
    SqliteQueue::open(&queue.db)
        .unwrap()
        .register_supervisor(&LeaseToken::new("kept"), dead, 1, dagq::VERSION)
        .unwrap();
    let (starting_file, starting) = signed_in("starting", own, process(own));

    start(&queue);
    assert!(!ended_file.exists(), "{}", ended_file.display());
    let answer = show_with(&queue_dir, &ended);
    assert_eq!(answer["error"]["code"], "unauthenticated", "{answer}");
    for (file, token) in [(&kept_file, &kept), (&starting_file, &starting)] {
        assert!(file.exists(), "{}", file.display());
        let answer = show_with(&queue_dir, token);
        assert_ne!(answer["error"]["code"], "unauthenticated", "{answer}");
    }
}

/// A supervisor's token an older binary left, kept with no process that
/// asked for it (ADR-t3102-1 decision 2), goes by the supervisor's own
/// process, the pid its principal names: once its registration is gone and
/// that pid is gone, or another process started on it after the issue, the
/// service's start revokes it, and a live supervisor's sweep revokes one
/// left while the service runs. One whose supervisor still starts (its pid
/// runs since before the issue, not registered yet) and one whose
/// registration is kept stay.
#[test]
fn a_supervisors_token_an_older_binary_left_goes_once_its_registration_and_process_ended() {
    use dagq::domain::LeaseToken;

    let queue = queue();
    let queue_dir = queue.dir().to_path_buf();
    let dead = common::lifecycle::dead_pid();
    let own = std::process::id();
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    // Issued as an older binary issued it: no process that asked is kept.
    let older = |token: &str, pid: u32, issued_at: i64| {
        let principal = Principal::supervisor(&LeaseToken::new(token), pid);
        let file = service::issue(&queue_dir, &principal, issued_at)
            .unwrap()
            .file;
        let value = service::read_token(&file).unwrap();
        (principal, file, value)
    };
    let refused = |file: &Path, value: &str| {
        assert!(!file.exists(), "{}", file.display());
        let answer = show_with(&queue_dir, value);
        assert_eq!(answer["error"]["code"], "unauthenticated", "{answer}");
    };
    let held = |file: &Path, value: &str| {
        assert!(file.exists(), "{}", file.display());
        let answer = show_with(&queue_dir, value);
        assert_ne!(answer["error"]["code"], "unauthenticated", "{answer}");
    };
    let (_, gone_file, gone) = older("gone", dead, now);
    // Its pid is this test's now, which started after the issue.
    let (_, reused_file, reused) = older("reused", own, 1);
    let (starting_principal, starting_file, starting) = older("starting", own, now);
    let (_, kept_file, kept) = older("kept", dead, now);
    let sweeper = LeaseToken::new("sweeper");
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    db.register_supervisor(&LeaseToken::new("kept"), dead, 1, dagq::VERSION)
        .unwrap();
    db.register_supervisor(&sweeper, own, 1, dagq::VERSION)
        .unwrap();
    drop(db);

    start(&queue);
    refused(&gone_file, &gone);
    refused(&reused_file, &reused);
    held(&kept_file, &kept);
    held(&starting_file, &starting);

    let supervisor = token(&queue, &Principal::supervisor(&sweeper, own));
    let revoke = |principal: &Principal| {
        call(
            &queue,
            Some(&supervisor),
            UseCase::Supervisor,
            json!({"op": "revoke_principal", "args": [principal]}),
        )
    };
    let (swept_principal, swept_file, swept) = older("swept", dead, now);
    assert_eq!(revoke(&swept_principal)["result"], true);
    refused(&swept_file, &swept);
    assert_eq!(revoke(&starting_principal)["result"], false);
    held(&starting_file, &starting);
}

/// The supervisor a `down` asks to stop the service at the end of its drain
/// has the service revoke its token as it stops (ADR-t3102-1 decision 2):
/// once the drain is over, before any service starts again, its file is
/// gone and no record authenticates it, and a service started again
/// refuses it. The supervisor is on the service's socket, as `up` starts
/// one.
#[test]
fn the_supervisor_that_stops_the_service_after_down_leaves_no_token() {
    use dagq::domain::LeaseToken;
    use dagq::domain::queue_service::ServiceState::Stopped;

    let (_fixture, repo, db) = idle_queue();
    let dagq = common::service::owned_executable(db.parent().unwrap());
    let queue_dir = db.parent().unwrap().to_path_buf();
    struct Down(PathBuf);
    impl Drop for Down {
        fn drop(&mut self) {
            let _ = invoke(&self.0, &["service", "stop"]);
        }
    }
    let _down = Down(db.clone());
    let supervisor = Running::with_options(
        &db,
        &repo,
        dagq::compose::SuperviseOptions {
            queue_service: Some(dagq::compose::QueueServiceOptions {
                executable: dagq.clone(),
                interval: Duration::ZERO,
                start_timeout: TIMEOUT,
                control: None,
            }),
            service_route: dagq::compose::ServiceRoute::Socket,
            ..crate::runtime_support::supervise_options(1, false)
        },
    );
    let token = supervisor_token(&db);
    supervisor.passes(2);
    let file = service::credential_path(
        &queue_dir,
        &Principal::supervisor(&LeaseToken::new(&token), std::process::id()).credential_key(),
    );
    let value = service::read_token(&file).unwrap();
    request_stop(&db, &[&token]);
    supervisor.drain();
    assert_eq!(service::probe(&queue_dir).state, Stopped);
    assert!(!file.exists(), "{}", file.display());
    assert_eq!(service::lookup(&queue_dir, &value).unwrap(), None);

    common::service::serve(&db);
    let answer = show_with(&queue_dir, &value);
    assert_eq!(answer["error"]["code"], "unauthenticated", "{answer}");
}

/// A `down --wait` that stops the service as soon as the supervisor's
/// registration is gone, before the supervisor's own stop of it and its
/// sign-out, leaves no token either (ADR-t3102-1 decision 2): the
/// supervisor asked the service to revoke it as it stops before it removed
/// the registration. Once `down` returns, while the supervisor is still held
/// right after its deregistration, the file is gone and no record
/// authenticates it; a service started again refuses it.
#[test]
fn a_down_wait_that_stops_the_service_once_the_registration_is_gone_leaves_no_token() {
    use crate::runtime_support::headless::{pause_once, wait_for_file};
    use dagq::domain::LeaseToken;
    use dagq::domain::queue_service::ServiceState::Stopped;

    let (_fixture, repo, db) = idle_queue();
    let dagq = common::service::owned_executable(db.parent().unwrap());
    let queue_dir = db.parent().unwrap().to_path_buf();
    struct Down(PathBuf);
    impl Drop for Down {
        fn drop(&mut self) {
            let _ = invoke(&self.0, &["service", "stop"]);
        }
    }
    let _down = Down(db.clone());
    let (deregistered, go) = (queue_dir.join("deregistered"), queue_dir.join("go"));
    let options = dagq::compose::SuperviseOptions {
        queue_service: Some(dagq::compose::QueueServiceOptions {
            executable: dagq.clone(),
            interval: Duration::ZERO,
            start_timeout: TIMEOUT,
            control: None,
        }),
        service_route: dagq::compose::ServiceRoute::Socket,
        pauses: Some(pause_once(
            dagq::runtime::PausePoint::Deregistered,
            deregistered.clone(),
            go.clone(),
        )),
        ..crate::runtime_support::supervise_options(1, false)
    };
    let stop = options.stop.clone();
    let supervisor = Running::with_options(&db, &repo, options);
    // A failure lets the held supervisor go on before it is waited for.
    struct Go(PathBuf);
    impl Drop for Go {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, "");
        }
    }
    let release = Go(go);
    let token = supervisor_token(&db);
    supervisor.passes(2);
    let file = service::credential_path(
        &queue_dir,
        &Principal::supervisor(&LeaseToken::new(&token), std::process::id()).credential_key(),
    );
    let value = service::read_token(&file).unwrap();

    let location =
        dagq::infrastructure::location::QueueLocation::explicit_in(&db, &queue_dir.join("home"));
    let launchd = FakeLaunchd::new(&db);
    let processes = FakeProcesses::default();
    let report = std::thread::scope(|scope| {
        let down = scope.spawn(|| {
            common::lifecycle::one_shot()
                .down(
                    &location,
                    &launchd,
                    &processes,
                    &dagq::lifecycle::DownOptions {
                        wait: true,
                        force: false,
                        poll: Duration::from_millis(20),
                    },
                )
                .unwrap()
        });
        // The supervisor drains once `down` asked it to stop the service
        // and signalled it, and is held right after its deregistration.
        {
            let _waiting = common::within(TIMEOUT * 3, "down to ask for the service's stop");
            while events(&db, "queue_service_stop_requested").is_empty() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        wait_for_file(&deregistered);
        let _waiting = common::within(TIMEOUT * 3, "down --wait to return");
        down.join().unwrap()
    });
    assert_eq!(report["queue_service"]["outcome"], "stopped", "{report}");
    assert_eq!(service::probe(&queue_dir).state, Stopped);
    assert!(!file.exists(), "{}", file.display());
    assert_eq!(service::lookup(&queue_dir, &value).unwrap(), None);

    drop(release);
    supervisor.drain();
    common::service::serve(&db);
    let answer = show_with(&queue_dir, &value);
    assert_eq!(answer["error"]["code"], "unauthenticated", "{answer}");
}

/// The supervisor's sweep of an ended worker (`revoke_principal`) takes,
/// on the service's queue, every record of it: one whose file a later
/// issue replaced, which still authenticates, and the one the file holds,
/// with the file. Another worker's token and file stay.
#[test]
fn the_supervisors_sweep_takes_a_workers_record_its_file_no_longer_names() {
    use dagq::domain::{LeaseToken, RunId, TaskId};
    let queue = queue();
    start(&queue);
    let pid = std::process::id();
    let lease = LeaseToken::new("it-sweep");
    SqliteQueue::open(&queue.db)
        .unwrap()
        .register_supervisor(&lease, pid, 1, "test")
        .unwrap();
    let supervisor = token(&queue, &Principal::supervisor(&lease, pid));
    let worker = Principal::worker(&RunId::new("it-ended").unwrap(), TaskId::new(1));
    let other = Principal::worker(&RunId::new("it-other").unwrap(), TaskId::new(2));
    // The first token's record, kept across a later issue over its file.
    let first = token(&queue, &worker);
    let records = queue.dir().join("service/tokens");
    let before: Vec<_> = std::fs::read_dir(&records)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    let kept: Vec<_> = before
        .iter()
        .map(|path| (path.clone(), std::fs::read(path).unwrap()))
        .collect();
    let later = token(&queue, &worker);
    for (path, bytes) in &kept {
        std::fs::write(path, bytes).unwrap();
    }
    let kept_token = token(&queue, &other);
    assert_eq!(
        service::lookup(queue.dir(), &first).unwrap(),
        Some(worker.clone())
    );

    let swept = call(
        &queue,
        Some(&supervisor),
        UseCase::Supervisor,
        json!({"op": "revoke_principal", "args": [worker]}),
    );
    assert_eq!(swept["result"], true, "{swept}");
    for value in [&first, &later] {
        assert_eq!(service::lookup(queue.dir(), value).unwrap(), None);
    }
    assert!(!service::credential_path_of(queue.dir(), &worker).exists());
    assert_eq!(
        service::lookup(queue.dir(), &kept_token).unwrap(),
        Some(other.clone())
    );
    assert!(service::credential_path_of(queue.dir(), &other).exists());
}
