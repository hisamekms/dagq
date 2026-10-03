//! Headless session wrappers started as background processes instead of in
//! cmux workspaces (ADR-t1404-1): the [`Cmux`](super::adapters::Cmux)
//! backend hands every call on a [`BackgroundHandle`] here. A wrapper is
//! started detached from the supervisor (a shell in a session of its own
//! starts it in the background and exits, so its parent becomes init), is
//! known by its pid and the start the system recorded for it, and is
//! stopped with signals: SIGTERM first, which the wrapper takes to stop
//! the turns it started (`stop_groups_on_exit_signals`), then SIGKILL to
//! its process group and to the groups of the processes it had started.

use std::{
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};

use crate::{application::ProcessControl, domain::background_wrapper::BackgroundHandle};

/// How long a wrapper sent SIGTERM is given to stop its turns and exit
/// before its groups are killed.
const TERM_GRACE: Duration = Duration::from_secs(3);
/// How long the processes sent SIGKILL are given to be gone.
const KILL_WAIT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(50);

/// Starts, finds and stops background wrappers through `processes`.
pub struct BackgroundWrappers<'a> {
    pub processes: &'a dyn ProcessControl,
}

impl BackgroundWrappers<'_> {
    /// Start the shell command `command` (one simple command, which the
    /// shell execs, so that the pid it reports is the command's) in `cwd`
    /// detached from this process, with `env` over this process's
    /// environment but cmux's (`CMUX_*`, which names the workspace of
    /// whoever started the supervisor) and dagq's own (`DAGQ_*`, which
    /// `env` sets for the session as a workspace's `--env` would), its
    /// stdin `/dev/null` and its output in `log`; the handle of the
    /// process. The log is in the run directory, which the worker writes:
    /// it is made a fresh file through a descriptor of its pinned directory
    /// ([`agent_dir::create_file`](super::agent_dir::create_file)), never
    /// following a link or opening a FIFO a worker left at its name, and
    /// the wrapper is given that descriptor, not the path.
    pub fn launch(
        &self,
        cwd: &Path,
        command: &str,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<String> {
        let output = super::agent_dir::create_file(log)
            .with_context(|| format!("create the wrapper's log {}", log.display()))?;
        // The log is the shell's stderr: the wrapper's stdout and stderr
        // are dups of it, and the shell's stdout carries only the pid.
        let script = format!("exec {command} </dev/null >&2 & echo $!");
        let mut shell = Command::new("/bin/sh");
        shell.arg("-c").arg(script).current_dir(cwd);
        for (key, _) in std::env::vars_os() {
            if key
                .to_str()
                .is_some_and(|key| key.starts_with("CMUX_") || key.starts_with("DAGQ_"))
            {
                shell.env_remove(key);
            }
        }
        shell.envs(env.iter().map(|(key, value)| (key, value)));
        shell
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(output));
        // The wrapper outlives this process's other children: it must not
        // hold a pipe another thread was making for one of them (its
        // close-on-exec set after it was made) open for hours.
        // SAFETY: sysconf(3) only reads a limit.
        let open_max = match unsafe { libc::sysconf(libc::_SC_OPEN_MAX) } {
            limit if limit > 3 => limit.min(65_536) as libc::c_int,
            _ => 1024,
        };
        // SAFETY: setsid(2) and fcntl(2) are async-signal-safe and touch no
        // memory.
        unsafe {
            shell.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                for fd in 3..open_max {
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                }
                Ok(())
            });
        }
        let output = shell.output().context("start the background wrapper")?;
        ensure!(
            output.status.success(),
            "the shell that starts the background wrapper failed ({}); see {}",
            output.status,
            log.display()
        );
        let pid: u32 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .context("the shell did not print the background wrapper's pid")?;
        let start = self.processes.start_identity(pid).with_context(|| {
            format!(
                "the background wrapper {pid} exited as soon as it started; see {}",
                log.display()
            )
        })?;
        Ok(BackgroundHandle::new(pid, &start).to_string())
    }

    /// Whether the wrapper `handle` names still runs: its pid shows the
    /// start it was recorded with.
    pub fn alive(&self, handle: &BackgroundHandle) -> bool {
        handle.is(
            handle.pid,
            self.processes.start_identity(handle.pid).as_deref(),
        )
    }

    /// Stop the wrapper `handle` names and what it started; nothing for a
    /// wrapper that is gone. The processes it started are listed first,
    /// since they are init's once it dies, and each is killed only while
    /// its pid shows the start it was listed with.
    pub fn stop(&self, handle: &BackgroundHandle) -> Result<()> {
        if !self.alive(handle) {
            return Ok(());
        }
        let started: Vec<(u32, Option<String>)> = self
            .processes
            .descendants(handle.pid)
            .into_iter()
            .map(|pid| (pid, self.processes.start_identity(pid)))
            .collect();
        signal(handle.pid as i32, libc::SIGTERM)?;
        if !self.gone_within(handle, TERM_GRACE) {
            // The wrapper leads its group (it calls setsid(2) as it starts),
            // or it is in the group of the shell that started it.
            let _ = signal(-(handle.pid as i32), libc::SIGKILL);
            if self.alive(handle) {
                signal(handle.pid as i32, libc::SIGKILL)?;
            }
        }
        for (pid, start) in &started {
            if start.is_some() && self.processes.start_identity(*pid) == *start {
                // A turn leads a group of its own; another process may not.
                let _ = signal(-(*pid as i32), libc::SIGKILL);
                let _ = signal(*pid as i32, libc::SIGKILL);
            }
        }
        if !self.gone_within(handle, KILL_WAIT) {
            bail!(
                "the background wrapper {} is still running after SIGKILL",
                handle.pid
            );
        }
        Ok(())
    }

    fn gone_within(&self, handle: &BackgroundHandle, limit: Duration) -> bool {
        let started = Instant::now();
        loop {
            if !self.alive(handle) {
                return true;
            }
            if started.elapsed() >= limit {
                return false;
            }
            thread::sleep(POLL);
        }
    }
}

