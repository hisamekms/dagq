//! The providers a supervisor's workers run on, and moving a worker off
//! one it cannot use (ADR-t813-2). Claude's login or usage limit is held by
//! the queue's `queue_hold` ask (it also stops the Claude-only jobs, which
//! a person must wait for); Codex's walls, and an agent of either provider
//! that did not start, by a [`ProviderHold`], recorded on the queue as
//! `provider_held` and ended on its own at its time (a usage limit's reset
//! when its text says it) or by a person's `done` on a hold ask
//! (`provider_released`). A claim runs each task on the [`WorkerRoute`] the
//! holds and the adapters leave it; a headless turn that failed at its
//! provider's login, usage limit or start has its call made on the other
//! provider in a new session ([`Supervisor::switch`]), or waits for a
//! provider, never failed ([`Supervisor::turn_at_wall`]). Every worker
//! session is headless since task 1437 (ADR-t1433-2).

use super::file_time::recorded_at;
use super::landing::{REVIEW_PROVIDER_DISABLED, ReviewRoute};
use super::*;
use crate::application::prompt::{
    NEXT_TURN_NAME_BYTES, PROVIDER_MESSAGE_BYTES, PROVIDER_TURN_LIMIT, UNDELIVERED_REQUEST_BYTES,
};
use crate::application::prompt_fit::{Fit, Keep, NOT_READABLE};
use crate::domain::{
    actor_model::{
        ActorLaunch, JobRoute, JobStartRoute, ModelRole, UnnamedWithoutClaude, job_route,
        job_start_route, job_wait_text,
    },
    claim_hold::QueueHold,
    provider_switch::{
        self, MAX_PROVIDER_SWITCHES, ProviderHold, SwitchPhase, SwitchReason, WallMove,
        WorkerRoute, switched_payload,
    },
    throughput_review::{HELD_FINISHES, finishes_to_hold},
    turn::{TurnFailure, TurnRequest, taken_path},
    worker::WorkerMode,
};

/// Why `provider` is held now ([`provider_switch::held`]), under the name
/// the goal review's route uses.
pub(super) use crate::domain::provider_switch::held as provider_held_of;

