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

use crate::domain::{EventKind, RecordedKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::session_step::StepTarget;
use super::{
    ActorContext, ActorRole, AuthorizationError, DomainError, LeaseToken, RunId, RunStatus, TaskId,
};

/// The API version this binary speaks. A change a caller of the version
/// before cannot read raises it, and so does one a service of the version
/// before would skip but must act on: from version 2 a host `dagq` names
/// its queue ([`ServiceRequest::queue`]) and an inbox's token its session
/// ([`Principal::session`]), which a service of version 1 neither compares
/// nor checks, so that service refuses every request of version 2 before
/// it reads the token.
pub const API_VERSION: u32 = 2;
/// The oldest API version the service still answers: a worker's or a
/// job's `dagq` of version 1 names no queue, and is answered as before.
pub const MIN_API_VERSION: u32 = 1;
/// The first API version whose host `dagq` names its queue and whose
/// inbox's token names its session ([`API_VERSION`]).
pub const HOST_CLIENT_API_VERSION: u32 = 2;

/// The API version a request of `use_case` is sent at: [`MIN_API_VERSION`]
/// for a hello, which names nothing a service has to act on, so that a
/// service of an older build that does not take [`API_VERSION`] still
/// answers it with its build and is found as one of another build (and
/// replaced) rather than refusing; [`API_VERSION`] for every other.
pub fn request_version(use_case: UseCase) -> u32 {
    if use_case == UseCase::Hello {
        MIN_API_VERSION
    } else {
        API_VERSION
    }
}
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
/// The directory in [`SERVICE_DIR`] where a session wrapper keeps its
/// target's step numbering and unsent steps, one file a target, beside
/// the tokens' files: outside every worktree, run, planner and job
/// directory, mounted in no container and named to no agent.
pub const STEPS_DIR: &str = "steps";
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
/// The variable that names the queue broker a client-mode `dagq` in a
/// run's container sends its commands to instead of a socket
/// (ADR-t2114-3 decision 3): `http://<host>:<port>`, the forwarder's
/// `127.0.0.1` port inside the container
/// ([`crate::domain::broker_relay::FORWARD_PORT`]), with no path but `/`.
/// With it, `dagq` opens no queue and sends each command as the broker's
/// request ([`crate::domain::queue_broker`]): the token of
/// [`CREDENTIAL_FILE_ENV`]'s file as the `Bearer`, and the run of
/// [`crate::domain::actor::RUN_ID_ENV`] as the run header. A `dagq` given
/// both it and [`SOCKET_ENV`] refuses every command and opens no queue
/// (fail closed).
pub const URL_ENV: &str = "DAGQ_SERVICE_URL";
/// The variables of a client-mode `dagq`.
pub const CLIENT_ENV: [&str; 3] = [SOCKET_ENV, URL_ENV, CREDENTIAL_FILE_ENV];
/// The longest request line the service reads.
pub const MAX_REQUEST_BYTES: usize = 1 << 20;
/// The longest answer line, its newline included, the service writes and a
/// client-mode `dagq` reads, by the socket or through the broker: the
/// service answers a longer one's use case with
/// [`ServiceErrorCode::ResponseTooLarge`] instead, and a client that reads
/// past it (a service of another limit) refuses it with the same code.
pub const MAX_ANSWER_BYTES: usize = MAX_REQUEST_BYTES * 16;

/// The message of a [`ServiceErrorCode::ResponseTooLarge`] of `use_case`
/// (`None`: a request that did not read): the answer's `size` (`None`: a
/// client that stopped reading at the limit), [`MAX_ANSWER_BYTES`], and how
/// to narrow that read.
pub fn too_large_message(use_case: Option<UseCase>, size: Option<usize>) -> String {
    let what = use_case.map_or("the request", UseCase::as_str);
    let size = size.map_or_else(
        || "is longer than".to_owned(),
        |size| format!("is {size} bytes, past"),
    );
    format!(
        "the queue service's answer to {what} {size} the {MAX_ANSWER_BYTES} bytes a client-mode \
         dagq reads, and is not sent in part: {}",
        narrowing(use_case)
    )
}

/// How to narrow a read of `use_case` to an answer within
/// [`MAX_ANSWER_BYTES`]: the options of its command that take less. Every
/// use case is named, so a new one decides its own.
fn narrowing(use_case: Option<UseCase>) -> &'static str {
    let Some(use_case) = use_case else {
        return "narrow the read with the options of its command (dagq <command> --help)";
    };
    match use_case {
        UseCase::Events => {
            "narrow the window with --since / --until, take fewer with a smaller --limit, read \
             the next page with --after <cursor> (the cursor of the last answer), narrow with \
             --run / --task / --goal / --kind, or leave out --full"
        }
        UseCase::Notes => {
            "take fewer with a smaller --limit and read on with --since <cursor>, or narrow with \
             --task / --goal"
        }
        UseCase::List => {
            "take a smaller page with --limit and go on with --before <next>, narrow with \
             --status / --goal, or leave out --full / --all"
        }
        UseCase::Search => {
            "take fewer with a smaller --limit, narrow with --kind / --status / --goal, or leave \
             out --full"
        }
        UseCase::Related => "take fewer with a smaller --limit, or narrow with --status",
        UseCase::Stats => {
            "narrow the window with --since / --until or the runs with --goal, or leave out --full"
        }
        UseCase::Kpi => {
            "narrow the window with --since / --until, list fewer periods with a smaller --last, \
             or narrow the runs with --goal / --change / --area"
        }
        UseCase::Marks => "narrow the window with --since / --until",
        UseCase::Findings => {
            "narrow with --status / --kind / --task / --run / --goal, or leave out --full / --all"
        }
        UseCase::Graph => "narrow to one goal with --goal",
        UseCase::Forecast => "narrow with --task / --goal",
        UseCase::Status => "narrow the attention to one role's with --role",
        UseCase::WatchPoll => {
            "narrow the attention to one role's with --role, or pass a later --after"
        }
        UseCase::Asks => "leave out --all, or narrow with --open / --role",
        UseCase::Show => "leave out --full, or show fewer events with a smaller --events",
        UseCase::Timeline => "leave out --full, or report fewer gaps with a larger --gap",
        UseCase::GoalShow => "leave out --full",
        UseCase::GoalList => "narrow with --tag",
        UseCase::ProposalList => "leave out --all",
        UseCase::Candidates => "leave out --ignore-deferrals",
        UseCase::Lint => "check fewer proposals at a time (fewer --proposal)",
        UseCase::ObserveHistory => "list fewer with a smaller --limit",
        UseCase::ObserveInput => {
            "read one section with --section, and fewer of its items with --offset and a smaller \
             --limit"
        }
        UseCase::AgentResults | UseCase::AgentSets => {
            "take fewer with a smaller --limit, or narrow with --agent"
        }
        UseCase::Requests | UseCase::Replans => "name one with its ID, or leave out --all",
        UseCase::CiFailures => "narrow to one task's with --task",
        UseCase::Planners => "leave out --all",
        UseCase::ProposalShow | UseCase::AgentResult | UseCase::AgentSet => {
            "its command has no option that narrows the answer"
        }
        UseCase::RunLog | UseCase::PlannerLog => {
            "the answer names the log's file, which the client reads: none is longer"
        }
        UseCase::GraphExport => "narrow to one goal with --goal",
        UseCase::Hello
        | UseCase::Answer
        | UseCase::AskClose
        | UseCase::Hold
        | UseCase::Release
        | UseCase::ReleaseRead
        | UseCase::ReleaseImages
        | UseCase::ReplanRequest
        | UseCase::Rebind
        | UseCase::Recover
        | UseCase::RunScreen
        | UseCase::RunSend
        | UseCase::PlannerScreen
        | UseCase::PlannerSend
        | UseCase::PlannerRequest
        | UseCase::Report
        | UseCase::ReportInputs
        | UseCase::Ask
        | UseCase::Note
        | UseCase::FindingRecord
        | UseCase::FindingResolve
        | UseCase::FindingDismiss
        | UseCase::AgentEval
        | UseCase::ReplanPropose
        | UseCase::AgentCases
        | UseCase::AgentDecide
        | UseCase::TelemetryReport
        | UseCase::Supervisor
        | UseCase::SessionRun
        | UseCase::SessionRecord
        | UseCase::Add
        | UseCase::Edit
        | UseCase::Submit
        | UseCase::Cancel
        | UseCase::DependencyAdd
        | UseCase::DependencyRemove
        | UseCase::SetGoal
        | UseCase::SetPaths
        | UseCase::SetPriority
        | UseCase::Revisit
        | UseCase::JudgeFollowUp
        | UseCase::Draft
        | UseCase::Ready
        | UseCase::GoalAdd
        | UseCase::GoalEdit
        | UseCase::GoalClose
        | UseCase::GoalReady
        | UseCase::GoalReview
        | UseCase::ProposalWithdraw
        | UseCase::RequestAdd
        | UseCase::RequestDecline
        | UseCase::Mark => {
            "narrow the read with the options of its command (dagq <command> --help)"
        }
    }
}

