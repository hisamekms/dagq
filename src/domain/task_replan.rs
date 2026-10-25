//! The replanning of a long-running task's run (ADR-t1521-1, ADR-t1521-2):
//! at most one open operation per task ([`carry`], [`needs_task_alone`]),
//! on its latest run while it does not guard the task and on the run it
//! fenced once it does, from a person's request (`replan request`), a
//! worker's proposal on its own run (`replan propose`) or the runtime's
//! detection, which only the trusted runtime moves on (diagnosis, the
//! park at a safe boundary, the snapshot, the planner's replacement
//! proposal, the atomic replacement). The operation keeps a copy of the
//! task's and its goal's conditions and their version as they were when
//! it began, the diagnosis's evidence and decision, and how it ended.
//!
//! While an operation guards its task ([`ReplanStatus::guards`]), every
//! ordinary step that would move the task or its run on is refused by the
//! one judgment [`check_step`], whichever role asks: the claim and the
//! candidates, a resume, a recovery round, a revise, `ready`, `edit`,
//! `cancel`, a change of its dependencies and a landing. The same judgment
//! takes the task's hold ([`super::task_hold`]) as its other guard: either
//! refuses the steps it reaches, and neither's end lifts the other.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    DomainError, EventId, ExecutionClass, GoalId, ProposalId, Provider, ReplanId, RequestId, RunId,
    RunStatus, Task, TaskChange, TaskId, TaskStatus,
    error::require,
    plan_request::RequestStatus,
    task_hold::{self, HoldGuard},
    worker::WorkerMode,
};

// Where an operation came from: a person's (or the inbox's) request, a
// worker's proposal on its own run, or the runtime's detection.
string_enum!(ReplanOrigin {
    Manual => "manual",
    Worker => "worker",
    Runtime => "runtime",
});

// Where an operation is (docs/plans/task-replanning.md,
// "永続状態と所有権"). `candidate` and `diagnosing` leave the run going;
// `continuing`, `applied`, `continuing_after_park` and `withdrawn` end the
// operation; every other one guards the task.
string_enum!(ReplanStatus {
    Candidate => "candidate",
    Diagnosing => "diagnosing",
    Continuing => "continuing",
    PauseRequested => "pause_requested",
    Quiescing => "quiescing",
    Snapshotting => "snapshotting",
    Parked => "parked",
    Planning => "planning",
    Reviewing => "reviewing",
    AwaitingAnswer => "awaiting_answer",
    Applying => "applying",
    Applied => "applied",
    ContinuingAfterPark => "continuing_after_park",
    Withdrawn => "withdrawn",
    Blocked => "blocked",
});

impl ReplanStatus {
    pub const ALL: [Self; 15] = [
        Self::Candidate,
        Self::Diagnosing,
        Self::Continuing,
        Self::PauseRequested,
        Self::Quiescing,
        Self::Snapshotting,
        Self::Parked,
        Self::Planning,
        Self::Reviewing,
        Self::AwaitingAnswer,
        Self::Applying,
        Self::Applied,
        Self::ContinuingAfterPark,
        Self::Withdrawn,
        Self::Blocked,
    ];

    /// Whether the operation has ended: a new request on the task's run
    /// opens a new one. Only a continuation or a withdrawal whose run's work cannot
    /// be restored is moved again, back to `blocked`
    /// ([`Self::reopens`]).
    pub const fn is_closed(self) -> bool {
        matches!(
            self,
            Self::Continuing | Self::Applied | Self::ContinuingAfterPark | Self::Withdrawn
        )
    }

    /// Whether the operation guards its task: from the pause's fence until
    /// it ends, a `blocked` one included, so that the run's results and the
    /// work it saved stay as they are.
    pub const fn guards(self) -> bool {
        !self.is_closed() && !matches!(self, Self::Candidate | Self::Diagnosing)
    }

    /// Whether the trusted runtime is parking the operation's run: from
    /// the pause's fence until the snapshot is recorded, the run's writer
    /// is waited for and stopped and its work saved.
    pub const fn parks(self) -> bool {
        matches!(
            self,
            Self::PauseRequested | Self::Quiescing | Self::Snapshotting
        )
    }

    /// Whether the operation holds a complete snapshot from here on: the
    /// request's planner is opened only then.
    pub const fn needs_snapshot(self) -> bool {
        matches!(
            self,
            Self::Parked
                | Self::Planning
                | Self::Reviewing
                | Self::AwaitingAnswer
                | Self::Applying
                | Self::Applied
        )
    }

    /// Whether a move to this status ends the operation's planning request
    /// undecided, to be declined in the same transaction, while no planner
    /// is open for it now (`planner_open` false: none was ever opened, or
    /// each one opened ended without deciding it): its planner is opened
    /// only while the operation is with the planner
    /// ([`Self::hands_to_planner`]), so the request of an operation closed,
    /// before its snapshot or after it (`continuing_after_park`,
    /// `withdrawn`), would stay `open` with nobody to open a planner for it
    /// or end it. It is declined at the close rather than when the run's
    /// restore is confirmed, since a closed operation's run may go on
    /// without a restore. `blocked` is not an end: it may still go on to
    /// the planner, and the request waits for it.
    ///
    /// A planner open at the close is left to finish its turn, the request
    /// still its own to submit or decline: stopping it mid-turn would throw
    /// away what it has written, and the move is the runtime's, not the
    /// planner's. When it ends without deciding, the request is declined
    /// as its row closes, by the same judgment with `planner_open` false
    /// on the operation's closed status.
    pub const fn ends_request_undecided(self, planner_open: bool) -> bool {
        self.is_closed() && !planner_open
    }

    /// Whether an operation in this status is with the planner: it holds a
    /// complete snapshot and its request's planner may be opened.
    pub const fn hands_to_planner(self) -> bool {
        matches!(
            self,
            Self::Parked | Self::Planning | Self::Reviewing | Self::AwaitingAnswer
        )
    }

    /// Whether a move from this status to `to` records a new planning
    /// request for the operation, its request having been declined by the
    /// runtime as the operation closed undecided (`declined_at_close`: the
    /// request's `request_declined` by the runtime names this operation,
    /// [`Self::ends_request_undecided`]): a closed operation whose restore
    /// failed, back in `blocked` ([`Self::reopens`]), that goes on to the
    /// planner, straight or through the snapshot again, gets one planner
    /// from the new request when it reaches it. An ended request never goes
    /// back to `open`, so it is not reopened; one its planner submitted,
    /// declined or ran out of planners on is not renewed.
    pub const fn renews_request(self, to: Self, declined_at_close: bool) -> bool {
        !self.hands_to_planner() && to.hands_to_planner() && declined_at_close
    }

