//! The steps of a session (ADR-t1662-1 decision 7, ADR-t1662-2 decision
//! 4, ADR-t1662-3 decisions 8, 10 and 12, docs/design/measurement.md
//! "SSOTとビュー"): a model's turn, a tool's call, a subagent or a wait,
//! as its length and its offset from the session's start, with what it
//! ran and never what it said. The SessionStepStore keeps each
//! `(session_id, seq)` once, with the one target the session is of, and
//! its missing seqs are the session's gaps ([`gaps`]). A step is kept
//! [`RETENTION_DAYS`] after it came in; the seqs removed are kept as
//! ranges, so they are no gap.
//!
//! A session's resource samples ([`ResourceSample`]) are what its executor
//! measured from inside, where the supervisor cannot measure from outside
//! (ADR-t1662-3 decisions 1 and 4), in the common units, kept by
//! `(session_id, seq)` once as the steps are. A session's owner is the
//! target the control side registered it to before its executor reports
//! ([`SessionOwner`]).
//!
//! Nothing here holds a prompt, an output, a command line or tokens
//! (tokens are the Executions' record, ADR-t1486-1).

use super::RunId;
use serde::{Deserialize, Serialize};

/// How long the SessionStepStore keeps a step after it came in
/// (ADR-t1662-2 decision 4).
pub const RETENTION_DAYS: i64 = 90;
/// [`RETENTION_DAYS`] in milliseconds.
pub const RETENTION_MS: i64 = RETENTION_DAYS * 24 * 60 * 60 * 1000;
/// The seq of a session's first step: the seqs from it up to the largest
/// one received that are not are its gaps.
pub const FIRST_SEQ: i64 = 1;

/// The unix milliseconds before which a step that came in is past the
/// retention, at `now_ms`.
pub const fn retained_since(now_ms: i64) -> i64 {
    now_ms.saturating_sub(RETENTION_MS)
}

/// What a step was (ADR-t1662-1 decision 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// A model's turn: from the request to the end of its answer.
    ModelTurn,
    /// A tool's call: from the call to its result.
    ToolCall,
    /// A subagent, from its start to its end.
    Subagent,
    /// A wait the session did not spend on any of the others (a
    /// background task's, a sleep, a rate limit).
    Wait,
}

impl StepKind {
    pub const ALL: [Self; 4] = [Self::ModelTurn, Self::ToolCall, Self::Subagent, Self::Wait];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelTurn => "model_turn",
            Self::ToolCall => "tool_call",
            Self::Subagent => "subagent",
            Self::Wait => "wait",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }
}

/// The kind of the one target a session's steps are of (ADR-t1662-3
/// decision 10): a run, which a worker's session and the jobs and
/// verifications on the run share, or one of the targets of no run, each a
/// session the control side starts an agent in with no run: the runtime's
/// planner, the supervisor's plan review, goal review and eval jobs, and
/// the observation and the throughput review its child commands run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    /// A run, by its id.
    Run,
    /// A planner of the runtime's, by its planner id.
    Planner,
    /// A plan review, by its id.
    PlanReview,
    /// A goal review job, by its id.
    GoalReview,
    /// A throughput review, by the id of its start
    /// (`throughput_review_started`).
    ThroughputReview,
    /// An observation, by the id of its start (`observe_started`).
    Observer,
    /// A job of the eval of an agent, or one that labels a case of a
    /// production set, by its `headless_jobs` id.
    EvalJob,
}

impl TargetKind {
    pub const ALL: [Self; 7] = [
        Self::Run,
        Self::Planner,
        Self::PlanReview,
        Self::GoalReview,
        Self::ThroughputReview,
        Self::Observer,
        Self::EvalJob,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Planner => "planner",
            Self::PlanReview => "plan_review",
            Self::GoalReview => "goal_review",
            Self::ThroughputReview => "throughput_review",
            Self::Observer => "observer",
            Self::EvalJob => "eval_job",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }
}

/// The one target every step of a session is of: a run by its id, or a
/// target of no run by its kind and id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StepTarget {
    pub kind: TargetKind,
    pub id: String,
}

impl StepTarget {
    /// The target of `run`.
    pub fn run(run: &RunId) -> Self {
        Self {
            kind: TargetKind::Run,
            id: run.to_string(),
        }
    }

    /// The target of no run of `kind` by its id `id`.
    pub fn of(kind: TargetKind, id: i64) -> Self {
        Self {
            kind,
            id: id.to_string(),
        }
    }

