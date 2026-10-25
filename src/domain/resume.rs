//! How the resumes of a `needs_session` run are counted, and when a run
//! whose resumes are used up is retried with its branch carried over
//! (ADR-0047 decision 24, amended by ADR-t946-1, ADR-t2946-1,
//! ADR-t2664-1 and ADR-t3255-1). Both are read from the run's events
//! alone.

use super::{
    CommitSha, MAX_RESUME_ATTEMPTS, ReasonCode, RunEvent,
    reason::{event_code, explains_last_error},
    recheck,
    task_hold::Released,
};
use crate::domain::EventKind;
use crate::domain::provider_switch::SwitchReason;
use crate::domain::run::{ConflictPrecheck, restore_payload};
use crate::domain::turn::{TurnFailure, TurnOutcome};
use serde_json::{Value, json};

/// How many conflict-only attempts of one run the supervisor makes: the
/// resumes it starts while the run was parked only by a rebase conflict
/// after its review passed, and the conflict precheck's requests to the
/// live session of a passed run (ADR-0027 decision 4, task 511). Neither is
/// one of [`MAX_RESUME_ATTEMPTS`], but a conflict that never resolves stops
/// here (ADR-0047 decision 24's fence). The default of
/// [`ResumeConfig::conflict_only_limit`], which `[resume]` of `dagq.toml`
/// sets.
pub const CONFLICT_ONLY_RESUME_LIMIT: usize = 5;

/// How many kill-only resumes of one run the supervisor makes: the resumes
/// a recovery job's (or a person's) `resume` started after a signal from
/// outside ended the session (`session_killed`), which the run did not
/// cause. They are not [`MAX_RESUME_ATTEMPTS`], but a session that is
/// killed again and again stops here (ADR-t946-1).
pub const KILL_ONLY_RESUME_LIMIT: usize = 3;

/// How many provider-only resumes of one run the supervisor makes: the
/// resumes whose session's turns ended only at their provider's wall (a
/// usage limit, a login, an agent that did not start) and that ended with
/// no new commit and no rewritten receipt ([`provider_only_resume`]). They
/// are neither [`MAX_RESUME_ATTEMPTS`] nor conflict-only attempts, and the
/// next resume after one waits for the provider's hold to end
/// ([`waits_for_provider`]), but a provider that fails again and again
/// stops the run here: 3 (ADR-t3255-1 decision 3).
pub const PROVIDER_ONLY_RESUME_LIMIT: usize = 3;

/// `[resume]` of `dagq.toml` (ADR-0047 decision 24): the limit of the
/// conflict-only attempts of one run. The counted resumes'
/// [`MAX_RESUME_ATTEMPTS`], the kill-only ones' [`KILL_ONLY_RESUME_LIMIT`]
/// and the provider-only ones' [`PROVIDER_ONLY_RESUME_LIMIT`] are not
/// configurable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResumeConfig {
    /// The conflict-only attempts one run makes at most.
    pub conflict_only_limit: usize,
}

impl ResumeConfig {
    /// The keys `[resume]` may set.
    pub const KEYS: [&'static str; 1] = ["conflict_only_limit"];
}

impl Default for ResumeConfig {
    fn default() -> Self {
        Self {
            conflict_only_limit: CONFLICT_ONLY_RESUME_LIMIT,
        }
    }
}

/// `triage_finished`'s `action` for the automatic retry that carries the
/// run's branch over to the next run (ADR-0047 decisions 24 and 40).
pub const RETRY_INHERIT: &str = "retry_inherit";

/// The events that park a run for a session, or decide what it resumes
/// with: the latest of them before a resume is why it was resumed.
const PARKING: [EventKind; 11] = [
    EventKind::IntegrationDeferred,
    EventKind::IntegrationError,
    EventKind::EvidenceMissing,
    EventKind::ScopeViolation,
    EventKind::LandingDecided,
    EventKind::TriageFinished,
    EventKind::TriageDecided,
    EventKind::SessionGoneParked,
    EventKind::RunE2eFailed,
    EventKind::ReplanParked,
    EventKind::HoldParked,
];

/// What the events so far say about the run: whether its landing was
/// approved (`integration_approved`, an `integrate` call or a person's
/// `land`), whether its latest review passed, whether the latest parking
/// event is a landing deferred for a rebase conflict or a landing recheck
/// that found one, and whether it is that recheck (ADR-0068 decision 5);
/// whether the latest parking event is a `resume` decided for a session a
/// signal killed (ADR-t946-1), whether it is a person's `send_back` of the
/// landing (ADR-t2946-1), whether it is the park of a task's hold
/// (`hold_parked`, ADR-t2664-1), and the code of the run's `last_error` so
/// far.
#[derive(Debug, Clone, Copy, Default)]
struct History {
    approved: bool,
    passed: bool,
    conflict: bool,
    rechecked: bool,
    killed: bool,
    sent_back: bool,
    held: bool,
    error: Option<ReasonCode>,
}

impl History {
    fn see(&mut self, event: &RunEvent) {
        match event.kind.known() {
            Some(EventKind::IntegrationApproved) => self.approved = true,
            Some(EventKind::ReviewFinished) => {
                self.passed = super::concern::lets_land(&event.payload);
            }
            _ if super::review_reason::judged_converged(event) => self.passed = true,
            Some(kind) if PARKING.contains(&kind) || recheck::parks(event) => {
                self.conflict = matches!(
                    kind,
                    EventKind::IntegrationDeferred | EventKind::LandingRecheckFailed
                ) && event.payload["code"] == ReasonCode::RebaseConflict.as_str();
                self.rechecked = kind == EventKind::LandingRecheckFailed;
                // The recovery job's or a person's `resume` carries
                // `triage_resume`; the error it replaces is what ended the
                // run.
                self.killed = matches!(kind, EventKind::TriageFinished | EventKind::TriageDecided)
                    && event_code(event) == Some(ReasonCode::TriageResume)
                    && self.error == Some(ReasonCode::SessionKilled);
                self.sent_back = kind == EventKind::LandingDecided
                    && event_code(event) == Some(ReasonCode::SentBack);
                self.held = kind == EventKind::HoldParked;
            }
            // Only the resume right after the kill's `resume` is kill-only,
            // only that after a `send_back` passes the conflict-only limit,
            // and only that after the hold's park is the hold's: a later
            // one, with no park in between, is judged as usual.
            Some(EventKind::ResumeStarted) => {
                self.killed = false;
                self.sent_back = false;
                self.held = false;
            }
            _ => {}
        }
        if explains_last_error(event) {
            self.error = event_code(event);
        }
    }

    /// The run waits for a session only because a signal from outside
    /// ended its session and the recovery job (or a person) resumed it
    /// (ADR-t946-1): not a failure the run caused.
    const fn kill_only(self) -> bool {
        self.killed
    }

    /// The run waits for a session only because a person's hold of its
    /// task parked it at a turn's end (ADR-t2664-1): not a failure the run
    /// caused, and once per release.
    const fn hold_only(self) -> bool {
        self.held
    }

    /// The run waits for a session only because it conflicts with main,
    /// found by the landing's rebase after its review passed (or its
    /// landing was approved), or by the landing recheck while it waited
    /// (ADR-0068 decision 5: the wait, not the run, made the conflict,
    /// whatever its review said): not a failed verification or recheck
    /// command, `evidence_missing`, `scope_violation`, a person's
    /// `send_back` or a triage.
    fn conflict_only(self) -> bool {
        self.conflict && (self.approved || self.passed || self.rechecked)
    }

    /// [`Self::conflict_only`] with a review that passed or an approved
    /// landing: what the automatic retry with the branch carried over asks.
    fn reviewed_conflict(self) -> bool {
        self.conflict && (self.approved || self.passed)
    }
}

/// Whether `event` parked its run for a session (or decided what it
/// resumes with): one of the parking events, or a landing recheck's park.
pub fn parks(event: &RunEvent) -> bool {
    event.kind.is_in(&PARKING) || recheck::parks(event)
}

fn history(events: &[RunEvent]) -> History {
    let mut history = History::default();
    for event in events {
        history.see(event);
    }
    history
}

/// Whether the run, as its events stand, waits for a session only because
/// of a rebase conflict after its review passed: its next resume is not
/// counted toward [`MAX_RESUME_ATTEMPTS`].
pub fn parked_for_conflict_only(events: &[RunEvent]) -> bool {
    history(events).conflict_only()
}

/// Why a run parked only by a conflict counts as such
/// ([`parked_for_conflict_only`]): its latest review passed, its landing was
/// approved, or the landing recheck found the conflict. `None` when it is
/// not parked only by a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictOnly {
    pub passed: bool,
    pub approved: bool,
    pub rechecked: bool,
}

