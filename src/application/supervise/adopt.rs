//! Adoption (ADR-0012): runs whose supervisor died while their session
//! lives on are taken over, each in the phase it was in.

use super::file_time::recorded_at;
use super::*;
use crate::domain::EventKind;
use crate::domain::RunEvent;
use crate::domain::concern::{ConcernDecision, EscalatedBecause};
use crate::domain::run::{
    AskOpened, AttemptOf, ConcernDecided, ConflictPrecheck, ReviewFinished, ReviseRequested,
    ReviseUnsent, TurnRequested, restore_payload as restore, review_verdict,
};
use crate::domain::turn;

impl Supervisor<'_> {
    /// Take over `running` / `validating` runs whose lease went stale under
    /// another token while their wrapper is alive (heartbeat within the
    /// lease TTL) or has already reported its exit (ADR-0012), and every
    /// `awaiting_integration` run whose lease went stale, whatever its
    /// wrapper: its review is headless and needs no session, and a session
    /// that died is handled as one that ended (task 236). A `running` /
    /// `validating` run whose wrapper is dead or silent is `recover`'s
    /// business; a run without a lease was abandoned or recovered on
    /// purpose and is never adopted. The staleness is judged here and again
    /// inside `adopt_run`, so two supervisors racing for one run take it
    /// exactly once.
    pub(super) fn adopt_stale_runs(&mut self, parallel: usize) -> Result<()> {
        for candidate in self.queue.runs_leased_by_others(&self.token)? {
            let now = self.generators.clock.now();
            let LeasedRun {
                run,
                lease,
                wrapper,
            } = candidate;
            if self.in_slot(run.id()) {
                continue;
            }
            if !self.lease_stale(&lease, now) {
                continue;
            }
            if !self.adoptable(&run, wrapper.as_ref(), now)? {
                continue;
            }
            // A run that waits for a person needs no free slot while the
            // waits are under their limit (ADR-0062 decision 11).
            let as_waiting = self.adopts_as_waiting(run.id())?;
            if !as_waiting && self.used_slots() >= parallel {
                continue;
            }
            let alive = self.wrapper_alive(wrapper.as_ref(), now);
            let observed = match &wrapper {
                Some(wrapper) => json!({
                    "pid": wrapper.pid,
                    "alive": alive,
                    "exited_at": wrapper.exited_at,
                }),
                None => Value::Null,
            };
            let pid = self.layout.pid;
            let Some(run) =
                self.queue
                    .adopt_run(run.id(), &lease.token, &self.token, pid, observed)?
            else {
                info!(run_id = %run.id(), "run {} was not adopted: its lease changed while judging it", run.id());
                continue;
            };
            info!(run_id = %run.id(), task_id = %run.task_id(), "run {} adopted from supervisor {} (pid {}, heartbeat {}s old; wrapper pid {} {}): task {} in workspace {}", run.id(), lease.token, lease.pid, now - lease.heartbeat_at, wrapper.as_ref().map_or(0, |w| w.pid), match wrapper.as_ref().map(|w| w.exited_at) {
                    Some(Some(at)) => format!("exited at {at}"),
                    Some(None) if alive == Some(true) => "alive".to_owned(),
                    Some(None) => "gone".to_owned(),
                    None => "none".to_owned(),
                }, run.task_id(), run.workspace_id().unwrap_or("?"));
            let phase = match run.status() {
                RunStatus::AwaitingIntegration => self.adopt_review(&run),
                RunStatus::NeedsSession => self.adopt_resume(&run).map(|watch| {
                    self.note_resume_adopted(&run, &watch, None);
                    Phase::Resume(watch)
                }),
                _ => self.resume(&run),
            };
            match phase {
                Ok(phase) => {
                    let mut slot = Slot::new(run, phase);
                    self.restore_waiting(&mut slot, as_waiting)?;
                    self.claim.slots.admit(slot);
                }
                Err(error) => {
                    // The lease is this process's now; give it up like any
                    // other runtime error so `recover` can judge the run.
                    let message = format!("run {} could not be resumed: {error:#}", run.id());
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "{}", message);
                    self.abandon(&run, message, &reason_of_error(&error, ReasonCode::Other));
                }
            }
        }
        Ok(())
    }
    /// Whether a run whose lease went stale is taken over by
    /// [`Self::adopt_stale_runs`] rather than recovered: an
    /// `awaiting_integration` run always; a run moved on by `resume_skipped`
    /// (it has no session of its own since: its supervisor alone owned it,
    /// whatever the wrapper of an earlier session left behind); otherwise
    /// only one whose wrapper is alive or has reported its exit; a
    /// `needs_session` run only in a resume whose session lives on
    /// ([`Self::resume_adoptable`]). Never a run outside `running` /
    /// `validating` / `awaiting_integration` / `needs_session` (`claimed` /
    /// `starting` need the claimer's token).
    pub(super) fn adoptable(
        &self,
        run: &TaskRun,
        wrapper: Option<&RunProcess>,
        now: i64,
    ) -> Result<bool> {
        // Only these read the run's events and the wrapper's process.
        let reads = matches!(
            run.status(),
            RunStatus::NeedsSession | RunStatus::Running | RunStatus::Validating
        );
        let events = if reads {
            self.queue.run_events(run.id())?
        } else {
            Vec::new()
        };
        Ok(adoptable(&AdoptionSeen {
            status: run.status(),
            wrapper: if reads {
                self.wrapper_seen(wrapper, now)
            } else {
                None
            },
            skipped_resume: RunHistory::from_events(&events).last_resume_skipped(),
            resume_in_progress: resume_in_progress(&events).is_some(),
        }))
    }
    /// Whether a `needs_session` run whose lease went stale is in a resume
    /// whose session lives on (task 356): its last resume event is
    /// `resume_started` with the workspace of its attempt recorded, and the
    /// wrapper of that session has not reported
    /// its exit and its process lives, however silent (a silent one is
    /// asked to exit by the adopter's watch). `resume_parked_runs` never
    /// joins such a session, so without the adoption nobody would watch it.
    /// A session that ended, or never registered, is left to
    /// `resume_parked_runs` and its next attempt; one whose workspace was
    /// never recorded has nothing to watch it through, and blocks the next
    /// attempt until it ends.
    pub(super) fn resume_adoptable(
        &self,
        run: &TaskRun,
        wrapper: Option<&RunProcess>,
    ) -> Result<bool> {
        let lives = wrapper.is_some_and(|w| w.exited_at.is_none() && self.wrapper_lives(w));
        Ok(lives && resume_in_progress(&self.queue.run_events(run.id())?).is_some())
    }
    /// Record that a resumed session is watched on by this process instead
    /// of being resumed again: `auto_repaired` (`repair: resume_adopted`,
    /// ADR-0047 decision 24), `handoff` telling an exec'd process (with the
    /// version it came from) from an adopter. A record that fails is only
    /// noted: the run is taken over either way.
    pub(super) fn note_resume_adopted(
        &mut self,
        run: &TaskRun,
        watch: &ResumeWatch,
        handoff: Option<Option<&str>>,
    ) {
        let mut detail = json!({
            "workspace_id": watch.workspace,
            "version": self.layout.version,
        });
        if let Some(previous_version) = handoff {
            detail["previous_version"] = json!(previous_version);
        }
        if let Err(error) = self.queue.record_runtime_event(
            run.id(),
            EventKind::AutoRepaired,
            json!({
                "layer": "runtime",
                "repair": "resume_adopted",
                "conditions": {
                    "handoff": handoff.is_some(),
                    "attempt": watch.attempt,
                    "request_sent": watch.message_sent.is_some(),
                },
                "detail": detail,
            }),
        ) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "auto_repaired of {} could not be recorded: {error:#}", run.id());
        }
    }
    /// Whether the wrapper that has not reported its exit is alive
    /// ([`wrapper_alive`]).
    fn wrapper_alive(&self, wrapper: Option<&RunProcess>, now: i64) -> Option<bool> {
        wrapper_alive(self.wrapper_seen(wrapper, now))
    }
    /// What the adopter sees of `wrapper` at unix second `now`: whether its
    /// process lives is read only while it has not reported its exit.
    fn wrapper_seen(&self, wrapper: Option<&RunProcess>, now: i64) -> Option<WrapperSeen> {
        wrapper.map(|wrapper| WrapperSeen {
            exited: wrapper.exited_at.is_some(),
            lives: wrapper.exited_at.is_none() && self.wrapper_lives(wrapper),
            heartbeat_age_secs: now - wrapper.heartbeat_at,
        })
    }
    pub(super) fn resume(&self, run: &TaskRun) -> Result<Phase> {
        Ok(match run.status() {
            RunStatus::Validating => {
                Phase::Validating(Some(self.validate(run.clone())), self.session_of(run)?)
            }
            _ => {
                let receipt_path =
                    PathBuf::from(run.receipt_path().context("missing receipt path")?);
                let events = self.queue.run_events(run.id())?;
                let history = RunHistory::from_events(&events);
                let receipt_seen =
                    self.files.is_file(&receipt_path) && history.has(event_kind::RECEIPT_OBSERVED);
                // The clock the session's waits are judged on (task 1557).
                let now = self.generators.clock.monotonic();
                let since = |event: &RunEvent| self.instant_of(event, now);
                let exit_requested = self.exit_requested_at(&events, |_| true, now);
                let first_commit_seen = history.has(event_kind::FIRST_COMMIT_OBSERVED);
                Phase::Session(SessionWatch {
                    workspace: run
                        .workspace_id()
                        .map(str::to_owned)
                        .context("adopted run has no workspace")?,
                    run_dir: PathBuf::from(run.run_dir().context("missing run directory")?),
                    receipt_path,
                    idle_marker: run.idle_marker_path()?,
                    startup: now,
                    receipt_seen,
                    receipt_seen_at: history
                        .last(event_kind::RECEIPT_OBSERVED)
                        .filter(|_| receipt_seen)
                        .map(since),
                    exit_requested,

                    first_commit_seen,

                    // Only a run whose wrapper heartbeats is adopted.
                    silent: false,
                    exit_for_silence: false,
                    answer_start: None,
                    stall: Box::new(StallWatch::adopt(&*self.queue, run)?),
                    stale: adopted_stale_nudge(&*self.queue, run, SESSION_PHASE, None)?,
                    recovery: RecoveryWatch::adopt(&*self.queue, run)?,
                    input_at: None,
                    answered_at: None,
                    asks_from: 0,
                })
            }
        })
    }
    /// The session an accepted run keeps open (ADR-0027): the workspace of
    /// the resume that handed its live session to validation
    /// (`resume_finished` with status `validating`) unless a
    /// `workspace_closed` of that resume followed, else the worker's own
    /// workspace while it is not closed.
    pub(super) fn session_of(&self, run: &TaskRun) -> Result<Option<SessionRef>> {
        let events = self.queue.run_events(run.id())?;
        match RunHistory::from_events(&events).resumed_session() {
            ResumedSession::Open { workspace, attempt } => {
                return Ok(Some(SessionRef {
                    workspace: workspace.to_owned(),
                    resume: Some(attempt),
                }));
            }
            // A run moved on by `resume_skipped` has no session open.
            ResumedSession::Closed => return Ok(None),
            ResumedSession::NotResumed => {}
        }
        Ok(run
            .workspace_id()
            .map(str::to_owned)
            .filter(|_| run.workspace_closed_at().is_none())
            .map(|workspace| SessionRef {
                workspace,
                resume: None,
            }))
    }
    /// Rebuild an adopted `awaiting_integration` run under review from its
    /// events: a `revise_requested` with nothing after it waits for the live
    /// session again (it is recorded before it is written, so it is written
    /// once more only when its attempt's request is not in `turns/`,
    /// [`Supervisor::adopted_start`]), with the recovery jobs the
    /// previous supervisor recorded during it ([`SessionWatch::adopt`]), and
    /// a `revise_unsent` asks a person; a verdict already recorded (`review_finished` with
    /// nothing after it), or an approved run not reviewed since its
    /// validation, goes on to its `/exit` without a second review and
    /// without a second `/exit` if one was already requested; anything else
    /// is reviewed from the start, the review being a function of the
    /// receipt and the commit.
    pub(super) fn adopt_review(&mut self, run: &TaskRun) -> Result<Phase> {
        let session = self.session_of(run)?;
        let events = self.queue.run_events(run.id())?;
        let history = RunHistory::from_events(&events);
        let Some(anchor) = crate::domain::review_anchor(&events) else {
            return self.start_review(run, session);
        };
        if anchor.kind == event_kind::REVIEW_STARTED
            && let Some(phase) = self.adopt_failed_review_ask(run, &session, &events, anchor)?
        {
            return Ok(phase);
        }
        // Read again once it is backfilled: a revise from an applied
        // send_back goes on as that concern (`sent_back_concern`).
        let events = if self.backfill_sent_back_concern(run, &history)? {
            self.queue.run_events(run.id())?
        } else {
            events
        };
        let history = RunHistory::from_events(&events);
        let Some(anchor) = crate::domain::review_anchor(&events) else {
            return self.start_review(run, session);
        };
        let live = session.is_some() && session_alive(self, run.id())?;
        let landing_ask_open = self
            .queue
            .has_unclosed_ask(run.id(), AskKind::ApproveLanding)?;
        match review_resumption(run, &history, anchor, live, landing_ask_open) {
            ReviewResumption::Review => self.start_review(run, session),
            ReviewResumption::Revise {
                attempt,
                sent_at,
                reasons,
                concern,
            } => {
                let Some(live) = session else {
                    return self.start_review(run, None);
                };
                let start = self
                    .adopted_start(
                        run,
                        &events,
                        anchor.id,
                        &live.workspace,
                        "revise request",
                        &format!("revise-{attempt}.txt"),
                        REVISE_REQUEST_LIMIT,
                    )
                    .then_some(sent_at);
                let mut watch = ReviseWatch::new(
                    run,
                    live,
                    attempt,
                    Fix::Revise { reasons, concern },
                    sent_at,
                    start,
                    self.generators.clock.monotonic(),
                )?;
                watch.live.adopt(&*self.queue, run, anchor.id)?;
                Ok(Phase::Revise(watch))
            }
            // A conflict request with nothing after it waits for the live
            // session again, with the passed verdict before it.
            ReviewResumption::Conflict {
                attempt,
                sent_at,
                verdict,
            } => {
                let Some(live) = session else {
                    return self.start_review(run, None);
                };
                let start = self
                    .adopted_start(
                        run,
                        &events,
                        anchor.id,
                        &live.workspace,
                        "conflict request",
                        &format!("conflict-{attempt}.txt"),
                        RESUME_REQUEST_LIMIT,
                    )
                    .then_some(sent_at);
                let mut watch = ReviseWatch::new(
                    run,
                    live,
                    attempt,
                    Fix::Conflict(verdict),
                    sent_at,
                    start,
                    self.generators.clock.monotonic(),
                )?;
                watch.live.adopt(&*self.queue, run, anchor.id)?;
                Ok(Phase::Revise(watch))
            }
            // A concern is decided again from its verdict (ADR-t451-1
            // decision 3).
            ReviewResumption::Concern(verdict) => {
                match self.adopted_concern(run, session.clone(), &history, anchor, verdict)? {
                    Ok(phase) => Ok(phase),
                    Err(then) => self.exit_after(run, session, &events, anchor, then),
                }
            }
            ReviewResumption::Precheck { verdict, job } => {
                self.adopted_precheck(run, session, verdict, job)
            }
            ReviewResumption::Exit(then) => self.exit_after(run, session, &events, anchor, then),
        }
    }
    /// The exit of an adopted run under review, then `then`.
    fn exit_after(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        events: &[RunEvent],
        anchor: &RunEvent,
        mut then: AfterExit,
    ) -> Result<Phase> {
        // The ask was opened before the supervisor died, after this anchor:
        // the run waits for it (open or answered) rather than asking again,
        // which would close it as stale, notify the inbox twice and lose an
        // answer given while no supervisor ran (task 425).
        if matches!(then, AfterExit::Ask { .. })
            && let Some(ask) = self.unclosed_landing_ask_after(events, anchor)?
        {
            info!(run_id = %run.id(), ask_id = %ask.id, "run {} waits for a person in ask {}, opened before the supervisor stopped; it is not asked again", run.id(), ask.id);
            then = AfterExit::Rest { close: true };
        }
        let mut watch = ExitWatch::new(session, then);
        // Never a second exit request after the anchor (task 959).
        watch.requested = exit_requested_after(events, anchor.id);
        Ok(Phase::Exiting(watch))
    }
    /// The failed review whose `approve_landing` ask was opened before the
    /// supervisor died (task 424): an ask the supervisor opened after the
    /// run's last `review_started`, not closed since, is that review's
    /// failure, since a review that gave a verdict records
    /// `review_finished` first. The run waits for the ask rather than being
    /// reviewed again: its `review_failed` (with the ask) is recorded if it
    /// was not, and the lease is given back once the session is gone, so
    /// an answer given or to come is applied as any other (ADR-0027).
    /// `None` when there is no such ask.
    fn adopt_failed_review_ask(
        &mut self,
        run: &TaskRun,
        session: &Option<SessionRef>,
        events: &[crate::domain::RunEvent],
        started: &crate::domain::RunEvent,
    ) -> Result<Option<Phase>> {
        let Some(ask) = self.unclosed_landing_ask_after(events, started)? else {
            return Ok(None);
        };
        let after = |kind: &str| events.iter().any(|e| e.id > started.id && e.kind == kind);
        if !after(event_kind::REVIEW_FAILED) {
            let attempt = restore::<AttemptOf>(&started.payload).attempt.unwrap_or(1);
            // The failure is in the ask's first line (`open_failed_review_ask`),
            // and a `send_back` names it to the resumed session.
            let error = ask
                .question
                .lines()
                .next()
                .and_then(|line| line.split_once("): "))
                .map_or_else(
                    || format!("review {attempt} failed and ask {} was opened for it before the supervisor stopped", ask.id),
                    |(_, error)| error.to_owned(),
                );
            self.queue.record_runtime_event(
                run.id(),
                EventKind::ReviewFailed,
                json!({
                    "code": ReasonCode::JobFailed,
                    "attempt": attempt,
                    "error": error,
                    "status": run.status().as_str(),
                    "ask_id": ask.id,
                    "adopted": true,
                }),
            )?;
        }
        info!(run_id = %run.id(), ask_id = %ask.id, "run {} waits for a person in ask {} about its failed review, opened before the supervisor stopped; it is not reviewed again", run.id(), ask.id);
        let mut watch = ExitWatch::new(session.clone(), AfterExit::Rest { close: true });
        // The failed review's exit was requested before the ask: never a
        // second one (task 959).
        watch.requested = after(event_kind::EXIT_REQUESTED);
        Ok(Some(Phase::Exiting(watch)))
    }
    /// The last `approve_landing` ask the supervisor opened after event
    /// `anchor`, if nobody closed it since: the ask of the step the anchor
    /// led to, opened before the supervisor died.
    fn unclosed_landing_ask_after(
        &self,
        events: &[crate::domain::RunEvent],
        anchor: &crate::domain::RunEvent,
    ) -> Result<Option<crate::domain::Ask>> {
        let Some(ask_id) = landing_ask_after(events, anchor.id) else {
            return Ok(None);
        };
        let ask = self.queue.read_ask(ask_id)?;
        Ok(ask.closed_at.is_none().then_some(ask))
    }
    /// Whether a lease no longer has a working process behind it: its pid
    /// is dead or its heartbeat is older than `HEARTBEAT_TIMEOUT_SECS`.
    pub(super) fn lease_stale(&self, lease: &RunLease, now: i64) -> bool {
        heartbeat_stale(self.processes.alive(lease.pid), now - lease.heartbeat_at)
    }
}

