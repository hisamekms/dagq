//! What an executor sends of the sessions it runs through the queue
//! service's `telemetry_report` (ADR-t1662-3 decisions 1, 2, 9 and 10,
//! docs/design/measurement.md "送る口"): one session's steps, and the
//! resources its executor measured from inside where the supervisor
//! cannot, of the one target its telemetry principal was issued for. The
//! params carry a version of their own ([`TELEMETRY_VERSION`]): a version
//! the service does not read is refused, and a field it does not know is
//! dropped ([`read`]). A report is bounded by its size as JSON
//! ([`MAX_REPORT_BYTES`]), so the largest one fits the service's request
//! line whatever its text escapes to, and by its counts and lengths, so
//! that what [`REPORT_INTERVAL_SECS`] holds goes in one.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::queue_service::MAX_REQUEST_BYTES;
use super::session_step::{
    self, ResourceSample, Step, StepError, StepTarget, StepsRecorded, TargetKind,
};
use super::{DomainError, LeaseToken, RunId};

/// The version of `telemetry_report`'s params this binary sends. A change
/// a service of the version before must not read in part raises it; a
/// field added beside the others does not, as a service drops the fields
/// it does not know.
pub const TELEMETRY_VERSION: u32 = 1;
/// The oldest version of `telemetry_report`'s params the service reads.
pub const MIN_TELEMETRY_VERSION: u32 = 1;
/// How often an executor sends what its sessions did: the steps of this
/// many seconds go in one report.
pub const REPORT_INTERVAL_SECS: u64 = 30;
/// The most steps one report carries. A backlog (after a service that did
/// not answer) goes in several reports.
pub const MAX_STEPS_PER_REPORT: usize = 512;
/// The most resource samples one report carries.
pub const MAX_RESOURCES_PER_REPORT: usize = 64;
/// What a request to the service holds besides its params, at most: its
/// API version, its token, its use case, the keys and the line's end.
pub const REQUEST_ENVELOPE_BYTES: usize = 1024;
/// The largest report, as the JSON the sink sends ([`serde_json::to_vec`]):
/// a report within it, with any envelope, fits the service's request line
/// ([`MAX_REQUEST_BYTES`]).
pub const MAX_REPORT_BYTES: usize = MAX_REQUEST_BYTES - REQUEST_ENVELOPE_BYTES;
/// The longest session id, target id or node id a report names, in bytes.
pub const MAX_ID_BYTES: usize = 128;
/// The longest model or tool a step names, in bytes.
pub const MAX_NAME_BYTES: usize = 128;
/// The longest command shape a step carries, as JSON, in bytes.
pub const MAX_SHAPE_BYTES: usize = 1024;
/// The prefix of the actor id of an executor's telemetry principal.
pub const ACTOR_PREFIX: &str = "executor-telemetry";

/// One `telemetry_report`: the steps and the resource samples of
/// `session_id`, all of `target`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryReport {
    /// The version of these params ([`TELEMETRY_VERSION`]).
    pub version: u32,
    pub target: StepTarget,
    pub session_id: String,
    #[serde(default)]
    pub steps: Vec<Step>,
    /// What the executor measured from inside; none where the supervisor
    /// measures the session from outside.
    #[serde(default)]
    pub resources: Vec<ResourceSample>,
}

impl TelemetryReport {
    /// A report of this binary's version with no resource sample.
    pub fn new(target: StepTarget, session_id: impl Into<String>, steps: Vec<Step>) -> Self {
        Self {
            version: TELEMETRY_VERSION,
            target,
            session_id: session_id.into(),
            steps,
            resources: Vec::new(),
        }
    }

    /// The report with `resources`.
    #[must_use]
    pub fn with_resources(mut self, resources: Vec<ResourceSample>) -> Self {
        self.resources = resources;
        self
    }
}

/// What a report recorded: the steps and the resource samples added, and
/// those held already.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportRecorded {
    pub steps: StepsRecorded,
    pub resources: StepsRecorded,
}

/// Why a report was refused before anything of it was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryError {
    /// The params name no version, or one the service does not read.
    Version(Option<u64>),
    /// The params do not read as a report.
    Unreadable(String),
    /// More steps than [`MAX_STEPS_PER_REPORT`].
    TooManySteps(usize),
    /// More resource samples than [`MAX_RESOURCES_PER_REPORT`].
    TooManyResources(usize),
    /// Larger, as JSON, than [`MAX_REPORT_BYTES`].
    TooLarge(usize),
    /// A field longer than its bound, or an empty or control-character id.
    Field(&'static str),
    /// The target's id is not one of its kind.
    Target,
    /// A step does not hold together ([`session_step::check`]).
    Step(StepError),
}

impl fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Version(Some(version)) => write!(
                f,
                "telemetry_report version {version} is not one of \
                 {MIN_TELEMETRY_VERSION} to {TELEMETRY_VERSION}"
            ),
            Self::Version(None) => f.write_str("telemetry_report names no version"),
            Self::Unreadable(why) => write!(f, "the telemetry_report does not read: {why}"),
            Self::TooManySteps(count) => write!(
                f,
                "a telemetry_report carries at most {MAX_STEPS_PER_REPORT} steps, not {count}"
            ),
            Self::TooManyResources(count) => write!(
                f,
                "a telemetry_report carries at most {MAX_RESOURCES_PER_REPORT} resource samples, \
                 not {count}"
            ),
            Self::TooLarge(bytes) => write!(
                f,
                "a telemetry_report is at most {MAX_REPORT_BYTES} bytes as JSON, not {bytes}"
            ),
            Self::Field(field) => write!(f, "the telemetry_report's {field} is out of its bounds"),
            Self::Target => f.write_str("the telemetry_report's target id is not of its kind"),
            Self::Step(error) => write!(f, "the telemetry_report's {error}"),
        }
    }
}

