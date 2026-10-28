//! The queue service's use cases ([Queue service], ADR-t1233-1 decisions
//! 2 to 4): one request is one use case, authenticated by the token the
//! control side issued for its caller (ADR-t1233-4 decision 4), authorized
//! for that principal by the [`StaticPolicy`] on the service's side
//! (ADR-t1233-5 decision 2), and done in one transaction of the service's
//! own; nothing of it is split across round trips with the caller. A
//! refusal of the policy is recorded as `authorization_denied` as the
//! command line records it (the use cases run through
//! [`Dialogue`] and [`Gate`]), and a request that names no principal as
//! `queue_service_unauthenticated`.
//!
//! The use cases of goal 82's stage (2): `hello`, `ask` (with the
//! observer's `blocked` ask on a finding), `show` (a task with its runs,
//! the whole queue being readable to every role, ADR-t1233-5 decision 3),
//! `note`, `proposal_list` and `proposal_show`, and the findings'
//! `finding_record` (which updates the open finding of the same kind,
//! target and subject), `finding_resolve` and `finding_dismiss`
//! (ADR-t1222-1 decisions 1, 2 and 4), and the reads of the whole queue a
//! read role's job or a worker runs (`events`, `timeline`, `stats`, `kpi`,
//! `search`, `goal_show`, ...: [`QueueRead`], answered as the command line
//! answers them), the planning commands (`add`, `submit`, `goal add`,
//! `request add`, ...: [`QueueWrite`], run as the command line runs them)
//! and `mark`. The dialogue and the operations a person or the inbox runs
//! on the host (`answer`, `hold`, `rebind`, `run log`, `report`, ...) are
//! use cases too ([`QueueCommand`], ADR-t3102-2 decision 6 (4)), run as
//! the command line runs them on the DB. The supervisor's claims, leases,
//! runs and their events and the tokens it issues are the `supervisor` use
//! case ([`super::supervisor_service`]).
//!
//! [Queue service]: ../../docs/design/queue-service.md

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use super::commands::dialogue::{Dialogue, DialogueStore, MarkChange};
use super::commands::{DenialLog, Gate};
use super::queue_commands::{BadCommand, QueueCommand};
use super::queue_reads::{BadRead, QueueRead};
use super::queue_writes::{BadWrite, QueueWrite};
use crate::domain::EventKind;
use crate::domain::queue_service::{
    API_VERSION, MIN_API_VERSION, Principal, ServiceErrorCode, ServiceRequest, ServiceResponse,
    ServiceState, UseCase, answers, run_holds_token,
};
use crate::domain::session_step::{StepError, StepTarget};
use crate::domain::telemetry::{ReportRecorded, TargetFacts, TelemetryReport};
use crate::domain::{
    ActorContext, ActorRole, Answerer, Ask, AskId, AskKind, AskReason, AuthorizationError,
    Capability, EventId, Finding, FindingId, FindingOutcome, FindingStatus, FindingTarget, GoalId,
    NewAsk, NewFinding, NewNote, NoteTarget, PlannerId, ProposalId, RequestId, Resource, RunEvent,
    RunId, RunStatus, SessionRole, StaticPolicy, TaskId,
};

/// What the issue's transaction reads of the queue for a token held
/// already ([`ServiceQueue::issuing`], ADR-t3102-1 decision 2).
pub trait IssueFacts {
    /// Whether the supervisor's registration `token` is kept, which tells
    /// whether a supervisor kept as a token's parent is still there
    /// ([`crate::domain::queue_service::TokenIssuer::there`]).
    fn registered(&self, token: &crate::domain::LeaseToken) -> bool;
    /// What the queue's records of the job `held` is of tell
    /// ([`crate::domain::queue_service::job_records_ended`]), the record of
    /// the start `starting` asks for left out while it runs, and with none,
    /// the processes that may have asked for that token, by when it was
    /// issued and the parent kept with it, and may still run
    /// ([`crate::domain::queue_service::may_have_asked`]): `MayRun` when
    /// the records cannot be read.
    fn job(
        &self,
        held: &crate::domain::queue_service::HeldToken,
        starting: Option<&crate::domain::queue_service::TokenIssuer>,
    ) -> crate::domain::queue_service::JobState;
    /// Who started `target`, a target of no run, as the queue records its
    /// start ([`crate::domain::queue_service::TargetStart`]); `None` for a
    /// target with no such record.
    fn target_start(
        &self,
        target: &crate::domain::session_step::StepTarget,
    ) -> Result<Option<crate::domain::queue_service::TargetStart>>;
}

/// The queue as one use case of the service reads and changes it, written
/// as the principal's actor.
pub trait ServiceQueue:
    DialogueStore
    + super::agent_eval::EvalStore
    + super::commands::replans::ReplanStore
    + super::agent_cases::CaseSetStore
{
    /// Task `id` as `dagq show` prints it: the whole detail with `full`,
    /// else its view with the last `events` events.
    fn show(&mut self, id: TaskId, full: bool, events: usize) -> Result<Value>;
    /// The status of run `id`; `None` for a run the queue does not know.
    fn run_status(&self, id: &RunId) -> Result<Option<RunStatus>>;
    /// The Git common directory the queue is bound to, `None` for one
    /// bound to none (`broker_binding`).
    fn repository_binding(&self) -> Result<Option<String>>;
    /// Record the queue broker's queue event `kind` with `payload`
    /// (`broker_record`), written as the principal's actor.
    fn record_broker_event(&self, kind: EventKind, payload: Value) -> Result<EventId>;
    /// The proposals as `dagq proposal list` prints them: the submitted
    /// and revising ones, or with `all` every one.
    fn proposals(&self, all: bool) -> Result<Value>;
    /// Proposal `id` as `dagq proposal show` prints it.
    fn show_proposal(&self, id: ProposalId) -> Result<Value>;
    /// A read of the whole queue as its command prints it.
    fn read(&mut self, read: &QueueRead) -> Result<Value>;
    /// One of the host's dialogue and operation commands as its command
    /// runs it on the DB, authorized already as far as the command line
    /// authorizes it before it runs ([`QueueCommand::authorization`]); the
    /// rest the application authorizes as it runs it.
    fn command(&mut self, command: &QueueCommand) -> Result<Value>;
    /// The target the control side registered `session_id` to
    /// ([`crate::application::SessionOwners`]); `None` for one never
    /// registered.
    fn session_owner(&self, session_id: &str) -> Result<Option<StepTarget>>;
    /// Record `report`'s steps and resource samples as received now
    /// ([`crate::application::SessionStepStore::record_steps`], then
    /// [`crate::application::SessionResourceStore::record_resources`]): a
    /// failure between leaves the steps, which the executor's resend adds
    /// no second time.
    fn record_report(&mut self, report: &TelemetryReport) -> Result<ReportRecorded>;
    /// What the queue holds of `target`, a target of no run, for whether
    /// its telemetry token still holds
    /// ([`crate::domain::telemetry::target_token_due`]).
    fn telemetry_target(&self, target: &StepTarget) -> Result<TargetFacts>;
    /// Register `session_id` as `target`'s
    /// ([`crate::application::SessionOwners::register_session`]), at a
    /// supervisor's request.
    fn register_session(&mut self, target: &StepTarget, session_id: &str, at_ms: i64)
    -> Result<()>;
    /// The queue's operations a supervisor's request runs
    /// ([`super::supervisor_service::answer`]).
    fn supervisor_store(&mut self) -> &mut dyn super::supervisor_service::SupervisorStore;
    /// Run `then`, the issue of a token, in one write transaction that
    /// finds `held` held first at `now_ms` (ADR-t3102-1 decision 2), and
    /// refuse with [`NotHeld`] (running nothing) when it is not: a lease
    /// lost, a registration whose heartbeat expired, a planner closed or a
    /// run ended. `then` is given what that transaction reads of a token
    /// held already ([`IssueFacts`]).
    fn issuing(
        &mut self,
        held: &crate::domain::queue_service::IssueHeld,
        now_ms: i64,
        then: &mut dyn FnMut(&dyn IssueFacts) -> Result<Value>,
    ) -> Result<Value>;
    /// Take the integration slot of `run` for the supervisor of `token`
    /// ([`super::RunTransitions::begin_integration`], or
    /// [`super::RunTransitions::begin_integration_over_handoff`] when
    /// `over_handoff`) and, in the same
    /// transaction once the slot and its lease are taken, run `issue` with
    /// the landing's `integration_started` event (the landing integrator's
    /// token, ADR-t3102-3 decision 3, ADR-t3102-1 decision 2): nothing is
    /// committed unless both are done. The run's lease another holds, or
    /// a supervisor's registration not live, is [`NotHeld`], found in that
    /// transaction ([`SlotHolder`]). The run begun and what `issue`
    /// answered.
    fn begin_landing(
        &mut self,
        run: &RunId,
        token: &crate::domain::LeaseToken,
        main: &crate::domain::CommitSha,
        over_handoff: bool,
        holder: SlotHolder,
        issue: &mut dyn FnMut(crate::domain::EventId) -> Result<Value>,
    ) -> Result<(crate::domain::TaskRun, Value)>;
    /// Do a session wrapper's `op`, authorized already
    /// ([`super::session::apply`]).
    fn session(&mut self, op: &super::session::SessionOp) -> Result<Value>;
    /// One look of `dagq watch` past `after` (`None`: the newest event
    /// now) for `role` ([`super::watch::look`]).
    fn watch_look(
        &self,
        after: Option<EventId>,
        role: Option<SessionRole>,
    ) -> Result<super::watch::WatchLook>;
    /// Run the planning command `write` as `actor`, as the command line
    /// runs it ([`super::queue_writes::answer`], the queue held to the
    /// repository's rules first), with the proposal of a `submit` owned by
    /// `actor`'s planner when it is one.
    fn write(&mut self, write: &QueueWrite, actor: &ActorContext) -> Result<Value>;
}

/// What the service runs its use cases on.
pub trait ServiceBackend: Send + Sync {
    /// The principal `token` was issued for; `None` for a token the
    /// control side did not issue or has revoked.
    fn principal(&self, token: &str) -> Result<Option<Principal>>;
    /// The queue, written as `actor`.
    fn open(&self, actor: &ActorContext) -> Result<Box<dyn ServiceQueue>>;
    /// The queue for a read as `actor`, as the command line reads it
    /// (ADR-0045 decision 18): on a read-only connection, or, at an older
    /// schema within the floor, from a copy migrated in memory. Nothing of
    /// the read writes to the queue; a refusal of the policy is recorded
    /// all the same, on a connection of its own that writes.
    fn open_read(&self, actor: &ActorContext) -> Result<Box<dyn ServiceQueue>> {
        self.open(actor)
    }
    /// The queue for one look of a watch as `actor`: the live queue on a
    /// read-only connection, never a copy, so an older schema is refused
    /// as the command line's `watch` refuses it. A refusal of the policy is
    /// recorded as [`Self::open_read`] records it.
    fn open_watch(&self, actor: &ActorContext) -> Result<Box<dyn ServiceQueue>> {
        self.open_read(actor)
    }
    /// Record a request that named no principal
    /// (`queue_service_unauthenticated`), written as the service itself.
    fn record_unauthenticated(&self, payload: Value) -> Result<()>;
    /// The unix milliseconds now, by the service's clock: what a target's
    /// command is read against, past the time it ends by or not.
    fn now_ms(&self) -> i64;
    /// The queue's DB, which names the queue to [`Self::access`] and
    /// [`Self::telemetry`].
    fn db(&self) -> &std::path::Path;
    /// The tokens' files of the principals the control side starts, which
    /// the service issues and revokes at a supervisor's request.
    fn access(&self) -> &dyn ServiceAccess;
    /// The tokens' files of the executors' telemetry principals, likewise.
    fn telemetry(&self) -> &dyn TelemetryTokens;
    /// Every principal a token is held for.
    fn principals(&self) -> Result<Vec<Principal>>;
    /// Whether `db`, the queue DB a request names, is the one the service
    /// serves: the same file, compared by its path and never opened.
    fn serves(&self, db: &std::path::Path) -> bool;
    /// Whether the process of `session` still runs: its pid lives and the
    /// system prints the same start for it
    /// ([`crate::domain::queue_service::SessionProcess::runs`]).
    fn session_alive(&self, session: &crate::domain::queue_service::SessionProcess) -> bool;
    /// Whether the process of `session` is known to have ended: its pid is
    /// gone or the system prints another start for it, not when its start
    /// cannot be read
    /// ([`crate::domain::queue_service::SessionProcess::ended`]).
    fn session_ended(&self, session: &crate::domain::queue_service::SessionProcess) -> bool;
    /// The process `pid` as a session is read (its start), `None` when
    /// that cannot be read.
    fn process_of(&self, pid: u32) -> Option<crate::domain::queue_service::SessionProcess>;
    /// Whether the process on `pid`, which had started by `by` (unix
    /// seconds), is known to have ended: its pid is gone, or a process that
    /// started after then is on it
    /// ([`crate::domain::queue_service::started_by_ended`]).
    fn started_by_ended(&self, pid: u32, by: i64) -> bool;
    /// Issue `principal`'s token as [`ServiceAccess::issue`] does, keeping
    /// `issuer` with it (ADR-t3102-1 decision 2).
    fn issue_as(
        &self,
        principal: &Principal,
        issuer: &crate::domain::queue_service::TokenIssuer,
    ) -> Result<std::path::PathBuf>;
    /// The token the file of `key` ([`Principal::credential_key`]) holds,
    /// its principal and who asked for it; `None` for no token.
    fn held(&self, key: &str) -> Result<Option<crate::domain::queue_service::HeldToken>>;
    /// Revoke the token of `key` as this service stops, before it exits: a
    /// supervisor's whose registration ends, which a `down --wait` or the
    /// supervisor itself may stop the service after, before its last
    /// requests (ADR-t3102-1 decision 2). A backend that never stops on its
    /// own (a supervisor's in-process one) keeps none, and the supervisor
    /// gives its token back at its end instead.
    fn revoke_at_stop(&self, _key: String) {}
    /// [`TelemetryTokens::issue_telemetry`], keeping `issuer` with the
    /// token.
    fn issue_telemetry_as(
        &self,
        target: &crate::domain::session_step::StepTarget,
        task: Option<TaskId>,
        issued_under: Option<&crate::domain::LeaseToken>,
        issuer: &crate::domain::queue_service::TokenIssuer,
    ) -> Result<std::path::PathBuf>;
    /// The Git common directory the queue is bound to, `None` for one
    /// bound to none.
    fn binding(&self) -> Result<Option<String>> {
        Ok(None)
    }
    /// Whether process `pid` of the host runs: whether a supervisor that
    /// owns a record (an eval round, a case set) is still there.
    fn alive(&self, pid: u32) -> bool;
}

/// The service's use cases on a backend.
pub struct QueueService<'a> {
    pub backend: &'a dyn ServiceBackend,
    /// The service's build identifier, which `hello` names.
    pub build: &'a str,
    /// The service's process.
    pub pid: u32,
}

/// Who is at the other end of a request's connection, as the service reads
/// it from the socket itself (ADR-t3102-1 decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    /// A process of the service's own OS user on its unix socket.
    Owner,
    /// Anything else, or a peer that could not be read.
    Other,
}

impl QueueService<'_> {
    /// Answer `request` from a peer that is not the user's ([`Peer::Other`]).
    pub fn handle(&self, request: &ServiceRequest) -> ServiceResponse {
        self.handle_from(request, Peer::Other)
    }

    /// Answer `request` from `peer`, whose process is not known.
    pub fn handle_from(&self, request: &ServiceRequest, peer: Peer) -> ServiceResponse {
        self.handle_from_process(request, peer, None)
    }

    /// Answer `request` from `peer`, from the process `process` when the
    /// connection tells it: the one that asked, which a token it asks for
    /// is kept with. An error never leaves without its code, and no
    /// response names the token.
    pub fn handle_from_process(
        &self,
        request: &ServiceRequest,
        peer: Peer,
        process: Option<u32>,
    ) -> ServiceResponse {
        if !answers(request.api_version) {
            return ServiceResponse::failure(
                ServiceErrorCode::ApiVersionMismatch,
                format!(
                    "the queue service answers API versions {MIN_API_VERSION} to {API_VERSION}, \
                     not {}",
                    request.api_version
                ),
            );
        }
        if let Some(db) = &request.queue
            && !self.backend.serves(db)
        {
            return ServiceResponse::failure(
                ServiceErrorCode::WrongQueue,
                format!(
                    "this queue service serves another queue than {}: the command is refused, \
                     and neither queue is opened",
                    db.display()
                ),
            );
        }
        if request.use_case == UseCase::Hello {
            return ServiceResponse::success(json!({
                "service": "dagq-queue-service",
                "build": self.build,
                "pid": self.pid,
                "api_version": API_VERSION,
                "min_api_version": MIN_API_VERSION,
            }));
        }
        let connection = connection(request.use_case, &request.params);
        let (principal, checked) = match self.authenticate(request, peer, connection) {
            Ok(authenticated) => authenticated,
            Err(response) => return response,
        };
        // A repository queue answers only the repository it is bound to,
        // as the command line refused every command of another, and only
        // to a principal it knows.
        if let Some(binding) = &request.binding {
            match self.backend.binding() {
                Ok(Some(bound)) if bound != *binding => {
                    return ServiceResponse::failure(
                        ServiceErrorCode::Failed,
                        format!(
                            "queue is bound to another Git repository: {bound} (this repository \
                             is {binding})"
                        ),
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    return ServiceResponse::failure(
                        ServiceErrorCode::Failed,
                        format!("the queue's repository could not be read: {error:#}"),
                    );
                }
            }
        }
        let actor = principal.actor();
        if request.use_case == UseCase::Control {
            return respond(super::supervisor_service::control::answer(
                self.backend,
                &principal,
                &request.params,
                process,
            ));
        }
        if connection == Connection::Supervisor {
            return respond(super::supervisor_service::answer(
                self.backend,
                &principal,
                &request.params,
                process,
            ));
        }
        if request.use_case == UseCase::IssueToken {
            return respond(issue_token_of(
                self.backend,
                &principal,
                &request.params,
                process,
            ));
        }
        if request.use_case == UseCase::RevokeToken {
            return respond(revoke_token_of(self.backend, &principal, &request.params));
        }
        // The queue the token's run or target was read on, when it was,
        // is the use case's: it is opened once.
        let opened = checked.map_or_else(|| self.open_on(connection, &actor), Ok);
        respond(
            opened
                .and_then(|mut queue| run(&mut *queue, &actor, request.use_case, &request.params)),
        )
    }

    /// The queue on `connection` as `actor`; the supervisor's requests
    /// open it to write.
    fn open_on(
        &self,
        connection: Connection,
        actor: &ActorContext,
    ) -> Result<Box<dyn ServiceQueue>> {
        match connection {
            Connection::Supervisor | Connection::Write => self.backend.open(actor),
            Connection::Read => self.backend.open_read(actor),
            Connection::Watch => self.backend.open_watch(actor),
        }
    }

    /// The principal of the request's token, whose run, when it names
    /// one, has not ended, and whose target of no run, when it has one,
    /// has not either; with no token, the user, when `peer` is the
    /// service's own OS user and the request names no role of its
    /// environment (ADR-t3102-1 decision 1). A refusal is
    /// `unauthenticated`, recorded with the use case and the reason, never
    /// the token: `missing_token` for no token or an empty one from any
    /// other caller, `unknown_token` for one the control side
    /// did not issue or has revoked, `session_ended` for an inbox's whose
    /// session's process has ended, `run_ended` for one whose run has
    /// ended or is not in the queue, `target_ended` for a token whose
    /// target of no run (a telemetry target, a planner) the queue says ended
    /// or whose runner is gone
    /// ([`crate::domain::telemetry::target_token_due`]), revoked yet or
    /// not: a token the process that issued it could not revoke before it
    /// fell is refused all the same. The run or the target is read on the
    /// `connection` the use case runs on, so a read of an older queue
    /// reads it from the copy the read reads, and that queue is returned
    /// for the use case.
    fn authenticate(
        &self,
        request: &ServiceRequest,
        peer: Peer,
        connection: Connection,
    ) -> Result<(Principal, Option<Box<dyn ServiceQueue>>), ServiceResponse> {
        let refuse = |reason: &str, message: &str| {
            let payload = json!({"use_case": request.use_case, "reason": reason});
            if let Err(error) = self.backend.record_unauthenticated(payload) {
                warn!("could not record the unauthenticated request ({reason}): {error:#}");
            }
            ServiceResponse::failure(ServiceErrorCode::Unauthenticated, message)
        };
        let Some(token) = request.token.as_deref().filter(|token| !token.is_empty()) else {
            let named = request.env_role.as_deref().filter(|role| !role.is_empty());
            return match (peer, named) {
                (Peer::Owner, None) if request.token.is_none() => Ok((Principal::user(), None)),
                (_, Some(role)) => {
                    let mut message = format!(
                        "the request carries no token, and its environment names the role \
                         {role}, which is never taken for the user's"
                    );
                    if let Some(next) = crate::domain::queue_service::missing_token_next(role) {
                        message.push_str(": ");
                        message.push_str(next);
                    }
                    Err(refuse("missing_token", &message))
                }
                _ => Err(refuse("missing_token", "the request carries no token")),
            };
        };
        let principal = match self.backend.principal(token) {
            // A record of a shape no issue makes (an inbox of no session)
            // authenticates nothing.
            Ok(Some(principal)) if principal.check().is_err() => {
                return Err(refuse(
                    "unknown_token",
                    "the token is not one the control side issued, or it was revoked",
                ));
            }
            Ok(Some(principal)) => principal,
            Ok(None) => {
                return Err(refuse(
                    "unknown_token",
                    "the token is not one the control side issued, or it was revoked",
                ));
            }
            Err(error) => {
                return Err(ServiceResponse::failure(
                    ServiceErrorCode::Failed,
                    format!("the token could not be read: {error:#}"),
                ));
            }
        };
        if let Some(session) = &principal.session
            && !self.backend.session_alive(session)
        {
            return Err(refuse(
                "session_ended",
                "the token's session has ended: open the inbox again (`dagq inbox` in a \
                 terminal with no DAGQ_ROLE)",
            ));
        }
        let target = principal.target.as_ref().filter(|t| t.run_id().is_none());
        if principal.run_id.is_none() && target.is_none() {
            return Ok((principal, None));
        }
        let read = if principal.run_id.is_some() {
            "run"
        } else {
            "target"
        };
        let unread = |error: &anyhow::Error| {
            ServiceResponse::failure(
                ServiceErrorCode::Failed,
                format!("the token's {read} could not be read: {error:#}"),
            )
        };
        let queue = self
            .open_on(connection, &principal.actor())
            .map_err(|error| unread(&error))?;
        if let Some(run) = &principal.run_id {
            match queue.run_status(run) {
                Ok(Some(status)) if run_holds_token(status) => {}
                Ok(_) => {
                    return Err(refuse(
                        "run_ended",
                        "the token's run has ended or is not in the queue",
                    ));
                }
                Err(error) => return Err(unread(&error)),
            }
        }
        if let Some(target) = target {
            match queue.telemetry_target(target) {
                Ok(facts)
                    if crate::domain::telemetry::target_token_due(
                        target.kind,
                        &facts,
                        self.backend.now_ms(),
                        &|_| true,
                    )
                    .is_none() => {}
                Ok(_) => {
                    return Err(refuse(
                        "target_ended",
                        "the token's target has ended or is not in the queue",
                    ));
                }
                Err(error) => {
                    return Err(ServiceResponse::failure(
                        ServiceErrorCode::Failed,
                        format!("the token's target could not be read: {error:#}"),
                    ));
                }
            }
        }
        Ok((principal, Some(queue)))
    }
}

