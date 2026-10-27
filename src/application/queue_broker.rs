//! The queue broker's use cases ([Queue broker], ADR-t2114-1 decisions 1
//! and 2, ADR-t1233-4 decisions 1, 2 and 4): how `up`, `down` and the
//! supervisor start, look at and stop it ([`QueueBrokerControl`],
//! [`ensure`]); what it does with one request ([`QueueBroker::handle`]):
//! a request for the queue service passes only with a worker's run token
//! of the run it names, and goes on to the service with that token, so
//! the service acts as the run's principal; a `CONNECT` opens a tunnel
//! only to a registry `dagq.toml` names, with a worker's run token or its
//! verification's (ADR-t2257-1 decision 3); a request for the Claude API
//! goes on only with a worker's run token of the run it names, with the
//! Claude credential `host.toml` chooses in the token's place
//! (ADR-t2177-1); how `integrate` issues the
//! token of each verification and revokes it at its end
//! ([`VerificationTokens`], [`VerificationToken`]), and how those a killed
//! verification left behind are revoked ([`sweep_left_tokens`],
//! [`LeftTokens`]); and how the container
//! runtime cannot be used while the broker does not run ([`BrokerGate`]).
//!
//! The protocol's shapes are [`crate::domain::queue_broker`]'s.
//!
//! [Queue broker]: ../../docs/design/queue-broker.md

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tracing::warn;

use crate::domain::RunId;
use crate::domain::RunStatus;
use crate::domain::container_runtime::{Availability, RuntimeTarget};
use crate::domain::queue_broker::{
    BEARER, ClaudeCredential, ClaudeCredentialProblem, Destination, Egress, Refusal, SERVICE_NAME,
    TokenRefusal, basic_credentials, claude_headers, judge_run_token, judge_tunnel,
    judge_verification_token,
};
use crate::domain::queue_service::{
    Principal, ServiceErrorCode, ServiceRequest, ServiceResponse, ServiceState,
};

/// What a look at the queue's broker found.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BrokerProbe {
    /// `running` (it answered its health), `stopped` (nothing holds its
    /// lock) or `unreachable` (its process holds the lock and does not
    /// answer).
    pub state: ServiceState,
    /// The `127.0.0.1:<port>` it listens on, as it recorded it.
    pub address: Option<String>,
    pub pid: Option<u32>,
    pub build: Option<String>,
    /// Whether it runs this binary's build.
    pub build_matches: Option<bool>,
    pub started_at: Option<i64>,
    /// Why it does not answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl BrokerProbe {
    /// Running this binary's build: nothing to do.
    pub fn current(&self) -> bool {
        self.state == ServiceState::Running && self.build_matches == Some(true)
    }

    /// What a look found, for an attention's message or a refusal.
    pub fn describe(&self) -> String {
        match &self.error {
            Some(error) => format!("{}: {error}", self.state.as_str()),
            None => self.state.as_str().to_owned(),
        }
    }
}

/// How `up`, `down` and the supervisor start, look at and stop the queue's
/// broker (ADR-t2114-1 decision 1): one per queue, from the fixed binary
/// that runs them.
pub trait QueueBrokerControl: Send + Sync {
    /// Look at the broker: its record and whether it answers its health.
    fn probe(&self) -> BrokerProbe;
    /// Start the broker of this binary, stopping one of another build
    /// first, and wait up to `timeout` for it to answer.
    fn start(&self, timeout: Duration) -> Result<BrokerProbe>;
    /// Stop the broker; `None` when none ran.
    fn stop(&self, timeout: Duration) -> Result<Option<u32>>;
}

/// `up`'s step: reuse a broker of this build that answers, else start one
/// (replacing one of another build), and report what was done
/// (`outcome`: `reused`, `started` or `replaced`; `broker`; `replaced`).
/// A broker that does not start stops `up`.
pub fn ensure(control: &dyn QueueBrokerControl, timeout: Duration) -> Result<Value> {
    let found = control.probe();
    if found.current() {
        return Ok(json!({"outcome": "reused", "broker": found}));
    }
    let replaced = (found.state != ServiceState::Stopped).then(|| found.clone());
    let started = control
        .start(timeout)
        .context("the queue broker did not start")?;
    Ok(json!({
        "outcome": if replaced.is_some() { "replaced" } else { "started" },
        "broker": started,
        "replaced": replaced,
    }))
}

/// The payload of `queue_broker_started` by `by` for `report`, an
/// [`ensure`]'s (`pid`, `build`, `address`, `replaced`; `restart` false).
pub fn started_payload(by: &str, report: &Value) -> Value {
    json!({
        "by": by,
        "pid": report["broker"]["pid"],
        "build": report["broker"]["build"],
        "address": report["broker"]["address"],
        "restart": false,
        "replaced": report["replaced"],
    })
}

/// What the broker runs a request on.
pub trait BrokerBackend: Send + Sync {
    /// The principal `token` was issued for; `None` for a token the
    /// control side did not issue or has revoked.
    fn principal(&self, token: &str) -> Result<Option<Principal>>;
    /// The status of run `id`; `None` for a run the queue does not hold.
    fn run_status(&self, id: &RunId) -> Result<Option<RunStatus>>;
    /// Send `line` (one request of the queue service, ending in a newline)
    /// to the service and read its whole answer, whatever its length.
    fn forward(&self, line: &[u8]) -> Result<Vec<u8>>;
    /// Record a refused request (`queue_broker_denied`).
    fn record_denied(&self, payload: Value) -> Result<()>;
    /// Where a tunnel may go now: the registries of the `dagq.toml` in the
    /// main checkout the queue is bound to, never a run's clone's, and the
    /// hosts of that repository's git remotes.
    fn egress(&self) -> Result<Egress>;
    /// The run whose verification `token` was issued for; `None` for a
    /// token not issued for a verification, or revoked.
    fn verification_run(&self, token: &str) -> Result<Option<RunId>>;
    /// The Claude credential `host.toml` chooses now, read for each request
    /// so a person's change takes effect without a restart; why there is
    /// none to send with. Only the chosen kind is read.
    fn claude_credential(&self) -> Result<ClaudeCredential, ClaudeCredentialProblem>;
}

/// Which check of a run's commit in a container a verification token is
/// for: each holds a token of its own, so the landing's verification and
/// the landing recheck's command, which may run at once for the same run,
/// never revoke each other's. Both reach the same registries alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenFor {
    /// The verification of `integrate`.
    Landing,
    /// The `[recheck] command` of the landing recheck (ADR-t2424-1).
    Recheck,
    /// The verification of an `already_satisfied` claim on a snapshot of
    /// main (ADR-t1818-1).
    Snapshot,
}

impl TokenFor {
    pub const ALL: [Self; 3] = [Self::Landing, Self::Recheck, Self::Snapshot];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Landing => "landing",
            Self::Recheck => "recheck",
            Self::Snapshot => "snapshot",
        }
    }

    /// Whether a verification for this may run while its run is `status`:
    /// the landing's only while it is `integrating`, the recheck's while
    /// it waits to land (or has just begun to land or was parked meanwhile),
    /// the snapshot's while its claim is `validating`. A token kept past
    /// these is one its verification left behind (its process was killed),
    /// which [`sweep_left_tokens`] and [`LeftTokens`] revoke.
    pub const fn verifies_while(self, status: RunStatus) -> bool {
        match self {
            Self::Landing => matches!(status, RunStatus::Integrating),
            Self::Recheck => matches!(
                status,
                RunStatus::AwaitingIntegration | RunStatus::Integrating | RunStatus::NeedsSession
            ),
            Self::Snapshot => matches!(status, RunStatus::Validating),
        }
    }
}

/// One issue of a run's verification token: the hash of its value, which
/// tells it from the one issued again for the same run and use, and gives
/// nothing of the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenIssue(pub String);

/// What a sweep of the verifications' tokens did ([`sweep_left_tokens`],
/// [`LeftTokens::revoke`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenSweep {
    /// The tokens revoked: their run and what they were for.
    pub revoked: Vec<(RunId, TokenFor)>,
    /// The halves of a token removed alone: a value with no record (the
    /// broker never took it) or a record whose value is gone, past the
    /// time an issue takes to write both.
    pub orphans: usize,
    /// What could not be read or revoked, kept for a later sweep.
    pub failures: Vec<String>,
}

impl TokenSweep {
    pub fn is_empty(&self) -> bool {
        self.revoked.is_empty() && self.orphans == 0 && self.failures.is_empty()
    }

