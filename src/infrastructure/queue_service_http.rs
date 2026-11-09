//! The queue service's HTTP entrance (ADR-t3990-1 decisions 1 to 3): one
//! use case's request in one HTTP request, one answer, then the connection
//! closes. It carries, limits the sizes and reads the authentication header;
//! the checks, the use cases and the policy are the socket's
//! ([`crate::application::queue_service::QueueService`] with
//! [`Peer::Absent`](crate::application::queue_service::Peer::Absent)), reached
//! through the queue service's shared backend. TLS normally ends on the
//! deployment's side; `deployment_config` can supply server TLS here too.
//!
//! - The request is `POST` [`USE_CASE_PATH`]`<use case>`, with the socket's
//!   request line as its body. The path's name and the body's must be the
//!   same ([`carried_request`]): the name is what a proxy and a log see.
//! - Human login is `POST /api/use-case/exchange_token`, whose matching
//!   body has `params: {"jwt": "<issuer JWT>"}`. This is the only JWT
//!   entrance and requires no bearer token. The HTTP composition verifies
//!   before joining the write line; the socket refuses this use case.
//! - A token is read from `Authorization: Bearer` alone ([`bearer_of`]). The
//!   query is cut from the path unread, cookies are not looked at, and a
//!   `token` or `env_role` of the body goes before the body is read.
//! - A `queue` of the body goes too, so the request names no queue, as a
//!   worker's does. The URL says which service is meant, and a path means
//!   nothing to a caller elsewhere. The service compares that path before
//!   the token, so carrying it would let a caller with no token try paths
//!   on the host: `wrong_queue` for any other, `unauthenticated` for the
//!   queue's own.
//! - `X-Forwarded-For` and `X-Forwarded-Proto` are copied to the log line
//!   ([`access_line`]) and to nothing else: no value of them reaches
//!   [`carried_request`].
//! - The head is limited by [`MAX_HEAD_BYTES`] and the body by
//!   [`MAX_REQUEST_BYTES`], judged from `Content-Length` before any more of
//!   the body is read, so a refused body stays unread and the close may reset
//!   a client still sending it. `Transfer-Encoding` is refused. The answer's
//!   limit is the socket's, applied by the service. A client sending
//!   `Expect: 100-continue` is told to go on only after these checks.
//! - The status follows the answer's code ([`status_of`]); the body is always
//!   the socket's answer, which holds the API version range. An answer the
//!   service could not give closes the connection unanswered, as on the
//!   socket: the caller reads it as maybe done.

use std::cell::RefCell;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tracing::{info, warn};

use crate::domain::queue_service::{
    MAX_REQUEST_BYTES, ServiceErrorCode, ServiceRequest, ServiceResponse, answers,
};

/// Where a use case's request goes: its name follows.
pub const USE_CASE_PATH: &str = "/api/use-case/";
/// The longest head (request line and headers) read of one request.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;
/// The most headers read of one request.
const MAX_HEADERS: usize = 64;
/// How long one request may take to arrive, however slowly it drips.
const READ_LIMIT: Duration = Duration::from_secs(30);
/// How long the writing of an answer may stall.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest a forwarded header's value is copied to the log.
const MAX_FORWARDED_CHARS: usize = 256;

/// What the service gave a request: the answer's line, and the code of the
/// refusal the line says (`None` for a success).
pub struct Answered {
    pub line: Vec<u8>,
    pub code: Option<ServiceErrorCode>,
}

/// How a carried request is answered. An error leaves it unanswered.
pub type HttpAnswerer = Arc<dyn Fn(ServiceRequest) -> Result<Answered> + Send + Sync>;

/// A refusal the HTTP layer gives itself, which reaches no use case.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub status: u16,
    pub response: Box<ServiceResponse>,
}

fn refusal(status: u16, code: ServiceErrorCode, message: impl Into<String>) -> Refusal {
    Refusal {
        status,
        response: Box::new(ServiceResponse::failure(code, message)),
    }
}

/// What is read of a request's head. It holds a token, so it has no `Debug`.
pub struct Head {
    method: String,
    /// The path without its query, which nothing reads.
    path: String,
    bearer: Option<String>,
    length: Result<usize, Refusal>,
    expects_continue: bool,
    forwarded_for: Option<String>,
    forwarded_proto: Option<String>,
}

/// What the bytes read so far say of the head.
pub enum HeadStep {
    /// It has not ended yet.
    More,
    /// It ended after this many bytes.
    Read(Box<Head>, usize),
    Refused(Refusal),
}

