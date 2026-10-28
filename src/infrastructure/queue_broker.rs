//! The queue broker on the host ([Queue broker]): the process `dagq
//! queue-broker serve` runs ([`serve`]) on a TCP port of `127.0.0.1`, its
//! HTTP/1.1 requests read and answered here ([`read_request`],
//! [`write_answer`]) and decided by [`QueueBroker::handle`], the requests
//! it passes on to the queue service's unix socket ([`SystemBrokerBackend`]),
//! the tunnels it opens to the registries of the `dagq.toml` in the main
//! checkout the queue is bound to ([`SystemBrokerBackend::egress`],
//! [`tunnel`]), the Claude API's requests it sends on with the credential
//! `host.toml` chooses and whose answers it streams back as they come
//! ([`forward_claude`], [`super::claude_credential`]), the client a caller speaks to it with ([`exchange`],
//! [`call`], [`health`]), the tokens of `integrate`'s verifications
//! ([`issue_verification`], [`revoke_verification`],
//! [`verification_issue`], [`revoke_verification_issue`],
//! [`sweep_verifications`], [`SystemVerificationTokens`], all under one
//! lock) and the control `up`, `down` and the
//! supervisor start, look at and stop it with ([`SystemQueueBroker`],
//! whose look is [`probe`]).
//!
//! Everything lives in `<queue dir>/broker/` (mode 0700): `state.json`
//! (what the running broker recorded of itself, its address among it),
//! `lock` (held with `flock` while one runs, as the queue service's),
//! `broker.log`, and the verifications' tokens. The broker reads the queue
//! service's tokens (it is the control side) and writes no token's value
//! to a log, an event or an error.
//!
//! [Queue broker]: ../../docs/design/queue-broker.md

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use super::queue_service::{
    AnswerPastLimit, Client, ClientFailure, Lock, lock_file_held, lookup, owner_gone, private_dir,
    queue_dir_of, runs_for, sha256_hex, signal, socket_path, spawn_detached, unix_now,
    write_private,
};
use crate::application::Generators;
use crate::application::queue_broker::ClaudeForward;
use crate::application::queue_broker::{
    BrokerAnswer, BrokerBackend, BrokerProbe, BrokerRequest, DenialRecords, QueueBroker,
    QueueBrokerControl, ServiceUnanswered, TokenFor, TokenIssue, TokenSweep, VerificationTokens,
};
use crate::domain::queue_broker::{
    AUTHORIZATION_HEADER, BEARER, BROKER_DIR, CLAUDE_PATH, ClaudeCredential,
    ClaudeCredentialProblem, Egress, HEALTH_PATH, LOCK_FILE, LOG_FILE, MAX_BODY_BYTES,
    MAX_CLAUDE_BODY_BYTES, PROXY_AUTHORIZATION_HEADER, QUEUE_PATH, RUN_HEADER, STATE_FILE,
    VERIFICATION_CREDENTIALS_DIR, VERIFICATION_TOKENS_DIR, git_remote_host,
};
use crate::domain::queue_broker::{
    BrokerBinding, BrokerRecordParams, BrokerRunStatus, BrokerRunStatusParams, stamped,
};
use crate::domain::queue_service::{
    MAX_ANSWER_BYTES, Principal, ServiceRequest, ServiceState, UseCase,
};
use crate::domain::{EventKind, RunId, RunStatus};

/// How long a look waits for the broker's health.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the broker waits for one request or for writing its answer.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the broker waits for the queue service's answer: as long as a
/// client-mode `dagq` waits for it.
const SERVICE_TIMEOUT: Duration = super::queue_service::CLIENT_TIMEOUT;
/// How long the broker waits for the queue service's answer to a request
/// of its own (a run's status, the binding, a record): one connection's
/// thread, the sweep's or the counts' waits this long at most, never the
/// loop that takes connections.
const OWN_SERVICE_TIMEOUT: Duration = IO_TIMEOUT;
/// How long a stopping broker waits for its last counts of refusals to be
/// recorded.
const FINAL_FLUSH_WAIT: Duration = Duration::from_secs(2);
/// How long the broker waits to reach a tunnel's host.
const TUNNEL_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a tunnel stays open with no byte either way.
const TUNNEL_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// How often a tunnel's quiet way looks whether the tunnel is idle.
const TUNNEL_POLL: Duration = Duration::from_secs(5);
/// How often the broker looks whether its queue is still there.
const QUEUE_CHECK: Duration = Duration::from_secs(2);
/// The longest request line or header line the broker reads.
const MAX_LINE_BYTES: u64 = 8 * 1024;
/// The most headers the broker reads of one request.
const MAX_HEADERS: usize = 64;
/// How long the broker waits to reach the Claude API.
const CLAUDE_CONNECT_TIMEOUT_SECS: u64 = 15;
/// The longest a Claude API request may take, its answer's stream
/// included: past it the request is cut.
const CLAUDE_MAX_TIME_SECS: u64 = 3600;
/// The longest head (status line and headers) of the Claude API's answer
/// the broker reads.
const MAX_ANSWER_HEAD_BYTES: usize = 64 * 1024;

/// `<queue dir>/broker`.
pub fn broker_dir(queue_dir: &Path) -> PathBuf {
    queue_dir.join(BROKER_DIR)
}

/// `<queue dir>/broker/broker.log`.
pub fn log_path(queue_dir: &Path) -> PathBuf {
    broker_dir(queue_dir).join(LOG_FILE)
}

/// What a running broker records in `state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerRecord {
    pub pid: u32,
    pub build: String,
    /// The `127.0.0.1:<port>` it listens on: the relay forwards there.
    pub address: String,
    pub started_at: i64,
}

