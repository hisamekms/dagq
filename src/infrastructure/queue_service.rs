//! The queue service on the host ([Queue service]): the process
//! `dagq service serve` runs ([`serve`]), its unix socket under the
//! queue's directory, the client a caller speaks to it with ([`call`]),
//! the tokens the control side issues for its principals ([`issue`],
//! [`revoke`]) and the control `up`, `down` and the supervisor start,
//! look at and stop it with ([`SystemQueueService`]).
//!
//! Everything lives in `<queue dir>/service/` (mode 0700): the socket
//! `queue.sock` (mode 0600), `state.json` (what the running service
//! recorded of itself), `lock` (held with `flock` while one runs, which is
//! how a look tells a live service from a stale record and how a second
//! one is refused), `service.log`, and the tokens: `tokens/<sha256 of the
//! value>.json` holds a token's principal, `credentials/<sha256 of the
//! actor id>` its value (mode 0600) for the caller to be handed the path
//! of. No token value is ever written to a log, an event or an error.
//!
//! [Queue service]: ../../docs/design/queue-service.md

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use super::adapters::Cmux;
use super::dialogue::DialogueQueue;
use super::sqlite::SqliteQueue;
use crate::application::commands::DenialLog;
use crate::application::commands::dialogue::{DialogueStore, MarkChange};
use crate::application::queue_reads::QueueRead;
use crate::application::queue_service::{
    QueueService, QueueServiceControl, ServiceAccess, ServiceBackend, ServiceProbe, ServiceQueue,
};
use crate::application::{Exit, Generators, RunLog, RunNotFound, Spawned, TaskStore};
use crate::domain::queue_service::{
    API_VERSION, LOCK_FILE, LOG_FILE, MAX_REQUEST_BYTES, Principal, SERVICE_DIR, SOCKET_FILE,
    STATE_FILE, ServiceErrorCode, ServiceRequest, ServiceResponse, ServiceState, UseCase,
};
use crate::domain::{
    ActorContext, ActorRole, Answerer, Ask, AskId, EventKind, Finding, FindingId, FindingOutcome,
    FindingStatus, NewAsk, NewFinding, NewNote, ProposalId, RunEvent, RunId, RunStatus, TaskId,
};

/// The tokens' principals in [`SERVICE_DIR`].
pub const TOKENS_DIR: &str = "tokens";
/// The tokens' values in [`SERVICE_DIR`].
pub const CREDENTIALS_DIR: &str = "credentials";
/// The longest socket path a unix socket takes on macOS (`sun_path`).
const MAX_SOCKET_PATH: usize = 103;
/// How long a look waits for `hello`.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the service waits for one request or one response.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the service looks whether its queue is still there.
const QUEUE_CHECK: Duration = Duration::from_secs(2);

/// `<queue dir>/service`.
pub fn service_dir(queue_dir: &Path) -> PathBuf {
    queue_dir.join(SERVICE_DIR)
}

/// `<queue dir>/service/queue.sock`.
/// `<queue dir>/service/queue.sock`, or, when that path is longer than a
/// unix socket takes (a queue under a long temporary directory),
/// `/tmp/dagq-<uid>/<hash of the queue dir>.sock` in a directory of this
/// user's with mode 0700. Every caller computes the same path from the
/// queue's directory.
pub fn socket_path(queue_dir: &Path) -> PathBuf {
    let socket = service_dir(queue_dir).join(SOCKET_FILE);
    if socket.as_os_str().len() <= MAX_SOCKET_PATH {
        return socket;
    }
    let real = fs::canonicalize(queue_dir).unwrap_or_else(|_| queue_dir.to_path_buf());
    let hash = sha256_hex(real.as_os_str().as_encoded_bytes());
    short_socket_dir().join(format!("{}.sock", &hash[..16]))
}

/// The directory of the sockets that do not fit under their queue.
fn short_socket_dir() -> PathBuf {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/dagq-{uid}"))
}

/// Make the directory `socket` goes in when it is not the queue's own:
/// this user's, mode 0700, and no link.
fn prepare_socket_dir(socket: &Path) -> Result<()> {
    let dir = socket.parent().context("a socket needs a directory")?;
    match fs::create_dir(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("create {}", dir.display())),
    }
    let metadata = fs::symlink_metadata(dir)?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    use std::os::unix::fs::MetadataExt;
    ensure!(
        metadata.is_dir() && metadata.uid() == uid,
        "{} is not a directory of this user's",
        dir.display()
    );
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("narrow {}", dir.display()))
}

/// `<queue dir>/service/service.log`.
pub fn log_path(queue_dir: &Path) -> PathBuf {
    service_dir(queue_dir).join(LOG_FILE)
}

