//! The queue service's use cases ([Queue service], ADR-t1233-1 decisions
//! 2 to 4): one request is one use case, authenticated by the token the
//! control side issued for its caller (ADR-t1233-4 decision 4), authorized
//! for that principal by the [`StaticPolicy`] on the service's side
//! (ADR-t1233-5 decision 2), and done in one transaction of the service's
//! own; nothing of it is split across round trips with the caller. A
//! refusal of the policy is recorded as `authorization_denied` as the
//! command line records it (the use cases run through
//! [`Dialogue`] and [`Gate`]), and a request that names no principal as
//! `queue_service_unauthenticated`.
//!
//! The use cases of goal 82's stage (2): `hello`, `ask`, `show` (a task
//! with its runs, the whole queue being readable to every role,
//! ADR-t1233-5 decision 3) and `note`. The supervisor and the command line
//! still open the DB themselves.
//!
//! [Queue service]: ../../docs/design/queue-service.md

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use super::commands::dialogue::{Dialogue, DialogueStore, MarkChange};
use super::commands::{DenialLog, Gate};
use crate::domain::queue_service::{
    API_VERSION, MIN_API_VERSION, Principal, ServiceErrorCode, ServiceRequest, ServiceResponse,
    ServiceState, UseCase, answers, run_holds_token,
};
use crate::domain::{
    ActorContext, Answerer, Ask, AskId, AskKind, AskReason, AuthorizationError, Capability,
    Finding, FindingId, FindingOutcome, FindingStatus, GoalId, NewAsk, NewFinding, NewNote,
    NoteTarget, Resource, RunEvent, RunId, RunStatus, StaticPolicy, TaskId,
};

/// The queue as one use case of the service reads and changes it, written
/// as the principal's actor.
pub trait ServiceQueue: DialogueStore {
    /// Task `id` as `dagq show` prints it: the whole detail with `full`,
    /// else its view with the last `events` events.
    fn show(&mut self, id: TaskId, full: bool, events: usize) -> Result<Value>;
    /// The status of run `id`; `None` for a run the queue does not know.
    fn run_status(&self, id: &RunId) -> Result<Option<RunStatus>>;
}

/// What the service runs its use cases on.
pub trait ServiceBackend: Send + Sync {
    /// The principal `token` was issued for; `None` for a token the
    /// control side did not issue or has revoked.
    fn principal(&self, token: &str) -> Result<Option<Principal>>;
    /// The queue, written as `actor`.
    fn open(&self, actor: &ActorContext) -> Result<Box<dyn ServiceQueue>>;
    /// Record a request that named no principal
    /// (`queue_service_unauthenticated`).
    fn record_unauthenticated(&self, payload: Value) -> Result<()>;
}

/// The service's use cases on a backend.
pub struct QueueService<'a> {
    pub backend: &'a dyn ServiceBackend,
    /// The service's build identifier, which `hello` names.
    pub build: &'a str,
    /// The service's process.
    pub pid: u32,
}