impl Supervisor<'_> {
    /// Why `provider` is held for the workers now, if it is: Claude by the
    /// queue's open hold ask or its [`ProviderHold`] (an agent that did not
    /// start), Codex by its [`ProviderHold`].
    pub(super) fn provider_held(&self, provider: Provider) -> Option<SwitchReason> {
        provider_held_of(
            provider,
            self.no_claude,
            self.queue_hold
                .and_then(|hold| SwitchReason::of_hold(hold.reason)),
            provider_switch::own_hold(&self.provider.holds, provider),
        )
    }

    /// How this pass's claims run each worker a task may ask for.
    pub(super) fn routes(&self) -> Vec<WorkerRoute> {
        provider_switch::routes(
            &self.claim.workers,
            |provider| self.provider_held(provider),
            self.provider.fallback.workers,
        )
    }

    /// Whether `worker` has no route only because `[provider_fallback]
    /// workers` is off (ADR-t1857-1).
    pub(super) fn stopped_by_fallback(&self, worker: Worker) -> bool {
        !self.provider.fallback.workers
            && provider_switch::stopped_by_fallback(
                &self.claim.workers,
                |provider| self.provider_held(provider),
                worker,
            )
    }

    /// Read `[provider_fallback]` again (ADR-t1857-1): a change takes
    /// effect from this pass on. A file that cannot be read or holds an
    /// invalid value keeps the value in use, warned of once per error; so
    /// does a missing file, which may only be a checkout rewriting it.
    pub(super) fn reread_provider_fallback(&mut self) {
        let Some(read) = self.provider.fallback_file.clone() else {
            return;
        };
        let to = match read() {
            Ok(Some(to)) => to,
            Ok(None) => {
                self.provider.fallback_error = None;
                return;
            }
            Err(error) => {
                let message = format!("{error:#}");
                if self.provider.fallback_error.as_ref() != Some(&message) {
                    warn!(error = %message, "[provider_fallback] of dagq.toml not read: {message}; keeping workers = {}, jobs = {}", self.provider.fallback.workers, self.provider.fallback.jobs);
                    self.provider.fallback_error = Some(message);
                }
                return;
            }
        };
        self.provider.fallback_error = None;
        if to != self.provider.fallback {
            info!(
                "[provider_fallback] of dagq.toml changed: workers {} -> {}, jobs {} -> {}",
                self.provider.fallback.workers, to.workers, self.provider.fallback.jobs, to.jobs
            );
            self.provider.fallback = to;
        }
    }

    /// Whether a headless worker of `provider` can take a run now: this
    /// supervisor has its adapters and it is not held.
    fn headless_usable(&self, provider: Provider) -> bool {
        self.claim.workers.contains(&Worker {
            provider,
            mode: WorkerMode::Headless,
        }) && self.provider_held(provider).is_none()
    }

    /// Read the providers' holds from the queue every pass (the queue's are
    /// the ones in place, whichever supervisor recorded or ended them), and
    /// end each once its time is up: the next call of its provider checks
    /// it again (ADR-t813-2 decision 6).
    pub(super) fn check_provider_holds(&mut self) -> Result<()> {
        let mut latest: Vec<RunEvent> = Vec::new();
        for kind in [event_kind::PROVIDER_HELD, event_kind::PROVIDER_RELEASED] {
            latest.extend(self.queue.latest_events_of(kind, HOLD_EVENTS_READ)?);
        }
        latest.sort_by_key(|e| std::cmp::Reverse(e.id));
        self.provider.holds = [Provider::Claude, Provider::Codex]
            .into_iter()
            .filter_map(|provider| {
                latest
                    .iter()
                    .find(|e| e.payload["provider"] == provider.as_str())
                    .and_then(|e| ProviderHold::in_place(&e.kind, &e.payload))
            })
            .collect();
        let now = self.generators.clock.now();
        for hold in self.provider.holds.clone() {
            if hold.due(now) {
                self.release_provider(hold.provider, "retry_due")?;
            }
        }
        Ok(())
    }

    /// Hold `provider` for `reason`, which a turn of `run` (or a headless
    /// job, with no run, ADR-t1063-1 decision 5) hit (`message`, the
    /// turn's, may say when a usage limit resets); a hold in place stays as
    /// it is.
    pub(super) fn hold_provider(
        &mut self,
        provider: Provider,
        reason: SwitchReason,
        run: Option<&RunId>,
        message: &str,
    ) -> Result<()> {
        if self.provider.holds.iter().any(|h| h.provider == provider) {
            return Ok(());
        }
        let now = self.generators.clock.now();
        let reset = (reason == SwitchReason::UsageLimit)
            .then(|| provider_switch::reset_at(message, now))
            .flatten();
        let hold = ProviderHold::new(provider, reason, now).until(reset);
        let mut payload = hold.held_payload(run);
        payload["reset_read"] = json!(reset.is_some());
        payload["supervisor"] = json!(self.token);
        self.queue
            .record_queue_event(EventKind::ProviderHeld, payload)?;
        warn!(run_id = %run.map_or_else(String::new, ToString::to_string), "{} is held ({}): its workers and jobs go to the other provider until {}", provider.as_str(), reason.as_str(), hold.retry_at);
        self.provider.holds.push(hold);
        Ok(())
    }

    /// End `provider`'s hold (`why`: `retry_due`, or `done` when a person
    /// answered a hold ask).
    fn release_provider(&mut self, provider: Provider, why: &str) -> Result<()> {
        let Some(at) = self
            .provider
            .holds
            .iter()
            .position(|hold| hold.provider == provider)
        else {
            return Ok(());
        };
        let hold = self.provider.holds.remove(at);
        let mut payload = hold.released_payload(why);
        payload["supervisor"] = json!(self.token);
        self.queue
            .record_queue_event(EventKind::ProviderReleased, payload)?;
        info!(
            "{} is no longer held ({why}): its next call checks it again",
            provider.as_str()
        );
        Ok(())
    }

    /// End every provider's hold: a person answered a hold ask `done`.
    pub(super) fn release_provider_holds(&mut self, why: &str) -> Result<()> {
        for provider in [Provider::Claude, Provider::Codex] {
            self.release_provider(provider, why)?;
        }
        Ok(())
    }

    /// The headless session of `run` (in `workspace`) whose last turn
    /// failed with `failure`, a provider that cannot be used (ADR-t813-2):
    /// the provider is held (Codex, and Claude for an agent that did not
    /// start, by its [`ProviderHold`]; Claude's login or usage limit by the
    /// hold ask, since the Claude-only jobs wait for a person), and the
    /// call the turn made is made again on the other provider as the first
    /// turn of a new session there, when that one can be used and the run
    /// has switches left. Otherwise the run waits, never failed: once its
    /// own provider's hold ends it makes the call again there
    /// (`provider retry`), or once the other provider can take it and
    /// switches are left it moves; while both providers cannot be used it
    /// is in the hold ask a person answers. Called on every look at the
    /// turn; a turn already answered (moved, retried, continued) is left as
    /// it is.
    pub(super) fn turn_at_wall(
        &mut self,
        run: &TaskRun,
        workspace: &str,
        failure: TurnFailure,
    ) -> Result<WallStep> {
        // The slot's copy may predate an earlier switch.
        let run = &self.queue.run(run.id())?;
        let events = self.queue.run_events(run.id())?;
        let Some((finished_at, finished)) = events
            .iter()
            .enumerate()
            .rfind(|(_, e)| e.kind == event_kind::TURN_FINISHED)
            .map(|(at, e)| (at, e.payload.clone()))
        else {
            return Ok(WallStep::Held);
        };
        let turn = finished["turn"].as_u64().unwrap_or(0);
        if let Some(switched) = provider_switch::switch_of_turn(&events, turn) {
            return Ok(WallStep::Switched(recorded_at(switched)));
        }
        // A request after the turn (a retry, the hold's continue) answers it.
        if let Some(requested) = events[finished_at..]
            .iter()
            .find(|e| e.kind == event_kind::TURN_REQUESTED)
        {
            return Ok(WallStep::Switched(recorded_at(requested)));
        }
        let Some(reason) = SwitchReason::of_failure(failure) else {
            return Ok(WallStep::Held);
        };
        let from = run.actual_provider();
        let message = finished["message"].as_str().unwrap_or("no message");
        let claude_wall = provider_switch::claude_wall(from, reason);
        let first_look = !provider_switch::waiting_on(&events, turn);
        if first_look && !claude_wall {
            self.hold_provider(from, reason, Some(run.id()), message)?;
        }
        let to = from.other();
        let started = events
            .iter()
            .rfind(|e| e.kind == event_kind::TURN_STARTED && e.payload["turn"] == turn)
            .map(|e| e.payload.clone())
            .unwrap_or_default();
        let request = started["request"].as_u64();
        // The call the failed turn made, to make again.
        let undelivered = request.and_then(|seq| self.taken_request(run, seq));
        let fallback = self.provider.fallback.workers;
        let may_switch = provider_switch::may_switch(&events);
        let other_usable = may_switch && self.headless_usable(to);
        let own_held = self.provider_held(from).is_some();
        // The hold ask is read only when the run does not move and its own
        // provider is free again.
        let hold_unclosed = !(fallback && other_usable)
            && !first_look
            && !own_held
            && self.queue.hold_unclosed(run.id())?;
        let next = provider_switch::wall_move(
            fallback,
            may_switch,
            other_usable,
            first_look,
            own_held,
            hold_unclosed,
        );
        if next == WallMove::Switch {
            let phase = SwitchPhase::of_request(request, started["what"].as_str().unwrap_or(""));
            let text = switch_text(run, from, to, reason, message, undelivered.as_ref());
            self.switch(
                run,
                workspace,
                to,
                reason,
                phase,
                Some(turn),
                message,
                &text,
            )?;
            if claude_wall {
                self.open_claude_hold(run, reason, &format!("turn {turn}: {message}"))?;
            }
            return Ok(WallStep::Switched(self.files.now()));
        }
        // Its own provider can be used again (its hold ended), and no hold
        // ask holds the run for a person's `done`: the call is made again
        // there, in the same session.
        if next == WallMove::Retry {
            let text = retry_text(
                from,
                reason,
                undelivered.as_ref(),
                run.run_dir().map(Path::new),
            );
            request_turn(self, run, workspace, Input::from(&text), PROVIDER_RETRY)?;
            info!(run_id = %run.id(), "run {}: {} can be used again; the call of turn {turn} is made again", run.id(), from.as_str());
            return Ok(WallStep::Switched(self.files.now()));
        }
        if next == (WallMove::Wait { record: true }) {
            let blocked = switch_blocked(fallback, &events, self.provider_held(to));
            self.queue.record_runtime_event(
                run.id(),
                EventKind::ProviderWaiting,
                json!({
                    "turn": turn,
                    "provider": from,
                    "reason": reason,
                    "other": to,
                    "blocked": blocked,
                    "retry_at": self
                        .provider.holds
                        .iter()
                        .find(|hold| hold.provider == from)
                        .map(|hold| hold.retry_at),
                }),
            )?;
            info!(run_id = %run.id(), "run {} waits for a provider: {} cannot be used ({}) and the run cannot move to {} ({blocked})", run.id(), from.as_str(), reason.as_str(), to.as_str());
        }
        // A person is asked when both providers cannot be used, and always
        // for Claude's login or usage limit (the Claude-only jobs wait too):
        // the run joins the hold ask, whose `done` has it go on.
        if let Some(wall) = self.wall_to_raise(from, reason, to)? {
            let text = format!(
                "turn {turn} of the headless session failed on {} ({}): {message}",
                from.as_str(),
                reason.as_str()
            );
            // Held in the queue's one queue_hold ask of the wall (task
            // 438), as a failed headless job is.
            raise_wall(self, run, workspace, &text, wall)?;
        }
        Ok(WallStep::Held)
    }

    /// The hold ask a run that waits for a provider joins, if any: Claude's
    /// login or usage limit always opens it; otherwise only when the other
    /// provider cannot be used either, with the wall of the hold ask open
    /// already, or of whichever provider's hold is a login or a usage
    /// limit. Two agents that do not start open none: the run waits on
    /// their holds. Under `--no-claude` none opens. `to` is
    /// `from.other()` ([`provider_switch::wall_to_raise`]).
    /// The hold ask is read afresh: one a person answered since the top of
    /// this pass no longer holds Claude.
    fn wall_to_raise(
        &self,
        from: Provider,
        reason: SwitchReason,
        to: Provider,
    ) -> Result<Option<Wall>> {
        // The asks are read only when the hold ask decides it: not under
        // `--no-claude`, nor for Claude's own wall.
        let open_hold = if self.no_claude || provider_switch::claude_wall(from, reason) {
            None
        } else {
            self.queue
                .asks(AskQuery::default())?
                .iter()
                .find_map(crate::domain::queue_hold::hold_of)
                .map(|hold| hold.reason)
        };
        Ok(provider_switch::wall_to_raise(
            self.no_claude,
            from,
            reason,
            self.claim.workers.contains(&Worker {
                provider: to,
                mode: WorkerMode::Headless,
            }),
            open_hold,
            &self.provider.holds,
        ))
    }

    /// Move `run`'s worker to headless `to` for `reason` (in `phase`, in
    /// place of turn `turn`) and ask its session for `text` as the first
    /// turn of the new session.
    #[allow(clippy::too_many_arguments)]
    fn switch(
        &mut self,
        run: &TaskRun,
        workspace: &str,
        to: Provider,
        reason: SwitchReason,
        phase: SwitchPhase,
        turn: Option<u64>,
        message: &str,
        text: &FittedPrompt,
    ) -> Result<TaskRun> {
        let worker = Worker {
            provider: to,
            mode: WorkerMode::Headless,
        };
        let count = provider_switch::switches(&self.queue.run_events(run.id())?) + 1;
        let moved = self.queue.switch_provider(
            run.id(),
            &self.token,
            worker,
            switched_payload(
                run.actual_provider(),
                worker,
                reason,
                phase,
                turn,
                count,
                Some(message),
            ),
        )?;
        warn!(run_id = %run.id(), "run {} moved from {} to {} ({}, switch {count} of at most {MAX_PROVIDER_SWITCHES}): a new session goes on in its worktree", run.id(), run.actual_provider().as_str(), to.as_str(), reason.as_str());
        // The slot's copy of the run takes its new worker after this step.
        self.provider.moved.insert(run.id().clone(), worker);
        // The new provider's worker is told as it takes it (Codex does no
        // subagent review, say).
        if let Some(run_dir) = moved.run_dir() {
            let task = self.queue.show(moved.task_id())?.task;
            self.write_prompt(&task, &moved, Path::new(run_dir))?;
        }
        // The headless session takes the call as its next turn.
        request_turn(self, &moved, workspace, Input::from(text), PROVIDER_SWITCH)?;
        Ok(moved)
    }

    /// Request `seq` of `run`'s session as the wrapper took it.
    fn taken_request(&self, run: &TaskRun, seq: u64) -> Option<TurnRequest> {
        let run_dir = Path::new(run.run_dir()?);
        let text = self.files.read_to_string(&taken_path(run_dir, seq)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Open Claude's hold ask (or join the open one) for the Claude-only
    /// jobs, with no run in it: `run` moved on to Codex (ADR-t813-2
    /// decision 6). The wall is recorded on the run.
    fn open_claude_hold(&mut self, run: &TaskRun, reason: SwitchReason, text: &str) -> Result<()> {
        // An agent that did not start is no wall a person moves.
        let Some(wall) = provider_switch::wall_of(reason) else {
            return Ok(());
        };
        let (outcome, _) = ask::hold(&mut *self.queue, NewHold::wall(wall, None, None))?;
        self.queue.record_runtime_event(
            run.id(),
            wall.event_kind(),
            json!({
                "excerpt": text,
                "ask_id": outcome.ask.id,
                "switched_to": Provider::Codex,
            }),
        )?;
        Ok(())
    }
}

/// How a headless turn at its provider's wall was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WallStep {
    /// The run moved to the other provider, or its call was made again;
    /// the request was written then.
    Switched(SystemTime),
    /// The run waits for a provider (in the hold ask, or on a hold).
    Held,
}

