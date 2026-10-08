//! The ports of 計画管理 (docs/design/architecture.md, section
//! "portのmodule"): tasks and goals, the planners' drafts and requests,
//! and the plan and goal reviews.

use crate::application::{GraphInput, TaskPage, TaskQuery};
use crate::domain::{
    Ask, AskId, AskOutcome, ClaimOutcome, CommitSha, DraftOrigin, DraftTarget, EventId, EventKind,
    Finding, FindingId, FindingStatus, FindingView, Goal, GoalDetail, GoalEdit, GoalId,
    GoalPredecessor, GoalSummary, GoalVerdict, LeaseToken, LintInput, NewAsk, NewGoal, NewNote,
    NewTask, NotePage, NoteQuery, PlanReviewCandidate, PlanReviewDecision, PlanReviewVerdict,
    PlannerId, PlannerSession, Predecessor, Priority, Proposal, ProposalId, RunEvent, RunId,
    StrandedDependency, Submission, Task, TaskAction, TaskDetail, TaskEdit, TaskId, TaskStatus,
    goal_review::{GoalReviewDecision, GoalReviewVerdict},
};
use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};

pub trait TaskStore {
    fn add(&mut self, task: NewTask) -> Result<Task>;
    /// One page of tasks matching `query`, newest first.
    fn list(&self, query: &TaskQuery) -> Result<TaskPage>;
    fn show(&mut self, task_id: TaskId) -> Result<TaskDetail>;
    fn transition(&mut self, task_id: TaskId, action: TaskAction) -> Result<Task>;
    /// Cancel a task as a duplicate of another (ADR-0046 decision 5),
    /// recording which in its `task_status_changed`. The other task must
    /// exist, differ and not be canceled.
    fn cancel_duplicate(&mut self, task_id: TaskId, duplicate_of: TaskId) -> Result<Task>;
    fn add_dependency(&mut self, task_id: TaskId, predecessor_id: TaskId) -> Result<()>;
    fn remove_dependency(&mut self, task_id: TaskId, predecessor_id: TaskId) -> Result<()>;
    /// Make a draft or ready task wait until `goal_id` is closed as achieved
    /// (ADR-0038); never its own goal, never a cycle.
    fn add_goal_dependency(&mut self, task_id: TaskId, goal_id: GoalId) -> Result<()>;
    fn remove_goal_dependency(&mut self, task_id: TaskId, goal_id: GoalId) -> Result<()>;
    /// Dependency-ready tasks in claim order (ADR-0040 decision 4); each
    /// task is limited to one unfinished run.
    fn candidates(&self) -> Result<Vec<Task>>;
    /// The ready tasks that wait for a build containing their
    /// dependencies' landings (ADR-t1632-1), each with the landed commits
    /// of its direct predecessors (none for a predecessor that landed
    /// nothing).
    fn build_waits(&self)
    -> Result<std::collections::HashMap<TaskId, Vec<crate::domain::Landing>>>;
    /// The unfinished tasks with their direct predecessors and the IDs of
    /// `candidates`, read in one snapshot.
    fn graph_input(&self) -> Result<GraphInput>;
    /// Reserve one run atomically, without a lease. Does not start a process or validate Git objects.
    fn claim(&mut self, base_commit: &CommitSha) -> Result<ClaimOutcome>;
    /// Direct predecessors of a task, each with the run that landed it, in ID order.
    fn predecessors(&self, task_id: TaskId) -> Result<Vec<Predecessor>>;
    /// Goals a task depends on, in ID order, each with its completed tasks
    /// and the runs that landed them.
    fn goal_predecessors(&self, task_id: TaskId) -> Result<Vec<GoalPredecessor>>;
    /// Tasks that are `in_progress` right now, in ID order.
    fn tasks_in_progress(&self) -> Result<Vec<Task>>;
    fn add_goal(&mut self, goal: NewGoal) -> Result<Goal>;
    /// Every goal in ID order with its task counts by status.
    fn list_goals(&self) -> Result<Vec<GoalSummary>>;
    fn show_goal(&mut self, goal_id: GoalId) -> Result<GoalDetail>;
    /// Replace the given fields; running runs keep their prompt snapshot.
    fn edit_goal(&mut self, goal_id: GoalId, edit: GoalEdit) -> Result<Goal>;
    /// Record the verdict once. `achieved` is refused while a task is not
    /// completed or canceled; `abandoned` while a task is in progress.
    fn close_goal(&mut self, goal_id: GoalId, verdict: GoalVerdict) -> Result<Goal>;
    /// Move a draft or ready task to an open goal, or to none.
    fn set_goal(&mut self, task_id: TaskId, goal_id: Option<GoalId>) -> Result<Task>;
    /// Replace the globs of the paths a draft or ready task may change
    /// (ADR-0029); an empty list removes the limit.
    fn set_paths(&mut self, task_id: TaskId, paths: Vec<String>) -> Result<Task>;
    /// Replace the given fields of a draft task (ADR-0041 decision 9),
    /// recording `task_edited` with the fields that changed; running runs
    /// keep their prompt snapshot. `authorized` is the status the caller
    /// authorized the edit with; a task whose status differs in the
    /// transaction is refused unchanged (ADR-t883-1).
    fn edit_task(
        &mut self,
        task_id: TaskId,
        edit: TaskEdit,
        authorized: TaskStatus,
    ) -> Result<Task>;
    /// Give a draft or ready task a priority of its own, or with none let
    /// it inherit its goal's (ADR-0040 decision 4, ADR-t1639-1 decision 2);
    /// it takes effect at the next claim.
    fn set_priority(&mut self, task_id: TaskId, priority: Option<Priority>) -> Result<Task>;
    /// Bundle draft tasks, the draft tasks of the given goals and those
    /// goals into a proposal and submit it for plan review (ADR-0041
    /// decisions 7, 8): the tasks become `submitted`, which no claim takes.
    /// With a proposal ID, submit that proposal again after a revise,
    /// with the drafts it holds. A task or goal of another active proposal
    /// is refused.
    fn submit(&mut self, submission: Submission) -> Result<Proposal>;
    /// The plan-review path to `ready` (ADR-0041 decisions 8, 11): the
    /// submitted proposal is accepted, its submitted tasks become ready and
    /// its draft goals open.
    fn approve_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal>;
    /// Plan review sends the submitted proposal back to its planner: its
    /// submitted tasks return to draft.
    fn send_back_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal>;
    /// Withdraw a submitted or revising proposal: it ends as canceled, its
    /// submitted tasks return to draft, and its tasks and goals are free to
    /// join another proposal.
    fn withdraw_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal>;
    fn show_proposal(&self, proposal_id: ProposalId) -> Result<Proposal>;
    /// The submitted and revising proposals, oldest submission first; with
    /// `all`, every proposal.
    fn proposals(&self, all: bool) -> Result<Vec<Proposal>>;
    /// What `lint` checks `tasks` against (ADR-0041 decision 10), read in
    /// one snapshot: the tasks in the order given, every task's status and
    /// dependencies, and every goal's verdict. A missing task is an error.
    fn lint_input(&self, tasks: &[TaskId]) -> Result<LintInput>;
    /// Open a draft goal so its tasks become candidates (ADR-0024 decision 5).
    fn ready_goal(&mut self, goal_id: GoalId) -> Result<Goal>;
    /// Record a note as an `observation` run event on its task, run or goal.
    fn add_note(&mut self, note: NewNote) -> Result<RunEvent>;
    /// One page of notes, oldest first.
    fn notes(&self, query: &NoteQuery) -> Result<NotePage>;
}

