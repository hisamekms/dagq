//! The cleanup of ended runs' worktrees off the supervisor's loop (task
//! 405): measuring and removing gigabytes of build outputs took the loop
//! away from the live runs, so a job thread does it. The loop picks the
//! candidates ([`Supervisor::request_cleanup`]), and joins the job on a
//! later pass ([`Supervisor::poll_cleanup`]) to record what it removed.
//!
//! One job runs at a time; what is asked for meanwhile waits for the next
//! one, so no worktree is cleaned twice at once. The loop does not wait
//! for a job, but for a cleanup for disk space, which the claims and
//! landings wait for, and for the last job once it ends.
//!
//! The runs a job picked are reserved until it has passed them
//! ([`CleanupWatch::cleaning`]): the loop takes the lock before it leases
//! an ended run and leaves a reserved one for a later pass, as the
//! cleanup on the loop came first. Before each worktree the job checks
//! the queue again under that lock: a run leased since it was picked (by
//! another supervisor) is left alone. A stop or handoff finishes a job for
//! disk space, and the rest of a cleanup for room another job took on
//! (task 1426); ordinary cleanup ends after its current worktree and the
//! next sweep picks up the rest. That check reads the run alone
//! ([`RunLog::ended_run_worktree`], task 1586), not every ended run again.
//!
//! A run the job found nothing left of ([`nothing_left`]) is settled
//! ([`Cleaning::settled`]): the next sweeps leave it out of their
//! candidates, without Git or the filesystem, until the queue lists it
//! otherwise, a slot holds it or the loop leases it (task 1586).
//!
//! The job also removes what an ended run whose task is over left outside
//! its worktree: the Claude Code scratchpad of its session (task 1100,
//! [`scratchpad_dir_name`]), and the temporary files directory the runtime
//! gave its Codex turns as their `TMPDIR` (task 1290, [`RUN_TMP_DIR`] in
//! its run directory). The rest of the run directory stays.
//!
//! Beside the ended runs, it removes the build outputs of the runs left in
//! the middle with no lease and no live session (task 1289,
//! [`WorktreeCleanup`]): of one waiting for a person's answer on every
//! cleanup, of any other only in a cleanup for disk space. The loop leaves
//! such a run reserved by the job before it lands, reviews or resumes it.

use super::*;
use crate::application::{EndedRunWorktree, RUN_TMP_DIR, WorktreeCleanup};
use crate::domain::EventKind;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::{disk::Cleaned, sweep::BUILD_OUTPUT_DIRS};

/// A cleanup for disk space (task 377): the free bytes before it and what
/// was needed, for `auto_repaired`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DiskRequest {
    pub(super) free: Option<u64>,
    pub(super) needed: Option<u64>,
}

/// What a cleanup is asked for: every ended run, or only some tasks'.
#[derive(Debug, Default)]
struct Request {
    all: bool,
    tasks: Vec<TaskId>,
    /// For disk space: every ended run, and the claims and landings
    /// wait for the job.
    disk: Option<DiskRequest>,
    /// `git worktree prune` after the worktrees (for disk space).
    prune: bool,
    /// The build outputs of the runs nobody works on that wait for no
    /// answer ([`WorktreeCleanup::Idle`]) go too (for disk space).
    idle: bool,
    /// What is removed counts for room as `auto_repaired`, though the
    /// claims and landings do not wait for it: the rest of a cleanup for
    /// room that another job took on.
    counted: Option<DiskRequest>,
}

impl Request {
    fn add(&mut self, task: Option<TaskId>, disk: Option<DiskRequest>) {
        match task {
            Some(task) if !self.tasks.contains(&task) => self.tasks.push(task),
            Some(_) => {}
            None => self.all = true,
        }
        if disk.is_some() {
            self.all = true;
            self.prune = true;
            self.idle = true;
            // The first reading is the one the shortage was found at.
            self.disk = self.disk.or(disk);
        }
    }
    const fn is_empty(&self) -> bool {
        !self.all && self.tasks.is_empty()
    }
    fn wants(&self, candidate: &EndedRunWorktree) -> bool {
        (self.all || self.tasks.contains(&candidate.task_id))
            && (self.idle || candidate.cleanup != WorktreeCleanup::Idle)
    }
}

/// The cleanup job running now, if any, and what waits for the next one.
#[derive(Default)]
pub(super) struct CleanupWatch {
    job: Option<Job>,
    pending: Request,
    /// The runs the job picked and has not passed yet, and the runs
    /// settled. The loop holds the lock while it leases an ended run.
    cleaning: Arc<Mutex<Cleaning>>,
    /// Stop ordinary cleanup after its current worktree; disk cleanup finishes.
    stop: Arc<AtomicBool>,
    /// Stop accepting requests without interrupting a job for disk space.
    ending: bool,
    /// The loop left a run reserved by the job for a later pass: it waits
    /// for the job, as for a run.
    pub(super) deferred: bool,
}

struct Job {
    handle: thread::JoinHandle<Vec<Outcome>>,
    disk: Option<DiskRequest>,
    /// [`Request::counted`].
    counted: Option<DiskRequest>,
}

impl CleanupWatch {
    /// A job runs.
    pub(super) const fn running(&self) -> bool {
        self.job.is_some()
    }
    /// A cleanup for disk space runs or waits to. Once ending (a stop or a
    /// handoff), so does the rest of one another job took on: a drain
    /// decides its landings on the reading after it (task 1426).
    pub(super) fn for_disk(&self) -> bool {
        self.job
            .as_ref()
            .is_some_and(|job| job.disk.is_some() || (self.ending && job.counted.is_some()))
            || self.pending.disk.is_some()
            || (self.ending && self.pending.counted.is_some())
    }
    /// The lock the loop holds while it leases an ended run, and the runs
    /// the job has yet to pass, which the loop leaves for a later pass.
    pub(super) fn cleaning(&self) -> Arc<Mutex<Cleaning>> {
        self.cleaning.clone()
    }
}

/// What the loop and the job share under one lock.
#[derive(Default)]
pub(super) struct Cleaning {
    /// The runs the job picked and has not passed yet.
    reserved: Vec<RunId>,
    /// The runs a job found nothing left to clean of ([`nothing_left`]),
    /// as the queue listed them then (task 1586). A sweep leaves them out
    /// of its candidates while the queue lists them the same, no slot
    /// holds them and the loop has not leased them since.
    settled: HashMap<RunId, EndedRunWorktree>,
}

