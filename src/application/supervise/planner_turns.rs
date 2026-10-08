//! How the turns of a headless planner of the runtime's ended, as the
//! supervisor acts on them (ADR-t1394-2 decisions 3 and 5): a turn that
//! failed at Claude's login, usage limit or start leaves the planner
//! waiting in the queue's hold, neither ended nor counted as a planner that
//! ended undecided, and once the hold is gone the call it failed at is made
//! again as its next turn; a turn its wrapper stopped at the `[stall]`
//! limits tells the inbox.

use super::headless::{last_turn, provider_failure};
use super::provider::{PROVIDER_RETRY, retry_text};
use super::*;
use crate::application::{planner::PlannerView, planner_idle_marker};
use crate::domain::{
    EventKind, PlannerId, PlannerOrigin, PlannerRoute, PlannerSession, PlannerState,
    provider_switch::{self, SwitchReason},
    turn::{
        TurnFailure, TurnMark, TurnOutcome, TurnRequest, request_read, request_to_retry,
        taken_path, wall_answered,
    },
};

/// Whether the planner of `view` is a headless planner of the runtime's.
fn headless_runtime(view: &PlannerView) -> bool {
    headless_runtime_row(&view.planner)
}

/// Whether `planner` is a headless planner of the runtime's: only its
/// turns are judged here.
fn headless_runtime_row(planner: &PlannerSession) -> bool {
    planner.route == PlannerRoute::Headless && planner.origin == PlannerOrigin::Runtime
}

/// What tending an idle headless planner whose last turn failed so does on
/// this look (ADR-t1394-2 decision 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WallStep {
    /// Nothing: the failure is not the provider's, or the `provider
    /// retry` after the turn answered it ([`wall_answered`]).
    Nothing,
    /// At first sight: the queue's hold ask of the wall (a login or a usage
    /// limit), or Claude's provider hold when there is none (a start), and
    /// `provider_waiting`.
    Hold {
        reason: SwitchReason,
        wall: Option<Wall>,
    },
    /// It waits already: once Claude is not held, the call the turn failed
    /// at is made again.
    Retry(SwitchReason),
}

/// [`WallStep`] for a turn that failed with `failure`, the retry after it
/// written and finished (`answered`) or not, and `provider_waiting` of the
/// turn recorded (`waiting`) or not.
fn wall_step(failure: TurnFailure, answered: bool, waiting: bool) -> WallStep {
    let Some(reason) = SwitchReason::of_failure(failure) else {
        return WallStep::Nothing;
    };
    if answered {
        WallStep::Nothing
    } else if waiting {
        WallStep::Retry(reason)
    } else {
        WallStep::Hold {
            reason,
            wall: provider_switch::wall_of(reason),
        }
    }
}

/// The payload of `provider_waiting` of planner `id` whose turn `turn`
/// failed on Claude for `reason` with `message`, holding in the queue's
/// hold ask `ask_id` (none for Claude's provider hold).
fn waiting_payload(
    id: PlannerId,
    turn: u64,
    reason: SwitchReason,
    message: &str,
    ask_id: Option<AskId>,
) -> Value {
    json!({
        "planner_id": id,
        "turn": turn,
        "provider": Provider::Claude,
        "reason": reason,
        "message": message,
        "ask_id": ask_id,
    })
}

