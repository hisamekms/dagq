//! The phases a run's life is recorded in (ADR-t1662-1 decisions 1 to 4
//! and 16, docs/design/measurement.md "区間とタグ"). Each time a run moves
//! to another phase, `run_phase_changed` records the phase it enters, so
//! that the records, each closing the one before, cover the run from its
//! claim to the end of its push without gaps. Statistics count by a
//! phase's tags, not by its name: [`Phase::tags`] is next to the phases,
//! and its exhaustive `match` stops a phase added without tags from
//! compiling.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{EventKind, RecoveredLanding, RunEvent, RunHistory, RunStatus, event_kind};

/// The version of the recording rules a `run_phase_changed` was written by
/// (its `v`): raised when what a phase or a tag means changes.
pub const RULES_VERSION: u32 = 1;

/// What decides when the run moves on (ADR-t1662-1 decision 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Blocker {
    /// The run waits for a slot or its turn to land.
    Queue,
    /// An agent works on it.
    Ai,
    /// Builds, tests and checks compute.
    Compute,
    /// A person's answer.
    Human,
    /// dagq's own steps and the intervals it watches at.
    Runtime,
    /// A service outside: GitHub, the remote, a provider's API.
    External,
    /// The run's environment: its provisioning, a failure, short resources
    /// (ADR-t1662-3 decision 5).
    Infra,
}

/// The slot the run holds meanwhile, as the supervisor counts its slots
/// (ADR-t1591-1: a run waiting only for its landing turn holds none): a
/// worker slot, the landing slot, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Holds {
    WorkerSlot,
    LandingSlot,
    None,
}

/// A phase's tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tags {
    pub blocker: Blocker,
    pub holds: Holds,
}

/// What the run tries the work as: its first try, or one taken up again
/// and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptKind {
    First,
    /// The live session fixes a `revise` verdict.
    Revise,
    /// The live session fixes a conflict with main.
    Conflict,
    /// A resumed session of a `needs_session` run.
    Resume,
}

/// The run's attempt: `n` counts the attempts of its kind, from 1, as the
/// runtime numbers its revises, conflict requests and resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub kind: AttemptKind,
    pub n: usize,
}

impl Attempt {
    pub const FIRST: Self = Self {
        kind: AttemptKind::First,
        n: 1,
    };

    pub const fn of(kind: AttemptKind, n: usize) -> Self {
        Self { kind, n }
    }
}

/// A phase of a run. The names are free: statistics read the tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Claimed, its worktree and session being provisioned.
    Provisioning,
    /// A wait its environment adds that the host does not have
    /// (ADR-t1662-3 decision 5): its placement, its image's pull, its
    /// preparation or the return of its results.
    Environment(super::environment::EnvironmentWait),
    /// The worker's session works.
    Worker,
    /// The runtime validates the receipt.
    Validating,
    /// The review job runs.
    Review,
    /// The review waits for the hold of its provider to end.
    ReviewHeld,
    /// The live session fixes a `revise` verdict or a conflict.
    Revise,
    /// The session is asked to exit and its workspace closed.
    Exiting,
    /// A resumed session works.
    Resume,
    /// The recovery job of a failed or interrupted run runs.
    Recovery,
    /// The run waits for a person's answer: outside the slots (ADR-0062),
    /// a `worker_question` among them, or, `in_slot`, a first session, a
    /// revise or a resume stopped at its own `worker_question` that the
    /// waiting limit keeps in its slot.
    Waiting { in_slot: bool },
    /// The task's verification runs on a snapshot of main for the run's
    /// `already_satisfied` claim (ADR-t1818-1), in its slot.
    Verifying,
    /// The claim rests out of the slots for the answer to its
    /// `approve_close` ask.
    ClaimAnswer,
    /// The claim rests out of the slots, no answer awaited, until its
    /// verification may run: a slot free and the container the
    /// verification is set to usable, as a landing waits for both.
    ClaimHeld,
    /// Its wait ended, and it waits to go back to a slot.
    Returning,
    /// A run to land waits in its slot to be decided: its e2e, the holds
    /// of a landing, the integration slot.
    AwaitingSlot,
    /// Its review passed and it waits in its slot for the dev of the agent
    /// definitions it changes (ADR-t1728-1 decision 10), its session open:
    /// the round waits for its turn or runs.
    AgentEval,
    /// It waits for another run's e2e or to try its own again.
    AwaitingE2e,
    /// Its e2e runs on the host.
    E2e,
    /// It waits only for its landing turn, outside the slots
    /// (ADR-t1591-1), or approved and queued with no lease.
    LandingQueue,
    /// It lands: rebase, verification and the move of main.
    Landing,
    /// It waits for a person: the answer to its `approve_landing` ask, or
    /// a landing given back to a person.
    LandingAnswer,
    /// It waits for a resume (`needs_session`).
    NeedsSession,
    /// Its task's hold reached it and nothing of it goes (ADR-t1879-1): the
    /// step that went when the task was held ended and the next did not
    /// start, until a person releases the hold. The time between the hold
    /// (`task_held`) and this phase stays in the phases the run went on
    /// in, and the release closes it with the phase the run waits in then.
    Held,
    /// Landed (`run_integrated`), main is pushed. `in_slot` is whether the
    /// supervisor's slot still holds the run (a person's `integrate` holds
    /// none).
    Push { in_slot: bool },
    /// Its push failed: a later push of the same branch, or a person,
    /// delivers it.
    PushPending,
    /// The run's life ended: pushed or its push skipped, failed,
    /// interrupted or canceled (the `cause` says which). A recovery job of
    /// a failed run moves it on again.
    Ended,
}

