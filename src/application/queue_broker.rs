//! The queue broker's use cases ([Queue broker], ADR-t2114-1 decisions 1
//! and 2, ADR-t1233-4 decisions 1, 2 and 4): how `up`, `down` and the
//! supervisor start, look at and stop it ([`QueueBrokerControl`],
//! [`ensure`]); what it does with one request ([`QueueBroker::handle`]):
//! a request for the queue service passes only with a worker's run token
//! of the run it names, and goes on to the service with that token, so
//! the service acts as the run's principal; and how the container runtime
//! cannot be used while the broker does not run ([`BrokerGate`]).
//!
//! The protocol's shapes are [`crate::domain::queue_broker`]'s.
//!
//! [Queue broker]: ../../docs/design/queue-broker.md

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tracing::warn;

use crate::domain::RunId;
use crate::domain::RunStatus;
use crate::domain::container_runtime::{Availability, RuntimeTarget};
use crate::domain::queue_broker::{
    BEARER, Destination, Refusal, SERVICE_NAME, TokenRefusal, judge_run_token,
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
    /// The run header ([`crate::domain::queue_broker::RUN_HEADER`]).
    pub run: Option<String>,
    pub body: Vec<u8>,
}

/// The broker's answer: an HTTP status and the body, JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerAnswer {
    pub status: u16,
    pub body: Vec<u8>,
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
            Destination::Outside(_) => {
                self.refuse(request, &destination, None, Refusal::destination(), None)
            }
            Destination::QueueService => match self.pass_on(request) {
                Ok(body) => BrokerAnswer { status: 200, body },
                Err(Refused {
                    refusal,
                    token_run,
                    message,
                }) => self.refuse(
                    request,
                    &destination,
                    token_run,
                    refusal,
                    message.as_deref(),
                ),
            },
        }
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
        self.refuse(read, &destination, None, refusal, Some(message))
    }

    /// Check the run token and pass the request on with it: the service's
    /// answer, or why not with the token's run when it differs.
    fn pass_on(&self, request: &BrokerRequest) -> Result<Vec<u8>, Refused> {
        let token_refused = |refusal: TokenRefusal| Refused::from(Refusal::token(refusal));
        let token = request
            .authorization
            .as_deref()
            .and_then(|value| bearer(value))
            .filter(|token| !token.is_empty())
            .ok_or_else(|| token_refused(TokenRefusal::MissingToken))?;
        let run = request
            .run
            .as_deref()
            .filter(|run| !run.is_empty())
            .ok_or_else(|| token_refused(TokenRefusal::MissingRun))?;
        let principal = match self.backend.principal(token) {
            Ok(Some(principal)) => principal,
            Ok(None) => return Err(token_refused(TokenRefusal::UnknownToken)),
            Err(error) => {
                return Err(failed(format!("the token could not be read: {error:#}")).into());
            }
        };
        let status = match principal.run_id.as_ref() {
            Some(own) => self.backend.run_status(own).map_err(|error| {
                Refused::from(failed(format!(
                    "the token's run could not be read: {error:#}"
                )))
            })?,
            None => None,
        };
        judge_run_token(&principal, run, status).map_err(|refusal| {
            let token_run = principal
                .run_id
                .clone()
                .filter(|own| refusal == TokenRefusal::OtherRun && own.as_str() != run);
            Refused {
                token_run,
                ..Refused::from(Refusal::token(refusal))
            }
        })?;
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

    fn refuse(
        &self,
        request: &BrokerRequest,
        destination: &Destination,
        token_run: Option<RunId>,
        refusal: Refusal,
        message: Option<&str>,
    ) -> BrokerAnswer {
        let mut payload = json!({
            "run": request.run,
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
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::anyhow;

    use super::*;
    use crate::domain::TaskId;
    use crate::domain::queue_service::{API_VERSION, UseCase};

    /// Run `r1` of task 1 in `status`, its worker's token `good`, a job's
    /// token on it `job`; what was forwarded and recorded is kept.
    struct Backend {
        status: Option<RunStatus>,
        reachable: bool,
        forwarded: Mutex<Vec<Value>>,
        denied: Mutex<Vec<Value>>,
    }

    impl Backend {
        fn new(status: Option<RunStatus>) -> Self {
            Self {
                status,
                reachable: true,
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
                body: Vec::new(),
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
