//! An update job's own token (ADR-t4629-1) on the queue service's real use
//! cases and [`crate::domain::StaticPolicy`]: issued at a live supervisor's
//! request, it outlives that supervisor's sign-out and sends the job's
//! reads and steps (the supervisors' registrations, the install's handoff,
//! the update's ask), is refused the rest with the refusal recorded, and
//! ends when the job gives it back, or once its process is gone, at the
//! service's start and at a supervisor's sweep. Which issue is refused and
//! when a token ended is `domain::queue_service`'s `update_job_*`; these
//! hold the wiring.

use std::{
    path::PathBuf,
    time::{Duration, SystemTime},
};

use serde_json::{Value, json};

use super::{agent::Script, harness::Wiring, host::STOPPED_AT, service::MemoryBackend};
use crate::{
    application::queue_service::{Peer, QueueService, ServiceBackend, revoke_ended_update_jobs},
    domain::{
        LeaseToken,
        queue_service::{
            API_VERSION, ControlCall, ControlCommand, Principal, ServiceErrorCode, ServiceRequest,
            ServiceResponse, TokenIssuer, UseCase,
        },
    },
};

/// `use_case` with `params` sent to `backend`'s service with `token`, from
/// the process `process`.
fn send(
    backend: &MemoryBackend,
    token: &str,
    use_case: UseCase,
    params: Value,
    process: u32,
) -> ServiceResponse {
    QueueService {
        backend,
        build: "wiring",
        pid: 1,
    }
    .handle_from_process(
        &ServiceRequest {
            caller: None,
            api_version: API_VERSION,
            token: Some(token.to_owned()),
            env_role: None,
            queue: None,
            binding: None,
            use_case,
            params,
        },
        Peer::Other,
        Some(process),
    )
}

/// The step `op` with `args` of the automatic update's job, process `pid`.
fn job_step(op: &str, args: Value, pid: u32) -> Value {
    control_step(ControlCommand::AutoUpdate, op, args, pid)
}

fn control_step(command: ControlCommand, op: &str, args: Value, pid: u32) -> Value {
    json!({
        "control": ControlCall { command, task: None, yes: false, pid },
        "call": {"op": op, "args": args},
    })
}

fn supervisor_op(op: &str, args: Value) -> Value {
    json!({"op": op, "args": args})
}

/// The code a refused response names.
fn refused(response: &ServiceResponse) -> ServiceErrorCode {
    assert!(!response.ok, "{response:?}");
    response.error.as_ref().expect("an error").code
}

struct Test {
    wiring: Wiring,
    backend: std::sync::Arc<MemoryBackend>,
    lease: LeaseToken,
    pid: u32,
}

impl Test {
    fn new(name: &str) -> Self {
        let wiring = Wiring::new(
            name,
            Script {
                receipts: Vec::new(),
                verdicts: Vec::new(),
            },
        );
        let lease = LeaseToken::new("update-parent");
        wiring.register_at(&lease, SystemTime::UNIX_EPOCH + STOPPED_AT);
        let backend = wiring.service_backend();
        Self {
            wiring,
            backend,
            lease,
            pid: std::process::id(),
        }
    }

    /// The value of a token whose file is `file`.
    fn read(&self, file: &std::path::Path) -> String {
        self.wiring.tokens().read(file).expect("read the token")
    }

    /// The supervisor's sign-in, as the user's `up` has it issued.
    fn sign_in(&self, lease: &LeaseToken) -> String {
        let file = self
            .backend
            .issue_as(
                &Principal::supervisor(lease, self.pid),
                &TokenIssuer::of(&Principal::user(), self.backend.process_of(self.pid)),
            )
            .expect("sign the supervisor in");
        self.read(&file)
    }

    /// Ask, as the supervisor of `token`, for the token of the update job
    /// `job`.
    fn issue(&self, token: &str, job: &str) -> ServiceResponse {
        send(
            &self.backend,
            token,
            UseCase::Supervisor,
            supervisor_op("issue_token", json!([Principal::update_job(job)])),
            self.pid,
        )
    }

    fn issued(&self, token: &str, job: &str) -> String {
        let issued = self.issue(token, job);
        assert!(issued.ok, "{issued:?}");
        let file: PathBuf =
            serde_json::from_value(issued.result.expect("issued")["file"].clone()).unwrap();
        self.read(&file)
    }

    /// The job's binding of its own token `job` to its process `pid`.
    fn bind(&self, job: &str, pid: u32) -> ServiceResponse {
        send(
            &self.backend,
            job,
            UseCase::Control,
            job_step("bind_job_token", json!([]), pid),
            pid,
        )
    }

    /// The job's read of the supervisors' registrations, from `pid`.
    fn read_supervisors(&self, job: &str, pid: u32) -> ServiceResponse {
        send(
            &self.backend,
            job,
            UseCase::Control,
            job_step("supervisors", json!([]), pid),
            pid,
        )
    }

