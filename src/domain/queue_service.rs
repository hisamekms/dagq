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

use crate::domain::{EventId, EventKind, RecordedKind};
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
/// Who starts the queue's service, which decides the migrations it applies
/// before it accepts (ADR-t3102-2 decision 2): only a service a replacement
/// starts applies any, and a service opened by hand or started again by the
/// supervisor's watch applies none (ADR-0073 decision 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStart {
    /// `up`, `install`, the automatic update or the release update starting
    /// the service of the build it puts in place; `breaking` after the
    /// drain of an `install --allow-breaking`.
    Replacement { breaking: bool },
    /// `dagq service start`.
    ByHand,
    /// The supervisor's watch, starting a service that stopped again.
    Restart,
}

/// The migrations of its build a service applies before it accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartMigrations {
    None,
    /// Those every one of which is compatible; a breaking one refuses the
    /// start.
    Compatible,
    /// Every one, a breaking one only on a queue no supervisor, run or
    /// wrapper uses.
    Breaking,
}

impl ServiceStart {
    pub fn migrations(self) -> StartMigrations {
        match self {
            Self::Replacement { breaking: false } => StartMigrations::Compatible,
            Self::Replacement { breaking: true } => StartMigrations::Breaking,
            Self::ByHand | Self::Restart => StartMigrations::None,
        }
    }
}

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
/// The variable that names, to a job command a supervisor starts as its
/// own (`observe`, `throughput-review`), the registration of that
/// supervisor (ADR-t3102-3 decision 3): with it the command opens no queue
/// and reaches it through the service's socket as that supervisor's
/// principal, with the token the supervisor signed in with, found from the
/// registration ([`Principal::credential_key`]); the value is not a token.
/// A supervisor whose service runs in its own process (a test's) leaves
/// it out, and its commands open the queue.
pub const SUPERVISOR_REGISTRATION_ENV: &str = "DAGQ_SUPERVISOR_REGISTRATION";
/// The variables of a client-mode `dagq`.
pub const CLIENT_ENV: [&str; 3] = [SOCKET_ENV, URL_ENV, CREDENTIAL_FILE_ENV];
/// The longest request line the service reads: a client on its socket
/// refuses a longer one before it sends it, with
/// [`ServiceErrorCode::RequestTooLarge`]; one through the queue broker
/// does not check it before it sends.
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

/// The message of a [`ServiceErrorCode::RequestTooLarge`] of `use_case`:
/// the request's `size` and [`MAX_REQUEST_BYTES`].
pub fn request_too_large_message(use_case: UseCase, size: usize) -> String {
    format!(
        "the request of {} is {size} bytes, past the {MAX_REQUEST_BYTES} bytes the queue service \
         reads, and is not sent",
        use_case.as_str()
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
        UseCase::ProposalShow
        | UseCase::AgentResult
        | UseCase::AgentSet
        | UseCase::ServiceStatus
        | UseCase::StatsInputs => "its command has no option that narrows the answer",
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
        | UseCase::ReadyInherit
        | UseCase::ReadyInheritRead
        | UseCase::ReadyInheritImages
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
        | UseCase::Control
        | UseCase::SessionRun
        | UseCase::SessionRecord
        | UseCase::DenialRecord
        | UseCase::IssueToken
        | UseCase::RevokeToken
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
        UseCase::BrokerRunStatus | UseCase::BrokerBinding | UseCase::BrokerRecord => {
            "its answer is of one run or one record, and nothing narrows it"
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
    // The part of `dagq service status` the queue holds, whether the
    // inbox's attention of the service's outage stands, read through the
    // service while it answers; the host's `dagq` adds what it finds of the
    // service outside the DB.
    ServiceStatus => "service_status",
    // What `dagq stats` and `dagq kpi` ask for first and read Git with
    // themselves (ADR-t3102-3 decision 5): the repository the queue is
    // bound to, the landed commits and how far back main's history is
    // read. No command line goes to it alone.
    StatsInputs => "stats_inputs",
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
    // A step of a control-side command a person or the inbox runs on the
    // host (`integrate`, `review`, `observe`, `throughput-review`,
    // `setup-review`): one of the queue's operations as a supervisor's
    // ([`ControlParams`]), each in one transaction, for the command's
    // principal once the command's operation is authorized for it, and
    // only the operations that command makes. The landing's slot is taken
    // with the integrator's token issued in the same transaction, and the
    // landing goes on with that token as a supervisor's does. Git and the
    // programs stay with the client (ADR-t3102-3 decision 5). So does
    // `up`, `down`, `dagq inbox` and `service start|stop` of the user, the
    // inbox, a planner or a supervisor (`service.lifecycle`): launchd, the
    // processes and the service's own process stay with the client, and
    // only the registrations, the events and the reads they make are
    // steps ([`ControlCommand::lifecycle`]).
    Control => "control",
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
    // The record of a refusal the host's `dagq` made itself before it ran
    // a command the service does not answer, as `authorization_denied` of
    // the caller's principal: only of its own role, never another actor's.
    // The command line sends it on its own refusal; no command goes to it.
    DenialRecord => "denial_record",
    // The issue of the token of a principal the caller starts
    // (ADR-t3102-1 decision 2): the user's of a supervisor or of the
    // inbox, a command's of a job. The parent is the caller's principal, which [`super::authorization::starts`]
    // judges with `token.issue`, and what the token is issued under
    // ([`IssueHeld`]) is checked in the transaction that issues it; the
    // answer is the file its value is in. The supervisor asks for its
    // children's as the `issue_token` operation of [`Self::Supervisor`],
    // which issues them the same way. No command line goes to it.
    IssueToken => "issue_token",
    // The revocation of a supervisor's token once its registration ended
    // (ADR-t3102-1 decision 2), at the request of `up` or `down` after they
    // pruned it, for a caller that may run them (`service.lifecycle`),
    // judged in the transaction that revokes it
    // ([`registration_end_refusal`]). The answer is whether one was
    // revoked. No command line goes to it.
    RevokeToken => "revoke_token",
    // The planning commands a person, the inbox or a planner runs
    // (ADR-t3102-3 decision 3), each as the command of its name changes
    // the queue and prints it, authorized by the application for the
    // caller ([`crate::application::queue_writes`]): `ready` without
    // `--inherit`, which is [`Self::ReadyInherit`].
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
    // A person's `ready --inherit`, recorded in one transaction once its
    // client chose the head and wrote it under `refs/dagq/runs/<run-id>`
    // with its own Git (ADR-t3294-1); the client puts the ref back when the
    // record is refused.
    ReadyInherit => "ready_inherit",
    // What `dagq ready --inherit` asks for first: the task's status and its
    // latest run, which the client chooses the head of and which the
    // record checks again. No command line goes to it alone.
    ReadyInheritRead => "ready_inherit_read",
    // The records of the image the client of `ready --inherit` made ready
    // for a run's facts-only turn as it chose the head. No command line
    // goes to it alone.
    ReadyInheritImages => "ready_inherit_images",
    // The queue broker's own requests (docs/design/queue-broker.md): the
    // status of the run a token names, the repository the queue is bound
    // to, whose `dagq.toml` names the registries a tunnel may reach, and
    // the record of a refusal or of a sweep of the verifications' tokens.
    // The broker sends them on the host's socket with no token and no
    // actor's environment, as the user (ADR-t3102-1 decision 1), and opens
    // no queue; the record is only `queue_broker_denied` and
    // `queue_broker_tokens_swept`, and only the user's and a supervisor's
    // (`queue_broker.record`). No command line goes to them.
    BrokerRunStatus => "broker_run_status",
    BrokerBinding => "broker_binding",
    BrokerRecord => "broker_record",
});

impl UseCase {
    /// The use case a `dagq` command line goes to through the service:
    /// `words` are its arguments after the global options, the subcommand
    /// first; a control-side command that sends its steps as
    /// [`Self::Control`] goes to that, and so do `up`, `down`, `dagq inbox`
    /// and `service start|stop`. `None` for a command the service does
    /// not answer (the rest of the control side's, `watch`, whose looks
    /// only the host's `dagq` sends as [`Self::WatchPoll`], `locate`,
    /// `doctor`, ...).
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
            ("ready", _) if has("--inherit") => Self::ReadyInherit,
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
            ("integrate" | "review" | "observe" | "throughput-review" | "setup-review", _)
            | ("up" | "down" | "inbox", _)
            | ("service", Some("start" | "stop")) => Self::Control,
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
    /// `request_too_large`, only a client's on the service's socket: the
    /// request is longer than [`MAX_REQUEST_BYTES`], so it is not sent
    /// ([`request_too_large_message`]).
    RequestTooLarge => "request_too_large",
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
    /// The landing of a supervisor asked to hand off was not begun
    /// ([`crate::application::HandoffAsked`]).
    HandoffAsked(Box<HandoffRefusal>),
}

/// The supervisor whose landing a handoff it was asked refused, and the
/// binary it is asked to hand off to ([`FailureKind::HandoffAsked`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffRefusal {
    pub supervisor: LeaseToken,
    pub binary: String,
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
    /// The identity of a result's record (ADR-t3102-3 decision 5), given
    /// at its first sending and kept by every sending of it, after a
    /// restart too: the queue takes an event once per identity, and two
    /// results alike are two records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<String>,
}

// The control side's commands a person or the inbox runs on the host
// ([`UseCase::Control`]): the use case each goes to through the service,
// whose operations of the queue the command sends by their names.
string_enum!(ControlCommand {
    Integrate => "integrate",
    Review => "review",
    Observe => "observe",
    ThroughputReview => "throughput_review",
    SetupReview => "setup_review",
    Up => "up",
    Down => "down",
    Inbox => "inbox",
    ServiceStart => "service_start",
    ServiceStop => "service_stop",
    BrokerStart => "broker_start",
    BrokerStop => "broker_stop",
});

impl ControlCommand {
    /// Whether the command starts, stops or opens what serves the queue
    /// (`up`, `down`, `dagq inbox`, `service start|stop`, `queue-broker
    /// start|stop`): its operation is
    /// `service.lifecycle`, and its steps act on any supervisor's
    /// registration, as the command line on the DB did, rather than under
    /// a lease of its own.
    pub const fn lifecycle(self) -> bool {
        matches!(
            self,
            Self::Up
                | Self::Down
                | Self::Inbox
                | Self::ServiceStart
                | Self::ServiceStop
                | Self::BrokerStart
                | Self::BrokerStop
        )
    }
}

/// The command a request of [`UseCase::Control`] is a step of, as its
/// command line names it: the task `integrate ID` and `review ID` name
/// (`integrate --next` names none), `setup-review --yes`, and the process
/// of the command, whose landing's integrator is `integrator:<pid>` as the
/// command line's on the DB is. The service authorizes the command's
/// operation for the principal at each step, as the command line authorizes
/// it before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlCall {
    pub command: ControlCommand,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskId>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub yes: bool,
    pub pid: u32,
}

impl ControlCall {
    /// The command's process as the connection `connection` (the one the
    /// OS names, `None` when it cannot be read) confirms it: its `pid` only
    /// when the step comes from that process, so a step that names another
    /// command's process asks as no process (ADR-t3102-1 decision 2: the
    /// parent that started a job is its process).
    pub fn process(&self, connection: Option<u32>) -> Option<u32> {
        connection.filter(|pid| *pid == self.pid)
    }
}

/// A step of a control-side command ([`UseCase::Control`]): the command
/// and one operation of the queue, named and given as a supervisor's is
/// ([`SupervisorParams`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlParams {
    pub control: ControlCall,
    pub call: SupervisorParams,
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
    /// The `integration_started` event of the landing an integrator
    /// principal's token was issued for, in the transaction that took its
    /// slot ([`Self::of_landing`]): the service takes the integrator's
    /// writes only while the queue holds that event, so a token whose
    /// landing never committed writes nothing. `None` for every other
    /// principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing: Option<EventId>,
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

impl SessionProcess {
    /// Whether this process still runs, `alive` saying whether its pid does
    /// and `start` the start the system prints for it (`None` when that
    /// cannot be read): its pid lives and prints the same start. A token
    /// bound to the session is taken only then.
    pub fn runs(&self, alive: bool, start: Option<&str>) -> bool {
        alive && start == Some(self.start.as_str())
    }

