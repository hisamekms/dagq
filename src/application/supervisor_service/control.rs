//! The service's side of a control-side command a person or the inbox runs
//! on the host ([`UseCase::Control`](crate::domain::queue_service::UseCase::Control)):
//! `integrate`, `review`, `observe`, `throughput-review` and
//! `setup-review` run in the caller's process, which does their Git and
//! starts their programs (ADR-t3102-3 decision 5), and send each operation
//! of the queue they make as a step of their command, one transaction
//! each, as the supervisor's are ([`super::SupervisorQueue`]).
//!
//! At each step the command's operation is authorized for the principal
//! as the command line authorizes it before it runs (refusals recorded), so
//! a principal the command is not for is refused at its first step; then
//! only the reads of the queue (`queue.read`) and the records that command
//! makes are answered, and a record only of the kinds that command
//! records. The caller names no lease but the one it takes the integration
//! slot under: a new one, which no run and no supervisor holds, with which
//! the landing's integrator token is issued in the same transaction
//! (ADR-t3102-1 decision 2), the landing then going on with that token
//! ([`super::answer`]'s integrator) as a supervisor's landing does. A
//! landing is begun and ended only by the process the command's steps
//! name as its own, as the connection confirms it: a step naming another
//! command's process, or one whose connection tells none, takes no slot,
//! is issued no token and revokes none, and is refused as
//! `authorization_denied`, recorded. The
//! events are the principal's own, the landing's the integrator's at its
//! request.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use super::super::commands::operations::Operation;
use super::super::queue_service::ServiceBackend;
use super::resend::{Resend, Resending, Waits};
use super::{BEGIN_LANDING, BadOperation, END_LANDING, NotOwnLease, ServiceCaller, read, reply};
use crate::application::{Generators, RunReads, RunRecovery, SupervisorReads};
use crate::domain::authorization::Capability;
use crate::domain::queue_service::{ControlCall, ControlCommand, ControlParams, Principal};
use crate::domain::session_step::StepTarget;
use crate::domain::{CommitSha, EventKind, LeaseToken, RunId, RunStatus, TaskId};

/// The step that starts a command: the queue's DB the service serves and
/// the principal's actor, which the command's records are the events of.
pub const CONTROL_QUEUE: &str = "control_queue";

/// The step of `integrate` that records the approval to land its run
/// ([`crate::application::RunTransitions::approve_integration`]): the run's
/// status, its lease and its approvals are read again in the approval's
/// own write (ADR-t3102-3 decision 4), a lease held as the service's own
/// processes and clock see it.
pub const APPROVE_INTEGRATION: &str = "approve_integration";

/// The steps of the telemetry of a command's agent or verification: the
/// token of a target of no run, issued and revoked by the service under no
/// lease (a run's is issued under its lease, by its supervisor), the
/// tokens held, the facts of a target and the registration of a session
/// to it.
const TELEMETRY_OPS: [&str; 5] = [
    "issue_telemetry",
    "revoke_telemetry",
    "held_telemetry",
    "target_facts",
    "register_telemetry_session",
];

/// The caller of a command's steps on `steps`: the records of the results
/// it made (`main` moved, an agent's process that ended) are sent again
/// under their first identity until the service takes them, for as long as
/// it takes ([`Waits::CONTROL`]), since they are its caller's and no
/// supervisor sends them as that caller (ADR-t3102-3 decision 5).
pub fn resending(steps: Arc<dyn ServiceCaller>, generators: Generators) -> Resending {
    Resending::with_waits(steps, Resend::Wait, generators, Waits::CONTROL)
}

/// The runtime operation `call`'s command is, as the command line
/// authorizes it ([`crate::application::commands::operations`]).
pub fn operation(call: &ControlCall) -> Result<Operation> {
    Ok(match call.command {
        ControlCommand::Integrate => Operation::Integrate(call.task),
        ControlCommand::Review => Operation::Review(
            call.task
                .ok_or_else(|| BadOperation("review names no task".into()))?,
        ),
        ControlCommand::Observe | ControlCommand::ThroughputReview => Operation::Observe,
        ControlCommand::SetupReview => Operation::SetupReview { yes: call.yes },
    })
}