impl Cleaning {
    /// The loop is about to lease `run`: `false` while the job has it
    /// reserved, which the loop leaves for a later pass. Otherwise the run
    /// is no longer settled, as it may get a worktree again.
    pub(super) fn may_lease(&mut self, run: &RunId) -> bool {
        if self.reserved.contains(run) {
            return false;
        }
        self.settled.remove(run);
        true
    }
}

/// Lock `cleaning`, whatever a panicking holder left.
pub(super) fn lock_cleaning(cleaning: &Mutex<Cleaning>) -> MutexGuard<'_, Cleaning> {
    cleaning.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the job saw of a candidate's run once it passed it (task 1586).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Left {
    /// The worktree's directory is there.
    worktree: bool,
    /// The run's branch is still listed; `None` when the job did not read
    /// the list.
    branch: Option<bool>,
    /// Something of the run could not be cleaned: its worktree or branch,
    /// its runner, a scratchpad or its temporary files directory.
    failed: bool,
}

/// Nothing is left of `candidate` that a cleanup would remove (task 1586):
/// no worktree directory, nothing that failed, and once its task is over
/// no branch either. A run whose task goes on keeps its branch, its
/// scratchpads and its temporary files directory on purpose, and a run
/// that has not ended its runner (a `needs_session` run waiting for a
/// resume): the job leaves them, and they count once the queue lists the
/// run otherwise. A branch the job did not see the list of may be left.
fn nothing_left(candidate: &EndedRunWorktree, left: Left) -> bool {
    let over = matches!(
        candidate.task_status,
        TaskStatus::Completed | TaskStatus::Canceled
    );
    let branch = over && candidate.branch.is_some() && left.branch != Some(false);
    !left.worktree && !left.failed && !branch
}

/// The candidates of a job: the runs `listed` that `request` wants, less
/// those a slot holds and those settled (task 1586). A settled run the
/// queue no longer lists the same (it was leased, it or its task moved
/// on), or that a slot holds, is no longer settled.
fn pick_candidates(
    listed: Vec<EndedRunWorktree>,
    request: &Request,
    in_slot: impl Fn(&RunId) -> bool,
    settled: &mut HashMap<RunId, EndedRunWorktree>,
) -> Vec<EndedRunWorktree> {
    let now: HashMap<&RunId, &EndedRunWorktree> = listed.iter().map(|w| (&w.run_id, w)).collect();
    settled.retain(|run, then| now.get(run).is_some_and(|now| *now == then) && !in_slot(run));
    listed
        .into_iter()
        .filter(|w| request.wants(w))
        .filter(|w| !in_slot(&w.run_id))
        .filter(|w| !settled.contains_key(&w.run_id))
        .collect()
}

/// What the job did to one worktree.
enum Outcome {
    /// The build outputs of a run whose task goes on, and why
    /// ([`WorktreeCleanup`]).
    BuildOutputs {
        run_id: RunId,
        status: RunStatus,
        cleanup: WorktreeCleanup,
        paths: Vec<String>,
        bytes: u64,
    },
    /// The worktree and branch of a run whose task is over; `missing` when
    /// the directory was already gone and only the branch went, `repaired`
    /// when `git worktree repair` had to point the worktree at the
    /// repository again first, `broken_git` when Git could not take it
    /// for a worktree and its directory was removed instead (task 1587,
    /// [`may_remove_broken`]).
    Worktree {
        run_id: RunId,
        task_id: TaskId,
        task_status: TaskStatus,
        path: String,
        branch: String,
        bytes: u64,
        missing: bool,
        repaired: bool,
        broken_git: bool,
    },
    /// The Claude Code scratchpads of a run whose task is over.
    Scratchpads {
        run_id: RunId,
        task_status: TaskStatus,
        paths: Vec<String>,
        bytes: u64,
    },
    /// The temporary files directory of a run whose task is over (task
    /// 1290).
    RunTmp {
        run_id: RunId,
        task_status: TaskStatus,
        path: String,
        bytes: u64,
    },
    Failed {
        run_id: RunId,
        /// `worktree`, `scratchpad` or `run tmp`.
        what: &'static str,
        path: String,
        error: anyhow::Error,
    },
}

/// `build_outputs_removed`'s `reason`: the run ended
/// ([`WorktreeCleanup::Ended`]).
const BUILD_OUTPUTS_RUN_ENDED: &str = "run_ended";
/// The run waits for a person's answer to the ask `ask_id` names
/// ([`WorktreeCleanup::AwaitingAnswer`]).
const BUILD_OUTPUTS_AWAITING_ANSWER: &str = "awaiting_answer";
/// The run waits for nobody's answer, and the free disk space ran short
/// ([`WorktreeCleanup::Idle`]).
const BUILD_OUTPUTS_DISK_SPACE: &str = "disk_space";

/// The longest name Claude Code gives a project's directory whole; it
/// shortens a longer one with a hash, which is not looked for.
const SCRATCHPAD_NAME_MAX: usize = 200;

/// The name of the directory Claude Code keeps the scratchpads of the
/// sessions started in `cwd` under (task 1100): `cwd` with every character
/// but an ASCII letter or digit turned into `-` (so `/.local/` becomes
/// `--local-`), as it names `~/.claude/projects/` too. `None` when longer
/// than [`SCRATCHPAD_NAME_MAX`]. The name holds no `/` and no `..`, so it
/// stays directly under the directory it is joined to.
pub(super) fn scratchpad_dir_name(cwd: &str) -> Option<String> {
    let name: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    (!name.is_empty() && name.len() <= SCRATCHPAD_NAME_MAX).then_some(name)
}

/// What the job works with, all of it shared with the loop.
struct JobPorts {
    files: Arc<dyn RunFiles>,
    repository: Arc<dyn Repository + Send + Sync>,
    queues: Arc<dyn QueueOpener>,
    runs_dir: PathBuf,
    repo_root: PathBuf,
    cleaning: Arc<Mutex<Cleaning>>,
    stop: Arc<AtomicBool>,
    prune: bool,
    /// Where Claude Code keeps the sessions' scratchpads (task 1100).
    scratchpad_roots: Vec<PathBuf>,
}