    /// Whether an operation in this status, closed, is moved again: a
    /// continuation or a withdrawal whose run's resume finds the work the
    /// park saved cannot be restored goes back to `blocked`, with the work
    /// kept, rather than leave the run waiting with nothing to say why.
    pub const fn reopens(self) -> bool {
        matches!(self, Self::ContinuingAfterPark | Self::Withdrawn)
    }

    /// The statuses the trusted runtime moves an operation in this one to.
    /// A proposal under review stays `reviewing` across its revises;
    /// `blocked` goes back to the step it failed at, or ends as a
    /// continuation or a withdrawal.
    pub const fn next(self) -> &'static [Self] {
        use ReplanStatus as S;
        match self {
            S::Candidate => &[S::Diagnosing, S::Continuing, S::PauseRequested],
            S::Diagnosing => &[S::Continuing, S::PauseRequested, S::Blocked],
            // `continuing` before the park: the run landed first, its
            // landing begun before the fence (the operation is not needed).
            S::PauseRequested => &[S::Quiescing, S::Snapshotting, S::Continuing, S::Blocked],
            S::Quiescing => &[S::Snapshotting, S::Continuing, S::Blocked],
            S::Snapshotting => &[S::Parked, S::Continuing, S::Blocked],
            S::Parked => &[S::Planning, S::ContinuingAfterPark, S::Blocked],
            S::Planning => &[
                S::Reviewing,
                S::ContinuingAfterPark,
                S::Withdrawn,
                S::Blocked,
            ],
            S::Reviewing | S::AwaitingAnswer => &[
                S::Planning,
                S::Reviewing,
                S::AwaitingAnswer,
                S::Applying,
                S::ContinuingAfterPark,
                S::Withdrawn,
                S::Blocked,
            ],
            S::Applying => &[S::Applied, S::Blocked],
            S::Blocked => &[
                S::Quiescing,
                S::Snapshotting,
                S::Parked,
                S::Planning,
                S::Reviewing,
                S::Applying,
                S::ContinuingAfterPark,
                S::Withdrawn,
            ],
            // A restore that failed (see `reopens`).
            S::ContinuingAfterPark | S::Withdrawn => &[S::Blocked],
            S::Continuing | S::Applied => &[],
        }
    }

    /// Whether the runtime may move an operation from this status to `to`.
    pub fn can_move_to(self, to: Self) -> bool {
        self.next().contains(&to)
    }
}

// A step of a task or its run an operation that guards it refuses.
string_enum!(GuardedStep {
    Claim => "claim",
    Resume => "resume",
    Recover => "recover",
    Revise => "revise",
    Ready => "ready",
    Edit => "edit",
    Cancel => "cancel",
    Dependency => "dependency",
    Integrate => "integrate",
});

/// The operation that guards a task now, as the guard reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ReplanGuard {
    pub task: TaskId,
    pub operation: ReplanId,
    pub status: ReplanStatus,
}

/// What guards a task's steps now: the replanning operation and the hold
/// open on it, each `None` when there is none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TaskGuards {
    pub replan: Option<ReplanGuard>,
    pub hold: Option<HoldGuard>,
}

impl TaskGuards {
    /// Only the replanning operation `replan`, for a step no hold reaches.
    pub const fn replan(replan: Option<ReplanGuard>) -> Self {
        Self { replan, hold: None }
    }

    /// What fences the task's run at its turn's boundary now: the
    /// replanning operation past its pause's fence, else the open hold.
    /// One fence for both (ADR-t1879-1 decision 7): while either holds, no
    /// next turn is sent or taken, no review, revise or landing starts, and
    /// the park takes the run once the step that goes ends. The replanning
    /// is named first, since its park also saves the work; a hold under it
    /// still fences once the replanning ends.
    pub fn fence(&self) -> Option<Fence> {
        match (self.replan.filter(|guard| guard.status.guards()), self.hold) {
            (Some(replan), _) => Some(Fence::Replan(replan)),
            (None, Some(hold)) => Some(Fence::Hold(hold)),
            (None, None) => None,
        }
    }
}

/// Why a task's run is fenced at its turn's boundary ([`TaskGuards::fence`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum Fence {
    Replan(ReplanGuard),
    Hold(HoldGuard),
}

impl Fence {
    /// What fences it, for the records and the logs.
    pub fn describe(&self) -> String {
        match self {
            Self::Replan(guard) => format!(
                "task {} is being replanned (replan operation {}, {})",
                guard.task,
                guard.operation,
                guard.status.as_str()
            ),
            Self::Hold(guard) => format!("task {} is held (hold {})", guard.task, guard.hold),
        }
    }
}

/// Refuse `step` of a task `guards` guard: the one judgment every entry
/// that moves a task or its run on asks, for every role. A replanning
/// operation refuses every step until its own outcome (written by the
/// trusted runtime) ends it; a hold refuses the steps
/// [`task_hold::holds_step`] names until a person releases it. Each is
/// checked whatever the other says, so ending one leaves the other.
pub fn check_step(guards: &TaskGuards, step: GuardedStep) -> Result<(), DomainError> {
    if let Some(guard) = guards.replan.filter(|guard| guard.status.guards()) {
        return Err(DomainError::TaskReplanning {
            task_id: guard.task,
            operation: guard.operation,
            status: guard.status,
            step,
        });
    }
    match guards.hold {
        Some(hold) if task_hold::holds_step(step) => Err(DomainError::TaskHeld {
            task_id: hold.task,
            hold: hold.hold,
            step,
        }),
        _ => Ok(()),
    }
}

/// Whether a run in `run` status of a task in `task` status, the task's
/// latest run when `latest`, takes a replanning operation: one the task
/// still waits on (unfinished, or stopped with the task `in_progress`).
pub fn check_replannable(
    run_id: &RunId,
    run: RunStatus,
    task: TaskStatus,
    latest: bool,
) -> Result<(), DomainError> {
    require(
        latest
            && task == TaskStatus::InProgress
            && !matches!(run, RunStatus::Integrated | RunStatus::Succeeded),
        || DomainError::RunNotReplannable {
            run_id: run_id.clone(),
            status: run,
            task_status: task,
        },
    )
}

/// One item of an acceptance as the operation fixes it: its 1-based
/// position among the non-blank lines, its text and the SHA-256 of the
/// text, so the same text twice is two items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceItem {
    pub ordinal: usize,
    pub text: String,
    pub hash: String,
}