/// How [`DraftPlannerStore::open_draft_planner`] ended.
#[derive(Debug, Clone)]
pub enum DraftPlannerStart {
    /// A planner of the runtime's is recorded for the bundle of drafts of
    /// `key` (ADR-t807-1): each member with which planner this is for it
    /// (1-based), oldest first; the caller opens its workspace. `exhausted`
    /// are the drafts left out as below.
    Opened {
        planner: Box<PlannerSession>,
        key: crate::domain::BundleKey,
        members: Vec<(DraftTarget, usize)>,
        exhausted: Vec<TaskId>,
    },
    /// [`crate::domain::MAX_DRAFT_PLANNERS`] planners ended without
    /// deciding each of these drafts: `draft_planner_exhausted` is
    /// recorded and a person decides.
    Exhausted { drafts: Vec<TaskId> },
    /// Not now: the draft moved on, or another planner took it.
    Skipped,
}

/// How [`DraftPlannerStore::open_finding_planner`] ended.
#[derive(Debug, Clone)]
pub enum FindingPlannerStart {
    /// A planner of the runtime's is recorded for the finding, its
    /// `attempt`-th since the finding was marked; the caller opens its
    /// workspace.
    Opened {
        planner: Box<PlannerSession>,
        finding: Box<Finding>,
        attempt: usize,
    },
    /// [`crate::domain::MAX_FINDING_PLANNERS`] planners ended without
    /// deciding the finding: `finding_planner_exhausted` is recorded and a
    /// person decides.
    Exhausted { attempts: usize },
    /// The improvements running reached the limit (ADR-0051 decision 25):
    /// the finding waits, `open`, for one to end.
    AtLimit(crate::domain::ImprovementLimit),
    /// Not now: the finding moved on, or another planner took it.
    Skipped,
}

