//! A headless worker's session as the supervisor sees it (ADR-t813-1), the
//! only kind since task 1437: every text for the session is written as its
//! next request in the run's `turns/` directory, and the end as its exit
//! request ([`request_turn`]); the session wrapper runs each request as a
//! resume of the same session and writes the idle marker when the turn
//! ended. The screen is never read for such a session (no dialog, input box
//! or idle to infer), and a request needs no check that it was taken: the
//! turn that runs it starts and ends. How the turns went is read from the
//! idle marker ([`TurnMark`]) and the `turn_finished` events.

use super::*;
use crate::application::planner::PlannerView;
use crate::domain::EventKind;
use crate::domain::PlannerRoute;
use crate::domain::recovery::PERMISSION_DENIED;
use crate::domain::turn::{
    self, LIMITS_FILE, ListedRequest, RequestState, TurnFailure, TurnMark, TurnOutcome,
    TurnRequest, exit_path, next_seq, request_path, turns_dir,
};

/// Write `input` for the headless session of `run` (in `workspace`, for
/// the records): a text becomes its next request, recorded as
/// `turn_requested` (with the `prompt_bytes` of a message held to its
/// limits), and `/exit` its exit request. A failed write is a failed
/// typing.
pub(super) fn request_turn(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    input: Input<'_>,
    what: &str,
) -> Result<Submission> {
    let run_dir = Path::new(run.run_dir().context("missing run directory")?);
    let dir = turns_dir(run_dir);
    sv.files.create_dir_all(&dir)?;
    let (text, bytes) = match input {
        Input::Exit => {
            sv.files.write(&exit_path(run_dir), b"")?;
            info!(run_id = %run.id(), "exit requested of the headless session of {}", run.id());
            return Ok(Submission::Queued);
        }
        Input::Text(text) => (text, None),
        Input::Prompt { text, bytes } => (text, Some(bytes)),
    };
    let seq = write_request(&*sv.files, run_dir, text, what)?;
    // Written: a record that fails is only noted, as for a typed text.
    let mut payload = json!({"seq": seq, "what": what, "workspace_id": workspace});
    if let Some(bytes) = bytes {
        payload["prompt_bytes"] = json!(bytes);
    }
    if let Err(error) = sv
        .queue
        .record_runtime_event(run.id(), EventKind::TurnRequested, payload)
    {
        warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "turn_requested of {} could not be recorded: {error:#}", run.id());
    }
    info!(run_id = %run.id(), "{what} requested of the headless session of {} (request {seq})", run.id());
    Ok(Submission::Queued)
}

/// The lock next to a headless session's `turns/` that the writers of its
/// requests take: the supervisor and `planner request` (ADR-t1533-1) may
/// write one at the same time, and each must get its own number.
const REQUESTS_LOCK: &str = "turns.lock";

/// How many times, a few milliseconds apart, a writer tries for the lock
/// another writer holds.
const REQUESTS_LOCK_TRIES: u32 = 200;

