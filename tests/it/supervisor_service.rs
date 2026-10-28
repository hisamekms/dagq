//! The supervisor's queue through the queue service (ADR-t3102-3
//! decisions 3 to 5, docs/design/queue-service.md): its registration, the
//! claim, the leases, a run's transitions and events, the reads it decides
//! on and the tokens it issues reach the queue only through the service's
//! socket, as the supervisor principal's, and a service that does not
//! answer is no reason to open the queue instead. So do the landing, the
//! review and validation records, the jobs' starts and results, the asks
//! and the attention of a whole supervisor, and the job commands it starts
//! as its own: each test refuses the queue to its own process
//! ([`refuse_opening`]) while the service runs in another.

use crate::common;
use crate::runtime_support::losing_answers::LosingAnswers;
use crate::runtime_support::{e2e_options, fixture, head_commit as head, with_e2e_paths};
use common::Bounded;

use std::{
    path::Path,
    process::Command,
    sync::{Arc, atomic::Ordering},
};

use dagq::{
    application::{
        QueueOpener,
        queue_service::ServiceAccess,
        supervisor_service::{
            ServiceCaller, ServiceTokens, ServiceUnreachable, SupervisorQueues,
            resend::{Resend, Resending},
            unreachable,
        },
    },
    domain::{
        ActorContext, ActorRole, ClaimOutcome, LeaseToken, Reason, ReasonCode,
        queue_service::Principal,
        slot_limits::{SlotFlags, SlotLimits, SupervisorConfig},
    },
    infrastructure::{
        queue_service::{SocketCaller, SystemServiceAccess},
        sqlite::refuse_opening,
    },
};

/// The registration, the claim, the leases' heartbeat, a run's
/// transition, its events, the reads of the candidates, the task, the
/// graph and the runs, and
/// the issue and the revocation of the run's worker's token all go through
/// the service: the queue is never opened in this process. Once the
/// service stops, the next operation fails as out of reach, still without
/// opening the queue.
#[test]
fn the_supervisors_operations_reach_the_queue_only_through_the_service() {
    let (_fixture, repo, db) = fixture();
    let served = common::service::Served::start(&db);
    let pid = 4242;
    let caller: Arc<dyn ServiceCaller> = Arc::new(SocketCaller::new(&db));
    let shared = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    );
    let queues = || shared.clone();
    let refused = refuse_opening(&db);
    let token = LeaseToken::new("supervisor-under-test");
    caller.sign_in(&token, pid).unwrap();
    let mut queue = queues().open().unwrap();

    let limits = SlotLimits::resolve(SlotFlags::default(), &SupervisorConfig::default());
    queue
        .register_supervisor(&token, pid, limits, &[], "0.0.0-test")
        .unwrap();
    let candidates = queue.candidates().unwrap();
    assert_eq!(candidates.len(), 1);
    // The reads a claim decides on, the task whole.
    let task = candidates[0].id();
    assert_eq!(
        queue.show(task).unwrap().task.paths(),
        candidates[0].paths()
    );
    assert_eq!(queue.graph_input().unwrap().candidates, [task]);
    queue.ready_at(&[task]).unwrap();
    queue.investigations().unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&head(&repo), &token).unwrap()
    else {
        panic!("no task was claimed");
    };
    let run = *run;
    assert!(queue.holds_lease(run.id(), &token).unwrap());
    assert_eq!(queue.heartbeat_leases(&token).unwrap(), 1);
    assert!(queue.heartbeat(&token).unwrap().registered);
    queue
        .record_runtime_error(
            run.id(),
            "checked through the service",
            &Reason::from(ReasonCode::LeaseLost),
        )
        .unwrap();
    assert_eq!(queue.active_runs().unwrap().len(), 1);
    let events = queue.run_events(run.id()).unwrap();
    let claimed = events
        .iter()
        .find(|event| event.kind == dagq::domain::EventKind::RunClaimed)
        .unwrap();
    assert_eq!(
        claimed.actor.as_ref().map(|actor| actor.id.as_str()),
        Some("supervisor:4242")
    );

    let tokens = ServiceTokens {
        queues: queues(),
        files: Arc::new(SystemServiceAccess),
        telemetry_files: Arc::new(SystemServiceAccess),
    };
    let worker = Principal::worker(run.id(), run.task_id());
    let file = tokens.issue(&db, &worker).unwrap().file;
    assert!(file.is_file());
    assert!(tokens.revoke(&db, &worker.actor_id).unwrap());

    drop(served);
    let error = queue.active_runs().unwrap_err();
    assert!(unreachable(&error), "{error:#}");
    drop(refused);
    caller.sign_out();
}