/// How [`PlanRequestStore::open_request_planner`] ended.
#[derive(Debug, Clone)]
pub enum RequestPlannerStart {
    /// A planner of the runtime's is recorded for the request, its
    /// `attempt`-th for it; the caller hands it the request and opens its
    /// workspace.
    Opened {
        planner: Box<PlannerSession>,
        request: Box<crate::domain::plan_request::PlanRequest>,
        attempt: usize,
    },
    /// [`crate::domain::plan_request::MAX_REQUEST_PLANNERS`] planners ended
    /// without deciding the request: it is `exhausted`, with
    /// `request_planner_exhausted`, and the inbox decides.
    Exhausted { attempts: usize },
    /// Not now: the request moved on, or another planner took it.
    Skipped,
}

/// The planning requests the inbox records for planners of the runtime's
/// (ADR-t1394-1): the ones waiting for a planner, the planner opened for
/// each, and what its prompt reads of what a request refers to.
pub trait PlanRequestStore {
    /// The `open` requests waiting for a planner of the runtime's, oldest
    /// first: none open for it, no `planner_question` about it nobody
    /// closed.
    fn planner_requests(&self) -> Result<Vec<crate::domain::plan_request::PlanRequest>>;
    /// Record a planner of the runtime's for `request` after re-checking it
    /// in the same write transaction (`request_planner_opened`); with
    /// `answer`, one that carries that answered `planner_question` about
    /// it. Past [`crate::domain::plan_request::MAX_REQUEST_PLANNERS`]
    /// planners (without `answer`), the request is made `exhausted`
    /// instead.
    fn open_request_planner(
        &mut self,
        request: crate::domain::RequestId,
        answer: Option<AskId>,
    ) -> Result<RequestPlannerStart>;
    fn plan_request(
        &self,
        request: crate::domain::RequestId,
    ) -> Result<crate::domain::plan_request::PlanRequest>;
    /// The asks about `request`, oldest first: its planners' questions.
    fn request_asks(&self, request: crate::domain::RequestId) -> Result<Vec<Ask>>;
    /// The event `id`, when there is one: what a request refers to.
    fn event_by_id(&self, id: EventId) -> Result<Option<RunEvent>>;
}

/// Where the answer of a `planner_question` goes (ADR-0041 decision 13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannerAnswerRoute {
    /// Typed into the workspace of this planner of the runtime's, which
    /// works on the ask's task (its draft, or its proposal).
    Planner(Box<PlannerSession>),
    /// The draft's planner is gone: a new one is opened with the answer.
    NewPlanner,
    /// The draft moved on (submitted, canceled) with no planner left: the
    /// supervisor closes the ask.
    Close,
    /// Its task is in this proposal, sent back for a revise no planner
    /// holds: the answer goes with the revise to the planner opened for it
    /// (ADR-t1704-1 decision 4).
    Revise(ProposalId),
    /// None of these: a person delivers it through the inbox.
    Person,
}