    /// The payload of `queue_broker_tokens_swept`
    /// ([`crate::domain::queue_broker::QUEUE_BROKER_TOKENS_SWEPT`]).
    pub fn payload(&self, by: &str) -> Value {
        json!({
            "by": by,
            "revoked": self
                .revoked
                .iter()
                .map(|(run, of)| json!({"run": run.as_str(), "for": of.as_str()}))
                .collect::<Vec<_>>(),
            "orphans": self.orphans,
            "failures": self.failures,
        })
    }
}

/// Revoke the tokens of the verifications no run runs now, left by an
/// `integrate` or a supervisor killed in the middle of one: a token whose
/// run the queue does not hold, or holds in a status its verification
/// does not run in ([`TokenFor::verifies_while`]). A run whose status
/// does not read keeps its token. The broker and the supervisor call this
/// as they start; it is idempotent, and what fails is in
/// [`TokenSweep::failures`], never an error.
pub fn sweep_left_tokens(
    tokens: &dyn VerificationTokens,
    status_of: &dyn Fn(&RunId) -> Result<Option<RunStatus>>,
) -> TokenSweep {
    let mut unread = Vec::new();
    let swept = tokens.sweep(&mut |run, of| match status_of(run) {
        Ok(status) => Ok(status.is_some_and(|status| of.verifies_while(status))),
        Err(error) => {
            unread.push(format!("run {run}'s status: {error:#}"));
            Ok(true)
        }
    });
    let mut sweep = swept.unwrap_or_else(|error| TokenSweep {
        failures: vec![format!("{error:#}")],
        ..TokenSweep::default()
    });
    sweep.failures.extend(unread);
    sweep
}

/// The tokens of `run`'s verifications that could run while it is
/// `previous`, as issued now: read before a recovery moves the run (the
/// supervisor's or `recover`'s, its verification's process killed), so
/// [`Self::revoke`] revokes those issues once it moved, and never one a
/// landing that began on the run in between issued.
#[derive(Debug, Clone)]
pub struct LeftTokens {
    run: RunId,
    held: Vec<(TokenFor, TokenIssue)>,
    failures: Vec<String>,
}

impl LeftTokens {
    pub fn read(tokens: &dyn VerificationTokens, run: &RunId, previous: RunStatus) -> Self {
        let mut left = Self {
            run: run.clone(),
            held: Vec::new(),
            failures: Vec::new(),
        };
        for of in TokenFor::ALL {
            if !of.verifies_while(previous) {
                continue;
            }
            match tokens.issued(run, of) {
                Ok(Some(issue)) => left.held.push((of, issue)),
                Ok(None) => {}
                Err(error) => left
                    .failures
                    .push(format!("run {run}'s {} token: {error:#}", of.as_str())),
            }
        }
        left
    }

    /// Revoke the issues read of the uses whose verification does not run
    /// now the run is `status`: their files go at once rather than at the
    /// next sweep. Idempotent; what fails is in [`TokenSweep::failures`].
    pub fn revoke(self, tokens: &dyn VerificationTokens, status: RunStatus) -> TokenSweep {
        let mut sweep = TokenSweep {
            failures: self.failures,
            ..TokenSweep::default()
        };
        for (of, issue) in self.held {
            if of.verifies_while(status) {
                continue;
            }
            match tokens.revoke_issued(&self.run, of, &issue) {
                Ok(true) => sweep.revoked.push((self.run.clone(), of)),
                Ok(false) => {}
                Err(error) => sweep.failures.push(format!(
                    "run {}'s {} token: {error:#}",
                    self.run,
                    of.as_str()
                )),
            }
        }
        sweep
    }
}

/// Log `sweep` by `by` and record it as `queue_broker_tokens_swept` with
/// `record` when it did or failed anything; a record that fails is only
/// logged, so nothing stops on it.
pub fn note_token_sweep(sweep: &TokenSweep, by: &str, record: impl FnOnce(Value) -> Result<()>) {
    if sweep.is_empty() {
        return;
    }
    if sweep.failures.is_empty() {
        tracing::info!(
            by,
            revoked = sweep.revoked.len(),
            orphans = sweep.orphans,
            "the verifications' tokens left behind were revoked"
        );
    } else {
        warn!(
            by,
            failures = ?sweep.failures,
            "the verifications' tokens left behind could not all be revoked"
        );
    }
    if let Err(error) = record(sweep.payload(by)) {
        warn!("queue_broker_tokens_swept could not be recorded: {error:#}");
    }
}

/// How `integrate`, the landing recheck and the verification of a
/// snapshot issue the token of a run's verification and revoke it (ADR-t2114-1 decision 2): the control
/// side's only.
pub trait VerificationTokens: Send + Sync {
    /// Issue the token of `run`'s verification for `of`, revoking the one
    /// it held for `of`: the file its value is in, for the verification
    /// to be handed the path of (never the value).
    fn issue(&self, run: &RunId, of: TokenFor) -> Result<PathBuf>;
    /// Revoke the token of `run`'s verification for `of`; whether there
    /// was one.
    fn revoke(&self, run: &RunId, of: TokenFor) -> Result<bool>;
    /// The issue of `run`'s token for `of` now; `None` while it holds none.
    fn issued(&self, run: &RunId, of: TokenFor) -> Result<Option<TokenIssue>>;
    /// Revoke `run`'s token for `of` only while it is still `issue`, never
    /// one issued since: whether it was revoked.
    fn revoke_issued(&self, run: &RunId, of: TokenFor, issue: &TokenIssue) -> Result<bool>;
    /// Revoke every token `keep` refuses (its run and what it is for), and
    /// remove the halves of a token left alone; a token `keep` fails on is
    /// kept and named in the failures.
    fn sweep(&self, keep: &mut dyn FnMut(&RunId, TokenFor) -> Result<bool>) -> Result<TokenSweep>;
}

/// The token of one verification of a run: issued by [`Self::issue`] and
/// revoked when this goes, at the verification's end however it ends.
pub struct VerificationToken<'a> {
    tokens: &'a dyn VerificationTokens,
    run: RunId,
    of: TokenFor,
    /// The file the token's value is in.
    pub file: PathBuf,
}

impl<'a> VerificationToken<'a> {
    pub fn issue(tokens: &'a dyn VerificationTokens, run: &RunId, of: TokenFor) -> Result<Self> {
        let file = tokens
            .issue(run, of)
            .with_context(|| format!("issue the token of run {run}'s verification"))?;
        Ok(Self {
            tokens,
            run: run.clone(),
            of,
            file,
        })
    }
}

impl Drop for VerificationToken<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.tokens.revoke(&self.run, self.of) {
            warn!(
                "could not revoke the token of run {}'s verification: {error:#}",
                self.run
            );
        }
    }
}

/// One HTTP request as the broker read it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrokerRequest {
    pub method: String,
    /// The request target: a path, an absolute URI or a `CONNECT`'s
    /// authority.
    pub target: String,
    /// The `Host` header.
    pub host: Option<String>,
    /// The `Authorization` header.
    pub authorization: Option<String>,
    /// The `Proxy-Authorization` header
    /// ([`crate::domain::queue_broker::PROXY_AUTHORIZATION_HEADER`]).
    pub proxy_authorization: Option<String>,
    /// The run header ([`crate::domain::queue_broker::RUN_HEADER`]).
    pub run: Option<String>,
    /// Every header as it came, in order: what a Claude API request goes
    /// on with ([`claude_headers`]).
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// The broker's answer: an HTTP status and the body, JSON; or a tunnel to
/// open to a host and port, whose own answer is the broker's to write
/// once it reaches them; or a request to send on to the Claude API, whose
/// answer is the API's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerAnswer {
    pub status: u16,
    pub body: Vec<u8>,
    pub tunnel: Option<(String, u16)>,
    pub claude: Option<ClaudeForward>,
}

/// A request to send on to the Claude API with the request's body: its
/// method, the path on the API and the headers, the credential among them
/// (which `Debug` never shows).
#[derive(Clone, PartialEq, Eq)]
pub struct ClaudeForward {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
}

impl std::fmt::Debug for ClaudeForward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeForward")
            .field("method", &self.method)
            .field("path", &self.path)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// The broker's use cases on a backend.
pub struct QueueBroker<'a> {
    pub backend: &'a dyn BrokerBackend,
    /// The broker's build, which its health names.
    pub build: &'a str,
    /// The broker's process.
    pub pid: u32,
}

