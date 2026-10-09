//! A run's health (its lease, its registered processes and its files) and
//! `recover` of an orphaned run (execution and landing).

use super::*;

/// Health of one run's lease as `status` and `doctor` report it.
#[derive(Debug, Clone, Serialize)]
pub struct LeaseHealth {
    pub pid: u32,
    pub alive: bool,
    pub heartbeat_at: i64,
    pub heartbeat_age_secs: i64,
    pub stale: bool,
}

/// Health of one registered wrapper/agent process. `alive` is only checked
/// while the wrapper has not reported an exit, because a dead PID may be reused.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessHealth {
    pub role: String,
    pub pid: u32,
    pub alive: Option<bool>,
    pub heartbeat_at: i64,
    pub heartbeat_age_secs: i64,
    pub heartbeat_stale: bool,
    pub exited_at: Option<i64>,
    pub exit_code: Option<i32>,
}

/// One unfinished run. `blockers` lists why `recover` would refuse it; an
/// empty list means it is recoverable now. Only this run's own lease and
/// processes count; other runs never block it.
#[derive(Debug, Clone, Serialize)]
pub struct RunHealth {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub status: RunStatus,
    pub workspace_id: Option<String>,
    pub worktree_path: Option<String>,
    pub worktree_exists: Option<bool>,
    pub run_dir: Option<String>,
    pub run_dir_exists: Option<bool>,
    pub receipt_exists: Option<bool>,
    pub last_error: Option<String>,
    pub lease: Option<LeaseHealth>,
    pub processes: Vec<ProcessHealth>,
    pub blockers: Vec<String>,
    pub recoverable: bool,
    /// Its phase, since when and its slot ([`Progress`]); `doctor` adds it,
    /// `recover`'s report has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
}

impl RunHealth {
    /// The run in `doctor`'s default output: whether it can be recovered and
    /// where it is, with `blockers` counted (`blocker_count`) and the lease
    /// reduced to `lease_stale` (null without a lease).
    pub fn summary(&self) -> Value {
        json!({
            "run_id": self.run_id,
            "task_id": self.task_id,
            "status": self.status,
            "lease_pid": self.lease.as_ref().map(|lease| lease.pid),
            "lease_stale": self.lease.as_ref().map(|lease| lease.stale),
            "recoverable": self.recoverable,
            "blocker_count": self.blockers.len(),
            "workspace_id": self.workspace_id,
            "worktree_path": self.worktree_path,
            "progress": self.progress,
        })
    }
}

/// Whether `recover` takes a run in `status`, `leased` or not: an
/// unfinished run, or one awaiting integration that is still leased (its
/// supervisor died during the review or the landing, task 236).
pub fn recover_takes(status: RunStatus, leased: bool) -> bool {
    matches!(
        status,
        RunStatus::Claimed
            | RunStatus::Starting
            | RunStatus::Running
            | RunStatus::Validating
            | RunStatus::Integrating
    ) || (status == RunStatus::AwaitingIntegration && leased)
}

/// The health of `run` at `now`: a status `recover` does not take, a live
/// process of the run, a fresh lease or a live lease holder blocks its
/// recovery.
pub fn run_health(
    run: &TaskRun,
    processes: &[RunProcess],
    lease: Option<LeaseHealth>,
    now: i64,
    control: &dyn ProcessControl,
    files: &dyn RunFiles,
) -> RunHealth {
    let mut blockers = Vec::new();
    if !recover_takes(run.status(), lease.is_some()) {
        blockers.push(format!(
            "run is {}{}; recover takes only unfinished runs or a leased run awaiting integration",
            run.status().as_str(),
            if lease.is_some() {
                ""
            } else {
                " without a lease"
            }
        ));
    }
    let processes: Vec<ProcessHealth> = processes
        .iter()
        .map(|process| {
            let age = now - process.heartbeat_at;
            let alive = process
                .exited_at
                .is_none()
                .then(|| control.alive(process.pid));
            if alive == Some(true) {
                blockers.push(format!("{} pid {} is alive", process.role, process.pid));
            }
            ProcessHealth {
                role: process.role.clone(),
                pid: process.pid,
                alive,
                heartbeat_at: process.heartbeat_at,
                heartbeat_age_secs: age,
                heartbeat_stale: process.exited_at.is_none() && age > HEARTBEAT_TIMEOUT_SECS,
                exited_at: process.exited_at,
                exit_code: process.exit_code,
            }
        })
        .collect();
    if let Some(lease) = &lease {
        if !lease.stale {
            blockers.push(format!(
                "lease heartbeat is {}s old (limit {HEARTBEAT_TIMEOUT_SECS}s)",
                lease.heartbeat_age_secs
            ));
        }
        if lease.alive {
            blockers.push(format!("supervisor pid {} is alive", lease.pid));
        }
    }
    let exists = |path: Option<&str>| path.map(|p| files.exists(Path::new(p)));
    RunHealth {
        run_id: run.id().clone(),
        task_id: run.task_id(),
        status: run.status(),
        workspace_id: run.workspace_id().map(str::to_owned),
        worktree_path: run.worktree_path().map(str::to_owned),
        worktree_exists: exists(run.worktree_path()),
        run_dir: run.run_dir().map(str::to_owned),
        run_dir_exists: exists(run.run_dir()),
        receipt_exists: exists(run.receipt_path()),
        last_error: run.last_error().map(str::to_owned),
        lease,
        processes,
        recoverable: blockers.is_empty(),
        blockers,
        progress: None,
    }
}