/// [`parked_for_conflict_only`] with what made it so.
pub fn conflict_only_basis(events: &[RunEvent]) -> Option<ConflictOnly> {
    let history = history(events);
    history.conflict_only().then_some(ConflictOnly {
        passed: history.passed,
        approved: history.approved,
        rechecked: history.rechecked,
    })
}

/// Whether the latest event that parked the run for a session is the
/// landing recheck's (ADR-0068 decision 3).
pub fn parked_by_recheck(events: &[RunEvent]) -> bool {
    history(events).rechecked
}

/// Whether the run, as its events stand, waits for a session only because
/// a signal from outside killed its session (ADR-t946-1): its next resume
/// is not counted toward [`MAX_RESUME_ATTEMPTS`] but toward
/// [`KILL_ONLY_RESUME_LIMIT`].
pub fn parked_for_kill_only(events: &[RunEvent]) -> bool {
    history(events).kill_only()
}

/// Whether the run, as its events stand, waits for a session only because
/// a hold of its task parked it (`hold_parked`, ADR-t2664-1): its next
/// resume, after the hold's release, is counted toward no limit.
pub fn parked_for_hold(events: &[RunEvent]) -> bool {
    history(events).hold_only()
}

/// The resumes of one run (`resume_started` events), split by whether
/// each counts toward [`MAX_RESUME_ATTEMPTS`], and the conflict precheck's
/// requests to its live session, which share the conflict-only limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResumeCount {
    /// Resumes that count toward [`MAX_RESUME_ATTEMPTS`].
    pub counted: usize,
    /// Resumes of a run parked only by a rebase conflict after its review
    /// passed, fenced by [`ResumeConfig::conflict_only_limit`].
    pub conflict_only: usize,
    /// Resumes of a run parked only because a signal from outside killed
    /// its session (ADR-t946-1), fenced by [`KILL_ONLY_RESUME_LIMIT`].
    pub kill_only: usize,
    /// Resumes after the release of a hold that parked the run
    /// (`hold_parked`, ADR-t2664-1): no limit fences them, since each
    /// follows a person's release.
    pub held: usize,
    /// The conflict precheck's resolution requests sent to the live
    /// session of a passed run (`conflict_precheck` with `requested: true`,
    /// but those a later one with `unsent: true` withdrew): conflicts only
    /// after a passed review, fenced with [`Self::conflict_only`] by
    /// [`ResumeConfig::conflict_only_limit`].
    pub conflict_requests: usize,
    /// Whether the run, as its events end, is parked only by a conflict
    /// ([`parked_for_conflict_only`]): its next resume would be a
    /// conflict-only one, which the precheck's requests fence too.
    pub parked_for_conflict: bool,
    /// Whether the run, as its events end, is parked by a person's
    /// `send_back` of its landing (`landing_decided` with `sent_back`):
    /// its next resume starts while counted resumes are left, even past
    /// the conflict-only limit (ADR-t2946-1).
    pub parked_for_send_back: bool,
    /// Whether the run, as its events end, is parked by a hold of its task
    /// ([`parked_for_hold`]): its next resume is a hold's, which no used-up
    /// limit stops.
    pub parked_for_hold: bool,
    /// Resumes that turned out provider-only as they finished
    /// ([`provider_only_resume`], ADR-t3255-1): taken out of
    /// [`Self::counted`] or [`Self::conflict_only`], where they started,
    /// and fenced by [`PROVIDER_ONLY_RESUME_LIMIT`].
    #[serde(default)]
    pub provider_only: usize,
    /// Whether the latest resume finished provider-only and no resume
    /// started since: the next one waits while the run's provider is held
    /// ([`waits_for_provider`]).
    #[serde(default)]
    pub after_provider_only: bool,
}

/// Where a resume was counted as it started, so that one that turns out
/// provider-only is taken back out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Started {
    Counted,
    ConflictOnly,
    Held,
    KillOnly,
}

/// What the run's events show of the resume in progress since its
/// `resume_started`, to judge it as it finishes
/// ([`provider_only_resume`]): whether one of its turns failed at its
/// provider's wall, whether one ended otherwise (it worked, or failed for
/// another reason), and the worktree head its resolution request was sent
/// at (`resume_request_sent`'s `head`).
#[derive(Debug, Clone, Default)]
struct Resuming {
    walled: bool,
    worked: bool,
    request_head: Option<Value>,
}

impl Resuming {
    fn see(&mut self, event: &RunEvent) {
        match event.kind.known() {
            Some(EventKind::TurnFinished) => {
                let outcome = event.payload["outcome"].as_str();
                let failure = event.payload["failure"]
                    .as_str()
                    .and_then(|failure| failure.parse::<TurnFailure>().ok());
                if outcome == Some(TurnOutcome::Failed.as_str())
                    && failure.is_some_and(|failure| SwitchReason::of_failure(failure).is_some())
                {
                    self.walled = true;
                } else if outcome != Some(TurnOutcome::Stopped.as_str()) {
                    // A turn the runtime stopped (the `/exit` after the
                    // wall) did no work either.
                    self.worked = true;
                }
            }
            Some(EventKind::ResumeRequestSent) => {
                self.request_head = Some(event.payload["head"].clone());
            }
            _ => {}
        }
    }

    /// ADR-t3255-1 decision 2: the resume's turns ended only at the
    /// provider's wall, and `finished` (its `resume_finished`) ended it
    /// unresolved, its `receipt_rewritten` false and its `head` the one its
    /// request was sent at (no new commit). A finish without those facts
    /// (an older event) is not provider-only.
    fn provider_only(&self, finished: &Value) -> bool {
        self.walled
            && !self.worked
            && finished["outcome"] == "unresolved"
            && finished["receipt_rewritten"] == false
            && finished["head"].is_string()
            && self.request_head.as_ref() == Some(&finished["head"])
    }
}

/// Whether the resume that `finished` (the payload of its
/// `resume_finished`, not yet recorded) ends is a provider-only one
/// (ADR-t3255-1 decision 2): with `events` the run's events so far, the
/// session's turns since the last `resume_started` ended only at their
/// provider's wall (a usage limit, a login, an agent that did not start:
/// a [`SwitchReason`]), and the resume ended unresolved with no new commit
/// and no rewritten receipt, however its session ended after the wall
/// (`/exit` after a lost heartbeat, a silent wrapper, its own end).
pub fn provider_only_resume(events: &[RunEvent], finished: &Value) -> bool {
    let Some(started) = events
        .iter()
        .rposition(|event| event.kind == EventKind::ResumeStarted)
    else {
        return false;
    };
    let mut resuming = Resuming::default();
    for event in &events[started + 1..] {
        resuming.see(event);
    }
    resuming.provider_only(finished)
}

/// Mark `finished`, the payload of the `resume_finished` about to be
/// recorded after the run's `events`, when it ends a provider-only resume
/// ([`provider_only_resume`]) that started counted or conflict-only:
/// `counted: false` and `provider_only: true`,
/// and the payload of the `auto_repaired` (`provider_resume_uncounted`)
/// that records the resume was not counted, with the run's resumes once
/// it ends. `None`, and `finished` as it was, for any other resume.
pub fn finish_provider_only(events: &[RunEvent], finished: &mut Value) -> Option<Value> {
    let resumes = ResumeCount::after_finish(events, finished);
    // Only a resume taken out of the counted or the conflict-only ones: a
    // hold's or a kill's was not counted already.
    if resumes.provider_only == ResumeCount::of(events).provider_only {
        return None;
    }
    finished["counted"] = json!(false);
    finished["provider_only"] = json!(true);
    Some(json!({
        "layer": "runtime",
        "repair": "provider_resume_uncounted",
        "conditions": {
            "turns": "provider_wall",
            "commit": false,
            "receipt_rewritten": false,
            "counted_resumes": resumes.counted,
            "conflict_only_resumes": resumes.conflict_only,
            "provider_only_resumes": resumes.provider_only,
        },
        "detail": {"attempt": finished["attempt"], "head": finished["head"]},
    }))
}

/// `provider_waiting`'s `phase` for a run whose next resume waits for its
/// provider ([`waits_for_provider`]), recorded once per wait, without a
/// `turn`.
pub const RESUME_WAIT_PHASE: &str = "resume";

/// Whether the run's wait for its provider before its next resume is
/// recorded already: a `provider_waiting` of [`RESUME_WAIT_PHASE`] comes
/// after its last resume, skipped resume and park.
pub fn resume_wait_recorded(events: &[RunEvent]) -> bool {
    events
        .iter()
        .rev()
        .find(|event| {
            event.kind.is_in(&[
                EventKind::ProviderWaiting,
                EventKind::ResumeStarted,
                EventKind::ResumeSkipped,
            ]) || parks(event)
        })
        .is_some_and(|event| {
            event.kind == EventKind::ProviderWaiting && event.payload["phase"] == RESUME_WAIT_PHASE
        })
}