impl QueueBroker<'_> {
    /// Answer `request`. A request the broker refuses is recorded with its
    /// run and destination, and no answer or record names a token.
    pub fn handle(&self, request: &BrokerRequest) -> BrokerAnswer {
        let destination =
            Destination::of(&request.method, &request.target, request.host.as_deref());
        match destination {
            Destination::Health => answer(
                200,
                &json!({"service": SERVICE_NAME, "build": self.build, "pid": self.pid}),
            ),
            Destination::Outside(_) => self.refuse(
                request,
                request.run.as_deref(),
                &destination,
                None,
                Refusal::destination(),
                None,
            ),
            Destination::QueueService => match self.pass_on(request) {
                Ok(body) => BrokerAnswer {
                    status: 200,
                    body,
                    tunnel: None,
                    claude: None,
                },
                Err(refused) => {
                    self.refused(request, request.run.as_deref(), &destination, refused)
                }
            },
            Destination::Tunnel { ref host, port } => {
                let (token, run) = tunnel_credentials(request);
                match self.open_tunnel(host, port, token.as_deref(), run.as_deref()) {
                    Ok(()) => BrokerAnswer {
                        status: 200,
                        body: Vec::new(),
                        tunnel: Some((host.clone(), port)),
                        claude: None,
                    },
                    Err(refused) => self.refused(request, run.as_deref(), &destination, refused),
                }
            }
            Destination::ClaudeApi { ref path } => match self.to_claude(request, path) {
                Ok(forward) => BrokerAnswer {
                    status: 200,
                    body: Vec::new(),
                    tunnel: None,
                    claude: Some(forward),
                },
                Err(refused) => {
                    self.refused(request, request.run.as_deref(), &destination, refused)
                }
            },
        }
    }

    /// Check the run token of a request for the Claude API (a worker's of
    /// the run it names; never a verification's) and take the Claude
    /// credential `host.toml` chooses: what to send on, or why not. A
    /// credential there is none of is refused `unauthenticated`, so the
    /// worker's turn stops at the login as at an expired one (ADR-t2114-2
    /// decision 4).
    fn to_claude(&self, request: &BrokerRequest, path: &str) -> Result<ClaudeForward, Refused> {
        self.check_token(
            request.authorization.as_deref().and_then(bearer),
            request.run.as_deref(),
            false,
        )?;
        let credential = self
            .backend
            .claude_credential()
            .map_err(|problem| Refused {
                refusal: Refusal {
                    code: ServiceErrorCode::Unauthenticated,
                    reason: format!("claude_credential_{}", problem.reason()),
                },
                token_run: None,
                message: Some(problem.message()),
            })?;
        Ok(ClaudeForward {
            method: request.method.clone(),
            path: path.to_owned(),
            headers: claude_headers(&request.headers, &credential),
        })
    }

    /// Refuse a Claude API `request` passed whose API did not answer, with
    /// `error`, as the service that does not answer is (`unreachable`).
    pub fn claude_failed(&self, request: &BrokerRequest, error: &anyhow::Error) -> BrokerAnswer {
        let destination =
            Destination::of(&request.method, &request.target, request.host.as_deref());
        self.refuse(
            request,
            request.run.as_deref(),
            &destination,
            None,
            Refusal {
                code: ServiceErrorCode::Unreachable,
                reason: format!("{error:#}"),
            },
            None,
        )
    }

    /// Refuse a tunnel `request` passed whose host did not answer, with
    /// `error`, as the service that does not answer is (`unreachable`).
    pub fn tunnel_failed(&self, request: &BrokerRequest, error: &anyhow::Error) -> BrokerAnswer {
        let destination =
            Destination::of(&request.method, &request.target, request.host.as_deref());
        let (_, run) = tunnel_credentials(request);
        self.refuse(
            request,
            run.as_deref(),
            &destination,
            None,
            Refusal {
                code: ServiceErrorCode::Unreachable,
                reason: format!("{error:#}"),
            },
            None,
        )
    }

    /// Whether a tunnel to `host`:`port` opens for `run` with `token`: the
    /// registries of `dagq.toml` name it, and the token is the run's own
    /// worker's or its verification's.
    fn open_tunnel(
        &self,
        host: &str,
        port: u16,
        token: Option<&str>,
        run: Option<&str>,
    ) -> Result<(), Refused> {
        let egress = self.backend.egress().map_err(|error| Refused {
            refusal: Refusal {
                code: ServiceErrorCode::ForbiddenDestination,
                reason: "egress_unreadable".to_owned(),
            },
            token_run: None,
            message: Some(format!(
                "the registries of dagq.toml in the main checkout could not be read, so no tunnel opens: {error:#}"
            )),
        })?;
        judge_tunnel(&egress, host, port).map_err(|refusal| Refused {
            refusal: Refusal {
                code: ServiceErrorCode::ForbiddenDestination,
                reason: refusal.as_str().to_owned(),
            },
            token_run: None,
            message: Some(refusal.message(host, port)),
        })?;
        self.check_token(token, run, true).map(drop)
    }

    /// Refuse a request that did not read as HTTP, with `message`, recorded
    /// with the run and the destination of what was `read` of it.
    pub fn malformed(&self, read: &BrokerRequest, message: &str) -> BrokerAnswer {
        let refusal = Refusal {
            code: ServiceErrorCode::BadRequest,
            reason: "malformed".to_owned(),
        };
        let destination = if read.method.is_empty() {
            Destination::Outside("unknown".to_owned())
        } else {
            Destination::of(&read.method, &read.target, read.host.as_deref())
        };
        self.refuse(
            read,
            read.run.as_deref(),
            &destination,
            None,
            refusal,
            Some(message),
        )
    }

    /// Check `token` for `run`: a worker's run token of that run, or, where
    /// `verification` allows it, the token of that run's verification. The
    /// token that passed, or why not with the token's run when it differs.
    fn check_token<'t>(
        &self,
        token: Option<&'t str>,
        run: Option<&str>,
        verification: bool,
    ) -> Result<&'t str, Refused> {
        let token_refused = |refusal: TokenRefusal| Refused::from(Refusal::token(refusal));
        let token = token
            .filter(|token| !token.is_empty())
            .ok_or_else(|| token_refused(TokenRefusal::MissingToken))?;
        let run = run
            .filter(|run| !run.is_empty())
            .ok_or_else(|| token_refused(TokenRefusal::MissingRun))?;
        let status_of = |own: &RunId| {
            self.backend.run_status(own).map_err(|error| {
                Refused::from(failed(format!(
                    "the token's run could not be read: {error:#}"
                )))
            })
        };
        let with_token_run = |own: Option<&RunId>, refusal: TokenRefusal| Refused {
            token_run: own
                .cloned()
                .filter(|own| refusal == TokenRefusal::OtherRun && own.as_str() != run),
            ..Refused::from(Refusal::token(refusal))
        };
        let unreadable =
            |error: anyhow::Error| failed(format!("the token could not be read: {error:#}"));
        let principal = match self.backend.principal(token) {
            Ok(Some(principal)) => principal,
            Ok(None) => {
                let Some(own) = self
                    .backend
                    .verification_run(token)
                    .map_err(|error| Refused::from(unreadable(error)))?
                else {
                    return Err(token_refused(TokenRefusal::UnknownToken));
                };
                if !verification {
                    return Err(token_refused(TokenRefusal::VerificationToken));
                }
                let status = status_of(&own)?;
                judge_verification_token(&own, run, status)
                    .map_err(|refusal| with_token_run(Some(&own), refusal))?;
                return Ok(token);
            }
            Err(error) => return Err(unreadable(error).into()),
        };
        let status = match principal.run_id.as_ref() {
            Some(own) => status_of(own)?,
            None => None,
        };
        judge_run_token(&principal, run, status)
            .map_err(|refusal| with_token_run(principal.run_id.as_ref(), refusal))?;
        Ok(token)
    }

    /// Check the run token and pass the request on with it: the service's
    /// answer, or why not with the token's run when it differs.
    fn pass_on(&self, request: &BrokerRequest) -> Result<Vec<u8>, Refused> {
        let token = self.check_token(
            request.authorization.as_deref().and_then(bearer),
            request.run.as_deref(),
            false,
        )?;
        let mut service_request: ServiceRequest =
            serde_json::from_slice(&request.body).map_err(|error| Refused {
                // Neither the record nor the answer carries serde's text: it
                // may quote what the client sent (a token in the wrong
                // field).
                refusal: Refusal {
                    code: ServiceErrorCode::BadRequest,
                    reason: "bad_body".to_owned(),
                },
                token_run: None,
                message: Some(unreadable_body(&error)),
            })?;
        service_request.token = Some(token.to_owned());
        let mut line = serde_json::to_vec(&service_request).map_err(|error| {
            Refused::from(failed(format!("the request does not write: {error}")))
        })?;
        line.push(b'\n');
        self.backend.forward(&line).map_err(|error| {
            Refused::from(Refusal {
                code: ServiceErrorCode::Unreachable,
                reason: format!("{error:#}"),
            })
        })
    }

    fn refused(
        &self,
        request: &BrokerRequest,
        run: Option<&str>,
        destination: &Destination,
        refused: Refused,
    ) -> BrokerAnswer {
        let Refused {
            refusal,
            token_run,
            message,
        } = refused;
        self.refuse(
            request,
            run,
            destination,
            token_run,
            refusal,
            message.as_deref(),
        )
    }

    fn refuse(
        &self,
        request: &BrokerRequest,
        run: Option<&str>,
        destination: &Destination,
        token_run: Option<RunId>,
        refusal: Refusal,
        message: Option<&str>,
    ) -> BrokerAnswer {
        let mut payload = json!({
            "run": run,
            "destination": destination.name(),
            "method": request.method,
            "reason": refusal.reason,
            "code": refusal.code,
        });
        if let Some(token_run) = &token_run {
            payload["token_run"] = json!(token_run);
        }
        if let Err(error) = self.backend.record_denied(payload) {
            warn!(
                "could not record the refused request ({}): {error:#}",
                refusal.reason
            );
        }
        let message = message.map_or_else(
            || match refusal.code {
                ServiceErrorCode::ForbiddenDestination => format!(
                    "the queue broker does not pass requests to {}",
                    destination.name()
                ),
                ServiceErrorCode::Unauthenticated
                    if refusal.reason == TokenRefusal::VerificationToken.as_str() =>
                {
                    "a verification's token reaches only the registries dagq.toml names, never the queue service".to_owned()
                }
                ServiceErrorCode::Unauthenticated => format!(
                    "the queue broker takes only the run token of the run the request names ({})",
                    refusal.reason
                ),
                _ => refusal.reason.clone(),
            },
            str::to_owned,
        );
        answer(
            refusal.status(),
            &serde_json::to_value(ServiceResponse::failure(refusal.code, message))
                .unwrap_or(Value::Null),
        )
    }
}

