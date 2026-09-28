//! [`BrokerClient`]: the typed calls of the broker's operations. An answer
//! the broker refused comes back as its structured error
//! ([`ClientError::Broker`]), unchanged.

use std::fmt;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use dagq_broker_protocol::{
    BrokerError, BrokerSessionToken, ErrorBody, HealthResponse, MAX_REQUEST_BYTES, Operation,
    PROTOCOL_HEADER, PROTOCOL_VERSION, decode, encode, fs as fs_ops, git, process,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::http::{self, ExchangeError, Request};

/// The env naming the broker, `http://127.0.0.1:<port>`.
pub const URL_ENV: &str = "DAGQ_BROKER_URL";

/// The env naming the run's token file, `<queue dir>/broker/tokens/<run id>`.
/// The token's value itself is never in an env or an argument.
pub const TOKEN_FILE_ENV: &str = "DAGQ_BROKER_TOKEN_FILE";

/// How long connecting and sending a request may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the client waits for an answer by default: past the server's
/// longest exec (`--exec-max-timeout-secs`, 300 by default), which the
/// server bounds itself.
pub const READ_TIMEOUT: Duration = Duration::from_secs(330);

/// Why a call did not return the operation's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// The broker refused or failed the operation: its structured error and
    /// the HTTP status it came with.
    Broker { status: u16, error: BrokerError },
    /// The client is not configured: no or a bad URL, no or an unreadable
    /// token file. Names the setting or the path, never the token.
    Config(String),
    /// The broker could not be reached, or the exchange broke off.
    Transport(String),
    /// The broker's answer is not what this client reads: another protocol
    /// version, or a body of another shape (fail closed).
    Protocol(String),
}