/// The drafts the runtime or a job registered (ADR-0041 decision 16):
/// where each came from, the planner of the runtime's opened for each, and
/// the `planner_question` answers the supervisor types into a planner.
pub trait DraftPlannerStore {
    /// Register every still-unrecorded entry of one receipt atomically,
    /// including skipped entries and their `follow_up_registered` events.
    /// Snapshot the source task's goal and state in the same transaction;
    /// callers cannot supply registration-time goal facts.
    fn register_follow_ups(
        &mut self,
        run: &RunId,
        entries: Vec<FollowUpRegistration>,
        depth: i64,
    ) -> Result<Vec<crate::domain::RegisteredFollowUp>>;
    /// Record where a draft the runtime or a job registered came from, and
    /// what its planner is shown about it. A draft has one origin: a second
    /// call for it is refused.
    fn record_draft_origin(
        &mut self,
        task: TaskId,
        origin: DraftOrigin,
        material: &serde_json::Value,
    ) -> Result<()>;
    /// Where the draft came from, if the runtime or a job registered it.
    fn draft_origin(&self, task: TaskId) -> Result<Option<(DraftOrigin, serde_json::Value)>>;
    /// The drafts waiting for a planner of the runtime's, by ID: `draft`,
    /// in no proposal, with an origin, no planner open for it, no
    /// `planner_question` about it nobody closed, not kept as a draft by
    /// an answer and not exhausted.
    fn planner_drafts(&self) -> Result<Vec<DraftTarget>>;
    /// Record a planner of the runtime's for the bundle of `drafts`
    /// (ADR-t807-1: `draft_bundles`, `draft_bundle_members`, and
    /// `draft_planner_opened` on each) after re-checking them in the same
    /// write transaction: a draft that no longer waits, or whose bundle key
    /// is not the first one's, is left out. With `answer`, the planner
    /// carries that answered `planner_question` about the first draft,
    /// whose planner is gone, instead of that draft being a target.
    fn open_draft_planner(
        &mut self,
        drafts: &[TaskId],
        answer: Option<AskId>,
    ) -> Result<DraftPlannerStart>;
    /// The drafts a planner of the runtime's works on (its bundle's).
    fn planner_draft_tasks(&self, planner: PlannerId) -> Result<Vec<TaskId>>;
    /// The bundle a planner of the runtime's was opened for, if any.
    fn draft_bundle(&self, planner: PlannerId) -> Result<Option<crate::domain::DraftBundleView>>;
    /// Answered `planner_question` asks nobody closed, oldest first.
    fn planner_answers(&self) -> Result<Vec<Ask>>;
    /// Where the answer of an answered `planner_question` goes.
    fn planner_answer_route(&self, ask: &Ask) -> Result<PlannerAnswerRoute>;
    /// The `planner_question`s nobody closed about the tasks of
    /// `proposal`, oldest first (ADR-t1704-1 decision 4).
    fn proposal_questions(&self, proposal: ProposalId) -> Result<Vec<Ask>>;
    /// The planning request whose planners carry the answer of `ask`: the
    /// one it is about, or the one whose planner added the draft it is
    /// about and left it outside any proposal (ADR-t2015-1).
    fn answer_request(&self, ask: &Ask) -> Result<Option<crate::domain::RequestId>>;
    /// Claim the typing of the answer of `ask` into `planner`'s
    /// `workspace` (`planner_answer_claimed`), in one write transaction:
    /// `false` when another process claimed it, the ask was closed, or its
    /// answer no longer goes to that planner. Only the claimer types it.
    fn claim_planner_answer(
        &mut self,
        ask: AskId,
        planner: PlannerId,
        workspace: &str,
    ) -> Result<bool>;
    /// Close an answered `planner_question` nobody needs any more (its
    /// draft moved on), recording `planner_answer_closed` with `why`.
    fn close_planner_answer(&mut self, ask: AskId, why: &str) -> Result<()>;
    /// Record an event of a task that has no run (a draft's).
    fn record_task_event(
        &mut self,
        task: TaskId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()>;
    /// Drafts still `draft` whose planners were used up
    /// (`draft_planner_exhausted`), by ID.
    fn exhausted_drafts(&self) -> Result<Vec<Task>>;
    /// The `follow_up_depth` of a task (ADR-0037 decision 6).
    fn follow_up_depth(&self, task: TaskId) -> Result<i64>;
    fn set_follow_up_depth(&mut self, task: TaskId, depth: i64) -> Result<()>;
    /// The findings waiting for a planner of the runtime's (ADR-0044
    /// decision 19): `open`, marked for a proposal, no planner open for
    /// it, no `planner_question` about it nobody closed and its planners
    /// since the mark not used up; the oldest mark first.
    fn planner_findings(&self) -> Result<Vec<Finding>>;
    /// Record a planner of the runtime's for `finding`
    /// (`finding_planner_opened`) after re-checking it in the same write
    /// transaction. With `answer`, the planner carries that answered
    /// `planner_question` about the finding, whose planner is gone.
    /// Without `answer`, none is opened while the improvements running
    /// reach `limit` (ADR-0051 decision 25).
    fn open_finding_planner(
        &mut self,
        finding: FindingId,
        answer: Option<AskId>,
        limit: usize,
    ) -> Result<FindingPlannerStart>;
    /// The improvement proposals running against `limit`.
    fn improvements(&self, limit: usize) -> Result<crate::domain::ImprovementLimit>;
    /// One finding as `findings ID --full` shows it.
    fn finding_view(&self, finding: FindingId) -> Result<FindingView>;
    /// The asks about the finding, oldest first.
    fn finding_asks(&self, finding: FindingId) -> Result<Vec<Ask>>;
    /// End the `proposed` findings whose proposal ended: `resolved`, or
    /// `open` again without the mark.
    fn settle_findings(&mut self) -> Result<Vec<(FindingId, FindingStatus)>>;
    /// Findings whose planners were used up (`finding_planner_exhausted`)
    /// and that still wait, by ID.
    fn exhausted_findings(&self) -> Result<Vec<Finding>>;
    /// Record an event on the finding's target (on nothing for the queue).
    fn record_finding_event(
        &mut self,
        finding: FindingId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()>;
    /// Whether the answer of `ask` was typed into `workspace`.
    fn ask_delivered_to(&self, ask: AskId, workspace: &str) -> Result<bool>;
    /// When the latest claim of the typing of the answer of `ask` into
    /// `workspace` was taken (`planner_answer_claimed`): a time before the
    /// typing, unlike the ask's close after it. `None` without a claim (an
    /// answer a new planner carried in its prompt).
    fn answer_claimed_at(&self, ask: AskId, workspace: &str) -> Result<Option<i64>>;
    /// Whether sending the answer of `ask` to `planner` ever failed.
    fn ask_delivery_failed(&self, ask: AskId, planner: PlannerId) -> Result<bool>;
}

/// One receipt entry prepared by integrate for atomic registration.
pub struct FollowUpRegistration {
    pub index: usize,
    pub entry: serde_json::Value,
    pub category: String,
    /// The worker's membership proposal as written, or null (ADR-t1504-2
    /// decision 11).
    pub membership_proposal: serde_json::Value,
    pub draft: Option<NewTask>,
    pub skipped: Option<&'static str>,
}

/// A planner of the runtime's that holds a place under
/// `--runtime-planners` while a revise with no planner waits (task 884):
/// its state and why the supervisor does not end it (`busy`, empty when it
/// is about to be asked to exit).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannerHold {
    pub planner_id: PlannerId,
    pub state: String,
    pub busy: Vec<&'static str>,
    pub proposal_id: Option<ProposalId>,
    pub draft_task_id: Option<TaskId>,
    pub finding_id: Option<FindingId>,
    pub request_id: Option<crate::domain::RequestId>,
}

/// A plan review job the queue recorded (ADR-0041 decision 11): the
/// proposal it reviews, its attempt at that proposal, the first task of the
/// proposal (where its events are recorded) and its directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanReviewJob {
    pub id: i64,
    pub proposal_id: ProposalId,
    pub attempt: usize,
    pub anchor: TaskId,
    pub dir: PathBuf,
    /// The job's session id, given to it by the runtime (ADR-0048
    /// decision 4); `None` for a provider that names its session itself
    /// (Codex, whose thread its end records).
    pub session_id: Option<String>,
}

