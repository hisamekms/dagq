//! The replanning of a long-running task's run (ADR-t1521-1, ADR-t1521-2):
//! one operation per run from a person's request (`replan request`), a
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
    DomainError, EventId, GoalId, ProposalId, ReplanId, RequestId, RunId, RunStatus, TaskId,
    TaskStatus,
    error::require,
    task_hold::{self, HoldGuard},
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

    /// Whether the operation has ended: a new request on the run opens a
    /// new one. Only a continuation or a withdrawal whose run's work cannot
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
/// carries the authority that decided it.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplanMove {
    pub to: ReplanStatus,
    pub reason: String,
    pub snapshot_id: Option<String>,
    pub diagnosis: Option<Diagnosis>,
}

/// Whether `op` (read under `generation`) moves as `step` says: a move the
/// status allows, by the generation it was read under, never to a status
/// that needs a snapshot without one, and not yet into the replacement,
/// whose atomic apply is not built: no operation reaches `applying` or
/// `applied` here.
pub fn check_move(
    op: &ReplanOperation,
    generation: i64,
    step: &ReplanMove,
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
        !matches!(step.to, ReplanStatus::Applying | ReplanStatus::Applied),
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
    fn no_operation_reaches_the_replacement_before_its_apply_is_built() {
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
}
