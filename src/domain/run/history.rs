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
use crate::domain::EventKind;
use crate::domain::resume::{ResumeConfig, ResumeCount};
use crate::domain::{
    ASK_EVENT_KINDS, AskId, AttentionNext, EventId, MAX_RESUME_ATTEMPTS, ReviseLimit, RunEvent,
    RunStatus, TriageState, abandon_left_session_open, event_attention, recheck, run_attention,
    triage_state,
};

/// The events of one run, oldest first, borrowed from whoever read them.
#[derive(Debug, Clone, Copy)]
pub struct RunHistory<'a> {
    events: &'a [RunEvent],
}

/// The events that park a run for a session with a reason of their own
/// (the landing or validation, or a person's `send_back`), the triage's
/// resume, an adopter's park of a run whose workspace was gone, and the
/// runtime's e2e that failed after the review (ADR-t1233-2), and the parks
/// at a turn's end for a replanning or a hold.
const PARKING: [EventKind; 12] = [
    EventKind::IntegrationDeferred,
    EventKind::IntegrationError,
    EventKind::EvidenceMissing,
    EventKind::ScopeViolation,
    EventKind::LandingDecided,
    EventKind::TriageFinished,
    EventKind::TriageDecided,
    EventKind::RecoveryParked,
    EventKind::SessionGoneParked,
    EventKind::RunE2eFailed,
    EventKind::ReplanParked,
    EventKind::HoldParked,
];

/// The parks at a turn's end (ADR-t1521-2 decision 1, ADR-t1879-1 decision
/// 7): what came before one (an approval, a review, the answers owed) was
/// of the work before the fence.
const FENCE_PARKS: [EventKind; 2] = [EventKind::ReplanParked, EventKind::HoldParked];

/// The first five of [`PARKING`], the landing recheck's failures, the
/// runtime's failed e2e and the parks at a turn's end: what parks a run
/// that a resumed session may have resolved already. A park at a turn's
/// end is one so that a resume that ended unresolved before it
/// does not let the receipt written after its fence move the run on. A recheck failure the host kept from judging the code
/// ([`recheck::RecordedFailure::skipped`]) parks nothing, and
/// [`RunHistory::unresolved_since_park`] passes it over.
const RESOLVABLE_PARKING: [EventKind; 9] = [
    EventKind::IntegrationDeferred,
    EventKind::IntegrationError,
    EventKind::EvidenceMissing,
    EventKind::ScopeViolation,
    EventKind::LandingDecided,
    EventKind::LandingRecheckFailed,
    EventKind::RunE2eFailed,
    EventKind::ReplanParked,
    EventKind::HoldParked,
];

const RESUMES: [EventKind; 3] = [
    EventKind::ResumeStarted,
    EventKind::ResumeFinished,
    EventKind::ResumeSkipped,
];

