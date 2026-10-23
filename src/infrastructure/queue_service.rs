//! The queue service on the host ([Queue service]): the process
//! `dagq service serve` runs ([`serve`]), its unix socket under the
//! queue's directory, the client a caller speaks to it with ([`call`]),
//! the tokens the control side issues for its principals ([`issue`],
//! [`revoke`], [`sweep`]) and the control `up`, `down` and the supervisor start,
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
//! `command-digest.key` (mode 0600, [`digest_key`]) is the key the
//! executors digest their commands' words with; it is read by them alone
//! and, like a token's value, never written anywhere else.
//! `steps/<sha256 of the telemetry actor id>.json` ([`steps_path`]) is
//! what a session wrapper kept of its target's steps to send again,
//! naming that target, which the supervisor's sweep removes once the
//! target ended ([`kept_steps`]).
//!
//! [Queue service]: ../../docs/design/queue-service.md

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
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

use super::dialogue::DialogueQueue;
use super::sqlite::SqliteQueue;
use crate::application::actor_executor::ExecutorEvents;
use crate::application::commands::DenialLog;
use crate::application::commands::dialogue::{DialogueStore, MarkChange};
use crate::application::queue_reads::QueueRead;
use crate::application::queue_service::{
    QueueService, QueueServiceControl, ServiceAccess, ServiceBackend, ServiceProbe, ServiceQueue,
    TelemetryTokens,
};
use crate::application::session::{AgentStart, ReasonParams, SessionOp, WrapperStore};
use crate::application::{
    EventStore, Exit, Generators, RunNotFound, RunReads, SessionOwners, SessionResourceStore,
    SessionStepStore, Spawned, TaskReads, TelemetrySink,
};
use crate::domain::queue_service::wrapper_actor_id;
use crate::domain::queue_service::{
    API_VERSION, LOCK_FILE, LOG_FILE, MAX_ANSWER_BYTES, MAX_REQUEST_BYTES, OWNER_PID_ENV,
    Principal, SERVICE_DIR, SOCKET_FILE, STATE_FILE, STEPS_DIR, ServiceDenial, ServiceErrorCode,
    ServiceRequest, ServiceResponse, ServiceState, UseCase,
};
use crate::domain::session_step::StepTarget;
use crate::domain::sessions::SessionHook;
use crate::domain::task_replan::Fence;
use crate::domain::telemetry::{
    HeldTelemetry, KeptStepsFile, ReportRecorded, TargetFacts, TelemetryReport,
};
use crate::domain::{
    ActorContext, ActorRole, Answerer, Ask, AskId, EventKind, Finding, FindingId, FindingOutcome,
    FindingStatus, LeaseToken, NewAsk, NewFinding, NewNote, PlannerId, PlannerSession, ProposalId,
    Reason, RequestId, RunEvent, RunId, RunStatus, TaskId, TaskRun,
};

/// The tokens' principals in [`SERVICE_DIR`].
pub const TOKENS_DIR: &str = "tokens";
/// The tokens' values in [`SERVICE_DIR`].
pub const CREDENTIALS_DIR: &str = "credentials";
/// The file in [`SERVICE_DIR`] [`TelemetryLock`] locks.
const TELEMETRY_LOCK_FILE: &str = "telemetry.lock";
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

pub(crate) fn queue_dir_of(db: &Path) -> PathBuf {
    db.parent().unwrap_or(Path::new(".")).to_path_buf()
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Make `dir` (and its parents) with mode 0700.
pub(crate) fn private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("narrow {}", dir.display()))
}

/// Write `bytes` to `path` with mode 0600 through a temporary file and a
/// rename, so a reader never sees half of it.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
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
    /// The lease (the supervisor's token) an executor's telemetry token was
    /// issued under; none for an agent's token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    issued_under: Option<LeaseToken>,
}

/// A token the control side issued: the file its value is in, for the
/// caller to be handed the path of (never the value). Its `Debug` names
/// the file only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedToken {
    pub file: PathBuf,
    pub principal: Principal,
}

/// The file the value of the token `actor_id` holds is written to, under
/// the queue's `service/credentials/`: outside every worktree and run
/// directory. The control side hands its path, never its value.
pub fn credential_path(queue_dir: &Path, actor_id: &str) -> PathBuf {
    service_dir(queue_dir)
        .join(CREDENTIALS_DIR)
        .join(sha256_hex(actor_id.as_bytes()))
}

/// The file a session wrapper keeps the step numbering and unsent steps of
/// the target whose telemetry actor is `actor_id` in, under the queue's
/// `service/steps/`, beside the tokens' files: outside every worktree,
/// run, planner and job directory, and mounted in no container.
pub fn steps_path(queue_dir: &Path, actor_id: &str) -> PathBuf {
    service_dir(queue_dir)
        .join(STEPS_DIR)
        .join(format!("{}.json", sha256_hex(actor_id.as_bytes())))
}

/// The files of the queue at `queue_dir`'s `service/steps/` at `now`: each
/// kept file, and each temporary file with none beside it, with the target
/// it names (`None` when it cannot be read) and its age. None when the
/// directory is not there.
pub fn kept_steps(queue_dir: &Path, now: std::time::SystemTime) -> Result<Vec<KeptStepsFile>> {
    /// What the sweep reads of a kept file: the target it names.
    #[derive(Deserialize)]
    struct Named {
        target: StepTarget,
    }
    let dir = service_dir(queue_dir).join(STEPS_DIR);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
    };
    let mut kept = Vec::new();
    for entry in entries {
        let file = entry
            .with_context(|| format!("read {}", dir.display()))?
            .path();
        let Some(name) = file.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let lone_tmp = name
            .strip_suffix(".tmp")
            .is_some_and(|kept| kept.ends_with(".json") && !dir.join(kept).exists());
        if !(name.ends_with(".json") || lone_tmp) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&file) else {
            continue;
        };
        let age_ms = meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .map_or(0, |age| i64::try_from(age.as_millis()).unwrap_or(i64::MAX));
        let target = meta
            .is_file()
            .then(|| fs::read(&file).ok())
            .flatten()
            .and_then(|bytes| serde_json::from_slice::<Named>(&bytes).ok())
            .map(|named| named.target);
        kept.push(KeptStepsFile {
            file,
            target,
            age_ms,
        });
    }
    kept.sort_by(|a, b| a.file.cmp(&b.file));
    Ok(kept)
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
    write_token(queue_dir, principal, None, now)
}

/// An exclusive `flock` on the queue's [`TELEMETRY_LOCK_FILE`], held while
/// it lives: every issue and revocation of an executor's telemetry token
/// holds it, so those of two supervisors never interleave. A revocation
/// reads the target's file and removes it only once no record of the value
/// it read is left; another supervisor's issue into that file in between
/// would have its new token's file removed.
pub struct TelemetryLock {
    _file: File,
}

impl TelemetryLock {
    /// Take the lock of the queue at `queue_dir`, waiting while another
    /// holds it.
    pub fn take(queue_dir: &Path) -> Result<Self> {
        let dir = service_dir(queue_dir);
        private_dir(&dir)?;
        let path = dir.join(TELEMETRY_LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        file.lock()
            .with_context(|| format!("lock {}", path.display()))?;
        Ok(Self { _file: file })
    }
}

/// [`issue`] for an executor's telemetry principal under `lock`, every
/// token it held revoked first ([`revoke_under`]), with the lease
/// `issued_under` the token was issued under kept in its record, for a
/// revocation of a lost lease to take only its own.
pub fn issue_under(
    lock: &TelemetryLock,
    queue_dir: &Path,
    principal: &Principal,
    issued_under: Option<&LeaseToken>,
    now: i64,
) -> Result<IssuedToken> {
    revoke_under(lock, queue_dir, &principal.actor_id, None)?;
    write_token(queue_dir, principal, issued_under, now)
}

/// Write a token for `principal` issued under `issued_under` ([`issue`]).
fn write_token(
    queue_dir: &Path,
    principal: &Principal,
    issued_under: Option<&LeaseToken>,
    now: i64,
) -> Result<IssuedToken> {
    principal.check().map_err(anyhow::Error::msg)?;
    let key = principal.credential_key();
    revoke(queue_dir, &key)?;
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
        issued_under: issued_under.cloned(),
    };
    // The value first: a token whose record is not written yet is no
    // token, and one whose value is written can always be revoked.
    let file = credential_path(queue_dir, &key);
    write_private(&file, token.as_bytes())?;
    // A record that cannot be written leaves no file of a value that
    // authenticates nothing.
    if let Err(error) = write_private(
        &record_path(queue_dir, &token),
        &serde_json::to_vec(&record)?,
    ) {
        let _ = fs::remove_file(&file);
        return Err(error);
    }
    Ok(IssuedToken {
        file,
        principal: principal.clone(),
    })
}

/// Revoke the token `actor_id` holds (its run ended, its lease was lost,
/// or a resume issues it another); whether one was revoked: a record of
/// it removed, and not only a file whose value authenticated nothing (an
/// issue that failed after its file, or another revocation that came
/// first). The record goes first, which is what makes the token unknown to
/// the service, and the file last: a file that cannot be read or a record
/// that cannot be removed is an error that leaves both, so the token is
/// still found to revoke again. A file left once its record is gone is
/// only logged: the value it holds authenticates nothing, and the next
/// issue replaces it.
pub fn revoke(queue_dir: &Path, actor_id: &str) -> Result<bool> {
    let file = credential_path(queue_dir, actor_id);
    let token = match fs::read_to_string(&file) {
        Ok(token) => token,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("read {}", file.display())),
    };
    let revoked = match fs::remove_file(record_path(queue_dir, token.trim())) {
        Ok(()) => true,
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("remove a token's record"),
    };
    match fs::remove_file(&file) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            warn!(
                actor_id,
                "the revoked token's file {} could not be removed: {error}",
                file.display()
            );
        }
    }
    Ok(revoked)
}

/// The records of the tokens that hold, each with its principal: what
/// [`lookup`] finds a token by. A record gone meanwhile is left out, and so
/// is one that does not parse, with a warning ([`lookup`] cannot read it
/// either, so it authenticates nothing); a directory or a record that
/// cannot be read is an error, as it may still authenticate.
fn records(queue_dir: &Path) -> Result<Vec<(PathBuf, TokenRecord)>> {
    let dir = service_dir(queue_dir).join(TOKENS_DIR);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
    };
    let mut records = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("read {}", dir.display()))?
            .path();
        // A record being written is a hidden temporary file.
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name.starts_with('.') || !name.ends_with(".json") {
            continue;
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        match serde_json::from_slice::<TokenRecord>(&bytes) {
            Ok(record) => records.push((path, record)),
            Err(error) => warn!(
                "the token record {} does not parse: {error}",
                path.display()
            ),
        }
    }
    Ok(records)
}

