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
//! A task that ends takes its hold with it ([`HoldEnd`]): the transaction
//! that completes or cancels it closes the hold, and every result the hold
//! kept is discarded, since nothing is left for it to apply to. A task
//! reopened later is not held.
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

use crate::domain::EventKind;
use serde::{Deserialize, Serialize};

use super::{
    AskId, CommitSha, DomainError, EventId, HoldId, Provider, ReplanId, RunEvent, RunId, RunStatus,
    TaskId, TaskStatus, error::require, replan_apply::ChildCarry, replan_plan::CarrySource,
    task_replan::GuardedStep, turn::RecordedTurn,
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

/// An open hold with its task's latest run and that run's status now.
pub type HeldTask = (TaskHold, Option<(RunId, RunStatus)>);

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

/// A release as `release` recorded it: the hold it closed, how the task
/// goes on (`continuation`, [`ReleaseDecision::continuation`], and
/// `decision`), the provider and model it chose, and what the next run
/// carries over when it chose one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseRecorded {
    pub hold: TaskHold,
    pub continuation: &'static str,
    pub decision: ReleaseDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inherit: Option<ReleasedWork>,
    /// The end of the handover of a replacement's saved work to the task
    /// the release recorded ([`ReplanCarryEnded`]), when it ended one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replan_carry_ended: Option<ReplanCarryEnded>,
}

/// What a release chooses besides lifting the mark (ADR-t1879-1): the
/// provider and the model of the task's later turns and runs, each `None`
/// to keep the task's. Only a release changes them on an `in_progress`
/// task (`edit` takes a draft's or a submitted one's).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleaseChoice {
    pub provider: Option<Provider>,
    pub model: Option<String>,
}

// How a released task goes on, `task_released`'s `decision`: `claim`, a
// task with no run going is among the claims again; `resume`, its run goes
// on in its session by its own path (a run waiting for a session through
// the resume's entry, which may open a new session for a large context,
// ADR-t2080-1; a run waiting to land to its landing); `retry_inherit`, its
// run ends and the next run carries its branch over; `retry_fresh`, its
// run ends with no commit of its own and the next run starts from the
// current main (with the uncommitted work saved, when there was some).
string_enum!(ReleaseDecision {
    Claim => "claim",
    Resume => "resume",
    RetryInherit => "retry_inherit",
    RetryFresh => "retry_fresh",
});

impl ReleaseDecision {
    /// Whether the release ends the run for a new one.
    pub const fn new_run(self) -> bool {
        matches!(self, Self::RetryInherit | Self::RetryFresh)
    }

    /// `task_released`'s `continuation`: `claim`, `continue` (the run goes
    /// on) or `new_run`.
    pub const fn continuation(self) -> &'static str {
        match self {
            Self::Claim => "claim",
            Self::Resume => "continue",
            Self::RetryInherit | Self::RetryFresh => "new_run",
        }
    }
}

/// `task_released` as the store records it and the readers of other
/// contexts restore it ([`Self::of_event`]), the run that the release
/// found latest being the event's run. Events written before a field was
/// recorded restore it as its default: no `decision`, no model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Released {
    pub hold_id: Option<HoldId>,
    #[serde(default)]
    pub continuation: String,
    #[serde(default)]
    pub decision: Option<ReleaseDecision>,
    #[serde(default)]
    pub held_secs: i64,
    #[serde(default)]
    pub settled_secs: Option<i64>,
    #[serde(default)]
    pub provider: Option<Provider>,
    /// The model and effort of the task's later sessions the release
    /// chose; none when it chose no model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// What the next run carries over; written, not restored.
    #[serde(default, skip_deserializing)]
    pub inherit: Option<ReleasedWork>,
}

impl Released {
    /// The release `event` records; `None` for another kind or a payload
    /// that does not restore.
    pub fn of_event(event: &RunEvent) -> Option<Self> {
        if event.kind != EventKind::TaskReleased {
            return None;
        }
        serde_json::from_value(event.payload.clone()).ok()
    }

    /// The model and effort the release chose for the task's later
    /// sessions, if it chose a model.
    pub fn session(&self) -> Option<(&str, &str)> {
        Some((self.model.as_deref()?, self.effort.as_deref()?))
    }

    /// Whether the release ended the event's run for a new one.
    pub fn ended_run(&self) -> bool {
        self.decision.is_some_and(ReleaseDecision::new_run)
    }
}

/// What a release reads of a task and its latest run to choose how it
/// goes on: the task's status, the run's status and the provider it runs
/// on, whether it has a session a resume goes on in
/// ([`session_to_resume`]), and whether a recovery job's result or a
/// person's answer to it was kept for the run while it was held and is
/// not settled yet (`kept`, [`disposition`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseFacts {
    pub task: TaskStatus,
    pub run: Option<RunStatus>,
    pub run_provider: Option<Provider>,
    pub session: bool,
    pub kept: bool,
}

/// Whether the release of a task at `facts` with `choice` ends its run for
/// a new one: an `in_progress` task whose waiting run has no session to go
/// on in or is asked to go on with another provider, whose session it
/// cannot carry (a model alone does not: [`super::worker_model::check_model`]
/// refuses one a provider does not take), or whose run stopped (`failed`,
/// `interrupted`). A stopped run whose kept result the release leaves as
/// it is (no provider and no model chosen) goes on by that result instead,
/// applied after the release as when nothing moved.
pub fn ends_run(facts: ReleaseFacts, choice: &ReleaseChoice) -> bool {
    let Some(run) = facts.run else {
        return false;
    };
    let other_provider = choice
        .provider
        .is_some_and(|provider| Some(provider) != facts.run_provider);
    facts.task == TaskStatus::InProgress
        && match run {
            RunStatus::Failed | RunStatus::Interrupted => {
                choice.provider.is_some() || choice.model.is_some() || !facts.kept
            }
            _ if run.is_unfinished() => !facts.session || other_provider,
            _ => false,
        }
}