impl Phase {
    /// The name `run_phase_changed` records.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Environment(wait) => wait.phase_name(),
            Self::Worker => "worker",
            Self::Validating => "validating",
            Self::Review => "review",
            Self::ReviewHeld => "review_held",
            Self::Revise => "revise",
            Self::Exiting => "exiting",
            Self::Resume => "resume",
            Self::Recovery => "recovery",
            Self::Waiting { .. } => "waiting",
            Self::Verifying => "verifying",
            Self::ClaimAnswer => "claim_answer",
            Self::ClaimHeld => "claim_held",
            Self::Returning => "returning",
            Self::AwaitingSlot => "awaiting_slot",
            Self::AgentEval => "agent_eval",
            Self::AwaitingE2e => "awaiting_e2e",
            Self::E2e => "e2e",
            Self::LandingQueue => "landing_queue",
            Self::Landing => "landing",
            Self::LandingAnswer => "landing_answer",
            Self::NeedsSession => "needs_session",
            Self::Held => "held",
            Self::Push { .. } => "push",
            Self::PushPending => "push_pending",
            Self::Ended => "ended",
        }
    }

    /// The phase's tags.
    pub const fn tags(self) -> Tags {
        use Blocker as B;
        use Holds as H;
        let (blocker, holds) = match self {
            Self::Provisioning | Self::Environment(_) => (B::Infra, H::WorkerSlot),
            Self::Worker
            | Self::Review
            | Self::Revise
            | Self::Resume
            | Self::Recovery
            | Self::AgentEval => (B::Ai, H::WorkerSlot),
            Self::Validating | Self::Exiting => (B::Runtime, H::WorkerSlot),
            Self::ReviewHeld => (B::External, H::WorkerSlot),
            Self::Waiting { in_slot: false }
            | Self::LandingAnswer
            | Self::ClaimAnswer
            | Self::PushPending
            | Self::Held => (B::Human, H::None),
            Self::Waiting { in_slot: true } => (B::Human, H::WorkerSlot),
            Self::Verifying => (B::Compute, H::WorkerSlot),
            Self::Returning | Self::LandingQueue | Self::NeedsSession | Self::ClaimHeld => {
                (B::Queue, H::None)
            }
            Self::AwaitingSlot | Self::AwaitingE2e => (B::Queue, H::WorkerSlot),
            Self::E2e => (B::Compute, H::WorkerSlot),
            Self::Landing => (B::Compute, H::LandingSlot),
            Self::Push { in_slot: true } => (B::External, H::WorkerSlot),
            Self::Push { in_slot: false } => (B::External, H::None),
            Self::Ended => (B::Runtime, H::None),
        };
        Tags { blocker, holds }
    }

    /// The phase of a run no slot holds, by its status: `queued` is
    /// whether a run awaiting integration is approved and queued to land
    /// (else it waits for a person). `None` for a run still on its way
    /// (another process takes it up and records), and for a landed one,
    /// whose push the landing records.
    pub fn at_rest(status: RunStatus, queued: bool) -> Option<Self> {
        match status {
            RunStatus::NeedsSession => Some(Self::NeedsSession),
            RunStatus::AwaitingIntegration if queued => Some(Self::LandingQueue),
            RunStatus::AwaitingIntegration => Some(Self::LandingAnswer),
            RunStatus::Failed | RunStatus::Interrupted | RunStatus::Succeeded => Some(Self::Ended),
            RunStatus::Claimed
            | RunStatus::Starting
            | RunStatus::Running
            | RunStatus::Validating
            | RunStatus::Integrating
            | RunStatus::Integrated => None,
        }
    }
}

impl Phase {
    /// The phase of a run no slot holds, by its status and its `events`:
    /// [`Self::at_rest`], except that a `validating` run whose
    /// `already_satisfied` claim rests ([`super::satisfied::rests`]) waits
    /// for the answer to its ask ([`super::satisfied::answer_awaited`]), or
    /// else for a slot or the container its verification is set to; and
    /// that a run whose hold's wait was recorded ([`Self::Held`]) and not
    /// released since is still in it; and that a run awaiting integration
    /// a supervisor takes up again waits to go back to a slot
    /// ([`Self::awaiting_at_rest`]).
    pub fn resting(status: RunStatus, events: &[RunEvent]) -> Option<Self> {
        if status.is_unfinished() && held_unreleased(events) {
            return Some(Self::Held);
        }
        if status == RunStatus::Validating && super::satisfied::rests(events) {
            return Some(if super::satisfied::answer_awaited(events) {
                Self::ClaimAnswer
            } else {
                Self::ClaimHeld
            });
        }
        Self::awaiting_at_rest(status, events).or_else(|| {
            Self::at_rest(
                status,
                RunHistory::from_events(events).queued_approval().is_some(),
            )
        })
    }

