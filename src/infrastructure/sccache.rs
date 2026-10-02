//! The host's sccache server (ADR-t1215-1): whether it listens is looked at
//! with a connect to its loopback port, never with an sccache client (which
//! starts the server it does not find, with the caller's environment and
//! no event); it is started with `sccache --start-server` from the
//! supervisor, outside any sandbox.

use crate::application::{SccacheServer, ServerPid};
use anyhow::{Context, Result, bail};
use std::{
    fs,
    net::{Ipv4Addr, SocketAddr, TcpStream},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// How long a connect to the port may take before nothing is taken to
/// listen there.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// How long a start may take, the client's wait and the server's listen.
pub const START_TIMEOUT: Duration = Duration::from_secs(30);

/// [`SccacheServer`] on this host. The start's output goes to `log`.
#[derive(Debug, Clone)]
pub struct SystemSccache {
    pub log: PathBuf,
    pub start_timeout: Duration,
}

impl SystemSccache {
    /// The start's output in `<queue dir>/sccache-start.log`.
    pub fn new(queue_dir: &Path, start_timeout: Duration) -> Self {
        Self {
            log: queue_dir.join("sccache-start.log"),
            start_timeout,
        }
    }
}

/// Whether something listens on the loopback `port`.
pub fn listening(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        CONNECT_TIMEOUT,
    )
    .is_ok()
}

/// The pid of the process listening on TCP `port`, as `lsof` lists it, or
/// why it could not be read.
fn listener_pid(port: u16) -> ServerPid {
    let output = Command::new("lsof")
        .args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("lsof could not be run: {error}"))?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().parse().ok())
        .ok_or_else(|| {
            format!(
                "lsof listed no process listening on port {port} ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )
        })
}

/// [`listener_pid`], tried again until `deadline` while lsof lists none.
fn listener_pid_until(port: u16, deadline: Instant) -> ServerPid {
    loop {
        let pid = listener_pid(port);
        if pid.is_ok() || Instant::now() >= deadline {
            return pid;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

impl SccacheServer for SystemSccache {
    fn listening(&self, port: u16) -> Result<bool> {
        Ok(listening(port))
    }

    fn start(&self, program: &Path, env: &[(String, String)], port: u16) -> Result<ServerPid> {
        let log = fs::File::create(&self.log)
            .with_context(|| format!("create {}", self.log.display()))?;
        let mut command = Command::new(program);
        command
            .arg("--start-server")
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            // A group of its own: a signal to the supervisor's group does
            // not reach the server it leaves running.
            .process_group(0);
        let mut child = command
            .spawn()
            .with_context(|| format!("start {} --start-server", program.display()))?;
        let deadline = Instant::now() + self.start_timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "{} --start-server did not finish within {:?}",
                    program.display(),
                    self.start_timeout
                );
            }
            thread::sleep(Duration::from_millis(20));
        };
        if !status.success() {
            let output = fs::read_to_string(&self.log).unwrap_or_default();
            bail!(
                "{} --start-server failed ({status}): {}",
                program.display(),
                output.trim()
            );
        }
        while !listening(port) {
            if Instant::now() >= deadline {
                bail!(
                    "{} --start-server finished, but nothing listens on port {port}",
                    program.display()
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
        // The server listens: a pid that cannot be read is reported, not a
        // failed start, for a few more seconds at most.
        Ok(listener_pid_until(
            port,
            deadline.max(Instant::now() + Duration::from_secs(2)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, os::unix::fs::PermissionsExt};

    /// A stub sccache that writes its arguments and the environment it
    /// was given to `calls.log`, and exits with `status`.
    fn stub(dir: &Path, status: i32) -> PathBuf {
        let program = dir.join("sccache");
        fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf '%s idle=%s path=%s\\n' \"$*\" \"${{SCCACHE_IDLE_TIMEOUT-unset}}\" \"$PATH\" >> \"${{0%/*}}/calls.log\"\necho stub said no >&2\nexit {status}\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    fn calls(dir: &Path) -> Vec<String> {
        fs::read_to_string(dir.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// A loopback port nothing listens on now.
    fn free_port() -> u16 {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn a_look_at_a_port_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        stub(dir.path(), 0);
        let sccache = SystemSccache::new(dir.path(), Duration::from_secs(5));
        let port = free_port();
        assert!(!sccache.listening(port).unwrap());
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert!(
            sccache
                .listening(listener.local_addr().unwrap().port())
                .unwrap()
        );
        // No sccache ran: no server with another environment, no event.
        assert!(calls(dir.path()).is_empty());
    }

    #[test]
    fn a_start_runs_the_program_with_the_environment_and_waits_for_the_port() {
        let dir = tempfile::tempdir().unwrap();
        let program = stub(dir.path(), 0);
        let sccache = SystemSccache::new(dir.path(), Duration::from_secs(30));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let env = vec![
            ("SCCACHE_IDLE_TIMEOUT".to_owned(), "0".to_owned()),
            ("PATH".to_owned(), "/run/env/bin:/usr/bin:/bin".to_owned()),
        ];
        let pid = sccache.start(&program, &env, port).unwrap();
        assert_eq!(
            calls(dir.path()),
            ["--start-server idle=0 path=/run/env/bin:/usr/bin:/bin"]
        );
        // lsof names this process (the listener's); a host without lsof
        // says why there is no pid.
        match pid {
            Ok(pid) => assert_eq!(pid, std::process::id()),
            Err(why) => assert!(why.contains("lsof could not be run"), "{why}"),
        }
    }

    #[test]
    fn a_port_nothing_listens_on_has_no_pid_and_says_why() {
        let port = free_port();
        let why = listener_pid_until(port, Instant::now()).unwrap_err();
        assert!(
            why.contains(&format!("no process listening on port {port}"))
                || why.contains("lsof could not be run"),
            "{why}"
        );
    }

    #[test]
    fn a_start_fails_when_the_program_fails_or_nothing_listens() {
        let dir = tempfile::tempdir().unwrap();
        let program = stub(dir.path(), 3);
        // Long enough for the stub to exit under load: it fails at once.
        let sccache = SystemSccache::new(dir.path(), Duration::from_secs(30));
        let error = sccache.start(&program, &[], free_port()).unwrap_err();
        assert!(format!("{error:#}").contains("stub said no"), "{error:#}");

        let program = stub(dir.path(), 0);
        // Waits its whole limit for a port nothing listens on.
        let sccache = SystemSccache::new(dir.path(), Duration::from_secs(2));
        let error = sccache.start(&program, &[], free_port()).unwrap_err();
        assert!(
            format!("{error:#}").contains("nothing listens"),
            "{error:#}"
        );
        let error = sccache
            .start(&dir.path().join("missing"), &[], free_port())
            .unwrap_err();
        assert!(format!("{error:#}").contains("--start-server"), "{error:#}");
    }

    #[test]
    fn a_start_that_does_not_finish_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("sccache");
        fs::write(&program, "#!/bin/sh\nexec sleep 30\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let sccache = SystemSccache::new(dir.path(), Duration::from_millis(200));
        let error = sccache.start(&program, &[], free_port()).unwrap_err();
        assert!(format!("{error:#}").contains("did not finish"), "{error:#}");
    }
}
