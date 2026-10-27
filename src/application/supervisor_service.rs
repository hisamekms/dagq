//! The supervisor's queue through the queue service ([Queue service],
//! ADR-t3102-3 decisions 3 to 5, goal 182's stage (5)): the claim, the
//! leases, the runs' transitions with their events, the registration, the
//! reads the supervisor decides on and the tokens of the principals it
//! starts are each one use case of the service, done in one
//! transaction on its side as the supervisor's principal (ADR-t3102-1
//! decisions 2 and 3); nothing of one is split across round trips.
//!
//! [`SupervisorQueue`] is the [`Queue`] the supervisor's loop, its threads
//! and the integrator it lands with work on: every operation of the queue
//! (the claim and the runs', the landing's, the review's and the
//! validation's records, the jobs' starts and verdicts, the planning, the
//! observation, the asks and the attention, the host's records) goes to
//! the service by its name ([`SupervisorParams::op`]), and the supervisor
//! opens no DB. On the service's side ([`answer`]) each operation is the
//! queue's own, so its lease rule and its transaction are the queue's, and
//! every lease token it names has to be the principal's registration: a
//! supervisor writes under no other supervisor's lease.
//!
//! The service out of reach is [`ServiceUnreachable`]: the supervisor
//! never opens the DB for an operation the service answers, and its loop
//! waits for the service instead ([`unreachable`]).
//!
//! The supervisor's token is issued after the hello and before the
//! registration ([`ServiceCaller::sign_in`]) by the service, at the
//! request of the user (the socket's peer) as its parent (ADR-t3102-1
//! decision 2), and revoked when the loop ends, but kept across a
//! handoff's exec for the image that takes it back
//! ([`ServiceCaller::take_back`]), so the `observe` and
//! `throughput-review` it started record across the exec. The tokens of
//! the principals the supervisor starts ([`TOKEN_OPS`]) are issued by the
//! service at the supervisor's request, in the transaction that checks
//! what the supervisor holds of their target, and the integrator's with
//! the landing slot (`begin_landing`). Its actor id is
//! `supervisor:<pid>`, its events' actor. The events name another actor only when the landing writes as
//! the integrator and when a job's verdict is applied (`requested_by`),
//! and only an actor of the control side. A principal without
//! `scheduler.supervise` is refused with `authorization_denied`, and the
//! refusal is recorded.
//!
//! [Queue service]: ../../docs/design/queue-service.md

pub mod control;
mod dispatch;
mod forward;
mod needs;
pub mod resend;
mod wire;

pub use wire::Wire;

use std::cell::RefCell;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, ensure};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::queue_service::{ServiceBackend, ServiceQueue};
use super::{ObserverLog, Queue, QueueBusy, QueueOpener, RunNotFound, RunReads};
use crate::domain::authorization::Capability;
use crate::domain::queue_service::{
    FailureKind, Principal, ServiceError, SupervisorParams, UseCase,
};
use crate::domain::session_step::StepTarget;
use crate::domain::{ActorContext, ActorRole, LeaseToken, TaskId};

/// How the supervisor reaches its queue's service: the socket of the
/// service's process in production, or the service's use cases run in
/// this process on a backend of its own (a test's supervisor, which keeps
/// no service).
pub trait ServiceCaller: Send + Sync {
    /// The `result` of `use_case` with `params`, as the principal this
    /// caller signed in as; the service's refusal or failure as an error
    /// ([`service_error`]), and [`ServiceUnreachable`] when the service did
    /// not answer.
    fn call(&self, use_case: UseCase, params: Value) -> Result<Value>;
    /// [`Self::call`] of a record that is never kept to be sent later
    /// (ADR-t3102-3 decision 5): one its caller makes before what it starts,
    /// which goes on only once the queue took it, or one a pass finds
    /// against the queue's history, which the next pass finds again
    /// ([`crate::application::EventStore::record_runtime_event_now`]). One
    /// the queue did not take is its caller's error; a caller that keeps
    /// the records of results ([`resend::Resending`]) sends it after those.
    fn call_now(&self, use_case: UseCase, params: Value) -> Result<Value> {
        self.call(use_case, params)
    }
    /// A caller of the same service with the token in `file`, which the
    /// service issued to another principal (a landing's integrator,
    /// ADR-t3102-1 decision 2): it neither signs in nor revokes it.
    fn with_token(&self, file: &std::path::Path) -> Result<Arc<dyn ServiceCaller>>;
    /// Become the supervisor of `registration`, process `pid`: the token
    /// of its principal ([`Principal::supervisor`]) is issued, replacing
    /// the one held before. Until then every call is refused.
    fn sign_in(&self, registration: &LeaseToken, pid: u32) -> Result<()>;
    /// Become again the supervisor of `registration`, process `pid`, as the
    /// image a handoff exec'd into: the token the image before kept
    /// ([`Self::hand_over`]) while it is still issued. With none,
    /// [`Self::credential`] stays `None`, and [`Self::sign_in`] asks for a
    /// new one once the service answers.
    fn take_back(&self, registration: &LeaseToken, pid: u32) -> Result<()>;
    /// Keep the principal's token for the image this process execs into (a
    /// handoff): it is not revoked, so a job command that calls with it
    /// goes on across the exec, and this caller calls with none.
    fn hand_over(&self);
    /// Give the principal's token back, at the supervisor's end.
    fn sign_out(&self);
    /// The file of the token this caller signed in with, which a command
    /// the supervisor starts is given to ask the service for its job's
    /// token as the supervisor (ADR-t3102-1 decision 2); `None` before the
    /// sign-in. A caller that wraps another is to answer the other's.
    fn credential(&self) -> Option<std::path::PathBuf>;
    /// Send the records of results this caller held while the service did
    /// not take them ([`resend::Resend::Hold`]), oldest first: whether none
    /// is still held (`false` when the queue stayed busy), or an error, the
    /// rest still held, when the service still does not answer.
    fn send_held(&self) -> Result<bool> {
        Ok(true)
    }
    /// Whether records this caller holds wait for a queue that stayed busy
    /// for them at their last sending ([`Self::send_held`]): the
    /// supervisor's pass waits for them, so nothing it writes goes ahead of
    /// them.
    fn holds_for_busy_queue(&self) -> bool {
        false
    }
    /// Keep `params` of `use_case` with the records held, to be sent by
    /// [`Self::send_held`] here or by the next supervisor: a job's end the
    /// supervisor could not write before it ends.
    fn keep(&self, use_case: UseCase, params: Value) -> Result<()> {
        let _ = (use_case, params);
        Err(anyhow!("this caller keeps no record"))
    }
}

/// The queue service did not answer (no socket, a process that does not
/// accept or does not answer in time), and the queue was not opened
/// instead (fail closed).
#[derive(Debug, Clone)]
pub struct ServiceUnreachable {
    pub reason: String,
    /// Whether the service may have done the request: not when no
    /// connection took it, nor when it was a read, which changes nothing;
    /// a request written whose answer did not come may have been done.
    pub maybe_done: bool,
}

impl ServiceUnreachable {
    /// A request the service may have done without its answer coming.
    pub fn maybe_done(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            maybe_done: true,
        }
    }

    /// A request the service did not do.
    pub fn not_done(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            maybe_done: false,
        }
    }
}

impl std::fmt::Display for ServiceUnreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the queue service did not answer: {}", self.reason)
    }
}

