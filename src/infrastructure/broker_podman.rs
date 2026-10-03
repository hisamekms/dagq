//! The adapters of the broker's lifecycle ([`crate::application::broker`],
//! ADR-t827-3): [`PodmanCli`] runs the podman executable, [`FileLock`] is
//! the host-wide lock of dagq's machine (`flock` on
//! `$XDG_CONFIG_HOME/dagq/podman-machine.lock`), [`HttpHealth`] asks
//! `GET /v1/health` on `127.0.0.1` with a hand-written HTTP/1.1 request
//! (dagq has no HTTP dependency, ADR-t827-1 decision 2), and
//! [`BrokerState`] is `<queue dir>/broker/state.json`, and
//! [`SystemProcesses`] lists and signals the host's processes (the
//! machine's orphaned gvproxy, task 1579). The image's build
//! context comes from the material this binary embeds
//! ([`super::broker_image`]).

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use dagq_broker_protocol::HealthResponse;
use serde::{Deserialize, Serialize};

use crate::application::broker::{
    BrokerFailure, BrokerResult, FailureCode, HealthProbe, HostLock, HostProcess, HostProcesses,
    Podman, PodmanOutput, Signal, broker_dir,
};
use crate::infrastructure::adapters::unpiped_output;

/// The machine's lock file under dagq's data dir.
pub const MACHINE_LOCK_FILE: &str = "podman-machine.lock";
/// The queue's broker state file under `<queue dir>/broker`.
pub const STATE_FILE: &str = "state.json";

/// The podman executable.
#[derive(Debug, Clone)]
pub struct PodmanCli {
    pub executable: PathBuf,
}

impl PodmanCli {
    /// `podman` on `PATH` (or `configured`), or `podman_missing`: a person
    /// installs it (`brew install podman`); dagq does not.
    pub fn resolve(configured: Option<&Path>) -> BrokerResult<Self> {
        let wanted = configured.unwrap_or(Path::new("podman"));
        crate::infrastructure::adapters::executable(wanted)
            .map(|executable| Self { executable })
            .map_err(|error| {
                BrokerFailure::new(
                    FailureCode::PodmanMissing,
                    format!(
                        "{error:#}; a person installs podman (brew install podman), dagq does not"
                    ),
                )
            })
    }
}