impl ClientError {
    /// The broker's structured error, when it refused.
    pub fn broker_error(&self) -> Option<&BrokerError> {
        match self {
            Self::Broker { error, .. } => Some(error),
            _ => None,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Broker { error, .. } => write!(f, "the broker refused: {error}"),
            Self::Config(message) => write!(f, "configuration: {message}"),
            Self::Transport(message) => write!(f, "transport: {message}"),
            Self::Protocol(message) => write!(f, "protocol: {message}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// Where the broker listens: a loopback address only, so a token is never
/// sent off the host (ADR-t827-2 decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint(SocketAddr);

impl Endpoint {
    /// Read `http://<loopback address>:<port>` (a trailing `/` is allowed;
    /// `localhost` is `127.0.0.1`). No TLS, path, query or user.
    pub fn parse(url: &str) -> Result<Self, ClientError> {
        let bad = |why: &str| ClientError::Config(format!("the broker URL `{url}` {why}"));
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| bad("is not http://"))?;
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if authority.contains(['/', '?', '#', '@']) {
            return Err(bad("has more than an address and a port"));
        }
        let addr = match authority.strip_prefix("localhost:") {
            Some(port) => port
                .parse::<u16>()
                .map(|port| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
                .map_err(|_| bad("has no port")),
            None => authority
                .parse::<SocketAddr>()
                .map_err(|_| bad("is not an address and a port")),
        }?;
        if !addr.ip().is_loopback() {
            return Err(bad("is not a loopback address"));
        }
        if addr.port() == 0 {
            return Err(bad("has port 0"));
        }
        Ok(Self(addr))
    }

    pub fn addr(self) -> SocketAddr {
        self.0
    }
}

/// The broker's client for one run: the endpoint and the run's token file,
/// which is read again for each request (the supervisor replaces it before
/// the token expires).
#[derive(Debug, Clone)]
pub struct BrokerClient {
    endpoint: Endpoint,
    token_file: Option<PathBuf>,
    read_timeout: Duration,
}

impl BrokerClient {
    /// A client of `endpoint`, with `token_file` for everything but health.
    pub fn new(endpoint: Endpoint, token_file: Option<PathBuf>) -> Self {
        Self {
            endpoint,
            token_file,
            read_timeout: READ_TIMEOUT,
        }
    }

    /// A client from `DAGQ_BROKER_URL` and `DAGQ_BROKER_TOKEN_FILE` as `env`
    /// gives them; the token file may be missing until a call needs it.
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Result<Self, ClientError> {
        let url = env(URL_ENV)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| ClientError::Config(format!("{URL_ENV} is not set")))?;
        let token_file = env(TOKEN_FILE_ENV)
            .filter(|file| !file.is_empty())
            .map(PathBuf::from);
        Ok(Self::new(Endpoint::parse(&url)?, token_file))
    }

    /// Wait at most `timeout` for each answer.
    pub fn with_read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = timeout;
        self
    }

    pub fn endpoint(&self) -> Endpoint {
        self.endpoint
    }

    /// The token as the token file holds it now.
    pub fn token(&self) -> Result<BrokerSessionToken, ClientError> {
        let file = self.token_file.as_deref().ok_or_else(|| {
            ClientError::Config(format!("no token file ({TOKEN_FILE_ENV} is not set)"))
        })?;
        read_token_file(file)
    }

    /// `GET /v1/health`, without a token.
    pub fn health(&self) -> Result<HealthResponse, ClientError> {
        self.send(Operation::Health, None, &[])
    }

    pub fn fs_read(
        &self,
        request: &fs_ops::ReadRequest,
    ) -> Result<fs_ops::ReadResponse, ClientError> {
        self.call(Operation::FsRead, request)
    }

    pub fn fs_list(
        &self,
        request: &fs_ops::ListRequest,
    ) -> Result<fs_ops::ListResponse, ClientError> {
        self.call(Operation::FsList, request)
    }

    pub fn fs_write(
        &self,
        request: &fs_ops::WriteRequest,
    ) -> Result<fs_ops::WriteResponse, ClientError> {
        self.call(Operation::FsWrite, request)
    }

    pub fn fs_edit(
        &self,
        request: &fs_ops::EditRequest,
    ) -> Result<fs_ops::EditResponse, ClientError> {
        self.call(Operation::FsEdit, request)
    }

    pub fn exec(
        &self,
        request: &process::ExecRequest,
    ) -> Result<process::ExecResponse, ClientError> {
        self.call(Operation::ProcessExec, request)
    }

    pub fn git_status(&self) -> Result<git::StatusResponse, ClientError> {
        self.call(Operation::GitStatus, &git::StatusRequest {})
    }

    pub fn git_diff(&self, request: &git::DiffRequest) -> Result<git::DiffResponse, ClientError> {
        self.call(Operation::GitDiff, request)
    }

    pub fn git_log(&self, request: &git::LogRequest) -> Result<git::LogResponse, ClientError> {
        self.call(Operation::GitLog, request)
    }

    pub fn git_show(&self, request: &git::ShowRequest) -> Result<git::ShowResponse, ClientError> {
        self.call(Operation::GitShow, request)
    }

    pub fn git_add(&self, request: &git::AddRequest) -> Result<git::AddResponse, ClientError> {
        self.call(Operation::GitAdd, request)
    }

    pub fn git_commit(
        &self,
        request: &git::CommitRequest,
    ) -> Result<git::CommitResponse, ClientError> {
        self.call(Operation::GitCommit, request)
    }

    pub fn git_restore(
        &self,
        request: &git::RestoreRequest,
    ) -> Result<git::RestoreResponse, ClientError> {
        self.call(Operation::GitRestore, request)
    }

    /// `operation` with `request` as its body and the token.
    pub fn call<Q: Serialize, A: DeserializeOwned>(
        &self,
        operation: Operation,
        request: &Q,
    ) -> Result<A, ClientError> {
        let token = self.token()?;
        let body = encode(request)
            .map_err(|error| ClientError::Protocol(format!("encode the request: {error}")))?;
        // The broker refuses a larger body without reading it and closes the
        // connection, which the client would see as a broken write.
        if body.len() > MAX_REQUEST_BYTES {
            return Err(ClientError::Config(format!(
                "{operation}: the request is {} bytes, more than the broker reads ({MAX_REQUEST_BYTES})",
                body.len()
            )));
        }
        self.send(operation, Some(&token), &body)
    }

    fn send<A: DeserializeOwned>(
        &self,
        operation: Operation,
        token: Option<&BrokerSessionToken>,
        body: &[u8],
    ) -> Result<A, ClientError> {
        let mut headers = vec![(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())];
        if operation.method() == "POST" {
            headers.push(("Content-Type", "application/json".to_owned()));
        }
        if let Some(token) = token {
            headers.push(("Authorization", format!("Bearer {}", token.expose())));
        }
        let request = Request {
            method: operation.method(),
            path: operation.path(),
            headers,
            body,
        };
        let response = http::exchange(
            self.endpoint.addr(),
            &request,
            CONNECT_TIMEOUT,
            self.read_timeout,
        )
        .map_err(|error| match error {
            ExchangeError::Io(error) => {
                ClientError::Transport(format!("{operation} at {}: {error}", self.endpoint.addr()))
            }
            ExchangeError::Malformed(reason) => {
                ClientError::Protocol(format!("{operation}: {reason}"))
            }
        })?;
        read_answer(operation, &response)
    }
}

/// The operation's answer in `response`, or the broker's error.
fn read_answer<A: DeserializeOwned>(
    operation: Operation,
    response: &http::Response,
) -> Result<A, ClientError> {
    let expected = PROTOCOL_VERSION.to_string();
    match response.header(PROTOCOL_HEADER) {
        Some(version) if version == expected => {}
        other => {
            return Err(ClientError::Protocol(format!(
                "{operation}: the broker speaks protocol {}, this client {expected}",
                other.unwrap_or("(none)")
            )));
        }
    }
    if response.status == 200 {
        return decode(&response.body).map_err(|error| {
            ClientError::Protocol(format!("{operation}: the answer is not its shape: {error}"))
        });
    }
    match decode::<ErrorBody>(&response.body) {
        Ok(body) => Err(ClientError::Broker {
            status: response.status,
            error: body.error,
        }),
        Err(_) => Err(ClientError::Protocol(format!(
            "{operation}: the broker answered {} without an error body",
            response.status
        ))),
    }
}

/// The token `file` holds, trimmed. Errors name the file only.
pub fn read_token_file(file: &Path) -> Result<BrokerSessionToken, ClientError> {
    let text = fs::read_to_string(file).map_err(|error| {
        ClientError::Config(format!("read the token file {}: {error}", file.display()))
    })?;
    let token = text.trim();
    if token.is_empty() {
        return Err(ClientError::Config(format!(
            "the token file {} is empty",
            file.display()
        )));
    }
    // The token goes into a header line as it is.
    if token.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(ClientError::Config(format!(
            "the token file {} holds more than one token",
            file.display()
        )));
    }
    Ok(BrokerSessionToken::new(token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dagq_broker_protocol::{BrokerRequestId, ErrorCode};

    #[test]
    fn endpoints_are_loopback_http_only() {
        for (url, addr) in [
            ("http://127.0.0.1:8750", "127.0.0.1:8750"),
            ("http://127.0.0.1:8750/", "127.0.0.1:8750"),
            ("http://localhost:9", "127.0.0.1:9"),
            ("http://[::1]:80", "[::1]:80"),
        ] {
            assert_eq!(
                Endpoint::parse(url).unwrap().addr(),
                addr.parse::<SocketAddr>().unwrap()
            );
        }
        for (url, why) in [
            ("https://127.0.0.1:1", "is not http://"),
            ("127.0.0.1:1", "is not http://"),
            ("http://127.0.0.1:1/v1", "more than an address"),
            ("http://u@127.0.0.1:1", "more than an address"),
            ("http://127.0.0.1", "not an address and a port"),
            ("http://localhost:x", "has no port"),
            ("http://192.168.1.2:1", "not a loopback address"),
            ("http://0.0.0.0:1", "not a loopback address"),
            ("http://127.0.0.1:0", "port 0"),
        ] {
            match Endpoint::parse(url) {
                Err(ClientError::Config(message)) => assert!(message.contains(why), "{message}"),
                other => panic!("{url}: {other:?}"),
            }
        }
    }

    #[test]
    fn from_env_needs_the_url_and_reads_the_token_file_when_called() {
        let client = BrokerClient::from_env(|name| match name {
            URL_ENV => Some("http://127.0.0.1:1".to_owned()),
            _ => None,
        })
        .unwrap();
        assert_eq!(client.endpoint().addr().port(), 1);
        let error = client.token().unwrap_err();
        assert!(error.to_string().contains(TOKEN_FILE_ENV), "{error}");
        let error = BrokerClient::from_env(|_| Some(String::new())).unwrap_err();
        assert!(error.to_string().contains("DAGQ_BROKER_URL is not set"));
    }

    #[test]
    fn the_token_file_is_trimmed_and_never_quoted_in_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        fs::write(&file, "dagq1.secret.sig\n").unwrap();
        assert_eq!(read_token_file(&file).unwrap().expose(), "dagq1.secret.sig");
        fs::write(&file, " \n").unwrap();
        let error = read_token_file(&file).unwrap_err().to_string();
        assert!(error.contains("is empty"), "{error}");
        fs::write(&file, "dagq1.a\r\nX-Other: b\n").unwrap();
        let error = read_token_file(&file).unwrap_err().to_string();
        assert!(error.contains("more than one token"), "{error}");
        assert!(!error.contains("dagq1.a"), "{error}");
        let error = read_token_file(&dir.path().join("missing"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("read the token file"), "{error}");
    }

    fn answer(status: u16, protocol: Option<&str>, body: &str) -> http::Response {
        http::Response {
            status,
            headers: protocol
                .map(|value| vec![("x-dagq-broker-protocol".to_owned(), value.to_owned())])
                .unwrap_or_default(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn answers_are_read_by_shape_and_protocol() {
        let health: HealthResponse = read_answer(
            Operation::Health,
            &answer(
                200,
                Some("1"),
                r#"{"status":"ok","build":"b","protocol":1}"#,
            ),
        )
        .unwrap();
        assert_eq!(health.build, "b");
        let refused = read_answer::<HealthResponse>(
            Operation::FsRead,
            &answer(
                403,
                Some("1"),
                r#"{"error":{"code":"workspace_violation","message":"m","request_id":"r"}}"#,
            ),
        )
        .unwrap_err();
        assert_eq!(
            refused,
            ClientError::Broker {
                status: 403,
                error: BrokerError::new(
                    ErrorCode::WorkspaceViolation,
                    "m",
                    BrokerRequestId::new("r")
                ),
            }
        );
        assert_eq!(
            refused.broker_error().unwrap().code,
            ErrorCode::WorkspaceViolation
        );
        for (response, why) in [
            (answer(200, Some("2"), "{}"), "speaks protocol 2"),
            (answer(200, None, "{}"), "speaks protocol (none)"),
            (answer(200, Some("1"), r#"{"x":1}"#), "not its shape"),
            (answer(502, Some("1"), "oops"), "without an error body"),
        ] {
            let error = read_answer::<HealthResponse>(Operation::Health, &response).unwrap_err();
            assert!(matches!(error, ClientError::Protocol(_)), "{error:?}");
            assert!(error.to_string().contains(why), "{error}");
            assert!(error.broker_error().is_none());
        }
    }

    #[test]
    fn an_unreachable_broker_is_a_transport_error() {
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let client = BrokerClient::new(Endpoint::parse(&format!("http://{addr}")).unwrap(), None)
            .with_read_timeout(Duration::from_secs(5));
        let error = client.health().unwrap_err();
        assert!(matches!(error, ClientError::Transport(_)), "{error:?}");
        // Without a token file an operation fails before connecting.
        assert!(matches!(client.git_status(), Err(ClientError::Config(_))));
    }
}