    /// The phase of a run awaiting integration that no lease holds and no
    /// `land` answer queued, when its events leave it to a supervisor to
    /// take up again ([`super::stranded_landing::kept`], ADR-t3049-1): it
    /// waits to go back to a slot, not for a person.
    fn awaiting_at_rest(status: RunStatus, events: &[RunEvent]) -> Option<Self> {
        (status == RunStatus::AwaitingIntegration
            && RunHistory::from_events(events).queued_approval().is_none()
            && super::stranded_landing::kept(events).is_none())
        .then_some(Self::Returning)
    }
}

impl Phase {
    /// The phase the push's outcome `kind` (`push_finished`,
    /// `push_skipped` or `push_failed`) moves a landed run to, and its
    /// cause (ADR-t1662-1 decision 16): its life ends once main is pushed
    /// or the push skipped; a failed push leaves it `push_pending` for a
    /// later push or a person.
    pub fn after_push(kind: EventKind) -> (Self, &'static str) {
        match kind {
            EventKind::PushFailed => (Self::PushPending, event_kind::PUSH_FAILED),
            EventKind::PushSkipped => (Self::Ended, event_kind::PUSH_SKIPPED),
            _ => (Self::Ended, PUSHED),
        }
    }
}

impl Phase {
    /// The phase a landing that gave its slot back before main moved
    /// leaves the run in, by the status it left the run in and the run's
    /// `events` (whether it is approved and queued to land): as
    /// [`Self::at_rest`], `None` for a run another process takes up; a run
    /// whose hold settled as it came to rest is [`Self::Held`].
    pub fn after_landing(status: RunStatus, events: &[RunEvent]) -> Option<Self> {
        if status.is_unfinished() && held_unreleased(events) {
            return Some(Self::Held);
        }
        let queued = RunHistory::from_events(events).queued_approval().is_some();
        Self::awaiting_at_rest(status, events).or_else(|| Self::at_rest(status, queued))
    }

    /// The phase a recovery (`run_recovered`, already among the run's
    /// `events`) leaves the run in, by the status it left the run in; a run
    /// whose hold settled as it came to rest is [`Self::Held`]. A run
    /// awaiting integration whose landing was given up and not moved on
    /// from ([`RunHistory::recovered_landing`]) is in the phase that
    /// recovery set, however often it is recovered again: one it lands
    /// again waits for its landing turn, one it reviews again waits for a
    /// slot to come back to. Any other run is [`Self::at_rest`] by its
    /// status and whether an `approve_landing` answer queued it to land.
    pub fn after_recovery(status: RunStatus, events: &[RunEvent]) -> Option<Self> {
        if status.is_unfinished() && held_unreleased(events) {
            return Some(Self::Held);
        }
        let history = RunHistory::from_events(events);
        match history.recovered_landing() {
            Some(RecoveredLanding::Land(_)) if status == RunStatus::AwaitingIntegration => {
                Some(Self::LandingQueue)
            }
            Some(RecoveredLanding::Review(_)) if status == RunStatus::AwaitingIntegration => {
                Some(Self::Returning)
            }
            _ => Self::awaiting_at_rest(status, events)
                .or_else(|| Self::at_rest(status, history.queued_approval().is_some())),
        }
    }
}

/// Whether the last `run_phase_changed` among `events` entered
/// [`Phase::Held`] and no close of the hold came after it
/// ([`super::task_hold::ends_hold`]).
fn held_unreleased(events: &[RunEvent]) -> bool {
    events
        .iter()
        .rev()
        .take_while(|event| !super::task_hold::ends_hold(event))
        .find(|event| event.kind == EventKind::RunPhaseChanged)
        .is_some_and(|event| event.payload["phase"] == Phase::Held.name())
}

/// The change that moves the run whose `events` these are to `phase`,
/// which `cause` moved it to, in the attempt last recorded (the first
/// when none is); `None` when it repeats the last record.
pub fn next_change(events: &[RunEvent], phase: Phase, cause: &str) -> Option<PhaseChange> {
    let last = last_recorded(events);
    let attempt = last
        .as_ref()
        .map_or(Attempt::FIRST, |recorded| recorded.attempt);
    let change = PhaseChange::new(phase, attempt, cause);
    (!change.repeats(last.as_ref())).then_some(change)
}

/// The cause of the end of a run whose push delivered main.
pub const PUSHED: &str = "pushed";

/// One `run_phase_changed`: the phase entered, the attempt, and its cause
/// (the kind of the event that moved the run, or a reason code).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseChange {
    pub phase: Phase,
    pub attempt: Attempt,
    pub cause: String,
    /// In [`Phase::LandingQueue`], the run that held the landing slot
    /// meanwhile (ADR-t1662-1 decision 6), when the supervisor saw one.
    pub blocked_by: Option<String>,
}

