//! The queue broker's protocol ([Queue broker]): the host process of the
//! control side that a container's run reaches the outside through
//! (ADR-t2114-1 decisions 1 and 2), and through it the queue service. It
//! is not the removed resource broker (ADR-t2113-1).
//!
//! The broker listens on a TCP port of `127.0.0.1` (the relay in the VM
//! forwards each run's unix socket there, ADR-t2114-3). One connection
//! carries one HTTP/1.1 request and its response, then closes
//! (`Connection: close`). The shapes a client follows:
//!
//! - The queue service's destination: `POST` [`QUEUE_PATH`], the run token
//!   in [`AUTHORIZATION_HEADER`] as `Bearer <token>` ([`BEARER`]), the run
//!   the request is for in [`RUN_HEADER`], the body's length in
//!   `Content-Length` (a `Transfer-Encoding` is refused), and the body the
//!   JSON of a
//!   [`ServiceRequest`](super::queue_service::ServiceRequest) (its `token`
//!   may be left out: the broker fills it with the run token it checked
//!   before it passes the request on to the queue service's unix socket).
//!   The response's body is the queue service's
//!   [`ServiceResponse`](super::queue_service::ServiceResponse) as the
//!   service wrote it, with status 200, never cut by its length.
//! - A refusal of the broker's own is a `ServiceResponse` with `ok: false`
//!   and a [`ServiceErrorCode`] (`unauthenticated`,
//!   `forbidden_destination`, `bad_request`, `unreachable`), read as the
//!   service's refusals are; its HTTP status is [`Refusal::status`]. Only
//!   a status of 200 is the service's answer.
//! - A package registry's destination: `CONNECT <host>:<port>` to a host
//!   and port the registries of `dagq.toml` name ([`RegistryHost`], read
//!   from the main checkout the queue is bound to, never from a run's
//!   clone; none by default), with the run and its token in
//!   [`PROXY_AUTHORIZATION_HEADER`] as a package manager sends them from
//!   the proxy URL `http://<run>:<token>@<broker>` (`Basic`, the run the
//!   user and the token the password; or `Bearer <token>` with the run in
//!   [`RUN_HEADER`]). The token is a worker's run token or the token of
//!   that run's verification ([`judge_verification_token`]). The answer
//!   is `200` and then the tunnel's bytes, both ways; a refusal is
//!   answered as below ([`judge_tunnel`] says which hosts are never
//!   passed).
//! - The Claude API's destination (ADR-t2177-1): any method on a path
//!   under [`CLAUDE_PATH`] (Claude Code is given
//!   `ANTHROPIC_BASE_URL=http://<broker>/anthropic`), the run token in
//!   [`AUTHORIZATION_HEADER`] as `Bearer <token>` (Claude Code's
//!   `ANTHROPIC_AUTH_TOKEN`), the run in [`RUN_HEADER`] (its
//!   `ANTHROPIC_CUSTOM_HEADERS`), the body's length in `Content-Length` (up
//!   to [`MAX_CLAUDE_BODY_BYTES`]). With a worker's run token of that run,
//!   the request goes to [`CLAUDE_API_URL`] with the rest of the path, its
//!   headers but those [`claude_headers`] drops, and the Claude credential
//!   `host.toml` chooses ([`ClaudeCredentialKind`]) in place of the run
//!   token; the answer is the API's status line, headers and body (an SSE
//!   stream included) as they come, the connection closing at its end.
//! - Any other request (a request that names `api.anthropic.com` as its
//!   host, any other path or host) is refused `forbidden_destination`.
//! - `GET` [`HEALTH_PATH`] answers the broker's build and pid without a
//!   token: `up`, `down` and the supervisor look at the broker with it.
//!
//! Every refusal is the queue event [`QUEUE_BROKER_DENIED`] with the run
//! and the destination; no token's value is in a record or an error.
//!
//! The token of a verification is issued by the control side for each of
//! `integrate`'s verifications and revoked at its end; its value is in
//! [`VERIFICATION_CREDENTIALS_DIR`] and its run in
//! [`VERIFICATION_TOKENS_DIR`], under [`BROKER_DIR`], apart from the queue
//! service's tokens, so the service never takes it.
//!
//! [Queue broker]: ../../docs/design/queue-broker.md

use crate::domain::{EventKind, RecordedKind};
use std::time::Duration;

use super::queue_service::{ServiceErrorCode, ServiceState, run_holds_token};
use serde::{Deserialize, Serialize};

use super::{ActorRole, DomainError, RunId, RunStatus};
use crate::domain::queue_service::Principal;

