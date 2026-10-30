//! The capability each operation of the supervisor's use case needs of
//! its principal's policy, on top of `scheduler.supervise` (default deny,
//! [Security]): an operation not named here is none of the supervisor's,
//! and one whose capability the policy does not grant is refused, and the
//! refusal recorded, before anything is written. The planning a person,
//! the inbox or a planner does needs what it needs on the command line
//! ([`PLANNING`]), which the supervisor's policy does not grant: it cancels
//! a task at an answer, and starts again the plan review of a proposal it
//! held, and does no other planning.
//!
//! [Security]: ../../../docs/design/security.md

use crate::domain::authorization::Capability as C;
use crate::domain::{AskKind, NewAsk, PlannerOrigin, Submission, TaskAction};
use serde_json::Value;

/// The operations that only read the queue, the tokens' principals and the
/// telemetry's targets.
pub(super) const READS: &[&str] = &[
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
    "open_replan_operations",
    "replan_lineage",
    "running_diagnosis_owners",
    "diagnosis_owners",
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
    "latest_task_events",
    "run_events",
    "task_events",
    "has_run_event",
    "last_observe",
    "latest_event_id",
    "earliest_event_at",
    "latest_event_of",
    "latest_events_of",
    "latest_queue_event",
    "latest_events_by_supervisor",
    "events_of_between",
    "events_between",
    "show",
    "graph_input",
    "ready_at",
    "investigations",
    "list",
    "show_goal",
    "show_proposal",
    "proposals",
    "lint_input",
    "notes",
    "related_landed_commits",
    "related_tasks",
    "search_documents",
    "task_goals",
    "task_titles",
    "task_changes",
    "draft_origins",
    "planner_requests",
    "plan_request",
    "request_asks",
    "event_by_id",
    "replan_brief",
    "replacement_brief",
    "replacements_to_apply",
    "child_carry",
    "planner_findings",
    "improvements",
    "draft_origin",
    "planner_drafts",
    "planner_draft_tasks",
    "draft_bundle",
    "planner_answers",
    "planner_answer_route",
    "proposal_questions",
    "replaced_tasks",
    "exhausted_drafts",
    "follow_up_depth",
    "finding_view",
    "finding_asks",
    "exhausted_findings",
    "ask_delivered_to",
    "answer_claimed_at",
    "plan_review_candidates",
    "revising_proposals",
    "plan_reviews_finished_before",
    "revise_goal_wait",
    "plan_answers",
    "applies_plan_answer",
    "plan_review_holds",
    "stranded_dependencies",
    "answered_asks",
    "goal_review_candidates",
    "goal_replacements",
    "unresolved_replacements",
    "goal_reviews",
    "goal_answers",
    "correction_answers",
    "goal_review_holds",
    "goal_follow_ups",
    "runs_to_triage",
    "released_kept_results",
    "silent_planners",
    "planner",
    "planners",
    "planner_turn_events",
    "planner_answer_wait",
    "planner_answer_undelivered",
    "orphaned_headless_jobs",
    "ask_opened_payload",
    "update_answers",
    "asks",
    "has_unclosed_ask",
    "hold_of",
    "hold_unclosed",
    "read_ask",
    "landing_answers",
    "triage_answers",
    "undelivered_answers",
    "has_unclosed_worker_question",
    "has_unclosed_worker_question_since",
    "last_worker_question_closed",
    "unclosed_stalled_ask",
    "all_events",
    "stats_window",
    "findings",
    "reports_written",
    "kpi_breaches_open",
    "ci_watch_events",
    "ci_failure_findings_of",
    "ci_failure_fix_tasks",
    "event_id_before",
    "ask_high_water",
    "written_by",
    "last_observation",
    "events_besides",
    "observations",
    "run_resources",
    "run_resources_after",
    "open_attachments",
    "e2e_gate_events",
    "token_principals",
    "held_telemetry",
    "target_facts",
    "job_read",
    "imported_files",
    "node_samples",
    "node_samples_after",
    "ledger_row",
    "ledger_rows",
    "steps_after",
    "target_steps",
    "target_resources",
    "resources_after",
    "session_owner",
    "ledger_row_part",
    "ledger_row_keys",
    "run_events_after",
    "task_events_after",
];