impl Supervisor<'_> {
    /// The instant, on the monotonic clock whose `now` is `now`, at which
    /// `event` was recorded on the files' wall clock: an adopted wait keeps
    /// the time already waited.
    pub(super) fn instant_of(&self, event: &RunEvent, now: Instant) -> Instant {
        let ago = self
            .files
            .now()
            .duration_since(recorded_at(event))
            .unwrap_or_default();
        now.checked_sub(ago).unwrap_or(now)
    }
    pub(super) fn exit_requested_at(
        &self,
        events: &[RunEvent],
        of: impl Fn(&RunEvent) -> bool,
        now: Instant,
    ) -> Option<Instant> {
        let requested = events
            .iter()
            .rfind(|e| e.kind == event_kind::EXIT_REQUESTED && of(e))?;
        Some(self.instant_of(requested, now))
    }
}

/// When an adopted revise or conflict request was recorded as sent, to the
/// millisecond (whole seconds for one recorded before task 1197).
fn adopted_sent_at(payload: &Value) -> SystemTime {
    super::file_time::request_sent_at_of(payload)
}

impl Supervisor<'_> {
    /// The delivery of an adopted request (`what`, event `anchor`): the
    /// request was recorded before it was written to the session's
    /// `turns/`, so the supervisor adopted from may have stopped in
    /// between. Its text is the one written to `file` in the run directory
    /// before the record. The attempt's request is looked for among the
    /// requests numbered after the last `turn_requested` before the record
    /// ([`turn::adopted_delivery`]); when it is neither waiting nor taken
    /// it is written once to the session in `workspace` and recorded as
    /// `turn_requested`. Without the text the request is only waited for,
    /// up to the resume timeout. A write that fails only warns and waits:
    /// unlike the first send, it records no `revise_unsent` or `unsent`,
    /// since the request was recorded already, and the person is asked
    /// after the resume timeout. Whether its text was read.
    ///
    /// The text is written as what it took measured anew against `limit`,
    /// the whole limit of its kind, and one past it (written before the
    /// limits) keeps its start and names `file` (ADR-t2072-1).
    #[allow(clippy::too_many_arguments)]
    fn adopted_start(
        &mut self,
        run: &TaskRun,
        events: &[crate::domain::RunEvent],
        anchor: EventId,
        workspace: &str,
        what: &str,
        file: &str,
        limit: usize,
    ) -> bool {
        let Some(run_dir) = run.run_dir().map(Path::new) else {
            return false;
        };
        let path = run_dir.join(file);
        let text = match self.files.read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                warn!(run_id = %run.id(), error = %error, "the adopted {what} of {} cannot be delivered: {} could not be read: {error}", run.id(), path.display());
                return false;
            }
        };
        let after = last_request_before(events, anchor);
        let requests = match listed_requests(&*self.files, run_dir) {
            Ok(requests) => requests,
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "the adopted {what} of {} cannot be delivered: {error:#}", run.id());
                return true;
            }
        };
        // Written whole by the supervisor that recorded it, or held to its
        // limit by an earlier adopter: either is its delivery.
        let request = restored_request(&text, limit, &path);
        match turn::adopted_delivery(&requests, what, &[&text, &request.text], after) {
            turn::AdoptedDelivery::Delivered(seq) => {
                info!(run_id = %run.id(), "the adopted {what} of {} was written as request {seq}; it is not written again", run.id());
            }
            turn::AdoptedDelivery::Write => {
                info!(run_id = %run.id(), "the adopted {what} of {} was recorded but not written; writing it once", run.id());
                if let Err(error) = request_turn(self, run, workspace, Input::from(&request), what)
                {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "the adopted {what} of {} could not be written: {error:#}", run.id());
                }
            }
        }
        true
    }
}

