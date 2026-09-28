//! The process backend ([Broker] "process.exec"): `process.exec`, one
//! program of the allowlist run by its argv (never through a shell) in the
//! token's workspace, with the limits enforced here on the server's side
//! (ADR-t827-2 decision 8).
//!
//! - `argv[0]` is a program name (no `/`) in `Limits::exec_allow`, looked up
//!   on the fixed [`PATH`] by the broker, not by the request's env; `git` is
//!   refused whatever the allowlist says (only the git backend runs git).
//! - The working directory is the workspace, opened as the fs backend opens
//!   it (from the mounted root, no symlink on the way) and entered with
//!   `fchdir` in the child, so what was checked is where it runs.
//! - The env starts empty: the fixed [`PATH`], a `HOME` of its own for each
//!   exec (removed afterwards), `LANG=C.UTF-8` and `TERM=dumb`, and of the
//!   request's env only the names in `Limits::exec_env` (never `PATH` or
//!   `HOME`). Nothing of the broker's own env reaches the program.
//! - The program leads a process group of its own. The timeout is the
//!   request's `timeout_secs` (the default when absent) capped by the
//!   server's maximum; past it, or past `Limits::output_limit_bytes` of
//!   stdout and stderr together, the whole group gets `SIGKILL` and the
//!   answer is `timeout` or `output_limit`. When the program ends by itself,
//!   what it left behind in its group is killed too, so nothing outlives the
//!   request but a process that left the group (`setsid`).
//! - `stdin` is bounded by the same output limit.
//!
//! [Broker]: https://github.com/hisamekms/dagq/blob/main/docs/design/broker.md

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use dagq_broker_protocol::{ErrorCode, encode, process};

use crate::backend::{Backend, BackendRequest, Call, Done, Failure};
use crate::backends::fs::open_workspace;

/// The `PATH` every program gets, and where `argv[0]` is looked up.
pub const PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// The env every program gets besides `PATH` and `HOME`; the request may
/// replace these when `exec_env` names them.
const BASE_ENV: [(&str, &str); 2] = [("LANG", "C.UTF-8"), ("TERM", "dumb")];

/// The env names the request can never set.
const FIXED_ENV: [&str; 2] = ["PATH", "HOME"];

/// How long one wait for output lasts before the clock and the program are
/// looked at again.
const POLL: Duration = Duration::from_millis(20);

/// How long the output of a program that has ended is still read, for what
/// its killed group had written; a process that left the group can hold the
/// pipes open no longer than this.
const DRAIN: Duration = Duration::from_millis(500);

/// The process backend over the server's mounted roots.
#[derive(Debug, Clone)]
pub struct ProcessBackend {
    roots: Vec<PathBuf>,
}

impl ProcessBackend {
    /// A backend that runs programs in workspaces under `roots` (`--root`).
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }
}

impl Backend for ProcessBackend {
    fn call(&self, call: &Call<'_>, request: BackendRequest) -> Result<Done, Failure> {
        let BackendRequest::ProcessExec(request) = request else {
            return Err(invalid(format!(
                "{} is not a process operation",
                call.operation
            )));
        };
        let limits = call.limits;
        let program = allowed_program(&request.argv, &limits.exec_allow)?;
        let timeout = timeout(request.timeout_secs, limits)?;
        let stdin = request.stdin.unwrap_or_default().into_bytes();
        if stdin.len() as u64 > limits.output_limit_bytes {
            return Err(Failure::new(
                ErrorCode::OutputLimit,
                format!(
                    "stdin: more than {} bytes to be given",
                    limits.output_limit_bytes
                ),
            ));
        }
        let env = request_env(&request.env, &limits.exec_env)?;
        let resolved = lookup(program)?;
        let workspace = open_workspace(&self.roots, Path::new(&call.claims.workspace))?;
        let home = Home::new()?;

        let started = Instant::now();
        let child = spawn(&resolved, &request.argv, &env, &home, &workspace)?;
        let ran = supervise(child, stdin, started + timeout, limits.output_limit_bytes)
            .map_err(|error| backend(format!("run {program}: {error}")))?;
        let duration_ms = started.elapsed().as_millis() as u64;
        let (status, stdout, stderr) = match ran {
            Ran::Exited {
                status,
                stdout,
                stderr,
            } => (status, stdout, stderr),
            Ran::TimedOut => {
                return Err(Failure::new(
                    ErrorCode::Timeout,
                    format!(
                        "{program} ran past {} seconds and was stopped",
                        timeout.as_secs()
                    ),
                ));
            }
            Ran::OverLimit => {
                return Err(Failure::new(
                    ErrorCode::OutputLimit,
                    format!(
                        "{program} wrote more than {} bytes and was stopped",
                        limits.output_limit_bytes
                    ),
                ));
            }
        };
        let exit_code = status.code();
        let body = encode(&process::ExecResponse {
            exit_code,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            duration_ms,
        })
        .map_err(|error| backend(error.to_string()))?;
        Ok(Done { body, exit_code })
    }
}