/// The head in `bytes`, of which more may come.
pub fn head_step(bytes: &[u8]) -> HeadStep {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut request = httparse::Request::new(&mut headers);
    match request.parse(bytes) {
        Ok(httparse::Status::Complete(end)) if end <= MAX_HEAD_BYTES => {
            let target = request.path.unwrap_or_default();
            HeadStep::Read(
                Box::new(Head {
                    method: request.method.unwrap_or_default().to_owned(),
                    path: target.split('?').next().unwrap_or_default().to_owned(),
                    bearer: bearer_of(request.headers),
                    length: body_length(request.headers),
                    expects_continue: value_of(request.headers, "expect")
                        .is_some_and(|value| value.eq_ignore_ascii_case(b"100-continue")),
                    forwarded_for: forwarded(request.headers, "x-forwarded-for"),
                    forwarded_proto: forwarded(request.headers, "x-forwarded-proto"),
                }),
                end,
            )
        }
        Ok(httparse::Status::Partial) if bytes.len() <= MAX_HEAD_BYTES => HeadStep::More,
        Ok(_) | Err(httparse::Error::TooManyHeaders) => HeadStep::Refused(refusal(
            431,
            ServiceErrorCode::RequestTooLarge,
            format!(
                "the request's head is past the {MAX_HEAD_BYTES} bytes or {MAX_HEADERS} headers \
                 the queue service reads"
            ),
        )),
        Err(error) => HeadStep::Refused(refusal(
            400,
            ServiceErrorCode::BadRequest,
            format!("the request's head does not read: {error}"),
        )),
    }
}

fn value_of<'a>(headers: &[httparse::Header<'a>], name: &str) -> Option<&'a [u8]> {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value)
}

/// The token of the one `Authorization` header in the `Bearer` form: none for
/// no such header, two of them, another scheme or an empty token.
pub fn bearer_of(headers: &[httparse::Header]) -> Option<String> {
    let mut named = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("authorization"));
    let header = named.next()?;
    if named.next().is_some() {
        return None;
    }
    let (scheme, token) = std::str::from_utf8(header.value).ok()?.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer")
        && !token.is_empty()
        && !token.contains(char::is_whitespace))
    .then(|| token.to_owned())
}

/// How long the body is, up to [`MAX_REQUEST_BYTES`]: its one `Content-Length`
/// in plain digits. A request that gives anything else is refused here, so
/// no more of its body is read.
pub fn body_length(headers: &[httparse::Header]) -> Result<usize, Refusal> {
    if value_of(headers, "transfer-encoding").is_some() {
        return Err(refusal(
            400,
            ServiceErrorCode::BadRequest,
            "a request gives its body's Content-Length, not a Transfer-Encoding",
        ));
    }
    let mut lengths = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("content-length"));
    let Some(header) = lengths.next() else {
        return Err(refusal(
            411,
            ServiceErrorCode::BadRequest,
            "a request gives its body's Content-Length",
        ));
    };
    let does_not_read = || {
        refusal(
            400,
            ServiceErrorCode::BadRequest,
            "the request's Content-Length does not read",
        )
    };
    if lengths.next().is_some() || !header.value.iter().all(u8::is_ascii_digit) {
        return Err(does_not_read());
    }
    let length = std::str::from_utf8(header.value)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(does_not_read)?;
    if length > MAX_REQUEST_BYTES {
        return Err(refusal(
            413,
            ServiceErrorCode::RequestTooLarge,
            format!(
                "the request is {length} bytes, past the {MAX_REQUEST_BYTES} bytes the queue \
                 service reads"
            ),
        ));
    }
    Ok(length)
}

fn forwarded(headers: &[httparse::Header], name: &str) -> Option<String> {
    value_of(headers, name).map(|value| {
        String::from_utf8_lossy(value)
            .chars()
            .take(MAX_FORWARDED_CHARS)
            .collect()
    })
}

/// The use case `head` names and its body's length, or why it reaches none.
fn routed(head: &Head) -> Result<(&str, usize), Refusal> {
    let name = head
        .path
        .strip_prefix(USE_CASE_PATH)
        .filter(|name| !name.is_empty() && !name.contains('/'))
        .ok_or_else(|| {
            refusal(
                404,
                ServiceErrorCode::BadRequest,
                format!("a use case's request goes to {USE_CASE_PATH}<use case>"),
            )
        })?;
    if head.method != "POST" {
        return Err(refusal(
            405,
            ServiceErrorCode::BadRequest,
            "a use case's request is a POST",
        ));
    }
    Ok((name, head.length.clone()?))
}

/// The service's request of the `body` carried to the use case `name` with
/// the header's `bearer`, whatever the body says of a token. A version out of
/// the range is told first, also for a use case of a version this build does
/// not know, which is how a newer caller learns the range.
pub fn carried_request(
    name: &str,
    bearer: Option<&str>,
    body: &[u8],
) -> Result<ServiceRequest, Refusal> {
    let unread = |error: &dyn std::fmt::Display| {
        refusal(
            400,
            ServiceErrorCode::BadRequest,
            format!("the request does not read: {error}"),
        )
    };
    let mut read: Value = serde_json::from_slice(body).map_err(|error| unread(&error))?;
    let fields = read
        .as_object_mut()
        .ok_or_else(|| unread(&"it is not an object"))?;
    for unread_field in ["token", "env_role", "queue"] {
        fields.remove(unread_field);
    }
    if let Some(version) = fields.get("api_version").and_then(Value::as_u64)
        && !u32::try_from(version).is_ok_and(answers)
    {
        return Err(Refusal {
            status: status_of(Some(ServiceErrorCode::ApiVersionMismatch)),
            response: Box::new(ServiceResponse::version_mismatch(version)),
        });
    }
    if fields.get("use_case").and_then(Value::as_str) != Some(name) {
        return Err(refusal(
            400,
            ServiceErrorCode::BadRequest,
            format!("the path names the use case {name}, and the body does not"),
        ));
    }
    let mut request: ServiceRequest =
        serde_json::from_value(read).map_err(|error| unread(&error))?;
    request.token = bearer.map(str::to_owned);
    Ok(request)
}

