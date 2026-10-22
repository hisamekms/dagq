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
        supervisor_service::{ServiceCaller, ServiceTokens, SupervisorQueues, unreachable},
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