/// `argv[0]` when it may run: a name (no `/`) in `allow`, never `git`.
fn allowed_program<'a>(argv: &'a [String], allow: &[String]) -> Result<&'a str, Failure> {
    let Some(program) = argv.first() else {
        return Err(invalid("argv is empty"));
    };
    if program.is_empty() || argv.iter().any(|arg| arg.contains('\0')) {
        return Err(invalid("argv holds an empty program or a NUL byte"));
    }
    let name = program.rsplit('/').next().unwrap_or(program);
    if name == "git" {
        return Err(denied("git runs through the git operations only"));
    }
    if program.contains('/') {
        return Err(denied(
            "argv[0] must be a program name of the allowlist, not a path",
        ));
    }
    if !allow.iter().any(|allowed| allowed == program) {
        return Err(denied(format!("{program} is not in the exec allowlist")));
    }
    Ok(program)
}

/// The request's timeout (the default when absent) capped by the maximum.
fn timeout(requested: Option<u64>, limits: &crate::config::Limits) -> Result<Duration, Failure> {
    let secs = match requested {
        Some(0) => return Err(invalid("timeout_secs must be at least 1")),
        Some(secs) => secs,
        None => limits.exec_timeout_secs,
    };
    Ok(Duration::from_secs(secs.min(limits.exec_max_timeout_secs)))
}

/// The env besides `HOME`: [`PATH`], [`BASE_ENV`], and the request's names
/// that `allow` lets through (never [`FIXED_ENV`]). Other names are dropped
/// without a word, their values unread.
fn request_env(
    requested: &BTreeMap<String, String>,
    allow: &[String],
) -> Result<BTreeMap<String, String>, Failure> {
    let mut env: BTreeMap<String, String> = BASE_ENV
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    env.insert("PATH".to_owned(), PATH.to_owned());
    for (name, value) in requested {
        if FIXED_ENV.contains(&name.as_str()) || !allow.iter().any(|allowed| allowed == name) {
            continue;
        }
        if value.contains('\0') {
            return Err(invalid(format!("the value of env {name} holds a NUL byte")));
        }
        env.insert(name.clone(), value.clone());
    }
    Ok(env)
}

/// The first executable file named `program` on [`PATH`].
fn lookup(program: &str) -> Result<PathBuf, Failure> {
    PATH.split(':')
        .map(|dir| Path::new(dir).join(program))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
        .ok_or_else(|| backend(format!("{program} is not found on {PATH}")))
}

/// A `HOME` of one exec's own, removed when dropped.
struct Home(PathBuf);

