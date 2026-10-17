//! The hold of a task (ADR-t1879-1): a person's or the inbox's mark, with
//! a reason, that keeps the runtime from moving the task on while
//! something outside the plan (the environment, a login, an outside
//! service) is fixed. It is not the task's status: a held `ready` task
//! stays `ready` and keeps its plan review, and `draft`, `cancel`, `edit`
//! and `recover` mean what they meant.
//!
//! While a hold is open the supervisor claims no task of it, resumes no
//! run of it and starts no recovery job of it ([`holds_step`]); the one
//! judgment that refuses those steps is the replanning's
//! [`super::task_replan::check_step`], which takes the hold as one of its
//! guards, so neither guard's end lifts the other. A recovery job's
//! verdict or a person's answer to its ask that arrives while the hold is
//! open is kept as a record, not applied; once the hold is released it is
//! applied if neither the task nor the run moved meanwhile, and discarded
//! as stale otherwise ([`disposition`]).
//!
//! A step of a run that is already going is not stopped by a hold
//! (ADR-0049 decision 4): the hold is `settling` while it goes
//! ([`run_wait`]), and the replanning's park takes the run at the step's
//! end the same way, the hold as its other reason (ADR-t1879-1 decision
//! 7): no next turn, review, revise or landing starts, the wrapper ends
//! between turns, and the run waits for a session with its session and
//! worktree left in place, out of the slots. The hold is `held` once the
//! run waits so; a run already at rest is `held` at once. The wait is not
//! a stall, an idle session or a run for a recovery job: nothing of the
//! run runs, and nothing watches it until the release lets it resume.

use serde::{Deserialize, Serialize};

use super::{
    DomainError, EventId, HoldId, RunEvent, RunId, RunStatus, TaskId, TaskStatus,
    error::require,
    event_kind::{
        HOLD_DEFERRED, HOLD_RESULT_APPLIED, HOLD_RESULT_DISCARDED, HOLD_RESULT_KEPT,
        LEASE_ACQUIRED, LEASE_RELEASED, TASK_HELD, TASK_RELEASED, WORKSPACE_CLOSED,
    },
    task_replan::GuardedStep,
};

/// An open hold of a task, as the queue keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskHold {
    pub id: HoldId,
    pub task_id: TaskId,
    /// The run the hold named, when it was asked on a run: the hold is
    /// still its task's.
    pub run_id: Option<RunId>,
    pub reason: String,
    /// When to remind a person of the hold (unix seconds); passing it
    /// releases nothing.
    pub until: Option<i64>,
    /// Who held it: the role and the actor id.
    pub by_role: String,
    pub by_id: String,
    /// When it was held (unix seconds).
    pub created_at: i64,
    /// When the task stopped moving under the hold (unix seconds); `None`
    /// while what was going still goes ([`HoldState::Settling`]).
    pub settled_at: Option<i64>,
}

impl TaskHold {
    /// Where the hold stands.
    pub const fn state(&self) -> HoldState {
        if self.settled_at.is_some() {
            HoldState::Held
        } else {
            HoldState::Settling
        }
    }

    /// The guard the hold puts on its task's steps.
    pub const fn guard(&self) -> HoldGuard {
        HoldGuard {
            task: self.task_id,
            hold: self.id,
        }
    }
}

/// What a hold or a release names: a task, or a run whose task it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldTarget {
    Task(TaskId),
    Run(RunId),
}

impl HoldTarget {
    /// `requested` of `task_held`: what the command named.
    pub const fn requested(&self) -> &'static str {
        match self {
            Self::Task(_) => "task",
            Self::Run(_) => "run",
        }
    }
}

/// A hold as `hold` recorded it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HoldRecorded {
    pub hold: TaskHold,
    pub state: HoldState,
}

/// A release as `release` recorded it: the hold it closed and how the
/// task goes on (`claim`: back among the claims; `continue`: its run goes
/// on by the supervisor's paths).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseRecorded {
    pub hold: TaskHold,
    pub continuation: &'static str,
}

// Where a hold stands: `settling` while a step that was going when it was
// held still goes, `held` once the task waits.
string_enum!(HoldState {
    Settling => "settling",
    Held => "held",
});

// Where the run of a held task stands in its wait for the hold: no run
// that waits (none, or one the hold does not reach), a run whose step
// still goes, or a run that waits. Release is refused while `settling`.
string_enum!(RunHoldWait {
    None => "none",
    Settling => "settling",
    Settled => "settled",
});

/// The open hold of a task as the guard of its steps reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct HoldGuard {
    pub task: TaskId,
    pub hold: HoldId,
}