impl Supervisor<'_> {
    /// [`Supervisor::precheck`] of a passed run an adopter takes over, at
    /// the request of the review job that passed it (`job`) as the
    /// supervisor it replaced ran it; without a job, as the supervisor's
    /// own step.
    /// Go on from a `concern` recorded with nothing after it but its
    /// `concern_decided` and an `/exit` (ADR-t451-1 decision 3): decided
    /// as the supervisor it replaced did, from the verdict. A `land` goes
    /// on to land, prechecked again before its `/exit`; a `send_back`,
    /// whose request was not recorded, asks a person as a `revise` does
    /// when the supervisor was replaced, and so does a concern the job
    /// did not decide. `concern_decided` is recorded unless it was. `Ok`
    /// is the phase of a precheck, `Err` what follows the session's `/exit`.
    /// Record the `concern_decided` of a concern sent back on the job's
    /// recommendation when the supervisor died after the request
    /// (`revise_requested`) and before the record (ADR-t451-1 decision 3):
    /// the latest review is a concern with no `concern_decided` after it,
    /// and a revise request followed it, which only an applied `send_back`
    /// records. A `revise_unsent` after the request records it as `unsent`.
    /// Whether it recorded one.
    fn backfill_sent_back_concern(
        &mut self,
        run: &TaskRun,
        history: &RunHistory<'_>,
    ) -> Result<bool> {
        let Some(review) = history.last(event_kind::REVIEW_FINISHED) else {
            return Ok(false);
        };
        // A send-back one of the review's subagents decided while the
        // verdict's own concern went lighter was no decision of that
        // concern (ADR-t1453-1 decision 7): nothing to record.
        let finished: ReviewFinished = restore(&review.payload);
        if finished.verdict != Some(ReviewDecision::Concern.as_str())
            || (finished.agents_decided() && !finished.parent_decided())
            || history.has_after(review.id, event_kind::CONCERN_DECIDED)
            || !history.has_after(review.id, event_kind::REVISE_REQUESTED)
        {
            return Ok(false);
        }
        let (Ok(verdict), Some(attempt)) = (verdict_of(review), finished.attempt) else {
            return Ok(false);
        };
        let escalated = history
            .has_after(review.id, event_kind::REVISE_UNSENT)
            .then_some(EscalatedBecause::Unsent);
        self.for_job(&ActorContext::review_job(run.id(), attempt), |sv| {
            sv.record_concern_decided(run, attempt as usize, &verdict, escalated)
        })?;
        Ok(true)
    }
    fn adopted_concern(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        history: &RunHistory<'_>,
        anchor: &RunEvent,
        verdict: ReviewVerdict,
    ) -> Result<std::result::Result<Phase, AfterExit>> {
        let job = review_job(run, anchor);
        let revise_left = decide_revise(history) != ReviseDecision::Ask;
        let decision = verdict.concern_decision(revise_left);
        let escalated = match decision {
            ConcernDecision::Land => None,
            ConcernDecision::SendBack => Some(EscalatedBecause::Unsent),
            ConcernDecision::Ask(why) => Some(why),
        };
        if !history.has_after(anchor.id, event_kind::CONCERN_DECIDED)
            && let Some(attempt) = restore::<AttemptOf>(&anchor.payload).attempt
        {
            let attempt = attempt as usize;
            self.for_job(&ActorContext::review_job(run.id(), attempt as u64), |sv| {
                sv.record_concern_decided(run, attempt, &verdict, escalated)
            })?;
        }
        let exited = history.has_after(anchor.id, event_kind::EXIT_REQUESTED);
        let then = match escalated {
            None if !exited => return Ok(Ok(self.adopted_precheck(run, session, verdict, job)?)),
            None => AfterExit::Land,
            Some(escalated) => AfterExit::Ask {
                why: match escalated {
                    EscalatedBecause::Unsent => Some(
                        "the send_back could not go on when the supervisor was replaced".to_owned(),
                    ),
                    _ => landing::concern_escalation(&verdict, escalated, history),
                },
                decision: verdict.verdict,
                recommendation: verdict.recommendation,
                confidence: verdict.confidence,
                reason_category: verdict.reason_category,
                reasons: verdict.reasons,
                summary: verdict.summary,
                requested_by: job,
                sent_back: None,
            },
        };
        Ok(Err(then))
    }
    fn adopted_precheck(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        verdict: ReviewVerdict,
        job: Option<ActorContext>,
    ) -> Result<Phase> {
        match job.clone() {
            Some(requester) => {
                self.for_job(&requester, |sv| sv.precheck(run, session, verdict, job))
            }
            None => self.precheck(run, session, verdict, None),
        }
    }
}