/// Where the next review of a run goes (ADR-t1207-1), given
/// `--no-claude` (`no_claude`), whether `[roles.review]` names its
/// provider (`switchable`), the role's `launch`, the queue's open hold ask
/// (`queue_hold`) and why a job cannot start on each provider now
/// (`unusable`). A role that names no provider reviews on Claude as
/// before: under `--no-claude` the person reviews it, and it waits while
/// the hold ask holds Claude (task 437). One that names its provider goes
/// like the goal review (ADR-t1063-1 decisions 4 and 5): to its provider
/// when it can be used, else to the other provider when that one can and
/// `[provider_fallback] jobs` is on (`fallback`, ADR-t1857-1), else it
/// waits, or, under `--no-claude`, goes to the person.
pub(super) fn review_route(
    no_claude: bool,
    (switchable, fallback): (bool, bool),
    launch: ActorLaunch,
    queue_hold: Option<QueueHold>,
    unusable: impl Fn(Provider) -> Option<SwitchReason>,
) -> ReviewRoute {
    if !switchable {
        if no_claude {
            return ReviewRoute::Manual(REVIEW_PROVIDER_DISABLED.to_owned());
        }
        return match queue_hold {
            Some(hold) => ReviewRoute::Wait(format!(
                "ask {} ({}) holds the headless jobs",
                hold.ask_id,
                hold.reason.as_str()
            )),
            None => ReviewRoute::Start(launch, false),
        };
    }
    match job_route(&launch, true, fallback, &unusable) {
        JobRoute::Start(launch) => ReviewRoute::Start(launch, true),
        JobRoute::Wait { .. } if no_claude => {
            let codex = unusable(Provider::Codex).map_or("unknown", |reason| reason.as_str());
            ReviewRoute::Manual(format!(
                "{REVIEW_PROVIDER_DISABLED} and codex cannot be used ({codex})"
            ))
        }
        JobRoute::Wait { provider, reason } => {
            ReviewRoute::Wait(job_wait_text(provider, reason, fallback))
        }
    }
}