impl std::error::Error for TelemetryError {}

/// The report `params` carry: its version is read first, and one the
/// service does not read refuses it whatever else it says; a field the
/// report does not know, at any level, is dropped. The report is then
/// checked against its bounds ([`check`]).
pub fn read(params: &Value) -> Result<TelemetryReport, TelemetryError> {
    let version = params.get("version").and_then(Value::as_u64);
    if !version.is_some_and(|version| {
        (u64::from(MIN_TELEMETRY_VERSION)..=u64::from(TELEMETRY_VERSION)).contains(&version)
    }) {
        return Err(TelemetryError::Version(version));
    }
    let report: TelemetryReport = serde_json::from_value(params.clone())
        .map_err(|error| TelemetryError::Unreadable(error.to_string()))?;
    check(&report)?;
    Ok(report)
}

/// Refuse `report` past its bounds: at most [`MAX_REPORT_BYTES`] as JSON,
/// at most [`MAX_STEPS_PER_REPORT`] steps and [`MAX_RESOURCES_PER_REPORT`]
/// resource samples, a session id and a target id of 1 to [`MAX_ID_BYTES`] bytes
/// without a control character, the target's id one of its kind (a run's
/// id, else a positive integer), and each step's model and tool within
/// [`MAX_NAME_BYTES`] and its command shape within [`MAX_SHAPE_BYTES`], and
/// each sample's node id within [`MAX_ID_BYTES`], and the steps and the
/// samples holding together ([`session_step::check`],
/// [`session_step::check_resources`]).
pub fn check(report: &TelemetryReport) -> Result<(), TelemetryError> {
    if report.steps.len() > MAX_STEPS_PER_REPORT {
        return Err(TelemetryError::TooManySteps(report.steps.len()));
    }
    if report.resources.len() > MAX_RESOURCES_PER_REPORT {
        return Err(TelemetryError::TooManyResources(report.resources.len()));
    }
    let bytes = serde_json::to_vec(report)
        .map_err(|error| TelemetryError::Unreadable(error.to_string()))?
        .len();
    if bytes > MAX_REPORT_BYTES {
        return Err(TelemetryError::TooLarge(bytes));
    }
    let id = |text: &str, field| {
        if text.is_empty() || text.len() > MAX_ID_BYTES || text.chars().any(char::is_control) {
            Err(TelemetryError::Field(field))
        } else {
            Ok(())
        }
    };
    id(&report.session_id, "session_id")?;
    id(&report.target.id, "target id")?;
    let of_kind = match report.target.kind {
        TargetKind::Run => RunId::new(&report.target.id).is_ok(),
        _ => report.target.id.parse::<i64>().is_ok_and(|id| id > 0),
    };
    if !of_kind {
        return Err(TelemetryError::Target);
    }
    for step in &report.steps {
        let long = |name: &Option<String>| name.as_ref().is_some_and(|n| n.len() > MAX_NAME_BYTES);
        if long(&step.model) {
            return Err(TelemetryError::Field("model"));
        }
        if long(&step.tool) {
            return Err(TelemetryError::Field("tool"));
        }
        if let Some(shape) = &step.command_shape
            && serde_json::to_vec(shape).map_or(true, |bytes| bytes.len() > MAX_SHAPE_BYTES)
        {
            return Err(TelemetryError::Field("command_shape"));
        }
    }
    if report.resources.iter().any(|sample| {
        sample
            .node_id
            .as_ref()
            .is_some_and(|node| node.len() > MAX_ID_BYTES)
    }) {
        return Err(TelemetryError::Field("node_id"));
    }
    session_step::check(&report.steps).map_err(TelemetryError::Step)?;
    session_step::check_resources(&report.resources).map_err(TelemetryError::Step)
}

/// The actor id of the telemetry principal of `target`:
/// `executor-telemetry:<kind>:<id>`, so the policy tells its target from
/// the token's principal and never from what a caller names.
pub fn actor_id(target: &StepTarget) -> String {
    format!("{ACTOR_PREFIX}:{}:{}", target.kind.as_str(), target.id)
}

/// The sessions of `run_id` its events tell the control side of, each
/// once, in the order they started: the one its worker's session wrapper
/// names at the claim and after each switch or renewal
/// ([`super::turn::session_name`], for a provider whose session the
/// runtime names ahead), and every session id an event of the run names
/// (`session_id`: the thread a provider named itself, as its turn or its
/// job's end recorded it). What the control side registers to the run
/// before or while its executor reports; a name its provider did not use
/// is registered to the run and reported by none.
pub fn run_sessions(run_id: &str, events: &[super::RunEvent]) -> Vec<String> {
    let mut sessions: Vec<String> = (0..=super::provider_switch::session_starts(events))
        .map(|starts| super::turn::session_name(run_id, starts))
        .collect();
    for event in events {
        if let Some(id) = event.payload["session_id"].as_str()
            && !id.is_empty()
            && !sessions.iter().any(|known| known == id)
        {
            sessions.push(id.to_owned());
        }
    }
    sessions
}