impl QueueService<'_> {
    /// Answer `request`. An error never leaves without its code, and no
    /// response names the token.
    pub fn handle(&self, request: &ServiceRequest) -> ServiceResponse {
        if !answers(request.api_version) {
            return ServiceResponse::failure(
                ServiceErrorCode::ApiVersionMismatch,
                format!(
                    "the queue service answers API versions {MIN_API_VERSION} to {API_VERSION}, \
                     not {}",
                    request.api_version
                ),
            );
        }
        if request.use_case == UseCase::Hello {
            return ServiceResponse::success(json!({
                "service": "dagq-queue-service",
                "build": self.build,
                "pid": self.pid,
                "api_version": API_VERSION,
                "min_api_version": MIN_API_VERSION,
            }));
        }
        let principal = match self.authenticate(request) {
            Ok(principal) => principal,
            Err(response) => return response,
        };
        let actor = principal.actor();
        let result = self
            .backend
            .open(&actor)
            .and_then(|mut queue| run(&mut *queue, &actor, request.use_case, &request.params));
        match result {
            Ok(value) => ServiceResponse::success(value),
            Err(error) => match error.downcast_ref::<AuthorizationError>() {
                Some(refused) => ServiceResponse::failure(
                    ServiceErrorCode::AuthorizationDenied,
                    refused.to_string(),
                ),
                None if error.downcast_ref::<BadParams>().is_some() => {
                    ServiceResponse::failure(ServiceErrorCode::BadRequest, format!("{error:#}"))
                }
                None => ServiceResponse::failure(ServiceErrorCode::Failed, format!("{error:#}")),
            },
        }
    }

    /// The principal of the request's token, whose run, when it names
    /// one, has not ended. A refusal is recorded with the use case and
    /// the reason, never the token.
    fn authenticate(&self, request: &ServiceRequest) -> Result<Principal, ServiceResponse> {
        let refuse = |reason: &str, message: &str| {
            let payload = json!({"use_case": request.use_case, "reason": reason});
            if let Err(error) = self.backend.record_unauthenticated(payload) {
                warn!("could not record the unauthenticated request ({reason}): {error:#}");
            }
            ServiceResponse::failure(ServiceErrorCode::Unauthenticated, message)
        };
        let Some(token) = request.token.as_deref().filter(|token| !token.is_empty()) else {
            return Err(refuse("missing_token", "the request carries no token"));
        };
        let principal = match self.backend.principal(token) {
            Ok(Some(principal)) => principal,
            Ok(None) => {
                return Err(refuse(
                    "unknown_token",
                    "the token is not one the control side issued, or it was revoked",
                ));
            }
            Err(error) => {
                return Err(ServiceResponse::failure(
                    ServiceErrorCode::Failed,
                    format!("the token could not be read: {error:#}"),
                ));
            }
        };
        if let Some(run) = &principal.run_id {
            let status = self
                .backend
                .open(&principal.actor())
                .and_then(|queue| queue.run_status(run));
            match status {
                Ok(Some(status)) if run_holds_token(status) => {}
                Ok(_) => {
                    return Err(refuse(
                        "run_ended",
                        "the token's run has ended or is not in the queue",
                    ));
                }
                Err(error) => {
                    return Err(ServiceResponse::failure(
                        ServiceErrorCode::Failed,
                        format!("the token's run could not be read: {error:#}"),
                    ));
                }
            }
        }
        Ok(principal)
    }
}

/// A request whose params do not read as the use case's.
#[derive(Debug)]
struct BadParams(String);

impl std::fmt::Display for BadParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BadParams {}

fn params<T: for<'de> Deserialize<'de>>(use_case: UseCase, params: &Value) -> Result<T> {
    let params = if params.is_null() { &json!({}) } else { params };
    serde_json::from_value(params.clone()).map_err(|error| {
        BadParams(format!(
            "the params of {} do not read: {error}",
            use_case.as_str()
        ))
        .into()
    })
}

/// `ask`'s params: those of `dagq ask`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskParams {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    because: Option<String>,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    task_id: Option<i64>,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    finding_id: Option<i64>,
}

/// `show`'s params: those of `dagq show`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShowParams {
    id: i64,
    #[serde(default)]
    full: bool,
    #[serde(default = "default_events")]
    events: usize,
}

const fn default_events() -> usize {
    5
}

/// `note`'s params: those of `dagq note`, one target.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoteParams {
    #[serde(default)]
    task: Option<i64>,
    #[serde(default)]
    run: Option<String>,
    #[serde(default)]
    goal: Option<i64>,
    text: String,
    #[serde(default)]
    kind: Option<String>,
}

