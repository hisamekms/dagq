//! What each run was measured under (goal 21, task 197), as `stats` reads
//! it from the events: the versions and load `run_claimed` recorded, the
//! load over its work (`receipt_observed`), its validation
//! (`validation_finished`) and its verification commands
//! (`verification_command` of `integrate`); and the aggregates over them:
//! the runs per version and per load band, and the time each verification
//! command takes; and why the verification commands of `integrate` failed
//! (their `failure`, task 467), per run and per class, with the retries of
//! the commands that failed on the host (task 639) and of the landings
//! whose failed tests were all flaky (task 768).
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

use super::{Intervals, RunStats, intervals, median_f64, tokens::TokenTotals};
use crate::domain::{
    EventId, RunEvent, TaskId,
    measure::{load_band, load_band_order},
};

/// The load average over one interval of a run, and its band (by the mean).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct IntervalLoad {
    pub mean: Option<f64>,
    pub max: Option<f64>,
    pub band: Option<&'static str>,
}

impl IntervalLoad {
    fn new(mean: Option<f64>, max: Option<f64>) -> Option<Self> {
        (mean.is_some() || max.is_some()).then(|| Self {
            mean,
            max,
            band: mean.map(load_band),
        })
    }

    fn of(payload: &Value) -> Option<Self> {
        Self::new(
            payload.get("load_avg_mean").and_then(Value::as_f64),
            payload.get("load_avg_max").and_then(Value::as_f64),
        )
    }
}

/// The load over a run's intervals: `work` (to its first receipt),
/// `validate` (to its first validation) and `verify` (its `integrate`
/// verification commands, the mean weighted by their durations); null for
/// an interval recorded without it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RunLoad {
    pub work: Option<IntervalLoad>,
    pub validate: Option<IntervalLoad>,
    pub verify: Option<IntervalLoad>,
}

/// One run's measures: what its `run_claimed` recorded (null for a run
/// claimed by hand, or before they were recorded), the load over its
/// intervals and its `load_band`: the band of its work's mean load, or of
/// the load at its claim when that is all there is.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RunMeasures {
    pub dagq_version: Option<String>,
    /// The host's Claude Code at the claim; null for a run that worked on
    /// Codex, which is no Claude run (task 892).
    pub claude_version: Option<String>,
    /// The run worked on Codex at some point: claimed there (a move at the
    /// claim included), moved there by a `provider_switched`, or a turn ran
    /// there. Such a run is a Codex run: its model is its Codex turns', it
    /// counts under no Claude version and in no trial group (task 892).
    #[serde(skip)]
    pub on_codex: bool,
    /// The worker's provider the task requested (the claim's
    /// `requested_provider`, else its `provider`) and route (`interactive` /
    /// `headless`, the claim's `worker_mode`) the run was claimed with,
    /// Codex's version when the supervisor ran Codex, and the version of the
    /// provider the claim started on (ADR-t813-2 decision 7); null when
    /// claimed before they were recorded.
    pub provider: Option<String>,
    /// The provider that did the run's work in the end: the `to` of its
    /// last `provider_switched`, else the claim's `provider` (the same as
    /// `provider` for a run that never moved); and how many times it moved
    /// (`provider_switched`, a move at the claim included).
    pub actual_provider: Option<String>,
    pub provider_switches: i64,
    pub route: Option<String>,
    pub codex_version: Option<String>,
    pub provider_version: Option<String>,
    /// Its headless turns: how many finished, how many of them failed, and
    /// their seconds from `turn_started` to `turn_finished`; null for a run
    /// without one.
    pub turns: Option<RunTurns>,
    pub rustc_release: Option<String>,
    pub rustc_host: Option<String>,
    pub claim_parallel: Option<i64>,
    pub claim_slots: Option<i64>,
    pub claim_load_avg: Option<f64>,
    /// The model and effort its worker session was claimed with, and its
    /// group in the trial (ADR-0079 decisions 3 and 4); null when claimed
    /// before they were recorded, or outside the trial for the group. A run
    /// that worked on Codex (`on_codex`) takes the model its first Codex
    /// turn that named one recorded, not the claim's (a Claude step, or a
    /// Claude model it started on before moving), and no group (task 892);
    /// null without one.
    pub worker_model: Option<String>,
    pub worker_effort: Option<String>,
    pub trial_group: Option<String>,
    pub load: RunLoad,
    pub load_band: Option<&'static str>,
    /// The verification commands of its `integrate` attempts that failed,
    /// with the class of the failure (task 467), in order; those recorded
    /// before the class was are left out.
    pub verify_failures: Vec<RunVerifyFailure>,
}

/// The headless turns of a run (ADR-t813-2 decision 7), and the same per
/// provider that ran each turn (its `turn_finished`'s `provider`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RunTurns {
    pub count: i64,
    pub failed: i64,
    pub secs: i64,
    pub by_provider: BTreeMap<String, ProviderTurns>,
}

/// The headless turns one provider ran in a run, with the tokens their
/// `turn_finished` recorded (`sessions` counts the turns that had them;
/// null when none did).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProviderTurns {
    pub count: i64,
    pub failed: i64,
    pub secs: i64,
    pub tokens: Option<TokenTotals>,
}

