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
//! ([`RunReads::ended_run_worktree`], task 1586), not every ended run again.
//!
//! A run the job found nothing left of ([`nothing_left`]) is settled
//! ([`Cleaning::settled`]): the next sweeps leave it out of their
//! candidates, without Git or the filesystem, until the queue lists it
//! otherwise, a slot holds it or the loop leases it (task 1586).
//!
//! The job also removes what an ended run whose task is over left outside
//! its worktree: the Claude Code scratchpad of its session (task 1100,
//! [`scratchpad_dir_name`]), and the temporary files directories the
//! runtime gave its worker's turns ([`RUN_TMP_DIR`] in its run directory,
//! task 1290) and its verification commands on the host (outside the
//! queue's `runs/`, [`host_verification_tmp_path`]) as their `TMPDIR`. The
//! rest of the run directory stays. Of every
//! ended run it removes the scratch worktree an `already_satisfied`
//! verification left in the queue's `satisfied/` (its supervisor died
//! while it ran, [`satisfied_scratch_of`]).
//!
//! Beside the ended runs, it removes the build outputs of the runs left in
//! the middle with no lease and no live session (task 1289,
//! [`WorktreeCleanup`]): of one waiting for a person's answer on every
//! cleanup, of any other only in a cleanup for disk space. The loop leaves
//! such a run reserved by the job before it lands, reviews or resumes it.
//! A cleanup for disk space also takes those of the runs the supervisor
//! itself leases that wait outside its slots for an answer (ADR-0071),
//! once their turn is over and nothing runs for them
//! ([`WorktreeCleanup::Waiting`]): the slots offer them ([`Held`]), and
//! their waits hold still while the job has them reserved, so no answer is
//! delivered and no turn starts in a worktree being cleaned.
//! It takes too those of the runs the supervisor leases that wait in its
//! slots for the landing slot, with nothing running for them
//! ([`WorktreeCleanup::Landing`]): the slots offer them back to front of
//! the landing queue ([`Held::landing`]), and the job passes them last.
//! Such a run is reserved only once the job reaches it, and only while
//! the loop has not moved it on since ([`Cleaning::withdraw`]): one whose
//! landing began meanwhile keeps its build outputs, and one reserved does
//! not begin to land until the job passed it; its landing builds again.
//!
//! Whatever the free space, the sweep of every run takes those same runs
//! (nobody's that waits for no answer, and the waiting and landing ones the
//! slots offer) for their stale caches alone ([`remove_stale_entries`]):
//! what the declared profiles name that no build reads any more, rustc's
//! older incremental sessions. They are reserved, checked again and held
//! still the same way, so nothing builds in them meanwhile. A run's next
//! build leaves such caches again, so a run whose stale caches went is
//! left out of the sweeps only until it may have built again
//! ([`leave_pruned`]).
//!
//! A cleanup for disk space, and the rest of one, also clears the build
//! caches of the queue's directory ([`build_caches`]): the target
//! directory of the automatic update and the build output of each
//! declaration in the landing recheck's directory, which are built again (sccache
//! shares the compiles). Each only while its lock
//! is free ([`clear_build_cache`]): the job holds it while it clears, so
//! what builds there waits, and leaves a cache in use for the next cleanup.

use super::*;
use crate::application::update::{self, UpdatePaths};
use crate::application::worktree_processes::{WORKTREE_STOP_POLLS, stop_worktree_processes};
use crate::application::{
    EndedRunWorktree, HostOpsQueue, RUN_TMP_DIR, WorktreeAdmin, WorktreeCleanup,
    host_verification_tmp_path,
};
use crate::domain::EventKind;
use crate::domain::disk::{ProcessExecutable, worktree_executables};
use crate::domain::recovery::{ProcessInfo, run_processes};
use crate::domain::toolchain::{BUILTIN, Toolchains};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::disk::Cleaned;

/// A cleanup for disk space (task 377): the free bytes before it and what
/// was needed, for `auto_repaired`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::application::supervise) struct DiskRequest {
    pub(super) free: Option<u64>,
    pub(super) needed: Option<u64>,
}

/// What the slots hold, which a cleanup leaves alone (`runs`), but for
/// the runs of `waiting` and `landing` a cleanup for disk space may take.
#[derive(Debug, Default)]
pub(in crate::application::supervise) struct Held {
    pub(in crate::application::supervise) runs: Vec<RunId>,
    pub(in crate::application::supervise) waiting: Vec<WaitingRun>,
    /// The runs waiting for the landing slot, the last to land first.
    pub(in crate::application::supervise) landing: Vec<LandingRun>,
}

/// A run the supervisor leases that waits in its slot for the landing
/// slot, with no landing, e2e or recheck running for it, as its slot
/// holds it: what a cleanup for disk space needs to take its build
/// outputs ([`landing_candidate`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::application::supervise) struct LandingRun {
    pub(in crate::application::supervise) run_id: RunId,
    pub(in crate::application::supervise) task_id: TaskId,
    pub(in crate::application::supervise) worktree: String,
    pub(in crate::application::supervise) branch: Option<String>,
}

/// A run the supervisor leases that waits outside its slots for a
/// person's answer (ADR-0071), as its slot holds it: what a cleanup for
/// disk space needs to take its build outputs ([`waiting_candidate`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::application::supervise) struct WaitingRun {
    pub(in crate::application::supervise) run_id: RunId,
    pub(in crate::application::supervise) task_id: TaskId,
    pub(in crate::application::supervise) status: RunStatus,
    pub(in crate::application::supervise) worktree: String,
    pub(in crate::application::supervise) branch: Option<String>,
    /// The asks the wait holds, oldest first.
    pub(in crate::application::supervise) asks: Vec<AskId>,
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
    /// answer ([`WorktreeCleanup::Idle`]), and of the runs the slots offer
    /// that wait for an answer with no turn running
    /// ([`WorktreeCleanup::Waiting`]) or for the landing slot
    /// ([`WorktreeCleanup::Landing`]), go too (for disk space). Without it,
    /// a request for every run (`all`) takes those runs for their stale
    /// caches alone ([`remove_stale_entries`]).
    idle: bool,
    /// The [`build_caches`] are cleared too (for disk space).
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
    /// Whether the job takes `candidate`: a request for every run takes
    /// every one, a run whose build outputs only a cleanup for room
    /// removes for its stale caches alone unless for room
    /// ([`Self::takes_held`]); one for some tasks takes theirs, but those.
    fn wants(&self, candidate: &EndedRunWorktree) -> bool {
        self.takes_held()
            || (self.tasks.contains(&candidate.task_id) && !for_room_only(candidate.cleanup))
    }
    /// Whether the job takes the runs a slot offers ([`Held`]): those of a
    /// cleanup for room and of a request for every run, the sweep's, the
    /// latter for their stale caches alone.
    const fn takes_held(&self) -> bool {
        self.idle || self.all
    }
}

/// The cleanup job running now, if any, and what waits for the next one.
#[derive(Default)]
pub(in crate::application::supervise) struct CleanupWatch {
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
    pub(in crate::application::supervise) const fn running(&self) -> bool {
        self.job.is_some()
    }
    /// A cleanup for disk space runs or waits to, or the rest of one
    /// another job took on: the disk is judged on the reading after it, so
    /// nothing is held or asked for while it may still free room (task
    /// 1478), and a drain decides its landings on that reading (task 1426).
    pub(in crate::application::supervise) fn for_disk(&self) -> bool {
        disk_cleanup_pending(
            self.job.as_ref().map(|job| (job.disk, job.counted)),
            &self.pending,
        )
    }
    /// The lock the loop holds while it leases an ended run, and the runs
    /// the job has yet to pass, which the loop leaves for a later pass.
    pub(in crate::application::supervise) fn cleaning(&self) -> Arc<Mutex<Cleaning>> {
        self.cleaning.clone()
    }
    /// The loop left a run the job reserved for a later pass: it waits for
    /// the job, as for a run, until the job is joined.
    pub(in crate::application::supervise) const fn defer(&mut self) {
        self.deferred = true;
    }
    /// A run was left for the job ([`Self::defer`]) and the job is not
    /// joined yet.
    pub(in crate::application::supervise) const fn deferred(&self) -> bool {
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
    /// requests, let an ordinary job stop after its current worktree, keep
    /// only the [`ending_rest`] of what waits, and withdraw the runs
    /// waiting for the landing slot the job has not reached, which the
    /// drain lands or hands over as before ([`Cleaning::withdraw`]).
    fn end(&mut self) {
        self.ending = true;
        lock_cleaning(&self.cleaning).withdraw_all();
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
    /// Whether a job for `request` takes the runs waiting for the landing
    /// slot: a cleanup for disk space and the sweep of every run (for the
    /// stale caches alone, [`Request::takes_held`]), and not once the
    /// cleanup ended ([`Self::end`]), whose drain lands or hands them over
    /// as before; the rest of a cleanup for room started then passes them
    /// by.
    const fn takes_landing(&self, request: &Request) -> bool {
        request.takes_held() && !self.ending
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
pub(in crate::application::supervise) const fn ends_cleanup(
    stopping: bool,
    handing_off: bool,
) -> bool {
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

/// Whether only a cleanup for disk space removes what a candidate left:
/// the build outputs of a run nobody works on that waits for no answer, or
/// of one this supervisor leases that waits outside its slots or for the
/// landing slot.
const fn for_room_only(cleanup: WorktreeCleanup) -> bool {
    matches!(
        cleanup,
        WorktreeCleanup::Idle | WorktreeCleanup::Waiting(_) | WorktreeCleanup::Landing
    )
}

fn task_over(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Completed | TaskStatus::Canceled)
}

fn safe_worktree(worktree: &Path, runs_dir: &Path, repo_root: &Path) -> bool {
    worktree.starts_with(runs_dir) && !repo_root.starts_with(worktree)
}

/// The build-output event and the suffix of its log message. Ordinary cleanup
/// records the event too; only its job's disk/counting request makes a repair.
fn build_outputs_record(
    cleanup: WorktreeCleanup,
    paths: &[String],
    size: &Size,
) -> (String, Value) {
    let (reason, why) = match cleanup {
        WorktreeCleanup::Ended => (BUILD_OUTPUTS_RUN_ENDED, String::new()),
        WorktreeCleanup::AwaitingAnswer(ask) => (
            BUILD_OUTPUTS_AWAITING_ANSWER,
            format!(", waiting for the answer to ask {ask}"),
        ),
        WorktreeCleanup::Idle => (BUILD_OUTPUTS_DISK_SPACE, ", for disk space".to_owned()),
        WorktreeCleanup::Waiting(ask) => (
            BUILD_OUTPUTS_DISK_SPACE,
            format!(", waiting outside its slot for the answer to ask {ask}, for disk space"),
        ),
        WorktreeCleanup::Landing => (
            BUILD_OUTPUTS_DISK_SPACE,
            ", waiting for the landing slot, for disk space".to_owned(),
        ),
    };
    let mut payload =
        json!({"paths": paths, "bytes": size.bytes, "by": "supervisor", "reason": reason});
    if let WorktreeCleanup::AwaitingAnswer(ask) | WorktreeCleanup::Waiting(ask) = cleanup {
        payload["ask_id"] = json!(ask);
    }
    size.mark(&mut payload);
    (why, payload)
}

/// The suffix of the log message and the payload of
/// `incremental_sessions_removed` ([`Outcome::StaleEntries`]): `reason`
/// says what the run waits for, as no build runs in it meanwhile (`idle`,
/// `awaiting_answer` with its `ask_id`, `awaiting_landing_slot`). Unlike
/// `build_outputs_removed`, the run's size is not read from it
/// ([`crate::domain::disk::run_size`]): the rest of its build outputs
/// stay, and its next build writes these again.
fn stale_entries_record(
    cleanup: WorktreeCleanup,
    paths: &[String],
    entries: usize,
    size: &Size,
) -> (String, Value) {
    let (reason, why) = match cleanup {
        WorktreeCleanup::Ended => (BUILD_OUTPUTS_RUN_ENDED, String::new()),
        WorktreeCleanup::Idle => (STALE_ENTRIES_IDLE, String::new()),
        WorktreeCleanup::AwaitingAnswer(ask) | WorktreeCleanup::Waiting(ask) => (
            BUILD_OUTPUTS_AWAITING_ANSWER,
            format!(", waiting for the answer to ask {ask}"),
        ),
        WorktreeCleanup::Landing => (
            STALE_ENTRIES_LANDING_SLOT,
            ", waiting for the landing slot".to_owned(),
        ),
    };
    let mut payload = json!({"paths": paths, "entries": entries, "bytes": size.bytes, "by": "supervisor", "reason": reason});
    if let WorktreeCleanup::AwaitingAnswer(ask) | WorktreeCleanup::Waiting(ask) = cleanup {
        payload["ask_id"] = json!(ask);
    }
    size.mark(&mut payload);
    (why, payload)
}

/// How a worktree went ([`Outcome::Worktree`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Removed {
    missing: bool,
    repaired: bool,
    broken_git: bool,
    no_admin: bool,
}

/// The payload of `worktree_removed`: the reason is the task's status, and
/// each of `removed`, the processes stopped and why they could not be
/// listed is named only when there is one.
fn worktree_record(
    task_status: TaskStatus,
    path: &str,
    branch: &str,
    size: &Size,
    removed: Removed,
    stopped: &[Value],
    unlisted: Option<&str>,
) -> Value {
    let reason = format!("task_{}", task_status.as_str());
    let mut payload = json!({"path": path, "branch": branch, "bytes": size.bytes, "by": "supervisor", "reason": reason});
    size.mark(&mut payload);
    if removed.missing {
        payload["worktree_missing"] = json!(true);
    }
    if removed.repaired {
        payload["repaired"] = json!(true);
    }
    if removed.broken_git {
        payload["broken_git"] = json!(true);
    }
    if removed.no_admin {
        payload["no_admin"] = json!(true);
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
fn task_over_record(task_status: TaskStatus, paths: &[String], size: &Size) -> Value {
    let mut payload = json!({"paths": paths, "bytes": size.bytes, "by": "supervisor", "reason": format!("task_{}", task_status.as_str())});
    size.mark(&mut payload);
    payload
}

/// The message and the payload of `cleanup_failed` for `what` (`worktree`,
/// `scratchpad`, `run tmp`, `satisfied scratch` or `stale cache`) at
/// `path`.
fn failed_record(what: &str, path: &str, error: &anyhow::Error) -> (String, Value) {
    let message = format!("{what} {path} could not be cleaned: {error:#}");
    let payload = reason_of_error(error, ReasonCode::Other)
        .on(json!({"path": path, "message": message, "by": "supervisor"}));
    (message, payload)
}

/// What the loop and the job share under one lock.
#[derive(Default)]
pub(in crate::application::supervise) struct Cleaning {
    /// The runs the job picked and has not passed yet.
    reserved: Vec<RunId>,
    /// The runs waiting for the landing slot the job picked and has not
    /// reached yet ([`WorktreeCleanup::Landing`]): reserved only once it
    /// reaches one the loop has not withdrawn ([`Self::withdraw`]).
    offered: Vec<RunId>,
    /// The runs of `offered` the job reached and reserved, until it
    /// passed them.
    reached: Vec<RunId>,
    /// The runs a job found nothing left to clean of ([`nothing_left`]),
    /// as the queue listed them then (task 1586). A sweep leaves them out
    /// of its candidates while the queue lists them the same, no slot
    /// holds them and the loop has not leased them since.
    settled: HashMap<RunId, EndedRunWorktree>,
    /// The runs a job removed the stale caches of
    /// ([`remove_stale_entries`]), as they were then: a sweep leaves them
    /// out while they are offered or listed the same and the loop has not
    /// leased them since ([`leave_pruned`]), as nothing built in them.
    pruned: HashMap<RunId, PrunedAs>,
}

/// A run as a job found it when it removed its stale caches
/// ([`Cleaning::pruned`]): the candidate, and the latest of its turn
/// events then, for one the supervisor leases.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PrunedAs {
    worktree: EndedRunWorktree,
    turn: Option<EventId>,
}

impl Cleaning {
    /// Whether the job has `run` reserved: a waiting run's wait holds
    /// still meanwhile ([`Supervisor::watch_waiting`]), so no answer goes
    /// to its session and no turn starts while its build outputs go.
    pub(in crate::application::supervise) fn reserved(&self, run: &RunId) -> bool {
        self.reserved.contains(run)
    }
    /// The loop is about to lease `run`: `false` while the job has it
    /// reserved, which the loop leaves for a later pass. Otherwise the run
    /// is no longer settled, as it may get a worktree again, nor pruned, as
    /// it may build again.
    pub(in crate::application::supervise) fn may_lease(&mut self, run: &RunId) -> bool {
        if self.reserved.contains(run) {
            return false;
        }
        self.settled.remove(run);
        self.pruned.remove(run);
        true
    }
    /// The loop moves `run`, which waited for the landing slot, on (its
    /// landing, its e2e, a park or the lease handed back): the job no
    /// longer takes it. The loop holds the lock from its look at the run
    /// until it moved it, so the job reserves no run being moved on. It
    /// is no longer pruned either, as what it moved on to may build.
    pub(in crate::application::supervise) fn withdraw(&mut self, run: &RunId) {
        self.offered.retain(|offered| offered != run);
        self.pruned.remove(run);
    }
    /// Whether the job may go on to `candidate` now, under the lock: one
    /// waiting for the landing slot only while it is still offered, which
    /// reserves it; any other was reserved when the job picked it.
    fn reach(&mut self, candidate: &EndedRunWorktree) -> bool {
        if candidate.cleanup != WorktreeCleanup::Landing {
            return true;
        }
        let offered = self.offered.contains(&candidate.run_id);
        self.withdraw(&candidate.run_id);
        if offered {
            self.reserved.push(candidate.run_id.clone());
            self.reached.push(candidate.run_id.clone());
        }
        offered
    }
    /// The job removes the build outputs of a run waiting for the landing
    /// slot now: a landing recheck does not start meanwhile.
    pub(in crate::application::supervise) fn landing_reached(&self) -> bool {
        !self.reached.is_empty()
    }
    /// A landing recheck started: the job takes none of the runs waiting
    /// for the landing slot it has not reached ([`Self::withdraw`]).
    pub(in crate::application::supervise) fn withdraw_all(&mut self) {
        self.offered.clear();
    }
    /// The job ended, having passed its candidates or not: none of them
    /// is reserved, offered or reached any longer.
    fn release(&mut self) {
        self.reserved.clear();
        self.offered.clear();
        self.reached.clear();
    }
}

/// Lock `cleaning`, whatever a panicking holder left.
pub(in crate::application::supervise) fn lock_cleaning(
    cleaning: &Mutex<Cleaning>,
) -> MutexGuard<'_, Cleaning> {
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
    /// its runner, a scratchpad, its temporary files directory or a stale
    /// cache.
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

/// Leave out of an ordinary job's `candidates` the runs whose stale
/// caches a job removed while they are as they were then
/// ([`Cleaning::pruned`]; `turn` gives a candidate's latest turn event),
/// and forget a pruned run no longer among them as it was: it was leased,
/// a turn of it ran, or it moved on, so it may have built again and is a
/// candidate on the next sweep it is back. Only the runs whose build
/// outputs only a cleanup for room removes are pruned ones.
fn leave_pruned(
    pruned: &mut HashMap<RunId, PrunedAs>,
    candidates: [&mut Vec<EndedRunWorktree>; 2],
    turn: impl Fn(&RunId) -> Option<EventId>,
) {
    let now = |candidate: &EndedRunWorktree| PrunedAs {
        worktree: candidate.clone(),
        turn: turn(&candidate.run_id),
    };
    let listed: HashMap<RunId, PrunedAs> = candidates
        .iter()
        .flat_map(|list| list.iter())
        .filter(|candidate| for_room_only(candidate.cleanup))
        .map(|candidate| (candidate.run_id.clone(), now(candidate)))
        .collect();
    pruned.retain(|run, then| listed.get(run) == Some(then));
    for list in candidates {
        list.retain(|candidate| {
            !for_room_only(candidate.cleanup) || !pruned.contains_key(&candidate.run_id)
        });
    }
}

/// The candidate a waiting run is to a cleanup for disk space
/// ([`WorktreeCleanup::Waiting`]): only once its last turn finished after
/// the last one requested ([`turn_idle`]), for the oldest ask its wait
/// holds that is still `open`. Its task goes on, as the run is leased.
fn waiting_candidate(
    run: &WaitingRun,
    events: &[RunEvent],
    open: impl Fn(AskId) -> bool,
) -> Option<EndedRunWorktree> {
    if !turn_idle(events) {
        return None;
    }
    let ask = run.asks.iter().copied().find(|ask| open(*ask))?;
    Some(EndedRunWorktree {
        run_id: run.run_id.clone(),
        task_id: run.task_id,
        status: run.status,
        task_status: TaskStatus::InProgress,
        worktree: run.worktree.clone(),
        branch: run.branch.clone(),
        cleanup: WorktreeCleanup::Waiting(ask),
    })
}

/// Whether no turn of the run runs nor is about to: of its turn events
/// the latest is `turn_finished`. One requested and not started, or
/// started and not finished (its wrapper may have died in it), is not.
fn turn_idle(events: &[RunEvent]) -> bool {
    latest_turn_event(events).is_some_and(|event| event.kind == EventKind::TurnFinished)
}

/// The latest of a run's turn events, by id.
fn latest_turn_event(events: &[RunEvent]) -> Option<&RunEvent> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.kind.known(),
                Some(EventKind::TurnRequested | EventKind::TurnStarted | EventKind::TurnFinished)
            )
        })
        .max_by_key(|event| event.id)
}