impl PhaseChange {
    pub fn new(phase: Phase, attempt: Attempt, cause: impl Into<String>) -> Self {
        Self {
            phase,
            attempt,
            cause: cause.into(),
            blocked_by: None,
        }
    }

    /// The change with `blocked_by` as the run that holds the landing
    /// slot, kept only in [`Phase::LandingQueue`].
    pub fn blocked_by(mut self, run: Option<String>) -> Self {
        self.blocked_by = run.filter(|_| self.phase == Phase::LandingQueue);
        self
    }

    /// The payload `{phase, blocker, holds, attempt, cause, v}`, and
    /// `blocked_by` when there is one.
    pub fn payload(&self) -> Value {
        let tags = self.phase.tags();
        let mut payload = json!({
            "phase": self.phase.name(),
            "blocker": tags.blocker,
            "holds": tags.holds,
            "attempt": self.attempt,
            "cause": self.cause,
            "v": RULES_VERSION,
        });
        if let Some(run) = &self.blocked_by {
            payload["blocked_by"] = json!(run);
        }
        payload
    }

    /// Whether it records the same phase, attempt, landing slot's holder
    /// and slot held as `recorded` (a phase of one name may hold a slot or
    /// none, [`Phase::Waiting`]).
    pub fn repeats(&self, recorded: Option<&Recorded>) -> bool {
        recorded.is_some_and(|r| {
            r.phase == self.phase.name()
                && r.attempt == self.attempt
                && r.blocked_by == self.blocked_by
                && r.holds.is_none_or(|holds| holds == self.phase.tags().holds)
        })
    }

    /// What this change records, as [`Recorded::of`] reads it back.
    pub fn recorded(&self) -> Recorded {
        Recorded {
            phase: self.phase.name().to_owned(),
            attempt: self.attempt,
            blocked_by: self.blocked_by.clone(),
            holds: Some(self.phase.tags().holds),
        }
    }
}

/// What a recorded `run_phase_changed` says of the phase, the attempt, the
/// landing slot's holder and the slot held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub phase: String,
    pub attempt: Attempt,
    pub blocked_by: Option<String>,
    /// The slot it held; `None` when the payload does not say.
    pub holds: Option<Holds>,
}

impl Recorded {
    /// The phase and attempt of a `run_phase_changed`'s payload; a payload
    /// without an attempt it can read counts as the first.
    pub fn of(payload: &Value) -> Option<Self> {
        Some(Self {
            phase: payload["phase"].as_str()?.to_owned(),
            attempt: serde_json::from_value(payload["attempt"].clone()).unwrap_or(Attempt::FIRST),
            blocked_by: payload["blocked_by"].as_str().map(str::to_owned),
            holds: serde_json::from_value(payload["holds"].clone()).ok(),
        })
    }

    /// What `event` records, when it is a `run_phase_changed` ([`Self::of`]).
    pub fn of_event(event: &RunEvent) -> Option<Self> {
        if event.kind == EventKind::RunPhaseChanged {
            Self::of(&event.payload)
        } else {
            None
        }
    }

    /// Whether it records `phase`.
    pub fn is(&self, phase: Phase) -> bool {
        self.phase == phase.name()
    }
}

