//! What a run's events say about it: [`RunHistory`] folds the `run_events`
//! of one run (oldest first) into the facts the supervisor, `integrate` and
//! the health checks decide by, and the functions below make the decisions
//! that depend on how often something happened (the resume and revise
//! limits) or on whether the run was approved. The events are read by the
//! application; nothing here reads or writes them.

use serde_json::Value;

use crate::domain::resume::ResumeCount;
use crate::domain::{
    ASK_EVENT_KINDS, AskId, AttentionNext, EventId, MAX_RESUME_ATTEMPTS, MAX_REVISE_ATTEMPTS,
    RunEvent, RunStatus, TriageState, event_attention, event_kind, recheck, run_attention,
    triage_state,
};

/// The events of one run, oldest first, borrowed from whoever read them.
#[derive(Debug, Clone, Copy)]
pub struct RunHistory<'a> {
    events: &'a [RunEvent],
}

/// The events that park a run for a session with a reason of their own
/// (the landing or validation, or a person's `send_back`), and the
/// triage's resume.
const PARKING: [&str; 7] = [
    event_kind::INTEGRATION_DEFERRED,
    event_kind::INTEGRATION_ERROR,
    event_kind::EVIDENCE_MISSING,
    event_kind::SCOPE_VIOLATION,
    event_kind::LANDING_DECIDED,
    event_kind::TRIAGE_FINISHED,
    event_kind::TRIAGE_DECIDED,
];

/// The first five of [`PARKING`] and any landing recheck failure: what
/// parks a run that a resumed session may have resolved already.
const RESOLVABLE_PARKING: [&str; 6] = [
    event_kind::INTEGRATION_DEFERRED,
    event_kind::INTEGRATION_ERROR,
    event_kind::EVIDENCE_MISSING,
    event_kind::SCOPE_VIOLATION,
    event_kind::LANDING_DECIDED,
    event_kind::LANDING_RECHECK_FAILED,
];

const RESUMES: [&str; 3] = [
    event_kind::RESUME_STARTED,
    event_kind::RESUME_FINISHED,
    event_kind::RESUME_SKIPPED,
];

/// What kind of request the latest parking of a run makes of its resumed
/// session (see [`RunHistory::last_park`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkCause {
    /// The landing deferred it (a conflict, a failed verification).
    Landing,
    /// Required evidence is missing from the receipt.
    EvidenceMissing,
    /// The run changed paths outside the task's `--paths`.
    ScopeViolation,
    /// A person sent it back (`landing_decided`).
    SentBack,
    /// The triage (or a person answering it) chose resume.
    Triage,
    /// The landing recheck found that the waiting run no longer lands
    /// (ADR-0068 decision 3).
    Recheck,
}

/// Why a run waits for a session, from its latest parking event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Park<'a> {
    pub cause: ParkCause,
    /// The event's `reason` (the triage's `instruction`), when it has one.
    pub reason: Option<&'a str>,
}

/// The session an accepted run keeps open after its last resume (ADR-0027).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumedSession<'a> {
    /// No resume started, finished or was skipped: the worker's session.
    NotResumed,
    /// The resume that handed its live session to validation, whose
    /// workspace was not closed since.
    Open { workspace: &'a str, attempt: usize },
    /// Resumed, but no session of a resume is open.
    Closed,
}

impl<'a> RunHistory<'a> {
    pub fn from_events(events: &'a [RunEvent]) -> Self {
        Self { events }
    }

