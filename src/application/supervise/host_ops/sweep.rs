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
//! The same sweep prunes the registrations other supervisors of the queue
//! left behind ([`prune_left_supervisors`]).

use super::background::{record_close_failure, stop_swept_wrapper};
use super::*;
use crate::application::{
    EndedRunWorkspace, LAST_TURNS_PAGE, LastSessionTurn, RunReads, RunSessionPair,
    SupervisorRegistry, lifecycle,
};
use crate::domain::{recovery::ProcessInfo, supervisor_prune};
use std::collections::{HashSet, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};

/// The sweep between passes.
#[derive(Default)]
pub(in crate::application::supervise) struct SweepWatch {
    /// When this process last swept the workspaces of ended runs
    /// (`LoopSettings::sweep_interval`); `None` until the first pass sweeps.
    last: Option<Instant>,
    /// A pid/start identity never becomes alive again. Keep a 64-bit hash
    /// of each confirmed gone (run, handle), not the handle alone. This
    /// costs tens of bytes per ended run rather than retaining event
    /// payloads (the GB-scale history memory problem); it is process-local
    /// and makes subsequent sweeps independent of checked history.
    gone: HashSet<u64>,
    /// The workspaces the sweep could not close: retried on every sweep,
    /// their `cleanup_failed` recorded once per process.
    failures: Vec<String>,
}

/// Whether the sweep is due at `now`: at once on the first pass (`last`
/// none), then once `interval` passed since the last one.
pub(in crate::application::supervise) fn sweep_due(
    last: Option<Instant>,
    now: Instant,
    interval: Duration,
) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= interval)
}

impl SweepWatch {
    /// Start a sweep at `now` when one is due ([`sweep_due`]); whether it
    /// starts. At most once per `interval`, and at once on the first pass.
    pub(in crate::application::supervise) fn start(
        &mut self,
        now: Instant,
        interval: Duration,
    ) -> bool {
        if !sweep_due(self.last, now, interval) {
            return false;
        }
        self.last = Some(now);
        true
    }

    /// Whether this failed stop needs its one cleanup_failed record.
    /// A stop failure never certifies that the session is gone.
    fn note_stop_failure(&mut self, candidate: &EndedRunWorkspace) -> bool {
        if self.failures.contains(&candidate.workspace_id) {
            return false;
        }
        self.failures.push(candidate.workspace_id.clone());
        true
    }

    /// Collect only live sessions. A failed wrapper query or page remains
    /// unchecked for the next sweep. The stop path deliberately retains
    /// its existing per-live-session reads and failure recording.
    fn live_candidates(
        &mut self,
        queue: &dyn RunReads,
        held: &[RunId],
        wrapper_exists: impl Fn(&str) -> Result<bool>,
        turn_alive: impl Fn(&LastSessionTurn) -> Result<bool>,
    ) -> Result<Vec<EndedRunWorkspace>> {
        let mut live = Vec::new();
        let mut pending = Vec::new();
        for candidate in queue.ended_run_workspaces()? {
            let key = session_key(&candidate);
            if sweep_decision(
                held.contains(&candidate.run_id),
                self.gone.contains(&key),
                None,
                None,
            ) == SweepDecision::Skip
            {
                continue;
            }
            if !crate::domain::background_wrapper::is_background(&candidate.workspace_id) {
                continue;
            }
            let wrapper = wrapper_exists(&candidate.workspace_id).ok();
            match sweep_decision(false, false, wrapper, None) {
                SweepDecision::Stop => live.push(candidate),
                SweepDecision::ReadTurn => {
                    let pair = RunSessionPair {
                        run_id: candidate.run_id.clone(),
                        workspace_id: candidate.workspace_id.clone(),
                    };
                    if pair.fits_sweep_read() {
                        pending.push((candidate, pair));
                    } else {
                        self.gone.insert(key);
                    }
                }
                _ => {}
            }
        }
        for page in pending.chunks(LAST_TURNS_PAGE) {
            let pairs: Vec<_> = page.iter().map(|(_, pair)| pair.clone()).collect();
            let Ok(turns) = queue.last_turns_of(&pairs) else {
                continue;
            };
            for (candidate, _) in page {
                let alive = turns
                    .iter()
                    .find(|turn| {
                        turn.run_id == candidate.run_id
                            && turn.workspace_id == candidate.workspace_id
                    })
                    .map_or(Some(false), |turn| turn_alive(turn).ok());
                match sweep_decision(false, false, Some(false), alive) {
                    SweepDecision::Stop => live.push(candidate.clone()),
                    SweepDecision::Remember => {
                        self.gone.insert(session_key(candidate));
                    }
                    _ => {}
                }
            }
        }
        Ok(live)
    }