/// Why [`QueueBroker::pass_on`] did not pass a request on: the refusal,
/// the token's run when it is another than the request names, and the
/// message of the answer when it says more than the recorded reason.
struct Refused {
    refusal: Refusal,
    token_run: Option<RunId>,
    message: Option<String>,
}

impl From<Refusal> for Refused {
    fn from(refusal: Refusal) -> Self {
        Self {
            refusal,
            token_run: None,
            message: None,
        }
    }
}

/// What the answer says of a body that does not read as a queue service
/// request: the kind of the error and where, never the text of the input.
fn unreadable_body(error: &serde_json::Error) -> String {
    use serde_json::error::Category;
    let kind = match error.classify() {
        Category::Io => "io",
        Category::Syntax => "syntax",
        Category::Data => "data",
        Category::Eof => "eof",
    };
    format!(
        "the request body does not read as a queue service request ({kind} error at line {} column {})",
        error.line(),
        error.column()
    )
}

fn failed(reason: String) -> Refusal {
    Refusal {
        code: ServiceErrorCode::Failed,
        reason,
    }
}

/// The token and the run of a tunnel's request: of its
/// `Proxy-Authorization` (`Basic` `<run>:<token>`, or `Bearer` with the run
/// header), else of its `Authorization` and run header.
fn tunnel_credentials(request: &BrokerRequest) -> (Option<String>, Option<String>) {
    let proxy = request.proxy_authorization.as_deref();
    if let Some((run, token)) = proxy.and_then(basic_credentials) {
        return (Some(token), Some(run));
    }
    let token = proxy
        .or(request.authorization.as_deref())
        .and_then(bearer)
        .map(str::to_owned);
    (token, request.run.clone())
}

/// The token of an `Authorization` header's value, its scheme matched
/// without case.
fn bearer(value: &str) -> Option<&str> {
    let scheme = value.get(..BEARER.len())?;
    scheme
        .eq_ignore_ascii_case(BEARER)
        .then(|| value[BEARER.len()..].trim())
}

fn answer(status: u16, body: &Value) -> BrokerAnswer {
    BrokerAnswer {
        status,
        body: serde_json::to_vec(body).unwrap_or_default(),
        tunnel: None,
        claude: None,
    }
}

/// The container runtime as it can be used with the queue's broker: while
/// the broker does not run, no target set to the container can run (a run
/// in a container has no way out but the broker), and the answer says
/// why; while it runs, `container` answers.
pub struct BrokerGate<C> {
    pub broker: Arc<dyn QueueBrokerControl>,
    pub container: C,
}

impl<C: super::ContainerRuntime> super::ContainerRuntime for BrokerGate<C> {
    fn availability(&self, target: RuntimeTarget) -> Availability {
        let found = self.broker.probe();
        if found.state != ServiceState::Running {
            return Availability::Unavailable(format!(
                "the queue broker is not running ({})",
                found.describe()
            ));
        }
        self.container.availability(target)
    }