/// The principals of the tokens that hold (see [`records`]).
pub fn principals(queue_dir: &Path) -> Result<Vec<Principal>> {
    Ok(records(queue_dir)?
        .into_iter()
        .map(|(_, record)| record.principal)
        .collect())
}

/// Revoke the tokens of `actor_id` under `lock`, only those issued under
/// `under` when it is given: every record of them, then the file when the
/// value it holds has no record left. The file is read first, and one that
/// cannot be read is an error that revokes nothing. Once it returns `Ok`,
/// none of those tokens authenticates; an error may leave one that does,
/// still among the [`principals`]. Whether there was one; a token issued
/// under another lease (the one a supervisor that adopted the run issued)
/// is left as it is, and so is its file.
pub fn revoke_under(
    _lock: &TelemetryLock,
    queue_dir: &Path,
    actor_id: &str,
    under: Option<&LeaseToken>,
) -> Result<bool> {
    let file = credential_path(queue_dir, actor_id);
    let value = match fs::read_to_string(&file) {
        Ok(value) => Some(value.trim().to_owned()),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("read {}", file.display())),
    };
    let mut revoked = false;
    for (path, record) in records(queue_dir)? {
        if record.principal.actor_id != actor_id
            || under.is_some_and(|under| record.issued_under.as_ref() != Some(under))
        {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => revoked = true,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove a token's record"),
        }
    }
    if let Some(value) = value
        && !record_path(queue_dir, &value).exists()
    {
        match fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                warn!(
                    actor_id,
                    "the revoked token's file {} could not be removed: {error}",
                    file.display()
                );
            }
        }
    }
    Ok(revoked)
}

/// Revoke every token whose principal `ended` says has ended (a worker's
/// whose run ended, found by the supervisor's sweep): its record, and its
/// value when the actor's file still holds that token (a token issued again
/// since the look, as another supervisor's resume does, is left); the
/// actor ids of those revoked. A record that does not read is left for a
/// person to look at, and one that cannot be removed for the next sweep.
pub fn sweep(queue_dir: &Path, ended: &mut dyn FnMut(&Principal) -> bool) -> Result<Vec<String>> {
    let dir = service_dir(queue_dir).join(TOKENS_DIR);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
    };
    let mut revoked = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<TokenRecord>(&bytes) else {
            continue;
        };
        if !ended(&record.principal) {
            continue;
        }
        let actor_id = record.principal.actor_id;
        if let Err(error) = sweep_one(queue_dir, &path, &actor_id) {
            warn!(error = %format_args!("{error:#}"), "the token of {actor_id} could not be revoked: {error:#}");
            continue;
        }
        if !revoked.contains(&actor_id) {
            revoked.push(actor_id);
        }
    }
    Ok(revoked)
}

/// Remove the token's record at `record`, and the value `actor_id`'s file
/// holds when it is that token's.
fn sweep_one(queue_dir: &Path, record: &Path, actor_id: &str) -> Result<()> {
    let file = credential_path(queue_dir, actor_id);
    match fs::read_to_string(&file) {
        Ok(token) if record_path(queue_dir, token.trim()) == record => {
            match fs::remove_file(&file) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("remove {}", file.display()));
                }
            }
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("read {}", file.display())),
    }
    match fs::remove_file(record) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("remove a token's record"),
    }
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

    fn revoke(&self, db: &Path, actor_id: &str) -> Result<bool> {
        revoke(&queue_dir_of(db), actor_id)
    }

    fn sweep(&self, db: &Path, ended: &mut dyn FnMut(&Principal) -> bool) -> Result<Vec<String>> {
        sweep(&queue_dir_of(db), ended)
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

impl TelemetryTokens for SystemServiceAccess {
    fn issue_telemetry(
        &self,
        db: &Path,
        target: &StepTarget,
        task: Option<TaskId>,
        issued_under: Option<&LeaseToken>,
    ) -> Result<PathBuf> {
        let queue_dir = queue_dir_of(db);
        let principal = Principal::telemetry(target, task);
        let lock = TelemetryLock::take(&queue_dir)?;
        Ok(issue_under(&lock, &queue_dir, &principal, issued_under, unix_now())?.file)
    }

    fn revoke_telemetry(
        &self,
        db: &Path,
        target: &StepTarget,
        issued_under: Option<&LeaseToken>,
    ) -> Result<bool> {
        let queue_dir = queue_dir_of(db);
        let lock = TelemetryLock::take(&queue_dir)?;
        revoke_under(
            &lock,
            &queue_dir,
            &crate::domain::telemetry::actor_id(target),
            issued_under,
        )
    }

    fn telemetry_credential(&self, db: &Path, target: &StepTarget) -> Option<PathBuf> {
        let queue_dir = queue_dir_of(db);
        let file = credential_path(&queue_dir, &crate::domain::telemetry::actor_id(target));
        // Only a file whose value is a token that holds.
        let token = read_token(&file).ok()?;
        matches!(lookup(&queue_dir, &token), Ok(Some(_))).then_some(file)
    }

    fn target_facts(&self, db: &Path, target: &StepTarget) -> Result<TargetFacts> {
        SqliteQueue::open(db)?.telemetry_target(target)
    }

    fn register_telemetry_session(
        &self,
        db: &Path,
        target: &StepTarget,
        session_id: &str,
    ) -> Result<()> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
            });
        SessionOwners::register_session(&SqliteQueue::open(db)?, target, session_id, now_ms)
    }

    fn kept_steps(&self, db: &Path) -> Result<Vec<KeptStepsFile>> {
        kept_steps(&queue_dir_of(db), std::time::SystemTime::now())
    }

    fn forget_steps(&self, db: &Path, file: &Path) -> Result<()> {
        let dir = service_dir(&queue_dir_of(db)).join(STEPS_DIR);
        ensure!(
            file.parent() == Some(dir.as_path()),
            "{} is not a file of {}",
            file.display(),
            dir.display()
        );
        let tmp = file
            .to_str()
            .filter(|name| name.ends_with(".json"))
            .map(|name| PathBuf::from(format!("{name}.tmp")));
        // With the temporary file a write that failed to rename left.
        for file in tmp.iter().map(PathBuf::as_path).chain([file]) {
            match fs::remove_file(file) {
                Err(error) if error.kind() != ErrorKind::NotFound => {
                    return Err(error).with_context(|| format!("remove {}", file.display()));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn held_telemetry(&self, db: &Path) -> Result<Vec<HeldTelemetry>> {
        Ok(records(&queue_dir_of(db))?
            .into_iter()
            .filter(|(_, record)| record.principal.role == ActorRole::ExecutorTelemetry)
            .filter_map(|(_, record)| {
                Some(HeldTelemetry {
                    target: record.principal.target?,
                    issued_under: record.issued_under,
                })
            })
            .collect())
    }
}

/// The sink of an executor's target: the service of the queue at `db` with
/// the target's token, while its file holds one that is issued
/// ([`TelemetryTokens::telemetry_credential`]).
impl crate::application::TelemetrySinks for SystemServiceAccess {
    fn sink(&self, db: &Path, target: &StepTarget) -> Option<Box<dyn TelemetrySink>> {
        ServiceTelemetrySink::of_target(db, target).map(|sink| Box::new(sink) as _)
    }

    fn digest_key(&self, db: &Path) -> Option<crate::domain::command_shape::DigestKey> {
        digest_key(&queue_dir_of(db))
            .map_err(|error| {
                warn!(error = %format_args!("{error:#}"), "the key of command digests could not be read or made; steps keep no command's shape: {error:#}");
            })
            .ok()
    }

    /// The service's directory must be there already (made with mode
    /// 0700 by the service), so `steps/` inherits its privacy.
    fn steps_file(&self, db: &Path, target: &StepTarget) -> Option<PathBuf> {
        let queue_dir = queue_dir_of(db);
        let file = steps_path(&queue_dir, &crate::domain::telemetry::actor_id(target));
        let dir = file.parent()?;
        if !service_dir(&queue_dir).is_dir() {
            return None;
        }
        match fs::DirBuilder::new().mode(0o700).create(dir) {
            Err(error) if error.kind() != ErrorKind::AlreadyExists => {
                warn!(
                    "{} could not be made; the wrapper keeps its steps in itself: {error:#}",
                    dir.display()
                );
                None
            }
            _ => Some(file),
        }
    }
}

/// The file of the key of command digests of the queue at `queue_dir`
/// (ADR-t1673-1 decision 10): in the service's private directory, never in
/// the queue's database.
pub fn digest_key_path(queue_dir: &Path) -> PathBuf {
    service_dir(queue_dir).join("command-digest.key")
}

/// The key of command digests of the queue at `queue_dir`: its file's
/// bytes, or, when it has none, [`DIGEST_KEY_BYTES`] random bytes written
/// with mode 0600 and linked into place only if no other process made one
/// first, whose key is then read. A file of another size is an error, never
/// replaced: its digests would no longer match.
///
/// [`DIGEST_KEY_BYTES`]: crate::domain::command_shape::DIGEST_KEY_BYTES
pub fn digest_key(queue_dir: &Path) -> Result<crate::domain::command_shape::DigestKey> {
    use crate::domain::command_shape::{DIGEST_KEY_BYTES, DigestKey};
    let path = digest_key_path(queue_dir);
    let read = |path: &Path| -> Result<DigestKey> {
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        DigestKey::from_bytes(&bytes).with_context(|| {
            format!(
                "{} does not hold a key of {DIGEST_KEY_BYTES} bytes",
                path.display()
            )
        })
    };
    match fs::symlink_metadata(&path) {
        Ok(_) => return read(&path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    }
    let dir = service_dir(queue_dir);
    private_dir(&dir)?;
    let mut bytes = [0u8; DIGEST_KEY_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("draw the key's random bytes: {error}"))?;
    let temporary = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        match fs::hard_link(&temporary, &path) {
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => Err(error),
            _ => Ok(()),
        }
    })();
    let _ = fs::remove_file(&temporary);
    written.with_context(|| format!("write {}", path.display()))?;
    read(&path)
}

/// How long an executor waits for the service to answer a report: the
/// executor's turn, landing or pass waits on it, and what is not answered
/// is sent again later.
pub const REPORT_TIMEOUT: Duration = Duration::from_secs(5);

/// An executor's [`TelemetrySink`] through the queue service's client mode
/// (ADR-t1233-1 decision 7): `telemetry_report` on `client`'s route, as the
/// telemetry principal of the token in its credential file. A refusal or a
/// service that does not answer is an error, and nothing is kept here: the
/// executor sends its steps again.
#[derive(Debug, Clone)]
pub struct ServiceTelemetrySink {
    pub client: Client,
}

impl ServiceTelemetrySink {
    /// The sink of the service of the queue at `db` with the telemetry
    /// token of `target`, never an agent's: `None` while it holds none.
    pub fn of_target(db: &Path, target: &StepTarget) -> Option<Self> {
        let credential = SystemServiceAccess.telemetry_credential(db, target)?;
        Some(Self::new(socket_path(&queue_dir_of(db)), credential))
    }

    /// The sink of the service on `socket`, with the token in `credential`.
    pub fn new(socket: PathBuf, credential: PathBuf) -> Self {
        Self {
            client: Client {
                route: ClientRoute::Socket(socket),
                credential: Some(credential),
            },
        }
    }
}

impl TelemetrySink for ServiceTelemetrySink {
    fn report(&self, report: &TelemetryReport) -> Result<ReportRecorded> {
        // A report past its bounds would not be read: it is not sent.
        crate::domain::telemetry::check(report).map_err(anyhow::Error::msg)?;
        let result = self.client.call_within(
            UseCase::TelemetryReport,
            serde_json::to_value(report)?,
            REPORT_TIMEOUT,
        )?;
        serde_json::from_value(result).context("the queue service's answer to telemetry_report")
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

/// An answer longer than [`MAX_ANSWER_BYTES`], which a client stops
/// reading at: the service answered, and [`Client::call`] refuses it as
/// `response_too_large`, not `unreachable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnswerPastLimit;

impl std::fmt::Display for AnswerPastLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the answer is longer than {MAX_ANSWER_BYTES} bytes")
    }
}

impl std::error::Error for AnswerPastLimit {}

/// The refusal of a client whose call to `use_case` got no answer it
/// reads: `response_too_large` for an [`AnswerPastLimit`] (the service
/// answered), `unreachable` with `said` (where to look) for any other.
fn no_answer(error: &anyhow::Error, use_case: UseCase, said: &str) -> ClientFailure {
    if error.downcast_ref::<AnswerPastLimit>().is_some() {
        return ClientFailure::new(
            ServiceErrorCode::ResponseTooLarge.as_str(),
            crate::domain::queue_service::too_large_message(Some(use_case), None),
        );
    }
    ClientFailure::new(
        "unreachable",
        format!("{error:#}; a client-mode dagq does not open the queue itself ({said})"),
    )
}

/// No connection to the service at `socket` took a request ([`call`]):
/// nothing of it was sent.
#[derive(Debug)]
pub struct NotConnected {
    socket: PathBuf,
    source: std::io::Error,
}

impl std::fmt::Display for NotConnected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the queue service at {} is unreachable",
            self.socket.display()
        )
    }
}