/// The broker's directory under the queue's directory.
pub const BROKER_DIR: &str = "broker";
/// What the running broker records of itself in [`BROKER_DIR`]: its pid,
/// build, address and start.
pub const STATE_FILE: &str = "state.json";
/// The lock one broker of a queue holds while it runs.
pub const LOCK_FILE: &str = "lock";
/// The log the broker a runtime starts writes to.
pub const LOG_FILE: &str = "broker.log";
/// The path of the queue service's destination.
pub const QUEUE_PATH: &str = "/queue";
/// The path that answers the broker's build and pid, without a token.
pub const HEALTH_PATH: &str = "/health";
/// The header (case-insensitive) that carries the run token.
pub const AUTHORIZATION_HEADER: &str = "authorization";
/// The scheme of the run token in [`AUTHORIZATION_HEADER`].
pub const BEARER: &str = "Bearer ";
/// The header (case-insensitive) that names the run the request is for:
/// the run whose socket carried it. A token of another run is refused.
pub const RUN_HEADER: &str = "x-dagq-run";
/// The longest request body the broker reads: the queue service's.
pub const MAX_BODY_BYTES: usize = super::queue_service::MAX_REQUEST_BYTES;
/// What `GET` [`HEALTH_PATH`] names as the service.
pub const SERVICE_NAME: &str = "dagq-queue-broker";
/// The header (case-insensitive) that carries a `CONNECT`'s run and token:
/// `Basic` ([`BASIC`]) with the base64 of `<run>:<token>`, or [`BEARER`]
/// with the run in [`RUN_HEADER`].
pub const PROXY_AUTHORIZATION_HEADER: &str = "proxy-authorization";
/// The scheme of the run and token in [`PROXY_AUTHORIZATION_HEADER`].
pub const BASIC: &str = "Basic ";
/// The port of a registry `dagq.toml` names without one: HTTPS's.
pub const REGISTRY_DEFAULT_PORT: u16 = 443;
/// The Claude API's host: a destination of the runtime's own (ADR-t2114-2),
/// never a tunnel's, whatever `dagq.toml` names.
pub const CLAUDE_API_HOST: &str = "api.anthropic.com";
/// The path the broker takes the Claude API's requests under: `<CLAUDE_PATH>/v1/messages`
/// goes to `<CLAUDE_API_URL>/v1/messages` (ADR-t2177-1).
pub const CLAUDE_PATH: &str = "/anthropic";
/// Where the broker sends the Claude API's requests.
pub const CLAUDE_API_URL: &str = "https://api.anthropic.com";
/// The longest body of a Claude API request the broker reads: the API's own
/// limit of a request.
pub const MAX_CLAUDE_BODY_BYTES: usize = 32 * 1024 * 1024;
/// The header (case-insensitive) an API key goes in.
pub const API_KEY_HEADER: &str = "x-api-key";
/// The directory under [`BROKER_DIR`] that holds each verification token's
/// run, under the hash of its value.
pub const VERIFICATION_TOKENS_DIR: &str = "verification-tokens";
/// The directory under [`BROKER_DIR`] that holds each verification token's
/// value, under the hash of its run, for the control side to hand its
/// path to the verification and to revoke it.
pub const VERIFICATION_CREDENTIALS_DIR: &str = "verification-credentials";

/// Where a request goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// `POST` [`QUEUE_PATH`]: the queue service.
    QueueService,
    /// `GET` [`HEALTH_PATH`]: the broker's own look.
    Health,
    /// `CONNECT <host>:<port>`: a tunnel, passed only to a registry
    /// `dagq.toml` names ([`judge_tunnel`], ADR-t2257-1 decision 3).
    Tunnel { host: String, port: u16 },
    /// A path under [`CLAUDE_PATH`]: the Claude API, at `path` (the rest of
    /// the target, its query kept; `/` for the prefix alone).
    ClaudeApi { path: String },
    /// Anything else, named by the host it asks for (a `CONNECT`'s
    /// authority without a port, an absolute URI's host, or the `Host`
    /// header) or else by its method and path (no user, password or query
    /// is kept). None of them is passed: a request that names
    /// `api.anthropic.com` itself is one (ADR-t2114-1 decision 2).
    Outside(String),
}

impl Destination {
    /// The destination of a request of `method` for `target` with the
    /// `host` header. The broker's own paths are its only for a request
    /// that names no host but the loopback (the broker, or the forwarder
    /// in front of it): one for the same path that names another host
    /// (`api.anthropic.com`, ...) goes to that host.
    pub fn of(method: &str, target: &str, host: Option<&str>) -> Self {
        if host.is_none_or(|host| host.is_empty() || is_loopback(host)) {
            match (method, target) {
                ("POST", QUEUE_PATH) => return Self::QueueService,
                ("GET", HEALTH_PATH) => return Self::Health,
                _ => {}
            }
            if method != "CONNECT"
                && let Some(rest) = target.strip_prefix(CLAUDE_PATH)
                && (rest.is_empty() || rest.starts_with(['/', '?']))
            {
                return Self::ClaudeApi {
                    path: if rest.starts_with('/') {
                        rest.to_owned()
                    } else {
                        format!("/{rest}")
                    },
                };
            }
        }
        if method == "CONNECT" {
            let authority = target
                .rsplit_once('@')
                .map_or(target, |(_, authority)| authority);
            let host = host_of(authority);
            return match authority
                .strip_prefix(host)
                .and_then(|rest| rest.strip_prefix(':'))
                .and_then(|port| port.parse::<u16>().ok())
            {
                Some(port) if !host.is_empty() => Self::Tunnel {
                    host: host.to_ascii_lowercase(),
                    port,
                },
                _ => Self::Outside(host.to_owned()),
            };
        }
        let absolute = target
            .split_once("://")
            .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(rest));
        match absolute.or(host).filter(|host| !host.is_empty()) {
            Some(authority) => Self::Outside(host_of(authority).to_owned()),
            // The path only: a query may carry what the client should not
            // see recorded.
            None => Self::Outside(format!(
                "{method} {}",
                target.split(['?', '#']).next().unwrap_or(target)
            )),
        }
    }

    /// The destination as a refusal records it.
    pub fn name(&self) -> &str {
        match self {
            Self::QueueService => "queue_service",
            Self::Health => "health",
            Self::Tunnel { host, .. } => host,
            Self::ClaudeApi { .. } => CLAUDE_API_HOST,
            Self::Outside(name) => name,
        }
    }
}

/// The host an authority names, without the user and password before an
/// `@` (never recorded) and without the port.
fn host_of(authority: &str) -> &str {
    strip_port(
        authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host),
    )
}

/// Whether the `Host` value `host` names the loopback: `127.0.0.1`,
/// `localhost` or `[::1]`, with or without a port.
fn is_loopback(host: &str) -> bool {
    let name = host
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
        .map_or_else(|| strip_port(host), |(inside, _)| inside);
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

fn strip_port(authority: &str) -> &str {
    match authority.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => host,
        _ => authority,
    }
}

