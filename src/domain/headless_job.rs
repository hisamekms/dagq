//! The processes of the supervisor's headless jobs (task 443): each job's
//! pid is recorded when it starts, so a supervisor that takes over after
//! the one that started it died can stop it before it starts its own, and
//! a job's timeout stops the processes the job started too.

use super::recovery::ProcessInfo;

/// `headless_jobs.kind` of a headless review of a run (ADR-0027).
pub const REVIEW: &str = "review";
/// `headless_jobs.kind` of a recovery job, of a run that ended (the
/// triage) or of a live session (ADR-0047 decisions 39 and 40).
pub const RECOVERY: &str = "recovery";
/// `headless_jobs.kind` of a plan review of a proposal (ADR-0041).
pub const PLAN_REVIEW: &str = "plan_review";
/// `headless_jobs.kind` of a goal review (ADR-0047 decision 43).
pub const GOAL_REVIEW: &str = "goal_review";

/// `headless_jobs.outcome` of a job that ended by itself (its exit read).
pub const ENDED: &str = "ended";
/// `headless_jobs.outcome` of a job its own supervisor stopped (its
/// timeout, or the supervisor stopped watching it).
pub const STOPPED: &str = "stopped";
/// `headless_jobs.outcome` of a job of a gone supervisor that another
/// supervisor stopped (`headless_job_stopped`).
pub const TAKEN_OVER: &str = "taken_over";
/// `headless_jobs.outcome` of a job of a gone supervisor whose process was
/// found gone already.
pub const GONE: &str = "gone";
/// `headless_jobs.outcome` of a job of a gone supervisor whose pid runs
/// another process now (or whose start could not be told): it is not
/// touched.
pub const NOT_THE_JOB: &str = "not_the_job";

/// What a supervisor that takes over does with the process of a job of a
/// gone supervisor, from what `ps` says of the pid now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Takeover {
    /// The pid runs no process: the row is only closed.
    Gone,
    /// The pid runs the job's process (the same start): it and its
    /// descendants are stopped.
    Stop,
    /// The pid runs another process, or the start of either could not be
    /// read: nothing is signalled.
    NotTheJob,
}

/// Judge the pid of a job: `recorded` is the process's start read when the
/// job started, `now` the start of the pid's process read now (`None` when
/// it could not be read), `alive` whether the pid runs.
pub fn takeover(alive: bool, recorded: Option<&str>, now: Option<&str>) -> Takeover {
    if !alive {
        return Takeover::Gone;
    }
    match (recorded, now) {
        (Some(recorded), Some(now)) if recorded == now => Takeover::Stop,
        _ => Takeover::NotTheJob,
    }
}

/// The descendants of `root` in `all` (children first, then theirs), not
/// `root` itself.
pub fn descendants(all: &[ProcessInfo], root: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for process in all {
            if process.ppid == parent
                && process.pid != root
                && process.pid > 1
                && !found.contains(&process.pid)
            {
                found.push(process.pid);
                frontier.push(process.pid);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, ppid: u32) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid,
            elapsed_secs: 0,
            command: String::new(),
            cwd: None,
            cpu_ms: None,
        }
    }

    #[test]
    fn descendants_follow_every_generation_and_leave_the_rest() {
        let all = [
            process(10, 1),
            process(11, 10),
            process(12, 11),
            process(13, 10),
            process(20, 1),
            process(21, 20),
        ];
        let mut found = descendants(&all, 10);
        found.sort();
        assert_eq!(found, [11, 12, 13]);
        assert!(descendants(&all, 12).is_empty());
        assert!(descendants(&all, 99).is_empty());
    }

    #[test]
    fn a_cycle_in_the_listing_ends() {
        let all = [process(5, 6), process(6, 5)];
        assert_eq!(descendants(&all, 5), [6]);
    }

    #[test]
    fn only_the_same_process_is_stopped() {
        assert_eq!(takeover(false, Some("a"), None), Takeover::Gone);
        assert_eq!(takeover(true, Some("a"), Some("a")), Takeover::Stop);
        assert_eq!(takeover(true, Some("a"), Some("b")), Takeover::NotTheJob);
        assert_eq!(takeover(true, None, Some("b")), Takeover::NotTheJob);
        assert_eq!(takeover(true, Some("a"), None), Takeover::NotTheJob);
    }
}
