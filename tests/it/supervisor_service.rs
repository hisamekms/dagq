//! The supervisor's queue through the queue service (ADR-t3102-3
//! decisions 3 to 5, docs/design/queue-service.md): its registration, the
//! claim, the leases, a run's transitions and events, the reads it decides
//! on and the tokens it issues reach the queue only through the service's
//! socket, as the supervisor principal's, and a service that does not
//! answer is no reason to open the queue instead.

use crate::common;
use crate::runtime_support::fixture;

use std::{
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use dagq::{
    application::{
        Queue, QueueOpener,
        queue_service::ServiceAccess,
        supervisor_service::{
            ServiceCaller, ServiceTokens, ServiceUnreachable, SupervisorQueues, unreachable,
        },
    },
    domain::{
        ActorContext, ActorRole, ClaimOutcome, CommitSha, LeaseToken, Reason, ReasonCode,
        queue_service::Principal,
        slot_limits::{SlotFlags, SlotLimits, SupervisorConfig},
    },
    infrastructure::queue_service::{SocketCaller, SystemServiceAccess},
};

/// The connection the supervisor's queue opens for what the service does
/// not answer yet: counted, and refused, so a use of it fails the test.
#[derive(Default)]
struct Unopened(AtomicUsize);

impl QueueOpener for Unopened {
    fn open(&self) -> anyhow::Result<Box<dyn Queue + Send>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("the supervisor's path opened the queue's DB")
    }
}

fn head(repo: &Path) -> CommitSha {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .unwrap();
    CommitSha::parse(String::from_utf8(out.stdout).unwrap().trim(), "head").unwrap()
}

/// The registration, the claim, the leases' heartbeat, a run's
/// transition, its events, the reads of the candidates, the task, the
/// graph and the runs, and
/// the issue and the revocation of the run's worker's token all go through
/// the service: the queue's own connection is never opened. Once the
/// service stops, the next operation fails as out of reach, still without
/// opening the queue.
#[test]
fn the_supervisors_operations_reach_the_queue_only_through_the_service() {
    let (_fixture, repo, db) = fixture();
    let served = common::service::Served::start(&db);
    let pid = 4242;
    let caller: Arc<dyn ServiceCaller> = Arc::new(SocketCaller::new(&db));
    let local = Arc::new(Unopened::default());
    let queues = || SupervisorQueues {
        service: caller.clone(),
        local: local.clone(),
        actor: ActorContext::instance(ActorRole::Supervisor, pid),
    };
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
    let file = tokens.issue(&db, &worker).unwrap();
    assert!(file.is_file());
    assert!(tokens.revoke(&db, &worker.actor_id).unwrap());
    assert_eq!(local.0.load(Ordering::SeqCst), 0);

    drop(served);
    let error = queue.active_runs().unwrap_err();
    assert!(unreachable(&error), "{error:#}");
    assert_eq!(local.0.load(Ordering::SeqCst), 0);
    caller.sign_out();
}

/// The service's caller that loses answers after the service did the
/// calls, as a service that stops or is replaced between its commit and
/// its answer: the answer of the first call of the first set of operations
/// it was given, then of the next set's first call after that, and so on.
struct LosingAnswers {
    inner: Arc<dyn ServiceCaller>,
    lose: std::sync::Mutex<Vec<&'static [&'static str]>>,
    lost: std::sync::Mutex<Vec<String>>,
}

impl ServiceCaller for LosingAnswers {
    fn call(
        &self,
        use_case: dagq::domain::queue_service::UseCase,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let op = params["op"].as_str().unwrap_or_default().to_owned();
        let answer = self.inner.call(use_case, params)?;
        let mut lose = self.lose.lock().unwrap();
        if lose.first().is_some_and(|ops| ops.contains(&op.as_str())) {
            lose.remove(0);
            self.lost.lock().unwrap().push(op.clone());
            return Err(
                ServiceUnreachable::maybe_done(format!("the answer to {op} was lost")).into(),
            );
        }
        Ok(answer)
    }
    fn sign_in(&self, registration: &LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.sign_in(registration, pid)
    }
    fn sign_out(&self) {
        self.inner.sign_out();
    }
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
    crate::runtime_e2e::with_e2e_paths(&repo, "[\"change.txt\"]");
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