impl std::error::Error for ServiceUnreachable {}

/// The pid of the supervisor that started a job command as its own,
/// which `actor` (the command's, from its environment) must be: the
/// command reaches the queue as that supervisor only, never as another
/// actor.
pub fn job_command_pid(actor: &crate::domain::ActorContext) -> Result<u32> {
    actor.supervisor_pid().ok_or_else(|| {
        anyhow::anyhow!(
            "a job command reaches the queue as the supervisor that started it, not as {}",
            actor.actor_id()
        )
    })
}

/// Whether `error` is the service out of reach ([`ServiceUnreachable`]).
pub fn unreachable(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<ServiceUnreachable>().is_some())
}

/// Whether `error` is the service out of reach for a request it did not
/// do ([`ServiceUnreachable::maybe_done`] false).
pub fn undone(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<ServiceUnreachable>()
            .is_some_and(|unreachable| !unreachable.maybe_done)
    })
}

/// The operations of [`dispatch`] that only read the queue: one the
/// service did not answer did nothing ([`undone`]). An operation not here
/// is taken as one that writes.
const READ_OPS: [&str; 59] = [
    "candidates",
    "build_waits",
    "predecessors",
    "goal_predecessors",
    "tasks_in_progress",
    "task_fenced",
    "list_goals",
    "active_runs",
    "all_runs",
    "run",
    "runs_with_status",
    "next_awaiting_integration",
    "ended_run_workspaces",
    "ended_run_worktrees",
    "ended_run_worktree",
    "latest_runs_in_progress",
    "runs_with_pending_push",
    "run_in_workspace",
    "replan_guard",
    "turn_fence",
    "settling_holds",
    "open_hold",
    "latest_run_of",
    "parking_replans",
    "replans_of_run",
    "runs_leased_by_others",
    "runs_leased_by",
    "unclosed_run_asks",
    "runs_needing_session",
    "hold_guard",
    "held_claims",
    "holds_lease",
    "run_leases",
    "run_lease",
    "processes",
    "backend_slots",
    "supervisors",
    "handoff_request",
    "repository_binding",
    "update_events",
    "installs_after",
    "e2e_gate_events",
    "latest_task_events",
    "run_events",
    "task_events",
    "has_run_event",
    "last_observe",
    "latest_event_id",
    "latest_event_of",
    "latest_events_of",
    "latest_queue_event",
    "latest_events_by_supervisor",
    "events_of_between",
    "events_between",
    "show",
    "graph_input",
    "token_principals",
    "held_telemetry",
    "target_facts",
];

thread_local! {
    /// The writes this thread's [`SupervisorQueue`]s sent or tried to
    /// send to the service, done or not.
    static WRITES_TRIED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// The requests of this thread's [`SupervisorQueue`]s the service did
    /// not answer.
    static UNANSWERED_HERE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Whether a request of a [`SupervisorQueue`] went unanswered since
/// [`take_unanswered`] last looked, on any thread: the loop's, or a
/// landing's or a validation's on its own.
static UNANSWERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How many writes this thread's supervisor queues tried so far: a step
/// that tried none and failed on a request the service did not do
/// ([`undone`]) changed nothing on the queue.
pub fn writes_tried() -> u64 {
    WRITES_TRIED.with(std::cell::Cell::get)
}

/// How many requests of this thread's supervisor queues went unanswered
/// so far, swallowed or not: a caller that goes on past a failure it only
/// logs tells by it whether the failure was the service out of reach.
pub fn unanswered_here() -> u64 {
    UNANSWERED_HERE.with(std::cell::Cell::get)
}

/// Whether a request of a supervisor's queue went unanswered since the
/// last call, on any thread and swallowed or not; the look clears it.
pub fn take_unanswered() -> bool {
    UNANSWERED.swap(false, std::sync::atomic::Ordering::SeqCst)
}

/// The service's refusal or failure `error` of a request as the error the
/// queue's own operation would have returned: a run not found and a busy
/// queue as the queue's, the rest with the service's code and message.
pub fn service_error(error: &ServiceError) -> anyhow::Error {
    match &error.kind {
        Some(FailureKind::RunNotFound(run)) => RunNotFound(run.clone()).into(),
        Some(FailureKind::QueueBusy) => anyhow!(error.message.clone()).context(QueueBusy),
        Some(FailureKind::HandoffAsked(refusal)) => super::HandoffAsked {
            supervisor: refusal.supervisor.clone(),
            binary: refusal.binary.clone(),
        }
        .into(),
        None => Refused {
            code: error.code,
            message: error.message.clone(),
        }
        .into(),
    }
}

/// The service's refusal or failure of a request with no kind the caller
/// tells apart ([`service_error`]), kept typed so a caller can read its
/// code.
#[derive(Debug)]
pub struct Refused {
    pub code: crate::domain::queue_service::ServiceErrorCode,
    pub message: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the queue service refused the request ({}): {}",
            self.code.as_str(),
            self.message
        )
    }
}

impl std::error::Error for Refused {}

impl Refused {
    /// Whether `error` is the service's refusal of the request as one it
    /// does not read (`bad_request`).
    pub fn bad_request(error: &anyhow::Error) -> bool {
        error.downcast_ref::<Self>().is_some_and(|refused| {
            refused.code == crate::domain::queue_service::ServiceErrorCode::BadRequest
        })
    }
}

/// Whether `error` is the service's refusal of an answer past its limit
/// ([`crate::domain::queue_service::ServiceErrorCode::ResponseTooLarge`]).
pub fn too_large(error: &anyhow::Error) -> bool {
    let code = crate::domain::queue_service::ServiceErrorCode::ResponseTooLarge.as_str();
    format!("{error:#}").contains(&format!("({code})"))
}

/// The kind of a use case's failure `error` the caller tells apart
/// ([`FailureKind`]), if any.
pub fn failure_kind(error: &anyhow::Error) -> Option<FailureKind> {
    if QueueBusy::is(error) {
        return Some(FailureKind::QueueBusy);
    }
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<RunNotFound>()
            .map(|RunNotFound(run)| FailureKind::RunNotFound(run.clone()))
            .or_else(|| {
                cause.downcast_ref::<super::HandoffAsked>().map(|asked| {
                    FailureKind::HandoffAsked(Box::new(
                        crate::domain::queue_service::HandoffRefusal {
                            supervisor: asked.supervisor.clone(),
                            binary: asked.binary.clone(),
                        },
                    ))
                })
            })
    })
}

/// Opens a [`SupervisorQueue`] for each connection the supervisor needs,
/// all through one caller.
#[derive(Clone)]
pub struct SupervisorQueues {
    pub service: Arc<dyn ServiceCaller>,
    /// The supervisor principal's actor, which its events record.
    pub actor: ActorContext,
    /// The landing whose integration slot one of its queues took, and the
    /// file of its integrator's token, until the landing ends.
    landing: Arc<std::sync::Mutex<Option<(crate::domain::RunId, std::path::PathBuf)>>>,
}

impl SupervisorQueues {
    /// The queues of the supervisor principal `actor` through `service`.
    pub fn new(service: Arc<dyn ServiceCaller>, actor: ActorContext) -> Self {
        Self {
            service,
            actor,
            landing: Arc::default(),
        }
    }

