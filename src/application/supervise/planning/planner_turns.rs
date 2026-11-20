//! An existing runtime planner keeps its provider (ADR-t2930-1 decision
//! 5). At a wall it stays open and its failed call takes priority once
//! that provider's hold ends. Only Claude's login and usage limit open
//! the queue's hold ask. Revise and answer delivery waits on the
//! destination's provider and on its failed turn's retry. A turn stopped
//! by the wrapper's `[stall]` limits is reported to the inbox.

use super::headless::{last_turn, provider_failure};
use super::provider::{PROVIDER_RETRY, retry_text};
use super::*;
use crate::application::{planner::PlannerView, planner_idle_marker};
use crate::domain::run::{TurnRequested, restore_payload};
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
    /// At first sight, hold the turn's provider and record
    /// `provider_waiting`. Claude can use the queue's hold ask.
    Hold {
        reason: SwitchReason,
        wall: Option<Wall>,
    },
    /// The recorded hold ended: repeat the failed call.
    Retry(SwitchReason),
}

/// [`WallStep`] for a turn that failed with `failure`, the retry after it
/// written and finished (`answered`) or not, and `provider_waiting` of the
/// turn recorded (`waiting`) or not. A recorded wait permits a retry once
/// that provider's hold ends.
fn wall_step(
    provider: Provider,
    failure: TurnFailure,
    answered: bool,
    waiting: bool,
    held: bool,
) -> WallStep {
    let Some(reason) = SwitchReason::of_failure(failure) else {
        return WallStep::Nothing;
    };
    if answered || (waiting && held) {
        WallStep::Nothing
    } else if waiting {
        WallStep::Retry(reason)
    } else {
        WallStep::Hold {
            reason,
            wall: provider_switch::wall_of(reason).filter(|_| provider == Provider::Claude),
        }
    }
}

fn turn_provider(payload: &Value) -> Provider {
    payload["provider"]
        .as_str()
        .and_then(|provider| provider.parse().ok())
        .unwrap_or(Provider::Claude)
}

