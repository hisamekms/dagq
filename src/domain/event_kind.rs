//! The `kind` names of `run_events`. They are a public contract (ADR-0016):
//! `status`, `show`, `events` and `watch` print them, and readers match on
//! them, so a name never changes.
//!
//! The write port takes an [`EventKind`], the kinds this binary knows, and
//! writes its [`EventKind::as_str`] (ADR-0073 decision 20): no event is
//! written with a kind given as text. The readers keep the kind as text and
//! take one they do not know (decision 21), so they name a kind by one of
//! the constants here, or by the constant of the domain module that owns it
//! (`recheck::LANDING_RECHECK_FINISHED`, `claim_hold::CLAIM_HELD`, ...),
//! never by a literal; each is the `as_str` of its [`EventKind`].

macro_rules! event_kinds {
    ($($variant:ident => $name:literal,)*) => {
        /// A kind of `run_events` the write port writes.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum EventKind {
            $($variant,)*
        }

        impl EventKind {
            /// Every kind, in the order of their names.
            pub const ALL: &'static [EventKind] = &[$(EventKind::$variant,)*];

            /// The text of the kind in `run_events.kind`.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(EventKind::$variant => $name,)*
                }
            }
        }
    };
}

event_kinds! {
    AgentStarted => "agent_started",
    ApproveWithheld => "approve_withheld",
    AskAnswered => "ask_answered",
    AskClosed => "ask_closed",
    AskDelivered => "ask_delivered",
    AskDeliveryFailed => "ask_delivery_failed",
    AskOpened => "ask_opened",
    AskUpdated => "ask_updated",
    AuthRequired => "auth_required",
    AuthorizationDenied => "authorization_denied",
    AutoRepaired => "auto_repaired",
    BackendCallFailed => "backend_call_failed",
    BrokerHealthy => "broker_healthy",
    BrokerImageBuilt => "broker_image_built",
    BrokerStarted => "broker_started",
    BrokerStopRequested => "broker_stop_requested",
    BrokerStopped => "broker_stopped",
    BrokerTokenIssued => "broker_token_issued",
    BrokerTokenRevoked => "broker_token_revoked",
    BrokerUnavailable => "broker_unavailable",
    BrokerUnhealthy => "broker_unhealthy",
    BuildOutputsRemoved => "build_outputs_removed",
    CandidatesSampled => "candidates_sampled",
    ClaimDeferralEnded => "claim_deferral_ended",
    ClaimDeferred => "claim_deferred",
    ClaimHeld => "claim_held",
    ClaimResumed => "claim_resumed",
    CleanupFailed => "cleanup_failed",
    ConflictPrecheck => "conflict_precheck",
    ConflictReceiptRejected => "conflict_receipt_rejected",
    ConflictResolved => "conflict_resolved",
    ConflictsConfigChanged => "conflicts_config_changed",
    DependencyAdded => "dependency_added",
    DependencyRemoved => "dependency_removed",
    DependencyStranded => "dependency_stranded",
    DraftAdopted => "draft_adopted",
    DraftPlannerExhausted => "draft_planner_exhausted",
    DraftPlannerOpened => "draft_planner_opened",
    DraftPlannerSettled => "draft_planner_settled",
    EvidenceMissing => "evidence_missing",
    ExitRequestTimedOut => "exit_request_timed_out",
    ExitRequested => "exit_requested",
    ExitRetried => "exit_retried",
    ExitUnsent => "exit_unsent",
    FindingPlannerExhausted => "finding_planner_exhausted",
    FindingPlannerOpened => "finding_planner_opened",
    FindingRecorded => "finding_recorded",
    FindingStatusChanged => "finding_status_changed",
    FindingUpdated => "finding_updated",
    FirstCommitObserved => "first_commit_observed",
    FollowUpAdopted => "follow_up_adopted",
    FollowUpRegistered => "follow_up_registered",
    ForecastRecorded => "forecast_recorded",
    GoalClosed => "goal_closed",
    GoalCreated => "goal_created",
    GoalDecided => "goal_decided",
    GoalDependencyAdded => "goal_dependency_added",
    GoalDependencyRemoved => "goal_dependency_removed",
    GoalReviewFailed => "goal_review_failed",
    GoalReviewFinished => "goal_review_finished",
    GoalReviewRearmed => "goal_review_rearmed",
    GoalReviewStarted => "goal_review_started",
    GoalStatusChanged => "goal_status_changed",
    GoalSubmitted => "goal_submitted",
    GoalUpdated => "goal_updated",
    HeadlessJobStopped => "headless_job_stopped",
    HoldAnswerApplied => "hold_answer_applied",
    HoldContinueSent => "hold_continue_sent",
    IdleInferred => "idle_inferred",
    InboxNudgeFailed => "inbox_nudge_failed",
    InboxNudged => "inbox_nudged",
    InputNotReady => "input_not_ready",
    IntegrationApproved => "integration_approved",
    IntegrationDeferred => "integration_deferred",
    IntegrationError => "integration_error",
    IntegrationFailed => "integration_failed",
    IntegrationHeld => "integration_held",
    IntegrationRebaseAborted => "integration_rebase_aborted",
    IntegrationRebased => "integration_rebased",
    IntegrationReceipt => "integration_receipt",
    IntegrationRetried => "integration_retried",
    IntegrationStarted => "integration_started",
    JobRestarted => "job_restarted",
    KnownDialogUnanswered => "known_dialog_unanswered",
    KpiBreachResolved => "kpi_breach_resolved",
    KpiBreachStarted => "kpi_breach_started",
    KpiPushAbandoned => "kpi_push_abandoned",
    KpiPushFailed => "kpi_push_failed",
    KpiPushSent => "kpi_push_sent",
    LandingDecided => "landing_decided",
    LandingHeld => "landing_held",
    LandingQueued => "landing_queued",
    LandingRecheckFailed => "landing_recheck_failed",
    LandingRecheckFinished => "landing_recheck_finished",
    LandingResumed => "landing_resumed",
    LeaseAcquired => "lease_acquired",
    LeaseReleased => "lease_released",
    MarkRecorded => "mark_recorded",
    MarkRetracted => "mark_retracted",
    MigrationRenumbered => "migration_renumbered",
    Observation => "observation",
    ObserveFinished => "observe_finished",
    ObserveStarted => "observe_started",
    PlanDecided => "plan_decided",
    PlanReviewDiscarded => "plan_review_discarded",
    PlanReviewFailed => "plan_review_failed",
    PlanReviewFinished => "plan_review_finished",
    PlanReviewOutcome => "plan_review_outcome",
    PlanReviewStarted => "plan_review_started",
    PlanReviseLost => "plan_revise_lost",
    PlanReviseSent => "plan_revise_sent",
    PlannerAnswerClaimed => "planner_answer_claimed",
    PlannerAnswerClosed => "planner_answer_closed",
    PlannerReleased => "planner_released",
    PlannerUnresponsive => "planner_unresponsive",
    PromptCleared => "prompt_cleared",
    PromptWaiting => "prompt_waiting",
    ProviderHeld => "provider_held",
    ProviderReleased => "provider_released",
    ProviderSwitched => "provider_switched",
    ProviderWaiting => "provider_waiting",
    ProposalResubmitted => "proposal_resubmitted",
    ProposalSettled => "proposal_settled",
    ProposalWithdrawn => "proposal_withdrawn",
    PushFailed => "push_failed",
    PushFinished => "push_finished",
    PushSkipped => "push_skipped",
    QueueHoldApplied => "queue_hold_applied",
    ReceiptObserved => "receipt_observed",
    RecoveryFailed => "recovery_failed",
    RecoveryFinished => "recovery_finished",
    RecoveryParked => "recovery_parked",
    RecoveryRequested => "recovery_requested",
    ReleaseCheckFailed => "release_check_failed",
    ReleaseChecked => "release_checked",
    ReportWritten => "report_written",
    ResumeFinished => "resume_finished",
    ResumeRequestSent => "resume_request_sent",
    ResumeSkipped => "resume_skipped",
    ResumeStarted => "resume_started",
    ReviewBypassed => "review_bypassed",
    ReviewFailed => "review_failed",
    ReviewFinished => "review_finished",
    ReviewOutcome => "review_outcome",
    ReviewRetried => "review_retried",
    ReviewStarted => "review_started",
    ReviseFinished => "revise_finished",
    ReviseReceiptRejected => "revise_receipt_rejected",
    ReviseRequested => "revise_requested",
    ReviseUnsent => "revise_unsent",
    RunAdopted => "run_adopted",
    RunClaimed => "run_claimed",
    RunEnvChanged => "run_env_changed",
    RunEnvProgramFound => "run_env_program_found",
    RunEnvProgramMissing => "run_env_program_missing",
    RunInherited => "run_inherited",
    RunIntegrated => "run_integrated",
    RunPlanned => "run_planned",
    RunRecovered => "run_recovered",
    RunSlotRegained => "run_slot_regained",
    RunWaitingAskAdded => "run_waiting_ask_added",
    RunWaitingDeferred => "run_waiting_deferred",
    RunWaitingEnded => "run_waiting_ended",
    RunWaitingStarted => "run_waiting_started",
    RuntimeError => "runtime_error",
    ScopeViolation => "scope_violation",
    ScratchpadRemoved => "scratchpad_removed",
    ScreenCaptureFailed => "screen_capture_failed",
    SessionClosed => "session_closed",
    SessionExited => "session_exited",
    SessionGoneParked => "session_gone_parked",
    SessionIdleObserved => "session_idle_observed",
    SessionOpened => "session_opened",
    SessionTurns => "session_turns",
    StaleReceiptNudged => "stale_receipt_nudged",
    StaleReceiptResolved => "stale_receipt_resolved",
    StallConfigLoaded => "stall_config_loaded",
    StallNudged => "stall_nudged",
    StallPreempted => "stall_preempted",
    StallResolved => "stall_resolved",
    SubmitNotStarted => "submit_not_started",
    SubmitResent => "submit_resent",
    SubmitRetried => "submit_retried",
    SubmitUnconfirmed => "submit_unconfirmed",
    SupervisionFinished => "supervision_finished",
    SupervisorConfigChanged => "supervisor_config_changed",
    SupervisorHandedOff => "supervisor_handed_off",
    SupervisorStarted => "supervisor_started",
    SupervisorStopped => "supervisor_stopped",
    TaskCreated => "task_created",
    TaskEdited => "task_edited",
    TaskGoalChanged => "task_goal_changed",
    TaskPathsChanged => "task_paths_changed",
    TaskPriorityChanged => "task_priority_changed",
    TaskReopened => "task_reopened",
    TaskStatusChanged => "task_status_changed",
    TaskSubmitted => "task_submitted",
    TaskWeightPredicted => "task_weight_predicted",
    ThroughputReviewFinished => "throughput_review_finished",
    ThroughputReviewReported => "throughput_review_reported",
    ThroughputReviewStarted => "throughput_review_started",
    TriageDecided => "triage_decided",
    TriageFailed => "triage_failed",
    TriageFinished => "triage_finished",
    TriageStarted => "triage_started",
    TurnFinished => "turn_finished",
    TurnRequested => "turn_requested",
    TurnSessionIdentified => "turn_session_identified",
    TurnStarted => "turn_started",
    UpdateAnswered => "update_answered",
    UpdateAwaitingApproval => "update_awaiting_approval",
    UpdateBuilt => "update_built",
    UpdateDropped => "update_dropped",
    UpdateE2ePassed => "update_e2e_passed",
    UpdateFailed => "update_failed",
    UpdateInstalled => "update_installed",
    UpdateRestored => "update_restored",
    UpdateRetry => "update_retry",
    UpdateStarted => "update_started",
    UsageLimited => "usage_limited",
    ValidationFinished => "validation_finished",
    VerificationCommand => "verification_command",
    WorkspaceClosed => "workspace_closed",
    WorkspaceCreated => "workspace_created",
    WorktreeCreated => "worktree_created",
    WorktreeRemoved => "worktree_removed",
    WrapperHeartbeatExpired => "wrapper_heartbeat_expired",
    WrapperStarted => "wrapper_started",
}