/// Take the lock `path` ([`RunFiles::try_lock`]), trying again a few
/// milliseconds apart while another writer holds it, for at most about two
/// seconds.
pub(crate) fn lock_waiting(
    files: &dyn RunFiles,
    path: &Path,
) -> Result<Box<dyn std::any::Any + Send>> {
    let mut tries = 0;
    loop {
        if let Some(guard) = files.try_lock(path)? {
            return Ok(guard);
        }
        tries += 1;
        ensure!(
            tries < REQUESTS_LOCK_TRIES,
            "another writer holds {} too long",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Write `text` as the next request (`what`) of the headless session whose
/// directory is `dir` (its `turns/` made); the request's sequence number.
/// Requests are taken in the order of their numbers, so one written while
/// a turn runs is taken after it.
pub(crate) fn write_request(
    files: &dyn RunFiles,
    dir: &Path,
    text: &str,
    what: &str,
) -> Result<u64> {
    let turns = turns_dir(dir);
    files.create_dir_all(&turns)?;
    let _guard = lock_waiting(files, &dir.join(REQUESTS_LOCK))?;
    let names: Vec<String> = files
        .read_dir(&turns)?
        .iter()
        .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
        .collect();
    let seq = next_seq(names.iter().map(String::as_str));
    let request = TurnRequest {
        seq,
        what: what.to_owned(),
        prompt: text.to_owned(),
    };
    let path = request_path(dir, seq);
    let tmp = path.with_extension("json.tmp");
    files.write(&tmp, serde_json::to_string(&request)?.as_bytes())?;
    files.rename(&tmp, &path)?;
    Ok(seq)
}

impl Supervisor<'_> {
    /// Send `input` (`what` names it) to the planner of `view` in
    /// `workspace`: a headless planner, every planner of the runtime's
    /// (ADR-t1394-2 decision 2, ADR-t1433-2 decision 3), gets it written as
    /// its next request in its directory's `turns/`, recorded as
    /// `turn_requested` naming it, and the exit as its exit request. Nothing
    /// is typed into a planner: a row in a cmux workspace (one of the
    /// runtime's an older binary opened, or a person's, ADR-t1433-2
    /// decision 5) is refused.
    pub(super) fn send_to_planner(
        &mut self,
        view: &PlannerView,
        workspace: &str,
        input: Input<'_>,
        what: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            view.planner.route == PlannerRoute::Headless,
            "planner {} was opened in a workspace: nothing is typed into it (ADR-t1433-2)",
            view.planner.id
        );
        let id = view.planner.id;
        let text = match input {
            Input::Exit => {
                self.files.create_dir_all(&turns_dir(&view.dir))?;
                self.files.write(&exit_path(&view.dir), b"")?;
                info!("exit requested of the headless planner {id}");
                return Ok(());
            }
            Input::Text(text) | Input::Prompt { text, .. } => text,
        };
        let seq = write_request(&*self.files, &view.dir, text, what)?;
        // Written: a record that fails is only noted, as for a typed text.
        if let Err(error) = self.queue.record_queue_event(
            EventKind::TurnRequested,
            json!({"planner_id": id, "seq": seq, "what": what, "workspace_id": workspace}),
        ) {
            warn!(error = %format_args!("{error:#}"), "turn_requested of planner {id} could not be recorded: {error:#}");
        }
        info!("{what} requested of the headless planner {id} (request {seq})");
        Ok(())
    }
}

/// What the request carrying the answer of the `stalled` ask `ask` is: it
/// names the ask, so the request itself records that the answer was sent.
pub(super) fn stalled_answer_what(ask: AskId) -> String {
    format!("answer of the stalled ask {ask}")
}

/// Whether the exit of the headless session of `run` is requested and not
/// yet dropped with its session.
pub(super) fn exit_requested(sv: &Supervisor<'_>, run: &TaskRun) -> bool {
    run.run_dir()
        .is_some_and(|dir| sv.files.is_file(&exit_path(Path::new(dir))))
}

/// The request `what` written for the headless session of `run`, waiting
/// or taken, if there is one: a supervisor that stopped after writing it
/// left it for its adopter to find, so the request is not written twice.
/// A request dropped with an earlier session was never run and does not
/// count.
pub(super) fn requested(sv: &Supervisor<'_>, run: &TaskRun, what: &str) -> Result<Option<u64>> {
    let run_dir = Path::new(run.run_dir().context("missing run directory")?);
    let paths = match sv.files.read_dir(&turns_dir(run_dir)) {
        Ok(paths) => paths,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read the requests of the headless session"),
    };
    for path in paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some((seq, _)) = turn::request_seq(name).filter(|_| name.ends_with(".json")) else {
            continue;
        };
        // A request taken between the listing and the read is read under
        // its taken name.
        let content = match sv.files.read(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                sv.files.read(&turn::taken_path(run_dir, seq))
            }
            read => read,
        }
        .context("read a request of the headless session")?;
        let Ok(request) = serde_json::from_slice::<TurnRequest>(&content) else {
            continue;
        };
        if request.what == what {
            return Ok(Some(request.seq));
        }
    }
    Ok(None)
}

/// Every request in the `turns/` of the run directory `run_dir`, waiting,
/// taken or dropped (none when it has no `turns/`). A request taken between
/// the listing and the read is read under its taken name; one that cannot
/// be parsed is skipped.
pub(super) fn listed_requests(files: &dyn RunFiles, run_dir: &Path) -> Result<Vec<ListedRequest>> {
    let paths = match files.read_dir(&turns_dir(run_dir)) {
        Ok(paths) => paths,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("read the requests of the headless session"),
    };
    let mut listed = Vec::new();
    for path in paths {
        let Some((seq, mut state)) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(turn::request_state)
        else {
            continue;
        };
        let content = match files.read(&path) {
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && state == RequestState::Pending =>
            {
                state = RequestState::Taken;
                files.read(&turn::taken_path(run_dir, seq))
            }
            read => read,
        }
        .context("read a request of the headless session")?;
        if let Ok(request) = serde_json::from_slice::<TurnRequest>(&content) {
            listed.push(ListedRequest { state, request });
        }
    }
    Ok(listed)
}