fn queue_dir_of(db: &Path) -> PathBuf {
    db.parent().unwrap_or(Path::new(".")).to_path_buf()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Make `dir` (and its parents) with mode 0700.
fn private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("narrow {}", dir.display()))
}

/// Write `bytes` to `path` with mode 0600 through a temporary file and a
/// rename, so a reader never sees half of it.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("a file needs a directory")?;
    let temporary = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written.with_context(|| format!("write {}", path.display()))
}

// --- Tokens (ADR-t1233-4 decision 4) ---------------------------------------

/// What a token's record holds: its principal and when it was issued.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenRecord {
    principal: Principal,
    issued_at: i64,
}

/// A token the control side issued: the file its value is in, for the
/// caller to be handed the path of (never the value). Its `Debug` names
/// the file only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedToken {
    pub file: PathBuf,
    pub principal: Principal,
}

fn credential_path(queue_dir: &Path, actor_id: &str) -> PathBuf {
    service_dir(queue_dir)
        .join(CREDENTIALS_DIR)
        .join(sha256_hex(actor_id.as_bytes()))
}

fn record_path(queue_dir: &Path, token: &str) -> PathBuf {
    service_dir(queue_dir)
        .join(TOKENS_DIR)
        .join(format!("{}.json", sha256_hex(token.as_bytes())))
}

/// Issue a token for `principal`, revoking the one its actor held: 32
/// random bytes in hex, its value in `credentials/` outside any worktree
/// and run directory, its principal in `tokens/` under the value's hash.
/// Only the control side calls this; no command of an AI actor's does
/// (ADR-t1233-4 decision 4).
pub fn issue(queue_dir: &Path, principal: &Principal, now: i64) -> Result<IssuedToken> {
    principal.check().map_err(anyhow::Error::msg)?;
    revoke(queue_dir, &principal.actor_id)?;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("draw a token's random bytes: {error}"))?;
    let token: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    for dir in [TOKENS_DIR, CREDENTIALS_DIR] {
        private_dir(&service_dir(queue_dir).join(dir))?;
    }
    let record = TokenRecord {
        principal: principal.clone(),
        issued_at: now,
    };
    // The value first: a token whose record is not written yet is no
    // token, and one whose value is written can always be revoked.
    let file = credential_path(queue_dir, &principal.actor_id);
    write_private(&file, token.as_bytes())?;
    write_private(
        &record_path(queue_dir, &token),
        &serde_json::to_vec(&record)?,
    )?;
    Ok(IssuedToken {
        file,
        principal: principal.clone(),
    })
}

/// Revoke the token `actor_id` holds (its run ended, its lease was lost,
/// or a resume issues it another); whether there was one.
pub fn revoke(queue_dir: &Path, actor_id: &str) -> Result<bool> {
    let file = credential_path(queue_dir, actor_id);
    let token = match fs::read_to_string(&file) {
        Ok(token) => token,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("read {}", file.display())),
    };
    match fs::remove_file(record_path(queue_dir, token.trim())) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("remove a token's record"),
    }
    fs::remove_file(&file).with_context(|| format!("remove {}", file.display()))?;
    Ok(true)
}

/// The principal `token` was issued for, `None` for one never issued or
/// revoked.
pub fn lookup(queue_dir: &Path, token: &str) -> Result<Option<Principal>> {
    // A hex value of the issued length only: no other text names a file.
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let path = record_path(queue_dir, token);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read a token's record"),
    };
    let record: TokenRecord = serde_json::from_slice(&bytes).context("a token's record")?;
    Ok(Some(record.principal))
}

/// The value of the token in `file`, as a caller reads the file it was
/// handed.
pub fn read_token(file: &Path) -> Result<String> {
    Ok(fs::read_to_string(file)
        .with_context(|| format!("read the token file {}", file.display()))?
        .trim()
        .to_owned())
}

/// The control side's [`ServiceAccess`] on this host: the tokens under the
/// queue's `service/`, whose files the actors are handed the paths of.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemServiceAccess;

impl ServiceAccess for SystemServiceAccess {
    fn socket(&self, db: &Path) -> PathBuf {
        socket_path(&queue_dir_of(db))
    }

    fn issue(&self, db: &Path, principal: &Principal) -> Result<PathBuf> {
        Ok(issue(&queue_dir_of(db), principal, unix_now())?.file)
    }

    fn credential(&self, db: &Path, actor_id: &str) -> Option<PathBuf> {
        let file = credential_path(&queue_dir_of(db), actor_id);
        file.is_file().then_some(file)
    }

    fn revoke_on_exit(
        &self,
        db: &Path,
        actor_id: &str,
        child: Box<dyn Spawned>,
    ) -> Box<dyn Spawned> {
        Box::new(Revoking {
            child,
            queue_dir: queue_dir_of(db),
            actor_id: actor_id.to_owned(),
            revoked: false,
        })
    }
}

