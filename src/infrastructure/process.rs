//! The [`Spawner`] port on `std::process`: a [`CommandSpec`] becomes a
//! `Command`, started with the streams the caller asked for.

use std::{
    fs,
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicI32, Ordering::SeqCst},
};

use anyhow::Result;

use crate::application::{CommandSpec, Exit, Spawned, Spawner, Streams};

/// The `Command` that starts `spec`, its streams left to the caller.
pub fn command(spec: &CommandSpec) -> Command {
    let mut command = Command::new(spec.get_program());
    command.args(spec.get_args());
    for (key, value) in spec.get_envs() {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    if let Some(dir) = spec.get_current_dir() {
        command.current_dir(dir);
    }
    if spec.get_new_session() {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid(2) is async-signal-safe and touches no memory.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command
}

/// How the process ended, in the port's terms.
pub fn exit(status: ExitStatus) -> Exit {
    Exit {
        success: status.success(),
        code: status.code(),
        signal: std::os::unix::process::ExitStatusExt::signal(&status),
        description: status.to_string(),
    }
}

/// The process groups this process started in sessions of their own and
/// has not reaped or killed yet, for [`stop_groups_on_exit_signals`]: a
/// fixed table, so that the signal handler reads it without allocating. A
/// group past its size is not tracked.
static GROUPS: [AtomicI32; 16] = [const { AtomicI32::new(0) }; 16];

fn track(group: i32) {
    let _ = GROUPS
        .iter()
        .find(|slot| slot.compare_exchange(0, group, SeqCst, SeqCst).is_ok());
}

fn untrack(group: i32) {
    for slot in &GROUPS {
        let _ = slot.compare_exchange(group, 0, SeqCst, SeqCst);
    }
}

extern "C" fn stop_groups(signal: libc::c_int) {
    for slot in &GROUPS {
        let group = slot.load(SeqCst);
        if group > 0 {
            // SAFETY: kill(2) is async-signal-safe and takes no pointer.
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
    }
    // SAFETY: signal(2) and raise(3) are async-signal-safe; the default
    // action ends this process as the signal would have.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Stop the groups this process started ([`CommandSpec::new_session`])
/// when it is hung up, terminated or interrupted: a headless turn of the
/// session wrapper leads a session of its own with no terminal, and would
/// outlive a wrapper whose workspace was closed (ADR-t813-1 decision 3).
/// Only the wrapper installs it.
pub fn stop_groups_on_exit_signals() {
    for signal in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler only reads atomics and calls
        // async-signal-safe functions.
        unsafe { libc::signal(signal, stop_groups as *const () as libc::sighandler_t) };
    }
}

/// Starts processes as children of this one.
pub struct LocalSpawner;

impl Spawner for LocalSpawner {
    fn spawn(&self, spec: &CommandSpec, streams: Streams<'_>) -> Result<Box<dyn Spawned>> {
        let mut command = command(spec);
        match streams {
            Streams::Inherit => {}
            Streams::Null => {
                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
            }
            Streams::Files { stdout, stderr } => {
                command
                    .stdin(Stdio::null())
                    .stdout(fs::File::create(stdout)?)
                    .stderr(fs::File::create(stderr)?);
            }
            Streams::Log(log) => {
                let log = fs::File::create(log)?;
                command
                    .stdin(Stdio::null())
                    .stdout(log.try_clone()?)
                    .stderr(log);
            }
        }
        let child = command.spawn()?;
        if spec.get_new_session() {
            track(child.id() as i32);
        }
        Ok(Box::new(LocalChild(child, spec.get_new_session())))
    }
}

/// A child, and whether it leads a session (and so a process group) of its
/// own.
struct LocalChild(Child, bool);

impl Spawned for LocalChild {
    fn id(&self) -> u32 {
        self.0.id()
    }
    fn try_wait(&mut self) -> Result<Option<Exit>> {
        let status = self.0.try_wait()?;
        if status.is_some() && self.1 {
            untrack(self.0.id() as i32);
        }
        Ok(status.map(exit))
    }
    fn kill(&mut self) -> Result<()> {
        Ok(self.0.kill()?)
    }
    fn wait(&mut self) -> Result<Exit> {
        let status = self.0.wait()?;
        if self.1 {
            untrack(self.0.id() as i32);
        }
        Ok(exit(status))
    }
    fn kill_group(&mut self) -> Result<()> {
        if !self.1 {
            return self.kill();
        }
        untrack(self.0.id() as i32);
        // SAFETY: kill(2) takes no pointer; a negative pid names the group
        // the child leads.
        let signaled = unsafe { libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL) };
        if signaled == -1 {
            let error = std::io::Error::last_os_error();
            // A group that is gone has nothing left to stop.
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error.into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_starts_with_its_arguments_environment_and_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (out, err) = (dir.path().join("out"), dir.path().join("err"));
        let mut spec = CommandSpec::new("/bin/sh");
        spec.current_dir(dir.path())
            .env("KEPT", "yes")
            .env("GONE", "no")
            .env_remove("GONE")
            .args(["-c", "printf '%s %s %s' \"$KEPT\" \"${GONE:-unset}\" \"$(basename \"$PWD\")\"; echo oops >&2; exit 3"]);
        let mut child = LocalSpawner
            .spawn(
                &spec,
                Streams::Files {
                    stdout: &out,
                    stderr: &err,
                },
            )
            .unwrap();
        let exit = child.wait().unwrap();
        assert_eq!((exit.success, exit.code), (false, Some(3)));
        assert_eq!(exit.to_string(), "exit status: 3");
        let name = dir.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            format!("yes unset {name}")
        );
        assert_eq!(fs::read_to_string(&err).unwrap(), "oops\n");
    }

    #[test]
    fn a_running_child_is_killed_and_reaped() {
        let mut child = LocalSpawner
            .spawn(CommandSpec::new("/bin/sleep").arg("30"), Streams::Null)
            .unwrap();
        assert!(child.id() > 0);
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        let exit = child.wait().unwrap();
        assert!(!exit.success && exit.code.is_none(), "{exit}");
        assert!(
            LocalSpawner
                .spawn(&CommandSpec::new("/nonexistent/program"), Streams::Inherit)
                .is_err()
        );
    }

    #[test]
    fn a_new_session_child_leads_its_own_session() {
        let dir = tempfile::tempdir().unwrap();
        let (out, err) = (dir.path().join("out"), dir.path().join("err"));
        let mut spec = CommandSpec::new("/bin/sh");
        spec.args(["-c", "ps -o sess= -o pgid= -p $$"])
            .new_session();
        let mut child = LocalSpawner
            .spawn(
                &spec,
                Streams::Files {
                    stdout: &out,
                    stderr: &err,
                },
            )
            .unwrap();
        let pid = child.id();
        assert!(child.wait().unwrap().success);
        // The child is the leader of its process group (setsid(2) made it).
        let fields = fs::read_to_string(&out).unwrap();
        let pgid = fields.split_whitespace().last().unwrap();
        assert_eq!(pgid, pid.to_string(), "{fields}");
    }

    #[test]
    fn a_group_is_killed_with_what_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mut spec = CommandSpec::new("/bin/sh");
        spec.args([
            "-c",
            &format!("sleep 30 & echo $! > {}; wait", pid_file.display()),
        ])
        .new_session();
        let mut child = LocalSpawner.spawn(&spec, Streams::Null).unwrap();
        let tracked = |pid: u32| GROUPS.iter().any(|slot| slot.load(SeqCst) == pid as i32);
        // Tracked for the exit signals of the process until it is killed.
        assert!(tracked(child.id()));
        let started = std::time::Instant::now();
        let grandchild: libc::pid_t = loop {
            if let Some(pid) = fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                break pid;
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(10));
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        child.kill_group().unwrap();
        assert!(!tracked(child.id()));
        assert!(!child.wait().unwrap().success);
        // The sleep was in the group: gone too (reaped by init, or a zombie
        // of nobody's for a moment).
        let started = std::time::Instant::now();
        // SAFETY: kill(2) with signal 0 only checks the pid.
        while unsafe { libc::kill(grandchild, 0) } == 0 {
            let stat = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &grandchild.to_string()])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&stat.stdout)
                .trim()
                .starts_with('Z')
            {
                break;
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(10));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Killing a group that is gone is no error.
        child.kill_group().unwrap();
        // A child that leads no group of its own is killed alone.
        let mut alone = LocalSpawner
            .spawn(CommandSpec::new("/bin/sleep").arg("30"), Streams::Null)
            .unwrap();
        alone.kill_group().unwrap();
        assert!(!alone.wait().unwrap().success);
    }

    #[test]
    fn a_log_takes_both_streams() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("output.log");
        let mut spec = CommandSpec::new("/bin/sh");
        spec.args(["-c", "echo out; echo err >&2"]);
        let exit = LocalSpawner
            .spawn(&spec, Streams::Log(&log))
            .unwrap()
            .wait()
            .unwrap();
        assert!(exit.success);
        assert_eq!(fs::read_to_string(&log).unwrap(), "out\nerr\n");
    }
}
