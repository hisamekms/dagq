//! The supervisor's queue through the queue service ([Queue service],
//! ADR-t3102-3 decisions 3 to 5, goal 182's stage (5)): the claim, the
//! leases, the runs' transitions with their events, the registration, the
//! reads the supervisor decides on and the tokens of the workers and the
//! executors' telemetry are each one use case of the service, done in one
//! transaction on its side as the supervisor's principal (ADR-t3102-1
//! decisions 2 and 3); nothing of one is split across round trips.
//!
//! [`SupervisorQueue`] is the [`Queue`] the supervisor's loop and its
//! threads work on: the operations of [`dispatch`] go to the service by
//! their names ([`SupervisorParams::op`]), and every other operation of the
//! queue (the landing's and the jobs' verdicts, which the next step moves,
//! the planning, the observation and the host's records) still goes to a
//! connection of its own to the DB, opened only when one of those is
//! called. On the service's side ([`answer`]) each operation is the
//! queue's own, so its lease rule and its transaction are the queue's, and
//! every lease token it names has to be the principal's registration: a
//! supervisor writes under no other supervisor's lease.
//!
//! The service out of reach is [`ServiceUnreachable`]: the supervisor
//! never opens the DB for an operation the service answers, and its loop
//! waits for the service instead ([`unreachable`]).
//!
//! [Queue service]: ../../docs/design/queue-service.md

mod dispatch;
mod forward;
mod wire;

pub use wire::Wire;

use std::cell::RefCell;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::queue_service::{ServiceBackend, ServiceQueue};
use super::{EventStore, RunReads, RunTransitions, SupervisorRegistry, TaskStore};
use super::{Queue, QueueBusy, QueueOpener, RunCoordination, RunNotFound, RunRecovery};
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
    /// Become the supervisor of `registration`, process `pid`: the token
    /// of its principal ([`Principal::supervisor`]) is issued, replacing
    /// the one held before. Until then every call is refused.
    fn sign_in(&self, registration: &LeaseToken, pid: u32) -> Result<()>;
    /// Give the principal's token back, at the supervisor's end.
    fn sign_out(&self);
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
    /// send to the service, done or not, or made on the queue's own
    /// connection.
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
    })
}

/// Opens a [`SupervisorQueue`] for each connection the supervisor needs,
/// all through one caller; `local` opens the DB for the operations the
/// service does not answer yet.
pub struct SupervisorQueues {
    pub service: Arc<dyn ServiceCaller>,
    pub local: Arc<dyn QueueOpener>,
    /// The supervisor principal's actor, which its events record unless
    /// one is written as another ([`EventStore::act_as`]).
    pub actor: ActorContext,
}

impl SupervisorQueues {
    fn queue(&self) -> SupervisorQueue {
        SupervisorQueue {
            service: self.service.clone(),
            opener: self.local.clone(),
            actor: self.actor.clone(),
            local: RefCell::new(None),
            acting: RefCell::new(None),
            requested_by: RefCell::new(None),
        }
    }
}

impl QueueOpener for SupervisorQueues {
    fn open(&self) -> Result<Box<dyn Queue + Send>> {
        Ok(Box::new(self.queue()))
    }
}

/// The supervisor's queue: the operations of [`dispatch`] through the
/// service, the rest on a connection of its own opened when first needed.
pub struct SupervisorQueue {
    service: Arc<dyn ServiceCaller>,
    opener: Arc<dyn QueueOpener>,
    actor: ActorContext,
    local: RefCell<Option<Box<dyn Queue + Send>>>,
    /// The actor the events are written as when not the principal's own
    /// ([`EventStore::act_as`]).
    acting: RefCell<Option<ActorContext>>,
    /// The `requested_by` of the events written ([`EventStore::request_as`]).
    requested_by: RefCell<Option<String>>,
}

