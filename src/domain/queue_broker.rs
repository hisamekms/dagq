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
//! - Any other request (a `CONNECT` to a package registry, a request for
//!   `api.anthropic.com`, any other path or host) is refused
//!   `forbidden_destination` until a later change adds that destination.
//! - `GET` [`HEALTH_PATH`] answers the broker's build and pid without a
//!   token: `up`, `down` and the supervisor look at the broker with it.
//!
//! Every refusal is the queue event [`QUEUE_BROKER_DENIED`] with the run
//! and the destination; no token's value is in a record or an error.
//!
//! [Queue broker]: ../../docs/design/queue-broker.md

use std::time::Duration;

use super::queue_service::{ServiceErrorCode, ServiceState, run_holds_token};
use serde::{Deserialize, Serialize};

use super::{ActorRole, DomainError, RunStatus};
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

/// Where a request goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// `POST` [`QUEUE_PATH`]: the queue service.
    QueueService,
    /// `GET` [`HEALTH_PATH`]: the broker's own look.
    Health,
    /// Anything else, named by the host it asks for (a `CONNECT`'s
    /// authority, an absolute URI's host, or the `Host` header) or else
    /// by its method and path (no user, password or query is kept). None
    /// of them is passed yet: `api.anthropic.com` and the package
    /// registries `dagq.toml` names are a later change's (ADR-t2114-1
    /// decision 2, ADR-t2257-1 decision 3).
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
        }
        if method == "CONNECT" {
            return Self::Outside(host_of(target).to_owned());
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
});

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
/// the DB, with the `run` the request named (null without one), the
/// `destination` ([`Destination::name`]), the `method`, the `reason` (a
/// [`TokenRefusal`], `not_allowed`, `bad_body`, `malformed`, or the error of
/// a service that did not answer or a token that could not be read) and
/// the `code`; and `token_run`, the token's run, when that is another.
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
pub fn attention_stands(latest: Option<&str>) -> bool {
    latest == Some(QUEUE_BROKER_DOWN)
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
        for (method, target, host, named) in [
            ("CONNECT", "crates.io:443", None, "crates.io"),
            ("CONNECT", "static.crates.io:443", None, "static.crates.io"),
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
            ("CONNECT", "user:secret@crates.io:443", None, "crates.io"),
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
        };
        for principal in [job, observer] {
            assert_eq!(
                judge_run_token(&principal, "r1", Some(RunStatus::Running)),
                Err(TokenRefusal::NotARunToken)
            );
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
        assert!(attention_stands(Some(QUEUE_BROKER_DOWN)));
        assert!(!attention_stands(Some(QUEUE_BROKER_RUNNING)));
        assert!(!attention_stands(None));
    }
}