/// The items of `acceptance`, one per non-blank line.
pub fn acceptance_items(acceptance: &str) -> Vec<AcceptanceItem> {
    acceptance
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
        .map(|(index, text)| AcceptanceItem {
            ordinal: index + 1,
            text: text.to_owned(),
            hash: sha256(text.as_bytes()),
        })
        .collect()
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The goal's conditions as the operation began: its acceptance's
/// version (`goals.acceptance_version`) and items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalConditions {
    pub id: GoalId,
    pub acceptance_version: i64,
    pub acceptance: Vec<AcceptanceItem>,
}

/// The task's conditions as the operation began, which a replacement
/// maps to its children: what it asks (title, description, acceptance
/// items), where and how it is checked (paths, verify, evidence), every
/// task it waits for and every task that waits for it, and its goal's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginalConditions {
    pub title: String,
    pub description: String,
    pub acceptance: Vec<AcceptanceItem>,
    pub paths: Vec<String>,
    pub verify: Vec<String>,
    pub evidence: serde_json::Value,
    pub predecessors: Vec<TaskId>,
    pub successors: Vec<TaskId>,
    pub goal: Option<GoalConditions>,
}

impl OriginalConditions {
    /// The version of the task's conditions: the SHA-256 of their JSON,
    /// which an apply compares with the task as it is then.
    pub fn version(&self) -> String {
        sha256(serde_json::to_string(self).unwrap_or_default().as_bytes())
    }
}

/// A child's conditions as plan review approves them and the apply checks
/// them again (ADR-t1521-2 decision 5): what it asks and how it is checked
/// as a replaced task's, every task and goal it waits for, and how its run
/// goes (who runs it, whether its claim waits for a build, its execution
/// class, its change and its context). The tasks that come to wait for it
/// change nothing it does, and its priority only orders the claims: they
/// are not among them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChildConditions {
    pub conditions: OriginalConditions,
    pub goal_predecessors: Vec<GoalId>,
    pub provider: Provider,
    pub worker_mode: Option<WorkerMode>,
    pub wait_for_build: bool,
    pub execution_class: ExecutionClass,
    pub change: Option<TaskChange>,
    pub context: String,
}

impl ChildConditions {
    /// Child `task`'s, from its `conditions` as a replaced task's (their
    /// successors left out) and the goals it waits for.
    pub fn of(task: &Task, mut conditions: OriginalConditions, goals: Vec<GoalId>) -> Self {
        conditions.successors.clear();
        Self {
            conditions,
            goal_predecessors: goals,
            provider: task.worker().provider,
            worker_mode: task.stored_worker_mode(),
            wait_for_build: task.wait_for_build(),
            execution_class: task.execution_class(),
            change: task.change().cloned(),
            context: task.context().to_owned(),
        }
    }

    /// The SHA-256 of their JSON, which the apply compares with the
    /// version plan review approved.
    pub fn version(&self) -> String {
        sha256(serde_json::to_string(self).unwrap_or_default().as_bytes())
    }
}

// A cause the diagnosis tells apart (ADR-t1521-1 decision 1).
string_enum!(ReplanCause {
    LocalDefect => "local_defect",
    AcceptanceUnclear => "acceptance_unclear",
    OversizedUnit => "oversized_unit",
    Conflict => "conflict",
    Verification => "verification",
    Environment => "environment",
});

// What the diagnosis chose.
string_enum!(DiagnosisChoice {
    Continue => "continue",
    Replan => "replan",
    Ask => "ask",
});

/// What the operation knows of why: the evidence (event IDs) its request
/// named or the runtime read, and, once diagnosed, the causes (the primary
/// first), the choice and its confidence and what to check next.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnosis {
    pub evidence: Vec<EventId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<ReplanCause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<DiagnosisChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_check: Option<String>,
}

/// A request or a proposal of an operation on `run`: why, with the events
/// it rests on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewReplan {
    pub run: RunId,
    pub reason: String,
    pub evidence: Vec<EventId>,
}

impl NewReplan {
    pub fn validate(&self) -> Result<(), DomainError> {
        require(!self.reason.trim().is_empty(), || DomainError::Blank {
            field: "reason",
        })
    }
}

/// What recording a request or a proposal did: the run's open operation,
/// whether this call opened it, and the planning request a person's
/// request recorded for it (now or before).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplanRecorded {
    pub operation: ReplanOperation,
    pub created: bool,
    pub request: Option<super::plan_request::PlanRequest>,
}

/// How one item of the task's acceptance went to the replacement: the
/// child task (and its item) that holds it, or none for a deletion, which
/// carries the authority that decided it. An operation deletes an item at
/// most once and maps a deleted item to no child, while several children
/// may hold one item together ([`check_mappings`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionMapping {
    pub ordinal: usize,
    pub hash: String,
    pub child_task_id: Option<TaskId>,
    pub child_ordinal: Option<usize>,
    pub disposition: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<serde_json::Value>,
}

/// An operation as the queue records it. Times are Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ReplanOperation {
    pub id: ReplanId,
    pub task_id: TaskId,
    pub run_id: RunId,
    pub goal_id: Option<GoalId>,
    pub origin: ReplanOrigin,
    pub status: ReplanStatus,
    /// Why the operation is where it is now (its last move's reason).
    pub status_reason: Option<String>,
    /// Raised at the pause's fence and at each adoption: a write under an
    /// older one is refused.
    pub generation: i64,
    /// The step a `blocked` operation goes back to.
    pub return_phase: Option<ReplanStatus>,
    pub reason: String,
    pub conditions: OriginalConditions,
    pub task_version: String,
    pub goal_acceptance_version: Option<i64>,
    pub diagnosis: Diagnosis,
    pub snapshot_id: Option<String>,
    pub request_id: Option<RequestId>,
    /// Where `request_id` is now, read with the operation: `declined`, with
    /// the operation's outcome as its reason, when the operation ended
    /// with no planner open for it
    /// ([`ReplanStatus::ends_request_undecided`]). `request_id` names the
    /// operation's latest request ([`ReplanStatus::renews_request`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_status: Option<RequestStatus>,
    pub proposal_id: Option<ProposalId>,
    pub requested_by: String,
    pub requested_by_id: String,
    pub mappings: Vec<ConditionMapping>,
    /// The tasks that replaced the task, once applied.
    pub replaced_by: Vec<TaskId>,
    pub created_at: i64,
    pub updated_at: i64,
    pub closed_at: Option<i64>,
}

impl ReplanOperation {
    /// The guard this operation puts on its task, if it guards it.
    pub fn guard(&self) -> Option<ReplanGuard> {
        self.status.guards().then_some(ReplanGuard {
            task: self.task_id,
            operation: self.id,
            status: self.status,
        })
    }
}

/// A move of an operation by the trusted runtime: to `to` for `reason`,
/// with the snapshot it saved and the diagnosis it made.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReplanMove {
    pub to: ReplanStatus,
    pub reason: String,
    pub snapshot_id: Option<String>,
    pub diagnosis: Option<Diagnosis>,
}

