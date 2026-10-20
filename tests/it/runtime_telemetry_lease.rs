//! Runtime tests: a supervisor that loses a run's lease revokes the
//! telemetry token it issued the run's executor under that lease, and the
//! queue service process refuses it from then on (ADR-t1662-3 decision
//! 11). The loss is the heartbeat's failure, which ends the loop, or the
//! run's lease gone while the supervisor goes on; a revocation that fails
//! is recorded only once a later sweep revoked it, by the process an exec
//! hands the registration to too. A supervisor that reviews a run after the
//! one that claimed it ended issues its executors a token of its own.
use crate::runtime_support;

use runtime_support::*;

use dagq::{
    application::{
        SessionOwners, TelemetrySink, queue_service::TelemetryTokens, telemetry_tokens::Retries,
    },
    domain::{
        EventKind,
        session_step::{Step, StepKind, StepTarget},
        telemetry::{self, TelemetryReport},
    },
    infrastructure::queue_service::{self as service, ServiceTelemetrySink, SystemServiceAccess},
};

/// A supervisor (not `--once`) that claimed task 1, whose worker waits,
/// and the run's telemetry token as its executor holds it: accepted by the
/// service for a session registered to the run.
struct Held {
    db: PathBuf,
    supervisor: Option<thread::JoinHandle<Result<Value>>>,
    options: SuperviseOptions,
    run: TaskRun,
    target: StepTarget,
    credential: PathBuf,
    sink: ServiceTelemetrySink,
    report: TelemetryReport,
    _served: crate::common::service::Served,
}