impl Supervisor<'_> {
    /// Ask for the worktrees of ended runs to be cleaned, every such run's
    /// or only `task`'s (see [`Self::clean_ended_worktrees`] for what is
    /// removed), for disk space when `disk` is given. It starts at once
    /// unless a job runs; then it waits for the next one. Whether it was
    /// taken: none is once ending (a stop or a handoff).
    pub(super) fn request_cleanup(
        &mut self,
        task: Option<TaskId>,
        disk: Option<DiskRequest>,
    ) -> bool {
        if self.cleanup.ending {
            return false;
        }
        // For room while another job runs: that job counts for it, so the
        // claims wait only for it, and the rest (the runs it did not pick,
        // and the prune) follows without holding them. The build outputs
        // of the runs nobody works on that wait for no answer (task 1289),
        // which only a cleanup for room removes, go in that rest, which
        // counts what it removes for room too.
        if let (Some(request), Some(job)) = (disk, self.cleanup.job.as_mut())
            && job.disk.is_none()
        {
            job.disk = Some(request);
            self.cleanup.pending.add(None, None);
            self.cleanup.pending.prune = true;
            self.cleanup.pending.idle = true;
            self.cleanup.pending.counted = self.cleanup.pending.counted.or(Some(request));
            return true;
        }
        self.cleanup.pending.add(task, disk);
        self.start_cleanup();
        true
    }
    /// Join a finished job and record what it did, then start what waits;
    /// with `ending` (a stop or a handoff), let the job end after its
    /// current worktree (all candidates for disk space) and start nothing
    /// more but the rest of a cleanup for room another job took on, which
    /// goes to its last candidate too (task 1426).
    pub(super) fn poll_cleanup(&mut self, ending: bool) {
        if ending {
            self.end_cleanup();
        }
        let Some(job) = self.cleanup.job.take_if(|job| job.handle.is_finished()) else {
            return;
        };
        let outcomes = job.handle.join().unwrap_or_else(|_| {
            warn!("the cleanup of ended runs' worktrees panicked");
            Vec::new()
        });
        // Whatever the job left reserved (it could not open the queue, or
        // panicked) is free again.
        lock_cleaning(&self.cleanup.cleaning).reserved.clear();
        self.cleanup.deferred = false;
        let cleaned = self.record_cleanup(outcomes);
        if let Some(disk) = job.disk.or(job.counted) {
            self.cleaned_for_disk(disk, &cleaned);
        }
        self.start_cleanup();
    }
    /// A stop or a handoff: take no more requests, let an ordinary job end
    /// after its current worktree, and drop what waits but the rest of a
    /// cleanup for room another job took on. No job is joined: a reading
    /// of the disk taken before stays the one of the cleanup in progress.
    pub(super) fn end_cleanup(&mut self) {
        self.cleanup.ending = true;
        if !self
            .cleanup
            .job
            .as_ref()
            .is_some_and(|job| job.disk.is_some() || job.counted.is_some())
        {
            self.cleanup.stop.store(true, Ordering::SeqCst);
        }
        let pending = std::mem::take(&mut self.cleanup.pending);
        if let Some(counted) = pending.counted {
            self.cleanup.pending = Request {
                all: true,
                prune: true,
                idle: true,
                counted: Some(counted),
                ..Request::default()
            };
        }
    }
    /// A handoff withdrawn while this process drained, which goes back to
    /// claims (task 1427): requests are taken again, an ordinary job that
    /// has not seen the stop yet goes on, and every ended run is asked for
    /// at once, which picks up what the drain dropped. A job that stopped
    /// after its current worktree leaves the rest to that request.
    pub(super) fn resume_cleanup(&mut self) {
        if !self.cleanup.ending {
            return;
        }
        self.cleanup.ending = false;
        self.cleanup.stop.store(false, Ordering::SeqCst);
        self.request_cleanup(None, None);
    }
    /// Once the loop ended: wait for the job and whatever waits for the
    /// next one, and record what they did. After a stop, only the running
    /// job and the rest of a cleanup for room it took on remain; ordinary
    /// cleanup stops after its current worktree.
    pub(super) fn finish_cleanup(&mut self) {
        while let Some(job) = &self.cleanup.job {
            while !job.handle.is_finished() {
                thread::sleep(Duration::from_millis(20));
            }
            self.poll_cleanup(false);
        }
    }
    /// Start a job for what waits, unless one runs: the candidates are
    /// picked here, on the loop, less the runs a slot holds.
    fn start_cleanup(&mut self) {
        if self.cleanup.job.is_some() || self.cleanup.pending.is_empty() {
            return;
        }
        let request = std::mem::take(&mut self.cleanup.pending);
        let listed = match self.queue.ended_run_worktrees() {
            Ok(listed) => listed,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the worktrees of ended runs could not be listed for their cleanup: {error:#}");
                return;
            }
        };
        let mut cleaning = lock_cleaning(&self.cleanup.cleaning);
        let candidates = pick_candidates(
            listed,
            &request,
            |run| self.slots.iter().any(|slot| slot.run.id() == run),
            &mut cleaning.settled,
        );
        if candidates.is_empty() && !request.prune {
            return;
        }
        cleaning.reserved = candidates.iter().map(|w| w.run_id.clone()).collect();
        drop(cleaning);
        let ports = JobPorts {
            files: self.files.clone(),
            repository: self.repository.clone(),
            queues: self.queues.clone(),
            runs_dir: self.layout.runs_dir.clone(),
            repo_root: self.layout.repo_root.clone(),
            cleaning: self.cleanup.cleaning.clone(),
            // A cleanup for room goes to its last candidate: it is not
            // stopped with ordinary cleanup.
            stop: if request.disk.is_some() || request.counted.is_some() {
                Arc::new(AtomicBool::new(false))
            } else {
                self.cleanup.stop.clone()
            },
            prune: request.prune,
            scratchpad_roots: (self.scratchpad_roots)(),
        };
        let handle = spawn_traced(move || run_job(&ports, candidates));
        self.cleanup.job = Some(Job {
            handle,
            disk: request.disk,
            counted: request.counted,
        });
    }
    /// Record the events of what the job did; what it removed.
    fn record_cleanup(&mut self, outcomes: Vec<Outcome>) -> Cleaned {
        let mut cleaned = Cleaned::default();
        for outcome in outcomes {
            let recorded = match outcome {
                Outcome::BuildOutputs {
                    run_id,
                    status,
                    cleanup,
                    paths,
                    bytes,
                } => {
                    let (reason, why) = match cleanup {
                        WorktreeCleanup::Ended => (BUILD_OUTPUTS_RUN_ENDED, String::new()),
                        WorktreeCleanup::AwaitingAnswer(ask) => (
                            BUILD_OUTPUTS_AWAITING_ANSWER,
                            format!(", waiting for the answer to ask {ask}"),
                        ),
                        WorktreeCleanup::Idle => {
                            (BUILD_OUTPUTS_DISK_SPACE, ", for disk space".to_owned())
                        }
                    };
                    info!(run_id = %run_id, "run {run_id} is {}{why}; removed the build outputs of its worktree ({bytes} bytes)", status.as_str());
                    cleaned.add(&run_id, bytes);
                    let mut payload = json!({"paths": paths, "bytes": bytes, "by": "supervisor", "reason": reason});
                    if let WorktreeCleanup::AwaitingAnswer(ask) = cleanup {
                        payload["ask_id"] = json!(ask);
                    }
                    self.queue.record_runtime_event(
                        &run_id,
                        EventKind::BuildOutputsRemoved,
                        payload,
                    )
                }
                Outcome::Worktree {
                    run_id,
                    task_id,
                    task_status,
                    path,
                    branch,
                    bytes,
                    missing,
                    repaired,
                    broken_git,
                } => {
                    let reason = format!("task_{}", task_status.as_str());
                    let mut payload = json!({"path": path, "branch": branch, "bytes": bytes, "by": "supervisor", "reason": reason});
                    if missing {
                        payload["worktree_missing"] = json!(true);
                        info!(run_id = %run_id, task_id = %task_id, "task {task_id} is {}; the worktree {path} of run {run_id} was already gone: removed its branch {branch}", task_status.as_str());
                    } else {
                        info!(run_id = %run_id, task_id = %task_id, "task {task_id} is {}; removed worktree {path} and branch {branch} of run {run_id} ({bytes} bytes)", task_status.as_str());
                    }
                    if repaired {
                        payload["repaired"] = json!(true);
                    }
                    if broken_git {
                        payload["broken_git"] = json!(true);
                        info!(run_id = %run_id, "the worktree {path} of run {run_id} had a broken .git: removed its directory");
                    }
                    cleaned.add(&run_id, bytes);
                    self.queue
                        .record_runtime_event(&run_id, EventKind::WorktreeRemoved, payload)
                }
                Outcome::Scratchpads {
                    run_id,
                    task_status,
                    paths,
                    bytes,
                } => {
                    info!(run_id = %run_id, "run {run_id}'s task is {}; removed its Claude Code scratchpad(s) {} ({bytes} bytes)", task_status.as_str(), paths.join(", "));
                    cleaned.add(&run_id, bytes);
                    self.queue.record_runtime_event(
                        &run_id,
                        EventKind::ScratchpadRemoved,
                        json!({"paths": paths, "bytes": bytes, "by": "supervisor", "reason": format!("task_{}", task_status.as_str())}),
                    )
                }
                Outcome::RunTmp {
                    run_id,
                    task_status,
                    path,
                    bytes,
                } => {
                    info!(run_id = %run_id, "run {run_id}'s task is {}; removed its temporary files {path} ({bytes} bytes)", task_status.as_str());
                    cleaned.add(&run_id, bytes);
                    self.queue.record_runtime_event(
                        &run_id,
                        EventKind::RunTmpRemoved,
                        json!({"paths": [path], "bytes": bytes, "by": "supervisor", "reason": format!("task_{}", task_status.as_str())}),
                    )
                }
                Outcome::Failed {
                    run_id,
                    what,
                    path,
                    error,
                } => {
                    if self.sweep_failures.contains(&path) {
                        warn!(run_id = %run_id, "run {run_id}: {what} {path} still could not be cleaned: {error:#}");
                        continue;
                    }
                    self.sweep_failures.push(path.clone());
                    let message = format!("{what} {path} could not be cleaned: {error:#}");
                    warn!(run_id = %run_id, "run {run_id}: {message}");
                    self.queue.record_runtime_event(
                        &run_id,
                        EventKind::CleanupFailed,
                        reason_of_error(&error, ReasonCode::Other)
                            .on(json!({"path": path, "message": message, "by": "supervisor"})),
                    )
                }
            };
            if let Err(error) = recorded {
                warn!(error = %format_args!("{error:#}"), "the cleanup of an ended run's worktree could not be recorded: {error:#}");
            }
        }
        cleaned
    }
}