/// A job's process that revokes its token once it has ended: at the wait
/// that sees it end, or when the handle goes (a job given up is killed
/// first).
struct Revoking {
    child: Box<dyn Spawned>,
    queue_dir: PathBuf,
    actor_id: String,
    revoked: bool,
}

impl Revoking {
    fn revoke(&mut self) {
        if self.revoked {
            return;
        }
        self.revoked = true;
        if let Err(error) = revoke(&self.queue_dir, &self.actor_id) {
            warn!(actor_id = %self.actor_id, "the token of {} could not be revoked: {error:#}", self.actor_id);
        }
    }
}

impl Spawned for Revoking {
    fn id(&self) -> u32 {
        self.child.id()
    }
    fn try_wait(&mut self) -> Result<Option<Exit>> {
        let exit = self.child.try_wait()?;
        if exit.is_some() {
            self.revoke();
        }
        Ok(exit)
    }
    fn kill(&mut self) -> Result<()> {
        self.child.kill()
    }
    fn wait(&mut self) -> Result<Exit> {
        let exit = self.child.wait()?;
        self.revoke();
        Ok(exit)
    }
    fn kill_group(&mut self) -> Result<()> {
        self.child.kill_group()
    }
}

impl Drop for Revoking {
    fn drop(&mut self) {
        self.revoke();
    }
}

// --- The client --------------------------------------------------------------

/// Send `request` to the service at `socket` and read its answer; an error
/// when nothing answers (the caller does not fall back to the DB,
/// ADR-t1233-1 decision 7).
pub fn call(socket: &Path, request: &ServiceRequest, timeout: Duration) -> Result<ServiceResponse> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("the queue service at {} is unreachable", socket.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .context("send the request to the queue service")?;
    stream.shutdown(std::net::Shutdown::Write).ok();
    let mut answer = Vec::new();
    stream
        .take(MAX_REQUEST_BYTES as u64 * 16)
        .read_to_end(&mut answer)
        .context("read the queue service's answer")?;
    serde_json::from_slice(&answer).context("the queue service's answer does not read")
}

/// `hello` to the service at `socket`.
pub fn hello(socket: &Path, timeout: Duration) -> Result<Value> {
    let response = call(
        socket,
        &ServiceRequest {
            api_version: API_VERSION,
            token: None,
            use_case: UseCase::Hello,
            params: Value::Null,
        },
        timeout,
    )?;
    match (response.ok, response.result, response.error) {
        (true, Some(result), _) => Ok(result),
        (_, _, Some(error)) => bail!("{}: {}", error.code.as_str(), error.message),
        _ => bail!("the queue service answered hello with nothing"),
    }
}

// --- Client mode (goal 82's stage (3)) --------------------------------------

/// How long a client-mode `dagq` waits for the service's answer: a read of
/// the whole queue (`stats --full`, `kpi`) may take a while.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(300);

/// Why a client-mode `dagq` did not get its answer: the service's own code
/// (`authorization_denied`, `unauthenticated`, `bad_request`, `failed`,
/// `api_version_mismatch`) or the client's (`unreachable`: no service
/// answers on the socket; `no_credential`: the token's file does not
/// read; `no_use_case`: the command is none of the service's;
/// `queue_named`: the command names a queue to open).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientFailure {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ClientFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ClientFailure {}

impl ClientFailure {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

/// Where a client-mode `dagq` sends its commands (ADR-t1233-1 decision 7):
/// the socket and the token's file the control side put in its
/// environment ([`SOCKET_ENV`](crate::domain::queue_service::SOCKET_ENV),
/// [`CREDENTIAL_FILE_ENV`](crate::domain::queue_service::CREDENTIAL_FILE_ENV)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub socket: PathBuf,
    /// `None` when the environment names none: the service then refuses
    /// the call as `unauthenticated`.
    pub credential: Option<PathBuf>,
}

impl Client {
    /// The client `env` names: `None` without a socket, when `dagq` opens
    /// the queue itself.
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Option<Self> {
        use crate::domain::queue_service::{CREDENTIAL_FILE_ENV, SOCKET_ENV};
        let named = |name: &str| env(name).filter(|value| !value.trim().is_empty());
        Some(Self {
            socket: PathBuf::from(named(SOCKET_ENV)?),
            credential: named(CREDENTIAL_FILE_ENV).map(PathBuf::from),
        })
    }