impl EventKind {
    /// The kind named `name`, `None` for a kind this binary does not know
    /// (another binary's, which a reader keeps as text).
    pub fn from_name(name: &str) -> Option<EventKind> {
        EventKind::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == name)
    }

    /// Whether an event of this kind may belong to no task, goal or run:
    /// the queue's own events (ADR-0073 decision 22). The queue enumerated
    /// them in a CHECK until migration 0039; the write port checks them
    /// now, in [`super::check_event_target`].
    pub const fn is_queue(self) -> bool {
        use EventKind::*;
        matches!(
            self,
            BackendCallFailed
                // A command the authorizer refused (ADR-t728-1 decision 5).
                | AuthorizationDenied
                | ObserveStarted
                | ObserveFinished
                // The throughput review (ADR-t996-1) is about the queue.
                | ThroughputReviewStarted
                | ThroughputReviewFinished
                | ThroughputReviewReported
                | AskOpened
                | AskAnswered
                | AskClosed
                | StallConfigLoaded
                | FindingRecorded
                | FindingUpdated
                | FindingStatusChanged
                // A finding's planners and the answers to their questions,
                // for a finding on the queue (ADR-0044 decision 19).
                | FindingPlannerOpened
                | FindingPlannerExhausted
                | AskDelivered
                | AskDeliveryFailed
                | PlannerAnswerClosed
                | PlannerAnswerClaimed
                | RunEnvProgramMissing
                | RunEnvProgramFound
                | SessionOpened
                | SessionClosed
                // A planner's screen inferred idle without its idle marker
                // (ADR-t803-1).
                | IdleInferred
                // A planner of the runtime's nothing was seen of within the
                // planner timeout (task 805); the revise's is on its
                // proposal's task.
                | PlannerUnresponsive
                // The supervisor's nudges of an inbox without a watcher
                // (ADR-t906-1 decision 1 (3)).
                | InboxNudged
                | InboxNudgeFailed
                // An idle planner of the runtime's asked to exit so that a
                // revise with no planner, waiting past the planner timeout,
                // gets its place (task 884).
                | PlannerReleased
                | SessionTurns
                | SupervisorStarted
                | SupervisorStopped
                | RunEnvChanged
                | MarkRecorded
                | MarkRetracted
                // The KPI report the supervisor wrote (ADR-0051 decision 20).
                | ReportWritten
                // The supervisor's forecast snapshots (ADR-0070 decision 3).
                | ForecastRecorded
                // The supervisor's samples of the candidates (ADR-0051
                // decision 3).
                | CandidatesSampled
                // The breaches of the KPIs' targets and their push
                // (ADR-0051 decisions 18 and 23).
                | KpiBreachStarted
                | KpiBreachResolved
                | KpiPushSent
                | KpiPushFailed
                | KpiPushAbandoned
                | ClaimHeld
                | ClaimResumed
                // The `[conflicts]` a supervisor read again changed
                // (ADR-0080).
                | ConflictsConfigChanged
                // The `[supervisor]` a supervisor read again changed (task
                // 698).
                | SupervisorConfigChanged
                | LandingHeld
                | LandingResumed
                // The answer of an authentication or usage-limit ask
                // applied (task 437).
                | QueueHoldApplied
                // A headless job with no run (a plan or goal review, the
                // observer) that joined a hold ask, and the wall it hit
                // (task 438).
                | AskUpdated
                | AuthRequired
                | UsageLimited
                // The cleanup for the disk (task 377) is about no run.
                | AutoRepaired
                // The supervisor's resource broker (ADR-t827-3 decisions
                // 2 and 3) is the queue's.
                | BrokerHealthy
                | BrokerImageBuilt
                | BrokerStarted
                | BrokerStopRequested
                | BrokerStopped
                | BrokerUnhealthy
                // The stop of a gone supervisor's plan or goal review (task
                // 443).
                | HeadlessJobStopped
                // The steps of the automatic update (ADR-0073 decision 17).
                | UpdateStarted
                | UpdateBuilt
                | UpdateE2ePassed
                | UpdateInstalled
                | UpdateFailed
                | UpdateRestored
                | UpdateAwaitingApproval
                | UpdateAnswered
                | UpdateRetry
                | UpdateDropped
                // A supervisor's look for a new release (ADR-t618-1
                // decision 2).
                | ReleaseChecked
                | ReleaseCheckFailed
                // A provider's hold and its end (ADR-t813-2 decision 6).
                | ProviderHeld
                | ProviderReleased
        )
    }
}