/// What the inbox is told of `planner`, judged `state`, whose last turn
/// `mark` says: the reason and the `planner_unresponsive` payload when its
/// wrapper stopped the turn at the `[stall]` limits (silent or past its
/// limit), else nothing.
fn stopped_turn_notice(
    planner: &PlannerSession,
    state: &str,
    mark: &TurnMark,
) -> Option<(String, Value)> {
    if !matches!(mark.outcome, TurnOutcome::TimedOut | TurnOutcome::Silent) {
        return None;
    }
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
    Some((reason, payload))
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
    /// written as its request: the turn that took the request finished
    /// otherwise than at Claude's wall, or the `provider retry` that made
    /// it again did ([`request_read`]); a turn that failed at the wall did
    /// not get to it, and the planner ended then would lose it unread.
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
        Ok(Some(request_read(&events, seq)))
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
        if SwitchReason::of_failure(failure).is_none() {
            return Ok(());
        }
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
        let turn = finished["turn"].as_u64().unwrap_or(0);
        let message = finished["message"].as_str().unwrap_or("no message");
        let reason = match wall_step(
            failure,
            // The retry after the turn answers it ([`wall_answered`]).
            wall_answered(&events[finished_at..]),
            provider_switch::waiting_on(&events, turn),
        ) {
            WallStep::Nothing => return Ok(()),
            WallStep::Hold { reason, wall } => {
                let ask_id = match wall {
                    Some(wall) => {
                        let (outcome, _) =
                            ask::hold(&mut *self.queue, NewHold::wall(wall, None, None))?;
                        Some(outcome.ask.id)
                    }
                    None => {
                        self.hold_provider(Provider::Claude, reason, None, message)?;
                        None
                    }
                };
                self.queue.record_queue_event(
                    EventKind::ProviderWaiting,
                    waiting_payload(id, turn, reason, message, ask_id),
                )?;
                warn!(
                    "planner {id} of the runtime: its turn {turn} failed on claude ({}); it waits for claude: {message}",
                    reason.as_str()
                );
                return Ok(());
            }
            WallStep::Retry(reason) => reason,
        };
        if self.provider_held(Provider::Claude).is_some() {
            return Ok(());
        }
        // The call that first met the wall, which a retry that met it
        // again carries too.
        let undelivered = request_to_retry(&events).and_then(|seq| {
            let text = self
                .files
                .read_to_string(&taken_path(&view.dir, seq))
                .ok()?;
            serde_json::from_str::<TurnRequest>(&text).ok()
        });
        let text = retry_text(
            Provider::Claude,
            reason,
            undelivered.as_ref(),
            Some(&view.dir),
        )
        .text;
        // Stamped before it is written, as every request: a stamp whose
        // retry was not written (or was, after a turn that already ended)
        // leaves the marker stale, and the wrapper, which takes nothing
        // before the retry, renews it so that the wall is tended again.
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
        if !headless_runtime_row(planner) {
            return Ok(());
        }
        let Some((reason, payload)) = last_turn(self, &planner_idle_marker(dir))
            .and_then(|mark| stopped_turn_notice(planner, state, &mark))
        else {
            return Ok(());
        };
        if self.queue.planner_silent(planner.id, payload)? {
            warn!("{reason}; the inbox is told");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{FindingId, RequestId, TaskId, queue_hold::Wall};

    fn planner(origin: PlannerOrigin, route: PlannerRoute) -> PlannerSession {
        PlannerSession {
            id: PlannerId::new(3),
            origin,
            proposal_id: None,
            draft_task_id: Some(TaskId::new(2)),
            finding_id: None,
            request_id: Some(RequestId::new(5)),
            workspace_id: Some("background:1:x".to_owned()),
            wrapper_pid: Some(1),
            agent_pid: Some(2),
            heartbeat_at: Some(0),
            exit_code: None,
            exited_at: None,
            closed_at: None,
            error: None,
            created_at: 0,
            route,
            answer_wait_at: None,
        }
    }

    fn mark(turn: u64, outcome: TurnOutcome) -> TurnMark {
        TurnMark {
            turn,
            outcome,
            failure: None,
            permission_denials: 0,
        }
    }

    #[test]
    fn only_a_headless_planner_of_the_runtimes_has_its_turns_judged() {
        assert!(headless_runtime_row(&planner(
            PlannerOrigin::Runtime,
            PlannerRoute::Headless
        )));
        for (origin, route) in [
            (PlannerOrigin::Runtime, PlannerRoute::Interactive),
            (PlannerOrigin::Person, PlannerRoute::Interactive),
            (PlannerOrigin::Person, PlannerRoute::Headless),
        ] {
            assert!(
                !headless_runtime_row(&planner(origin, route)),
                "{origin:?} {route:?}"
            );
        }
    }

    // Moved here by task 1711 from the tests/it cases it removed:
    // planner_headless_turns::a_headless_planner_at_the_usage_limit_waits_in_the_hold_and_makes_the_same_call_again
    // and a_headless_planner_at_a_login_failure_waits_in_the_hold_and_goes_on.
    // The kept planner_headless_turns::a_request_written_during_a_turn_at_the_usage_limit_waits_for_the_retry_of_that_turn
    // checks the wiring of the hold and the retry.
    #[test]
    fn a_turn_at_the_wall_holds_at_first_sight_then_is_made_again_once_and_only_then() {
        // A login or a usage limit holds in the queue's hold ask of its
        // wall; an agent that did not start holds Claude itself.
        for (failure, reason, wall) in [
            (
                TurnFailure::Authentication,
                SwitchReason::Authentication,
                Some(Wall::Authentication),
            ),
            (
                TurnFailure::UsageLimit,
                SwitchReason::UsageLimit,
                Some(Wall::UsageLimit),
            ),
            (TurnFailure::Launch, SwitchReason::LaunchFailed, None),
        ] {
            assert_eq!(
                wall_step(failure, false, false),
                WallStep::Hold { reason, wall },
                "{failure:?}"
            );
            // Waiting already: the call is made again.
            assert_eq!(
                wall_step(failure, false, true),
                WallStep::Retry(reason),
                "{failure:?}"
            );
            // The retry after the turn answered it.
            for waiting in [false, true] {
                assert_eq!(wall_step(failure, true, waiting), WallStep::Nothing);
            }
        }
        // A failure that is not the provider's is not tended here.
        for failure in [TurnFailure::Model, TurnFailure::Sandbox, TurnFailure::Other] {
            for (answered, waiting) in [(false, false), (false, true), (true, false)] {
                assert_eq!(
                    wall_step(failure, answered, waiting),
                    WallStep::Nothing,
                    "{failure:?}"
                );
            }
        }
        // The login's hold ask is an authentication one, the limit's a
        // cost one.
        assert_eq!(
            NewHold::wall(Wall::Authentication, None, None).reason_category,
            AskReason::Authentication
        );
        assert_eq!(
            NewHold::wall(Wall::UsageLimit, None, None).reason_category,
            AskReason::Cost
        );
    }

    #[test]
    fn provider_waiting_names_the_planner_its_turn_why_and_the_hold() {
        let waiting = waiting_payload(
            PlannerId::new(3),
            2,
            SwitchReason::UsageLimit,
            "Claude AI usage limit reached",
            Some(AskId::new(9)),
        );
        assert_eq!(
            waiting,
            json!({
                "planner_id": 3,
                "turn": 2,
                "provider": "claude",
                "reason": "usage_limit",
                "message": "Claude AI usage limit reached",
                "ask_id": 9,
            })
        );
        let login = waiting_payload(
            PlannerId::new(3),
            1,
            SwitchReason::Authentication,
            "Not logged in",
            Some(AskId::new(4)),
        );
        assert_eq!(login["turn"], 1);
        assert_eq!(login["reason"], "authentication");
        assert_eq!(login["ask_id"], 4);
        // Claude's provider hold is no ask.
        let start = waiting_payload(
            PlannerId::new(3),
            1,
            SwitchReason::LaunchFailed,
            "no message",
            None,
        );
        assert_eq!(start["ask_id"], Value::Null);
        assert_eq!(start["reason"], "launch_failed");
    }

    // Kept as the boundary: planner_headless_turns::a_headless_planners_turn_stopped_at_its_limit_tells_the_inbox_and_the_planner_closes
    // and the_sweep_tells_of_a_headless_planners_turn_stopped_at_its_limit_as_it_closes_it.
    #[test]
    fn a_turn_stopped_at_its_limit_tells_the_inbox_with_the_planner_and_the_turn() {
        let mut row = planner(PlannerOrigin::Runtime, PlannerRoute::Headless);
        row.finding_id = Some(FindingId::new(7));
        for outcome in [TurnOutcome::TimedOut, TurnOutcome::Silent] {
            let (reason, payload) = stopped_turn_notice(&row, "closed", &mark(4, outcome)).unwrap();
            assert_eq!(
                reason,
                format!(
                    "turn 4 of planner 3 of the runtime was stopped at its limit ({}); the planner ends undecided",
                    outcome.as_str()
                )
            );
            assert_eq!(
                payload,
                json!({
                    "subject": "planner",
                    "planner_id": 3,
                    "origin": "runtime",
                    "route": "headless",
                    "workspace_id": "background:1:x",
                    "draft_task_id": 2,
                    "finding_id": 7,
                    "request_id": 5,
                    "state": "closed",
                    "turn": 4,
                    "outcome": outcome.as_str(),
                    "reason": reason,
                })
            );
        }
        for outcome in [
            TurnOutcome::Succeeded,
            TurnOutcome::Failed,
            TurnOutcome::LaunchMismatch,
            TurnOutcome::Stopped,
        ] {
            assert_eq!(
                stopped_turn_notice(&row, "idle", &mark(1, outcome)),
                None,
                "{outcome:?}"
            );
        }
    }
}
