//! What an actor may do (ADR-t728-1 decision 5): a [`Capability`] on a
//! [`Resource`], allowed or refused by an [`Authorizer`]. The policy is a
//! static allowlist per [`ActorRole`] plus the resource rules of a few
//! roles (a worker acts on its own run, a planner withdraws its own
//! proposal and changes tasks that have not started, but none of another
//! planner's revising proposal). Everything not
//! listed is refused (default deny), and so is a resource whose owner the
//! rule needs but the caller could not name (fail closed). On a host the
//! check is advisory: any process can set `DAGQ_ROLE` (ADR-t728-1
//! decision 6). The table is in `docs/design/authorization.md`.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{
    ActorContext, ActorRole, AskId, AskKind, FindingId, GoalId, PlannerId, ProposalId,
    ProposalStatus, RequestId, RunId,
};
use super::{DomainError, Priority, TaskId, TaskStatus, error::require, plan_request::PriorityBy};

string_enum!(Capability {
    // Reading.
    QueueRead => "queue.read",
    QueueWatch => "queue.watch",
    ExportFile => "queue.export",
    // The tests that fail already on the watched branch (`ci failures`,
    // ADR-t1920-1); a worker and a job get them in their prompt.
    CiRead => "ci.read",
    // Planning.
    GoalWrite => "goal.write",
    GoalReady => "goal.ready",
    GoalClose => "goal.close",
    GoalReviewRequest => "goal.review_request",
    /// Changing a task before it starts. `revisit` (a draft's time to be
    /// looked at again) is one of them rather than a capability of its
    /// own, since the roles allowed it are the same.
    TaskWrite => "task.write",
    FollowUpJudge => "follow_up.judge",
    TaskVerifyEdit => "task.verify_edit",
    TaskCancel => "task.cancel",
    TaskReady => "task.ready",
    TaskReadyBypassReview => "task.ready_bypass_review",
    ProposalSubmit => "proposal.submit",
    ProposalWithdraw => "proposal.withdraw",
    NoteWrite => "note.write",
    MarkWrite => "mark.write",
    // A worker's (and a session's) communication.
    AskOpen => "ask.open",
    SessionRun => "session.run",
    SessionRecord => "session.record",
    // Review and triage.
    ReviewSubmit => "review.submit",
    TriageSubmit => "triage.submit",
    PrepareReview => "review.prepare",
    // Observation.
    FindingRecord => "finding.record",
    FindingResolve => "finding.resolve",
    FindingDismiss => "finding.dismiss",
    FindingAsk => "finding.ask",
    ObserveRun => "observe.run",
    // Dialogue with a person.
    AskAnswer => "ask.answer",
    AskClose => "ask.close",
    PlannerOpen => "planner.open",
    // A planning request for a planner of the runtime's, recorded at a
    // person's word, and declined by its planner (ADR-t1394-1 decisions 3
    // and 6).
    RequestRecord => "request.record",
    RequestDecline => "request.decline",
    // The replanning of a run (ADR-t1521-1, ADR-t1521-2): a person's
    // request, a worker's proposal on its own run, and the park and the
    // replacement only the trusted runtime does.
    ReplanRequest => "replan.request",
    ReplanPropose => "replan.propose",
    ReplanPark => "replan.park",
    ReplanApply => "replan.apply",
    // The hold of a task and its release (ADR-t1879-1 decision 3): a
    // person's and the inbox's only.
    TaskHold => "task.hold",
    // A session's screen read and keys or an answer typed into it, by
    // its run or planner id (ADR-t1228-1 decisions 4 and 5).
    ScreenRead => "screen.read",
    ScreenSend => "screen.send",
    // A follow-up request handed to a headless planner of the runtime's
    // as its next turn, by its planner id (ADR-t1533-1).
    PlannerRequest => "planner.request",
    // The eval of an agent (ADR-t1728-1 decision 6): asking for a dev
    // round, for a hold-out or production round, running a held-out set
    // again, and reading the rounds and their scores. Running a round is
    // the supervisor's `scheduler.supervise`, never a request of its own.
    EvalRequest => "eval.request",
    EvalRequestHeldOut => "eval.request_held_out",
    EvalRerun => "eval.rerun",
    EvalRead => "eval.read",
    // A production set of an agent's cases (ADR-t1728-1 (g)): asking for
    // one, which the supervisor makes, and deciding a case's label.
    EvalCases => "eval.cases",
    // Generating the review's settings of the checkout with a coding agent
    // and writing them to its working tree at a person's approval
    // (`setup-review`, ADR-t2259-1 decision 7): a person's and the inbox's
    // only.
    ReviewSetup => "review.setup",
    // Approving those changes beforehand (`setup-review --yes`): a
    // person's alone, never the inbox's, as the changes reach the working
    // tree only at a person's approval.
    ReviewSetupApprove => "review.setup_approve",
    // Scheduler transitions and the service.
    Supervise => "scheduler.supervise",
    RunRecover => "run.recover",
    // Closing the workspaces ended runs left open (ADR-t1228-1 decision 6).
    WorkspaceCleanup => "workspace.cleanup",
    ServiceLifecycle => "service.lifecycle",
    BinaryInstall => "service.install",
    QueueAdmin => "queue.admin",
    // Landing and push (ADR-t728-2).
    IntegrationRequest => "landing.request",
    Land => "landing.land",
    Push => "landing.push",
    // The steps of a session an executor sends of its own target
    // (`telemetry_report`, ADR-t1662-3 decision 9): the executor's
    // telemetry principal's one capability, and no other role's.
    TelemetryReport => "telemetry.report",
    // Asking the queue service to issue the token of a principal this one
    // starts (`issue_token`, ADR-t3102-1 decision 2): which child a parent
    // may ask for is [`starts`].
    TokenIssue => "token.issue",
    // Recording what the queue broker refused and the verifications'
    // tokens it or a supervisor swept (`broker_record`): the user's, which
    // the broker is on the host's socket, and the supervisor's, never an
    // AI actor's.
    QueueBrokerRecord => "queue_broker.record",
    // Reserved for a later stage (sandboxed execution): no role has them.
    FilesystemRead => "reserved.filesystem_read",
    FilesystemWrite => "reserved.filesystem_write",
    NetworkAccess => "reserved.network",
    SecretRead => "reserved.secret_read",
});

impl Capability {
    pub const ALL: [Self; 65] = [
        Self::QueueRead,
        Self::QueueWatch,
        Self::ExportFile,
        Self::CiRead,
        Self::GoalWrite,
        Self::GoalReady,
        Self::GoalClose,
        Self::GoalReviewRequest,
        Self::TaskWrite,
        Self::FollowUpJudge,
        Self::TaskVerifyEdit,
        Self::TaskCancel,
        Self::TaskReady,
        Self::TaskReadyBypassReview,
        Self::ProposalSubmit,
        Self::ProposalWithdraw,
        Self::NoteWrite,
        Self::MarkWrite,
        Self::AskOpen,
        Self::SessionRun,
        Self::SessionRecord,
        Self::ReviewSubmit,
        Self::TriageSubmit,
        Self::PrepareReview,
        Self::FindingRecord,
        Self::FindingResolve,
        Self::FindingDismiss,
        Self::FindingAsk,
        Self::ObserveRun,
        Self::AskAnswer,
        Self::AskClose,
        Self::PlannerOpen,
        Self::RequestRecord,
        Self::RequestDecline,
        Self::ReplanRequest,
        Self::ReplanPropose,
        Self::ReplanPark,
        Self::ReplanApply,
        Self::TaskHold,
        Self::ScreenRead,
        Self::ScreenSend,
        Self::PlannerRequest,
        Self::EvalRequest,
        Self::EvalRequestHeldOut,
        Self::EvalRerun,
        Self::EvalRead,
        Self::EvalCases,
        Self::ReviewSetup,
        Self::ReviewSetupApprove,
        Self::Supervise,
        Self::RunRecover,
        Self::WorkspaceCleanup,
        Self::ServiceLifecycle,
        Self::BinaryInstall,
        Self::QueueAdmin,
        Self::IntegrationRequest,
        Self::Land,
        Self::Push,
        Self::TelemetryReport,
        Self::TokenIssue,
        Self::QueueBrokerRecord,
        Self::FilesystemRead,
        Self::FilesystemWrite,
        Self::NetworkAccess,
        Self::SecretRead,
    ];

    /// Named for a later stage and given to no role in this one.
    pub const fn is_reserved(self) -> bool {
        matches!(
            self,
            Self::FilesystemRead | Self::FilesystemWrite | Self::NetworkAccess | Self::SecretRead
        )
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a capability acts on, with the owner a rule needs when the caller
/// knows it. `None` for an owner or a status is "not known", which a rule
/// that needs it refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    /// The queue as a whole: reads, and what names nothing smaller.
    Queue,
    Goal(GoalId),
    /// A task and its status when known, with the proposal it is in when
    /// the caller read its status (`None` for a task in no proposal, or one
    /// named without its status). A planner changes no task of a revising
    /// proposal that it does not own, nor of one whose owner the queue does
    /// not name as an actor.
    Task {
        id: TaskId,
        status: Option<TaskStatus>,
        proposal: Option<TaskProposal>,
    },
    /// A run and its task when known.
    Run {
        id: RunId,
        task: Option<TaskId>,
    },
    /// An ask and the run it is on when known.
    Ask {
        id: AskId,
        run: Option<RunId>,
    },
    /// An ask to open: its kind, and the run or the task it is about as
    /// the command names them; for a `planner_question` on a planning
    /// request (`--request`), the request and the planner of the runtime's
    /// open for it, the only planner that asks on it (task 1564).
    NewAsk {
        kind: AskKind,
        run: Option<RunId>,
        task: Option<TaskId>,
        request: Option<RequestId>,
        planner: Option<PlannerId>,
    },
    /// A proposal and the actor id of the planner that submitted it.
    Proposal {
        id: ProposalId,
        owner: Option<String>,
    },
    Finding(FindingId),
    Planner(PlannerId),
    /// A planning request and the planner of the runtime's open for it,
    /// the one that may decline it (ADR-t1394-1 decision 6).
    Request {
        id: RequestId,
        planner: Option<PlannerId>,
    },
    /// The eval of an agent (ADR-t1728-1): a round to ask for or to read,
    /// with the run it was asked for (a worker's own), `None` for one
    /// asked outside a run.
    AgentEval {
        run: Option<RunId>,
    },
    /// The target a telemetry report names its steps of (ADR-t1662-3
    /// decision 10), and the owner the control side registered the
    /// report's session to (`None` for a session never registered): only
    /// the principal issued for the target sends them, and only of a
    /// session registered to it.
    Telemetry {
        target: super::session_step::StepTarget,
        owner: Option<super::session_step::StepTarget>,
    },
    /// The principal of `role` a token is asked to be issued for
    /// ([`C::TokenIssue`]).
    Principal(ActorRole),
    /// A resource the caller named but could not read (such as a malformed
    /// run id): no owner can match it.
    Unresolved,
}

/// The proposal a task is in, as a change of the task is authorized with
/// it: its status and the actor id of the planner that owns it now (the
/// planner a revise went to while it is revising), `None` when the queue
/// recorded none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskProposal {
    pub id: ProposalId,
    pub status: ProposalStatus,
    pub owner: Option<String>,
}