impl std::fmt::Display for EventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<EventKind> for str {
    fn eq(&self, other: &EventKind) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<EventKind> for &str {
    fn eq(&self, other: &EventKind) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<EventKind> for String {
    fn eq(&self, other: &EventKind) -> bool {
        self == other.as_str()
    }
}

pub const AGENT_STARTED: &str = EventKind::AgentStarted.as_str();
pub const APPROVE_WITHHELD: &str = EventKind::ApproveWithheld.as_str();
pub const ASK_ANSWERED: &str = EventKind::AskAnswered.as_str();
pub const ASK_CLOSED: &str = EventKind::AskClosed.as_str();
pub const ASK_DELIVERED: &str = EventKind::AskDelivered.as_str();
pub const ASK_DELIVERY_FAILED: &str = EventKind::AskDeliveryFailed.as_str();
pub const ASK_OPENED: &str = EventKind::AskOpened.as_str();
pub const ASK_UPDATED: &str = EventKind::AskUpdated.as_str();
pub const AUTH_REQUIRED: &str = EventKind::AuthRequired.as_str();
/// A state-changing command the [`super::Authorizer`] refused (ADR-t728-1
/// decision 5): a queue event whose actor is the refused caller.
pub const AUTHORIZATION_DENIED: &str = EventKind::AuthorizationDenied.as_str();
pub const AUTO_REPAIRED: &str = EventKind::AutoRepaired.as_str();
pub const BACKEND_CALL_FAILED: &str = EventKind::BackendCallFailed.as_str();
pub const BUILD_OUTPUTS_REMOVED: &str = EventKind::BuildOutputsRemoved.as_str();
pub const CLEANUP_FAILED: &str = EventKind::CleanupFailed.as_str();
pub const CONFLICT_PRECHECK: &str = EventKind::ConflictPrecheck.as_str();
pub const CONFLICT_RECEIPT_REJECTED: &str = EventKind::ConflictReceiptRejected.as_str();
pub const CONFLICT_RESOLVED: &str = EventKind::ConflictResolved.as_str();
pub const DEPENDENCY_ADDED: &str = EventKind::DependencyAdded.as_str();
pub const DEPENDENCY_REMOVED: &str = EventKind::DependencyRemoved.as_str();
/// A task of a closed goal that will not complete has tasks waiting on it
/// (task 421): on that task, with `goal_id`, `verdict`, `waiting` and
/// `cause` (`approve_withheld` or `goal_abandoned`). The inbox's attention.
pub const DEPENDENCY_STRANDED: &str = EventKind::DependencyStranded.as_str();
pub const DRAFT_ADOPTED: &str = EventKind::DraftAdopted.as_str();
pub const DRAFT_PLANNER_EXHAUSTED: &str = EventKind::DraftPlannerExhausted.as_str();
pub const DRAFT_PLANNER_OPENED: &str = EventKind::DraftPlannerOpened.as_str();
pub const DRAFT_PLANNER_SETTLED: &str = EventKind::DraftPlannerSettled.as_str();
pub const EVIDENCE_MISSING: &str = EventKind::EvidenceMissing.as_str();
pub const EXIT_REQUESTED: &str = EventKind::ExitRequested.as_str();
pub const EXIT_REQUEST_TIMED_OUT: &str = EventKind::ExitRequestTimedOut.as_str();
/// A retry of a `/exit` the session held back (ADR-0047 decision 25):
/// `attempt`, `cause` (`exit_timeout` / `backend_timeout`), `screen`
/// (`input_ready` / `input_pending` / `dialog` / `not_ready` /
/// `unreadable`) and what it will `send`, recorded before it is sent.
pub const EXIT_RETRIED: &str = EventKind::ExitRetried.as_str();
pub const EXIT_UNSENT: &str = EventKind::ExitUnsent.as_str();
pub const FINDING_PLANNER_EXHAUSTED: &str = EventKind::FindingPlannerExhausted.as_str();
pub const FINDING_PLANNER_OPENED: &str = EventKind::FindingPlannerOpened.as_str();
pub const FINDING_RECORDED: &str = EventKind::FindingRecorded.as_str();
pub const FINDING_STATUS_CHANGED: &str = EventKind::FindingStatusChanged.as_str();
pub const FINDING_UPDATED: &str = EventKind::FindingUpdated.as_str();
pub const FIRST_COMMIT_OBSERVED: &str = EventKind::FirstCommitObserved.as_str();
pub const FOLLOW_UP_ADOPTED: &str = EventKind::FollowUpAdopted.as_str();
pub const FOLLOW_UP_REGISTERED: &str = EventKind::FollowUpRegistered.as_str();
pub const GOAL_CLOSED: &str = EventKind::GoalClosed.as_str();
pub const GOAL_CREATED: &str = EventKind::GoalCreated.as_str();
pub const GOAL_DECIDED: &str = EventKind::GoalDecided.as_str();
pub const GOAL_DEPENDENCY_ADDED: &str = EventKind::GoalDependencyAdded.as_str();
pub const GOAL_DEPENDENCY_REMOVED: &str = EventKind::GoalDependencyRemoved.as_str();
pub const GOAL_REVIEW_FAILED: &str = EventKind::GoalReviewFailed.as_str();
pub const GOAL_REVIEW_FINISHED: &str = EventKind::GoalReviewFinished.as_str();
pub const GOAL_REVIEW_REARMED: &str = EventKind::GoalReviewRearmed.as_str();
pub const GOAL_REVIEW_STARTED: &str = EventKind::GoalReviewStarted.as_str();
pub const GOAL_STATUS_CHANGED: &str = EventKind::GoalStatusChanged.as_str();
pub const GOAL_SUBMITTED: &str = EventKind::GoalSubmitted.as_str();
pub const GOAL_UPDATED: &str = EventKind::GoalUpdated.as_str();
pub const HEADLESS_JOB_STOPPED: &str = EventKind::HeadlessJobStopped.as_str();
pub const HOLD_ANSWER_APPLIED: &str = EventKind::HoldAnswerApplied.as_str();
pub const HOLD_CONTINUE_SENT: &str = EventKind::HoldContinueSent.as_str();
/// A session without a fresh idle marker whose screen was inferred idle
/// (ADR-t803-1), once per span.
pub const IDLE_INFERRED: &str = EventKind::IdleInferred.as_str();
/// The supervisor typed a line into the inbox without a watcher, or told a
/// person by `cmux notify` (ADR-t906-1 decision 1 (3)): its claim, once per
/// absence and attempt.
pub const INBOX_NUDGED: &str = EventKind::InboxNudged.as_str();
/// A nudge of the inbox the supervisor could not deliver.
pub const INBOX_NUDGE_FAILED: &str = EventKind::InboxNudgeFailed.as_str();
pub const INPUT_NOT_READY: &str = EventKind::InputNotReady.as_str();
pub const INTEGRATION_APPROVED: &str = EventKind::IntegrationApproved.as_str();
pub const INTEGRATION_DEFERRED: &str = EventKind::IntegrationDeferred.as_str();
pub const INTEGRATION_ERROR: &str = EventKind::IntegrationError.as_str();
pub const INTEGRATION_FAILED: &str = EventKind::IntegrationFailed.as_str();
pub const INTEGRATION_HELD: &str = EventKind::IntegrationHeld.as_str();
pub const INTEGRATION_REBASED: &str = EventKind::IntegrationRebased.as_str();
pub const INTEGRATION_REBASE_ABORTED: &str = EventKind::IntegrationRebaseAborted.as_str();
pub const INTEGRATION_RECEIPT: &str = EventKind::IntegrationReceipt.as_str();
pub const INTEGRATION_RETRIED: &str = EventKind::IntegrationRetried.as_str();
pub const INTEGRATION_STARTED: &str = EventKind::IntegrationStarted.as_str();
pub const JOB_RESTARTED: &str = EventKind::JobRestarted.as_str();
pub const KNOWN_DIALOG_UNANSWERED: &str = EventKind::KnownDialogUnanswered.as_str();
pub const LANDING_DECIDED: &str = EventKind::LandingDecided.as_str();
pub const LANDING_QUEUED: &str = EventKind::LandingQueued.as_str();
pub const LANDING_RECHECK_FAILED: &str = EventKind::LandingRecheckFailed.as_str();
pub const LEASE_ACQUIRED: &str = EventKind::LeaseAcquired.as_str();
pub const LEASE_RELEASED: &str = EventKind::LeaseReleased.as_str();
pub const MIGRATION_RENUMBERED: &str = EventKind::MigrationRenumbered.as_str();
pub const OBSERVATION: &str = EventKind::Observation.as_str();
pub const OBSERVE_FINISHED: &str = EventKind::ObserveFinished.as_str();
pub const OBSERVE_STARTED: &str = EventKind::ObserveStarted.as_str();
pub const PLANNER_ANSWER_CLAIMED: &str = EventKind::PlannerAnswerClaimed.as_str();
pub const PLANNER_ANSWER_CLOSED: &str = EventKind::PlannerAnswerClosed.as_str();
pub const PLANNER_RELEASED: &str = EventKind::PlannerReleased.as_str();
pub const PLANNER_UNRESPONSIVE: &str = EventKind::PlannerUnresponsive.as_str();
pub const PLAN_DECIDED: &str = EventKind::PlanDecided.as_str();
pub const PLAN_REVIEW_DISCARDED: &str = EventKind::PlanReviewDiscarded.as_str();
pub const PLAN_REVIEW_FAILED: &str = EventKind::PlanReviewFailed.as_str();
pub const PLAN_REVIEW_FINISHED: &str = EventKind::PlanReviewFinished.as_str();
/// What a person's answer to the `approve_plan` ask of a concern says of
/// plan review's findings (ADR-t947-1 decision 4).
pub const PLAN_REVIEW_OUTCOME: &str = EventKind::PlanReviewOutcome.as_str();
pub const PLAN_REVIEW_STARTED: &str = EventKind::PlanReviewStarted.as_str();
pub const PLAN_REVISE_LOST: &str = EventKind::PlanReviseLost.as_str();
pub const PLAN_REVISE_SENT: &str = EventKind::PlanReviseSent.as_str();
pub const PROMPT_CLEARED: &str = EventKind::PromptCleared.as_str();
pub const PROMPT_WAITING: &str = EventKind::PromptWaiting.as_str();
pub const PROPOSAL_RESUBMITTED: &str = EventKind::ProposalResubmitted.as_str();
pub const PROPOSAL_SETTLED: &str = EventKind::ProposalSettled.as_str();
pub const PROPOSAL_WITHDRAWN: &str = EventKind::ProposalWithdrawn.as_str();
pub const PUSH_FAILED: &str = EventKind::PushFailed.as_str();
pub const PUSH_FINISHED: &str = EventKind::PushFinished.as_str();
pub const PUSH_SKIPPED: &str = EventKind::PushSkipped.as_str();
pub const QUEUE_HOLD_APPLIED: &str = EventKind::QueueHoldApplied.as_str();
pub const RECEIPT_OBSERVED: &str = EventKind::ReceiptObserved.as_str();
/// A provider the workers could not use is held (ADR-t813-2 decision 6):
/// a queue event with `provider`, `reason` (`authentication`,
/// `usage_limit`, `launch_failed`), `since`, `retry_at`, `reset_read`,
/// `run_id` and `supervisor`. Codex's holds, and Claude's for an agent that
/// did not start; Claude's login or usage limit is the queue's
/// `queue_hold` ask.
pub const PROVIDER_HELD: &str = EventKind::ProviderHeld.as_str();
/// A provider's hold ended (`provider`, `reason`, `why`: `retry_due`, or
/// `done` when a person answered a hold ask): the next call of it checks it
/// again.
pub const PROVIDER_RELEASED: &str = EventKind::ProviderReleased.as_str();
/// A run's worker moved to the other provider because its own could not be
/// used (ADR-t813-2 decisions 2 and 4): `from`, `to`, `reason`, `phase`,
/// `turn`, `count`, `worker_mode` and `message`.
pub const PROVIDER_SWITCHED: &str = EventKind::ProviderSwitched.as_str();
/// A headless run's turn failed because its provider cannot be used, and
/// the run cannot move to the other provider now (ADR-t813-2): it waits,
/// not failed (`turn`, `provider`, `reason`, `other`, `blocked`,
/// `retry_at`), until its provider's hold ends (a `provider retry`
/// request), the other provider can take it (`provider_switched`) or a
/// person answers the hold ask it joined.
pub const PROVIDER_WAITING: &str = EventKind::ProviderWaiting.as_str();
pub const RECOVERY_FAILED: &str = EventKind::RecoveryFailed.as_str();
pub const RECOVERY_FINISHED: &str = EventKind::RecoveryFinished.as_str();
pub const RECOVERY_PARKED: &str = EventKind::RecoveryParked.as_str();
pub const RECOVERY_REQUESTED: &str = EventKind::RecoveryRequested.as_str();
pub const RESUME_FINISHED: &str = EventKind::ResumeFinished.as_str();
pub const RESUME_SKIPPED: &str = EventKind::ResumeSkipped.as_str();
pub const RESUME_STARTED: &str = EventKind::ResumeStarted.as_str();
/// A supervisor of a release build read crates.io's sparse index
/// (ADR-t618-1 decision 2): a queue event with `latest` (the newest
/// release, `null` for none), `current` (the supervisor's build),
/// `plugin` (`null` until the plugin is read), `checked_at` (unix seconds)
/// and `etag`. Not an attention.
pub const RELEASE_CHECKED: &str = EventKind::ReleaseChecked.as_str();
/// The index could not be read (`error`, `checked_at`, `current`). Not an
/// attention; the next look reads it again.
pub const RELEASE_CHECK_FAILED: &str = EventKind::ReleaseCheckFailed.as_str();
pub const REVIEW_BYPASSED: &str = EventKind::ReviewBypassed.as_str();
pub const REVIEW_FAILED: &str = EventKind::ReviewFailed.as_str();
pub const REVIEW_FINISHED: &str = EventKind::ReviewFinished.as_str();
/// What a person's answer to the `approve_landing` ask of a concern says
/// of the review's findings (ADR-t947-1 decision 4).
pub const REVIEW_OUTCOME: &str = EventKind::ReviewOutcome.as_str();
pub const REVIEW_RETRIED: &str = EventKind::ReviewRetried.as_str();
pub const REVIEW_STARTED: &str = EventKind::ReviewStarted.as_str();
pub const REVISE_FINISHED: &str = EventKind::ReviseFinished.as_str();
pub const REVISE_RECEIPT_REJECTED: &str = EventKind::ReviseReceiptRejected.as_str();
pub const REVISE_REQUESTED: &str = EventKind::ReviseRequested.as_str();
pub const REVISE_UNSENT: &str = EventKind::ReviseUnsent.as_str();
pub const RUNTIME_ERROR: &str = EventKind::RuntimeError.as_str();
pub const RUN_ADOPTED: &str = EventKind::RunAdopted.as_str();
pub const RUN_CLAIMED: &str = EventKind::RunClaimed.as_str();
pub const RUN_INHERITED: &str = EventKind::RunInherited.as_str();
pub const RUN_INTEGRATED: &str = EventKind::RunIntegrated.as_str();
pub const RUN_PLANNED: &str = EventKind::RunPlanned.as_str();
pub const RUN_RECOVERED: &str = EventKind::RunRecovered.as_str();
pub const SCOPE_VIOLATION: &str = EventKind::ScopeViolation.as_str();
/// The supervisor removed the Claude Code scratchpads of an ended run
/// whose task is over (`paths`, `bytes`, `by`, `reason`; task 1100).
pub const SCRATCHPAD_REMOVED: &str = EventKind::ScratchpadRemoved.as_str();
pub const SCREEN_CAPTURE_FAILED: &str = EventKind::ScreenCaptureFailed.as_str();
pub const SESSION_CLOSED: &str = EventKind::SessionClosed.as_str();
pub const SESSION_EXITED: &str = EventKind::SessionExited.as_str();
/// An adopter parked a run for a resume, whose `/exit` never reached its
/// session, whose workspace was gone, and which could not land without that
/// session (task 960): `code: session_gone`, `workspace_id`, `held`, the
/// status and the reason.
pub const SESSION_GONE_PARKED: &str = EventKind::SessionGoneParked.as_str();
pub const SESSION_IDLE_OBSERVED: &str = EventKind::SessionIdleObserved.as_str();
pub const SESSION_OPENED: &str = EventKind::SessionOpened.as_str();
pub const SESSION_TURNS: &str = EventKind::SessionTurns.as_str();
pub const STALE_RECEIPT_NUDGED: &str = EventKind::StaleReceiptNudged.as_str();
pub const STALE_RECEIPT_RESOLVED: &str = EventKind::StaleReceiptResolved.as_str();
pub const STALL_CONFIG_LOADED: &str = EventKind::StallConfigLoaded.as_str();
pub const STALL_NUDGED: &str = EventKind::StallNudged.as_str();
pub const STALL_PREEMPTED: &str = EventKind::StallPreempted.as_str();
pub const STALL_RESOLVED: &str = EventKind::StallResolved.as_str();
pub const SUBMIT_NOT_STARTED: &str = EventKind::SubmitNotStarted.as_str();
pub const SUBMIT_RESENT: &str = EventKind::SubmitResent.as_str();
pub const SUBMIT_RETRIED: &str = EventKind::SubmitRetried.as_str();
pub const SUBMIT_UNCONFIRMED: &str = EventKind::SubmitUnconfirmed.as_str();
pub const SUPERVISION_FINISHED: &str = EventKind::SupervisionFinished.as_str();
pub const TASK_CREATED: &str = EventKind::TaskCreated.as_str();
pub const TASK_EDITED: &str = EventKind::TaskEdited.as_str();
pub const TASK_GOAL_CHANGED: &str = EventKind::TaskGoalChanged.as_str();
pub const TASK_PATHS_CHANGED: &str = EventKind::TaskPathsChanged.as_str();
pub const TASK_PRIORITY_CHANGED: &str = EventKind::TaskPriorityChanged.as_str();
pub const TASK_REOPENED: &str = EventKind::TaskReopened.as_str();
pub const TASK_STATUS_CHANGED: &str = EventKind::TaskStatusChanged.as_str();
pub const TASK_SUBMITTED: &str = EventKind::TaskSubmitted.as_str();
pub const TASK_WEIGHT_PREDICTED: &str = EventKind::TaskWeightPredicted.as_str();
/// A throughput review ended (ADR-t996-1): `mode`, `period`, `outcome`
/// (`skipped` for an hour no rule hit, `succeeded`, `failed`, `error`).
pub const THROUGHPUT_REVIEW_FINISHED: &str = EventKind::ThroughputReviewFinished.as_str();
/// A throughput review's conclusion for the inbox: a notice (`report the
/// review`) that asks nothing.
pub const THROUGHPUT_REVIEW_REPORTED: &str = EventKind::ThroughputReviewReported.as_str();
/// A throughput review started its job.
pub const THROUGHPUT_REVIEW_STARTED: &str = EventKind::ThroughputReviewStarted.as_str();
pub const TRIAGE_DECIDED: &str = EventKind::TriageDecided.as_str();
pub const TRIAGE_FAILED: &str = EventKind::TriageFailed.as_str();
pub const TRIAGE_FINISHED: &str = EventKind::TriageFinished.as_str();
pub const TRIAGE_STARTED: &str = EventKind::TriageStarted.as_str();
/// A headless worker's agent named the session it started in its output
/// (Codex's `thread.started`, ADR-t813-1): `turn`, `session_id` and
/// `provider`, recorded by the session wrapper as soon as it is read. The
/// later turns resume the last one recorded.
pub const TURN_SESSION_IDENTIFIED: &str = EventKind::TurnSessionIdentified.as_str();
/// A headless worker's turn ended (ADR-t813-1): its `turn`, `outcome`,
/// `failure`, `session_id`, `usage` and `permission_denials`, recorded by
/// the session wrapper.
pub const TURN_FINISHED: &str = EventKind::TurnFinished.as_str();
/// The supervisor asked a headless worker's session for its next turn
/// (`seq`, `what`), where it would type into an interactive one.
pub const TURN_REQUESTED: &str = EventKind::TurnRequested.as_str();
/// A headless worker's turn started (`turn`, `resume`, `request`, `pid`).
pub const TURN_STARTED: &str = EventKind::TurnStarted.as_str();
/// A worker's session or a headless job stopped at Claude Code's usage
/// limit (ADR-0047 decision 42): on the run, or on the queue for a job
/// without one.
pub const USAGE_LIMITED: &str = EventKind::UsageLimited.as_str();
pub const VALIDATION_FINISHED: &str = EventKind::ValidationFinished.as_str();
pub const VERIFICATION_COMMAND: &str = EventKind::VerificationCommand.as_str();
pub const WORKSPACE_CLOSED: &str = EventKind::WorkspaceClosed.as_str();
pub const WORKSPACE_CREATED: &str = EventKind::WorkspaceCreated.as_str();
pub const WORKTREE_CREATED: &str = EventKind::WorktreeCreated.as_str();
pub const WORKTREE_REMOVED: &str = EventKind::WorktreeRemoved.as_str();
pub const WRAPPER_HEARTBEAT_EXPIRED: &str = EventKind::WrapperHeartbeatExpired.as_str();
pub const WRAPPER_STARTED: &str = EventKind::WrapperStarted.as_str();

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind writes the text it wrote before it had a type (ADR-0073
    /// decision 20), which readers and other binaries match on.
    #[test]
    fn each_kind_writes_the_text_it_always_wrote() {
        let table = [
            (EventKind::AgentStarted, "agent_started"),
            (EventKind::ApproveWithheld, "approve_withheld"),
            (EventKind::AskAnswered, "ask_answered"),
            (EventKind::AskClosed, "ask_closed"),
            (EventKind::AskDelivered, "ask_delivered"),
            (EventKind::AskDeliveryFailed, "ask_delivery_failed"),
            (EventKind::AskOpened, "ask_opened"),
            (EventKind::AskUpdated, "ask_updated"),
            (EventKind::AuthRequired, "auth_required"),
            (EventKind::AuthorizationDenied, "authorization_denied"),
            (EventKind::AutoRepaired, "auto_repaired"),
            (EventKind::BackendCallFailed, "backend_call_failed"),
            (EventKind::BrokerHealthy, "broker_healthy"),
            (EventKind::BrokerImageBuilt, "broker_image_built"),
            (EventKind::BrokerStarted, "broker_started"),
            (EventKind::BrokerStopRequested, "broker_stop_requested"),
            (EventKind::BrokerStopped, "broker_stopped"),
            (EventKind::BrokerTokenIssued, "broker_token_issued"),
            (EventKind::BrokerTokenRevoked, "broker_token_revoked"),
            (EventKind::BrokerUnavailable, "broker_unavailable"),
            (EventKind::BrokerUnhealthy, "broker_unhealthy"),
            (EventKind::BuildOutputsRemoved, "build_outputs_removed"),
            (EventKind::CandidatesSampled, "candidates_sampled"),
            (EventKind::ClaimDeferralEnded, "claim_deferral_ended"),
            (EventKind::ClaimDeferred, "claim_deferred"),
            (EventKind::ClaimHeld, "claim_held"),
            (EventKind::ClaimResumed, "claim_resumed"),
            (EventKind::CleanupFailed, "cleanup_failed"),
            (EventKind::ConflictPrecheck, "conflict_precheck"),
            (
                EventKind::ConflictReceiptRejected,
                "conflict_receipt_rejected",
            ),
            (EventKind::ConflictResolved, "conflict_resolved"),
            (
                EventKind::ConflictsConfigChanged,
                "conflicts_config_changed",
            ),
            (EventKind::DependencyAdded, "dependency_added"),
            (EventKind::DependencyRemoved, "dependency_removed"),
            (EventKind::DependencyStranded, "dependency_stranded"),
            (EventKind::DraftAdopted, "draft_adopted"),
            (EventKind::DraftPlannerExhausted, "draft_planner_exhausted"),
            (EventKind::DraftPlannerOpened, "draft_planner_opened"),
            (EventKind::DraftPlannerSettled, "draft_planner_settled"),
            (EventKind::EvidenceMissing, "evidence_missing"),
            (EventKind::ExitRequestTimedOut, "exit_request_timed_out"),
            (EventKind::ExitRequested, "exit_requested"),
            (EventKind::ExitRetried, "exit_retried"),
            (EventKind::ExitUnsent, "exit_unsent"),
            (
                EventKind::FindingPlannerExhausted,
                "finding_planner_exhausted",
            ),
            (EventKind::FindingPlannerOpened, "finding_planner_opened"),
            (EventKind::FindingRecorded, "finding_recorded"),
            (EventKind::FindingStatusChanged, "finding_status_changed"),
            (EventKind::FindingUpdated, "finding_updated"),
            (EventKind::FirstCommitObserved, "first_commit_observed"),
            (EventKind::FollowUpAdopted, "follow_up_adopted"),
            (EventKind::FollowUpRegistered, "follow_up_registered"),
            (EventKind::ForecastRecorded, "forecast_recorded"),
            (EventKind::GoalClosed, "goal_closed"),
            (EventKind::GoalCreated, "goal_created"),
            (EventKind::GoalDecided, "goal_decided"),
            (EventKind::GoalDependencyAdded, "goal_dependency_added"),
            (EventKind::GoalDependencyRemoved, "goal_dependency_removed"),
            (EventKind::GoalReviewFailed, "goal_review_failed"),
            (EventKind::GoalReviewFinished, "goal_review_finished"),
            (EventKind::GoalReviewRearmed, "goal_review_rearmed"),
            (EventKind::GoalReviewStarted, "goal_review_started"),
            (EventKind::GoalStatusChanged, "goal_status_changed"),
            (EventKind::GoalSubmitted, "goal_submitted"),
            (EventKind::GoalUpdated, "goal_updated"),
            (EventKind::HeadlessJobStopped, "headless_job_stopped"),
            (EventKind::HoldAnswerApplied, "hold_answer_applied"),
            (EventKind::HoldContinueSent, "hold_continue_sent"),
            (EventKind::IdleInferred, "idle_inferred"),
            (EventKind::InboxNudgeFailed, "inbox_nudge_failed"),
            (EventKind::InboxNudged, "inbox_nudged"),
            (EventKind::InputNotReady, "input_not_ready"),
            (EventKind::IntegrationApproved, "integration_approved"),
            (EventKind::IntegrationDeferred, "integration_deferred"),
            (EventKind::IntegrationError, "integration_error"),
            (EventKind::IntegrationFailed, "integration_failed"),
            (EventKind::IntegrationHeld, "integration_held"),
            (
                EventKind::IntegrationRebaseAborted,
                "integration_rebase_aborted",
            ),
            (EventKind::IntegrationRebased, "integration_rebased"),
            (EventKind::IntegrationReceipt, "integration_receipt"),
            (EventKind::IntegrationRetried, "integration_retried"),
            (EventKind::IntegrationStarted, "integration_started"),
            (EventKind::JobRestarted, "job_restarted"),
            (EventKind::KnownDialogUnanswered, "known_dialog_unanswered"),
            (EventKind::KpiBreachResolved, "kpi_breach_resolved"),
            (EventKind::KpiBreachStarted, "kpi_breach_started"),
            (EventKind::KpiPushAbandoned, "kpi_push_abandoned"),
            (EventKind::KpiPushFailed, "kpi_push_failed"),
            (EventKind::KpiPushSent, "kpi_push_sent"),
            (EventKind::LandingDecided, "landing_decided"),
            (EventKind::LandingHeld, "landing_held"),
            (EventKind::LandingQueued, "landing_queued"),
            (EventKind::LandingRecheckFailed, "landing_recheck_failed"),
            (
                EventKind::LandingRecheckFinished,
                "landing_recheck_finished",
            ),
            (EventKind::LandingResumed, "landing_resumed"),
            (EventKind::LeaseAcquired, "lease_acquired"),
            (EventKind::LeaseReleased, "lease_released"),
            (EventKind::MarkRecorded, "mark_recorded"),
            (EventKind::MarkRetracted, "mark_retracted"),
            (EventKind::MigrationRenumbered, "migration_renumbered"),
            (EventKind::Observation, "observation"),
            (EventKind::ObserveFinished, "observe_finished"),
            (EventKind::ObserveStarted, "observe_started"),
            (EventKind::PlanDecided, "plan_decided"),
            (EventKind::PlanReviewDiscarded, "plan_review_discarded"),
            (EventKind::PlanReviewFailed, "plan_review_failed"),
            (EventKind::PlanReviewFinished, "plan_review_finished"),
            (EventKind::PlanReviewOutcome, "plan_review_outcome"),
            (EventKind::PlanReviewStarted, "plan_review_started"),
            (EventKind::PlanReviseLost, "plan_revise_lost"),
            (EventKind::PlanReviseSent, "plan_revise_sent"),
            (EventKind::PlannerAnswerClaimed, "planner_answer_claimed"),
            (EventKind::PlannerAnswerClosed, "planner_answer_closed"),
            (EventKind::PlannerReleased, "planner_released"),
            (EventKind::PlannerUnresponsive, "planner_unresponsive"),
            (EventKind::PromptCleared, "prompt_cleared"),
            (EventKind::PromptWaiting, "prompt_waiting"),
            (EventKind::ProviderHeld, "provider_held"),
            (EventKind::ProviderReleased, "provider_released"),
            (EventKind::ProviderSwitched, "provider_switched"),
            (EventKind::ProviderWaiting, "provider_waiting"),
            (EventKind::ProposalResubmitted, "proposal_resubmitted"),
            (EventKind::ProposalSettled, "proposal_settled"),
            (EventKind::ProposalWithdrawn, "proposal_withdrawn"),
            (EventKind::PushFailed, "push_failed"),
            (EventKind::PushFinished, "push_finished"),
            (EventKind::PushSkipped, "push_skipped"),
            (EventKind::QueueHoldApplied, "queue_hold_applied"),
            (EventKind::ReceiptObserved, "receipt_observed"),
            (EventKind::RecoveryFailed, "recovery_failed"),
            (EventKind::RecoveryFinished, "recovery_finished"),
            (EventKind::RecoveryParked, "recovery_parked"),
            (EventKind::RecoveryRequested, "recovery_requested"),
            (EventKind::ReleaseCheckFailed, "release_check_failed"),
            (EventKind::ReleaseChecked, "release_checked"),
            (EventKind::ReportWritten, "report_written"),
            (EventKind::ResumeFinished, "resume_finished"),
            (EventKind::ResumeRequestSent, "resume_request_sent"),
            (EventKind::ResumeSkipped, "resume_skipped"),
            (EventKind::ResumeStarted, "resume_started"),
            (EventKind::ReviewBypassed, "review_bypassed"),
            (EventKind::ReviewFailed, "review_failed"),
            (EventKind::ReviewFinished, "review_finished"),
            (EventKind::ReviewOutcome, "review_outcome"),
            (EventKind::ReviewRetried, "review_retried"),
            (EventKind::ReviewStarted, "review_started"),
            (EventKind::ReviseFinished, "revise_finished"),
            (EventKind::ReviseReceiptRejected, "revise_receipt_rejected"),
            (EventKind::ReviseRequested, "revise_requested"),
            (EventKind::ReviseUnsent, "revise_unsent"),
            (EventKind::RunAdopted, "run_adopted"),
            (EventKind::RunClaimed, "run_claimed"),
            (EventKind::RunEnvChanged, "run_env_changed"),
            (EventKind::RunEnvProgramFound, "run_env_program_found"),
            (EventKind::RunEnvProgramMissing, "run_env_program_missing"),
            (EventKind::RunInherited, "run_inherited"),
            (EventKind::RunIntegrated, "run_integrated"),
            (EventKind::RunPlanned, "run_planned"),
            (EventKind::RunRecovered, "run_recovered"),
            (EventKind::RunSlotRegained, "run_slot_regained"),
            (EventKind::RunWaitingAskAdded, "run_waiting_ask_added"),
            (EventKind::RunWaitingDeferred, "run_waiting_deferred"),
            (EventKind::RunWaitingEnded, "run_waiting_ended"),
            (EventKind::RunWaitingStarted, "run_waiting_started"),
            (EventKind::RuntimeError, "runtime_error"),
            (EventKind::ScopeViolation, "scope_violation"),
            (EventKind::ScratchpadRemoved, "scratchpad_removed"),
            (EventKind::ScreenCaptureFailed, "screen_capture_failed"),
            (EventKind::SessionClosed, "session_closed"),
            (EventKind::SessionExited, "session_exited"),
            (EventKind::SessionGoneParked, "session_gone_parked"),
            (EventKind::SessionIdleObserved, "session_idle_observed"),
            (EventKind::SessionOpened, "session_opened"),
            (EventKind::SessionTurns, "session_turns"),
            (EventKind::StaleReceiptNudged, "stale_receipt_nudged"),
            (EventKind::StaleReceiptResolved, "stale_receipt_resolved"),
            (EventKind::StallConfigLoaded, "stall_config_loaded"),
            (EventKind::StallNudged, "stall_nudged"),
            (EventKind::StallPreempted, "stall_preempted"),
            (EventKind::StallResolved, "stall_resolved"),
            (EventKind::SubmitNotStarted, "submit_not_started"),
            (EventKind::SubmitResent, "submit_resent"),
            (EventKind::SubmitRetried, "submit_retried"),
            (EventKind::SubmitUnconfirmed, "submit_unconfirmed"),
            (EventKind::SupervisionFinished, "supervision_finished"),
            (
                EventKind::SupervisorConfigChanged,
                "supervisor_config_changed",
            ),
            (EventKind::SupervisorHandedOff, "supervisor_handed_off"),
            (EventKind::SupervisorStarted, "supervisor_started"),
            (EventKind::SupervisorStopped, "supervisor_stopped"),
            (EventKind::TaskCreated, "task_created"),
            (EventKind::TaskEdited, "task_edited"),
            (EventKind::TaskGoalChanged, "task_goal_changed"),
            (EventKind::TaskPathsChanged, "task_paths_changed"),
            (EventKind::TaskPriorityChanged, "task_priority_changed"),
            (EventKind::TaskReopened, "task_reopened"),
            (EventKind::TaskStatusChanged, "task_status_changed"),
            (EventKind::TaskSubmitted, "task_submitted"),
            (EventKind::TaskWeightPredicted, "task_weight_predicted"),
            (
                EventKind::ThroughputReviewFinished,
                "throughput_review_finished",
            ),
            (
                EventKind::ThroughputReviewReported,
                "throughput_review_reported",
            ),
            (
                EventKind::ThroughputReviewStarted,
                "throughput_review_started",
            ),
            (EventKind::TriageDecided, "triage_decided"),
            (EventKind::TriageFailed, "triage_failed"),
            (EventKind::TriageFinished, "triage_finished"),
            (EventKind::TriageStarted, "triage_started"),
            (EventKind::TurnFinished, "turn_finished"),
            (EventKind::TurnRequested, "turn_requested"),
            (EventKind::TurnSessionIdentified, "turn_session_identified"),
            (EventKind::TurnStarted, "turn_started"),
            (EventKind::UpdateAnswered, "update_answered"),
            (
                EventKind::UpdateAwaitingApproval,
                "update_awaiting_approval",
            ),
            (EventKind::UpdateBuilt, "update_built"),
            (EventKind::UpdateDropped, "update_dropped"),
            (EventKind::UpdateE2ePassed, "update_e2e_passed"),
            (EventKind::UpdateFailed, "update_failed"),
            (EventKind::UpdateInstalled, "update_installed"),
            (EventKind::UpdateRestored, "update_restored"),
            (EventKind::UpdateRetry, "update_retry"),
            (EventKind::UpdateStarted, "update_started"),
            (EventKind::UsageLimited, "usage_limited"),
            (EventKind::ValidationFinished, "validation_finished"),
            (EventKind::VerificationCommand, "verification_command"),
            (EventKind::WorkspaceClosed, "workspace_closed"),
            (EventKind::WorkspaceCreated, "workspace_created"),
            (EventKind::WorktreeCreated, "worktree_created"),
            (EventKind::WorktreeRemoved, "worktree_removed"),
            (
                EventKind::WrapperHeartbeatExpired,
                "wrapper_heartbeat_expired",
            ),
            (EventKind::WrapperStarted, "wrapper_started"),
        ];
        assert_eq!(table.len(), EventKind::ALL.len());
        for (kind, text) in table {
            assert_eq!(kind.as_str(), text);
            assert_eq!(kind.to_string(), text);
            assert_eq!(EventKind::from_name(text), Some(kind));
            assert!(*text == kind);
            let owned: String = text.into();
            assert!(owned == kind);
        }
        let mut names: Vec<&str> = EventKind::ALL.iter().map(|kind| kind.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), EventKind::ALL.len());
        assert_eq!(EventKind::from_name("future_kind"), None);
    }

    #[test]
    fn the_module_constants_are_their_kinds_text() {
        assert_eq!(RUN_CLAIMED, "run_claimed");
        assert_eq!(OBSERVATION, "observation");
        assert_eq!(super::super::claim_hold::CLAIM_HELD, "claim_held");
        assert_eq!(super::super::UPDATE_STARTED, "update_started");
        assert_eq!(super::super::marks::MARK_RECORDED, "mark_recorded");
    }
}