impl BrokerRecord {
    pub fn read(queue_dir: &Path) -> Option<Self> {
        let bytes = fs::read(broker_dir(queue_dir).join(STATE_FILE)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

fn lock_held(queue_dir: &Path) -> bool {
    lock_file_held(&broker_dir(queue_dir).join(LOCK_FILE))
}

// --- HTTP ----------------------------------------------------------------------

/// Read one HTTP/1.1 request from `reader`: the request line, the headers
/// the broker reads ([`AUTHORIZATION_HEADER`],
/// [`PROXY_AUTHORIZATION_HEADER`], [`RUN_HEADER`], `Host`,
/// `Content-Length`) with every header as it came, and a body of up to
/// [`MAX_BODY_BYTES`] ([`MAX_CLAUDE_BODY_BYTES`] for a path under
/// [`CLAUDE_PATH`]), whose length `Content-Length` gives (a
/// `Transfer-Encoding` is refused). A request that does not read comes back
/// with what was read of it.
pub fn read_request(
    reader: &mut impl BufRead,
) -> Result<BrokerRequest, (Box<BrokerRequest>, anyhow::Error)> {
    let mut request = BrokerRequest::default();
    // A header the broker refuses does not stop the read: the run and the
    // host that follow it are still read, for the refusal's record.
    let mut problem: Option<anyhow::Error> = None;
    macro_rules! fail {
        ($error:expr) => {
            return Err((Box::new(request), $error))
        };
    }
    let line = match read_line(reader) {
        Ok(line) => line,
        Err(error) => fail!(error),
    };
    let mut words = line.split_whitespace();
    let (Some(method), Some(target), Some(version)) = (words.next(), words.next(), words.next())
    else {
        fail!(anyhow::anyhow!("the request line does not read"));
    };
    request.method = method.to_owned();
    request.target = target.to_owned();
    if !version.starts_with("HTTP/1.") {
        fail!(anyhow::anyhow!("not an HTTP/1 request"));
    }
    // Only the Claude API's own paths (as `Destination::of` reads them)
    // may carry its larger bodies.
    let claude_path = target
        .strip_prefix(CLAUDE_PATH)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?']));
    let max_body = if claude_path {
        MAX_CLAUDE_BODY_BYTES
    } else {
        MAX_BODY_BYTES
    };
    let mut length = 0usize;
    for _ in 0..=MAX_HEADERS {
        let line = match read_line(reader) {
            Ok(line) => line,
            Err(error) => fail!(error),
        };
        if line.is_empty() {
            if let Some(problem) = problem {
                fail!(problem);
            }
            let mut body = Vec::new();
            if let Err(error) = reader.take(length as u64).read_to_end(&mut body) {
                fail!(error.into());
            }
            if body.len() != length {
                fail!(anyhow::anyhow!("the body ends before its length"));
            }
            request.body = body;
            return Ok(request);
        }
        let Some((name, value)) = line.split_once(':') else {
            problem.get_or_insert_with(|| anyhow::anyhow!("a header does not read"));
            continue;
        };
        let value = value.trim().to_owned();
        request
            .headers
            .push((name.trim().to_owned(), value.clone()));
        match name.trim().to_ascii_lowercase().as_str() {
            AUTHORIZATION_HEADER => request.authorization = Some(value),
            PROXY_AUTHORIZATION_HEADER => request.proxy_authorization = Some(value),
            RUN_HEADER => request.run = Some(value),
            "host" => request.host = Some(value),
            // A chunked or otherwise encoded body is not read: a request
            // gives its body's length.
            "transfer-encoding" => {
                problem.get_or_insert_with(|| {
                    anyhow::anyhow!(
                        "a request gives its body's Content-Length, not a Transfer-Encoding"
                    )
                });
            }
            "content-length" => match value.parse::<usize>() {
                Ok(read) if read <= max_body => length = read,
                Ok(_) => {
                    problem.get_or_insert_with(|| anyhow::anyhow!("the request is too long"));
                }
                Err(_) => {
                    problem
                        .get_or_insert_with(|| anyhow::anyhow!("the content length does not read"));
                }
            },
            _ => {}
        }
    }
    fail!(anyhow::anyhow!(
        "the request has more than {MAX_HEADERS} headers"
    ))
}

fn read_line(reader: &mut impl BufRead) -> Result<String> {
    let mut line = Vec::new();
    reader
        .take(MAX_LINE_BYTES + 1)
        .read_until(b'\n', &mut line)?;
    ensure!(
        line.len() as u64 <= MAX_LINE_BYTES,
        "a line of the request is too long"
    );
    ensure!(line.ends_with(b"\n"), "the request ends before its headers");
    let line = String::from_utf8(line).context("the request is not UTF-8")?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "Unknown",
    }
}

/// Write `answer` as an HTTP/1.1 response that closes the connection.
pub fn write_answer(writer: &mut impl Write, answer: &BrokerAnswer) -> std::io::Result<()> {
    write!(
        writer,
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        answer.status,
        reason_phrase(answer.status),
        answer.body.len()
    )?;
    writer.write_all(&answer.body)?;
    writer.flush()
}

/// Open the tunnel `stream`'s `CONNECT` was passed to `host`:`port`: reach
/// the host, answer `200`, and copy the bytes both ways (those `reader`
/// read past the request first) until either side closes or stays idle
/// for [`TUNNEL_IDLE_TIMEOUT`]. A host that is not reached is answered
/// with `failed`'s refusal.
pub fn tunnel(
    stream: &TcpStream,
    reader: &BufReader<TcpStream>,
    host: &str,
    port: u16,
    failed: impl FnOnce(anyhow::Error) -> BrokerAnswer,
) -> Result<()> {
    let upstream = match connect_to(host, port) {
        Ok(upstream) => upstream,
        Err(error) => {
            write_answer(&mut &*stream, &failed(error))?;
            return Ok(());
        }
    };
    // Each read wakes up now and then to look whether the other way is
    // still busy: a long download sends nothing the other way.
    for side in [stream, &upstream] {
        side.set_read_timeout(Some(TUNNEL_POLL))?;
        side.set_write_timeout(Some(TUNNEL_IDLE_TIMEOUT))?;
    }
    (&*stream).write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    (&upstream).write_all(reader.buffer())?;
    let opened = Instant::now();
    let last = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (client, server) = (stream.try_clone()?, upstream.try_clone()?);
    let up = {
        let last = last.clone();
        thread::spawn(move || splice(client, server, opened, &last))
    };
    splice(upstream, stream.try_clone()?, opened, &last);
    let _ = up.join();
    Ok(())
}

/// Reach `host`:`port` at the first of its addresses that answers.
fn connect_to(host: &str, port: u16) -> Result<TcpStream> {
    use std::net::ToSocketAddrs;
    let mut last = None;
    for address in (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolve {host}"))?
    {
        match TcpStream::connect_timeout(&address, TUNNEL_CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(match last {
        Some(error) => anyhow::Error::new(error).context(format!("reach {host}:{port}")),
        None => anyhow::anyhow!("{host} has no address"),
    })
}

/// Copy `from` to `to` until `from` ends or fails, or neither way of the
/// tunnel moved a byte for [`TUNNEL_IDLE_TIMEOUT`] (`last`: the time of
/// the last byte either way, in milliseconds since `opened`), then end
/// `to`'s writes.
fn splice(
    mut from: TcpStream,
    to: TcpStream,
    opened: Instant,
    last: &std::sync::atomic::AtomicU64,
) {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match from.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if (&to).write_all(&buffer[..read]).is_err() {
                    break;
                }
                last.store(opened.elapsed().as_millis() as u64, Ordering::SeqCst);
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                let idle = opened
                    .elapsed()
                    .saturating_sub(Duration::from_millis(last.load(Ordering::SeqCst)));
                if idle >= TUNNEL_IDLE_TIMEOUT {
                    break;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = to.shutdown(Shutdown::Write);
    let _ = from.shutdown(Shutdown::Read);
}

// --- The client ------------------------------------------------------------------

/// Send one request to the broker at `address` (`127.0.0.1:<port>`): the
/// request line and headers in `head` (without the blank line that ends
/// them; a `Content-Length` of `body` is added), and `body`. The answer's
/// status and body.
pub fn exchange(
    address: &str,
    head: &str,
    body: &[u8],
    timeout: Duration,
) -> Result<(u16, Vec<u8>)> {
    // The connection waits no longer than the exchange: a probe's short
    // wait holds for a host that drops the packets too.
    let stream = connect(address, timeout)
        .with_context(|| format!("the queue broker at {address} is unreachable"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    exchange_on(stream, head, body)
}

/// A connection to `address` (`<host>:<port>`) to each of its addresses in
/// turn, waiting at most `timeout` on each.
fn connect(address: &str, timeout: Duration) -> std::io::Result<TcpStream> {
    let mut last = None;
    for addr in std::net::ToSocketAddrs::to_socket_addrs(address)? {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(ErrorKind::NotFound, "the address resolves to nothing")
    }))
}

/// [`exchange`] on a connection already made: the broker's port, or a
/// run's socket a relay passes on to it.
pub fn exchange_on(
    mut stream: impl Read + Write,
    head: &str,
    body: &[u8],
) -> Result<(u16, Vec<u8>)> {
    write!(
        stream,
        "{head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let status_line = read_line(&mut reader)?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .context("the broker's status line does not read")?;
    let mut length = None;
    loop {
        let line = read_line(&mut reader)?;
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<u64>().ok();
        }
    }
    // A client reads no more than a client of the service's socket does.
    let limit = MAX_ANSWER_BYTES as u64;
    if length.is_some_and(|length| length > limit) {
        return Err(AnswerPastLimit.into());
    }
    let mut answer = Vec::new();
    reader
        .take(length.unwrap_or(limit + 1))
        .read_to_end(&mut answer)?;
    if answer.len() as u64 > limit {
        return Err(AnswerPastLimit.into());
    }
    Ok((status, answer))
}

/// Send `request` to the queue service through the broker at `address`,
/// with the run token `token` of `run`, in the shape
/// [`crate::domain::queue_broker`] gives. The answer's status and body.
pub fn call(
    address: &str,
    token: Option<&str>,
    run: Option<&str>,
    request: &ServiceRequest,
    timeout: Duration,
) -> Result<(u16, Vec<u8>)> {
    let mut head =
        format!("POST {QUEUE_PATH} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json");
    if let Some(token) = token {
        head.push_str(&format!("\r\nAuthorization: {BEARER}{token}"));
    }
    if let Some(run) = run {
        head.push_str(&format!("\r\n{RUN_HEADER}: {run}"));
    }
    exchange(address, &head, &serde_json::to_vec(request)?, timeout)
}

/// The broker's health at `address`.
pub fn health(address: &str, timeout: Duration) -> Result<Value> {
    let (status, body) = exchange(
        address,
        &format!("GET {HEALTH_PATH} HTTP/1.1\r\nHost: {address}"),
        &[],
        timeout,
    )?;
    health_answer(status, &body)
}

/// The broker's health through the unix socket at `socket`, which a relay
/// passes on to it: whether the run's whole path to the broker holds.
pub fn health_through(socket: &Path, timeout: Duration) -> Result<Value> {
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("the relay's socket {} is unreachable", socket.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let (status, body) = exchange_on(
        stream,
        &format!("GET {HEALTH_PATH} HTTP/1.1\r\nHost: 127.0.0.1"),
        &[],
    )?;
    health_answer(status, &body)
}

fn health_answer(status: u16, body: &[u8]) -> Result<Value> {
    ensure!(
        status == 200,
        "the queue broker answered its health with {status}"
    );
    serde_json::from_slice(body).context("the queue broker's health does not read")
}

// --- The broker -----------------------------------------------------------------

/// What the broker runs its requests on, on this host: the queue service's
/// tokens of the queue, and the service's socket, which it passes a run's
/// requests on to and sends its own to as the user ([`Client::broker`]).
/// It holds no queue's path and opens no queue: a service that does not
/// answer is a run it cannot read (the request is refused) or a record it
/// only logs.
pub struct SystemBrokerBackend {
    pub queue_dir: PathBuf,
    /// The broker's process and build, which its records name
    /// ([`stamped`]): their actor is the user's.
    pub pid: u32,
    pub build: String,
    /// The host-wide `host.toml`, under the queue's
    /// ([`super::claude_credential::load`]).
    pub host_wide: Option<PathBuf>,
    /// The keychain a `[claude_credential]` may name an item of: this
    /// host's ([`super::claude_credential::Keychain::of_host`]).
    pub keychain: Option<super::claude_credential::Keychain>,
}

impl SystemBrokerBackend {
    /// `use_case` with `params` done by the queue service for the broker.
    fn call<T: serde::de::DeserializeOwned>(&self, use_case: UseCase, params: Value) -> Result<T> {
        let answer =
            Client::broker(&self.queue_dir).call_within(use_case, params, OWN_SERVICE_TIMEOUT)?;
        serde_json::from_value(answer)
            .with_context(|| format!("the queue service's answer to {}", use_case.as_str()))
    }

    /// Record the queue event `kind` with `payload`, naming the broker.
    fn record(&self, kind: EventKind, payload: Value) -> Result<()> {
        self.call::<Value>(
            UseCase::BrokerRecord,
            serde_json::to_value(BrokerRecordParams {
                kind,
                payload: stamped(payload, self.pid, &self.build),
            })?,
        )
        .map(drop)
    }
}

impl BrokerBackend for SystemBrokerBackend {
    fn principal(&self, token: &str) -> Result<Option<Principal>> {
        lookup(&self.queue_dir, token)
    }

    fn run_status(&self, id: &RunId) -> Result<Option<RunStatus>> {
        let read: BrokerRunStatus = self
            .call(
                UseCase::BrokerRunStatus,
                serde_json::to_value(BrokerRunStatusParams {
                    run: id.as_str().to_owned(),
                })?,
            )
            .map_err(|error| {
                let unanswered = error
                    .downcast_ref::<ClientFailure>()
                    .is_some_and(|failure| failure.code == "unreachable");
                if unanswered {
                    error.context(ServiceUnanswered)
                } else {
                    error
                }
            })?;
        Ok(read.status)
    }

    /// The service's answer as it wrote it, whatever its length: the
    /// broker cuts nothing.
    fn forward(&self, line: &[u8]) -> Result<Vec<u8>> {
        let socket = socket_path(&self.queue_dir);
        let mut stream = UnixStream::connect(&socket)
            .with_context(|| format!("the queue service at {} is unreachable", socket.display()))?;
        stream.set_read_timeout(Some(SERVICE_TIMEOUT))?;
        stream.set_write_timeout(Some(SERVICE_TIMEOUT))?;
        stream
            .write_all(line)
            .context("send the request to the queue service")?;
        stream.shutdown(Shutdown::Write).ok();
        let mut answer = Vec::new();
        stream
            .read_to_end(&mut answer)
            .context("read the queue service's answer")?;
        ensure!(!answer.is_empty(), "the queue service answered nothing");
        Ok(answer)
    }

    fn record_denied(&self, payload: Value) -> Result<()> {
        self.record(EventKind::QueueBrokerDenied, payload)
    }

    /// Read again for each tunnel, so a landing that changes the
    /// registries takes effect without a restart: the `dagq.toml` of the
    /// main checkout of the repository the queue is bound to, and the hosts
    /// of its remotes. A queue bound to none reaches no registry.
    fn egress(&self) -> Result<Egress> {
        let bound: BrokerBinding = self.call(UseCase::BrokerBinding, json!({}))?;
        let Some(common_dir) = bound.binding else {
            return Ok(Egress::default());
        };
        let checkout = super::adapters::main_checkout_of(Path::new(&common_dir))?;
        let registries = super::run_env::load_egress_registries(&checkout)?;
        let upstream_git_hosts = if registries.is_empty() {
            Vec::new()
        } else {
            remote_hosts(&checkout)?
        };
        Ok(Egress {
            registries,
            upstream_git_hosts,
        })
    }

    fn verification_run(&self, token: &str) -> Result<Option<RunId>> {
        verification_run(&self.queue_dir, token)
    }

    fn claude_credential(&self) -> Result<ClaudeCredential, ClaudeCredentialProblem> {
        super::claude_credential::read(
            &self.queue_dir,
            self.host_wide.as_deref(),
            self.keychain
                .as_ref()
                .map(|keychain| keychain as &dyn super::claude_credential::KeychainLookup),
        )
    }
}

/// The hosts the remotes of the repository at `checkout` fetch from and
/// push to (each `remote.<name>.url` and `pushurl`).
fn remote_hosts(checkout: &Path) -> Result<Vec<String>> {
    let output = Command::new(super::git_binary::git_executable()?)
        .arg("-C")
        .arg(checkout)
        .args(["config", "--get-regexp", r"^remote\..*\.(push)?url$"])
        .stdin(Stdio::null())
        .output()
        .context("read the repository's remotes")?;
    // `git config --get-regexp` exits with 1 when nothing matches.
    ensure!(
        output.status.success() || output.status.code() == Some(1),
        "git config could not read the remotes of {}: {}",
        checkout.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let mut hosts: Vec<String> = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(host) = line
            .split_once(' ')
            .and_then(|(_, url)| git_remote_host(url))
            && !hosts.contains(&host)
        {
            hosts.push(host);
        }
    }
    Ok(hosts)
}

// --- The tokens of the verifications ---------------------------------------------

/// What a verification token's record holds: its run and when it was
/// issued.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VerificationRecord {
    run: RunId,
    issued_at: i64,
}

/// Where the value of `run`'s token for `of` is: the hash of the run's ID,
/// with the landing recheck's and the snapshot's apart from the landing's.
fn verification_credential_path(queue_dir: &Path, run: &RunId, of: TokenFor) -> PathBuf {
    let key = match of {
        TokenFor::Landing => run.as_str().to_owned(),
        TokenFor::Recheck => format!("recheck:{run}"),
        TokenFor::Snapshot => format!("satisfied:{run}"),
    };
    broker_dir(queue_dir)
        .join(VERIFICATION_CREDENTIALS_DIR)
        .join(sha256_hex(key.as_bytes()))
}

fn verification_record_path(queue_dir: &Path, token: &str) -> PathBuf {
    broker_dir(queue_dir)
        .join(VERIFICATION_TOKENS_DIR)
        .join(format!("{}.json", sha256_hex(token.as_bytes())))
}

/// The lock file in the broker's directory every issue, revocation and
/// sweep of a verification token holds while it reads and changes the
/// files: a sweep or a recovery that judged one issue of a run's token
/// never removes another issued in between.
const VERIFICATION_LOCK_FILE: &str = "verification.lock";

/// [`VERIFICATION_LOCK_FILE`] of the queue at `queue_dir`, held until it
/// is dropped, waiting while another process holds it.
fn verification_lock(queue_dir: &Path) -> Result<fs::File> {
    let dir = broker_dir(queue_dir);
    private_dir(&dir)?;
    let path = dir.join(VERIFICATION_LOCK_FILE);
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
    Ok(file)
}

/// Issue the token of `run`'s verification for `of`, revoking the one it
/// held for `of`: 32
/// random bytes in hex, its value in the broker's
/// [`VERIFICATION_CREDENTIALS_DIR`], its run in [`VERIFICATION_TOKENS_DIR`]
/// under the value's hash, apart from the queue service's tokens. The file
/// its value is in. Only the control side (`integrate`, the landing
/// recheck and the verification of a snapshot) calls this.
pub fn issue_verification(
    queue_dir: &Path,
    run: &RunId,
    of: TokenFor,
    now: i64,
) -> Result<PathBuf> {
    let _lock = verification_lock(queue_dir)?;
    revoke_locked(queue_dir, run, of)?;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("draw a token's random bytes: {error}"))?;
    let token: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    for dir in [VERIFICATION_TOKENS_DIR, VERIFICATION_CREDENTIALS_DIR] {
        private_dir(&broker_dir(queue_dir).join(dir))?;
    }
    // The value first: one whose value is written can always be revoked.
    let file = verification_credential_path(queue_dir, run, of);
    write_private(&file, token.as_bytes())?;
    write_private(
        &verification_record_path(queue_dir, &token),
        &serde_json::to_vec(&VerificationRecord {
            run: run.clone(),
            issued_at: now,
        })?,
    )?;
    Ok(file)
}

/// Revoke the token of `run`'s verification for `of`; whether there was
/// one.
pub fn revoke_verification(queue_dir: &Path, run: &RunId, of: TokenFor) -> Result<bool> {
    let _lock = verification_lock(queue_dir)?;
    revoke_locked(queue_dir, run, of)
}

/// The issue of `run`'s token for `of` now (the hash of its value, which
/// names the token's record); `None` while it holds none.
pub fn verification_issue(
    queue_dir: &Path,
    run: &RunId,
    of: TokenFor,
) -> Result<Option<TokenIssue>> {
    let file = verification_credential_path(queue_dir, run, of);
    match fs::read_to_string(&file) {
        Ok(value) => Ok(Some(TokenIssue(sha256_hex(value.trim().as_bytes())))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", file.display())),
    }
}

/// Revoke `run`'s token for `of` only while it is still `issue`, under
/// the lock an issue holds: one issued again since is another
/// verification's and stays. Whether it was revoked.
pub fn revoke_verification_issue(
    queue_dir: &Path,
    run: &RunId,
    of: TokenFor,
    issue: &TokenIssue,
) -> Result<bool> {
    let _lock = verification_lock(queue_dir)?;
    if verification_issue(queue_dir, run, of)?.as_ref() != Some(issue) {
        return Ok(false);
    }
    revoke_locked(queue_dir, run, of)
}

/// [`revoke_verification`] under the lock its caller holds.
fn revoke_locked(queue_dir: &Path, run: &RunId, of: TokenFor) -> Result<bool> {
    let file = verification_credential_path(queue_dir, run, of);
    let token = match fs::read_to_string(&file) {
        Ok(token) => token,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("read {}", file.display())),
    };
    match fs::remove_file(verification_record_path(queue_dir, token.trim())) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("remove a verification token's record"),
    }
    fs::remove_file(&file).with_context(|| format!("remove {}", file.display()))?;
    Ok(true)
}

/// The run whose verification `token` was issued for; `None` for one never
/// issued or revoked.
pub fn verification_run(queue_dir: &Path, token: &str) -> Result<Option<RunId>> {
    // A hex value of the issued length only: no other text names a file.
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let bytes = match fs::read(verification_record_path(queue_dir, token)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read a verification token's record"),
    };
    let record: VerificationRecord =
        serde_json::from_slice(&bytes).context("a verification token's record")?;
    Ok(Some(record.run))
}

/// How long an issue may take to write both halves of a token: a half
/// alone younger than this may be one an issue outside the lock (of a
/// binary before it) is writing, and is left.
const ISSUE_GRACE_SECS: i64 = 60;

/// Revoke each verification token `keep` refuses, and remove the halves of
/// a token left alone past [`ISSUE_GRACE_SECS`]: a record whose value is
/// gone or that does not read, or a value with no record (which the broker
/// never took). `keep` judges outside the lock, and only the issue it
/// judged is revoked ([`revoke_verification_issue`]). A token whose record
/// does not read within the grace, that `keep` fails on, or whose files do
/// not read is kept and named in the failures; a file gone meanwhile is
/// not. Idempotent: what is gone is not looked for again.
pub fn sweep_verifications(
    queue_dir: &Path,
    now: i64,
    keep: &mut dyn FnMut(&RunId, TokenFor) -> Result<bool>,
) -> Result<TokenSweep> {
    let mut sweep = TokenSweep::default();
    let records = broker_dir(queue_dir).join(VERIFICATION_TOKENS_DIR);
    for path in entries(&records, &mut sweep.failures) {
        let Some(hash) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".json"))
            .map(str::to_owned)
        else {
            continue;
        };
        let record = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<VerificationRecord>(&bytes)
                .map_err(|error| anyhow::Error::from(error).context("does not read")),
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                failed(&mut sweep, &format!("read {}", path.display()), error);
                continue;
            }
        };
        let record = match record {
            Ok(record) => record,
            Err(error) => {
                // One that does not read past the grace is no token the
                // broker could take: it goes, rather than fail every sweep.
                match age_secs(&path, now) {
                    Ok(age) if age >= ISSUE_GRACE_SECS => {
                        remove_orphan(queue_dir, &path, || true, &mut sweep);
                    }
                    Ok(_) => sweep
                        .failures
                        .push(format!("the record {}: {error:#}", path.display())),
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => failed(&mut sweep, &format!("look at {}", path.display()), error),
                }
                continue;
            }
        };
        // What it is for: the use whose value file holds its value.
        let mut of = None;
        let mut unread = false;
        for each in TokenFor::ALL {
            match verification_issue(queue_dir, &record.run, each) {
                Ok(Some(issue)) if issue.0 == hash => of = Some(each),
                Ok(_) => {}
                Err(error) => {
                    unread = true;
                    sweep.failures.push(format!("{error:#}"));
                }
            }
        }
        if unread {
            continue;
        }
        let Some(of) = of else {
            // No issue after it writes a value of its hash: it stays alone.
            if now - record.issued_at >= ISSUE_GRACE_SECS {
                remove_orphan(queue_dir, &path, || true, &mut sweep);
            }
            continue;
        };
        let what =
            |error: anyhow::Error| format!("run {}'s {} token: {error:#}", record.run, of.as_str());
        match keep(&record.run, of) {
            Ok(true) => {}
            Ok(false) => {
                match revoke_verification_issue(queue_dir, &record.run, of, &TokenIssue(hash)) {
                    Ok(true) => sweep.revoked.push((record.run.clone(), of)),
                    Ok(false) => {}
                    Err(error) => sweep.failures.push(what(error)),
                }
            }
            Err(error) => sweep.failures.push(what(error)),
        }
    }
    let credentials = broker_dir(queue_dir).join(VERIFICATION_CREDENTIALS_DIR);
    for path in entries(&credentials, &mut sweep.failures) {
        let value = match fs::read_to_string(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                failed(&mut sweep, &format!("read {}", path.display()), error);
                continue;
            }
        };
        let record = verification_record_path(queue_dir, value.trim());
        if record.exists() {
            continue;
        }
        match age_secs(&path, now) {
            Ok(age) if age >= ISSUE_GRACE_SECS => {
                // Still the value it read, issued again into the same file
                // or not, with still no record.
                let alone =
                    || fs::read_to_string(&path).is_ok_and(|now| now == value) && !record.exists();
                remove_orphan(queue_dir, &path, alone, &mut sweep);
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => failed(&mut sweep, &format!("look at {}", path.display()), error),
        }
    }
    Ok(sweep)
}