/// How the release of a task at `facts` with `choice` goes on;
/// `own_commits` says whether the run's branch holds commits of its own on
/// top of its base, read only when the run ends ([`ends_run`]).
pub fn release_decision(
    facts: ReleaseFacts,
    choice: &ReleaseChoice,
    own_commits: bool,
) -> ReleaseDecision {
    if facts.task != TaskStatus::InProgress {
        ReleaseDecision::Claim
    } else if !ends_run(facts, choice) {
        ReleaseDecision::Resume
    } else if own_commits {
        ReleaseDecision::RetryInherit
    } else {
        ReleaseDecision::RetryFresh
    }
}

/// Why a release ended the run at `facts` for a new one, choosing
/// `provider` ([`ends_run`]), in a person's words: the run's `last_error`
/// after `released: ` and the reason of the retry by hand that carries it
/// over.
pub fn new_run_reason(facts: ReleaseFacts, provider: Option<Provider>) -> String {
    match (facts.run, provider) {
        (Some(run @ (RunStatus::Failed | RunStatus::Interrupted)), _) => format!(
            "the hold was released and the run had stopped ({})",
            run.as_str()
        ),
        (_, Some(provider)) if Some(provider) != facts.run_provider => format!(
            "the hold was released with the provider changed to {}, which cannot go on in the run's session",
            provider.as_str()
        ),
        _ => "the hold was released and the run had no session to go on in".to_owned(),
    }
}

/// `replan_carry_ended`, which a release that ends a run for a new one
/// from the current main (`retry_fresh`) records on that run, in its own
/// transaction, when its task is a child of an applied replacement whose
/// saved work was still handed to it ([`replan_carry_live`]): no later run
/// is handed that work ([`super::resume::inherited_from`] stops at the
/// release), so its end is a record, not a silent clean-up (ADR-t1521-2
/// decision 2). The snapshot (`snapshot_id`) and its ref (`pinned_ref`)
/// are kept, for a person to hand the work over by hand; `ended_run` is
/// the run the release ended and `reason` says why in a person's words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplanCarryEnded {
    pub operation_id: ReplanId,
    pub snapshot_id: String,
    pub pinned_ref: String,
    pub carry: CarrySource,
    pub ended_run: RunId,
    pub reason: String,
}

impl ReplanCarryEnded {
    /// The end of `carry`'s handover to `task` that the release of its
    /// hold at `facts` choosing `provider` records as it ends `ended_run`
    /// afresh, the reason after the release's own ([`new_run_reason`]).
    pub fn new(
        carry: &ChildCarry,
        task: TaskId,
        ended_run: RunId,
        facts: ReleaseFacts,
        provider: Option<Provider>,
    ) -> Self {
        let pinned_ref = super::replan_snapshot::pinned_ref(carry.operation);
        let reason = format!(
            "{}, so the next run starts from the current main and the work replan operation {} saved in snapshot {} is handed to task {task} no more; the snapshot and {pinned_ref} are kept to hand it over by hand",
            new_run_reason(facts, provider),
            carry.operation,
            carry.snapshot_id,
        );
        Self {
            operation_id: carry.operation,
            snapshot_id: carry.snapshot_id.clone(),
            pinned_ref,
            carry: carry.carry,
            ended_run,
            reason,
        }
    }

    /// The end `event` records; `None` for another kind or a payload that
    /// does not restore.
    pub fn of_event(event: &RunEvent) -> Option<Self> {
        if event.kind != EventKind::ReplanCarryEnded {
            return None;
        }
        serde_json::from_value(event.payload.clone()).ok()
    }
}

/// Whether the handover of replacement `operation`'s saved work to a task
/// still goes on, given the events of the task's runs newest first
/// (`task_events`), read in the order [`super::resume::inherited_from`]
/// reads them: a record of the handover ([`ChildCarry::handover_of`]
/// `operation`) found before a release that ended a run afresh
/// (`retry_fresh`), an end of it already recorded
/// ([`ReplanCarryEnded::of_event`]), or a retry that carried a run's
/// branch over ([`super::resume::is_inherit_retry`]: the later runs are
/// handed that run's work instead, as `inherited_from` reads it). So a run
/// started afresh, handed nothing, and released afresh again ends nothing
/// a second time.
pub fn replan_carry_live<'a>(
    task_events: impl IntoIterator<Item = &'a RunEvent>,
    operation: ReplanId,
) -> bool {
    for event in task_events {
        let ended = ReplanCarryEnded::of_event(event)
            .is_some_and(|ended| ended.operation_id == operation)
            || Released::of_event(event)
                .is_some_and(|released| released.decision == Some(ReleaseDecision::RetryFresh))
            || super::resume::is_inherit_retry(event);
        if ended {
            return false;
        }
        if ChildCarry::handover_of(event) == Some(operation) {
            return true;
        }
    }
    false
}

/// Whether the run's worker has had a session of its provider that the
/// resume's entry goes on from, as the run's `events` tell by what the
/// provider said since it last said it had no session it was to resume
/// (Codex's thread gone), read as the execution context restores its turns
/// ([`RecordedTurn`]): a turn whose model answered, a session the provider
/// named (Codex's thread), or a turn, any of them, cut off before it ended
/// (its wrapper died in it, so its answer is unknown and the provider may
/// keep its session). A session started anew
/// since (a large context's, another provider's) leaves it so: the
/// resume's entry opens that one. A wrapper started, or turns that all
/// ended with no answer (one a hold fenced before it began among them),
/// leave no session.
pub fn session_to_resume(events: &[RunEvent]) -> bool {
    let turns: Vec<RecordedTurn> = events.iter().filter_map(RecordedTurn::restore).collect();
    let since = turns
        .iter()
        .rposition(|turn| *turn == RecordedTurn::Session { id: None })
        .map_or(&turns[..], |gone| &turns[gone + 1..]);
    let ended = |started: Option<u64>| {
        since
            .iter()
            .any(|turn| matches!(turn, RecordedTurn::Finished { turn, .. } if *turn == started))
    };
    since.iter().any(|turn| match turn {
        RecordedTurn::Finished { answered, .. } => *answered,
        RecordedTurn::Session { id } => id.is_some(),
        RecordedTurn::Started { turn } => !ended(*turn),
    })
}