/// A whole supervisor on the service's socket claims a task, validates and
/// reviews its run, records the review's job and lands it as the
/// integrator, while its process refuses to open the queue: every record of
/// it, the landing's included, went through the service, written as the
/// same actors as before (the supervisor, the integrator at its request).
#[test]
fn a_supervisor_on_the_socket_reviews_and_lands_a_run_without_opening_the_queue() {
    use crate::runtime_support::{
        IDLE_AGENT, TestReviewer, TestWorkspace, event_kinds, position, supervise_options,
        supervise_reviewed_with, verdict,
    };
    use dagq::{
        application::TaskReads,
        domain::{EventKind, TaskId, TaskStatus},
        infrastructure::sqlite::SqliteQueue,
    };
    let (_fixture, repo, db) = fixture();
    let _served = common::service::Served::start(&db);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let options = dagq::compose::SuperviseOptions {
        service_route: dagq::compose::ServiceRoute::Socket,
        ..supervise_options(4, true)
    };
    let refused = refuse_opening(&db);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    drop(refused);
    assert_eq!(outcome["errors"], serde_json::json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");

    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let kinds = event_kinds(&detail);
    for (earlier, later) in [
        ("validation_finished", "review_started"),
        ("review_started", "review_finished"),
        ("review_finished", "integration_started"),
        ("integration_started", "run_integrated"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    let actor_of = |kind: EventKind| {
        let event = detail
            .events
            .iter()
            .find(|event| event.kind == kind)
            .unwrap_or_else(|| panic!("no {kind:?}"));
        let actor = event.actor.as_ref().unwrap();
        (actor.role.clone(), actor.requested_by.clone())
    };
    let supervisor = format!("supervisor:{}", std::process::id());
    assert_eq!(actor_of(EventKind::ReviewFinished).0, "supervisor");
    assert_eq!(actor_of(EventKind::ValidationFinished).0, "supervisor");
    let (role, requested_by) = actor_of(EventKind::RunIntegrated);
    assert_eq!(role, "integrator");
    assert_eq!(requested_by.as_deref(), Some(supervisor.as_str()));
    // The integrator's token, issued as the slot was taken, was revoked as
    // the landing left `integrating`.
    let landing = dagq::domain::queue_service::Principal::integrator(
        detail.runs[0].id(),
        &LeaseToken::new("any"),
        std::process::id(),
    );
    let credential = dagq::infrastructure::queue_service::credential_path(
        db.parent().unwrap(),
        &landing.credential_key(),
    );
    assert!(!credential.exists(), "{}", credential.display());
}

/// With `[ci_watch]` and a `[landing_verification]` command in place of
/// the task's, the landing the supervisor asks of the integrator on the
/// socket reads the CI watch's records the queue holds, as the
/// integrator, before its verification: the read is the integrator's, so
/// the landing moves main and records `run_integrated`, and its command
/// is told the known failures.
#[test]
fn a_landing_on_the_socket_reads_the_ci_watch_as_the_integrator() {
    use crate::runtime_support::{
        IDLE_AGENT, TestReviewer, TestWorkspace, git, record_known_failure, supervise_options,
        supervise_reviewed_with, verdict,
    };
    use dagq::{
        application::TaskReads,
        domain::{EventKind, TaskId, TaskStatus},
        infrastructure::sqlite::SqliteQueue,
    };
    let (_fixture, repo, db) = fixture();
    let listed = repo.parent().unwrap().join("known failures");
    std::fs::write(
        repo.join("dagq.toml"),
        format!(
            "[ci_watch]\nworkflow = \"ci.yml\"\n\n[landing_verification]\nreplaces = ['seed.txt']\ncommand = {}\n",
            serde_json::json!(format!(
                "test -f seed.txt && cp \"$DAGQ_CI_KNOWN_FAILURES\" {}",
                common::shell_path(&listed)
            ))
        ),
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-q", "-m", "watch the CI"]);
    let before = head(&repo);
    record_known_failure(&db, &["dagq::it known::failure"]);
    let _served = common::service::Served::start(&db);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let options = dagq::compose::SuperviseOptions {
        service_route: dagq::compose::ServiceRoute::Socket,
        ..supervise_options(4, true)
    };
    let refused = refuse_opening(&db);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    drop(refused);
    assert_eq!(outcome["errors"], serde_json::json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");

    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let integrated = detail
        .events
        .iter()
        .find(|event| event.kind == EventKind::RunIntegrated)
        .expect("run_integrated");
    assert_eq!(integrated.actor.as_ref().unwrap().role, "integrator");
    assert_ne!(head(&repo), before);
    let list: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&listed).unwrap()).unwrap();
    assert_eq!(
        list["failures"][0]["name"], "dagq::it known::failure",
        "{list}"
    );
}

/// A job command the supervisor starts as its own (an observation)
/// reaches the queue through the service with the supervisor's token,
/// found from its registration, and opens none: its records are the
/// supervisor's, its agent's findings the observer's.
#[test]
fn a_job_command_of_the_supervisor_records_through_the_service() {
    use crate::runtime_support::service::JobOfSupervisor;
    let (_fixture, _repo, db) = fixture();
    let _served = common::service::Served::start(&db);
    let db = db.canonicalize().unwrap();
    let job = JobOfSupervisor::sign_in(&db);
    // The supervisor kept as registered, under which its agent's telemetry
    // token is issued.
    dagq::infrastructure::sqlite::SqliteQueue::open(&db)
        .unwrap()
        .register_supervisor(
            &LeaseToken::new("supervisor-of-the-job"),
            std::process::id(),
            1,
            "test",
        )
        .unwrap();
    let refused = refuse_opening(&db);
    let outcome = dagq::compose::observe_through(
        &db,
        job.queues.clone(),
        &crate::runtime_support::observer::ObserverProvider::new(
            "dagq finding record --kind capacity --queue --subject idle_slots --summary 'slots idle' > /dev/null",
        ),
        None,
        &crate::runtime_support::observer::observe_options(
            dagq::application::observer::ObserveMode::Hourly,
        ),
    )
    .unwrap();
    // The adopted token is the supervisor's: the job neither revokes it nor
    // signs in as another.
    let pid = std::process::id();
    assert!(
        job.queues
            .service
            .sign_in(&LeaseToken::new("another"), pid)
            .is_err()
    );
    job.queues.service.sign_out();
    let reads = job.queues.queue();
    assert!(
        dagq::application::RunReads::active_runs(&reads)
            .unwrap()
            .is_empty()
    );
    drop(refused);
    assert_eq!(outcome["outcome"], "succeeded", "{outcome}");
    assert_eq!(outcome["findings_recorded"], 1, "{outcome}");
    let finished = crate::runtime_support::observer::queue_event_actors(&db, "observe_finished");
    assert_eq!(
        finished,
        [("supervisor".to_owned(), format!("supervisor:{pid}"), None)]
    );
    // Its agent's telemetry token is issued to the supervisor whose command
    // started the observation (ADR-t3102-1 decision 2).
    assert_eq!(
        crate::runtime_support::observer::queue_event_actors(&db, "telemetry_token_issued").len(),
        1
    );
}

/// The `observe` command a supervisor starts as its own, named its
/// registration, opens no queue: given a path whose DB is no database but
/// whose service directory is the queue's, its dry run reads the queue
/// through the service as that supervisor.
#[test]
fn the_observe_command_of_a_supervisor_opens_no_queue() {
    use crate::runtime_support::service::JobOfSupervisor;
    let (_fixture, _repo, db) = fixture();
    let _served = common::service::Served::start(&db);
    let db = db.canonicalize().unwrap();
    let _job = JobOfSupervisor::sign_in(&db);
    // The queue's service and tokens beside a DB that does not open.
    let elsewhere = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(
        db.parent()
            .unwrap()
            .join(dagq::domain::queue_service::SERVICE_DIR),
        elsewhere
            .path()
            .join(dagq::domain::queue_service::SERVICE_DIR),
    )
    .unwrap();
    // A socket too long for its queue's directory is named by a hash of
    // that directory, not in it: the one named for `elsewhere` leads to the
    // queue's.
    struct Link(std::path::PathBuf);
    impl Drop for Link {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let socket = dagq::infrastructure::queue_service::socket_path(db.parent().unwrap());
    let named = dagq::infrastructure::queue_service::socket_path(elsewhere.path());
    let _link =
        (std::fs::canonicalize(&named).ok() != std::fs::canonicalize(&socket).ok()).then(|| {
            std::os::unix::fs::symlink(&socket, &named).unwrap();
            Link(named)
        });
    let unopenable = elsewhere.path().join(db.file_name().unwrap());
    std::fs::write(&unopenable, "not a database").unwrap();
    let launch = dagq::domain::actor_model::ActorLaunch::default_of(
        dagq::domain::actor_model::ModelRole::Observer,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .arg("--db")
        .arg(&unopenable)
        .args(["observe", "--dry-run", "--launch"])
        .arg(launch.to_value().to_string())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", elsewhere.path())
        .envs(ActorContext::instance(ActorRole::Supervisor, std::process::id()).env())
        .env(
            dagq::domain::queue_service::SUPERVISOR_REGISTRATION_ENV,
            "supervisor-of-the-job",
        )
        .bounded_output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let outcome: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["dry_run"], true, "{outcome}");
    assert_eq!(std::fs::read(&unopenable).unwrap(), b"not a database");
}

/// The calls of a landing, as the service gives them, but the first
/// request of each of `ops` the service did and whose answer is lost on
/// the way back (ADR-t3102-3 decision 5); a caller of the integrator's
/// token loses the landing's record and its event's.
struct LosesOneAnswer {
    inner: Arc<dyn ServiceCaller>,
    ops: &'static [&'static str],
    lost: std::sync::Mutex<Vec<String>>,
}

impl LosesOneAnswer {
    fn new(inner: Arc<dyn ServiceCaller>, ops: &'static [&'static str]) -> Self {
        Self {
            inner,
            ops,
            lost: std::sync::Mutex::default(),
        }
    }
}

impl ServiceCaller for LosesOneAnswer {
    fn call(
        &self,
        use_case: dagq::domain::queue_service::UseCase,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let op = params["op"].as_str().unwrap_or_default().to_owned();
        let answer = self.inner.call(use_case, params)?;
        let mut lost = self.lost.lock().unwrap();
        if self.ops.contains(&op.as_str()) && !lost.contains(&op) {
            lost.push(op);
            return Err(ServiceUnreachable::maybe_done("the answer was lost").into());
        }
        Ok(answer)
    }
    fn with_token(&self, file: &Path) -> anyhow::Result<Arc<dyn ServiceCaller>> {
        Ok(Arc::new(Self::new(
            self.inner.with_token(file)?,
            &["finish_integration", "record_runtime_event"],
        )))
    }
    fn send_held(&self) -> anyhow::Result<bool> {
        self.inner.send_held()
    }
    fn sign_in(&self, registration: &LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.sign_in(registration, pid)
    }
    fn take_back(&self, registration: &LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.take_back(registration, pid)
    }
    fn hand_over(&self) {
        self.inner.hand_over();
    }
    fn sign_out(&self) {
        self.inner.sign_out();
    }
    fn credential(&self) -> Option<std::path::PathBuf> {
        self.inner.credential()
    }
}

/// The calls of the supervisor, as the service gives them, but the first
/// request of `op`, which meets the queue busy past its wait and is not
/// done.
struct BusyOnce {
    inner: Arc<dyn ServiceCaller>,
    op: &'static str,
    met: std::sync::atomic::AtomicBool,
}

impl ServiceCaller for BusyOnce {
    fn call(
        &self,
        use_case: dagq::domain::queue_service::UseCase,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        if params["op"] == self.op && !self.met.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err(anyhow::anyhow!("locked").context(dagq::application::QueueBusy));
        }
        self.inner.call(use_case, params)
    }
    fn with_token(&self, file: &Path) -> anyhow::Result<Arc<dyn ServiceCaller>> {
        self.inner.with_token(file)
    }
    fn send_held(&self) -> anyhow::Result<bool> {
        self.inner.send_held()
    }
    fn sign_in(&self, registration: &LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.sign_in(registration, pid)
    }
    fn take_back(&self, registration: &LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.take_back(registration, pid)
    }
    fn hand_over(&self) {
        self.inner.hand_over();
    }
    fn sign_out(&self) {
        self.inner.sign_out();
    }
    fn credential(&self) -> Option<std::path::PathBuf> {
        self.inner.credential()
    }
}

/// The calls of the supervisor, as the service gives them, but the next
/// `left` requests that record a run's event of one of `kinds`, which meet
/// the queue busy past its wait and are not done.
struct BusyFor {
    inner: Arc<dyn ServiceCaller>,
    kinds: &'static [&'static str],
    left: Arc<std::sync::atomic::AtomicUsize>,
}

impl BusyFor {
    /// The layer of the supervisor's in-process route in this double, busy
    /// `times` times, and how many of them are left.
    fn layer(
        kinds: &'static [&'static str],
        times: usize,
    ) -> (
        dagq::compose::CallerLayer,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let left = Arc::new(std::sync::atomic::AtomicUsize::new(times));
        let counted = left.clone();
        let layer = dagq::compose::CallerLayer(Arc::new(move |inner| {
            Arc::new(Self {
                inner,
                kinds,
                left: counted.clone(),
            }) as Arc<dyn ServiceCaller>
        }));
        (layer, left)
    }
}

impl ServiceCaller for BusyFor {
    fn call(
        &self,
        use_case: dagq::domain::queue_service::UseCase,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        if params["op"] == "record_runtime_event"
            && self.kinds.iter().any(|kind| params["args"][1] == *kind)
            && self
                .left
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
        {
            return Err(anyhow::anyhow!("locked").context(dagq::application::QueueBusy));
        }
        self.inner.call(use_case, params)
    }
    fn with_token(&self, file: &Path) -> anyhow::Result<Arc<dyn ServiceCaller>> {
        self.inner.with_token(file)
    }
    fn send_held(&self) -> anyhow::Result<bool> {
        self.inner.send_held()
    }
    fn sign_in(&self, registration: &LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.sign_in(registration, pid)
    }
    fn take_back(&self, registration: &LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.take_back(registration, pid)
    }
    fn hand_over(&self) {
        self.inner.hand_over();
    }
    fn sign_out(&self) {
        self.inner.sign_out();
    }
    fn credential(&self) -> Option<std::path::PathBuf> {
        self.inner.credential()
    }
}

/// A landing whose record meets the service down is sent again once the
/// service is back, and one whose answer is lost is sent again and
/// recorded once, as is an event of the landing's whose answer is lost:
/// the run is integrated with one `run_integrated`, written as the
/// integrator principal the slot's use case issued, whose token the
/// landing's end revokes. The queue is never opened here.
#[test]
fn a_landings_record_is_sent_again_until_the_service_answers_and_recorded_once() {
    use dagq::application::{Landing, integrate::Integrator};
    use dagq::domain::{EventKind, RunStatus, TaskStatus};
    let (_fixture, repo, db) = fixture();
    let served = common::service::Served::start(&db);
    let pid = std::process::id();
    let caller: Arc<dyn ServiceCaller> = Arc::new(Resending::new(
        Arc::new(LosesOneAnswer::new(Arc::new(SocketCaller::new(&db)), &[])),
        Resend::Hold,
        dagq::infrastructure::clock::system(),
    ));
    let queues = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    );
    let token = LeaseToken::new("supervisor-of-the-landing");
    caller.sign_in(&token, pid).unwrap();
    let mut queue = queues.open().unwrap();
    let limits = SlotLimits::resolve(SlotFlags::default(), &SupervisorConfig::default());
    queue
        .register_supervisor(&token, pid, limits, &[], "0.0.0-test")
        .unwrap();
    let main = head(&repo);
    let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&main, &token).unwrap() else {
        panic!("no task was claimed");
    };
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration' WHERE id=?1",
            [run.id().as_str()],
        )
        .unwrap();
    let refused = refuse_opening(&db);
    let landing_run = queue.begin_integration(run.id(), &token, &main).unwrap();
    assert_eq!(landing_run.status(), RunStatus::Integrating);

    let mut landing_queue = queues.open().unwrap();
    let integrator = Integrator::of_process(pid);
    landing_queue.act_as(integrator.actor().clone());
    landing_queue
        .record_runtime_event(
            run.id(),
            EventKind::IntegrationRebased,
            serde_json::json!({"onto": main}),
        )
        .unwrap();
    // The service goes down as `main` has moved, and comes back.
    drop(served);
    let restart = {
        let db = db.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(600));
            common::service::Served::start(&db)
        })
    };
    let landing = Landing {
        commit: main.clone(),
        source_commit: main.clone(),
        main_before: main.clone(),
        history_ref: format!("refs/dagq/runs/{}", run.id()),
        message: "feat: landed through the service".into(),
        verification_skipped: false,
    };
    let (task, landed) = landing_queue
        .finish_integration(run.id(), &token, &landing, "/repo/.git")
        .unwrap();
    let _served = {
        let _waiting = common::within(common::STEP_LIMIT, "the service's restart to return");
        restart.join().unwrap()
    };
    assert_eq!(landed.status(), RunStatus::Integrated);
    assert_eq!(task.status(), TaskStatus::Completed);
    landing_queue.act_as(ActorContext::instance(ActorRole::Supervisor, pid));
    drop(refused);

    let events = queue.run_events(run.id()).unwrap();
    let integrated: Vec<_> = events
        .iter()
        .filter(|event| event.kind == EventKind::RunIntegrated)
        .collect();
    assert_eq!(integrated.len(), 1, "{events:?}");
    let rebased = events
        .iter()
        .filter(|event| event.kind == EventKind::IntegrationRebased)
        .count();
    assert_eq!(rebased, 1, "{events:?}");
    assert_eq!(
        integrated[0].actor.as_ref().map(|actor| actor.id.clone()),
        Some(format!("integrator:{pid}"))
    );
    let credential = dagq::infrastructure::queue_service::credential_path(
        db.parent().unwrap(),
        &Principal::integrator(run.id(), &token, pid).credential_key(),
    );
    assert!(!credential.exists(), "{}", credential.display());
    caller.sign_out();
}