    pub fn events(&self) -> &'a [RunEvent] {
        self.events
    }

    /// Whether an event of `kind` was recorded.
    pub fn has(&self, kind: &str) -> bool {
        self.events.iter().any(|e| e.kind == kind)
    }

    /// How many events of `kind` were recorded.
    pub fn count(&self, kind: &str) -> usize {
        self.events.iter().filter(|e| e.kind == kind).count()
    }

    /// The latest event of `kind`.
    pub fn last(&self, kind: &str) -> Option<&'a RunEvent> {
        self.last_of(&[kind])
    }

    /// The latest event of any of `kinds`.
    pub fn last_of(&self, kinds: &[&str]) -> Option<&'a RunEvent> {
        self.events
            .iter()
            .rev()
            .find(|e| kinds.contains(&e.kind.as_str()))
    }

    /// The latest event of `kind` before event `id`.
    pub fn last_before(&self, id: EventId, kind: &str) -> Option<&'a RunEvent> {
        self.events
            .iter()
            .rev()
            .find(|e| e.id < id && e.kind == kind)
    }

    /// Whether an event of `kind` was recorded after event `id`.
    pub fn has_after(&self, id: EventId, kind: &str) -> bool {
        self.events.iter().any(|e| e.id > id && e.kind == kind)
    }

    /// Whether `integrate` was called on the run (`integration_approved`):
    /// the approval to land it (ADR-0016 decision 5).
    pub fn approved(&self) -> bool {
        self.has(event_kind::INTEGRATION_APPROVED)
    }

    /// Whether landing the run pushes `main`: unless an approving
    /// `integrate --no-push` recorded `push: false`.
    /// `integration_approved` is recorded once; the first one counts.
    pub fn landing_pushes(&self) -> bool {
        self.events
            .iter()
            .find(|e| e.kind == event_kind::INTEGRATION_APPROVED)
            .is_none_or(|e| e.payload.get("push") != Some(&Value::Bool(false)))
    }

    /// The resumed sessions started (`resume_started`), split by whether
    /// each counts toward [`MAX_RESUME_ATTEMPTS`] (ADR-0047 decision 24).
    pub fn resumes(&self) -> ResumeCount {
        ResumeCount::of(self.events)
    }

    /// How many `revise` verdicts were sent to the live session: each
    /// `revise_requested` but those a `revise_unsent` withdrew (the request
    /// is recorded before it is typed).
    pub fn revise_attempts(&self) -> usize {
        self.count(event_kind::REVISE_REQUESTED)
            .saturating_sub(self.count(event_kind::REVISE_UNSENT))
    }

    /// How many headless reviews were started.
    pub fn review_attempts(&self) -> usize {
        self.count(event_kind::REVIEW_STARTED)
    }

    /// How many headless triages were started.
    pub fn triage_attempts(&self) -> usize {
        self.count(event_kind::TRIAGE_STARTED)
    }

    /// How many conflict resolutions the precheck sent to the live
    /// session: each `conflict_precheck` with `requested: true` but those a
    /// later one with `unsent: true` withdrew (the request is recorded
    /// before it is typed).
    pub fn conflict_requests(&self) -> usize {
        let count = |key: &str| {
            self.events
                .iter()
                .filter(|e| e.kind == event_kind::CONFLICT_PRECHECK && e.payload[key] == true)
                .count()
        };
        count("requested").saturating_sub(count("unsent"))
    }

    /// Where the recovery of a `failed` or `interrupted` run stands
    /// ([`triage_state`]).
    pub fn triage_state(&self) -> TriageState {
        triage_state(self.events)
    }

    /// Whether the run's `/exit` request timed out with no session exit
    /// since.
    pub fn exit_pending(&self) -> bool {
        self.last_of(&[
            event_kind::EXIT_REQUEST_TIMED_OUT,
            event_kind::SESSION_EXITED,
        ])
        .is_some_and(|e| e.kind == event_kind::EXIT_REQUEST_TIMED_OUT)
    }

    /// Why the run waits for a session: its latest `integration_deferred`
    /// / `integration_error` / `evidence_missing` / `scope_violation` /
    /// `landing_decided` / `triage_finished` / `triage_decided`, or a
    /// landing recheck that resumed it ([`recheck::parks`]), and what kind
    /// of request that makes. A landing deferred for missing evidence names
    /// the `checks`, one deferred for its scope the paths; the triage's
    /// resume asks for its `instruction`.
    pub fn last_park(&self) -> Option<Park<'a>> {
        let event = self
            .events
            .iter()
            .rev()
            .find(|e| PARKING.contains(&e.kind.as_str()) || recheck::parks(e))?;
        let key = if event.kind == event_kind::TRIAGE_FINISHED {
            "instruction"
        } else {
            "reason"
        };
        let cause = if event.kind == event_kind::EVIDENCE_MISSING
            || event.payload.get("checks").is_some()
        {
            ParkCause::EvidenceMissing
        } else if event.kind == event_kind::SCOPE_VIOLATION
            || event.payload.get("scope_violation").is_some()
        {
            ParkCause::ScopeViolation
        } else if event.kind == event_kind::LANDING_DECIDED {
            ParkCause::SentBack
        } else if event.kind == event_kind::TRIAGE_FINISHED
            || event.kind == event_kind::TRIAGE_DECIDED
        {
            ParkCause::Triage
        } else if event.kind == event_kind::LANDING_RECHECK_FAILED {
            ParkCause::Recheck
        } else {
            ParkCause::Landing
        };
        Some(Park {
            cause,
            reason: event.payload.get(key).and_then(Value::as_str),
        })
    }

    /// Whether the run was last parked by the landing, its recheck or
    /// validation (not a person's `send_back`) and the last of those parking events,
    /// `resume_finished` and `resume_skipped` is a `resume_finished` with
    /// `outcome: unresolved`: a session ran after the parking, and may
    /// have resolved the run although it was not judged so.
    pub fn unresolved_since_park(&self) -> bool {
        let parked = self.last_of(&RESOLVABLE_PARKING);
        let last = self.events.iter().rev().find(|e| {
            RESOLVABLE_PARKING.contains(&e.kind.as_str())
                || e.kind == event_kind::RESUME_FINISHED
                || e.kind == event_kind::RESUME_SKIPPED
        });
        parked.is_some_and(|e| e.kind != event_kind::LANDING_DECIDED)
            && last.is_some_and(|e| {
                e.kind == event_kind::RESUME_FINISHED && e.payload["outcome"] == "unresolved"
            })
    }

    /// Whether the last resume event is `resume_skipped`: the run was
    /// moved on without a session, and no resume opened one since.
    pub fn last_resume_skipped(&self) -> bool {
        self.last_of(&RESUMES)
            .is_some_and(|e| e.kind == event_kind::RESUME_SKIPPED)
    }

    /// The session of the run's last resume (see [`ResumedSession`]).
    pub fn resumed_session(&self) -> ResumedSession<'a> {
        let Some(event) = self.last_of(&RESUMES) else {
            return ResumedSession::NotResumed;
        };
        if event.kind == event_kind::RESUME_FINISHED
            && event.payload["status"] == RunStatus::Validating.as_str()
            && let (Some(workspace), Some(attempt)) = (
                event.payload["workspace_id"].as_str(),
                event.payload["attempt"].as_u64(),
            )
        {
            let closed = self.events.iter().any(|e| {
                e.id > event.id
                    && e.kind == event_kind::WORKSPACE_CLOSED
                    && e.payload["workspace_id"] == workspace
            });
            if !closed {
                return ResumedSession::Open {
                    workspace,
                    attempt: attempt as usize,
                };
            }
        }
        ResumedSession::Closed
    }

    /// The `screen_hash` of the dialog the session waits at: a
    /// `prompt_waiting` with no `prompt_cleared` or `receipt_observed` since.
    pub fn waiting_prompt_hash(&self) -> Option<&'a str> {
        self.last_of(&[
            event_kind::PROMPT_WAITING,
            event_kind::PROMPT_CLEARED,
            event_kind::RECEIPT_OBSERVED,
        ])
        .filter(|e| e.kind == event_kind::PROMPT_WAITING)
        .and_then(|e| e.payload["screen_hash"].as_str())
    }

    /// The asks whose answer the supervisor could not type into the
    /// worker's terminal (`ask_delivery_failed`).
    pub fn failed_deliveries(&self) -> Vec<AskId> {
        self.events
            .iter()
            .filter(|e| e.kind == event_kind::ASK_DELIVERY_FAILED)
            .filter_map(|e| e.payload.get("ask_id").and_then(Value::as_i64))
            .map(AskId::new)
            .collect()
    }

    /// The `index` of every follow-up `integrate` registered already.
    pub fn registered_follow_ups(&self) -> Vec<u64> {
        self.events
            .iter()
            .filter(|e| e.kind == event_kind::FOLLOW_UP_REGISTERED)
            .filter_map(|e| e.payload["index"].as_u64())
            .collect()
    }

    /// The `error` of the latest `push_failed`.
    pub fn push_failure(&self) -> Option<&'a str> {
        self.last(event_kind::PUSH_FAILED)
            .and_then(|e| e.payload.get("error").and_then(Value::as_str))
    }
}