impl TaskProposal {
    /// Whether `actor` may change the tasks of this proposal as a planner:
    /// any but a revising one, which only its owner may, and an owner that
    /// names a role but no one actor (`planner`) is no owner.
    fn open_to(&self, actor: &ActorContext) -> bool {
        self.status != ProposalStatus::Revising
            || self
                .owner
                .as_deref()
                .is_some_and(|owner| owner.contains(':') && owner == actor.actor_id())
    }
}

/// What a planning command's change of a task was authorized with: the
/// task's status and the proposal it is in, as the store read them before.
/// The store reads them again in its transaction and refuses the change,
/// writing nothing, when any of them differs (ADR-t883-1). Without it, a
/// planner's `cancel` or `draft` authorized on a `ready` task would reach
/// one claimed in between whose run failed (the domain lets an `in_progress`
/// task with an ended run go to either), and a planner's change authorized
/// on a draft would reach it after plan review moved it into another
/// planner's revising proposal, or sent its proposal to another planner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskAuthorization {
    pub status: TaskStatus,
    pub proposal: Option<TaskProposal>,
}

impl TaskAuthorization {
    /// A task in no proposal with `status`.
    pub const fn outside_proposals(status: TaskStatus) -> Self {
        Self {
            status,
            proposal: None,
        }
    }

    /// The task `id` as the authorizer is asked about it.
    pub fn resource(&self, id: TaskId) -> Resource {
        Resource::Task {
            id,
            status: Some(self.status),
            proposal: self.proposal.clone(),
        }
    }

    /// Refuse `now`, the task `task_id` as the store reads it in the
    /// transaction of the change, unless it is what the change was
    /// authorized with: another status is refused as such, another
    /// proposal, proposal status or owner as a change of its proposal.
    pub fn check_unchanged(&self, task_id: TaskId, now: &Self) -> Result<(), DomainError> {
        require(self.status == now.status, || {
            DomainError::TaskStatusChangedSinceAuthorized {
                task_id,
                authorized: self.status,
                status: now.status,
            }
        })?;
        require(self.proposal == now.proposal, || {
            DomainError::TaskProposalChangedSinceAuthorized { task_id }
        })
    }
}

impl Resource {
    pub const fn task(id: TaskId) -> Self {
        Self::Task {
            id,
            status: None,
            proposal: None,
        }
    }

    pub const fn run(id: RunId) -> Self {
        Self::Run { id, task: None }
    }

    /// The resource as a refusal records it (`authorization_denied`): its
    /// kind, its id, and the status or owner the rule was given.
    pub fn record(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Self::Queue => json!({"kind": "queue"}),
            Self::Goal(id) => json!({"kind": "goal", "id": id}),
            Self::Task {
                id,
                status,
                proposal,
            } => {
                let mut record = json!({"kind": "task", "id": id, "status": status});
                // A task in a proposal names it, its status and its owner.
                if let Some(proposal) = proposal {
                    record["proposal"] = json!({
                        "id": proposal.id,
                        "status": proposal.status,
                        "owner": proposal.owner,
                    });
                }
                record
            }
            Self::Run { id, task } => json!({"kind": "run", "id": id, "task": task}),
            Self::Ask { id, run } => json!({"kind": "ask", "id": id, "run": run}),
            Self::NewAsk {
                kind,
                run,
                task,
                request,
                planner,
            } => {
                let mut record =
                    json!({"kind": "new_ask", "ask_kind": kind, "run": run, "task": task});
                // An ask on a request names it and its planner.
                if request.is_some() {
                    record["request"] = json!(request);
                    record["planner"] = json!(planner);
                }
                record
            }
            Self::Proposal { id, owner } => json!({"kind": "proposal", "id": id, "owner": owner}),
            Self::Finding(id) => json!({"kind": "finding", "id": id}),
            Self::Planner(id) => json!({"kind": "planner", "id": id}),
            Self::Request { id, planner } => {
                json!({"kind": "request", "id": id, "planner": planner})
            }
            Self::AgentEval { run } => json!({"kind": "agent_eval", "run": run}),
            Self::Telemetry { target, owner } => json!({
                "kind": "telemetry", "target": target.kind.as_str(), "id": target.id,
                "owner": owner.as_ref().map(|owner| format!("{}:{}", owner.kind.as_str(), owner.id)),
            }),
            Self::Principal(role) => json!({"kind": "principal", "role": role.as_str()}),
            Self::Unresolved => json!({"kind": "unresolved"}),
        }
    }
}

/// Why a request was refused. It names no id or text of the resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// The role's allowlist lacks the capability.
    NotGranted,
    /// The capability is reserved for a later stage.
    Reserved,
    /// The resource is not the actor's, or its owner or state is unknown.
    Resource,
    /// The role does not open asks of this kind.
    AskKind,
    /// The change would change a priority a person set, which only a
    /// person changes ([`check_priority_change`]).
    PersonsPriority,
    /// A planner moves a `goal_gap` draft out of its goal
    /// ([`super::follow_up::check_goal_gap_move`]).
    GoalGapDraft,
    /// What a token is issued under is not held: the run's lease, the
    /// supervisor's registration, or the planner open
    /// ([`super::queue_service::IssueHeld`]).
    NotHeld,
}

impl DenyReason {
    /// The reason as the error and the record name it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotGranted => "not granted",
            Self::Reserved => "reserved",
            Self::Resource => "not on this resource",
            Self::AskKind => "not of this kind",
            Self::PersonsPriority => "a person's priority",
            Self::GoalGapDraft => "a goal review's goal_gap draft",
            Self::NotHeld => "what it is issued under is not held",
        }
    }
}

/// A refused request: the role and the capability, nothing else of the
/// actor (its id may carry a session id) or of the resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationError {
    pub role: ActorRole,
    pub capability: Capability,
    pub reason: DenyReason,
}

impl fmt::Display for AuthorizationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} may not {} ({})",
            self.role.as_str(),
            self.capability,
            self.reason.as_str()
        )
    }
}

impl std::error::Error for AuthorizationError {}

/// Decides whether `actor` may use `capability` on `resource`.
pub trait Authorizer {
    fn authorize(
        &self,
        actor: &ActorContext,
        capability: Capability,
        resource: &Resource,
    ) -> Result<(), AuthorizationError>;
}

/// The static policy of ADR-t728-1: [`grants`] per role, then the
/// resource rules of the worker, the planner, the observer and the jobs
/// that submit on a run.
#[derive(Debug, Clone, Copy, Default)]
pub struct StaticPolicy;

impl Authorizer for StaticPolicy {
    fn authorize(
        &self,
        actor: &ActorContext,
        capability: Capability,
        resource: &Resource,
    ) -> Result<(), AuthorizationError> {
        let deny = |reason| AuthorizationError {
            role: actor.role(),
            capability,
            reason,
        };
        if capability.is_reserved() {
            return Err(deny(DenyReason::Reserved));
        }
        if !grants(actor.role()).contains(&capability) {
            return Err(deny(DenyReason::NotGranted));
        }
        if let Resource::NewAsk { kind, .. } = resource
            && !opens_ask(actor.role(), kind)
        {
            return Err(deny(DenyReason::AskKind));
        }
        if resource_permits(actor, capability, resource) {
            Ok(())
        } else {
            Err(deny(DenyReason::Resource))
        }
    }
}

use Capability as C;

/// What the inbox may do at a person's word: everything but the jobs'
/// submissions, the integrator's own landing and push, the park and the
/// replacement of a replanning, which only the runtime does, and the
/// approval of `setup-review`'s changes beforehand, which is the person's
/// own ([`USER`]). It alone (with the person) holds and releases a task and
/// generates the review's settings (`setup-review`).
const INBOX: &[Capability] = &[
    C::QueueRead,
    C::QueueWatch,
    C::ExportFile,
    C::CiRead,
    C::GoalWrite,
    C::GoalReady,
    C::GoalClose,
    C::GoalReviewRequest,
    C::TaskWrite,
    C::FollowUpJudge,
    C::TaskVerifyEdit,
    C::TaskCancel,
    C::TaskReady,
    C::TaskReadyBypassReview,
    C::ProposalSubmit,
    C::ProposalWithdraw,
    C::NoteWrite,
    C::MarkWrite,
    C::AskOpen,
    C::SessionRun,
    C::SessionRecord,
    C::PrepareReview,
    C::FindingRecord,
    C::FindingResolve,
    C::FindingDismiss,
    C::FindingAsk,
    C::ObserveRun,
    C::AskAnswer,
    C::AskClose,
    C::PlannerOpen,
    C::RequestRecord,
    C::ReplanRequest,
    C::TaskHold,
    C::ScreenRead,
    C::ScreenSend,
    C::PlannerRequest,
    C::EvalRequest,
    C::EvalRequestHeldOut,
    C::EvalRerun,
    C::EvalRead,
    C::EvalCases,
    C::ReviewSetup,
    C::Supervise,
    C::RunRecover,
    C::WorkspaceCleanup,
    C::ServiceLifecycle,
    C::BinaryInstall,
    C::QueueAdmin,
    C::IntegrationRequest,
    C::TokenIssue,
];

/// The number of [`USER`]'s capabilities: the inbox's and two more.
const USER_LEN: usize = INBOX.len() + 2;