/// Name in `sweep`'s failures what `doing` failed with.
fn failed(sweep: &mut TokenSweep, doing: &str, error: std::io::Error) {
    sweep.failures.push(format!("{doing}: {error}"));
}

/// How long ago at `now` the file at `path` was written.
fn age_secs(path: &Path, now: i64) -> std::io::Result<i64> {
    let modified = fs::metadata(path)?.modified()?;
    let secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    Ok(now - i64::try_from(secs).unwrap_or(i64::MAX))
}

/// Remove a half of a token left alone, under the lock an issue holds and
/// while `alone` still finds it so, counted in `sweep`.
fn remove_orphan(
    queue_dir: &Path,
    path: &Path,
    alone: impl FnOnce() -> bool,
    sweep: &mut TokenSweep,
) {
    let removed = verification_lock(queue_dir).and_then(|_lock| {
        if !alone() {
            return Ok(false);
        }
        match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
        }
    });
    match removed {
        Ok(true) => sweep.orphans += 1,
        Ok(false) => {}
        Err(error) => sweep.failures.push(format!("{error:#}")),
    }
}

/// The files in `dir`, none when it is not there; what does not read is
/// named in `failures`.
fn entries(dir: &Path, failures: &mut Vec<String>) -> Vec<PathBuf> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            failures.push(format!("read {}: {error}", dir.display()));
            return Vec::new();
        }
    };
    let mut paths = Vec::new();
    for entry in read {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => failures.push(format!("read {}: {error}", dir.display())),
        }
    }
    paths
}