    /// The run of a run target.
    pub fn run_id(&self) -> Option<RunId> {
        (self.kind == TargetKind::Run)
            .then(|| RunId::new(&self.id).ok())
            .flatten()
    }
}

/// One step as its executor reports it: its number in the session from
/// [`FIRST_SEQ`], its kind, its offset from the session's start and its
/// length (milliseconds), and what it ran, `None` where it has none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub seq: i64,
    pub kind: StepKind,
    pub offset_ms: i64,
    pub duration_ms: i64,
    /// The model of a model's turn.
    pub model: Option<String>,
    /// The name of the tool a tool's call ran.
    pub tool: Option<String>,
    /// The generic shape of the command a tool's call ran (ADR-t1662-3
    /// decision 15), never its text: what makes the shape and checks it
    /// is the executor's and the report's; the store keeps it as given.
    pub command_shape: Option<serde_json::Value>,
    pub exit_code: Option<i64>,
    /// Whether the command ran in the background.
    pub background: Option<bool>,
    /// How many bytes its output was (its text is not kept).
    pub output_bytes: Option<i64>,
}

impl Step {
    /// A step of `kind` at `offset_ms` lasting `duration_ms`, with nothing
    /// it ran.
    pub fn new(seq: i64, kind: StepKind, offset_ms: i64, duration_ms: i64) -> Self {
        Self {
            seq,
            kind,
            offset_ms,
            duration_ms,
            model: None,
            tool: None,
            command_shape: None,
            exit_code: None,
            background: None,
            output_bytes: None,
        }
    }
}

/// Why steps were refused before any was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepError {
    /// A step's seq is before [`FIRST_SEQ`].
    Seq(i64),
    /// A step's offset, length or output is negative.
    Negative(i64),
    /// The session's steps came in under another target, or the session
    /// is registered to another: one session is one target's.
    SessionOfAnotherTarget,
    /// A resource sample's seq is before [`FIRST_SEQ`], or one of its
    /// values is negative or not finite.
    Resource(i64),
}

impl std::fmt::Display for StepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Seq(seq) => write!(f, "step seq {seq} is before {FIRST_SEQ}"),
            Self::Negative(seq) => write!(f, "step {seq} has a negative offset, length or output"),
            Self::SessionOfAnotherTarget => write!(f, "the session's steps are of another target"),
            Self::Resource(seq) => write!(
                f,
                "resource sample {seq} has a seq before {FIRST_SEQ}, or a negative or non-finite value"
            ),
        }
    }
}

impl std::error::Error for StepError {}

/// Refuse `steps` unless each holds together: a seq from [`FIRST_SEQ`] and
/// no negative offset, length or output.
pub fn check(steps: &[Step]) -> Result<(), StepError> {
    for step in steps {
        if step.seq < FIRST_SEQ {
            return Err(StepError::Seq(step.seq));
        }
        if step.offset_ms < 0 || step.duration_ms < 0 || step.output_bytes.is_some_and(|n| n < 0) {
            return Err(StepError::Negative(step.seq));
        }
    }
    Ok(())
}

/// What an executor measured of its session's resources over one window
/// from inside (ADR-t1662-3 decision 4), in the common units: its number
/// in the session's samples from [`FIRST_SEQ`], the window's offset from
/// the session's start and its length (milliseconds), and each value the
/// environment let it measure, `None` where it could not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceSample {
    pub seq: i64,
    pub offset_ms: i64,
    pub duration_ms: i64,
    /// CPU time spent in the window, in seconds.
    pub cpu_seconds: Option<f64>,
    /// The CPU limit, in cores.
    pub cpu_limit: Option<f64>,
    /// Time the CPU limit throttled, in seconds.
    pub throttled_seconds: Option<f64>,
    /// The most memory used, in bytes.
    pub memory_max_bytes: Option<i64>,
    /// The memory limit, in bytes.
    pub memory_limit_bytes: Option<i64>,
    /// Bytes read and written.
    pub read_bytes: Option<i64>,
    pub write_bytes: Option<i64>,
    /// The CPU pressure (PSI `some avg10`, a percentage).
    pub cpu_pressure: Option<f64>,
    /// The load average over a minute, where the environment has one.
    pub load_average: Option<f64>,
    /// The node the session ran on.
    pub node_id: Option<String>,
}