impl Supervisor<'_> {
    /// Before a headless session of `run` starts in its run directory: its
    /// turn limits from the `[stall]` settings, and neither the exit request
    /// nor a request an earlier session left untaken: the new session
    /// starts from the supervisor's own request.
    pub(super) fn prepare_turns(&self, run: &TaskRun, run_dir: &Path) -> Result<()> {
        let dir = turns_dir(run_dir);
        self.files.create_dir_all(&dir)?;
        self.files.write(
            &dir.join(LIMITS_FILE),
            serde_json::to_string(&self.stall.turn_limits())?.as_bytes(),
        )?;
        let names: Vec<String> = self
            .files
            .read_dir(&dir)?
            .iter()
            .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
            .collect();
        for seq in turn::pending(names.iter().map(String::as_str)) {
            let path = request_path(run_dir, seq);
            self.files
                .rename(&path, &path.with_extension("dropped"))
                .with_context(|| format!("drop the untaken request {}", path.display()))?;
            info!(run_id = %run.id(), "request {seq} of an earlier session of {} was never taken; dropped", run.id());
        }
        match self.files.remove_file(&exit_path(run_dir)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove an earlier exit request"),
        }
    }
}

/// How the last turn of a headless session ended, from its idle marker.
pub(super) fn last_turn(sv: &Supervisor<'_>, idle_marker: &Path) -> Option<TurnMark> {
    sv.files
        .read(idle_marker)
        .ok()
        .and_then(|content| TurnMark::parse(&content))
}

/// Whether the headless session is between turns: its idle marker names
/// the last turn that started (`events` are the run's). What is sent then
/// is the next turn's request, however long ago the turn ended: an idle
/// marker older than the ask the turn opened (its clock and the queue's
/// need not agree to the second) does not mean the session went on past
/// it.
pub(super) fn between_turns(sv: &Supervisor<'_>, idle_marker: &Path, events: &[RunEvent]) -> bool {
    let Some(mark) = last_turn(sv, idle_marker) else {
        return false;
    };
    events
        .iter()
        .filter(|event| event.kind == event_kind::TURN_STARTED)
        .filter_map(|event| event.payload["turn"].as_u64())
        .max()
        .is_none_or(|started| mark.turn >= started)
}

/// What the recovery job of a headless session reads instead of its
/// screen: its last turns, as recorded.
pub(super) fn turns_excerpt(sv: &Supervisor<'_>, run: &TaskRun) -> String {
    let events = match sv.queue.run_events(run.id()) {
        Ok(events) => events,
        Err(error) => return format!("(the turns could not be read: {error:#})"),
    };
    format_turns_excerpt(&events)
}

