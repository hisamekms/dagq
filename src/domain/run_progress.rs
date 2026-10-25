//! What a run that `status` and `doctor` list is doing now (goal 98): its
//! current phase and since when, from the run's events, and how it uses
//! its supervisor's slot. The run's persistent status stays what it is;
//! this only reads the events the runtime already records.
//!
//! The phase is the last of the events that start one (a forward scan, so
//! a later step replaces an earlier one), kept only when it can belong to
//! the run's status now: a phase of another status, or one the events do
//! not show (a step ended with no next one recorded yet, or events this
//! binary does not know), is `None` rather than a guess. A run nobody
//! leases, or whose lease no working process holds (stale), has no step
//! going on, except a landing it waits for in the queue: one nobody leases
//! only when its events queue it to land.
//! This is not `stats`' `watched_phase`, which only times the sessions the
//! supervisor watches.

use crate::domain::EventKind;
use crate::domain::run::{ConflictPrecheck, restore_payload};
use serde::Serialize;

use super::agent_eval::before_landing::WaitReason;
use super::{
    RunEvent, RunHistory, RunStatus, run_e2e, stats::timestamp_millis, waiting::WaitState,
};

/// The step a listed run is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Claimed, its worker not started yet (`run_claimed`).
    Claim,
    /// The worker's first session (`agent_started`).
    Session,
    /// The runtime validates the receipt (`supervision_finished`, a
    /// `revise_finished` / `conflict_resolved`, or a resume handed to
    /// validation).
    Validate,
    /// Validated and accepted, its review not started yet.
    Validated,
    /// It is reviewed: its program reviews run (`review_programs_started`),
    /// the review job runs (`review_started`), or, after a
    /// `review_retried`, runs again on the other provider or waits for one.
    Review,
    /// Its live session fixes a `revise` verdict (`revise_requested`).
    Revise,
    /// Its live session fixes a conflict (a requested `conflict_precheck`).
    ConflictFix,
    /// The supervisor asked its session to `/exit` and waits for it.
    Exit,
    /// Its review passed and it waits in its slot for the dev of the
    /// agent definitions it changes to start (ADR-t1728-1 decision 10):
    /// another round runs or goes first, or the review's provider is held
    /// (`run_agent_eval_requested`, `run_agent_eval_waiting`).
    AgentEvalWaiting,
    /// That dev runs (`run_agent_eval_waiting` with `agent_eval_running`).
    AgentEval,
    /// Waits for the landing slot (`landing_queued`, or an e2e that
    /// passed or found nothing to run).
    LandingQueue,
    /// Waits for another run's e2e (`run_e2e_waiting`).
    E2eWaiting,
    /// Its e2e runs on the host (`run_e2e_started`).
    E2e,
    /// Its e2e could not run and waits to be tried again (`run_e2e_finished`
    /// with `outcome: unavailable`).
    E2eRetryWait,
    /// It lands (`integration_started`).
    Integrating,
    /// A resumed session works on it (`resume_started`, and the
    /// `agent_started` of the resumed session's agent).
    Resume,
    /// Its recovery job runs (`triage_started`).
    Recovery,
}

