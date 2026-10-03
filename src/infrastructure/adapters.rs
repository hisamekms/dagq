use crate::infrastructure::git_binary::git_executable;
use crate::{
    application::{
        AgentProvider, CommandSpec, DetachedRefusal, FileStamp, LandingBranchStamp, MainRemote,
        PlannerCommand, PluginState, ProcessControl, Repository, SupervisorEnvironment, TurnReader,
        TurnTarget, WorkspaceBackend, WorkspaceTags, execution::permission_deny,
        stats::WorkspaceListing,
    },
    domain::{
        ActorRole, CommitSha, PlannerOrigin, Task, TaskId, TaskRun,
        headless_job::JobAccess,
        landing_branch::{self, LandingBranch, PushTarget, RepositoryConfig, RepositorySettings},
        measure::HostVersions,
        recovery::ProcessInfo,
        stall::IDLE_LOG,
        stats::{
            ListedWorkspace,
            conflicts::{MainChange, MainCommit, MainHistory},
        },
    },
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fs,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::application::naming::repository_name;
pub use crate::application::{
    SOCKET_PASSWORD_ENV,
    naming::{
        ask_notification_title, inbox_workspace_name, planner_workspace_name,
        resume_workspace_description, shell_join, shell_quote, supervisor_workspace_name,
        workspace_description, workspace_group_name,
    },
    path_text,
};
use crate::domain::background_wrapper::{BackgroundHandle, is_background};
use crate::domain::turn::TurnSession;
use crate::infrastructure::claude_turns::{ClaudeTurnReader, HEADLESS_PERMISSION_MODE};
use crate::infrastructure::run_env::load_repository_config;

pub fn executable(path: &Path) -> Result<PathBuf> {
    let candidate = if path.components().count() > 1 || path.is_absolute() {
        path.to_owned()
    } else {
        env::split_paths(&env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join(path))
            .find(|p| p.is_file())
            .with_context(|| format!("{} was not found on PATH", path.display()))?
    };
    candidate
        .canonicalize()
        .with_context(|| format!("resolve executable {}", candidate.display()))
}

/// `kill -0` semantics: a process we may not signal (EPERM) still exists.
/// Run `command`, an outer shell that backgrounds a process printing
/// `pid=N` as its first stdout line and exits at once, and return what that
/// process wrote after the pid line and to stderr once it is gone (its exit
/// closes the pipes). The orphan is not this process's child, so a deadline
/// is kept by hand and the pid is killed when it passes.
///
/// It reads pipes to their end by design (the end is the orphan's exit),
/// so it shares the race [`output_file`] avoids: a process another thread
/// spawns while the pipes are being made can inherit their write ends, and
/// then the read waits for that process too, here up to `timeout`, which
/// then fails the call though the orphan had answered (task 1273).
fn orphan_output(command: &mut Command, timeout: Duration) -> Result<(String, String)> {
    let label = format!("{:?}", command.get_program());
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("start {label}"))?;
    let stdout = child.stdout.take().context("stdout unavailable")?;
    let mut stderr = child.stderr.take().context("stderr unavailable")?;
    let (lines, received) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let err = thread::spawn(move || {
        let mut text = String::new();
        stderr.read_to_string(&mut text).map(|_| text)
    });
    let status = child.wait().with_context(|| format!("wait for {label}"))?;
    ensure!(status.success(), "{label} failed ({status})");
    let deadline = Instant::now() + timeout;
    let mut pid = None;
    let mut reply = Vec::new();
    loop {
        match received.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(line) => {
                let line = line.context("read the orphan's stdout")?;
                if pid.is_some() {
                    reply.push(line);
                } else {
                    pid = Some(
                        line.strip_prefix("pid=")
                            .and_then(|pid| pid.parse::<u32>().ok())
                            .with_context(|| format!("{label} did not report a pid: {line}"))?,
                    );
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                if let Some(pid) = pid {
                    let _ = signal(pid, libc::SIGKILL);
                }
                anyhow::bail!("{label} did not finish within {timeout:?}");
            }
        }
    }
    let stderr = err
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader failed"))?
        .context("read the orphan's stderr")?;
    Ok((reply.join("\n"), stderr))
}

pub fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 performs no action beyond the existence and permission check.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Signals through `libc::kill`, the way `process_alive` checks liveness.
pub struct SystemProcesses;

impl ProcessControl for SystemProcesses {
    fn alive(&self, pid: u32) -> bool {
        process_alive(pid)
    }

    fn terminate(&self, pid: u32) -> Result<()> {
        signal(pid, libc::SIGTERM)
    }

    fn interrupt(&self, pid: u32) -> Result<()> {
        signal(pid, libc::SIGINT)
    }

    fn kill(&self, pid: u32) -> Result<()> {
        signal(pid, libc::SIGKILL)
    }

    fn kill_group(&self, leader: u32) -> Result<()> {
        ensure!(
            signal_group(leader, libc::SIGKILL),
            "SIGKILL to the process group {leader}: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    fn reap(&self, pid: u32) {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return;
        };
        let mut status = 0;
        // SAFETY: waitpid(2) with WNOHANG only reads the child table; for a
        // pid that is not our child it fails with ECHILD, which is ignored.
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    }

    fn list(&self) -> Result<Vec<ProcessInfo>> {
        // SAFETY: getuid(2) has no failure and no memory effects.
        let uid = unsafe { libc::getuid() }.to_string();
        let listing = output(Command::new("ps").args([
            "-U",
            &uid,
            "-o",
            "pid=,ppid=,etime=,time=,command=",
        ]))?;
        Ok(with_working_directories(parse_ps(&listing), process_cwd))
    }

    fn start_identity(&self, pid: u32) -> Option<String> {
        // `lstart` is the start to the second, in the C locale so two reads
        // print it alike; `ps` fails for a pid that runs nothing.
        let start = output(Command::new("ps").env("LC_ALL", "C").args([
            "-o",
            "lstart=",
            "-p",
            &pid.to_string(),
        ]))
        .ok()?;
        let start = start.trim();
        (!start.is_empty()).then(|| start.to_owned())
    }

    fn started_at(&self, pid: u32) -> Option<i64> {
        process_started_at(pid)
    }

    fn descendants(&self, pid: u32) -> Vec<u32> {
        // Only the parents are needed: no working directories.
        // SAFETY: getuid(2) has no failure and no memory effects.
        let uid = unsafe { libc::getuid() }.to_string();
        output(Command::new("ps").args(["-U", &uid, "-o", "pid=,ppid=,etime=,time=,command="]))
            .map(|listing| crate::domain::headless_job::descendants(&parse_ps(&listing), pid))
            .unwrap_or_default()
    }
}

/// When the process `pid` started, in unix seconds on the system clock:
/// the time before `ps` runs less the age `ps -o etime=` prints, so up to
/// a second late (the age is whole seconds) and early by however long `ps`
/// took to start on a loaded host. `None` when `ps` finds no such process
/// or its output does not read.
pub fn process_started_at(pid: u32) -> Option<i64> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let age = output(Command::new("ps").args(["-o", "etime=", "-p", &pid.to_string()])).ok()?;
    let age = i64::try_from(parse_etime(age.trim())?).ok()?;
    Some(i64::try_from(now).ok()? - age)
}

/// The lines of `ps -o pid=,ppid=,etime=,time=,command=`; a line that does
/// not read is skipped.
fn parse_ps(listing: &str) -> Vec<ProcessInfo> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let elapsed_secs = parse_etime(fields.next()?)?;
            let cpu_ms = parse_cpu_time(fields.next()?);
            let command = fields.collect::<Vec<_>>().join(" ");
            Some(ProcessInfo {
                pid,
                ppid,
                elapsed_secs,
                command,
                cwd: None,
                cpu_ms,
            })
        })
        .collect()
}

/// `ps`'s `etime`, `[[dd-]hh:]mm:ss`, in seconds.
fn parse_etime(text: &str) -> Option<u64> {
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, text),
    };
    let mut secs = 0;
    for part in clock.split(':') {
        secs = secs * 60 + part.parse::<u64>().ok()?;
    }
    Some(days * 86_400 + secs)
}

/// `ps`'s `time`, `[dd-][hh:]mm:ss[.ff]` (macOS prints hundredths, Linux
/// whole seconds), in milliseconds.
fn parse_cpu_time(text: &str) -> Option<u64> {
    let (clock, fraction) = match text.split_once('.') {
        Some((clock, fraction)) => (clock, fraction),
        None => (text, ""),
    };
    let secs = parse_etime(clock)?;
    let digits: String = fraction.chars().take(3).collect();
    let millis = if digits.is_empty() {
        0
    } else {
        digits.parse::<u64>().ok()? * 10u64.pow(3 - u32::try_from(digits.len()).ok()?)
    };
    Some(secs * 1000 + millis)
}

/// `processes` with the working directories `read_cwd` reads, one pid at
/// a time; a process whose directory cannot be read stays listed, with no
/// `cwd`.
fn with_working_directories(
    mut processes: Vec<ProcessInfo>,
    read_cwd: impl FnMut(u32) -> Option<String>,
) -> Vec<ProcessInfo> {
    let mut cwds: HashMap<u32, String> = working_directories(&processes, read_cwd)
        .into_iter()
        .collect();
    for process in &mut processes {
        process.cwd = cwds.remove(&process.pid);
    }
    processes
}

/// The working directories of `processes` that `read_cwd` can read; what
/// cannot be read is left out.
fn working_directories(
    processes: &[ProcessInfo],
    mut read_cwd: impl FnMut(u32) -> Option<String>,
) -> Vec<(u32, String)> {
    processes
        .iter()
        .filter_map(|p| Some((p.pid, read_cwd(p.pid)?)))
        .collect()
}

/// The working directory of `pid` from `proc_pidinfo(PROC_PIDVNODEPATHINFO)`:
/// one system call for the one process, no `lsof` and no walk of other
/// processes' files (task 1581). `None` when the process is gone or not
/// the user's.
#[cfg(target_os = "macos")]
fn process_cwd(pid: u32) -> Option<String> {
    let pid = libc::c_int::try_from(pid).ok()?;
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_vnodepathinfo>()).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    // SAFETY: the buffer is a zeroed proc_vnodepathinfo of `size` bytes,
    // which proc_pidinfo(3) fills for this flavor and does not keep.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    // SAFETY: proc_pidinfo filled all `size` bytes, and every bit pattern
    // is a valid proc_vnodepathinfo (integers and arrays of them).
    let info = unsafe { info.assume_init() };
    let path: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .as_flattened()
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    (!path.is_empty()).then(|| String::from_utf8_lossy(&path).into_owned())
}

/// The working directory of `pid` from `/proc/<pid>/cwd`; `None` where
/// there is no `/proc` or the link cannot be read.
#[cfg(not(target_os = "macos"))]
fn process_cwd(pid: u32) -> Option<String> {
    let cwd = fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    Some(cwd.to_string_lossy().into_owned())
}

fn signal(pid: u32, signal: libc::c_int) -> Result<()> {
    let pid = libc::pid_t::try_from(pid).context("pid does not fit a pid_t")?;
    // SAFETY: kill(2) with a valid pid and signal has no memory effects here.
    ensure!(
        unsafe { libc::kill(pid, signal) } == 0,
        "signal {signal} to pid {pid}: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

/// How long [`output`] lets a command run, and so each cmux call.
pub const OUTPUT_TIMEOUT: Duration = Duration::from_secs(30);

/// The 1-minute load average (getloadavg(3)); `None` when it is unavailable.
pub fn load_average() -> Option<f64> {
    let mut loads = [0f64; 1];
    // SAFETY: getloadavg writes at most `nelem` doubles into the buffer.
    let written = unsafe { libc::getloadavg(loads.as_mut_ptr(), 1) };
    (written >= 1 && loads[0].is_finite()).then_some(loads[0])
}

/// The bytes free for an unprivileged process on the file system of `path`
/// (statvfs(3): available blocks times the fragment size); `None` when it
/// cannot be read.
pub fn free_disk_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a NUL-terminated string and statvfs fills `stat`
    // when it returns 0.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs returned 0, so it initialized `stat`.
    let stat = unsafe { stat.assume_init() };
    #[allow(clippy::useless_conversion)]
    let (available, fragment) = (u64::from(stat.f_bavail), u64::from(stat.f_frsize));
    available.checked_mul(fragment)
}

/// The space of the filesystem `path` is on (statvfs(3)): free for a
/// process that is not root (`f_bavail`) and the total (`f_blocks`), in
/// bytes; `None` when it cannot be read (task 1371).
pub fn disk_space(path: &Path) -> Option<crate::domain::host_metrics::DiskSpace> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a NUL-terminated string and statvfs fills `stat`
    // when it returns 0.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs returned 0, so it initialized `stat`.
    let stat = unsafe { stat.assume_init() };
    #[allow(clippy::useless_conversion)]
    let (available, blocks, fragment) = (
        u64::from(stat.f_bavail),
        u64::from(stat.f_blocks),
        u64::from(stat.f_frsize),
    );
    Some(crate::domain::host_metrics::DiskSpace {
        free_bytes: available.checked_mul(fragment)?,
        total_bytes: blocks.checked_mul(fragment)?,
    })
}

/// How long [`host_versions`] lets `rustc -vV` run.
const RUSTC_VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// The versions a claim records (task 197): Claude Code's from the file
/// `claude` resolves to (`<...>/versions/<version>`, where its installer
/// keeps each version; null for any other path), Codex's from `codex
/// --version` when `codex` is given (null when it cannot be run;
/// ADR-t813-2 decision 7), and `release` and `host` of `rustc -vV` run in
/// `rustc_in`, so that its toolchain file applies (null when it cannot be
/// run, and none asked without a checkout: only dagq's source records the
/// toolchain, ADR-t614-1).
pub fn host_versions(claude: &Path, codex: Option<&Path>, rustc_in: Option<&Path>) -> HostVersions {
    let mut versions = HostVersions {
        claude_version: claude_version(claude),
        ..HostVersions::default()
    };
    if let Some(codex) = codex {
        versions = match capture(
            Command::new(codex).arg("--version").stdin(Stdio::null()),
            RUSTC_VERSION_TIMEOUT,
        ) {
            Ok((status, stdout, _)) if status.success() => versions.with_codex_version(&stdout),
            _ => versions,
        };
    }
    let Some(checkout) = rustc_in else {
        return versions;
    };
    match capture(
        Command::new("rustc").arg("-vV").current_dir(checkout),
        RUSTC_VERSION_TIMEOUT,
    ) {
        Ok((status, stdout, _)) if status.success() => versions.with_rustc_verbose(&stdout),
        _ => versions,
    }
}

/// The version the path of Claude Code names: the file name of what
/// `claude` resolves to when it sits in a `versions` directory.
pub fn claude_version(claude: &Path) -> Option<String> {
    let resolved = claude.canonicalize().ok()?;
    let parent = resolved.parent()?.file_name()?;
    (parent == "versions")
        .then(|| resolved.file_name()?.to_str().map(str::to_owned))
        .flatten()
}

pub fn output(command: &mut Command) -> Result<String> {
    let (status, stdout, stderr) = capture(command, OUTPUT_TIMEOUT)?;
    ensure!(
        status.success(),
        "{:?} failed ({status}): {stderr}",
        command.get_program()
    );
    Ok(stdout)
}

/// Run to completion with a deadline; the caller interprets the exit status.
pub fn capture(command: &mut Command, timeout: Duration) -> Result<(ExitStatus, String, String)> {
    let (status, stdout, stderr) = capture_bytes(command, timeout)?;
    Ok((
        status,
        String::from_utf8(stdout).context("command output is not UTF-8")?,
        stderr,
    ))
}

/// [`capture`] without the UTF-8 requirement on stdout.
pub(crate) fn capture_bytes(
    command: &mut Command,
    timeout: Duration,
) -> Result<(ExitStatus, Vec<u8>, String)> {
    let label = format!("{:?}", command.get_program());
    let stdout = output_file()?;
    let stderr = output_file()?;
    let mut child = command
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("start {label}"))?;
    let status = wait_with_deadline(&mut child, &label, timeout)?;
    let stderr = read_back(stderr)?;
    Ok((
        status,
        read_back(stdout)?,
        String::from_utf8_lossy(&stderr).into_owned(),
    ))
}

/// An unlinked file in the temporary directory to take a command's output.
/// Not a pipe: macOS creates a pipe and marks it close-on-exec in two steps,
/// so a process another thread spawns in between inherits its write end,
/// and a pipe read to its end then waits for that process as well. A
/// long-lived one (a session, an agent) held the supervisor in such a read
/// forever after the command itself had exited (task 1022). A file is
/// complete once the command exits, whoever else holds it.
///
/// The standard library opens it close-on-exec (`O_CLOEXEC`) in the one
/// `open`, so only the child it is handed to as a stream holds it.
pub(crate) fn output_file() -> io::Result<fs::File> {
    let path = env::temp_dir().join(format!("dagq-output-{}", uuid::Uuid::new_v4()));
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| {
            io::Error::new(error.kind(), format!("create {}: {error}", path.display()))
        })?;
    fs::remove_file(&path).map_err(|error| {
        io::Error::new(error.kind(), format!("unlink {}: {error}", path.display()))
    })?;
    Ok(file)
}

pub(crate) fn read_back(mut file: fs::File) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// [`Command::output`] with stdout and stderr taken in [`output_file`]s,
/// not pipes, so another process holding them open (one spawned on another
/// thread at the same moment) does not hold the call past the command's
/// own exit. Like `output`, stdin is null and there is no time limit.
pub(crate) fn unpiped_output(command: &mut Command) -> io::Result<Output> {
    let (mut child, stdout, stderr) = spawn_unpiped(command)?;
    let status = child.wait()?;
    Ok(Output {
        status,
        stdout: read_back(stdout)?,
        stderr: read_back(stderr)?,
    })
}

/// [`unpiped_output`] within `timeout`; `None` when the command ran past it
/// and was killed.
pub(crate) fn unpiped_output_within(
    command: &mut Command,
    timeout: Duration,
) -> io::Result<Option<Output>> {
    let (mut child, stdout, stderr) = spawn_unpiped(command)?;
    let Some(status) = wait_until(&mut child, timeout).map_err(io::Error::other)? else {
        return Ok(None);
    };
    Ok(Some(Output {
        status,
        stdout: read_back(stdout)?,
        stderr: read_back(stderr)?,
    }))
}

fn spawn_unpiped(command: &mut Command) -> io::Result<(Child, fs::File, fs::File)> {
    let stdout = output_file()?;
    let stderr = output_file()?;
    let child = command
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .stdin(Stdio::null())
        .spawn()?;
    Ok((child, stdout, stderr))
}

/// Deadline of the Git commands that gather a review: a large diff takes far
/// longer to produce than the 30 seconds of [`output`].
pub const REVIEW_TIMEOUT: Duration = Duration::from_secs(300);

/// Like [`output`] with [`REVIEW_TIMEOUT`], reading stdout lossily so text in
/// any encoding (Latin-1 files, non-UTF-8 commit messages) does not fail.
fn review_output(command: &mut Command) -> Result<String> {
    let (status, stdout, stderr) = capture_bytes(command, REVIEW_TIMEOUT)?;
    ensure!(
        status.success(),
        "{:?} failed ({status}): {stderr}",
        command.get_program()
    );
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// Run `command` with its stdout appended to `file` as raw bytes, never
/// holding it in memory, under [`REVIEW_TIMEOUT`].
fn review_output_to(command: &mut Command, file: &fs::File) -> Result<()> {
    let label = format!("{:?}", command.get_program());
    let stderr = output_file()?;
    let mut child = command
        .stdout(Stdio::from(file.try_clone()?))
        .stderr(stderr.try_clone()?)
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("start {label}"))?;
    let status = wait_with_deadline(&mut child, &label, REVIEW_TIMEOUT)?;
    let stderr = String::from_utf8_lossy(&read_back(stderr)?).into_owned();
    ensure!(status.success(), "{label} failed ({status}): {stderr}");
    Ok(())
}

/// How often [`wait_with_deadline`] checks for the child's exit: every
/// millisecond for its first [`EXIT_POLL_FAST_FOR`], then every
/// [`EXIT_POLL`]. Most commands (Git's) exit within a few to a few tens of
/// milliseconds, and a run makes many of them: a fixed 20 ms pause added
/// up to most of the time they took.
const EXIT_POLL: Duration = Duration::from_millis(20);
const EXIT_POLL_FAST: Duration = Duration::from_millis(1);
const EXIT_POLL_FAST_FOR: Duration = Duration::from_millis(100);

fn wait_with_deadline(child: &mut Child, label: &str, timeout: Duration) -> Result<ExitStatus> {
    match wait_until(child, timeout)? {
        Some(status) => Ok(status),
        None => bail!("{label} timed out; external resources may have been created"),
    }
}

/// Wait for `child` to exit within `timeout`; `None` when it did not and
/// was killed.
fn wait_until(child: &mut Child, timeout: Duration) -> Result<Option<ExitStatus>> {
    let started = Instant::now();
    let deadline = started + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(if started.elapsed() < EXIT_POLL_FAST_FOR {
            EXIT_POLL_FAST
        } else {
            EXIT_POLL
        });
    }
}

/// Verification commands are task-defined shell lines whose full output belongs
/// in the run directory, not in the event payload.
pub const VERIFICATION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long a verification command's process group has, after SIGTERM at
/// its limit, before the group gets SIGKILL.
const VERIFICATION_STOP_GRACE: Duration = Duration::from_secs(15);

/// How often [`stop_verification_group`] looks for what is left during the
/// grace (each look lists the processes).
const VERIFICATION_STOP_POLL: Duration = Duration::from_millis(100);

/// How long [`stop_verification_group`] waits for the group to be empty
/// once it was killed: the descendants are reaped by `init` once they die.
const VERIFICATION_GONE_WITHIN: Duration = Duration::from_secs(10);

/// The variable that sets how nextest counts a test that passed on its
/// retry. integrate passes it only for its flaky retry (task 1039).
const NEXTEST_FLAKY_RESULT: &str = "NEXTEST_FLAKY_RESULT";

/// `script` under `/bin/sh -c` in `cwd` with `env`. The first verification
/// takes nextest's flaky result from `.config/nextest.toml`, so a value the
/// process inherited (a supervisor or test started inside a flaky retry's
/// verification) is not passed on unless `env` names one (task 1161).
fn verification_command(script: &str, cwd: &Path, env: &[(String, String)]) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .env_remove(NEXTEST_FLAKY_RESULT)
        .envs(env.iter().map(|(key, value)| (key, value)));
    command
}

