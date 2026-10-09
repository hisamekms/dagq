//! The cleanup of ended runs' worktrees off the supervisor's loop (task
//! 405): measuring and removing gigabytes of build outputs took the loop
//! away from the live runs, so a job thread does it. The loop picks the
//! candidates ([`HostOpsState::request_cleanup`]), and joins the job on a
//! later pass ([`HostOpsState::poll_cleanup`]) to record what it removed.
//!
//! One job runs at a time; what is asked for meanwhile waits for the next
//! one, so no worktree is cleaned twice at once. The loop does not wait
//! for a job, but for a cleanup for disk space and the rest of one another
//! job took on, which the claims and landings short of room wait for, and
//! for the last job once it ends.
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
//!
//! A cleanup for disk space, and the rest of one, also clears the build
//! caches of the queue's directory ([`BUILD_CACHES`]): the target
//! directories of the automatic update and of the landing recheck, which
//! are built again (sccache shares the compiles). Each only while its lock
//! is free ([`clear_build_cache`]): the job holds it while it clears, so
//! what builds there waits, and leaves a cache in use for the next cleanup.

use super::*;
use crate::application::update::{self, UpdatePaths};
use crate::application::{EndedRunWorktree, RUN_TMP_DIR, WorktreeCleanup};
use crate::domain::EventKind;
use crate::domain::disk::worktree_executables;
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
    /// The [`BUILD_CACHES`] are cleared too (for disk space).
    caches: bool,
    /// What is removed counts for room as `auto_repaired`: the rest of a
    /// cleanup for room that another job took on. While the disk is short,
    /// nothing is held or asked for until it is done, but a claim or a
    /// landing there is room for does not wait for it (task 1289, task
    /// 1478).
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
            self.caches = true;
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
    deferred: bool,
    /// The paths the job could not clean: retried on every cleanup, their
    /// `cleanup_failed` recorded once per process.
    failed: Vec<String>,
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
    /// A cleanup for disk space runs or waits to, or the rest of one
    /// another job took on: the disk is judged on the reading after it, so
    /// nothing is held or asked for while it may still free room (task
    /// 1478), and a drain decides its landings on that reading (task 1426).
    pub(super) fn for_disk(&self) -> bool {
        disk_cleanup_pending(
            self.job.as_ref().map(|job| (job.disk, job.counted)),
            &self.pending,
        )
    }
    /// The lock the loop holds while it leases an ended run, and the runs
    /// the job has yet to pass, which the loop leaves for a later pass.
    pub(super) fn cleaning(&self) -> Arc<Mutex<Cleaning>> {
        self.cleaning.clone()
    }
    /// The loop left a run the job reserved for a later pass: it waits for
    /// the job, as for a run, until the job is joined.
    pub(super) const fn defer(&mut self) {
        self.deferred = true;
    }
    /// A run was left for the job ([`Self::defer`]) and the job is not
    /// joined yet.
    pub(super) const fn deferred(&self) -> bool {
        self.deferred
    }
    /// Take a request for the next job (see
    /// [`HostOpsState::request_cleanup`]); whether it was taken: none is
    /// once ending (a stop or a handoff).
    fn take(&mut self, task: Option<TaskId>, disk: Option<DiskRequest>) -> bool {
        if self.ending {
            return false;
        }
        // For room while another job runs: that job counts for it, and the
        // rest (the runs it did not pick, and the prune) follows. The build
        // outputs of the runs nobody works on that wait for no answer (task
        // 1289), which only a cleanup for room removes, go in that rest,
        // which counts what it removes for room too; while the disk is
        // short, nothing is held or asked for until it is done (task 1478).
        // While a cleanup for room waits or runs, no other is added (task
        // 1627).
        if let Some(request) = disk {
            add_disk_request(
                self.job.as_mut().map(|job| (&mut job.disk, job.counted)),
                &mut self.pending,
                request,
            );
        }
        if disk.is_none() || task.is_some() {
            self.pending.add(task, None);
        }
        true
    }
    /// End the cleanup (see [`HostOpsState::end_cleanup`]): take no more
    /// requests, let an ordinary job stop after its current worktree, and
    /// keep only the [`ending_rest`] of what waits.
    fn end(&mut self) {
        self.ending = true;
        if !self
            .job
            .as_ref()
            .is_some_and(|job| for_room(job.disk, job.counted).is_some())
        {
            self.stop.store(true, Ordering::SeqCst);
        }
        self.pending = ending_rest(std::mem::take(&mut self.pending));
    }
    /// Take requests again after [`Self::end`] (see
    /// [`HostOpsState::resume_cleanup`]): whether it had ended, so every
    /// ended run is to be asked for.
    fn resume(&mut self) -> bool {
        if !self.ending {
            return false;
        }
        self.ending = false;
        self.stop.store(false, Ordering::SeqCst);
        true
    }
    /// Whether the failure to clean `path` is recorded: once per process,
    /// and only logged when it fails again.
    fn first_failure(&mut self, path: &str) -> bool {
        if self.failed.iter().any(|failed| failed == path) {
            return false;
        }
        self.failed.push(path.to_owned());
        true
    }
}

/// Whether cleanup may still free room; draining does not alter this decision.
fn disk_cleanup_pending(
    job: Option<(Option<DiskRequest>, Option<DiskRequest>)>,
    pending: &Request,
) -> bool {
    job.is_some_and(|(disk, counted)| for_room(disk, counted).is_some())
        || for_room(pending.disk, pending.counted).is_some()
}

/// The cleanup for room a job or a request counts for: its own
/// (`disk`), or the one another job took on whose rest it is (`counted`).
/// Such a job goes to its last candidate through a stop or a handoff, and
/// records `auto_repaired` once joined; an ordinary one does neither.
const fn for_room(disk: Option<DiskRequest>, counted: Option<DiskRequest>) -> Option<DiskRequest> {
    match disk {
        Some(disk) => Some(disk),
        None => counted,
    }
}

/// Whether this pass ends the cleanup: a stop or a handoff does. A drain
/// on a provisioning failure (claiming stopped, neither a stop nor a
/// handoff) does not: the cleanup takes requests and its jobs go on as
/// usual (task 1636).
pub(super) const fn ends_cleanup(stopping: bool, handing_off: bool) -> bool {
    stopping || handing_off
}

/// What waits for the next job once the cleanup ends (a stop or a
/// handoff): only the rest of a cleanup for room another job took on
/// (task 1426), which goes to every ended run with the prune, the runs
/// nobody works on that wait for no answer and the build caches; the rest
/// is dropped.
fn ending_rest(pending: Request) -> Request {
    pending
        .counted
        .map_or_else(Request::default, |counted| Request {
            all: true,
            prune: true,
            idle: true,
            caches: true,
            counted: Some(counted),
            ..Request::default()
        })
}

/// Take a cleanup for room on (task 1627): while one waits or runs (the
/// rest of one another job took on included), nothing is added, as it is
/// the one the next reading follows. An ordinary job running takes it on
/// (`disk`, the job's), and its rest, every ended run with the prune, the
/// runs nobody works on that wait for no answer and the build caches,
/// waits for the next job, counted for room (task 1289, task 1478). With no
/// job running it waits for the next. So one cleanup for room runs as one job, or as the
/// job it rode on and its rest: no rest follows a rest.
fn add_disk_request(
    job: Option<(&mut Option<DiskRequest>, Option<DiskRequest>)>,
    pending: &mut Request,
    request: DiskRequest,
) {
    let running = job.as_ref().map(|(disk, counted)| (**disk, *counted));
    if disk_cleanup_pending(running, pending) {
        return;
    }
    if let Some((disk, _)) = job {
        *disk = Some(request);
        pending.add(None, None);
        pending.prune = true;
        pending.idle = true;
        pending.caches = true;
        pending.counted = Some(request);
        return;
    }
    pending.add(None, Some(request));
}

fn task_over(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Completed | TaskStatus::Canceled)
}

fn safe_worktree(worktree: &Path, runs_dir: &Path, repo_root: &Path) -> bool {
    worktree.starts_with(runs_dir) && !repo_root.starts_with(worktree)
}

/// The build-output event and the suffix of its log message. Ordinary cleanup
/// records the event too; only its job's disk/counting request makes a repair.
fn build_outputs_record(cleanup: WorktreeCleanup, paths: &[String], bytes: u64) -> (String, Value) {
    let (reason, why) = match cleanup {
        WorktreeCleanup::Ended => (BUILD_OUTPUTS_RUN_ENDED, String::new()),
        WorktreeCleanup::AwaitingAnswer(ask) => (
            BUILD_OUTPUTS_AWAITING_ANSWER,
            format!(", waiting for the answer to ask {ask}"),
        ),
        WorktreeCleanup::Idle => (BUILD_OUTPUTS_DISK_SPACE, ", for disk space".to_owned()),
    };
    let mut payload = json!({"paths": paths, "bytes": bytes, "by": "supervisor", "reason": reason});
    if let WorktreeCleanup::AwaitingAnswer(ask) = cleanup {
        payload["ask_id"] = json!(ask);
    }
    (why, payload)
}

