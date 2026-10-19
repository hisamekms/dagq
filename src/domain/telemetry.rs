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

use super::RunId;
use super::queue_service::MAX_REQUEST_BYTES;
use super::session_step::{
    self, ResourceSample, Step, StepError, StepTarget, StepsRecorded, TargetKind,
};

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

#[cfg(test)]
mod tests {
    use super::*;
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
}