/// Whether an open hold refuses `step`: the claim, the resume, the start
/// of a recovery job, the revise a review asks for and the start of a
/// landing. A hold changes no plan, so `ready`, `edit`, `cancel` and the
/// dependencies stay as they are, and a landing already begun goes on.
pub const fn holds_step(step: GuardedStep) -> bool {
    matches!(
        step,
        GuardedStep::Claim
            | GuardedStep::Resume
            | GuardedStep::Recover
            | GuardedStep::Revise
            | GuardedStep::Integrate
    )
}

/// Where the latest run of a held task stands, in `status` and `leased`
/// (a process holds its lease): a run whose step goes (its turn, its
/// validation, review, e2e or landing, or a recovery job's round, each
/// under a lease, or a run on its way to a session) is
/// [`RunHoldWait::Settling`] until that step ends; a run at rest with no
/// one holding it (waiting for a resume, its landing turn, a person's
/// answer, or stopped) waits as it is ([`RunHoldWait::Settled`]); an ended
/// run, or none, is [`RunHoldWait::None`].
pub const fn run_wait(status: Option<RunStatus>, leased: bool) -> RunHoldWait {
    match status {
        Some(RunStatus::Claimed | RunStatus::Starting | RunStatus::Running)
        | Some(RunStatus::Integrating) => RunHoldWait::Settling,
        Some(
            RunStatus::Validating
            | RunStatus::AwaitingIntegration
            | RunStatus::NeedsSession
            | RunStatus::Failed
            | RunStatus::Interrupted,
        ) if leased => RunHoldWait::Settling,
        Some(
            RunStatus::Validating
            | RunStatus::AwaitingIntegration
            | RunStatus::NeedsSession
            | RunStatus::Failed
            | RunStatus::Interrupted,
        ) => RunHoldWait::Settled,
        Some(RunStatus::Integrated | RunStatus::Succeeded) | None => RunHoldWait::None,
    }
}

/// Whether a task in `status` takes a hold for `reason`: a reason that is
/// not blank, a task that is `ready`, `submitted` or `in_progress` (a
/// draft is not planned yet, an ended task has nothing to hold) and no
/// hold of it open already.
pub fn check_hold(
    task: TaskId,
    status: TaskStatus,
    open: Option<&TaskHold>,
    reason: &str,
) -> Result<(), DomainError> {
    require(!reason.trim().is_empty(), || DomainError::HoldReasonEmpty)?;
    require(
        matches!(
            status,
            TaskStatus::Ready | TaskStatus::Submitted | TaskStatus::InProgress
        ),
        || DomainError::TaskNotHoldable {
            task_id: task,
            status,
        },
    )?;
    match open {
        Some(open) => Err(DomainError::TaskAlreadyHeld {
            task_id: task,
            hold: open.id,
        }),
        None => Ok(()),
    }
}

/// The state a hold begins in, by its run's wait.
pub const fn held_state(wait: RunHoldWait) -> HoldState {
    match wait {
        RunHoldWait::Settling => HoldState::Settling,
        RunHoldWait::None | RunHoldWait::Settled => HoldState::Held,
    }
}

/// What may still write a resting run, as the queue's records tell, and
/// whether a park looked: a replanning's park that could not stop or save
/// the run's work ended `blocked` (`park_blocked`); a session process the
/// run registered has no exit recorded, whatever its heartbeat says
/// (`registered_process`: a lease given back, as by a resume given up, is
/// no writer's end); the supervisor's park found the writer ended
/// (`checked`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestWriter {
    pub park_blocked: bool,
    pub registered_process: bool,
    pub checked: bool,
}

impl RestWriter {
    /// Whether a writer may still run: the records leave one, and no park
    /// looked.
    pub const fn may_run(self) -> bool {
        !self.checked && (self.park_blocked || self.registered_process)
    }
}

/// The wait of a task's latest run in `status`, `leased` or not, for a
/// hold being recorded: [`run_wait`], a run whose writer may still run
/// ([`RestWriter::may_run`]) settling like a leased one, once a park found
/// its writer ended.
pub const fn rest_wait(status: Option<RunStatus>, leased: bool, writer: RestWriter) -> RunHoldWait {
    run_wait(status, leased || writer.may_run())
}

/// Whether a task's hold settles now at its run's rest: the run is the
/// task's latest (`latest`), nobody leases it, it rests in `status`
/// ([`run_wait`]) and no writer of it may run ([`RestWriter::may_run`]).
pub const fn settles_at_rest(
    status: RunStatus,
    leased: bool,
    latest: bool,
    writer: RestWriter,
) -> bool {
    latest
        && matches!(
            rest_wait(Some(status), leased, writer),
            RunHoldWait::Settled
        )
}