/// Run `script` with `/bin/sh -c` in `cwd`, its output in `log`, in a
/// process group of its own, stopped once it runs past `timeout` (the whole
/// command, [`VERIFICATION_TIMEOUT`] for the verifier). At the limit the
/// whole group (the shell and what it started, `cargo` and the tests, and
/// the groups nextest's tests run in) gets SIGTERM, then SIGKILL after
/// [`VERIFICATION_STOP_GRACE`] unless it ended, and the call returns once
/// no process of them is left, or [`VERIFICATION_GONE_WITHIN`] after the
/// SIGKILL (task 1098): a retry in the same worktree must not race the
/// previous attempt's descendants. A stop at the limit is the error
/// [`CommandTimedOut`](crate::domain::verify_failure::CommandTimedOut),
/// which integrate records as a `timeout` failure of the command (task 639).
pub fn run_shell_to_log(
    script: &str,
    cwd: &Path,
    env: &[(String, String)],
    log: &Path,
    timeout: Duration,
) -> Result<ExitStatus> {
    let file =
        super::agent_dir::create_file(log).with_context(|| format!("create {}", log.display()))?;
    let mut child = verification_command(script, cwd, env)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file.try_clone()?))
        .stderr(Stdio::from(file))
        .process_group(0)
        .spawn()
        .with_context(|| format!("start verification command {script:?}"))?;
    let started = Instant::now();
    let deadline = started + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            stop_verification_group(&mut child, VERIFICATION_STOP_GRACE);
            return Err(
                anyhow::Error::new(crate::domain::verify_failure::CommandTimedOut {
                    limit_secs: timeout.as_secs(),
                })
                .context(format!("verification command {script:?} timed out")),
            );
        }
        thread::sleep(if started.elapsed() < EXIT_POLL_FAST_FOR {
            EXIT_POLL_FAST
        } else {
            EXIT_POLL
        });
    }
}

/// Send `signal` to the process group `leader` leads; `false` when no
/// process of the group is left.
fn signal_group(leader: u32, signal: i32) -> bool {
    let Ok(group) = libc::pid_t::try_from(leader) else {
        return false;
    };
    signal_target(-group, signal)
}

/// Send `signal` to `target` (a pid, or a process group when negative);
/// `false` when it is gone.
fn signal_target(target: libc::pid_t, signal: i32) -> bool {
    // SAFETY: kill(2) takes no pointer.
    let sent = unsafe { libc::kill(target, signal) } == 0;
    sent || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Add to `groups` the process groups of `leader`'s descendants that left
/// its group: cargo-nextest runs each test in a group of its own, and the
/// processes a test starts join it.
fn note_escaped_groups(leader: u32, groups: &mut std::collections::BTreeSet<libc::pid_t>) {
    let Ok(own) = libc::pid_t::try_from(leader) else {
        return;
    };
    for pid in SystemProcesses.descendants(leader) {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            continue;
        };
        // SAFETY: getpgid(2) takes no pointer.
        let group = unsafe { libc::getpgid(pid) };
        if group > 0 && group != own {
            groups.insert(group);
        }
    }
}

/// Stop the process group `leader` leads, and the groups its descendants
/// moved to (nextest's tests): SIGTERM to all of them, then SIGKILL once
/// none is left or `grace` has passed, then wait (within
/// [`VERIFICATION_GONE_WITHIN`], killing again) until no process of them
/// is left. The descendants are listed again while the grace runs, so the
/// groups started before the SIGTERM landed are found while their parents
/// still live. The leader is reaped as it is watched, so a zombie leader
/// does not keep the group alive. The grace is longer than the one nextest
/// gives its tests on SIGTERM (10 seconds), so nextest stops them itself.
fn stop_verification_group(leader: &mut Child, grace: Duration) {
    let id = leader.id();
    let mut escaped = std::collections::BTreeSet::new();
    note_escaped_groups(id, &mut escaped);
    signal_group(id, libc::SIGTERM);
    for group in &escaped {
        signal_target(-group, libc::SIGTERM);
    }
    let mut reaped = false;
    let left = |escaped: &std::collections::BTreeSet<libc::pid_t>| {
        signal_group(id, 0) || escaped.iter().any(|group| signal_target(-group, 0))
    };
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if !reaped {
            let mut found = escaped.clone();
            note_escaped_groups(id, &mut found);
            for group in found.difference(&escaped) {
                signal_target(-group, libc::SIGTERM);
            }
            escaped = found;
            reaped = !matches!(leader.try_wait(), Ok(None));
        }
        if reaped && !left(&escaped) {
            return;
        }
        thread::sleep(VERIFICATION_STOP_POLL);
    }
    let kill_all = |escaped: &std::collections::BTreeSet<libc::pid_t>| {
        let mut any = signal_group(id, libc::SIGKILL);
        for group in escaped {
            any |= signal_target(-group, libc::SIGKILL);
        }
        any
    };
    kill_all(&escaped);
    if !reaped {
        let _ = leader.wait();
    }
    let deadline = Instant::now() + VERIFICATION_GONE_WITHIN;
    while kill_all(&escaped) && Instant::now() < deadline {
        thread::sleep(EXIT_POLL);
    }
}

/// `path`'s [`FileStamp`], `None` when it is not there (or not readable).
fn file_stamp(path: &Path) -> Option<FileStamp> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path).ok()?;
    Some(FileStamp {
        modified: metadata.modified().ok(),
        len: metadata.len(),
        inode: metadata.ino(),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    })
}

/// Canonical Git common directory of the repository containing `path`. Every
/// worktree of a repository, including run worktrees, resolves to the same one.
pub fn git_common_dir(path: &Path) -> Result<PathBuf> {
    let git = git_executable()?;
    let raw = output(Command::new(&git).arg("-C").arg(path).args([
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ]))
    .with_context(|| format!("{} is not inside a Git repository", path.display()))?;
    PathBuf::from(raw.trim())
        .canonicalize()
        .context("resolve Git common directory")
}

/// The main checkout of the repository at `path` (any worktree of it, or
/// its Git common directory): the main worktree, which `git worktree list`
/// lists first and whose `dagq.toml` dagq reads. The only place dagq
/// decides it. A bare main worktree has no files, and so no main checkout;
/// Git does not record the main worktree of a repository with a separate
/// Git directory (`git init --separate-git-dir`) and lists the Git
/// directory in its place, which leaves only `path` itself when it is that
/// worktree. Both are errors that say why and what to do: the `dagq.toml`
/// of the worktree dagq runs in is never read in the main checkout's place.
pub fn main_checkout_of(path: &Path) -> Result<PathBuf> {
    let git = git_executable()?;
    let common_dir = git_common_dir(path)?;
    let listing =
        output(
            Command::new(&git)
                .arg("-C")
                .arg(path)
                .args(["worktree", "list", "--porcelain"]),
        )?;
    let block = listing.split("\n\n").next().unwrap_or_default();
    let first = block
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .with_context(|| {
            format!(
                "git worktree list printed no worktree for {}",
                path.display()
            )
        })?;
    let first = first.canonicalize().unwrap_or(first);
    // A bare repository lists its Git directory. A worktree apart from it
    // is a checkout even when Git, run in a Git directory whose config has
    // no `core.bare`, marks it bare.
    if first != common_dir {
        return Ok(first);
    }
    ensure!(
        !block.lines().any(|line| line == "bare"),
        "the repository {} is bare: its main worktree has no files, so there is no main checkout to read dagq.toml from; use a clone that is not bare, so that its main worktree is a checkout, and run dagq there",
        common_dir.display()
    );
    // Git lists the Git directory for a main worktree it does not know:
    // only that worktree itself can say where it is.
    let (status, stdout, _) = capture(
        Command::new(&git).arg("-C").arg(path).args([
            "rev-parse",
            "--is-inside-work-tree",
            "--absolute-git-dir",
            "--show-toplevel",
        ]),
        Duration::from_secs(30),
    )?;
    let mut lines = stdout.lines();
    if status.success()
        && lines.next() == Some("true")
        && lines
            .next()
            .and_then(|dir| Path::new(dir).canonicalize().ok())
            .is_some_and(|dir| dir == common_dir)
        && let Some(root) = lines.next()
    {
        return Ok(PathBuf::from(root).canonicalize()?);
    }
    bail!(
        "the repository whose Git directory is {} keeps it apart from its main worktree (git init --separate-git-dir), and Git does not record where that worktree is, so the main checkout to read dagq.toml from is unknown from {}; run the command in the main worktree",
        common_dir.display(),
        path.display()
    )
}

/// The checkout that names, in a notification, the repository a queue is
/// bound to (`binding`, its Git common directory): its main checkout
/// ([`main_checkout_of`]), else the bound path itself; `checkout`, where
/// the command runs, for a queue bound to none.
pub fn naming_checkout(binding: Option<&Path>, checkout: &Path) -> PathBuf {
    match binding {
        Some(dir) => main_checkout_of(dir).unwrap_or_else(|_| dir.to_path_buf()),
        None => checkout.to_path_buf(),
    }
}

/// The full message of `commit` in the repository whose Git directory is
/// `git_dir`; `None` when Git cannot read it there.
pub fn commit_message(git_dir: &Path, commit: &str) -> Option<String> {
    let git = git_executable().ok()?;
    output(Command::new(&git).arg("--git-dir").arg(git_dir).args([
        "show",
        "-s",
        "--format=%B",
        commit,
        "--",
    ]))
    .ok()
    .map(|message| message.trim_end().to_owned())
}

