//! The record of `stats`' judgments of now (ADR-t1662-1, ADR-t1662-2
//! decision 5; docs/design/measurement.md "今の判定の記録"): the slot
//! alert, the running alerts and the workspace check, which `stats` judges
//! on inputs outside the events (the slots, the run directories' markers,
//! the wrappers' processes, the thresholds).
//!
//! A supervisor observes the whole queue's inputs on its pass and keeps a
//! stream of records of its own ([`LiveRecorder`]): a baseline of every
//! input after it starts, an input's new value only when it changed (each
//! moving the stream's input version on by one), the start and the end of
//! each alert only when the set of alert keys changed, and a reach every
//! [`REACH_INTERVAL_SECS`] (at once on a failure and on the return from
//! one) with the version it confirmed. [`fold`] takes back from the events
//! alone, at the end of a window, the inputs of the stream with the latest
//! successful observation and judges them with the same functions as
//! `stats` ([`judge`]); the alerts' starts and ends are not used for the
//! values. An input holds only discrete times (when a marker was written,
//! when a task started), never a length that grows with the time.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    EventId, EventKind, RunEvent, RunId, RunStatus, TaskId, claim_defer, claim_hold,
    stall::{BackgroundTask, StallConfig},
    stats::{
        Alert, LiveRun, RunningAlert, SlotSnapshot, StallConfigReport, WorkspaceCheck,
        judge_running_alerts, slot_alert, timestamp_millis, workspace_check,
    },
    supervisor_life::supervisor_life_end,
};

/// The least time between two observations of a supervisor: each reads
/// every event, so not on every pass.
pub const OBSERVE_INTERVAL_SECS: i64 = 60;
/// How often a supervisor whose inputs do not change records a reach.
pub const REACH_INTERVAL_SECS: i64 = 300;
/// How long after its last successful observation a stream still stands
/// for the queue at the end of a window: three reaches.
pub const GRACE_SECS: i64 = 3 * REACH_INTERVAL_SECS;

/// Everything the judgments of now read besides the events: the queue's
/// slots, the thresholds, the runs not finished yet (in the queue's order),
/// the `running_outlier` threshold of each that has one, and each run's
/// position in the queue's list of every run, which keeps the order of a
/// run that comes back unfinished (a triage's resume) where `stats` puts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveInputs {
    pub slots: SlotSnapshot,
    pub config: StallConfigReport,
    pub runs: Vec<LiveRun>,
    pub outliers: HashMap<RunId, i64>,
    pub positions: HashMap<RunId, usize>,
}

/// The judgments of now: the slot alert (at most one), the running alerts
/// and the workspace check, as `stats` lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judged {
    pub slot_alerts: Vec<Alert>,
    pub running_alerts: Vec<RunningAlert>,
    pub workspace_check: WorkspaceCheck,
}

/// Judge `inputs` at `now` (unix seconds) with `events` (ascending id),
/// by the functions `stats` judges with.
pub fn judge(events: &[RunEvent], now: i64, inputs: &LiveInputs) -> Judged {
    let (first, last) = (EventId::new(0), EventId::new(i64::MAX));
    let held = claim_hold::claim_holds(events, first, last, now * 1000, |_| true)
        .held
        .is_some();
    let deferred = claim_defer::claim_deferrals(events, first, last, now * 1000, |_| true)
        .deferred
        .len();
    Judged {
        slot_alerts: slot_alert(inputs.slots, held, deferred)
            .into_iter()
            .collect(),
        running_alerts: judge_running_alerts(
            events,
            now,
            &inputs.runs,
            &inputs.config.config,
            &inputs.outliers,
        ),
        workspace_check: workspace_check(events, &inputs.runs),
    }
}

/// What tells one alert from another: the judgment (`slots` or
/// `running`), the alert's kind, the run it is about and its reason. Who
/// leases the run is not part of it, so a lease that moves changes no key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AlertKey {
    pub judgment: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Judged {
    /// The keys of the alerts judged.
    pub fn keys(&self) -> BTreeSet<AlertKey> {
        let slots = self.slot_alerts.iter().map(|alert| AlertKey {
            judgment: "slots".into(),
            kind: alert.kind.into(),
            run_id: alert.run_id.as_ref().map(|run| run.as_str().to_owned()),
            reason: None,
        });
        let running = self.running_alerts.iter().map(|alert| AlertKey {
            judgment: "running".into(),
            kind: alert.kind.into(),
            run_id: alert.run_id.as_ref().map(|run| run.as_str().to_owned()),
            reason: alert.reason.map(str::to_owned),
        });
        slots.chain(running).collect()
    }
}