/// The connection a use case runs on (ADR-0045 decision 18): a read
/// (`show`, the proposals', the evals' and the case sets' and the whole
/// queue's reads) on a read-only one, a watch's look on the live queue
/// read-only, the supervisor's requests apart, and the rest on one that
/// writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Connection {
    Supervisor,
    Write,
    Read,
    Watch,
}

fn connection(use_case: UseCase, params: &Value) -> Connection {
    match use_case {
        UseCase::Supervisor | UseCase::Control => Connection::Supervisor,
        UseCase::WatchPoll => Connection::Watch,
        UseCase::Show
        | UseCase::ProposalList
        | UseCase::ProposalShow
        | UseCase::AgentResults
        | UseCase::AgentResult
        | UseCase::AgentSets
        | UseCase::AgentSet
        | UseCase::BrokerRunStatus
        | UseCase::BrokerBinding => Connection::Read,
        _ if matches!(QueueRead::parse(use_case, params), Ok(Some(_))) => Connection::Read,
        _ if matches!(QueueCommand::parse(use_case, params), Ok(Some(command)) if command.reads_only()) => {
            Connection::Read
        }
        _ => Connection::Write,
    }
}

/// The answer of a use case's `result`: a refusal of the policy as
/// `authorization_denied`, params that do not read as `bad_request`, and
/// any other failure as `failed`, with its
/// [`crate::domain::queue_service::FailureKind`] when the caller
/// tells it apart.
fn respond(result: Result<Value>) -> ServiceResponse {
    match result {
        Ok(value) => ServiceResponse::success(value),
        Err(error) => match error.downcast_ref::<AuthorizationError>() {
            Some(refused) => {
                let mut response = ServiceResponse::denied(refused);
                if let (Some(because), Some(failure)) =
                    (error.downcast_ref::<RefusedBecause>(), &mut response.error)
                {
                    failure.message = format!("{}: {because}", failure.message);
                }
                response
            }
            None if error.downcast_ref::<BadParams>().is_some()
                || error.downcast_ref::<BadRead>().is_some()
                || error.downcast_ref::<BadWrite>().is_some()
                || error.downcast_ref::<BadCommand>().is_some()
                || error
                    .downcast_ref::<super::supervisor_service::BadOperation>()
                    .is_some() =>
            {
                ServiceResponse::failure(ServiceErrorCode::BadRequest, format!("{error:#}"))
            }
            None => {
                let mut response =
                    ServiceResponse::failure(ServiceErrorCode::Failed, format!("{error:#}"));
                if let Some(failure) = &mut response.error {
                    failure.kind = super::supervisor_service::failure_kind(&error);
                }
                response
            }
        },
    }
}

/// Who takes a landing's slot under its lease token
/// ([`ServiceQueue::begin_landing`]).
#[derive(Clone, Copy, Debug)]
pub enum SlotHolder {
    /// The supervisor the token is the registration of, which is to be
    /// live at `now` (unix seconds); the lease and the event name its
    /// process.
    Supervisor { now: i64 },
    /// Process `pid`, the command of a person's or the inbox's
    /// `integrate`, which lands in its own process and is named in the
    /// lease and the event.
    Command { pid: u32 },
}

/// What a token is to be issued under is not held
/// ([`ServiceQueue::issuing`]): why, as the refusal's record keeps it.
#[derive(Debug)]
pub struct NotHeld(pub String);

impl std::fmt::Display for NotHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotHeld {}

/// The executor's telemetry token of a target asked for is not issued:
/// the target was not started by the parent that asks, or holds one
/// already, and why
/// ([`crate::domain::queue_service::telemetry_target_refusal`]), found in
/// the issue's transaction ([`issue_token`], and the supervisor's
/// `issue_telemetry` under no lease, which checks the start only).
#[derive(Debug)]
pub(super) struct TelemetryRefused(pub(super) &'static str);

impl std::fmt::Display for TelemetryRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for TelemetryRefused {}

/// The token the key asked for holds already, which the issue may not
/// replace, and why ([`crate::domain::queue_service::reissue`]),
/// found in the issue's transaction.
#[derive(Debug)]
pub(super) struct Replacing(pub(super) &'static str);

impl std::fmt::Display for Replacing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Replacing {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueTokenParams {
    principal: Principal,
}

/// The issue use case (`issue_token`, ADR-t3102-1 decision 2): the token
/// of `raw`'s principal, asked for by `parent`, the principal of the
/// request ([`issue_token`]).
fn issue_token_of(
    backend: &dyn ServiceBackend,
    parent: &Principal,
    raw: &Value,
    process: Option<u32>,
) -> Result<Value> {
    let p: IssueTokenParams = params(UseCase::IssueToken, raw)?;
    let mut queue = backend.open(&parent.actor())?;
    issue_token(backend, &mut *queue, parent, &p.principal, process)
}

/// Revoke, as the service starts, the supervisors' sign-ins left over from
/// registrations that ended (ADR-t3102-1 decision 2): one a supervisor
/// could not give back because the service it asked to revoke it as it
/// stopped was killed first, one an older binary left, and any other no
/// prune and no sweep revoked. Each is judged in a write transaction as the
/// sweep's is ([`supervisor_ended`]: its registration not kept and the
/// process that asked for it ended); while no service runs, no token
/// authenticates anything. The keys revoked; one that cannot be judged or
/// revoked is logged and left.
pub fn revoke_ended_sign_ins(backend: &dyn ServiceBackend) -> Result<Vec<String>> {
    let mut revoked = Vec::new();
    let signed_in = backend.principals()?.into_iter().filter(|principal| {
        principal.role == ActorRole::Supervisor && principal.registration.is_some()
    });
    for principal in signed_in {
        let key = principal.credential_key();
        // Only a token whose asking process ended is judged in a write
        // transaction, as if its registration were gone: a live
        // supervisor's goes by without the queue's lock, so a busy queue
        // holds no start back for it.
        let asked_ended = backend
            .held(&key)
            .ok()
            .flatten()
            .is_some_and(|held| supervisor_ended(backend, &held, |_| false) == Some(true));
        if !asked_ended {
            continue;
        }
        let done = backend.open(&principal.actor()).and_then(|mut queue| {
            queue.issuing(
                &crate::domain::queue_service::IssueHeld::Nothing,
                backend.now_ms(),
                &mut |facts| {
                    let Some(held) = backend
                        .held(&key)?
                        .filter(|held| held.principal == principal)
                    else {
                        return Ok(json!(false));
                    };
                    let ended = supervisor_ended(backend, &held, |registration| {
                        facts.registered(registration)
                    });
                    if ended != Some(true) {
                        return Ok(json!(false));
                    }
                    Ok(json!(backend.access().revoke(backend.db(), &key)?))
                },
            )
        });
        match done {
            Ok(Value::Bool(true)) => revoked.push(key),
            Ok(_) => {}
            Err(error) => warn!("the token of {key} could not be judged or revoked: {error:#}"),
        }
    }
    Ok(revoked)
}

/// Whether `held`, a supervisor's sign-in, is left over from a
/// registration that ended
/// ([`crate::domain::queue_service::supervisor_token_ended`]), its
/// registration as `registered` reads it and the processes as `backend`
/// does; `None` for any other principal's.
pub(super) fn supervisor_ended(
    backend: &dyn ServiceBackend,
    held: &crate::domain::queue_service::HeldToken,
    registered: impl Fn(&crate::domain::LeaseToken) -> bool,
) -> Option<bool> {
    crate::domain::queue_service::supervisor_token_ended(
        held,
        registered,
        |process| backend.session_ended(process),
        |pid, by| backend.started_by_ended(pid, by),
    )
}

/// The revocation use case (`revoke_token`, ADR-t3102-1 decision 2): the
/// token of `raw`'s principal, a supervisor's sign-in, revoked at the
/// request of `caller`, the principal of the request, once `up` or `down`
/// pruned its registration: the caller is to hold what `down` asks of the
/// policy (the user's, and the inbox's that runs it for the user;
/// [`authorize_operation`]). Then, in a write transaction as the issue's,
/// the token the principal's key holds is revoked only when it is that
/// principal's and its registration is no longer kept
/// ([`crate::domain::queue_service::registration_end_refusal`]), a refusal
/// recorded as `authorization_denied`. The answer is whether one was
/// revoked: its value, record and file.
fn revoke_token_of(backend: &dyn ServiceBackend, caller: &Principal, raw: &Value) -> Result<Value> {
    let p: IssueTokenParams = params(UseCase::RevokeToken, raw)?;
    let child = p.principal;
    let actor = caller.actor();
    let mut queue = backend.open(&actor)?;
    let down = super::commands::operations::Operation::Down;
    let (capability, _) = down.request();
    authorize_operation(&mut *queue, &actor, &down)?;
    let resource = Resource::Principal(child.role);
    let store = Store(&mut *queue);
    let key = child.credential_key();
    let revoked = store.0.issuing(
        &crate::domain::queue_service::IssueHeld::Nothing,
        backend.now_ms(),
        &mut |facts| {
            let Some(held) = backend.held(&key)?.filter(|held| held.principal == child) else {
                return Ok(json!(false));
            };
            if let Some(why) =
                crate::domain::queue_service::registration_end_refusal(&held, |token| {
                    facts.registered(token)
                })
            {
                return Err(Replacing(why).into());
            }
            Ok(json!(backend.access().revoke(backend.db(), &key)?))
        },
    );
    match revoked {
        Err(error) => match error.downcast_ref::<Replacing>() {
            Some(Replacing(why)) => Err(refuse_recorded(
                store.0,
                &actor,
                capability,
                &resource,
                crate::domain::authorization::DenyReason::Resource,
                why,
            )),
            None => Err(error),
        },
        revoked => revoked,
    }
}

/// Issue the token of `child` at `parent`'s request (ADR-t3102-1 decision
/// 2): `token.issue` on a principal of `child`'s role is asked of the
/// policy for `parent` ([`crate::domain::authorization::starts`]), then
/// what the token is issued under
/// ([`crate::domain::queue_service::issue_held`]) is checked in the
/// transaction that writes it, with the token the child's key holds
/// already: the parent and the process that asked for that one, kept with
/// it, decide whether this one may replace it
/// ([`crate::domain::queue_service::reissue`]; a job's only at its
/// own parent's request once that process ended, the same one handed to
/// that parent again while it runs). A refusal of any is
/// `authorization_denied`, recorded with the role asked for (and why it is
/// not held). The token keeps `parent` and its `process` (the one that
/// asked, the connection's). The answer is the file the value is in,
/// outside any worktree and run directory, and whether it is the one held
/// already ([`Issued`]). An executor's telemetry token
/// of a run's target is `issue_telemetry`'s, under the run's lease, and is
/// refused here; one of a target of no run is issued only to the parent
/// that started it, asking from the process its start names, and only
/// while the target holds none, as read in the issue's transaction
/// ([`crate::domain::queue_service::telemetry_target_refusal`]).
pub(super) fn issue_token(
    backend: &dyn ServiceBackend,
    queue: &mut dyn ServiceQueue,
    parent: &Principal,
    child: &Principal,
    process: Option<u32>,
) -> Result<Value> {
    child
        .check()
        .map_err(|error| BadParams(format!("issue_token's principal: {error}")))?;
    if !child.names_itself() {
        return Err(BadParams(format!(
            "issue_token's principal: the actor id {} is not one of a {}",
            child.actor_id,
            child.role.as_str()
        ))
        .into());
    }
    let actor = parent.actor();
    let resource = Resource::Principal(child.role);
    let store = Store(queue);
    Gate {
        actor: &actor,
        authorizer: &StaticPolicy,
    }
    .authorize(&store, Capability::TokenIssue, &resource)?;
    let telemetry = child.role == ActorRole::ExecutorTelemetry;
    if telemetry
        && let Some(why) = crate::domain::queue_service::telemetry_issue_refusal(parent.role, child)
    {
        return Err(refuse_recorded(
            store.0,
            &actor,
            Capability::TokenIssue,
            &resource,
            crate::domain::authorization::DenyReason::Resource,
            why,
        ));
    }
    let db = backend.db();
    let held = crate::domain::queue_service::issue_held(parent, child);
    let issuer = crate::domain::queue_service::TokenIssuer::of(
        parent,
        process.and_then(|pid| backend.process_of(pid)),
    );
    let issued = store.0.issuing(&held, backend.now_ms(), &mut |facts| {
        let file = match (&child.target, telemetry) {
            (Some(target), true) => {
                // Only at the request of the parent that started the
                // target, asking from the process its start names, as the
                // queue records the start in this transaction; and once, as
                // the job starts: one its target holds already is that
                // job's, which no other caller takes over (the issue would
                // revoke it). Looked at in the issue's write transaction,
                // which one request at a time holds, so of two asking at
                // once for the same target only the first is issued.
                if let Some(why) = crate::domain::queue_service::telemetry_target_refusal(
                    parent,
                    process,
                    facts.target_start(target)?.as_ref(),
                    backend
                        .telemetry()
                        .telemetry_credential(db, target)
                        .is_some(),
                ) {
                    return Err(TelemetryRefused(why).into());
                }
                // An executor's under the telemetry's own lock as well,
                // revoking every one its target held (ADR-t1662-3 decision
                // 11).
                Issued::new(backend.issue_telemetry_as(target, child.task_id, None, &issuer)?)
            }
            _ => {
                // Over the token its key holds only as its parent and its
                // process allow, looked at in the issue's write
                // transaction too: of two asking at once, the second finds
                // the first's.
                use crate::domain::queue_service::{Reissue, reissue};
                let key = child.credential_key();
                let held = backend.held(&key)?;
                let kept = held.as_ref().map(|held| held.issued_by.clone());
                match reissue(
                    child,
                    &issuer,
                    kept.as_ref(),
                    parent_there(backend, facts),
                    // Asked only over a token held.
                    || {
                        held.as_ref()
                            .map_or(crate::domain::queue_service::JobState::MayRun, |held| {
                                facts.job(held, Some(&issuer))
                            })
                    },
                ) {
                    Reissue::Issue => Issued::new(backend.issue_as(child, &issuer)?),
                    Reissue::Held => Issued {
                        file: backend
                            .access()
                            .credential(db, &key)
                            .ok_or_else(|| anyhow!("the token held for {key} has no file"))?,
                        held: true,
                    },
                    Reissue::Refused(why) => return Err(Replacing(why).into()),
                }
            }
        };
        Ok(serde_json::to_value(file)?)
    });
    let Err(error) = issued else {
        return issued;
    };
    if let Some(TelemetryRefused(why)) = error.downcast_ref::<TelemetryRefused>() {
        return Err(refuse_recorded(
            store.0,
            &actor,
            Capability::TokenIssue,
            &resource,
            crate::domain::authorization::DenyReason::Resource,
            why,
        ));
    }
    if let Some(Replacing(why)) = error.downcast_ref::<Replacing>() {
        return Err(refuse_recorded(
            store.0,
            &actor,
            Capability::TokenIssue,
            &resource,
            crate::domain::authorization::DenyReason::Resource,
            why,
        ));
    }
    let Some(not_held) = error.downcast_ref::<NotHeld>() else {
        return Err(error);
    };
    let why = not_held.0.clone();
    Err(refuse_recorded(
        store.0,
        &actor,
        Capability::TokenIssue,
        &resource,
        crate::domain::authorization::DenyReason::NotHeld,
        &why,
    ))
}

/// Whether the parent kept with a token is still there
/// ([`crate::domain::queue_service::TokenIssuer::there`]): its process as
/// `backend` reads it, and a supervisor's registration as `facts`, read in
/// the issue's transaction, tell ([`ServiceQueue::issuing`]).
pub(super) fn parent_there<'a>(
    backend: &'a dyn ServiceBackend,
    facts: &'a dyn IssueFacts,
) -> impl Fn(&crate::domain::queue_service::TokenIssuer) -> bool + Copy + 'a {
    move |kept| {
        kept.there(
            |process| !backend.session_ended(process),
            |registration| facts.registered(registration),
        )
    }
}

/// Refuse `actor`'s `capability` on `resource` for `reason`, which the
/// policy alone does not see (`why`: a precondition of the queue the
/// request does not hold): recorded as `authorization_denied` with the
/// why (`not_held` for [`crate::domain::authorization::DenyReason::NotHeld`],
/// else `why`), the error the service answers it with.
pub(super) fn refuse_recorded(
    queue: &mut dyn ServiceQueue,
    actor: &ActorContext,
    capability: Capability,
    resource: &Resource,
    reason: crate::domain::authorization::DenyReason,
    why: &str,
) -> anyhow::Error {
    let refused = AuthorizationError {
        role: actor.role(),
        capability,
        reason,
    };
    let mut payload = super::commands::denial(&refused, resource);
    let key = if reason == crate::domain::authorization::DenyReason::NotHeld {
        "not_held"
    } else {
        "why"
    };
    payload[key] = json!(why);
    if let Err(record) = Store(queue).record_denial(payload) {
        warn!("could not record the refusal ({refused}): {record:#}");
    }
    anyhow::Error::new(refused).context(RefusedBecause(why.to_owned()))
}

/// Why a refusal of [`refuse_recorded`] was made, which its answer says
/// after the refusal ([`respond`]).
#[derive(Debug)]
struct RefusedBecause(String);

impl std::fmt::Display for RefusedBecause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Authorize a supervisor's request (`scheduler.supervise`, recorded as
/// `authorization_denied` when refused): `registered` says the principal
/// is a supervisor's with its registration, which a principal of any other
/// role, or a supervisor's without one, is not.
pub(super) fn authorize_supervisor(
    queue: &mut dyn ServiceQueue,
    actor: &ActorContext,
    registered: bool,
) -> Result<()> {
    let store = Store(queue);
    let gate = Gate {
        actor,
        authorizer: &StaticPolicy,
    };
    if registered {
        gate.authorize(&store, Capability::Supervise, &Resource::Queue)?;
        Ok(())
    } else {
        gate.authorize(&store, Capability::Supervise, &Resource::Unresolved)?;
        // The policy grants a supervisor (and the user) `scheduler.supervise`
        // on every resource: one with no registration is refused all the
        // same, and the refusal recorded as the policy's is.
        Err(refuse_recorded(
            store.0,
            actor,
            Capability::Supervise,
            &Resource::Unresolved,
            crate::domain::authorization::DenyReason::NotGranted,
            "the principal holds no supervisor's registration",
        ))
    }
}

/// Authorize the `capability` an operation of the authorized supervisor
/// `actor` needs on top of `scheduler.supervise`, recording a refusal as
/// [`authorize_supervisor`] does.
pub(super) fn authorize_supervisor_needs(
    queue: &mut dyn ServiceQueue,
    actor: &ActorContext,
    capability: Capability,
) -> Result<()> {
    let store = Store(queue);
    let gate = Gate {
        actor,
        authorizer: &StaticPolicy,
    };
    gate.authorize(&store, capability, &Resource::Queue)?;
    Ok(())
}

/// Authorize the runtime operation `operation` of a control-side command
/// for `actor`, as the command line authorizes it before it runs,
/// recording a refusal.
pub(super) fn authorize_operation(
    queue: &mut dyn ServiceQueue,
    actor: &ActorContext,
    operation: &super::commands::operations::Operation,
) -> Result<()> {
    super::commands::operations::authorize(actor, &StaticPolicy, &Store(queue), operation)
}

/// Check that the integrator `actor` may land `run`, recording a refusal
/// as the supervisor's is ([`authorize_supervisor`]): the policy grants
/// the integrator role [`Capability::Land`] (ADR-t728-2), and no other.
pub(super) fn authorize_integrator(
    queue: &mut dyn ServiceQueue,
    actor: &ActorContext,
    run: &RunId,
) -> Result<()> {
    let store = Store(queue);
    let gate = Gate {
        actor,
        authorizer: &StaticPolicy,
    };
    gate.authorize(
        &store,
        Capability::Land,
        &Resource::Run {
            id: run.clone(),
            task: None,
        },
    )?;
    Ok(())
}

/// A request whose params do not read as the use case's.
#[derive(Debug)]
struct BadParams(String);

impl std::fmt::Display for BadParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BadParams {}

fn params<T: for<'de> Deserialize<'de>>(use_case: UseCase, params: &Value) -> Result<T> {
    let params = if params.is_null() { &json!({}) } else { params };
    serde_json::from_value(params.clone()).map_err(|error| {
        BadParams(format!(
            "the params of {} do not read: {error}",
            use_case.as_str()
        ))
        .into()
    })
}

/// `ask`'s params: those of `dagq ask`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskParams {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    because: Option<String>,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    recommend: Option<String>,
    #[serde(default)]
    confidence: Option<String>,
    #[serde(default)]
    task_id: Option<i64>,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    finding_id: Option<i64>,
    /// The planning request a planner asks on (`--request`), as its own.
    #[serde(default)]
    request_id: Option<i64>,
}