string_enum!(
/// Why a telemetry token was revoked (ADR-t1662-3 decision 11), as
/// `telemetry_token_revoked` records it once the revocation succeeded.
TokenRevocation {
    /// A resume issued the run another one.
    Reissued => "reissued",
    /// The run ended ([`run_token_due`]).
    RunEnded => "run_ended",
    /// The supervisor that held the run's lease lost it.
    LeaseLost => "lease_lost",
    /// The job or the command of a target of no run ended, or the process
    /// that ran it is gone ([`target_token_due`]).
    JobEnded => "job_ended",
    /// The runtime's planner was closed (`planner_closed`).
    PlannerClosed => "planner_closed",
});

/// A file a session wrapper kept its target's steps in, as the
/// supervisor's sweep lists it: the target it names, `None` when that
/// cannot be read (a temporary file a write left half done), and how long
/// ago it was last written (milliseconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptStepsFile {
    pub file: std::path::PathBuf,
    pub target: Option<StepTarget>,
    pub age_ms: i64,
}

/// How old a kept file whose target cannot be read is before the sweep
/// removes it: a wrapper writes and renames its file at once, so one that
/// stays unreadable this long is no write in progress.
pub const UNREAD_KEPT_STEPS_MS: i64 = 60 * 60 * 1000;

/// Whether the sweep removes `kept` (ADR-t1662-3 decision 11): its target
/// ended by `ended`, given the file's age (read only for a file that names
/// one), or it names none and is older than [`UNREAD_KEPT_STEPS_MS`]. Not
/// its token decides, so a file written again after the end, or of a run
/// whose token went with a lost lease, goes as well.
pub fn kept_steps_due(kept: &KeptStepsFile, ended: impl FnOnce(&StepTarget, i64) -> bool) -> bool {
    match &kept.target {
        Some(target) => ended(target, kept.age_ms),
        None => kept.age_ms >= UNREAD_KEPT_STEPS_MS,
    }
}

/// Whether recovery may still bring a `failed` or `interrupted` `run` back
/// under its own ID: it is its task's latest run (`latest_of_task`; `None`
/// tells no later run) and its task (`task`, `None`: not in the queue) did
/// not end (`completed`, `canceled`). A retry or a new claim starts a later
/// run, which the triage takes up in its place. A task back to `ready` or
/// `draft` with no later run stays revivable, as a triage already under way
/// may still resume the run. Recovery refuses a resume of a run that is not
/// (checked again in the transaction that resumes it), and the sweep keeps
/// the kept steps of one that is, up to a bound ([`run_ended`]), by this
/// one rule.
pub fn run_revivable(
    run: &RunId,
    latest_of_task: Option<&RunId>,
    task: Option<super::TaskStatus>,
) -> bool {
    latest_of_task.is_none_or(|latest| latest == run)
        && task.is_some_and(|task| !task.is_terminal())
}

/// What the queue holds of a run, for whether its wrapper's kept steps go
/// ([`run_ended`]): its status (`None`: not in the queue), and, for a
/// `failed` or `interrupted` one, whether recovery may still bring it back
/// ([`run_revivable`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeptRun {
    pub status: Option<super::RunStatus>,
    pub revivable: bool,
}

impl KeptRun {
    /// The facts of `run` in `status`, whose task's latest run is
    /// `latest_of_task` and whose task is in `task` (`None`: not in the
    /// queue).
    pub fn of(
        run: &RunId,
        status: Option<super::RunStatus>,
        latest_of_task: Option<&RunId>,
        task: Option<super::TaskStatus>,
    ) -> Self {
        Self {
            status,
            revivable: run_revivable(run, latest_of_task, task),
        }
    }
}

/// How long after its wrapper last wrote it the kept file of a `failed` or
/// `interrupted` run that recovery could still put back is removed all the
/// same: a resume writes it again at its first report, so one this old is
/// of a run nobody took up. Well within the 90 days the store keeps steps;
/// the file holds only a bounded number of unsent steps per session.
pub const REVIVABLE_KEPT_STEPS_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Whether `run`'s wrapper's kept steps, last written `age_ms` ago, are of
/// a run that ended for good ([`kept_steps_due`]): its numbering must
/// outlive the run while recovery can bring it back, or the wrapper of its
/// resume numbers its session from the first seq again and the store,
/// which keeps one step per `(session_id, seq)`, drops the new steps as
/// repeats with no gap to show it. A run not in the queue, `integrated` or
/// `succeeded` ended. A `failed` or `interrupted` one ended once recovery
/// can no longer put it back ([`run_revivable`]), or the file is
/// [`REVIVABLE_KEPT_STEPS_MS`] old. A run in any other status goes on. Its
/// telemetry token ends with the status all the same
/// ([`super::queue_service::run_holds_token`]): a resume is issued a new
/// one.
pub fn run_ended(run: KeptRun, age_ms: i64) -> bool {
    use super::RunStatus;
    match run.status {
        None => true,
        Some(RunStatus::Failed | RunStatus::Interrupted) => {
            !run.revivable || age_ms >= REVIVABLE_KEPT_STEPS_MS
        }
        Some(status) => !super::queue_service::run_holds_token(status),
    }
}