    /// The file of the token of the integrator whose landing's slot one of
    /// these queues took, until the landing ends.
    pub fn landing_token(&self) -> Option<std::path::PathBuf> {
        self.landing
            .lock()
            .expect("the landing lock")
            .as_ref()
            .map(|(_, file)| file.clone())
    }

    /// One queue of the supervisor's, through the service.
    pub fn queue(&self) -> SupervisorQueue {
        SupervisorQueue {
            service: self.service.clone(),
            actor: self.actor.clone(),
            landing: self.landing.clone(),
            integrator: RefCell::new(None),
            tokenless: std::cell::Cell::new(false),
            requested_by: RefCell::new(None),
        }
    }
}

impl QueueOpener for SupervisorQueues {
    fn open(&self) -> Result<Box<dyn Queue + Send>> {
        Ok(Box::new(self.queue()))
    }
}

/// Opens the supervisor's queues as only the ports `P` a context takes
/// (its observation, its host's operations, its sessions' records): each a
/// [`SupervisorQueue`], boxed as `P` by `keep`.
pub struct SupervisorPorts<P: ?Sized> {
    pub queues: SupervisorQueues,
    pub keep: fn(SupervisorQueue) -> Box<P>,
}

impl<P: ?Sized> QueueOpener<P> for SupervisorPorts<P> {
    fn open(&self) -> Result<Box<P>> {
        Ok((self.keep)(self.queues.queue()))
    }
}

/// The supervisor's queue: each of its operations one request to the
/// service.
pub struct SupervisorQueue {
    service: Arc<dyn ServiceCaller>,
    actor: ActorContext,
    /// The landing whose slot the supervisor took, shared by its queues
    /// ([`SupervisorQueues::landing`]).
    landing: Arc<std::sync::Mutex<Option<(crate::domain::RunId, std::path::PathBuf)>>>,
    /// While the queue is written as the landing's integrator
    /// ([`EventStore::act_as`]): its actor, its run, and the caller of its
    /// token.
    integrator: RefCell<Option<Landing>>,
    /// Written as the integrator with no token of its to call with: every
    /// call fails rather than going as the supervisor.
    tokenless: std::cell::Cell<bool>,
    /// The `requested_by` of the events written ([`EventStore::request_as`]).
    requested_by: RefCell<Option<String>>,
}

impl SupervisorQueue {
    /// A read of the whole queue of a job the supervisor started, as the
    /// command of its name computes it on the service's side.
    pub fn job_read(&self, read: &super::queue_reads::JobRead) -> Result<Value> {
        self.remote("job_read", vec![to_value(read)?])
    }

    /// Record `finding` as the supervisor's (a throughput review's): its id.
    pub fn record_finding(&self, finding: &crate::domain::NewFinding) -> Result<i64> {
        self.remote("record_finding", vec![to_value(finding)?])
    }

    /// The operation `op` of the service with `args`, read back as `T`:
    /// as the landing's integrator, with its token. A record of a result
    /// the service does not answer is sent again as the caller's
    /// [`resend::Resend`] says.
    fn remote<T: Wire>(&self, op: &str, args: Vec<Value>) -> Result<T> {
        self.send(op, args, false)
    }

    /// [`Self::remote`] of a record that is never kept to be sent later
    /// ([`ServiceCaller::call_now`]).
    fn remote_now<T: Wire>(&self, op: &str, args: Vec<Value>) -> Result<T> {
        self.send(op, args, true)
    }

    fn send<T: Wire>(&self, op: &str, args: Vec<Value>, now: bool) -> Result<T> {
        ensure!(
            !self.tokenless.get(),
            "the landing's integrator holds no token, so its {op} is not sent"
        );
        let params = serde_json::to_value(SupervisorParams {
            op: op.to_owned(),
            args,
            acting: None,
            requested_by: self.requested_by.borrow().clone(),
            record: None,
        })?;
        let integrator = self
            .integrator
            .borrow()
            .as_ref()
            .map(|(_, _, caller)| caller.clone());
        let read = READ_OPS.contains(&op);
        if !read {
            WRITES_TRIED.with(|tried| tried.set(tried.get() + 1));
        }
        let caller = integrator.unwrap_or_else(|| self.service.clone());
        let answered = if now {
            caller.call_now(UseCase::Supervisor, params)
        } else {
            caller.call(UseCase::Supervisor, params)
        };
        let value = match answered {
            Ok(value) => value,
            Err(error) => {
                let Some(lost) = error.downcast_ref::<ServiceUnreachable>() else {
                    return Err(error);
                };
                UNANSWERED.store(true, std::sync::atomic::Ordering::SeqCst);
                UNANSWERED_HERE.with(|seen| seen.set(seen.get() + 1));
                // A read the service did not answer did nothing.
                return Err(if read && lost.maybe_done {
                    anyhow::Error::new(ServiceUnreachable::not_done(lost.reason.clone()))
                } else {
                    error
                });
            }
        };
        let form: T::Form = serde_json::from_value(value)
            .with_context(|| format!("the queue service's answer to {op} does not read"))?;
        T::from_form(form)
    }

    /// Take the integration slot of run `id` under `token` on `main`
    /// (whatever handoff the supervisor is asked when `over_handoff`), and
    /// keep the file of the integrator's token the service issued with it
    /// for the landing ([`EventStore::act_as`]).
    fn begin_landing(
        &self,
        id: &crate::domain::RunId,
        token: &LeaseToken,
        main: &crate::domain::CommitSha,
        over_handoff: bool,
    ) -> Result<crate::domain::TaskRun> {
        let begun: Value = self.remote(
            BEGIN_LANDING,
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(main)?,
                to_value(over_handoff)?,
            ],
        )?;
        let run_form = serde_json::from_value(begun["run"].clone())
            .context("the queue service's answer to begin_landing does not read")?;
        let run = crate::domain::TaskRun::from_form(run_form)?;
        let file: std::path::PathBuf = serde_json::from_value(begun["integrator"].clone())
            .context("the queue service's answer to begin_landing names no token")?;
        *self.landing.lock().expect("the landing lock") = Some((id.clone(), file));
        Ok(run)
    }

    /// Write as `actor`: the landing's integrator calls with the token
    /// issued as its slot was taken, and the supervisor's own actor
    /// again revokes it, the landing having left `integrating`.
    fn write_as(&self, actor: ActorContext) -> Option<ActorContext> {
        let previous = self
            .integrator
            .borrow()
            .as_ref()
            .map_or_else(|| self.actor.clone(), |(actor, _, _)| actor.clone());
        if actor.role() == ActorRole::Integrator {
            let landing = self.landing.lock().expect("the landing lock").clone();
            match landing.map(|(run, file)| self.service.with_token(&file).map(|c| (run, c))) {
                Some(Ok((run, caller))) => {
                    self.integrator.replace(Some((actor, run, caller)));
                }
                Some(Err(error)) => {
                    tracing::warn!("the integrator's token could not be read: {error:#}");
                    self.tokenless.set(true);
                }
                None => {
                    tracing::warn!("no landing holds an integrator's token");
                    self.tokenless.set(true);
                }
            }
        } else if self.tokenless.replace(false) {
            // Nothing was written as the integrator; the supervisor's sweep
            // revokes a token the landing could not read.
        } else if let Some((_, run, _)) = self.integrator.take() {
            let mut landing = self.landing.lock().expect("the landing lock");
            if landing.as_ref().is_some_and(|(held, _)| *held == run) {
                *landing = None;
            }
            drop(landing);
            if let Err(error) =
                to_value(&run).and_then(|run| self.remote::<bool>(END_LANDING, vec![run]))
            {
                tracing::warn!(run_id = %run, "the integrator's token of run {run} could not be revoked: {error:#}");
            }
        }
        Some(previous)
    }
}

