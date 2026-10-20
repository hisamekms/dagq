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
//! The use cases of goal 82's stage (2): `hello`, `ask` (with the
//! observer's `blocked` ask on a finding), `show` (a task with its runs,
//! the whole queue being readable to every role, ADR-t1233-5 decision 3),
//! `note`, `proposal_list` and `proposal_show`, and the findings'
//! `finding_record` (which updates the open finding of the same kind,
//! target and subject), `finding_resolve` and `finding_dismiss`
//! (ADR-t1222-1 decisions 1, 2 and 4), and the reads of the whole queue a
//! read role's job or a worker runs (`events`, `timeline`, `stats`, `kpi`,
//! `search`, `goal_show`, ...: [`QueueRead`], answered as the command line
//! answers them). The supervisor and the command line still open the DB
//! themselves.
//!
//! [Queue service]: ../../docs/design/queue-service.md

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use super::commands::dialogue::{Dialogue, DialogueStore, MarkChange};
use super::commands::{DenialLog, Gate};
use super::queue_reads::{BadRead, QueueRead};
use crate::domain::EventKind;
use crate::domain::queue_service::{
    API_VERSION, MIN_API_VERSION, Principal, ServiceErrorCode, ServiceRequest, ServiceResponse,
    ServiceState, UseCase, answers, run_holds_token,
};
use crate::domain::session_step::{StepError, StepTarget};
use crate::domain::telemetry::{ReportRecorded, TelemetryReport};
use crate::domain::{
    ActorContext, ActorRole, Answerer, Ask, AskId, AskKind, AskReason, AuthorizationError,
    Capability, EventId, Finding, FindingId, FindingOutcome, FindingStatus, FindingTarget, GoalId,
    NewAsk, NewFinding, NewNote, NoteTarget, PlannerId, ProposalId, RequestId, Resource, RunEvent,
    RunId, RunStatus, StaticPolicy, TaskId,
};

/// The queue as one use case of the service reads and changes it, written
/// as the principal's actor.
pub trait ServiceQueue:
    DialogueStore
    + super::agent_eval::EvalStore
    + super::commands::replans::ReplanStore
    + super::agent_cases::CaseSetStore
{
    /// Task `id` as `dagq show` prints it: the whole detail with `full`,
    /// else its view with the last `events` events.
    fn show(&mut self, id: TaskId, full: bool, events: usize) -> Result<Value>;
    /// The status of run `id`; `None` for a run the queue does not know.
    fn run_status(&self, id: &RunId) -> Result<Option<RunStatus>>;
    /// The proposals as `dagq proposal list` prints them: the submitted
    /// and revising ones, or with `all` every one.
    fn proposals(&self, all: bool) -> Result<Value>;
    /// Proposal `id` as `dagq proposal show` prints it.
    fn show_proposal(&self, id: ProposalId) -> Result<Value>;
    /// A read of the whole queue as its command prints it.
    fn read(&mut self, read: &QueueRead) -> Result<Value>;
    /// The target the control side registered `session_id` to
    /// ([`crate::application::SessionOwners`]); `None` for one never
    /// registered.
    fn session_owner(&self, session_id: &str) -> Result<Option<StepTarget>>;
    /// Record `report`'s steps and resource samples as received now
    /// ([`crate::application::SessionStepStore::record_steps`], then
    /// [`crate::application::SessionResourceStore::record_resources`]): a
    /// failure between leaves the steps, which the executor's resend adds
    /// no second time.
    fn record_report(&mut self, report: &TelemetryReport) -> Result<ReportRecorded>;
}

/// What the service runs its use cases on.
pub trait ServiceBackend: Send + Sync {
    /// The principal `token` was issued for; `None` for a token the
    /// control side did not issue or has revoked.
    fn principal(&self, token: &str) -> Result<Option<Principal>>;
    /// The queue, written as `actor`.
    fn open(&self, actor: &ActorContext) -> Result<Box<dyn ServiceQueue>>;
    /// Record a request that named no principal
    /// (`queue_service_unauthenticated`), written as the service itself.
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
                Some(refused) => ServiceResponse::denied(refused),
                None if error.downcast_ref::<BadParams>().is_some()
                    || error.downcast_ref::<BadRead>().is_some() =>
                {
                    ServiceResponse::failure(ServiceErrorCode::BadRequest, format!("{error:#}"))
                }
                None => ServiceResponse::failure(ServiceErrorCode::Failed, format!("{error:#}")),
            },
        }
    }

    /// The principal of the request's token, whose run, when it names
    /// one, has not ended. A refusal is `unauthenticated`, recorded with
    /// the use case and the reason, never the token: `missing_token` for
    /// no token or an empty one, `unknown_token` for one the control side
    /// did not issue or has revoked, `run_ended` for one whose run has
    /// ended or is not in the queue.
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
    recommend: Option<String>,
    #[serde(default)]
    confidence: Option<String>,
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

/// `proposal_list`'s params: those of `dagq proposal list`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalListParams {
    #[serde(default)]
    all: bool,
}

/// `proposal_show`'s params: those of `dagq proposal show`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalShowParams {
    id: i64,
}

/// `finding_record`'s params: those of `dagq finding record`, one target
/// (`task`, `run`, `goal`, or `queue: true`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FindingRecordParams {
    kind: String,
    #[serde(default)]
    task: Option<i64>,
    #[serde(default)]
    run: Option<String>,
    #[serde(default)]
    goal: Option<i64>,
    #[serde(default)]
    queue: bool,
    #[serde(default)]
    subject: String,
    summary: String,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    impact: Option<String>,
    #[serde(default)]
    evidence: Vec<i64>,
    #[serde(default)]
    propose: Option<String>,
}

/// `finding_resolve`'s and `finding_dismiss`'s params: those of `dagq
/// finding resolve` and `dagq finding dismiss`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FindingStatusParams {
    id: i64,
    reason: String,
    /// `finding dismiss --covered-by` (ADR-t1920-1); absent from an older
    /// client, which never covers.
    #[serde(default)]
    covered_by: Option<i64>,
}

/// `replan_propose`'s params: those of `dagq replan propose`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplanProposeParams {
    run: String,
    reason: String,
    #[serde(default)]
    evidence: Vec<i64>,
}