/// How long after its timeout a command of a target of no run (an
/// observation, a throughput review) whose finish the queue does not hold
/// is taken to have ended: the margin of
/// [`super::throughput_review::RUNNING_MS`] over the job's timeout.
pub const COMMAND_MARGIN_MS: i64 = 5 * 60 * 1000;

/// What the queue holds of a target of no run (ADR-t1662-3 decision 11),
/// for whether its telemetry token still holds ([`target_token_due`]):
/// the queue service reads it at each request, and the supervisor's sweep
/// at each pass.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TargetFacts {
    /// The queue has no such target, or records its end: the planner was
    /// closed, the job's row finished, the command's finish was recorded.
    Ended,
    /// A planner of the runtime's still open.
    OpenPlanner,
    /// A job the supervisor watches (a plan review, a goal review, an eval
    /// job) whose row is unfinished: the registration of that supervisor,
    /// `None` once it is gone.
    SupervisedJob { supervisor: Option<Beat> },
    /// A command the supervisor started as a child (an observation, a
    /// throughput review) whose start has no finish: by when it ends at the
    /// latest (unix ms: its start, its timeout and [`COMMAND_MARGIN_MS`]),
    /// and its process, `None` for a start that names none.
    StartedCommand { ends_by_ms: i64, pid: Option<u32> },
}

/// A supervisor's registration as the end of its job is read: its process
/// and how long ago (seconds) it last beat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Beat {
    pub pid: u32,
    pub age_secs: i64,
}

/// Whether the telemetry token of a target of no run of `kind` is due at
/// `now_ms` by what the queue holds of it (`facts`), and why: the target
/// ended (a closed planner is [`TokenRevocation::PlannerClosed`]), or the
/// process that ran it is gone though the queue does not say it ended: the
/// supervisor of a job no longer registered or beating, a command past the
/// time it ends by, or one whose process `alive` says is gone. The service
/// reads only the queue (`alive` true for every process); the supervisor's
/// sweep asks the host too. A token due is refused by the service whether
/// it was revoked yet or not.
pub fn target_token_due(
    kind: TargetKind,
    facts: &TargetFacts,
    now_ms: i64,
    alive: &dyn Fn(u32) -> bool,
) -> Option<TokenRevocation> {
    let ended = match facts {
        TargetFacts::Ended => true,
        TargetFacts::OpenPlanner => false,
        TargetFacts::SupervisedJob { supervisor } => {
            supervisor.is_none_or(|beat| super::heartbeat_stale(alive(beat.pid), beat.age_secs))
        }
        TargetFacts::StartedCommand { ends_by_ms, pid } => {
            now_ms > *ends_by_ms || pid.is_some_and(|pid| !alive(pid))
        }
    };
    ended.then_some(if kind == TargetKind::Planner {
        TokenRevocation::PlannerClosed
    } else {
        TokenRevocation::JobEnded
    })
}

/// A telemetry token the control side holds: its target, and the lease (the
/// supervisor's token) it was issued under, `None` for one issued under no
/// lease. A revocation for a lost lease takes only the tokens issued under
/// that lease, so it never takes the one the supervisor that adopted the run
/// issued since.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HeldTelemetry {
    pub target: StepTarget,
    pub issued_under: Option<LeaseToken>,
}

/// What the sweep reads of the run of a run target's token: its status
/// (`None`: the queue has no such run), the lease on it now, and, with no
/// lease, whether the supervisor the token was issued under still lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTokenState {
    pub status: Option<super::RunStatus>,
    pub lease: Option<LeaseView>,
    pub issuer_alive: bool,
}

/// The lease on a run as the sweep reads it: whose, and whether its
/// supervisor stopped beating ([`super::heartbeat_stale`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseView {
    pub token: LeaseToken,
    pub stale: bool,
}

