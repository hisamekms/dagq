//! The telemetry token of a run's executor on the host (ADR-t1662-3
//! decision 11, docs/design/queue-service.md "principalとtoken"): issued
//! under the supervisor's lease at the claim and again at a resume or an
//! adoption, which revokes the one before, and revoked at a lost lease
//! (only the one issued under it) and by the sweep once the run ended; the
//! queue service process refuses each token once it was revoked. A
//! revocation is recorded on the run only once it succeeded: a token file
//! that cannot be read or a record that cannot be removed leaves the token
//! listed and unrecorded, and the next sweep revokes it and records it
//! then. An issue and a revocation hold the queue's lock, so a late
//! revocation never removes the file of a token issued meanwhile.

use crate::common;
use common::service::{events, queue, start};

use std::{fs, path::Path, sync::mpsc, thread, time::Duration};

use dagq::{
    application::{
        RunReads, SessionOwners, TelemetrySink,
        queue_service::TelemetryTokens,
        telemetry_tokens::{Retries, issue_run, revoke_run, sweep},
    },
    domain::{
        EventKind, LeaseToken, RunId, TaskId,
        queue_service::Principal,
        session_step::{Step, StepKind, StepTarget},
        telemetry::{
            self, HeldTelemetry, LeaseView, RunTokenState, TelemetryReport, TokenRevocation,
        },
    },
    infrastructure::{
        queue_service::{self as service, ServiceTelemetrySink, SystemServiceAccess},
        run_files::LocalRunFiles,
        sqlite::SqliteQueue,
    },
};

/// What the run's executor sends: one step of session `s1`.
fn report(target: &StepTarget) -> TelemetryReport {
    TelemetryReport::new(
        target.clone(),
        "s1",
        vec![Step::new(1, StepKind::ModelTurn, 0, 5)],
    )
}

/// Send as `token` (written to a file of the test's, as an executor that
/// read it earlier holds it); the error when it is refused.
fn send(dir: &Path, token: &str, target: &StepTarget) -> Option<String> {
    let file = dir.join("held-token");
    fs::write(&file, token).unwrap();
    ServiceTelemetrySink::new(service::socket_path(dir), file)
        .report(&report(target))
        .err()
        .map(|error| format!("{error:#}"))
}