/// Whether a review whose attempt on `provider` could not run moves on
/// `route`: to the other provider, or to wait for one (with
/// `[provider_fallback] jobs` off the route waits for `provider` instead of
/// choosing the other, ADR-t1857-1); not when the route still sends it to
/// `provider` (its hold was not written) or to the person, so that it
/// fails to the person instead.
pub(super) fn review_moves(route: &ReviewRoute, provider: Provider) -> bool {
    match route {
        ReviewRoute::Start(next, _) => next.provider != provider,
        ReviewRoute::Wait(_) => true,
        ReviewRoute::Manual(_) => false,
    }
}

/// How the supervisor holds the provider the finish of a job on its timer
/// (a throughput review, an observation) says could not be used
/// (`provider_unusable`), so that the job waits for that provider and
/// starts again: never the other provider (ADR-t1063-1 decision 5,
/// ADR-t1857-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UnusableHold {
    /// Record the provider's [`ProviderHold`] for the reason: Codex's, and
    /// Claude's for an agent that did not start.
    Provider(Provider, SwitchReason),
    /// Claude's login or usage limit: the queue's hold ask, as for any
    /// other Claude job (task 438).
    Ask(Wall),
    /// Claude's agent did not start while the hold ask already holds
    /// Claude: nothing more is written.
    Held,
}

/// How `provider`, which a job on its timer could not use for `reason`, is
/// held, given whether the queue's hold ask holds Claude now
/// (`claude_asked`); `None` for a reason that holds no provider.
pub(super) fn unusable_hold(
    provider: Provider,
    reason: SwitchReason,
    claude_asked: bool,
) -> Option<UnusableHold> {
    if !reason.unusable() {
        return None;
    }
    Some(match (provider, reason) {
        (Provider::Codex, _) => UnusableHold::Provider(provider, reason),
        (Provider::Claude, SwitchReason::Authentication) => UnusableHold::Ask(Wall::Authentication),
        (Provider::Claude, SwitchReason::UsageLimit) => UnusableHold::Ask(Wall::UsageLimit),
        (Provider::Claude, _) if claude_asked => UnusableHold::Held,
        (Provider::Claude, _) => UnusableHold::Provider(provider, reason),
    })
}

impl Supervisor<'_> {
    /// Hold the providers the finishes of jobs on 観測と分析's timer say
    /// could not be used (`unusable`, which that context read off its
    /// finish events into typed values and published), each finish once
    /// ([`finishes_to_hold`], so no second `provider_held`): the jobs whose
    /// provider is held now, which 観測と分析 makes due again. Called in the
    /// pass that reaped them, before either job's next start reads the
    /// holds (ADR-t1545-1 decision 2).
    pub(super) fn hold_timer_jobs_unusable(
        &mut self,
        unusable: Vec<observer::UnusableTimerJob>,
    ) -> Vec<observer::TimerJob> {
        let mut held = Vec::new();
        for job in finishes_to_hold(unusable, &self.provider.timer_finishes_held, |job| {
            job.finish.event
        }) {
            self.provider.timer_finishes_held.push(job.finish.event);
            let over = self
                .provider
                .timer_finishes_held
                .len()
                .saturating_sub(HELD_FINISHES);
            self.provider.timer_finishes_held.drain(..over);
            let finish = &job.finish;
            if self
                .hold_unusable(
                    (finish.provider, finish.reason),
                    (&finish.error, &job.output),
                    &job.hold,
                    &job.what,
                )
                .is_some()
            {
                held.push(job.job);
            }
        }
        held
    }

    /// Hold `provider`, which a job on its timer (`job`, `what` in the
    /// log) could not use for `reason`, as [`unusable_hold`] says: `error`
    /// is the job's error and `output` its provider's words, which may say
    /// when a usage limit resets. The provider held, when it is held now (a
    /// hold that cannot be written is logged, and the job is not started
    /// again at once).
    fn hold_unusable(
        &mut self,
        (provider, reason): (Provider, SwitchReason),
        (error, output): (&str, &str),
        job: &HoldJob,
        what: &str,
    ) -> Option<Provider> {
        let hold = unusable_hold(provider, reason, self.queue_hold.is_some())?;
        let held = match hold {
            UnusableHold::Provider(provider, reason) => {
                let said = format!("{error}\n{output}");
                match self.hold_provider(provider, reason, None, &said) {
                    Ok(()) => true,
                    Err(held) => {
                        warn!(error = %format_args!("{held:#}"), "{} could not be held after the {what} failed: {held:#}", provider.as_str());
                        false
                    }
                }
            }
            UnusableHold::Ask(wall) => self.raise_job_wall(wall, job, error),
            UnusableHold::Held => true,
        };
        if !held {
            return None;
        }
        if self.provider.fallback.jobs {
            info!(
                "{what}: {} cannot be used ({}); it starts again on the other provider",
                provider.as_str(),
                reason.as_str()
            );
        } else {
            info!(
                "{what}: {} cannot be used ({}); [provider_fallback] jobs is false, so it starts again on {} once its hold ends",
                provider.as_str(),
                reason.as_str(),
                provider.as_str()
            );
        }
        Some(provider)
    }
}

/// What a request that moves a run to the other provider is called
/// (`turn_requested`'s `what`).
pub(super) const PROVIDER_SWITCH: &str = "provider switch";

/// What a request that makes a failed call again on the same provider,
/// once its hold ended, is called.
pub(super) use crate::domain::turn::PROVIDER_RETRY;

/// How many of the latest `provider_held` and `provider_released` events
/// are read for the holds in place.
const HOLD_EVENTS_READ: usize = 20;

/// The request that makes a failed call again on `provider` once its hold
/// ended: the call it failed at (`undelivered`, taken from `dir`'s
/// `turns/`), or a request to go on. Held to [`PROVIDER_TURN_LIMIT`]
/// ([`undelivered_part`]); the fixed text is never cut.
pub(super) fn retry_text(
    provider: Provider,
    reason: SwitchReason,
    undelivered: Option<&TurnRequest>,
    dir: Option<&Path>,
) -> FittedPrompt {
    let mut fit = Fit::new(PROVIDER_TURN_LIMIT);
    let head = format!(
        "dagq: {} can be used again after the {} that stopped your previous turn. Go on with the task in this turn.",
        provider.as_str(),
        reason.as_str()
    );
    let text = match undelivered {
        Some(request) => {
            let (what, prompt) = undelivered_part(&mut fit, request, dir);
            format!(
                "{head} Your previous turn was asked this ({what}) and did not get to it:\n\n{prompt}"
            )
        }
        None => head,
    };
    fit.finish(text)
}