/// The `<host>:<port>` the broker's URL `url` ([`URL_ENV`]) names, or why
/// it names none: only `http://`, a host and a port, and no path but `/`
/// (no user, query or fragment).
pub fn broker_address(url: &str) -> Result<String, String> {
    let why = |what: &str| format!("{URL_ENV}={url:?} {what}: give http://<host>:<port>");
    let rest = url
        .trim()
        .strip_prefix("http://")
        .ok_or_else(|| why("is not an http URL"))?;
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.contains(['/', '?', '#', '@']) {
        return Err(why("names more than a host and a port"));
    }
    match authority.rsplit_once(':') {
        Some((host, port))
            if !host.is_empty() && !port.is_empty() && port.parse::<u16>().is_ok() =>
        {
            Ok(authority.to_owned())
        }
        _ => Err(why("names no host and port")),
    }
}

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
    // The planning requests, the replanning operations, the landing
    // branch's known CI failures and the planners with their states, as
    // `dagq requests`, `dagq replans`, `dagq ci failures` and `dagq
    // planners` print them.
    Requests => "requests",
    Replans => "replans",
    CiFailures => "ci_failures",
    Planners => "planners",
    // The eval of an agent (ADR-t1728-1 decisions 5 and 6): a request of a
    // round, which the supervisor runs, as `dagq agent eval` records it,
    // and the rounds and one round read back, as `dagq agent results` and
    // `dagq agent result` print them; a worker's on its own run only.
    AgentEval => "agent_eval",
    AgentResults => "agent_results",
    AgentResult => "agent_result",
    // A worker's proposal to replan its own run, as `dagq replan
    // propose` records it (ADR-t1521-2 decision 3).
    ReplanPropose => "replan_propose",
    // One look of `dagq watch` at the queue (ADR-t3102-3 decision 2): the
    // attention past a cursor for a role and the supervisors' health, read
    // at once and never waited on in the service, so a client-mode watch
    // waits between its looks itself and finds at its next look that the
    // service does not answer.
    WatchPoll => "watch_poll",
    // A production set of an agent's cases (ADR-t1728-1 (g)): a request,
    // which the supervisor makes, and a decision on a case's label, as
    // `dagq agent cases` and `dagq agent decide` record them; a person's,
    // the inbox's or a planner's, so a worker's is refused and recorded.
    // The sets and one set read back, as `dagq agent sets` and `dagq agent
    // set` print them: an actor's with `eval.read`.
    AgentCases => "agent_cases",
    AgentDecide => "agent_decide",
    AgentSets => "agent_sets",
    AgentSet => "agent_set",
    // The steps of a session an executor sends of the one target its
    // telemetry principal was issued for (ADR-t1662-3 decisions 2 and 9),
    // with a version of its params of their own
    // ([`crate::domain::telemetry`]): the only use case that principal
    // reaches, and one no agent's token reaches. No command line goes to
    // it.
    TelemetryReport => "telemetry_report",
    // An operation of a supervisor on the queue (ADR-t3102-3 decisions 3
    // and 4): one of the queue's operations that each run in one
    // transaction (a claim, a lease's heartbeat, a run's transition with
    // its event, a read the supervisor decides on, the issue of a worker's
    // or an executor's token), named by its `op`
    // ([`SupervisorParams`]). Only the supervisor principal reaches it
    // (`scheduler.supervise`), and every lease it names is the one the
    // principal was issued for. No command line goes to it.
    Supervisor => "supervisor",
    // A session wrapper's operations on its own target, a run or a
    // planner (ADR-t3102-1 decision 4, ADR-t3102-3 decision 3), each the
    // one transaction it was on the DB: its registration, heartbeats,
    // agents and exit, and the reads it waits and starts its turns on
    // ([`crate::application::session::SessionOp`]). Only the wrapper's
    // principal holds `session.run`; no command line goes to it.
    SessionRun => "session_run",
    // What a session wrapper records of its own target: its events, and
    // the hook's spans it relays (ADR-t3102-1 decision 4). Only the
    // wrapper's principal (and the inbox's, on the queue) holds
    // `session.record`; no command line goes to it.
    SessionRecord => "session_record",
    // The planning commands a person, the inbox or a planner runs
    // (ADR-t3102-3 decision 3), each as the command of its name changes
    // the queue and prints it, authorized by the application for the
    // caller ([`crate::application::queue_writes`]): `ready` without
    // `--inherit`, which moves a run's branch.
    Add => "add",
    Edit => "edit",
    Submit => "submit",
    Cancel => "cancel",
    DependencyAdd => "dependency_add",
    DependencyRemove => "dependency_remove",
    SetGoal => "set_goal",
    SetPaths => "set_paths",
    SetPriority => "set_priority",
    Revisit => "revisit",
    JudgeFollowUp => "judge_follow_up",
    Draft => "draft",
    Ready => "ready",
    GoalAdd => "goal_add",
    GoalEdit => "goal_edit",
    GoalClose => "goal_close",
    GoalReady => "goal_ready",
    GoalReview => "goal_review",
    ProposalWithdraw => "proposal_withdraw",
    RequestAdd => "request_add",
    RequestDecline => "request_decline",
    // Record a change mark, or retract one, as `dagq mark` does.
    Mark => "mark",
    // The dialogue and the operations a person or the inbox runs on the
    // host, and a planner as far as the policy grants it (ADR-t3102-2
    // decision 6 (4)), each as the command of its name runs it on the DB
    // ([`crate::application::queue_commands::QueueCommand`]): an answer
    // and an ask's close, the hold of a task and its release, a replanning
    // request, the queue's binding to a repository and a run's recovery.
    Answer => "answer",
    AskClose => "ask_close",
    Hold => "hold",
    Release => "release",
    // What `dagq release` asks for first: the run whose work the release
    // reads, which the client reads and keeps with Git itself before it
    // sends the release. No command line goes to it alone.
    ReleaseRead => "release_read",
    // The records of the image a release's client made ready for a run's
    // facts-only turn, which the client keeps and sends once it read the
    // run's work, read or not. No command line goes to it alone.
    ReleaseImages => "release_images",
    ReplanRequest => "replan_request",
    Rebind => "rebind",
    Recover => "recover",
    // A run's and a planner's screen, input and log, as `dagq run
    // screen|send|log` and `dagq planner screen|send|log` answer them: the
    // log's answer is the background session it is read from, and the
    // client reads the file and follows it itself.
    RunScreen => "run_screen",
    RunSend => "run_send",
    RunLog => "run_log",
    PlannerScreen => "planner_screen",
    PlannerSend => "planner_send",
    PlannerLog => "planner_log",
    // A follow-up request handed to a headless planner, as `dagq planner
    // request` hands it: the client reads the words (`--text-file`, `-`).
    PlannerRequest => "planner_request",
    // The KPI report of a period, as `dagq report` makes and writes it,
    // with the near-term diagram the client drew.
    Report => "report",
    // What `dagq report` asks for first and reads Git and draws with
    // itself: the near-term diagram's d2 source, the repository the queue
    // is bound to and the landed commits. No command line goes to it
    // alone.
    ReportInputs => "report_inputs",
    // The dependency diagram `dagq graph --out` writes: its d2 source and
    // the tasks it shows, which the client draws (`svg`) and writes.
    GraphExport => "graph_export",
});