/// Why the service refused the latest unauthenticated request:
/// `unknown_token` for a token revoked, `run_ended` for one still issued
/// whose run ended.
fn last_refusal(db: &Path) -> String {
    let refused = events(db, "queue_service_unauthenticated");
    refused.last().unwrap()["payload"]["reason"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// The run's `telemetry_token_*` events, each as its kind and reason.
fn token_events(db: &Path, run: &RunId) -> Vec<(EventKind, Option<String>)> {
    SqliteQueue::open(db)
        .unwrap()
        .run_events(run)
        .unwrap()
        .into_iter()
        .filter(|event| {
            event.kind.is_in(&[
                EventKind::TelemetryTokenIssued,
                EventKind::TelemetryTokenRevoked,
            ])
        })
        .map(|event| {
            (
                event.kind.known().unwrap(),
                event.payload["reason"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

fn issued() -> (EventKind, Option<String>) {
    (EventKind::TelemetryTokenIssued, None)
}

fn revoked(why: TokenRevocation) -> (EventKind, Option<String>) {
    (
        EventKind::TelemetryTokenRevoked,
        Some(why.as_str().to_owned()),
    )
}

/// The sweep with the queue's status of each run, as a run with no lease
/// whose issuer lives reads: only its end makes a token due here (a lost
/// lease's reading is the unit tests' of `domain::telemetry`).
fn swept(log: &SqliteQueue, db: &Path) -> Vec<RunId> {
    sweep(
        &SystemServiceAccess,
        log,
        db,
        &LeaseToken::new("sweeper"),
        &mut Retries::default(),
        &|run, _| {
            Ok(RunTokenState {
                status: status(db, run),
                lease: None,
                issuer_alive: true,
            })
        },
    )
}

/// The status of `run` in the queue, `None` when it has no such run.
fn status(db: &Path, run: &RunId) -> Option<dagq::domain::RunStatus> {
    RunReads::run(&SqliteQueue::open(db).unwrap(), run)
        .map(|run| run.status())
        .ok()
}

fn issue(
    log: &SqliteQueue,
    db: &Path,
    run: &RunId,
    task: TaskId,
    under: &str,
) -> std::path::PathBuf {
    issue_run(
        &SystemServiceAccess,
        log,
        db,
        (run, task),
        &LeaseToken::new(under),
        &mut Retries::default(),
    )
    .unwrap()
}

fn lost(log: &SqliteQueue, db: &Path, run: &RunId, under: &str) -> bool {
    revoke_run(
        &SystemServiceAccess,
        log,
        db,
        run,
        TokenRevocation::LeaseLost,
        Some(&LeaseToken::new(under)),
    )
    .unwrap()
}

fn end_run(db: &Path, run: &RunId) {
    rusqlite::Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='failed' WHERE id=?1",
            [run.as_str()],
        )
        .unwrap();
}

/// The claim's token is refused once a resume issued another; the
/// resume's once the supervisor that adopted the run issued its own, which
/// the late revocation of the supervisor that lost the lease leaves alone;
/// the adopter's once it lost the lease in turn; and the last once the run
/// ended and the sweep revoked it. Each issue and revocation is on the run,
/// in order, and none of the late revocation that revoked nothing.
#[test]
fn a_runs_token_is_refused_after_a_resume_an_adoption_a_lost_lease_and_the_runs_end() {
    let queue = queue();
    start(&queue);
    let (db, dir) = (queue.db.as_path(), queue.dir());
    let (run, task) = (queue.run.id(), queue.run.task_id());
    let target = StepTarget::run(run);
    SqliteQueue::open(db)
        .unwrap()
        .register_session(&target, "s1", 1)
        .unwrap();
    let log = SqliteQueue::open(db).unwrap();
    let read = |file: &Path| service::read_token(file).unwrap();

    let claim_file = issue(&log, db, run, task, "a");
    let claimed = read(&claim_file);
    assert_eq!(send(dir, &claimed, &target), None);
    // The file is the queue's, outside the run directory and the worktree.
    assert!(claim_file.starts_with(service::service_dir(dir)));
    for inside in [queue.run.run_dir(), queue.run.worktree_path()]
        .into_iter()
        .flatten()
    {
        assert!(!claim_file.starts_with(inside), "{}", claim_file.display());
    }

    // A resume: the same file holds another token, and the claim's is no
    // token any more.
    let resume_file = issue(&log, db, run, task, "a");
    assert_eq!(resume_file, claim_file);
    let resumed = read(&resume_file);
    assert_ne!(resumed, claimed);
    let refused = send(dir, &claimed, &target).unwrap();
    assert!(refused.contains("unauthenticated"), "{refused}");
    assert_eq!(last_refusal(db), "unknown_token");
    assert_eq!(send(dir, &resumed, &target), None);

    // Supervisor b adopts the run before a sees its lease lost: a's late
    // revocation takes nothing of b's.
    let adopted = read(&issue(&log, db, run, task, "b"));
    assert!(send(dir, &resumed, &target).is_some());
    assert!(!lost(&log, db, run, "a"));
    assert_eq!(send(dir, &adopted, &target), None);
    assert_eq!(
        SystemServiceAccess.held_telemetry(db).unwrap(),
        [HeldTelemetry {
            target: target.clone(),
            issued_under: Some(LeaseToken::new("b")),
        }]
    );

    // Then b loses the lease.
    assert!(lost(&log, db, run, "b"));
    assert!(send(dir, &adopted, &target).is_some());
    assert_eq!(last_refusal(db), "unknown_token");
    assert_eq!(SystemServiceAccess.telemetry_credential(db, &target), None);

    // The supervisor that takes the run over next issues it another, which
    // the run's end and the sweep revoke.
    let last = read(&issue(&log, db, run, task, "c"));
    assert_eq!(send(dir, &last, &target), None);
    assert_eq!(swept(&log, db), Vec::<RunId>::new());
    end_run(db, run);
    assert_eq!(swept(&log, db), std::slice::from_ref(run));
    assert!(send(dir, &last, &target).is_some());
    assert_eq!(last_refusal(db), "unknown_token");
    assert_eq!(SystemServiceAccess.held_telemetry(db).unwrap(), []);
    assert_eq!(
        token_events(db, run),
        [
            issued(),
            revoked(TokenRevocation::Reissued),
            issued(),
            revoked(TokenRevocation::Reissued),
            issued(),
            revoked(TokenRevocation::LeaseLost),
            issued(),
            revoked(TokenRevocation::RunEnded),
        ]
    );
}

/// A sweep whose revocation fails on the host (the token's file cannot be
/// read; its record cannot be removed) records nothing and leaves the
/// token issued (the service refuses it for the run's end only) and
/// listed; once the failure is gone, the next sweep revokes it and records
/// it once.
#[test]
fn a_revocation_that_fails_is_recorded_only_after_a_later_sweep_revoked_it() {
    let queue = queue();
    start(&queue);
    let (db, dir) = (queue.db.as_path(), queue.dir());
    let run = queue.run.id();
    let target = StepTarget::run(run);
    let log = SqliteQueue::open(db).unwrap();
    let file = issue(&log, db, run, queue.run.task_id(), "a");
    let token = service::read_token(&file).unwrap();
    end_run(db, run);
    let still_issued = || {
        assert_eq!(
            SystemServiceAccess.held_telemetry(db).unwrap(),
            [HeldTelemetry {
                target: target.clone(),
                issued_under: Some(LeaseToken::new("a")),
            }]
        );
        assert!(send(dir, &token, &target).is_some());
        assert_eq!(last_refusal(db), "run_ended");
        assert_eq!(token_events(db, run), [issued()]);
    };

    // The token's file cannot be read.
    common::set_mode(&file, 0o000);
    if fs::read(&file).is_ok() {
        // A process that permissions do not hold back (root) cannot make
        // the host fail here.
        eprintln!("the permissions do not hold this process back; no failure can be injected");
        return;
    }
    assert_eq!(swept(&log, db), Vec::<RunId>::new());
    common::set_mode(&file, 0o600);
    still_issued();

    // The token's record cannot be removed.
    let records = service::service_dir(dir).join(service::TOKENS_DIR);
    common::set_mode(&records, 0o500);
    assert_eq!(swept(&log, db), Vec::<RunId>::new());
    common::set_mode(&records, 0o700);
    still_issued();
    assert!(file.is_file());

    assert_eq!(swept(&log, db), std::slice::from_ref(run));
    assert!(send(dir, &token, &target).is_some());
    assert_eq!(last_refusal(db), "unknown_token");
    assert!(!file.exists());
    assert_eq!(swept(&log, db), Vec::<RunId>::new());
    assert_eq!(
        token_events(db, run),
        [issued(), revoked(TokenRevocation::RunEnded)]
    );
}

/// The tokens held are listed by their records, each with the lease it was
/// issued under; an agent's token is no telemetry target. A record left
/// without its file still authenticates and is listed, and the revocation
/// of its target removes it; a file whose value has no record (an issue
/// that failed after it) is no token, and revoking it revokes none.
#[test]
fn a_telemetry_token_is_listed_until_every_record_of_it_is_revoked() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let run = RunId::new("r1").unwrap();
    let target = StepTarget::run(&run);
    let access = SystemServiceAccess;
    assert_eq!(access.held_telemetry(&db).unwrap(), []);
    assert_eq!(access.telemetry_credential(&db, &target), None);
    let file = access
        .issue_telemetry(
            &db,
            &target,
            Some(TaskId::new(3)),
            Some(&LeaseToken::new("a")),
        )
        .unwrap();
    service::issue(dir.path(), &Principal::worker(&run, TaskId::new(3)), 1).unwrap();
    let held = HeldTelemetry {
        target: target.clone(),
        issued_under: Some(LeaseToken::new("a")),
    };
    assert_eq!(
        access.held_telemetry(&db).unwrap(),
        std::slice::from_ref(&held)
    );
    assert_eq!(
        access.telemetry_credential(&db, &target),
        Some(file.clone())
    );
    let token = service::read_token(&file).unwrap();
    // Its file gone, its record still authenticates and is listed.
    fs::remove_file(&file).unwrap();
    assert!(service::lookup(dir.path(), &token).unwrap().is_some());
    assert_eq!(
        access.held_telemetry(&db).unwrap(),
        std::slice::from_ref(&held)
    );
    assert!(
        !access
            .revoke_telemetry(&db, &target, Some(&LeaseToken::new("b")))
            .unwrap()
    );
    assert!(access.revoke_telemetry(&db, &target, None).unwrap());
    assert_eq!(service::lookup(dir.path(), &token).unwrap(), None);
    assert_eq!(access.held_telemetry(&db).unwrap(), []);
    assert!(!access.revoke_telemetry(&db, &target, None).unwrap());
    assert_eq!(service::principals(dir.path()).unwrap().len(), 1);
    let left = service::credential_path(dir.path(), &dagq::domain::telemetry::actor_id(&target));
    fs::write(&left, "f".repeat(64)).unwrap();
    assert_eq!(access.telemetry_credential(&db, &target), None);
    assert!(!access.revoke_telemetry(&db, &target, None).unwrap());
    assert!(!left.exists());
}

/// A resume whose revocation of the token before fails (its file cannot be
/// read) issues none and records nothing, and the token before still
/// authenticates; the supervisor holds the revocation, keeps it in its file
/// when it execs for a handoff, and the process after the exec (the same
/// registration and lease) takes it over: its sweep, once the file can be
/// read, revokes the token before, records it and issues the run another.
#[test]
fn a_resumes_revocation_that_fails_is_made_by_the_next_sweep() {
    let queue = queue();
    start(&queue);
    let (db, dir) = (queue.db.as_path(), queue.dir());
    let (run, task) = (queue.run.id(), queue.run.task_id());
    let target = StepTarget::run(run);
    SqliteQueue::open(db)
        .unwrap()
        .register_session(&target, "s1", 1)
        .unwrap();
    let log = SqliteQueue::open(db).unwrap();
    let lease = LeaseToken::new("a");
    let mut retries = Retries::default();
    let file = issue_run(
        &SystemServiceAccess,
        &log,
        db,
        (run, task),
        &lease,
        &mut retries,
    )
    .unwrap();
    let claimed = service::read_token(&file).unwrap();
    let sweep_of_a = |retries: &mut Retries| {
        sweep(
            &SystemServiceAccess,
            &log,
            db,
            &lease,
            retries,
            &|run, _| {
                Ok(RunTokenState {
                    status: status(db, run),
                    lease: Some(LeaseView {
                        token: LeaseToken::new("a"),
                        stale: false,
                    }),
                    issuer_alive: true,
                })
            },
        )
    };

    common::set_mode(&file, 0o000);
    if fs::read(&file).is_ok() {
        // A process that permissions do not hold back (root) cannot make
        // the host fail here.
        eprintln!("the permissions do not hold this process back; no failure can be injected");
        return;
    }
    let resumed = issue_run(
        &SystemServiceAccess,
        &log,
        db,
        (run, task),
        &lease,
        &mut retries,
    );
    assert_eq!(resumed, None);
    assert!(retries.holds(run));
    assert_eq!(sweep_of_a(&mut retries), Vec::<RunId>::new());
    assert!(retries.holds(run));
    assert_eq!(send(dir, &claimed, &target), None);
    assert_eq!(token_events(db, run), [issued()]);

    // The exec of a handoff while the file still cannot be read.
    let kept = Retries::file(dir, &lease);
    retries.save(&LocalRunFiles, &kept, &lease).unwrap();
    drop(retries);
    let mut retries = Retries::take(&LocalRunFiles, &kept, &lease);
    assert!(!kept.exists());
    assert!(retries.holds(run));
    assert_eq!(send(dir, &claimed, &target), None);

    common::set_mode(&file, 0o600);
    assert_eq!(sweep_of_a(&mut retries), Vec::<RunId>::new());
    assert!(!retries.holds(run));
    let refused = send(dir, &claimed, &target).unwrap();
    assert!(refused.contains("unauthenticated"), "{refused}");
    assert_eq!(last_refusal(db), "unknown_token");
    let reissued = service::read_token(&file).unwrap();
    assert_ne!(reissued, claimed);
    assert_eq!(send(dir, &reissued, &target), None);
    assert_eq!(
        token_events(db, run),
        [issued(), revoked(TokenRevocation::Reissued), issued()]
    );
}

/// The supervisor that lost the run's lease (a) revokes its token while the
/// one that adopted the run (b) issues it another into the same file: a's
/// revocation holds the queue's lock from its read of the file to the
/// file's removal, so b's issue waits for it, and the file then holds b's
/// token, which a's revocation, late again, leaves as it is.
#[test]
fn a_late_revocation_never_removes_the_file_of_the_token_issued_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let target = StepTarget::run(&RunId::new("r1").unwrap());
    let actor = telemetry::actor_id(&target);
    let (a, b) = (LeaseToken::new("a"), LeaseToken::new("b"));
    SystemServiceAccess
        .issue_telemetry(&db, &target, Some(TaskId::new(3)), Some(&a))
        .unwrap();

    // a's revocation has begun: it holds the lock.
    let lock = service::TelemetryLock::take(dir.path()).unwrap();
    let (issued, waited) = mpsc::channel();
    let adopter = {
        let (db, target, b) = (db.clone(), target.clone(), b.clone());
        thread::spawn(move || {
            let file =
                SystemServiceAccess.issue_telemetry(&db, &target, Some(TaskId::new(3)), Some(&b));
            issued.send(()).unwrap();
            file
        })
    };
    assert!(
        waited.recv_timeout(Duration::from_millis(500)).is_err(),
        "b's issue waits for a's revocation"
    );
    assert!(service::revoke_under(&lock, dir.path(), &actor, Some(&a)).unwrap());
    drop(lock);
    let _waiting = crate::common::within(crate::common::STEP_LIMIT, "b's issue to end");
    let file = adopter.join().unwrap().unwrap();
    let token = service::read_token(&file).unwrap();
    assert!(service::lookup(dir.path(), &token).unwrap().is_some());
    assert!(
        !SystemServiceAccess
            .revoke_telemetry(&db, &target, Some(&a))
            .unwrap()
    );
    assert_eq!(
        SystemServiceAccess.telemetry_credential(&db, &target),
        Some(file)
    );
    assert_eq!(
        SystemServiceAccess.held_telemetry(&db).unwrap(),
        [HeldTelemetry {
            target,
            issued_under: Some(b),
        }]
    );
}
