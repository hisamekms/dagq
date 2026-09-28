//! Adoption (ADR-0012): runs whose supervisor died while their session
//! lives on are taken over, each in the phase it was in.

use super::stall_recovery::recorded_at;
use super::*;
use crate::domain::EventKind;
use crate::domain::RunEvent;

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
            wrapper.is_some_and(|w| w.exited_at.is_none() && self.processes.alive(w.pid))
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
                self.processes.alive(wrapper.pid)
                    && now - wrapper.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS
            })
        })
    }
    /// Rebuild the slot of an adopted run from what the queue and the run
    /// directory hold: the planned paths, whether the receipt is already on
    /// disk and whether `/exit` was already requested (never sent twice, and
    /// an `exit_request_timed_out` already recorded is not recorded again).
    /// The wait after the receipt and the `/exit`'s timeout run from the
    /// events that started them (`receipt_observed`, and `exit_requested` or
    /// a dialog answered by rule since), not from the takeover: supervisors
    /// handed over more often than the resume timeout must not hold a run
    /// forever (task 879). The wrapper is registered, so no registration
    /// timeout applies. A `validating` run restarts validation from the
    /// beginning: it is a function of the receipt and the worktree alone.
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
                // Only the latest request's timeout and ask count: a run
                // resumed and stuck again is asked again (task 240).
                let exit_timed_out = history.latest_exit_timed_out();
                // The retries recorded are not made again (ADR-0047
                // decision 25).
                let exit_retry = Box::new(ExitRetry::adopt(&history, since));
                let first_commit_seen = history.has(event_kind::FIRST_COMMIT_OBSERVED);
                // A dialog recorded before adoption is not recorded again
                // while the same screen stays up; one the receipt ended is
                // kept so the adopter closes its `answer_prompt` ask
                // (task 239).
                let prompt_hash = if receipt_seen {
                    history.prompt_hash_at_receipt()
                } else {
                    history.waiting_prompt_hash()
                }
                .map(Box::from);
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
                    exit_timed_out,
                    exit_retry,
                    first_commit_seen,
                    agent_seen: None,
                    prompt_checked: None,
                    prompt_hash,
                    // A timeout recorded without its ask (by a binary that
                    // made none, or a supervisor that died between the two)
                    // still gets one; one asked before is not asked again.
                    // A request that has not timed out yet is asked about
                    // (after its retries) once the adopter's own timeout
                    // passes.
                    exit_asked: history.latest_exit_asked(),
                    // Only a run whose wrapper heartbeats is adopted.
                    silent: false,
                    exit_for_silence: false,
                    answer_start: None,
                    stall: StallWatch::adopt(&*self.queue, run)?,
                    stale: adopted_stale_nudge(&*self.queue, run, SESSION_PHASE, None)?,
                    recovery: RecoveryWatch::adopt(&*self.queue, run)?,
                    input_at: None,
                    answered_at: None,
                    asks_from: 0,
                    stage: Stage::Session,
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
    /// session again (it is recorded before it is typed, so it is typed a
    /// second time only when the session shows no sign of it,
    /// [`StartCheck::adopted`]), with the dialog and the recovery jobs the
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
        let then = match anchor.kind.as_str() {
            event_kind::REVISE_REQUESTED => {
                if let Some(live) = session.clone()
                    && session_alive(self, run.id())?
                {
                    let attempt = anchor.payload["attempt"].as_u64().unwrap_or(1) as usize;
                    let sent_at = adopted_sent_at(&anchor.payload);
                    let start = self.adopted_start(
                        run,
                        &events,
                        anchor.id,
                        "revise request",
                        &format!("revise-{attempt}.txt"),
                        sent_at,
                    );
                    let mut watch = ReviseWatch::new(
                        run,
                        live,
                        attempt,
                        Fix::Revise(
                            serde_json::from_value(anchor.payload["reasons"].clone())
                                .unwrap_or_default(),
                        ),
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
                    let start = self.adopted_start(
                        run,
                        &events,
                        anchor.id,
                        "conflict request",
                        &format!("conflict-{attempt}.txt"),
                        sent_at,
                    );
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
                passed_before(&history, anchor.id).map(|verdict| AfterExit::Ask {
                    why: anchor.payload["error"].as_str().map(str::to_owned),
                    decision: verdict.verdict,
                    reasons: verdict.reasons,
                    summary: verdict.summary,
                })
            }
            event_kind::REVIEW_FINISHED => {
                match serde_json::from_value::<ReviewVerdict>(json!({
                    "verdict": anchor.payload["verdict"],
                    "reasons": anchor.payload["reasons"],
                    "summary": anchor.payload["summary"],
                })) {
                    // A pass not yet followed by its /exit is prechecked
                    // (again): main may have moved.
                    Ok(verdict)
                        if verdict.verdict == ReviewDecision::Pass
                            && !history.has_after(anchor.id, event_kind::EXIT_REQUESTED) =>
                    {
                        return self.precheck(run, session, verdict);
                    }
                    Ok(verdict) if verdict.verdict == ReviewDecision::Pass => Some(AfterExit::Land),
                    Ok(verdict) => Some(AfterExit::Ask {
                        why: (verdict.verdict == ReviewDecision::Revise).then(|| {
                            "the revise could not go on when the supervisor was replaced".to_owned()
                        }),
                        decision: verdict.verdict,
                        reasons: verdict.reasons,
                        summary: verdict.summary,
                    }),
                    Err(_) => None,
                }
            }
            // A precheck that sent nothing decided to land (no session to
            // ask) or to ask a person (past the limit); before its /exit it
            // is prechecked again, as main may have moved.
            event_kind::CONFLICT_PRECHECK => match passed_before(&history, anchor.id) {
                Some(verdict) if !history.has_after(anchor.id, event_kind::EXIT_REQUESTED) => {
                    return self.precheck(run, session, verdict);
                }
                Some(verdict) => Some(match anchor.payload["asked"].as_str() {
                    Some(why) => Fix::Conflict(verdict).ask(String::new(), why.to_owned()),
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
        // Never a second /exit; its timeout restarts now.
        if after(event_kind::EXIT_REQUESTED) {
            watch.requested = Some(Instant::now());
        }
        watch.timed_out =
            after(event_kind::EXIT_REQUEST_TIMED_OUT) && history.latest_exit_timed_out();
        // A timeout recorded without its ask still gets one; one asked
        // about the same request is not asked again (as for a running run,
        // task 104), one about an earlier request is (task 240).
        watch.exit_asked = history.latest_exit_asked();
        // Its retries recorded are not made again (ADR-0047 decision 25).
        if watch.timed_out {
            let now = Instant::now();
            watch.retry = ExitRetry::adopt(&history, |event| self.instant_of(event, now));
        }
        // A /exit that never got there, whose close to land the previous
        // supervisor decided but did not record, is judged again now
        // (task 464) rather than waited out as one the session held back.
        if watch.requested.is_some()
            && let Some(unsent) = history.latest_exit_unsent_to_land()
            && let Some(workspace) = watch.session.as_ref().map(|s| s.workspace.clone())
        {
            let attempts = unsent.payload["attempts"].as_u64().unwrap_or(1);
            watch.adopt_unsent(self, run, &workspace, attempts)?;
        }
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
        // The failed review's /exit was requested before the ask: never a
        // second one.
        if after(event_kind::EXIT_REQUESTED) {
            watch.requested = Some(Instant::now());
        }
        let history = RunHistory::from_events(events);
        watch.timed_out =
            after(event_kind::EXIT_REQUEST_TIMED_OUT) && history.latest_exit_timed_out();
        watch.exit_asked = history.latest_exit_asked();
        if watch.timed_out {
            let now = Instant::now();
            watch.retry = ExitRetry::adopt(&history, |event| self.instant_of(event, now));
        }
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
    /// The instant from which the `/exit`'s timeout runs, on the monotonic
    /// clock whose `now` is `now`: the last `exit_requested` of `events`
    /// that `of` keeps, or the last restart of its timeout since: a known
    /// dialog answered by rule (ADR-0047 decision 29), or a `stuck_exit`
    /// recovery job that answered the dialog or stopped the processes
    /// holding the `/exit` back. `None` when no such request was recorded.
    /// A takeover keeps the time already waited (tasks 879 and 894).
    pub(super) fn exit_requested_at(
        &self,
        events: &[RunEvent],
        of: impl Fn(&RunEvent) -> bool,
        now: Instant,
    ) -> Option<Instant> {
        let requested = events
            .iter()
            .rfind(|e| e.kind == event_kind::EXIT_REQUESTED && of(e))?;
        let restarted = events
            .iter()
            .rev()
            .find(|e| e.id > requested.id && restarts_exit_timeout(e));
        Some(self.instant_of(restarted.unwrap_or(requested), now))
    }
}

/// Whether `event` gave a held `/exit` its timeout again: a known dialog
/// answered by rule (ADR-0047 decision 29), or a `stuck_exit` recovery job
/// that answered the dialog or stopped the processes holding it back.
pub(super) fn restarts_exit_timeout(event: &RunEvent) -> bool {
    event.kind == event_kind::AUTO_REPAIRED
        && (event.payload["repair"] == DIALOG_ANSWERED
            || event.payload["alert"] == RecoveryAlert::StuckExit.as_str()
                && [RECOVERY_DIALOG_ANSWERED, "stop_processes"]
                    .iter()
                    .any(|repair| event.payload["repair"] == *repair))
}

/// When an adopted revise or conflict request was recorded as sent.
fn adopted_sent_at(payload: &Value) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(payload["sent_at"].as_u64().unwrap_or_default())
}

impl Supervisor<'_> {
    /// The start check of an adopted request (task 546): the request was
    /// recorded before it was typed, so the supervisor adopted from may have
    /// stopped in between. Its text is the one written to `file` in the run
    /// directory before the record; without it the request is only waited
    /// for, up to the resume timeout.
    /// What the checks of earlier supervisors recorded after the request
    /// (event `anchor`) carries over: one that sent it again leaves no
    /// second resend, and one that recorded `submit_not_started` leaves
    /// nothing to check (its recovery job has it).
    fn adopted_start(
        &self,
        run: &TaskRun,
        events: &[crate::domain::RunEvent],
        anchor: EventId,
        what: &str,
        file: &str,
        sent_at: SystemTime,
    ) -> Option<StartCheck> {
        let checked = |kind: &str| {
            events
                .iter()
                .skip_while(|e| e.id != anchor)
                .skip(1)
                .any(|e| e.kind == kind && e.payload["what"] == what)
        };
        if checked(event_kind::SUBMIT_NOT_STARTED) {
            return None;
        }
        let resent = checked(event_kind::SUBMIT_RESENT);
        let path = Path::new(run.run_dir()?).join(file);
        match self.files.read_to_string(&path) {
            Ok(text) => Some(StartCheck::adopted(what, &text, sent_at, resent)),
            Err(error) => {
                warn!(run_id = %run.id(), error = %error, "the adopted {what} of {} cannot be checked for a start: {} could not be read: {error}", run.id(), path.display());
                None
            }
        }
    }
}

/// The verdict of the last `review_finished` before event `before`: the
/// pass a conflict precheck followed, or the revise a `revise_unsent` did.
pub(super) fn passed_before(history: &RunHistory<'_>, before: EventId) -> Option<ReviewVerdict> {
    history
        .last_before(before, event_kind::REVIEW_FINISHED)
        .and_then(|e| {
            serde_json::from_value(json!({
                "verdict": e.payload["verdict"],
                "reasons": e.payload["reasons"],
                "summary": e.payload["summary"],
            }))
            .ok()
        })
}