/// A record the queue was busy for is held, and the next one, which the
/// queue takes, sends it first: the queue writes them in the order they
/// were recorded, as the supervisor. The queue is never opened here.
#[test]
fn a_record_after_one_the_queue_was_busy_for_is_written_after_it() {
    use dagq::application::EventStore;
    use dagq::domain::EventKind;
    let (_fixture, _repo, db) = fixture();
    let _served = common::service::Served::start(&db);
    let pid = std::process::id();
    let caller: Arc<dyn ServiceCaller> = Arc::new(Resending::new(
        Arc::new(BusyOnce {
            inner: Arc::new(SocketCaller::new(&db)),
            op: "record_queue_event",
            met: std::sync::atomic::AtomicBool::new(false),
        }),
        Resend::Hold,
        dagq::infrastructure::clock::system(),
    ));
    caller
        .sign_in(&LeaseToken::new("supervisor-behind"), pid)
        .unwrap();
    let queue = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    )
    .queue();
    let refused = refuse_opening(&db);
    let finished =
        |dir: &str| serde_json::json!({"mode": "hourly", "outcome": "succeeded", "dir": dir});
    let held = queue
        .record_queue_event(EventKind::ObserveFinished, finished("/observations/1"))
        .unwrap_err();
    assert!(dagq::application::QueueBusy::is(&held), "{held:#}");
    for dir in ["/observations/2", "/observations/3"] {
        queue
            .record_queue_event(EventKind::ObserveFinished, finished(dir))
            .unwrap();
    }
    assert!(caller.send_held().unwrap());
    drop(refused);
    let dirs: Vec<String> = dagq::infrastructure::sqlite::SqliteQueue::open(&db)
        .unwrap()
        .latest_events_of("observe_finished", 10)
        .unwrap()
        .into_iter()
        .rev()
        .map(|event| event.payload["dir"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        dirs,
        ["/observations/1", "/observations/2", "/observations/3"]
    );
    caller.sign_out();
}

/// A run's event a pass finds against the run's history
/// (`provider_waiting`) is never held (ADR-t3102-3 decision 5): while the
/// queue stays busy for it, a pass that goes on finds it missing and sends
/// it again, and the queue has it once. A request's record
/// (`revise_requested`) the queue is busy for is its caller's error, and
/// nothing of it is written later; a job's end and a turn's request, each
/// recorded once after what it did, are held and written once. The queue
/// is never opened here.
#[test]
fn a_runs_event_found_again_is_written_once_and_a_request_not_taken_never() {
    use dagq::application::supervisor_service::resend;
    use dagq::domain::EventKind;
    let (_fixture, repo, db) = fixture();
    let _served = common::service::Served::start(&db);
    let pid = std::process::id();
    let busy = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let caller: Arc<dyn ServiceCaller> = Arc::new(Resending::new(
        Arc::new(BusyFor {
            inner: Arc::new(SocketCaller::new(&db)),
            kinds: &[
                "provider_waiting",
                "revise_requested",
                "review_job_finished",
            ],
            left: busy.clone(),
        }),
        Resend::Hold,
        dagq::infrastructure::clock::system(),
    ));
    let token = LeaseToken::new("supervisor-that-reads-again");
    caller.sign_in(&token, pid).unwrap();
    let queues = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    );
    let mut queue = queues.open().unwrap();
    let limits = SlotLimits::resolve(SlotFlags::default(), &SupervisorConfig::default());
    queue
        .register_supervisor(&token, pid, limits, &[], "0.0.0-test")
        .unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&head(&repo), &token).unwrap()
    else {
        panic!("no task was claimed");
    };
    let refused = refuse_opening(&db);
    // A pass: what is held goes first, then the wait is recorded unless
    // the run's history has it.
    let pass = || -> anyhow::Result<()> {
        caller.send_held()?;
        let events = queue.run_events(run.id())?;
        if !events
            .iter()
            .any(|event| event.kind == EventKind::ProviderWaiting)
        {
            queue.record_runtime_event_now(
                run.id(),
                EventKind::ProviderWaiting,
                serde_json::json!({"phase": "resume"}),
            )?;
        }
        Ok(())
    };
    busy.store(2, Ordering::SeqCst);
    for _ in 0..2 {
        let error = pass().unwrap_err();
        assert!(
            dagq::application::QueueBusy::is(&error) && !resend::kept(&error),
            "{error:#}"
        );
    }
    pass().unwrap();
    pass().unwrap();
    busy.store(1, Ordering::SeqCst);
    let error = queue
        .record_runtime_event_now(
            run.id(),
            EventKind::ReviseRequested,
            serde_json::json!({"attempt": 1}),
        )
        .unwrap_err();
    assert!(
        dagq::application::QueueBusy::is(&error) && !resend::kept(&error),
        "{error:#}"
    );
    busy.store(1, Ordering::SeqCst);
    queue
        .record_runtime_event(
            run.id(),
            EventKind::ReviewJobFinished,
            serde_json::json!({"job": "acceptance"}),
        )
        .unwrap();
    // A result its caller records once after what it did (a request
    // written to `turns/`), of any kind, is held behind it.
    queue
        .record_runtime_event(
            run.id(),
            EventKind::TurnRequested,
            serde_json::json!({"turn": 1}),
        )
        .unwrap();
    assert!(caller.send_held().unwrap());
    assert!(caller.send_held().unwrap());
    drop(refused);
    let events = dagq::infrastructure::sqlite::SqliteQueue::open(&db)
        .unwrap()
        .run_events(run.id())
        .unwrap();
    let count = |kind: EventKind| events.iter().filter(|event| event.kind == kind).count();
    assert_eq!(count(EventKind::ProviderWaiting), 1);
    assert_eq!(count(EventKind::ReviseRequested), 0);
    assert_eq!(count(EventKind::ReviewJobFinished), 1);
    assert_eq!(count(EventKind::TurnRequested), 1);
    caller.sign_out();
}

