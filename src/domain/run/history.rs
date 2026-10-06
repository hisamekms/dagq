//! What a run's events say about it: [`RunHistory`] folds the `run_events`
//! of one run (oldest first) into the facts the supervisor, `integrate` and
//! the health checks decide by, and the functions below make the decisions
//! that depend on how often something happened (the resume and revise
//! limits) or on whether the run was approved. The events are read by the
//! application; nothing here reads or writes them.

use super::payload::{
    AskOf, AskOpened, FollowUpRegistered, HeadRewritten, IntegrationApproved, Parking, PushFailed,
    ResumeFinished, RunRecovered, StatusOf, WorkspaceClosed, restore,
};
use crate::domain::resume::{ResumeConfig, ResumeCount};
use crate::domain::{
    ASK_EVENT_KINDS, AskId, AttentionNext, EventId, MAX_RESUME_ATTEMPTS, MAX_REVISE_ATTEMPTS,
    RunEvent, RunStatus, TriageState, abandon_left_session_open, event_attention, event_kind,
    recheck, run_attention, triage_state,
};

/// The events of one run, oldest first, borrowed from whoever read them.
#[derive(Debug, Clone, Copy)]
pub struct RunHistory<'a> {
    events: &'a [RunEvent],
}

/// The events that park a run for a session with a reason of their own
/// (the landing or validation, or a person's `send_back`), the triage's
/// resume, an adopter's park of a run whose workspace was gone, and the
/// runtime's e2e that failed after the review (ADR-t1233-2).
const PARKING: [&str; 10] = [
    event_kind::INTEGRATION_DEFERRED,
    event_kind::INTEGRATION_ERROR,
    event_kind::EVIDENCE_MISSING,
    event_kind::SCOPE_VIOLATION,
    event_kind::LANDING_DECIDED,
    event_kind::TRIAGE_FINISHED,
    event_kind::TRIAGE_DECIDED,
    event_kind::RECOVERY_PARKED,
    event_kind::SESSION_GONE_PARKED,
    event_kind::RUN_E2E_FAILED,
];

/// The first five of [`PARKING`], any landing recheck failure and the
/// runtime's failed e2e: what parks a run that a resumed session may have
/// resolved already.
const RESOLVABLE_PARKING: [&str; 7] = [
    event_kind::INTEGRATION_DEFERRED,
    event_kind::INTEGRATION_ERROR,
    event_kind::EVIDENCE_MISSING,
    event_kind::SCOPE_VIOLATION,
    event_kind::LANDING_DECIDED,
    event_kind::LANDING_RECHECK_FAILED,
    event_kind::RUN_E2E_FAILED,
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
    /// An adopter found the workspace of its session gone while it could
    /// not land without that session (task 960).
    SessionGone,
    /// The e2e the runtime ran on the host after the review failed
    /// (ADR-t1233-2 decision 3).
    E2e,
}

/// Why a run waits for a session, from its latest parking event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Park<'a> {
    pub cause: ParkCause,
    /// The event's `reason` (the triage's `instruction`), when it has one.
    pub reason: Option<&'a str>,
}