/// The name and the text of the request a failed turn did not get to,
/// which a retry or switch request carries (ADR-t2072-1): whatever it was
/// (a first request, a retry or switch request that failed in its turn
/// too, one written before the limits), its text is one section cut to
/// [`UNDELIVERED_REQUEST_BYTES`] keeping its start, so a request wrapped
/// again and again stays within the whole limit; its name is cut to
/// [`NEXT_TURN_NAME_BYTES`]. What is cut is in the file the wrapper moved
/// it to in `dir`'s `turns/`, in no file without `dir`.
fn undelivered_part(
    fit: &mut Fit,
    undelivered: &TurnRequest,
    dir: Option<&Path>,
) -> (String, String) {
    let read = dir.map_or_else(
        || NOT_READABLE.to_owned(),
        |dir| {
            format!(
                "the whole request is in {}",
                taken_path(dir, undelivered.seq).display()
            )
        },
    );
    let what = fit.text(
        "what",
        &undelivered.what,
        NEXT_TURN_NAME_BYTES,
        Keep::Start,
        &read,
    );
    fit.section("what", &what);
    let prompt = fit.text(
        "undelivered",
        &undelivered.prompt,
        UNDELIVERED_REQUEST_BYTES,
        Keep::Start,
        &read,
    );
    fit.section("undelivered", &prompt);
    (what, prompt)
}

/// Why a run did not move to the other provider: `[provider_fallback]
/// workers` is off (`fallback` false, ADR-t1857-1), its switches are used
/// up, or that provider is held (`held`) or has no headless worker here.
fn switch_blocked(fallback: bool, events: &[RunEvent], held: Option<SwitchReason>) -> String {
    if !fallback {
        "[provider_fallback] workers is false: the run waits for its own provider".to_owned()
    } else if !provider_switch::may_switch(events) {
        format!("its {MAX_PROVIDER_SWITCHES} switches are used up")
    } else if let Some(reason) = held {
        format!("the other provider is held ({})", reason.as_str())
    } else {
        "this supervisor has no headless worker of the other provider".to_owned()
    }
}

/// The first prompt of the new session a switch starts (the wrapper puts
/// the task's prompt before it, as for any new session): why the worker
/// moved, that nothing of the conversation carries over, where the work
/// so far is, and the call the failed turn made (`undelivered`). Held to
/// [`PROVIDER_TURN_LIMIT`]: the provider's `message` to
/// [`PROVIDER_MESSAGE_BYTES`] and the call as [`undelivered_part`] holds
/// it; the fixed text is never cut.
pub(super) fn switch_text(
    run: &TaskRun,
    from: Provider,
    to: Provider,
    reason: SwitchReason,
    message: &str,
    undelivered: Option<&TurnRequest>,
) -> FittedPrompt {
    let mut fit = Fit::new(PROVIDER_TURN_LIMIT);
    let message = fit.text(
        "message",
        message,
        PROVIDER_MESSAGE_BYTES,
        Keep::Start,
        NOT_READABLE,
    );
    fit.section("message", &message);
    let mut text = format!(
        "dagq: this run's worker moved from {} to {} ({}: {message}). This is a new session: nothing of the earlier conversation carries over. The work so far is in this worktree and its branch: run `git log --oneline {}..HEAD` and `git status` to see the commits and the uncommitted changes, and go on from them rather than starting over.",
        from.as_str(),
        to.as_str(),
        reason.as_str(),
        run.base_commit(),
    );
    if let Some(request) = undelivered {
        let (what, prompt) = undelivered_part(&mut fit, request, run.run_dir().map(Path::new));
        text.push_str(&format!(
            " The earlier session was asked this ({what}) and did not get to it:\n\n{prompt}"
        ));
    }
    fit.finish(text)
}

/// What a revise's or a resume's watch does about a headless turn at its
/// provider's wall ([`SessionWatch::provider_wall`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WallGate {
    /// No such turn since the last input: the watch judges as before.
    Open,
    /// The run waits for a provider: not idle without its receipt, nor out
    /// of time.
    Held,
    /// The call went to the other provider at this time, the watch's last
    /// input now.
    Moved(SystemTime),
}

impl Supervisor<'_> {
    /// Where the due job of `role` (the throughput review, the observer)
    /// goes, or `None` while it waits ([`job_start_route`] with
    /// `[provider_fallback] jobs`): a role that names no provider does not
    /// start under `--no-claude` (ADR-t1204-1 decision 2).
    pub(super) fn job_start_route(&self, role: ModelRole) -> Option<JobStartRoute> {
        self.start_route(role, UnnamedWithoutClaude::Wait)
    }

    /// Where the due job of `role` goes, from `[roles.<role>]` as it reads
    /// now, the queue's hold ask, `[provider_fallback] jobs` and why each
    /// provider cannot be used ([`job_start_route`]), or `None` while it
    /// waits, saying why in the debug log.
    pub(super) fn start_route(
        &self,
        role: ModelRole,
        unnamed: UnnamedWithoutClaude,
    ) -> Option<JobStartRoute> {
        let models = self.role_models(role);
        job_start_route(
            models.launch(role),
            models.switchable(role),
            self.no_claude,
            self.queue_hold.is_some(),
            self.provider.fallback.jobs,
            unnamed,
            |provider| self.job_unusable(provider),
        )
        .inspect_err(|why| {
            if let Some(why) = why {
                tracing::debug!("the {} job waits: {why}", role.as_str());
            }
        })
        .ok()
    }
}