/// The kinds of the queue's events `command` records as the principal
/// itself (`record_queue_event`), and of a run's (`record_runtime_event`):
/// an observation's and a throughput review's start and end, an agent's
/// provider found again by its name and its telemetry's token, the image
/// a landing's container verification built or failed to, the wall an
/// observation stopped at (`auth_required`, `usage_limited`) and the phase
/// of the run a landing begins. The approval to land is its own step
/// ([`APPROVE_INTEGRATION`]), and so is the hold an observation's wall
/// opens or joins ([`own_hold`]).
fn recorded_kinds(command: ControlCommand) -> (&'static [EventKind], &'static [EventKind]) {
    match command {
        ControlCommand::Integrate => (
            &[
                EventKind::ContainerImageBuilt,
                EventKind::ContainerImageFailed,
            ],
            &[EventKind::RunPhaseChanged],
        ),
        ControlCommand::Review => (&[], &[]),
        ControlCommand::Observe => (
            &[
                EventKind::ObserveStarted,
                EventKind::ObserveFinished,
                EventKind::AuthRequired,
                EventKind::UsageLimited,
                EventKind::ProviderExecutableRelocated,
                EventKind::TelemetryTokenIssued,
                EventKind::TelemetryTokenRevoked,
            ],
            &[],
        ),
        ControlCommand::ThroughputReview => (
            &[
                EventKind::ThroughputReviewStarted,
                EventKind::ThroughputReviewFinished,
                EventKind::ThroughputReviewReported,
                EventKind::ProviderExecutableRelocated,
                EventKind::TelemetryTokenIssued,
                EventKind::TelemetryTokenRevoked,
            ],
            &[],
        ),
        ControlCommand::SetupReview => (
            &[
                EventKind::ProviderExecutableRelocated,
                EventKind::TelemetryTokenIssued,
                EventKind::TelemetryTokenRevoked,
            ],
            &[],
        ),
    }
}

/// Why `command` does not send `op` with `args`, if it does not: the
/// reads of the queue are every command's, and the rest are the steps
/// its own records and its landing make ([`recorded_kinds`]).
pub fn scope(command: ControlCommand, op: &str, args: &[Value]) -> Result<(), String> {
    let kind = |index: usize| serde_json::from_value::<EventKind>(args.get(index)?.clone()).ok();
    let (queue_kinds, run_kinds) = recorded_kinds(command);
    let named = match op {
        CONTROL_QUEUE => true,
        "record_queue_event" => kind(0).is_some_and(|kind| queue_kinds.contains(&kind)),
        "record_runtime_event" => kind(1).is_some_and(|kind| run_kinds.contains(&kind)),
        BEGIN_LANDING | END_LANDING | APPROVE_INTEGRATION => command == ControlCommand::Integrate,
        "job_read" => matches!(
            command,
            ControlCommand::Observe | ControlCommand::ThroughputReview
        ),
        "record_finding" => command == ControlCommand::ThroughputReview,
        "hold" => own_hold(command, args),
        _ if TELEMETRY_OPS.contains(&op) => command != ControlCommand::Review,
        _ => super::needs::READS.contains(&op),
    };
    if named {
        Ok(())
    } else {
        Err(format!(
            "dagq {} does not {op} with {}",
            command.as_str().replace('_', "-"),
            Value::Array(args.to_vec())
        ))
    }
}

/// Whether the hold `args` name is one `command` opens or joins itself:
/// the `queue_hold` ask of the wall an observation's agent stopped at, as
/// the observer job and on no run ([`crate::domain::NewHold::wall`]), as
/// the command on the DB opens it.
fn own_hold(command: ControlCommand, args: &[Value]) -> bool {
    let hold = args
        .first()
        .and_then(|hold| serde_json::from_value::<crate::domain::NewHold>(hold.clone()).ok());
    command == ControlCommand::Observe
        && hold.is_some_and(|hold| {
            hold.run_id.is_none() && hold.job == Some(crate::domain::queue_hold::HoldJob::Observer)
        })
}