/// The non-empty fields of Git's NUL-separated `-z` output.
fn split_nul(text: &str) -> Vec<String> {
    text.split('\0')
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The commit Git printed (a full object ID, surrounded by whitespace).
fn object_id(text: &str, field: &'static str) -> Result<CommitSha> {
    Ok(CommitSha::parse(text.trim(), field)?)
}

pub use crate::application::DiffNumbers;

/// Whether the checkout at `dir` is dagq's source (ADR-t614-1), from its
/// `Cargo.toml` as it is now; a missing or unreadable file is not.
pub fn is_dagq_source(dir: &Path) -> bool {
    crate::domain::source_repository::is_source(
        fs::read_to_string(dir.join("Cargo.toml")).ok().as_deref(),
    )
}

#[derive(Clone)]
pub struct GitRepository {
    pub root: PathBuf,
    pub common_dir: PathBuf,
    /// The main checkout, whose `dagq.toml` names the landing branch
    /// ([`main_checkout_of`]), or why the repository has none.
    checkout: std::result::Result<PathBuf, String>,
    git: PathBuf,
}

impl GitRepository {
    pub fn inspect(path: &Path) -> Result<Self> {
        let git = git_executable()?;
        let root = PathBuf::from(
            output(
                Command::new(&git)
                    .arg("-C")
                    .arg(path)
                    .args(["rev-parse", "--show-toplevel"]),
            )?
            .trim(),
        )
        .canonicalize()?;
        let common_dir = git_common_dir(&root)?;
        let checkout = main_checkout_of(&root).map_err(|error| format!("{error:#}"));
        Ok(Self {
            root,
            common_dir,
            checkout,
            git,
        })
    }

    /// The main checkout ([`main_checkout_of`]), whose `dagq.toml` dagq
    /// reads; an error for a repository that has none (a bare one, or one
    /// with a separate Git directory inspected from a linked worktree).
    pub fn checkout(&self) -> Result<&Path> {
        self.checkout
            .as_deref()
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    /// `[repository]` of the main checkout's `dagq.toml` (ADR-t615-1),
    /// with its `branch` and `remote` checked as Git names; an error points
    /// at `[repository]`.
    pub fn repository_config(&self) -> Result<RepositoryConfig> {
        let config = load_repository_config(self.checkout()?).with_context(|| {
            format!(
                "cannot resolve the landing branch; {}",
                landing_branch::HINT
            )
        })?;
        if let Some(name) = &config.branch {
            let (status, _, _) = capture(
                Command::new(&self.git).args(["check-ref-format", "--branch", name]),
                Duration::from_secs(30),
            )?;
            ensure!(
                status.success(),
                "[repository] branch = {name:?} of dagq.toml is not a valid branch name; {}",
                landing_branch::HINT
            );
        }
        if let Some(name) = &config.remote {
            // Git's own test of a remote name.
            let (status, _, _) = capture(
                Command::new(&self.git)
                    .args(["check-ref-format", &format!("refs/remotes/{name}/test")]),
                Duration::from_secs(30),
            )?;
            ensure!(
                status.success(),
                "[repository] remote = {name:?} of dagq.toml is not a valid remote name; name the remote the landing is pushed to as remote = \"<name>\" under [repository] in the dagq.toml of the repository's main checkout"
            );
        }
        Ok(config)
    }

    /// The branch runs land on (ADR-t615-1), resolved now from the main
    /// checkout's `dagq.toml` and the repository's branches; an error says
    /// what could not be resolved and points at `[repository]`.
    pub fn landing_branch(&self) -> Result<LandingBranch> {
        self.resolve_landing_branch(&self.repository_config()?)
    }

    /// A stamp of what [`Self::landing_branch`] reads, from the files'
    /// metadata alone (task 1078): the main checkout's `dagq.toml`, the
    /// repository's `config`, `packed-refs` and reftable list, the push
    /// remote's HEAD, and the loose refs of the branches the resolution
    /// may test (the configured one, or the one the remote's HEAD names,
    /// `main` and `master`). Which remote and which branches come from the
    /// files as they are now, so a change of them changes the paths too.
    /// `None` without a main checkout.
    pub fn landing_branch_stamp(&self) -> Option<LandingBranchStamp> {
        let checkout = self.checkout().ok()?;
        // A file that does not parse resolves to an error; its own stamp
        // below tells when it changes.
        let config = load_repository_config(checkout).unwrap_or_default();
        let remote = config.remote();
        let common = &self.common_dir;
        let remote_head = common.join("refs/remotes").join(remote).join("HEAD");
        let branches = match &config.branch {
            Some(name) => vec![name.clone()],
            None => {
                let prefix = format!("ref: refs/remotes/{remote}/");
                fs::read_to_string(&remote_head)
                    .ok()
                    .and_then(|text| text.trim().strip_prefix(&prefix).map(str::to_owned))
                    .into_iter()
                    .chain(["main".to_owned(), "master".to_owned()])
                    .collect()
            }
        };
        let paths = [
            checkout.join(crate::infrastructure::run_env::CONFIG_FILE_NAME),
            common.join("config"),
            common.join("packed-refs"),
            common.join("reftable/tables.list"),
            remote_head,
        ]
        .into_iter()
        .chain(
            branches
                .iter()
                .map(|name| common.join("refs/heads").join(name)),
        );
        Some(LandingBranchStamp(
            paths
                .map(|path| {
                    let stamp = file_stamp(&path);
                    (path, stamp)
                })
                .collect(),
        ))
    }

    fn resolve_landing_branch(&self, config: &RepositoryConfig) -> Result<LandingBranch> {
        let remote = config.remote();
        let (status, stdout, _) = capture(
            self.git_root().args([
                "symbolic-ref",
                "--quiet",
                &format!("refs/remotes/{remote}/HEAD"),
            ]),
            Duration::from_secs(30),
        )?;
        let prefix = format!("refs/remotes/{remote}/");
        let remote_head = status
            .success()
            .then(|| stdout.trim().strip_prefix(&prefix).map(str::to_owned))
            .flatten();
        landing_branch::resolve(
            config.branch.as_deref(),
            remote,
            remote_head.as_deref(),
            &mut |name| {
                let (status, _, stderr) = capture(
                    self.git_root().args([
                        "show-ref",
                        "--verify",
                        "--quiet",
                        &format!("refs/heads/{name}"),
                    ]),
                    Duration::from_secs(30),
                )?;
                match status.code() {
                    Some(0) => Ok(true),
                    Some(1) => Ok(false),
                    _ => bail!("git show-ref failed ({status}): {stderr}"),
                }
            },
        )
    }

    /// The landing branch and the push (ADR-t615-1), as `up`'s preflight
    /// and `doctor` resolve them: an error when the branch does not
    /// resolve or the landing would be pushed to a configured remote the
    /// repository does not have.
    pub fn repository_settings(&self) -> Result<RepositorySettings> {
        let settings = self.resolve_repository_settings()?;
        settings.push.check()?;
        Ok(settings)
    }

    /// [`Self::repository_settings`] without the check of the push remote,
    /// for `doctor` to show the fields next to that error.
    pub fn resolve_repository_settings(&self) -> Result<RepositorySettings> {
        let config = self.repository_config()?;
        let branch = self.resolve_landing_branch(&config)?;
        let push = PushTarget::new(&config, self.has_remote(config.remote())?);
        Ok(RepositorySettings { branch, push })
    }

    /// Whether the repository is dagq's source (ADR-t614-1), judged now
    /// from the main checkout's `Cargo.toml`.
    pub fn is_dagq_source(&self) -> bool {
        self.checkout().is_ok_and(is_dagq_source)
    }

    /// `git -C <root>`.
    fn git_root(&self) -> Command {
        let mut command = Command::new(&self.git);
        command.arg("-C").arg(&self.root);
        command
    }

    /// A read-only `git -C <worktree>` for a worktree a session may be
    /// working in. `GIT_OPTIONAL_LOCKS=0` keeps `git status` from taking
    /// `index.lock` to write back the index it refreshed, which would make
    /// the session's own `git add` / `rebase --continue` fail on the lock
    /// or have its index overwritten by a stale one.
    fn read_worktree(&self, worktree: &Path) -> Command {
        let mut command = Command::new(&self.git);
        command
            .env("GIT_OPTIONAL_LOCKS", "0")
            .arg("-C")
            .arg(worktree);
        command
    }

    /// The landing branch's current commit, resolved and read again so
    /// that a task unblocked by an integration starts from the landing
    /// branch that contains its predecessor.
    pub fn main_head(&self) -> Result<CommitSha> {
        let branch = self.landing_branch()?;
        object_id(
            &output(self.git_root().args([
                "rev-parse",
                "--verify",
                &format!("{}^{{commit}}", branch.reference()),
            ]))?,
            "landing branch commit",
        )
    }

    /// Read the blob from the object store, never from a checkout or index.
    pub fn file_in(&self, commit: &str, path: &str) -> Result<Option<String>> {
        let entry =
            output(
                self.git_root()
                    .args(["ls-tree", "--name-only", "-z", commit, "--", path]),
            )?;
        if entry.is_empty() {
            return Ok(None);
        }
        output(
            self.git_root()
                .args(["cat-file", "blob", &format!("{commit}:{path}")]),
        )
        .map(Some)
    }

    /// Symbolic HEAD of a worktree, or None when detached.
    pub fn current_branch(&self, worktree: &Path) -> Result<Option<String>> {
        let (status, stdout, stderr) = capture(
            self.read_worktree(worktree)
                .args(["symbolic-ref", "--quiet", "HEAD"]),
            Duration::from_secs(30),
        )?;
        match status.code() {
            Some(0) => Ok(Some(stdout.trim().to_owned())),
            Some(1) => Ok(None),
            _ => bail!("git symbolic-ref failed ({status}): {stderr}"),
        }
    }

    pub fn head(&self, worktree: &Path) -> Result<CommitSha> {
        object_id(
            &output(
                self.read_worktree(worktree)
                    .args(["rev-parse", "--verify", "HEAD^{commit}"]),
            )?,
            "HEAD",
        )
    }

    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let (status, _, stderr) = capture(
            Command::new(&self.git).arg("-C").arg(&self.root).args([
                "merge-base",
                "--is-ancestor",
                ancestor,
                descendant,
            ]),
            Duration::from_secs(30),
        )?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!("git merge-base failed ({status}): {stderr}"),
        }
    }

    /// `git merge-base <a> <b>`: their best common ancestor, `None` when
    /// they share no history.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<CommitSha>> {
        let (status, stdout, stderr) = capture(
            Command::new(&self.git)
                .arg("-C")
                .arg(&self.root)
                .args(["merge-base", a, b]),
            Duration::from_secs(30),
        )?;
        match status.code() {
            Some(0) => Ok(Some(object_id(&stdout, "merge base")?)),
            Some(1) => Ok(None),
            _ => bail!("git merge-base failed ({status}): {stderr}"),
        }
    }

    /// The paths that conflict when `head` is merged with `main` (over
    /// their merge base), judged by `git merge-tree --write-tree` in the
    /// object store alone: no worktree, index or ref moves (ADR-0027
    /// decision 4). Empty when they merge cleanly.
    pub fn merge_conflicts(&self, main: &str, head: &str) -> Result<Vec<String>> {
        Ok(self.merged_tree(main, head)?.err().unwrap_or_default())
    }

    /// The tree `git merge-tree --write-tree` makes of `head` merged with
    /// `main`, in the object store alone: `Ok(tree)` when they merge
    /// cleanly, `Err(paths)` with each conflicted path once otherwise.
    pub fn merged_tree(
        &self,
        main: &str,
        head: &str,
    ) -> Result<std::result::Result<String, Vec<String>>> {
        let (status, stdout, stderr) = capture(
            Command::new(&self.git).arg("-C").arg(&self.root).args([
                "merge-tree",
                "--write-tree",
                "--name-only",
                "--no-messages",
                "-z",
                main,
                head,
            ]),
            Duration::from_secs(5 * 60),
        )?;
        match status.code() {
            // The tree's OID, NUL-terminated.
            Some(0) => Ok(Ok(stdout
                .split('\0')
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned())),
            // The tree's OID, then each conflicted path, NUL-terminated and
            // never quoted.
            Some(1) => {
                let mut paths: Vec<String> = Vec::new();
                for path in stdout.split('\0').skip(1).take_while(|p| !p.is_empty()) {
                    if !paths.iter().any(|p| p == path) {
                        paths.push(path.to_owned());
                    }
                }
                Ok(Err(paths))
            }
            _ => bail!("git merge-tree failed ({status}): {stderr}"),
        }
    }

    /// Check `commit` out detached in the scratch worktree at `path` (the
    /// landing recheck's, ADR-0068 decision 2), adding it when it is not a
    /// worktree yet and dropping whatever its last use left in it.
    pub fn checkout_scratch(&self, path: &Path, commit: &str) -> Result<()> {
        if !path.join(".git").exists() {
            // A directory left without its worktree, or a record left
            // without its directory, is cleared first.
            if path.exists() {
                fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
            }
            output(
                Command::new(&self.git)
                    .arg("-C")
                    .arg(&self.root)
                    .args(["worktree", "prune"]),
            )?;
            output(
                Command::new(&self.git)
                    .arg("-C")
                    .arg(&self.root)
                    .args(["worktree", "add", "--detach", "--force"])
                    .arg(path)
                    .arg(commit),
            )?;
            return Ok(());
        }
        output(
            Command::new(&self.git)
                .arg("-C")
                .arg(path)
                .args(["checkout", "--detach", "--force", "--quiet", commit]),
        )?;
        output(
            Command::new(&self.git)
                .arg("-C")
                .arg(path)
                .args(["clean", "-ffdxq"]),
        )?;
        Ok(())
    }

    /// The commits of main's first-parent line since `since` (unix
    /// seconds), oldest first, with the paths each changed (renames
    /// followed), and the paths main has now: what `conflict_hotspots`
    /// counts landings and tells deleted and renamed files by.
    pub fn main_history(&self, since: i64) -> Result<MainHistory> {
        let branch = self.landing_branch()?.reference();
        let log = review_output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "log",
            "-z",
            "--first-parent",
            "--reverse",
            "--diff-merges=first-parent",
            "-M",
            "--name-status",
            "--format=%x01%ct",
            &format!("--max-age={}", since.max(0)),
            &branch,
            "--",
        ]))?;
        let tree = review_output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "ls-tree",
            "-r",
            "--name-only",
            "-z",
            &branch,
        ]))?;
        Ok(MainHistory {
            commits: parse_main_log(&log),
            paths: tree
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }

    /// The paths each of `commits` changed against its first parent
    /// (`--no-renames`: both sides of a rename), by the commit's full ID:
    /// what the areas of the landed runs are read from (ADR-t980-1). One
    /// `git log --no-walk` per [`LANDED_BATCH`] commits, not one per
    /// commit; a commit the repository does not have is left out.
    pub fn landed_changes(&self, commits: &[String]) -> Result<HashMap<String, Vec<String>>> {
        let mut changes = HashMap::new();
        for batch in commits.chunks(LANDED_BATCH) {
            let log = review_output(
                Command::new(&self.git)
                    .arg("-C")
                    .arg(&self.root)
                    .args([
                        "log",
                        "-z",
                        "--ignore-missing",
                        "--no-walk=unsorted",
                        "--diff-merges=first-parent",
                        "--no-renames",
                        "--name-only",
                        "--format=%x01%H",
                    ])
                    .args(batch)
                    .arg("--"),
            )?;
            changes.extend(parse_landed_log(&log));
        }
        Ok(changes)
    }

    /// Porcelain status including untracked files; empty means clean.
    pub fn status(&self, worktree: &Path) -> Result<String> {
        output(self.read_worktree(worktree).args([
            "status",
            "--porcelain",
            "--untracked-files=all",
        ]))
    }

    pub fn create_worktree(&self, run: &TaskRun) -> Result<String> {
        output(
            Command::new(&self.git)
                .arg("-C")
                .arg(&self.root)
                .args(["worktree", "add", "-b"])
                .arg(run.branch().context("missing branch")?)
                .arg(run.worktree_path().context("missing worktree")?)
                .arg(run.base_commit().as_str()),
        )
    }

    /// Whether a `git rebase` was left half-done in the worktree (by a
    /// crashed landing or an unfinished session).
    pub fn rebase_in_progress(&self, worktree: &Path) -> Result<bool> {
        let paths = output(self.read_worktree(worktree).args([
            "rev-parse",
            "--git-path",
            "rebase-merge",
            "--git-path",
            "rebase-apply",
        ]))?;
        Ok(paths.lines().any(|p| worktree.join(p.trim()).exists()))
    }

    pub fn rebase_abort(&self, worktree: &Path) -> Result<()> {
        output(
            Command::new(&self.git)
                .arg("-C")
                .arg(worktree)
                .args(["rebase", "--abort"]),
        )?;
        Ok(())
    }

    /// Rebase the worktree's branch onto `onto`. `Ok(Err(output))` is a
    /// conflict (or any other rebase failure) with the rebase still in
    /// progress if Git left one; the caller decides whether to abort it.
    pub fn rebase(&self, worktree: &Path, onto: &str) -> Result<std::result::Result<(), String>> {
        let (status, stdout, stderr) = capture(
            Command::new(&self.git)
                .arg("-C")
                .arg(worktree)
                .env("GIT_TERMINAL_PROMPT", "0")
                .args(["rebase", "--no-autostash", "--no-verify", onto]),
            Duration::from_secs(10 * 60),
        )?;
        Ok(if status.success() {
            Ok(())
        } else {
            Err(format!("{stdout}{stderr}"))
        })
    }

    /// Paths with unresolved conflicts in the worktree. The plumbing
    /// `diff-files`, since porcelain `git diff` writes a refreshed index
    /// back even under `GIT_OPTIONAL_LOCKS=0`.
    pub fn conflicted_files(&self, worktree: &Path) -> Result<Vec<String>> {
        Ok(output(self.read_worktree(worktree).args([
            "diff-files",
            "--name-only",
            "--diff-filter=U",
        ]))?
        .lines()
        .map(str::to_owned)
        .collect())
    }

    /// `git log --oneline <base>..<head>`: the commits a run added. Messages
    /// that are not UTF-8 are read lossily.
    pub fn log_oneline(&self, base: &str, head: &str) -> Result<String> {
        review_output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "log",
            "--oneline",
            "--no-decorate",
            "--no-color",
            &format!("{base}..{head}"),
        ]))
    }

    /// The task IDs in the `Dagq-Task` trailers of `<base>..<head>`, oldest
    /// landing first: the tasks `integrate` put on `main` since `base`.
    pub fn landed_task_ids(&self, base: &str, head: &str) -> Result<Vec<TaskId>> {
        let text = review_output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "log",
            "--reverse",
            "--format=%(trailers:key=Dagq-Task,valueonly)",
            &format!("{base}..{head}"),
        ]))?;
        let mut ids: Vec<TaskId> = Vec::new();
        for id in text
            .lines()
            .filter_map(|line| line.trim().parse().ok().map(TaskId::new))
        {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// The commit of `<base>..<head>`'s first-parent history whose
    /// `Dagq-Run` trailer names `run`, with its first parent: where
    /// `integrate` put that run on `main` (task 1118). `None` when no
    /// landing of it is there.
    pub fn landed_run_commit(
        &self,
        base: &str,
        head: &str,
        run: &str,
    ) -> Result<Option<(CommitSha, CommitSha)>> {
        let text = review_output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "log",
            "--first-parent",
            "--format=%H%x1f%P%x1f%(trailers:key=Dagq-Run,valueonly)%x1e",
            &format!("{base}..{head}"),
        ]))?;
        for record in text.split('\x1e') {
            let mut fields = record.trim_start_matches('\n').split('\x1f');
            let (Some(commit), Some(parents), Some(trailers)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if trailers.lines().any(|value| value.trim() == run) {
                let parent = parents.split_whitespace().next().unwrap_or_default();
                return Ok(Some((
                    object_id(commit, "landed commit")?,
                    object_id(parent, "landed commit's parent")?,
                )));
            }
        }
        Ok(None)
    }

    /// `git diff <args> <base>...<head>`: the change since the merge base,
    /// without color, external diff drivers or textconv filters.
    fn diff_since(&self, base: &str, head: &str, args: &[&str]) -> Command {
        let mut command = Command::new(&self.git);
        command
            .arg("-C")
            .arg(&self.root)
            .args(["diff", "--no-color", "--no-ext-diff", "--no-textconv"])
            .args(args)
            .arg(format!("{base}...{head}"))
            .arg("--");
        command
    }

    /// `git diff --stat <base>...<head>`, read lossily.
    pub fn diff_stat(&self, base: &str, head: &str) -> Result<String> {
        review_output(&mut self.diff_since(base, head, &["--stat"]))
    }

    /// Full `git diff <base>...<head>` appended to `file` as Git's raw bytes,
    /// whatever the encoding of the files; the diff never passes through memory.
    pub fn diff_to(&self, base: &str, head: &str, file: &fs::File) -> Result<()> {
        review_output_to(&mut self.diff_since(base, head, &[]), file)
    }

    /// Files changed, lines inserted and lines deleted in
    /// `<base>...<head>`, summed from `--numstat` (binary files count as a
    /// changed file with no lines).
    pub fn diff_numbers(&self, base: &str, head: &str) -> Result<DiffNumbers> {
        let numstat = review_output(&mut self.diff_since(base, head, &["--numstat"]))?;
        let mut numbers = DiffNumbers::default();
        for line in numstat.lines().filter(|line| !line.is_empty()) {
            let mut fields = line.split('\t');
            numbers.files_changed += 1;
            numbers.insertions += fields.next().and_then(|n| n.parse().ok()).unwrap_or(0);
            numbers.deletions += fields.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        }
        Ok(numbers)
    }

    /// The paths whose content differs between the trees of `from` and
    /// `to` (`git diff --name-only --no-renames`): both sides of a rename,
    /// never quoted.
    pub fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let names = output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            from,
            to,
            "--",
        ]))?;
        Ok(names
            .split('\0')
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// The paths `to` adds over `from` (`git diff --diff-filter=A
    /// --no-renames`), never quoted.
    pub fn added_paths(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let names = output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--diff-filter=A",
            "--no-ext-diff",
            "--no-textconv",
            from,
            to,
            "--",
        ]))?;
        Ok(split_nul(&names))
    }

    /// The paths of the files directly in `directory` of `commit`'s tree,
    /// never quoted; none when the directory is not there.
    pub fn paths_in(&self, commit: &str, directory: &str) -> Result<Vec<String>> {
        let names = output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "ls-tree",
            "--name-only",
            "-z",
            commit,
            "--",
            &format!("{directory}/"),
        ]))?;
        Ok(split_nul(&names))
    }

    /// Which of `paths` contain `needle` in `commit`'s tree (`git grep -l
    /// -F`, binary files included).
    pub fn paths_containing(
        &self,
        commit: &str,
        needle: &str,
        paths: &[String],
    ) -> Result<Vec<String>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let (status, stdout, stderr) = capture(
            Command::new(&self.git)
                .arg("-C")
                .arg(&self.root)
                // The paths are names, not patterns.
                .arg("--literal-pathspecs")
                .args([
                    "grep",
                    "-l",
                    "-z",
                    "-F",
                    "--no-color",
                    "-e",
                    needle,
                    commit,
                    "--",
                ])
                .args(paths),
            OUTPUT_TIMEOUT,
        )?;
        // 1 is "no match".
        match status.code() {
            Some(0) => (),
            Some(1) if stderr.trim().is_empty() => return Ok(Vec::new()),
            _ => bail!("git grep failed ({status}): {stderr}"),
        }
        let prefix = format!("{commit}:");
        Ok(split_nul(&stdout)
            .into_iter()
            .map(|path| {
                path.strip_prefix(&prefix)
                    .map(str::to_owned)
                    .unwrap_or(path)
            })
            .collect())
    }

    /// `git mv` `from` to `to` in the clean `worktree` and commit that on its
    /// branch with `paragraphs` as the message; the new head. When Git
    /// refuses the commit (a signing or identity failure, or a failing
    /// `prepare-commit-msg` hook, which `--no-verify` does not skip), the
    /// rename is undone and `Ok(Err(output))` carries what Git said.
    pub fn rename_and_commit(
        &self,
        worktree: &Path,
        from: &str,
        to: &str,
        paragraphs: &[String],
    ) -> Result<std::result::Result<CommitSha, String>> {
        output(
            Command::new(&self.git)
                .arg("-C")
                .arg(worktree)
                .args(["mv", "--", from, to]),
        )?;
        // The worktree was clean before the move, so the index holds the
        // rename alone.
        let mut commit = Command::new(&self.git);
        commit
            .arg("-C")
            .arg(worktree)
            .args(["commit", "-q", "--no-verify"]);
        for paragraph in paragraphs {
            commit.arg("-m").arg(paragraph);
        }
        // A commit that did not finish (a signing program waiting for a
        // passphrase until the timeout) is refused like one Git turned down.
        let refused = match capture(&mut commit, OUTPUT_TIMEOUT) {
            Ok((status, _, _)) if status.success() => return Ok(Ok(self.head(worktree)?)),
            Ok((status, stdout, stderr)) => format!("{status}: {stdout}{stderr}"),
            Err(error) => format!("{error:#}"),
        }
        .trim()
        .to_owned();
        // The worktree was clean at HEAD before the move, and the refused
        // commit left HEAD where it was, so resetting to it undoes the move
        // alone.
        output(
            Command::new(&self.git)
                .arg("-C")
                .arg(worktree)
                .args(["reset", "-q", "--hard", "HEAD"]),
        )
        .with_context(|| {
            format!(
                "git commit of the rename {from} -> {to} failed ({refused}); undoing the rename also failed"
            )
        })?;
        Ok(Err(refused))
    }

    pub fn tree_of(&self, commit: &str) -> Result<String> {
        Ok(
            output(Command::new(&self.git).arg("-C").arg(&self.root).args([
                "rev-parse",
                "--verify",
                &format!("{commit}^{{tree}}"),
            ]))?
            .trim()
            .to_owned(),
        )
    }

    /// One commit with `tree` on top of `parent`; `paragraphs` become the
    /// message separated by blank lines. No hook runs and no checkout changes.
    pub fn commit_tree(
        &self,
        tree: &str,
        parent: &str,
        paragraphs: &[String],
    ) -> Result<CommitSha> {
        let mut command = Command::new(&self.git);
        command
            .arg("-C")
            .arg(&self.root)
            .args(["commit-tree", tree, "-p", parent]);
        for paragraph in paragraphs {
            command.arg("-m").arg(paragraph);
        }
        object_id(&output(&mut command)?, "new commit")
    }

    pub fn update_ref(&self, name: &str, value: &str) -> Result<()> {
        output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "update-ref",
            name,
            value,
        ]))?;
        Ok(())
    }

    pub fn ref_exists(&self, name: &str) -> Result<Option<CommitSha>> {
        let (status, stdout, stderr) = capture(
            Command::new(&self.git).arg("-C").arg(&self.root).args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{name}^{{commit}}"),
            ]),
            Duration::from_secs(30),
        )?;
        match status.code() {
            Some(0) => Ok(Some(object_id(&stdout, "ref")?)),
            Some(1) => Ok(None),
            _ => bail!("git rev-parse failed ({status}): {stderr}"),
        }
    }

    /// `git worktree list --porcelain` as (path, block) pairs, the main
    /// working tree first.
    fn worktrees(&self) -> Result<Vec<(PathBuf, String)>> {
        let listing = output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "worktree",
            "list",
            "--porcelain",
        ]))?;
        Ok(listing
            .split("\n\n")
            .filter_map(|block| {
                let path = block.lines().next()?.strip_prefix("worktree ")?;
                Some((PathBuf::from(path), block.to_owned()))
            })
            .collect())
    }

    /// The worktree that has the landing branch checked out, if any.
    pub fn main_checkout(&self) -> Result<Option<PathBuf>> {
        self.checkout_of(&self.landing_branch()?)
    }

    /// The worktree that has `branch` checked out, if any.
    fn checkout_of(&self, branch: &LandingBranch) -> Result<Option<PathBuf>> {
        let line = format!("branch {}", branch.reference());
        Ok(self
            .worktrees()?
            .into_iter()
            .find(|(_, block)| block.lines().any(|l| l == line))
            .map(|(path, _)| path))
    }

    /// The main working tree, from which linked worktrees are administered;
    /// `root` may itself be the linked worktree being removed.
    fn primary_worktree(&self) -> Result<PathBuf> {
        Ok(self
            .worktrees()?
            .into_iter()
            .next()
            .map(|(path, _)| path)
            .unwrap_or_else(|| self.root.clone()))
    }

    /// Fast-forward `branch`, the landing branch resolved once when the
    /// landing began, from `from` to `to`. Where it is checked out the
    /// merge goes through that worktree so its index and files move with
    /// the ref (local changes that collide make it fail); otherwise the ref
    /// is updated with `from` as the expected old value.
    pub fn advance_main(&self, branch: &LandingBranch, from: &str, to: &str) -> Result<()> {
        match self.checkout_of(branch)? {
            Some(checkout) => {
                output(
                    Command::new(&self.git)
                        .arg("-C")
                        .arg(&checkout)
                        .env("GIT_TERMINAL_PROMPT", "0")
                        .args(["merge", "--ff-only", to]),
                )
                .with_context(|| {
                    format!("fast-forward {} in {}", branch.name, checkout.display())
                })?;
            }
            None => {
                output(
                    self.git_root()
                        .args(["update-ref", &branch.reference(), to, from]),
                )?;
            }
        }
        Ok(())
    }

    /// Point the repository's record of a linked worktree back at `worktree`
    /// after the directory moved (`git worktree repair`); a no-op otherwise.
    pub fn repair_worktree(&self, worktree: &Path) -> Result<()> {
        let primary = self.primary_worktree()?;
        output(
            Command::new(&self.git)
                .arg("-C")
                .arg(&primary)
                .args(["worktree", "repair"])
                .arg(worktree),
        )
        .with_context(|| {
            format!(
                "repair worktree {} (was its record pruned after the queue moved?)",
                worktree.display()
            )
        })?;
        Ok(())
    }

    /// Forget the worktrees whose directory is gone (`git worktree
    /// prune`), administered from the main working tree.
    pub fn prune_worktrees(&self) -> Result<()> {
        let primary = self.primary_worktree()?;
        output(
            Command::new(&self.git)
                .arg("-C")
                .arg(&primary)
                .args(["worktree", "prune"]),
        )
        .context("prune the worktrees whose directory is gone")?;
        Ok(())
    }

    /// Remove a run's worktree and branch. Administered from the main
    /// working tree, since `root` may be the worktree being removed. A
    /// branch already gone is left at that.
    pub fn remove_worktree_and_branch(&self, worktree: &Path, branch: &str) -> Result<()> {
        let primary = self.primary_worktree()?;
        output(
            Command::new(&self.git)
                .arg("-C")
                .arg(&primary)
                .args(["worktree", "remove", "--force"])
                .arg(worktree),
        )?;
        self.delete_branch_from(&primary, branch)
    }

    /// The local branches, by their short name.
    pub fn branches(&self) -> Result<Vec<String>> {
        let listed = output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads/",
        ]))?;
        Ok(listed
            .lines()
            .filter_map(|line| line.trim().strip_prefix("refs/heads/"))
            .map(str::to_owned)
            .collect())
    }

    /// Delete a local branch, administered from the main working tree; one
    /// already gone is left at that.
    pub fn delete_branch(&self, branch: &str) -> Result<()> {
        self.delete_branch_from(&self.primary_worktree()?, branch)
    }

    /// [`Self::delete_branch`] from `primary`, resolved before `root` may
    /// have been removed.
    fn delete_branch_from(&self, primary: &Path, branch: &str) -> Result<()> {
        let reference = format!("refs/heads/{}", branch.trim_start_matches("refs/heads/"));
        let (status, _, _) = capture(
            Command::new(&self.git)
                .arg("-C")
                .arg(primary)
                .args(["show-ref", "--verify", "--quiet", &reference]),
            OUTPUT_TIMEOUT,
        )?;
        if status.success() {
            output(
                Command::new(&self.git)
                    .arg("-C")
                    .arg(primary)
                    .args(["branch", "-D", branch]),
            )?;
        }
        Ok(())
    }

    /// Whether Git tracks any file at or under `path` in `worktree`.
    pub fn tracks(&self, worktree: &Path, path: &str) -> Result<bool> {
        let listed = output(
            Command::new(&self.git)
                .arg("-C")
                .arg(worktree)
                .args(["ls-files", "--"])
                .arg(path),
        )?;
        Ok(!listed.trim().is_empty())
    }
}

/// How long `integrate` waits for `git push` before counting it as failed.
const PUSH_TIMEOUT: Duration = Duration::from_secs(300);

/// The commits [`GitRepository::landed_changes`] reads per `git log`.
const LANDED_BATCH: usize = 500;

/// `git log -z --name-only --format=%x01%H`: each commit's full ID and
/// the paths it changed.
fn parse_landed_log(log: &str) -> HashMap<String, Vec<String>> {
    let mut changes: HashMap<String, Vec<String>> = HashMap::new();
    let mut current: Option<String> = None;
    for field in log.split('\0').map(|field| field.trim_start_matches('\n')) {
        if let Some(commit) = field.strip_prefix('\u{1}') {
            let commit = commit.trim().to_owned();
            changes.entry(commit.clone()).or_default();
            current = Some(commit);
        } else if !field.is_empty()
            && let Some(commit) = &current
        {
            changes
                .get_mut(commit)
                .expect("the commit was listed")
                .push(field.to_owned());
        }
    }
    changes
}

/// `git log -z --format=%x01%ct --name-status` as commits: NUL-separated
/// fields where a `\x01<unix seconds>` field starts a commit, then each
/// change is a status field (`M`, `D`, ... after a newline) and its path,
/// or `R100` / `C75` and the old and new paths. Paths are never quoted.
fn parse_main_log(log: &str) -> Vec<MainCommit> {
    let mut commits: Vec<MainCommit> = Vec::new();
    let mut fields = log.split('\0').map(|field| field.trim_start_matches('\n'));
    while let Some(field) = fields.next() {
        if let Some(at) = field.strip_prefix('\u{1}') {
            if let Ok(at) = at.trim().parse() {
                commits.push(MainCommit {
                    at,
                    changes: Vec::new(),
                });
            }
            continue;
        }
        let Some(status) = field.chars().next() else {
            continue;
        };
        let change = match status {
            'R' | 'C' => {
                let (Some(from), Some(to)) = (fields.next(), fields.next()) else {
                    break;
                };
                MainChange {
                    path: to.to_owned(),
                    from: (status == 'R').then(|| from.to_owned()),
                    deleted: false,
                }
            }
            _ => {
                let Some(path) = fields.next() else { break };
                MainChange {
                    path: path.to_owned(),
                    from: None,
                    deleted: status == 'D',
                }
            }
        };
        if let Some(commit) = commits.last_mut() {
            commit.changes.push(change);
        }
    }
    commits
}