/// A conflict-only resume of a run whose provider is held waits (ADR-t3255-1
/// decision 1), and its `provider_waiting`, found against the run's
/// history, is never kept (ADR-t3102-3 decision 5): while the queue does
/// not take it, the supervisor's pass through the service records none and
/// leaves nothing held beside the queue for a later one to write. Once the
/// queue takes it the next pass records it, the passes after find it and
/// record no other, and the run is not resumed meanwhile.
#[test]
fn a_resumes_wait_for_its_held_provider_is_never_kept_and_is_recorded_once() {
    use crate::runtime_support::{
        TestWorkspace, VALID_AGENT, parked_conflict, payloads, supervise_options, supervise_with,
    };
    use dagq::application::{TaskReads, supervisor_service::resend::HeldRecords};
    use dagq::domain::{RunStatus, TaskId};
    use dagq::infrastructure::{held_records::HeldDir, sqlite::SqliteQueue};
    let (_fixture, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    SqliteQueue::open(&db)
        .unwrap()
        .hold(dagq::domain::NewHold {
            reason_category: dagq::domain::AskReason::Cost,
            subject: Some(dagq::domain::queue_hold::USAGE_LIMIT_SUBJECT.to_owned()),
            run_id: None,
            job: None,
            question: "the usage limit".into(),
            options: dagq::domain::HOLD_OPTIONS
                .iter()
                .map(|o| (*o).to_owned())
                .collect(),
            asked_by: "supervisor".into(),
        })
        .unwrap();
    // More refusals than the passes send: a wait kept and sent again
    // would use them up, and then be written.
    const REFUSALS: usize = 20;
    let (layer, busy) = BusyFor::layer(&["provider_waiting"], REFUSALS);
    let options = dagq::compose::SuperviseOptions {
        service_route: dagq::compose::ServiceRoute::InProcessThrough(layer),
        ..supervise_options(1, true)
    };
    let passes = |count: usize| {
        for _ in 0..count {
            let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
            assert_eq!(outcome["errors"], serde_json::json!([]), "{outcome}");
        }
        SqliteQueue::open(&db)
            .unwrap()
            .show(TaskId::new(2))
            .unwrap()
    };
    let resume_waits = |detail: &dagq::domain::TaskDetail| -> Vec<serde_json::Value> {
        payloads(detail, "provider_waiting")
            .into_iter()
            .filter(|payload| payload["phase"] == "resume")
            .cloned()
            .collect()
    };
    let detail = passes(1);
    assert_eq!(
        resume_waits(&detail),
        Vec::<serde_json::Value>::new(),
        "a wait the queue did not take was kept and written later"
    );
    assert_eq!(
        busy.load(Ordering::SeqCst),
        REFUSALS - 1,
        "the pass sent the wait once"
    );
    assert!(
        HeldDir::of(&db).kept().unwrap().is_empty(),
        "a wait the queue did not take is kept"
    );
    busy.store(0, Ordering::SeqCst);
    let detail = passes(2);
    let waits = resume_waits(&detail);
    assert_eq!(waits.len(), 1, "{waits:?}");
    assert_eq!(waits[0]["conflict_only"], true, "{}", waits[0]);
    assert_eq!(waits[0]["provider"], "claude", "{}", waits[0]);
    assert!(payloads(&detail, "resume_started").is_empty());
    assert_eq!(detail.runs[0].id(), run.id());
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
}

/// The supervisor's calls in this double, but every `supervisor_draining`
/// the queue stays busy for: a supervisor whose owner is gone keeps the
/// record of its stop, and holds it to its end.
struct DrainingBusy {
    inner: Arc<dyn ServiceCaller>,
    refused: Arc<std::sync::atomic::AtomicUsize>,
}

impl ServiceCaller for DrainingBusy {
    fn call(
        &self,
        use_case: dagq::domain::queue_service::UseCase,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        if params["op"] == "record_queue_event" && params["args"][0] == "supervisor_draining" {
            self.refused.fetch_add(1, Ordering::SeqCst);
            return Err(anyhow::anyhow!("locked").context(dagq::application::QueueBusy));
        }
        self.inner.call(use_case, params)
    }
    fn with_token(&self, file: &Path) -> anyhow::Result<Arc<dyn ServiceCaller>> {
        self.inner.with_token(file)
    }
    fn send_held(&self) -> anyhow::Result<bool> {
        self.inner.send_held()
    }
    fn sign_in(&self, registration: &LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.sign_in(registration, pid)
    }
    fn take_back(&self, registration: &LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.take_back(registration, pid)
    }
    fn hand_over(&self) {
        self.inner.hand_over();
    }
    fn sign_out(&self) {
        self.inner.sign_out();
    }
    fn credential(&self) -> Option<std::path::PathBuf> {
        self.inner.credential()
    }
}

/// A supervisor whose owner is gone exits within its grace, shortened here
/// to 500 ms, while the queue stays busy for the record of its stop: the
/// end of its loop sends what it holds again only within the grace, not
/// its tries a poll (here 1 s) apart, which would take half a minute, and
/// leaves it kept beside the queue for the next supervisor (ADR-t3102-3
/// decision 5).
#[test]
fn a_supervisor_whose_owner_is_gone_sends_what_it_holds_only_within_its_grace() {
    use crate::runtime_support::{
        IDLE_AGENT, TestWorkspace, await_passes, joined, supervise_options, supervise_with,
    };
    use dagq::application::supervisor_service::resend::HeldRecords;
    use dagq::infrastructure::held_records::HeldDir;
    use std::time::{Duration, Instant};
    let (_fixture, repo, db) = fixture();
    // Claude held: nothing is claimed.
    dagq::infrastructure::sqlite::SqliteQueue::open(&db)
        .unwrap()
        .hold(dagq::domain::NewHold {
            reason_category: dagq::domain::AskReason::Cost,
            subject: Some(dagq::domain::queue_hold::USAGE_LIMIT_SUBJECT.to_owned()),
            run_id: None,
            job: None,
            question: "the usage limit".into(),
            options: dagq::domain::HOLD_OPTIONS
                .iter()
                .map(|o| (*o).to_owned())
                .collect(),
            asked_by: "supervisor".into(),
        })
        .unwrap();
    let refused = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let layer = {
        let refused = refused.clone();
        dagq::compose::CallerLayer(Arc::new(move |inner| {
            Arc::new(DrainingBusy {
                inner,
                refused: refused.clone(),
            }) as Arc<dyn ServiceCaller>
        }))
    };
    let mut owner = common::service::StandInOwner::start();
    let options = dagq::compose::SuperviseOptions {
        owner: Some(owner.pid()),
        owner_gone_grace: Duration::from_millis(500),
        idle_poll: Duration::from_secs(1),
        service_route: dagq::compose::ServiceRoute::InProcessThrough(layer),
        ..supervise_options(1, false)
    };
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo) = (db.clone(), repo.clone());
        std::thread::spawn(move || {
            let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
            supervise_with(&db, &repo, &backend, &options)
        })
    };
    await_passes(&passes, 1);
    owner.kill();
    let killed = Instant::now();
    let outcome = joined(supervisor, "the supervisor whose owner is gone").unwrap();
    let took = killed.elapsed();
    assert_eq!(outcome["outcome"], "owner_gone", "{outcome}");
    assert!(
        refused.load(Ordering::SeqCst) >= 1,
        "the stop was never recorded"
    );
    // A pass to find the owner gone (1 s), then the grace: well short of
    // the 28 s of the two ends' tries a poll apart.
    assert!(took < Duration::from_secs(10), "{took:?}");
    assert!(
        HeldDir::of(&db)
            .kept()
            .unwrap()
            .iter()
            .any(|kept| kept.params["args"][0] == "supervisor_draining"),
        "the stop's record is not kept for the next supervisor"
    );
}

/// A `--once` supervisor whose run's e2e end the queue stays busy for
/// holds it, and its next passes wait for the queue to take it before they
/// read the run again (ADR-t3102-3 decision 5): the e2e runs once, and the
/// run lands with one `run_e2e_finished`. A pass that went on would find
/// the e2e still due and run it again.
#[test]
fn a_once_supervisor_reads_a_run_again_only_once_the_queue_took_its_held_end() {
    use crate::runtime_support::{
        IDLE_AGENT, TestReviewer, TestWorkspace, supervise_reviewed_with, verdict,
    };
    use dagq::application::TaskReads;
    use dagq::domain::{EventKind, RunStatus, TaskId};
    let (fixture, repo, db) = fixture();
    with_e2e_paths(&repo, "[\"change.txt\"]");
    let ran = fixture.path().join("e2e.ran");
    let e2e = format!(
        "echo ran >> {}; echo 'test result: ok. 1 passed; 0 failed'",
        common::shell_path(&ran)
    );
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let (layer, busy) = BusyFor::layer(&["run_e2e_finished"], 3);
    let options = dagq::compose::SuperviseOptions {
        service_route: dagq::compose::ServiceRoute::InProcessThrough(layer),
        ..e2e_options(e2e)
    };
    assert!(options.once);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], serde_json::json!([]), "{outcome}");
    assert_eq!(
        busy.load(Ordering::SeqCst),
        0,
        "the queue was busy for the end"
    );
    assert_eq!(
        std::fs::read_to_string(&ran).unwrap().lines().count(),
        1,
        "the e2e ran once"
    );
    let mut queue = dagq::infrastructure::sqlite::SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let events = queue.run_events(detail.runs[0].id()).unwrap();
    let count = |kind: EventKind| events.iter().filter(|event| event.kind == kind).count();
    assert_eq!(count(EventKind::RunE2eFinished), 1);
    assert_eq!(count(EventKind::RunIntegrated), 1);
}

/// The supervisor's record of a result the service did not answer is held
/// and sent first at its next pass, and the service takes it once
/// (ADR-t3102-3 decision 5): one whose answer was lost after it was
/// recorded, and one the service was down for, held while it is still
/// down. A result alike recorded after them is another event. The queue
/// is never opened here.
#[test]
fn a_record_the_service_did_not_answer_is_held_and_taken_once() {
    use dagq::application::EventStore;
    use dagq::domain::EventKind;
    let (_fixture, _repo, db) = fixture();
    let served = common::service::Served::start(&db);
    let pid = std::process::id();
    let caller: Arc<dyn ServiceCaller> = Arc::new(Resending::new(
        Arc::new(LosesOneAnswer::new(
            Arc::new(SocketCaller::new(&db)),
            &["record_queue_event"],
        )),
        Resend::Hold,
        dagq::infrastructure::clock::system(),
    ));
    caller
        .sign_in(&LeaseToken::new("supervisor-that-holds"), pid)
        .unwrap();
    let queue = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    )
    .queue();
    let refused = refuse_opening(&db);
    let finished =
        |dir: &str| serde_json::json!({"mode": "hourly", "outcome": "succeeded", "dir": dir});
    let lost = queue
        .record_queue_event(EventKind::ObserveFinished, finished("/observations/1"))
        .unwrap_err();
    assert!(unreachable(&lost), "{lost:#}");
    drop(served);
    let down = queue
        .record_queue_event(EventKind::ObserveFinished, finished("/observations/2"))
        .unwrap_err();
    assert!(unreachable(&down), "{down:#}");
    let still = caller.send_held().unwrap_err();
    assert!(unreachable(&still), "{still:#}");
    let _served = common::service::Served::start(&db);
    caller.send_held().unwrap();
    caller.send_held().unwrap();
    queue
        .record_queue_event(EventKind::ObserveFinished, finished("/observations/1"))
        .unwrap();
    drop(refused);
    let dirs: Vec<(String, String)> = dagq::infrastructure::sqlite::SqliteQueue::open(&db)
        .unwrap()
        .latest_events_of("observe_finished", 10)
        .unwrap()
        .into_iter()
        .rev()
        .map(|event| {
            (
                event.payload["dir"].as_str().unwrap().to_owned(),
                event.actor.unwrap().id,
            )
        })
        .collect();
    let supervisor = format!("supervisor:{pid}");
    assert_eq!(
        dirs,
        [
            ("/observations/1".to_owned(), supervisor.clone()),
            ("/observations/2".to_owned(), supervisor.clone()),
            ("/observations/1".to_owned(), supervisor)
        ]
    );
    caller.sign_out();
}