/// Whether a conflict-only attempt, or a resume after a provider-only one
/// (`gated`, [`ResumeCount::waits_on_a_held_provider`]), waits for the
/// run's provider (ADR-t3255-1 decisions 1 and 3): its provider is held
/// (`held`) while the run cannot move to the other one (`[provider_fallback]
/// workers` is on (`fallback`, ADR-t1857-1), it has switches left
/// (`may_switch`) and that provider can be used (`other_usable`)). The
/// wait is no attempt.
pub const fn waits_for_provider(
    gated: bool,
    held: Option<SwitchReason>,
    fallback: bool,
    may_switch: bool,
    other_usable: bool,
) -> bool {
    gated && held.is_some() && !(fallback && may_switch && other_usable)
}

impl ResumeCount {
    pub fn of(events: &[RunEvent]) -> Self {
        Self::of_events(events)
    }

    /// [`Self::of`] the run's `events` and the `resume_finished` with
    /// `finished` as its payload, before it is recorded: what the run's
    /// resumes are once the resume in progress ends so.
    pub fn after_finish(events: &[RunEvent], finished: &Value) -> Self {
        let finish = RunEvent {
            id: super::EventId::new(0),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: EventKind::ResumeFinished.into(),
            payload: finished.clone(),
            created_at: String::new(),
            actor: None,
        };
        Self::of_events(events.iter().chain(std::iter::once(&finish)))
    }

    fn of_events<'a>(events: impl IntoIterator<Item = &'a RunEvent>) -> Self {
        let mut history = History::default();
        let mut count = Self::default();
        let mut withdrawn = 0;
        // The resume in progress: where it was counted, what parked it for
        // a person's `send_back` (given back when it turns out
        // provider-only), and what its session showed.
        let mut resuming: Option<(Started, bool, Resuming)> = None;
        for event in events {
            if let Some((_, _, facts)) = &mut resuming {
                facts.see(event);
            }
            if event.kind == EventKind::ResumeStarted {
                let started = if history.conflict_only() {
                    count.conflict_only += 1;
                    Started::ConflictOnly
                } else if history.hold_only() {
                    count.held += 1;
                    Started::Held
                } else if history.kill_only() {
                    count.kill_only += 1;
                    Started::KillOnly
                } else {
                    count.counted += 1;
                    Started::Counted
                };
                count.after_provider_only = false;
                resuming = Some((started, history.sent_back, Resuming::default()));
            } else if event.kind == EventKind::ResumeFinished {
                // Neither counted nor conflict-only (ADR-t3255-1 decision
                // 2); a hold's and a kill's stay theirs, which no counted
                // limit fences either, but the next resume waits for the
                // provider all the same.
                if let Some((started, sent_back, facts)) = resuming.take()
                    && facts.provider_only(&event.payload)
                {
                    count.after_provider_only = true;
                    if matches!(started, Started::Counted | Started::ConflictOnly) {
                        if started == Started::Counted {
                            count.counted -= 1;
                        } else {
                            count.conflict_only -= 1;
                        }
                        count.provider_only += 1;
                        history.see(event);
                        // The resume did not happen as far as the park
                        // before it goes: a person's `send_back` still lets
                        // the next one pass the conflict-only limit.
                        history.sent_back = sent_back;
                        continue;
                    }
                }
            } else if event.kind == EventKind::ConflictPrecheck {
                if restore_payload::<ConflictPrecheck>(&event.payload).requested == Some(true) {
                    count.conflict_requests += 1;
                }
                if event.payload["unsent"] == true {
                    withdrawn += 1;
                }
            }
            history.see(event);
        }
        count.conflict_requests = count.conflict_requests.saturating_sub(withdrawn);
        count.parked_for_conflict = history.conflict_only();
        count.parked_for_send_back = history.sent_back;
        count.parked_for_hold = history.hold_only();
        count
    }

    /// The attempts fenced by [`ResumeConfig::conflict_only_limit`]: the
    /// conflict-only resumes and the precheck's conflict requests.
    pub const fn conflict_attempts(self) -> usize {
        self.conflict_only + self.conflict_requests
    }

    /// Every resume started, counted or not: the number of the last one.
    pub const fn total(self) -> usize {
        self.counted + self.conflict_only + self.kill_only + self.held + self.provider_only
    }

    /// Whether the next resume waits while the run's provider is held
    /// ([`waits_for_provider`]): it would be a conflict-only one, or it
    /// follows a provider-only one (ADR-t3255-1 decisions 1 and 3).
    pub const fn waits_on_a_held_provider(self) -> bool {
        self.parked_for_conflict || self.after_provider_only
    }

    /// How many counted resumes are left.
    pub const fn left(self) -> usize {
        MAX_RESUME_ATTEMPTS.saturating_sub(self.counted)
    }

    /// No further resume starts: the counted ones reached
    /// [`MAX_RESUME_ATTEMPTS`], the kill-only ones
    /// [`KILL_ONLY_RESUME_LIMIT`], the conflict-only resumes
    /// `config`'s [`ResumeConfig::conflict_only_limit`], or, while the run is parked only by a
    /// conflict, the conflict-only attempts with the precheck's requests
    /// ([`Self::conflict_attempts`]) reached it. A run the requests brought
    /// to the limit that is parked for another reason (a failed
    /// verification, missing evidence) keeps its counted resumes, and so
    /// does one a person's `send_back` parked whose conflict-only resumes
    /// reached it (ADR-t2946-1): the fence stops a conflict that never
    /// resolves, not a person's explicit send-back. A run parked by a hold
    /// of its task ([`Self::parked_for_hold`]) is never used up: its next
    /// resume follows the hold's release (ADR-t2664-1). The provider-only
    /// resumes reaching [`PROVIDER_ONLY_RESUME_LIMIT`] use the run up too
    /// (ADR-t3255-1 decision 3).
    pub const fn exhausted(self, config: ResumeConfig) -> bool {
        let limit = config.conflict_only_limit;
        !self.parked_for_hold
            && (self.counted >= MAX_RESUME_ATTEMPTS
                || self.kill_only >= KILL_ONLY_RESUME_LIMIT
                || self.provider_only >= PROVIDER_ONLY_RESUME_LIMIT
                || (self.conflict_only >= limit && !self.parked_for_send_back)
                || (self.parked_for_conflict && self.conflict_attempts() >= limit))
    }
}

/// How the next resume of a run starts, from its events so far
/// ([`resume_start_plan`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeStart {
    /// The number of the resume: every resume so far and this one.
    pub attempt: usize,
    /// Whether it counts toward [`MAX_RESUME_ATTEMPTS`] (`resume_started`'s
    /// `counted`).
    pub counted: bool,
    /// The payload of the `auto_repaired` that records an uncounted resume:
    /// `conflict_resume_uncounted` for a run parked only by a conflict
    /// after its review passed (ADR-0047 decision 24),
    /// `kill_resume_uncounted` for one a signal from outside killed
    /// (ADR-t946-1), or `hold_resume_uncounted` for one a hold of its task
    /// parked (ADR-t2664-1). `None` for a counted resume.
    pub repair: Option<Value>,
}

/// How the next resume of a run with `events` starts on `main`: its
/// attempt, whether it is counted, and the repair that records it when it
/// is not. A conflict-only park wins over a hold's, and a hold's over a
/// kill-only one.
pub fn resume_start_plan(events: &[RunEvent], main: &CommitSha) -> ResumeStart {
    let resumes = ResumeCount::of(events);
    let attempt = resumes.total() + 1;
    let basis = conflict_only_basis(events);
    let held = basis.is_none() && resumes.parked_for_hold;
    let killed = basis.is_none() && !held && parked_for_kill_only(events);
    let repair = if let Some(basis) = basis {
        Some(json!({
            "layer": "runtime",
            "repair": "conflict_resume_uncounted",
            "conditions": {
                "review_passed": basis.passed,
                "landing_approved": basis.approved,
                "rechecked": basis.rechecked,
                "parked": ReasonCode::RebaseConflict,
                "counted_resumes": resumes.counted,
                "conflict_only_resumes": resumes.conflict_only + 1,
            },
            "detail": {"attempt": attempt, "main": main},
        }))
    } else if held {
        let hold = events
            .iter()
            .rev()
            .find(|event| event.kind == EventKind::HoldParked)
            .map_or(Value::Null, |event| event.payload["hold_id"].clone());
        Some(json!({
            "layer": "runtime",
            "repair": "hold_resume_uncounted",
            "conditions": {
                "parked": EventKind::HoldParked.as_str(),
                "hold_id": hold,
                "counted_resumes": resumes.counted,
                "hold_resumes": resumes.held + 1,
            },
            "detail": {"attempt": attempt, "main": main},
        }))
    } else if killed {
        Some(json!({
            "layer": "runtime",
            "repair": "kill_resume_uncounted",
            "conditions": {
                "parked": ReasonCode::SessionKilled,
                "counted_resumes": resumes.counted,
                "kill_only_resumes": resumes.kill_only + 1,
            },
            "detail": {"attempt": attempt, "main": main},
        }))
    } else {
        None
    };
    ResumeStart {
        attempt,
        counted: repair.is_none(),
        repair,
    }
}

