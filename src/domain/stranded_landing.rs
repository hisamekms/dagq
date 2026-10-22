//! A run left awaiting integration with no lease and no open ask, though
//! its last review lets it land (ADR-t3049-1): a supervisor that stopped
//! gave its lease back while it waited for its landing, and nothing else
//! takes it up. A supervisor takes it up again, leases it and puts it back
//! in its landing's line without a second review; a run that waits for a
//! person instead stays a person's (`review and integrate`).
//!
//! The judgment reads the run's events and what the queue says of its
//! lease, its asks and its task. It is told apart from a run left for a
//! person by what follows its last review, not by a mark of the hand-back:
//! a run stranded by any route is taken up, and every route that leaves a
//! run for a person records an event this reads ([`Keep`]).

use super::run::{ConflictPrecheck, restore_payload as restore};
use super::{EventKind, RunEvent, RunHistory, RunStatus, TaskStatus, review_anchor};

/// Why a run with no lease and no open ask stays a person's, not taken up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    /// Its last review does not let it land (none passed, a `revise` or a
    /// `concern` for a person, one its subagents sent elsewhere, a review
    /// or a fix still under way when it was left), so a person reviews it.
    ReviewNotLanding,
    /// An ask was opened after its last review: its `approve_landing`
    /// (open, answered, or answered and not applied yet) or another.
    Asked,
    /// Its landing was held for a person (`integration_held`), or decided,
    /// deferred or failed after its last review.
    LandingStopped,
    /// A supervisor gave it up on a runtime error after its last review (a
    /// `runtime_error` that released its lease: an abandon, a take-up that
    /// failed among them). The error is the inbox's attention, and the run
    /// waits for a person's hand rather than for the same error again.
    GivenUp,
    /// The agent eval before its landing did not pass.
    EvalStopsLanding,
    /// Its task is replanned or held, or was parked for it since the
    /// review.
    Fenced,
    /// A landing recheck parked it for its ask.
    RecheckParked,
    /// Another path of the supervisor takes it up: a `land` answer or a
    /// recovered landing queued it, or it is to be reviewed again.
    QueuedElsewhere,
    /// Its task is not in progress (canceled, or done).
    TaskEnded,
    /// An ask of the run is not closed.
    AskOpen,
}

/// What the queue says of a run, besides its events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seen {
    pub status: RunStatus,
    /// A lease row exists, fresh or stale: a stale one is adopted
    /// (ADR-0039 decision 1), not taken up here.
    pub leased: bool,
    /// An ask of the run is not closed.
    pub ask_open: bool,
    /// A replanning or a hold guards its task now.
    pub fenced: bool,
    pub task: TaskStatus,
}

/// What a supervisor does with a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stranded {
    /// Not a run this judges: not awaiting integration, or leased.
    No,
    /// Leased again and put back in its landing's line.
    TakeUp,
    /// Left for a person, for this reason.
    Keep(Keep),
}

/// The judgment of a run `seen` with `events`, oldest first.
pub fn judge(seen: Seen, events: &[RunEvent]) -> Stranded {
    if seen.status != RunStatus::AwaitingIntegration || seen.leased {
        return Stranded::No;
    }
    let keep = if seen.ask_open {
        Some(Keep::AskOpen)
    } else if seen.task != TaskStatus::InProgress {
        Some(Keep::TaskEnded)
    } else if seen.fenced {
        Some(Keep::Fenced)
    } else {
        kept(events)
    };
    keep.map_or(Stranded::TakeUp, Stranded::Keep)
}

/// Why the events of a run awaiting integration with no lease keep it for
/// a person, `None` when they let a supervisor take it up: its last review
/// lets it land and nothing after it asked a person, stopped its landing
/// or queued it elsewhere.
pub fn kept(events: &[RunEvent]) -> Option<Keep> {
    let history = RunHistory::from_events(events);
    let Some(anchor) = review_anchor(events) else {
        return Some(Keep::ReviewNotLanding);
    };
    // From the last review on: a precheck after it is no step of its own.
    let review = history
        .last(EventKind::ReviewFinished)
        .map_or(anchor.id, |review| review.id);
    let after = |kinds: &[EventKind]| {
        events
            .iter()
            .any(|event| event.id > review && event.kind.is_in(kinds))
    };
    // A park for its task's replanning or hold sets the review before it
    // aside ([`RunHistory::review_lets_land`]).
    if after(&[EventKind::ReplanParked, EventKind::HoldParked]) {
        return Some(Keep::Fenced);
    }
    let landing = match anchor.kind.known() {
        Some(EventKind::ReviewFinished) => history.review_lets_land(),
        // A precheck that sent the session nothing and asked nobody.
        Some(EventKind::ConflictPrecheck) => {
            let precheck = restore::<ConflictPrecheck>(&anchor.payload);
            precheck.requested != Some(true)
                && precheck.asked.is_none()
                && history.review_lets_land()
        }
        _ => false,
    };
    if !landing {
        return Some(Keep::ReviewNotLanding);
    }
    if after(&[EventKind::AskOpened]) {
        return Some(Keep::Asked);
    }
    if events.iter().any(|event| {
        event.id > review
            && event.kind == EventKind::RuntimeError
            && event.payload["lease_released"] == true
    }) {
        return Some(Keep::GivenUp);
    }
    if after(&[
        EventKind::IntegrationHeld,
        EventKind::IntegrationFailed,
        EventKind::IntegrationDeferred,
        EventKind::IntegrationError,
        EventKind::LandingDecided,
    ]) {
        return Some(Keep::LandingStopped);
    }
    if eval_stopped(events, review) {
        return Some(Keep::EvalStopsLanding);
    }
    if super::resume::parked_by_recheck(events) {
        return Some(Keep::RecheckParked);
    }
    if history.queued_to_land().is_some()
        || history.recovered_landing().is_some()
        || history.queued_review_retry().is_some()
    {
        return Some(Keep::QueuedElsewhere);
    }
    None
}