/// Whether the sweep revokes a run's telemetry token issued under
/// `issued_under`, and why. The run ended or is gone: once the run no
/// longer holds tokens ([`run_holds_token`](super::queue_service::run_holds_token)),
/// as the service refuses them from then on. The lease was lost: the run's
/// lease is another supervisor's, or still the issuer's but stale, or there
/// is none and the issuer no longer lives. A run with no lease whose issuer
/// lives (one waiting to land, say) keeps it, and so does a token issued
/// under no lease while its run holds tokens. A lease lost while its
/// supervisor lives is revoked by that supervisor ([`TokenRevocation::LeaseLost`]).
pub fn run_token_due(
    issued_under: Option<&LeaseToken>,
    state: &RunTokenState,
) -> Option<TokenRevocation> {
    match state.status {
        Some(status) if super::queue_service::run_holds_token(status) => {}
        _ => return Some(TokenRevocation::RunEnded),
    }
    let issuer = issued_under?;
    let lost = match &state.lease {
        Some(lease) => lease.token != *issuer || lease.stale,
        None => !state.issuer_alive,
    };
    lost.then_some(TokenRevocation::LeaseLost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RunStatus;
    use crate::domain::session_step::StepKind;
    use serde_json::json;

    fn run_target() -> StepTarget {
        StepTarget::run(&RunId::new("r1").unwrap())
    }

    #[test]
    fn a_report_reads_back_and_drops_the_fields_it_does_not_know() {
        let report = TelemetryReport::new(
            run_target(),
            "s1",
            vec![Step::new(1, StepKind::ModelTurn, 0, 5)],
        );
        let mut params = serde_json::to_value(&report).unwrap();
        assert_eq!(read(&params), Ok(report.clone()));
        params["later"] = json!({"a": 1});
        params["steps"][0]["later"] = json!(true);
        params["target"]["later"] = json!("x");
        assert_eq!(read(&params), Ok(report));
    }

    #[test]
    fn a_version_the_service_does_not_read_is_refused_first() {
        let mut params =
            serde_json::to_value(TelemetryReport::new(run_target(), "s1", vec![])).unwrap();
        for version in [
            json!(0),
            json!(TELEMETRY_VERSION + 1),
            json!("1"),
            json!(null),
        ] {
            params["version"] = version.clone();
            // Whatever else the params say.
            params["steps"] = json!("not steps");
            assert!(
                matches!(read(&params), Err(TelemetryError::Version(_))),
                "{version}"
            );
        }
        params.as_object_mut().unwrap().remove("version");
        assert_eq!(read(&params), Err(TelemetryError::Version(None)));
        params["version"] = json!(TELEMETRY_VERSION);
        assert!(matches!(read(&params), Err(TelemetryError::Unreadable(_))));
        assert!(
            TelemetryError::Version(Some(9))
                .to_string()
                .contains("version 9")
        );
    }

    #[test]
    fn a_report_past_its_bounds_is_refused() {
        let step = |seq| Step::new(seq, StepKind::ToolCall, 0, 1);
        let at_most = TelemetryReport::new(
            run_target(),
            "s1",
            (1..=MAX_STEPS_PER_REPORT as i64).map(step).collect(),
        );
        assert_eq!(check(&at_most), Ok(()));
        let mut over = at_most.clone();
        over.steps.push(step(MAX_STEPS_PER_REPORT as i64 + 1));
        assert_eq!(
            check(&over),
            Err(TelemetryError::TooManySteps(MAX_STEPS_PER_REPORT + 1))
        );
        let long = "x".repeat(MAX_ID_BYTES + 1);
        let mut bad = at_most.clone();
        for (session, field) in [
            (long.as_str(), "session_id"),
            ("", "session_id"),
            ("a\nb", "session_id"),
        ] {
            bad.session_id = session.to_owned();
            assert_eq!(check(&bad), Err(TelemetryError::Field(field)));
        }
        let mut named = TelemetryReport::new(run_target(), "s1", vec![step(1)]);
        named.steps[0].model = Some("m".repeat(MAX_NAME_BYTES + 1));
        assert_eq!(check(&named), Err(TelemetryError::Field("model")));
        named.steps[0].model = None;
        named.steps[0].tool = Some("t".repeat(MAX_NAME_BYTES + 1));
        assert_eq!(check(&named), Err(TelemetryError::Field("tool")));
        named.steps[0].tool = None;
        named.steps[0].command_shape = Some(json!("s".repeat(MAX_SHAPE_BYTES)));
        assert_eq!(check(&named), Err(TelemetryError::Field("command_shape")));
        named.steps[0].command_shape = None;
        named.steps[0].seq = 0;
        assert_eq!(check(&named), Err(TelemetryError::Step(StepError::Seq(0))));
    }

    #[test]
    fn a_target_id_is_one_of_its_kind() {
        let report = |kind, id: &str| {
            TelemetryReport::new(
                StepTarget {
                    kind,
                    id: id.to_owned(),
                },
                "s1",
                vec![],
            )
        };
        assert_eq!(check(&report(TargetKind::Planner, "3")), Ok(()));
        assert_eq!(check(&report(TargetKind::Run, "r1")), Ok(()));
        for kind in [
            TargetKind::Planner,
            TargetKind::PlanReview,
            TargetKind::GoalReview,
            TargetKind::ThroughputReview,
            TargetKind::Observer,
            TargetKind::EvalJob,
        ] {
            for id in ["0", "-1", "x"] {
                assert_eq!(check(&report(kind, id)), Err(TelemetryError::Target));
            }
        }
        assert_eq!(
            actor_id(&run_target()),
            "executor-telemetry:run:r1".to_owned()
        );
    }

    /// A report of `target` exactly [`MAX_REPORT_BYTES`] long, of steps
    /// whose model and tool escape to six bytes a character.
    fn largest(target: StepTarget) -> TelemetryReport {
        fn size<T: Serialize>(value: &T) -> usize {
            serde_json::to_vec(value).unwrap().len()
        }
        // Seqs of four digits, so every full step is as long.
        let full = |seq| {
            let mut step = Step::new(seq, StepKind::ToolCall, i64::MAX, i64::MAX);
            step.model = Some("\u{1}".repeat(MAX_NAME_BYTES));
            step.tool = Some("\u{1}".repeat(MAX_NAME_BYTES));
            step.command_shape = Some(json!("x".repeat(MAX_SHAPE_BYTES - 2)));
            step.exit_code = Some(i64::MIN);
            step.background = Some(false);
            step.output_bytes = Some(i64::MAX);
            step
        };
        let mut report = TelemetryReport::new(target, "s".repeat(MAX_ID_BYTES), vec![]);
        let empty = size(&report);
        let one = size(&full(1000)) + 1;
        let mut last = Step {
            model: Some(String::new()),
            tool: Some(String::new()),
            command_shape: Some(json!("")),
            ..full(1000)
        };
        let count = (MAX_REPORT_BYTES - empty - size(&last)) / one;
        report.steps = (0..count).map(|at| full(1000 + at as i64)).collect();
        last.seq = 1000 + count as i64;
        // The last step, grown to the byte: six at a time by its names,
        // then one at a time by its shape.
        let budget = MAX_REPORT_BYTES - empty - count * one;
        let chars = ((budget - size(&last)) / 6).min(2 * MAX_NAME_BYTES);
        last.model = Some("\u{1}".repeat(chars.min(MAX_NAME_BYTES)));
        last.tool = Some("\u{1}".repeat(chars.saturating_sub(MAX_NAME_BYTES)));
        let rest = budget - size(&last);
        assert!(rest + 2 <= MAX_SHAPE_BYTES, "{rest}");
        last.command_shape = Some(json!("x".repeat(rest)));
        report.steps.push(last);
        assert_eq!(size(&report), MAX_REPORT_BYTES);
        report
    }

    /// The bound is the report's size as JSON, so the largest report
    /// within it fits the request line with any envelope, however its
    /// text escapes, and one byte more is refused.
    #[test]
    fn the_largest_report_fits_the_request_line_and_one_byte_more_is_refused() {
        let report = largest(run_target());
        assert!(report.steps.len() <= MAX_STEPS_PER_REPORT);
        assert_eq!(check(&report), Ok(()));
        let request = crate::domain::queue_service::ServiceRequest {
            api_version: u32::MAX,
            token: Some("f".repeat(64)),
            env_role: None,
            queue: None,
            binding: None,
            use_case: crate::domain::queue_service::UseCase::TelemetryReport,
            params: serde_json::to_value(&report).unwrap(),
        };
        // The service reads the line with its end.
        assert!(serde_json::to_vec(&request).unwrap().len() < MAX_REQUEST_BYTES);
        let mut over = report;
        let at = over.steps.len() - 1;
        over.steps[at].command_shape = Some(json!(format!(
            "{}x",
            over.steps[at]
                .command_shape
                .as_ref()
                .unwrap()
                .as_str()
                .unwrap()
        )));
        assert_eq!(
            check(&over),
            Err(TelemetryError::TooLarge(MAX_REPORT_BYTES + 1))
        );
        // Steps within every count and length that escape past the bound.
        let mut step = Step::new(1, StepKind::ToolCall, 0, 1);
        step.model = Some("\u{1}".repeat(MAX_NAME_BYTES));
        step.tool = Some("\u{1}".repeat(MAX_NAME_BYTES));
        step.command_shape = Some(json!("x".repeat(MAX_SHAPE_BYTES - 2)));
        let escaped = TelemetryReport::new(
            run_target(),
            "s1",
            (1..=MAX_STEPS_PER_REPORT as i64)
                .map(|seq| Step {
                    seq,
                    ..step.clone()
                })
                .collect(),
        );
        assert!(matches!(check(&escaped), Err(TelemetryError::TooLarge(_))));
    }

    #[test]
    fn resource_samples_are_bounded_and_hold_together() {
        let mut sample = ResourceSample::new(1, 0, 30_000);
        sample.cpu_seconds = Some(12.5);
        sample.memory_max_bytes = Some(1 << 30);
        sample.node_id = Some("host".into());
        let report =
            TelemetryReport::new(run_target(), "s1", vec![]).with_resources(vec![sample.clone()]);
        assert_eq!(check(&report), Ok(()));
        assert_eq!(
            read(&serde_json::to_value(&report).unwrap()),
            Ok(report.clone())
        );
        let many = report.clone().with_resources(
            (1..=MAX_RESOURCES_PER_REPORT as i64 + 1)
                .map(|seq| ResourceSample::new(seq, 0, 1))
                .collect(),
        );
        assert_eq!(
            check(&many),
            Err(TelemetryError::TooManyResources(
                MAX_RESOURCES_PER_REPORT + 1
            ))
        );
        let mut long = sample.clone();
        long.node_id = Some("n".repeat(MAX_ID_BYTES + 1));
        assert_eq!(
            check(&report.clone().with_resources(vec![long])),
            Err(TelemetryError::Field("node_id"))
        );
        for bad in [
            ResourceSample {
                cpu_seconds: Some(-1.0),
                ..sample.clone()
            },
            ResourceSample {
                cpu_pressure: Some(f64::NAN),
                ..sample.clone()
            },
            ResourceSample {
                read_bytes: Some(-1),
                ..sample.clone()
            },
            ResourceSample {
                seq: 0,
                ..sample.clone()
            },
        ] {
            let seq = bad.seq;
            assert_eq!(
                check(&report.clone().with_resources(vec![bad])),
                Err(TelemetryError::Step(StepError::Resource(seq)))
            );
        }
        // A report without the key reads as one of no sample.
        let mut params =
            serde_json::to_value(TelemetryReport::new(run_target(), "s1", vec![])).unwrap();
        params.as_object_mut().unwrap().remove("resources");
        assert_eq!(read(&params).unwrap().resources, vec![]);
    }

    /// ADR-t1662-3 decision 11: the sweep revokes a run's token once its
    /// run ends or is gone, and once the lease it was issued under was lost
    /// (taken by another supervisor, stale, or gone with its supervisor);
    /// it leaves one whose run and issuer hold it.
    #[test]
    fn a_runs_token_is_due_once_its_run_ends_or_its_lease_was_lost() {
        use crate::domain::RunStatus;
        let a = LeaseToken::new("a");
        let lease = |token: &str, stale: bool| {
            Some(LeaseView {
                token: LeaseToken::new(token),
                stale,
            })
        };
        let state = |status, lease, issuer_alive| RunTokenState {
            status,
            lease,
            issuer_alive,
        };
        for status in [
            RunStatus::Integrated,
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Interrupted,
        ] {
            assert_eq!(
                run_token_due(Some(&a), &state(Some(status), lease("a", false), true)),
                Some(TokenRevocation::RunEnded),
                "{status:?}"
            );
        }
        assert_eq!(
            run_token_due(None, &state(None, None, true)),
            Some(TokenRevocation::RunEnded)
        );
        for status in [RunStatus::Running, RunStatus::AwaitingIntegration] {
            let held = |lease, alive| run_token_due(Some(&a), &state(Some(status), lease, alive));
            assert_eq!(held(lease("a", false), false), None, "{status:?}");
            // No lease (waiting to land): kept while its issuer lives.
            assert_eq!(held(None, true), None, "{status:?}");
            assert_eq!(held(None, false), Some(TokenRevocation::LeaseLost));
            assert_eq!(
                held(lease("a", true), true),
                Some(TokenRevocation::LeaseLost)
            );
            assert_eq!(
                held(lease("b", false), true),
                Some(TokenRevocation::LeaseLost)
            );
            // Issued under no lease: only the run's end ends it.
            assert_eq!(
                run_token_due(None, &state(Some(status), lease("b", true), false)),
                None
            );
        }
        assert_eq!(TokenRevocation::Reissued.as_str(), "reissued");
        assert_eq!(TokenRevocation::LeaseLost.as_str(), "lease_lost");
    }

    /// ADR-t1662-3 decision 11: a target of no run's token is due once the
    /// queue says it ended, or once the process that ran it is gone: a
    /// job's supervisor unregistered, stale or dead, a command past the
    /// time it ends by or whose process is dead. A closed planner's reason
    /// is its own.
    #[test]
    fn a_target_of_no_runs_token_is_due_once_it_ended_or_its_runner_is_gone() {
        use super::super::HEARTBEAT_TIMEOUT_SECS;
        let live = |_: u32| true;
        let dead = |pid: u32| pid != 7;
        let due = |kind, facts: &TargetFacts, alive: &dyn Fn(u32) -> bool| {
            target_token_due(kind, facts, 1_000, alive)
        };
        assert_eq!(
            due(TargetKind::Planner, &TargetFacts::Ended, &live),
            Some(TokenRevocation::PlannerClosed)
        );
        assert_eq!(
            due(TargetKind::Planner, &TargetFacts::OpenPlanner, &live),
            None
        );
        for kind in [
            TargetKind::PlanReview,
            TargetKind::GoalReview,
            TargetKind::EvalJob,
            TargetKind::ThroughputReview,
            TargetKind::Observer,
        ] {
            assert_eq!(
                due(kind, &TargetFacts::Ended, &live),
                Some(TokenRevocation::JobEnded),
                "{kind:?}"
            );
        }
        let job = |supervisor| TargetFacts::SupervisedJob { supervisor };
        let beat = |age_secs| Some(Beat { pid: 7, age_secs });
        assert_eq!(due(TargetKind::PlanReview, &job(beat(1)), &live), None);
        for (facts, alive) in [
            (job(None), &live as &dyn Fn(u32) -> bool),
            (job(beat(HEARTBEAT_TIMEOUT_SECS + 1)), &live),
            (job(beat(1)), &dead),
        ] {
            assert_eq!(
                due(TargetKind::GoalReview, &facts, alive),
                Some(TokenRevocation::JobEnded),
                "{facts:?}"
            );
        }
        let command = |ends_by_ms, pid| TargetFacts::StartedCommand { ends_by_ms, pid };
        assert_eq!(
            due(TargetKind::Observer, &command(1_000, Some(7)), &live),
            None
        );
        assert_eq!(
            due(TargetKind::Observer, &command(1_000, None), &dead),
            None
        );
        for (facts, alive) in [
            (command(999, Some(7)), &live as &dyn Fn(u32) -> bool),
            (command(5_000, Some(7)), &dead),
        ] {
            assert_eq!(
                due(TargetKind::ThroughputReview, &facts, alive),
                Some(TokenRevocation::JobEnded),
                "{facts:?}"
            );
        }
        assert_eq!(TokenRevocation::PlannerClosed.as_str(), "planner_closed");
        assert_eq!(TokenRevocation::JobEnded.as_str(), "job_ended");
    }

    /// A run's sessions: the one its claim names, one more for each switch
    /// and renewal, and each session id its events name (a Codex thread a
    /// turn identified, a job's), each once, the missing thread's null
    /// none.
    #[test]
    fn a_runs_sessions_are_its_named_ones_and_those_its_events_name() {
        use crate::domain::{EventId, EventKind, RunEvent, TaskId, turn::session_name};
        let event = |kind: EventKind, payload: Value| RunEvent {
            id: EventId::new(1),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        };
        assert_eq!(run_sessions("r1", &[]), ["r1"]);
        let events = [
            event(EventKind::RunClaimed, json!({})),
            event(
                EventKind::TurnSessionIdentified,
                json!({"session_id": "thread-1"}),
            ),
            event(EventKind::TurnFinished, json!({"session_id": "thread-1"})),
            event(EventKind::ProviderSwitched, json!({"to": "claude"})),
            event(EventKind::SessionRenewed, json!({"session": "x"})),
            event(
                EventKind::TurnSessionIdentified,
                json!({"session_id": null}),
            ),
            event(EventKind::TurnSessionIdentified, json!({"session_id": ""})),
            event(EventKind::ReviewFinished, json!({"session_id": "thread-2"})),
        ];
        assert_eq!(
            run_sessions("r1", &events),
            [
                "r1".to_owned(),
                session_name("r1", 1),
                session_name("r1", 2),
                "thread-1".to_owned(),
                "thread-2".to_owned(),
            ]
        );
    }

    /// A kept file goes once its target ended, whatever its token; one
    /// that names no target only once it is old, so a write in progress
    /// stays.
    #[test]
    fn a_kept_file_goes_once_its_target_ended_or_unread_and_old() {
        let kept = |target: Option<StepTarget>, age_ms| KeptStepsFile {
            file: "/q/service/steps/a.json".into(),
            target,
            age_ms,
        };
        assert!(kept_steps_due(&kept(Some(run_target()), 0), |_, _| true));
        assert!(!kept_steps_due(
            &kept(Some(run_target()), i64::MAX),
            |_, _| { false }
        ));
        assert!(kept_steps_due(&kept(Some(run_target()), 7), |_, age| age == 7));
        assert!(!kept_steps_due(
            &kept(None, UNREAD_KEPT_STEPS_MS - 1),
            |_, _| true
        ));
        assert!(kept_steps_due(&kept(None, UNREAD_KEPT_STEPS_MS), |_, _| {
            false
        }));
    }

    /// Recovery may bring a failed or interrupted run back while it is its
    /// task's latest run and its task did not end, a task back to ready or
    /// draft among them; not once its task has a later run, or ended or is
    /// gone.
    #[test]
    fn a_run_is_revivable_while_it_is_its_tasks_latest_and_the_task_goes_on() {
        use super::super::TaskStatus;
        let (r1, r2) = (RunId::new("r1").unwrap(), RunId::new("r2").unwrap());
        for task in [
            TaskStatus::InProgress,
            TaskStatus::Ready,
            TaskStatus::Draft,
            TaskStatus::Submitted,
        ] {
            assert!(run_revivable(&r1, Some(&r1), Some(task)), "{task:?}");
            assert!(run_revivable(&r1, None, Some(task)), "{task:?}");
            assert!(!run_revivable(&r1, Some(&r2), Some(task)), "{task:?}");
        }
        for task in [
            Some(TaskStatus::Completed),
            Some(TaskStatus::Canceled),
            None,
        ] {
            assert!(!run_revivable(&r1, Some(&r1), task), "{task:?}");
        }
    }

    /// A run not in the queue, landed or succeeded ended for its kept
    /// steps, whatever its task; a live one goes on; a failed or
    /// interrupted one goes on while recovery can put it back (no later
    /// run of its task, the task not ended, its file younger than the
    /// bound), a task back to `ready` among them, and ended once its task
    /// has a later run or ended, or the file is past the bound. Its token
    /// ends with the status all the same.
    #[test]
    fn a_failed_or_interrupted_runs_kept_steps_stay_while_recovery_can_bring_it_back() {
        use super::super::TaskStatus;
        let (r1, r2) = (RunId::new("r1").unwrap(), RunId::new("r2").unwrap());
        let of = |status, latest: &RunId, task| KeptRun::of(&r1, status, Some(latest), task);
        for task in [TaskStatus::InProgress, TaskStatus::Completed] {
            let facts = |status| of(status, &r1, Some(task));
            assert!(run_ended(facts(None), 0));
            for status in [RunStatus::Integrated, RunStatus::Succeeded] {
                assert!(run_ended(facts(Some(status)), 0), "{status:?}");
            }
            for status in [
                RunStatus::Claimed,
                RunStatus::Starting,
                RunStatus::Running,
                RunStatus::Validating,
                RunStatus::NeedsSession,
                RunStatus::AwaitingIntegration,
                RunStatus::Integrating,
            ] {
                assert!(!run_ended(facts(Some(status)), i64::MAX), "{status:?}");
            }
        }
        for ended in [RunStatus::Failed, RunStatus::Interrupted] {
            let status = Some(ended);
            // The task's latest run, its task not ended: in progress, or
            // back to ready or draft with no later run yet.
            for task in [TaskStatus::InProgress, TaskStatus::Ready, TaskStatus::Draft] {
                let facts = of(status, &r1, Some(task));
                assert!(!run_ended(facts, 0), "{status:?} {task:?}");
                assert!(
                    !run_ended(facts, REVIVABLE_KEPT_STEPS_MS - 1),
                    "{status:?} {task:?}"
                );
                assert!(
                    run_ended(facts, REVIVABLE_KEPT_STEPS_MS),
                    "{status:?} {task:?}"
                );
            }
            // A later run of its task.
            assert!(run_ended(of(status, &r2, Some(TaskStatus::InProgress)), 0));
            // Its task completed, canceled or gone.
            for task in [
                Some(TaskStatus::Completed),
                Some(TaskStatus::Canceled),
                None,
            ] {
                assert!(run_ended(of(status, &r1, task), 0), "{task:?}");
            }
            // Its token ends all the same.
            assert!(!super::super::queue_service::run_holds_token(ended));
        }
    }
}