/// What the runtime makes of a plan review's verdict before it is applied:
/// the decision it acts on (a `revise` past [`crate::domain::MAX_PLAN_REVISES`]
/// is a `concern`, `overridden` saying why), the reasons a revise carries to
/// the planner (the verdict's, then the precedents it named), and the
/// `approve_plan` ask a concern opens.
#[derive(Debug, Clone)]
pub struct PlanReviewApply {
    pub verdict: PlanReviewVerdict,
    pub decision: PlanReviewDecision,
    pub overridden: Option<String>,
    pub revise_reasons: Vec<String>,
    pub ask: Option<NewAsk>,
    /// What the runtime made of a `concern` verdict (ADR-t451-1 decision
    /// 4), recorded as `plan_concern_decided`; `None` for any other.
    pub concern: Option<crate::domain::plan_review::PlanConcernDecision>,
    pub duration_secs: u64,
    /// The session the job's output names (Codex's thread and model,
    /// ADR-t1063-1 decision 6); `None` for a Claude job.
    pub session: Option<crate::domain::headless_job::JobSession>,
    /// What the job's prompt took, recorded as `prompt_bytes` (task 1561).
    pub prompt_bytes: Option<crate::application::prompt::PromptBytes>,
}

/// Why a plan review job failed (`plan_review_failed`).
#[derive(Debug, Clone, Default)]
pub struct PlanReviewFailure {
    pub error: String,
    pub duration_secs: u64,
    /// The session the job's output names, as in [`PlanReviewApply`].
    pub session: Option<crate::domain::headless_job::JobSession>,
    /// The job's provider could not be used, and why (ADR-t1063-1
    /// decision 4): its row ends `interrupted` and the proposal is not
    /// held, so it is reviewed again at once, on the other provider unless
    /// both are held.
    pub unusable: Option<(
        crate::domain::Provider,
        crate::domain::provider_switch::SwitchReason,
    )>,
    /// What the job's prompt took, recorded as `prompt_bytes`; `None` when
    /// no prompt was written (task 1561).
    pub prompt_bytes: Option<crate::application::prompt::PromptBytes>,
}

/// A ready task a verdict took back to submitted, with the proposal of its
/// own a planner fixes it in (ADR-0041 decision 14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReopenedTask {
    pub task_id: TaskId,
    pub proposal_id: ProposalId,
}

/// What applying a verdict did. `stale`: nothing, because the job's row or
/// its proposal moved on meanwhile (the job is finished as `interrupted`).
#[derive(Debug, Clone, Default)]
pub struct PlanReviewApplied {
    pub stale: bool,
    pub reopened: Vec<ReopenedTask>,
    /// The `approve_plan` ask a concern opened; `created: false` when an
    /// open one already stood.
    pub ask: Option<AskOutcome>,
}