impl std::error::Error for NotConnected {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Send `request` to the service at `socket` and read its answer; an error
/// when nothing answers (the caller does not fall back to the DB,
/// ADR-t1233-1 decision 7), or [`AnswerPastLimit`].
pub fn call(socket: &Path, request: &ServiceRequest, timeout: Duration) -> Result<ServiceResponse> {
    let mut stream = UnixStream::connect(socket).map_err(|source| NotConnected {
        socket: socket.to_path_buf(),
        source,
    })?;
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
        .take(MAX_ANSWER_BYTES as u64 + 1)
        .read_to_end(&mut answer)
        .context("read the queue service's answer")?;
    if answer.len() > MAX_ANSWER_BYTES {
        return Err(AnswerPastLimit.into());
    }
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
/// `api_version_mismatch`), the queue broker's in front of it
/// (`unauthenticated`, `forbidden_destination`, `bad_request`,
/// `unreachable`), or the client's (`unreachable`: nothing answers on the
/// socket or at the broker; `response_too_large`: the answer, which the
/// service gave, is longer than [`MAX_ANSWER_BYTES`]; `no_credential`: the
/// token's file does not read; `control_side_command`: the command is one of the control
/// side's (`doctor`, `service status`, `planners`), which a person or the
/// inbox runs on the host; `no_use_case`: any other command that is none
/// of the service's;
/// `queue_named`: the command names a queue to open; `client_misconfigured`:
/// the environment names both a socket and a broker, a broker URL that
/// does not read, or, with a broker, a run that holds a control character).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientFailure {
    pub code: String,
    pub message: String,
    /// What the service's policy refused, with its `authorization_denied`:
    /// the command line prints it as a local refusal's `denied`.
    pub denied: Option<ServiceDenial>,
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
            denied: None,
        }
    }
}

/// Where a client-mode `dagq` sends its requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientRoute {
    /// The queue service's unix socket on the host
    /// ([`SOCKET_ENV`](crate::domain::queue_service::SOCKET_ENV)): a
    /// worker's or a job's on the host.
    Socket(PathBuf),
    /// The queue broker at `address` (`<host>:<port>`,
    /// [`URL_ENV`](crate::domain::queue_service::URL_ENV)), for `run`
    /// ([`RUN_ID_ENV`](crate::domain::actor::RUN_ID_ENV)): a run's in its
    /// container, through the forwarder and the relay.
    Broker {
        address: String,
        run: Option<String>,
    },
}

/// Where a client-mode `dagq` sends its commands (ADR-t1233-1 decision 7,
/// ADR-t2114-3 decision 3): the route and the token's file the control
/// side put in its environment
/// ([`CREDENTIAL_FILE_ENV`](crate::domain::queue_service::CREDENTIAL_FILE_ENV)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub route: ClientRoute,
    /// `None` when the environment names none: the service (or the broker)
    /// then refuses the call as `unauthenticated`.
    pub credential: Option<PathBuf>,
}

impl Client {
    /// The client `env` names: `Ok(None)` with neither a socket nor a
    /// broker, when `dagq` opens the queue itself; a `client_misconfigured`
    /// failure with both, with a broker URL that does not read, or with a
    /// broker and a run that holds a control character (it would go in a
    /// header), when `dagq` refuses every command and opens no queue (fail
    /// closed).
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, ClientFailure> {
        use crate::domain::actor::RUN_ID_ENV;
        use crate::domain::queue_service::{
            CREDENTIAL_FILE_ENV, SOCKET_ENV, URL_ENV, broker_address,
        };
        let named = |name: &str| env(name).filter(|value| !value.trim().is_empty());
        let route = match (named(SOCKET_ENV), named(URL_ENV)) {
            (None, None) => return Ok(None),
            (Some(_), Some(_)) => {
                return Err(ClientFailure::new(
                    "client_misconfigured",
                    format!(
                        "both {SOCKET_ENV} and {URL_ENV} are set, and a client-mode dagq does not \
                         choose between them: it runs no command and opens no queue"
                    ),
                ));
            }
            (Some(socket), None) => ClientRoute::Socket(PathBuf::from(socket)),
            (None, Some(url)) => ClientRoute::Broker {
                address: broker_address(&url)
                    .map_err(|why| ClientFailure::new("client_misconfigured", why))?,
                // The run goes in a header: a value with a control
                // character is no run.
                run: match named(RUN_ID_ENV) {
                    Some(run) if run.chars().any(char::is_control) => {
                        return Err(ClientFailure::new(
                            "client_misconfigured",
                            format!("{RUN_ID_ENV} holds a control character"),
                        ));
                    }
                    run => run,
                },
            },
        };
        Ok(Some(Self {
            route,
            credential: named(CREDENTIAL_FILE_ENV).map(PathBuf::from),
        }))
    }

    /// `use_case` with `params` done by the service as the principal of
    /// the token: its result, or a [`ClientFailure`]. A service (or a
    /// broker) that does not answer is a failure; the client never opens
    /// the queue instead (fail closed).
    pub fn call(&self, use_case: UseCase, params: Value) -> Result<Value> {
        self.call_within(use_case, params, CLIENT_TIMEOUT)
    }

    /// [`Self::call`] waiting at most `timeout` for a service on a socket
    /// to answer.
    pub fn call_within(
        &self,
        use_case: UseCase,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let token = match &self.credential {
            Some(file) => Some(
                read_token(file)
                    .map_err(|error| ClientFailure::new("no_credential", format!("{error:#}")))?,
            ),
            None => None,
        };
        let unreachable = |error: anyhow::Error, said: &str| no_answer(&error, use_case, said);
        let response = match &self.route {
            ClientRoute::Socket(socket) => {
                let request = ServiceRequest {
                    api_version: API_VERSION,
                    token,
                    use_case,
                    params,
                };
                call(socket, &request, timeout).map_err(|error| {
                    unreachable(
                        error,
                        "dagq service status, from the control side, says why",
                    )
                })?
            }
            ClientRoute::Broker { address, run } => {
                // The token goes as the `Bearer` only: the broker puts the
                // one it checked in the request it passes on.
                let request = ServiceRequest {
                    api_version: API_VERSION,
                    token: None,
                    use_case,
                    params,
                };
                call_broker(address, token.as_deref(), run.as_deref(), &request).map_err(
                    |error| {
                        unreachable(
                            error,
                            "the queue broker's state on the control side says why",
                        )
                    },
                )?
            }
        };
        answer_of(response)
    }

    /// Whether the queue service answers `hello` within [`PROBE_TIMEOUT`],
    /// the short wait of a look at it: what `locate` adds to its answer
    /// (see [`reach_of`]). It never fails, and opens no queue whatever it
    /// finds (fail closed).
    pub fn reach(&self) -> Value {
        let request = ServiceRequest {
            api_version: API_VERSION,
            token: None,
            use_case: UseCase::Hello,
            params: Value::Null,
        };
        let response = match &self.route {
            ClientRoute::Socket(socket) => call(socket, &request, PROBE_TIMEOUT),
            ClientRoute::Broker { address, run } => {
                // The broker lets through only a request with a token, and
                // records the refusal of one without: with no token to
                // read, it is not asked.
                let token = match &self.credential {
                    Some(file) => read_token(file),
                    None => Err(anyhow::anyhow!("no token's file is named")),
                };
                let token = match token {
                    Ok(token) => token,
                    Err(error) => {
                        return json!({
                            "reachable": false,
                            "reason": format!("no_credential: {error:#}"),
                        });
                    }
                };
                super::queue_broker::call(
                    address,
                    Some(&token),
                    run.as_deref(),
                    &request,
                    PROBE_TIMEOUT,
                )
                .and_then(|(status, body)| {
                    serde_json::from_slice(&body).with_context(|| {
                        format!("the queue broker's answer (status {status}) does not read")
                    })
                })
            }
        };
        reach_of(response)
    }
}

