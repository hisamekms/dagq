//! The task aggregate: its state, the rules that create and restore it, and
//! the commands and queries on it. The fields are private, so a task changes
//! only through the functions here; the store saves what they return.

use serde::Serialize;

use super::{
    DomainError, EvidenceCheck, GoalId, NewTask, Priority, PrioritySource, Provider, TaskChange,
    TaskEdit, TaskId, TaskRecord, TaskStatus, base_priority,
    plan_request::{PriorityBy, RecordedOrigin},
    require,
    scope::{dedup_globs, validate_path_globs},
    worker::{Worker, WorkerMode},
};

/// What moves a task between statuses by hand or by plan review; only a
/// claim marks it in progress and only a landing completes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskAction {
    /// A retry: an in-progress task whose runs all failed or were
    /// interrupted goes back to ready unchanged, which needs no plan review
    /// (ADR-0041 decision 8). A draft or submitted task is refused: plan
    /// review ([`Self::Approve`]) or a person's bypass readies it.
    Ready,
    Draft,
    Cancel,
    /// `dagq submit`: a draft enters plan review in a proposal.
    Submit,
    /// Plan review passed the task's proposal: the one path to ready for a
    /// submitted task besides the bypass.
    Approve,
    /// `ready --bypass-review`: a person skips plan review.
    BypassReview,
    /// Plan review found that a ready task has to change (ADR-0041
    /// decision 14): it goes back to submitted, out of the claim.
    Reopen,
}

impl TaskStatus {
    /// `unfinished_run` is whether the task still owns a run that is executing,
    /// awaiting or undergoing integration, or waiting for a session. An in-progress task whose runs have all failed or
    /// been interrupted may be retried or canceled by hand; a retry is a new run.
    pub fn transition(self, action: TaskAction, unfinished_run: bool) -> Result<Self, DomainError> {
        match (self, action) {
            (Self::Draft, TaskAction::Submit) => Ok(Self::Submitted),
            (Self::Ready, TaskAction::Reopen) => Ok(Self::Submitted),
            (Self::Submitted, TaskAction::Approve)
            | (Self::Draft | Self::Submitted, TaskAction::BypassReview) => Ok(Self::Ready),
            (Self::Draft | Self::Submitted, TaskAction::Ready) => {
                Err(DomainError::ReadyNeedsPlanReview { status: self })
            }
            (Self::Ready | Self::Submitted, TaskAction::Draft) => Ok(Self::Draft),
            (Self::Draft | Self::Submitted | Self::Ready, TaskAction::Cancel) => Ok(Self::Canceled),
            (Self::InProgress, _) if unfinished_run => {
                Err(DomainError::TaskHasUnfinishedRun { action })
            }
            (Self::InProgress, TaskAction::Ready) => Ok(Self::Ready),
            (Self::InProgress, TaskAction::Draft) => Ok(Self::Draft),
            (Self::InProgress, TaskAction::Cancel) => Ok(Self::Canceled),
            _ => Err(DomainError::TransitionNotAllowed {
                status: self,
                action,
            }),
        }
    }

    /// Dependencies, the goal and the paths may change only before the
    /// task is claimed; plan review adds dependencies and lowers priorities
    /// of submitted tasks (ADR-0041 decision 11).
    pub fn dependencies_editable(self) -> bool {
        matches!(self, Self::Draft | Self::Submitted | Self::Ready)
    }

    /// The priority changes before the task is claimed and while it is in
    /// progress, where it orders the next resume and recovery job of its
    /// run (ADR-t1850-1 decision 7); a finished task keeps it.
    pub fn priority_editable(self) -> bool {
        self.dependencies_editable() || self == Self::InProgress
    }

    /// Whether `dagq edit` may change the content of the task: a draft or a
    /// submitted task (ADR-0041 decision 9). A ready task goes back to
    /// submitted to be edited (decision 14).
    pub fn content_editable(self) -> bool {
        matches!(self, Self::Draft | Self::Submitted)
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Canceled)
    }
}

/// A unit of work the queue runs. `Serialize` is the JSON the CLI prints;
/// there is no `Deserialize`: a task is built by [`Task::new`] or
/// [`Task::restore`] only.
#[derive(Debug, Clone, Serialize)]
pub struct Task {
    id: TaskId,
    title: String,
    description: String,
    acceptance: String,
    verification_commands: Vec<String>,
    /// Receipt checks validation requires to be `passed` with evidence
    /// (ADR-0019 decision 5); a receipt without them parks the run as
    /// `needs_session`.
    required_evidence: Vec<EvidenceCheck>,
    /// Globs of the paths a run may change (ADR-0029): validation and
    /// `integrate` refuse a diff with a path none of them matches. Empty:
    /// no limit.
    paths: Vec<String>,
    /// How urgently a person wants it claimed (ADR-0040 decision 4): its
    /// own setting, else its goal's (ADR-t1639-1 decision 2). The claim
    /// order raises it to the effective priority. Without a setting of its
    /// own it follows the goal's current priority in every status, read
    /// each time and never frozen at the claim (ADR-t1811-1 decision 1);
    /// the priority a run was claimed at is in its `run_claimed` event.
    priority: Priority,
    /// Where `priority` comes from: `task`, `goal` or `default`.
    priority_source: PrioritySource,
    /// Who set `priority`, its own's setter or its goal's (ADR-t1975-1
    /// decision 2): `human` for a person's.
    priority_by: PriorityBy,
    /// The task's own setting; none inherits `goal_priority`. Not shown.
    #[serde(skip)]
    own_priority: Option<Priority>,
    /// Who set its own priority; none without one, or when nobody can
    /// tell (read as a person's). Not shown.
    #[serde(skip)]
    own_priority_by: Option<PriorityBy>,
    /// The priority of the goal it belongs to, as read with it. Not shown.
    #[serde(skip)]
    goal_priority: Option<Priority>,
    /// Who set its goal's priority, as read with it. Not shown.
    #[serde(skip)]
    goal_priority_by: Option<PriorityBy>,
    /// Where it comes from, recorded at its creation (ADR-t1975-1
    /// decision 5): shown as `origin`, `origin_kind`, `origin_request_id`.
    #[serde(flatten)]
    origin: RecordedOrigin,
    /// The kind of change it makes, as the registrant declared it
    /// (ADR-t980-1); null for a task registered without one.
    change: Option<TaskChange>,
    /// Claimed only once the supervisor's own build contains the landed
    /// commits of every task it depends on (ADR-t1632-1); shown only when
    /// declared.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    wait_for_build: bool,
    /// The provider and mode its worker runs on (ADR-t813-2 decision 1,
    /// ADR-t1340-1): Claude headless unless it asks for another; shown as
    /// `provider` and `worker_mode`, resolved.
    #[serde(flatten)]
    worker: Worker,
    /// The mode the task names (`--headless`, or `interactive` recorded
    /// before ADR-t1433-2); none
    /// leaves it to the provider's default, so a task registered without
    /// one follows a change of the default (ADR-t1340-1). Not shown.
    #[serde(skip)]
    named_mode: Option<WorkerMode>,
    /// Only a claim makes a task `in_progress`, and only the landing of its
    /// run makes it `completed`; a terminal status never changes.
    status: TaskStatus,
    /// The one goal it belongs to, if any. A closed goal takes no new tasks.
    goal_id: Option<GoalId>,
    /// Why the task exists and what to read first; the worker's prompt
    /// carries it.
    context: String,
    created_at: String,
    updated_at: String,
}