    /// `use_case` with `params` done by the service as the principal of
    /// the token: its result, or a [`ClientFailure`]. A service that does
    /// not answer is a failure; the client never opens the queue instead
    /// (fail closed).
    pub fn call(&self, use_case: UseCase, params: Value) -> Result<Value> {
        let token = match &self.credential {
            Some(file) => Some(
                read_token(file)
                    .map_err(|error| ClientFailure::new("no_credential", format!("{error:#}")))?,
            ),
            None => None,
        };
        let request = ServiceRequest {
            api_version: API_VERSION,
            token,
            use_case,
            params,
        };
        let response = call(&self.socket, &request, CLIENT_TIMEOUT).map_err(|error| {
            ClientFailure::new(
                "unreachable",
                format!(
                    "{error:#}; a client-mode dagq does not open the queue itself \
                     (dagq service status, from the control side, says why)"
                ),
            )
        })?;
        if let Some(error) = response.error {
            return Err(ClientFailure::new(error.code.as_str(), error.message).into());
        }
        if !crate::domain::queue_service::understands(&response) {
            return Err(ClientFailure::new(
                ServiceErrorCode::ApiVersionMismatch.as_str(),
                format!(
                    "the queue service answers API versions {} to {}, not {API_VERSION}",
                    response.min_api_version, response.api_version
                ),
            )
            .into());
        }
        match (response.ok, response.result) {
            (true, Some(result)) => Ok(result),
            _ => Err(ClientFailure::new("failed", "the queue service answered nothing").into()),
        }
    }
}

// --- The service's record of itself ----------------------------------------

/// What a running service records in `state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceRecord {
    pub pid: u32,
    pub build: String,
    pub api_version: u32,
    pub socket: PathBuf,
    pub started_at: i64,
}