/// How a worktree went ([`Outcome::Worktree`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Removed {
    missing: bool,
    repaired: bool,
    broken_git: bool,
}

/// The payload of `worktree_removed`: the reason is the task's status, and
/// each of `removed`, the processes stopped and why they could not be
/// listed is named only when there is one.
fn worktree_record(
    task_status: TaskStatus,
    path: &str,
    branch: &str,
    bytes: u64,
    removed: Removed,
    stopped: &[Value],
    unlisted: Option<&str>,
) -> Value {
    let reason = format!("task_{}", task_status.as_str());
    let mut payload = json!({"path": path, "branch": branch, "bytes": bytes, "by": "supervisor", "reason": reason});
    if removed.missing {
        payload["worktree_missing"] = json!(true);
    }
    if removed.repaired {
        payload["repaired"] = json!(true);
    }
    if removed.broken_git {
        payload["broken_git"] = json!(true);
    }
    if !stopped.is_empty() {
        payload["stopped_processes"] = json!(stopped);
    }
    if let Some(unlisted) = unlisted {
        payload["processes_unlisted"] = json!(unlisted);
    }
    payload
}

/// The payload of what goes once a run's task is over beside its worktree
/// (`scratchpad_removed`, `run_tmp_removed`): the reason is the task's
/// status.
fn task_over_record(task_status: TaskStatus, paths: &[String], bytes: u64) -> Value {
    json!({"paths": paths, "bytes": bytes, "by": "supervisor", "reason": format!("task_{}", task_status.as_str())})
}

/// The message and the payload of `cleanup_failed` for `what` (`worktree`,
/// `scratchpad` or `run tmp`) at `path`.
fn failed_record(what: &str, path: &str, error: &anyhow::Error) -> (String, Value) {
    let message = format!("{what} {path} could not be cleaned: {error:#}");
    let payload = reason_of_error(error, ReasonCode::Other)
        .on(json!({"path": path, "message": message, "by": "supervisor"}));
    (message, payload)
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
    let over = task_over(candidate.task_status);
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

/// A build cache of the queue's directory: a target directory something
/// builds into under its owner's lock, which the cleanup for room clears
/// through its owner ([`Self::clear_under_lock`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildCache {
    /// The automatic update's target (host operation's own).
    Update,
    /// The landing recheck's target, which the recheck publishes.
    Recheck,
}

/// The build caches a cleanup for room clears. Their owners clear the
/// target alone: the rest of their directories (the update's release,
/// staged build and checkout, the recheck's scratch worktree) stays.
const BUILD_CACHES: [BuildCache; 2] = [BuildCache::Update, BuildCache::Recheck];

impl BuildCache {
    /// `build_cache_removed`'s `cache`.
    const fn name(self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::Recheck => "recheck",
        }
    }
    /// Run `clear` on the target of the queue at `queue_dir` under its
    /// owner's lock, taken without waiting ([`update::clear_target`],
    /// [`super::recheck::clear_target`]): `None` with no target, or while
    /// it is in use.
    fn clear_under_lock<R>(
        self,
        files: &dyn RunFiles,
        queue_dir: &Path,
        clear: impl FnOnce(&Path) -> R,
    ) -> Result<Option<R>> {
        match self {
            Self::Update => update::clear_target(files, &UpdatePaths::under(queue_dir), clear),
            Self::Recheck => super::recheck::clear_target(files, queue_dir, clear),
        }
    }
}

/// Clear what `cache`'s target directory holds, keeping the directory:
/// nothing when there is none (nothing built there, or a link, which is
/// not followed), and nothing while it is in use (the next cleanup tries
/// again). Its owner's lock is held until the target is cleared, so
/// nothing starts to build there meanwhile. The target is measured before
/// it goes. A lock that cannot be taken fails at the cache's directory.
fn clear_build_cache(files: &dyn RunFiles, queue_dir: &Path, cache: BuildCache) -> Option<Outcome> {
    let cleared = cache.clear_under_lock(files, queue_dir, |target| {
        let removed = remove_tree(files, target);
        // Only its contents go: what builds there next finds it.
        if matches!(removed, Ok(Some(_)))
            && let Err(error) = files.create_dir_all(target)
        {
            warn!(
                "the build cache {} could not be made again: {error}",
                target.display()
            );
        }
        (target.to_string_lossy().into_owned(), removed)
    });
    match cleared {
        Ok(None) | Ok(Some((_, Ok(None)))) => None,
        Ok(Some((path, Ok(Some(bytes))))) => Some(Outcome::BuildCache {
            name: cache.name(),
            path,
            bytes,
        }),
        Ok(Some((path, Err(error)))) => Some(Outcome::CacheFailed { path, error }),
        Err(error) => Some(Outcome::CacheFailed {
            path: queue_dir.join(cache.name()).to_string_lossy().into_owned(),
            error,
        }),
    }
}

/// The payload of `build_cache_removed`: which cache, its target and the
/// bytes it took.
fn build_cache_record(name: &str, path: &str, bytes: u64) -> Value {
    json!({"cache": name, "paths": [path], "bytes": bytes, "by": "supervisor", "reason": BUILD_OUTPUTS_DISK_SPACE})
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
        /// What ran from under the worktree and was stopped before its
        /// removal (task 1590, [`stop_worktree_processes`]).
        stopped: Vec<Value>,
        /// Why the processes could not be listed, when they could not.
        unlisted: Option<String>,
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
    /// A build cache of the queue's directory cleared ([`clear_build_cache`]).
    BuildCache {
        name: &'static str,
        path: String,
        bytes: u64,
    },
    /// A build cache that could not be cleared.
    CacheFailed { path: String, error: anyhow::Error },
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
    processes: Arc<dyn ProcessControl + Send + Sync>,
    repository: Arc<dyn Repository + Send + Sync>,
    queues: Arc<dyn QueueOpener>,
    runs_dir: PathBuf,
    repo_root: PathBuf,
    cleaning: Arc<Mutex<Cleaning>>,
    stop: Arc<AtomicBool>,
    prune: bool,
    /// Where Claude Code keeps the sessions' scratchpads (task 1100).
    scratchpad_roots: Vec<PathBuf>,
    /// The queue's directory, whose [`BUILD_CACHES`] the job clears (for
    /// disk space).
    caches: Option<PathBuf>,
}