impl Phase {
    /// The phase `event` starts, `Some(None)` when it ends the phase
    /// before it with no next one, `None` when it moves nothing.
    fn started_by(event: &RunEvent) -> Option<Option<Self>> {
        let payload = &event.payload;
        Some(match event.kind.known() {
            Some(EventKind::RunClaimed) => Some(Self::Claim),
            Some(EventKind::AgentStarted) => Some(Self::Session),
            Some(
                EventKind::SupervisionFinished
                | EventKind::ReviseFinished
                | EventKind::ConflictResolved,
            ) => Some(Self::Validate),
            Some(EventKind::ResumeStarted) => Some(Self::Resume),
            Some(EventKind::ResumeFinished) => {
                (payload["status"] == RunStatus::Validating.as_str()).then_some(Self::Validate)
            }
            Some(EventKind::ValidationFinished) => {
                (payload["accepted"] == true).then_some(Self::Validated)
            }
            Some(
                EventKind::ReviewProgramsStarted
                | EventKind::ReviewStarted
                | EventKind::ReviewRetried,
            ) => Some(Self::Review),
            Some(EventKind::ReviseRequested) => Some(Self::Revise),
            Some(EventKind::ConflictPrecheck)
                if restore_payload::<ConflictPrecheck>(payload).requested == Some(true) =>
            {
                Some(Self::ConflictFix)
            }
            // A precheck that found no conflict moves nothing.
            Some(EventKind::ConflictPrecheck) if payload["unsent"] != true => return None,
            Some(EventKind::ExitRequested | EventKind::ExitRetried) => Some(Self::Exit),
            // A landing a handoff's drain handed over waits for the slot
            // again (ADR-t3344-1).
            Some(EventKind::LandingQueued | EventKind::IntegrationHandedOver) => {
                Some(Self::LandingQueue)
            }
            Some(EventKind::RunAgentEvalRequested) if !rounds_asked(payload) => return None,
            Some(EventKind::RunAgentEvalRequested) => Some(Self::AgentEvalWaiting),
            Some(EventKind::RunAgentEvalWaiting) => {
                Some(if payload["reason"] == WaitReason::Running.as_str() {
                    Self::AgentEval
                } else {
                    Self::AgentEvalWaiting
                })
            }
            Some(EventKind::RunE2eWaiting) => Some(Self::E2eWaiting),
            Some(EventKind::RunE2eStarted) => Some(Self::E2e),
            Some(EventKind::RunE2eFinished) => match payload["outcome"].as_str() {
                Some(run_e2e::PASSED | run_e2e::NOT_CONFIGURED) => Some(Self::LandingQueue),
                Some(run_e2e::UNAVAILABLE) => Some(Self::E2eRetryWait),
                _ => None,
            },
            Some(EventKind::IntegrationStarted) => Some(Self::Integrating),
            Some(EventKind::TriageStarted) => Some(Self::Recovery),
            Some(
                EventKind::ResumeSkipped
                | EventKind::ReviewFinished
                | EventKind::ReviewFailed
                | EventKind::ReviseUnsent
                | EventKind::ConflictPrecheck
                | EventKind::SessionExited
                | EventKind::RunE2eFailed
                | EventKind::RunAgentEvalFinished
                | EventKind::RunIntegrated
                | EventKind::IntegrationDeferred
                | EventKind::IntegrationError
                | EventKind::IntegrationHeld
                | EventKind::IntegrationFailed
                | EventKind::TriageFinished
                | EventKind::TriageFailed
                | EventKind::RunRecovered,
            ) => None,
            _ => return None,
        })
    }

    /// Whether a run in `status` with `lease` can be in this phase, and
    /// with `queued` whether its events queue it to land with no lease
    /// ([`RunHistory::queued_to_land`]): without a working process holding
    /// its lease only a landing it waits for in the queue goes on, and with
    /// no lease at all only one the supervisor lands from its events (a run
    /// whose lease was given back is in no queue, ADR-t3049-1).
    fn fits(self, status: RunStatus, lease: Lease, queued: bool) -> bool {
        use RunStatus as S;
        if lease != Lease::Live {
            return self == Self::LandingQueue
                && status == S::AwaitingIntegration
                && (lease == Lease::Stale || queued);
        }
        match status {
            S::Claimed | S::Starting => self == Self::Claim,
            S::Running => matches!(self, Self::Session | Self::Exit),
            S::Validating => self == Self::Validate,
            S::AwaitingIntegration => matches!(
                self,
                Self::Validated
                    | Self::Review
                    | Self::Revise
                    | Self::ConflictFix
                    | Self::Exit
                    | Self::LandingQueue
                    | Self::E2eWaiting
                    | Self::E2e
                    | Self::E2eRetryWait
                    | Self::AgentEvalWaiting
                    | Self::AgentEval
            ),
            S::Integrating => self == Self::Integrating,
            S::NeedsSession => matches!(self, Self::Resume | Self::Exit),
            S::Failed | S::Interrupted => matches!(self, Self::Recovery | Self::Exit),
            S::Integrated | S::Succeeded | S::Replaced => self == Self::Exit,
        }
    }
}