impl Podman for PodmanCli {
    fn run(&self, args: &[String]) -> BrokerResult<PodmanOutput> {
        let output =
            unpiped_output(Command::new(&self.executable).args(args)).map_err(|error| {
                let code = if error.kind() == ErrorKind::NotFound {
                    FailureCode::PodmanMissing
                } else {
                    FailureCode::PodmanFailed
                };
                BrokerFailure::new(code, format!("run {}: {error}", self.executable.display()))
            })?;
        Ok(PodmanOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// An exclusive `flock` on a file, held until the guard drops.
#[derive(Debug, Clone)]
pub struct FileLock {
    pub path: PathBuf,
}

/// Where the host-wide lock of dagq's machine lives: the config home podman
/// finds its machines under (`$XDG_CONFIG_HOME`, else `~/.config`), never
/// `$XDG_DATA_HOME`, which a throwaway queue (an e2e's fixture) points
/// elsewhere while podman's machine stays the one of the host. A lock under
/// the data home let two e2e run the machine at once, one stopping it under
/// the other's build (task 1162).
pub fn machine_lock_home() -> Result<PathBuf> {
    machine_lock_home_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

fn machine_lock_home_from(
    config: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf> {
    let absolute = |value: Option<std::ffi::OsString>| {
        value.map(PathBuf::from).filter(|path| path.is_absolute())
    };
    if let Some(config) = absolute(config) {
        return Ok(config);
    }
    let home = absolute(home).context("XDG_CONFIG_HOME and HOME are unset or not absolute")?;
    Ok(home.join(".config"))
}

impl FileLock {
    /// The host-wide lock of dagq's machine,
    /// `<lock home>/dagq/podman-machine.lock` ([`machine_lock_home`]).
    pub fn machine(lock_home: &Path) -> Self {
        Self {
            path: lock_home.join("dagq").join(MACHINE_LOCK_FILE),
        }
    }

    /// The queue's lock, `<queue dir>/broker/lock`.
    pub fn queue(queue_dir: &Path) -> Self {
        Self {
            path: broker_dir(queue_dir).join("lock"),
        }
    }

    fn open(&self) -> std::io::Result<File> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.path)?;
        // SAFETY: flock on a descriptor this function owns; it blocks until
        // the lock is free and is released when the file closes.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(file)
    }
}

impl HostLock for FileLock {
    fn hold(&self) -> BrokerResult<Box<dyn std::any::Any>> {
        self.open()
            .map(|file| Box::new(file) as Box<dyn std::any::Any>)
            .map_err(|error| {
                BrokerFailure::new(
                    FailureCode::PodmanFailed,
                    format!("lock {}: {error}", self.path.display()),
                )
            })
    }
}

/// The host's processes through `ps` and `kill(2)`.
#[derive(Debug, Clone, Default)]
pub struct SystemProcesses;

/// `ps`, which lists every process with its full arguments on macOS and
/// Linux alike.
const PS: &str = "/bin/ps";

impl HostProcesses for SystemProcesses {
    fn list(&self) -> Result<Vec<HostProcess>, String> {
        let output =
            unpiped_output(Command::new(PS).args(["-A", "-ww", "-o", "pid=", "-o", "args="]))
                .map_err(|error| format!("run {PS}: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "{PS} exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(parse_processes(&String::from_utf8_lossy(&output.stdout)))
    }

    fn signal(&self, pid: u32, signal: Signal) -> Result<(), String> {
        let pid = signalled_pid(pid)?;
        let signal = match signal {
            Signal::Terminate => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        };
        // SAFETY: kill(2) on one positive pid that is not dagq's own.
        if unsafe { libc::kill(pid, signal) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error.to_string())
        }
    }

    fn alive(&self, pid: u32) -> bool {
        let Ok(pid) = signalled_pid(pid) else {
            return false;
        };
        // SAFETY: kill(2) with signal 0 only asks whether the pid exists.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// `pid` as kill(2) takes it, never 0, 1, a negative (a process group) or
/// dagq itself.
fn signalled_pid(pid: u32) -> Result<libc::pid_t, String> {
    match libc::pid_t::try_from(pid) {
        Ok(pid) if pid > 1 && pid != std::process::id() as libc::pid_t => Ok(pid),
        _ => Err(format!("pid {pid} is not one dagq signals")),
    }
}

/// `ps -o pid= -o args=`'s lines: the pid, then the arguments split on
/// white space (a path with a space in it splits too; the gvproxy's socket
/// is under podman's runtime dir, whose path has none).
fn parse_processes(stdout: &str) -> Vec<HostProcess> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse().ok()?;
            let args: Vec<String> = words.map(str::to_owned).collect();
            (pid > 0 && !args.is_empty()).then_some(HostProcess { pid, args })
        })
        .collect()
}

/// `GET /v1/health` on `127.0.0.1:<port>`.
#[derive(Debug, Clone)]
pub struct HttpHealth {
    pub timeout: Duration,
}

impl Default for HttpHealth {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(3),
        }
    }
}

impl HealthProbe for HttpHealth {
    fn probe(&self, port: u16) -> Result<HealthResponse, String> {
        get_health(SocketAddr::from((Ipv4Addr::LOCALHOST, port)), self.timeout)
    }
}

/// `GET /v1/health` on `address`, read as a [`HealthResponse`].
pub fn get_health(address: SocketAddr, timeout: Duration) -> Result<HealthResponse, String> {
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|error| format!("connect to {address}: {error}"))?;
    let io = |error: std::io::Error| format!("health on {address}: {error}");
    stream.set_read_timeout(Some(timeout)).map_err(io)?;
    stream.set_write_timeout(Some(timeout)).map_err(io)?;
    write!(
        stream,
        "GET /v1/health HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .map_err(io)?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(io)?;
    let response = String::from_utf8_lossy(&response);
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("health on {address}: not an HTTP answer"))?;
    let status = head.lines().next().unwrap_or_default();
    if status.split_whitespace().nth(1) != Some("200") {
        return Err(format!("health on {address} answered `{status}`"));
    }
    serde_json::from_str(body.trim())
        .map_err(|error| format!("health on {address} answered what dagq cannot read: {error}"))
}

/// A port on `127.0.0.1` nothing listens on now.
pub fn free_port() -> Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("pick a free port")?;
    Ok(listener.local_addr()?.port())
}

/// `<queue dir>/broker/state.json`: what `dagq broker` last did, and the
/// port the queue's container publishes (kept across restarts).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerState {
    pub port: Option<u16>,
    pub container: Option<String>,
    pub image: Option<String>,
    /// `building` (the supervisor builds the image), `running`,
    /// `stopped`, or a failure's code.
    pub state: Option<String>,
    /// The build identifier of the dagq that made the container.
    #[serde(default)]
    pub build: Option<String>,
    /// When the container last answered its health after a start (unix
    /// seconds).
    #[serde(default)]
    pub started_at: Option<i64>,
}

impl BrokerState {
    pub fn path(queue_dir: &Path) -> PathBuf {
        broker_dir(queue_dir).join(STATE_FILE)
    }