/// Clean each candidate in turn (see the module), then `git worktree
/// prune` for disk space.
fn run_job(ports: &JobPorts, candidates: Vec<EndedRunWorktree>) -> Vec<Outcome> {
    let queue = match ports.queues.open() {
        Ok(queue) => queue,
        Err(error) => {
            warn!(error = %format_args!("{error:#}"), "the cleanup of ended runs' worktrees could not open the queue: {error:#}");
            return Vec::new();
        }
    };
    let mut branches: Option<Vec<String>> = None;
    let mut pruned = false;
    let outcomes = pass_candidates(
        &ports.cleaning,
        &ports.stop,
        candidates,
        |run| queue.ended_run_worktree(run),
        |candidate, outcomes| {
            clean_candidate(ports, candidate, &mut branches, &mut pruned, outcomes)
        },
    );
    if ports.prune
        && !pruned
        && let Err(error) = ports.repository.prune_worktrees()
    {
        warn!(error = %format_args!("{error:#}"), "git worktree prune failed: {error:#}");
    }
    outcomes
}

/// Pass each candidate in turn: under the lock, read it again
/// (`recheck`, that run alone) and clean it (`clean`, whether nothing is
/// left of it) only while the queue lists it as it was picked: nobody
/// leased it since, and neither it nor its task moved on. A run passed
/// with nothing left is settled under the same lock it is released in,
/// so a lease the loop takes after it unsettles it.
fn pass_candidates(
    cleaning: &Mutex<Cleaning>,
    stop: &AtomicBool,
    candidates: Vec<EndedRunWorktree>,
    mut recheck: impl FnMut(&RunId) -> Result<Option<EndedRunWorktree>>,
    mut clean: impl FnMut(&EndedRunWorktree, &mut Vec<Outcome>) -> bool,
) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    for candidate in candidates {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let still = {
            let _cleaning = lock_cleaning(cleaning);
            match recheck(&candidate.run_id) {
                Ok(now) => now.as_ref() == Some(&candidate),
                Err(error) => {
                    warn!(run_id = %candidate.run_id, error = %format_args!("{error:#}"), "run {}: its worktree was not cleaned, the queue could not be read: {error:#}", candidate.run_id);
                    false
                }
            }
        };
        let settled = still && clean(&candidate, &mut outcomes);
        let mut cleaning = lock_cleaning(cleaning);
        cleaning.reserved.retain(|run| *run != candidate.run_id);
        if settled {
            cleaning.settled.insert(candidate.run_id.clone(), candidate);
        }
    }
    lock_cleaning(cleaning).reserved.clear();
    outcomes
}