/// How a run whose resumes are used up ends (ADR-0047 decision 24).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Exhaustion {
    /// The run becomes `failed` and its `resume_exhausted` alert goes to
    /// the recovery job (ADR-0047 decision 39).
    Recover,
    /// The task is ready again, and its next run carries this run's
    /// `branch` over from `head` (its review passed; only conflicts kept
    /// it from landing).
    Inherit {
        branch: Option<String>,
        head: CommitSha,
    },
}

/// Whether the event is a retry that carried a run's branch over: the
/// runtime's or the recovery job's, or a person's by hand
/// ([`is_inherit_retry_by_hand`]).
pub fn is_inherit_retry(event: &RunEvent) -> bool {
    event.kind == EventKind::TriageFinished && event.payload["action"] == RETRY_INHERIT
}

/// Whether the event is a retry that carried a run's branch over by hand
/// (`ready --inherit`, ADR-t1962-1): its `by` is `user` or `inbox`.
pub fn is_inherit_retry_by_hand(event: &RunEvent) -> bool {
    is_inherit_retry(event) && matches!(event.payload["by"].as_str(), Some("user" | "inbox"))
}

/// Whether the event used the task's one automatic retry that carries a
/// branch over: an [`is_inherit_retry`] not made by hand, which does not
/// count (ADR-t1962-1).
pub fn uses_automatic_inherit(event: &RunEvent) -> bool {
    is_inherit_retry(event) && !is_inherit_retry_by_hand(event)
}

/// Whether a run whose resumes are used up is retried with its branch
/// carried over, without a person (ADR-0047 decision 24): its review
/// passed and the last reason it waited for a session is a rebase conflict
/// only, and no run of its task (`task_events`) was retried that way
/// before. Otherwise a person decides.
pub fn inherits_on_exhaustion(run_events: &[RunEvent], task_events: &[RunEvent]) -> bool {
    history(run_events).reviewed_conflict() && !task_events.iter().any(uses_automatic_inherit)
}

/// Whether the run was ended by the automatic retry that carries its
/// branch over: its latest `triage_finished` / `triage_decided` is one.
/// The next run of its task inherits it.
pub fn retried_with_inheritance(run_events: &[RunEvent]) -> bool {
    run_events
        .iter()
        .rev()
        .find(|e| {
            matches!(
                e.kind.known(),
                Some(EventKind::TriageFinished | EventKind::TriageDecided)
            )
        })
        .is_some_and(is_inherit_retry)
}

/// What a task's next run inherits of its earlier runs ([`inherited_from`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InheritedFrom {
    /// The earlier run at this index, newest first.
    Run(usize),
    /// Nothing: a person's release of the task's hold ended a run since for
    /// a new one from the current main (`retry_fresh`, nothing saved,
    /// ADR-t1879-1), so nothing else is handed over either, not even the
    /// saved work a replacement gave the task.
    Fresh,
    /// No earlier run carried its branch over, and none was released afresh.
    Nothing,
}