    /// The state, or the default when there is none (or it cannot be read).
    pub fn read(queue_dir: &Path) -> Self {
        fs::read(Self::path(queue_dir))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Write it through a temporary file and a rename.
    pub fn write(&self, queue_dir: &Path) -> Result<()> {
        let path = Self::path(queue_dir);
        let dir = broker_dir(queue_dir);
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let temporary = dir.join(format!(".{STATE_FILE}.{}", uuid::Uuid::new_v4()));
        fs::write(&temporary, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("write {}", temporary.display()))?;
        fs::rename(&temporary, &path).with_context(|| format!("write {}", path.display()))
    }
}

/// The build identifier `client --version` reports (its last word), read
/// again only when the file's modification time changes (a process asks
/// for it on every claim).
pub fn client_version(client: &Path) -> Result<String, String> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::SystemTime;
    static SEEN: OnceLock<Mutex<HashMap<PathBuf, (SystemTime, String)>>> = OnceLock::new();
    let modified = fs::metadata(client)
        .and_then(|metadata| metadata.modified())
        .map_err(|error| format!("{}: {error}", client.display()))?;
    let seen = SEEN.get_or_init(Mutex::default);
    if let Some((at, version)) = seen.lock().ok().and_then(|seen| seen.get(client).cloned())
        && at == modified
    {
        return Ok(version);
    }
    // Its stderr is taken and dropped.
    let output = unpiped_output(Command::new(client).arg("--version"))
        .map_err(|error| format!("run {}: {error}", client.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} --version exited with {}",
            client.display(),
            output.status
        ));
    }
    let version = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .last()
        .map(str::to_owned)
        .ok_or_else(|| format!("{} --version printed no version", client.display()))?;
    if let Ok(mut seen) = seen.lock() {
        seen.insert(client.to_path_buf(), (modified, version.clone()));
    }
    Ok(version)
}