/// A proposal sent back to its planner (`revising`), with where its revise
/// stands: the reasons, when and to which planner they went (`None`: the
/// supervisor still has to deliver them), and when the inbox was told the
/// planner did not answer.
#[derive(Debug, Clone)]
pub struct RevisingProposal {
    pub proposal: Proposal,
    pub reasons: Vec<String>,
    /// Since when the revise waits (Unix seconds).
    pub revised_at: Option<i64>,
    pub sent_at: Option<i64>,
    pub planner_id: Option<PlannerId>,
    pub unresponsive_at: Option<i64>,
}

/// A proposal that waits for a person outside an ask: its plan review
/// failed (`plan_review_failed`), or its planner did not answer a revise
/// (`planner_unresponsive`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanReviewHold {
    pub proposal_id: ProposalId,
    pub anchor: TaskId,
    pub kind: &'static str,
    pub error: Option<String>,
}

/// What a person's `approve_plan` answer did to the proposal.
#[derive(Debug, Clone)]
pub struct PlanDecided {
    pub proposal: Proposal,
    pub answer: String,
}

/// Plan review (ADR-0041 decisions 11-15, 17): the submitted proposals it
/// takes, its one job at a time, the verdicts and answers the runtime
/// applies (each in one transaction), and the revises it delivers.
pub trait PlanReviewStore {
    /// Close unfinished reviews stopped for an exec handoff, owned only by
    /// `token`, including their session spans. Call before starting jobs.
    fn interrupt_plan_reviews_for_handoff(&mut self, token: &LeaseToken) -> Result<()>;
    /// Submitted proposals plan review may take now: not held, with a
    /// submitted task, and whether one of those has the interrupt priority.
    fn plan_review_candidates(&self) -> Result<Vec<PlanReviewCandidate>>;
    /// Record the start of a plan review of `proposal` by `token`, in the
    /// directory named by its row's ID under `plan_reviews_dir`
    /// (`plan_review_started`, with `cwd`, the checkout the job runs in, for
    /// its Claude session: ADR-0048). Rows of gone supervisors are finished
    /// as `interrupted` first. `None`: another supervisor's job runs, or the
    /// proposal is no longer a candidate. `launch` is what the job is
    /// started with (ADR-0079 decision 7), recorded as the event's `launch`.
    fn begin_plan_review(
        &mut self,
        proposal: ProposalId,
        token: &LeaseToken,
        plan_reviews_dir: &Path,
        cwd: &Path,
        launch: &crate::domain::actor_model::ActorLaunch,
    ) -> Result<Option<PlanReviewJob>>;
    /// Apply what the runtime made of the verdict and finish the job
    /// (`plan_review_finished`); an action the job may not take is an
    /// error, and nothing is applied.
    fn finish_plan_review(
        &mut self,
        job: &PlanReviewJob,
        token: &LeaseToken,
        apply: &PlanReviewApply,
    ) -> Result<PlanReviewApplied>;
    /// Record the job's failure (`plan_review_failed`, the inbox's) and
    /// hold the proposal as `failed`, unless the job moved on or its
    /// provider could not be used ([`PlanReviewFailure::unusable`]).
    fn fail_plan_review(
        &mut self,
        job: &PlanReviewJob,
        token: &LeaseToken,
        failure: &PlanReviewFailure,
    ) -> Result<()>;
    /// Every proposal sent back to its planner, oldest first.
    fn revising_proposals(&self) -> Result<Vec<RevisingProposal>>;
    /// Claim the delivery of the revise of `proposal` (it records when):
    /// `false` when another process claimed it, or it is no longer waiting.
    fn claim_revise(&mut self, proposal: ProposalId) -> Result<bool>;
    /// The claimed revise of `proposal` went to `planner`
    /// (`plan_revise_sent`); `opened` is what the planner's agent started
    /// with when the runtime opened that planner for it (its effort raised
    /// one step, ADR-0079 decision 7 (c)), `None` when it went to a live
    /// planner, whose effort is not raised.
    fn revise_sent(
        &mut self,
        proposal: ProposalId,
        planner: PlannerId,
        workspace: &str,
        opened: Option<&crate::domain::actor_model::ActorLaunch>,
    ) -> Result<()>;
    /// Take the revise of `proposal` back for another delivery: the
    /// planner it went to is gone before it submitted again
    /// (`plan_revise_lost`), or the delivery failed (`planner` `None`).
    fn revise_lost(
        &mut self,
        proposal: ProposalId,
        planner: Option<PlannerId>,
        why: &str,
    ) -> Result<()>;
    /// No planner submitted the proposal again within the timeout
    /// (`planner_unresponsive`, the inbox's), once per revise; `planner`
    /// is `None` when none took the revise yet, and `holders` are then the
    /// runtime's planners that fill the limit it waits on (task 884).
    fn planner_unresponsive(
        &mut self,
        proposal: ProposalId,
        planner: Option<PlannerId>,
        waited_secs: i64,
        holders: &[PlannerHold],
    ) -> Result<()>;
    /// End the submitted proposals none of whose tasks waits for plan
    /// review any more (a person readied them with the bypass or canceled
    /// them): `accepted` when a task is left, `canceled` otherwise
    /// (`proposal_settled`).
    fn settle_proposals(&mut self) -> Result<Vec<(ProposalId, crate::domain::ProposalStatus)>>;
    /// Answered `approve_plan` asks nobody closed, oldest first.
    fn plan_answers(&self) -> Result<Vec<Ask>>;
    /// Apply a person's answer to an `approve_plan` ask and close it.
    /// `None` when the answer is not one the runtime applies (left to the
    /// inbox), or the proposal no longer waits for it (the ask is closed).
    fn decide_plan(&mut self, ask: AskId) -> Result<Option<PlanDecided>>;
    /// Whether the supervisor applies the answer the `approve_plan` ask has.
    fn applies_plan_answer(&self, ask: &Ask) -> Result<bool>;
    /// The proposals held for a person outside an ask.
    fn plan_review_holds(&self) -> Result<Vec<PlanReviewHold>>;
    /// The tasks of closed goals left unfinished that other tasks wait on
    /// (task 421), held for a person outside an ask.
    fn stranded_dependencies(&self) -> Result<Vec<StrandedDependency>>;
    /// Asks a person answered, newest first, at most `limit`: the
    /// precedents plan review may cite.
    fn answered_asks(&self, limit: usize) -> Result<Vec<Ask>>;
}

