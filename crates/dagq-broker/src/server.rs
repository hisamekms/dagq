//! The HTTP server of `dagq-broker serve`: health without a token, every
//! other request behind a verified, active token, default deny on what it
//! does not know, and one audit line per request.

use std::fs;
use std::io::{self, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dagq_broker_protocol::{
    BUILD_HEADER, BrokerError, BrokerRequestId, BrokerSessionToken, ErrorBody, ErrorCode,
    Operation, PROTOCOL_HEADER, PROTOCOL_VERSION, SigningKey, TokenClaims, check_active, encode,
    verify,
};
use sha2::{Digest, Sha256};

use crate::audit::{AuditLog, AuditRecord, rfc3339};
use crate::backend::{BackendKind, BackendRequest, Backends, Call};
use crate::config::{Config, Limits};
use crate::http::{self, ReadError, Response};

/// How long the server waits for a client to send its whole request, and
/// to take the response.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// The most connections served at once; more are closed unanswered.
pub const MAX_CONNECTIONS: usize = 32;

/// A bound server, not yet serving.
#[derive(Debug)]
pub struct Server {
    listener: TcpListener,
    handler: Arc<Handler>,
    open: Arc<AtomicUsize>,
}

impl Server {
    /// Read the key, open the audit (removing the day files past the
    /// retention) and bind `config.listen`.
    pub fn bind(config: &Config, backends: Backends) -> Result<Self, String> {
        let key = fs::read(&config.key)
            .map_err(|error| format!("read the key {}: {error}", config.key.display()))
            .and_then(|bytes| {
                SigningKey::from_bytes(&bytes)
                    .map_err(|error| format!("the key {}: {error}", config.key.display()))
            })?;
        let audit = AuditLog::open(&config.audit)
            .map_err(|error| format!("open the audit {}: {error}", config.audit.display()))?;
        audit
            .prune(SystemTime::now())
            .map_err(|error| format!("prune the audit {}: {error}", config.audit.display()))?;
        let listener = TcpListener::bind(config.listen)
            .map_err(|error| format!("listen on {}: {error}", config.listen))?;
        Ok(Self {
            listener,
            handler: Arc::new(Handler {
                key,
                active: config.active.clone(),
                roots: config.roots.clone(),
                limits: config.limits.clone(),
                backends,
                audit,
            }),
            open: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// The address it listens on (the port chosen for port 0).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serve until the process ends, one thread per connection and at most
    /// [`MAX_CONNECTIONS`] at once.
    pub fn serve(self) {
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    if self.open.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                        self.open.fetch_sub(1, Ordering::SeqCst);
                        drop(stream);
                        continue;
                    }
                    let handler = Arc::clone(&self.handler);
                    let open = Arc::clone(&self.open);
                    std::thread::spawn(move || {
                        handler.connection(stream);
                        open.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                Err(error) => {
                    eprintln!("{}: accept: {error}", crate::NAME);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

/// Answers requests; shared by the connections.
#[derive(Debug)]
pub struct Handler {
    key: SigningKey,
    active: PathBuf,
    roots: Vec<PathBuf>,
    limits: Limits,
    backends: Backends,
    audit: AuditLog,
}

/// A refusal on the way: the code and a message for the client.
type Refusal = (ErrorCode, String);

impl Handler {
    fn connection(&self, stream: TcpStream) {
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        let request = http::read_request(Deadline {
            stream: &stream,
            deadline: Instant::now() + IO_TIMEOUT,
        });
        let response = match request {
            Ok(request) => self.answer(&request),
            Err(ReadError::Bad(reason)) => self.refuse_unread(reason),
            Err(ReadError::Incomplete) => self.refuse_unread("the request is incomplete"),
            // Not a request: the peer sent nothing.
            Err(ReadError::Empty) => return,
        };
        let _ = http::write_response(&stream, &response);
    }

    /// Answer `request` and write its audit line before the answer goes.
    pub fn answer(&self, request: &http::Request) -> Response {
        let started = Instant::now();
        let at = SystemTime::now();
        let request_id = new_request_id();
        let mut record = AuditRecord {
            ts: rfc3339(at),
            request_id: request_id.to_string(),
            bytes_in: request.body.len() as u64,
            ..AuditRecord::default()
        };
        let (status, body) = match self.decide(request, at, &mut record) {
            Ok((body, exit_code)) => {
                record.result = "ok".to_owned();
                record.exit_code = exit_code;
                (200, body)
            }
            Err((code, message)) => {
                record.result = code.as_str().to_owned();
                (code.http_status(), error_body(code, message, &request_id))
            }
        };
        record.bytes_out = body.len() as u64;
        record.duration_ms = started.elapsed().as_millis() as u64;
        self.audited(at, &record, &request_id, response(status, body))
    }

    /// A request that could not be read: `invalid_request`, audited without
    /// an operation.
    fn refuse_unread(&self, reason: &str) -> Response {
        let at = SystemTime::now();
        let request_id = new_request_id();
        let code = ErrorCode::InvalidRequest;
        let body = error_body(code, reason.to_owned(), &request_id);
        let record = AuditRecord {
            ts: rfc3339(at),
            request_id: request_id.to_string(),
            result: code.as_str().to_owned(),
            bytes_out: body.len() as u64,
            ..AuditRecord::default()
        };
        self.audited(at, &record, &request_id, response(code.http_status(), body))
    }

    /// Write `record`, then give `answer`; when the line cannot be written,
    /// say so to stderr and answer `backend_error` instead (nothing goes
    /// back unaudited).
    fn audited(
        &self,
        at: SystemTime,
        record: &AuditRecord,
        request_id: &BrokerRequestId,
        answer: Response,
    ) -> Response {
        match self.audit.append(at, record) {
            Ok(()) => answer,
            Err(error) => {
                eprintln!(
                    "{}: write the audit line of request {request_id}: {error}",
                    crate::NAME
                );
                let code = ErrorCode::BackendError;
                let message = "the audit could not be written".to_owned();
                response(code.http_status(), error_body(code, message, request_id))
            }
        }
    }

    /// The decision on `request`, filling `record` as it learns who asks
    /// and what for. Who asks comes from the verified token only.
    fn decide(
        &self,
        request: &http::Request,
        at: SystemTime,
        record: &mut AuditRecord,
    ) -> Result<(Vec<u8>, Option<i32>), Refusal> {
        if request.method == "GET" && request.path == Operation::Health.path() {
            record.op = Some(Operation::Health.name().to_owned());
            let health = encode(&crate::health()).map_err(backend_error)?;
            return Ok((health, None));
        }

        // Default deny: nothing past this point without a valid token, not
        // even the answer that a path is unknown.
        let claims = self.authenticate(request, at, record)?;

        let routed = Operation::route(&request.method, &request.path).and_then(|operation| {
            Some((
                operation,
                BackendKind::of(operation)?,
                operation.capability()?,
            ))
        });
        let Some((operation, kind, capability)) = routed else {
            return Err((ErrorCode::InvalidRequest, "no such operation".to_owned()));
        };
        record.op = Some(operation.name().to_owned());
        record.backend = Some(kind.name().to_owned());
        record.capability = Some(capability.as_str().to_owned());

        match request.header(&PROTOCOL_HEADER.to_ascii_lowercase()) {
            Some(version) if version == PROTOCOL_VERSION.to_string() => {}
            Some(_) => {
                return Err((
                    ErrorCode::InvalidRequest,
                    format!("this broker speaks protocol {PROTOCOL_VERSION}"),
                ));
            }
            None => {
                return Err((
                    ErrorCode::InvalidRequest,
                    format!("the {PROTOCOL_HEADER} header is missing"),
                ));
            }
        }

        claims
            .require(capability)
            .map_err(|error| (error.code(), error.to_string()))?;

        let decoded = BackendRequest::decode(operation, &request.body).ok_or_else(|| {
            (
                ErrorCode::InvalidRequest,
                format!("the body is not a {operation} request"),
            )
        })?;
        if let Some(argv) = decoded.argv() {
            record.program = argv.first().map(|program| basename(program));
            record.argc = Some(argv.len());
            record.argv_sha256 = Some(arguments_sha256(argv));
        }

        let workspace = Path::new(&claims.workspace);
        let plain = workspace
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)));
        if !plain || !self.roots.iter().any(|root| workspace.starts_with(root)) {
            return Err((
                ErrorCode::WorkspaceViolation,
                "the token's workspace is not under a mounted root".to_owned(),
            ));
        }
        let confined = decoded
            .paths()
            .into_iter()
            .map(|path| claims.confine(Path::new(path)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| (error.code(), error.to_string()))?;
        if let [path] = confined.as_slice() {
            record.path = Some(relative(workspace, path));
        }

        let call = Call {
            operation,
            claims: &claims,
            confined,
            limits: &self.limits,
        };
        match self.backends.get(kind).call(&call, decoded) {
            Ok(done) => Ok((done.body, done.exit_code)),
            Err(failure) => {
                record.exit_code = failure.exit_code;
                Err((failure.code, failure.message))
            }
        }
    }

    /// The claims of the request's token: its format, signature, claims and
    /// expiry, then its active mark. Claims that did not verify never reach
    /// `record`.
    fn authenticate(
        &self,
        request: &http::Request,
        at: SystemTime,
        record: &mut AuditRecord,
    ) -> Result<TokenClaims, Refusal> {
        let unauthorized = |message: &str| (ErrorCode::Unauthorized, message.to_owned());
        let header = request
            .header("authorization")
            .ok_or_else(|| unauthorized("no token"))?;
        let token = match header.split_once(' ') {
            Some((scheme, token)) if scheme.eq_ignore_ascii_case("bearer") => token.trim(),
            _ => {
                return Err(unauthorized(
                    "the Authorization header is not a bearer token",
                ));
            }
        };
        let now = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let claims = verify(&self.key, &BrokerSessionToken::new(token), now)
            .map_err(|error| (error.code(), error.to_string()))?;
        record.jti = Some(claims.jti.clone());
        record.run_id = Some(claims.run_id.clone());
        record.task_id = Some(claims.task_id);
        record.actor_id = Some(claims.actor_id.clone());
        check_active(&claims, &self.active).map_err(|error| (error.code(), error.to_string()))?;
        Ok(claims)
    }
}

/// The connection's stream, read before a deadline for the whole request:
/// each read waits at most until the deadline, and none starts after it.
struct Deadline<'a> {
    stream: &'a TcpStream,
    deadline: Instant,
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(left))?;
        (&mut &*self.stream).read(buf)
    }
}

fn new_request_id() -> BrokerRequestId {
    BrokerRequestId::new(uuid::Uuid::new_v4().to_string())
}

fn backend_error(error: impl std::fmt::Display) -> Refusal {
    (ErrorCode::BackendError, error.to_string())
}

fn error_body(code: ErrorCode, message: String, request_id: &BrokerRequestId) -> Vec<u8> {
    let body = ErrorBody {
        error: BrokerError::new(code, message, request_id.clone()),
    };
    encode(&body).expect("an error body serializes")
}

fn response(status: u16, body: Vec<u8>) -> Response {
    Response {
        status,
        headers: vec![
            (PROTOCOL_HEADER, PROTOCOL_VERSION.to_string()),
            (BUILD_HEADER, crate::BUILD.to_owned()),
        ],
        body,
    }
}

fn basename(program: &str) -> String {
    Path::new(program)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// SHA-256 of the arguments after `argv[0]`, each followed by a NUL byte,
/// in lower-case hex.
fn arguments_sha256(argv: &[String]) -> String {
    let mut hasher = Sha256::new();
    for argument in argv.iter().skip(1) {
        hasher.update(argument.as_bytes());
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `path` relative to `workspace`, `.` for the workspace itself.
fn relative(workspace: &Path, path: &Path) -> String {
    match path.strip_prefix(workspace) {
        Ok(rest) if rest.as_os_str().is_empty() => ".".to_owned(),
        Ok(rest) => rest.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn helpers() {
        assert_eq!(basename("/usr/bin/ls"), "ls");
        assert_eq!(basename("ls"), "ls");
        assert_eq!(basename(""), "");
        let argv = |args: &[&str]| args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            arguments_sha256(&argv(&["ls"])),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_ne!(
            arguments_sha256(&argv(&["ls", "ab", "c"])),
            arguments_sha256(&argv(&["ls", "a", "bc"]))
        );
        assert_eq!(
            arguments_sha256(&argv(&["ls", "-l"])),
            arguments_sha256(&argv(&["/bin/ls", "-l"]))
        );
        let workspace = Path::new("/r/w");
        assert_eq!(relative(workspace, Path::new("/r/w")), ".");
        assert_eq!(relative(workspace, Path::new("/r/w/a/b")), "a/b");
        assert_eq!(relative(workspace, Path::new("/x")), "/x");
        assert_eq!(
            backend_error("boom"),
            (ErrorCode::BackendError, "boom".to_owned())
        );
    }

    #[test]
    fn no_read_starts_after_the_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (&client).write_all(b"x").unwrap();
        let mut reader = Deadline {
            stream: &server,
            deadline: Instant::now() + IO_TIMEOUT,
        };
        let mut buf = [0; 1];
        assert_eq!(reader.read(&mut buf).unwrap(), 1);
        reader.deadline = Instant::now();
        let error = reader.read(&mut buf).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
