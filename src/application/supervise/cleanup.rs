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
//! another supervisor) is left alone. A stop or a handoff lets the job end after its current
//! worktree; the next sweep picks up the rest.

use super::*;
use crate::application::EndedRunWorktree;
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
            // The first reading is the one the shortage was found at.
            self.disk = self.disk.or(disk);
        }
    }
    const fn is_empty(&self) -> bool {
        !self.all && self.tasks.is_empty()
    }
    fn wants(&self, candidate: &EndedRunWorktree) -> bool {
        self.all || self.tasks.contains(&candidate.task_id)
    }
}

/// The cleanup job running now, if any, and what waits for the next one.
#[derive(Default)]
pub(super) struct CleanupWatch {
    job: Option<Job>,
    pending: Request,
    /// The runs the job picked and has not passed yet. The loop holds the
    /// lock while it leases an ended run and skips the runs listed.
    cleaning: Arc<Mutex<Vec<RunId>>>,
    /// Set on a stop or a handoff: the job ends after its current worktree.
    stop: Arc<AtomicBool>,
    /// The loop left a run reserved by the job for a later pass: it waits
    /// for the job, as for a run.
    pub(super) deferred: bool,
}

struct Job {
    handle: thread::JoinHandle<Vec<Outcome>>,
    disk: Option<DiskRequest>,
}

impl CleanupWatch {
    /// A job runs.
    pub(super) const fn running(&self) -> bool {
        self.job.is_some()
    }
    /// A cleanup for disk space runs or waits to.
    pub(super) fn for_disk(&self) -> bool {
        self.job.as_ref().is_some_and(|job| job.disk.is_some()) || self.pending.disk.is_some()
    }
    /// The lock the loop holds while it leases an ended run, and the runs
    /// the job has yet to pass, which the loop leaves for a later pass.
    pub(super) fn cleaning(&self) -> Arc<Mutex<Vec<RunId>>> {
        self.cleaning.clone()
    }
}

/// Lock `cleaning`, whatever a panicking holder left.
pub(super) fn lock_cleaning(cleaning: &Mutex<Vec<RunId>>) -> MutexGuard<'_, Vec<RunId>> {
    cleaning.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the job did to one worktree.
enum Outcome {
    /// The build outputs of a run whose task goes on.
    BuildOutputs {
        run_id: RunId,
        status: RunStatus,
        paths: Vec<String>,
        bytes: u64,
    },
    /// The worktree and branch of a run whose task is over; `missing` when
    /// the directory was already gone and only the branch went, `repaired`
    /// when `git worktree repair` had to point the worktree at the
    /// repository again first.
    Worktree {
        run_id: RunId,
        task_id: TaskId,
        task_status: TaskStatus,
        path: String,
        branch: String,
        bytes: u64,
        missing: bool,
        repaired: bool,
    },
    Failed {
        run_id: RunId,
        path: String,
        error: anyhow::Error,
    },
}

/// What the job works with, all of it shared with the loop.
struct JobPorts {
    files: Arc<dyn RunFiles>,
    repository: Arc<dyn Repository + Send + Sync>,
    queues: Arc<dyn QueueOpener>,
    runs_dir: PathBuf,
    repo_root: PathBuf,
    cleaning: Arc<Mutex<Vec<RunId>>>,
    stop: Arc<AtomicBool>,
    prune: bool,
}