/// Whether `op` (read under `generation`) moves as `step` says: a move the
/// status allows, by the generation it was read under, never to a status
/// that needs a snapshot without one, and not into the replacement:
/// `applying` and `applied` are reached only by the trusted runtime's
/// atomic apply ([`check_apply_move`]).
pub fn check_move(
    op: &ReplanOperation,
    generation: i64,
    step: &ReplanMove,
) -> Result<(), DomainError> {
    check_move_as(op, generation, step, false)
}

/// [`check_move`] for the trusted runtime's atomic apply
/// ([`super::replan_apply`]), the one writer that moves an operation into
/// `applying` and `applied`.
pub fn check_apply_move(
    op: &ReplanOperation,
    generation: i64,
    step: &ReplanMove,
) -> Result<(), DomainError> {
    check_move_as(op, generation, step, true)
}

fn check_move_as(
    op: &ReplanOperation,
    generation: i64,
    step: &ReplanMove,
    apply: bool,
) -> Result<(), DomainError> {
    require(op.generation == generation, || {
        DomainError::ReplanGenerationStale {
            operation: op.id,
            expected: generation,
            actual: op.generation,
        }
    })?;
    require(op.status.can_move_to(step.to), || {
        DomainError::ReplanMoveNotAllowed {
            operation: op.id,
            from: op.status,
            to: step.to,
        }
    })?;
    require(
        apply || !matches!(step.to, ReplanStatus::Applying | ReplanStatus::Applied),
        || DomainError::ReplanApplyUnavailable { operation: op.id },
    )?;
    require(
        !step.to.needs_snapshot() || step.snapshot_id.is_some() || op.snapshot_id.is_some(),
        || DomainError::ReplanNeedsSnapshot {
            operation: op.id,
            to: step.to,
        },
    )?;
    require(!step.reason.trim().is_empty(), || DomainError::Blank {
        field: "reason",
    })
}

/// Whether operation `operation`, whose condition mappings are
/// `existing`, may add `added`: each acceptance item is deleted (a
/// mapping without a child task) at most once, and a deleted item has no
/// mapping to a child task, whichever was written first; an item may go to
/// several children. The key of the mappings holds the rows with a child
/// task one per child; a deletion's NULL child is not one the key
/// compares, so this check holds it.
pub fn check_mappings(
    operation: ReplanId,
    existing: &[ConditionMapping],
    added: &[ConditionMapping],
) -> Result<(), DomainError> {
    let mut deleted = std::collections::BTreeSet::new();
    let mut mapped = std::collections::BTreeSet::new();
    for mapping in existing.iter().chain(added) {
        let ordinal = mapping.ordinal;
        if mapping.child_task_id.is_none() {
            require(deleted.insert(ordinal), || {
                DomainError::ReplanDeletionRepeated { operation, ordinal }
            })?;
        } else {
            mapped.insert(ordinal);
        }
        require(
            !(deleted.contains(&ordinal) && mapped.contains(&ordinal)),
            || DomainError::ReplanDeletionMapped { operation, ordinal },
        )?;
    }
    Ok(())
}

/// The generation an operation in `from` takes on moving to `to`: one
/// more at the pause's fence, so the writes under the earlier one (a
/// receipt, a turn, a landing begun before) are refused.
pub const fn generation_after(from: ReplanStatus, to: ReplanStatus, generation: i64) -> i64 {
    if matches!(to, ReplanStatus::PauseRequested) && !from.guards() {
        generation + 1
    } else {
        generation
    }
}

/// Whether a move of an operation from `from` to `to` needs it to be its
/// task's only open operation: one that leaves it open from a status that
/// does not guard the task (`candidate` and `diagnosing` going on, a
/// closed one reopened to `blocked`). With the opening of one only on a
/// task that has none and [`carry`] at each new run, this keeps a task to
/// at most one open operation: no second one is diagnosed, fenced or
/// reopened beside another, so two never guard one task, and the work a
/// guarding one saved is never what is given up to keep one.
pub const fn needs_task_alone(from: ReplanStatus, to: ReplanStatus) -> bool {
    !to.is_closed() && !from.guards()
}

/// What to do with the operations open on a task (`open`, oldest first)
/// so that it keeps one, on its latest run `latest` when it does not guard
/// the task ([`carry`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Carry {
    /// The task's operation from here on ([`current`]).
    pub kept: Option<ReplanId>,
    /// The kept operation moved to `latest`, with the generation it takes.
    pub moved: Option<(ReplanId, i64)>,
    /// The others that do not guard the task (`candidate`, `diagnosing`),
    /// closed as `continuing`: superseded by the kept one.
    pub superseded: Vec<ReplanId>,
}

/// The one of `open` (a task's open operations, oldest first) that is the
/// task's operation: the one that guards it, which is what the guard
/// reads, else the newest. The rule of one per task leaves one, but the
/// rule of one per run before it could leave one on each run, none of
/// them guarding (nothing moved one into a guarding status beside
/// another, [`needs_task_alone`]).
pub fn current(open: &[ReplanOperation]) -> Option<&ReplanOperation> {
    open.iter()
        .find(|op| op.status.guards())
        .or_else(|| open.last())
}

/// Keep one of a task's open operations `open` (oldest first) once its
/// latest run is `latest` (the claim of a new run, or a request on it).
/// The operation is the task's, as its guard and its replacement are, so
/// a request made while an earlier run went on is taken over by the run
/// that goes on rather than closed, under a raised generation so that a
/// move decided on the earlier run is refused. One that guards stays on
/// the run it fenced, with the work it is saving or saved, and is never
/// closed here, whatever its step (the pause's fence, the wait for the
/// writer and the snapshot included): only the others that do not guard
/// (`candidate`, `diagnosing`, which leave their run going and hold no
/// work) are superseded by the kept one.
pub fn carry(open: &[ReplanOperation], latest: &RunId) -> Carry {
    let Some(kept) = current(open) else {
        return Carry {
            kept: None,
            moved: None,
            superseded: Vec::new(),
        };
    };
    let moved =
        (!kept.status.guards() && kept.run_id != *latest).then_some((kept.id, kept.generation + 1));
    let superseded = open
        .iter()
        .filter(|op| op.id != kept.id && !op.status.guards())
        .map(|op| op.id)
        .collect();
    Carry {
        kept: Some(kept.id),
        moved,
        superseded,
    }
}

/// What a request on run `run` does with the task's operation `kept`
/// (after [`carry`]): it joins one on the same run, or one that guards
/// the task (the task is being replanned already, whichever run it
/// parked), is refused while one that does not guard is open on another
/// run (after the carry, `run` is not the task's latest), and opens one
/// when there is none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Join(ReplanId),
    Refuse(ReplanId),
    Open,
}