/// Answer the step `params` of the control-side command of `principal` on
/// `backend` (see the module's documentation), sent from the process
/// `process` when the connection tells it.
pub fn answer(
    backend: &dyn ServiceBackend,
    principal: &Principal,
    params: &Value,
    process: Option<u32>,
) -> Result<Value> {
    let actor = principal.actor();
    let params: ControlParams = serde_json::from_value(params.clone())
        .map_err(|error| BadOperation(format!("the params of control do not read: {error}")))?;
    let ControlParams { control, call } = params;
    let mut queue = backend.open(&actor)?;
    super::super::queue_service::authorize_operation(&mut *queue, &actor, &operation(&control)?)?;
    let op = call.op.as_str();
    scope(control.command, op, &call.args).map_err(BadOperation)?;
    // The caller's records are its own: no other actor and no requester
    // (its landing's go with the integrator's token, at its request).
    if call.acting.is_some() || call.requested_by.is_some() {
        return Err(BadOperation(format!("dagq {op} writes as the caller, not as another")).into());
    }
    if super::needs::READS.contains(&op) {
        super::super::queue_service::authorize_supervisor_needs(
            &mut *queue,
            &actor,
            Capability::QueueRead,
        )?;
    }
    let args = Value::Array(call.args.clone());
    match op {
        CONTROL_QUEUE => {
            return Ok(json!({
                "db": backend.db(),
                "role": actor.role(),
                "actor_id": actor.actor_id(),
            }));
        }
        BEGIN_LANDING => {
            let (run, token, main, over_handoff): (RunId, LeaseToken, CommitSha, bool) =
                read(op, args)?;
            if over_handoff {
                return Err(BadOperation(
                    "a person's landing is never begun over a handoff".into(),
                )
                .into());
            }
            // Only the command's own process, as the connection confirms
            // it, takes a slot under its pid (a refusal recorded).
            let pid = landing_process(&mut *queue, &actor, &control, process, &run)?;
            let store = queue.supervisor_store();
            of_named_task(store, &control, &run)?;
            // A lease the caller makes for its landing: none another holds.
            if !RunRecovery::runs_leased_by(&*store, &token)?.is_empty()
                || SupervisorReads::supervisors(&*store)?
                    .iter()
                    .any(|supervisor| supervisor.token == token)
            {
                return Err(anyhow::Error::new(NotOwnLease));
            }
            store.restore_request(None);
            let landing = Principal::integrator(&run, &token, pid);
            // The lease and `integration_started` name the command's
            // process, which lands: one that dies leaves a landing the
            // supervisor releases.
            let issuer =
                crate::domain::queue_service::TokenIssuer::of(principal, backend.process_of(pid));
            return super::begin_landing(
                backend,
                &mut *queue,
                &landing,
                &main,
                false,
                super::LandingBy::Command { pid },
                &issuer,
            );
        }
        END_LANDING => {
            let (run,): (RunId,) = read(op, args)?;
            // Only the landing the command's own process began: one naming
            // another command's process ends none of its landings.
            let pid = landing_process(&mut *queue, &actor, &control, process, &run)?;
            let key = landing_key(&run, pid);
            let store = queue.supervisor_store();
            of_named_task(store, &control, &run)?;
            // Only the token of a landing that left `integrating`, and never
            // a supervisor's landing's: one going on is not ended here.
            let integrating = RunReads::run(&*store, &run)?.status() == RunStatus::Integrating;
            let supervisors = SupervisorReads::supervisors(&*store)?;
            let a_supervisors = backend.principals()?.iter().any(|principal| {
                principal.credential_key() == key
                    && supervisors.iter().any(|supervisor| {
                        principal.registration.as_ref() == Some(&supervisor.token)
                    })
            });
            if integrating || a_supervisors {
                return Err(BadOperation(format!(
                    "dagq integrate ends no landing of run {run} that goes on or is a supervisor's"
                ))
                .into());
            }
            return reply(backend.access().revoke(backend.db(), &key));
        }
        "issue_telemetry" => {
            let (target, task, under): (StepTarget, Option<TaskId>, Option<LeaseToken>) =
                read(op, args)?;
            if under.is_some() || target.run_id().is_some() {
                return Err(anyhow::Error::new(NotOwnLease));
            }
            own_target(queue.supervisor_store(), &control, &actor, &target)?;
            // Issued as `issue_token` issues it at a parent's request: only
            // to the command whose process the start names, that process
            // the connection's (a step naming another's asks as none), and
            // only while the target holds none, read in the issue's
            // transaction (a refusal recorded); the token keeps the command
            // that asked.
            let issued = super::super::queue_service::issue_token(
                backend,
                &mut *queue,
                principal,
                &Principal::telemetry(&target, task),
                control.process(process),
            )?;
            let issued: super::super::queue_service::Issued = serde_json::from_value(issued)?;
            return reply(Ok(issued.file));
        }
        "revoke_telemetry" => {
            let (target, under): (StepTarget, Option<LeaseToken>) = read(op, args)?;
            if under.is_some() || target.run_id().is_some() {
                return Err(anyhow::Error::new(NotOwnLease));
            }
            own_target(queue.supervisor_store(), &control, &actor, &target)?;
            return reply(
                backend
                    .telemetry()
                    .revoke_telemetry(backend.db(), &target, None),
            );
        }
        "held_telemetry" => return reply(backend.telemetry().held_telemetry(backend.db())),
        "target_facts" => {
            let (target,): (StepTarget,) = read(op, args)?;
            return reply(queue.telemetry_target(&target));
        }
        "register_telemetry_session" => {
            let (target, session_id): (StepTarget, String) = read(op, args)?;
            own_target(queue.supervisor_store(), &control, &actor, &target)?;
            return reply(queue.register_session(&target, &session_id, backend.now_ms()));
        }
        APPROVE_INTEGRATION => {
            let (run, payload): (RunId, Value) = read(op, args)?;
            let store = queue.supervisor_store();
            // The run's task does not change: its status, lease and
            // approvals are read in the write.
            of_named_task(store, &control, &run)?;
            store.restore_request(None);
            let now = backend.now_ms() / 1000;
            let held = |lease: &crate::domain::RunLease| {
                !crate::domain::heartbeat_stale(backend.alive(lease.pid), now - lease.heartbeat_at)
            };
            return reply(store.approve_integration(&run, payload, &held));
        }
        "job_read" | "record_finding" => {
            return super::jobs(&mut *queue, &call);
        }
        _ => {}
    }
    let store = queue.supervisor_store();
    if op == "record_runtime_event" {
        let run: RunId = read(op, call.args.first().cloned().unwrap_or(Value::Null))?;
        landing_run(store, &control, &run)?;
    }
    store.restore_request(None);
    if let Some(record) = super::once_record(&call) {
        return super::dispatch::dispatch_once(store, op, args, &record);
    }
    // The caller names no lease of its own here: its landing's goes with
    // the integrator's token.
    super::dispatch::dispatch(
        store,
        &|_| Err(anyhow::Error::new(NotOwnLease)),
        &|pid| backend.alive(pid),
        backend.now_ms() / 1000,
        op,
        args,
    )
}