/// The landing's integrator a [`SupervisorQueue`] writes as: its actor, its
/// run, and the caller of its token.
type Landing = (ActorContext, crate::domain::RunId, Arc<dyn ServiceCaller>);

fn to_value(value: impl serde::Serialize) -> Result<Value> {
    Ok(serde_json::to_value(value)?)
}

/// The queue the service runs a supervisor's operations on: the whole
/// queue, each operation of it answered by [`dispatch`].
pub trait SupervisorStore: Queue + ObserverLog {}

impl<T: Queue + ObserverLog + ?Sized> SupervisorStore for T {}

/// A supervisor's operation whose arguments do not read as the
/// operation's, or one the service does not know.
#[derive(Debug)]
pub struct BadOperation(pub String);

impl std::fmt::Display for BadOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BadOperation {}

/// A lease token named that is not the principal's registration.
#[derive(Debug)]
pub struct NotOwnLease;

impl std::fmt::Display for NotOwnLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the lease token named is not the supervisor principal's registration")
    }
}

impl std::error::Error for NotOwnLease {}

/// The operations on the tokens of the principals the supervisor starts
/// (ADR-t3102-1 decision 2): each is done by the service, a worker's and a
/// run's executor's under the run's lease checked in the same transaction.
const TOKEN_OPS: [&str; 9] = [
    "issue_token",
    "revoke_token",
    "revoke_principal",
    "token_principals",
    "issue_telemetry",
    "revoke_telemetry",
    "held_telemetry",
    "target_facts",
    "register_telemetry_session",
];

/// The operations of the jobs the supervisor starts as commands of its
/// own (an observation, a throughput review), which reach the queue as
/// its principal: their reads of the whole queue, as the commands of their
/// names compute them on the service's side, and the throughput review's
/// finding.
const JOB_OPS: [&str; 2] = ["job_read", "record_finding"];

/// The job operations ([`JOB_OPS`]) of `params` on `queue`.
fn jobs(queue: &mut dyn ServiceQueue, params: &SupervisorParams) -> Result<Value> {
    let op = params.op.as_str();
    let args = Value::Array(params.args.clone());
    match op {
        "job_read" => {
            let (read,): (super::queue_reads::JobRead,) = read(op, args)?;
            queue.read(&super::queue_reads::QueueRead::Job(read))
        }
        "record_finding" => {
            let (finding,): (crate::domain::NewFinding,) = read(op, args)?;
            Ok(serde_json::to_value(
                queue.record_finding(finding)?.finding.id.as_i64(),
            )?)
        }
        _ => unreachable!("a job operation"),
    }
}

/// Answer the supervisor `principal`'s request `params` on `backend`, as
/// the service's `supervisor` use case: an operation that is none of the
/// supervisor's is refused, the policy's `scheduler.supervise` and the
/// capability the operation needs ([`needs::supervisor_needs`]) are
/// checked (and a refusal recorded) first, then the operation runs on a
/// queue written as the principal's actor, or as the actor the request
/// names when it is one of the control side's (the landing's integrator),
/// with the request's `requested_by`. `process` is the one that asked,
/// which a token it asks for is kept with ([`tokens`]).
pub fn answer(
    backend: &dyn ServiceBackend,
    principal: &Principal,
    params: &Value,
    process: Option<u32>,
) -> Result<Value> {
    if principal.role == ActorRole::Integrator {
        return integrator(backend, principal, params);
    }
    let actor = principal.actor();
    let registration = match (&principal.registration, principal.role) {
        (Some(registration), ActorRole::Supervisor) => Some(registration.clone()),
        _ => None,
    };
    let mut queue = backend.open(&actor)?;
    super::queue_service::authorize_supervisor(&mut *queue, &actor, registration.is_some())?;
    let registration = registration.expect("authorized above");
    let params: SupervisorParams = serde_json::from_value(params.clone())
        .map_err(|error| BadOperation(format!("the params of supervisor do not read: {error}")))?;
    // Default deny: an operation that is none of the supervisor's is
    // refused, and one is answered only with the capability it needs.
    let needs = needs::supervisor_needs(&params.op, &params.args).map_err(BadOperation)?;
    if needs != Capability::Supervise {
        super::queue_service::authorize_supervisor_needs(&mut *queue, &actor, needs)?;
    }
    let own = |token: &LeaseToken| {
        if *token == registration {
            Ok(())
        } else {
            Err(anyhow::Error::new(NotOwnLease))
        }
    };
    if TOKEN_OPS.contains(&params.op.as_str()) {
        return tokens(
            backend,
            &mut *queue,
            principal,
            &registration,
            &params,
            process,
        );
    }
    if JOB_OPS.contains(&params.op.as_str()) {
        return jobs(&mut *queue, &params);
    }
    // The landing's events are the integrator principal's own: a
    // supervisor writes as no other actor.
    if let Some(acting) = &params.acting
        && acting.actor() != actor
    {
        return Err(BadOperation(format!(
            "a supervisor writes as itself, not as {}",
            acting.actor_id
        ))
        .into());
    }
    // An owner other than the principal's process that still runs.
    let pid = principal
        .actor_id
        .strip_prefix("supervisor:")
        .and_then(|pid| pid.parse::<u32>().ok());
    let alive = |other: u32| Some(other) != pid && backend.alive(other);
    let now = backend.now_ms() / 1000;
    if params.op == BEGIN_LANDING {
        let (run, token, main, over_handoff): (
            crate::domain::RunId,
            LeaseToken,
            crate::domain::CommitSha,
            bool,
        ) = read(&params.op, Value::Array(params.args.clone()))?;
        // Under no lease but its registration: refused and recorded, as
        // the integrator's token would be issued under it (ADR-t3102-1
        // decision 2).
        own_lease(
            &mut *queue,
            principal,
            &registration,
            &token,
            Capability::IntegrationRequest,
            &crate::domain::Resource::run(run.clone()),
        )?;
        queue
            .supervisor_store()
            .restore_request(params.requested_by.clone());
        let landing = Principal::integrator(&run, &token, pid.unwrap_or_default());
        let issuer = crate::domain::queue_service::TokenIssuer::of(
            principal,
            process.and_then(|pid| backend.process_of(pid)),
        );
        return begin_landing(
            backend,
            &mut *queue,
            &landing,
            &main,
            over_handoff,
            LandingBy::Supervisor { now, actor: &actor },
            &issuer,
        );
    }
    if params.op == END_LANDING {
        let (run,): (crate::domain::RunId,) = read(&params.op, Value::Array(params.args))?;
        let landing = Principal::integrator(&run, &registration, pid.unwrap_or_default());
        return reply(
            backend
                .access()
                .revoke(backend.db(), &landing.credential_key()),
        );
    }
    let store = queue.supervisor_store();
    store.restore_request(params.requested_by.clone());
    if let Some(record) = once_record(&params) {
        return dispatch::dispatch_once(store, &params.op, Value::Array(params.args), &record);
    }
    dispatch::dispatch(
        store,
        &own,
        &alive,
        now,
        &params.op,
        Value::Array(params.args),
    )
}