impl Task {
    /// A task registered now as `id`: the creation rules of [`NewTask`]
    /// hold, it starts as a draft, and its required checks and path globs
    /// are kept once each. `new.dependencies` are edges the store records
    /// next to the task; they are not part of it.
    pub fn new(id: TaskId, new: NewTask, created_at: String) -> Result<Self, DomainError> {
        new.validate()?;
        require_positive(id)?;
        let (priority, priority_source) = base_priority(new.priority, None);
        Ok(Self {
            id,
            worker: new.worker()?,
            named_mode: new.worker_mode,
            required_evidence: new.required_evidence(),
            paths: dedup_globs(&new.paths),
            priority,
            priority_source,
            priority_by: PriorityBy::effective(new.priority, None, None),
            own_priority: new.priority,
            own_priority_by: None,
            goal_priority: None,
            goal_priority_by: None,
            origin: RecordedOrigin::UNKNOWN,
            change: new.change,
            wait_for_build: new.wait_for_build,
            title: new.title,
            description: new.description,
            acceptance: new.acceptance,
            verification_commands: new.verification_commands,
            status: TaskStatus::Draft,
            goal_id: new.goal_id,
            context: new.context,
            updated_at: created_at.clone(),
            created_at,
        })
    }

    /// A stored task as it was saved. Only what every stored task satisfies
    /// is checked (a positive ID and goal ID, a title that is not blank); the
    /// creation rules are not applied again.
    pub fn restore(record: TaskRecord) -> Result<Self, DomainError> {
        require_positive(record.id)?;
        require(!record.title.trim().is_empty(), || DomainError::Blank {
            field: "task title",
        })?;
        require(record.goal_id.is_none_or(|id| id.as_i64() > 0), || {
            DomainError::NonPositiveId { field: "goal ID" }
        })?;
        let (priority, priority_source) = base_priority(record.priority, record.goal_priority);
        Ok(Self {
            id: record.id,
            title: record.title,
            description: record.description,
            acceptance: record.acceptance,
            verification_commands: record.verification_commands,
            required_evidence: record.required_evidence,
            paths: record.paths,
            priority,
            priority_source,
            priority_by: PriorityBy::effective(record.priority, None, None),
            own_priority: record.priority,
            own_priority_by: None,
            goal_priority: record.goal_priority,
            goal_priority_by: None,
            origin: RecordedOrigin::UNKNOWN,
            change: record.change,
            wait_for_build: record.wait_for_build,
            worker: record.worker,
            named_mode: record.named_mode,
            status: record.status,
            goal_id: record.goal_id,
            context: record.context,
            created_at: record.created_at,
            updated_at: record.updated_at,
        })
    }

    /// The task with what the queue records beside it: who set its own
    /// priority and its goal's, and its origin.
    pub fn with_record(
        mut self,
        own_priority_by: Option<PriorityBy>,
        goal_priority_by: Option<PriorityBy>,
        origin: RecordedOrigin,
    ) -> Self {
        self.own_priority_by = self.own_priority.and(own_priority_by);
        self.goal_priority_by = self.goal_priority.and(goal_priority_by);
        self.priority_by =
            PriorityBy::effective(self.own_priority, own_priority_by, goal_priority_by);
        self.origin = origin;
        self
    }

    pub fn id(&self) -> TaskId {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn acceptance(&self) -> &str {
        &self.acceptance
    }

    pub fn verification_commands(&self) -> &[String] {
        &self.verification_commands
    }

    pub fn required_evidence(&self) -> &[EvidenceCheck] {
        &self.required_evidence
    }

    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    /// The base priority: its own, else its goal's, else `normal`.
    pub fn priority(&self) -> Priority {
        self.priority
    }

    pub fn priority_source(&self) -> PrioritySource {
        self.priority_source
    }

    /// Who set [`Self::priority`] (ADR-t1975-1 decision 2).
    pub fn priority_by(&self) -> PriorityBy {
        self.priority_by
    }

    /// Who set its own priority, none without one.
    pub fn own_priority_by(&self) -> Option<PriorityBy> {
        self.own_priority
            .map(|_| self.own_priority_by.unwrap_or(PriorityBy::Human))
    }

    /// Where it comes from (ADR-t1975-1 decision 5).
    pub fn origin(&self) -> RecordedOrigin {
        self.origin
    }

    /// The task's own setting, which the store keeps; none inherits.
    pub fn own_priority(&self) -> Option<Priority> {
        self.own_priority
    }

    /// Its goal's priority, which it takes without one of its own; none
    /// without a goal.
    pub fn goal_priority(&self) -> Option<Priority> {
        self.goal_priority
    }

    pub fn change(&self) -> Option<&TaskChange> {
        self.change.as_ref()
    }

    /// Whether its claim waits for a build that contains the landings of
    /// its dependencies (ADR-t1632-1).
    pub fn wait_for_build(&self) -> bool {
        self.wait_for_build
    }

    pub fn worker(&self) -> Worker {
        self.worker
    }

    /// The mode the store keeps for the task: the one it names, or none
    /// for the provider's default. A Codex task keeps `headless`, its only
    /// mode, as it always has.
    pub fn stored_worker_mode(&self) -> Option<WorkerMode> {
        match self.worker.provider {
            Provider::Claude => self.named_mode,
            Provider::Codex => Some(self.worker.mode),
        }
    }

    pub fn status(&self) -> TaskStatus {
        self.status
    }

    pub fn goal_id(&self) -> Option<GoalId> {
        self.goal_id
    }

    pub fn context(&self) -> &str {
        &self.context
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    pub fn updated_at(&self) -> &str {
        &self.updated_at
    }

    /// The title, consuming the task.
    pub fn into_title(self) -> String {
        self.title
    }
}

fn require_positive(id: TaskId) -> Result<(), DomainError> {
    require(id.as_i64() > 0, || DomainError::NonPositiveId {
        field: "task ID",
    })
}

/// Whether the task's dependencies, goal and paths may still change: only
/// before it is claimed.
pub fn dependencies_editable(task: &Task) -> bool {
    task.status.dependencies_editable()
}

/// Rejects a change to `what` of a task that is already claimed or finished.
fn require_editable(task: &Task, what: &'static str) -> Result<(), DomainError> {
    require(dependencies_editable(task), || {
        DomainError::TaskNotEditable { what }
    })
}

/// Apply a user's `action` to `task`; `unfinished_run` is whether it still
/// owns an unfinished run (see [`TaskStatus::transition`]).
pub fn transition(
    mut task: Task,
    action: TaskAction,
    unfinished_run: bool,
) -> Result<Task, DomainError> {
    task.status = task.status.transition(action, unfinished_run)?;
    Ok(task)
}

/// A supervisor takes `task` for a new run: only a ready task is claimed,
/// and it stays `in_progress` until its run lands or a person moves it.
pub fn claim(mut task: Task) -> Result<Task, DomainError> {
    require(task.status == TaskStatus::Ready, || {
        DomainError::TaskNotClaimable {
            task_id: task.id,
            status: task.status,
        }
    })?;
    task.status = TaskStatus::InProgress;
    Ok(task)
}

/// The goal a task moves to, as the move reads it: its ID, its priority
/// and who set that priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Destination {
    pub id: GoalId,
    pub priority: Priority,
    pub priority_by: PriorityBy,
}

impl Destination {
    pub fn of(goal: &super::Goal) -> Self {
        Self {
            id: goal.id(),
            priority: goal.priority(),
            priority_by: goal.priority_by(),
        }
    }
}

/// Who moves a task between goals, for what becomes of a person's priority
/// it takes from its goal (ADR-t1975-1 decision 4): a person (the user,
/// the inbox, a person's answer the runtime applies) or the AI (a planner,
/// the runtime on its own).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovedBy {
    Person,
    Ai,
}