/// Clean one candidate: its runner, worktree or build outputs, scratchpads
/// and temporary files directory; whether nothing is left of it
/// ([`nothing_left`]).
fn clean_candidate(
    ports: &JobPorts,
    candidate: &EndedRunWorktree,
    branches: &mut Option<Vec<String>>,
    pruned: &mut bool,
    outcomes: &mut Vec<Outcome>,
) -> bool {
    let first = outcomes.len();
    let runner = candidate.cleanup != WorktreeCleanup::Ended || remove_run_runner(ports, candidate);
    match clean_worktree(ports, candidate, branches, pruned) {
        Ok(Some(outcome)) => outcomes.push(outcome),
        Ok(None) => {}
        Err(error) => outcomes.push(Outcome::Failed {
            run_id: candidate.run_id.clone(),
            what: "worktree",
            path: candidate.worktree.clone(),
            error,
        }),
    }
    remove_scratchpads(ports, candidate, outcomes);
    remove_run_tmp(ports, candidate, outcomes);
    let mine = &outcomes[first..];
    let branch = if mine.iter().any(|o| matches!(o, Outcome::Worktree { .. })) {
        Some(false)
    } else {
        candidate.branch.as_deref().and_then(|branch| {
            let short = branch.trim_start_matches("refs/heads/");
            branches
                .as_ref()
                .map(|listed| listed.iter().any(|listed| listed == short))
        })
    };
    nothing_left(
        candidate,
        Left {
            worktree: ports.files.is_dir(Path::new(&candidate.worktree)),
            branch,
            failed: !runner || mine.iter().any(|o| matches!(o, Outcome::Failed { .. })),
        },
    )
}

/// Remove the runner (the binary snapshot its session wrapper ran from)
/// of an ended run nobody leases: no session of it runs, and a resume,
/// which needs the lease, copies it again. The run stays reserved while
/// this runs, so the loop cannot lease it and copy one in between. A
/// failure is logged and retried on the next cleanup; `false` for one.
fn remove_run_runner(ports: &JobPorts, candidate: &EndedRunWorktree) -> bool {
    let runner = ports
        .runs_dir
        .join(candidate.run_id.as_str())
        .join(RUN_RUNNER_FILE);
    match crate::application::planner::remove_runner(&*ports.files, &runner) {
        Ok(true) => {
            info!(run_id = %candidate.run_id, "run {} is {}; removed its runner", candidate.run_id, candidate.status.as_str());
            true
        }
        Ok(false) => true,
        Err(error) => {
            warn!(run_id = %candidate.run_id, error = %format_args!("{error:#}"), "run {}: its runner could not be removed: {error:#}", candidate.run_id);
            false
        }
    }
}

