//! The service's side of [`super::SupervisorQueue`]: each operation the
//! supervisor sends by its name is the queue's own on the store, with
//! every lease token it names checked to be the principal's registration
//! first (`own`). Made from the ports' traits as [`super::forward`] is.

#![allow(clippy::wildcard_imports)]

use super::{BadOperation, SupervisorStore, read, reply as answer};
use crate::application::*;
use crate::domain::*;
use anyhow::Result;
use serde_json::Value;

/// Run the operation `op` with `args` on `store`: its result as the
/// service answers it.
pub(super) fn dispatch(
    store: &mut dyn SupervisorStore,
    own: &dyn Fn(&LeaseToken) -> Result<()>,
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
            let (token, max_load): (LeaseToken, Option<f64>) = read(op, args)?;
            own(&token)?;
            answer(SupervisorRegistry::set_max_load(&*store, &token, max_load))
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
            own(&token)?;
            answer(SupervisorRegistry::prune_supervisor(
                &*store,
                &token,
                kind,
                &|_| stopped.clone(),
            ))
        }
        _ => Err(BadOperation(format!(
            "the queue service has no supervisor operation {op}"
        ))
        .into()),
    }
}