/// The params of a use case that takes none.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoParams {}

/// `show`'s params: those of `dagq show`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShowParams {
    id: i64,
    #[serde(default)]
    full: bool,
    #[serde(default = "default_events")]
    events: usize,
}

const fn default_events() -> usize {
    5
}

/// `note`'s params: those of `dagq note`, one target.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoteParams {
    #[serde(default)]
    task: Option<i64>,
    #[serde(default)]
    run: Option<String>,
    #[serde(default)]
    goal: Option<i64>,
    text: String,
    #[serde(default)]
    kind: Option<String>,
}

/// `proposal_list`'s params: those of `dagq proposal list`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalListParams {
    #[serde(default)]
    all: bool,
}

/// `proposal_show`'s params: those of `dagq proposal show`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalShowParams {
    id: i64,
}

/// `finding_record`'s params: those of `dagq finding record`, one target
/// (`task`, `run`, `goal`, or `queue: true`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FindingRecordParams {
    kind: String,
    #[serde(default)]
    task: Option<i64>,
    #[serde(default)]
    run: Option<String>,
    #[serde(default)]
    goal: Option<i64>,
    #[serde(default)]
    queue: bool,
    #[serde(default)]
    subject: String,
    summary: String,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    impact: Option<String>,
    #[serde(default)]
    evidence: Vec<i64>,
    #[serde(default)]
    propose: Option<String>,
}

/// `finding_resolve`'s and `finding_dismiss`'s params: those of `dagq
/// finding resolve` and `dagq finding dismiss`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FindingStatusParams {
    id: i64,
    reason: String,
    /// `finding dismiss --covered-by` (ADR-t1920-1); absent from an older
    /// client, which never covers.
    #[serde(default)]
    covered_by: Option<i64>,
}

/// `mark`'s params: those of `dagq mark`, a label or the mark to retract.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkParams {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    note: Option<String>,
    /// `--at` as the command line takes it: an event id, `@<unix seconds>`
    /// or an RFC 3339 time.
    #[serde(default)]
    at: Option<String>,
    #[serde(default)]
    retract: Option<i64>,
}

/// `watch_poll`'s params: those of `dagq watch` a look reads.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchPollParams {
    #[serde(default)]
    after: Option<i64>,
    #[serde(default)]
    role: Option<String>,
}

/// `replan_propose`'s params: those of `dagq replan propose`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplanProposeParams {
    run: String,
    reason: String,
    #[serde(default)]
    evidence: Vec<i64>,
}

/// `agent_eval`'s params: those of `dagq agent eval`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentEvalParams {
    agent: String,
    #[serde(default)]
    split: Option<String>,
    #[serde(default)]
    k: Option<u32>,
    #[serde(default)]
    cases: Option<Vec<String>>,
    #[serde(default)]
    set: Option<i64>,
    #[serde(default)]
    rerun: bool,
}

/// `agent_cases`' params: those of `dagq agent cases`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentCasesParams {
    agent: String,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    until: Option<String>,
}

/// `agent_decide`'s params: those of `dagq agent decide`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentDecideParams {
    set: i64,
    case: String,
    verdict: String,
    #[serde(default)]
    codes: Vec<String>,
    reason: String,
}

/// `agent_sets`' params: those of `dagq agent sets`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentSetsParams {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `agent_set`'s params: those of `dagq agent set`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentSetParams {
    id: i64,
}

/// `agent_results`' params: those of `dagq agent results`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentResultsParams {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `agent_result`'s params: those of `dagq agent result`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentResultParams {
    id: i64,
}

/// `denial_record`'s params: the `authorization_denied` payload of a
/// refusal the host's `dagq` made itself
/// ([`super::commands::DenialLog`]).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DenialParams {
    event: String,
    role: String,
    capability: String,
    reason: String,
    resource: Value,
}

/// The [`crate::domain::authorization::DenyReason`] named `name`. The
/// match on the reasons fails to compile when one is added and not listed.
fn deny_reason(name: &str) -> Option<crate::domain::authorization::DenyReason> {
    use crate::domain::authorization::DenyReason as R;
    const ALL: [R; 7] = [
        R::NotGranted,
        R::Reserved,
        R::Resource,
        R::AskKind,
        R::PersonsPriority,
        R::GoalGapDraft,
        R::NotHeld,
    ];
    const fn listed(reason: R) -> usize {
        match reason {
            R::NotGranted => 0,
            R::Reserved => 1,
            R::Resource => 2,
            R::AskKind => 3,
            R::PersonsPriority => 4,
            R::GoalGapDraft => 5,
            R::NotHeld => 6,
        }
    }
    ALL.into_iter()
        .find(|reason| listed(*reason) < ALL.len() && reason.as_str() == name)
}

/// The most bytes of a refusal's resource `denial_record` records.
const DENIAL_RESOURCE_BYTES: usize = 1024;

/// Whether `resource` has the shape of a resource's record
/// ([`Resource::record`]): an object with a `kind`, whose values are
/// scalars or objects of scalars, within [`DENIAL_RESOURCE_BYTES`]. A
/// caller records no free text of its own in the queue this way.
fn resource_record_shape(resource: &Value) -> bool {
    let scalar = |value: &Value| match value {
        Value::String(text) => text.len() <= 256,
        Value::Null | Value::Bool(_) | Value::Number(_) => true,
        _ => false,
    };
    let Some(fields) = resource.as_object() else {
        return false;
    };
    resource["kind"].is_string()
        && fields.values().all(|value| {
            scalar(value)
                || value
                    .as_object()
                    .is_some_and(|inner| inner.values().all(scalar))
        })
        && serde_json::to_vec(resource).is_ok_and(|bytes| bytes.len() <= DENIAL_RESOURCE_BYTES)
}

/// Record the refusal `p` the host's `dagq` of `actor` made itself, as
/// `actor`: a refusal of the caller's own role only. One that names
/// another role is refused as `authorization_denied` (and that refusal
/// recorded), one the policy would not make (a capability the role holds
/// named as not granted) is not recorded, and neither is one whose
/// capability, reason or resource does not read.
fn record_own_denial(
    queue: &mut dyn ServiceQueue,
    actor: &ActorContext,
    p: &DenialParams,
) -> Result<Value> {
    use crate::domain::authorization::DenyReason;
    let bad = |what: &str| -> anyhow::Error { BadParams(format!("denial_record's {what}")).into() };
    if p.event != crate::domain::event_kind::AUTHORIZATION_DENIED {
        return Err(bad("event: not authorization_denied"));
    }
    let capability: Capability = p
        .capability
        .parse()
        .map_err(|_| bad("capability: unknown"))?;
    let reason = deny_reason(&p.reason).ok_or_else(|| bad("reason: unknown"))?;
    if !resource_record_shape(&p.resource) {
        return Err(bad("resource: not the shape of a resource's record"));
    }
    if p.role != actor.role().as_str() {
        return Err(refuse_recorded(
            queue,
            actor,
            capability,
            &Resource::Unresolved,
            DenyReason::Resource,
            "a refusal of another role than the caller's is not recorded",
        ));
    }
    if reason == DenyReason::NotGranted
        && crate::domain::authorization::grants(actor.role()).contains(&capability)
    {
        return Err(bad("reason: the caller's role holds the capability"));
    }
    queue.record_denial(json!({
        "event": p.event,
        "role": actor.role(),
        "capability": capability,
        "reason": reason.as_str(),
        "resource": p.resource,
    }))?;
    Ok(json!({"recorded": true}))
}

/// Authorize reading the whole queue: every role reads it in goal 82
/// (ADR-t1233-5 decision 3), and the policy is still asked, on the
/// service's side, as it would be for a narrower one.
fn authorize_read(store: &Store<'_>, actor: &ActorContext, resource: &Resource) -> Result<()> {
    authorize_as(store, actor, Capability::QueueRead, resource)
}

/// Authorize `capability` on `resource` for `actor`, recording a refusal.
fn authorize_as(
    store: &Store<'_>,
    actor: &ActorContext,
    capability: Capability,
    resource: &Resource,
) -> Result<()> {
    Gate {
        actor,
        authorizer: &StaticPolicy,
    }
    .authorize(store, capability, resource)?;
    Ok(())
}

/// Run `use_case` as `actor` on `queue`.
fn run(
    queue: &mut dyn ServiceQueue,
    actor: &ActorContext,
    use_case: UseCase,
    raw: &Value,
) -> Result<Value> {
    let mut store = Store(queue);
    // The executor's telemetry principal reaches `telemetry_report` alone
    // and a role without `telemetry.report` never reaches it (ADR-t1662-3
    // decision 9): each is refused, and recorded, before its params are
    // read, as the least the other use cases need, a read of the queue, and
    // as `telemetry.report` on no target it may name.
    let gate = Gate {
        actor,
        authorizer: &StaticPolicy,
    };
    if actor.role() == ActorRole::ExecutorTelemetry && use_case != UseCase::TelemetryReport {
        gate.authorize(&store, Capability::QueueRead, &Resource::Queue)?;
    }
    if use_case == UseCase::TelemetryReport
        && !crate::domain::authorization::grants(actor.role())
            .contains(&Capability::TelemetryReport)
    {
        gate.authorize(&store, Capability::TelemetryReport, &Resource::Unresolved)?;
    }
    // The reads of the whole queue (ADR-t1233-5 decisions 1 to 3), each
    // authorized as the command line asks for its command.
    if let Some(read) = QueueRead::parse(use_case, raw)? {
        authorize_as(&store, actor, read.capability(), &read.resource())?;
        return store.0.read(&read);
    }
    // The planning commands (ADR-t3102-3 decision 3), each authorized by
    // the application as the command line authorizes it, refusals
    // recorded.
    if let Some(write) = QueueWrite::parse(use_case, raw)? {
        return store.0.write(&write, actor);
    }
    // The host's dialogue and operations (ADR-t3102-2 decision 6 (4)),
    // each authorized before it runs as the command line authorizes it, or
    // by the application as it runs it.
    if let Some(command) = QueueCommand::parse(use_case, raw)? {
        if let Some((capability, resource)) = command.authorization() {
            authorize_as(&store, actor, capability, &resource)?;
        }
        return store.0.command(&command);
    }
    match use_case {
        UseCase::Hello => unreachable!("hello is answered before a principal"),
        UseCase::Supervisor | UseCase::Control => {
            unreachable!("a supervisor's and a control-side command's requests are answered apart")
        }
        UseCase::IssueToken | UseCase::RevokeToken => {
            unreachable!("an issue and a revocation are answered apart")
        }
        UseCase::List
        | UseCase::Candidates
        | UseCase::Graph
        | UseCase::Status
        | UseCase::Asks
        | UseCase::Events
        | UseCase::Timeline
        | UseCase::Stats
        | UseCase::Kpi
        | UseCase::Forecast
        | UseCase::Notes
        | UseCase::Marks
        | UseCase::Findings
        | UseCase::Search
        | UseCase::Related
        | UseCase::GoalList
        | UseCase::GoalShow
        | UseCase::Lint
        | UseCase::ObserveHistory
        | UseCase::ObserveInput
        | UseCase::Requests
        | UseCase::Replans
        | UseCase::CiFailures
        | UseCase::Planners
        | UseCase::ServiceStatus
        | UseCase::StatsInputs => unreachable!("a read is answered above"),
        UseCase::Add
        | UseCase::Edit
        | UseCase::Submit
        | UseCase::Cancel
        | UseCase::DependencyAdd
        | UseCase::DependencyRemove
        | UseCase::SetGoal
        | UseCase::SetPaths
        | UseCase::SetPriority
        | UseCase::Revisit
        | UseCase::JudgeFollowUp
        | UseCase::Draft
        | UseCase::Ready
        | UseCase::GoalAdd
        | UseCase::GoalEdit
        | UseCase::GoalClose
        | UseCase::GoalReady
        | UseCase::GoalReview
        | UseCase::ProposalWithdraw
        | UseCase::RequestAdd
        | UseCase::RequestDecline => unreachable!("a write is answered above"),
        UseCase::Mark => {
            let p: MarkParams = params(use_case, raw)?;
            let change = match p.retract {
                Some(id) => MarkChange::Retract(EventId::new(id)),
                None => MarkChange::Record {
                    label: p.label.unwrap_or_default(),
                    note: p.note,
                    at: p
                        .at
                        .as_deref()
                        .map(str::parse)
                        .transpose()
                        .map_err(|error| BadParams(format!("mark's at: {error}")))?,
                },
            };
            Dialogue::new(&mut store, actor, &StaticPolicy).mark(change)
        }
        UseCase::Answer
        | UseCase::AskClose
        | UseCase::Hold
        | UseCase::Release
        | UseCase::ReleaseRead
        | UseCase::ReleaseImages
        | UseCase::ReadyInherit
        | UseCase::ReadyInheritRead
        | UseCase::ReadyInheritImages
        | UseCase::ReplanRequest
        | UseCase::Rebind
        | UseCase::Recover
        | UseCase::RunScreen
        | UseCase::RunSend
        | UseCase::RunLog
        | UseCase::PlannerScreen
        | UseCase::PlannerSend
        | UseCase::PlannerLog
        | UseCase::PlannerRequest
        | UseCase::Report
        | UseCase::ReportInputs
        | UseCase::GraphExport => unreachable!("a command is answered above"),
        UseCase::TelemetryReport => {
            // A version the service does not read, or a report past its
            // bounds, is refused before anything is recorded; a field it
            // does not know is dropped (crate::domain::telemetry::read).
            let report = crate::domain::telemetry::read(raw)
                .map_err(|error| BadParams(error.to_string()))?;
            // Of the target the token was issued for, and of a session the
            // control side registered to it.
            let owner = store.0.session_owner(&report.session_id)?;
            gate.authorize(
                &store,
                Capability::TelemetryReport,
                &Resource::Telemetry {
                    target: report.target.clone(),
                    owner,
                },
            )?;
            let recorded = store.0.record_report(&report).map_err(|error| {
                match error.downcast_ref::<StepError>() {
                    Some(refused) => BadParams(refused.to_string()).into(),
                    None => error,
                }
            })?;
            Ok(serde_json::to_value(recorded)?)
        }
        UseCase::SessionRun | UseCase::SessionRecord => {
            // One operation of a session wrapper's (ADR-t3102-3 decision
            // 3), carried by its own use case and asked of the policy for
            // its own run or planner: only the wrapper's principal holds
            // `session.run` and `session.record` (ADR-t3102-1 decision 4).
            let op: super::session::SessionOp = params(use_case, raw)?;
            if op.use_case() != use_case {
                return Err(BadParams(format!(
                    "{} does not carry this operation: {} does",
                    use_case.as_str(),
                    op.use_case().as_str()
                ))
                .into());
            }
            let (capability, resource) = op.request();
            gate.authorize(&store, capability, &resource)?;
            if !op.permits(actor) {
                let error = crate::domain::AuthorizationError {
                    role: actor.role(),
                    capability,
                    reason: crate::domain::authorization::DenyReason::Resource,
                };
                super::commands::record(&store, &error, &resource);
                return Err(error.into());
            }
            store.0.session(&op)
        }
        UseCase::DenialRecord => {
            let p: DenialParams = params(use_case, raw)?;
            record_own_denial(store.0, actor, &p)
        }
        UseCase::WatchPoll => {
            let p: WatchPollParams = params(use_case, raw)?;
            let role = p
                .role
                .as_deref()
                .map(str::parse::<SessionRole>)
                .transpose()
                .map_err(|error| BadParams(format!("watch_poll's role: {error}")))?;
            gate.authorize(&store, Capability::QueueWatch, &Resource::Queue)?;
            let look = store.0.watch_look(p.after.map(EventId::new), role)?;
            Ok(serde_json::to_value(look)?)
        }
        UseCase::AgentEval => {
            let p: AgentEvalParams = params(use_case, raw)?;
            let name = p.split.as_deref().unwrap_or("dev");
            let split = crate::domain::agent_eval::Split::ALL
                .into_iter()
                .find(|split| split.as_str() == name)
                .ok_or_else(|| BadParams(format!("agent_eval's split: unknown {name}")))?;
            super::agent_eval::AgentEvals::new(&*store.0, actor, &StaticPolicy).request(
                &super::agent_eval::EvalRequest {
                    agent: p.agent,
                    split,
                    k: p.k,
                    cases: p.cases,
                    set: p.set,
                    rerun: p.rerun,
                },
            )
        }
        UseCase::AgentCases => {
            let p: AgentCasesParams = params(use_case, raw)?;
            super::agent_cases::AgentCaseSets::new(&*store.0, actor, &StaticPolicy)
                .request(&p.agent, p.since, p.until)
        }
        UseCase::AgentDecide => {
            let p: AgentDecideParams = params(use_case, raw)?;
            super::agent_cases::AgentCaseSets::new(&*store.0, actor, &StaticPolicy)
                .decide(p.set, &p.case, &p.verdict, p.codes, &p.reason)
        }
        UseCase::AgentSets => {
            let p: AgentSetsParams = params(use_case, raw)?;
            super::agent_cases::AgentCaseSets::new(&*store.0, actor, &StaticPolicy)
                .list(p.agent.as_deref(), p.limit.unwrap_or(20))
        }
        UseCase::AgentSet => {
            let p: AgentSetParams = params(use_case, raw)?;
            super::agent_cases::AgentCaseSets::new(&*store.0, actor, &StaticPolicy).show(p.id)
        }
        UseCase::AgentResults => {
            let p: AgentResultsParams = params(use_case, raw)?;
            super::agent_eval::AgentEvals::new(&*store.0, actor, &StaticPolicy)
                .list(p.agent.as_deref(), p.limit.unwrap_or(20))
        }
        UseCase::AgentResult => {
            let p: AgentResultParams = params(use_case, raw)?;
            super::agent_eval::AgentEvals::new(&*store.0, actor, &StaticPolicy).show(p.id)
        }
        UseCase::Ask => {
            let p: AskParams = params(use_case, raw)?;
            let ask = NewAsk {
                recommendation: p.recommend,
                confidence: p.confidence.as_deref().map(str::parse).transpose()?,
                kind: p.kind.unwrap_or_default().parse::<AskKind>()?,
                task_id: p.task_id.map(TaskId::new),
                run_id: p.run_id.map(RunId::new).transpose()?,
                question: p.question.unwrap_or_default(),
                options: p.options,
                asked_by: actor.written_by().to_owned(),
                reason_category: p.because.unwrap_or_default().parse::<AskReason>()?,
                topics: p.topics,
                finding_id: p.finding_id.map(FindingId::new),
                request_id: p.request_id.map(RequestId::new),
            };
            Dialogue::new(&mut store, actor, &StaticPolicy).ask(ask)
        }
        UseCase::Note => {
            let p: NoteParams = params(use_case, raw)?;
            let target = match (p.task, p.run, p.goal) {
                (Some(task), None, None) => NoteTarget::Task(TaskId::new(task)),
                (None, Some(run), None) => NoteTarget::Run(RunId::new(run)?),
                (None, None, Some(goal)) => NoteTarget::Goal(GoalId::new(goal)),
                _ => {
                    return Err(
                        BadParams("note names one target: task, run or goal".to_owned()).into(),
                    );
                }
            };
            let note = Dialogue::new(&mut store, actor, &StaticPolicy).note(NewNote {
                target,
                text: p.text,
                kind: p.kind,
                by: actor.written_by().to_owned(),
            })?;
            Ok(serde_json::to_value(note)?)
        }
        UseCase::Show => {
            let p: ShowParams = params(use_case, raw)?;
            let id = TaskId::new(p.id);
            authorize_read(&store, actor, &Resource::task(id))?;
            store.0.show(id, p.full, p.events)
        }
        UseCase::ProposalList => {
            let p: ProposalListParams = params(use_case, raw)?;
            authorize_read(&store, actor, &Resource::Queue)?;
            store.0.proposals(p.all)
        }
        UseCase::ProposalShow => {
            let p: ProposalShowParams = params(use_case, raw)?;
            // The command line reads a proposal as the queue
            // (`queue.read` on the queue), and so does the service.
            authorize_read(&store, actor, &Resource::Queue)?;
            store.0.show_proposal(ProposalId::new(p.id))
        }
        UseCase::FindingRecord => {
            let p: FindingRecordParams = params(use_case, raw)?;
            let target = match (p.task, p.run, p.goal, p.queue) {
                (Some(task), None, None, false) => FindingTarget::Task(TaskId::new(task)),
                (None, Some(run), None, false) => FindingTarget::Run(RunId::new(run)?),
                (None, None, Some(goal), false) => FindingTarget::Goal(GoalId::new(goal)),
                (None, None, None, true) => FindingTarget::Queue,
                _ => {
                    return Err(BadParams(
                        "finding_record names one target: task, run, goal or queue".to_owned(),
                    )
                    .into());
                }
            };
            let outcome =
                Dialogue::new(&mut store, actor, &StaticPolicy).record_finding(NewFinding {
                    kind: p.kind,
                    target,
                    subject: p.subject,
                    summary: p.summary,
                    detail: p.detail,
                    // `dagq finding record` takes high, normal or low
                    // only, before anything runs.
                    impact: p
                        .impact
                        .map(|impact| impact.parse())
                        .transpose()
                        .map_err(|error| BadParams(format!("finding_record's impact: {error}")))?,
                    evidence: p.evidence.into_iter().map(EventId::new).collect(),
                    propose: p.propose,
                    by: actor.written_by().to_owned(),
                })?;
            Ok(serde_json::to_value(outcome)?)
        }
        UseCase::ReplanPropose => {
            let p: ReplanProposeParams = params(use_case, raw)?;
            let recorded = super::commands::replans::Replans::new(&mut store, actor, &StaticPolicy)
                .propose(&crate::domain::task_replan::NewReplan {
                    run: RunId::new(p.run)?,
                    reason: p.reason,
                    evidence: p.evidence.into_iter().map(EventId::new).collect(),
                })?;
            Ok(serde_json::to_value(recorded)?)
        }
        UseCase::BrokerRunStatus => {
            let p: crate::domain::queue_broker::BrokerRunStatusParams = params(use_case, raw)?;
            let run = RunId::new(p.run).map_err(|error| BadParams(error.to_string()))?;
            authorize_read(&store, actor, &Resource::Queue)?;
            let status = store.0.run_status(&run)?;
            Ok(serde_json::to_value(
                crate::domain::queue_broker::BrokerRunStatus { status },
            )?)
        }
        UseCase::BrokerBinding => {
            let _: NoParams = params(use_case, raw)?;
            authorize_read(&store, actor, &Resource::Queue)?;
            let binding = store.0.repository_binding()?;
            Ok(serde_json::to_value(
                crate::domain::queue_broker::BrokerBinding { binding },
            )?)
        }
        UseCase::BrokerRecord => {
            // The policy first, so an AI actor's is refused and recorded
            // whatever it names; then only the broker's two kinds.
            gate.authorize(&store, Capability::QueueBrokerRecord, &Resource::Queue)?;
            let p: crate::domain::queue_broker::BrokerRecordParams = params(use_case, raw)?;
            if !crate::domain::queue_broker::broker_records(p.kind) {
                return Err(BadParams(format!(
                    "broker_record records queue_broker_denied and queue_broker_tokens_swept, \
                     not {}",
                    p.kind.as_str()
                ))
                .into());
            }
            let id = store.0.record_broker_event(p.kind, p.payload)?;
            Ok(json!({"recorded": id.as_i64()}))
        }
        UseCase::FindingResolve | UseCase::FindingDismiss => {
            let p: FindingStatusParams = params(use_case, raw)?;
            let id = FindingId::new(p.id);
            let mut dialogue = Dialogue::new(&mut store, actor, &StaticPolicy);
            let finding = if use_case == UseCase::FindingResolve {
                dialogue.resolve_finding(id, &p.reason)?
            } else {
                dialogue.dismiss_finding(id, &p.reason, p.covered_by.map(TaskId::new))?
            };
            Ok(serde_json::to_value(finding)?)
        }
    }
}

