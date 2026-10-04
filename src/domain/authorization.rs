//! What an actor may do (ADR-t728-1 decision 5): a [`Capability`] on a
//! [`Resource`], allowed or refused by an [`Authorizer`]. The policy is a
//! static allowlist per [`ActorRole`] plus the resource rules of a few
//! roles (a worker acts on its own run, a planner withdraws its own
//! proposal and changes tasks that have not started). Everything not
//! listed is refused (default deny), and so is a resource whose owner the
//! rule needs but the caller could not name (fail closed). On a host the
//! check is advisory: any process can set `DAGQ_ROLE` (ADR-t728-1
//! decision 6). The table is in `docs/design/authorization.md`.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{
    ActorContext, ActorRole, AskId, AskKind, FindingId, GoalId, PlannerId, ProposalId, RequestId,
    RunId,
};
use super::{DomainError, TaskId, TaskStatus};

string_enum!(Capability {
    // Reading.
    QueueRead => "queue.read",
    QueueWatch => "queue.watch",
    ExportFile => "queue.export",
    // Planning.
    GoalWrite => "goal.write",
    GoalReady => "goal.ready",
    GoalClose => "goal.close",
    GoalReviewRequest => "goal.review_request",
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
    // A session's screen read and keys or an answer typed into it, by
    // its run or planner id (ADR-t1228-1 decisions 4 and 5).
    ScreenRead => "screen.read",
    ScreenSend => "screen.send",
    // A follow-up request handed to a headless planner of the runtime's
    // as its next turn, by its planner id (ADR-t1533-1).
    PlannerRequest => "planner.request",
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
    // Reserved for a later stage (sandboxed execution): no role has them.
    FilesystemRead => "reserved.filesystem_read",
    FilesystemWrite => "reserved.filesystem_write",
    NetworkAccess => "reserved.network",
    SecretRead => "reserved.secret_read",
});

