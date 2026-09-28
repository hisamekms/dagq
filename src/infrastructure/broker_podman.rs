//! The adapters of the broker's lifecycle ([`crate::application::broker`],
//! ADR-t827-3): [`PodmanCli`] runs the podman executable, [`FileLock`] is
//! the host-wide lock of dagq's machine (`flock` on
//! `$XDG_DATA_HOME/dagq/podman-machine.lock`), [`HttpHealth`] asks
//! `GET /v1/health` on `127.0.0.1` with a hand-written HTTP/1.1 request
//! (dagq has no HTTP dependency, ADR-t827-1 decision 2), [`CheckoutSource`]
//! stages the image's build context from a dagq checkout, and
//! [`BrokerState`] is `<queue dir>/broker/state.json`.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use dagq_broker_protocol::HealthResponse;
use serde::{Deserialize, Serialize};

use crate::application::broker::{
    BrokerFailure, BrokerResult, FailureCode, HealthProbe, HostLock, ImageSource, Podman,
    PodmanOutput, broker_dir,
};

/// The machine's lock file under dagq's data dir.
pub const MACHINE_LOCK_FILE: &str = "podman-machine.lock";
/// The queue's broker state file under `<queue dir>/broker`.
pub const STATE_FILE: &str = "state.json";
/// The Containerfile in a dagq checkout.
pub const CONTAINERFILE: &str = "containers/broker/Containerfile";
/// The crates the image builds, in a dagq checkout.
pub const IMAGE_CRATES: [&str; 2] = ["crates/dagq-broker-protocol", "crates/dagq-broker"];

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
        let output = Command::new(&self.executable)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| {
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

impl FileLock {
    /// The host-wide lock of dagq's machine, `<data home>/dagq/podman-machine.lock`.
    pub fn machine(data_home: &Path) -> Self {
        Self {
            path: data_home.join("dagq").join(MACHINE_LOCK_FILE),
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

/// A dagq checkout the image is built from.
#[derive(Debug, Clone)]
pub struct CheckoutSource {
    pub checkout: PathBuf,
}

impl CheckoutSource {
    /// The checkout this binary was built from, when it is still there and
    /// has the broker's sources (a dev build, ADR-t827-1 decision 6).
    pub fn of_this_build() -> Option<Self> {
        let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        Self::is_source(&checkout).then_some(Self { checkout })
    }

    /// Whether `dir` has what the image is built from.
    pub fn is_source(dir: &Path) -> bool {
        dir.join(CONTAINERFILE).is_file()
            && dir.join("Cargo.lock").is_file()
            && IMAGE_CRATES
                .iter()
                .all(|name| dir.join(name).join("Cargo.toml").is_file())
    }
}

impl ImageSource for CheckoutSource {
    fn stage(&self, dir: &Path) -> BrokerResult<String> {
        stage_context(&self.checkout, dir).map_err(|error| {
            BrokerFailure::new(
                FailureCode::ImageSourceMissing,
                format!(
                    "stage the image's build context from {}: {error:#}",
                    self.checkout.display()
                ),
            )
        })
    }
}

/// The workspace manifest of the image's build context: the two crates
/// the server needs and nothing else.
pub fn narrowed_manifest() -> String {
    let members = IMAGE_CRATES
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "# Written by dagq for the broker's image: the workspace narrowed to the server.\n\
         [workspace]\nmembers = [{members}]\nresolver = \"3\"\n"
    )
}

/// Put the image's build context in `dir` (emptied first): the
/// Containerfile, `Cargo.lock`, the sources and manifests of
/// [`IMAGE_CRATES`] (without their tests) and [`narrowed_manifest`].
/// Returns the Rust version of `rust-toolchain.toml` for the build stage.
pub fn stage_context(checkout: &Path, dir: &Path) -> Result<String> {
    if !CheckoutSource::is_source(checkout) {
        anyhow::bail!(
            "{} is not a dagq checkout with {CONTAINERFILE} and the broker's crates",
            checkout.display()
        );
    }
    if dir.exists() {
        fs::remove_dir_all(dir).with_context(|| format!("empty {}", dir.display()))?;
    }
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    fs::copy(checkout.join(CONTAINERFILE), dir.join("Containerfile"))
        .context("copy the Containerfile")?;
    fs::copy(checkout.join("Cargo.lock"), dir.join("Cargo.lock")).context("copy Cargo.lock")?;
    fs::write(dir.join("Cargo.toml"), narrowed_manifest()).context("write Cargo.toml")?;
    for name in IMAGE_CRATES {
        let (from, to) = (checkout.join(name), dir.join(name));
        fs::create_dir_all(&to)?;
        fs::copy(from.join("Cargo.toml"), to.join("Cargo.toml"))
            .with_context(|| format!("copy {name}/Cargo.toml"))?;
        copy_tree(&from.join("src"), &to.join("src"))
            .with_context(|| format!("copy {name}/src"))?;
    }
    rust_version(checkout)
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// The `channel` of the checkout's `rust-toolchain.toml`.
pub fn rust_version(checkout: &Path) -> Result<String> {
    let path = checkout.join("rust-toolchain.toml");
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("channel"))
        .filter_map(|rest| rest.trim().strip_prefix('='))
        .map(|value| value.trim().trim_matches('"').to_owned())
        .find(|value| !value.is_empty())
        .with_context(|| format!("{} names no channel", path.display()))
}

/// `<queue dir>/broker/state.json`: what `dagq broker` last did, and the
/// port the queue's container publishes (kept across restarts).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerState {
    pub port: Option<u16>,
    pub container: Option<String>,
    pub image: Option<String>,
    /// `running`, `stopped`, or a failure's code.
    pub state: Option<String>,
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
    fn the_context_is_the_server_and_nothing_else() {
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(CheckoutSource::is_source(checkout));
        assert_eq!(
            CheckoutSource::of_this_build().unwrap().checkout,
            checkout.to_path_buf()
        );
        let dir = tempfile::tempdir().unwrap();
        let context = dir.path().join("context");
        fs::create_dir_all(context.join("stale")).unwrap();
        let version = CheckoutSource {
            checkout: checkout.to_path_buf(),
        }
        .stage(&context)
        .unwrap();
        assert_eq!(version, rust_version(checkout).unwrap());
        assert!(version.starts_with("1."), "{version}");
        let mut top: Vec<String> = fs::read_dir(&context)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        top.sort();
        assert_eq!(top, ["Cargo.lock", "Cargo.toml", "Containerfile", "crates"]);
        let mut crates: Vec<String> = fs::read_dir(context.join("crates"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        crates.sort();
        assert_eq!(crates, ["dagq-broker", "dagq-broker-protocol"]);
        assert!(context.join("crates/dagq-broker/src/main.rs").is_file());
        assert!(!context.join("crates/dagq-broker/tests").exists());
        let manifest = fs::read_to_string(context.join("Cargo.toml")).unwrap();
        assert!(
            manifest
                .contains("members = [\"crates/dagq-broker-protocol\", \"crates/dagq-broker\"]")
        );
        let error = CheckoutSource {
            checkout: dir.path().to_path_buf(),
        }
        .stage(&context)
        .unwrap_err();
        assert_eq!(error.code, FailureCode::ImageSourceMissing);
    }

    #[test]
    fn rust_version_reads_the_channel() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\n# channel = \"old\"\nchannel = \"1.2.3\"\n",
        )
        .unwrap();
        assert_eq!(rust_version(dir.path()).unwrap(), "1.2.3");
        fs::write(dir.path().join("rust-toolchain.toml"), "[toolchain]\n").unwrap();
        assert!(rust_version(dir.path()).is_err());
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
}
