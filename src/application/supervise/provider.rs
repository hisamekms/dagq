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
//! provider, never failed ([`Supervisor::turn_at_wall`]); an interactive
//! Claude session stopped at such a wall is parked and resumed as a
//! headless Codex one ([`interactive_switch`]).

use super::file_time::recorded_at;
use super::*;
use crate::domain::{
    provider_switch::{
        self, MAX_PROVIDER_SWITCHES, ProviderHold, SwitchPhase, SwitchReason, WorkerRoute,
        switched_payload,
    },
    turn::{TurnFailure, TurnRequest, taken_path},
    worker::WorkerMode,
};

impl Supervisor<'_> {
    /// Why `provider` is held for the workers now, if it is: Claude by the
    /// queue's open hold ask or its [`ProviderHold`] (an agent that did not
    /// start), Codex by its [`ProviderHold`].
    pub(super) fn provider_held(&self, provider: Provider) -> Option<SwitchReason> {
        if self.no_claude && provider == Provider::Claude {
            return Some(SwitchReason::Disabled);
        }
        let own = self
            .provider_holds
            .iter()
            .find(|hold| hold.provider == provider)
            .map(|hold| hold.reason);
        match provider {
            Provider::Claude => self
                .queue_hold
                .and_then(|hold| SwitchReason::of_hold(hold.reason))
                .or(own),
            Provider::Codex => own,
        }
    }

    /// How this pass's claims run each worker a task may ask for.
    pub(super) fn routes(&self) -> Vec<WorkerRoute> {
        provider_switch::routes(&self.workers, |provider| self.provider_held(provider))
    }

    /// Whether a headless worker of `provider` can take a run now: this
    /// supervisor has its adapters and it is not held.
    fn headless_usable(&self, provider: Provider) -> bool {
        self.workers.contains(&Worker {
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
        self.provider_holds = [Provider::Claude, Provider::Codex]
            .into_iter()
            .filter_map(|provider| {
                latest
                    .iter()
                    .find(|e| e.payload["provider"] == provider.as_str())
                    .and_then(|e| ProviderHold::in_place(&e.kind, &e.payload))
            })
            .collect();
        let now = self.generators.clock.now();
        for hold in self.provider_holds.clone() {
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
        if self.provider_holds.iter().any(|h| h.provider == provider) {
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
        self.provider_holds.push(hold);
        Ok(())
    }

    /// End `provider`'s hold (`why`: `retry_due`, or `done` when a person
    /// answered a hold ask).
    fn release_provider(&mut self, provider: Provider, why: &str) -> Result<()> {
        let Some(at) = self
            .provider_holds
            .iter()
            .position(|hold| hold.provider == provider)
        else {
            return Ok(());
        };
        let hold = self.provider_holds.remove(at);
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
        let claude_wall = from == Provider::Claude && reason != SwitchReason::LaunchFailed;
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
        if provider_switch::may_switch(&events) && self.headless_usable(to) {
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
        if !first_look
            && self.provider_held(from).is_none()
            && !self.queue.hold_unclosed(run.id())?
        {
            let text = retry_text(from, reason, undelivered.as_ref());
            request_turn(self, run, workspace, Input::Text(&text), PROVIDER_RETRY)?;
            info!(run_id = %run.id(), "run {}: {} can be used again; the call of turn {turn} is made again", run.id(), from.as_str());
            return Ok(WallStep::Switched(self.files.now()));
        }
        if first_look {
            let blocked = switch_blocked(&events, self.provider_held(to));
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
                        .provider_holds
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
            // The same hold, event and question as an interactive session's
            // screen at the wall (task 438).
            raise_wall(self, run, workspace, &text, wall)?;
        }
        Ok(WallStep::Held)
    }

    /// The hold ask a run that waits for a provider joins, if any: Claude's
    /// login or usage limit always opens it; otherwise only when the other
    /// provider cannot be used either, with the wall of the hold ask open
    /// already, or of whichever provider's hold is a login or a usage
    /// limit. Two agents that do not start open none: the run waits on
    /// their holds.
    /// The hold ask is read afresh: one a person answered since the top of
    /// this pass no longer holds Claude.
    fn wall_to_raise(
        &self,
        from: Provider,
        reason: SwitchReason,
        to: Provider,
    ) -> Result<Option<Wall>> {
        if self.no_claude {
            // Codex retains its own real hold; do not describe it as a Claude queue hold.
            return Ok(None);
        }
        let wall_of = |reason: SwitchReason| match reason {
            SwitchReason::UsageLimit => Some(Wall::UsageLimit),
            SwitchReason::Authentication => Some(Wall::Authentication),
            SwitchReason::Disabled
            | SwitchReason::LaunchFailed
            | SwitchReason::ExecutableMissing
            | SwitchReason::SubagentsUnsupported => None,
        };
        if from == Provider::Claude && reason != SwitchReason::LaunchFailed {
            return Ok(wall_of(reason));
        }
        let open_hold = self
            .queue
            .asks(AskQuery::default())?
            .iter()
            .find_map(crate::domain::queue_hold::hold_of);
        let own_hold = |provider: Provider| {
            self.provider_holds
                .iter()
                .find(|hold| hold.provider == provider)
                .map(|hold| hold.reason)
        };
        let to_held = match to {
            Provider::Claude => open_hold
                .and_then(|hold| SwitchReason::of_hold(hold.reason))
                .or(own_hold(to)),
            Provider::Codex => own_hold(to),
        };
        let other_usable = self.workers.contains(&Worker {
            provider: to,
            mode: WorkerMode::Headless,
        }) && to_held.is_none();
        if other_usable {
            return Ok(None);
        }
        if let Some(hold) = open_hold {
            return Ok(SwitchReason::of_hold(hold.reason).and_then(wall_of));
        }
        Ok(wall_of(reason).or_else(|| to_held.and_then(wall_of)))
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
        text: &str,
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
        self.moved.insert(run.id().clone(), worker);
        // The new provider's worker is told as it takes it (Codex does no
        // subagent review, say).
        if let Some(run_dir) = moved.run_dir() {
            let task = self.queue.show(moved.task_id())?.task;
            self.write_prompt(&task, &moved, Path::new(run_dir))?;
        }
        // A headless session takes the call as its next turn; an
        // interactive one is parked, and its resume carries it.
        if headless(run) {
            request_turn(self, &moved, workspace, Input::Text(text), PROVIDER_SWITCH)?;
        }
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
        let wall = match reason {
            SwitchReason::UsageLimit => Wall::UsageLimit,
            SwitchReason::Authentication => Wall::Authentication,
            // An agent that did not start is no wall a person moves.
            SwitchReason::Disabled
            | SwitchReason::LaunchFailed
            | SwitchReason::ExecutableMissing
            | SwitchReason::SubagentsUnsupported => return Ok(()),
        };
        let (outcome, _) = ask::hold(
            &mut *self.queue,
            &self.layout.main_checkout,
            NewHold::wall(wall, None, None),
            self.cmux,
        )?;
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
/// ended: the call it failed at (`undelivered`), or a request to go on.
pub(super) fn retry_text(
    provider: Provider,
    reason: SwitchReason,
    undelivered: Option<&TurnRequest>,
) -> String {
    let head = format!(
        "dagq: {} can be used again after the {} that stopped your previous turn. Go on with the task in this turn.",
        provider.as_str(),
        reason.as_str()
    );
    match undelivered {
        Some(request) => format!(
            "{head} Your previous turn was asked this ({}) and did not get to it:\n\n{}",
            request.what, request.prompt
        ),
        None => head,
    }
}

/// Why a run did not move to the other provider: its switches are used
/// up, or that provider is held (`held`) or has no headless worker here.
fn switch_blocked(events: &[RunEvent], held: Option<SwitchReason>) -> String {
    if !provider_switch::may_switch(events) {
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
/// so far is, and the call the failed turn made (`undelivered`).
pub(super) fn switch_text(
    run: &TaskRun,
    from: Provider,
    to: Provider,
    reason: SwitchReason,
    message: &str,
    undelivered: Option<&TurnRequest>,
) -> String {
    let mut text = format!(
        "dagq: this run's worker moved from {} to {} ({}: {message}). This is a new session: nothing of the earlier conversation carries over. The work so far is in this worktree and its branch: run `git log --oneline {}..HEAD` and `git status` to see the commits and the uncommitted changes, and go on from them rather than starting over.",
        from.as_str(),
        to.as_str(),
        reason.as_str(),
        run.base_commit(),
    );
    if let Some(request) = undelivered {
        text.push_str(&format!(
            " The earlier session was asked this ({}) and did not get to it:\n\n{}",
            request.what, request.prompt
        ));
    }
    text
}

/// An interactive session's move to headless Codex, from its wall to the
/// park of its run: what the resume says, and why.
#[derive(Debug, Clone)]
pub(super) struct PendingSwitch {
    pub(super) instruction: String,
    pub(super) reason: SwitchReason,
    pub(super) message: String,
}

/// Why a parked run moves to headless Codex when its resume begins: the
/// `provider_switch` its interactive session's park recorded
/// ([`interactive_switch`]).
pub(super) const PARK_SWITCH: &str = "provider_switch";

impl Supervisor<'_> {
    /// The resume of `run` begins (under this supervisor's lease): when its
    /// interactive session was parked to move to headless Codex
    /// ([`interactive_switch`]) and Codex can still take it, the run moves
    /// now, before the resume's session opens, and the moved run is
    /// returned. The interactive session was asked to exit as the
    /// interactive one it was.
    pub(super) fn switch_parked(&mut self, run: TaskRun) -> Result<TaskRun> {
        if headless(&run) {
            return Ok(run);
        }
        let events = self.queue.run_events(run.id())?;
        let Some(park) = events
            .iter()
            .rfind(|e| e.kind == event_kind::RECOVERY_PARKED)
            .map(|e| e.payload[PARK_SWITCH].clone())
            .filter(|switch| !switch.is_null())
        else {
            return Ok(run);
        };
        let reason = park["reason"]
            .as_str()
            .and_then(|reason| reason.parse::<SwitchReason>().ok())
            .unwrap_or(SwitchReason::UsageLimit);
        // Codex held since is its first turn's to find: the run then waits
        // in the hold ask with both providers held.
        let codex = Worker {
            provider: Provider::Codex,
            mode: WorkerMode::Headless,
        };
        if !provider_switch::may_switch(&events) || !self.workers.contains(&codex) {
            info!(run_id = %run.id(), "run {} stays on interactive Claude: this supervisor has no headless Codex worker", run.id());
            return Ok(run);
        }
        let message = park["message"].as_str().unwrap_or("").to_owned();
        self.switch(
            &run,
            "",
            Provider::Codex,
            reason,
            SwitchPhase::Resume,
            None,
            &message,
            "",
        )
    }
}

/// A worker's interactive Claude session stopped at `wall` on its screen:
/// when Codex's headless worker can take the run and the run has switches
/// left, Claude's hold ask opens (without the run) for the Claude-only jobs
/// and the instruction the run's resume carries is returned, for the
/// session's watch to park the run with; the run moves to headless Codex
/// when that resume begins ([`Supervisor::switch_parked`], ADR-t813-2
/// decision 5). `None` leaves the run to the hold ask as before.
pub(super) fn interactive_switch(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    wall: Wall,
) -> Result<Option<PendingSwitch>> {
    if headless(run) || run.actual_provider() != Provider::Claude {
        return Ok(None);
    }
    let events = sv.queue.run_events(run.id())?;
    if !provider_switch::may_switch(&events) || !sv.headless_usable(Provider::Codex) {
        return Ok(None);
    }
    let reason = match wall {
        Wall::Authentication => SwitchReason::Authentication,
        Wall::UsageLimit => SwitchReason::UsageLimit,
    };
    let message = format!(
        "the interactive session stopped at the {} wall",
        wall.as_str()
    );
    let instruction = switch_text(
        run,
        Provider::Claude,
        Provider::Codex,
        reason,
        &message,
        None,
    );
    sv.open_claude_hold(run, reason, &message)?;
    Ok(Some(PendingSwitch {
        instruction,
        reason,
        message,
    }))
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
        if !headless(run) {
            return Ok(WallGate::Open);
        }
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