impl SessionWatch {
    /// The last turn of this headless session, newer than the last input,
    /// failed because its provider cannot be used (ADR-t813-2): its call
    /// goes to the other provider ([`Supervisor::turn_at_wall`]), which is
    /// the watch's last input from then, or the run waits in the hold ask.
    pub(super) fn provider_wall(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<WallGate> {
        let Ok(modified) = sv.files.modified(&self.idle_marker) else {
            return Ok(WallGate::Open);
        };
        if self.input_at.is_some_and(|input| modified <= input) {
            return Ok(WallGate::Open);
        }
        let Some(failure) = provider_failure(last_turn(sv, &self.idle_marker)) else {
            return Ok(WallGate::Open);
        };
        let workspace = self.workspace.clone();
        Ok(match sv.turn_at_wall(run, &workspace, failure)? {
            WallStep::Switched(at) => {
                // Never older than the marker it answers.
                let at = at.max(modified);
                self.input_at = Some(at);
                WallGate::Moved(at)
            }
            WallStep::Held => WallGate::Held,
        })
    }
}

/// Hold `run` at `wall` in the queue's one `queue_hold` ask of that wall
/// (task 438), opening it or joining it; a run that newly joins records the
/// wall's event with `screen` (the failed turn's text) as its excerpt.
/// `false` when the same excerpt was already held by an ask now closed: the
/// wall a person cleared is not raised again for it.
pub(super) fn raise_wall(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    screen: &str,
    wall: Wall,
) -> Result<bool> {
    let excerpt = screen.to_owned();
    let hash = format!("{:x}", Sha256::digest(excerpt.as_bytes()));
    let last = sv
        .queue
        .run_events(run.id())?
        .into_iter()
        .rev()
        .find(|e| e.kind == wall.event_kind() && e.payload.get("job").is_none());
    if let Some(last) = last
        && last.payload.get("screen_hash").and_then(Value::as_str) == Some(hash.as_str())
        && let Some(id) = last.payload.get("ask_id").and_then(Value::as_i64)
        && !sv.queue.read_ask(AskId::new(id))?.is_open()
    {
        return Ok(false);
    }
    let (outcome, _) = ask::hold(
        &mut *sv.queue,
        NewHold::wall(wall, Some(run.id().clone()), None),
    )?;
    if outcome.joined {
        sv.queue.record_runtime_event(
            run.id(),
            wall.event_kind(),
            json!({
                "workspace_id": workspace,
                "excerpt": excerpt,
                "screen_hash": hash,
                "ask_id": outcome.ask.id,
            }),
        )?;
        warn!(ask_id = %outcome.ask.id, run_id = %run.id(), "run {} stopped at the {} wall in workspace {workspace}; ask {} holds {} run(s) and job(s)", run.id(), wall.as_str(), outcome.ask.id, outcome.ask.affected.len());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, actor_model::ModelRole, claim_hold::HoldReason};

    /// The review's launch when `[roles.review]` names `provider`.
    fn launch(provider: Provider) -> ActorLaunch {
        ActorLaunch {
            provider,
            ..ActorLaunch::default_of(ModelRole::Review)
        }
    }

    fn unusable(
        claude: Option<SwitchReason>,
        codex: Option<SwitchReason>,
    ) -> impl Fn(Provider) -> Option<SwitchReason> {
        move |provider| match provider {
            Provider::Claude => claude,
            Provider::Codex => codex,
        }
    }

    const HOLD: QueueHold = QueueHold {
        reason: HoldReason::Authentication,
        ask_id: 7,
        affected: 0,
    };

    // Moved here by task 1713 from the tests/it case it removed:
    // runtime_codex::no_claude_review_with_no_provider_asks_without_a_recommendation.
    #[test]
    fn a_review_whose_role_names_no_provider_goes_to_claude_a_person_or_waits() {
        let route = review_route(
            false,
            (false, true),
            launch(Provider::Claude),
            None,
            unusable(None, None),
        );
        assert!(
            matches!(&route, ReviewRoute::Start(l, false) if l.provider == Provider::Claude),
            "{}",
            describe(&route)
        );
        // Under `--no-claude` the person reviews it: no review agent ran.
        let route = review_route(
            true,
            (false, true),
            launch(Provider::Claude),
            None,
            unusable(None, None),
        );
        assert!(
            matches!(&route, ReviewRoute::Manual(why) if why == REVIEW_PROVIDER_DISABLED),
            "{}",
            describe(&route)
        );
        // Claude's hold ask holds it (task 437).
        let route = review_route(
            false,
            (false, true),
            launch(Provider::Claude),
            Some(HOLD),
            unusable(Some(SwitchReason::Authentication), None),
        );
        assert!(
            matches!(&route, ReviewRoute::Wait(why) if why == "ask 7 (authentication) holds the headless jobs"),
            "{}",
            describe(&route)
        );
    }

    // Moved here by task 1713 from the tests/it cases it removed:
    // runtime_codex::a_codex_review_starts_while_claudes_hold_ask_is_open,
    // a_codex_review_without_codex_moves_to_claude_and_lands and
    // no_claude_codex_review_at_its_usage_limit_holds_codex_and_asks_a_person.
    // The kept runtime_codex cases check the wiring:
    // a_codex_review_at_its_usage_limit_holds_codex_and_moves_to_claude and
    // no_claude_unreadable_codex_review_at_its_limit_fails_with_its_output.
    #[test]
    fn a_review_on_codex_moves_to_claude_waits_or_goes_to_a_person() {
        // Claude's hold ask does not hold a review on Codex.
        let claude_held = Some(SwitchReason::Authentication);
        let route = review_route(
            false,
            (true, true),
            launch(Provider::Codex),
            Some(HOLD),
            unusable(claude_held, None),
        );
        assert!(
            matches!(&route, ReviewRoute::Start(l, true) if l.provider == Provider::Codex && l.switched_from.is_none()),
            "{}",
            describe(&route)
        );
        // No Codex here, or Codex held: on Claude, saying from which and why.
        for reason in [SwitchReason::ExecutableMissing, SwitchReason::UsageLimit] {
            let route = review_route(
                false,
                (true, true),
                launch(Provider::Codex),
                None,
                unusable(None, Some(reason)),
            );
            let ReviewRoute::Start(moved, true) = &route else {
                panic!("{}", describe(&route));
            };
            assert_eq!(moved.provider, Provider::Claude);
            assert_eq!(moved.switched_from, Some(Provider::Codex));
            assert_eq!(moved.switch_reason, Some(reason));
            assert!(review_moves(&route, Provider::Codex));
        }
        // Neither provider: it waits with the session open.
        let route = review_route(
            false,
            (true, true),
            launch(Provider::Codex),
            Some(HOLD),
            unusable(claude_held, Some(SwitchReason::UsageLimit)),
        );
        assert!(
            matches!(&route, ReviewRoute::Wait(why) if why == "codex cannot be used (usage_limit), nor can the other provider"),
            "{}",
            describe(&route)
        );
        assert!(review_moves(&route, Provider::Codex));
        // Under `--no-claude` with Codex held: the person reviews it, and
        // the review that could not run does not move.
        let route = review_route(
            true,
            (true, true),
            launch(Provider::Codex),
            None,
            unusable(Some(SwitchReason::Disabled), Some(SwitchReason::UsageLimit)),
        );
        assert!(
            matches!(&route, ReviewRoute::Manual(why) if why == "provider_disabled: Claude is disabled by --no-claude and codex cannot be used (usage_limit)"),
            "{}",
            describe(&route)
        );
        assert!(!review_moves(&route, Provider::Codex));
        // A route that still sends it to the provider that failed (its hold
        // was not written) does not move it either.
        let same = review_route(
            false,
            (true, true),
            launch(Provider::Codex),
            None,
            unusable(None, None),
        );
        assert!(!review_moves(&same, Provider::Codex));
        assert!(review_moves(&same, Provider::Claude));
    }

    /// With `[provider_fallback] jobs` off a review whose role names its
    /// provider waits for that provider when it cannot be used: the review
    /// that could not run moves to the wait, not to the other provider, and
    /// starts on its own provider once it can be used. `--no-claude` still
    /// sends a Claude one to Codex, and with no provider left goes to the
    /// person as before (ADR-t1857-1).
    #[test]
    fn with_the_fallback_off_a_review_waits_for_its_own_provider() {
        for provider in [Provider::Claude, Provider::Codex] {
            for reason in [
                SwitchReason::ExecutableMissing,
                SwitchReason::LaunchFailed,
                SwitchReason::Authentication,
                SwitchReason::UsageLimit,
            ] {
                let own = |p: Provider| (p == provider).then_some(reason);
                let route = review_route(false, (true, false), launch(provider), None, own);
                assert!(
                    matches!(&route, ReviewRoute::Wait(why) if why == &format!("{p} cannot be used ({r}); [provider_fallback] jobs is false, so it waits for {p}", p = provider.as_str(), r = reason.as_str())),
                    "{}",
                    describe(&route)
                );
                // The review moves to the wait (no `review_failed`)...
                assert!(review_moves(&route, provider));
                // ...where on it would have moved to the other provider.
                let on = review_route(false, (true, true), launch(provider), None, own);
                assert!(
                    matches!(&on, ReviewRoute::Start(l, true) if l.provider == provider.other()),
                    "{}",
                    describe(&on)
                );
            }
            // Its hold ended: on its own provider.
            let route = review_route(
                false,
                (true, false),
                launch(provider),
                None,
                unusable(None, None),
            );
            assert!(
                matches!(&route, ReviewRoute::Start(l, true) if l.provider == provider && l.switched_from.is_none()),
                "{}",
                describe(&route)
            );
        }
        // `--no-claude`: a Claude role still goes to Codex...
        let route = review_route(
            true,
            (true, false),
            launch(Provider::Claude),
            None,
            unusable(Some(SwitchReason::Disabled), None),
        );
        assert!(
            matches!(&route, ReviewRoute::Start(l, true) if l.provider == Provider::Codex && l.switch_reason == Some(SwitchReason::Disabled)),
            "{}",
            describe(&route)
        );
        // ...and, Codex held, to the person as before.
        let route = review_route(
            true,
            (true, false),
            launch(Provider::Codex),
            None,
            unusable(Some(SwitchReason::Disabled), Some(SwitchReason::UsageLimit)),
        );
        assert!(
            matches!(&route, ReviewRoute::Manual(why) if why == "provider_disabled: Claude is disabled by --no-claude and codex cannot be used (usage_limit)"),
            "{}",
            describe(&route)
        );
        assert!(!review_moves(&route, Provider::Codex));
    }

    /// The provider a timer job's finish says could not be used is held
    /// the way any other job's is: Codex by its `ProviderHold`, Claude's
    /// login or usage limit by the hold ask, and Claude's agent that did
    /// not start by its `ProviderHold` unless the hold ask holds Claude;
    /// the other provider is never held (ADR-t1857-1).
    #[test]
    fn a_timer_job_that_could_not_use_its_provider_holds_that_provider() {
        use crate::domain::queue_hold::Wall;
        for reason in [
            SwitchReason::ExecutableMissing,
            SwitchReason::LaunchFailed,
            SwitchReason::Authentication,
            SwitchReason::UsageLimit,
        ] {
            for asked in [false, true] {
                assert_eq!(
                    unusable_hold(Provider::Codex, reason, asked),
                    Some(UnusableHold::Provider(Provider::Codex, reason))
                );
            }
        }
        for asked in [false, true] {
            assert_eq!(
                unusable_hold(Provider::Claude, SwitchReason::UsageLimit, asked),
                Some(UnusableHold::Ask(Wall::UsageLimit))
            );
            assert_eq!(
                unusable_hold(Provider::Claude, SwitchReason::Authentication, asked),
                Some(UnusableHold::Ask(Wall::Authentication))
            );
        }
        for reason in [SwitchReason::LaunchFailed, SwitchReason::ExecutableMissing] {
            assert_eq!(
                unusable_hold(Provider::Claude, reason, false),
                Some(UnusableHold::Provider(Provider::Claude, reason))
            );
            assert_eq!(
                unusable_hold(Provider::Claude, reason, true),
                Some(UnusableHold::Held)
            );
        }
        for reason in [SwitchReason::Disabled, SwitchReason::SubagentsUnsupported] {
            assert_eq!(unusable_hold(Provider::Claude, reason, false), None);
        }
    }

    fn describe(route: &ReviewRoute) -> String {
        match route {
            ReviewRoute::Start(launch, switchable) => {
                format!("start {} {switchable}", launch.to_value())
            }
            ReviewRoute::Wait(why) => format!("wait {why}"),
            ReviewRoute::Manual(why) => format!("manual {why}"),
        }
    }

    fn switched() -> RunEvent {
        RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: event_kind::PROVIDER_SWITCHED.to_owned(),
            payload: json!({}),
            created_at: String::new(),
            actor: None,
        }
    }