/// Send `signal` to `pid` (a negative one names a process group); one
/// that is gone is no error.
fn signal(pid: i32, signal: libc::c_int) -> Result<()> {
    // SAFETY: kill(2) takes no pointer.
    if unsafe { libc::kill(pid, signal) } == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).with_context(|| format!("signal {signal} to {pid}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::naming::shell_quote;
    use crate::infrastructure::adapters::SystemProcesses;
    use std::{fs, os::unix::fs::symlink, sync::mpsc};

    /// A run directory under a queue's `runs/` in `root`, where the worker
    /// writes and the wrapper's log is made without following anything.
    fn run_dir(root: &Path) -> std::path::PathBuf {
        let dir = root
            .join("runs")
            .join("3aa21145-c873-4cec-aee3-ee7f07f52e4a");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Launch `script` with its log at `log`, failing the test instead of
    /// hanging when the launch blocks (a FIFO opened for writing would).
    fn launched(cwd: &Path, script: &str, log: &Path) -> Result<BackgroundHandle> {
        let (cwd, script, log) = (cwd.to_owned(), sh(script), log.to_owned());
        let (sent, received) = mpsc::channel();
        thread::spawn(move || {
            let wrappers = BackgroundWrappers {
                processes: &SystemProcesses,
            };
            let _ = sent.send(
                wrappers
                    .launch(&cwd, &script, &[], &log)
                    .map(|id| BackgroundHandle::parse(&id).unwrap()),
            );
        });
        received
            .recv_timeout(Duration::from_secs(20))
            .expect("the launch returns without blocking")
    }

    /// Stop the wrapper `handle` names.
    fn stopped(handle: &BackgroundHandle) {
        BackgroundWrappers {
            processes: &SystemProcesses,
        }
        .stop(handle)
        .unwrap();
    }

    /// The wrapper's log is in the run directory the worker writes: a link
    /// a worker left at its name is replaced, never followed, so the file
    /// it points at outside is not written; a FIFO is replaced without
    /// being opened, so the launch does not block; and a run directory that
    /// is itself a link is refused, nothing written where it points.
    #[test]
    fn the_log_is_made_without_following_what_a_worker_left_at_its_name() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        fs::write(&outside, b"secret").unwrap();
        let dir = run_dir(root.path());
        let log = dir.join("session-resume-1.log");
        symlink(&outside, &log).unwrap();
        let handle = launched(root.path(), "echo linked; exec sleep 30", &log).unwrap();
        wait_for("the output in the log", || {
            fs::read_to_string(&log).is_ok_and(|text| text.contains("linked"))
        });
        assert!(!fs::symlink_metadata(&log).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&outside).unwrap(), b"secret");
        stopped(&handle);

        let fifo = dir.join("session.log");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: the path is NUL terminated.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let handle = launched(root.path(), "echo piped; exec sleep 30", &fifo).unwrap();
        wait_for("the output in the log", || {
            fs::read_to_string(&fifo).is_ok_and(|text| text.contains("piped"))
        });
        assert!(fs::symlink_metadata(&fifo).unwrap().file_type().is_file());
        stopped(&handle);

        let elsewhere = root.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let linked_run = root.path().join("runs").join("run-linked");
        symlink(&elsewhere, &linked_run).unwrap();
        let refused = launched(root.path(), "echo escaped", &linked_run.join("session.log"));
        assert!(refused.is_err(), "a linked run directory is refused");
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    /// `script` as one simple command.
    fn sh(script: &str) -> String {
        format!("sh -c {}", shell_quote(script))
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let started = Instant::now();
        while !done() {
            assert!(started.elapsed() < Duration::from_secs(20), "{what}");
            thread::sleep(POLL);
        }
    }

    fn pid_in(path: &Path) -> u32 {
        let mut pid = None;
        wait_for("the pid file", || {
            pid = fs::read_to_string(path)
                .ok()
                .and_then(|text| text.trim().parse().ok());
            pid.is_some()
        });
        pid.unwrap()
    }

    fn running(pid: u32) -> bool {
        // A zombie of nobody's is gone for this purpose.
        let stat = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&stat.stdout);
        !stat.trim().is_empty() && !stat.trim().starts_with('Z')
    }

    /// A wrapper is started detached, with the environment it is given and
    /// none of cmux's, in its directory, its output in its log; it is told
    /// by its handle, and SIGTERM stops one that takes it.
    #[test]
    fn a_detached_process_is_started_found_and_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("session.log");
        let wrappers = BackgroundWrappers {
            processes: &SystemProcesses,
        };
        let handle = wrappers
            .launch(
                dir.path(),
                &sh("echo \"$DAGQ_ROLE ${CMUX_WORKSPACE_ID:-none} $(basename \"$PWD\")\"; ps -o ppid= -p $$; exec sleep 30"),
                &[("DAGQ_ROLE".into(), "worker".into())],
                &log,
            )
            .unwrap();
        let handle = BackgroundHandle::parse(&handle).unwrap();
        assert!(wrappers.alive(&handle));
        let name = dir.path().file_name().unwrap().to_str().unwrap().to_owned();
        let mut text = String::new();
        wait_for("the log", || {
            text = fs::read_to_string(&log).unwrap_or_default();
            text.lines().count() >= 2
        });
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(format!("worker none {name}").as_str()));
        // Not this process's child: init's, once the shell exited.
        assert_ne!(lines.next().unwrap().trim(), std::process::id().to_string());
        wrappers.stop(&handle).unwrap();
        assert!(!wrappers.alive(&handle));
        // Stopping one that is gone is nothing.
        wrappers.stop(&handle).unwrap();
        // A pid with another start is not the wrapper.
        let other = BackgroundHandle {
            pid: std::process::id(),
            start: "Thu_Jan_1_00:00:00_1970".into(),
        };
        assert!(!wrappers.alive(&other));
        wrappers.stop(&other).unwrap();
    }

    /// A wrapper that ignores SIGTERM is killed with its group, and a
    /// process it started in a group of its own (a turn) is killed too.
    #[test]
    fn a_wrapper_that_holds_on_is_killed_with_what_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("session.log");
        let turn = dir.path().join("turn.pid");
        let wrappers = BackgroundWrappers {
            processes: &SystemProcesses,
        };
        let script = sh(&format!(
            "trap '' TERM; perl -e 'use POSIX; setsid(); exec qw(sleep 30)' & echo $! > {}; sleep 30; wait",
            shell_quote(turn.to_str().unwrap())
        ));
        let handle = wrappers
            .launch(dir.path(), &script, &[], &log)
            .map(|id| BackgroundHandle::parse(&id).unwrap())
            .unwrap();
        let turn = pid_in(&turn);
        wait_for("the turn to start", || running(turn));
        wrappers.stop(&handle).unwrap();
        assert!(!wrappers.alive(&handle));
        wait_for("the turn to be killed", || !running(turn));
    }

    /// A command that exits at once leaves no wrapper to record.
    #[test]
    fn a_wrapper_that_exits_at_once_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("session.log");
        let wrappers = BackgroundWrappers {
            processes: &SystemProcesses,
        };
        let mut error = None;
        // The shell may still see the process for a moment after its exit.
        for _ in 0..20 {
            match wrappers.launch(dir.path(), &sh("exit 3"), &[], &log) {
                Err(e) => {
                    error = Some(format!("{e:#}"));
                    break;
                }
                Ok(handle) => {
                    let handle = BackgroundHandle::parse(&handle).unwrap();
                    wait_for("the process to end", || !wrappers.alive(&handle));
                }
            }
        }
        let error = error.expect("a launch saw the process gone");
        assert!(error.contains("exited as soon as it started"), "{error}");
        assert!(
            wrappers
                .launch(&dir.path().join("missing"), "true", &[], &log)
                .is_err()
        );
    }
}