/// What waits about the latest run of an `in_progress` task in `status`,
/// from its history (see [`run_attention`]): what to do next, and the kind
/// of the event that brought it there (`None` when no event did, and the
/// caller names the status). A run whose triage finished waits for
/// nothing; a failed triage and a failed headless review are a person's.
pub fn run_attention_of<'a>(
    history: &RunHistory<'a>,
    status: RunStatus,
    leased: bool,
) -> Option<(AttentionNext, Option<&'a str>)> {
    let next = run_attention(status, history.exit_pending(), false, leased)?;
    let next = match (next, history.triage_state()) {
        (AttentionNext::Triaging, TriageState::Finished) => return None,
        (AttentionNext::Triaging, TriageState::Failed) => AttentionNext::TriageByHand,
        (next, _) => next,
    };
    let kind = history
        .events
        .iter()
        .rev()
        .find(|e| match next {
            // The error the owner gave up with, whatever its payload.
            AttentionNext::RecoverRun => e.kind == event_kind::RUNTIME_ERROR,
            // Whatever parked the run for a session last.
            AttentionNext::Resuming => {
                e.payload.get("status").and_then(Value::as_str)
                    == Some(RunStatus::NeedsSession.as_str())
            }
            // A failed review whose `approve_landing` ask was closed
            // without moving the run (task 328) is reviewed by hand.
            AttentionNext::ReviewAndIntegrate if e.kind == event_kind::REVIEW_FAILED => true,
            // An ask about the run is its own attention, not the run's.
            _ => {
                !ASK_EVENT_KINDS.contains(&e.kind.as_str())
                    && event_attention(&e.kind, &e.payload).is_some()
            }
        })
        .map(|e| e.kind.as_str());
    // After a failed headless review the run is a person's to review.
    let next = match next {
        AttentionNext::ReviewAndIntegrate if kind == Some(event_kind::REVIEW_FAILED) => {
            AttentionNext::ReviewByHand
        }
        next => next,
    };
    Some((next, kind))
}