impl MovedBy {
    /// Who `role` moves a task as.
    pub const fn of(role: super::ActorRole) -> Self {
        if super::authorization::changes_persons_priority(role) {
            Self::Person
        } else {
            Self::Ai
        }
    }
}

/// The priority a move of `task` to `to` (none: out of any goal, which
/// gives `normal`) by `by` keeps as the task's own and a person's
/// (ADR-t1975-1 decision 4), none when the task takes its new goal's as it
/// always did: a person's move, a task with a priority of its own (which no
/// move changes), a task whose priority no person set, and a move to a
/// goal whose priority is the same value set by a person, which keeps the
/// same protection. A goal of the same value the AI set does not: the AI
/// could then lower the goal, or move the task again.
pub fn priority_kept_on_move(
    task: &Task,
    to: Option<Destination>,
    by: MovedBy,
) -> Option<Priority> {
    let inherits = by == MovedBy::Person
        || task.own_priority.is_some()
        || task.priority_by != PriorityBy::Human
        || to.is_some_and(|goal| {
            (goal.priority, goal.priority_by) == (task.priority, PriorityBy::Human)
        });
    (!inherits).then_some(task.priority)
}

/// Move `task` to `to`, or out of any goal, as `by` moves it: a person's
/// priority it took from its old goal stays its own as a person's when
/// [`priority_kept_on_move`] says so, and otherwise it takes its new
/// goal's. Whether the goal takes tasks is
/// [`super::goal::check_accepts_tasks`], which the caller applies to the
/// goal it reads.
pub fn set_goal(mut task: Task, to: Option<Destination>, by: MovedBy) -> Result<Task, DomainError> {
    require_editable(&task, "the goal")?;
    if task.goal_id == to.map(|goal| goal.id) {
        return Ok(task);
    }
    if let Some(kept) = priority_kept_on_move(&task, to, by) {
        task.own_priority = Some(kept);
        task.own_priority_by = Some(PriorityBy::Human);
    }
    task.goal_id = to.map(|goal| goal.id);
    task.goal_priority = to.map(|goal| goal.priority);
    task.goal_priority_by = to.map(|goal| goal.priority_by);
    (task.priority, task.priority_source) = base_priority(task.own_priority, task.goal_priority);
    task.priority_by = PriorityBy::effective(
        task.own_priority,
        task.own_priority_by,
        task.goal_priority_by,
    );
    Ok(task)
}

/// Replace the path globs `task` may change (ADR-0029), each kept once.
pub fn set_paths(mut task: Task, paths: Vec<String>) -> Result<Task, DomainError> {
    validate_path_globs(&paths)?;
    require_editable(&task, "the paths")?;
    task.paths = dedup_globs(&paths);
    Ok(task)
}

/// Give `task` a priority of its own, or with none take its goal's again
/// (ADR-0040 decision 4, ADR-t1639-1 decision 2): before it is claimed, or
/// while it is in progress (ADR-t1850-1 decision 7), where it orders only
/// the next resume and recovery job, so a running run is never preempted.
pub fn set_priority(mut task: Task, priority: Option<Priority>) -> Result<Task, DomainError> {
    require(task.status.priority_editable(), || {
        DomainError::TaskPriorityNotEditable {
            task_id: task.id,
            status: task.status,
        }
    })?;
    task.own_priority = priority;
    (task.priority, task.priority_source) = base_priority(priority, task.goal_priority);
    Ok(task)
}

/// `task` with the fields of `edit` replaced (`dagq edit`): only while its
/// status keeps the content editable, with the creation rules of
/// [`NewTask`] for what changes; the required checks and globs are kept
/// once each.
pub fn edit(mut task: Task, edit: TaskEdit) -> Result<Task, DomainError> {
    edit.validate()?;
    require(task.status.content_editable(), || {
        DomainError::TaskContentNotEditable {
            task_id: task.id,
            status: task.status,
        }
    })?;
    if let Some(title) = edit.title {
        task.title = title;
    }
    if let Some(description) = edit.description {
        task.description = description;
    }
    if let Some(acceptance) = edit.acceptance {
        task.acceptance = acceptance;
    }
    if let Some(commands) = edit.verification_commands {
        task.verification_commands = commands;
    }
    if let Some(checks) = edit.required_evidence {
        task.required_evidence = Vec::new();
        for check in checks {
            if !task.required_evidence.contains(&check) {
                task.required_evidence.push(check);
            }
        }
    }
    if let Some(paths) = edit.paths {
        task.paths = dedup_globs(&paths);
    }
    if let Some(context) = edit.context {
        task.context = context;
    }
    if let Some(change) = edit.change {
        task.change = Some(change);
    }
    if let Some(wait) = edit.wait_for_build {
        task.wait_for_build = wait;
    }
    task.worker = task.worker.with(edit.provider, edit.worker_mode)?;
    // A new provider without a mode goes back to its default; a mode given
    // is named. Neither keeps what the task named.
    match (edit.provider, edit.worker_mode) {
        (_, Some(mode)) => task.named_mode = Some(mode),
        (Some(_), None) => task.named_mode = None,
        (None, None) => {}
    }
    Ok(task)
}

/// Replace only verification commands after a run has ended. The store
/// checks the run history in the same transaction before calling this.
pub fn edit_ended_verify(mut task: Task, edit: TaskEdit) -> Result<Task, DomainError> {
    edit.validate()?;
    require(
        task.status == TaskStatus::InProgress && edit.verify_only(),
        || DomainError::TaskContentNotEditable {
            task_id: task.id,
            status: task.status,
        },
    )?;
    task.verification_commands = edit
        .verification_commands
        .expect("verify_only has commands");
    Ok(task)
}