/// The scheduler's own transitions and records (`scheduler.supervise`):
/// the claim, the leases and the runs', the registration and the handoff,
/// the sessions' and the jobs' records, the plan and goal reviews it runs
/// and applies, the planners' routes, the observation's and the reports'
/// records, the host's load samples, the ledger's rows and retention, the
/// eval's rounds and the tokens it issues.
const SUPERVISE: &[&str] = &[
    "claim_for_supervisor",
    "claim_for_supervisor_holding",
    "abandon_run",
    "record_cleanup_failure",
    "remind_task_holds",
    "park_for_hold",
    "settle_hold_at_rest",
    "release_for_hold",
    "rest_fenced",
    "lease_for_hold_park",
    "begin_restore",
    "finish_restore",
    "record_clone_restore",
    "workspace_closed",
    "cleanup_failed",
    "switch_provider",
    "record_runtime_error",
    "plan_run",
    "workspace_created",
    "finish_supervision",
    "finish_lost_session",
    "finish_supervision_live",
    "finish_validation",
    "restart_validation",
    "park_live",
    "park_e2e_failed",
    "lease_for_e2e",
    "begin_e2e",
    "park_rechecked",
    "record_workspace_closed",
    "lease_for_review",
    "lease_satisfied",
    "begin_resume",
    "finish_resume",
    "finish_approved_resume",
    "skip_resume",
    "exhaust_resumes",
    "note_hold_deferred",
    "end_holds_of_ended_tasks",
    "heartbeat_leases",
    "release_lease",
    "heartbeat",
    "register_wrapper",
    "register_resume_wrapper",
    "clear_lost_session",
    "session_reopened",
    "register_agent",
    "register_resume_agent",
    "register_turn_agent",
    "heartbeat_wrapper",
    "wrapper_exited",
    "register_supervisor",
    "accept_handoff",
    "request_handoff",
    "take_handoff",
    "cancel_handoff",
    "resume_registration",
    "prune_supervisor",
    "record_runtime_event",
    "record_backend_failure",
    "record_queue_event",
    "record_queue_events",
    "register_follow_ups",
    "register_recurring_draft",
    "record_draft_origin",
    "answer_request",
    "claim_planner_answer",
    "close_planner_answer",
    "record_task_event",
    "set_follow_up_depth",
    "ask_delivery_failed",
    "interrupt_plan_reviews_for_handoff",
    "begin_plan_review",
    "take_over_plan_review",
    "finish_plan_review",
    "fail_plan_review",
    "claim_revise",
    "settle_replan_requests",
    "revise_sent",
    "revise_lost",
    "planner_unresponsive",
    "settle_proposals",
    "decide_plan",
    "interrupt_goal_reviews_for_handoff",
    "begin_goal_review",
    "take_over_goal_review",
    "finish_goal_review",
    "fail_goal_review",
    "decide_goal",
    "decide_correction",
    "rearm_goal_review",
    "remove_session_workspace",
    "forget_retired_session_workspaces",
    "planner_workspace_created",
    "close_planner",
    "end_planner",
    "set_planner_route",
    "planner_handover",
    "register_planner_wrapper",
    "register_planner_agent",
    "heartbeat_planner",
    "planner_exited",
    "planner_silent",
    "record_session_turns",
    "record_session_tokens",
    "record_session_hook",
    "open_hook_session_spans",
    "close_inferred_sessions",
    "close_review_session",
    "record_headless_job",
    "end_headless_job",
    "ask_delivered",
    "end_stalled_detections",
    "record_attributes_baseline",
    "record_findings_baseline",
    "record_report_written",
    "record_kpi_breach",
    "record_kpi_push_abandoned",
    "record_forecast",
    "record_ci_check",
    "record_run_resource",
    "claim_inbox_nudge",
    "record_inbox_watcher_change",
    "settle_eval_round",
    "take_up_eval_round",
    "take_case_set",
    "sign_out",
    "sign_out_at_stop",
    "issue_token",
    "revoke_token",
    "revoke_principal",
    "issue_telemetry",
    "revoke_telemetry",
    "register_telemetry_session",
    "record_node_sample",
    "import_node_sample_part",
    "remove_node_samples",
    "write_ledger_row_over",
    "remove_node_rows",
    "remove_steps",
    "stage_ledger_row",
    "write_staged_ledger_row_over",
];