string_enum!(
    /// Why a request's run token was refused: the `reason` of
    /// `queue_broker_denied`, spelled as each variant's text; the client
    /// reads `unauthenticated`.
    TokenRefusal {
    /// The request carries no `Bearer` token.
    MissingToken => "missing_token",
    /// The request names no run in its run header.
    MissingRun => "missing_run",
    /// The token is not one the control side issued, or it was revoked.
    UnknownToken => "unknown_token",
    /// The token is not a worker's run token (a job's, or one without a
    /// run): only the run tokens of a worker and its resume pass.
    NotARunToken => "not_a_run_token",
    /// The token is another run's than the one the request names.
    OtherRun => "other_run",
    /// The token's run has ended, or the queue does not hold it.
    RunEnded => "run_ended",
    /// The token is a verification's: it reaches the registries of
    /// `dagq.toml` only, never the queue service.
    VerificationToken => "verification_token",
});

/// Whether the token of `token_run`'s verification may open a tunnel for
/// `run`, the token's run being in `status` (`None` when the queue does
/// not hold it): only for that run, while the run holds its tokens. Its
/// revocation at the verification's end ends it before that.
pub fn judge_verification_token(
    token_run: &RunId,
    run: &str,
    status: Option<RunStatus>,
) -> Result<(), TokenRefusal> {
    if token_run.as_str() != run {
        return Err(TokenRefusal::OtherRun);
    }
    match status {
        Some(status) if run_holds_token(status) => Ok(()),
        _ => Err(TokenRefusal::RunEnded),
    }
}

/// The run and the token of a [`PROXY_AUTHORIZATION_HEADER`]'s `Basic`
/// value (the base64 of `<run>:<token>`), its scheme matched without case;
/// `None` for another scheme or a value that does not read.
pub fn basic_credentials(value: &str) -> Option<(String, String)> {
    let scheme = value.get(..BASIC.len())?;
    if !scheme.eq_ignore_ascii_case(BASIC) {
        return None;
    }
    let decoded = String::from_utf8(decode_base64(value[BASIC.len()..].trim())?).ok()?;
    let (run, token) = decoded.split_once(':')?;
    Some((run.to_owned(), token.to_owned()))
}

/// The bytes of standard base64 (padding optional); `None` for any other
/// character.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = Vec::new();
    for byte in text.trim_end_matches('=').bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    Some(out)
}

/// A package registry's host the queue broker passes a `CONNECT` to: one
/// entry of the registries of `dagq.toml`, written `<host>` (HTTPS's port,
/// [`REGISTRY_DEFAULT_PORT`]) or `<host>:<port>`. The host is a DNS name or
/// an IPv4 address (letters, digits, `-` and `.`), matched without case;
/// no scheme, path, user or wildcard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryHost {
    pub host: String,
    pub port: u16,
}

impl std::str::FromStr for RegistryHost {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (host, port) = match text.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (text, None),
        };
        let valid = port.is_none_or(|port| port.bytes().all(|byte| byte.is_ascii_alphanumeric()))
            && !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            });
        if !valid {
            return Err(format!(
                "{text:?} is not a host or <host>:<port> (letters, digits, '-' and '.'; no scheme, path or wildcard)"
            ));
        }
        let port = match port {
            Some(port) => port
                .parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| format!("{text:?} names no port 1 to 65535 after its ':'"))?,
            None => REGISTRY_DEFAULT_PORT,
        };
        Ok(Self {
            host: host.to_ascii_lowercase(),
            port,
        })
    }
}

impl std::fmt::Display for RegistryHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.port == REGISTRY_DEFAULT_PORT {
            f.write_str(&self.host)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

/// Where a tunnel may go: the registries `dagq.toml` names in the main
/// checkout the queue is bound to (none for a queue bound to none), and the
/// hosts of that repository's git remotes (the upstream a run's clone
/// came from).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Egress {
    pub registries: Vec<RegistryHost>,
    pub upstream_git_hosts: Vec<String>,
}

string_enum!(
    /// Why a `CONNECT` was refused: the `reason` of `queue_broker_denied`,
    /// spelled as each variant's text; the client reads
    /// `forbidden_destination`.
    TunnelRefusal {
    /// The registries of `dagq.toml` do not name the host and port (none
    /// are named by default).
    NotAllowed => "not_allowed",
    /// The registries name the host of an upstream git remote, which is
    /// never passed: a run has no way to push (ADR-t2113-2 decision 3).
    UpstreamGitHost => "upstream_git_host",
    /// The host is a destination of the runtime's own
    /// ([`CLAUDE_API_HOST`]), never a tunnel's.
    BuiltinDestination => "builtin_destination",
});

impl TunnelRefusal {
    /// What the answer says of the refusal of a tunnel to `host`:`port`.
    pub fn message(self, host: &str, port: u16) -> String {
        match self {
            Self::NotAllowed => format!(
                "the registries of dagq.toml's [egress] in the main checkout do not name {host}:{port}"
            ),
            Self::UpstreamGitHost => format!(
                "dagq.toml's [egress] registries name {host}, the host of an upstream git remote of the repository; the queue broker never passes it, so a run has no way to push"
            ),
            Self::BuiltinDestination => format!(
                "{host} is a destination of the runtime's own and is never a tunnel's, whatever dagq.toml's [egress] registries name"
            ),
        }
    }
}

/// Whether a tunnel to `host`:`port` may open on `egress`: only to a
/// registry it names that is neither an upstream git remote's host nor
/// the Claude API's.
pub fn judge_tunnel(egress: &Egress, host: &str, port: u16) -> Result<(), TunnelRefusal> {
    let host = host.to_ascii_lowercase();
    if host == CLAUDE_API_HOST {
        return Err(TunnelRefusal::BuiltinDestination);
    }
    let named = egress
        .registries
        .iter()
        .any(|registry| registry.host == host && registry.port == port);
    if egress
        .upstream_git_hosts
        .iter()
        .any(|upstream| upstream.eq_ignore_ascii_case(&host))
    {
        return Err(if named {
            TunnelRefusal::UpstreamGitHost
        } else {
            TunnelRefusal::NotAllowed
        });
    }
    if named {
        Ok(())
    } else {
        Err(TunnelRefusal::NotAllowed)
    }
}