/// Who takes a landing's slot ([`begin_landing`]).
enum LandingBy<'a> {
    /// A supervisor under its registration, which is to be live at `now`
    /// (seconds): the run's lease another's, or the registration not live,
    /// found in the slot's transaction, is refused and recorded as
    /// `actor`'s.
    Supervisor { now: i64, actor: &'a ActorContext },
    /// The command of a person's or the inbox's `integrate`, process
    /// `pid`, which lands in its own process and names it in the lease.
    Command { pid: u32 },
}

/// Take the integration slot of the run `landing` (an integrator
/// principal, [`Principal::integrator`]) lands under its lease on `main`,
/// and issue its token: the run begun and the file of the token. The slot
/// and the token in one transaction: the token is issued once the slot's
/// lease is taken (and, by a supervisor, its registration found live), and
/// the slot is committed only with it (ADR-t3102-3 decision 3, ADR-t3102-1
/// decision 2), so no landing begins that cannot record, and a supervisor
/// that lost the run's lease is issued none. The token names the landing's
/// `integration_started`, so a token whose landing did not commit writes
/// nothing ([`integrator`]); it keeps `issuer`, the principal that asked,
/// as every token the service issues at a parent's request.
fn begin_landing(
    backend: &dyn ServiceBackend,
    queue: &mut dyn ServiceQueue,
    landing: &Principal,
    main: &crate::domain::CommitSha,
    over_handoff: bool,
    by: LandingBy<'_>,
    issuer: &crate::domain::queue_service::TokenIssuer,
) -> Result<Value> {
    let (Some(run), Some(token)) = (landing.landing_run(), landing.registration.clone()) else {
        return Err(BadOperation(format!("{} lands no run", landing.actor_id)).into());
    };
    let holder = match &by {
        LandingBy::Supervisor { now, .. } => {
            super::queue_service::SlotHolder::Supervisor { now: *now }
        }
        LandingBy::Command { pid } => super::queue_service::SlotHolder::Command { pid: *pid },
    };
    let mut issued = false;
    let begun = queue.begin_landing(&run, &token, main, over_handoff, holder, &mut |started| {
        let file = backend.issue_as(&landing.clone().of_landing(started), issuer)?;
        issued = true;
        reply(Ok(file))
    });
    match begun {
        Ok((run, file)) => Ok(serde_json::json!({"run": reply(Ok(run))?, "integrator": file})),
        Err(error) => {
            // A token issued before a commit that failed is of no
            // landing: it goes too.
            if issued
                && let Err(back) = backend
                    .access()
                    .revoke(backend.db(), &landing.credential_key())
            {
                tracing::warn!(run_id = %run, "run {run}: the integrator's token of a landing that did not begin could not be revoked: {back:#}");
            }
            match (error.downcast_ref::<super::queue_service::NotHeld>(), by) {
                (Some(not_held), LandingBy::Supervisor { actor, .. }) => {
                    let why = not_held.0.clone();
                    Err(super::queue_service::refuse_recorded(
                        &mut *queue,
                        actor,
                        Capability::TokenIssue,
                        &crate::domain::Resource::Principal(ActorRole::Integrator),
                        crate::domain::authorization::DenyReason::NotHeld,
                        &why,
                    ))
                }
                _ => Err(error),
            }
        }
    }
}

/// The identity of the result's record `params` send, when the queue takes
/// their event once per identity ([`resend::MATCHED_OPS`]).
fn once_record(params: &SupervisorParams) -> Option<String> {
    params
        .record
        .clone()
        .filter(|_| resend::MATCHED_OPS.contains(&params.op.as_str()))
}

/// The supervisor's operation that takes the integration slot of a run
/// and issues the integrator principal's token for its landing
/// (ADR-t3102-3 decision 3, ADR-t3102-1 decision 2): the run and the file
/// of the token.
const BEGIN_LANDING: &str = "begin_landing";
/// The supervisor's operation that revokes the integrator's token of a
/// run's landing, once the landing left `integrating`.
const END_LANDING: &str = "end_landing";

/// The integrator's operations on its run, the run their first argument
/// (ADR-t3102-3 decision 3's landing): its reads, its events, the ends of
/// its landing, its follow-ups and the asks it closes.
const INTEGRATOR_RUN_OPS: [&str; 16] = [
    "run",
    "run_events",
    "holds_lease",
    "run_lease",
    "record_runtime_event",
    "finish_integration",
    "abort_integration",
    "fail_integration",
    "defer_integration",
    "hold_integration",
    "hand_over_integration",
    "record_cleanup_failure",
    "register_follow_ups",
    "end_stalled_detections",
    "close_run_asks",
    "close_blocked_asks",
];
/// The integrator's operations on its run's task, the task their first
/// argument.
const INTEGRATOR_TASK_OPS: [&str; 4] = [
    "show",
    "close_ended_landing_asks",
    "ci_failure_findings_of",
    "follow_up_depth",
];
/// The integrator's operations on the queue as a whole: the reads its
/// checks make (the CI watch's records among them), the container
/// runtime's events of its verification, and the heartbeat of its lease
/// (a person's landing keeps its own slot's lease alive; a supervisor's
/// is its registration's).
const INTEGRATOR_QUEUE_OPS: [&str; 5] = [
    "latest_events_of",
    "latest_queue_event",
    "ci_watch_events",
    "record_queue_event",
    "heartbeat",
];

/// Whether the integrator of `run`, of `task`, may send `op` with `args`:
/// one of the landing's operations ([`INTEGRATOR_RUN_OPS`],
/// [`INTEGRATOR_TASK_OPS`], [`INTEGRATOR_QUEUE_OPS`]) every argument of
/// which that names a run or a task names its own (a task's none, which
/// would be every task's, is not its own). The queue-wide reads name
/// neither, so they are answered whole: the CI watch's records, from
/// which its verification's known failures are read, among them. Why
/// not, otherwise.
pub(super) fn integrator_scope(
    op: &str,
    args: &[Value],
    run: &crate::domain::RunId,
    task: TaskId,
) -> std::result::Result<(), String> {
    let at = |index: usize| args.get(index).unwrap_or(&Value::Null);
    let names_run = |index: usize| at(index).as_str() == Some(run.as_str());
    let names_task = |index: usize| at(index).as_i64() == Some(task.as_i64());
    let scoped = if INTEGRATOR_RUN_OPS.contains(&op) {
        // The asks a landing closes as blocked are those of its run's task.
        names_run(0) && (op != "close_blocked_asks" || names_task(1))
    } else if INTEGRATOR_TASK_OPS.contains(&op) {
        names_task(0)
    } else {
        INTEGRATOR_QUEUE_OPS.contains(&op)
    };
    if scoped {
        Ok(())
    } else {
        Err(format!(
            "the integrator of run {run} (task {task}) does not {op} with {}",
            Value::Array(args.to_vec())
        ))
    }
}

/// Whether the landing of the integrator `principal` committed: `events`,
/// those of its run, hold the `integration_started` its token was issued
/// for ([`Principal::landing`]). It stays once the landing has left
/// `integrating`, so the landing's records after it are still taken; a
/// token issued in a transaction that did not commit names none. Why not,
/// otherwise.
pub(super) fn landing_committed(
    events: &[crate::domain::RunEvent],
    principal: &Principal,
) -> std::result::Result<(), String> {
    let started = principal.landing.filter(|landing| {
        events.iter().any(|event| {
            event.id == *landing && event.kind == crate::domain::EventKind::IntegrationStarted
        })
    });
    match started {
        Some(_) => Ok(()),
        None => Err(format!(
            "{} holds the token of no landing that began",
            principal.actor_id
        )),
    }
}