/// Records the service does not take for longer than their callers send
/// them again are kept beside the queue (ADR-t3102-3 decision 5): one whose
/// answer was lost after it was recorded, and one the service was down for,
/// both still held when their supervisor ended, and one a job command gave
/// up on past its wait. The next supervisor, another pid, sends them once
/// the service is back: each is recorded once, the lost one not twice,
/// without the queue being opened.
#[test]
fn records_not_taken_past_their_callers_are_kept_and_sent_by_the_next_supervisor() {
    use dagq::application::EventStore;
    use dagq::application::supervisor_service::resend::{HeldRecords, Waits};
    use dagq::domain::EventKind;
    use dagq::infrastructure::held_records::HeldDir;
    let (_fixture, _repo, db) = fixture();
    let served = common::service::Served::start(&db);
    let pid = std::process::id();
    let clock = dagq::infrastructure::clock::system();
    let socket: Arc<dyn ServiceCaller> = Arc::new(SocketCaller::new(&db));
    socket
        .sign_in(&LeaseToken::new("supervisor-that-ends"), pid)
        .unwrap();
    let ending: Arc<dyn ServiceCaller> = Arc::new(
        Resending::new(
            Arc::new(LosesOneAnswer::new(socket.clone(), &["record_queue_event"])),
            Resend::Hold,
            clock.clone(),
        )
        .kept_in(Arc::new(HeldDir::of(&db))),
    );
    let job: Arc<dyn ServiceCaller> = Arc::new(
        Resending::with_waits(
            socket,
            Resend::Wait,
            clock.clone(),
            Waits {
                first: std::time::Duration::from_millis(10),
                longest: std::time::Duration::from_millis(20),
                limit: std::time::Duration::from_millis(100),
            },
        )
        .kept_in(Arc::new(HeldDir::of(&db))),
    );
    let as_supervisor = ActorContext::instance(ActorRole::Supervisor, pid);
    let refused = refuse_opening(&db);
    let finished =
        |dir: &str| serde_json::json!({"mode": "hourly", "outcome": "succeeded", "dir": dir});
    let supervisor = SupervisorQueues::new(ending.clone(), as_supervisor.clone()).queue();
    let lost = supervisor
        .record_queue_event(EventKind::ObserveFinished, finished("/observations/lost"))
        .unwrap_err();
    assert!(unreachable(&lost), "{lost:#}");
    drop(served);
    let held = supervisor
        .record_queue_event(EventKind::ObserveFinished, finished("/observations/held"))
        .unwrap_err();
    assert!(unreachable(&held), "{held:#}");
    for _ in 0..3 {
        assert!(unreachable(&ending.send_held().unwrap_err()));
    }
    let given_up = SupervisorQueues::new(job, as_supervisor)
        .queue()
        .record_queue_event(
            EventKind::ObserveFinished,
            finished("/observations/given-up"),
        )
        .unwrap_err();
    assert!(
        format!("{given_up:#}").contains("kept for the supervisor"),
        "{given_up:#}"
    );
    drop(supervisor);
    drop(ending);
    assert_eq!(HeldDir::of(&db).kept().unwrap().len(), 3);
    let _served = common::service::Served::start(&db);
    let next: Arc<dyn ServiceCaller> = Arc::new(
        Resending::new(Arc::new(SocketCaller::new(&db)), Resend::Hold, clock)
            .kept_in(Arc::new(HeldDir::of(&db))),
    );
    next.sign_in(&LeaseToken::new("the-next-supervisor"), pid + 1)
        .unwrap();
    assert!(next.send_held().unwrap());
    assert!(next.send_held().unwrap());
    drop(refused);
    assert!(HeldDir::of(&db).kept().unwrap().is_empty());
    let recorded: Vec<(String, String)> = dagq::infrastructure::sqlite::SqliteQueue::open(&db)
        .unwrap()
        .latest_events_of("observe_finished", 10)
        .unwrap()
        .into_iter()
        .rev()
        .map(|event| {
            (
                event.payload["dir"].as_str().unwrap().to_owned(),
                event.actor.unwrap().id,
            )
        })
        .collect();
    let (first, next_one) = (
        format!("supervisor:{pid}"),
        format!("supervisor:{}", pid + 1),
    );
    assert_eq!(
        recorded,
        [
            ("/observations/lost".to_owned(), first),
            ("/observations/held".to_owned(), next_one.clone()),
            ("/observations/given-up".to_owned(), next_one)
        ]
    );
    next.sign_out();
}

/// A job command's finding (a weekly review's next move) the service does
/// not answer is sent again until it does, and recorded once
/// (ADR-t3102-3 decision 5): its first sending finds the service out of
/// reach, its second is recorded but its answer lost, and its third is
/// answered with the finding the second recorded, its same evidence no
/// new occurrence. The queue is never opened here.
#[test]
fn a_findings_record_the_service_did_not_answer_is_sent_again_and_recorded_once() {
    use dagq::application::supervisor_service::resend::Waits;
    use dagq::domain::{FindingQuery, FindingTarget, NewFinding};
    let (_fixture, _repo, db) = fixture();
    let _served = common::service::Served::start(&db);
    let pid = std::process::id();
    let (layer, sent) = crate::runtime_support::service::DropsThenLoses::wrap("record_finding");
    let caller: Arc<dyn ServiceCaller> = Arc::new(Resending::with_waits(
        (layer.0)(Arc::new(SocketCaller::new(&db))),
        Resend::Wait,
        dagq::infrastructure::clock::system(),
        Waits {
            first: std::time::Duration::from_millis(10),
            longest: std::time::Duration::from_millis(50),
            limit: common::STEP_LIMIT,
        },
    ));
    caller
        .sign_in(&LeaseToken::new("supervisor-of-a-review"), pid)
        .unwrap();
    let queue = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    )
    .queue();
    let refused = refuse_opening(&db);
    let id = queue
        .record_finding(&NewFinding {
            kind: "throughput_next_move".into(),
            target: FindingTarget::Queue,
            subject: "weekly/2026-W43".into(),
            summary: "cache the build".into(),
            detail: Some("Why: half the landings wait on it".into()),
            impact: None,
            evidence: Vec::new(),
            propose: Some("half the landings wait on it".into()),
            by: "supervisor".into(),
        })
        .unwrap();
    drop(refused);
    assert_eq!(sent.load(Ordering::SeqCst), 3);
    let findings = dagq::infrastructure::sqlite::SqliteQueue::open(&db)
        .unwrap()
        .findings(&FindingQuery {
            id: None,
            all: true,
            statuses: Vec::new(),
            kinds: vec!["throughput_next_move".into()],
            target: None,
            full: false,
        })
        .unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].finding.id.as_i64(), id);
    assert_eq!(findings[0].finding.occurrences, 1);
    caller.sign_out();
}

/// The integrator's token, issued as the slot is taken, is refused
/// anything outside its landing's run and task, and a supervisor's token
/// any answer but the runtime's, through the service and with nothing
/// written: another task's `blocked` ask stays open and unanswered.
#[test]
fn an_integrator_and_a_supervisor_are_refused_outside_their_scope_writing_nothing() {
    use crate::runtime_support::add_ready_task;
    use dagq::application::integrate::Integrator;
    use dagq::application::{TaskReads, TaskStore};
    use dagq::domain::{
        Answerer, AskKind, AskReason, NewAsk, NewGoal, NewTask, PlannerOrigin, PlannerOwner,
        Submission, TaskAction, TaskStatus,
    };
    use dagq::infrastructure::sqlite::SqliteQueue;
    let new_task = |title: &str| NewTask {
        title: title.into(),
        description: "small change".into(),
        acceptance: "works".into(),
        verification_commands: vec!["test -f seed.txt".into()],
        required_evidence: Vec::new(),
        paths: Vec::new(),
        priority: Default::default(),
        change: None,
        dependencies: Vec::new(),
        goal_dependencies: Vec::new(),
        goal_id: None,
        context: String::new(),
        provider: None,
        worker_mode: None,
        wait_for_build: false,
        execution_class: Default::default(),
    };
    let new_goal = || NewGoal {
        priority: Default::default(),
        title: "faster landings".into(),
        description: "land in half the time".into(),
        acceptance: "(1) the median landing is under 5 minutes".into(),
        constraints: String::new(),
        doc: None,
        draft: true,
        tags: Vec::new(),
    };
    let (_fixture, repo, db) = fixture();
    let _served = common::service::Served::start(&db);
    let pid = std::process::id();
    let caller: Arc<dyn ServiceCaller> = Arc::new(SocketCaller::new(&db));
    let queues = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    );
    let token = LeaseToken::new("supervisor-in-scope");
    caller.sign_in(&token, pid).unwrap();
    let mut queue = queues.open().unwrap();
    let limits = SlotLimits::resolve(SlotFlags::default(), &SupervisorConfig::default());
    queue
        .register_supervisor(&token, pid, limits, &[], "0.0.0-test")
        .unwrap();
    let main = head(&repo);
    let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&main, &token).unwrap() else {
        panic!("no task was claimed");
    };
    let mut direct = SqliteQueue::open(&db).unwrap();
    let other = add_ready_task(&mut direct, "another task", &[]);
    let ask = direct
        .ask(NewAsk {
            kind: AskKind::Blocked,
            task_id: Some(other),
            run_id: None,
            question: "Stuck?".into(),
            options: vec!["done".into()],
            asked_by: "observer".into(),
            reason_category: AskReason::Scope,
            topics: Vec::new(),
            recommendation: None,
            confidence: None,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    let draft = direct.add(new_task("a draft")).unwrap().id();
    let goal = direct.add_goal(new_goal()).unwrap().id();
    let goals = direct.list_goals().unwrap().len();
    drop(direct);
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration' WHERE id=?1",
            [run.id().as_str()],
        )
        .unwrap();
    let refused = refuse_opening(&db);
    queue.begin_integration(run.id(), &token, &main).unwrap();
    let mut landing = queues.open().unwrap();
    landing.act_as(Integrator::of_process(pid).actor().clone());
    for refusal in [
        landing.close_ended_landing_asks(None).map(|_| ()),
        landing
            .close_blocked_asks(run.id(), other, "landed")
            .map(|_| ()),
        landing.show(other).map(|_| ()),
    ] {
        let error = format!("{:#}", refusal.unwrap_err());
        assert!(error.contains("the integrator of run"), "{error}");
    }
    landing.act_as(ActorContext::instance(ActorRole::Supervisor, pid));
    let as_person = queue
        .answer_as(ask.id, "done", Answerer::PERSON)
        .unwrap_err();
    assert!(
        format!("{as_person:#}").contains("answers no ask as person"),
        "{as_person:#}"
    );
    let kept = queue.read_ask(ask.id).unwrap();
    // The planning a person, the inbox or a planner does is refused to a
    // supervisor as the command line refuses it, writing nothing; what the
    // supervisor never sends is none of its operations (default deny).
    for (refusal, capability) in [
        (
            queue
                .transition(draft, TaskAction::BypassReview)
                .map(|_| ()),
            "task.ready_bypass_review",
        ),
        (queue.add(new_task("another")).map(|_| ()), "task.write"),
        (queue.add_goal(new_goal()).map(|_| ()), "goal.write"),
        (queue.ready_goal(goal).map(|_| ()), "goal.ready"),
        (
            queue
                .submit(Submission {
                    tasks: vec![draft],
                    goals: Vec::new(),
                    proposal: None,
                    owner: PlannerOwner {
                        origin: PlannerOrigin::Runtime,
                        workspace_id: None,
                    },
                })
                .map(|_| ()),
            "proposal.submit",
        ),
        (
            queue.transition(draft, TaskAction::Ready).map(|_| ()),
            "does not transition",
        ),
    ] {
        let error = format!("{:#}", refusal.unwrap_err());
        assert!(error.contains(capability), "{error}");
    }
    drop(refused);
    assert_eq!(kept.answer, None);
    assert_eq!(kept.closed_at, None);
    let mut direct = SqliteQueue::open(&db).unwrap();
    assert_eq!(direct.show(draft).unwrap().task.status(), TaskStatus::Draft);
    assert!(direct.show_goal(goal).unwrap().goal.is_draft());
    assert_eq!(direct.list_goals().unwrap().len(), goals);
    assert!(direct.proposals(true).unwrap().is_empty());
    let denied: Vec<String> = direct
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind.as_str() == "authorization_denied")
        .map(|event| {
            event.payload["capability"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(
        denied,
        [
            "task.ready_bypass_review",
            "task.write",
            "goal.write",
            "goal.ready",
            "proposal.submit"
        ]
    );
    caller.sign_out();
}

/// The supervisor's route through a [`LosingAnswers`] that loses `lose`,
/// and where the test finds that caller once the supervisor made it.
fn losing_answers(
    lose: Vec<&'static [&'static str]>,
) -> (
    Arc<std::sync::Mutex<Option<Arc<LosingAnswers>>>>,
    dagq::compose::CallerLayer,
) {
    let losing = Arc::new(std::sync::Mutex::new(None::<Arc<LosingAnswers>>));
    let layer = {
        let losing = losing.clone();
        dagq::compose::CallerLayer(Arc::new(move |inner| {
            let caller = Arc::new(LosingAnswers {
                inner,
                lose: std::sync::Mutex::new(lose.clone()),
                lost: std::sync::Mutex::new(Vec::new()),
            });
            *losing.lock().unwrap() = Some(caller.clone());
            caller as Arc<dyn ServiceCaller>
        }))
    };
    (losing, layer)
}

/// A claim the service did whose answer was lost leaves the run under
/// the supervisor's own lease in no slot: once the service answers, the
/// supervisor's next pass provisions it, without an exec and without a
/// second claim. The end of its session, done unanswered too, is not
/// sent again: the slot is rebuilt from the queue, the run goes on, and
/// `supervision_finished` is recorded once.
#[test]
fn a_claim_and_a_transition_done_unanswered_are_found_again_once_the_service_answers() {
    use crate::runtime_support::{
        TestWorkspace, VALID_AGENT, joined, supervise_options, supervise_retrying_by, wait_until,
    };
    use dagq::application::TaskReads;
    use dagq::domain::{EventKind, RunStatus, TaskId};

    let (_fixture, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let (losing, layer) = losing_answers(vec![
        &["claim_for_supervisor_holding"],
        &["finish_supervision", "finish_supervision_live"],
    ]);
    let options = dagq::compose::SuperviseOptions {
        service_route: dagq::compose::ServiceRoute::InProcessThrough(layer),
        retry_unreadable_review: false,
        ..supervise_options(1, false)
    };
    // The service a supervisor would start ends with this test's process.
    let dagq = common::service::owned_executable(db.parent().unwrap());
    let supervisor = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        std::thread::spawn(move || supervise_retrying_by(&db, &repo, &backend, &options, &dagq))
    };
    let task = TaskId::new(1);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue.show(task).unwrap().runs.first().is_some_and(|run| {
            !matches!(
                run.status(),
                RunStatus::Claimed
                    | RunStatus::Starting
                    | RunStatus::Running
                    | RunStatus::Validating
            )
        })
    });
    options.stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to stop").unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "stopped", "{outcome}");
    assert_eq!(outcome["errors"], serde_json::json!([]), "{outcome}");

    let lost = losing.lock().unwrap().clone().unwrap();
    assert_eq!(lost.lost.lock().unwrap().len(), 2, "{:?}", lost.lost);
    let mut queue = dagq::infrastructure::sqlite::SqliteQueue::open(&db).unwrap();
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.runs.len(), 1, "claimed once");
    let events = queue.run_events(detail.runs[0].id()).unwrap();
    let count = |kind: EventKind| events.iter().filter(|event| event.kind == kind).count();
    assert_eq!(count(EventKind::RunClaimed), 1);
    assert_eq!(count(EventKind::AgentStarted), 1);
    assert_eq!(count(EventKind::SupervisionFinished), 1);
}