    // Moved here by task 1713 from the tests/it case it removed:
    // runtime_provider_switch::a_run_out_of_switches_waits_on_codexs_hold_and_retries_at_its_reset.
    // The kept runtime_provider_switch::a_codex_that_does_not_start_while_claude_is_held_waits_in_the_hold_ask
    // checks the wiring.
    #[test]
    fn a_waiting_run_says_why_it_did_not_move() {
        assert_eq!(
            switch_blocked(true, &[switched(), switched()], None),
            "its 2 switches are used up"
        );
        assert_eq!(
            switch_blocked(true, &[switched()], Some(SwitchReason::UsageLimit)),
            "the other provider is held (usage_limit)"
        );
        assert_eq!(
            switch_blocked(true, &[], Some(SwitchReason::Disabled)),
            "the other provider is held (provider_disabled)"
        );
        assert_eq!(
            switch_blocked(true, &[], None),
            "this supervisor has no headless worker of the other provider"
        );
        // The fallback off says so before anything else (ADR-t1857-1).
        assert_eq!(
            switch_blocked(false, &[], None),
            "[provider_fallback] workers is false: the run waits for its own provider"
        );
    }

    // Moved here by task 1713 from the tests/it case it removed:
    // runtime_provider_switch::a_run_out_of_switches_waits_on_codexs_hold_and_retries_at_its_reset.
    // The kept runtime_provider_switch::no_claude_waits_on_codex_limit_then_retries_codex_without_a_claude_hold
    // checks the wiring.
    #[test]
    fn a_retry_makes_the_call_again_or_asks_to_go_on() {
        let head = "dagq: codex can be used again after the usage_limit that stopped your previous turn. Go on with the task in this turn.";
        assert_eq!(
            retry_text(Provider::Codex, SwitchReason::UsageLimit, None, None).text,
            head
        );
        let request = TurnRequest {
            seq: 2,
            what: "revise request".to_owned(),
            prompt: "fix it".to_owned(),
        };
        let fitted = retry_text(
            Provider::Codex,
            SwitchReason::UsageLimit,
            Some(&request),
            Some(Path::new("/runs/run")),
        );
        assert_eq!(
            fitted.text,
            format!(
                "{head} Your previous turn was asked this (revise request) and did not get to it:\n\nfix it"
            )
        );
        // Within its limits it carries the call whole and records no cut.
        assert_eq!(fitted.bytes.limit, PROVIDER_TURN_LIMIT);
        assert_eq!(fitted.bytes.total, fitted.text.len());
        assert_eq!(fitted.bytes.sections["undelivered"], "fix it".len());
        assert!(fitted.bytes.omitted.is_empty(), "{:?}", fitted.bytes);
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn run() -> TaskRun {
        TaskRun::restore(crate::domain::RunRecord {
            id: RunId::new("run-1").unwrap(),
            task_id: TaskId::new(7),
            status: RunStatus::Running,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            worker_mode: WorkerMode::Headless,
            base_commit: CommitSha::try_from(SHA).unwrap(),
            branch: Some("dagq/run-1".into()),
            worktree_path: Some("/runs/run/worktree".into()),
            workspace_id: None,
            receipt_path: Some("/runs/run/receipt.json".into()),
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: Some("/runs/run".into()),
            last_error: None,
            workspace_closed_at: None,
            created_at: String::new(),
        })
        .unwrap()
    }

    /// The fixed text of a switch request, which no limit cuts.
    fn switch_steps() -> Vec<String> {
        vec![
            "This is a new session: nothing of the earlier conversation carries over.".to_owned(),
            format!(
                "run `git log --oneline {SHA}..HEAD` and `git status` to see the commits and the uncommitted changes"
            ),
        ]
    }

    /// ADR-t2072-1: with the largest input (a huge provider message, and a
    /// huge call the failed turn did not get to, as one written before the
    /// limits) a retry and a switch request stay within
    /// `PROVIDER_TURN_LIMIT` without their middle cut, keep their fixed
    /// text, and say how many bytes they cut and that the call is whole in
    /// its taken file under `turns/`.
    #[test]
    fn a_retry_and_a_switch_stay_within_their_limit_with_the_largest_input() {
        let run = run();
        let huge = "依".repeat(400_000);
        let request = TurnRequest {
            seq: 9,
            what: "w".repeat(10_000),
            prompt: huge.clone(),
        };
        let taken = "the whole request is in /runs/run/turns/request-000009.taken.json]";
        let retry = retry_text(
            Provider::Codex,
            SwitchReason::UsageLimit,
            Some(&request),
            run.run_dir().map(Path::new),
        );
        let switch = switch_text(
            &run,
            Provider::Claude,
            Provider::Codex,
            SwitchReason::Authentication,
            &huge,
            Some(&request),
        );
        for (fitted, cut, kept) in [
            (
                &retry,
                vec!["undelivered", "what"],
                vec!["dagq: codex can be used again after the usage_limit".to_owned()],
            ),
            (
                &switch,
                vec!["message", "undelivered", "what"],
                switch_steps(),
            ),
        ] {
            let text = &fitted.text;
            assert!(
                text.len() <= PROVIDER_TURN_LIMIT - crate::application::prompt_fit::LANGUAGE_ROOM,
                "{}",
                text.len()
            );
            assert_eq!(fitted.bytes.total, text.len());
            assert_eq!(fitted.bytes.over_limit, None);
            for section in &cut {
                assert_eq!(fitted.bytes.omitted[section], 1, "{section}");
            }
            assert_eq!(fitted.bytes.omitted.len(), cut.len());
            for kept in kept {
                assert!(text.contains(&kept), "{kept}");
            }
            let (_, call) = text.split_once("did not get to it:\n\n").unwrap();
            let (kept, note) = call.split_once("\n[… ").unwrap();
            assert!(kept.len() <= UNDELIVERED_REQUEST_BYTES && huge.starts_with(kept));
            assert_eq!(
                note,
                format!(
                    "{} bytes left out by the prompt's limit; {taken}",
                    huge.len() - kept.len()
                )
            );
        }
        assert!(
            switch
                .text
                .contains(&format!("by the prompt's limit; {NOT_READABLE}]")),
            "the message is in no file"
        );
    }

    /// ADR-t2072-1: a retry or switch request that failed in its turn too
    /// is the next one's call, wrapped again with its fixed text; however
    /// often that happens, each request stays within `PROVIDER_TURN_LIMIT`
    /// without its middle cut and keeps its own fixed text.
    #[test]
    fn retries_and_switches_wrapped_again_and_again_stay_within_their_limit() {
        let run = run();
        let mut call = TurnRequest {
            seq: 1,
            what: "resolution request".to_owned(),
            prompt: "理".repeat(100_000),
        };
        for round in 0..8_u64 {
            let (fitted, what) = if round % 2 == 0 {
                let fitted = retry_text(
                    Provider::Codex,
                    SwitchReason::UsageLimit,
                    Some(&call),
                    run.run_dir().map(Path::new),
                );
                (fitted, PROVIDER_RETRY)
            } else {
                let fitted = switch_text(
                    &run,
                    Provider::Codex,
                    Provider::Claude,
                    SwitchReason::UsageLimit,
                    "usage limit reached",
                    Some(&call),
                );
                assert!(fitted.text.starts_with("dagq: this run's worker moved"));
                for kept in switch_steps() {
                    assert!(fitted.text.contains(&kept), "{round}: {kept}");
                }
                (fitted, PROVIDER_SWITCH)
            };
            let text = &fitted.text;
            assert!(
                text.len() <= PROVIDER_TURN_LIMIT - crate::application::prompt_fit::LANGUAGE_ROOM,
                "{round}: {}",
                text.len()
            );
            assert_eq!(fitted.bytes.over_limit, None, "{round}");
            assert_eq!(fitted.bytes.omitted["undelivered"], 1, "{round}");
            assert!(text.contains(&format!(
                "the whole request is in /runs/run/turns/request-{:06}.taken.json]",
                call.seq
            )));
            call = TurnRequest {
                seq: call.seq + 1,
                what: what.to_owned(),
                prompt: fitted.text,
            };
        }
    }
}