/// A recovery job's material from recorded turns, independent of the queue.
fn format_turns_excerpt(events: &[RunEvent]) -> String {
    let finished: Vec<String> = events
        .iter()
        .filter(|e| e.kind == event_kind::TURN_FINISHED)
        .rev()
        .take(5)
        .map(|e| {
            let p = &e.payload;
            format!(
                "turn {}: {}{}; {} permission denial(s){}{}",
                p["turn"],
                p["outcome"].as_str().unwrap_or("?"),
                p["failure"]
                    .as_str()
                    .map_or(String::new(), |f| format!(" ({f})")),
                p["permission_denials"].as_u64().unwrap_or(0),
                p["denied_tools"]
                    .as_array()
                    .filter(|tools| !tools.is_empty())
                    .map_or(String::new(), |tools| format!(
                        " [{}]",
                        tools
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                p["message"]
                    .as_str()
                    .map_or(String::new(), |m| format!(": {m}")),
            )
        })
        .collect();
    if finished.is_empty() {
        return "(a headless session: no turn ended yet)".to_owned();
    }
    format!(
        "(a headless session has no screen; its last turns, newest first)\n{}",
        finished.join("\n")
    )
}

/// Why a headless session that ended its turn with neither a receipt nor
/// an open question goes to its recovery job at once, if it does: its
/// last turn was refused too many tool calls. `None` when it is nudged
/// first.
pub(super) fn alert_at_once(mark: Option<TurnMark>) -> Option<&'static str> {
    mark.filter(|mark| mark.permission_denials >= turn::PERMISSION_DENIAL_LIMIT)
        .map(|_| PERMISSION_DENIED)
}

/// The provider could not be used in the last turn (its login ran out, its
/// usage limit was hit, or its agent did not start): the failure, which
/// moves the run to the other provider or holds it for a person rather
/// than a nudge (ADR-t813-2).
pub(crate) fn provider_failure(mark: Option<TurnMark>) -> Option<TurnFailure> {
    mark.filter(|mark| mark.outcome == TurnOutcome::Failed)
        .and_then(|mark| mark.failure)
        .filter(|failure| {
            matches!(
                failure,
                TurnFailure::Authentication | TurnFailure::UsageLimit | TurnFailure::Launch
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(outcome: TurnOutcome, failure: Option<TurnFailure>, denials: usize) -> TurnMark {
        TurnMark {
            turn: 1,
            outcome,
            failure,
            permission_denials: denials,
        }
    }

    #[test]
    fn recovery_material_includes_silent_and_timed_out_turns_without_a_screen() {
        let event = |kind: &str, payload: Value| RunEvent {
            id: crate::domain::EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        };
        assert_eq!(
            format_turns_excerpt(&[]),
            "(a headless session: no turn ended yet)"
        );
        let events = [
            event(event_kind::TURN_STARTED, json!({"turn": 1})),
            event(
                event_kind::TURN_FINISHED,
                json!({"turn": 1, "outcome": "silent", "failure": null, "permission_denials": 0}),
            ),
            event(
                event_kind::TURN_FINISHED,
                json!({"turn": 2, "outcome": "timed_out", "failure": null, "permission_denials": 0}),
            ),
            event(
                event_kind::TURN_FINISHED,
                json!({"turn": 3, "outcome": "failed", "failure": "authentication", "permission_denials": 3, "denied_tools": ["Bash", "Edit"], "message": "Not logged in"}),
            ),
        ];
        assert_eq!(
            format_turns_excerpt(&events),
            "(a headless session has no screen; its last turns, newest first)\nturn 3: failed (authentication); 3 permission denial(s) [Bash, Edit]: Not logged in\nturn 2: timed_out; 0 permission denial(s)\nturn 1: silent; 0 permission denial(s)"
        );
        let many: Vec<_> = (1..=6)
            .map(|turn| {
                event(
                    event_kind::TURN_FINISHED,
                    json!({"turn": turn, "outcome": "succeeded"}),
                )
            })
            .collect();
        let text = format_turns_excerpt(&many);
        assert_eq!(text.lines().count(), 6);
        assert!(!text.contains("turn 1:"));
        assert!(text.lines().nth(1).unwrap().starts_with("turn 6:"));
    }

    #[test]
    fn permission_denials_and_provider_failures_cover_every_outcome() {
        for outcome in [
            TurnOutcome::Succeeded,
            TurnOutcome::Failed,
            TurnOutcome::Silent,
            TurnOutcome::TimedOut,
            TurnOutcome::LaunchMismatch,
            TurnOutcome::Stopped,
        ] {
            for denials in [
                0,
                turn::PERMISSION_DENIAL_LIMIT - 1,
                turn::PERMISSION_DENIAL_LIMIT,
                turn::PERMISSION_DENIAL_LIMIT + 1,
            ] {
                assert_eq!(
                    alert_at_once(Some(mark(outcome, None, denials))),
                    (denials >= turn::PERMISSION_DENIAL_LIMIT).then_some(PERMISSION_DENIED)
                );
            }
            for failure in [
                None,
                Some(TurnFailure::Authentication),
                Some(TurnFailure::UsageLimit),
                Some(TurnFailure::Launch),
                Some(TurnFailure::Model),
                Some(TurnFailure::Sandbox),
            ] {
                let expected = failure.filter(|failure| {
                    outcome == TurnOutcome::Failed
                        && matches!(
                            failure,
                            TurnFailure::Authentication
                                | TurnFailure::UsageLimit
                                | TurnFailure::Launch
                        )
                });
                assert_eq!(provider_failure(Some(mark(outcome, failure, 10))), expected);
            }
        }
    }

    #[test]
    fn a_turn_refused_too_often_goes_to_its_recovery_job_at_once() {
        assert_eq!(alert_at_once(None), None);
        assert_eq!(
            alert_at_once(Some(mark(TurnOutcome::Succeeded, None, 2))),
            None
        );
        assert_eq!(
            alert_at_once(Some(mark(TurnOutcome::Succeeded, None, 3))),
            Some(PERMISSION_DENIED)
        );
    }

    #[test]
    fn only_a_login_a_usage_limit_or_a_start_is_the_providers() {
        assert_eq!(
            provider_failure(Some(mark(
                TurnOutcome::Failed,
                Some(TurnFailure::Authentication),
                0
            ))),
            Some(TurnFailure::Authentication)
        );
        assert_eq!(
            provider_failure(Some(mark(
                TurnOutcome::Failed,
                Some(TurnFailure::UsageLimit),
                0
            ))),
            Some(TurnFailure::UsageLimit)
        );
        assert_eq!(
            provider_failure(Some(mark(
                TurnOutcome::Failed,
                Some(TurnFailure::Launch),
                0
            ))),
            Some(TurnFailure::Launch)
        );
        assert_eq!(
            provider_failure(Some(mark(TurnOutcome::Failed, Some(TurnFailure::Model), 0))),
            None
        );
        assert_eq!(
            provider_failure(Some(mark(TurnOutcome::Succeeded, None, 0))),
            None
        );
        assert_eq!(provider_failure(None), None);
    }
}