/// The review job that recorded `review_finished` (`event`), from the
/// `attempt` of its payload: what an adopter records as the
/// `requested_by` of the `approve_landing` ask it opens for the job's
/// verdict, as the supervisor it replaced would have (task 798). `None`
/// when the payload has no attempt, so no job is named rather than a
/// wrong one.
fn review_job(run: &TaskRun, event: &RunEvent) -> Option<ActorContext> {
    restore::<AttemptOf>(&event.payload)
        .attempt
        .map(|attempt| ActorContext::review_job(run.id(), attempt))
}

/// [`review_job`] of the last `review_finished` before `before`.
fn review_job_before(
    run: &TaskRun,
    history: &RunHistory<'_>,
    before: EventId,
) -> Option<ActorContext> {
    history
        .last_before(before, event_kind::REVIEW_FINISHED)
        .and_then(|event| review_job(run, event))
}

/// The verdict of the last `review_finished` before event `before`: the
/// pass a conflict precheck followed, or the revise a `revise_unsent` did.
pub(super) fn passed_before(history: &RunHistory<'_>, before: EventId) -> Option<ReviewVerdict> {
    history
        .last_before(before, event_kind::REVIEW_FINISHED)
        .and_then(|e| verdict_of(e).ok())
}

/// Whether the route a `review_finished` recorded (ADR-t1453-1 decision
/// 7) was decided by one of its subagents: one went further than landing,
/// to where the review went.
fn agents_decided(payload: &Value) -> bool {
    restore::<ReviewFinished>(payload).agents_decided()
}