/// [`INBOX`]'s capabilities, the approval of `setup-review`'s changes
/// beforehand ([`C::ReviewSetupApprove`]) and the queue broker's records
/// ([`C::QueueBrokerRecord`]).
const USER_CAPABILITIES: [Capability; USER_LEN] = {
    let mut all = [C::ReviewSetupApprove; USER_LEN];
    let mut at = 0;
    while at < INBOX.len() {
        all[at] = INBOX[at];
        at += 1;
    }
    all[at + 1] = C::QueueBrokerRecord;
    all
};

/// What a person at a plain terminal may do: the inbox's, approving
/// `setup-review`'s changes beforehand (`--yes`), which only a person does,
/// and the queue broker's records, which the broker sends as the user
/// (ADR-t3102-1 decision 1) and no AI actor does.
const USER: &[Capability] = &USER_CAPABILITIES;

/// The planner's authority as it is (ADR-t728-1 decision 7): goals, tasks
/// before they start, its proposals, notes, marks, resolving and dismissing
/// findings, its
/// questions (its session is its wrapper's to run and record, ADR-t3102-1
/// decision 4), the eval's rounds of any split (never `--rerun`, ADR-t1728-1
/// decision 7), the production sets and their labels and, at a person's word, `up` / `down` / `install`. No run,
/// no landing, no `ready`, no answer.
const PLANNER: &[Capability] = &[
    C::QueueRead,
    C::QueueWatch,
    C::ExportFile,
    C::CiRead,
    C::GoalWrite,
    C::GoalClose,
    C::TaskWrite,
    C::FollowUpJudge,
    C::TaskCancel,
    C::ProposalSubmit,
    C::ProposalWithdraw,
    C::NoteWrite,
    C::MarkWrite,
    C::AskOpen,
    C::FindingResolve,
    C::FindingDismiss,
    C::PlannerOpen,
    C::RequestDecline,
    C::EvalRequest,
    C::EvalRequestHeldOut,
    C::EvalRead,
    C::EvalCases,
    C::ServiceLifecycle,
    C::BinaryInstall,
    C::QueueAdmin,
];

/// A worker reads, asks and notes on its own run; its session is its
/// wrapper's to run and record, under the wrapper's principal
/// (ADR-t3102-1 decision 4). Of the eval it asks for a dev round and reads
/// the rounds of its run only (ADR-t1728-1 decision 6): it never runs an
/// LLM's CLI itself. It proposes to replan its own run, and nothing more
/// of a replanning (ADR-t1521-2 decision 3).
const WORKER: &[Capability] = &[
    C::QueueRead,
    C::AskOpen,
    C::NoteWrite,
    C::ReplanPropose,
    C::EvalRequest,
    C::EvalRead,
];

/// A review job reads; its verdict comes back as data on its own run.
const REVIEW_JOB: &[Capability] = &[C::QueueRead, C::ReviewSubmit];
/// A recovery job reads; its verdict comes back as data on its own run.
const RECOVERY_JOB: &[Capability] = &[C::QueueRead, C::TriageSubmit];
/// Plan review, goal review and the throughput review read; the supervisor
/// applies their verdicts and saves the review. The job of `setup-review`
/// reads too: it writes files in its temporary place only, and changes no
/// state of the queue.
const READ_ONLY: &[Capability] = &[C::QueueRead];

/// The observer (ADR-0044 decision 4): reads, records and resolves
/// findings, and asks `blocked` on a finding.
const OBSERVER: &[Capability] = &[
    C::QueueRead,
    C::QueueWatch,
    C::CiRead,
    C::FindingRecord,
    C::FindingResolve,
    C::FindingAsk,
    C::EvalRead,
];

/// The supervisor's transitions. It asks the integrator to land
/// (ADR-t728-2) and applies answers without writing them. Its automatic
/// update checks, migrates and probes queues with the new binary as
/// `install` does (task 734). It runs the eval's rounds that were asked
/// for (`scheduler.supervise`) and reads them, and asks for none itself
/// (ADR-t1728-1 decision 6). It alone parks a run to replan it and
/// applies the replacement (ADR-t1521-2 decisions 1 and 5). It may record
/// the verifications' tokens it swept through the service as the broker
/// does (`queue_broker.record`).
const SUPERVISOR: &[Capability] = &[
    C::QueueRead,
    C::QueueWatch,
    C::ExportFile,
    C::CiRead,
    C::GoalClose,
    C::TaskCancel,
    C::TaskReady,
    C::NoteWrite,
    C::AskOpen,
    C::AskClose,
    C::SessionRun,
    C::PrepareReview,
    C::FindingRecord,
    C::FindingResolve,
    C::FindingDismiss,
    C::ObserveRun,
    C::PlannerOpen,
    C::EvalRead,
    C::Supervise,
    C::RunRecover,
    C::ServiceLifecycle,
    C::BinaryInstall,
    C::QueueAdmin,
    C::IntegrationRequest,
    C::ReplanPark,
    C::ReplanApply,
    C::TokenIssue,
    C::QueueBrokerRecord,
];

/// The session wrapper and its hook: its own run's or planner's session
/// and its record (ADR-t3102-1 decision 4).
const WRAPPER: &[Capability] = &[C::QueueRead, C::SessionRun, C::SessionRecord];

/// The integrator lands and pushes (ADR-t728-2).
const INTEGRATOR: &[Capability] = &[C::QueueRead, C::Land, C::Push];

/// An executor's telemetry principal sends the steps of its own target and
/// nothing else, not even a read (ADR-t1662-3 decision 9).
const EXECUTOR_TELEMETRY: &[Capability] = &[C::TelemetryReport];

/// The allowlist of `role`. The inbox has the user's (ADR-t728-3 decision
/// 1: it acts at a person's word) but the approval of `setup-review`'s
/// changes beforehand; the record tells the two apart.
pub const fn grants(role: ActorRole) -> &'static [Capability] {
    match role {
        ActorRole::User => USER,
        ActorRole::Inbox => INBOX,
        ActorRole::Planner => PLANNER,
        ActorRole::Worker => WORKER,
        ActorRole::ReviewJob => REVIEW_JOB,
        ActorRole::RecoveryJob => RECOVERY_JOB,
        ActorRole::PlanReviewJob
        | ActorRole::GoalReviewJob
        | ActorRole::ThroughputReviewJob
        | ActorRole::SetupReviewJob => READ_ONLY,
        ActorRole::Observer => OBSERVER,
        ActorRole::Supervisor => SUPERVISOR,
        ActorRole::Wrapper => WRAPPER,
        ActorRole::Integrator => INTEGRATOR,
        ActorRole::ExecutorTelemetry => EXECUTOR_TELEMETRY,
    }
}

/// The kinds of ask a role opens (with [`C::AskOpen`]): a worker its
/// `worker_question`, a planner its `planner_question`, and the user, the
/// inbox and the supervisor any the command line opens. The observer's
/// `blocked` ask on a finding is [`C::FindingAsk`], not an ask it opens.
pub fn opens_ask(role: ActorRole, kind: &AskKind) -> bool {
    match role {
        ActorRole::User | ActorRole::Inbox | ActorRole::Supervisor => true,
        ActorRole::Worker => *kind == AskKind::WorkerQuestion,
        ActorRole::Planner => *kind == AskKind::PlannerQuestion,
        _ => false,
    }
}

/// The resource rules on top of the allowlist.
fn resource_permits(actor: &ActorContext, capability: Capability, resource: &Resource) -> bool {
    if capability == C::TokenIssue {
        return matches!(resource, Resource::Principal(child) if starts(actor.role(), *child));
    }
    match actor.role() {
        ActorRole::Worker => capability == C::QueueRead || worker_owns(actor, resource),
        ActorRole::Planner => planner_permits(actor, capability, resource),
        ActorRole::ReviewJob | ActorRole::RecoveryJob => {
            capability == C::QueueRead
                || matches!(resource, Resource::Run { id, .. } if actor.run_id() == Some(id))
        }
        ActorRole::Observer => match capability {
            C::FindingAsk | C::FindingResolve => matches!(resource, Resource::Finding(_)),
            _ => true,
        },
        ActorRole::ExecutorTelemetry => telemetry_owns(actor, resource),
        ActorRole::Wrapper => capability == C::QueueRead || wrapper_owns(actor, resource),
        _ => true,
    }
}

/// A session wrapper's own run or planner: the one its actor id names
/// ([`super::queue_service::wrapper_actor_id`], which the service takes
/// from the token's principal, never from the caller).
fn wrapper_owns(actor: &ActorContext, resource: &Resource) -> bool {
    use super::session_step::{StepTarget, TargetKind};
    let target = match resource {
        Resource::Run { id, .. } => StepTarget::run(id),
        Resource::Planner(id) => StepTarget::of(TargetKind::Planner, id.as_i64()),
        _ => return false,
    };
    actor.actor_id() == super::queue_service::wrapper_actor_id(&target)
}

/// Whether a principal of role `parent` may ask the queue service for the
/// token of a principal of role `child` it starts (`issue_token`,
/// ADR-t3102-1 decision 2, default deny): the user starts the supervisor
/// and the inbox; the supervisor its runs' workers and session wrappers,
/// the planners and their wrappers, the executors' telemetry and its jobs;
/// the user and the inbox the job a command of theirs starts (`observe`,
/// `setup-review`), only one whose capabilities are all the parent's, as
/// the answer hands the parent the token's file (ADR-t728-1 decision 5),
/// and that job's executor's telemetry, whose one capability reaches only
/// its target, the job's (which
/// [`super::queue_service::telemetry_issue_refusal`] checks). No
/// parent asks for the integrator's here: it is issued inside the
/// supervisor's operation that takes the landing slot (`begin_landing`),
/// under the run's lease. No principal is started by itself.
pub fn starts(parent: ActorRole, child: ActorRole) -> bool {
    use ActorRole as R;
    let within = || {
        grants(child)
            .iter()
            .all(|capability| grants(parent).contains(capability))
    };
    match child {
        R::Supervisor | R::Inbox => parent == R::User,
        R::Worker | R::Wrapper | R::Planner | R::ReviewJob | R::RecoveryJob => {
            parent == R::Supervisor
        }
        R::ExecutorTelemetry => matches!(parent, R::Supervisor | R::User | R::Inbox),
        R::Observer
        | R::PlanReviewJob
        | R::GoalReviewJob
        | R::ThroughputReviewJob
        | R::SetupReviewJob => match parent {
            R::Supervisor => true,
            R::User | R::Inbox => within(),
            _ => false,
        },
        R::Integrator | R::User => false,
    }
}