/// Whether `run` is of the task `call` names (any task for `integrate
/// --next`): a landing's approval, its slot and its integrator's token, and
/// its token's end, are only of a run of that task. Why not, otherwise.
fn of_named_task(
    store: &dyn super::SupervisorStore,
    call: &ControlCall,
    run: &RunId,
) -> Result<()> {
    let task = RunReads::run(store, run)?.task_id();
    match call.task.filter(|named| *named != task) {
        Some(named) => Err(BadOperation(format!(
            "dagq {} of task {named} lands no run {run} of task {task}",
            call.command.as_str().replace('_', "-")
        ))
        .into()),
        None => Ok(()),
    }
}

/// Whether `run` is one `call`'s landing records on: a run of the task it
/// names (any task for `integrate --next`) that awaits integration or a
/// session, or lands. Why not, otherwise.
fn landing_run(store: &dyn super::SupervisorStore, call: &ControlCall, run: &RunId) -> Result<()> {
    let found = RunReads::run(store, run)?;
    let landable = matches!(
        found.status(),
        RunStatus::AwaitingIntegration | RunStatus::NeedsSession | RunStatus::Integrating
    );
    if landable && call.task.is_none_or(|task| task == found.task_id()) {
        Ok(())
    } else {
        Err(BadOperation(format!(
            "dagq {} records nothing on run {run} ({})",
            call.command.as_str().replace('_', "-"),
            found.status().as_str()
        ))
        .into())
    }
}