    fn memory_wait(&self) -> Option<String> {
        self.container.memory_wait()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::anyhow;

    use super::*;
    use crate::domain::TaskId;
    use crate::domain::queue_broker::ClaudeCredentialKind;
    use crate::domain::queue_service::{API_VERSION, UseCase};

    /// Run `r1` of task 1 in `status`, its worker's token `good`, a job's
    /// token on it `job`, its verification's token `verify`; the egress
    /// tunnels may go to (`None`: it cannot be read); what was forwarded
    /// and recorded is kept.
    struct Backend {
        status: Option<RunStatus>,
        reachable: bool,
        egress: Option<Egress>,
        credential: Result<ClaudeCredential, ClaudeCredentialProblem>,
        forwarded: Mutex<Vec<Value>>,
        denied: Mutex<Vec<Value>>,
    }

    impl Backend {
        fn new(status: Option<RunStatus>) -> Self {
            Self {
                status,
                reachable: true,
                egress: Some(Egress::default()),
                credential: Err(ClaudeCredentialProblem::NotSelected),
                forwarded: Mutex::new(Vec::new()),
                denied: Mutex::new(Vec::new()),
            }
        }
    }

    impl BrokerBackend for Backend {
        fn principal(&self, token: &str) -> Result<Option<Principal>> {
            let run = RunId::new("r1")?;
            Ok(match token {
                "good" => Some(Principal::worker(&run, TaskId::new(1))),
                "job" => Some(Principal {
                    role: crate::domain::ActorRole::ReviewJob,
                    ..Principal::worker(&run, TaskId::new(1))
                }),
                "broken" => return Err(anyhow!("unreadable")),
                _ => None,
            })
        }
        fn run_status(&self, _: &RunId) -> Result<Option<RunStatus>> {
            Ok(self.status)
        }
        fn forward(&self, line: &[u8]) -> Result<Vec<u8>> {
            if !self.reachable {
                return Err(anyhow!("no service"));
            }
            self.forwarded
                .lock()
                .unwrap()
                .push(serde_json::from_slice(line)?);
            Ok(b"{\"ok\":true,\"verbatim\":1}\n".to_vec())
        }
        fn record_denied(&self, payload: Value) -> Result<()> {
            self.denied.lock().unwrap().push(payload);
            Ok(())
        }
        fn egress(&self) -> Result<Egress> {
            self.egress
                .clone()
                .ok_or_else(|| anyhow!("no main checkout"))
        }
        fn verification_run(&self, token: &str) -> Result<Option<RunId>> {
            Ok((token == "verify").then(|| RunId::new("r1").unwrap()))
        }
        fn claude_credential(&self) -> Result<ClaudeCredential, ClaudeCredentialProblem> {
            self.credential.clone()
        }
    }

    fn body() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "api_version": API_VERSION,
            "token": "forged",
            "use_case": UseCase::Show,
            "params": {"id": 1},
        }))
        .unwrap()
    }

    fn queue_request(token: Option<&str>, run: Option<&str>) -> BrokerRequest {
        BrokerRequest {
            method: "POST".into(),
            target: crate::domain::queue_broker::QUEUE_PATH.into(),
            host: Some("127.0.0.1:1".into()),
            authorization: token.map(|token| format!("Bearer {token}")),
            run: run.map(str::to_owned),
            body: body(),
            ..BrokerRequest::default()
        }
    }

    fn handle(backend: &Backend, request: &BrokerRequest) -> (u16, Value) {
        let answer = QueueBroker {
            backend,
            build: "b1",
            pid: 7,
        }
        .handle(request);
        (answer.status, serde_json::from_slice(&answer.body).unwrap())
    }

    #[test]
    fn a_live_run_s_own_token_passes_with_the_token_in_place_of_the_body_s() {
        let backend = Backend::new(Some(RunStatus::Running));
        let request = BrokerRequest {
            authorization: Some("bearer good".into()),
            ..queue_request(None, Some("r1"))
        };
        let (status, body) = handle(&backend, &request);
        assert_eq!(status, 200);
        assert_eq!(body, json!({"ok": true, "verbatim": 1}));
        let forwarded = backend.forwarded.lock().unwrap();
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0]["token"], "good");
        assert_eq!(forwarded[0]["use_case"], "show");
        assert_eq!(forwarded[0]["params"], json!({"id": 1}));
        assert!(backend.denied.lock().unwrap().is_empty());
    }

    fn claude_request(token: Option<&str>, run: Option<&str>) -> BrokerRequest {
        let mut headers = vec![
            ("Host".to_owned(), "127.0.0.1:1".to_owned()),
            ("anthropic-version".to_owned(), "2023-06-01".to_owned()),
        ];
        if let Some(token) = token {
            headers.push(("Authorization".to_owned(), format!("Bearer {token}")));
        }
        if let Some(run) = run {
            headers.push(("x-dagq-run".to_owned(), run.to_owned()));
        }
        BrokerRequest {
            method: "POST".into(),
            target: "/anthropic/v1/messages?beta=true".into(),
            host: Some("127.0.0.1:1".into()),
            authorization: token.map(|token| format!("Bearer {token}")),
            run: run.map(str::to_owned),
            headers,
            body: b"{}".to_vec(),
            ..BrokerRequest::default()
        }
    }

    fn with_credential(status: Option<RunStatus>, kind: ClaudeCredentialKind) -> Backend {
        Backend {
            credential: Ok(ClaudeCredential {
                kind,
                secret: "the-secret".into(),
            }),
            ..Backend::new(status)
        }
    }

    #[test]
    fn a_claude_request_with_its_run_s_token_goes_on_with_the_chosen_credential_only() {
        for (kind, header, value) in [
            (
                ClaudeCredentialKind::SetupToken,
                "Authorization",
                "Bearer the-secret",
            ),
            (ClaudeCredentialKind::ApiKey, "x-api-key", "the-secret"),
        ] {
            let backend = with_credential(Some(RunStatus::Running), kind);
            let answer = QueueBroker {
                backend: &backend,
                build: "b1",
                pid: 7,
            }
            .handle(&claude_request(Some("good"), Some("r1")));
            let forward = answer.claude.expect("sent on");
            assert_eq!(forward.method, "POST");
            assert_eq!(forward.path, "/v1/messages?beta=true");
            assert_eq!(
                forward.headers,
                vec![
                    ("anthropic-version".to_owned(), "2023-06-01".to_owned()),
                    (header.to_owned(), value.to_owned()),
                ]
            );
            assert!(!format!("{forward:?}").contains("the-secret"));
            assert!(backend.denied.lock().unwrap().is_empty());
            assert!(backend.forwarded.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn a_claude_request_without_its_run_s_live_token_or_a_credential_is_refused_and_recorded() {
        use ClaudeCredentialKind::SetupToken;
        for (backend, token, run, reason) in [
            (
                with_credential(Some(RunStatus::Running), SetupToken),
                None,
                Some("r1"),
                "missing_token",
            ),
            (
                with_credential(Some(RunStatus::Running), SetupToken),
                Some("revoked"),
                Some("r1"),
                "unknown_token",
            ),
            (
                with_credential(Some(RunStatus::Running), SetupToken),
                Some("good"),
                Some("r2"),
                "other_run",
            ),
            (
                with_credential(Some(RunStatus::Integrated), SetupToken),
                Some("good"),
                Some("r1"),
                "run_ended",
            ),
            (
                with_credential(Some(RunStatus::Running), SetupToken),
                Some("verify"),
                Some("r1"),
                "verification_token",
            ),
            (
                Backend::new(Some(RunStatus::Running)),
                Some("good"),
                Some("r1"),
                "claude_credential_not_selected",
            ),
            (
                Backend {
                    credential: Err(ClaudeCredentialProblem::NotPrivate {
                        file: "/k".into(),
                        mode: 0o644,
                    }),
                    ..Backend::new(Some(RunStatus::Running))
                },
                Some("good"),
                Some("r1"),
                "claude_credential_not_private",
            ),
        ] {
            let answer = QueueBroker {
                backend: &backend,
                build: "b1",
                pid: 7,
            }
            .handle(&claude_request(token, run));
            assert!(answer.claude.is_none(), "{reason}");
            assert_eq!(answer.status, 401, "{reason}");
            let body: Value = serde_json::from_slice(&answer.body).unwrap();
            assert_eq!(body["error"]["code"], "unauthenticated");
            assert!(!body.to_string().contains("the-secret"), "{body}");
            if reason == "claude_credential_not_selected" {
                assert!(
                    body["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("api_key"),
                    "{body}"
                );
            }
            let denied = backend.denied.lock().unwrap();
            assert_eq!(denied.len(), 1, "{reason}");
            assert_eq!(denied[0]["reason"], reason);
            assert_eq!(denied[0]["destination"], "api.anthropic.com");
        }
        // An API that does not answer is the broker's refusal.
        let backend = with_credential(Some(RunStatus::Running), SetupToken);
        let broker = QueueBroker {
            backend: &backend,
            build: "b1",
            pid: 7,
        };
        let failed = broker.claude_failed(
            &claude_request(Some("good"), Some("r1")),
            &anyhow!("no route"),
        );
        assert_eq!(failed.status, 502);
        let denied = backend.denied.lock().unwrap();
        assert_eq!(denied[0]["code"], "unreachable");
        assert_eq!(denied[0]["destination"], "api.anthropic.com");
    }

    #[test]
    fn every_other_token_is_refused_and_recorded_with_its_run() {
        for (status, token, run, reason) in [
            (Some(RunStatus::Running), None, Some("r1"), "missing_token"),
            (
                Some(RunStatus::Running),
                Some(""),
                Some("r1"),
                "missing_token",
            ),
            (Some(RunStatus::Running), Some("good"), None, "missing_run"),
            (
                Some(RunStatus::Running),
                Some("revoked"),
                Some("r1"),
                "unknown_token",
            ),
            (
                Some(RunStatus::Running),
                Some("job"),
                Some("r1"),
                "not_a_run_token",
            ),
            (
                Some(RunStatus::Running),
                Some("good"),
                Some("r2"),
                "other_run",
            ),
            (
                Some(RunStatus::Integrated),
                Some("good"),
                Some("r1"),
                "run_ended",
            ),
            (None, Some("good"), Some("r1"), "run_ended"),
        ] {
            let backend = Backend::new(status);
            let (code, body) = handle(&backend, &queue_request(token, run));
            assert_eq!(code, 401, "{reason}");
            assert_eq!(body["ok"], false);
            assert_eq!(body["error"]["code"], "unauthenticated");
            assert_eq!(body["api_version"], API_VERSION);
            assert!(!body.to_string().contains("good"), "{body}");
            assert!(backend.forwarded.lock().unwrap().is_empty());
            let denied = backend.denied.lock().unwrap();
            assert_eq!(denied.len(), 1);
            assert_eq!(denied[0]["reason"], reason);
            assert_eq!(denied[0]["destination"], "queue_service");
            assert_eq!(denied[0]["run"], json!(run));
            assert_eq!(denied[0]["code"], "unauthenticated");
            if reason == "other_run" {
                assert_eq!(denied[0]["token_run"], "r1");
            } else {
                assert!(denied[0].get("token_run").is_none());
            }
        }
    }

    #[test]
    fn a_destination_not_built_yet_is_refused_and_recorded_with_its_host() {
        let backend = Backend::new(Some(RunStatus::Running));
        for (method, target, host, named) in [
            ("CONNECT", "crates.io:443", None, "crates.io"),
            ("CONNECT", "crates.io", None, "crates.io"),
            (
                "POST",
                "/v1/messages",
                Some("api.anthropic.com"),
                "api.anthropic.com",
            ),
            ("GET", "/anything", Some("example.com"), "example.com"),
        ] {
            let request = BrokerRequest {
                method: method.into(),
                target: target.into(),
                host: host.map(str::to_owned),
                authorization: Some("Bearer good".into()),
                run: Some("r1".into()),
                ..BrokerRequest::default()
            };
            let (status, body) = handle(&backend, &request);
            assert_eq!(status, 403);
            assert_eq!(body["error"]["code"], "forbidden_destination");
            let denied = backend.denied.lock().unwrap();
            let last = denied.last().unwrap();
            assert_eq!(last["destination"], named);
            assert_eq!(last["run"], "r1");
            assert_eq!(last["method"], method);
        }
        assert!(backend.forwarded.lock().unwrap().is_empty());
    }

    fn connect(target: &str, proxy: Option<&str>) -> BrokerRequest {
        BrokerRequest {
            method: "CONNECT".into(),
            target: target.into(),
            host: Some(target.into()),
            proxy_authorization: proxy.map(str::to_owned),
            ..BrokerRequest::default()
        }
    }

    fn egress(registries: &[&str], upstream: &[&str]) -> Egress {
        Egress {
            registries: registries.iter().map(|r| r.parse().unwrap()).collect(),
            upstream_git_hosts: upstream.iter().map(|h| (*h).to_owned()).collect(),
        }
    }

    /// base64 of `r1:good`, `r1:verify`, `r1:job`, `r1:nope`.
    const BASIC_GOOD: &str = "Basic cjE6Z29vZA==";
    const BASIC_VERIFY: &str = "Basic cjE6dmVyaWZ5";
    const BASIC_JOB: &str = "Basic cjE6am9i";
    const BASIC_NOPE: &str = "Basic cjE6bm9wZQ==";

    #[test]
    fn a_tunnel_opens_only_to_a_registry_dagq_toml_names_with_the_run_s_token() {
        let backend = Backend {
            egress: Some(egress(&["crates.io", "github.com"], &["github.com"])),
            ..Backend::new(Some(RunStatus::Running))
        };
        let broker = QueueBroker {
            backend: &backend,
            build: "b1",
            pid: 7,
        };
        // A worker's run token and its verification's, in a proxy URL's
        // credentials or as a bearer with the run header.
        for request in [
            connect("crates.io:443", Some(BASIC_GOOD)),
            connect("Crates.IO:443", Some(BASIC_VERIFY)),
            BrokerRequest {
                authorization: Some("Bearer good".into()),
                run: Some("r1".into()),
                ..connect("crates.io:443", None)
            },
            BrokerRequest {
                run: Some("r1".into()),
                ..connect("crates.io:443", Some("Bearer verify"))
            },
        ] {
            let answer = broker.handle(&request);
            assert_eq!(
                (answer.status, answer.tunnel),
                (200, Some(("crates.io".to_owned(), 443))),
                "{request:?}"
            );
        }
        assert!(backend.denied.lock().unwrap().is_empty());
        // Any other host, port or token is refused and recorded with the
        // run and the host; the answer says why, and names no token.
        for (request, status, reason, host) in [
            (
                connect("example.com:443", Some(BASIC_GOOD)),
                403,
                "not_allowed",
                "example.com",
            ),
            (
                connect("crates.io:80", Some(BASIC_GOOD)),
                403,
                "not_allowed",
                "crates.io",
            ),
            (
                connect("github.com:443", Some(BASIC_GOOD)),
                403,
                "upstream_git_host",
                "github.com",
            ),
            (
                connect("api.anthropic.com:443", Some(BASIC_VERIFY)),
                403,
                "builtin_destination",
                "api.anthropic.com",
            ),
            (
                connect("crates.io:443", Some(BASIC_JOB)),
                401,
                "not_a_run_token",
                "crates.io",
            ),
            (
                connect("crates.io:443", Some(BASIC_NOPE)),
                401,
                "unknown_token",
                "crates.io",
            ),
        ] {
            let answer = broker.handle(&request);
            assert_eq!((answer.status, &answer.tunnel), (status, &None), "{reason}");
            let body: Value = serde_json::from_slice(&answer.body).unwrap();
            assert!(!body.to_string().contains("verify"), "{body}");
            let denied = backend.denied.lock().unwrap();
            let last = denied.last().unwrap();
            assert_eq!(
                (&last["reason"], &last["destination"], &last["run"]),
                (&json!(reason), &json!(host), &json!("r1")),
            );
            if reason == "upstream_git_host" {
                let message = body["error"]["message"].as_str().unwrap();
                assert!(message.contains("upstream git remote"), "{message}");
            }
        }
        // A token without a run, and one of a run that ended.
        let answer = broker.handle(&connect("crates.io:443", Some("Bearer good")));
        assert_eq!(answer.status, 401);
        let ended = Backend {
            egress: Some(egress(&["crates.io"], &[])),
            ..Backend::new(Some(RunStatus::Integrated))
        };
        for token in [BASIC_GOOD, BASIC_VERIFY] {
            let answer = QueueBroker {
                backend: &ended,
                build: "b1",
                pid: 7,
            }
            .handle(&connect("crates.io:443", Some(token)));
            assert_eq!(answer.status, 401);
            assert_eq!(
                ended.denied.lock().unwrap().last().unwrap()["reason"],
                "run_ended"
            );
        }
        // A tunnel whose host does not answer is the broker's
        // `unreachable`, recorded with its run.
        let answer = broker.tunnel_failed(
            &connect("crates.io:443", Some(BASIC_GOOD)),
            &anyhow!("connection refused"),
        );
        assert_eq!(answer.status, 502);
        let denied = backend.denied.lock().unwrap();
        assert_eq!(
            (
                &denied.last().unwrap()["code"],
                &denied.last().unwrap()["run"]
            ),
            (&json!("unreachable"), &json!("r1"))
        );
    }

    #[test]
    fn no_tunnel_opens_without_registries_or_when_they_cannot_be_read() {
        for (egress, reason) in [
            (Some(Egress::default()), "not_allowed"),
            (None, "egress_unreadable"),
        ] {
            let backend = Backend {
                egress,
                ..Backend::new(Some(RunStatus::Running))
            };
            let (status, body) = handle(&backend, &connect("crates.io:443", Some(BASIC_GOOD)));
            assert_eq!(
                (status, &body["error"]["code"]),
                (403, &json!("forbidden_destination"))
            );
            let denied = backend.denied.lock().unwrap();
            assert_eq!(
                (
                    &denied[0]["reason"],
                    &denied[0]["destination"],
                    &denied[0]["run"]
                ),
                (&json!(reason), &json!("crates.io"), &json!("r1"))
            );
        }
    }

    #[test]
    fn a_verification_s_token_never_reaches_the_queue_service() {
        let backend = Backend::new(Some(RunStatus::Integrating));
        let (status, body) = handle(&backend, &queue_request(Some("verify"), Some("r1")));
        assert_eq!(
            (status, &body["error"]["code"]),
            (401, &json!("unauthenticated"))
        );
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("only the registries"),
            "{body}"
        );
        assert!(backend.forwarded.lock().unwrap().is_empty());
        assert_eq!(
            backend.denied.lock().unwrap()[0]["reason"],
            "verification_token"
        );
    }

    struct Tokens {
        issued: Mutex<Vec<String>>,
        revoked: Mutex<Vec<String>>,
        fail_revoke: bool,
    }

    impl VerificationTokens for Tokens {
        fn issue(&self, run: &RunId, _: TokenFor) -> Result<PathBuf> {
            self.issued.lock().unwrap().push(run.to_string());
            Ok(PathBuf::from(format!("/tokens/{run}")))
        }
        fn revoke(&self, run: &RunId, _: TokenFor) -> Result<bool> {
            self.revoked.lock().unwrap().push(run.to_string());
            if self.fail_revoke {
                return Err(anyhow!("gone"));
            }
            Ok(true)
        }
        fn issued(&self, _: &RunId, _: TokenFor) -> Result<Option<TokenIssue>> {
            Ok(None)
        }
        fn revoke_issued(&self, _: &RunId, _: TokenFor, _: &TokenIssue) -> Result<bool> {
            unreachable!()
        }
        fn sweep(&self, _: &mut dyn FnMut(&RunId, TokenFor) -> Result<bool>) -> Result<TokenSweep> {
            Err(anyhow!("unreadable"))
        }
    }

    /// The tokens left in the broker's directory: each one's run, what it
    /// is for and its issue (one more each time one is issued); what a
    /// sweep revoked goes, and a sweep that cannot read the directory
    /// fails.
    struct Left {
        held: Mutex<Vec<(RunId, TokenFor, u32)>>,
        issues: std::sync::atomic::AtomicU32,
        unreadable: bool,
        fail_revoke: bool,
    }

    impl Left {
        fn new(held: &[(&str, TokenFor)]) -> Self {
            Self {
                held: Mutex::new(
                    held.iter()
                        .map(|(run, of)| (RunId::new(*run).unwrap(), *of, 0))
                        .collect(),
                ),
                issues: std::sync::atomic::AtomicU32::new(1),
                unreadable: false,
                fail_revoke: false,
            }
        }

        /// The run and the use of each token left.
        fn left(&self) -> Vec<(RunId, TokenFor)> {
            self.held
                .lock()
                .unwrap()
                .iter()
                .map(|(run, of, _)| (run.clone(), *of))
                .collect()
        }
    }

    impl VerificationTokens for Left {
        fn issue(&self, run: &RunId, of: TokenFor) -> Result<PathBuf> {
            let issue = self
                .issues
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut held = self.held.lock().unwrap();
            held.retain(|(held_run, held_of, _)| (held_run, *held_of) != (run, of));
            held.push((run.clone(), of, issue));
            Ok(PathBuf::from(format!("/tokens/{run}/{issue}")))
        }
        fn revoke(&self, run: &RunId, of: TokenFor) -> Result<bool> {
            if self.fail_revoke {
                return Err(anyhow!("read-only"));
            }
            let mut held = self.held.lock().unwrap();
            let before = held.len();
            held.retain(|(held_run, held_of, _)| (held_run, *held_of) != (run, of));
            Ok(held.len() < before)
        }
        fn issued(&self, run: &RunId, of: TokenFor) -> Result<Option<TokenIssue>> {
            Ok(self
                .held
                .lock()
                .unwrap()
                .iter()
                .find(|(held_run, held_of, _)| (held_run, *held_of) == (run, of))
                .map(|(_, _, issue)| TokenIssue(issue.to_string())))
        }
        fn revoke_issued(&self, run: &RunId, of: TokenFor, issue: &TokenIssue) -> Result<bool> {
            if self.issued(run, of)?.as_ref() != Some(issue) {
                return Ok(false);
            }
            self.revoke(run, of)
        }
        fn sweep(
            &self,
            keep: &mut dyn FnMut(&RunId, TokenFor) -> Result<bool>,
        ) -> Result<TokenSweep> {
            if self.unreadable {
                return Err(anyhow!("unreadable"));
            }
            let mut sweep = TokenSweep::default();
            for (run, of) in self.left() {
                if !keep(&run, of)? && self.revoke(&run, of)? {
                    sweep.revoked.push((run, of));
                }
            }
            Ok(sweep)
        }
    }

    #[test]
    fn a_verification_runs_only_in_the_statuses_of_what_its_token_is_for() {
        use RunStatus::*;
        let held = |of: TokenFor| {
            [
                Claimed,
                Starting,
                Running,
                Succeeded,
                Validating,
                AwaitingIntegration,
                NeedsSession,
                Integrating,
                Integrated,
                Failed,
                Interrupted,
                Replaced,
            ]
            .into_iter()
            .filter(|status| of.verifies_while(*status))
            .collect::<Vec<_>>()
        };
        assert_eq!(held(TokenFor::Landing), [Integrating]);
        assert_eq!(
            held(TokenFor::Recheck),
            [AwaitingIntegration, NeedsSession, Integrating]
        );
        assert_eq!(held(TokenFor::Snapshot), [Validating]);
    }

    /// A token whose run is gone or rests where its verification does not
    /// run is revoked, the one of a verification that runs is kept, and a
    /// run whose status does not read keeps its token; a second sweep has
    /// nothing to do.
    #[test]
    fn the_sweep_revokes_the_tokens_no_verification_runs_with_and_is_idempotent() {
        let tokens = Left::new(&[
            ("landing", TokenFor::Landing),
            ("rested", TokenFor::Landing),
            ("gone", TokenFor::Snapshot),
            ("waiting", TokenFor::Recheck),
            ("unread", TokenFor::Landing),
        ]);
        let status_of = |run: &RunId| match run.as_str() {
            "landing" => Ok(Some(RunStatus::Integrating)),
            "rested" => Ok(Some(RunStatus::AwaitingIntegration)),
            "waiting" => Ok(Some(RunStatus::AwaitingIntegration)),
            "unread" => Err(anyhow!("locked")),
            _ => Ok(None),
        };
        let sweep = sweep_left_tokens(&tokens, &status_of);
        let run = |id: &str| RunId::new(id).unwrap();
        assert_eq!(
            sweep.revoked,
            [
                (run("rested"), TokenFor::Landing),
                (run("gone"), TokenFor::Snapshot)
            ]
        );
        assert_eq!(sweep.failures.len(), 1, "{:?}", sweep.failures);
        assert!(sweep.failures[0].contains("unread"));
        assert_eq!(
            tokens.left(),
            [
                (run("landing"), TokenFor::Landing),
                (run("waiting"), TokenFor::Recheck),
                (run("unread"), TokenFor::Landing)
            ]
        );
        let again = sweep_left_tokens(&tokens, &|run| match run.as_str() {
            "unread" => Ok(Some(RunStatus::Integrating)),
            other => status_of(&RunId::new(other).unwrap()),
        });
        assert!(again.is_empty(), "{again:?}");
        assert_eq!(
            again.payload("broker"),
            json!({"by": "broker", "revoked": [], "orphans": 0, "failures": []})
        );
        assert_eq!(
            sweep.payload("supervisor")["revoked"],
            json!([{"run": "rested", "for": "landing"}, {"run": "gone", "for": "snapshot"}])
        );
    }

    /// A sweep that cannot read the tokens fails nothing: it is a failure to
    /// log and record, and the start goes on.
    #[test]
    fn a_sweep_that_cannot_read_the_tokens_is_a_failure_not_an_error() {
        let mut tokens = Left::new(&[("rested", TokenFor::Landing)]);
        tokens.unreadable = true;
        let sweep = sweep_left_tokens(&tokens, &|_| Ok(None));
        assert!(sweep.revoked.is_empty());
        assert_eq!(sweep.failures, ["unreadable"]);
        assert_eq!(tokens.left().len(), 1);
    }

    /// A recovered run's tokens of the status it left go: the landing's of
    /// an `integrating` run gone back to wait, the snapshot's of a
    /// `validating` one interrupted, never another run's nor the recheck's
    /// of a run that may still be rechecked; again, there is nothing to
    /// revoke; a revocation that fails is a failure.
    #[test]
    fn a_recovered_run_s_tokens_of_the_status_it_left_are_revoked_once() {
        use RunStatus::*;
        let tokens = Left::new(&[
            ("r1", TokenFor::Landing),
            ("r1", TokenFor::Recheck),
            ("r1", TokenFor::Snapshot),
            ("r2", TokenFor::Landing),
        ]);
        let r1 = RunId::new("r1").unwrap();
        let revoked =
            LeftTokens::read(&tokens, &r1, Integrating).revoke(&tokens, AwaitingIntegration);
        assert_eq!(revoked.revoked, [(r1.clone(), TokenFor::Landing)]);
        assert!(
            LeftTokens::read(&tokens, &r1, Integrating)
                .revoke(&tokens, AwaitingIntegration)
                .is_empty()
        );
        let revoked = LeftTokens::read(&tokens, &r1, Validating).revoke(&tokens, Interrupted);
        assert_eq!(revoked.revoked, [(r1.clone(), TokenFor::Snapshot)]);
        assert_eq!(tokens.left().len(), 2);
        let mut failing = Left::new(&[("r1", TokenFor::Landing)]);
        failing.fail_revoke = true;
        let failed =
            LeftTokens::read(&failing, &r1, Integrating).revoke(&failing, AwaitingIntegration);
        assert!(failed.revoked.is_empty());
        assert_eq!(failed.failures.len(), 1);
        assert!(failed.failures[0].contains("read-only"));
    }

    /// A landing that began on the run between the read and the recovery's
    /// revocation issued its token again: the issue read goes, the new one
    /// stays for the verification that runs with it.
    #[test]
    fn a_token_issued_again_after_the_read_is_not_revoked_by_the_recovery() {
        use RunStatus::*;
        let tokens = Left::new(&[("r1", TokenFor::Landing)]);
        let r1 = RunId::new("r1").unwrap();
        let left = LeftTokens::read(&tokens, &r1, Integrating);
        tokens.issue(&r1, TokenFor::Landing).unwrap();
        let revoked = left.revoke(&tokens, AwaitingIntegration);
        assert!(revoked.is_empty(), "{revoked:?}");
        assert_eq!(tokens.left(), [(r1, TokenFor::Landing)]);
    }

    /// Only a sweep that did or failed something is recorded, and a record
    /// that fails is only logged.
    #[test]
    fn a_sweep_is_recorded_only_when_it_did_or_failed_something() {
        let recorded = Mutex::new(Vec::new());
        note_token_sweep(&TokenSweep::default(), "broker", |payload| {
            recorded.lock().unwrap().push(payload);
            Ok(())
        });
        assert!(recorded.lock().unwrap().is_empty());
        let failed = TokenSweep {
            failures: vec!["unreadable".to_owned()],
            ..TokenSweep::default()
        };
        note_token_sweep(&failed, "supervisor", |payload| {
            recorded.lock().unwrap().push(payload);
            Ok(())
        });
        assert_eq!(
            *recorded.lock().unwrap(),
            [json!({"by": "supervisor", "revoked": [], "orphans": 0, "failures": ["unreadable"]})]
        );
        note_token_sweep(&failed, "broker", |_| Err(anyhow!("the queue is locked")));
    }

    #[test]
    fn a_verification_s_token_is_revoked_when_the_verification_ends() {
        for fail_revoke in [false, true] {
            let tokens = Tokens {
                issued: Mutex::new(Vec::new()),
                revoked: Mutex::new(Vec::new()),
                fail_revoke,
            };
            let run = RunId::new("r1").unwrap();
            {
                let token = VerificationToken::issue(&tokens, &run, TokenFor::Landing).unwrap();
                assert_eq!(token.file, PathBuf::from("/tokens/r1"));
                assert_eq!(*tokens.issued.lock().unwrap(), ["r1"]);
                assert!(tokens.revoked.lock().unwrap().is_empty());
            }
            assert_eq!(*tokens.revoked.lock().unwrap(), ["r1"]);
        }
    }

    #[test]
    fn a_body_that_does_not_read_or_a_service_that_does_not_answer_is_the_broker_s_refusal() {
        // A token sent in the wrong field, or as a use case, is copied into
        // neither the record nor the answer.
        for (raw, kind) in [
            (
                r#"{"api_version": "secret-in-the-wrong-field"}"#,
                "data error",
            ),
            (
                r#"{"api_version": 1, "use_case": "secret-as-a-use-case"}"#,
                "data error",
            ),
            (r#"{"api_version": "secret-cut"#, "eof error"),
        ] {
            let backend = Backend::new(Some(RunStatus::Running));
            let request = BrokerRequest {
                body: raw.as_bytes().to_vec(),
                ..queue_request(Some("good"), Some("r1"))
            };
            let (status, body) = handle(&backend, &request);
            assert_eq!(
                (status, &body["error"]["code"]),
                (400, &json!("bad_request"))
            );
            let message = body["error"]["message"].as_str().unwrap();
            assert!(message.contains("does not read"), "{body}");
            assert!(message.contains(kind), "{body}");
            assert!(message.contains("line 1"), "{body}");
            assert!(!body.to_string().contains("secret"), "{body}");
            let denied = backend.denied.lock().unwrap().clone();
            assert_eq!(denied[0]["reason"], "bad_body");
            assert!(!denied[0].to_string().contains("secret"), "{denied:?}");
            assert!(backend.forwarded.lock().unwrap().is_empty());
        }
        let backend = Backend {
            reachable: false,
            ..Backend::new(Some(RunStatus::Running))
        };
        let (status, body) = handle(&backend, &queue_request(Some("good"), Some("r1")));
        assert_eq!(
            (status, &body["error"]["code"]),
            (502, &json!("unreachable"))
        );
        assert_eq!(backend.denied.lock().unwrap()[0]["code"], "unreachable");
        let (status, body) = handle(&backend, &queue_request(Some("broken"), Some("r1")));
        assert_eq!((status, &body["error"]["code"]), (500, &json!("failed")));
        let broker = QueueBroker {
            backend: &backend,
            build: "b1",
            pid: 7,
        };
        let answer = broker.malformed(&BrokerRequest::default(), "no request line");
        assert_eq!(answer.status, 400);
        let body: Value = serde_json::from_slice(&answer.body).unwrap();
        assert_eq!(body["error"]["message"], "no request line");
        assert_eq!(
            backend.denied.lock().unwrap().last().unwrap()["destination"],
            "unknown"
        );
        // What was read of a request that does not read is recorded.
        let read = BrokerRequest {
            method: "CONNECT".into(),
            target: "crates.io:443".into(),
            run: Some("r1".into()),
            ..BrokerRequest::default()
        };
        broker.malformed(&read, "a Transfer-Encoding");
        let denied = backend.denied.lock().unwrap();
        let last = denied.last().unwrap();
        assert_eq!(
            (&last["run"], &last["destination"], &last["reason"]),
            (&json!("r1"), &json!("crates.io"), &json!("malformed"))
        );
    }

    #[test]
    fn the_health_answers_without_a_token() {
        let backend = Backend::new(None);
        let request = BrokerRequest {
            method: "GET".into(),
            target: crate::domain::queue_broker::HEALTH_PATH.into(),
            ..BrokerRequest::default()
        };
        let (status, body) = handle(&backend, &request);
        assert_eq!(status, 200);
        assert_eq!(
            body,
            json!({"service": SERVICE_NAME, "build": "b1", "pid": 7})
        );
        assert!(backend.denied.lock().unwrap().is_empty());
    }

    struct Control {
        found: BrokerProbe,
        starts: Mutex<u32>,
    }

    impl QueueBrokerControl for Control {
        fn probe(&self) -> BrokerProbe {
            self.found.clone()
        }
        fn start(&self, _: Duration) -> Result<BrokerProbe> {
            *self.starts.lock().unwrap() += 1;
            Ok(BrokerProbe {
                state: ServiceState::Running,
                build_matches: Some(true),
                ..self.found.clone()
            })
        }
        fn stop(&self, _: Duration) -> Result<Option<u32>> {
            Ok(None)
        }
    }

    fn probe(state: ServiceState, build_matches: Option<bool>) -> BrokerProbe {
        BrokerProbe {
            state,
            address: None,
            pid: None,
            build: None,
            build_matches,
            started_at: None,
            error: None,
        }
    }

    #[test]
    fn up_reuses_a_current_broker_and_starts_or_replaces_any_other() {
        for (found, outcome, starts) in [
            (probe(ServiceState::Running, Some(true)), "reused", 0),
            (probe(ServiceState::Stopped, None), "started", 1),
            (probe(ServiceState::Running, Some(false)), "replaced", 1),
            (probe(ServiceState::Unreachable, None), "replaced", 1),
        ] {
            let control = Control {
                found,
                starts: Mutex::new(0),
            };
            let report = ensure(&control, Duration::from_secs(1)).unwrap();
            assert_eq!(report["outcome"], outcome);
            assert_eq!(*control.starts.lock().unwrap(), starts);
            assert_eq!(report["broker"]["state"], "running");
        }
    }

    struct Ready;

    impl super::super::ContainerRuntime for Ready {
        fn availability(&self, _: RuntimeTarget) -> Availability {
            Availability::Available
        }
    }

    #[test]
    fn the_container_cannot_be_used_while_the_broker_does_not_run() {
        use super::super::ContainerRuntime;
        for (state, available) in [
            (ServiceState::Running, true),
            (ServiceState::Stopped, false),
            (ServiceState::Unreachable, false),
        ] {
            let gate = BrokerGate {
                broker: Arc::new(Control {
                    found: probe(state, Some(true)),
                    starts: Mutex::new(0),
                }),
                container: Ready,
            };
            match gate.availability(RuntimeTarget::Worker) {
                Availability::Available => assert!(available),
                Availability::Unavailable(why) => {
                    assert!(!available);
                    assert!(why.contains("queue broker"), "{why}");
                    assert!(why.contains(state.as_str()), "{why}");
                }
            }
        }
    }
}
