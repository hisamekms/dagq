//! A headless session's wrapper started without a workspace (ADR-t1404-1,
//! ADR-t1433-3): the log of a run's session, the flag that tells the
//! wrapper it starts in the background, and the record of its start the
//! wrapper waits for before it registers.

use super::*;
use crate::application::prompt::PromptBytes;
use crate::domain::background_wrapper::{
    BACKGROUND_FLAG, BackgroundHandle, HeadlessWrapper, StopRoute, is_background, launch_of,
    session_log_name, wrapper_is_recorded,
};

impl Supervisor<'_> {
    /// The log of a run's session wrapper, which always starts in the
    /// background (ADR-t1433-3 decision 1): `session.log` in `run_dir`, or
    /// the log of the resume or, with `reopen`, the reopening `resume`.
    /// `[headless] wrapper` of `dagq.toml` is accepted and ignored for a
    /// worker (decision 2); [`Self::warn_ignored_wrapper_setting`] says so.
    pub(super) fn session_log(
        &self,
        run_dir: &Path,
        resume: Option<usize>,
        reopen: bool,
    ) -> PathBuf {
        run_dir.join(session_log_name(resume, reopen))
    }

    /// Warn once per supervisor that `[headless] wrapper = "workspace"` of
    /// `dagq.toml` does not open a worker's session in a workspace any
    /// more: the setting is accepted and ignored, so that a `dagq.toml`
    /// that still has it keeps loading (ADR-t1433-3 decision 2). Until the
    /// warning is given, the setting is read as each session starts, so a
    /// `dagq.toml` that comes to say `"workspace"` later is warned of too.
    /// A planner of the runtime's ignores it as well: its wrapper always
    /// starts in the background (ADR-t1433-2 decision 3).
    pub(super) fn warn_ignored_wrapper_setting(&mut self) {
        let warn = warns_of_ignored_setting(self.wrapper_setting_warned, || {
            self.verifier.headless_wrapper_setting()
        });
        if warn {
            self.wrapper_setting_warned = true;
            warn!(
                "[headless] wrapper = \"workspace\" of dagq.toml is ignored for workers: a worker's session wrapper always starts in the background"
            );
        }
    }

    /// Whether the process of `wrapper` lives: its pid is alive and, for a
    /// wrapper started in the background, shows the start recorded at its
    /// launch (ADR-t1404-1 decision 2), so that a process that took the pid
    /// of a wrapper that died is not taken for it (nor made a silent
    /// wrapper, nor kept from a resume). A wrapper in a workspace is told
    /// by its pid, as before; one whose records cannot be read too.
    pub(super) fn wrapper_lives(&self, wrapper: &RunProcess) -> bool {
        self.processes.alive(wrapper.pid)
            && match self.queue.run_events(&wrapper.run_id) {
                Ok(events) if launch_of(&events, wrapper.pid).is_some() => wrapper_is_recorded(
                    &events,
                    wrapper.pid,
                    self.processes.start_identity(wrapper.pid).as_deref(),
                ),
                _ => true,
            }
    }

    /// Whether the session `id` a run recorded still runs
    /// ([`run_session_open`]).
    pub(super) fn run_session_open(&self, id: &str) -> Result<bool> {
        run_session_open(self.sessions, id)
    }

    /// Whether the session `id` a run recorded is known to have ended
    /// ([`run_session_gone`]).
    pub(super) fn run_session_gone(&self, id: &str) -> Result<bool> {
        run_session_gone(self.sessions, id)
    }

    /// Record `wrapper_launched` for the background wrapper `handle` of
    /// `run`, after the run recorded the handle as its session: the
    /// wrapper registers once it finds this record with its own pid. A
    /// record that cannot be made stops the wrapper, which nothing would
    /// find. Nothing for a handle that is not a background wrapper's (a
    /// test backend's).
    pub(super) fn record_launch(
        &mut self,
        run: &TaskRun,
        handle: &str,
        log: &Path,
        prompt_bytes: Option<&PromptBytes>,
    ) -> Result<()> {
        let Some(parsed) = BackgroundHandle::parse(handle) else {
            return Ok(());
        };
        let mut payload = json!({
            "pid": parsed.pid,
            "start": parsed.start,
            "workspace_id": handle,
            "log": log.to_string_lossy(),
        });
        if let Some(bytes) = prompt_bytes {
            payload["prompt_bytes"] = json!(bytes);
        }
        let recorded =
            self.queue
                .record_runtime_event(run.id(), EventKind::WrapperLaunched, payload);
        if let Err(error) = recorded {
            return Err(match stop_session(self.sessions, handle, StopRoute::Unrecorded) {
                Ok(()) => error.context(format!(
                    "the start of the background wrapper {handle} of run {} could not be recorded, and it was stopped",
                    run.id()
                )),
                Err(stop) => error.context(format!(
                    "the start of the background wrapper {handle} of run {} could not be recorded, and stopping it failed: {stop:#}",
                    run.id()
                )),
            });
        }
        info!(run_id = %run.id(), "the session wrapper of run {} runs in the background as {handle}; its output goes to {}", run.id(), log.display());
        Ok(())
    }
}