/// [`Request`] for a request on `run` while `kept` is the task's operation.
pub fn request_on(kept: Option<&ReplanOperation>, run: &RunId) -> Request {
    match kept {
        Some(op) if op.run_id == *run || op.status.guards() => Request::Join(op.id),
        Some(op) => Request::Refuse(op.id),
        None => Request::Open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(status: ReplanStatus) -> ReplanOperation {
        ReplanOperation {
            id: ReplanId::new(1),
            task_id: TaskId::new(7),
            run_id: RunId::new("run-1").unwrap(),
            goal_id: None,
            origin: ReplanOrigin::Manual,
            status,
            status_reason: None,
            generation: 1,
            return_phase: None,
            reason: "too long".into(),
            conditions: OriginalConditions {
                title: "t".into(),
                description: "d".into(),
                acceptance: acceptance_items("a\nb"),
                paths: Vec::new(),
                verify: Vec::new(),
                evidence: serde_json::Value::Null,
                predecessors: Vec::new(),
                successors: Vec::new(),
                goal: None,
            },
            task_version: String::new(),
            goal_acceptance_version: None,
            diagnosis: Diagnosis::default(),
            snapshot_id: None,
            request_id: None,
            request_status: None,
            proposal_id: None,
            requested_by: "user".into(),
            requested_by_id: "user".into(),
            mappings: Vec::new(),
            replaced_by: Vec::new(),
            created_at: 0,
            updated_at: 0,
            closed_at: None,
        }
    }

    fn step(to: ReplanStatus) -> ReplanMove {
        ReplanMove {
            to,
            reason: "because".into(),
            snapshot_id: None,
            diagnosis: None,
        }
    }

    #[test]
    fn only_the_statuses_from_the_fence_to_the_end_guard_the_task() {
        let guarding: Vec<_> = ReplanStatus::ALL
            .into_iter()
            .filter(|status| status.guards())
            .map(ReplanStatus::as_str)
            .collect();
        assert_eq!(
            guarding,
            [
                "pause_requested",
                "quiescing",
                "snapshotting",
                "parked",
                "planning",
                "reviewing",
                "awaiting_answer",
                "applying",
                "blocked"
            ]
        );
        for status in ReplanStatus::ALL {
            assert!(!(status.guards() && status.is_closed()), "{status:?}");
            assert_eq!(
                status.is_closed() && !status.reopens(),
                status.next().is_empty(),
                "{status:?}"
            );
            if status.reopens() {
                assert!(status.is_closed());
                assert_eq!(status.next(), [ReplanStatus::Blocked]);
            }
        }
    }

    #[test]
    fn only_a_close_with_no_planner_open_leaves_the_request_undecided() {
        let undecided: Vec<_> = ReplanStatus::ALL
            .into_iter()
            .filter(|status| status.ends_request_undecided(false))
            .map(ReplanStatus::as_str)
            .collect();
        assert_eq!(
            undecided,
            [
                "continuing",
                "applied",
                "continuing_after_park",
                "withdrawn"
            ]
        );
        // Every status the operation may end from with no planner open
        // reaches one of them: before the snapshot, after it (`parked`),
        // `blocked` either side of it, which is no end itself, and with
        // the planner once its planners ended without deciding.
        for from in [
            ReplanStatus::Candidate,
            ReplanStatus::Diagnosing,
            ReplanStatus::PauseRequested,
            ReplanStatus::Quiescing,
            ReplanStatus::Snapshotting,
            ReplanStatus::Parked,
            ReplanStatus::Planning,
            ReplanStatus::Reviewing,
            ReplanStatus::AwaitingAnswer,
            ReplanStatus::Blocked,
        ] {
            assert!(
                from.next()
                    .iter()
                    .any(|to| to.ends_request_undecided(false)),
                "{from:?}"
            );
        }
        // A request whose planner is open now is its planner's to end.
        assert!(
            ReplanStatus::ALL
                .into_iter()
                .all(|status| !status.ends_request_undecided(true))
        );
    }

    #[test]
    fn only_a_reopened_operation_going_to_the_planner_renews_its_ended_request() {
        let with_planner: Vec<_> = ReplanStatus::ALL
            .into_iter()
            .filter(|status| status.hands_to_planner())
            .map(ReplanStatus::as_str)
            .collect();
        assert_eq!(
            with_planner,
            ["parked", "planning", "reviewing", "awaiting_answer"]
        );
        for status in ReplanStatus::ALL {
            assert!(!status.hands_to_planner() || status.needs_snapshot());
        }
        // A closed operation whose restore failed goes back to `blocked`,
        // and from there may go on to the planner.
        for closed in ReplanStatus::ALL.into_iter().filter(|s| s.reopens()) {
            assert!(closed.ends_request_undecided(false));
            assert!(closed.can_move_to(ReplanStatus::Blocked));
        }
        let renewing: Vec<_> = ReplanStatus::Blocked
            .next()
            .iter()
            .filter(|to| ReplanStatus::Blocked.renews_request(**to, true))
            .map(|to| to.as_str())
            .collect();
        assert_eq!(renewing, ["parked", "planning", "reviewing"]);
        // Through the snapshot again: renewed when it reaches the planner.
        assert!(!ReplanStatus::Blocked.renews_request(ReplanStatus::Snapshotting, true));
        assert!(ReplanStatus::Snapshotting.renews_request(ReplanStatus::Parked, true));
        for from in ReplanStatus::ALL {
            for to in ReplanStatus::ALL {
                // A request still open, or one its planner submitted or
                // declined, is kept.
                assert!(!from.renews_request(to, false), "{from:?} {to:?}");
                // Only the move onto the planner records it, once.
                assert_eq!(
                    from.renews_request(to, true),
                    !from.hands_to_planner() && to.hands_to_planner(),
                    "{from:?} {to:?}"
                );
            }
        }
    }

    #[test]
    fn a_guarding_operation_refuses_every_step_and_names_itself() {
        for status in ReplanStatus::ALL {
            let guard = ReplanGuard {
                task: TaskId::new(7),
                operation: ReplanId::new(3),
                status,
            };
            for step in [
                GuardedStep::Claim,
                GuardedStep::Resume,
                GuardedStep::Recover,
                GuardedStep::Revise,
                GuardedStep::Ready,
                GuardedStep::Edit,
                GuardedStep::Cancel,
                GuardedStep::Dependency,
                GuardedStep::Integrate,
            ] {
                let checked = check_step(&TaskGuards::replan(Some(guard)), step);
                assert_eq!(checked.is_err(), status.guards(), "{status:?} {step:?}");
                if let Err(error) = checked {
                    let text = error.to_string();
                    assert!(text.contains("task 7"), "{text}");
                    assert!(text.contains("replan operation 3"), "{text}");
                    assert!(text.contains(step.as_str()), "{text}");
                    assert!(text.contains(status.as_str()), "{text}");
                }
            }
        }
        assert!(check_step(&TaskGuards::default(), GuardedStep::Claim).is_ok());
    }

    #[test]
    fn a_hold_and_a_replanning_guard_the_task_each_whatever_the_other_says() {
        let task = TaskId::new(7);
        let hold = HoldGuard {
            task,
            hold: crate::domain::HoldId::new(2),
        };
        let parked = ReplanGuard {
            task,
            operation: ReplanId::new(3),
            status: ReplanStatus::Parked,
        };
        let ended = ReplanGuard {
            status: ReplanStatus::ContinuingAfterPark,
            ..parked
        };
        let held = |replan| TaskGuards {
            replan,
            hold: Some(hold),
        };
        for step in [
            GuardedStep::Claim,
            GuardedStep::Resume,
            GuardedStep::Recover,
            GuardedStep::Revise,
            GuardedStep::Integrate,
        ] {
            // Both: the replanning names itself first.
            assert!(matches!(
                check_step(&held(Some(parked)), step),
                Err(DomainError::TaskReplanning { .. })
            ));
            // The replanning ended: the hold still refuses.
            assert_eq!(
                check_step(&held(Some(ended)), step),
                Err(DomainError::TaskHeld {
                    task_id: task,
                    hold: hold.hold,
                    step
                })
            );
            // The hold released: the replanning still refuses.
            assert!(check_step(&TaskGuards::replan(Some(parked)), step).is_err());
            assert!(check_step(&TaskGuards::replan(Some(ended)), step).is_ok());
        }
        // A hold changes no plan: the steps it does not reach go.
        for step in [
            GuardedStep::Ready,
            GuardedStep::Edit,
            GuardedStep::Cancel,
            GuardedStep::Dependency,
        ] {
            assert!(check_step(&held(None), step).is_ok(), "{step:?}");
        }
        // One fence at the turn's boundary: the replanning while it guards,
        // the hold under it once it ends, and none once both are gone.
        assert_eq!(held(Some(parked)).fence(), Some(Fence::Replan(parked)));
        assert_eq!(held(Some(ended)).fence(), Some(Fence::Hold(hold)));
        assert_eq!(held(None).fence(), Some(Fence::Hold(hold)));
        assert_eq!(
            TaskGuards::replan(Some(parked)).fence(),
            Some(Fence::Replan(parked))
        );
        assert_eq!(TaskGuards::replan(Some(ended)).fence(), None);
        assert!(Fence::Hold(hold).describe().contains("held (hold 2)"));
    }

    #[test]
    fn a_run_takes_an_operation_while_its_in_progress_task_waits_on_it() {
        let run = RunId::new("run-1").unwrap();
        for status in [
            RunStatus::Running,
            RunStatus::NeedsSession,
            RunStatus::AwaitingIntegration,
            RunStatus::Failed,
            RunStatus::Interrupted,
        ] {
            assert!(check_replannable(&run, status, TaskStatus::InProgress, true).is_ok());
        }
        assert!(
            check_replannable(&run, RunStatus::Integrated, TaskStatus::InProgress, true).is_err()
        );
        assert!(check_replannable(&run, RunStatus::Running, TaskStatus::Completed, true).is_err());
        assert!(
            check_replannable(&run, RunStatus::Running, TaskStatus::InProgress, false).is_err()
        );
    }

    #[test]
    fn the_same_text_twice_is_two_items_and_the_version_follows_the_conditions() {
        let items = acceptance_items("- same\n\n- same\n  other  ");
        assert_eq!(
            items.iter().map(|item| item.ordinal).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(items[0].hash, items[1].hash);
        assert_ne!(items[0].hash, items[2].hash);
        assert_eq!(items[2].text, "other");
        let mut conditions = op(ReplanStatus::Candidate).conditions;
        let before = conditions.version();
        assert_eq!(before, conditions.version());
        conditions.successors.push(TaskId::new(9));
        assert_ne!(before, conditions.version());
    }

    #[test]
    fn a_move_needs_its_generation_an_allowed_status_and_a_snapshot_past_the_park() {
        let candidate = op(ReplanStatus::Candidate);
        assert!(check_move(&candidate, 1, &step(ReplanStatus::PauseRequested)).is_ok());
        assert!(matches!(
            check_move(&candidate, 0, &step(ReplanStatus::PauseRequested)),
            Err(DomainError::ReplanGenerationStale { .. })
        ));
        assert!(matches!(
            check_move(&candidate, 1, &step(ReplanStatus::Parked)),
            Err(DomainError::ReplanMoveNotAllowed { .. })
        ));
        let snapshotting = op(ReplanStatus::Snapshotting);
        assert!(matches!(
            check_move(&snapshotting, 1, &step(ReplanStatus::Parked)),
            Err(DomainError::ReplanNeedsSnapshot { .. })
        ));
        let saved = ReplanMove {
            snapshot_id: Some("snap-1".into()),
            ..step(ReplanStatus::Parked)
        };
        assert!(check_move(&snapshotting, 1, &saved).is_ok());
        let blank = ReplanMove {
            reason: " ".into(),
            ..step(ReplanStatus::Blocked)
        };
        assert!(check_move(&snapshotting, 1, &blank).is_err());
    }

    #[test]
    fn only_the_apply_moves_an_operation_into_the_replacement() {
        let mut reviewing = op(ReplanStatus::Reviewing);
        reviewing.snapshot_id = Some("snap".into());
        assert!(matches!(
            check_move(&reviewing, 1, &step(ReplanStatus::Applying)),
            Err(DomainError::ReplanApplyUnavailable { .. })
        ));
        let mut applying = op(ReplanStatus::Applying);
        applying.snapshot_id = Some("snap".into());
        assert!(check_move(&applying, 1, &step(ReplanStatus::Applied)).is_err());
        assert!(check_move(&reviewing, 1, &step(ReplanStatus::Reviewing)).is_ok());
        // The apply's own moves, still by the status and the generation.
        assert!(check_apply_move(&reviewing, 1, &step(ReplanStatus::Applying)).is_ok());
        assert!(check_apply_move(&applying, 1, &step(ReplanStatus::Applied)).is_ok());
        assert!(check_apply_move(&applying, 2, &step(ReplanStatus::Applied)).is_err());
        assert!(check_apply_move(&reviewing, 1, &step(ReplanStatus::Applied)).is_err());
    }

    #[test]
    fn the_runtime_parks_from_the_fence_to_the_snapshot_and_a_landing_first_ends_it() {
        let parking: Vec<_> = ReplanStatus::ALL
            .into_iter()
            .filter(|status| status.parks())
            .map(ReplanStatus::as_str)
            .collect();
        assert_eq!(parking, ["pause_requested", "quiescing", "snapshotting"]);
        for status in ReplanStatus::ALL.into_iter().filter(|s| s.parks()) {
            assert!(status.guards(), "{status:?}");
            assert!(!status.needs_snapshot(), "{status:?}");
        }
        assert!(ReplanStatus::PauseRequested.can_move_to(ReplanStatus::Continuing));
        assert!(ReplanStatus::Quiescing.can_move_to(ReplanStatus::Continuing));
        assert!(ReplanStatus::Snapshotting.can_move_to(ReplanStatus::Continuing));
        // A continuation or a withdrawal whose restore failed is blocked
        // again, by the generation it was read under; nothing else moves it.
        for closed in [ReplanStatus::ContinuingAfterPark, ReplanStatus::Withdrawn] {
            let mut ended = op(closed);
            ended.snapshot_id = Some("snap".into());
            assert!(check_move(&ended, 1, &step(ReplanStatus::Blocked)).is_ok());
            assert!(check_move(&ended, 0, &step(ReplanStatus::Blocked)).is_err());
            assert!(check_move(&ended, 1, &step(ReplanStatus::Planning)).is_err());
        }
        assert!(!ReplanStatus::Applied.reopens());
        assert!(!ReplanStatus::Continuing.reopens());
        assert!(!ReplanStatus::Parked.can_move_to(ReplanStatus::Continuing));
    }

    #[test]
    fn an_item_is_deleted_at_most_once() {
        let mapping = |ordinal: usize, child: Option<i64>| ConditionMapping {
            ordinal,
            hash: format!("h{ordinal}"),
            child_task_id: child.map(TaskId::new),
            child_ordinal: child.map(|_| 0),
            disposition: "d".into(),
            authority: None,
        };
        let id = ReplanId::new(3);
        let mapped = [mapping(0, Some(5)), mapping(0, Some(6)), mapping(1, None)];
        assert!(check_mappings(id, &[], &mapped).is_ok());
        assert!(check_mappings(id, &mapped[..2], &[mapping(0, Some(7))]).is_ok());
        assert!(matches!(
            check_mappings(id, &mapped, &[mapping(1, None)]),
            Err(DomainError::ReplanDeletionRepeated { ordinal: 1, .. })
        ));
        assert!(matches!(
            check_mappings(id, &[], &[mapping(2, None), mapping(2, None)]),
            Err(DomainError::ReplanDeletionRepeated { ordinal: 2, .. })
        ));
    }

    #[test]
    fn a_deleted_item_goes_to_no_child() {
        let mapping = |ordinal: usize, child: Option<i64>| ConditionMapping {
            ordinal,
            hash: format!("h{ordinal}"),
            child_task_id: child.map(TaskId::new),
            child_ordinal: child.map(|_| 0),
            disposition: "d".into(),
            authority: None,
        };
        let id = ReplanId::new(3);
        let conflict = |r: Result<(), DomainError>| {
            matches!(r, Err(DomainError::ReplanDeletionMapped { ordinal: 0, .. }))
        };
        // In one batch, in either order.
        assert!(conflict(check_mappings(
            id,
            &[],
            &[mapping(0, Some(5)), mapping(0, None)]
        )));
        assert!(conflict(check_mappings(
            id,
            &[],
            &[mapping(0, None), mapping(0, Some(5))]
        )));
        // In a later write, whichever was written first.
        assert!(conflict(check_mappings(
            id,
            &[mapping(0, Some(5)), mapping(0, Some(6))],
            &[mapping(0, None)]
        )));
        assert!(conflict(check_mappings(
            id,
            &[mapping(0, None)],
            &[mapping(0, Some(5))]
        )));
        // Other items stay free.
        assert!(check_mappings(id, &[mapping(0, None)], &[mapping(1, Some(5))]).is_ok());
    }

    #[test]
    fn the_fence_raises_the_generation_once() {
        assert_eq!(
            generation_after(ReplanStatus::Candidate, ReplanStatus::PauseRequested, 1),
            2
        );
        assert_eq!(
            generation_after(ReplanStatus::Diagnosing, ReplanStatus::PauseRequested, 2),
            3
        );
        assert_eq!(
            generation_after(ReplanStatus::PauseRequested, ReplanStatus::Quiescing, 2),
            2
        );
        let parked = op(ReplanStatus::Parked);
        assert_eq!(parked.guard().map(|guard| guard.operation), Some(parked.id));
        assert!(op(ReplanStatus::Candidate).guard().is_none());
    }

    fn on(id: i64, run: &str, status: ReplanStatus) -> ReplanOperation {
        ReplanOperation {
            id: ReplanId::new(id),
            run_id: RunId::new(run).unwrap(),
            ..op(status)
        }
    }

    #[test]
    fn a_candidate_of_an_earlier_run_moves_to_the_latest_under_a_new_generation() {
        let latest = RunId::new("run-2").unwrap();
        for status in [ReplanStatus::Candidate, ReplanStatus::Diagnosing] {
            let open = [on(1, "run-1", status)];
            assert_eq!(
                carry(&open, &latest),
                Carry {
                    kept: Some(ReplanId::new(1)),
                    moved: Some((ReplanId::new(1), 2)),
                    superseded: Vec::new(),
                }
            );
        }
        // Already on the latest run, or none: nothing moves.
        let open = [on(1, "run-2", ReplanStatus::Candidate)];
        assert_eq!(carry(&open, &latest).moved, None);
        assert_eq!(
            carry(&[], &latest),
            Carry {
                kept: None,
                moved: None,
                superseded: Vec::new()
            }
        );
    }

    #[test]
    fn of_two_open_the_newest_is_kept_unless_one_guards() {
        let latest = RunId::new("run-3").unwrap();
        let open = [
            on(1, "run-1", ReplanStatus::Diagnosing),
            on(2, "run-2", ReplanStatus::Candidate),
        ];
        assert_eq!(current(&open).unwrap().id, ReplanId::new(2));
        assert_eq!(
            carry(&open, &latest),
            Carry {
                kept: Some(ReplanId::new(2)),
                moved: Some((ReplanId::new(2), 2)),
                superseded: vec![ReplanId::new(1)],
            }
        );
        // A blocked one keeps its run and its saved work, and the guard
        // and the readings name it; the candidate is superseded.
        let open = [
            on(1, "run-1", ReplanStatus::Blocked),
            on(2, "run-2", ReplanStatus::Candidate),
        ];
        assert_eq!(current(&open).unwrap().id, ReplanId::new(1));
        assert_eq!(
            carry(&open, &RunId::new("run-2").unwrap()),
            Carry {
                kept: Some(ReplanId::new(1)),
                moved: None,
                superseded: vec![ReplanId::new(2)],
            }
        );
        // One that guards at any step is never closed to keep one; only
        // the candidate or diagnosing one beside it is.
        for guarding in ReplanStatus::ALL
            .into_iter()
            .filter(|status| status.guards())
        {
            for (older, newer) in [
                (guarding, ReplanStatus::Candidate),
                (ReplanStatus::Diagnosing, guarding),
            ] {
                let open = [on(1, "run-1", older), on(2, "run-2", newer)];
                let kept = if older.guards() { 1 } else { 2 };
                assert_eq!(
                    carry(&open, &latest),
                    Carry {
                        kept: Some(ReplanId::new(kept)),
                        moved: None,
                        superseded: vec![ReplanId::new(3 - kept)],
                    },
                    "{older:?} then {newer:?}"
                );
            }
        }
    }

    #[test]
    fn a_guarding_operation_is_not_closed_to_keep_one_while_its_run_is_parked() {
        // A blocked one beside one still parking its run (a state no write
        // makes, `needs_task_alone`): neither is closed, so the park still
        // waits for the writer and confirms the snapshot.
        let latest = RunId::new("run-3").unwrap();
        for parking in [
            ReplanStatus::PauseRequested,
            ReplanStatus::Quiescing,
            ReplanStatus::Snapshotting,
        ] {
            let open = [
                on(1, "run-1", ReplanStatus::Blocked),
                on(2, "run-2", parking),
            ];
            let carried = carry(&open, &latest);
            assert_eq!(carried.superseded, Vec::<ReplanId>::new(), "{parking:?}");
            assert_eq!(carried.moved, None, "{parking:?}");
        }
    }

    #[test]
    fn no_second_operation_of_a_task_goes_on_into_a_status_beside_another() {
        use ReplanStatus as S;
        // From a status that does not guard, a move that leaves it open
        // (a diagnosis, the pause's fence, a reopen to blocked) needs the
        // task to itself; a guarding one goes on, and any one may close.
        for (from, to) in [
            (S::Candidate, S::Diagnosing),
            (S::Candidate, S::PauseRequested),
            (S::Diagnosing, S::PauseRequested),
            (S::Diagnosing, S::Blocked),
            (S::Withdrawn, S::Blocked),
            (S::ContinuingAfterPark, S::Blocked),
        ] {
            assert!(from.can_move_to(to), "{from:?} to {to:?}");
            assert!(needs_task_alone(from, to), "{from:?} to {to:?}");
        }
        for (from, to) in [
            (S::Candidate, S::Continuing),
            (S::Diagnosing, S::Continuing),
            (S::PauseRequested, S::Quiescing),
            (S::Quiescing, S::Snapshotting),
            (S::Snapshotting, S::Parked),
            (S::Blocked, S::Parked),
            (S::Parked, S::Planning),
            (S::Snapshotting, S::Continuing),
        ] {
            assert!(from.can_move_to(to), "{from:?} to {to:?}");
            assert!(!needs_task_alone(from, to), "{from:?} to {to:?}");
        }
    }

    #[test]
    fn a_request_joins_the_operation_on_its_run_and_is_refused_on_another() {
        let one = on(4, "run-2", ReplanStatus::Candidate);
        let run = |name: &str| RunId::new(name).unwrap();
        assert_eq!(
            request_on(Some(&one), &run("run-2")),
            Request::Join(ReplanId::new(4))
        );
        assert_eq!(
            request_on(Some(&one), &run("run-1")),
            Request::Refuse(ReplanId::new(4))
        );
        assert_eq!(request_on(None, &run("run-1")), Request::Open);
        let blocked = on(3, "run-1", ReplanStatus::Blocked);
        assert_eq!(
            request_on(Some(&blocked), &run("run-2")),
            Request::Join(ReplanId::new(3))
        );
    }

    /// A child's version follows what it does, every task and goal it
    /// waits for and how its run goes, and not the tasks that come to wait
    /// for it nor its priority (ADR-t1521-2 decision 5).
    #[test]
    fn a_childs_version_follows_its_goals_and_how_it_runs_not_its_successors_or_priority() {
        use crate::domain::{Priority, TaskRecord, worker::Worker};
        let record = || TaskRecord {
            id: TaskId::new(10),
            title: "t".into(),
            description: "d".into(),
            acceptance: "a".into(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: None,
            goal_priority: None,
            change: None,
            status: TaskStatus::Submitted,
            goal_id: None,
            context: "why".into(),
            created_at: "c".into(),
            updated_at: "u".into(),
            worker: Worker::CLAUDE_HEADLESS,
            named_mode: Some(WorkerMode::Headless),
            wait_for_build: false,
            execution_class: ExecutionClass::Implementation,
        };
        let version = |edit: &dyn Fn(&mut TaskRecord), goals: &[i64], successors: &[i64]| {
            let mut task = record();
            edit(&mut task);
            let mut conditions = op(ReplanStatus::Reviewing).conditions;
            conditions.successors = successors.iter().copied().map(TaskId::new).collect();
            let goals = goals.iter().copied().map(GoalId::new).collect();
            ChildConditions::of(&Task::restore(task).unwrap(), conditions, goals).version()
        };
        let base = version(&|_| {}, &[3], &[]);
        assert_eq!(version(&|_| {}, &[3], &[20, 21]), base);
        assert_eq!(
            version(&|r| r.priority = Some(Priority::Urgent), &[3], &[]),
            base
        );
        let changed: [(&str, String); 8] = [
            ("goal removed", version(&|_| {}, &[], &[])),
            ("goal added", version(&|_| {}, &[3, 4], &[])),
            (
                "wait for build",
                version(&|r| r.wait_for_build = true, &[3], &[]),
            ),
            (
                "spike",
                version(&|r| r.execution_class = ExecutionClass::Spike, &[3], &[]),
            ),
            (
                "provider",
                version(&|r| r.worker = Worker::ALL[2], &[3], &[]),
            ),
            ("worker mode", version(&|r| r.named_mode = None, &[3], &[])),
            (
                "change",
                version(
                    &|r| r.change = Some(TaskChange::try_from("fix".to_owned()).unwrap()),
                    &[3],
                    &[],
                ),
            ),
            (
                "context",
                version(&|r| r.context = "else".into(), &[3], &[]),
            ),
        ];
        for (what, changed) in changed {
            assert_ne!(changed, base, "{what}");
        }
    }
}