/// A goal review job the queue recorded (ADR-0047 decision 43): the goal
/// it reviews, its attempt at that goal, the goal's first task (where its
/// `approve_goal` ask is), its directory, and how many `gaps` verdicts the
/// goal got in a row before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GoalReviewJob {
    pub id: i64,
    pub goal_id: GoalId,
    pub attempt: usize,
    pub anchor: TaskId,
    pub dir: PathBuf,
    pub gaps_in_a_row: usize,
    /// The job's session id, given to it by the runtime (ADR-0048
    /// decision 4); `None` for a provider that names its session itself
    /// (Codex, whose thread its end records).
    pub session_id: Option<String>,
}

/// A finished goal review of a goal, for the next one's prompt.
#[derive(Debug, Clone, Serialize)]
pub struct GoalReviewRecord {
    pub id: i64,
    pub attempt: usize,
    pub outcome: String,
    pub verdict: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// What the runtime makes of a goal review's verdict before it is applied:
/// the decision it acts on (a `gaps` past
/// [`crate::domain::goal_review::MAX_GOAL_GAPS`] in a row is an `ask`,
/// `overridden` saying why) and the `approve_goal` ask an `ask` opens.
#[derive(Debug, Clone)]
pub struct GoalReviewApply {
    pub verdict: GoalReviewVerdict,
    pub decision: GoalReviewDecision,
    pub overridden: Option<String>,
    pub ask: Option<NewAsk>,
    pub duration_secs: u64,
    /// The session the job's output names (Codex's thread and model,
    /// ADR-t1063-1 decision 6); `None` for a Claude job.
    pub session: Option<crate::domain::headless_job::JobSession>,
    /// What the job's prompt took (task 1571), recorded as
    /// `goal_review_finished`'s `prompt_bytes`.
    pub prompt_bytes: Option<crate::application::prompt::PromptBytes>,
}

/// Why a goal review job failed (`goal_review_failed`).
#[derive(Debug, Clone, Default)]
pub struct GoalReviewFailure {
    pub error: String,
    pub duration_secs: u64,
    /// The session the job's output names, as in [`GoalReviewApply`].
    pub session: Option<crate::domain::headless_job::JobSession>,
    /// The job's provider could not be used, and why (ADR-t1063-1
    /// decision 4): its row ends `interrupted` rather than `failed`, so
    /// the goal is reviewed again at once, on the other provider unless
    /// both are held.
    pub unusable: Option<(
        crate::domain::Provider,
        crate::domain::provider_switch::SwitchReason,
    )>,
    /// What the job's prompt took, when it was written (task 1571):
    /// `goal_review_failed`'s `prompt_bytes`.
    pub prompt_bytes: Option<crate::application::prompt::PromptBytes>,
}

/// What applying a goal review's verdict did. `stale`: nothing, because
/// the job's row moved on or the goal's tasks changed meanwhile.
#[derive(Debug, Clone, Default)]
pub struct GoalReviewApplied {
    pub stale: bool,
    pub closed: bool,
    /// The drafts a `gaps` verdict registered.
    pub gap_tasks: Vec<TaskId>,
    pub ask: Option<AskOutcome>,
}

/// A goal whose goal review failed and that waits for a person
/// (`goal_review_failed`) until its tasks change or `goal review ID`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalReviewHold {
    pub goal_id: GoalId,
    pub anchor: Option<TaskId>,
    pub error: Option<String>,
}

