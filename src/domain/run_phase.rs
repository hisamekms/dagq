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

use super::{EventKind, RunEvent, RunStatus, event_kind};

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
    /// A service outside: the remote, a provider's API.
    External,
    /// The run's environment: its provisioning, a failure, short resources.
    Infra,
}

/// The slot the run holds meanwhile, as the supervisor counts its slots
/// (ADR-t1591-1: a run waiting only for its landing turn holds none).
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
    /// The run waits outside the slots for a person's answer (ADR-0062),
    /// a `worker_question` among them.
    Waiting,
    /// Its wait ended, and it waits to go back to a slot.
    Returning,
    /// A run to land waits in its slot to be decided: its e2e, the holds
    /// of a landing, the integration slot.
    AwaitingSlot,
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
            Self::Worker => "worker",
            Self::Validating => "validating",
            Self::Review => "review",
            Self::ReviewHeld => "review_held",
            Self::Revise => "revise",
            Self::Exiting => "exiting",
            Self::Resume => "resume",
            Self::Recovery => "recovery",
            Self::Waiting => "waiting",
            Self::Returning => "returning",
            Self::AwaitingSlot => "awaiting_slot",
            Self::AwaitingE2e => "awaiting_e2e",
            Self::E2e => "e2e",
            Self::LandingQueue => "landing_queue",
            Self::Landing => "landing",
            Self::LandingAnswer => "landing_answer",
            Self::NeedsSession => "needs_session",
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
            Self::Provisioning => (B::Infra, H::WorkerSlot),
            Self::Worker | Self::Review | Self::Revise | Self::Resume | Self::Recovery => {
                (B::Ai, H::WorkerSlot)
            }
            Self::Validating | Self::Exiting => (B::Runtime, H::WorkerSlot),
            Self::ReviewHeld => (B::External, H::WorkerSlot),
            Self::Waiting | Self::LandingAnswer | Self::PushPending => (B::Human, H::None),
            Self::Returning | Self::LandingQueue | Self::NeedsSession => (B::Queue, H::None),
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

    /// Whether it records the same phase, attempt and landing slot's
    /// holder as `recorded`.
    pub fn repeats(&self, recorded: Option<&Recorded>) -> bool {
        recorded.is_some_and(|r| {
            r.phase == self.phase.name()
                && r.attempt == self.attempt
                && r.blocked_by == self.blocked_by
        })
    }

    /// What this change records, as [`Recorded::of`] reads it back.
    pub fn recorded(&self) -> Recorded {
        Recorded {
            phase: self.phase.name().to_owned(),
            attempt: self.attempt,
            blocked_by: self.blocked_by.clone(),
        }
    }
}

/// What a recorded `run_phase_changed` says of the phase, the attempt and
/// the landing slot's holder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub phase: String,
    pub attempt: Attempt,
    pub blocked_by: Option<String>,
}

impl Recorded {
    /// The phase and attempt of a `run_phase_changed`'s payload; a payload
    /// without an attempt it can read counts as the first.
    pub fn of(payload: &Value) -> Option<Self> {
        Some(Self {
            phase: payload["phase"].as_str()?.to_owned(),
            attempt: serde_json::from_value(payload["attempt"].clone()).unwrap_or(Attempt::FIRST),
            blocked_by: payload["blocked_by"].as_str().map(str::to_owned),
        })
    }
}

/// The last `run_phase_changed` among a run's `events`.
pub fn last_recorded(events: &[RunEvent]) -> Option<Recorded> {
    events
        .iter()
        .rev()
        .find(|event| event.kind == event_kind::RUN_PHASE_CHANGED)
        .and_then(|event| Recorded::of(&event.payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Phase; 22] = [
        Phase::Provisioning,
        Phase::Worker,
        Phase::Validating,
        Phase::Review,
        Phase::ReviewHeld,
        Phase::Revise,
        Phase::Exiting,
        Phase::Resume,
        Phase::Recovery,
        Phase::Waiting,
        Phase::Returning,
        Phase::AwaitingSlot,
        Phase::AwaitingE2e,
        Phase::E2e,
        Phase::LandingQueue,
        Phase::Landing,
        Phase::LandingAnswer,
        Phase::NeedsSession,
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
            (Phase::Waiting, tags(B::Human, H::None)),
            (Phase::PushPending, tags(B::Human, H::None)),
            (
                Phase::Push { in_slot: true },
                tags(B::External, H::WorkerSlot),
            ),
            (Phase::Push { in_slot: false }, tags(B::External, H::None)),
            (Phase::Provisioning, tags(B::Infra, H::WorkerSlot)),
        ] {
            assert_eq!(phase.tags(), expected, "{}", phase.name());
        }
    }

    /// Each phase has its own name, but the two pushes, which differ only
    /// in the slot they hold.
    #[test]
    fn phase_names_are_distinct() {
        let mut names: Vec<&str> = ALL.iter().map(|phase| phase.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ALL.len() - 1);
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
            kind: kind.to_owned(),
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
            })
        );
        assert_eq!(last_recorded(&events[2..]), None);
    }
}