/// The tokens of the verifications of the queue at `db`, on this host.
#[derive(Debug, Clone)]
pub struct SystemVerificationTokens {
    pub db: PathBuf,
}

impl VerificationTokens for SystemVerificationTokens {
    fn issue(&self, run: &RunId, of: TokenFor) -> Result<PathBuf> {
        issue_verification(&queue_dir_of(&self.db), run, of, unix_now())
    }

    fn revoke(&self, run: &RunId, of: TokenFor) -> Result<bool> {
        revoke_verification(&queue_dir_of(&self.db), run, of)
    }

    fn issued(&self, run: &RunId, of: TokenFor) -> Result<Option<TokenIssue>> {
        verification_issue(&queue_dir_of(&self.db), run, of)
    }

    fn revoke_issued(&self, run: &RunId, of: TokenFor, issue: &TokenIssue) -> Result<bool> {
        revoke_verification_issue(&queue_dir_of(&self.db), run, of, issue)
    }

    fn sweep(&self, keep: &mut dyn FnMut(&RunId, TokenFor) -> Result<bool>) -> Result<TokenSweep> {
        sweep_verifications(&queue_dir_of(&self.db), unix_now(), keep)
    }
}

/// How `dagq queue-broker serve` runs.
#[derive(Clone)]
pub struct BrokerServeOptions {
    pub db: PathBuf,
    pub generators: Generators,
    /// Set by SIGINT or SIGTERM: the broker stops accepting and exits.
    pub stop: Arc<AtomicBool>,
    /// How often the loop looks for a connection or a stop.
    pub poll: Duration,
    /// The process the broker stops with: a test's, named as the queue
    /// service's owner is ([`crate::domain::queue_service::OWNER_PID_ENV`]).
    /// None in production.
    pub owner: Option<u32>,
    /// The build the broker names in its record and its health: this
    /// binary's ([`crate::VERSION`]); a test's broker stands in for another
    /// binary's with another.
    pub build: String,
    /// Where the Claude API's requests go:
    /// [`crate::domain::queue_broker::CLAUDE_API_URL`]; a test's fake
    /// upstream.
    pub claude_api: String,
    /// The host-wide `host.toml` the Claude credential is read from under
    /// the queue's: [`super::kpi_config::host_wide_file`]; none in a test.
    pub host_wide: Option<PathBuf>,
}