/// The `approve_landing` ask an adopter opens for `review`, a
/// `review_finished` whose route one of its subagents decided: the reasons
/// of every judgment that went there, an agent's under its name, the
/// `discard` or `scope` they give, and why (`why` after what each of them
/// returned), at the review job's request.
fn agents_ask(
    run: &TaskRun,
    review: &RunEvent,
    verdict: ReviewVerdict,
    revise_left: bool,
    why: String,
) -> AfterExit {
    let route = verdict.route(revise_left);
    let parent_decides = route.parent == route.destination;
    let combined = landing::combined_verdict(&verdict, &route, parent_decides);
    let judged = landing::agents_escalation(&verdict, &route, parent_decides);
    AfterExit::Ask {
        why: Some(format!("{why}; {judged}")),
        decision: combined.verdict,
        recommendation: None,
        confidence: None,
        reason_category: combined.reason_category,
        reasons: combined.reasons,
        summary: combined.summary,
        requested_by: review_job(run, review),
        sent_back: None,
    }
}

/// The concern whose `send_back` the revise request `anchor` applied: the
/// review before it is a concern whose `concern_decided` (recorded after
/// the request, or backfilled) says it was applied (task 1392).
fn sent_back_concern(history: &RunHistory<'_>, anchor: &RunEvent) -> Option<SentBackConcern> {
    let review = history.last_before(anchor.id, event_kind::REVIEW_FINISHED)?;
    let attempt = restore::<AttemptOf>(&review.payload).attempt?;
    let applied = history.events().iter().any(|event| {
        event.id > review.id && event.kind == event_kind::CONCERN_DECIDED && {
            let decided: ConcernDecided = restore(&event.payload);
            decided.attempt == Some(attempt) && decided.applied == Some(true)
        }
    });
    if !applied {
        return None;
    }
    SentBackConcern::of(&verdict_of(review).ok()?, attempt as usize)
}

/// The verdict a `review_finished` recorded, with a concern's
/// recommendation, confidence and reason when it gave them.
fn verdict_of(event: &RunEvent) -> serde_json::Result<ReviewVerdict> {
    review_verdict(&event.payload)
}

/// What an adopter saw of a run's wrapper process.
#[derive(Debug, Clone, Copy)]
pub(super) struct WrapperSeen {
    /// It reported its exit.
    exited: bool,
    /// Its process lives ([`Supervisor::wrapper_lives`]); false once it
    /// reported its exit.
    lives: bool,
    heartbeat_age_secs: i64,
}

/// Whether the wrapper that has not reported its exit is alive: its
/// process lives and its heartbeat is within the lease TTL
/// ([`HEARTBEAT_TIMEOUT_SECS`]). `None` when there is no wrapper or it
/// reported its exit.
fn wrapper_alive(wrapper: Option<WrapperSeen>) -> Option<bool> {
    wrapper.and_then(|wrapper| {
        (!wrapper.exited)
            .then_some(wrapper.lives && wrapper.heartbeat_age_secs <= HEARTBEAT_TIMEOUT_SECS)
    })
}

/// What decides whether a run whose lease went stale is adopted
/// ([`adoptable`]).
#[derive(Debug, Clone, Copy)]
struct AdoptionSeen {
    status: RunStatus,
    wrapper: Option<WrapperSeen>,
    /// Its last resume event is `resume_skipped`: it was moved on without
    /// a session, and no resume opened one since.
    skipped_resume: bool,
    /// Its last resume event is `resume_started` with the workspace of its
    /// attempt recorded ([`resume_in_progress`]).
    resume_in_progress: bool,
}

/// [`Supervisor::adoptable`] of what the adopter saw.
fn adoptable(seen: &AdoptionSeen) -> bool {
    match seen.status {
        RunStatus::NeedsSession => {
            seen.wrapper.is_some_and(|w| !w.exited && w.lives) && seen.resume_in_progress
        }
        RunStatus::AwaitingIntegration => true,
        RunStatus::Running | RunStatus::Validating => {
            seen.skipped_resume
                || (seen.wrapper.is_some() && wrapper_alive(seen.wrapper) != Some(false))
        }
        _ => false,
    }
}

/// How an adopted `awaiting_integration` run under review goes on, from
/// its history alone ([`review_resumption`]).
enum ReviewResumption {
    /// Reviewed from the start, the review being a function of the
    /// receipt and the commit.
    Review,
    /// The revise request `anchor` with nothing after it waits for the live
    /// session again.
    Revise {
        attempt: usize,
        sent_at: SystemTime,
        reasons: Vec<String>,
        concern: Option<SentBackConcern>,
    },
    /// The conflict request `anchor` with nothing after it waits for the
    /// live session again, with the passed verdict before it.
    Conflict {
        attempt: usize,
        sent_at: SystemTime,
        verdict: ReviewVerdict,
    },
    /// A `concern` decided again from its verdict.
    Concern(ReviewVerdict),
    /// A pass not yet followed by its exit request is prechecked again:
    /// main may have moved. At the request of the review job that passed
    /// it, when its attempt is recorded.
    Precheck {
        verdict: ReviewVerdict,
        job: Option<ActorContext>,
    },
    /// The session's exit, then this.
    Exit(AfterExit),
}