/// What the conflict precheck does about a passed run whose head conflicts
/// with `main` (ADR-0027 decision 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictDecision {
    /// Send the live session the resolution request.
    RequestRebase,
    /// The conflict requests and the resumes together used up
    /// [`MAX_RESUME_ATTEMPTS`]: a person decides.
    Ask,
}

/// The conflict requests and the run's counted resumes share
/// [`MAX_RESUME_ATTEMPTS`]: past it, a person is asked. Resumes of a run
/// parked only by a conflict after its review passed are not counted
/// (ADR-0047 decision 24).
pub fn decide_conflict(history: &RunHistory<'_>) -> ConflictDecision {
    if history.conflict_requests() + history.resumes().counted >= MAX_RESUME_ATTEMPTS {
        ConflictDecision::Ask
    } else {
        ConflictDecision::RequestRebase
    }
}

/// What the supervisor does about a `revise` verdict (ADR-0027 decision 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviseDecision {
    /// Send revise `attempt` to the live session.
    Request { attempt: usize },
    /// [`MAX_REVISE_ATTEMPTS`] revises were sent already: a person decides.
    Ask,
}

pub fn decide_revise(history: &RunHistory<'_>) -> ReviseDecision {
    let revises = history.revise_attempts();
    if revises >= MAX_REVISE_ATTEMPTS {
        ReviseDecision::Ask
    } else {
        ReviseDecision::Request {
            attempt: revises + 1,
        }
    }
}