/// The candidate a run waiting for the landing slot is to a cleanup for
/// disk space ([`WorktreeCleanup::Landing`]), `status` as the queue holds
/// it now: unless a turn of it runs or is about to ([`turn_idle`]; a run
/// with no turn yet has none running). Its task goes on, as the run is
/// leased.
fn landing_candidate(
    run: &LandingRun,
    status: RunStatus,
    events: &[RunEvent],
) -> Option<EndedRunWorktree> {
    if latest_turn_event(events).is_some() && !turn_idle(events) {
        return None;
    }
    Some(EndedRunWorktree {
        run_id: run.run_id.clone(),
        task_id: run.task_id,
        status,
        task_status: TaskStatus::InProgress,
        worktree: run.worktree.clone(),
        branch: run.branch.clone(),
        cleanup: WorktreeCleanup::Landing,
    })
}

/// Whether something runs for a waiting run in its `worktree`, besides
/// its session's `wrapper` waiting between turns and the `agent` of its
/// last turn: a process working in the worktree or under the wrapper (a
/// turn, or a command or build a turn left, cargo included;
/// [`run_processes`] of the recovery job, from `listed`), or one whose
/// executable is under the worktree wherever it works (a test or build
/// script from `target/`, [`worktree_executables`]). Never `except` (the
/// supervisor) nor what it runs or runs under.
fn worktree_busy(
    listed: &[ProcessInfo],
    executables: &[ProcessExecutable],
    wrapper: Option<u32>,
    agent: Option<u32>,
    worktree: &Path,
    except: u32,
) -> bool {
    !run_processes(listed, worktree, wrapper, agent, except).is_empty()
        || !worktree_executables(executables, worktree, except).is_empty()
}

/// A build cache of the queue's directory: a target directory something
/// builds into under its owner's lock, which the cleanup for room clears
/// through its owner ([`Self::clear_under_lock`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum BuildCache {
    /// The automatic update's target (host operation's own).
    Update,
    /// The declarations' build outputs in the landing recheck's
    /// directory, by their names there, which the recheck publishes
    /// ([`super::recheck::build_dirs`]) and clears under one lock.
    Recheck(Vec<String>),
}

/// The build caches a cleanup for room clears: the update's target, and
/// `recheck`, the build outputs in the recheck's directory (one per
/// declaration of a built-in profile, whatever is declared now). Their
/// owners clear the target alone: the rest of their directories (the
/// update's release, staged build and checkout, the recheck's scratch
/// worktree) stays.
fn build_caches(recheck: Vec<String>) -> [BuildCache; 2] {
    [BuildCache::Update, BuildCache::Recheck(recheck)]
}

impl BuildCache {
    /// `build_cache_removed`'s `cache`.
    const fn name(&self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::Recheck(_) => "recheck",
        }
    }
    /// Run `clear` on each target of the queue at `queue_dir` under its
    /// owner's lock, taken once without waiting ([`update::clear_target`],
    /// [`super::recheck::clear_targets`]): `None` with no target, or while
    /// it is in use.
    fn clear_under_lock<R>(
        &self,
        files: &dyn RunFiles,
        queue_dir: &Path,
        mut clear: impl FnMut(&Path) -> R,
    ) -> Result<Option<Vec<R>>> {
        match self {
            Self::Update => Ok(update::clear_target(
                files,
                &UpdatePaths::under(queue_dir),
                |target| clear(target),
            )?
            .map(|cleared| vec![cleared])),
            Self::Recheck(names) => super::recheck::clear_targets(files, queue_dir, names, clear),
        }
    }
}

/// Clear what each of `cache`'s target directories holds, keeping the
/// directory: nothing for one there is not (nothing built there, or a
/// link, which is not followed), and nothing while they are in use (the
/// next cleanup tries again). Its owner's lock is held until the targets
/// are cleared, so nothing starts to build there meanwhile. A target is
/// measured before it goes. A lock that cannot be taken fails at the
/// cache's directory.
fn clear_build_cache(files: &dyn RunFiles, queue_dir: &Path, cache: &BuildCache) -> Vec<Outcome> {
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
        Ok(None) => Vec::new(),
        Ok(Some(cleared)) => cleared
            .into_iter()
            .filter_map(|(path, removed)| match removed {
                Ok(None) => None,
                Ok(Some(size)) => Some(Outcome::BuildCache {
                    name: cache.name(),
                    path,
                    size,
                }),
                Err(error) => Some(Outcome::CacheFailed { path, error }),
            })
            .collect(),
        Err(error) => vec![Outcome::CacheFailed {
            path: queue_dir.join(cache.name()).to_string_lossy().into_owned(),
            error,
        }],
    }
}

/// The payload of `build_cache_removed`: which cache, its target and the
/// bytes it took.
fn build_cache_record(name: &str, path: &str, size: &Size) -> Value {
    let mut payload = json!({"cache": name, "paths": [path], "bytes": size.bytes, "by": "supervisor", "reason": BUILD_OUTPUTS_DISK_SPACE});
    size.mark(&mut payload);
    payload
}

/// What the directories a cleanup removed took on disk. A directory is
/// measured only for the record: whether it goes is decided before, so one
/// that cannot be measured goes all the same, its size unknown.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Size {
    /// The bytes of the directories measured.
    bytes: u64,
    /// Why each directory that was removed unmeasured could not be
    /// measured; its bytes count in no `bytes`.
    unmeasured: Vec<String>,
}

impl Size {
    fn of(bytes: u64) -> Self {
        Self {
            bytes,
            unmeasured: Vec::new(),
        }
    }
    fn unmeasured(why: String) -> Self {
        Self {
            bytes: 0,
            unmeasured: vec![why],
        }
    }
    fn add(&mut self, other: Self) {
        self.bytes += other.bytes;
        self.unmeasured.extend(other.unmeasured);
    }
    /// Name in a removal's `payload`, beside its `bytes`, why the
    /// directories removed unmeasured could not be measured (`unmeasured`),
    /// when there is one.
    fn mark(&self, payload: &mut Value) {
        if !self.unmeasured.is_empty() {
            payload["unmeasured"] = json!(self.unmeasured);
        }
    }
}