/// What a client takes from the service's `response`: its result, or a
/// [`ClientFailure`]. The versions of every answer are judged first
/// (ADR-t3102-2 decision 4): a service whose range does not hold this
/// client's [`API_VERSION`] is not used, whatever it answered, whichever
/// binary the client runs from (a run's `runner` copy, a development
/// build).
fn answer_of(response: ServiceResponse) -> Result<Value> {
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
    if let Some(error) = response.error {
        return Err(ClientFailure {
            denied: error
                .denied
                .filter(|_| error.code == ServiceErrorCode::AuthorizationDenied)
                .map(|denied| *denied),
            ..ClientFailure::new(error.code.as_str(), error.message)
        }
        .into());
    }
    match (response.ok, response.result) {
        (true, Some(result)) => Ok(result),
        _ => Err(ClientFailure::new("failed", "the queue service answered nothing").into()),
    }
}

/// What a client's `hello` found: `reachable: true` with the service's
/// `api_version` and `min_api_version` and whether this client's version
/// is among them (`api_compatible`; a service that does not take it
/// answers `api_version_mismatch` with its versions), or `reachable:
/// false` with the `reason` (nothing answers, or the broker refused).
fn reach_of(response: Result<ServiceResponse>) -> Value {
    let response = match response {
        Ok(response) => response,
        Err(error) => return json!({"reachable": false, "reason": format!("{error:#}")}),
    };
    match &response.error {
        Some(error) if error.code != ServiceErrorCode::ApiVersionMismatch => json!({
            "reachable": false,
            "reason": format!("{}: {}", error.code.as_str(), error.message),
        }),
        _ => json!({
            "reachable": true,
            "api_version": response.api_version,
            "min_api_version": response.min_api_version,
            "api_compatible": crate::domain::queue_service::understands(&response),
        }),
    }
}

/// `request` through the broker at `address` in the shape
/// [`crate::domain::queue_broker`] gives: the service's answer, or the
/// broker's refusal, which has the same shape whatever its HTTP status.
fn call_broker(
    address: &str,
    token: Option<&str>,
    run: Option<&str>,
    request: &ServiceRequest,
) -> Result<ServiceResponse> {
    let (status, body) = super::queue_broker::call(address, token, run, request, CLIENT_TIMEOUT)?;
    serde_json::from_slice(&body)
        .with_context(|| format!("the queue broker's answer (status {status}) does not read"))
}

// --- A session wrapper's store (ADR-t3102-1 decision 4) ----------------------

/// A session wrapper's [`WrapperStore`] through the queue service: each
/// method is one [`SessionOp`] the service does as the wrapper's principal,
/// whose token the control side issued under the queue's `service/`
/// ([`credential_path`] of [`wrapper_actor_id`]) before it started the
/// wrapper. The wrapper's process opens no queue: a service that does not
/// answer, or refuses, is the method's error (fail closed).
#[derive(Debug, Clone)]
pub struct ServiceWrapperStore {
    pub client: Client,
    /// The run or the planner the wrapper runs the session of.
    pub target: StepTarget,
}

impl ServiceWrapperStore {
    /// The store of the wrapper of `target` of the queue at `db`: the
    /// queue's socket and the file of the token issued for the wrapper.
    pub fn of_queue(db: &Path, target: StepTarget) -> Self {
        let queue_dir = queue_dir_of(db);
        Self {
            client: Client {
                route: ClientRoute::Socket(socket_path(&queue_dir)),
                credential: Some(credential_path(&queue_dir, &wrapper_actor_id(&target))),
            },
            target,
        }
    }

    /// `op`'s answer, the method's value.
    fn call(&self, op: &SessionOp) -> Result<Value> {
        let mut answer = self
            .client
            .call(op.use_case(), serde_json::to_value(op)?)
            .with_context(|| {
                let params = serde_json::to_value(op).unwrap_or_default();
                format!(
                    "the queue service did not do {} for the session wrapper of {} {}, which \
                     opens no queue itself",
                    params["op"].as_str().unwrap_or("the operation"),
                    self.target.kind.as_str(),
                    self.target.id
                )
            })?;
        Ok(answer
            .get_mut("value")
            .map(Value::take)
            .unwrap_or(Value::Null))
    }

    /// `op`'s answer read as `T`.
    fn read<T: serde::de::DeserializeOwned>(&self, op: &SessionOp) -> Result<T> {
        serde_json::from_value(self.call(op)?)
            .context("the queue service's answer to the session wrapper does not read")
    }

    fn done(&self, op: &SessionOp) -> Result<()> {
        self.call(op).map(drop)
    }
}

impl WrapperStore for ServiceWrapperStore {
    fn run(&self, id: &RunId) -> Result<TaskRun> {
        self.read(&SessionOp::Run { run: id.clone() })
    }
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        self.read(&SessionOp::RunEvents { run: id.clone() })
    }
    fn turn_fence(&self, task: TaskId) -> Result<Option<Fence>> {
        self.read(&SessionOp::TurnFence { task })
    }
    fn planner(&self, id: PlannerId) -> Result<PlannerSession> {
        self.read(&SessionOp::Planner { planner: id })
    }
    fn planner_turn_events(&self, id: PlannerId) -> Result<Vec<RunEvent>> {
        self.read(&SessionOp::PlannerTurnEvents { planner: id })
    }
    fn repository_binding(&self) -> Result<Option<String>> {
        self.read(&SessionOp::RepositoryBinding)
    }
    fn register_wrapper(&mut self, id: &RunId, token: &LeaseToken, pid: u32) -> Result<()> {
        self.done(&SessionOp::RegisterWrapper {
            run: id.clone(),
            lease: token.clone(),
            pid,
            resume: false,
        })
    }
    fn register_resume_wrapper(&mut self, id: &RunId, token: &LeaseToken, pid: u32) -> Result<()> {
        self.done(&SessionOp::RegisterWrapper {
            run: id.clone(),
            lease: token.clone(),
            pid,
            resume: true,
        })
    }
    fn register_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()> {
        self.done(&SessionOp::RegisterAgent {
            run: id.clone(),
            wrapper_pid,
            agent_pid,
            start: AgentStart::First,
        })
    }
    fn register_resume_agent(
        &mut self,
        id: &RunId,
        wrapper_pid: u32,
        agent_pid: u32,
    ) -> Result<()> {
        self.done(&SessionOp::RegisterAgent {
            run: id.clone(),
            wrapper_pid,
            agent_pid,
            start: AgentStart::Resume,
        })
    }
    fn register_turn_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()> {
        self.done(&SessionOp::RegisterAgent {
            run: id.clone(),
            wrapper_pid,
            agent_pid,
            start: AgentStart::Turn,
        })
    }
    fn heartbeat_wrapper(&self, id: &RunId, pid: u32) -> Result<()> {
        self.done(&SessionOp::HeartbeatWrapper {
            run: id.clone(),
            pid,
        })
    }
    fn wrapper_exited(&mut self, id: &RunId, pid: u32, exit_code: i32) -> Result<()> {
        self.done(&SessionOp::WrapperExited {
            run: id.clone(),
            pid,
            exit_code,
        })
    }
    fn record_runtime_error(&mut self, id: &RunId, message: &str, reason: &Reason) -> Result<()> {
        self.done(&SessionOp::RuntimeError {
            run: id.clone(),
            message: message.to_owned(),
            reason: ReasonParams {
                code: reason.code,
                detail: reason.detail.clone(),
            },
        })
    }
    fn register_planner_wrapper(&self, id: PlannerId, pid: u32) -> Result<()> {
        self.done(&SessionOp::RegisterPlannerWrapper { planner: id, pid })
    }
    fn register_planner_agent(&self, id: PlannerId, wrapper_pid: u32, agent: u32) -> Result<()> {
        self.done(&SessionOp::RegisterPlannerAgent {
            planner: id,
            wrapper_pid,
            agent_pid: agent,
        })
    }
    fn heartbeat_planner(&self, id: PlannerId, wrapper_pid: u32) -> Result<()> {
        self.done(&SessionOp::HeartbeatPlanner {
            planner: id,
            wrapper_pid,
        })
    }
    fn planner_exited(&self, id: PlannerId, wrapper_pid: u32, exit_code: i32) -> Result<()> {
        self.done(&SessionOp::PlannerExited {
            planner: id,
            wrapper_pid,
            exit_code,
        })
    }
    fn record_runtime_event(&self, id: &RunId, kind: EventKind, payload: Value) -> Result<()> {
        self.done(&SessionOp::RuntimeEvent {
            run: id.clone(),
            kind: kind.as_str().to_owned(),
            payload,
        })
    }
    /// The service records a planner's wrapper's events of the queue only
    /// with its planner named (`planner_id`), which one that does not name
    /// it (an agent found again at its start) is given here.
    fn record_queue_event(&self, kind: EventKind, mut payload: Value) -> Result<()> {
        if let (Value::Object(fields), Ok(planner)) = (&mut payload, self.target.id.parse::<i64>())
        {
            fields
                .entry("planner_id")
                .or_insert_with(|| Value::from(planner));
        }
        self.done(&SessionOp::QueueEvent {
            owner: self.target.clone(),
            kind: kind.as_str().to_owned(),
            payload,
        })
    }
    fn record_session_hook(&self, hook: &SessionHook) -> Result<Value> {
        self.call(&SessionOp::Hook { hook: hook.clone() })
    }
    fn as_events(&self) -> &dyn ExecutorEvents {
        self
    }
}