/// Where a run goes once its validation finished in `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterValidation {
    /// An approved run (its integrate was called) lands without a review
    /// (ADR-0027 decision 3).
    Land,
    /// An accepted run is reviewed by the headless review.
    Review,
    /// Anything else rests; `close` gives up the session's workspace (a run
    /// parked for a session, since a resume opens one of its own, or one
    /// that waits for a person's answer), otherwise it stays for inspection.
    Rest { close: bool },
}

/// `approved`: `integrate` was called on the run. `awaits_landing_answer`:
/// a landing recheck resumed the run without waiting for the answer to its
/// still unclosed `approve_landing` ask (ADR-0068 decision 4), so the
/// rebased run waits for that answer instead of a new review.
pub fn after_validation(
    status: RunStatus,
    approved: bool,
    awaits_landing_answer: bool,
) -> AfterValidation {
    match status {
        RunStatus::AwaitingIntegration if approved => AfterValidation::Land,
        RunStatus::AwaitingIntegration if awaits_landing_answer => {
            AfterValidation::Rest { close: true }
        }
        RunStatus::AwaitingIntegration => AfterValidation::Review,
        RunStatus::NeedsSession => AfterValidation::Rest { close: true },
        _ => AfterValidation::Rest { close: false },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
        }
    }

    fn kinds(kinds: &[&str]) -> Vec<RunEvent> {
        kinds
            .iter()
            .enumerate()
            .map(|(i, kind)| event(i as i64 + 1, kind, json!({})))
            .collect()
    }

    #[test]
    fn kinds_keep_their_public_names() {
        assert_eq!(event_kind::INTEGRATION_APPROVED, "integration_approved");
        assert_eq!(event_kind::RESUME_STARTED, "resume_started");
        assert_eq!(event_kind::OBSERVATION, "observation");
    }

    #[test]
    fn counts_and_approval() {
        let events = kinds(&[
            "resume_started",
            "revise_requested",
            "review_started",
            "resume_started",
            "triage_started",
        ]);
        let history = RunHistory::from_events(&events);
        assert_eq!(history.resumes().total(), 2);
        assert_eq!(history.resumes().counted, 2);
        assert_eq!(history.revise_attempts(), 1);
        assert_eq!(history.review_attempts(), 1);
        assert_eq!(history.triage_attempts(), 1);
        assert!(!history.approved());
        assert!(history.landing_pushes());
        assert!(history.has_after(EventId::new(1), "resume_started"));
        assert!(!history.has_after(EventId::new(4), "resume_started"));
        assert_eq!(
            history
                .last_before(EventId::new(4), "resume_started")
                .map(|e| e.id),
            Some(EventId::new(1))
        );
        assert_eq!(history.events().len(), 5);

        let events = vec![event(1, "integration_approved", json!({"push": false}))];
        let history = RunHistory::from_events(&events);
        assert!(history.approved());
        assert!(!history.landing_pushes());
        let events = vec![event(1, "integration_approved", json!({"push": true}))];
        assert!(RunHistory::from_events(&events).landing_pushes());
    }

    #[test]
    fn triage_state_follows_the_latest_triage_or_resume() {
        let state = |k: &[&str]| RunHistory::from_events(&kinds(k)).triage_state();
        assert_eq!(state(&[]), TriageState::Pending);
        assert_eq!(state(&["triage_started"]), TriageState::Pending);
        assert_eq!(state(&["triage_failed"]), TriageState::Failed);
        assert_eq!(
            state(&["triage_failed", "triage_finished"]),
            TriageState::Finished
        );
        assert_eq!(
            state(&["triage_finished", "resume_started", "session_exited"]),
            TriageState::Pending
        );
    }

    #[test]
    fn exit_pending_until_the_session_exits() {
        let pending = |k: &[&str]| RunHistory::from_events(&kinds(k)).exit_pending();
        assert!(!pending(&[]));
        assert!(pending(&["exit_requested", "exit_request_timed_out"]));
        assert!(!pending(&["exit_request_timed_out", "session_exited"]));
    }

    #[test]
    fn conflict_requests_and_resumes_share_the_limit() {
        let precheck =
            |id, requested| event(id, "conflict_precheck", json!({"requested": requested}));
        let events = vec![precheck(1, true), precheck(2, false)];
        let history = RunHistory::from_events(&events);
        assert_eq!(history.conflict_requests(), 1);
        assert_eq!(decide_conflict(&history), ConflictDecision::RequestRebase);
        // A request withdrawn before it was typed is not one.
        let events = vec![
            precheck(1, true),
            event(2, "conflict_precheck", json!({"unsent": true})),
        ];
        assert_eq!(RunHistory::from_events(&events).conflict_requests(), 0);
        let events = vec![
            precheck(1, true),
            event(2, "resume_started", json!({})),
            precheck(3, true),
        ];
        let history = RunHistory::from_events(&events);
        assert_eq!(decide_conflict(&history), ConflictDecision::Ask);
    }

    #[test]
    fn revises_stop_at_the_limit() {
        let decide = |k: &[&str]| decide_revise(&RunHistory::from_events(&kinds(k)));
        assert_eq!(decide(&[]), ReviseDecision::Request { attempt: 1 });
        assert_eq!(
            decide(&["revise_requested"]),
            ReviseDecision::Request { attempt: 2 }
        );
        assert_eq!(
            decide(&["revise_requested", "revise_requested"]),
            ReviseDecision::Ask
        );
        // A revise withdrawn before it was typed does not count.
        assert_eq!(
            decide(&["revise_requested", "revise_unsent", "revise_requested"]),
            ReviseDecision::Request { attempt: 2 }
        );
    }

    #[test]
    fn resumes_after_a_conflict_only_park_do_not_share_the_conflict_limit() {
        let events = vec![
            event(1, "review_finished", json!({"verdict": "pass"})),
            event(
                2,
                "integration_deferred",
                json!({"code": "rebase_conflict"}),
            ),
            event(3, "resume_started", json!({})),
            event(4, "conflict_precheck", json!({"requested": true})),
            event(5, "conflict_precheck", json!({"requested": true})),
        ];
        let history = RunHistory::from_events(&events);
        assert_eq!(history.resumes().counted, 0);
        assert_eq!(history.resumes().conflict_only, 1);
        assert_eq!(decide_conflict(&history), ConflictDecision::RequestRebase);
    }

    #[test]
    fn after_validation_lands_reviews_or_rests() {
        use RunStatus::*;
        assert_eq!(
            after_validation(AwaitingIntegration, true, true),
            AfterValidation::Land
        );
        assert_eq!(
            after_validation(AwaitingIntegration, false, false),
            AfterValidation::Review
        );
        assert_eq!(
            after_validation(AwaitingIntegration, false, true),
            AfterValidation::Rest { close: true }
        );
        assert_eq!(
            after_validation(NeedsSession, true, false),
            AfterValidation::Rest { close: true }
        );
        assert_eq!(
            after_validation(Failed, false, false),
            AfterValidation::Rest { close: false }
        );
    }

    #[test]
    fn last_park_names_the_request() {
        let park = |events: Vec<RunEvent>| {
            RunHistory::from_events(&events)
                .last_park()
                .map(|p| (p.cause, p.reason.map(str::to_owned)))
        };
        assert_eq!(park(Vec::new()), None);
        assert_eq!(
            park(vec![event(
                1,
                "integration_deferred",
                json!({"reason": "conflict"})
            )]),
            Some((ParkCause::Landing, Some("conflict".into())))
        );
        assert_eq!(
            park(vec![event(
                1,
                "integration_deferred",
                json!({"reason": "r", "checks": ["e2e"]})
            )]),
            Some((ParkCause::EvidenceMissing, Some("r".into())))
        );
        assert_eq!(
            park(vec![event(1, "evidence_missing", json!({}))]),
            Some((ParkCause::EvidenceMissing, None))
        );
        assert_eq!(
            park(vec![event(
                1,
                "integration_deferred",
                json!({"reason": "r", "scope_violation": ["x"]})
            )]),
            Some((ParkCause::ScopeViolation, Some("r".into())))
        );
        assert_eq!(
            park(vec![event(1, "scope_violation", json!({"reason": "s"}))]),
            Some((ParkCause::ScopeViolation, Some("s".into())))
        );
        assert_eq!(
            park(vec![event(1, "landing_decided", json!({"reason": "back"}))]),
            Some((ParkCause::SentBack, Some("back".into())))
        );
        assert_eq!(
            park(vec![event(
                1,
                "triage_finished",
                json!({"reason": "why", "instruction": "do"})
            )]),
            Some((ParkCause::Triage, Some("do".into())))
        );
        assert_eq!(
            park(vec![event(
                1,
                "triage_decided",
                json!({"reason": "person"})
            )]),
            Some((ParkCause::Triage, Some("person".into())))
        );
        assert_eq!(
            park(vec![
                event(1, "evidence_missing", json!({})),
                event(2, "integration_error", json!({"reason": "later"})),
            ]),
            Some((ParkCause::Landing, Some("later".into())))
        );
        // Only a recheck that resumed the run parks it.
        let recheck = |id, action: &str| {
            event(
                id,
                "landing_recheck_failed",
                json!({"action": action, "reason": "moved"}),
            )
        };
        assert_eq!(
            park(vec![recheck(1, recheck::RESUMED)]),
            Some((ParkCause::Recheck, Some("moved".into())))
        );
        assert_eq!(park(vec![recheck(1, recheck::HELD)]), None);
    }

    #[test]
    fn unresolved_since_park_needs_an_unresolved_resume_after_a_system_park() {
        let unresolved =
            |events: Vec<RunEvent>| RunHistory::from_events(&events).unresolved_since_park();
        let finished =
            |id, outcome: &str| event(id, "resume_finished", json!({"outcome": outcome}));
        let deferred = |id| event(id, "integration_deferred", json!({}));
        assert!(!unresolved(Vec::new()));
        assert!(!unresolved(vec![deferred(1)]));
        assert!(unresolved(vec![deferred(1), finished(2, "unresolved")]));
        assert!(!unresolved(vec![deferred(1), finished(2, "error")]));
        assert!(!unresolved(vec![
            deferred(1),
            finished(2, "unresolved"),
            event(3, "resume_skipped", json!({}))
        ]));
        assert!(!unresolved(vec![
            event(1, "landing_decided", json!({})),
            finished(2, "unresolved")
        ]));
        assert!(!unresolved(vec![finished(1, "unresolved")]));
        assert!(unresolved(vec![
            event(1, "landing_recheck_failed", json!({})),
            finished(2, "unresolved")
        ]));
    }

    #[test]
    fn resumed_session_is_the_open_workspace_of_the_last_resume() {
        fn session(events: &[RunEvent]) -> ResumedSession<'_> {
            RunHistory::from_events(events).resumed_session()
        }
        assert_eq!(session(&[]), ResumedSession::NotResumed);
        let validating = event(
            1,
            "resume_finished",
            json!({"status": "validating", "workspace_id": "w", "attempt": 2}),
        );
        assert_eq!(
            session(std::slice::from_ref(&validating)),
            ResumedSession::Open {
                workspace: "w",
                attempt: 2
            }
        );
        let closed = event(2, "workspace_closed", json!({"workspace_id": "w"}));
        assert_eq!(
            session(&[validating.clone(), closed]),
            ResumedSession::Closed
        );
        let other = event(2, "workspace_closed", json!({"workspace_id": "x"}));
        assert!(matches!(
            session(&[validating, other]),
            ResumedSession::Open { .. }
        ));
        assert_eq!(session(&kinds(&["resume_started"])), ResumedSession::Closed);
        let skipped = kinds(&["resume_finished", "resume_skipped"]);
        assert_eq!(session(&skipped), ResumedSession::Closed);
        assert!(RunHistory::from_events(&skipped).last_resume_skipped());
        assert!(
            !RunHistory::from_events(&kinds(&["resume_skipped", "resume_started"]))
                .last_resume_skipped()
        );
    }

    #[test]
    fn workspaces_prompts_deliveries_follow_ups_and_pushes() {
        let events = vec![
            event(
                1,
                "resume_finished",
                json!({"workspace_id": "a", "workspace_closed": true}),
            ),
            event(
                2,
                "resume_finished",
                json!({"workspace_id": "b", "workspace_closed": false}),
            ),
            event(3, "resume_finished", json!({"workspace_id": "c"})),
            event(4, "prompt_waiting", json!({"screen_hash": "h"})),
            event(5, "ask_delivery_failed", json!({"ask_id": 7})),
            event(6, "follow_up_registered", json!({"index": 0})),
            event(7, "push_failed", json!({"error": "old"})),
            event(8, "push_failed", json!({"error": "new"})),
        ];
        let history = RunHistory::from_events(&events);
        assert_eq!(history.waiting_prompt_hash(), Some("h"));
        assert_eq!(history.failed_deliveries(), [AskId::new(7)]);
        assert_eq!(history.registered_follow_ups(), [0]);
        assert_eq!(history.push_failure(), Some("new"));
        let events = kinds(&["prompt_waiting", "prompt_cleared"]);
        assert_eq!(RunHistory::from_events(&events).waiting_prompt_hash(), None);
    }

    #[test]
    fn run_attention_names_the_event_and_refines_the_next_step() {
        let attention = |events: &[RunEvent], status, leased| {
            run_attention_of(&RunHistory::from_events(events), status, leased)
                .map(|(next, kind)| (next, kind.map(str::to_owned)))
        };
        // A finished triage waits for nothing; a failed one for a person.
        assert_eq!(
            attention(&kinds(&["triage_finished"]), RunStatus::Failed, false),
            None
        );
        assert_eq!(
            attention(
                &[event(1, "triage_failed", json!({}))],
                RunStatus::Failed,
                false
            ),
            Some((AttentionNext::TriageByHand, Some("triage_failed".into())))
        );
        assert_eq!(
            attention(&[], RunStatus::Interrupted, false),
            Some((AttentionNext::Triaging, None))
        );
        // Unleased and unfinished: the error the owner gave up with.
        assert_eq!(
            attention(
                &[
                    event(1, "runtime_error", json!({"message": "x"})),
                    event(2, "lease_acquired", json!({})),
                ],
                RunStatus::Running,
                false
            ),
            Some((AttentionNext::RecoverRun, Some("runtime_error".into())))
        );
        // Whatever parked the run for a session last.
        assert_eq!(
            attention(
                &[event(
                    1,
                    "integration_deferred",
                    json!({"status": "needs_session"})
                )],
                RunStatus::NeedsSession,
                true
            ),
            Some((AttentionNext::Resuming, Some("integration_deferred".into())))
        );
        // A failed review is reviewed by hand.
        assert_eq!(
            attention(
                &[event(1, "review_failed", json!({}))],
                RunStatus::AwaitingIntegration,
                false
            ),
            Some((AttentionNext::ReviewByHand, Some("review_failed".into())))
        );
        // A run asked to exit waits in its stuck_exit ask.
        assert_eq!(
            attention(
                &kinds(&["exit_request_timed_out"]),
                RunStatus::AwaitingIntegration,
                true
            ),
            None
        );
    }
}