/// `agent_eval`'s params: those of `dagq agent eval`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentEvalParams {
    agent: String,
    #[serde(default)]
    split: Option<String>,
    #[serde(default)]
    k: Option<u32>,
    #[serde(default)]
    cases: Option<Vec<String>>,
    #[serde(default)]
    set: Option<i64>,
    #[serde(default)]
    rerun: bool,
}

/// `agent_cases`' params: those of `dagq agent cases`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentCasesParams {
    agent: String,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    until: Option<String>,
}

/// `agent_decide`'s params: those of `dagq agent decide`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentDecideParams {
    set: i64,
    case: String,
    verdict: String,
    #[serde(default)]
    codes: Vec<String>,
    reason: String,
}

/// `agent_results`' params: those of `dagq agent results`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentResultsParams {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `agent_result`'s params: those of `dagq agent result`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentResultParams {
    id: i64,
}

/// Authorize reading the whole queue: every role reads it in goal 82
/// (ADR-t1233-5 decision 3), and the policy is still asked, on the
/// service's side, as it would be for a narrower one.
fn authorize_read(store: &Store<'_>, actor: &ActorContext, resource: &Resource) -> Result<()> {
    Gate {
        actor,
        authorizer: &StaticPolicy,
    }
    .authorize(store, Capability::QueueRead, resource)?;
    Ok(())
}

/// Run `use_case` as `actor` on `queue`.
fn run(
    queue: &mut dyn ServiceQueue,
    actor: &ActorContext,
    use_case: UseCase,
    raw: &Value,
) -> Result<Value> {
    let mut store = Store(queue);
    // The executor's telemetry principal reaches `telemetry_report` alone
    // and a role without `telemetry.report` never reaches it (ADR-t1662-3
    // decision 9): each is refused, and recorded, before its params are
    // read, as the least the other use cases need, a read of the queue, and
    // as `telemetry.report` on no target it may name.
    let gate = Gate {
        actor,
        authorizer: &StaticPolicy,
    };
    if actor.role() == ActorRole::ExecutorTelemetry && use_case != UseCase::TelemetryReport {
        gate.authorize(&store, Capability::QueueRead, &Resource::Queue)?;
    }
    if use_case == UseCase::TelemetryReport
        && !crate::domain::authorization::grants(actor.role())
            .contains(&Capability::TelemetryReport)
    {
        gate.authorize(&store, Capability::TelemetryReport, &Resource::Unresolved)?;
    }
    // The reads of the whole queue (ADR-t1233-5 decisions 1 to 3), each
    // authorized as the command line asks for its command.
    if let Some(read) = QueueRead::parse(use_case, raw)? {
        authorize_read(&store, actor, &read.resource())?;
        return store.0.read(&read);
    }
    match use_case {
        UseCase::Hello => unreachable!("hello is answered before a principal"),
        UseCase::List
        | UseCase::Candidates
        | UseCase::Graph
        | UseCase::Status
        | UseCase::Asks
        | UseCase::Events
        | UseCase::Timeline
        | UseCase::Stats
        | UseCase::Kpi
        | UseCase::Forecast
        | UseCase::Notes
        | UseCase::Marks
        | UseCase::Findings
        | UseCase::Search
        | UseCase::Related
        | UseCase::GoalList
        | UseCase::GoalShow
        | UseCase::Lint
        | UseCase::ObserveHistory
        | UseCase::ObserveInput => unreachable!("a read is answered above"),
        UseCase::TelemetryReport => {
            // A version the service does not read, or a report past its
            // bounds, is refused before anything is recorded; a field it
            // does not know is dropped (crate::domain::telemetry::read).
            let report = crate::domain::telemetry::read(raw)
                .map_err(|error| BadParams(error.to_string()))?;
            // Of the target the token was issued for, and of a session the
            // control side registered to it.
            let owner = store.0.session_owner(&report.session_id)?;
            gate.authorize(
                &store,
                Capability::TelemetryReport,
                &Resource::Telemetry {
                    target: report.target.clone(),
                    owner,
                },
            )?;
            let recorded = store.0.record_report(&report).map_err(|error| {
                match error.downcast_ref::<StepError>() {
                    Some(refused) => BadParams(refused.to_string()).into(),
                    None => error,
                }
            })?;
            Ok(serde_json::to_value(recorded)?)
        }
        UseCase::AgentEval => {
            let p: AgentEvalParams = params(use_case, raw)?;
            let name = p.split.as_deref().unwrap_or("dev");
            let split = crate::domain::agent_eval::Split::ALL
                .into_iter()
                .find(|split| split.as_str() == name)
                .ok_or_else(|| BadParams(format!("agent_eval's split: unknown {name}")))?;
            super::agent_eval::AgentEvals::new(&*store.0, actor, &StaticPolicy).request(
                &super::agent_eval::EvalRequest {
                    agent: p.agent,
                    split,
                    k: p.k,
                    cases: p.cases,
                    set: p.set,
                    rerun: p.rerun,
                },
            )
        }
        UseCase::AgentCases => {
            let p: AgentCasesParams = params(use_case, raw)?;
            super::agent_cases::AgentCaseSets::new(&*store.0, actor, &StaticPolicy)
                .request(&p.agent, p.since, p.until)
        }
        UseCase::AgentDecide => {
            let p: AgentDecideParams = params(use_case, raw)?;
            super::agent_cases::AgentCaseSets::new(&*store.0, actor, &StaticPolicy)
                .decide(p.set, &p.case, &p.verdict, p.codes, &p.reason)
        }
        UseCase::AgentResults => {
            let p: AgentResultsParams = params(use_case, raw)?;
            super::agent_eval::AgentEvals::new(&*store.0, actor, &StaticPolicy)
                .list(p.agent.as_deref(), p.limit.unwrap_or(20))
        }
        UseCase::AgentResult => {
            let p: AgentResultParams = params(use_case, raw)?;
            super::agent_eval::AgentEvals::new(&*store.0, actor, &StaticPolicy).show(p.id)
        }
        UseCase::Ask => {
            let p: AskParams = params(use_case, raw)?;
            let ask = NewAsk {
                recommendation: p.recommend,
                confidence: p.confidence.as_deref().map(str::parse).transpose()?,
                kind: p.kind.unwrap_or_default().parse::<AskKind>()?,
                task_id: p.task_id.map(TaskId::new),
                run_id: p.run_id.map(RunId::new).transpose()?,
                question: p.question.unwrap_or_default(),
                options: p.options,
                asked_by: actor.written_by().to_owned(),
                reason_category: p.because.unwrap_or_default().parse::<AskReason>()?,
                topics: p.topics,
                finding_id: p.finding_id.map(FindingId::new),
                request_id: None,
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
            authorize_read(&store, actor, &Resource::task(id))?;
            store.0.show(id, p.full, p.events)
        }
        UseCase::ProposalList => {
            let p: ProposalListParams = params(use_case, raw)?;
            authorize_read(&store, actor, &Resource::Queue)?;
            store.0.proposals(p.all)
        }
        UseCase::ProposalShow => {
            let p: ProposalShowParams = params(use_case, raw)?;
            // The command line reads a proposal as the queue
            // (`queue.read` on the queue), and so does the service.
            authorize_read(&store, actor, &Resource::Queue)?;
            store.0.show_proposal(ProposalId::new(p.id))
        }
        UseCase::FindingRecord => {
            let p: FindingRecordParams = params(use_case, raw)?;
            let target = match (p.task, p.run, p.goal, p.queue) {
                (Some(task), None, None, false) => FindingTarget::Task(TaskId::new(task)),
                (None, Some(run), None, false) => FindingTarget::Run(RunId::new(run)?),
                (None, None, Some(goal), false) => FindingTarget::Goal(GoalId::new(goal)),
                (None, None, None, true) => FindingTarget::Queue,
                _ => {
                    return Err(BadParams(
                        "finding_record names one target: task, run, goal or queue".to_owned(),
                    )
                    .into());
                }
            };
            let outcome =
                Dialogue::new(&mut store, actor, &StaticPolicy).record_finding(NewFinding {
                    kind: p.kind,
                    target,
                    subject: p.subject,
                    summary: p.summary,
                    detail: p.detail,
                    // `dagq finding record` takes high, normal or low
                    // only, before anything runs.
                    impact: p
                        .impact
                        .map(|impact| impact.parse())
                        .transpose()
                        .map_err(|error| BadParams(format!("finding_record's impact: {error}")))?,
                    evidence: p.evidence.into_iter().map(EventId::new).collect(),
                    propose: p.propose,
                    by: actor.written_by().to_owned(),
                })?;
            Ok(serde_json::to_value(outcome)?)
        }
        UseCase::ReplanPropose => {
            let p: ReplanProposeParams = params(use_case, raw)?;
            let recorded = super::commands::replans::Replans::new(&mut store, actor, &StaticPolicy)
                .propose(&crate::domain::task_replan::NewReplan {
                    run: RunId::new(p.run)?,
                    reason: p.reason,
                    evidence: p.evidence.into_iter().map(EventId::new).collect(),
                })?;
            Ok(serde_json::to_value(recorded)?)
        }
        UseCase::FindingResolve | UseCase::FindingDismiss => {
            let p: FindingStatusParams = params(use_case, raw)?;
            let id = FindingId::new(p.id);
            let mut dialogue = Dialogue::new(&mut store, actor, &StaticPolicy);
            let finding = if use_case == UseCase::FindingResolve {
                dialogue.resolve_finding(id, &p.reason)?
            } else {
                dialogue.dismiss_finding(id, &p.reason, p.covered_by.map(TaskId::new))?
            };
            Ok(serde_json::to_value(finding)?)
        }
    }
}

/// A [`ServiceQueue`] as the [`Dialogue`] commands' store.
struct Store<'a>(&'a mut dyn ServiceQueue);

