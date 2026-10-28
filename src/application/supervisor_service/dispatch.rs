//! The service's side of [`super::SupervisorQueue`]: each operation the
//! supervisor sends by its name is the queue's own on the store, with
//! every lease token it names checked to be the principal's registration
//! first (`own`). Made from the ports' traits as [`super::forward`] is.

#![allow(clippy::wildcard_imports)]

use super::{BadOperation, SupervisorStore, read, reply as answer};
use crate::application::Landing;
use crate::application::*;
use crate::domain::cancel_reason::NewCancel;
use crate::domain::search::SearchQuery;
use crate::domain::*;
use anyhow::Result;
use serde_json::Value;

/// Record the event of `op` (one of [`super::resend::MATCHED_OPS`]) with
/// `args` as the result's record `record`: as the event recorded under it
/// before, if there is one (ADR-t3102-3 decision 5).
pub(super) fn dispatch_once(
    store: &mut dyn SupervisorStore,
    op: &str,
    args: Value,
    record: &str,
) -> Result<Value> {
    match op {
        "record_runtime_event" => {
            let (id, kind, payload): (RunId, EventKind, serde_json::Value) = read(op, args)?;
            answer(
                EventStore::record_event_once(&*store, Some(&id), kind, payload, record)
                    .map(|_| ()),
            )
        }
        "record_queue_event" => {
            let (kind, payload): (EventKind, serde_json::Value) = read(op, args)?;
            answer(EventStore::record_event_once(
                &*store, None, kind, payload, record,
            ))
        }
        _ => Err(BadOperation(format!("{op} is not taken once by its record")).into()),
    }
}