/// The close of a passed run's workspace after its `/exit`, done by the
/// service but unanswered, comes after the step took what follows the
/// `/exit` out of the slot: the slot is not stepped again, which would
/// only give the lease back, but rebuilt from the queue, so the run still
/// lands, its workspace closed and its review finished once.
#[test]
fn a_passed_runs_exit_done_unanswered_still_lands() {
    use crate::runtime_support::{
        IDLE_AGENT, TestReviewer, TestWorkspace, claude_stub, joined, runtime, supervise_options,
        verdict, wait_until,
    };
    use dagq::application::TaskReads;
    use dagq::domain::{EventKind, RunStatus, TaskId};

    let (_fixture, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let reviewer = Arc::new(TestReviewer::new(&[verdict(
        "pass",
        &[],
        "meets the acceptance",
    )]));
    let (losing, layer) = losing_answers(vec![&["workspace_closed"]]);
    let options = dagq::compose::SuperviseOptions {
        service_route: dagq::compose::ServiceRoute::InProcessThrough(layer),
        ..supervise_options(1, false)
    };
    // The service a supervisor would start ends with this test's process.
    let dagq = common::service::owned_executable(db.parent().unwrap());
    let supervisor = {
        let (db, repo, backend, reviewer, options) = (
            db.clone(),
            repo.clone(),
            backend.clone(),
            reviewer.clone(),
            options.clone(),
        );
        std::thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                &dagq,
                &options,
            )
        })
    };
    let task = TaskId::new(1);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .show(task)
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| run.status() == RunStatus::Integrated)
    });
    options.stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to stop").unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "stopped", "{outcome}");
    assert_eq!(outcome["errors"], serde_json::json!([]), "{outcome}");

    let lost = losing.lock().unwrap().clone().unwrap();
    assert_eq!(*lost.lost.lock().unwrap(), ["workspace_closed"]);
    let mut queue = dagq::infrastructure::sqlite::SqliteQueue::open(&db).unwrap();
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.runs.len(), 1);
    let events = queue.run_events(detail.runs[0].id()).unwrap();
    let count = |kind: EventKind| events.iter().filter(|event| event.kind == kind).count();
    assert_eq!(count(EventKind::WorkspaceClosed), 1);
    assert_eq!(count(EventKind::ReviewFinished), 1);
    assert_eq!(count(EventKind::RunIntegrated), 1);
}

/// An approved run's resume that resolved it, ended by the service
/// without its answer (so the supervisor never wrote its
/// `landing_queued`), and then the read of the run's events the
/// reconciliation takes it over by, unanswered too: the run keeps its
/// lease rather than giving it back on a guess, and the reconciliation
/// after that reads its events and puts it back to wait for its landing.
/// It lands, resumed and its resume ended once, never resumed again nor
/// skipped.
#[test]
fn an_approved_resumes_end_and_the_read_after_it_unanswered_still_land() {
    use crate::runtime_support::{
        TestWorkspace, VALID_AGENT, joined, parked_conflict, supervise_options,
        supervise_retrying_by, wait_until,
    };
    use dagq::application::TaskReads;
    use dagq::domain::{EventKind, RunStatus, TaskId};

    let (_fixture, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        "await_message; resolve || exit 1; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let backend = Arc::new(backend);
    let (losing, layer) = losing_answers(vec![&["finish_approved_resume"], &["run_events"]]);
    let options = dagq::compose::SuperviseOptions {
        service_route: dagq::compose::ServiceRoute::InProcessThrough(layer),
        retry_unreadable_review: false,
        ..supervise_options(1, false)
    };
    // The service a supervisor would start ends with this test's process.
    let dagq = common::service::owned_executable(db.parent().unwrap());
    let supervisor = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        std::thread::spawn(move || supervise_retrying_by(&db, &repo, &backend, &options, &dagq))
    };
    let task = TaskId::new(2);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .show(task)
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| run.status() == RunStatus::Integrated)
    });
    options.stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to stop").unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "stopped", "{outcome}");
    assert_eq!(outcome["errors"], serde_json::json!([]), "{outcome}");

    let lost = losing.lock().unwrap().clone().unwrap();
    assert_eq!(
        *lost.lost.lock().unwrap(),
        ["finish_approved_resume", "run_events"]
    );
    let mut queue = dagq::infrastructure::sqlite::SqliteQueue::open(&db).unwrap();
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.runs.len(), 1);
    let events = queue.run_events(run.id()).unwrap();
    let count = |kind: EventKind| events.iter().filter(|event| event.kind == kind).count();
    assert_eq!(count(EventKind::ResumeStarted), 1);
    assert_eq!(count(EventKind::ResumeFinished), 1);
    assert_eq!(count(EventKind::ResumeSkipped), 0);
    assert_eq!(count(EventKind::RunIntegrated), 1);
}