/// The host a git remote's `url` names: of `scheme://[user@]host[:port]/...`
/// or of the scp-like `[user@]host:path`; `None` for a local path or a
/// `file://` URL.
pub fn git_remote_host(url: &str) -> Option<String> {
    let url = url.trim();
    let authority = match url.split_once("://") {
        Some((scheme, rest)) => {
            if scheme.eq_ignore_ascii_case("file") {
                return None;
            }
            rest.split('/').next().unwrap_or(rest)
        }
        None => {
            let (before, _) = url.split_once(':')?;
            // A path with a ':' after a '/' is a local path, not scp-like.
            if before.contains('/') {
                return None;
            }
            before
        }
    };
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or(rest, |(inside, _)| inside),
        None => strip_port(authority),
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Whether `principal`, the one the request's token was issued for, may
/// send a request for `run` through the broker, its run being in `status`
/// (`None` when the queue does not hold it): only the run token of a
/// worker (and its resume) on that very run, while the run holds its
/// tokens.
pub fn judge_run_token(
    principal: &Principal,
    run: &str,
    status: Option<RunStatus>,
) -> Result<(), TokenRefusal> {
    let Some(own) = principal
        .run_id
        .as_ref()
        .filter(|_| principal.role == ActorRole::Worker)
    else {
        return Err(TokenRefusal::NotARunToken);
    };
    if own.as_str() != run {
        return Err(TokenRefusal::OtherRun);
    }
    match status {
        Some(status) if run_holds_token(status) => Ok(()),
        _ => Err(TokenRefusal::RunEnded),
    }
}

string_enum!(
    /// Which Claude credential the broker puts in place of a run token: the
    /// `kind` of `host.toml`'s `[claude_credential]`, chosen by a person for
    /// the host, with no default (ADR-t2177-1 decisions 1 to 3). Only the
    /// chosen one is read, and a failure never switches to the other.
    ClaudeCredentialKind {
    /// A long-lived token `claude setup-token` issued (a subscription's):
    /// sent as the `Authorization` bearer. For one's own subscription used
    /// by oneself.
    SetupToken => "setup_token",
    /// An API key a person issued in the Anthropic Console (paid by use):
    /// sent in [`API_KEY_HEADER`], with no `Authorization`. Recommended for
    /// an organization, a team, a shared host or work.
    ApiKey => "api_key",
});

/// The Claude credential the broker sends with: its kind and its value,
/// which `Debug` never shows.
#[derive(Clone, PartialEq, Eq)]
pub struct ClaudeCredential {
    pub kind: ClaudeCredentialKind,
    pub secret: String,
}

impl std::fmt::Debug for ClaudeCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCredential")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// What the broker and `doctor` tell a person who has chosen no Claude
/// credential (ADR-t2177-1 decision 5).
pub const CLAUDE_CREDENTIAL_GUIDANCE: &str = "choose the Claude credential of the queue broker in host.toml's \
[claude_credential]: kind = \"setup_token\" (the long-lived token of `claude setup-token`; for your own \
subscription used by you alone) or kind = \"api_key\" (an API key issued in the Anthropic Console, paid by \
use; recommended for an organization, a team, a shared host or work), and file = the absolute path of a \
file of mode 0600 that holds it, issued and put there by you";

/// Why the broker has no Claude credential to send with: a container's
/// worker cannot be used meanwhile (ADR-t2177-1 decision 3). No message
/// names a credential's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeCredentialProblem {
    /// No `host.toml` chooses a `kind`.
    NotSelected,
    /// The `[claude_credential]` that chooses does not read: why.
    Invalid(String),
    /// The chosen kind's file is not there.
    Missing(String),
    /// The file is not a regular file of mode 0600 (no access for the group
    /// or others): its path and mode.
    NotPrivate { file: String, mode: u32 },
    /// The file holds nothing.
    Empty(String),
    /// The file could not be read: why.
    Unreadable(String),
}

impl ClaudeCredentialProblem {
    /// The `reason` of the broker's refusal and of `doctor`.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NotSelected => "not_selected",
            Self::Invalid(_) => "invalid",
            Self::Missing(_) => "missing",
            Self::NotPrivate { .. } => "not_private",
            Self::Empty(_) => "empty",
            Self::Unreadable(_) => "unreadable",
        }
    }

    /// What a person reads: the problem, and for [`Self::NotSelected`] the
    /// guidance on which to choose.
    pub fn message(&self) -> String {
        match self {
            Self::NotSelected => format!(
                "no Claude credential is chosen for the queue broker: {CLAUDE_CREDENTIAL_GUIDANCE}"
            ),
            Self::Invalid(why) => format!("host.toml's [claude_credential] does not read: {why}"),
            Self::Missing(file) => {
                format!("the chosen Claude credential's file {file} is not there")
            }
            Self::NotPrivate { file, mode } => format!(
                "the chosen Claude credential's file {file} is not a regular file of mode 0600 (mode {mode:04o}); \
                 make it readable by its owner only"
            ),
            Self::Empty(file) => format!("the chosen Claude credential's file {file} is empty"),
            Self::Unreadable(why) => format!("the chosen Claude credential cannot be read: {why}"),
        }
    }
}

/// Whether a credential file's `mode` (its permission bits) is 0600: read
/// and written by its owner only (ADR-t2177-1 decision 3).
pub fn credential_mode_private(mode: u32) -> bool {
    mode & 0o7777 == 0o600
}