/// Run the queue's broker until `stop` is set, its queue goes or its owner
/// does: take the queue's broker lock (a second broker of the queue is
/// refused), listen on a port of `127.0.0.1`, record itself with that
/// address, and answer each connection on its own thread.
pub fn serve(options: &BrokerServeOptions) -> Result<Value> {
    let queue_dir = queue_dir_of(&options.db);
    ensure!(options.db.is_file(), "no queue at {}", options.db.display());
    let dir = broker_dir(&queue_dir);
    private_dir(&dir)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let lock = loop {
        if let Some(lock) = Lock::try_take(&dir.join(LOCK_FILE), true, libc::LOCK_EX)? {
            break lock;
        }
        if Instant::now() >= deadline {
            let pid = BrokerRecord::read(&queue_dir).map(|record| record.pid);
            bail!(
                "a queue broker already runs for this queue (pid {})",
                pid.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string())
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("listen on 127.0.0.1")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?.to_string();
    let pid = std::process::id();
    let record = BrokerRecord {
        pid,
        build: options.build.clone(),
        address: address.clone(),
        started_at: unix_now(),
    };
    write_private(&dir.join(STATE_FILE), &serde_json::to_vec(&record)?)?;
    info!(pid, %address, "the queue broker listens");
    let backend = Arc::new(SystemBrokerBackend {
        queue_dir: queue_dir.clone(),
        pid,
        build: options.build.clone(),
        host_wide: options.host_wide.clone(),
        keychain: super::claude_credential::Keychain::of_host(),
    });
    // The sweep and the counts of refusals wait on the queue service, which
    // may take a connection and never answer: they run beside the loop, so
    // the health, a stop and the looks at the queue and the owner never
    // wait on them. What a verification killed before its end left behind
    // goes as the broker starts; a failure is recorded and the broker
    // serves on.
    {
        let backend = backend.clone();
        let tokens = SystemVerificationTokens {
            db: options.db.clone(),
        };
        thread::spawn(move || {
            let swept = crate::application::queue_broker::sweep_left_tokens(&tokens, &|run| {
                backend.run_status(run)
            });
            crate::application::queue_broker::note_token_sweep(&swept, "broker", |payload| {
                backend.record(EventKind::QueueBrokerTokensSwept, payload)
            });
        });
    }
    let denials = Arc::new(DenialRecords::new(options.generators.clock.clone()));
    let flushing = Arc::new(AtomicBool::new(true));
    let (flushed_tx, flushed) = std::sync::mpsc::channel();
    {
        let (backend, denials, flushing) = (backend.clone(), denials.clone(), flushing.clone());
        let poll = options.poll;
        thread::spawn(move || {
            while flushing.load(Ordering::SeqCst) {
                denials.flush(&*backend);
                thread::sleep(poll);
            }
            denials.flush(&*backend);
            let _ = flushed_tx.send(());
        });
    }
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
                let denials = denials.clone();
                let build = options.build.clone();
                let claude = ClaudeUpstream {
                    url: options.claude_api.clone(),
                    scratch: dir.clone(),
                };
                thread::spawn(move || {
                    if let Err(error) = answer(stream, &*backend, &denials, &build, pid, &claude) {
                        warn!("a queue broker connection failed: {error:#}");
                    }
                });
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => thread::sleep(options.poll),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => {
                warn!("the queue broker could not accept: {error}");
                thread::sleep(options.poll);
            }
        }
    };
    // The last counts, waited on only for a while: a service that does
    // not answer does not hold the stop.
    flushing.store(false, Ordering::SeqCst);
    if flushed.recv_timeout(FINAL_FLUSH_WAIT).is_err() {
        warn!(
            "the last counts of refusals not recorded were not recorded within {}s",
            FINAL_FLUSH_WAIT.as_secs()
        );
    }
    if BrokerRecord::read(&queue_dir).is_some_and(|record| record.pid == pid) {
        let _ = fs::remove_file(dir.join(STATE_FILE));
    }
    info!(pid, outcome, "the queue broker stopped: {outcome}");
    drop(lock);
    Ok(json!({"outcome": outcome, "pid": pid, "served": served}))
}

/// Read one request from `stream`, answer it, and close.
fn answer(
    stream: TcpStream,
    backend: &dyn BrokerBackend,
    denials: &DenialRecords,
    build: &str,
    pid: u32,
    claude: &ClaudeUpstream,
) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let broker = QueueBroker {
        backend,
        denials,
        build,
        pid,
    };
    let mut reader = BufReader::new(stream.try_clone()?);
    // A connection closed before it sent anything (a look whether the port
    // is open) is no request, and nothing is recorded of it.
    if reader.fill_buf()?.is_empty() {
        return Ok(());
    }
    let (request, answer) = match read_request(&mut reader) {
        Ok(request) => {
            let answer = broker.handle(&request);
            (request, answer)
        }
        Err((read, error)) => {
            let answer = broker.malformed(&read, &format!("{error:#}"));
            (*read, answer)
        }
    };
    match (&answer.tunnel, &answer.claude) {
        (Some((host, port)), _) => tunnel(&stream, &reader, host, *port, |error| {
            broker.tunnel_failed(&request, &error)
        }),
        (None, Some(forward)) => forward_claude(&stream, claude, forward, &request.body, |error| {
            broker.claude_failed(&request, &error)
        }),
        (None, None) => Ok(write_answer(&mut &stream, &answer)?),
    }
}