/// Remove the Claude Code scratchpads of an ended run whose task is over
/// (task 1100): the directory named [`scratchpad_dir_name`] after its
/// worktree, the cwd of its session, under each of the scratchpad roots.
/// A run whose task goes on keeps them, as a resume may go on in them. A
/// directory that is not there, or is a link, is left alone; nothing
/// outside the directory is followed, and one gone before its removal
/// (another supervisor's cleanup, or Claude Code's) is no failure. What
/// was removed is pushed to `outcomes` with a failure under another root.
fn remove_scratchpads(ports: &JobPorts, candidate: &EndedRunWorktree, outcomes: &mut Vec<Outcome>) {
    if !matches!(
        candidate.task_status,
        TaskStatus::Completed | TaskStatus::Canceled
    ) || !Path::new(&candidate.worktree).starts_with(&ports.runs_dir)
    {
        return;
    }
    let Some(name) = scratchpad_dir_name(&candidate.worktree) else {
        return;
    };
    let mut paths = Vec::new();
    let mut bytes = 0;
    for root in &ports.scratchpad_roots {
        let dir = root.join(&name);
        let removed = ports
            .files
            .tree_size(&dir)
            .with_context(|| format!("measure {}", dir.display()))
            .and_then(|size| {
                // `None` for no directory there, and for a link.
                let Some(size) = size else {
                    return Ok(None);
                };
                match ports.files.remove_dir_all(&dir) {
                    Ok(()) => Ok(Some(size)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => {
                        Err(anyhow::Error::new(error).context(format!("remove {}", dir.display())))
                    }
                }
            });
        match removed {
            Ok(Some(size)) => {
                paths.push(dir.to_string_lossy().into_owned());
                bytes += size;
            }
            Ok(None) => {}
            Err(error) => outcomes.push(Outcome::Failed {
                run_id: candidate.run_id.clone(),
                what: "scratchpad",
                path: dir.to_string_lossy().into_owned(),
                error,
            }),
        }
    }
    if !paths.is_empty() {
        outcomes.push(Outcome::Scratchpads {
            run_id: candidate.run_id.clone(),
            task_status: candidate.task_status,
            paths,
            bytes,
        });
    }
}

/// Remove the temporary files directory ([`RUN_TMP_DIR`]) of a run whose
/// task is over (task 1290): the `TMPDIR` the runtime gave its Codex
/// turns, in its run directory. A run whose task goes on keeps it, as a
/// resume may go on in it. Only that directory goes, without following a
/// link (one there is left alone) and nothing else of the run directory;
/// one not there, or gone before its removal, is no failure.
fn remove_run_tmp(ports: &JobPorts, candidate: &EndedRunWorktree, outcomes: &mut Vec<Outcome>) {
    if !matches!(
        candidate.task_status,
        TaskStatus::Completed | TaskStatus::Canceled
    ) {
        return;
    }
    let dir = ports
        .runs_dir
        .join(candidate.run_id.as_str())
        .join(RUN_TMP_DIR);
    let removed = ports
        .files
        .tree_size(&dir)
        .with_context(|| format!("measure {}", dir.display()))
        .and_then(|size| {
            // `None` for no directory there, and for a link.
            let Some(size) = size else {
                return Ok(None);
            };
            match ports.files.remove_dir_all(&dir) {
                Ok(()) => Ok(Some(size)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => {
                    Err(anyhow::Error::new(error).context(format!("remove {}", dir.display())))
                }
            }
        });
    let path = dir.to_string_lossy().into_owned();
    match removed {
        Ok(Some(bytes)) => outcomes.push(Outcome::RunTmp {
            run_id: candidate.run_id.clone(),
            task_status: candidate.task_status,
            path,
            bytes,
        }),
        Ok(None) => {}
        Err(error) => outcomes.push(Outcome::Failed {
            run_id: candidate.run_id.clone(),
            what: "run tmp",
            path,
            error,
        }),
    }
}

/// Clean one ended run's worktree ([`Supervisor::clean_ended_worktrees`]).
fn clean_worktree(
    ports: &JobPorts,
    candidate: &EndedRunWorktree,
    branches: &mut Option<Vec<String>>,
    pruned: &mut bool,
) -> Result<Option<Outcome>> {
    let worktree = Path::new(&candidate.worktree);
    if !worktree.starts_with(&ports.runs_dir) || ports.repo_root.starts_with(worktree) {
        return Ok(None);
    }
    let over = matches!(
        candidate.task_status,
        TaskStatus::Completed | TaskStatus::Canceled
    );
    let removed = |bytes, missing, removal, branch: &str| Outcome::Worktree {
        run_id: candidate.run_id.clone(),
        task_id: candidate.task_id,
        task_status: candidate.task_status,
        path: candidate.worktree.clone(),
        branch: branch.to_owned(),
        bytes,
        missing,
        repaired: removal == Removal::Repaired,
        broken_git: removal == Removal::BrokenGit,
    };
    if !ports.files.is_dir(worktree) {
        // The directory is gone, but the branch may be left: Git still
        // records the worktree until it is pruned, and refuses to delete
        // a branch checked out there.
        let Some(branch) = candidate.branch.as_deref().filter(|_| over) else {
            return Ok(None);
        };
        let listed = match branches {
            Some(listed) => listed,
            None => branches.insert(ports.repository.branches()?),
        };
        let short = branch.trim_start_matches("refs/heads/");
        if !listed.iter().any(|listed| listed == short) {
            return Ok(None);
        }
        if !*pruned {
            ports.repository.prune_worktrees()?;
            *pruned = true;
        }
        ports.repository.delete_branch(branch)?;
        listed.retain(|listed| listed != short);
        return Ok(Some(removed(0, true, Removal::Removed, branch)));
    }
    if over {
        let Some(branch) = candidate.branch.as_deref() else {
            return Ok(None);
        };
        let bytes = ports
            .files
            .tree_size(worktree)
            .with_context(|| format!("measure {}", worktree.display()))?
            .unwrap_or(0);
        let removal = remove_worktree(ports, candidate, branch, pruned)?;
        return Ok(Some(removed(bytes, false, removal, branch)));
    }
    let mut paths = Vec::new();
    let mut bytes = 0;
    for name in BUILD_OUTPUT_DIRS {
        let dir = worktree.join(name);
        let Some(size) = ports
            .files
            .tree_size(&dir)
            .with_context(|| format!("measure {}", dir.display()))?
        else {
            continue;
        };
        if ports.repository.tracks(worktree, name)? {
            continue;
        }
        ports
            .files
            .remove_dir_all(&dir)
            .with_context(|| format!("remove {}", dir.display()))?;
        paths.push(dir.to_string_lossy().into_owned());
        bytes += size;
    }
    if paths.is_empty() {
        return Ok(None);
    }
    Ok(Some(Outcome::BuildOutputs {
        run_id: candidate.run_id.clone(),
        status: candidate.status,
        cleanup: candidate.cleanup,
        paths,
        bytes,
    }))
}

/// How [`remove_worktree`] removed a worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Removal {
    /// `git worktree remove` at once.
    Removed,
    /// After `git worktree repair`.
    Repaired,
    /// Git took it for no worktree, so its directory was removed
    /// ([`may_remove_broken`]).
    BrokenGit,
}

/// Remove the worktree and its branch. When Git refuses (the worktree's
/// `.git` points at a common directory the queue's rebind left behind),
/// `git worktree repair` points it at the repository again and the removal
/// is tried once more. When the repair finds the `.git` broken and
/// [`may_remove_broken`] allows it (task 1587), the directory is removed,
/// the worktrees whose directory is gone are pruned and the branch is
/// deleted.
fn remove_worktree(
    ports: &JobPorts,
    candidate: &EndedRunWorktree,
    branch: &str,
    pruned: &mut bool,
) -> Result<Removal> {
    let repository = &*ports.repository;
    let worktree = Path::new(&candidate.worktree);
    let Err(error) = repository.remove_worktree_and_branch(worktree, branch) else {
        return Ok(Removal::Removed);
    };
    if let Err(failed) = repository.repair_worktree(worktree) {
        let (removal, repair) = (format!("{error:#}"), format!("{failed:#}"));
        if !may_remove_broken(
            candidate,
            &ports.runs_dir,
            &ports.repo_root,
            &removal,
            &repair,
        ) {
            return Err(failed.context(format!("{removal}; and repairing the worktree failed")));
        }
        match ports.files.remove_dir_all(worktree) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!(
                    "{removal}; its .git is broken ({repair}), and removing {} failed",
                    worktree.display()
                )));
            }
        }
        let gone = || format!("removed {} as its .git is broken", worktree.display());
        repository.prune_worktrees().with_context(gone)?;
        *pruned = true;
        repository.delete_branch(branch).with_context(gone)?;
        return Ok(Removal::BrokenGit);
    }
    repository
        .remove_worktree_and_branch(worktree, branch)
        .with_context(|| format!("{error:#}; repaired, and removing it again failed"))?;
    Ok(Removal::Repaired)
}

/// What `git worktree remove` says of a directory it takes for no
/// worktree of the repository: not registered, its `.git` not a gitfile,
/// or no `.git` at all.
const NOT_A_WORKTREE: [&str; 3] = [
    "is not a working tree",
    "is not a .git file",
    ".git' does not exist",
];
/// What `git worktree repair` says of a `.git` it cannot follow to a
/// repository.
const BROKEN_GIT: &str = ".git file broken";