/// A planning command's change of `task` (`dagq edit`, `set-goal`,
/// `set-paths`, `set-priority`, `draft`, `ready`, `cancel`, `dependency`)
/// goes on only while it has the status the command was authorized with:
/// what the actor may do (`task.write`, or `task.verify_edit` on an
/// `in_progress` task) was decided from that status, and the store reads it
/// again in its transaction (task 1609).
pub fn check_status_authorized(task: &Task, authorized: TaskStatus) -> Result<(), DomainError> {
    require(task.status == authorized, || {
        DomainError::TaskStatusChangedSinceAuthorized {
            task_id: task.id,
            authorized,
            status: task.status,
        }
    })
}

/// A task never depends on itself; checked before either task is read.
pub fn check_not_self(task_id: TaskId, predecessor_id: TaskId) -> Result<(), DomainError> {
    require(task_id != predecessor_id, || DomainError::SelfDependency)
}

/// Why `task_id` may not be canceled as a duplicate of `duplicate_of`
/// (ADR-0046 decision 5), `None` when it may: not of itself, and of a task
/// that exists and is not canceled. `target` is `duplicate_of`'s status
/// and, when a cancel made it a duplicate, its original, which the refusal
/// names; `None` when it does not exist.
pub fn duplicate_refusal(
    task_id: TaskId,
    duplicate_of: TaskId,
    target: Option<(TaskStatus, Option<TaskId>)>,
) -> Option<String> {
    if task_id == duplicate_of {
        return Some(format!("task {task_id} cannot be a duplicate of itself"));
    }
    match target {
        None => Some(format!("task {duplicate_of} does not exist")),
        Some((TaskStatus::Canceled, Some(original))) => Some(format!(
            "task {duplicate_of} is canceled as a duplicate of task {original}; pass --duplicate-of {original}"
        )),
        Some((TaskStatus::Canceled, None)) => Some(format!(
            "task {duplicate_of} is canceled; a duplicate needs a task that is not"
        )),
        Some(_) => None,
    }
}

/// Whether `task` may gain or lose a predecessor.
pub fn check_dependencies_editable(task: &Task) -> Result<(), DomainError> {
    require_editable(task, "dependencies")
}

/// `creates_cycle` is whether `predecessor_id` already depends on `task_id`,
/// directly or not; the store finds it over the whole dependency graph,
/// where a task also waits for the goals it depends on and a goal for its
/// tasks (ADR-0038).
pub fn check_acyclic(
    task_id: TaskId,
    predecessor_id: TaskId,
    creates_cycle: bool,
) -> Result<(), DomainError> {
    require(!creates_cycle, || DomainError::DependencyCycle {
        task_id,
        predecessor_id,
    })
}

/// A task never depends on the goal it belongs to: the goal already waits
/// for it.
pub fn check_not_own_goal(task: &Task, goal_id: GoalId) -> Result<(), DomainError> {
    require(task.goal_id != Some(goal_id), || {
        DomainError::OwnGoalDependency { goal_id }
    })
}

/// `creates_cycle` is whether `goal_id` already waits for `task_id`,
/// directly or not, over the same graph as [`check_acyclic`].
pub fn check_goal_acyclic(
    task_id: TaskId,
    goal_id: GoalId,
    creates_cycle: bool,
) -> Result<(), DomainError> {
    require(!creates_cycle, || DomainError::GoalDependencyCycle {
        task_id,
        goal_id,
    })
}