/// The telemetry principal's own target: the one its actor id names
/// ([`super::telemetry::actor_id`], which the service takes from the
/// token's principal, never from the caller), of a session the control
/// side registered to that target.
fn telemetry_owns(actor: &ActorContext, resource: &Resource) -> bool {
    matches!(resource, Resource::Telemetry { target, owner }
        if actor.actor_id() == super::telemetry::actor_id(target)
            && owner.as_ref() == Some(target))
}

/// A worker's run, its task, or an ask on its run.
fn worker_owns(actor: &ActorContext, resource: &Resource) -> bool {
    let Some(own) = actor.run_id() else {
        return false;
    };
    match resource {
        Resource::Run { id, .. }
        | Resource::Ask { run: Some(id), .. }
        | Resource::NewAsk { run: Some(id), .. }
        | Resource::AgentEval { run: Some(id) } => id == own,
        Resource::Task { id, .. }
        | Resource::NewAsk {
            run: None,
            task: Some(id),
            ..
        } => actor.task_id() == Some(*id),
        _ => false,
    }
}

fn planner_permits(actor: &ActorContext, capability: Capability, resource: &Resource) -> bool {
    match resource {
        // A note on a run, as a person's free text (ADR-t728-1 decision 7).
        Resource::Run { .. } if capability == C::NoteWrite => true,
        // Nothing else on a run, and no ask on one.
        Resource::Run { .. }
        | Resource::Ask { run: Some(_), .. }
        | Resource::NewAsk { run: Some(_), .. }
        | Resource::Unresolved => false,
        // Changes to tasks before they start (draft, submitted, ready), but
        // not of another planner's revising proposal; a note on any task.
        Resource::Task {
            status, proposal, ..
        } if matches!(capability, C::TaskWrite | C::TaskCancel) => {
            matches!(
                status,
                Some(TaskStatus::Draft | TaskStatus::Submitted | TaskStatus::Ready)
            ) && proposal.as_ref().is_none_or(|p| p.open_to(actor))
        }
        Resource::Proposal { owner, .. } if capability == C::ProposalWithdraw => {
            owner.as_deref() == Some(actor.actor_id())
        }
        Resource::Planner(id) => actor.actor_id() == format!("planner:{id}"),
        // Only the request's own planner declines it, or asks on it.
        Resource::Request { planner, .. }
        | Resource::NewAsk {
            request: Some(_),
            planner,
            ..
        } => planner.is_some_and(|id| actor.actor_id() == format!("planner:{id}")),
        _ => true,
    }
}

/// The capabilities a request of an eval needs (ADR-t1728-1 decisions 6
/// and 7): a dev round [`C::EvalRequest`], a hold-out or production round
/// [`C::EvalRequestHeldOut`], and `--rerun` of a held-out set
/// [`C::EvalRerun`] as well.
pub fn eval_request_needs(held_out: bool, rerun: bool) -> Vec<Capability> {
    let mut needs = vec![if held_out {
        C::EvalRequestHeldOut
    } else {
        C::EvalRequest
    }];
    if rerun {
        needs.push(C::EvalRerun);
    }
    needs
}

/// What holds a priority: a task (its own, or the goal's it takes) or a
/// goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorityHolder {
    Task(TaskId),
    Goal(GoalId),
}

impl fmt::Display for PriorityHolder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Task(id) => write!(f, "task {id}"),
            Self::Goal(id) => write!(f, "goal {id}"),
        }
    }
}

/// Whether `role` changes a priority a person set (ADR-t1975-1 decision
/// 3): the user, and the inbox at a person's word. The AI's actors (a
/// planner, the jobs, the supervisor applying a verdict or fixing on its
/// own) do not.
pub const fn changes_persons_priority(role: ActorRole) -> bool {
    matches!(role, ActorRole::User | ActorRole::Inbox)
}