impl ExecutorEvents for ServiceWrapperStore {
    fn record_agent_event(
        &self,
        run: Option<&RunId>,
        kind: EventKind,
        payload: Value,
    ) -> Result<()> {
        match run {
            Some(run) => self.record_runtime_event(run, kind, payload),
            None => self.record_queue_event(kind, payload),
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

/// An exclusive `flock` on the service's (or the queue broker's) lock
/// file, held while it lives.
pub(crate) struct Lock {
    _file: File,
}

impl Lock {
    /// Take the lock without waiting; `None` while another holds it.
    pub(crate) fn try_take(
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
    lock_file_held(&service_dir(queue_dir).join(LOCK_FILE))
}

/// Whether a process holds the lock file at `path` now, looked at as
/// [`lock_held`] looks.
pub(crate) fn lock_file_held(path: &Path) -> bool {
    match Lock::try_take(path, false, libc::LOCK_SH) {
        Ok(Some(_)) | Err(_) => false,
        Ok(None) => true,
    }
}

// --- The service ---------------------------------------------------------------

/// How the service answers a read use case: as the command line reads it
/// (`compose::read_queue`), given by the composition root. It takes the
/// request's queue and its DB. No read calls cmux (ADR-t1433-1).
pub type ServiceReads =
    Arc<dyn Fn(&mut SqliteQueue, &Path, &QueueRead) -> Result<Value> + Send + Sync>;

/// How the service shows a task without `--full`: as the command line
/// shows it (`compose::service_detail`), given by the composition root, so
/// the infrastructure does not shape the output itself (architecture L6).
pub type ServiceDetail = fn(&crate::domain::TaskDetail, usize) -> Value;

/// How `dagq service serve` runs.
#[derive(Clone)]
pub struct ServeOptions {
    pub db: PathBuf,
    pub generators: Generators,
    pub reads: ServiceReads,
    pub detail: ServiceDetail,
    /// Set by SIGINT or SIGTERM: the service stops accepting and exits.
    pub stop: Arc<AtomicBool>,
    /// How often the loop looks for a connection or a stop.
    pub poll: Duration,
    /// The process the service stops with ([`OWNER_PID_ENV`]): a test's.
    /// None in production, where the service runs until it is stopped or
    /// its queue goes.
    pub owner: Option<u32>,
}

/// The owner [`OWNER_PID_ENV`] names in `env`, if it names a pid.
pub fn owner_from_env(env: impl Fn(&str) -> Option<String>) -> Option<u32> {
    env(OWNER_PID_ENV).and_then(|pid| pid.trim().parse().ok())
}

/// Whether the service's owner is gone: never without one. A pid another
/// user's process holds is there, and so is a zombie until its parent
/// reaps it.
pub(crate) fn owner_gone(owner: Option<u32>) -> bool {
    let Some(pid) = owner.and_then(|pid| libc::pid_t::try_from(pid).ok()) else {
        return false;
    };
    // SAFETY: kill(2) with signal 0 only checks the pid; it takes no pointer.
    let found = unsafe { libc::kill(pid, 0) } == 0;
    !found && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Run the queue's service until `stop` is set, its queue goes or its
/// owner does (`owner`, a test's process): take
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
        generators: options.generators.clone(),
        reads: options.reads.clone(),
        detail: options.detail,
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
            if owner_gone(options.owner) {
                break "owner_gone";
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
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                wait_for_connection(&listener, options.poll);
            }
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
    // Why, in the line's text (the service's log has the message only),
    // said while the lock is held: a look that finds the service stopped
    // (its lock free) finds why in its log too.
    info!(pid, outcome, "the queue service stopped: {outcome}");
    drop(lock);
    Ok(json!({"outcome": outcome, "pid": pid, "served": served}))
}

/// Wait up to `poll` for a connection to `listener`, returning as soon as
/// one comes: a supervisor sends many requests a pass, each on a
/// connection of its own, which a fixed sleep between looks would each
/// hold back.
fn wait_for_connection(listener: &UnixListener, poll: Duration) {
    let mut fd = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = libc::c_int::try_from(poll.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: poll(2) on one pollfd that lives through the call.
    unsafe { libc::poll(&raw mut fd, 1, millis) };
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
    let mut use_case = None;
    let response = if line.len() > MAX_REQUEST_BYTES {
        ServiceResponse::failure(ServiceErrorCode::BadRequest, "the request is too long")
    } else {
        match serde_json::from_slice::<ServiceRequest>(&line) {
            Ok(request) => {
                use_case = Some(request.use_case);
                QueueService {
                    backend,
                    build: crate::VERSION,
                    pid,
                }
                .handle(&request)
            }
            Err(error) => ServiceResponse::failure(
                ServiceErrorCode::BadRequest,
                format!("the request does not read: {error}"),
            ),
        }
    };
    (&stream).write_all(&answer_line(&response, use_case)?)?;
    Ok(())
}

/// `response` to `use_case` as the line the service writes, or, when that
/// is longer than [`MAX_ANSWER_BYTES`], its `response_too_large` (a client
/// would read only a part of it).
fn answer_line(response: &ServiceResponse, use_case: Option<UseCase>) -> Result<Vec<u8>> {
    let mut out = serde_json::to_vec(response)?;
    out.push(b'\n');
    if out.len() > MAX_ANSWER_BYTES {
        warn!(
            bytes = out.len(),
            "the queue service answered a request past its answer's limit"
        );
        out = serde_json::to_vec(&ServiceResponse::too_large(use_case, out.len()))?;
        out.push(b'\n');
    }
    Ok(out)
}

/// The queue the service's use cases run on: a fresh connection to the
/// DB per request, written as the request's principal.
pub struct SqliteServiceBackend {
    pub db: PathBuf,
    pub queue_dir: PathBuf,
    pub generators: Generators,
    pub reads: ServiceReads,
    pub detail: ServiceDetail,
}

impl SqliteServiceBackend {
    /// The service itself, as the records name it: a part of the control
    /// side (ADR-t1233-1 decision 1), with role `supervisor` and actor id
    /// `supervisor:queue-service:<pid>`.
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
            reads: self.reads.clone(),
            detail: self.detail,
        }))
    }

    fn record_unauthenticated(&self, payload: Value) -> Result<()> {
        let queue = SqliteQueue::open(&self.db)?
            .with_generators(self.generators.clone())
            .with_actor(Self::own_actor());
        EventStore::record_queue_event(&queue, EventKind::QueueServiceUnauthenticated, payload)
            .map(drop)
    }

    fn now_ms(&self) -> i64 {
        self.generators
            .clock
            .system_time()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|since| i64::try_from(since.as_millis()).ok())
            .unwrap_or_default()
    }

    fn db(&self) -> &Path {
        &self.db
    }

    fn access(&self) -> &dyn ServiceAccess {
        &SystemServiceAccess
    }

    fn telemetry(&self) -> &dyn TelemetryTokens {
        &SystemServiceAccess
    }

    fn principals(&self) -> Result<Vec<Principal>> {
        principals(&self.queue_dir)
    }
}

// --- The supervisor's calls (goal 182's stage (5)) ---------------------------

/// How long the supervisor waits for the service's answer to one of its
/// operations: a write may wait for the queue's lock first.
pub const SUPERVISOR_TIMEOUT: Duration = Duration::from_secs(120);

/// The supervisor principal's token, once it signed in: its value, read
/// from the file the issue wrote, and the key it is kept under
/// ([`Principal::credential_key`]), for the revocation.
#[derive(Default)]
struct SignedIn(std::sync::Mutex<Option<(String, String)>>);

impl SignedIn {
    fn sign_in(&self, queue_dir: &Path, registration: &LeaseToken, pid: u32) -> Result<()> {
        let principal = Principal::supervisor(registration, pid);
        let issued = issue(queue_dir, &principal, unix_now())?;
        let value = read_token(&issued.file)?;
        *self.0.lock().expect("the sign-in lock") = Some((value, principal.credential_key()));
        Ok(())
    }

    fn sign_out(&self, queue_dir: &Path) {
        if let Some((_, key)) = self.0.lock().expect("the sign-in lock").take()
            && let Err(error) = revoke(queue_dir, &key)
        {
            warn!("the supervisor's queue service token could not be revoked: {error:#}");
        }
    }

    /// The token's value; `None` before the sign-in, when only `hello` is
    /// answered.
    fn token(&self) -> Option<String> {
        self.0
            .lock()
            .expect("the sign-in lock")
            .as_ref()
            .map(|(value, _)| value.clone())
    }
}

/// The supervisor's calls on its queue service's socket
/// (ADR-t1233-1 decision 5): nothing answering is
/// [`crate::application::supervisor_service::ServiceUnreachable`], and the
/// queue is never opened instead.
pub struct SocketCaller {
    queue_dir: PathBuf,
    socket: PathBuf,
    timeout: Duration,
    signed_in: SignedIn,
}

impl SocketCaller {
    /// The caller of the service of the queue at `db`.
    pub fn new(db: &Path) -> Self {
        let queue_dir = queue_dir_of(db);
        Self {
            socket: socket_path(&queue_dir),
            queue_dir,
            timeout: SUPERVISOR_TIMEOUT,
            signed_in: SignedIn::default(),
        }
    }
}

/// The answer `response` of a supervisor's request as its result.
fn supervisor_answer(response: ServiceResponse) -> Result<Value> {
    if !crate::domain::queue_service::understands(&response) {
        bail!(
            "the queue service answers API versions {} to {}, not {API_VERSION}",
            response.min_api_version,
            response.api_version
        );
    }
    match (response.ok, response.result, response.error) {
        (true, result, _) => Ok(result.unwrap_or(Value::Null)),
        (_, _, Some(error)) => Err(crate::application::supervisor_service::service_error(
            &error,
        )),
        _ => bail!("the queue service answered with neither a result nor an error"),
    }
}

impl crate::application::supervisor_service::ServiceCaller for SocketCaller {
    fn call(&self, use_case: UseCase, params: Value) -> Result<Value> {
        let request = ServiceRequest {
            api_version: API_VERSION,
            token: self.signed_in.token(),
            use_case,
            params,
        };
        let response = call(&self.socket, &request, self.timeout).map_err(|error| {
            use crate::application::supervisor_service::ServiceUnreachable;
            if error.downcast_ref::<AnswerPastLimit>().is_some() {
                error
            } else if error.downcast_ref::<NotConnected>().is_some() {
                // Nothing was sent: the service did not do it.
                anyhow::Error::new(ServiceUnreachable::not_done(format!("{error:#}")))
            } else {
                // Written, and its answer not read: it may have been done.
                anyhow::Error::new(ServiceUnreachable::maybe_done(format!("{error:#}")))
            }
        })?;
        supervisor_answer(response)
    }