/// While a passed run's e2e runs (a stand-in that waits for the test), the
/// look at the run's lease goes unanswered. A read did nothing, so the
/// slot keeps its e2e rather than being rebuilt and starting another: the
/// e2e starts once, and once the service answers and the e2e ends, its
/// result is recorded once and the run lands.
#[test]
fn an_e2e_whose_lease_look_went_unanswered_starts_once_and_its_result_is_kept() {
    use crate::runtime_support::{
        IDLE_AGENT, TestReviewer, TestWorkspace, claude_stub, joined, runtime, supervise_options,
        verdict, wait_until,
    };
    use dagq::application::TaskReads;
    use dagq::domain::{EventKind, RunStatus, TaskId};

    let (fixture, repo, db) = fixture();
    with_e2e_paths(&repo, "[\"change.txt\"]");
    let go = fixture.path().join("e2e.go");
    let ran = fixture.path().join("e2e.ran");
    let e2e = format!(
        "echo ran >> {}; {}; echo 'test result: ok. 1 passed; 0 failed'",
        common::shell_path(&ran),
        common::await_path(&go)
    );
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let reviewer = Arc::new(TestReviewer::new(&[verdict(
        "pass",
        &[],
        "meets the acceptance",
    )]));
    let (losing, layer) = losing_answers(Vec::new());
    let options = dagq::compose::SuperviseOptions {
        service_route: dagq::compose::ServiceRoute::InProcessThrough(layer),
        run_e2e: runtime::RunE2eOptions {
            command: Some(e2e),
            ..Default::default()
        },
        ..supervise_options(1, false)
    };
    // The service a supervisor would start ends with this test's process.
    let dagq = common::service::owned_executable(db.parent().unwrap());
    let supervisor = {
        let (db, repo, backend, reviewer, options) = (
            db.clone(),
            repo.clone(),
            backend.clone(),
            reviewer.clone(),
            options.clone(),
        );
        std::thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                &dagq,
                &options,
            )
        })
    };
    let task = TaskId::new(1);
    let started = |queue: &mut dagq::infrastructure::sqlite::SqliteQueue, kind: &str| {
        queue
            .show(task)
            .unwrap()
            .events
            .iter()
            .filter(|event| event.kind.as_str() == kind)
            .count()
    };
    wait_until(&db, common::STEP_LIMIT, |queue| {
        started(queue, "run_e2e_started") == 1
    });
    // The e2e waits; the next look at the lease is not answered.
    let caller = losing.lock().unwrap().clone().unwrap();
    caller.lose.lock().unwrap().push(&["holds_lease"]);
    {
        let _waiting = common::within(common::STEP_LIMIT, "the lease's look to go unanswered");
        while caller.lost.lock().unwrap().is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    std::fs::write(&go, "").unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .show(task)
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| run.status() == RunStatus::Integrated)
    });
    options.stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to stop").unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "stopped", "{outcome}");
    assert_eq!(outcome["errors"], serde_json::json!([]), "{outcome}");

    assert_eq!(*caller.lost.lock().unwrap(), ["holds_lease"]);
    assert_eq!(
        std::fs::read_to_string(&ran).unwrap().lines().count(),
        1,
        "the e2e ran once"
    );
    let mut queue = dagq::infrastructure::sqlite::SqliteQueue::open(&db).unwrap();
    let detail = queue.show(task).unwrap();
    let events = queue.run_events(detail.runs[0].id()).unwrap();
    let of = |kind: EventKind| {
        events
            .iter()
            .filter(|event| event.kind == kind)
            .collect::<Vec<_>>()
    };
    assert_eq!(of(EventKind::RunE2eStarted).len(), 1);
    let finished = of(EventKind::RunE2eFinished);
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].payload["outcome"], "passed");
    assert_eq!(of(EventKind::RunIntegrated).len(), 1);
}

/// The supervisor's sweep revokes through the service the token of an
/// inbox whose session ended by its principal, past its own sign-in, which
/// the service refuses to revoke, while a revoke on request by the key the
/// token is kept under is refused (ADR-t3102-1 decision 2); which keys a
/// supervisor revokes is the unit test of
/// `domain::queue_service::supervisor_revokes`.
#[test]
fn the_supervisors_sweep_revokes_an_ended_inbox_sessions_token_through_the_service() {
    use dagq::domain::queue_service::SessionProcess;
    use dagq::infrastructure::queue_service::{issue, lookup, read_token};
    let (_fixture, _repo, db) = fixture();
    let _served = common::service::Served::start(&db);
    let queue_dir = db.parent().unwrap().to_path_buf();
    let inbox = Principal::inbox(SessionProcess {
        pid: 42,
        start: "Thu Oct 23 10:00:00 2026".to_owned(),
    });
    let inbox_token = read_token(&issue(&queue_dir, &inbox, 0).unwrap().file).unwrap();
    let _refused = refuse_opening(&db);
    let pid = std::process::id();
    let caller: Arc<dyn ServiceCaller> = Arc::new(SocketCaller::new(&db));
    let registration = LeaseToken::new("supervisor-sweeping");
    caller.sign_in(&registration, pid).unwrap();
    let tokens = ServiceTokens {
        queues: SupervisorQueues::new(
            caller.clone(),
            ActorContext::instance(ActorRole::Supervisor, pid),
        ),
        files: Arc::new(SystemServiceAccess),
        telemetry_files: Arc::new(SystemServiceAccess),
    };
    assert!(tokens.revoke(&db, &inbox.credential_key()).is_err());
    assert!(lookup(&queue_dir, &inbox_token).unwrap().is_some());
    let revoked = tokens
        .sweep(&db, &mut |principal| {
            principal.session.is_some() || principal.role == ActorRole::Supervisor
        })
        .unwrap();
    assert_eq!(revoked, [inbox.credential_key()]);
    assert!(lookup(&queue_dir, &inbox_token).unwrap().is_none());
    // The supervisor's own sign-in still answers.
    tokens.queues.open().unwrap().all_runs().unwrap();
    caller.sign_out();
}

/// The queue takes an event once per result's record (ADR-t3102-3
/// decision 5): sent again an hour later, after 1500 newer events of its
/// kind, it is still the one event, and two results alike recorded in the
/// same second under two records are two events.
#[test]
fn an_event_is_taken_once_per_record_however_late_and_two_alike_are_two() {
    use dagq::domain::EventKind;
    use dagq::infrastructure::sqlite::SqliteQueue;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let queue = SqliteQueue::init(&db).unwrap();
    let finished = || serde_json::json!({"mode": "hourly", "outcome": "succeeded"});
    let record = |name: &str| {
        queue
            .record_event_once(None, EventKind::ObserveFinished, finished(), name)
            .unwrap()
    };
    let first = record("record-1");
    let alike = record("record-2");
    assert_ne!(first, alike);
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE run_events SET created_at=strftime('%Y-%m-%dT%H:%M:%fZ','now','-1 hour') WHERE id=?1",
        [first.as_i64()],
    )
    .unwrap();
    conn.execute(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<1500)
         INSERT INTO run_events(kind, payload, actor_role, actor_id)
         SELECT 'observe_finished', ?1, 'supervisor', 'supervisor:1' FROM n",
        [finished().to_string()],
    )
    .unwrap();
    assert_eq!(record("record-1"), first);
    assert_eq!(record("record-2"), alike);
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM run_events WHERE kind='observe_finished'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1502);
}

/// A landing begins with its integrator's token in one transaction
/// (ADR-t3102-3 decision 3, ADR-t3102-1 decision 2): a token that cannot
/// be issued leaves the run awaiting its landing, its lease as it was, no
/// `integration_started` and no token. A token left by a landing that did
/// not commit writes nothing to the run; the token of the landing that
/// began writes to it, after the landing has left `integrating` too. A
/// supervisor takes no slot outside the landing's operation.
#[test]
fn a_landings_token_writes_only_once_its_landing_has_begun() {
    use dagq::application::RunReads;
    use dagq::application::integrate::Integrator;
    use dagq::domain::queue_service::{SupervisorParams, UseCase};
    use dagq::domain::{EventId, EventKind, RunStatus};
    use dagq::infrastructure::queue_service::{credential_path, issue, principals};
    use dagq::infrastructure::sqlite::SqliteQueue;
    let (_fixture, repo, db) = fixture();
    let _served = common::service::Served::start(&db);
    let queue_dir = db.parent().unwrap().to_path_buf();
    let pid = std::process::id();
    let caller: Arc<dyn ServiceCaller> = Arc::new(SocketCaller::new(&db));
    let queues = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    );
    let token = LeaseToken::new("supervisor-landing");
    caller.sign_in(&token, pid).unwrap();
    let mut queue = queues.open().unwrap();
    let limits = SlotLimits::resolve(SlotFlags::default(), &SupervisorConfig::default());
    queue
        .register_supervisor(&token, pid, limits, &[], "0.0.0-test")
        .unwrap();
    let main = head(&repo);
    let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&main, &token).unwrap() else {
        panic!("no task was claimed");
    };
    let sql = rusqlite::Connection::open(&db).unwrap();
    sql.execute(
        "UPDATE task_runs SET status='awaiting_integration' WHERE id=?1",
        [run.id().as_str()],
    )
    .unwrap();
    let lease_holder = || -> (String, i64) {
        sql.query_row(
            "SELECT token, pid FROM run_leases WHERE run_id=?1",
            [run.id().as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    };
    let lease = lease_holder();
    let status = || {
        RunReads::run(&SqliteQueue::open(&db).unwrap(), run.id())
            .unwrap()
            .status()
    };
    let started = || {
        SqliteQueue::open(&db)
            .unwrap()
            .run_events(run.id())
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == EventKind::IntegrationStarted)
            .map(|event| event.id)
            .collect::<Vec<_>>()
    };
    let integrators = || {
        principals(&queue_dir)
            .unwrap()
            .into_iter()
            .filter(|principal| principal.role == ActorRole::Integrator)
            .collect::<Vec<_>>()
    };
    let landing = Principal::integrator(run.id(), &token, pid);
    // The token's file cannot be written where a directory stands.
    let in_the_way = credential_path(&queue_dir, &landing.credential_key());
    std::fs::create_dir_all(in_the_way.join("in-the-way")).unwrap();

    queue
        .begin_integration(run.id(), &token, &main)
        .unwrap_err();
    assert_eq!(status(), RunStatus::AwaitingIntegration);
    assert_eq!(lease_holder(), lease);
    assert!(started().is_empty());
    assert!(integrators().is_empty());

    // A token issued in a landing's transaction that did not commit names
    // the event the transaction would have written; one of an older shape
    // names none. Neither writes to the run.
    let next: i64 = sql
        .query_row("SELECT coalesce(max(id),0)+1 FROM run_events", [], |row| {
            row.get(0)
        })
        .unwrap();
    std::fs::remove_dir_all(&in_the_way).unwrap();
    let send = |file: &Path, op: &str, args: serde_json::Value| {
        let params = SupervisorParams {
            op: op.to_owned(),
            args: args.as_array().unwrap().clone(),
            acting: None,
            requested_by: None,
            record: None,
        };
        caller
            .with_token(file)
            .unwrap()
            .call(UseCase::Supervisor, serde_json::to_value(params).unwrap())
    };
    let recorded = |payload: &serde_json::Value| {
        SqliteQueue::open(&db)
            .unwrap()
            .run_events(run.id())
            .unwrap()
            .into_iter()
            .filter(|event| event.payload == *payload)
            .count()
    };
    for orphan in [
        landing.clone().of_landing(EventId::new(next)),
        landing.clone(),
    ] {
        let file = issue(&queue_dir, &orphan, 0).unwrap().file;
        let payload = serde_json::json!({"orphan": format!("{:?}", orphan.landing)});
        let refusal = send(
            &file,
            "record_runtime_event",
            serde_json::json!([run.id(), "integration_rebased", payload]),
        )
        .unwrap_err();
        assert!(
            format!("{refusal:#}").contains("holds the token of no landing that began"),
            "{refusal:#}"
        );
        assert_eq!(recorded(&payload), 0);
        let closing = send(
            &file,
            "close_run_asks",
            serde_json::json!([run.id(), "landed"]),
        );
        assert!(closing.is_err(), "{closing:?}");
        dagq::infrastructure::queue_service::revoke(&queue_dir, &orphan.credential_key()).unwrap();
    }

    let mut writer = queues.open().unwrap();
    writer.begin_integration(run.id(), &token, &main).unwrap();
    assert_eq!(status(), RunStatus::Integrating);
    let began = started();
    assert_eq!(began.len(), 1);
    assert_eq!(integrators(), [landing.clone().of_landing(began[0])]);
    writer.act_as(Integrator::of_process(pid).actor().clone());
    let during = serde_json::json!({"during": "the landing"});
    writer
        .record_runtime_event(run.id(), EventKind::IntegrationRebased, during.clone())
        .unwrap();
    writer
        .hold_integration(run.id(), &token, "held", serde_json::json!({}))
        .unwrap();
    // Its records after it has left `integrating` are still taken.
    let after = serde_json::json!({"after": "the landing"});
    writer
        .record_runtime_event(run.id(), EventKind::IntegrationRebased, after.clone())
        .unwrap();
    writer.act_as(ActorContext::instance(ActorRole::Supervisor, pid));
    assert_eq!(status(), RunStatus::AwaitingIntegration);
    assert_eq!((recorded(&during), recorded(&after)), (1, 1));

    let bare = caller.call(
        UseCase::Supervisor,
        serde_json::json!({"op": "begin_integration", "args": [run.id(), token, main]}),
    );
    assert!(
        format!("{:#}", bare.unwrap_err()).contains("not an operation of the supervisor"),
        "a bare begin_integration was answered"
    );
    caller.sign_out();
}