/// One failed verification command of a run's `integrate`: its attempt,
/// its place in the task's commands, and its `failure`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunVerifyFailure {
    pub attempt: Option<i64>,
    pub index: Option<i64>,
    pub command: Option<String>,
    pub class: String,
    pub evidence: Option<String>,
    /// Whether it failed on the retry of a command that failed on the host
    /// first (task 639).
    pub retry: bool,
}

impl RunVerifyFailure {
    /// The failure a `verification_command` of `integrate` records, if any.
    fn of(event: &RunEvent) -> Option<Self> {
        let payload = &event.payload;
        if event.kind != "verification_command" || payload["phase"] != "integration" {
            return None;
        }
        let failure = &payload["failure"];
        let text = |value: &Value| value.as_str().map(str::to_owned);
        Some(Self {
            attempt: payload["attempt"].as_i64(),
            index: payload["index"].as_i64(),
            command: text(&payload["command"]),
            class: text(&failure["class"])?,
            evidence: text(&failure["evidence"]),
            retry: payload["retry"] == true,
        })
    }
}

/// The measures of one run as its events come in.
#[derive(Debug, Default)]
pub(super) struct MeasureTrack {
    measures: RunMeasures,
    claimed: bool,
    /// The first `receipt_observed` / `validation_finished` was seen: a
    /// later one's load belongs to another interval.
    receipt_seen: bool,
    validated_seen: bool,
    verify_sum: f64,
    verify_weight: f64,
    verify_max: Option<f64>,
    /// When the turn running started (unix milliseconds).
    turn_started: Option<i64>,
    turn_millis: i64,
    turn_millis_by_provider: BTreeMap<String, i64>,
    /// The claim's `provider`, and the `to` of the last `provider_switched`
    /// (which may come before the claim: a move at the claim is recorded
    /// first).
    claim_provider: Option<String>,
    switched_to: Option<String>,
    /// The model the first Codex turn that named one read.
    codex_model: Option<String>,
}

impl MeasureTrack {
    pub(super) fn observe(&mut self, event: &RunEvent) {
        let payload = &event.payload;
        let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_owned);
        let measures = &mut self.measures;
        match event.kind.as_str() {
            "run_claimed" if !self.claimed => {
                self.claimed = true;
                measures.on_codex |= payload["provider"] == "codex";
                measures.dagq_version = text("dagq_version");
                measures.claude_version = text("claude_version");
                self.claim_provider = text("provider");
                measures.provider = text("requested_provider").or_else(|| text("provider"));
                measures.route = text("worker_mode");
                measures.codex_version = text("codex_version");
                measures.provider_version = text("provider_version");
                measures.rustc_release = text("rustc_release");
                measures.rustc_host = text("rustc_host");
                measures.claim_parallel = payload.get("parallel").and_then(Value::as_i64);
                measures.claim_slots = payload.get("slots").and_then(Value::as_i64);
                measures.claim_load_avg = payload.get("load_avg").and_then(Value::as_f64);
                measures.worker_model = text("model");
                measures.worker_effort = text("effort");
                measures.trial_group = text("group");
            }
            "provider_switched" => {
                measures.provider_switches += 1;
                self.switched_to = text("to");
                measures.on_codex |= payload["to"] == "codex";
            }
            "turn_started" => self.turn_started = super::timestamp_millis(&event.created_at),
            "turn_finished" => {
                // A turn recorded without its provider ran on the one the
                // run was on then.
                let provider = text("provider")
                    .or_else(|| self.switched_to.clone())
                    .or_else(|| self.claim_provider.clone())
                    .unwrap_or_else(|| super::asks::UNKNOWN.to_owned());
                if provider == "codex" {
                    measures.on_codex = true;
                    if self.codex_model.is_none() {
                        self.codex_model = text("model");
                    }
                }
                let failed = payload["outcome"] != "succeeded";
                let millis = match (
                    self.turn_started.take(),
                    super::timestamp_millis(&event.created_at),
                ) {
                    (Some(start), Some(end)) => (end - start).max(0),
                    _ => 0,
                };
                self.turn_millis += millis;
                let by_provider = self
                    .turn_millis_by_provider
                    .entry(provider.clone())
                    .or_default();
                *by_provider += millis;
                let turns = measures.turns.get_or_insert_with(RunTurns::default);
                turns.count += 1;
                turns.failed += i64::from(failed);
                turns.secs = self.turn_millis / 1000;
                let one = turns.by_provider.entry(provider).or_default();
                one.count += 1;
                one.failed += i64::from(failed);
                one.secs = *by_provider / 1000;
                if payload["tokens"].is_object() {
                    one.tokens
                        .get_or_insert_with(TokenTotals::default)
                        .add(&payload["tokens"]);
                }
            }
            "receipt_observed" if !self.receipt_seen => {
                self.receipt_seen = true;
                measures.load.work = IntervalLoad::of(payload);
            }
            "validation_finished" if !self.validated_seen => {
                self.validated_seen = true;
                measures.load.validate = IntervalLoad::of(payload);
            }
            "verification_command" if payload["phase"] == "integration" => {
                measures.verify_failures.extend(RunVerifyFailure::of(event));
                let Some(load) = IntervalLoad::of(payload) else {
                    return;
                };
                if let Some(mean) = load.mean {
                    // A command recorded without its duration counts as one second.
                    let weight = payload
                        .get("duration_secs")
                        .and_then(Value::as_f64)
                        .filter(|secs| *secs > 0.0)
                        .unwrap_or(1.0);
                    self.verify_sum += mean * weight;
                    self.verify_weight += weight;
                }
                if let Some(max) = load.max {
                    self.verify_max = Some(self.verify_max.map_or(max, |m| m.max(max)));
                }
            }
            _ => {}
        }
    }

    pub(super) fn finish(mut self) -> RunMeasures {
        self.measures.actual_provider = self.switched_to.take().or(self.claim_provider.take());
        // A run that worked on Codex is a Codex run: not a run of the host's
        // Claude Code, not in the trial, and of the model Codex used rather
        // than the claim's (the step's Claude model, the Claude model it
        // started on, or what a Codex claim before task 892 wrote).
        if self.measures.on_codex {
            self.measures.claude_version = None;
            self.measures.trial_group = None;
            self.measures.worker_model = self.codex_model.take();
        }
        let mean = (self.verify_weight > 0.0)
            .then(|| (self.verify_sum / self.verify_weight * 100.0).round() / 100.0);
        self.measures.load.verify = IntervalLoad::new(mean, self.verify_max);
        self.measures.load_band = self
            .measures
            .load
            .work
            .as_ref()
            .and_then(|work| work.band)
            .or_else(|| self.measures.claim_load_avg.map(load_band));
        self.measures
    }
}