/// One run's input as recorded: [`LiveRun`], its `running_outlier`
/// threshold and its position in the queue's list of runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RunInput {
    run_id: String,
    task_id: TaskId,
    status: RunStatus,
    workspace_id: Option<String>,
    idle: Option<(i64, Vec<BackgroundTask>)>,
    receipt: Option<i64>,
    input: Option<i64>,
    background_since: Option<i64>,
    background_alive: Option<bool>,
    outlier_threshold: Option<i64>,
    #[serde(default)]
    position: Option<usize>,
}

impl RunInput {
    fn of(run: &LiveRun, inputs: &LiveInputs) -> Self {
        Self {
            run_id: run.run_id.as_str().to_owned(),
            task_id: run.task_id,
            status: run.status,
            workspace_id: run.workspace_id.clone(),
            idle: run.idle.clone(),
            receipt: run.receipt,
            input: run.input,
            background_since: run.background_since,
            background_alive: run.background_alive,
            outlier_threshold: inputs.outliers.get(&run.run_id).copied(),
            position: inputs.positions.get(&run.run_id).copied(),
        }
    }

    fn live(self) -> Option<(LiveRun, Option<i64>, Option<usize>)> {
        let run = LiveRun {
            run_id: RunId::new(self.run_id).ok()?,
            task_id: self.task_id,
            status: self.status,
            workspace_id: self.workspace_id,
            idle: self.idle,
            receipt: self.receipt,
            input: self.input,
            background_since: self.background_since,
            background_alive: self.background_alive,
        };
        Some((run, self.outlier_threshold, self.position))
    }
}

/// The thresholds as recorded: their seconds by key, and where they came
/// from.
fn config_value(config: &StallConfigReport) -> Value {
    let mut value = serde_json::to_value(config.config).unwrap_or_else(|_| json!({}));
    value["source"] = json!(config.source);
    value
}

fn config_of(value: &Value) -> StallConfigReport {
    let mut config = StallConfig::default();
    for key in StallConfig::KEYS {
        if let Some(secs) = value.get(key).and_then(Value::as_i64) {
            config.set(key, secs);
        }
    }
    let source = match value.get("source").and_then(Value::as_str) {
        Some("supervisor") => "supervisor",
        Some("file") => "file",
        _ => "default",
    };
    StallConfigReport { config, source }
}

/// What one input is of: the slots, the thresholds or a run.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Target {
    Slots,
    Config,
    Run(String),
}

/// Every input of `inputs` with its recorded value, the runs in order.
fn targets(inputs: &LiveInputs) -> Vec<(Target, Value)> {
    let mut targets = vec![
        (Target::Slots, json!(inputs.slots)),
        (Target::Config, config_value(&inputs.config)),
    ];
    targets.extend(inputs.runs.iter().map(|run| {
        (
            Target::Run(run.run_id.as_str().to_owned()),
            json!(RunInput::of(run, inputs)),
        )
    }));
    targets
}

/// One observation of a supervisor's pass: the inputs it read with the
/// keys of the alerts judged on them, or why it could not read them (the
/// reads `stats` fails on: the queue, a run directory, a process).
#[derive(Debug, Clone)]
pub enum Observation {
    Seen {
        inputs: Box<LiveInputs>,
        alerts: BTreeSet<AlertKey>,
    },
    Failed {
        reason: String,
    },
}

/// What the supervisor's stream has recorded so far.
#[derive(Debug, Clone)]
struct Stream {
    version: u64,
    values: BTreeMap<Target, Value>,
    open: BTreeSet<AlertKey>,
}

/// One supervisor's stream of records ([`Self::observe`]): what it last
/// recorded of the inputs and the alerts, its failure, and when it last
/// recorded a reach. A new process starts with none, so its first
/// successful observation is a baseline.
#[derive(Debug, Clone, Default)]
pub struct LiveRecorder {
    stream: Option<Stream>,
    failing: Option<String>,
    last_reach_ms: Option<i64>,
}

