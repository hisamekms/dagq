//! How the turns of a headless planner of the runtime's ended, as the
//! supervisor acts on them (ADR-t1394-2 decisions 3 and 5): a turn that
//! failed at Claude's login, usage limit or start leaves the planner
//! waiting in the queue's hold, neither ended nor counted as a planner that
//! ended undecided, and once the hold is gone the call it failed at is made
//! again as its next turn; a turn its wrapper stopped at the `[stall]`
//! limits tells the inbox, as an interactive planner nothing was seen of
//! does.

use super::headless::{last_turn, provider_failure};
use super::provider::{PROVIDER_RETRY, retry_text};
use super::*;
use crate::application::{planner::PlannerView, planner_idle_marker};
use crate::domain::{
    EventKind, PlannerOrigin, PlannerRoute, PlannerSession, PlannerState,
    provider_switch::{self, SwitchReason},
    turn::{TurnFailure, TurnMark, TurnOutcome, TurnRequest, taken_path},
};

/// Whether the planner of `view` is a headless planner of the runtime's.
fn headless_runtime(view: &PlannerView) -> bool {
    view.planner.route == PlannerRoute::Headless && view.planner.origin == PlannerOrigin::Runtime
}

impl Supervisor<'_> {
    /// How the last turn of the planner of `view` ended, if it is a
    /// headless planner of the runtime's whose wrapper wrote it.
    fn planner_last_turn(&self, view: &PlannerView) -> Option<TurnMark> {
        headless_runtime(view)
            .then(|| last_turn(self, &planner_idle_marker(&view.dir)))
            .flatten()
    }

    /// Whether the headless planner of `view` took up the answer of `ask`
    /// written as its request: the turn that took the request finished.
    /// `None` for an interactive planner, and when no request of the answer
    /// is recorded (its record failed), which leave it to the idle marker's
    /// time.
    pub(super) fn headless_answer_taken(
        &self,
        view: &PlannerView,
        ask: AskId,
    ) -> Result<Option<bool>> {
        if !headless_runtime(view) {
            return Ok(None);
        }
        let events = self.queue.planner_turn_events(view.planner.id)?;
        let what = format!("answer of ask {ask}");
        let Some(seq) = events
            .iter()
            .rfind(|e| e.kind == event_kind::TURN_REQUESTED && e.payload["what"] == what.as_str())
            .and_then(|e| e.payload["seq"].as_u64())
        else {
            return Ok(None);
        };
        let turn = events
            .iter()
            .find(|e| e.kind == event_kind::TURN_STARTED && e.payload["request"] == seq)
            .and_then(|e| e.payload["turn"].as_u64());
        Ok(Some(turn.is_some_and(|turn| {
            events
                .iter()
                .any(|e| e.kind == event_kind::TURN_FINISHED && e.payload["turn"] == turn)
        })))
    }

    /// The provider failure the idle headless planner of `view` stopped at
    /// in its last turn, if it did: it waits for Claude rather than being
    /// done (ADR-t1394-2 decision 5).
    pub(super) fn planner_at_wall(&self, view: &PlannerView) -> Option<TurnFailure> {
        if view.state != PlannerState::Idle {
            return None;
        }
        provider_failure(self.planner_last_turn(view))
    }

    /// Hold or go on with each idle headless planner of the runtime's whose
    /// last turn failed at Claude's login, usage limit or start: at first
    /// sight the queue's hold ask (a login or a usage limit) or Claude's
    /// provider hold (a start) is raised and `provider_waiting` recorded
    /// with the planner's ID; once Claude is no longer held, the call the
    /// turn failed at is written as the planner's next request (`provider
    /// retry`), as a worker's is.
    pub(super) fn tend_planner_walls(&mut self, views: &[PlannerView]) -> Result<()> {
        for view in views {
            let Some(failure) = self.planner_at_wall(view) else {
                continue;
            };
            if let Err(error) = self.planner_wall(view, failure) {
                warn!(error = %format_args!("{error:#}"), "planner {}: its turn at {} could not be tended: {error:#}", view.planner.id, failure.as_str());
            }
        }
        Ok(())
    }

    fn planner_wall(&mut self, view: &PlannerView, failure: TurnFailure) -> Result<()> {
        let id = view.planner.id;
        let Some(reason) = SwitchReason::of_failure(failure) else {
            return Ok(());
        };
        let Some(workspace) = view.planner.workspace_id.clone() else {
            return Ok(());
        };
        let events = self.queue.planner_turn_events(id)?;
        let Some((finished_at, finished)) = events
            .iter()
            .enumerate()
            .rfind(|(_, e)| e.kind == event_kind::TURN_FINISHED)
            .map(|(at, e)| (at, e.payload.clone()))
        else {
            return Ok(());
        };
        // A request after the turn (the retry) answers it.
        if events[finished_at..]
            .iter()
            .any(|e| e.kind == event_kind::TURN_REQUESTED)
        {
            return Ok(());
        }
        let turn = finished["turn"].as_u64().unwrap_or(0);
        let message = finished["message"].as_str().unwrap_or("no message");
        if !provider_switch::waiting_on(&events, turn) {
            let ask_id = match reason {
                SwitchReason::Authentication | SwitchReason::UsageLimit => {
                    let wall = if reason == SwitchReason::Authentication {
                        Wall::Authentication
                    } else {
                        Wall::UsageLimit
                    };
                    let (outcome, _) = ask::hold(
                        &mut *self.queue,
                        &self.layout.main_checkout,
                        NewHold::wall(wall, None, None),
                        self.cmux,
                    )?;
                    Some(outcome.ask.id)
                }
                _ => {
                    self.hold_provider(Provider::Claude, reason, None, message)?;
                    None
                }
            };
            self.queue.record_queue_event(
                EventKind::ProviderWaiting,
                json!({
                    "planner_id": id,
                    "turn": turn,
                    "provider": Provider::Claude,
                    "reason": reason,
                    "message": message,
                    "ask_id": ask_id,
                }),
            )?;
            warn!(
                "planner {id} of the runtime: its turn {turn} failed on claude ({}); it waits for claude: {message}",
                reason.as_str()
            );
            return Ok(());
        }
        if self.provider_held(Provider::Claude).is_some() {
            return Ok(());
        }
        let started = events
            .iter()
            .rfind(|e| e.kind == event_kind::TURN_STARTED && e.payload["turn"] == turn)
            .map(|e| e.payload.clone())
            .unwrap_or_default();
        let undelivered = started["request"].as_u64().and_then(|seq| {
            let text = self
                .files
                .read_to_string(&taken_path(&view.dir, seq))
                .ok()?;
            serde_json::from_str::<TurnRequest>(&text).ok()
        });
        let text = retry_text(Provider::Claude, reason, undelivered.as_ref());
        self.stamp_planner_input(view);
        self.send_to_planner(view, &workspace, Input::Text(&text), PROVIDER_RETRY)?;
        info!(
            "planner {id} of the runtime: claude can be used again; the call of turn {turn} is made again"
        );
        Ok(())
    }

    /// Tell the inbox, once per planner, of a headless planner of the
    /// runtime's whose wrapper stopped its last turn at the `[stall]`
    /// limits (silent past `turn_silence_secs`, or running past
    /// `turn_limit_secs`): the turn's own limit stands in for the planner
    /// timeout of an interactive one (ADR-t1394-2 decision 3).
    pub(super) fn tell_of_stopped_planner_turns(&mut self, views: &[PlannerView]) -> Result<()> {
        for view in views {
            self.tell_of_stopped_planner_turn(&view.planner, &view.dir, view.state.as_str())?;
        }
        Ok(())
    }

    /// [`Self::tell_of_stopped_planner_turns`] for `planner`, whose
    /// directory is `dir` and state `state`: also for one the sweep closed
    /// before a pass saw it, its session over after the stopped turn.
    pub(super) fn tell_of_stopped_planner_turn(
        &mut self,
        planner: &PlannerSession,
        dir: &Path,
        state: &str,
    ) -> Result<()> {
        if planner.route != PlannerRoute::Headless || planner.origin != PlannerOrigin::Runtime {
            return Ok(());
        }
        let Some(mark) = last_turn(self, &planner_idle_marker(dir))
            .filter(|mark| matches!(mark.outcome, TurnOutcome::TimedOut | TurnOutcome::Silent))
        else {
            return Ok(());
        };
        let id = planner.id;
        let reason = format!(
            "turn {} of planner {id} of the runtime was stopped at its limit ({}); the planner ends undecided",
            mark.turn,
            mark.outcome.as_str()
        );
        let payload = json!({
            "subject": "planner",
            "planner_id": id,
            "origin": planner.origin.as_str(),
            "route": planner.route.as_str(),
            "workspace_id": planner.workspace_id,
            "draft_task_id": planner.draft_task_id,
            "finding_id": planner.finding_id,
            "request_id": planner.request_id,
            "state": state,
            "turn": mark.turn,
            "outcome": mark.outcome.as_str(),
            "reason": reason,
        });
        if self.queue.planner_silent(id, payload)? {
            warn!("{reason}; the inbox is told");
        }
        Ok(())
    }
}