    /// Whether this process is known to have ended, `alive` and `start` as
    /// [`Self::runs`] takes them: its pid is gone, or another process took
    /// it (another start is printed). A live pid whose start cannot be read
    /// has not, since what ended lets another parent take its tokens over
    /// or revoke them.
    pub fn ended(&self, alive: bool, start: Option<&str>) -> bool {
        !alive || start.is_some_and(|start| start != self.start)
    }
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
            landing: None,
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

    /// The integrator of `run`'s landing, at the request of the supervisor
    /// of process `pid` holding its integration slot under `lease`
    /// (ADR-t3102-1 decision 2): its actor is the one the landing's events
    /// record (`integrator:<pid>`), its one target the run, and its
    /// registration the lease, which every lease it names has to be. The
    /// run is its target, not its `run_id`, so the landing still records
    /// as it after the run is integrated; the token is revoked as the
    /// landing leaves `integrating` ([`Self::credential_key`]).
    pub fn integrator(run: &RunId, lease: &LeaseToken, pid: u32) -> Self {
        Self {
            target: Some(StepTarget::run(run)),
            registration: Some(lease.clone()),
            ..Self::of(&ActorContext::instance(ActorRole::Integrator, pid))
        }
    }

    /// This integrator principal as the one of the landing whose
    /// `integration_started` is `event` ([`Self::landing`]).
    pub fn of_landing(self, event: EventId) -> Self {
        Self {
            landing: Some(event),
            ..self
        }
    }

    /// The run an integrator principal lands ([`Self::integrator`]).
    pub fn landing_run(&self) -> Option<RunId> {
        match (self.role, &self.target) {
            (ActorRole::Integrator, Some(target)) => target.run_id(),
            _ => None,
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
            landing: None,
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
            landing: None,
        }
    }

    /// The principals the control side issues tokens for: the AI actors
    /// (ADR-t1233-4 decision 4), an executor's telemetry principal whose
    /// actor id and run are its one target's (ADR-t1662-3 decisions 9 and
    /// 13), a supervisor, whose one target is its registration
    /// (ADR-t3102-1 decision 3), the session wrapper of one run (with its
    /// task) or one planner, the inbox of one session, and a planner whose
    /// one target is the planner its actor id names (ADR-t3102-1 decision
    /// 3), and the integrator of one run's landing, whose one target is
    /// the run and whose registration is the slot's lease ([`Self::integrator`]).
    /// No other principal has a target, a registration or a session. The
    /// user takes no token (decision 1).
    pub fn check(&self) -> Result<(), DomainError> {
        let refuse = |value: String| {
            Err(DomainError::UnknownValue {
                kind: "Principal role",
                value,
            })
        };
        if let Some(registration) = &self.registration {
            let target = match (self.role, &self.target) {
                (ActorRole::Supervisor, None) => true,
                (ActorRole::Integrator, Some(target)) => {
                    target.kind == super::session_step::TargetKind::Run && target.run_id().is_some()
                }
                _ => false,
            };
            return if target
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

    /// Whether its actor id is the one its role and run name, as the
    /// actors' constructors make it: `<role>:<instance>`, a worker's
    /// `<role>:<run>`, a principal's of another run `<role>:<run>:...`,
    /// the inbox's `inbox`, and one of a target its target's. An issue of
    /// one that names another (an observer named `planner:5`) would revoke
    /// and take the file of that other principal's token, so the issue use
    /// case refuses it.
    pub fn names_itself(&self) -> bool {
        // One of a target has the actor id of its target ([`Self::check`]).
        if self.target.is_some() {
            return true;
        }
        let role = self.role.as_str();
        let Some(instance) = self
            .actor_id
            .strip_prefix(role)
            .and_then(|rest| rest.strip_prefix(':'))
        else {
            return self.role == ActorRole::Inbox && self.actor_id == role;
        };
        match (self.role, &self.run_id) {
            (ActorRole::Inbox, _) => false,
            (ActorRole::Worker, Some(run)) => instance == run.as_str(),
            (ActorRole::Worker, None) => false,
            (_, Some(run)) => instance
                .strip_prefix(run.as_str())
                .is_some_and(|rest| rest.starts_with(':')),
            (_, None) => !instance.is_empty(),
        }
    }

    /// The name its token is kept under, which a new issue for it
    /// replaces: its actor id, and a supervisor's registration with it, as
    /// one process may run more than one supervisor (a test's does) under
    /// the one actor id its events record, or an inbox's session with it,
    /// so that the file a session was handed never holds the token of a
    /// session opened after it (ADR-t3102-1 decision 2), or a landing
    /// integrator's run with it, as one supervisor may land more than one.
    pub fn credential_key(&self) -> String {
        if let Some(run) = self.landing_run() {
            return format!("{}@{run}", self.actor_id);
        }
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

/// What the issue of a token is checked under, in the transaction that
/// issues it (ADR-t3102-1 decision 2): what its parent holds of its
/// target, as [`issue_held`] reads it from the two principals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueHeld {
    /// Nothing of the queue: the inbox of a session the user opens, or a
    /// job a command of the user's or the inbox's starts.
    Nothing,
    /// A supervisor's own registration, which it registers, or resumes after
    /// the exec of a handoff (its heartbeat expired meanwhile or not), with
    /// the token next: nothing of the queue is held yet, and every lease and
    /// child's token it asks for after is checked against the registration.
    Registration(LeaseToken),
    /// The run leased to the supervisor's registration, and that
    /// registration live: a worker's and its wrapper's.
    Lease { run: RunId, lease: LeaseToken },
    /// The planner open and the supervisor's registration live: a
    /// planner's and its wrapper's.
    OpenPlanner { planner: i64, lease: LeaseToken },
    /// The supervisor's registration live: a job's, on a run or not.
    Registered(LeaseToken),
    /// The target of no run not ended, and the registration of the
    /// supervisor that asked live when it is one: the telemetry of the
    /// executor of a job a command starts.
    OpenTarget {
        target: StepTarget,
        registration: Option<LeaseToken>,
    },
}

/// What the token of `child` asked for by `parent` is issued under
/// ([`IssueHeld`]), once [`super::authorization::starts`] allowed it.
pub fn issue_held(parent: &Principal, child: &Principal) -> IssueHeld {
    let planner = child
        .target
        .as_ref()
        .filter(|target| target.kind == super::session_step::TargetKind::Planner)
        .and_then(|target| target.id.parse().ok());
    if child.role == ActorRole::ExecutorTelemetry
        && let Some(target) = &child.target
    {
        return IssueHeld::OpenTarget {
            target: target.clone(),
            registration: parent.registration.clone(),
        };
    }
    match (&parent.registration, &child.registration) {
        (_, Some(registration)) => IssueHeld::Registration(registration.clone()),
        (Some(lease), None) => match (&child.run_id, planner) {
            (Some(run), _) if matches!(child.role, ActorRole::Worker | ActorRole::Wrapper) => {
                IssueHeld::Lease {
                    run: run.clone(),
                    lease: lease.clone(),
                }
            }
            (None, Some(planner)) => IssueHeld::OpenPlanner {
                planner,
                lease: lease.clone(),
            },
            _ => IssueHeld::Registered(lease.clone()),
        },
        (None, None) => IssueHeld::Nothing,
    }
}

/// What the queue says, in the issue's transaction, of what [`IssueHeld`]
/// names: the registration of its lease (`None`: not in the queue) and
/// whether its heartbeat expired, whether the run is leased to it, whether
/// the planner is open, and whether the target has not ended. Only what
/// the variant needs is read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeldFacts {
    pub registration_expired: Option<bool>,
    pub leased: bool,
    pub planner_open: bool,
    pub target_open: bool,
}

/// Why `held` is not held, as `facts` read it; `None` when it is, and the
/// token is issued in the same transaction.
pub fn not_held(held: &IssueHeld, facts: &HeldFacts) -> Option<String> {
    let live = || match facts.registration_expired {
        None => Some("the supervisor that asked is not registered".to_owned()),
        Some(true) => Some("the registration of the supervisor that asked has expired".to_owned()),
        Some(false) => None,
    };
    match held {
        IssueHeld::Nothing | IssueHeld::Registration(_) => None,
        IssueHeld::Lease { run, .. } => live().or_else(|| {
            (!facts.leased).then(|| format!("run {run} is not leased to the supervisor that asked"))
        }),
        IssueHeld::OpenPlanner { planner, .. } => live()
            .or_else(|| (!facts.planner_open).then(|| format!("planner {planner} is not open"))),
        IssueHeld::Registered(_) => live(),
        IssueHeld::OpenTarget {
            target,
            registration,
        } => registration.as_ref().and_then(|_| live()).or_else(|| {
            (!facts.target_open).then(|| {
                format!(
                    "the {} {} has ended or is not in the queue",
                    target.kind.as_str(),
                    target.id
                )
            })
        }),
    }
}

/// Who asked for a token, kept with it (ADR-t3102-1 decision 2): the
/// parent, by its [`Principal::credential_key`] (`user` for the person on
/// the service's socket), and the process that asked (the connection's),
/// which starts the child, waits for it and gives its token back at its
/// end: a command's own process for the job it starts, the supervisor's
/// for its jobs, so a job lives while that process does. `None` when the
/// service could not read that process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenIssuer {
    pub parent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<SessionProcess>,
    /// A supervisor parent's registration, which goes as that supervisor
    /// stops or is pruned; `None` for another parent, and for one kept
    /// before the service kept it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration: Option<LeaseToken>,
}

impl TokenIssuer {
    /// `parent` asking from `process`.
    pub fn of(parent: &Principal, process: Option<SessionProcess>) -> Self {
        Self {
            parent: parent.credential_key(),
            process,
            registration: parent
                .registration
                .clone()
                .filter(|_| parent.role == ActorRole::Supervisor),
        }
    }

    /// Whether the parent that asked may still be there: the process that
    /// asked may still run (`alive` says whether a process may, that is
    /// has not ended as [`SessionProcess::ended`] tells; one not read is
    /// taken to), and a supervisor's registration is still kept
    /// (`registered` says whether one is). A supervisor whose registration
    /// went has stopped, whatever still runs in the process that asked:
    /// what it started is taken as ended with it, as the sweep of the
    /// targets of no run takes a job whose supervisor is gone.
    pub fn there(
        &self,
        alive: impl Fn(&SessionProcess) -> bool,
        registered: impl Fn(&LeaseToken) -> bool,
    ) -> bool {
        self.process.as_ref().is_none_or(alive) && self.registration.as_ref().is_none_or(registered)
    }

    /// Whether what is kept tells whether the parent is there: its process
    /// or its registration. Neither is kept when the service could not
    /// read the process of a parent with no registration.
    fn judged(&self) -> bool {
        self.process.is_some() || self.registration.is_some()
    }
}

/// A token a key holds: its principal, who asked for it (`None` for one
/// issued before the service kept that) and when it was issued (unix
/// seconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldToken {
    pub principal: Principal,
    pub issued_by: Option<TokenIssuer>,
    pub issued_at: i64,
}

/// What the queue and the host tell of the job of a token whose parent
/// cannot be judged ([`reissue`], [`revoke_refusal`], [`job_token_ended`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    /// Its records tell that it ended ([`job_records_ended`]).
    Ended,
    /// They tell that it may still run.
    MayRun,
    /// The queue keeps no record of it (it keeps none of its kind, or none
    /// but the asking parent's own start: a parent that stopped after the
    /// token and before its record), so the processes that may have asked
    /// for its token tell ([`may_have_asked`]).
    Unrecorded(MayHaveAsked),
}

/// The processes that may have asked for a token whose parent cannot be
/// judged and that may still run ([`may_have_asked`]): its job, which the
/// queue keeps no record of, ran under one of them if under any.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MayHaveAsked {
    /// The credential keys of the supervisors registered by its issue
    /// whose registration is kept and whose process runs.
    pub supervisors: Vec<String>,
    /// Whether a `dagq` process of a command that may start the job (a
    /// person's or the inbox's) may still run: one that started by its
    /// issue and runs, none of those supervisors nor the service.
    pub command: bool,
}

impl JobState {
    /// Whether the job is taken as ended: its records tell so, or no
    /// process that may have asked for its token may still run (the
    /// parent that asked is gone, as a judged one's end is taken).
    pub fn ended(&self) -> bool {
        match self {
            Self::Ended => true,
            Self::MayRun => false,
            Self::Unrecorded(asked) => asked.supervisors.is_empty() && !asked.command,
        }
    }