impl std::fmt::Display for Size {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} bytes", self.bytes)?;
        if !self.unmeasured.is_empty() {
            write!(f, ", and unmeasured: {}", self.unmeasured.join("; "))?;
        }
        Ok(())
    }
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
        size: Size,
    },
    /// The stale caches of a run that builds nothing now
    /// ([`remove_stale_entries`]): the directories they were in, how many
    /// entries went, and what that freed.
    StaleEntries {
        run_id: RunId,
        status: RunStatus,
        cleanup: WorktreeCleanup,
        paths: Vec<String>,
        entries: usize,
        size: Size,
    },
    /// The worktree and branch of a run whose task is over; `missing` when
    /// the directory was already gone and only the branch went, `repaired`
    /// when `git worktree repair` had to point the worktree at the
    /// repository again first, `broken_git` when Git could not take it
    /// for a worktree and its directory was removed instead (task 1587,
    /// [`may_remove_broken`]), `no_admin` when its `.git` named an admin
    /// dir not its own and only its directory was removed ([`owns_admin`]).
    Worktree {
        run_id: RunId,
        task_id: TaskId,
        task_status: TaskStatus,
        path: String,
        branch: String,
        size: Size,
        missing: bool,
        repaired: bool,
        broken_git: bool,
        no_admin: bool,
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
        size: Size,
    },
    /// The temporary files directories of a run whose task is over ([`run_tmp_dirs`]).
    RunTmp {
        run_id: RunId,
        task_status: TaskStatus,
        paths: Vec<String>,
        size: Size,
    },
    Failed {
        run_id: RunId,
        /// `worktree`, `scratchpad`, `run tmp`, `satisfied scratch` or
        /// `stale cache`.
        what: &'static str,
        path: String,
        error: anyhow::Error,
    },
    /// A build cache of the queue's directory cleared ([`clear_build_cache`]).
    BuildCache {
        name: &'static str,
        path: String,
        size: Size,
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
/// `incremental_sessions_removed`'s `reason`: the run waits for no
/// answer, with no lease ([`WorktreeCleanup::Idle`]).
const STALE_ENTRIES_IDLE: &str = "idle";
/// The run waits in its slot for the landing slot
/// ([`WorktreeCleanup::Landing`]).
const STALE_ENTRIES_LANDING_SLOT: &str = "awaiting_landing_slot";

/// The longest name Claude Code gives a project's directory whole; it
/// shortens a longer one with a hash, which is not looked for.
const SCRATCHPAD_NAME_MAX: usize = 200;

/// The name of the directory Claude Code keeps the scratchpads of the
/// sessions started in `cwd` under (task 1100): `cwd` with every character
/// but an ASCII letter or digit turned into `-` (so `/.local/` becomes
/// `--local-`), as it names `~/.claude/projects/` too. `None` when longer
/// than [`SCRATCHPAD_NAME_MAX`]. The name holds no `/` and no `..`, so it
/// stays directly under the directory it is joined to.
pub(in crate::application::supervise) fn scratchpad_dir_name(cwd: &str) -> Option<String> {
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
    queues: Arc<dyn QueueOpener<dyn crate::application::HostOpsQueue + Send>>,
    runs_dir: PathBuf,
    repo_root: PathBuf,
    cleaning: Arc<Mutex<Cleaning>>,
    /// The session's processes of each waiting or landing candidate, as
    /// its events named them when it was picked ([`session_pids`]).
    waiting: HashMap<RunId, SessionPids>,
    stop: Arc<AtomicBool>,
    prune: bool,
    /// For room ([`Request::idle`]): the runs whose build outputs only a
    /// cleanup for room removes lose them whole; otherwise only their
    /// stale caches ([`remove_stale_entries`]).
    room: bool,
    /// Where Claude Code keeps the sessions' scratchpads (task 1100).
    scratchpad_roots: Vec<PathBuf>,
    /// The queue's `satisfied/`, of the runs' `already_satisfied`
    /// verifications' scratch worktrees.
    satisfied_dir: PathBuf,
    /// The queue's directory, whose [`build_caches`] the job clears (for
    /// disk space).
    caches: Option<PathBuf>,
    /// The profiles the repository declares, as the job started: what
    /// names the build outputs of a worktree ([`remove_build_outputs`]).
    toolchains: Toolchains,
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
    ///   no worktree, its `.git` broken, so the run's own worktree
    ///   directory under `<runs>/<run-id>` was removed instead, task
    ///   1587, `no_admin: true` when its `.git` named an admin dir that is
    ///   another worktree's or none, so only that directory was removed
    ///   and the other's admin dir and branch left, and `stopped_processes` (`pid`, `executable`, `killed`)
    ///   for what ran from under the run's own worktree and was stopped
    ///   before its removal, or `processes_unlisted` when the processes
    ///   could not be listed, task 1590). A worktree whose directory is already gone
    ///   loses its branch (after `git worktree prune`), recorded the same
    ///   with `bytes` 0 and `worktree_missing: true`. The run directory and
    ///   its records, and the landed commit's `refs/dagq/runs/<run-id>`,
    ///   stay;
    /// - otherwise (the task may run it again, or retry it on a new run):
    ///   only the build outputs ([`remove_build_outputs`]) go, and the sources,
    ///   commits and run directory stay; recorded as
    ///   `build_outputs_removed` (`paths`, `bytes`, `by: supervisor`,
    ///   `reason: run_ended`). A link in their place is not followed (its
    ///   size reads as none and it stays), and a run waiting for triage is
    ///   a target too: its build outputs are made again once it resumes.
    ///
    /// The build outputs of an `awaiting_integration` or `needs_session`
    /// run of a task that goes on, with no lease and no live session, go
    /// too (task 1289): on every cleanup while an ask of it waits for an
    /// answer (`reason: awaiting_answer`, with its `ask_id`), else only in
    /// a cleanup for disk space (`reason: disk_space`). A cleanup for disk
    /// space takes the build outputs of the waiting runs `held` offers too
    /// (`reason: disk_space`, with the `ask_id` the wait holds).
    ///
    /// Not for room, a request for every run takes those runs, and the
    /// waiting runs and those waiting for the landing slot `held` offers,
    /// for their stale caches alone ([`remove_stale_entries`]), recorded as
    /// `incremental_sessions_removed` ([`stale_entries_record`]) when any
    /// went.
    ///
    /// `bytes` is what the removed files took on disk. A directory that
    /// cannot be measured goes all the same: its bytes are not counted,
    /// and why it could not be measured is named in `unmeasured`
    /// ([`Size`]). Only a worktree
    /// under the run directory is touched, never the checkout the
    /// supervisor was given. A failure records `cleanup_failed` (`path`,
    /// `message`, `by: supervisor`) once per worktree and process, is
    /// retried on the next sweep, and the others go on.
    ///
    /// Each of these events is a repair the runtime makes in a person's
    /// place, but `stats`' automatic repairs count only `auto_repaired`:
    /// a cleanup counts there only through the `auto_repaired` of a
    /// cleanup for disk space ([`super::disk`]).
    pub(in crate::application::supervise) fn request_cleanup(
        &mut self,
        env: &mut HostEnv<'_>,
        held: &Held,
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
    pub(in crate::application::supervise) fn poll_cleanup(
        &mut self,
        env: &mut HostEnv<'_>,
        held: &Held,
        ending: bool,
    ) {
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
        // Whatever the job left reserved, offered or reached (it could not
        // open the queue, or panicked) is free again.
        lock_cleaning(&self.cleanup.cleaning).release();
        self.cleanup.deferred = false;
        let cleaned = self.record_cleanup(env, outcomes);
        if let Some(disk) = for_room(job.disk, job.counted) {
            self.cleaned_for_disk(env, disk, &cleaned);
            // The next cleanup for room waits its interval from here, so
            // the pass after it judges the reading after it (task 1627).
            self.disk.cleaned = Some(env.generators.clock.monotonic());
        }
        self.start_cleanup(env, held);
    }
    /// A stop or a handoff: take no more requests, let an ordinary job end
    /// after its current worktree, and drop what waits but the rest of a
    /// cleanup for room another job took on. No job is joined: a reading
    /// of the disk taken before stays the one of the cleanup in progress.
    pub(in crate::application::supervise) fn end_cleanup(&mut self) {
        self.cleanup.end();
    }
    /// A handoff withdrawn while this process drained, which goes back to
    /// claims (task 1427): requests are taken again, an ordinary job that
    /// has not seen the stop yet goes on, and every ended run is asked for
    /// at once, which picks up what the drain dropped. A job that stopped
    /// after its current worktree leaves the rest to that request.
    pub(in crate::application::supervise) fn resume_cleanup(
        &mut self,
        env: &mut HostEnv<'_>,
        held: &Held,
    ) {
        if self.cleanup.resume() {
            self.request_cleanup(env, held, None, None);
        }
    }
    /// Once the loop ended: wait for the job and whatever waits for the
    /// next one, and record what they did. After a stop, only the running
    /// job and the rest of a cleanup for room it took on remain; ordinary
    /// cleanup stops after its current worktree. With a `deadline` (a
    /// supervisor whose owner is gone, [`super::OWNER_GONE_GRACE`], or
    /// whose heartbeat stopped, [`super::LEASE_LOST_GRACE`]), a job
    /// still running then is left, its outcomes unrecorded: a cleanup for
    /// room goes to its last candidate and may take long.
    pub(in crate::application::supervise) fn finish_cleanup(
        &mut self,
        env: &mut HostEnv<'_>,
        held: &Held,
        deadline: &super::EndBy,
    ) {
        while let Some(job) = &self.cleanup.job {
            if !super::finished_by(&job.handle, deadline, &*env.generators.clock) {
                warn!(
                    "the cleanup of ended runs' worktrees still runs as the supervisor ends; it is left, its outcomes unrecorded"
                );
                return;
            }
            self.poll_cleanup(env, held, false);
        }
    }
    /// Start a job for what waits, unless one runs: the candidates are
    /// picked here, on the loop, less the runs a slot holds. A cleanup for
    /// disk space, and the sweep of every run for their stale caches alone
    /// ([`Request::takes_held`]), also take the waiting runs the slots
    /// offer whose turn is over ([`waiting_candidate`]); the job reserves
    /// them as any other,
    /// which holds their waits still until it passed them. It takes last
    /// the runs the slots offer that wait for the landing slot
    /// ([`landing_candidate`]), in the order offered, the last to land
    /// first: each is reserved only once the job reaches it
    /// ([`Cleaning::reach`]), so the run nearest its landing is the last
    /// to lose its build outputs, and keeps them when the room the others
    /// made lets it land first. Not for room, the job leaves out the runs
    /// whose stale caches went while nothing could have built in them
    /// since ([`leave_pruned`]).
    fn start_cleanup(&mut self, env: &mut HostEnv<'_>, held: &Held) {
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
        let (waiting, waiting_pids): (Vec<_>, HashMap<_, _>) = if request.takes_held() {
            waiting_candidates(&*env.queue, &held.waiting)
                .into_iter()
                .map(|(candidate, pids)| {
                    let run = candidate.run_id.clone();
                    (candidate, (run, pids))
                })
                .unzip()
        } else {
            (Vec::new(), HashMap::new())
        };
        let (mut landing, landing_pids): (Vec<_>, Vec<_>) = if self.cleanup.takes_landing(&request)
        {
            landing_candidates(&*env.queue, &held.landing)
                .into_iter()
                .map(|(candidate, pids)| {
                    let run = candidate.run_id.clone();
                    (candidate, (run, pids))
                })
                .unzip()
        } else {
            (Vec::new(), Vec::new())
        };
        let mut waiting_pids = waiting_pids;
        waiting_pids.extend(landing_pids);
        let mut cleaning = lock_cleaning(&self.cleanup.cleaning);
        // The waiting runs first: their waits hold still until the job
        // passed them, so an answer waits for theirs alone.
        let mut candidates = waiting;
        candidates.extend(pick_candidates(
            listed,
            &request,
            |run| held.runs.contains(run),
            &mut cleaning.settled,
        ));
        // The sweep of every run alone: a request for some tasks takes none
        // of the pruned runs, so it would forget them all.
        if request.all && !request.idle {
            // The runs whose stale caches alone go first, so the job holds
            // their leases back only that short while, not while it removes
            // the ended runs' worktrees.
            candidates.sort_by_key(|candidate| !for_room_only(candidate.cleanup));
            leave_pruned(
                &mut cleaning.pruned,
                [&mut candidates, &mut landing],
                |run| waiting_pids.get(run).and_then(|pids| pids.turn),
            );
        }
        cleaning.reserved = candidates.iter().map(|w| w.run_id.clone()).collect();
        cleaning.offered = landing.iter().map(|w| w.run_id.clone()).collect();
        candidates.extend(landing);
        if candidates.is_empty() && !request.prune {
            cleaning.reserved.clear();
            cleaning.offered.clear();
            return;
        }
        drop(cleaning);
        let ports = JobPorts {
            files: env.files.clone(),
            processes: env.processes.clone(),
            repository: env.repository.clone(),
            queues: self.queues.clone(),
            runs_dir: env.layout.runs_dir.clone(),
            repo_root: env.layout.repo_root.clone(),
            cleaning: self.cleanup.cleaning.clone(),
            waiting: waiting_pids,
            // A cleanup for room goes to its last candidate: it is not
            // stopped with ordinary cleanup.
            stop: if for_room(request.disk, request.counted).is_some() {
                Arc::new(AtomicBool::new(false))
            } else {
                self.cleanup.stop.clone()
            },
            prune: request.prune,
            room: request.idle,
            scratchpad_roots: (self.scratchpad_roots)(),
            satisfied_dir: env.layout.satisfied_dir.clone(),
            caches: env
                .layout
                .db
                .parent()
                .filter(|_| request.caches)
                .map(Path::to_path_buf),
            // As the landing recheck resolves them (ADR-t2268-1); a
            // `dagq.toml` that cannot be read or resolved names no profile,
            // so nothing a profile the repository may not use names goes.
            toolchains: env.verifier.toolchains().unwrap_or_else(|error| {
                warn!(error = %format_args!("{error:#}"), "the toolchains of dagq.toml could not be read; the cleanup removes no build output of a worktree: {error:#}");
                Toolchains::none()
            }),
        };
        let handle = spawn_traced(move || run_job(&ports, candidates));
        self.cleanup.job = Some(Job {
            handle,
            disk: request.disk,
            counted: request.counted,
        });
    }
    /// Record the events of what the job did; what it removed.
    fn record_cleanup(&mut self, env: &mut HostEnv<'_>, outcomes: Vec<Outcome>) -> Cleaned {
        let mut cleaned = Cleaned::default();
        for outcome in outcomes {
            let recorded = match outcome {
                Outcome::BuildOutputs {
                    run_id,
                    status,
                    cleanup,
                    paths,
                    size,
                } => {
                    let (why, payload) = build_outputs_record(cleanup, &paths, &size);
                    info!(run_id = %run_id, "run {run_id} is {}{why}; removed the build outputs of its worktree ({size})", status.as_str());
                    cleaned.add(&run_id, size.bytes);
                    env.queue
                        .record_runtime_event(&run_id, EventKind::BuildOutputsRemoved, payload)
                }
                Outcome::StaleEntries {
                    run_id,
                    status,
                    cleanup,
                    paths,
                    entries,
                    size,
                } => {
                    let (why, payload) = stale_entries_record(cleanup, &paths, entries, &size);
                    info!(run_id = %run_id, "run {run_id} is {}{why} and builds nothing; removed {entries} stale cache entries of its worktree ({size})", status.as_str());
                    cleaned.add(&run_id, size.bytes);
                    env.queue.record_runtime_event(
                        &run_id,
                        EventKind::IncrementalSessionsRemoved,
                        payload,
                    )
                }
                Outcome::Worktree {
                    run_id,
                    task_id,
                    task_status,
                    path,
                    branch,
                    size,
                    missing,
                    repaired,
                    broken_git,
                    no_admin,
                    stopped,
                    unlisted,
                } => {
                    let payload = worktree_record(
                        task_status,
                        &path,
                        &branch,
                        &size,
                        Removed {
                            missing,
                            repaired,
                            broken_git,
                            no_admin,
                        },
                        &stopped,
                        unlisted.as_deref(),
                    );
                    if missing {
                        info!(run_id = %run_id, task_id = %task_id, "task {task_id} is {}; the worktree {path} of run {run_id} was already gone: removed its branch {branch}", task_status.as_str());
                    } else {
                        info!(run_id = %run_id, task_id = %task_id, "task {task_id} is {}; removed worktree {path} and branch {branch} of run {run_id} ({size})", task_status.as_str());
                    }
                    if broken_git {
                        info!(run_id = %run_id, "the worktree {path} of run {run_id} had a broken .git: removed its directory");
                    }
                    if no_admin {
                        info!(run_id = %run_id, "the .git of the worktree {path} of run {run_id} named an admin dir not its own: removed its directory only");
                    }
                    if !stopped.is_empty() {
                        info!(run_id = %run_id, "stopped {} process(es) running from the worktree {path} of run {run_id} before its removal", stopped.len());
                    }
                    if let Some(unlisted) = unlisted {
                        warn!(run_id = %run_id, "the processes running from the worktree {path} of run {run_id} could not be listed before its removal: {unlisted}");
                    }
                    cleaned.add(&run_id, size.bytes);
                    env.queue
                        .record_runtime_event(&run_id, EventKind::WorktreeRemoved, payload)
                }
                Outcome::Scratchpads {
                    run_id,
                    task_status,
                    paths,
                    size,
                } => {
                    info!(run_id = %run_id, "run {run_id}'s task is {}; removed its Claude Code scratchpad(s) {} ({size})", task_status.as_str(), paths.join(", "));
                    cleaned.add(&run_id, size.bytes);
                    env.queue.record_runtime_event(
                        &run_id,
                        EventKind::ScratchpadRemoved,
                        task_over_record(task_status, &paths, &size),
                    )
                }
                Outcome::RunTmp {
                    run_id,
                    task_status,
                    paths,
                    size,
                } => {
                    info!(run_id = %run_id, "run {run_id}'s task is {}; removed its temporary files {} ({size})", task_status.as_str(), paths.join(", "));
                    cleaned.add(&run_id, size.bytes);
                    env.queue.record_runtime_event(
                        &run_id,
                        EventKind::RunTmpRemoved,
                        task_over_record(task_status, &paths, &size),
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
                Outcome::BuildCache { name, path, size } => {
                    info!(
                        "the free disk space was short: cleared the {name} build cache {path} ({size})"
                    );
                    cleaned.add_cache(&path, size.bytes);
                    env.queue
                        .record_queue_event(
                            EventKind::BuildCacheRemoved,
                            build_cache_record(name, &path, &size),
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
/// prune` for disk space. The repository's branches are listed at most
/// once per job, when a candidate first needs them.
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
        |candidate| match candidate.cleanup {
            WorktreeCleanup::Waiting(_) | WorktreeCleanup::Landing => {
                still_waiting(&*queue, ports, candidate)
            }
            _ => queue.ended_run_worktree(&candidate.run_id),
        },
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
        let recheck = super::recheck::build_dirs(&*ports.files, queue_dir, &BUILTIN);
        outcomes.extend(
            build_caches(recheck)
                .iter()
                .flat_map(|cache| clear_build_cache(&*ports.files, queue_dir, cache)),
        );
    }
    outcomes
}

/// Pass each candidate in turn: under the lock, read it again
/// (`recheck`, that run alone) and clean it (`clean`, whether nothing is
/// left of it) only while the queue lists it as it was picked: nobody
/// leased it since, and neither it nor its task moved on. A run waiting
/// for the landing slot is reserved there, if the loop still offers it
/// ([`Cleaning::reach`]). A run passed with nothing left is settled
/// under the same lock it is released in, so a lease the loop takes after
/// it unsettles it.
fn pass_candidates(
    cleaning: &Mutex<Cleaning>,
    stop: &AtomicBool,
    candidates: Vec<EndedRunWorktree>,
    mut recheck: impl FnMut(&EndedRunWorktree) -> Result<Option<EndedRunWorktree>>,
    mut clean: impl FnMut(&EndedRunWorktree, &mut Vec<Outcome>) -> bool,
) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    for candidate in candidates {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let still = {
            let mut cleaning = lock_cleaning(cleaning);
            if !cleaning.reach(&candidate) {
                continue;
            }
            match recheck(&candidate) {
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
        cleaning.reached.retain(|run| *run != candidate.run_id);
        if settled {
            cleaning.settled.insert(candidate.run_id.clone(), candidate);
        }
    }
    lock_cleaning(cleaning).release();
    outcomes
}

/// The waiting runs of `waiting` a cleanup for disk space takes
/// ([`waiting_candidate`]), read on the loop: a run whose events or asks
/// cannot be read is left for the next cleanup.
fn waiting_candidates(
    queue: &(dyn HostOpsQueue + Send),
    waiting: &[WaitingRun],
) -> Vec<(EndedRunWorktree, SessionPids)> {
    waiting
        .iter()
        .filter_map(|run| {
            // The slot's copy of the run may be older than its status.
            let read = queue
                .run(&run.run_id)
                .and_then(|now| Ok((now.status(), queue.run_events(&run.run_id)?)))
                .inspect_err(|error| warn!(run_id = %run.run_id, error = %format_args!("{error:#}"), "run {}: whether a turn of it runs could not be read for the cleanup for disk space: {error:#}", run.run_id))
                .ok()?;
            let (status, events) = read;
            let run = WaitingRun {
                status,
                ..run.clone()
            };
            let candidate = waiting_candidate(&run, &events, |ask| {
                queue
                    .read_ask(ask)
                    .is_ok_and(|ask| ask.answered_at.is_none() && ask.closed_at.is_none())
            })?;
            Some((candidate, session_pids(&events)))
        })
        .collect()
}

/// The runs of `landing` a cleanup for disk space takes
/// ([`landing_candidate`]), in their order, read on the loop: a run whose
/// status or events cannot be read is left for the next cleanup.
fn landing_candidates(
    queue: &(dyn HostOpsQueue + Send),
    landing: &[LandingRun],
) -> Vec<(EndedRunWorktree, SessionPids)> {
    landing
        .iter()
        .filter_map(|run| {
            let read = queue
                .run(&run.run_id)
                .and_then(|now| Ok((now.status(), queue.run_events(&run.run_id)?)))
                .inspect_err(|error| warn!(run_id = %run.run_id, error = %format_args!("{error:#}"), "run {}: whether a turn of it runs could not be read for the cleanup for disk space: {error:#}", run.run_id))
                .ok()?;
            let (status, events) = read;
            let candidate = landing_candidate(run, status, &events)?;
            Some((candidate, session_pids(&events)))
        })
        .collect()
}

/// The processes of a waiting run's session: the pid of its wrapper (the
/// latest `wrapper_started`) and of the agent of its last turn (the latest
/// `turn_started`); and its latest turn event, which tells whether a turn
/// ran since its stale caches went ([`PrunedAs`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SessionPids {
    wrapper: Option<u32>,
    agent: Option<u32>,
    turn: Option<EventId>,
}

/// The [`SessionPids`] a run's `events` name. The loop reopens no session
/// and starts no turn of a run the job has reserved, so they stay the
/// session's while the job passes it.
fn session_pids(events: &[RunEvent]) -> SessionPids {
    let latest = |kind: EventKind| {
        events
            .iter()
            .filter(|event| event.kind == kind)
            .max_by_key(|event| event.id)
            .and_then(|event| event.payload["pid"].as_u64())
            .and_then(|pid| u32::try_from(pid).ok())
    };
    SessionPids {
        wrapper: latest(EventKind::WrapperStarted),
        agent: latest(EventKind::TurnStarted),
        turn: latest_turn_event(events).map(|event| event.id),
    }
}

/// A waiting or landing candidate as the job finds it again under the
/// lock: its status is the same, the agent of its last turn is gone, and
/// nothing else runs for it ([`worktree_busy`]). The loop delivers no
/// answer, starts no turn and begins no landing or e2e while the job has
/// it reserved, so it stays so while its build outputs go. The lease is
/// the loop's: this live process keeps heartbeating it, so no other
/// supervisor takes the run meanwhile.
fn still_waiting(
    queue: &(dyn HostOpsQueue + Send),
    ports: &JobPorts,
    candidate: &EndedRunWorktree,
) -> Result<Option<EndedRunWorktree>> {
    let run = &candidate.run_id;
    if queue.run(run)?.status() != candidate.status {
        return Ok(None);
    }
    let SessionPids { wrapper, agent, .. } = ports.waiting.get(run).copied().unwrap_or_default();
    let worktree = Path::new(&candidate.worktree);
    let resolved = ports
        .files
        .canonicalize(worktree)
        .unwrap_or_else(|_| worktree.to_path_buf());
    // A turn's agent still alive is a turn that runs; a listing that
    // cannot be read counts as something running.
    let busy = agent.is_some_and(|agent| ports.processes.alive(agent))
        || match (ports.processes.list(), ports.processes.executables()) {
            (Ok(listed), Ok(executables)) => worktree_busy(
                &listed,
                &executables,
                wrapper,
                agent,
                &resolved,
                std::process::id(),
            ),
            _ => true,
        };
    if busy {
        info!(run_id = %run, "run {run} waits, but something runs for it: its build outputs stay");
        return Ok(None);
    }
    Ok(Some(candidate.clone()))
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
    if stale_only(ports.room, candidate.cleanup) {
        clean_stale(ports, candidate, outcomes);
    } else {
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
    }
    remove_scratchpads(ports, candidate, outcomes);
    remove_run_tmp(ports, candidate, outcomes);
    remove_satisfied_scratch(ports, candidate, outcomes);
    let mine = &outcomes[first..];
    let failed = !runner || mine.iter().any(|o| matches!(o, Outcome::Failed { .. }));
    // One that failed is not pruned: the next sweep tries it again.
    if stale_only(ports.room, candidate.cleanup) && !failed {
        // Under the reservation: a lease the loop takes after it forgets it.
        lock_cleaning(&ports.cleaning).pruned.insert(
            candidate.run_id.clone(),
            PrunedAs {
                worktree: candidate.clone(),
                turn: ports
                    .waiting
                    .get(&candidate.run_id)
                    .and_then(|pids| pids.turn),
            },
        );
    }
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
            failed,
        },
    )
}

/// Whether a job removes only the stale caches of a candidate's worktree
/// ([`remove_stale_entries`]): a run whose build outputs only a cleanup
/// for room removes, in a job not for room.
const fn stale_only(room: bool, cleanup: WorktreeCleanup) -> bool {
    !room && for_room_only(cleanup)
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
/// The removal records no event (a log line only), and its size counts in
/// no event's `bytes`.
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

/// The temporary files directories of an ended run once its task is over:
/// the [`RUN_TMP_DIR`] in its run directory, the `TMPDIR` the runtime gave
/// its worker's turns (Claude's and Codex's), and its
/// [`host_verification_tmp_path`] outside the queue's `runs/`, the one its
/// landing's verification and its `already_satisfied` snapshot's had on
/// the host, also when its supervisor stopped while they ran. A run whose
/// task goes on keeps them, as a resume may go on in them and its landing
/// may be verified again; nothing else of the run directory goes.
/// The removal of the build outputs of a run left in the middle (one
/// waiting for an answer, a landing or a resume, the cleanup for disk
/// space included) leaves them too: what the worker keeps there (a
/// throwaway repository, a build outside the worktree) is what its next
/// turn goes on with in the same session, which no look at the processes
/// running now can tell from what nothing needs any more.
fn run_tmp_dirs(candidate: &EndedRunWorktree, runs_dir: &Path) -> Vec<PathBuf> {
    if !task_over(candidate.task_status) {
        return Vec::new();
    }
    vec![
        runs_dir.join(candidate.run_id.as_str()).join(RUN_TMP_DIR),
        host_verification_tmp_path(&candidate.run_id),
    ]
}

/// Measure the directory `dir` before it goes: `None` for no directory
/// there, or a link (left alone, nothing outside it is followed). One
/// that cannot be measured is still a directory to remove, its size
/// unknown ([`Size::unmeasured`]), unless what it is cannot be read either.
fn measure(files: &dyn RunFiles, dir: &Path) -> Result<Option<Size>> {
    let error = match files.tree_size(dir) {
        Ok(size) => return Ok(size.map(Size::of)),
        Err(error) => error,
    };
    let why = format!("measure {}: {error}", dir.display());
    match files.entry_kind(dir) {
        Ok(crate::application::EntryKind::Dir) => Ok(Some(Size::unmeasured(why))),
        Ok(_) => Ok(None),
        Err(kind) if kind.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(kind) => Err(anyhow::Error::new(kind).context(format!(
            "{why}; and reading what {} is failed",
            dir.display()
        ))),
    }
}

/// The size of the worktree or clone `dir` of a run whose task is over,
/// which goes whatever it is: none for none there, and unknown for one
/// that cannot be measured.
fn worktree_size(files: &dyn RunFiles, dir: &Path) -> Size {
    match files.tree_size(dir) {
        Ok(size) => Size::of(size.unwrap_or(0)),
        Err(error) => Size::unmeasured(format!("measure {}: {error}", dir.display())),
    }
}

/// Measure ([`measure`]) and remove the directory `dir`; its size, or
/// `None` for no directory there, a link, or one gone before its removal
/// (another supervisor's cleanup, or Claude Code's), which is no failure.
fn remove_tree(files: &dyn RunFiles, dir: &Path) -> Result<Option<Size>> {
    let Some(size) = measure(files, dir)? else {
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
    let mut size = Size::default();
    for dir in scratchpad_dirs(candidate, &ports.runs_dir, &ports.scratchpad_roots) {
        match remove_tree(&*ports.files, &dir) {
            Ok(Some(removed)) => {
                paths.push(dir.to_string_lossy().into_owned());
                size.add(removed);
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
            size,
        });
    }
}

/// The scratch worktree of an ended run's `already_satisfied`
/// verification ([`crate::application::integrate::satisfied_scratch`]),
/// which one whose supervisor died while it ran leaves behind: none for a
/// run in the middle, whose claim may be verified again.
fn satisfied_scratch_of(candidate: &EndedRunWorktree, satisfied_dir: &Path) -> Option<PathBuf> {
    (candidate.cleanup == WorktreeCleanup::Ended)
        .then(|| crate::application::integrate::satisfied_scratch(satisfied_dir, &candidate.run_id))
}

/// Remove the [`satisfied_scratch_of`] an ended run, with Git's record of
/// it, without trusting what is there. The removal records no event (a log
/// line only).
fn remove_satisfied_scratch(
    ports: &JobPorts,
    candidate: &EndedRunWorktree,
    outcomes: &mut Vec<Outcome>,
) {
    let Some(scratch) = satisfied_scratch_of(candidate, &ports.satisfied_dir) else {
        return;
    };
    if !ports.files.exists(&scratch) {
        return;
    }
    match ports.repository.remove_scratch(&scratch) {
        Ok(()) => {
            info!(run_id = %candidate.run_id, "run {} is {}; removed the scratch worktree {} its verification left", candidate.run_id, candidate.status.as_str(), scratch.display());
        }
        Err(error) => outcomes.push(Outcome::Failed {
            run_id: candidate.run_id.clone(),
            what: "satisfied scratch",
            path: scratch.to_string_lossy().into_owned(),
            error,
        }),
    }
}

/// Remove the [`run_tmp_dirs`] of an ended run. What was removed is
/// pushed to `outcomes`, in one record whose size is theirs together, with
/// a failure of the other.
fn remove_run_tmp(ports: &JobPorts, candidate: &EndedRunWorktree, outcomes: &mut Vec<Outcome>) {
    let mut paths = Vec::new();
    let mut size = Size::default();
    for dir in run_tmp_dirs(candidate, &ports.runs_dir) {
        let path = dir.to_string_lossy().into_owned();
        match remove_tree(&*ports.files, &dir) {
            Ok(Some(removed)) => {
                paths.push(path);
                size.add(removed);
            }
            Ok(None) => {}
            Err(error) => outcomes.push(Outcome::Failed {
                run_id: candidate.run_id.clone(),
                what: "run tmp",
                path,
                error,
            }),
        }
    }
    if !paths.is_empty() {
        outcomes.push(Outcome::RunTmp {
            run_id: candidate.run_id.clone(),
            task_status: candidate.task_status,
            paths,
            size,
        });
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
    if crate::domain::run_clone::is_clone_path(worktree) {
        return clean_clone(ports, candidate, branches);
    }
    if !safe_worktree(worktree, &ports.runs_dir, &ports.repo_root) {
        return Ok(None);
    }
    let over = task_over(candidate.task_status);
    let removed = |size, missing, removal, branch: &str, stopped, unlisted| Outcome::Worktree {
        run_id: candidate.run_id.clone(),
        task_id: candidate.task_id,
        task_status: candidate.task_status,
        path: candidate.worktree.clone(),
        branch: branch.to_owned(),
        size,
        missing,
        repaired: removal == Removal::Repaired,
        broken_git: removal == Removal::BrokenGit,
        no_admin: removal == Removal::NoAdmin,
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
            Size::default(),
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
        let size = worktree_size(&*ports.files, worktree);
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
            size, false, removal, branch, stopped, unlisted,
        )));
    }
    let tracks = |name: &str| ports.repository.tracks(worktree, name);
    let (paths, size) = remove_build_outputs(&*ports.files, worktree, &ports.toolchains, &tracks)?;
    if paths.is_empty() {
        return Ok(None);
    }
    Ok(Some(Outcome::BuildOutputs {
        run_id: candidate.run_id.clone(),
        status: candidate.status,
        cleanup: candidate.cleanup,
        paths,
        size,
    }))
}

/// Remove the build outputs of `worktree`: the directories directly under
/// it the declared profiles name ([`Toolchains::worktree_build_dirs`]),
/// none while none is declared, unless `tracks` says Git tracks a file in
/// one. A link in their place measures as none and stays ([`measure`]).
/// The paths removed, and what they took.
fn remove_build_outputs(
    files: &dyn RunFiles,
    worktree: &Path,
    toolchains: &Toolchains,
    tracks: &dyn Fn(&str) -> Result<bool>,
) -> Result<(Vec<String>, Size)> {
    let mut paths = Vec::new();
    let mut size = Size::default();
    for name in toolchains.worktree_build_dirs() {
        let dir = worktree.join(name);
        let Some(measured) = measure(files, &dir)? else {
            continue;
        };
        if tracks(name)? {
            continue;
        }
        files
            .remove_dir_all(&dir)
            .with_context(|| format!("remove {}", dir.display()))?;
        paths.push(dir.to_string_lossy().into_owned());
        size.add(measured);
    }
    Ok((paths, size))
}

/// Remove the stale caches of a candidate's worktree
/// ([`remove_stale_entries`]), a run's own under the runs directory and
/// not a clone (whose build outputs are elsewhere): what went, and each
/// failure on its own, are pushed to `outcomes`.
fn clean_stale(ports: &JobPorts, candidate: &EndedRunWorktree, outcomes: &mut Vec<Outcome>) {
    let worktree = Path::new(&candidate.worktree);
    if crate::domain::run_clone::is_clone_path(worktree)
        || !safe_worktree(worktree, &ports.runs_dir, &ports.repo_root)
        || !ports.files.is_dir(worktree)
    {
        return;
    }
    let removal = remove_stale_entries(&*ports.files, worktree, &ports.toolchains, &|name| {
        ports.repository.tracks(worktree, name)
    });
    if !removal.paths.is_empty() {
        outcomes.push(Outcome::StaleEntries {
            run_id: candidate.run_id.clone(),
            status: candidate.status,
            cleanup: candidate.cleanup,
            paths: removal.paths,
            entries: removal.entries,
            size: removal.size,
        });
    }
    outcomes.extend(
        removal
            .failed
            .into_iter()
            .map(|(path, error)| Outcome::Failed {
                run_id: candidate.run_id.clone(),
                what: "stale cache",
                path,
                error,
            }),
    );
}

/// What [`remove_stale_entries`] did: the directories something went
/// from, how many entries went whole and what went freed (also from an
/// entry that failed halfway), and what failed, each with its path.
#[derive(Debug, Default)]
struct StaleRemoval {
    paths: Vec<String>,
    entries: usize,
    size: Size,
    failed: Vec<(String, anyhow::Error)>,
}

/// Remove what the declared profiles name stale in the build directories
/// of `worktree` ([`Toolchains::stale_build_entries`]), a run that builds
/// nothing now: rustc's incremental sessions no build reads any more. A
/// build directory that is not one (a link included) or that Git tracks a
/// file in (`tracks`) is left, and so is anything a profile does not
/// name; no link is followed. Each directory is measured before and after,
/// as what went shares files with what stayed (rustc links the files of a
/// session into the next one), so the sizes of the entries would count
/// those files that stay. A directory is returned when an entry went or
/// when it freed bytes (or could not be measured), as an entry that fails
/// halfway (a session's directory losing some of its files) frees what
/// went from it, which no later sweep finds again; one that lost nothing
/// is not.
///
/// A directory that is not there or is no directory (a link) has nothing;
/// one that cannot be read, an entry that cannot be removed, and a build directory
/// Git cannot be asked about are failures, which leave the rest to go and
/// are returned beside what went: a failure is no "nothing stale", so the
/// run is tried again ([`clean_candidate`]).
fn remove_stale_entries(
    files: &dyn RunFiles,
    worktree: &Path,
    toolchains: &Toolchains,
    tracks: &dyn Fn(&str) -> Result<bool>,
) -> StaleRemoval {
    use crate::application::EntryKind;
    // Not there, or no directory: a link the port opens without following
    // it fails with ELOOP on macOS and ENOTDIR on Linux.
    let gone = |error: &std::io::Error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        ) || error.raw_os_error() == Some(libc::ELOOP)
    };
    let failed = std::cell::RefCell::new(Vec::new());
    let fail = |path: &Path, what: &str, error: anyhow::Error| {
        let path = path.to_string_lossy().into_owned();
        let error = error.context(format!("{what} {path}"));
        failed.borrow_mut().push((path, error));
    };
    // Whether `dir` is a directory: `None` once that could not be read.
    let is_dir = |dir: &Path| match files.entry_kind(dir) {
        Ok(kind) => Some(kind == EntryKind::Dir),
        Err(error) if gone(&error) => Some(false),
        Err(error) => {
            fail(dir, "read", error.into());
            None
        }
    };
    let read_dir = |dir: &Path| -> Vec<String> {
        if is_dir(dir) != Some(true) {
            return Vec::new();
        }
        match files.read_dir(dir) {
            Ok(entries) => entries
                .iter()
                .filter_map(|entry| entry.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .collect(),
            Err(error) if gone(&error) => Vec::new(),
            Err(error) => {
                fail(dir, "read", error.into());
                Vec::new()
            }
        }
    };
    let measure = |dir: &Path| match files.tree_size(dir) {
        Ok(size) => Ok(size.unwrap_or(0)),
        Err(error) => Err(format!("measure {}: {error}", dir.display())),
    };
    let mut removal = StaleRemoval::default();
    for name in toolchains.worktree_build_dirs() {
        let build_dir = worktree.join(name);
        if is_dir(&build_dir) != Some(true) {
            continue;
        }
        match tracks(name) {
            Ok(false) => {}
            Ok(true) => continue,
            Err(error) => {
                fail(&build_dir, "ask Git whether it tracks a file in", error);
                continue;
            }
        }
        for stale in toolchains.stale_build_entries(name, &build_dir, &read_dir) {
            let before = measure(&stale.dir);
            let mut removed = 0;
            for entry in &stale.remove {
                let path = stale.dir.join(entry);
                let result = match files.entry_kind(&path) {
                    Ok(EntryKind::Dir) => files.remove_dir_all(&path),
                    Ok(_) => files.remove_file(&path),
                    Err(error) => Err(error),
                };
                match result {
                    Ok(()) => removed += 1,
                    Err(error) if gone(&error) => {}
                    Err(error) => fail(&path, "remove", error.into()),
                }
            }
            let freed = match (before, measure(&stale.dir)) {
                (Ok(before), Ok(after)) => Size::of(before.saturating_sub(after)),
                (Err(why), _) | (_, Err(why)) => Size::unmeasured(why),
            };
            if removed == 0 && freed == Size::default() {
                continue;
            }
            removal.size.add(freed);
            removal.paths.push(stale.dir.to_string_lossy().into_owned());
            removal.entries += removed;
        }
    }
    removal.failed = failed.into_inner();
    removal
}

/// Whether the run's task is over and its worktree is the run's own under
/// `<runs>/<run-id>` ([`crate::domain::is_run_worktree`]), holding no part
/// of the repository's
/// checkout: the worktree the cleanup stops what runs from under before it
/// removes it (task 1590), and may remove as a directory when Git cannot
/// ([`may_remove_broken`]). The worktree of a run whose task goes on only
/// loses its build outputs, and its processes are left alone; a leased run
/// is no candidate.
fn own_ended_worktree(candidate: &EndedRunWorktree, runs_dir: &Path, repo_root: &Path) -> bool {
    let worktree = Path::new(&candidate.worktree);
    task_over(candidate.task_status)
        && crate::domain::is_run_worktree(&runs_dir.join(candidate.run_id.as_str()), worktree)
        && !repo_root.starts_with(worktree)
}

/// Clean one ended container run's clone (ADR-t2179-1 decision 7): once
/// its task is over, the clone (removed as files, with no git run in it
/// and no symlink followed), its landing worktree, the control side's run
/// branch and the bundles and state file of its directory go together. A
/// clone has no build output to take while the task goes on: that is in a
/// volume of the dagq machine, and nothing in the clone is read.
fn clean_clone(
    ports: &JobPorts,
    candidate: &EndedRunWorktree,
    branches: &mut Option<Vec<String>>,
) -> Result<Option<Outcome>> {
    let (true, Some(branch)) = (
        task_over(candidate.task_status),
        candidate.branch.as_deref(),
    ) else {
        return Ok(None);
    };
    let clone = Path::new(&candidate.worktree);
    let missing = !ports.files.is_dir(clone);
    if missing {
        let listed = match branches {
            Some(listed) => listed,
            None => branches.insert(ports.repository.branches()?),
        };
        let short = branch.trim_start_matches("refs/heads/");
        if !listed.iter().any(|listed| listed == short) {
            return Ok(None);
        }
        listed.retain(|listed| listed != short);
    }
    let size = worktree_size(&*ports.files, clone);
    ports.repository.remove_clone(clone, branch)?;
    Ok(Some(Outcome::Worktree {
        run_id: candidate.run_id.clone(),
        task_id: candidate.task_id,
        task_status: candidate.task_status,
        path: candidate.worktree.clone(),
        branch: branch.to_owned(),
        size,
        missing,
        repaired: false,
        broken_git: false,
        no_admin: false,
        stopped: Vec::new(),
        unlisted: None,
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
    /// Its `.git` named an admin dir that is not its own ([`owns_admin`]),
    /// so only its directory was removed.
    NoAdmin,
}

/// Remove the worktree and its branch. When Git refuses (the worktree's
/// `.git` points at a common directory the queue's rebind left behind),
/// `git worktree repair` points it at the repository again and the removal
/// is tried once more. The repair finds the admin dir by the name the
/// `.git` gives, so first [`owns_admin`] tells whether that one is the
/// worktree's own: when it is not (the worktree lost its own and another
/// worktree took the name), neither the repair nor a removal by Git runs,
/// which would move the other's admin dir here and remove it; the run's own
/// directory is removed as one with no admin dir and the branch deleted,
/// the other's admin dir and branch left as they are. When the repair finds
/// the `.git` broken and [`may_remove_broken`] allows it (task 1587), the
/// directory is removed, the worktrees whose directory is gone are pruned
/// and the branch is deleted.
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
    let admin = repository
        .worktree_admin(worktree)
        .with_context(|| format!("{error:#}; and reading the admin dir its .git names failed"))?;
    if !owns_admin(&admin, branch) {
        if !own_ended_worktree(candidate, &ports.runs_dir, &ports.repo_root) {
            bail!("{error:#}; its .git names an admin dir not its own ({admin:?}), so it is left");
        }
        match ports.files.remove_dir_all(worktree) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(failed) => {
                return Err(anyhow::Error::new(failed).context(format!(
                    "{error:#}; its .git names an admin dir not its own ({admin:?}), and removing {} failed",
                    worktree.display()
                )));
            }
        }
        repository.delete_branch(branch).with_context(|| {
            format!(
                "removed {} as its .git names an admin dir not its own",
                worktree.display()
            )
        })?;
        return Ok(Removal::NoAdmin);
    }
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

/// Whether `admin`, the admin dir a worktree's `.git` names, is the
/// worktree's own, which `git worktree repair` may point it at again: its
/// `gitdir` points back at the worktree, or its `HEAD` is `branch`, the
/// run's (a worktree the queue's rebind moved, its record still at the old
/// path). One that is missing, points elsewhere and has another `HEAD` is
/// another worktree's that took the name. A `.git` naming none is left to
/// the repair, which cannot follow it to another's.
fn owns_admin(admin: &WorktreeAdmin, branch: &str) -> bool {
    match admin {
        WorktreeAdmin::Unreadable => true,
        WorktreeAdmin::Missing => false,
        WorktreeAdmin::Found { points_back, head } => {
            let own = format!("refs/heads/{}", branch.trim_start_matches("refs/heads/"));
            *points_back || head.strip_prefix("ref: ").map(str::trim) == Some(own.as_str())
        }
    }
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
/// `git worktree remove --force`), the worktree is the run's own under
/// `<runs>/<run-id>` ([`crate::domain::is_run_worktree`]) and holds no part
/// of the repository's
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
    use crate::application::{DiffNumbers, EntryKind, RepositoryReads};
    use crate::domain::toolchain::fake;

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
        /// Measuring fails with too many open files, as a tree with more
        /// children than the file descriptors did.
        unmeasurable: bool,
        /// Not a directory, but `kind` (a link), or what it is cannot be
        /// read (`None` with `dir` false and `kind_fails`).
        kind: Option<EntryKind>,
        kind_fails: bool,
        fails: bool,
        lock_fails: bool,
        /// What `read_dir` lists; `None` is no directory.
        entries: Option<Vec<PathBuf>>,
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
        fn read_dir(&self, dir: &Path) -> std::io::Result<Vec<PathBuf>> {
            self.did("list", dir);
            self.entries
                .clone()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
        }
        fn rename(&self, _: &Path, _: &Path) -> std::io::Result<()> {
            unimplemented!()
        }
        fn remove_file(&self, _: &Path) -> std::io::Result<()> {
            unimplemented!()
        }
        fn tree_size(&self, dir: &Path) -> std::io::Result<Option<u64>> {
            self.did("measure", dir);
            if self.unmeasurable {
                return Err(std::io::Error::from_raw_os_error(libc::EMFILE));
            }
            Ok(self.size)
        }
        fn entry_kind(&self, _: &Path) -> std::io::Result<EntryKind> {
            if self.dir {
                return Ok(EntryKind::Dir);
            }
            if self.kind_fails {
                return Err(std::io::Error::from_raw_os_error(libc::EMFILE));
            }
            self.kind
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
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
        fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
            Ok(path.to_path_buf())
        }
        fn write_fenced(&self, _: &Path, _: &str, _: &str, _: &Path) -> Result<()> {
            unimplemented!()
        }
        fn now(&self) -> std::time::SystemTime {
            unimplemented!()
        }
    }

    /// What too many open files reads as in `unmeasured`.
    fn emfile() -> String {
        std::io::Error::from_raw_os_error(libc::EMFILE).to_string()
    }

    /// A directory that cannot be measured still goes, removed as one that
    /// can, its size unknown and the failure named; a link there and none
    /// there stay unmeasured and untouched, and so does one whose kind
    /// cannot be read either, which fails. One measured goes with its
    /// bytes, and one that cannot be removed fails however it measured.
    #[test]
    fn a_directory_that_cannot_be_measured_is_removed_with_its_size_unknown() {
        let dir = Path::new("/claude/projects/x");
        let unmeasurable = CacheFiles {
            dir: true,
            unmeasurable: true,
            ..CacheFiles::default()
        };
        let size = remove_tree(&unmeasurable, dir).unwrap().unwrap();
        assert_eq!(
            size,
            Size::unmeasured(format!("measure /claude/projects/x: {}", emfile()))
        );
        assert_eq!(
            unmeasurable.done(),
            ["measure /claude/projects/x", "remove /claude/projects/x"]
        );
        let payload = task_over_record(TaskStatus::Completed, &[dir.display().to_string()], &size);
        assert_eq!(payload["bytes"], 0);
        assert_eq!(
            payload["unmeasured"],
            json!([format!("measure /claude/projects/x: {}", emfile())])
        );
        for left in [
            CacheFiles {
                unmeasurable: true,
                kind: Some(EntryKind::Link),
                ..CacheFiles::default()
            },
            CacheFiles {
                unmeasurable: true,
                ..CacheFiles::default()
            },
        ] {
            assert_eq!(remove_tree(&left, dir).unwrap(), None);
            assert_eq!(left.done(), ["measure /claude/projects/x"]);
        }
        let unknown = CacheFiles {
            unmeasurable: true,
            kind_fails: true,
            ..CacheFiles::default()
        };
        let error = remove_tree(&unknown, dir).unwrap_err();
        assert!(format!("{error:#}").contains("reading what /claude/projects/x is failed"));
        assert_eq!(unknown.done(), ["measure /claude/projects/x"]);
        let measured = CacheFiles {
            dir: true,
            size: Some(4096),
            ..CacheFiles::default()
        };
        assert_eq!(remove_tree(&measured, dir).unwrap(), Some(Size::of(4096)));
        let measured_payload = task_over_record(TaskStatus::Completed, &[], &Size::of(4096));
        assert_eq!(measured_payload["bytes"], 4096);
        assert!(measured_payload.get("unmeasured").is_none());
        for (unmeasurable, size) in [(true, None), (false, Some(4096))] {
            let failing = CacheFiles {
                dir: true,
                size,
                unmeasurable,
                fails: true,
                ..CacheFiles::default()
            };
            let error = remove_tree(&failing, dir).unwrap_err();
            assert_eq!(format!("{error}"), "remove /claude/projects/x");
        }
    }

    /// The build outputs of a worktree that cannot be measured go, each
    /// named unmeasured with no bytes counted, and a directory Git tracks
    /// a file in still stays; one that cannot be removed still fails.
    #[test]
    fn build_outputs_that_cannot_be_measured_still_go() {
        let worktree = Path::new("/runs/r/worktree");
        let files = CacheFiles {
            dir: true,
            unmeasurable: true,
            ..CacheFiles::default()
        };
        let (paths, size) =
            remove_build_outputs(&files, worktree, &Toolchains::this_repository(), &|name| {
                Ok(name == "target")
            })
            .unwrap();
        assert_eq!(paths, ["/runs/r/worktree/llvm-cov-target"]);
        assert_eq!(
            size,
            Size::unmeasured(format!(
                "measure /runs/r/worktree/llvm-cov-target: {}",
                emfile()
            ))
        );
        let (_, payload) = build_outputs_record(WorktreeCleanup::Ended, &paths, &size);
        assert_eq!(payload["bytes"], 0);
        assert_eq!(payload["unmeasured"], json!(size.unmeasured));
        let failing = CacheFiles {
            dir: true,
            unmeasurable: true,
            fails: true,
            ..CacheFiles::default()
        };
        assert!(
            remove_build_outputs(&failing, worktree, &Toolchains::this_repository(), &|_| {
                Ok(false)
            })
            .is_err()
        );
    }

    /// A build cache that cannot be measured is cleared and made again,
    /// its size unknown in `build_cache_removed`.
    #[test]
    fn a_build_cache_that_cannot_be_measured_is_still_cleared() {
        let files = CacheFiles {
            dir: true,
            unmeasurable: true,
            ..CacheFiles::default()
        };
        let cleared = clear_build_cache(&files, Path::new("/q"), &BuildCache::Update);
        let [Outcome::BuildCache { path, size, .. }] = cleared.as_slice() else {
            panic!("not cleared");
        };
        assert_eq!(path, "/q/update/target");
        assert_eq!(
            size,
            &Size::unmeasured(format!("measure /q/update/target: {}", emfile()))
        );
        assert!(files.done().contains(&"create /q/update/target".to_owned()));
        let payload = build_cache_record("update", path, size);
        assert_eq!(payload["bytes"], 0);
        assert_eq!(payload["unmeasured"], json!(size.unmeasured));
    }

    /// The repository of an ended run whose worktree or clone goes: what
    /// was removed, and whether the removal fails.
    #[derive(Default)]
    struct Removing {
        fails: bool,
        removed: Mutex<Vec<String>>,
    }

    impl Removing {
        fn removal(&self, what: &str, path: &Path) -> Result<()> {
            if self.fails {
                bail!("git refused to remove {}", path.display());
            }
            self.removed
                .lock()
                .unwrap()
                .push(format!("{what} {}", path.display()));
            Ok(())
        }
    }

    impl RepositoryReads for Removing {
        fn main_head(&self) -> Result<CommitSha> {
            unimplemented!()
        }
        fn merge_base(&self, _: &str, _: &str) -> Result<Option<CommitSha>> {
            unimplemented!()
        }
    }

    impl Repository for Removing {
        fn is_dagq_source(&self) -> bool {
            unimplemented!()
        }
        fn current_branch(&self, _: &Path) -> Result<Option<String>> {
            unimplemented!()
        }
        fn head(&self, _: &Path) -> Result<CommitSha> {
            unimplemented!()
        }
        fn is_ancestor(&self, _: &str, _: &str) -> Result<bool> {
            unimplemented!()
        }
        fn status(&self, _: &Path) -> Result<String> {
            unimplemented!()
        }
        fn rebase_in_progress(&self, _: &Path) -> Result<bool> {
            unimplemented!()
        }
        fn rebase_abort(&self, _: &Path) -> Result<()> {
            unimplemented!()
        }
        fn rebase(&self, _: &Path, _: &str) -> Result<std::result::Result<(), String>> {
            unimplemented!()
        }
        fn conflicted_files(&self, _: &Path) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn changed_paths(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn added_paths(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn paths_in(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn paths_containing(&self, _: &str, _: &str, _: &[String]) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn rename_and_commit(
            &self,
            _: &Path,
            _: &str,
            _: &str,
            _: &[String],
        ) -> Result<std::result::Result<CommitSha, String>> {
            unimplemented!()
        }
        fn tree_of(&self, _: &str) -> Result<String> {
            unimplemented!()
        }
        fn commit_tree(&self, _: &str, _: &str, _: &[String]) -> Result<CommitSha> {
            unimplemented!()
        }
        fn update_ref(&self, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn advance_main(
            &self,
            _: &crate::domain::landing_branch::LandingBranch,
            _: &str,
            _: &str,
        ) -> Result<()> {
            unimplemented!()
        }
        fn repair_worktree(&self, _: &Path) -> Result<()> {
            bail!("no repair")
        }
        fn remove_worktree_and_branch(&self, worktree: &Path, _: &str) -> Result<()> {
            self.removal("worktree", worktree)
        }
        fn worktree_admin(&self, _: &Path) -> Result<WorktreeAdmin> {
            Ok(WorktreeAdmin::Unreadable)
        }
        fn branches(&self) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn delete_branch(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn tracks(&self, _: &Path, _: &str) -> Result<bool> {
            Ok(false)
        }
        fn main_checkout(&self) -> Result<Option<PathBuf>> {
            unimplemented!()
        }
        fn create_worktree(&self, _: &TaskRun) -> Result<String> {
            unimplemented!()
        }
        fn merge_conflicts(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn landed_task_ids(&self, _: &str, _: &str) -> Result<Vec<TaskId>> {
            unimplemented!()
        }
        fn log_oneline(&self, _: &str, _: &str) -> Result<String> {
            unimplemented!()
        }
        fn diff_stat(&self, _: &str, _: &str) -> Result<String> {
            unimplemented!()
        }
        fn diff_numbers(&self, _: &str, _: &str) -> Result<DiffNumbers> {
            unimplemented!()
        }
        fn diff_to_file(&self, _: &str, _: &str, _: &Path) -> Result<()> {
            unimplemented!()
        }
        fn remove_clone(&self, clone: &Path, _: &str) -> Result<()> {
            self.removal("clone", clone)
        }
    }

    /// Nothing runs from under a worktree.
    struct NoProcesses;

    impl ProcessControl for NoProcesses {
        fn alive(&self, _: u32) -> bool {
            false
        }
        fn terminate(&self, _: u32) -> Result<()> {
            Ok(())
        }
        fn interrupt(&self, _: u32) -> Result<()> {
            Ok(())
        }
        fn kill(&self, _: u32) -> Result<()> {
            Ok(())
        }
        fn executables(&self) -> Result<Vec<ProcessExecutable>> {
            Ok(Vec::new())
        }
    }

    /// No queue: cleaning one worktree opens none.
    struct NoQueue;

    impl QueueOpener<dyn HostOpsQueue + Send> for NoQueue {
        fn open(&self) -> Result<Box<dyn HostOpsQueue + Send>> {
            bail!("no queue")
        }
    }

    fn job_ports(files: CacheFiles, repository: &Arc<Removing>) -> JobPorts {
        JobPorts {
            files: Arc::new(files),
            processes: Arc::new(NoProcesses),
            repository: repository.clone(),
            queues: Arc::new(NoQueue),
            runs_dir: PathBuf::from("/runs"),
            repo_root: PathBuf::from("/repo"),
            cleaning: Arc::default(),
            waiting: HashMap::new(),
            stop: Arc::default(),
            prune: false,
            room: false,
            scratchpad_roots: vec![PathBuf::from("/claude/projects")],
            satisfied_dir: PathBuf::from("/q/satisfied"),
            caches: None,
            toolchains: Toolchains::this_repository(),
        }
    }

    /// The worktree, and the clone, of a run whose task is over go though
    /// they cannot be measured: the removal is recorded with its size
    /// unknown and the failure named, and is no `cleanup_failed`. One
    /// measured is recorded with its bytes, and a removal that fails is
    /// still a failure.
    #[test]
    fn an_ended_worktree_or_clone_that_cannot_be_measured_still_goes() {
        let worktree = candidate("r", TaskStatus::Completed);
        let clone = EndedRunWorktree {
            worktree: "/runs/r/clones/worktree-r".to_owned(),
            ..candidate("r", TaskStatus::Completed)
        };
        for (candidate, removed) in [
            (&worktree, "worktree /runs/r/worktree"),
            (&clone, "clone /runs/r/clones/worktree-r"),
        ] {
            let path = candidate.worktree.as_str();
            let repository = Arc::new(Removing::default());
            let ports = job_ports(
                CacheFiles {
                    dir: true,
                    unmeasurable: true,
                    ..CacheFiles::default()
                },
                &repository,
            );
            let outcome = clean_worktree(&ports, candidate, &mut None, &mut false)
                .unwrap()
                .unwrap();
            let Outcome::Worktree {
                size,
                missing: false,
                ..
            } = outcome
            else {
                panic!("not a removed worktree");
            };
            let why = format!("measure {path}: {}", emfile());
            assert_eq!(size, Size::unmeasured(why.clone()));
            let payload = worktree_record(
                TaskStatus::Completed,
                path,
                "refs/heads/dagq/r",
                &size,
                Removed::default(),
                &[],
                None,
            );
            assert_eq!(payload["bytes"], 0);
            assert_eq!(payload["unmeasured"], json!([why]));
            assert_eq!(*repository.removed.lock().unwrap(), [removed]);
            let measured = job_ports(
                CacheFiles {
                    dir: true,
                    size: Some(42),
                    ..CacheFiles::default()
                },
                &Arc::default(),
            );
            let Some(Outcome::Worktree { size, .. }) =
                clean_worktree(&measured, candidate, &mut None, &mut false).unwrap()
            else {
                panic!("not a removed worktree");
            };
            assert_eq!(size, Size::of(42));
            let failing = job_ports(
                CacheFiles {
                    dir: true,
                    unmeasurable: true,
                    ..CacheFiles::default()
                },
                &Arc::new(Removing {
                    fails: true,
                    ..Removing::default()
                }),
            );
            assert!(clean_worktree(&failing, candidate, &mut None, &mut false).is_err());
        }
    }

    /// The scratchpad of a run whose task is over goes though it cannot
    /// be measured, recorded as removed with its size unknown, not as a
    /// failure; one that cannot be removed is still a failure.
    #[test]
    fn a_scratchpad_that_cannot_be_measured_still_goes() {
        let candidate = candidate("r", TaskStatus::Canceled);
        let ports = job_ports(
            CacheFiles {
                dir: true,
                unmeasurable: true,
                ..CacheFiles::default()
            },
            &Arc::default(),
        );
        let mut outcomes = Vec::new();
        remove_scratchpads(&ports, &candidate, &mut outcomes);
        let [Outcome::Scratchpads { paths, size, .. }] = outcomes.as_slice() else {
            panic!("not a removed scratchpad");
        };
        assert_eq!(*paths, ["/claude/projects/-runs-r-worktree"]);
        assert_eq!(size.bytes, 0);
        assert_eq!(size.unmeasured.len(), 1);
        let failing = job_ports(
            CacheFiles {
                dir: true,
                unmeasurable: true,
                fails: true,
                ..CacheFiles::default()
            },
            &Arc::default(),
        );
        let mut outcomes = Vec::new();
        remove_scratchpads(&failing, &candidate, &mut outcomes);
        assert!(matches!(
            outcomes.as_slice(),
            [Outcome::Failed {
                what: "scratchpad",
                ..
            }]
        ));
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
                BuildCache::Recheck(vec!["target-tools".to_owned()]),
                "recheck",
                "/q/recheck/lock",
                "/q/recheck/target-tools",
            ),
        ] {
            let free = CacheFiles {
                dir: true,
                size: Some(7),
                ..CacheFiles::default()
            };
            assert!(matches!(
                clear_build_cache(&free, q, &cache).as_slice(),
                [Outcome::BuildCache { name: n, path, size }] if *n == name && path == target && *size == Size::of(7)
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
            assert!(clear_build_cache(&busy, q, &cache).is_empty());
            assert_eq!(busy.done(), [format!("lock {lock}")]);
            let missing = CacheFiles::default();
            assert!(clear_build_cache(&missing, q, &cache).is_empty());
            assert!(missing.done().is_empty());
            let link = CacheFiles {
                dir: true,
                ..CacheFiles::default()
            };
            assert!(clear_build_cache(&link, q, &cache).is_empty());
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
                clear_build_cache(&failing, q, &cache).as_slice(),
                [Outcome::CacheFailed { path, .. }] if path == target
            ));
            let unlockable = CacheFiles {
                dir: true,
                lock_fails: true,
                ..CacheFiles::default()
            };
            assert!(matches!(
                clear_build_cache(&unlockable, q, &cache).as_slice(),
                [Outcome::CacheFailed { path, .. }] if path == &format!("/q/{name}")
            ));
        }
    }

    /// A worktree loses the directories the declared profiles name and
    /// nothing else: the Rust profile's `target` and `llvm-cov-target`, a
    /// second profile's with nothing but its implementation (ADR-t2268-1
    /// decision 8), and none while no profile is declared, whatever the
    /// repository's declaration resolves to then. A directory Git tracks a
    /// file in stays.
    #[test]
    fn a_worktree_loses_the_build_outputs_the_declared_profiles_name() {
        let worktree = Path::new("/runs/r/worktree");
        let removed = |toolchains: &Toolchains, tracked: &[&str]| {
            let files = CacheFiles {
                size: Some(3),
                ..CacheFiles::default()
            };
            let tracks = |name: &str| Ok(tracked.contains(&name));
            let (paths, size) =
                remove_build_outputs(&files, worktree, toolchains, &tracks).unwrap();
            assert_eq!(size, Size::of(3 * paths.len() as u64));
            let removes: Vec<String> = files
                .done()
                .into_iter()
                .filter_map(|done| done.strip_prefix("remove ").map(str::to_owned))
                .collect();
            assert_eq!(removes, paths);
            paths
        };
        let under = |names: &[&str]| -> Vec<String> {
            names
                .iter()
                .map(|name| worktree.join(name).to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            removed(&Toolchains::this_repository(), &[]),
            under(&["target", "llvm-cov-target"])
        );
        assert_eq!(
            removed(&Toolchains::this_repository(), &["target"]),
            under(&["llvm-cov-target"])
        );
        assert_eq!(
            removed(&fake::only_fake(), &[]),
            under(&["fake-out", "fake-cache"])
        );
        assert!(removed(&Toolchains::none(), &[]).is_empty());
        // No declaration removes what its resolution names, and nothing
        // once it names no profile.
        let undeclared = Toolchains::resolve(&BUILTIN, &[]).unwrap();
        let named: Vec<&str> = undeclared
            .profiles()
            .flat_map(|profile| profile.worktree_build_dirs().iter().copied())
            .collect();
        assert_eq!(removed(&undeclared, &[]), under(&named));
    }

    /// The recheck's build outputs of several declarations are cleared
    /// under one take of the recheck lock, each its own outcome, so a
    /// recheck that starts meanwhile finds them all cleared or none; while
    /// a recheck holds the lock none is.
    #[test]
    fn the_recheck_build_outputs_are_cleared_together_under_one_lock() {
        let q = Path::new("/q");
        let cache = BuildCache::Recheck(vec!["target".to_owned(), "target-core".to_owned()]);
        let free = CacheFiles {
            dir: true,
            size: Some(7),
            ..CacheFiles::default()
        };
        let cleared: Vec<String> = clear_build_cache(&free, q, &cache)
            .into_iter()
            .map(|outcome| match outcome {
                Outcome::BuildCache {
                    name: "recheck",
                    path,
                    size,
                } if size == Size::of(7) => path,
                _ => panic!("not a cleared recheck cache"),
            })
            .collect();
        assert_eq!(cleared, ["/q/recheck/target", "/q/recheck/target-core"]);
        let done = free.done();
        assert_eq!(done.iter().filter(|d| d.starts_with("lock ")).count(), 1);
        assert_eq!(done[0], "lock /q/recheck/lock");
        assert!(done.contains(&"remove /q/recheck/target-core".to_owned()));
        let busy = CacheFiles {
            dir: true,
            busy: true,
            size: Some(7),
            ..CacheFiles::default()
        };
        assert!(clear_build_cache(&busy, q, &cache).is_empty());
        assert_eq!(busy.done(), ["lock /q/recheck/lock"]);
    }

    /// The caches cleared are the update's target and, in the recheck's
    /// directory, the build output of each declaration of a registered
    /// profile, whatever is declared now (a removed declaration's
    /// directory included), and nothing else there: a second profile's
    /// are cleared with nothing but its registration (ADR-t2268-1 decision
    /// 8). No recheck directory yet leaves the recheck nothing to clear.
    #[test]
    fn the_recheck_caches_are_each_declarations_build_output() {
        let q = Path::new("/q");
        let listed = CacheFiles {
            entries: Some(
                [
                    "worktree",
                    "lock",
                    "dagq-rustc-wrapper",
                    "target-tools",
                    "target",
                    "fake-out-web",
                ]
                .iter()
                .map(|name| Path::new("/q/recheck").join(name))
                .collect(),
            ),
            ..CacheFiles::default()
        };
        let recheck = |names: &[&str]| {
            [
                BuildCache::Update,
                BuildCache::Recheck(names.iter().map(|name| (*name).to_owned()).collect()),
            ]
        };
        assert_eq!(
            build_caches(super::super::recheck::build_dirs(&listed, q, &BUILTIN)),
            recheck(&["target", "target-tools"])
        );
        assert_eq!(listed.done(), ["list /q/recheck"]);
        assert_eq!(
            build_caches(super::super::recheck::build_dirs(
                &listed,
                q,
                &fake::REGISTRY
            )),
            recheck(&["fake-out-web", "target", "target-tools"])
        );
        let none = CacheFiles::default();
        assert_eq!(
            build_caches(super::super::recheck::build_dirs(&none, q, &BUILTIN)),
            recheck(&[])
        );
    }

    /// `build_cache_removed` names the cache, its target and its bytes,
    /// for disk space.
    #[test]
    fn a_cleared_build_cache_is_recorded_for_disk_space() {
        assert_eq!(
            build_cache_record("recheck", "/q/recheck/target", &Size::of(9)),
            json!({"cache": "recheck", "paths": ["/q/recheck/target"], "bytes": 9, "by": "supervisor", "reason": "disk_space"})
        );
    }

    /// A request for some tasks takes theirs but the runs whose build
    /// outputs only a cleanup for room removes; a request for every run
    /// takes those too, for their stale caches alone unless for room.
    #[test]
    fn cleanup_requests_merge_tasks_and_all_and_take_idle_runs_whole_only_for_room() {
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
            ..run.clone()
        };
        let waiting = EndedRunWorktree {
            cleanup: WorktreeCleanup::Waiting(AskId::new(8)),
            ..run
        };
        assert!(request.wants(&awaiting));
        assert!(!request.wants(&idle));
        assert!(!request.wants(&waiting));
        request.add(None, None);
        assert!(request.all);
        assert!(request.wants(&other));
        assert!(request.wants(&idle));
        assert!(request.wants(&waiting));
        assert!(request.takes_held() && !request.idle);
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
        assert!(request.wants(&waiting));
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
            &Size::of(42),
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
            &Size::default(),
            Removed {
                missing: true,
                repaired: true,
                broken_git: true,
                no_admin: true,
            },
            &stopped,
            Some("ps failed"),
        );
        assert_eq!(marked["reason"], "task_completed");
        assert_eq!(marked["worktree_missing"], true);
        assert_eq!(marked["repaired"], true);
        assert_eq!(marked["broken_git"], true);
        assert_eq!(marked["no_admin"], true);
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
            (
                Removed {
                    no_admin: true,
                    ..Removed::default()
                },
                "no_admin",
            ),
        ] {
            let payload = worktree_record(
                TaskStatus::Completed,
                "p",
                "b",
                &Size::default(),
                removed,
                &[],
                None,
            );
            let marks: Vec<&str> = [
                "worktree_missing",
                "repaired",
                "broken_git",
                "no_admin",
                "stopped_processes",
                "processes_unlisted",
            ]
            .into_iter()
            .filter(|mark| payload.get(mark).is_some())
            .collect();
            assert_eq!(marks, [key]);
        }

        let tmp = task_over_record(
            TaskStatus::Canceled,
            &["/runs/r/tmp".to_owned()],
            &Size::of(32_768),
        );
        assert_eq!(
            tmp,
            json!({"paths": ["/runs/r/tmp"], "bytes": 32_768, "by": "supervisor", "reason": "task_canceled"})
        );
        assert_eq!(
            task_over_record(TaskStatus::Completed, &[], &Size::default())["reason"],
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

    /// An ended run's `already_satisfied` scratch worktree goes from the
    /// queue's `satisfied/`, whatever its task; a run in the middle keeps
    /// it, as its claim may be verified again.
    #[test]
    fn an_ended_runs_satisfied_scratch_goes_from_the_queues_directory() {
        let dir = Path::new("/q/satisfied");
        for status in [TaskStatus::InProgress, TaskStatus::Completed] {
            assert_eq!(
                satisfied_scratch_of(&candidate("r", status), dir),
                Some(PathBuf::from("/q/satisfied/r"))
            );
        }
        for cleanup in [
            WorktreeCleanup::AwaitingAnswer(AskId::new(7)),
            WorktreeCleanup::Idle,
        ] {
            let middle = EndedRunWorktree {
                cleanup,
                ..candidate("r", TaskStatus::InProgress)
            };
            assert_eq!(satisfied_scratch_of(&middle, dir), None, "{cleanup:?}");
        }
    }

    /// Task 1100 and task 1290: the scratchpads and the temporary files
    /// directories of a run (its run directory's and its verification's on
    /// the host) go once its task is over; a run whose task goes
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
            assert!(run_tmp_dirs(&going_on, runs).is_empty(), "{status:?}");
        }
        for status in [TaskStatus::Completed, TaskStatus::Canceled] {
            let over = candidate("r", status);
            assert_eq!(
                scratchpad_dirs(&over, runs, &roots),
                [roots[0].join(&name), roots[1].join(&name)]
            );
            assert_eq!(
                run_tmp_dirs(&over, runs),
                [
                    PathBuf::from("/runs/r").join(RUN_TMP_DIR),
                    PathBuf::from("/tmp/dagq-v-r")
                ]
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
            (
                WorktreeCleanup::Waiting(AskId::new(7)),
                "disk_space",
                ", waiting outside its slot for the answer to ask 7, for disk space",
            ),
        ] {
            let (suffix, payload) = build_outputs_record(cleanup, &paths, &Size::of(42));
            assert_eq!(suffix, why);
            let mut expected =
                json!({"paths": paths, "bytes": 42, "by": "supervisor", "reason": reason});
            if let WorktreeCleanup::AwaitingAnswer(_) | WorktreeCleanup::Waiting(_) = cleanup {
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
            let (_, payload) = build_outputs_record(cleanup, &[], &Size::of(42));
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

    fn turn_event(id: i64, kind: &str) -> RunEvent {
        RunEvent {
            id: crate::domain::EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload: json!({}),
            created_at: String::new(),
            actor: None,
        }
    }

    /// A run waiting outside its slot is a candidate of a cleanup for room
    /// only once its last turn finished after the last one requested, for
    /// the oldest ask its wait holds that is still open; its task goes on,
    /// so only its build outputs go, its runner stays and it is no
    /// candidate of an ordinary cleanup.
    #[test]
    fn a_waiting_run_is_a_candidate_only_once_its_turn_is_over_and_an_ask_is_open() {
        let run = WaitingRun {
            run_id: RunId::new("w".to_owned()).unwrap(),
            task_id: TaskId::new(4),
            status: RunStatus::NeedsSession,
            worktree: "/runs/w/worktree".to_owned(),
            branch: Some("refs/heads/dagq/w".to_owned()),
            asks: vec![AskId::new(5), AskId::new(6)],
        };
        use event_kind::{TURN_FINISHED as F, TURN_REQUESTED as Q, TURN_STARTED as S};
        let open = |ask: AskId| ask != AskId::new(5);
        for (events, idle) in [
            (vec![], false),
            (vec![(1, Q)], false),
            (vec![(1, Q), (2, S)], false),
            (vec![(1, Q), (2, S), (3, F)], true),
            // The wait began before the turn that asked ended.
            (
                vec![(1, Q), (2, S), (4, F), (3, "run_waiting_started")],
                true,
            ),
            (vec![(1, Q), (2, S), (3, F), (4, Q)], false),
            (vec![(1, Q), (2, S), (3, F), (4, Q), (5, S)], false),
            // Read out of order, the latest by id is the one.
            (vec![(3, F), (1, Q), (2, S)], true),
        ] {
            let events: Vec<RunEvent> = events
                .into_iter()
                .map(|(id, kind)| turn_event(id, kind))
                .collect();
            assert_eq!(turn_idle(&events), idle, "{events:?}");
            let picked = waiting_candidate(&run, &events, open);
            assert_eq!(picked.is_some(), idle, "{events:?}");
            if let Some(picked) = picked {
                assert_eq!(
                    picked,
                    EndedRunWorktree {
                        run_id: run.run_id.clone(),
                        task_id: TaskId::new(4),
                        status: RunStatus::NeedsSession,
                        task_status: TaskStatus::InProgress,
                        worktree: run.worktree.clone(),
                        branch: run.branch.clone(),
                        cleanup: WorktreeCleanup::Waiting(AskId::new(6)),
                    }
                );
                assert!(for_room_only(picked.cleanup));
                assert!(!runner_goes(picked.cleanup));
                assert!(!task_over(picked.task_status));
            }
        }
        let finished = [turn_event(1, S), turn_event(2, F)];
        assert_eq!(waiting_candidate(&run, &finished, |_| false), None);
    }

    /// Something runs for a waiting run while a process works in its
    /// worktree (cargo too) or under its wrapper, or runs from its
    /// worktree's `target/` wherever it works; the wrapper waiting between
    /// turns, the agent of its last turn, the supervisor and what runs
    /// elsewhere are nothing.
    #[test]
    fn a_waiting_run_is_busy_while_something_works_in_its_worktree_or_under_its_wrapper() {
        let listed = |pid, ppid, cwd: &str| ProcessInfo {
            pid,
            ppid,
            elapsed_secs: 1,
            command: String::new(),
            cwd: Some(cwd.to_owned()),
            cpu_ms: None,
        };
        let exe = |pid, ppid, executable: &str| ProcessExecutable {
            pid,
            ppid,
            executable: Some(executable.to_owned()),
        };
        let worktree = Path::new("/runs/w/worktree");
        let (wrapper, agent, supervisor) = (10, 11, 99);
        let quiet = vec![
            listed(wrapper, 1, "/runs/w/worktree"),
            listed(agent, wrapper, "/runs/w/worktree"),
            listed(supervisor, 1, "/repo"),
            listed(98, supervisor, "/runs/w/worktree"),
            listed(30, 1, "/elsewhere"),
        ];
        let executables = [
            exe(30, 1, "/usr/bin/cargo"),
            exe(wrapper, 1, "/runs/w/runner"),
        ];
        let busy = |listed: &[ProcessInfo], executables: &[ProcessExecutable]| {
            worktree_busy(
                listed,
                executables,
                Some(wrapper),
                Some(agent),
                worktree,
                supervisor,
            )
        };
        assert!(!busy(&quiet, &executables));
        // Only as named: unnamed, they work in the worktree.
        assert!(worktree_busy(
            &quiet,
            &executables,
            None,
            None,
            worktree,
            supervisor
        ));
        let cargo = [quiet.clone(), vec![listed(40, 1, "/runs/w/worktree/src")]].concat();
        assert!(busy(&cargo, &executables));
        let under_wrapper = [quiet.clone(), vec![listed(41, wrapper, "/elsewhere")]].concat();
        assert!(busy(&under_wrapper, &executables));
        let test = [
            executables.to_vec(),
            vec![exe(42, 1, "/runs/w/worktree/target/debug/deps/it-0123")],
        ]
        .concat();
        assert!(busy(&quiet, &test));
    }

    /// A waiting run's session is its latest wrapper and the agent of its
    /// latest turn, as their events name them.
    #[test]
    fn a_waiting_runs_session_is_its_latest_wrapper_and_turn_agent() {
        let event = |id, kind: &str, pid: Value| RunEvent {
            payload: json!({ "pid": pid }),
            ..turn_event(id, kind)
        };
        assert_eq!(session_pids(&[]), SessionPids::default());
        let events = [
            event(1, event_kind::WRAPPER_STARTED, json!(10)),
            event(2, event_kind::TURN_STARTED, json!(11)),
            event(3, event_kind::TURN_FINISHED, json!(null)),
            event(5, event_kind::TURN_STARTED, json!(13)),
            event(4, event_kind::WRAPPER_STARTED, json!(12)),
            event(6, event_kind::AGENT_STARTED, json!(14)),
        ];
        assert_eq!(
            session_pids(&events),
            SessionPids {
                wrapper: Some(12),
                agent: Some(13),
                turn: Some(EventId::new(5)),
            }
        );
    }

    /// The job reserves a waiting run it picked as any other: the wait
    /// holds still while it is reserved, and not once it passed the run.
    #[test]
    fn a_picked_waiting_run_is_reserved_until_the_job_passed_it() {
        let waiting = EndedRunWorktree {
            cleanup: WorktreeCleanup::Waiting(AskId::new(6)),
            ..candidate("w", TaskStatus::InProgress)
        };
        let run = waiting.run_id.clone();
        let cleaning = Mutex::new(Cleaning {
            reserved: vec![run.clone()],
            ..Cleaning::default()
        });
        let stop = AtomicBool::new(false);
        let mut seen = Vec::new();
        pass_candidates(
            &cleaning,
            &stop,
            vec![waiting.clone()],
            |picked| Ok(Some(picked.clone())),
            |picked, _| {
                seen.push(lock_cleaning(&cleaning).reserved(&picked.run_id));
                false
            },
        );
        assert_eq!(seen, [true]);
        assert!(!lock_cleaning(&cleaning).reserved(&run));
    }

    /// A run waiting for the landing slot is a candidate of a cleanup for
    /// room unless a turn of it runs or was asked for; its task goes on,
    /// so only its build outputs go, its runner stays, an ordinary cleanup
    /// does not take it, and its removal is recorded for disk space.
    #[test]
    fn a_landing_run_is_a_candidate_for_room_only_while_no_turn_runs() {
        let run = LandingRun {
            run_id: RunId::new("l".to_owned()).unwrap(),
            task_id: TaskId::new(4),
            worktree: "/runs/l/worktree".to_owned(),
            branch: Some("refs/heads/dagq/l".to_owned()),
        };
        use event_kind::{TURN_FINISHED as F, TURN_REQUESTED as Q, TURN_STARTED as S};
        for (events, picked) in [
            (vec![], true),
            (vec![(1, Q)], false),
            (vec![(1, Q), (2, S)], false),
            (vec![(1, Q), (2, S), (3, F)], true),
            (vec![(1, Q), (2, S), (3, F), (4, Q)], false),
        ] {
            let events: Vec<RunEvent> = events
                .into_iter()
                .map(|(id, kind)| turn_event(id, kind))
                .collect();
            let candidate = landing_candidate(&run, RunStatus::AwaitingIntegration, &events);
            assert_eq!(candidate.is_some(), picked, "{events:?}");
            if let Some(candidate) = candidate {
                assert_eq!(
                    candidate,
                    EndedRunWorktree {
                        run_id: run.run_id.clone(),
                        task_id: TaskId::new(4),
                        status: RunStatus::AwaitingIntegration,
                        task_status: TaskStatus::InProgress,
                        worktree: run.worktree.clone(),
                        branch: run.branch.clone(),
                        cleanup: WorktreeCleanup::Landing,
                    }
                );
                assert!(for_room_only(candidate.cleanup));
                assert!(!runner_goes(candidate.cleanup));
                let mut task = Request::default();
                task.add(Some(TaskId::new(4)), None);
                assert!(!task.wants(&candidate));
            }
        }
        let (why, payload) = build_outputs_record(
            WorktreeCleanup::Landing,
            &["/runs/l/worktree/target".to_owned()],
            &Size::of(3),
        );
        assert_eq!(why, ", waiting for the landing slot, for disk space");
        assert_eq!(
            payload,
            json!({"paths": ["/runs/l/worktree/target"], "bytes": 3, "by": "supervisor", "reason": "disk_space"})
        );
    }

    /// The job reserves a run waiting for the landing slot only once it
    /// reaches it, and only while the loop still offers it: one the loop
    /// moved on meanwhile (its landing began) is passed untouched, and the
    /// one reached is reserved while it is cleaned, not after. Neither is
    /// reserved before the job reaches it.
    #[test]
    fn a_landing_run_is_reserved_only_once_reached_and_still_offered() {
        let landing = |run: &str| EndedRunWorktree {
            cleanup: WorktreeCleanup::Landing,
            status: RunStatus::AwaitingIntegration,
            ..candidate(run, TaskStatus::InProgress)
        };
        let (back, front) = (landing("back"), landing("front"));
        let cleaning = Mutex::new(Cleaning {
            offered: vec![back.run_id.clone(), front.run_id.clone()],
            ..Cleaning::default()
        });
        let stop = AtomicBool::new(false);
        let mut seen = Vec::new();
        pass_candidates(
            &cleaning,
            &stop,
            vec![back.clone(), front.clone()],
            |picked| Ok(Some(picked.clone())),
            |picked, _| {
                let mut cleaning = lock_cleaning(&cleaning);
                seen.push((
                    picked.run_id.clone(),
                    cleaning.reserved(&back.run_id),
                    cleaning.reserved(&front.run_id),
                ));
                assert!(cleaning.landing_reached());
                // The front run's landing begins while the job cleans the
                // back one.
                cleaning.withdraw(&front.run_id);
                false
            },
        );
        assert_eq!(seen, [(back.run_id.clone(), true, false)]);
        let cleaning = lock_cleaning(&cleaning);
        assert!(!cleaning.reserved(&back.run_id) && !cleaning.reserved(&front.run_id));
        assert!(cleaning.offered.is_empty());
        assert!(!cleaning.landing_reached());
    }

    /// A landing recheck that starts after the job was offered runs
    /// waiting for the landing slot withdraws those it has not reached:
    /// the job passes them untouched. One reached and reserved holds the
    /// recheck back until the job passed it.
    #[test]
    fn a_landing_recheck_and_the_removal_of_a_landing_runs_outputs_exclude_each_other() {
        let landing = |run: &str| EndedRunWorktree {
            cleanup: WorktreeCleanup::Landing,
            ..candidate(run, TaskStatus::InProgress)
        };
        let (first, second) = (landing("first"), landing("second"));
        let cleaning = Mutex::new(Cleaning {
            offered: vec![first.run_id.clone(), second.run_id.clone()],
            ..Cleaning::default()
        });
        assert!(!lock_cleaning(&cleaning).landing_reached());
        let stop = AtomicBool::new(false);
        let mut reached = Vec::new();
        pass_candidates(
            &cleaning,
            &stop,
            vec![first.clone()],
            |picked| Ok(Some(picked.clone())),
            |_, _| {
                // The recheck waits while its outputs go.
                reached.push(lock_cleaning(&cleaning).landing_reached());
                false
            },
        );
        assert_eq!(reached, [true]);
        let mut cleaning = lock_cleaning(&cleaning);
        assert!(!cleaning.landing_reached());
        // The recheck starts: the second one, not reached yet, is passed.
        cleaning.withdraw_all();
        assert!(!cleaning.reach(&second));
        assert!(!cleaning.reserved(&second.run_id));
    }

    /// A stop or a handoff withdraws the runs waiting for the landing slot
    /// a job for room has not reached: the job, which goes on to its last
    /// candidate, passes them untouched, and the drain lands or hands them
    /// over. The rest of a cleanup for room started after takes none of
    /// them; once a withdrawn handoff takes requests again, one does.
    #[test]
    fn ending_the_cleanup_withdraws_the_landing_runs_not_reached() {
        let landing = EndedRunWorktree {
            cleanup: WorktreeCleanup::Landing,
            ..candidate("l", TaskStatus::InProgress)
        };
        let for_room = Request {
            idle: true,
            ..Request::default()
        };
        let mut watch = CleanupWatch::default();
        assert!(watch.takes_landing(&for_room));
        assert!(!watch.takes_landing(&Request::default()));
        let mut sweep = Request::default();
        sweep.add(None, None);
        assert!(watch.takes_landing(&sweep));
        lock_cleaning(&watch.cleaning)
            .offered
            .push(landing.run_id.clone());
        watch.end();
        assert!(!watch.takes_landing(&for_room));
        assert!(!watch.takes_landing(&sweep));
        {
            let mut cleaning = lock_cleaning(&watch.cleaning);
            assert!(!cleaning.reach(&landing));
            assert!(!cleaning.reserved(&landing.run_id));
        }
        assert!(watch.resume());
        assert!(watch.takes_landing(&for_room));
    }

    /// A job that ended before it passed its candidates (it panicked
    /// cleaning a run waiting for the landing slot) leaves none reserved,
    /// offered or reached: the run lands and a landing recheck starts
    /// again.
    #[test]
    fn a_job_ended_midway_leaves_no_landing_run_reached() {
        let landing = EndedRunWorktree {
            cleanup: WorktreeCleanup::Landing,
            ..candidate("l", TaskStatus::InProgress)
        };
        let other = RunId::new("o").unwrap();
        let mut cleaning = Cleaning {
            offered: vec![landing.run_id.clone(), other.clone()],
            ..Cleaning::default()
        };
        assert!(cleaning.reach(&landing));
        assert!(cleaning.landing_reached());
        cleaning.release();
        assert!(!cleaning.landing_reached());
        assert!(!cleaning.reserved(&landing.run_id));
        assert!(cleaning.offered.is_empty());
        assert!(!cleaning.reach(&EndedRunWorktree {
            run_id: other,
            ..landing
        }));
    }

    /// A job not for room removes only the stale caches of the runs whose
    /// build outputs only a cleanup for room removes (nobody's run waiting
    /// for no answer, a waiting run the slots offer, one waiting for the
    /// landing slot), and a job for room their build outputs whole; an
    /// ended run, and one waiting with no lease for an answer, lose them
    /// whole on any job, so they have no stale caches to lose apart.
    #[test]
    fn only_a_job_not_for_room_removes_the_stale_caches_of_a_run_left_in_the_middle_alone() {
        let ask = AskId::new(7);
        for cleanup in [
            WorktreeCleanup::Idle,
            WorktreeCleanup::Waiting(ask),
            WorktreeCleanup::Landing,
        ] {
            assert!(stale_only(false, cleanup), "{cleanup:?}");
            assert!(!stale_only(true, cleanup), "{cleanup:?}");
        }
        for cleanup in [WorktreeCleanup::Ended, WorktreeCleanup::AwaitingAnswer(ask)] {
            assert!(!stale_only(false, cleanup) && !stale_only(true, cleanup));
        }
    }

    /// `incremental_sessions_removed` names the directories, how many
    /// entries went, their bytes and what the run waits for, with the ask
    /// of a run waiting for an answer.
    #[test]
    fn the_stale_caches_removed_are_recorded_with_what_the_run_waits_for() {
        let paths = ["/runs/r/worktree/target/debug/incremental/dagq-1".to_owned()];
        let record = |cleanup| stale_entries_record(cleanup, &paths, 4, &Size::of(9));
        let payload = |reason: &str| json!({"paths": paths, "entries": 4, "bytes": 9, "by": "supervisor", "reason": reason});
        assert_eq!(
            record(WorktreeCleanup::Idle),
            (String::new(), payload("idle"))
        );
        assert_eq!(
            record(WorktreeCleanup::Landing),
            (
                ", waiting for the landing slot".to_owned(),
                payload("awaiting_landing_slot")
            )
        );
        let mut waiting = payload("awaiting_answer");
        waiting["ask_id"] = json!(8);
        assert_eq!(
            record(WorktreeCleanup::Waiting(AskId::new(8))),
            (", waiting for the answer to ask 8".to_owned(), waiting)
        );
        let (_, unmeasured) = stale_entries_record(
            WorktreeCleanup::Idle,
            &paths,
            1,
            &Size::unmeasured("measure: busy".to_owned()),
        );
        assert_eq!(unmeasured["unmeasured"], json!(["measure: busy"]));
    }

    /// A run whose stale caches a job removed is left out of the next
    /// jobs not for room while it is listed or offered as it was then;
    /// once it is not (a lease hid it, a turn of it ran, it moved on) it is
    /// forgotten, and a candidate again once back, so its next build's
    /// stale caches go too. The loop's lease, and its moving a run waiting
    /// for the landing slot on, forget it. Other candidates
    /// are never left out.
    #[test]
    fn a_pruned_run_is_left_out_only_until_it_may_have_built_again() {
        let idle = EndedRunWorktree {
            cleanup: WorktreeCleanup::Idle,
            status: RunStatus::NeedsSession,
            ..candidate("idle", TaskStatus::InProgress)
        };
        let landing = EndedRunWorktree {
            cleanup: WorktreeCleanup::Landing,
            status: RunStatus::AwaitingIntegration,
            ..candidate("landing", TaskStatus::InProgress)
        };
        let ended = candidate("ended", TaskStatus::InProgress);
        let turn =
            |id: i64| move |run: &RunId| (run.as_str() == "landing").then_some(EventId::new(id));
        let pruned_as = |worktree: &EndedRunWorktree, turn: Option<i64>| PrunedAs {
            worktree: worktree.clone(),
            turn: turn.map(EventId::new),
        };
        let mut pruned = HashMap::from([
            (idle.run_id.clone(), pruned_as(&idle, None)),
            (landing.run_id.clone(), pruned_as(&landing, Some(3))),
            (ended.run_id.clone(), pruned_as(&ended, None)),
        ]);
        let run = |list: &[EndedRunWorktree]| -> Vec<String> {
            list.iter().map(|w| w.run_id.as_str().to_owned()).collect()
        };
        // As they were: the pruned left out, the ended run kept.
        let (mut listed, mut offered) = (vec![idle.clone(), ended.clone()], vec![landing.clone()]);
        leave_pruned(&mut pruned, [&mut listed, &mut offered], turn(3));
        assert_eq!(
            (run(&listed), run(&offered)),
            (vec!["ended".to_owned()], vec![])
        );
        assert_eq!(pruned.len(), 2);
        // A turn of the landing run ran, and the idle run is not listed
        // (leased): both forgotten, the landing one a candidate now.
        let (mut listed, mut offered) = (vec![ended.clone()], vec![landing.clone()]);
        leave_pruned(&mut pruned, [&mut listed, &mut offered], turn(9));
        assert_eq!(run(&offered), ["landing"]);
        assert!(pruned.is_empty());
        // Back as it was: a candidate again.
        let (mut listed, mut offered) = (vec![idle.clone()], Vec::new());
        leave_pruned(&mut pruned, [&mut listed, &mut offered], turn(9));
        assert_eq!(run(&listed), ["idle"]);
        // The loop's lease forgets a pruned run.
        let mut cleaning = Cleaning {
            pruned: HashMap::from([(idle.run_id.clone(), pruned_as(&idle, None))]),
            ..Cleaning::default()
        };
        assert!(cleaning.may_lease(&idle.run_id));
        assert!(cleaning.pruned.is_empty());
        // The loop moving a run waiting for the landing slot on (its e2e,
        // which builds in its worktree) forgets it.
        cleaning
            .pruned
            .insert(landing.run_id.clone(), pruned_as(&landing, Some(3)));
        cleaning.withdraw(&landing.run_id);
        assert!(cleaning.pruned.is_empty());
    }

    /// A tree of directories in memory: the names each directory lists,
    /// or the error reading it fails with; a name no directory is a file.
    /// Removing a path under `stuck` fails, the rest goes; removing one
    /// under `halfway` takes what is in it and then fails. A path in
    /// `links` is a link, which the port does not follow: ELOOP, as on
    /// macOS. Each name measures 100 bytes.
    #[derive(Default)]
    struct TreeFiles {
        dirs: Mutex<HashMap<PathBuf, std::result::Result<Vec<String>, std::io::ErrorKind>>>,
        stuck: Mutex<Vec<PathBuf>>,
        halfway: Mutex<Vec<PathBuf>>,
        links: Vec<PathBuf>,
    }

    impl TreeFiles {
        fn with(dirs: &[(&str, &[&str])]) -> Self {
            let files = Self::default();
            for (dir, names) in dirs {
                files.list(dir, Ok(names));
            }
            files
        }
        fn list(&self, dir: &str, names: std::result::Result<&[&str], std::io::ErrorKind>) {
            let names = names.map(|names| names.iter().map(|name| (*name).to_owned()).collect());
            self.dirs.lock().unwrap().insert(PathBuf::from(dir), names);
        }
        fn listed(&self, dir: &str) -> Vec<String> {
            self.dirs.lock().unwrap()[Path::new(dir)].clone().unwrap()
        }
        fn remove(&self, path: &Path) -> std::io::Result<()> {
            if self.stuck.lock().unwrap().iter().any(|stuck| stuck == path) {
                return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
            }
            let mut dirs = self.dirs.lock().unwrap();
            if self.halfway.lock().unwrap().iter().any(|half| half == path) {
                dirs.retain(|dir, _| dir == path || !dir.starts_with(path));
                dirs.insert(path.to_path_buf(), Ok(Vec::new()));
                return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
            }
            dirs.retain(|dir, _| !dir.starts_with(path));
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if let Some(Ok(names)) = dirs.get_mut(path.parent().unwrap()) {
                names.retain(|listed| *listed != name);
            }
            Ok(())
        }
    }

    impl RunFiles for TreeFiles {
        fn create_dir_all(&self, _: &Path) -> std::io::Result<()> {
            unimplemented!()
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
        fn modified(&self, _: &Path) -> std::io::Result<std::time::SystemTime> {
            unimplemented!()
        }
        fn read_stamped(&self, _: &Path) -> Result<Option<(std::time::SystemTime, Vec<u8>)>> {
            unimplemented!()
        }
        fn is_file(&self, _: &Path) -> bool {
            unimplemented!()
        }
        fn is_dir(&self, path: &Path) -> bool {
            self.dirs.lock().unwrap().contains_key(path)
        }
        fn exists(&self, _: &Path) -> bool {
            unimplemented!()
        }
        fn read_dir(&self, dir: &Path) -> std::io::Result<Vec<PathBuf>> {
            match self.dirs.lock().unwrap().get(dir) {
                Some(Ok(names)) => Ok(names.iter().map(|name| dir.join(name)).collect()),
                Some(Err(kind)) => Err(std::io::Error::from(*kind)),
                None => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            }
        }
        fn rename(&self, _: &Path, _: &Path) -> std::io::Result<()> {
            unimplemented!()
        }
        fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            self.remove(path)
        }
        fn tree_size(&self, dir: &Path) -> std::io::Result<Option<u64>> {
            let dirs = self.dirs.lock().unwrap();
            let names: usize = dirs
                .iter()
                .filter(|(listed, _)| listed.starts_with(dir))
                .map(|(_, names)| names.as_ref().map_or(0, Vec::len))
                .sum();
            Ok(dirs.contains_key(dir).then_some(100 * names as u64))
        }
        fn entry_kind(&self, path: &Path) -> std::io::Result<EntryKind> {
            if path
                .ancestors()
                .any(|up| self.links.iter().any(|link| link == up))
            {
                return Err(std::io::Error::from_raw_os_error(libc::ELOOP));
            }
            let dirs = self.dirs.lock().unwrap();
            if dirs.contains_key(path) {
                return Ok(EntryKind::Dir);
            }
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            match dirs.get(path.parent().unwrap()) {
                Some(Ok(names)) if names.contains(&name) => Ok(EntryKind::File),
                Some(Err(kind)) => Err(std::io::Error::from(*kind)),
                _ => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            }
        }
        fn remove_dir_all(&self, dir: &Path) -> std::io::Result<()> {
            self.remove(dir)
        }
        fn append_line(&self, _: &Path, _: &str) -> std::io::Result<()> {
            unimplemented!()
        }
        fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
            Ok(path.to_path_buf())
        }
        fn write_fenced(&self, _: &Path, _: &str, _: &str, _: &Path) -> Result<()> {
            unimplemented!()
        }
        fn now(&self) -> std::time::SystemTime {
            unimplemented!()
        }
    }

    const INCREMENTAL: &str = "/runs/idle/worktree/target/debug/incremental";

    /// A worktree whose `target` has two crates of incremental sessions,
    /// each with a finished session older than its newest.
    fn two_crates() -> TreeFiles {
        TreeFiles::with(&[
            ("/runs/idle/worktree", &["target"]),
            ("/runs/idle/worktree/target", &["debug"]),
            ("/runs/idle/worktree/target/debug", &["incremental"]),
            (INCREMENTAL, &["a-1", "b-2"]),
            (
                "/runs/idle/worktree/target/debug/incremental/a-1",
                &["s-00a-r-x", "s-00a-r.lock", "s-00b-r-y", "s-00b-r.lock"],
            ),
            (
                "/runs/idle/worktree/target/debug/incremental/a-1/s-00a-r-x",
                &["q"],
            ),
            (
                "/runs/idle/worktree/target/debug/incremental/a-1/s-00b-r-y",
                &["q"],
            ),
            (
                "/runs/idle/worktree/target/debug/incremental/b-2",
                &["s-00a-r-x", "s-00a-r.lock", "s-00b-r-y", "s-00b-r.lock"],
            ),
            (
                "/runs/idle/worktree/target/debug/incremental/b-2/s-00a-r-x",
                &["q"],
            ),
            (
                "/runs/idle/worktree/target/debug/incremental/b-2/s-00b-r-y",
                &["q"],
            ),
        ])
    }

    /// A removal that fails after another went returns what went (its
    /// entries and the bytes measured before and after) beside the
    /// failure, and goes on with the rest: the lock of the crate whose
    /// session could not go still goes.
    #[test]
    fn the_stale_caches_that_went_are_kept_beside_a_removal_that_failed() {
        let files = two_crates();
        let stuck = Path::new(INCREMENTAL).join("b-2/s-00a-r-x");
        files.stuck.lock().unwrap().push(stuck.clone());
        let removal = remove_stale_entries(
            &files,
            Path::new("/runs/idle/worktree"),
            &Toolchains::this_repository(),
            &|_| Ok(false),
        );
        assert_eq!(
            removal.paths,
            [format!("{INCREMENTAL}/a-1"), format!("{INCREMENTAL}/b-2")]
        );
        assert_eq!(removal.entries, 3);
        // a-1 lost a session (a name and its file) and a lock, b-2 a lock.
        assert_eq!(removal.size, Size::of(300 + 100));
        assert_eq!(
            files.listed(&format!("{INCREMENTAL}/a-1")),
            ["s-00b-r-y", "s-00b-r.lock"]
        );
        assert_eq!(
            files.listed(&format!("{INCREMENTAL}/b-2")),
            ["s-00a-r-x", "s-00b-r-y", "s-00b-r.lock"]
        );
        let failed: Vec<(&str, String)> = removal
            .failed
            .iter()
            .map(|(path, error)| (path.as_str(), format!("{error:#}")))
            .collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, stuck.to_string_lossy());
        assert!(failed[0].1.starts_with("remove "), "{failed:?}");
    }

    /// A session whose directory loses its files and then fails to go
    /// frees bytes no later sweep finds: they are recorded although no
    /// entry of its crate went whole, whether it has no lock or its lock
    /// fails to go too, and each failure is returned beside them; a crate
    /// that lost nothing is not named.
    #[test]
    fn the_bytes_of_a_session_that_went_halfway_are_recorded_beside_its_failure() {
        let session = Path::new(INCREMENTAL).join("b-2/s-00a-r-x");
        let lock = Path::new(INCREMENTAL).join("b-2/s-00a-r.lock");
        for with_lock in [false, true] {
            let files = two_crates();
            // a-1 has nothing to remove: its session and lock are stuck.
            files.stuck.lock().unwrap().extend([
                Path::new(INCREMENTAL).join("a-1/s-00a-r-x"),
                Path::new(INCREMENTAL).join("a-1/s-00a-r.lock"),
            ]);
            files.halfway.lock().unwrap().push(session.clone());
            if with_lock {
                files.stuck.lock().unwrap().push(lock.clone());
            } else {
                files.list(
                    &format!("{INCREMENTAL}/b-2"),
                    Ok(&["s-00a-r-x", "s-00b-r-y", "s-00b-r.lock"]),
                );
            }
            let removal = remove_stale_entries(
                &files,
                Path::new("/runs/idle/worktree"),
                &Toolchains::this_repository(),
                &|_| Ok(false),
            );
            assert_eq!(removal.paths, [format!("{INCREMENTAL}/b-2")], "{with_lock}");
            assert_eq!(removal.entries, 0);
            // The file the session's directory held.
            assert_eq!(removal.size, Size::of(100));
            let mut failed: Vec<&str> = removal.failed.iter().map(|(p, _)| p.as_str()).collect();
            failed.sort_unstable();
            let mut expected = vec![
                format!("{INCREMENTAL}/a-1/s-00a-r-x"),
                format!("{INCREMENTAL}/a-1/s-00a-r.lock"),
                session.to_string_lossy().into_owned(),
            ];
            if with_lock {
                expected.push(lock.to_string_lossy().into_owned());
            }
            expected.sort_unstable();
            assert_eq!(failed, expected);
        }
    }

    /// A run whose stale caches went only halfway records what that freed
    /// as `incremental_sessions_removed` with no entry, and the failure as
    /// `cleanup_failed` apart; one that lost nothing records the failure
    /// alone.
    #[test]
    fn a_run_whose_stale_caches_went_halfway_records_the_bytes_and_the_failure() {
        let pass = |halfway: bool| {
            let files = Arc::new(two_crates());
            files.list(INCREMENTAL, Ok(&["b-2"]));
            files.list(
                &format!("{INCREMENTAL}/b-2"),
                Ok(&["s-00a-r-x", "s-00b-r-y", "s-00b-r.lock"]),
            );
            let session = Path::new(INCREMENTAL).join("b-2/s-00a-r-x");
            if halfway {
                files.halfway.lock().unwrap().push(session);
            } else {
                files.stuck.lock().unwrap().push(session);
            }
            let repository = Arc::new(Removing::default());
            let ports = JobPorts {
                files,
                ..job_ports(CacheFiles::default(), &repository)
            };
            let idle = EndedRunWorktree {
                cleanup: WorktreeCleanup::Idle,
                status: RunStatus::NeedsSession,
                ..candidate("idle", TaskStatus::InProgress)
            };
            let mut outcomes = Vec::new();
            clean_candidate(&ports, &idle, &mut None, &mut false, &mut outcomes);
            outcomes
        };
        let outcomes = pass(true);
        assert!(
            matches!(
                outcomes.as_slice(),
                [
                    Outcome::StaleEntries { entries: 0, size, .. },
                    Outcome::Failed { what: "stale cache", .. },
                ] if *size == Size::of(100)
            ),
            "{}",
            outcomes.len()
        );
        let outcomes = pass(false);
        assert!(
            matches!(
                outcomes.as_slice(),
                [Outcome::Failed {
                    what: "stale cache",
                    ..
                }]
            ),
            "{}",
            outcomes.len()
        );
    }

    /// A directory that cannot be read is a failure, not a directory with
    /// nothing stale: the crates it hides are not reached. One that is not
    /// there, or a link in a directory's place, is nothing, and the others
    /// lose theirs.
    #[test]
    fn a_directory_that_cannot_be_read_is_a_failure_not_nothing_stale() {
        let files = two_crates();
        files.list(INCREMENTAL, Err(std::io::ErrorKind::PermissionDenied));
        let removal = remove_stale_entries(
            &files,
            Path::new("/runs/idle/worktree"),
            &Toolchains::this_repository(),
            &|_| Ok(false),
        );
        assert_eq!(removal.entries, 0);
        assert_eq!(removal.failed.len(), 1);
        assert_eq!(removal.failed[0].0, INCREMENTAL);
        // A Git that cannot be asked is a failure too.
        let removal = remove_stale_entries(
            &two_crates(),
            Path::new("/runs/idle/worktree"),
            &Toolchains::this_repository(),
            &|_| Err(anyhow::anyhow!("git failed")),
        );
        assert_eq!(removal.entries, 0);
        assert_eq!(removal.failed.len(), 1);
        // No incremental directory: nothing, and no failure.
        let none = TreeFiles::with(&[
            ("/runs/idle/worktree", &["target"]),
            ("/runs/idle/worktree/target", &["debug"]),
            ("/runs/idle/worktree/target/debug", &["deps"]),
            ("/runs/idle/worktree/target/debug/deps", &[]),
        ]);
        let removal = remove_stale_entries(
            &none,
            Path::new("/runs/idle/worktree"),
            &Toolchains::this_repository(),
            &|_| Ok(false),
        );
        assert_eq!((removal.entries, removal.failed.len()), (0, 0));
        // A link beside the profiles' directories: nothing, and the
        // crates of the others lose theirs.
        let linked = TreeFiles {
            links: vec![PathBuf::from("/runs/idle/worktree/target/elsewhere")],
            ..two_crates()
        };
        linked.list("/runs/idle/worktree/target", Ok(&["debug", "elsewhere"]));
        let removal = remove_stale_entries(
            &linked,
            Path::new("/runs/idle/worktree"),
            &Toolchains::this_repository(),
            &|_| Ok(false),
        );
        assert_eq!((removal.entries, removal.failed.len()), (4, 0));
    }

    /// A run whose stale caches could not all be read or removed records
    /// `cleanup_failed` and is not pruned, so the next sweep tries it
    /// again; once they can be, they go and it is pruned.
    #[test]
    fn a_run_whose_stale_caches_failed_is_tried_again_on_the_next_sweep() {
        let files = Arc::new(two_crates());
        files.list(INCREMENTAL, Err(std::io::ErrorKind::PermissionDenied));
        let repository = Arc::new(Removing::default());
        let ports = JobPorts {
            files: files.clone(),
            ..job_ports(CacheFiles::default(), &repository)
        };
        let idle = EndedRunWorktree {
            cleanup: WorktreeCleanup::Idle,
            status: RunStatus::NeedsSession,
            ..candidate("idle", TaskStatus::InProgress)
        };
        let pass = || {
            let mut outcomes = Vec::new();
            clean_candidate(&ports, &idle, &mut None, &mut false, &mut outcomes);
            outcomes
        };
        let outcomes = pass();
        assert!(
            matches!(
                outcomes.as_slice(),
                [Outcome::Failed { what: "stale cache", path, .. }] if path == INCREMENTAL
            ),
            "{}",
            outcomes.len()
        );
        assert!(lock_cleaning(&ports.cleaning).pruned.is_empty());
        files.list(INCREMENTAL, Ok(&["a-1", "b-2"]));
        let outcomes = pass();
        assert!(
            matches!(
                outcomes.as_slice(),
                [Outcome::StaleEntries { entries: 4, .. }]
            ),
            "{}",
            outcomes.len()
        );
        assert!(
            lock_cleaning(&ports.cleaning)
                .pruned
                .contains_key(&idle.run_id)
        );
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
                |picked| {
                    reads.push(picked.run_id.clone());
                    Ok(runs.iter().find(|w| w.run_id == picked.run_id).cloned())
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
            |picked| match picked.run_id.as_str() {
                "leased" => Ok(None),
                "moved" => Ok(Some(EndedRunWorktree {
                    task_status: TaskStatus::Canceled,
                    ..candidate("moved", TaskStatus::Completed)
                })),
                "unread" => {
                    stop.store(true, Ordering::SeqCst);
                    Err(anyhow::anyhow!("busy"))
                }
                _ => Ok(Some(picked.clone())),
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

    /// The admin dir a worktree's `.git` names is its own, so the repair may
    /// point it there again, when it points back at the worktree or holds
    /// the run's branch (a rebind moved the worktree); missing, or another
    /// worktree's that took the name, it is not. A `.git` naming none is
    /// left to the repair.
    #[test]
    fn only_an_admin_dir_pointing_back_or_on_the_runs_branch_is_the_worktrees_own() {
        let found = |points_back, head: &str| WorktreeAdmin::Found {
            points_back,
            head: head.to_owned(),
        };
        let branch = "dagq/r";
        assert!(owns_admin(&found(true, "ref: refs/heads/other"), branch));
        assert!(owns_admin(&found(false, "ref: refs/heads/dagq/r"), branch));
        assert!(owns_admin(
            &found(false, "ref: refs/heads/dagq/r"),
            "refs/heads/dagq/r"
        ));
        assert!(owns_admin(&WorktreeAdmin::Unreadable, branch));
        assert!(!owns_admin(&WorktreeAdmin::Missing, branch));
        assert!(!owns_admin(&found(false, "ref: refs/heads/other"), branch));
        assert!(!owns_admin(
            &found(false, "ref: refs/heads/dagq/r2"),
            branch
        ));
        assert!(!owns_admin(&found(false, &"a".repeat(40)), branch));
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
}