/// Whether a `run_agent_eval_requested` asked for any round: one that
/// only left agents out moves nothing.
fn rounds_asked(payload: &serde_json::Value) -> bool {
    payload["rounds"]
        .as_array()
        .is_some_and(|rounds| !rounds.is_empty())
}

/// How a run uses its supervisor's slot, as `status`' `slots` and
/// `waiting` count it ([`WaitState`]): a leased run that does not wait
/// takes a slot (`used`), one that waits only for its landing turn
/// (`landing_queue`, ADR-t1591-1) leaves room for a light task, one that
/// waits for a person is out of it (`waiting`), one whose wait ended waits
/// to go back (`returning`). A run nobody leases has none (`None`).
/// Counting the `slot` of a supervisor's `run_ids` gives its `slots` and
/// `waiting`, but for a landing queue longer than `parallel`, whose runs
/// past `parallel` `slots` counts in `used`; the number of `run_ids` is not `slots.used`.
/// The supervisor counts its landing queue only for a run whose last look
/// found another run landing, so while it holds its landings (and its
/// claims), and before a needed e2e starts, `status`' `landing_queue` may
/// be more than the supervisor's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotUse {
    Used,
    LandingQueue,
    Waiting,
    Returning,
}

impl SlotUse {
    pub fn of(leased: bool, wait: Option<&WaitState>) -> Option<Self> {
        leased.then_some(match wait {
            None => Self::Used,
            Some(WaitState { ended: None, .. }) => Self::Waiting,
            Some(_) => Self::Returning,
        })
    }
}

/// Whether a run in `status` waits only for its landing turn by its events
/// (ADR-t1591-1): awaiting integration, its review passed and its e2e done
/// or not needed (`landing_queued`, or an e2e that passed or found nothing
/// to run, last).
pub fn in_landing_queue(status: RunStatus, events: &[RunEvent]) -> bool {
    status == RunStatus::AwaitingIntegration
        && Progress::of(status, Lease::Live, events, 0).phase == Some(Phase::LandingQueue)
}

/// The run's lease as [`Progress::of`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lease {
    /// Nobody leases the run.
    None,
    /// A lease whose holder is dead or whose heartbeat is stale: it still
    /// counts in its supervisor's slots, but nothing goes on.
    Stale,
    /// A lease a working process holds.
    Live,
}

/// A listed run's `progress`: its phase, when it started (`since`, unix
/// seconds, `None` when the event's time cannot be read) and for how long
/// (`elapsed_secs` at `now`), and its slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Progress {
    pub phase: Option<Phase>,
    pub since: Option<i64>,
    pub elapsed_secs: Option<i64>,
    pub slot: Option<SlotUse>,
}