impl Home {
    fn new() -> Result<Self, Failure> {
        let path = std::env::temp_dir().join(format!("dagq-broker-home-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|error| backend(format!("create the program's HOME: {error}")))?;
        Ok(Self(path))
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spawn(
    resolved: &Path,
    argv: &[String],
    env: &BTreeMap<String, String>,
    home: &Home,
    workspace: &OwnedFd,
) -> Result<Child, Failure> {
    let dir: RawFd = workspace.as_raw_fd();
    let mut command = Command::new(resolved);
    command
        .arg0(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .envs(env)
        .env("HOME", &home.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // SAFETY: between fork and exec only `fchdir`, which is
    // async-signal-safe, on a descriptor the parent keeps open until spawn
    // returns.
    unsafe {
        command.pre_exec(move || {
            if libc::fchdir(dir) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .spawn()
        .map_err(|error| backend(format!("start {}: {error}", argv[0])))
}

/// How a program's run ended.
#[derive(Debug)]
enum Ran {
    Exited {
        status: ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    TimedOut,
    OverLimit,
}

/// One of the program's output pipes, read without blocking.
struct Output<R> {
    pipe: Option<R>,
    bytes: Vec<u8>,
}

impl<R: Read + AsRawFd> Output<R> {
    fn new(pipe: Option<R>) -> io::Result<Self> {
        if let Some(pipe) = &pipe {
            nonblocking(pipe.as_raw_fd())?;
        }
        Ok(Self {
            pipe,
            bytes: Vec::new(),
        })
    }

    /// Read one buffer of what is there; the pipe is closed at its end.
    /// One buffer at a time, so a program that writes faster than it is
    /// read cannot keep the watch from its limit and its clock.
    fn read(&mut self) -> io::Result<()> {
        let Some(pipe) = &mut self.pipe else {
            return Ok(());
        };
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => {
                    self.pipe = None;
                    return Ok(());
                }
                Ok(read) => {
                    self.bytes.extend_from_slice(&buffer[..read]);
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn poll_fd(&self) -> Option<libc::pollfd> {
        self.pipe.as_ref().map(|pipe| libc::pollfd {
            fd: pipe.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
    }
}

/// The program's stdin and what is left to give it.
struct Input {
    pipe: Option<ChildStdin>,
    bytes: Vec<u8>,
    given: usize,
}

impl Input {
    fn new(pipe: Option<ChildStdin>, bytes: Vec<u8>) -> io::Result<Self> {
        let mut input = Self {
            pipe,
            bytes,
            given: 0,
        };
        match &input.pipe {
            Some(pipe) if !input.bytes.is_empty() => nonblocking(pipe.as_raw_fd())?,
            // Nothing to give: close it now, so the program sees its end.
            _ => input.pipe = None,
        }
        Ok(input)
    }

    /// Give what the pipe takes; closed once all is given or the program
    /// stopped reading.
    fn write(&mut self) -> io::Result<()> {
        let Some(pipe) = &mut self.pipe else {
            return Ok(());
        };
        while self.given < self.bytes.len() {
            match pipe.write(&self.bytes[self.given..]) {
                Ok(written) => self.given += written,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
                Err(error) => return Err(error),
            }
        }
        self.pipe = None;
        Ok(())
    }

    fn poll_fd(&self) -> Option<libc::pollfd> {
        self.pipe.as_ref().map(|pipe| libc::pollfd {
            fd: pipe.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        })
    }
}

/// Feed `stdin`, read the output and watch the clock until the program
/// ends, the deadline passes or the output passes `limit`. The program's
/// process group is killed in every case before it is reaped.
fn supervise(mut child: Child, stdin: Vec<u8>, deadline: Instant, limit: u64) -> io::Result<Ran> {
    let group = child.id() as libc::pid_t;
    let outcome = watch(&mut child, stdin, deadline, limit);
    kill_group(group);
    let status = child.wait();
    let outcome = outcome?;
    let status = status?;
    Ok(match outcome {
        Watched::Exited { stdout, stderr } => Ran::Exited {
            status,
            stdout,
            stderr,
        },
        Watched::TimedOut => Ran::TimedOut,
        Watched::OverLimit => Ran::OverLimit,
    })
}

enum Watched {
    Exited { stdout: Vec<u8>, stderr: Vec<u8> },
    TimedOut,
    OverLimit,
}

fn watch(child: &mut Child, stdin: Vec<u8>, deadline: Instant, limit: u64) -> io::Result<Watched> {
    let pid = child.id() as libc::pid_t;
    let mut input = Input::new(child.stdin.take(), stdin)?;
    let mut stdout: Output<ChildStdout> = Output::new(child.stdout.take())?;
    let mut stderr: Output<ChildStderr> = Output::new(child.stderr.take())?;
    let mut ended: Option<Instant> = None;
    loop {
        let now = Instant::now();
        match ended {
            // A program that has ended by the deadline is not timed out;
            // the next turn reads what it left.
            None if now >= deadline && !has_ended(pid)? => return Ok(Watched::TimedOut),
            None if now >= deadline => {
                kill_group(pid);
                input.pipe = None;
                ended = Some(now);
            }
            Some(at) if stdout.pipe.is_none() && stderr.pipe.is_none() || now >= at + DRAIN => {
                return Ok(Watched::Exited {
                    stdout: stdout.bytes,
                    stderr: stderr.bytes,
                });
            }
            _ => {}
        }
        let wait = match ended {
            None => deadline.saturating_duration_since(now),
            Some(at) => (at + DRAIN).saturating_duration_since(now),
        }
        .min(POLL);
        let mut fds: Vec<libc::pollfd> = [stdout.poll_fd(), stderr.poll_fd(), input.poll_fd()]
            .into_iter()
            .flatten()
            .collect();
        // SAFETY: `fds` is a live array of `fds.len()` pollfd; a signal
        // (EINTR) only ends the wait early.
        unsafe {
            libc::poll(
                fds.as_mut_ptr(),
                fds.len() as libc::nfds_t,
                wait.as_millis().max(1) as libc::c_int,
            );
        }
        input.write()?;
        stdout.read()?;
        stderr.read()?;
        if (stdout.bytes.len() + stderr.bytes.len()) as u64 > limit {
            return Ok(Watched::OverLimit);
        }
        if ended.is_none() && has_ended(pid)? {
            // What the program left in its group goes with it; the pipes
            // then close once the rest is read.
            kill_group(pid);
            input.pipe = None;
            ended = Some(Instant::now());
        }
    }
}

/// Whether the child `pid` has ended, leaving it unreaped so its process
/// group id stays its own until the group is killed.
fn has_ended(pid: libc::pid_t) -> io::Result<bool> {
    // SAFETY: a zeroed siginfo_t is a valid value to be filled in.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a live siginfo_t; WNOWAIT leaves the child to reap.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    // SAFETY: waitid filled `info` (or left it zeroed: nothing ended yet).
    Ok(unsafe { info.si_pid() } != 0)
}

fn kill_group(group: libc::pid_t) {
    // SAFETY: a signal to the group the program leads; the leader is not
    // reaped yet, so the id is still that group's. ESRCH (none left) is fine.
    unsafe {
        libc::killpg(group, libc::SIGKILL);
    }
}

fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor the caller owns.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn denied(message: impl Into<String>) -> Failure {
    Failure::new(ErrorCode::CapabilityDenied, message)
}

fn backend(message: impl Into<String>) -> Failure {
    Failure::new(ErrorCode::BackendError, message)
}

fn invalid(message: impl Into<String>) -> Failure {
    Failure::new(ErrorCode::InvalidRequest, message)
}

#[cfg(test)]
mod tests;