impl ServiceRecord {
    pub fn read(queue_dir: &Path) -> Option<Self> {
        let bytes = fs::read(service_dir(queue_dir).join(STATE_FILE)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

/// An exclusive `flock` on the service's lock file, held while it lives.
struct Lock {
    _file: File,
}

impl Lock {
    /// Take the lock without waiting; `None` while another holds it.
    fn try_take(
        path: &Path,
        create: bool,
        operation: libc::c_int,
    ) -> std::io::Result<Option<Self>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
            Ok(Some(Self { _file: file }))
        } else {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

/// Whether a service holds the queue's lock now. A look that finds no
/// lock file finds none. A look takes the lock shared and lets it go at
/// once, so two looks never take each other for a service; only the
/// service takes it exclusive.
fn lock_held(queue_dir: &Path) -> bool {
    match Lock::try_take(
        &service_dir(queue_dir).join(LOCK_FILE),
        false,
        libc::LOCK_SH,
    ) {
        Ok(Some(_)) | Err(_) => false,
        Ok(None) => true,
    }
}

// --- The service ---------------------------------------------------------------

/// How the service answers a read use case: as the command line reads it
/// (`compose::read_queue`), given by the composition root. It takes the
/// request's queue, its DB and the cmux that lists the workspaces for
/// `stats` (none when it does not resolve).
pub type ServiceReads =
    Arc<dyn Fn(&mut SqliteQueue, &Path, Option<&Cmux>, &QueueRead) -> Result<Value> + Send + Sync>;

/// How `dagq service serve` runs.
#[derive(Clone)]
pub struct ServeOptions {
    pub db: PathBuf,
    /// The cmux a new ask notifies the inbox through.
    pub cmux: PathBuf,
    pub generators: Generators,
    pub reads: ServiceReads,
    /// Set by SIGINT or SIGTERM: the service stops accepting and exits.
    pub stop: Arc<AtomicBool>,
    /// How often the loop looks for a connection or a stop.
    pub poll: Duration,
}

/// Run the queue's service until `stop` is set or its queue goes: take
/// the queue's lock (a second service of the queue is refused), bind the
/// socket, record itself, and answer each connection on its own thread.
pub fn serve(options: &ServeOptions) -> Result<Value> {
    let queue_dir = queue_dir_of(&options.db);
    ensure!(options.db.is_file(), "no queue at {}", options.db.display());
    let dir = service_dir(&queue_dir);
    private_dir(&dir)?;
    let socket = socket_path(&queue_dir);
    if !socket.starts_with(&dir) {
        prepare_socket_dir(&socket)?;
    }
    ensure!(
        socket.as_os_str().len() <= MAX_SOCKET_PATH,
        "the queue service's socket path {} is longer than {MAX_SOCKET_PATH} bytes",
        socket.display()
    );
    // A service starting while a look holds the lock for an instant waits
    // a little; one that another service holds is refused.
    let deadline = Instant::now() + Duration::from_secs(2);
    let lock = loop {
        if let Some(lock) = Lock::try_take(&dir.join(LOCK_FILE), true, libc::LOCK_EX)? {
            break lock;
        }
        if Instant::now() >= deadline {
            let pid = ServiceRecord::read(&queue_dir).map(|record| record.pid);
            bail!(
                "a queue service already runs for this queue (pid {})",
                pid.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string())
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    match fs::remove_file(&socket) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("remove {}", socket.display()));
        }
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let pid = std::process::id();
    let record = ServiceRecord {
        pid,
        build: crate::VERSION.to_owned(),
        api_version: API_VERSION,
        socket: socket.clone(),
        started_at: unix_now(),
    };
    write_private(&dir.join(STATE_FILE), &serde_json::to_vec(&record)?)?;
    info!(pid, socket = %socket.display(), "the queue service listens");
    let backend = Arc::new(SqliteServiceBackend {
        db: options.db.clone(),
        queue_dir: queue_dir.clone(),
        cmux: options.cmux.clone(),
        generators: options.generators.clone(),
        reads: options.reads.clone(),
    });
    let mut served = 0u64;
    let mut checked = Instant::now();
    let outcome = loop {
        if options.stop.load(Ordering::SeqCst) {
            break "stopped";
        }
        if checked.elapsed() >= QUEUE_CHECK {
            checked = Instant::now();
            if !options.db.is_file() {
                break "queue_gone";
            }
        }
        match listener.accept() {
            Ok((stream, _)) => {
                served += 1;
                let backend = backend.clone();
                thread::spawn(move || {
                    if let Err(error) = answer(stream, &*backend, pid) {
                        warn!("a queue service connection failed: {error:#}");
                    }
                });
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => thread::sleep(options.poll),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => {
                warn!("the queue service could not accept: {error}");
                thread::sleep(options.poll);
            }
        }
    };
    // What this process put in place goes; a record another wrote stays.
    if ServiceRecord::read(&queue_dir).is_some_and(|record| record.pid == pid) {
        let _ = fs::remove_file(dir.join(STATE_FILE));
        let _ = fs::remove_file(&socket);
    }
    drop(lock);
    info!(pid, outcome, "the queue service stopped");
    Ok(json!({"outcome": outcome, "pid": pid, "served": served}))
}

/// Read one request from `stream`, answer it, and close.
fn answer(stream: UnixStream, backend: &dyn ServiceBackend, pid: u32) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut line = Vec::new();
    BufReader::new(stream.try_clone()?)
        .take(MAX_REQUEST_BYTES as u64 + 1)
        .read_until(b'\n', &mut line)?;
    let response = if line.len() > MAX_REQUEST_BYTES {
        ServiceResponse::failure(ServiceErrorCode::BadRequest, "the request is too long")
    } else {
        match serde_json::from_slice::<ServiceRequest>(&line) {
            Ok(request) => QueueService {
                backend,
                build: crate::VERSION,
                pid,
            }
            .handle(&request),
            Err(error) => ServiceResponse::failure(
                ServiceErrorCode::BadRequest,
                format!("the request does not read: {error}"),
            ),
        }
    };
    let mut out = serde_json::to_vec(&response)?;
    out.push(b'\n');
    (&stream).write_all(&out)?;
    Ok(())
}

/// The queue the service's use cases run on: a fresh connection to the
/// DB per request, written as the request's principal.
pub struct SqliteServiceBackend {
    pub db: PathBuf,
    pub queue_dir: PathBuf,
    pub cmux: PathBuf,
    pub generators: Generators,
    pub reads: ServiceReads,
}

impl SqliteServiceBackend {
    /// The service itself, as the records name it: a part of the control
    /// side (ADR-t1233-1 decision 1).
    fn own_actor() -> ActorContext {
        ActorContext::instance(
            ActorRole::Supervisor,
            format_args!("queue-service:{}", std::process::id()),
        )
    }
}

impl ServiceBackend for SqliteServiceBackend {
    fn principal(&self, token: &str) -> Result<Option<Principal>> {
        lookup(&self.queue_dir, token)
    }

    fn open(&self, actor: &ActorContext) -> Result<Box<dyn ServiceQueue>> {
        let queue = SqliteQueue::open(&self.db)?
            .with_generators(self.generators.clone())
            .with_actor(actor.clone());
        Ok(Box::new(ServiceSqlite {
            queue,
            db: self.db.clone(),
            queue_dir: self.queue_dir.clone(),
            cmux: Cmux {
                executable: self.cmux.clone(),
            },
            reads: self.reads.clone(),
        }))
    }

    fn record_unauthenticated(&self, payload: Value) -> Result<()> {
        let queue = SqliteQueue::open(&self.db)?
            .with_generators(self.generators.clone())
            .with_actor(Self::own_actor());
        RunLog::record_queue_event(&queue, EventKind::QueueServiceUnauthenticated, payload)
            .map(drop)
    }
}

/// One request's queue.
struct ServiceSqlite {
    queue: SqliteQueue,
    db: PathBuf,
    queue_dir: PathBuf,
    cmux: Cmux,
    reads: ServiceReads,
}

impl ServiceSqlite {
    fn dialogue(&mut self) -> DialogueQueue<'_> {
        DialogueQueue {
            queue: &mut self.queue,
            checkout: &self.queue_dir,
            cmux: &self.cmux,
        }
    }
}

impl DenialLog for ServiceSqlite {
    fn record_denial(&self, payload: Value) -> Result<()> {
        RunLog::record_queue_event(&self.queue, EventKind::AuthorizationDenied, payload).map(drop)
    }
}

impl DialogueStore for ServiceSqlite {
    fn read_ask(&self, id: AskId) -> Result<Ask> {
        self.queue.read_ask(id)
    }
    fn open_ask(&mut self, ask: NewAsk) -> Result<Value> {
        self.dialogue().open_ask(ask)
    }
    fn answer(&mut self, id: AskId, text: &str, answerer: Answerer) -> Result<Ask> {
        self.dialogue().answer(id, text, answerer)
    }
    fn close_ask(&mut self, id: AskId) -> Result<Ask> {
        self.dialogue().close_ask(id)
    }
    fn add_note(&mut self, note: NewNote) -> Result<RunEvent> {
        self.dialogue().add_note(note)
    }
    fn mark(&mut self, change: MarkChange, by: &str) -> Result<Value> {
        self.dialogue().mark(change, by)
    }
    fn record_finding(&mut self, finding: NewFinding) -> Result<FindingOutcome> {
        self.dialogue().record_finding(finding)
    }
    fn set_finding_status(
        &mut self,
        id: FindingId,
        to: FindingStatus,
        reason: &str,
        by: &str,
    ) -> Result<Finding> {
        self.dialogue().set_finding_status(id, to, reason, by)
    }
}

impl ServiceQueue for ServiceSqlite {
    fn show(&mut self, id: TaskId, full: bool, events: usize) -> Result<Value> {
        let detail = TaskStore::show(&mut self.queue, id)?;
        Ok(if full {
            serde_json::to_value(detail)?
        } else {
            crate::view::task_detail(&detail, events)
        })
    }

    fn run_status(&self, id: &RunId) -> Result<Option<RunStatus>> {
        match RunLog::run(&self.queue, id) {
            Ok(run) => Ok(Some(run.status())),
            Err(error) if error.downcast_ref::<RunNotFound>().is_some() => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn proposals(&self, all: bool) -> Result<Value> {
        Ok(json!({"proposals": TaskStore::proposals(&self.queue, all)?}))
    }

    fn show_proposal(&self, id: ProposalId) -> Result<Value> {
        Ok(serde_json::to_value(TaskStore::show_proposal(
            &self.queue,
            id,
        )?)?)
    }

    /// As the command line reads it ([`ServiceReads`]), with the
    /// service's own cmux for `stats`' workspaces: as for the command
    /// line, a cmux that does not resolve lists none.
    fn read(&mut self, read: &QueueRead) -> Result<Value> {
        let cmux = super::adapters::executable(&self.cmux.executable)
            .ok()
            .map(|executable| Cmux { executable });
        (self.reads)(&mut self.queue, &self.db, cmux.as_ref(), read)
    }
}

// --- The control `up`, `down` and the supervisor use -------------------------

/// The queue's service on this host: started from `executable` (the
/// fixed binary that runs `up` or the supervisor, ADR-t1233-4 decision
/// 1), its output appended to `service.log`. The service is started
/// through a second fork, so it is no child of its starter's: a
/// supervisor that execs another binary leaves no service to reap.
#[derive(Debug, Clone)]
pub struct SystemQueueService {
    pub db: PathBuf,
    pub executable: PathBuf,
    pub cmux: PathBuf,
}

impl SystemQueueService {
    pub fn new(db: &Path, executable: &Path, cmux: &Path) -> Self {
        Self {
            db: db.to_path_buf(),
            executable: executable.to_path_buf(),
            cmux: cmux.to_path_buf(),
        }
    }

    fn queue_dir(&self) -> PathBuf {
        queue_dir_of(&self.db)
    }
}

/// Look at the service of the queue in `queue_dir` without a binary to
/// start one: `status` and `doctor`.
pub fn probe(queue_dir: &Path) -> ServiceProbe {
    let socket = socket_path(queue_dir);
    let record = ServiceRecord::read(queue_dir);
    let stopped = ServiceProbe {
        state: ServiceState::Stopped,
        socket: socket.clone(),
        pid: None,
        build: None,
        api_version: None,
        min_api_version: None,
        build_matches: None,
        started_at: None,
        error: None,
    };
    if !lock_held(queue_dir) {
        return stopped;
    }
    let recorded = ServiceProbe {
        pid: record.as_ref().map(|record| record.pid),
        build: record.as_ref().map(|record| record.build.clone()),
        api_version: record.as_ref().map(|record| record.api_version),
        build_matches: record.as_ref().map(|record| record.build == crate::VERSION),
        started_at: record.as_ref().map(|record| record.started_at),
        ..stopped
    };
    match hello(&socket, PROBE_TIMEOUT) {
        Ok(hello) => {
            let build = hello["build"].as_str().map(str::to_owned);
            ServiceProbe {
                state: ServiceState::Running,
                pid: hello["pid"]
                    .as_u64()
                    .and_then(|pid| u32::try_from(pid).ok()),
                build_matches: build.as_deref().map(|build| build == crate::VERSION),
                build,
                api_version: hello["api_version"]
                    .as_u64()
                    .and_then(|v| u32::try_from(v).ok()),
                min_api_version: hello["min_api_version"]
                    .as_u64()
                    .and_then(|v| u32::try_from(v).ok()),
                ..recorded
            }
        }
        Err(error) => ServiceProbe {
            state: ServiceState::Unreachable,
            error: Some(format!("{error:#}")),
            ..recorded
        },
    }
}

fn signal(pid: u32, signal: libc::c_int) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: a signal to a pid; an error (no such process) is fine.
        unsafe { libc::kill(pid, signal) };
    }
}

/// Whether `pid` runs `dagq ... service serve` for `db` now: a pid read
/// from a record, not from the service's own answer, is signalled only
/// then, never a process that took it later.
fn serves(pid: u32, db: &Path) -> bool {
    let Ok(output) = crate::infrastructure::adapters::unpiped_output(Command::new("ps").args([
        "-o",
        "command=",
        "-p",
        &pid.to_string(),
    ])) else {
        return false;
    };
    let command = String::from_utf8_lossy(&output.stdout);
    output.status.success()
        && command.contains("service serve")
        && command.contains(&*db.to_string_lossy())
}

impl QueueServiceControl for SystemQueueService {
    fn probe(&self) -> ServiceProbe {
        probe(&self.queue_dir())
    }

    fn start(&self, timeout: Duration) -> Result<ServiceProbe> {
        let found = self.probe();
        if found.current() {
            return Ok(found);
        }
        if found.state != ServiceState::Stopped {
            self.stop(timeout)?;
        }
        ensure!(
            self.executable.is_file(),
            "no binary at {} to start the queue service with",
            self.executable.display()
        );
        let queue_dir = self.queue_dir();
        private_dir(&service_dir(&queue_dir))?;
        let log_file = log_path(&queue_dir);
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&log_file)
            .with_context(|| format!("open {}", log_file.display()))?;
        let mut command = Command::new(&self.executable);
        command
            .arg("--db")
            .arg(&self.db)
            .args(["service", "serve", "--cmux"])
            .arg(&self.cmux)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        // The service is the control side's, whoever started it: no
        // actor's variables reach it, nor a client-mode `dagq`'s (the
        // service opens the queue it names).
        for name in crate::domain::actor::ACTOR_ENV
            .into_iter()
            .chain(crate::domain::queue_service::CLIENT_ENV)
        {
            command.env_remove(name);
        }
        // SAFETY: setsid(2), fork(2) and _exit(2) are async-signal-safe and
        // touch no memory. The service leaves the starter's session, so a
        // terminal's signals do not reach it, and the first child exits at
        // once, so the service is reparented and never left to reap.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                match libc::fork() {
                    -1 => Err(std::io::Error::last_os_error()),
                    0 => Ok(()),
                    _ => libc::_exit(0),
                }
            });
        }
        // The exec's failure still comes back here: the second child holds
        // the spawn's pipe until it execs.
        let mut intermediate = command
            .spawn()
            .with_context(|| format!("start {}", self.executable.display()))?;
        intermediate.wait()?;
        let deadline = Instant::now() + timeout;
        loop {
            // Any service of this build that answers will do: another
            // starter may have won the lock.
            let found = probe(&queue_dir);
            if found.current() {
                return Ok(found);
            }
            if Instant::now() >= deadline {
                bail!(
                    "the queue service did not answer within {}s ({}); see {}",
                    timeout.as_secs(),
                    found.state.as_str(),
                    log_file.display()
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn stop(&self, timeout: Duration) -> Result<Option<u32>> {
        let queue_dir = self.queue_dir();
        let clean = || {
            // A record and a socket no service holds are left over.
            if !lock_held(&queue_dir) {
                let _ = fs::remove_file(service_dir(&queue_dir).join(STATE_FILE));
                let _ = fs::remove_file(socket_path(&queue_dir));
            }
        };
        let deadline = Instant::now() + timeout;
        // The pid the service answered with; a service that holds its lock
        // and does not answer (starting, or hung) is looked at again, and
        // its recorded pid is signalled only while it runs `service serve`
        // for this queue.
        let pid = loop {
            let found = self.probe();
            match found.state {
                ServiceState::Stopped => {
                    clean();
                    return Ok(None);
                }
                ServiceState::Running => {
                    break found
                        .pid
                        .context("the queue service answered with no pid")?;
                }
                ServiceState::Unreachable => {
                    if Instant::now() >= deadline {
                        match found.pid.filter(|pid| serves(*pid, &self.db)) {
                            Some(pid) => break pid,
                            None => bail!(
                                "the queue service holds its lock and does not answer, and no \
                                 process of it was found to stop: {}",
                                found.error.unwrap_or_default()
                            ),
                        }
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        };
        signal(pid, libc::SIGTERM);
        let deadline = Instant::now() + timeout;
        while lock_held(&queue_dir) {
            if Instant::now() >= deadline {
                warn!(pid, "the queue service did not stop on SIGTERM: killing it");
                signal(pid, libc::SIGKILL);
                thread::sleep(Duration::from_millis(200));
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        clean();
        Ok(Some(pid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_names_its_principal_until_it_is_revoked_or_issued_again() {
        let dir = tempfile::tempdir().unwrap();
        let run = RunId::new("r1").unwrap();
        let principal = Principal::worker(&run, TaskId::new(3));
        let issued = issue(dir.path(), &principal, 10).unwrap();
        let token = read_token(&issued.file).unwrap();
        assert_eq!(token.len(), 64);
        assert!(!format!("{issued:?}").contains(&token));
        assert_eq!(lookup(dir.path(), &token).unwrap(), Some(principal.clone()));
        let mode = fs::metadata(&issued.file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // A resume's token replaces the run's.
        let again = issue(dir.path(), &principal, 11).unwrap();
        let second = read_token(&again.file).unwrap();
        assert_ne!(second, token);
        assert_eq!(lookup(dir.path(), &token).unwrap(), None);
        assert_eq!(
            lookup(dir.path(), &second).unwrap(),
            Some(principal.clone())
        );
        assert!(revoke(dir.path(), &principal.actor_id).unwrap());
        assert!(!revoke(dir.path(), &principal.actor_id).unwrap());
        assert_eq!(lookup(dir.path(), &second).unwrap(), None);
        // Text that is no issued value names no file.
        for forged in ["", "../../state", "zz", &"a".repeat(64)] {
            assert_eq!(lookup(dir.path(), forged).unwrap(), None);
        }
        // The control side issues no token for itself or a person.
        let user = Principal::of(&ActorContext::user());
        assert!(issue(dir.path(), &user, 1).is_err());
    }

    #[test]
    fn a_queue_too_deep_for_a_socket_gets_a_short_one() {
        let dir = tempfile::tempdir().unwrap();
        let short = dir.path().join("q");
        assert_eq!(socket_path(&short), short.join("service/queue.sock"));
        let deep = dir.path().join("d".repeat(120));
        let socket = socket_path(&deep);
        assert!(socket.as_os_str().len() <= MAX_SOCKET_PATH, "{socket:?}");
        assert!(socket.starts_with(short_socket_dir()));
        assert_eq!(socket, socket_path(&deep));
        assert_ne!(socket, socket_path(&dir.path().join("e".repeat(120))));
    }

    #[test]
    fn a_queue_with_no_service_looks_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let found = probe(dir.path());
        assert_eq!(found.state, ServiceState::Stopped);
        assert_eq!(found.socket, socket_path(dir.path()));
        assert!(!found.current());
        // A record no service holds the lock for is left over, and a stop
        // clears it.
        private_dir(&service_dir(dir.path())).unwrap();
        let record = ServiceRecord {
            pid: u32::MAX - 1,
            build: "x".into(),
            api_version: 1,
            socket: socket_path(dir.path()),
            started_at: 0,
        };
        write_private(
            &service_dir(dir.path()).join(STATE_FILE),
            &serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        fs::write(service_dir(dir.path()).join(LOCK_FILE), b"").unwrap();
        assert_eq!(probe(dir.path()).state, ServiceState::Stopped);
        let control = SystemQueueService::new(
            &dir.path().join("queue.db"),
            Path::new("/nonexistent/dagq"),
            Path::new("cmux"),
        );
        assert_eq!(control.stop(Duration::from_secs(1)).unwrap(), None);
        assert!(ServiceRecord::read(dir.path()).is_none());
        // A start of a binary that is not there fails.
        assert!(control.start(Duration::from_secs(1)).is_err());
    }
}