/// Run `use_case` as `actor` on `queue`.
fn run(
    queue: &mut dyn ServiceQueue,
    actor: &ActorContext,
    use_case: UseCase,
    raw: &Value,
) -> Result<Value> {
    let mut store = Store(queue);
    match use_case {
        UseCase::Hello => unreachable!("hello is answered before a principal"),
        UseCase::Ask => {
            let p: AskParams = params(use_case, raw)?;
            let ask = NewAsk {
                kind: p.kind.unwrap_or_default().parse::<AskKind>()?,
                task_id: p.task_id.map(TaskId::new),
                run_id: p.run_id.map(RunId::new).transpose()?,
                question: p.question.unwrap_or_default(),
                options: p.options,
                asked_by: actor.written_by().to_owned(),
                reason_category: p.because.unwrap_or_default().parse::<AskReason>()?,
                topics: p.topics,
                finding_id: p.finding_id.map(FindingId::new),
            };
            Dialogue::new(&mut store, actor, &StaticPolicy).ask(ask)
        }
        UseCase::Note => {
            let p: NoteParams = params(use_case, raw)?;
            let target = match (p.task, p.run, p.goal) {
                (Some(task), None, None) => NoteTarget::Task(TaskId::new(task)),
                (None, Some(run), None) => NoteTarget::Run(RunId::new(run)?),
                (None, None, Some(goal)) => NoteTarget::Goal(GoalId::new(goal)),
                _ => {
                    return Err(
                        BadParams("note names one target: task, run or goal".to_owned()).into(),
                    );
                }
            };
            let note = Dialogue::new(&mut store, actor, &StaticPolicy).note(NewNote {
                target,
                text: p.text,
                kind: p.kind,
                by: actor.written_by().to_owned(),
            })?;
            Ok(serde_json::to_value(note)?)
        }
        UseCase::Show => {
            let p: ShowParams = params(use_case, raw)?;
            let id = TaskId::new(p.id);
            // Every role reads the whole queue in goal 82 (ADR-t1233-5
            // decision 3); the policy is still asked, on the service's
            // side, as it would be for a narrower one.
            Gate {
                actor,
                authorizer: &StaticPolicy,
            }
            .authorize(&store, Capability::QueueRead, &Resource::task(id))?;
            store.0.show(id, p.full, p.events)
        }
    }
}

/// A [`ServiceQueue`] as the [`Dialogue`] commands' store.
struct Store<'a>(&'a mut dyn ServiceQueue);

impl DenialLog for Store<'_> {
    fn record_denial(&self, payload: Value) -> Result<()> {
        self.0.record_denial(payload)
    }
}

impl DialogueStore for Store<'_> {
    fn read_ask(&self, id: AskId) -> Result<Ask> {
        self.0.read_ask(id)
    }
    fn open_ask(&mut self, ask: NewAsk) -> Result<Value> {
        self.0.open_ask(ask)
    }
    fn answer(&mut self, id: AskId, text: &str, answerer: Answerer) -> Result<Ask> {
        self.0.answer(id, text, answerer)
    }
    fn close_ask(&mut self, id: AskId) -> Result<Ask> {
        self.0.close_ask(id)
    }
    fn add_note(&mut self, note: NewNote) -> Result<RunEvent> {
        self.0.add_note(note)
    }
    fn mark(&mut self, change: MarkChange, by: &str) -> Result<Value> {
        self.0.mark(change, by)
    }
    fn record_finding(&mut self, finding: NewFinding) -> Result<FindingOutcome> {
        self.0.record_finding(finding)
    }
    fn set_finding_status(
        &mut self,
        id: FindingId,
        to: FindingStatus,
        reason: &str,
        by: &str,
    ) -> Result<Finding> {
        self.0.set_finding_status(id, to, reason, by)
    }
}

/// What a look at the queue's service found.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ServiceProbe {
    /// `running` (it answered `hello`), `stopped` (nothing recorded, or
    /// its process is gone) or `unreachable` (its process lives and does
    /// not answer).
    pub state: ServiceState,
    pub socket: std::path::PathBuf,
    pub pid: Option<u32>,
    pub build: Option<String>,
    pub api_version: Option<u32>,
    pub min_api_version: Option<u32>,
    /// Whether it runs this binary's build.
    pub build_matches: Option<bool>,
    pub started_at: Option<i64>,
    /// Why it does not answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ServiceProbe {
    /// Running this binary's build: nothing to do.
    pub fn current(&self) -> bool {
        self.state == ServiceState::Running && self.build_matches == Some(true)
    }
}