/// Whether `[headless] wrapper` as `dagq.toml` writes it (`setting`, read
/// only while nothing was `warned` yet) calls for the warning that a worker
/// ignores it (ADR-t1433-3 decision 2): once per supervisor, and only for
/// `"workspace"`. No key, `"background"` and a setting that cannot be read
/// warn nothing here (an unreadable `dagq.toml` is reported where it is
/// read for `[run.env]`).
fn warns_of_ignored_setting(
    warned: bool,
    setting: impl FnOnce() -> Result<Option<HeadlessWrapper>>,
) -> bool {
    !warned && matches!(setting(), Ok(Some(HeadlessWrapper::Workspace)))
}

/// `args`, the session wrapper's command, with the flag of a background
/// start, as one shell command line.
pub(super) fn wrapper_command(mut args: Vec<String>) -> String {
    args.push(BACKGROUND_FLAG.into());
    shell_join(&args)
}

/// The turn the background session `handle` (of the run `run`) recorded
/// starting that still runs: its pid shows the start recorded with it
/// (ADR-t1404-1 decision 3). A turn leads a process group of its own, so
/// it outlives a wrapper that died (killed, say) without stopping it;
/// whatever stops the session stops such a turn by this record.
pub(crate) fn left_turn(
    queue: &dyn Queue,
    processes: &dyn ProcessControl,
    run: &RunId,
    handle: &str,
) -> Option<BackgroundHandle> {
    let events = queue.run_events(run).ok()?;
    let turn = crate::domain::background_wrapper::last_turn_of(&events, handle)?;
    turn.is(turn.pid, processes.start_identity(turn.pid).as_deref())
        .then_some(turn)
}

/// The turn the headless planner whose background handle is `handle`
/// recorded starting that still runs (ADR-t1404-1 decisions 3 and 8): its
/// pid shows the start recorded with it. Its turns are the queue's events
/// that name it (ADR-t1394-2), not a run's; a planner whose row closed is
/// found too, so a close after it still stops the turn.
pub(crate) fn left_planner_turn(
    queue: &dyn Queue,
    processes: &dyn ProcessControl,
    handle: &str,
) -> Option<BackgroundHandle> {
    let planner = queue
        .planners(true)
        .ok()?
        .into_iter()
        .find(|planner| planner.workspace_id.as_deref() == Some(handle))?;
    let events = queue.planner_turn_events(planner.id).ok()?;
    let turn = crate::domain::background_wrapper::last_planner_turn(&events)?;
    turn.is(turn.pid, processes.start_identity(turn.pid).as_deref())
        .then_some(turn)
}