/// Whether the latest agent eval before the landing after event `review`
/// ended with anything but a pass.
fn eval_stopped(events: &[RunEvent], review: super::EventId) -> bool {
    events
        .iter()
        .rev()
        .take_while(|event| event.id > review)
        .find(|event| event.kind == EventKind::RunAgentEvalFinished)
        .is_some_and(|finished| finished.payload["outcome"] != "passed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, TaskId};
    use serde_json::{Value, json};

    fn event(id: i64, kind: EventKind, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: None,
            kind: kind.as_str().into(),
            payload,
            created_at: format!("2026-10-20T00:00:{id:02}.000Z"),
            actor: None,
        }
    }

    /// A run whose third review passed, its session asked to exit, queued
    /// to land and its lease given back by a supervisor that stopped.
    fn handed_back() -> Vec<RunEvent> {
        vec![
            event(1, EventKind::ValidationFinished, json!({"accepted": true})),
            event(2, EventKind::ReviewStarted, json!({"attempt": 3})),
            event(3, EventKind::ReviewFinished, json!({"verdict": "pass"})),
            event(4, EventKind::ConflictPrecheck, json!({})),
            event(5, EventKind::ExitRequested, json!({})),
            event(6, EventKind::LandingQueued, json!({"via": "exit"})),
            event(7, EventKind::LeaseReleased, json!({"reason": "finished"})),
        ]
    }

    fn seen() -> Seen {
        Seen {
            status: RunStatus::AwaitingIntegration,
            leased: false,
            ask_open: false,
            fenced: false,
            task: TaskStatus::InProgress,
        }
    }

    fn with(mut events: Vec<RunEvent>, kind: EventKind, payload: Value) -> Vec<RunEvent> {
        let id = events.len() as i64 + 1;
        events.push(event(id, kind, payload));
        events
    }

    #[test]
    fn a_passed_run_given_back_with_no_lease_and_no_ask_is_taken_up() {
        assert_eq!(judge(seen(), &handed_back()), Stranded::TakeUp);
        // Its e2e retried before the hand-back changes nothing.
        let events = with(
            handed_back(),
            EventKind::RunE2eFinished,
            json!({"outcome": "unavailable"}),
        );
        assert_eq!(judge(seen(), &events), Stranded::TakeUp);
        // A send-back judged converged past the revises (ADR-t2599-1)
        // lands as a pass does.
        let mut converged = handed_back();
        converged[2].payload = json!({"verdict": "revise"});
        let converged = with(
            converged,
            EventKind::ReviseLimitJudged,
            json!({"converged": true}),
        );
        assert_eq!(kept(&converged), None);
    }

    #[test]
    fn a_run_leased_or_not_awaiting_integration_is_not_judged() {
        let leased = Seen {
            leased: true,
            ..seen()
        };
        assert_eq!(judge(leased, &handed_back()), Stranded::No);
        for status in [
            RunStatus::NeedsSession,
            RunStatus::Integrating,
            RunStatus::Running,
        ] {
            let other = Seen { status, ..seen() };
            assert_eq!(judge(other, &handed_back()), Stranded::No);
        }
    }

    #[test]
    fn what_the_queue_says_of_the_run_keeps_it_for_a_person() {
        let open = Seen {
            ask_open: true,
            ..seen()
        };
        assert_eq!(judge(open, &handed_back()), Stranded::Keep(Keep::AskOpen));
        let fenced = Seen {
            fenced: true,
            ..seen()
        };
        assert_eq!(judge(fenced, &handed_back()), Stranded::Keep(Keep::Fenced));
        for task in [TaskStatus::Canceled, TaskStatus::Completed] {
            let ended = Seen { task, ..seen() };
            assert_eq!(
                judge(ended, &handed_back()),
                Stranded::Keep(Keep::TaskEnded)
            );
        }
    }

    #[test]
    fn a_review_that_does_not_let_it_land_keeps_it_for_a_person() {
        // Validated only: no review ran (a supervisor that does not review).
        assert_eq!(kept(&handed_back()[..1]), Some(Keep::ReviewNotLanding));
        // A review that started and gave nothing.
        assert_eq!(kept(&handed_back()[..2]), Some(Keep::ReviewNotLanding));
        assert_eq!(kept(&[]), Some(Keep::ReviewNotLanding));
        // A revise past the third review needs a person's approval
        // (ADR-t2599-1), and a concern for a person.
        for verdict in [
            json!({"verdict": "revise"}),
            json!({"verdict": "concern", "recommendation": "ask", "confidence": "low"}),
        ] {
            let mut events = handed_back();
            events[2].payload = verdict;
            assert_eq!(kept(&events), Some(Keep::ReviewNotLanding));
        }
        // A pass its subagents sent elsewhere.
        let mut routed = handed_back();
        routed[2].payload = json!({"verdict": "pass", "route": {"destination": "ask", "agents": [[{"destination": "ask"}]]}});
        assert_eq!(kept(&routed), Some(Keep::ReviewNotLanding));
        // A conflict request sent to the session, and a precheck that
        // asked a person.
        let mut requested = handed_back()[..4].to_vec();
        requested[3].payload = json!({"requested": true});
        assert_eq!(kept(&requested), Some(Keep::ReviewNotLanding));
        let mut asked = handed_back()[..4].to_vec();
        asked[3].payload = json!({"asked": "past the limit"});
        assert_eq!(kept(&asked), Some(Keep::ReviewNotLanding));
        // A revise sent after the pass.
        let revising = with(handed_back(), EventKind::ReviseRequested, json!({}));
        assert_eq!(kept(&revising), Some(Keep::ReviewNotLanding));
    }

    #[test]
    fn what_followed_the_review_keeps_it_for_a_person() {
        let cases = [
            (
                EventKind::AskOpened,
                json!({"kind": "approve_landing", "asked_by": "supervisor", "ask_id": 9}),
                Keep::Asked,
            ),
            (EventKind::IntegrationHeld, json!({}), Keep::LandingStopped),
            (
                EventKind::IntegrationFailed,
                json!({}),
                Keep::LandingStopped,
            ),
            (EventKind::LandingDecided, json!({}), Keep::LandingStopped),
            (EventKind::HoldParked, json!({}), Keep::Fenced),
            (EventKind::ReplanParked, json!({}), Keep::Fenced),
            (
                EventKind::RunAgentEvalFinished,
                json!({"outcome": "ask", "why": "the dev failed"}),
                Keep::EvalStopsLanding,
            ),
        ];
        for (kind, payload, keep) in cases {
            let events = with(handed_back(), kind, payload);
            assert_eq!(kept(&events), Some(keep), "{kind:?}");
        }
        // A runtime error that let go of the run (a take-up that failed)
        // leaves it to a person; one that kept its lease is only a record.
        let given_up = with(
            handed_back(),
            EventKind::RuntimeError,
            json!({"message": "run r could not be taken up: x", "lease_released": true}),
        );
        assert_eq!(kept(&given_up), Some(Keep::GivenUp));
        assert_eq!(judge(seen(), &given_up), Stranded::Keep(Keep::GivenUp));
        let recorded = with(
            handed_back(),
            EventKind::RuntimeError,
            json!({"message": "heartbeat", "lease_released": false}),
        );
        assert_eq!(kept(&recorded), None);
        // An eval that passed lets it land.
        let passed = with(
            handed_back(),
            EventKind::RunAgentEvalFinished,
            json!({"outcome": "passed"}),
        );
        assert_eq!(kept(&passed), None);
    }

    #[test]
    fn a_run_another_path_takes_up_is_left_to_it() {
        let approved = with(
            with(
                handed_back(),
                EventKind::IntegrationApproved,
                json!({"ask_id": 9, "push": true}),
            ),
            EventKind::LandingQueued,
            json!({"via": "approve", "ask_id": 9}),
        );
        assert_eq!(kept(&approved), Some(Keep::QueuedElsewhere));
        let recovered = with(
            handed_back(),
            EventKind::RunRecovered,
            json!({"previous_status": "integrating"}),
        );
        assert_eq!(kept(&recovered), Some(Keep::QueuedElsewhere));
        let retried = with(handed_back(), EventKind::ReviewRetryQueued, json!({}));
        assert_eq!(kept(&retried), Some(Keep::QueuedElsewhere));
    }
}