/// Whether the worktree whose removal failed with `removal` and whose
/// repair failed with `repair` may be removed as a directory (task 1587):
/// the run's task is over (its sources and commits are not needed, as for
/// `git worktree remove --force`), the worktree is the run's own
/// `<runs>/<run-id>/worktree` and holds no part of the repository's
/// checkout, and Git failed on both because the `.git` is broken, so it
/// can never take it for a worktree. Any other failure stays a failure.
fn may_remove_broken(
    candidate: &EndedRunWorktree,
    runs_dir: &Path,
    repo_root: &Path,
    removal: &str,
    repair: &str,
) -> bool {
    let worktree = Path::new(&candidate.worktree);
    matches!(
        candidate.task_status,
        TaskStatus::Completed | TaskStatus::Canceled
    ) && worktree == runs_dir.join(candidate.run_id.as_str()).join("worktree")
        && !repo_root.starts_with(worktree)
        && NOT_A_WORKTREE.iter().any(|said| removal.contains(said))
        && repair.contains(BROKEN_GIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scratchpad_is_named_after_its_cwd_as_claude_code_names_it() {
        assert_eq!(
            scratchpad_dir_name(
                "/Users/me/.local/share/dagq/7706/runs/c0480993-f9e9-4a81-ade7-b2551b7134f3/worktree"
            )
            .as_deref(),
            Some(
                "-Users-me--local-share-dagq-7706-runs-c0480993-f9e9-4a81-ade7-b2551b7134f3-worktree"
            )
        );
        // Nothing to climb out of the directory it is joined to.
        assert_eq!(scratchpad_dir_name("/../x_y").as_deref(), Some("----x-y"));
        assert_eq!(scratchpad_dir_name(""), None);
        let long = format!("/{}", "a".repeat(SCRATCHPAD_NAME_MAX));
        assert_eq!(scratchpad_dir_name(&long), None);
        assert!(scratchpad_dir_name(&long[..SCRATCHPAD_NAME_MAX]).is_some());
    }

    /// An ended run of task `task` whose task is `task_status`.
    fn candidate(run: &str, task_status: TaskStatus) -> EndedRunWorktree {
        EndedRunWorktree {
            run_id: RunId::new(run.to_owned()).unwrap(),
            task_id: TaskId::new(1),
            status: RunStatus::Failed,
            task_status,
            worktree: format!("/runs/{run}/worktree"),
            branch: Some(format!("refs/heads/dagq/{run}")),
            cleanup: WorktreeCleanup::Ended,
        }
    }

    const GONE: Left = Left {
        worktree: false,
        branch: Some(false),
        failed: false,
    };

    /// Task 1586: what has to be left of a run for it to stay a
    /// candidate.
    #[test]
    fn a_run_stays_a_candidate_while_something_of_it_is_left() {
        let over = candidate("r", TaskStatus::Completed);
        assert!(nothing_left(&over, GONE));
        assert!(nothing_left(&candidate("r", TaskStatus::Canceled), GONE));
        // The worktree's directory, a failure, or the branch of a task
        // that is over.
        for left in [
            Left {
                worktree: true,
                ..GONE
            },
            Left {
                failed: true,
                ..GONE
            },
            Left {
                branch: Some(true),
                ..GONE
            },
            // The list was not read: the branch may be there.
            Left {
                branch: None,
                ..GONE
            },
        ] {
            assert!(!nothing_left(&over, left), "{left:?}");
        }
        // A run with no branch has none to be left.
        let no_branch = EndedRunWorktree {
            branch: None,
            ..over.clone()
        };
        assert!(nothing_left(
            &no_branch,
            Left {
                branch: None,
                ..GONE
            }
        ));
        // A task that goes on keeps the branch on purpose; the worktree
        // and a failure still count.
        let goes_on = candidate("r", TaskStatus::InProgress);
        assert!(nothing_left(
            &goes_on,
            Left {
                branch: Some(true),
                ..GONE
            }
        ));
        assert!(nothing_left(
            &goes_on,
            Left {
                branch: None,
                ..GONE
            }
        ));
        assert!(!nothing_left(
            &goes_on,
            Left {
                worktree: true,
                ..GONE
            }
        ));
        assert!(!nothing_left(
            &goes_on,
            Left {
                failed: true,
                ..GONE
            }
        ));
        let waiting = EndedRunWorktree {
            status: RunStatus::NeedsSession,
            cleanup: WorktreeCleanup::Idle,
            ..goes_on
        };
        assert!(nothing_left(&waiting, GONE));
        assert!(!nothing_left(
            &waiting,
            Left {
                worktree: true,
                ..GONE
            }
        ));
    }

    /// Task 1586, the comparison of the receipt: N = 10 ended runs of
    /// tasks that are over, M = 6 of them with nothing left once the job
    /// passed them (the job cleaned them, or there was nothing). The job
    /// reads each candidate alone (10 reads) and never lists every run;
    /// the loop lists once per sweep. The second sweep's candidates are
    /// the 4 runs something is left of.
    #[test]
    fn a_job_reads_each_candidate_alone_and_the_next_sweep_leaves_the_settled_out() {
        const N: usize = 10;
        const M: usize = 6;
        let runs: Vec<EndedRunWorktree> = (0..N)
            .map(|i| candidate(&format!("run-{i}"), TaskStatus::Completed))
            .collect();
        let cleaning = Mutex::new(Cleaning::default());
        let stop = AtomicBool::new(false);
        let all = Request {
            all: true,
            ..Request::default()
        };
        let mut lists = 0;
        let mut sweep = |cleaning: &Mutex<Cleaning>| {
            lists += 1;
            let mut guard = lock_cleaning(cleaning);
            let picked = pick_candidates(runs.clone(), &all, |_| false, &mut guard.settled);
            guard.reserved = picked.iter().map(|w| w.run_id.clone()).collect();
            drop(guard);
            let picked_count = picked.len();
            let mut reads = Vec::new();
            pass_candidates(
                cleaning,
                &stop,
                picked,
                |run| {
                    reads.push(run.clone());
                    Ok(runs.iter().find(|w| w.run_id == *run).cloned())
                },
                // The first M have nothing left once passed.
                |candidate, _| runs[..M].contains(candidate),
            );
            (picked_count, reads.len())
        };
        assert_eq!(sweep(&cleaning), (N, N));
        assert_eq!(sweep(&cleaning), (N - M, N - M));
        assert_eq!(lists, 2);
        let guard = lock_cleaning(&cleaning);
        assert!(guard.reserved.is_empty());
        assert_eq!(guard.settled.len(), M);
    }

    /// Task 1586: the job skips a candidate leased, or moved on, since it
    /// was picked, and settles none of them; a run it could not read
    /// again is skipped too; a stop ends the pass with the rest unread.
    #[test]
    fn a_job_skips_a_candidate_that_moved_on_since_it_was_picked() {
        let picked: Vec<EndedRunWorktree> = ["same", "leased", "moved", "unread", "after"]
            .map(|run| candidate(run, TaskStatus::Completed))
            .to_vec();
        let cleaning = Mutex::new(Cleaning {
            reserved: picked.iter().map(|w| w.run_id.clone()).collect(),
            ..Cleaning::default()
        });
        let stop = AtomicBool::new(false);
        let mut cleaned = Vec::new();
        pass_candidates(
            &cleaning,
            &stop,
            picked.clone(),
            |run| match run.as_str() {
                "leased" => Ok(None),
                "moved" => Ok(Some(EndedRunWorktree {
                    task_status: TaskStatus::Canceled,
                    ..candidate("moved", TaskStatus::Completed)
                })),
                "unread" => {
                    stop.store(true, Ordering::SeqCst);
                    Err(anyhow::anyhow!("busy"))
                }
                _ => Ok(picked.iter().find(|w| w.run_id == *run).cloned()),
            },
            |candidate, _| {
                cleaned.push(candidate.run_id.as_str().to_owned());
                true
            },
        );
        assert_eq!(cleaned, ["same"]);
        let guard = lock_cleaning(&cleaning);
        assert!(guard.reserved.is_empty());
        assert_eq!(
            guard.settled.keys().map(RunId::as_str).collect::<Vec<_>>(),
            ["same"]
        );
    }

    /// Task 1586: a settled run is a candidate again once the queue lists
    /// it otherwise (its task moved on, or a lease hid it), once a slot
    /// holds it, or once the loop leases it (a resume makes its worktree
    /// again); while the job reserves a run, the loop may not lease it.
    #[test]
    fn a_settled_run_is_a_candidate_again_once_it_may_have_a_worktree_again() {
        let run = candidate("r", TaskStatus::InProgress);
        let all = Request {
            all: true,
            ..Request::default()
        };
        let settled_with =
            |then: &EndedRunWorktree| HashMap::from([(then.run_id.clone(), then.clone())]);
        // Listed the same: left out, and still settled.
        let mut settled = settled_with(&run);
        assert!(pick_candidates(vec![run.clone()], &all, |_| false, &mut settled).is_empty());
        assert_eq!(settled.len(), 1);
        // Its task is over now: its branch goes.
        let over = EndedRunWorktree {
            task_status: TaskStatus::Completed,
            ..run.clone()
        };
        assert_eq!(
            pick_candidates(vec![over.clone()], &all, |_| false, &mut settled),
            [over]
        );
        assert!(settled.is_empty());
        // Not listed (leased by another supervisor meanwhile), then listed
        // the same again.
        let mut settled = settled_with(&run);
        assert!(pick_candidates(Vec::new(), &all, |_| false, &mut settled).is_empty());
        assert_eq!(
            pick_candidates(vec![run.clone()], &all, |_| false, &mut settled),
            std::slice::from_ref(&run)
        );
        // A slot holds it: no candidate, and no longer settled.
        let mut settled = settled_with(&run);
        assert!(pick_candidates(vec![run.clone()], &all, |_| true, &mut settled).is_empty());
        assert!(settled.is_empty());
        // The loop leases it.
        let mut cleaning = Cleaning {
            settled: settled_with(&run),
            ..Cleaning::default()
        };
        assert!(cleaning.may_lease(&run.run_id));
        assert!(cleaning.settled.is_empty());
        cleaning.reserved.push(run.run_id.clone());
        cleaning.settled = settled_with(&run);
        assert!(!cleaning.may_lease(&run.run_id));
        assert_eq!(cleaning.settled.len(), 1);
    }

    /// Task 1587: a worktree is removed as a directory only when its task
    /// is over, it is the run's own under the runs directory, and Git
    /// failed on it because its `.git` is broken.
    #[test]
    fn only_a_broken_git_of_an_ended_task_under_the_runs_dir_lets_the_directory_go() {
        let runs = Path::new("/runs");
        let repo = Path::new("/repo");
        // As Git said it of the worktrees of 2026-10-03.
        let unregistered =
            "\"git\" failed (exit status: 128): fatal: '/runs/r/worktree' is not a working tree";
        let broken = "repair worktree /runs/r/worktree: \"git\" failed (exit status: 1): error: unable to locate repository; .git file broken: /runs/r/worktree/.git";
        let over = candidate("r", TaskStatus::Completed);
        assert!(may_remove_broken(&over, runs, repo, unregistered, broken));
        assert!(may_remove_broken(
            &candidate("r", TaskStatus::Canceled),
            runs,
            repo,
            unregistered,
            broken
        ));
        // Registered, with a `.git` that is no gitfile or none at all.
        for removal in [
            "fatal: validation failed, cannot remove working tree: '/runs/r/worktree/.git' is not a .git file, error code 5",
            "fatal: validation failed, cannot remove working tree: '/runs/r/worktree/.git' does not exist",
        ] {
            assert!(
                may_remove_broken(&over, runs, repo, removal, broken),
                "{removal}"
            );
        }
        // A task that goes on keeps its worktree.
        for status in [TaskStatus::InProgress, TaskStatus::Ready] {
            assert!(!may_remove_broken(
                &candidate("r", status),
                runs,
                repo,
                unregistered,
                broken
            ));
        }
        // Outside the runs directory, not the run's own, or holding the
        // repository's checkout.
        for worktree in [
            "/elsewhere/r/worktree",
            "/runs/other/worktree",
            "/runs/r/worktree/sub",
            "/runs/r",
        ] {
            let moved = EndedRunWorktree {
                worktree: worktree.to_owned(),
                ..over.clone()
            };
            assert!(
                !may_remove_broken(&moved, runs, repo, unregistered, broken),
                "{worktree}"
            );
        }
        assert!(!may_remove_broken(
            &over,
            runs,
            Path::new("/runs/r/worktree/repo"),
            unregistered,
            broken
        ));
        // Any other failure of Git.
        let locked = "fatal: cannot remove a locked working tree";
        let busy = "error: could not lock config file: File exists";
        assert!(!may_remove_broken(&over, runs, repo, locked, broken));
        assert!(!may_remove_broken(&over, runs, repo, unregistered, busy));
        assert!(!may_remove_broken(&over, runs, repo, unregistered, ""));
    }
}