/// A supervisor on the socket asked to hand off begins no landing
/// (ADR-t3077-1 decision 1): the service refuses it in the write that
/// would record `integration_started`, and the supervisor reads the
/// refusal as the queue's own (`HandoffAsked`), nothing written. One
/// whose stop wins over the handoff begins it through the service too.
/// The queue is never opened here.
#[test]
fn a_supervisor_on_the_socket_asked_to_hand_off_begins_no_landing() {
    use dagq::application::RunReads;
    use dagq::domain::{EventKind, RunStatus};
    use dagq::infrastructure::sqlite::SqliteQueue;
    let (_fixture, repo, db) = fixture();
    let _served = common::service::Served::start(&db);
    let pid = std::process::id();
    let caller: Arc<dyn ServiceCaller> = Arc::new(SocketCaller::new(&db));
    let queues = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    );
    let token = LeaseToken::new("supervisor-handing-off");
    caller.sign_in(&token, pid).unwrap();
    let mut queue = queues.open().unwrap();
    let limits = SlotLimits::resolve(SlotFlags::default(), &SupervisorConfig::default());
    queue
        .register_supervisor(&token, pid, limits, &[], "0.0.0-test")
        .unwrap();
    let main = head(&repo);
    let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&main, &token).unwrap() else {
        panic!("no task was claimed");
    };
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration' WHERE id=?1",
            [run.id().as_str()],
        )
        .unwrap();
    let direct = SqliteQueue::open(&db).unwrap();
    direct.accept_handoff(&token).unwrap();
    assert!(direct.request_handoff(&token, "/next/dagq").unwrap());
    drop(direct);
    let refused = refuse_opening(&db);
    let asked = queue
        .begin_integration(run.id(), &token, &main)
        .unwrap_err();
    let handoff = asked
        .downcast_ref::<dagq::application::HandoffAsked>()
        .unwrap_or_else(|| panic!("not a handoff's refusal: {asked:#}"));
    assert_eq!(handoff.binary, "/next/dagq");
    let landing = queue
        .begin_integration_over_handoff(run.id(), &token, &main)
        .unwrap();
    drop(refused);
    assert_eq!(landing.status(), RunStatus::Integrating);
    let started = SqliteQueue::open(&db)
        .unwrap()
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == EventKind::IntegrationStarted)
        .count();
    assert_eq!(started, 1);
    assert_eq!(
        RunReads::run(&SqliteQueue::open(&db).unwrap(), run.id())
            .unwrap()
            .status(),
        RunStatus::Integrating
    );
    caller.sign_out();
}

/// The landing's end on `main` is the integrator's alone (ADR-t728-2,
/// docs/design/security.md「reviewのpassとIntegrator」): a supervisor that
/// sends `finish_integration` with its own token and under its own lease
/// is refused, and the run stays integrating, its task in progress, with
/// no `run_integrated` and no event of the refusal's run added.
#[test]
fn a_supervisor_is_refused_the_integrators_end_of_a_landing() {
    use dagq::application::{Landing, RunReads, TaskReads};
    use dagq::domain::queue_service::UseCase;
    use dagq::domain::{EventKind, RunStatus, TaskStatus};
    use dagq::infrastructure::sqlite::SqliteQueue;
    let (_fixture, repo, db) = fixture();
    let _served = common::service::Served::start(&db);
    let pid = std::process::id();
    let caller: Arc<dyn ServiceCaller> = Arc::new(SocketCaller::new(&db));
    let queues = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    );
    let token = LeaseToken::new("supervisor-not-the-integrator");
    caller.sign_in(&token, pid).unwrap();
    let mut queue = queues.open().unwrap();
    let limits = SlotLimits::resolve(SlotFlags::default(), &SupervisorConfig::default());
    queue
        .register_supervisor(&token, pid, limits, &[], "0.0.0-test")
        .unwrap();
    let main = head(&repo);
    let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&main, &token).unwrap() else {
        panic!("no task was claimed");
    };
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration' WHERE id=?1",
            [run.id().as_str()],
        )
        .unwrap();
    queue.begin_integration(run.id(), &token, &main).unwrap();
    let state = || {
        let mut direct = SqliteQueue::open(&db).unwrap();
        let events = direct.run_events(run.id()).unwrap();
        (
            RunReads::run(&direct, run.id()).unwrap().status(),
            TaskReads::show(&mut direct, run.task_id())
                .unwrap()
                .task
                .status(),
            events.len(),
            events
                .iter()
                .any(|event| event.kind == EventKind::RunIntegrated),
        )
    };
    let before = state();
    assert_eq!(
        before,
        (
            RunStatus::Integrating,
            TaskStatus::InProgress,
            before.2,
            false
        )
    );
    let landing = Landing {
        commit: main.clone(),
        source_commit: main.clone(),
        main_before: main.clone(),
        history_ref: format!("refs/dagq/runs/{}", run.id()),
        message: "feat: landed by the supervisor itself".into(),
        verification_skipped: false,
    };
    let refused = caller
        .call(
            UseCase::Supervisor,
            serde_json::json!({
                "op": "finish_integration",
                "args": [run.id(), token, landing, "/repo/.git"],
            }),
        )
        .unwrap_err();
    assert!(
        format!("{refused:#}").contains("finish_integration is not an operation of the supervisor"),
        "{refused:#}"
    );
    assert_eq!(state(), before);
    caller.sign_out();
}

/// The records of the container's parts a supervisor makes and reads (the
/// image's builds, the relays' attention, the runs that own users of the
/// dagq machine) go through the service as the supervisor's, while its
/// process refuses to open the queue; another queue's runs are not
/// reached. Once the service stops, each fails as out of reach (a run
/// reads as unknown), still without opening the queue.
#[test]
fn the_supervisors_container_records_go_through_the_service_alone() {
    use dagq::application::{broker_relay::RelayRecords, container_image::ImageRecords};
    use dagq::compose::ServiceContainerRecords;
    use dagq::domain::{EventKind, broker_relay::RelayRun};
    use dagq::infrastructure::podman_machine::OwnerRuns;
    use serde_json::json;
    let (fixture, repo, db) = fixture();
    let served = common::service::Served::start(&db);
    let pid = 4243;
    let caller: Arc<dyn ServiceCaller> = Arc::new(SocketCaller::new(&db));
    let queues = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    );
    let token = LeaseToken::new("supervisor-of-containers");
    caller.sign_in(&token, pid).unwrap();
    let mut queue = queues.open().unwrap();
    let limits = SlotLimits::resolve(SlotFlags::default(), &SupervisorConfig::default());
    queue
        .register_supervisor(&token, pid, limits, &[], "0.0.0-test")
        .unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&head(&repo), &token).unwrap()
    else {
        panic!("no task was claimed");
    };
    let refused = refuse_opening(&db);
    let records = ServiceContainerRecords::new(queues.clone(), &db);

    assert_eq!(records.latest().unwrap(), None);
    records
        .record(
            EventKind::ContainerImageBuilt,
            json!({"reference": "localhost/dagq-q:ab"}),
        )
        .unwrap();
    let (kind, payload) = records.latest().unwrap().unwrap();
    assert!(kind == EventKind::ContainerImageBuilt, "{kind:?}");
    assert_eq!(payload["reference"], "localhost/dagq-q:ab");
    assert_eq!(records.down().unwrap(), None);
    queue
        .record_queue_event(
            EventKind::QueueRelayDown,
            json!({"message": "the relay of r1 is down"}),
        )
        .unwrap();
    assert_eq!(
        records.down().unwrap().as_deref(),
        Some("the relay of r1 is down")
    );
    assert_eq!(
        records.run(&db, run.id().as_str()),
        Some(RelayRun::Found {
            status: run.status(),
            task_in_progress: true,
        })
    );
    let other = fixture.path().join("other").join("queue.db");
    assert_eq!(records.run(&other, "r2"), None);

    drop(served);
    for error in [
        records.latest().unwrap_err(),
        records.down().unwrap_err(),
        records
            .record(EventKind::ContainerImageFailed, json!({}))
            .unwrap_err(),
    ] {
        assert!(unreachable(&error), "{error:#}");
    }
    assert_eq!(records.run(&db, run.id().as_str()), Some(RelayRun::Unknown));
    drop(refused);
    let built: Vec<_> = dagq::infrastructure::sqlite::SqliteQueue::open(&db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == EventKind::ContainerImageBuilt)
        .collect();
    assert_eq!(built.len(), 1);
    assert_eq!(
        built[0].actor.as_ref().map(|actor| actor.id.as_str()),
        Some("supervisor:4243")
    );
    caller.sign_out();
}
