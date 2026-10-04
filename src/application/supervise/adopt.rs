//! Adoption (ADR-0012): runs whose supervisor died while their session
//! lives on are taken over, each in the phase it was in.

use super::file_time::recorded_at;
use super::*;
use crate::domain::EventKind;
use crate::domain::RunEvent;
use crate::domain::concern::{ConcernDecision, EscalatedBecause};
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
                    self.slots.push(slot);
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
        if run.status() == RunStatus::NeedsSession {
            return self.resume_adoptable(run, wrapper);
        }
        if !matches!(
            run.status(),
            RunStatus::Running | RunStatus::Validating | RunStatus::AwaitingIntegration
        ) {
            return Ok(false);
        }
        if run.status() == RunStatus::AwaitingIntegration || self.skipped_resume(run.id())? {
            return Ok(true);
        }
        Ok(wrapper.is_some() && self.wrapper_alive(wrapper, now) != Some(false))
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
        Ok(
            wrapper.is_some_and(|w| w.exited_at.is_none() && self.wrapper_lives(w))
                && resume_in_progress(&self.queue.run_events(run.id())?).is_some(),
        )
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
    /// Whether the wrapper that has not reported its exit is alive (its
    /// process lives and its heartbeat is within the lease TTL); `None`
    /// when there is no wrapper or it reported its exit.
    fn wrapper_alive(&self, wrapper: Option<&RunProcess>, now: i64) -> Option<bool> {
        wrapper.and_then(|wrapper| {
            wrapper.exited_at.is_none().then(|| {
                self.wrapper_lives(wrapper) && now - wrapper.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS
            })
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
                let now = Instant::now();
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
    /// Whether the run's last resume event is `resume_skipped`: it was moved
    /// on without a session, and no resume opened one since.
    pub(super) fn skipped_resume(&self, id: &RunId) -> Result<bool> {
        Ok(RunHistory::from_events(&self.queue.run_events(id)?).last_resume_skipped())
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
        let then = match anchor.kind.as_str() {
            event_kind::REVISE_REQUESTED => {
                if let Some(live) = session.clone()
                    && session_alive(self, run.id())?
                {
                    let attempt = anchor.payload["attempt"].as_u64().unwrap_or(1) as usize;
                    let sent_at = adopted_sent_at(&anchor.payload);
                    let start = self
                        .adopted_start(
                            run,
                            &events,
                            anchor.id,
                            &live.workspace,
                            "revise request",
                            &format!("revise-{attempt}.txt"),
                        )
                        .then_some(sent_at);
                    let mut watch = ReviseWatch::new(
                        run,
                        live,
                        attempt,
                        Fix::Revise {
                            reasons: serde_json::from_value(anchor.payload["reasons"].clone())
                                .unwrap_or_default(),
                            concern: sent_back_concern(&history, anchor),
                        },
                        sent_at,
                        start,
                    )?;
                    watch.live.adopt(&*self.queue, run, anchor.id)?;
                    return Ok(Phase::Revise(watch));
                }
                None
            }
            // A conflict request with nothing after it waits for the live
            // session again, with the passed verdict before it.
            event_kind::CONFLICT_PRECHECK if anchor.payload["requested"] == true => {
                let passed = passed_before(&history, anchor.id);
                if let Some(live) = session.clone()
                    && let Some(verdict) = passed
                    && session_alive(self, run.id())?
                {
                    let attempt = anchor.payload["attempt"].as_u64().unwrap_or(1) as usize;
                    let sent_at = adopted_sent_at(&anchor.payload);
                    let start = self
                        .adopted_start(
                            run,
                            &events,
                            anchor.id,
                            &live.workspace,
                            "conflict request",
                            &format!("conflict-{attempt}.txt"),
                        )
                        .then_some(sent_at);
                    let mut watch = ReviseWatch::new(
                        run,
                        live,
                        attempt,
                        Fix::Conflict(verdict),
                        sent_at,
                        start,
                    )?;
                    watch.live.adopt(&*self.queue, run, anchor.id)?;
                    return Ok(Phase::Revise(watch));
                }
                None
            }
            // A revise request recorded but not sent asks a person, as it
            // did before the supervisor was replaced.
            event_kind::REVISE_UNSENT => {
                // A revise the review's subagents decided (ADR-t1453-1
                // decision 7) is asked with their reasons, as the
                // supervisor that could not send it asked.
                let review = history.last_before(anchor.id, event_kind::REVIEW_FINISHED);
                if let Some(review) = review.filter(|r| agents_decided(&r.payload))
                    && let Ok(verdict) = verdict_of(review)
                {
                    let why = anchor.payload["error"]
                        .as_str()
                        .unwrap_or("the revise request could not be sent");
                    // The request was recorded, so the round had a revise
                    // left when the review was decided.
                    Some(agents_ask(run, review, verdict, true, why.to_owned()))
                } else {
                    passed_before(&history, anchor.id).map(|verdict| AfterExit::Ask {
                        why: anchor.payload["error"].as_str().map(str::to_owned),
                        decision: verdict.verdict,
                        recommendation: verdict.recommendation,
                        confidence: verdict.confidence,
                        reason_category: verdict.reason_category,
                        reasons: verdict.reasons,
                        summary: verdict.summary,
                        requested_by: review_job_before(run, &history, anchor.id),
                        sent_back: None,
                    })
                }
            }
            event_kind::REVIEW_FINISHED => {
                match verdict_of(anchor) {
                    // A review whose subagents sent the run further than
                    // its verdict, or carried reasons of their own
                    // (ADR-t1453-1 decision 7), asks a person: an adopter
                    // does not apply the verdict alone.
                    Ok(verdict) if agents_decided(&anchor.payload) => {
                        let revise_left = decide_revise(&history) != ReviseDecision::Ask;
                        let why = format!(
                            "the review's subagents sent the run to {} and the supervisor was replaced before it was applied",
                            anchor.payload["route"]["destination"]
                                .as_str()
                                .unwrap_or("a person")
                        );
                        Some(agents_ask(run, anchor, verdict, revise_left, why))
                    }
                    // A concern is decided again from its verdict
                    // (ADR-t451-1 decision 3).
                    Ok(verdict) if verdict.verdict == ReviewDecision::Concern => {
                        match self.adopted_concern(
                            run,
                            session.clone(),
                            &history,
                            anchor,
                            verdict,
                        )? {
                            Ok(phase) => return Ok(phase),
                            Err(then) => Some(then),
                        }
                    }
                    // A pass not yet followed by its /exit is prechecked
                    // (again): main may have moved.
                    Ok(verdict)
                        if verdict.verdict == ReviewDecision::Pass
                            && !history.has_after(anchor.id, event_kind::EXIT_REQUESTED) =>
                    {
                        return self.adopted_precheck(
                            run,
                            session,
                            verdict,
                            review_job(run, anchor),
                        );
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
            // ask) or to ask a person (past the limit); before its /exit it
            // is prechecked again, as main may have moved.
            event_kind::CONFLICT_PRECHECK => match passed_before(&history, anchor.id) {
                Some(verdict) if !history.has_after(anchor.id, event_kind::EXIT_REQUESTED) => {
                    let job = review_job_before(run, &history, anchor.id);
                    return self.adopted_precheck(run, session, verdict, job);
                }
                Some(verdict) => Some(match anchor.payload["asked"].as_str() {
                    Some(why) => Fix::Conflict(verdict).ask(
                        String::new(),
                        why.to_owned(),
                        review_job_before(run, &history, anchor.id),
                    ),
                    None => AfterExit::Land,
                }),
                None => None,
            },
            event_kind::VALIDATION_FINISHED
                if events
                    .iter()
                    .any(|e| e.kind == event_kind::INTEGRATION_APPROVED) =>
            {
                Some(AfterExit::Land)
            }
            // A rebased run a landing recheck resumed waits for the answer
            // to its approve_landing ask, not a review (ADR-0068 decision 4).
            event_kind::VALIDATION_FINISHED
                if crate::domain::resume::parked_by_recheck(&events)
                    && self
                        .queue
                        .has_unclosed_ask(run.id(), AskKind::ApproveLanding)? =>
            {
                Some(AfterExit::Rest { close: true })
            }
            _ => None,
        };
        let Some(mut then) = then else {
            return self.start_review(run, session);
        };
        // The ask was opened before the supervisor died, after this anchor:
        // the run waits for it (open or answered) rather than asking again,
        // which would close it as stale and notify the inbox twice (task 425).
        if matches!(then, AfterExit::Ask { .. })
            && let Some(ask) = self.unclosed_landing_ask_after(&events, anchor)?
        {
            info!(run_id = %run.id(), ask_id = %ask.id, "run {} waits for a person in ask {}, opened before the supervisor stopped; it is not asked again", run.id(), ask.id);
            then = AfterExit::Rest { close: true };
        }
        let after = |kind: &str| events.iter().any(|e| e.id > anchor.id && e.kind == kind);
        let mut watch = ExitWatch::new(session, then);
        // Never a second exit request after the anchor (task 959).
        watch.requested = after(event_kind::EXIT_REQUESTED);
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
            let attempt = started.payload["attempt"].as_u64().unwrap_or(1);
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
        let opened = events.iter().rev().find(|e| {
            e.id > anchor.id
                && e.kind == event_kind::ASK_OPENED
                && e.payload["kind"] == AskKind::ApproveLanding.as_str()
                && e.payload["asked_by"] == "supervisor"
        });
        let Some(ask_id) = opened.and_then(|e| e.payload["ask_id"].as_i64()) else {
            return Ok(None);
        };
        let ask = self.queue.read_ask(AskId::new(ask_id))?;
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
    /// up to the resume timeout. Whether its text was read.
    fn adopted_start(
        &mut self,
        run: &TaskRun,
        events: &[crate::domain::RunEvent],
        anchor: EventId,
        workspace: &str,
        what: &str,
        file: &str,
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
        let after = events
            .iter()
            .filter(|e| e.id < anchor && e.kind == event_kind::TURN_REQUESTED)
            .filter_map(|e| e.payload["seq"].as_u64())
            .next_back();
        let requests = match listed_requests(&*self.files, run_dir) {
            Ok(requests) => requests,
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "the adopted {what} of {} cannot be delivered: {error:#}", run.id());
                return true;
            }
        };
        match turn::adopted_delivery(&requests, what, &text, after) {
            turn::AdoptedDelivery::Delivered(seq) => {
                info!(run_id = %run.id(), "the adopted {what} of {} was written as request {seq}; it is not written again", run.id());
            }
            turn::AdoptedDelivery::Write => {
                info!(run_id = %run.id(), "the adopted {what} of {} was recorded but not written; writing it once", run.id());
                if let Err(error) = request_turn(self, run, workspace, Input::Text(&text), what) {
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
        if review.payload["verdict"] != ReviewDecision::Concern.as_str()
            || (agents_decided(&review.payload) && !parent_decided(&review.payload))
            || history.has_after(review.id, event_kind::CONCERN_DECIDED)
            || !history.has_after(review.id, event_kind::REVISE_REQUESTED)
        {
            return Ok(false);
        }
        let (Ok(verdict), Some(attempt)) = (verdict_of(review), review.payload["attempt"].as_u64())
        else {
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
            && let Some(attempt) = anchor.payload["attempt"].as_u64()
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
    event.payload["attempt"]
        .as_u64()
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
    let route = &payload["route"];
    let Some(destination) = route["destination"].as_str() else {
        return false;
    };
    destination != "land"
        && route["agents"]
            .as_array()
            .is_some_and(|agents| agents.iter().any(|a| a["destination"] == destination))
}

/// Whether the verdict's own judgment went where the route a
/// `review_finished` recorded went (ADR-t1453-1 decision 7).
fn parent_decided(payload: &Value) -> bool {
    let route = &payload["route"];
    route["parent"].is_string() && route["parent"] == route["destination"]
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
    let attempt = review.payload["attempt"].as_u64()?;
    let applied = history.events().iter().any(|event| {
        event.id > review.id
            && event.kind == event_kind::CONCERN_DECIDED
            && event.payload["attempt"].as_u64() == Some(attempt)
            && event.payload["applied"].as_bool() == Some(true)
    });
    if !applied {
        return None;
    }
    SentBackConcern::of(&verdict_of(review).ok()?, attempt as usize)
}

/// The verdict a `review_finished` recorded, with a concern's
/// recommendation, confidence and reason when it gave them.
fn verdict_of(event: &RunEvent) -> serde_json::Result<ReviewVerdict> {
    let mut verdict = json!({
        "verdict": event.payload["verdict"],
        "reasons": event.payload["reasons"],
        "summary": event.payload["summary"],
        "recommendation": event.payload["recommendation"],
        "confidence": event.payload["confidence"],
        "reason_category": event.payload["reason_category"],
    });
    // The required subagents' results, recorded only when the review had
    // them (ADR-t1453-1).
    if let Some(agents) = event.payload.get("agents").filter(|a| a.is_array()) {
        verdict["agents"] = agents.clone();
    }
    serde_json::from_value(verdict)
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
}