    /// Whether `parent` (its credential key) alone may have asked for it:
    /// its job, if it still runs, is that parent's.
    fn only_of(&self, parent: &str) -> bool {
        matches!(self, Self::Unrecorded(asked)
            if !asked.command && asked.supervisors.len() == 1 && asked.supervisors[0] == parent)
    }

    /// Why a token of this job is neither replaced nor revoked while it is
    /// not [`Self::ended`].
    fn refusal(&self) -> &'static str {
        match self {
            Self::Unrecorded(_) => {
                "the job's token was kept with no parent that can be judged, the queue keeps no \
                 record of its job, and a process that may have asked for it still runs"
            }
            _ => UNJUDGED_JOB,
        }
    }
}

/// A process of the host as [`may_have_asked`] takes it: its command line
/// (`ps`'s `command`, its arguments), and when it started (unix seconds,
/// `None` when that cannot be read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostProcess {
    pub pid: u32,
    pub command: String,
    pub started_at: Option<i64>,
}

/// Who starts a job of `role` in this code: whether a supervisor does, and
/// the `dagq` subcommands a person's or the inbox's command starts one
/// with (an observation and a throughput review on the supervisor's timer
/// too, a setup review only by its command). What [`may_have_asked`]
/// counts as a process that may have asked for such a job's token; a new
/// way to start a job is to be added here.
fn job_starters(role: ActorRole) -> (bool, &'static [&'static str]) {
    match role {
        ActorRole::Observer => (true, &["observe"]),
        ActorRole::ThroughputReviewJob => (true, &["throughput-review"]),
        ActorRole::SetupReviewJob => (false, &["setup-review"]),
        _ => (true, &[]),
    }
}

/// Whether `command`, a process's command line, runs a `dagq` (by its
/// name, the fixed binary's `dagq.previous` an install leaves too) with one
/// of `subcommands` among its arguments.
fn runs_dagq_with(command: &str, subcommands: &[&str]) -> bool {
    let mut words = command.split_whitespace();
    words.by_ref().any(|word| {
        let name = word.rsplit('/').next().unwrap_or(word);
        name == "dagq" || name.starts_with("dagq.")
    }) && words.any(|word| subcommands.contains(&word))
}

/// The processes that may have asked for the token of `role` issued at
/// `issued_at` (unix seconds) whose parent cannot be judged and that may
/// still run (ADR-t3102-1 decision 2), `kept` the parent's credential key
/// kept with it (`None` for none kept, by an older binary):
///
/// - The supervisors in `registrations` whose process `alive` says runs:
///   with no parent kept, those registered by then, for a job a supervisor
///   starts ([`job_starters`]); with a supervisor kept, that one; with
///   another parent kept, none.
/// - Unless a supervisor is kept, whether one of the host's `processes`
///   (`None` when they cannot be listed, asked for only then) runs a
///   `dagq` subcommand that starts the job and started by then, other
///   than those supervisors' and `own` (the service's).
///
/// A second's slack is given to each time, as the host reads a start up to
/// a second late. Another queue's command is not told apart from this
/// queue's, so it keeps such a token while it runs.
pub fn may_have_asked(
    role: ActorRole,
    kept: Option<&str>,
    issued_at: i64,
    registrations: &[super::SupervisorRegistration],
    alive: &dyn Fn(u32) -> bool,
    processes: &dyn Fn() -> Option<Vec<HostProcess>>,
    own: u32,
) -> MayHaveAsked {
    let by_then = |started: i64| started <= issued_at + 1;
    let (by_supervisor, subcommands) = job_starters(role);
    let key = |registration: &super::SupervisorRegistration| {
        Principal::supervisor(&registration.token, registration.pid).credential_key()
    };
    let supervisor_kept = kept.is_some_and(|kept| kept.starts_with("supervisor:"));
    let supervisors: Vec<&super::SupervisorRegistration> = registrations
        .iter()
        .filter(|registration| {
            alive(registration.pid)
                && match kept {
                    None => by_supervisor && by_then(registration.started_at),
                    Some(kept) => kept == key(registration),
                }
        })
        .collect();
    let command = !supervisor_kept
        && !subcommands.is_empty()
        && processes().is_none_or(|processes| {
            processes.iter().any(|process| {
                process.pid != own
                    && !supervisors
                        .iter()
                        .any(|registration| registration.pid == process.pid)
                    && runs_dagq_with(&process.command, subcommands)
                    && process.started_at.is_none_or(by_then)
            })
        });
    MayHaveAsked {
        supervisors: supervisors.into_iter().map(key).collect(),
        command,
    }
}

/// Whether `role` is a job's: an agent a parent starts for one piece of
/// work, as a child of the process that asked for its token.
pub fn is_job(role: ActorRole) -> bool {
    matches!(
        role,
        ActorRole::Observer
            | ActorRole::ReviewJob
            | ActorRole::RecoveryJob
            | ActorRole::PlanReviewJob
            | ActorRole::GoalReviewJob
            | ActorRole::ThroughputReviewJob
            | ActorRole::SetupReviewJob
    )
}

/// Whether `principal`'s token is one a parent keeps for a target it
/// started that another may take over once that parent is gone: a
/// planner's, its wrapper's, and the executor's telemetry of a target of
/// no run.
fn taken_over(principal: &Principal) -> bool {
    use super::session_step::TargetKind;
    let target = principal.target.as_ref();
    match principal.role {
        ActorRole::Planner => true,
        ActorRole::Wrapper => target.is_some_and(|target| target.kind == TargetKind::Planner),
        ActorRole::ExecutorTelemetry => target.is_some_and(|target| target.run_id().is_none()),
        _ => false,
    }
}

/// What the issue of a token does with the one its key holds already
/// ([`reissue`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reissue {
    /// A new token, which replaces the one held, if any.
    Issue,
    /// The one held, handed over again: its own parent asks for it again
    /// while the process that asked for it runs (the jobs of one attempt
    /// that share an actor, a start tried again).
    Held,
    /// Refused, and why.
    Refused(&'static str),
}

/// What the issue of `child`'s token at the request of `issuer` does with
/// the one its key holds (`held`: `None` for no token, else the issuer kept
/// with it, `None` for one kept with none), `there` saying whether the
/// parent kept with a token is still there ([`TokenIssuer::there`]) and
/// `job` what the queue and the host tell of `child`'s job ([`JobState`])
/// (ADR-t3102-1 decision 2):
///
/// - A job's is replaced only at the request of the parent that asked for
///   it, once that parent is gone (the process that asked ended): the job
///   ran under it; while it is there, that parent is handed the same token
///   again. One of another parent's is never replaced (a parent starts
///   only its own jobs). One whose parent cannot be judged (kept with no
///   parent, by a binary before the service kept them, or with neither
///   its process nor a registration) is replaced once its job is taken as
///   ended ([`JobState::ended`]: its records tell so, or with none, no
///   process that may have asked for it still runs), and not before; the
///   one supervisor that alone may have asked for it is handed it again.
/// - A planner's, its wrapper's and the executor's telemetry of a target
///   of no run are not issued over one another parent asked for while
///   that parent is there; one whose parent is gone is (a supervisor
///   taking a planner over from one gone).
/// - Any other principal's replaces the one it holds by its own rule: a
///   run's under the run's lease, an inbox's for its new session, a
///   supervisor's for its registration.
pub fn reissue(
    child: &Principal,
    issuer: &TokenIssuer,
    held: Option<&Option<TokenIssuer>>,
    there: impl Fn(&TokenIssuer) -> bool,
    job: impl Fn() -> JobState,
) -> Reissue {
    let Some(held) = held else {
        return Reissue::Issue;
    };
    if is_job(child.role) {
        return match held {
            Some(held) if !held.judged() => unjudged(&job(), issuer),
            None => unjudged(&job(), issuer),
            Some(held) if held.parent != issuer.parent => {
                Reissue::Refused("the job's token is another parent's, which alone replaces it")
            }
            Some(held) if there(held) => Reissue::Held,
            Some(_) => Reissue::Issue,
        };
    }
    match held {
        Some(held) if taken_over(child) && held.parent != issuer.parent && there(held) => {
            Reissue::Refused("its token is another parent's, which is still there")
        }
        _ => Reissue::Issue,
    }
}

/// Why a job's token whose parent cannot be judged is neither replaced nor
/// revoked while the queue does not tell that its job ended.
const UNJUDGED_JOB: &str = "the job's token was kept with no parent that can be judged, and the \
                            queue does not tell that its job ended";

/// [`reissue`] over a job's token whose parent cannot be judged, at the
/// request of `issuer`: replaced once `job` is taken as ended, handed again
/// to the one supervisor that alone may have asked for it.
fn unjudged(job: &JobState, issuer: &TokenIssuer) -> Reissue {
    if job.ended() {
        Reissue::Issue
    } else if job.only_of(&issuer.parent) {
        Reissue::Held
    } else {
        Reissue::Refused(job.refusal())
    }
}

/// Why `parent` (its credential key) may not revoke `held`, `None` when it
/// may: a job's, a planner's, its wrapper's and the executor's telemetry
/// of a target of no run that another parent asked for is that parent's
/// to give back while it is there (`there`, [`TokenIssuer::there`];
/// ADR-t3102-1 decision 2), as the revocation would let the issue take it
/// over. A job's whose parent cannot be judged, kept with no parent at all
/// included, is no other caller's to give back than the parent kept with
/// it and the one supervisor that alone may have asked for it, and is
/// taken as there until its job is taken as ended (`job`,
/// [`JobState::ended`]), as [`reissue`] takes it.
pub fn revoke_refusal(
    held: &HeldToken,
    parent: &str,
    there: impl Fn(&TokenIssuer) -> bool,
    job: impl Fn() -> JobState,
) -> Option<&'static str> {
    let is_job = is_job(held.principal.role);
    let judged = held.issued_by.as_ref().is_some_and(TokenIssuer::judged);
    match &held.issued_by {
        Some(issuer) if issuer.parent == parent => None,
        _ if is_job && !judged => {
            let job = job();
            (!job.ended() && !job.only_of(parent)).then(|| job.refusal())
        }
        Some(issuer) => ((is_job || taken_over(&held.principal)) && there(issuer))
            .then_some("the token is another parent's, which is still there"),
        None => None,
    }
}

/// Whether a job's token kept with `issued_by` is left over: the parent
/// that asked for it, under whose process its job ran, is gone (`there`,
/// [`TokenIssuer::there`]). One whose parent cannot be judged (kept with
/// no issuer, or with neither its process nor a registration) is left
/// over once its job is taken as ended (`job`, [`JobState::ended`]).
pub fn job_token_ended(
    issued_by: Option<&TokenIssuer>,
    there: impl Fn(&TokenIssuer) -> bool,
    job: impl Fn() -> JobState,
) -> bool {
    match issued_by {
        Some(issuer) if issuer.judged() => !there(issuer),
        _ => job().ended(),
    }
}

/// The queue's records of the job a token is of ([`job_records_of`]): the
/// rows and starts its parent records as it runs it, which tell its end
/// whoever asked for its token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobRecords {
    /// The `plan_reviews` rows of a proposal's attempt.
    PlanReview { proposal: i64, attempt: i64 },
    /// The `goal_reviews` rows of a goal's attempt.
    GoalReview { goal: i64, attempt: i64 },
    /// The `headless_jobs` rows of a run's job of one of `kinds`, of
    /// `attempt`, whose label `label` takes.
    RunJob {
        kinds: &'static [&'static str],
        run: String,
        label: RowLabel,
        attempt: i64,
    },
    /// The `throughput_review_started` of a mode's period.
    ThroughputReview { mode: String, period: String },
    /// The `observe_started` of the session the observation named ahead
    /// (its actor's instance; a Codex observation names none).
    Observation { session_id: String },
}

/// Which labels of a run's job's `headless_jobs` rows are its
/// ([`JobRecords::RunJob`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowLabel {
    /// Only this label: a recovery job's alert, the judgment of an
    /// `already_satisfied` claim.
    Is(String),
    /// Any but this one: a run review's rows (its agent's and its
    /// programs'), but the judgment of a claim, a job of its own actor.
    Not(&'static str),
}