impl LiveRecorder {
    /// Forget what was recorded (a write failed): the next successful
    /// observation is a baseline again.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The records of the supervisor `supervisor`'s `observation` at
    /// `now_ms`, to write in order:
    /// - the first success: a baseline of every input (version 1) and the
    ///   alerts open;
    /// - a later success: each input whose value changed (version on by
    ///   one each, a run gone as null), the alerts that ended and started,
    ///   and a reach on the return from a failure or once
    ///   [`REACH_INTERVAL_SECS`] passed;
    /// - a failure: a reach that failed when it starts, its reason
    ///   changes or the interval passed.
    pub fn observe(
        &mut self,
        supervisor: &str,
        now_ms: i64,
        observation: Observation,
    ) -> Vec<(EventKind, Value)> {
        let due = self
            .last_reach_ms
            .is_none_or(|last| now_ms - last >= REACH_INTERVAL_SECS * 1000);
        let mut records = Vec::new();
        match observation {
            Observation::Failed { reason } => {
                let version = self.stream.as_ref().map_or(0, |stream| stream.version);
                if self.failing.as_deref() != Some(reason.as_str()) || due {
                    records.push((
                        EventKind::LiveAlertReached,
                        json!({"supervisor": supervisor, "ok": false, "version": version, "reason": reason}),
                    ));
                    self.last_reach_ms = Some(now_ms);
                }
                self.failing = Some(reason);
            }
            Observation::Seen { inputs, alerts } => {
                let values = targets(&inputs);
                let Some(stream) = &mut self.stream else {
                    let runs: Vec<&Value> = values
                        .iter()
                        .filter(|(target, _)| matches!(target, Target::Run(_)))
                        .map(|(_, value)| value)
                        .collect();
                    records.push((
                        EventKind::LiveAlertBaseline,
                        json!({
                            "supervisor": supervisor,
                            "version": 1,
                            "slots": values[0].1,
                            "config": values[1].1,
                            "runs": runs,
                            "open": alerts,
                        }),
                    ));
                    self.stream = Some(Stream {
                        version: 1,
                        values: values.into_iter().collect(),
                        open: alerts,
                    });
                    self.failing = None;
                    self.last_reach_ms = Some(now_ms);
                    return records;
                };
                let now: BTreeSet<&Target> = values.iter().map(|(target, _)| target).collect();
                let mut changes: Vec<(Target, Value)> = stream
                    .values
                    .keys()
                    .filter(|target| !now.contains(target))
                    .map(|target| (target.clone(), Value::Null))
                    .collect();
                changes.extend(
                    values
                        .iter()
                        .filter(|(target, value)| stream.values.get(target) != Some(value))
                        .cloned(),
                );
                for (target, value) in changes {
                    stream.version += 1;
                    records.push((
                        EventKind::LiveAlertInputChanged,
                        input_payload(supervisor, stream.version, &target, &value),
                    ));
                    if value.is_null() {
                        stream.values.remove(&target);
                    } else {
                        stream.values.insert(target, value);
                    }
                }
                for key in stream.open.difference(&alerts) {
                    records.push((EventKind::LiveAlertEnded, key_payload(supervisor, key)));
                }
                for key in alerts.difference(&stream.open) {
                    records.push((EventKind::LiveAlertStarted, key_payload(supervisor, key)));
                }
                stream.open = alerts;
                if self.failing.take().is_some() || due {
                    records.push((
                        EventKind::LiveAlertReached,
                        json!({"supervisor": supervisor, "ok": true, "version": stream.version}),
                    ));
                    self.last_reach_ms = Some(now_ms);
                }
            }
        }
        records
    }
}

/// The record of `target`'s new `value` (null: the run is gone) at
/// `version`.
fn input_payload(supervisor: &str, version: u64, target: &Target, value: &Value) -> Value {
    let mut payload = json!({"supervisor": supervisor, "version": version, "value": value});
    match target {
        Target::Slots => payload["target"] = json!("slots"),
        Target::Config => payload["target"] = json!("config"),
        Target::Run(run) => {
            payload["target"] = json!("run");
            payload["run_id"] = json!(run);
        }
    }
    payload
}

fn key_payload(supervisor: &str, key: &AlertKey) -> Value {
    let mut payload = json!(key);
    payload["supervisor"] = json!(supervisor);
    payload
}

/// Why a judgment has no observation at the end of a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Missing {
    /// No stream has a baseline yet: the window is before this record.
    NoBaseline,
    /// Every stream's supervisor has stopped or gone silent
    /// ([`supervisor_life_end`]).
    SupervisorStopped,
    /// The supervisor lives, but its observation has failed since.
    ObservationFailed,
    /// The stream's reach confirmed an input version its records do not
    /// reach: an input's record is missing.
    RecordGap,
    /// The stream's last successful observation is older than
    /// [`GRACE_SECS`], for none of the reasons above.
    Stale,
}