/// Whether `target` is the command's own, whose telemetry it reports: for
/// `integrate` a run it lands ([`landing_run`]), for `observe` and
/// `throughput-review` the observation or review whose start `actor`
/// recorded (the target's id is that event's). Why not, otherwise.
fn own_target(
    store: &dyn super::SupervisorStore,
    call: &ControlCall,
    actor: &crate::domain::ActorContext,
    target: &StepTarget,
) -> Result<()> {
    use crate::domain::session_step::TargetKind;
    let started = |kind: EventKind| -> Result<bool> {
        let Ok(id) = target.id.parse::<i64>() else {
            return Ok(false);
        };
        Ok(crate::application::PlanRequestStore::event_by_id(
            store,
            crate::domain::EventId::new(id),
        )?
        .is_some_and(|event| {
            event.kind.known() == Some(kind)
                && event
                    .actor
                    .is_some_and(|recorded| recorded.id == actor.actor_id())
        }))
    };
    let own = match (call.command, target.kind) {
        (ControlCommand::Integrate, TargetKind::Run) => match target.run_id() {
            Some(run) => return landing_run(store, call, &run),
            None => false,
        },
        (ControlCommand::Observe, TargetKind::Observer) => started(EventKind::ObserveStarted)?,
        (ControlCommand::ThroughputReview, TargetKind::ThroughputReview) => {
            started(EventKind::ThroughputReviewStarted)?
        }
        _ => false,
    };
    if own {
        Ok(())
    } else {
        Err(BadOperation(format!(
            "dagq {} reports no telemetry of {} {}",
            call.command.as_str().replace('_', "-"),
            target.kind.as_str(),
            target.id
        ))
        .into())
    }
}

/// Why a landing's step that names `named` as its command's process is
/// not of that process as the connection tells it (`connection`, `None`
/// when it cannot be read; [`ControlCall::process`]), if it is not: the
/// slot, its lease, the integrator's token and that token's end are only
/// of the process that lands (ADR-t3102-1 decision 2).
fn landing_refusal(named: &ControlCall, connection: Option<u32>) -> Option<String> {
    match (named.process(connection), connection) {
        (Some(_), _) => None,
        (None, Some(connection)) => Some(format!(
            "the landing's step names process {} but comes from process {connection}",
            named.pid
        )),
        (None, None) => Some(format!(
            "the landing's step names process {} and its connection tells none",
            named.pid
        )),
    }
}

/// The pid of the process `control`'s landing of `run` is of: its own as
/// the connection `connection` confirms it, or a refusal recorded as
/// `authorization_denied` of `actor` ([`landing_refusal`]).
fn landing_process(
    queue: &mut dyn super::super::queue_service::ServiceQueue,
    actor: &crate::domain::ActorContext,
    control: &ControlCall,
    connection: Option<u32>,
    run: &RunId,
) -> Result<u32> {
    match landing_refusal(control, connection) {
        None => Ok(control.pid),
        Some(why) => Err(super::super::queue_service::refuse_recorded(
            queue,
            actor,
            Capability::TokenIssue,
            &crate::domain::Resource::Principal(crate::domain::ActorRole::Integrator),
            crate::domain::authorization::DenyReason::Resource,
            &format!("run {run}: {why}"),
        )),
    }
}