    fn sign_in(&self, registration: &LeaseToken, pid: u32) -> Result<()> {
        self.signed_in.sign_in(&self.queue_dir, registration, pid)
    }

    fn sign_out(&self) {
        self.signed_in.sign_out(&self.queue_dir);
    }
}

/// The supervisor's calls answered in this process by the service's use
/// cases on a backend of its own (a test's supervisor, which keeps no
/// service): every request is made and read back as the socket's would
/// be, token and all.
pub struct InProcessCaller {
    queue_dir: PathBuf,
    backend: SqliteServiceBackend,
    signed_in: SignedIn,
}

impl InProcessCaller {
    /// The caller of the queue at `db`, its records made with `generators`.
    pub fn new(
        db: &Path,
        generators: Generators,
        reads: ServiceReads,
        detail: ServiceDetail,
    ) -> Self {
        let queue_dir = queue_dir_of(db);
        Self {
            backend: SqliteServiceBackend {
                db: db.to_path_buf(),
                queue_dir: queue_dir.clone(),
                generators,
                reads,
                detail,
            },
            queue_dir,
            signed_in: SignedIn::default(),
        }
    }
}

impl crate::application::supervisor_service::ServiceCaller for InProcessCaller {
    fn call(&self, use_case: UseCase, params: Value) -> Result<Value> {
        let request = ServiceRequest {
            api_version: API_VERSION,
            token: self.signed_in.token(),
            use_case,
            params,
        };
        // Through the request's text and the answer's, as on the socket.
        let request: ServiceRequest = serde_json::from_slice(&serde_json::to_vec(&request)?)?;
        let response = QueueService {
            backend: &self.backend,
            build: crate::VERSION,
            pid: std::process::id(),
        }
        .handle(&request);
        supervisor_answer(serde_json::from_slice(&serde_json::to_vec(&response)?)?)
    }

    fn sign_in(&self, registration: &LeaseToken, pid: u32) -> Result<()> {
        self.signed_in.sign_in(&self.queue_dir, registration, pid)
    }

    fn sign_out(&self) {
        self.signed_in.sign_out(&self.queue_dir);
    }
}

/// One request's queue.
struct ServiceSqlite {
    queue: SqliteQueue,
    db: PathBuf,
    reads: ServiceReads,
    detail: ServiceDetail,
}

impl ServiceSqlite {
    fn dialogue(&mut self) -> DialogueQueue<'_> {
        DialogueQueue {
            queue: &mut self.queue,
        }
    }
}

impl DenialLog for ServiceSqlite {
    fn record_denial(&self, payload: Value) -> Result<()> {
        EventStore::record_queue_event(&self.queue, EventKind::AuthorizationDenied, payload)
            .map(drop)
    }
}

impl crate::application::agent_eval::EvalStore for ServiceSqlite {
    fn record_eval_event(&self, kind: EventKind, payload: Value) -> Result<crate::domain::EventId> {
        crate::application::agent_eval::EvalStore::record_eval_event(&self.queue, kind, payload)
    }
    fn eval_events(&self) -> Result<Vec<RunEvent>> {
        crate::application::agent_eval::EvalStore::eval_events(&self.queue)
    }
}

impl crate::application::commands::replans::ReplanStore for ServiceSqlite {
    fn run_task(&self, run: &RunId) -> Result<Option<TaskId>> {
        crate::application::commands::replans::ReplanStore::run_task(&self.queue, run)
    }
    fn record_replan(
        &mut self,
        new: &crate::domain::task_replan::NewReplan,
        origin: crate::domain::task_replan::ReplanOrigin,
        by_role: &str,
        by_id: &str,
    ) -> Result<crate::domain::task_replan::ReplanRecorded> {
        self.queue.record_replan(new, origin, by_role, by_id)
    }
    fn replan_operation(
        &self,
        id: crate::domain::ReplanId,
    ) -> Result<crate::domain::task_replan::ReplanOperation> {
        self.queue.replan_operation(id)
    }
    fn move_replan(
        &mut self,
        id: crate::domain::ReplanId,
        generation: i64,
        step: &crate::domain::task_replan::ReplanMove,
    ) -> Result<crate::domain::task_replan::ReplanOperation> {
        self.queue.move_replan(id, generation, step)
    }
}

impl crate::application::agent_cases::CaseSetStore for ServiceSqlite {
    fn record_cases_event(
        &self,
        kind: EventKind,
        payload: Value,
    ) -> Result<crate::domain::EventId> {
        crate::application::agent_cases::CaseSetStore::record_cases_event(
            &self.queue,
            kind,
            payload,
        )
    }
    fn cases_events(&self) -> Result<Vec<RunEvent>> {
        crate::application::agent_cases::CaseSetStore::cases_events(&self.queue)
    }
}

impl DialogueStore for ServiceSqlite {
    fn read_ask(&self, id: AskId) -> Result<Ask> {
        self.queue.read_ask(id)
    }
    fn request_planner(&self, request: RequestId) -> Result<Option<PlannerId>> {
        crate::application::commands::requests::RequestStore::request_planner(&self.queue, request)
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
        covered_by: Option<TaskId>,
    ) -> Result<Finding> {
        self.dialogue()
            .set_finding_status(id, to, reason, by, covered_by)
    }
}

impl ServiceQueue for ServiceSqlite {
    fn show(&mut self, id: TaskId, full: bool, events: usize) -> Result<Value> {
        let detail = TaskReads::show(&mut self.queue, id)?;
        Ok(if full {
            serde_json::to_value(detail)?
        } else {
            (self.detail)(&detail, events)
        })
    }