/// What a person's `approve_goal` answer did to the goal.
#[derive(Debug, Clone)]
pub struct GoalDecided {
    pub goal_id: GoalId,
    pub answer: String,
    pub closed: Option<GoalVerdict>,
    pub gap_tasks: Vec<TaskId>,
}

/// Goal review (ADR-0047 decision 43): the open goals whose tasks all
/// ended, its one job at a time, the verdicts and answers the runtime
/// applies (each in one transaction).
pub trait GoalReviewStore {
    /// Close unfinished reviews stopped for an exec handoff, owned only by
    /// `token`, including their session spans. Call before starting jobs.
    fn interrupt_goal_reviews_for_handoff(&mut self, token: &LeaseToken) -> Result<()>;
    /// Open goals a goal review may take now, in ID order: at least one
    /// task, each completed or canceled with one completed, no unclosed
    /// `approve_goal` ask, and tasks that changed since the goal's last
    /// review (or a person rearmed it).
    fn goal_review_candidates(&self) -> Result<Vec<GoalId>>;
    /// Record the start of a goal review of `goal` by `token`, in the
    /// directory named by its row's ID under `goal_reviews_dir`
    /// (`goal_review_started`, with the session id it gives the job, the
    /// checkout `cwd` it runs in and its `launch`). Rows of gone
    /// supervisors are finished as `interrupted` first. `None`: another
    /// supervisor's job runs, or the goal is no longer a candidate.
    fn begin_goal_review(
        &mut self,
        goal: GoalId,
        token: &LeaseToken,
        goal_reviews_dir: &Path,
        cwd: &Path,
        launch: &crate::domain::actor_model::ActorLaunch,
    ) -> Result<Option<GoalReviewJob>>;
    /// The finished reviews of `goal`, oldest first.
    fn goal_reviews(&self, goal: GoalId) -> Result<Vec<GoalReviewRecord>>;
    /// Apply the verdict in one transaction (`goal_review_finished`).
    fn finish_goal_review(
        &mut self,
        job: &GoalReviewJob,
        token: &LeaseToken,
        apply: &GoalReviewApply,
    ) -> Result<GoalReviewApplied>;
    /// Record the job's failure (`goal_review_failed`, the inbox's); the
    /// goal is not reviewed again until its tasks change or a person
    /// rearms it, unless its provider could not be used
    /// ([`GoalReviewFailure::unusable`]).
    fn fail_goal_review(
        &mut self,
        job: &GoalReviewJob,
        token: &LeaseToken,
        failure: &GoalReviewFailure,
    ) -> Result<()>;
    /// Answered `approve_goal` asks nobody closed whose answer the runtime
    /// took to apply when it was given (`runtime_delivers`), oldest first;
    /// an answer left to the inbox is never applied later by itself.
    fn goal_answers(&self) -> Result<Vec<Ask>>;
    /// Apply a person's answer to an `approve_goal` ask and close it.
    /// `None` when the answer is not one the runtime applies now (left
    /// open for the inbox), or the goal no longer waits for it (the ask is
    /// closed).
    fn decide_goal(&mut self, ask: AskId) -> Result<Option<GoalDecided>>;
    /// Answered `correct_goal` asks (ADR-t1504-2 decision 9) nobody closed
    /// whose answer the runtime took to apply when it was given
    /// (`runtime_delivers`), oldest first.
    fn correction_answers(&self) -> Result<Vec<Ask>>;
    /// Apply a person's answer to a `correct_goal` ask and close it
    /// (`goal_correction_decided`, and `goal_reopened` for `reopen`); the
    /// event's payload, or `None` when the answer is not one the runtime
    /// applies now (left open for the inbox) or the goal is no longer
    /// closed as achieved (the ask is closed).
    fn decide_correction(&mut self, ask: AskId) -> Result<Option<serde_json::Value>>;
    /// The goals whose review failed, held for a person.
    fn goal_review_holds(&self) -> Result<Vec<GoalReviewHold>>;
    /// Each open goal with a follow-up whose source it is that has no
    /// settled membership, with its tasks and those follow-ups and what
    /// already shows or handles each (task 1660), in goal order.
    fn goal_follow_ups(&self) -> Result<Vec<crate::domain::follow_up::GoalFollowUps>>;
    /// `goal review ID`: let the supervisor review the open goal again
    /// although its tasks did not change (`goal_review_rearmed`).
    fn rearm_goal_review(&mut self, goal: GoalId) -> Result<serde_json::Value>;
}