impl SupervisorQueue {
    /// The operation `op` of the service with `args`, read back as `T`.
    fn remote<T: Wire>(&self, op: &str, args: Vec<Value>) -> Result<T> {
        let params = SupervisorParams {
            op: op.to_owned(),
            args,
            acting: self
                .acting
                .borrow()
                .as_ref()
                .filter(|actor| **actor != self.actor)
                .map(Principal::of),
            requested_by: self.requested_by.borrow().clone(),
        };
        let read = READ_OPS.contains(&op);
        if !read {
            WRITES_TRIED.with(|tried| tried.set(tried.get() + 1));
        }
        let value = match self
            .service
            .call(UseCase::Supervisor, serde_json::to_value(params)?)
        {
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

    /// The actor the principal's events record.
    fn service_actor(&self) -> ActorContext {
        self.actor.clone()
    }

    /// The connection for the operations the service does not answer yet,
    /// opened at the first of them with the actor and the requester set
    /// since. Each use counts as a write tried ([`writes_tried`]): one made
    /// there is done, whatever the service answers next.
    fn local(&self) -> Result<std::cell::RefMut<'_, Box<dyn Queue + Send>>> {
        WRITES_TRIED.with(|tried| tried.set(tried.get() + 1));
        let mut local = self.local.borrow_mut();
        if local.is_none() {
            let queue = self.opener.open()?;
            if let Some(actor) = self.acting.borrow().clone() {
                queue.act_as(actor);
            }
            queue.restore_request(self.requested_by.borrow().clone());
            *local = Some(queue);
        }
        Ok(std::cell::RefMut::map(local, |local| {
            local.as_mut().expect("opened above")
        }))
    }
}

fn to_value(value: impl serde::Serialize) -> Result<Value> {
    Ok(serde_json::to_value(value)?)
}

/// The queue the service runs a supervisor's operations on: the queue's
/// operations the service answers ([`dispatch`]).
pub trait SupervisorStore:
    RunTransitions
    + RunRecovery
    + RunCoordination
    + SupervisorRegistry
    + RunReads
    + EventStore
    + TaskStore
{
}

impl<
    T: RunTransitions
        + RunRecovery
        + RunCoordination
        + SupervisorRegistry
        + RunReads
        + EventStore
        + TaskStore
        + ?Sized,
> SupervisorStore for T
{
}

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

/// Answer the supervisor `principal`'s request `params` on `backend`, as
/// the service's `supervisor` use case: the policy's `scheduler.supervise`
/// is checked (and a refusal recorded) first, then the operation runs on a
/// queue written as the principal's actor, or as the actor the request
/// names when it is one of the control side's (the landing's integrator),
/// with the request's `requested_by`.
pub fn answer(
    backend: &dyn ServiceBackend,
    principal: &Principal,
    params: &Value,
) -> Result<Value> {
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
    let own = |token: &LeaseToken| {
        if *token == registration {
            Ok(())
        } else {
            Err(anyhow::Error::new(NotOwnLease))
        }
    };
    if TOKEN_OPS.contains(&params.op.as_str()) {
        return tokens(backend, &mut *queue, &registration, &params);
    }
    if let Some(acting) = &params.acting {
        let as_actor = acting.actor();
        if !matches!(
            as_actor.role(),
            ActorRole::Supervisor | ActorRole::Integrator
        ) {
            return Err(BadOperation(format!(
                "a supervisor writes as itself or the integrator, not as {}",
                as_actor.role().as_str()
            ))
            .into());
        }
        queue.supervisor_store().act_as(as_actor);
    }
    let store = queue.supervisor_store();
    store.restore_request(params.requested_by.clone());
    dispatch::dispatch(
        store,
        &own,
        backend.now_ms() / 1000,
        &params.op,
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

/// The token operations ([`TOKEN_OPS`]) of `params` on `backend`'s files
/// and `queue`.
fn tokens(
    backend: &dyn ServiceBackend,
    queue: &mut dyn ServiceQueue,
    registration: &LeaseToken,
    params: &SupervisorParams,
) -> Result<Value> {
    let own = |token: &LeaseToken| {
        if token == registration {
            Ok(())
        } else {
            Err(anyhow::Error::new(NotOwnLease))
        }
    };
    let op = params.op.as_str();
    let args = Value::Array(params.args.clone());
    let db = backend.db();
    match op {
        "issue_token" => {
            let (principal,): (Principal,) = read(op, args)?;
            principal.check()?;
            // An AI actor's and a session wrapper's only: a supervisor's
            // token is its own sign-in, and an executor's telemetry token
            // is `issue_telemetry`'s.
            if principal.registration.is_some()
                || (principal.role.trust() != crate::domain::TrustLevel::UntrustedAgent
                    && principal.role != ActorRole::Wrapper)
            {
                return Err(BadOperation(format!(
                    "a supervisor issues no token of role {} here",
                    principal.role.as_str()
                ))
                .into());
            }
            // A worker's token, and its run's wrapper's, is the run's,
            // issued only under its lease.
            match (&principal.run_id, principal.role) {
                (Some(run), ActorRole::Worker | ActorRole::Wrapper) => {
                    queue.under_lease(run, registration, &mut || {
                        reply(backend.access().issue(db, &principal))
                    })
                }
                _ => reply(backend.access().issue(db, &principal)),
            }
        }
        "revoke_token" => {
            let (actor_id,): (String,) = read(op, args)?;
            // Never a supervisor's own sign-in, which only it gives back.
            if actor_id.contains('@') || actor_id.starts_with("supervisor:") {
                return Err(
                    BadOperation(format!("a supervisor revokes no token of {actor_id}")).into(),
                );
            }
            reply(backend.access().revoke(db, &actor_id))
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
            // Only a token held for that very principal, and an inbox's only
            // once its session ended: never a live session's on request.
            let held = backend.principals()?.contains(&principal);
            let ended = principal
                .session
                .as_ref()
                .is_none_or(|session| !backend.session_alive(session));
            if !(held && ended) {
                return reply(Ok(false));
            }
            reply(backend.access().revoke(db, &key))
        }
        "token_principals" => reply(backend.principals()),
        "issue_telemetry" => {
            let (target, task, issued_under): (StepTarget, Option<TaskId>, Option<LeaseToken>) =
                read(op, args)?;
            match (&issued_under, target.run_id()) {
                (Some(lease), Some(run)) => {
                    own(lease)?;
                    queue.under_lease(&run, lease, &mut || {
                        reply(
                            backend
                                .telemetry()
                                .issue_telemetry(db, &target, task, Some(lease)),
                        )
                    })
                }
                (Some(lease), None) => {
                    own(lease)?;
                    reply(
                        backend
                            .telemetry()
                            .issue_telemetry(db, &target, task, Some(lease)),
                    )
                }
                (None, _) => reply(backend.telemetry().issue_telemetry(db, &target, task, None)),
            }
        }
        "revoke_telemetry" => {
            let (target, issued_under): (StepTarget, Option<LeaseToken>) = read(op, args)?;
            reply(
                backend
                    .telemetry()
                    .revoke_telemetry(db, &target, issued_under.as_ref()),
            )
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

impl super::queue_service::ServiceAccess for ServiceTokens {
    fn socket(&self, db: &std::path::Path) -> std::path::PathBuf {
        self.files.socket(db)
    }
    fn issue(&self, _db: &std::path::Path, principal: &Principal) -> Result<std::path::PathBuf> {
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
        let queue = self.queue();
        let principals: Vec<Principal> = queue.remote("token_principals", vec![])?;
        let mut revoked = Vec::new();
        // By the principal, whose token's file the service finds by its
        // key: an inbox's is its session's, not its actor id's.
        for principal in principals.iter().filter(|principal| ended(principal)) {
            let gone: bool = queue.remote("revoke_principal", vec![to_value(principal)?])?;
            if gone {
                revoked.push(principal.actor_id.clone());
            }
        }
        Ok(revoked)
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