/// One record of a job's ([`JobRecords`]) as the queue holds it: its facts
/// as a target of no run's are read, and whose it is: the registration of
/// the supervisor that runs its row, or the process its command's start
/// names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    pub facts: super::telemetry::TargetFacts,
    pub supervisor: Option<LeaseToken>,
    pub pid: Option<u32>,
}

/// The queue's records of the job `principal` is of, by its role and its
/// actor id ([`crate::domain::ActorContext`]'s job actors); `None` for a
/// principal of no job, and for a job the queue keeps no record of (a
/// setup review's, an eval's agent job).
pub fn job_records_of(principal: &Principal) -> Option<JobRecords> {
    use super::headless_job::{RECOVERY, REVIEW, REVIEW_PROGRAM, SATISFIED_REVIEW_LABEL};
    let instance = principal
        .actor_id
        .strip_prefix(principal.role.as_str())?
        .strip_prefix(':')?;
    let pair = |text: &str| -> Option<(i64, i64)> {
        let (first, second) = text.split_once(':')?;
        Some((first.parse().ok()?, second.parse().ok()?))
    };
    match principal.role {
        ActorRole::PlanReviewJob => {
            pair(instance).map(|(proposal, attempt)| JobRecords::PlanReview { proposal, attempt })
        }
        ActorRole::GoalReviewJob => {
            pair(instance).map(|(goal, attempt)| JobRecords::GoalReview { goal, attempt })
        }
        ActorRole::ReviewJob => {
            let (run, attempt) = instance.split_once(':')?;
            let run = RunId::new(run).ok()?.as_str().to_owned();
            // The judgment of an `already_satisfied` claim
            // (`satisfied-<attempt>`), else a run review's attempt.
            Some(match attempt.strip_prefix("satisfied-") {
                Some(attempt) => JobRecords::RunJob {
                    kinds: &[REVIEW],
                    run,
                    label: RowLabel::Is(SATISFIED_REVIEW_LABEL.to_owned()),
                    attempt: attempt.parse().ok()?,
                },
                None => JobRecords::RunJob {
                    kinds: &[REVIEW, REVIEW_PROGRAM],
                    run,
                    label: RowLabel::Not(SATISFIED_REVIEW_LABEL),
                    attempt: attempt.parse().ok()?,
                },
            })
        }
        ActorRole::RecoveryJob => {
            let (run, rest) = instance.split_once(':')?;
            let (alert, attempt) = rest.rsplit_once(':')?;
            Some(JobRecords::RunJob {
                kinds: &[RECOVERY],
                run: RunId::new(run).ok()?.as_str().to_owned(),
                label: RowLabel::Is(alert.to_owned()),
                attempt: attempt.parse().ok()?,
            })
        }
        ActorRole::ThroughputReviewJob => {
            let (mode, period) = instance.split_once(':')?;
            Some(JobRecords::ThroughputReview {
                mode: mode.to_owned(),
                period: period.to_owned(),
            })
        }
        ActorRole::Observer => Some(JobRecords::Observation {
            session_id: instance.to_owned(),
        }),
        _ => None,
    }
}

/// Whether the queue's records of a job (`records`, of its
/// [`JobRecords`]) tell that it ended at `now_ms`, `alive` saying whether
/// a process runs: each ended as a target of no run's does
/// ([`super::telemetry::target_token_due`]: its row finished or its
/// supervisor gone, its command's finish recorded or its process gone or
/// past the time it ends by). `None` with no record: they tell nothing
/// ([`JobState::Unrecorded`]). `starting` is the parent asking to start the
/// job again, whose own record of that start (the row its registration
/// claimed, the start its command's process recorded) is left out while it
/// runs: it is the start that asks, not the job left.
pub fn job_records_ended(
    records: &[JobRecord],
    starting: Option<&TokenIssuer>,
    now_ms: i64,
    alive: &dyn Fn(u32) -> bool,
) -> Option<bool> {
    use super::session_step::TargetKind;
    let ended = |record: &JobRecord| {
        super::telemetry::target_token_due(TargetKind::PlanReview, &record.facts, now_ms, alive)
            .is_some()
    };
    let asking = |record: &JobRecord| {
        starting.is_some_and(|asking| {
            (record.supervisor.is_some() && record.supervisor == asking.registration)
                || (record.pid.is_some()
                    && record.pid == asking.process.as_ref().map(|process| process.pid))
        })
    };
    let left: Vec<&JobRecord> = records
        .iter()
        .filter(|record| !(asking(record) && !ended(record)))
        .collect();
    (!left.is_empty()).then(|| left.into_iter().all(ended))
}

/// Why `parent`'s request for the telemetry token of `child`, an
/// executor's, is refused, once [`super::authorization::starts`] allowed
/// the role: a target of a run is the supervisor's `issue_telemetry`
/// under the run's lease, and the user and the inbox ask only for the
/// executor of the job a command of theirs starts (an observation, a
/// throughput review).
pub fn telemetry_issue_refusal(parent: ActorRole, child: &Principal) -> Option<&'static str> {
    use super::session_step::TargetKind;
    let target = child.target.as_ref()?;
    if target.run_id().is_some() {
        return Some("a run's executor's telemetry token is issued under the run's lease");
    }
    match parent {
        ActorRole::Supervisor => None,
        _ if matches!(
            target.kind,
            TargetKind::Observer | TargetKind::ThroughputReview
        ) =>
        {
            None
        }
        _ => Some("the target is not of a job a command starts"),
    }
}

/// Who started a target of no run, as the queue records its start: what
/// the issue of its executor's telemetry token is matched against
/// ([`telemetry_starter_refusal`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetStart {
    /// A command's start (`observe_started`, `throughput_review_started`):
    /// the actor that recorded it (its role and actor id, `None` for one
    /// the queue does not know) and the process its payload names (the
    /// command's own, `None` for a start that names none).
    Command {
        actor: Option<(ActorRole, String)>,
        pid: Option<u32>,
    },
    /// A job the supervisor runs from its row (a plan review, a goal
    /// review, an eval job): the registration of the supervisor that
    /// claimed it, `None` for a row that names none.
    Supervised { supervisor: Option<LeaseToken> },
}

/// Why `parent`, asking from the process `process` (the connection's),
/// is refused the telemetry token of a target of no run whose start the
/// queue records as `start` (`None`: no record of it), read in the
/// issue's transaction (ADR-t3102-1 decision 2: only the parent that
/// started the job asks for its executor's token). A command's start is
/// its parent's when the actor that recorded it is the parent's (its role
/// and actor id) and the process it names is the one asking: a person's
/// or the inbox's command, or a supervisor's, is told from another
/// command of the same principal by its process. A supervised job's row is
/// its parent's when it names that supervisor's registration. `None` when
/// the start is the parent's.
pub fn telemetry_starter_refusal(
    parent: &Principal,
    process: Option<u32>,
    start: Option<&TargetStart>,
) -> Option<&'static str> {
    let Some(start) = start else {
        return Some("the queue has no record of the target's start");
    };
    let own = match start {
        TargetStart::Command { actor: None, .. } | TargetStart::Command { pid: None, .. } => {
            return Some("the target's start names no caller that started it");
        }
        TargetStart::Command {
            actor: Some((role, actor_id)),
            pid,
        } => *role == parent.role && *actor_id == parent.actor_id && *pid == process,
        TargetStart::Supervised { supervisor } => {
            parent.role == ActorRole::Supervisor
                && supervisor.is_some()
                && *supervisor == parent.registration
        }
    };
    (!own).then_some("the target was started by another caller")
}

/// Why the telemetry token of a target of no run is not issued to
/// `parent` asking from `process`, read in the issue's transaction: a
/// start that is not the parent's ([`telemetry_starter_refusal`]), or a
/// token the target holds already (`held`), the job's, which no request
/// replaces, its own starter's asking again included (the issue would
/// revoke it). `None` when it is issued.
pub fn telemetry_target_refusal(
    parent: &Principal,
    process: Option<u32>,
    start: Option<&TargetStart>,
    held: bool,
) -> Option<&'static str> {
    telemetry_starter_refusal(parent, process, start)
        .or_else(|| held.then_some("the target's executor holds its token already"))
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
        RunStatus::Integrated
            | RunStatus::Succeeded
            | RunStatus::Failed
            | RunStatus::Interrupted
            | RunStatus::Replaced
    )
}

/// Whether a supervisor may revoke the token kept under `key`
/// ([`Principal::credential_key`]): never a supervisor's own sign-in,
/// which only it gives back. Of the keys naming more than an actor id, a
/// landing's integrator's and an inbox session's are the sweep's once its
/// landing or its session ended (ADR-t3102-1 decision 2).
pub fn supervisor_revokes(key: &str) -> bool {
    if key.starts_with("supervisor:") {
        return false;
    }
    match key.split_once('@') {
        None => true,
        Some((actor_id, _)) => {
            actor_id.starts_with("integrator:") || actor_id == ActorRole::Inbox.as_str()
        }
    }
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
/// another session, keeps its token. A supervisor's sign-in goes by it
/// too, but only once its registration ended
/// ([`supervisor_token_ended`]); `None` for a key no principal the sweep
/// revokes has ([`supervisor_revokes`]).
pub fn swept_token_key(principal: &Principal) -> Option<String> {
    let key = principal.credential_key();
    let signed_in = principal.role == ActorRole::Supervisor && principal.registration.is_some();
    (signed_in || (principal.role != ActorRole::Supervisor && supervisor_revokes(&key)))
        .then_some(key)
}

/// Whether the supervisor that prunes the registration `pruning` revokes
/// the token of the registration `pruned` with the prune (ADR-t3102-1
/// decision 2, the end of a registration): another's, left behind, whose
/// process can no longer give it back. Its own it asks the service to
/// revoke as its process ends, after the last of its requests (the stop of
/// the queue service after a `down`'s drain reads the queue after its
/// registration went; `sign_out` of the application's supervisor
/// service); an exec of a handoff prunes none, and keeps the token for the
/// next image.
pub fn revoked_with_prune(pruning: &LeaseToken, pruned: &LeaseToken) -> bool {
    pruning != pruned
}

/// Why `held`, a supervisor's sign-in, may not be revoked at the request
/// of `up` or `down` (the `revoke_token` use case, ADR-t3102-1 decision 2),
/// `None` when it may: only once its registration is no longer kept
/// (`registered`), which they ask for after they pruned it. A supervisor
/// signs in before it registers, and its registration is not kept yet in
/// between either; `up` and `down` name only the registrations they pruned,
/// whose lease token a registration not written yet would not be known by.
/// Any other principal's token is refused: it goes by its own end
/// ([`swept_token_key`]).
pub fn registration_end_refusal(
    held: &HeldToken,
    registered: impl Fn(&LeaseToken) -> bool,
) -> Option<&'static str> {
    let Some(registration) = held
        .principal
        .registration
        .as_ref()
        .filter(|_| held.principal.role == ActorRole::Supervisor)
    else {
        return Some("only a supervisor's token is revoked at its registration's end");
    };
    registered(registration).then_some("the supervisor's registration is still kept")
}

/// Whether `held`, a supervisor's sign-in, is left over from a
/// registration that ended (ADR-t3102-1 decision 2): the registration is
/// no longer kept (`registered`), pruned by its stop, by `up` or `down`, or
/// by another supervisor's sweep, and the process that asked for it has
/// ended (`ended`, [`SessionProcess::ended`]). The registration is checked
/// as well as the process because the token is issued before the
/// registration is written; the process because an exec of a handoff keeps
/// both. One kept with no process that asked (an older binary's, whose
/// supervisor signed itself in, or one whose asking process's start did
/// not read) goes by the supervisor's own process, the pid its principal
/// names, which had started by the issue (`started_by_ended` with that pid
/// and [`HeldToken::issued_at`], [`started_by_ended`]); one that names no
/// pid is kept. `None` for any other principal's.
pub fn supervisor_token_ended(
    held: &HeldToken,
    registered: impl Fn(&LeaseToken) -> bool,
    ended: impl Fn(&SessionProcess) -> bool,
    started_by_ended: impl Fn(u32, i64) -> bool,
) -> Option<bool> {
    let registration = held
        .principal
        .registration
        .as_ref()
        .filter(|_| held.principal.role == ActorRole::Supervisor)?;
    let asked = held
        .issued_by
        .as_ref()
        .and_then(|issuer| issuer.process.as_ref());
    let process_ended = match asked {
        Some(process) => ended(process),
        None => held
            .principal
            .actor_id
            .strip_prefix("supervisor:")
            .and_then(|pid| pid.parse().ok())
            .is_some_and(|pid| started_by_ended(pid, held.issued_at)),
    };
    Some(!registered(registration) && process_ended)
}