/// The headers a Claude API request goes on with: the client's but those
/// of its own connection (hop-by-hop), its `Host` and length, the run token
/// and the run, and any credential it sent (no run token leaves the
/// broker); then `credential` as its kind sends it.
pub fn claude_headers(
    headers: &[(String, String)],
    credential: &ClaudeCredential,
) -> Vec<(String, String)> {
    const DROPPED: [&str; 14] = [
        "host",
        "content-length",
        "connection",
        "keep-alive",
        "proxy-connection",
        PROXY_AUTHORIZATION_HEADER,
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
        "expect",
        AUTHORIZATION_HEADER,
        API_KEY_HEADER,
        RUN_HEADER,
    ];
    let mut kept: Vec<(String, String)> = headers
        .iter()
        .filter(|(name, _)| {
            !DROPPED
                .iter()
                .any(|dropped| name.trim().eq_ignore_ascii_case(dropped))
        })
        .cloned()
        .collect();
    kept.push(match credential.kind {
        ClaudeCredentialKind::SetupToken => (
            "Authorization".to_owned(),
            format!("{BEARER}{}", credential.secret),
        ),
        ClaudeCredentialKind::ApiKey => (API_KEY_HEADER.to_owned(), credential.secret.clone()),
    });
    kept
}

/// A request the broker refuses on its own: the code a client reads and
/// the reason the record keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: ServiceErrorCode,
    pub reason: String,
}

impl Refusal {
    pub fn token(refusal: TokenRefusal) -> Self {
        Self {
            code: ServiceErrorCode::Unauthenticated,
            reason: refusal.as_str().to_owned(),
        }
    }

    pub fn destination() -> Self {
        Self {
            code: ServiceErrorCode::ForbiddenDestination,
            reason: "not_allowed".to_owned(),
        }
    }

    /// The HTTP status of the refusal: 401 for `unauthenticated`, 403 for
    /// `forbidden_destination`, 502 for `unreachable`, 500 for `failed`
    /// (the broker could not read a token or a run), 400 otherwise.
    pub fn status(&self) -> u16 {
        match self.code {
            ServiceErrorCode::Unauthenticated => 401,
            ServiceErrorCode::ForbiddenDestination => 403,
            ServiceErrorCode::Unreachable => 502,
            ServiceErrorCode::Failed => 500,
            _ => 400,
        }
    }
}

/// A look at the broker as the supervisor's watch judges it: its state,
/// the build it answered with, and whether that is the supervisor's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrokerSeen<'a> {
    pub state: ServiceState,
    pub build: Option<&'a str>,
    pub build_matches: Option<bool>,
}

/// What the supervisor's watch does after a look ([`watch_step`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchStep {
    /// The broker runs a build to keep: nothing to do.
    Keep,
    /// Start a broker: in place of one of another build that answers
    /// (`replacing`, not counted against the limit), or because none
    /// answers (counted).
    Start { replacing: bool },
    /// None answers and the starts in the window reached the limit: leave
    /// it to a person (`queue_broker_down`).
    LeaveToPerson,
}

/// Whether a look is due, `since_last` after the last one (`None` before
/// any): once `interval` has passed.
pub fn look_due(since_last: Option<Duration>, interval: Duration) -> bool {
    since_last.is_none_or(|elapsed| elapsed >= interval)
}

/// Whether a start `age` ago still counts in a restart window of `window`:
/// up to the window's end, not at it.
pub fn in_window(age: Duration, window: Duration) -> bool {
    age < window
}

/// The watch's next step after a look that found `found`. A broker that
/// answers is kept when it runs the supervisor's build, or the build of
/// the broker this supervisor last started (`accepted`: an `install` may
/// have put the next binary in place before this process execs it, and
/// that broker is not replaced again); another that answers is replaced,
/// uncounted, until a start of this supervisor's answers (a start that
/// fails is tried again at the next look, and tells the inbox). One that
/// does not answer is started while the `starts` in the
/// window are below `limit`, and left to a person once they reach it.
pub fn watch_step(
    found: &BrokerSeen<'_>,
    accepted: Option<&str>,
    starts: usize,
    limit: usize,
) -> WatchStep {
    if found.state == ServiceState::Running {
        let kept = found.build_matches == Some(true)
            || found.build.is_some_and(|build| Some(build) == accepted);
        return if kept {
            WatchStep::Keep
        } else {
            WatchStep::Start { replacing: true }
        };
    }
    if starts >= limit {
        WatchStep::LeaveToPerson
    } else {
        WatchStep::Start { replacing: false }
    }
}

/// The broker refused a request: a queue event, written by the broker on
/// the DB, with the `run` the request named (null without one; a
/// `CONNECT`'s from its proxy credentials), the `destination`
/// ([`Destination::name`]), the `method`, the `reason` (a [`TokenRefusal`],
/// a [`TunnelRefusal`], `egress_unreadable` (the registries could not be
/// read), `bad_body`, `malformed`, or the error of a service or a tunnel's
/// host that did not answer or of a token that could not be read) and the
/// `code`; and `token_run`, the token's run, when that is another.
pub const QUEUE_BROKER_DENIED: &str =
    crate::domain::event_kind::EventKind::QueueBrokerDenied.as_str();
/// The supervisor could not keep the broker running: the inbox's attention
/// (`reason`: `start_failed` or `restart_limit`, `message`, and the
/// `supervisor`'s token) until the broker runs again. Meanwhile the
/// container runtime cannot be used.
pub const QUEUE_BROKER_DOWN: &str = crate::domain::event_kind::EventKind::QueueBrokerDown.as_str();
/// The broker answers again after a [`QUEUE_BROKER_DOWN`].
pub const QUEUE_BROKER_RUNNING: &str =
    crate::domain::event_kind::EventKind::QueueBrokerRunning.as_str();
/// `up` or the supervisor started the broker (`by`, `pid`, `build`,
/// `address`, `restart`, `replaced`; the `supervisor`'s token when it
/// started it).
pub const QUEUE_BROKER_STARTED: &str =
    crate::domain::event_kind::EventKind::QueueBrokerStarted.as_str();
/// `down`, or the last supervisor at the end of the drain `down` asked
/// for, stopped the broker (`pid`, `by`; the `supervisor`'s token when it
/// stopped it).
pub const QUEUE_BROKER_STOPPED: &str =
    crate::domain::event_kind::EventKind::QueueBrokerStopped.as_str();