impl ResourceSample {
    /// A sample of the window at `offset_ms` lasting `duration_ms`, with no
    /// value measured.
    pub fn new(seq: i64, offset_ms: i64, duration_ms: i64) -> Self {
        Self {
            seq,
            offset_ms,
            duration_ms,
            cpu_seconds: None,
            cpu_limit: None,
            throttled_seconds: None,
            memory_max_bytes: None,
            memory_limit_bytes: None,
            read_bytes: None,
            write_bytes: None,
            cpu_pressure: None,
            load_average: None,
            node_id: None,
        }
    }
}

/// Refuse `samples` unless each holds together: a seq from [`FIRST_SEQ`],
/// and no value negative or, for a real, not finite.
pub fn check_resources(samples: &[ResourceSample]) -> Result<(), StepError> {
    for sample in samples {
        let real = |value: Option<f64>| value.is_none_or(|v| v.is_finite() && v >= 0.0);
        let whole = |value: Option<i64>| value.is_none_or(|v| v >= 0);
        let holds = sample.seq >= FIRST_SEQ
            && sample.offset_ms >= 0
            && sample.duration_ms >= 0
            && [
                sample.cpu_seconds,
                sample.cpu_limit,
                sample.throttled_seconds,
                sample.cpu_pressure,
                sample.load_average,
            ]
            .into_iter()
            .all(real)
            && [
                sample.memory_max_bytes,
                sample.memory_limit_bytes,
                sample.read_bytes,
                sample.write_bytes,
            ]
            .into_iter()
            .all(whole);
        if !holds {
            return Err(StepError::Resource(sample.seq));
        }
    }
    Ok(())
}

/// A resource sample as the store holds it, as [`SessionStep`] a step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionResource {
    pub id: i64,
    pub target: StepTarget,
    pub session_id: String,
    pub received_at: i64,
    pub sample: ResourceSample,
}

/// The target the control side registered a session to before its
/// executor reports: the queue service records a session's steps and
/// samples only for its owner's telemetry principal, so no token takes
/// another target's session first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOwner {
    pub session_id: String,
    pub target: StepTarget,
    pub registered_at: i64,
}

/// A step as the SessionStepStore holds it: its id in the order the rows
/// came in (a cursor reads past it), its target and session, and when it
/// came in (unix milliseconds).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStep {
    pub id: i64,
    pub target: StepTarget,
    pub session_id: String,
    pub received_at: i64,
    pub step: Step,
}

/// What recording a session's steps did: the steps added, and those whose
/// `(session_id, seq)` was held already.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepsRecorded {
    pub added: usize,
    pub duplicates: usize,
}

/// The seqs `from..=to` of a session: steps held or removed by the
/// retention, or a gap (no step of them came in, below the largest seq).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqRange {
    pub from: i64,
    pub to: i64,
}

impl SeqRange {
    /// How many seqs it spans.
    pub const fn count(self) -> i64 {
        self.to - self.from + 1
    }

    pub const fn contains(self, seq: i64) -> bool {
        self.from <= seq && seq <= self.to
    }
}

/// The ascending, distinct `seqs` as runs of consecutive seqs.
pub fn ranges(seqs: impl IntoIterator<Item = i64>) -> Vec<SeqRange> {
    let mut ranges: Vec<SeqRange> = Vec::new();
    for seq in seqs {
        match ranges.last_mut() {
            Some(last) if last.to.checked_add(1) == Some(seq) => last.to = seq,
            _ => ranges.push(SeqRange { from: seq, to: seq }),
        }
    }
    ranges
}