/// The Git port over the inherent methods above, which callers that hold a
/// `GitRepository` keep using directly.
impl Repository for GitRepository {
    fn landing_branch(&self) -> Result<LandingBranch> {
        GitRepository::landing_branch(self)
    }
    fn landing_branch_stamp(&self) -> Option<LandingBranchStamp> {
        GitRepository::landing_branch_stamp(self)
    }
    fn repository_config(&self) -> Result<RepositoryConfig> {
        GitRepository::repository_config(self)
    }
    fn is_dagq_source(&self) -> bool {
        GitRepository::is_dagq_source(self)
    }
    fn main_head(&self) -> Result<CommitSha> {
        GitRepository::main_head(self)
    }
    fn file_in(&self, commit: &str, path: &str) -> Result<Option<String>> {
        GitRepository::file_in(self, commit, path)
    }
    fn main_history(&self, since: i64) -> Result<MainHistory> {
        GitRepository::main_history(self, since)
    }
    fn current_branch(&self, worktree: &Path) -> Result<Option<String>> {
        GitRepository::current_branch(self, worktree)
    }
    fn head(&self, worktree: &Path) -> Result<CommitSha> {
        GitRepository::head(self, worktree)
    }
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        GitRepository::is_ancestor(self, ancestor, descendant)
    }
    fn merge_base(&self, a: &str, b: &str) -> Result<Option<CommitSha>> {
        GitRepository::merge_base(self, a, b)
    }
    fn status(&self, worktree: &Path) -> Result<String> {
        GitRepository::status(self, worktree)
    }
    fn rebase_in_progress(&self, worktree: &Path) -> Result<bool> {
        GitRepository::rebase_in_progress(self, worktree)
    }
    fn rebase_abort(&self, worktree: &Path) -> Result<()> {
        GitRepository::rebase_abort(self, worktree)
    }
    fn rebase(&self, worktree: &Path, onto: &str) -> Result<std::result::Result<(), String>> {
        GitRepository::rebase(self, worktree, onto)
    }
    fn conflicted_files(&self, worktree: &Path) -> Result<Vec<String>> {
        GitRepository::conflicted_files(self, worktree)
    }
    fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>> {
        GitRepository::changed_paths(self, from, to)
    }
    fn added_paths(&self, from: &str, to: &str) -> Result<Vec<String>> {
        GitRepository::added_paths(self, from, to)
    }
    fn paths_in(&self, commit: &str, directory: &str) -> Result<Vec<String>> {
        GitRepository::paths_in(self, commit, directory)
    }
    fn paths_containing(
        &self,
        commit: &str,
        needle: &str,
        paths: &[String],
    ) -> Result<Vec<String>> {
        GitRepository::paths_containing(self, commit, needle, paths)
    }
    fn rename_and_commit(
        &self,
        worktree: &Path,
        from: &str,
        to: &str,
        paragraphs: &[String],
    ) -> Result<std::result::Result<CommitSha, String>> {
        GitRepository::rename_and_commit(self, worktree, from, to, paragraphs)
    }
    fn merged_tree(
        &self,
        main: &str,
        head: &str,
    ) -> Result<std::result::Result<String, Vec<String>>> {
        GitRepository::merged_tree(self, main, head)
    }
    fn checkout_scratch(&self, path: &Path, commit: &str) -> Result<()> {
        GitRepository::checkout_scratch(self, path, commit)
    }
    fn tree_of(&self, commit: &str) -> Result<String> {
        GitRepository::tree_of(self, commit)
    }
    fn commit_tree(&self, tree: &str, parent: &str, paragraphs: &[String]) -> Result<CommitSha> {
        GitRepository::commit_tree(self, tree, parent, paragraphs)
    }
    fn update_ref(&self, name: &str, value: &str) -> Result<()> {
        GitRepository::update_ref(self, name, value)
    }
    fn advance_main(&self, branch: &LandingBranch, from: &str, to: &str) -> Result<()> {
        GitRepository::advance_main(self, branch, from, to)
    }
    fn prune_worktrees(&self) -> Result<()> {
        GitRepository::prune_worktrees(self)
    }
    fn repair_worktree(&self, worktree: &Path) -> Result<()> {
        GitRepository::repair_worktree(self, worktree)
    }
    fn remove_worktree_and_branch(&self, worktree: &Path, branch: &str) -> Result<()> {
        GitRepository::remove_worktree_and_branch(self, worktree, branch)
    }
    fn branches(&self) -> Result<Vec<String>> {
        GitRepository::branches(self)
    }
    fn delete_branch(&self, branch: &str) -> Result<()> {
        GitRepository::delete_branch(self, branch)
    }
    fn tracks(&self, worktree: &Path, path: &str) -> Result<bool> {
        GitRepository::tracks(self, worktree, path)
    }
    fn main_checkout(&self) -> Result<Option<PathBuf>> {
        GitRepository::main_checkout(self)
    }
    fn log_oneline(&self, base: &str, head: &str) -> Result<String> {
        GitRepository::log_oneline(self, base, head)
    }
    fn diff_stat(&self, base: &str, head: &str) -> Result<String> {
        GitRepository::diff_stat(self, base, head)
    }
    fn diff_numbers(&self, base: &str, head: &str) -> Result<DiffNumbers> {
        GitRepository::diff_numbers(self, base, head)
    }
    fn diff_to_file(&self, base: &str, head: &str, path: &Path) -> Result<()> {
        let file = super::agent_dir::create_file(path)
            .with_context(|| format!("create {}", path.display()))?;
        GitRepository::diff_to(self, base, head, &file)
    }
    fn create_worktree(&self, run: &TaskRun) -> Result<String> {
        GitRepository::create_worktree(self, run)
    }
    fn merge_conflicts(&self, main: &str, head: &str) -> Result<Vec<String>> {
        GitRepository::merge_conflicts(self, main, head)
    }
    fn landed_task_ids(&self, base: &str, head: &str) -> Result<Vec<TaskId>> {
        GitRepository::landed_task_ids(self, base, head)
    }
    fn landed_run_commit(
        &self,
        base: &str,
        head: &str,
        run: &str,
    ) -> Result<Option<(CommitSha, CommitSha)>> {
        GitRepository::landed_run_commit(self, base, head, run)
    }
}

/// Run against (and from) the common directory, since `root`, or the
/// working directory, may be a run worktree that the landing removed
/// before the push.
impl MainRemote for GitRepository {
    fn push_config(&self) -> Result<RepositoryConfig> {
        self.repository_config()
    }

    fn has_remote(&self, remote: &str) -> Result<bool> {
        let remotes = output(
            Command::new(&self.git)
                .current_dir(&self.common_dir)
                .arg("--git-dir")
                .arg(&self.common_dir)
                .arg("remote"),
        )?;
        Ok(remotes.lines().any(|line| line.trim() == remote))
    }

    /// The one push of the landing branch; the grant is the Integrator's
    /// (ADR-t728-2), so nothing else in the runtime reaches it.
    fn push_main(
        &self,
        _: &crate::application::integrate::PushGrant,
        remote: &str,
        branch: &LandingBranch,
    ) -> Result<()> {
        let reference = branch.reference();
        let (status, stdout, stderr) = capture(
            Command::new(&self.git)
                .current_dir(&self.common_dir)
                .arg("--git-dir")
                .arg(&self.common_dir)
                .env("GIT_TERMINAL_PROMPT", "0")
                .args(["push", remote, &format!("{reference}:{reference}")]),
            PUSH_TIMEOUT,
        )?;
        ensure!(
            status.success(),
            "git push {remote} {} failed ({status}): {}",
            branch.name,
            format!("{}\n{}", stderr.trim(), stdout.trim()).trim()
        );
        Ok(())
    }

    fn contains_landed_commit(
        &self,
        remote: &str,
        branch: &LandingBranch,
        commit: &CommitSha,
    ) -> Result<bool> {
        let reference = branch.reference();
        let (status, stdout, stderr) = capture(
            Command::new(&self.git)
                .current_dir(&self.common_dir)
                .arg("--git-dir")
                .arg(&self.common_dir)
                .env("GIT_TERMINAL_PROMPT", "0")
                .args(["ls-remote", remote, &reference]),
            PUSH_TIMEOUT,
        )?;
        ensure!(
            status.success(),
            "git ls-remote {remote} failed ({status}): {stderr}"
        );
        let Some(head) = stdout.lines().find_map(|line| {
            let (sha, name) = line.split_once('\t')?;
            (name == reference).then_some(sha)
        }) else {
            return Ok(false);
        };
        let head = object_id(head, "remote branch head")?;
        let has_object = |sha: &CommitSha| -> Result<bool> {
            let (status, _, stderr) = capture(
                Command::new(&self.git)
                    .current_dir(&self.common_dir)
                    .arg("--git-dir")
                    .arg(&self.common_dir)
                    .args(["cat-file", "-e", &format!("{}^{{commit}}", sha.as_str())]),
                Duration::from_secs(30),
            )?;
            match status.code() {
                Some(0) => Ok(true),
                Some(1) => Ok(false),
                _ => bail!("git cat-file failed ({status}): {stderr}"),
            }
        };
        if !has_object(&head)? {
            // Fetch into the object store without touching FETCH_HEAD, which
            // other landings may be using at the same time.
            let (status, _, stderr) = capture(
                Command::new(&self.git)
                    .current_dir(&self.common_dir)
                    .arg("--git-dir")
                    .arg(&self.common_dir)
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .args([
                        "fetch",
                        "--no-write-fetch-head",
                        "--no-tags",
                        remote,
                        &reference,
                    ]),
                PUSH_TIMEOUT,
            )?;
            ensure!(
                status.success(),
                "git fetch {remote} failed ({status}): {stderr}"
            );
            ensure!(
                has_object(&head)?,
                "remote branch head {head} is unavailable after fetch"
            );
        }
        let (status, _, stderr) = capture(
            Command::new(&self.git)
                .current_dir(&self.common_dir)
                .arg("--git-dir")
                .arg(&self.common_dir)
                .args([
                    "merge-base",
                    "--is-ancestor",
                    commit.as_str(),
                    head.as_str(),
                ]),
            Duration::from_secs(30),
        )?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!("git merge-base failed ({status}): {stderr}"),
        }
    }
}

pub struct Cmux {
    pub executable: PathBuf,
}

/// How long the detached ping may take. Without `CMUX_SOCKET_PATH` cmux's
/// CLI discovers the socket on its own, which has taken up to 11 seconds
/// (cmux 0.64.25) before the reply or the refusal came.
pub const DETACHED_PING_TIMEOUT: Duration = Duration::from_secs(60);

/// Give `command` the environment a launchd-started supervisor has: every
/// `CMUX_*` variable in `inherited` (the socket capability, the workspace
/// and surface IDs, the socket path) removed, PATH replaced, and the socket
/// password set only when the invoking shell exported it.
pub fn detach(
    command: &mut Command,
    inherited: impl IntoIterator<Item = OsString>,
    environment: &SupervisorEnvironment,
) {
    for name in inherited {
        if name.to_string_lossy().starts_with("CMUX_") {
            command.env_remove(name);
        }
    }
    command.env("PATH", &environment.path);
    if let Some(password) = &environment.socket_password {
        command.env(SOCKET_PASSWORD_ENV, password);
    }
}

fn expect_pong(reply: &str) -> Result<()> {
    ensure!(
        reply.trim() == "PONG",
        "unexpected cmux ping response: {reply}"
    );
    Ok(())
}

/// The background wrappers of this host (ADR-t1404-1), which the cmux
/// backend serves the calls on a [`BackgroundHandle`] with.
fn background_wrappers() -> super::background::BackgroundWrappers<'static> {
    super::background::BackgroundWrappers {
        processes: &SystemProcesses,
    }
}

/// A background wrapper has no terminal: `what` cannot be sent to it nor
/// read from it (its requests go to its `turns/`, ADR-t813-1).
fn refuse_background(workspace_id: &str, what: &str) -> Result<()> {
    ensure!(
        !is_background(workspace_id),
        "the background wrapper {workspace_id} has no terminal for {what}"
    );
    Ok(())
}

impl WorkspaceBackend for Cmux {
    fn launch_background(
        &self,
        cwd: &Path,
        command: &str,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<String> {
        background_wrappers().launch(cwd, command, env, log)
    }

    fn preflight(&self) -> Result<()> {
        expect_pong(&output(Command::new(&self.executable).arg("ping"))?)
    }

    fn preflight_detached(&self, environment: &SupervisorEnvironment) -> Result<()> {
        self.preflight_detached_within(environment, DETACHED_PING_TIMEOUT)
    }

    fn call_timeout(&self) -> Duration {
        OUTPUT_TIMEOUT
    }

    fn create(
        &self,
        task: &Task,
        run: &TaskRun,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        let raw = self.create_workspace(
            &run_workspace_name(task, run)?,
            Path::new(run.worktree_path().context("missing worktree")?),
            command,
            tags,
        )?;
        // Persist the returned handle before resolving its stable UUID.
        crate::application::RunFiles::write(
            &super::run_files::LocalRunFiles,
            &Path::new(run.run_dir().context("missing run directory")?)
                .join("workspace-create.txt"),
            raw.as_bytes(),
        )?;
        self.identify_created(workspace_handle(&raw)?)
    }

    fn create_resume(
        &self,
        task: &Task,
        run: &TaskRun,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        let raw = self.create_workspace(
            &run_workspace_name(task, run)?,
            Path::new(run.worktree_path().context("missing worktree")?),
            command,
            tags,
        )?;
        self.identify_created(workspace_handle(&raw)?)
    }

    /// `cmux send` reads `\n`, `\r` and `\t` as keys, so the text goes as
    /// one line with backslashes replaced; Enter submits it once the agent
    /// had [`paste_settle`] to take the paste in (an Enter in the middle of
    /// a long paste is taken as part of it, task 285).
    fn send_text(&self, workspace_id: &str, text: &str) -> Result<()> {
        refuse_background(workspace_id, "text")?;
        let line = single_line(text);
        output(Command::new(&self.executable).args([
            "send",
            "--workspace",
            workspace_id,
            "--",
            &line,
        ]))?;
        thread::sleep(paste_settle(line.chars().count()));
        self.send_enter(workspace_id)
    }

    fn send_enter(&self, workspace_id: &str) -> Result<()> {
        self.send_key(workspace_id, "enter")
    }

    fn send_key(&self, workspace_id: &str, key: &str) -> Result<()> {
        refuse_background(workspace_id, "keys")?;
        output(Command::new(&self.executable).args([
            "send-key",
            "--workspace",
            workspace_id,
            "--",
            key,
        ]))?;
        Ok(())
    }

    fn capture(&self, workspace_id: &str) -> Result<String> {
        refuse_background(workspace_id, "a screen")?;
        output(Command::new(&self.executable).args([
            "read-screen",
            "--workspace",
            workspace_id,
            "--scrollback",
            "--lines",
            "2000",
        ]))
    }

    /// cmux refuses to close a pinned workspace ("protected", cmux
    /// 0.64.25), so the pin goes first. An unpin that fails (the
    /// workspace is already gone, say) does not stop the close, whose own
    /// error is the one reported.
    fn close(&self, workspace_id: &str) -> Result<()> {
        if let Some(handle) = BackgroundHandle::parse(workspace_id) {
            return background_wrappers().stop(&handle);
        }
        let _ = self.workspace_action(workspace_id, &["unpin"]);
        let raw = output(
            Command::new(&self.executable)
                .args(["workspace", "close"])
                .arg(workspace_id),
        )?;
        workspace_handle(&raw).context("cmux did not confirm the workspace close")?;
        Ok(())
    }

    fn set_color(&self, workspace_id: &str, color: &str) -> Result<()> {
        // A background wrapper has no sidebar entry to color.
        if is_background(workspace_id) {
            return Ok(());
        }
        self.workspace_action(workspace_id, &["set-color", "--color", color])
    }

    fn set_status(&self, workspace_id: &str, key: &str, value: &str, icon: &str) -> Result<()> {
        if is_background(workspace_id) {
            return Ok(());
        }
        output(Command::new(&self.executable).args([
            "set-status",
            key,
            value,
            "--icon",
            icon,
            "--workspace",
            workspace_id,
        ]))?;
        Ok(())
    }

    fn pin(&self, workspace_id: &str) -> Result<()> {
        if is_background(workspace_id) {
            return Ok(());
        }
        self.workspace_action(workspace_id, &["pin"])
    }

    /// Type `/exit` at Claude's prompt exactly as a person would.
    fn send_exit(&self, workspace_id: &str) -> Result<()> {
        refuse_background(workspace_id, "/exit")?;
        output(Command::new(&self.executable).args([
            "send",
            "--workspace",
            workspace_id,
            "--",
            "/exit",
        ]))?;
        thread::sleep(paste_settle("/exit".len()));
        self.send_enter(workspace_id)
    }

    fn exists(&self, workspace_id: &str) -> Result<bool> {
        if let Some(handle) = BackgroundHandle::parse(workspace_id) {
            return Ok(background_wrappers().alive(&handle));
        }
        Ok(workspace_listed(&self.workspace_listing()?, workspace_id))
    }

    fn listed_workspace_ids(&self) -> Result<Vec<String>> {
        Ok(listed_workspaces(&self.workspace_listing()?)?
            .into_iter()
            .map(|workspace| workspace.id)
            .collect())
    }

    fn workspaces_described(&self, description: &str) -> Result<Vec<String>> {
        Ok(listed_workspaces(&self.workspace_listing()?)?
            .into_iter()
            .filter(|workspace| workspace.description.as_deref() == Some(description))
            .map(|workspace| workspace.id)
            .collect())
    }

    fn create_named(
        &self,
        name: &str,
        cwd: &Path,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        let raw = self.create_workspace(name, cwd, command, tags)?;
        self.identify_created(workspace_handle(&raw)?)
    }

    fn ensure_group(&self, external_id: &str, name: &str) -> Result<String> {
        let reply = output(Command::new(&self.executable).args([
            "--json",
            "--id-format",
            "uuids",
            "workspace-group",
            "create",
            "--name",
            name,
            "--external-id",
            external_id,
        ]))?;
        let reply: Value =
            serde_json::from_str(&reply).context("decode cmux workspace-group create")?;
        Ok(created_group_id(&reply)?.to_owned())
    }

    fn notify(&self, title: &str, body: &str, workspace: Option<&str>) -> Result<()> {
        let mut command = Command::new(&self.executable);
        command.args(["notify", "--title", title, "--body", body]);
        if let Some(workspace) = workspace {
            command.args(["--workspace", workspace]);
        }
        output(&mut command)?;
        Ok(())
    }
}

impl WorkspaceListing for Cmux {
    fn list_workspaces(&self) -> Result<Vec<ListedWorkspace>> {
        listed_workspaces(&self.workspace_listing()?)
    }
}

impl Cmux {
    /// Every window's `cmux --json --id-format uuids workspace list`, merged
    /// into one listing. Without `--window` cmux lists only the caller's
    /// window, so a workspace a person moved to another window would look
    /// closed and `up` would open a second one.
    fn workspace_listing(&self) -> Result<Value> {
        let windows = output(Command::new(&self.executable).args([
            "--json",
            "--id-format",
            "uuids",
            "list-windows",
        ]))?;
        let windows: Value = serde_json::from_str(&windows).context("decode cmux list-windows")?;
        merged_workspace_listing(&windows, |window| {
            let listing = output(Command::new(&self.executable).args([
                "--json",
                "--id-format",
                "uuids",
                "workspace",
                "list",
                "--window",
                window,
            ]))?;
            serde_json::from_str(&listing).context("decode cmux workspace list")
        })
    }

    /// The detached ping with an explicit deadline. cmux admits a client by
    /// its ancestry, not its environment: a child of one of its terminals
    /// gets through however its variables look, a process under launchd
    /// does not (verified against cmux 0.64.25). So besides the scrubbed
    /// environment the ping runs orphaned, the way the LaunchAgent's
    /// supervisor does: an outer `sh` in a session of its own backgrounds
    /// an inner one and exits; the inner one waits until the outer is gone
    /// (so launchd is its parent before cmux looks), prints its pid and
    /// becomes `cmux ping` by `exec`, so the pid is cmux's and can be
    /// killed at the deadline.
    pub fn preflight_detached_within(
        &self,
        environment: &SupervisorEnvironment,
        timeout: Duration,
    ) -> Result<()> {
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                r#"/bin/sh -c 'while kill -0 "$1" 2>/dev/null; do sleep 0.01; done; printf "pid=%s\n" "$$"; exec "$0" ping' "$0" "$$" &"#,
            ])
            .arg(&self.executable);
        // SAFETY: setsid only detaches the child from this session and
        // terminal; it allocates nothing and is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        detach(
            &mut command,
            env::vars_os().map(|(name, _)| name),
            environment,
        );
        let (reply, stderr) = orphan_output(&mut command, timeout)?;
        if reply.trim() == "PONG" {
            return Ok(());
        }
        Err(DetachedRefusal {
            reason: format!(
                "{:?} ping from outside cmux failed: {}",
                self.executable,
                if stderr.trim().is_empty() {
                    format!("unexpected response: {reply}")
                } else {
                    stderr.trim().to_owned()
                }
            ),
        }
        .into())
    }

    /// `cmux workspace-action --action <action> [flags…] --workspace <id>`.
    fn workspace_action(&self, workspace_id: &str, action: &[&str]) -> Result<()> {
        output(
            Command::new(&self.executable)
                .args(["workspace-action", "--action"])
                .args(action)
                .args(["--workspace", workspace_id]),
        )?;
        Ok(())
    }

    /// `cmux workspace create`; the raw reply carries the `OK workspace:N` handle.
    fn create_workspace(
        &self,
        name: &str,
        cwd: &Path,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        let mut create = Command::new(&self.executable);
        create.args(workspace_create_arguments(name, command, tags));
        output(create.arg("--cwd").arg(cwd))
    }

    /// Resolve the handle of a workspace this process just created to its
    /// UUID. A workspace cmux created but did not identify would be left
    /// open with nothing to find it by (its caller records no UUID), so it
    /// is closed by its handle; the error says whether that close failed
    /// too.
    fn identify_created(&self, handle: &str) -> Result<String> {
        self.identify(handle).map_err(|error| match self.close(handle) {
            Ok(()) => error.context(format!(
                "cmux created workspace {handle} but did not identify it; it was closed"
            )),
            Err(close) => error.context(format!(
                "cmux created workspace {handle} but did not identify it, and closing it failed: {close:#}"
            )),
        })
    }

    /// Resolve a numeric handle to the workspace's stable UUID.
    fn identify(&self, handle: &str) -> Result<String> {
        let identity = output(
            Command::new(&self.executable)
                .args(["--json", "--id-format", "uuids", "identify", "--workspace"])
                .arg(handle),
        )?;
        let identity: Value =
            serde_json::from_str(&identity).context("decode cmux workspace identity")?;
        let id = identity
            .pointer("/caller/workspace_id")
            .and_then(Value::as_str)
            .context("cmux did not return the requested workspace UUID")?;
        uuid::Uuid::parse_str(id).context("invalid cmux workspace UUID")?;
        Ok(id.into())
    }
}

/// `workspace create` and its flags but `--cwd` (a path, added by the
/// caller): the title, the description, one `--env KEY=VALUE` per variable
/// and the group from `tags`, and the command, opened without focus.
pub fn workspace_create_arguments(name: &str, command: &str, tags: &WorkspaceTags) -> Vec<String> {
    let mut arguments: Vec<String> = ["workspace", "create", "--name", name]
        .map(str::to_owned)
        .into();
    if let Some(description) = &tags.description {
        arguments.extend(["--description".into(), description.clone()]);
    }
    for (key, value) in &tags.env {
        arguments.extend(["--env".into(), format!("{key}={value}")]);
    }
    if let Some(group) = &tags.group {
        arguments.extend(["--group".into(), group.clone()]);
    }
    arguments.extend([
        "--command".into(),
        command.into(),
        "--focus".into(),
        "false".into(),
    ]);
    arguments
}