/// The payload of `provider_waiting` of planner `id` whose turn `turn`
/// failed on `provider` for `reason` with `message`, with the queue's
/// hold ask `ask_id` if the wall raised one.
fn waiting_payload(
    provider: Provider,
    id: PlannerId,
    turn: u64,
    reason: SwitchReason,
    message: &str,
    ask_id: Option<AskId>,
) -> Value {
    json!({
        "planner_id": id,
        "turn": turn,
        "provider": provider,
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

impl PlanningState {
    /// How the last turn of the planner of `view` ended, if it is a
    /// headless planner of the runtime's whose wrapper wrote it.
    fn planner_last_turn(&self, env: &PlanningEnv<'_>, view: &PlannerView) -> Option<TurnMark> {
        headless_runtime(view)
            .then(|| last_turn(&**env.files, &planner_idle_marker(&view.dir)))
            .flatten()
    }

    /// Whether the headless planner of `view` took up the answer of `ask`
    /// written as its request: the turn that took the request finished
    /// otherwise than at the provider's wall, or the `provider retry` that made
    /// it again did ([`request_read`]); a turn that failed at the wall did
    /// not get to it, and the planner ended then would lose it unread.
    /// `None` for an interactive planner, and when no request of the answer
    /// is recorded (its record failed), which leave it to the idle marker's
    /// time. It is not judged by the idle marker's second against the
    /// delivery's, which would keep waiting after a turn that ended within
    /// the same second, nor by when the ask closed: the ask closes after
    /// the delivery, and a planner idle before that would be waited on for
    /// good, filling the limit.
    pub(in crate::application::supervise) fn headless_answer_taken(
        &self,
        env: &PlanningEnv<'_>,
        view: &PlannerView,
        ask: AskId,
    ) -> Result<Option<bool>> {
        if !headless_runtime(view) {
            return Ok(None);
        }
        let events = env.queue.planner_turn_events(view.planner.id)?;
        let what = format!("answer of ask {ask}");
        let Some(seq) = events
            .iter()
            .rfind(|e| e.kind == EventKind::TurnRequested && e.payload["what"] == what.as_str())
            .and_then(|e| restore_payload::<TurnRequested>(&e.payload).seq)
        else {
            return Ok(None);
        };
        Ok(Some(request_read(&events, seq)))
    }

    /// The provider failure the idle headless planner of `view` stopped at
    /// in its last turn, if it did: it waits at its provider's hold rather than being
    /// done (ADR-t1394-2 decision 5).
    fn planner_at_wall(&self, env: &PlanningEnv<'_>, view: &PlannerView) -> Option<TurnFailure> {
        if view.state != PlannerState::Idle {
            return None;
        }
        provider_failure(self.planner_last_turn(env, view))
    }

    /// Whether to leave an idle planner's ordinary inputs waiting. For
    /// old turn records without a provider, the planner ran on Claude.
    pub(super) fn planner_delivery_waits(
        &self,
        env: &PlanningEnv<'_>,
        view: &PlannerView,
    ) -> Result<bool> {
        if self.planner_at_wall(env, view).is_some() {
            return Ok(true);
        }
        if [Provider::Claude, Provider::Codex]
            .into_iter()
            .all(|provider| env.jobs.provider_held(provider).is_none())
        {
            return Ok(false);
        }
        let provider = env
            .queue
            .planner_turn_events(view.planner.id)?
            .iter()
            .rfind(|event| event.kind == EventKind::TurnStarted)
            .map_or(Provider::Claude, |event| turn_provider(&event.payload));
        Ok(env.jobs.provider_held(provider).is_some())
    }

    /// Hold or go on with each idle headless planner of the runtime's whose
    /// last turn failed at a provider wall: Claude's login and usage limit
    /// raise the queue's hold ask, and other walls raise a provider hold.
    /// Once the hold ends, retry the failed call as the next request.
    pub(in crate::application::supervise) fn tend_planner_walls(
        &mut self,
        env: &mut PlanningEnv<'_>,
        views: &[PlannerView],
    ) -> Result<()> {
        for view in views {
            let Some(failure) = self.planner_at_wall(env, view) else {
                continue;
            };
            if let Err(error) = self.planner_wall(env, view, failure) {
                warn!(error = %format_args!("{error:#}"), "planner {}: its turn at {} could not be tended: {error:#}", view.planner.id, failure.as_str());
            }
        }
        Ok(())
    }

    fn planner_wall(
        &mut self,
        env: &mut PlanningEnv<'_>,
        view: &PlannerView,
        failure: TurnFailure,
    ) -> Result<()> {
        let id = view.planner.id;
        if SwitchReason::of_failure(failure).is_none() {
            return Ok(());
        }
        let Some(workspace) = view.planner.workspace_id.clone() else {
            return Ok(());
        };
        let events = env.queue.planner_turn_events(id)?;
        let Some((finished_at, finished)) = events
            .iter()
            .enumerate()
            .rfind(|(_, e)| e.kind == EventKind::TurnFinished)
            .map(|(at, e)| (at, e.payload.clone()))
        else {
            return Ok(());
        };
        let provider = turn_provider(&finished);
        let turn = finished["turn"].as_u64().unwrap_or(0);
        let message = finished["message"].as_str().unwrap_or("no message");
        let reason = match wall_step(
            provider,
            failure,
            // The retry after the turn answers it ([`wall_answered`]).
            wall_answered(&events, finished_at),
            provider_switch::waiting_on(&events, turn),
            env.jobs.provider_held(provider).is_some(),
        ) {
            WallStep::Nothing => return Ok(()),
            WallStep::Hold { reason, wall } => {
                let ask_id = match wall {
                    Some(wall) => {
                        let (outcome, _) =
                            ask::hold(&mut *env.queue, NewHold::wall(wall, None, None))?;
                        env.jobs.remember_queue_hold(&outcome.ask);
                        Some(outcome.ask.id)
                    }
                    _ => {
                        env.jobs
                            .hold_provider(&mut env.pass, provider, reason, None, message)?;
                        None
                    }
                };
                // Kept as a result, though the next pass would find it
                // missing: found again, the hold would be taken again, and
                // a hold ask a person answered meanwhile opened again. What
                // is kept goes before each pass reads the queue.
                env.queue.record_queue_event(
                    EventKind::ProviderWaiting,
                    waiting_payload(provider, id, turn, reason, message, ask_id),
                )?;
                warn!(
                    "planner {id} of the runtime: its turn {turn} failed on {} ({}); it waits for that provider: {message}",
                    provider.as_str(),
                    reason.as_str()
                );
                return Ok(());
            }
            WallStep::Retry(reason) => reason,
        };
        // The call that first met the wall, which a retry that met it
        // again carries too.
        let undelivered = request_to_retry(&events).and_then(|seq| {
            let text = env.files.read_to_string(&taken_path(&view.dir, seq)).ok()?;
            serde_json::from_str::<TurnRequest>(&text).ok()
        });
        let text = retry_text(provider, reason, undelivered.as_ref(), Some(&view.dir)).text;
        // Stamped before it is written, as every request: a stamp whose
        // retry was not written (or was, after a turn that already ended)
        // leaves the marker stale, and the wrapper, which takes nothing
        // before the retry, renews it so that the wall is tended again.
        self.stamp_planner_input(env, view);
        send_to_planner(
            &**env.files,
            &*env.queue,
            view,
            &workspace,
            Input::Text(&text),
            PROVIDER_RETRY,
        )?;
        info!(
            "planner {id} of the runtime: {} can be used again; the call of turn {turn} is made again",
            provider.as_str()
        );
        Ok(())
    }

    /// Tell the inbox, once per planner, of a headless planner of the
    /// runtime's whose wrapper stopped its last turn at the `[stall]`
    /// limits (silent past `turn_silence_secs`, or running past
    /// `turn_limit_secs`): the turn's own limit stands in for the planner
    /// timeout of an interactive one (ADR-t1394-2 decision 3).
    pub(in crate::application::supervise) fn tell_of_stopped_planner_turns(
        &mut self,
        env: &mut PlanningEnv<'_>,
        views: &[PlannerView],
    ) -> Result<()> {
        for view in views {
            tell_of_stopped_planner_turn(env, &view.planner, &view.dir, view.state.as_str())?;
        }
        Ok(())
    }
}

/// [`PlanningState::tell_of_stopped_planner_turns`] for `planner`, whose
/// directory is `dir` and state `state`: also for one the sweep closed
/// before a pass saw it, its session over after the stopped turn.
pub(in crate::application::supervise) fn tell_of_stopped_planner_turn(
    env: &PassEnv<'_>,
    planner: &PlannerSession,
    dir: &Path,
    state: &str,
) -> Result<()> {
    if !headless_runtime_row(planner) {
        return Ok(());
    }
    let Some((reason, payload)) = last_turn(&**env.files, &planner_idle_marker(dir))
        .and_then(|mark| stopped_turn_notice(planner, state, &mark))
    else {
        return Ok(());
    };
    if env.queue.planner_silent(planner.id, payload)? {
        warn!("{reason}; the inbox is told");
    }
    Ok(())
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
        for provider in [Provider::Claude, Provider::Codex] {
            for (failure, reason, claude_wall) in [
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
                let wall = claude_wall.filter(|_| provider == Provider::Claude);
                assert_eq!(
                    wall_step(provider, failure, false, false, false),
                    WallStep::Hold { reason, wall }
                );
                assert_eq!(
                    wall_step(provider, failure, false, true, true),
                    WallStep::Nothing
                );
                assert_eq!(
                    wall_step(provider, failure, false, true, false),
                    WallStep::Retry(reason)
                );
                for waiting in [false, true] {
                    assert_eq!(
                        wall_step(provider, failure, true, waiting, false),
                        WallStep::Nothing
                    );
                }
            }
            for failure in [TurnFailure::Model, TurnFailure::Sandbox, TurnFailure::Other] {
                assert_eq!(
                    wall_step(provider, failure, false, false, false),
                    WallStep::Nothing
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
    fn a_turn_at_the_wall_holds_after_the_late_record_of_its_own_retry() {
        let event = |kind: EventKind, payload: Value| RunEvent {
            id: crate::domain::EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.as_str().into(),
            payload,
            created_at: String::new(),
            actor: None,
        };
        let retry = |seq: u64| {
            event(
                EventKind::TurnRequested,
                json!({"seq": seq, "what": PROVIDER_RETRY}),
            )
        };
        // The wrapper took retry 4 before the supervisor recorded it, and
        // the turn met the usage limit again before that record.
        let late = vec![
            event(
                EventKind::TurnStarted,
                json!({"turn": 3, "request": 4, "what": PROVIDER_RETRY}),
            ),
            event(
                EventKind::TurnFinished,
                json!({"turn": 3, "outcome": "failed", "failure": "usage_limit"}),
            ),
            retry(4),
        ];
        assert_eq!(
            wall_step(
                Provider::Claude,
                TurnFailure::UsageLimit,
                wall_answered(&late, 1),
                false,
                false
            ),
            WallStep::Hold {
                reason: SwitchReason::UsageLimit,
                wall: Some(Wall::UsageLimit),
            }
        );
        // A retry recorded after it, past the request the turn took,
        // answers it as before.
        let answered = [late, vec![retry(5)]].concat();
        assert_eq!(
            wall_step(
                Provider::Claude,
                TurnFailure::UsageLimit,
                wall_answered(&answered, 1),
                false,
                false
            ),
            WallStep::Nothing
        );
    }

    #[test]
    fn provider_waiting_names_the_planner_its_turn_why_and_the_hold() {
        let waiting = waiting_payload(
            Provider::Claude,
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
            Provider::Claude,
            PlannerId::new(3),
            1,
            SwitchReason::Authentication,
            "Not logged in",
            Some(AskId::new(4)),
        );
        assert_eq!(login["turn"], 1);
        assert_eq!(login["reason"], "authentication");
        assert_eq!(login["ask_id"], 4);
        // Codex's provider hold is no ask.
        let start = waiting_payload(
            Provider::Codex,
            PlannerId::new(3),
            1,
            SwitchReason::LaunchFailed,
            "no message",
            None,
        );
        assert_eq!(start["ask_id"], Value::Null);
        assert_eq!(start["provider"], "codex");
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
