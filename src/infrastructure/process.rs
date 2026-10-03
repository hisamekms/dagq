//! The [`Spawner`] port on `std::process`: a [`CommandSpec`] becomes a
//! `Command`, started with the streams the caller asked for.

use std::{
    io::{Seek, SeekFrom, Write},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicI32, Ordering::SeqCst},
};

use anyhow::Result;

use crate::application::{CommandSpec, Exit, Spawned, Spawner, StdinUnprepared, Streams};

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

/// The standard input of `spec`: its own text
/// ([`CommandSpec::get_stdin`]) from a file of its own, which is unlinked
/// at once and so goes with the process, or nothing. A file and not a pipe,
/// so a text of any size is handed over without a writer waiting on the
/// process to read it. Readable by this user only, as the prompt may say
/// what others should not read. Its errors are [`StdinUnprepared`], so that
/// a start that fails here is not taken for its program's
/// ([`crate::application::job_start_failure`]).
pub fn stdin(spec: &CommandSpec) -> Result<Stdio> {
    stdin_in(spec, &std::env::temp_dir())
}

/// [`stdin`] with its file in `dir`.
fn stdin_in(spec: &CommandSpec, dir: &std::path::Path) -> Result<Stdio> {
    use std::os::unix::fs::OpenOptionsExt;
    let Some(text) = spec.get_stdin() else {
        return Ok(Stdio::null());
    };
    let path = dir.join(format!("dagq-stdin-{}", uuid::Uuid::new_v4()));
    let failed = |what: &str, source: std::io::Error| {
        anyhow::Error::new(StdinUnprepared {
            what: format!("{what} the standard input {}", path.display()),
            source,
        })
    };
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| failed("create", error))?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.seek(SeekFrom::Start(0)));
    std::fs::remove_file(&path).map_err(|error| failed("remove", error))?;
    written.map_err(|error| failed("write", error))?;
    Ok(Stdio::from(file))
}