impl UseCase {
    /// The use case a `dagq` command line goes to through the service:
    /// `words` are its arguments after the global options, the subcommand
    /// first. `None` for a command the service does not answer (the
    /// control side's, `watch`, whose looks only the host's `dagq` sends as
    /// [`Self::WatchPoll`], `observe` without `--history` or `--input`,
    /// `locate`, `doctor`, `ready --inherit`, ...).
    pub fn of_command(words: &[&str]) -> Option<Self> {
        let has = |flag: &str| {
            words
                .iter()
                .any(|word| *word == flag || word.starts_with(&format!("{flag}=")))
        };
        let command = *words.first()?;
        let next = words.get(1).copied();
        Some(match (command, next) {
            ("ask", Some("close")) => Self::AskClose,
            ("ask", _) => Self::Ask,
            ("answer", _) => Self::Answer,
            ("hold", _) => Self::Hold,
            ("release", _) => Self::Release,
            ("replan", Some("request")) => Self::ReplanRequest,
            ("rebind", _) => Self::Rebind,
            ("recover", _) => Self::Recover,
            ("run", Some("screen")) => Self::RunScreen,
            ("run", Some("send")) => Self::RunSend,
            ("run", Some("log")) => Self::RunLog,
            ("planner", Some("screen")) => Self::PlannerScreen,
            ("planner", Some("send")) => Self::PlannerSend,
            ("planner", Some("log")) => Self::PlannerLog,
            ("planner", Some("request")) => Self::PlannerRequest,
            ("report", _) => Self::Report,
            ("show", _) => Self::Show,
            ("note", _) => Self::Note,
            ("proposal", Some("list")) => Self::ProposalList,
            ("proposal", Some("show")) => Self::ProposalShow,
            ("finding", Some("record")) => Self::FindingRecord,
            ("finding", Some("resolve")) => Self::FindingResolve,
            ("finding", Some("dismiss")) => Self::FindingDismiss,
            ("list", _) => Self::List,
            ("candidates", _) => Self::Candidates,
            ("graph", _) if has("--out") => Self::GraphExport,
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
            ("requests", _) => Self::Requests,
            ("replans", _) => Self::Replans,
            ("ci", Some("failures")) => Self::CiFailures,
            ("planners", _) => Self::Planners,
            ("agent", Some("eval")) => Self::AgentEval,
            ("agent", Some("results")) => Self::AgentResults,
            ("agent", Some("result")) => Self::AgentResult,
            ("replan", Some("propose")) => Self::ReplanPropose,
            ("agent", Some("cases")) => Self::AgentCases,
            ("agent", Some("decide")) => Self::AgentDecide,
            ("agent", Some("sets")) => Self::AgentSets,
            ("agent", Some("set")) => Self::AgentSet,
            ("add", _) => Self::Add,
            ("edit", _) => Self::Edit,
            ("submit", _) => Self::Submit,
            ("cancel", _) => Self::Cancel,
            ("dependency", Some("add")) => Self::DependencyAdd,
            ("dependency", Some("remove")) => Self::DependencyRemove,
            ("set-goal", _) => Self::SetGoal,
            ("set-paths", _) => Self::SetPaths,
            ("set-priority", _) => Self::SetPriority,
            ("revisit", _) => Self::Revisit,
            ("judge-follow-up", _) => Self::JudgeFollowUp,
            ("draft", _) => Self::Draft,
            ("ready", _) if has("--inherit") => return None,
            ("ready", _) => Self::Ready,
            ("goal", Some("add")) => Self::GoalAdd,
            ("goal", Some("edit")) => Self::GoalEdit,
            ("goal", Some("close")) => Self::GoalClose,
            ("goal", Some("ready")) => Self::GoalReady,
            ("goal", Some("review")) => Self::GoalReview,
            ("proposal", Some("withdraw")) => Self::ProposalWithdraw,
            ("request", Some("add")) => Self::RequestAdd,
            ("request", Some("decline")) => Self::RequestDecline,
            ("mark", _) => Self::Mark,
            _ => return None,
        })
    }
}

string_enum!(
    /// Why the service, or the queue broker in front of it, refused or
    /// failed a request: the `code` of a [`ServiceError`], spelled as each
    /// variant's text. A client reads a refusal of either by this code.
    ServiceErrorCode {
    /// `api_version_mismatch`: the caller's API version is not one the
    /// service answers.
    ApiVersionMismatch => "api_version_mismatch",
    /// `unauthenticated`: no token, one the service did not issue, or one
    /// revoked or ended with its run. The queue broker refuses with it too:
    /// a request with no run token or no run named, a token not issued or
    /// revoked, a run ended, a token that is not a worker's run token, or
    /// one of another run than the request names
    /// ([`crate::domain::queue_broker::TokenRefusal`]).
    Unauthenticated => "unauthenticated",
    /// `authorization_denied`: the policy refused the principal (recorded
    /// as `authorization_denied`).
    AuthorizationDenied => "authorization_denied",
    /// `bad_request`: the request could not be read (the broker's too: a
    /// request that is not HTTP it reads, or a body that is no request).
    BadRequest => "bad_request",
    /// `failed`: the use case failed (a missing task, a bad option, ...),
    /// or the broker could not read a token or a run.
    Failed => "failed",
    /// `forbidden_destination`, only the queue broker's
    /// ([`crate::domain::queue_broker`]): the request goes to a destination
    /// the broker does not pass (anything but the queue service and the
    /// package registries `dagq.toml` names, `api.anthropic.com` included
    /// now), recorded as `queue_broker_denied`.
    ForbiddenDestination => "forbidden_destination",
    /// `unreachable`, only the queue broker's: the queue service did not
    /// answer the request the broker passed on. The broker never opens the
    /// queue instead.
    Unreachable => "unreachable",
    /// `wrong_queue`: the request names a queue DB other than the one the
    /// service serves (the host's `dagq` resolved another queue in the same
    /// directory), checked before anything is read.
    WrongQueue => "wrong_queue",
    /// `response_too_large`: the answer is longer than
    /// [`MAX_ANSWER_BYTES`], so the service sends this instead of a part of
    /// it, with how to narrow the read ([`too_large_message`]).
    ResponseTooLarge => "response_too_large",
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
    /// [`UseCase::Hello`] and a person's (ADR-t3102-1 decision 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The role the caller's environment names (`DAGQ_ROLE`), sent only
    /// with no token: a request that names one and carries no token is
    /// refused as `missing_token` and never taken for the user's
    /// (ADR-t3102-1 decision 1). Not what the service authorizes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_role: Option<String>,
    /// The queue DB the caller resolved (the host's `dagq`, from `--db` or
    /// its working directory), which the service compares with its own
    /// before anything else and refuses as [`ServiceErrorCode::WrongQueue`]
    /// when they differ, opening neither. `None` from a caller that names
    /// no queue (a worker's or a job's, given only the socket).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<std::path::PathBuf>,
    /// The Git common directory of the repository the host's `dagq` ran in
    /// for a repository queue, which the service compares with the one the
    /// queue is bound to before it answers, as the command line did on
    /// every command of a repository queue; `None` from a caller that names
    /// none (a `--db` queue's, a worker's or a job's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    pub use_case: UseCase,
    #[serde(default)]
    pub params: Value,
}

/// Why a request was not done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceError {
    pub code: ServiceErrorCode,
    pub message: String,
    /// What the policy refused, with `authorization_denied` only: the
    /// values its event records. A client that does not know the field
    /// ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied: Option<Box<ServiceDenial>>,
    /// What kind of failure a `failed` is, when a caller decides on it
    /// ([`FailureKind`]): a client that does not know the field ignores
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<FailureKind>,
}

/// A failure of a use case the caller tells apart from the others, as the
/// queue's own error would be told apart: the supervisor reads them as it
/// read the queue's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The run named does not exist.
    RunNotFound(RunId),
    /// Another writer held the queue's lock past its wait: a passing
    /// condition the same write may try again.
    QueueBusy,
}

/// A supervisor's request ([`UseCase::Supervisor`]): the operation, its
/// arguments in the order the queue's operation takes them, and the actor
/// and the requester its events record when they are not the principal's
/// own (the landing's integrator, a job whose verdict it applies).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorParams {
    pub op: String,
    #[serde(default)]
    pub args: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acting: Option<Principal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
}

/// A refusal of the policy as the service answers it: the role, the
/// capability and the reason as the `authorization_denied` event and the
/// command line's `denied` spell them. Kept as text, so that a client
/// reads a role or a capability it does not know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceDenial {
    pub role: String,
    pub capability: String,
    pub reason: String,
}