/// Whether `event` ends a resume of an approved run so that it lands: a
/// `resume_finished` resolved, or a `resume_skipped` approved, after which
/// the run stays `needs_session` for its landing.
fn resolved_to_land(event: &RunEvent) -> bool {
    let stays = event.payload["status"] == RunStatus::NeedsSession.as_str();
    match event.kind.known() {
        Some(EventKind::ResumeFinished) => {
            stays && restore::<ResumeFinished>(&event.payload).outcome == Some("resolved")
        }
        Some(EventKind::ResumeSkipped) => stays && event.payload["approved"] == true,
        _ => false,
    }
}

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
    /// The trusted runtime parked it for a replanning at a turn's end and
    /// saved its work (ADR-t1521-2 decision 1); it resumes once the
    /// operation ends in a continuation or a withdrawal.
    Replan,
    /// The supervisor parked it at a turn's end for its task's hold
    /// (ADR-t1879-1 decision 7), its work left in place; it resumes once
    /// the hold is released.
    Hold,
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
    pub fn has(&self, kind: EventKind) -> bool {
        self.events.iter().any(|e| e.kind == kind)
    }

    /// How many events of `kind` were recorded.
    pub fn count(&self, kind: EventKind) -> usize {
        self.events.iter().filter(|e| e.kind == kind).count()
    }

    /// The latest event of `kind`.
    pub fn last(&self, kind: EventKind) -> Option<&'a RunEvent> {
        self.last_of(&[kind])
    }

    /// The latest event of any of `kinds`.
    pub fn last_of(&self, kinds: &[EventKind]) -> Option<&'a RunEvent> {
        self.events.iter().rev().find(|e| e.kind.is_in(kinds))
    }

    /// The latest event of `kind` before event `id`.
    pub fn last_before(&self, id: EventId, kind: EventKind) -> Option<&'a RunEvent> {
        self.events
            .iter()
            .rev()
            .find(|e| e.id < id && e.kind == kind)
    }

    /// Whether an event of `kind` was recorded after event `id`.
    pub fn has_after(&self, id: EventId, kind: EventKind) -> bool {
        self.events.iter().any(|e| e.id > id && e.kind == kind)
    }

    /// Whether `integrate` was called on the run (`integration_approved`):
    /// the approval to land it (ADR-0016 decision 5). An approval from
    /// before the run's latest park at a turn's end (`replan_parked` or
    /// `hold_parked`) does not count: it approved the work before the park,
    /// not what the session resumed after it adds (ADR-t1521-2,
    /// ADR-t1879-1).
    pub fn approved(&self) -> bool {
        self.since_fence_park()
            .iter()
            .any(|e| e.kind == EventKind::IntegrationApproved)
    }

    /// The events after the run's latest park at a turn's end
    /// ([`FENCE_PARKS`]), or all of them when it was never parked so.
    fn since_fence_park(&self) -> &'a [RunEvent] {
        let start = self
            .events
            .iter()
            .rposition(|e| e.kind.is_in(&FENCE_PARKS))
            .map_or(0, |at| at + 1);
        &self.events[start..]
    }

    /// The approval of an `approve_landing` answer that queues the run to
    /// land (task 949): the latest `integration_approved` naming the ask
    /// (`ask_id`) when nothing after it started a landing, a review or a
    /// resume, decided the landing, or opened another `approve_landing`
    /// ask. The supervisor lands the queued runs by this event, the oldest
    /// first; `None` when the run is not queued so.
    pub fn queued_approval(&self) -> Option<EventId> {
        let approved = self.events.iter().rev().find(|e| {
            e.kind == EventKind::IntegrationApproved
                && restore::<IntegrationApproved>(&e.payload).names_ask()
        })?;
        (!self.moved_on_after(approved.id)).then_some(approved.id)
    }

    /// Whether something after event `id` started a landing, a review or a
    /// resume, decided the landing, or opened an `approve_landing` ask: a
    /// run queued by that event is not queued any more. A landing a
    /// handoff's drain handed over (ADR-t3344-1) left the run queued as
    /// it was.
    fn moved_on_after(&self, id: EventId) -> bool {
        self.events.iter().any(|e| {
            e.id > id
                && (e.kind == EventKind::IntegrationStarted && !self.handed_over(e.id)
                    || matches!(
                        e.kind.known(),
                        Some(
                            EventKind::ReviewStarted
                                | EventKind::ResumeStarted
                                | EventKind::LandingDecided
                        )
                    )
                    || e.kind == EventKind::AskOpened
                        && restore::<AskOpened>(&e.payload).kind == Some("approve_landing"))
        })
    }

    /// Whether the landing `integration_started` `started` began was handed
    /// over by a handoff's drain (ADR-t3344-1): its end, the first
    /// `integration_handed_over` or `integration_started` after it, is an
    /// `integration_handed_over`.
    fn handed_over(&self, started: EventId) -> bool {
        self.events
            .iter()
            .find(|e| {
                e.id > started
                    && matches!(
                        e.kind.known(),
                        Some(EventKind::IntegrationHandedOver | EventKind::IntegrationStarted)
                    )
            })
            .is_some_and(|e| e.kind == EventKind::IntegrationHandedOver)
    }

    /// The `review_retry_queued` of a `retry_review` answer that queues the
    /// run's review again (ADR-t2521-1): the latest one, when nothing after
    /// it started a review (its program reviews or its agents), a landing
    /// or a resume, decided the landing, or opened another
    /// `approve_landing` ask ([`Self::moved_on_after`]). `None` when the
    /// run is not queued so.
    pub fn queued_review_retry(&self) -> Option<EventId> {
        let queued = self
            .events
            .iter()
            .rev()
            .find(|e| e.kind == EventKind::ReviewRetryQueued)?;
        (!self.moved_on_after(queued.id)
            && !self.has_after(queued.id, EventKind::ReviewProgramsStarted))
        .then_some(queued.id)
    }

    /// Whether `review_retry_queued` was recorded for the answer of ask
    /// `ask_id`.
    pub fn review_retry_queued_by_ask(&self, ask_id: AskId) -> bool {
        self.events.iter().any(|e| {
            e.kind == EventKind::ReviewRetryQueued
                && e.payload["ask_id"].as_i64() == Some(ask_id.as_i64())
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
            e.kind == EventKind::RunRecovered
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

    /// The `landing_queued` of an approved run whose resume resolved or
    /// was skipped (`via: resume`), or that resume's end itself (a
    /// `resume_finished` resolved or a `resume_skipped` approved, either
    /// leaving the run `needs_session`), which queues the run to land as
    /// much as the `landing_queued` written after it: the latest of these,
    /// when nothing after it started a landing, a review or a resume,
    /// decided the landing, or opened an `approve_landing` ask. Such a run
    /// waits for the landing slot `needs_session`, its resume done, even
    /// when its `landing_queued` was never written (the queue service did
    /// the resume's end without its answer reaching the supervisor).
    pub fn queued_after_resume(&self) -> Option<EventId> {
        let queued = self
            .events
            .iter()
            .rev()
            .find(|e| e.kind == EventKind::LandingQueued || resolved_to_land(e))?;
        let via_resume =
            queued.kind != EventKind::LandingQueued || queued.payload["via"] == "resume";
        (via_resume && !self.moved_on_after(queued.id)).then_some(queued.id)
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
                e.kind.known(),
                Some(EventKind::IntegrationRebased | EventKind::MigrationRenumbered)
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
    /// ([`crate::domain::concern::lets_land`]), or sent the run back past
    /// its revise limit with no code finding (`revise_limit_judged` with
    /// `converged: true` after it, ADR-t2599-1); a review from before the
    /// run's latest park at a turn's end does not.
    pub fn review_lets_land(&self) -> bool {
        let events = self.since_fence_park();
        let Some(at) = events
            .iter()
            .rposition(|e| e.kind == EventKind::ReviewFinished)
        else {
            return false;
        };
        crate::domain::concern::lets_land(&events[at].payload)
            || events[at + 1..]
                .iter()
                .any(crate::domain::review_reason::judged_converged)
    }

    /// Whether `integration_approved` was recorded for the answer of ask
    /// `ask_id`.
    pub fn approved_by_ask(&self, ask_id: AskId) -> bool {
        self.events.iter().any(|e| {
            e.kind == EventKind::IntegrationApproved
                && restore::<IntegrationApproved>(&e.payload).ask_id() == Some(ask_id.as_i64())
        })
    }

    /// Whether landing the run pushes `main`: unless an approving
    /// `integrate --no-push` recorded `push: false`.
    /// `integration_approved` is recorded once; the first one counts.
    pub fn landing_pushes(&self) -> bool {
        self.events
            .iter()
            .find(|e| e.kind == EventKind::IntegrationApproved)
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
        self.count(EventKind::ReviseRequested)
            .saturating_sub(self.count(EventKind::ReviseUnsent))
    }

    /// The revise limit the latest review attempt read: what its
    /// `review_started`, or its `review_programs_started` before that,
    /// recorded ([`ReviseLimit::recorded`]); [`MAX_REVISE_ATTEMPTS`]
    /// without an attempt or with a start that recorded none.
    pub fn revise_limit(&self) -> ReviseLimit {
        self.events
            .iter()
            .rev()
            .find(|e| {
                e.kind == EventKind::ReviewStarted || e.kind == EventKind::ReviewProgramsStarted
            })
            .and_then(|e| ReviseLimit::recorded(&e.payload))
            .unwrap_or_default()
    }

    /// How many headless reviews were started.
    pub fn review_attempts(&self) -> usize {
        self.count(EventKind::ReviewStarted)
    }

    /// How many headless triages were started.
    pub fn triage_attempts(&self) -> usize {
        self.count(EventKind::TriageStarted)
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
        self.last_of(&[EventKind::ExitRequestTimedOut, EventKind::SessionExited])
            .is_some_and(|e| e.kind == EventKind::ExitRequestTimedOut)
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
            .find(|e| e.kind.is_in(&PARKING) || recheck::parks(e))?;
        let parking: Parking = restore(&event.payload);
        let reason =
            if event.kind == EventKind::TriageFinished || event.kind == EventKind::RecoveryParked {
                parking.instruction
            } else {
                parking.reason
            };
        let cause = if event.kind == EventKind::EvidenceMissing || parking.names_checks {
            ParkCause::EvidenceMissing
        } else if event.kind == EventKind::ScopeViolation || parking.names_scope_violation {
            ParkCause::ScopeViolation
        } else if event.kind == EventKind::LandingDecided {
            ParkCause::SentBack
        } else if event.kind == EventKind::TriageFinished
            || event.kind == EventKind::TriageDecided
            || event.kind == EventKind::RecoveryParked
        {
            ParkCause::Triage
        } else if event.kind == EventKind::LandingRecheckFailed {
            ParkCause::Recheck
        } else if event.kind == EventKind::SessionGoneParked {
            ParkCause::SessionGone
        } else if event.kind == EventKind::RunE2eFailed {
            ParkCause::E2e
        } else if event.kind == EventKind::ReplanParked {
            ParkCause::Replan
        } else if event.kind == EventKind::HoldParked {
            ParkCause::Hold
        } else {
            ParkCause::Landing
        };
        Some(Park { cause, reason })
    }

    /// Whether the run was last parked by the landing, its recheck or
    /// validation (not a person's `send_back`) and the last of those parking events,
    /// `resume_finished` and `resume_skipped` is a `resume_finished` with
    /// `outcome: unresolved`: a session ran after the parking, and may
    /// have resolved the run although it was not judged so. A recheck
    /// whose command failed on the host ([`recheck::SKIPPED`]) parks
    /// nothing and is not read.
    pub fn unresolved_since_park(&self) -> bool {
        let parking = |e: &RunEvent| {
            e.kind.is_in(&RESOLVABLE_PARKING)
                && !(e.kind == EventKind::LandingRecheckFailed
                    && recheck::RecordedFailure::restore(e).skipped())
        };
        let parked = self.events.iter().rev().find(|e| parking(e));
        let last = self.events.iter().rev().find(|e| {
            parking(e) || e.kind == EventKind::ResumeFinished || e.kind == EventKind::ResumeSkipped
        });
        parked.is_some_and(|e| e.kind != EventKind::LandingDecided)
            && last.is_some_and(|e| {
                e.kind == EventKind::ResumeFinished
                    && restore::<ResumeFinished>(&e.payload).outcome == Some("unresolved")
            })
    }

    /// The requests a turn of the run's sessions ran (`turn_started`'s
    /// `request`): a turn whose agent kept no thread of the session it
    /// resumed (`turn_session_identified` with `missing`) did not run its
    /// request, which the wrapper runs again as a new session's.
    pub fn requests_run(&self) -> Vec<u64> {
        self.turns_run(0)
            .filter_map(|event| event.payload["request"].as_u64())
            .collect()
    }

    /// The `turn_started` from `from` on whose turn ran its request
    /// ([`Self::requests_run`]).
    fn turns_run(&self, from: usize) -> impl Iterator<Item = &'a RunEvent> + '_ {
        let missing: Vec<u64> = self
            .events
            .iter()
            .filter(|e| {
                e.kind == EventKind::TurnSessionIdentified && !e.payload["missing"].is_null()
            })
            .filter_map(|e| e.payload["turn"].as_u64())
            .collect();
        self.events[from.min(self.events.len())..]
            .iter()
            .filter(|e| e.kind == EventKind::TurnStarted)
            .filter(move |e| {
                e.payload["turn"]
                    .as_u64()
                    .is_none_or(|turn| !missing.contains(&turn))
            })
    }

    /// The answers to the run's questions its session had been sent but
    /// had not run when a replanning or a hold parked it (`replan_parked`'s
    /// or `hold_parked`'s `carried_answers`, each `{ask_id, seq, prompt}`),
    /// oldest first: the resume after it gives them to its session, and they stay owed
    /// until a turn after that park runs its request
    /// ([`Self::requests_run`]); a park that comes before one keeps them.
    /// Empty when the run was never parked so, or owes nothing.
    pub fn owed_answers(&self) -> Vec<serde_json::Value> {
        let Some(parked) = self.events.iter().rposition(|e| e.kind.is_in(&FENCE_PARKS)) else {
            return Vec::new();
        };
        if self.turns_run(parked + 1).next().is_some() {
            return Vec::new();
        }
        self.events[parked].payload["carried_answers"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// The texts of [`Self::owed_answers`], as each was sent.
    pub fn answers_to_carry(&self) -> Vec<String> {
        self.owed_answers()
            .iter()
            .filter_map(|answer| answer["prompt"].as_str().map(str::to_owned))
            .collect()
    }

    /// Whether the last resume event is `resume_skipped`: the run was
    /// moved on without a session, and no resume opened one since.
    pub fn last_resume_skipped(&self) -> bool {
        self.last_of(&RESUMES)
            .is_some_and(|e| e.kind == EventKind::ResumeSkipped)
    }

    /// The session of the run's last resume (see [`ResumedSession`]).
    pub fn resumed_session(&self) -> ResumedSession<'a> {
        let Some(event) = self.last_of(&RESUMES) else {
            return ResumedSession::NotResumed;
        };
        let finished: ResumeFinished = restore(&event.payload);
        if event.kind == EventKind::ResumeFinished
            && finished.status == Some(RunStatus::Validating.as_str())
            && let (Some(workspace), Some(attempt)) = (finished.workspace_id, finished.attempt)
        {
            let closed = self.events.iter().any(|e| {
                e.id > event.id
                    && e.kind == EventKind::WorkspaceClosed
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
            .filter(|e| e.kind == EventKind::AskDeliveryFailed)
            .filter_map(|e| restore::<AskOf>(&e.payload).ask_id)
            .map(AskId::new)
            .collect()
    }

    /// The `index` of every follow-up `integrate` registered already.
    pub fn registered_follow_ups(&self) -> Vec<u64> {
        self.events
            .iter()
            .filter(|e| e.kind == EventKind::FollowUpRegistered)
            .filter_map(|e| restore::<FollowUpRegistered>(&e.payload).index)
            .collect()
    }

    /// The document of every follow-up a review of the run registered
    /// already (ADR-t2352-1).
    pub fn registered_review_follow_ups(&self) -> Vec<crate::domain::ReviewFollowUpKey> {
        self.events
            .iter()
            .filter(|e| e.kind == EventKind::FollowUpRegistered)
            .filter_map(|e| {
                let registered = restore::<FollowUpRegistered>(&e.payload);
                (registered.from == Some("review")).then(|| {
                    crate::domain::ReviewFollowUpKey::new(
                        registered.path.unwrap_or_default(),
                        registered.section.unwrap_or_default(),
                    )
                })
            })
            .collect()
    }

    /// Whether the supervisor gave the run up with its live session left
    /// open, the `/exit` not sent (task 237), and no session has exited,
    /// been closed or made way for a resume since.
    pub fn session_left_open(&self) -> bool {
        self.events
            .iter()
            .rev()
            .take_while(|e| {
                !e.kind.is_in(&[
                    EventKind::SessionExited,
                    EventKind::WorkspaceClosed,
                    EventKind::ResumeStarted,
                ])
            })
            .any(|e| abandon_left_session_open(&e.kind, &e.payload))
    }

    /// The `error` of the latest `push_failed`.
    pub fn push_failure(&self) -> Option<&'a str> {
        self.last(EventKind::PushFailed)
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
    // A `needs_session` run given up on with its session left open waits
    // for nobody but a person: no resume starts while that session lives
    // (task 751).
    let resume_blocked = next == AttentionNext::Resuming && !leased && history.session_left_open();
    let event = history.events.iter().rev().find(|e| match next {
        // The error the owner gave up with, whatever its payload.
        AttentionNext::RecoverRun => e.kind == EventKind::RuntimeError,
        AttentionNext::Resuming if resume_blocked => abandon_left_session_open(&e.kind, &e.payload),
        // Whatever parked the run for a session last.
        AttentionNext::Resuming => {
            restore::<StatusOf>(&e.payload).status == Some(RunStatus::NeedsSession.as_str())
        }
        // A failed review whose `approve_landing` ask was closed
        // without moving the run (task 328) is reviewed by hand.
        AttentionNext::ReviewAndIntegrate if e.kind == EventKind::ReviewFailed => true,
        // An ask about the run is its own attention, not the run's.
        _ => !e.kind.is_in(ASK_EVENT_KINDS) && event_attention(&e.kind, &e.payload).is_some(),
    });
    let kind = event.map(|e| e.kind.as_str());
    let next = match next {
        // After a failed headless review the run is a person's to review.
        AttentionNext::ReviewAndIntegrate
            if event.is_some_and(|e| e.kind == EventKind::ReviewFailed) =>
        {
            AttentionNext::ReviewByHand
        }
        // Given up on with its session left open (task 237): the session
        // is ended first, and its exit brings the run's own attention back.
        // A `failed` / `interrupted` run is not: its recovery job takes it
        // and the round's end stops the session.
        AttentionNext::ReviewAndIntegrate | AttentionNext::RecoverRun
            if history.session_left_open() =>
        {
            AttentionNext::ExitSession
        }
        AttentionNext::Resuming if resume_blocked => AttentionNext::ExitSession,
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

/// What the supervisor does about a review that sends the run back
/// (ADR-0027 decision 2, counted across the run by ADR-t2599-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviseDecision {
    /// Send revise `number` of the run's `limit` to the live session;
    /// `attempt` numbers its request (`revise_requested` and
    /// `revise-<attempt>.txt`), which counts the requests withdrawn too, so
    /// it never reuses an earlier request's number.
    Request {
        attempt: usize,
        number: usize,
        limit: ReviseLimit,
    },
    /// The run's revise limit (the latest attempt's,
    /// [`RunHistory::revise_limit`]) was reached already: the review's
    /// reasons decide whether the run lands or a person is asked
    /// ([`crate::domain::review_reason::converged`]).
    Ask,
}

pub fn decide_revise(history: &RunHistory<'_>) -> ReviseDecision {
    let revises = history.revise_attempts();
    let limit = history.revise_limit();
    if revises >= limit.get() {
        ReviseDecision::Ask
    } else {
        ReviseDecision::Request {
            attempt: history.count(EventKind::ReviseRequested) + 1,
            number: revises + 1,
            limit,
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
    /// A run whose `already_satisfied` claim validation found sound stays
    /// `validating`: its session exits and the runtime verifies the claim
    /// on main (ADR-t1818-1).
    Satisfied,
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
        RunStatus::Validating => AfterValidation::Satisfied,
        RunStatus::NeedsSession => AfterValidation::Rest { close: true },
        _ => AfterValidation::Rest { close: false },
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::MAX_REVISE_ATTEMPTS;
    use crate::domain::event_kind;
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
        assert!(history.has_after(EventId::new(1), EventKind::ResumeStarted));
        assert!(!history.has_after(EventId::new(4), EventKind::ResumeStarted));
        assert_eq!(
            history
                .last_before(EventId::new(4), EventKind::ResumeStarted)
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
    fn a_review_retry_is_queued_until_a_review_or_another_step_starts() {
        let retry = |id: i64| {
            event(
                id,
                "review_retry_queued",
                json!({"ask_id": 7, "answer": "retry_review"}),
            )
        };
        let queued = |events: &[RunEvent]| RunHistory::from_events(events).queued_review_retry();
        assert_eq!(queued(&[event(1, "review_failed", json!({}))]), None);
        let events = [event(1, "review_failed", json!({"ask_id": 7})), retry(2)];
        assert_eq!(queued(&events), Some(EventId::new(2)));
        let history = RunHistory::from_events(&events);
        assert!(history.review_retry_queued_by_ask(AskId::new(7)));
        assert!(!history.review_retry_queued_by_ask(AskId::new(8)));
        for kind in [
            "review_programs_started",
            "review_started",
            "resume_started",
            "integration_started",
            "landing_decided",
        ] {
            assert_eq!(
                queued(&[retry(1), event(2, kind, json!({}))]),
                None,
                "{kind}"
            );
        }
        assert_eq!(
            queued(&[
                retry(1),
                event(2, "ask_opened", json!({"kind": "approve_landing"}))
            ]),
            None
        );
        // An ask about something else keeps it queued.
        assert_eq!(
            queued(&[retry(1), event(2, "ask_opened", json!({"kind": "blocked"}))]),
            Some(EventId::new(1))
        );
    }

    /// A run queued to land after its resume stays queued until a landing,
    /// a review or a resume starts; one queued another way, or queued
    /// again since by its `/exit`, is not.
    #[test]
    fn queued_after_resume_is_the_latest_resume_queue_nothing_moved_on_from() {
        let queued = |events: &[RunEvent]| RunHistory::from_events(events).queued_after_resume();
        let events = [
            event(1, "resume_finished", json!({"outcome": "resolved"})),
            event(2, "landing_queued", json!({"via": "resume"})),
            event(3, "run_phase", json!({})),
        ];
        assert_eq!(queued(&events), Some(EventId::new(2)));
        for kind in ["integration_started", "resume_started", "review_started"] {
            let events = [
                event(1, "landing_queued", json!({"via": "resume"})),
                event(2, kind, json!({})),
            ];
            assert_eq!(queued(&events), None, "{kind}");
        }
        assert_eq!(
            queued(&[event(1, "landing_queued", json!({"via": "exit"}))]),
            None
        );
        let events = [
            event(1, "landing_queued", json!({"via": "resume"})),
            event(2, "landing_queued", json!({"via": "exit"})),
        ];
        assert_eq!(queued(&events), None);
        assert_eq!(queued(&[]), None);
    }

    /// An approved run's resume that ended resolved, or was skipped, under
    /// its lease queues the run to land by itself: its `landing_queued`,
    /// written after it, may be missing (the queue service did the
    /// resume's end without its answer reaching the supervisor), and the
    /// run still waits for the landing slot rather than being resumed
    /// again. A resume that left the run otherwise does not.
    #[test]
    fn an_approved_resumes_end_queues_the_run_to_land_without_its_landing_queued() {
        let queued = |events: &[RunEvent]| RunHistory::from_events(events).queued_after_resume();
        let resolved = json!({"outcome": "resolved", "approved": true, "status": "needs_session"});
        let events = [
            event(1, "resume_started", json!({"attempt": 3})),
            event(2, "resume_finished", resolved.clone()),
        ];
        assert_eq!(queued(&events), Some(EventId::new(2)));
        let skipped = [event(
            1,
            "resume_skipped",
            json!({"approved": true, "status": "needs_session"}),
        )];
        assert_eq!(queued(&skipped), Some(EventId::new(1)));
        // Queued once, by whichever came last.
        let both = [
            event(1, "resume_finished", resolved.clone()),
            event(2, "landing_queued", json!({"via": "resume"})),
        ];
        assert_eq!(queued(&both), Some(EventId::new(2)));
        // Moved on since: landed, resumed again, or decided.
        for kind in ["integration_started", "resume_started", "landing_decided"] {
            let events = [
                event(1, "resume_finished", resolved.clone()),
                event(2, kind, json!({})),
            ];
            assert_eq!(queued(&events), None, "{kind}");
        }
        for (kind, payload) in [
            (
                "resume_finished",
                json!({"outcome": "unresolved", "status": "needs_session"}),
            ),
            (
                "resume_finished",
                json!({"outcome": "resolved", "status": "validating"}),
            ),
            (
                "resume_skipped",
                json!({"approved": false, "status": "validating"}),
            ),
        ] {
            assert_eq!(
                queued(&[event(1, kind, payload.clone())]),
                None,
                "{payload}"
            );
        }
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
        // A landing a handoff's drain handed over leaves it queued
        // (ADR-t3344-1), but not one that began after it.
        let mut events = vec![
            approved(1, json!(3)),
            event(2, "integration_started", json!({})),
            event(3, "integration_handed_over", json!({})),
        ];
        assert_eq!(queued(&events), Some(EventId::new(1)));
        events.push(event(4, "integration_started", json!({})));
        assert_eq!(queued(&events), None);
        let events = [
            approved(1, json!(3)),
            event(2, "integration_started", json!({})),
            event(3, "integration_deferred", json!({})),
            event(4, "integration_started", json!({})),
            event(5, "integration_handed_over", json!({})),
        ];
        assert_eq!(queued(&events), None);
    }

    /// ADR-t3344-1: an approved run whose resume resolved stays queued
    /// after its landing was handed over, and waits for the slot again.
    #[test]
    fn queued_after_resume_outlives_a_handed_over_landing() {
        let events = [
            event(1, "landing_queued", json!({"via": "resume"})),
            event(2, "integration_started", json!({})),
            event(3, "integration_handed_over", json!({})),
        ];
        assert_eq!(
            RunHistory::from_events(&events).queued_after_resume(),
            Some(EventId::new(1))
        );
        let events = [
            event(1, "landing_queued", json!({"via": "resume"})),
            event(2, "integration_started", json!({})),
            event(3, "integration_error", json!({})),
        ];
        assert_eq!(RunHistory::from_events(&events).queued_after_resume(), None);
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
        let request = |attempt, number| ReviseDecision::Request {
            attempt,
            number,
            limit: ReviseLimit::default(),
        };
        let sent = |n: usize| vec!["revise_requested"; n];
        for n in 0..MAX_REVISE_ATTEMPTS {
            assert_eq!(decide(&sent(n)), request(n + 1, n + 1));
        }
        assert_eq!(decide(&sent(MAX_REVISE_ATTEMPTS)), ReviseDecision::Ask);
        // A revise withdrawn before it was typed does not count, but its
        // number is not reused.
        assert_eq!(
            decide(&["revise_requested", "revise_unsent"]),
            request(2, 1)
        );
        // Events between the revises leave the count going on.
        let mut spread = sent(MAX_REVISE_ATTEMPTS);
        spread.splice(
            1..1,
            ["revise_finished", "review_started", "resume_skipped"],
        );
        assert_eq!(decide(&spread), ReviseDecision::Ask);
    }

    /// The revises are counted across the run (ADR-t2599-1): a resume, a
    /// conflict resolution and a person's `send_back` do not start the
    /// count again.
    #[test]
    fn revises_are_counted_across_the_run_through_resumes() {
        let decide = |events: &[RunEvent]| decide_revise(&RunHistory::from_events(events));
        let boundaries = [
            ("resume_started", json!({"attempt": 1})),
            ("conflict_resolved", json!({})),
            ("landing_decided", json!({"status": "needs_session"})),
        ];
        for (kind, payload) in boundaries {
            let mut events = vec![event(1, "revise_requested", json!({"attempt": 1}))];
            events.push(event(2, kind, payload.clone()));
            // The run's second revise, numbered after the first.
            assert_eq!(
                decide(&events),
                ReviseDecision::Request {
                    attempt: 2,
                    number: 2,
                    limit: ReviseLimit::default(),
                },
                "{kind}"
            );
            events.push(event(3, "revise_requested", json!({"attempt": 2})));
            events.push(event(4, kind, payload.clone()));
            assert_eq!(decide(&events), ReviseDecision::Ask, "{kind}");
            assert_eq!(
                RunHistory::from_events(&events).revise_attempts(),
                MAX_REVISE_ATTEMPTS
            );
        }
    }

    /// The limit is the one the latest attempt read and recorded
    /// (ADR-t3033-1): its `review_started`, or its
    /// `review_programs_started` before that; a start that recorded none
    /// (or one that is not at least 1) keeps [`MAX_REVISE_ATTEMPTS`].
    #[test]
    fn the_revise_limit_is_the_latest_attempts() {
        let started = |id, kind, limit: Value| event(id, kind, json!({"revise_limit": limit}));
        let limit = |events: &[RunEvent]| RunHistory::from_events(events).revise_limit().get();
        assert_eq!(limit(&[]), MAX_REVISE_ATTEMPTS);
        assert_eq!(limit(&kinds(&["review_started"])), MAX_REVISE_ATTEMPTS);
        let mut events = vec![started(1, "review_started", json!(4))];
        assert_eq!(limit(&events), 4);
        events.push(started(2, "review_programs_started", json!(3)));
        assert_eq!(limit(&events), 3);
        events.push(started(3, "review_started", json!(1)));
        assert_eq!(limit(&events), 1);
        for unreadable in [json!(0), json!(-1), json!("3"), Value::Null] {
            assert_eq!(
                limit(&[started(1, "review_started", unreadable.clone())]),
                MAX_REVISE_ATTEMPTS,
                "{unreadable}"
            );
        }
    }

    /// A raised limit sends more revises of the same count, and a lowered
    /// one below the revises the run was sent judges its next send-back
    /// past the limit (ADR-t3033-1).
    #[test]
    fn revises_stop_at_the_limit_the_latest_attempt_read() {
        let decide = |events: &[RunEvent]| decide_revise(&RunHistory::from_events(events));
        let started =
            |id, limit: usize| event(id, "review_started", json!({"revise_limit": limit}));
        let revised =
            |id, attempt: usize| event(id, "revise_requested", json!({"attempt": attempt}));
        let mut events = vec![started(1, 3)];
        for n in 1..=3 {
            assert_eq!(
                decide(&events),
                ReviseDecision::Request {
                    attempt: n,
                    number: n,
                    limit: ReviseLimit::new(3).unwrap(),
                }
            );
            events.push(revised(events.len() as i64 + 1, n));
            events.push(started(events.len() as i64 + 1, 3));
        }
        assert_eq!(decide(&events), ReviseDecision::Ask);
        // Lowered to 1 after two revises: the next attempt is judged.
        let lowered = [
            started(1, 3),
            revised(2, 1),
            started(3, 3),
            revised(4, 2),
            started(5, 1),
        ];
        assert_eq!(decide(&lowered), ReviseDecision::Ask);
        assert_eq!(
            decide(&lowered[..4]),
            ReviseDecision::Request {
                attempt: 3,
                number: 3,
                limit: ReviseLimit::new(3).unwrap(),
            }
        );
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
        assert_eq!(
            after_validation(Validating, false, false),
            AfterValidation::Satisfied
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
        assert_eq!(
            park(vec![event(
                1,
                "replan_parked",
                json!({"operation_id": 3, "reason": "parked for replan operation 3"})
            )]),
            Some((
                ParkCause::Replan,
                Some("parked for replan operation 3".into())
            ))
        );
    }

    #[test]
    fn an_approval_or_a_pass_before_a_replannings_park_does_not_let_the_run_land() {
        let history = |events: &[RunEvent]| {
            let h = RunHistory::from_events(events);
            (h.approved(), h.review_lets_land(), h.landable())
        };
        let approved = event(1, "integration_approved", json!({"ask_id": 4}));
        let passed = event(2, "review_finished", json!({"verdict": "pass"}));
        let parked = event(3, "replan_parked", json!({"code": "replan_parked"}));
        assert_eq!(
            history(&[approved.clone(), passed.clone()]),
            (true, true, true)
        );
        assert_eq!(
            history(&[approved.clone(), passed.clone(), parked.clone()]),
            (false, false, false)
        );
        // What comes after the park counts again.
        assert_eq!(
            history(&[
                approved.clone(),
                parked.clone(),
                event(4, "review_finished", json!({"verdict": "pass"}))
            ]),
            (false, true, true)
        );
        assert_eq!(
            history(&[passed, parked, event(5, "integration_approved", json!({}))]),
            (true, false, true)
        );
    }

    #[test]
    fn answers_a_replanning_parked_untaken_are_carried_until_a_turn_starts() {
        let parked = |answers: Value| {
            event(
                3,
                "replan_parked",
                json!({"operation_id": 1, "carried_answers": answers}),
            )
        };
        let carried = json!([
            {"ask_id": 7, "seq": 4, "prompt": "answer to ask 7: yes"},
            {"ask_id": 8, "seq": 5, "prompt": "answer to ask 8: no"},
        ]);
        let carry = |events: &[RunEvent]| RunHistory::from_events(events).answers_to_carry();
        assert!(carry(&[event(1, "turn_started", json!({"turn": 1}))]).is_empty());
        assert_eq!(
            carry(&[parked(carried.clone())]),
            ["answer to ask 7: yes", "answer to ask 8: no"]
        );
        // A resume that started no turn still owes them.
        assert_eq!(
            carry(&[
                parked(carried.clone()),
                event(4, "resume_started", json!({"attempt": 1})),
                event(5, "resume_finished", json!({"outcome": "error"})),
            ])
            .len(),
            2
        );
        assert!(
            carry(&[
                parked(carried.clone()),
                event(4, "turn_started", json!({"turn": 2}))
            ])
            .is_empty()
        );
        // A turn whose resumed thread was gone ran nothing: they are still
        // owed, until the turn run again as a new session's starts.
        let missing = event(
            5,
            "turn_session_identified",
            json!({"turn": 2, "session_id": null, "missing": "t1"}),
        );
        assert_eq!(
            carry(&[
                parked(carried.clone()),
                event(4, "turn_started", json!({"turn": 2, "request": 9})),
                missing.clone(),
            ])
            .len(),
            2
        );
        assert!(
            carry(&[
                parked(carried.clone()),
                event(4, "turn_started", json!({"turn": 2, "request": 9})),
                missing.clone(),
                event(6, "turn_started", json!({"turn": 3, "request": 9})),
            ])
            .is_empty()
        );
        let history = [
            event(1, "turn_started", json!({"turn": 1, "request": 1})),
            event(2, "turn_started", json!({"turn": 2, "request": 9})),
            missing,
        ];
        assert_eq!(RunHistory::from_events(&history).requests_run(), [1]);
        assert!(carry(&[parked(json!(null))]).is_empty());
        // Only the last park counts.
        assert!(
            carry(&[
                parked(carried),
                event(4, "turn_started", json!({"turn": 2})),
                event(5, "replan_parked", json!({"operation_id": 2})),
            ])
            .is_empty()
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
        // Parked again after the skip (its landing failed on the skipped
        // head): a resume comes first, and one that could not start (an
        // `error`) leaves it to be resumed again, never skipped.
        let skipped_then_parked = vec![
            deferred(1),
            finished(2, "unresolved"),
            event(3, "resume_skipped", json!({})),
            deferred(4),
        ];
        assert!(!unresolved(skipped_then_parked.clone()));
        let mut errored = skipped_then_parked.clone();
        errored.push(finished(5, "error"));
        assert!(!unresolved(errored));
        let mut resumed = skipped_then_parked;
        resumed.push(finished(5, "unresolved"));
        assert!(unresolved(resumed));
        // A park for a replanning after the unresolved resume: the receipt
        // written after its fence moves nothing on; a resume after it that
        // ended unresolved is read as after any system park.
        let replanned = |id| event(id, "replan_parked", json!({"code": "replan_parked"}));
        assert!(!unresolved(vec![
            deferred(1),
            finished(2, "unresolved"),
            replanned(3)
        ]));
        assert!(unresolved(vec![
            deferred(1),
            finished(2, "unresolved"),
            replanned(3),
            finished(4, "unresolved")
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
        // A recheck the host kept from judging the code parked nothing.
        assert!(!unresolved(vec![
            event(1, "landing_recheck_failed", json!({"action": "skipped"})),
            event(2, "session_gone_parked", json!({})),
            finished(3, "unresolved")
        ]));
        assert!(unresolved(vec![
            deferred(1),
            event(2, "landing_recheck_failed", json!({"action": "skipped"})),
            finished(3, "unresolved")
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
        // A `needs_session` run's resume waits for that session to end, so
        // it is a person's too (task 751), while a `failed` one's recovery
        // job takes it and stops the session itself.
        let parked = event(1, "validation_failed", json!({"status": "needs_session"}));
        let left_open_parked = [
            parked.clone(),
            event(2, "runtime_error", left_open.payload.clone()),
        ];
        assert_eq!(
            attention(&left_open_parked, RunStatus::NeedsSession, false),
            Some((AttentionNext::ExitSession, Some("runtime_error".into())))
        );
        assert_eq!(
            attention(
                &[
                    left_open_parked[0].clone(),
                    left_open_parked[1].clone(),
                    event(3, "session_exited", json!({}))
                ],
                RunStatus::NeedsSession,
                false
            ),
            Some((AttentionNext::Resuming, Some("validation_failed".into())))
        );
        for ended in ["workspace_closed", "resume_started"] {
            assert_eq!(
                attention(
                    &[
                        left_open_parked[0].clone(),
                        left_open_parked[1].clone(),
                        event(3, ended, json!({}))
                    ],
                    RunStatus::NeedsSession,
                    false
                )
                .map(|(next, _)| next),
                Some(AttentionNext::Resuming),
                "{ended}"
            );
        }
        assert_eq!(
            attention(&left_open_parked, RunStatus::NeedsSession, true),
            Some((AttentionNext::Resuming, Some("validation_failed".into())))
        );
        for status in [RunStatus::Failed, RunStatus::Interrupted] {
            assert_eq!(
                attention(std::slice::from_ref(&left_open), status, false).map(|(next, _)| next),
                Some(AttentionNext::Triaging)
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
    /// The documents a review's follow_ups registered for the run
    /// (ADR-t2352-1), apart from a receipt's indexes.
    #[test]
    fn registered_review_follow_ups_are_read_by_their_document() {
        let events = [
            event(1, "follow_up_registered", json!({"index": 0})),
            event(
                2,
                "follow_up_registered",
                json!({"from": "review", "review_attempt": 1, "path": "docs/a.md", "section": "流れ"}),
            ),
            event(
                3,
                "follow_up_registered",
                json!({"from": "review", "review_attempt": 2, "path": "./docs/b.md"}),
            ),
            event(4, "follow_up_registered", json!({"path": "docs/c.md"})),
        ];
        assert_eq!(
            RunHistory::from_events(&events).registered_review_follow_ups(),
            vec![
                crate::domain::ReviewFollowUpKey::new("docs/a.md", "流れ"),
                crate::domain::ReviewFollowUpKey::new("docs/b.md", ""),
            ]
        );
        assert_eq!(
            RunHistory::from_events(&events).registered_follow_ups(),
            vec![0]
        );
    }

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