/// A [`ServiceQueue`] as the [`Dialogue`] commands' store.
struct Store<'a>(&'a mut dyn ServiceQueue);

impl super::commands::replans::ReplanStore for Store<'_> {
    fn run_task(&self, run: &RunId) -> Result<Option<TaskId>> {
        self.0.run_task(run)
    }
    fn record_replan(
        &mut self,
        new: &crate::domain::task_replan::NewReplan,
        origin: crate::domain::task_replan::ReplanOrigin,
        by_role: &str,
        by_id: &str,
    ) -> Result<crate::domain::task_replan::ReplanRecorded> {
        self.0.record_replan(new, origin, by_role, by_id)
    }
    fn replan_operation(
        &self,
        id: crate::domain::ReplanId,
    ) -> Result<crate::domain::task_replan::ReplanOperation> {
        self.0.replan_operation(id)
    }
    fn move_replan(
        &mut self,
        id: crate::domain::ReplanId,
        generation: i64,
        step: &crate::domain::task_replan::ReplanMove,
    ) -> Result<crate::domain::task_replan::ReplanOperation> {
        self.0.move_replan(id, generation, step)
    }
}

impl DenialLog for Store<'_> {
    fn record_denial(&self, payload: Value) -> Result<()> {
        self.0.record_denial(payload)
    }
}

impl DialogueStore for Store<'_> {
    fn read_ask(&self, id: AskId) -> Result<Ask> {
        self.0.read_ask(id)
    }
    fn request_planner(&self, request: RequestId) -> Result<Option<PlannerId>> {
        self.0.request_planner(request)
    }
    fn open_ask(&mut self, ask: NewAsk) -> Result<Value> {
        self.0.open_ask(ask)
    }
    fn answer(&mut self, id: AskId, text: &str, answerer: Answerer) -> Result<Ask> {
        self.0.answer(id, text, answerer)
    }
    fn close_ask(&mut self, id: AskId) -> Result<Ask> {
        self.0.close_ask(id)
    }
    fn add_note(&mut self, note: NewNote) -> Result<RunEvent> {
        self.0.add_note(note)
    }
    fn mark(&mut self, change: MarkChange, by: &str) -> Result<Value> {
        self.0.mark(change, by)
    }
    fn record_finding(&mut self, finding: NewFinding) -> Result<FindingOutcome> {
        self.0.record_finding(finding)
    }
    fn set_finding_status(
        &mut self,
        id: FindingId,
        to: FindingStatus,
        reason: &str,
        by: &str,
        covered_by: Option<TaskId>,
    ) -> Result<Finding> {
        self.0.set_finding_status(id, to, reason, by, covered_by)
    }
}

/// What a look at the queue's service found.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ServiceProbe {
    /// `running` (it answered `hello`), `stopped` (nothing recorded, or
    /// its process is gone) or `unreachable` (its process lives and does
    /// not answer).
    pub state: ServiceState,
    pub socket: std::path::PathBuf,
    pub pid: Option<u32>,
    pub build: Option<String>,
    pub api_version: Option<u32>,
    pub min_api_version: Option<u32>,
    /// Whether it runs this binary's build.
    pub build_matches: Option<bool>,
    pub started_at: Option<i64>,
    /// Why it does not answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ServiceProbe {
    /// Running this binary's build: nothing to do.
    pub fn current(&self) -> bool {
        self.state == ServiceState::Running && self.build_matches == Some(true)
    }
}

/// How `up`, `down` and the supervisor start, look at and stop the
/// queue's service (ADR-t1233-4 decisions 1 and 2).
pub trait QueueServiceControl: Send + Sync {
    /// Look at the service: its record and whether it answers `hello`.
    fn probe(&self) -> ServiceProbe;
    /// Start the service of this binary, stopping one of another build
    /// first, and wait up to `timeout` for it to answer.
    fn start(&self, timeout: Duration) -> Result<ServiceProbe>;
    /// Stop the service; `None` when none ran.
    fn stop(&self, timeout: Duration) -> Result<Option<u32>>;
    /// Start the service of this binary as a replacement's
    /// ([`crate::domain::queue_service::ServiceStart::Replacement`], the
    /// breaking migrations too with `breaking`, ADR-t3102-2 decision 2),
    /// unless `reuse` lets one of this build that answers on a queue it
    /// found with nothing pending stay. The started service takes over
    /// from the one running only once it found it may apply what is
    /// pending, and applies it before it accepts; one that refuses then
    /// (not the queue's binary, a breaking migration not allowed, a queue in
    /// use) leaves the running one as it was and accepts nothing. One that
    /// fails after it took over (the queue found in use again in the
    /// migration's transaction, the migration itself) leaves none running.
    /// Replacements of one queue start their services one at a time.
    fn replace(&self, breaking: bool, reuse: bool, timeout: Duration) -> Result<Replaced>;
    /// Hold the service against the supervisors' watch while a replacement
    /// puts its build in place and hands the supervisors over to it
    /// (ADR-t3102-2 decision 2): a supervisor of the build before would
    /// otherwise replace the service of the build after, the one that
    /// migrated, before the handoff reaches it. Let go when dropped, or
    /// when the holding process ends; each hold holds until it is let go,
    /// whatever other holds end meanwhile. A supervisor of a build before
    /// the hold knows none, and the replacement goes through it all the
    /// same (the service applied the migrations before it accepted).
    fn hold(&self) -> Result<ServiceHold> {
        Ok(ServiceHold::none())
    }
    /// Whether a live replacement holds the service ([`Self::hold`]).
    fn held(&self) -> bool {
        false
    }
}

/// A replacement's hold on the queue's service
/// ([`QueueServiceControl::hold`]), let go when dropped.
pub struct ServiceHold {
    _guard: Option<Box<dyn std::any::Any + Send>>,
}

impl ServiceHold {
    /// A hold that holds nothing.
    pub fn none() -> Self {
        Self { _guard: None }
    }

    /// The hold `guard` keeps until it is dropped.
    pub fn new(guard: Box<dyn std::any::Any + Send>) -> Self {
        Self {
            _guard: Some(guard),
        }
    }
}

/// What [`QueueServiceControl::replace`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replaced {
    /// The service of this build that answers now.
    pub service: ServiceProbe,
    /// Whether that is the one that ran already.
    pub reused: bool,
    /// The one the started service took over from.
    pub replaced: Option<ServiceProbe>,
    /// What the started service applied before it accepted (`migrate`'s
    /// report); `None` when nothing was pending or none started.
    pub migrated: Option<Value>,
}

/// The answer to a token's issue ([`ServiceAccess::issue`]): the file its
/// value is in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issued {
    pub file: std::path::PathBuf,
    /// Whether the token is the one its key held already, handed over
    /// again ([`crate::domain::queue_service::Reissue::Held`]): another
    /// start's of the same job's actor and parent, which gives it back at
    /// its own end, so a start handed it gives back nothing when it does
    /// not start.
    pub held: bool,
}

impl Issued {
    /// A token this request issued.
    pub fn new(file: std::path::PathBuf) -> Self {
        Self { file, held: false }
    }
}

/// What the control side gives an actor whose `dagq` runs in client mode
/// (goal 82's stage (3), ADR-t1233-4 decision 4): the socket of the queue
/// at `db`, and the file of a token for the actor's principal. Only the
/// control side holds it: the supervisor issues a worker's token at the
/// claim and the resume and a job's at its start, and no command of an AI
/// actor's issues one. The supervisor revokes a worker's token when its run
/// ends, and sweeps those whose runs ended while it did not look.
pub trait ServiceAccess: Send + Sync {
    /// The socket of the service of the queue at `db`.
    fn socket(&self, db: &std::path::Path) -> std::path::PathBuf;
    /// Issue a token for `principal` (the one its actor held is revoked),
    /// or be handed the one held already ([`Issued::held`]).
    fn issue(&self, db: &std::path::Path, principal: &Principal) -> Result<Issued>;
    /// The file of the token `actor_id` holds, `None` when it holds none.
    fn credential(&self, db: &std::path::Path, actor_id: &str) -> Option<std::path::PathBuf>;
    /// Revoke the token `actor_id` holds (a worker's whose run ended):
    /// whether there was one.
    fn revoke(&self, db: &std::path::Path, actor_id: &str) -> Result<bool>;
    /// Revoke every token of the queue at `db` whose principal `ended`
    /// says has ended: the actor ids of those revoked.
    fn sweep(
        &self,
        db: &std::path::Path,
        ended: &mut dyn FnMut(&Principal) -> bool,
    ) -> Result<Vec<String>>;
    /// `child`, which revokes the token of `actor_id` once it has ended (a
    /// job's, whose token ends with it): when a wait sees it end, or when
    /// the handle is dropped.
    fn revoke_on_exit(
        &self,
        db: &std::path::Path,
        actor_id: &str,
        child: Box<dyn super::Spawned>,
    ) -> Box<dyn super::Spawned>;
}

/// How the control side issues and revokes the token of an executor's
/// telemetry principal (ADR-t1662-3 decision 11): only the control side
/// calls it, and no command of an AI actor's does. The token's file is
/// the executor's to read, found by its target
/// ([`Self::telemetry_credential`]); it is handed to no agent, neither in
/// its environment nor on its command line.
pub trait TelemetryTokens: Send + Sync {
    /// Issue the token of the telemetry principal of `target` of the queue
    /// at `db` (of `task`, for a run target), under `issued_under` (the
    /// lease of the supervisor that issues it), revoking every one it
    /// held: the file holding it.
    fn issue_telemetry(
        &self,
        db: &std::path::Path,
        target: &StepTarget,
        task: Option<TaskId>,
        issued_under: Option<&crate::domain::LeaseToken>,
    ) -> Result<std::path::PathBuf>;
    /// Revoke the tokens of the telemetry principal of `target`, only those
    /// issued under `issued_under` when it is given: whether there was one.
    /// Once it returns `Ok`, none of them authenticates; an error may leave
    /// one that does, which [`Self::held_telemetry`] still lists.
    fn revoke_telemetry(
        &self,
        db: &std::path::Path,
        target: &StepTarget,
        issued_under: Option<&crate::domain::LeaseToken>,
    ) -> Result<bool>;
    /// Where the executor of `target` reads its token: a file outside every
    /// worktree and run directory, `None` while none holds.
    fn telemetry_credential(
        &self,
        db: &std::path::Path,
        target: &StepTarget,
    ) -> Option<std::path::PathBuf>;
    /// The files the session wrappers of the queue at `db` kept their
    /// targets' step numbering and unsent steps in
    /// ([`crate::application::TelemetrySinks::steps_file`]), each with the
    /// target it names and its age, for the sweep that removes those of
    /// the targets that ended.
    fn kept_steps(
        &self,
        db: &std::path::Path,
    ) -> Result<Vec<crate::domain::telemetry::KeptStepsFile>>;
    /// Remove `file`, one [`Self::kept_steps`] listed, and the temporary
    /// file a write of it left. One that is not there is no error.
    fn forget_steps(&self, db: &std::path::Path, file: &std::path::Path) -> Result<()>;
    /// The telemetry tokens held, each with its target and the lease it was
    /// issued under: the sweep's list, which keeps a token whose revocation
    /// failed.
    fn held_telemetry(
        &self,
        db: &std::path::Path,
    ) -> Result<Vec<crate::domain::telemetry::HeldTelemetry>>;
    /// What the queue at `db` holds of `target`, a target of no run, for
    /// whether its token is due ([`crate::domain::telemetry::target_token_due`]):
    /// what the sweep reads, as the service does at each request.
    fn target_facts(&self, db: &std::path::Path, target: &StepTarget) -> Result<TargetFacts>;
    /// Register `session_id` as `target`'s in the queue at `db`
    /// ([`crate::application::SessionOwners::register_session`]): the
    /// control side that starts the agent asks for it through the op
    /// `register_telemetry_session` of the service's `supervisor` or
    /// `control` use case when it starts or learns of the session, before
    /// or while its executor reports, and the service checks what is
    /// registered (on `control`, also that the target is the caller's)
    /// before it records. Again to the same target changes nothing, and
    /// one of another target is refused. No worker, planner, job or
    /// executor token can ask for it.
    fn register_telemetry_session(
        &self,
        db: &std::path::Path,
        target: &StepTarget,
        session_id: &str,
    ) -> Result<()>;
}

/// `up`'s step (ADR-t1233-4 decision 1): reuse a service of this build
/// that answers, else start one (replacing one of another build), and
/// report what was done. A service that does not start stops `up`.
pub fn ensure(control: &dyn QueueServiceControl, timeout: Duration) -> Result<Value> {
    let found = control.probe();
    if found.current() {
        return Ok(json!({"outcome": "reused", "service": found}));
    }
    let replaced = (found.state != ServiceState::Stopped).then(|| found.clone());
    let started = control
        .start(timeout)
        .context("the queue service did not start")?;
    Ok(json!({
        "outcome": if replaced.is_some() { "replaced" } else { "started" },
        "service": started,
        "replaced": replaced,
    }))
}

/// `up`'s step (ADR-t1233-4 decision 1) for a replacement (ADR-t3102-2
/// decision 2): the service of this build by
/// [`QueueServiceControl::replace`], reported as [`ensure`] reports it with
/// what the started service applied (`migrated`). A service that does not
/// start stops `up`.
pub fn ensure_replaced(control: &dyn QueueServiceControl, timeout: Duration) -> Result<Value> {
    let replaced = control
        .replace(false, true, timeout)
        .context("the queue service did not start")?;
    let outcome = match (&replaced.replaced, replaced.reused) {
        (_, true) => "reused",
        (Some(_), false) => "replaced",
        (None, false) => "started",
    };
    Ok(json!({
        "outcome": outcome,
        "service": replaced.service,
        "replaced": replaced.replaced,
        "migrated": replaced.migrated,
    }))
}

/// The queue service as `status`, `doctor` and `dagq service status` show
/// it (ADR-t1233-4): `probe`, what a look at its record and its socket
/// found, the API version this binary speaks, and the [`attention`] of the
/// queue's `log` when it can be read (`null` without one).
pub fn view(probe: &ServiceProbe, log: Option<&dyn super::RunLog>) -> Value {
    view_with(probe, log.map_or(Value::Null, attention))
}

/// Whether the attention the latest of the service's events in `log`
/// leaves stands (`queue_service_down`): a boolean, or `{"error"}` when the
/// read fails.
pub fn attention(log: &dyn super::RunLog) -> Value {
    use crate::domain::queue_service::QUEUE_SERVICE_ATTENTION_KINDS;
    match log.latest_queue_event(&QUEUE_SERVICE_ATTENTION_KINDS) {
        Ok(latest) => json!(latest.is_some_and(|event| {
            crate::domain::queue_service::attention_stands(Some(&event.kind))
        })),
        Err(error) => json!({"error": format!("{error:#}")}),
    }
}

/// [`view`] with its `attention` read elsewhere: the host's `dagq service
/// status` reads it through the service (`service_status`), and opens no
/// queue.
pub fn view_with(probe: &ServiceProbe, attention: Value) -> Value {
    let mut view = serde_json::to_value(probe).unwrap_or(Value::Null);
    view["client_api_version"] = json!(API_VERSION);
    view["attention"] = attention;
    view
}

/// What [`unreached_view`] says to do next.
pub const UNREACHED_NEXT: &str = "dagq up (or dagq service start) starts it; with an outage \
                                  recorded, the supervisor's log says why it could not keep it";

/// The host's `dagq service status` of a service that did not answer
/// (ADR-t3102-3 decision 2), read without opening the queue: [`view_with`]
/// the attention unknown (`null`), the `outage` its supervisor recorded
/// outside the DB (`null` when none stands: no supervisor has found it
/// down since it last answered), and what to do `next`.
pub fn unreached_view(
    probe: &ServiceProbe,
    outage: Option<&crate::domain::queue_service::ServiceOutage>,
) -> Value {
    let mut view = view_with(probe, Value::Null);
    view["outage"] = json!(outage);
    view["next"] = json!(UNREACHED_NEXT);
    view
}

/// What the host's `dagq service status` asked the service for its
/// `service_status` came to.
#[derive(Debug, Clone, PartialEq)]
pub enum ServiceStatusRead {
    /// The service's answer, `{"attention"}`.
    Answered(Value),
    /// Nothing answered (`unreachable`), and why.
    Unreached(String),
    /// The service answered with a refusal or an error (`wrong_queue`,
    /// `authorization_denied`, a use case an older build does not know).
    Refused(String),
}

/// Where the host's `dagq service status` looks, none of it in the DB: the
/// service's record and socket, its read through the service, and the
/// outage its supervisor recorded outside the DB.
pub trait ServiceStatusSource {
    fn probe(&self) -> ServiceProbe;
    /// Ask the service for `service_status`.
    fn read(&self) -> ServiceStatusRead;
    fn recorded_outage(&self) -> Option<crate::domain::queue_service::ServiceOutage>;
}

/// The host's `dagq service status` (ADR-t3102-2 decision 1, ADR-t3102-3
/// decisions 1 and 2): a service the look finds running is asked for its
/// attention ([`view_with`]); a refusal leaves the attention as
/// `{"error"}`; one the look does not find running, or that does not
/// answer the read, is [`unreached_view`], `unreachable` with why when the
/// look found it running.
pub fn host_status(source: &dyn ServiceStatusSource) -> Value {
    let mut probe = source.probe();
    if probe.state == ServiceState::Running {
        match source.read() {
            ServiceStatusRead::Answered(answer) => {
                return view_with(&probe, answer["attention"].clone());
            }
            ServiceStatusRead::Refused(why) => return view_with(&probe, json!({"error": why})),
            ServiceStatusRead::Unreached(why) => {
                probe.state = ServiceState::Unreachable;
                probe.error = Some(why);
            }
        }
    }
    unreached_view(&probe, source.recorded_outage().as_ref())
}