/// The operations of one capability other than `scheduler.supervise`.
/// The operations the landing's integrator alone sends, with its token
/// ([`super::integrator`]): the landing's end on `main` and its run's
/// `run_integrated` are never a supervisor's own records (ADR-t728-2,
/// docs/design/security.md「reviewのpassとIntegrator」), so
/// [`supervisor_needs`] refuses them to a supervisor's token.
pub(super) const INTEGRATOR_ONLY: [&str; 1] = ["finish_integration"];

const OTHERS: &[(C, &[&str])] = &[
    // The recovery of a run left by a supervisor, and its triage.
    (
        C::RunRecover,
        &[
            "adopt_run",
            "lease_stranded_landing",
            "recover_run",
            "begin_triage",
            "finish_triage",
            "decide_triage",
            "keep_triage_answer",
            "keep_live_verdict",
            "settle_kept_result",
        ],
    ),
    // A run parked to be replanned and its replacement (ADR-t1521-2).
    (
        C::ReplanPark,
        &[
            "lease_for_park",
            "detect_replan",
            "ask_replan_diagnosis",
            "start_replan_diagnosis",
        ],
    ),
    (
        C::ReplanApply,
        &[
            "move_replan",
            "settle_replan",
            "begin_replan_apply",
            "apply_replacement",
        ],
    ),
    // The landing it asks the integrator for (ADR-t728-2), and the
    // records of it that the supervisor makes itself; the landing's end on
    // `main` is none of these ([`INTEGRATOR_ONLY`]).
    (
        C::IntegrationRequest,
        &[
            "begin_landing",
            "end_landing",
            "defer_integration",
            "hold_integration",
            "hand_over_integration",
            "fail_integration",
            "abort_integration",
            "decide_landing",
            "close_satisfied",
            "decide_satisfied",
        ],
    ),
    // Its settings at its start and its checkout's binding.
    (
        C::ServiceLifecycle,
        &[
            "set_slot_limits",
            "set_max_load",
            "set_supervisor_mode",
            "bind_repository",
            "rebind_repository",
            "assert_repository",
        ],
    ),
    (C::BinaryInstall, &["set_auto_update", "release_updates_on"]),
    (
        C::PlannerOpen,
        &[
            "open_request_planner",
            "open_draft_planner",
            "open_finding_planner",
            "open_planner",
        ],
    ),
    (C::AskOpen, &["open_update_ask", "hold"]),
    // The runtime's withdrawal of an ask (`answer_as`, as the runtime
    // only) and the asks it closes.
    (
        C::AskClose,
        &[
            "answer_as",
            "close_ask",
            "close_installed_update_ask",
            "close_hold_asks",
            "close_stuck_exit_asks",
            "close_answer_prompt_asks",
            "close_stalled_asks",
            "close_approve_landing_asks",
            "close_run_asks",
            "close_ended_landing_asks",
            "close_blocked_asks",
        ],
    ),
    (C::NoteWrite, &["note_on_asks"]),
    (
        C::FindingRecord,
        &[
            "record_finding_once",
            "record_finding_event",
            "record_finding",
        ],
    ),
    (C::FindingResolve, &["settle_findings"]),
    // A task canceled at a person's answer to its landing's ask.
    (C::TaskCancel, &["cancel"]),
];

/// The planning a person, the inbox or a planner does, by the capability
/// the command line checks for it: the supervisor's policy grants none of
/// them, so its request is refused as `authorization_denied`.
const PLANNING: &[(C, &[&str])] = &[
    (
        C::TaskWrite,
        &[
            "add",
            "add_dependency",
            "remove_dependency",
            "add_goal_dependency",
            "remove_goal_dependency",
            "set_goal",
            "set_paths",
            "edit_task",
            "set_priority",
        ],
    ),
    (C::GoalWrite, &["add_goal", "edit_goal"]),
    (C::GoalReady, &["ready_goal"]),
    (C::ProposalWithdraw, &["withdraw_proposal"]),
];