/// How a judgment was observed at the end of a window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ObservationState {
    /// The stream of `supervisor` observed it last, `last_ok_ms`, with its
    /// inputs at `version`.
    Observed {
        supervisor: String,
        version: u64,
        last_ok_ms: i64,
    },
    /// Nothing stands for the queue: `reason`, with the latest stream's
    /// version and last successful observation when there is one.
    Missing {
        reason: Missing,
        version: Option<u64>,
        last_ok_ms: Option<i64>,
    },
}

/// One judgment folded at the end of a window: its value (`None` when
/// it was not observed) and how it was observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folded<T> {
    pub value: Option<T>,
    pub state: ObservationState,
}

/// The three judgments folded at the end of a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldedJudgments {
    pub slot_alerts: Folded<Vec<Alert>>,
    pub running_alerts: Folded<Vec<RunningAlert>>,
    pub workspace_check: Folded<WorkspaceCheck>,
}

/// One stream as [`fold`] reads it.
#[derive(Debug, Default)]
struct StreamFold {
    slots: Option<Value>,
    config: Option<Value>,
    /// The runs' inputs, in the order they appeared.
    runs: Vec<(String, Value)>,
    baselined: bool,
    version: u64,
    /// An input record out of order: the stream is broken until its next
    /// baseline.
    broken: bool,
    /// The latest version a reach confirmed.
    confirmed: u64,
    /// The time and id of its last successful observation.
    last_ok: Option<(i64, EventId)>,
    failing: bool,
}

impl StreamFold {
    fn gap(&self) -> bool {
        self.broken || self.confirmed > self.version
    }

    fn apply(&mut self, target: &str, run_id: Option<&str>, value: &Value) {
        match (target, run_id) {
            ("slots", _) => self.slots = Some(value.clone()),
            ("config", _) => self.config = Some(value.clone()),
            ("run", Some(run)) => {
                let at = self.runs.iter().position(|(id, _)| id == run);
                match (at, value.is_null()) {
                    (Some(at), true) => {
                        self.runs.remove(at);
                    }
                    (Some(at), false) => self.runs[at].1 = value.clone(),
                    (None, false) => self.runs.push((run.to_owned(), value.clone())),
                    (None, true) => {}
                }
            }
            _ => self.broken = true,
        }
    }

    fn inputs(&self) -> Option<LiveInputs> {
        let slots = serde_json::from_value(self.slots.clone()?).ok()?;
        let config = config_of(self.config.as_ref()?);
        let mut runs = Vec::new();
        let mut outliers = HashMap::new();
        let mut positions = HashMap::new();
        for (_, value) in &self.runs {
            let input: RunInput = serde_json::from_value(value.clone()).ok()?;
            let (run, outlier, position) = input.live()?;
            if let Some(outlier) = outlier {
                outliers.insert(run.run_id.clone(), outlier);
            }
            if let Some(position) = position {
                positions.insert(run.run_id.clone(), position);
            }
            runs.push(run);
        }
        // The queue's order: a run that came back is put where it was, one
        // without a position after the rest in the order it appeared.
        runs.sort_by_key(|run| positions.get(&run.run_id).copied().unwrap_or(usize::MAX));
        Some(LiveInputs {
            slots,
            config,
            runs,
            outliers,
            positions,
        })
    }
}