/// How `up`, `down` and the supervisor start, look at and stop the
/// queue's service (ADR-t1233-4 decisions 1 and 2).
pub trait QueueServiceControl: Send + Sync {
    /// Look at the service: its record and whether it answers `hello`.
    fn probe(&self) -> ServiceProbe;
    /// Start the service of this binary, stopping one of another build
    /// first, and wait up to `timeout` for it to answer.
    fn start(&self, timeout: Duration) -> Result<ServiceProbe>;
    /// Stop the service; `None` when none ran.
    fn stop(&self, timeout: Duration) -> Result<Option<u32>>;
}

/// `up`'s step (ADR-t1233-4 decision 1): reuse a service of this build
/// that answers, else start one (replacing one of another build), and
/// report what was done. A service that does not start stops `up`.
pub fn ensure(control: &dyn QueueServiceControl, timeout: Duration) -> Result<Value> {
    let found = control.probe();
    if found.current() {
        return Ok(json!({"outcome": "reused", "service": found}));
    }
    let replaced = (found.state != ServiceState::Stopped).then(|| found.clone());
    let started = control
        .start(timeout)
        .context("the queue service did not start")?;
    Ok(json!({
        "outcome": if replaced.is_some() { "replaced" } else { "started" },
        "service": started,
        "replaced": replaced,
    }))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Mutex;

    use anyhow::anyhow;

    use super::*;
    use crate::domain::{ActorRole, Answerer};

    /// One queue: task 1 with run `r1` in `status`; everything it was
    /// asked to write is kept.
    struct Queue {
        status: RunStatus,
        log: std::sync::Arc<Mutex<Vec<Value>>>,
        denials: RefCell<Vec<Value>>,
    }

    impl DenialLog for Queue {
        fn record_denial(&self, payload: Value) -> Result<()> {
            self.denials.borrow_mut().push(payload.clone());
            self.log.lock().unwrap().push(payload);
            Ok(())
        }
    }

    impl DialogueStore for Queue {
        fn read_ask(&self, _: AskId) -> Result<Ask> {
            Err(anyhow!("no ask"))
        }
        fn open_ask(&mut self, ask: NewAsk) -> Result<Value> {
            let value = json!({"opened": ask.kind.as_str(), "run": ask.run_id, "by": ask.asked_by});
            self.log.lock().unwrap().push(value.clone());
            Ok(value)
        }
        fn answer(&mut self, _: AskId, _: &str, _: Answerer) -> Result<Ask> {
            Err(anyhow!("answer"))
        }
        fn close_ask(&mut self, _: AskId) -> Result<Ask> {
            Err(anyhow!("close"))
        }
        fn add_note(&mut self, note: NewNote) -> Result<RunEvent> {
            self.log
                .lock()
                .unwrap()
                .push(json!({"note": note.text, "by": note.by}));
            Err(anyhow!("noted"))
        }
        fn mark(&mut self, _: MarkChange, _: &str) -> Result<Value> {
            Err(anyhow!("mark"))
        }
        fn record_finding(&mut self, _: NewFinding) -> Result<FindingOutcome> {
            Err(anyhow!("finding"))
        }
        fn set_finding_status(
            &mut self,
            _: FindingId,
            _: FindingStatus,
            _: &str,
            _: &str,
        ) -> Result<Finding> {
            Err(anyhow!("finding"))
        }
    }

    impl ServiceQueue for Queue {
        fn show(&mut self, id: TaskId, full: bool, events: usize) -> Result<Value> {
            Ok(json!({"id": id, "full": full, "events": events}))
        }
        fn run_status(&self, id: &RunId) -> Result<Option<RunStatus>> {
            Ok((id.as_str() == "r1").then_some(self.status))
        }
    }

    struct Backend {
        status: RunStatus,
        log: std::sync::Arc<Mutex<Vec<Value>>>,
    }

    impl ServiceBackend for Backend {
        fn principal(&self, token: &str) -> Result<Option<Principal>> {
            let run = RunId::new("r1")?;
            Ok(match token {
                "worker" => Some(Principal::worker(&run, TaskId::new(1))),
                "review" => Some(Principal::of(&ActorContext::review_job(&run, 1))),
                "broken" => return Err(anyhow!("unreadable")),
                _ => None,
            })
        }
        fn open(&self, _: &ActorContext) -> Result<Box<dyn ServiceQueue>> {
            Ok(Box::new(Queue {
                status: self.status,
                log: self.log.clone(),
                denials: RefCell::default(),
            }))
        }
        fn record_unauthenticated(&self, payload: Value) -> Result<()> {
            self.log.lock().unwrap().push(payload);
            Ok(())
        }
    }

    fn backend(status: RunStatus) -> Backend {
        Backend {
            status,
            log: std::sync::Arc::default(),
        }
    }

    fn request(token: Option<&str>, use_case: UseCase, params: Value) -> ServiceRequest {
        ServiceRequest {
            api_version: API_VERSION,
            token: token.map(str::to_owned),
            use_case,
            params,
        }
    }

    fn code(response: &ServiceResponse) -> Option<ServiceErrorCode> {
        response.error.as_ref().map(|error| error.code)
    }

    #[test]
    fn hello_needs_no_token_and_names_the_build_and_the_versions() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "1.0.0",
            pid: 42,
        };
        let response = service.handle(&request(None, UseCase::Hello, Value::Null));
        assert!(response.ok);
        let result = response.result.unwrap();
        assert_eq!(result["build"], "1.0.0");
        assert_eq!(result["pid"], 42);
        assert_eq!(result["api_version"], API_VERSION);
        // A caller of another version is refused before anything else.
        let mut newer = request(Some("worker"), UseCase::Show, json!({"id": 1}));
        newer.api_version = API_VERSION + 1;
        let response = service.handle(&newer);
        assert_eq!(code(&response), Some(ServiceErrorCode::ApiVersionMismatch));
        assert_eq!(response.api_version, API_VERSION);
        assert!(backend.log.lock().unwrap().is_empty());
    }

    #[test]
    fn a_request_without_a_principal_is_refused_and_recorded() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        for (token, reason) in [
            (None, "missing_token"),
            (Some(""), "missing_token"),
            (Some("forged"), "unknown_token"),
        ] {
            let response = service.handle(&request(token, UseCase::Show, json!({"id": 1})));
            assert_eq!(code(&response), Some(ServiceErrorCode::Unauthenticated));
            let recorded = backend.log.lock().unwrap().pop().unwrap();
            assert_eq!(recorded, json!({"use_case": "show", "reason": reason}));
        }
        let response = service.handle(&request(Some("broken"), UseCase::Show, json!({"id": 1})));
        assert_eq!(code(&response), Some(ServiceErrorCode::Failed));
        // A worker's token ends with its run.
        let ended = Backend {
            status: RunStatus::Integrated,
            ..backend
        };
        let service = QueueService {
            backend: &ended,
            build: "b",
            pid: 1,
        };
        let response = service.handle(&request(Some("worker"), UseCase::Show, json!({"id": 1})));
        assert_eq!(code(&response), Some(ServiceErrorCode::Unauthenticated));
        assert_eq!(
            ended.log.lock().unwrap().pop().unwrap()["reason"],
            "run_ended"
        );
    }

    #[test]
    fn the_policy_is_the_service_s_and_acts_as_the_principal() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        // The worker asks on its own run, as itself whatever it names.
        let response = service.handle(&request(
            Some("worker"),
            UseCase::Ask,
            json!({"kind": "worker_question", "because": "scope", "topics": ["design_choice"],
                   "question": "q", "run_id": "r1"}),
        ));
        assert!(response.ok, "{response:?}");
        assert_eq!(
            response.result.unwrap(),
            json!({"opened": "worker_question", "run": "r1", "by": "worker"})
        );
        // Not on another run: refused and recorded as the worker.
        let response = service.handle(&request(
            Some("worker"),
            UseCase::Ask,
            json!({"kind": "worker_question", "because": "scope", "topics": ["design_choice"],
                   "question": "q", "run_id": "r2"}),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::AuthorizationDenied));
        let denial = backend.log.lock().unwrap().pop().unwrap();
        assert_eq!(denial["event"], "authorization_denied");
        assert_eq!(denial["role"], "worker");
        assert_eq!(denial["capability"], "ask.open");
        // A review job asks nothing and notes nothing.
        for (use_case, params) in [
            (
                UseCase::Ask,
                json!({"kind": "worker_question", "because": "scope", "question": "q", "run_id": "r1"}),
            ),
            (UseCase::Note, json!({"task": 1, "text": "x"})),
        ] {
            let response = service.handle(&request(Some("review"), use_case, params));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{use_case:?}"
            );
            assert_eq!(
                backend.log.lock().unwrap().pop().unwrap()["role"],
                ActorRole::ReviewJob.as_str()
            );
        }
        // Every role reads (ADR-t1233-5 decision 3).
        for token in ["worker", "review"] {
            let response = service.handle(&request(
                Some(token),
                UseCase::Show,
                json!({"id": 9, "full": true}),
            ));
            assert_eq!(
                response.result,
                Some(json!({"id": 9, "full": true, "events": 5}))
            );
        }
        // The worker notes on its task; the store's own error comes back as
        // a failure.
        let response = service.handle(&request(
            Some("worker"),
            UseCase::Note,
            json!({"task": 1, "text": "x"}),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::Failed));
        assert_eq!(
            backend.log.lock().unwrap().pop().unwrap(),
            json!({"note": "x", "by": "worker"})
        );
    }

    #[test]
    fn params_that_do_not_read_are_a_bad_request() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        for (use_case, params) in [
            (UseCase::Show, json!({})),
            (UseCase::Show, json!({"id": 1, "table": "tasks"})),
            (UseCase::Note, json!({"text": "no target"})),
            (UseCase::Note, json!({"task": 1, "goal": 2, "text": "two"})),
            (UseCase::Ask, json!({"sql": "x"})),
        ] {
            let response = service.handle(&request(Some("worker"), use_case, params.clone()));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::BadRequest),
                "{params}"
            );
        }
    }

    struct Control {
        found: ServiceProbe,
        started: Mutex<u32>,
    }

    impl QueueServiceControl for Control {
        fn probe(&self) -> ServiceProbe {
            self.found.clone()
        }
        fn start(&self, _: Duration) -> Result<ServiceProbe> {
            *self.started.lock().unwrap() += 1;
            Ok(ServiceProbe {
                state: ServiceState::Running,
                build_matches: Some(true),
                ..self.found.clone()
            })
        }
        fn stop(&self, _: Duration) -> Result<Option<u32>> {
            Ok(None)
        }
    }

    fn probe(state: ServiceState, build_matches: Option<bool>) -> ServiceProbe {
        ServiceProbe {
            state,
            socket: "/q/service/queue.sock".into(),
            pid: None,
            build: None,
            api_version: None,
            min_api_version: None,
            build_matches,
            started_at: None,
            error: None,
        }
    }

    #[test]
    fn up_reuses_a_current_service_and_starts_or_replaces_any_other() {
        for (found, outcome, starts) in [
            (probe(ServiceState::Running, Some(true)), "reused", 0),
            (probe(ServiceState::Stopped, None), "started", 1),
            (probe(ServiceState::Running, Some(false)), "replaced", 1),
            (probe(ServiceState::Unreachable, None), "replaced", 1),
        ] {
            let control = Control {
                found,
                started: Mutex::new(0),
            };
            let report = ensure(&control, Duration::from_secs(1)).unwrap();
            assert_eq!(report["outcome"], outcome);
            assert_eq!(*control.started.lock().unwrap(), starts);
            assert_eq!(report["service"]["state"], "running");
        }
    }
}