/// The capability the supervisor's operation `op` with `args` needs, on
/// top of `scheduler.supervise`: why it is none of the supervisor's,
/// otherwise. A submission is the supervisor's (`scheduler.supervise`)
/// only as the runtime's start again of a proposal's plan review, adding
/// no task and no goal; any other is `proposal.submit`. A task's
/// transition is the command line's: a draft's, a submission's or a ready
/// that bypasses plan review. An ask is `ask.open`, but a `blocked` one
/// on a finding `finding.ask`.
pub(super) fn supervisor_needs(op: &str, args: &[Value]) -> Result<C, String> {
    let arg = |index: usize| args.get(index).cloned().unwrap_or(Value::Null);
    if op == "submit" {
        let restart = serde_json::from_value::<Submission>(arg(0)).is_ok_and(|submission| {
            submission.tasks.is_empty()
                && submission.goals.is_empty()
                && submission.proposal.is_some()
                && submission.owner.origin == PlannerOrigin::Runtime
                && submission.owner.workspace_id.is_none()
        });
        return Ok(if restart {
            C::Supervise
        } else {
            C::ProposalSubmit
        });
    }
    if op == "ask" {
        // A `blocked` ask on a finding is the observer's (`finding.ask`),
        // not an ask the supervisor opens.
        return serde_json::from_value::<NewAsk>(arg(0))
            .map(|ask| {
                if ask.kind == AskKind::Blocked && ask.finding_id.is_some() {
                    C::FindingAsk
                } else {
                    C::AskOpen
                }
            })
            .map_err(|error| format!("the ask a supervisor opens does not read: {error}"));
    }
    if op == "transition" {
        return match serde_json::from_value::<TaskAction>(arg(1)) {
            Ok(TaskAction::BypassReview) => Ok(C::TaskReadyBypassReview),
            Ok(TaskAction::Draft) => Ok(C::TaskWrite),
            Ok(TaskAction::Submit) => Ok(C::ProposalSubmit),
            _ => Err(format!(
                "a supervisor does not transition a task with {}",
                arg(1)
            )),
        };
    }
    if INTEGRATOR_ONLY.contains(&op) {
        return Err(format!(
            "{op} is not an operation of the supervisor: the landing's integrator sends it"
        ));
    }
    if READS.contains(&op) {
        return Ok(C::QueueRead);
    }
    if SUPERVISE.contains(&op) {
        return Ok(C::Supervise);
    }
    OTHERS
        .iter()
        .chain(PLANNING)
        .find(|(_, ops)| ops.contains(&op))
        .map(|(capability, _)| *capability)
        .ok_or_else(|| format!("{op} is not an operation of the supervisor"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ActorRole;
    use crate::domain::authorization::grants;
    use crate::domain::{GoalId, PlannerOwner, ProposalId, TaskId};
    use serde_json::json;

    fn submission(tasks: Vec<TaskId>, goals: Vec<GoalId>, origin: PlannerOrigin) -> Value {
        serde_json::to_value(Submission {
            tasks,
            goals,
            proposal: Some(ProposalId::new(3)),
            owner: PlannerOwner {
                origin,
                workspace_id: None,
            },
        })
        .unwrap()
    }

    fn transition(action: TaskAction) -> Result<C, String> {
        supervisor_needs(
            "transition",
            &[json!(1), serde_json::to_value(action).unwrap()],
        )
    }

    #[test]
    fn an_operation_is_in_one_list_only() {
        let mut all: Vec<&str> = READS
            .iter()
            .chain(SUPERVISE)
            .chain(&INTEGRATOR_ONLY)
            .chain(
                OTHERS
                    .iter()
                    .chain(PLANNING)
                    .flat_map(|(_, ops)| ops.iter()),
            )
            .copied()
            .collect();
        let count = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), count);
    }

    #[test]
    fn the_supervisor_has_what_its_operations_need_and_no_planning() {
        let supervisor = grants(ActorRole::Supervisor);
        for (capability, _) in OTHERS {
            assert!(supervisor.contains(capability), "{capability:?}");
        }
        for (capability, _) in PLANNING {
            assert!(!supervisor.contains(capability), "{capability:?}");
        }
        for capability in [C::ProposalSubmit, C::TaskReadyBypassReview] {
            assert!(!supervisor.contains(&capability), "{capability:?}");
        }
    }

    #[test]
    fn the_planning_of_a_person_or_a_planner_needs_what_the_command_line_needs() {
        assert_eq!(
            transition(TaskAction::BypassReview),
            Ok(C::TaskReadyBypassReview)
        );
        assert_eq!(transition(TaskAction::Draft), Ok(C::TaskWrite));
        assert_eq!(transition(TaskAction::Submit), Ok(C::ProposalSubmit));
        for (op, capability) in [
            ("add", C::TaskWrite),
            ("set_priority", C::TaskWrite),
            ("add_dependency", C::TaskWrite),
            ("add_goal", C::GoalWrite),
            ("edit_goal", C::GoalWrite),
            ("ready_goal", C::GoalReady),
            ("withdraw_proposal", C::ProposalWithdraw),
            ("cancel", C::TaskCancel),
            ("show", C::QueueRead),
            ("claim_for_supervisor", C::Supervise),
            ("begin_landing", C::IntegrationRequest),
            ("answer_as", C::AskClose),
        ] {
            assert_eq!(supervisor_needs(op, &[json!(1)]), Ok(capability), "{op}");
        }
    }

    #[test]
    fn what_the_supervisor_never_sends_is_none_of_its_operations() {
        for action in [
            TaskAction::Ready,
            TaskAction::Cancel,
            TaskAction::Approve,
            TaskAction::Reopen,
        ] {
            assert!(transition(action).is_err(), "{action:?}");
        }
        assert!(supervisor_needs("transition", &[json!(1)]).is_err());
        for op in [
            "claim",
            "close_goal",
            "approve_proposal",
            "send_back_proposal",
            "add_note",
            "no_such_op",
        ] {
            assert!(supervisor_needs(op, &[json!(1)]).is_err(), "{op}");
        }
    }

    #[test]
    fn an_ask_on_a_finding_is_the_observers_and_any_other_the_supervisor_opens() {
        use crate::domain::{AskReason, FindingId};
        let ask = |kind: AskKind, finding_id: Option<FindingId>| {
            let ask = NewAsk {
                kind,
                task_id: None,
                run_id: None,
                question: "Stuck?".into(),
                options: vec!["done".into()],
                asked_by: "supervisor".into(),
                reason_category: AskReason::Scope,
                topics: Vec::new(),
                recommendation: None,
                confidence: None,
                finding_id,
                request_id: None,
            };
            supervisor_needs("ask", &[serde_json::to_value(ask).unwrap()])
        };
        let finding = Some(FindingId::new(4));
        assert_eq!(ask(AskKind::Blocked, finding), Ok(C::FindingAsk));
        assert_eq!(ask(AskKind::Blocked, None), Ok(C::AskOpen));
        assert_eq!(ask(AskKind::ApproveLanding, None), Ok(C::AskOpen));
        assert!(supervisor_needs("ask", &[]).is_err());
        assert!(!grants(ActorRole::Supervisor).contains(&C::FindingAsk));
    }

    #[test]
    fn a_supervisor_submits_only_a_plan_reviews_start_again() {
        let restart = submission(Vec::new(), Vec::new(), PlannerOrigin::Runtime);
        assert_eq!(supervisor_needs("submit", &[restart]), Ok(C::Supervise));
        for other in [
            submission(vec![TaskId::new(1)], Vec::new(), PlannerOrigin::Runtime),
            submission(Vec::new(), vec![GoalId::new(1)], PlannerOrigin::Runtime),
            submission(Vec::new(), Vec::new(), PlannerOrigin::Person),
            json!({"tasks": []}),
        ] {
            assert_eq!(
                supervisor_needs("submit", std::slice::from_ref(&other)),
                Ok(C::ProposalSubmit),
                "{other}"
            );
        }
        assert_eq!(supervisor_needs("submit", &[]), Ok(C::ProposalSubmit));
    }
}