impl Progress {
    /// The progress of a run in `status` with `lease`, from its events
    /// (oldest first) at `now` (unix seconds).
    pub fn of(status: RunStatus, lease: Lease, events: &[RunEvent], now: i64) -> Self {
        let mut current: Option<(Phase, &RunEvent)> = None;
        for event in events {
            // The agent of a resumed session starts within the resume.
            if event.kind == EventKind::AgentStarted
                && current.is_some_and(|(phase, _)| phase == Phase::Resume)
            {
                continue;
            }
            if let Some(started) = Phase::started_by(event) {
                current = started.map(|phase| (phase, event));
            }
        }
        let queued =
            lease == Lease::None && RunHistory::from_events(events).queued_to_land().is_some();
        let current = current.filter(|(phase, _)| phase.fits(status, lease, queued));
        let since = current
            .and_then(|(_, event)| timestamp_millis(&event.created_at))
            .map(|ms| ms.div_euclid(1000));
        Self {
            phase: current.map(|(phase, _)| phase),
            since,
            elapsed_secs: since.map(|since| (now - since).max(0)),
            slot: SlotUse::of(lease != Lease::None, WaitState::of(events).as_ref()).map(|slot| {
                let queued = status == RunStatus::AwaitingIntegration
                    && current.is_some_and(|(phase, _)| phase == Phase::LandingQueue);
                if slot == SlotUse::Used && queued {
                    SlotUse::LandingQueue
                } else {
                    slot
                }
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::event_kind;
    use crate::domain::{EventId, TaskId};
    use serde_json::{Value, json};

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: format!("2026-10-04T00:00:{id:02}.000Z"),
            actor: None,
        }
    }

    /// The run's phase after each of `steps` in turn, in `status`.
    fn phases(steps: &[(&str, Value, RunStatus)]) -> Vec<Option<Phase>> {
        let mut events = Vec::new();
        steps
            .iter()
            .enumerate()
            .map(|(i, (kind, payload, status))| {
                events.push(event(i as i64 + 1, kind, payload.clone()));
                Progress::of(*status, Lease::Live, &events, 0).phase
            })
            .collect()
    }

    const AT_00: i64 = 1_791_072_000;

    #[test]
    fn revise_review_e2e_exit_landing_and_integrating_follow_one_another() {
        use Phase as P;
        use RunStatus as S;
        let steps = [
            ("run_claimed", json!({}), S::Claimed),
            ("agent_started", json!({}), S::Running),
            ("supervision_finished", json!({}), S::Validating),
            (
                "validation_finished",
                json!({"accepted": true}),
                S::AwaitingIntegration,
            ),
            ("review_started", json!({}), S::AwaitingIntegration),
            (
                "review_finished",
                json!({"verdict": "revise"}),
                S::AwaitingIntegration,
            ),
            ("revise_requested", json!({}), S::AwaitingIntegration),
            ("revise_finished", json!({}), S::Validating),
            (
                "validation_finished",
                json!({"accepted": true}),
                S::AwaitingIntegration,
            ),
            ("review_started", json!({}), S::AwaitingIntegration),
            (
                "review_finished",
                json!({"verdict": "pass"}),
                S::AwaitingIntegration,
            ),
            ("conflict_precheck", json!({}), S::AwaitingIntegration),
            ("exit_requested", json!({}), S::AwaitingIntegration),
            ("session_exited", json!({}), S::AwaitingIntegration),
            (
                "landing_queued",
                json!({"via": "exit"}),
                S::AwaitingIntegration,
            ),
            ("run_e2e_waiting", json!({}), S::AwaitingIntegration),
            ("run_e2e_started", json!({}), S::AwaitingIntegration),
            (
                "run_e2e_finished",
                json!({"outcome": "unavailable"}),
                S::AwaitingIntegration,
            ),
            ("run_e2e_started", json!({}), S::AwaitingIntegration),
            (
                "run_e2e_finished",
                json!({"outcome": "passed"}),
                S::AwaitingIntegration,
            ),
            ("integration_started", json!({}), S::Integrating),
        ];
        assert_eq!(
            phases(&steps),
            [
                Some(P::Claim),
                Some(P::Session),
                Some(P::Validate),
                Some(P::Validated),
                Some(P::Review),
                None,
                Some(P::Revise),
                Some(P::Validate),
                Some(P::Validated),
                Some(P::Review),
                None,
                // A precheck that found no conflict moves nothing.
                None,
                Some(P::Exit),
                None,
                Some(P::LandingQueue),
                Some(P::E2eWaiting),
                Some(P::E2e),
                Some(P::E2eRetryWait),
                Some(P::E2e),
                Some(P::LandingQueue),
                Some(P::Integrating),
            ]
        );
    }

    /// A passed run whose agent eval waits for its turn while another round
    /// runs shows that wait, then the round that runs, in its slot; an
    /// eval that only left agents out moves nothing, and its end leaves
    /// the run to the steps after it.
    #[test]
    fn a_runs_agent_eval_before_it_lands_shows_why_it_waits() {
        use Phase as P;
        use RunStatus as S;
        let steps = [
            (
                "review_finished",
                json!({"verdict": "pass"}),
                S::AwaitingIntegration,
            ),
            (
                event_kind::RUN_AGENT_EVAL_REQUESTED,
                json!({"rounds": [], "skipped": [{"agent": "a", "reason": "no_dev_cases"}]}),
                S::AwaitingIntegration,
            ),
            (
                event_kind::RUN_AGENT_EVAL_REQUESTED,
                json!({"rounds": [{"agent": "a", "eval_id": 7}]}),
                S::AwaitingIntegration,
            ),
            (
                event_kind::RUN_AGENT_EVAL_WAITING,
                json!({"reason": "agent_eval_queued"}),
                S::AwaitingIntegration,
            ),
            (
                event_kind::RUN_AGENT_EVAL_WAITING,
                json!({"reason": "agent_eval_running"}),
                S::AwaitingIntegration,
            ),
            (
                event_kind::RUN_AGENT_EVAL_FINISHED,
                json!({"outcome": "passed"}),
                S::AwaitingIntegration,
            ),
            ("exit_requested", json!({}), S::AwaitingIntegration),
        ];
        assert_eq!(
            phases(&steps),
            [
                None,
                None,
                Some(P::AgentEvalWaiting),
                Some(P::AgentEvalWaiting),
                Some(P::AgentEval),
                None,
                Some(P::Exit),
            ]
        );
        // It holds its slot meanwhile.
        let mut events = Vec::new();
        for (i, (kind, payload, _)) in steps[..4].iter().enumerate() {
            events.push(event(i as i64 + 1, kind, payload.clone()));
        }
        let progress = Progress::of(S::AwaitingIntegration, Lease::Live, &events, 0);
        assert_eq!(progress.phase, Some(P::AgentEvalWaiting));
        assert_eq!(progress.slot, Some(SlotUse::Used));
    }

    /// The program reviews before the agents' review are the review: their
    /// start puts the run in it, their ends move nothing, and a program
    /// that rejects the run sends it to the revise as the agents' verdict
    /// does.
    #[test]
    fn the_program_reviews_are_the_review_phase() {
        use Phase as P;
        use RunStatus as S;
        let steps = [
            (
                "validation_finished",
                json!({"accepted": true}),
                S::AwaitingIntegration,
            ),
            (
                event_kind::REVIEW_PROGRAMS_STARTED,
                json!({}),
                S::AwaitingIntegration,
            ),
            (
                event_kind::REVIEW_PROGRAM_FINISHED,
                json!({"outcome": "rejected"}),
                S::AwaitingIntegration,
            ),
            (
                event_kind::REVIEW_PROGRAMS_FINISHED,
                json!({"outcome": "rejected"}),
                S::AwaitingIntegration,
            ),
            ("revise_requested", json!({}), S::AwaitingIntegration),
            ("revise_finished", json!({}), S::Validating),
            (
                "validation_finished",
                json!({"accepted": true}),
                S::AwaitingIntegration,
            ),
            (
                event_kind::REVIEW_PROGRAMS_STARTED,
                json!({}),
                S::AwaitingIntegration,
            ),
            (
                event_kind::REVIEW_PROGRAMS_FINISHED,
                json!({"outcome": "passed"}),
                S::AwaitingIntegration,
            ),
            ("review_started", json!({}), S::AwaitingIntegration),
        ];
        assert_eq!(
            phases(&steps),
            [
                Some(P::Validated),
                Some(P::Review),
                Some(P::Review),
                Some(P::Review),
                Some(P::Revise),
                Some(P::Validate),
                Some(P::Validated),
                Some(P::Review),
                Some(P::Review),
                Some(P::Review),
            ]
        );
    }

    #[test]
    fn a_resume_goes_to_validation_and_review_and_a_conflict_is_fixed() {
        use Phase as P;
        use RunStatus as S;
        let steps = [
            ("run_e2e_failed", json!({}), S::NeedsSession),
            ("resume_started", json!({}), S::NeedsSession),
            // The resumed session's agent keeps the resume.
            ("agent_started", json!({}), S::NeedsSession),
            (
                "resume_finished",
                json!({"status": "validating"}),
                S::Validating,
            ),
            (
                "validation_finished",
                json!({"accepted": true}),
                S::AwaitingIntegration,
            ),
            ("review_started", json!({}), S::AwaitingIntegration),
            (
                "review_finished",
                json!({"verdict": "pass"}),
                S::AwaitingIntegration,
            ),
            (
                "conflict_precheck",
                json!({"requested": true}),
                S::AwaitingIntegration,
            ),
            ("conflict_resolved", json!({}), S::Validating),
            ("resume_started", json!({}), S::NeedsSession),
            (
                "resume_finished",
                json!({"status": "needs_session"}),
                S::NeedsSession,
            ),
            ("triage_started", json!({}), S::Failed),
            ("triage_finished", json!({}), S::Failed),
        ];
        assert_eq!(
            phases(&steps),
            [
                None,
                Some(P::Resume),
                Some(P::Resume),
                Some(P::Validate),
                Some(P::Validated),
                Some(P::Review),
                None,
                Some(P::ConflictFix),
                Some(P::Validate),
                Some(P::Resume),
                None,
                Some(P::Recovery),
                None,
            ]
        );
    }

    #[test]
    fn a_phase_of_another_status_or_without_a_lease_is_not_asserted() {
        let events = [
            event(1, "run_claimed", json!({})),
            event(2, "review_started", json!({})),
        ];
        // The review's phase does not fit a run that is running again.
        assert_eq!(
            Progress::of(RunStatus::Running, Lease::Live, &events, 0).phase,
            None
        );
        let review = Progress::of(
            RunStatus::AwaitingIntegration,
            Lease::Live,
            &events,
            AT_00 + 10,
        );
        assert_eq!(review.phase, Some(Phase::Review));
        assert_eq!(review.since, Some(AT_00 + 2));
        assert_eq!(review.elapsed_secs, Some(8));
        assert_eq!(review.slot, Some(SlotUse::Used));
        // Its holder died: no review goes on, though it holds the slot.
        let stale = Progress::of(RunStatus::AwaitingIntegration, Lease::Stale, &events, 0);
        assert_eq!((stale.phase, stale.slot), (None, Some(SlotUse::Used)));
        // Nobody leases it: no review goes on, and it holds no slot.
        let unleased = Progress::of(RunStatus::AwaitingIntegration, Lease::None, &events, 0);
        assert_eq!((unleased.phase, unleased.slot), (None, None));
        // A landing it waits for in the queue does without a lease, when
        // a `land` answer queued it.
        let queued = [
            event(
                1,
                "integration_approved",
                json!({"ask_id": 9, "push": true}),
            ),
            event(2, "landing_queued", json!({"via": "approve", "ask_id": 9})),
        ];
        assert_eq!(
            Progress::of(RunStatus::AwaitingIntegration, Lease::None, &queued, 0).phase,
            Some(Phase::LandingQueue)
        );
        assert_eq!(
            Progress::of(RunStatus::NeedsSession, Lease::None, &queued, 0).phase,
            None
        );
    }

    /// A passed run whose supervisor gave its lease back as it waited for
    /// its landing (ADR-t3049-1) is in no queue while nobody leases it,
    /// though its last step was `landing_queued`; with a stale lease, its
    /// supervisor's landing queue is what an adopter goes on with.
    #[test]
    fn a_run_whose_lease_was_given_back_is_not_shown_in_the_landing_queue() {
        let given_back = [
            event(1, "review_finished", json!({"verdict": "pass"})),
            event(2, "exit_requested", json!({})),
            event(3, "landing_queued", json!({"via": "exit"})),
            event(4, "lease_released", json!({"reason": "finished"})),
        ];
        let progress = Progress::of(RunStatus::AwaitingIntegration, Lease::None, &given_back, 0);
        assert_eq!((progress.phase, progress.slot), (None, None));
        let stale = Progress::of(RunStatus::AwaitingIntegration, Lease::Stale, &given_back, 0);
        assert_eq!(stale.phase, Some(Phase::LandingQueue));
        // Taken up again, its lease live, it waits for its landing turn.
        let live = Progress::of(RunStatus::AwaitingIntegration, Lease::Live, &given_back, 0);
        assert_eq!(
            (live.phase, live.slot),
            (Some(Phase::LandingQueue), Some(SlotUse::LandingQueue))
        );
    }

    #[test]
    fn unknown_events_and_unreadable_times_assert_nothing_and_do_not_fail() {
        let mut events = vec![
            event(1, "review_started", json!({})),
            // A kind a newer binary writes moves nothing.
            event(2, "review_subagent_spawned", json!({"agent": "x"})),
            event(3, "run_e2e_finished", json!({"outcome": "something_new"})),
        ];
        let progress = Progress::of(RunStatus::AwaitingIntegration, Lease::Live, &events, 0);
        assert_eq!(progress.phase, None);
        events.truncate(2);
        events[0].created_at = "not a time".into();
        let progress = Progress::of(RunStatus::AwaitingIntegration, Lease::Live, &events, 0);
        assert_eq!(progress.phase, Some(Phase::Review));
        assert_eq!((progress.since, progress.elapsed_secs), (None, None));
        let empty = Progress::of(RunStatus::Running, Lease::Live, &[], 0);
        assert_eq!(
            empty,
            Progress {
                phase: None,
                since: None,
                elapsed_secs: None,
                slot: Some(SlotUse::Used),
            }
        );
    }

    #[test]
    fn the_slot_is_used_waiting_or_returning_as_the_wait_stands() {
        let started = event(
            1,
            "run_waiting_started",
            json!({"phase": "revise", "status": "awaiting_integration", "ask_id": 4, "ask_kind": "worker_question"}),
        );
        let waiting = [started.clone()];
        assert_eq!(
            Progress::of(RunStatus::AwaitingIntegration, Lease::Live, &waiting, 0).slot,
            Some(SlotUse::Waiting)
        );
        let returning = [
            started.clone(),
            event(2, "run_waiting_ended", json!({"cause": "answered"})),
        ];
        assert_eq!(
            Progress::of(RunStatus::AwaitingIntegration, Lease::Live, &returning, 0).slot,
            Some(SlotUse::Returning)
        );
        let back = [
            started,
            event(2, "run_waiting_ended", json!({"cause": "answered"})),
            event(3, "run_slot_regained", json!({})),
        ];
        assert_eq!(
            Progress::of(RunStatus::AwaitingIntegration, Lease::Live, &back, 0).slot,
            Some(SlotUse::Used)
        );
        assert_eq!(
            Progress::of(RunStatus::AwaitingIntegration, Lease::None, &returning, 0).slot,
            None
        );
    }

    #[test]
    fn a_leased_run_that_waits_only_for_its_landing_turn_is_in_the_landing_queue() {
        use RunStatus as S;
        let queued = [
            event(1, "review_finished", json!({"verdict": "pass"})),
            event(2, "landing_queued", json!({"via": "exit"})),
        ];
        assert!(in_landing_queue(S::AwaitingIntegration, &queued));
        assert_eq!(
            Progress::of(S::AwaitingIntegration, Lease::Live, &queued, 0).slot,
            Some(SlotUse::LandingQueue)
        );
        assert_eq!(
            Progress::of(S::AwaitingIntegration, Lease::None, &queued, 0).slot,
            None
        );
        // Its e2e runs: it works, in its slot.
        let e2e = [queued[1].clone(), event(3, "run_e2e_started", json!({}))];
        assert!(!in_landing_queue(S::AwaitingIntegration, &e2e));
        assert_eq!(
            Progress::of(S::AwaitingIntegration, Lease::Live, &e2e, 0).slot,
            Some(SlotUse::Used)
        );
        // Its e2e passed: back in the landing queue.
        let passed = [
            e2e[0].clone(),
            e2e[1].clone(),
            event(4, "run_e2e_finished", json!({"outcome": "passed"})),
        ];
        assert!(in_landing_queue(S::AwaitingIntegration, &passed));
        // Landing, or back in a session, it is in its slot.
        let landing = [
            passed[2].clone(),
            event(5, "integration_started", json!({})),
        ];
        assert!(!in_landing_queue(S::Integrating, &landing));
        assert!(!in_landing_queue(S::NeedsSession, &queued));
        // In review: in its slot.
        let review = [event(1, "review_started", json!({}))];
        assert!(!in_landing_queue(S::AwaitingIntegration, &review));
    }
}