/// The status of an answer of `code`. The body says the rest.
pub fn status_of(code: Option<ServiceErrorCode>) -> u16 {
    use ServiceErrorCode as Code;
    match code {
        None => 200,
        Some(Code::ApiVersionMismatch | Code::BadRequest | Code::WrongQueue) => 400,
        Some(Code::Unauthenticated) => 401,
        Some(Code::AuthorizationDenied | Code::ForbiddenDestination) => 403,
        Some(Code::RequestTooLarge) => 413,
        // The use case's own refusal (a missing task, a bad option), which no
        // proxy should count as the service failing.
        Some(Code::Failed) => 422,
        Some(Code::Unreachable) => 502,
        Some(Code::NotLeader | Code::Busy) => 503,
        Some(Code::ResponseTooLarge) => 500,
    }
}

/// The HTTP response of `status` with `body`, closing the connection.
fn response_bytes(status: u16, body: &[u8]) -> Vec<u8> {
    let (reason, extra) = match status {
        200 => ("OK", ""),
        400 => ("Bad Request", ""),
        401 => ("Unauthorized", "WWW-Authenticate: Bearer\r\n"),
        403 => ("Forbidden", ""),
        404 => ("Not Found", ""),
        405 => ("Method Not Allowed", "Allow: POST\r\n"),
        411 => ("Length Required", ""),
        413 => ("Content Too Large", ""),
        422 => ("Unprocessable Content", ""),
        431 => ("Request Header Fields Too Large", ""),
        500 => ("Internal Server Error", ""),
        502 => ("Bad Gateway", ""),
        503 => ("Service Unavailable", ""),
        _ => ("Unknown", ""),
    };
    let mut out = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n{extra}Content-Length: \
         {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// The log's line of one request: what it reached and the forwarded headers,
/// quoted so a value breaks no line. No token, path or query is in it.
pub fn access_line(use_case: Option<&str>, status: u16, head: Option<&Head>) -> String {
    format!(
        "the queue service answered over HTTP: use_case={use_case:?} status={status} \
         forwarded_for={:?} forwarded_proto={:?}",
        head.and_then(|head| head.forwarded_for.as_deref()),
        head.and_then(|head| head.forwarded_proto.as_deref()),
    )
}

fn write_refusal(writer: &mut impl Write, refused: &Refusal) -> Result<()> {
    let mut body = serde_json::to_vec(&refused.response)?;
    body.push(b'\n');
    writer.write_all(&response_bytes(refused.status, &body))?;
    Ok(())
}

/// Read one request from `reader`, answer it with `answer` on `writer`, and
/// return the log's line of it. An error leaves it unanswered.
pub fn carry(
    reader: &mut impl Read,
    writer: &mut impl Write,
    answer: &(dyn Fn(ServiceRequest) -> Result<Answered> + Send + Sync),
) -> Result<String> {
    let mut read = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head, end) = loop {
        match head_step(&read) {
            HeadStep::More => {}
            HeadStep::Read(head, end) => break (head, end),
            HeadStep::Refused(refused) => {
                write_refusal(writer, &refused)?;
                return Ok(access_line(None, refused.status, None));
            }
        }
        let count = match reader.read(&mut chunk) {
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("read the request's head"),
        };
        if count == 0 {
            bail!("the connection ended before the request's head did");
        }
        read.extend_from_slice(&chunk[..count]);
    };
    let (name, length) = match routed(&head) {
        Ok(routed) => routed,
        Err(refused) => {
            write_refusal(writer, &refused)?;
            return Ok(access_line(None, refused.status, Some(&head)));
        }
    };
    let mut body = read.split_off(end);
    if body.len() < length {
        if head.expects_continue {
            writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            writer.flush()?;
        }
        reader
            .take((length - body.len()) as u64)
            .read_to_end(&mut body)
            .context("read the request's body")?;
    }
    if body.len() < length {
        bail!("the connection ended before the request's body did");
    }
    body.truncate(length);
    let request = match carried_request(name, head.bearer.as_deref(), &body) {
        Ok(request) => request,
        Err(refused) => {
            write_refusal(writer, &refused)?;
            return Ok(access_line(None, refused.status, Some(&head)));
        }
    };
    let use_case = request.use_case;
    let answered = answer(request)?;
    let status = status_of(answered.code);
    writer.write_all(&response_bytes(status, &answered.line))?;
    Ok(access_line(Some(use_case.as_str()), status, Some(&head)))
}

/// A connection's reads, which all end by one deadline: a request that drips
/// holds no thread past [`READ_LIMIT`].
struct UntilDeadline<'a> {
    stream: &'a TcpStream,
    deadline: Instant,
}

impl Read for UntilDeadline<'_> {
    fn read(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(left))?;
        (&mut &*self.stream).read(into)
    }
}

impl Write for UntilDeadline<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        (&mut &*self.stream).write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        (&mut &*self.stream).flush()
    }
}