impl From<&AuthorizationError> for ServiceDenial {
    fn from(error: &AuthorizationError) -> Self {
        Self {
            role: error.role.as_str().to_owned(),
            capability: error.capability.as_str().to_owned(),
            reason: error.reason.as_str().to_owned(),
        }
    }
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
                denied: None,
                kind: None,
            }),
        }
    }

    /// `response_too_large` for an answer of `size` bytes to `use_case`.
    pub fn too_large(use_case: Option<UseCase>, size: usize) -> Self {
        Self::failure(
            ServiceErrorCode::ResponseTooLarge,
            too_large_message(use_case, Some(size)),
        )
    }

    /// `authorization_denied` for `refused`, with its message and what it
    /// refused.
    pub fn denied(refused: &AuthorizationError) -> Self {
        let mut response =
            Self::failure(ServiceErrorCode::AuthorizationDenied, refused.to_string());
        if let Some(error) = &mut response.error {
            error.denied = Some(Box::new(refused.into()));
        }
        response
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
/// job on a run). An executor's telemetry principal (ADR-t1662-3
/// decisions 9 and 10) has its one target too, and a run target's run. The
/// service acts as this principal and never as the role a caller names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub role: ActorRole,
    pub actor_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// The one target of an executor's telemetry principal or a session
    /// wrapper, and the planner of a planner's; `None` for every other
    /// principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<StepTarget>,
    /// The registration of a supervisor principal (ADR-t3102-1 decisions 2
    /// and 3): the lease token every run lease it takes, renews or gives
    /// back carries. `None` for every other principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration: Option<LeaseToken>,
    /// The inbox's session (ADR-t3102-1 decisions 2 and 3): the process
    /// of its agent, whose end ends the token. `None` for every other
    /// principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionProcess>,
}

/// A session a token is bound to: the process of its agent, by its pid and
/// the start the system prints for it in UTC (`ProcessControl::session_start`),
/// so that another process that takes the pid later is not taken for it. `dagq inbox` puts the inbox's agent
/// in its own process's place, so the pid and the start are those of the
/// `dagq inbox` that issued the token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProcess {
    pub pid: u32,
    pub start: String,
}

impl Principal {
    /// The worker (or its resume) of `run` on `task`.
    pub fn worker(run: &RunId, task: TaskId) -> Self {
        Self::of(&ActorContext::worker(run, task))
    }

    /// The user: a request with no token and no role from a peer of the
    /// service's own OS user on its socket (ADR-t3102-1 decision 1). No
    /// token is ever issued for it ([`Self::check`] refuses it).
    pub fn user() -> Self {
        Self::of(&ActorContext::user())
    }

    /// The inbox of the session `session` (ADR-t3102-1 decisions 2 and 3):
    /// its actor id is the inbox's, and its token ends with the session's
    /// process, or when the inbox is opened again (an issue for the same
    /// actor revokes the one before).
    pub fn inbox(session: SessionProcess) -> Self {
        Self {
            session: Some(session),
            ..Self::of(&ActorContext::new(
                ActorRole::Inbox,
                ActorRole::Inbox.as_str(),
            ))
        }
    }

    /// Planner `id` of the runtime's (ADR-t3102-1 decision 2): its actor
    /// id is `planner:<id>`, and its one target the planner, so its token
    /// ends when the planner is closed.
    pub fn planner(id: i64) -> Self {
        Self {
            target: Some(StepTarget::of(super::session_step::TargetKind::Planner, id)),
            ..Self::of(&ActorContext::instance(ActorRole::Planner, id))
        }
    }

    /// The principal the control side issues, as it starts its turn, for
    /// `actor`, a planner whose `dagq` takes a token: the planner its actor
    /// id names ([`Self::planner`]). `None` for a planner whose id names
    /// none and any other role: the user takes no token, and the inbox's
    /// is issued by `dagq inbox` for its session ([`Self::inbox`]).
    pub fn host_client(actor: &ActorContext) -> Option<Self> {
        match actor.role() {
            ActorRole::Planner => actor
                .actor_id()
                .strip_prefix("planner:")
                .and_then(|id| id.parse().ok())
                .map(Self::planner),
            _ => None,
        }
    }

    /// The principal of `actor`.
    pub fn of(actor: &ActorContext) -> Self {
        Self {
            role: actor.role(),
            actor_id: actor.actor_id().to_owned(),
            run_id: actor.run_id().cloned(),
            task_id: actor.task_id(),
            target: None,
            registration: None,
            session: None,
        }
    }

    /// The supervisor of `registration`, process `pid`: its actor is the
    /// one the supervisor's own events record (`supervisor:<pid>`).
    pub fn supervisor(registration: &LeaseToken, pid: u32) -> Self {
        Self {
            registration: Some(registration.clone()),
            ..Self::of(&ActorContext::instance(ActorRole::Supervisor, pid))
        }
    }

    /// The telemetry principal of `target` (ADR-t1662-3 decision 10): its
    /// actor id is the target's ([`super::telemetry::actor_id`]), and a run
    /// target's names the run and its `task`, so its token ends with the
    /// run as a worker's does. A target of no run names no task.
    pub fn telemetry(target: &StepTarget, task: Option<TaskId>) -> Self {
        let run_id = target.run_id();
        Self {
            role: ActorRole::ExecutorTelemetry,
            actor_id: super::telemetry::actor_id(target),
            task_id: run_id.as_ref().and(task),
            run_id,
            target: Some(target.clone()),
            registration: None,
            session: None,
        }
    }

    /// The session wrapper of `target`, a run (of `task`) or a planner,
    /// with its hook (ADR-t3102-1 decision 4): its actor id is the
    /// wrapper's ([`wrapper_actor_id`]), and a run's names the run and its
    /// task, so its token ends with the run as a worker's does.
    pub fn wrapper(target: &StepTarget, task: Option<TaskId>) -> Self {
        let run_id = target.run_id();
        Self {
            role: ActorRole::Wrapper,
            actor_id: wrapper_actor_id(target),
            task_id: run_id.as_ref().and(task),
            run_id,
            target: Some(target.clone()),
            registration: None,
            session: None,
        }
    }

    /// The principals the control side issues tokens for: the AI actors
    /// (ADR-t1233-4 decision 4), an executor's telemetry principal whose
    /// actor id and run are its one target's (ADR-t1662-3 decisions 9 and
    /// 13), a supervisor, whose one target is its registration
    /// (ADR-t3102-1 decision 3), the session wrapper of one run (with its
    /// task) or one planner, the inbox of one session, and a planner whose
    /// one target is the planner its actor id names (ADR-t3102-1 decision
    /// 3). No other principal has a target, a registration or a session.
    /// The user takes no token (decision 1), and the rest of the control
    /// side (the integrator) opens the DB itself until its stage.
    pub fn check(&self) -> Result<(), DomainError> {
        let refuse = |value: String| {
            Err(DomainError::UnknownValue {
                kind: "Principal role",
                value,
            })
        };
        if let Some(registration) = &self.registration {
            return if self.role == ActorRole::Supervisor
                && self.target.is_none()
                && self.run_id.is_none()
                && self.task_id.is_none()
                && !registration.as_str().is_empty()
            {
                Ok(())
            } else {
                refuse(format!("{} of a registration", self.role.as_str()))
            };
        }
        match (&self.session, self.role) {
            (Some(_), ActorRole::Inbox) => {}
            (Some(session), role) => {
                return refuse(format!("{} of session {}", role.as_str(), session.pid));
            }
            (None, ActorRole::Inbox) => return refuse("inbox of no session".to_owned()),
            (None, _) => {}
        }
        match (&self.target, self.role) {
            (None, ActorRole::Planner) => refuse("planner of no planner".to_owned()),
            (None, role) if role.trust() == super::TrustLevel::UntrustedAgent => Ok(()),
            (None, ActorRole::Inbox) if self.run_id.is_none() && self.task_id.is_none() => Ok(()),
            (Some(target), ActorRole::Planner)
                if target.kind == super::session_step::TargetKind::Planner
                    && self.actor_id == format!("planner:{}", target.id)
                    && self.run_id.is_none()
                    && self.task_id.is_none() =>
            {
                Ok(())
            }
            (Some(target), ActorRole::ExecutorTelemetry)
                if self.actor_id == super::telemetry::actor_id(target)
                    && self.run_id == target.run_id()
                    && (target.kind != super::session_step::TargetKind::Run
                        || self.run_id.is_some())
                    && (self.run_id.is_some() || self.task_id.is_none()) =>
            {
                Ok(())
            }
            (Some(target), ActorRole::Wrapper)
                if self.actor_id == wrapper_actor_id(target)
                    && self.run_id == target.run_id()
                    && match target.kind {
                        super::session_step::TargetKind::Run => {
                            self.run_id.is_some() && self.task_id.is_some()
                        }
                        super::session_step::TargetKind::Planner => {
                            target.id.parse::<i64>().is_ok() && self.task_id.is_none()
                        }
                        _ => false,
                    } =>
            {
                Ok(())
            }
            (Some(target), role) => refuse(format!(
                "{} of {} {}",
                role.as_str(),
                target.kind.as_str(),
                target.id
            )),
            (None, role) => refuse(role.as_str().to_owned()),
        }
    }

