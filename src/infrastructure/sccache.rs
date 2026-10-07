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
/// Process identity and stats queries are bounded to five seconds. The process
/// uses C-locale `ps` start text; only a `sandbox-exec` parent proves confinement.
/// `lsof` and `ps` paths can be injected without changing the host's PATH.
#[derive(Debug, Clone)]
pub struct SystemSccache {
    pub log: PathBuf,
    pub start_timeout: Duration,
    pub lsof: PathBuf,
    pub ps: PathBuf,
}

impl SystemSccache {
    /// The start's output in `<queue dir>/sccache-start.log`.
    pub fn new(queue_dir: &Path, start_timeout: Duration) -> Self {
        Self {
            log: queue_dir.join("sccache-start.log"),
            start_timeout,
            lsof: "lsof".into(),
            ps: "ps".into(),
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
fn listener_pid(port: u16, lsof: &Path) -> ServerPid {
    let output = crate::infrastructure::adapters::unpiped_output_within(
        Command::new(lsof).args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"]),
        Duration::from_secs(5),
    )
    .map_err(|error| format!("lsof could not be run: {error}"))?
    .ok_or_else(|| "lsof timed out reading the sccache listener".to_owned())?;
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
fn listener_pid_until(port: u16, deadline: Instant, lsof: &Path) -> ServerPid {
    loop {
        let pid = listener_pid(port, lsof);
        if pid.is_ok() || Instant::now() >= deadline {
            return pid;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Bounded host queries: neither process listing nor stats may stall a pass.
fn query(command: &mut Command) -> Result<std::process::Output> {
    let output =
        crate::infrastructure::adapters::unpiped_output_within(command, Duration::from_secs(5))?
            .ok_or_else(|| anyhow::anyhow!("sccache query timed out"))?;
    if !output.status.success() {
        bail!(
            "sccache query failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output)
}

fn process_with(
    port: u16,
    lsof: &Path,
    ps: &Path,
) -> Result<Option<crate::domain::sccache::ServerProcess>> {
    use crate::domain::sccache::ServerProcess;
    let output = crate::infrastructure::adapters::unpiped_output_within(
        Command::new(lsof).args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"]),
        Duration::from_secs(5),
    )?
    .ok_or_else(|| anyhow::anyhow!("lsof timed out"))?;
    let Some(pid) = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().parse::<u32>().ok())
    else {
        if !listening(port) {
            return Ok(None);
        }
        bail!("lsof could not identify the listener on port {port}");
    };
    let output = query(Command::new(ps).env("LC_ALL", "C").args([
        "-p",
        &pid.to_string(),
        "-o",
        "ppid=,lstart=,command=",
    ]))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<_> = text.split_whitespace().collect();
    if parts.len() < 7 {
        bail!("ps did not describe pid {pid}");
    }
    let parent_pid: u32 = parts[0].parse()?;
    let command = parts[6..].join(" ");
    // An observed sandbox-exec ancestor is positive evidence. Absence is
    // unknown: a daemon can have been reparented after inheriting a sandbox.
    let parent =
        query(Command::new(ps).args(["-p", &parent_pid.to_string(), "-o", "command="])).ok();
    let sandboxed = parent.as_ref().and_then(|output| {
        let text = String::from_utf8_lossy(&output.stdout);
        text.split_whitespace()
            .next()
            .and_then(|command| Path::new(command).file_name())
            .is_some_and(|name| name == "sandbox-exec")
            .then_some(true)
    });
    Ok(Some(ServerProcess {
        pid,
        parent_pid,
        started_at: parts[1..6].join(" "),
        command,
        sandboxed,
    }))
}

fn parse_stats(bytes: &[u8]) -> Result<crate::domain::sccache::ServerStats> {
    let json: serde_json::Value = serde_json::from_slice(bytes)?;
    let stats = &json["stats"];
    let count = |key| {
        stats[key]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("sccache stats missing {key}"))
    };
    Ok(crate::domain::sccache::ServerStats {
        requests: count("compile_requests")?,
        failures: count("compile_fails")?,
        compilations: count("compilations")?,
    })
}

impl SccacheServer for SystemSccache {
    fn listening(&self, port: u16) -> Result<bool> {
        Ok(listening(port))
    }

    fn process(&self, port: u16) -> Result<Option<crate::domain::sccache::ServerProcess>> {
        process_with(port, &self.lsof, &self.ps)
    }

    fn stats(
        &self,
        program: &Path,
        env: &[(String, String)],
        port: u16,
    ) -> Result<Option<crate::domain::sccache::ServerStats>> {
        // A missing listener never invokes a client. In sccache 0.18 the
        // ShowStats branch also uses connect_to_server, not connect_or_start.
        if !listening(port) {
            return Ok(None);
        }
        let output = query(
            Command::new(program)
                .args(["--show-stats", "--stats-format", "json"])
                .envs(env.iter().map(|(k, v)| (k, v)))
                .env("SCCACHE_SERVER_PORT", port.to_string())
                .env("SCCACHE_IDLE_TIMEOUT", "0"),
        )?;
        parse_stats(&output.stdout).map(Some)
    }

    fn stop(&self, program: &Path, env: &[(String, String)], port: u16) -> Result<()> {
        query(
            Command::new(program)
                .arg("--stop-server")
                .envs(env.iter().map(|(k, v)| (k, v)))
                .env("SCCACHE_SERVER_PORT", port.to_string())
                .env("SCCACHE_IDLE_TIMEOUT", "0"),
        )?;
        let deadline = Instant::now() + self.start_timeout;
        while listening(port) {
            if Instant::now() >= deadline {
                bail!("sccache still listens after --stop-server");
            }
            thread::sleep(Duration::from_millis(20));
        }
        Ok(())
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
            &self.lsof,
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
    fn stats_client_is_skipped_without_a_listener_and_reads_json_with_run_env() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("sccache");
        fs::write(
            &program,
            r#"#!/bin/sh
printf '%s idle=%s path=%s\n' "$*" "$SCCACHE_IDLE_TIMEOUT" "$PATH" >> "${0%/*}/calls.log"
printf '%s\n' '{"stats":{"compile_requests":7,"compile_fails":7,"compilations":0}}'
"#,
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let server = SystemSccache::new(dir.path(), Duration::from_secs(5));
        assert_eq!(server.stats(&program, &[], free_port()).unwrap(), None);
        assert!(calls(dir.path()).is_empty());
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let stats = server
            .stats(
                &program,
                &[("PATH".into(), "/configured/bin".into())],
                listener.local_addr().unwrap().port(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(stats.failures, 7);
        assert_eq!(stats.failure_ratio(), 1.0);
        assert_eq!(
            calls(dir.path()),
            ["--show-stats --stats-format json idle=0 path=/configured/bin"]
        );
        assert!(parse_stats(br#"{"stats":{}}"#).is_err());
    }

    #[test]
    fn stub_process_listing_retains_identity_parent_command_and_sandbox_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let lsof = dir.path().join("lsof");
        let ps = dir.path().join("ps");
        fs::write(&lsof, "#!/bin/sh\nprintf '42\\n'\n").unwrap();
        fs::write(
            &ps,
            r#"#!/bin/sh
case "$*" in
*ppid*) printf '%s\n' '12 Mon Oct 5 10:11:12 2026 /bin/sccache --internal-start-server' ;;
*) printf '%s\n' '/usr/bin/sandbox-exec -p profile /bin/sccache' ;;
esac
"#,
        )
        .unwrap();
        for path in [&lsof, &ps] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let process = process_with(4226, &lsof, &ps).unwrap().unwrap();
        assert_eq!(process.pid, 42);
        assert_eq!(process.parent_pid, 12);
        assert_eq!(process.started_at, "Mon Oct 5 10:11:12 2026");
        assert_eq!(process.command, "/bin/sccache --internal-start-server");
        assert_eq!(process.sandboxed, Some(true));
        fs::write(&lsof, "#!/bin/sh\nexit 1\n").unwrap();
        assert!(process_with(free_port(), &lsof, &ps).unwrap().is_none());
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
        let mut sccache = SystemSccache::new(dir.path(), Duration::from_secs(30));
        let lsof = dir.path().join("lsof");
        fs::write(&lsof, "#!/bin/sh\necho 42\n").unwrap();
        fs::set_permissions(&lsof, fs::Permissions::from_mode(0o755)).unwrap();
        sccache.lsof = lsof;
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
        assert_eq!(pid.unwrap(), 42);
    }

    #[test]
    fn a_port_nothing_listens_on_has_no_pid_and_says_why() {
        let port = free_port();
        let dir = tempfile::tempdir().unwrap();
        let lsof = dir.path().join("lsof");
        fs::write(&lsof, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&lsof, fs::Permissions::from_mode(0o755)).unwrap();
        let why = listener_pid_until(port, Instant::now(), &lsof).unwrap_err();
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