/// What becomes of a run recovered from `integrating`
/// ([`RunHistory::recovered_landing`]), with its `run_recovered`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveredLanding {
    /// Approved or passed: it waits to land again, without a person.
    Land(EventId),
    /// Neither: the supervisor reviews it, as a run just validated.
    Review(EventId),
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

    /// The approval of an `approve_landing` answer that queues the run to
    /// land (task 949): the latest `integration_approved` naming the ask
    /// (`ask_id`) when nothing after it started a landing, a review or a
    /// resume, decided the landing, or opened another `approve_landing`
    /// ask. The supervisor lands the queued runs by this event, the oldest
    /// first; `None` when the run is not queued so.
    pub fn queued_approval(&self) -> Option<EventId> {
        let approved = self.events.iter().rev().find(|e| {
            e.kind == event_kind::INTEGRATION_APPROVED
                && restore::<IntegrationApproved>(&e.payload).names_ask()
        })?;
        (!self.moved_on_after(approved.id)).then_some(approved.id)
    }

    /// Whether something after event `id` started a landing, a review or a
    /// resume, decided the landing, or opened an `approve_landing` ask: a
    /// run queued by that event is not queued any more.
    fn moved_on_after(&self, id: EventId) -> bool {
        self.events.iter().any(|e| {
            e.id > id
                && (matches!(
                    e.kind.as_str(),
                    event_kind::INTEGRATION_STARTED
                        | event_kind::REVIEW_STARTED
                        | event_kind::RESUME_STARTED
                        | event_kind::LANDING_DECIDED
                ) || e.kind == event_kind::ASK_OPENED
                    && restore::<AskOpened>(&e.payload).kind == Some("approve_landing"))
        })
    }

    /// What the supervisor does with a run whose landing was given up
    /// while it was `integrating` (task 1118): the latest `run_recovered`
    /// with `previous_status: integrating` (the supervisor's, or a
    /// person's `recover`) when nothing moved on from it
    /// ([`Self::moved_on_after`]). The run lands again when the Integrator
    /// would land it ([`Self::landable`]), and is reviewed
    /// otherwise. `None` when the run was not recovered so.
    pub fn recovered_landing(&self) -> Option<RecoveredLanding> {
        let recovered = self.events.iter().rev().find(|e| {
            e.kind == event_kind::RUN_RECOVERED
                && restore::<RunRecovered>(&e.payload).previous_status
                    == Some(RunStatus::Integrating.as_str())
        })?;
        if self.moved_on_after(recovered.id) {
            return None;
        }
        Some(if self.landable() {
            RecoveredLanding::Land(recovered.id)
        } else {
            RecoveredLanding::Review(recovered.id)
        })
    }

    /// The event that queues the run to land without a person, the oldest
    /// first: a `land` answer ([`Self::queued_approval`]) or the recovery
    /// of a landing it may land again ([`Self::recovered_landing`]).
    pub fn queued_to_land(&self) -> Option<EventId> {
        self.queued_approval()
            .or_else(|| match self.recovered_landing() {
                Some(RecoveredLanding::Land(id)) => Some(id),
                _ => None,
            })
    }

    /// Whether `head` is what earlier landings of the run made of the
    /// receipt's `commit` (task 1118): the chain of their rebases
    /// (`integration_rebased`) and migration renumberings
    /// (`migration_renumbered`), oldest first, each from the head the
    /// previous one left, leads from `commit` to `head`. A landing that
    /// stopped after its rebase left the worktree there.
    pub fn landing_rewrote(&self, commit: &str, head: &str) -> bool {
        let mut current = commit.to_ascii_lowercase();
        for event in self.events.iter().filter(|e| {
            matches!(
                e.kind.as_str(),
                event_kind::INTEGRATION_REBASED | event_kind::MIGRATION_RENUMBERED
            )
        }) {
            let rewritten: HeadRewritten = restore(&event.payload);
            if rewritten.head_before == Some(current.as_str())
                && let Some(after) = rewritten.head_after
            {
                current = after.to_owned();
            }
        }
        current != commit.to_ascii_lowercase() && current == head
    }

    /// Whether the Integrator lands the run at a request (ADR-t728-2
    /// decision 3): it was approved to land, or its latest review passed
    /// or recommended to land in a way the runtime applies (ADR-t451-1
    /// decision 3).
    pub fn landable(&self) -> bool {
        self.approved() || self.review_lets_land()
    }

    /// Whether the latest review lets the run land without a person
    /// ([`crate::domain::concern::lets_land`]).
    pub fn review_lets_land(&self) -> bool {
        self.last(event_kind::REVIEW_FINISHED)
            .is_some_and(|review| crate::domain::concern::lets_land(&review.payload))
    }

    /// Whether `integration_approved` was recorded for the answer of ask
    /// `ask_id`.
    pub fn approved_by_ask(&self, ask_id: AskId) -> bool {
        self.events.iter().any(|e| {
            e.kind == event_kind::INTEGRATION_APPROVED
                && restore::<IntegrationApproved>(&e.payload).ask_id() == Some(ask_id.as_i64())
        })
    }

    /// Whether landing the run pushes `main`: unless an approving
    /// `integrate --no-push` recorded `push: false`.
    /// `integration_approved` is recorded once; the first one counts.
    pub fn landing_pushes(&self) -> bool {
        self.events
            .iter()
            .find(|e| e.kind == event_kind::INTEGRATION_APPROVED)
            .is_none_or(|e| restore::<IntegrationApproved>(&e.payload).push != Some(false))
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

    /// [`Self::revise_attempts`] of the current review round: only the
    /// revises after the round's boundary, the last `resume_started`,
    /// `landing_decided` with `status: needs_session` (a person's
    /// `send_back`) or `conflict_resolved` (ADR-0050 decisions 1 and 2).
    pub fn round_revise_attempts(&self) -> usize {
        let start = self
            .events
            .iter()
            .rposition(|e| match e.kind.as_str() {
                event_kind::RESUME_STARTED | event_kind::CONFLICT_RESOLVED => true,
                event_kind::LANDING_DECIDED => {
                    restore::<StatusOf>(&e.payload).status == Some(RunStatus::NeedsSession.as_str())
                }
                _ => false,
            })
            .map_or(0, |i| i + 1);
        let round = &self.events[start..];
        let count = |kind| round.iter().filter(|e| e.kind == kind).count();
        count(event_kind::REVISE_REQUESTED).saturating_sub(count(event_kind::REVISE_UNSENT))
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
    /// before it is typed). [`ResumeCount::conflict_requests`].
    pub fn conflict_requests(&self) -> usize {
        self.resumes().conflict_requests
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
    /// the `checks`, one deferred for its scope the paths; the recovery
    /// job's resume (of a run that ended, or of a live session parked by
    /// it) asks for its `instruction`.
    pub fn last_park(&self) -> Option<Park<'a>> {
        let event = self
            .events
            .iter()
            .rev()
            .find(|e| PARKING.contains(&e.kind.as_str()) || recheck::parks(e))?;
        let parking: Parking = restore(&event.payload);
        let reason = if event.kind == event_kind::TRIAGE_FINISHED
            || event.kind == event_kind::RECOVERY_PARKED
        {
            parking.instruction
        } else {
            parking.reason
        };
        let cause = if event.kind == event_kind::EVIDENCE_MISSING || parking.names_checks {
            ParkCause::EvidenceMissing
        } else if event.kind == event_kind::SCOPE_VIOLATION || parking.names_scope_violation {
            ParkCause::ScopeViolation
        } else if event.kind == event_kind::LANDING_DECIDED {
            ParkCause::SentBack
        } else if event.kind == event_kind::TRIAGE_FINISHED
            || event.kind == event_kind::TRIAGE_DECIDED
            || event.kind == event_kind::RECOVERY_PARKED
        {
            ParkCause::Triage
        } else if event.kind == event_kind::LANDING_RECHECK_FAILED {
            ParkCause::Recheck
        } else if event.kind == event_kind::SESSION_GONE_PARKED {
            ParkCause::SessionGone
        } else if event.kind == event_kind::RUN_E2E_FAILED {
            ParkCause::E2e
        } else {
            ParkCause::Landing
        };
        Some(Park { cause, reason })
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
                e.kind == event_kind::RESUME_FINISHED
                    && restore::<ResumeFinished>(&e.payload).outcome == Some("unresolved")
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
        let finished: ResumeFinished = restore(&event.payload);
        if event.kind == event_kind::RESUME_FINISHED
            && finished.status == Some(RunStatus::Validating.as_str())
            && let (Some(workspace), Some(attempt)) = (finished.workspace_id, finished.attempt)
        {
            let closed = self.events.iter().any(|e| {
                e.id > event.id
                    && e.kind == event_kind::WORKSPACE_CLOSED
                    && restore::<WorkspaceClosed>(&e.payload).workspace_id == Some(workspace)
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

    /// The asks whose answer the supervisor could not type into the
    /// worker's terminal (`ask_delivery_failed`).
    pub fn failed_deliveries(&self) -> Vec<AskId> {
        self.events
            .iter()
            .filter(|e| e.kind == event_kind::ASK_DELIVERY_FAILED)
            .filter_map(|e| restore::<AskOf>(&e.payload).ask_id)
            .map(AskId::new)
            .collect()
    }

    /// The `index` of every follow-up `integrate` registered already.
    pub fn registered_follow_ups(&self) -> Vec<u64> {
        self.events
            .iter()
            .filter(|e| e.kind == event_kind::FOLLOW_UP_REGISTERED)
            .filter_map(|e| restore::<FollowUpRegistered>(&e.payload).index)
            .collect()
    }

    /// Whether the supervisor gave the run up with its live session left
    /// open, the `/exit` not sent (task 237), and no session has exited
    /// since.
    pub fn session_left_open(&self) -> bool {
        self.events
            .iter()
            .rev()
            .take_while(|e| e.kind != event_kind::SESSION_EXITED)
            .any(|e| abandon_left_session_open(&e.kind, &e.payload))
    }

    /// The `error` of the latest `push_failed`.
    pub fn push_failure(&self) -> Option<&'a str> {
        self.last(event_kind::PUSH_FAILED)
            .and_then(|e| restore::<PushFailed>(&e.payload).error)
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
    let event = history.events.iter().rev().find(|e| match next {
        // The error the owner gave up with, whatever its payload.
        AttentionNext::RecoverRun => e.kind == event_kind::RUNTIME_ERROR,
        // Whatever parked the run for a session last.
        AttentionNext::Resuming => {
            restore::<StatusOf>(&e.payload).status == Some(RunStatus::NeedsSession.as_str())
        }
        // A failed review whose `approve_landing` ask was closed
        // without moving the run (task 328) is reviewed by hand.
        AttentionNext::ReviewAndIntegrate if e.kind == event_kind::REVIEW_FAILED => true,
        // An ask about the run is its own attention, not the run's.
        _ => {
            !ASK_EVENT_KINDS.contains(&e.kind.as_str())
                && event_attention(&e.kind, &e.payload).is_some()
        }
    });
    let kind = event.map(|e| e.kind.as_str());
    let next = match next {
        // After a failed headless review the run is a person's to review.
        AttentionNext::ReviewAndIntegrate if kind == Some(event_kind::REVIEW_FAILED) => {
            AttentionNext::ReviewByHand
        }
        // Given up on with its session left open (task 237): the session
        // is ended first, and its exit brings the run's own attention back.
        AttentionNext::ReviewAndIntegrate | AttentionNext::RecoverRun
            if history.session_left_open() =>
        {
            AttentionNext::ExitSession
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
    /// The conflict-only attempts (the requests and the conflict-only
    /// resumes) reached [`ResumeConfig::conflict_only_limit`] with counted
    /// resumes left, and the run can be retried with its branch carried
    /// over: nobody is asked, the run goes on to land, and a landing that
    /// conflicts parks it with its resumes used up, which retries it
    /// (ADR-0047 decision 24).
    Inherit,
    /// The counted resumes used up [`MAX_RESUME_ATTEMPTS`], or the
    /// conflict-only attempts their limit on a run that cannot be retried
    /// with its branch carried over: a person decides.
    Ask,
}

/// The precheck's requests are conflict-only attempts after a passed
/// review: they share [`ResumeConfig::conflict_only_limit`] (`config`) with the conflict-only
/// resumes and are not counted toward [`MAX_RESUME_ATTEMPTS`] (ADR-0047
/// decision 24). `inheritable` says whether the run can be retried with its
/// branch carried over (no run of its task was, and its head holds commits
/// on top of its base).
pub fn decide_conflict(
    history: &RunHistory<'_>,
    inheritable: bool,
    config: ResumeConfig,
) -> ConflictDecision {
    let resumes = history.resumes();
    if resumes.counted >= MAX_RESUME_ATTEMPTS {
        ConflictDecision::Ask
    } else if resumes.conflict_attempts() < config.conflict_only_limit {
        ConflictDecision::RequestRebase
    } else if inheritable {
        ConflictDecision::Inherit
    } else {
        ConflictDecision::Ask
    }
}

/// What the supervisor does about a `revise` verdict (ADR-0027 decision 2,
/// counted per review round by ADR-0050).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviseDecision {
    /// Send revise `round` of the review round to the live session;
    /// `attempt` numbers it across the run (`revise_requested` and
    /// `revise-<attempt>.txt`), never reusing an earlier round's number.
    Request { attempt: usize, round: usize },
    /// [`MAX_REVISE_ATTEMPTS`] revises were sent in this review round
    /// already: a person decides.
    Ask,
}

pub fn decide_revise(history: &RunHistory<'_>) -> ReviseDecision {
    let revises = history.round_revise_attempts();
    if revises >= MAX_REVISE_ATTEMPTS {
        ReviseDecision::Ask
    } else {
        ReviseDecision::Request {
            attempt: history.count(event_kind::REVISE_REQUESTED) + 1,
            round: revises + 1,
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
    use serde_json::{Value, json};

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
            actor: None,
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
    fn a_run_recovered_from_integrating_lands_again_when_landable_else_is_reviewed() {
        let recovered =
            |id: i64, from: &str| event(id, "run_recovered", json!({"previous_status": from}));
        let passed =
            |id: i64, verdict: &str| event(id, "review_finished", json!({"verdict": verdict}));
        let of = |events: &[RunEvent]| {
            let history = RunHistory::from_events(events);
            (history.recovered_landing(), history.queued_to_land())
        };
        assert_eq!(of(&[passed(1, "pass")]), (None, None));
        // Only a landing given up counts, not a review a dead supervisor left.
        assert_eq!(
            of(&[passed(1, "pass"), recovered(2, "awaiting_integration")]),
            (None, None)
        );
        let events = [
            passed(1, "pass"),
            event(2, "integration_started", json!({})),
            recovered(3, "integrating"),
        ];
        let land = Some(RecoveredLanding::Land(EventId::new(3)));
        assert_eq!(of(&events), (land, Some(EventId::new(3))));
        // An approval lands it whatever the review said.
        let events = [
            passed(1, "concern"),
            event(2, "integration_approved", json!({})),
            recovered(3, "integrating"),
        ];
        assert_eq!(of(&events), (land, Some(EventId::new(3))));
        // A run neither approved nor passed is reviewed, and not queued.
        let events = [passed(1, "concern"), recovered(2, "integrating")];
        assert_eq!(
            of(&events),
            (Some(RecoveredLanding::Review(EventId::new(2))), None)
        );
        // Once something moved on from it, the recovery queues nothing.
        for kind in ["integration_started", "review_started", "resume_started"] {
            let events = [
                passed(1, "pass"),
                recovered(2, "integrating"),
                event(3, kind, json!({})),
            ];
            assert_eq!(of(&events), (None, None), "{kind}");
        }
    }

    #[test]
    fn landing_rewrote_follows_the_landings_rebases_from_the_receipt_commit() {
        let rebased = |id: i64, before: &str, after: &str| {
            event(
                id,
                "integration_rebased",
                json!({"head_before": before, "head_after": after}),
            )
        };
        let events = [
            rebased(1, "aaa", "bbb"),
            event(
                2,
                "migration_renumbered",
                json!({"head_before": "bbb", "head_after": "ccc"}),
            ),
            rebased(3, "zzz", "yyy"),
            rebased(4, "ccc", "ddd"),
        ];
        let history = RunHistory::from_events(&events);
        assert!(history.landing_rewrote("AAA", "ddd"));
        assert!(!history.landing_rewrote("aaa", "ccc"));
        assert!(!history.landing_rewrote("aaa", "yyy"));
        assert!(!history.landing_rewrote("ddd", "ddd"));
        assert!(!RunHistory::from_events(&[]).landing_rewrote("aaa", "aaa"));
    }

    #[test]
    fn queued_approval_is_the_latest_ask_approval_nothing_moved_on_from() {
        let approved =
            |id: i64, ask: Value| event(id, "integration_approved", json!({"ask_id": ask}));
        let queued = |events: &[RunEvent]| RunHistory::from_events(events).queued_approval();
        // `integrate`'s own approval names no ask.
        assert_eq!(queued(&[event(1, "integration_approved", json!({}))]), None);
        assert_eq!(queued(&[approved(1, json!(null))]), None);
        let events = [
            approved(1, json!(3)),
            event(2, "landing_queued", json!({"via": "approve"})),
            event(3, "ask_opened", json!({"kind": "worker_question"})),
        ];
        assert_eq!(queued(&events), Some(EventId::new(1)));
        let history = RunHistory::from_events(&events);
        assert!(history.approved_by_ask(AskId::new(3)));
        assert!(!history.approved_by_ask(AskId::new(4)));
        for (kind, payload) in [
            ("integration_started", json!({})),
            ("review_started", json!({})),
            ("resume_started", json!({})),
            ("landing_decided", json!({})),
            ("ask_opened", json!({"kind": "approve_landing"})),
        ] {
            let events = [approved(1, json!(3)), event(2, kind, payload)];
            assert_eq!(queued(&events), None, "{kind}");
        }
        // A later approval queues the run again.
        let events = [
            approved(1, json!(3)),
            event(2, "integration_started", json!({})),
            approved(3, json!(5)),
        ];
        assert_eq!(queued(&events), Some(EventId::new(3)));
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
    fn conflict_requests_are_conflict_only_attempts() {
        let precheck =
            |id, requested| event(id, "conflict_precheck", json!({"requested": requested}));
        let events = vec![precheck(1, true), precheck(2, false)];
        let history = RunHistory::from_events(&events);
        assert_eq!(history.conflict_requests(), 1);
        assert_eq!(history.resumes().counted, 0);
        assert_eq!(history.resumes().conflict_attempts(), 1);
        assert_eq!(
            decide_conflict(&history, true, ResumeConfig::default()),
            ConflictDecision::RequestRebase
        );
        // A request withdrawn before it was typed is not one.
        let events = vec![
            precheck(1, true),
            event(2, "conflict_precheck", json!({"unsent": true})),
        ];
        assert_eq!(RunHistory::from_events(&events).conflict_requests(), 0);
        // Three requests and a resume no longer reach the three attempts.
        let events = vec![
            precheck(1, true),
            event(2, "resume_started", json!({})),
            precheck(3, true),
            precheck(4, true),
        ];
        let history = RunHistory::from_events(&events);
        assert_eq!(history.resumes().counted, 1);
        assert_eq!(
            decide_conflict(&history, true, ResumeConfig::default()),
            ConflictDecision::RequestRebase
        );
        // The requests and the conflict-only resumes share their limit:
        // past it, the run is retried with its branch, or a person asked
        // when it cannot be.
        let pass = event(1, "review_finished", json!({"verdict": "pass"}));
        let conflict = |id| {
            event(
                id,
                "integration_deferred",
                json!({"code": "rebase_conflict"}),
            )
        };
        let mut events = vec![pass, conflict(2), event(3, "resume_started", json!({}))];
        events.extend(
            (4..)
                .take(crate::domain::resume::CONFLICT_ONLY_RESUME_LIMIT - 2)
                .map(|id| precheck(id, true)),
        );
        let history = RunHistory::from_events(&events);
        assert_eq!(history.resumes().conflict_only, 1);
        assert_eq!(
            history.resumes().conflict_attempts(),
            crate::domain::resume::CONFLICT_ONLY_RESUME_LIMIT - 1
        );
        assert!(!history.resumes().exhausted(ResumeConfig::default()));
        assert_eq!(
            decide_conflict(&history, true, ResumeConfig::default()),
            ConflictDecision::RequestRebase
        );
        events.push(precheck(10, true));
        let history = RunHistory::from_events(&events);
        assert!(history.resumes().exhausted(ResumeConfig::default()));
        assert_eq!(
            decide_conflict(&history, true, ResumeConfig::default()),
            ConflictDecision::Inherit
        );
        assert_eq!(
            decide_conflict(&history, false, ResumeConfig::default()),
            ConflictDecision::Ask
        );
        // A higher configured limit keeps requesting.
        let higher = ResumeConfig {
            conflict_only_limit: crate::domain::resume::CONFLICT_ONLY_RESUME_LIMIT + 1,
        };
        assert!(!history.resumes().exhausted(higher));
        assert_eq!(
            decide_conflict(&history, true, higher),
            ConflictDecision::RequestRebase
        );
        // Counted resumes used up ask, whatever the conflicts.
        let events = vec![
            event(1, "resume_started", json!({})),
            event(2, "resume_started", json!({})),
            event(3, "resume_started", json!({})),
        ];
        let history = RunHistory::from_events(&events);
        assert_eq!(
            decide_conflict(&history, true, ResumeConfig::default()),
            ConflictDecision::Ask
        );
    }

    #[test]
    fn revises_stop_at_the_limit() {
        let decide = |k: &[&str]| decide_revise(&RunHistory::from_events(&kinds(k)));
        let request = |attempt, round| ReviseDecision::Request { attempt, round };
        assert_eq!(decide(&[]), request(1, 1));
        assert_eq!(decide(&["revise_requested"]), request(2, 2));
        assert_eq!(
            decide(&["revise_requested", "revise_requested"]),
            ReviseDecision::Ask
        );
        // A revise withdrawn before it was typed does not count, but its
        // number is not reused.
        assert_eq!(
            decide(&["revise_requested", "revise_unsent", "revise_requested"]),
            request(3, 2)
        );
        // Events that are not boundaries leave the round going on.
        assert_eq!(
            decide(&[
                "revise_requested",
                "revise_finished",
                "review_started",
                "resume_skipped",
                "revise_requested",
            ]),
            ReviseDecision::Ask
        );
    }

    #[test]
    fn revises_are_counted_again_after_a_boundary() {
        let two = || {
            vec![
                event(1, "revise_requested", json!({"attempt": 1})),
                event(2, "revise_requested", json!({"attempt": 2})),
            ]
        };
        let decide = |events: &[RunEvent]| decide_revise(&RunHistory::from_events(events));
        let boundaries = [
            ("resume_started", json!({"attempt": 1})),
            ("conflict_resolved", json!({})),
            ("landing_decided", json!({"status": "needs_session"})),
        ];
        for (kind, payload) in boundaries {
            let mut events = two();
            assert_eq!(decide(&events), ReviseDecision::Ask);
            events.push(event(3, kind, payload.clone()));
            // A new round: revise 1 of it, numbered after the run's two.
            assert_eq!(
                decide(&events),
                ReviseDecision::Request {
                    attempt: 3,
                    round: 1
                },
                "{kind}"
            );
            events.push(event(4, "revise_requested", json!({"attempt": 3})));
            assert_eq!(
                decide(&events),
                ReviseDecision::Request {
                    attempt: 4,
                    round: 2
                },
                "{kind}"
            );
            events.push(event(5, "revise_requested", json!({"attempt": 4})));
            assert_eq!(decide(&events), ReviseDecision::Ask, "{kind}");
            let history = RunHistory::from_events(&events);
            assert_eq!(history.revise_attempts(), 4);
            assert_eq!(history.round_revise_attempts(), 2);
        }
        // A landing decision that does not send the run back (a person's
        // `land`) is no boundary.
        let mut events = two();
        events.push(event(3, "landing_decided", json!({"status": "failed"})));
        assert_eq!(decide(&events), ReviseDecision::Ask);
    }

    #[test]
    fn conflict_only_resumes_and_requests_are_not_counted() {
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
        assert_eq!(history.resumes().conflict_attempts(), 3);
        assert_eq!(
            decide_conflict(&history, true, ResumeConfig::default()),
            ConflictDecision::RequestRebase
        );
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
    fn a_live_session_parked_by_its_recovery_job_asks_for_the_instruction() {
        let events = vec![event(
            1,
            "recovery_parked",
            json!({"reason": "parked", "instruction": "rerun the tests"}),
        )];
        let park = RunHistory::from_events(&events).last_park().unwrap();
        assert_eq!(park.cause, ParkCause::Triage);
        assert_eq!(park.reason, Some("rerun the tests"));
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
        assert_eq!(
            park(vec![event(
                1,
                "session_gone_parked",
                json!({"code": "session_gone", "reason": "gone"})
            )]),
            Some((ParkCause::SessionGone, Some("gone".into())))
        );
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
            event(5, "ask_delivery_failed", json!({"ask_id": 7})),
            event(6, "follow_up_registered", json!({"index": 0})),
            event(7, "push_failed", json!({"error": "old"})),
            event(8, "push_failed", json!({"error": "new"})),
        ];
        let history = RunHistory::from_events(&events);
        assert_eq!(history.failed_deliveries(), [AskId::new(7)]);
        assert_eq!(history.registered_follow_ups(), [0]);
        assert_eq!(history.push_failure(), Some("new"));
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
        // Given up on with its session left open (task 237): the session
        // is a person's to end first, until it exits.
        let left_open = event(
            1,
            "runtime_error",
            json!({"message": "x", "lease_released": true, "session": {"workspace_id": "w", "exit": "failed"}}),
        );
        for (status, then) in [
            (
                RunStatus::AwaitingIntegration,
                AttentionNext::ReviewAndIntegrate,
            ),
            (RunStatus::Validating, AttentionNext::RecoverRun),
        ] {
            assert_eq!(
                attention(std::slice::from_ref(&left_open), status, false),
                Some((AttentionNext::ExitSession, Some("runtime_error".into())))
            );
            assert_eq!(
                attention(
                    &[left_open.clone(), event(2, "session_exited", json!({}))],
                    status,
                    false
                ),
                Some((then, Some("runtime_error".into())))
            );
        }
        let sent = event(
            1,
            "runtime_error",
            json!({"message": "x", "lease_released": true, "session": {"workspace_id": "w", "exit": "sent"}}),
        );
        assert_eq!(
            attention(&[sent], RunStatus::AwaitingIntegration, false),
            Some((
                AttentionNext::ReviewAndIntegrate,
                Some("runtime_error".into())
            ))
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

    /// Recorded payloads are read as the earlier `serde_json::Value` reads
    /// read them (task 1551): a missing key, an extra key, `null`, a value
    /// of another type or a payload that is not an object reads as the
    /// key's absence; `checks` and `scope_violation` count when present at
    /// all.
    #[test]
    fn recorded_payloads_with_missing_extra_null_or_mistyped_keys_are_read_as_before() {
        fn history(events: &[RunEvent]) -> RunHistory<'_> {
            RunHistory::from_events(events)
        }

        // integration_approved: `ask_id` names an ask unless missing or
        // null, whatever its type; only an integer is the ask's id; only a
        // boolean `false` stops the push.
        for (payload, queued, by_ask_7, pushes) in [
            (json!({}), false, false, true),
            (json!({"ask_id": null, "push": null}), false, false, true),
            (json!({"ask_id": 7, "extra": [1]}), true, true, true),
            (json!({"ask_id": "7", "push": "false"}), true, false, true),
            (json!({"ask_id": 7.0, "push": 0}), true, false, true),
            (json!({"ask_id": -7, "push": false}), true, false, false),
            (json!(null), false, false, true),
            (json!([7, false]), false, false, true),
        ] {
            let events = [event(1, "integration_approved", payload.clone())];
            let h = history(&events);
            assert_eq!(h.queued_approval().is_some(), queued, "{payload}");
            assert_eq!(h.approved_by_ask(AskId::new(7)), by_ask_7, "{payload}");
            assert_eq!(h.landing_pushes(), pushes, "{payload}");
        }

        // ask_opened: a kind of another type moves nothing on.
        let approved = event(1, "integration_approved", json!({"ask_id": 7}));
        for (kind, moved) in [
            (json!("approve_landing"), true),
            (json!(["approve_landing"]), false),
            (Value::Null, false),
        ] {
            let events = [
                approved.clone(),
                event(2, "ask_opened", json!({"kind": kind, "ask_id": 8})),
            ];
            assert_eq!(history(&events).queued_approval().is_none(), moved);
        }

        // run_recovered with no or a mistyped previous status is not a
        // landing given up.
        for payload in [json!({}), json!({"previous_status": 1})] {
            let events = [event(1, "run_recovered", payload)];
            assert_eq!(history(&events).recovered_landing(), None);
        }

        // A rebase without its heads, or with heads of another type, is
        // skipped; the next one still continues the chain.
        let events = [
            event(1, "integration_rebased", json!({"head_before": "a"})),
            event(
                2,
                "integration_rebased",
                json!({"head_before": 1, "head_after": "x"}),
            ),
            event(
                3,
                "migration_renumbered",
                json!({"head_before": "a", "head_after": "b", "n": 1}),
            ),
        ];
        assert!(history(&events).landing_rewrote("A", "b"));

        // A landing_decided boundary needs `status: needs_session`.
        for (payload, round) in [
            (json!({}), 2),
            (json!({"status": null}), 2),
            (json!({"status": "needs_session", "reason": 1}), 1),
        ] {
            let events = [
                event(1, "revise_requested", json!({})),
                event(2, "landing_decided", payload),
                event(3, "revise_requested", json!({})),
            ];
            assert_eq!(history(&events).round_revise_attempts(), round);
        }

        // A park reads its reason only as a string; `checks` and
        // `scope_violation` count even when null.
        let park = |payload: Value| {
            let events = [event(1, "integration_deferred", payload)];
            history(&events)
                .last_park()
                .map(|p| (p.cause, p.reason.map(str::to_owned)))
        };
        assert_eq!(park(json!({"reason": 1})), Some((ParkCause::Landing, None)));
        assert_eq!(
            park(json!({"reason": "r", "checks": null})),
            Some((ParkCause::EvidenceMissing, Some("r".into())))
        );
        assert_eq!(
            park(json!({"scope_violation": null})),
            Some((ParkCause::ScopeViolation, None))
        );
        assert_eq!(park(json!("reason")), Some((ParkCause::Landing, None)));

        // resume_finished: an outcome or workspace of another type, or an
        // attempt that is not an unsigned integer, reads as absent.
        let parked = event(1, "integration_deferred", json!({}));
        for (outcome, unresolved) in [
            (json!("unresolved"), true),
            (json!(null), false),
            (json!(0), false),
        ] {
            let events = [
                parked.clone(),
                event(2, "resume_finished", json!({"outcome": outcome})),
            ];
            assert_eq!(history(&events).unresolved_since_park(), unresolved);
        }
        for attempt in [json!(-1), json!(2.0), json!("2"), Value::Null] {
            let events = [event(
                1,
                "resume_finished",
                json!({"status": "validating", "workspace_id": "w", "attempt": attempt}),
            )];
            assert_eq!(history(&events).resumed_session(), ResumedSession::Closed);
        }
        let events = [
            event(
                1,
                "resume_finished",
                json!({"status": "validating", "workspace_id": "w", "attempt": 1, "extra": {}}),
            ),
            event(2, "workspace_closed", json!({"workspace_id": ["w"]})),
        ];
        assert_eq!(
            history(&events).resumed_session(),
            ResumedSession::Open {
                workspace: "w",
                attempt: 1
            }
        );

        // Deliveries, follow-ups and push failures skip mistyped values.
        let events = [
            event(1, "ask_delivery_failed", json!({"ask_id": "7"})),
            event(2, "ask_delivery_failed", json!({"ask_id": 8})),
            event(3, "follow_up_registered", json!({"index": -1})),
            event(4, "follow_up_registered", json!({"index": 1.5})),
            event(5, "follow_up_registered", json!({"index": 2})),
            event(6, "push_failed", json!({"error": "old"})),
            event(7, "push_failed", json!({"error": {"code": 1}})),
        ];
        let h = history(&events);
        assert_eq!(h.failed_deliveries(), [AskId::new(8)]);
        assert_eq!(h.registered_follow_ups(), [2]);
        assert_eq!(h.push_failure(), None);

        // run_attention's `status: needs_session` of whatever event parked
        // the run: one of another type is not it.
        let events = [
            event(
                1,
                "integration_deferred",
                json!({"status": "needs_session"}),
            ),
            event(2, "integration_error", json!({"status": ["needs_session"]})),
        ];
        assert_eq!(
            run_attention_of(&history(&events), RunStatus::NeedsSession, false),
            Some((AttentionNext::Resuming, Some("integration_deferred")))
        );
    }
}
