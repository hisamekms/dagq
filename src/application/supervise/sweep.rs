//! host運用's sweep of the sessions of ended runs: it stops the background
//! wrappers still running of the runs the triage never takes (task 180,
//! ADR-t1433-3 decision 3), with its own state ([`SweepWatch`]). It decides
//! when and which wrapper; the stop and its `workspace_closed`, 実行と着地's
//! record, are 実行と着地's ([`stop_swept_wrapper`] in `background`, beside
//! [`close_open_workspaces`], which the triage, a resume or a landing
//! calls). The loop starts the sweep when it is due and,
//! in the same pass, asks for the disk of ended runs to be freed (task 376)
//! and has 計画管理 sweep its planners. cmux is not called for a run: a
//! workspace of a session from before ADR-t1433-3 is left to a person.

use super::background::{record_close_failure, run_session_open, stop_swept_wrapper};
use super::*;
use crate::application::EndedRunWorkspace;

/// The build outputs removed from the worktree of an ended run: these
/// directories directly under the worktree, unless Git tracks a file in
/// them. `cargo llvm-cov` builds under `target/llvm-cov-target` unless told
/// otherwise.
pub(super) const BUILD_OUTPUT_DIRS: &[&str] = &["target", "llvm-cov-target"];

/// The sweep between passes.
#[derive(Default)]
pub(super) struct SweepWatch {
    /// When this process last swept the workspaces of ended runs
    /// (`LoopSettings::sweep_interval`); `None` until the first pass sweeps.
    last: Option<Instant>,
    /// The workspaces the sweep could not close: retried on every sweep,
    /// their `cleanup_failed` recorded once per process.
    failures: Vec<String>,
}

/// Whether the sweep is due at `now`: at once on the first pass (`last`
/// none), then once `interval` passed since the last one.
pub(super) fn sweep_due(last: Option<Instant>, now: Instant, interval: Duration) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= interval)
}

impl SweepWatch {
    /// Start a sweep at `now` when one is due ([`sweep_due`]); whether it
    /// starts. At most once per `interval`, and at once on the first pass.
    pub(super) fn start(&mut self, now: Instant, interval: Duration) -> bool {
        if !sweep_due(self.last, now, interval) {
            return false;
        }
        self.last = Some(now);
        true
    }

    /// Stop the background wrappers still running of ended runs the triage
    /// never takes (task 180, ADR-t1404-1 decision 3): runs superseded in
    /// their task, runs of a task that moved on, and landed runs, however
    /// they ended (a hand `integrate` or `recover` included), but those the
    /// slots hold (`held`). Whether a wrapper runs decides
    /// ([`run_session_open`]), not the recorded stops; one that does not
    /// run gets no event, and a workspace of a session from before
    /// ADR-t1433-3 is not asked of cmux (a person closes it). A stop
    /// records `workspace_closed` (`by: supervisor`, `reason` `superseded`
    /// or `ended`) and closes the run's `stuck_exit`, `answer_prompt` and
    /// `stalled` asks, ending its stalled detections with no end
    /// (`stall_resolved`, `run_ended`); a failure records `cleanup_failed`
    /// (once per wrapper and process; the stop is retried on every sweep)
    /// and the others go on. Worktrees, branches and run directories stay
    /// for a person.
    pub(super) fn sweep_ended_sessions(
        &mut self,
        env: &mut PassEnv<'_>,
        held: &[RunId],
    ) -> Result<()> {
        let candidates: Vec<EndedRunWorkspace> = env
            .queue
            .ended_run_workspaces()?
            .into_iter()
            .filter(|w| !held.contains(&w.run_id))
            .collect();
        let mut closed_runs: Vec<RunId> = Vec::new();
        for candidate in candidates {
            if !matches!(
                run_session_open(env.sessions, &candidate.workspace_id),
                Ok(true)
            ) {
                continue;
            }
            let workspace = &candidate.workspace_id;
            match stop_swept_wrapper(&mut *env.queue, env.sessions, &candidate)? {
                Ok(()) => {
                    if !closed_runs.contains(&candidate.run_id) {
                        closed_runs.push(candidate.run_id);
                    }
                }
                // Retried on the next sweep; recorded once.
                Err(error) if self.failures.contains(workspace) => {
                    warn!(run_id = %candidate.run_id, "run {}: the wrapper {workspace} still could not be stopped: {error:#}", candidate.run_id);
                }
                Err(error) => {
                    self.failures.push(workspace.clone());
                    record_close_failure(
                        &mut *env.queue,
                        &candidate.run_id,
                        workspace,
                        WorkspaceCloser::Runtime,
                        &error,
                    )?;
                }
            }
        }
        for run_id in closed_runs {
            let answer = "the run ended; the runtime stopped its session";
            env.queue.close_stuck_exit_asks(&run_id, answer)?;
            env.queue.close_answer_prompt_asks(&run_id, answer)?;
            env.queue.end_stalled_detections(&run_id, answer)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sweep is due at once on the first pass, then not 1 ms before
    /// its interval since the last one, and at it.
    #[test]
    fn the_sweep_is_due_first_and_then_once_its_interval_passed() {
        let interval = Duration::from_secs(60);
        let now = Instant::now();
        assert!(sweep_due(None, now, interval));
        let last = now.checked_sub(interval).unwrap();
        assert!(sweep_due(Some(last), now, interval));
        let last = now
            .checked_sub(interval - Duration::from_millis(1))
            .unwrap();
        assert!(!sweep_due(Some(last), now, interval));
        // A clock that reads earlier than the last sweep is no sweep.
        assert!(!sweep_due(Some(now + interval), now, interval));
        let mut watch = SweepWatch::default();
        assert!(watch.start(now, interval));
        assert!(!watch.start(now, interval));
        assert!(watch.start(now + interval, interval));
    }
}