fn held(db: &Path, repo: &Path) -> Held {
    let queue_dir = db.parent().unwrap().to_owned();
    let started = queue_dir.join("worker-started");
    let worker = format!(
        ": > {started}; await_file \"$EXIT.go\"; {VALID_AGENT}",
        started = shell_path(&started)
    );
    let backend = TestWorkspace::new(db, false, &worker);
    let served = crate::common::service::Served::start(db);
    let options = SuperviseOptions {
        heartbeat_interval: Duration::from_millis(100),
        ..supervise_options(1, false)
    };
    let supervisor = {
        let (db, repo, options) = (db.to_path_buf(), repo.to_path_buf(), options.clone());
        thread::spawn(move || {
            runtime::supervise(
                &db,
                &repo,
                &backend,
                &claude_stub(&db),
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    headless::wait_for_file(&started);
    let run = SqliteQueue::open(db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    let target = StepTarget::run(run.id());
    let credential = SystemServiceAccess
        .telemetry_credential(db, &target)
        .expect("the claim issued the run's telemetry token");
    let sent = queue_dir.join("held-token");
    fs::write(&sent, service::read_token(&credential).unwrap()).unwrap();
    let sink = ServiceTelemetrySink::new(service::socket_path(&queue_dir), sent);
    let report = TelemetryReport::new(
        target.clone(),
        "s1",
        vec![Step::new(1, StepKind::ModelTurn, 0, 5)],
    );
    SqliteQueue::open(db)
        .unwrap()
        .register_session(&target, "s1", 1)
        .unwrap();
    sink.report(&report).unwrap();
    Held {
        db: db.to_path_buf(),
        supervisor: Some(supervisor),
        options,
        run,
        target,
        credential,
        sink,
        report,
        _served: served,
    }
}

impl Held {
    /// The reasons of the run's `telemetry_token_revoked`, in order.
    fn revoked(&self) -> Vec<Value> {
        SqliteQueue::open(&self.db)
            .unwrap()
            .run_events(self.run.id())
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == EventKind::TelemetryTokenRevoked)
            .map(|event| event.payload["reason"].clone())
            .collect()
    }

    /// The service refuses the token the executor held, revoked, and no
    /// file of the run's token is left to read.
    fn refused(&self) {
        let refused = self.sink.report(&self.report).unwrap_err();
        assert!(
            format!("{refused:#}").contains("unauthenticated"),
            "{refused:#}"
        );
        assert_eq!(
            SystemServiceAccess.telemetry_credential(&self.db, &self.target),
            None
        );
    }

    /// What the supervisor's thread returned.
    fn join(&mut self) -> Result<Value> {
        let _waiting =
            crate::common::within(crate::common::STEP_LIMIT, "the supervisor thread to return");
        self.supervisor.take().unwrap().join().unwrap()
    }
}

/// A supervisor whose heartbeat fails loses the leases of its runs: the
/// telemetry token of the run it held is revoked and recorded as such, and
/// the service refuses what is sent with it.
#[test]
fn a_failed_heartbeat_revokes_the_runs_telemetry_token() {
    let (_dir, repo, db) = fixture();
    let mut held = held(&db, &repo);
    // The heartbeat fails from now on: the loop ends on an error.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_heartbeat BEFORE UPDATE ON supervisors
             BEGIN SELECT RAISE(ABORT, 'heartbeat refused by the test'); END;",
        )
        .unwrap();
    let error = format!("{:#}", held.join().unwrap_err());
    assert!(error.contains("heartbeat failed"), "{error}");
    held.refused();
    assert_eq!(held.revoked(), [json!("lease_lost")]);
}

/// A run whose lease is gone while its supervisor goes on (the run stays
/// `running`): the supervisor disowns it and revokes the token issued under
/// its lease. While the token's file cannot be read the revocation fails,
/// nothing is recorded and the token still authenticates; the supervisor
/// tries again on its next sweeps, and once the file can be read it revokes
/// it and records it, and the service refuses it.
#[test]
fn a_lost_lease_whose_revocation_failed_is_revoked_by_a_later_sweep() {
    let (_dir, repo, db) = fixture();
    let mut held = held(&db, &repo);
    crate::common::set_mode(&held.credential, 0o000);
    if fs::read(&held.credential).is_ok() {
        // A process that permissions do not hold back (root) cannot make
        // the host fail here.
        eprintln!("the permissions do not hold this process back; no failure can be injected");
        crate::common::set_mode(&held.credential, 0o600);
        held.options
            .stop
            .store(true, std::sync::atomic::Ordering::SeqCst);
        held.join().unwrap();
        return;
    }
    let deleted = rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM run_leases WHERE run_id=?1",
            [held.run.id().as_str()],
        )
        .unwrap();
    assert_eq!(deleted, 1);
    // The disown and two sweeps after it, each failing.
    await_passes(&held.options.passes, 3);
    assert_eq!(held.revoked(), Vec::<Value>::new());
    held.sink.report(&held.report).unwrap();
    assert_eq!(
        SqliteQueue::open(&db)
            .unwrap()
            .run(held.run.id())
            .unwrap()
            .status(),
        RunStatus::Running
    );

    crate::common::set_mode(&held.credential, 0o600);
    wait_until(&db, crate::common::STEP_LIMIT, |_| {
        !held.revoked().is_empty()
    });
    assert_eq!(held.revoked(), [json!("lease_lost")]);
    held.refused();
    held.options
        .stop
        .store(true, std::sync::atomic::Ordering::SeqCst);
    held.join().unwrap();
}

/// A supervisor whose revocation of a lost lease's token fails hands its
/// registration off (the exec of a handoff) while the failure lasts. The
/// process after the exec keeps the registration and its lease token, so
/// no sweep finds the token due (the run has no lease and its issuer
/// lives); the revocation the process before kept is taken over, and once
/// the token's file can be read its first pass revokes the token and
/// records it, and the service refuses it.
#[test]
fn a_revocation_held_at_a_handoff_is_made_by_the_process_after_the_exec() {
    let (_dir, repo, db) = fixture();
    let mut held = held(&db, &repo);
    let queue_dir = db.parent().unwrap().to_owned();
    crate::common::set_mode(&held.credential, 0o000);
    if fs::read(&held.credential).is_ok() {
        // A process that permissions do not hold back (root) cannot make
        // the host fail here.
        eprintln!("the permissions do not hold this process back; no failure can be injected");
        crate::common::set_mode(&held.credential, 0o600);
        held.options
            .stop
            .store(true, std::sync::atomic::Ordering::SeqCst);
        held.join().unwrap();
        return;
    }
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM run_leases WHERE run_id=?1",
            [held.run.id().as_str()],
        )
        .unwrap();
    await_passes(&held.options.passes, 3);
    let token = SqliteQueue::open(&db).unwrap().supervisors().unwrap()[0]
        .token
        .clone();
    assert!(
        SqliteQueue::open(&db)
            .unwrap()
            .request_handoff(&token, "/next/dagq")
            .unwrap()
    );
    let outcome = held.join().unwrap();
    assert_eq!(outcome["outcome"], "handoff", "{outcome}");
    assert_eq!(held.revoked(), Vec::<Value>::new());
    held.sink.report(&held.report).unwrap();
    let kept = Retries::file(&queue_dir, &token);
    assert!(kept.is_file());

    crate::common::set_mode(&held.credential, 0o600);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let after = supervise_with(
        &db,
        &repo,
        &backend,
        &SuperviseOptions {
            handoff_token: Some(token),
            ..supervise_options(1, true)
        },
    );
    assert!(after.is_ok(), "{after:?}");
    assert!(!kept.exists());
    assert_eq!(held.revoked(), [json!("lease_lost")]);
    held.refused();
}

/// A run whose two reviews gave no verdict waits, with no lease, for the
/// answer to its `approve_landing` ask once the supervisor that claimed it
/// ended; the next supervisor's sweep revokes that one's token (its issuer
/// is gone). The `retry_review` answer has the next supervisor review it
/// again without a resume: the review is issued a token under that
/// supervisor's lease, which its review job finds in the run's file, and
/// the landing's verification after it finds the same; each holds, the
/// claim's does not, and the run's end revokes it.
#[test]
fn a_review_after_its_supervisor_ended_and_its_landing_find_a_token_that_holds() {
    let (_dir, repo, db) = fixture();
    let queue_dir = db.parent().unwrap().to_owned();
    let snapshots = queue_dir.join("snapshots");
    let verify = serde_json::to_string(&[format!(
        "{}test -f seed.txt",
        runtime_support::service::token_snapshot(&queue_dir, &snapshots, "verification")
    )])
    .unwrap();
    // A ready task's verification is not editable through the CLI.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands=?1 WHERE id=1",
            [verify],
        )
        .unwrap();
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "{}{IDLE_AGENT}",
            runtime_support::service::token_snapshot(&queue_dir, &snapshots, "worker")
        ),
    );
    const NO_VERDICT: &str = "echo 'no verdict here'";
    let reviewer = TestReviewer::new(&[
        NO_VERDICT.to_owned(),
        NO_VERDICT.to_owned(),
        format!(
            "{}{}",
            runtime_support::service::token_snapshot(&queue_dir, &snapshots, "review"),
            verdict("pass", &[], "fine")
        ),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    queue.answer(asks[0].id, "retry_review").unwrap();
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Integrated);
    assert!(payloads(&detail, "resume_started").is_empty());
    let target = StepTarget::run(run.id());
    let file = service::credential_path(&queue_dir, &telemetry::actor_id(&target));
    let held = |name: &str| {
        let root = snapshots.join(name);
        let token = service::read_token(
            &root
                .join("service")
                .join(service::CREDENTIALS_DIR)
                .join(file.file_name().unwrap()),
        )
        .unwrap();
        (root, token)
    };
    let (_, claimed) = held("worker");
    let (at_review, reviewed) = held("review");
    let (at_verification, verified) = held("verification");
    assert_ne!(reviewed, claimed);
    assert_eq!(verified, reviewed);
    assert_eq!(service::lookup(&at_review, &claimed).unwrap(), None);
    for (root, token) in [(&at_review, &reviewed), (&at_verification, &verified)] {
        let principal = service::lookup(root, token).unwrap().unwrap();
        assert_eq!(principal.target, Some(target.clone()));
    }
    assert_eq!(SystemServiceAccess.telemetry_credential(&db, &target), None);
    assert_eq!(SystemServiceAccess.held_telemetry(&db).unwrap(), []);
    let recorded: Vec<(EventKind, Value)> = detail
        .events
        .iter()
        .filter(|event| {
            event.kind.is_in(&[
                EventKind::TelemetryTokenIssued,
                EventKind::TelemetryTokenRevoked,
            ])
        })
        .map(|event| (event.kind.known().unwrap(), event.payload["reason"].clone()))
        .collect();
    assert_eq!(
        recorded,
        [
            (EventKind::TelemetryTokenIssued, Value::Null),
            (EventKind::TelemetryTokenRevoked, json!("lease_lost")),
            (EventKind::TelemetryTokenIssued, Value::Null),
            (EventKind::TelemetryTokenRevoked, json!("run_ended")),
        ]
    );
}