/// [`Supervisor::adopt_review`]'s choice for `anchor`, the run's last
/// review event ([`crate::domain::review_anchor`]), given whether its
/// session lives (`live`) and whether an `approve_landing` ask of the run
/// is open (`landing_ask_open`): a revise or conflict request waits for the
/// live session again, and is reviewed again without one; a `revise_unsent`
/// asks a person; a verdict a review's subagents decided asks a person; a
/// concern is decided again; a pass is prechecked again before its exit
/// request and lands after it; a precheck that sent nothing lands or asks;
/// an approved run lands; a run a landing recheck resumed waits for its
/// ask; anything else is reviewed from the start.
fn review_resumption(
    run: &TaskRun,
    history: &RunHistory<'_>,
    anchor: &RunEvent,
    live: bool,
    landing_ask_open: bool,
) -> ReviewResumption {
    let exited = history.has_after(anchor.id, event_kind::EXIT_REQUESTED);
    let then = match anchor.kind.as_str() {
        event_kind::REVISE_REQUESTED => {
            if !live {
                return ReviewResumption::Review;
            }
            let request: ReviseRequested = restore(&anchor.payload);
            return ReviewResumption::Revise {
                attempt: request.attempt.unwrap_or(1) as usize,
                sent_at: adopted_sent_at(&anchor.payload),
                reasons: request.reasons.unwrap_or_default(),
                concern: sent_back_concern(history, anchor),
            };
        }
        event_kind::CONFLICT_PRECHECK
            if restore::<ConflictPrecheck>(&anchor.payload).requested == Some(true) =>
        {
            return match passed_before(history, anchor.id) {
                Some(verdict) if live => ReviewResumption::Conflict {
                    attempt: restore::<ConflictPrecheck>(&anchor.payload)
                        .attempt
                        .unwrap_or(1) as usize,
                    sent_at: adopted_sent_at(&anchor.payload),
                    verdict,
                },
                _ => ReviewResumption::Review,
            };
        }
        // A revise request recorded but not sent asks a person, as it
        // did before the supervisor was replaced.
        event_kind::REVISE_UNSENT => {
            let unsent: ReviseUnsent = restore(&anchor.payload);
            // A revise the review's subagents decided (ADR-t1453-1
            // decision 7) is asked with their reasons, as the
            // supervisor that could not send it asked.
            let review = history.last_before(anchor.id, event_kind::REVIEW_FINISHED);
            if let Some(review) = review.filter(|r| agents_decided(&r.payload))
                && let Ok(verdict) = verdict_of(review)
            {
                let why = unsent
                    .error
                    .unwrap_or("the revise request could not be sent");
                // The request was recorded, so the round had a revise
                // left when the review was decided.
                Some(agents_ask(run, review, verdict, true, why.to_owned()))
            } else {
                passed_before(history, anchor.id).map(|verdict| AfterExit::Ask {
                    why: unsent.error.map(str::to_owned),
                    decision: verdict.verdict,
                    recommendation: verdict.recommendation,
                    confidence: verdict.confidence,
                    reason_category: verdict.reason_category,
                    reasons: verdict.reasons,
                    summary: verdict.summary,
                    requested_by: review_job_before(run, history, anchor.id),
                    sent_back: None,
                })
            }
        }
        event_kind::REVIEW_FINISHED => {
            let finished: ReviewFinished = restore(&anchor.payload);
            match verdict_of(anchor) {
                // A review whose subagents sent the run further than
                // its verdict, or carried reasons of their own
                // (ADR-t1453-1 decision 7), asks a person: an adopter
                // does not apply the verdict alone.
                Ok(verdict) if finished.agents_decided() => {
                    let revise_left = decide_revise(history) != ReviseDecision::Ask;
                    let why = format!(
                        "the review's subagents sent the run to {} and the supervisor was replaced before it was applied",
                        finished.destination().unwrap_or("a person")
                    );
                    Some(agents_ask(run, anchor, verdict, revise_left, why))
                }
                Ok(verdict) if verdict.verdict == ReviewDecision::Concern => {
                    return ReviewResumption::Concern(verdict);
                }
                // A pass not yet followed by its exit request is
                // prechecked (again): main may have moved.
                Ok(verdict) if verdict.verdict == ReviewDecision::Pass && !exited => {
                    return ReviewResumption::Precheck {
                        verdict,
                        job: review_job(run, anchor),
                    };
                }
                Ok(verdict) if verdict.verdict == ReviewDecision::Pass => Some(AfterExit::Land),
                Ok(verdict) => Some(AfterExit::Ask {
                    why: (verdict.verdict == ReviewDecision::Revise).then(|| {
                        "the revise could not go on when the supervisor was replaced".to_owned()
                    }),
                    decision: verdict.verdict,
                    recommendation: None,
                    confidence: None,
                    reason_category: None,
                    reasons: verdict.reasons,
                    summary: verdict.summary,
                    requested_by: review_job(run, anchor),
                    sent_back: None,
                }),
                Err(_) => None,
            }
        }
        // A precheck that sent nothing decided to land (no session to
        // ask) or to ask a person (past the limit); before its exit request
        // it is prechecked again, as main may have moved.
        event_kind::CONFLICT_PRECHECK => match passed_before(history, anchor.id) {
            Some(verdict) if !exited => {
                return ReviewResumption::Precheck {
                    verdict,
                    job: review_job_before(run, history, anchor.id),
                };
            }
            Some(verdict) => Some(match restore::<ConflictPrecheck>(&anchor.payload).asked {
                Some(why) => Fix::Conflict(verdict).ask(
                    String::new(),
                    why.to_owned(),
                    review_job_before(run, history, anchor.id),
                ),
                None => AfterExit::Land,
            }),
            None => None,
        },
        event_kind::VALIDATION_FINISHED
            if history
                .events()
                .iter()
                .any(|e| e.kind == event_kind::INTEGRATION_APPROVED) =>
        {
            Some(AfterExit::Land)
        }
        // A rebased run a landing recheck resumed waits for the answer
        // to its approve_landing ask, not a review (ADR-0068 decision 4).
        event_kind::VALIDATION_FINISHED
            if crate::domain::resume::parked_by_recheck(history.events()) && landing_ask_open =>
        {
            Some(AfterExit::Rest { close: true })
        }
        _ => None,
    };
    then.map_or(ReviewResumption::Review, ReviewResumption::Exit)
}

/// Whether an exit request was recorded after event `anchor`: an adopter
/// never sends a second one (task 959).
fn exit_requested_after(events: &[RunEvent], anchor: EventId) -> bool {
    events
        .iter()
        .any(|e| e.id > anchor && e.kind == event_kind::EXIT_REQUESTED)
}

/// The last `approve_landing` ask the supervisor opened after event
/// `anchor`.
fn landing_ask_after(events: &[RunEvent], anchor: EventId) -> Option<AskId> {
    events
        .iter()
        .rev()
        .filter(|e| e.id > anchor && e.kind == event_kind::ASK_OPENED)
        .map(|e| restore::<AskOpened>(&e.payload))
        .find(|opened| {
            opened.kind == Some(AskKind::ApproveLanding.as_str())
                && opened.asked_by == Some(SessionRole::Supervisor.as_str())
        })
        .and_then(|opened| opened.ask_id)
        .map(AskId::new)
}