/// The runs of one version (or load band): null for the runs without one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionStats {
    pub version: Option<String>,
    #[serde(flatten)]
    pub intervals: Intervals,
}

/// The runs grouped by what they were claimed with (task 197): the build
/// identifier of `dagq`, Claude Code's version, the host's `rustc`
/// (`<release> <host>`), the worker's provider and route and Codex's
/// version, each by name with the runs without one last.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Versions {
    pub dagq: Vec<VersionStats>,
    pub claude: Vec<VersionStats>,
    pub rustc: Vec<VersionStats>,
    /// By the provider that did the run's work in the end
    /// (`actual_provider`), the route it was claimed with and Codex's
    /// version (ADR-t813-2 decision 7).
    pub provider: Vec<VersionStats>,
    pub route: Vec<VersionStats>,
    pub codex: Vec<VersionStats>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LoadBandStats {
    pub band: Option<&'static str>,
    #[serde(flatten)]
    pub intervals: Intervals,
}

fn grouped<K: Ord + Clone>(
    runs: &[&RunStats],
    key: impl Fn(&RunStats) -> Option<K>,
) -> Vec<(Option<K>, Intervals)> {
    let mut groups: BTreeMap<(bool, Option<K>), Vec<&RunStats>> = BTreeMap::new();
    for run in runs {
        let key = key(run);
        groups.entry((key.is_none(), key)).or_default().push(run);
    }
    groups
        .into_iter()
        .map(|((_, key), runs)| (key, intervals(&runs)))
        .collect()
}

pub(super) fn versions(runs: &[&RunStats]) -> Versions {
    let by = |key: fn(&RunStats) -> Option<String>| {
        grouped(runs, key)
            .into_iter()
            .map(|(version, intervals)| VersionStats { version, intervals })
            .collect()
    };
    Versions {
        dagq: by(|run| run.measures.dagq_version.clone()),
        claude: by(|run| run.measures.claude_version.clone()),
        rustc: by(|run| {
            let measures = &run.measures;
            match (&measures.rustc_release, &measures.rustc_host) {
                (None, None) => None,
                (release, host) => Some(format!(
                    "{} {}",
                    release.as_deref().unwrap_or("unknown"),
                    host.as_deref().unwrap_or("unknown")
                )),
            }
        }),
        provider: by(|run| run.measures.actual_provider.clone()),
        route: by(|run| run.measures.route.clone()),
        codex: by(|run| run.measures.codex_version.clone()),
    }
}

/// The runs per `load_band`, lightest first, the runs without one last.
pub(super) fn load_bands(runs: &[&RunStats]) -> Vec<LoadBandStats> {
    grouped(runs, |run| {
        run.measures
            .load_band
            .map(|band| (load_band_order(band), band))
    })
    .into_iter()
    .map(|(band, intervals)| LoadBandStats {
        band: band.map(|(_, band)| band),
        intervals,
    })
    .collect()
}

/// The `backend_call_failed` of one load band.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BandCount {
    pub band: &'static str,
    pub count: i64,
}

/// Count a failure recorded under `load` in its band of `bands`, kept
/// lightest first.
pub(super) fn count_band(bands: &mut Vec<BandCount>, load: f64) {
    let band = load_band(load);
    match bands.iter_mut().find(|count| count.band == band) {
        Some(count) => count.count += 1,
        None => {
            bands.push(BandCount { band, count: 1 });
            bands.sort_by_key(|count| load_band_order(count.band));
        }
    }
}