/// A run's lease as its `progress` reads it: a stale lease or a dead
/// holder keeps the slot but shows no step going on.
pub(super) fn progress_lease(lease: Option<&LeaseHealth>) -> Lease {
    match lease {
        None => Lease::None,
        Some(lease) if lease.stale || !lease.alive => Lease::Stale,
        Some(_) => Lease::Live,
    }
}

/// The runs `status` and `doctor` list (goal 98), each once, oldest first:
/// every unfinished run (`claimed` to `integrating`, leased or not, as
/// before), the latest run of each `in_progress` task that awaits
/// integration or a session (under review, revise, resume, its e2e, its
/// `/exit` or its landing, or waiting for the supervisor or a person), and
/// every run a lease holds (the runs in `supervisors[].run_ids`: a failed
/// or interrupted run under recovery, a run that ended and is being
/// cleaned up). A finished or cancelled run nobody leases and an earlier
/// attempt of a retried task are history and are left out. The selection
/// is the listing's own: the automatic recovery, adoption and `stats` keep
/// [`RunReads::active_runs`].
pub(super) fn listed_runs(
    queue: &(impl RunLog + ?Sized),
    leases: &[RunLease],
) -> Result<Vec<TaskRun>> {
    let mut runs = queue.active_runs()?;
    let listed = |runs: &[TaskRun], id: &RunId| runs.iter().any(|run| run.id() == id);
    for run in queue.latest_runs_in_progress()? {
        if matches!(
            run.status(),
            RunStatus::AwaitingIntegration | RunStatus::NeedsSession
        ) && !listed(&runs, run.id())
        {
            runs.push(run);
        }
    }
    for lease in leases {
        if !listed(&runs, &lease.run_id) {
            runs.push(queue.run(&lease.run_id)?);
        }
    }
    runs.sort_by(|a, b| a.created_at().cmp(b.created_at()));
    Ok(runs)
}

/// Mark an orphaned run `interrupted` (or a run whose `integrate` process
/// died `awaiting_integration` again) and drop its lease, after checking that
/// nothing registered for it is still alive. An `awaiting_integration` run
/// that a dead supervisor still leases (it died during the review or the
/// landing) keeps its
/// status and loses the stale lease, so that it can be integrated (task
/// 236). Never reruns, never deletes the worktree or workspace, leaves the
/// task `in_progress`, and does not touch any other run.
pub fn recover(
    queue: &mut (impl RunCoordination + RunLog + RunRecovery + ?Sized),
    control: &dyn ProcessControl,
    files: &dyn RunFiles,
    clock: &dyn Clock,
    id: &RunId,
) -> Result<Value> {
    let run = queue.run(id)?;
    let lease = queue.run_lease(id)?;
    ensure!(
        recover_takes(run.status(), lease.is_some()),
        "run {id} is {}; only unfinished runs, or a run awaiting integration that is still leased, can be recovered",
        run.status().as_str()
    );
    let now = clock.now();
    let lease = lease.map(|l| lease_health(&l, now, control));
    let processes = queue.processes(run.id())?;
    let health = run_health(&run, &processes, lease, now, control, files);
    ensure!(
        health.recoverable,
        "refusing to recover run {id}: {}",
        health.blockers.join("; ")
    );
    let report = json!({"run": health});
    let run = queue.recover_run(run.id(), processes.len(), report)?;
    Ok(json!({"outcome": "recovered", "run": run}))
}

/// The health of one lease at `now`.
pub fn lease_health(lease: &RunLease, now: i64, control: &dyn ProcessControl) -> LeaseHealth {
    let age = now - lease.heartbeat_at;
    LeaseHealth {
        pid: lease.pid,
        alive: control.alive(lease.pid),
        heartbeat_at: lease.heartbeat_at,
        heartbeat_age_secs: age,
        stale: age > HEARTBEAT_TIMEOUT_SECS,
    }
}

/// Whether a lease no longer has a working process behind it: its pid is
/// dead or its heartbeat is older than `HEARTBEAT_TIMEOUT_SECS`.
pub(super) fn lease_is_stale(lease: &RunLease, now: i64, control: &dyn ProcessControl) -> bool {
    !control.alive(lease.pid) || now - lease.heartbeat_at > HEARTBEAT_TIMEOUT_SECS
}