impl Capability {
    pub const ALL: [Self; 49] = [
        Self::QueueRead,
        Self::QueueWatch,
        Self::ExportFile,
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
        Self::ScreenRead,
        Self::ScreenSend,
        Self::PlannerRequest,
        Self::Supervise,
        Self::RunRecover,
        Self::WorkspaceCleanup,
        Self::ServiceLifecycle,
        Self::BinaryInstall,
        Self::QueueAdmin,
        Self::IntegrationRequest,
        Self::Land,
        Self::Push,
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
    Task {
        id: TaskId,
        status: Option<TaskStatus>,
    },
    Run {
        id: RunId,
        task: Option<TaskId>,
    },
    Ask {
        id: AskId,
        run: Option<RunId>,
    },
    /// An ask to open: its kind, and the run or the task it is about as
    /// the command names them.
    NewAsk {
        kind: AskKind,
        run: Option<RunId>,
        task: Option<TaskId>,
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
    /// A resource the caller named but could not read (such as a malformed
    /// run id): no owner can match it.
    Unresolved,
}

impl Resource {
    pub const fn task(id: TaskId) -> Self {
        Self::Task { id, status: None }
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
            Self::Task { id, status } => json!({"kind": "task", "id": id, "status": status}),
            Self::Run { id, task } => json!({"kind": "run", "id": id, "task": task}),
            Self::Ask { id, run } => json!({"kind": "ask", "id": id, "run": run}),
            Self::NewAsk { kind, run, task } => {
                json!({"kind": "new_ask", "ask_kind": kind, "run": run, "task": task})
            }
            Self::Proposal { id, owner } => json!({"kind": "proposal", "id": id, "owner": owner}),
            Self::Finding(id) => json!({"kind": "finding", "id": id}),
            Self::Planner(id) => json!({"kind": "planner", "id": id}),
            Self::Request { id, planner } => {
                json!({"kind": "request", "id": id, "planner": planner})
            }
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
}

impl DenyReason {
    /// The reason as the error and the record name it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotGranted => "not granted",
            Self::Reserved => "reserved",
            Self::Resource => "not on this resource",
            Self::AskKind => "not of this kind",
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

/// What a person at a plain terminal may do: everything but the jobs'
/// submissions and the integrator's own landing and push.
const USER: &[Capability] = &[
    C::QueueRead,
    C::QueueWatch,
    C::ExportFile,
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
    C::ScreenRead,
    C::ScreenSend,
    C::PlannerRequest,
    C::Supervise,
    C::RunRecover,
    C::WorkspaceCleanup,
    C::ServiceLifecycle,
    C::BinaryInstall,
    C::QueueAdmin,
    C::IntegrationRequest,
];

/// The planner's authority as it is (ADR-t728-1 decision 7): goals, tasks
/// before they start, its proposals, notes, marks, resolving and dismissing
/// findings, its
/// questions and, at a person's word, `up` / `down` / `install`. No run,
/// no landing, no `ready`, no answer.
const PLANNER: &[Capability] = &[
    C::QueueRead,
    C::QueueWatch,
    C::ExportFile,
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
    C::SessionRun,
    C::SessionRecord,
    C::FindingResolve,
    C::FindingDismiss,
    C::PlannerOpen,
    C::RequestDecline,
    C::ServiceLifecycle,
    C::BinaryInstall,
    C::QueueAdmin,
];

/// A worker reads, asks, notes, and runs and records its session, on its
/// own run: the runtime's wrapper and hooks run in the worker's
/// environment (task 734).
const WORKER: &[Capability] = &[
    C::QueueRead,
    C::AskOpen,
    C::NoteWrite,
    C::SessionRun,
    C::SessionRecord,
];

/// A review job reads; its verdict comes back as data on its own run.
const REVIEW_JOB: &[Capability] = &[C::QueueRead, C::ReviewSubmit];
/// A recovery job reads; its verdict comes back as data on its own run.
const RECOVERY_JOB: &[Capability] = &[C::QueueRead, C::TriageSubmit];
/// Plan review, goal review and the throughput review read; the supervisor
/// applies their verdicts and saves the review.
const READ_ONLY: &[Capability] = &[C::QueueRead];

/// The observer (ADR-0044 decision 4): reads, records and resolves
/// findings, and asks `blocked` on a finding.
const OBSERVER: &[Capability] = &[
    C::QueueRead,
    C::QueueWatch,
    C::FindingRecord,
    C::FindingResolve,
    C::FindingAsk,
];

/// The supervisor's transitions. It asks the integrator to land
/// (ADR-t728-2) and applies answers without writing them. Its automatic
/// update checks, migrates and probes queues with the new binary as
/// `install` does (task 734).
const SUPERVISOR: &[Capability] = &[
    C::QueueRead,
    C::QueueWatch,
    C::ExportFile,
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
    C::Supervise,
    C::RunRecover,
    C::ServiceLifecycle,
    C::BinaryInstall,
    C::QueueAdmin,
    C::IntegrationRequest,
];

/// The session wrapper: its session and the session record.
const WRAPPER: &[Capability] = &[C::QueueRead, C::SessionRun, C::SessionRecord];

/// The integrator lands and pushes (ADR-t728-2).
const INTEGRATOR: &[Capability] = &[C::QueueRead, C::Land, C::Push];

/// The allowlist of `role`. The inbox has the user's (ADR-t728-3 decision
/// 1: it acts at a person's word); the record tells the two apart.
pub const fn grants(role: ActorRole) -> &'static [Capability] {
    match role {
        ActorRole::User | ActorRole::Inbox => USER,
        ActorRole::Planner => PLANNER,
        ActorRole::Worker => WORKER,
        ActorRole::ReviewJob => REVIEW_JOB,
        ActorRole::RecoveryJob => RECOVERY_JOB,
        ActorRole::PlanReviewJob | ActorRole::GoalReviewJob | ActorRole::ThroughputReviewJob => {
            READ_ONLY
        }
        ActorRole::Observer => OBSERVER,
        ActorRole::Supervisor => SUPERVISOR,
        ActorRole::Wrapper => WRAPPER,
        ActorRole::Integrator => INTEGRATOR,
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
        _ => true,
    }
}

/// A worker's run, its task, or an ask on its run.
fn worker_owns(actor: &ActorContext, resource: &Resource) -> bool {
    let Some(own) = actor.run_id() else {
        return false;
    };
    match resource {
        Resource::Run { id, .. }
        | Resource::Ask { run: Some(id), .. }
        | Resource::NewAsk { run: Some(id), .. } => id == own,
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
        // Changes to tasks before they start (draft, submitted, ready); a
        // note on any task.
        Resource::Task { status, .. } if matches!(capability, C::TaskWrite | C::TaskCancel) => {
            matches!(
                status,
                Some(TaskStatus::Draft | TaskStatus::Submitted | TaskStatus::Ready)
            )
        }
        Resource::Proposal { owner, .. } if capability == C::ProposalWithdraw => {
            owner.as_deref() == Some(actor.actor_id())
        }
        Resource::Planner(id) => actor.actor_id() == format!("planner:{id}"),
        // Only the request's own planner declines it.
        Resource::Request { planner, .. } => {
            planner.is_some_and(|id| actor.actor_id() == format!("planner:{id}"))
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_user_and_inbox_can_correct_an_ended_tasks_verify() {
        let task = Resource::Task {
            id: TaskId::new(1),
            status: Some(TaskStatus::InProgress),
        };
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
        Resource::Task {
            id: TaskId::new(1),
            status: Some(status),
        }
    }

    fn role(role: ActorRole) -> ActorContext {
        ActorContext::instance(role, 1)
    }

    const JOBS: [ActorRole; 5] = [
        ActorRole::ReviewJob,
        ActorRole::RecoveryJob,
        ActorRole::PlanReviewJob,
        ActorRole::GoalReviewJob,
        ActorRole::ThroughputReviewJob,
    ];

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

    #[test]
    fn every_role_reads_and_is_refused_what_it_was_not_granted() {
        for role_ in ActorRole::ALL {
            let actor = role(role_);
            assert!(allowed(&actor, C::QueueRead, &Resource::Queue), "{role_:?}");
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
        assert!(allowed(&worker, C::SessionRun, &Resource::run(own.clone())));
        assert!(allowed(
            &worker,
            C::SessionRecord,
            &Resource::run(own.clone())
        ));
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
        for capability in [C::AskOpen, C::SessionRun, C::SessionRecord, C::NoteWrite] {
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
        assert!(allowed(
            &planner,
            C::SessionRun,
            &Resource::Planner(PlannerId::new(7))
        ));
        assert!(!allowed(
            &planner,
            C::SessionRun,
            &Resource::Planner(PlannerId::new(8))
        ));
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
        assert!(allowed(&wrapper, C::SessionRecord, &Resource::Queue));
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
    fn new_ask(kind: AskKind, run: Option<&str>, task: Option<i64>) -> Resource {
        Resource::NewAsk {
            kind,
            run: run.map(|id| RunId::new(id).unwrap()),
            task: task.map(TaskId::new),
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
                    &Resource::Task {
                        id: TaskId::new(1),
                        status: Some(status),
                    },
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
}