/// One `workspace list`-shaped listing of the workspaces of every window in
/// a `cmux --json --id-format uuids list-windows` reply, `list` giving each
/// window's listing by its UUID. A window that cannot be listed fails the
/// whole listing: a workspace missed there would look closed.
pub fn merged_workspace_listing(
    windows: &Value,
    mut list: impl FnMut(&str) -> Result<Value>,
) -> Result<Value> {
    let mut workspaces = Vec::new();
    for window in windows
        .as_array()
        .context("cmux list-windows is not a list")?
    {
        let id = window
            .get("id")
            .and_then(Value::as_str)
            .context("cmux listed a window without an ID")?;
        let listing = list(id).with_context(|| format!("list the workspaces of window {id}"))?;
        workspaces.extend(
            listing
                .get("workspaces")
                .and_then(Value::as_array)
                .with_context(|| format!("cmux workspace list of window {id} has no workspaces"))?
                .iter()
                .cloned(),
        );
    }
    Ok(serde_json::json!({ "workspaces": workspaces }))
}

/// Whether a `cmux --json --id-format uuids workspace list` reply lists the
/// workspace `id` (UUIDs compared without regard to case).
pub fn workspace_listed(listing: &Value, id: &str) -> bool {
    listing
        .get("workspaces")
        .and_then(Value::as_array)
        .is_some_and(|workspaces| {
            workspaces.iter().any(|workspace| {
                workspace
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|listed| listed.eq_ignore_ascii_case(id))
            })
        })
}

/// Every workspace of a `cmux --json --id-format uuids workspace list`
/// reply, with its description.
pub fn listed_workspaces(listing: &Value) -> Result<Vec<ListedWorkspace>> {
    listing
        .get("workspaces")
        .and_then(Value::as_array)
        .context("cmux workspace list has no workspaces")?
        .iter()
        .map(|workspace| {
            Ok(ListedWorkspace {
                id: workspace
                    .get("id")
                    .and_then(Value::as_str)
                    .context("cmux listed a workspace without an ID")?
                    .to_owned(),
                description: workspace
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}

/// The group's UUID in a `cmux --json --id-format uuids workspace-group
/// create` reply, which is the same whether the call created the group or
/// found it by its external ID.
pub fn created_group_id(reply: &Value) -> Result<&str> {
    reply
        .pointer("/group/id")
        .and_then(Value::as_str)
        .context("cmux did not return the workspace group's ID")
}

/// Workspaces are named per repository because one cmux serves several
/// queues, and every name is `[<repo>]<role>` with no space after the
/// bracket, where `<repo>` is the basename of the repository root (the path
/// itself when it has none). A worker is
/// `[<repo>]worker#<task-id> - <task title>`, `<repo>` being the root the run
/// was planned from and the title the task's, unabridged (ADR-0028, which
/// replaces the name ADR-0018 chose).
/// The resume workspace of a `needs_session` run (goal 8's automatic resume)
/// takes this same name, with `run <run-id> resume` as its description.
/// Names are for people only: the runtime identifies workspaces by UUID
/// (ADR-0026).
pub fn run_workspace_name(task: &Task, run: &TaskRun) -> Result<String> {
    let repo = Path::new(run.repo_path().context("missing repository path")?);
    Ok(format!(
        "[{}]worker#{} - {}",
        repository_name(repo),
        run.task_id(),
        task.title()
    ))
}

/// `text` as one line for `cmux send`: line breaks and tabs become spaces
/// and backslashes slashes, so nothing in it reads as a key.
/// How long the agent is given to take in `chars` typed characters
/// before the Enter that submits them: 300 ms and 1 ms for every 10
/// characters, at most 3 s (1.6 s for a 13 KB resolution request).
pub fn paste_settle(chars: usize) -> Duration {
    Duration::from_millis((300 + chars as u64 / 10).min(3000))
}

pub fn single_line(text: &str) -> String {
    text.split(['\n', '\r', '\t'])
        .filter(|part| !part.trim().is_empty())
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join(" ")
        .replace('\\', "/")
}

pub fn workspace_handle(raw: &str) -> Result<&str> {
    let handle = raw
        .lines()
        .find_map(|line| line.strip_prefix("OK "))
        .context("unrecognized cmux workspace creation response; inspect workspace-create.txt")?
        .trim();
    let suffix = handle
        .strip_prefix("workspace:")
        .context("unexpected cmux workspace handle")?;
    ensure!(
        !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()),
        "invalid cmux workspace handle"
    );
    Ok(handle)
}

/// The entries of `claude plugin list --json` (an array of `{"id":
/// "<plugin>@<marketplace>", "enabled": bool, "version": ..., ...}`) that
/// are of the plugin `name`, each with its id.
fn plugin_entries(listed: &str, name: &str) -> Result<Vec<(String, Value)>> {
    let entries: Vec<Value> = serde_json::from_str(listed)
        .with_context(|| format!("the plugin list is not a JSON array: {}", listed.trim()))?;
    let mut found = Vec::new();
    for entry in entries {
        let id = entry["id"]
            .as_str()
            .with_context(|| format!("a plugin without an id: {entry}"))?
            .to_owned();
        if id.split_once('@').map_or(id.as_str(), |(plugin, _)| plugin) == name {
            found.push((id, entry));
        }
    }
    Ok(found)
}

/// What `claude plugin list --json` says of the plugin `name`: enabled
/// when any entry of it is.
pub fn plugin_state(listed: &str, name: &str) -> Result<PluginState> {
    let mut disabled = Vec::new();
    for (id, entry) in plugin_entries(listed, name)? {
        match entry["enabled"].as_bool() {
            Some(true) => return Ok(PluginState::Enabled),
            Some(false) => disabled.push(id),
            None => bail!("the plugin {id} has no enabled flag: {entry}"),
        }
    }
    Ok(if disabled.is_empty() {
        PluginState::Missing
    } else {
        PluginState::Disabled(disabled)
    })
}

/// The version `claude plugin list --json` gives the plugin `name`: that of
/// an enabled entry, else of the first; `None` when it is not installed or
/// its entry has no version.
pub fn plugin_version(listed: &str, name: &str) -> Result<Option<String>> {
    let entries = plugin_entries(listed, name)?;
    let entry = entries
        .iter()
        .find(|(_, entry)| entry["enabled"].as_bool() == Some(true))
        .or_else(|| entries.first());
    Ok(entry.and_then(|(_, entry)| entry["version"].as_str().map(str::to_owned)))
}

/// The dagq plugin of the Claude Code at `executable`, as sessions started
/// in `cwd` see it (ADR-t618-2): its version from the same `claude plugin
/// list --json` the check of `up` reads, and its update by
/// [`lifecycle::PLUGIN_UPDATE_ARGUMENTS`](crate::application::lifecycle::PLUGIN_UPDATE_ARGUMENTS).
pub struct ClaudePlugin {
    pub executable: PathBuf,
    pub cwd: PathBuf,
}

/// How long one command of the plugin's update may take: it fetches the
/// marketplace's repository.
const PLUGIN_UPDATE_TIMEOUT: Duration = Duration::from_secs(300);

impl crate::application::InstalledPlugin for ClaudePlugin {
    fn version(&self) -> Result<Option<String>> {
        let listed = output(
            Command::new(&self.executable)
                .args(["plugin", "list", "--json"])
                .current_dir(&self.cwd),
        )?;
        plugin_version(&listed, crate::application::lifecycle::DAGQ_PLUGIN)
    }

    fn update(&self) -> Vec<crate::application::PluginCommandRun> {
        let mut runs = Vec::new();
        for arguments in crate::application::lifecycle::PLUGIN_UPDATE_ARGUMENTS {
            let command = format!("claude {}", arguments.join(" "));
            let (succeeded, output) = match capture(
                Command::new(&self.executable)
                    .args(arguments)
                    .current_dir(&self.cwd),
                PLUGIN_UPDATE_TIMEOUT,
            ) {
                Ok((status, stdout, stderr)) => {
                    let mut output = format!("{stdout}{stderr}").trim().to_owned();
                    if !status.success() {
                        output.push_str(&format!(" ({status})"));
                    }
                    (status.success(), output)
                }
                Err(error) => (false, format!("{error:#}")),
            };
            runs.push(crate::application::PluginCommandRun {
                command,
                output,
                succeeded,
            });
            if !succeeded {
                break;
            }
        }
        runs
    }
}

pub struct ClaudeCode {
    pub executable: PathBuf,
}

impl AgentProvider for ClaudeCode {
    fn preflight(&self) -> Result<()> {
        output(Command::new(&self.executable).arg("--version"))?;
        Ok(())
    }

    /// `claude plugin list --json` in `cwd` (a project's plugins count
    /// where its sessions start): `name` is enabled when an entry
    /// `<name>@<marketplace>` has `enabled: true`.
    fn installed_plugin(&self, cwd: &Path, name: &str) -> Result<PluginState> {
        let listed = output(
            Command::new(&self.executable)
                .args(["plugin", "list", "--json"])
                .current_dir(cwd),
        )?;
        plugin_state(&listed, name)
    }

    fn command(&self, run: &TaskRun, prompt: &str) -> Result<CommandSpec> {
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        let settings = run_dir.join("claude-settings.json");
        write_settings(&settings, ActorRole::Worker, None, &run.idle_marker_path()?)?;
        let mut command = CommandSpec::new(&self.executable);
        command
            .current_dir(run.worktree_path().context("missing worktree")?)
            .arg("--session-id")
            .arg(run.id().as_str())
            .arg("--debug-file")
            .arg(run.log_path().context("missing log path")?)
            .arg("--add-dir")
            .arg(run_dir)
            .arg("--settings")
            .arg(&settings)
            .arg("--")
            .arg(prompt);
        Ok(command)
    }

    /// `claude --resume <run-id>` in the worktree with the run's settings
    /// (its `Stop` hook), so the resumed session opens like the worker did.
    fn resume_command(&self, run: &TaskRun) -> Result<CommandSpec> {
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        let settings = run_dir.join("claude-settings.json");
        write_settings(&settings, ActorRole::Worker, None, &run.idle_marker_path()?)?;
        let mut command = CommandSpec::new(&self.executable);
        command
            .current_dir(run.worktree_path().context("missing worktree")?)
            .arg("--resume")
            .arg(run.id().as_str())
            .arg("--debug-file")
            .arg(run_dir.join(crate::application::screen_idle::RESUME_DEBUG_LOG))
            .arg("--add-dir")
            .arg(run_dir)
            .arg("--settings")
            .arg(&settings);
        Ok(command)
    }

    /// `claude` in the checkout with the planner directory's settings (its
    /// `Stop` hook writes the idle marker there), its debug file, the
    /// directory added, and the plugin directory when one was given. The
    /// settings of a planner the runtime started also turn the prompt
    /// suggestions off; a person's planner keeps them.
    fn planner_command(&self, planner: &PlannerCommand<'_>) -> Result<CommandSpec> {
        let settings = planner.dir.join("claude-settings.json");
        write_settings(
            &settings,
            ActorRole::Planner,
            Some(planner.origin),
            &planner.idle_marker(),
        )?;
        let mut command = CommandSpec::new(&self.executable);
        command
            .current_dir(planner.cwd)
            .arg("--debug-file")
            .arg(
                planner
                    .dir
                    .join(crate::application::planner::PLANNER_DEBUG_LOG),
            )
            .arg("--add-dir")
            .arg(planner.dir)
            .arg("--settings")
            .arg(&settings);
        if let Some(dir) = planner.plugin_dir {
            command.arg("--plugin-dir").arg(dir);
        }
        command.arg("--").arg(planner.prompt);
        Ok(command)
    }

    /// `claude -p` (print mode): no terminal, no trust dialog, no settings
    /// of dagq's ([`AgentSettings::None`]); a tool that needs permission
    /// and is not among the tools of `access` ([`claude_tools`]) is
    /// refused. The prompt is its standard input, never an argument: `-p`
    /// with no prompt reads it there, and a prompt of any size starts
    /// (task 1560; on the command line one past the system's limit on the
    /// arguments fails with `E2BIG`).
    fn headless_command(&self, cwd: &Path, prompt: &str, access: JobAccess) -> Result<CommandSpec> {
        let mut command = CommandSpec::new(&self.executable);
        command.current_dir(cwd).arg("-p");
        let tools = claude_tools(access);
        if !tools.is_empty() {
            command.arg("--allowedTools").args(tools);
        }
        command.stdin(prompt);
        Ok(command)
    }
    /// `claude -p` prints the final reply of the job only (its default
    /// text output): the reply is stdout as it is.
    fn job_reply(&self, stdout: &str) -> String {
        stdout.to_owned()
    }
    /// `claude -p` in the worktree with `claude-review-settings.json` of
    /// the run directory: the worker's settings without its `Stop` hook, so
    /// the review never writes the live session's idle marker. It may only
    /// do what `access` says (for [`JobAccess::ReadFiles`], `Read`, `Grep`,
    /// `Glob` allowed; `Bash`, `Edit`, `Write`, `NotebookEdit`
    /// disallowed); `review.md` is in the run directory. It loads no
    /// setting sources (`--setting-sources ""`), with or without required
    /// subagents: the worktree's `.claude/settings.json`,
    /// `.claude/settings.local.json`, `.claude/agents`, `.claude/skills`,
    /// `.mcp.json` and `CLAUDE.md`, which the worker may change, and the
    /// user's settings do not shape the review (ADR-t1470-1 decision 1);
    /// the prompt names the repository's instructions to read instead
    /// (decision 2). The prompt is its standard input, as a headless job's
    /// ([`AgentProvider::headless_command`], task 1560).
    fn review_command(
        &self,
        run: &TaskRun,
        prompt: &str,
        access: JobAccess,
    ) -> Result<CommandSpec> {
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        let settings = run_dir.join("claude-review-settings.json");
        write_settings(
            &settings,
            ActorRole::ReviewJob,
            None,
            // The review has no hook to write a marker with.
            run_dir,
        )?;
        let mut command = CommandSpec::new(&self.executable);
        command
            .current_dir(run.worktree_path().context("missing worktree")?)
            .arg("-p")
            .arg("--debug-file")
            .arg(run_dir.join("claude-review.log"))
            .arg("--add-dir")
            .arg(run_dir)
            .arg("--settings")
            .arg(&settings)
            .arg("--allowedTools")
            .arg(claude_tools(access).join(","))
            // The live worker session owns the worktree: the review never
            // edits it or runs commands in it.
            .arg("--disallowedTools")
            .arg(review_disallowed_tools(access).join(","))
            .arg("--setting-sources")
            .arg("")
            .stdin(prompt);
        Ok(command)
    }
    fn runs_review_subagents(&self) -> bool {
        true
    }
    /// `--agents <json>` with each definition (its `description`, its body
    /// as the `prompt`, and only the review's reads as its `tools`) and
    /// `--allowedTools Agent` so the review can start them (ADR-t1453-1
    /// decision 8). The review's `--setting-sources ""`
    /// ([`AgentProvider::review_command`], ADR-t1470-1), its `--settings`
    /// and its `--disallowedTools`, which reach the subagents too, stay.
    fn review_subagents(
        &self,
        command: &mut CommandSpec,
        agents: &[crate::domain::review_subagents::AgentDefinition],
    ) -> Result<()> {
        command.option_args([
            "--agents".to_owned(),
            claude_review_agents(agents)?,
            "--allowedTools".to_owned(),
            "Agent".to_owned(),
        ]);
        Ok(())
    }
    /// `claude --settings <queue dir>/claude-inbox-settings.json
    /// [--plugin-dir <dir>] -- <prompt>`: the settings are only the
    /// inbox's `permissions.deny` ([`inbox_settings`], ADR-t1228-2 decision
    /// 3), written again at each open. The inbox's workspace keeps its role
    /// and queue in its own environment (ADR-0026), so a `claude` started
    /// again there still has them, though not the settings (decision 4).
    fn inbox_command(
        &self,
        prompt: &str,
        plugin_dir: Option<&Path>,
        queue_dir: &Path,
    ) -> Result<CommandSpec> {
        let settings = queue_dir.join(INBOX_SETTINGS);
        crate::application::RunFiles::write(
            &super::run_files::LocalRunFiles,
            &settings,
            inbox_settings(&permission_deny(ActorRole::Inbox))?.as_bytes(),
        )
        .with_context(|| format!("write {}", settings.display()))?;
        let mut command = CommandSpec::new(&self.executable);
        command.arg("--settings").arg(&settings);
        if let Some(dir) = plugin_dir {
            command.arg("--plugin-dir").arg(dir);
        }
        command.arg("--").arg(prompt);
        Ok(command)
    }
    fn inbox_settings(&self, queue_dir: &Path) -> Option<PathBuf> {
        Some(queue_dir.join(INBOX_SETTINGS))
    }
    /// `--model <model> --effort <effort>` among the options, before the
    /// prompt.
    fn select_model(&self, command: &mut CommandSpec, model: &str, effort: &str) {
        command.option_args(["--model", model, "--effort", effort]);
    }
    /// `--session-id <id>` among the options, before the prompt.
    fn assign_session_id(&self, command: &mut CommandSpec, session_id: &str) {
        command.option_args(["--session-id", session_id]);
    }
    /// `--strict-mcp-config` without an `--mcp-config`: Claude Code loads
    /// the servers of no configuration, the user's, the project's, the
    /// plugins' and claude.ai's alike.
    fn without_mcp(&self, command: &mut CommandSpec) {
        command.option_args(["--strict-mcp-config"]);
    }
    /// `--mcp-config <config>` with the broker client's server, and
    /// `--allowedTools mcp__dagq-broker` so its tools need no prompt. The
    /// built-in tools stay (`preferred`, ADR-t827-4 decision 1).
    fn broker_tools(&self, command: &mut CommandSpec, config: &Path) -> bool {
        command
            .option_args([std::ffi::OsStr::new("--mcp-config"), config.as_os_str()])
            .option_args(["--allowedTools", crate::application::broker_run::MCP_TOOLS]);
        true
    }
    /// `claude -p --output-format stream-json --verbose` in the target's
    /// working directory (a run's worktree, a planner's checkout),
    /// `--session-id <name>` for the first turn and `--resume <name>` after
    /// it, in [`HEADLESS_PERMISSION_MODE`] with
    /// `claude-headless-settings.json` of the target's directory (its
    /// actor's `permissions.deny`, no hook: the wrapper writes the idle
    /// marker when the turn's process ends), its debug file, the directory
    /// added and the plugin directory a planner loads. It leads a session
    /// of its own, so that stopping it stops what it runs.
    fn turn_command(
        &self,
        target: &TurnTarget<'_>,
        prompt: &str,
        session: TurnSession<'_>,
    ) -> Result<CommandSpec> {
        let settings = target.dir.join(HEADLESS_SETTINGS);
        crate::application::RunFiles::write(
            &super::run_files::LocalRunFiles,
            &settings,
            headless_worker_settings(&permission_deny(target.role))?.as_bytes(),
        )
        .with_context(|| format!("write {}", settings.display()))?;
        let mut command = CommandSpec::new(&self.executable);
        command
            .current_dir(target.cwd)
            .args(["-p", "--output-format", "stream-json", "--verbose"])
            .args(match session {
                TurnSession::Resume(id) => ["--resume", id],
                TurnSession::New(name) => ["--session-id", name],
            })
            .arg("--permission-mode")
            .arg(HEADLESS_PERMISSION_MODE)
            .arg("--debug-file")
            .arg(target.debug_log.context("missing log path")?)
            .arg("--add-dir")
            .arg(target.dir)
            .arg("--settings")
            .arg(&settings);
        if let Some(dir) = target.plugin_dir {
            command.arg("--plugin-dir").arg(dir);
        }
        command.arg("--").arg(prompt).new_session();
        Ok(command)
    }
    fn turn_reader(&self) -> Result<Box<dyn TurnReader>> {
        Ok(Box::new(ClaudeTurnReader::default()))
    }
    /// Its transcript under `$CLAUDE_CONFIG_DIR` (or `~/.claude`): Claude
    /// Code refuses a `--session-id` in use.
    fn turn_session_exists(&self, cwd: &Path, name: &str) -> bool {
        cwd.to_str().is_some_and(|cwd| {
            crate::infrastructure::transcripts::ClaudeTranscripts::from_env().exists(cwd, name)
        })
    }
    fn turn_permission_mode(&self) -> Option<&'static str> {
        Some(HEADLESS_PERMISSION_MODE)
    }
}

/// The settings file of the inbox `up` opens, in the queue's directory
/// (ADR-t1228-2 decision 3).
pub const INBOX_SETTINGS: &str = "claude-inbox-settings.json";

/// Settings of the inbox (ADR-t1228-2 decision 3): `permissions.deny`
/// only, `deny` being the inbox's ([`permission_deny`]: the `dagq`
/// commands its role may not run, the variables naming the actor and
/// `Bash(cmux:*)`). No hook and no idle marker (the inbox is not judged
/// idle), no suggestion setting (a person types in it), no `autoMode`.
/// A guardrail, not enforcement (decision 5).
pub fn inbox_settings(deny: &[String]) -> Result<String> {
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "permissions": {
            "deny": deny
        }
    }))?)
}

/// The settings file of a headless worker's turns, in the run directory.
pub const HEADLESS_SETTINGS: &str = "claude-headless-settings.json";

/// Settings of a headless worker's turns (ADR-t813-1): no hook, the
/// worker's `permissions.deny` ([`SIGNAL_BY_NAME_DENIED`], then `deny`, as
/// in [`stop_hook_settings`]) and the same `autoMode` environment as an
/// interactive run session.
pub fn headless_worker_settings(deny: &[String]) -> Result<String> {
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "permissions": {
            "deny": SIGNAL_BY_NAME_DENIED
                .iter()
                .map(|rule| (*rule).to_owned())
                .chain(deny.iter().cloned())
                .collect::<Vec<_>>()
        },
        "autoMode": {
            "environment": ["$defaults"]
        }
    }))?)
}

/// The Claude settings a role's agent starts with (ADR-t728-1): the one
/// place that decides them. They are Claude Code's, so they belong to its
/// implementation (ADR-t1063-1 decision 3): a headless job names only its
/// intent ([`JobAccess`]), and another provider gives its jobs its own
/// mechanism or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSettings {
    /// None of these: the inbox, whose settings are its denials alone
    /// ([`inbox_settings`], written when `up` opens it), and the headless
    /// jobs other than the review, which their allowed tools
    /// ([`claude_tools`]) restrict.
    None,
    /// The review's: no hooks, so it never writes the live worker session's
    /// idle marker.
    Review,
    /// A session's: the `Stop` hook writing the idle marker, the
    /// `UserPromptSubmit` hook, and `permissions.deny` of signals by name;
    /// with the prompt suggestions off in a session nobody types in.
    Session { suggestions: bool },
}

/// The settings of the agent of `role`, a planner's by its `origin`: a
/// worker and a planner the runtime opened are sessions without
/// suggestions, a person's planner keeps them, the review has its own, and
/// the rest none.
pub fn agent_settings(role: ActorRole, origin: Option<PlannerOrigin>) -> AgentSettings {
    match role {
        ActorRole::Worker => AgentSettings::Session { suggestions: false },
        ActorRole::Planner => AgentSettings::Session {
            suggestions: origin == Some(PlannerOrigin::Person),
        },
        ActorRole::ReviewJob => AgentSettings::Review,
        _ => AgentSettings::None,
    }
}