/// Whether the open hold `open` of `task` may be released while its run
/// stands at `wait`: refused when none is open, and while a step that
/// was going when it was held still goes (`hold_not_settled`), which
/// leaves the hold as it is.
pub fn check_release(
    task: TaskId,
    open: Option<&TaskHold>,
    wait: RunHoldWait,
) -> Result<(), DomainError> {
    let Some(open) = open else {
        return Err(DomainError::TaskNotHeld { task_id: task });
    };
    require(wait != RunHoldWait::Settling, || {
        DomainError::HoldNotSettled {
            task_id: task,
            hold: open.id,
        }
    })
}

/// `hold_result_discarded`'s `superseded`: the run's recovery moved on by
/// another path after the result was kept (a newer round or decision),
/// so the discard does not make its next round due again
/// ([`super::triage_state`]).
pub const SUPERSEDED: &str = "superseded";

/// `triage_finished`'s `action` of a recovery round whose verdict a hold
/// kept: the round applied nothing, and the run's next round is due once
/// the hold is released ([`super::triage_state`]).
pub const HELD_ACTION: &str = "held";

// Which result a hold kept instead of applying: a recovery job's verdict
// on an ended run (`act_on_recovery`), a person's answer to the ask of
// one (`apply_triage_answers`), or a recovery job's verdict on a live
// session (`apply_live`).
string_enum!(KeptSource {
    RecoveryVerdict => "recovery_verdict",
    TriageAnswer => "triage_answer",
    LiveVerdict => "live_verdict",
});

/// What says whether a task and its run moved while a result was kept:
/// the task's status and last change, the run's status and the last event
/// of the run that is not the hold's own nor a lease's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressVersion {
    pub task_status: TaskStatus,
    pub task_updated_at: String,
    pub run_status: RunStatus,
    pub last_event: Option<EventId>,
}

/// The kinds a run's [`ProgressVersion`] does not count: the hold's own
/// records, and the leases a recovery round takes and gives back and the
/// session it closes at its end, which move nothing on.
const NOT_PROGRESS: [&str; 9] = [
    TASK_HELD,
    TASK_RELEASED,
    HOLD_DEFERRED,
    HOLD_RESULT_KEPT,
    HOLD_RESULT_APPLIED,
    HOLD_RESULT_DISCARDED,
    LEASE_ACQUIRED,
    LEASE_RELEASED,
    WORKSPACE_CLOSED,
];

impl ProgressVersion {
    /// The version of a task in `task_status` last changed at
    /// `task_updated_at` and of its run in `run_status` with `events`.
    pub fn of(
        task_status: TaskStatus,
        task_updated_at: &str,
        run_status: RunStatus,
        events: &[RunEvent],
    ) -> Self {
        Self {
            task_status,
            task_updated_at: task_updated_at.to_owned(),
            run_status,
            last_event: events
                .iter()
                .rev()
                .find(|event| !NOT_PROGRESS.contains(&event.kind.as_str()))
                .map(|event| event.id),
        }
    }
}

// Why a kept result was discarded at its hold's release: the task or the
// run moved since it was kept, it was a verdict on a live session, which
// only the session's own watch can check again, or the path that applies
// it refused it now (a precondition no longer holds).
string_enum!(DiscardReason {
    Changed => "changed",
    LiveSession => "live_session",
    Refused => "refused",
});

/// What becomes of a result a hold kept, once the hold is released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Applied by the path that would have applied it, its preconditions
    /// checked again.
    Apply,
    /// Discarded as stale; the run goes on as if no result came.
    Discard(DiscardReason),
}