/// Where the broker sends the Claude API's requests, and the directory
/// (the broker's, mode 0700) a request's body waits in while it is sent.
#[derive(Debug, Clone)]
pub struct ClaudeUpstream {
    pub url: String,
    pub scratch: PathBuf,
}

/// Send `forward` with `body` to the Claude API and write its answer to
/// `stream` as it comes: the status line and headers (with `Connection:
/// close` in place of the API's own connection's), then the body's bytes
/// untouched (a chunked SSE stream stays chunked, each event passed on as
/// it arrives), until the API ends it or `stream` goes. An API that is not
/// reached, before any byte of its answer, is answered with `failed`'s
/// refusal.
///
/// `curl` sends it (HTTP/1.1, no `~/.curlrc`, no redirect followed, the
/// answer's encodings untouched): the request's headers, the credential
/// among them, reach it on stdin as its config, never in its arguments,
/// and the body from a file of mode 0600 in [`ClaudeUpstream::scratch`],
/// removed once sent.
pub fn forward_claude(
    stream: &TcpStream,
    upstream: &ClaudeUpstream,
    forward: &ClaudeForward,
    body: &[u8],
    failed: impl FnOnce(anyhow::Error) -> BrokerAnswer,
) -> Result<()> {
    let body_file = upstream
        .scratch
        .join(format!("claude-{}.body", uuid::Uuid::new_v4().simple()));
    struct Remove<'a>(&'a Path);
    impl Drop for Remove<'_> {
        fn drop(&mut self) {
            let _ = fs::remove_file(self.0);
        }
    }
    let _remove = Remove(&body_file);
    let config = (|| -> Result<String> {
        if !body.is_empty() {
            write_private(&body_file, body)?;
        }
        Ok(curl_config(
            &format!("{}{}", upstream.url.trim_end_matches('/'), forward.path),
            forward,
            (!body.is_empty()).then_some(body_file.as_path()),
        ))
    })();
    let sent = config.and_then(|config| {
        let mut child = Command::new("curl")
            // `-g`: a `[` or `{` of the path is no glob; a host's proxy's
            // `CONNECT` answer is not written before the API's.
            .args(["-q", "-sS", "-g", "--http1.1", "-i", "--raw", "-N"])
            .arg("--suppress-connect-headers")
            .args(["--noproxy", "localhost,127.0.0.1,::1"])
            .args([
                "--connect-timeout",
                &CLAUDE_CONNECT_TIMEOUT_SECS.to_string(),
            ])
            .args(["--max-time", &CLAUDE_MAX_TIME_SECS.to_string()])
            .args(["--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("start curl")?;
        if let Some(mut stdin) = child.stdin.take()
            && let Err(error) = stdin.write_all(config.as_bytes())
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::Error::new(error).context("give curl the request"));
        }
        Ok(child)
    });
    let mut child = match sent {
        Ok(child) => child,
        Err(error) => {
            write_answer(&mut &*stream, &failed(error))?;
            return Ok(());
        }
    };
    let mut output = BufReader::new(child.stdout.take().context("curl's stdout")?);
    let head = match read_answer_head(&mut output) {
        Ok(Some(head)) => head,
        // Nothing came (curl says why on stderr), or a head that does not
        // read: what may follow it is not waited for.
        outcome => {
            if outcome.is_err() {
                let _ = child.kill();
            }
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            let status = child.wait()?;
            let error = match outcome {
                Err(error) => error,
                _ => anyhow::anyhow!(
                    "the Claude API was not reached (curl {status}): {}",
                    stderr.trim()
                ),
            };
            write_answer(&mut &*stream, &failed(error))?;
            return Ok(());
        }
    };
    let copied = (|| -> std::io::Result<()> {
        (&*stream).write_all(&head)?;
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let read = match output.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(read) => read,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            (&*stream).write_all(&buffer[..read])?;
        }
    })();
    if copied.is_err() {
        // The client went: stop sending for it.
        let _ = child.kill();
    }
    let _ = child.wait();
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