/// Run the operation `op` with `args` on `store` at `now` (seconds): its
/// result as the service answers it.
pub(super) fn dispatch<S: SupervisorStore + ?Sized>(
    store: &mut S,
    own: &dyn Fn(&LeaseToken) -> Result<()>,
    alive: &dyn Fn(u32) -> bool,
    now: i64,
    op: &str,
    args: Value,
) -> Result<Value> {
    match op {
        "candidates" => answer(TaskReads::candidates(&*store)),
        "build_waits" => answer(TaskReads::build_waits(&*store)),
        "predecessors" => {
            let (task_id,): (TaskId,) = read(op, args)?;
            answer(TaskReads::predecessors(&*store, task_id))
        }
        "goal_predecessors" => {
            let (task_id,): (TaskId,) = read(op, args)?;
            answer(TaskReads::goal_predecessors(&*store, task_id))
        }
        "tasks_in_progress" => answer(TaskReads::tasks_in_progress(&*store)),
        "task_fenced" => {
            let (task,): (TaskId,) = read(op, args)?;
            answer(TaskReads::task_fenced(&*store, task))
        }
        "list_goals" => answer(TaskReads::list_goals(&*store)),
        "active_runs" => answer(RunReads::active_runs(&*store)),
        "all_runs" => answer(RunReads::all_runs(&*store)),
        "run" => {
            let (id,): (RunId,) = read(op, args)?;
            answer(RunReads::run(&*store, &id))
        }
        "runs_with_status" => {
            let (status,): (RunStatus,) = read(op, args)?;
            answer(RunReads::runs_with_status(&*store, status))
        }
        "next_awaiting_integration" => answer(RunReads::next_awaiting_integration(&*store)),
        "ended_run_workspaces" => answer(RunReads::ended_run_workspaces(&*store)),
        "ended_run_worktrees" => answer(RunReads::ended_run_worktrees(&*store)),
        "ended_run_worktree" => {
            let (id,): (RunId,) = read(op, args)?;
            answer(RunReads::ended_run_worktree(&*store, &id))
        }
        "latest_runs_in_progress" => answer(RunReads::latest_runs_in_progress(&*store)),
        "runs_with_pending_push" => answer(RunReads::runs_with_pending_push(&*store)),
        "run_in_workspace" => {
            let (workspace_id,): (String,) = read(op, args)?;
            answer(RunReads::run_in_workspace(&*store, workspace_id.as_str()))
        }
        "claim_for_supervisor" => {
            let (base_commit, token): (CommitSha, LeaseToken) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::claim_for_supervisor(
                &mut *store,
                &base_commit,
                &token,
            ))
        }
        "abandon_run" => {
            let (id, token, message, reason, session): (
                RunId,
                LeaseToken,
                String,
                Reason,
                Option<serde_json::Value>,
            ) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::abandon_run(
                &mut *store,
                &id,
                &token,
                message.as_str(),
                &reason,
                session.as_ref(),
            ))
        }
        "record_cleanup_failure" => {
            let (id, message, reason): (RunId, String, Reason) = read(op, args)?;
            answer(RunTransitions::record_cleanup_failure(
                &mut *store,
                &id,
                message.as_str(),
                &reason,
            ))
        }
        "replan_guard" => {
            let (task,): (crate::domain::TaskId,) = read(op, args)?;
            answer(RunTransitions::replan_guard(&*store, task))
        }
        "turn_fence" => {
            let (task,): (crate::domain::TaskId,) = read(op, args)?;
            answer(RunTransitions::turn_fence(&*store, task))
        }
        "settling_holds" => answer(RunTransitions::settling_holds(&*store)),
        "remind_task_holds" => answer(RunTransitions::remind_task_holds(&mut *store)),
        "open_hold" => {
            let (task,): (crate::domain::TaskId,) = read(op, args)?;
            answer(RunTransitions::open_hold(&*store, task))
        }
        "latest_run_of" => {
            let (task,): (crate::domain::TaskId,) = read(op, args)?;
            answer(RunTransitions::latest_run_of(&*store, task))
        }
        "park_for_hold" => {
            let (hold, id, token, reason, payload): (
                crate::domain::HoldId,
                RunId,
                LeaseToken,
                String,
                serde_json::Value,
            ) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::park_for_hold(
                &mut *store,
                hold,
                &id,
                &token,
                reason.as_str(),
                payload,
            ))
        }
        "settle_hold_at_rest" => {
            let (hold, id, status): (crate::domain::HoldId, RunId, RunStatus) = read(op, args)?;
            answer(RunTransitions::settle_hold_at_rest(
                &mut *store,
                hold,
                &id,
                status,
            ))
        }
        "release_for_hold" => {
            let (hold, id, token): (crate::domain::HoldId, RunId, LeaseToken) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::release_for_hold(
                &mut *store,
                hold,
                &id,
                &token,
            ))
        }
        "parking_replans" => answer(RunTransitions::parking_replans(&*store)),
        "replans_of_run" => {
            let (run,): (RunId,) = read(op, args)?;
            answer(RunTransitions::replans_of_run(&*store, &run))
        }
        "open_replan_operations" => answer(RunTransitions::open_replan_operations(&*store)),
        "replan_lineage" => {
            let (task,): (TaskId,) = read(op, args)?;
            answer(RunTransitions::replan_lineage(&*store, task))
        }
        "ask_replan_diagnosis" => {
            let (ask, replan, reason, run, payload, premise): (
                NewAsk,
                (crate::domain::ReplanId, i64),
                String,
                RunId,
                Value,
                Option<crate::domain::task_replan::DiagnosisPremise>,
            ) = read(op, args)?;
            answer(RunTransitions::ask_replan_diagnosis(
                &mut *store,
                &ask,
                replan,
                &reason,
                &run,
                (payload, premise.as_ref()),
            ))
        }
        "detect_replan" => {
            let (new, by_id): (crate::domain::task_replan::NewReplan, String) = read(op, args)?;
            answer(RunTransitions::detect_replan(&mut *store, &new, &by_id))
        }
        "move_replan" => {
            let (id, generation, step): (
                crate::domain::ReplanId,
                i64,
                crate::domain::task_replan::ReplanMove,
            ) = read(op, args)?;
            answer(RunTransitions::move_replan(
                &mut *store,
                id,
                generation,
                &step,
            ))
        }
        "settle_replan" => {
            let (op, generation, step, token, payload): (
                crate::domain::ReplanId,
                i64,
                crate::domain::task_replan::ReplanMove,
                Option<LeaseToken>,
                serde_json::Value,
            ) = read(op, args)?;
            if let Some(token) = &token {
                own(token)?;
            }
            answer(RunTransitions::settle_replan(
                &mut *store,
                op,
                generation,
                &step,
                token.as_ref(),
                payload,
            ))
        }
        "rest_fenced" => {
            let (id, token, reason, payload): (RunId, LeaseToken, String, serde_json::Value) =
                read(op, args)?;
            own(&token)?;
            answer(RunTransitions::rest_fenced(
                &mut *store,
                &id,
                &token,
                reason.as_str(),
                payload,
            ))
        }
        "lease_for_park" => {
            let (id, token, op, generation): (RunId, LeaseToken, crate::domain::ReplanId, i64) =
                read(op, args)?;
            own(&token)?;
            answer(RunTransitions::lease_for_park(
                &mut *store,
                &id,
                &token,
                op,
                generation,
            ))
        }
        "lease_for_hold_park" => {
            let (id, token, hold): (RunId, LeaseToken, crate::domain::HoldId) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::lease_for_hold_park(
                &mut *store,
                &id,
                &token,
                hold,
            ))
        }
        "begin_restore" => {
            let (id, token, op, generation): (RunId, LeaseToken, crate::domain::ReplanId, i64) =
                read(op, args)?;
            own(&token)?;
            answer(RunTransitions::begin_restore(
                &mut *store,
                &id,
                &token,
                op,
                generation,
            ))
        }
        "finish_restore" => {
            let (id, token, op, generation, restored): (
                RunId,
                LeaseToken,
                crate::domain::ReplanId,
                i64,
                std::result::Result<serde_json::Value, String>,
            ) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::finish_restore(
                &mut *store,
                &id,
                &token,
                op,
                generation,
                restored,
            ))
        }
        "record_clone_restore" => {
            let (id, token, op, generation, restored): (
                RunId,
                LeaseToken,
                crate::domain::ReplanId,
                i64,
                std::result::Result<serde_json::Value, String>,
            ) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::record_clone_restore(
                &mut *store,
                &id,
                &token,
                op,
                generation,
                restored,
            ))
        }
        "workspace_closed" => {
            let (id, token): (RunId, LeaseToken) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::workspace_closed(&mut *store, &id, &token))
        }
        "cleanup_failed" => {
            let (id, token, message, reason): (RunId, LeaseToken, String, Reason) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::cleanup_failed(
                &mut *store,
                &id,
                &token,
                message.as_str(),
                &reason,
            ))
        }
        "claim_for_supervisor_holding" => {
            let (base_commit, token, order, held, attributes, trial, routes): (
                CommitSha,
                LeaseToken,
                Vec<TaskId>,
                crate::domain::claim_order::ClaimHolds,
                Option<serde_json::Value>,
                crate::domain::worker_model::WorkerTrial,
                Vec<crate::domain::provider_switch::WorkerRoute>,
            ) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::claim_for_supervisor_holding(
                &mut *store,
                &base_commit,
                &token,
                &order,
                &held,
                attributes.as_ref(),
                &trial,
                &routes,
            ))
        }
        "switch_provider" => {
            let (id, token, worker, payload): (
                RunId,
                LeaseToken,
                crate::domain::worker::Worker,
                serde_json::Value,
            ) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::switch_provider(
                &mut *store,
                &id,
                &token,
                worker,
                payload,
            ))
        }
        "record_runtime_error" => {
            let (id, message, reason): (RunId, String, Reason) = read(op, args)?;
            answer(RunTransitions::record_runtime_error(
                &mut *store,
                &id,
                message.as_str(),
                &reason,
            ))
        }
        "plan_run" => {
            let (id, token, plan): (RunId, LeaseToken, RunPlan) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::plan_run(&mut *store, &id, &token, &plan))
        }
        "workspace_created" => {
            let (id, token, workspace): (RunId, LeaseToken, String) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::workspace_created(
                &mut *store,
                &id,
                &token,
                workspace.as_str(),
            ))
        }
        "finish_supervision" => {
            let (id, token, receipt): (RunId, LeaseToken, bool) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::finish_supervision(
                &mut *store,
                &id,
                &token,
                receipt,
            ))
        }
        "finish_lost_session" => {
            let (id, token, exit_code, receipt): (RunId, LeaseToken, i32, bool) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::finish_lost_session(
                &mut *store,
                &id,
                &token,
                exit_code,
                receipt,
            ))
        }
        "finish_supervision_live" => {
            let (id, token): (RunId, LeaseToken) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::finish_supervision_live(
                &mut *store,
                &id,
                &token,
            ))
        }
        "finish_validation" => {
            let (id, token, validation): (RunId, LeaseToken, Validation) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::finish_validation(
                &mut *store,
                &id,
                &token,
                &validation,
            ))
        }
        "restart_validation" => {
            let (id, token): (RunId, LeaseToken) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::restart_validation(&mut *store, &id, &token))
        }
        "park_live" => {
            let (id, token, reason, payload): (RunId, LeaseToken, String, serde_json::Value) =
                read(op, args)?;
            own(&token)?;
            answer(RunTransitions::park_live(
                &mut *store,
                &id,
                &token,
                reason.as_str(),
                payload,
            ))
        }
        "park_e2e_failed" => {
            let (id, token, reason, payload): (RunId, LeaseToken, String, serde_json::Value) =
                read(op, args)?;
            own(&token)?;
            answer(RunTransitions::park_e2e_failed(
                &mut *store,
                &id,
                &token,
                reason.as_str(),
                payload,
            ))
        }
        "park_rechecked" => {
            let (id, token, reason, payload): (
                RunId,
                Option<LeaseToken>,
                String,
                serde_json::Value,
            ) = read(op, args)?;
            if let Some(token) = &token {
                own(token)?;
            }
            answer(RunTransitions::park_rechecked(
                &mut *store,
                &id,
                token.as_ref(),
                reason.as_str(),
                payload,
            ))
        }
        "record_workspace_closed" => {
            let (id, workspace_id, payload): (RunId, String, serde_json::Value) = read(op, args)?;
            answer(RunTransitions::record_workspace_closed(
                &mut *store,
                &id,
                workspace_id.as_str(),
                payload,
            ))
        }
        "runs_leased_by_others" => {
            let (token,): (LeaseToken,) = read(op, args)?;
            own(&token)?;
            answer(RunRecovery::runs_leased_by_others(&*store, &token))
        }
        "runs_leased_by" => {
            let (token,): (LeaseToken,) = read(op, args)?;
            own(&token)?;
            answer(RunRecovery::runs_leased_by(&*store, &token))
        }
        "unclosed_run_asks" => {
            let (run_id,): (RunId,) = read(op, args)?;
            answer(RunRecovery::unclosed_run_asks(&*store, &run_id))
        }
        "adopt_run" => {
            let (id, previous_token, token, pid, wrapper): (
                RunId,
                LeaseToken,
                LeaseToken,
                u32,
                serde_json::Value,
            ) = read(op, args)?;
            own(&token)?;
            answer(RunRecovery::adopt_run(
                &mut *store,
                &id,
                &previous_token,
                &token,
                pid,
                wrapper,
            ))
        }
        "lease_for_review" => {
            let (id, token): (RunId, LeaseToken) = read(op, args)?;
            own(&token)?;
            answer(RunRecovery::lease_for_review(&mut *store, &id, &token))
        }
        "begin_e2e" => {
            let (id, token, payload, over_handoff): (RunId, LeaseToken, serde_json::Value, bool) =
                read(op, args)?;
            own(&token)?;
            answer(if over_handoff {
                RunTransitions::begin_e2e_over_handoff(&mut *store, &id, &token, payload)
            } else {
                RunTransitions::begin_e2e(&mut *store, &id, &token, payload)
            })
        }
        "lease_for_e2e" => {
            let (id, token): (RunId, LeaseToken) = read(op, args)?;
            own(&token)?;
            answer(RunRecovery::lease_for_e2e(&mut *store, &id, &token))
        }
        "lease_stranded_landing" => {
            let (id, token): (RunId, LeaseToken) = read(op, args)?;
            own(&token)?;
            answer(RunRecovery::lease_stranded_landing(
                &mut *store,
                &id,
                &token,
            ))
        }
        "lease_satisfied" => {
            let (id, token): (RunId, LeaseToken) = read(op, args)?;
            own(&token)?;
            answer(RunRecovery::lease_satisfied(&mut *store, &id, &token))
        }
        "recover_run" => {
            let (id, checked_processes, report): (RunId, usize, serde_json::Value) =
                read(op, args)?;
            answer(RunRecovery::recover_run(
                &mut *store,
                &id,
                checked_processes,
                report,
            ))
        }
        "runs_needing_session" => answer(RunRecovery::runs_needing_session(&*store)),
        "begin_resume" => {
            let (id, token, main, reason, config): (
                RunId,
                LeaseToken,
                CommitSha,
                Option<String>,
                crate::domain::resume::ResumeConfig,
            ) = read(op, args)?;
            own(&token)?;
            answer(RunRecovery::begin_resume(
                &mut *store,
                &id,
                &token,
                &main,
                reason.as_deref(),
                config,
            ))
        }
        "finish_resume" => {
            let (id, token, status, reason, keep_lease, payload): (
                RunId,
                LeaseToken,
                Option<RunStatus>,
                Option<String>,
                bool,
                serde_json::Value,
            ) = read(op, args)?;
            own(&token)?;
            answer(RunRecovery::finish_resume(
                &mut *store,
                &id,
                &token,
                status,
                reason.as_deref(),
                keep_lease,
                payload,
            ))
        }
        "finish_approved_resume" => {
            let (id, token, head, payload): (RunId, LeaseToken, CommitSha, serde_json::Value) =
                read(op, args)?;
            own(&token)?;
            answer(RunRecovery::finish_approved_resume(
                &mut *store,
                &id,
                &token,
                &head,
                payload,
            ))
        }
        "skip_resume" => {
            let (id, token, head, main, approved): (RunId, LeaseToken, CommitSha, CommitSha, bool) =
                read(op, args)?;
            own(&token)?;
            answer(RunRecovery::skip_resume(
                &mut *store,
                &id,
                &token,
                &head,
                &main,
                approved,
            ))
        }
        "exhaust_resumes" => {
            let (id, exhaustion, reason, config): (
                RunId,
                Exhaustion,
                String,
                crate::domain::resume::ResumeConfig,
            ) = read(op, args)?;
            answer(RunRecovery::exhaust_resumes(
                &mut *store,
                &id,
                &exhaustion,
                reason.as_str(),
                config,
            ))
        }
        "hold_guard" => {
            let (task,): (TaskId,) = read(op, args)?;
            answer(RunRecovery::hold_guard(&*store, task))
        }
        "held_claims" => answer(RunRecovery::held_claims(&*store)),
        "note_hold_deferred" => {
            let (hold, run, step, supervisor): (
                crate::domain::task_hold::HoldGuard,
                Option<RunId>,
                crate::domain::task_replan::GuardedStep,
                LeaseToken,
            ) = read(op, args)?;
            own(&supervisor)?;
            answer(RunRecovery::note_hold_deferred(
                &mut *store,
                hold,
                run.as_ref(),
                step,
                &supervisor,
            ))
        }
        "end_holds_of_ended_tasks" => answer(RunRecovery::end_holds_of_ended_tasks(&mut *store)),
        "holds_lease" => {
            let (id, token): (RunId, LeaseToken) = read(op, args)?;
            own(&token)?;
            answer(CoordinationReads::holds_lease(&*store, &id, &token))
        }
        "run_leases" => answer(CoordinationReads::run_leases(&*store)),
        "run_lease" => {
            let (id,): (RunId,) = read(op, args)?;
            answer(CoordinationReads::run_lease(&*store, &id))
        }
        "processes" => {
            let (id,): (RunId,) = read(op, args)?;
            answer(CoordinationReads::processes(&*store, &id))
        }
        "backend_slots" => {
            let (token,): (Option<LeaseToken>,) = read(op, args)?;
            if let Some(token) = &token {
                own(token)?;
            }
            answer(CoordinationReads::backend_slots(&*store, token.as_ref()))
        }
        "heartbeat_leases" => {
            let (token,): (LeaseToken,) = read(op, args)?;
            own(&token)?;
            answer(RunCoordination::heartbeat_leases(&*store, &token))
        }
        "release_lease" => {
            let (id, token): (RunId, LeaseToken) = read(op, args)?;
            own(&token)?;
            answer(RunCoordination::release_lease(&mut *store, &id, &token))
        }
        "heartbeat" => {
            let (token,): (LeaseToken,) = read(op, args)?;
            own(&token)?;
            answer(RunCoordination::heartbeat(&mut *store, &token))
        }
        "register_wrapper" => {
            let (id, token, pid): (RunId, LeaseToken, u32) = read(op, args)?;
            own(&token)?;
            answer(RunCoordination::register_wrapper(
                &mut *store,
                &id,
                &token,
                pid,
            ))
        }
        "register_resume_wrapper" => {
            let (id, token, pid): (RunId, LeaseToken, u32) = read(op, args)?;
            own(&token)?;
            answer(RunCoordination::register_resume_wrapper(
                &mut *store,
                &id,
                &token,
                pid,
            ))
        }
        "clear_lost_session" => {
            let (id, token, lost): (RunId, LeaseToken, Option<u32>) = read(op, args)?;
            own(&token)?;
            answer(RunCoordination::clear_lost_session(
                &mut *store,
                &id,
                &token,
                lost,
            ))
        }
        "session_reopened" => {
            let (id, token, workspace, attempt, repaired): (
                RunId,
                LeaseToken,
                String,
                u64,
                serde_json::Value,
            ) = read(op, args)?;
            own(&token)?;
            answer(RunCoordination::session_reopened(
                &mut *store,
                &id,
                &token,
                workspace.as_str(),
                attempt,
                repaired,
            ))
        }
        "register_agent" => {
            let (id, wrapper_pid, agent_pid): (RunId, u32, u32) = read(op, args)?;
            answer(RunCoordination::register_agent(
                &mut *store,
                &id,
                wrapper_pid,
                agent_pid,
            ))
        }
        "register_resume_agent" => {
            let (id, wrapper_pid, agent_pid): (RunId, u32, u32) = read(op, args)?;
            answer(RunCoordination::register_resume_agent(
                &mut *store,
                &id,
                wrapper_pid,
                agent_pid,
            ))
        }
        "register_turn_agent" => {
            let (id, wrapper_pid, agent_pid): (RunId, u32, u32) = read(op, args)?;
            answer(RunCoordination::register_turn_agent(
                &mut *store,
                &id,
                wrapper_pid,
                agent_pid,
            ))
        }
        "heartbeat_wrapper" => {
            let (id, pid): (RunId, u32) = read(op, args)?;
            answer(RunCoordination::heartbeat_wrapper(&*store, &id, pid))
        }
        "wrapper_exited" => {
            let (id, pid, exit_code): (RunId, u32, i32) = read(op, args)?;
            answer(RunCoordination::wrapper_exited(
                &mut *store,
                &id,
                pid,
                exit_code,
            ))
        }
        "supervisors" => answer(SupervisorReads::supervisors(&*store)),
        "handoff_request" => {
            let (token,): (LeaseToken,) = read(op, args)?;
            own(&token)?;
            answer(SupervisorReads::handoff_request(&*store, &token))
        }
        "repository_binding" => answer(SupervisorReads::repository_binding(&*store)),
        "register_supervisor" => {
            let (token, pid, limits, providers, binary_version): (
                LeaseToken,
                u32,
                crate::domain::slot_limits::SlotLimits,
                Vec<crate::domain::worker::ProviderCheck>,
                String,
            ) = read(op, args)?;
            own(&token)?;
            answer(SupervisorRegistry::register_supervisor(
                &mut *store,
                &token,
                pid,
                limits,
                &providers,
                binary_version.as_str(),
            ))
        }
        "accept_handoff" => {
            let (token,): (LeaseToken,) = read(op, args)?;
            own(&token)?;
            answer(SupervisorRegistry::accept_handoff(&*store, &token))
        }
        "request_handoff" => {
            let (token, binary): (LeaseToken, String) = read(op, args)?;
            own(&token)?;
            answer(SupervisorRegistry::request_handoff(
                &*store,
                &token,
                binary.as_str(),
            ))
        }
        "take_handoff" => {
            let (token, binary): (LeaseToken, String) = read(op, args)?;
            own(&token)?;
            answer(SupervisorRegistry::take_handoff(
                &*store,
                &token,
                binary.as_str(),
            ))
        }
        "cancel_handoff" => {
            let (token, binary): (LeaseToken, String) = read(op, args)?;
            own(&token)?;
            answer(SupervisorRegistry::cancel_handoff(
                &*store,
                &token,
                binary.as_str(),
            ))
        }
        "resume_registration" => {
            let (token, pid, limits, providers, binary_version): (
                LeaseToken,
                u32,
                crate::domain::slot_limits::SlotLimits,
                Vec<crate::domain::worker::ProviderCheck>,
                String,
            ) = read(op, args)?;
            own(&token)?;
            answer(SupervisorRegistry::resume_registration(
                &mut *store,
                &token,
                pid,
                limits,
                &providers,
                binary_version.as_str(),
            ))
        }
        "set_auto_update" => {
            let (token, enabled): (LeaseToken, bool) = read(op, args)?;
            own(&token)?;
            answer(SupervisorRegistry::set_auto_update(
                &*store, &token, enabled,
            ))
        }
        "set_slot_limits" => {
            let (token, limits): (LeaseToken, crate::domain::slot_limits::SlotLimits) =
                read(op, args)?;
            own(&token)?;
            answer(SupervisorRegistry::set_slot_limits(&*store, &token, limits))
        }
        "set_max_load" => {
            let (token, max_load, source) = max_load_args(op, args)?;
            own(&token)?;
            answer(SupervisorRegistry::set_max_load(
                &*store, &token, max_load, source,
            ))
        }
        "rebind_repository" => {
            let (common_dir,): (String,) = read(op, args)?;
            answer(SupervisorRegistry::rebind_repository(
                &mut *store,
                common_dir.as_str(),
            ))
        }
        "bind_repository" => {
            let (common_dir,): (String,) = read(op, args)?;
            answer(SupervisorRegistry::bind_repository(
                &mut *store,
                common_dir.as_str(),
            ))
        }
        "assert_repository" => {
            let (common_dir,): (String,) = read(op, args)?;
            answer(SupervisorRegistry::assert_repository(
                &*store,
                common_dir.as_str(),
            ))
        }
        "set_supervisor_mode" => {
            let (token, mode, workspace_id): (LeaseToken, SupervisorMode, Option<String>) =
                read(op, args)?;
            own(&token)?;
            answer(SupervisorRegistry::set_supervisor_mode(
                &*store,
                &token,
                mode,
                workspace_id.as_deref(),
            ))
        }
        "update_events" => {
            let (limit,): (usize,) = read(op, args)?;
            answer(EventStore::update_events(&*store, limit))
        }
        "installs_after" => {
            let (after, limit): (EventId, usize) = read(op, args)?;
            answer(EventStore::installs_after(&*store, after, limit))
        }
        "e2e_gate_events" => {
            let (limit,): (usize,) = read(op, args)?;
            answer(EventStore::e2e_gate_events(&*store, limit))
        }
        "latest_task_events" => {
            let (kinds,): (Vec<String>,) = read(op, args)?;
            answer(EventStore::latest_task_events(
                &*store,
                &kinds.iter().map(String::as_str).collect::<Vec<_>>(),
            ))
        }
        "run_events" => {
            let (id,): (RunId,) = read(op, args)?;
            answer(EventStore::run_events(&*store, &id))
        }
        "task_events" => {
            let (task,): (TaskId,) = read(op, args)?;
            answer(EventStore::task_events(&*store, task))
        }
        "has_run_event" => {
            let (id, kind): (RunId, String) = read(op, args)?;
            answer(EventStore::has_run_event(&*store, &id, kind.as_str()))
        }
        "record_runtime_event" => {
            let (id, kind, payload): (RunId, EventKind, serde_json::Value) = read(op, args)?;
            answer(EventStore::record_runtime_event(
                &*store, &id, kind, payload,
            ))
        }
        "last_observe" => {
            let (mode,): (String,) = read(op, args)?;
            answer(EventStore::last_observe(&*store, mode.as_str()))
        }
        "latest_event_id" => answer(EventStore::latest_event_id(&*store)),
        "record_backend_failure" => {
            let (run, payload): (Option<RunId>, serde_json::Value) = read(op, args)?;
            answer(EventStore::record_backend_failure(
                &*store,
                run.as_ref(),
                payload,
            ))
        }
        "record_queue_event" => {
            let (kind, payload): (EventKind, serde_json::Value) = read(op, args)?;
            answer(EventStore::record_queue_event(&*store, kind, payload))
        }
        "record_queue_events" => {
            let (events,): (Vec<(EventKind, serde_json::Value)>,) = read(op, args)?;
            answer(EventStore::record_queue_events(&*store, events))
        }
        "latest_event_of" => {
            let (kind,): (String,) = read(op, args)?;
            answer(EventStore::latest_event_of(&*store, kind.as_str()))
        }
        "latest_events_of" => {
            let (kind, limit): (String, usize) = read(op, args)?;
            answer(EventStore::latest_events_of(&*store, kind.as_str(), limit))
        }
        "latest_queue_event" => {
            let (kinds,): (Vec<String>,) = read(op, args)?;
            answer(EventStore::latest_queue_event(
                &*store,
                &kinds.iter().map(String::as_str).collect::<Vec<_>>(),
            ))
        }
        "latest_events_by_supervisor" => {
            let (kinds, supervisors): (Vec<String>, Vec<String>) = read(op, args)?;
            answer(EventStore::latest_events_by_supervisor(
                &*store,
                &kinds.iter().map(String::as_str).collect::<Vec<_>>(),
                &supervisors.iter().map(String::as_str).collect::<Vec<_>>(),
            ))
        }
        "events_of_between" => {
            let (kinds, after, upto, limit): (Vec<String>, EventId, EventId, usize) =
                read(op, args)?;
            answer(EventStore::events_of_between(
                &*store,
                &kinds.iter().map(String::as_str).collect::<Vec<_>>(),
                after,
                upto,
                limit,
            ))
        }
        "events_between" => {
            let (after, upto, filter, limit): (EventId, EventId, EventFilter, usize) =
                read(op, args)?;
            answer(EventStore::events_between(
                &*store, after, upto, &filter, limit,
            ))
        }
        "show" => {
            let (task_id,): (TaskId,) = read(op, args)?;
            answer(TaskReads::show(&mut *store, task_id))
        }
        "graph_input" => answer(TaskReads::graph_input(&*store)),
        "ready_at" => {
            let (tasks,): (Vec<TaskId>,) = read(op, args)?;
            answer(TaskStore::ready_at(&*store, &tasks))
        }
        "investigations" => answer(TaskStore::investigations(&mut *store)),
        "prune_supervisor" => {
            let (token, kind, stopped): (LeaseToken, EventKind, Value) = read(op, args)?;
            // Its own registration, or another's whose heartbeat expired,
            // which a live supervisor prunes as left behind (whether its
            // pid still runs it is the supervisor's judgement on the host).
            if own(&token).is_err() {
                let expired = SupervisorReads::supervisors(&*store)?
                    .iter()
                    .find(|registration| registration.token == token)
                    .is_none_or(|registration| {
                        supervisor_prune::heartbeat_expired(registration, now)
                    });
                if !expired {
                    own(&token)?;
                }
            }
            answer(SupervisorRegistry::prune_supervisor(
                &*store,
                &token,
                kind,
                &|_| stopped.clone(),
            ))
        }
        "list" => {
            let (query,): (TaskQuery,) = read(op, args)?;
            answer(TaskReads::list(&*store, &query))
        }
        "show_goal" => {
            let (goal_id,): (GoalId,) = read(op, args)?;
            answer(TaskReads::show_goal(&mut *store, goal_id))
        }
        "show_proposal" => {
            let (proposal_id,): (ProposalId,) = read(op, args)?;
            answer(TaskReads::show_proposal(&*store, proposal_id))
        }
        "proposals" => {
            let (all,): (bool,) = read(op, args)?;
            answer(TaskReads::proposals(&*store, all))
        }
        "lint_input" => {
            let (tasks,): (Vec<TaskId>,) = read(op, args)?;
            answer(TaskReads::lint_input(&*store, &tasks))
        }
        "notes" => {
            let (query,): (NoteQuery,) = read(op, args)?;
            answer(TaskReads::notes(&*store, &query))
        }
        "cancel" => {
            let (task_id, cancel): (TaskId, NewCancel) = read(op, args)?;
            answer(TaskStore::cancel(&mut *store, task_id, cancel))
        }
        "submit" => {
            let (submission,): (Submission,) = read(op, args)?;
            answer(TaskStore::submit(&mut *store, submission))
        }
        "related_landed_commits" => {
            let (task, limit): (TaskId, usize) = read(op, args)?;
            answer(PlanningRecords::related_landed_commits(
                &*store, task, limit,
            ))
        }
        "related_tasks" => {
            let (task, statuses, limit): (TaskId, Vec<String>, usize) = read(op, args)?;
            answer(PlanningRecords::related_tasks(
                &*store, task, &statuses, limit,
            ))
        }
        "search_documents" => {
            let (query,): (SearchQuery,) = read(op, args)?;
            answer(PlanningRecords::search_documents(&*store, &query))
        }
        "task_goals" => answer(PlanningRecords::task_goals(&*store)),
        "task_titles" => answer(PlanningRecords::task_titles(&*store)),
        "task_changes" => answer(PlanningRecords::task_changes(&*store)),
        "draft_origins" => answer(PlanningRecords::draft_origins(&*store)),
        "planner_requests" => answer(PlanRequestStore::planner_requests(&*store)),
        "open_request_planner" => {
            let (request, answer_): (crate::domain::RequestId, Option<AskId>) = read(op, args)?;
            answer(PlanRequestStore::open_request_planner(
                &mut *store,
                request,
                answer_,
            ))
        }
        "plan_request" => {
            let (request,): (crate::domain::RequestId,) = read(op, args)?;
            answer(PlanRequestStore::plan_request(&*store, request))
        }
        "request_asks" => {
            let (request,): (crate::domain::RequestId,) = read(op, args)?;
            answer(PlanRequestStore::request_asks(&*store, request))
        }
        "event_by_id" => {
            let (id,): (EventId,) = read(op, args)?;
            answer(PlanRequestStore::event_by_id(&*store, id))
        }
        "replan_brief" => {
            let (id,): (crate::domain::ReplanId,) = read(op, args)?;
            answer(PlanRequestStore::replan_brief(&*store, id))
        }
        "replacement_brief" => {
            let (proposal,): (ProposalId,) = read(op, args)?;
            answer(PlanRequestStore::replacement_brief(&*store, proposal))
        }
        "settle_replan_requests" => answer(PlanRequestStore::settle_replan_requests(&mut *store)),
        "replacements_to_apply" => answer(PlanRequestStore::replacements_to_apply(&*store)),
        "begin_replan_apply" => {
            let (id, generation): (crate::domain::ReplanId, i64) = read(op, args)?;
            answer(PlanRequestStore::begin_replan_apply(
                &mut *store,
                id,
                generation,
            ))
        }
        "apply_replacement" => {
            let (id, generation, problem): (crate::domain::ReplanId, i64, Option<String>) =
                read(op, args)?;
            answer(PlanRequestStore::apply_replacement(
                &mut *store,
                id,
                generation,
                problem.as_deref(),
            ))
        }
        "child_carry" => {
            let (task,): (TaskId,) = read(op, args)?;
            answer(PlanRequestStore::child_carry(&*store, task))
        }
        "planner_findings" => answer(DraftPlannerReads::planner_findings(&*store)),
        "improvements" => {
            let (limit,): (usize,) = read(op, args)?;
            answer(DraftPlannerReads::improvements(&*store, limit))
        }
        "register_follow_ups" => {
            let (run, entries, depth): (RunId, Vec<FollowUpRegistration>, i64) = read(op, args)?;
            answer(DraftPlannerStore::register_follow_ups(
                &mut *store,
                &run,
                entries,
                depth,
            ))
        }
        "record_draft_origin" => {
            let (task, origin, material): (TaskId, DraftOrigin, serde_json::Value) =
                read(op, args)?;
            answer(DraftPlannerStore::record_draft_origin(
                &mut *store,
                task,
                origin,
                &material,
            ))
        }
        "register_recurring_draft" => {
            let (item,): (crate::domain::recurring::RecurringDraft,) = read(op, args)?;
            answer(DraftPlannerStore::register_recurring_draft(
                &mut *store,
                &item,
            ))
        }
        "draft_origin" => {
            let (task,): (TaskId,) = read(op, args)?;
            answer(DraftPlannerStore::draft_origin(&*store, task))
        }
        "planner_drafts" => answer(DraftPlannerStore::planner_drafts(&*store)),
        "open_draft_planner" => {
            let (drafts, answer_): (Vec<TaskId>, Option<AskId>) = read(op, args)?;
            answer(DraftPlannerStore::open_draft_planner(
                &mut *store,
                &drafts,
                answer_,
            ))
        }
        "planner_draft_tasks" => {
            let (planner,): (PlannerId,) = read(op, args)?;
            answer(DraftPlannerStore::planner_draft_tasks(&*store, planner))
        }
        "draft_bundle" => {
            let (planner,): (PlannerId,) = read(op, args)?;
            answer(DraftPlannerStore::draft_bundle(&*store, planner))
        }
        "planner_answers" => answer(DraftPlannerStore::planner_answers(&*store)),
        "planner_answer_route" => {
            let (ask,): (Ask,) = read(op, args)?;
            answer(DraftPlannerStore::planner_answer_route(&*store, &ask))
        }
        "proposal_questions" => {
            let (proposal,): (ProposalId,) = read(op, args)?;
            answer(DraftPlannerStore::proposal_questions(&*store, proposal))
        }
        "replaced_tasks" => {
            let (proposal,): (ProposalId,) = read(op, args)?;
            answer(DraftPlannerStore::replaced_tasks(&*store, proposal))
        }
        "answer_request" => {
            let (ask,): (Ask,) = read(op, args)?;
            answer(DraftPlannerStore::answer_request(&*store, &ask))
        }
        "claim_planner_answer" => {
            let (ask, planner, workspace): (AskId, PlannerId, String) = read(op, args)?;
            answer(DraftPlannerStore::claim_planner_answer(
                &mut *store,
                ask,
                planner,
                workspace.as_str(),
            ))
        }
        "close_planner_answer" => {
            let (ask, why): (AskId, String) = read(op, args)?;
            answer(DraftPlannerStore::close_planner_answer(
                &mut *store,
                ask,
                why.as_str(),
            ))
        }
        "record_task_event" => {
            let (task, kind, payload): (TaskId, EventKind, serde_json::Value) = read(op, args)?;
            answer(DraftPlannerStore::record_task_event(
                &mut *store,
                task,
                kind,
                payload,
            ))
        }
        "exhausted_drafts" => answer(DraftPlannerStore::exhausted_drafts(&*store)),
        "follow_up_depth" => {
            let (task,): (TaskId,) = read(op, args)?;
            answer(DraftPlannerStore::follow_up_depth(&*store, task))
        }
        "set_follow_up_depth" => {
            let (task, depth): (TaskId, i64) = read(op, args)?;
            answer(DraftPlannerStore::set_follow_up_depth(
                &mut *store,
                task,
                depth,
            ))
        }
        "open_finding_planner" => {
            let (finding, answer_, limit): (FindingId, Option<AskId>, usize) = read(op, args)?;
            answer(DraftPlannerStore::open_finding_planner(
                &mut *store,
                finding,
                answer_,
                limit,
            ))
        }
        "finding_view" => {
            let (finding,): (FindingId,) = read(op, args)?;
            answer(DraftPlannerStore::finding_view(&*store, finding))
        }
        "finding_asks" => {
            let (finding,): (FindingId,) = read(op, args)?;
            answer(DraftPlannerStore::finding_asks(&*store, finding))
        }
        "settle_findings" => answer(DraftPlannerStore::settle_findings(&mut *store)),
        "exhausted_findings" => answer(DraftPlannerStore::exhausted_findings(&*store)),
        "record_finding_event" => {
            let (finding, kind, payload): (FindingId, EventKind, serde_json::Value) =
                read(op, args)?;
            answer(DraftPlannerStore::record_finding_event(
                &mut *store,
                finding,
                kind,
                payload,
            ))
        }
        "ask_delivered_to" => {
            let (ask, workspace): (AskId, String) = read(op, args)?;
            answer(DraftPlannerStore::ask_delivered_to(
                &*store,
                ask,
                workspace.as_str(),
            ))
        }
        "answer_claimed_at" => {
            let (ask, workspace): (AskId, String) = read(op, args)?;
            answer(DraftPlannerStore::answer_claimed_at(
                &*store,
                ask,
                workspace.as_str(),
            ))
        }
        "ask_delivery_failed" => {
            let (ask, planner): (AskId, PlannerId) = read(op, args)?;
            answer(DraftPlannerStore::ask_delivery_failed(
                &*store, ask, planner,
            ))
        }
        "interrupt_plan_reviews_for_handoff" => {
            let (token, keep): (LeaseToken, Vec<i64>) = read(op, args)?;
            own(&token)?;
            answer(PlanReviewStore::interrupt_plan_reviews_for_handoff(
                &mut *store,
                &token,
                &keep,
            ))
        }
        "plan_review_candidates" => answer(PlanReviewStore::plan_review_candidates(&*store)),
        "begin_plan_review" => {
            let (proposal, token, plan_reviews_dir, cwd, launch): (
                ProposalId,
                LeaseToken,
                std::path::PathBuf,
                std::path::PathBuf,
                crate::domain::actor_model::ActorLaunch,
            ) = read(op, args)?;
            own(&token)?;
            answer(PlanReviewStore::begin_plan_review(
                &mut *store,
                proposal,
                &token,
                &plan_reviews_dir,
                &cwd,
                &launch,
                alive,
            ))
        }
        "take_over_plan_review" => {
            let (job, token): (PlanReviewJob, LeaseToken) = read(op, args)?;
            own(&token)?;
            answer(PlanReviewStore::take_over_plan_review(
                &mut *store,
                &job,
                &token,
                alive,
            ))
        }
        "finish_plan_review" => {
            let (job, token, apply): (PlanReviewJob, LeaseToken, PlanReviewApply) = read(op, args)?;
            own(&token)?;
            answer(PlanReviewStore::finish_plan_review(
                &mut *store,
                &job,
                &token,
                &apply,
            ))
        }
        "fail_plan_review" => {
            let (job, token, failure): (PlanReviewJob, LeaseToken, PlanReviewFailure) =
                read(op, args)?;
            own(&token)?;
            answer(PlanReviewStore::fail_plan_review(
                &mut *store,
                &job,
                &token,
                &failure,
            ))
        }
        "revising_proposals" => answer(PlanReviewStore::revising_proposals(&*store)),
        "plan_reviews_finished_before" => {
            let (before,): (i64,) = read(op, args)?;
            answer(PlanReviewStore::plan_reviews_finished_before(
                &*store, before,
            ))
        }
        "claim_revise" => {
            let (proposal,): (ProposalId,) = read(op, args)?;
            answer(PlanReviewStore::claim_revise(&mut *store, proposal))
        }
        "revise_goal_wait" => {
            let (proposal,): (ProposalId,) = read(op, args)?;
            answer(PlanReviewStore::revise_goal_wait(&*store, proposal))
        }
        "revise_sent" => {
            let (proposal, planner, workspace, opened, answers): (
                ProposalId,
                PlannerId,
                String,
                Option<crate::domain::actor_model::ActorLaunch>,
                Vec<AskId>,
            ) = read(op, args)?;
            answer(PlanReviewStore::revise_sent(
                &mut *store,
                proposal,
                planner,
                workspace.as_str(),
                opened.as_ref(),
                &answers,
            ))
        }
        "revise_lost" => {
            let (proposal, planner, why): (ProposalId, Option<PlannerId>, String) = read(op, args)?;
            answer(PlanReviewStore::revise_lost(
                &mut *store,
                proposal,
                planner,
                why.as_str(),
            ))
        }
        "planner_unresponsive" => {
            let (proposal, planner, waited_secs, holders): (
                ProposalId,
                Option<PlannerId>,
                i64,
                Vec<PlannerHold>,
            ) = read(op, args)?;
            answer(PlanReviewStore::planner_unresponsive(
                &mut *store,
                proposal,
                planner,
                waited_secs,
                &holders,
            ))
        }
        "settle_proposals" => answer(PlanReviewStore::settle_proposals(&mut *store)),
        "plan_answers" => answer(PlanReviewStore::plan_answers(&*store)),
        "decide_plan" => {
            let (ask,): (AskId,) = read(op, args)?;
            answer(PlanReviewStore::decide_plan(&mut *store, ask))
        }
        "applies_plan_answer" => {
            let (ask,): (Ask,) = read(op, args)?;
            answer(PlanReviewStore::applies_plan_answer(&*store, &ask))
        }
        "plan_review_holds" => answer(PlanReviewStore::plan_review_holds(&*store)),
        "stranded_dependencies" => answer(PlanReviewStore::stranded_dependencies(&*store)),
        "answered_asks" => {
            let (limit,): (usize,) = read(op, args)?;
            answer(PlanReviewStore::answered_asks(&*store, limit))
        }
        "interrupt_goal_reviews_for_handoff" => {
            let (token, keep): (LeaseToken, Vec<i64>) = read(op, args)?;
            own(&token)?;
            answer(GoalReviewStore::interrupt_goal_reviews_for_handoff(
                &mut *store,
                &token,
                &keep,
            ))
        }
        "goal_review_candidates" => answer(GoalReviewStore::goal_review_candidates(&*store)),
        "goal_replacements" => {
            let (goal,): (GoalId,) = read(op, args)?;
            answer(GoalReviewStore::goal_replacements(&*store, goal))
        }
        "unresolved_replacements" => answer(GoalReviewStore::unresolved_replacements(&*store)),
        "begin_goal_review" => {
            let (goal, token, goal_reviews_dir, cwd, launch): (
                GoalId,
                LeaseToken,
                std::path::PathBuf,
                std::path::PathBuf,
                crate::domain::actor_model::ActorLaunch,
            ) = read(op, args)?;
            own(&token)?;
            answer(GoalReviewStore::begin_goal_review(
                &mut *store,
                goal,
                &token,
                &goal_reviews_dir,
                &cwd,
                &launch,
                alive,
            ))
        }
        "goal_reviews" => {
            let (goal,): (GoalId,) = read(op, args)?;
            answer(GoalReviewStore::goal_reviews(&*store, goal))
        }
        "take_over_goal_review" => {
            let (job, token): (GoalReviewJob, LeaseToken) = read(op, args)?;
            own(&token)?;
            answer(GoalReviewStore::take_over_goal_review(
                &mut *store,
                &job,
                &token,
                alive,
            ))
        }
        "finish_goal_review" => {
            let (job, token, apply): (GoalReviewJob, LeaseToken, GoalReviewApply) = read(op, args)?;
            own(&token)?;
            answer(GoalReviewStore::finish_goal_review(
                &mut *store,
                &job,
                &token,
                &apply,
            ))
        }
        "fail_goal_review" => {
            let (job, token, failure): (GoalReviewJob, LeaseToken, GoalReviewFailure) =
                read(op, args)?;
            own(&token)?;
            answer(GoalReviewStore::fail_goal_review(
                &mut *store,
                &job,
                &token,
                &failure,
            ))
        }
        "goal_answers" => answer(GoalReviewStore::goal_answers(&*store)),
        "decide_goal" => {
            let (ask,): (AskId,) = read(op, args)?;
            answer(GoalReviewStore::decide_goal(&mut *store, ask))
        }
        "correction_answers" => answer(GoalReviewStore::correction_answers(&*store)),
        "decide_correction" => {
            let (ask,): (AskId,) = read(op, args)?;
            answer(GoalReviewStore::decide_correction(&mut *store, ask))
        }
        "goal_review_holds" => answer(GoalReviewStore::goal_review_holds(&*store)),
        "goal_follow_ups" => answer(GoalReviewStore::goal_follow_ups(&*store)),
        "rearm_goal_review" => {
            let (goal,): (GoalId,) = read(op, args)?;
            answer(GoalReviewStore::rearm_goal_review(&mut *store, goal))
        }
        "defer_integration" => {
            let (id, token, reason, detail): (RunId, LeaseToken, String, serde_json::Value) =
                read(op, args)?;
            own(&token)?;
            answer(RunTransitions::defer_integration(
                &mut *store,
                &id,
                &token,
                reason.as_str(),
                detail,
            ))
        }
        "hold_integration" => {
            let (id, token, reason, detail): (RunId, LeaseToken, String, serde_json::Value) =
                read(op, args)?;
            own(&token)?;
            answer(RunTransitions::hold_integration(
                &mut *store,
                &id,
                &token,
                reason.as_str(),
                detail,
            ))
        }
        "hand_over_integration" => {
            let (id, token, revert_to, reason, detail): (
                RunId,
                LeaseToken,
                String,
                String,
                serde_json::Value,
            ) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::hand_over_integration(
                &mut *store,
                &id,
                &token,
                revert_to.as_str(),
                reason.as_str(),
                detail,
            ))
        }
        "fail_integration" => {
            let (id, token, reason, receipt): (RunId, LeaseToken, String, serde_json::Value) =
                read(op, args)?;
            own(&token)?;
            answer(RunTransitions::fail_integration(
                &mut *store,
                &id,
                &token,
                reason.as_str(),
                receipt,
            ))
        }
        "abort_integration" => {
            let (id, token, revert_to, message, reason): (
                RunId,
                LeaseToken,
                String,
                String,
                Reason,
            ) = read(op, args)?;
            own(&token)?;
            answer(RunTransitions::abort_integration(
                &mut *store,
                &id,
                &token,
                revert_to.as_str(),
                message.as_str(),
                &reason,
            ))
        }
        "finish_integration" => {
            let (id, token, landing, common_dir): (RunId, LeaseToken, Landing, String) =
                read(op, args)?;
            own(&token)?;
            answer(RunTransitions::finish_integration(
                &mut *store,
                &id,
                &token,
                &landing,
                common_dir.as_str(),
            ))
        }
        "decide_landing" => {
            let (id, status, reason, payload): (RunId, RunStatus, String, serde_json::Value) =
                read(op, args)?;
            answer(RunTransitions::decide_landing(
                &mut *store,
                &id,
                status,
                reason.as_str(),
                payload,
            ))
        }
        "close_satisfied" => {
            let (id, token, payload): (RunId, Option<LeaseToken>, serde_json::Value) =
                read(op, args)?;
            if let Some(token) = &token {
                own(token)?;
            }
            answer(RunTransitions::close_satisfied(
                &mut *store,
                &id,
                token.as_ref(),
                payload,
            ))
        }
        "decide_satisfied" => {
            let (id, reason, payload, task): (
                RunId,
                String,
                serde_json::Value,
                crate::domain::satisfied::TaskMove,
            ) = read(op, args)?;
            answer(RunTransitions::decide_satisfied(
                &mut *store,
                &id,
                reason.as_str(),
                payload,
                &task,
            ))
        }
        "runs_to_triage" => answer(RunRecovery::runs_to_triage(&*store)),
        "begin_triage" => {
            let (id, token, request, launch): (
                RunId,
                LeaseToken,
                Option<serde_json::Value>,
                crate::domain::actor_model::ActorLaunch,
            ) = read(op, args)?;
            own(&token)?;
            answer(RunRecovery::begin_triage(
                &mut *store,
                &id,
                &token,
                request,
                &launch,
            ))
        }
        "finish_triage" => {
            let (id, token, action, payload, also): (
                RunId,
                LeaseToken,
                TriageAction,
                serde_json::Value,
                Vec<(EventKind, serde_json::Value)>,
            ) = read(op, args)?;
            own(&token)?;
            answer(RunRecovery::finish_triage(
                &mut *store,
                &id,
                &token,
                &action,
                payload,
                also,
            ))
        }
        "decide_triage" => {
            let (id, ask_id, answer_, reason): (RunId, AskId, String, String) = read(op, args)?;
            answer(RunRecovery::decide_triage(
                &mut *store,
                &id,
                ask_id,
                answer_.as_str(),
                reason.as_str(),
            ))
        }
        "keep_triage_answer" => {
            let (id, ask_id, answer_, reason): (RunId, AskId, String, String) = read(op, args)?;
            answer(RunRecovery::keep_triage_answer(
                &mut *store,
                &id,
                ask_id,
                answer_.as_str(),
                reason.as_str(),
            ))
        }
        "keep_live_verdict" => {
            let (id, verdict): (RunId, serde_json::Value) = read(op, args)?;
            answer(RunRecovery::keep_live_verdict(&mut *store, &id, verdict))
        }
        "released_kept_results" => answer(RunRecovery::released_kept_results(&*store)),
        "settle_kept_result" => {
            let (kept,): (EventId,) = read(op, args)?;
            answer(RunRecovery::settle_kept_result(&mut *store, kept))
        }
        "silent_planners" => answer(SessionReads::silent_planners(&*store)),
        "remove_session_workspace" => {
            let (role,): (SessionRole,) = read(op, args)?;
            answer(SessionRegistry::remove_session_workspace(&*store, role))
        }
        "forget_retired_session_workspaces" => {
            answer(SessionRegistry::forget_retired_session_workspaces(&*store))
        }
        "open_planner" => {
            let (origin, proposal): (PlannerOrigin, Option<ProposalId>) = read(op, args)?;
            answer(SessionRegistry::open_planner(&*store, origin, proposal))
        }
        "planner_workspace_created" => {
            let (id, workspace_id): (PlannerId, String) = read(op, args)?;
            answer(SessionRegistry::planner_workspace_created(
                &*store,
                id,
                workspace_id.as_str(),
            ))
        }
        "close_planner" => {
            let (id, error): (PlannerId, Option<String>) = read(op, args)?;
            answer(SessionRegistry::close_planner(
                &*store,
                id,
                error.as_deref(),
            ))
        }
        "end_planner" => {
            let (id, payload): (PlannerId, serde_json::Value) = read(op, args)?;
            answer(SessionRegistry::end_planner(&*store, id, &payload))
        }
        "planner" => {
            let (id,): (PlannerId,) = read(op, args)?;
            answer(SessionRegistry::planner(&*store, id))
        }
        "planners" => {
            let (all,): (bool,) = read(op, args)?;
            answer(SessionRegistry::planners(&*store, all))
        }
        "set_planner_route" => {
            let (id, route): (PlannerId, crate::domain::PlannerRoute) = read(op, args)?;
            answer(SessionRegistry::set_planner_route(&*store, id, route))
        }
        "planner_turn_events" => {
            let (id,): (PlannerId,) = read(op, args)?;
            answer(SessionRegistry::planner_turn_events(&*store, id))
        }
        "planner_answer_wait" => {
            let (id, asks, payload): (PlannerId, Vec<AskId>, serde_json::Value) = read(op, args)?;
            answer(SessionRegistry::planner_answer_wait(
                &*store, id, &asks, &payload,
            ))
        }
        "planner_answer_undelivered" => {
            let (id, ask, payload): (PlannerId, AskId, serde_json::Value) = read(op, args)?;
            answer(SessionRegistry::planner_answer_undelivered(
                &*store, id, ask, &payload,
            ))
        }
        "planner_handover" => {
            let (ask,): (AskId,) = read(op, args)?;
            answer(SessionRegistry::planner_handover(&*store, ask))
        }
        "register_planner_wrapper" => {
            let (id, pid): (PlannerId, u32) = read(op, args)?;
            answer(SessionRegistry::register_planner_wrapper(&*store, id, pid))
        }
        "register_planner_agent" => {
            let (id, wrapper_pid, agent): (PlannerId, u32, u32) = read(op, args)?;
            answer(SessionRegistry::register_planner_agent(
                &*store,
                id,
                wrapper_pid,
                agent,
            ))
        }
        "heartbeat_planner" => {
            let (id, wrapper_pid): (PlannerId, u32) = read(op, args)?;
            answer(SessionRegistry::heartbeat_planner(&*store, id, wrapper_pid))
        }
        "planner_exited" => {
            let (id, wrapper_pid, exit_code): (PlannerId, u32, i32) = read(op, args)?;
            answer(SessionRegistry::planner_exited(
                &*store,
                id,
                wrapper_pid,
                exit_code,
            ))
        }
        "planner_silent" => {
            let (id, payload): (PlannerId, serde_json::Value) = read(op, args)?;
            answer(SessionRegistry::planner_silent(&*store, id, payload))
        }
        "record_session_turns" => answer(SessionRegistry::record_session_turns(&*store)),
        "record_session_tokens" => answer(SessionRegistry::record_session_tokens(&*store)),
        "record_session_hook" => {
            let (hook,): (crate::domain::sessions::SessionHook,) = read(op, args)?;
            answer(SessionRegistry::record_session_hook(&*store, &hook))
        }
        "open_hook_session_spans" => answer(SessionRegistry::open_hook_session_spans(&*store)),
        "close_inferred_sessions" => {
            let (ended,): (Vec<EventId>,) = read(op, args)?;
            answer(SessionRegistry::close_inferred_sessions(&*store, &ended))
        }
        "close_review_session" => {
            let (id, session): (RunId, Option<crate::domain::headless_job::JobSession>) =
                read(op, args)?;
            answer(SessionRegistry::close_review_session(
                &*store,
                &id,
                session.as_ref(),
            ))
        }
        "record_headless_job" => {
            let (job,): (NewHeadlessJob,) = read(op, args)?;
            answer(HeadlessJobStore::record_headless_job(&*store, &job))
        }
        "end_headless_job" => {
            let (id, outcome): (i64, String) = read(op, args)?;
            answer(HeadlessJobStore::end_headless_job(
                &*store,
                id,
                outcome.as_str(),
            ))
        }
        "orphaned_headless_jobs" => {
            let (token, own_): (LeaseToken, bool) = read(op, args)?;
            own(&token)?;
            answer(HeadlessJobStore::orphaned_headless_jobs(
                &*store, &token, own_,
            ))
        }
        "open_update_ask" => {
            let (kind, question, options, asked_by, subject, details): (
                crate::domain::AskKind,
                String,
                Vec<String>,
                String,
                Option<String>,
                serde_json::Value,
            ) = read(op, args)?;
            answer(AskStore::open_update_ask(
                &mut *store,
                kind,
                question.as_str(),
                &options.iter().map(String::as_str).collect::<Vec<_>>(),
                asked_by.as_str(),
                subject.as_deref(),
                details,
            ))
        }
        "ask_opened_payload" => {
            let (id,): (crate::domain::AskId,) = read(op, args)?;
            answer(AskStore::ask_opened_payload(&*store, id))
        }
        "close_installed_update_ask" => {
            let (id, answer_, installed): (crate::domain::AskId, String, String) = read(op, args)?;
            answer(AskStore::close_installed_update_ask(
                &mut *store,
                id,
                answer_.as_str(),
                installed.as_str(),
            ))
        }
        "release_updates_on" => answer(Ok(AskStore::release_updates_on(&*store))),
        "update_answers" => {
            let (kind,): (crate::domain::AskKind,) = read(op, args)?;
            answer(AskStore::update_answers(&*store, &kind))
        }
        "asks" => {
            let (query,): (AskQuery,) = read(op, args)?;
            answer(AskStore::asks(&*store, query))
        }
        "has_unclosed_ask" => {
            let (run_id, kind): (RunId, AskKind) = read(op, args)?;
            answer(AskStore::has_unclosed_ask(&*store, &run_id, kind))
        }
        "ask" => {
            let (ask,): (NewAsk,) = read(op, args)?;
            answer(AskStore::ask(&mut *store, ask))
        }
        "hold" => {
            let (hold,): (crate::domain::NewHold,) = read(op, args)?;
            answer(AskStore::hold(&mut *store, hold))
        }
        "hold_of" => {
            let (run_id,): (RunId,) = read(op, args)?;
            answer(AskStore::hold_of(&*store, &run_id))
        }
        "hold_unclosed" => {
            let (run_id,): (RunId,) = read(op, args)?;
            answer(AskStore::hold_unclosed(&*store, &run_id))
        }
        "close_hold_asks" => {
            let (reason, subject, answer_): (crate::domain::AskReason, Option<String>, String) =
                read(op, args)?;
            answer(AskStore::close_hold_asks(
                &mut *store,
                reason,
                subject.as_deref(),
                answer_.as_str(),
            ))
        }
        "read_ask" => {
            let (id,): (AskId,) = read(op, args)?;
            answer(AskStore::read_ask(&*store, id))
        }
        "answer_as" => {
            let (id, text, answerer): (AskId, String, crate::domain::Answerer) = read(op, args)?;
            super::supervisor_answers_as(&answerer).map_err(BadOperation)?;
            answer(AskStore::answer_as(
                &mut *store,
                id,
                text.as_str(),
                answerer,
            ))
        }
        "close_ask" => {
            let (id,): (AskId,) = read(op, args)?;
            answer(AskStore::close_ask(&mut *store, id))
        }
        "landing_answers" => answer(AskStore::landing_answers(&*store)),
        "triage_answers" => answer(AskStore::triage_answers(&*store)),
        "undelivered_answers" => {
            let (run_id,): (RunId,) = read(op, args)?;
            answer(AskStore::undelivered_answers(&*store, &run_id))
        }
        "ask_delivered" => {
            let (id, workspace_id): (AskId, String) = read(op, args)?;
            answer(AskStore::ask_delivered(
                &mut *store,
                id,
                workspace_id.as_str(),
            ))
        }
        "has_unclosed_worker_question" => {
            let (run_id,): (RunId,) = read(op, args)?;
            answer(AskStore::has_unclosed_worker_question(&*store, &run_id))
        }
        "has_unclosed_worker_question_since" => {
            let (run_id, created_from): (RunId, i64) = read(op, args)?;
            answer(AskStore::has_unclosed_worker_question_since(
                &*store,
                &run_id,
                created_from,
            ))
        }
        "last_worker_question_closed" => {
            let (run_id,): (RunId,) = read(op, args)?;
            answer(AskStore::last_worker_question_closed(&*store, &run_id))
        }
        "close_stuck_exit_asks" => {
            let (run_id, answer_): (RunId, String) = read(op, args)?;
            answer(AskStore::close_stuck_exit_asks(
                &mut *store,
                &run_id,
                answer_.as_str(),
            ))
        }
        "close_answer_prompt_asks" => {
            let (run_id, answer_): (RunId, String) = read(op, args)?;
            answer(AskStore::close_answer_prompt_asks(
                &mut *store,
                &run_id,
                answer_.as_str(),
            ))
        }
        "unclosed_stalled_ask" => {
            let (run_id,): (RunId,) = read(op, args)?;
            answer(AskStore::unclosed_stalled_ask(&*store, &run_id))
        }
        "close_stalled_asks" => {
            let (run_id, answer_): (RunId, String) = read(op, args)?;
            answer(AskStore::close_stalled_asks(
                &mut *store,
                &run_id,
                answer_.as_str(),
            ))
        }
        "end_stalled_detections" => {
            let (run_id, answer_): (RunId, String) = read(op, args)?;
            answer(AskStore::end_stalled_detections(
                &mut *store,
                &run_id,
                answer_.as_str(),
            ))
        }
        "note_on_asks" => {
            let (run_id, note, why): (RunId, String, String) = read(op, args)?;
            answer(AskStore::note_on_asks(
                &mut *store,
                &run_id,
                note.as_str(),
                why.as_str(),
            ))
        }
        "close_approve_landing_asks" => {
            let (run_id, answer_): (RunId, String) = read(op, args)?;
            answer(AskStore::close_approve_landing_asks(
                &mut *store,
                &run_id,
                answer_.as_str(),
            ))
        }
        "close_run_asks" => {
            let (run_id, answer_): (RunId, String) = read(op, args)?;
            answer(AskStore::close_run_asks(
                &mut *store,
                &run_id,
                answer_.as_str(),
            ))
        }
        "close_ended_landing_asks" => {
            let (task,): (Option<TaskId>,) = read(op, args)?;
            answer(AskStore::close_ended_landing_asks(&mut *store, task))
        }
        "close_blocked_asks" => {
            let (run_id, task_id, answer_): (RunId, TaskId, String) = read(op, args)?;
            answer(AskStore::close_blocked_asks(
                &mut *store,
                &run_id,
                task_id,
                answer_.as_str(),
            ))
        }
        "all_events" => answer(EventStore::all_events(&*store)),
        "record_attributes_baseline" => answer(EventStore::record_attributes_baseline(&*store)),
        "record_findings_baseline" => answer(EventStore::record_findings_baseline(&*store)),
        "findings" => {
            let (query,): (crate::domain::FindingQuery,) = read(op, args)?;
            answer(QueueRecords::findings(&*store, &query))
        }
        "reports_written" => answer(QueueRecords::reports_written(&*store)),
        "record_report_written" => {
            let (payload,): (serde_json::Value,) = read(op, args)?;
            answer(QueueRecords::record_report_written(&*store, payload))
        }
        "kpi_breaches_open" => answer(QueueRecords::kpi_breaches_open(&*store)),
        "record_kpi_breach" => {
            let (kind, payload, push_day): (EventKind, serde_json::Value, Option<(i64, usize)>) =
                read(op, args)?;
            answer(QueueRecords::record_kpi_breach(
                &*store, kind, payload, push_day,
            ))
        }
        "record_kpi_push_abandoned" => {
            let (payload,): (serde_json::Value,) = read(op, args)?;
            answer(QueueRecords::record_kpi_push_abandoned(&*store, payload))
        }
        "record_forecast" => {
            let (payload, previous): (serde_json::Value, Option<EventId>) = read(op, args)?;
            answer(QueueRecords::record_forecast(&*store, payload, previous))
        }
        "ci_watch_events" => answer(QueueRecords::ci_watch_events(&*store)),
        "record_ci_check" => {
            let (record,): (crate::domain::ci_watch::CiCheckRecord,) = read(op, args)?;
            answer(QueueRecords::record_ci_check(&*store, record))
        }
        "record_finding_once" => {
            let (finding,): (crate::domain::NewFinding,) = read(op, args)?;
            answer(QueueRecords::record_finding_once(&*store, finding))
        }
        "ci_failure_findings_of" => {
            let (task,): (TaskId,) = read(op, args)?;
            answer(QueueRecords::ci_failure_findings_of(&*store, task))
        }
        "ci_failure_fix_tasks" => answer(QueueRecords::ci_failure_fix_tasks(&*store)),
        "claim_inbox_nudge" => {
            let (payload,): (serde_json::Value,) = read(op, args)?;
            answer(InboxWatchLog::claim_inbox_nudge(&*store, payload))
        }
        "record_inbox_watcher_change" => {
            let (kind, payload): (EventKind, serde_json::Value) = read(op, args)?;
            answer(InboxWatchLog::record_inbox_watcher_change(
                &*store, kind, payload,
            ))
        }
        "settle_eval_round" => {
            let (eval_id, kind, payload): (i64, EventKind, serde_json::Value) = read(op, args)?;
            answer(EvalRounds::settle_eval_round(
                &*store, eval_id, kind, payload,
            ))
        }
        "take_up_eval_round" => {
            let (eval_id, token): (i64, String) = read(op, args)?;
            own(&LeaseToken::new(token.clone()))?;
            answer(EvalRounds::take_up_eval_round(
                &*store,
                eval_id,
                token.as_str(),
                alive,
            ))
        }
        "take_case_set" => {
            let (set_id, token): (i64, String) = read(op, args)?;
            own(&LeaseToken::new(token.clone()))?;
            answer(EvalRounds::take_case_set(
                &*store,
                set_id,
                token.as_str(),
                alive,
            ))
        }
        "event_id_before" => {
            let (unix,): (i64,) = read(op, args)?;
            answer(ObserverLog::event_id_before(&*store, unix))
        }
        "ask_high_water" => answer(ObserverLog::ask_high_water(&*store)),
        "written_by" => {
            let (role, event_id, ask_id): (String, EventId, AskId) = read(op, args)?;
            answer(ObserverLog::written_by(
                &*store,
                role.as_str(),
                event_id,
                ask_id,
            ))
        }
        "last_observation" => {
            let (mode,): (String,) = read(op, args)?;
            answer(ObserverLog::last_observation(&*store, mode.as_str()))
        }
        "events_besides" => {
            let (role, span_kind, after): (String, String, EventId) = read(op, args)?;
            answer(ObserverLog::events_besides(
                &*store,
                role.as_str(),
                span_kind.as_str(),
                after,
            ))
        }
        "observations" => {
            let (limit,): (usize,) = read(op, args)?;
            answer(ObserverLog::observations(&*store, limit))
        }
        _ => Err(BadOperation(format!(
            "the queue service has no supervisor operation {op}"
        ))
        .into()),
    }
}

/// The arguments of `set_max_load`: the token, the threshold and its
/// source, or the first two alone from a supervisor of a binary that
/// sends no source.
pub(super) fn max_load_args(
    op: &str,
    args: Value,
) -> Result<(
    LeaseToken,
    Option<f64>,
    Option<crate::domain::slot_limits::SettingSource>,
)> {
    match read::<(LeaseToken, Option<f64>)>(op, args.clone()) {
        Ok((token, max_load)) => Ok((token, max_load, None)),
        Err(_) => read(op, args),
    }
}