    /// The name its token is kept under, which a new issue for it
    /// replaces: its actor id, and a supervisor's registration with it, as
    /// one process may run more than one supervisor (a test's does) under
    /// the one actor id its events record, or an inbox's session with it,
    /// so that the file a session was handed never holds the token of a
    /// session opened after it (ADR-t3102-1 decision 2).
    pub fn credential_key(&self) -> String {
        match (&self.registration, &self.session) {
            (Some(registration), _) => format!("{}@{}", self.actor_id, registration.as_str()),
            (None, Some(session)) => {
                format!("{}@{}:{}", self.actor_id, session.pid, session.start)
            }
            (None, None) => self.actor_id.clone(),
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

/// The actor id of the session wrapper of `target`, as its events record
/// it: `wrapper:<run>` for a run's, `wrapper:planner:<id>` for a
/// planner's.
pub fn wrapper_actor_id(target: &StepTarget) -> String {
    match target.kind {
        super::session_step::TargetKind::Run => {
            ActorContext::instance(ActorRole::Wrapper, &target.id)
        }
        kind => ActorContext::instance(
            ActorRole::Wrapper,
            format_args!("{}:{}", kind.as_str(), target.id),
        ),
    }
    .actor_id()
    .to_owned()
}

/// Whether the `dagq` of an actor of `role` runs in client mode (goal 82's
/// stage (3)): the worker (and its resume) and the headless jobs with the
/// observer get the service's socket and a token instead of the queue's
/// path. The inbox and the planners keep the queue's path
/// ([`host_client_role`]), as the control side does but the supervisor
/// ([`UseCase::Supervisor`]).
pub const fn client_role(role: ActorRole) -> bool {
    matches!(role, ActorRole::Worker | ActorRole::Observer) || role.is_headless_job()
}

/// Whether the host `dagq` of an actor of `role` sends the commands the
/// service answers to the service of the queue it resolves, once that
/// queue is served (ADR-t3102-2 decisions 5 and 6 (2)): the user as the
/// peer of its socket, the inbox and a planner with the token the control
/// side put in a file their environment names, with the queue's path kept
/// for the commands the service does not answer yet, which open the DB.
/// Never back to the DB for one the service answers (fail closed).
pub const fn host_client_role(role: ActorRole) -> bool {
    matches!(
        role,
        ActorRole::User | ActorRole::Inbox | ActorRole::Planner
    )
}

/// Whether a token issued on a run still holds while the run is in
/// `status`: one ended (landed, succeeded, failed, interrupted) ends its
/// tokens (ADR-t1233-4 decision 4). The service refuses as well a token
/// whose run is not in the queue, which has no status to ask this of.
pub const fn run_holds_token(status: RunStatus) -> bool {
    !matches!(
        status,
        RunStatus::Integrated | RunStatus::Succeeded | RunStatus::Failed | RunStatus::Interrupted
    )
}

/// Whether the token of `principal` is an inbox's whose session ended:
/// `alive` says whether a session's process still runs (its pid and its
/// start). The service refuses such a token already; the supervisor's
/// sweep removes its value and record. Every other principal's is left.
pub fn session_token_ended(principal: &Principal, alive: impl Fn(&SessionProcess) -> bool) -> bool {
    principal
        .session
        .as_ref()
        .is_some_and(|session| !alive(session))
}

/// The name the supervisor's sweep, through the service, revokes the
/// token of `principal` by once it ended (ADR-t3102-1 decision 2): its
/// [`Principal::credential_key`], so that an inbox's is found in the file
/// of its own session, and an inbox opened again since, whose key names
/// another session, keeps its token. `None` for a supervisor's sign-in,
/// which only that supervisor gives back.
pub fn swept_token_key(principal: &Principal) -> Option<String> {
    let supervisor = principal.role == ActorRole::Supervisor
        || principal.registration.is_some()
        || principal.actor_id.starts_with("supervisor:");
    (!supervisor).then(|| principal.credential_key())
}

/// Whether the token of `principal` is a planner's or a planner's session
/// wrapper's whose planner ended (ADR-t3102-1 decision 2); `None` for every
/// other principal (an executor's telemetry token of a planner is its own
/// port's to revoke). `planner` reads the planner the token's target names,
/// `None` when it does not read, which keeps the token. A planner's own
/// lasts across its turns until the planner closes, as the service reads it
/// (`target_ended`); its wrapper's ends as its agent exits too.
pub fn planner_token_ended(
    principal: &Principal,
    planner: impl FnOnce(super::PlannerId) -> Option<super::planner::PlannerSession>,
) -> Option<bool> {
    let target = principal
        .target
        .as_ref()
        .filter(|target| target.kind == super::session_step::TargetKind::Planner)?;
    let wrapper = match principal.role {
        ActorRole::Wrapper => true,
        ActorRole::Planner => false,
        _ => return None,
    };
    let session = target
        .id
        .parse()
        .ok()
        .map(super::PlannerId::new)
        .and_then(planner);
    Some(session.is_some_and(|session| {
        session.closed_at.is_some() || (wrapper && session.exited_at.is_some())
    }))
}

/// What to do next when a request of an environment naming `role` came
/// with no token and was refused as `missing_token` (ADR-t3105-1 decision
/// 2): an inbox or a planner opened before its build gave it a token's
/// file has none, and is never taken for the user nor sent to the DB.
/// `None` for any other role.
pub fn missing_token_next(role: &str) -> Option<&'static str> {
    match role.parse::<ActorRole>().ok()? {
        ActorRole::Inbox => Some(
            "an inbox opened before it was given a token has none: open the inbox again \
             (`dagq inbox` in a terminal with no DAGQ_ROLE)",
        ),
        ActorRole::Planner => Some(
            "a planner opened before it was given a token has none: wait for it to close \
             (the next planner is given one)",
        ),
        _ => None,
    }
}

/// Whether the token of `principal` is a worker's or a run's session
/// wrapper's whose run ended: `status` is its run's, `None` for a run not
/// in the queue. The supervisor revokes such a token's value and record;
/// the service refuses it already ([`run_holds_token`]). Every other
/// principal's token is left as it is: a job's ends with its process.
pub fn run_token_ended(principal: &Principal, status: Option<RunStatus>) -> bool {
    matches!(principal.role, ActorRole::Worker | ActorRole::Wrapper)
        && principal.run_id.is_some()
        && !status.is_some_and(run_holds_token)
}

/// The supervisor could not keep the service running: the inbox's
/// attention (`reason`, `message`), next `dagq service status`. The
/// supervisor writes it from the outage's record ([`ServiceOutage`]) once
/// the service answers again, with the event that says so after it
/// (ADR-t3102-3 decision 1); one an older binary wrote while the service
/// was down stands until the service runs again (ADR-t1233-4 decision 3).
pub const QUEUE_SERVICE_DOWN: &str =
    crate::domain::event_kind::EventKind::QueueServiceDown.as_str();
/// The service answers again after a [`QUEUE_SERVICE_DOWN`].
pub const QUEUE_SERVICE_RUNNING: &str =
    crate::domain::event_kind::EventKind::QueueServiceRunning.as_str();
/// `up` or the supervisor started the service (`pid`, `build`, `by`,
/// `replaced`). `restart` is true once the starting supervisor has seen
/// the service running, whether a look found its build answering or one of
/// its own starts succeeded (the second start on); `up` and
/// `service start` always write false.
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
/// The record of an outage of the queue's service, beside the queue's DB
/// and outside it (ADR-t3102-3 decision 1): what the supervisor knows
/// while it reaches no service, and so writes nothing to the queue.
pub const OUTAGE_FILE: &str = "queue-service-outage.json";
/// Which outage of [`OUTAGE_FILE`] the host's `watch` of a role returned
/// already (its `since`), beside it in `<this>-<role>.json` (`none` for a
/// watch of no role), a file of each role so that watches of two roles
/// never write over each other's: the next watch of that role waits out
/// the same outage rather than return it again (ADR-t3102-3 decision 2).
pub const OUTAGE_TOLD_FILE: &str = "queue-service-outage-told";

/// What [`OUTAGE_FILE`] holds: since when (unix seconds) the supervisor
/// found the service down, why (`reason`: `unreachable`, `start_failed`
/// or `restart_limit`, and the `message`), the supervisor that found it,
/// and whether `[push]` was sent for it. One outage has one record: a
/// later look while it stands leaves it, and the service's return closes
/// it into a [`QUEUE_SERVICE_DOWN`] event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceOutage {
    pub since: i64,
    pub reason: String,
    pub message: String,
    #[serde(default)]
    pub supervisor: Option<String>,
    #[serde(default)]
    pub pushed: bool,
}

impl ServiceOutage {
    /// The payload of the `queue_service_down` the service's return
    /// records of it at `now`: its fields, and when it was closed.
    pub fn down_payload(&self, now: i64) -> Value {
        serde_json::json!({
            "reason": self.reason,
            "message": self.message,
            "since": self.since,
            "until": now,
            "supervisor": self.supervisor,
            "pushed": self.pushed,
        })
    }
}

/// The kinds whose latest says whether the attention stands.
pub const QUEUE_SERVICE_ATTENTION_KINDS: [&str; 4] = [
    QUEUE_SERVICE_DOWN,
    QUEUE_SERVICE_RUNNING,
    QUEUE_SERVICE_STARTED,
    QUEUE_SERVICE_STOPPED,
];

/// Whether the attention stands: the latest of
/// [`QUEUE_SERVICE_ATTENTION_KINDS`] is [`QUEUE_SERVICE_DOWN`].
pub fn attention_stands(latest: Option<&RecordedKind>) -> bool {
    latest.is_some_and(|kind| *kind == EventKind::QueueServiceDown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_too_large_answer_says_its_size_the_limit_and_how_to_narrow_its_read() {
        let events = too_large_message(Some(UseCase::Events), Some(20_000_000));
        assert!(events.contains("events is 20000000 bytes"), "{events}");
        assert!(events.contains(&MAX_ANSWER_BYTES.to_string()), "{events}");
        for option in ["--since / --until", "--limit", "--after <cursor>"] {
            assert!(events.contains(option), "{events}");
        }
        let read = too_large_message(Some(UseCase::Show), None);
        assert!(read.contains("show is longer than"), "{read}");
        assert!(read.contains("--full"), "{read}");
        let other = too_large_message(None, Some(1));
        assert!(other.contains("--help"), "{other}");
        let response = ServiceResponse::too_large(Some(UseCase::List), 1);
        assert_eq!(
            response.error.map(|error| error.code),
            Some(ServiceErrorCode::ResponseTooLarge)
        );
    }

    #[test]
    fn each_read_that_its_options_narrow_names_them_not_the_help() {
        let reads: [(UseCase, &[&str]); 26] = [
            (
                UseCase::Events,
                &["--run", "--task", "--goal", "--kind", "--full"],
            ),
            (
                UseCase::Notes,
                &["--limit", "--since <cursor>", "--task", "--goal"],
            ),
            (
                UseCase::List,
                &["--limit", "--before <next>", "--status", "--all"],
            ),
            (
                UseCase::Search,
                &["--limit", "--kind", "--status", "--goal"],
            ),
            (UseCase::Related, &["--limit", "--status"]),
            (UseCase::Stats, &["--since / --until", "--goal", "--full"]),
            (
                UseCase::Kpi,
                &["--since / --until", "--last", "--change", "--area"],
            ),
            (UseCase::Marks, &["--since / --until"]),
            (UseCase::Findings, &["--status", "--run", "--full / --all"]),
            (UseCase::Graph, &["--goal"]),
            (UseCase::Forecast, &["--task / --goal"]),
            (UseCase::Status, &["--role"]),
            (UseCase::Asks, &["--all", "--open / --role"]),
            (UseCase::Show, &["--full", "--events"]),
            (UseCase::Timeline, &["--full", "--gap"]),
            (UseCase::GoalShow, &["--full"]),
            (UseCase::GoalList, &["--tag"]),
            (UseCase::ProposalList, &["--all"]),
            (UseCase::Candidates, &["--ignore-deferrals"]),
            (UseCase::Lint, &["--proposal"]),
            (UseCase::ObserveHistory, &["--limit"]),
            (UseCase::ObserveInput, &["--section", "--offset", "--limit"]),
            (UseCase::AgentResults, &["--limit", "--agent"]),
            (UseCase::AgentResult, &["no option"]),
            (UseCase::AgentSets, &["--limit", "--agent"]),
            (UseCase::AgentSet, &["no option"]),
        ];
        for (use_case, options) in reads {
            let message = too_large_message(Some(use_case), None);
            assert!(!message.contains("--help"), "{message}");
            for option in options {
                assert!(message.contains(option), "{use_case:?}: {message}");
            }
        }
    }

    #[test]
    fn a_service_of_the_version_before_the_host_client_refuses_its_requests_and_this_one_answers_both()
     {
        // A service of version 1 answered that version only.
        let answered_before = |version: u32| (1..HOST_CLIENT_API_VERSION).contains(&version);
        let host = ServiceRequest {
            api_version: API_VERSION,
            token: None,
            env_role: None,
            queue: Some("/q/queue.db".into()),
            binding: None,
            use_case: UseCase::Note,
            params: json!({"task": 3, "text": "x"}),
        };
        assert!(host.api_version >= HOST_CLIENT_API_VERSION);
        assert!(!answered_before(host.api_version));
        // This service answers the host and a worker of version 1.
        assert!(answers(host.api_version));
        assert!(answers(1));
        // A host `dagq` uses no answer of a service of version 1.
        let before = ServiceResponse {
            api_version: 1,
            min_api_version: 1,
            ..ServiceResponse::failure(ServiceErrorCode::ApiVersionMismatch, "x")
        };
        assert!(!understands(&before));
    }

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
    fn a_refusal_of_the_policy_names_what_it_refused_and_reads_without_it() {
        use crate::domain::Capability;
        use crate::domain::authorization::DenyReason;
        let refused = AuthorizationError {
            role: ActorRole::Worker,
            capability: Capability::NoteWrite,
            reason: DenyReason::Resource,
        };
        let response = ServiceResponse::denied(&refused);
        let written = serde_json::to_value(&response).unwrap();
        assert_eq!(
            written["error"],
            json!({"code": "authorization_denied",
                   "message": "worker may not note.write (not on this resource)",
                   "denied": {"role": "worker", "capability": "note.write",
                              "reason": "not on this resource"}})
        );
        assert_eq!(
            serde_json::from_value::<ServiceResponse>(written.clone()).unwrap(),
            response
        );
        // A client that knows no `denied` reads the refusal by its code.
        #[derive(Deserialize)]
        struct Older {
            code: ServiceErrorCode,
            message: String,
        }
        let older: Older = serde_json::from_value(written["error"].clone()).unwrap();
        assert_eq!(older.code, ServiceErrorCode::AuthorizationDenied);
        assert_eq!(older.message, refused.to_string());
        // A role or a capability this binary does not know still reads.
        let newer: ServiceError = serde_json::from_value(json!({
            "code": "authorization_denied", "message": "m",
            "denied": {"role": "future-job", "capability": "future.write", "reason": "r"},
        }))
        .unwrap();
        assert_eq!(newer.denied.unwrap().role, "future-job");
    }

    #[test]
    fn a_broker_url_names_only_a_host_and_a_port() {
        for (url, address) in [
            ("http://127.0.0.1:47310", "127.0.0.1:47310"),
            ("http://127.0.0.1:47310/", "127.0.0.1:47310"),
            (" http://localhost:1 ", "localhost:1"),
        ] {
            assert_eq!(broker_address(url).as_deref(), Ok(address), "{url}");
        }
        for url in [
            "",
            "127.0.0.1:1",
            "https://127.0.0.1:1",
            "http://127.0.0.1",
            "http://127.0.0.1:",
            "http://:1",
            "http://127.0.0.1:99999",
            "http://127.0.0.1:1/queue",
            "http://u@127.0.0.1:1",
            "http://127.0.0.1:1?x",
        ] {
            let refused = broker_address(url).unwrap_err();
            assert!(refused.contains(URL_ENV), "{refused}");
        }
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

    /// ADR-t1662-3 decisions 9, 10 and 13: the executor's telemetry
    /// principal is the one role of the control side a token is issued
    /// for, with its actor and run its one target's; the wrapper, the
    /// integrator, the supervisor and the user are refused as before, and
    /// so is a target on any other role or one its actor does not match.
    #[test]
    fn a_telemetry_principal_is_issued_for_one_target_and_no_other_control_role_is() {
        use crate::domain::session_step::TargetKind;
        let run = RunId::new("r1").unwrap();
        let of_run = Principal::telemetry(&StepTarget::run(&run), Some(TaskId::new(4)));
        assert!(of_run.check().is_ok());
        assert_eq!(of_run.role, ActorRole::ExecutorTelemetry);
        assert_eq!(of_run.actor().run_id(), Some(&run));
        assert_eq!(of_run.actor().task_id(), Some(TaskId::new(4)));
        assert_eq!(of_run.actor().actor_id(), "executor-telemetry:run:r1");
        assert_eq!(
            serde_json::to_value(&of_run).unwrap(),
            json!({"role": "executor-telemetry", "actor_id": "executor-telemetry:run:r1",
                   "run_id": "r1", "task_id": 4, "target": {"kind": "run", "id": "r1"}})
        );
        let planner = StepTarget {
            kind: TargetKind::Planner,
            id: "3".into(),
        };
        let of_planner = Principal::telemetry(&planner, Some(TaskId::new(4)));
        assert!(of_planner.check().is_ok());
        assert_eq!(
            (of_planner.run_id.clone(), of_planner.task_id),
            (None, None)
        );
        assert_eq!(of_planner.actor().run_id(), None);
        for role in [
            ActorRole::User,
            ActorRole::Supervisor,
            ActorRole::Wrapper,
            ActorRole::Integrator,
            ActorRole::ExecutorTelemetry,
        ] {
            assert!(
                Principal::of(&ActorContext::new(role, "x"))
                    .check()
                    .is_err(),
                "{role:?}"
            );
        }
        // A target on an agent or another control role, and a target its
        // actor or run does not match, are no principal.
        let mut on_worker = Principal::worker(&run, TaskId::new(4));
        on_worker.target = Some(StepTarget::run(&run));
        assert!(on_worker.check().is_err());
        for role in [
            ActorRole::Wrapper,
            ActorRole::Integrator,
            ActorRole::Supervisor,
            ActorRole::User,
        ] {
            let mut on_role = of_run.clone();
            on_role.role = role;
            assert!(on_role.check().is_err(), "{role:?}");
        }
        let mut another_actor = of_run.clone();
        another_actor.actor_id = "executor-telemetry:run:r2".into();
        assert!(another_actor.check().is_err());
        let mut another_run = of_run.clone();
        another_run.run_id = Some(RunId::new("r2").unwrap());
        assert!(another_run.check().is_err());
        // A run target whose id is no run's names no run its token ends
        // with, and the telemetry role without a target is no principal.
        let bad_run = StepTarget {
            kind: TargetKind::Run,
            id: " ".into(),
        };
        assert!(Principal::telemetry(&bad_run, None).check().is_err());
        let mut no_target = of_run.clone();
        no_target.target = None;
        assert!(no_target.check().is_err());
        let mut planner_with_task = of_planner;
        planner_with_task.task_id = Some(TaskId::new(4));
        assert!(planner_with_task.check().is_err());
        // An older record, without a target, reads as before.
        let read: Principal =
            serde_json::from_value(json!({"role": "worker", "actor_id": "worker:r1"})).unwrap();
        assert_eq!(read.target, None);
        assert_eq!(UseCase::TelemetryReport.as_str(), "telemetry_report");
        assert_eq!(UseCase::of_command(&["telemetry_report"]), None);
    }

    /// ADR-t3102-1 decisions 3 and 4: the session wrapper's principal is
    /// issued for one run (with its task) or one planner, its actor the
    /// wrapper's as its events record it; any other target, or an actor,
    /// run or task its target does not match, is no principal.
    #[test]
    fn a_wrapper_principal_is_issued_for_one_run_or_one_planner() {
        use crate::domain::session_step::TargetKind;
        let run = RunId::new("r1").unwrap();
        let of_run = Principal::wrapper(&StepTarget::run(&run), Some(TaskId::new(4)));
        assert!(of_run.check().is_ok());
        assert_eq!(
            serde_json::to_value(&of_run).unwrap(),
            json!({"role": "wrapper", "actor_id": "wrapper:r1", "run_id": "r1", "task_id": 4,
                   "target": {"kind": "run", "id": "r1"}})
        );
        let actor = of_run.actor();
        assert_eq!(actor.role(), ActorRole::Wrapper);
        assert_eq!(actor.run_id(), Some(&run));
        assert_eq!(
            actor.actor_id(),
            ActorContext::instance(ActorRole::Wrapper, &run).actor_id()
        );
        let planner = StepTarget::of(TargetKind::Planner, 3);
        let of_planner = Principal::wrapper(&planner, Some(TaskId::new(4)));
        assert!(of_planner.check().is_ok());
        assert_eq!(of_planner.actor_id, "wrapper:planner:3");
        assert_eq!(
            (of_planner.run_id.clone(), of_planner.task_id),
            (None, None)
        );
        // A run's wrapper without its task, a target of another kind, and
        // an actor, a run or a task its target does not match.
        let mut no_task = of_run.clone();
        no_task.task_id = None;
        assert!(no_task.check().is_err());
        let review = Principal::wrapper(&StepTarget::of(TargetKind::PlanReview, 3), None);
        assert!(review.check().is_err());
        let mut another_actor = of_run.clone();
        another_actor.actor_id = "wrapper:r2".into();
        assert!(another_actor.check().is_err());
        let mut another_run = of_run.clone();
        another_run.run_id = Some(RunId::new("r2").unwrap());
        assert!(another_run.check().is_err());
        let mut planner_with_task = of_planner.clone();
        planner_with_task.task_id = Some(TaskId::new(4));
        assert!(planner_with_task.check().is_err());
        // The wrapper's role on a telemetry principal's target, and a
        // worker on the wrapper's, are no principal either.
        let mut as_worker = of_run;
        as_worker.role = ActorRole::Worker;
        assert!(as_worker.check().is_err());
        for use_case in [UseCase::SessionRun, UseCase::SessionRecord] {
            assert_eq!(UseCase::of_command(&[use_case.as_str()]), None);
        }
        assert_eq!(UseCase::of_command(&["session", "--run", "r1"]), None);
        assert_eq!(UseCase::of_command(&["session-event", "open"]), None);
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
        // A worker's token goes with its run's end or absence, never while
        // the run waits for its resume; no other principal's goes so.
        let run = RunId::new("r1").unwrap();
        let worker = Principal::worker(&run, TaskId::new(4));
        assert!(!run_token_ended(&worker, Some(RunStatus::NeedsSession)));
        assert!(!run_token_ended(&worker, Some(RunStatus::Running)));
        assert!(run_token_ended(&worker, Some(RunStatus::Integrated)));
        assert!(run_token_ended(&worker, Some(RunStatus::Failed)));
        assert!(run_token_ended(&worker, None));
        let job = Principal::of(&ActorContext::review_job(&run, 1));
        assert!(!run_token_ended(&job, None));
        let telemetry = Principal::telemetry(&StepTarget::run(&run), Some(TaskId::new(4)));
        assert!(!run_token_ended(&telemetry, Some(RunStatus::Failed)));
        // A run's wrapper's goes with its run as its worker's does; a
        // planner's wrapper's is not a run's.
        let wrapper = Principal::wrapper(&StepTarget::run(&run), Some(TaskId::new(4)));
        assert!(run_token_ended(&wrapper, Some(RunStatus::Integrated)));
        assert!(!run_token_ended(&wrapper, Some(RunStatus::NeedsSession)));
        let planner = Principal::wrapper(
            &StepTarget::of(crate::domain::session_step::TargetKind::Planner, 3),
            None,
        );
        assert!(!run_token_ended(&planner, None));
        assert!(attention_stands(Some(&EventKind::QueueServiceDown.into())));
        assert!(!attention_stands(Some(
            &EventKind::QueueServiceRunning.into()
        )));
        assert!(!attention_stands(None));
    }

    /// ADR-t3102-1 decision 2: a planner's token ends as its planner
    /// closes, its wrapper's as its agent exits too; one whose planner does
    /// not read is kept, and no other principal's is judged so.
    #[test]
    fn a_planner_s_token_ends_as_its_planner_closes_and_its_wrapper_s_as_it_exits() {
        use super::super::session_step::TargetKind;
        use super::super::{PlannerOrigin, PlannerRoute, planner::PlannerSession};
        let session = |exited_at, closed_at| PlannerSession {
            id: super::super::PlannerId::new(7),
            origin: PlannerOrigin::Runtime,
            proposal_id: None,
            draft_task_id: None,
            finding_id: None,
            request_id: None,
            workspace_id: None,
            wrapper_pid: None,
            agent_pid: None,
            heartbeat_at: None,
            exit_code: None,
            exited_at,
            closed_at,
            error: None,
            created_at: 1,
            route: PlannerRoute::Headless,
            answer_wait_at: None,
        };
        let target = StepTarget::of(TargetKind::Planner, 7);
        let wrapper = Principal::wrapper(&target, None);
        let planner = Principal::planner(7);
        let ended = |principal: &Principal, exited, closed| {
            planner_token_ended(principal, |id| {
                assert_eq!(id.as_i64(), 7);
                Some(session(exited, closed))
            })
        };
        for principal in [&planner, &wrapper] {
            assert_eq!(ended(principal, None, None), Some(false));
            assert_eq!(ended(principal, Some(2), Some(3)), Some(true));
            assert_eq!(planner_token_ended(principal, |_| None), Some(false));
        }
        assert_eq!(ended(&planner, Some(2), None), Some(false));
        assert_eq!(ended(&wrapper, Some(2), None), Some(true));
        for other in [
            Principal::telemetry(&target, None),
            Principal::worker(&RunId::new("r1").unwrap(), TaskId::new(1)),
            Principal::wrapper(&StepTarget::run(&RunId::new("r1").unwrap()), None),
        ] {
            assert_eq!(ended(&other, None, Some(3)), None, "{other:?}");
        }
    }

    /// The sweep revokes an ended token by its key: an inbox's by its
    /// session's, so an inbox opened again keeps its own; a planner's and
    /// a worker's by their actor ids; never a supervisor's sign-in.
    #[test]
    fn the_sweep_revokes_by_the_key_of_the_session_and_never_a_supervisor_s() {
        let session = |pid| SessionProcess {
            pid,
            start: "Thu Oct 23 10:00:00 2026".to_owned(),
        };
        let (ended, again) = (Principal::inbox(session(7)), Principal::inbox(session(8)));
        assert_eq!(
            swept_token_key(&ended).as_deref(),
            Some("inbox@7:Thu Oct 23 10:00:00 2026")
        );
        assert_ne!(swept_token_key(&ended), swept_token_key(&again));
        assert_eq!(swept_token_key(&ended), Some(ended.credential_key()));
        let worker = Principal::worker(&RunId::new("r1").unwrap(), TaskId::new(1));
        for principal in [Principal::planner(3), worker] {
            assert_eq!(
                swept_token_key(&principal),
                Some(principal.actor_id.clone()),
                "{principal:?}"
            );
        }
        let supervisor = Principal::supervisor(&LeaseToken::new("lease"), 42);
        let named = Principal {
            actor_id: supervisor.actor_id.clone(),
            ..Principal::inbox(session(7))
        };
        for refused in [supervisor.clone(), named] {
            assert_eq!(swept_token_key(&refused), None, "{refused:?}");
        }
    }

    #[test]
    fn the_inbox_of_its_session_and_a_planner_of_its_planner_take_tokens_and_the_user_none() {
        let inbox = ActorContext::new(ActorRole::Inbox, "inbox");
        let planner = ActorContext::instance(ActorRole::Planner, 7);
        let session = SessionProcess {
            pid: 42,
            start: "Thu Oct 23 10:00:00 2026".to_owned(),
        };
        let of_session = Principal::inbox(session.clone());
        assert_eq!(of_session.actor(), inbox);
        let of_planner = Principal::host_client(&planner).unwrap();
        assert_eq!(of_planner, Principal::planner(7));
        assert_eq!(of_planner.actor(), planner);
        for principal in [of_session.clone(), of_planner] {
            assert_eq!(principal.check(), Ok(()), "{principal:?}");
        }
        // The user takes no token, nor an inbox of no session, a session of
        // another role, a planner of no planner or of another one; the
        // inbox's is not issued at a turn's start.
        assert_eq!(Principal::host_client(&ActorContext::user()), None);
        assert_eq!(Principal::host_client(&inbox), None);
        assert_eq!(
            Principal::host_client(&ActorContext::new(ActorRole::Planner, "planner")),
            None
        );
        let other = Principal {
            actor_id: "planner:8".to_owned(),
            ..Principal::planner(7)
        };
        let worker_of_session = Principal {
            session: Some(session.clone()),
            ..Principal::worker(&RunId::new("r1").unwrap(), TaskId::new(1))
        };
        for refused in [
            Principal::user(),
            Principal::of(&inbox),
            Principal::of(&planner),
            other,
            worker_of_session,
        ] {
            assert!(refused.check().is_err(), "{refused:?}");
        }
        // An inbox's token ends with its session; no other is judged so.
        assert!(!session_token_ended(&of_session, |s| *s == session));
        assert!(session_token_ended(&of_session, |_| false));
        assert!(!session_token_ended(&Principal::planner(7), |_| false));
        for role in [ActorRole::User, ActorRole::Inbox, ActorRole::Planner] {
            assert!(host_client_role(role) && !client_role(role), "{role:?}");
        }
        assert!(!host_client_role(ActorRole::Worker));
        assert!(!host_client_role(ActorRole::Supervisor));
        // A request names the role and the queue only when given, which an
        // older service never sees.
        let request = ServiceRequest {
            api_version: API_VERSION,
            token: None,
            env_role: None,
            queue: None,
            binding: None,
            use_case: UseCase::Show,
            params: json!({"id": 1}),
        };
        let sent = serde_json::to_string(&request).unwrap();
        assert!(
            !sent.contains("env_role") && !sent.contains("queue"),
            "{sent}"
        );
        // The next step a tokenless inbox or planner is told.
        assert!(missing_token_next("inbox").unwrap().contains("dagq inbox"));
        assert!(missing_token_next("planner").unwrap().contains("close"));
        assert_eq!(missing_token_next("worker"), None);
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
            (&["agent", "sets", "--agent", "adr-rules"], "agent_sets"),
            (&["agent", "set", "3"], "agent_set"),
            (&["agent", "eval", "adr-rules", "--set", "3"], "agent_eval"),
            (&["requests", "--all"], "requests"),
            (&["replans", "3"], "replans"),
            (&["ci", "failures", "--task", "4"], "ci_failures"),
            (&["planners", "--all"], "planners"),
            (&["add", "x", "--goal", "3"], "add"),
            (&["edit", "3", "--title", "x"], "edit"),
            (&["submit", "3"], "submit"),
            (&["cancel", "3"], "cancel"),
            (&["dependency", "add", "3", "2"], "dependency_add"),
            (
                &["dependency", "remove", "3", "--goal", "2"],
                "dependency_remove",
            ),
            (&["set-goal", "3", "--none"], "set_goal"),
            (&["set-paths", "3", "--none"], "set_paths"),
            (&["set-priority", "3", "high"], "set_priority"),
            (&["revisit", "3", "--clear"], "revisit"),
            (&["judge-follow-up", "3"], "judge_follow_up"),
            (&["draft", "3"], "draft"),
            (&["ready", "3", "--bypass-review"], "ready"),
            (&["goal", "add", "x"], "goal_add"),
            (&["goal", "edit", "3"], "goal_edit"),
            (&["goal", "close", "3"], "goal_close"),
            (&["goal", "ready", "3"], "goal_ready"),
            (&["goal", "review", "3"], "goal_review"),
            (&["proposal", "withdraw", "1"], "proposal_withdraw"),
            (&["request", "add", "--text", "x"], "request_add"),
            (&["request", "decline", "3"], "request_decline"),
            (&["mark", "x"], "mark"),
            (&["answer", "3", "yes"], "answer"),
            (&["ask", "close", "3"], "ask_close"),
            (&["hold", "4", "--reason", "x"], "hold"),
            (&["release", "4"], "release"),
            (&["replan", "request", "r1"], "replan_request"),
            (&["rebind"], "rebind"),
            (&["recover", "r1"], "recover"),
            (&["run", "screen", "r1"], "run_screen"),
            (&["run", "send", "r1"], "run_send"),
            (&["run", "log", "r1", "--follow"], "run_log"),
            (&["planner", "screen", "2"], "planner_screen"),
            (&["planner", "send", "2"], "planner_send"),
            (&["planner", "log", "2"], "planner_log"),
            (
                &["planner", "request", "2", "--text", "x"],
                "planner_request",
            ),
            (&["report", "--period", "week"], "report"),
            (
                &["graph", "--format", "svg", "--out", "g.svg"],
                "graph_export",
            ),
            (&["graph", "--out=g.d2"], "graph_export"),
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
            &["locate"],
            &["doctor"],
            &["observe"],
            &["observe", "--daily"],
            &["run", "close-workspaces"],
            &["goal"],
            &["finding"],
            &["ready", "3", "--inherit", "--reason", "x"],
            &["dependency"],
            &["request"],
            &["integrate"],
        ] {
            assert_eq!(UseCase::of_command(words), None, "{words:?}");
        }
    }
}