/// Whether a supervisor may answer an ask as `answerer`: only as the
/// runtime withdrawing it; an answer as a person or the inbox is theirs to
/// give (`ask.answer`). Why not, otherwise.
pub(super) fn supervisor_answers_as(
    answerer: &crate::domain::Answerer,
) -> std::result::Result<(), String> {
    if *answerer == crate::domain::Answerer::RUNTIME {
        Ok(())
    } else {
        Err(format!(
            "a supervisor answers no ask as {} ({})",
            answerer.by,
            answerer.authority.as_str()
        ))
    }
}

/// Answer the integrator `principal`'s request `params`: the policy's
/// landing is checked (and a refusal recorded) first, then the queue's
/// operation runs written as the integrator, at the request's
/// `requested_by`, on its one run under its one lease. Only the landing's
/// operations are answered ([`INTEGRATOR_RUN_OPS`], [`INTEGRATOR_TASK_OPS`],
/// [`INTEGRATOR_QUEUE_OPS`]); one of another run or task, naming another
/// lease, or of a token whose landing the queue holds no `integration_started`
/// of ([`landing_committed`]), is refused before anything is written.
fn integrator(
    backend: &dyn ServiceBackend,
    principal: &Principal,
    params: &Value,
) -> Result<Value> {
    let actor = principal.actor();
    let (Some(run), Some(lease)) = (principal.landing_run(), principal.registration.clone()) else {
        return Err(BadOperation(format!("{} lands no run", principal.actor_id)).into());
    };
    let mut queue = backend.open(&actor)?;
    super::queue_service::authorize_integrator(&mut *queue, &actor, &run)?;
    let params: SupervisorParams = serde_json::from_value(params.clone())
        .map_err(|error| BadOperation(format!("the params of supervisor do not read: {error}")))?;
    let op = params.op.as_str();
    if params.acting.is_some() {
        return Err(BadOperation(format!("the integrator writes as itself, not {op}")).into());
    }
    let store = queue.supervisor_store();
    let task = RunReads::run(&*store, &run)?.task_id();
    integrator_scope(op, &params.args, &run, task).map_err(BadOperation)?;
    landing_committed(&super::EventStore::run_events(&*store, &run)?, principal)
        .map_err(BadOperation)?;
    let own = |token: &LeaseToken| {
        if *token == lease {
            Ok(())
        } else {
            Err(anyhow::Error::new(NotOwnLease))
        }
    };
    store.restore_request(params.requested_by.clone());
    if let Some(record) = once_record(&params) {
        return dispatch::dispatch_once(store, op, Value::Array(params.args), &record);
    }
    dispatch::dispatch(
        store,
        &own,
        &|_| false,
        backend.now_ms() / 1000,
        op,
        Value::Array(params.args),
    )
}

/// The arguments `args` of `op` as `T`.
fn read<T: DeserializeOwned>(op: &str, args: Value) -> Result<T> {
    serde_json::from_value(args)
        .map_err(|error| BadOperation(format!("the arguments of {op} do not read: {error}")).into())
}

/// An operation's result as the service's answer.
fn reply<T: Wire>(result: Result<T>) -> Result<Value> {
    Ok(serde_json::to_value(result?.into_form())?)
}

/// Why the supervisor `principal`, asking from `process`, is refused under
/// no lease (`issued_under` `None`) the telemetry token of `target`, a
/// target of no run, as `facts` read its start in the issue's transaction
/// ([`crate::domain::queue_service::telemetry_starter_refusal`]): only the
/// target its own command started (ADR-t3102-1 decision 2). `None` under a
/// lease, which the registration it names rules.
fn leaseless_starter_refusal(
    principal: &Principal,
    process: Option<u32>,
    issued_under: Option<&LeaseToken>,
    target: &StepTarget,
    facts: &dyn super::queue_service::IssueFacts,
) -> Result<Option<&'static str>> {
    if issued_under.is_some() {
        return Ok(None);
    }
    Ok(crate::domain::queue_service::telemetry_starter_refusal(
        principal,
        process,
        facts.target_start(target)?.as_ref(),
    ))
}