/// curl's config for `forward` to `url`: the method, the URL, each header
/// (an `Expect:` left empty, so no `100 Continue` comes first), and the
/// body's file.
fn curl_config(url: &str, forward: &ClaudeForward, body: Option<&Path>) -> String {
    fn quoted(text: &str) -> String {
        let mut out = String::from("\"");
        for c in text.chars() {
            match c {
                '"' | '\\' => {
                    out.push('\\');
                    out.push(c);
                }
                '\n' | '\r' => out.push(' '),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }
    let mut config = format!(
        "url = {}\nrequest = {}\nheader = \"Expect:\"\n",
        quoted(url),
        quoted(&forward.method)
    );
    for (name, value) in &forward.headers {
        config.push_str(&format!(
            "header = {}\n",
            quoted(&format!("{name}: {value}"))
        ));
    }
    if let Some(body) = body {
        config.push_str(&format!(
            "data-binary = {}\n",
            quoted(&format!("@{}", body.display()))
        ));
    }
    config
}

/// The head (status line and headers) of the answer `curl -i` writes,
/// ready to pass on: a `1xx` head before the last is dropped, and the
/// API's `Connection` and `Keep-Alive` give way to `Connection: close`
/// (the broker's connection carries one request). `None` when nothing
/// came.
fn read_answer_head(output: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    loop {
        let mut lines: Vec<String> = Vec::new();
        let mut read = 0usize;
        loop {
            let mut line = Vec::new();
            let count = output
                .take((MAX_ANSWER_HEAD_BYTES - read) as u64 + 1)
                .read_until(b'\n', &mut line)?;
            if count == 0 {
                if lines.is_empty() {
                    return Ok(None);
                }
                bail!("the Claude API's answer ends in its head");
            }
            read += count;
            ensure!(
                read <= MAX_ANSWER_HEAD_BYTES,
                "the Claude API's answer has a head too long"
            );
            let line = String::from_utf8(line).context("the Claude API's answer is not UTF-8")?;
            let line = line.trim_end_matches(['\r', '\n']).to_owned();
            if line.is_empty() {
                break;
            }
            lines.push(line);
        }
        let status = lines[0]
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse::<u16>().ok())
            .with_context(|| {
                format!("no HTTP status in the Claude API's answer: {:?}", lines[0])
            })?;
        if (100..200).contains(&status) {
            continue;
        }
        let mut head = String::new();
        for line in lines.iter().filter(|line| {
            line.split_once(':').is_none_or(|(name, _)| {
                !["connection", "keep-alive", "proxy-connection"]
                    .iter()
                    .any(|dropped| name.trim().eq_ignore_ascii_case(dropped))
            })
        }) {
            head.push_str(line);
            head.push_str("\r\n");
        }
        head.push_str("Connection: close\r\n\r\n");
        return Ok(Some(head.into_bytes()));
    }
}

// --- The control `up`, `down` and the supervisor use -------------------------

/// Look at the broker of the queue in `queue_dir` without a binary to
/// start one.
pub fn probe(queue_dir: &Path) -> BrokerProbe {
    let stopped = BrokerProbe {
        state: ServiceState::Stopped,
        address: None,
        pid: None,
        build: None,
        build_matches: None,
        started_at: None,
        error: None,
    };
    if !lock_held(queue_dir) {
        return stopped;
    }
    let record = BrokerRecord::read(queue_dir);
    let recorded = BrokerProbe {
        address: record.as_ref().map(|record| record.address.clone()),
        pid: record.as_ref().map(|record| record.pid),
        build: record.as_ref().map(|record| record.build.clone()),
        build_matches: record.as_ref().map(|record| record.build == crate::VERSION),
        started_at: record.as_ref().map(|record| record.started_at),
        ..stopped
    };
    let Some(address) = recorded.address.clone() else {
        return BrokerProbe {
            state: ServiceState::Unreachable,
            error: Some("it holds its lock and recorded no address yet".to_owned()),
            ..recorded
        };
    };
    match health(&address, PROBE_TIMEOUT) {
        Ok(health) => {
            let build = health["build"].as_str().map(str::to_owned);
            BrokerProbe {
                state: ServiceState::Running,
                pid: health["pid"]
                    .as_u64()
                    .and_then(|pid| u32::try_from(pid).ok()),
                build_matches: build.as_deref().map(|build| build == crate::VERSION),
                build,
                ..recorded
            }
        }
        Err(error) => BrokerProbe {
            state: ServiceState::Unreachable,
            error: Some(format!("{error:#}")),
            ..recorded
        },
    }
}

/// The queue's broker on this host: started from `executable` (the fixed
/// binary that runs `up` or the supervisor, ADR-t2114-1 decision 1), its
/// output appended to `broker.log`, detached as the queue service is.
#[derive(Debug, Clone)]
pub struct SystemQueueBroker {
    pub db: PathBuf,
    pub executable: PathBuf,
}

impl SystemQueueBroker {
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

impl QueueBrokerControl for SystemQueueBroker {
    fn probe(&self) -> BrokerProbe {
        probe(&self.queue_dir())
    }

    fn start(&self, timeout: Duration) -> Result<BrokerProbe> {
        let found = self.probe();
        if found.current() {
            return Ok(found);
        }
        if found.state != ServiceState::Stopped {
            self.stop(timeout)?;
        }
        ensure!(
            self.executable.is_file(),
            "no binary at {} to start the queue broker with",
            self.executable.display()
        );
        let queue_dir = self.queue_dir();
        private_dir(&broker_dir(&queue_dir))?;
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
            .args(["queue-broker", "serve"])
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        // The broker is the control side's, whoever started it.
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
            // Any broker that answers will do, whatever its build: another
            // starter may have won the lock, and the executable may already
            // be the next binary an `install` put in place. The caller
            // judges the build.
            let found = probe(&queue_dir);
            if found.state == ServiceState::Running {
                return Ok(found);
            }
            if Instant::now() >= deadline {
                bail!(
                    "the queue broker did not answer within {}s ({}); see {}",
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
            if !lock_held(&queue_dir) {
                let _ = fs::remove_file(broker_dir(&queue_dir).join(STATE_FILE));
            }
        };
        let deadline = Instant::now() + timeout;
        // The pid the broker answered with; one that holds its lock and does
        // not answer is signalled by its recorded pid only while that runs
        // `queue-broker serve` for this queue.
        let pid = loop {
            let found = self.probe();
            match found.state {
                ServiceState::Stopped => {
                    clean();
                    return Ok(None);
                }
                ServiceState::Running => {
                    break found.pid.context("the queue broker answered with no pid")?;
                }
                ServiceState::Unreachable => {
                    if Instant::now() >= deadline {
                        match found
                            .pid
                            .filter(|pid| runs_for(*pid, "queue-broker serve", &self.db))
                        {
                            Some(pid) => break pid,
                            None => bail!(
                                "the queue broker holds its lock and does not answer, and no \
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
                warn!(pid, "the queue broker did not stop on SIGTERM: killing it");
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

    /// Mark the file at `path` written at second `at`, so a test judges
    /// its age against a time it chose, never the clock.
    fn written_at(path: &Path, at: i64) {
        let at = std::time::UNIX_EPOCH + Duration::from_secs(u64::try_from(at).unwrap());
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    /// The tokens a killed verification left behind: the sweep revokes
    /// those `keep` refuses (their value and record both go, so the broker
    /// no longer knows them), keeps the rest, removes a half left alone
    /// past the issue's grace and leaves a younger one, and a second sweep
    /// finds nothing.
    #[test]
    fn the_sweep_revokes_the_refused_tokens_and_the_halves_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let queue_dir = dir.path();
        let run = |id: &str| RunId::new(id).unwrap();
        let value = |file: &Path| fs::read_to_string(file).unwrap();
        let now = 1_000_000;
        let kept = issue_verification(queue_dir, &run("landing"), TokenFor::Landing, now).unwrap();
        let left = issue_verification(queue_dir, &run("ended"), TokenFor::Landing, now).unwrap();
        let recheck =
            issue_verification(queue_dir, &run("landing"), TokenFor::Recheck, now).unwrap();
        let (kept_value, left_value, recheck_value) = (value(&kept), value(&left), value(&recheck));
        // A value whose record is gone, and a record whose value is gone.
        let alone = issue_verification(queue_dir, &run("alone"), TokenFor::Snapshot, now).unwrap();
        fs::remove_file(verification_record_path(queue_dir, &value(&alone))).unwrap();
        written_at(&alone, now);
        let orphan = issue_verification(queue_dir, &run("orphan"), TokenFor::Landing, now).unwrap();
        let orphan_value = value(&orphan);
        fs::remove_file(&orphan).unwrap();
        let mut asked = Vec::new();
        let mut keep = |run: &RunId, of: TokenFor| {
            asked.push((run.to_string(), of));
            Ok(run.as_str() == "landing" && of == TokenFor::Landing)
        };

        // Within the grace the halves alone stay: they may be an issue's.
        let sweep = sweep_verifications(queue_dir, now, &mut keep).unwrap();
        let mut revoked = sweep.revoked.clone();
        revoked.sort_by_key(|(run, _)| run.to_string());
        assert_eq!(
            revoked,
            [
                (run("ended"), TokenFor::Landing),
                (run("landing"), TokenFor::Recheck)
            ]
        );
        assert_eq!((sweep.orphans, sweep.failures.len()), (0, 0), "{sweep:?}");
        assert_eq!(
            verification_run(queue_dir, &kept_value).unwrap(),
            Some(run("landing"))
        );
        assert!(kept.exists());
        for (file, token) in [(&left, &left_value), (&recheck, &recheck_value)] {
            assert!(!file.exists());
            assert_eq!(verification_run(queue_dir, token).unwrap(), None);
        }
        assert!(alone.exists());
        assert_eq!(
            verification_run(queue_dir, &orphan_value).unwrap(),
            Some(run("orphan"))
        );

        // Past it they go; a second sweep then finds nothing.
        let later = now + ISSUE_GRACE_SECS + 1;
        let sweep = sweep_verifications(queue_dir, later, &mut keep).unwrap();
        assert!(sweep.revoked.is_empty());
        assert_eq!((sweep.orphans, sweep.failures.len()), (2, 0), "{sweep:?}");
        assert!(!alone.exists());
        assert_eq!(verification_run(queue_dir, &orphan_value).unwrap(), None);
        assert!(kept.exists());
        let again = sweep_verifications(queue_dir, later, &mut keep).unwrap();
        assert!(again.is_empty(), "{again:?}");
        drop(keep);
        assert!(
            asked
                .iter()
                .all(|(run, _)| run != "alone" && run != "orphan")
        );
    }

    /// A queue with no token, a record that does not read, and a `keep`
    /// that fails: nothing is revoked on a doubt, and the doubts are named.
    #[test]
    fn the_sweep_keeps_what_it_cannot_judge_and_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let queue_dir = dir.path();
        assert!(
            sweep_verifications(queue_dir, 0, &mut |_, _| Ok(false))
                .unwrap()
                .is_empty()
        );
        let run = RunId::new("r1").unwrap();
        let file = issue_verification(queue_dir, &run, TokenFor::Landing, 0).unwrap();
        let sweep =
            sweep_verifications(queue_dir, 0, &mut |_, _| Err(anyhow::anyhow!("locked"))).unwrap();
        assert!(sweep.revoked.is_empty());
        assert_eq!(sweep.failures.len(), 1);
        assert!(sweep.failures[0].contains("locked"), "{sweep:?}");
        assert!(file.exists());
        let bad = broker_dir(queue_dir)
            .join(VERIFICATION_TOKENS_DIR)
            .join("bad.json");
        fs::write(&bad, b"{").unwrap();
        written_at(&bad, 0);
        let sweep = sweep_verifications(queue_dir, 0, &mut |_, _| Ok(true)).unwrap();
        assert_eq!(sweep.failures.len(), 1, "{sweep:?}");
        assert!(bad.exists());
        // Past the grace it is no token anyone could hold: it goes.
        let later = ISSUE_GRACE_SECS + 1;
        let sweep = sweep_verifications(queue_dir, later, &mut |_, _| Ok(true)).unwrap();
        assert_eq!((sweep.orphans, sweep.failures.len()), (1, 0), "{sweep:?}");
        assert!(!bad.exists());
        assert!(file.exists());
    }

    /// A landing that begins on the run after the sweep judged its old
    /// token, and issues it again before the sweep revokes: the issue
    /// judged is the only one revoked, and the new token stays.
    #[test]
    fn the_sweep_revokes_only_the_issue_it_judged() {
        let dir = tempfile::tempdir().unwrap();
        let queue_dir = dir.path();
        let run = RunId::new("r1").unwrap();
        let old = issue_verification(queue_dir, &run, TokenFor::Landing, 0).unwrap();
        let old_value = fs::read_to_string(&old).unwrap();
        let mut new_value = None;
        let sweep = sweep_verifications(queue_dir, 0, &mut |run, of| {
            let file = issue_verification(queue_dir, run, of, 0).unwrap();
            new_value = Some(fs::read_to_string(file).unwrap());
            Ok(false)
        })
        .unwrap();
        assert!(sweep.is_empty(), "{sweep:?}");
        let new_value = new_value.unwrap();
        assert_ne!(new_value, old_value);
        assert_eq!(
            verification_run(queue_dir, &new_value).unwrap(),
            Some(run.clone())
        );
        assert_eq!(verification_run(queue_dir, &old_value).unwrap(), None);
        // Judged, it goes.
        let sweep = sweep_verifications(queue_dir, 0, &mut |_, _| Ok(false)).unwrap();
        assert_eq!(sweep.revoked, [(run.clone(), TokenFor::Landing)]);
        assert_eq!(verification_run(queue_dir, &new_value).unwrap(), None);
    }

    /// A recovery's revocation of the issue it read before moving the run
    /// leaves a token issued since, and revokes the one it read.
    #[test]
    fn a_revocation_of_an_issue_leaves_a_token_issued_since() {
        let dir = tempfile::tempdir().unwrap();
        let queue_dir = dir.path();
        let run = RunId::new("r1").unwrap();
        assert_eq!(
            verification_issue(queue_dir, &run, TokenFor::Landing).unwrap(),
            None
        );
        issue_verification(queue_dir, &run, TokenFor::Landing, 0).unwrap();
        let read = verification_issue(queue_dir, &run, TokenFor::Landing)
            .unwrap()
            .unwrap();
        let again = issue_verification(queue_dir, &run, TokenFor::Landing, 0).unwrap();
        let again_value = fs::read_to_string(&again).unwrap();
        assert!(!revoke_verification_issue(queue_dir, &run, TokenFor::Landing, &read).unwrap());
        assert_eq!(
            verification_run(queue_dir, &again_value).unwrap(),
            Some(run.clone())
        );
        let now = verification_issue(queue_dir, &run, TokenFor::Landing)
            .unwrap()
            .unwrap();
        assert!(revoke_verification_issue(queue_dir, &run, TokenFor::Landing, &now).unwrap());
        assert_eq!(verification_run(queue_dir, &again_value).unwrap(), None);
        assert!(!revoke_verification_issue(queue_dir, &run, TokenFor::Landing, &now).unwrap());
    }

    /// Files that do not read are failures the sweep names, never a token
    /// it revokes or a half it removes, and the sweep goes on: a value
    /// file of another use of a token's run, a value file alone, and a
    /// directory of the records that is no directory. A directory stands
    /// for each unreadable file, so the test holds for any user.
    #[test]
    fn the_sweep_names_the_files_it_cannot_read_and_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let queue_dir = dir.path();
        let run = RunId::new("r1").unwrap();
        let landing = issue_verification(queue_dir, &run, TokenFor::Landing, 0).unwrap();
        let landing_value = fs::read_to_string(&landing).unwrap();
        fs::create_dir(verification_credential_path(
            queue_dir,
            &run,
            TokenFor::Recheck,
        ))
        .unwrap();
        let alone =
            verification_credential_path(queue_dir, &RunId::new("r2").unwrap(), TokenFor::Landing);
        fs::create_dir(&alone).unwrap();
        let later = ISSUE_GRACE_SECS * 10;
        let sweep = sweep_verifications(queue_dir, later, &mut |_, _| Ok(false)).unwrap();
        assert!(sweep.revoked.is_empty(), "{sweep:?}");
        assert_eq!(sweep.orphans, 0, "{sweep:?}");
        // The run's recheck file once as its token's use is looked for and
        // once among the values, the value alone once.
        assert_eq!(sweep.failures.len(), 3, "{sweep:?}");
        assert_eq!(
            verification_run(queue_dir, &landing_value).unwrap(),
            Some(run.clone())
        );
        assert!(alone.exists());

        let other = tempfile::tempdir().unwrap();
        fs::create_dir_all(broker_dir(other.path())).unwrap();
        fs::write(broker_dir(other.path()).join(VERIFICATION_TOKENS_DIR), b"").unwrap();
        let sweep = sweep_verifications(other.path(), later, &mut |_, _| Ok(false)).unwrap();
        assert_eq!(sweep.failures.len(), 1, "{sweep:?}");
        assert!(
            sweep.failures[0].contains(VERIFICATION_TOKENS_DIR),
            "{sweep:?}"
        );
    }

    #[test]
    fn a_request_reads_its_line_headers_and_body() {
        let raw = b"POST /queue HTTP/1.1\r\nHost: 127.0.0.1:1\r\nauthorization: Bearer t\r\nX-Dagq-Run: r1\r\nContent-Length: 2\r\nOther: x\r\n\r\n{}";
        let request = read_request(&mut &raw[..]).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/queue");
        assert_eq!(request.host.as_deref(), Some("127.0.0.1:1"));
        assert_eq!(request.authorization.as_deref(), Some("Bearer t"));
        assert_eq!(request.run.as_deref(), Some("r1"));
        assert_eq!(request.body, b"{}");
        let connect = read_request(&mut &b"CONNECT crates.io:443 HTTP/1.1\r\n\r\n"[..]).unwrap();
        assert_eq!(connect.target, "crates.io:443");
        assert!(connect.body.is_empty());
    }

    #[test]
    fn a_request_that_does_not_read_is_an_error() {
        let too_long = format!(
            "POST /queue HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        let headers = format!(
            "GET / HTTP/1.1\r\n{}\r\n",
            "A: b\r\n".repeat(MAX_HEADERS + 1)
        );
        let line = format!(
            "GET /{} HTTP/1.1\r\n\r\n",
            "a".repeat(MAX_LINE_BYTES as usize)
        );
        for raw in [
            "",
            "GET\r\n\r\n",
            "GET / SMTP\r\n\r\n",
            "GET / HTTP/1.1\r\nno colon\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: x\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nab",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n",
            "GET / HTTP/1.1\r\n",
            &too_long,
            &headers,
            &line,
        ] {
            assert!(read_request(&mut raw.as_bytes()).is_err(), "{raw:.60}");
        }
        // The headers after a refused one are still read.
        let raw = "CONNECT crates.io:443 HTTP/1.1\r\nTransfer-Encoding: chunked\r\nX-Dagq-Run: r1\r\n\r\n";
        let (read, error) = read_request(&mut raw.as_bytes()).unwrap_err();
        assert!(
            format!("{error:#}").contains("Transfer-Encoding"),
            "{error:#}"
        );
        assert_eq!(
            (
                read.method.as_str(),
                read.target.as_str(),
                read.run.as_deref()
            ),
            ("CONNECT", "crates.io:443", Some("r1"))
        );
    }

    #[test]
    fn an_answer_is_written_with_its_status_and_length_and_closes() {
        let mut out = Vec::new();
        write_answer(
            &mut out,
            &BrokerAnswer {
                status: 403,
                body: b"{}".to_vec(),
                tunnel: None,
                claude: None,
            },
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
        );
        for status in [200, 400, 401, 500, 502, 999] {
            assert!(!reason_phrase(status).is_empty());
        }
    }
}