impl super::commands::replans::ReplanStore for Store<'_> {
    fn run_task(&self, run: &RunId) -> Result<Option<TaskId>> {
        self.0.run_task(run)
    }
    fn record_replan(
        &mut self,
        new: &crate::domain::task_replan::NewReplan,
        origin: crate::domain::task_replan::ReplanOrigin,
        by_role: &str,
        by_id: &str,
    ) -> Result<crate::domain::task_replan::ReplanRecorded> {
        self.0.record_replan(new, origin, by_role, by_id)
    }
    fn replan_operation(
        &self,
        id: crate::domain::ReplanId,
    ) -> Result<crate::domain::task_replan::ReplanOperation> {
        self.0.replan_operation(id)
    }
    fn move_replan(
        &mut self,
        id: crate::domain::ReplanId,
        generation: i64,
        step: &crate::domain::task_replan::ReplanMove,
    ) -> Result<crate::domain::task_replan::ReplanOperation> {
        self.0.move_replan(id, generation, step)
    }
}

impl DenialLog for Store<'_> {
    fn record_denial(&self, payload: Value) -> Result<()> {
        self.0.record_denial(payload)
    }
}

impl DialogueStore for Store<'_> {
    fn read_ask(&self, id: AskId) -> Result<Ask> {
        self.0.read_ask(id)
    }
    fn request_planner(&self, request: RequestId) -> Result<Option<PlannerId>> {
        self.0.request_planner(request)
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
        covered_by: Option<TaskId>,
    ) -> Result<Finding> {
        self.0.set_finding_status(id, to, reason, by, covered_by)
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

/// What the control side gives an actor whose `dagq` runs in client mode
/// (goal 82's stage (3), ADR-t1233-4 decision 4): the socket of the queue
/// at `db`, and the file of a token for the actor's principal. Only the
/// control side holds it: the supervisor issues a worker's token at the
/// claim and the resume and a job's at its start, and no command of an AI
/// actor's issues one. The supervisor revokes a worker's token when its run
/// ends, and sweeps those whose runs ended while it did not look.
pub trait ServiceAccess: Send + Sync {
    /// The socket of the service of the queue at `db`.
    fn socket(&self, db: &std::path::Path) -> std::path::PathBuf;
    /// Issue a token for `principal` (the one its actor held is revoked);
    /// the file holding it.
    fn issue(&self, db: &std::path::Path, principal: &Principal) -> Result<std::path::PathBuf>;
    /// The file of the token `actor_id` holds, `None` when it holds none.
    fn credential(&self, db: &std::path::Path, actor_id: &str) -> Option<std::path::PathBuf>;
    /// Revoke the token `actor_id` holds (a worker's whose run ended):
    /// whether there was one.
    fn revoke(&self, db: &std::path::Path, actor_id: &str) -> Result<bool>;
    /// Revoke every token of the queue at `db` whose principal `ended`
    /// says has ended: the actor ids of those revoked.
    fn sweep(
        &self,
        db: &std::path::Path,
        ended: &mut dyn FnMut(&Principal) -> bool,
    ) -> Result<Vec<String>>;
    /// `child`, which revokes the token of `actor_id` once it has ended (a
    /// job's, whose token ends with it).
    fn revoke_on_exit(
        &self,
        db: &std::path::Path,
        actor_id: &str,
        child: Box<dyn super::Spawned>,
    ) -> Box<dyn super::Spawned>;
}

/// How the control side issues and revokes the token of an executor's
/// telemetry principal (ADR-t1662-3 decision 11): only the control side
/// calls it, and no command of an AI actor's does. The token's file is
/// the executor's to read, found by its target
/// ([`Self::telemetry_credential`]); it is handed to no agent, neither in
/// its environment nor on its command line.
pub trait TelemetryTokens: Send + Sync {
    /// Issue the token of the telemetry principal of `target` of the queue
    /// at `db` (of `task`, for a run target), under `issued_under` (the
    /// lease of the supervisor that issues it), revoking every one it
    /// held: the file holding it.
    fn issue_telemetry(
        &self,
        db: &std::path::Path,
        target: &StepTarget,
        task: Option<TaskId>,
        issued_under: Option<&crate::domain::LeaseToken>,
    ) -> Result<std::path::PathBuf>;
    /// Revoke the tokens of the telemetry principal of `target`, only those
    /// issued under `issued_under` when it is given: whether there was one.
    /// Once it returns `Ok`, none of them authenticates; an error may leave
    /// one that does, which [`Self::held_telemetry`] still lists.
    fn revoke_telemetry(
        &self,
        db: &std::path::Path,
        target: &StepTarget,
        issued_under: Option<&crate::domain::LeaseToken>,
    ) -> Result<bool>;
    /// Where the executor of `target` reads its token: a file outside every
    /// worktree and run directory, `None` while none holds.
    fn telemetry_credential(
        &self,
        db: &std::path::Path,
        target: &StepTarget,
    ) -> Option<std::path::PathBuf>;
    /// The telemetry tokens held, each with its target and the lease it was
    /// issued under: the sweep's list, which keeps a token whose revocation
    /// failed.
    fn held_telemetry(
        &self,
        db: &std::path::Path,
    ) -> Result<Vec<crate::domain::telemetry::HeldTelemetry>>;
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

/// The queue service as `status`, `doctor` and `dagq service status` show
/// it (ADR-t1233-4): `probe`, what a look at its record and its socket
/// found, the API version this binary speaks, and the attention its latest
/// event leaves standing (`queue_service_down`) when the queue's `log` can
/// be read (`null` without one, `error` when the read fails).
pub fn view(probe: &ServiceProbe, log: Option<&dyn super::RunLog>) -> Value {
    use crate::domain::queue_service::QUEUE_SERVICE_ATTENTION_KINDS;
    let mut view = serde_json::to_value(probe).unwrap_or(Value::Null);
    view["client_api_version"] = json!(API_VERSION);
    view["attention"] = match log.map(|log| log.latest_queue_event(&QUEUE_SERVICE_ATTENTION_KINDS))
    {
        None => Value::Null,
        Some(Ok(latest)) => json!(latest.is_some_and(|event| {
            crate::domain::queue_service::attention_stands(Some(&event.kind))
        })),
        Some(Err(error)) => json!({"error": format!("{error:#}")}),
    };
    view
}

/// `dagq service start`: [`ensure`] the service with `control`, recorded
/// in `log` as `queue_service_started` (`by: service start`) unless one of
/// this build already answered.
pub fn start_by_hand(
    log: &dyn super::RunLog,
    control: &dyn QueueServiceControl,
    timeout: Duration,
) -> Result<Value> {
    let report = ensure(control, timeout)?;
    if report["outcome"] != "reused" {
        log.record_queue_event(
            EventKind::QueueServiceStarted,
            json!({
                "by": "service start",
                "pid": report["service"]["pid"],
                "build": report["service"]["build"],
                "api_version": report["service"]["api_version"],
                "socket": report["service"]["socket"],
                "restart": false,
                "replaced": report["replaced"],
            }),
        )?;
    }
    Ok(report)
}

/// `dagq service stop`: stop the queue's service with `control`, recorded
/// in `log` as `queue_service_stopped` (`by: service stop`) when one ran. A
/// supervisor at work starts it again at its next look.
pub fn stop_by_hand(
    log: &dyn super::RunLog,
    control: &dyn QueueServiceControl,
    timeout: Duration,
) -> Result<Value> {
    Ok(match control.stop(timeout)? {
        Some(pid) => {
            log.record_queue_event(
                EventKind::QueueServiceStopped,
                json!({"pid": pid, "by": "service stop"}),
            )?;
            json!({"outcome": "stopped", "pid": pid})
        }
        None => json!({"outcome": "not_running"}),
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Mutex;

    use anyhow::anyhow;

    use super::*;
    use crate::domain::Answerer;

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
        fn request_planner(&self, _: RequestId) -> Result<Option<PlannerId>> {
            Err(anyhow!("no request"))
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
        fn record_finding(&mut self, finding: NewFinding) -> Result<FindingOutcome> {
            self.log.lock().unwrap().push(json!({
                "finding": finding.kind, "target": format!("{:?}", finding.target),
                "by": finding.by,
            }));
            Err(anyhow!("recorded"))
        }
        fn set_finding_status(
            &mut self,
            id: FindingId,
            to: FindingStatus,
            reason: &str,
            by: &str,
            _covered_by: Option<TaskId>,
        ) -> Result<Finding> {
            self.log
                .lock()
                .unwrap()
                .push(json!({"finding": id, "to": to.as_str(), "reason": reason, "by": by}));
            Err(anyhow!("set"))
        }
    }

    /// The eval's events are kept in the log, each as its kind and
    /// payload; none is read back.
    impl crate::application::agent_eval::EvalStore for Queue {
        fn record_eval_event(&self, kind: EventKind, payload: Value) -> Result<EventId> {
            let mut log = self.log.lock().unwrap();
            log.push(json!({"eval_event": kind.as_str(), "payload": payload}));
            Ok(EventId::new(i64::try_from(log.len()).unwrap()))
        }
        fn eval_events(&self) -> Result<Vec<RunEvent>> {
            Ok(Vec::new())
        }
    }

    /// A replanning request or proposal is kept in the log as its origin,
    /// run and recorder; none is read back.
    impl crate::application::commands::replans::ReplanStore for Queue {
        fn run_task(&self, run: &RunId) -> Result<Option<TaskId>> {
            Ok((run.as_str() == "r1").then_some(TaskId::new(1)))
        }
        fn record_replan(
            &mut self,
            new: &crate::domain::task_replan::NewReplan,
            origin: crate::domain::task_replan::ReplanOrigin,
            by_role: &str,
            _: &str,
        ) -> Result<crate::domain::task_replan::ReplanRecorded> {
            self.log
                .lock()
                .unwrap()
                .push(json!({"replan": origin.as_str(), "run": new.run, "by": by_role}));
            Err(anyhow!("replan recorded"))
        }
        fn replan_operation(
            &self,
            _: crate::domain::ReplanId,
        ) -> Result<crate::domain::task_replan::ReplanOperation> {
            Err(anyhow!("no operation"))
        }
        fn move_replan(
            &mut self,
            _: crate::domain::ReplanId,
            _: i64,
            _: &crate::domain::task_replan::ReplanMove,
        ) -> Result<crate::domain::task_replan::ReplanOperation> {
            Err(anyhow!("no move"))
        }
    }

    impl crate::application::agent_cases::CaseSetStore for Queue {
        fn record_cases_event(&self, kind: EventKind, payload: Value) -> Result<EventId> {
            let mut log = self.log.lock().unwrap();
            log.push(json!({"cases_event": kind.as_str(), "payload": payload}));
            Ok(EventId::new(i64::try_from(log.len()).unwrap()))
        }
        fn cases_events(&self) -> Result<Vec<RunEvent>> {
            Ok(Vec::new())
        }
    }

    impl ServiceQueue for Queue {
        fn show(&mut self, id: TaskId, full: bool, events: usize) -> Result<Value> {
            Ok(json!({"id": id, "full": full, "events": events}))
        }
        fn run_status(&self, id: &RunId) -> Result<Option<RunStatus>> {
            Ok((id.as_str() == "r1").then_some(self.status))
        }
        fn proposals(&self, all: bool) -> Result<Value> {
            Ok(json!({"proposals": [], "all": all}))
        }
        fn show_proposal(&self, id: ProposalId) -> Result<Value> {
            Ok(json!({"proposal": id}))
        }
        fn read(&mut self, read: &QueueRead) -> Result<Value> {
            Ok(json!({"read": format!("{read:?}")}))
        }
        /// A session `<kind>:<id>:<name>` is registered to that target,
        /// any other to none; one named `taken` has steps of another target.
        fn session_owner(&self, session_id: &str) -> Result<Option<StepTarget>> {
            let mut parts = session_id.splitn(3, ':');
            Ok(match (parts.next(), parts.next(), parts.next()) {
                (Some(kind), Some(id), Some(_)) => Some(StepTarget {
                    kind: crate::domain::session_step::TargetKind::parse(kind).unwrap(),
                    id: id.to_owned(),
                }),
                _ => None,
            })
        }
        fn record_report(&mut self, report: &TelemetryReport) -> Result<ReportRecorded> {
            if report.session_id.ends_with(":taken") {
                return Err(StepError::SessionOfAnotherTarget.into());
            }
            self.log.lock().unwrap().push(json!({
                "steps": report.steps.len(), "resources": report.resources.len(),
                "session": report.session_id,
                "target": format!("{}:{}", report.target.kind.as_str(), report.target.id),
            }));
            let added = |added| crate::domain::session_step::StepsRecorded {
                added,
                duplicates: 0,
            };
            Ok(ReportRecorded {
                steps: added(report.steps.len()),
                resources: added(report.resources.len()),
            })
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
                "observer" => Some(Principal::of(&ActorContext::instance(
                    ActorRole::Observer,
                    "s1",
                ))),
                "recovery" => Some(Principal::of(&ActorContext::recovery_job(&run, "a", 1))),
                "plan-review" => Some(Principal::of(&ActorContext::plan_review_job(4, 1))),
                "goal-review" => Some(Principal::of(&ActorContext::goal_review_job(5, 1))),
                "throughput-review" => Some(Principal::of(&ActorContext::throughput_review_job(
                    "hourly", "h",
                ))),
                "planner" => Some(Principal::of(&ActorContext::instance(
                    ActorRole::Planner,
                    3,
                ))),
                "telemetry" => Some(Principal::telemetry(
                    &StepTarget::run(&run),
                    Some(TaskId::new(1)),
                )),
                "telemetry-planner" => Some(Principal::telemetry(
                    &StepTarget {
                        kind: crate::domain::session_step::TargetKind::Planner,
                        id: "3".into(),
                    },
                    None,
                )),
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

    /// A worker proposes to replan its own run through the service, and
    /// nothing on another run; a read role's job proposes nothing
    /// (ADR-t1521-2 decision 3).
    #[test]
    fn a_worker_proposes_a_replanning_of_its_own_run_only() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let propose = |token, run: &str| {
            service.handle(&request(
                Some(token),
                UseCase::ReplanPropose,
                json!({"run": run, "reason": "two independent conditions", "evidence": [4]}),
            ))
        };
        let own = propose("worker", "r1");
        assert_eq!(code(&own), Some(ServiceErrorCode::Failed));
        assert!(
            own.error.unwrap().message.contains("replan recorded"),
            "the store was reached"
        );
        assert_eq!(
            backend.log.lock().unwrap().pop().unwrap(),
            json!({"replan": "worker", "run": "r1", "by": "worker"})
        );
        for (token, run) in [("worker", "r2"), ("review", "r1"), ("observer", "r1")] {
            let refused = propose(token, run);
            assert_eq!(
                code(&refused),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{token} on {run}"
            );
            let denial = backend.log.lock().unwrap().pop().unwrap();
            assert_eq!(denial["capability"], "replan.propose", "{token} on {run}");
        }
        assert_eq!(
            UseCase::of_command(&["replan", "propose", "--run", "r1"]),
            Some(UseCase::ReplanPropose)
        );
        assert_eq!(UseCase::of_command(&["replan", "request"]), None);
    }

    /// The eval through the service (ADR-t1728-1 decision 6): a worker
    /// asks for a dev round on its own run and reads its run's rounds; a
    /// hold-out, a production round and a rerun are refused it and
    /// recorded; a review job asks for none.
    #[test]
    fn a_worker_asks_for_its_runs_dev_eval_only() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let response = service.handle(&request(
            Some("worker"),
            UseCase::AgentEval,
            json!({"agent": "adr-rules", "k": 2}),
        ));
        assert!(response.ok, "{response:?}");
        assert_eq!(response.result.unwrap()["split"], "dev");
        let recorded = backend.log.lock().unwrap().pop().unwrap();
        assert_eq!(recorded["eval_event"], "agent_eval_requested");
        assert_eq!(recorded["payload"]["run_id"], "r1");
        assert_eq!(recorded["payload"]["requested_by"], "worker");
        assert_eq!(recorded["payload"]["k"], 2);
        for params in [
            json!({"agent": "adr-rules", "split": "holdout"}),
            json!({"agent": "adr-rules", "split": "production"}),
            json!({"agent": "adr-rules", "set": 7}),
            json!({"agent": "adr-rules", "rerun": true}),
        ] {
            let response =
                service.handle(&request(Some("worker"), UseCase::AgentEval, params.clone()));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{params}"
            );
            let denial = backend.log.lock().unwrap().pop().unwrap();
            assert_eq!(denial["event"], "authorization_denied", "{params}");
            assert_eq!(denial["resource"]["kind"], "agent_eval");
            // The refusal answers what its event records.
            let denied = response.error.unwrap().denied.unwrap();
            assert_eq!(
                json!([denied.role, denied.capability, denied.reason]),
                json!([denial["role"], denial["capability"], denial["reason"]]),
                "{params}"
            );
        }
        // A production set is never a worker's to ask for or decide
        // (ADR-t1728-1 (g)).
        for (use_case, params) in [
            (UseCase::AgentCases, json!({"agent": "adr-rules"})),
            (
                UseCase::AgentDecide,
                json!({"set": 1, "case": "c", "verdict": "clean", "reason": "r"}),
            ),
        ] {
            let response = service.handle(&request(Some("worker"), use_case, params));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{use_case:?}"
            );
            let denial = backend.log.lock().unwrap().pop().unwrap();
            assert_eq!(denial["event"], "authorization_denied");
        }
        assert_eq!(
            UseCase::of_command(&["agent", "cases", "adr-rules"]),
            Some(UseCase::AgentCases)
        );
        let response = service.handle(&request(
            Some("worker"),
            UseCase::AgentResults,
            json!({"agent": "adr-rules"}),
        ));
        assert_eq!(response.result, Some(json!({"evals": []})));
        let response = service.handle(&request(
            Some("review"),
            UseCase::AgentEval,
            json!({"agent": "adr-rules"}),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::AuthorizationDenied));
        let response = service.handle(&request(
            Some("worker"),
            UseCase::AgentEval,
            json!({"agent": "Not Kebab"}),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::Failed));
        assert_eq!(
            UseCase::of_command(&["agent", "eval", "adr-rules"]),
            Some(UseCase::AgentEval)
        );
        assert_eq!(
            UseCase::of_command(&["agent", "results"]),
            Some(UseCase::AgentResults)
        );
        assert_eq!(
            UseCase::of_command(&["agent", "result", "3"]),
            Some(UseCase::AgentResult)
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
            (UseCase::ProposalShow, json!({})),
            (UseCase::ProposalList, json!({"all": true, "owner": "x"})),
            (
                UseCase::FindingRecord,
                json!({"kind": "stall", "summary": "no target"}),
            ),
            (
                UseCase::FindingRecord,
                json!({"kind": "stall", "summary": "two", "task": 1, "queue": true}),
            ),
            (
                UseCase::FindingRecord,
                json!({"kind": "stall", "summary": "s", "queue": true, "impact": "huge"}),
            ),
            (UseCase::FindingResolve, json!({"id": 1})),
            (UseCase::FindingDismiss, json!({"reason": "x"})),
        ] {
            let response = service.handle(&request(Some("worker"), use_case, params.clone()));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::BadRequest),
                "{params}"
            );
        }
    }

    #[test]
    fn the_observer_writes_findings_as_itself_and_nothing_else() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let pop = || backend.log.lock().unwrap().pop().unwrap();
        // A finding on each target, recorded as the observer.
        for (target, debug) in [
            (json!({"queue": true}), "Queue"),
            (json!({"task": 3}), "Task(TaskId(3))"),
            (json!({"goal": 4}), "Goal(GoalId(4))"),
            (json!({"run": "r9"}), "Run(RunId(\"r9\"))"),
        ] {
            let mut params = json!({"kind": "stall", "summary": "s", "evidence": [7]});
            params
                .as_object_mut()
                .unwrap()
                .extend(target.as_object().unwrap().clone());
            let response =
                service.handle(&request(Some("observer"), UseCase::FindingRecord, params));
            // The store's own error comes back as a failure.
            assert_eq!(code(&response), Some(ServiceErrorCode::Failed));
            assert_eq!(
                pop(),
                json!({"finding": "stall", "target": debug, "by": "observer"})
            );
        }
        let response = service.handle(&request(
            Some("observer"),
            UseCase::FindingResolve,
            json!({"id": 5, "reason": "gone"}),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::Failed));
        assert_eq!(
            pop(),
            json!({"finding": 5, "to": "resolved", "reason": "gone", "by": "observer"})
        );
        // Dismissing and noting are not the observer's, nor is a finding
        // the worker's: refused and recorded as the one refused.
        for (token, use_case, params, capability) in [
            (
                "observer",
                UseCase::FindingDismiss,
                json!({"id": 5, "reason": "no"}),
                "finding.dismiss",
            ),
            (
                "observer",
                UseCase::Note,
                json!({"task": 1, "text": "x"}),
                "note.write",
            ),
            (
                "worker",
                UseCase::FindingRecord,
                json!({"kind": "stall", "summary": "s", "queue": true}),
                "finding.record",
            ),
            (
                "worker",
                UseCase::FindingResolve,
                json!({"id": 5, "reason": "gone"}),
                "finding.resolve",
            ),
        ] {
            let response = service.handle(&request(Some(token), use_case, params));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{use_case:?}"
            );
            let denial = pop();
            assert_eq!(denial["event"], "authorization_denied");
            assert_eq!(denial["role"], token);
            assert_eq!(denial["capability"], capability);
        }
        // The proposals are read by every role.
        for token in ["observer", "worker", "review"] {
            let listed = service.handle(&request(
                Some(token),
                UseCase::ProposalList,
                json!({"all": true}),
            ));
            assert_eq!(listed.result, Some(json!({"proposals": [], "all": true})));
            let shown = service.handle(&request(
                Some(token),
                UseCase::ProposalShow,
                json!({"id": 2}),
            ));
            assert_eq!(shown.result, Some(json!({"proposal": 2})));
        }
        assert!(backend.log.lock().unwrap().is_empty());
    }

    #[test]
    fn every_role_reads_the_whole_queue_and_a_bad_read_is_a_bad_request() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        for token in ["worker", "review", "observer"] {
            let response = service.handle(&request(
                Some(token),
                UseCase::Events,
                json!({"full": true, "run": "r9"}),
            ));
            let read = response.result.unwrap()["read"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(read.starts_with("Events(EventsRead"), "{read}");
            assert!(read.contains("full: true"), "{read}");
            let response = service.handle(&request(Some(token), UseCase::GoalList, Value::Null));
            assert_eq!(
                response.result.unwrap()["read"],
                "GoalList(GoalListRead { tag: [] })"
            );
        }
        for (use_case, params) in [
            (UseCase::Stats, json!({"cmux": "/bin/sh"})),
            (UseCase::Kpi, json!({"period": "year"})),
            (UseCase::Candidates, json!({"x": 1})),
        ] {
            let response = service.handle(&request(Some("worker"), use_case, params));
            assert_eq!(code(&response), Some(ServiceErrorCode::BadRequest));
        }
        assert!(backend.log.lock().unwrap().is_empty());
    }

    /// A report of one step and one resource sample of `session` of the
    /// target `kind` `id`.
    fn sent(kind: &str, id: &str, session: &str) -> Value {
        json!({"version": crate::domain::telemetry::TELEMETRY_VERSION,
               "target": {"kind": kind, "id": id}, "session_id": session,
               "steps": [{"seq": 1, "kind": "model_turn", "offset_ms": 0, "duration_ms": 5}],
               "resources": [{"seq": 1, "offset_ms": 0, "duration_ms": 30000,
                              "cpu_seconds": 1.5, "memory_max_bytes": 1024}]})
    }

    /// [`sent`] of a session registered to its own target.
    fn report(kind: &str, id: &str) -> Value {
        sent(kind, id, &format!("{kind}:{id}:s1"))
    }

    /// ADR-t1662-3 decisions 9 and 10: the telemetry principal records the
    /// steps of its own target and is refused another's; no agent's token
    /// reports, whatever target it names.
    #[test]
    fn only_the_telemetry_principal_reports_and_only_of_its_own_target() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let pop = || backend.log.lock().unwrap().pop().unwrap();
        for (token, kind, id) in [
            ("telemetry", "run", "r1"),
            ("telemetry-planner", "planner", "3"),
        ] {
            let response = service.handle(&request(
                Some(token),
                UseCase::TelemetryReport,
                report(kind, id),
            ));
            assert_eq!(
                response.result,
                Some(json!({"steps": {"added": 1, "duplicates": 0},
                            "resources": {"added": 1, "duplicates": 0}})),
                "{response:?}"
            );
            assert_eq!(
                pop(),
                json!({"steps": 1, "resources": 1, "session": format!("{kind}:{id}:s1"),
                       "target": format!("{kind}:{id}")})
            );
        }
        // Its own target's, of a session registered to no target or to
        // another: refused, so no token takes another's session first.
        for session in ["s1", "planner:9:s1", "run:r2:s1"] {
            let response = service.handle(&request(
                Some("telemetry"),
                UseCase::TelemetryReport,
                sent("run", "r1", session),
            ));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{session}"
            );
            assert_eq!(
                response.error.unwrap().message,
                "executor-telemetry may not telemetry.report (not on this resource)"
            );
            assert_eq!(pop()["role"], "executor-telemetry");
            assert!(backend.log.lock().unwrap().is_empty());
        }
        // Another run's, planner's or job's target: refused and recorded as
        // the telemetry principal, nothing written.
        for (token, kind, id) in [
            ("telemetry", "run", "r2"),
            ("telemetry", "planner", "3"),
            ("telemetry", "plan_review", "4"),
            ("telemetry", "goal_review", "5"),
            ("telemetry", "throughput_review", "6"),
            ("telemetry-planner", "planner", "4"),
            ("telemetry-planner", "run", "r1"),
        ] {
            let response = service.handle(&request(
                Some(token),
                UseCase::TelemetryReport,
                report(kind, id),
            ));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{token} on {kind} {id}"
            );
            let denial = pop();
            assert_eq!(denial["role"], "executor-telemetry");
            assert_eq!(denial["capability"], "telemetry.report");
            assert!(backend.log.lock().unwrap().is_empty());
        }
        // No agent's token reports, of its own run or target included.
        for (token, role, kind, id) in [
            ("worker", "worker", "run", "r1"),
            ("review", "review-job", "run", "r1"),
            ("recovery", "recovery-job", "run", "r1"),
            ("plan-review", "plan-review-job", "plan_review", "4"),
            ("goal-review", "goal-review-job", "goal_review", "5"),
            (
                "throughput-review",
                "throughput-review-job",
                "throughput_review",
                "6",
            ),
            ("observer", "observer", "run", "r1"),
            ("planner", "planner", "planner", "3"),
        ] {
            let response = service.handle(&request(
                Some(token),
                UseCase::TelemetryReport,
                report(kind, id),
            ));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{token}"
            );
            assert_eq!(
                response.error.unwrap().message,
                format!("{role} may not telemetry.report (not granted)")
            );
            assert_eq!(pop()["role"], role);
            assert!(backend.log.lock().unwrap().is_empty());
        }
        // A session whose steps came in under another target is refused.
        let response = service.handle(&request(
            Some("telemetry"),
            UseCase::TelemetryReport,
            sent("run", "r1", "run:r1:taken"),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::BadRequest));
        assert!(backend.log.lock().unwrap().is_empty());
    }

    /// The report's own version, its bounds and the fields it does not
    /// know (docs/design/queue-service.md).
    #[test]
    fn a_report_of_an_unknown_version_or_past_its_bounds_is_refused_and_unknown_fields_dropped() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        let send = |params: Value| {
            service.handle(&request(
                Some("telemetry"),
                UseCase::TelemetryReport,
                params,
            ))
        };
        let mut unknown = report("run", "r1");
        unknown["version"] = json!(crate::domain::telemetry::TELEMETRY_VERSION + 1);
        let mut over = report("run", "r1");
        over["steps"] = Value::Array(
            (1..=crate::domain::telemetry::MAX_STEPS_PER_REPORT + 1)
                .map(|seq| json!({"seq": seq, "kind": "wait", "offset_ms": 0, "duration_ms": 1}))
                .collect(),
        );
        let mut negative = report("run", "r1");
        negative["steps"][0]["seq"] = json!(0);
        let mut negative_sample = report("run", "r1");
        negative_sample["resources"][0]["cpu_seconds"] = json!(-1.0);
        let mut large = report("run", "r1");
        large["steps"][0]["command_shape"] =
            json!("x".repeat(crate::domain::queue_service::MAX_REQUEST_BYTES));
        for (params, says) in [
            (unknown, "version"),
            (over, "at most"),
            (negative, "seq 0"),
            (negative_sample, "resource sample 1"),
            (large, "bytes as JSON"),
            (Value::Null, "version"),
        ] {
            let response = send(params);
            assert_eq!(code(&response), Some(ServiceErrorCode::BadRequest));
            assert!(response.error.unwrap().message.contains(says), "{says}");
        }
        assert!(backend.log.lock().unwrap().is_empty());
        let mut later = report("run", "r1");
        later["sent_by"] = json!({"cpu_seconds": 1});
        later["steps"][0]["later"] = json!(true);
        later["resources"][0]["later"] = json!(true);
        assert!(send(later).ok);
        assert_eq!(backend.log.lock().unwrap().len(), 1);
        // A token whose run ended is no token.
        let ended = self::backend(RunStatus::Integrated);
        let response = QueueService {
            backend: &ended,
            build: "b",
            pid: 1,
        }
        .handle(&request(
            Some("telemetry"),
            UseCase::TelemetryReport,
            report("run", "r1"),
        ));
        assert_eq!(code(&response), Some(ServiceErrorCode::Unauthenticated));
    }

    /// ADR-t1662-3 decision 9: the telemetry principal reaches no use case
    /// but `telemetry_report`, a read included, and changes nothing.
    #[test]
    fn the_telemetry_principal_is_refused_every_other_use_case() {
        let backend = backend(RunStatus::Running);
        let service = QueueService {
            backend: &backend,
            build: "b",
            pid: 1,
        };
        for (use_case, params) in [
            (UseCase::Status, Value::Null),
            (UseCase::Show, json!({"id": 1})),
            (UseCase::Events, json!({"run": "r1"})),
            (UseCase::List, Value::Null),
            (UseCase::GoalList, Value::Null),
            (
                UseCase::Ask,
                json!({"kind": "worker_question", "because": "scope", "topics": ["design_choice"],
                       "question": "q", "run_id": "r1"}),
            ),
            (UseCase::Note, json!({"run": "r1", "text": "x"})),
            (UseCase::ProposalList, Value::Null),
            (
                UseCase::FindingRecord,
                json!({"kind": "k", "run": "r1", "summary": "s"}),
            ),
            (UseCase::ReplanPropose, json!({"run": "r1", "reason": "r"})),
            (UseCase::AgentResults, Value::Null),
        ] {
            let response = service.handle(&request(Some("telemetry"), use_case, params));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{use_case:?}: {response:?}"
            );
            let denial = backend.log.lock().unwrap().pop().unwrap();
            assert_eq!(denial["role"], "executor-telemetry", "{use_case:?}");
            assert!(backend.log.lock().unwrap().is_empty(), "{use_case:?}");
        }
        // Whatever params it sends, no other use case answers it.
        for name in [
            "ask",
            "show",
            "note",
            "proposal_list",
            "proposal_show",
            "finding_record",
            "finding_resolve",
            "finding_dismiss",
            "list",
            "candidates",
            "graph",
            "status",
            "asks",
            "events",
            "timeline",
            "stats",
            "kpi",
            "forecast",
            "notes",
            "marks",
            "findings",
            "search",
            "related",
            "goal_list",
            "goal_show",
            "lint",
            "observe_history",
            "observe_input",
            "agent_eval",
            "agent_results",
            "agent_result",
            "replan_propose",
            "agent_cases",
            "agent_decide",
        ] {
            let use_case: UseCase = name.parse().unwrap();
            let response = service.handle(&request(Some("telemetry"), use_case, json!({})));
            assert_eq!(
                code(&response),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{use_case:?}"
            );
            let log: Vec<Value> = backend.log.lock().unwrap().drain(..).collect();
            assert_eq!(log.len(), 1, "{use_case:?}: {log:?}");
            assert_eq!(log[0]["role"], "executor-telemetry", "{use_case:?}");
        }
        // An agent's token is refused telemetry_report, and recorded,
        // whatever its params.
        for params in [json!({}), json!({"version": 99}), Value::Null] {
            let response =
                service.handle(&request(Some("worker"), UseCase::TelemetryReport, params));
            assert_eq!(code(&response), Some(ServiceErrorCode::AuthorizationDenied));
            let log: Vec<Value> = backend.log.lock().unwrap().drain(..).collect();
            assert_eq!(log.len(), 1, "{log:?}");
            assert_eq!(log[0]["capability"], "telemetry.report");
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