/// The key of the token of the integrator of `run`'s landing a command of
/// process `pid` began ([`Principal::credential_key`]).
fn landing_key(run: &RunId, pid: u32) -> String {
    Principal::integrator(run, &LeaseToken::new("landing"), pid).credential_key()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(command: ControlCommand) -> ControlCall {
        ControlCall {
            command,
            task: Some(TaskId::new(3)),
            yes: false,
            pid: 7,
        }
    }

    #[test]
    fn each_command_is_authorized_as_its_command_line_is() {
        use ControlCommand as C;
        assert_eq!(
            operation(&call(C::Integrate)).unwrap(),
            Operation::Integrate(Some(TaskId::new(3)))
        );
        assert_eq!(
            operation(&call(C::Review)).unwrap(),
            Operation::Review(TaskId::new(3))
        );
        for command in [C::Observe, C::ThroughputReview] {
            assert_eq!(operation(&call(command)).unwrap(), Operation::Observe);
        }
        let setup = ControlCall {
            yes: true,
            ..call(C::SetupReview)
        };
        assert_eq!(
            operation(&setup).unwrap(),
            Operation::SetupReview { yes: true }
        );
        let unnamed = ControlCall {
            task: None,
            ..call(C::Review)
        };
        assert!(operation(&unnamed).is_err());
    }

    #[test]
    fn a_command_sends_the_reads_and_its_own_records_only() {
        use ControlCommand as C;
        let event = |kind: EventKind| serde_json::to_value(kind).unwrap();
        let queue_event = |kind| vec![event(kind), json!({})];
        let run_event = |kind| vec![json!("r1"), event(kind), json!({})];
        for command in [
            C::Integrate,
            C::Review,
            C::Observe,
            C::ThroughputReview,
            C::SetupReview,
        ] {
            assert!(scope(command, "show", &[json!(1)]).is_ok());
            assert!(scope(command, CONTROL_QUEUE, &[]).is_ok());
            // None claims, transitions or lands as another.
            for op in ["claim_for_supervisor", "finish_integration", "cancel"] {
                assert!(scope(command, op, &[]).is_err(), "{command:?} {op}");
            }
        }
        assert!(scope(C::Integrate, BEGIN_LANDING, &[]).is_ok());
        assert!(scope(C::Observe, BEGIN_LANDING, &[]).is_err());
        assert!(scope(C::Integrate, APPROVE_INTEGRATION, &[]).is_ok());
        assert!(scope(C::Review, APPROVE_INTEGRATION, &[]).is_err());
        // The approval goes only by its own step, which reads its
        // prerequisites in its write.
        assert!(
            scope(
                C::Integrate,
                "record_runtime_event",
                &run_event(EventKind::IntegrationApproved)
            )
            .is_err()
        );
        assert!(
            scope(
                C::Integrate,
                "record_queue_event",
                &queue_event(EventKind::ContainerImageBuilt)
            )
            .is_ok()
        );
        assert!(
            scope(
                C::Integrate,
                "record_runtime_event",
                &run_event(EventKind::RunIntegrated)
            )
            .is_err()
        );
        assert!(
            scope(
                C::Observe,
                "record_queue_event",
                &queue_event(EventKind::ObserveFinished)
            )
            .is_ok()
        );
        assert!(
            scope(
                C::Observe,
                "record_queue_event",
                &queue_event(EventKind::ThroughputReviewFinished)
            )
            .is_err()
        );
        // An observation's wall holds as the observer job, on no run.
        let wall = |job| {
            let hold = crate::domain::NewHold::wall(
                crate::domain::queue_hold::Wall::Authentication,
                None,
                job,
            );
            vec![serde_json::to_value(hold).unwrap()]
        };
        let observer = Some(crate::domain::queue_hold::HoldJob::Observer);
        assert!(scope(C::Observe, "hold", &wall(observer.clone())).is_ok());
        assert!(scope(C::Observe, "hold", &wall(None)).is_err());
        assert!(scope(C::ThroughputReview, "hold", &wall(observer.clone())).is_err());
        assert!(scope(C::Integrate, "hold", &wall(observer)).is_err());
        assert!(
            scope(
                C::Observe,
                "record_queue_event",
                &queue_event(EventKind::AuthRequired)
            )
            .is_ok()
        );
        assert!(scope(C::ThroughputReview, "record_finding", &[]).is_ok());
        assert!(scope(C::Observe, "record_finding", &[]).is_err());
        assert!(scope(C::Review, "register_telemetry_session", &[]).is_err());
        assert!(
            scope(
                C::Review,
                "record_queue_event",
                &queue_event(EventKind::ObserveFinished)
            )
            .is_err()
        );
    }

    #[test]
    fn a_landing_is_only_of_the_process_the_connection_confirms() {
        let named = call(ControlCommand::Integrate);
        assert_eq!(landing_refusal(&named, Some(7)), None);
        // Another command's pid named from this connection, and a
        // connection that tells no process, are refused.
        assert!(landing_refusal(&named, Some(8)).is_some_and(|why| why.contains("process 8")));
        assert!(landing_refusal(&named, None).is_some());
    }

    #[test]
    fn the_landing_s_token_is_revoked_by_its_run_and_process() {
        let run = RunId::new("r1").unwrap();
        assert_eq!(
            landing_key(&run, 7),
            Principal::integrator(&run, &LeaseToken::new("slot"), 7).credential_key()
        );
    }
}