/// How long one verification command took in `integrate`, over the
/// commands recorded with their duration in the window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandStats {
    pub command: String,
    pub count: usize,
    /// Of those, the ones that exited non-zero.
    pub failed: usize,
    pub total_secs: f64,
    pub median_secs: Option<f64>,
}

/// The `verification_command` events of `integrate` with `after < id <=
/// upto` whose task `counts` accepts and that recorded their duration:
/// the command, its seconds and whether it exited non-zero, in event order.
fn integration_commands(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> impl Iterator<Item = (&str, f64, bool)> {
    events
        .iter()
        .filter(move |event| {
            event.kind == "verification_command"
                && event.id > after
                && event.id <= upto
                && counts(event.task_id)
                && event.payload["phase"] == "integration"
        })
        .filter_map(|event| {
            Some((
                event.payload["command"].as_str()?,
                event.payload["duration_secs"].as_f64()?,
                event.payload["exit_code"].as_i64() != Some(0),
            ))
        })
}

/// The seconds each verification command of `integrate` took in the same
/// window as [`verification_commands`], per command: what the KPIs'
/// `verify_command.<command>` summarize (ADR-0051 decision 1).
pub fn verification_durations(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> BTreeMap<String, Vec<f64>> {
    let mut by_command: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for (command, secs, _) in integration_commands(events, after, upto, counts) {
        by_command.entry(command.to_owned()).or_default().push(secs);
    }
    by_command
}

/// The `verification_command` events of `integrate` with `after < id <=
/// upto` whose task `counts` accepts, per command, by command.
pub(super) fn verification_commands(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> Vec<CommandStats> {
    let mut by_command: BTreeMap<&str, (Vec<f64>, usize)> = BTreeMap::new();
    for (command, secs, failed) in integration_commands(events, after, upto, counts) {
        let entry = by_command.entry(command).or_default();
        entry.0.push(secs);
        if failed {
            entry.1 += 1;
        }
    }
    by_command
        .into_iter()
        .map(|(command, (mut secs, failed))| CommandStats {
            command: command.to_owned(),
            count: secs.len(),
            failed,
            total_secs: (secs.iter().sum::<f64>() * 1000.0).round() / 1000.0,
            median_secs: median_f64(&mut secs),
        })
        .collect()
}

/// The failed verification commands of `integrate` of one class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailureClassStats {
    pub class: String,
    /// The commands that failed so, retries included.
    pub count: usize,
    /// The runs they belong to.
    pub runs: usize,
    /// The retries of commands that failed so first (a class of the host,
    /// task 639), and how they ended: `passed`, or `failed` again (on the
    /// host or in the code). For `flaky` (task 768), the landings done once
    /// more (`integration_retried`): `passed` when the run landed,
    /// `failed` when it left the landing otherwise; one still landing is
    /// in neither. Zero for the other classes of the code.
    pub retried: usize,
    pub retry_passed: usize,
    pub retry_failed: usize,
}

/// The failed verification commands of `integrate` with `after < id <=
/// upto` whose task `counts` accepts, per class of their `failure` (task
/// 467): the most frequent first, then by name.
pub(super) fn verification_failures(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> Vec<FailureClassStats> {
    #[derive(Default)]
    struct Tally<'a> {
        count: usize,
        runs: BTreeSet<&'a str>,
        retried: usize,
        retry_passed: usize,
        retry_failed: usize,
    }
    let mut by_class: BTreeMap<String, Tally> = BTreeMap::new();
    // The runs landed once more (task 768), by the class that sent them.
    let mut relanding: BTreeMap<&str, String> = BTreeMap::new();
    for event in events
        .iter()
        .filter(|event| event.id > after && event.id <= upto && counts(event.task_id))
    {
        // A retry counts under the class of the failure it retried.
        if let Some(class) = retried_class(event) {
            let entry = by_class.entry(class.to_owned()).or_default();
            entry.retried += 1;
            if event.payload["exit_code"].as_i64() == Some(0) {
                entry.retry_passed += 1;
            } else {
                entry.retry_failed += 1;
            }
        }
        if let Some(run) = &event.run_id {
            match event.kind.as_str() {
                "integration_retried" => {
                    if let Some(class) = event.payload["failure"]["class"].as_str() {
                        by_class.entry(class.to_owned()).or_default().retried += 1;
                        relanding.insert(run.as_str(), class.to_owned());
                    }
                }
                "run_integrated" => {
                    if let Some(class) = relanding.remove(run.as_str()) {
                        by_class.entry(class).or_default().retry_passed += 1;
                    }
                }
                "integration_deferred"
                | "integration_held"
                | "integration_failed"
                | "integration_error" => {
                    if let Some(class) = relanding.remove(run.as_str()) {
                        by_class.entry(class).or_default().retry_failed += 1;
                    }
                }
                _ => (),
            }
        }
        let Some(failure) = RunVerifyFailure::of(event) else {
            continue;
        };
        let entry = by_class.entry(failure.class).or_default();
        entry.count += 1;
        if let Some(run) = &event.run_id {
            entry.runs.insert(run.as_str());
        }
    }
    let mut classes: Vec<FailureClassStats> = by_class
        .into_iter()
        .map(|(class, tally)| FailureClassStats {
            class,
            count: tally.count,
            runs: tally.runs.len(),
            retried: tally.retried,
            retry_passed: tally.retry_passed,
            retry_failed: tally.retry_failed,
        })
        .collect();
    classes.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.class.cmp(&b.class)));
    classes
}