/// The judgments at `window_end` (unix seconds) from `events` (ascending
/// id) alone: the records of each supervisor's stream written up to the
/// window's end are folded, the stream with the latest successful
/// observation within [`GRACE_SECS`] (on a tie, the later event) is taken,
/// and its inputs are judged at `window_end` ([`judge`]). Streams are not
/// added together: every one observes the whole queue. A stream whose
/// records do not reach the version its reach confirmed is not taken.
/// Reads nothing but `events`.
pub fn fold(events: &[RunEvent], window_end: i64) -> FoldedJudgments {
    let end_ms = window_end * 1000;
    let upto: Vec<RunEvent> = events
        .iter()
        .filter(|event| timestamp_millis(&event.created_at).is_some_and(|at| at <= end_ms))
        .cloned()
        .collect();
    let mut streams: BTreeMap<String, StreamFold> = BTreeMap::new();
    for event in &upto {
        let kind = event.kind.as_str();
        if !kind.starts_with("live_alert_") {
            continue;
        }
        let (Some(supervisor), Some(at)) = (
            event.payload.get("supervisor").and_then(Value::as_str),
            timestamp_millis(&event.created_at),
        ) else {
            continue;
        };
        let stream = streams.entry(supervisor.to_owned()).or_default();
        let version = event.payload.get("version").and_then(Value::as_u64);
        match kind {
            k if k == EventKind::LiveAlertBaseline.as_str() => {
                *stream = StreamFold {
                    slots: event.payload.get("slots").cloned(),
                    config: event.payload.get("config").cloned(),
                    runs: event
                        .payload
                        .get("runs")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|run| {
                            Some((run.get("run_id")?.as_str()?.to_owned(), run.clone()))
                        })
                        .collect(),
                    baselined: true,
                    version: version.unwrap_or(1),
                    broken: false,
                    confirmed: version.unwrap_or(1),
                    last_ok: Some((at, event.id)),
                    failing: false,
                };
            }
            k if k == EventKind::LiveAlertInputChanged.as_str() => {
                if !stream.baselined {
                    continue;
                }
                match version {
                    Some(version) if version == stream.version + 1 && !stream.broken => {
                        stream.apply(
                            event
                                .payload
                                .get("target")
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                            event.payload.get("run_id").and_then(Value::as_str),
                            event.payload.get("value").unwrap_or(&Value::Null),
                        );
                        stream.version = version;
                    }
                    _ => {
                        stream.broken = true;
                        stream.version = stream.version.max(version.unwrap_or(0));
                    }
                }
                stream.last_ok = Some((at, event.id));
            }
            k if k == EventKind::LiveAlertStarted.as_str()
                || k == EventKind::LiveAlertEnded.as_str() =>
            {
                if stream.baselined {
                    stream.last_ok = Some((at, event.id));
                }
            }
            k if k == EventKind::LiveAlertReached.as_str() => {
                if event.payload.get("ok").and_then(Value::as_bool) == Some(true) {
                    stream.failing = false;
                    stream.confirmed = stream.confirmed.max(version.unwrap_or(0));
                    if stream.baselined {
                        stream.last_ok = Some((at, event.id));
                    }
                } else {
                    stream.failing = true;
                }
            }
            _ => {}
        }
    }
    let within = |stream: &StreamFold| {
        stream
            .last_ok
            .is_some_and(|(at, _)| end_ms - at <= GRACE_SECS * 1000)
    };
    let taken = streams
        .iter()
        .filter(|(_, stream)| stream.baselined && !stream.gap() && within(stream))
        .max_by_key(|(_, stream)| stream.last_ok)
        .and_then(|(supervisor, stream)| Some((supervisor, stream, stream.inputs()?)));
    let (value, state) = match taken {
        Some((supervisor, stream, inputs)) => (
            Some(judge(&upto, window_end, &inputs)),
            ObservationState::Observed {
                supervisor: supervisor.clone(),
                version: stream.version,
                last_ok_ms: stream.last_ok.map_or(0, |(at, _)| at),
            },
        ),
        None => (None, missing(&upto, &streams, end_ms)),
    };
    let state_of = || state.clone();
    match value {
        Some(judged) => FoldedJudgments {
            slot_alerts: Folded {
                value: Some(judged.slot_alerts),
                state: state_of(),
            },
            running_alerts: Folded {
                value: Some(judged.running_alerts),
                state: state_of(),
            },
            workspace_check: Folded {
                value: Some(judged.workspace_check),
                state,
            },
        },
        None => FoldedJudgments {
            slot_alerts: Folded {
                value: None,
                state: state_of(),
            },
            running_alerts: Folded {
                value: None,
                state: state_of(),
            },
            workspace_check: Folded { value: None, state },
        },
    }
}

/// Why no stream stands for the queue at `end_ms`: the reason of the
/// stream with the latest successful observation.
fn missing(
    events: &[RunEvent],
    streams: &BTreeMap<String, StreamFold>,
    end_ms: i64,
) -> ObservationState {
    let Some(latest) = streams
        .values()
        .filter(|stream| stream.baselined)
        .max_by_key(|stream| stream.last_ok)
    else {
        return ObservationState::Missing {
            reason: Missing::NoBaseline,
            version: None,
            last_ok_ms: None,
        };
    };
    let stopped = streams
        .keys()
        .all(|supervisor| supervisor_life_end(events, supervisor, end_ms).is_some());
    let reason = if latest.gap() {
        Missing::RecordGap
    } else if stopped {
        Missing::SupervisorStopped
    } else if latest.failing {
        Missing::ObservationFailed
    } else {
        Missing::Stale
    };
    ObservationState::Missing {
        reason,
        version: Some(latest.version),
        last_ok_ms: latest.last_ok.map(|(at, _)| at),
    }
}

#[cfg(test)]
mod tests;
