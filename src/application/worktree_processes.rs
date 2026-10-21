//! Stopping what runs from under a run's worktree before the worktree is
//! removed: by the supervisor's cleanup of an ended run and by the landing
//! of `integrate`, so no process writes into the directory while Git and
//! the runtime remove it.

use std::{path::Path, thread, time::Duration};

use anyhow::Result;
use serde_json::{Value, json};
use tracing::warn;

use super::ProcessControl;
use crate::domain::disk::worktree_executables;

/// How often [`stop_worktree_processes`] looks whether what it sent a
/// SIGTERM ended, every [`WORKTREE_STOP_POLL`]: 3 seconds in all, the
/// grace the recovery job's `stop_processes` gives. Counted rather than
/// timed, so the wait reads no clock (L5).
pub(crate) const WORKTREE_STOP_POLLS: u32 = 60;
const WORKTREE_STOP_POLL: Duration = Duration::from_millis(50);

/// Stop the processes whose executable is under `worktree`
/// ([`worktree_executables`]: a test's child `dagq` left running, or what
/// a worker's turn left with `&`), SIGTERM and then SIGKILL once `polls`
/// looks ([`WORKTREE_STOP_POLLS`]) found one still running, so none writes
/// into the worktree again once it is removed (task 1590). The processes are listed only here, once a
/// worktree is about to go. Returns what was stopped: the pid, the
/// executable, and whether it took a SIGKILL.
pub(crate) fn stop_worktree_processes(
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

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