/// `dagq service start`: [`ensure`] the service with `control`, recorded
/// in `log` as `queue_service_started` (`by: service start`) unless one of
/// this build already answered.
pub fn start_by_hand(
    log: &dyn super::RunLog,
    control: &dyn QueueServiceControl,
    timeout: Duration,
) -> Result<Value> {
    let report = ensure(control, timeout)?;
    if report["outcome"] != "reused" {
        log.record_queue_event(
            EventKind::QueueServiceStarted,
            json!({
                "by": "service start",
                "pid": report["service"]["pid"],
                "build": report["service"]["build"],
                "api_version": report["service"]["api_version"],
                "socket": report["service"]["socket"],
                "restart": false,
                "replaced": report["replaced"],
            }),
        )?;
    }
    Ok(report)
}

/// `dagq service stop`: stop the queue's service with `control`, recorded
/// in `log` as `queue_service_stopped` (`by: service stop`) when one ran. A
/// supervisor at work starts it again at its next look.
pub fn stop_by_hand(
    log: &dyn super::RunLog,
    control: &dyn QueueServiceControl,
    timeout: Duration,
) -> Result<Value> {
    Ok(match control.stop(timeout)? {
        Some(pid) => {
            log.record_queue_event(
                EventKind::QueueServiceStopped,
                json!({"pid": pid, "by": "service stop"}),
            )?;
            json!({"outcome": "stopped", "pid": pid})
        }
        None => json!({"outcome": "not_running"}),
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Mutex;

    use anyhow::anyhow;

    use super::*;
    use crate::domain::Answerer;
    use crate::domain::queue_service::SessionProcess;

    /// One queue: task 1 with run `r1` in `status`; everything it was
    /// asked to write is kept.
    struct Queue {
        status: RunStatus,
        log: std::sync::Arc<Mutex<Vec<Value>>>,
        denials: RefCell<Vec<Value>>,
    }

    impl DenialLog for Queue {
        fn record_denial(&self, payload: Value) -> Result<()> {
            self.denials.borrow_mut().push(payload.clone());
            self.log.lock().unwrap().push(payload);
            Ok(())
        }
    }

    impl DialogueStore for Queue {
        fn read_ask(&self, _: AskId) -> Result<Ask> {
            Err(anyhow!("no ask"))
        }
        /// Request 7 is planner 3's, request 8 planner 4's.
        fn request_planner(&self, request: RequestId) -> Result<Option<PlannerId>> {
            Ok(match request.as_i64() {
                7 => Some(PlannerId::new(3)),
                8 => Some(PlannerId::new(4)),
                _ => None,
            })
        }
        fn open_ask(&mut self, ask: NewAsk) -> Result<Value> {
            let mut value =
                json!({"opened": ask.kind.as_str(), "run": ask.run_id, "by": ask.asked_by});
            if let Some(request) = ask.request_id {
                value["request"] = json!(request);
            }
            self.log.lock().unwrap().push(value.clone());
            Ok(value)
        }
        fn answer(&mut self, _: AskId, _: &str, _: Answerer) -> Result<Ask> {
            Err(anyhow!("answer"))
        }
        fn close_ask(&mut self, _: AskId) -> Result<Ask> {
            Err(anyhow!("close"))
        }
        fn add_note(&mut self, note: NewNote) -> Result<RunEvent> {
            self.log
                .lock()
                .unwrap()
                .push(json!({"note": note.text, "by": note.by}));
            Err(anyhow!("noted"))
        }
        fn mark(&mut self, _: MarkChange, _: &str) -> Result<Value> {
            Err(anyhow!("mark"))
        }
        fn record_finding(&mut self, finding: NewFinding) -> Result<FindingOutcome> {
            self.log.lock().unwrap().push(json!({
                "finding": finding.kind, "target": format!("{:?}", finding.target),
                "by": finding.by,
            }));
            Err(anyhow!("recorded"))
        }
        fn set_finding_status(
            &mut self,
            id: FindingId,
            to: FindingStatus,
            reason: &str,
            by: &str,
            _covered_by: Option<TaskId>,
        ) -> Result<Finding> {
            self.log
                .lock()
                .unwrap()
                .push(json!({"finding": id, "to": to.as_str(), "reason": reason, "by": by}));
            Err(anyhow!("set"))
        }
    }

    /// The eval's events are kept in the log, each as its kind and
    /// payload; none is read back.
    impl crate::application::agent_eval::EvalStore for Queue {
        fn record_eval_event(&self, kind: EventKind, payload: Value) -> Result<EventId> {
            let mut log = self.log.lock().unwrap();
            log.push(json!({"eval_event": kind.as_str(), "payload": payload}));
            Ok(EventId::new(i64::try_from(log.len()).unwrap()))
        }
        fn eval_events(&self) -> Result<Vec<RunEvent>> {
            Ok(Vec::new())
        }
    }

    /// A replanning request or proposal is kept in the log as its origin,
    /// run and recorder; none is read back.
    impl crate::application::commands::replans::ReplanStore for Queue {
        fn run_task(&self, run: &RunId) -> Result<Option<TaskId>> {
            Ok((run.as_str() == "r1").then_some(TaskId::new(1)))
        }
        fn record_replan(
            &mut self,
            new: &crate::domain::task_replan::NewReplan,
            origin: crate::domain::task_replan::ReplanOrigin,
            by_role: &str,
            _: &str,
        ) -> Result<crate::domain::task_replan::ReplanRecorded> {
            self.log
                .lock()
                .unwrap()
                .push(json!({"replan": origin.as_str(), "run": new.run, "by": by_role}));
            Err(anyhow!("replan recorded"))
        }
        fn replan_operation(
            &self,
            _: crate::domain::ReplanId,
        ) -> Result<crate::domain::task_replan::ReplanOperation> {
            Err(anyhow!("no operation"))
        }
        fn move_replan(
            &mut self,
            _: crate::domain::ReplanId,
            _: i64,
            _: &crate::domain::task_replan::ReplanMove,
        ) -> Result<crate::domain::task_replan::ReplanOperation> {
            Err(anyhow!("no move"))
        }
    }

    impl crate::application::agent_cases::CaseSetStore for Queue {
        fn record_cases_event(&self, kind: EventKind, payload: Value) -> Result<EventId> {
            let mut log = self.log.lock().unwrap();
            log.push(json!({"cases_event": kind.as_str(), "payload": payload}));
            Ok(EventId::new(i64::try_from(log.len()).unwrap()))
        }
        fn cases_events(&self) -> Result<Vec<RunEvent>> {
            Ok(Vec::new())
        }
    }

    impl ServiceQueue for Queue {
        fn show(&mut self, id: TaskId, full: bool, events: usize) -> Result<Value> {
            Ok(json!({"id": id, "full": full, "events": events}))
        }
        fn run_status(&self, id: &RunId) -> Result<Option<RunStatus>> {
            Ok((id.as_str() == "r1").then_some(self.status))
        }
        fn repository_binding(&self) -> Result<Option<String>> {
            Ok(Some("/repo/.git".to_owned()))
        }
        fn record_broker_event(&self, kind: EventKind, payload: Value) -> Result<EventId> {
            let mut log = self.log.lock().unwrap();
            log.push(json!({"broker_event": kind.as_str(), "payload": payload}));
            Ok(EventId::new(i64::try_from(log.len()).unwrap()))
        }
        fn proposals(&self, all: bool) -> Result<Value> {
            Ok(json!({"proposals": [], "all": all}))
        }
        fn show_proposal(&self, id: ProposalId) -> Result<Value> {
            Ok(json!({"proposal": id}))
        }
        fn read(&mut self, read: &QueueRead) -> Result<Value> {
            Ok(json!({"read": format!("{read:?}")}))
        }
        /// Kept as asked, after its authorization.
        fn command(&mut self, command: &QueueCommand) -> Result<Value> {
            let value = json!({"command": command.use_case().as_str()});
            self.log.lock().unwrap().push(value.clone());
            Ok(value)
        }
        /// A session `<kind>:<id>:<name>` is registered to that target,
        /// any other to none; one named `taken` has steps of another target.
        fn session_owner(&self, session_id: &str) -> Result<Option<StepTarget>> {
            let mut parts = session_id.splitn(3, ':');
            Ok(match (parts.next(), parts.next(), parts.next()) {
                (Some(kind), Some(id), Some(_)) => Some(StepTarget {
                    kind: crate::domain::session_step::TargetKind::parse(kind).unwrap(),
                    id: id.to_owned(),
                }),
                _ => None,
            })
        }
        fn record_report(&mut self, report: &TelemetryReport) -> Result<ReportRecorded> {
            if report.session_id.ends_with(":taken") {
                return Err(StepError::SessionOfAnotherTarget.into());
            }
            self.log.lock().unwrap().push(json!({
                "steps": report.steps.len(), "resources": report.resources.len(),
                "session": report.session_id,
                "target": format!("{}:{}", report.target.kind.as_str(), report.target.id),
            }));
            let added = |added| crate::domain::session_step::StepsRecorded {
                added,
                duplicates: 0,
            };
            Ok(ReportRecorded {
                steps: added(report.steps.len()),
                resources: added(report.resources.len()),
            })
        }
        /// A target of id 9 ended; every other is still open.
        fn telemetry_target(&self, target: &StepTarget) -> Result<TargetFacts> {
            Ok(if target.id == "9" {
                TargetFacts::Ended
            } else {
                TargetFacts::OpenPlanner
            })
        }
        fn register_session(&mut self, _: &StepTarget, _: &str, _: i64) -> Result<()> {
            Ok(())
        }
        fn supervisor_store(
            &mut self,
        ) -> &mut dyn crate::application::supervisor_service::SupervisorStore {
            unimplemented!("these tests send no supervisor's operation")
        }
        fn issuing(
            &mut self,
            _: &crate::domain::queue_service::IssueHeld,
            _: i64,
            _: &mut dyn FnMut(&dyn IssueFacts) -> Result<Value>,
        ) -> Result<Value> {
            unimplemented!("these tests issue no token")
        }
        fn begin_landing(
            &mut self,
            _: &RunId,
            _: &crate::domain::LeaseToken,
            _: &crate::domain::CommitSha,
            _: bool,
            _: SlotHolder,
            _: &mut dyn FnMut(crate::domain::EventId) -> Result<Value>,
        ) -> Result<(crate::domain::TaskRun, Value)> {
            unimplemented!("these tests send no supervisor's operation")
        }
        /// A session wrapper's operation is kept in the log as it came.
        fn session(&mut self, op: &crate::application::session::SessionOp) -> Result<Value> {
            self.log
                .lock()
                .unwrap()
                .push(json!({"session": serde_json::to_value(op)?}));
            Ok(json!({"value": null}))
        }
        fn watch_look(
            &self,
            after: Option<EventId>,
            role: Option<SessionRole>,
        ) -> Result<crate::application::watch::WatchLook> {
            self.log
                .lock()
                .unwrap()
                .push(json!({"watch_look": after, "role": role}));
            Ok(crate::application::watch::WatchLook {
                after: after.unwrap_or(EventId::new(0)),
                events: Vec::new(),
                cursor: after.unwrap_or(EventId::new(0)),
                woken: false,
                opened: Vec::new(),
                pulses: json!([]),
                supervisors: json!([]),
                binding: None,
            })
        }
        fn write(&mut self, write: &QueueWrite, actor: &ActorContext) -> Result<Value> {
            self.log
                .lock()
                .unwrap()
                .push(json!({"write": write.use_case(), "actor": actor.actor_id()}));
            Ok(json!({"written": write.use_case()}))
        }
    }

    struct Backend {
        status: RunStatus,
        log: std::sync::Arc<Mutex<Vec<Value>>>,
        /// The queue is at an older schema: as the SQLite backend, it
        /// opens for a read (from a copy) and refuses to open to write or
        /// for a watch's look.
        older: bool,
        /// The connections opened, in order: `write`, `read` or `watch`.
        opened: Mutex<Vec<&'static str>>,
    }

    impl Backend {
        fn opening(&self, connection: &'static str) -> Result<Box<dyn ServiceQueue>> {
            self.opened.lock().unwrap().push(connection);
            if self.older && connection != "read" {
                anyhow::bail!("queue schema version 1 is older than this binary's schema");
            }
            Ok(Box::new(Queue {
                status: self.status,
                log: self.log.clone(),
                denials: RefCell::default(),
            }))
        }
    }

    impl ServiceBackend for Backend {
        fn principal(&self, token: &str) -> Result<Option<Principal>> {
            let run = RunId::new("r1")?;
            Ok(match token {
                "worker" => Some(Principal::worker(&run, TaskId::new(1))),
                "review" => Some(Principal::of(&ActorContext::review_job(&run, 1))),
                "observer" => Some(Principal::of(&ActorContext::instance(
                    ActorRole::Observer,
                    "s1",
                ))),
                "recovery" => Some(Principal::of(&ActorContext::recovery_job(&run, "a", 1))),
                "plan-review" => Some(Principal::of(&ActorContext::plan_review_job(4, 1))),
                "goal-review" => Some(Principal::of(&ActorContext::goal_review_job(5, 1))),
                "throughput-review" => Some(Principal::of(&ActorContext::throughput_review_job(
                    "hourly", "h",
                ))),
                "planner" => Some(Principal::planner(3)),
                "planner-3" => Some(Principal::planner(3)),
                "planner-closed" => Some(Principal::planner(9)),
                "inbox" => Some(Principal::inbox(SessionProcess {
                    pid: 7,
                    start: "then".into(),
                })),
                // A session whose process another took since.
                "inbox-ended" => Some(Principal::inbox(SessionProcess {
                    pid: 7,
                    start: "earlier".into(),
                })),
                // A session whose process lives and whose start cannot be
                // read.
                "inbox-unread" => Some(Principal::inbox(SessionProcess {
                    pid: 8,
                    start: "then".into(),
                })),
                // A record no issue makes: an inbox of no session.
                "inbox-unbound" => {
                    Some(Principal::of(&ActorContext::new(ActorRole::Inbox, "inbox")))
                }
                "telemetry" => Some(Principal::telemetry(
                    &StepTarget::run(&run),
                    Some(TaskId::new(1)),
                )),
                "telemetry-planner" => Some(Principal::telemetry(
                    &StepTarget {
                        kind: crate::domain::session_step::TargetKind::Planner,
                        id: "3".into(),
                    },
                    None,
                )),
                "telemetry-ended" => Some(Principal::telemetry(
                    &StepTarget {
                        kind: crate::domain::session_step::TargetKind::PlanReview,
                        id: "9".into(),
                    },
                    None,
                )),
                "wrapper" => Some(Principal::wrapper(
                    &StepTarget::run(&run),
                    Some(TaskId::new(1)),
                )),
                "wrapper-planner" => Some(Principal::wrapper(
                    &StepTarget::of(crate::domain::session_step::TargetKind::Planner, 3),
                    None,
                )),
                "supervisor" => Some(Principal::supervisor(
                    &crate::domain::LeaseToken::new("registration"),
                    1,
                )),
                "broken" => return Err(anyhow!("unreadable")),
                _ => None,
            })
        }
        fn open(&self, _: &ActorContext) -> Result<Box<dyn ServiceQueue>> {
            self.opening("write")
        }
        fn open_read(&self, _: &ActorContext) -> Result<Box<dyn ServiceQueue>> {
            self.opening("read")
        }
        fn open_watch(&self, _: &ActorContext) -> Result<Box<dyn ServiceQueue>> {
            self.opening("watch")
        }
        fn record_unauthenticated(&self, payload: Value) -> Result<()> {
            self.log.lock().unwrap().push(payload);
            Ok(())
        }
        fn now_ms(&self) -> i64 {
            0
        }
        fn db(&self) -> &std::path::Path {
            std::path::Path::new("queue.db")
        }
        fn access(&self) -> &dyn ServiceAccess {
            unimplemented!("these tests issue no token")
        }
        fn telemetry(&self) -> &dyn TelemetryTokens {
            unimplemented!("these tests issue no token")
        }
        fn principals(&self) -> Result<Vec<Principal>> {
            Ok(Vec::new())
        }
        fn serves(&self, db: &std::path::Path) -> bool {
            db == std::path::Path::new("/q/queue.db")
        }
        fn session_alive(&self, session: &crate::domain::queue_service::SessionProcess) -> bool {
            session.pid == 7 && session.start == "then"
        }
        fn alive(&self, _: u32) -> bool {
            false
        }
        fn session_ended(&self, session: &crate::domain::queue_service::SessionProcess) -> bool {
            // Pid 8 lives, and its start cannot be read.
            session.pid != 8 && !self.session_alive(session)
        }
        fn process_of(&self, _: u32) -> Option<crate::domain::queue_service::SessionProcess> {
            None
        }
        fn started_by_ended(&self, pid: u32, _: i64) -> bool {
            pid != 8
        }
        fn issue_as(
            &self,
            principal: &Principal,
            _: &crate::domain::queue_service::TokenIssuer,
        ) -> Result<std::path::PathBuf> {
            Ok(self.access().issue(self.db(), principal)?.file)
        }
        fn held(&self, _: &str) -> Result<Option<crate::domain::queue_service::HeldToken>> {
            Ok(None)
        }
        fn issue_telemetry_as(
            &self,
            target: &crate::domain::session_step::StepTarget,
            task: Option<TaskId>,
            issued_under: Option<&crate::domain::LeaseToken>,
            _: &crate::domain::queue_service::TokenIssuer,
        ) -> Result<std::path::PathBuf> {
            self.telemetry()
                .issue_telemetry(self.db(), target, task, issued_under)
        }
    }

    fn backend(status: RunStatus) -> Backend {
        Backend {
            status,
            log: std::sync::Arc::default(),
            older: false,
            opened: Mutex::default(),
        }
    }

    /// The issue's facts with every registration kept and no job's
    /// records.
    struct Kept;

    impl IssueFacts for Kept {
        fn registered(&self, _: &crate::domain::LeaseToken) -> bool {
            true
        }
        fn job(
            &self,
            _: &crate::domain::queue_service::HeldToken,
            _: Option<&crate::domain::queue_service::TokenIssuer>,
        ) -> crate::domain::queue_service::JobState {
            crate::domain::queue_service::JobState::MayRun
        }
        fn target_start(
            &self,
            _: &crate::domain::session_step::StepTarget,
        ) -> Result<Option<crate::domain::queue_service::TargetStart>> {
            Ok(None)
        }
    }

    /// A parent kept with a token is gone only once its process is known
    /// to have ended (ADR-t3102-1 decision 2): one whose pid lives and
    /// whose start cannot be read is still there, so another parent
    /// neither takes over nor revokes what it asked for.
    #[test]
    fn a_parent_whose_start_cannot_be_read_is_still_there() {
        use crate::domain::queue_service::{
            HeldToken, JobState, Reissue, TokenIssuer, reissue, revoke_refusal,
        };
        let backend = backend(RunStatus::Running);
        let there = parent_there(&backend, &Kept);
        let inbox = |pid, start: &str| {
            Principal::inbox(SessionProcess {
                pid,
                start: start.into(),
            })
        };
        let kept = |pid, start: &str| {
            TokenIssuer::of(
                &inbox(pid, start),
                Some(SessionProcess {
                    pid,
                    start: start.into(),
                }),
            )
        };
        let asking = TokenIssuer::of(&Principal::user(), None);
        let planner = Principal::planner(3);
        // Pid 8 lives with a start that cannot be read; pid 9 is gone.
        let unread = kept(8, "then");
        assert!(there(&unread));
        assert!(matches!(
            reissue(
                &planner,
                &asking,
                Some(&Some(unread.clone())),
                there,
                || JobState::MayRun
            ),
            Reissue::Refused(_)
        ));
        let held = HeldToken {
            principal: planner.clone(),
            issued_by: Some(unread),
            issued_at: 0,
        };
        assert!(revoke_refusal(&held, &asking.parent, there, || JobState::MayRun).is_some());
        let gone = kept(9, "then");
        assert!(!there(&gone));
        assert_eq!(
            reissue(&planner, &asking, Some(&Some(gone)), there, || {
                JobState::MayRun
            }),
            Reissue::Issue
        );
    }

    fn request(token: Option<&str>, use_case: UseCase, params: Value) -> ServiceRequest {
        ServiceRequest {
            api_version: API_VERSION,
            token: token.map(str::to_owned),
            env_role: None,
            queue: None,
            binding: None,
            use_case,
            params,
        }
    }

    fn code(response: &ServiceResponse) -> Option<ServiceErrorCode> {
        response.error.as_ref().map(|error| error.code)
    }

    /// A [`ServiceStatusSource`] whose look finds `state` and whose read
    /// comes to `read`, counting the reads.
    struct FakeStatus {
        state: ServiceState,
        read: ServiceStatusRead,
        outage: Option<crate::domain::queue_service::ServiceOutage>,
        reads: RefCell<usize>,
    }

    impl ServiceStatusSource for FakeStatus {
        fn probe(&self) -> ServiceProbe {
            ServiceProbe {
                state: self.state,
                socket: "/q/service/queue.sock".into(),
                pid: Some(7),
                build: Some("b".to_owned()),
                api_version: Some(API_VERSION),
                min_api_version: None,
                build_matches: Some(true),
                started_at: Some(1),
                error: None,
            }
        }
        fn read(&self) -> ServiceStatusRead {
            *self.reads.borrow_mut() += 1;
            self.read.clone()
        }
        fn recorded_outage(&self) -> Option<crate::domain::queue_service::ServiceOutage> {
            self.outage.clone()
        }
    }

    /// The host's `service status` asks only a service its look finds
    /// running; its answer is the attention, a refusal the attention's
    /// `error`, and no answer the outage recorded with the look turned
    /// `unreachable`, as is a service the look does not find running.
    #[test]
    fn host_status_reads_a_running_service_and_falls_back_to_the_outage() {
        let outage = crate::domain::queue_service::ServiceOutage {
            since: 5,
            reason: "unreachable".to_owned(),
            message: "m".to_owned(),
            supervisor: None,
            pushed: true,
        };
        let fake = |state, read| FakeStatus {
            state,
            read,
            outage: Some(outage.clone()),
            reads: RefCell::new(0),
        };
        let answered = fake(
            ServiceState::Running,
            ServiceStatusRead::Answered(json!({"attention": true})),
        );
        let view = host_status(&answered);
        assert_eq!(view["state"], "running");
        assert_eq!(view["attention"], true);
        assert!(view.get("outage").is_none(), "{view}");

        let refused = fake(
            ServiceState::Running,
            ServiceStatusRead::Refused("bad_request: unknown use case".to_owned()),
        );
        let view = host_status(&refused);
        assert_eq!(view["state"], "running");
        assert_eq!(
            view["attention"],
            json!({"error": "bad_request: unknown use case"})
        );
        assert!(view.get("outage").is_none(), "{view}");

        let gone = fake(
            ServiceState::Running,
            ServiceStatusRead::Unreached("the socket refused".to_owned()),
        );
        let view = host_status(&gone);
        assert_eq!(view["state"], "unreachable");
        assert_eq!(view["error"], "the socket refused");
        assert_eq!(view["attention"], Value::Null);
        assert_eq!(view["outage"], json!(outage));
        assert_eq!(view["next"], UNREACHED_NEXT);

        for state in [ServiceState::Stopped, ServiceState::Unreachable] {
            let down = fake(
                state,
                ServiceStatusRead::Answered(json!({"attention": true})),
            );
            let view = host_status(&down);
            assert_eq!(*down.reads.borrow(), 0, "{state:?}");
            assert_eq!(view["state"], json!(state));
            assert_eq!(view["attention"], Value::Null);
            assert_eq!(view["outage"], json!(outage));
        }
    }

    /// `service status` of a service that answers is the look and the
    /// attention it read; of one that does not, the look, no attention
    /// (the queue is not opened), the outage recorded or `null`, and what
    /// to do next.
    #[test]
    fn service_status_says_the_attention_or_the_outage_and_what_to_do() {
        let probe = |state| ServiceProbe {
            state,
            socket: "/q/service/queue.sock".into(),
            pid: Some(7),
            build: Some("b".to_owned()),
            api_version: Some(API_VERSION),
            min_api_version: None,
            build_matches: Some(true),
            started_at: Some(1),
            error: None,
        };
        let answered = view_with(&probe(ServiceState::Running), json!(true));
        assert_eq!(answered["state"], "running");
        assert_eq!(answered["attention"], true);
        assert_eq!(answered["client_api_version"], API_VERSION);
        assert!(answered.get("outage").is_none() && answered.get("next").is_none());
        let none = unreached_view(&probe(ServiceState::Stopped), None);
        assert_eq!(none["state"], "stopped");
        assert_eq!(none["attention"], Value::Null);
        assert_eq!(none["outage"], Value::Null);
        assert_eq!(none["next"], UNREACHED_NEXT);
        let outage = crate::domain::queue_service::ServiceOutage {
            since: 5,
            reason: "start_failed".to_owned(),
            message: "m".to_owned(),
            supervisor: None,
            pushed: false,
        };
        let recorded = unreached_view(&probe(ServiceState::Unreachable), Some(&outage));
        assert_eq!(recorded["state"], "unreachable");
        assert_eq!(recorded["outage"], json!(outage));
        assert_eq!(recorded["next"], UNREACHED_NEXT);
    }

    #[test]
    fn hello_needs_no_token_and_names_the_build_and_the_versions() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "1.0.0",
            pid: 42,
        };
        let response = service.handle(&request(None, UseCase::Hello, Value::Null));
        assert!(response.ok);
        let result = response.result.unwrap();
        assert_eq!(result["build"], "1.0.0");
        assert_eq!(result["pid"], 42);
        assert_eq!(result["api_version"], API_VERSION);
        // A caller of another version is refused before anything else.
        let mut newer = request(Some("worker"), UseCase::Show, json!({"id": 1}));
        newer.api_version = API_VERSION + 1;
        let response = service.handle(&newer);
        assert_eq!(code(&response), Some(ServiceErrorCode::ApiVersionMismatch));
        assert_eq!(response.api_version, API_VERSION);
        assert!(backend.log.lock().unwrap().is_empty());
        // A worker of the oldest version answered is answered.
        let mut older = request(Some("worker"), UseCase::Show, json!({"id": 1}));
        older.api_version = MIN_API_VERSION;
        assert!(service.handle(&older).ok);
    }

    /// The broker's own requests, sent as the user with no token: the
    /// run's status, the queue's binding, and the record of only its two
    /// kinds, which the supervisor may record too and no AI actor may.
    #[test]
    fn the_broker_reads_and_records_as_the_user_and_no_ai_actor_records_for_it() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let user =
            |use_case, params| service.handle_from(&request(None, use_case, params), Peer::Owner);
        let status = user(UseCase::BrokerRunStatus, json!({"run": "r1"}));
        assert_eq!(
            status.result,
            Some(json!({"status": "running"})),
            "{status:?}"
        );
        let missing = user(UseCase::BrokerRunStatus, json!({"run": "r9"}));
        assert_eq!(missing.result, Some(json!({"status": null})));
        let binding = user(UseCase::BrokerBinding, json!({}));
        assert_eq!(binding.result, Some(json!({"binding": "/repo/.git"})));
        assert_eq!(
            code(&user(UseCase::BrokerBinding, json!({"more": 1}))),
            Some(ServiceErrorCode::BadRequest)
        );
        backend.log.lock().unwrap().clear();

        let record =
            |kind: &str| json!({"kind": kind, "payload": {"broker": {"pid": 42, "build": "b"}}});
        for kind in ["queue_broker_denied", "queue_broker_tokens_swept"] {
            let response = user(UseCase::BrokerRecord, record(kind));
            assert!(response.ok, "{response:?}");
            let recorded = backend.log.lock().unwrap().pop().unwrap();
            assert_eq!(recorded["broker_event"], kind);
            assert_eq!(recorded["payload"]["broker"]["pid"], 42);
        }
        // Another kind is no record of the broker's.
        let other = user(UseCase::BrokerRecord, record("queue_broker_down"));
        assert_eq!(code(&other), Some(ServiceErrorCode::BadRequest));
        assert!(backend.log.lock().unwrap().is_empty());
        // The supervisor records its sweeps the same way.
        let supervisor = service.handle(&request(
            Some("supervisor"),
            UseCase::BrokerRecord,
            record("queue_broker_tokens_swept"),
        ));
        assert!(supervisor.ok, "{supervisor:?}");
        backend.log.lock().unwrap().clear();
        // An AI actor's is refused as `authorization_denied`, recorded, and
        // writes nothing of the broker's.
        for token in [
            "worker",
            "review",
            "recovery",
            "observer",
            "plan-review",
            "planner",
            "inbox",
            "wrapper",
        ] {
            let response = service.handle(&request(
                Some(token),
                UseCase::BrokerRecord,
                record("queue_broker_denied"),
            ));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{token}"
            );
            let log = std::mem::take(&mut *backend.log.lock().unwrap());
            assert!(
                log.iter().all(|entry| entry.get("broker_event").is_none()),
                "{token}: {log:?}"
            );
            assert!(
                log.iter()
                    .any(|entry| entry.to_string().contains("queue_broker.record")),
                "{token}: {log:?}"
            );
        }
    }

    #[test]
    fn no_token_is_the_user_only_from_the_owner_s_peer_with_no_role_named() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let note = || request(None, UseCase::Note, json!({"task": 1, "text": "x"}));
        // The owner's peer naming no role: the user, whose note is its own.
        let response = service.handle_from(&note(), Peer::Owner);
        assert_ne!(code(&response), Some(ServiceErrorCode::Unauthenticated));
        assert_eq!(
            backend.log.lock().unwrap().pop().unwrap(),
            json!({"note": "x", "by": "human"})
        );
        // A role named with no token is refused, from the owner's peer too,
        // and so is a peer that is not the owner's.
        let mut named = note();
        named.env_role = Some("worker".to_owned());
        for (request, peer) in [(named, Peer::Owner), (note(), Peer::Other)] {
            let response = service.handle_from(&request, peer);
            assert_eq!(code(&response), Some(ServiceErrorCode::Unauthenticated));
            let recorded = backend.log.lock().unwrap().pop().unwrap();
            assert_eq!(
                recorded,
                json!({"use_case": "note", "reason": "missing_token"})
            );
        }
        // An empty token is no user's either.
        let response = service.handle_from(
            &request(Some(""), UseCase::Note, json!({"task": 1, "text": "x"})),
            Peer::Owner,
        );
        assert_eq!(code(&response), Some(ServiceErrorCode::Unauthenticated));
    }

    #[test]
    fn an_inbox_or_a_planner_with_no_token_is_told_why_and_what_to_do_next() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        for (role, next) in [
            (
                "inbox",
                "open the inbox again (`dagq inbox` in a terminal with no DAGQ_ROLE)",
            ),
            (
                "planner",
                "wait for it to close (the next planner is given one)",
            ),
        ] {
            let mut tokenless = request(None, UseCase::Show, json!({"id": 1}));
            tokenless.env_role = Some(role.to_owned());
            let response = service.handle_from(&tokenless, Peer::Owner);
            let error = response.error.unwrap();
            assert_eq!(error.code, ServiceErrorCode::Unauthenticated);
            assert!(
                error.message.contains("carries no token"),
                "{}",
                error.message
            );
            assert!(error.message.ends_with(next), "{role}: {}", error.message);
            assert_eq!(
                backend.log.lock().unwrap().pop().unwrap(),
                json!({"use_case": "show", "reason": "missing_token"})
            );
        }
        // Another role is only told that it carries no token.
        let mut worker = request(None, UseCase::Show, json!({"id": 1}));
        worker.env_role = Some("worker".to_owned());
        let message = service
            .handle_from(&worker, Peer::Owner)
            .error
            .unwrap()
            .message;
        assert!(message.ends_with("never taken for the user's"), "{message}");
    }

    #[test]
    fn the_user_the_inbox_and_a_planner_are_allowed_what_the_command_line_allows() {
        use crate::domain::authorization::grants;
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        // Each use case and the capability the command line asks for it.
        let cases = [
            (UseCase::Show, json!({"id": 1}), Capability::QueueRead),
            (UseCase::List, json!({}), Capability::QueueRead),
            (
                UseCase::Requests,
                json!({"all": true}),
                Capability::QueueRead,
            ),
            (UseCase::Replans, json!({"id": 3}), Capability::QueueRead),
            (UseCase::CiFailures, json!({"task": 1}), Capability::CiRead),
            (
                UseCase::Planners,
                json!({"all": true}),
                Capability::QueueRead,
            ),
            (UseCase::ServiceStatus, json!({}), Capability::QueueRead),
            (UseCase::StatsInputs, json!({}), Capability::QueueRead),
            (
                UseCase::WatchPoll,
                json!({"role": "inbox"}),
                Capability::QueueWatch,
            ),
            (
                UseCase::Note,
                json!({"task": 1, "text": "x"}),
                Capability::NoteWrite,
            ),
            (
                UseCase::FindingRecord,
                json!({"kind": "k", "task": 1, "summary": "s"}),
                Capability::FindingRecord,
            ),
            (
                UseCase::FindingResolve,
                json!({"id": 1, "reason": "r"}),
                Capability::FindingResolve,
            ),
            (
                UseCase::ReplanPropose,
                json!({"run": "r1", "reason": "r"}),
                Capability::ReplanPropose,
            ),
            (UseCase::Mark, json!({"label": "m"}), Capability::MarkWrite),
        ];
        let callers = [
            (None, ActorRole::User),
            (Some("inbox"), ActorRole::Inbox),
            (Some("planner-3"), ActorRole::Planner),
            (Some("worker"), ActorRole::Worker),
        ];
        let mut refused = 0;
        for (token, role) in callers {
            for (use_case, params, capability) in &cases {
                let response =
                    service.handle_from(&request(token, *use_case, params.clone()), Peer::Owner);
                let denied = code(&response) == Some(ServiceErrorCode::AuthorizationDenied);
                assert_eq!(
                    denied,
                    !grants(role).contains(capability),
                    "{} {}: {response:?}",
                    role.as_str(),
                    use_case.as_str()
                );
                if denied {
                    refused += 1;
                    // Refused as the command line refuses it, and recorded.
                    let denial = response.error.unwrap().denied.unwrap();
                    assert_eq!(denial.role, role.as_str());
                    assert_eq!(denial.capability, capability.as_str());
                    let log = backend.log.lock().unwrap();
                    let recorded = log.last().unwrap();
                    assert_eq!(recorded["role"], role.as_str(), "{recorded}");
                    assert_eq!(recorded["capability"], capability.as_str(), "{recorded}");
                }
            }
        }
        // The planner's finding_record and replan_propose, the user's and
        // the inbox's replan_propose, and the worker's watch.
        assert!(refused >= 5, "{refused}");
        // A planner's token ends with its planner.
        let response = service.handle_from(
            &request(Some("planner-closed"), UseCase::Show, json!({"id": 1})),
            Peer::Owner,
        );
        assert_eq!(code(&response), Some(ServiceErrorCode::Unauthenticated));
        let recorded = backend.log.lock().unwrap().pop().unwrap();
        assert_eq!(
            recorded,
            json!({"use_case": "show", "reason": "target_ended"})
        );
    }

    /// A planning command reaches the queue's write as its caller's actor,
    /// which the application authorizes as the command line does
    /// ([`super::super::queue_writes`]'s tests); params its command has not
    /// are a bad request, and the executor's telemetry principal is refused
    /// before they are read.
    #[test]
    fn a_planning_command_runs_as_its_caller_and_params_it_has_not_are_bad() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        for (token, actor) in [
            (None, "user"),
            (Some("planner-3"), "planner:3"),
            (Some("worker"), "worker:r1"),
        ] {
            let response = service.handle_from(
                &request(token, UseCase::Add, json!({"title": "t"})),
                Peer::Owner,
            );
            assert_eq!(response.result, Some(json!({"written": "add"})), "{actor}");
            let log = backend.log.lock().unwrap();
            assert_eq!(
                log.last().unwrap(),
                &json!({"write": "add", "actor": actor})
            );
        }
        let response = service.handle_from(
            &request(None, UseCase::Draft, json!({"id": 1, "full": true})),
            Peer::Owner,
        );
        assert_eq!(code(&response), Some(ServiceErrorCode::BadRequest));
        let response = service.handle(&request(
            Some("telemetry"),
            UseCase::Submit,
            json!({"nothing": "read"}),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::AuthorizationDenied));
    }

    /// The host's dialogue and operations (ADR-t3102-2 decision 6 (4))
    /// are asked of the policy before they run as the command line asks
    /// it, a refusal recorded and nothing run; the ones the application
    /// authorizes as it runs them reach it for every principal, and a
    /// relative path is a bad request before anything is asked.
    #[test]
    fn the_host_s_commands_are_authorized_before_they_run_as_the_command_line_does() {
        use crate::domain::authorization::grants;
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let gated = [
            (
                UseCase::Rebind,
                json!({"git_common_dir": "/repo/.git"}),
                Capability::QueueAdmin,
            ),
            (
                UseCase::Recover,
                json!({"run": "r1"}),
                Capability::RunRecover,
            ),
            (
                UseCase::RunLog,
                json!({"run": "r1"}),
                Capability::ScreenRead,
            ),
            (
                UseCase::RunSend,
                json!({"run": "1"}),
                Capability::ScreenSend,
            ),
            (
                UseCase::PlannerScreen,
                json!({"planner": 3}),
                Capability::ScreenRead,
            ),
            (
                UseCase::PlannerRequest,
                json!({"planner": 3, "text": "t"}),
                Capability::PlannerRequest,
            ),
            (
                UseCase::Report,
                json!({"period": "day", "print": true}),
                Capability::ExportFile,
            ),
            (
                UseCase::ReportInputs,
                json!({"period": "day"}),
                Capability::ExportFile,
            ),
            (UseCase::GraphExport, json!({}), Capability::ExportFile),
        ];
        let callers = [
            (None, ActorRole::User),
            (Some("inbox"), ActorRole::Inbox),
            (Some("planner-3"), ActorRole::Planner),
            (Some("worker"), ActorRole::Worker),
        ];
        let mut refused = 0;
        for (token, role) in callers {
            for (use_case, params, capability) in &gated {
                let response =
                    service.handle_from(&request(token, *use_case, params.clone()), Peer::Owner);
                let recorded = backend.log.lock().unwrap().last().cloned().unwrap();
                if grants(role).contains(capability) {
                    assert!(
                        response.ok,
                        "{} {}: {response:?}",
                        role.as_str(),
                        use_case.as_str()
                    );
                    assert_eq!(recorded, json!({"command": use_case.as_str()}));
                } else {
                    refused += 1;
                    assert_eq!(
                        code(&response),
                        Some(ServiceErrorCode::AuthorizationDenied),
                        "{} {}",
                        role.as_str(),
                        use_case.as_str()
                    );
                    assert_eq!(recorded["capability"], capability.as_str(), "{recorded}");
                }
            }
            // The application authorizes these as it runs them.
            for (use_case, params) in [
                (UseCase::Answer, json!({"id": 1, "text": "yes"})),
                (UseCase::AskClose, json!({"id": 1})),
                (UseCase::Hold, json!({"target": "1", "reason": "r"})),
                (UseCase::Release, json!({"target": "r1"})),
                (UseCase::ReplanRequest, json!({"run": "r1", "reason": "r"})),
            ] {
                let response = service.handle_from(&request(token, use_case, params), Peer::Owner);
                assert!(
                    response.ok,
                    "{} {}: {response:?}",
                    role.as_str(),
                    use_case.as_str()
                );
            }
        }
        // The worker's every one, the planner's operations.
        assert!(refused >= gated.len(), "{refused}");
        let response = service.handle_from(
            &request(
                None,
                UseCase::Rebind,
                json!({"git_common_dir": "checkout/.git"}),
            ),
            Peer::Owner,
        );
        assert_eq!(code(&response), Some(ServiceErrorCode::BadRequest));
    }

    /// A planner asks on its planning request through the service as the
    /// command line lets it (ADR-t3102-1 decision 2): only on a request
    /// whose planner it is, the other refused and recorded.
    #[test]
    fn a_planner_asks_on_its_own_request_and_is_refused_another_planners() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let ask = |on: i64| {
            let params = json!({"kind": "planner_question", "question": "which?",
                                "because": "scope", "request_id": on});
            service.handle_from(
                &request(Some("planner-3"), UseCase::Ask, params),
                Peer::Owner,
            )
        };
        let response = ask(7);
        assert!(response.ok, "{response:?}");
        assert_eq!(
            backend.log.lock().unwrap().last().unwrap(),
            &json!({"opened": "planner_question", "run": null, "by": "planner", "request": 7})
        );
        let response = ask(8);
        assert_eq!(
            code(&response),
            Some(ServiceErrorCode::AuthorizationDenied),
            "{response:?}"
        );
        let recorded = backend.log.lock().unwrap().last().unwrap().clone();
        assert_eq!(recorded["role"], "planner", "{recorded}");
        assert_eq!(recorded["capability"], "ask.open", "{recorded}");
        assert_eq!(recorded["resource"]["request"], 8, "{recorded}");
        assert_eq!(recorded["resource"]["planner"], 4, "{recorded}");
    }

    #[test]
    fn a_request_naming_another_queue_is_refused_before_anything_and_an_ended_session_s_token_too()
    {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let named = |db: &str, token: Option<&str>| {
            let mut request = request(token, UseCase::Show, json!({"id": 1}));
            request.queue = Some(std::path::PathBuf::from(db));
            request
        };
        // Another queue's DB, or one that is not there: refused before the
        // token is read, the use case run or anything recorded.
        for db in ["/q/other.db", "/q/nonexistent.db"] {
            let response = service.handle_from(&named(db, Some("worker")), Peer::Owner);
            assert_eq!(code(&response), Some(ServiceErrorCode::WrongQueue), "{db}");
            assert!(response.error.unwrap().message.contains(db));
        }
        assert!(backend.log.lock().unwrap().is_empty());
        // Its own is served, by the user and the inbox of a live session.
        for token in [None, Some("inbox")] {
            let response = service.handle_from(&named("/q/queue.db", token), Peer::Owner);
            assert!(response.ok, "{token:?}: {response:?}");
        }
        // A session whose process ended (or another took its pid) ends its
        // inbox's token, which says to open the inbox again.
        // So does one whose start cannot be read: the token is taken only
        // while its session is known to run.
        for token in ["inbox-ended", "inbox-unread"] {
            let response = service.handle_from(&named("/q/queue.db", Some(token)), Peer::Owner);
            let error = response.error.unwrap();
            assert_eq!(error.code, ServiceErrorCode::Unauthenticated, "{token}");
            assert!(error.message.contains("dagq inbox"), "{}", error.message);
            assert_eq!(
                backend.log.lock().unwrap().pop().unwrap(),
                json!({"use_case": "show", "reason": "session_ended"})
            );
        }
        let unbound =
            service.handle_from(&named("/q/queue.db", Some("inbox-unbound")), Peer::Owner);
        assert_eq!(code(&unbound), Some(ServiceErrorCode::Unauthenticated));
        assert_eq!(
            backend.log.lock().unwrap().pop().unwrap(),
            json!({"use_case": "show", "reason": "unknown_token"})
        );
    }

    /// ADR-0045 decision 18: the reads and a watch's look open the queue
    /// read-only, the supervisor's requests are answered apart, and the
    /// rest open it to write.
    #[test]
    fn reads_and_a_watch_s_look_open_the_queue_read_only() {
        for (use_case, params) in [
            (UseCase::Show, json!({"id": 1})),
            (UseCase::ProposalList, json!({})),
            (UseCase::ProposalShow, json!({"id": 1})),
            (UseCase::Status, json!({})),
            (UseCase::Events, json!({})),
            (UseCase::Requests, json!({})),
            (UseCase::AgentResults, Value::Null),
            (UseCase::AgentResult, json!({"id": 1})),
            (UseCase::AgentSets, Value::Null),
            (UseCase::AgentSet, json!({"id": 1})),
        ] {
            assert_eq!(
                connection(use_case, &params),
                Connection::Read,
                "{use_case:?}"
            );
        }
        assert_eq!(
            connection(UseCase::WatchPoll, &json!({})),
            Connection::Watch
        );
        assert_eq!(
            connection(UseCase::Supervisor, &json!({"op": "x"})),
            Connection::Supervisor
        );
        for (use_case, params) in [
            (UseCase::Note, json!({"task": 1, "text": "x"})),
            (UseCase::Ask, json!({})),
            (UseCase::FindingRecord, json!({})),
            (UseCase::AgentEval, json!({})),
            (UseCase::AgentCases, json!({})),
            (UseCase::AgentDecide, json!({})),
        ] {
            assert_eq!(
                connection(use_case, &params),
                Connection::Write,
                "{use_case:?}"
            );
        }
    }

    /// A token's run or target is read on the connection its use case runs
    /// on, opened once and kept for it: a worker's and a planner's reads of
    /// a queue at an older schema are answered from the read's copy, as
    /// the user's are, and what writes to it or watches it is refused as
    /// the command line refuses it.
    #[test]
    fn a_token_s_run_or_target_is_read_on_the_connection_its_use_case_runs_on() {
        let backend = Backend {
            older: true,
            ..backend(RunStatus::Running)
        };
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let opened = |response: &ServiceResponse| {
            let opened = std::mem::take(&mut *backend.opened.lock().unwrap());
            (response.ok, code(response), opened)
        };
        for token in ["worker", "planner"] {
            for (use_case, params) in [
                (UseCase::Show, json!({"id": 1})),
                (UseCase::Status, json!({})),
            ] {
                let response = service.handle(&request(Some(token), use_case, params));
                assert_eq!(
                    opened(&response),
                    (true, None, vec!["read"]),
                    "{token} {use_case:?}"
                );
            }
        }
        let response = service.handle_from(
            &request(None, UseCase::AgentResults, Value::Null),
            Peer::Owner,
        );
        assert_eq!(opened(&response), (true, None, vec!["read"]));
        // The check fails on the connection the use case would, and
        // nothing else is opened.
        let note = service.handle(&request(
            Some("planner"),
            UseCase::Note,
            json!({"task": 1, "text": "x"}),
        ));
        assert_eq!(
            opened(&note),
            (false, Some(ServiceErrorCode::Failed), vec!["write"])
        );
        assert!(
            note.error
                .unwrap()
                .message
                .contains("older than this binary"),
            "the schema's refusal is said"
        );
        let watch = service.handle(&request(
            Some("planner"),
            UseCase::WatchPoll,
            json!({"role": "planner"}),
        ));
        assert_eq!(
            opened(&watch),
            (false, Some(ServiceErrorCode::Failed), vec!["watch"])
        );
        // A refusal of the token is still decided on the same read.
        let ended = Backend {
            status: RunStatus::Integrated,
            ..backend
        };
        let service = QueueService {
            backend: &ended,
            build: "b",
            pid: 1,
        };
        let response = service.handle(&request(Some("worker"), UseCase::Show, json!({"id": 1})));
        assert_eq!(code(&response), Some(ServiceErrorCode::Unauthenticated));
        assert_eq!(*ended.opened.lock().unwrap(), ["read"]);
        assert_eq!(
            ended.log.lock().unwrap().pop().unwrap(),
            json!({"use_case": "show", "reason": "run_ended"})
        );
    }

    #[test]
    fn watch_poll_reads_one_look_for_its_role_and_refuses_a_role_it_does_not_know() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let response = service.handle_from(
            &request(
                None,
                UseCase::WatchPoll,
                json!({"after": 7, "role": "inbox"}),
            ),
            Peer::Owner,
        );
        assert!(response.ok, "{response:?}");
        assert_eq!(response.result.unwrap()["after"], 7);
        assert_eq!(
            backend.log.lock().unwrap().pop().unwrap(),
            json!({"watch_look": 7, "role": "inbox"})
        );
        let response = service.handle_from(
            &request(None, UseCase::WatchPoll, json!({"role": "nobody"})),
            Peer::Owner,
        );
        assert_eq!(code(&response), Some(ServiceErrorCode::BadRequest));
    }

    #[test]
    fn a_request_without_a_principal_is_refused_and_recorded() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        for (token, reason) in [
            (None, "missing_token"),
            (Some(""), "missing_token"),
            (Some("forged"), "unknown_token"),
        ] {
            let response = service.handle(&request(token, UseCase::Show, json!({"id": 1})));
            assert_eq!(code(&response), Some(ServiceErrorCode::Unauthenticated));
            let recorded = backend.log.lock().unwrap().pop().unwrap();
            assert_eq!(recorded, json!({"use_case": "show", "reason": reason}));
        }
        let response = service.handle(&request(Some("broken"), UseCase::Show, json!({"id": 1})));
        assert_eq!(code(&response), Some(ServiceErrorCode::Failed));
        // A worker's token ends with its run.
        let ended = Backend {
            status: RunStatus::Integrated,
            ..backend
        };
        let service = QueueService {
            backend: &ended,
            build: "b",
            pid: 1,
        };
        let response = service.handle(&request(Some("worker"), UseCase::Show, json!({"id": 1})));
        assert_eq!(code(&response), Some(ServiceErrorCode::Unauthenticated));
        assert_eq!(
            ended.log.lock().unwrap().pop().unwrap()["reason"],
            "run_ended"
        );
        // A telemetry token of a target of no run ends with its target,
        // revoked yet or not (ADR-t1662-3 decision 11).
        let report = |target: &str| {
            json!({"version": crate::domain::telemetry::TELEMETRY_VERSION,
                "target": {"kind": "plan_review", "id": target},
                "session_id": format!("plan_review:{target}:s"), "steps": []})
        };
        let response = service.handle(&request(
            Some("telemetry-ended"),
            UseCase::TelemetryReport,
            report("9"),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::Unauthenticated));
        assert_eq!(
            ended.log.lock().unwrap().pop().unwrap(),
            json!({"use_case": "telemetry_report", "reason": "target_ended"})
        );
        let response = service.handle(&request(
            Some("telemetry-planner"),
            UseCase::TelemetryReport,
            json!({"version": crate::domain::telemetry::TELEMETRY_VERSION,
                "target": {"kind": "planner", "id": "3"},
                "session_id": "planner:3:s", "steps": []}),
        ));
        assert_eq!(code(&response), None, "{response:?}");
    }

    /// ADR-t3102-1 decision 4: a session wrapper's operations reach the
    /// queue as the wrapper's principal, on its own run or planner only;
    /// the worker's token (and a planner's, and a job's) is refused them
    /// and the refusal recorded as `authorization_denied`, and the records
    /// and reads of a run's turn's container's parts are refused to any
    /// but that run's wrapper (the inbox's and the supervisor's, a
    /// planner's wrapper's on its planner) the same way; an operation
    /// under the other use case is a bad request.
    #[test]
    fn only_the_wrapper_runs_and_records_its_own_session() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let heartbeat = |run: &str| json!({"op": "heartbeat_wrapper", "run": run, "pid": 7});
        let record = |run: &str| json!({"op": "runtime_event", "run": run, "kind": "turn_started", "payload": {}});
        let hook = json!({"op": "hook", "hook": {
            "event": {"hook": "start", "source": "startup"}, "kind": "runtime_planner",
            "session_id": "s1", "planner_id": 3}});
        // The records and reads of the container's parts a run's turn
        // readies are its own wrapper's alone.
        let image = |run: &str| {
            json!({"op": "image_event", "owner": {"kind": "run", "id": run},
            "kind": "container_image_built", "payload": {"reference": "localhost/q:ab"}})
        };
        let machine_user = |run: &str| {
            json!({"op": "machine_user_run",
            "owner": {"kind": "run", "id": run}, "run": "r9"})
        };
        let of_planner = |mut op: Value| {
            op["owner"] = json!({"kind": "planner", "id": "3"});
            op
        };
        let send = |token, use_case, params: &Value| {
            service.handle(&request(Some(token), use_case, params.clone()))
        };
        for (token, use_case, params) in [
            ("wrapper", UseCase::SessionRun, heartbeat("r1")),
            ("wrapper", UseCase::SessionRecord, record("r1")),
            (
                "wrapper",
                UseCase::SessionRun,
                json!({"op": "run", "run": "r2"}),
            ),
            ("wrapper-planner", UseCase::SessionRecord, hook.clone()),
            (
                "wrapper-planner",
                UseCase::SessionRun,
                json!({"op": "planner_exited", "planner": 3, "wrapper_pid": 7, "exit_code": 0}),
            ),
            ("wrapper", UseCase::SessionRecord, image("r1")),
            ("wrapper", UseCase::SessionRun, machine_user("r1")),
        ] {
            let response = send(token, use_case, &params);
            assert!(response.ok, "{token} {params}: {response:?}");
            assert_eq!(response.result, Some(json!({"value": null})));
            let done = backend.log.lock().unwrap().pop().unwrap();
            assert_eq!(done["session"]["op"], params["op"], "{token}");
        }
        for (token, use_case, params, capability) in [
            (
                "worker",
                UseCase::SessionRun,
                heartbeat("r1"),
                "session.run",
            ),
            (
                "worker",
                UseCase::SessionRecord,
                record("r1"),
                "session.record",
            ),
            (
                "planner",
                UseCase::SessionRecord,
                hook.clone(),
                "session.record",
            ),
            (
                "review",
                UseCase::SessionRun,
                heartbeat("r1"),
                "session.run",
            ),
            (
                "wrapper",
                UseCase::SessionRun,
                heartbeat("r2"),
                "session.run",
            ),
            (
                "wrapper",
                UseCase::SessionRecord,
                hook.clone(),
                "session.record",
            ),
            (
                "wrapper-planner",
                UseCase::SessionRun,
                json!({"op": "heartbeat_planner", "planner": 4, "wrapper_pid": 7}),
                "session.run",
            ),
            (
                "worker",
                UseCase::SessionRecord,
                image("r1"),
                "session.record",
            ),
            (
                "worker",
                UseCase::SessionRun,
                machine_user("r1"),
                "session.run",
            ),
            (
                "review",
                UseCase::SessionRecord,
                image("r1"),
                "session.record",
            ),
            (
                "review",
                UseCase::SessionRun,
                machine_user("r1"),
                "session.run",
            ),
            (
                "planner",
                UseCase::SessionRun,
                machine_user("r1"),
                "session.run",
            ),
            (
                "wrapper",
                UseCase::SessionRecord,
                image("r2"),
                "session.record",
            ),
            (
                "wrapper",
                UseCase::SessionRun,
                machine_user("r2"),
                "session.run",
            ),
            // The inbox and the supervisor hold both capabilities on any
            // run, and a planner's wrapper its own planner: none readies a
            // run's turn's container.
            (
                "inbox",
                UseCase::SessionRecord,
                image("r1"),
                "session.record",
            ),
            (
                "inbox",
                UseCase::SessionRun,
                machine_user("r1"),
                "session.run",
            ),
            (
                "supervisor",
                UseCase::SessionRun,
                machine_user("r1"),
                "session.run",
            ),
            (
                "wrapper-planner",
                UseCase::SessionRecord,
                of_planner(image("r1")),
                "session.record",
            ),
            (
                "wrapper-planner",
                UseCase::SessionRun,
                of_planner(machine_user("r1")),
                "session.run",
            ),
        ] {
            let refused = send(token, use_case, &params);
            assert_eq!(
                code(&refused),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{token} {params}"
            );
            let denial = backend.log.lock().unwrap().pop().unwrap();
            assert_eq!(denial["capability"], capability, "{token} {params}");
        }
        let crossed = send("wrapper", UseCase::SessionRecord, &heartbeat("r1"));
        assert_eq!(code(&crossed), Some(ServiceErrorCode::BadRequest));
        let crossed = send("wrapper", UseCase::SessionRecord, &machine_user("r1"));
        assert_eq!(code(&crossed), Some(ServiceErrorCode::BadRequest));
        let unknown = send(
            "wrapper",
            UseCase::SessionRun,
            &json!({"op": "drop_queue", "run": "r1"}),
        );
        assert_eq!(code(&unknown), Some(ServiceErrorCode::BadRequest));
        assert!(backend.log.lock().unwrap().is_empty());
    }

    /// `denial_record` records a refusal only as the caller's own: one of
    /// another role is refused (and that refusal recorded as the caller's),
    /// one the policy would not make and one that does not read are bad
    /// requests, recorded nowhere.
    #[test]
    fn a_principal_records_only_its_own_refusal() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let denial = |role: &str, capability: &str, reason: &str| {
            json!({"event": "authorization_denied", "role": role, "capability": capability,
                   "reason": reason, "resource": {"kind": "queue"}})
        };
        let send =
            |params: Value| service.handle(&request(Some("worker"), UseCase::DenialRecord, params));
        let own = send(denial("worker", "queue.watch", "not granted"));
        assert!(own.ok, "{own:?}");
        let recorded = backend.log.lock().unwrap().pop().unwrap();
        assert_eq!(
            recorded,
            denial("worker", "queue.watch", "not granted"),
            "{recorded}"
        );
        let crossed = send(denial("observer", "queue.watch", "not granted"));
        assert_eq!(code(&crossed), Some(ServiceErrorCode::AuthorizationDenied));
        let recorded = backend.log.lock().unwrap().pop().unwrap();
        assert_eq!(recorded["role"], "worker");
        assert_eq!(recorded["capability"], "queue.watch");
        assert_eq!(recorded["reason"], "not on this resource");
        for params in [
            denial("worker", "queue.read", "not granted"),
            denial("worker", "queue.watch", "because"),
            denial("worker", "queue.everything", "not granted"),
            json!({"event": "note", "role": "worker", "capability": "queue.watch",
                   "reason": "not granted", "resource": {"kind": "queue"}}),
            json!({"event": "authorization_denied", "role": "worker",
                   "capability": "queue.watch", "reason": "not granted", "resource": {}}),
            json!({"event": "authorization_denied", "role": "worker",
                   "capability": "queue.watch", "reason": "not granted",
                   "resource": {"kind": "queue", "text": "x".repeat(2000)}}),
            json!({"event": "authorization_denied", "role": "worker",
                   "capability": "queue.watch", "reason": "not granted",
                   "resource": {"kind": "queue", "list": [1, 2]}}),
        ] {
            assert_eq!(
                code(&send(params.clone())),
                Some(ServiceErrorCode::BadRequest),
                "{params}"
            );
        }
        assert!(backend.log.lock().unwrap().is_empty());
    }

    /// A worker proposes to replan its own run through the service, and
    /// nothing on another run; a read role's job proposes nothing
    /// (ADR-t1521-2 decision 3).
    #[test]
    fn a_worker_proposes_a_replanning_of_its_own_run_only() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let propose = |token, run: &str| {
            service.handle(&request(
                Some(token),
                UseCase::ReplanPropose,
                json!({"run": run, "reason": "two independent conditions", "evidence": [4]}),
            ))
        };
        let own = propose("worker", "r1");
        assert_eq!(code(&own), Some(ServiceErrorCode::Failed));
        assert!(
            own.error.unwrap().message.contains("replan recorded"),
            "the store was reached"
        );
        assert_eq!(
            backend.log.lock().unwrap().pop().unwrap(),
            json!({"replan": "worker", "run": "r1", "by": "worker"})
        );
        for (token, run) in [("worker", "r2"), ("review", "r1"), ("observer", "r1")] {
            let refused = propose(token, run);
            assert_eq!(
                code(&refused),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{token} on {run}"
            );
            let denial = backend.log.lock().unwrap().pop().unwrap();
            assert_eq!(denial["capability"], "replan.propose", "{token} on {run}");
        }
        assert_eq!(
            UseCase::of_command(&["replan", "propose", "--run", "r1"]),
            Some(UseCase::ReplanPropose)
        );
        assert_eq!(
            UseCase::of_command(&["replan", "request"]),
            Some(UseCase::ReplanRequest)
        );
    }

    /// The eval through the service (ADR-t1728-1 decision 6): a worker
    /// asks for a dev round on its own run and reads its run's rounds; a
    /// hold-out, a production round and a rerun are refused it and
    /// recorded; a review job asks for none.
    #[test]
    fn a_worker_asks_for_its_runs_dev_eval_only() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let response = service.handle(&request(
            Some("worker"),
            UseCase::AgentEval,
            json!({"agent": "adr-rules", "k": 2}),
        ));
        assert!(response.ok, "{response:?}");
        assert_eq!(response.result.unwrap()["split"], "dev");
        let recorded = backend.log.lock().unwrap().pop().unwrap();
        assert_eq!(recorded["eval_event"], "agent_eval_requested");
        assert_eq!(recorded["payload"]["run_id"], "r1");
        assert_eq!(recorded["payload"]["requested_by"], "worker");
        assert_eq!(recorded["payload"]["k"], 2);
        for params in [
            json!({"agent": "adr-rules", "split": "holdout"}),
            json!({"agent": "adr-rules", "split": "production"}),
            json!({"agent": "adr-rules", "set": 7}),
            json!({"agent": "adr-rules", "rerun": true}),
        ] {
            let response =
                service.handle(&request(Some("worker"), UseCase::AgentEval, params.clone()));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{params}"
            );
            let denial = backend.log.lock().unwrap().pop().unwrap();
            assert_eq!(denial["event"], "authorization_denied", "{params}");
            assert_eq!(denial["resource"]["kind"], "agent_eval");
            // The refusal answers what its event records.
            let denied = response.error.unwrap().denied.unwrap();
            assert_eq!(
                json!([denied.role, denied.capability, denied.reason]),
                json!([denial["role"], denial["capability"], denial["reason"]]),
                "{params}"
            );
        }
        // A production set is never a worker's to ask for or decide
        // (ADR-t1728-1 (g)).
        for (use_case, params) in [
            (UseCase::AgentCases, json!({"agent": "adr-rules"})),
            (
                UseCase::AgentDecide,
                json!({"set": 1, "case": "c", "verdict": "clean", "reason": "r"}),
            ),
        ] {
            let response = service.handle(&request(Some("worker"), use_case, params));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{use_case:?}"
            );
            let denial = backend.log.lock().unwrap().pop().unwrap();
            assert_eq!(denial["event"], "authorization_denied");
        }
        assert_eq!(
            UseCase::of_command(&["agent", "cases", "adr-rules"]),
            Some(UseCase::AgentCases)
        );
        let response = service.handle(&request(
            Some("worker"),
            UseCase::AgentResults,
            json!({"agent": "adr-rules"}),
        ));
        assert_eq!(response.result, Some(json!({"evals": []})));
        let response = service.handle(&request(
            Some("review"),
            UseCase::AgentEval,
            json!({"agent": "adr-rules"}),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::AuthorizationDenied));
        let response = service.handle(&request(
            Some("worker"),
            UseCase::AgentEval,
            json!({"agent": "Not Kebab"}),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::Failed));
        assert_eq!(
            UseCase::of_command(&["agent", "eval", "adr-rules"]),
            Some(UseCase::AgentEval)
        );
        assert_eq!(
            UseCase::of_command(&["agent", "results"]),
            Some(UseCase::AgentResults)
        );
        assert_eq!(
            UseCase::of_command(&["agent", "result", "3"]),
            Some(UseCase::AgentResult)
        );
    }

    /// The production sets read back through the service as `eval.read`
    /// lets the command line read them: the observer reads them, a review
    /// job and a worker off its own run are refused and recorded.
    #[test]
    fn the_production_sets_are_read_with_eval_read() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let response = service.handle(&request(
            Some("observer"),
            UseCase::AgentSets,
            json!({"agent": "adr-rules", "limit": 5}),
        ));
        assert_eq!(response.result, Some(json!({"sets": []})), "{response:?}");
        let response = service.handle(&request(
            Some("observer"),
            UseCase::AgentSet,
            json!({"id": 1}),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::Failed));
        for token in ["review", "worker"] {
            for (use_case, params) in [
                (UseCase::AgentSets, Value::Null),
                (UseCase::AgentSet, json!({"id": 1})),
            ] {
                let response = service.handle(&request(Some(token), use_case, params));
                assert_eq!(
                    code(&response),
                    Some(ServiceErrorCode::AuthorizationDenied),
                    "{token} {use_case:?}"
                );
                let denial = backend.log.lock().unwrap().pop().unwrap();
                assert_eq!(denial["capability"], "eval.read", "{token} {use_case:?}");
            }
        }
        assert_eq!(
            UseCase::of_command(&["agent", "sets"]),
            Some(UseCase::AgentSets)
        );
        assert_eq!(
            UseCase::of_command(&["agent", "set", "3"]),
            Some(UseCase::AgentSet)
        );
    }

    #[test]
    fn the_policy_is_the_service_s_and_acts_as_the_principal() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        // The worker asks on its own run, as itself whatever it names.
        let response = service.handle(&request(
            Some("worker"),
            UseCase::Ask,
            json!({"kind": "worker_question", "because": "scope", "topics": ["design_choice"],
                   "question": "q", "run_id": "r1"}),
        ));
        assert!(response.ok, "{response:?}");
        assert_eq!(
            response.result.unwrap(),
            json!({"opened": "worker_question", "run": "r1", "by": "worker"})
        );
        // Not on another run: refused and recorded as the worker.
        let response = service.handle(&request(
            Some("worker"),
            UseCase::Ask,
            json!({"kind": "worker_question", "because": "scope", "topics": ["design_choice"],
                   "question": "q", "run_id": "r2"}),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::AuthorizationDenied));
        let denial = backend.log.lock().unwrap().pop().unwrap();
        assert_eq!(denial["event"], "authorization_denied");
        assert_eq!(denial["role"], "worker");
        assert_eq!(denial["capability"], "ask.open");
        // A review job asks nothing and notes nothing.
        for (use_case, params) in [
            (
                UseCase::Ask,
                json!({"kind": "worker_question", "because": "scope", "question": "q", "run_id": "r1"}),
            ),
            (UseCase::Note, json!({"task": 1, "text": "x"})),
        ] {
            let response = service.handle(&request(Some("review"), use_case, params));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{use_case:?}"
            );
            assert_eq!(
                backend.log.lock().unwrap().pop().unwrap()["role"],
                ActorRole::ReviewJob.as_str()
            );
        }
        // Every role reads (ADR-t1233-5 decision 3).
        for token in ["worker", "review"] {
            let response = service.handle(&request(
                Some(token),
                UseCase::Show,
                json!({"id": 9, "full": true}),
            ));
            assert_eq!(
                response.result,
                Some(json!({"id": 9, "full": true, "events": 5}))
            );
        }
        // The worker notes on its task; the store's own error comes back as
        // a failure.
        let response = service.handle(&request(
            Some("worker"),
            UseCase::Note,
            json!({"task": 1, "text": "x"}),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::Failed));
        assert_eq!(
            backend.log.lock().unwrap().pop().unwrap(),
            json!({"note": "x", "by": "worker"})
        );
    }

    #[test]
    fn params_that_do_not_read_are_a_bad_request() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        for (use_case, params) in [
            (UseCase::Show, json!({})),
            (UseCase::Show, json!({"id": 1, "table": "tasks"})),
            (UseCase::Note, json!({"text": "no target"})),
            (UseCase::Note, json!({"task": 1, "goal": 2, "text": "two"})),
            (UseCase::Ask, json!({"sql": "x"})),
            (UseCase::ProposalShow, json!({})),
            (UseCase::ProposalList, json!({"all": true, "owner": "x"})),
            (
                UseCase::FindingRecord,
                json!({"kind": "stall", "summary": "no target"}),
            ),
            (
                UseCase::FindingRecord,
                json!({"kind": "stall", "summary": "two", "task": 1, "queue": true}),
            ),
            (
                UseCase::FindingRecord,
                json!({"kind": "stall", "summary": "s", "queue": true, "impact": "huge"}),
            ),
            (UseCase::FindingResolve, json!({"id": 1})),
            (UseCase::FindingDismiss, json!({"reason": "x"})),
        ] {
            let response = service.handle(&request(Some("worker"), use_case, params.clone()));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::BadRequest),
                "{params}"
            );
        }
    }

    #[test]
    fn the_observer_writes_findings_as_itself_and_nothing_else() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let pop = || backend.log.lock().unwrap().pop().unwrap();
        // A finding on each target, recorded as the observer.
        for (target, debug) in [
            (json!({"queue": true}), "Queue"),
            (json!({"task": 3}), "Task(TaskId(3))"),
            (json!({"goal": 4}), "Goal(GoalId(4))"),
            (json!({"run": "r9"}), "Run(RunId(\"r9\"))"),
        ] {
            let mut params = json!({"kind": "stall", "summary": "s", "evidence": [7]});
            params
                .as_object_mut()
                .unwrap()
                .extend(target.as_object().unwrap().clone());
            let response =
                service.handle(&request(Some("observer"), UseCase::FindingRecord, params));
            // The store's own error comes back as a failure.
            assert_eq!(code(&response), Some(ServiceErrorCode::Failed));
            assert_eq!(
                pop(),
                json!({"finding": "stall", "target": debug, "by": "observer"})
            );
        }
        let response = service.handle(&request(
            Some("observer"),
            UseCase::FindingResolve,
            json!({"id": 5, "reason": "gone"}),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::Failed));
        assert_eq!(
            pop(),
            json!({"finding": 5, "to": "resolved", "reason": "gone", "by": "observer"})
        );
        // Dismissing and noting are not the observer's, nor is a finding
        // the worker's: refused and recorded as the one refused.
        for (token, use_case, params, capability) in [
            (
                "observer",
                UseCase::FindingDismiss,
                json!({"id": 5, "reason": "no"}),
                "finding.dismiss",
            ),
            (
                "observer",
                UseCase::Note,
                json!({"task": 1, "text": "x"}),
                "note.write",
            ),
            (
                "worker",
                UseCase::FindingRecord,
                json!({"kind": "stall", "summary": "s", "queue": true}),
                "finding.record",
            ),
            (
                "worker",
                UseCase::FindingResolve,
                json!({"id": 5, "reason": "gone"}),
                "finding.resolve",
            ),
        ] {
            let response = service.handle(&request(Some(token), use_case, params));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{use_case:?}"
            );
            let denial = pop();
            assert_eq!(denial["event"], "authorization_denied");
            assert_eq!(denial["role"], token);
            assert_eq!(denial["capability"], capability);
        }
        // The proposals are read by every role.
        for token in ["observer", "worker", "review"] {
            let listed = service.handle(&request(
                Some(token),
                UseCase::ProposalList,
                json!({"all": true}),
            ));
            assert_eq!(listed.result, Some(json!({"proposals": [], "all": true})));
            let shown = service.handle(&request(
                Some(token),
                UseCase::ProposalShow,
                json!({"id": 2}),
            ));
            assert_eq!(shown.result, Some(json!({"proposal": 2})));
        }
        assert!(backend.log.lock().unwrap().is_empty());
    }

    #[test]
    fn every_role_reads_the_whole_queue_and_a_bad_read_is_a_bad_request() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        for token in ["worker", "review", "observer"] {
            let response = service.handle(&request(
                Some(token),
                UseCase::Events,
                json!({"full": true, "run": "r9"}),
            ));
            let read = response.result.unwrap()["read"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(read.starts_with("Events(EventsRead"), "{read}");
            assert!(read.contains("full: true"), "{read}");
            let response = service.handle(&request(Some(token), UseCase::GoalList, Value::Null));
            assert_eq!(
                response.result.unwrap()["read"],
                "GoalList(GoalListRead { tag: [] })"
            );
        }
        for (use_case, params) in [
            (UseCase::Stats, json!({"cmux": "/bin/sh"})),
            (UseCase::Kpi, json!({"period": "year"})),
            (UseCase::Candidates, json!({"x": 1})),
        ] {
            let response = service.handle(&request(Some("worker"), use_case, params));
            assert_eq!(code(&response), Some(ServiceErrorCode::BadRequest));
        }
        assert!(backend.log.lock().unwrap().is_empty());
    }

    /// A report of one step and one resource sample of `session` of the
    /// target `kind` `id`.
    fn sent(kind: &str, id: &str, session: &str) -> Value {
        json!({"version": crate::domain::telemetry::TELEMETRY_VERSION,
               "target": {"kind": kind, "id": id}, "session_id": session,
               "steps": [{"seq": 1, "kind": "model_turn", "offset_ms": 0, "duration_ms": 5}],
               "resources": [{"seq": 1, "offset_ms": 0, "duration_ms": 30000,
                              "cpu_seconds": 1.5, "memory_max_bytes": 1024}]})
    }

    /// [`sent`] of a session registered to its own target.
    fn report(kind: &str, id: &str) -> Value {
        sent(kind, id, &format!("{kind}:{id}:s1"))
    }

    /// ADR-t1662-3 decisions 9 and 10: the telemetry principal records the
    /// steps of its own target and is refused another's; no agent's token
    /// reports, whatever target it names.
    #[test]
    fn only_the_telemetry_principal_reports_and_only_of_its_own_target() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let pop = || backend.log.lock().unwrap().pop().unwrap();
        for (token, kind, id) in [
            ("telemetry", "run", "r1"),
            ("telemetry-planner", "planner", "3"),
        ] {
            let response = service.handle(&request(
                Some(token),
                UseCase::TelemetryReport,
                report(kind, id),
            ));
            assert_eq!(
                response.result,
                Some(json!({"steps": {"added": 1, "duplicates": 0},
                            "resources": {"added": 1, "duplicates": 0}})),
                "{response:?}"
            );
            assert_eq!(
                pop(),
                json!({"steps": 1, "resources": 1, "session": format!("{kind}:{id}:s1"),
                       "target": format!("{kind}:{id}")})
            );
        }
        // Its own target's, of a session registered to no target or to
        // another: refused, so no token takes another's session first.
        for session in ["s1", "planner:9:s1", "run:r2:s1"] {
            let response = service.handle(&request(
                Some("telemetry"),
                UseCase::TelemetryReport,
                sent("run", "r1", session),
            ));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{session}"
            );
            assert_eq!(
                response.error.unwrap().message,
                "executor-telemetry may not telemetry.report (not on this resource)"
            );
            assert_eq!(pop()["role"], "executor-telemetry");
            assert!(backend.log.lock().unwrap().is_empty());
        }
        // Another run's, planner's or job's target: refused and recorded as
        // the telemetry principal, nothing written.
        for (token, kind, id) in [
            ("telemetry", "run", "r2"),
            ("telemetry", "planner", "3"),
            ("telemetry", "plan_review", "4"),
            ("telemetry", "goal_review", "5"),
            ("telemetry", "throughput_review", "6"),
            ("telemetry-planner", "planner", "4"),
            ("telemetry-planner", "run", "r1"),
        ] {
            let response = service.handle(&request(
                Some(token),
                UseCase::TelemetryReport,
                report(kind, id),
            ));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{token} on {kind} {id}"
            );
            let denial = pop();
            assert_eq!(denial["role"], "executor-telemetry");
            assert_eq!(denial["capability"], "telemetry.report");
            assert!(backend.log.lock().unwrap().is_empty());
        }
        // No agent's token reports, of its own run or target included.
        for (token, role, kind, id) in [
            ("worker", "worker", "run", "r1"),
            ("review", "review-job", "run", "r1"),
            ("recovery", "recovery-job", "run", "r1"),
            ("plan-review", "plan-review-job", "plan_review", "4"),
            ("goal-review", "goal-review-job", "goal_review", "5"),
            (
                "throughput-review",
                "throughput-review-job",
                "throughput_review",
                "6",
            ),
            ("observer", "observer", "run", "r1"),
            ("planner", "planner", "planner", "3"),
        ] {
            let response = service.handle(&request(
                Some(token),
                UseCase::TelemetryReport,
                report(kind, id),
            ));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{token}"
            );
            assert_eq!(
                response.error.unwrap().message,
                format!("{role} may not telemetry.report (not granted)")
            );
            assert_eq!(pop()["role"], role);
            assert!(backend.log.lock().unwrap().is_empty());
        }
        // A session whose steps came in under another target is refused.
        let response = service.handle(&request(
            Some("telemetry"),
            UseCase::TelemetryReport,
            sent("run", "r1", "run:r1:taken"),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::BadRequest));
        assert!(backend.log.lock().unwrap().is_empty());
    }

    /// The report's own version, its bounds and the fields it does not
    /// know (docs/design/queue-service.md).
    #[test]
    fn a_report_of_an_unknown_version_or_past_its_bounds_is_refused_and_unknown_fields_dropped() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let send = |params: Value| {
            service.handle(&request(
                Some("telemetry"),
                UseCase::TelemetryReport,
                params,
            ))
        };
        let mut unknown = report("run", "r1");
        unknown["version"] = json!(crate::domain::telemetry::TELEMETRY_VERSION + 1);
        let mut over = report("run", "r1");
        over["steps"] = Value::Array(
            (1..=crate::domain::telemetry::MAX_STEPS_PER_REPORT + 1)
                .map(|seq| json!({"seq": seq, "kind": "wait", "offset_ms": 0, "duration_ms": 1}))
                .collect(),
        );
        let mut negative = report("run", "r1");
        negative["steps"][0]["seq"] = json!(0);
        let mut negative_sample = report("run", "r1");
        negative_sample["resources"][0]["cpu_seconds"] = json!(-1.0);
        let mut large = report("run", "r1");
        large["steps"][0]["command_shape"] =
            json!("x".repeat(crate::domain::queue_service::MAX_REQUEST_BYTES));
        for (params, says) in [
            (unknown, "version"),
            (over, "at most"),
            (negative, "seq 0"),
            (negative_sample, "resource sample 1"),
            (large, "bytes as JSON"),
            (Value::Null, "version"),
        ] {
            let response = send(params);
            assert_eq!(code(&response), Some(ServiceErrorCode::BadRequest));
            assert!(response.error.unwrap().message.contains(says), "{says}");
        }
        assert!(backend.log.lock().unwrap().is_empty());
        let mut later = report("run", "r1");
        later["sent_by"] = json!({"cpu_seconds": 1});
        later["steps"][0]["later"] = json!(true);
        later["resources"][0]["later"] = json!(true);
        assert!(send(later).ok);
        assert_eq!(backend.log.lock().unwrap().len(), 1);
        // A token whose run ended is no token.
        let ended = self::backend(RunStatus::Integrated);
        let response = QueueService {
            backend: &ended,
            build: "b",
            pid: 1,
        }
        .handle(&request(
            Some("telemetry"),
            UseCase::TelemetryReport,
            report("run", "r1"),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::Unauthenticated));
    }

    /// ADR-t1662-3 decision 9: the telemetry principal reaches no use case
    /// but `telemetry_report`, a read included, and changes nothing.
    #[test]
    fn the_telemetry_principal_is_refused_every_other_use_case() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        for (use_case, params) in [
            (UseCase::Status, Value::Null),
            (UseCase::Show, json!({"id": 1})),
            (UseCase::Events, json!({"run": "r1"})),
            (UseCase::List, Value::Null),
            (UseCase::GoalList, Value::Null),
            (
                UseCase::Ask,
                json!({"kind": "worker_question", "because": "scope", "topics": ["design_choice"],
                       "question": "q", "run_id": "r1"}),
            ),
            (UseCase::Note, json!({"run": "r1", "text": "x"})),
            (UseCase::ProposalList, Value::Null),
            (
                UseCase::FindingRecord,
                json!({"kind": "k", "run": "r1", "summary": "s"}),
            ),
            (UseCase::ReplanPropose, json!({"run": "r1", "reason": "r"})),
            (UseCase::AgentResults, Value::Null),
        ] {
            let response = service.handle(&request(Some("telemetry"), use_case, params));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{use_case:?}: {response:?}"
            );
            let denial = backend.log.lock().unwrap().pop().unwrap();
            assert_eq!(denial["role"], "executor-telemetry", "{use_case:?}");
            assert!(backend.log.lock().unwrap().is_empty(), "{use_case:?}");
        }
        // Whatever params it sends, no other use case answers it.
        for name in [
            "ask",
            "show",
            "note",
            "proposal_list",
            "proposal_show",
            "finding_record",
            "finding_resolve",
            "finding_dismiss",
            "list",
            "candidates",
            "graph",
            "status",
            "asks",
            "events",
            "timeline",
            "stats",
            "kpi",
            "forecast",
            "notes",
            "marks",
            "findings",
            "search",
            "related",
            "goal_list",
            "goal_show",
            "lint",
            "observe_history",
            "observe_input",
            "agent_eval",
            "agent_results",
            "agent_result",
            "replan_propose",
            "agent_cases",
            "agent_decide",
            "agent_sets",
            "agent_set",
        ] {
            let use_case: UseCase = name.parse().unwrap();
            let response = service.handle(&request(Some("telemetry"), use_case, json!({})));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{use_case:?}"
            );
            let log: Vec<Value> = backend.log.lock().unwrap().drain(..).collect();
            assert_eq!(log.len(), 1, "{use_case:?}: {log:?}");
            assert_eq!(log[0]["role"], "executor-telemetry", "{use_case:?}");
        }
        // An agent's token is refused telemetry_report, and recorded,
        // whatever its params.
        for params in [json!({}), json!({"version": 99}), Value::Null] {
            let response =
                service.handle(&request(Some("worker"), UseCase::TelemetryReport, params));
            assert_eq!(code(&response), Some(ServiceErrorCode::AuthorizationDenied));
            let log: Vec<Value> = backend.log.lock().unwrap().drain(..).collect();
            assert_eq!(log.len(), 1, "{log:?}");
            assert_eq!(log[0]["capability"], "telemetry.report");
        }
    }

    struct Control {
        found: ServiceProbe,
        started: Mutex<u32>,
    }

    impl QueueServiceControl for Control {
        fn probe(&self) -> ServiceProbe {
            self.found.clone()
        }
        fn start(&self, _: Duration) -> Result<ServiceProbe> {
            *self.started.lock().unwrap() += 1;
            Ok(ServiceProbe {
                state: ServiceState::Running,
                build_matches: Some(true),
                ..self.found.clone()
            })
        }
        fn stop(&self, _: Duration) -> Result<Option<u32>> {
            Ok(None)
        }
        fn replace(&self, _: bool, _: bool, _: Duration) -> Result<Replaced> {
            *self.started.lock().unwrap() += 1;
            Ok(Replaced {
                service: self.found.clone(),
                reused: false,
                replaced: None,
                migrated: Some(json!({"applied": [{"version": 9}]})),
            })
        }
    }

    /// `up` reports what the service a replacement started applied.
    #[test]
    fn up_reports_what_the_replacing_service_applied() {
        let control = Control {
            found: probe(ServiceState::Running, Some(true)),
            started: Mutex::new(0),
        };
        let report = ensure_replaced(&control, Duration::from_secs(1)).unwrap();
        assert_eq!(report["outcome"], "started");
        assert_eq!(report["migrated"]["applied"][0]["version"], 9);
        assert_eq!(*control.started.lock().unwrap(), 1);
    }

    fn probe(state: ServiceState, build_matches: Option<bool>) -> ServiceProbe {
        ServiceProbe {
            state,
            socket: "/q/service/queue.sock".into(),
            pid: None,
            build: None,
            api_version: None,
            min_api_version: None,
            build_matches,
            started_at: None,
            error: None,
        }
    }

    #[test]
    fn up_reuses_a_current_service_and_starts_or_replaces_any_other() {
        for (found, outcome, starts) in [
            (probe(ServiceState::Running, Some(true)), "reused", 0),
            (probe(ServiceState::Stopped, None), "started", 1),
            (probe(ServiceState::Running, Some(false)), "replaced", 1),
            (probe(ServiceState::Unreachable, None), "replaced", 1),
        ] {
            let control = Control {
                found,
                started: Mutex::new(0),
            };
            let report = ensure(&control, Duration::from_secs(1)).unwrap();
            assert_eq!(report["outcome"], outcome);
            assert_eq!(*control.started.lock().unwrap(), starts);
            assert_eq!(report["service"]["state"], "running");
        }
    }

    /// A principal the policy grants `scheduler.supervise` that holds no
    /// supervisor's registration (the user, a supervisor of none) is
    /// refused as `authorization_denied`, the refusal recorded once as the
    /// policy's is; one that holds it is answered, recording nothing.
    #[test]
    fn a_supervisor_request_of_no_registration_is_refused_and_recorded() {
        let unregistered = [
            ActorContext::user(),
            ActorContext::instance(ActorRole::Supervisor, 7),
        ];
        for actor in unregistered {
            let mut queue = Queue {
                status: RunStatus::Running,
                log: Default::default(),
                denials: RefCell::default(),
            };
            let error = authorize_supervisor(&mut queue, &actor, false).unwrap_err();
            let refused = error.downcast_ref::<AuthorizationError>().unwrap();
            assert_eq!(refused.role, actor.role());
            assert_eq!(refused.capability, Capability::Supervise);
            assert_eq!(
                refused.reason,
                crate::domain::authorization::DenyReason::NotGranted
            );
            let response = respond(Err(error));
            let failure = response.error.unwrap();
            assert_eq!(failure.code, ServiceErrorCode::AuthorizationDenied);
            assert!(failure.denied.is_some());
            let denials = queue.denials.borrow();
            assert_eq!(denials.len(), 1, "{denials:?}");
            assert_eq!(denials[0]["capability"], "scheduler.supervise");
            assert_eq!(denials[0]["reason"], "not granted");
            assert_eq!(denials[0]["role"], json!(actor.role()));
            assert_eq!(
                denials[0]["why"],
                "the principal holds no supervisor's registration"
            );
        }
        let mut queue = Queue {
            status: RunStatus::Running,
            log: Default::default(),
            denials: RefCell::default(),
        };
        let supervisor = ActorContext::instance(ActorRole::Supervisor, 7);
        authorize_supervisor(&mut queue, &supervisor, true).unwrap();
        assert!(queue.denials.borrow().is_empty());
    }
}