    /// Stop the background wrappers still running of ended runs the triage
    /// never takes (task 180, ADR-t1404-1 decision 3): runs superseded in
    /// their task, runs of a task that moved on, and landed runs, however
    /// they ended (a hand `integrate` or `recover` included), but those the
    /// slots hold (`held`). Wrapper and remaining turn liveness decide,
    /// not the recorded stops; a session with neither gets no event, and a workspace of a session from before
    /// ADR-t1433-3 is not asked of cmux (a person closes it). A stop
    /// records `workspace_closed` (`by: supervisor`, `reason` `superseded`
    /// or `ended`) and closes the run's `stuck_exit`, `answer_prompt` and
    /// `stalled` asks, ending its stalled detections with no end
    /// (`stall_resolved`, `run_ended`); a failure records `cleanup_failed`
    /// (once per wrapper and process; the stop is retried on every sweep)
    /// and the others go on. Worktrees, branches and run directories stay
    /// for a person.
    pub(in crate::application::supervise) fn sweep_ended_sessions(
        &mut self,
        env: &mut PassEnv<'_>,
        held: &[RunId],
    ) -> Result<()> {
        let candidates = self.live_candidates(
            &*env.queue,
            held,
            |handle| env.sessions.wrapper_exists(handle),
            |turn| {
                let handle = crate::domain::background_wrapper::BackgroundHandle {
                    pid: turn.pid,
                    start: turn.start.clone(),
                };
                env.processes.confirmed_identity_alive(&handle)
            },
        )?;
        let mut closed_runs: Vec<RunId> = Vec::new();
        for candidate in candidates {
            let workspace = &candidate.workspace_id;
            match stop_swept_wrapper(&mut *env.queue, env.sessions, &candidate)? {
                Ok(()) => {
                    if !closed_runs.contains(&candidate.run_id) {
                        closed_runs.push(candidate.run_id);
                    }
                }
                Err(error) => {
                    if self.note_stop_failure(&candidate) {
                        record_close_failure(
                            &mut *env.queue,
                            &candidate.run_id,
                            workspace,
                            WorkspaceCloser::Runtime,
                            &error,
                        )?;
                    } else {
                        warn!(run_id = %candidate.run_id, "run {}: the wrapper {workspace} still could not be stopped: {error:#}", candidate.run_id);
                    }
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

fn session_key(candidate: &EndedRunWorkspace) -> u64 {
    let mut hash = DefaultHasher::new();
    (candidate.run_id.as_str(), &candidate.workspace_id).hash(&mut hash);
    hash.finish()
}

#[derive(Debug, PartialEq, Eq)]
enum SweepDecision {
    Skip,
    Retry,
    ReadTurn,
    Stop,
    Remember,
}

/// Unknown liveness (including a failed read) never proves a session gone.
fn sweep_decision(
    held: bool,
    remembered: bool,
    wrapper: Option<bool>,
    turn: Option<bool>,
) -> SweepDecision {
    match (held, remembered, wrapper, turn) {
        (true, _, _, _) | (_, true, _, _) => SweepDecision::Skip,
        (_, _, Some(true), _) | (_, _, Some(false), Some(true)) => SweepDecision::Stop,
        (_, _, Some(false), Some(false)) => SweepDecision::Remember,
        (_, _, Some(false), None) => SweepDecision::ReadTurn,
        _ => SweepDecision::Retry,
    }
}

/// Prune the registrations other supervisors of the queue left behind
/// ([`supervisor_prune::left_behind`]): the heartbeat expired and the pid
/// runs nothing or was taken over, as a supervisor the host's shutdown
/// ended before it removed its own leaves. Each prune records its
/// `supervisor_stopped` as `up` and `down` do, in the transaction that
/// removes the row ([`lifecycle::prune_supervisor`]); nothing goes to the
/// inbox. This process's own registration (`own`), and one with a fresh
/// heartbeat whatever its pid, are kept. This user's processes are listed
/// only for a silent registration whose pid lives, once per sweep. A
/// prune that fails is logged and tried again on the next sweep.
pub(in crate::application::supervise) fn prune_left_supervisors(
    queue: &(impl SupervisorRegistry + ?Sized),
    processes: &dyn ProcessControl,
    own: &LeaseToken,
    now: i64,
) -> Result<()> {
    let mut listing: Option<Option<Vec<ProcessInfo>>> = None;
    for registration in queue.supervisors()? {
        if registration.token == *own || !supervisor_prune::heartbeat_expired(&registration, now) {
            continue;
        }
        let alive = processes.alive(registration.pid);
        let listed = if alive {
            listing
                .get_or_insert_with(|| processes.list().ok())
                .as_deref()
        } else {
            None
        };
        if !supervisor_prune::left_behind(&registration, now, alive, listed) {
            continue;
        }
        match lifecycle::prune_supervisor(queue, &registration) {
            Ok(()) => {
                info!(supervisor = %registration.token, pid = registration.pid, "pruned the registration supervisor {} left behind (pid {})", registration.token, registration.pid);
            }
            Err(error) => {
                warn!(supervisor = %registration.token, error = %format_args!("{error:#}"), "the registration supervisor {} left behind could not be pruned: {error:#}", registration.token);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::application::EndedRunWorktree;
    use std::cell::Cell;

    struct ReadQueue {
        candidates: Vec<EndedRunWorkspace>,
        ended_reads: Cell<usize>,
        turn_reads: Cell<usize>,
        fail: Cell<bool>,
        turns: Vec<LastSessionTurn>,
    }

    fn candidate(run: &str, handle: &str) -> EndedRunWorkspace {
        EndedRunWorkspace {
            run_id: RunId::new(run).unwrap(),
            workspace_id: handle.into(),
            status: RunStatus::Integrated,
        }
    }

    fn queue(candidates: Vec<EndedRunWorkspace>) -> ReadQueue {
        ReadQueue {
            candidates,
            ended_reads: Cell::new(0),
            turn_reads: Cell::new(0),
            fail: Cell::new(false),
            turns: Vec::new(),
        }
    }

    #[test]
    fn checked_history_needs_only_one_read_and_no_liveness_calls() {
        for count in [10_usize, 1000] {
            let q = queue(
                (0..count)
                    .map(|n| candidate(&format!("r{n}"), &format!("background:{n}:start")))
                    .collect(),
            );
            let mut watch = SweepWatch::default();
            let calls = Cell::new(0);
            assert!(
                watch
                    .live_candidates(
                        &q,
                        &[],
                        |_| {
                            calls.set(calls.get() + 1);
                            Ok(false)
                        },
                        |_| panic!("no turn")
                    )
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(calls.get(), count);
            assert_eq!(q.ended_reads.get(), 1);
            assert_eq!(q.turn_reads.get(), count.div_ceil(LAST_TURNS_PAGE));
            for _ in 0..2 {
                assert!(
                    watch
                        .live_candidates(
                            &q,
                            &[],
                            |_| panic!("checked wrapper"),
                            |_| panic!("checked turn")
                        )
                        .unwrap()
                        .is_empty()
                );
            }
            assert_eq!(q.ended_reads.get(), 3);
            assert_eq!(q.turn_reads.get(), count.div_ceil(LAST_TURNS_PAGE));
        }
    }

    #[test]
    fn liveness_errors_and_held_runs_choose_stop_remember_or_retry() {
        use SweepDecision::*;
        for (held, remembered, wrapper, turn, expected) in [
            (true, false, Some(true), Some(true), Skip),
            (false, true, None, None, Skip),
            (false, false, Some(true), None, Stop),
            (false, false, Some(false), Some(true), Stop),
            (false, false, Some(false), Some(false), Remember),
            (false, false, None, None, Retry),
            (false, false, Some(false), None, ReadTurn),
        ] {
            assert_eq!(sweep_decision(held, remembered, wrapper, turn), expected);
        }
        let c = candidate("r", "background:1:start");
        let mut q = queue(vec![c.clone()]);
        let mut watch = SweepWatch::default();
        let select = |watch: &mut SweepWatch, wrapper: bool| {
            watch
                .live_candidates(&q, &[], |_| Ok(wrapper), |_| Ok(true))
                .unwrap()
        };
        assert_eq!(
            select(&mut watch, true).as_slice(),
            std::slice::from_ref(&c)
        );
        assert!(watch.note_stop_failure(&c));
        assert_eq!(
            select(&mut watch, true).as_slice(),
            std::slice::from_ref(&c)
        );
        assert!(!watch.note_stop_failure(&c));
        assert!(watch.gone.is_empty());
        assert!(
            watch
                .live_candidates(
                    &q,
                    std::slice::from_ref(&c.run_id),
                    |_| panic!("held"),
                    |_| panic!("held")
                )
                .unwrap()
                .is_empty()
        );
        assert!(
            watch
                .live_candidates(
                    &q,
                    &[],
                    |_| anyhow::bail!("exists failed"),
                    |_| panic!("no turn")
                )
                .unwrap()
                .is_empty()
        );
        assert!(watch.gone.is_empty());
        q.fail.set(true);
        assert!(
            watch
                .live_candidates(&q, &[], |_| Ok(false), |_| Ok(true))
                .unwrap()
                .is_empty()
        );
        assert!(watch.gone.is_empty());
        q.fail.set(false);
        q.turns.push(LastSessionTurn {
            run_id: c.run_id.clone(),
            workspace_id: c.workspace_id.clone(),
            pid: 2,
            start: "turn".into(),
        });
        assert_eq!(
            watch
                .live_candidates(&q, &[], |_| Ok(false), |_| Ok(true))
                .unwrap(),
            [c]
        );
        assert!(watch.gone.is_empty());
        assert!(
            watch
                .live_candidates(&q, &[], |_| Ok(false), |_| Ok(false))
                .unwrap()
                .is_empty()
        );
        assert_eq!(watch.gone.len(), 1);
        // The same handle on a different run is a distinct read and decision.
        q.candidates[0].run_id = RunId::new("other").unwrap();
        let before = q.turn_reads.get();
        watch
            .live_candidates(&q, &[], |_| Ok(false), |_| Ok(false))
            .unwrap();
        assert_eq!(q.turn_reads.get(), before + 1);
    }

    struct IdentityRead {
        start: Cell<Option<&'static str>>,
        exists: Cell<bool>,
    }

    impl ProcessControl for IdentityRead {
        fn alive(&self, _: u32) -> bool {
            self.exists.get()
        }
        fn start_identity(&self, _: u32) -> Option<String> {
            self.start.get().map(str::to_owned)
        }
        fn terminate(&self, _: u32) -> Result<()> {
            unreachable!()
        }
        fn interrupt(&self, _: u32) -> Result<()> {
            unreachable!()
        }
        fn kill(&self, _: u32) -> Result<()> {
            unreachable!()
        }
    }

    #[test]
    fn unreadable_identity_is_retried_for_wrappers_and_remaining_turns() {
        use crate::domain::background_wrapper::BackgroundHandle;
        for remaining_turn in [false, true] {
            for final_start in [None, Some("different-process")] {
                let c = candidate("run", "background:1:original");
                let mut q = queue(vec![c.clone()]);
                if remaining_turn {
                    q.turns.push(LastSessionTurn {
                        run_id: c.run_id.clone(),
                        workspace_id: c.workspace_id.clone(),
                        pid: 2,
                        start: "original".into(),
                    });
                }
                let processes = IdentityRead {
                    start: Cell::new(None),
                    exists: Cell::new(true),
                };
                assert!(
                    !processes
                        .confirmed_identity_alive(&BackgroundHandle {
                            pid: 0,
                            start: "original".into(),
                        })
                        .unwrap()
                );
                let mut watch = SweepWatch::default();
                let select = |watch: &mut SweepWatch| {
                    watch
                        .live_candidates(
                            &q,
                            &[],
                            |id| {
                                if remaining_turn {
                                    Ok(false)
                                } else {
                                    processes.confirmed_identity_alive(
                                        &BackgroundHandle::parse(id).unwrap(),
                                    )
                                }
                            },
                            |turn| {
                                processes.confirmed_identity_alive(&BackgroundHandle {
                                    pid: turn.pid,
                                    start: turn.start.clone(),
                                })
                            },
                        )
                        .unwrap()
                };
                // A live pid with an unreadable start proves neither death
                // nor reuse. It must stay out of the permanent gone set.
                assert!(select(&mut watch).is_empty());
                assert!(watch.gone.is_empty());
                processes.start.set(Some("original"));
                assert_eq!(select(&mut watch).as_slice(), std::slice::from_ref(&c));
                assert!(watch.gone.is_empty());
                // Either a readable different identity, or an independently
                // absent pid after a missing start, really proves gone.
                processes.start.set(final_start);
                processes.exists.set(final_start.is_some());
                assert!(select(&mut watch).is_empty());
                assert_eq!(watch.gone.len(), 1);
                assert!(
                    watch
                        .live_candidates(
                            &q,
                            &[],
                            |_| panic!("confirmed gone"),
                            |_| panic!("confirmed gone")
                        )
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }

    #[test]
    fn oversized_pairs_never_reach_the_read() {
        let q = queue(vec![candidate(
            &"r".repeat(128),
            &format!("background:{}", "x".repeat(129)),
        )]);
        let mut watch = SweepWatch::default();
        assert!(
            watch
                .live_candidates(&q, &[], |_| Ok(false), |_| panic!("oversized"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(q.turn_reads.get(), 0);
        assert_eq!(watch.gone.len(), 1);
    }

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

    #[allow(unused_variables)]
    impl RunReads for ReadQueue {
        fn active_runs(&self) -> Result<Vec<TaskRun>> {
            unreachable!("active_runs is not what the test reaches")
        }
        fn all_runs(&self) -> Result<Vec<TaskRun>> {
            unreachable!("all_runs is not what the test reaches")
        }
        fn run(&self, id: &RunId) -> Result<TaskRun> {
            unreachable!("run is not what the test reaches")
        }
        fn runs_with_status(&self, status: RunStatus) -> Result<Vec<TaskRun>> {
            unreachable!("runs_with_status is not what the test reaches")
        }
        fn next_awaiting_integration(&self) -> Result<Option<TaskRun>> {
            unreachable!("next_awaiting_integration is not what the test reaches")
        }
        fn last_turns_of(&self, pairs: &[RunSessionPair]) -> Result<Vec<LastSessionTurn>> {
            self.turn_reads.set(self.turn_reads.get() + 1);
            assert!(pairs.len() <= LAST_TURNS_PAGE);
            assert!(pairs.iter().all(RunSessionPair::fits_sweep_read));
            anyhow::ensure!(!self.fail.get(), "page read failed");
            Ok(self
                .turns
                .iter()
                .filter(|turn| {
                    pairs.iter().any(|pair| {
                        pair.run_id == turn.run_id && pair.workspace_id == turn.workspace_id
                    })
                })
                .cloned()
                .collect())
        }
        fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>> {
            self.ended_reads.set(self.ended_reads.get() + 1);
            Ok(self.candidates.clone())
        }
        fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>> {
            unreachable!("ended_run_worktrees is not what the test reaches")
        }
        fn ended_run_worktree(&self, id: &RunId) -> Result<Option<EndedRunWorktree>> {
            unreachable!("ended_run_worktree is not what the test reaches")
        }
        fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>> {
            unreachable!("latest_runs_in_progress is not what the test reaches")
        }
        fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>> {
            unreachable!("runs_with_pending_push is not what the test reaches")
        }
        fn run_in_workspace(&self, workspace_id: &str) -> Result<Option<RunId>> {
            unreachable!("run_in_workspace is not what the test reaches")
        }
    }
}
