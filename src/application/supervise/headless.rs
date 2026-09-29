//! A headless worker's session as the supervisor sees it (ADR-t813-1): what
//! it would type into an interactive session is written as the session's
//! next request in the run's `turns/` directory, and `/exit` as its exit
//! request ([`request_turn`]); the session wrapper runs each request as a
//! resume of the same session and writes the idle marker when the turn
//! ended. The screen is never read for such a session (no dialog, input box
//! or idle to infer), and a request needs no check that it was taken: the
//! turn that runs it starts and ends. How the turns went is read from the
//! idle marker ([`TurnMark`]) and the `turn_finished` events.

use super::*;
use crate::domain::EventKind;
use crate::domain::recovery::PERMISSION_DENIED;
use crate::domain::turn::{
    self, LIMITS_FILE, TurnFailure, TurnMark, TurnOutcome, TurnRequest, exit_path, next_seq,
    request_path, turns_dir,
};
use crate::domain::worker::WorkerMode;

/// Whether `run`'s worker runs headless.
pub(super) fn headless(run: &TaskRun) -> bool {
    run.worker_mode() == WorkerMode::Headless
}

/// Write `input` for the headless session of `run` (in `workspace`, for
/// the records): a text becomes its next request, recorded as
/// `turn_requested`, and `/exit` its exit request. A failed write is a
/// failed typing.
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
    let text = match input {
        Input::Exit => {
            sv.files.write(&exit_path(run_dir), b"")?;
            info!(run_id = %run.id(), "exit requested of the headless session of {}", run.id());
            return Ok(Submission::Queued);
        }
        Input::Text(text) => text,
    };
    let names: Vec<String> = sv
        .files
        .read_dir(&dir)?
        .iter()
        .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
        .collect();
    let seq = next_seq(names.iter().map(String::as_str));
    let request = TurnRequest {
        seq,
        what: what.to_owned(),
        prompt: text.to_owned(),
    };
    let path = request_path(run_dir, seq);
    let tmp = path.with_extension("json.tmp");
    sv.files
        .write(&tmp, serde_json::to_string(&request)?.as_bytes())?;
    sv.files.rename(&tmp, &path)?;
    // Written: a record that fails is only noted, as for a typed text.
    if let Err(error) = sv.queue.record_runtime_event(
        run.id(),
        EventKind::TurnRequested,
        json!({"seq": seq, "what": what, "workspace_id": workspace}),
    ) {
        warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "turn_requested of {} could not be recorded: {error:#}", run.id());
    }
    info!(run_id = %run.id(), "{what} requested of the headless session of {} (request {seq})", run.id());
    Ok(Submission::Queued)
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
        .is_some_and(|dir| sv.files.exists(&exit_path(Path::new(dir))))
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

impl Supervisor<'_> {
    /// Before a headless session of `run` starts in its run directory: its
    /// turn limits from the `[stall]` settings, and neither the exit request
    /// nor a request an earlier session left untaken: the new session
    /// starts from the supervisor's own request. Nothing for an
    /// interactive run.
    pub(super) fn prepare_turns(&self, run: &TaskRun, run_dir: &Path) -> Result<()> {
        if !headless(run) {
            return Ok(());
        }
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

/// What the recovery job of a headless session reads instead of its
/// screen: its last turns, as recorded.
pub(super) fn turns_excerpt(sv: &Supervisor<'_>, run: &TaskRun) -> String {
    let events = match sv.queue.run_events(run.id()) {
        Ok(events) => events,
        Err(error) => return format!("(the turns could not be read: {error:#})"),
    };
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
pub(super) fn provider_failure(mark: Option<TurnMark>) -> Option<TurnFailure> {
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