/// Stop the turn `turn` left by a background wrapper that is gone: what it
/// started (listed first, since they are init's once it dies, and each
/// killed only while its pid shows the start it was listed with), then its
/// process group. Waits up to [`TURN_STOP_WAIT`] for it to be gone.
pub(crate) fn stop_left_turn(
    processes: &dyn ProcessControl,
    turn: &BackgroundHandle,
) -> Result<()> {
    let started: Vec<(u32, Option<String>)> = processes
        .descendants(turn.pid)
        .into_iter()
        .map(|pid| (pid, processes.start_identity(pid)))
        .collect();
    let alive =
        |turn: &BackgroundHandle| turn.is(turn.pid, processes.start_identity(turn.pid).as_deref());
    if !alive(turn) {
        return Ok(());
    }
    let _ = processes.kill_group(turn.pid);
    let _ = processes.kill(turn.pid);
    for (pid, start) in &started {
        if start.is_some() && processes.start_identity(*pid) == *start {
            let _ = processes.kill_group(*pid);
            let _ = processes.kill(*pid);
        }
    }
    let waited = Instant::now();
    while alive(turn) {
        ensure!(
            waited.elapsed() < TURN_STOP_WAIT,
            "the turn {} the background wrapper left is still running after SIGKILL",
            turn.pid
        );
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// How long the processes of a turn left by its wrapper, sent SIGKILL, are
/// given to be gone.
const TURN_STOP_WAIT: Duration = Duration::from_secs(5);

/// Whether the session `id` a run recorded still runs: a background
/// wrapper's handle while its process runs (`exists` on the handle, which
/// asks no cmux). A workspace ID, a session opened in a workspace before
/// ADR-t1433-3, counts as not open: cmux is not asked for a run's session
/// any more, and a person closes such a workspace in their own terminal
/// (decision 3).
pub(crate) fn run_session_open(sessions: &dyn SessionWrappers, id: &str) -> Result<bool> {
    if !is_background(id) {
        return Ok(false);
    }
    sessions.exists(id)
}

/// Whether the session `id` a run recorded is known to have ended, for an
/// exit to be asked of it: a background wrapper's handle whose process no
/// longer runs (`exists` on the handle, which asks no cmux). A workspace
/// ID, a session opened in a workspace before ADR-t1433-3, is not asked of
/// cmux and is not known to have ended: its wrapper's registration says
/// whether it runs, and the exit asked is a file in the run dir that the
/// wrapper reads wherever it runs.
pub(crate) fn run_session_gone(sessions: &dyn SessionWrappers, id: &str) -> Result<bool> {
    if !is_background(id) {
        return Ok(false);
    }
    Ok(!sessions.exists(id)?)
}

/// Stop the session `id` a run recorded: its background wrapper and what
/// it started (`stop_background` on the handle, which asks no cmux,
/// ADR-t1404-1 decision 3), recorded as `wrapper_stopped` with `route`
/// (task 1657). A workspace ID, a session opened in a workspace before
/// ADR-t1433-3, is not closed: cmux is not called for a run's session any
/// more, and a person closes the workspace in their own terminal
/// (decision 3); it counts as stopped.
pub(crate) fn stop_run_session(
    sessions: &dyn SessionWrappers,
    id: &str,
    route: StopRoute,
) -> Result<()> {
    if !is_background(id) {
        info!(
            "the workspace {id} of a session an older binary opened is left to a person to close"
        );
        return Ok(());
    }
    sessions.stop_background(id, route).map(drop)
}

/// Stop the session `id`: a background wrapper's handle by
/// `stop_background`, recorded as `wrapper_stopped` with `route` (task
/// 1657). A workspace ID is not closed: the supervisor calls no cmux
/// (ADR-t1433-1), so a workspace an older binary opened is left to a
/// person to close in their own terminal, and it counts as stopped.
pub(crate) fn stop_session(
    sessions: &dyn SessionWrappers,
    id: &str,
    route: StopRoute,
) -> Result<()> {
    if is_background(id) {
        sessions.stop_background(id, route).map(drop)
    } else {
        info!("the workspace {id} is left to a person to close: the supervisor calls no cmux");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Session wrappers that record the `exists` and the stops they are
    /// asked, say a session is open (ended with `ended`), and start none.
    #[derive(Default)]
    struct Sessions {
        calls: Mutex<Vec<String>>,
        ended: bool,
    }

    impl Sessions {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SessionWrappers for Sessions {
        fn launch_background(
            &self,
            _: &Path,
            _: &str,
            _: &[(String, String)],
            _: &Path,
        ) -> Result<String> {
            unimplemented!()
        }
        fn stop_background(
            &self,
            id: &str,
            route: StopRoute,
        ) -> Result<Option<crate::domain::background_wrapper::WrapperStop>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("stop {id} {}", route.as_str()));
            Ok(None)
        }
        fn exists(&self, id: &str) -> Result<bool> {
            self.calls.lock().unwrap().push(format!("exists {id}"));
            Ok(!self.ended)
        }
    }

    /// A run's background handle is judged and stopped through the
    /// backend's `exists` and `stop_background` on the handle (which ask
    /// no cmux), with the route the stop is recorded with (task 1657);
    /// a workspace ID from before ADR-t1433-3 is never asked of the
    /// backend: it counts as not open, and as stopped, left to a person
    /// (decision 3).
    #[test]
    fn a_pre_adr_workspace_of_a_run_is_never_asked_of_the_backend() {
        let sessions = Sessions::default();
        let handle = "background:4242:Mon_Oct__5_10:00:00_2026";
        assert!(run_session_open(&sessions, handle).unwrap());
        stop_run_session(&sessions, handle, StopRoute::AfterReview).unwrap();
        assert_eq!(
            sessions.calls(),
            [
                format!("exists {handle}"),
                format!("stop {handle} after_review")
            ]
        );
        let sessions = Sessions::default();
        let workspace = "01234567-89AB-4DEF-8123-000000000000";
        assert!(!run_session_open(&sessions, workspace).unwrap());
        stop_run_session(&sessions, workspace, StopRoute::AfterReview).unwrap();
        assert!(sessions.calls().is_empty(), "{:?}", sessions.calls());
    }

    /// A session given up on is asked to exit unless it is known to have
    /// ended: a background handle whose process is gone. A workspace ID
    /// from before ADR-t1433-3 is not asked of the backend and is not known
    /// to have ended, so its still registered wrapper is asked to exit
    /// through the run dir.
    #[test]
    fn only_a_background_session_is_known_to_have_ended_without_its_registration() {
        let handle = "background:4242:Mon_Oct__5_10:00:00_2026";
        let sessions = Sessions::default();
        assert!(!run_session_gone(&sessions, handle).unwrap());
        let ended = Sessions {
            ended: true,
            ..Sessions::default()
        };
        assert!(run_session_gone(&ended, handle).unwrap());
        assert_eq!(ended.calls(), [format!("exists {handle}")]);
        let workspace = "01234567-89AB-4DEF-8123-000000000000";
        assert!(!run_session_gone(&ended, workspace).unwrap());
        assert_eq!(ended.calls(), [format!("exists {handle}")]);
    }

    /// `stop_session` stops a background handle by `stop_background` with
    /// its route, and leaves a workspace to a person: it calls no cmux.
    #[test]
    fn a_session_is_stopped_by_its_kind() {
        let sessions = Sessions::default();
        let handle = "background:4242:Mon_Oct__5_10:00:00_2026";
        stop_session(&sessions, handle, StopRoute::Sweep).unwrap();
        // A workspace an older binary opened is not closed: no cmux.
        stop_session(&sessions, "WS-1", StopRoute::Planner).unwrap();
        assert_eq!(sessions.calls(), [format!("stop {handle} sweep")]);
    }

    /// Only `"workspace"` is warned of, and only once: a supervisor that
    /// warned does not read the setting again (ADR-t1433-3 decision 2).
    #[test]
    fn only_a_workspace_setting_is_warned_of_and_only_once() {
        assert!(warns_of_ignored_setting(false, || Ok(Some(
            HeadlessWrapper::Workspace
        ))));
        for setting in [
            Ok(Some(HeadlessWrapper::Background)),
            Ok(None),
            Err(anyhow::anyhow!("dagq.toml:3: value of wrapper")),
        ] {
            assert!(!warns_of_ignored_setting(false, || setting));
        }
        assert!(!warns_of_ignored_setting(true, || unreachable!(
            "the setting is not read again once warned"
        )));
    }
}