/// The token operations ([`TOKEN_OPS`]) of `params` on `backend`'s files
/// and `queue`.
fn tokens(
    backend: &dyn ServiceBackend,
    queue: &mut dyn ServiceQueue,
    principal: &Principal,
    registration: &LeaseToken,
    params: &SupervisorParams,
    process: Option<u32>,
) -> Result<Value> {
    use super::queue_service::parent_there;
    use crate::domain::queue_service::{
        IssueHeld, Reissue, TokenIssuer, is_job, job_token_ended, reissue, revoke_refusal,
    };
    use crate::domain::{Capability, Resource};
    let issuer = TokenIssuer::of(principal, process.and_then(|pid| backend.process_of(pid)));
    let op = params.op.as_str();
    let args = Value::Array(params.args.clone());
    let db = backend.db();
    match op {
        "issue_token" => {
            // The issue use case's, with this supervisor as the parent: a
            // worker's and its run's wrapper's under the run's lease, a
            // planner's and its wrapper's while it is open, a job's while
            // the registration lives (ADR-t3102-1 decision 2).
            let (child,): (Principal,) = read(op, args)?;
            super::queue_service::issue_token(backend, queue, principal, &child, process)
        }
        "revoke_token" => {
            let (actor_id,): (String,) = read(op, args)?;
            // By an actor id only: a session's token goes by its principal
            // (`revoke_principal`), and a landing integrator's with its landing.
            if actor_id.contains('@')
                || !crate::domain::queue_service::supervisor_revokes(&actor_id)
            {
                return Err(
                    BadOperation(format!("a supervisor revokes no token of {actor_id}")).into(),
                );
            }
            // Never another parent's job or planner while that parent is
            // there, nor a job's whose parent cannot be judged before its
            // job is taken as ended (its records tell so, or with none, no
            // process that may have asked for it runs), which the issue
            // would then take over ([`revoke_refusal`]): in a write
            // transaction, as the issue's, so neither misses the other.
            let mut role = None;
            let revoked = queue.issuing(&IssueHeld::Nothing, backend.now_ms(), &mut |facts| {
                if let Some(held) = backend.held(&actor_id)? {
                    role = Some(held.principal.role);
                    if let Some(why) =
                        revoke_refusal(&held, &issuer.parent, parent_there(backend, facts), || {
                            facts.job(&held, None)
                        })
                    {
                        return Err(super::queue_service::Replacing(why).into());
                    }
                }
                reply(backend.access().revoke(db, &actor_id))
            });
            let resource = role.map_or(Resource::Queue, Resource::Principal);
            recorded(queue, principal, &resource, revoked)
        }
        "revoke_principal" => {
            let (principal,): (Principal,) = read(op, args)?;
            let Some(key) = crate::domain::queue_service::swept_token_key(&principal) else {
                return Err(BadOperation(format!(
                    "a supervisor revokes no token of {}",
                    principal.actor_id
                ))
                .into());
            };
            // Only the token its file holds now, that very principal's, an
            // inbox's only once its session is known to have ended, a job's
            // once the process that asked for it ended (one whose parent
            // cannot be judged, once its job is taken as ended), and never
            // another parent's that is there: never a live session's or
            // job's on request. In a write transaction, as the issue's, so a
            // token issued again meanwhile is not the one revoked.
            queue.issuing(&IssueHeld::Nothing, backend.now_ms(), &mut |facts| {
                let there = parent_there(backend, facts);
                let Some(held) = backend
                    .held(&key)?
                    .filter(|held| held.principal == principal)
                else {
                    return reply(Ok(false));
                };
                // Its job's state read once, which both judgements take.
                let state = std::cell::OnceCell::new();
                let job = || state.get_or_init(|| facts.job(&held, None)).clone();
                let ended = principal
                    .session
                    .as_ref()
                    .is_none_or(|session| backend.session_ended(session))
                    && (!is_job(principal.role)
                        || job_token_ended(held.issued_by.as_ref(), there, job))
                    && revoke_refusal(&held, &issuer.parent, there, job).is_none();
                if !ended {
                    return reply(Ok(false));
                }
                reply(backend.access().revoke(db, &key))
            })
        }
        "token_principals" => reply(backend.principals()),
        "issue_telemetry" => {
            let (target, task, issued_under): (StepTarget, Option<TaskId>, Option<LeaseToken>) =
                read(op, args)?;
            // Under the run's lease and the registration live for a run's
            // target, the registration live for another's, each checked in
            // the issue's transaction (ADR-t3102-1 decision 2); a run's is
            // never issued under no lease, another's under none only to the
            // command of this supervisor that started it.
            let telemetry = Resource::Principal(ActorRole::ExecutorTelemetry);
            let held = match (&issued_under, target.run_id()) {
                (Some(lease), Some(run)) => {
                    own_lease(
                        queue,
                        principal,
                        registration,
                        lease,
                        Capability::TokenIssue,
                        &telemetry,
                    )?;
                    crate::domain::queue_service::IssueHeld::Lease {
                        run,
                        lease: lease.clone(),
                    }
                }
                (Some(lease), None) => {
                    own_lease(
                        queue,
                        principal,
                        registration,
                        lease,
                        Capability::TokenIssue,
                        &telemetry,
                    )?;
                    crate::domain::queue_service::IssueHeld::Registered(lease.clone())
                }
                (None, Some(_)) => {
                    return Err(super::queue_service::refuse_recorded(
                        queue,
                        &principal.actor(),
                        Capability::TokenIssue,
                        &telemetry,
                        crate::domain::authorization::DenyReason::NotHeld,
                        "a run's executor's telemetry token is issued under the run's lease",
                    ));
                }
                (None, None) => {
                    crate::domain::queue_service::IssueHeld::Registered(registration.clone())
                }
            };
            let child = Principal::telemetry(&target, task);
            let issued = queue.issuing(&held, backend.now_ms(), &mut |facts| {
                // Under no lease, only the target this supervisor's command
                // started, as the queue records its start in the issue's
                // transaction.
                if let Some(why) = leaseless_starter_refusal(
                    principal,
                    process,
                    issued_under.as_ref(),
                    &target,
                    facts,
                )? {
                    return Err(super::queue_service::NotStarter(why).into());
                }
                // Not over a target's another live supervisor asked for, as
                // found in the issue's transaction.
                let held = backend
                    .held(&child.credential_key())?
                    .map(|held| held.issued_by);
                if let Reissue::Refused(why) = reissue(
                    &child,
                    &issuer,
                    held.as_ref(),
                    parent_there(backend, facts),
                    // Of no job: never asked.
                    || crate::domain::queue_service::JobState::MayRun,
                ) {
                    return Err(super::queue_service::Replacing(why).into());
                }
                reply(backend.issue_telemetry_as(&target, task, issued_under.as_ref(), &issuer))
            });
            match issued {
                Err(error) => match error.downcast_ref::<super::queue_service::NotHeld>() {
                    Some(not_held) => {
                        let why = not_held.0.clone();
                        Err(super::queue_service::refuse_recorded(
                            queue,
                            &principal.actor(),
                            Capability::TokenIssue,
                            &telemetry,
                            crate::domain::authorization::DenyReason::NotHeld,
                            &why,
                        ))
                    }
                    None => match error
                        .downcast_ref::<super::queue_service::Replacing>()
                        .map(|super::queue_service::Replacing(why)| *why)
                        .or_else(|| {
                            error
                                .downcast_ref::<super::queue_service::NotStarter>()
                                .map(|super::queue_service::NotStarter(why)| *why)
                        }) {
                        Some(why) => Err(super::queue_service::refuse_recorded(
                            queue,
                            &principal.actor(),
                            Capability::TokenIssue,
                            &telemetry,
                            crate::domain::authorization::DenyReason::Resource,
                            why,
                        )),
                        None => Err(error),
                    },
                },
                issued => issued,
            }
        }
        "revoke_telemetry" => {
            let (target, issued_under): (StepTarget, Option<LeaseToken>) = read(op, args)?;
            // A run's as its lease rules them (a lease lost included); a
            // target of no run's never another live parent's, whatever
            // lease it names ([`revoke_refusal`]).
            let telemetry = Resource::Principal(ActorRole::ExecutorTelemetry);
            let key = Principal::telemetry(&target, None).credential_key();
            let revoked = queue.issuing(&IssueHeld::Nothing, backend.now_ms(), &mut |facts| {
                if let Some(held) = backend.held(&key)?
                    && let Some(why) =
                        revoke_refusal(&held, &issuer.parent, parent_there(backend, facts), || {
                            // Of no job: never asked.
                            crate::domain::queue_service::JobState::MayRun
                        })
                {
                    return Err(super::queue_service::Replacing(why).into());
                }
                reply(
                    backend
                        .telemetry()
                        .revoke_telemetry(db, &target, issued_under.as_ref()),
                )
            });
            recorded(queue, principal, &telemetry, revoked)
        }
        "held_telemetry" => reply(backend.telemetry().held_telemetry(db)),
        "target_facts" => {
            let (target,): (StepTarget,) = read(op, args)?;
            reply(queue.telemetry_target(&target))
        }
        "register_telemetry_session" => {
            let (target, session_id): (StepTarget, String) = read(op, args)?;
            reply(queue.register_session(&target, &session_id, backend.now_ms()))
        }
        _ => unreachable!("a token operation"),
    }
}

/// `result` of `principal`'s token operation on `resource`, a refusal of
/// [`super::queue_service::Replacing`] recorded as `authorization_denied`.
fn recorded(
    queue: &mut dyn ServiceQueue,
    principal: &Principal,
    resource: &crate::domain::Resource,
    result: Result<Value>,
) -> Result<Value> {
    let Err(error) = result else {
        return result;
    };
    match error.downcast_ref::<super::queue_service::Replacing>() {
        Some(super::queue_service::Replacing(why)) => Err(super::queue_service::refuse_recorded(
            queue,
            &principal.actor(),
            crate::domain::Capability::TokenIssue,
            resource,
            crate::domain::authorization::DenyReason::Resource,
            why,
        )),
        None => Err(error),
    }
}

