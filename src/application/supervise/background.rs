//! A headless session's wrapper started without a workspace (ADR-t1404-1):
//! whether the session of a run is started in the background, the flag
//! that tells the wrapper so, and the record of its start the wrapper
//! waits for before it registers.

use super::*;
use crate::domain::background_wrapper::{
    BACKGROUND_FLAG, BackgroundHandle, HeadlessWrapper, is_background, launch_of, session_log_name,
    wrapper_is_recorded,
};

impl Supervisor<'_> {
    /// The log of `run`'s session wrapper when it is started in the
    /// background: a headless run under `[headless] wrapper =
    /// "background"`, read now, as the wrapper starts (ADR-t1404-1
    /// decision 7). `resume` is the attempt of a resume or, with
    /// `reopen`, of a reopening. `None` starts it in a workspace, as does
    /// a setting that cannot be read.
    pub(super) fn background_log(
        &self,
        run: &TaskRun,
        run_dir: &Path,
        resume: Option<usize>,
        reopen: bool,
    ) -> Option<PathBuf> {
        match self.verifier.headless_wrapper() {
            Ok(HeadlessWrapper::Background) => Some(run_dir.join(session_log_name(resume, reopen))),
            Ok(HeadlessWrapper::Workspace) => None,
            Err(error) => {
                warn!(run_id = %run.id(), "[headless] of dagq.toml could not be read, so the session of run {} opens in a workspace: {error:#}", run.id());
                None
            }
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

    /// The group of a session's workspace: none for one started in the
    /// background, which cmux is not asked for.
    pub(super) fn session_group(&self, background: Option<&Path>) -> Option<String> {
        match background {
            Some(_) => None,
            None => self.workspace_group(),
        }
    }

    /// Record `wrapper_launched` for the background wrapper `handle` of
    /// `run`, after the run recorded the handle as its session's
    /// workspace: the wrapper registers once it finds this record with its
    /// own pid. A record that cannot be made stops the wrapper, which
    /// nothing would find. Nothing for a workspace.
    pub(super) fn record_launch(
        &mut self,
        run: &TaskRun,
        handle: &str,
        log: Option<&Path>,
    ) -> Result<()> {
        let (Some(log), Some(parsed)) = (log, BackgroundHandle::parse(handle)) else {
            return Ok(());
        };
        let recorded = self.queue.record_runtime_event(
            run.id(),
            EventKind::WrapperLaunched,
            json!({
                "pid": parsed.pid,
                "start": parsed.start,
                "workspace_id": handle,
                "log": log.to_string_lossy(),
            }),
        );
        if let Err(error) = recorded {
            return Err(match self.cmux.close(handle) {
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

/// `args`, the session wrapper's command, with the flag of a background
/// start when there is a `background` log, as one shell command line.
pub(super) fn wrapper_command(mut args: Vec<String>, background: Option<&Path>) -> String {
    if background.is_some() {
        args.push(BACKGROUND_FLAG.into());
    }
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

/// Whether the session `id` a run recorded is still open: a workspace
/// while cmux lists it in `listed`, a background wrapper's handle while
/// its process runs (`exists` on the handle; one that cannot be judged
/// counts as gone, as a workspace missing from the list does). A
/// background handle is judged without the list, so a listing that
/// failed (`None`) leaves only the workspaces unjudged, counted as not
/// open for the caller to report.
pub(crate) fn still_open(cmux: &dyn WorkspaceBackend, listed: Option<&[String]>, id: &str) -> bool {
    if is_background(id) {
        return matches!(cmux.exists(id), Ok(true));
    }
    listed.is_some_and(|listed| listed.iter().any(|listed| listed.eq_ignore_ascii_case(id)))
}
