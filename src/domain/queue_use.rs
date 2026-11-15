//! The shared, side-effect-free evidence of a queue still in use.

/// What uses a queue: supervisors (token, pid), the runs not finished (id, status) and the
/// wrappers not exited (run, pid).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct QueueUse {
    pub supervisors: Vec<(String, i64)>,
    pub runs: Vec<(String, String)>,
    pub wrappers: Vec<(String, i64)>,
}

/// The statuses of a run whose wrapper or supervisor uses the queue.
pub const RUNS_IN_USE: [&str; 5] = [
    "claimed",
    "starting",
    "running",
    "validating",
    "integrating",
];

/// Who of `found` uses the queue, liveness per `alive`: a supervisor or a
/// wrapper whose process lives, and every run not finished (its wrapper
/// may be on another host's view of the pids).
pub fn queue_users(found: &QueueUse, alive: &dyn Fn(u32) -> bool) -> Vec<String> {
    let lives = |pid: i64| u32::try_from(pid).is_ok_and(alive);
    let supervisors = found
        .supervisors
        .iter()
        .filter(|(_, pid)| lives(*pid))
        .map(|(token, pid)| format!("supervisor {token} (pid {pid})"));
    let runs = found
        .runs
        .iter()
        .filter(|(_, status)| RUNS_IN_USE.contains(&status.as_str()))
        .map(|(id, status)| format!("run {id} ({status})"));
    let wrappers = found
        .wrappers
        .iter()
        .filter(|(_, pid)| lives(*pid))
        .map(|(run, pid)| format!("wrapper of run {run} (pid {pid})"));
    supervisors.chain(runs).chain(wrappers).collect()
}