/// What becomes of a result from `source` kept at `kept` now that the task
/// and its run stand at `now`.
pub fn disposition(
    source: KeptSource,
    kept: &ProgressVersion,
    now: &ProgressVersion,
) -> Disposition {
    if source == KeptSource::LiveVerdict {
        Disposition::Discard(DiscardReason::LiveSession)
    } else if kept == now {
        Disposition::Apply
    } else {
        Disposition::Discard(DiscardReason::Changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hold(settled: bool) -> TaskHold {
        TaskHold {
            id: HoldId::new(4),
            task_id: TaskId::new(7),
            run_id: None,
            reason: "the login expired".into(),
            until: None,
            by_role: "user".into(),
            by_id: "user".into(),
            created_at: 100,
            settled_at: settled.then_some(100),
        }
    }

    #[test]
    fn a_hold_is_taken_by_a_planned_task_that_has_not_ended_and_has_none_open() {
        let task = TaskId::new(7);
        for status in [
            TaskStatus::Ready,
            TaskStatus::Submitted,
            TaskStatus::InProgress,
        ] {
            assert!(check_hold(task, status, None, "why").is_ok(), "{status:?}");
        }
        for status in [
            TaskStatus::Draft,
            TaskStatus::Completed,
            TaskStatus::Canceled,
        ] {
            let refused = check_hold(task, status, None, "why").unwrap_err();
            assert_eq!(
                refused,
                DomainError::TaskNotHoldable {
                    task_id: task,
                    status
                }
            );
            assert!(refused.to_string().contains(status.as_str()));
        }
        assert_eq!(
            check_hold(task, TaskStatus::Ready, None, "  "),
            Err(DomainError::HoldReasonEmpty)
        );
        assert_eq!(
            check_hold(task, TaskStatus::Ready, Some(&hold(true)), "again"),
            Err(DomainError::TaskAlreadyHeld {
                task_id: task,
                hold: HoldId::new(4)
            })
        );
    }

    #[test]
    fn a_release_waits_while_the_run_settles_and_needs_an_open_hold() {
        let task = TaskId::new(7);
        let open = hold(true);
        assert!(check_release(task, Some(&open), RunHoldWait::None).is_ok());
        assert!(check_release(task, Some(&open), RunHoldWait::Settled).is_ok());
        let refused = check_release(task, Some(&open), RunHoldWait::Settling).unwrap_err();
        assert_eq!(
            refused,
            DomainError::HoldNotSettled {
                task_id: task,
                hold: open.id
            }
        );
        assert!(refused.to_string().contains("hold_not_settled"));
        // The refusal is a value: the hold it read is as it was.
        assert_eq!(open, hold(true));
        assert_eq!(
            check_release(task, None, RunHoldWait::None),
            Err(DomainError::TaskNotHeld { task_id: task })
        );
    }

    #[test]
    fn a_hold_settles_at_once_unless_its_run_still_goes() {
        assert_eq!(held_state(RunHoldWait::None), HoldState::Held);
        assert_eq!(held_state(RunHoldWait::Settled), HoldState::Held);
        assert_eq!(held_state(RunHoldWait::Settling), HoldState::Settling);
        assert_eq!(hold(true).state(), HoldState::Held);
        assert_eq!(hold(false).state(), HoldState::Settling);
    }

    #[test]
    fn a_run_settles_for_its_hold_once_nothing_of_it_goes() {
        use RunStatus as S;
        for status in [S::Claimed, S::Starting, S::Running, S::Integrating] {
            for leased in [false, true] {
                assert_eq!(
                    run_wait(Some(status), leased),
                    RunHoldWait::Settling,
                    "{status:?}"
                );
            }
        }
        for status in [
            S::Validating,
            S::AwaitingIntegration,
            S::NeedsSession,
            S::Failed,
            S::Interrupted,
        ] {
            assert_eq!(run_wait(Some(status), true), RunHoldWait::Settling);
            assert_eq!(run_wait(Some(status), false), RunHoldWait::Settled);
        }
        for status in [Some(S::Integrated), Some(S::Succeeded), None] {
            assert_eq!(run_wait(status, true), RunHoldWait::None);
        }
    }

    #[test]
    fn a_rest_settles_its_hold_only_with_no_writer_left_or_one_a_park_found_ended() {
        let none = RestWriter::default();
        let blocked = RestWriter {
            park_blocked: true,
            ..none
        };
        let registered = RestWriter {
            registered_process: true,
            ..none
        };
        let both = RestWriter {
            park_blocked: true,
            registered_process: true,
            ..none
        };
        let rest = RunStatus::NeedsSession;
        // Nothing may write it: it settles as it rests.
        assert!(settles_at_rest(rest, false, true, none));
        // A blocked park or a process with no exit recorded keeps it
        // settling, until a park found the writer ended.
        for writer in [blocked, registered, both] {
            assert!(writer.may_run(), "{writer:?}");
            assert!(!settles_at_rest(rest, false, true, writer), "{writer:?}");
            assert_eq!(rest_wait(Some(rest), false, writer), RunHoldWait::Settling);
            let checked = RestWriter {
                checked: true,
                ..writer
            };
            assert!(!checked.may_run());
            assert!(settles_at_rest(rest, false, true, checked), "{checked:?}");
            assert_eq!(rest_wait(Some(rest), false, checked), RunHoldWait::Settled);
        }
        // A lease, a run that is not the task's latest, or a status whose
        // step goes settle nothing, a park's look or not.
        let checked = RestWriter {
            checked: true,
            ..none
        };
        assert!(!settles_at_rest(rest, true, true, checked));
        assert!(!settles_at_rest(rest, false, false, checked));
        assert!(!settles_at_rest(RunStatus::Running, false, true, checked));
        assert!(!settles_at_rest(
            RunStatus::Integrated,
            false,
            true,
            checked
        ));
        // An ended run, or none, has nothing to wait for.
        assert_eq!(rest_wait(None, false, registered), RunHoldWait::None);
    }

    #[test]
    fn a_hold_refuses_what_moves_its_task_on_but_not_its_plan() {
        let refused: Vec<GuardedStep> = [
            GuardedStep::Claim,
            GuardedStep::Resume,
            GuardedStep::Recover,
            GuardedStep::Revise,
            GuardedStep::Ready,
            GuardedStep::Edit,
            GuardedStep::Cancel,
            GuardedStep::Dependency,
            GuardedStep::Integrate,
        ]
        .into_iter()
        .filter(|step| holds_step(*step))
        .collect();
        assert_eq!(
            refused,
            [
                GuardedStep::Claim,
                GuardedStep::Resume,
                GuardedStep::Recover,
                GuardedStep::Revise,
                GuardedStep::Integrate
            ]
        );
    }

    fn event(id: i64, kind: &str) -> RunEvent {
        serde_json::from_value(json!({
            "id": id, "task_id": 7, "goal_id": null, "run_id": "r1", "kind": kind,
            "payload": {}, "created_at": "t",
        }))
        .unwrap()
    }

    #[test]
    fn a_kept_result_is_applied_only_when_nothing_moved_but_the_hold() {
        let events = vec![event(1, "run_claimed"), event(2, "triage_finished")];
        let kept = ProgressVersion::of(TaskStatus::InProgress, "t1", RunStatus::Failed, &events);
        assert_eq!(kept.last_event, Some(EventId::new(2)));
        let mut later = events.clone();
        later.extend([
            event(3, HOLD_RESULT_KEPT),
            event(4, LEASE_RELEASED),
            event(5, WORKSPACE_CLOSED),
            event(6, TASK_RELEASED),
        ]);
        let now = ProgressVersion::of(TaskStatus::InProgress, "t1", RunStatus::Failed, &later);
        for source in [KeptSource::RecoveryVerdict, KeptSource::TriageAnswer] {
            assert_eq!(disposition(source, &kept, &now), Disposition::Apply);
        }
        assert_eq!(
            disposition(KeptSource::LiveVerdict, &kept, &now),
            Disposition::Discard(DiscardReason::LiveSession)
        );
        later.push(event(7, "resume_started"));
        let moved = ProgressVersion::of(TaskStatus::InProgress, "t1", RunStatus::Failed, &later);
        assert_eq!(
            disposition(KeptSource::RecoveryVerdict, &kept, &moved),
            Disposition::Discard(DiscardReason::Changed)
        );
        let edited = ProgressVersion::of(TaskStatus::InProgress, "t2", RunStatus::Failed, &events);
        assert_eq!(
            disposition(KeptSource::TriageAnswer, &kept, &edited),
            Disposition::Discard(DiscardReason::Changed)
        );
    }

    fn payload(id: i64, kind: &str, payload: serde_json::Value) -> RunEvent {
        RunEvent {
            payload,
            ..event(id, kind)
        }
    }

    #[test]
    fn a_kept_round_and_a_discard_make_the_next_round_due_unless_superseded() {
        use crate::domain::{TriageState, triage_state};
        let held = payload(1, "triage_finished", json!({"action": HELD_ACTION}));
        assert_eq!(
            triage_state(std::slice::from_ref(&held)),
            TriageState::Pending
        );
        let asked = payload(2, "triage_finished", json!({"action": "ask"}));
        assert_eq!(triage_state(&[held, asked.clone()]), TriageState::Finished);
        let discarded = payload(3, HOLD_RESULT_DISCARDED, json!({SUPERSEDED: false}));
        assert_eq!(
            triage_state(&[asked.clone(), discarded]),
            TriageState::Pending
        );
        let superseded = payload(3, HOLD_RESULT_DISCARDED, json!({SUPERSEDED: true}));
        assert_eq!(triage_state(&[asked, superseded]), TriageState::Finished);
    }
}