/// Claude Code's tools a headless job of `access` is allowed beyond what
/// needs no permission (ADR-t1063-1 decision 2): reading files is `Read`,
/// `Grep` and `Glob`, the queue CLI `Bash(dagq:*)`.
pub fn claude_tools(access: JobAccess) -> Vec<&'static str> {
    let mut tools = Vec::new();
    if access.reads_files() {
        tools.extend(["Read", "Grep", "Glob"]);
    }
    if access.runs_queue_cli() {
        tools.push("Bash(dagq:*)");
    }
    tools
}

/// The `--agents` JSON of the review's subagents: each definition's
/// `description` and body (`prompt`), allowed only
/// [`SUBAGENT_TOOLS`](crate::domain::review_subagents::SUBAGENT_TOOLS)
/// whatever the definition says.
pub fn claude_review_agents(
    agents: &[crate::domain::review_subagents::AgentDefinition],
) -> Result<String> {
    let map: serde_json::Map<String, serde_json::Value> = agents
        .iter()
        .map(|agent| {
            (
                agent.name.clone(),
                serde_json::json!({
                    "description": agent.description,
                    "prompt": agent.prompt,
                    "tools": crate::domain::review_subagents::SUBAGENT_TOOLS,
                }),
            )
        })
        .collect();
    Ok(serde_json::to_string(&map)?)
}

/// Claude Code's tools the review of `access` is refused outright: it
/// never edits the worktree the live worker session owns, and runs no
/// command unless it may run the queue CLI.
fn review_disallowed_tools(access: JobAccess) -> Vec<&'static str> {
    let mut tools = Vec::new();
    if !access.runs_queue_cli() {
        tools.push("Bash");
    }
    tools.extend(["Edit", "Write", "NotebookEdit"]);
    tools
}

/// Write the settings of the agent of `role` (a planner's by its
/// `origin`) whose idle marker is `idle_marker` to `path`: the one place
/// the role's settings ([`agent_settings`]) and the `permissions.deny` of
/// its policy ([`permission_deny`]) become Claude Code's. Settings of none
/// write nothing.
fn write_settings(
    path: &Path,
    role: ActorRole,
    origin: Option<PlannerOrigin>,
    idle_marker: &Path,
) -> Result<()> {
    let deny = permission_deny(role);
    let text = match agent_settings(role, origin) {
        AgentSettings::None => return Ok(()),
        AgentSettings::Review => review_settings(&deny)?,
        AgentSettings::Session { suggestions: true } => stop_hook_settings(idle_marker, &deny)?,
        AgentSettings::Session { suggestions: false } => {
            runtime_session_settings(idle_marker, &deny)?
        }
    };
    crate::application::RunFiles::write(&super::run_files::LocalRunFiles, path, text.as_bytes())
        .with_context(|| format!("write {}", path.display()))
}

/// Settings of the headless review: no hooks, the `permissions.deny` of
/// its role (`deny`, see [`stop_hook_settings`]), the same `autoMode`
/// environment as a run session, and no auto memory: Claude Code loads the
/// memory of the cwd (the worktree, whose worker session may write it)
/// even with no setting sources (ADR-t1470-1 decision 1).
pub fn review_settings(deny: &[String]) -> Result<String> {
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "permissions": {
            "deny": deny
        },
        "autoMode": {
            "environment": ["$defaults"]
        },
        "autoMemoryEnabled": false
    }))?)
}

/// Claude Code's global config, where the folder trust of each project is
/// kept: `$CLAUDE_CONFIG_DIR/.claude.json` when that is set (non-empty),
/// else `~/.claude.json`. `None` when neither can be named.
pub fn claude_global_config(config_dir: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    match (config_dir.filter(|dir| !dir.is_empty()), home) {
        (Some(dir), _) => Some(Path::new(dir).join(".claude.json")),
        (None, Some(home)) if !home.is_empty() => Some(Path::new(home).join(".claude.json")),
        _ => None,
    }
}

/// Whether Claude Code has recorded the folder trust dialog as accepted for
/// the repository at `root` in its global config (`config`). Every run
/// worktree resolves to its repository's root for the trust check, so this
/// one key decides whether run sessions stop at the dialog
/// (docs/design/provider-lifecycle.md). A missing config trusts nothing.
pub fn claude_trusts_repository(config: &Path, root: &Path) -> Result<bool> {
    let text = match fs::read_to_string(config) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("read {}", config.display())),
    };
    let config_json: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parse Claude Code config {}", config.display()))?;
    let accepted = |key: &Path| {
        key.to_str().is_some_and(|key| {
            config_json["projects"][key]["hasTrustDialogAccepted"].as_bool() == Some(true)
        })
    };
    Ok(accepted(root) || root.canonicalize().is_ok_and(|real| accepted(&real)))
}

/// The script the `Stop` hook runs (in `sh`, with the marker as `$1` and
/// the [`IDLE_LOG`] as `$2`) to add the marker's line to the log. A marker
/// in which no `"status": "running"` pair can be found lists no running
/// background task (looked for in the line, with the marker's newlines
/// taken out as the readers see it; JSON escapes a quote inside a string, so the text of
/// the last assistant message cannot form the pair), and the streaks of
/// the lines before it are over: the line replaces the log, through a
/// temporary file and a rename so a reader never sees it empty. Any other
/// marker is appended. A pair outside `background_tasks` only keeps lines
/// a reader skips. It always exits 0.
pub const IDLE_LOG_APPEND: &str = r#"line=$(printf '%s\t' "$(date +%s)" && tr -d '\r\n' < "$1") || exit 0
if printf '%s' "$line" | grep -Eq '"status"[[:space:]]*:[[:space:]]*"running"'; then
  printf '%s\n' "$line" >> "$2"
else
  printf '%s\n' "$line" > "$2.tmp" && mv -f "$2.tmp" "$2"
fi
exit 0"#;

/// Per-run Claude settings. The `Stop` hook publishes the hook's stdin JSON
/// as the idle marker. Each finished response replaces the marker
/// atomically, so its modification time tells the supervisor whether the
/// agent went idle after writing the receipt. `SessionEnd` is not used:
/// session exit is confirmed by the wrapper's exit code instead. Before the
/// marker is replaced, the hook appends it with the time to the
/// [`IDLE_LOG`] next to it, so `stats` can time background work from the
/// first marker that listed it; a log that cannot be written does not keep
/// the marker from being published. The append runs in its own `sh`, so
/// the command itself stays a plain `&&` chain whatever shell runs hooks.
/// A marker that lists no running background task ends every streak, so
/// the log is then replaced by that marker's line alone
/// ([`IDLE_LOG_APPEND`]): the log holds no more than the current streak
/// and the first-seen times read from it stay the same (task 422).
///
/// The `UserPromptSubmit` hook publishes its stdin JSON the same way as the
/// input marker ([`PROMPT_SUBMIT_MARKER`](crate::application::stats::PROMPT_SUBMIT_MARKER),
/// next to the idle marker) each time the session takes an input, typed or
/// Claude Code's own notice that background work ended (ADR-0043 decision
/// 2): the supervisor compares its time with the texts it sent. The hook
/// prints nothing (a `UserPromptSubmit` hook's output reaches the agent's
/// context) and always exits 0 (a failed marker never holds the input up).
///
/// `autoMode.environment: ["$defaults"]` keeps the built-in classifier
/// environment and, being a non-empty environment from flag settings, keeps
/// the "Teach auto mode about your environment?" dialog from opening in a
/// run session (docs/design/provider-lifecycle.md).
///
/// `permissions.deny` refuses [`SIGNAL_BY_NAME_DENIED`]: the session may
/// stop what it started by pid, never processes picked by name or pattern.
/// After them come `deny`, the rules of the role's policy
/// ([`permission_deny`]): the `dagq` commands the role may not run and
/// rewriting `DAGQ_ROLE` and the other variables naming the actor. They
/// are a guardrail, not enforcement: the CLI's own check refuses.
pub fn stop_hook_settings(idle_marker: &Path, deny: &[String]) -> Result<String> {
    let log = path_text(&idle_marker.with_file_name(IDLE_LOG))?;
    let marker = path_text(idle_marker)?;
    let command = format!(
        "cat > {tmp} && sh -c {append} sh {tmp} {log} && mv -f {tmp} {marker}",
        append = shell_quote(IDLE_LOG_APPEND),
        tmp = shell_quote(&format!("{marker}.tmp")),
        log = shell_quote(&log),
        marker = shell_quote(&marker),
    );
    let input =
        path_text(&idle_marker.with_file_name(crate::application::stats::PROMPT_SUBMIT_MARKER))?;
    let input_command = format!(
        "cat > {tmp} && mv -f {tmp} {input} || true",
        tmp = shell_quote(&format!("{input}.tmp")),
        input = shell_quote(&input),
    );
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "hooks": {
            "Stop": [{
                "hooks": [{"type": "command", "command": command, "timeout": 10}]
            }],
            "UserPromptSubmit": [{
                "hooks": [{"type": "command", "command": input_command, "timeout": 10}]
            }]
        },
        "permissions": {
            "deny": SIGNAL_BY_NAME_DENIED
                .iter()
                .map(|rule| (*rule).to_owned())
                .chain(deny.iter().cloned())
                .collect::<Vec<_>>()
        },
        "autoMode": {
            "environment": ["$defaults"]
        }
    }))?)
}

/// The settings of a session the runtime starts (a worker, its resume and
/// a planner the runtime starts): [`stop_hook_settings`] with Claude Code's
/// prompt suggestions off (`promptSuggestionEnabled: false`). A suggestion
/// fills the input box with grey text that reads on the screen like a
/// half-typed message, and nobody types in these sessions (goal 48). The
/// sessions a person works in (the inbox, a person's planner) keep them.
pub fn runtime_session_settings(idle_marker: &Path, deny: &[String]) -> Result<String> {
    let mut settings: serde_json::Value =
        serde_json::from_str(&stop_hook_settings(idle_marker, deny)?)?;
    settings["promptSuggestionEnabled"] = serde_json::Value::Bool(false);
    Ok(serde_json::to_string_pretty(&settings)?)
}