impl Supervisor<'_> {
    /// Ask for the worktrees of ended runs to be cleaned, every such run's
    /// or only `task`'s (see [`Self::clean_ended_worktrees`] for what is
    /// removed), for disk space when `disk` is given. It starts at once
    /// unless a job runs; then it waits for the next one.
    pub(super) fn request_cleanup(&mut self, task: Option<TaskId>, disk: Option<DiskRequest>) {
        if self.cleanup.stop.load(Ordering::SeqCst) {
            return;
        }
        // For room while another job runs: that job counts for it, so the
        // claims wait only for it, and the rest (the runs it did not pick,
        // and the prune) follows without holding them.
        if let (Some(request), Some(job)) = (disk, self.cleanup.job.as_mut())
            && job.disk.is_none()
        {
            job.disk = Some(request);
            self.cleanup.pending.add(None, None);
            self.cleanup.pending.prune = true;
            return;
        }
        self.cleanup.pending.add(task, disk);
        self.start_cleanup();
    }
    /// Join a finished job and record what it did, then start what waits;
    /// with `ending` (a stop or a handoff), let the job end after its
    /// current worktree and start nothing more.
    pub(super) fn poll_cleanup(&mut self, ending: bool) {
        if ending {
            self.cleanup.stop.store(true, Ordering::SeqCst);
            self.cleanup.pending = Request::default();
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
        lock_cleaning(&self.cleanup.cleaning).clear();
        self.cleanup.deferred = false;
        let cleaned = self.record_cleanup(outcomes);
        if let Some(disk) = job.disk {
            self.cleaned_for_disk(disk, &cleaned);
        }
        self.start_cleanup();
    }
    /// Once the loop ended: wait for the job and whatever waits for the
    /// next one (after a stop, only for the job's current worktree), and
    /// record what they did.
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
        let candidates: Vec<EndedRunWorktree> = match self.queue.ended_run_worktrees() {
            Ok(all) => all
                .into_iter()
                .filter(|w| request.wants(w))
                .filter(|w| !self.slots.iter().any(|slot| *slot.run.id() == w.run_id))
                .collect(),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the worktrees of ended runs could not be listed for their cleanup: {error:#}");
                return;
            }
        };
        if candidates.is_empty() && !request.prune {
            return;
        }
        *lock_cleaning(&self.cleanup.cleaning) =
            candidates.iter().map(|w| w.run_id.clone()).collect();
        let ports = JobPorts {
            files: self.files.clone(),
            repository: self.repository.clone(),
            queues: self.queues.clone(),
            runs_dir: self.layout.runs_dir.clone(),
            repo_root: self.layout.repo_root.clone(),
            cleaning: self.cleanup.cleaning.clone(),
            stop: self.cleanup.stop.clone(),
            prune: request.prune,
        };
        let handle = spawn_traced(move || run_job(&ports, candidates));
        self.cleanup.job = Some(Job {
            handle,
            disk: request.disk,
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
                    paths,
                    bytes,
                } => {
                    info!(run_id = %run_id, "run {run_id} is {}; removed the build outputs of its worktree ({bytes} bytes)", status.as_str());
                    cleaned.add(&run_id, bytes);
                    self.queue.record_runtime_event(
                        &run_id,
                        "build_outputs_removed",
                        json!({"paths": paths, "bytes": bytes, "by": "supervisor"}),
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
                    cleaned.add(&run_id, bytes);
                    self.queue
                        .record_runtime_event(&run_id, "worktree_removed", payload)
                }
                Outcome::Failed {
                    run_id,
                    path,
                    error,
                } => {
                    if self.sweep_failures.contains(&path) {
                        warn!(run_id = %run_id, "run {run_id}: worktree {path} still could not be cleaned: {error:#}");
                        continue;
                    }
                    self.sweep_failures.push(path.clone());
                    let message = format!("worktree {path} could not be cleaned: {error:#}");
                    warn!(run_id = %run_id, "run {run_id}: {message}");
                    self.queue.record_runtime_event(
                        &run_id,
                        "cleanup_failed",
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
    let mut outcomes = Vec::new();
    let queue = match ports.queues.open() {
        Ok(queue) => queue,
        Err(error) => {
            warn!(error = %format_args!("{error:#}"), "the cleanup of ended runs' worktrees could not open the queue: {error:#}");
            return outcomes;
        }
    };
    let mut branches: Option<Vec<String>> = None;
    let mut pruned = false;
    for candidate in candidates {
        if ports.stop.load(Ordering::SeqCst) {
            break;
        }
        let still = {
            let _cleaning = lock_cleaning(&ports.cleaning);
            match queue.ended_run_worktrees() {
                // Still to clean as it was picked: nobody leased it since,
                // and neither it nor its task moved on.
                Ok(now) => now.contains(&candidate),
                Err(error) => {
                    warn!(run_id = %candidate.run_id, error = %format_args!("{error:#}"), "run {}: its worktree was not cleaned, the queue could not be read: {error:#}", candidate.run_id);
                    false
                }
            }
        };
        let result = if still {
            clean_worktree(ports, &candidate, &mut branches, &mut pruned)
        } else {
            Ok(None)
        };
        lock_cleaning(&ports.cleaning).retain(|run| *run != candidate.run_id);
        match result {
            Ok(Some(outcome)) => outcomes.push(outcome),
            Ok(None) => {}
            Err(error) => outcomes.push(Outcome::Failed {
                run_id: candidate.run_id.clone(),
                path: candidate.worktree.clone(),
                error,
            }),
        }
    }
    lock_cleaning(&ports.cleaning).clear();
    if ports.prune
        && !pruned
        && let Err(error) = ports.repository.prune_worktrees()
    {
        warn!(error = %format_args!("{error:#}"), "git worktree prune failed: {error:#}");
    }
    outcomes
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
    let removed = |bytes, missing, repaired, branch: &str| Outcome::Worktree {
        run_id: candidate.run_id.clone(),
        task_id: candidate.task_id,
        task_status: candidate.task_status,
        path: candidate.worktree.clone(),
        branch: branch.to_owned(),
        bytes,
        missing,
        repaired,
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
        return Ok(Some(removed(0, true, false, branch)));
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
        let repaired = remove_worktree(&*ports.repository, worktree, branch)?;
        return Ok(Some(removed(bytes, false, repaired, branch)));
    }
    if !matches!(
        candidate.status,
        RunStatus::Integrated | RunStatus::Succeeded | RunStatus::Failed | RunStatus::Interrupted
    ) {
        return Ok(None);
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
        paths,
        bytes,
    }))
}

/// Remove the worktree and its branch. When Git refuses (the worktree's
/// `.git` points at a common directory the queue's rebind left behind),
/// `git worktree repair` points it at the repository again and the removal
/// is tried once more; whether it had to.
fn remove_worktree(repository: &dyn Repository, worktree: &Path, branch: &str) -> Result<bool> {
    let Err(error) = repository.remove_worktree_and_branch(worktree, branch) else {
        return Ok(false);
    };
    repository
        .repair_worktree(worktree)
        .with_context(|| format!("{error:#}; and repairing the worktree failed"))?;
    repository
        .remove_worktree_and_branch(worktree, branch)
        .with_context(|| format!("{error:#}; repaired, and removing it again failed"))?;
    Ok(true)
}
