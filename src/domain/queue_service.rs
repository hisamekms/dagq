//! The queue service's protocol ([Queue service]): the host process that
//! opens the queue DB for the callers that are not to open it
//! (ADR-t1233-1 decision 2), with use cases rather than rows (decision 3),
//! the policy applied on its side to the principal a token names
//! (decision 4, ADR-t1233-4 decision 4), and an API version that decides
//! whether a caller and the service understand each other instead of the
//! queue's schema (decision 2).
//!
//! A request is one line of JSON on a connection to the queue's unix
//! socket (decision 5), and the response one line back; the connection
//! then closes. Nothing here names a token's value in an error or a
//! record.
//!
//! [Queue service]: ../../docs/design/queue-service.md

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ActorContext, ActorRole, DomainError, RunId, RunStatus, TaskId};

/// The API version this binary speaks. A change a caller of the version
/// before cannot read raises it.
pub const API_VERSION: u32 = 1;
/// The oldest API version the service still answers.
pub const MIN_API_VERSION: u32 = 1;
/// The service's directory under the queue's directory.
pub const SERVICE_DIR: &str = "service";
/// The socket in [`SERVICE_DIR`].
pub const SOCKET_FILE: &str = "queue.sock";
/// What the running service records of itself in [`SERVICE_DIR`].
pub const STATE_FILE: &str = "state.json";
/// The lock one service of a queue holds while it runs.
pub const LOCK_FILE: &str = "lock";
/// The log the service a runtime starts writes to.
pub const LOG_FILE: &str = "service.log";
/// The variable that names the socket to a client-mode `dagq` (goal 82's
/// stage (3)): with it, `dagq` opens no queue and sends its command to the
/// service (ADR-t1233-1 decision 7).
pub const SOCKET_ENV: &str = "DAGQ_SERVICE_SOCKET";
/// The variable that names the file holding a client's token (ADR-t1233-4
/// decision 4: the value itself is never in the environment). Its name
/// says no `TOKEN`: Codex keeps a variable whose name holds `KEY`,
/// `SECRET` or `TOKEN` out of the commands it runs.
pub const CREDENTIAL_FILE_ENV: &str = "DAGQ_SERVICE_CREDENTIAL_FILE";
/// The variable that names the process a service started with it stops
/// with: a test names its own pid, so a service it started stops within
/// seconds of the test's process going, however that ends (a timeout's
/// `process::exit`, a SIGKILL) and with its queue's directory left behind
/// (task 1352). Nothing in production sets it.
pub const OWNER_PID_ENV: &str = "DAGQ_SERVICE_OWNER_PID";
/// The variables of a client-mode `dagq`.
pub const CLIENT_ENV: [&str; 2] = [SOCKET_ENV, CREDENTIAL_FILE_ENV];
/// The longest request line the service reads.
pub const MAX_REQUEST_BYTES: usize = 1 << 20;

// The use cases of the API.
string_enum!(UseCase {
    // Whether the service answers, and its build and API version: the
    // only use case that takes no token.
    Hello => "hello",
    // Open an ask (a worker's `worker_question`, ...).
    Ask => "ask",
    // A task with its runs, as `dagq show` prints it.
    Show => "show",
    // Write a note on a task, a run or a goal.
    Note => "note",
    // The proposals awaiting plan review, as `dagq proposal list` prints
    // them.
    ProposalList => "proposal_list",
    // A proposal, as `dagq proposal show` prints it.
    ProposalShow => "proposal_show",
    // Record a finding, or update the open one of the same kind, target
    // and subject, as `dagq finding record` does.
    FindingRecord => "finding_record",
    // Mark a finding resolved, as `dagq finding resolve` does.
    FindingResolve => "finding_resolve",
    // Mark a finding dismissed, as `dagq finding dismiss` does.
    FindingDismiss => "finding_dismiss",
    // The reads of the whole queue a read role's job or a worker runs
    // (ADR-t1233-5 decisions 1 and 3), each as the command of its name
    // prints it.
    List => "list",
    Candidates => "candidates",
    Graph => "graph",
    Status => "status",
    Asks => "asks",
    Events => "events",
    Timeline => "timeline",
    Stats => "stats",
    Kpi => "kpi",
    Forecast => "forecast",
    Notes => "notes",
    Marks => "marks",
    Findings => "findings",
    Search => "search",
    Related => "related",
    GoalList => "goal_list",
    GoalShow => "goal_show",
    Lint => "lint",
    ObserveHistory => "observe_history",
    // A section of an observation's whole input, as `dagq observe --input`
    // prints it: what the observer's prompt left out (task 1567).
    ObserveInput => "observe_input",
    // The eval of an agent (ADR-t1728-1 decisions 5 and 6): a request of a
    // round, which the supervisor runs, as `dagq agent eval` records it,
    // and the rounds and one round read back, as `dagq agent results` and
    // `dagq agent result` print them; a worker's on its own run only.
    AgentEval => "agent_eval",
    AgentResults => "agent_results",
    AgentResult => "agent_result",
});