/// The Bash permission rules a session's settings deny: commands that
/// signal processes chosen by name or pattern. Every run session's command
/// line holds its prompt, which names the checks (`cargo test`, `cargo
/// llvm-cov`), so a worker's `pkill -f llvm-cov` also ended the other
/// runs' sessions (exit 143) and `integrate`'s checks (task 359). Claude
/// Code applies a deny rule to each command of a `;` / `&&` chain.
pub const SIGNAL_BY_NAME_DENIED: [&str; 2] = ["Bash(pkill:*)", "Bash(killall:*)"];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        PlannerId, PlannerOrigin, ProposalId, Provider, RunId, RunStatus, SessionRole,
    };

    /// The pids a verification command wrote to `dir`, once it wrote them
    /// all (within a bound).
    fn written_pids(dir: &Path, names: &[&str]) -> Vec<u32> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let pids: Vec<Option<u32>> = names
                .iter()
                .map(|name| {
                    fs::read_to_string(dir.join(name))
                        .ok()
                        .and_then(|text| text.trim().parse().ok())
                })
                .collect();
            if pids.iter().all(Option::is_some) {
                return pids.into_iter().flatten().collect();
            }
            assert!(
                Instant::now() < deadline,
                "the pids {names:?} were not written"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// A verification command past its limit is stopped with all it
    /// started: when the call returns, neither its child nor its grandchild
    /// is alive, and the error is the timeout (task 1098).
    #[test]
    fn a_verification_command_past_its_limit_leaves_no_descendant() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let script = "sleep 60 & echo $! > child; \
             sh -c 'sleep 60 & echo $! > grandchild; wait' & echo $! > middle; \
             wait";
        let started = Instant::now();
        let error = run_shell_to_log(
            script,
            dir,
            &[],
            &dir.join("verify.log"),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(40));
        assert_eq!(
            error.downcast_ref::<crate::domain::verify_failure::CommandTimedOut>(),
            Some(&crate::domain::verify_failure::CommandTimedOut { limit_secs: 5 })
        );
        for pid in written_pids(dir, &["child", "middle", "grandchild"]) {
            assert!(!process_alive(pid), "{pid} outlived the timeout");
        }
    }

    /// A descendant that moved to a process group of its own (as
    /// cargo-nextest runs each test) is stopped with the command.
    #[test]
    fn a_descendant_in_a_group_of_its_own_is_stopped_with_the_command() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let script = "perl -e 'setpgrp(0, 0); open my $f, \">\", \"escaped\"; \
             print $f \"$$\\n\"; close $f; sleep 60' & wait";
        let error = run_shell_to_log(
            script,
            dir,
            &[],
            &dir.join("verify.log"),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::domain::verify_failure::CommandTimedOut>()
                .is_some()
        );
        let pid = written_pids(dir, &["escaped"])[0];
        assert!(!process_alive(pid), "{pid} outlived the timeout");
    }

    /// A group that ignores SIGTERM gets SIGKILL after the grace, and the
    /// stop returns once none of it is left.
    #[test]
    fn a_group_that_ignores_sigterm_is_killed_after_the_grace() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("trap '' TERM; sh -c 'sleep 60 & echo $! > grandchild; wait' & echo $! > middle; wait")
            .current_dir(dir)
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pids = written_pids(dir, &["middle", "grandchild"]);
        let leader = child.id();
        stop_verification_group(&mut child, Duration::from_millis(300));
        assert!(!process_alive(leader));
        for pid in pids {
            assert!(!process_alive(pid), "{pid} outlived the stop");
        }
        assert!(!signal_group(leader, 0));
    }

    /// A verification command within its limit returns its exit as before,
    /// its output in the log.
    #[test]
    fn a_verification_command_within_its_limit_returns_its_exit() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("verify.log");
        let status = run_shell_to_log(
            "echo out; echo err >&2; exit 3",
            temp.path(),
            &[("GREETING".to_owned(), "hi".to_owned())],
            &log,
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(status.code(), Some(3));
        assert_eq!(fs::read_to_string(&log).unwrap(), "out\nerr\n");
        let status = run_shell_to_log(
            "test \"$GREETING\" = hi",
            temp.path(),
            &[("GREETING".to_owned(), "hi".to_owned())],
            &log,
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(status.success());
    }

    /// A verification command drops an inherited `NEXTEST_FLAKY_RESULT`
    /// and gets the one the runtime passes (task 1161).
    #[test]
    fn a_verification_command_takes_its_flaky_result_only_from_the_runtime() {
        let flaky_result = |env: &[(String, String)]| {
            let command = verification_command("true", Path::new("/"), env);
            command
                .get_envs()
                .find(|(key, _)| *key == NEXTEST_FLAKY_RESULT)
                .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
        };
        assert_eq!(flaky_result(&[]), Some(None));
        assert_eq!(
            flaky_result(&[(NEXTEST_FLAKY_RESULT.to_owned(), "pass".to_owned())]),
            Some(Some("pass".to_owned()))
        );
    }

    /// Claude Code gets the broker client's server and the permission to
    /// use its tools among its options, before the prompt; nothing else of
    /// the command changes.
    #[test]
    fn claude_gets_the_brokers_mcp_configuration_before_the_prompt() {
        let claude = ClaudeCode {
            executable: PathBuf::from("claude"),
        };
        let mut command = CommandSpec::new("claude");
        command.args(["--settings", "s.json", "--", "the prompt"]);
        assert!(claude.broker_tools(&mut command, Path::new("/r/broker/mcp.json")));
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--settings",
                "s.json",
                "--mcp-config",
                "/r/broker/mcp.json",
                "--allowedTools",
                "mcp__dagq-broker",
                "--",
                "the prompt"
            ]
        );
        let mut resume = CommandSpec::new("claude");
        resume.args(["--resume", "r1"]);
        claude.broker_tools(&mut resume, Path::new("/m.json"));
        let args: Vec<String> = resume
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--resume",
                "r1",
                "--mcp-config",
                "/m.json",
                "--allowedTools",
                "mcp__dagq-broker"
            ]
        );
    }
    #[test]
    fn ps_listings_are_read() {
        assert_eq!(parse_etime("05"), Some(5));
        assert_eq!(parse_etime("01:05"), Some(65));
        assert_eq!(parse_etime("02:01:05"), Some(7265));
        assert_eq!(parse_etime("3-02:01:05"), Some(3 * 86_400 + 7265));
        assert_eq!(parse_etime("x"), None);
        assert_eq!(parse_cpu_time("0:00.01"), Some(10));
        assert_eq!(
            parse_cpu_time("562:29.18"),
            Some((562 * 60 + 29) * 1000 + 180)
        );
        assert_eq!(parse_cpu_time("01:02:03"), Some(3_723_000));
        assert_eq!(parse_cpu_time("1-00:00:01"), Some(86_401_000));
        assert_eq!(parse_cpu_time("0:01.5"), Some(1500));
        assert_eq!(parse_cpu_time("x"), None);
        let processes = parse_ps(
            "  1     0  3-00:00:00 12:00.50 /sbin/launchd\n 42  1 01:05 bad sleep 600 --x\nbad line\n",
        );
        assert_eq!(processes.len(), 2);
        assert_eq!(processes[0].cpu_ms, Some(720_500));
        assert_eq!(processes[1].pid, 42);
        assert_eq!(processes[1].ppid, 1);
        assert_eq!(processes[1].elapsed_secs, 65);
        assert_eq!(processes[1].cpu_ms, None);
        assert_eq!(processes[1].command, "sleep 600 --x");
    }

    /// Task 1581: the directories are read one pid at a time, once for
    /// each listed process and nothing more (no `lsof` of the user's every
    /// process); a pid whose directory cannot be read is left out of what
    /// was read, and its process stays listed with no `cwd`.
    #[test]
    fn each_listed_pid_is_read_once_and_one_unread_stays_listed_without_a_cwd() {
        let processes =
            parse_ps("  7  1 00:05 0:00.01 a\n  8  7 00:05 0:00.01 b\n  9  7 00:05 0:00.01 c\n");
        let mut reads = Vec::new();
        let read = |pid: u32| {
            reads.push(pid);
            (pid != 8).then(|| format!("/w/{pid}"))
        };
        assert_eq!(
            working_directories(&processes, read),
            [(7, "/w/7".to_owned()), (9, "/w/9".to_owned())]
        );
        assert_eq!(reads, [7, 8, 9]);
        let mut reads = 0;
        let listed = with_working_directories(processes, |pid| {
            reads += 1;
            (pid != 8).then(|| format!("/w/{pid}"))
        });
        assert_eq!(reads, 3);
        let cwds: Vec<_> = listed.iter().map(|p| (p.pid, p.cwd.as_deref())).collect();
        assert_eq!(cwds, [(7, Some("/w/7")), (8, None), (9, Some("/w/9"))]);
    }

    /// Task 1581: macOS reads a process's directory with `proc_pidinfo`,
    /// a child's as it was started in, and none for a pid that runs
    /// nothing.
    #[cfg(target_os = "macos")]
    #[test]
    fn proc_pidinfo_reads_a_childs_directory_and_none_for_a_pid_that_runs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .current_dir(&dir)
            .spawn()
            .unwrap();
        let cwd = process_cwd(child.id());
        let _ = child.kill();
        let _ = child.wait();
        let cwd = PathBuf::from(cwd.unwrap());
        assert_eq!(cwd.canonicalize().unwrap(), dir);
        assert_eq!(process_cwd(child.id()), None);
        assert_eq!(process_cwd(u32::MAX), None);
    }

    #[test]
    fn this_users_processes_are_listed_with_their_directories() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .current_dir(&dir)
            .spawn()
            .unwrap();
        let listed = SystemProcesses.list().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        let found = listed.iter().find(|p| p.pid == child.id()).unwrap();
        assert_eq!(found.ppid, std::process::id());
        assert!(found.command.contains("sleep 30"), "{found:?}");
        assert!(found.cpu_ms.is_some(), "{found:?}");
        let cwd = PathBuf::from(found.cwd.as_deref().unwrap());
        assert_eq!(cwd.canonicalize().unwrap(), dir);
    }

    #[test]
    fn a_process_start_reads_the_same_until_the_pid_runs_nothing() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let first = SystemProcesses.start_identity(child.id()).unwrap();
        assert_eq!(SystemProcesses.start_identity(child.id()), Some(first));
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(SystemProcesses.start_identity(child.id()), None);
        assert_eq!(SystemProcesses.start_identity(u32::MAX), None);
    }

    #[test]
    fn a_process_started_at_is_about_when_it_was_spawned_until_it_runs_nothing() {
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let started = SystemProcesses.started_at(child.id()).unwrap();
        assert!(
            (before - 2..=before + 5).contains(&started),
            "{started} against {before}"
        );
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(SystemProcesses.started_at(child.id()), None);
        assert_eq!(SystemProcesses.started_at(u32::MAX), None);
    }

    #[test]
    fn a_long_paste_gets_more_time_before_its_enter() {
        assert_eq!(paste_settle(5), Duration::from_millis(300));
        assert_eq!(paste_settle(13_000), Duration::from_millis(1600));
        assert_eq!(paste_settle(1_000_000), Duration::from_secs(3));
        assert_eq!(single_line("a\\b\n\n  c\t"), "a/b   c");
    }

    #[test]
    fn system_processes_signal_a_child_and_see_it_gone() {
        let processes = SystemProcesses;
        for send in [
            SystemProcesses::terminate,
            SystemProcesses::interrupt,
            SystemProcesses::kill,
        ] {
            let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
            assert!(processes.alive(child.id()));
            send(&processes, child.id()).unwrap();
            assert!(!child.wait().unwrap().success());
            assert!(!processes.alive(child.id()));
            let error = send(&processes, child.id()).unwrap_err().to_string();
            assert!(error.contains(&format!("pid {}", child.id())), "{error}");
        }
        assert!(!process_alive(u32::MAX));
        assert!(processes.kill(u32::MAX).is_err());
    }

    /// Another process holding the command's output open (as one spawned
    /// at the same moment can inherit it, task 1022) does not hold the
    /// capture past the command's own exit.
    #[test]
    fn capture_returns_at_the_exit_though_another_process_holds_the_output() {
        let started = Instant::now();
        let (status, stdout, stderr) = capture(
            Command::new("/bin/sh").args(["-c", "/bin/sleep 60 & echo $!; echo err >&2"]),
            Duration::from_secs(60),
        )
        .unwrap();
        let held = started.elapsed();
        let holder: libc::pid_t = stdout.trim().parse().unwrap();
        // SAFETY: kill(2) on the pid of the sleep the shell reported.
        unsafe { libc::kill(holder, libc::SIGKILL) };
        assert!(status.success());
        assert_eq!(stderr, "err\n");
        assert!(held < Duration::from_secs(30), "held for {held:?}");
    }

    /// [`unpiped_output`] returns its command's output and exit at the
    /// command's exit, while another process still holds the output open.
    #[test]
    fn unpiped_output_returns_at_the_exit_though_another_process_holds_the_output() {
        let started = Instant::now();
        let output = unpiped_output(
            Command::new("/bin/sh").args(["-c", "/bin/sleep 60 & echo $!; echo err >&2; exit 3"]),
        )
        .unwrap();
        let held = started.elapsed();
        let holder: libc::pid_t = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap();
        // SAFETY: kill(2) on the pid of the sleep the shell reported.
        unsafe { libc::kill(holder, libc::SIGKILL) };
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stderr, b"err\n");
        assert!(held < Duration::from_secs(30), "held for {held:?}");
        let missing = unpiped_output(&mut Command::new("/nonexistent/dagq-none")).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }

    fn git_in(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new(git_executable().expect("git executable"))
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
    }

    /// A repository on `main` with one committed `change.txt`.
    fn committed_repository() -> (tempfile::TempDir, GitRepository) {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@example.com"],
            &["config", "user.name", "t"],
        ] {
            assert!(git_in(dir.path(), args).status.success());
        }
        fs::write(dir.path().join("change.txt"), "0\n").unwrap();
        assert!(git_in(dir.path(), &["add", "change.txt"]).status.success());
        assert!(
            git_in(dir.path(), &["commit", "-q", "-m", "c"])
                .status
                .success()
        );
        let git = GitRepository::inspect(dir.path()).unwrap();
        (dir, git)
    }

    #[test]
    fn committed_files_ignore_checkout_edits_and_report_missing_or_unreadable_blobs() {
        let (dir, git) = committed_repository();
        let head = git.main_head().unwrap();
        fs::write(dir.path().join("change.txt"), "uncommitted").unwrap();
        assert_eq!(
            git.file_in(head.as_str(), "change.txt").unwrap(),
            Some("0\n".into())
        );
        assert_eq!(git.file_in(head.as_str(), "absent").unwrap(), None);
        assert!(git.file_in("refs/heads/missing", "change.txt").is_err());
        fs::write(dir.path().join("binary"), [0xff]).unwrap();
        assert!(git_in(dir.path(), &["add", "binary"]).status.success());
        assert!(
            git_in(dir.path(), &["commit", "-qm", "binary"])
                .status
                .success()
        );
        assert!(
            git.file_in(git.main_head().unwrap().as_str(), "binary")
                .is_err()
        );
    }

    /// The landing branch follows origin's HEAD before `main` and
    /// `master`, and `[repository] branch` of the main checkout's
    /// dagq.toml before all of them; an invalid name is refused.
    #[test]
    fn landing_branch_follows_origin_head_and_the_config() {
        use crate::domain::landing_branch::BranchSource;
        let (dir, git) = committed_repository();
        let source = |git: &GitRepository| {
            let branch = git.landing_branch().unwrap();
            (branch.name, branch.source)
        };
        assert_eq!(source(&git), ("main".into(), BranchSource::Main));
        assert!(git_in(dir.path(), &["branch", "dev"]).status.success());
        // origin's HEAD, as a clone leaves it, without a network call.
        let head = git_in(dir.path(), &["rev-parse", "HEAD"]);
        let head = String::from_utf8_lossy(&head.stdout).trim().to_owned();
        for args in [
            &["remote", "add", "origin", "/nonexistent/origin.git"][..],
            &["update-ref", "refs/remotes/origin/dev", &head],
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/dev",
            ],
        ] {
            assert!(git_in(dir.path(), args).status.success(), "{args:?}");
        }
        assert_eq!(source(&git), ("dev".into(), BranchSource::RemoteHead));
        assert_eq!(git.main_head().unwrap().as_str(), head);
        fs::write(
            dir.path().join("dagq.toml"),
            "[repository]\nbranch = \"main\"\n",
        )
        .unwrap();
        assert_eq!(source(&git), ("main".into(), BranchSource::Config));
        assert_eq!(
            git.main_checkout().unwrap(),
            Some(PathBuf::from(
                String::from_utf8_lossy(
                    &git_in(dir.path(), &["rev-parse", "--show-toplevel"]).stdout
                )
                .trim()
            ))
        );
        fs::write(
            dir.path().join("dagq.toml"),
            "[repository]\nbranch = \"a..b\"\n",
        )
        .unwrap();
        let error = format!("{:#}", git.landing_branch().unwrap_err());
        assert!(error.contains("not a valid branch name"), "{error}");
        fs::write(dir.path().join("dagq.toml"), "[repository]\nbranch = 1\n").unwrap();
        let error = format!("{:#}", git.main_head().unwrap_err());
        assert!(error.contains("[repository]"), "{error}");
    }

    /// The stamp of the landing branch's inputs (task 1078) stays the same
    /// while nothing it reads changes, and changes with the landing branch
    /// deleted or created, a new commit on it, packed refs, the remote's
    /// HEAD, the branch it names, and `dagq.toml`.
    #[test]
    fn the_landing_branch_stamp_follows_what_the_resolution_reads() {
        let (dir, git) = committed_repository();
        let run = |args: &[&str]| assert!(git_in(dir.path(), args).status.success(), "{args:?}");
        let mut last = git.landing_branch_stamp().unwrap();
        assert_eq!(git.landing_branch_stamp().unwrap(), last);
        // Reading the branch, the log or the status changes nothing.
        run(&["log", "-1"]);
        run(&["status", "--short"]);
        git.landing_branch().unwrap();
        assert_eq!(git.landing_branch_stamp().unwrap(), last);
        let changed = |step: &str, last: &mut LandingBranchStamp| {
            let now = git.landing_branch_stamp().unwrap();
            assert_ne!(&now, last, "{step}");
            *last = now;
        };
        run(&["branch", "-m", "main", "trunk"]);
        changed("main renamed away", &mut last);
        run(&["branch", "master", "trunk"]);
        changed("master created", &mut last);
        run(&["update-ref", "-d", "refs/heads/master"]);
        changed("master deleted", &mut last);
        // origin's HEAD names trunk, then trunk moves on.
        run(&["update-ref", "refs/remotes/origin/trunk", "trunk"]);
        run(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk",
        ]);
        changed("origin's HEAD set", &mut last);
        run(&["commit", "-q", "--allow-empty", "-m", "on trunk"]);
        changed("a commit on the branch origin's HEAD names", &mut last);
        run(&["pack-refs", "--all"]);
        changed("refs packed", &mut last);
        run(&["update-ref", "-d", "refs/heads/trunk"]);
        changed("a packed branch deleted", &mut last);
        run(&[
            "update-ref",
            "refs/heads/trunk",
            "refs/remotes/origin/trunk",
        ]);
        changed("the branch created again", &mut last);
        fs::write(
            dir.path().join("dagq.toml"),
            "[repository]\nbranch = \"dev\"\n",
        )
        .unwrap();
        changed("dagq.toml names a branch", &mut last);
        run(&["update-ref", "refs/heads/dev", "trunk"]);
        changed("the named branch created", &mut last);
        assert_eq!(git.landing_branch().unwrap().name, "dev");
        assert_eq!(git.landing_branch_stamp().unwrap(), last);
    }

    /// `landed_changes` reads the paths of many commits in one `git log`,
    /// both sides of a rename, leaving out a commit the repository lacks.
    #[test]
    fn landed_changes_reads_the_commits_paths_at_once() {
        let (dir, git) = committed_repository();
        let head = |dir: &Path| {
            String::from_utf8_lossy(&git_in(dir, &["rev-parse", "HEAD"]).stdout)
                .trim()
                .to_owned()
        };
        let first = head(dir.path());
        fs::create_dir(dir.path().join("docs")).unwrap();
        fs::write(dir.path().join("docs/a.md"), "a\n").unwrap();
        for step in [
            &["add", "docs/a.md"][..],
            &["mv", "change.txt", "moved.txt"],
            &["commit", "-q", "-m", "second"],
        ] {
            assert!(git_in(dir.path(), step).status.success(), "{step:?}");
        }
        let second = head(dir.path());
        let missing = "0123456789012345678901234567890123456789".to_owned();
        let changes = git
            .landed_changes(&[second.clone(), missing.clone(), first.clone()])
            .unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[&first], ["change.txt"]);
        assert_eq!(changes[&second], ["change.txt", "docs/a.md", "moved.txt"]);
        assert!(git.landed_changes(&[missing]).unwrap().is_empty());
        assert!(git.landed_changes(&[]).unwrap().is_empty());
    }

    /// `main_history` lists main's commits with the paths each changed,
    /// following a rename and seeing a deletion, and the paths main has.
    #[test]
    fn main_history_follows_renames_and_deletions() {
        let (dir, git) = committed_repository();
        let commit = |args: &[&[&str]]| {
            for step in args {
                assert!(git_in(dir.path(), step).status.success(), "{step:?}");
            }
            assert!(
                git_in(dir.path(), &["commit", "-q", "-m", "c"])
                    .status
                    .success()
            );
        };
        fs::write(dir.path().join("keep.txt"), "k\n").unwrap();
        commit(&[&["add", "keep.txt"]]);
        commit(&[&["mv", "change.txt", "moved.txt"]]);
        commit(&[&["rm", "-q", "keep.txt"]]);
        let history = git.main_history(0).unwrap();
        assert_eq!(history.commits.len(), 4);
        assert_eq!(history.commits[0].changes[0].path, "change.txt");
        let renamed = &history.commits[2].changes[0];
        assert_eq!(
            (renamed.path.as_str(), renamed.from.as_deref()),
            ("moved.txt", Some("change.txt"))
        );
        let deleted = &history.commits[3].changes[0];
        assert!(deleted.deleted && deleted.path == "keep.txt");
        assert_eq!(
            history.paths,
            ["moved.txt".to_owned()].into_iter().collect()
        );
        // Nothing since a time after the last commit.
        assert!(
            git.main_history(history.commits[3].at + 3600)
                .unwrap()
                .commits
                .is_empty()
        );
        let copied = parse_main_log("\u{1}5\0\nC75\0a\0b\0M\0c\0\u{1}x\0\nM\0d\0\nR100\0e");
        assert_eq!(copied.len(), 1);
        assert_eq!(copied[0].changes.len(), 3);
        assert_eq!(copied[0].changes[0].path, "b");
        assert_eq!(copied[0].changes[0].from, None);
        assert_eq!(copied[0].changes[1].path, "c");
        assert_eq!(copied[0].changes[2].path, "d");
        // A path Git would quote without -z is read as it is.
        fs::write(dir.path().join("a\"b.txt"), "q\n").unwrap();
        commit(&[&["add", "a\"b.txt"]]);
        let quoted = git.main_history(0).unwrap();
        assert_eq!(quoted.commits[4].changes[0].path, "a\"b.txt");
    }

    /// The supervisor's `status` leaves the index as it found it, where a
    /// plain `git status` refreshes the stale stat data and writes the
    /// index back under `index.lock`.
    #[test]
    fn worktree_status_does_not_write_back_the_index() {
        let (dir, git) = committed_repository();
        let index = dir.path().join(".git/index");
        let before = fs::read(&index).unwrap();
        // Same content, new stat data: the index entry is stale.
        std::thread::sleep(Duration::from_millis(20));
        fs::remove_file(dir.path().join("change.txt")).unwrap();
        fs::write(dir.path().join("change.txt"), "0\n").unwrap();

        assert_eq!(git.status(dir.path()).unwrap(), "");
        assert_eq!(
            git.current_branch(dir.path()).unwrap().as_deref(),
            Some("refs/heads/main")
        );
        git.head(dir.path()).unwrap();
        assert!(!git.rebase_in_progress(dir.path()).unwrap());
        assert!(git.conflicted_files(dir.path()).unwrap().is_empty());
        assert_eq!(fs::read(&index).unwrap(), before, "index was rewritten");

        assert!(
            git_in(dir.path(), &["status", "--porcelain"])
                .status
                .success()
        );
        assert_ne!(
            fs::read(&index).unwrap(),
            before,
            "a plain git status should refresh the stale index"
        );
    }

    /// A session's `git add` never meets the supervisor's `index.lock`
    /// while `status` polls the same worktree in a tight loop, and the
    /// index ends with what the session staged.
    #[test]
    fn worktree_status_polling_does_not_block_a_sessions_git_add() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (dir, git) = committed_repository();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let poller = {
            let (stop, git, path) = (stop.clone(), git.clone(), dir.path().to_owned());
            std::thread::spawn(move || {
                let mut polls = 0;
                loop {
                    git.status(&path).unwrap();
                    polls += 1;
                    if stop.load(Ordering::Relaxed) {
                        break polls;
                    }
                }
            })
        };
        for round in 1..=40 {
            fs::write(dir.path().join("change.txt"), format!("{round}\n")).unwrap();
            let add = git_in(dir.path(), &["add", "change.txt"]);
            assert!(
                add.status.success(),
                "round {round}: {}",
                String::from_utf8_lossy(&add.stderr)
            );
        }
        stop.store(true, Ordering::Relaxed);
        assert!(poller.join().unwrap() > 0);
        assert!(!dir.path().join(".git/index.lock").exists());
        let staged = git_in(dir.path(), &["show", ":change.txt"]);
        assert_eq!(String::from_utf8_lossy(&staged.stdout), "40\n");
        assert_eq!(git.status(dir.path()).unwrap(), "M  change.txt\n");
    }

    #[test]
    fn a_default_push_report_is_skipped_for_origin() {
        let report = crate::domain::PushReport::default();
        assert_eq!(report.outcome, crate::domain::PushResult::Skipped);
        assert_eq!(report.remote, "origin");
        assert!(report.error.is_none() && report.reason.is_none());
    }

    /// A planner the runtime started turns Claude Code's prompt
    /// suggestions off like a worker; a person's planner keeps them (goal
    /// 48), with the same hooks either way.
    #[test]
    fn only_a_runtime_planner_turns_the_prompt_suggestions_off() {
        let claude = ClaudeCode {
            executable: "/bin/claude".into(),
        };
        let settings_of = |origin| {
            let dir = tempfile::tempdir().unwrap();
            let planner = PlannerCommand {
                origin,
                dir: dir.path(),
                cwd: dir.path(),
                prompt: "plan",
                plugin_dir: None,
            };
            let command = claude.planner_command(&planner).unwrap();
            let path = dir.path().join("claude-settings.json");
            assert!(command.get_args().any(|arg| arg == path.as_os_str()));
            let settings: Value =
                serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            let expected: Value = serde_json::from_str(
                &stop_hook_settings(&planner.idle_marker(), &permission_deny(ActorRole::Planner))
                    .unwrap(),
            )
            .unwrap();
            (settings, expected)
        };
        let (runtime, mut expected) = settings_of(PlannerOrigin::Runtime);
        assert_eq!(runtime["promptSuggestionEnabled"], Value::Bool(false));
        expected["promptSuggestionEnabled"] = Value::Bool(false);
        assert_eq!(runtime, expected);
        let (person, expected) = settings_of(PlannerOrigin::Person);
        assert_eq!(person.get("promptSuggestionEnabled"), None);
        assert_eq!(person, expected);
        // Either way the planner's policy denies it landing and answering.
        let deny = person["permissions"]["deny"].as_array().unwrap();
        for rule in [
            "Bash(pkill:*)",
            "Bash(dagq integrate:*)",
            "Bash(dagq answer:*)",
        ] {
            assert!(deny.contains(&Value::from(rule)), "{rule}");
        }
        assert!(!deny.contains(&Value::from("Bash(dagq submit:*)")));
    }

    #[test]
    fn claude_headless_command_prints_with_only_the_allowed_tools() {
        let claude = ClaudeCode {
            executable: "/bin/claude".into(),
        };
        let command = claude
            .headless_command(Path::new("/tmp/obs"), "observe", JobAccess::QueueCli)
            .unwrap();
        assert_eq!(command.get_program(), "/bin/claude");
        assert_eq!(command.get_current_dir(), Some(Path::new("/tmp/obs")));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["-p", "--allowedTools", "Bash(dagq:*)"]
        );
        // The prompt is its standard input, never an argument (task 1560).
        assert_eq!(command.get_stdin(), Some("observe"));
        // A job's session id goes among the options (ADR-0048 decision 4).
        let mut named = claude
            .headless_command(Path::new("/tmp"), "p", JobAccess::QueueCli)
            .unwrap();
        claude.assign_session_id(&mut named, "s-1");
        assert_eq!(
            named.get_args().collect::<Vec<_>>(),
            [
                "-p",
                "--allowedTools",
                "Bash(dagq:*)",
                "--session-id",
                "s-1",
            ]
        );
        // The observer loads no MCP server (ADR-0044).
        claude.without_mcp(&mut named);
        assert_eq!(
            named.get_args().collect::<Vec<_>>(),
            [
                "-p",
                "--allowedTools",
                "Bash(dagq:*)",
                "--session-id",
                "s-1",
                "--strict-mcp-config",
            ]
        );
        let mut plain = CommandSpec::new("x");
        plain.arg("a").option_args(["b"]);
        assert_eq!(plain.get_args().collect::<Vec<_>>(), ["a", "b"]);
    }

    /// Every headless job's intent becomes the same `--allowedTools` its
    /// Claude job was started with before jobs named intents (task 1064):
    /// the recovery job reads files, the plan and goal reviews read files
    /// and run the queue CLI, the observer and the throughput review run
    /// the queue CLI only.
    #[test]
    fn each_jobs_intent_starts_claude_with_the_tools_it_had() {
        let claude = ClaudeCode {
            executable: "/bin/claude".into(),
        };
        for (job, access, tools) in [
            (
                "recovery",
                crate::application::prompt::TRIAGE_ACCESS,
                &["Read", "Grep", "Glob"][..],
            ),
            (
                "plan review",
                crate::application::prompt::PLAN_REVIEW_ACCESS,
                &["Read", "Grep", "Glob", "Bash(dagq:*)"][..],
            ),
            (
                "goal review",
                crate::application::prompt::GOAL_REVIEW_ACCESS,
                &["Read", "Grep", "Glob", "Bash(dagq:*)"][..],
            ),
            (
                "observer",
                crate::application::observer::ACCESS,
                &["Bash(dagq:*)"][..],
            ),
            (
                "throughput review",
                crate::throughput_review::ACCESS,
                &["Bash(dagq:*)"][..],
            ),
        ] {
            let command = claude
                .headless_command(Path::new("/tmp/job"), "p", access)
                .unwrap();
            let mut expected = vec!["-p", "--allowedTools"];
            expected.extend(tools);
            assert_eq!(command.get_args().collect::<Vec<_>>(), expected, "{job}");
            assert_eq!(command.get_stdin(), Some("p"), "{job}");
        }
    }

    #[test]
    fn the_review_starts_claude_with_the_tools_it_had() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_in(dir.path());
        let claude = ClaudeCode {
            executable: "/bin/claude".into(),
        };
        let command = claude
            .review_command(&run, "review it", crate::application::prompt::REVIEW_ACCESS)
            .unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let run_dir = dir.path().join("run");
        let run_dir = run_dir.to_string_lossy();
        assert_eq!(
            args,
            [
                "-p",
                "--debug-file",
                &format!("{run_dir}/claude-review.log"),
                "--add-dir",
                &run_dir,
                "--settings",
                &format!("{run_dir}/claude-review-settings.json"),
                "--allowedTools",
                "Read,Grep,Glob",
                "--disallowedTools",
                "Bash,Edit,Write,NotebookEdit",
                "--setting-sources",
                "",
            ]
        );
        // The prompt is its standard input, never an argument (task 1560).
        assert_eq!(command.get_stdin(), Some("review it"));
        // The review's settings carry no hook (it never writes the live
        // session's idle marker).
        let settings: Value = serde_json::from_str(
            &fs::read_to_string(dir.path().join("run/claude-review-settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings.get("hooks"), None);
        // Nor does it load the worktree's auto memory (ADR-t1470-1).
        assert_eq!(settings["autoMemoryEnabled"], Value::Bool(false));
        // A review that may run the queue CLI keeps `Bash(dagq:*)` and is
        // refused only the edits.
        let command = claude
            .review_command(&run, "p", JobAccess::ReadFilesAndQueueCli)
            .unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert!(args.contains(&std::ffi::OsStr::new("Read,Grep,Glob,Bash(dagq:*)")));
        assert!(args.contains(&std::ffi::OsStr::new("Edit,Write,NotebookEdit")));
    }

    /// A review that requires subagents hands Claude their definitions as
    /// `--agents`, allowed only the reads, and lets the review start them
    /// with `Agent` (ADR-t1453-1 decision 8). With or without them the
    /// review loads no setting sources, so the worktree's `.claude/agents`
    /// and `.claude/settings.json` are not read (ADR-t1470-1 decision 1),
    /// and the review's settings and refusals stay.
    #[test]
    fn the_review_hands_claude_its_subagents_and_no_worktree_settings() {
        use crate::domain::review_subagents::AgentDefinition;
        let dir = tempfile::tempdir().unwrap();
        let run = run_in(dir.path());
        let claude = ClaudeCode {
            executable: "/bin/claude".into(),
        };
        assert!(claude.runs_review_subagents());
        let plain = claude
            .review_command(&run, "review it", crate::application::prompt::REVIEW_ACCESS)
            .unwrap();
        let mut command = plain.clone();
        let agents = [
            AgentDefinition::read(
                "design",
                "---\ndescription: design checks\n---\nCheck it.\n",
            ),
            AgentDefinition::read("plain", "No frontmatter.\n"),
        ];
        claude.review_subagents(&mut command, &agents).unwrap();
        let args = |command: &CommandSpec| -> Vec<String> {
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };
        let before = args(&plain);
        let after = args(&command);
        assert_eq!(after[..before.len()], before[..]);
        assert_eq!(command.get_stdin(), Some("review it"));
        let added = &after[before.len()..];
        assert_eq!(added[0], "--agents");
        assert_eq!(added[2..], ["--allowedTools", "Agent"]);
        let handed: Value = serde_json::from_str(&added[1]).unwrap();
        assert_eq!(
            handed,
            serde_json::json!({
                "design": {"description": "design checks", "prompt": "Check it.\n", "tools": ["Read", "Grep", "Glob"]},
                "plain": {"description": "The review subagent plain", "prompt": "No frontmatter.\n", "tools": ["Read", "Grep", "Glob"]},
            })
        );
        // Both load no setting sources and keep the review's own settings
        // and refusals, which reach the subagents.
        let run_dir = dir.path().join("run");
        let settings = run_dir.join("claude-review-settings.json");
        for args in [&before, &after] {
            for pair in [
                ["--setting-sources", ""],
                ["--settings", &settings.to_string_lossy()],
                ["--allowedTools", "Read,Grep,Glob"],
                ["--disallowedTools", "Bash,Edit,Write,NotebookEdit"],
            ] {
                assert!(args.windows(2).any(|w| w == pair), "{pair:?} in {args:?}");
            }
        }
        // Without subagents nothing of them is in the command.
        for absent in ["--agents", "Agent"] {
            assert!(!before.iter().any(|arg| arg == absent), "{absent}");
        }
    }

    /// A prompt past the system's limit on the arguments (about 1 MB on
    /// macOS) starts a headless job and a review: the stub `claude` reads it
    /// whole on its standard input, and its arguments do not carry it (task
    /// 1560).
    #[test]
    fn a_claude_job_starts_with_a_prompt_past_the_argument_limit() {
        use crate::infrastructure::process::stub_agent;
        let dir = tempfile::tempdir().unwrap();
        let run = run_in(dir.path());
        fs::create_dir_all(dir.path().join("worktree")).unwrap();
        let claude = ClaudeCode {
            executable: stub_agent::write(dir.path()),
        };
        let prompt = "p".repeat(2 << 20);
        let job = claude
            .headless_command(dir.path(), &prompt, JobAccess::ReadFilesAndQueueCli)
            .unwrap();
        let review = claude
            .review_command(&run, &prompt, crate::application::prompt::REVIEW_ACCESS)
            .unwrap();
        for command in [job, review] {
            let (args, stdin) = stub_agent::run(&command, dir.path());
            assert_eq!(stdin, prompt.len());
            assert!(args < 4096, "{args} bytes of arguments");
        }
    }

    #[test]
    fn a_claude_jobs_reply_is_its_stdout() {
        let claude = ClaudeCode {
            executable: "/bin/claude".into(),
        };
        let stdout = "Looked at it.\n{\"verdict\":\"pass\",\"summary\":\"ok\"}\n";
        assert_eq!(claude.job_reply(stdout), stdout);
    }

    /// The planners' settings (a planner session's and a headless
    /// planner's turns) and the inbox's refuse raw cmux beside their role's
    /// denials and the identity variables; a worker's, its turns' and the
    /// review's do not (ADR-t1228-2 decisions 2, 3 and 7).
    #[test]
    fn raw_cmux_is_denied_to_the_planners_and_the_inbox_only() {
        let claude = ClaudeCode {
            executable: "/bin/claude".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        let deny_of = |path: &Path| -> Vec<String> {
            let settings: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
            serde_json::from_value(settings["permissions"]["deny"].clone()).unwrap()
        };
        let cmux = crate::application::execution::RAW_CMUX_DENIED.to_owned();
        let identity = "Bash(export DAGQ_ROLE*)".to_owned();
        let turn = |role: ActorRole, sub: &str| {
            let target_dir = dir.path().join(sub);
            fs::create_dir_all(&target_dir).unwrap();
            let log = target_dir.join("turn.log");
            claude
                .turn_command(
                    &TurnTarget {
                        role,
                        dir: &target_dir,
                        cwd: &target_dir,
                        debug_log: Some(&log),
                        plugin_dir: None,
                    },
                    "go",
                    crate::domain::turn::TurnSession::New("s"),
                )
                .unwrap();
            deny_of(&target_dir.join(HEADLESS_SETTINGS))
        };
        let planner_turn = turn(ActorRole::Planner, "planner");
        assert!(planner_turn.contains(&cmux), "{planner_turn:?}");
        assert!(planner_turn.contains(&identity));
        assert!(planner_turn.contains(&"Bash(dagq integrate:*)".to_owned()));
        let worker_turn = turn(ActorRole::Worker, "worker");
        assert!(!worker_turn.contains(&cmux), "{worker_turn:?}");
        assert!(worker_turn.contains(&identity));

        let planner_dir = dir.path().join("session");
        fs::create_dir_all(&planner_dir).unwrap();
        let planner = PlannerCommand {
            origin: PlannerOrigin::Runtime,
            dir: &planner_dir,
            cwd: &planner_dir,
            prompt: "plan",
            plugin_dir: None,
        };
        claude.planner_command(&planner).unwrap();
        let session = deny_of(&planner_dir.join("claude-settings.json"));
        assert!(session.contains(&cmux) && session.contains(&identity));

        let run = run_in(dir.path());
        fs::create_dir_all(run.run_dir().unwrap()).unwrap();
        claude
            .review_command(&run, "review", crate::application::prompt::REVIEW_ACCESS)
            .unwrap();
        let review =
            deny_of(&Path::new(run.run_dir().unwrap()).join("claude-review-settings.json"));
        assert!(!review.contains(&cmux) && review.contains(&identity));

        let queue_dir = dir.path().join("queue");
        fs::create_dir_all(&queue_dir).unwrap();
        let command = claude.inbox_command("inbox", None, &queue_dir).unwrap();
        let path = queue_dir.join(INBOX_SETTINGS);
        assert_eq!(claude.inbox_settings(&queue_dir), Some(path.clone()));
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args[..2],
            [std::ffi::OsStr::new("--settings"), path.as_os_str()]
        );
        let settings: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            settings,
            serde_json::json!({"permissions": {"deny": permission_deny(ActorRole::Inbox)}})
        );
        assert!(deny_of(&path).contains(&cmux));
    }

    #[test]
    fn every_role_gets_its_settings_from_one_table() {
        for role in ActorRole::ALL {
            let expected = match role {
                ActorRole::Worker => AgentSettings::Session { suggestions: false },
                ActorRole::Planner => AgentSettings::Session { suggestions: false },
                ActorRole::ReviewJob => AgentSettings::Review,
                _ => AgentSettings::None,
            };
            assert_eq!(agent_settings(role, None), expected, "{role:?}");
        }
        assert_eq!(
            agent_settings(ActorRole::Planner, Some(PlannerOrigin::Person)),
            AgentSettings::Session { suggestions: true }
        );
        assert_eq!(
            agent_settings(ActorRole::Planner, Some(PlannerOrigin::Runtime)),
            AgentSettings::Session { suggestions: false }
        );
    }

    fn run(repo_path: Option<&str>) -> TaskRun {
        TaskRun::restore(record(repo_path)).unwrap()
    }

    /// A run whose worktree and run directory are under `dir`.
    fn run_in(dir: &Path) -> TaskRun {
        let run_dir = dir.join("run");
        fs::create_dir_all(&run_dir).unwrap();
        let mut record = record(None);
        record.worktree_path = Some(dir.join("worktree").to_string_lossy().into_owned());
        record.run_dir = Some(run_dir.to_string_lossy().into_owned());
        TaskRun::restore(record).unwrap()
    }

    fn record(repo_path: Option<&str>) -> crate::domain::RunRecord {
        crate::domain::RunRecord {
            id: RunId::new("0d8e3f1a-7c1b-4e35-9a11-3f6d2c9b8e47").unwrap(),
            task_id: TaskId::new(15),
            status: RunStatus::Claimed,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            worker_mode: crate::domain::worker::WorkerMode::Interactive,
            base_commit: CommitSha::try_from("a".repeat(40)).unwrap(),
            branch: None,
            worktree_path: None,
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit: None,
            repo_path: repo_path.map(str::to_owned),
            run_dir: None,
            last_error: None,
            workspace_closed_at: None,
            created_at: "2026-09-22 00:00:00".into(),
        }
    }

    fn task(title: &str) -> Task {
        Task::restore(crate::domain::TaskRecord {
            id: TaskId::new(15),
            title: title.into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            status: crate::domain::TaskStatus::InProgress,
            goal_id: None,
            context: String::new(),
            created_at: "2026-09-22 00:00:00".into(),
            updated_at: "2026-09-22 00:00:00".into(),
            worker: crate::domain::worker::Worker::CLAUDE_INTERACTIVE,
            named_mode: None,
        })
        .unwrap()
    }

    /// One cmux serves several repositories, so every workspace name
    /// carries the repository (the basename of its root). A worker's name
    /// carries the task and its title as is; the run ID goes to the
    /// description instead (ADR-0018, ADR-0028).
    #[test]
    fn workspace_names_carry_the_repository_and_the_task() {
        let title = "Set last_error when a run fails";
        assert_eq!(
            run_workspace_name(&task(title), &run(Some("/home/u/ghq/dagq"))).unwrap(),
            "[dagq]worker#15 - Set last_error when a run fails"
        );
        // The title is neither trimmed nor shortened.
        let long = format!("  {}  ", "x".repeat(200));
        assert_eq!(
            run_workspace_name(&task(&long), &run(Some("/tmp/my repo/"))).unwrap(),
            format!("[my repo]worker#15 - {long}")
        );
        assert!(
            !run_workspace_name(&task(title), &run(Some("/home/u/ghq/dagq")))
                .unwrap()
                .contains("0d8e3f1a")
        );
        assert!(
            run_workspace_name(&task(title), &run(None))
                .unwrap_err()
                .to_string()
                .contains("missing repository path")
        );
        // A root with no basename falls back to the path itself.
        assert_eq!(
            planner_workspace_name(Path::new("/"), PlannerId::new(1), None),
            "[/]planner#1"
        );
        assert_eq!(
            supervisor_workspace_name(Path::new("/home/u/ghq/dagq")),
            "[dagq]supervisor"
        );
        assert_eq!(
            planner_workspace_name(
                Path::new("/home/u/ghq/dagq"),
                PlannerId::new(3),
                Some(ProposalId::new(7))
            ),
            "[dagq]planner#3 - proposal 7"
        );
        assert_eq!(
            inbox_workspace_name(Path::new("/tmp/my repo/")),
            "[my repo]inbox"
        );
    }

    /// Every workspace of a queue says what it is in one line; the run and
    /// the task appear only where the workspace has them (ADR-0026).
    #[test]
    fn workspace_descriptions_are_one_machine_readable_line() {
        assert_eq!(
            workspace_description(
                SessionRole::Worker,
                "77067154921b9014",
                Some(&RunId::new("0d8e3f1a-7c1b-4e35-9a11-3f6d2c9b8e47").unwrap()),
                Some(TaskId::new(15))
            ),
            "dagq role=worker queue=77067154921b9014 run=0d8e3f1a-7c1b-4e35-9a11-3f6d2c9b8e47 task=15"
        );
        assert_eq!(
            workspace_description(SessionRole::Inbox, "abc", None, None),
            "dagq role=inbox queue=abc"
        );
        assert_eq!(
            workspace_description(SessionRole::Supervisor, "abc", None, None),
            "dagq role=supervisor queue=abc"
        );
        assert_eq!(
            workspace_group_name(Path::new("/home/u/ghq/dagq")),
            "[dagq]"
        );
        assert_eq!(workspace_group_name(Path::new("/")), "[/]");
    }

    /// A workspace is found by its UUID, never its title, and cmux may
    /// print the UUID in either case.
    #[test]
    fn workspace_listed_matches_the_id_only() {
        let listing = serde_json::json!({
            "window_id": "W",
            "workspaces": [
                {"id": "4AC63CB7-3BE1-40A1-BCC4-CA0461685F01", "title": "[dagq]inbox"},
                {"title": "no id"}
            ]
        });
        assert!(workspace_listed(
            &listing,
            "4AC63CB7-3BE1-40A1-BCC4-CA0461685F01"
        ));
        assert!(workspace_listed(
            &listing,
            "4ac63cb7-3be1-40a1-bcc4-ca0461685f01"
        ));
        assert!(!workspace_listed(&listing, "[dagq]inbox"));
        assert!(!workspace_listed(&serde_json::json!({}), "x"));
    }

    #[test]
    fn listed_workspaces_read_the_ids_and_descriptions() {
        let listing = serde_json::json!({"workspaces": [
            {"id": "A", "description": "dagq role=worker queue=q run=r task=1"},
            {"id": "B", "description": null}
        ]});
        assert_eq!(
            listed_workspaces(&listing).unwrap(),
            [
                ListedWorkspace {
                    id: "A".into(),
                    description: Some("dagq role=worker queue=q run=r task=1".into()),
                },
                ListedWorkspace {
                    id: "B".into(),
                    description: None,
                },
            ]
        );
        assert!(listed_workspaces(&serde_json::json!({})).is_err());
        assert!(listed_workspaces(&serde_json::json!({"workspaces": [{}]})).is_err());
    }

    #[test]
    fn created_group_id_reads_the_group_uuid() {
        let reply = serde_json::json!({
            "created": false,
            "group": {"id": "F5FCC58F-D44B-4CA0-871F-D4C6AED704D6", "external_id": "abc"}
        });
        assert_eq!(
            created_group_id(&reply).unwrap(),
            "F5FCC58F-D44B-4CA0-871F-D4C6AED704D6"
        );
        assert!(created_group_id(&serde_json::json!({"created": true})).is_err());
    }

    /// `ensure_group` asks for the group by its external ID and `exists`
    /// reads the UUID listing of every window, both through the real argv.
    #[cfg(unix)]
    #[test]
    fn ensure_group_and_exists_call_cmux() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args.log");
        let executable = dir.path().join("cmux");
        fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
printf '%s ' "$@" >> '{log}'; printf '\n' >> '{log}'
case "$4" in
  workspace-group) echo '{{"created":true,"group":{{"id":"G-1"}}}}' ;;
  list-windows) echo '[{{"id":"A"}},{{"id":"B"}}]' ;;
  workspace)
    case "$7" in
      A) echo '{{"window_id":"A","workspaces":[{{"id":"W-0"}}]}}' ;;
      B) echo '{{"window_id":"B","workspaces":[{{"id":"W-1"}}]}}' ;;
      *) echo 'Error: unavailable: TabManager not available' >&2; exit 1 ;;
    esac ;;