/// Refuse the token operation of `principal`, whose registration is
/// `registration`, that names `lease` when that is another's: what it asks
/// for would be issued under a lease it does not hold (ADR-t3102-1
/// decision 2), `authorization_denied` of `capability` on `resource`,
/// recorded.
fn own_lease(
    queue: &mut dyn ServiceQueue,
    principal: &Principal,
    registration: &LeaseToken,
    lease: &LeaseToken,
    capability: crate::domain::Capability,
    resource: &crate::domain::Resource,
) -> Result<()> {
    if lease == registration {
        return Ok(());
    }
    Err(super::queue_service::refuse_recorded(
        queue,
        &principal.actor(),
        capability,
        resource,
        crate::domain::authorization::DenyReason::NotHeld,
        &NotOwnLease.to_string(),
    ))
}

/// The tokens of the workers and of the executors' telemetry, issued and
/// revoked by the service at the supervisor's request (ADR-t3102-1
/// decision 2): where each token's file is is read here, as the files
/// tell it, and nothing of the DB is.
pub struct ServiceTokens {
    pub queues: SupervisorQueues,
    /// Where the token files of the principals are found
    /// ([`crate::application::queue_service::ServiceAccess::credential`]
    /// and [`crate::application::queue_service::TelemetryTokens::telemetry_credential`]),
    /// and the socket: files, read where they are.
    pub files: Arc<dyn super::queue_service::ServiceAccess>,
    pub telemetry_files: Arc<dyn super::queue_service::TelemetryTokens>,
}

impl ServiceTokens {
    fn queue(&self) -> SupervisorQueue {
        self.queues.queue()
    }
}

/// Revoke through `queue` the token of every principal `ended` says ended,
/// by its key: the keys revoked. One token the service does not revoke
/// leaves the rest to revoke, as the files' own sweep does; a service out
/// of reach ends the sweep.
fn sweep_tokens(
    queue: &SupervisorQueue,
    ended: &mut dyn FnMut(&Principal) -> bool,
) -> Result<Vec<String>> {
    let principals: Vec<Principal> = queue.remote("token_principals", vec![])?;
    let mut revoked = Vec::new();
    // By the principal, whose token's file the service finds by its key:
    // an inbox's is its session's, not its actor id's.
    for principal in principals.iter().filter(|principal| ended(principal)) {
        let key = principal.credential_key();
        match queue.remote::<bool>("revoke_principal", vec![to_value(principal)?]) {
            Ok(true) => revoked.push(key),
            Ok(false) => {}
            Err(error) if unreachable(&error) => return Err(error),
            Err(error) => {
                tracing::warn!("the token of {key} could not be revoked: {error:#}");
            }
        }
    }
    Ok(revoked)
}

impl super::queue_service::ServiceAccess for ServiceTokens {
    fn socket(&self, db: &std::path::Path) -> std::path::PathBuf {
        self.files.socket(db)
    }
    fn issue(
        &self,
        _db: &std::path::Path,
        principal: &Principal,
    ) -> Result<super::queue_service::Issued> {
        self.queue()
            .remote("issue_token", vec![to_value(principal)?])
    }
    fn credential(&self, db: &std::path::Path, actor_id: &str) -> Option<std::path::PathBuf> {
        self.files.credential(db, actor_id)
    }
    fn revoke(&self, _db: &std::path::Path, actor_id: &str) -> Result<bool> {
        self.queue()
            .remote("revoke_token", vec![to_value(actor_id)?])
    }
    fn sweep(
        &self,
        _db: &std::path::Path,
        ended: &mut dyn FnMut(&Principal) -> bool,
    ) -> Result<Vec<String>> {
        sweep_tokens(&self.queue(), ended)
    }
    fn revoke_on_exit(
        &self,
        _db: &std::path::Path,
        actor_id: &str,
        child: Box<dyn super::Spawned>,
    ) -> Box<dyn super::Spawned> {
        Box::new(RevokeOnExit {
            child,
            queue: self.queue(),
            actor_id: Some(actor_id.to_owned()),
        })
    }
}

impl super::queue_service::TelemetryTokens for ServiceTokens {
    fn issue_telemetry(
        &self,
        _db: &std::path::Path,
        target: &StepTarget,
        task: Option<TaskId>,
        issued_under: Option<&LeaseToken>,
    ) -> Result<std::path::PathBuf> {
        self.queue().remote(
            "issue_telemetry",
            vec![to_value(target)?, to_value(task)?, to_value(issued_under)?],
        )
    }
    fn revoke_telemetry(
        &self,
        _db: &std::path::Path,
        target: &StepTarget,
        issued_under: Option<&LeaseToken>,
    ) -> Result<bool> {
        self.queue().remote(
            "revoke_telemetry",
            vec![to_value(target)?, to_value(issued_under)?],
        )
    }
    fn telemetry_credential(
        &self,
        db: &std::path::Path,
        target: &StepTarget,
    ) -> Option<std::path::PathBuf> {
        self.telemetry_files.telemetry_credential(db, target)
    }
    fn kept_steps(
        &self,
        db: &std::path::Path,
    ) -> Result<Vec<crate::domain::telemetry::KeptStepsFile>> {
        self.telemetry_files.kept_steps(db)
    }
    fn forget_steps(&self, db: &std::path::Path, file: &std::path::Path) -> Result<()> {
        self.telemetry_files.forget_steps(db, file)
    }
    fn held_telemetry(
        &self,
        _db: &std::path::Path,
    ) -> Result<Vec<crate::domain::telemetry::HeldTelemetry>> {
        self.queue().remote("held_telemetry", vec![])
    }
    fn target_facts(
        &self,
        _db: &std::path::Path,
        target: &StepTarget,
    ) -> Result<crate::domain::telemetry::TargetFacts> {
        self.queue().remote("target_facts", vec![to_value(target)?])
    }
    fn register_telemetry_session(
        &self,
        _db: &std::path::Path,
        target: &StepTarget,
        session_id: &str,
    ) -> Result<()> {
        self.queue().remote(
            "register_telemetry_session",
            vec![to_value(target)?, to_value(session_id)?],
        )
    }
}

/// A job's process whose token is revoked through the service once it
/// ended ([`super::queue_service::ServiceAccess::revoke_on_exit`]).
struct RevokeOnExit {
    child: Box<dyn super::Spawned>,
    queue: SupervisorQueue,
    actor_id: Option<String>,
}

impl RevokeOnExit {
    fn revoke(&mut self) {
        if let Some(actor_id) = self.actor_id.take()
            && let Err(error) = to_value(&actor_id)
                .and_then(|id| self.queue.remote::<bool>("revoke_token", vec![id]))
        {
            tracing::warn!("the token of {actor_id} could not be revoked at its end: {error:#}");
        }
    }
}

impl super::Spawned for RevokeOnExit {
    fn id(&self) -> u32 {
        self.child.id()
    }
    fn try_wait(&mut self) -> Result<Option<super::Exit>> {
        let exit = self.child.try_wait()?;
        if exit.is_some() {
            self.revoke();
        }
        Ok(exit)
    }
    fn kill(&mut self) -> Result<()> {
        self.child.kill()
    }
    fn wait(&mut self) -> Result<super::Exit> {
        let exit = self.child.wait();
        self.revoke();
        exit
    }
    fn kill_group(&mut self) -> Result<()> {
        self.child.kill_group()
    }
}

impl Drop for RevokeOnExit {
    fn drop(&mut self) {
        self.revoke();
    }
}

#[cfg(test)]
mod tests;
