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
//!
//! A landing a supervisor started and a runtime error stopped before
//! `main` moved is a person's too, except once for each other build
//! (ADR-t3524-1, [`build_retry`]): a supervisor of a build the run's
//! history since its last review has not seen takes it up again, as the
//! binary that stopped it may have been the cause.

use super::reason::{ReasonCode, event_code};
use super::run::{ConflictPrecheck, restore_payload as restore};
use super::{EventId, EventKind, RunEvent, RunHistory, RunStatus, TaskStatus, review_anchor};
use serde_json::{Value, json};

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
    /// deferred, failed or stopped by an error after its last review, and
    /// no [`build_retry`] takes it up.
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

/// The judgment of a run `seen` with `events`, oldest first, by a
/// supervisor of build `build` (`None` takes up no [`build_retry`]).
pub fn judge(seen: Seen, events: &[RunEvent], build: Option<&str>) -> Stranded {
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
        kept(events, build)
    };
    keep.map_or(Stranded::TakeUp, Stranded::Keep)
}

/// Why the events of a run awaiting integration with no lease keep it for
/// a person, `None` when they let a supervisor take it up: its last review
/// lets it land and nothing after it asked a person, stopped its landing
/// (unless a supervisor of build `build` takes it up again, [`build_retry`])
/// or queued it elsewhere.
pub fn kept(events: &[RunEvent], build: Option<&str>) -> Option<Keep> {
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
    if after(&LANDING_STOPS) && retry_after(events, review, build).is_none() {
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

/// The events that stop a run's landing and leave it a person's.
const LANDING_STOPS: [EventKind; 5] = [
    EventKind::IntegrationHeld,
    EventKind::IntegrationFailed,
    EventKind::IntegrationDeferred,
    EventKind::IntegrationError,
    EventKind::LandingDecided,
];

/// The `lease_acquired` payload's key that records a [`BuildRetry`].
pub const BUILD_RETRY_KEY: &str = "build_retry";

/// A run taken up again by a supervisor of another build than the one
/// whose landing an error stopped before `main` moved (ADR-t3524-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildRetry {
    /// The `integration_error` it takes up.
    pub error: EventId,
    /// The build that error recorded.
    pub error_build: String,
    /// The build of the supervisor that takes it up.
    pub build: String,
}

impl BuildRetry {
    /// The record of the take-up in its `lease_acquired`'s payload, under
    /// [`BUILD_RETRY_KEY`].
    pub fn record(&self) -> Value {
        json!({"error_id": self.error, "error_build": self.error_build, "build": self.build})
    }
}

/// The take-up again of a run by a supervisor of build `build`, when what
/// stopped its landing since its last review is `integration_error` alone,
/// the latest of which has code `other`, came of a landing a supervisor
/// started (its actor's `requested_by`; a person's `integrate` leaves its
/// error to the person) and recorded a build other than `build`, and
/// `build` has not been seen since that review: no `integration_error` of
/// it and no take-up again by it. So each build takes a run up at most
/// once, and a build tried before (A, then B, then A again) is not tried
/// again. Each error is taken up at most once too: a take-up by any build
/// after the latest error (one given back without a new error) leaves the
/// run a person's, so a supervisor that judged it before another build
/// took it up takes nothing. The events alone decide, the same after a
/// restart and in the lease's transaction. `None` for
/// any other run: an error with no build, its own build's, another code, or
/// a landing held, failed, deferred or decided.
pub fn build_retry(events: &[RunEvent], build: Option<&str>) -> Option<BuildRetry> {
    let anchor = review_anchor(events)?;
    let review = RunHistory::from_events(events)
        .last(EventKind::ReviewFinished)
        .map_or(anchor.id, |review| review.id);
    retry_after(events, review, build)
}

/// [`build_retry`] on the events after event `review`.
fn retry_after(events: &[RunEvent], review: EventId, build: Option<&str>) -> Option<BuildRetry> {
    let build = build?;
    let since: Vec<&RunEvent> = events.iter().filter(|event| event.id > review).collect();
    let stops: Vec<&RunEvent> = since
        .iter()
        .copied()
        .filter(|event| event.kind.is_in(&LANDING_STOPS))
        .collect();
    if stops
        .iter()
        .any(|event| event.kind != EventKind::IntegrationError)
    {
        return None;
    }
    let error = stops.last()?;
    let error_build = error.payload["build"].as_str()?;
    let by_supervisor = error
        .actor
        .as_ref()
        .and_then(|actor| actor.requested_by.as_deref())
        .is_some_and(|id| id.split(':').next() == Some("supervisor"));
    if event_code(error) != Some(ReasonCode::Other) || !by_supervisor || error_build == build {
        return None;
    }
    // A take-up again after the latest error, by any build, is that
    // error's one: a supervisor that judged the run before another build
    // took it up (and gave it back without a new error) takes nothing.
    let retaken = since.iter().any(|event| {
        event.id > error.id
            && event.kind == EventKind::LeaseAcquired
            && event.payload.get(BUILD_RETRY_KEY).is_some()
    });
    if retaken {
        return None;
    }
    let seen = since.iter().any(|event| match event.kind.known() {
        Some(EventKind::IntegrationError) => event.payload["build"] == build,
        Some(EventKind::LeaseAcquired) => event.payload[BUILD_RETRY_KEY]["build"] == build,
        _ => false,
    });
    (!seen).then(|| BuildRetry {
        error: error.id,
        error_build: error_build.to_owned(),
        build: build.to_owned(),
    })
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
    use crate::domain::{EventActor, TaskId};

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
        assert_eq!(judge(seen(), &handed_back(), None), Stranded::TakeUp);
        // Its e2e retried before the hand-back changes nothing.
        let events = with(
            handed_back(),
            EventKind::RunE2eFinished,
            json!({"outcome": "unavailable"}),
        );
        assert_eq!(judge(seen(), &events, None), Stranded::TakeUp);
        // A send-back judged converged past the revises (ADR-t2599-1)
        // lands as a pass does.
        let mut converged = handed_back();
        converged[2].payload = json!({"verdict": "revise"});
        let converged = with(
            converged,
            EventKind::ReviseLimitJudged,
            json!({"converged": true}),
        );
        assert_eq!(kept(&converged, None), None);
    }

    #[test]
    fn a_run_leased_or_not_awaiting_integration_is_not_judged() {
        let leased = Seen {
            leased: true,
            ..seen()
        };
        assert_eq!(judge(leased, &handed_back(), None), Stranded::No);
        for status in [
            RunStatus::NeedsSession,
            RunStatus::Integrating,
            RunStatus::Running,
        ] {
            let other = Seen { status, ..seen() };
            assert_eq!(judge(other, &handed_back(), None), Stranded::No);
        }
    }

    #[test]
    fn what_the_queue_says_of_the_run_keeps_it_for_a_person() {
        let open = Seen {
            ask_open: true,
            ..seen()
        };
        assert_eq!(
            judge(open, &handed_back(), None),
            Stranded::Keep(Keep::AskOpen)
        );
        let fenced = Seen {
            fenced: true,
            ..seen()
        };
        assert_eq!(
            judge(fenced, &handed_back(), None),
            Stranded::Keep(Keep::Fenced)
        );
        for task in [TaskStatus::Canceled, TaskStatus::Completed] {
            let ended = Seen { task, ..seen() };
            assert_eq!(
                judge(ended, &handed_back(), None),
                Stranded::Keep(Keep::TaskEnded)
            );
        }
    }

    #[test]
    fn a_review_that_does_not_let_it_land_keeps_it_for_a_person() {
        // Validated only: no review ran (a supervisor that does not review).
        assert_eq!(
            kept(&handed_back()[..1], None),
            Some(Keep::ReviewNotLanding)
        );
        // A review that started and gave nothing.
        assert_eq!(
            kept(&handed_back()[..2], None),
            Some(Keep::ReviewNotLanding)
        );
        assert_eq!(kept(&[], None), Some(Keep::ReviewNotLanding));
        // A revise past the third review needs a person's approval
        // (ADR-t2599-1), and a concern for a person.
        for verdict in [
            json!({"verdict": "revise"}),
            json!({"verdict": "concern", "recommendation": "ask", "confidence": "low"}),
        ] {
            let mut events = handed_back();
            events[2].payload = verdict;
            assert_eq!(kept(&events, None), Some(Keep::ReviewNotLanding));
        }
        // A pass its subagents sent elsewhere.
        let mut routed = handed_back();
        routed[2].payload = json!({"verdict": "pass", "route": {"destination": "ask", "agents": [[{"destination": "ask"}]]}});
        assert_eq!(kept(&routed, None), Some(Keep::ReviewNotLanding));
        // A conflict request sent to the session, and a precheck that
        // asked a person.
        let mut requested = handed_back()[..4].to_vec();
        requested[3].payload = json!({"requested": true});
        assert_eq!(kept(&requested, None), Some(Keep::ReviewNotLanding));
        let mut asked = handed_back()[..4].to_vec();
        asked[3].payload = json!({"asked": "past the limit"});
        assert_eq!(kept(&asked, None), Some(Keep::ReviewNotLanding));
        // A revise sent after the pass.
        let revising = with(handed_back(), EventKind::ReviseRequested, json!({}));
        assert_eq!(kept(&revising, None), Some(Keep::ReviewNotLanding));
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
            assert_eq!(kept(&events, None), Some(keep), "{kind:?}");
        }
        // A runtime error that let go of the run (a take-up that failed)
        // leaves it to a person; one that kept its lease is only a record.
        let given_up = with(
            handed_back(),
            EventKind::RuntimeError,
            json!({"message": "run r could not be taken up: x", "lease_released": true}),
        );
        assert_eq!(kept(&given_up, None), Some(Keep::GivenUp));
        assert_eq!(
            judge(seen(), &given_up, None),
            Stranded::Keep(Keep::GivenUp)
        );
        let recorded = with(
            handed_back(),
            EventKind::RuntimeError,
            json!({"message": "heartbeat", "lease_released": false}),
        );
        assert_eq!(kept(&recorded, None), None);
        // An eval that passed lets it land.
        let passed = with(
            handed_back(),
            EventKind::RunAgentEvalFinished,
            json!({"outcome": "passed"}),
        );
        assert_eq!(kept(&passed, None), None);
    }

    /// The `integration_error` of a landing an actor `requested_by` asked
    /// for, recording `payload`.
    fn error_of(id: i64, requested_by: Option<&str>, payload: Value) -> RunEvent {
        RunEvent {
            actor: Some(EventActor {
                role: "integrator".into(),
                id: "integrator:1".into(),
                requested_by: requested_by.map(str::to_owned),
            }),
            ..event(id, EventKind::IntegrationError, payload)
        }
    }

    /// A supervisor's landing that build `build` ran and an error of code
    /// `other` stopped before main moved.
    fn stopped(id: i64, build: &str) -> RunEvent {
        error_of(
            id,
            Some("supervisor:7"),
            json!({"code": "other", "build": build, "status": "awaiting_integration"}),
        )
    }

    /// A supervisor of build `build` took the run up again after error
    /// `error` of build `error_build`.
    fn retaken(id: i64, error: i64, error_build: &str, build: &str) -> RunEvent {
        event(
            id,
            EventKind::LeaseAcquired,
            json!({"reason": "stranded_landing", BUILD_RETRY_KEY:
                {"error_id": error, "error_build": error_build, "build": build}}),
        )
    }

    fn then(mut events: Vec<RunEvent>, event: RunEvent) -> Vec<RunEvent> {
        events.push(event);
        events
    }

    #[test]
    fn a_supervisors_landing_stopped_by_an_error_is_taken_up_once_by_another_build() {
        let events = then(handed_back(), stopped(8, "A"));
        assert_eq!(
            build_retry(&events, Some("B")),
            Some(BuildRetry {
                error: EventId::new(8),
                error_build: "A".into(),
                build: "B".into(),
            })
        );
        assert_eq!(judge(seen(), &events, Some("B")), Stranded::TakeUp);
        assert_eq!(
            retaken(9, 8, "A", "B").payload[BUILD_RETRY_KEY],
            build_retry(&events, Some("B")).unwrap().record()
        );
        // The same build, no build to judge with, and what the queue says
        // of the run (an open ask) still keep it.
        assert_eq!(kept(&events, Some("A")), Some(Keep::LandingStopped));
        assert_eq!(kept(&events, None), Some(Keep::LandingStopped));
        let open = Seen {
            ask_open: true,
            ..seen()
        };
        assert_eq!(
            judge(open, &events, Some("B")),
            Stranded::Keep(Keep::AskOpen)
        );
        // An error before the last review is no stop of this landing.
        let reviewed = then(
            then(
                then(handed_back()[..1].to_vec(), stopped(2, "B")),
                event(3, EventKind::ReviewFinished, json!({"verdict": "pass"})),
            ),
            stopped(4, "A"),
        );
        assert_eq!(judge(seen(), &reviewed, Some("B")), Stranded::TakeUp);
    }

    #[test]
    fn an_error_another_build_may_not_take_up_keeps_the_run_for_a_person() {
        let keeps = |events: &[RunEvent]| {
            assert_eq!(build_retry(events, Some("B")), None, "{events:?}");
            assert_eq!(kept(events, Some("B")), Some(Keep::LandingStopped));
        };
        // No build recorded, another code, a person's integrate, no actor.
        keeps(&then(
            handed_back(),
            error_of(8, Some("supervisor:7"), json!({"code": "other"})),
        ));
        keeps(&then(
            handed_back(),
            error_of(
                8,
                Some("supervisor:7"),
                json!({"code": "verify_failed", "build": "A"}),
            ),
        ));
        keeps(&then(
            handed_back(),
            error_of(8, Some("user"), json!({"code": "other", "build": "A"})),
        ));
        keeps(&then(
            handed_back(),
            error_of(8, None, json!({"code": "other", "build": "A"})),
        ));
        // Another stop of the landing since the review, before or after
        // the error.
        for kind in [
            EventKind::IntegrationHeld,
            EventKind::IntegrationFailed,
            EventKind::IntegrationDeferred,
            EventKind::LandingDecided,
        ] {
            keeps(&then(
                then(handed_back(), event(8, kind, json!({"code": "other"}))),
                stopped(9, "A"),
            ));
            keeps(&then(
                then(handed_back(), stopped(8, "A")),
                event(9, kind, json!({"code": "other"})),
            ));
        }
        // An error of B before the latest one, of A.
        keeps(&then(then(handed_back(), stopped(8, "B")), stopped(9, "A")));
    }

    #[test]
    fn each_build_takes_a_run_up_at_most_once_since_its_last_review() {
        // B took it up and failed the same way again: B does not again.
        let again = then(
            then(
                then(handed_back(), stopped(8, "A")),
                retaken(9, 8, "A", "B"),
            ),
            stopped(10, "B"),
        );
        assert_eq!(kept(&again, Some("B")), Some(Keep::LandingStopped));
        // A, then B, then A: A was tried, and is not again.
        assert_eq!(kept(&again, Some("A")), Some(Keep::LandingStopped));
        assert_eq!(build_retry(&again, Some("A")), None);
        // A third build is tried once.
        assert_eq!(judge(seen(), &again, Some("C")), Stranded::TakeUp);
        // B took it up and gave it back before its landing (a restart):
        // judged again on the same events, B takes nothing, and neither
        // does C, which judged it before B took it up.
        let given_back = then(
            then(
                then(handed_back(), stopped(8, "A")),
                retaken(9, 8, "A", "B"),
            ),
            event(10, EventKind::LeaseReleased, json!({"reason": "finished"})),
        );
        assert_eq!(kept(&given_back, Some("B")), Some(Keep::LandingStopped));
        assert_eq!(build_retry(&given_back, Some("C")), None);
        assert_eq!(
            judge(seen(), &given_back, Some("C")),
            Stranded::Keep(Keep::LandingStopped)
        );
        // A new review starts the count again.
        let reviewed = then(
            again.clone(),
            event(11, EventKind::ReviewFinished, json!({"verdict": "pass"})),
        );
        assert_eq!(kept(&reviewed, Some("B")), None);
        let stopped_again = then(reviewed, stopped(12, "A"));
        assert_eq!(judge(seen(), &stopped_again, Some("B")), Stranded::TakeUp);
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
        assert_eq!(kept(&approved, None), Some(Keep::QueuedElsewhere));
        let recovered = with(
            handed_back(),
            EventKind::RunRecovered,
            json!({"previous_status": "integrating"}),
        );
        assert_eq!(kept(&recovered, None), Some(Keep::QueuedElsewhere));
        let retried = with(handed_back(), EventKind::ReviewRetryQueued, json!({}));
        assert_eq!(kept(&retried, None), Some(Keep::QueuedElsewhere));
    }
}