/// Whether any run holds a token now: an entry in `<queue dir>/broker/active`.
pub fn tokens_active(queue_dir: &Path) -> bool {
    fs::read_dir(broker_dir(queue_dir).join("active"))
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    fn a_missing_podman_is_podman_missing() {
        let dir = tempfile::tempdir().unwrap();
        let error = PodmanCli::resolve(Some(&dir.path().join("podman"))).unwrap_err();
        assert_eq!(error.code, FailureCode::PodmanMissing);
        assert!(error.message.contains("brew install podman"), "{error}");
        // One that vanished after it was resolved.
        let gone = PodmanCli {
            executable: dir.path().join("gone"),
        };
        let error = gone.run(&["--version".to_owned()]).unwrap_err();
        assert_eq!(error.code, FailureCode::PodmanMissing);
    }

    #[test]
    fn the_machine_lock_follows_the_config_home_not_the_data_home() {
        let some = |s: &str| Some(std::ffi::OsString::from(s));
        assert_eq!(
            machine_lock_home_from(some("/xdg/config"), some("/home/u")).unwrap(),
            Path::new("/xdg/config")
        );
        for bad in ["", "relative"] {
            assert_eq!(
                machine_lock_home_from(some(bad), some("/home/u")).unwrap(),
                Path::new("/home/u/.config")
            );
        }
        assert!(machine_lock_home_from(None, None).is_err());
        assert_eq!(
            FileLock::machine(Path::new("/home/u/.config")).path,
            Path::new("/home/u/.config/dagq/podman-machine.lock")
        );
    }

    #[test]
    fn a_podman_that_runs_reports_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("podman");
        fs::write(&fake, "#!/bin/sh\necho \"out $*\"\necho err >&2\nexit 3\n").unwrap();
        fs::set_permissions(
            &fake,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
        let podman = PodmanCli::resolve(Some(&fake)).unwrap();
        let output = podman.run(&["a".to_owned(), "b".to_owned()]).unwrap();
        assert!(!output.success);
        assert_eq!(output.stdout, "out a b\n");
        assert_eq!(output.stderr, "err\n");
    }

    #[test]
    fn the_lock_is_exclusive_across_holders() {
        let dir = tempfile::tempdir().unwrap();
        let lock = FileLock::machine(dir.path());
        assert_eq!(
            lock.path,
            dir.path().join("dagq").join("podman-machine.lock")
        );
        assert_eq!(
            FileLock::queue(Path::new("/q")).path,
            PathBuf::from("/q/broker/lock")
        );
        let held = lock.hold().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let other = lock.clone();
        let waiter = thread::spawn(move || {
            let _held = other.hold().unwrap();
            sender.send(()).unwrap();
        });
        assert!(receiver.recv_timeout(Duration::from_millis(200)).is_err());
        drop(held);
        receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        waiter.join().unwrap();
        let blocked = FileLock {
            path: dir.path().join("file").join("lock"),
        };
        fs::write(dir.path().join("file"), "").unwrap();
        assert_eq!(blocked.hold().unwrap_err().code, FailureCode::PodmanFailed);
    }

    fn serve_once(answer: &'static str) -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Read the whole request before answering: closing with unread
            // bytes would reset the connection under the answer.
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.ends_with(b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0, "the request ended early");
                request.extend_from_slice(&buffer[..read]);
            }
            assert!(String::from_utf8_lossy(&request).starts_with("GET /v1/health "));
            stream.write_all(answer.as_bytes()).unwrap();
        });
        port
    }

    #[test]
    fn health_is_read_over_loopback() {
        let probe = HttpHealth::default();
        let port = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":\"ok\",\"build\":\"b\",\"protocol\":1}",
        );
        let health = probe.probe(port).unwrap();
        assert_eq!(health, HealthResponse::ok("b"));
        let port = serve_once("HTTP/1.1 502 Bad Gateway\r\n\r\n{}");
        assert!(probe.probe(port).unwrap_err().contains("502"));
        let port = serve_once("HTTP/1.1 200 OK\r\n\r\nnot json");
        assert!(probe.probe(port).unwrap_err().contains("cannot read"));
        let port = serve_once("garbage");
        assert!(
            probe
                .probe(port)
                .unwrap_err()
                .contains("not an HTTP answer")
        );
        let port = free_port().unwrap();
        assert!(probe.probe(port).unwrap_err().contains("connect"));
    }

    #[test]
    fn the_state_round_trips_and_tokens_are_seen() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(BrokerState::read(dir.path()), BrokerState::default());
        let state = BrokerState {
            port: Some(4000),
            container: Some("c".to_owned()),
            image: Some("i".to_owned()),
            state: Some("running".to_owned()),
            build: Some("0.4.0-dev+abc".to_owned()),
            started_at: Some(1),
        };
        state.write(dir.path()).unwrap();
        assert_eq!(BrokerState::read(dir.path()), state);
        assert!(!tokens_active(dir.path()));
        let active = dir.path().join("broker").join("active");
        fs::create_dir_all(&active).unwrap();
        assert!(!tokens_active(dir.path()));
        fs::write(active.join("jti"), "run").unwrap();
        assert!(tokens_active(dir.path()));
    }

    #[test]
    fn the_client_s_version_is_its_last_word_and_read_again_when_it_changes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let client = dir.path().join("dagq-broker-client");
        let write = |version: &str| {
            let temporary = dir.path().join("next");
            fs::write(
                &temporary,
                format!("#!/bin/sh\necho dagq-broker-client {version}\n"),
            )
            .unwrap();
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755)).unwrap();
            fs::rename(&temporary, &client).unwrap();
        };
        write("0.4.0-dev+abc");
        assert_eq!(client_version(&client).unwrap(), "0.4.0-dev+abc");
        // Replaced by a rename (as install does): a new modification time.
        std::thread::sleep(Duration::from_millis(20));
        write("0.4.0-dev+def");
        assert_eq!(client_version(&client).unwrap(), "0.4.0-dev+def");
        fs::write(&client, "#!/bin/sh\nexit 3\n").unwrap();
        assert!(client_version(&client).unwrap_err().contains("exited"));
        assert!(client_version(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn ps_lines_are_pids_and_their_arguments() {
        let listed = parse_processes(
            "    1 /sbin/launchd\n  4242 /opt/podman/libexec/podman/gvproxy -listen-vfkit unixgram:///t/podman/dagq-gvproxy.sock\n\n  0 kernel\nbad line\n  7\n",
        );
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[1].pid, 4242);
        assert_eq!(listed[1].args[2], "unixgram:///t/podman/dagq-gvproxy.sock");
    }

    #[test]
    fn an_orphan_is_listed_ended_and_seen_gone() {
        // A sleep whose shell has exited, so it is not dagq's child (as a
        // gvproxy's parent is launchd) and its exit is reaped elsewhere.
        let output =
            unpiped_output(Command::new("/bin/sh").args(["-c", "/bin/sleep 60 & echo $!"]))
                .unwrap();
        let pid: u32 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap();
        let processes = SystemProcesses;
        let listed = processes.list().unwrap();
        let sleep = listed.iter().find(|process| process.pid == pid).unwrap();
        assert!(sleep.args[0].ends_with("sleep"), "{sleep:?}");
        assert_eq!(sleep.args[1], "60");
        assert!(processes.alive(pid));
        processes.signal(pid, Signal::Terminate).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while processes.alive(pid) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!processes.alive(pid));
        // A process already gone is not an error.
        processes.signal(pid, Signal::Kill).unwrap();
        // dagq itself, init and process groups are never signalled.
        for pid in [0, 1, std::process::id(), u32::MAX] {
            assert!(processes.signal(pid, Signal::Kill).is_err(), "{pid}");
            assert!(!processes.alive(pid) || pid == std::process::id() || pid == 1);
        }
    }
}