/// Whether the process on a pid that had started by `by` (unix seconds) is
/// known to have ended, `alive` saying whether the pid lives and
/// `started_at` when the process on it started (`None` when that cannot be
/// read): the pid is gone, or the process on it started after then, another
/// one. A second's slack is given, as the host reads a start up to a second
/// late; a live pid whose start cannot be read has not ended.
pub fn started_by_ended(alive: bool, started_at: Option<i64>, by: i64) -> bool {
    !alive || started_at.is_some_and(|started| started > by + 1)
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

/// The directory, in the queue's service directory, of the records of
/// results the service did not take while their callers ran: a supervisor
/// that ended or execed and a job command that gave up keep them there,
/// one file each, and the next supervisor sends them (ADR-t3102-3
/// decision 5).
pub const HELD_DIR: &str = "held";

/// The directory, in the queue's service directory, of the verdicts of
/// plan and goal reviews that ended which the queue did not take: the
/// supervisor that reaped one keeps it there, one file each, until the
/// queue takes it, and the next supervisor applies those of a process
/// that ended or execed (ADR-t3102-3 decision 5).
pub const VERDICTS_DIR: &str = "verdicts";

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
    fn only_a_replacement_s_service_applies_migrations() {
        assert_eq!(
            ServiceStart::Replacement { breaking: false }.migrations(),
            StartMigrations::Compatible
        );
        assert_eq!(
            ServiceStart::Replacement { breaking: true }.migrations(),
            StartMigrations::Breaking
        );
        assert_eq!(ServiceStart::ByHand.migrations(), StartMigrations::None);
        assert_eq!(ServiceStart::Restart.migrations(), StartMigrations::None);
    }

    /// What a token is issued under follows its parent and its target: a
    /// supervisor's registration, a run's lease for its worker and wrapper,
    /// the planner open for its own and its wrapper's, the registration
    /// live for a job, and nothing for what the user and the inbox ask for.
    #[test]
    fn a_token_is_issued_under_what_its_parent_holds_of_its_target() {
        use super::super::session_step::TargetKind;
        let (lease, run) = (LeaseToken::new("l"), RunId::new("r1").unwrap());
        let supervisor = Principal::supervisor(&lease, 7);
        let user = Principal::user();
        assert_eq!(
            issue_held(&user, &Principal::supervisor(&LeaseToken::new("n"), 8)),
            IssueHeld::Registration(LeaseToken::new("n"))
        );
        let leased = IssueHeld::Lease {
            run: run.clone(),
            lease: lease.clone(),
        };
        for child in [
            Principal::worker(&run, TaskId::new(1)),
            Principal::wrapper(&StepTarget::run(&run), Some(TaskId::new(1))),
        ] {
            assert_eq!(issue_held(&supervisor, &child), leased, "{child:?}");
        }
        for child in [
            Principal::planner(3),
            Principal::wrapper(&StepTarget::of(TargetKind::Planner, 3), None),
        ] {
            assert_eq!(
                issue_held(&supervisor, &child),
                IssueHeld::OpenPlanner {
                    planner: 3,
                    lease: lease.clone()
                }
            );
        }
        for child in [
            Principal::of(&ActorContext::review_job(&run, 1)),
            Principal::of(&ActorContext::plan_review_job(4, 1)),
        ] {
            assert_eq!(
                issue_held(&supervisor, &child),
                IssueHeld::Registered(lease.clone())
            );
        }
        assert_eq!(
            issue_held(&user, &Principal::of(&ActorContext::setup_review_job("s"))),
            IssueHeld::Nothing
        );
        let inbox = Principal::inbox(SessionProcess {
            pid: 1,
            start: "s".into(),
        });
        assert_eq!(issue_held(&user, &inbox), IssueHeld::Nothing);
        // Each names itself; one named as another principal does not.
        for child in [
            Principal::worker(&run, TaskId::new(1)),
            Principal::wrapper(&StepTarget::run(&run), Some(TaskId::new(1))),
            Principal::planner(3),
            Principal::supervisor(&lease, 7),
            Principal::of(&ActorContext::review_job(&run, 1)),
            Principal::of(&ActorContext::recovery_job(&run, "a", 1)),
            Principal::of(&ActorContext::plan_review_job(4, 1)),
            Principal::of(&ActorContext::instance(ActorRole::Observer, "s1")),
            inbox.clone(),
        ] {
            assert!(child.names_itself(), "{child:?}");
        }
        let named = |role, id: &str| Principal::of(&ActorContext::new(role, id.to_owned()));
        for other in [
            named(ActorRole::Observer, "planner:5"),
            named(ActorRole::Observer, "observer:"),
            named(ActorRole::Inbox, "inbox:1"),
        ] {
            assert!(!other.names_itself(), "{other:?}");
        }
        let mut stray = Principal::worker(&run, TaskId::new(1));
        stray.actor_id = "worker:r2".into();
        assert!(!stray.names_itself());
    }

    /// A job's token is replaced only at the request of the parent that
    /// asked for it, once the process that asked has ended, and handed to
    /// it again while that process runs; another parent's, or one kept
    /// with no parent, never. A planner's, its wrapper's and the telemetry
    /// of a target of no run are not issued over, nor revoked from,
    /// another parent whose process runs; a run's, an inbox's and a
    /// supervisor's replace theirs by their own rules.
    #[test]
    fn a_jobs_token_is_replaced_only_by_its_parent_once_its_process_ended() {
        use super::super::session_step::TargetKind;
        let process = |start: &str| SessionProcess {
            pid: 7,
            start: start.into(),
        };
        let alive = |issuer: &TokenIssuer| issuer.there(|session| session.start == "now", |_| true);
        let supervisor = Principal::supervisor(&LeaseToken::new("a"), 7);
        let other = Principal::supervisor(&LeaseToken::new("b"), 8);
        let inbox = Principal::inbox(process("now"));
        let asking = |parent: &Principal| TokenIssuer::of(parent, Some(process("now")));
        let kept =
            |parent: &Principal, start: &str| Some(TokenIssuer::of(parent, Some(process(start))));
        let job = Principal::of(&ActorContext::throughput_review_job("hourly", "h"));
        assert_eq!(
            reissue(&job, &asking(&inbox), None, alive, || JobState::MayRun),
            Reissue::Issue
        );
        let refused = |reissued: Reissue| matches!(reissued, Reissue::Refused(_));
        for (by, held, want) in [
            (&inbox, kept(&supervisor, "now"), "refused"),
            (&inbox, kept(&supervisor, "gone"), "refused"),
            (&inbox, None, "refused"),
            (&supervisor, None, "refused"),
            (&supervisor, kept(&supervisor, "now"), "held"),
            (
                &supervisor,
                Some(TokenIssuer::of(&supervisor, None)),
                "held",
            ),
            (&supervisor, kept(&supervisor, "gone"), "issue"),
        ] {
            let reissued = reissue(&job, &asking(by), Some(&held), alive, || JobState::MayRun);
            let got = match reissued {
                Reissue::Issue => "issue",
                Reissue::Held => "held",
                Reissue::Refused(_) => "refused",
            };
            assert_eq!(got, want, "{by:?} over {held:?}");
        }
        let planner = StepTarget::of(TargetKind::Planner, 4);
        for child in [
            Principal::planner(4),
            Principal::wrapper(&planner, None),
            Principal::telemetry(&planner, None),
        ] {
            let over = |by: &Principal, held: Option<TokenIssuer>| {
                reissue(&child, &asking(by), Some(&held), alive, || JobState::MayRun)
            };
            assert!(refused(over(&other, kept(&supervisor, "now"))));
            assert_eq!(over(&other, kept(&supervisor, "gone")), Reissue::Issue);
            assert_eq!(over(&supervisor, kept(&supervisor, "now")), Reissue::Issue);
            assert_eq!(over(&other, None), Reissue::Issue);
        }
        let run = RunId::new("r1").unwrap();
        for child in [
            Principal::worker(&run, TaskId::new(1)),
            Principal::wrapper(&StepTarget::run(&run), Some(TaskId::new(1))),
            Principal::telemetry(&StepTarget::run(&run), Some(TaskId::new(1))),
            Principal::inbox(process("now")),
            supervisor.clone(),
        ] {
            assert_eq!(
                reissue(
                    &child,
                    &asking(&other),
                    Some(&kept(&supervisor, "now")),
                    alive,
                    || JobState::MayRun
                ),
                Reissue::Issue,
                "{child:?}"
            );
        }
        // Revoked by its own parent, or once that parent's process ended;
        // a run's by anyone, as before.
        let held = |principal: &Principal, start: &str| HeldToken {
            principal: principal.clone(),
            issued_by: kept(&supervisor, start),
            issued_at: 0,
        };
        let mine = supervisor.credential_key();
        let theirs = other.credential_key();
        for principal in [
            job.clone(),
            Principal::planner(4),
            Principal::telemetry(&planner, None),
        ] {
            assert!(
                revoke_refusal(&held(&principal, "now"), &theirs, alive, || {
                    JobState::MayRun
                })
                .is_some()
            );
            assert_eq!(
                revoke_refusal(&held(&principal, "now"), &mine, alive, || JobState::MayRun),
                None
            );
            assert_eq!(
                revoke_refusal(&held(&principal, "gone"), &theirs, alive, || {
                    JobState::MayRun
                }),
                None
            );
        }
        for of_run in [
            Principal::worker(&run, TaskId::new(1)),
            Principal::telemetry(&StepTarget::run(&run), Some(TaskId::new(1))),
        ] {
            assert_eq!(
                revoke_refusal(&held(&of_run, "now"), &theirs, alive, || JobState::MayRun),
                None
            );
        }
        // Left over once the process that asked ended; kept with none, kept.
        assert!(job_token_ended(
            kept(&supervisor, "gone").as_ref(),
            alive,
            || JobState::MayRun
        ));
        assert!(!job_token_ended(
            kept(&supervisor, "now").as_ref(),
            alive,
            || JobState::MayRun
        ));
        assert!(!job_token_ended(
            Some(&TokenIssuer::of(&supervisor, None)),
            alive,
            || JobState::MayRun
        ));
        assert!(!job_token_ended(None, alive, || JobState::MayRun));
    }

    /// A session's process runs only while its pid lives with the same
    /// start, and has ended only once its pid is gone or another start is
    /// printed for it: a live pid whose start cannot be read neither runs
    /// (its token is not taken) nor has ended (its parent is still there).
    #[test]
    fn a_process_whose_start_cannot_be_read_neither_runs_nor_has_ended() {
        let process = SessionProcess {
            pid: 7,
            start: "now".into(),
        };
        assert!(process.runs(true, Some("now")));
        assert!(!process.ended(true, Some("now")));
        for start in [Some("now"), Some("then"), None] {
            assert!(!process.runs(false, start));
            assert!(process.ended(false, start));
        }
        assert!(!process.runs(true, Some("then")));
        assert!(process.ended(true, Some("then")));
        assert!(!process.runs(true, None));
        assert!(!process.ended(true, None));
        // A parent asking from it is there while it has not ended.
        let issuer = TokenIssuer::of(
            &Principal::supervisor(&LeaseToken::new("a"), 7),
            Some(process.clone()),
        );
        assert!(issuer.there(|kept| !kept.ended(true, None), |_| true));
        assert!(!issuer.there(|kept| !kept.ended(true, Some("then")), |_| true));
    }

    /// A job's token whose parent cannot be judged and whose job the queue
    /// keeps no record of (a supervisor that stopped after its token and
    /// before its row, a setup review's) is protected while a process that
    /// may have asked for it may still run (ADR-t3102-1 decision 2): a
    /// supervisor registered by its issue whose process runs, for a job a
    /// supervisor starts, or a `dagq` of a subcommand that starts the job
    /// and started by then. Once none may, it
    /// is replaced, revoked and swept; the one supervisor that alone may
    /// have asked for it is handed it again and may give it back.
    #[test]
    fn a_job_token_of_no_record_goes_once_no_process_that_may_have_asked_runs() {
        let registration =
            |token: &str, pid: u32, started_at: i64| super::super::SupervisorRegistration {
                token: LeaseToken::new(token),
                pid,
                parallel: 1,
                started_at,
                heartbeat_at: started_at,
                mode: None,
                workspace_id: None,
                handoff_accepted: false,
                handoff_binary: None,
                auto_update: false,
                max_waiting: None,
                parallel_source: None,
                max_waiting_source: None,
                runtime_planners: None,
                runtime_planners_source: None,
                claim_spacing: None,
                claim_spacing_source: None,
                max_spikes: None,
                max_spikes_source: None,
                class_aging: None,
                class_aging_source: None,
                max_load: None,
                max_load_source: None,
                providers: None,
                binary_version: None,
            };
        // Old (pid 1) registered by the issue at 100; new (pid 2) after it;
        // gone (pid 3) by then, its process ended.
        let registrations = [
            registration("old", 1, 50),
            registration("new", 2, 200),
            registration("gone", 3, 50),
        ];
        let alive = |pid: u32| pid != 3;
        let old = Principal::supervisor(&LeaseToken::new("old"), 1).credential_key();
        let new = Principal::supervisor(&LeaseToken::new("new"), 2).credential_key();
        let asked = |role, kept: Option<&str>, processes: Vec<HostProcess>| {
            may_have_asked(
                role,
                kept,
                100,
                &registrations,
                &alive,
                &|| Some(processes.clone()),
                9,
            )
        };
        assert_eq!(
            asked(ActorRole::RecoveryJob, None, Vec::new()),
            MayHaveAsked {
                supervisors: vec![old.clone()],
                command: false
            }
        );
        // A job no command starts: no process of the host is asked.
        let unlisted = || -> Option<Vec<HostProcess>> { panic!("no command starts it") };
        assert_eq!(
            may_have_asked(ActorRole::ReviewJob, None, 100, &[], &alive, &unlisted, 9),
            MayHaveAsked::default()
        );
        // A setup review, which only its command starts: no supervisor
        // counts, and of the host's processes a `dagq setup-review` that
        // started by then and runs, the renamed fixed binary's too, never a
        // resident `dagq` (the broker, the service, a supervisor) nor the
        // service's own.
        let process = |pid: u32, command: &str, started_at: Option<i64>| HostProcess {
            pid,
            command: command.into(),
            started_at,
        };
        let setup = |processes| asked(ActorRole::SetupReviewJob, None, processes).command;
        assert_eq!(
            asked(ActorRole::SetupReviewJob, None, Vec::new()),
            MayHaveAsked::default()
        );
        for command in [
            "/Users/u/.local/bin/dagq setup-review --db q.db",
            "/Users/u/.local/bin/dagq.previous setup-review",
            "dagq --db q.db setup-review",
        ] {
            assert!(setup(vec![process(4, command, Some(90))]), "{command}");
            assert!(setup(vec![process(4, command, None)]), "{command}");
            assert!(!setup(vec![process(4, command, Some(150))]), "{command}");
            assert!(!setup(vec![process(9, command, Some(90))]), "{command}");
        }
        for command in [
            "/Users/u/.local/bin/dagq queue-broker --db q.db",
            "dagq service serve",
            "dagq supervise",
            "dagq observe",
            "vim setup-review.md",
        ] {
            assert!(!setup(vec![process(4, command, Some(90))]), "{command}");
        }
        assert!(
            may_have_asked(
                ActorRole::SetupReviewJob,
                None,
                100,
                &[],
                &alive,
                &|| None,
                9
            )
            .command
        );
        // An observation, which a supervisor's timer starts too: the
        // supervisors registered by then, whose own `dagq observe` is not a
        // command's.
        assert_eq!(
            asked(
                ActorRole::Observer,
                None,
                vec![
                    process(1, "dagq observe", Some(40)),
                    process(5, "dagq supervise", Some(40))
                ]
            ),
            MayHaveAsked {
                supervisors: vec![old.clone()],
                command: false
            }
        );
        // A parent kept with it whose process was not read: a supervisor is
        // judged by its own registration alone, another parent by the
        // commands alone.
        assert_eq!(
            may_have_asked(
                ActorRole::ThroughputReviewJob,
                Some(&new),
                100,
                &registrations,
                &alive,
                &unlisted,
                9
            ),
            MayHaveAsked {
                supervisors: vec![new.clone()],
                command: false
            }
        );
        let gone = Principal::supervisor(&LeaseToken::new("gone"), 3).credential_key();
        assert_eq!(
            may_have_asked(
                ActorRole::ThroughputReviewJob,
                Some(&gone),
                100,
                &registrations,
                &alive,
                &unlisted,
                9
            ),
            MayHaveAsked::default()
        );
        assert_eq!(
            asked(
                ActorRole::ThroughputReviewJob,
                Some("user"),
                vec![process(4, "dagq throughput-review run", Some(90))]
            ),
            MayHaveAsked {
                supervisors: Vec::new(),
                command: true
            }
        );

        let job = Principal::of(&ActorContext::recovery_job(
            &RunId::new("r").unwrap(),
            "idle",
            1,
        ));
        let asking = |parent: &str| TokenIssuer {
            parent: parent.to_owned(),
            process: None,
            registration: None,
        };
        let there = |_: &TokenIssuer| true;
        let unrecorded = |supervisors: Vec<String>, command: bool| {
            JobState::Unrecorded(MayHaveAsked {
                supervisors,
                command,
            })
        };
        let held = HeldToken {
            principal: job.clone(),
            issued_by: None,
            issued_at: 100,
        };
        // Another supervisor, or a command, may still run it: kept.
        for state in [
            unrecorded(vec![old.clone()], false),
            unrecorded(Vec::new(), true),
        ] {
            assert!(matches!(
                reissue(&job, &asking(&new), Some(&None), there, || state.clone()),
                Reissue::Refused(why) if why.contains("no record of its job")
            ));
            assert!(revoke_refusal(&held, &new, there, || state.clone()).is_some());
            assert!(!job_token_ended(None, there, || state.clone()));
        }
        // None may: replaced, revoked and swept.
        let ended = unrecorded(Vec::new(), false);
        assert_eq!(
            reissue(&job, &asking(&new), Some(&None), there, || ended.clone()),
            Reissue::Issue
        );
        assert_eq!(revoke_refusal(&held, &new, there, || ended.clone()), None);
        assert!(job_token_ended(None, there, || ended.clone()));
        // The one that alone may have: handed it again, and gives it back,
        // which another is refused; it is not swept while that one runs.
        let alone = unrecorded(vec![old.clone()], false);
        assert_eq!(
            reissue(&job, &asking(&old), Some(&None), there, || alone.clone()),
            Reissue::Held
        );
        assert_eq!(revoke_refusal(&held, &old, there, || alone.clone()), None);
        assert!(revoke_refusal(&held, &new, there, || alone.clone()).is_some());
        assert!(!job_token_ended(None, there, || alone.clone()));
        // Records that tell it may run keep it whoever asks.
        assert!(matches!(
            reissue(&job, &asking(&old), Some(&None), there, || JobState::MayRun),
            Reissue::Refused(_)
        ));
    }

    /// A job's token whose parent cannot be judged (kept with no parent, or
    /// with neither its process nor a registration) is replaced, by any
    /// parent that may ask for it, and swept once the queue's records tell
    /// that its job ended, and not before. A token whose parent is judged
    /// keeps its parent's rule whatever the records say.
    #[test]
    fn a_job_token_of_no_parent_judged_goes_once_its_jobs_records_tell_it_ended() {
        let job = Principal::of(&ActorContext::throughput_review_job("hourly", "h"));
        let supervisor = Principal::supervisor(&LeaseToken::new("a"), 7);
        let other = Principal::supervisor(&LeaseToken::new("b"), 8);
        let process = |start: &str| SessionProcess {
            pid: 7,
            start: start.into(),
        };
        let asking = TokenIssuer::of(&other, Some(process("now")));
        let there = |issuer: &TokenIssuer| issuer.there(|_| true, |_| true);
        let unread = TokenIssuer::of(&Principal::inbox(process("now")), None);
        for held in [None, Some(unread.clone())] {
            assert!(matches!(
                reissue(&job, &asking, Some(&held), there, || JobState::MayRun),
                Reissue::Refused(why) if why.contains("no parent that can be judged")
            ));
            assert_eq!(
                reissue(&job, &asking, Some(&held), there, || JobState::Ended),
                Reissue::Issue
            );
            assert!(!job_token_ended(held.as_ref(), there, || JobState::MayRun));
            assert!(job_token_ended(held.as_ref(), there, || JobState::Ended));
        }
        // Its revocation by another parent, kept with no parent included, as
        // long as the records do not tell its job ended: else a revocation
        // would let the issue take it over.
        let theirs = other.credential_key();
        for issued_by in [None, Some(unread.clone())] {
            let held = HeldToken {
                principal: job.clone(),
                issued_by,
                issued_at: 0,
            };
            assert!(revoke_refusal(&held, &theirs, there, || JobState::MayRun).is_some());
            assert_eq!(
                revoke_refusal(&held, &theirs, there, || JobState::Ended),
                None
            );
        }
        // Given back by the parent kept with it, though not judged; another
        // principal's kept with no parent goes by its own rule.
        let unread_held = HeldToken {
            principal: job.clone(),
            issued_by: Some(unread.clone()),
            issued_at: 0,
        };
        assert_eq!(
            revoke_refusal(&unread_held, &unread.parent, there, || JobState::MayRun),
            None
        );
        let planner_held = HeldToken {
            principal: Principal::planner(4),
            issued_by: None,
            issued_at: 0,
        };
        assert_eq!(
            revoke_refusal(&planner_held, &theirs, there, || JobState::MayRun),
            None
        );
        // A supervisor's whose process was not read is judged by its
        // registration.
        let registered = Some(TokenIssuer::of(&supervisor, None));
        assert!(matches!(
            reissue(&job, &asking, Some(&registered), there, || JobState::Ended),
            Reissue::Refused(why) if why.contains("another parent's")
        ));
        assert!(!job_token_ended(registered.as_ref(), there, || {
            JobState::Ended
        }));
        let gone = |issuer: &TokenIssuer| issuer.there(|_| true, |_| false);
        assert!(job_token_ended(registered.as_ref(), gone, || {
            JobState::MayRun
        }));
    }

    /// The records of a job are named by its role and actor id; a job the
    /// queue keeps none of, and a principal of no job, name none.
    #[test]
    fn a_jobs_records_are_named_by_its_role_and_actor_id() {
        use super::super::headless_job::{
            RECOVERY, REVIEW, REVIEW_PROGRAM, SATISFIED_REVIEW_LABEL,
        };
        let run = RunId::new("r1").unwrap();
        let of = |actor: ActorContext| job_records_of(&Principal::of(&actor));
        assert_eq!(
            of(ActorContext::plan_review_job(4, 2)),
            Some(JobRecords::PlanReview {
                proposal: 4,
                attempt: 2
            })
        );
        assert_eq!(
            of(ActorContext::goal_review_job(5, 1)),
            Some(JobRecords::GoalReview {
                goal: 5,
                attempt: 1
            })
        );
        assert_eq!(
            of(ActorContext::review_job(&run, 3)),
            Some(JobRecords::RunJob {
                kinds: &[REVIEW, REVIEW_PROGRAM],
                run: "r1".into(),
                label: RowLabel::Not(SATISFIED_REVIEW_LABEL),
                attempt: 3
            })
        );
        assert_eq!(
            of(ActorContext::recovery_job(&run, "idle:long", 2)),
            Some(JobRecords::RunJob {
                kinds: &[RECOVERY],
                run: "r1".into(),
                label: RowLabel::Is("idle:long".into()),
                attempt: 2
            })
        );
        assert_eq!(
            of(ActorContext::throughput_review_job(
                "hourly",
                "2026-10-25T10:00"
            )),
            Some(JobRecords::ThroughputReview {
                mode: "hourly".into(),
                period: "2026-10-25T10:00".into()
            })
        );
        assert_eq!(
            of(ActorContext::instance(ActorRole::Observer, "u1")),
            Some(JobRecords::Observation {
                session_id: "u1".into()
            })
        );
        assert_eq!(
            of(ActorContext::review_job(&run, "satisfied-2")),
            Some(JobRecords::RunJob {
                kinds: &[REVIEW],
                run: "r1".into(),
                label: RowLabel::Is(SATISFIED_REVIEW_LABEL.into()),
                attempt: 2
            })
        );
        assert_eq!(of(ActorContext::agent_eval_job("e", 0, 1)), None);
        assert_eq!(of(ActorContext::setup_review_job("p")), None);
        assert_eq!(of(ActorContext::worker(&run, TaskId::new(1))), None);
    }

    /// A job's records tell that it ended only when there is one and each
    /// ended: its row finished or its supervisor gone, its command's finish
    /// recorded, its process gone or past the time it ends by. The record
    /// of the start that asks (the row its registration claimed, the start
    /// its process recorded) is left out while it runs, and only then.
    #[test]
    fn a_jobs_records_tell_its_end_only_when_each_ended() {
        use super::super::telemetry::{Beat, TargetFacts};
        let alive = |pid: u32| pid == 1 || pid == 7;
        let record = |facts: TargetFacts| JobRecord {
            facts,
            supervisor: None,
            pid: None,
        };
        let running = TargetFacts::SupervisedJob {
            supervisor: Some(Beat {
                pid: 1,
                age_secs: 0,
            }),
        };
        let command = |pid: u32, ends_by_ms: i64| JobRecord {
            pid: Some(pid),
            ..record(TargetFacts::StartedCommand {
                ends_by_ms,
                pid: Some(pid),
            })
        };
        let ended =
            |records: &[JobRecord]| job_records_ended(records, None, 1_000, &alive) == Some(true);
        // With no record they tell nothing; with one that runs, that it may.
        assert_eq!(job_records_ended(&[], None, 1_000, &alive), None);
        assert_eq!(
            job_records_ended(&[command(1, 2_000)], None, 1_000, &alive),
            Some(false)
        );
        assert!(ended(&[record(TargetFacts::Ended)]));
        assert!(!ended(&[
            record(TargetFacts::Ended),
            record(running.clone())
        ]));
        assert!(ended(&[record(TargetFacts::SupervisedJob {
            supervisor: None
        })]));
        assert!(!ended(&[command(1, 2_000)]));
        assert!(ended(&[command(2, 2_000)]));
        assert!(ended(&[command(1, 500)]));

        // Asked again by the command of process 7, whose own start runs,
        // and by the supervisor of registration `a`, whose row it claimed.
        let command_asks = TokenIssuer::of(
            &Principal::inbox(SessionProcess {
                pid: 7,
                start: "now".into(),
            }),
            Some(SessionProcess {
                pid: 7,
                start: "now".into(),
            }),
        );
        let supervisor_asks =
            TokenIssuer::of(&Principal::supervisor(&LeaseToken::new("a"), 9), None);
        let claimed = |token: &str| JobRecord {
            supervisor: Some(LeaseToken::new(token)),
            ..record(running.clone())
        };
        let again = |records: &[JobRecord], asking: &TokenIssuer| {
            job_records_ended(records, Some(asking), 1_000, &alive) == Some(true)
        };
        assert!(again(
            &[command(2, 2_000), command(7, 2_000)],
            &command_asks
        ));
        assert!(!again(
            &[command(1, 2_000), command(7, 2_000)],
            &command_asks
        ));
        // Its own start alone: no record of the job left, nothing told.
        assert_eq!(
            job_records_ended(&[command(7, 2_000)], Some(&command_asks), 1_000, &alive),
            None
        );
        assert!(!ended(&[command(2, 2_000), command(7, 2_000)]));
        assert!(again(
            &[record(TargetFacts::Ended), claimed("a")],
            &supervisor_asks
        ));
        assert!(!again(
            &[record(TargetFacts::Ended), claimed("b")],
            &supervisor_asks
        ));
    }

    /// A supervisor whose registration went (it stopped, or was pruned)
    /// is gone though its process runs on: what only a parent there keeps
    /// from another (a planner's, the telemetry of a target of no run, a
    /// job's revocation and its sweep) is no longer kept. Another parent
    /// still never replaces a job's, and a parent kept with no
    /// registration is judged by its process alone.
    #[test]
    fn a_supervisor_whose_registration_went_is_gone_though_its_process_runs() {
        use super::super::session_step::TargetKind;
        let process = SessionProcess {
            pid: 7,
            start: "now".into(),
        };
        let stopped = Principal::supervisor(&LeaseToken::new("stopped"), 7);
        let live = Principal::supervisor(&LeaseToken::new("live"), 7);
        let there = |issuer: &TokenIssuer| {
            issuer.there(|_| true, |registration| registration.as_str() != "stopped")
        };
        let kept = |parent: &Principal| TokenIssuer::of(parent, Some(process.clone()));
        assert_eq!(
            kept(&stopped).registration,
            Some(LeaseToken::new("stopped"))
        );
        assert!(!there(&kept(&stopped)));
        assert!(there(&kept(&live)));
        assert!(there(&kept(&Principal::inbox(process.clone()))));
        let planner = StepTarget::of(TargetKind::Planner, 4);
        for child in [
            Principal::planner(4),
            Principal::wrapper(&planner, None),
            Principal::telemetry(&planner, None),
        ] {
            assert_eq!(
                reissue(
                    &child,
                    &kept(&live),
                    Some(&Some(kept(&stopped))),
                    there,
                    || JobState::MayRun
                ),
                Reissue::Issue
            );
            let held = HeldToken {
                principal: child.clone(),
                issued_by: Some(kept(&stopped)),
                issued_at: 0,
            };
            assert_eq!(
                revoke_refusal(&held, &live.credential_key(), there, || JobState::MayRun),
                None
            );
        }
        let job = Principal::of(&ActorContext::throughput_review_job("hourly", "h"));
        assert!(matches!(
            reissue(
                &job,
                &kept(&live),
                Some(&Some(kept(&stopped))),
                there,
                || JobState::MayRun
            ),
            Reissue::Refused(_)
        ));
        let held = |issuer: TokenIssuer| HeldToken {
            principal: job.clone(),
            issued_by: Some(issuer),
            issued_at: 0,
        };
        assert_eq!(
            revoke_refusal(&held(kept(&stopped)), &live.credential_key(), there, || {
                JobState::MayRun
            }),
            None
        );
        assert!(
            revoke_refusal(&held(kept(&live)), &stopped.credential_key(), there, || {
                JobState::MayRun
            })
            .is_some()
        );
        assert!(job_token_ended(Some(&kept(&stopped)), there, || {
            JobState::MayRun
        }));
        assert!(!job_token_ended(Some(&kept(&live)), there, || {
            JobState::MayRun
        }));
        // One kept with no registration (the inbox's, or a supervisor's
        // kept before the registration was) by its process alone.
        let ended = SessionProcess {
            pid: 7,
            start: "gone".into(),
        };
        let by_process =
            |issuer: &TokenIssuer| issuer.there(|session| session.start == "now", |_| false);
        let unregistered = TokenIssuer {
            registration: None,
            ..kept(&stopped)
        };
        assert!(by_process(&unregistered));
        assert!(!by_process(&TokenIssuer {
            process: Some(ended.clone()),
            ..unregistered.clone()
        }));
        assert!(job_token_ended(
            Some(&TokenIssuer::of(
                &Principal::inbox(process.clone()),
                Some(ended)
            )),
            by_process,
            || JobState::MayRun
        ));
    }

    /// Whether what a token is issued under is held, from the queue's
    /// facts read in the issue's transaction: a lease lost, a registration
    /// gone or expired, a planner closed each refuse it, and what needs
    /// nothing of the queue never is.
    #[test]
    fn what_a_token_is_issued_under_is_held_or_says_why_not() {
        let (lease, run) = (LeaseToken::new("l"), RunId::new("r1").unwrap());
        let live = HeldFacts {
            registration_expired: Some(false),
            leased: true,
            planner_open: true,
            target_open: true,
        };
        let leased = IssueHeld::Lease {
            run: run.clone(),
            lease: lease.clone(),
        };
        let planner = IssueHeld::OpenPlanner {
            planner: 3,
            lease: lease.clone(),
        };
        let registered = IssueHeld::Registered(lease.clone());
        for held in [
            &IssueHeld::Nothing,
            &IssueHeld::Registration(lease.clone()),
            &leased,
            &planner,
            &registered,
        ] {
            assert_eq!(not_held(held, &live), None, "{held:?}");
        }
        // A supervisor's own registration needs nothing, gone or expired
        // (the exec of a handoff resumes it).
        assert_eq!(
            not_held(
                &IssueHeld::Registration(lease.clone()),
                &HeldFacts::default()
            ),
            None
        );
        let lost = HeldFacts {
            leased: false,
            ..live
        };
        assert!(not_held(&leased, &lost).unwrap().contains("not leased"));
        let gone = HeldFacts {
            registration_expired: None,
            ..live
        };
        let expired = HeldFacts {
            registration_expired: Some(true),
            ..live
        };
        // A run's lease, a planner and a job's each need the registration
        // live (ADR-t3102-1 decision 2): a lease left by a registration
        // gone or expired issues nothing.
        for held in [&leased, &planner, &registered] {
            assert!(not_held(held, &gone).unwrap().contains("not registered"));
            assert!(not_held(held, &expired).unwrap().contains("expired"));
        }
        let closed = HeldFacts {
            planner_open: false,
            ..live
        };
        assert!(not_held(&planner, &closed).unwrap().contains("not open"));
        assert_eq!(not_held(&registered, &closed), None);
    }

    /// An executor's telemetry token is issued under its target open, and
    /// the registration of the supervisor that asked live when one asked;
    /// the user and the inbox ask only for an observation's and a
    /// throughput review's, and nobody here for a run's.
    #[test]
    fn a_telemetry_token_is_issued_under_its_target_open() {
        use super::super::session_step::TargetKind;
        let observer = StepTarget::of(TargetKind::Observer, 4);
        let child = Principal::telemetry(&observer, None);
        let lease = LeaseToken::new("l");
        let supervisor = Principal::supervisor(&lease, 7);
        assert_eq!(
            issue_held(&supervisor, &child),
            IssueHeld::OpenTarget {
                target: observer.clone(),
                registration: Some(lease.clone()),
            }
        );
        let by_user = issue_held(&Principal::user(), &child);
        assert_eq!(
            by_user,
            IssueHeld::OpenTarget {
                target: observer,
                registration: None,
            }
        );
        let open = HeldFacts {
            target_open: true,
            ..HeldFacts::default()
        };
        assert_eq!(not_held(&by_user, &open), None);
        assert!(
            not_held(&by_user, &HeldFacts::default())
                .unwrap()
                .contains("has ended")
        );
        // The supervisor's needs its registration too.
        let held = issue_held(&supervisor, &child);
        assert!(not_held(&held, &open).unwrap().contains("not registered"));
        let live = HeldFacts {
            registration_expired: Some(false),
            ..open
        };
        assert_eq!(not_held(&held, &live), None);

        for role in [ActorRole::User, ActorRole::Inbox, ActorRole::Supervisor] {
            assert_eq!(telemetry_issue_refusal(role, &child), None, "{role:?}");
            let review =
                Principal::telemetry(&StepTarget::of(TargetKind::ThroughputReview, 2), None);
            assert_eq!(telemetry_issue_refusal(role, &review), None, "{role:?}");
            let run = RunId::new("r1").unwrap();
            let of_run = Principal::telemetry(&StepTarget::run(&run), Some(TaskId::new(1)));
            assert!(telemetry_issue_refusal(role, &of_run).is_some(), "{role:?}");
        }
        let plan_review = Principal::telemetry(&StepTarget::of(TargetKind::PlanReview, 2), None);
        assert!(telemetry_issue_refusal(ActorRole::User, &plan_review).is_some());
        assert_eq!(
            telemetry_issue_refusal(ActorRole::Supervisor, &plan_review),
            None
        );
    }

    /// The telemetry token of a target of no run goes only to the parent
    /// that started it (ADR-t3102-1 decision 2): a command's start to the
    /// principal that recorded it asking from the process it names, a
    /// supervised job's row to the supervisor whose registration it names.
    /// Another principal, another process of the same one, a start that
    /// names no caller and a target with no start are refused.
    #[test]
    fn a_targets_telemetry_token_goes_only_to_the_parent_that_started_it() {
        let lease = LeaseToken::new("l");
        let supervisor = Principal::supervisor(&lease, 7);
        let another = Principal::supervisor(&LeaseToken::new("m"), 8);
        let (user, inbox) = (
            Principal::user(),
            Principal::inbox(SessionProcess {
                pid: 9,
                start: "s".into(),
            }),
        );
        let started = |by: &Principal, pid| TargetStart::Command {
            actor: Some((by.role, by.actor_id.clone())),
            pid: Some(pid),
        };
        let own = |parent: &Principal, process, start: &TargetStart| {
            telemetry_starter_refusal(parent, process, Some(start)).is_none()
        };
        for starter in [&user, &inbox, &supervisor] {
            let start = started(starter, 40);
            assert!(own(starter, Some(40), &start), "{starter:?}");
            // Another command of the same principal, or none read.
            assert!(!own(starter, Some(41), &start), "{starter:?}");
            assert!(!own(starter, None, &start), "{starter:?}");
            for other in [&user, &inbox, &supervisor, &another] {
                if other != starter {
                    assert!(!own(other, Some(40), &start), "{other:?} {starter:?}");
                }
            }
        }
        for unnamed in [
            TargetStart::Command {
                actor: None,
                pid: Some(40),
            },
            TargetStart::Command {
                actor: Some((ActorRole::User, "user".into())),
                pid: None,
            },
        ] {
            assert!(
                telemetry_starter_refusal(&user, Some(40), Some(&unnamed))
                    .unwrap()
                    .contains("names no caller")
            );
        }
        assert!(
            telemetry_starter_refusal(&user, Some(40), None)
                .unwrap()
                .contains("no record")
        );

        let row = TargetStart::Supervised {
            supervisor: Some(lease.clone()),
        };
        assert!(own(&supervisor, Some(1), &row));
        for other in [&another, &user, &inbox] {
            assert!(!own(other, Some(1), &row), "{other:?}");
        }
        assert!(!own(
            &supervisor,
            Some(1),
            &TargetStart::Supervised { supervisor: None }
        ));

        // A command's step asks from its process only as the connection
        // names it: one naming another command's process asks as none,
        // which no start names.
        let call = |pid| ControlCall {
            command: ControlCommand::Observe,
            task: None,
            yes: false,
            pid,
        };
        assert_eq!(call(40).process(Some(40)), Some(40));
        assert_eq!(call(40).process(Some(41)), None);
        assert_eq!(call(40).process(None), None);
        let start = started(&user, 40);
        assert!(own(&user, call(40).process(Some(40)), &start));
        assert!(!own(&user, call(40).process(Some(41)), &start));

        // A token the target holds already is replaced by none: neither
        // another command of the same principal nor its starter asking
        // again; one not the starter's is refused as such first.
        for starter in [&user, &inbox] {
            let start = started(starter, 40);
            assert_eq!(
                telemetry_target_refusal(starter, Some(40), Some(&start), false),
                None
            );
            assert!(
                telemetry_target_refusal(starter, Some(40), Some(&start), true)
                    .unwrap()
                    .contains("holds its token already")
            );
            for held in [false, true] {
                assert!(
                    telemetry_target_refusal(starter, Some(41), Some(&start), held)
                        .unwrap()
                        .contains("another caller"),
                    "{starter:?} {held}"
                );
            }
        }
    }

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
    fn an_integrator_principal_lands_one_run_under_one_lease() {
        use crate::domain::session_step::TargetKind;
        let run = RunId::new("r1").unwrap();
        let lease = LeaseToken::new("slot");
        let landing = Principal::integrator(&run, &lease, 9);
        assert!(landing.check().is_ok());
        assert_eq!(landing.landing_run(), Some(run.clone()));
        // Its actor is the landing's events', and the run is its target, not
        // the run its token would end with.
        let actor = landing.actor();
        assert_eq!(actor.actor_id(), "integrator:9");
        assert_eq!(actor.run_id(), None);
        assert_eq!(landing.credential_key(), "integrator:9@r1");
        let mut no_lease = landing.clone();
        no_lease.registration = None;
        assert!(no_lease.check().is_err());
        let mut on_planner = landing.clone();
        on_planner.target = Some(StepTarget::of(TargetKind::Planner, 3));
        assert!(on_planner.check().is_err());
        let mut with_run = landing.clone();
        with_run.run_id = Some(run);
        assert!(with_run.check().is_err());
        let mut as_supervisor = landing;
        as_supervisor.role = ActorRole::Supervisor;
        assert!(as_supervisor.check().is_err());
    }

    /// The sweep revokes an ended landing's integrator's and an ended inbox
    /// session's tokens as it does a worker's, and never a supervisor's
    /// sign-in nor a key of a form no principal has.
    #[test]
    fn a_supervisor_revokes_any_token_but_its_own_sign_in() {
        let run = RunId::new("r1").unwrap();
        let lease = LeaseToken::new("slot");
        let inbox = Principal::inbox(SessionProcess {
            pid: 42,
            start: "Thu Oct 23 10:00:00 2026".to_owned(),
        });
        for principal in [
            Principal::integrator(&run, &lease, 9),
            inbox,
            Principal::planner(7),
        ] {
            let key = principal.credential_key();
            assert!(supervisor_revokes(&key), "{key}");
        }
        assert!(supervisor_revokes("worker:r1"));
        let sign_in = Principal::supervisor(&lease, 9).credential_key();
        assert!(!supervisor_revokes(&sign_in), "{sign_in}");
        assert!(!supervisor_revokes("supervisor:9"));
        assert!(!supervisor_revokes("worker:r1@slot"));
    }

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
    /// a worker's by their actor ids; a supervisor's sign-in by its
    /// registration's, and never a key that only names a supervisor.
    #[test]
    fn the_sweep_revokes_by_the_key_of_the_session_and_of_the_registration() {
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
        // A landing's integrator's, by its run's key, once its landing ended.
        let integrator =
            Principal::integrator(&RunId::new("r1").unwrap(), &LeaseToken::new("lease"), 42);
        assert_eq!(
            swept_token_key(&integrator).as_deref(),
            Some("integrator:42@r1")
        );
        let supervisor = Principal::supervisor(&LeaseToken::new("lease"), 42);
        assert_eq!(
            swept_token_key(&supervisor).as_deref(),
            Some("supervisor:42@lease")
        );
        let named = Principal {
            actor_id: supervisor.actor_id.clone(),
            ..Principal::inbox(session(7))
        };
        let unregistered = Principal {
            registration: None,
            ..supervisor.clone()
        };
        for refused in [unregistered, named] {
            assert_eq!(swept_token_key(&refused), None, "{refused:?}");
        }
    }

    /// A supervisor's sign-in is revoked at `up`'s or `down`'s request
    /// only once its registration is no longer kept; never another
    /// principal's.
    #[test]
    fn a_supervisor_s_token_is_revoked_on_request_only_once_its_registration_is_gone() {
        let held = |principal: Principal| HeldToken {
            principal,
            issued_by: None,
            issued_at: 0,
        };
        let signed_in = held(Principal::supervisor(&LeaseToken::new("lease"), 42));
        assert_eq!(registration_end_refusal(&signed_in, |_| false), None);
        assert!(registration_end_refusal(&signed_in, |_| true).is_some());
        let worker = held(Principal::worker(
            &RunId::new("r1").unwrap(),
            TaskId::new(1),
        ));
        assert!(registration_end_refusal(&worker, |_| false).is_some());
    }

    /// A process that had started by a time ended once its pid is gone or
    /// another started on it after then, past a second's slack; a live pid
    /// whose start does not read has not.
    #[test]
    fn a_process_started_by_a_time_ends_with_its_pid_or_another_start_on_it() {
        assert!(started_by_ended(false, None, 100));
        assert!(started_by_ended(true, Some(102), 100));
        assert!(!started_by_ended(true, Some(101), 100));
        assert!(!started_by_ended(true, Some(50), 100));
        assert!(!started_by_ended(true, None, 100));
    }

    /// A supervisor revokes with a prune the token of another's
    /// registration, left behind, and leaves its own for its process's end.
    #[test]
    fn a_prune_revokes_another_registration_s_token_and_not_the_pruning_one_s() {
        let (own, left) = (LeaseToken::new("own"), LeaseToken::new("left"));
        assert!(revoked_with_prune(&own, &left));
        assert!(!revoked_with_prune(&own, &own));
    }

    /// A supervisor's sign-in is left over only once its registration is no
    /// longer kept and the process that asked for it ended: not before the
    /// registration is written (the process runs), and not across an exec
    /// of a handoff (both are kept). One kept with no process that asked
    /// goes by the supervisor's own pid and the issue's time. Any other
    /// principal's is not judged.
    #[test]
    fn a_supervisor_s_token_ends_with_its_registration_and_its_process() {
        let lease = LeaseToken::new("lease");
        let process = SessionProcess {
            pid: 42,
            start: "Thu Oct 23 10:00:00 2026".to_owned(),
        };
        let held = |principal: Principal, process: Option<SessionProcess>| HeldToken {
            principal,
            issued_by: Some(TokenIssuer {
                parent: "user".to_owned(),
                process,
                registration: None,
            }),
            issued_at: 0,
        };
        let signed_in = held(Principal::supervisor(&lease, 42), Some(process.clone()));
        // The asking process's end is `ended`; the supervisor's own pid's,
        // which only a token with no asking process is judged by, never.
        let judge = |held: &HeldToken, registered: bool, ended: bool| {
            supervisor_token_ended(held, |_| registered, |_| ended, |_, _| unreachable!())
        };
        // Pruned by any path, its process gone or another on its pid.
        assert_eq!(judge(&signed_in, false, true), Some(true));
        // Signed in before it registers, or an exec of a handoff.
        assert_eq!(judge(&signed_in, false, false), Some(false));
        assert_eq!(judge(&signed_in, true, false), Some(false));
        // Registered still, whatever its process.
        assert_eq!(judge(&signed_in, true, true), Some(false));
        // No asking process kept (an older binary's): the supervisor's own
        // pid, by the issue's time, tells.
        let older = HeldToken {
            principal: Principal::supervisor(&lease, 42),
            issued_by: None,
            issued_at: 100,
        };
        let unknown = HeldToken {
            issued_at: 100,
            ..held(Principal::supervisor(&lease, 42), None)
        };
        for unasked in [older, unknown] {
            let judge = |registered: bool, ended: bool| {
                supervisor_token_ended(
                    &unasked,
                    |_| registered,
                    |_| unreachable!(),
                    |pid, by| {
                        assert_eq!((pid, by), (42, 100));
                        ended
                    },
                )
            };
            assert_eq!(judge(false, true), Some(true));
            // Its supervisor still starting, or registered still.
            assert_eq!(judge(false, false), Some(false));
            assert_eq!(judge(true, true), Some(false));
        }
        // One that names no pid is kept.
        let unnamed = HeldToken {
            principal: Principal {
                actor_id: "supervisor".to_owned(),
                ..Principal::supervisor(&lease, 42)
            },
            issued_by: None,
            issued_at: 100,
        };
        assert_eq!(
            supervisor_token_ended(&unnamed, |_| false, |_| true, |_, _| true),
            Some(false)
        );
        let worker = held(
            Principal::worker(&RunId::new("r1").unwrap(), TaskId::new(1)),
            Some(process),
        );
        assert_eq!(judge(&worker, false, true), None);
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
            (
                &["ready", "3", "--inherit", "--reason", "x"],
                "ready_inherit",
            ),
            (&["ready", "3"], "ready"),
            (&["integrate", "3"], "control"),
            (&["integrate", "--next"], "control"),
            (&["review", "3"], "control"),
            (&["observe"], "control"),
            (&["observe", "--daily"], "control"),
            (&["throughput-review"], "control"),
            (&["setup-review", "--yes"], "control"),
            (&["up", "--auto-update"], "control"),
            (&["down", "--wait"], "control"),
            (&["inbox"], "control"),
            (&["service", "start"], "control"),
            (&["service", "stop"], "control"),
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
            &["run", "close-workspaces"],
            &["goal"],
            &["finding"],
            &["dependency"],
            &["request"],
            &["supervise"],
            &["service", "serve"],
            &["service", "status"],
        ] {
            assert_eq!(UseCase::of_command(words), None, "{words:?}");
        }
    }
}