/// The class of the failure a `verification_command` of `integrate`
/// retried (task 639), when it is a retry.
fn retried_class(event: &RunEvent) -> Option<&str> {
    let payload = &event.payload;
    (event.kind == "verification_command"
        && payload["phase"] == "integration"
        && payload["retry"] == true)
        .then(|| payload["retry_of"]["failure"]["class"].as_str())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: "1970-01-01T00:00:00.000Z".to_owned(),
            actor: None,
        }
    }

    /// The durations per command are those of `integrate`'s commands in
    /// the window that recorded one, the failed ones too; the summary
    /// counts the same events.
    #[test]
    fn verification_durations_are_the_commands_of_the_window() {
        let command = |id: i64, command: &str, phase: &str, secs: Value, exit: i64| {
            event(
                id,
                "verification_command",
                json!({"command": command, "phase": phase, "duration_secs": secs, "exit_code": exit}),
            )
        };
        let events = [
            command(1, "a", "integration", json!(5.0), 0),
            command(2, "a", "integration", json!(7.5), 1),
            command(3, "a", "validation", json!(9.0), 0),
            command(4, "b", "integration", Value::Null, 0),
            command(5, "b", "integration", json!(1.0), 0),
        ];
        let durations = verification_durations(&events, EventId::new(0), EventId::new(4), |_| true);
        assert_eq!(durations.len(), 1);
        assert_eq!(durations["a"], vec![5.0, 7.5]);
        let summary = verification_commands(&events, EventId::new(0), EventId::new(5), |_| true);
        assert_eq!(
            (summary[0].count, summary[0].failed, summary[1].count),
            (2, 1, 1)
        );
    }

    /// A failed command of `integrate` with its class is on its run's row
    /// and counted per class, by run; a pass, another phase and a failure
    /// recorded before the class are not.
    #[test]
    fn verification_failures_are_per_run_and_per_class() {
        let command = |id: i64, run: &str, phase: &str, class: Option<&str>| {
            let mut event = event(
                id,
                "verification_command",
                json!({
                    "phase": phase, "attempt": 1, "index": 2, "command": "cargo llvm-cov",
                    "exit_code": if class.is_some() { 1 } else { 0 },
                    "failure": class.map(|class| json!({"class": class, "evidence": format!("{class} line")})),
                }),
            );
            event.run_id = Some(crate::domain::RunId::new(run).unwrap());
            event
        };
        let events = [
            command(1, "a", "integration", Some("disk_full")),
            command(2, "a", "integration", Some("test_failure")),
            command(3, "b", "integration", Some("test_failure")),
            command(4, "b", "integration", None),
            command(5, "b", "recheck", Some("build_error")),
            event(
                6,
                "verification_command",
                json!({"phase": "integration", "exit_code": 1}),
            ),
            command(7, "c", "integration", Some("killed")),
        ];
        let mut track = MeasureTrack::default();
        for event in &events[..2] {
            track.observe(event);
        }
        track.observe(&events[5]);
        assert_eq!(
            track.finish().verify_failures,
            vec![
                RunVerifyFailure {
                    attempt: Some(1),
                    index: Some(2),
                    command: Some("cargo llvm-cov".to_owned()),
                    class: "disk_full".to_owned(),
                    evidence: Some("disk_full line".to_owned()),
                    retry: false,
                },
                RunVerifyFailure {
                    attempt: Some(1),
                    index: Some(2),
                    command: Some("cargo llvm-cov".to_owned()),
                    class: "test_failure".to_owned(),
                    evidence: Some("test_failure line".to_owned()),
                    retry: false,
                },
            ]
        );
        let classes = verification_failures(&events, EventId::new(0), EventId::new(6), |_| true);
        assert_eq!(
            json!(classes),
            json!([
                {"class": "test_failure", "count": 2, "runs": 2, "retried": 0, "retry_passed": 0, "retry_failed": 0},
                {"class": "disk_full", "count": 1, "runs": 1, "retried": 0, "retry_passed": 0, "retry_failed": 0},
            ])
        );
        let none = verification_failures(&events, EventId::new(0), EventId::new(7), |_| false);
        assert!(none.is_empty());
    }

    /// A retry of a command that failed on the host (task 639) counts under
    /// the class it retried, as passed or failed; a failed retry is also a
    /// failure of its own class, marked `retry` on its run's row.
    #[test]
    fn retries_count_under_the_class_they_retried() {
        let retry = |id: i64, retried: &str, class: Option<&str>| {
            let mut event = event(
                id,
                "verification_command",
                json!({
                    "phase": "integration", "attempt": 1, "index": 1, "command": "c",
                    "exit_code": if class.is_some() { 1 } else { 0 },
                    "failure": class.map(|class| json!({"class": class, "evidence": "e"})),
                    "retry": true,
                    "retry_of": {"failure": {"class": retried, "evidence": "e"}, "log_path": "l"},
                }),
            );
            event.run_id = Some(crate::domain::RunId::new("r").unwrap());
            event
        };
        let events = [
            retry(1, "timeout", None),
            retry(2, "timeout", Some("timeout")),
            retry(3, "killed", Some("test_failure")),
        ];
        let classes = verification_failures(&events, EventId::new(0), EventId::new(3), |_| true);
        assert_eq!(
            json!(classes),
            json!([
                {"class": "test_failure", "count": 1, "runs": 1, "retried": 0, "retry_passed": 0, "retry_failed": 0},
                {"class": "timeout", "count": 1, "runs": 1, "retried": 2, "retry_passed": 1, "retry_failed": 1},
                {"class": "killed", "count": 0, "runs": 0, "retried": 1, "retry_passed": 0, "retry_failed": 1},
            ])
        );
        let mut track = MeasureTrack::default();
        track.observe(&events[1]);
        assert!(track.finish().verify_failures[0].retry);
    }

    /// A landing done once more for flaky tests (task 768) counts under
    /// `flaky` as passed when the run landed, failed when it left the
    /// landing otherwise, and in neither while it still lands.
    #[test]
    fn landings_done_once_more_count_under_flaky() {
        let on = |id: i64, run: &str, kind: &str, payload: Value| {
            let mut event = event(id, kind, payload);
            event.run_id = Some(crate::domain::RunId::new(run).unwrap());
            event
        };
        let flaky = || {
            json!({
                "phase": "integration", "attempt": 1, "index": 1, "command": "c", "exit_code": 100,
                "failure": {"class": "flaky", "evidence": "FLKY-FL"},
            })
        };
        let retried = || json!({"code": "verification_flaky", "failure": {"class": "flaky"}});
        let events = [
            on(1, "a", "verification_command", flaky()),
            on(2, "a", "integration_retried", retried()),
            on(3, "a", "run_integrated", json!({})),
            on(4, "b", "verification_command", flaky()),
            on(5, "b", "integration_retried", retried()),
            on(6, "b", "verification_command", flaky()),
            on(7, "b", "integration_deferred", json!({})),
            on(8, "c", "integration_retried", retried()),
            // Not after a landing done once more.
            on(9, "d", "run_integrated", json!({})),
        ];
        let classes = verification_failures(&events, EventId::new(0), EventId::new(9), |_| true);
        assert_eq!(
            json!(classes),
            json!([
                {"class": "flaky", "count": 3, "runs": 2, "retried": 3, "retry_passed": 1, "retry_failed": 1},
            ])
        );
    }

    /// Task 892: a run that worked on Codex (claimed there or moved there)
    /// counts under no Claude version and in no trial group, and its model
    /// is the first one its Codex turns read, not the claim's step (nor the
    /// Claude model and group a claim recorded before, nor the Claude model
    /// it started on); a Claude run keeps its claim's.
    #[test]
    fn a_codex_run_has_the_model_its_turns_read() {
        let mut track = MeasureTrack::default();
        for event in [
            event(
                1,
                "run_claimed",
                json!({"provider": "codex", "requested_provider": "claude", "worker_mode": "headless",
                       "claude_version": "2.1.0", "model": null, "ladder_model": "claude-opus-5-5",
                       "effort": "medium", "group": null}),
            ),
            event(
                2,
                "turn_finished",
                json!({"turn": 1, "outcome": "succeeded", "model": null, "model_unknown": "no rollout"}),
            ),
            event(
                3,
                "turn_finished",
                json!({"turn": 2, "outcome": "succeeded", "model": "gpt-6-astra"}),
            ),
            event(
                4,
                "turn_finished",
                json!({"turn": 3, "outcome": "succeeded", "model": "gpt-6-nova"}),
            ),
        ] {
            track.observe(&event);
        }
        let measures = track.finish();
        assert!(measures.on_codex);
        assert_eq!(measures.provider.as_deref(), Some("claude"));
        assert_eq!(measures.claude_version, None);
        assert_eq!(measures.worker_model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(measures.worker_effort.as_deref(), Some("medium"));
        assert_eq!(measures.trial_group, None);
        // A Codex claim from before task 892 named the step's Claude model
        // and a group: neither is read.
        let mut track = MeasureTrack::default();
        track.observe(&event(
            1,
            "run_claimed",
            json!({"provider": "codex", "model": "claude-sonnet-5", "effort": "medium",
                   "group": "treatment"}),
        ));
        let measures = track.finish();
        assert_eq!(measures.worker_model, None);
        assert_eq!(measures.trial_group, None);
        // A Claude run keeps its claim's model and version.
        let mut track = MeasureTrack::default();
        for event in [
            event(
                1,
                "run_claimed",
                json!({"provider": "claude", "worker_mode": "headless", "claude_version": "2.1.0",
                       "model": "claude-opus-5-5", "effort": "medium", "group": null}),
            ),
            event(
                2,
                "turn_finished",
                json!({"turn": 1, "outcome": "succeeded", "model": "claude-sonnet-5"}),
            ),
        ] {
            track.observe(&event);
        }
        let measures = track.finish();
        assert!(!measures.on_codex);
        assert_eq!(measures.claude_version.as_deref(), Some("2.1.0"));
        assert_eq!(measures.worker_model.as_deref(), Some("claude-opus-5-5"));
        // A trial run claimed on Claude that the fallback moved to Codex
        // worked on Codex: it leaves the trial and the Claude strata, and
        // its model is the first Codex turn's, not the Claude one it
        // started on.
        let mut track = MeasureTrack::default();
        for event in [
            event(
                1,
                "run_claimed",
                json!({"provider": "claude", "requested_provider": "claude", "worker_mode": "headless",
                       "claude_version": "2.1.0", "model": "claude-sonnet-5", "effort": "medium",
                       "group": "treatment"}),
            ),
            event(
                2,
                "turn_finished",
                json!({"turn": 1, "outcome": "failed", "provider": "claude", "model": "claude-sonnet-5"}),
            ),
            event(
                3,
                "provider_switched",
                json!({"from": "claude", "to": "codex", "reason": "usage_limit", "phase": "start"}),
            ),
            event(
                4,
                "turn_finished",
                json!({"turn": 2, "outcome": "succeeded", "provider": "codex", "model": "gpt-6-astra"}),
            ),
        ] {
            track.observe(&event);
        }
        let measures = track.finish();
        assert!(measures.on_codex);
        assert_eq!(measures.provider.as_deref(), Some("claude"));
        assert_eq!(measures.actual_provider.as_deref(), Some("codex"));
        assert_eq!(measures.claude_version, None);
        assert_eq!(measures.trial_group, None);
        assert_eq!(measures.worker_model.as_deref(), Some("gpt-6-astra"));
        // Moved without a Codex turn that named its model: unknown, not
        // the Claude model.
        let mut track = MeasureTrack::default();
        for event in [
            event(
                1,
                "run_claimed",
                json!({"provider": "claude", "model": "claude-opus-5-5", "effort": "medium", "group": "control"}),
            ),
            event(
                2,
                "provider_switched",
                json!({"from": "claude", "to": "codex"}),
            ),
        ] {
            track.observe(&event);
        }
        let measures = track.finish();
        assert_eq!(measures.worker_model, None);
        assert_eq!(measures.trial_group, None);
    }

    /// The claim's provider, route and versions, and the run's headless
    /// turns: how many, the failed ones and their seconds (ADR-t813-2
    /// decision 7).
    #[test]
    fn a_headless_run_has_its_provider_route_and_turns() {
        let at = |id: i64, kind: &str, secs: i64, payload: Value| RunEvent {
            created_at: crate::domain::transcript::millis_text(secs * 1000),
            ..event(id, kind, payload)
        };
        let mut track = MeasureTrack::default();
        for event in [
            at(
                1,
                "run_claimed",
                0,
                json!({"provider": "codex", "worker_mode": "headless",
                                           "claude_version": "2.1.0", "codex_version": "0.46.0",
                                           "provider_version": "0.46.0"}),
            ),
            at(2, "turn_started", 10, json!({"turn": 1})),
            at(
                3,
                "turn_finished",
                40,
                json!({"turn": 1, "outcome": "succeeded"}),
            ),
            at(4, "turn_started", 50, json!({"turn": 2})),
            at(
                5,
                "turn_finished",
                55,
                json!({"turn": 2, "outcome": "failed"}),
            ),
        ] {
            track.observe(&event);
        }
        let measures = track.finish();
        assert_eq!(measures.provider.as_deref(), Some("codex"));
        assert_eq!(measures.route.as_deref(), Some("headless"));
        // A Codex run is no run of the host's Claude Code (task 892).
        assert_eq!(measures.claude_version, None);
        assert_eq!(measures.codex_version.as_deref(), Some("0.46.0"));
        assert_eq!(measures.provider_version.as_deref(), Some("0.46.0"));
        assert_eq!(
            measures.turns,
            Some(RunTurns {
                count: 2,
                failed: 1,
                secs: 35,
                by_provider: BTreeMap::from([(
                    "codex".to_owned(),
                    ProviderTurns {
                        count: 2,
                        failed: 1,
                        secs: 35,
                        tokens: None,
                    }
                )]),
            })
        );
        // A run that never moved: its actual provider is the claim's.
        assert_eq!(measures.actual_provider.as_deref(), Some("codex"));
        assert_eq!(measures.provider_switches, 0);
    }

    /// A run that moved from Codex to Claude in the middle (and one moved
    /// at its claim): the requested provider stays, the actual one is the
    /// last it moved to, and the turns and their tokens are split by the
    /// provider that ran them (ADR-t813-2 decision 7).
    #[test]
    fn a_run_that_moved_has_its_requested_and_actual_provider_and_turns_per_provider() {
        let at = |id: i64, kind: &str, secs: i64, payload: Value| RunEvent {
            created_at: crate::domain::transcript::millis_text(secs * 1000),
            ..event(id, kind, payload)
        };
        let mut track = MeasureTrack::default();
        for event in [
            at(
                1,
                "run_claimed",
                0,
                json!({"provider": "codex", "requested_provider": "codex", "worker_mode": "headless"}),
            ),
            at(
                2,
                "turn_started",
                10,
                json!({"turn": 1, "provider": "codex"}),
            ),
            at(
                3,
                "turn_finished",
                40,
                json!({"turn": 1, "outcome": "failed", "provider": "codex",
                       "tokens": {"input": 10, "output": 5, "cache_read": 0, "cache_creation": 0, "messages": 1}}),
            ),
            at(
                4,
                "provider_switched",
                41,
                json!({"from": "codex", "to": "claude", "reason": "usage_limit", "phase": "start"}),
            ),
            at(
                5,
                "turn_started",
                50,
                json!({"turn": 2, "provider": "claude"}),
            ),
            at(
                6,
                "turn_finished",
                70,
                json!({"turn": 2, "outcome": "succeeded", "provider": "claude",
                       "tokens": {"input": 100, "output": 20, "cache_read": 3, "cache_creation": 2, "messages": 1, "cost_usd": 0.5}}),
            ),
            // A turn recorded without its provider ran on the run's then.
            at(7, "turn_started", 80, json!({"turn": 3})),
            at(
                8,
                "turn_finished",
                85,
                json!({"turn": 3, "outcome": "succeeded"}),
            ),
        ] {
            track.observe(&event);
        }
        let measures = track.finish();
        assert_eq!(measures.provider.as_deref(), Some("codex"));
        assert_eq!(measures.actual_provider.as_deref(), Some("claude"));
        assert_eq!(measures.provider_switches, 1);
        let turns = measures.turns.unwrap();
        assert_eq!((turns.count, turns.failed, turns.secs), (3, 1, 55));
        let codex = &turns.by_provider["codex"];
        assert_eq!((codex.count, codex.failed, codex.secs), (1, 1, 30));
        assert_eq!(codex.tokens.as_ref().unwrap().total, 15);
        let claude = &turns.by_provider["claude"];
        assert_eq!((claude.count, claude.failed, claude.secs), (2, 0, 25));
        let tokens = claude.tokens.as_ref().unwrap();
        assert_eq!((tokens.sessions, tokens.total), (1, 125));
        assert_eq!(json!(tokens.cost_usd), json!(0.5));

        // Moved at its claim: the switch comes before `run_claimed`, which
        // carries the provider it moved to and the requested one.
        let mut track = MeasureTrack::default();
        for event in [
            event(
                1,
                "provider_switched",
                json!({"from": "claude", "to": "codex", "reason": "executable_missing", "phase": "start"}),
            ),
            event(
                2,
                "run_claimed",
                json!({"provider": "codex", "requested_provider": "claude", "worker_mode": "headless"}),
            ),
        ] {
            track.observe(&event);
        }
        let measures = track.finish();
        assert_eq!(measures.provider.as_deref(), Some("claude"));
        assert_eq!(measures.actual_provider.as_deref(), Some("codex"));
        assert_eq!(measures.provider_switches, 1);
    }

    #[test]
    fn a_run_without_measures_has_none() {
        let mut track = MeasureTrack::default();
        track.observe(&event(1, "run_claimed", json!({"from": "ready"})));
        track.observe(&event(2, "receipt_observed", json!({"validated": false})));
        // A later receipt's load is not the work's.
        track.observe(&event(2, "receipt_observed", json!({"load_avg_mean": 1.0})));
        track.observe(&event(2, "validation_finished", json!({})));
        track.observe(&event(
            2,
            "validation_finished",
            json!({"load_avg_mean": 1.0}),
        ));
        track.observe(&event(
            3,
            "verification_command",
            json!({"phase": "integration", "exit_code": 0}),
        ));
        assert_eq!(track.finish(), RunMeasures::default());
    }

    #[test]
    fn the_verify_load_weighs_each_command_by_its_duration() {
        let mut track = MeasureTrack::default();
        track.observe(&event(
            1,
            "verification_command",
            json!({"phase": "integration", "duration_secs": 30.0, "load_avg_mean": 10.0, "load_avg_max": 12.0}),
        ));
        track.observe(&event(
            2,
            "verification_command",
            json!({"phase": "integration", "load_avg_mean": 40.0, "load_avg_max": 50.0}),
        ));
        track.observe(&event(
            3,
            "verification_command",
            json!({"phase": "recheck", "duration_secs": 1.0, "load_avg_mean": 99.0}),
        ));
        track.observe(&event(4, "run_claimed", json!({"load_avg": 70.0})));
        let measures = track.finish();
        assert_eq!(
            measures.load.verify,
            Some(IntervalLoad {
                mean: Some(10.97),
                max: Some(50.0),
                band: Some("8-16"),
            })
        );
        // No work load: the band is the claim's.
        assert_eq!(measures.load_band, Some("64+"));
    }

    #[test]
    fn failures_count_in_their_band_lightest_first() {
        let mut bands = Vec::new();
        for load in [70.0, 2.0, 5.0, 3.0] {
            count_band(&mut bands, load);
        }
        assert_eq!(
            bands,
            vec![
                BandCount {
                    band: "0-4",
                    count: 2
                },
                BandCount {
                    band: "4-8",
                    count: 1
                },
                BandCount {
                    band: "64+",
                    count: 1
                },
            ]
        );
    }
}