/// What a release that ends a run for a new one carries over: the run's
/// `branch`, the commit of its own work (`head`: its own commits, with its
/// uncommitted work on top when there was some) and the commit that saved
/// the uncommitted work (`saved`), kept as `refs/dagq/runs/<run-id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleasedWork {
    pub branch: Option<String>,
    pub head: CommitSha,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved: Option<CommitSha>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
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
        Some(RunStatus::Integrated | RunStatus::Succeeded | RunStatus::Replaced) | None => {
            RunHoldWait::None
        }
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

/// Where a release that ends a run for a new one read the run's work,
/// before its store's transaction: the hold open then, the run and its
/// [`ProgressVersion`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseReadAt {
    pub hold: HoldId,
    pub run: RunId,
    pub version: ProgressVersion,
}

/// Whether a release may go on with what it read before its store's
/// transaction, now that the transaction finds the task's latest run at
/// `now` (`None` with no run) under its open hold `hold`. A release that
/// ends the run (`ends`) or read the run's work (`read`) goes on only with
/// the same hold open and the same run, which has not gone on: a hold
/// released, the run resumed or ended and held again in between would
/// otherwise end the newer hold and carry older work over, or drop the
/// work read. Refused, which changes nothing, also when the release ends
/// the run with no work read (`release_read_moved`).
pub fn check_release_read(
    task: TaskId,
    hold: HoldId,
    ends: bool,
    read: Option<&ReleaseReadAt>,
    now: Option<&ReleaseReadAt>,
) -> Result<(), DomainError> {
    if !ends && read.is_none() {
        return Ok(());
    }
    require(read.is_some() && read == now, || {
        DomainError::ReleaseReadMoved {
            task_id: task,
            hold,
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

/// A result a hold kept, read back from its `hold_result_kept` of any
/// version: where it came from (`None` for a source this binary does not
/// know or a record without one) and the ask a person's answer answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeptResult {
    pub source: Option<KeptSource>,
    pub ask: Option<AskId>,
}

impl KeptResult {
    /// The kept result `event` records; `None` for an event of another
    /// kind. A field it cannot read is left out, not an error.
    pub fn of_event(event: &RunEvent) -> Option<Self> {
        if event.kind != EventKind::HoldResultKept {
            return None;
        }
        let field = |key: &str| event.payload.get(key).cloned();
        Some(Self {
            source: field("source")
                .and_then(|value| serde_json::from_value::<String>(value).ok())
                .and_then(|source| source.parse().ok()),
            ask: field("ask_id").and_then(|value| serde_json::from_value(value).ok()),
        })
    }

    /// The ask a discard of the result closes: the one a person's answer
    /// answered; none for a job's verdict.
    pub const fn ask_to_close(self) -> Option<AskId> {
        match self.source {
            Some(KeptSource::TriageAnswer) => self.ask,
            _ => None,
        }
    }
}

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
const NOT_PROGRESS: [EventKind; 11] = [
    EventKind::TaskHeld,
    EventKind::TaskHoldReminded,
    EventKind::TaskReleased,
    EventKind::TaskHoldEnded,
    EventKind::HoldDeferred,
    EventKind::HoldResultKept,
    EventKind::HoldResultApplied,
    EventKind::HoldResultDiscarded,
    EventKind::LeaseAcquired,
    EventKind::LeaseReleased,
    EventKind::WorkspaceClosed,
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
                .find(|event| !event.kind.is_in(&NOT_PROGRESS))
                .map(|event| event.id),
        }
    }
}

// Why a kept result was discarded at its hold's release: the task or the
// run moved since it was kept, it was a verdict on a live session, which
// only the session's own watch can check again, the path that applies
// it refused it now (a precondition no longer holds), or the task ended,
// which leaves nothing for it to apply to ([`HoldEnd`]).
string_enum!(DiscardReason {
    Changed => "changed",
    LiveSession => "live_session",
    Refused => "refused",
    TaskEnded => "task_ended",
});

// Why a hold closed with its task (`task_hold_ended`'s `reason`): the task
// completed, was canceled or was replaced by a replanning.
string_enum!(HoldEnd {
    Completed => "completed",
    Canceled => "canceled",
    Replaced => "replaced",
});

impl HoldEnd {
    /// The end a task moved to `status` puts to its open hold: `None` for
    /// a status that has not ended the task, whose hold stays.
    pub const fn of(status: TaskStatus) -> Option<Self> {
        match status {
            TaskStatus::Completed => Some(Self::Completed),
            TaskStatus::Canceled => Some(Self::Canceled),
            TaskStatus::Replaced => Some(Self::Replaced),
            _ => None,
        }
    }

    /// What becomes of a result the hold kept when its task ends: it is
    /// discarded, whatever it was, as nothing is left for it to apply to.
    pub const fn kept_result(self) -> Disposition {
        Disposition::Discard(DiscardReason::TaskEnded)
    }
}

/// Whether `event` closes a hold: its release (`task_released`) or the end
/// of its task (`task_hold_ended`).
pub fn ends_hold(event: &RunEvent) -> bool {
    event.kind == EventKind::TaskReleased || event.kind == EventKind::TaskHoldEnded
}

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
/// and its run stand at `now`: discarded with a task that ended
/// ([`HoldEnd::kept_result`]).
pub fn disposition(
    source: KeptSource,
    kept: &ProgressVersion,
    now: &ProgressVersion,
) -> Disposition {
    if let Some(end) = HoldEnd::of(now.task_status) {
        end.kept_result()
    } else if source == KeptSource::LiveVerdict {
        Disposition::Discard(DiscardReason::LiveSession)
    } else if kept == now {
        Disposition::Apply
    } else {
        Disposition::Discard(DiscardReason::Changed)
    }
}

/// How long a hold with no `until` goes before a person is told of it,
/// and how long the queue keeps quiet about a hold after telling of it:
/// one day. Telling releases nothing.
pub const HOLD_REMIND_SECS: i64 = 24 * 60 * 60;

// Why a hold is told to a person: its `until` passed, or, with none, it
// has been on for [`HOLD_REMIND_SECS`].
string_enum!(HoldReminder {
    UntilPassed => "until_passed",
    LongHeld => "long_held",
});

impl TaskHold {
    /// When the hold is first due to be told: its `until`, or
    /// [`HOLD_REMIND_SECS`] after it was held.
    pub const fn first_due(&self) -> i64 {
        match self.until {
            Some(until) => until,
            None => self.created_at.saturating_add(HOLD_REMIND_SECS),
        }
    }

    /// Why the hold waits for a person at `now`: its first due time has
    /// passed ([`Self::first_due`]); `None` before it.
    pub const fn overdue(&self, now: i64) -> Option<HoldReminder> {
        if now < self.first_due() {
            None
        } else if self.until.is_some() {
            Some(HoldReminder::UntilPassed)
        } else {
            Some(HoldReminder::LongHeld)
        }
    }

    /// Whether to tell a person of the hold at `now`, `last` being when it
    /// was last told: once it is [overdue](Self::overdue), and after that
    /// once each [`HOLD_REMIND_SECS`], so one hold does not wake the inbox
    /// again before the next threshold.
    pub const fn reminder_due(&self, last: Option<i64>, now: i64) -> Option<HoldReminder> {
        match (self.overdue(now), last) {
            (Some(_), Some(last)) if now < last.saturating_add(HOLD_REMIND_SECS) => None,
            (cause, _) => cause,
        }
    }
}

/// The waits of a run for its task's hold in its `events` (ascending id),
/// in unix milliseconds: from each `run_phase_changed` into
/// [`Phase::Held`](super::run_phase::Phase::Held) to the next record of
/// another phase (the release writes it) or the hold's close
/// ([`ends_hold`]), whichever comes first; `None` as the end of a wait not over. The time from
/// `task_held` to that record is not among them: the run went on in the
/// phases it was in.
pub fn held_spans(events: &[RunEvent]) -> Vec<(i64, Option<i64>)> {
    use super::run_phase::{Phase, Recorded};
    let mut spans = Vec::new();
    let mut open: Option<i64> = None;
    for event in events {
        let Some(at) = super::stats::timestamp_millis(&event.created_at) else {
            continue;
        };
        let held = Recorded::of_event(event).map(|record| record.is(Phase::Held));
        match (open, held) {
            (None, Some(true)) => open = Some(at),
            (Some(start), Some(false)) => {
                spans.push((start, Some(at.max(start))));
                open = None;
            }
            (Some(start), None) if ends_hold(event) => {
                spans.push((start, Some(at.max(start))));
                open = None;
            }
            _ => {}
        }
    }
    spans.extend(open.map(|start| (start, None)));
    spans
}

/// The holds of a task with no run going in its `events` (ascending id),
/// in unix milliseconds: each `task_held` to the close ([`ends_hold`]) of
/// the same `hold_id`, `None` as the end of one still open. The fold lays
/// them over the task's time `ready` (its time before a claim).
pub fn task_hold_spans(events: &[RunEvent]) -> Vec<(i64, Option<i64>)> {
    let hold_id = |event: &RunEvent| event.payload["hold_id"].as_i64();
    let mut spans: Vec<(Option<i64>, i64, Option<i64>)> = Vec::new();
    for event in events {
        let Some(at) = super::stats::timestamp_millis(&event.created_at) else {
            continue;
        };
        if event.kind == EventKind::TaskHeld {
            spans.push((hold_id(event), at, None));
        } else if ends_hold(event)
            && let Some(span) = spans
                .iter_mut()
                .find(|(id, _, end)| end.is_none() && (id.is_none() || *id == hold_id(event)))
        {
            span.2 = Some(at.max(span.1));
        }
    }
    spans
        .into_iter()
        .map(|(_, start, end)| (start, end))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::event_kind::HOLD_RESULT_DISCARDED;
    use crate::domain::event_kind::HOLD_RESULT_KEPT;
    use crate::domain::event_kind::LEASE_RELEASED;
    use crate::domain::event_kind::TASK_HELD;
    use crate::domain::event_kind::TASK_HOLD_ENDED;
    use crate::domain::event_kind::TASK_HOLD_REMINDED;
    use crate::domain::event_kind::TASK_RELEASED;
    use crate::domain::event_kind::WORKSPACE_CLOSED;
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

    fn facts(run: Option<RunStatus>, session: bool) -> ReleaseFacts {
        ReleaseFacts {
            task: TaskStatus::InProgress,
            run,
            run_provider: run.map(|_| Provider::Claude),
            session,
            kept: false,
        }
    }

    fn choose(provider: Option<Provider>, model: Option<&str>) -> ReleaseChoice {
        ReleaseChoice {
            provider,
            model: model.map(str::to_owned),
        }
    }

    #[test]
    fn a_release_resumes_a_waiting_run_with_its_session_on_the_same_provider() {
        for run in [
            RunStatus::NeedsSession,
            RunStatus::AwaitingIntegration,
            RunStatus::Validating,
        ] {
            let facts = facts(Some(run), true);
            for choice in [
                choose(None, None),
                choose(Some(Provider::Claude), None),
                choose(None, Some("claude-sonnet-5")),
            ] {
                assert!(!ends_run(facts, &choice), "{run:?} {choice:?}");
                for own in [true, false] {
                    assert_eq!(
                        release_decision(facts, &choice, own),
                        ReleaseDecision::Resume,
                        "{run:?} {choice:?} {own}"
                    );
                }
            }
        }
        assert_eq!(ReleaseDecision::Resume.continuation(), "continue");
        assert!(!ReleaseDecision::Resume.new_run());
    }

    #[test]
    fn a_release_ends_the_run_for_another_provider_no_session_or_a_stopped_run() {
        let codex = choose(Some(Provider::Codex), None);
        let none = choose(None, None);
        let cases = [
            (facts(Some(RunStatus::NeedsSession), true), &codex),
            (facts(Some(RunStatus::AwaitingIntegration), true), &codex),
            (facts(Some(RunStatus::NeedsSession), false), &none),
            (facts(Some(RunStatus::Failed), true), &none),
            (facts(Some(RunStatus::Interrupted), true), &none),
        ];
        for (facts, choice) in cases {
            assert!(ends_run(facts, choice), "{facts:?} {choice:?}");
            assert_eq!(
                release_decision(facts, choice, true),
                ReleaseDecision::RetryInherit
            );
            assert_eq!(
                release_decision(facts, choice, false),
                ReleaseDecision::RetryFresh
            );
        }
        assert_eq!(ReleaseDecision::RetryInherit.continuation(), "new_run");
        assert!(ReleaseDecision::RetryFresh.new_run());
    }

    #[test]
    fn a_stopped_run_goes_on_by_its_kept_result_unless_the_release_chose_a_provider_or_a_model() {
        for run in [RunStatus::Failed, RunStatus::Interrupted] {
            let kept = ReleaseFacts {
                kept: true,
                ..facts(Some(run), true)
            };
            assert!(!ends_run(kept, &choose(None, None)), "{run:?}");
            assert_eq!(
                release_decision(kept, &choose(None, None), true),
                ReleaseDecision::Resume
            );
            for choice in [
                choose(Some(Provider::Claude), None),
                choose(None, Some("claude-sonnet-5")),
            ] {
                assert!(ends_run(kept, &choice), "{run:?} {choice:?}");
            }
        }
        // A kept result does not keep a run that has no session.
        let sessionless = ReleaseFacts {
            kept: true,
            ..facts(Some(RunStatus::NeedsSession), false)
        };
        assert!(ends_run(sessionless, &choose(None, None)));
    }

    #[test]
    fn a_new_run_says_why_the_run_ended() {
        let codex = Some(Provider::Codex);
        assert!(
            new_run_reason(facts(Some(RunStatus::Failed), true), codex)
                .contains("stopped (failed)")
        );
        assert!(
            new_run_reason(facts(Some(RunStatus::NeedsSession), true), codex)
                .contains("provider changed to codex")
        );
        assert!(
            new_run_reason(facts(Some(RunStatus::NeedsSession), false), None)
                .contains("no session")
        );
    }

    #[test]
    fn a_release_of_a_task_with_no_run_going_lifts_the_mark_only() {
        let codex = choose(Some(Provider::Codex), None);
        for task in [
            TaskStatus::Ready,
            TaskStatus::Submitted,
            TaskStatus::Completed,
        ] {
            let facts = ReleaseFacts {
                task,
                ..facts(Some(RunStatus::Failed), false)
            };
            assert!(!ends_run(facts, &codex), "{task:?}");
            assert_eq!(
                release_decision(facts, &codex, true),
                ReleaseDecision::Claim
            );
        }
        // A run that ended leaves nothing to end.
        assert!(!ends_run(facts(Some(RunStatus::Integrated), false), &codex));
        assert!(!ends_run(facts(None, false), &codex));
        assert_eq!(ReleaseDecision::Claim.continuation(), "claim");
    }

    #[test]
    fn a_session_to_resume_is_one_its_provider_answered_in_named_or_was_cut_off_in() {
        use crate::domain::event_kind::{
            EventKind, PROVIDER_SWITCHED, SESSION_RENEWED, TURN_FINISHED, TURN_SESSION_IDENTIFIED,
            TURN_STARTED, WORKSPACE_CREATED, WRAPPER_LAUNCHED,
        };
        let event = |kind: &str, payload: serde_json::Value| RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        };
        let plain = |kind: &str| event(kind, json!({}));
        let started = |turn: u64| event(TURN_STARTED, json!({"turn": turn}));
        let finished = |turn: u64, created: bool| {
            event(
                TURN_FINISHED,
                json!({"turn": turn, "session_created": created}),
            )
        };
        let named =
            |id: serde_json::Value| event(TURN_SESSION_IDENTIFIED, json!({"session_id": id}));
        let with = |events: &[RunEvent], more: &[RunEvent]| [events, more].concat();
        // A wrapper started, fenced by a hold before its first turn, or
        // turns that all ended with no answer: no session.
        let launched = [
            plain("run_claimed"),
            plain(WORKSPACE_CREATED),
            plain(WRAPPER_LAUNCHED),
        ];
        assert!(!session_to_resume(&launched));
        let withheld = plain(EventKind::TurnWithheld.as_str());
        assert!(!session_to_resume(&with(&launched, &[withheld])));
        let unanswered = with(&launched, &[started(1), finished(1, false)]);
        assert!(!session_to_resume(&unanswered));
        // A turn its model answered in (Claude's), a session the provider
        // named with no answer yet (Codex's thread), or a turn cut off
        // before it ended, the last or an earlier one, is one.
        let answered = with(&launched, &[started(1), finished(1, true)]);
        assert!(session_to_resume(&answered));
        let thread = with(
            &launched,
            &[started(1), named(json!("thread-1")), finished(1, false)],
        );
        assert!(session_to_resume(&thread));
        assert!(session_to_resume(&with(&launched, &[started(1)])));
        assert!(session_to_resume(&with(&unanswered, &[started(2)])));
        assert!(session_to_resume(&with(
            &launched,
            &[started(1), started(2), finished(2, false)]
        )));
        // An older turn's event with no answer recorded answered nothing,
        // and one that named no session id said the session was gone.
        let older = with(
            &launched,
            &[started(1), event(TURN_FINISHED, json!({"turn": 1}))],
        );
        assert!(!session_to_resume(&older));
        assert!(!session_to_resume(&with(
            &answered,
            &[event(TURN_SESSION_IDENTIFIED, json!({}))]
        )));
        // A thread the provider said is gone leaves none, whatever was said
        // in it, until it names another.
        let gone = with(
            &answered,
            &[started(2), finished(2, false), named(json!(null))],
        );
        assert!(!session_to_resume(&gone));
        assert!(!session_to_resume(&with(
            &thread,
            &[started(2), finished(2, false), named(json!(null))]
        )));
        assert!(session_to_resume(&with(&gone, &[named(json!("thread-2"))])));
        // A session started anew (another provider, or a large context)
        // goes on from the one before it, before its own first turn.
        for anew in [PROVIDER_SWITCHED, SESSION_RENEWED] {
            assert!(
                session_to_resume(&with(&answered, &[plain(anew)])),
                "{anew}"
            );
            assert!(!session_to_resume(&with(&unanswered, &[plain(anew)])));
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

    /// The handover of a replacement's saved work goes on while a record
    /// of it (`run_inherited` or `replan_carry_failed` of the operation) is
    /// found, newest first, before a fresh release, a retry that carried a
    /// run's branch over or a recorded end;
    /// another operation's records, an inheritance of a run and a release
    /// that goes on or carries work over are passed by.
    #[test]
    fn a_replan_carry_is_live_until_a_fresh_release_or_its_recorded_end() {
        let op = ReplanId::new(3);
        let with = |kind: &str, payload: serde_json::Value| RunEvent {
            payload,
            ..event(0, kind)
        };
        let inherited = with("run_inherited", json!({"replan_operation_id": 3}));
        let failed = with("replan_carry_failed", json!({"operation_id": 3}));
        let other_op = with("run_inherited", json!({"replan_operation_id": 4}));
        let retried = with("run_inherited", json!({"replan_operation_id": null}));
        let fresh = with(
            TASK_RELEASED,
            json!({"hold_id": 1, "decision": "retry_fresh"}),
        );
        let resumed = with(TASK_RELEASED, json!({"hold_id": 1, "decision": "resume"}));
        let inherit = with(
            TASK_RELEASED,
            json!({"hold_id": 1, "decision": "retry_inherit"}),
        );
        let end_of = |operation: i64| {
            let carry = ChildCarry {
                operation: ReplanId::new(operation),
                ..child_carry()
            };
            let end = ReplanCarryEnded::new(
                &carry,
                TaskId::new(7),
                RunId::new("r1").unwrap(),
                facts(Some(RunStatus::Failed), false),
                None,
            );
            with("replan_carry_ended", serde_json::to_value(end).unwrap())
        };
        let (ended, ended_other) = (end_of(3), end_of(4));
        // An inheritance written before the operation was recorded.
        let older = with("run_inherited", json!({"inherit_from_run": "r0"}));
        let live = |events: &[&RunEvent]| replan_carry_live(events.iter().copied(), op);
        assert!(live(&[&inherited]));
        assert!(live(&[&failed]));
        assert!(live(&[
            &resumed,
            &inherit,
            &retried,
            &ended_other,
            &inherited
        ]));
        assert!(!live(&[]));
        assert!(!live(&[&other_op, &retried, &older]));
        assert!(!live(&[&fresh, &inherited]));
        assert!(!live(&[&ended, &fresh, &failed]));
        // A run started afresh and handed nothing, released afresh again,
        // ends nothing a second time.
        assert!(!live(&[&ended, &fresh, &inherited]));
        assert!(!live(&[&resumed, &ended, &inherited]));
        // A run's branch carried over since: its work goes on that way.
        let carried = with(
            "triage_finished",
            json!({"action": "retry_inherit", "by": "user"}),
        );
        assert!(!live(&[&carried, &fresh, &inherited]));
        assert!(!live(&[&carried, &inherited]));
    }

    fn child_carry() -> ChildCarry {
        ChildCarry {
            operation: ReplanId::new(3),
            from_task: TaskId::new(1),
            from_run: RunId::new("r0").unwrap(),
            proposal: None,
            snapshot_id: "snap-1".into(),
            carry: CarrySource::SnapshotDiff,
            order: Some(1),
        }
    }

    /// The end a fresh release records names the operation, the snapshot
    /// and its ref, the run it ended, and why in a person's words: the
    /// release's own reason, then that the work goes to the task no more
    /// and what is kept. It reads back as written.
    #[test]
    fn the_end_of_a_handover_says_why_and_what_is_kept() {
        let end = ReplanCarryEnded::new(
            &child_carry(),
            TaskId::new(7),
            RunId::new("r1").unwrap(),
            facts(Some(RunStatus::Failed), false),
            None,
        );
        assert_eq!(
            (
                end.operation_id,
                end.snapshot_id.as_str(),
                end.pinned_ref.as_str(),
                end.carry,
                end.ended_run.as_str()
            ),
            (
                ReplanId::new(3),
                "snap-1",
                "refs/dagq/replans/3",
                CarrySource::SnapshotDiff,
                "r1"
            )
        );
        assert_eq!(
            end.reason,
            "the hold was released and the run had stopped (failed), so the next run starts from the current main and the work replan operation 3 saved in snapshot snap-1 is handed to task 7 no more; the snapshot and refs/dagq/replans/3 are kept to hand it over by hand"
        );
        let recorded = RunEvent {
            payload: serde_json::to_value(&end).unwrap(),
            ..event(1, "replan_carry_ended")
        };
        assert_eq!(ReplanCarryEnded::of_event(&recorded), Some(end));
        assert_eq!(
            ReplanCarryEnded::of_event(&RunEvent {
                payload: json!({"operation_id": 3}),
                ..event(1, "replan_carry_ended")
            }),
            None
        );
    }

    /// A release's read of the run's work stands only while the hold it
    /// read is open and the run has not gone on: another hold after a
    /// release, a resume under the same hold, another run or none, no read
    /// for a release that ends the run, or a read for one that no longer
    /// does, refuses it; the hold's and the leases' own records do not. A
    /// release that neither ends the run nor read its work goes on.
    #[test]
    fn a_release_carries_over_only_the_work_it_read_under_the_same_hold_and_progress() {
        let task = TaskId::new(7);
        let events = vec![event(1, "run_claimed"), event(2, "integration_deferred")];
        let at = |hold: i64, run: &str, events: &[RunEvent]| ReleaseReadAt {
            hold: HoldId::new(hold),
            run: RunId::new(run).unwrap(),
            version: ProgressVersion::of(
                TaskStatus::InProgress,
                "t1",
                RunStatus::NeedsSession,
                events,
            ),
        };
        let read = at(1, "r1", &events);
        let mut quiet = events.clone();
        quiet.extend([event(3, TASK_HOLD_REMINDED), event(4, LEASE_RELEASED)]);
        let quiet = at(1, "r1", &quiet);
        assert_eq!(
            check_release_read(task, HoldId::new(1), true, Some(&read), Some(&quiet)),
            Ok(())
        );
        assert_eq!(
            check_release_read(task, HoldId::new(1), false, None, Some(&quiet)),
            Ok(())
        );
        assert_eq!(
            check_release_read(task, HoldId::new(1), false, None, None),
            Ok(())
        );
        let mut resumed = events.clone();
        resumed.extend([event(3, "resume_started")]);
        let mut held_again = events.clone();
        held_again.extend([event(3, TASK_RELEASED), event(4, TASK_HELD)]);
        for (ends, read, now) in [
            (true, Some(&read), Some(at(1, "r1", &resumed))),
            (true, Some(&read), Some(at(2, "r1", &held_again))),
            (true, Some(&read), Some(at(2, "r1", &resumed))),
            (true, Some(&read), Some(at(1, "r2", &events))),
            (true, Some(&read), None),
            (true, None, Some(at(1, "r1", &events))),
            (false, Some(&read), Some(at(2, "r2", &events))),
        ] {
            let hold = now.as_ref().map_or(HoldId::new(1), |now| now.hold);
            let refused = check_release_read(task, hold, ends, read, now.as_ref()).unwrap_err();
            assert_eq!(
                refused,
                DomainError::ReleaseReadMoved {
                    task_id: task,
                    hold
                }
            );
            assert!(refused.to_string().contains("release_read_moved"));
        }
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

    /// A hold with no `until` is told a day after it was held, one with
    /// an `until` once that passed; after a reminder it is quiet until a
    /// day later, however often it is asked.
    #[test]
    fn a_hold_is_told_once_past_its_threshold_and_again_a_day_later() {
        let day = HOLD_REMIND_SECS;
        let long = hold(true);
        assert_eq!(long.first_due(), 100 + day);
        assert_eq!(long.overdue(100 + day - 1), None);
        assert_eq!(long.reminder_due(None, 100 + day - 1), None);
        assert_eq!(long.overdue(100 + day), Some(HoldReminder::LongHeld));
        assert_eq!(
            long.reminder_due(None, 100 + day),
            Some(HoldReminder::LongHeld)
        );
        let told = 100 + day + 5;
        assert_eq!(long.reminder_due(Some(told), told), None);
        assert_eq!(long.reminder_due(Some(told), told + day - 1), None);
        assert_eq!(
            long.reminder_due(Some(told), told + day),
            Some(HoldReminder::LongHeld)
        );
        // An `until` takes the place of the day, sooner or later.
        let soon = TaskHold {
            until: Some(160),
            ..hold(false)
        };
        assert_eq!(soon.reminder_due(None, 159), None);
        assert_eq!(
            soon.reminder_due(None, 160),
            Some(HoldReminder::UntilPassed)
        );
        assert_eq!(soon.reminder_due(Some(170), 170 + day - 1), None);
        let late = TaskHold {
            until: Some(100 + 3 * day),
            ..hold(true)
        };
        assert_eq!(late.reminder_due(None, 100 + 2 * day), None);
        assert_eq!(late.overdue(100 + 3 * day), Some(HoldReminder::UntilPassed));
    }

    fn at(id: i64, kind: &str, payload: serde_json::Value, secs: i64) -> RunEvent {
        RunEvent {
            payload,
            created_at: crate::domain::marks::utc_text(secs * 1000),
            ..event(id, kind)
        }
    }

    /// A run's wait for its hold runs from its `held` record to the next
    /// record of another phase or the release, whichever comes first; the
    /// time from `task_held` to the `held` record is not part of it.
    #[test]
    fn a_runs_wait_for_its_hold_runs_from_its_held_record_to_its_release() {
        let phase =
            |id, name: &str, secs| at(id, "run_phase_changed", json!({"phase": name}), secs);
        let events = [
            phase(1, "worker", 0),
            at(2, TASK_HELD, json!({"hold_id": 1}), 10),
            phase(3, "held", 40),
            at(4, TASK_RELEASED, json!({"hold_id": 1}), 100),
            phase(5, "needs_session", 100),
            at(6, TASK_HELD, json!({"hold_id": 2}), 200),
            phase(7, "held", 200),
            phase(8, "worker", 260),
            phase(9, "held", 300),
        ];
        assert_eq!(
            held_spans(&events),
            [
                (40_000, Some(100_000)),
                (200_000, Some(260_000)),
                (300_000, None)
            ]
        );
        // A task's holds run from `task_held` to its own release.
        assert_eq!(
            task_hold_spans(&events),
            [(10_000, Some(100_000)), (200_000, None)]
        );
    }

    /// A task's end puts its own end to its hold: completed or canceled,
    /// and nothing while it has not ended.
    #[test]
    fn only_a_task_that_ended_ends_its_hold_with_its_end() {
        assert_eq!(HoldEnd::of(TaskStatus::Completed), Some(HoldEnd::Completed));
        assert_eq!(HoldEnd::of(TaskStatus::Canceled), Some(HoldEnd::Canceled));
        for status in [
            TaskStatus::Draft,
            TaskStatus::Submitted,
            TaskStatus::Ready,
            TaskStatus::InProgress,
        ] {
            assert_eq!(HoldEnd::of(status), None, "{status:?}");
        }
        assert_eq!(HoldEnd::Completed.as_str(), "completed");
        assert_eq!(HoldEnd::Canceled.as_str(), "canceled");
    }

    /// A result kept for a task that ended is discarded as `task_ended`,
    /// whatever its source and whether anything else moved.
    #[test]
    fn a_result_kept_for_a_task_that_ended_is_discarded() {
        for end in [HoldEnd::Completed, HoldEnd::Canceled] {
            assert_eq!(
                end.kept_result(),
                Disposition::Discard(DiscardReason::TaskEnded)
            );
        }
        let events = vec![event(1, "run_claimed")];
        let kept = ProgressVersion::of(TaskStatus::InProgress, "t1", RunStatus::Failed, &events);
        for status in [TaskStatus::Completed, TaskStatus::Canceled] {
            let ended = ProgressVersion::of(status, "t2", RunStatus::Failed, &events);
            for source in [
                KeptSource::RecoveryVerdict,
                KeptSource::TriageAnswer,
                KeptSource::LiveVerdict,
            ] {
                assert_eq!(
                    disposition(source, &kept, &ended),
                    Disposition::Discard(DiscardReason::TaskEnded)
                );
            }
        }
        assert_eq!(DiscardReason::TaskEnded.as_str(), "task_ended");
    }

    /// The end of a task closes its hold's spans as a release does: the
    /// run's wait and the task's hold end at `task_hold_ended`, which is
    /// none of the run's progress.
    #[test]
    fn a_task_hold_ended_closes_the_holds_spans_like_a_release() {
        let phase =
            |id, name: &str, secs| at(id, "run_phase_changed", json!({"phase": name}), secs);
        let ended = at(3, TASK_HOLD_ENDED, json!({"hold_id": 1}), 90);
        assert!(ends_hold(&ended));
        assert!(ends_hold(&event(1, TASK_RELEASED)));
        assert!(!ends_hold(&event(1, TASK_HELD)));
        let events = [
            at(1, TASK_HELD, json!({"hold_id": 1}), 10),
            phase(2, "held", 40),
            ended,
        ];
        assert_eq!(held_spans(&events), [(40_000, Some(90_000))]);
        assert_eq!(task_hold_spans(&events), [(10_000, Some(90_000))]);
        let before = vec![event(1, "run_claimed")];
        let mut after = before.clone();
        after.push(event(2, TASK_HOLD_ENDED));
        assert_eq!(
            ProgressVersion::of(TaskStatus::InProgress, "t", RunStatus::Failed, &before),
            ProgressVersion::of(TaskStatus::InProgress, "t", RunStatus::Failed, &after)
        );
    }

    /// A kept result is read back typed from its record of any version: a
    /// person's answer names the ask a discard closes, a job's verdict
    /// none; an unknown or missing source, or an ask that is not an id,
    /// reads as none of them; another kind is no kept result.
    #[test]
    fn a_kept_result_is_read_back_typed_and_names_the_ask_of_an_answer() {
        let kept = |value: serde_json::Value| {
            KeptResult::of_event(&payload(1, HOLD_RESULT_KEPT, value)).unwrap()
        };
        let answer = kept(json!({"source": "triage_answer", "ask_id": 5, "answer": "retry"}));
        assert_eq!(answer.source, Some(KeptSource::TriageAnswer));
        assert_eq!(answer.ask_to_close(), Some(AskId::new(5)));
        let verdict = kept(json!({"source": "recovery_verdict", "action": {"action": "retry"}}));
        assert_eq!(verdict.source, Some(KeptSource::RecoveryVerdict));
        assert_eq!(verdict.ask_to_close(), None);
        assert_eq!(
            kept(json!({"source": "live_verdict", "ask_id": 5})).ask_to_close(),
            None
        );
        for unread in [
            json!({"source": "someday_source", "ask_id": 5}),
            json!({"ask_id": 5}),
            json!({"source": 3, "ask_id": 5}),
        ] {
            let result = kept(unread.clone());
            assert_eq!(result.source, None, "{unread}");
            assert_eq!(result.ask_to_close(), None, "{unread}");
        }
        let odd = kept(json!({"source": "triage_answer", "ask_id": "five"}));
        assert_eq!(odd.source, Some(KeptSource::TriageAnswer));
        assert_eq!(odd.ask_to_close(), None);
        assert_eq!(
            KeptResult::of_event(&payload(1, HOLD_RESULT_DISCARDED, json!({"ask_id": 5}))),
            None
        );
    }
}