impl UseCase {
    /// The use case a `dagq` command line goes to through the service:
    /// `words` are its arguments after the global options, the subcommand
    /// first. `None` for a command the service does not answer (the
    /// control side's, `watch`, `report`, `graph --out`, `ask close`,
    /// `observe` without `--history` or `--input`, `locate`, `doctor`, the planning
    /// commands, ...).
    pub fn of_command(words: &[&str]) -> Option<Self> {
        let has = |flag: &str| {
            words
                .iter()
                .any(|word| *word == flag || word.starts_with(&format!("{flag}=")))
        };
        let command = *words.first()?;
        let next = words.get(1).copied();
        Some(match (command, next) {
            ("ask", Some("close")) => return None,
            ("ask", _) => Self::Ask,
            ("show", _) => Self::Show,
            ("note", _) => Self::Note,
            ("proposal", Some("list")) => Self::ProposalList,
            ("proposal", Some("show")) => Self::ProposalShow,
            ("finding", Some("record")) => Self::FindingRecord,
            ("finding", Some("resolve")) => Self::FindingResolve,
            ("finding", Some("dismiss")) => Self::FindingDismiss,
            ("list", _) => Self::List,
            ("candidates", _) => Self::Candidates,
            ("graph", _) if has("--out") => return None,
            ("graph", _) => Self::Graph,
            ("status", _) => Self::Status,
            ("asks", _) => Self::Asks,
            ("events", _) => Self::Events,
            ("timeline", _) => Self::Timeline,
            ("stats", _) => Self::Stats,
            ("kpi", _) => Self::Kpi,
            ("forecast", _) => Self::Forecast,
            ("notes", _) => Self::Notes,
            ("marks", _) => Self::Marks,
            ("findings", _) => Self::Findings,
            ("search", _) => Self::Search,
            ("related", _) => Self::Related,
            ("goal", Some("list")) => Self::GoalList,
            ("goal", Some("show")) => Self::GoalShow,
            ("lint", _) => Self::Lint,
            ("observe", _) if has("--history") => Self::ObserveHistory,
            ("observe", _) if has("--input") => Self::ObserveInput,
            ("agent", Some("eval")) => Self::AgentEval,
            ("agent", Some("results")) => Self::AgentResults,
            ("agent", Some("result")) => Self::AgentResult,
            _ => return None,
        })
    }
}

// Why the service refused or failed a request.
string_enum!(ServiceErrorCode {
    // The caller's API version is not one the service answers.
    ApiVersionMismatch => "api_version_mismatch",
    // No token, or one the service did not issue, or one revoked or
    // ended with its run.
    Unauthenticated => "unauthenticated",
    // The policy refused the principal (recorded as
    // `authorization_denied`).
    AuthorizationDenied => "authorization_denied",
    // The request could not be read.
    BadRequest => "bad_request",
    // The use case failed (a missing task, a bad option, ...).
    Failed => "failed",
});