    /// The `authorization_denied` the queue recorded of an update job.
    fn denials(&self) -> Vec<Value> {
        let connection = super::harness::connect(&self.wiring.db);
        let mut statement = connection
            .prepare(
                "SELECT payload FROM run_events WHERE kind = 'authorization_denied' \
                 AND actor_role = 'updater' ORDER BY id",
            )
            .unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
            .collect()
    }
}

#[test]
fn an_update_job_sends_past_its_supervisors_sign_out_until_it_gives_its_token_back() {
    let test = Test::new("an_update_job_sends_past_its_supervisors_sign_out");
    let supervisor = test.sign_in(&test.lease);
    // A supervisor with no registration is refused, recorded as not held.
    let unregistered = test.sign_in(&LeaseToken::new("not-registered"));
    let refusal = test.issue(&unregistered, "update-1");
    assert_eq!(refused(&refusal), ServiceErrorCode::AuthorizationDenied);
    // A job named so that the sweeps could not revoke its token is
    // refused before anything is issued.
    assert_eq!(
        refused(&test.issue(&supervisor, "update@1")),
        ServiceErrorCode::BadRequest
    );
    assert!(
        test.backend
            .principals()
            .unwrap()
            .iter()
            .all(|principal| { principal.role != crate::domain::ActorRole::Updater })
    );
    // The same supervisor's issue again replaces the token before.
    let before = test.issued(&supervisor, "update-1");
    let job = test.issued(&supervisor, "update-1");
    assert_ne!(before, job);
    let (job_pid, _) = test.wiring.processes().start();
    assert_eq!(
        refused(&test.read_supervisors(&before, job_pid)),
        ServiceErrorCode::Unauthenticated
    );
    // The job binds its process first, which the service answers only
    // once it kept it: a binding that could not be written is refused,
    // nothing bound; again from it is answered so, and from another
    // process, or with a token not kept, refused.
    *test.backend.on_bind.lock().unwrap() = Some(Box::new(|| {
        Err(anyhow::anyhow!("the binding's file could not be written"))
    }));
    assert_eq!(refused(&test.bind(&job, job_pid)), ServiceErrorCode::Failed);
    let bound = test.bind(&job, job_pid);
    assert_eq!(bound.result, Some(json!(true)), "{bound:?}");
    assert_eq!(test.bind(&job, job_pid).result, Some(json!(true)));
    let (child, _) = test.wiring.processes().start();
    assert_eq!(refused(&test.bind(&job, child)), ServiceErrorCode::Failed);
    assert_eq!(
        refused(&test.bind(&before, job_pid)),
        ServiceErrorCode::Unauthenticated
    );
    let read = test.read_supervisors(&job, job_pid);
    assert!(read.ok, "{read:?}");
    // Another live supervisor is refused the token of that job.
    let other = LeaseToken::new("other-parent");
    test.wiring
        .register_at(&other, SystemTime::UNIX_EPOCH + STOPPED_AT);
    let others = test.sign_in(&other);
    assert_eq!(
        refused(&test.issue(&others, "update-1")),
        ServiceErrorCode::AuthorizationDenied
    );

    // Nor may it revoke that token while the job runs.
    let revoked = send(
        &test.backend,
        &others,
        UseCase::Supervisor,
        supervisor_op("revoke_token", json!(["updater:update-1"])),
        test.pid,
    );
    assert_eq!(refused(&revoked), ServiceErrorCode::AuthorizationDenied);
    assert!(test.read_supervisors(&job, job_pid).ok);

    // The supervisor that asked for it signs out: its own token goes.
    let signed_out = send(
        &test.backend,
        &supervisor,
        UseCase::Supervisor,
        supervisor_op("sign_out", json!([])),
        test.pid,
    );
    assert!(signed_out.ok, "{signed_out:?}");
    assert_eq!(
        refused(&send(
            &test.backend,
            &supervisor,
            UseCase::Supervisor,
            supervisor_op("supervisors", json!([])),
            test.pid,
        )),
        ServiceErrorCode::Unauthenticated
    );
    // The job's goes on: its reads, the install's handoff and its ask.
    let read = test.read_supervisors(&job, job_pid);
    assert!(read.ok, "{read:?}");
    let handoff = send(
        &test.backend,
        &job,
        UseCase::Control,
        job_step(
            "request_handoff",
            json!([test.lease, "/usr/local/bin/dagq"]),
            job_pid,
        ),
        job_pid,
    );
    assert!(handoff.ok, "{handoff:?}");
    let ask = send(
        &test.backend,
        &job,
        UseCase::Control,
        job_step(
            "open_update_ask",
            json!([
                "update_failed",
                "the update failed",
                ["retry", "skip"],
                "supervisor",
                null,
                null
            ]),
            job_pid,
        ),
        job_pid,
    );
    assert!(ask.ok, "{ask:?}");

    // Refused what it was not granted, each recorded: the supervisor's
    // use case (claims, leases, landings, its start), a token's issue and
    // `up`.
    let denied = test.denials().len();
    for response in [
        send(
            &test.backend,
            &job,
            UseCase::Supervisor,
            supervisor_op("supervisors", json!([])),
            job_pid,
        ),
        send(
            &test.backend,
            &job,
            UseCase::IssueToken,
            json!({"principal": Principal::update_job("update-9")}),
            job_pid,
        ),
        send(
            &test.backend,
            &job,
            UseCase::Control,
            control_step(ControlCommand::Up, "supervisors", json!([]), job_pid),
            job_pid,
        ),
        // A step of its own command that is no update's or install's: a
        // landing's, a token's issue.
        send(
            &test.backend,
            &job,
            UseCase::Control,
            job_step("begin_landing", json!([]), job_pid),
            job_pid,
        ),
        send(
            &test.backend,
            &job,
            UseCase::Control,
            job_step(
                "issue_token",
                json!([Principal::update_job("update-9")]),
                job_pid,
            ),
            job_pid,
        ),
    ] {
        assert_eq!(refused(&response), ServiceErrorCode::AuthorizationDenied);
    }
    let denials = test.denials();
    assert_eq!(denials.len(), denied + 5, "{denials:?}");
    let capabilities: Vec<&str> = denials[denied..]
        .iter()
        .map(|denial| denial["capability"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        capabilities,
        [
            "scheduler.supervise",
            "token.issue",
            "service.lifecycle",
            "service.install",
            "service.install"
        ]
    );
    for (denial, op) in denials[denied + 3..]
        .iter()
        .zip(["begin_landing", "issue_token"])
    {
        assert_eq!(denial["reason"], "not on this resource", "{denial}");
        assert!(
            denial.to_string().contains(op),
            "the refusal names {op}: {denial}"
        );
    }
    let denied = denied + 2;
    // Its `queue.admin` is for the new build's check and migrations: the
    // queue's binding to a repository, by `rebind` or `init`, is refused
    // too, recorded as the job's, and leaves the binding as it was.
    let bound = test.backend.binding().unwrap();
    for (use_case, params) in [
        (
            UseCase::Rebind,
            json!({"git_common_dir": "/elsewhere/.git"}),
        ),
        (UseCase::Init, json!({})),
    ] {
        let response = send(&test.backend, &job, use_case, params, job_pid);
        assert_eq!(
            refused(&response),
            ServiceErrorCode::AuthorizationDenied,
            "{use_case:?}"
        );
    }
    let denials = test.denials();
    assert_eq!(denials.len(), denied + 5, "{denials:?}");
    for denial in &denials[denied + 3..] {
        assert_eq!(denial["capability"], "queue.admin", "{denial}");
        assert_eq!(denial["reason"], "not on this resource", "{denial}");
    }
    assert_eq!(test.backend.binding().unwrap(), bound);

    // Another process's give-back, as one whose bind was refused sends,
    // leaves the job's token to it.
    let not_its = send(
        &test.backend,
        &job,
        UseCase::Control,
        job_step("return_job_token", json!([]), child),
        child,
    );
    assert_eq!(not_its.result, Some(json!(false)), "{not_its:?}");
    let read = test.read_supervisors(&job, job_pid);
    assert!(read.ok, "{read:?}");
    // The job gives its token back at its end: refused from then on.
    let returned = send(
        &test.backend,
        &job,
        UseCase::Control,
        job_step("return_job_token", json!([]), job_pid),
        job_pid,
    );
    assert_eq!(returned.result, Some(json!(true)), "{returned:?}");
    assert_eq!(
        refused(&test.read_supervisors(&job, job_pid)),
        ServiceErrorCode::Unauthenticated
    );
}

#[test]
fn an_update_jobs_token_is_swept_once_its_process_is_gone_and_not_before() {
    let test = Test::new("an_update_jobs_token_is_swept_once_its_process_is_gone");
    let supervisor = test.sign_in(&test.lease);
    let at_start = test.issued(&supervisor, "update-1");
    let swept = test.issued(&supervisor, "update-2");
    let (first, _) = test.wiring.processes().start();
    let (second, _) = test.wiring.processes().start();
    for (token, pid) in [(&at_start, first), (&swept, second)] {
        let bound = test.bind(token, pid);
        assert_eq!(bound.result, Some(json!(true)), "{bound:?}");
    }
    // While the jobs run, neither sweep revokes their tokens, the
    // supervisor that asked for them gone or not.
    let signed_out = send(
        &test.backend,
        &supervisor,
        UseCase::Supervisor,
        supervisor_op("sign_out", json!([])),
        test.pid,
    );
    assert!(signed_out.ok, "{signed_out:?}");
    assert!(revoke_ended_update_jobs(&*test.backend).unwrap().is_empty());
    let sweeper = test.sign_in(&test.lease);
    let revoke = |job: &str| {
        send(
            &test.backend,
            &sweeper,
            UseCase::Supervisor,
            supervisor_op("revoke_principal", json!([Principal::update_job(job)])),
            test.pid,
        )
    };
    assert_eq!(revoke("update-2").result, Some(json!(false)));
    for (token, pid) in [(&at_start, first), (&swept, second)] {
        assert!(test.read_supervisors(token, pid).ok);
    }

    // The service's start sweeps the one whose process is gone.
    test.wiring.processes().end(first);
    assert_eq!(
        revoke_ended_update_jobs(&*test.backend).unwrap(),
        ["updater:update-1"]
    );
    assert_eq!(
        refused(&test.read_supervisors(&at_start, first)),
        ServiceErrorCode::Unauthenticated
    );
    // A supervisor's periodic sweep likewise.
    test.wiring.processes().end(second);
    assert_eq!(revoke("update-2").result, Some(json!(true)));
    assert_eq!(
        refused(&test.read_supervisors(&swept, second)),
        ServiceErrorCode::Unauthenticated
    );
}

/// Whether the queue's write lock is held: another connection to `db`
/// cannot begin a write, which it does not wait for.
fn write_lock_held(db: &std::path::Path) -> bool {
    let other = super::harness::connect(db);
    other.busy_timeout(Duration::ZERO).unwrap();
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK;").is_err()
}

/// The job's binding and the sweeps of the update jobs' tokens exclude
/// each other: each holds the queue's write lock while it binds, or while
/// it reads, judges and removes, so no sweep judges a token unbound that
/// was bound since it read it. Then, in either order, with the service's
/// clock past the time an unbound token is kept (no wait): a token bound
/// first is kept by the sweep while its job's process runs, and one the
/// sweep revoked first, unbound, is refused the job's bind after it.
#[test]
fn an_update_jobs_bind_and_the_sweeps_exclude_each_other() {
    use std::sync::mpsc;
    let test = Test::new("an_update_jobs_bind_and_the_sweeps_exclude_each_other");
    let supervisor = test.sign_in(&test.lease);
    let bound = test.issued(&supervisor, "update-1");
    let unbound = test.issued(&supervisor, "update-2");
    let (job_pid, _) = test.wiring.processes().start();
    let (look, held) = mpsc::channel();
    let looked = |held: &mpsc::Receiver<bool>| held.try_recv().expect("the look ran");

    // The bind writes under the write lock.
    let (db, sent) = (test.wiring.db.clone(), look.clone());
    *test.backend.on_bind.lock().unwrap() = Some(Box::new(move || {
        sent.send(write_lock_held(&db)).unwrap();
        Ok(())
    }));
    let answered = test.bind(&bound, job_pid);
    assert_eq!(answered.result, Some(json!(true)), "{answered:?}");
    assert!(looked(&held), "the bind wrote outside a write transaction");

    // A supervisor's sweep reads the records under it.
    let (db, sent) = (test.wiring.db.clone(), look.clone());
    *test.backend.on_held_records.lock().unwrap() = Some(Box::new(move || {
        sent.send(write_lock_held(&db)).unwrap();
    }));
    let swept = send(
        &test.backend,
        &supervisor,
        UseCase::Supervisor,
        supervisor_op(
            "revoke_principal",
            json!([Principal::update_job("update-1")]),
        ),
        test.pid,
    );
    assert_eq!(swept.result, Some(json!(false)), "{swept:?}");
    assert!(looked(&held), "the sweep read outside a write transaction");

    // The service's start, past the unbound token's limit: it keeps the
    // bound one and revokes the unbound one, reading under the lock.
    let limit = u64::try_from(crate::domain::queue_service::UPDATE_JOB_UNBOUND_SECS).unwrap();
    let later = test
        .wiring
        .service_backend_at(SystemTime::UNIX_EPOCH + STOPPED_AT + Duration::from_secs(limit + 1));
    let (db, sent) = (test.wiring.db.clone(), look);
    *later.on_held_records.lock().unwrap() = Some(Box::new(move || {
        sent.send(write_lock_held(&db)).unwrap();
    }));
    assert_eq!(
        revoke_ended_update_jobs(&*later).unwrap(),
        ["updater:update-2"]
    );
    assert!(
        looked(&held),
        "the start's sweep read outside a write transaction"
    );
    assert!(test.read_supervisors(&bound, job_pid).ok);
    // The job of the token swept first does not go on.
    let (late, _) = test.wiring.processes().start();
    assert_eq!(
        refused(&test.bind(&unbound, late)),
        ServiceErrorCode::Unauthenticated
    );
}