/// The gaps of a session: each run of seqs from [`FIRST_SEQ`] to the
/// largest one `covered` holds that no range of it holds. `covered` is the
/// steps held and the seqs the retention removed, so a removed step is no
/// gap and a seq that never came in stays one, whatever order they came
/// in. Left as gaps, never filled (ADR-t1662-2 decision 4). Safe up to
/// `i64::MAX`.
pub fn gaps(covered: impl IntoIterator<Item = SeqRange>) -> Vec<SeqRange> {
    let mut covered: Vec<SeqRange> = covered.into_iter().collect();
    covered.sort_by_key(|range| range.from);
    let mut gaps = Vec::new();
    let mut next = FIRST_SEQ;
    for range in covered {
        if range.from > next {
            gaps.push(SeqRange {
                from: next,
                to: range.from - 1,
            });
        }
        next = next.max(range.to.saturating_add(1));
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(seqs: &[i64]) -> Vec<SeqRange> {
        ranges(seqs.iter().copied())
    }

    #[test]
    fn ranges_are_the_runs_of_consecutive_seqs() {
        assert_eq!(ranges([]), vec![]);
        assert_eq!(
            ranges([1, 2, 3, 5, 7, 8]),
            vec![
                SeqRange { from: 1, to: 3 },
                SeqRange { from: 5, to: 5 },
                SeqRange { from: 7, to: 8 },
            ]
        );
        assert_eq!(
            ranges([i64::MAX - 1, i64::MAX]),
            vec![SeqRange {
                from: i64::MAX - 1,
                to: i64::MAX
            }]
        );
        let range = SeqRange { from: 4, to: 5 };
        assert_eq!(range.count(), 2);
        assert!(range.contains(4) && range.contains(5));
        assert!(!range.contains(3) && !range.contains(6));
    }

    #[test]
    fn gaps_are_the_runs_of_missing_seqs_up_to_the_largest() {
        assert_eq!(gaps([]), vec![]);
        assert_eq!(gaps(held(&[1, 2, 3])), vec![]);
        assert_eq!(
            gaps(held(&[2, 3, 6, 7, 9])),
            vec![
                SeqRange { from: 1, to: 1 },
                SeqRange { from: 4, to: 5 },
                SeqRange { from: 8, to: 8 },
            ]
        );
    }

    #[test]
    fn removed_seqs_are_no_gap_and_seqs_never_received_still_are() {
        // Seq 5 came in first and was removed; 1 and 6 came in later.
        let removed = SeqRange { from: 5, to: 5 };
        let covered = held(&[1, 6]).into_iter().chain([removed]);
        assert_eq!(gaps(covered), vec![SeqRange { from: 2, to: 4 }]);
        // A removed range above every held seq still counts up to it.
        let covered = held(&[1]).into_iter().chain([SeqRange { from: 3, to: 4 }]);
        assert_eq!(gaps(covered), vec![SeqRange { from: 2, to: 2 }]);
        // Ranges in any order, overlapping or not.
        let covered = [SeqRange { from: 4, to: 6 }, SeqRange { from: 1, to: 5 }];
        assert_eq!(gaps(covered), vec![]);
    }

    #[test]
    fn gaps_hold_at_the_largest_seq() {
        assert_eq!(
            gaps(held(&[1, i64::MAX])),
            vec![SeqRange {
                from: 2,
                to: i64::MAX - 1
            }]
        );
        let at_max = SeqRange {
            from: i64::MAX,
            to: i64::MAX,
        };
        assert_eq!(
            gaps([at_max, at_max]),
            vec![SeqRange {
                from: 1,
                to: i64::MAX - 1
            }]
        );
        assert_eq!(
            SeqRange {
                from: 1,
                to: i64::MAX - 1
            }
            .count(),
            i64::MAX - 1
        );
    }

    #[test]
    fn steps_refuse_a_seq_before_the_first_and_negative_lengths() {
        let ok = Step::new(1, StepKind::Wait, 0, 0);
        assert_eq!(check(std::slice::from_ref(&ok)), Ok(()));
        assert_eq!(
            check(&[Step::new(0, StepKind::Wait, 0, 0)]),
            Err(StepError::Seq(0))
        );
        assert_eq!(
            check(&[ok.clone(), Step::new(2, StepKind::ToolCall, -1, 0)]),
            Err(StepError::Negative(2))
        );
        assert_eq!(
            check(&[Step::new(3, StepKind::ToolCall, 0, -5)]),
            Err(StepError::Negative(3))
        );
        let mut output = Step::new(4, StepKind::ToolCall, 0, 0);
        output.output_bytes = Some(-1);
        assert_eq!(check(&[output]), Err(StepError::Negative(4)));
    }

    #[test]
    fn kinds_read_back_as_written() {
        for kind in StepKind::ALL {
            assert_eq!(StepKind::parse(kind.as_str()), Some(kind));
        }
        for kind in TargetKind::ALL {
            assert_eq!(TargetKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(StepKind::parse("prompt"), None);
        assert_eq!(TargetKind::parse("task"), None);
    }

    #[test]
    fn a_run_target_names_its_run_and_others_none() {
        let run = RunId::new("r-1").unwrap();
        let target = StepTarget::run(&run);
        assert_eq!(target.kind, TargetKind::Run);
        assert_eq!(target.run_id(), Some(run));
        let planner = StepTarget {
            kind: TargetKind::Planner,
            id: "7".to_owned(),
        };
        assert_eq!(planner.run_id(), None);
    }

    #[test]
    fn the_retention_starts_ninety_days_before_now() {
        let day = 24 * 60 * 60 * 1000;
        assert_eq!(retained_since(100 * day), 10 * day);
        assert_eq!(retained_since(i64::MIN), i64::MIN);
    }
}