/// A stub agent for the tests of a provider's command (task 1560): it
/// writes how many bytes its arguments and its standard input had to
/// `args` and `stdin` in the directory `OUT` names.
#[cfg(test)]
pub(crate) mod stub_agent {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, path::Path, path::PathBuf};

    /// The stub's executable, written in `dir`.
    pub(crate) fn write(dir: &Path) -> PathBuf {
        let stub = dir.join("agent");
        fs::write(
            &stub,
            "#!/bin/sh\nprintf '%s' \"$*\" | wc -c | tr -d ' ' > \"$OUT/args\"\nwc -c | tr -d ' ' > \"$OUT/stdin\"\n",
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        stub
    }

    /// Start `command` (whose program is the stub) as a headless job is
    /// started, wait for it, and read the bytes of its arguments and its
    /// standard input from `dir`.
    pub(crate) fn run(command: &CommandSpec, dir: &Path) -> (usize, usize) {
        let mut command = command.clone();
        command.env("OUT", dir);
        let (out, err) = (dir.join("out"), dir.join("err"));
        let streams = Streams::Files {
            stdout: &out,
            stderr: &err,
        };
        let mut child = LocalSpawner.spawn(&command, streams).unwrap();
        assert!(child.wait().unwrap().success);
        let read = |name: &str| {
            fs::read_to_string(dir.join(name))
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        };
        (read("args"), read("stdin"))
    }
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
/// outlive a wrapper whose workspace was closed (ADR-t813-1 decision 3),
/// or a wrapper started in the background that the supervisor stops with
/// SIGTERM (ADR-t1404-1 decision 3; the supervisor kills what is left with
/// SIGKILL after a grace, the turns' groups included, which this handler
/// cannot see). Only the wrapper installs it. It stops the groups alone: listing a
/// turn's descendants (`ps`) is not async-signal-safe, so a command the
/// turn runs in a group of its own is left to `stop_processes`.
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
                command.stdout(Stdio::null()).stderr(Stdio::null());
            }
            Streams::Files { stdout, stderr } => {
                command
                    .stdout(super::agent_dir::create_file(stdout)?)
                    .stderr(super::agent_dir::create_file(stderr)?);
            }
            Streams::Log(log) => {
                let log = super::agent_dir::create_file(log)?;
                command.stdout(log.try_clone()?).stderr(log);
            }
        }
        if spec.get_stdin().is_some() || !matches!(streams, Streams::Inherit) {
            command.stdin(stdin(spec)?);
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
            // A group that is gone has nothing left to stop. On macOS a
            // group left with only its leader that exited and is not reaped
            // yet (a zombie) refuses the signal with EPERM: a headless turn
            // that said its login ran out and ended before the wrapper
            // stopped it (task 1397).
            let gone = error.raw_os_error() == Some(libc::ESRCH)
                || (error.raw_os_error() == Some(libc::EPERM)
                    && matches!(self.0.try_wait(), Ok(Some(_))));
            if !gone {
                return Err(error.into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn redirected_output_never_writes_through_worker_links() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("auth.json");
        fs::write(&outside, b"secret").unwrap();
        let run = temp.path().join("runs/run");
        fs::create_dir_all(run.join("turns")).unwrap();
        let out = run.join("turns/turn-000002.jsonl");
        let err = run.join("turns/turn-000002.err");
        symlink(&outside, &out).unwrap();
        symlink(&outside, &err).unwrap();
        let mut spec = CommandSpec::new("/bin/sh");
        spec.args(["-c", "printf output; printf error >&2"]);
        let mut child = LocalSpawner
            .spawn(
                &spec,
                Streams::Files {
                    stdout: &out,
                    stderr: &err,
                },
            )
            .unwrap();
        assert!(child.wait().unwrap().success);
        assert_eq!(fs::read(&out).unwrap(), b"output");
        assert_eq!(fs::read(&err).unwrap(), b"error");
        assert_eq!(fs::read(&outside).unwrap(), b"secret");
        fs::rename(run.join("turns"), run.join("original")).unwrap();
        symlink(temp.path(), run.join("turns")).unwrap();
        assert!(
            LocalSpawner
                .spawn(
                    &spec,
                    Streams::Files {
                        stdout: &out,
                        stderr: &err
                    }
                )
                .is_err()
        );
        assert!(!temp.path().join("turn-000002.jsonl").exists());
    }

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

    /// A text past the system's limit on the arguments (about 1 MB on
    /// macOS, task 1560) reaches the process on its standard input whole,
    /// where on its command line it fails to start with `E2BIG`.
    #[test]
    fn a_text_past_the_argument_limit_reaches_stdin_whole() {
        let dir = tempfile::tempdir().unwrap();
        let (out, err) = (dir.path().join("out"), dir.path().join("err"));
        let text = "x".repeat(2 * 1024 * 1024);
        let streams = || Streams::Files {
            stdout: &out,
            stderr: &err,
        };
        let mut spec = CommandSpec::new("/bin/sh");
        spec.args(["-c", "wc -c | tr -d ' '"]).stdin(text.as_str());
        let mut child = LocalSpawner.spawn(&spec, streams()).unwrap();
        assert!(child.wait().unwrap().success);
        assert_eq!(fs::read_to_string(&out).unwrap().trim(), "2097152");
        let mut on_the_line = CommandSpec::new("/bin/echo");
        on_the_line.arg(&text);
        let error = LocalSpawner.spawn(&on_the_line, streams()).err().unwrap();
        let error = error.downcast_ref::<std::io::Error>().unwrap();
        assert_eq!(error.raw_os_error(), Some(libc::E2BIG), "{error}");
        // Without a text of its own, the process reads nothing.
        let mut empty = CommandSpec::new("/bin/sh");
        empty.args(["-c", "wc -c | tr -d ' '"]);
        let mut child = LocalSpawner.spawn(&empty, streams()).unwrap();
        assert!(child.wait().unwrap().success);
        assert_eq!(fs::read_to_string(&out).unwrap().trim(), "0");
    }

    /// A standard input whose file cannot be made (its directory gone, as
    /// a `TMPDIR` that was removed) fails the start as the starter's
    /// environment, which holds no provider (task 1560).
    #[test]
    fn a_stdin_that_cannot_be_prepared_is_no_providers_failure() {
        use crate::{application::job_start_failure, domain::headless_job::JobFailure};
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        let mut spec = CommandSpec::new("/bin/cat");
        spec.stdin("the prompt");
        let error = stdin_in(&spec, &gone).err().unwrap();
        assert!(
            format!("{error:#}").contains("create the standard input"),
            "{error:#}"
        );
        assert_eq!(job_start_failure(&error), JobFailure::Other);
        // Without a text of its own there is nothing to prepare.
        assert!(stdin_in(&CommandSpec::new("/bin/cat"), &gone).is_ok());
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

    /// Task 1397: a group whose leader exited and is not reaped yet is
    /// nothing left to stop, wherever the system refuses the signal to a
    /// group of zombies (macOS: EPERM); the exit is read after.
    #[test]
    fn a_group_whose_leader_exited_unreaped_is_no_error_to_kill() {
        let mut spec = CommandSpec::new("/bin/sh");
        spec.args(["-c", "exit 3"]).new_session();
        let mut child = LocalSpawner.spawn(&spec, Streams::Null).unwrap();
        let pid = child.id().to_string();
        let started = std::time::Instant::now();
        loop {
            let stat = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid])
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
        child.kill_group().unwrap();
        assert_eq!(child.wait().unwrap().code, Some(3));
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