/// Whether `task`, moved to `goal_id`, keeps the graph acyclic:
/// `depends_on_goal` is whether it depends on that goal directly, and
/// `creates_cycle` whether it waits for the goal at all (the store finds it
/// over the whole graph). A direct dependency is a dependency on its own
/// goal; an indirect one a cycle through the goal's membership.
pub fn check_membership_acyclic(
    task: &Task,
    goal_id: GoalId,
    depends_on_goal: bool,
    creates_cycle: bool,
) -> Result<(), DomainError> {
    require(!depends_on_goal, || DomainError::OwnGoalDependency {
        goal_id,
    })?;
    require(!creates_cycle, || DomainError::GoalMembershipCycle {
        task_id: task.id,
        goal_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_task() -> NewTask {
        NewTask {
            title: "t".into(),
            description: "d".into(),
            acceptance: "a".into(),
            verification_commands: vec!["cargo test".into()],
            required_evidence: vec![EvidenceCheck::E2e, EvidenceCheck::E2e],
            paths: vec!["docs/**".into(), "docs/**".into()],
            priority: Default::default(),
            change: None,
            dependencies: vec![TaskId::new(1)],
            goal_dependencies: Vec::new(),
            goal_id: Some(GoalId::new(2)),
            context: "c".into(),
            provider: None,
            worker_mode: None,
            wait_for_build: false,
        }
    }

    fn record(status: TaskStatus) -> TaskRecord {
        TaskRecord {
            id: TaskId::new(5),
            title: "t".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: None,
            goal_priority: None,
            change: None,
            status,
            goal_id: None,
            context: String::new(),
            created_at: "c".into(),
            updated_at: "u".into(),
            worker: crate::domain::worker::Worker::CLAUDE_INTERACTIVE,
            named_mode: None,
            wait_for_build: false,
        }
    }

    fn goal(id: i64, priority: Priority, priority_by: PriorityBy) -> Destination {
        Destination {
            id: GoalId::new(id),
            priority,
            priority_by,
        }
    }

    /// A ready task as the store reads it: its own priority and who set
    /// it, and its goal's.
    fn read(own: Option<(Priority, PriorityBy)>, in_goal: Option<Destination>) -> Task {
        Task::restore(TaskRecord {
            priority: own.map(|(priority, _)| priority),
            goal_priority: in_goal.map(|goal| goal.priority),
            goal_id: in_goal.map(|goal| goal.id),
            ..record(TaskStatus::Ready)
        })
        .unwrap()
        .with_record(
            own.map(|(_, by)| by),
            in_goal.map(|goal| goal.priority_by),
            RecordedOrigin::UNKNOWN,
        )
    }

    /// The task read again after its goal changed to `in_goal`.
    fn reread(task: &Task, in_goal: Option<Destination>) -> Task {
        read(
            task.own_priority().zip(task.own_priority_by()),
            in_goal.filter(|goal| task.goal_id() == Some(goal.id)),
        )
    }

    fn state(task: &Task) -> (Priority, PrioritySource, PriorityBy, Option<GoalId>) {
        (
            task.priority(),
            task.priority_source(),
            task.priority_by(),
            task.goal_id(),
        )
    }

    /// ADR-t1975-1 decision 4: the AI's move of a task that takes a
    /// person's priority from its goal keeps that value as the task's own
    /// and a person's, unless the new goal has the same value as a
    /// person's; a person's move, a task with its own priority and a task
    /// whose priority the AI set take the new goal's as always.
    #[test]
    fn the_ais_move_keeps_a_persons_priority_the_task_took_from_its_goal() {
        use PriorityBy::{Ai, Human};
        use PrioritySource as Src;
        let persons = goal(1, Priority::Interrupt, Human);
        let low = goal(2, Priority::Low, Ai);
        let id = |n| Some(GoalId::new(n));
        // To a lower goal, and out of any goal (which gives normal).
        for to in [Some(low), None] {
            let moved = set_goal(read(None, Some(persons)), to, MovedBy::Ai).unwrap();
            assert_eq!(
                state(&moved),
                (
                    Priority::Interrupt,
                    Src::Task,
                    Human,
                    to.map(|goal| goal.id)
                ),
                "{to:?}"
            );
            assert_eq!(moved.own_priority_by(), Some(Human));
            // A person's move takes the new goal's, or normal.
            let moved = set_goal(read(None, Some(persons)), to, MovedBy::Person).unwrap();
            assert_eq!(moved.own_priority(), None, "{to:?}");
            assert_eq!(
                moved.priority(),
                to.map_or(Priority::Normal, |goal| goal.priority)
            );
        }
        // A person's own priority stays as it is, wherever the AI moves it.
        let own = read(Some((Priority::Urgent, Human)), Some(persons));
        let moved = set_goal(own, Some(low), MovedBy::Ai).unwrap();
        assert_eq!(state(&moved), (Priority::Urgent, Src::Task, Human, id(2)));
        // A priority the AI set, its own or its goal's: the new goal's.
        let ais = goal(3, Priority::High, Ai);
        let moved = set_goal(read(None, Some(ais)), Some(low), MovedBy::Ai).unwrap();
        assert_eq!(state(&moved), (Priority::Low, Src::Goal, Ai, id(2)));
        let moved = set_goal(
            read(Some((Priority::High, Ai)), Some(persons)),
            None,
            MovedBy::Ai,
        )
        .unwrap();
        assert_eq!(state(&moved), (Priority::High, Src::Task, Ai, None));
        let alone = set_goal(read(None, None), Some(low), MovedBy::Ai).unwrap();
        assert_eq!(state(&alone), (Priority::Low, Src::Goal, Ai, id(2)));
        // Staying in the same goal changes nothing.
        let stays = set_goal(read(None, Some(persons)), Some(persons), MovedBy::Ai).unwrap();
        assert_eq!(stays.own_priority(), None);
    }

    /// ADR-t1975-1 decision 4, in a row: the AI moves a task that takes a
    /// person's `high` to a goal of the same `high` the AI set, which keeps
    /// it as the task's own; lowering that goal, moving the task to a low
    /// goal and out of any goal leave the task at the person's `high`.
    /// Moved instead to a goal of the same value a person set, it takes
    /// that goal's, and the AI may then change neither.
    #[test]
    fn a_kept_priority_survives_the_ais_later_changes_and_moves() {
        use super::super::{
            ActorRole,
            authorization::{PriorityHolder, check_priority_change},
        };
        use PriorityBy::{Ai, Human};
        let persons = goal(1, Priority::High, Human);
        let same = goal(2, Priority::High, Ai);
        let task = set_goal(read(None, Some(persons)), Some(same), MovedBy::Ai).unwrap();
        assert_eq!(
            task.own_priority().zip(task.own_priority_by()),
            Some((Priority::High, Human))
        );
        // The AI's goal is the AI's to lower...
        check_priority_change(
            ActorRole::Planner,
            PriorityHolder::Goal(same.id),
            same.priority,
            same.priority_by,
        )
        .unwrap();
        let lowered = goal(2, Priority::Low, Ai);
        let task = reread(&task, Some(lowered));
        assert_eq!(
            (task.priority(), task.priority_by()),
            (Priority::High, Human)
        );
        // ...but the task's priority is still the person's, and no AI move
        // changes it.
        assert!(
            check_priority_change(
                ActorRole::Planner,
                PriorityHolder::Task(task.id()),
                task.priority(),
                task.priority_by()
            )
            .is_err()
        );
        let task = set_goal(task, Some(goal(3, Priority::Low, Ai)), MovedBy::Ai).unwrap();
        assert_eq!(
            (task.priority(), task.priority_by()),
            (Priority::High, Human)
        );
        let task = set_goal(task, None, MovedBy::Ai).unwrap();
        assert_eq!(
            (task.priority(), task.priority_source(), task.priority_by()),
            (Priority::High, PrioritySource::Task, Human)
        );

        // To a goal of the same value a person set: it takes that goal's.
        let other = goal(4, Priority::High, Human);
        let task = set_goal(read(None, Some(persons)), Some(other), MovedBy::Ai).unwrap();
        assert_eq!(
            (
                task.own_priority(),
                task.priority_source(),
                task.priority_by()
            ),
            (None, PrioritySource::Goal, Human)
        );
        // The AI changes neither that goal's priority nor the task's.
        for holder in [
            PriorityHolder::Goal(other.id),
            PriorityHolder::Task(task.id()),
        ] {
            assert!(
                check_priority_change(
                    ActorRole::Planner,
                    holder,
                    Priority::High,
                    task.priority_by()
                )
                .is_err(),
                "{holder}"
            );
        }
        assert!(
            check_priority_change(
                ActorRole::Inbox,
                PriorityHolder::Goal(other.id),
                Priority::High,
                Human
            )
            .is_ok()
        );
        assert_eq!(MovedBy::of(ActorRole::User), MovedBy::Person);
        assert_eq!(MovedBy::of(ActorRole::Inbox), MovedBy::Person);
        assert_eq!(MovedBy::of(ActorRole::Planner), MovedBy::Ai);
        assert_eq!(MovedBy::of(ActorRole::Supervisor), MovedBy::Ai);
    }

    /// ADR-t1340-1: a task that names no mode stores none and runs the
    /// provider's default (headless); a named mode is stored, a new
    /// provider without a mode names none again, and Codex keeps
    /// `headless`, its only mode.
    #[test]
    fn the_stored_mode_is_the_named_one_or_none() {
        use crate::domain::worker::Worker;
        let task = Task::new(TaskId::new(1), new_task(), "c".into()).unwrap();
        assert_eq!(task.worker(), Worker::CLAUDE_HEADLESS);
        assert_eq!(task.stored_worker_mode(), None);
        let named = Task::new(
            TaskId::new(2),
            NewTask {
                worker_mode: Some(WorkerMode::Interactive),
                ..new_task()
            },
            "c".into(),
        )
        .unwrap();
        assert_eq!(named.worker(), Worker::CLAUDE_INTERACTIVE);
        assert_eq!(named.stored_worker_mode(), Some(WorkerMode::Interactive));
        let edit_of = |provider, worker_mode| TaskEdit {
            provider,
            worker_mode,
            ..TaskEdit::default()
        };
        let retitled = TaskEdit {
            title: Some("t2".into()),
            ..TaskEdit::default()
        };
        let kept = edit(named, retitled).unwrap();
        assert_eq!(kept.stored_worker_mode(), Some(WorkerMode::Interactive));
        let codex = edit(kept, edit_of(Some(Provider::Codex), None)).unwrap();
        assert_eq!(codex.stored_worker_mode(), Some(WorkerMode::Headless));
        let back = edit(codex, edit_of(Some(Provider::Claude), None)).unwrap();
        assert_eq!(back.worker(), Worker::CLAUDE_HEADLESS);
        assert_eq!(back.stored_worker_mode(), None);
        let headless = edit(back, edit_of(None, Some(WorkerMode::Headless))).unwrap();
        assert_eq!(headless.stored_worker_mode(), Some(WorkerMode::Headless));
    }

    #[test]
    fn claim_takes_only_a_ready_task() {
        let task = claim(Task::restore(record(TaskStatus::Ready)).unwrap()).unwrap();
        assert_eq!(task.status(), TaskStatus::InProgress);
        let error = claim(task).unwrap_err();
        assert_eq!(
            error,
            DomainError::TaskNotClaimable {
                task_id: TaskId::new(5),
                status: TaskStatus::InProgress,
            }
        );
        assert_eq!(error.to_string(), "task 5 is in_progress, not ready");
    }

    #[test]
    fn a_new_task_is_a_draft_with_each_check_and_glob_once() {
        let task = Task::new(TaskId::new(3), new_task(), "now".into()).unwrap();
        assert_eq!(task.id(), TaskId::new(3));
        assert_eq!(task.status(), TaskStatus::Draft);
        assert_eq!(task.required_evidence(), [EvidenceCheck::E2e]);
        assert_eq!(task.paths(), ["docs/**"]);
        assert_eq!(task.goal_id(), Some(GoalId::new(2)));
        assert_eq!((task.created_at(), task.updated_at()), ("now", "now"));
        assert_eq!(
            (task.title(), task.description(), task.acceptance()),
            ("t", "d", "a")
        );
        assert_eq!(task.verification_commands(), ["cargo test"]);
        assert_eq!(task.context(), "c");
        assert_eq!(
            serde_json::to_value(&task).unwrap()["status"],
            serde_json::json!("draft")
        );
        assert_eq!(task.into_title(), "t");

        let blank = NewTask {
            title: " ".into(),
            ..new_task()
        };
        assert_eq!(
            Task::new(TaskId::new(3), blank, "now".into())
                .unwrap_err()
                .to_string(),
            "task title must not be blank"
        );
        assert_eq!(
            Task::new(TaskId::new(0), new_task(), "now".into())
                .unwrap_err()
                .to_string(),
            "task ID must be positive"
        );
    }

    #[test]
    fn restore_keeps_the_stored_state_and_checks_its_invariants() {
        let task = Task::restore(record(TaskStatus::InProgress)).unwrap();
        assert_eq!(task.status(), TaskStatus::InProgress);
        assert_eq!((task.created_at(), task.updated_at()), ("c", "u"));
        assert_eq!(
            Task::restore(TaskRecord {
                title: "".into(),
                ..record(TaskStatus::Ready)
            })
            .unwrap_err(),
            DomainError::Blank {
                field: "task title"
            }
        );
        assert_eq!(
            Task::restore(TaskRecord {
                id: TaskId::new(-1),
                ..record(TaskStatus::Ready)
            })
            .unwrap_err()
            .to_string(),
            "task ID must be positive"
        );
        assert_eq!(
            Task::restore(TaskRecord {
                goal_id: Some(GoalId::new(0)),
                ..record(TaskStatus::Ready)
            })
            .unwrap_err()
            .to_string(),
            "goal ID must be positive"
        );
    }

    #[test]
    fn transition_follows_the_status_rules() {
        let ready = transition(
            Task::restore(record(TaskStatus::Draft)).unwrap(),
            TaskAction::BypassReview,
            false,
        )
        .unwrap();
        assert_eq!(ready.status(), TaskStatus::Ready);
        let canceled = transition(ready, TaskAction::Cancel, false).unwrap();
        assert_eq!(canceled.status(), TaskStatus::Canceled);
        assert_eq!(
            transition(canceled, TaskAction::Ready, false).unwrap_err(),
            DomainError::TransitionNotAllowed {
                status: TaskStatus::Canceled,
                action: TaskAction::Ready
            }
        );
        let in_progress = Task::restore(record(TaskStatus::InProgress)).unwrap();
        assert!(transition(in_progress.clone(), TaskAction::Draft, true).is_err());
        assert_eq!(
            transition(in_progress.clone(), TaskAction::Draft, false)
                .unwrap()
                .status(),
            TaskStatus::Draft
        );
        // A retry returns an in-progress task to ready without plan review.
        assert_eq!(
            transition(in_progress.clone(), TaskAction::Ready, false)
                .unwrap()
                .status(),
            TaskStatus::Ready
        );
        assert!(transition(in_progress, TaskAction::BypassReview, false).is_err());
    }

    #[test]
    fn only_plan_review_or_a_bypass_readies_a_draft_or_submitted_task() {
        let draft = || Task::restore(record(TaskStatus::Draft)).unwrap();
        let submitted = transition(draft(), TaskAction::Submit, false).unwrap();
        assert_eq!(submitted.status(), TaskStatus::Submitted);
        assert_eq!(
            serde_json::to_value(submitted.status()).unwrap(),
            serde_json::json!("submitted")
        );
        for task in [draft(), submitted.clone()] {
            let status = task.status();
            let error = transition(task, TaskAction::Ready, false).unwrap_err();
            assert_eq!(error, DomainError::ReadyNeedsPlanReview { status });
            assert!(error.to_string().contains("pass --bypass-review"));
        }
        assert_eq!(
            transition(submitted.clone(), TaskAction::Approve, false)
                .unwrap()
                .status(),
            TaskStatus::Ready
        );
        assert_eq!(
            transition(submitted.clone(), TaskAction::BypassReview, false)
                .unwrap()
                .status(),
            TaskStatus::Ready
        );
        assert_eq!(
            transition(submitted.clone(), TaskAction::Draft, false)
                .unwrap()
                .status(),
            TaskStatus::Draft
        );
        assert_eq!(
            transition(submitted.clone(), TaskAction::Cancel, false)
                .unwrap()
                .status(),
            TaskStatus::Canceled
        );
        // Plan review approves only a submitted task, and submits only a draft.
        assert!(transition(draft(), TaskAction::Approve, false).is_err());
        assert_eq!(
            transition(submitted, TaskAction::Submit, false)
                .unwrap_err()
                .to_string(),
            "cannot apply Submit to task in submitted state"
        );
        let ready = Task::restore(record(TaskStatus::Ready)).unwrap();
        assert!(transition(ready.clone(), TaskAction::Submit, false).is_err());
        // Plan review takes a ready task back to submitted to change it.
        assert_eq!(
            transition(ready, TaskAction::Reopen, false)
                .unwrap()
                .status(),
            TaskStatus::Submitted
        );
        assert!(transition(draft(), TaskAction::Reopen, false).is_err());
        // Nothing claims a submitted task.
        let waiting = Task::restore(record(TaskStatus::Submitted)).unwrap();
        assert!(claim(waiting.clone()).is_err());
        assert!(waiting.status().content_editable());
        assert!(dependencies_editable(&waiting));
        assert_eq!(
            set_priority(waiting, Some(Priority::Low))
                .unwrap()
                .priority(),
            Priority::Low
        );
    }

    /// ADR-t1639-1 decision 2: a task without a priority of its own takes
    /// its goal's; setting one overrides it, and clearing it inherits again.
    #[test]
    fn a_task_inherits_its_goals_priority_until_it_sets_its_own() {
        let inheriting = Task::restore(TaskRecord {
            goal_id: Some(GoalId::new(3)),
            goal_priority: Some(Priority::High),
            ..record(TaskStatus::Ready)
        })
        .unwrap();
        assert_eq!(
            (inheriting.priority(), inheriting.priority_source()),
            (Priority::High, PrioritySource::Goal)
        );
        let json = serde_json::to_value(&inheriting).unwrap();
        assert_eq!(
            (&json["priority"], &json["priority_source"]),
            (&serde_json::json!("high"), &serde_json::json!("goal"))
        );
        let own = set_priority(inheriting, Some(Priority::Low)).unwrap();
        assert_eq!(
            (own.priority(), own.priority_source(), own.own_priority()),
            (Priority::Low, PrioritySource::Task, Some(Priority::Low))
        );
        let back = set_priority(own, None).unwrap();
        assert_eq!(
            (back.priority(), back.priority_source(), back.own_priority()),
            (Priority::High, PrioritySource::Goal, None)
        );
        let alone = Task::restore(record(TaskStatus::Draft)).unwrap();
        assert_eq!(
            (alone.priority(), alone.priority_source()),
            (Priority::Normal, PrioritySource::Default)
        );
        let registered = Task::new(
            TaskId::new(9),
            NewTask {
                priority: Some(Priority::Urgent),
                ..new_task()
            },
            "now".into(),
        )
        .unwrap();
        assert_eq!(
            (registered.priority(), registered.priority_source()),
            (Priority::Urgent, PrioritySource::Task)
        );
    }

    #[test]
    fn the_priority_changes_until_the_task_is_finished() {
        for (status, editable) in [
            (TaskStatus::Draft, true),
            (TaskStatus::Submitted, true),
            (TaskStatus::Ready, true),
            (TaskStatus::InProgress, true),
            (TaskStatus::Completed, false),
            (TaskStatus::Canceled, false),
        ] {
            assert_eq!(status.priority_editable(), editable, "{status:?}");
            let task = Task::restore(record(status)).unwrap();
            assert_eq!(
                set_priority(task, Some(Priority::Low)).is_ok(),
                editable,
                "{status:?}"
            );
        }
        // Only the priority: the rest stays fixed once the task is claimed.
        assert!(!TaskStatus::InProgress.dependencies_editable());
        let task = Task::restore(record(TaskStatus::InProgress)).unwrap();
        let low = set_priority(task, Some(Priority::Low)).unwrap();
        assert_eq!(low.priority(), Priority::Low);
        assert_eq!(low.status(), TaskStatus::InProgress);
        assert_eq!(set_priority(low, None).unwrap().own_priority(), None);
    }

    #[test]
    fn only_a_draft_or_ready_task_changes_its_goal_paths_or_dependencies() {
        let ready = Task::restore(record(TaskStatus::Ready)).unwrap();
        assert!(dependencies_editable(&ready));
        check_dependencies_editable(&ready).unwrap();
        let moved = set_goal(
            ready,
            Some(goal(4, Priority::Normal, PriorityBy::Ai)),
            MovedBy::Ai,
        )
        .unwrap();
        assert_eq!(moved.goal_id(), Some(GoalId::new(4)));
        let scoped = set_paths(moved, vec!["src/**".into(), "src/**".into()]).unwrap();
        assert_eq!(scoped.paths(), ["src/**"]);
        assert_eq!(scoped.priority(), Priority::Normal);
        let urgent = set_priority(scoped, Some(Priority::Urgent)).unwrap();
        assert_eq!(urgent.priority(), Priority::Urgent);
        assert_eq!(
            serde_json::to_value(&urgent).unwrap()["priority"],
            serde_json::json!("urgent")
        );
        assert!(matches!(
            set_paths(urgent, vec![" ".into()]),
            Err(DomainError::InvalidPathGlob { .. })
        ));
        let draft = Task::restore(record(TaskStatus::Draft)).unwrap();
        assert_eq!(
            set_priority(draft, Some(Priority::Low)).unwrap().priority(),
            Priority::Low
        );

        let claimed = || Task::restore(record(TaskStatus::InProgress)).unwrap();
        assert!(!dependencies_editable(&claimed()));
        assert_eq!(
            set_goal(claimed(), None, MovedBy::Person)
                .unwrap_err()
                .to_string(),
            "the goal can only be changed for draft, submitted or ready tasks"
        );
        assert_eq!(
            set_paths(claimed(), Vec::new()).unwrap_err().to_string(),
            "the paths can only be changed for draft, submitted or ready tasks"
        );
        for status in [TaskStatus::Completed, TaskStatus::Canceled] {
            let task = Task::restore(record(status)).unwrap();
            assert_eq!(
                set_priority(task, Some(Priority::Interrupt))
                    .unwrap_err()
                    .to_string(),
                format!(
                    "task 5 is {}; the priority can only be changed for draft, submitted, ready or in_progress tasks",
                    status.as_str()
                )
            );
        }
        assert_eq!(
            check_dependencies_editable(&claimed())
                .unwrap_err()
                .to_string(),
            "dependencies can only be changed for draft, submitted or ready tasks"
        );
    }

    #[test]
    fn edit_replaces_the_given_fields_of_a_draft_or_submitted_task_only() {
        let draft = Task::new(TaskId::new(3), new_task(), "now".into()).unwrap();
        let kept = edit(draft.clone(), TaskEdit::default()).unwrap();
        assert_eq!(
            serde_json::to_value(&kept).unwrap(),
            serde_json::to_value(&draft).unwrap()
        );
        let edited = edit(
            draft,
            TaskEdit {
                change: Some("feature".parse::<TaskChange>().unwrap()),
                title: Some("t2".into()),
                description: Some("d2".into()),
                acceptance: Some("a2".into()),
                verification_commands: Some(Vec::new()),
                required_evidence: Some(vec![
                    EvidenceCheck::Tests,
                    EvidenceCheck::E2e,
                    EvidenceCheck::Tests,
                ]),
                paths: Some(vec!["src/**".into(), "src/**".into()]),
                context: Some("c2".into()),
                provider: None,
                worker_mode: None,
                wait_for_build: Some(true),
            },
        )
        .unwrap();
        assert!(edited.wait_for_build());
        // Shown only when declared (ADR-t1632-1).
        assert_eq!(
            serde_json::to_value(&edited).unwrap()["wait_for_build"],
            true
        );
        assert!(
            serde_json::to_value(&kept)
                .unwrap()
                .get("wait_for_build")
                .is_none()
        );
        let withdrawn = edit(
            edited.clone(),
            TaskEdit {
                wait_for_build: Some(false),
                ..TaskEdit::default()
            },
        )
        .unwrap();
        assert!(!withdrawn.wait_for_build());
        assert_eq!(
            (edited.title(), edited.description(), edited.acceptance()),
            ("t2", "d2", "a2")
        );
        assert!(edited.verification_commands().is_empty());
        assert_eq!(
            edited.required_evidence(),
            [EvidenceCheck::Tests, EvidenceCheck::E2e]
        );
        assert_eq!(edited.paths(), ["src/**"]);
        assert_eq!(edited.context(), "c2");
        assert_eq!(edited.change().map(TaskChange::as_str), Some("feature"));
        assert_eq!(edited.status(), TaskStatus::Draft);

        let invalid = [
            (
                TaskEdit {
                    title: Some(" ".into()),
                    ..TaskEdit::default()
                },
                "task title must not be blank",
            ),
            (
                TaskEdit {
                    verification_commands: Some(vec!["".into()]),
                    ..TaskEdit::default()
                },
                "verification commands must not be blank",
            ),
        ];
        for (change, message) in invalid {
            assert!(!change.is_empty());
            assert_eq!(
                edit(edited.clone(), change).unwrap_err().to_string(),
                message
            );
        }
        assert!(matches!(
            edit(
                edited,
                TaskEdit {
                    paths: Some(vec!["/abs".into()]),
                    ..TaskEdit::default()
                }
            ),
            Err(DomainError::InvalidPathGlob { .. })
        ));
        assert!(TaskEdit::default().is_empty());

        for status in [
            TaskStatus::Ready,
            TaskStatus::InProgress,
            TaskStatus::Completed,
            TaskStatus::Canceled,
        ] {
            assert!(!status.content_editable());
            let task = Task::restore(record(status)).unwrap();
            let change = TaskEdit {
                context: Some("x".into()),
                ..TaskEdit::default()
            };
            assert_eq!(
                edit(task, change).unwrap_err(),
                DomainError::TaskContentNotEditable {
                    task_id: TaskId::new(5),
                    status,
                }
            );
        }
        let ready = Task::restore(record(TaskStatus::Ready)).unwrap();
        assert_eq!(
            edit(
                ready,
                TaskEdit {
                    title: Some("x".into()),
                    ..TaskEdit::default()
                }
            )
            .unwrap_err()
            .to_string(),
            "task 5 is ready; only a draft or submitted task can be edited freely; an in_progress task permits only user or inbox --verify/--no-verify after its latest run ended and no live run remains"
        );
        let submitted = Task::restore(record(TaskStatus::Submitted)).unwrap();
        let edited = edit(
            submitted,
            TaskEdit {
                acceptance: Some("a3".into()),
                ..TaskEdit::default()
            },
        )
        .unwrap();
        assert_eq!(
            (edited.acceptance(), edited.status()),
            ("a3", TaskStatus::Submitted)
        );
    }

    #[test]
    fn a_dependency_is_neither_on_itself_nor_a_cycle() {
        check_not_self(TaskId::new(1), TaskId::new(2)).unwrap();
        assert_eq!(
            check_not_self(TaskId::new(1), TaskId::new(1))
                .unwrap_err()
                .to_string(),
            "a task cannot depend on itself"
        );
        check_acyclic(TaskId::new(1), TaskId::new(2), false).unwrap();
        assert_eq!(
            check_acyclic(TaskId::new(1), TaskId::new(2), true)
                .unwrap_err()
                .to_string(),
            "dependency 1 -> 2 would create a cycle"
        );
    }

    #[test]
    fn a_goal_dependency_is_neither_on_the_own_goal_nor_a_cycle() {
        let in_goal = Task::restore(TaskRecord {
            goal_id: Some(GoalId::new(4)),
            ..record(TaskStatus::Ready)
        })
        .unwrap();
        check_not_own_goal(&in_goal, GoalId::new(3)).unwrap();
        assert_eq!(
            check_not_own_goal(&in_goal, GoalId::new(4))
                .unwrap_err()
                .to_string(),
            "a task cannot depend on its own goal 4; the goal already waits for it"
        );
        check_goal_acyclic(TaskId::new(5), GoalId::new(3), false).unwrap();
        assert_eq!(
            check_goal_acyclic(TaskId::new(5), GoalId::new(3), true)
                .unwrap_err()
                .to_string(),
            "dependency 5 -> goal 3 would create a cycle"
        );
        check_membership_acyclic(&in_goal, GoalId::new(3), false, false).unwrap();
        assert_eq!(
            check_membership_acyclic(&in_goal, GoalId::new(3), true, true).unwrap_err(),
            DomainError::OwnGoalDependency {
                goal_id: GoalId::new(3)
            }
        );
        assert_eq!(
            check_membership_acyclic(&in_goal, GoalId::new(3), false, true)
                .unwrap_err()
                .to_string(),
            "moving task 5 to goal 3 would create a cycle: the task already waits for the goal"
        );
        let mut own = new_task();
        own.goal_dependencies = vec![GoalId::new(2)];
        assert_eq!(
            own.validate().unwrap_err(),
            DomainError::OwnGoalDependency {
                goal_id: GoalId::new(2)
            }
        );
        own.goal_dependencies = vec![GoalId::new(0)];
        assert_eq!(
            own.validate().unwrap_err().to_string(),
            "goal dependency IDs must be positive"
        );
    }

    /// ADR-t883-1: an edit authorized on a `ready` task with `task.write`
    /// is refused once the task is `in_progress`, where only `task.verify_edit`
    /// lets it through; the same status goes on.
    #[test]
    fn an_edit_goes_on_only_with_the_status_it_was_authorized_with() {
        let task = Task::restore(record(TaskStatus::InProgress)).unwrap();
        assert_eq!(
            check_status_authorized(&task, TaskStatus::Ready)
                .unwrap_err()
                .to_string(),
            "task 5 is in_progress now, not ready as when this command was authorized; nothing was changed, run it again"
        );
        check_status_authorized(&task, TaskStatus::InProgress).unwrap();
        let ready = Task::restore(record(TaskStatus::Ready)).unwrap();
        check_status_authorized(&ready, TaskStatus::Ready).unwrap();
        assert!(check_status_authorized(&ready, TaskStatus::InProgress).is_err());
    }

    /// A duplicate is of another task that exists and is not canceled; one
    /// canceled as a duplicate names its original (moved from the
    /// integration test a_plan_review_duplicate_of_a_canceled_missing_or_the_same_task_fails).
    #[test]
    fn a_duplicate_is_of_another_task_that_is_not_canceled() {
        let (task, original) = (TaskId::new(5), TaskId::new(1));
        assert_eq!(
            duplicate_refusal(task, original, Some((TaskStatus::Ready, None))),
            None
        );
        assert_eq!(
            duplicate_refusal(task, task, Some((TaskStatus::Ready, None))).as_deref(),
            Some("task 5 cannot be a duplicate of itself")
        );
        assert_eq!(
            duplicate_refusal(task, TaskId::new(999), None).as_deref(),
            Some("task 999 does not exist")
        );
        assert_eq!(
            duplicate_refusal(task, TaskId::new(2), Some((TaskStatus::Canceled, None))).as_deref(),
            Some("task 2 is canceled; a duplicate needs a task that is not")
        );
        assert_eq!(
            duplicate_refusal(
                task,
                TaskId::new(3),
                Some((TaskStatus::Canceled, Some(original)))
            )
            .as_deref(),
            Some("task 3 is canceled as a duplicate of task 1; pass --duplicate-of 1")
        );
    }
}