esac
"#,
                log = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let cmux = Cmux { executable };
        assert_eq!(cmux.ensure_group("abc", "[dagq]").unwrap(), "G-1");
        assert!(cmux.exists("W-0").unwrap());
        assert!(cmux.exists("w-1").unwrap(), "a workspace of another window");
        assert!(!cmux.exists("W-2").unwrap());
        assert_eq!(cmux.list_workspaces().unwrap().len(), 2);
        let calls = fs::read_to_string(&log).unwrap();
        assert!(
            calls.contains(
                "--json --id-format uuids workspace-group create --name [dagq] --external-id abc"
            ),
            "{calls}"
        );
        assert!(calls.contains("--json --id-format uuids list-windows"));
        assert!(calls.contains("--json --id-format uuids workspace list --window A"));
        assert!(calls.contains("--json --id-format uuids workspace list --window B"));
    }

    /// The windows' listings are concatenated; a window that cannot be
    /// listed, or a reply of the wrong shape, fails the whole listing.
    #[test]
    fn merged_workspace_listing_joins_every_window() {
        let windows = serde_json::json!([{"id": "A"}, {"id": "B"}]);
        let merged = merged_workspace_listing(&windows, |window| {
            Ok(serde_json::json!({"window_id": window, "workspaces": [{"id": format!("{window}-1")}]}))
        })
        .unwrap();
        assert_eq!(
            merged,
            serde_json::json!({"workspaces": [{"id": "A-1"}, {"id": "B-1"}]})
        );
        assert!(workspace_listed(&merged, "b-1"));
        assert_eq!(
            merged_workspace_listing(&serde_json::json!([]), |_| unreachable!()).unwrap(),
            serde_json::json!({"workspaces": []})
        );
        let error = merged_workspace_listing(&windows, |window| {
            ensure!(window == "A", "TabManager not available");
            Ok(serde_json::json!({"workspaces": []}))
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("window B"), "{error:#}");
        assert!(merged_workspace_listing(&serde_json::json!({}), |_| unreachable!()).is_err());
        assert!(merged_workspace_listing(&serde_json::json!([{}]), |_| unreachable!()).is_err());
        assert!(merged_workspace_listing(&windows, |_| Ok(serde_json::json!({}))).is_err());
    }

    /// `create` passes the run's name and its tags (description, env,
    /// group) to `cmux workspace create`, keeps the raw reply in the run directory and returns the
    /// UUID `identify` resolves.
    #[cfg(unix)]
    #[test]
    fn create_names_the_run_workspace_and_tags_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args.log");
        let executable = dir.path().join("cmux");
        fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >> '{log}'; done
printf -- '--\n' >> '{log}'
case "$1" in
  workspace) echo "OK workspace:7" ;;
  --json) echo '{{"caller":{{"workspace_id":"4AC63CB7-3BE1-40A1-BCC4-CA0461685F01"}}}}' ;;
esac
"#,
                log = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let run_dir = dir.path().join("run");
        fs::create_dir(&run_dir).unwrap();
        let mut record = record(Some("/home/u/ghq/dagq"));
        record.status = RunStatus::Starting;
        record.worktree_path = Some(dir.path().display().to_string());
        record.run_dir = Some(run_dir.display().to_string());
        let run = TaskRun::restore(record).unwrap();
        let cmux = Cmux { executable };
        let tags = WorkspaceTags {
            env: vec![
                ("DAGQ_ROLE".into(), "worker".into()),
                ("DAGQ_QUEUE".into(), "/q/queue.db".into()),
            ],
            description: Some("dagq role=worker queue=abc".into()),
            group: Some("G-1".into()),
        };
        let id = cmux
            .create(
                &task("Set last_error when a run fails"),
                &run,
                "true",
                &tags,
            )
            .unwrap();
        assert_eq!(id, "4AC63CB7-3BE1-40A1-BCC4-CA0461685F01");
        assert_eq!(
            fs::read_to_string(run_dir.join("workspace-create.txt")).unwrap(),
            "OK workspace:7\n"
        );
        let calls = fs::read_to_string(&log).unwrap();
        let create: Vec<&str> = calls.split("--\n").next().unwrap().lines().collect();
        assert_eq!(
            create,
            [
                "workspace",
                "create",
                "--name",
                "[dagq]worker#15 - Set last_error when a run fails",
                "--description",
                "dagq role=worker queue=abc",
                "--env",
                "DAGQ_ROLE=worker",
                "--env",
                "DAGQ_QUEUE=/q/queue.db",
                "--group",
                "G-1",
                "--command",
                "true",
                "--focus",
                "false",
                "--cwd",
                &dir.path().display().to_string(),
            ]
        );
        assert!(calls.contains("identify\n--workspace\nworkspace:7\n"));
    }

    /// Task 344: a workspace cmux created but did not identify is closed
    /// by its handle, and the error says so; when the close fails too, the
    /// error carries both.
    #[cfg(unix)]
    #[test]
    fn a_created_workspace_cmux_does_not_identify_is_closed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args.log");
        let script = |close: &str| {
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
case "$1 $2" in
  "workspace create") echo "OK workspace:9" ;;
  "workspace close") {close} ;;
  "--json --id-format") echo 'identify broke' >&2; exit 1 ;;
esac
"#,
                log = log.display()
            )
        };
        let executable = dir.path().join("cmux");
        fs::write(&executable, script(r#"echo "OK workspace:9""#)).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let cmux = Cmux {
            executable: executable.clone(),
        };
        let tags = WorkspaceTags::default();
        let error = cmux
            .create_named("[dagq]planner#1", dir.path(), "true", &tags)
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains(
                "cmux created workspace workspace:9 but did not identify it; it was closed"
            ),
            "{text}"
        );
        assert!(text.contains("identify broke"), "{text}");
        let calls = fs::read_to_string(&log).unwrap();
        assert!(calls.contains("workspace close workspace:9\n"), "{calls}");

        fs::write(&executable, script("echo 'no such workspace' >&2; exit 1")).unwrap();
        let error = cmux
            .create_named("[dagq]planner#2", dir.path(), "true", &tags)
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("and closing it failed"), "{text}");
        assert!(text.contains("no such workspace"), "{text}");
        assert!(text.contains("identify broke"), "{text}");
    }

    #[test]
    fn review_output_reads_non_utf8_lossily_where_output_refuses_it() {
        let latin1 = || {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", r"printf 'caf\351\n'"]);
            command
        };
        let error = format!("{:#}", output(&mut latin1()).unwrap_err());
        assert!(error.contains("command output is not UTF-8"), "{error}");
        assert_eq!(review_output(&mut latin1()).unwrap(), "caf\u{fffd}\n");
        let error = review_output(Command::new("/bin/sh").args(["-c", "echo no >&2; exit 3"]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("failed") && error.contains("no"), "{error}");
    }

    #[test]
    fn review_output_to_streams_raw_bytes_into_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let file = fs::File::create(&path).unwrap();
        review_output_to(
            Command::new("/bin/sh").args(["-c", r"printf 'caf\351\n'"]),
            &file,
        )
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"caf\xe9\n");
        let error = review_output_to(
            Command::new("/bin/sh").args(["-c", "echo broken >&2; exit 2"]),
            &file,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("broken"), "{error}");
    }

    #[test]
    fn claude_global_config_prefers_the_config_dir_over_home() {
        assert_eq!(
            claude_global_config(Some("/cfg"), Some("/home/u")),
            Some(PathBuf::from("/cfg/.claude.json"))
        );
        assert_eq!(
            claude_global_config(Some(""), Some("/home/u")),
            Some(PathBuf::from("/home/u/.claude.json"))
        );
        assert_eq!(claude_global_config(None, Some("")), None);
        assert_eq!(claude_global_config(None, None), None);
    }

    /// Claude Code's version is the name of the versioned file `claude`
    /// resolves to, through a link; any other path names none. `rustc -vV`
    /// runs in the checkout, and a directory it cannot run in gives none.
    /// Codex's is asked of `codex` only when it is given.
    #[test]
    fn host_versions_come_from_the_claude_path_and_rustc() {
        let dir = tempfile::tempdir().unwrap();
        let versions = dir.path().join("versions");
        fs::create_dir(&versions).unwrap();
        let installed = versions.join("2.1.3");
        fs::write(&installed, "").unwrap();
        let link = dir.path().join("claude");
        std::os::unix::fs::symlink(&installed, &link).unwrap();
        assert_eq!(claude_version(&link).as_deref(), Some("2.1.3"));
        assert_eq!(claude_version(&dir.path().join("elsewhere")), None);
        let other = dir.path().join("claude-stub");
        fs::write(&other, "").unwrap();
        assert_eq!(claude_version(&other), None);
        let host = host_versions(&link, None, Some(dir.path()));
        assert_eq!(host.claude_version.as_deref(), Some("2.1.3"));
        assert_eq!(host.codex_version, None);
        assert!(host.rustc_release.is_some(), "{host:?}");
        // No checkout to run it in (not dagq's source): no toolchain.
        let none = host_versions(&link, None, None);
        assert_eq!(none.claude_version.as_deref(), Some("2.1.3"));
        assert_eq!((none.rustc_release, none.rustc_host), (None, None));
        let missing = host_versions(&other, None, Some(&dir.path().join("missing")));
        assert_eq!(missing, HostVersions::default());
        // Codex's is what `codex --version` prints; none when it fails.
        let codex = dir.path().join("codex");
        fs::write(&codex, "#!/bin/sh\necho 'codex-cli 0.46.0'\n").unwrap();
        fs::set_permissions(&codex, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let host = host_versions(&other, Some(&codex), Some(&dir.path().join("missing")));
        assert_eq!(host.codex_version.as_deref(), Some("0.46.0"));
        let gone = host_versions(&other, Some(&dir.path().join("gone")), Some(dir.path()));
        assert_eq!(gone.codex_version, None);
    }
}