/// Refuse `role` a change of the priority `holder` has now, `priority`
/// set by `by` (for a task, who set the priority it takes: its own's
/// setter, else its goal's), when a person set it and `role` is not a
/// person's ([`changes_persons_priority`]). The caller asks only for a
/// change that changes something; the error names the person's value.
pub fn check_priority_change(
    role: ActorRole,
    holder: PriorityHolder,
    priority: Priority,
    by: PriorityBy,
) -> Result<(), DomainError> {
    require(
        by != PriorityBy::Human || changes_persons_priority(role),
        || DomainError::PersonsPriority {
            holder,
            priority,
            role,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_user_and_inbox_can_correct_an_ended_tasks_verify() {
        let task = task_in(TaskStatus::InProgress);
        for role in [ActorRole::User, ActorRole::Inbox] {
            assert!(allowed(
                &ActorContext::instance(role, 1),
                C::TaskVerifyEdit,
                &task
            ));
        }
        for role in [
            ActorRole::Planner,
            ActorRole::Worker,
            ActorRole::RecoveryJob,
            ActorRole::ReviewJob,
            ActorRole::Supervisor,
        ] {
            assert!(!allowed(
                &ActorContext::instance(role, 1),
                C::TaskVerifyEdit,
                &task
            ));
        }
    }

    fn allowed(actor: &ActorContext, capability: Capability, resource: &Resource) -> bool {
        StaticPolicy.authorize(actor, capability, resource).is_ok()
    }

    fn run(id: &str) -> RunId {
        RunId::new(id).unwrap()
    }

    fn task_in(status: TaskStatus) -> Resource {
        TaskAuthorization::outside_proposals(status).resource(TaskId::new(1))
    }

    fn role(role: ActorRole) -> ActorContext {
        ActorContext::instance(role, 1)
    }

    const JOBS: [ActorRole; 6] = [
        ActorRole::ReviewJob,
        ActorRole::RecoveryJob,
        ActorRole::PlanReviewJob,
        ActorRole::GoalReviewJob,
        ActorRole::ThroughputReviewJob,
        ActorRole::SetupReviewJob,
    ];

    #[test]
    fn a_person_requests_a_worker_proposes_on_its_run_and_only_the_runtime_parks_and_applies() {
        let on = |id: &str| Resource::Run {
            id: run(id),
            task: Some(TaskId::new(1)),
        };
        for role_ in [ActorRole::User, ActorRole::Inbox] {
            assert!(allowed(&role(role_), C::ReplanRequest, &on("r1")));
            assert!(!allowed(&role(role_), C::ReplanPark, &on("r1")));
            assert!(!allowed(&role(role_), C::ReplanApply, &on("r1")));
        }
        let worker = ActorContext::worker(&run("r1"), TaskId::new(1));
        assert!(allowed(&worker, C::ReplanPropose, &on("r1")));
        assert!(!allowed(&worker, C::ReplanPropose, &on("r2")));
        assert!(!allowed(&worker, C::ReplanRequest, &on("r1")));
        assert!(!allowed(&worker, C::ReplanPark, &on("r1")));
        let supervisor = role(ActorRole::Supervisor);
        assert!(allowed(&supervisor, C::ReplanPark, &on("r1")));
        assert!(allowed(&supervisor, C::ReplanApply, &on("r1")));
        assert!(!allowed(&supervisor, C::ReplanRequest, &on("r1")));
        for role_ in ActorRole::ALL.into_iter().filter(|role_| {
            !matches!(
                role_,
                ActorRole::User | ActorRole::Inbox | ActorRole::Worker | ActorRole::Supervisor
            )
        }) {
            for capability in [
                C::ReplanRequest,
                C::ReplanPropose,
                C::ReplanPark,
                C::ReplanApply,
            ] {
                assert!(!allowed(&role(role_), capability, &on("r1")), "{role_:?}");
            }
        }
    }

    #[test]
    fn only_a_person_and_the_inbox_hold_and_release_a_task() {
        let task = Resource::task(TaskId::new(1));
        for role_ in ActorRole::ALL {
            assert_eq!(
                allowed(&role(role_), C::TaskHold, &task),
                matches!(role_, ActorRole::User | ActorRole::Inbox),
                "{role_:?}"
            );
        }
        let worker = ActorContext::worker(&run("r1"), TaskId::new(1));
        assert!(!allowed(&worker, C::TaskHold, &task));
    }

    /// `setup-review` is a person's and the inbox's alone, and its job
    /// reads only: every capability that changes the queue is refused it.
    #[test]
    fn only_a_person_and_the_inbox_set_up_the_review_and_its_job_only_reads() {
        for role_ in ActorRole::ALL {
            assert_eq!(
                allowed(&role(role_), C::ReviewSetup, &Resource::Queue),
                matches!(role_, ActorRole::User | ActorRole::Inbox),
                "{role_:?}"
            );
        }
        let worker = ActorContext::worker(&run("r1"), TaskId::new(1));
        assert!(!allowed(&worker, C::ReviewSetup, &Resource::Queue));
        // Approving beforehand is a person's own, not even the inbox's.
        for role_ in ActorRole::ALL {
            assert_eq!(
                allowed(&role(role_), C::ReviewSetupApprove, &Resource::Queue),
                role_ == ActorRole::User,
                "{role_:?}"
            );
        }
        let mut inbox = grants(ActorRole::Inbox).to_vec();
        inbox.push(C::ReviewSetupApprove);
        inbox.push(C::QueueBrokerRecord);
        assert_eq!(inbox, grants(ActorRole::User));
        assert_eq!(grants(ActorRole::SetupReviewJob), [C::QueueRead]);
        let job = ActorContext::setup_review_job("s1");
        for capability in Capability::ALL
            .into_iter()
            .filter(|capability| *capability != C::QueueRead)
        {
            for resource in [
                Resource::Queue,
                task_in(TaskStatus::Draft),
                Resource::run(run("r1")),
                Resource::Goal(GoalId::new(1)),
            ] {
                assert!(!allowed(&job, capability, &resource), "{capability:?}");
            }
        }
    }

    /// ADR-t3102-1 decision 2's table: only the user, the inbox and the
    /// supervisor ask for a token (`token.issue`), each for the children it
    /// starts; a command's job only within the asker's capabilities, and
    /// its executor's telemetry; the integrator's never (the landing's use
    /// case issues it); every other asked for is refused on its resource.
    #[test]
    fn a_token_is_asked_for_only_by_the_parent_of_its_role() {
        use ActorRole as R;
        let parents = |child: R| -> Vec<&str> {
            let mut parents: Vec<&str> = R::ALL
                .into_iter()
                .filter(|parent| {
                    allowed(&role(*parent), C::TokenIssue, &Resource::Principal(child))
                })
                .map(R::as_str)
                .collect();
            parents.sort_unstable();
            parents
        };
        assert_eq!(parents(R::Supervisor), [R::User.as_str()]);
        assert_eq!(parents(R::Inbox), [R::User.as_str()]);
        // A review's or a triage's verdict is not the user's or the inbox's
        // to submit, so neither is handed its job's token.
        for child in [
            R::Worker,
            R::Wrapper,
            R::Planner,
            R::ReviewJob,
            R::RecoveryJob,
        ] {
            assert_eq!(parents(child), [R::Supervisor.as_str()], "{child:?}");
        }
        let mut anyone = [R::User.as_str(), R::Inbox.as_str(), R::Supervisor.as_str()];
        anyone.sort_unstable();
        for child in [
            R::Observer,
            R::PlanReviewJob,
            R::SetupReviewJob,
            R::ExecutorTelemetry,
        ] {
            assert_eq!(parents(child), anyone, "{child:?}");
        }
        assert!(parents(R::Integrator).is_empty());
        assert!(parents(R::User).is_empty());
        let refused = StaticPolicy
            .authorize(
                &role(R::Supervisor),
                C::TokenIssue,
                &Resource::Principal(R::Inbox),
            )
            .unwrap_err();
        assert_eq!(refused.reason, DenyReason::Resource);
        let refused = StaticPolicy
            .authorize(
                &role(R::Worker),
                C::TokenIssue,
                &Resource::Principal(R::Wrapper),
            )
            .unwrap_err();
        assert_eq!(refused.reason, DenyReason::NotGranted);
        // Not on a resource that names no principal.
        assert!(!allowed(&role(R::User), C::TokenIssue, &Resource::Queue));
        assert_eq!(
            Resource::Principal(R::Worker).record(),
            serde_json::json!({"kind": "principal", "role": "worker"})
        );
    }

    #[test]
    fn capabilities_are_listed_once_and_named_uniquely() {
        for capability in Capability::ALL {
            assert_eq!(
                capability.as_str().parse::<Capability>().unwrap(),
                capability
            );
        }
        let mut names: Vec<_> = Capability::ALL.iter().map(|c| c.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Capability::ALL.len());
    }

    #[test]
    fn reserved_capabilities_are_refused_to_every_role() {
        for role_ in ActorRole::ALL {
            for capability in Capability::ALL.into_iter().filter(|c| c.is_reserved()) {
                assert!(!grants(role_).contains(&capability), "{role_:?}");
                let error = StaticPolicy
                    .authorize(&role(role_), capability, &Resource::Queue)
                    .unwrap_err();
                assert_eq!(error.reason, DenyReason::Reserved);
            }
        }
    }

    /// Every role but the executor's telemetry principal reads: it sends
    /// the steps of its target only (ADR-t1662-3 decision 9).
    #[test]
    fn every_role_reads_and_is_refused_what_it_was_not_granted() {
        for role_ in ActorRole::ALL {
            let actor = role(role_);
            assert_eq!(
                allowed(&actor, C::QueueRead, &Resource::Queue),
                role_ != ActorRole::ExecutorTelemetry,
                "{role_:?}"
            );
            for capability in Capability::ALL {
                if !grants(role_).contains(&capability) {
                    let error = StaticPolicy
                        .authorize(&actor, capability, &Resource::Queue)
                        .unwrap_err();
                    assert_eq!((error.role, error.capability), (role_, capability));
                }
            }
        }
    }

    #[test]
    fn the_error_names_the_role_and_the_capability_and_no_ids() {
        let actor = ActorContext::instance(ActorRole::ReviewJob, "secret-session-id");
        let error = StaticPolicy
            .authorize(
                &actor,
                C::IntegrationRequest,
                &Resource::task(TaskId::new(42)),
            )
            .unwrap_err();
        let text = error.to_string();
        assert_eq!(text, "review-job may not landing.request (not granted)");
        assert!(!text.contains("secret") && !text.contains("42"));
    }

    #[test]
    fn the_user_and_the_inbox_do_everything_but_the_jobs_and_the_integrator() {
        for role_ in [ActorRole::User, ActorRole::Inbox] {
            let actor = role(role_);
            for capability in [
                C::AskAnswer,
                C::IntegrationRequest,
                C::RunRecover,
                C::WorkspaceCleanup,
                C::TaskReadyBypassReview,
                C::TaskCancel,
                C::GoalClose,
                C::ServiceLifecycle,
                C::BinaryInstall,
            ] {
                assert!(
                    allowed(&actor, capability, &task_in(TaskStatus::InProgress)),
                    "{role_:?} {capability:?}"
                );
            }
            for capability in [C::Land, C::Push, C::ReviewSubmit, C::TriageSubmit] {
                assert!(!allowed(&actor, capability, &Resource::Queue));
            }
        }
    }

    #[test]
    fn a_worker_acts_on_its_own_run_only() {
        let own = run("r1");
        let worker = ActorContext::worker(&own, TaskId::new(1));
        assert!(allowed(&worker, C::AskOpen, &Resource::run(own.clone())));
        // Its session is its wrapper's to run and record (ADR-t3102-1
        // decision 4), on its own run too.
        for capability in [C::SessionRun, C::SessionRecord] {
            let error = StaticPolicy
                .authorize(&worker, capability, &Resource::run(own.clone()))
                .unwrap_err();
            assert_eq!(error.reason, DenyReason::NotGranted, "{capability:?}");
        }
        assert!(allowed(
            &worker,
            C::NoteWrite,
            &Resource::task(TaskId::new(1))
        ));
        let ask_on_own = Resource::Ask {
            id: AskId::new(1),
            run: Some(own.clone()),
        };
        assert!(allowed(&worker, C::AskOpen, &ask_on_own));

        let other = Resource::run(run("r2"));
        for capability in [C::AskOpen, C::NoteWrite] {
            let error = StaticPolicy
                .authorize(&worker, capability, &other)
                .unwrap_err();
            assert_eq!(error.reason, DenyReason::Resource);
        }
        assert!(!allowed(
            &worker,
            C::NoteWrite,
            &Resource::task(TaskId::new(2))
        ));
        assert!(!allowed(&worker, C::AskOpen, &Resource::Queue));
        assert!(!allowed(&worker, C::AskOpen, &Resource::Unresolved));
        // A worker without its run in its environment owns nothing.
        let anonymous = role(ActorRole::Worker);
        assert!(!allowed(
            &anonymous,
            C::AskOpen,
            &Resource::run(own.clone())
        ));
        for capability in [
            C::IntegrationRequest,
            C::AskAnswer,
            C::TaskReady,
            C::TaskCancel,
            C::RunRecover,
            C::Land,
        ] {
            assert!(!allowed(&worker, capability, &Resource::run(own.clone())));
        }
    }

    #[test]
    fn a_planner_keeps_its_authority_on_tasks_before_they_start() {
        let planner = ActorContext::instance(ActorRole::Planner, 7);
        for status in [TaskStatus::Draft, TaskStatus::Submitted, TaskStatus::Ready] {
            for capability in [C::TaskCancel, C::TaskWrite] {
                assert!(
                    allowed(&planner, capability, &task_in(status)),
                    "{status:?}"
                );
            }
        }
        for status in [
            TaskStatus::InProgress,
            TaskStatus::Completed,
            TaskStatus::Canceled,
        ] {
            assert!(!allowed(&planner, C::TaskCancel, &task_in(status)));
        }
        // An unknown status is refused.
        assert!(!allowed(
            &planner,
            C::TaskCancel,
            &Resource::task(TaskId::new(1))
        ));
        let goal = Resource::Goal(GoalId::new(3));
        for capability in [C::GoalWrite, C::GoalClose, C::TaskWrite, C::NoteWrite] {
            assert!(allowed(&planner, capability, &goal), "{capability:?}");
        }
        for capability in [
            C::ProposalSubmit,
            C::MarkWrite,
            C::ServiceLifecycle,
            C::AskOpen,
        ] {
            assert!(allowed(&planner, capability, &Resource::Queue));
        }
        for capability in [C::FindingDismiss, C::FindingResolve] {
            assert!(allowed(
                &planner,
                capability,
                &Resource::Finding(FindingId::new(1))
            ));
        }
        // Its session is its wrapper's (ADR-t3102-1 decision 4).
        for capability in [C::SessionRun, C::SessionRecord] {
            assert!(!allowed(
                &planner,
                capability,
                &Resource::Planner(PlannerId::new(7))
            ));
        }
    }

    #[test]
    fn a_planner_may_not_touch_runs_land_ready_or_answer() {
        let planner = ActorContext::instance(ActorRole::Planner, 7);
        let own_run = Resource::run(run("r1"));
        for capability in [C::AskOpen, C::SessionRun] {
            assert!(!allowed(&planner, capability, &own_run), "{capability:?}");
        }
        // A note it keeps (ADR-t728-1 decision 7).
        assert!(allowed(&planner, C::NoteWrite, &own_run));
        for capability in [
            C::RunRecover,
            C::IntegrationRequest,
            C::Land,
            C::Push,
            C::TaskReady,
            C::TaskReadyBypassReview,
            C::GoalReady,
            C::AskAnswer,
            C::AskClose,
            C::Supervise,
            C::WorkspaceCleanup,
        ] {
            let error = StaticPolicy
                .authorize(&planner, capability, &task_in(TaskStatus::Draft))
                .unwrap_err();
            assert_eq!(error.reason, DenyReason::NotGranted, "{capability:?}");
        }
    }

    #[test]
    fn a_planner_withdraws_only_its_own_proposal() {
        let planner = ActorContext::instance(ActorRole::Planner, 7);
        let proposal = |owner: Option<&str>| Resource::Proposal {
            id: ProposalId::new(1),
            owner: owner.map(str::to_owned),
        };
        assert!(allowed(
            &planner,
            C::ProposalWithdraw,
            &proposal(Some("planner:7"))
        ));
        assert!(!allowed(
            &planner,
            C::ProposalWithdraw,
            &proposal(Some("planner:8"))
        ));
        assert!(!allowed(&planner, C::ProposalWithdraw, &proposal(None)));
        // The user and the inbox withdraw any.
        for role_ in [ActorRole::User, ActorRole::Inbox] {
            assert!(allowed(&role(role_), C::ProposalWithdraw, &proposal(None)));
        }
    }

    /// A planner changes no task of a revising proposal but its own, and
    /// one whose owner is unrecorded or names a role but no one actor is
    /// no planner's; a proposal in any other status, and every proposal for
    /// the user, the inbox and the supervisor's cancel, decide nothing.
    #[test]
    fn a_planner_changes_no_task_of_another_planners_revising_proposal() {
        let me = ActorContext::instance(ActorRole::Planner, 7);
        let legacy = ActorContext::new(ActorRole::Planner, "planner");
        let task = |status: ProposalStatus, owner: Option<&str>| {
            TaskAuthorization {
                status: TaskStatus::Draft,
                proposal: Some(TaskProposal {
                    id: ProposalId::new(1),
                    status,
                    owner: owner.map(str::to_owned),
                }),
            }
            .resource(TaskId::new(1))
        };
        for capability in [C::TaskWrite, C::TaskCancel] {
            let revising = |owner| task(ProposalStatus::Revising, owner);
            assert!(allowed(&me, capability, &revising(Some("planner:7"))));
            for owner in [Some("planner:8"), Some("planner"), None] {
                let error = StaticPolicy
                    .authorize(&me, capability, &revising(owner))
                    .unwrap_err();
                assert_eq!(error.reason, DenyReason::Resource, "{owner:?}");
            }
            assert!(!allowed(&legacy, capability, &revising(Some("planner"))));
            for status in [
                ProposalStatus::Submitted,
                ProposalStatus::Accepted,
                ProposalStatus::Canceled,
            ] {
                for owner in [Some("planner:8"), Some("planner"), None] {
                    assert!(
                        allowed(&me, capability, &task(status, owner)),
                        "{status:?} {owner:?}"
                    );
                }
            }
            for role_ in [ActorRole::User, ActorRole::Inbox] {
                for owner in [Some("planner:8"), None] {
                    assert!(allowed(&role(role_), capability, &revising(owner)));
                }
            }
        }
        assert!(allowed(
            &role(ActorRole::Supervisor),
            C::TaskCancel,
            &task(ProposalStatus::Revising, None)
        ));
        assert_eq!(
            task(ProposalStatus::Revising, Some("planner:8")).record(),
            serde_json::json!({"kind": "task", "id": 1, "status": "draft",
                "proposal": {"id": 1, "status": "revising", "owner": "planner:8"}})
        );
    }

    /// The store refuses a change whose task's status, proposal, proposal
    /// status or owner differs from what it was authorized with.
    #[test]
    fn a_change_goes_on_only_with_the_task_and_proposal_it_was_authorized_with() {
        let id = TaskId::new(5);
        let in_proposal = |status: ProposalStatus, owner: &str| TaskAuthorization {
            status: TaskStatus::Draft,
            proposal: Some(TaskProposal {
                id: ProposalId::new(1),
                status,
                owner: Some(owner.into()),
            }),
        };
        let authorized = in_proposal(ProposalStatus::Revising, "planner:1");
        assert_eq!(authorized.check_unchanged(id, &authorized.clone()), Ok(()));
        let outside = TaskAuthorization::outside_proposals(TaskStatus::Draft);
        assert_eq!(outside.check_unchanged(id, &outside.clone()), Ok(()));
        let moved = TaskAuthorization {
            proposal: Some(TaskProposal {
                id: ProposalId::new(2),
                ..authorized.proposal.clone().unwrap()
            }),
            ..authorized.clone()
        };
        for now in [
            outside.clone(),
            moved,
            in_proposal(ProposalStatus::Submitted, "planner:1"),
            in_proposal(ProposalStatus::Revising, "planner:2"),
        ] {
            assert_eq!(
                authorized.check_unchanged(id, &now),
                Err(DomainError::TaskProposalChangedSinceAuthorized { task_id: id }),
                "{now:?}"
            );
        }
        assert_eq!(
            outside.check_unchanged(id, &authorized),
            Err(DomainError::TaskProposalChangedSinceAuthorized { task_id: id })
        );
        assert_eq!(
            outside.check_unchanged(
                id,
                &TaskAuthorization::outside_proposals(TaskStatus::Submitted)
            ),
            Err(DomainError::TaskStatusChangedSinceAuthorized {
                task_id: id,
                authorized: TaskStatus::Draft,
                status: TaskStatus::Submitted,
            })
        );
    }

    /// Workers, wrappers, the integrator and the jobs read the queue but
    /// neither watch it nor export it to a file; the user, the inbox and a
    /// planner do both (the observer watches too, as
    /// `the_observer_records_findings_and_asks_on_one_only` has it) (moved from `tests/it/cli_authorization.rs`, task
    /// 1709).
    #[test]
    fn workers_wrappers_the_integrator_and_the_jobs_neither_watch_nor_export() {
        for role_ in [
            ActorRole::Worker,
            ActorRole::Wrapper,
            ActorRole::Integrator,
            ActorRole::ReviewJob,
        ] {
            let actor = role(role_);
            assert!(allowed(&actor, C::QueueRead, &Resource::Queue), "{role_:?}");
            for capability in [C::QueueWatch, C::ExportFile, C::CiRead] {
                let error = StaticPolicy
                    .authorize(&actor, capability, &Resource::Queue)
                    .unwrap_err();
                assert_eq!(
                    (error.role, error.capability, error.reason),
                    (role_, capability, DenyReason::NotGranted)
                );
            }
        }
        for role_ in [ActorRole::User, ActorRole::Inbox, ActorRole::Planner] {
            for capability in [C::QueueRead, C::QueueWatch, C::ExportFile, C::CiRead] {
                assert!(
                    allowed(&role(role_), capability, &Resource::Queue),
                    "{role_:?} {capability:?}"
                );
            }
        }
    }

    #[test]
    fn the_jobs_read_and_write_nothing() {
        let writes = [
            C::IntegrationRequest,
            C::Land,
            C::AskAnswer,
            C::AskOpen,
            C::TaskReady,
            C::TaskReadyBypassReview,
            C::TaskCancel,
            C::TaskWrite,
            C::FollowUpJudge,
            C::NoteWrite,
            C::MarkWrite,
            C::GoalClose,
            C::GoalWrite,
            C::FindingRecord,
            C::FindingResolve,
            C::QueueWatch,
            C::CiRead,
            C::PrepareReview,
        ];
        for job in JOBS {
            let actor = role(job);
            assert!(allowed(&actor, C::QueueRead, &Resource::Queue));
            for capability in writes {
                assert!(
                    !allowed(&actor, capability, &Resource::Queue),
                    "{job:?} {capability:?}"
                );
            }
        }
        for job in [
            ActorRole::PlanReviewJob,
            ActorRole::GoalReviewJob,
            ActorRole::ThroughputReviewJob,
        ] {
            for capability in [
                C::ReviewSubmit,
                C::TriageSubmit,
                C::FindingAsk,
                C::MarkWrite,
            ] {
                assert!(!allowed(&role(job), capability, &Resource::Queue));
            }
        }
    }

    #[test]
    fn a_review_or_recovery_job_submits_on_its_own_run_only() {
        let own = run("r1");
        for (job, capability, other) in [
            (ActorRole::ReviewJob, C::ReviewSubmit, C::TriageSubmit),
            (ActorRole::RecoveryJob, C::TriageSubmit, C::ReviewSubmit),
        ] {
            let actor = ActorContext::instance(job, "r1:1").with_run(own.clone(), TaskId::new(1));
            assert!(allowed(&actor, capability, &Resource::run(own.clone())));
            assert!(!allowed(&actor, capability, &Resource::run(run("r2"))));
            assert!(!allowed(&actor, other, &Resource::run(own.clone())));
            // Without its run named, the owner is unknown.
            assert!(!allowed(
                &role(job),
                capability,
                &Resource::run(own.clone())
            ));
        }
    }

    #[test]
    fn the_observer_records_findings_and_asks_on_one_only() {
        let observer = role(ActorRole::Observer);
        let finding = Resource::Finding(FindingId::new(1));
        assert!(allowed(&observer, C::QueueWatch, &Resource::Queue));
        assert!(allowed(&observer, C::FindingRecord, &Resource::Queue));
        assert!(allowed(&observer, C::FindingResolve, &finding));
        assert!(allowed(&observer, C::FindingAsk, &finding));
        assert!(!allowed(&observer, C::FindingAsk, &Resource::Queue));
        for capability in [
            C::TaskCancel,
            C::GoalClose,
            C::FindingDismiss,
            C::AskOpen,
            C::AskAnswer,
            C::NoteWrite,
            C::MarkWrite,
            C::GoalWrite,
            C::TaskWrite,
            C::FollowUpJudge,
            C::IntegrationRequest,
            C::ObserveRun,
            C::ExportFile,
        ] {
            assert!(
                !allowed(&observer, capability, &Resource::Queue),
                "{capability:?}"
            );
        }
    }

    #[test]
    fn the_supervisor_and_the_integrator_have_explicit_sets() {
        let supervisor = role(ActorRole::Supervisor);
        for capability in [
            C::Supervise,
            C::TaskReady,
            C::IntegrationRequest,
            C::RunRecover,
            C::AskOpen,
            C::AskClose,
        ] {
            assert!(allowed(&supervisor, capability, &Resource::Queue));
        }
        // The supervisor closes ended runs' workspaces on its own path
        // (ADR-t1228-1 decision 7).
        for capability in [
            C::AskAnswer,
            C::TaskReadyBypassReview,
            C::Land,
            C::Push,
            C::ReviewSubmit,
            C::WorkspaceCleanup,
        ] {
            assert!(!allowed(&supervisor, capability, &Resource::Queue));
        }
        let integrator = role(ActorRole::Integrator);
        assert_eq!(
            grants(ActorRole::Integrator),
            [C::QueueRead, C::Land, C::Push]
        );
        assert!(allowed(&integrator, C::Land, &Resource::Queue));
        for capability in [C::AskAnswer, C::TaskCancel, C::Supervise] {
            assert!(!allowed(&integrator, capability, &Resource::Queue));
        }
        let wrapper = role(ActorRole::Wrapper);
        assert!(allowed(&wrapper, C::QueueRead, &Resource::Queue));
        assert!(!allowed(&wrapper, C::SessionRecord, &Resource::Queue));
        assert!(!allowed(&wrapper, C::IntegrationRequest, &Resource::Queue));
        // Neither is everything.
        for trusted in [
            ActorRole::Supervisor,
            ActorRole::Integrator,
            ActorRole::Wrapper,
        ] {
            assert!(grants(trusted).len() < Capability::ALL.len());
        }
    }
    /// ADR-t3102-1 decision 4: a session wrapper runs and records only its
    /// own run's or planner's session, by the actor id its principal names.
    #[test]
    fn a_wrapper_runs_and_records_its_own_session_only() {
        let own = run("r1");
        let of_run = ActorContext::instance(ActorRole::Wrapper, &own);
        let of_planner = ActorContext::instance(ActorRole::Wrapper, "planner:7");
        for capability in [C::SessionRun, C::SessionRecord] {
            assert!(allowed(&of_run, capability, &Resource::run(own.clone())));
            assert!(allowed(
                &of_planner,
                capability,
                &Resource::Planner(PlannerId::new(7))
            ));
            for (wrapper, other) in [
                (&of_run, Resource::run(run("r2"))),
                (&of_run, Resource::Planner(PlannerId::new(7))),
                (&of_planner, Resource::Planner(PlannerId::new(8))),
                (&of_planner, Resource::run(own.clone())),
                (&of_planner, Resource::Queue),
            ] {
                let error = StaticPolicy
                    .authorize(wrapper, capability, &other)
                    .unwrap_err();
                assert_eq!(error.reason, DenyReason::Resource, "{other:?}");
            }
        }
        // It reads the queue, and does nothing else.
        assert!(allowed(&of_run, C::QueueRead, &Resource::Queue));
        for capability in [C::AskOpen, C::NoteWrite, C::TelemetryReport] {
            assert!(!allowed(&of_run, capability, &Resource::run(own.clone())));
        }
    }

    fn new_ask(kind: AskKind, run: Option<&str>, task: Option<i64>) -> Resource {
        Resource::NewAsk {
            kind,
            run: run.map(|id| RunId::new(id).unwrap()),
            task: task.map(TaskId::new),
            request: None,
            planner: None,
        }
    }

    #[test]
    fn a_worker_asks_its_question_on_its_own_run_or_task_only() {
        let worker = ActorContext::worker(&run("r1"), TaskId::new(1));
        let question = AskKind::WorkerQuestion;
        assert!(allowed(
            &worker,
            C::AskOpen,
            &new_ask(question.clone(), Some("r1"), None)
        ));
        assert!(allowed(
            &worker,
            C::AskOpen,
            &new_ask(question.clone(), None, Some(1))
        ));
        for other in [
            new_ask(question.clone(), Some("r2"), None),
            new_ask(question.clone(), None, Some(2)),
            new_ask(question.clone(), None, None),
        ] {
            let error = StaticPolicy
                .authorize(&worker, C::AskOpen, &other)
                .unwrap_err();
            assert_eq!(error.reason, DenyReason::Resource, "{other:?}");
        }
        for kind in [
            AskKind::Decide,
            AskKind::ApproveLanding,
            AskKind::AnswerPrompt,
            AskKind::PlannerQuestion,
            AskKind::Blocked,
        ] {
            let error = StaticPolicy
                .authorize(
                    &worker,
                    C::AskOpen,
                    &new_ask(kind.clone(), Some("r1"), None),
                )
                .unwrap_err();
            assert_eq!(error.reason, DenyReason::AskKind, "{kind:?}");
            assert_eq!(
                error.to_string(),
                "worker may not ask.open (not of this kind)"
            );
        }
    }

    #[test]
    fn each_role_opens_its_own_kinds_of_ask() {
        let kinds = [
            AskKind::ApproveLanding,
            AskKind::AnswerPrompt,
            AskKind::Decide,
            AskKind::WorkerQuestion,
            AskKind::PlannerQuestion,
            AskKind::Blocked,
        ];
        for role_ in ActorRole::ALL {
            for kind in &kinds {
                let expected = match role_ {
                    ActorRole::User | ActorRole::Inbox | ActorRole::Supervisor => true,
                    ActorRole::Worker => *kind == AskKind::WorkerQuestion,
                    ActorRole::Planner => *kind == AskKind::PlannerQuestion,
                    _ => false,
                };
                assert_eq!(opens_ask(role_, kind), expected, "{role_:?} {kind:?}");
            }
        }
        let planner = ActorContext::instance(ActorRole::Planner, 7);
        let question = AskKind::PlannerQuestion;
        assert!(allowed(
            &planner,
            C::AskOpen,
            &new_ask(question.clone(), None, Some(3))
        ));
        assert!(allowed(
            &planner,
            C::AskOpen,
            &new_ask(question.clone(), None, None)
        ));
        assert!(!allowed(
            &planner,
            C::AskOpen,
            &new_ask(question, Some("r1"), None)
        ));
        assert!(!allowed(
            &planner,
            C::AskOpen,
            &new_ask(AskKind::Decide, None, Some(3))
        ));
        for role_ in [ActorRole::User, ActorRole::Inbox, ActorRole::Supervisor] {
            assert!(allowed(
                &role(role_),
                C::AskOpen,
                &new_ask(AskKind::Decide, Some("r9"), None)
            ));
        }
        for role_ in [
            ActorRole::Observer,
            ActorRole::ReviewJob,
            ActorRole::RecoveryJob,
            ActorRole::PlanReviewJob,
            ActorRole::GoalReviewJob,
            ActorRole::Wrapper,
            ActorRole::Integrator,
        ] {
            let error = StaticPolicy
                .authorize(
                    &role(role_),
                    C::AskOpen,
                    &new_ask(AskKind::Blocked, None, None),
                )
                .unwrap_err();
            assert_eq!(error.reason, DenyReason::NotGranted, "{role_:?}");
        }
        assert_eq!(
            new_ask(AskKind::WorkerQuestion, Some("r1"), None).record(),
            serde_json::json!({"kind": "new_ask", "ask_kind": "worker_question", "run": "r1", "task": null})
        );
    }

    #[test]
    fn only_the_user_and_the_inbox_answer_and_the_supervisor_also_closes() {
        let ask = Resource::Ask {
            id: AskId::new(1),
            run: Some(run("r1")),
        };
        for role_ in ActorRole::ALL {
            let mut actor = role(role_);
            if role_ == ActorRole::Worker {
                actor = ActorContext::worker(&run("r1"), TaskId::new(1));
            }
            let answers = matches!(role_, ActorRole::User | ActorRole::Inbox);
            let closes = answers || role_ == ActorRole::Supervisor;
            assert_eq!(allowed(&actor, C::AskAnswer, &ask), answers, "{role_:?}");
            assert_eq!(allowed(&actor, C::AskClose, &ask), closes, "{role_:?}");
        }
    }

    /// The eval's requests and reads (ADR-t1728-1 decisions 6 and 7): a
    /// worker asks for a dev round and reads the rounds of its own run;
    /// the user, the inbox and a planner ask for every split; only the
    /// user and the inbox run a held-out set again; the supervisor asks for
    /// none (it runs them) and reads them.
    #[test]
    fn the_evals_requests_are_split_by_role_and_a_worker_keeps_to_its_run() {
        let worker = ActorContext::worker(&run("r1"), TaskId::new(1));
        let own = Resource::AgentEval {
            run: Some(run("r1")),
        };
        let other = Resource::AgentEval {
            run: Some(run("r2")),
        };
        let unrun = Resource::AgentEval { run: None };
        let may = |actor: &ActorContext, needs: Vec<Capability>, on: &Resource| {
            needs.into_iter().all(|need| allowed(actor, need, on))
        };
        assert!(may(&worker, eval_request_needs(false, false), &own));
        assert!(!may(&worker, eval_request_needs(false, false), &other));
        assert!(!may(&worker, eval_request_needs(false, false), &unrun));
        for (held_out, rerun) in [(true, false), (false, true), (true, true)] {
            assert!(!may(&worker, eval_request_needs(held_out, rerun), &own));
        }
        assert!(allowed(&worker, C::EvalRead, &own));
        assert!(!allowed(&worker, C::EvalRead, &other));
        assert!(!allowed(&worker, C::EvalRead, &unrun));
        for role_ in [ActorRole::User, ActorRole::Inbox, ActorRole::Planner] {
            let actor = role(role_);
            for held_out in [false, true] {
                assert!(
                    may(&actor, eval_request_needs(held_out, false), &unrun),
                    "{role_:?}"
                );
            }
            assert!(allowed(&actor, C::EvalRead, &unrun), "{role_:?}");
            let reruns = role_ != ActorRole::Planner;
            assert_eq!(
                may(&actor, eval_request_needs(true, true), &unrun),
                reruns,
                "{role_:?}"
            );
        }
        let supervisor = role(ActorRole::Supervisor);
        for (held_out, rerun) in [(true, false), (true, true), (false, true)] {
            let error = eval_request_needs(held_out, rerun)
                .into_iter()
                .find_map(|need| StaticPolicy.authorize(&supervisor, need, &unrun).err())
                .expect("refused");
            assert_eq!(error.reason, DenyReason::NotGranted);
        }
        assert!(allowed(&supervisor, C::EvalRead, &unrun));
        assert!(allowed(&supervisor, C::Supervise, &Resource::Queue));
        for role_ in JOBS {
            assert!(!may(&role(role_), eval_request_needs(false, false), &unrun));
        }
        assert_eq!(
            own.record(),
            serde_json::json!({"kind": "agent_eval", "run": "r1"})
        );
    }

    /// A production set of an agent's cases (ADR-t1728-1 (g)): the user,
    /// the inbox and a planner ask for one and decide its labels; a worker
    /// (on its own run too), the supervisor, which makes the sets it is
    /// asked for, and the jobs do not.
    #[test]
    fn only_the_user_the_inbox_and_a_planner_ask_for_a_production_set() {
        let unrun = Resource::AgentEval { run: None };
        for role_ in ActorRole::ALL {
            let asks = matches!(
                role_,
                ActorRole::User | ActorRole::Inbox | ActorRole::Planner
            );
            assert_eq!(
                allowed(&role(role_), C::EvalCases, &unrun),
                asks,
                "{role_:?}"
            );
        }
        let worker = ActorContext::worker(&run("r1"), TaskId::new(1));
        let own = Resource::AgentEval {
            run: Some(run("r1")),
        };
        let error = StaticPolicy
            .authorize(&worker, C::EvalCases, &own)
            .unwrap_err();
        assert_eq!(error.reason, DenyReason::NotGranted);
        let error = StaticPolicy
            .authorize(&role(ActorRole::Supervisor), C::EvalCases, &unrun)
            .unwrap_err();
        assert_eq!(error.reason, DenyReason::NotGranted);
    }

    #[test]
    fn only_the_user_and_the_inbox_record_a_request_and_only_its_planner_declines_it() {
        for role_ in ActorRole::ALL {
            let records = matches!(role_, ActorRole::User | ActorRole::Inbox);
            assert_eq!(
                allowed(&role(role_), C::RequestRecord, &Resource::Queue),
                records,
                "{role_:?}"
            );
        }
        let request = |planner: Option<i64>| Resource::Request {
            id: RequestId::new(3),
            planner: planner.map(PlannerId::new),
        };
        let planner = ActorContext::instance(ActorRole::Planner, 7);
        assert!(allowed(&planner, C::RequestDecline, &request(Some(7))));
        assert!(!allowed(&planner, C::RequestDecline, &request(Some(8))));
        assert!(!allowed(&planner, C::RequestDecline, &request(None)));
        for role_ in ActorRole::ALL
            .into_iter()
            .filter(|role_| *role_ != ActorRole::Planner)
        {
            assert!(
                !allowed(&role(role_), C::RequestDecline, &request(Some(1))),
                "{role_:?}"
            );
        }
        assert_eq!(
            request(Some(7)).record(),
            serde_json::json!({"kind": "request", "id": 3, "planner": 7})
        );
    }

    #[test]
    fn only_a_requests_own_planner_asks_on_it() {
        let on_request = |planner: Option<i64>| Resource::NewAsk {
            kind: AskKind::PlannerQuestion,
            run: None,
            task: None,
            request: Some(RequestId::new(3)),
            planner: planner.map(PlannerId::new),
        };
        let planner = ActorContext::instance(ActorRole::Planner, 7);
        assert!(allowed(&planner, C::AskOpen, &on_request(Some(7))));
        // Another request's planner, or a draft's or a finding's, and any
        // planner while the request has none open.
        assert_eq!(
            StaticPolicy.authorize(&planner, C::AskOpen, &on_request(Some(8))),
            Err(AuthorizationError {
                role: ActorRole::Planner,
                capability: C::AskOpen,
                reason: DenyReason::Resource,
            })
        );
        assert!(!allowed(&planner, C::AskOpen, &on_request(None)));
        // Without a request, a planner's question is as before.
        assert!(allowed(
            &planner,
            C::AskOpen,
            &new_ask(AskKind::PlannerQuestion, None, None)
        ));
        // The people's and the supervisor's asks keep no owner.
        for role_ in [ActorRole::User, ActorRole::Inbox, ActorRole::Supervisor] {
            assert!(allowed(&role(role_), C::AskOpen, &on_request(None)));
        }
        assert_eq!(
            on_request(Some(7)).record(),
            serde_json::json!({
                "kind": "new_ask",
                "ask_kind": "planner_question",
                "run": null,
                "task": null,
                "request": 3,
                "planner": 7,
            })
        );
    }

    #[test]
    fn a_planner_notes_on_any_task_but_changes_only_those_before_they_start() {
        let planner = ActorContext::instance(ActorRole::Planner, 7);
        for status in [TaskStatus::InProgress, TaskStatus::Completed] {
            assert!(allowed(&planner, C::NoteWrite, &task_in(status)));
            assert!(!allowed(&planner, C::TaskWrite, &task_in(status)));
        }
        assert!(allowed(
            &planner,
            C::NoteWrite,
            &Resource::task(TaskId::new(1))
        ));
    }
}

#[cfg(test)]
mod membership_policy_tests {
    use super::*;
    #[test]
    fn only_people_and_runtime_planners_judge_even_after_a_task_started() {
        for role in ActorRole::ALL {
            let actor = ActorContext::instance(role, 1);
            for status in [
                TaskStatus::Draft,
                TaskStatus::Submitted,
                TaskStatus::Ready,
                TaskStatus::InProgress,
                TaskStatus::Completed,
            ] {
                let result = StaticPolicy.authorize(
                    &actor,
                    Capability::FollowUpJudge,
                    &TaskAuthorization::outside_proposals(status).resource(TaskId::new(1)),
                );
                assert_eq!(
                    result.is_ok(),
                    matches!(
                        role,
                        ActorRole::User | ActorRole::Inbox | ActorRole::Planner
                    ),
                    "{role:?} {status:?}"
                );
            }
        }
    }

    /// ADR-t1975-1 decision 3: a priority a person set (on a task, its own
    /// or the goal's it takes; on a goal) is changed by the user and the
    /// inbox only, and the refusal names the person's value; one the AI set
    /// is changed by anyone the allowlist lets.
    #[test]
    fn only_the_user_and_the_inbox_change_a_persons_priority() {
        let task = PriorityHolder::Task(TaskId::new(5));
        let goal = PriorityHolder::Goal(GoalId::new(3));
        for role in ActorRole::ALL {
            let person = matches!(role, ActorRole::User | ActorRole::Inbox);
            assert_eq!(changes_persons_priority(role), person, "{role:?}");
            for holder in [task, goal] {
                let result =
                    check_priority_change(role, holder, Priority::Interrupt, PriorityBy::Human);
                assert_eq!(result.is_ok(), person, "{role:?} {holder}");
                assert!(
                    check_priority_change(role, holder, Priority::Low, PriorityBy::Ai).is_ok(),
                    "{role:?} {holder}"
                );
            }
        }
        assert_eq!(
            check_priority_change(
                ActorRole::Planner,
                task,
                Priority::Interrupt,
                PriorityBy::Human
            )
            .unwrap_err()
            .to_string(),
            "a person set the priority of task 5 (interrupt): planner may not change it; only the user or the inbox, at a person's word, may (a doubt goes to a person)"
        );
        assert_eq!(
            check_priority_change(
                ActorRole::Supervisor,
                goal,
                Priority::High,
                PriorityBy::Human
            ),
            Err(DomainError::PersonsPriority {
                holder: goal,
                priority: Priority::High,
                role: ActorRole::Supervisor,
            })
        );
        assert_eq!(DenyReason::PersonsPriority.as_str(), "a person's priority");
    }
}

#[cfg(test)]
mod telemetry_policy_tests {
    use super::*;
    use crate::domain::queue_service::Principal;
    use crate::domain::session_step::{StepTarget, TargetKind};

    fn allowed(actor: &ActorContext, capability: Capability, resource: &Resource) -> bool {
        StaticPolicy.authorize(actor, capability, resource).is_ok()
    }

    /// `target`'s resource, of a session registered to it.
    fn owned(target: &StepTarget) -> Resource {
        Resource::Telemetry {
            target: target.clone(),
            owner: Some(target.clone()),
        }
    }

    fn target(kind: TargetKind, id: &str) -> StepTarget {
        StepTarget {
            kind,
            id: id.to_owned(),
        }
    }

    /// ADR-t1662-3 decisions 9 and 10: the telemetry principal sends the
    /// steps of its own target and nothing else, and no other role sends
    /// any, an agent's token of the same target included.
    #[test]
    fn only_the_telemetry_principal_reports_and_only_of_its_own_target() {
        let run = RunId::new("r1").unwrap();
        let own = StepTarget::run(&run);
        let others = [
            StepTarget::run(&RunId::new("r2").unwrap()),
            target(TargetKind::Planner, "3"),
            target(TargetKind::PlanReview, "4"),
            target(TargetKind::GoalReview, "5"),
            target(TargetKind::ThroughputReview, "6"),
        ];
        let telemetry = Principal::telemetry(&own, Some(TaskId::new(1))).actor();
        assert_eq!(telemetry.role(), ActorRole::ExecutorTelemetry);
        assert!(allowed(&telemetry, C::TelemetryReport, &owned(&own)));
        for other in &others {
            let refused = StaticPolicy
                .authorize(&telemetry, C::TelemetryReport, &owned(other))
                .unwrap_err();
            assert_eq!(refused.reason, DenyReason::Resource, "{other:?}");
        }
        let of_planner = Principal::telemetry(&others[1], None).actor();
        assert!(allowed(&of_planner, C::TelemetryReport, &owned(&others[1])));
        assert!(!allowed(
            &of_planner,
            C::TelemetryReport,
            &owned(&others[2])
        ));
        // Its own target's session registered to no target, or to
        // another, is refused it.
        for owner in [None, Some(others[0].clone())] {
            let refused = StaticPolicy
                .authorize(
                    &telemetry,
                    C::TelemetryReport,
                    &Resource::Telemetry {
                        target: own.clone(),
                        owner,
                    },
                )
                .unwrap_err();
            assert_eq!(refused.reason, DenyReason::Resource);
        }
        // Every other capability is refused it, a read included.
        for capability in Capability::ALL
            .into_iter()
            .filter(|capability| *capability != C::TelemetryReport)
        {
            for resource in [
                Resource::Queue,
                Resource::run(run.clone()),
                Resource::task(TaskId::new(1)),
                owned(&own),
            ] {
                assert!(
                    !allowed(&telemetry, capability, &resource),
                    "{capability} on {resource:?}"
                );
            }
        }
        // No other role reports, whatever it acts on: the worker of the
        // run, its jobs and the planner of the target among them.
        let agents = [
            ActorContext::worker(&run, TaskId::new(1)),
            ActorContext::review_job(&run, 1),
            ActorContext::recovery_job(&run, "a", 1),
            ActorContext::plan_review_job(4, 1),
            ActorContext::goal_review_job(5, 1),
            ActorContext::throughput_review_job("hourly", "h"),
            ActorContext::instance(ActorRole::Observer, 1),
            ActorContext::instance(ActorRole::Planner, 3),
        ];
        for actor in agents.iter().cloned().chain(
            ActorRole::ALL
                .into_iter()
                .filter(|role| *role != ActorRole::ExecutorTelemetry)
                .map(|role| ActorContext::instance(role, 1)),
        ) {
            for target in std::iter::once(&own).chain(&others) {
                let refused = StaticPolicy
                    .authorize(&actor, C::TelemetryReport, &owned(target))
                    .unwrap_err();
                assert_eq!(refused.reason, DenyReason::NotGranted, "{actor:?}");
            }
        }
        assert_eq!(grants(ActorRole::ExecutorTelemetry), &[C::TelemetryReport]);
        assert_eq!(
            owned(&others[2]).record(),
            serde_json::json!({"kind": "telemetry", "target": "plan_review", "id": "4",
                               "owner": "plan_review:4"})
        );
    }
}