impl HostOpsState {
    /// Ask for the worktrees of ended runs to be cleaned, every such run's
    /// or only `task`'s (task 376), for disk space when `disk` is given,
    /// off the loop (task 405: a job thread does it). It starts at once
    /// unless a job runs; then it waits for the next one. The runs the
    /// slots hold (`held`) are left out. Whether it was taken: none is once
    /// ending (a stop or a handoff).
    ///
    /// A run nobody leases and no slot holds qualifies once it is
    /// `integrated`, `succeeded`, `failed` or `interrupted`, or whatever
    /// its status once its task is over:
    ///
    /// - its task `completed` or `canceled`: the worktree and its branch are
    ///   removed, recorded as `worktree_removed` (`path`, `branch`, `bytes`,
    ///   `by: supervisor`, `reason` `task_completed` / `task_canceled`, and
    ///   `repaired: true` when the worktree had to be repaired first, after
    ///   the queue's rebind, and `broken_git: true` when Git took it for
    ///   no worktree, its `.git` broken, so the run's own
    ///   `<runs>/<run-id>/worktree` directory was removed instead, task
    ///   1587, and `stopped_processes` (`pid`, `executable`, `killed`)
    ///   for what ran from under the run's own worktree and was stopped
    ///   before its removal, or `processes_unlisted` when the processes
    ///   could not be listed, task 1590). A worktree whose directory is already gone
    ///   loses its branch (after `git worktree prune`), recorded the same
    ///   with `bytes` 0 and `worktree_missing: true`;
    /// - otherwise (the task may run it again, or retry it on a new run):
    ///   only the build outputs ([`BUILD_OUTPUT_DIRS`]) go, and the sources,
    ///   commits and run directory stay; recorded as
    ///   `build_outputs_removed` (`paths`, `bytes`, `by: supervisor`,
    ///   `reason: run_ended`).
    ///
    /// The build outputs of an `awaiting_integration` or `needs_session`
    /// run of a task that goes on, with no lease and no live session, go
    /// too (task 1289): on every cleanup while an ask of it waits for an
    /// answer (`reason: awaiting_answer`, with its `ask_id`), else only in
    /// a cleanup for disk space (`reason: disk_space`).
    ///
    /// `bytes` is what the removed files took on disk. Only a worktree
    /// under the run directory is touched, never the checkout the
    /// supervisor was given. A failure records `cleanup_failed` (`path`,
    /// `message`, `by: supervisor`) once per worktree and process, is
    /// retried on the next sweep, and the others go on.
    pub(super) fn request_cleanup(
        &mut self,
        env: &mut PassEnv<'_>,
        held: &[RunId],
        task: Option<TaskId>,
        disk: Option<DiskRequest>,
    ) -> bool {
        if !self.cleanup.take(task, disk) {
            return false;
        }
        self.start_cleanup(env, held);
        true
    }
    /// Join a finished job and record what it did, then start what waits;
    /// with `ending` (a stop or a handoff), let the job end after its
    /// current worktree (all candidates for disk space) and start nothing
    /// more but the rest of a cleanup for room another job took on, which
    /// goes to its last candidate too (task 1426).
    pub(super) fn poll_cleanup(&mut self, env: &mut PassEnv<'_>, held: &[RunId], ending: bool) {
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
        let cleaned = self.record_cleanup(env, outcomes);
        if let Some(disk) = for_room(job.disk, job.counted) {
            self.cleaned_for_disk(env, disk, &cleaned);
            // The next cleanup for room waits its interval from here, so
            // the pass after it judges the reading after it (task 1627).
            self.disk.cleaned = Some(Instant::now());
        }
        self.start_cleanup(env, held);
    }
    /// A stop or a handoff: take no more requests, let an ordinary job end
    /// after its current worktree, and drop what waits but the rest of a
    /// cleanup for room another job took on. No job is joined: a reading
    /// of the disk taken before stays the one of the cleanup in progress.
    pub(super) fn end_cleanup(&mut self) {
        self.cleanup.end();
    }
    /// A handoff withdrawn while this process drained, which goes back to
    /// claims (task 1427): requests are taken again, an ordinary job that
    /// has not seen the stop yet goes on, and every ended run is asked for
    /// at once, which picks up what the drain dropped. A job that stopped
    /// after its current worktree leaves the rest to that request.
    pub(super) fn resume_cleanup(&mut self, env: &mut PassEnv<'_>, held: &[RunId]) {
        if self.cleanup.resume() {
            self.request_cleanup(env, held, None, None);
        }
    }
    /// Once the loop ended: wait for the job and whatever waits for the
    /// next one, and record what they did. After a stop, only the running
    /// job and the rest of a cleanup for room it took on remain; ordinary
    /// cleanup stops after its current worktree.
    pub(super) fn finish_cleanup(&mut self, env: &mut PassEnv<'_>, held: &[RunId]) {
        while let Some(job) = &self.cleanup.job {
            while !job.handle.is_finished() {
                thread::sleep(Duration::from_millis(20));
            }
            self.poll_cleanup(env, held, false);
        }
    }
    /// Start a job for what waits, unless one runs: the candidates are
    /// picked here, on the loop, less the runs a slot holds.
    fn start_cleanup(&mut self, env: &mut PassEnv<'_>, held: &[RunId]) {
        if self.cleanup.job.is_some() || self.cleanup.pending.is_empty() {
            return;
        }
        let request = std::mem::take(&mut self.cleanup.pending);
        let listed = match env.queue.ended_run_worktrees() {
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
            |run| held.contains(run),
            &mut cleaning.settled,
        );
        if candidates.is_empty() && !request.prune {
            return;
        }
        cleaning.reserved = candidates.iter().map(|w| w.run_id.clone()).collect();
        drop(cleaning);
        let ports = JobPorts {
            files: env.files.clone(),
            processes: env.processes.clone(),
            repository: env.repository.clone(),
            queues: env.queues.clone(),
            runs_dir: env.layout.runs_dir.clone(),
            repo_root: env.layout.repo_root.clone(),
            cleaning: self.cleanup.cleaning.clone(),
            // A cleanup for room goes to its last candidate: it is not
            // stopped with ordinary cleanup.
            stop: if for_room(request.disk, request.counted).is_some() {
                Arc::new(AtomicBool::new(false))
            } else {
                self.cleanup.stop.clone()
            },
            prune: request.prune,
            scratchpad_roots: (self.scratchpad_roots)(),
            caches: env
                .layout
                .db
                .parent()
                .filter(|_| request.caches)
                .map(Path::to_path_buf),
        };
        let handle = spawn_traced(move || run_job(&ports, candidates));
        self.cleanup.job = Some(Job {
            handle,
            disk: request.disk,
            counted: request.counted,
        });
    }
    /// Record the events of what the job did; what it removed.
    fn record_cleanup(&mut self, env: &mut PassEnv<'_>, outcomes: Vec<Outcome>) -> Cleaned {
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
                    let (why, payload) = build_outputs_record(cleanup, &paths, bytes);
                    info!(run_id = %run_id, "run {run_id} is {}{why}; removed the build outputs of its worktree ({bytes} bytes)", status.as_str());
                    cleaned.add(&run_id, bytes);
                    env.queue
                        .record_runtime_event(&run_id, EventKind::BuildOutputsRemoved, payload)
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
                    stopped,
                    unlisted,
                } => {
                    let payload = worktree_record(
                        task_status,
                        &path,
                        &branch,
                        bytes,
                        Removed {
                            missing,
                            repaired,
                            broken_git,
                        },
                        &stopped,
                        unlisted.as_deref(),
                    );
                    if missing {
                        info!(run_id = %run_id, task_id = %task_id, "task {task_id} is {}; the worktree {path} of run {run_id} was already gone: removed its branch {branch}", task_status.as_str());
                    } else {
                        info!(run_id = %run_id, task_id = %task_id, "task {task_id} is {}; removed worktree {path} and branch {branch} of run {run_id} ({bytes} bytes)", task_status.as_str());
                    }
                    if broken_git {
                        info!(run_id = %run_id, "the worktree {path} of run {run_id} had a broken .git: removed its directory");
                    }
                    if !stopped.is_empty() {
                        info!(run_id = %run_id, "stopped {} process(es) running from the worktree {path} of run {run_id} before its removal", stopped.len());
                    }
                    if let Some(unlisted) = unlisted {
                        warn!(run_id = %run_id, "the processes running from the worktree {path} of run {run_id} could not be listed before its removal: {unlisted}");
                    }
                    cleaned.add(&run_id, bytes);
                    env.queue
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
                    env.queue.record_runtime_event(
                        &run_id,
                        EventKind::ScratchpadRemoved,
                        task_over_record(task_status, &paths, bytes),
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
                    env.queue.record_runtime_event(
                        &run_id,
                        EventKind::RunTmpRemoved,
                        task_over_record(task_status, std::slice::from_ref(&path), bytes),
                    )
                }
                Outcome::Failed {
                    run_id,
                    what,
                    path,
                    error,
                } => {
                    if !self.cleanup.first_failure(&path) {
                        warn!(run_id = %run_id, "run {run_id}: {what} {path} still could not be cleaned: {error:#}");
                        continue;
                    }
                    let (message, payload) = failed_record(what, &path, &error);
                    warn!(run_id = %run_id, "run {run_id}: {message}");
                    env.queue
                        .record_runtime_event(&run_id, EventKind::CleanupFailed, payload)
                }
                Outcome::BuildCache { name, path, bytes } => {
                    info!(
                        "the free disk space was short: cleared the {name} build cache {path} ({bytes} bytes)"
                    );
                    cleaned.add_cache(&path, bytes);
                    env.queue
                        .record_queue_event(
                            EventKind::BuildCacheRemoved,
                            build_cache_record(name, &path, bytes),
                        )
                        .map(drop)
                }
                Outcome::CacheFailed { path, error } => {
                    if !self.cleanup.first_failure(&path) {
                        warn!("the build cache {path} still could not be cleared: {error:#}");
                        continue;
                    }
                    let (message, payload) = failed_record("build cache", &path, &error);
                    warn!("{message}");
                    env.queue
                        .record_queue_event(EventKind::CleanupFailed, payload)
                        .map(drop)
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
    let mut outcomes = outcomes;
    if let Some(queue_dir) = &ports.caches {
        outcomes.extend(
            BUILD_CACHES
                .iter()
                .filter_map(|cache| clear_build_cache(&*ports.files, queue_dir, *cache)),
        );
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
    let runner = !runner_goes(candidate.cleanup) || remove_run_runner(ports, candidate);
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

/// Whether the job removes a candidate's runner: only an ended run's
/// ([`WorktreeCleanup::Ended`]). A run in the middle (waiting for an
/// answer, a landing or a resume) keeps it, as its session may go on.
fn runner_goes(cleanup: WorktreeCleanup) -> bool {
    cleanup == WorktreeCleanup::Ended
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

/// The directories of an ended run's Claude Code scratchpads (task 1100):
/// the one named [`scratchpad_dir_name`] after its worktree, the cwd of its
/// session, under each of the scratchpad `roots`, once its task is over. A
/// run whose task goes on keeps them, as a resume may go on in them, and
/// so does a worktree outside the runs directory.
fn scratchpad_dirs(
    candidate: &EndedRunWorktree,
    runs_dir: &Path,
    roots: &[PathBuf],
) -> Vec<PathBuf> {
    if !task_over(candidate.task_status) || !Path::new(&candidate.worktree).starts_with(runs_dir) {
        return Vec::new();
    }
    scratchpad_dir_name(&candidate.worktree)
        .map(|name| roots.iter().map(|root| root.join(&name)).collect())
        .unwrap_or_default()
}

/// The temporary files directory ([`RUN_TMP_DIR`]) of an ended run, in its
/// run directory, once its task is over (task 1290): the `TMPDIR` the
/// runtime gave its Codex turns. A run whose task goes on keeps it, as a
/// resume may go on in it; nothing else of the run directory goes.
fn run_tmp_dir(candidate: &EndedRunWorktree, runs_dir: &Path) -> Option<PathBuf> {
    task_over(candidate.task_status)
        .then(|| runs_dir.join(candidate.run_id.as_str()).join(RUN_TMP_DIR))
}

/// Measure and remove the directory `dir`; its bytes, or `None` for no
/// directory there, a link (left alone, nothing outside it is followed),
/// or one gone before its removal (another supervisor's cleanup, or
/// Claude Code's), which is no failure.
fn remove_tree(files: &dyn RunFiles, dir: &Path) -> Result<Option<u64>> {
    let Some(size) = files
        .tree_size(dir)
        .with_context(|| format!("measure {}", dir.display()))?
    else {
        return Ok(None);
    };
    match files.remove_dir_all(dir) {
        Ok(()) => Ok(Some(size)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(anyhow::Error::new(error).context(format!("remove {}", dir.display()))),
    }
}

/// Remove the [`scratchpad_dirs`] of an ended run. What was removed is
/// pushed to `outcomes` with a failure under another root.
fn remove_scratchpads(ports: &JobPorts, candidate: &EndedRunWorktree, outcomes: &mut Vec<Outcome>) {
    let mut paths = Vec::new();
    let mut bytes = 0;
    for dir in scratchpad_dirs(candidate, &ports.runs_dir, &ports.scratchpad_roots) {
        match remove_tree(&*ports.files, &dir) {
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

/// Remove the [`run_tmp_dir`] of an ended run.
fn remove_run_tmp(ports: &JobPorts, candidate: &EndedRunWorktree, outcomes: &mut Vec<Outcome>) {
    let Some(dir) = run_tmp_dir(candidate, &ports.runs_dir) else {
        return;
    };
    let path = dir.to_string_lossy().into_owned();
    match remove_tree(&*ports.files, &dir) {
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

/// Clean one ended run's worktree (see [`HostOpsState::request_cleanup`]).
fn clean_worktree(
    ports: &JobPorts,
    candidate: &EndedRunWorktree,
    branches: &mut Option<Vec<String>>,
    pruned: &mut bool,
) -> Result<Option<Outcome>> {
    let worktree = Path::new(&candidate.worktree);
    if !safe_worktree(worktree, &ports.runs_dir, &ports.repo_root) {
        return Ok(None);
    }
    let over = task_over(candidate.task_status);
    let removed = |bytes, missing, removal, branch: &str, stopped, unlisted| Outcome::Worktree {
        run_id: candidate.run_id.clone(),
        task_id: candidate.task_id,
        task_status: candidate.task_status,
        path: candidate.worktree.clone(),
        branch: branch.to_owned(),
        bytes,
        missing,
        repaired: removal == Removal::Repaired,
        broken_git: removal == Removal::BrokenGit,
        stopped,
        unlisted,
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
        return Ok(Some(removed(
            0,
            true,
            Removal::Removed,
            branch,
            Vec::new(),
            None,
        )));
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
        let (stopped, unlisted) =
            if own_ended_worktree(candidate, &ports.runs_dir, &ports.repo_root) {
                // The executables' paths are read resolved: so is the
                // worktree's, when it can be.
                let resolved = ports
                    .files
                    .canonicalize(worktree)
                    .unwrap_or_else(|_| worktree.to_path_buf());
                match stop_worktree_processes(&*ports.processes, &resolved, WORKTREE_STOP_POLLS) {
                    Ok(stopped) => (stopped, None),
                    Err(error) => (Vec::new(), Some(format!("{error:#}"))),
                }
            } else {
                (Vec::new(), None)
            };
        let removal = remove_worktree(ports, candidate, branch, pruned)?;
        return Ok(Some(removed(
            bytes, false, removal, branch, stopped, unlisted,
        )));
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

/// Whether the run's task is over and its worktree is the run's own
/// `<runs>/<run-id>/worktree`, holding no part of the repository's
/// checkout: the worktree the cleanup stops what runs from under before it
/// removes it (task 1590), and may remove as a directory when Git cannot
/// ([`may_remove_broken`]). The worktree of a run whose task goes on only
/// loses its build outputs, and its processes are left alone; a leased run
/// is no candidate.
fn own_ended_worktree(candidate: &EndedRunWorktree, runs_dir: &Path, repo_root: &Path) -> bool {
    let worktree = Path::new(&candidate.worktree);
    task_over(candidate.task_status)
        && worktree == runs_dir.join(candidate.run_id.as_str()).join("worktree")
        && !repo_root.starts_with(worktree)
}

/// How often [`stop_worktree_processes`] looks whether what it sent a
/// SIGTERM ended, every [`WORKTREE_STOP_POLL`]: 3 seconds in all, the
/// grace the recovery job's `stop_processes` gives. Counted rather than
/// timed, so the wait reads no clock (L5).
const WORKTREE_STOP_POLLS: u32 = 60;
const WORKTREE_STOP_POLL: Duration = Duration::from_millis(50);

/// Stop the processes whose executable is under `worktree`
/// ([`worktree_executables`]: a test's child `dagq` left running, or what
/// a worker's turn left with `&`), SIGTERM and then SIGKILL once `polls`
/// looks ([`WORKTREE_STOP_POLLS`]) found one still running, so none writes
/// into the worktree again once it is removed (task 1590). The processes are listed only here, once a
/// worktree is about to go. Returns what was stopped: the pid, the
/// executable, and whether it took a SIGKILL.
fn stop_worktree_processes(
    processes: &dyn ProcessControl,
    worktree: &Path,
    polls: u32,
) -> Result<Vec<Value>> {
    let all = processes.executables()?;
    let targets = worktree_executables(&all, worktree, std::process::id());
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    for process in &targets {
        if let Err(error) = processes.terminate(process.pid)
            && processes.alive(process.pid)
        {
            warn!(error = %format_args!("{error:#}"), "pid {} running from {} could not be stopped: {error:#}", process.pid, worktree.display());
        }
    }
    for _ in 0..polls {
        if !targets.iter().any(|p| processes.alive(p.pid)) {
            break;
        }
        thread::sleep(WORKTREE_STOP_POLL);
    }
    Ok(targets
        .into_iter()
        .map(|process| {
            let killed = processes.alive(process.pid);
            if killed
                && let Err(error) = processes.kill(process.pid)
                && processes.alive(process.pid)
            {
                warn!(error = %format_args!("{error:#}"), "pid {} running from {} could not be killed: {error:#}", process.pid, worktree.display());
            }
            json!({"pid": process.pid, "executable": process.executable, "killed": killed})
        })
        .collect())
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
    own_ended_worktree(candidate, runs_dir, repo_root)
        && NOT_A_WORKTREE.iter().any(|said| removal.contains(said))
        && repair.contains(BROKEN_GIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISK: DiskRequest = DiskRequest {
        free: Some(1),
        needed: Some(2),
    };

    /// Only a cleanup for room and the rest of one clear the build caches:
    /// an ordinary cleanup, of every ended run or of a task's, does not,
    /// and the rest a stop or a handoff keeps does only when it is the rest
    /// of a cleanup for room.
    #[test]
    fn only_a_cleanup_for_room_and_its_rest_clear_the_build_caches() {
        let mut ordinary = Request::default();
        ordinary.add(None, None);
        ordinary.add(Some(TaskId::new(1)), None);
        assert!(!ordinary.caches);
        let mut room = Request::default();
        room.add(None, Some(DISK));
        assert!(room.caches);
        // Taken on by an ordinary job running: its rest clears them.
        let (mut disk, mut rest) = (None, Request::default());
        add_disk_request(Some((&mut disk, None)), &mut rest, DISK);
        assert!(rest.caches && rest.counted == Some(DISK));
        assert!(ending_rest(rest).caches);
        let mut task = Request::default();
        task.add(Some(TaskId::new(1)), None);
        assert!(!ending_rest(task).caches);
    }

    /// The files of one build cache: whether its target is a directory,
    /// whether its lock is held elsewhere, what it measures, and what was
    /// done to it.
    #[derive(Default)]
    struct CacheFiles {
        dir: bool,
        busy: bool,
        size: Option<u64>,
        fails: bool,
        lock_fails: bool,
        done: Mutex<Vec<String>>,
    }

    impl CacheFiles {
        fn did(&self, what: &str, path: &Path) {
            self.done
                .lock()
                .unwrap()
                .push(format!("{what} {}", path.display()));
        }
        fn done(&self) -> Vec<String> {
            self.done.lock().unwrap().clone()
        }
    }

    impl RunFiles for CacheFiles {
        fn create_dir_all(&self, dir: &Path) -> std::io::Result<()> {
            self.did("create", dir);
            Ok(())
        }
        fn create_new_dir(&self, _: &Path) -> std::io::Result<()> {
            unimplemented!()
        }
        fn write(&self, _: &Path, _: &[u8]) -> std::io::Result<()> {
            unimplemented!()
        }
        fn copy(&self, _: &Path, _: &Path) -> std::io::Result<()> {
            unimplemented!()
        }
        fn read(&self, _: &Path) -> std::io::Result<Vec<u8>> {
            unimplemented!()
        }
        fn read_to_string(&self, _: &Path) -> std::io::Result<String> {
            unimplemented!()
        }
        fn try_lock(&self, path: &Path) -> std::io::Result<Option<Box<dyn std::any::Any + Send>>> {
            self.did("lock", path);
            if self.lock_fails {
                return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
            }
            Ok((!self.busy).then(|| Box::new(()) as Box<dyn std::any::Any + Send>))
        }
        fn modified(&self, _: &Path) -> std::io::Result<std::time::SystemTime> {
            unimplemented!()
        }
        fn read_stamped(&self, _: &Path) -> Result<Option<(std::time::SystemTime, Vec<u8>)>> {
            unimplemented!()
        }
        fn is_file(&self, _: &Path) -> bool {
            unimplemented!()
        }
        fn is_dir(&self, _: &Path) -> bool {
            self.dir
        }
        fn exists(&self, _: &Path) -> bool {
            unimplemented!()
        }
        fn read_dir(&self, _: &Path) -> std::io::Result<Vec<PathBuf>> {
            unimplemented!()
        }
        fn rename(&self, _: &Path, _: &Path) -> std::io::Result<()> {
            unimplemented!()
        }
        fn remove_file(&self, _: &Path) -> std::io::Result<()> {
            unimplemented!()
        }
        fn tree_size(&self, dir: &Path) -> std::io::Result<Option<u64>> {
            self.did("measure", dir);
            Ok(self.size)
        }
        fn remove_dir_all(&self, dir: &Path) -> std::io::Result<()> {
            self.did("remove", dir);
            if self.fails {
                return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
            }
            Ok(())
        }
        fn append_line(&self, _: &Path, _: &str) -> std::io::Result<()> {
            unimplemented!()
        }
        fn canonicalize(&self, _: &Path) -> std::io::Result<PathBuf> {
            unimplemented!()
        }
        fn write_fenced(&self, _: &Path, _: &str, _: &str, _: &Path) -> Result<()> {
            unimplemented!()
        }
        fn now(&self) -> std::time::SystemTime {
            unimplemented!()
        }
    }

    /// A build cache is cleared only when there is a directory and its
    /// owner's lock is free: the update's target under the update lock, the
    /// recheck's under the recheck lock, measured, removed and made again
    /// empty under it, and nothing else of their directories. One in use, a
    /// link (not measured) and a missing one are left with no outcome; one
    /// that cannot be removed is a failure of its target, and a lock that
    /// cannot be taken one of the cache's directory.
    #[test]
    fn a_build_cache_is_cleared_only_while_its_lock_is_free() {
        let q = Path::new("/q");
        for (cache, name, lock, target) in [
            (
                BuildCache::Update,
                "update",
                "/q/update/lock",
                "/q/update/target",
            ),
            (
                BuildCache::Recheck,
                "recheck",
                "/q/recheck/lock",
                "/q/recheck/target",
            ),
        ] {
            let free = CacheFiles {
                dir: true,
                size: Some(7),
                ..CacheFiles::default()
            };
            assert!(matches!(
                clear_build_cache(&free, q, cache),
                Some(Outcome::BuildCache { name: n, ref path, bytes: 7 }) if n == name && path == target
            ));
            assert_eq!(
                free.done(),
                [
                    format!("lock {lock}"),
                    format!("measure {target}"),
                    format!("remove {target}"),
                    format!("create {target}"),
                ]
            );
            let busy = CacheFiles {
                dir: true,
                busy: true,
                size: Some(7),
                ..CacheFiles::default()
            };
            assert!(clear_build_cache(&busy, q, cache).is_none());
            assert_eq!(busy.done(), [format!("lock {lock}")]);
            let missing = CacheFiles::default();
            assert!(clear_build_cache(&missing, q, cache).is_none());
            assert!(missing.done().is_empty());
            let link = CacheFiles {
                dir: true,
                ..CacheFiles::default()
            };
            assert!(clear_build_cache(&link, q, cache).is_none());
            assert_eq!(
                link.done(),
                [format!("lock {lock}"), format!("measure {target}")]
            );
            let failing = CacheFiles {
                dir: true,
                size: Some(7),
                fails: true,
                ..CacheFiles::default()
            };
            assert!(matches!(
                clear_build_cache(&failing, q, cache),
                Some(Outcome::CacheFailed { ref path, .. }) if path == target
            ));
            let unlockable = CacheFiles {
                dir: true,
                lock_fails: true,
                ..CacheFiles::default()
            };
            assert!(matches!(
                clear_build_cache(&unlockable, q, cache),
                Some(Outcome::CacheFailed { ref path, .. }) if path == &format!("/q/{name}")
            ));
        }
    }

    /// `build_cache_removed` names the cache, its target and its bytes,
    /// for disk space.
    #[test]
    fn a_cleared_build_cache_is_recorded_for_disk_space() {
        assert_eq!(
            build_cache_record("recheck", "/q/recheck/target", 9),
            json!({"cache": "recheck", "paths": ["/q/recheck/target"], "bytes": 9, "by": "supervisor", "reason": "disk_space"})
        );
    }

    #[test]
    fn cleanup_requests_merge_tasks_and_all_without_selecting_idle_runs() {
        let mut request = Request::default();
        assert!(request.is_empty());
        request.add(Some(TaskId::new(1)), None);
        request.add(Some(TaskId::new(1)), None);
        request.add(Some(TaskId::new(2)), None);
        assert!(!request.is_empty());
        assert_eq!(request.tasks, [TaskId::new(1), TaskId::new(2)]);
        let run = candidate("r", TaskStatus::InProgress);
        assert!(request.wants(&run));
        let other = EndedRunWorktree {
            task_id: TaskId::new(3),
            ..run.clone()
        };
        assert!(!request.wants(&other));
        let awaiting = EndedRunWorktree {
            cleanup: WorktreeCleanup::AwaitingAnswer(AskId::new(7)),
            ..run.clone()
        };
        let idle = EndedRunWorktree {
            cleanup: WorktreeCleanup::Idle,
            ..run
        };
        assert!(request.wants(&awaiting));
        assert!(!request.wants(&idle));
        request.add(None, None);
        assert!(request.all);
        assert!(request.wants(&other));
        assert!(!request.wants(&idle));
        assert!(!request.prune);
        assert!(request.disk.is_none());
        request.add(Some(TaskId::new(1)), Some(DISK));
        request.add(
            None,
            Some(DiskRequest {
                free: Some(100),
                needed: None,
            }),
        );
        assert!(request.all && request.prune && request.idle);
        assert!(request.wants(&idle));
        assert_eq!(request.disk, Some(DISK));
        assert_eq!(request.counted, None);
        let mut disk_only = Request::default();
        disk_only.add(Some(TaskId::new(1)), Some(DISK));
        assert!(disk_only.wants(&other));
        assert!(!disk_only.is_empty());
    }

    #[test]
    fn disk_cleanup_includes_running_and_pending_disk_or_counted_requests() {
        for job_disk in [None, Some(DISK)] {
            for job_counted in [None, Some(DISK)] {
                for pending_disk in [None, Some(DISK)] {
                    for pending_counted in [None, Some(DISK)] {
                        let pending = Request {
                            disk: pending_disk,
                            counted: pending_counted,
                            ..Request::default()
                        };
                        let expected = job_disk.is_some()
                            || job_counted.is_some()
                            || pending_disk.is_some()
                            || pending_counted.is_some();
                        assert_eq!(
                            disk_cleanup_pending(Some((job_disk, job_counted)), &pending),
                            expected
                        );
                        assert_eq!(
                            disk_cleanup_pending(None, &pending),
                            pending_disk.is_some() || pending_counted.is_some()
                        );
                        // No running thread is needed to test ending or waiting.
                        for ending in [false, true] {
                            let watch = CleanupWatch {
                                ending,
                                pending: Request {
                                    disk: pending_disk,
                                    counted: pending_counted,
                                    ..Request::default()
                                },
                                ..CleanupWatch::default()
                            };
                            assert_eq!(
                                watch.for_disk(),
                                pending_disk.is_some() || pending_counted.is_some()
                            );
                        }
                    }
                }
            }
        }
    }

    /// Task 1627: a cleanup for room is added only while none waits or
    /// runs. An ordinary job running takes it on and its rest waits,
    /// counted for room (task 1478); a job for room, a rest running, or one
    /// waiting takes nothing more and gets no further rest.
    #[test]
    fn a_cleanup_for_room_is_added_only_while_none_waits_or_runs() {
        let other = DiskRequest {
            free: Some(3),
            needed: Some(4),
        };
        // None running and none waiting: it waits for (or starts) a job.
        let mut pending = Request::default();
        add_disk_request(None, &mut pending, DISK);
        assert_eq!(pending.disk, Some(DISK));
        assert_eq!(pending.counted, None);
        assert!(pending.all && pending.prune && pending.idle);
        // An ordinary job running, with an ordinary request waiting: the
        // job takes it on, and its rest waits with that request.
        let mut pending = Request::default();
        pending.add(Some(TaskId::new(5)), None);
        let mut disk = None;
        add_disk_request(Some((&mut disk, None)), &mut pending, DISK);
        assert_eq!(disk, Some(DISK));
        assert_eq!(pending.disk, None);
        assert_eq!(pending.counted, Some(DISK));
        assert!(pending.all && pending.prune && pending.idle);
        assert_eq!(pending.tasks, [TaskId::new(5)]);
        // A cleanup for room running, its rest running, or one waiting:
        // nothing is added.
        for (job, waiting_disk, waiting_counted) in [
            (Some((Some(DISK), None)), None, None),
            (Some((None, Some(DISK))), None, None),
            (Some((Some(DISK), Some(DISK))), None, None),
            (Some((Some(DISK), None)), None, Some(DISK)),
            (Some((None, None)), None, Some(DISK)),
            (Some((None, None)), Some(DISK), None),
            (None, Some(DISK), None),
            (None, None, Some(DISK)),
        ] {
            let case = format!("{job:?} {waiting_disk:?} {waiting_counted:?}");
            let mut pending = Request {
                disk: waiting_disk,
                counted: waiting_counted,
                ..Request::default()
            };
            let mut running = job;
            add_disk_request(
                running.as_mut().map(|(disk, counted)| (disk, *counted)),
                &mut pending,
                other,
            );
            assert_eq!(running, job, "{case}");
            assert_eq!(pending.disk, waiting_disk, "{case}");
            assert_eq!(pending.counted, waiting_counted, "{case}");
            assert!(
                !pending.prune && !pending.idle && pending.is_empty(),
                "{case}"
            );
        }
    }

    /// The loop's passes over a disk that stays short, as the supervisor
    /// makes them: join a job that ended (the end of a cleanup for room
    /// sets when the next may be asked for) and start what waits, then ask
    /// for a cleanup for room when [`asks_for_cleanup`] says so, and judge
    /// the reading when none waits or runs. Each job runs `length`; the
    /// first is an ordinary one with `ordinary_first`. The jobs for room
    /// that ran before the first pass that judged, or `None` when none
    /// judged within `passes`.
    fn jobs_before_a_judgement(
        length: Duration,
        ordinary_first: bool,
        passes: usize,
    ) -> Option<usize> {
        use super::super::disk::asks_for_cleanup;
        let interval = Duration::from_secs(60);
        let tick = Duration::from_secs(1);
        let mut now = Duration::ZERO;
        let mut job: Option<(Option<DiskRequest>, Option<DiskRequest>, Duration)> =
            ordinary_first.then_some((None, None, length));
        let mut pending = Request::default();
        let mut cleaned: Option<Duration> = None;
        let mut jobs = 0;
        let start = |pending: &mut Request, now: Duration| {
            (!pending.is_empty()).then(|| {
                let request = std::mem::take(pending);
                (request.disk, request.counted, now + length)
            })
        };
        for _ in 0..passes {
            if let Some((disk, counted, _)) = job.take_if(|(_, _, end)| *end <= now) {
                if disk.or(counted).is_some() {
                    jobs += 1;
                    cleaned = Some(now);
                }
                job = start(&mut pending, now);
            }
            let in_flight = |job: &Option<(Option<DiskRequest>, Option<DiskRequest>, Duration)>,
                             pending: &Request| {
                disk_cleanup_pending(job.map(|(disk, counted, _)| (disk, counted)), pending)
            };
            if asks_for_cleanup(
                cleaned.map(|at| now - at),
                in_flight(&job, &pending),
                interval,
            ) {
                add_disk_request(
                    job.as_mut().map(|(disk, counted, _)| (disk, *counted)),
                    &mut pending,
                    DISK,
                );
                if job.is_none() {
                    job = start(&mut pending, now);
                }
                cleaned = Some(now);
            }
            if !in_flight(&job, &pending) {
                return Some(jobs);
            }
            now += tick;
        }
        None
    }

    /// Task 1627: however long each job runs (here shorter and far longer
    /// than the interval), a disk that stays short is judged (the disk ask
    /// opened) after one cleanup for room, or after the ordinary job it
    /// rode on and its rest: the chain ends there.
    #[test]
    fn a_cleanup_for_room_chains_at_most_into_its_rest_before_the_disk_is_judged() {
        for length in [30, 61, 180, 6_000].map(Duration::from_secs) {
            assert_eq!(
                jobs_before_a_judgement(length, false, 100_000),
                Some(1),
                "{length:?}"
            );
            assert_eq!(
                jobs_before_a_judgement(length, true, 100_000),
                Some(2),
                "{length:?}"
            );
        }
    }

    /// A job that holds no thread's work: what it counts for is all a
    /// test of the watch reads of it.
    fn job(disk: Option<DiskRequest>, counted: Option<DiskRequest>) -> Job {
        Job {
            handle: thread::spawn(Vec::new),
            disk,
            counted,
        }
    }

    /// A job for room, or the rest of one another job took on, counts for
    /// room: it records `auto_repaired` once joined and goes to its last
    /// candidate through a stop or a handoff. An ordinary job does neither,
    /// so a run whose build outputs an ordinary sweep removed records no
    /// `auto_repaired`.
    #[test]
    fn a_job_for_room_or_its_rest_counts_for_room_and_an_ordinary_one_does_not() {
        let other = DiskRequest {
            free: Some(3),
            needed: None,
        };
        assert_eq!(for_room(None, None), None);
        assert_eq!(for_room(Some(DISK), None), Some(DISK));
        assert_eq!(for_room(None, Some(other)), Some(other));
        // The job's own request is the reading it was asked at.
        assert_eq!(for_room(Some(DISK), Some(other)), Some(DISK));
    }

    /// Only a stop or a handoff ends the cleanup. A drain on a provisioning
    /// failure (claiming stopped, neither) does not: a request asked for
    /// during it (a run that failed in the drain) is taken and waits for
    /// the ordinary job running, which is not stopped (task 1636).
    #[test]
    fn only_a_stop_or_a_handoff_ends_the_cleanup() {
        for (stopping, handing_off, ends) in [
            (false, false, false),
            (true, false, true),
            (false, true, true),
            (true, true, true),
        ] {
            assert_eq!(ends_cleanup(stopping, handing_off), ends);
            let mut watch = CleanupWatch {
                job: Some(job(None, None)),
                ..CleanupWatch::default()
            };
            if ends {
                watch.end();
            }
            assert_eq!(watch.stop.load(Ordering::SeqCst), ends);
            assert_eq!(watch.take(Some(TaskId::new(3)), None), !ends);
            assert_eq!(watch.pending.tasks.is_empty(), ends);
            assert!(watch.running());
        }
    }

    /// Task 648 and task 1426: a stop or a handoff takes no more requests,
    /// stops an ordinary job after its current worktree and drops what
    /// waits, but a job for room or the rest of one runs to its last
    /// candidate, and the rest of a cleanup for room another job took on
    /// still waits for the next job: the drain's landings wait for it, so
    /// too when the rest started (it waits) or finished (its job is not
    /// joined yet) on the pass the handoff is read first.
    #[test]
    fn ending_keeps_only_the_rest_of_a_cleanup_for_room() {
        let mut waiting = Request::default();
        waiting.add(Some(TaskId::new(5)), None);
        waiting.add(None, None);
        let dropped = ending_rest(waiting);
        assert!(dropped.is_empty() && dropped.disk.is_none() && dropped.counted.is_none());
        assert!(!dropped.prune && !dropped.idle);
        let mut rest = Request::default();
        rest.add(Some(TaskId::new(5)), None);
        rest.counted = Some(DISK);
        let kept = ending_rest(rest);
        assert!(kept.all && kept.prune && kept.idle);
        assert!(kept.tasks.is_empty());
        assert_eq!((kept.disk, kept.counted), (None, Some(DISK)));
        // A pending request for room of its own is dropped with the rest.
        assert!(
            ending_rest(Request {
                all: true,
                disk: Some(DISK),
                ..Request::default()
            })
            .is_empty()
        );

        for (job_disk, job_counted, stops) in [
            (None, None, true),
            (Some(DISK), None, false),
            (None, Some(DISK), false),
        ] {
            let case = format!("{job_disk:?} {job_counted:?}");
            let mut watch = CleanupWatch {
                job: Some(job(job_disk, job_counted)),
                ..CleanupWatch::default()
            };
            watch.pending.add(None, None);
            watch.end();
            assert!(watch.ending, "{case}");
            assert_eq!(watch.stop.load(Ordering::SeqCst), stops, "{case}");
            assert!(watch.pending.is_empty(), "{case}");
            assert_eq!(watch.for_disk(), !stops, "{case}");
            assert!(!watch.take(None, Some(DISK)), "{case}");
            assert!(watch.pending.is_empty(), "{case}");
        }
        // An ordinary job that took on a cleanup for room: its rest waits
        // through the end, and the cleanup stays one for room.
        let mut watch = CleanupWatch {
            job: Some(job(None, None)),
            ..CleanupWatch::default()
        };
        assert!(watch.take(None, Some(DISK)));
        assert_eq!(watch.pending.counted, Some(DISK));
        watch.end();
        assert!(!watch.stop.load(Ordering::SeqCst));
        assert_eq!(watch.pending.counted, Some(DISK));
        assert!(watch.pending.all && watch.pending.prune && watch.pending.idle);
        assert!(watch.for_disk());
        // That rest started on the pass the handoff is read first, or
        // finished since (not joined yet): it still counts.
        let watch = CleanupWatch {
            job: Some(job(None, Some(DISK))),
            ending: true,
            ..CleanupWatch::default()
        };
        assert!(watch.for_disk());
    }

    /// Task 1427: a handoff withdrawn while draining takes requests again,
    /// lets an ordinary job that has not seen the stop go on, and asks for
    /// every ended run once, which picks up what the drain dropped. One
    /// that never ended asks for nothing more.
    #[test]
    fn a_withdrawn_end_takes_requests_again_and_asks_for_every_ended_run() {
        let mut watch = CleanupWatch {
            job: Some(job(None, None)),
            ..CleanupWatch::default()
        };
        assert!(!watch.resume());
        assert!(watch.pending.is_empty());
        watch.end();
        assert!(watch.stop.load(Ordering::SeqCst));
        assert!(watch.resume());
        assert!(!watch.ending);
        assert!(!watch.stop.load(Ordering::SeqCst));
        assert!(!watch.resume());
        assert!(watch.take(None, None));
        assert!(watch.pending.all && !watch.pending.idle && !watch.pending.prune);
        // Back to normal, a shortage asks for a cleanup for room, which
        // the job running takes on.
        assert!(watch.take(None, Some(DISK)));
        assert!(watch.for_disk());
        assert_eq!(watch.pending.counted, Some(DISK));
    }

    /// A request for some tasks' runs and one for every run merge; one for
    /// room alone adds no task, one for room and a task adds both.
    #[test]
    fn a_request_is_taken_into_what_waits_for_the_next_job() {
        let mut watch = CleanupWatch::default();
        assert!(watch.take(Some(TaskId::new(1)), None));
        assert!(watch.take(Some(TaskId::new(1)), None));
        assert_eq!(watch.pending.tasks, [TaskId::new(1)]);
        assert!(!watch.pending.all && !watch.for_disk());
        assert!(watch.take(Some(TaskId::new(2)), Some(DISK)));
        assert_eq!(watch.pending.tasks, [TaskId::new(1), TaskId::new(2)]);
        assert_eq!(watch.pending.disk, Some(DISK));
        assert!(watch.pending.all && watch.pending.prune && watch.pending.idle);
        let mut watch = CleanupWatch::default();
        assert!(watch.take(None, Some(DISK)));
        assert!(watch.pending.tasks.is_empty());
        assert!(watch.for_disk());
    }

    /// `worktree_removed`, `scratchpad_removed`, `run_tmp_removed` and
    /// `cleanup_failed` as the job's outcomes record them: the reason is
    /// the task's status, and each mark only when it applies.
    #[test]
    fn the_records_of_a_cleanup_keep_their_reasons_and_marks() {
        let plain = worktree_record(
            TaskStatus::Canceled,
            "/runs/r/worktree",
            "refs/heads/dagq/r",
            42,
            Removed::default(),
            &[],
            None,
        );
        assert_eq!(
            plain,
            json!({"path": "/runs/r/worktree", "branch": "refs/heads/dagq/r", "bytes": 42, "by": "supervisor", "reason": "task_canceled"})
        );
        let stopped = [json!({"pid": 7, "executable": "/runs/r/worktree/dagq", "killed": false})];
        let marked = worktree_record(
            TaskStatus::Completed,
            "/runs/r/worktree",
            "refs/heads/dagq/r",
            0,
            Removed {
                missing: true,
                repaired: true,
                broken_git: true,
            },
            &stopped,
            Some("ps failed"),
        );
        assert_eq!(marked["reason"], "task_completed");
        assert_eq!(marked["worktree_missing"], true);
        assert_eq!(marked["repaired"], true);
        assert_eq!(marked["broken_git"], true);
        assert_eq!(marked["stopped_processes"], json!(stopped));
        assert_eq!(marked["processes_unlisted"], "ps failed");
        for (removed, key) in [
            (
                Removed {
                    missing: true,
                    ..Removed::default()
                },
                "worktree_missing",
            ),
            (
                Removed {
                    repaired: true,
                    ..Removed::default()
                },
                "repaired",
            ),
            (
                Removed {
                    broken_git: true,
                    ..Removed::default()
                },
                "broken_git",
            ),
        ] {
            let payload = worktree_record(TaskStatus::Completed, "p", "b", 0, removed, &[], None);
            let marks: Vec<&str> = [
                "worktree_missing",
                "repaired",
                "broken_git",
                "stopped_processes",
                "processes_unlisted",
            ]
            .into_iter()
            .filter(|mark| payload.get(mark).is_some())
            .collect();
            assert_eq!(marks, [key]);
        }

        let tmp = task_over_record(TaskStatus::Canceled, &["/runs/r/tmp".to_owned()], 32_768);
        assert_eq!(
            tmp,
            json!({"paths": ["/runs/r/tmp"], "bytes": 32_768, "by": "supervisor", "reason": "task_canceled"})
        );
        assert_eq!(
            task_over_record(TaskStatus::Completed, &[], 0)["reason"],
            "task_completed"
        );

        let error = anyhow::anyhow!("denied").context("remove /claude/x");
        let (message, payload) = failed_record("scratchpad", "/claude/x", &error);
        assert_eq!(
            message,
            "scratchpad /claude/x could not be cleaned: remove /claude/x: denied"
        );
        assert_eq!(
            payload,
            json!({"path": "/claude/x", "message": message, "by": "supervisor", "code": "other"})
        );
        // A path that fails again is recorded once per process.
        let mut watch = CleanupWatch::default();
        assert!(watch.first_failure("/claude/x"));
        assert!(!watch.first_failure("/claude/x"));
        assert!(watch.first_failure("/runs/r/tmp"));
    }

    /// Task 1289: only an ended run loses its runner; a run waiting for an
    /// answer, or one nobody works on, keeps it for its session.
    #[test]
    fn only_an_ended_run_loses_its_runner() {
        assert!(runner_goes(WorktreeCleanup::Ended));
        assert!(!runner_goes(WorktreeCleanup::AwaitingAnswer(AskId::new(7))));
        assert!(!runner_goes(WorktreeCleanup::Idle));
    }

    /// Task 1100 and task 1290: the scratchpads and the temporary files
    /// directory of a run go once its task is over; a run whose task goes
    /// on keeps them, and a worktree outside the runs directory has no
    /// scratchpad looked for.
    #[test]
    fn the_scratchpads_and_the_tmp_dir_go_only_once_the_task_is_over() {
        let runs = Path::new("/runs");
        let roots = [PathBuf::from("/claude/a"), PathBuf::from("/claude/b")];
        let name = scratchpad_dir_name("/runs/r/worktree").unwrap();
        for status in [
            TaskStatus::Draft,
            TaskStatus::Submitted,
            TaskStatus::Ready,
            TaskStatus::InProgress,
        ] {
            let going_on = candidate("r", status);
            assert!(
                scratchpad_dirs(&going_on, runs, &roots).is_empty(),
                "{status:?}"
            );
            assert_eq!(run_tmp_dir(&going_on, runs), None, "{status:?}");
        }
        for status in [TaskStatus::Completed, TaskStatus::Canceled] {
            let over = candidate("r", status);
            assert_eq!(
                scratchpad_dirs(&over, runs, &roots),
                [roots[0].join(&name), roots[1].join(&name)]
            );
            assert_eq!(
                run_tmp_dir(&over, runs),
                Some(PathBuf::from("/runs/r").join(RUN_TMP_DIR))
            );
            let elsewhere = EndedRunWorktree {
                worktree: "/elsewhere/r/worktree".into(),
                ..over.clone()
            };
            assert!(scratchpad_dirs(&elsewhere, runs, &roots).is_empty());
            assert!(scratchpad_dirs(&over, runs, &[]).is_empty());
        }
    }

    #[test]
    fn build_output_records_keep_the_reason_ask_and_log_suffix() {
        let paths = vec![
            "/runs/r/worktree/target".into(),
            "/runs/r/worktree/llvm-cov-target".into(),
        ];
        for (cleanup, reason, why) in [
            (WorktreeCleanup::Ended, "run_ended", ""),
            (
                WorktreeCleanup::AwaitingAnswer(AskId::new(7)),
                "awaiting_answer",
                ", waiting for the answer to ask 7",
            ),
            (WorktreeCleanup::Idle, "disk_space", ", for disk space"),
        ] {
            let (suffix, payload) = build_outputs_record(cleanup, &paths, 42);
            assert_eq!(suffix, why);
            let mut expected =
                json!({"paths": paths, "bytes": 42, "by": "supervisor", "reason": reason});
            if cleanup == WorktreeCleanup::AwaitingAnswer(AskId::new(7)) {
                expected["ask_id"] = json!(7);
            }
            assert_eq!(payload, expected);
        }
    }

    #[test]
    fn only_completed_and_canceled_tasks_remove_tmp_and_worktree_branches() {
        for status in [
            TaskStatus::Draft,
            TaskStatus::Submitted,
            TaskStatus::Ready,
            TaskStatus::InProgress,
        ] {
            assert!(!task_over(status));
        }
        assert!(task_over(TaskStatus::Completed));
        assert!(task_over(TaskStatus::Canceled));
        let runs = Path::new("/runs");
        let repo = Path::new("/repo");
        assert!(safe_worktree(Path::new("/runs/r/worktree"), runs, repo));
        assert!(!safe_worktree(
            Path::new("/elsewhere/r/worktree"),
            runs,
            repo
        ));
        assert!(!safe_worktree(
            Path::new("/runs-other/r/worktree"),
            runs,
            repo
        ));
        assert!(!safe_worktree(
            Path::new("/runs/r/worktree"),
            runs,
            Path::new("/runs/r/worktree")
        ));
        assert!(!safe_worktree(
            Path::new("/runs/r/worktree"),
            runs,
            Path::new("/runs/r/worktree/repo")
        ));
        assert!(!safe_worktree(runs, runs, Path::new("/runs/repo")));
    }

    #[test]
    fn cleaned_counts_positive_bytes_and_each_run_once_regardless_of_reason() {
        let run = RunId::new("r").unwrap();
        let other = RunId::new("other").unwrap();
        let mut cleaned = Cleaned::default();
        cleaned.add(&run, 0);
        assert!(cleaned.runs.is_empty());
        for cleanup in [
            WorktreeCleanup::Ended,
            WorktreeCleanup::AwaitingAnswer(AskId::new(7)),
            WorktreeCleanup::Idle,
        ] {
            let (_, payload) = build_outputs_record(cleanup, &[], 42);
            cleaned.add(&run, payload["bytes"].as_u64().unwrap());
        }
        cleaned.add(&other, 1);
        assert_eq!(cleaned.bytes, 127);
        assert_eq!(cleaned.runs, [run, other]);
    }

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

    /// Task 1590: only the run's own worktree of a task that is over has
    /// what runs from under it stopped; not a task that goes on, nor a
    /// worktree outside the runs directory, another run's, or one holding
    /// the repository's checkout.
    #[test]
    fn only_the_own_worktree_of_an_ended_task_has_its_processes_stopped() {
        let runs = Path::new("/runs");
        let repo = Path::new("/repo");
        for status in [TaskStatus::Completed, TaskStatus::Canceled] {
            assert!(own_ended_worktree(&candidate("r", status), runs, repo));
        }
        for status in [
            TaskStatus::InProgress,
            TaskStatus::Ready,
            TaskStatus::Submitted,
            TaskStatus::Draft,
        ] {
            assert!(
                !own_ended_worktree(&candidate("r", status), runs, repo),
                "{status:?}"
            );
        }
        let over = candidate("r", TaskStatus::Completed);
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
            assert!(!own_ended_worktree(&moved, runs, repo), "{worktree}");
        }
        assert!(!own_ended_worktree(
            &over,
            runs,
            Path::new("/runs/r/worktree/repo")
        ));
    }

    /// Processes listed with their executables, of which `stubborn` live
    /// on through a SIGTERM; what was signalled is recorded.
    #[derive(Default)]
    struct Signalled {
        listed: Vec<crate::domain::disk::ProcessExecutable>,
        stubborn: Vec<u32>,
        unlisted: bool,
        alive: Mutex<Vec<u32>>,
        calls: Mutex<Vec<String>>,
    }

    impl ProcessControl for Signalled {
        fn alive(&self, pid: u32) -> bool {
            self.alive.lock().unwrap().contains(&pid)
        }
        fn terminate(&self, pid: u32) -> Result<()> {
            self.calls.lock().unwrap().push(format!("term {pid}"));
            if !self.stubborn.contains(&pid) {
                self.alive.lock().unwrap().retain(|alive| *alive != pid);
            }
            Ok(())
        }
        fn interrupt(&self, pid: u32) -> Result<()> {
            self.calls.lock().unwrap().push(format!("int {pid}"));
            Ok(())
        }
        fn kill(&self, pid: u32) -> Result<()> {
            self.calls.lock().unwrap().push(format!("kill {pid}"));
            self.alive.lock().unwrap().retain(|alive| *alive != pid);
            Ok(())
        }
        fn executables(&self) -> Result<Vec<crate::domain::disk::ProcessExecutable>> {
            self.calls.lock().unwrap().push("list".to_owned());
            anyhow::ensure!(!self.unlisted, "ps failed");
            Ok(self.listed.clone())
        }
    }

    /// Task 1590: what runs from under the worktree gets a SIGTERM, and a
    /// SIGKILL once the grace is over if it still runs; what runs from
    /// elsewhere is not signalled, and each stopped one is recorded with
    /// its executable. A listing that fails stops nothing.
    #[test]
    fn processes_running_from_the_worktree_are_terminated_then_killed() {
        let process = |pid, executable: &str| crate::domain::disk::ProcessExecutable {
            pid,
            ppid: 1,
            executable: Some(executable.to_owned()),
        };
        let control = Signalled {
            listed: vec![
                process(60, "/runs/r/worktree/target/llvm-cov-target/debug/dagq"),
                process(61, "/runs/r/worktree/target/debug/dagq"),
                // Started in the worktree, from elsewhere.
                process(72, "/bin/sleep"),
            ],
            stubborn: vec![61],
            alive: Mutex::new(vec![60, 61, 72]),
            ..Signalled::default()
        };
        let stopped = stop_worktree_processes(&control, Path::new("/runs/r/worktree"), 0).unwrap();
        assert_eq!(
            stopped,
            [
                json!({"pid": 60, "executable": "/runs/r/worktree/target/llvm-cov-target/debug/dagq", "killed": false}),
                json!({"pid": 61, "executable": "/runs/r/worktree/target/debug/dagq", "killed": true}),
            ]
        );
        assert_eq!(
            *control.calls.lock().unwrap(),
            ["list", "term 60", "term 61", "kill 61"]
        );
        assert_eq!(*control.alive.lock().unwrap(), [72]);

        // Nothing from under the worktree: nothing signalled.
        let idle = Signalled {
            listed: vec![process(72, "/bin/sleep")],
            alive: Mutex::new(vec![72]),
            ..Signalled::default()
        };
        assert!(
            stop_worktree_processes(&idle, Path::new("/runs/r/worktree"), 0)
                .unwrap()
                .is_empty()
        );
        assert_eq!(*idle.calls.lock().unwrap(), ["list"]);

        let unlisted = Signalled {
            unlisted: true,
            ..Signalled::default()
        };
        assert!(stop_worktree_processes(&unlisted, Path::new("/runs/r/worktree"), 0).is_err());
    }
}
