//! The queue broker on the host ([Queue broker]): the process `dagq
//! queue-broker serve` runs ([`serve`]) on a TCP port of `127.0.0.1`, its
//! HTTP/1.1 requests read and answered here ([`read_request`],
//! [`write_answer`]) and decided by [`QueueBroker::handle`], the requests
//! it passes on to the queue service's unix socket ([`SystemBrokerBackend`]),
//! the client a caller speaks to it with ([`exchange`], [`call`],
//! [`health`]) and the control `up`, `down` and the supervisor start, look
//! at and stop it with ([`SystemQueueBroker`], whose look is [`probe`]).
//!
//! Everything lives in `<queue dir>/broker/` (mode 0700): `state.json`
//! (what the running broker recorded of itself, its address among it),
//! `lock` (held with `flock` while one runs, as the queue service's), and
//! `broker.log`. The broker reads the queue service's tokens (it is the
//! control side) and writes no token's value to a log, an event or an
//! error.
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
    Lock, lock_file_held, lookup, owner_gone, private_dir, queue_dir_of, runs_for, signal,
    socket_path, spawn_detached, unix_now, write_private,
};
use super::sqlite::SqliteQueue;
use crate::application::queue_broker::{
    BrokerAnswer, BrokerBackend, BrokerProbe, BrokerRequest, QueueBroker, QueueBrokerControl,
};
use crate::application::{EventStore, Generators, RunNotFound, RunReads};
use crate::domain::queue_broker::{
    AUTHORIZATION_HEADER, BEARER, BROKER_DIR, HEALTH_PATH, LOCK_FILE, LOG_FILE, MAX_BODY_BYTES,
    QUEUE_PATH, RUN_HEADER, STATE_FILE,
};
use crate::domain::queue_service::{Principal, ServiceRequest, ServiceState};
use crate::domain::{ActorContext, ActorRole, EventKind, RunId, RunStatus};

/// How long a look waits for the broker's health.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the broker waits for one request or for writing its answer.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the broker waits for the queue service's answer: as long as a
/// client-mode `dagq` waits for it.
const SERVICE_TIMEOUT: Duration = super::queue_service::CLIENT_TIMEOUT;
/// How often the broker looks whether its queue is still there.
const QUEUE_CHECK: Duration = Duration::from_secs(2);
/// The longest request line or header line the broker reads.
const MAX_LINE_BYTES: u64 = 8 * 1024;
/// The most headers the broker reads of one request.
const MAX_HEADERS: usize = 64;

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
/// the broker reads ([`AUTHORIZATION_HEADER`], [`RUN_HEADER`], `Host`,
/// `Content-Length`) and a body of up to [`MAX_BODY_BYTES`], whose length
/// `Content-Length` gives (a `Transfer-Encoding` is refused). A request
/// that does not read comes back with what was read of it.
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
        match name.trim().to_ascii_lowercase().as_str() {
            AUTHORIZATION_HEADER => request.authorization = Some(value),
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
                Ok(read) if read <= MAX_BODY_BYTES => length = read,
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
    let mut stream = TcpStream::connect(address)
        .with_context(|| format!("the queue broker at {address} is unreachable"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
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
    let mut answer = Vec::new();
    match length {
        Some(length) => reader.take(length).read_to_end(&mut answer)?,
        None => reader.read_to_end(&mut answer)?,
    };
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
    ensure!(
        status == 200,
        "the queue broker answered its health with {status}"
    );
    serde_json::from_slice(&body).context("the queue broker's health does not read")
}

// --- The broker -----------------------------------------------------------------

/// What the broker runs its requests on, on this host: the queue service's
/// tokens of the queue, the queue's runs, and the service's socket.
pub struct SystemBrokerBackend {
    pub db: PathBuf,
    pub queue_dir: PathBuf,
    pub generators: Generators,
}

impl SystemBrokerBackend {
    /// The broker itself, as the records name it: a part of the control
    /// side.
    fn own_actor() -> ActorContext {
        ActorContext::instance(
            ActorRole::Supervisor,
            format_args!("queue-broker:{}", std::process::id()),
        )
    }

    fn queue(&self) -> Result<SqliteQueue> {
        Ok(SqliteQueue::open(&self.db)?
            .with_generators(self.generators.clone())
            .with_actor(Self::own_actor()))
    }
}

impl BrokerBackend for SystemBrokerBackend {
    fn principal(&self, token: &str) -> Result<Option<Principal>> {
        lookup(&self.queue_dir, token)
    }

    fn run_status(&self, id: &RunId) -> Result<Option<RunStatus>> {
        match RunReads::run(&self.queue()?, id) {
            Ok(run) => Ok(Some(run.status())),
            Err(error) if error.downcast_ref::<RunNotFound>().is_some() => Ok(None),
            Err(error) => Err(error),
        }
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
        EventStore::record_queue_event(&self.queue()?, EventKind::QueueBrokerDenied, payload)
            .map(drop)
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
        db: options.db.clone(),
        queue_dir: queue_dir.clone(),
        generators: options.generators.clone(),
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
                let build = options.build.clone();
                thread::spawn(move || {
                    if let Err(error) = answer(stream, &*backend, &build, pid) {
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
    if BrokerRecord::read(&queue_dir).is_some_and(|record| record.pid == pid) {
        let _ = fs::remove_file(dir.join(STATE_FILE));
    }
    info!(pid, outcome, "the queue broker stopped: {outcome}");
    drop(lock);
    Ok(json!({"outcome": outcome, "pid": pid, "served": served}))
}

/// Read one request from `stream`, answer it, and close.
fn answer(stream: TcpStream, backend: &dyn BrokerBackend, build: &str, pid: u32) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let broker = QueueBroker {
        backend,
        build,
        pid,
    };
    let mut reader = BufReader::new(stream.try_clone()?);
    // A connection closed before it sent anything (a look whether the port
    // is open) is no request, and nothing is recorded of it.
    if reader.fill_buf()?.is_empty() {
        return Ok(());
    }
    let answer = match read_request(&mut reader) {
        Ok(request) => broker.handle(&request),
        Err((read, error)) => broker.malformed(&read, &format!("{error:#}")),
    };
    write_answer(&mut &stream, &answer)?;
    Ok(())
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