    fn run_status(&self, id: &RunId) -> Result<Option<RunStatus>> {
        match RunReads::run(&self.queue, id) {
            Ok(run) => Ok(Some(run.status())),
            Err(error) if error.downcast_ref::<RunNotFound>().is_some() => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn proposals(&self, all: bool) -> Result<Value> {
        Ok(json!({"proposals": TaskReads::proposals(&self.queue, all)?}))
    }

    fn show_proposal(&self, id: ProposalId) -> Result<Value> {
        Ok(serde_json::to_value(TaskReads::show_proposal(
            &self.queue,
            id,
        )?)?)
    }

    /// As the command line reads it ([`ServiceReads`]), without cmux.
    fn read(&mut self, read: &QueueRead) -> Result<Value> {
        (self.reads)(&mut self.queue, &self.db, read)
    }

    fn session_owner(&self, session_id: &str) -> Result<Option<StepTarget>> {
        Ok(SessionOwners::session_owner(&self.queue, session_id)?.map(|owner| owner.target))
    }

    fn record_report(&mut self, report: &TelemetryReport) -> Result<ReportRecorded> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
            });
        let steps = SessionStepStore::record_steps(
            &self.queue,
            &report.target,
            &report.session_id,
            &report.steps,
            now_ms,
        )?;
        let resources = SessionResourceStore::record_resources(
            &self.queue,
            &report.target,
            &report.session_id,
            &report.resources,
            now_ms,
        )?;
        Ok(ReportRecorded { steps, resources })
    }

    fn telemetry_target(&self, target: &StepTarget) -> Result<TargetFacts> {
        self.queue.telemetry_target(target)
    }

    fn register_session(
        &mut self,
        target: &StepTarget,
        session_id: &str,
        at_ms: i64,
    ) -> Result<()> {
        SessionOwners::register_session(&self.queue, target, session_id, at_ms)
    }

    fn supervisor_store(
        &mut self,
    ) -> &mut dyn crate::application::supervisor_service::SupervisorStore {
        &mut self.queue
    }

    fn under_lease(
        &mut self,
        run: &RunId,
        token: &LeaseToken,
        then: &mut dyn FnMut() -> Result<Value>,
    ) -> Result<Value> {
        let conn = &self.queue.conn;
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(anyhow::Error::from)
            .map_err(super::sqlite::tag_busy)?;
        let result = match self.queue.holds_lease(run, token) {
            Ok(true) => then(),
            Ok(false) => Err(anyhow::anyhow!(
                "run {run} is not leased to the supervisor that asked: nothing is issued under its lease"
            )),
            Err(error) => Err(error),
        };
        match result {
            Ok(value) => {
                conn.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn session(&mut self, op: &crate::application::session::SessionOp) -> Result<Value> {
        crate::application::session::apply(&mut self.queue, op)
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
}

impl SystemQueueService {
    pub fn new(db: &Path, executable: &Path) -> Self {
        Self {
            db: db.to_path_buf(),
            executable: executable.to_path_buf(),
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

/// Start `command` as a process of the control side no starter has to
/// reap: it leaves the starter's session, so a terminal's signals do not
/// reach it, and runs as the child of a first child that exits at once.
/// The exec's failure still comes back: the second child holds the
/// spawn's pipe until it execs.
pub(crate) fn spawn_detached(command: &mut Command) -> std::io::Result<()> {
    // SAFETY: setsid(2), fork(2) and _exit(2) are async-signal-safe and
    // touch no memory.
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
    command.spawn()?.wait().map(drop)
}

pub(crate) fn signal(pid: u32, signal: libc::c_int) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: a signal to a pid; an error (no such process) is fine.
        unsafe { libc::kill(pid, signal) };
    }
}

/// Whether `pid` runs `dagq ... service serve` for `db` now: a pid read
/// from a record, not from the service's own answer, is signalled only
/// then, never a process that took it later.
fn serves(pid: u32, db: &Path) -> bool {
    runs_for(pid, "service serve", db)
}

/// Whether `pid` runs a `dagq` command line holding `command` for `db` now.
pub(crate) fn runs_for(pid: u32, command_words: &str, db: &Path) -> bool {
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
        && command.contains(command_words)
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
            .args(["service", "serve"])
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
        spawn_detached(&mut command)
            .with_context(|| format!("start {}", self.executable.display()))?;
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
mod supervisor_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn the_environment_names_a_socket_a_broker_or_neither_and_both_is_refused() {
        use crate::domain::queue_service::{CREDENTIAL_FILE_ENV, SOCKET_ENV, URL_ENV};
        // Neither: dagq opens the queue itself, as before.
        assert_eq!(Client::from_env(env_of(&[])), Ok(None));
        assert_eq!(
            Client::from_env(env_of(&[(SOCKET_ENV, " "), (URL_ENV, "")])),
            Ok(None)
        );
        assert_eq!(
            Client::from_env(env_of(&[
                (SOCKET_ENV, "/q.sock"),
                (CREDENTIAL_FILE_ENV, "/c")
            ])),
            Ok(Some(Client {
                route: ClientRoute::Socket(PathBuf::from("/q.sock")),
                credential: Some(PathBuf::from("/c")),
            }))
        );
        assert_eq!(
            Client::from_env(env_of(&[
                (URL_ENV, "http://127.0.0.1:47310"),
                (CREDENTIAL_FILE_ENV, "/c"),
                ("DAGQ_RUN_ID", "r1"),
            ])),
            Ok(Some(Client {
                route: ClientRoute::Broker {
                    address: "127.0.0.1:47310".to_owned(),
                    run: Some("r1".to_owned()),
                },
                credential: Some(PathBuf::from("/c")),
            }))
        );
        let both = Client::from_env(env_of(&[
            (SOCKET_ENV, "/q.sock"),
            (URL_ENV, "http://127.0.0.1:1"),
        ]))
        .unwrap_err();
        assert_eq!(both.code, "client_misconfigured");
        assert!(
            both.message.contains(SOCKET_ENV) && both.message.contains(URL_ENV),
            "{both}"
        );
        let bad = Client::from_env(env_of(&[(URL_ENV, "https://example.com")])).unwrap_err();
        assert_eq!(bad.code, "client_misconfigured");
        let injected = Client::from_env(env_of(&[
            (URL_ENV, "http://127.0.0.1:1"),
            ("DAGQ_RUN_ID", "r1\r\nX: y"),
        ]))
        .unwrap_err();
        assert_eq!(injected.code, "client_misconfigured");
    }

    #[test]
    fn an_answer_past_the_limit_is_written_and_read_as_response_too_large() {
        // The service writes an answer within the limit as it is, and one
        // past it as its refusal, with its size and how to narrow the read.
        let small = ServiceResponse::success(json!({"events": []}));
        let line = answer_line(&small, Some(UseCase::Events)).unwrap();
        assert_eq!(
            line,
            [serde_json::to_vec(&small).unwrap(), b"\n".to_vec()].concat()
        );
        let large = ServiceResponse::success(json!("x".repeat(MAX_ANSWER_BYTES)));
        let line = answer_line(&large, Some(UseCase::Events)).unwrap();
        assert!(line.len() < MAX_ANSWER_BYTES);
        let refusal: ServiceResponse = serde_json::from_slice(&line).unwrap();
        let error = refusal.error.unwrap();
        assert_eq!(error.code, ServiceErrorCode::ResponseTooLarge);
        let size = serde_json::to_vec(&large).unwrap().len() + 1;
        assert!(
            error.message.contains(&format!("is {size} bytes")),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("--since / --until"),
            "{}",
            error.message
        );

        // A client that read past the limit refuses it so too, and one that
        // got no answer is `unreachable`.
        let past = no_answer(&AnswerPastLimit.into(), UseCase::Notes, "look");
        assert_eq!(past.code, "response_too_large", "{past}");
        assert!(past.message.contains("--limit"), "{past}");
        let events = no_answer(&AnswerPastLimit.into(), UseCase::Events, "look");
        assert_eq!(events.code, "response_too_large", "{events}");
        assert!(
            events
                .message
                .contains(&format!("longer than the {MAX_ANSWER_BYTES} bytes")),
            "{events}"
        );
        for option in ["--since / --until", "--limit", "--after <cursor>"] {
            assert!(events.message.contains(option), "{events}");
        }
        let gone = no_answer(
            &anyhow::anyhow!("connection refused"),
            UseCase::Notes,
            "look",
        );
        assert_eq!(gone.code, "unreachable", "{gone}");
        assert!(gone.message.contains("connection refused"), "{gone}");
        assert!(gone.message.contains("(look)"), "{gone}");
    }

    /// A broker on a port of this process that answers one request with
    /// `status` and `body`, and hands back what it read.
    fn one_shot_broker(status: u16, body: Vec<u8>) -> (String, thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let request = super::super::queue_broker::read_request(&mut reader).unwrap();
            // The client may stop reading early (an answer past its
            // limit): what is left unwritten does not matter.
            let mut writer = &stream;
            let _ = write!(
                writer,
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let _ = writer.write_all(&body);
            format!(
                "{} {} auth={:?} run={:?} body={}",
                request.method,
                request.target,
                request.authorization,
                request.run,
                String::from_utf8_lossy(&request.body)
            )
        });
        (address, handle)
    }

    #[test]
    fn a_broker_client_sends_the_broker_s_request_and_reads_its_answer_and_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let credential = dir.path().join("token");
        fs::write(&credential, "secret-token\n").unwrap();
        let client = |address: &str| Client {
            route: ClientRoute::Broker {
                address: address.to_owned(),
                run: Some("r1".to_owned()),
            },
            credential: Some(credential.clone()),
        };
        let answer = serde_json::to_vec(&ServiceResponse::success(json!({"id": 3}))).unwrap();
        let (address, broker) = one_shot_broker(200, answer);
        let result = client(&address)
            .call(UseCase::Show, json!({"id": 3}))
            .unwrap();
        assert_eq!(result, json!({"id": 3}));
        let seen = broker.join().unwrap();
        assert!(
            seen.starts_with("POST /queue auth=Some(\"Bearer secret-token\") run=Some(\"r1\")"),
            "{seen}"
        );
        // The body is the service's request; the token is only the Bearer.
        let body: ServiceRequest =
            serde_json::from_str(seen.split_once("body=").unwrap().1).unwrap();
        assert_eq!(body.use_case, UseCase::Show);
        assert_eq!(body.token, None);
        assert_eq!(body.params, json!({"id": 3}));

        // The broker's refusal is read as the service's, by its code.
        let refusal = serde_json::to_vec(&ServiceResponse::failure(
            ServiceErrorCode::Unauthenticated,
            "no run token",
        ))
        .unwrap();
        let (address, broker) = one_shot_broker(401, refusal);
        let error = client(&address)
            .call(UseCase::Show, json!({"id": 3}))
            .unwrap_err();
        broker.join().unwrap();
        let failure = error.downcast_ref::<ClientFailure>().unwrap();
        assert_eq!(failure.code, "unauthenticated");

        // No broker is `unreachable`.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = closed.local_addr().unwrap().to_string();
        drop(closed);
        let error = client(&address)
            .call(UseCase::Show, json!({"id": 3}))
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ClientFailure>().unwrap().code,
            "unreachable"
        );
    }

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

    /// The key of command digests is made once per queue (ADR-t1673-1
    /// decision 10): threads that ask at once for a queue that has none get
    /// one key, its file in the service's directory has mode 0600, and a
    /// later ask reads the same key; a file of another size is refused and
    /// kept; another queue has another key.
    #[test]
    fn a_queues_key_of_command_digests_is_made_once_and_kept() {
        use crate::application::TelemetrySinks;
        let dir = tempfile::tempdir().unwrap();
        let queue = dir.path().join("queue");
        let db = queue.join("queue.db");
        // Made by threads at once: one key, whichever made it.
        let start = std::sync::Arc::new(std::sync::Barrier::new(4));
        let made: Vec<_> = (0..4)
            .map(|_| {
                let (queue, start) = (queue.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    digest_key(&queue).unwrap()
                })
            })
            .collect();
        let made: Vec<_> = made.into_iter().map(|made| made.join().unwrap()).collect();
        let key = SystemServiceAccess.digest_key(&db).unwrap();
        assert!(made.iter().all(|made| *made == key));
        let path = digest_key_path(&queue);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let made = fs::read(&path).unwrap();
        assert_eq!(made.len(), crate::domain::command_shape::DIGEST_KEY_BYTES);
        // Asked again: the same key, the file kept.
        assert_eq!(digest_key(&queue).unwrap(), key);
        assert_eq!(fs::read(&path).unwrap(), made);
        assert_eq!(
            key.digest("hunter2"),
            digest_key(&queue).unwrap().digest("hunter2")
        );
        // Another queue's key digests the same word apart.
        let other = digest_key(&dir.path().join("other")).unwrap();
        assert_ne!(other.digest("hunter2"), key.digest("hunter2"));
        // A file of another size is no key and is not replaced.
        fs::write(&path, b"short").unwrap();
        assert!(digest_key(&queue).is_err());
        assert_eq!(SystemServiceAccess.digest_key(&db), None);
        assert_eq!(fs::read(&path).unwrap(), b"short");
        // No temporary file is left beside it.
        let left: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    /// The executor's sink sends with its target's telemetry token, read
    /// from its file, never the agent's token of the same run, and has none
    /// once the telemetry token is revoked.
    #[test]
    fn an_executors_sink_holds_its_targets_telemetry_token_not_the_agents() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("queue.db");
        let run = RunId::new("r1").unwrap();
        let target = StepTarget::run(&run);
        let agent = issue(dir.path(), &Principal::worker(&run, TaskId::new(3)), 10).unwrap();
        assert!(ServiceTelemetrySink::of_target(&db, &target).is_none());
        // Issued at a time given as a value, as `issue_telemetry` does at
        // its own.
        let telemetry = issue_under(
            &TelemetryLock::take(dir.path()).unwrap(),
            dir.path(),
            &Principal::telemetry(&target, Some(TaskId::new(3))),
            None,
            10,
        )
        .unwrap()
        .file;
        let sink = ServiceTelemetrySink::of_target(&db, &target).unwrap();
        assert_eq!(sink.client.credential.as_deref(), Some(telemetry.as_path()));
        assert_ne!(
            sink.client.credential.as_deref(),
            Some(agent.file.as_path())
        );
        assert_eq!(
            sink.client.route,
            ClientRoute::Socket(socket_path(dir.path()))
        );
        assert!(
            SystemServiceAccess
                .revoke_telemetry(&db, &target, None)
                .unwrap()
        );
        assert!(ServiceTelemetrySink::of_target(&db, &target).is_none());
    }

    /// A wrapper's kept steps are in the queue's `service/steps/`, beside
    /// the tokens' files: outside the run's directory and its worktree,
    /// the planners' and the jobs' directories, and every directory a
    /// run's container mounts; the target's end removes the file.
    #[test]
    fn a_wrappers_kept_steps_are_beside_the_tokens_outside_what_an_agent_writes() {
        use crate::application::TelemetrySinks;
        use crate::domain::run_container::{ContainerLimits, WorkerContainer, worker_run_args};
        use crate::infrastructure::location;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("queue.db");
        let run = RunId::new("r1").unwrap();
        let target = StepTarget::run(&run);
        // No service's directory yet: none is made outside it.
        assert_eq!(SystemServiceAccess.steps_file(&db, &target), None);
        issue(dir.path(), &Principal::worker(&run, TaskId::new(3)), 10).unwrap();
        let file = SystemServiceAccess.steps_file(&db, &target).unwrap();
        assert_eq!(
            file.parent(),
            Some(service_dir(dir.path()).join(STEPS_DIR).as_path())
        );
        let mode = fs::metadata(file.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        let run_dir = location::runs_dir(&db).join("r1");
        let clone = run_dir.join("worktree-r1");
        for agents in [
            run_dir.clone(),
            clone.clone(),
            location::planners_dir(&db),
            location::plan_reviews_dir(&db),
            dir.path().join(location::GOAL_REVIEWS_DIR_NAME),
            dir.path().join(location::AGENT_EVALS_DIR_NAME),
        ] {
            assert!(!file.starts_with(&agents), "{}", agents.display());
        }
        let queue = crate::domain::podman_machine::QueueKey::new("77067154921b9014").unwrap();
        let args = worker_run_args(&WorkerContainer {
            queue: &queue,
            run: "r1",
            image: "localhost/dagq-q-image:t",
            clone: &clone,
            run_dir: &run_dir,
            build_output: Some("/build"),
            worker_cache: &["/cache".to_owned()],
            limits: ContainerLimits::default(),
            env: &[],
            command: &["claude".to_owned()],
        })
        .unwrap();
        for pair in args.windows(2).filter(|pair| pair[0] == "--volume") {
            let source = pair[1].split(':').next().unwrap();
            assert!(!file.starts_with(source), "{}", pair[1]);
        }
        // Another target keeps its own file.
        let planner = StepTarget::of(crate::domain::session_step::TargetKind::Planner, 3);
        assert_ne!(
            SystemServiceAccess.steps_file(&db, &planner),
            Some(file.clone())
        );
        // The sweep reads the target each kept file names, a temporary
        // file beside it going with it, and a lone half-written one naming
        // none.
        let written = crate::application::step_report::KeptSteps {
            target: target.clone(),
            state: crate::application::step_report::ReporterState::default(),
        };
        fs::write(&file, serde_json::to_vec(&written).unwrap()).unwrap();
        let tmp = file.with_extension("json.tmp");
        fs::write(&tmp, "{").unwrap();
        let lone = file.with_file_name("other.json.tmp");
        fs::write(&lone, "{\"tar").unwrap();
        fs::write(file.with_file_name("notes.txt"), "").unwrap();
        // Their times, and the sweep's, are values: the file written 5 s
        // before it looks, the lone one an hour before.
        let at = |secs: u64| std::time::UNIX_EPOCH + Duration::from_secs(secs);
        let written_at = |path: &Path, secs| {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(at(secs))
                .unwrap();
        };
        written_at(&file, 1_000_000);
        written_at(&lone, 1_000_005 - 3_600);
        let listed = kept_steps(dir.path(), at(1_000_005)).unwrap();
        let named: Vec<_> = listed
            .iter()
            .map(|kept| (kept.file.clone(), kept.target.clone(), kept.age_ms))
            .collect();
        let mut expected = vec![
            (file.clone(), Some(target.clone()), 5_000),
            (lone.clone(), None, 3_600_000),
        ];
        expected.sort();
        assert_eq!(named, expected);
        // A time written after the look (a clock set back) is no age.
        assert!(
            kept_steps(dir.path(), at(999_999))
                .unwrap()
                .iter()
                .all(|kept| kept.file != file || kept.age_ms == 0)
        );
        SystemServiceAccess.forget_steps(&db, &file).unwrap();
        assert!(!file.exists());
        assert!(!tmp.exists());
        SystemServiceAccess.forget_steps(&db, &lone).unwrap();
        assert!(!lone.exists());
        // Nothing to remove is no error; a file elsewhere is refused.
        SystemServiceAccess.forget_steps(&db, &file).unwrap();
        let elsewhere = run_dir.join("turns").join("steps.json");
        assert!(SystemServiceAccess.forget_steps(&db, &elsewhere).is_err());
    }

    #[test]
    fn a_sweep_revokes_the_tokens_it_is_told_ended_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        // No token issued yet: nothing to sweep.
        assert!(sweep(dir.path(), &mut |_| true).unwrap().is_empty());
        let ended = Principal::worker(&RunId::new("r1").unwrap(), TaskId::new(3));
        let live = Principal::worker(&RunId::new("r2").unwrap(), TaskId::new(4));
        let ended_file = issue(dir.path(), &ended, 1).unwrap().file;
        let ended_token = read_token(&ended_file).unwrap();
        let live_token = read_token(&issue(dir.path(), &live, 1).unwrap().file).unwrap();
        // A record its actor's value no longer names, as a crash between
        // the two removals of a revoke leaves.
        let orphan = "b".repeat(64);
        let record = TokenRecord {
            principal: ended.clone(),
            issued_at: 0,
            issued_under: None,
        };
        write_private(
            &record_path(dir.path(), &orphan),
            &serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        // The record of an actor's old token, swept after a token was
        // issued to it again (the look saw the old one ended): the sweep of
        // the old record leaves the new value.
        let run = RunId::new("r3").unwrap();
        let again = Principal::worker(&run, TaskId::new(5));
        let again_file = issue(dir.path(), &again, 1).unwrap().file;
        let old = Principal::worker(&run, TaskId::new(6));
        let stale = "c".repeat(64);
        write_private(
            &record_path(dir.path(), &stale),
            &serde_json::to_vec(&TokenRecord {
                principal: old.clone(),
                issued_at: 0,
                issued_under: None,
            })
            .unwrap(),
        )
        .unwrap();
        let swept = sweep(dir.path(), &mut |principal| principal == &old).unwrap();
        assert_eq!(swept, [again.actor_id.as_str()]);
        assert!(again_file.exists());
        assert_eq!(lookup(dir.path(), &stale).unwrap(), None);
        assert!(revoke(dir.path(), &again.actor_id).unwrap());
        let revoked = sweep(dir.path(), &mut |principal| principal == &ended).unwrap();
        assert_eq!(revoked, [ended.actor_id.as_str()]);
        assert!(!ended_file.exists());
        assert_eq!(lookup(dir.path(), &ended_token).unwrap(), None);
        assert_eq!(lookup(dir.path(), &orphan).unwrap(), None);
        assert_eq!(lookup(dir.path(), &live_token).unwrap(), Some(live));
        let records = fs::read_dir(service_dir(dir.path()).join(TOKENS_DIR)).unwrap();
        assert_eq!(records.count(), 1);
    }

    #[test]
    fn a_service_takes_its_owner_from_the_env_and_never_stops_without_one() {
        let env = |value: &'static str| {
            move |name: &str| (name == OWNER_PID_ENV).then(|| value.to_owned())
        };
        assert_eq!(owner_from_env(env("123")), Some(123));
        assert_eq!(owner_from_env(env("not a pid")), None);
        assert_eq!(owner_from_env(|_| None), None);
        // Production names no owner: the service never stops for one.
        assert!(!owner_gone(None));
        assert!(!owner_gone(Some(std::process::id())));
        // The init process is there, whoever's it is.
        assert!(!owner_gone(Some(1)));
        // A gone owner: queue_service::a_service_stops_once_the_test_process_that_started_it_is_gone
        // in tests/it, with real processes.
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
        let control =
            SystemQueueService::new(&dir.path().join("queue.db"), Path::new("/nonexistent/dagq"));
        assert_eq!(control.stop(Duration::from_secs(1)).unwrap(), None);
        assert!(ServiceRecord::read(dir.path()).is_none());
        // A start of a binary that is not there fails.
        assert!(control.start(Duration::from_secs(1)).is_err());
    }

    /// A client uses only a service whose range holds its API version
    /// (ADR-t3102-2 decision 4): an answer of a newer or an older range is
    /// `api_version_mismatch` whatever it carries, a result or a refusal;
    /// one within it is taken as it is.
    #[test]
    fn a_client_takes_an_answer_only_from_a_service_whose_range_holds_its_version() {
        let code = |result: Result<Value>| {
            result
                .unwrap_err()
                .downcast::<ClientFailure>()
                .unwrap()
                .code
        };
        let within = ServiceResponse::success(json!({"a": 1}));
        assert_eq!(answer_of(within.clone()).unwrap(), json!({"a": 1}));
        let ranges = [
            (API_VERSION + 1, API_VERSION + 2),
            (API_VERSION.saturating_sub(2), API_VERSION.saturating_sub(1)),
        ];
        for (min, max) in ranges {
            for mut response in [
                within.clone(),
                ServiceResponse::failure(ServiceErrorCode::AuthorizationDenied, "no"),
            ] {
                response.min_api_version = min;
                response.api_version = max;
                assert_eq!(code(answer_of(response)), "api_version_mismatch");
            }
        }
        let refused = ServiceResponse::failure(ServiceErrorCode::Unauthenticated, "no token");
        assert_eq!(code(answer_of(refused)), "unauthenticated");
    }

    /// `locate`'s reach: a service that answers is reachable with its
    /// versions, one of other versions too but not compatible, and nothing
    /// answering or a refusal in front of it is not reachable, with why.
    #[test]
    fn a_hello_is_read_as_reachable_or_not_with_its_reason() {
        let answered = reach_of(Ok(ServiceResponse::success(json!({"build": "x"}))));
        assert_eq!(
            answered,
            json!({"reachable": true, "api_version": API_VERSION,
                   "min_api_version": crate::domain::queue_service::MIN_API_VERSION,
                   "api_compatible": true})
        );
        let mut newer =
            ServiceResponse::failure(ServiceErrorCode::ApiVersionMismatch, "not this version");
        newer.api_version = API_VERSION + 2;
        newer.min_api_version = API_VERSION + 1;
        let other = reach_of(Ok(newer));
        assert_eq!(other["reachable"], true);
        assert_eq!(other["api_compatible"], false);
        assert_eq!(other["min_api_version"], API_VERSION + 1);
        let refused = reach_of(Ok(ServiceResponse::failure(
            ServiceErrorCode::Unauthenticated,
            "no token",
        )));
        assert_eq!(
            refused,
            json!({"reachable": false, "reason": "unauthenticated: no token"})
        );
        let silent = reach_of(Err(anyhow::anyhow!("nothing listens")));
        assert_eq!(
            silent,
            json!({"reachable": false, "reason": "nothing listens"})
        );
    }
}