/// `down` asked the supervisors it signalled (`supervisors`) to stop the
/// broker once their drain ends.
pub const QUEUE_BROKER_STOP_REQUESTED: &str =
    crate::domain::event_kind::EventKind::QueueBrokerStopRequested.as_str();
/// The kinds whose latest says whether the attention stands.
pub const QUEUE_BROKER_ATTENTION_KINDS: [&str; 4] = [
    QUEUE_BROKER_DOWN,
    QUEUE_BROKER_RUNNING,
    QUEUE_BROKER_STARTED,
    QUEUE_BROKER_STOPPED,
];

/// Whether the attention stands: the latest of
/// [`QUEUE_BROKER_ATTENTION_KINDS`] is [`QUEUE_BROKER_DOWN`].
pub fn attention_stands(latest: Option<&RecordedKind>) -> bool {
    latest.is_some_and(|kind| *kind == EventKind::QueueBrokerDown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{RunId, TaskId};

    #[test]
    fn only_the_queue_path_goes_to_the_queue_service_and_the_rest_is_named_by_its_host() {
        assert_eq!(
            Destination::of("POST", QUEUE_PATH, Some("127.0.0.1:9")),
            Destination::QueueService
        );
        for host in [
            None,
            Some(""),
            Some("localhost"),
            Some("127.0.0.1:1"),
            Some("[::1]:2"),
        ] {
            assert_eq!(
                Destination::of("POST", QUEUE_PATH, host),
                Destination::QueueService,
                "{host:?}"
            );
            assert_eq!(
                Destination::of("GET", HEALTH_PATH, host),
                Destination::Health
            );
        }
        // A `CONNECT` with a port is a tunnel, its host without case and
        // without a user and password.
        for (target, host, port) in [
            ("crates.io:443", "crates.io", 443),
            ("Static.Crates.IO:8443", "static.crates.io", 8443),
            ("user:secret@crates.io:443", "crates.io", 443),
            ("127.0.0.1:9", "127.0.0.1", 9),
        ] {
            let destination = Destination::of("CONNECT", target, Some("ignored"));
            assert_eq!(
                destination,
                Destination::Tunnel {
                    host: host.to_owned(),
                    port
                }
            );
            assert_eq!(destination.name(), host);
        }
        for (method, target, host, named) in [
            ("CONNECT", "crates.io", None, "crates.io"),
            ("CONNECT", "crates.io:https", None, "crates.io:https"),
            (
                "POST",
                "/v1/messages",
                Some("api.anthropic.com"),
                "api.anthropic.com",
            ),
            (
                "GET",
                "http://example.com:80/x?y",
                Some("ignored"),
                "example.com",
            ),
            ("GET", QUEUE_PATH, None, "GET /queue"),
            // The queue's path for another host goes to that host.
            (
                "POST",
                QUEUE_PATH,
                Some("api.anthropic.com"),
                "api.anthropic.com",
            ),
            ("GET", HEALTH_PATH, Some("crates.io:443"), "crates.io"),
            ("POST", "/other", Some(""), "POST /other"),
            // Neither a user and password nor a query is recorded.
            (
                "GET",
                "http://user:secret@example.com:8080/x",
                None,
                "example.com",
            ),
            ("CONNECT", "user:secret@crates.io", None, "crates.io"),
            ("GET", "/other?key=secret#frag", None, "GET /other"),
        ] {
            let destination = Destination::of(method, target, host);
            assert_eq!(destination, Destination::Outside(named.to_owned()));
            assert_eq!(destination.name(), named);
        }
    }

    #[test]
    fn only_a_worker_s_live_run_token_for_its_own_run_passes() {
        let run = RunId::new("r1").unwrap();
        let worker = Principal::worker(&run, TaskId::new(1));
        assert_eq!(
            judge_run_token(&worker, "r1", Some(RunStatus::Running)),
            Ok(())
        );
        assert_eq!(
            judge_run_token(&worker, "r2", Some(RunStatus::Running)),
            Err(TokenRefusal::OtherRun)
        );
        for ended in [
            Some(RunStatus::Integrated),
            Some(RunStatus::Failed),
            Some(RunStatus::Interrupted),
            Some(RunStatus::Succeeded),
            None,
        ] {
            assert_eq!(
                judge_run_token(&worker, "r1", ended),
                Err(TokenRefusal::RunEnded)
            );
        }
        // A job's token, even one on the run, is no run token.
        let job = Principal {
            role: ActorRole::ReviewJob,
            ..worker.clone()
        };
        let observer = Principal {
            role: ActorRole::Observer,
            actor_id: "observer:1".into(),
            run_id: None,
            task_id: None,
            target: None,
            registration: None,
            session: None,
        };
        // Nor is an executor's telemetry token of the run.
        let telemetry = Principal::telemetry(
            &crate::domain::session_step::StepTarget::run(&RunId::new("r1").unwrap()),
            Some(TaskId::new(1)),
        );
        for principal in [job, observer, telemetry] {
            assert_eq!(
                judge_run_token(&principal, "r1", Some(RunStatus::Running)),
                Err(TokenRefusal::NotARunToken)
            );
        }
    }

    #[test]
    fn a_tunnel_passes_only_to_a_registry_dagq_toml_names() {
        let registries: Vec<RegistryHost> = ["crates.io", "index.crates.io:8443", "github.com"]
            .iter()
            .map(|entry| entry.parse().unwrap())
            .collect();
        // None named, by default: every tunnel is refused.
        assert_eq!(
            judge_tunnel(&Egress::default(), "crates.io", 443),
            Err(TunnelRefusal::NotAllowed)
        );
        let egress = Egress {
            registries,
            upstream_git_hosts: vec!["github.com".into(), "gitlab.example".into()],
        };
        assert_eq!(judge_tunnel(&egress, "crates.io", 443), Ok(()));
        assert_eq!(judge_tunnel(&egress, "Crates.IO", 443), Ok(()));
        assert_eq!(judge_tunnel(&egress, "index.crates.io", 8443), Ok(()));
        for (host, port) in [
            ("crates.io", 80),
            ("index.crates.io", 443),
            ("static.crates.io", 443),
            ("example.com", 443),
            ("gitlab.example", 443),
        ] {
            assert_eq!(
                judge_tunnel(&egress, host, port),
                Err(TunnelRefusal::NotAllowed),
                "{host}:{port}"
            );
        }
        // An upstream git remote's host named in dagq.toml is refused, and
        // the answer says why.
        let refusal = judge_tunnel(&egress, "github.com", 443).unwrap_err();
        assert_eq!(refusal, TunnelRefusal::UpstreamGitHost);
        assert_eq!(refusal.as_str(), "upstream_git_host");
        let message = refusal.message("github.com", 443);
        assert!(message.contains("upstream git remote"), "{message}");
        assert!(message.contains("push"), "{message}");
        // The Claude API's host is never a tunnel's, even when named.
        let anthropic = Egress {
            registries: vec![CLAUDE_API_HOST.parse().unwrap()],
            upstream_git_hosts: Vec::new(),
        };
        assert_eq!(
            judge_tunnel(&anthropic, CLAUDE_API_HOST, 443),
            Err(TunnelRefusal::BuiltinDestination)
        );
        assert!(TunnelRefusal::NotAllowed.message("x", 1).contains("x:1"));
    }

    #[test]
    fn a_registry_is_a_host_with_an_optional_port() {
        assert_eq!(
            "Crates.io".parse::<RegistryHost>(),
            Ok(RegistryHost {
                host: "crates.io".into(),
                port: 443
            })
        );
        let local: RegistryHost = "127.0.0.1:8080".parse().unwrap();
        assert_eq!((local.host.as_str(), local.port), ("127.0.0.1", 8080));
        assert_eq!(local.to_string(), "127.0.0.1:8080");
        assert_eq!(
            "crates.io:443".parse::<RegistryHost>().unwrap().to_string(),
            "crates.io"
        );
        for bad in [
            "",
            "https://crates.io",
            "crates.io/api",
            "*.crates.io",
            "crates.io:0",
            "crates.io:x",
            "user@crates.io",
            "-a.io",
            "a..io",
        ] {
            assert!(bad.parse::<RegistryHost>().is_err(), "{bad}");
        }
    }

    #[test]
    fn the_host_of_a_git_remote_is_read_from_each_form_of_its_url() {
        for (url, host) in [
            ("git@github.com:o/r.git", Some("github.com")),
            ("https://GitHub.com/o/r.git", Some("github.com")),
            (
                "https://user:pw@gitlab.example:8443/o/r",
                Some("gitlab.example"),
            ),
            ("ssh://git@git.example:2222/o/r.git", Some("git.example")),
            ("git://[::1]:9418/r", Some("::1")),
            ("host.example:r.git", Some("host.example")),
            ("/srv/git/r.git", None),
            ("./a:b", None),
            ("file:///srv/git/r.git", None),
            ("r.git", None),
        ] {
            assert_eq!(git_remote_host(url).as_deref(), host, "{url}");
        }
    }

    #[test]
    fn a_verification_token_holds_for_its_own_live_run_only() {
        let run = RunId::new("r1").unwrap();
        assert_eq!(
            judge_verification_token(&run, "r1", Some(RunStatus::Integrating)),
            Ok(())
        );
        assert_eq!(
            judge_verification_token(&run, "r2", Some(RunStatus::Integrating)),
            Err(TokenRefusal::OtherRun)
        );
        for ended in [Some(RunStatus::Integrated), Some(RunStatus::Failed), None] {
            assert_eq!(
                judge_verification_token(&run, "r1", ended),
                Err(TokenRefusal::RunEnded)
            );
        }
    }

    #[test]
    fn basic_credentials_carry_the_run_and_the_token() {
        // base64 of "r1:tok:en" and of "r1" (no ':').
        assert_eq!(
            basic_credentials("basic cjE6dG9rOmVu"),
            Some(("r1".into(), "tok:en".into()))
        );
        assert_eq!(
            basic_credentials("Basic cjE6dA=="),
            Some(("r1".into(), "t".into()))
        );
        for value in ["Basic cjE=", "Bearer cjE6dA==", "Basic !!", "Bas"] {
            assert_eq!(basic_credentials(value), None, "{value}");
        }
    }

    #[test]
    fn a_look_is_due_once_its_interval_passed_and_a_start_counts_up_to_the_window_s_end() {
        let second = Duration::from_secs(1);
        assert!(look_due(None, 10 * second));
        assert!(!look_due(
            Some(10 * second - Duration::from_millis(1)),
            10 * second
        ));
        assert!(look_due(Some(10 * second), 10 * second));
        assert!(look_due(Some(Duration::ZERO), Duration::ZERO));
        let window = 600 * second;
        assert!(in_window(window - Duration::from_millis(1), window));
        assert!(!in_window(window, window));
    }

    #[test]
    fn the_watch_keeps_its_builds_replaces_another_once_and_leaves_a_dead_broker_after_the_limit() {
        let seen = |state, build: Option<&'static str>, matches| BrokerSeen {
            state,
            build,
            build_matches: matches,
        };
        let running = ServiceState::Running;
        assert_eq!(
            watch_step(&seen(running, Some("this"), Some(true)), None, 0, 3),
            WatchStep::Keep
        );
        // Another build is replaced, uncounted, and then kept once it is the
        // one this supervisor started.
        assert_eq!(
            watch_step(&seen(running, Some("old"), Some(false)), None, 3, 3),
            WatchStep::Start { replacing: true }
        );
        assert_eq!(
            watch_step(
                &seen(running, Some("next"), Some(false)),
                Some("next"),
                0,
                3
            ),
            WatchStep::Keep
        );
        for state in [ServiceState::Stopped, ServiceState::Unreachable] {
            assert_eq!(
                watch_step(&seen(state, None, None), None, 2, 3),
                WatchStep::Start { replacing: false }
            );
            assert_eq!(
                watch_step(&seen(state, None, None), None, 3, 3),
                WatchStep::LeaveToPerson
            );
        }
    }

    #[test]
    fn a_refusal_has_its_code_and_status() {
        let token = Refusal::token(TokenRefusal::UnknownToken);
        assert_eq!(token.code, ServiceErrorCode::Unauthenticated);
        assert_eq!(token.reason, "unknown_token");
        assert_eq!(token.status(), 401);
        let destination = Refusal::destination();
        assert_eq!(destination.code, ServiceErrorCode::ForbiddenDestination);
        assert_eq!(destination.status(), 403);
        let unreachable = Refusal {
            code: ServiceErrorCode::Unreachable,
            reason: "x".into(),
        };
        assert_eq!(unreachable.status(), 502);
        let bad = Refusal {
            code: ServiceErrorCode::BadRequest,
            reason: "x".into(),
        };
        assert_eq!(bad.status(), 400);
        assert!(attention_stands(Some(&EventKind::QueueBrokerDown.into())));
        assert!(!attention_stands(Some(
            &EventKind::QueueBrokerRunning.into()
        )));
        assert!(!attention_stands(None));
    }

    #[test]
    fn a_loopback_path_under_the_claude_prefix_goes_to_the_claude_api() {
        for (method, target, path) in [
            (
                "POST",
                "/anthropic/v1/messages?beta=true",
                "/v1/messages?beta=true",
            ),
            ("GET", "/anthropic/v1/models", "/v1/models"),
            ("POST", "/anthropic", "/"),
            ("GET", "/anthropic?x=1", "/?x=1"),
        ] {
            for host in [None, Some("127.0.0.1:9"), Some("localhost")] {
                let destination = Destination::of(method, target, host);
                assert_eq!(
                    destination,
                    Destination::ClaudeApi {
                        path: path.to_owned()
                    },
                    "{method} {target} {host:?}"
                );
                assert_eq!(destination.name(), CLAUDE_API_HOST);
            }
        }
        // Not the prefix, another host, or a tunnel: not the Claude API.
        for (method, target, host) in [
            ("POST", "/anthropicx/v1/messages", None),
            ("POST", "/anthropic/v1/messages", Some("api.anthropic.com")),
            ("POST", "/v1/messages", None),
            ("CONNECT", "/anthropic", None),
        ] {
            assert!(
                !matches!(
                    Destination::of(method, target, host),
                    Destination::ClaudeApi { .. }
                ),
                "{method} {target} {host:?}"
            );
        }
    }

    #[test]
    fn the_chosen_credential_takes_the_run_token_s_place_and_nothing_else_of_the_run_goes_on() {
        let headers: Vec<(String, String)> = [
            ("Host", "127.0.0.1:9"),
            ("Authorization", "Bearer run-token"),
            ("X-Api-Key", "sent-by-the-client"),
            ("x-dagq-run", "r1"),
            ("Proxy-Authorization", "Basic xyz"),
            ("Content-Length", "2"),
            ("Connection", "keep-alive"),
            ("Expect", "100-continue"),
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", "a,b"),
            ("Content-Type", "application/json"),
            ("Accept", "text/event-stream"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .to_vec();
        let kept = [
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", "a,b"),
            ("Content-Type", "application/json"),
            ("Accept", "text/event-stream"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()));
        let token = ClaudeCredential {
            kind: ClaudeCredentialKind::SetupToken,
            secret: "long-lived".into(),
        };
        let mut expected = kept.to_vec();
        expected.push(("Authorization".into(), "Bearer long-lived".into()));
        assert_eq!(claude_headers(&headers, &token), expected);
        let key = ClaudeCredential {
            kind: ClaudeCredentialKind::ApiKey,
            secret: "sk-key".into(),
        };
        let mut expected = kept.to_vec();
        expected.push(("x-api-key".into(), "sk-key".into()));
        let sent = claude_headers(&headers, &key);
        assert_eq!(sent, expected);
        assert!(
            !sent
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("authorization")
                    || value.contains("run-token"))
        );
        // The value never shows in a debug print.
        assert!(!format!("{key:?}").contains("sk-key"));
    }

    #[test]
    fn a_credential_file_is_kept_to_its_owner_and_each_problem_says_so() {
        assert!(credential_mode_private(0o600));
        assert!(!credential_mode_private(0o400));
        for mode in [0o640, 0o604, 0o644, 0o200, 0o000, 0o660] {
            assert!(!credential_mode_private(mode), "{mode:o}");
        }
        assert_eq!(
            "setup_token".parse::<ClaudeCredentialKind>().unwrap(),
            ClaudeCredentialKind::SetupToken
        );
        assert_eq!(
            "api_key".parse::<ClaudeCredentialKind>().unwrap(),
            ClaudeCredentialKind::ApiKey
        );
        assert!("oauth".parse::<ClaudeCredentialKind>().is_err());
        let not_selected = ClaudeCredentialProblem::NotSelected;
        assert_eq!(not_selected.reason(), "not_selected");
        assert!(not_selected.message().contains(CLAUDE_CREDENTIAL_GUIDANCE));
        let open = ClaudeCredentialProblem::NotPrivate {
            file: "/k".into(),
            mode: 0o644,
        };
        assert_eq!(open.reason(), "not_private");
        assert!(open.message().contains("0644"), "{}", open.message());
        for (problem, reason) in [
            (ClaudeCredentialProblem::Invalid("x".into()), "invalid"),
            (ClaudeCredentialProblem::Missing("/k".into()), "missing"),
            (ClaudeCredentialProblem::Empty("/k".into()), "empty"),
            (
                ClaudeCredentialProblem::Unreadable("x".into()),
                "unreadable",
            ),
        ] {
            assert_eq!(problem.reason(), reason);
            assert!(!problem.message().is_empty());
        }
    }
}