/// The number of the last turn request recorded before event `anchor`.
fn last_request_before(events: &[RunEvent], anchor: EventId) -> Option<u64> {
    events
        .iter()
        .filter(|e| e.id < anchor && e.kind == event_kind::TURN_REQUESTED)
        .filter_map(|e| restore::<TurnRequested>(&e.payload).seq)
        .next_back()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An adopter asks a person rather than apply a verdict whose
    /// subagents sent the run further (ADR-t1453-1 decision 7); a review
    /// without a route, or whose agents all let it land, goes as before.
    #[test]
    fn only_a_route_one_of_its_agents_decided_stops_an_adopter() {
        let route = |destination: &str, agents: &[&str]| {
            json!({"route": {"destination": destination, "agents": agents
                .iter()
                .map(|d| json!({"agent": "a", "destination": d}))
                .collect::<Vec<_>>()}})
        };
        assert!(!agents_decided(&json!({"verdict": "pass"})));
        assert!(!agents_decided(&route("land", &["land"])));
        assert!(!agents_decided(&route("send_back", &["land"])));
        assert!(agents_decided(&route("send_back", &["land", "send_back"])));
        assert!(agents_decided(&route("ask", &["ask"])));
    }

    /// The verdict an adopter reads back from `review_finished` keeps the
    /// agents' results it recorded, and reads as before without them.
    #[test]
    fn a_recorded_verdict_keeps_its_agents_results() {
        let event = |payload: Value| RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: event_kind::REVIEW_FINISHED.to_owned(),
            payload,
            created_at: String::new(),
            actor: None,
        };
        let mut payload = json!({"verdict": "pass", "reasons": [], "summary": "ok", "attempt": 1});
        let plain = verdict_of(&event(payload.clone())).unwrap();
        assert!(plain.agents.is_empty());
        payload["agents"] = json!([{"agent": "tests", "status": "completed", "verdict": "revise",
            "reasons": ["add a test"], "summary": "s"}]);
        let with = verdict_of(&event(payload)).unwrap();
        assert_eq!(with.agents[0].agent, "tests");
        let route = with.route(true);
        let combined = landing::combined_verdict(&with, &route, false);
        assert_eq!(combined.reasons, ["tests: add a test"]);
    }

    fn wrapper(exited: bool, lives: bool, heartbeat_age_secs: i64) -> Option<WrapperSeen> {
        Some(WrapperSeen {
            exited,
            lives,
            heartbeat_age_secs,
        })
    }

    fn seen(status: RunStatus, wrapper: Option<WrapperSeen>) -> AdoptionSeen {
        AdoptionSeen {
            status,
            wrapper,
            skipped_resume: false,
            resume_in_progress: false,
        }
    }

    /// Task 1558, with the cases of
    /// `runtime_adopt::fresh_leases_dead_wrappers_early_runs_leaseless_and_integrating_runs_are_not_adopted`:
    /// a `running` or `validating` run is adopted while its wrapper lives
    /// with a heartbeat within the TTL (not past it) or has reported its
    /// exit, or when it was moved on by `resume_skipped`; an
    /// `awaiting_integration` run always; a `needs_session` run only in a
    /// resume whose wrapper lives; `claimed`, `starting` and `integrating`
    /// never.
    #[test]
    fn which_runs_with_a_stale_lease_are_adopted() {
        let at_ttl = HEARTBEAT_TIMEOUT_SECS;
        for status in [RunStatus::Running, RunStatus::Validating] {
            assert!(adoptable(&seen(status, wrapper(false, true, at_ttl))));
            assert!(!adoptable(&seen(status, wrapper(false, true, at_ttl + 1))));
            assert!(!adoptable(&seen(status, wrapper(false, false, 0))));
            assert!(adoptable(&seen(status, wrapper(true, false, at_ttl + 100))));
            assert!(!adoptable(&seen(status, None)));
            assert!(adoptable(&AdoptionSeen {
                skipped_resume: true,
                ..seen(status, None)
            }));
        }
        assert!(adoptable(&seen(RunStatus::AwaitingIntegration, None)));
        let resumed = AdoptionSeen {
            resume_in_progress: true,
            ..seen(RunStatus::NeedsSession, wrapper(false, true, at_ttl + 100))
        };
        assert!(adoptable(&resumed));
        assert!(!adoptable(&AdoptionSeen {
            resume_in_progress: false,
            ..resumed
        }));
        assert!(!adoptable(&AdoptionSeen {
            wrapper: wrapper(true, false, 0),
            ..resumed
        }));
        for status in [
            RunStatus::Claimed,
            RunStatus::Starting,
            RunStatus::Integrating,
        ] {
            assert!(!adoptable(&AdoptionSeen {
                skipped_resume: true,
                resume_in_progress: true,
                ..seen(status, wrapper(false, true, 0))
            }));
        }
        assert_eq!(wrapper_alive(None), None);
        assert_eq!(wrapper_alive(wrapper(true, false, 0)), None);
        assert_eq!(wrapper_alive(wrapper(false, true, at_ttl)), Some(true));
    }

    /// Task 1558 (the rule `runtime_adopt::dead_supervisor_pid_with_a_fresh_heartbeat_is_adopted`
    /// checks through a real process): a lease whose supervisor's pid is dead is stale however fresh its
    /// heartbeat; a live one only past the TTL.
    #[test]
    fn a_lease_is_stale_by_its_dead_pid_or_past_its_heartbeat_ttl() {
        assert!(heartbeat_stale(false, 0));
        assert!(!heartbeat_stale(true, HEARTBEAT_TIMEOUT_SECS));
        assert!(heartbeat_stale(true, HEARTBEAT_TIMEOUT_SECS + 1));
    }

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: "2026-09-30T00:00:00.000Z".to_owned(),
            actor: None,
        }
    }

    fn pass(id: i64) -> RunEvent {
        event(
            id,
            event_kind::REVIEW_FINISHED,
            json!({"verdict": "pass", "reasons": [], "summary": "ok", "attempt": 1}),
        )
    }

    /// The resumption of `events`, whose last is the anchor.
    fn resumption(events: &[RunEvent], live: bool, landing_ask_open: bool) -> ReviewResumption {
        let run = super::super::recovery::test_run(RunStatus::AwaitingIntegration, None);
        let history = RunHistory::from_events(events);
        let anchor = crate::domain::review_anchor(events).unwrap();
        review_resumption(&run, &history, anchor, live, landing_ask_open)
    }

    /// Task 1558, moved from
    /// `runtime_adopt::an_adopter_does_not_repeat_the_exit_request_recorded_after_a_passed_review`:
    /// a pass not followed by its exit request is prechecked again at the
    /// review job's request; one followed by it lands, with the exit
    /// request taken as sent. A pass recorded without its attempt names no
    /// job; one without its reasons (not a verdict) is reviewed again.
    #[test]
    fn an_adopted_pass_is_prechecked_before_its_exit_request_and_lands_after() {
        let events = [
            event(1, event_kind::REVIEW_STARTED, json!({"attempt": 1})),
            pass(2),
        ];
        assert!(matches!(
            resumption(&events, true, false),
            ReviewResumption::Precheck { job: Some(_), .. }
        ));
        let exited = [
            events[0].clone(),
            events[1].clone(),
            event(3, event_kind::EXIT_REQUESTED, json!({})),
        ];
        assert!(matches!(
            resumption(&exited, false, false),
            ReviewResumption::Exit(AfterExit::Land)
        ));
        assert!(exit_requested_after(&exited, EventId::new(2)));
        assert!(!exit_requested_after(&exited, EventId::new(3)));
        let unnumbered = [event(
            1,
            event_kind::REVIEW_FINISHED,
            json!({"verdict": "pass", "reasons": [], "summary": "ok", "attempt": "1"}),
        )];
        assert!(matches!(
            resumption(&unnumbered, true, false),
            ReviewResumption::Precheck { job: None, .. }
        ));
        let broken = [event(
            1,
            event_kind::REVIEW_FINISHED,
            json!({"verdict": "pass"}),
        )];
        assert!(matches!(
            resumption(&broken, true, false),
            ReviewResumption::Review
        ));
        let concern = [event(
            1,
            event_kind::REVIEW_FINISHED,
            json!({"verdict": "concern", "reasons": ["r"], "summary": "s", "attempt": 1}),
        )];
        assert!(matches!(
            resumption(&concern, true, false),
            ReviewResumption::Concern(_)
        ));
        let revise = [event(
            1,
            event_kind::REVIEW_FINISHED,
            json!({"verdict": "revise", "reasons": ["r"], "summary": "s", "attempt": 1}),
        )];
        assert!(matches!(
            resumption(&revise, true, false),
            ReviewResumption::Exit(AfterExit::Ask { why: Some(_), .. })
        ));
        // A route one of its subagents decided asks a person.
        let routed = [event(
            1,
            event_kind::REVIEW_FINISHED,
            json!({"verdict": "pass", "reasons": [], "summary": "s", "attempt": 1,
                "route": {"destination": "send_back", "agents": [{"destination": "send_back"}]}}),
        )];
        assert!(matches!(
            resumption(&routed, true, false),
            ReviewResumption::Exit(AfterExit::Ask { .. })
        ));
    }

    /// Task 1558: a revise or conflict request with nothing after it waits
    /// for the live session again (its attempt 1 when it is not recorded,
    /// its reasons none when they are not texts), and is reviewed again
    /// without one; a precheck that sent nothing lands or asks after its
    /// exit request.
    #[test]
    fn an_adopted_request_waits_for_its_live_session() {
        let revise = [event(
            1,
            event_kind::REVISE_REQUESTED,
            json!({"attempt": 2, "reasons": ["fix it"], "sent_at": 1000.25}),
        )];
        match resumption(&revise, true, false) {
            ReviewResumption::Revise {
                attempt, reasons, ..
            } => {
                assert_eq!(attempt, 2);
                assert_eq!(reasons, ["fix it"]);
            }
            _ => panic!("not a revise"),
        }
        assert!(matches!(
            resumption(&revise, false, false),
            ReviewResumption::Review
        ));
        let old = [event(
            1,
            event_kind::REVISE_REQUESTED,
            json!({"reasons": [1]}),
        )];
        match resumption(&old, true, false) {
            ReviewResumption::Revise {
                attempt, reasons, ..
            } => {
                assert_eq!(attempt, 1);
                assert!(reasons.is_empty());
            }
            _ => panic!("not a revise"),
        }
        let conflict = [
            pass(1),
            event(
                2,
                event_kind::CONFLICT_PRECHECK,
                json!({"requested": true, "attempt": 3}),
            ),
        ];
        assert!(matches!(
            resumption(&conflict, true, false),
            ReviewResumption::Conflict { attempt: 3, .. }
        ));
        assert!(matches!(
            resumption(&conflict, false, false),
            ReviewResumption::Review
        ));
        // `requested` of another type is a precheck that sent nothing.
        let unsent = [
            pass(1),
            event(
                2,
                event_kind::CONFLICT_PRECHECK,
                json!({"requested": "true"}),
            ),
        ];
        assert!(matches!(
            resumption(&unsent, true, false),
            ReviewResumption::Precheck { .. }
        ));
        let asked = [
            pass(1),
            event(
                2,
                event_kind::CONFLICT_PRECHECK,
                json!({"asked": "past the limit"}),
            ),
            event(3, event_kind::EXIT_REQUESTED, json!({})),
        ];
        assert!(matches!(
            resumption(&asked[..2], true, false),
            ReviewResumption::Precheck { .. }
        ));
        assert!(matches!(
            resumption(&asked, true, false),
            ReviewResumption::Exit(AfterExit::Ask { .. })
        ));
        let landed = [
            pass(1),
            event(2, event_kind::CONFLICT_PRECHECK, json!({})),
            event(3, event_kind::EXIT_REQUESTED, json!({})),
        ];
        assert!(matches!(
            resumption(&landed, true, false),
            ReviewResumption::Exit(AfterExit::Land)
        ));
        // A revise recorded but not sent asks, with its error when it is a
        // text.
        let unsent = [
            pass(1),
            event(2, event_kind::REVISE_UNSENT, json!({"error": "no session"})),
        ];
        match resumption(&unsent, true, false) {
            ReviewResumption::Exit(AfterExit::Ask { why, .. }) => {
                assert_eq!(why.as_deref(), Some("no session"));
            }
            _ => panic!("not an ask"),
        }
        let mistyped = [
            pass(1),
            event(2, event_kind::REVISE_UNSENT, json!({"error": 3})),
        ];
        assert!(matches!(
            resumption(&mistyped, true, false),
            ReviewResumption::Exit(AfterExit::Ask { why: None, .. })
        ));
    }

    /// Task 1558: the `approve_landing` ask an adopter waits for is the
    /// last one the supervisor opened after the anchor, named by an
    /// integer `ask_id`; the turn request it looks after is the last one
    /// numbered before the anchor.
    #[test]
    fn an_adopter_finds_the_landing_ask_and_the_request_of_its_anchor() {
        let opened = |id, kind: &str, by: &str, ask: Value| {
            event(
                id,
                event_kind::ASK_OPENED,
                json!({"kind": kind, "asked_by": by, "ask_id": ask}),
            )
        };
        let events = [
            opened(1, "approve_landing", "supervisor", json!(4)),
            opened(3, "approve_landing", "supervisor", json!(5)),
            opened(4, "approve_landing", "worker", json!(6)),
            opened(5, "stalled", "supervisor", json!(7)),
        ];
        assert_eq!(
            landing_ask_after(&events, EventId::new(2)),
            Some(AskId::new(5))
        );
        assert_eq!(landing_ask_after(&events, EventId::new(3)), None);
        let mistyped = [opened(3, "approve_landing", "supervisor", json!("5"))];
        assert_eq!(landing_ask_after(&mistyped, EventId::new(2)), None);
        let requests = [
            event(1, event_kind::TURN_REQUESTED, json!({"seq": 2})),
            event(2, event_kind::TURN_REQUESTED, json!({"seq": "3"})),
            event(4, event_kind::TURN_REQUESTED, json!({"seq": 5})),
        ];
        assert_eq!(last_request_before(&requests, EventId::new(3)), Some(2));
        assert_eq!(last_request_before(&requests, EventId::new(1)), None);
    }

    /// Task 1558: the verdict an adopter reads back is the domain's
    /// [`review_verdict`]; the route's parent decided only when it is a
    /// text and the route went there.
    #[test]
    fn the_route_of_a_recorded_review_is_read_leniently() {
        let finished =
            |route: Value| restore::<ReviewFinished>(&json!({"route": route})).parent_decided();
        assert!(finished(json!({"destination": "ask", "parent": "ask"})));
        assert!(!finished(json!({"destination": "ask", "parent": "land"})));
        assert!(!finished(json!({"parent": "ask"})));
        assert!(!finished(json!({"destination": 1, "parent": 1})));
        assert!(!finished(json!("ask")));
        assert!(!agents_decided(
            &json!({"route": {"destination": "ask", "agents": ["ask", 3]}})
        ));
        // Arrays are no objects.
        assert!(!agents_decided(
            &json!({"route": ["send_back", null, [["send_back"]]]})
        ));
        assert!(!agents_decided(
            &json!({"route": {"destination": "ask", "agents": [["ask"]]}})
        ));
    }
}