// The state of a queue's service as a look at it finds it: `running` (it
// answered `hello`), `stopped` (nothing recorded, or its process is gone)
// or `unreachable` (its process lives and does not answer).
string_enum!(ServiceState {
    Running => "running",
    Stopped => "stopped",
    Unreachable => "unreachable",
});

/// One request to the service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceRequest {
    pub api_version: u32,
    /// The token the control side issued for the caller; `None` for
    /// [`UseCase::Hello`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub use_case: UseCase,
    #[serde(default)]
    pub params: Value,
}

/// Why a request was not done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceError {
    pub code: ServiceErrorCode,
    pub message: String,
}

/// The answer to one request: `result` when `ok`, else `error`. It
/// always carries the service's API version, so a caller of another one
/// sees the mismatch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceResponse {
    pub api_version: u32,
    pub min_api_version: u32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ServiceError>,
}

impl ServiceResponse {
    pub fn success(result: Value) -> Self {
        Self {
            api_version: API_VERSION,
            min_api_version: MIN_API_VERSION,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(code: ServiceErrorCode, message: impl Into<String>) -> Self {
        Self {
            api_version: API_VERSION,
            min_api_version: MIN_API_VERSION,
            ok: false,
            result: None,
            error: Some(ServiceError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// Whether the service answers a caller of `version`.
pub const fn answers(version: u32) -> bool {
    version >= MIN_API_VERSION && version <= API_VERSION
}

/// Whether a caller of [`API_VERSION`] can use a service that answered
/// with `response`'s versions: the service's range has to hold the
/// caller's version.
pub const fn understands(response: &ServiceResponse) -> bool {
    response.min_api_version <= API_VERSION && API_VERSION <= response.api_version
}

/// Who a token was issued for (ADR-t1233-4 decision 4): the role, the
/// actor id the records keep, and the run and task of a worker (or of a
/// job on a run). The service acts as this principal and never as the
/// role a caller names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub role: ActorRole,
    pub actor_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
}

impl Principal {
    /// The worker (or its resume) of `run` on `task`.
    pub fn worker(run: &RunId, task: TaskId) -> Self {
        Self::of(&ActorContext::worker(run, task))
    }

    /// The principal of `actor`.
    pub fn of(actor: &ActorContext) -> Self {
        Self {
            role: actor.role(),
            actor_id: actor.actor_id().to_owned(),
            run_id: actor.run_id().cloned(),
            task_id: actor.task_id(),
        }
    }

    /// The principals the control side issues tokens for: the AI actors
    /// (ADR-t1233-4 decision 4). A person and the control side open the
    /// DB themselves until stage (5) (decision 6).
    pub fn check(&self) -> Result<(), DomainError> {
        if self.role.trust() == super::TrustLevel::UntrustedAgent {
            Ok(())
        } else {
            Err(DomainError::UnknownValue {
                kind: "Principal role",
                value: self.role.as_str().to_owned(),
            })
        }
    }

    /// The actor the service acts as.
    pub fn actor(&self) -> ActorContext {
        let actor = ActorContext::new(self.role, self.actor_id.clone());
        match (&self.run_id, self.task_id) {
            (Some(run), Some(task)) => actor.with_run(run.clone(), task),
            _ => actor,
        }
    }
}

/// Whether the `dagq` of an actor of `role` runs in client mode (goal 82's
/// stage (3)): the worker (and its resume) and the headless jobs with the
/// observer get the service's socket and a token instead of the queue's
/// path. The inbox and the planners open the queue themselves until stage
/// (5) (ADR-t1233-4 decision 6), as the control side does.
pub const fn client_role(role: ActorRole) -> bool {
    matches!(role, ActorRole::Worker | ActorRole::Observer) || role.is_headless_job()
}

/// Whether a token issued on a run still holds while the run is in
/// `status`: one ended (landed, succeeded, failed, interrupted) ends its
/// tokens (ADR-t1233-4 decision 4).
pub const fn run_holds_token(status: RunStatus) -> bool {
    !matches!(
        status,
        RunStatus::Integrated | RunStatus::Succeeded | RunStatus::Failed | RunStatus::Interrupted
    )
}

/// The supervisor could not keep the service running: the inbox's
/// attention (`reason`, `message`), next `dagq service status`, until the
/// service runs again (ADR-t1233-4 decision 3).
pub const QUEUE_SERVICE_DOWN: &str =
    crate::domain::event_kind::EventKind::QueueServiceDown.as_str();
/// The service answers again after a [`QUEUE_SERVICE_DOWN`].
pub const QUEUE_SERVICE_RUNNING: &str =
    crate::domain::event_kind::EventKind::QueueServiceRunning.as_str();
/// `up` or the supervisor started the service (`pid`, `build`, `by`,
/// `replaced`).
pub const QUEUE_SERVICE_STARTED: &str =
    crate::domain::event_kind::EventKind::QueueServiceStarted.as_str();
/// `down`, or a supervisor at the end of the drain `down` asked for,
/// stopped the service (`pid`, `by`).
pub const QUEUE_SERVICE_STOPPED: &str =
    crate::domain::event_kind::EventKind::QueueServiceStopped.as_str();
/// `down` asked the supervisors it signalled (`supervisors`) to stop the
/// service once their drain ends.
pub const QUEUE_SERVICE_STOP_REQUESTED: &str =
    crate::domain::event_kind::EventKind::QueueServiceStopRequested.as_str();
/// A request the service could not authenticate (`use_case`, `reason`).
pub const QUEUE_SERVICE_UNAUTHENTICATED: &str =
    crate::domain::event_kind::EventKind::QueueServiceUnauthenticated.as_str();
/// The kinds whose latest says whether the attention stands.
pub const QUEUE_SERVICE_ATTENTION_KINDS: [&str; 4] = [
    QUEUE_SERVICE_DOWN,
    QUEUE_SERVICE_RUNNING,
    QUEUE_SERVICE_STARTED,
    QUEUE_SERVICE_STOPPED,
];

/// Whether the attention stands: the latest of
/// [`QUEUE_SERVICE_ATTENTION_KINDS`] is [`QUEUE_SERVICE_DOWN`].
pub fn attention_stands(latest: Option<&str>) -> bool {
    latest == Some(QUEUE_SERVICE_DOWN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_request_and_a_response_read_back_as_written() {
        let request: ServiceRequest = serde_json::from_value(json!({
            "api_version": 1, "token": "t", "use_case": "note",
            "params": {"task": 3, "text": "x"},
        }))
        .unwrap();
        assert_eq!(request.use_case, UseCase::Note);
        assert_eq!(request.token.as_deref(), Some("t"));
        // A hello names no token and no params.
        let hello: ServiceRequest =
            serde_json::from_value(json!({"api_version": 1, "use_case": "hello"})).unwrap();
        assert_eq!(hello.token, None);
        assert_eq!(hello.params, Value::Null);
        assert!(
            serde_json::from_value::<ServiceRequest>(
                json!({"api_version": 1, "use_case": "drop_table"})
            )
            .is_err()
        );
        let failure = ServiceResponse::failure(ServiceErrorCode::Unauthenticated, "no token");
        assert_eq!(
            serde_json::to_value(&failure).unwrap(),
            json!({"api_version": API_VERSION, "min_api_version": MIN_API_VERSION, "ok": false,
                   "error": {"code": "unauthenticated", "message": "no token"}})
        );
        let success = ServiceResponse::success(json!({"a": 1}));
        assert_eq!(
            serde_json::from_value::<ServiceResponse>(serde_json::to_value(&success).unwrap())
                .unwrap(),
            success
        );
    }

    #[test]
    fn the_versions_decide_who_understands_whom() {
        assert!(answers(API_VERSION));
        assert!(answers(MIN_API_VERSION));
        assert!(!answers(API_VERSION + 1));
        assert!(!answers(0));
        let newer = ServiceResponse {
            api_version: API_VERSION + 2,
            min_api_version: API_VERSION + 1,
            ..ServiceResponse::success(json!({}))
        };
        assert!(!understands(&newer));
        let wider = ServiceResponse {
            api_version: API_VERSION + 1,
            min_api_version: MIN_API_VERSION,
            ..ServiceResponse::success(json!({}))
        };
        assert!(understands(&wider));
        let older = ServiceResponse {
            api_version: API_VERSION - 1,
            min_api_version: 0,
            ..ServiceResponse::success(json!({}))
        };
        assert!(!understands(&older));
        assert!(understands(&ServiceResponse::success(json!({}))));
    }

    #[test]
    fn a_principal_is_an_ai_actor_and_acts_as_itself() {
        let run = RunId::new("r1").unwrap();
        let worker = Principal::worker(&run, TaskId::new(4));
        assert!(worker.check().is_ok());
        let actor = worker.actor();
        assert_eq!(actor.role(), ActorRole::Worker);
        assert_eq!(actor.run_id(), Some(&run));
        assert_eq!(actor.task_id(), Some(TaskId::new(4)));
        assert_eq!(actor, ActorContext::worker(&run, TaskId::new(4)));
        let job = Principal::of(&ActorContext::review_job(&run, 2));
        assert_eq!(job.actor().role(), ActorRole::ReviewJob);
        assert_eq!(job.actor().run_id(), None);
        for role in [
            ActorRole::User,
            ActorRole::Supervisor,
            ActorRole::Wrapper,
            ActorRole::Integrator,
        ] {
            assert!(
                Principal::of(&ActorContext::new(role, "x"))
                    .check()
                    .is_err()
            );
        }
        assert_eq!(
            serde_json::to_value(&worker).unwrap(),
            json!({"role": "worker", "actor_id": "worker:r1", "run_id": "r1", "task_id": 4})
        );
    }

    #[test]
    fn an_ended_run_ends_its_tokens() {
        for status in [
            RunStatus::Claimed,
            RunStatus::Running,
            RunStatus::NeedsSession,
            RunStatus::AwaitingIntegration,
        ] {
            assert!(run_holds_token(status), "{status:?}");
        }
        for status in [
            RunStatus::Integrated,
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Interrupted,
        ] {
            assert!(!run_holds_token(status), "{status:?}");
        }
        assert!(attention_stands(Some(QUEUE_SERVICE_DOWN)));
        assert!(!attention_stands(Some(QUEUE_SERVICE_RUNNING)));
        assert!(!attention_stands(None));
    }

    #[test]
    fn a_command_line_goes_to_its_use_case_and_the_rest_to_none() {
        for (words, use_case) in [
            (&["events", "--full"][..], "events"),
            (&["goal", "show", "3"], "goal_show"),
            (&["goal", "list"], "goal_list"),
            (&["proposal", "show", "1"], "proposal_show"),
            (&["finding", "record", "--kind", "x"], "finding_record"),
            (&["observe", "--history"], "observe_history"),
            (&["observe", "--limit", "5", "--history"], "observe_history"),
            (
                &[
                    "observe",
                    "--input",
                    "1791005872",
                    "--section",
                    "stats.runs",
                ],
                "observe_input",
            ),
            (&["ask", "--kind", "blocked"], "ask"),
            (&["graph", "--format", "d2"], "graph"),
            (&["stats"], "stats"),
        ] {
            assert_eq!(
                UseCase::of_command(words).map(UseCase::as_str),
                Some(use_case),
                "{words:?}"
            );
        }
        for words in [
            &[][..],
            &["watch"],
            &["report"],
            &["locate"],
            &["doctor"],
            &["observe"],
            &["observe", "--daily"],
            &["graph", "--format", "svg", "--out", "g.svg"],
            &["graph", "--out=g.d2"],
            &["ask", "close", "3"],
            &["goal", "add", "x"],
            &["goal"],
            &["proposal", "withdraw", "1"],
            &["finding"],
            &["mark", "x"],
            &["add"],
            &["integrate"],
        ] {
            assert_eq!(UseCase::of_command(words), None, "{words:?}");
        }
    }
}