/// The last `run_phase_changed` among a run's `events`.
pub fn last_recorded(events: &[RunEvent]) -> Option<Recorded> {
    events
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::RunPhaseChanged)
        .and_then(|event| Recorded::of(&event.payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::environment::EnvironmentWait;

    const ALL: [Phase; 32] = [
        Phase::Provisioning,
        Phase::Environment(EnvironmentWait::Placement),
        Phase::Environment(EnvironmentWait::ImagePull),
        Phase::Environment(EnvironmentWait::Preparation),
        Phase::Environment(EnvironmentWait::ResultReturn),
        Phase::Worker,
        Phase::Validating,
        Phase::Review,
        Phase::ReviewHeld,
        Phase::Revise,
        Phase::Exiting,
        Phase::Resume,
        Phase::Recovery,
        Phase::Waiting { in_slot: false },
        Phase::Waiting { in_slot: true },
        Phase::Verifying,
        Phase::ClaimAnswer,
        Phase::ClaimHeld,
        Phase::Returning,
        Phase::AwaitingSlot,
        Phase::AgentEval,
        Phase::AwaitingE2e,
        Phase::E2e,
        Phase::LandingQueue,
        Phase::Landing,
        Phase::LandingAnswer,
        Phase::NeedsSession,
        Phase::Held,
        Phase::Push { in_slot: true },
        Phase::Push { in_slot: false },
        Phase::PushPending,
        Phase::Ended,
    ];

    fn tags(blocker: Blocker, holds: Holds) -> Tags {
        Tags { blocker, holds }
    }

    /// The representative tags: the agent's work holds a worker slot, the
    /// landing the landing slot, and every wait for a person (an
    /// `approve_landing` answer among them) holds none.
    #[test]
    fn phases_carry_their_tags() {
        use Blocker as B;
        use Holds as H;
        for (phase, expected) in [
            (Phase::Worker, tags(B::Ai, H::WorkerSlot)),
            (Phase::Validating, tags(B::Runtime, H::WorkerSlot)),
            (Phase::Landing, tags(B::Compute, H::LandingSlot)),
            (Phase::LandingQueue, tags(B::Queue, H::None)),
            (Phase::AwaitingSlot, tags(B::Queue, H::WorkerSlot)),
            (Phase::LandingAnswer, tags(B::Human, H::None)),
            (Phase::Waiting { in_slot: false }, tags(B::Human, H::None)),
            (
                Phase::Waiting { in_slot: true },
                tags(B::Human, H::WorkerSlot),
            ),
            (Phase::Verifying, tags(B::Compute, H::WorkerSlot)),
            (Phase::ClaimAnswer, tags(B::Human, H::None)),
            (Phase::ClaimHeld, tags(B::Queue, H::None)),
            (Phase::PushPending, tags(B::Human, H::None)),
            (Phase::Held, tags(B::Human, H::None)),
            (
                Phase::Push { in_slot: true },
                tags(B::External, H::WorkerSlot),
            ),
            (Phase::Push { in_slot: false }, tags(B::External, H::None)),
            (Phase::Provisioning, tags(B::Infra, H::WorkerSlot)),
            (
                Phase::Environment(EnvironmentWait::ImagePull),
                tags(B::Infra, H::WorkerSlot),
            ),
        ] {
            assert_eq!(phase.tags(), expected, "{}", phase.name());
        }
    }

    /// Each phase has its own name, but the two pushes and the two waits,
    /// which differ only in the slot they hold.
    #[test]
    fn phase_names_are_distinct() {
        let mut names: Vec<&str> = ALL.iter().map(|phase| phase.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ALL.len() - 2);
    }

    #[test]
    fn a_run_at_rest_is_in_the_phase_of_its_status() {
        let at_rest = |status, queued| Phase::at_rest(status, queued);
        assert_eq!(
            at_rest(RunStatus::NeedsSession, false),
            Some(Phase::NeedsSession)
        );
        assert_eq!(
            at_rest(RunStatus::AwaitingIntegration, false),
            Some(Phase::LandingAnswer)
        );
        assert_eq!(
            at_rest(RunStatus::AwaitingIntegration, true),
            Some(Phase::LandingQueue)
        );
        for status in [
            RunStatus::Failed,
            RunStatus::Interrupted,
            RunStatus::Succeeded,
        ] {
            assert_eq!(at_rest(status, false), Some(Phase::Ended));
        }
        // The landing records its push; a run on its way is another
        // process's to record.
        for status in [
            RunStatus::Integrated,
            RunStatus::Running,
            RunStatus::Validating,
        ] {
            assert_eq!(at_rest(status, false), None);
        }
    }

    /// The landing queue names the run that held the landing slot; a new
    /// holder is a new record, and no other phase keeps one.
    #[test]
    fn the_landing_queue_records_who_held_the_landing_slot() {
        let held = |run: &str| {
            PhaseChange::new(Phase::LandingQueue, Attempt::FIRST, "landing_turn")
                .blocked_by(Some(run.to_owned()))
        };
        let first = held("r1");
        assert_eq!(first.payload()["blocked_by"], "r1");
        let recorded = Recorded::of(&first.payload()).unwrap();
        assert_eq!(recorded, first.recorded());
        assert!(held("r1").repeats(Some(&recorded)));
        assert!(!held("r2").repeats(Some(&recorded)));
        let worker =
            PhaseChange::new(Phase::Worker, Attempt::FIRST, "x").blocked_by(Some("r1".into()));
        assert_eq!(worker.blocked_by, None);
        assert!(worker.payload().get("blocked_by").is_none());
    }

    /// A wait that leaves its slot is a new record though its name is the
    /// same; a record from before `holds` was read repeats by its name.
    #[test]
    fn a_wait_that_leaves_its_slot_is_a_new_record() {
        let attempt = Attempt::of(AttemptKind::Revise, 2);
        let in_slot = PhaseChange::new(Phase::Waiting { in_slot: true }, attempt, "ask_opened");
        let out = PhaseChange::new(
            Phase::Waiting { in_slot: false },
            attempt,
            "run_waiting_started",
        );
        let recorded = Recorded::of(&in_slot.payload()).unwrap();
        assert_eq!(recorded, in_slot.recorded());
        assert_eq!(recorded.holds, Some(Holds::WorkerSlot));
        assert!(in_slot.repeats(Some(&recorded)));
        assert!(!out.repeats(Some(&recorded)));
        let mut unread = in_slot.payload();
        unread.as_object_mut().unwrap().remove("holds");
        assert!(out.repeats(Recorded::of(&unread).as_ref()));
    }

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: crate::domain::EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    /// A landing that gave its slot back leaves the run at rest by its
    /// status; one queued by an `approve_landing` answer is not queued any
    /// more once its landing started.
    #[test]
    fn a_landing_given_back_leaves_the_run_at_rest() {
        let approved = event(
            1,
            event_kind::INTEGRATION_APPROVED,
            json!({"ask_id": 3, "answer": "land"}),
        );
        let started = event(2, event_kind::INTEGRATION_STARTED, json!({}));
        let after = |status| Phase::after_landing(status, &[approved.clone(), started.clone()]);
        assert_eq!(
            after(RunStatus::AwaitingIntegration),
            Some(Phase::LandingAnswer)
        );
        assert_eq!(after(RunStatus::NeedsSession), Some(Phase::NeedsSession));
        assert_eq!(after(RunStatus::Failed), Some(Phase::Ended));
        assert_eq!(after(RunStatus::Integrated), None);
    }

    /// A passed run whose supervisor stopped and gave its lease back as it
    /// waited for its landing (ADR-t3049-1) waits to go back to a slot,
    /// for the supervisor that takes it up, not for a person; one an ask
    /// was opened for after its review waits for a person's answer.
    #[test]
    fn a_passed_run_given_back_waits_to_go_back_unless_a_person_was_asked() {
        let given_back = vec![
            event(1, event_kind::REVIEW_FINISHED, json!({"verdict": "pass"})),
            event(2, event_kind::EXIT_REQUESTED, json!({})),
            event(3, event_kind::LANDING_QUEUED, json!({"via": "exit"})),
            event(4, event_kind::LEASE_RELEASED, json!({"reason": "finished"})),
        ];
        let awaiting = RunStatus::AwaitingIntegration;
        assert_eq!(
            Phase::resting(awaiting, &given_back),
            Some(Phase::Returning)
        );
        assert_eq!(
            Phase::after_landing(awaiting, &given_back),
            Some(Phase::Returning)
        );
        assert_eq!(
            Phase::after_recovery(awaiting, &given_back),
            Some(Phase::Returning)
        );
        assert_eq!(Phase::Returning.tags(), tags(Blocker::Queue, Holds::None));
        let mut asked = given_back.clone();
        asked.push(event(
            5,
            event_kind::ASK_OPENED,
            json!({"kind": "approve_landing", "asked_by": "supervisor", "ask_id": 9}),
        ));
        assert_eq!(Phase::resting(awaiting, &asked), Some(Phase::LandingAnswer));
        let mut held = given_back.clone();
        held.push(event(5, event_kind::INTEGRATION_HELD, json!({})));
        assert_eq!(Phase::resting(awaiting, &held), Some(Phase::LandingAnswer));
        // A take-up that failed let go of it: it waits for a person.
        let mut given_up = given_back;
        given_up.push(event(
            5,
            event_kind::RUNTIME_ERROR,
            json!({"message": "x", "lease_released": true}),
        ));
        assert_eq!(
            Phase::resting(awaiting, &given_up),
            Some(Phase::LandingAnswer)
        );
    }

    /// A run whose hold's wait was recorded rests in it until the release,
    /// whatever its status says; the release lets its status say again.
    #[test]
    fn a_held_run_rests_in_its_wait_until_the_release() {
        let held = event(
            1,
            event_kind::RUN_PHASE_CHANGED,
            PhaseChange::new(Phase::Held, Attempt::FIRST, "hold_parked").payload(),
        );
        let lease = event(2, event_kind::LEASE_RELEASED, json!({}));
        let released = event(3, event_kind::TASK_RELEASED, json!({"hold_id": 1}));
        let resting = |events: &[RunEvent]| Phase::resting(RunStatus::NeedsSession, events);
        assert_eq!(resting(&[held.clone(), lease.clone()]), Some(Phase::Held));
        assert_eq!(
            Phase::resting(RunStatus::AwaitingIntegration, std::slice::from_ref(&held)),
            Some(Phase::Held)
        );
        assert_eq!(
            resting(&[held.clone(), lease, released]),
            Some(Phase::NeedsSession)
        );
        // A landing given back or a recovery that let the run rest under
        // its hold leaves it in the wait, not in the rest it would record.
        let recovered = event(4, event_kind::RUN_RECOVERED, json!({}));
        assert_eq!(
            Phase::after_landing(RunStatus::AwaitingIntegration, std::slice::from_ref(&held)),
            Some(Phase::Held)
        );
        assert_eq!(
            Phase::after_recovery(RunStatus::NeedsSession, &[held.clone(), recovered.clone()]),
            Some(Phase::Held)
        );
        // An ended run is not held: its life ended.
        assert_eq!(
            Phase::resting(RunStatus::Failed, std::slice::from_ref(&held)),
            Some(Phase::Ended)
        );
        assert_eq!(
            Phase::after_recovery(RunStatus::Interrupted, &[held, recovered]),
            Some(Phase::Ended)
        );
        assert_eq!(Phase::Held.tags(), tags(Blocker::Human, Holds::None));
    }

    /// An `already_satisfied` claim at rest (ADR-t1818-1): accepted with no
    /// ask, it waits for its verification to run; verified, for the answer
    /// to its ask; answered, for its verification to run again. A snapshot
    /// is a verification running, the slot's to record, and a run in
    /// another status rests by it.
    #[test]
    fn a_resting_claim_waits_for_its_answer_or_its_verification() {
        let accepted = event(
            1,
            event_kind::VALIDATION_FINISHED,
            json!({"status": "validating", "already_satisfied": true}),
        );
        let snapshot = event(2, event_kind::RUN_SATISFIED_SNAPSHOT, json!({"head": "a"}));
        let verified = event(
            3,
            event_kind::RUN_SATISFIED_VERIFIED,
            json!({"passed": true}),
        );
        let asked = event(
            4,
            event_kind::ASK_OPENED,
            json!({"ask_id": 9, "kind": "approve_close"}),
        );
        let answered = event(5, event_kind::ASK_ANSWERED, json!({"ask_id": 9}));
        let resting = |events: &[&RunEvent]| {
            let events: Vec<RunEvent> = events.iter().map(|e| (*e).clone()).collect();
            Phase::resting(RunStatus::Validating, &events)
        };
        assert_eq!(resting(&[&accepted]), Some(Phase::ClaimHeld));
        assert_eq!(resting(&[&accepted, &snapshot]), None);
        assert_eq!(
            resting(&[&accepted, &snapshot, &verified, &asked]),
            Some(Phase::ClaimAnswer)
        );
        assert_eq!(
            resting(&[&accepted, &snapshot, &verified, &asked, &answered]),
            Some(Phase::ClaimHeld)
        );
        let validated = event(
            1,
            event_kind::VALIDATION_FINISHED,
            json!({"status": "validating"}),
        );
        assert_eq!(Phase::resting(RunStatus::Validating, &[validated]), None);
        assert_eq!(
            Phase::resting(RunStatus::Succeeded, &[accepted.clone(), verified.clone()]),
            Some(Phase::Ended)
        );
    }

    fn recovered(id: i64, previous: RunStatus) -> RunEvent {
        event(
            id,
            event_kind::RUN_RECOVERED,
            json!({"previous_status": previous, "status": "awaiting_integration"}),
        )
    }

    /// A landing given up: the run lands again, waiting for its landing
    /// turn, when it was approved (by a `land` answer, whose queue the
    /// landing's start consumed, or by a person's `integrate`) or its
    /// review lets it land; it is reviewed again, waiting for a slot,
    /// otherwise.
    #[test]
    fn a_recovered_landing_lands_again_or_returns_for_its_review() {
        let started = event(10, event_kind::INTEGRATION_STARTED, json!({}));
        let after = |before: Vec<RunEvent>| {
            let mut events = before;
            events.push(started.clone());
            events.push(recovered(11, RunStatus::Integrating));
            Phase::after_recovery(RunStatus::AwaitingIntegration, &events)
        };
        let land_answer = event(
            1,
            event_kind::INTEGRATION_APPROVED,
            json!({"ask_id": 3, "answer": "land"}),
        );
        let integrate = event(1, event_kind::INTEGRATION_APPROVED, json!({}));
        let passed = event(1, event_kind::REVIEW_FINISHED, json!({"verdict": "pass"}));
        for landable in [land_answer, integrate, passed] {
            assert_eq!(
                after(vec![landable.clone()]),
                Some(Phase::LandingQueue),
                "{landable:?}"
            );
        }
        let revise = event(1, event_kind::REVIEW_FINISHED, json!({"verdict": "revise"}));
        for unlandable in [vec![], vec![revise]] {
            assert_eq!(after(unlandable), Some(Phase::Returning));
        }
        assert_eq!(Phase::Returning.tags(), Phase::LandingQueue.tags());
    }

    /// A run recovered again before anything moved on from its given-up
    /// landing stays in the phase the first recovery set, so nothing new
    /// is recorded: its approval's queue is gone, yet it does not wait for
    /// a person, and one to review does not join the landing queue.
    #[test]
    fn a_second_recovery_keeps_the_phase_of_the_first() {
        let resume = Attempt::of(AttemptKind::Resume, 2);
        for (before, phase) in [
            (
                event(
                    1,
                    event_kind::INTEGRATION_APPROVED,
                    json!({"ask_id": 3, "answer": "land"}),
                ),
                Phase::LandingQueue,
            ),
            (event(1, "ask_closed", json!({})), Phase::Returning),
        ] {
            let mut events = vec![
                before,
                event(2, event_kind::INTEGRATION_STARTED, json!({})),
                recovered(3, RunStatus::Integrating),
            ];
            assert_eq!(
                Phase::after_recovery(RunStatus::AwaitingIntegration, &events),
                Some(phase)
            );
            events.push(event(
                4,
                event_kind::RUN_PHASE_CHANGED,
                PhaseChange::new(phase, resume, event_kind::RUN_RECOVERED).payload(),
            ));
            events.push(recovered(5, RunStatus::AwaitingIntegration));
            assert_eq!(
                Phase::after_recovery(RunStatus::AwaitingIntegration, &events),
                Some(phase),
                "{}",
                phase.name()
            );
            assert_eq!(
                next_change(&events, phase, event_kind::RUN_RECOVERED),
                None,
                "{}",
                phase.name()
            );
        }
    }

    /// A recovery with no given-up landing left to act on rests the run by
    /// its status and an `approve_landing` answer's queue.
    #[test]
    fn a_recovery_without_a_landing_given_up_rests_the_run() {
        let approved = event(
            1,
            event_kind::INTEGRATION_APPROVED,
            json!({"ask_id": 3, "answer": "land"}),
        );
        let after = |status, events: &[RunEvent]| Phase::after_recovery(status, events);
        let from_awaiting = recovered(2, RunStatus::AwaitingIntegration);
        assert_eq!(
            after(
                RunStatus::AwaitingIntegration,
                &[approved.clone(), from_awaiting.clone()]
            ),
            Some(Phase::LandingQueue)
        );
        assert_eq!(
            after(
                RunStatus::AwaitingIntegration,
                std::slice::from_ref(&from_awaiting)
            ),
            Some(Phase::LandingAnswer)
        );
        assert_eq!(
            after(
                RunStatus::NeedsSession,
                std::slice::from_ref(&from_awaiting)
            ),
            Some(Phase::NeedsSession)
        );
        assert_eq!(
            after(RunStatus::Interrupted, &[recovered(2, RunStatus::Running)]),
            Some(Phase::Ended)
        );
        // A review taken up after the given-up landing consumed it.
        let consumed = [
            recovered(1, RunStatus::Integrating),
            event(2, event_kind::REVIEW_STARTED, json!({})),
            from_awaiting,
        ];
        assert_eq!(
            after(RunStatus::AwaitingIntegration, &consumed),
            Some(Phase::LandingAnswer)
        );
    }

    /// The next change goes on in the attempt last recorded and is `None`
    /// when it repeats the last record.
    #[test]
    fn the_next_change_goes_on_in_the_last_attempt() {
        let resume = Attempt::of(AttemptKind::Resume, 2);
        let events = [event(
            1,
            event_kind::RUN_PHASE_CHANGED,
            PhaseChange::new(Phase::LandingAnswer, resume, "x").payload(),
        )];
        assert_eq!(
            next_change(&events, Phase::Landing, "integration_started"),
            Some(PhaseChange::new(
                Phase::Landing,
                resume,
                "integration_started"
            ))
        );
        assert_eq!(next_change(&events, Phase::LandingAnswer, "y"), None);
        assert_eq!(
            next_change(&[], Phase::Landing, "integration_started"),
            Some(PhaseChange::new(
                Phase::Landing,
                Attempt::FIRST,
                "integration_started"
            ))
        );
    }

    #[test]
    fn a_push_ends_the_run_or_leaves_it_to_a_person() {
        assert_eq!(
            Phase::after_push(EventKind::PushFinished),
            (Phase::Ended, "pushed")
        );
        assert_eq!(
            Phase::after_push(EventKind::PushSkipped),
            (Phase::Ended, "push_skipped")
        );
        let (pending, cause) = Phase::after_push(EventKind::PushFailed);
        assert_eq!((pending, cause), (Phase::PushPending, "push_failed"));
        assert_eq!(pending.tags(), tags(Blocker::Human, Holds::None));
    }

    #[test]
    fn the_payload_holds_the_phase_its_tags_the_attempt_the_cause_and_the_version() {
        let change = PhaseChange::new(
            Phase::Revise,
            Attempt::of(AttemptKind::Revise, 2),
            "review_finished",
        );
        assert_eq!(
            change.payload(),
            json!({
                "phase": "revise",
                "blocker": "ai",
                "holds": "worker_slot",
                "attempt": {"kind": "revise", "n": 2},
                "cause": "review_finished",
                "v": RULES_VERSION,
            })
        );
        let recorded = Recorded::of(&change.payload()).unwrap();
        assert!(change.repeats(Some(&recorded)));
        assert!(!PhaseChange::new(Phase::Revise, Attempt::FIRST, "x").repeats(Some(&recorded)));
        assert!(!change.repeats(None));
    }

    #[test]
    fn the_last_record_is_read_from_the_runs_events() {
        let event = |id: i64, kind: &str, payload: Value| RunEvent {
            id: crate::domain::EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        };
        let worker = PhaseChange::new(Phase::Worker, Attempt::FIRST, "run_claimed");
        let resume = PhaseChange::new(
            Phase::Resume,
            Attempt::of(AttemptKind::Resume, 1),
            "resume_started",
        );
        let events = [
            event(1, "run_phase_changed", worker.payload()),
            event(2, "run_phase_changed", resume.payload()),
            event(3, "session_exited", json!({})),
        ];
        assert_eq!(
            last_recorded(&events),
            Some(Recorded {
                phase: "resume".into(),
                attempt: Attempt::of(AttemptKind::Resume, 1),
                blocked_by: None,
                holds: Some(Holds::WorkerSlot),
            })
        );
        assert_eq!(last_recorded(&events[2..]), None);
    }
}