/// The two sequential halves of `carry` share a bidirectional TLS stream.
/// Each borrow ends with its I/O call; no borrow spans a read and a write.
struct SharedConnection<'a, T>(&'a RefCell<T>);

impl<T: Read> Read for SharedConnection<'_, T> {
    fn read(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().read(into)
    }
}

impl<T: Write> Write for SharedConnection<'_, T> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

fn serve_connection(
    stream: &TcpStream,
    answer: &HttpAnswerer,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> Result<String> {
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let mut reader = UntilDeadline {
        stream,
        deadline: Instant::now() + READ_LIMIT,
    };
    if let Some(config) = tls {
        let connection = rustls::ServerConnection::new(config)?;
        let stream = RefCell::new(rustls::StreamOwned::new(connection, reader));
        // Read and write the same TLS connection, including interim responses
        // before a client waiting for 100 Continue sends its request body.
        let result = carry(
            &mut SharedConnection(&stream),
            &mut SharedConnection(&stream),
            answer.as_ref(),
        );
        let mut stream = stream.into_inner();
        stream.conn.send_close_notify();
        stream.flush()?;
        result
    } else {
        carry(&mut reader, &mut &*stream, answer.as_ref())
    }
}

/// Accepted connections, held through reading, dispatch and the final write.
/// Stopping acceptance does not discard their replies. All waits share the
/// service's one drain deadline, so slow peers cannot multiply its grace.
#[derive(Clone, Default)]
pub(super) struct InFlight(Arc<(Mutex<usize>, Condvar)>);

pub(super) struct InFlightRequest(InFlight);

impl InFlight {
    pub(super) fn begin(&self) -> InFlightRequest {
        *self.0.0.lock().unwrap() += 1;
        InFlightRequest(self.clone())
    }

    pub(super) fn wait(&self, limit: Duration) -> bool {
        let count = self.0.0.lock().unwrap();
        let (count, _) = self
            .0
            .1
            .wait_timeout_while(count, limit, |count| *count != 0)
            .unwrap();
        *count == 0
    }
}

impl Drop for InFlightRequest {
    fn drop(&mut self) {
        let (count, changed) = &*self.0.0;
        *count.lock().unwrap() -= 1;
        changed.notify_all();
    }
}

/// An HTTP listener with accepted requests tracked through their final write.
/// The service stops acceptance, then drains these requests before its line.
pub struct HttpEntrance {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    requests: InFlight,
}

/// Take requests on `listener` with `answer`, looking for a stop every `poll`.
pub fn open(listener: TcpListener, answer: HttpAnswerer, poll: Duration) -> Result<HttpEntrance> {
    open_tls(listener, answer, poll, None)
}

/// Serve plaintext or TLS as decided before binding the listener.
pub fn open_tls(
    listener: TcpListener,
    answer: HttpAnswerer,
    poll: Duration,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> Result<HttpEntrance> {
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let requests = InFlight::default();
    let accepted = requests.clone();
    let thread = thread::Builder::new()
        .name("queue-service-http".to_owned())
        .spawn(move || accept_until(&listener, &stopping, &answer, poll, tls, &accepted))
        .context("start the queue service's HTTP entrance")?;
    Ok(HttpEntrance {
        stop,
        thread: Some(thread),
        requests,
    })
}

impl HttpEntrance {
    pub(super) fn stop_accepting(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    pub(super) fn wait(&self, limit: Duration) -> bool {
        self.requests.wait(limit)
    }
}

impl Drop for HttpEntrance {
    fn drop(&mut self) {
        self.stop_accepting();
    }
}

fn accept_until(
    listener: &TcpListener,
    stop: &AtomicBool,
    answer: &HttpAnswerer,
    poll: Duration,
    tls: Option<Arc<rustls::ServerConfig>>,
    requests: &InFlight,
) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let answer = answer.clone();
                let tls = tls.clone();
                let request = requests.begin();
                let started = thread::Builder::new()
                    .name("queue-service-http-request".to_owned())
                    .spawn(move || {
                        let _request = request;
                        match serve_connection(&stream, &answer, tls) {
                            Ok(line) => info!("{line}"),
                            Err(error) => warn!("a queue service HTTP request failed: {error:#}"),
                        }
                    });
                if let Err(error) = started {
                    warn!("the queue service could not take an HTTP request: {error}");
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                super::queue_service::wait_for_connection(listener, poll);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => {
                warn!("the queue service could not accept over HTTP: {error}");
                thread::sleep(poll);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::queue_service::tests::{Backend, backend};
    use crate::application::queue_service::{Peer, QueueService};
    use crate::domain::RunStatus;
    use crate::domain::queue_service::{API_VERSION, MIN_API_VERSION, UseCase};
    use serde_json::json;

    #[test]
    fn accepted_connections_live_until_their_final_write_finishes() {
        let requests = InFlight::default();
        let first = requests.begin();
        let second = requests.begin();
        assert!(!requests.wait(Duration::ZERO));
        drop(first);
        assert!(!requests.wait(Duration::ZERO));
        drop(second);
        assert!(requests.wait(Duration::ZERO));
    }

    /// What a request made of: what the stub service was given, the bytes
    /// written, and the log's line.
    struct Carried {
        given: Vec<ServiceRequest>,
        written: String,
        line: String,
    }

    fn carried(raw: &[u8]) -> Carried {
        let given = Mutex::new(Vec::new());
        let mut written = Vec::new();
        let line = carry(&mut &raw[..], &mut written, &|request| {
            given.lock().unwrap().push(request);
            Ok(Answered {
                line: b"{\"answered\":true}\n".to_vec(),
                code: None,
            })
        })
        .unwrap();
        Carried {
            given: given.into_inner().unwrap(),
            written: String::from_utf8(written).unwrap(),
            line,
        }
    }

    fn body_of(use_case: &str) -> String {
        json!({"api_version": API_VERSION, "use_case": use_case, "params": {"id": 1}}).to_string()
    }

    fn post(target: &str, headers: &str, body: &str) -> Vec<u8> {
        format!(
            "POST {target} HTTP/1.1\r\nHost: x\r\n{headers}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn refusal_of(written: &str) -> (u16, String, Value) {
        let (head, body) = written.split_once("\r\n\r\n").unwrap();
        let body: Value = serde_json::from_str(body).unwrap();
        (
            head.split(' ').nth(1).unwrap().parse().unwrap(),
            body["error"]["code"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            body,
        )
    }

    #[test]
    fn one_use_case_goes_in_one_request_and_the_answer_comes_back_as_it_is() {
        let raw = post(
            "/api/use-case/show",
            "Authorization: Bearer tok\r\n",
            &body_of("show"),
        );
        let carried = carried(&raw);
        assert_eq!(carried.given.len(), 1);
        assert_eq!(carried.given[0].use_case, UseCase::Show);
        assert_eq!(carried.given[0].token.as_deref(), Some("tok"));
        assert_eq!(carried.given[0].params, json!({"id": 1}));
        assert!(carried.written.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(carried.written.contains("Connection: close\r\n"));
        assert!(carried.written.ends_with("\r\n\r\n{\"answered\":true}\n"));
    }

    #[test]
    fn a_path_that_names_another_use_case_than_the_body_is_a_bad_request() {
        for body in [
            body_of("note"),
            json!({"api_version": API_VERSION}).to_string(),
            "[]".to_owned(),
            "not json".to_owned(),
        ] {
            let carried = carried(&post("/api/use-case/show", "", &body));
            assert!(carried.given.is_empty(), "{body}");
            let (status, code, _) = refusal_of(&carried.written);
            assert_eq!((status, code.as_str()), (400, "bad_request"), "{body}");
        }
        // One the body and the path agree on and this build does not know:
        // the socket's refusal.
        let carried_unknown = carried(&post("/api/use-case/nope", "", &body_of("nope")));
        assert!(carried_unknown.given.is_empty());
        assert_eq!(refusal_of(&carried_unknown.written).1, "bad_request");
    }

    #[test]
    fn only_a_post_to_a_use_cases_path_reaches_the_service() {
        for (raw, want) in [
            (post("/", "", ""), 404),
            (post("/api/use-case/", "", ""), 404),
            (post("/api/use-case/show/x", "", ""), 404),
            (post("/healthz", "", ""), 404),
            (
                b"GET /api/use-case/show HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(),
                405,
            ),
            (b"\xffnot http\r\n\r\n".to_vec(), 400),
        ] {
            let carried = carried(&raw);
            assert!(carried.given.is_empty());
            assert_eq!(refusal_of(&carried.written).0, want);
        }
    }

    #[test]
    fn a_token_is_read_from_the_bearer_header_alone() {
        let body = json!({
            "api_version": API_VERSION,
            "use_case": "show",
            "token": "from-body",
            "env_role": "worker",
        })
        .to_string();
        // The query, a cookie, another header and the body carry none.
        let unread = post(
            "/api/use-case/show?token=from-query",
            "Cookie: token=from-cookie\r\nX-Token: from-header\r\n",
            &body,
        );
        let carried_unread = carried(&unread);
        assert_eq!(carried_unread.given[0].token, None);
        assert_eq!(carried_unread.given[0].env_role, None);
        // A token of the wrong type in the body does not change the refusal.
        let typed = json!({"api_version": API_VERSION, "use_case": "show", "token": 1});
        let carried_typed = carried(&post("/api/use-case/show", "", &typed.to_string()));
        assert_eq!(carried_typed.given[0].token, None);
        // Only the one header in the bearer form names a token, and it wins
        // over the body's.
        for (authorization, want) in [
            ("Authorization: Bearer good\r\n", Some("good")),
            ("authorization: bearer  good \r\n", Some("good")),
            ("Authorization: Basic good\r\n", None),
            ("Authorization: Bearer\r\n", None),
            ("Authorization: Bearer \r\n", None),
            ("Authorization: Bearer a b\r\n", None),
            ("Authorization: good\r\n", None),
            (
                "Authorization: Bearer a\r\nAuthorization: Bearer b\r\n",
                None,
            ),
        ] {
            let carried = carried(&post("/api/use-case/show", authorization, &body));
            assert_eq!(carried.given[0].token.as_deref(), want, "{authorization}");
        }
    }

    #[test]
    fn a_queue_named_in_the_body_is_not_carried() {
        for queue in [json!("/some/other/queue.db"), json!(1)] {
            let body = json!({"api_version": API_VERSION, "use_case": "show", "queue": queue});
            let carried = carried(&post("/api/use-case/show", "", &body.to_string()));
            assert_eq!(carried.given[0].queue, None);
        }
    }

    #[test]
    fn exchange_carries_a_jwt_without_bearer_and_logs_neither_credential() {
        let body = json!({"api_version": API_VERSION, "use_case": "exchange_token", "params": {"jwt": "secret.jwt.value"}}).to_string();
        let result = carried(&post("/api/use-case/exchange_token", "", &body));
        assert_eq!(result.given[0].use_case, UseCase::ExchangeToken);
        assert_eq!(result.given[0].params["jwt"], "secret.jwt.value");
        assert!(result.given[0].token.is_none());
        assert!(!result.line.contains("secret.jwt.value"));
    }

    #[test]
    fn no_token_value_reaches_the_log_line() {
        let raw = post(
            "/api/use-case/show?token=from-query",
            "Authorization: Bearer from-header\r\nCookie: from-cookie\r\n",
            &body_of("show"),
        );
        let refused = post(
            "/api/use-case/show?token=from-query",
            "Authorization: Bearer from-header\r\n",
            &body_of("note"),
        );
        for raw in [raw, refused] {
            let line = carried(&raw).line;
            assert!(!line.contains("from-"), "{line}");
        }
    }

    #[test]
    fn a_version_out_of_the_range_is_refused_with_the_range_before_any_token() {
        for version in [
            json!(MIN_API_VERSION - 1),
            json!(API_VERSION + 1),
            json!(u64::MAX),
        ] {
            // Also of a use case this build does not know and a path that
            // disagrees, which only a newer caller would send.
            let body = json!({"api_version": version, "use_case": "from_the_future"}).to_string();
            let carried = carried(&post("/api/use-case/show", "", &body));
            assert!(carried.given.is_empty());
            let (status, code, answer) = refusal_of(&carried.written);
            assert_eq!((status, code.as_str()), (400, "api_version_mismatch"));
            assert_eq!(answer["api_version"], API_VERSION);
            assert_eq!(answer["min_api_version"], MIN_API_VERSION);
        }
    }

    /// Gives `head` and then bytes without end, counting those it gave.
    struct Endless {
        head: Vec<u8>,
        given: usize,
    }

    impl Read for Endless {
        fn read(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
            let count = into.len().min(1024);
            for (at, byte) in into[..count].iter_mut().enumerate() {
                *byte = self.head.get(self.given + at).copied().unwrap_or(b'x');
            }
            self.given += count;
            Ok(count)
        }
    }

    fn refused_unread(head: String) -> (u16, String, usize) {
        let mut reader = Endless {
            head: head.into_bytes(),
            given: 0,
        };
        let mut written = Vec::new();
        carry(&mut reader, &mut written, &|_| {
            panic!("the service is not asked")
        })
        .unwrap();
        let (status, code, _) = refusal_of(&String::from_utf8(written).unwrap());
        (status, code, reader.given)
    }

    #[test]
    fn a_request_past_the_limit_is_refused_without_reading_it_through() {
        let past = MAX_REQUEST_BYTES + 1;
        let (status, code, read) = refused_unread(format!(
            "POST /api/use-case/show HTTP/1.1\r\nContent-Length: {past}\r\n\r\n"
        ));
        assert_eq!((status, code.as_str()), (413, "request_too_large"));
        assert!(read < 4096, "read {read}");
        // A head that never ends stops at its own limit.
        let (status, code, read) =
            refused_unread("POST /api/use-case/show HTTP/1.1\r\nA: ".to_owned());
        assert_eq!((status, code.as_str()), (431, "request_too_large"));
        assert!(read <= MAX_HEAD_BYTES + 1024, "read {read}");
        let headers = "A: b\r\n".repeat(MAX_HEADERS + 1);
        let (status, ..) = refused_unread(format!("POST / HTTP/1.1\r\n{headers}\r\n"));
        assert_eq!(status, 431);
        // At the limit it is read and goes to the service.
        let body = body_of("show");
        let padded = format!("{body}{}", " ".repeat(MAX_REQUEST_BYTES - body.len()));
        let at = carried(&post("/api/use-case/show", "", &padded));
        assert_eq!(at.given.len(), 1);
    }

    #[test]
    fn a_body_that_gives_no_plain_length_is_refused_unread() {
        for (headers, want) in [
            ("Transfer-Encoding: chunked\r\nContent-Length: 2\r\n", 400),
            ("Transfer-Encoding: chunked\r\n", 400),
            ("", 411),
            ("Content-Length: 2\r\nContent-Length: 2\r\n", 400),
            ("Content-Length: +2\r\n", 400),
            ("Content-Length: x\r\n", 400),
            ("Content-Length: 99999999999999999999999\r\n", 400),
        ] {
            let raw = format!("POST /api/use-case/show HTTP/1.1\r\n{headers}\r\n{{}}");
            let carried = carried(raw.as_bytes());
            assert!(carried.given.is_empty(), "{headers}");
            assert_eq!(refusal_of(&carried.written).0, want, "{headers}");
        }
    }

    #[test]
    fn a_connection_that_ends_early_is_left_unanswered() {
        let body = body_of("show");
        let whole = post("/api/use-case/show", "", &body);
        for raw in [&whole[..10], &whole[..whole.len() - 1]] {
            let mut written = Vec::new();
            let carrying = carry(&mut &raw[..], &mut written, &|_| {
                panic!("the service is not asked")
            });
            assert!(carrying.is_err());
            assert!(written.is_empty());
        }
    }

    #[test]
    fn a_signal_during_the_heads_read_loses_nothing() {
        struct InterruptedOnce<'a>(bool, &'a [u8]);
        impl Read for InterruptedOnce<'_> {
            fn read(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
                if std::mem::replace(&mut self.0, false) {
                    return Err(ErrorKind::Interrupted.into());
                }
                self.1.read(into)
            }
        }
        let raw = post("/api/use-case/show", "", &body_of("show"));
        let mut written = Vec::new();
        carry(&mut InterruptedOnce(true, &raw), &mut written, &|_| {
            Ok(Answered {
                line: b"{}".to_vec(),
                code: None,
            })
        })
        .unwrap();
        assert!(written.starts_with(b"HTTP/1.1 200"));
    }

    #[test]
    fn a_failed_answer_closes_with_nothing_written() {
        let raw = post("/api/use-case/show", "", &body_of("show"));
        let mut written = Vec::new();
        let carrying = carry(&mut &raw[..], &mut written, &|_| bail!("line stopped"));
        assert!(carrying.is_err());
        assert!(written.is_empty());
    }

    #[test]
    fn a_client_that_expects_to_be_told_goes_on_only_after_the_checks() {
        let body = body_of("show");
        let head = format!(
            "POST /api/use-case/show HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let told = carried(format!("{head}{body}").as_bytes());
        // All of it has come: nothing to wait for.
        assert!(told.written.starts_with("HTTP/1.1 200"));
        let mut written = Vec::new();
        carry(
            &mut head.as_bytes().chain(body.as_bytes()),
            &mut written,
            &|_| {
                Ok(Answered {
                    line: b"{}".to_vec(),
                    code: None,
                })
            },
        )
        .unwrap();
        assert!(written.starts_with(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200"));
        let past = format!(
            "POST /api/use-case/show HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: {}\r\n\r\n",
            MAX_REQUEST_BYTES + 1
        );
        assert!(carried(past.as_bytes()).written.starts_with("HTTP/1.1 413"));
    }

    #[test]
    fn the_forwarded_headers_reach_the_log_line_and_nothing_else() {
        let body = body_of("show");
        let plain = carried(&post(
            "/api/use-case/show",
            "Authorization: Bearer tok\r\n",
            &body,
        ));
        let forwarded = carried(&post(
            "/api/use-case/show",
            "X-Forwarded-For: 203.0.113.9\r\nX-Forwarded-Proto: http\r\nAuthorization: Bearer \
             tok\r\n",
            &body,
        ));
        assert_eq!(forwarded.given, plain.given);
        assert_eq!(forwarded.written, plain.written);
        assert!(forwarded.line.contains("203.0.113.9"), "{}", forwarded.line);
        assert!(forwarded.line.contains("\"http\""), "{}", forwarded.line);
        // A long value is cut, and a quote stays within the field.
        let long = "x".repeat(MAX_FORWARDED_CHARS + 10);
        let cut = carried(&post(
            "/api/use-case/show",
            &format!("X-Forwarded-For: \"{long}\r\n"),
            &body,
        ));
        assert!(!cut.line.contains(&long), "{}", cut.line);
        assert!(cut.line.contains("\\\""), "{}", cut.line);
    }

    /// `raw` carried to the service of `backend` with no peer: the status and
    /// the answer, with all that was written.
    fn through_the_service(backend: &Backend, raw: &[u8]) -> (u16, Value, String) {
        let service = QueueService {
            backend,
            build: "b",
            pid: 1,
        };
        let mut written = Vec::new();
        carry(&mut &raw[..], &mut written, &|request| {
            let response = service.handle_from(&request, Peer::Absent);
            Ok(Answered {
                line: serde_json::to_vec(&response)?,
                code: response.error.as_ref().map(|error| error.code),
            })
        })
        .unwrap();
        let written = String::from_utf8(written).unwrap();
        let (status, _, answer) = refusal_of(&written);
        (status, answer, written)
    }

    #[test]
    fn only_the_token_decides_and_no_forwarded_header_changes_the_answer() {
        let backend = backend(RunStatus::Running);
        let forwarded = [
            "",
            "X-Forwarded-For: 127.0.0.1\r\nX-Forwarded-Proto: https\r\n",
            "X-Forwarded-For: 203.0.113.9, 10.0.0.1\r\nX-Forwarded-Proto: http\r\n",
        ];
        let show = json!({"api_version": API_VERSION, "use_case": "show", "params": {"id": 1}});
        let supervisor = json!({"api_version": API_VERSION, "use_case": "supervisor",
            "params": {"op": "active_runs", "args": []}});
        let worker = "Authorization: Bearer worker\r\n";
        for (name, body, token, want) in [
            ("show", &show, worker, 200),
            ("supervisor", &supervisor, worker, 403),
            ("show", &show, "", 401),
        ] {
            let answers: Vec<_> = forwarded
                .iter()
                .map(|headers| {
                    let raw = post(
                        &format!("/api/use-case/{name}"),
                        &format!("{headers}{token}"),
                        &body.to_string(),
                    );
                    through_the_service(&backend, &raw).2
                })
                .collect();
            assert_eq!(refusal_of(&answers[0]).0, want, "{name}: {answers:?}");
            assert!(
                answers.iter().all(|answer| *answer == answers[0]),
                "{answers:?}"
            );
        }
        // Who writes is the token's, whatever is forwarded.
        let note = json!({"api_version": API_VERSION, "use_case": "note",
            "params": {"run": "r1", "text": "x"}});
        let raw = post(
            "/api/use-case/note",
            &format!("X-Forwarded-For: 127.0.0.1\r\n{worker}"),
            &note.to_string(),
        );
        through_the_service(&backend, &raw);
        assert_eq!(
            backend.log.lock().unwrap().pop().unwrap(),
            json!({"note": "x", "by": "worker"})
        );
    }

    #[test]
    fn a_token_anywhere_but_the_header_is_refused_and_no_value_is_kept() {
        let backend = backend(RunStatus::Running);
        let secret = "secret-value-of-a-token";
        let in_the_body = json!({"api_version": API_VERSION, "use_case": "show", "token": "worker",
            "params": {"id": 1}})
        .to_string();
        let show = body_of("show");
        for (target, headers, body) in [
            ("/api/use-case/show", "", in_the_body.as_str()),
            ("/api/use-case/show?token=worker", "", show.as_str()),
            (
                "/api/use-case/show",
                "Cookie: token=worker\r\n",
                show.as_str(),
            ),
        ] {
            let (status, answer, _) = through_the_service(&backend, &post(target, headers, body));
            assert_eq!(status, 401, "{target} {headers}: {answer}");
            assert_eq!(
                backend.log.lock().unwrap().pop().unwrap(),
                json!({"use_case": "show", "reason": "missing_token"})
            );
        }
        // One the service did not issue is refused, and neither the answer nor
        // the record has its value.
        let raw = post(
            "/api/use-case/show",
            &format!("Authorization: Bearer {secret}\r\n"),
            &show,
        );
        let (status, _, written) = through_the_service(&backend, &raw);
        assert_eq!(status, 401);
        assert!(!written.contains(secret), "{written}");
        assert_eq!(
            backend.log.lock().unwrap().pop().unwrap(),
            json!({"use_case": "show", "reason": "unknown_token"})
        );
    }

    #[test]
    fn no_path_tells_a_caller_anything_of_the_queue() {
        let backend = backend(RunStatus::Running);
        // The stub's own queue, and others it would refuse as `wrong_queue`.
        let paths = ["/q/queue.db", "/some/other/queue.db", "/etc/passwd"];
        let answers: Vec<_> = paths
            .iter()
            .map(|queue| {
                let body = json!({"api_version": API_VERSION, "use_case": "show", "queue": queue,
                    "params": {"id": 1}});
                let raw = post("/api/use-case/show", "", &body.to_string());
                through_the_service(&backend, &raw).2
            })
            .collect();
        assert_eq!(refusal_of(&answers[0]).0, 401, "{answers:?}");
        assert!(
            answers.iter().all(|answer| *answer == answers[0]),
            "{answers:?}"
        );
    }

    #[test]
    fn hello_asks_for_a_token_and_reads_its_run_beside_the_write_line() {
        let backend = backend(RunStatus::Running);
        let hello = body_of("hello");
        let raw = post("/api/use-case/hello", "", &hello);
        assert_eq!(through_the_service(&backend, &raw).0, 401);
        assert!(backend.opened.lock().unwrap().is_empty());
        let raw = post(
            "/api/use-case/hello",
            "Authorization: Bearer worker\r\n",
            &hello,
        );
        let (status, answer, _) = through_the_service(&backend, &raw);
        assert_eq!(status, 200, "{answer}");
        assert_eq!(answer["result"]["service"], "dagq-queue-service");
        assert_eq!(*backend.opened.lock().unwrap(), ["read"]);
    }

    #[test]
    fn the_status_follows_the_answers_code() {
        use ServiceErrorCode as Code;
        for (code, want) in [
            (None, 200),
            (Some(Code::ApiVersionMismatch), 400),
            (Some(Code::BadRequest), 400),
            (Some(Code::WrongQueue), 400),
            (Some(Code::Unauthenticated), 401),
            (Some(Code::AuthorizationDenied), 403),
            (Some(Code::ForbiddenDestination), 403),
            (Some(Code::RequestTooLarge), 413),
            (Some(Code::Failed), 422),
            (Some(Code::ResponseTooLarge), 500),
            (Some(Code::Unreachable), 502),
            (Some(Code::NotLeader), 503),
            (Some(Code::Busy), 503),
        ] {
            assert_eq!(status_of(code), want, "{code:?}");
        }
        let unauthenticated = response_bytes(401, b"{}");
        assert!(
            String::from_utf8(unauthenticated)
                .unwrap()
                .contains("WWW-Authenticate: Bearer\r\n")
        );
    }
}