/// Which of a task's earlier runs the next run inherits, given each one's
/// events newest first (`previous`): the latest ended by a retry that
/// carried its branch over ([`retried_with_inheritance`]), so that a run
/// after it that failed early does not lose the carried-over work; but
/// none past a run a release of the task's hold ended for a new run from
/// the current main, which a person chose to start afresh.
pub fn inherited_from<'a>(previous: impl IntoIterator<Item = &'a [RunEvent]>) -> InheritedFrom {
    for (index, events) in previous.into_iter().enumerate() {
        if retried_with_inheritance(events) {
            return InheritedFrom::Run(index);
        }
        if events
            .iter()
            .filter_map(Released::of_event)
            .any(|released| {
                released.decision == Some(super::task_hold::ReleaseDecision::RetryFresh)
            })
        {
            return InheritedFrom::Fresh;
        }
    }
    InheritedFrom::Nothing
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::domain::EventId;

    fn event(kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(0),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    fn conflict() -> RunEvent {
        event(
            "integration_deferred",
            json!({"code": "rebase_conflict", "conflicts": ["src/lib.rs"]}),
        )
    }

    fn resume() -> RunEvent {
        event("resume_started", json!({}))
    }

    fn pass() -> RunEvent {
        event("review_finished", json!({"verdict": "pass"}))
    }

    /// A release of a task's hold that ends its run for a new one carries
    /// the branch over by hand (ADR-t1879-1): it neither uses the task's
    /// one automatic retry nor gives back one used already.
    #[test]
    fn a_releases_carry_over_is_by_hand_and_leaves_the_automatic_retry_as_it_was() {
        let reviewed = [pass(), conflict()];
        for by in ["user", "inbox"] {
            let released = event(
                "triage_finished",
                json!({"action": RETRY_INHERIT, "by": by, "released": 3}),
            );
            assert!(is_inherit_retry(&released));
            assert!(!uses_automatic_inherit(&released), "{by}");
            assert!(inherits_on_exhaustion(
                &reviewed,
                std::slice::from_ref(&released)
            ));
            let automatic = event(
                "triage_finished",
                json!({"action": RETRY_INHERIT, "by": "runtime"}),
            );
            assert!(!inherits_on_exhaustion(
                &reviewed,
                &[automatic.clone(), released.clone()]
            ));
            assert!(!inherits_on_exhaustion(&reviewed, &[released, automatic]));
        }
    }

    /// The next run inherits the latest run that carried its branch over,
    /// past runs that failed early, but not past a run a release ended for
    /// a fresh run.
    #[test]
    fn a_fresh_release_bounds_the_search_for_a_carried_over_run() {
        let carried = vec![event(
            "triage_finished",
            json!({"action": RETRY_INHERIT, "by": "runtime"}),
        )];
        let failed_early = vec![event("triage_finished", json!({"action": "retry"}))];
        let fresh = vec![event(
            "task_released",
            json!({"hold_id": 1, "decision": "retry_fresh"}),
        )];
        let resumed = vec![event(
            "task_released",
            json!({"hold_id": 1, "decision": "resume"}),
        )];
        let newest_first =
            |runs: &[&Vec<RunEvent>]| inherited_from(runs.iter().map(|events| events.as_slice()));
        use InheritedFrom::{Fresh, Nothing, Run};
        assert_eq!(newest_first(&[&carried]), Run(0));
        assert_eq!(newest_first(&[&failed_early, &carried]), Run(1));
        assert_eq!(newest_first(&[&resumed, &carried]), Run(1));
        assert_eq!(newest_first(&[&fresh, &carried]), Fresh);
        assert_eq!(newest_first(&[&failed_early, &fresh, &carried]), Fresh);
        assert_eq!(newest_first(&[&fresh]), Fresh);
        // No source is not a fresh start: a replacement's saved work may
        // still be handed over.
        assert_eq!(newest_first(&[]), Nothing);
        assert_eq!(newest_first(&[&failed_early, &resumed]), Nothing);
        // A fresh release whose saved work is carried over is inherited.
        let fresh_saved = [fresh.clone(), carried.clone()].concat();
        assert_eq!(newest_first(&[&fresh_saved]), Run(0));
    }

    #[test]
    fn a_conflict_after_a_passed_review_is_not_counted() {
        let events = [pass(), conflict(), resume(), conflict(), resume()];
        let count = ResumeCount::of(&events);
        assert_eq!(
            count,
            ResumeCount {
                counted: 0,
                conflict_only: 2,
                kill_only: 0,
                held: 0,
                conflict_requests: 0,
                parked_for_conflict: true,
                parked_for_hold: false,
                parked_for_send_back: false,
                provider_only: 0,
                after_provider_only: false,
            }
        );
        assert_eq!(count.total(), 2);
        assert_eq!(count.left(), MAX_RESUME_ATTEMPTS);
        assert!(!count.exhausted(ResumeConfig::default()));
        assert!(parked_for_conflict_only(&events));
        assert_eq!(
            conflict_only_basis(&events),
            Some(ConflictOnly {
                passed: true,
                approved: false,
                rechecked: false,
            })
        );
    }

    #[test]
    fn an_approved_landing_counts_as_a_passed_review() {
        let events = [
            event("integration_approved", json!({})),
            conflict(),
            resume(),
        ];
        assert_eq!(ResumeCount::of(&events).conflict_only, 1);
    }

    #[test]
    fn a_conflict_without_a_passed_review_is_counted() {
        let events = [conflict(), resume()];
        assert_eq!(ResumeCount::of(&events).counted, 1);
        // A later review that did not pass takes the pass back.
        let events = [
            pass(),
            event("review_finished", json!({"verdict": "revise"})),
            conflict(),
            resume(),
        ];
        assert_eq!(ResumeCount::of(&events).counted, 1);
        assert!(!parked_for_conflict_only(&events));
        assert_eq!(conflict_only_basis(&events), None);
    }

    #[test]
    fn other_reasons_after_a_passed_review_are_counted() {
        for parking in [
            event(
                "integration_deferred",
                json!({"code": "verification_failed"}),
            ),
            event("integration_error", json!({"code": "rebase_conflict"})),
            event("evidence_missing", json!({})),
            event("scope_violation", json!({})),
            event("landing_decided", json!({"answer": "send_back"})),
            event("triage_finished", json!({"action": "resume"})),
            event("triage_decided", json!({"answer": "resume"})),
            // A continuation after a replanning's park is counted, whatever
            // parked the run before it.
            event("replan_parked", json!({"code": "replan_parked"})),
        ] {
            let events = [pass(), conflict(), parking.clone(), resume()];
            assert_eq!(ResumeCount::of(&events).counted, 1, "{}", parking.kind);
        }
    }

    #[test]
    fn a_resume_is_judged_by_what_parked_it_before_it() {
        // A conflict recorded after the resume started does not change it.
        let events = [
            pass(),
            event("evidence_missing", json!({})),
            resume(),
            conflict(),
            resume(),
        ];
        assert_eq!(
            ResumeCount::of(&events),
            ResumeCount {
                counted: 1,
                conflict_only: 1,
                kill_only: 0,
                held: 0,
                conflict_requests: 0,
                parked_for_conflict: true,
                parked_for_hold: false,
                parked_for_send_back: false,
                provider_only: 0,
                after_provider_only: false,
            }
        );
    }

    #[test]
    fn either_limit_uses_the_resumes_up() {
        let counted = ResumeCount {
            counted: MAX_RESUME_ATTEMPTS,
            conflict_only: 0,
            conflict_requests: 0,
            ..Default::default()
        };
        assert!(counted.exhausted(ResumeConfig::default()));
        assert_eq!(counted.left(), 0);
        let conflicts = ResumeCount {
            counted: 0,
            conflict_only: CONFLICT_ONLY_RESUME_LIMIT,
            conflict_requests: 0,
            ..Default::default()
        };
        assert!(conflicts.exhausted(ResumeConfig::default()));
        assert!(
            !ResumeCount {
                counted: MAX_RESUME_ATTEMPTS - 1,
                conflict_only: CONFLICT_ONLY_RESUME_LIMIT - 1,
                conflict_requests: 0,
                ..Default::default()
            }
            .exhausted(ResumeConfig::default())
        );
    }

    /// A person's `send_back` of a run whose conflict-only resumes reached
    /// their limit resumes it while counted resumes are left, and that
    /// resume is counted (ADR-t2946-1); the counted limit still ends it,
    /// and a run parked by the conflict or killed is fenced as before.
    #[test]
    fn a_persons_send_back_resumes_past_the_conflict_only_limit() {
        let sent_back = || {
            event(
                "landing_decided",
                json!({"code": "sent_back", "answer": "send_back"}),
            )
        };
        let mut events = vec![pass()];
        for _ in 0..CONFLICT_ONLY_RESUME_LIMIT {
            events.extend([conflict(), resume()]);
        }
        events.push(sent_back());
        let count = ResumeCount::of(&events);
        assert_eq!(count.conflict_only, CONFLICT_ONLY_RESUME_LIMIT);
        assert!(count.parked_for_send_back);
        assert!(!count.exhausted(ResumeConfig::default()));
        let plan = resume_start_plan(&events, &main());
        assert!(plan.counted && plan.repair.is_none());
        events.push(resume());
        let count = ResumeCount::of(&events);
        assert_eq!(count.counted, 1);
        assert_eq!(count.conflict_only, CONFLICT_ONLY_RESUME_LIMIT);
        // Once resumed, a conflict that parks it again is fenced as before
        // and retried with the branch carried over.
        let mut conflicted = events.clone();
        conflicted.push(conflict());
        assert!(ResumeCount::of(&conflicted).exhausted(ResumeConfig::default()));
        assert!(inherits_on_exhaustion(&conflicted, &conflicted));
        // Parked for another reason, the conflict-only limit ends it as
        // before.
        let mut failed = events.clone();
        failed.push(event(
            "integration_deferred",
            json!({"code": "verification_failed"}),
        ));
        assert!(ResumeCount::of(&failed).exhausted(ResumeConfig::default()));
        // With its counted resumes used up, the send_back ends the run.
        let mut used_up = events;
        for _ in 1..MAX_RESUME_ATTEMPTS {
            used_up.extend([event("evidence_missing", json!({})), resume()]);
        }
        used_up.push(sent_back());
        let count = ResumeCount::of(&used_up);
        assert_eq!(count.counted, MAX_RESUME_ATTEMPTS);
        assert!(count.exhausted(ResumeConfig::default()));
        // The kill-only limit is not the send_back's to pass.
        let kills = ResumeCount {
            kill_only: KILL_ONLY_RESUME_LIMIT,
            parked_for_send_back: true,
            ..Default::default()
        };
        assert!(kills.exhausted(ResumeConfig::default()));
    }

    #[test]
    fn the_conflict_only_limit_is_the_configured_one() {
        let two = ResumeConfig {
            conflict_only_limit: 2,
        };
        let one_conflict = ResumeCount {
            conflict_only: 1,
            ..Default::default()
        };
        assert!(!one_conflict.exhausted(two));
        let two_conflicts = ResumeCount {
            conflict_only: 2,
            ..Default::default()
        };
        assert!(two_conflicts.exhausted(two));
        assert!(!two_conflicts.exhausted(ResumeConfig::default()));
        // The precheck's requests count toward it while parked for a
        // conflict; the counted resumes' limit stays.
        let requests = ResumeCount {
            conflict_only: 1,
            kill_only: 0,
            held: 0,
            conflict_requests: 1,
            parked_for_conflict: true,
            parked_for_hold: false,
            ..Default::default()
        };
        assert!(requests.exhausted(two));
        let counted = ResumeCount {
            counted: MAX_RESUME_ATTEMPTS,
            ..Default::default()
        };
        assert!(counted.exhausted(ResumeConfig {
            conflict_only_limit: 100,
        }));
        assert_eq!(
            ResumeConfig::default().conflict_only_limit,
            CONFLICT_ONLY_RESUME_LIMIT
        );
    }

    #[test]
    fn precheck_requests_share_the_conflict_only_limit() {
        let precheck = |payload| event("conflict_precheck", payload);
        let mut events = vec![
            pass(),
            conflict(),
            resume(),
            precheck(json!({"requested": true})),
            precheck(json!({"requested": false})),
            precheck(json!({"requested": true})),
            // Withdrawn before it was typed.
            precheck(json!({"requested": false, "unsent": true})),
        ];
        let count = ResumeCount::of(&events);
        assert_eq!(
            count,
            ResumeCount {
                counted: 0,
                conflict_only: 1,
                kill_only: 0,
                held: 0,
                conflict_requests: 1,
                parked_for_conflict: true,
                parked_for_hold: false,
                parked_for_send_back: false,
                provider_only: 0,
                after_provider_only: false,
            }
        );
        assert_eq!(count.total(), 1);
        assert_eq!(count.conflict_attempts(), 2);
        assert_eq!(count.left(), MAX_RESUME_ATTEMPTS);
        events.extend(
            std::iter::repeat_with(|| precheck(json!({"requested": true})))
                .take(CONFLICT_ONLY_RESUME_LIMIT - 2),
        );
        let count = ResumeCount::of(&events);
        assert_eq!(count.conflict_attempts(), CONFLICT_ONLY_RESUME_LIMIT);
        assert_eq!(count.counted, 0);
        assert!(count.exhausted(ResumeConfig::default()));
        // A conflict found by the landing after the requests is retried
        // with the branch carried over.
        events.push(conflict());
        assert!(inherits_on_exhaustion(&events, &events));
        // Parked for another reason, the run keeps its counted resumes: the
        // requests fence conflict-only attempts only.
        let mut failed = events.clone();
        failed.push(event(
            "integration_deferred",
            json!({"code": "verification_failed"}),
        ));
        let count = ResumeCount::of(&failed);
        assert!(!count.parked_for_conflict);
        assert_eq!(count.conflict_attempts(), CONFLICT_ONLY_RESUME_LIMIT);
        assert!(!count.exhausted(ResumeConfig::default()));
        failed.push(resume());
        assert_eq!(ResumeCount::of(&failed).counted, 1);
    }

    #[test]
    fn a_used_up_conflict_run_inherits_once_per_task() {
        let run = [pass(), conflict(), resume()];
        assert!(inherits_on_exhaustion(&run, &run));
        let inherited = event("triage_finished", json!({"action": RETRY_INHERIT}));
        let task: Vec<RunEvent> = run.iter().cloned().chain([inherited.clone()]).collect();
        assert!(!inherits_on_exhaustion(&run, &task));
        // Not after a reason other than the conflict.
        let failed = [pass(), conflict(), event("evidence_missing", json!({}))];
        assert!(!inherits_on_exhaustion(&failed, &failed));
        assert!(retried_with_inheritance(std::slice::from_ref(&inherited)));
        assert!(!retried_with_inheritance(&[
            inherited,
            event("triage_decided", json!({"answer": "retry"}))
        ]));
        assert!(!retried_with_inheritance(&run));
    }

    #[test]
    fn a_retry_carried_over_by_hand_is_inherited_but_leaves_the_automatic_one() {
        let run = [pass(), conflict(), resume()];
        for by in ["user", "inbox"] {
            let by_hand = event(
                "triage_finished",
                json!({"action": RETRY_INHERIT, "by": by}),
            );
            assert!(is_inherit_retry(&by_hand));
            assert!(is_inherit_retry_by_hand(&by_hand));
            assert!(!uses_automatic_inherit(&by_hand));
            assert!(retried_with_inheritance(std::slice::from_ref(&by_hand)));
            let task: Vec<RunEvent> = run.iter().cloned().chain([by_hand]).collect();
            assert!(inherits_on_exhaustion(&run, &task));
        }
        let automatic = event(
            "triage_finished",
            json!({"action": RETRY_INHERIT, "by": "runtime"}),
        );
        assert!(uses_automatic_inherit(&automatic));
        assert!(!is_inherit_retry_by_hand(&automatic));
    }

    fn killed() -> RunEvent {
        event(
            "supervision_finished",
            json!({"status": "failed", "exit_code": 143, "code": "session_killed", "signal": 15}),
        )
    }

    fn triage_resume() -> RunEvent {
        event(
            "triage_finished",
            json!({"action": "resume", "code": "triage_resume"}),
        )
    }

    fn main() -> CommitSha {
        CommitSha::parse("a".repeat(40), "main").unwrap()
    }

    /// A resume of a run with no uncounted park is counted and recorded
    /// by `resume_started` alone.
    #[test]
    fn a_counted_resume_starts_with_no_repair() {
        let plan = resume_start_plan(&[conflict()], &main());
        assert_eq!(
            plan,
            ResumeStart {
                attempt: 1,
                counted: true,
                repair: None,
            }
        );
        // The attempt numbers every resume so far, counted or not.
        let events = [
            pass(),
            conflict(),
            resume(),
            killed(),
            triage_resume(),
            resume(),
            event(
                "integration_deferred",
                json!({"code": "verification_failed"}),
            ),
        ];
        let plan = resume_start_plan(&events, &main());
        assert_eq!((plan.attempt, plan.counted, plan.repair), (3, true, None));
    }

    /// ADR-0047 decision 24: each resume of a run parked only by a conflict
    /// after its review passed is left out of the count, recorded as the
    /// repair `conflict_resume_uncounted` with what made it so and the
    /// conflict-only resumes with this one.
    #[test]
    fn a_conflict_only_resume_is_an_uncounted_repair() {
        let mut events = vec![event("integration_approved", json!({})), conflict()];
        for attempt in 1..=CONFLICT_ONLY_RESUME_LIMIT {
            let plan = resume_start_plan(&events, &main());
            assert_eq!(plan.attempt, attempt);
            assert!(!plan.counted);
            assert_eq!(
                plan.repair,
                Some(json!({
                    "layer": "runtime",
                    "repair": "conflict_resume_uncounted",
                    "conditions": {
                        "review_passed": false,
                        "landing_approved": true,
                        "rechecked": false,
                        "parked": "rebase_conflict",
                        "counted_resumes": 0,
                        "conflict_only_resumes": attempt,
                    },
                    "detail": {"attempt": attempt, "main": main()},
                }))
            );
            events.extend([resume(), conflict()]);
        }
        assert!(ResumeCount::of(&events).exhausted(ResumeConfig::default()));
        // Found by the landing recheck, whatever the review said.
        let plan = resume_start_plan(&[recheck("rebase_conflict", recheck::RESUMED)], &main());
        assert!(!plan.counted);
        assert_eq!(plan.repair.unwrap()["conditions"]["rechecked"], true);
    }

    /// ADR-t946-1: a resume decided after a signal from outside killed the
    /// session is left out of the count, recorded as the repair
    /// `kill_resume_uncounted`, up to its own limit.
    #[test]
    fn a_kill_only_resume_is_an_uncounted_repair() {
        let mut events = vec![conflict(), resume()];
        for kills in 1..=KILL_ONLY_RESUME_LIMIT {
            events.extend([killed(), triage_resume()]);
            let plan = resume_start_plan(&events, &main());
            assert_eq!(plan.attempt, kills + 1);
            assert!(!plan.counted);
            assert_eq!(
                plan.repair,
                Some(json!({
                    "layer": "runtime",
                    "repair": "kill_resume_uncounted",
                    "conditions": {
                        "parked": "session_killed",
                        "counted_resumes": 1,
                        "kill_only_resumes": kills,
                    },
                    "detail": {"attempt": kills + 1, "main": main()},
                }))
            );
            events.push(resume());
        }
        let count = ResumeCount::of(&events);
        assert_eq!(
            (count.counted, count.kill_only),
            (1, KILL_ONLY_RESUME_LIMIT)
        );
        assert!(count.exhausted(ResumeConfig::default()));
    }

    #[test]
    fn a_resume_after_a_kill_from_outside_is_not_counted() {
        let events = [killed(), triage_resume(), resume()];
        let count = ResumeCount::of(&events);
        assert_eq!(
            count,
            ResumeCount {
                counted: 0,
                conflict_only: 0,
                kill_only: 1,
                held: 0,
                conflict_requests: 0,
                parked_for_conflict: false,
                parked_for_hold: false,
                parked_for_send_back: false,
                provider_only: 0,
                after_provider_only: false,
            }
        );
        assert_eq!(count.total(), 1);
        assert_eq!(count.left(), MAX_RESUME_ATTEMPTS);
        assert!(!count.exhausted(ResumeConfig::default()));
        assert!(parked_for_kill_only(&events[..2]));
        assert!(!parked_for_kill_only(&events[..1]));
        // A person's `resume` after the kill is not counted either.
        let decided = [
            killed(),
            event(
                "triage_decided",
                json!({"answer": "resume", "code": "triage_resume"}),
            ),
            resume(),
        ];
        assert_eq!(ResumeCount::of(&decided).kill_only, 1);
    }

    #[test]
    fn only_the_resume_right_after_a_kill_is_not_counted() {
        // A resumed session that ends unresolved is resumed again, counted.
        let events = [killed(), triage_resume(), resume(), resume(), resume()];
        let count = ResumeCount::of(&events);
        assert_eq!((count.kill_only, count.counted), (1, 2));
        assert!(!parked_for_kill_only(&events[..3]));
        // Another kill later is another kill-only resume.
        let again = [
            killed(),
            triage_resume(),
            resume(),
            killed(),
            triage_resume(),
            resume(),
        ];
        assert_eq!(ResumeCount::of(&again).kill_only, 2);
    }

    #[test]
    fn a_resume_after_another_failure_is_counted() {
        let failed = event(
            "supervision_finished",
            json!({"status": "failed", "exit_code": 1, "code": "session_exit_code"}),
        );
        let events = [failed, triage_resume(), resume()];
        assert_eq!(ResumeCount::of(&events).counted, 1);
        assert!(!parked_for_kill_only(&events[..2]));
        // A resume the recovery job decided without a kill before it, such
        // as one after a later error, is counted.
        let later = [
            killed(),
            event(
                "runtime_error",
                json!({"code": "backend_failed", "message": "x"}),
            ),
            triage_resume(),
            resume(),
        ];
        assert_eq!(ResumeCount::of(&later).counted, 1);
    }

    #[test]
    fn a_park_after_the_kills_resume_is_judged_by_its_own_reason() {
        let events = [
            killed(),
            triage_resume(),
            resume(),
            event(
                "integration_deferred",
                json!({"code": "verification_failed"}),
            ),
            resume(),
        ];
        let count = ResumeCount::of(&events);
        assert_eq!((count.counted, count.kill_only), (1, 1));
        let conflicts = [
            killed(),
            triage_resume(),
            resume(),
            pass(),
            conflict(),
            resume(),
        ];
        let count = ResumeCount::of(&conflicts);
        assert_eq!(
            (count.counted, count.kill_only, count.conflict_only),
            (0, 1, 1)
        );
    }

    #[test]
    fn kill_only_resumes_are_used_up_at_their_own_limit() {
        let mut events = Vec::new();
        for _ in 0..KILL_ONLY_RESUME_LIMIT {
            events.extend([killed(), triage_resume(), resume()]);
        }
        let count = ResumeCount::of(&events);
        assert_eq!(count.kill_only, KILL_ONLY_RESUME_LIMIT);
        assert_eq!(count.counted, 0);
        assert_eq!(count.left(), MAX_RESUME_ATTEMPTS);
        assert!(count.exhausted(ResumeConfig::default()));
        let fewer = ResumeCount::of(&events[..events.len() - 1]);
        assert!(!fewer.exhausted(ResumeConfig::default()));
    }

    fn recheck(code: &str, action: &str) -> RunEvent {
        event(
            recheck::LANDING_RECHECK_FAILED,
            json!({"code": code, "action": action}),
        )
    }

    #[test]
    fn a_conflict_the_landing_recheck_found_is_not_counted_whatever_the_review() {
        let concern = event("review_finished", json!({"verdict": "concern"}));
        let events = [
            concern.clone(),
            recheck("rebase_conflict", recheck::RESUMED),
            resume(),
        ];
        assert_eq!(
            ResumeCount::of(&events),
            ResumeCount {
                counted: 0,
                conflict_only: 1,
                kill_only: 0,
                held: 0,
                conflict_requests: 0,
                parked_for_conflict: true,
                parked_for_hold: false,
                parked_for_send_back: false,
                provider_only: 0,
                after_provider_only: false,
            }
        );
        // The retry that carries the branch over still needs a review that
        // passed.
        assert!(!inherits_on_exhaustion(&events, &events));
        let passed = [pass(), recheck("rebase_conflict", recheck::RESUMED)];
        assert!(inherits_on_exhaustion(&passed, &passed));
        assert!(parked_by_recheck(&passed));
        assert!(!parked_by_recheck(&[pass(), conflict()]));
        // A failed recheck command is counted, and a held failure parks
        // nothing.
        let failed = [
            pass(),
            recheck("verification_failed", recheck::RESUMED),
            resume(),
        ];
        assert_eq!(ResumeCount::of(&failed).counted, 1);
        let held = [concern, recheck("rebase_conflict", recheck::HELD), resume()];
        assert_eq!(ResumeCount::of(&held).counted, 1);
    }

    fn hold_parked() -> RunEvent {
        event(
            "hold_parked",
            json!({"reason": "hold_parked", "hold_id": 7}),
        )
    }

    fn released() -> RunEvent {
        event("task_released", json!({"hold_id": 7, "decision": "resume"}))
    }

    fn verification_failed() -> RunEvent {
        event(
            "integration_deferred",
            json!({"code": "verification_failed"}),
        )
    }

    /// ADR-t2664-1: the resume after a hold's release is not counted but
    /// held apart, and a run whose counted resumes are used up still
    /// resumes after the release.
    #[test]
    fn a_resume_after_a_hold_is_not_counted_and_not_used_up() {
        let used_up = [
            verification_failed(),
            resume(),
            verification_failed(),
            resume(),
            verification_failed(),
            resume(),
        ];
        assert!(ResumeCount::of(&used_up).exhausted(ResumeConfig::default()));
        let parked: Vec<RunEvent> = used_up
            .iter()
            .cloned()
            .chain([hold_parked(), released()])
            .collect();
        assert!(parked_for_hold(&parked));
        let before = ResumeCount::of(&parked);
        assert!(before.parked_for_hold);
        assert!(!before.exhausted(ResumeConfig::default()));
        let resumed: Vec<RunEvent> = parked.iter().cloned().chain([resume()]).collect();
        let after = ResumeCount::of(&resumed);
        assert_eq!((after.counted, after.held, after.total()), (3, 1, 4));
        assert!(!after.parked_for_hold);
        // Its session parked once more for its own reason: counted, and
        // used up again.
        let failed: Vec<RunEvent> = resumed
            .iter()
            .cloned()
            .chain([verification_failed()])
            .collect();
        assert!(ResumeCount::of(&failed).exhausted(ResumeConfig::default()));
        let again: Vec<RunEvent> = failed.iter().cloned().chain([resume()]).collect();
        assert_eq!(
            (
                ResumeCount::of(&again).counted,
                ResumeCount::of(&again).held
            ),
            (4, 1)
        );
    }

    /// A run parked by a hold and then for another reason (a failed
    /// verification) before it resumed is counted by that reason; and a
    /// resume that ended with no park after it does not stay the hold's.
    #[test]
    fn a_park_after_the_hold_is_judged_by_its_own_reason() {
        let events = [hold_parked(), released(), verification_failed(), resume()];
        let count = ResumeCount::of(&events);
        assert_eq!((count.counted, count.held), (1, 0));
        let errored = [hold_parked(), released(), resume(), resume()];
        let count = ResumeCount::of(&errored);
        assert_eq!((count.counted, count.held), (1, 1));
        // The kill-only and conflict-only resumes are judged as before.
        let killed = [killed(), triage_resume(), resume()];
        assert_eq!(ResumeCount::of(&killed).kill_only, 1);
        let conflicted = [pass(), conflict(), resume()];
        assert_eq!(ResumeCount::of(&conflicted).conflict_only, 1);
        assert_eq!(ResumeCount::of(&conflicted).held, 0);
    }

    /// The hold's resume is recorded as the repair
    /// `hold_resume_uncounted` with the hold and the counted resumes so far.
    #[test]
    fn a_hold_resume_is_an_uncounted_repair() {
        let events = [verification_failed(), resume(), hold_parked(), released()];
        let plan = resume_start_plan(&events, &main());
        assert_eq!(
            plan,
            ResumeStart {
                attempt: 2,
                counted: false,
                repair: Some(json!({
                    "layer": "runtime",
                    "repair": "hold_resume_uncounted",
                    "conditions": {
                        "parked": "hold_parked",
                        "hold_id": 7,
                        "counted_resumes": 1,
                        "hold_resumes": 1,
                    },
                    "detail": {"attempt": 2, "main": main()},
                })),
            }
        );
    }

    const HEAD: &str = "1111111111111111111111111111111111111111";

    /// The resume 3 of run d2cf121b (finding 474): after a landing
    /// recheck's conflict, its turn 7 failed at Claude's usage limit, the
    /// run waited for its provider, the wrapper's heartbeat expired, the
    /// session was asked to `/exit`, and the resume finished unresolved
    /// with the head its request was sent at and no rewritten receipt.
    /// `finished` is that `resume_finished`'s payload.
    fn provider_only_resume_events(finished: Value) -> Vec<RunEvent> {
        vec![
            event("resume_started", json!({"attempt": 3, "counted": false})),
            event(
                "resume_request_sent",
                json!({"resume_attempt": 3, "head": HEAD}),
            ),
            event("turn_started", json!({"turn": 7})),
            event(
                "turn_finished",
                json!({"turn": 7, "outcome": "failed", "failure": "usage_limit"}),
            ),
            event(
                "provider_waiting",
                json!({"turn": 7, "provider": "claude", "reason": "usage_limit"}),
            ),
            event(
                "wrapper_heartbeat_expired",
                json!({"heartbeat_age_secs": 935}),
            ),
            event("exit_requested", json!({"resume_attempt": 3})),
            event("resume_finished", finished),
        ]
    }

    fn unresolved(head: &str, receipt_rewritten: bool) -> Value {
        json!({
            "attempt": 3,
            "outcome": "unresolved",
            "head": head,
            "receipt_rewritten": receipt_rewritten,
        })
    }

    fn rechecked_conflict() -> Vec<RunEvent> {
        vec![pass(), recheck("rebase_conflict", recheck::RESUMED)]
    }

    /// ADR-t3255-1 decision 2: d2cf121b's resume 3 is neither a
    /// conflict-only attempt nor a counted one but a provider-only one,
    /// however its session ended after the wall; one that made a commit or
    /// rewrote its receipt is counted as before.
    #[test]
    fn a_resume_that_ended_only_at_its_providers_wall_is_not_counted() {
        let mut events = rechecked_conflict();
        events.extend(provider_only_resume_events(unresolved(HEAD, false)));
        let count = ResumeCount::of(&events);
        assert_eq!(
            (count.counted, count.conflict_only, count.provider_only),
            (0, 0, 1)
        );
        assert_eq!(count.total(), 1);
        assert!(count.after_provider_only);
        assert!(count.parked_for_conflict);
        assert!(count.waits_on_a_held_provider());
        assert!(!count.exhausted(ResumeConfig::default()));
        // The same resume after a failed verification is not counted either.
        let mut failed = vec![verification_failed()];
        failed.extend(provider_only_resume_events(unresolved(HEAD, false)));
        let count = ResumeCount::of(&failed);
        assert_eq!((count.counted, count.provider_only), (0, 1));
        // A session that ended by itself or went silent after the wall is
        // judged the same: only the turns and the repair decide.
        let ended: Vec<RunEvent> = rechecked_conflict()
            .into_iter()
            .chain(provider_only_resume_events(unresolved(HEAD, false)))
            .filter(|e| {
                !matches!(
                    e.kind.known(),
                    Some(EventKind::WrapperHeartbeatExpired | EventKind::ExitRequested)
                )
            })
            .collect();
        assert_eq!(ResumeCount::of(&ended).provider_only, 1);
        // A new commit, a rewritten receipt, or a turn that did work is a
        // repair attempt: counted as before.
        for finished in [
            unresolved("2222222222222222222222222222222222222222", false),
            unresolved(HEAD, true),
        ] {
            let mut repaired = rechecked_conflict();
            repaired.extend(provider_only_resume_events(finished.clone()));
            let count = ResumeCount::of(&repaired);
            assert_eq!(
                (count.conflict_only, count.provider_only),
                (1, 0),
                "{finished}"
            );
            assert!(!count.after_provider_only);
        }
        let mut worked = rechecked_conflict();
        worked.extend(provider_only_resume_events(unresolved(HEAD, false)));
        worked.insert(
            4,
            event("turn_finished", json!({"turn": 6, "outcome": "succeeded"})),
        );
        assert_eq!(ResumeCount::of(&worked).conflict_only, 1);
        // An older `resume_finished` without the facts is counted.
        let mut older = rechecked_conflict();
        older.extend(provider_only_resume_events(
            json!({"attempt": 3, "outcome": "unresolved", "head": HEAD}),
        ));
        assert_eq!(ResumeCount::of(&older).conflict_only, 1);
        // So is a turn that failed for another reason.
        let mut other = rechecked_conflict();
        other.extend(provider_only_resume_events(unresolved(HEAD, false)));
        other[5] = event(
            "turn_finished",
            json!({"turn": 7, "outcome": "failed", "failure": "model"}),
        );
        assert_eq!(ResumeCount::of(&other).conflict_only, 1);
    }

    /// A hold's resume that ended only at its provider's wall stays the
    /// hold's and is not marked, but the next resume waits for the provider
    /// all the same.
    #[test]
    fn a_holds_resume_at_the_wall_stays_the_holds_and_the_next_waits() {
        let mut events = vec![hold_parked(), released()];
        events.extend(provider_only_resume_events(unresolved(HEAD, false)));
        let mut finished = events.pop().unwrap().payload;
        assert_eq!(finish_provider_only(&events, &mut finished), None);
        assert_eq!(finished, unresolved(HEAD, false));
        let count = ResumeCount::after_finish(&events, &finished);
        assert_eq!((count.held, count.provider_only), (1, 0));
        assert!(count.waits_on_a_held_provider());
    }

    /// The provider-only resume is recorded as it finishes: its
    /// `resume_finished` says `counted: false` and the repair
    /// `provider_resume_uncounted` gives the resumes once it ends.
    #[test]
    fn a_provider_only_resume_is_marked_and_recorded_as_a_repair() {
        let mut events = rechecked_conflict();
        events.extend(provider_only_resume_events(unresolved(HEAD, false)));
        let finish = events.pop().unwrap();
        let mut finished = finish.payload.clone();
        assert!(provider_only_resume(&events, &finished));
        let repair = finish_provider_only(&events, &mut finished);
        assert_eq!(finished["counted"], false);
        assert_eq!(finished["provider_only"], true);
        assert_eq!(
            repair,
            Some(json!({
                "layer": "runtime",
                "repair": "provider_resume_uncounted",
                "conditions": {
                    "turns": "provider_wall",
                    "commit": false,
                    "receipt_rewritten": false,
                    "counted_resumes": 0,
                    "conflict_only_resumes": 0,
                    "provider_only_resumes": 1,
                },
                "detail": {"attempt": 3, "head": HEAD},
            }))
        );
        assert_eq!(
            ResumeCount::after_finish(&events, &finished).provider_only,
            1
        );
        let mut repaired = unresolved(HEAD, true);
        assert_eq!(finish_provider_only(&events, &mut repaired), None);
        assert_eq!(repaired, unresolved(HEAD, true));
    }

    /// ADR-t3255-1 decisions 3 and 4: provider-only resumes stop at their
    /// own limit, which uses the run up as decision 24 says (a conflict
    /// after a passed review retries with the branch carried over), and a
    /// resume after one that parked for another reason is counted by it.
    #[test]
    fn provider_only_resumes_are_used_up_at_their_own_limit() {
        let mut events = rechecked_conflict();
        for _ in 0..PROVIDER_ONLY_RESUME_LIMIT {
            events.extend(provider_only_resume_events(unresolved(HEAD, false)));
        }
        let count = ResumeCount::of(&events);
        assert_eq!(count.provider_only, PROVIDER_ONLY_RESUME_LIMIT);
        assert_eq!((count.counted, count.conflict_only), (0, 0));
        assert!(count.exhausted(ResumeConfig::default()));
        assert!(inherits_on_exhaustion(&events, &[]));
        let fewer = ResumeCount::of(&events[..events.len() - 8]);
        assert!(!fewer.exhausted(ResumeConfig::default()));
        // After a provider-only resume, a park for another reason decides
        // the next resume: a failed verification is counted.
        let mut parked = rechecked_conflict();
        parked.extend(provider_only_resume_events(unresolved(HEAD, false)));
        parked.extend([verification_failed(), resume()]);
        let count = ResumeCount::of(&parked);
        assert_eq!(
            (count.counted, count.conflict_only, count.provider_only),
            (1, 0, 1)
        );
        assert!(!count.after_provider_only);
        // A conflict after a passed review is conflict-only again.
        let mut conflicted = rechecked_conflict();
        conflicted.extend(provider_only_resume_events(unresolved(HEAD, false)));
        conflicted.extend([conflict(), resume()]);
        let count = ResumeCount::of(&conflicted);
        assert_eq!((count.conflict_only, count.provider_only), (1, 1));
        // A person's send-back still lets the resume after the
        // provider-only one past the conflict-only limit.
        let mut sent_back = vec![pass()];
        for _ in 0..CONFLICT_ONLY_RESUME_LIMIT {
            sent_back.extend([conflict(), resume()]);
        }
        sent_back.push(event(
            "landing_decided",
            json!({"answer": "send_back", "code": "sent_back"}),
        ));
        sent_back.extend(provider_only_resume_events(unresolved(HEAD, false)));
        let count = ResumeCount::of(&sent_back);
        assert_eq!((count.counted, count.provider_only), (0, 1));
        assert!(count.parked_for_send_back);
        assert!(!count.exhausted(ResumeConfig::default()));
    }

    /// ADR-t3255-1 decision 1: a conflict-only attempt (or a resume after a
    /// provider-only one) does not start while the run's provider is held,
    /// with `[provider_fallback] workers` off; it starts once the hold
    /// ends, or when the run may move to the other provider and that one
    /// can be used. Any other resume is not held back.
    #[test]
    fn a_conflict_only_resume_waits_while_its_provider_is_held() {
        let held = Some(SwitchReason::UsageLimit);
        // Held, fallback off: it waits.
        assert!(waits_for_provider(true, held, false, true, true));
        // The hold ended: it starts.
        assert!(!waits_for_provider(true, None, false, true, true));
        // The run may move and the other provider can be used: it starts.
        assert!(!waits_for_provider(true, held, true, true, true));
        // It cannot move: no switch left, or the other one cannot be used.
        assert!(waits_for_provider(true, held, true, false, true));
        assert!(waits_for_provider(true, held, true, true, false));
        // A counted resume of another reason is not held back.
        assert!(!waits_for_provider(false, held, false, true, true));
        let mut events = rechecked_conflict();
        assert!(ResumeCount::of(&events).waits_on_a_held_provider());
        events.extend([verification_failed()]);
        assert!(!ResumeCount::of(&events).waits_on_a_held_provider());
        events.extend(provider_only_resume_events(unresolved(HEAD, false)));
        assert!(ResumeCount::of(&events).waits_on_a_held_provider());
        events.push(resume());
        assert!(!ResumeCount::of(&events).waits_on_a_held_provider());
    }

    /// The wait before a resume is recorded once, until the next resume or
    /// park.
    #[test]
    fn the_wait_before_a_resume_is_recorded_once() {
        let waiting = event(
            "provider_waiting",
            json!({"phase": RESUME_WAIT_PHASE, "provider": "claude"}),
        );
        let mut events = rechecked_conflict();
        assert!(!resume_wait_recorded(&events));
        events.push(waiting.clone());
        assert!(resume_wait_recorded(&events));
        events.push(event("provider_held", json!({})));
        assert!(resume_wait_recorded(&events));
        events.push(resume());
        assert!(!resume_wait_recorded(&events));
        // A turn's wait is not the resume's.
        events.push(event("provider_waiting", json!({"turn": 9})));
        assert!(!resume_wait_recorded(&events));
        events.extend([conflict(), waiting]);
        assert!(resume_wait_recorded(&events));
        events.push(conflict());
        assert!(!resume_wait_recorded(&events));
    }
}
