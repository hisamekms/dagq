//! The free disk space a run needs (ADR-0047 decision 44 as ADR-t4473-1
//! amends it): the supervisor claims no new run, and starts no landing's
//! verification, while the free space of the queue's directory (where the
//! run worktrees are) is short of what one needs. What one needs follows
//! from a reserve of the volume ([`reserve_bytes`]) and the size a new run
//! is expected to take from the configured method ([`stage_budgets`]),
//! with no factor; [`DiskState::decide`] freezes them for every pass
//! consumer, and the queue always moves
//! on above the reserve with no run moving. The `[disk]` table of
//! `dagq.toml` sets the reserve's share and expectation method.

use crate::domain::EventKind;
use serde::Serialize;
use std::path::Path;

use super::views::RunEvent;

/// The `subject` of the `cost` ask about the disk (ADR-0047 decision 42).
pub const DISK_SUBJECT: &str = "disk";
/// The options of the disk ask (ADR-0047 decision 44): `done` once a
/// person freed the disk, `wait` to leave the queue waiting.
pub const DISK_OPTIONS: &[&str] = &["done", "wait"];
/// The `repair` of the `auto_repaired` a cleanup for the disk records.
pub const DISK_CLEANUP: &str = "disk_cleanup";

/// Share above the reserve at which the runtime closes an open disk ask.
pub const DISK_CLOSE_MARGIN: f64 = 0.05;

/// How `[disk] estimate` obtains one run's expectation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskEstimate {
    /// The recent stage peaks' or run samples' maximum or p90
    /// ([`stage_budgets`]).
    #[default]
    Recent,
    /// The project's configured `run_size`, independent of samples.
    Fixed,
}

/// Waiting-build cleanup, independent of the reservation estimate method.
/// `[disk] waiting_build_cleanup` accepts "eager", "lazy" (the default),
/// or "never"; other values are configuration errors with a line number.
/// Eager removes eligible waiting builds during ordinary cleanup. Lazy
/// removes them only when a claim lacks space, oldest wait first until
/// enough bytes have gone. The Never mode retains outputs even when claims lack space.
/// All modes retain the separate pruning of obsolete incremental sessions.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingBuildCleanup {
    Eager,
    #[default]
    Lazy,
    Never,
}

/// An eligible, quiescent waiting build, after ownership/activity checks.
/// `since` is the wait's start in milliseconds; unknown starts sort last.
/// `bytes` is recoverable space, zero when not yet measured. A streaming
/// caller may offer unknown sizes and replan after each actual removal.
#[derive(Debug, Clone, Copy)]
pub struct WaitingBuild {
    pub since: i64,
    pub bytes: u64,
}

/// Indices of waiting builds to remove, oldest first (stable for ties).
/// `shortage` is the claim's missing bytes, not a landing's shortage;
/// None or zero means a claim is not blocked by space. Callers supply only
/// eligible stopped waits: unleased ones or those offered by their owner,
/// never another owner's lease, active work, or output-excluded waits.
/// Keeping eligibility outside this policy lets wait-specific age/output
/// restrictions evolve without changing mode or budgeting semantics.
pub fn waiting_build_plan(
    mode: WaitingBuildCleanup,
    builds: &[WaitingBuild],
    shortage: Option<u64>,
) -> Vec<usize> {
    let mut remaining = match mode {
        WaitingBuildCleanup::Never => return Vec::new(),
        WaitingBuildCleanup::Eager => u64::MAX,
        WaitingBuildCleanup::Lazy => shortage.unwrap_or(0),
    };
    let mut order: Vec<usize> = (0..builds.len()).collect();
    order.sort_by_key(|&i| builds[i].since);
    order.retain(|&i| {
        if mode == WaitingBuildCleanup::Eager {
            return true;
        }
        let take = remaining > 0;
        remaining = remaining.saturating_sub(builds[i].bytes);
        take
    });
    order
}

/// The `[disk]` table of `dagq.toml`.
/// `estimate` is a string, `"recent"` by default or `"fixed"`; other values
/// are read errors with a line number. In fixed mode `run_size` is required,
/// a positive size string read by [`parse_size`] (for example `"20GiB"`),
/// with no runtime default. Missing, unreadable or zero sizes are read errors
/// with a line number, like invalid `reserve`. In recent mode a supplied
/// `run_size` and `landing_size` are warned of and ignored, even if unreadable.
/// Recent mode takes W and I from the stage peaks ([`stage_budgets`]).
/// Fixed mode takes W from
/// `run_size` and I from `landing_size`, falling back to W when omitted.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DiskConfig {
    /// `reserve` (a share of the volume written as `"5%"`, default
    /// [`DEFAULT_RESERVE_SHARE`]): the free space no claim nor landing goes
    /// below is this share of the volume's size, at least
    /// [`MIN_RESERVE_BYTES`] ([`reserve_bytes`]).
    pub reserve: f64,
    /// The method used by both supervisor and human integration.
    pub estimate: DiskEstimate,
    /// The fixed expectation in bytes; validated as positive for `Fixed`,
    /// absent for `Recent` by the configuration reader.
    pub run_size: Option<u64>,
    /// Fixed INTEGRATOR reservation I: positive size string `landing_size`.
    /// Omitted means `run_size`; recent mode warns and ignores it.
    pub landing_size: Option<u64>,
    /// Waiting build removal policy; independent of `estimate`.
    pub waiting_build_cleanup: WaitingBuildCleanup,
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            reserve: DEFAULT_RESERVE_SHARE,
            estimate: DiskEstimate::Recent,
            run_size: None,
            landing_size: None,
            waiting_build_cleanup: WaitingBuildCleanup::Lazy,
        }
    }
}

impl DiskConfig {
    /// The setting names of the `[disk]` table.
    pub const KEYS: [&str; 5] = [
        "reserve",
        "estimate",
        "run_size",
        "landing_size",
        "waiting_build_cleanup",
    ];
    /// The settings of an older `[disk]` that are read no more: the
    /// thresholds follow the reserve and the configured expectation with no
    /// factor (ADR-t4473-1), so a `dagq.toml` that still has them is
    /// warned of and read on without them.
    pub const RETIRED_KEYS: [&str; 7] = [
        "sample_runs",
        "claim_factor",
        "integrate_factor",
        "min_free_bytes",
        "ask_after_secs",
        "ask_margin",
        "close_margin",
    ];
}

/// The share of the volume `text` (`"5%"`: a number above 0 and below 100
/// followed by `%`) names, for `[disk] reserve`; `None` for any other text.
pub fn parse_reserve(text: &str) -> Option<f64> {
    let percent: f64 = text.trim().strip_suffix('%')?.trim().parse().ok()?;
    (percent.is_finite() && percent > 0.0 && percent < 100.0).then_some(percent / 100.0)
}

/// The kinds of the records the samples are read from ([`run_samples`]).
pub const SAMPLE_KINDS: [EventKind; 4] = [
    EventKind::BuildOutputsRemoved,
    EventKind::WorktreeRemoved,
    EventKind::ScratchpadRemoved,
    EventKind::RunTmpRemoved,
];

/// How many of the latest records of each of [`SAMPLE_KINDS`] the samples
/// are read from: 500, far more than the [`SAMPLE_COUNT`] runs the
/// expectation reads, as a run records some kinds more than once, while a
/// pass reads no more than a bounded number.
pub const SAMPLE_EVENT_LIMIT: usize = 500;

/// The kinds of the records a run's size sample is read from
/// ([`run_samples`]), with the part of the size each gives. The
/// `incremental_sessions_removed` is not one: a run's next build writes
/// that part of its build outputs again, so its bytes are no run's size.
const SAMPLE_EVENTS: [(EventKind, SamplePart); 4] = [
    (EventKind::BuildOutputsRemoved, SamplePart::Build),
    (EventKind::WorktreeRemoved, SamplePart::Build),
    (EventKind::ScratchpadRemoved, SamplePart::Scratchpad),
    (EventKind::RunTmpRemoved, SamplePart::Tmp),
];

/// The part of a run's size a record gives. A `worktree_removed` holds
/// the build outputs it removed with the worktree, so it and a
/// `build_outputs_removed` are the same part: the larger is taken, never
/// both added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SamplePart {
    Build,
    Scratchpad,
    Tmp,
}

/// What one record says of a run's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// Measured: `bytes` and no `unmeasured`; a 0 is a measured 0.
    Measured(u64),
    /// Unknown: no `bytes`, or `unmeasured` names a directory that could
    /// not be measured. The cleanup still writes the bytes it measured
    /// then, so they are at most a lower bound, and a lower bound would
    /// lower the expectation.
    Unknown,
    /// Nothing to measure: a `worktree_removed` whose worktree was gone
    /// before the cleanup (`worktree_missing`); its 0 is no run's size.
    Nothing,
}

fn reading(kind: EventKind, payload: &serde_json::Value) -> Reading {
    if kind == EventKind::WorktreeRemoved
        && payload
            .get("worktree_missing")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    {
        return Reading::Nothing;
    }
    let unmeasured = payload
        .get("unmeasured")
        .filter(|names| !names.is_null())
        .is_some_and(|names| names.as_array().is_none_or(|names| !names.is_empty()));
    match payload.get("bytes").and_then(serde_json::Value::as_u64) {
        Some(bytes) if !unmeasured => Reading::Measured(bytes),
        _ => Reading::Unknown,
    }
}

/// The size one ended run took on the disk at once, with the time of its
/// last record, for [`expected_run_size`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunSample {
    /// The larger of its largest build outputs and its largest worktree,
    /// plus its largest Claude Code scratchpad and its largest run
    /// `TMPDIR`.
    pub bytes: u64,
    /// Unix milliseconds of the last of its records read.
    pub at_millis: i64,
}

/// The run size samples of `events` (the `build_outputs_removed`,
/// `worktree_removed`, `scratchpad_removed` and `run_tmp_removed`; other
/// kinds and events without a run are not read), one per run, oldest
/// first. A run is a sample only when none of its records is unknown (an
/// unknown part counted as 0 or as its lower bound would lower the
/// expectation) and one of its build outputs or worktree records is
/// measured: a run with only a scratchpad or a `TMPDIR` measured is not,
/// as what it built was never measured. A `worktree_removed` of a
/// worktree already gone is skipped, neither measured nor unknown, though
/// its time is the run's last as any record's. An `unmeasured` that is
/// null is as absent. A
/// record whose time does not parse counts for the size but not the time,
/// and a run with no time is no sample.
pub fn run_samples(events: &[RunEvent]) -> Vec<RunSample> {
    #[derive(Default)]
    struct Run {
        build: Option<u64>,
        scratchpad: u64,
        tmp: u64,
        unknown: bool,
        at_millis: Option<i64>,
    }
    let mut runs: std::collections::BTreeMap<&super::RunId, Run> = Default::default();
    for event in events {
        let Some(run_id) = event.run_id.as_ref() else {
            continue;
        };
        let Some(&(kind, part)) = SAMPLE_EVENTS.iter().find(|(kind, _)| *kind == event.kind) else {
            continue;
        };
        let run = runs.entry(run_id).or_default();
        if let Some(at) = super::stats::timestamp_millis(&event.created_at) {
            run.at_millis = Some(run.at_millis.map_or(at, |seen| seen.max(at)));
        }
        let bytes = match reading(kind, &event.payload) {
            Reading::Nothing => continue,
            Reading::Unknown => {
                run.unknown = true;
                continue;
            }
            Reading::Measured(bytes) => bytes,
        };
        match part {
            SamplePart::Build => run.build = Some(run.build.map_or(bytes, |seen| seen.max(bytes))),
            SamplePart::Scratchpad => run.scratchpad = run.scratchpad.max(bytes),
            SamplePart::Tmp => run.tmp = run.tmp.max(bytes),
        }
    }
    let mut samples: Vec<RunSample> = runs
        .into_values()
        .filter(|run| !run.unknown)
        .filter_map(|run| {
            Some(RunSample {
                bytes: run
                    .build?
                    .saturating_add(run.scratchpad)
                    .saturating_add(run.tmp),
                at_millis: run.at_millis?,
            })
        })
        .collect();
    samples.sort_by_key(|sample| sample.at_millis);
    samples
}

/// How old a sample may be, from its last record, for the expectation to
/// read it: 14 days. A claim held for want of space records no new
/// sample, so it is the age, not the count, that drops an outlier then
/// and ends the hold. Replayed on the records (the disk claim replay
/// Spike), 7, 14 and 30 days gave the same expectations, the count acting
/// first; 14 days drops a slow project's old build sizes without
/// emptying the window over a weekend or a long task.
pub const SAMPLE_WINDOW_MILLIS: i64 = 14 * 24 * 60 * 60 * 1000;
/// How many of the newest samples in the window the expectation reads:
/// 20. Replayed, 50 gave the same share of runs above the expectation
/// (10.0% against 10.2%) while following a change of the build sizes
/// later; few samples fill early in a project with few records.
pub const SAMPLE_COUNT: usize = 20;
/// Below this many samples the expectation is their largest, not a
/// quantile: a quantile of few samples is close to their largest, and
/// erring large costs less than a run that fails for want of space.
pub const QUANTILE_SAMPLES: usize = 5;
/// The quantile of the samples the expectation is, in tenths: p90.
/// Replayed (`docs/plans/disk-claim-replay-spike.md`, as the other
/// values here), p90, p95 and the largest gave close medians (10.8, 12.4 and
/// 13.3 GiB) but the largest held claims 12 h in the stretch one
/// outlier set it, p90 1 h. A rank needs no unit nor size of a project,
/// and over 20 samples the 2 largest do not move it.
pub const QUANTILE_TENTHS: usize = 9;

/// The size a new run is expected to take, from `samples`
/// ([`run_samples`]) and `now_millis` (Unix milliseconds): of the newest
/// [`SAMPLE_COUNT`] samples within [`SAMPLE_WINDOW_MILLIS`] of now, their
/// largest below [`QUANTILE_SAMPLES`], their p90 (the
/// ceil(0.9 × n)-th smallest) from it; `None` without one, when only the
/// rule that one run moves on at a time holds.
pub fn expected_run_size(samples: &[RunSample], now_millis: i64) -> Option<u64> {
    expected_run_estimate(samples, now_millis).0
}

/// One run's expectation from the validated `[disk]` configuration,
/// samples and Unix time in milliseconds. The default `estimate = "recent"`
/// delegates to [`expected_run_estimate`]: newest 20 within 14 days, maximum
/// below five samples, p90 otherwise, `None` without samples. With
/// `estimate = "fixed"`, returns `run_size` regardless of sample count,
/// age or outliers, including with no samples. The reader requires a
/// positive size string via [`parse_size`] for fixed mode (missing, invalid
/// or zero is a line-numbered error); recent mode warns and ignores it.
/// Relaxation is applied later by [`DiskState::needs`] to either method.
pub fn run_expectation(config: &DiskConfig, samples: &[RunSample], now_millis: i64) -> Option<u64> {
    match config.estimate {
        DiskEstimate::Recent => expected_run_estimate(samples, now_millis).0,
        DiskEstimate::Fixed => config.run_size,
    }
}

/// The expectation and number of samples actually used in its window.
pub fn expected_run_estimate(samples: &[RunSample], now_millis: i64) -> (Option<u64>, usize) {
    let mut recent: Vec<&RunSample> = samples
        .iter()
        .filter(|sample| now_millis.saturating_sub(sample.at_millis) <= SAMPLE_WINDOW_MILLIS)
        .collect();
    recent.sort_by_key(|sample| std::cmp::Reverse(sample.at_millis));
    recent.truncate(SAMPLE_COUNT);
    let mut sizes: Vec<u64> = recent.iter().map(|sample| sample.bytes).collect();
    sizes.sort_unstable();
    let n = sizes.len();
    if n < QUANTILE_SAMPLES {
        return (sizes.last().copied(), n);
    }
    let rank = (n * QUANTILE_TENTHS).div_ceil(10);
    (sizes.get(rank.max(1) - 1).copied(), n)
}

/// The least free bytes a reserve is, whatever the volume: 5 GiB, so a
/// small volume keeps room for the system and the queue. Replayed, a
/// reserve of 5 GiB alone ran short of space about 10 times as often as
/// one of 5% (2,125 samples against 220).
pub const MIN_RESERVE_BYTES: u64 = 5 << 30;
/// The default share of the volume the reserve is: 5%. A share follows
/// the volume, not a project's build size; replayed, 10% ran short in
/// 0.2% of the claims against 1.1% but held claims 121.7 h longer.
pub const DEFAULT_RESERVE_SHARE: f64 = 0.05;

/// The free bytes no claim nor landing goes below: the larger of
/// [`MIN_RESERVE_BYTES`] and `share` of `total`, the size of the volume.
pub fn reserve_bytes(total: u64, share: f64) -> u64 {
    MIN_RESERVE_BYTES.max((total as f64 * share).ceil() as u64)
}

/// An unfinished run's reservation stage, reassigned on every pass.
/// Ended runs have no entry in the registry. WORKER covers claim
/// through receipt, including revise/resume/rebuild; review, landing queue,
/// human and provider waits reserve no growth. The single `disk_usage`
/// decision derives this stage from build/location and landing-lane facts,
/// rather than worker/integrator roles. INTEGRATOR covers landing
/// verification, including retries. Waiting bytes already reduce statvfs free.
/// One immutable `DiskDecision` per pass supplies all gates, holds and asks;
/// consumers do not reclassify phases or recalculate candidate requirements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskStage {
    Worker,
    Waiting,
    Integrator,
}

/// Phase facts used by every disk reader. New phases must supply all facts;
/// the exhaustive match deliberately has no wildcard. Host e2e (including
/// its queued build) uses W; reviews and agent evaluation do not build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskUsage {
    pub stage: DiskStage,
    pub active: bool,
}

/// Build placement is explicit: worker/container placement and the e2e
/// engine's placement are independent (the current e2e engine is on host).
pub fn disk_usage(
    phase: super::run_phase::Phase,
    builds_on_host: bool,
    e2e_on_host: bool,
) -> DiskUsage {
    use super::run_phase::Phase as P;
    let (builds, landing_lane, active) = match phase {
        P::Provisioning
        | P::Environment(_)
        | P::Worker
        | P::Revise
        | P::Resume
        | P::Recovery
        | P::Verifying => (builds_on_host, false, true),
        P::AwaitingE2e | P::E2e => (e2e_on_host, false, true),
        P::Landing => (false, true, true),
        P::Validating | P::Review | P::Exiting | P::AgentEval => (false, false, true),
        P::ReviewHeld
        | P::Waiting { .. }
        | P::ClaimAnswer
        | P::ClaimHeld
        | P::Returning
        | P::AwaitingSlot
        | P::LandingQueue
        | P::LandingAnswer
        | P::NeedsSession
        | P::Held
        | P::Push { .. }
        | P::PushPending
        | P::Ended => (false, false, false),
    };
    DiskUsage {
        stage: if landing_lane {
            DiskStage::Integrator
        } else if builds {
            DiskStage::Worker
        } else {
            DiskStage::Waiting
        },
        active,
    }
}

/// The single landing requirement, at start and after disk-full alike.
/// `others` excludes the candidate if it is already in the landing lane;
/// with no other active run, the reserve alone guarantees progress.
pub fn landing_need(reserve: u64, budget: Option<u64>, current: Option<u64>, others: usize) -> u64 {
    reserve.saturating_add(if others == 0 {
        0
    } else {
        budget.unwrap_or(0).saturating_sub(current.unwrap_or(0))
    })
}

/// Candidate inputs from one pass's run registry, including queued runs
/// outside slots and the active integrator (whose decision also gates retry).
#[derive(Debug, Clone)]
pub struct LandingCandidate {
    pub run: super::RunId,
    pub current: Option<u64>,
    pub integrating: bool,
    pub waiting: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LandingDecision {
    pub run: super::RunId,
    pub needed: u64,
    pub allowed: bool,
    pub waiting: bool,
}
/// Reserve policy flags in the same disk decision, including the existing
/// ask-close margin. An unavailable reading holds nothing.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ReserveDecision {
    pub below: bool,
    pub recovered: bool,
}
pub fn reserve_decision(free: Option<u64>, need: Option<u64>) -> ReserveDecision {
    match (free, need) {
        (Some(f), Some(n)) => ReserveDecision {
            below: f < n,
            recovered: f >= n && (f - n) as f64 >= n as f64 * DISK_CLOSE_MARGIN,
        },
        _ => ReserveDecision {
            below: false,
            recovered: true,
        },
    }
}

/// One immutable decision for a pass. Gates, hold records, disk asks and
/// status consume this result; they never compare free bytes again.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiskDecision {
    pub reserve: ReserveDecision,
    pub free: Option<u64>,
    pub needs: DiskNeeds,
    pub claim_allowed: bool,
    pub claims: Option<usize>,
    pub claim_short: bool,
    pub landings: Vec<LandingDecision>,
    pub short: bool,
}
impl DiskDecision {
    pub fn landing(&self, run: &super::RunId) -> Option<&LandingDecision> {
        self.landings.iter().find(|d| d.run == *run)
    }
    pub fn blocked(&self) -> impl Iterator<Item = &LandingDecision> {
        self.landings.iter().filter(|d| d.waiting && !d.allowed)
    }
    pub fn claim_hold(&self) -> Option<super::claim_hold::ClaimHold> {
        if !self.claim_short {
            return None;
        }
        Some(super::claim_hold::ClaimHold {
            reason: super::claim_hold::HoldReason::DiskSpace,
            value: self.free? as f64,
            threshold: self.needs.claim as f64,
            ask_id: None,
            disk: Some(self.needs),
        })
    }
    pub fn landing_hold(&self) -> Option<super::claim_hold::ClaimHold> {
        let blocked = self.blocked().max_by_key(|d| d.needed)?;
        let mut needs = self.needs;
        needs.landing = blocked.needed;
        Some(super::claim_hold::ClaimHold {
            reason: super::claim_hold::HoldReason::DiskSpace,
            value: self.free? as f64,
            threshold: blocked.needed as f64,
            ask_id: None,
            disk: Some(needs),
        })
    }
}

/// A run's stage and trusted current size; unknown sizes reserve the whole
/// stage budget. Waiting reserves zero even when its size is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskRun {
    pub stage: DiskStage,
    pub current: Option<u64>,
}

/// I when no stage-specific source gives one: fixed uses `landing_size` or
/// W; recent uses W, which [`stage_budgets`] takes when neither the
/// INTEGRATOR peaks nor the run samples give I, so W and I are known or
/// unknown together and an empty INTEGRATOR slot is always reserved once W
/// is. Both methods use the same formulas.
pub fn landing_expectation(config: &DiskConfig, worker: Option<u64>) -> Option<u64> {
    match config.estimate {
        DiskEstimate::Fixed => config.landing_size.or(worker),
        DiskEstimate::Recent => worker,
    }
}

/// The kind of the record of a stage's peak ([`StagePeaks`]), the one
/// [`stage_samples`] reads.
pub const STAGE_PEAK_KIND: EventKind = EventKind::RunStagePeak;

/// One stage of a run left, as [`StagePeaks::pass`] gives it: the largest
/// trusted size read while the run was in `stage` (WORKER or INTEGRATOR;
/// the measure held as the stage began counts for it) and how many
/// distinct measures it read there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagePeak {
    pub run: super::RunId,
    pub stage: DiskStage,
    pub peak_bytes: u64,
    pub measures: u32,
}

/// What one pass read of one registered run: its stage now and its trusted
/// size with the stamp of the measure it comes from (`S`, the walk's start
/// time for the supervisor), `None` when no size is trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageObservation<S> {
    pub run: super::RunId,
    pub stage: DiskStage,
    pub measure: Option<(u64, S)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Peak<S> {
    bytes: u64,
    measures: u32,
    /// The stamp of the measure last counted in this stage.
    last: S,
}

#[derive(Debug, Clone)]
struct RunPeaks<S> {
    stage: DiskStage,
    worker: Option<Peak<S>>,
    integrator: Option<Peak<S>>,
}

impl<S: Copy> RunPeaks<S> {
    fn peak(&mut self, stage: DiskStage) -> Option<&mut Option<Peak<S>>> {
        match stage {
            DiskStage::Worker => Some(&mut self.worker),
            DiskStage::Integrator => Some(&mut self.integrator),
            DiskStage::Waiting => None,
        }
    }

    fn left(&mut self, run: &super::RunId) -> Option<StagePeak> {
        let stage = self.stage;
        let peak = (*self.peak(stage)?)?;
        Some(StagePeak {
            run: run.clone(),
            stage,
            peak_bytes: peak.bytes,
            measures: peak.measures,
        })
    }
}

/// The peaks of the WORKER and INTEGRATOR stages of the unfinished runs,
/// read on every pass from the same trusted sizes as the reservations,
/// whatever the `[disk] estimate` method (the fixed method records them
/// too: they are the material to review its size and the recent method's
/// samples).
///
/// - A measure counts once per stage: a pass that reads the same measure
///   again (the walk is slower than the loop, or the run came back from a
///   wait before the next walk) adds nothing; the measure held as a stage
///   begins counts for that stage too, so a stage shorter than a walk
///   interval gives the size it began with.
/// - A stage is left when the run's stage changes (WORKER at its receipt,
///   or any wait; INTEGRATOR at the end of the landing's verification) or
///   when the run leaves the registry (it ended). Each leaving gives one
///   [`StagePeak`] when the stage saw a measure.
/// - A run that goes back to WORKER (revise, resume, a rebuild after a
///   send back) or INTEGRATOR (a retry) keeps that stage's peak and count:
///   the next leaving gives the peak of all its stints so far, and the
///   samples take the largest per run and stage ([`stage_samples`]), so a
///   run is one sample per stage.
/// - The peaks live in the supervisor process alone: after a restart a
///   stage under way gives what was measured since; a stage left before
///   was recorded then. A stopping supervisor records no stage under way.
#[derive(Debug, Clone)]
pub struct StagePeaks<S> {
    runs: std::collections::HashMap<super::RunId, RunPeaks<S>>,
}

impl<S> Default for StagePeaks<S> {
    fn default() -> Self {
        Self {
            runs: std::collections::HashMap::new(),
        }
    }
}

impl<S: Copy + PartialEq> StagePeaks<S> {
    /// Read `registry`, this pass's observations of every registered run,
    /// and give the stages left since the last pass; runs no longer
    /// registered are forgotten.
    pub fn pass(&mut self, registry: &[StageObservation<S>]) -> Vec<StagePeak> {
        let mut left = Vec::new();
        let mut gone: Vec<super::RunId> = self
            .runs
            .keys()
            .filter(|run| !registry.iter().any(|seen| seen.run == **run))
            .cloned()
            .collect();
        gone.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        for run in gone {
            if let Some(mut peaks) = self.runs.remove(&run) {
                left.extend(peaks.left(&run));
            }
        }
        for seen in registry {
            let peaks = self.runs.entry(seen.run.clone()).or_insert(RunPeaks {
                stage: seen.stage,
                worker: None,
                integrator: None,
            });
            if peaks.stage != seen.stage {
                left.extend(peaks.left(&seen.run));
                peaks.stage = seen.stage;
            }
            let Some((bytes, stamp)) = seen.measure else {
                continue;
            };
            let Some(peak) = peaks.peak(seen.stage) else {
                continue;
            };
            match peak {
                Some(peak) if peak.last == stamp => {}
                Some(peak) => {
                    peak.bytes = peak.bytes.max(bytes);
                    peak.measures = peak.measures.saturating_add(1);
                    peak.last = stamp;
                }
                None => {
                    *peak = Some(Peak {
                        bytes,
                        measures: 1,
                        last: stamp,
                    });
                }
            }
        }
        left
    }
}

/// The peak samples of the WORKER and INTEGRATOR stages, one per run and
/// stage, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StageSamples {
    pub worker: Vec<RunSample>,
    pub integrator: Vec<RunSample>,
}

/// The stage samples of `events`: of the [`STAGE_PEAK_KIND`] records (other
/// kinds, those without a run, a `peak_bytes`, a known `phase` or a time
/// that parses are not read), the largest `peak_bytes` per run and
/// `phase`, at the time of its last record.
pub fn stage_samples(events: &[RunEvent]) -> StageSamples {
    let mut runs: std::collections::BTreeMap<(&super::RunId, bool), RunSample> = Default::default();
    for event in events {
        if event.kind != STAGE_PEAK_KIND {
            continue;
        }
        let Some(run) = event.run_id.as_ref() else {
            continue;
        };
        let worker = match event
            .payload
            .get("phase")
            .and_then(serde_json::Value::as_str)
        {
            Some("worker") => true,
            Some("integrator") => false,
            _ => continue,
        };
        let (Some(bytes), Some(at)) = (
            event
                .payload
                .get("peak_bytes")
                .and_then(serde_json::Value::as_u64),
            super::stats::timestamp_millis(&event.created_at),
        ) else {
            continue;
        };
        let sample = runs.entry((run, worker)).or_insert(RunSample {
            bytes,
            at_millis: at,
        });
        sample.bytes = sample.bytes.max(bytes);
        sample.at_millis = sample.at_millis.max(at);
    }
    let mut samples = StageSamples::default();
    for ((_, worker), sample) in runs {
        if worker {
            samples.worker.push(sample);
        } else {
            samples.integrator.push(sample);
        }
    }
    samples.worker.sort_by_key(|sample| sample.at_millis);
    samples.integrator.sort_by_key(|sample| sample.at_millis);
    samples
}

/// W and I, and how many samples W was read from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageBudgets {
    pub worker: Option<u64>,
    pub integrator: Option<u64>,
    pub sample_count: usize,
}

/// W and I under the configured method, from the ended runs' size samples
/// (`runs`, [`run_samples`]) and the stage peaks (`stages`,
/// [`stage_samples`]) at `now_millis`. Fixed: `run_size` and
/// `landing_size` (or `run_size`), whatever the samples. Recent: W from the
/// WORKER peaks and I from the INTEGRATOR peaks, each by the rule of
/// [`expected_run_estimate`] (the newest [`SAMPLE_COUNT`] within
/// [`SAMPLE_WINDOW_MILLIS`], their largest below [`QUANTILE_SAMPLES`],
/// their p90 from it). A stage with no peak sample in that window falls back
/// to the one expectation of `runs` by the same rule, so a queue with no
/// stage peak yet does not claim one run at a time for want of samples.
/// An I that neither gives falls back to W ([`landing_expectation`]): a
/// queue with WORKER peaks but no INTEGRATOR peak nor run sample (before
/// its first landing, or after an idle window) still reserves the empty
/// INTEGRATOR slot. `sample_count` is the samples W was read from.
pub fn stage_budgets(
    config: &DiskConfig,
    runs: &[RunSample],
    stages: &StageSamples,
    now_millis: i64,
) -> StageBudgets {
    match config.estimate {
        DiskEstimate::Fixed => {
            let worker = run_expectation(config, runs, now_millis);
            StageBudgets {
                worker,
                integrator: landing_expectation(config, worker),
                sample_count: expected_run_estimate(runs, now_millis).1,
            }
        }
        DiskEstimate::Recent => {
            let fallback = || expected_run_estimate(runs, now_millis);
            let of = |stage: &[RunSample]| match expected_run_estimate(stage, now_millis) {
                (_, 0) => fallback(),
                read => read,
            };
            let (worker, sample_count) = of(&stages.worker);
            StageBudgets {
                worker,
                integrator: of(&stages.integrator)
                    .0
                    .or_else(|| landing_expectation(config, worker)),
                sample_count,
            }
        }
    }
}

/// Inputs to the pure reservation formula, one entry per unfinished run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskState {
    pub free: Option<u64>,
    pub total: u64,
    /// WORKER budget W, from the configured estimation method.
    pub expected: Option<u64>,
    /// INTEGRATOR budget I, from the same method.
    pub integrator_expected: Option<u64>,
    pub runs: Vec<DiskRun>,
    /// Runs going on by themselves (DiskMoves), including review jobs.
    /// Activity controls idle progress/NoSamples; a review still has a waiting
    /// reservation of zero growth, even while its job is active.
    pub moving: usize,
    /// Trusted current size of the run whose landing is being considered.
    pub landing_current: Option<u64>,
    pub landing_waiting: bool,
    pub relaxed: Option<DiskRelaxed>,
}

/// Whether a claim and a landing may go now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskVerdict {
    pub claim: bool,
    pub landing: bool,
}

/// The free bytes a claim and a landing need now ([`DiskState::needs`]),
/// for the holds, the cleanup and the disk ask.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct DiskNeeds {
    /// Size in bytes of the volume used to calculate the reserve; zero when unread.
    pub volume: u64,
    /// Configured fraction of that volume; the minimum reserve still applies.
    pub reserve_share: f64,
    /// The configured method, including when a relaxation overrides its value.
    pub estimate: DiskEstimate,
    /// Samples within the expectation's age and count window.
    pub sample_count: usize,
    /// Runs moving at this reading, including landing verifications.
    pub moving: usize,
    /// Unfinished runs by stage (review and all other waits are waiting).
    pub workers: usize,
    pub waiting: usize,
    pub integrators: usize,
    /// Effective INTEGRATOR budget I, after expected relaxation.
    pub integrator_expected: Option<u64>,
    /// U: sum of unused stage reservations, before no_remaining relaxation.
    pub remaining: u64,
    /// WORKER/INTEGRATOR runs with a trusted size, including measured zero.
    pub measured: usize,
    /// WORKER/INTEGRATOR runs reserving the whole budget (no trusted size).
    pub unmeasured: usize,
    /// Whether a landing waits, even when the idle rule adds no expectation.
    pub landing_waiting: bool,
    /// Bytes added for the new claim; zero with no moving run or no expectation.
    pub claim_expected: u64,
    /// Remaining bytes added to the claim; zero under the idle/no-sample rule.
    pub claim_remaining: u64,
    /// Empty INTEGRATOR slot term; zero when occupied or relaxed (or idle).
    pub landing_expected: u64,
    /// Why claims wait regardless of free space; None when space alone decides.
    pub claims_wait_reason: Option<ClaimsWaitReason>,
    /// The reserve ([`reserve_bytes`]).
    pub reserve: u64,
    /// What a new run is expected to take, W ([`stage_budgets`]).
    pub expected: Option<u64>,
    /// Claim threshold: reserve + W + U + empty-slot I; reserve alone
    /// for idle progress, or without samples (NoSamples prevents claims).
    pub claim: u64,
    /// What a landing's verification needs free: the reserve with no run
    /// moving or no expectation, else reserve + max(0, I - candidate size).
    pub landing: u64,
    /// The claims wait this pass whatever is free, with no hold: a landing
    /// waits with no run moving (it goes first), or runs move with no
    /// expectation (a project with no samples runs one at a time).
    pub claims_wait: bool,
    /// The relaxation these needs were reckoned under; `None` outside one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relaxed: Option<DiskRelaxed>,
}

/// The existing ordering and no-sample rules that defer claims without a hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimsWaitReason {
    /// An idle queue gives the waiting landing the first turn.
    LandingFirst,
    /// With no expectation only one run moves at a time.
    NoSamples,
}

impl DiskNeeds {
    /// The same breakdown for event messages and cleanup waits; sizes are GiB.
    pub fn message(&self) -> String {
        let show = |bytes: u64| gib(bytes as f64);
        format!(
            "reserve {} (volume {}, share {:.2}%); expected {} ({}); moving {}, remaining {} (measured {}, unmeasured {}); landing_waiting {}, landing_expected {}; claim additions: expected {}, remaining {}; claims_wait {}; stages worker {}, waiting {}, integrator {}; W {}, I {}, U {}",
            show(self.reserve),
            show(self.volume),
            self.reserve_share * 100.0,
            self.expected.map_or_else(|| "none yet".into(), show),
            match self.estimate {
                DiskEstimate::Recent => format!("recent; samples {}", self.sample_count),
                DiskEstimate::Fixed => "fixed".into(),
            },
            self.moving,
            show(self.remaining),
            self.measured,
            self.unmeasured,
            self.landing_waiting,
            show(self.landing_expected),
            show(self.claim_expected),
            show(self.claim_remaining),
            match self.claims_wait_reason {
                Some(ClaimsWaitReason::LandingFirst) => "landing first",
                Some(ClaimsWaitReason::NoSamples) => "no samples; one run at a time",
                None => "none",
            },
            self.workers,
            self.waiting,
            self.integrators,
            self.expected.map_or_else(|| "none yet".into(), show),
            self.integrator_expected
                .map_or_else(|| "none yet".into(), show),
            show(self.remaining),
        ) + &self
            .relaxed
            .map_or_else(String::new, |relaxed| format!("; {}", relaxed.message()))
    }
}

impl DiskState {
    /// Pure stage reservation formula. U = sum(max(0, stage budget - current));
    /// unknown current uses the whole budget. Waiting contributes zero.
    /// Each occupied integration slot is already in U; the one empty slot
    /// contributes I separately. Claim = reserve + U + W + empty-slot I.
    /// Landing = reserve + max(0, I - candidate current).
    /// With no moving run the reserve alone allows one run to progress;
    /// LandingFirst and NoSamples retain their ordering. Relax expected
    /// overrides both W/I, no_remaining clears U, no_landing_expectation
    /// clears only the empty-slot term. Nothing relaxes the reserve.
    pub fn needs(&self, share: f64) -> DiskNeeds {
        let reserve = reserve_bytes(self.total, share);
        let relax = self.relaxed.map(|r| r.relax).unwrap_or_default();
        let expected = self.expected.map(|w| relax.expected.unwrap_or(w));
        let integrator_expected = self
            .integrator_expected
            .map(|i| relax.expected.unwrap_or(i));
        let workers = self
            .runs
            .iter()
            .filter(|r| r.stage == DiskStage::Worker)
            .count();
        let integrators = self
            .runs
            .iter()
            .filter(|r| r.stage == DiskStage::Integrator)
            .count();
        let waiting = self.runs.len() - workers - integrators;
        let moving = self.moving;
        let remaining = self.runs.iter().fold(0u64, |sum, run| {
            let budget = match run.stage {
                DiskStage::Worker => expected,
                DiskStage::Integrator => integrator_expected,
                DiskStage::Waiting => Some(0),
            }
            .unwrap_or(0);
            sum.saturating_add(budget.saturating_sub(run.current.unwrap_or(0)))
        });
        let measured = self
            .runs
            .iter()
            .filter(|r| r.stage != DiskStage::Waiting && r.current.is_some())
            .count();
        let claim_remaining = if moving == 0 || relax.no_remaining {
            0
        } else {
            remaining
        };
        let landing_expected = if moving == 0 || integrators > 0 || relax.no_landing_expectation {
            0
        } else {
            integrator_expected.unwrap_or(0)
        };
        let claim_expected = if moving == 0 {
            0
        } else {
            expected.unwrap_or(0)
        };
        let claims_wait_reason = if moving == 0 && self.landing_waiting {
            Some(ClaimsWaitReason::LandingFirst)
        } else if moving > 0 && expected.is_none() {
            Some(ClaimsWaitReason::NoSamples)
        } else {
            None
        };
        DiskNeeds {
            volume: self.total,
            reserve_share: share,
            reserve,
            expected,
            integrator_expected,
            workers,
            waiting,
            integrators,
            moving,
            remaining,
            measured,
            unmeasured: workers + integrators - measured,
            landing_waiting: self.landing_waiting,
            claim_expected,
            claim_remaining,
            landing_expected,
            claim: reserve
                .saturating_add(claim_expected)
                .saturating_add(claim_remaining)
                .saturating_add(landing_expected),
            landing: landing_need(reserve, integrator_expected, self.landing_current, moving),
            claims_wait_reason,
            claims_wait: claims_wait_reason.is_some(),
            relaxed: self.relaxed,
            ..DiskNeeds::default()
        }
    }

    /// Freeze every candidate and claim decision once for all pass consumers.
    pub fn decide(
        &self,
        share: f64,
        candidates: &[LandingCandidate],
        estimate: DiskEstimate,
        sample_count: usize,
    ) -> DiskDecision {
        let mut needs = self.needs(share);
        needs.estimate = estimate;
        needs.sample_count = sample_count;
        let landings: Vec<_> = candidates
            .iter()
            .map(|c| {
                let needed = landing_need(
                    needs.reserve,
                    needs.integrator_expected,
                    c.current,
                    self.moving.saturating_sub(usize::from(c.integrating)),
                );
                LandingDecision {
                    run: c.run.clone(),
                    needed,
                    allowed: self.free.is_none_or(|free| free >= needed),
                    waiting: c.waiting,
                }
            })
            .collect();
        needs.landing = landings
            .iter()
            .map(|d| d.needed)
            .max()
            .unwrap_or(needs.reserve);
        let claim_short = self.free.is_some_and(|f| f < needs.claim);
        let claims = self.claim_count(&needs);
        let claim_allowed = claims != Some(0);
        let short = claim_short || landings.iter().any(|d| d.waiting && !d.allowed);
        DiskDecision {
            reserve: reserve_decision(self.free, Some(needs.reserve)),
            free: self.free,
            needs,
            claims,
            claim_allowed,
            claim_short,
            landings,
            short,
        }
    }

    /// Unread free space holds nothing. The reserve is a hard floor.
    pub fn verdict(&self, share: f64) -> DiskVerdict {
        let Some(free) = self.free else {
            return DiskVerdict {
                claim: true,
                landing: true,
            };
        };
        let needs = self.needs(share);
        DiskVerdict {
            claim: free >= needs.claim && !needs.claims_wait,
            landing: free >= needs.landing,
        }
    }

    /// floor((free - reserve - U - empty-slot I) / W). The caller combines
    /// this with the configured parallel/available-slot upper bound as before.
    /// Idle progress allows at least one even when the empty slot consumes
    /// the room. Unknown free space or W=0 imposes no numeric limit.
    pub fn claims(&self, share: f64) -> Option<usize> {
        self.claim_count(&self.needs(share))
    }

    fn claim_count(&self, needs: &DiskNeeds) -> Option<usize> {
        let free = self.free?;
        if free < needs.claim || needs.claims_wait {
            return Some(0);
        }
        let Some(w) = needs.expected else {
            return Some(1);
        };
        if w == 0 {
            return None;
        }
        let relax = self.relaxed.map(|r| r.relax).unwrap_or_default();
        let empty = if needs.integrators == 0 && !relax.no_landing_expectation {
            needs.integrator_expected.unwrap_or(0)
        } else {
            0
        };
        let room = free
            .saturating_sub(needs.reserve)
            .saturating_sub(needs.claim_remaining)
            .saturating_sub(empty);
        let claims = usize::try_from(room / w).unwrap_or(usize::MAX);
        Some(if needs.moving == 0 {
            claims.max(1)
        } else {
            claims
        })
    }
}

/// How often the supervisor measures the runs moving, off its loop
/// ([`moving_remaining`]): every 60 s. The worktree and `TMPDIR` of one
/// run measured in about 0.08 s in the Spike
/// (`docs/plans/disk-run-size-measure-spike.md`), so this is far shorter
/// than the measure of the runs that wait (10 min) and still a small load.
pub const MOVING_SIZE_INTERVAL_SECS: u64 = 60;
/// How many [`MOVING_SIZE_INTERVAL_SECS`] old a moving run's size may be
/// for the claims to trust it: 3. A walk that does not end (a slow or
/// stuck file system) leaves its runs counted as expected, not at a size
/// they may have outgrown.
pub const MOVING_SIZE_STALE_INTERVALS: u32 = 3;

/// What a pass knows of the walk of the moving runs ([`moving_walk`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MovingWalkInputs {
    /// How long since the last walk started; `None` before the first.
    pub since_started: Option<std::time::Duration>,
    /// A walk runs now.
    pub walking: bool,
    /// A run moves.
    pub moving: bool,
    /// A moving run has no measure of the run and directories it has now.
    pub unmeasured: bool,
}

/// What a pass does with the walk of the moving runs ([`moving_walk`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovingWalk {
    /// Start one now.
    Start,
    /// Start none this pass.
    Wait,
}

/// Whether a pass starts a walk of the moving runs: never while one runs
/// (walks never overlap) or with no run moving; otherwise once
/// [`MOVING_SIZE_INTERVAL_SECS`] passed since the last one started (or
/// before the first), or at once when a moving run has no measure of its
/// present target, as just after its claim.
pub fn moving_walk(inputs: &MovingWalkInputs) -> MovingWalk {
    let interval = std::time::Duration::from_secs(MOVING_SIZE_INTERVAL_SECS);
    let due = inputs.since_started.is_none_or(|since| since >= interval);
    if !inputs.walking && inputs.moving && (due || inputs.unmeasured) {
        MovingWalk::Start
    } else {
        MovingWalk::Wait
    }
}

/// The latest size measured of a moving run ([`trusted_size`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeMeasure {
    /// Its worktree and `TMPDIR` together; `None` when either could not be
    /// measured.
    pub bytes: Option<u64>,
    /// How long ago its measure started: the time the walk took counts.
    pub age: std::time::Duration,
    /// Whether it was measured of the run, worktree and `TMPDIR` the run
    /// has now.
    pub same_target: bool,
}

/// The size of a moving run now the claims count, from its latest
/// measure, `interval` the time between measures: the measured bytes when
/// it is of the same run and directories, measured, and no older than
/// [`MOVING_SIZE_STALE_INTERVALS`] intervals; otherwise `None`, which
/// [`moving_remaining`] counts as 0, the expectation whole (the safe side).
/// No measure yet, as just after the claim, is `None` too.
pub fn trusted_size(measure: Option<&SizeMeasure>, interval: std::time::Duration) -> Option<u64> {
    let measure = measure?;
    let fresh = measure.age <= interval.saturating_mul(MOVING_SIZE_STALE_INTERVALS);
    (measure.same_target && fresh)
        .then_some(measure.bytes)
        .flatten()
}

/// Whether to record `run_size_exceeded_expected` for a moving run.
/// `bytes` must be its trusted current size from [`trusted_size`], never
/// the zero used for an unmeasured run's remaining-space arithmetic.
/// No expectation or no trusted size produces no observation; equality
/// does not exceed it. The returned pair is (measured bytes, expectation).
/// The caller remembers successful records for the entire supervisor
/// process, even across waits/resumes, and sets `recorded` accordingly.
/// Restart clears that memory and permits one further record per run;
/// no event-history read is required on the loop. Expectations are the
/// configured fixed or recent estimate before temporary disk relaxation.
pub fn size_exceeded_expected(
    bytes: Option<u64>,
    expected: Option<u64>,
    recorded: bool,
) -> Option<(u64, u64)> {
    let (bytes, expected) = (bytes?, expected?);
    (!recorded && bytes > expected).then_some((bytes, expected))
}

/// What the moving runs are expected to take yet, together
/// ([`DiskNeeds::remaining`]): for each, the expectation less its size now
/// ([`trusted_size`]; `None` counts as 0), or 0 for a run past it. With no
/// expectation it is 0: a project with no samples runs one at a time.
pub fn moving_remaining(
    expected: Option<u64>,
    sizes: impl IntoIterator<Item = Option<u64>>,
) -> u64 {
    let Some(expected) = expected else {
        return 0;
    };
    sizes.into_iter().fold(0, |remaining: u64, size| {
        remaining.saturating_add(expected.saturating_sub(size.unwrap_or(0)))
    })
}

/// `bytes` in GiB with one decimal, for messages.
pub fn gib(bytes: f64) -> String {
    format!("{:.1} GiB", bytes / f64::from(1u32 << 30))
}

/// The kinds of the records of a relaxation of the needs (ADR-t4727-1):
/// the newest of them on the queue decides whether one is in effect
/// ([`relax_state`]), so a pass reads that one event and no other.
pub const RELAX_KINDS: [EventKind; 3] = [
    EventKind::DiskRelaxed,
    EventKind::DiskRelaxCleared,
    EventKind::DiskRelaxExpired,
];

/// The longest a relaxation may last: 24 hours. It is a way out of an
/// emergency, ended by its time rather than by someone remembering to undo
/// it, so it is short enough that a forgotten one ends within a day, and
/// long enough to last a night until a person frees the disk.
pub const MAX_RELAX_SECS: u64 = 24 * 60 * 60;

/// What a relaxation of the needs loosens (ADR-t4727-1); the reserve is
/// never one of them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DiskRelax {
    /// Override both WORKER W and INTEGRATOR I from the configured method;
    /// `None` keeps those budgets. Recent with no samples stays unsampled.
    #[serde(rename = "expected_bytes")]
    pub expected: Option<u64>,
    /// A claim adds no reservation for an empty INTEGRATOR slot, whether
    /// or not a landing waits. An active integrator's remainder stays in U.
    pub no_landing_expectation: bool,
    /// U, the unused WORKER and INTEGRATOR reservations, contributes zero
    /// to claims. Waiting stages already contribute zero.
    pub no_remaining: bool,
}

impl DiskRelax {
    /// Whether it loosens anything.
    pub fn loosens(&self) -> bool {
        self.expected.is_some() || self.no_landing_expectation || self.no_remaining
    }

    /// The terms it loosens, for messages.
    fn terms(&self) -> String {
        let mut terms = Vec::new();
        if let Some(expected) = self.expected {
            terms.push(format!("expected {}", gib(expected as f64)));
        }
        if self.no_landing_expectation {
            terms.push("no landing expectation".to_owned());
        }
        if self.no_remaining {
            terms.push("no remaining".to_owned());
        }
        terms.join(", ")
    }
}

/// A relaxation in effect: what it loosens, until when, and its record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DiskRelaxed {
    #[serde(flatten)]
    pub relax: DiskRelax,
    /// Unix milliseconds it ends at.
    pub until_millis: i64,
    /// The id of its `disk_relaxed`.
    pub event_id: i64,
}

impl DiskRelaxed {
    /// What the needs' message says of it.
    pub fn message(&self) -> String {
        format!(
            "relaxed until {} by disk relax (event {}): {}; the reserve is kept",
            super::marks::utc_text(self.until_millis),
            self.event_id,
            self.relax.terms()
        )
    }
}

/// A relaxation as `disk relax` asks for it, checked ([`relax_request`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelaxRequest {
    pub relax: DiskRelax,
    /// How long it lasts from its record, in seconds.
    pub for_secs: u64,
}

impl RelaxRequest {
    /// The payload of its `disk_relaxed`, with the `reason` given.
    pub fn payload(&self, reason: &str) -> serde_json::Value {
        serde_json::json!({
            "expected_bytes": self.relax.expected,
            "no_landing_expectation": self.relax.no_landing_expectation,
            "no_remaining": self.relax.no_remaining,
            "for_secs": self.for_secs,
            "reason": reason,
        })
    }
}

/// `relax` for `for_secs` as `disk relax` asks for it, or why it is
/// refused: it loosens nothing, it has no duration, a duration of 0 or one
/// past [`MAX_RELAX_SECS`], or an expectation of 0 (which would lift the
/// bound on the claims of a pass).
pub fn relax_request(relax: DiskRelax, for_secs: Option<u64>) -> Result<RelaxRequest, String> {
    if !relax.loosens() {
        return Err(
            "give what to relax: --expected <SIZE>, --no-landing-expectation or --no-remaining (the reserve is never relaxed)"
                .into(),
        );
    }
    if relax.expected == Some(0) {
        return Err("--expected must be above 0".into());
    }
    let Some(for_secs) = for_secs else {
        return Err("give how long the relaxation lasts with --for <DURATION>".into());
    };
    if for_secs == 0 || for_secs > MAX_RELAX_SECS {
        return Err(format!(
            "--for must be above 0 and at most {} hours",
            MAX_RELAX_SECS / 3600
        ));
    }
    Ok(RelaxRequest { relax, for_secs })
}

/// Whether a relaxation is in effect, from the newest of the queue's
/// [`RELAX_KINDS`] events (`latest`) at `now_millis`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelaxState {
    /// None: the needs are reckoned as decided before it.
    Normal,
    /// One is in effect until its time.
    Active(DiskRelaxed),
    /// The newest is a relaxation past its time that no
    /// `disk_relax_expired` records yet: the pass records one, and the
    /// needs are reckoned as with none.
    Expired(DiskRelaxed),
}

impl RelaxState {
    /// The relaxation the needs are reckoned under.
    pub fn in_effect(self) -> Option<DiskRelaxed> {
        match self {
            Self::Active(relaxed) => Some(relaxed),
            Self::Normal | Self::Expired(_) => None,
        }
    }
}

/// [`RelaxState`] of `latest` at `now_millis`: a `disk_relaxed` is in
/// effect from its record for its `for_secs`; a `disk_relax_cleared` or
/// `disk_relax_expired` newer than it ended it. One whose payload or time
/// does not read is none.
pub fn relax_state(latest: Option<&RunEvent>, now_millis: i64) -> RelaxState {
    let Some(event) = latest.filter(|event| event.kind == EventKind::DiskRelaxed) else {
        return RelaxState::Normal;
    };
    let payload = &event.payload;
    let flag = |name: &str| payload.get(name).and_then(serde_json::Value::as_bool) == Some(true);
    let read = || {
        let at = super::stats::timestamp_millis(&event.created_at)?;
        let for_secs = payload.get("for_secs")?.as_u64()?;
        let for_millis = i64::try_from(for_secs.checked_mul(1000)?).ok()?;
        Some(DiskRelaxed {
            relax: DiskRelax {
                expected: payload
                    .get("expected_bytes")
                    .and_then(serde_json::Value::as_u64),
                no_landing_expectation: flag("no_landing_expectation"),
                no_remaining: flag("no_remaining"),
            },
            until_millis: at.saturating_add(for_millis),
            event_id: event.id.as_i64(),
        })
    };
    match read() {
        Some(relaxed) if !relaxed.relax.loosens() => RelaxState::Normal,
        Some(relaxed) if now_millis < relaxed.until_millis => RelaxState::Active(relaxed),
        Some(relaxed) => RelaxState::Expired(relaxed),
        None => RelaxState::Normal,
    }
}

/// The active relaxation as `status` shows it for every role. Its terms
/// are flattened beside the actor, reason, start and UTC expiry from the
/// newest record; an expired or cleared one has no view, even before the
/// supervisor records its expiry.
#[derive(Debug, Clone, Serialize)]
pub struct DiskRelaxView {
    #[serde(flatten)]
    pub relaxed: DiskRelaxed,
    pub actor: Option<super::EventActor>,
    pub reason: Option<String>,
    pub since: String,
    pub until: String,
}

/// Read only the newest of [`RELAX_KINDS`], already fetched by the
/// caller. Status and its attention share this view and its expiry test.
pub fn relax_view(latest: Option<&RunEvent>, now_millis: i64) -> Option<DiskRelaxView> {
    let relaxed = relax_state(latest, now_millis).in_effect()?;
    let event = latest?;
    Some(DiskRelaxView {
        relaxed,
        actor: event.actor.clone(),
        reason: event
            .payload
            .get("reason")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        since: event.created_at.clone(),
        until: super::marks::utc_text(relaxed.until_millis),
    })
}

/// The payload of the `disk_relax_expired` of `relaxed`.
pub fn expired_payload(relaxed: &DiskRelaxed) -> serde_json::Value {
    serde_json::json!({
        "relaxed": relaxed.event_id,
        "until": super::marks::utc_text(relaxed.until_millis),
    })
}

/// A size as `disk relax --expected` takes it: a whole number of bytes, or
/// a number followed by `K`, `M`, `G` or `T` (powers of 1024, with or
/// without `iB` / `B`).
pub fn parse_size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let digits = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(digits);
    let shift = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 0,
        "K" | "KB" | "KIB" => 10,
        "M" | "MB" | "MIB" => 20,
        "G" | "GB" | "GIB" => 30,
        "T" | "TB" | "TIB" => 40,
        _ => {
            return Err(format!(
                "{text:?} is not a size: give e.g. 8GiB or 8589934592"
            ));
        }
    };
    let number: f64 = number
        .parse()
        .map_err(|_| format!("{text:?} is not a size: give e.g. 8GiB or 8589934592"))?;
    let bytes = number * (1u64 << shift) as f64;
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        return Err(format!("{text:?} is out of range"));
    }
    Ok(bytes.round() as u64)
}

/// A duration as `disk relax --for` takes it, in seconds: a whole number
/// followed by `s`, `m`, `h` or `d`.
pub fn parse_duration_secs(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let why = || format!("{text:?} is not a duration: give e.g. 30m, 6h or 1d");
    let unit = text.chars().last().ok_or_else(why)?;
    let scale = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return Err(why()),
    };
    let number: u64 = text[..text.len() - 1].parse().map_err(|_| why())?;
    number.checked_mul(scale).ok_or_else(why)
}

/// A process with the path of the executable it runs (task 1590):
/// `None` when that path could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExecutable {
    pub pid: u32,
    pub ppid: u32,
    pub executable: Option<String>,
}

/// The processes the cleanup of an ended run whose task is over stops
/// before it removes the run's `worktree` (task 1590): those whose
/// executable is under it (a test's child `dagq` left running from
/// `target/`), which would otherwise write into it again as they end.
/// Never `except` (the supervisor) nor a process it runs under, nor pid 0
/// or 1; never one whose executable could not be read or is elsewhere,
/// even with its working directory in the worktree.
pub fn worktree_executables<'a>(
    all: &'a [ProcessExecutable],
    worktree: &Path,
    except: u32,
) -> Vec<&'a ProcessExecutable> {
    let mut supervisor = vec![except];
    let mut current = except;
    while let Some(next) = all.iter().find(|p| p.pid == current).map(|p| p.ppid) {
        if next <= 1 || supervisor.contains(&next) {
            break;
        }
        supervisor.push(next);
        current = next;
    }
    all.iter()
        .filter(|p| p.pid > 1 && !supervisor.contains(&p.pid))
        .filter(|p| {
            p.executable
                .as_deref()
                .is_some_and(|executable| Path::new(executable).starts_with(worktree))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::event_kind::{BUILD_OUTPUTS_REMOVED, RUN_TMP_REMOVED, SCRATCHPAD_REMOVED};
    use super::*;

    fn event(kind: &str, run: Option<&str>, bytes: Option<u64>) -> RunEvent {
        RunEvent {
            id: crate::domain::EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: run.map(|run| crate::domain::RunId::new(run).unwrap()),
            kind: kind.into(),
            payload: bytes.map_or_else(
                || serde_json::json!({}),
                |bytes| serde_json::json!({"bytes": bytes}),
            ),
            created_at: String::new(),
            actor: None,
        }
    }

    #[test]
    fn waiting_build_modes_select_only_the_oldest_needed_builds() {
        use WaitingBuildCleanup::{Eager, Lazy, Never};
        let builds = [
            WaitingBuild {
                since: 3,
                bytes: 20 * GIB,
            }, // c
            WaitingBuild {
                since: 1,
                bytes: 20 * GIB,
            }, // a
            WaitingBuild {
                since: 2,
                bytes: 20 * GIB,
            }, // b
        ];
        for shortage in [None, Some(0)] {
            assert!(waiting_build_plan(Lazy, &builds, shortage).is_empty());
        }
        assert_eq!(waiting_build_plan(Lazy, &builds, Some(30 * GIB)), [1, 2]);
        assert_eq!(waiting_build_plan(Lazy, &builds, Some(20 * GIB)), [1]);
        assert_eq!(waiting_build_plan(Lazy, &builds, Some(61 * GIB)), [1, 2, 0]);
        assert_eq!(waiting_build_plan(Eager, &builds, None), [1, 2, 0]);
        assert!(waiting_build_plan(Never, &builds, Some(30 * GIB)).is_empty());
    }

    /// The incremental sessions removed of a run's build outputs are no
    /// sample's record: the run's next build writes them again.
    #[test]
    fn the_incremental_sessions_removed_are_no_sample() {
        let sessions = super::super::event_kind::INCREMENTAL_SESSIONS_REMOVED;
        assert!(!SAMPLE_KINDS.iter().any(|kind| kind.as_str() == sessions));
        for (kind, _) in SAMPLE_EVENTS {
            assert!(SAMPLE_KINDS.contains(&kind), "{kind:?}");
        }
    }

    #[test]
    fn estimate_reports_only_samples_used_by_the_expectation() {
        let now = SAMPLE_WINDOW_MILLIS + 1;
        let mut samples = vec![
            RunSample {
                bytes: GIB,
                at_millis: now
            };
            SAMPLE_COUNT + 3
        ];
        samples.push(RunSample {
            bytes: 100 * GIB,
            at_millis: 0,
        });
        assert_eq!(
            expected_run_estimate(&samples, now),
            (Some(GIB), SAMPLE_COUNT)
        );
        assert_eq!(expected_run_estimate(&[], now), (None, 0));
    }

    #[test]
    fn configured_expectation_selects_recent_or_a_sample_independent_fixed_size() {
        let recent = DiskConfig::default();
        let fixed = DiskConfig {
            estimate: DiskEstimate::Fixed,
            run_size: Some(20 * GIB),
            ..recent
        };
        let mut twenty: Vec<_> = (1..=19).map(|n| sample(n * GIB, NOW)).collect();
        twenty.push(sample(gib_f(45.6), NOW));
        for (samples, expected) in [
            (vec![], None),
            (vec![sample(7 * GIB, NOW)], Some(7 * GIB)),
            (
                vec![sample(9 * GIB, NOW), sample(3 * GIB, NOW)],
                Some(9 * GIB),
            ),
            (
                (1..=5).map(|n| sample(n * GIB, NOW)).collect(),
                Some(5 * GIB),
            ),
            (twenty, Some(18 * GIB)),
            (vec![sample(90 * GIB, NOW - SAMPLE_WINDOW_MILLIS - 1)], None),
        ] {
            // Literal expected values keep the baseline independent of the
            // estimator; the compatibility assertion also names the old API.
            assert_eq!(run_expectation(&recent, &samples, NOW), expected);
            assert_eq!(expected_run_estimate(&samples, NOW).0, expected);
            for now in [NOW, NOW + SAMPLE_WINDOW_MILLIS + 1] {
                assert_eq!(run_expectation(&fixed, &samples, now), Some(20 * GIB));
            }
        }
    }

    #[test]
    fn the_settings_are_set_by_name() {
        assert_eq!(
            DiskConfig::KEYS,
            [
                "reserve",
                "estimate",
                "run_size",
                "landing_size",
                "waiting_build_cleanup"
            ]
        );
        assert_eq!(DiskConfig::default().reserve, DEFAULT_RESERVE_SHARE);
        for key in DiskConfig::RETIRED_KEYS {
            assert!(!DiskConfig::KEYS.contains(&key), "{key}");
        }
        assert_eq!(gib(1.5 * f64::from(1u32 << 30)), "1.5 GiB");
    }

    #[test]
    fn a_reserve_is_a_share_written_as_a_percentage() {
        assert_eq!(parse_reserve("5%"), Some(0.05));
        assert_eq!(parse_reserve(" 12.5 % "), Some(0.125));
        for bad in ["5", "0%", "100%", "-1%", "x%", "", "%", "inf%"] {
            assert_eq!(parse_reserve(bad), None, "{bad}");
        }
    }

    const GIB: u64 = 1 << 30;
    const DAY: i64 = 24 * 60 * 60 * 1000;
    /// 2026-11-03T00:00:00Z in Unix milliseconds.
    const NOW: i64 = 1_793_664_000_000;

    fn run_id(n: usize) -> String {
        format!("00000000-0000-4000-8000-{n:012}")
    }

    fn record(kind: &str, run: usize, payload: serde_json::Value, at: &str) -> RunEvent {
        RunEvent {
            run_id: Some(crate::domain::RunId::new(run_id(run)).unwrap()),
            payload,
            created_at: at.into(),
            ..event(kind, None, None)
        }
    }

    fn sample(bytes: u64, at_millis: i64) -> RunSample {
        RunSample { bytes, at_millis }
    }

    fn gib_f(gib: f64) -> u64 {
        (gib * GIB as f64) as u64
    }

    #[test]
    fn the_test_clock_is_the_day_it_names() {
        assert_eq!(
            super::super::stats::timestamp_millis("2026-11-03T00:00:00.000Z"),
            Some(NOW)
        );
    }

    /// One run is one sample: the larger of the build outputs and the
    /// worktree, plus the largest scratchpad and `TMPDIR`, at its last
    /// record; a run with a scratchpad or `TMPDIR` only, or with no run,
    /// is none, and other kinds are not read.
    #[test]
    fn a_run_sample_takes_each_part_largest_and_the_larger_build() {
        use super::super::event_kind::{INCREMENTAL_SESSIONS_REMOVED, WORKTREE_REMOVED};
        let bytes = |bytes: u64| serde_json::json!({"bytes": bytes});
        let samples = run_samples(&[
            record(BUILD_OUTPUTS_REMOVED, 1, bytes(300), "2026-11-01T00:00:00Z"),
            record(BUILD_OUTPUTS_REMOVED, 1, bytes(500), "2026-11-01T01:00:00Z"),
            record(WORKTREE_REMOVED, 1, bytes(400), "2026-11-01T02:00:00Z"),
            record(SCRATCHPAD_REMOVED, 1, bytes(10), "2026-11-01T03:00:00Z"),
            record(SCRATCHPAD_REMOVED, 1, bytes(30), "2026-11-01T00:30:00Z"),
            record(RUN_TMP_REMOVED, 1, bytes(7), "2026-11-01T00:10:00Z"),
            record(
                INCREMENTAL_SESSIONS_REMOVED,
                1,
                bytes(9_000),
                "2026-11-02T00:00:00Z",
            ),
            // The worktree larger than the build outputs.
            record(BUILD_OUTPUTS_REMOVED, 2, bytes(100), "2026-10-30T00:00:00Z"),
            record(WORKTREE_REMOVED, 2, bytes(900), "2026-10-30T01:00:00Z"),
            // A scratchpad and a `TMPDIR` only.
            record(SCRATCHPAD_REMOVED, 3, bytes(50), "2026-11-01T00:00:00Z"),
            record(RUN_TMP_REMOVED, 3, bytes(50), "2026-11-01T00:00:00Z"),
            // No run.
            RunEvent {
                created_at: "2026-11-01T00:00:00Z".into(),
                ..event(BUILD_OUTPUTS_REMOVED, None, Some(9_000))
            },
        ]);
        let at = |text: &str| super::super::stats::timestamp_millis(text).unwrap();
        assert_eq!(
            samples,
            [
                sample(900, at("2026-10-30T01:00:00Z")),
                sample(537, at("2026-11-01T03:00:00Z")),
            ]
        );
        // The incremental sessions removed alone are no sample.
        assert_eq!(
            run_samples(&[record(
                INCREMENTAL_SESSIONS_REMOVED,
                1,
                bytes(9_000),
                "2026-11-01T00:00:00Z"
            )]),
            []
        );
    }

    /// A record with a directory unmeasured, whether it counted 0 bytes
    /// or a lower bound, or with no bytes, makes its run no sample, so the
    /// expectation is what it is without that run; a measured 0 is a
    /// sample of 0.
    #[test]
    fn a_run_with_an_unknown_record_is_no_sample_and_a_measured_zero_is_one() {
        let at = "2026-11-02T00:00:00Z";
        let measured: Vec<RunEvent> = (1..=6)
            .map(|run| {
                record(
                    BUILD_OUTPUTS_REMOVED,
                    run,
                    serde_json::json!({"bytes": run as u64 * GIB}),
                    at,
                )
            })
            .collect();
        let alone = expected_run_size(&run_samples(&measured), NOW);
        assert_eq!(alone, Some(6 * GIB));
        for unknown in [
            serde_json::json!({"bytes": 0, "unmeasured": ["measure /w/target: busy"]}),
            serde_json::json!({"bytes": 40 * GIB, "unmeasured": ["measure /w/target: busy"]}),
            serde_json::json!({}),
            serde_json::json!({"bytes": null}),
        ] {
            let mut events = measured.clone();
            // Run 7 has a measured large build beside the unknown record.
            events.push(record(
                BUILD_OUTPUTS_REMOVED,
                7,
                serde_json::json!({"bytes": 50 * GIB}),
                at,
            ));
            events.push(record(
                super::super::event_kind::WORKTREE_REMOVED,
                7,
                unknown.clone(),
                at,
            ));
            let samples = run_samples(&events);
            assert_eq!(samples.len(), 6, "{unknown}");
            assert_eq!(expected_run_size(&samples, NOW), alone, "{unknown}");
            // An unknown scratchpad or `TMPDIR` excludes the run as well.
            for kind in [SCRATCHPAD_REMOVED, RUN_TMP_REMOVED] {
                let mut events = measured.clone();
                events.push(record(
                    BUILD_OUTPUTS_REMOVED,
                    7,
                    serde_json::json!({"bytes": 50 * GIB}),
                    at,
                ));
                events.push(record(kind, 7, unknown.clone(), at));
                assert_eq!(run_samples(&events).len(), 6, "{kind} {unknown}");
            }
        }
        // A measured 0, with `unmeasured` absent or empty, is a sample.
        for zero in [
            serde_json::json!({"bytes": 0}),
            serde_json::json!({"bytes": 0, "unmeasured": []}),
            serde_json::json!({"bytes": 0, "unmeasured": null}),
        ] {
            for kind in [
                BUILD_OUTPUTS_REMOVED,
                super::super::event_kind::WORKTREE_REMOVED,
            ] {
                let samples = run_samples(&[record(kind, 1, zero.clone(), at)]);
                assert_eq!(samples.len(), 1, "{kind} {zero}");
                assert_eq!(samples[0].bytes, 0);
                assert_eq!(expected_run_size(&samples, NOW), Some(0));
            }
        }
    }

    /// A worktree gone before its cleanup is skipped: the run's sample
    /// comes from its other measured records (its time is still the
    /// run's last), and with no other build or
    /// worktree measured it is none.
    #[test]
    fn a_missing_worktree_is_skipped() {
        let missing = serde_json::json!({"bytes": 0, "worktree_missing": true});
        let worktree = super::super::event_kind::WORKTREE_REMOVED;
        let samples = run_samples(&[
            record(worktree, 1, missing.clone(), "2026-11-02T05:00:00Z"),
            record(
                BUILD_OUTPUTS_REMOVED,
                1,
                serde_json::json!({"bytes": 800}),
                "2026-11-02T00:00:00Z",
            ),
            record(
                SCRATCHPAD_REMOVED,
                1,
                serde_json::json!({"bytes": 20}),
                "2026-11-02T01:00:00Z",
            ),
        ]);
        assert_eq!(
            samples,
            [sample(
                820,
                super::super::stats::timestamp_millis("2026-11-02T05:00:00Z").unwrap()
            )]
        );
        assert_eq!(
            run_samples(&[
                record(worktree, 1, missing.clone(), "2026-11-02T05:00:00Z"),
                record(
                    SCRATCHPAD_REMOVED,
                    1,
                    serde_json::json!({"bytes": 20}),
                    "2026-11-02T01:00:00Z"
                ),
            ]),
            []
        );
        assert_eq!(
            run_samples(&[record(worktree, 1, missing, "2026-11-02T05:00:00Z")]),
            []
        );
    }

    #[test]
    fn the_expectation_is_the_largest_below_five_samples_and_p90_from_them() {
        assert_eq!(expected_run_size(&[], NOW), None);
        let at = NOW - DAY;
        assert_eq!(expected_run_size(&[sample(7, at)], NOW), Some(7));
        let four: Vec<RunSample> = [3, 9, 1, 4].map(|bytes| sample(bytes, at)).into();
        assert_eq!(expected_run_size(&four, NOW), Some(9));
        // 5: ceil(4.5) = the 5th smallest; 10: the 9th; 20: the 18th.
        let five: Vec<RunSample> = [3, 9, 1, 4, 5].map(|bytes| sample(bytes, at)).into();
        assert_eq!(expected_run_size(&five, NOW), Some(9));
        let ten: Vec<RunSample> = (1..=10).map(|bytes| sample(bytes, at)).collect();
        assert_eq!(expected_run_size(&ten, NOW), Some(9));
        let twenty: Vec<RunSample> = (1..=20).map(|bytes| sample(bytes, at)).collect();
        assert_eq!(expected_run_size(&twenty, NOW), Some(18));
        // Of 21, the oldest is not read: the newest 20 are 1..=20.
        let mut more = twenty.clone();
        more.push(sample(1_000, at - 1));
        assert_eq!(expected_run_size(&more, NOW), Some(18));
        // A sample older than 14 days is not read, one at 14 days is.
        let old = [sample(5, NOW - 14 * DAY - 1), sample(3, NOW - 14 * DAY)];
        assert_eq!(expected_run_size(&old, NOW), Some(3));
    }

    /// A walk starts before the first and at the interval, not 1 ms before
    /// it, for the same runs all measured; at once for a run not measured;
    /// never while one runs or with no run moving.
    #[test]
    fn a_walk_of_the_moving_runs_starts_at_its_interval_or_for_a_run_not_measured() {
        use std::time::Duration;
        let interval = Duration::from_secs(MOVING_SIZE_INTERVAL_SECS);
        let ms = Duration::from_millis(1);
        let inputs = |since_started, walking, moving, unmeasured| MovingWalkInputs {
            since_started,
            walking,
            moving,
            unmeasured,
        };
        for (case, given, step) in [
            (
                "the first",
                inputs(None, false, true, false),
                MovingWalk::Start,
            ),
            (
                "1 ms before the interval",
                inputs(Some(interval - ms), false, true, false),
                MovingWalk::Wait,
            ),
            (
                "at the interval",
                inputs(Some(interval), false, true, false),
                MovingWalk::Start,
            ),
            (
                "a run not measured",
                inputs(Some(ms), false, true, true),
                MovingWalk::Start,
            ),
            (
                "a walk runs",
                inputs(Some(interval), true, true, true),
                MovingWalk::Wait,
            ),
            (
                "no run moving",
                inputs(None, false, false, false),
                MovingWalk::Wait,
            ),
        ] {
            assert_eq!(moving_walk(&given), step, "{case}");
        }
    }

    /// A moving run's size is trusted only measured, of the same run and
    /// directories, and no older than the stale intervals; everything else
    /// counts as unmeasured.
    #[test]
    fn a_moving_runs_size_is_trusted_only_measured_fresh_and_of_the_same_target() {
        let interval = std::time::Duration::from_secs(MOVING_SIZE_INTERVAL_SECS);
        let stale = interval * MOVING_SIZE_STALE_INTERVALS;
        let measure = |bytes, age, same_target| SizeMeasure {
            bytes,
            age,
            same_target,
        };
        let one = std::time::Duration::from_secs(1);
        for (case, measured, trusted) in [
            ("measured now", Some(measure(Some(7), one, true)), Some(7)),
            (
                "at the stale bound",
                Some(measure(Some(7), stale, true)),
                Some(7),
            ),
            ("a measured 0", Some(measure(Some(0), one, true)), Some(0)),
            ("never measured", None, None),
            (
                "could not be measured",
                Some(measure(None, one, true)),
                None,
            ),
            (
                "past the stale bound",
                Some(measure(Some(7), stale + one, true)),
                None,
            ),
            (
                "of another target",
                Some(measure(Some(7), one, false)),
                None,
            ),
        ] {
            assert_eq!(trusted_size(measured.as_ref(), interval), trusted, "{case}");
        }
    }

    /// The remaining is the sum of the expectation less each run's size
    /// now, 0 for a run past it, the whole expectation for one unmeasured;
    /// with no expectation it is 0.
    #[test]
    fn the_remaining_is_what_each_moving_run_has_yet_to_take() {
        let expected = Some(10 * GIB);
        assert_eq!(moving_remaining(expected, []), 0);
        assert_eq!(moving_remaining(expected, [Some(4 * GIB)]), 6 * GIB);
        assert_eq!(moving_remaining(expected, [Some(12 * GIB)]), 0);
        assert_eq!(moving_remaining(expected, [None]), 10 * GIB);
        assert_eq!(
            moving_remaining(expected, [Some(4 * GIB), Some(12 * GIB), None]),
            16 * GIB
        );
        assert_eq!(moving_remaining(None, [None, Some(GIB)]), 0);
        assert_eq!(moving_remaining(Some(u64::MAX), [None, None]), u64::MAX);
    }

    #[test]
    fn the_reserve_is_5_gib_or_5_percent() {
        let reserve = reserve_bytes(gib_f(228.3), DEFAULT_RESERVE_SHARE);
        assert_eq!(gib(reserve as f64), "11.4 GiB");
        assert_eq!(reserve_bytes(50 * GIB, DEFAULT_RESERVE_SHARE), 5 * GIB);
        assert_eq!(reserve_bytes(0, DEFAULT_RESERVE_SHARE), MIN_RESERVE_BYTES);
        assert_eq!(reserve_bytes(1_000 * GIB, 0.10), 100 * GIB);
    }

    /// A `disk_relaxed` (or another of [`RELAX_KINDS`]) on the queue,
    /// recorded at 2026-11-03T00:00:00Z.
    fn relax_record(kind: &str, payload: serde_json::Value) -> RunEvent {
        RunEvent {
            id: crate::domain::EventId::new(7),
            payload,
            created_at: "2026-11-03T00:00:00.000Z".into(),
            ..event(kind, None, None)
        }
    }

    fn relaxed(relax: DiskRelax) -> DiskRelaxed {
        DiskRelaxed {
            relax,
            until_millis: NOW + DAY,
            event_id: 7,
        }
    }

    /// Acceptance 1 of task 4727: a relaxation needs something to loosen
    /// and a duration above 0 and at most 24 h; an expectation of 0 is
    /// refused. The payload carries what it loosens.
    #[test]
    fn a_relaxation_needs_a_term_and_a_duration_within_the_limit() {
        let none = DiskRelax::default();
        let remaining = DiskRelax {
            no_remaining: true,
            ..DiskRelax::default()
        };
        assert!(relax_request(none, Some(3600)).is_err());
        assert!(relax_request(remaining, None).is_err());
        assert!(relax_request(remaining, Some(0)).is_err());
        assert!(relax_request(remaining, Some(MAX_RELAX_SECS + 1)).is_err());
        let zero = DiskRelax {
            expected: Some(0),
            ..DiskRelax::default()
        };
        assert!(relax_request(zero, Some(60)).is_err());
        let request = relax_request(remaining, Some(MAX_RELAX_SECS)).unwrap();
        let payload = request.payload("why");
        assert_eq!(payload["for_secs"], MAX_RELAX_SECS);
        assert_eq!(payload["no_remaining"], true);
        assert_eq!(payload["reason"], "why");
        assert_eq!(MAX_RELAX_SECS, 24 * 3600);
    }

    #[test]
    fn sizes_and_durations_read_as_the_command_line_takes_them() {
        assert_eq!(parse_size("8GiB"), Ok(8 * GIB));
        assert_eq!(parse_size("8g"), Ok(8 * GIB));
        assert_eq!(parse_size("1.5G"), Ok(3 * GIB / 2));
        assert_eq!(parse_size("512M"), Ok(512 << 20));
        assert_eq!(parse_size("1024"), Ok(1024));
        assert!(parse_size("8 parsecs").is_err());
        assert!(parse_size("").is_err());
        assert_eq!(parse_duration_secs("6h"), Ok(6 * 3600));
        assert_eq!(parse_duration_secs("30m"), Ok(1800));
        assert_eq!(parse_duration_secs("1d"), Ok(86_400));
        assert_eq!(parse_duration_secs("90s"), Ok(90));
        assert!(parse_duration_secs("6").is_err());
        assert!(parse_duration_secs("h").is_err());
        assert!(parse_duration_secs("").is_err());
    }

    /// Acceptance 2: the newest record decides; a relaxation is in effect
    /// from its record for its duration, past it is expired (once: the
    /// expired record then is the newest), and a clear or an expiry ends it.
    #[test]
    fn the_newest_record_decides_whether_a_relaxation_is_in_effect() {
        let payload = serde_json::json!({
            "expected_bytes": 4 * GIB, "no_landing_expectation": true,
            "no_remaining": false, "for_secs": 3600, "reason": "r",
        });
        let record = relax_record("disk_relaxed", payload);
        let until = NOW + 3600 * 1000;
        let relaxed = DiskRelaxed {
            relax: DiskRelax {
                expected: Some(4 * GIB),
                no_landing_expectation: true,
                no_remaining: false,
            },
            until_millis: until,
            event_id: 7,
        };
        assert_eq!(relax_state(None, NOW), RelaxState::Normal);
        assert_eq!(relax_state(Some(&record), NOW), RelaxState::Active(relaxed));
        assert_eq!(
            relax_state(Some(&record), until - 1).in_effect(),
            Some(relaxed)
        );
        assert_eq!(
            relax_state(Some(&record), until),
            RelaxState::Expired(relaxed)
        );
        assert_eq!(relax_state(Some(&record), until).in_effect(), None);
        assert_eq!(expired_payload(&relaxed)["relaxed"], 7);
        for ended in ["disk_relax_cleared", "disk_relax_expired"] {
            let ended = relax_record(ended, serde_json::json!({"relaxed": 7}));
            assert_eq!(relax_state(Some(&ended), NOW), RelaxState::Normal);
        }
        // A record that does not read relaxes nothing.
        for payload in [
            serde_json::json!({"no_remaining": true}),
            serde_json::json!({"for_secs": 60}),
        ] {
            let record = relax_record("disk_relaxed", payload);
            assert_eq!(relax_state(Some(&record), NOW), RelaxState::Normal);
        }
        let mut untimed = record.clone();
        untimed.created_at = "never".into();
        assert_eq!(relax_state(Some(&untimed), NOW), RelaxState::Normal);
        assert!(
            relaxed
                .message()
                .contains("expected 4.0 GiB, no landing expectation")
        );
    }

    /// The public view keeps the exact terms and provenance only while
    /// the newest record is active, including at the instant it expires.
    #[test]
    fn the_relaxation_view_shows_terms_and_provenance_until_it_ends() {
        let mut record = relax_record(
            "disk_relaxed",
            serde_json::json!({
                "expected_bytes": 1234, "no_remaining": true,
                "no_landing_expectation": true, "for_secs": 60,
                "reason": "allow an urgent run while space is freed"
            }),
        );
        record.actor = Some(super::super::EventActor {
            role: "inbox".into(),
            id: "desk".into(),
            requested_by: None,
        });
        let view = serde_json::to_value(relax_view(Some(&record), NOW).unwrap()).unwrap();
        assert_eq!(
            view,
            serde_json::json!({
                "expected_bytes": 1234, "no_remaining": true, "no_landing_expectation": true,
                "event_id": 7, "until_millis": NOW + 60_000,
                "actor": {"role": "inbox", "id": "desk"},
                "reason": "allow an urgent run while space is freed",
                "since": record.created_at, "until": super::super::marks::utc_text(NOW + 60_000)
            })
        );
        assert!(relax_view(Some(&record), NOW + 59_999).is_some());
        assert!(relax_view(Some(&record), NOW + 60_000).is_none());
        assert!(relax_view(Some(&record), NOW + 60_001).is_none());
        for kind in ["disk_relax_cleared", "disk_relax_expired"] {
            let ended = relax_record(kind, serde_json::json!({"relaxed": 7}));
            assert!(relax_view(Some(&ended), NOW).is_none());
        }
        assert!(relax_view(None, NOW).is_none());
    }

    #[test]
    fn one_outlier_does_not_hold_the_claims_of_2026_11_03() {
        let mut samples: Vec<RunSample> = (0..19)
            .map(|n| {
                sample(
                    gib_f(8.0 + 6.0 * f64::from(n) / 18.0),
                    NOW - i64::from(n) * 3_600_000,
                )
            })
            .collect();
        samples.push(sample(gib_f(45.6), NOW - 60_000));
        let expected = expected_run_size(&samples, NOW);
        assert!(
            expected.is_some_and(|expected| expected <= 14 * GIB),
            "{expected:?}"
        );
        for moving in [0, 1] {
            let state = DiskState {
                free: Some(71 * GIB),
                total: 228 * GIB,
                expected,
                moving,
                integrator_expected: expected,
                runs: vec![staged(DiskStage::Worker, expected); moving],
                landing_current: None,
                landing_waiting: false,
                relaxed: None,
            };
            assert!(state.verdict(DEFAULT_RESERVE_SHARE).claim, "{moving}");
        }
    }

    #[test]
    fn old_samples_leave_no_expectation_and_a_claim_goes() {
        let samples = [
            sample(90 * GIB, NOW - 15 * DAY),
            sample(80 * GIB, NOW - 20 * DAY),
        ];
        let expected = expected_run_size(&samples, NOW);
        assert_eq!(expected, None);
        let state = DiskState {
            free: Some(20 * GIB),
            total: 228 * GIB,
            expected,
            moving: 0,
            integrator_expected: expected,
            runs: vec![],
            landing_current: None,
            landing_waiting: false,
            relaxed: None,
        };
        assert_eq!(
            state.verdict(DEFAULT_RESERVE_SHARE),
            DiskVerdict {
                claim: true,
                landing: true
            }
        );
    }

    #[test]
    fn phase_properties_choose_host_builds_idle_reviews_and_landing() {
        use super::super::run_phase::Phase as P;
        for phase in [P::Worker, P::Revise, P::Resume, P::E2e, P::AwaitingE2e] {
            assert_eq!(disk_usage(phase, true, true).stage, DiskStage::Worker);
        }
        for phase in [
            P::Review,
            P::AgentEval,
            P::Waiting { in_slot: false },
            P::LandingQueue,
        ] {
            assert_eq!(disk_usage(phase, true, true).stage, DiskStage::Waiting);
        }
        assert_eq!(
            disk_usage(P::Landing, true, true).stage,
            DiskStage::Integrator
        );
        assert_eq!(disk_usage(P::E2e, false, true).stage, DiskStage::Worker); // current host e2e of a container run
        assert_eq!(disk_usage(P::E2e, false, false).stage, DiskStage::Waiting); // a container-only e2e engine
        assert_eq!(
            disk_usage(P::AwaitingE2e, false, false).stage,
            DiskStage::Waiting
        );
    }

    #[test]
    fn one_pass_keeps_candidate_gates_holds_and_ask_membership_consistent() {
        let a = super::super::RunId::new("measured-slot").unwrap();
        let b = super::super::RunId::new("queued-outside-slot").unwrap();
        let mut state = staged_state();
        state.total = 0; // reserve 5 GiB
        state.free = Some(15 * GIB);
        state.runs = vec![staged(DiskStage::Worker, Some(20 * GIB))];
        state.moving = 1;
        let candidates = vec![
            LandingCandidate {
                run: a.clone(),
                current: Some(18 * GIB),
                integrating: false,
                waiting: true,
            },
            LandingCandidate {
                run: b.clone(),
                current: None,
                integrating: false,
                waiting: true,
            },
        ];
        let pass = state.decide(0.05, &candidates, DiskEstimate::Fixed, 0);
        assert_eq!(
            (
                pass.landing(&a).unwrap().needed,
                pass.landing(&a).unwrap().allowed
            ),
            (7 * GIB, true)
        );
        assert_eq!(
            (
                pass.landing(&b).unwrap().needed,
                pass.landing(&b).unwrap().allowed
            ),
            (25 * GIB, false)
        );
        assert_eq!(
            pass.blocked().map(|d| d.run.clone()).collect::<Vec<_>>(),
            vec![b.clone()]
        );
        assert_eq!(pass.landing_hold().unwrap().threshold, (25 * GIB) as f64);
        assert!(pass.claim_short);
        assert!(!pass.claim_allowed);
        // Completing the measurement changes gate, hold and ask membership together.
        let mut measured = candidates.clone();
        measured[1].current = Some(18 * GIB);
        let pass = state.decide(0.05, &measured, DiskEstimate::Fixed, 0);
        assert!(pass.landing(&b).unwrap().allowed);
        assert!(pass.landing_hold().is_none());
        assert_eq!(pass.blocked().count(), 0);
        // Start and retry use identical progress, excluding an already active candidate.
        state.moving = 0;
        state.runs = vec![staged(DiskStage::Waiting, None); 2];
        let start = state.decide(0.05, &candidates, DiskEstimate::Fixed, 0);
        state.moving = 1;
        state.runs = vec![staged(DiskStage::Integrator, None)];
        let retry = state.decide(
            0.05,
            &[LandingCandidate {
                integrating: true,
                ..candidates[1].clone()
            }],
            DiskEstimate::Fixed,
            0,
        );
        assert_eq!(start.landing(&b).unwrap().needed, 5 * GIB);
        assert_eq!(retry.landing(&b).unwrap().needed, 5 * GIB);
        state.free = Some(5 * GIB - 1);
        assert!(
            !state
                .decide(
                    0.05,
                    &[LandingCandidate {
                        integrating: true,
                        ..candidates[1].clone()
                    }],
                    DiskEstimate::Fixed,
                    0
                )
                .landing(&b)
                .unwrap()
                .allowed
        );
    }

    fn staged(stage: DiskStage, current: Option<u64>) -> DiskRun {
        DiskRun { stage, current }
    }
    fn staged_state() -> DiskState {
        DiskState {
            free: Some(100 * GIB),
            total: 228 * GIB,
            expected: Some(20 * GIB),
            integrator_expected: Some(20 * GIB),
            runs: vec![
                staged(DiskStage::Worker, Some(12 * GIB)),
                staged(DiskStage::Worker, Some(5 * GIB)),
                staged(DiskStage::Waiting, Some(18 * GIB)),
            ],
            moving: 2,
            landing_current: Some(18 * GIB),
            landing_waiting: false,
            relaxed: None,
        }
    }

    #[test]
    fn stage_reservations_keep_an_integration_slot_without_double_counting() {
        let mut state = staged_state();
        let reserve = (228 * GIB).div_ceil(20);
        let needs = state.needs(0.05);
        assert_eq!(needs.remaining, 23 * GIB);
        assert_eq!((needs.workers, needs.waiting, needs.integrators), (2, 1, 0));
        assert_eq!(needs.claim, reserve + 63 * GIB); // 74.4 GiB
        assert_eq!(needs.landing, reserve + 2 * GIB);
        state.landing_waiting = true;
        assert_eq!(state.needs(0.05).claim, reserve + 63 * GIB);
        state
            .runs
            .push(staged(DiskStage::Integrator, Some(14 * GIB)));
        state.moving += 1;
        let needs = state.needs(0.05);
        assert_eq!(needs.remaining, 29 * GIB);
        assert_eq!(needs.landing_expected, 0);
        assert_eq!(needs.claim, reserve + 49 * GIB);
        state.landing_current = Some(33 * GIB);
        assert_eq!(state.needs(0.05).landing, reserve);
        // A wait releases all unused W, and resuming reinstates it.
        state.runs[0].stage = DiskStage::Waiting;
        assert_eq!(state.needs(0.05).remaining, 21 * GIB);
        state.runs[0].stage = DiskStage::Worker;
        assert_eq!(state.needs(0.05).remaining, 29 * GIB);
        state.runs[0].current = None;
        assert_eq!(state.needs(0.05).remaining, 41 * GIB);
    }

    #[test]
    fn stage_claims_use_the_floor_and_keep_progress_order_and_no_samples() {
        let mut state = staged_state();
        let reserve = (228 * GIB).div_ceil(20);
        state.runs = vec![staged(DiskStage::Worker, Some(gib_f(33.7)))];
        state.moving = 1;
        assert_eq!(state.needs(0.05).remaining, 0);
        for (free, claims) in [
            (reserve - 1, 0),
            (reserve + 40 * GIB - 1, 0),
            (reserve + 40 * GIB, 1),
            (reserve + 80 * GIB, 3),
        ] {
            state.free = Some(free);
            assert_eq!(state.claims(0.05), Some(claims));
        }
        state.runs = vec![staged(DiskStage::Waiting, None)];
        state.moving = 0;
        state.free = Some(reserve);
        assert_eq!(state.claims(0.05), Some(1));
        assert!(state.verdict(0.05).landing);
        state.free = Some(reserve + 80 * GIB);
        assert_eq!(state.claims(0.05), Some(3)); // still reserves the empty I slot
        state.landing_waiting = true;
        assert_eq!(
            state.needs(0.05).claims_wait_reason,
            Some(ClaimsWaitReason::LandingFirst)
        );
        assert_eq!(state.claims(0.05), Some(0));
        state.runs.push(staged(DiskStage::Worker, None));
        state.moving = 1;
        state.expected = None;
        state.integrator_expected = None;
        assert_eq!(
            state.needs(0.05).claims_wait_reason,
            Some(ClaimsWaitReason::NoSamples)
        );
        assert_eq!(state.claims(0.05), Some(0));
        state.relaxed = Some(relaxed(DiskRelax {
            expected: Some(GIB),
            ..DiskRelax::default()
        }));
        assert_eq!(
            state.needs(0.05).claims_wait_reason,
            Some(ClaimsWaitReason::NoSamples)
        );
        state.expected = Some(0);
        state.integrator_expected = Some(0);
        state.relaxed = None;
        assert_eq!(state.claims(0.05), None);
        state.free = None;
        assert_eq!(state.claims(0.05), None);
    }

    #[test]
    fn stage_relaxations_override_both_budgets_the_empty_slot_and_u() {
        let mut state = staged_state();
        state.integrator_expected = Some(30 * GIB);
        let reserve = (228 * GIB).div_ceil(20);
        assert_eq!(state.needs(0.05).claim, reserve + 73 * GIB);
        state.relaxed = Some(relaxed(DiskRelax {
            expected: Some(10 * GIB),
            ..DiskRelax::default()
        }));
        let needs = state.needs(0.05);
        assert_eq!(needs.expected, Some(10 * GIB));
        assert_eq!(needs.integrator_expected, Some(10 * GIB));
        assert_eq!(needs.remaining, 5 * GIB); // recomputed from current sizes
        assert_eq!(needs.claim, reserve + 25 * GIB);
        assert_eq!(needs.landing, reserve);
        state.relaxed = Some(relaxed(DiskRelax {
            no_landing_expectation: true,
            ..DiskRelax::default()
        }));
        assert_eq!(state.needs(0.05).claim, reserve + 43 * GIB);
        assert_eq!(state.needs(0.05).landing, reserve + 12 * GIB);
        state.relaxed = Some(relaxed(DiskRelax {
            no_remaining: true,
            ..DiskRelax::default()
        }));
        assert_eq!(state.needs(0.05).claim, reserve + 50 * GIB);
        state
            .runs
            .push(staged(DiskStage::Integrator, Some(14 * GIB)));
        state.moving += 1;
        state.relaxed = Some(relaxed(DiskRelax {
            no_landing_expectation: true,
            ..DiskRelax::default()
        }));
        assert_eq!(state.needs(0.05).claim, reserve + 59 * GIB); // occupied I remains in U
        state.free = Some(reserve - 1);
        assert_eq!(state.claims(0.05), Some(0));
        assert!(!state.verdict(0.05).landing);
    }

    fn executable(pid: u32, ppid: u32, executable: Option<&str>) -> ProcessExecutable {
        ProcessExecutable {
            pid,
            ppid,
            executable: executable.map(str::to_owned),
        }
    }

    /// Task 1590: only what runs an executable under the worktree is
    /// stopped, never the supervisor or what it runs under, pid 0 or 1, a
    /// process whose executable is elsewhere (its cwd only in the
    /// worktree) or unread, nor one under a sibling directory that only
    /// shares the prefix.
    #[test]
    fn only_a_process_running_an_executable_under_the_worktree_may_be_stopped() {
        let worktree = Path::new("/runs/r1/worktree");
        let all = [
            executable(1, 0, Some("/runs/r1/worktree/target/debug/dagq")),
            executable(0, 0, Some("/runs/r1/worktree/target/debug/dagq")),
            // The supervisor and its parents, as if run from the worktree.
            executable(50, 40, Some("/runs/r1/worktree/target/debug/dagq")),
            executable(40, 30, Some("/runs/r1/worktree/target/debug/dagq")),
            executable(30, 1, Some("/runs/r1/worktree/wrapper")),
            // Left behind by a test, and by a worker's `&`.
            executable(
                60,
                1,
                Some("/runs/r1/worktree/target/llvm-cov-target/debug/dagq"),
            ),
            executable(61, 60, Some("/runs/r1/worktree/bin/tool")),
            // Another run, a prefix only, outside, unread.
            executable(70, 1, Some("/runs/r2/worktree/target/debug/dagq")),
            executable(71, 1, Some("/runs/r1/worktree-old/target/debug/dagq")),
            // A `sleep` started in the worktree: its cwd is not read.
            executable(72, 1, Some("/bin/sleep")),
            executable(73, 1, None),
        ];
        let stopped: Vec<u32> = worktree_executables(&all, worktree, 50)
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(stopped, [60, 61]);
        // A supervisor not listed protects only itself.
        let stopped: Vec<u32> = worktree_executables(&all, worktree, 99)
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(stopped, [50, 40, 30, 60, 61]);
    }

    #[test]
    fn exceeding_an_expectation_records_only_the_first_trusted_observation() {
        let gib = 1 << 30;
        let expected = Some(20 * gib); // Fixed 20 GiB, independent of samples.
        assert_eq!(
            size_exceeded_expected(Some(19 * gib), expected, false),
            None
        );
        assert_eq!(
            size_exceeded_expected(Some(20 * gib), expected, false),
            None
        );
        assert_eq!(
            size_exceeded_expected(Some(21 * gib), expected, false),
            Some((21 * gib, 20 * gib))
        );
        assert_eq!(size_exceeded_expected(Some(25 * gib), expected, true), None);
        assert_eq!(size_exceeded_expected(Some(21 * gib), None, false), None);
        let interval = std::time::Duration::from_secs(MOVING_SIZE_INTERVAL_SECS);
        for measure in [
            None,
            Some(SizeMeasure {
                bytes: None,
                age: interval,
                same_target: true,
            }),
            Some(SizeMeasure {
                bytes: Some(21 * gib),
                age: interval * MOVING_SIZE_STALE_INTERVALS + std::time::Duration::from_secs(1),
                same_target: true,
            }),
            Some(SizeMeasure {
                bytes: Some(21 * gib),
                age: interval,
                same_target: false,
            }),
        ] {
            assert_eq!(
                size_exceeded_expected(trusted_size(measure.as_ref(), interval), expected, false),
                None
            );
        }
    }

    const HOUR: i64 = 60 * 60 * 1000;

    /// Stage samples of `gibs`, the newest at [`NOW`], one hour apart.
    fn stage(gibs: &[u64]) -> Vec<RunSample> {
        gibs.iter()
            .enumerate()
            .map(|(i, gib)| sample(gib * GIB, NOW - i as i64 * HOUR))
            .collect()
    }

    /// Acceptance 1 of task 5065: in recent mode W is the p90 of the WORKER
    /// peaks and I that of the INTEGRATOR peaks, apart; a stage of 4
    /// samples gives their largest, one of none (or only outside the
    /// window of 20 samples and 14 days) the one expectation of the run
    /// samples; fixed gives the configured sizes whatever the samples.
    #[test]
    fn stage_budgets_read_each_stages_peaks_and_fall_back_to_the_run_samples() {
        let recent = DiskConfig::default();
        let runs = stage(&[11, 12, 13, 14, 15, 16, 17, 18, 19, 20]);
        let ten = StageSamples {
            worker: stage(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
            integrator: stage(&[2, 4, 6, 8, 10, 12, 14, 16, 18, 20]),
        };
        assert_eq!(
            stage_budgets(&recent, &runs, &ten, NOW),
            StageBudgets {
                worker: Some(9 * GIB),
                integrator: Some(18 * GIB),
                sample_count: 10,
            }
        );
        // 4 samples: their largest, not a quantile.
        let four = StageSamples {
            integrator: stage(&[3, 7, 5, 2]),
            ..ten.clone()
        };
        assert_eq!(
            stage_budgets(&recent, &runs, &four, NOW).integrator,
            Some(7 * GIB)
        );
        // None: the run samples' p90, for either stage.
        let none = StageSamples::default();
        assert_eq!(
            stage_budgets(&recent, &runs, &none, NOW),
            StageBudgets {
                worker: Some(19 * GIB),
                integrator: Some(19 * GIB),
                sample_count: 10,
            }
        );
        assert_eq!(
            stage_budgets(&recent, &[], &none, NOW),
            StageBudgets::default()
        );
        // WORKER peaks, no INTEGRATOR peak and no run sample: I is W, so the
        // empty INTEGRATOR slot is still reserved.
        let worker_only = StageSamples {
            worker: ten.worker.clone(),
            integrator: Vec::new(),
        };
        assert_eq!(
            stage_budgets(&recent, &[], &worker_only, NOW),
            StageBudgets {
                worker: Some(9 * GIB),
                integrator: Some(9 * GIB),
                sample_count: 10,
            }
        );
        // INTEGRATOR peaks alone give I, while W is unknown (NoSamples).
        let integrator_only = StageSamples {
            worker: Vec::new(),
            integrator: ten.integrator.clone(),
        };
        assert_eq!(
            stage_budgets(&recent, &[], &integrator_only, NOW),
            StageBudgets {
                worker: None,
                integrator: Some(18 * GIB),
                sample_count: 0,
            }
        );
        // Older than 14 days: not read, so the run samples again.
        let old = StageSamples {
            worker: vec![sample(100 * GIB, NOW - 14 * DAY - 1)],
            integrator: stage(&[3, 7, 5, 2]),
        };
        assert_eq!(
            stage_budgets(&recent, &runs, &old, NOW),
            StageBudgets {
                worker: Some(19 * GIB),
                integrator: Some(7 * GIB),
                sample_count: 10,
            }
        );
        // Beyond the newest 20, and older than 14 days, not read.
        let mut worker = stage(&[1; 20]);
        worker.push(sample(50 * GIB, NOW - 21 * HOUR));
        worker.push(sample(60 * GIB, NOW - 15 * DAY));
        let window = StageSamples {
            worker,
            ..ten.clone()
        };
        assert_eq!(
            stage_budgets(&recent, &runs, &window, NOW).worker,
            Some(GIB)
        );
        // Fixed: the configured sizes, landing_size falling back to run_size.
        let fixed = DiskConfig {
            estimate: DiskEstimate::Fixed,
            run_size: Some(20 * GIB),
            landing_size: Some(30 * GIB),
            ..DiskConfig::default()
        };
        for stages in [&ten, &none] {
            let budgets = stage_budgets(&fixed, &runs, stages, NOW);
            assert_eq!(
                (budgets.worker, budgets.integrator),
                (Some(20 * GIB), Some(30 * GIB))
            );
        }
        let fixed = DiskConfig {
            landing_size: None,
            ..fixed
        };
        let budgets = stage_budgets(&fixed, &[], &ten, NOW);
        assert_eq!(
            (budgets.worker, budgets.integrator),
            (Some(20 * GIB), Some(20 * GIB))
        );
    }

    /// The largest `peak_bytes` per run and phase, at its last record;
    /// other kinds, unknown phases and records without bytes, run or time
    /// are not read.
    #[test]
    fn stage_samples_are_one_per_run_and_phase() {
        let kind = EventKind::RunStagePeak.as_str();
        let peak =
            |phase: &str, bytes: u64| serde_json::json!({"phase": phase, "peak_bytes": bytes});
        let events = [
            record(kind, 1, peak("worker", 5), "2026-11-01T00:00:00Z"),
            record(kind, 1, peak("worker", 9), "2026-11-01T01:00:00Z"),
            record(kind, 1, peak("worker", 7), "2026-11-01T02:00:00Z"),
            record(kind, 1, peak("integrator", 4), "2026-11-01T03:00:00Z"),
            record(kind, 2, peak("worker", 3), "2026-11-02T00:00:00Z"),
            record(kind, 3, peak("waiting", 30), "2026-11-02T00:00:00Z"),
            record(
                kind,
                3,
                serde_json::json!({"phase": "worker"}),
                "2026-11-02T00:00:00Z",
            ),
            record(kind, 3, peak("worker", 30), "not a time"),
            record(
                BUILD_OUTPUTS_REMOVED,
                3,
                peak("worker", 30),
                "2026-11-02T00:00:00Z",
            ),
            RunEvent {
                payload: peak("worker", 30),
                created_at: "2026-11-02T00:00:00Z".into(),
                ..event(kind, None, None)
            },
        ];
        let at = |text: &str| super::super::stats::timestamp_millis(text).unwrap();
        assert_eq!(
            stage_samples(&events),
            StageSamples {
                worker: vec![
                    sample(9, at("2026-11-01T02:00:00Z")),
                    sample(3, at("2026-11-02T00:00:00Z")),
                ],
                integrator: vec![sample(4, at("2026-11-01T03:00:00Z"))],
            }
        );
    }

    /// A stage counts each measure once and gives its peak as it is left;
    /// a run back in WORKER keeps that stage's peak, a measure first read
    /// in a new stage counts for it, one read again after a wait does not
    /// count again, waiting is never a peak, a stage that
    /// saw no measure gives none, and a run leaving the registry gives its
    /// stage under way.
    #[test]
    fn stage_peaks_are_given_as_each_stage_is_left() {
        let run = |n: usize| crate::domain::RunId::new(run_id(n)).unwrap();
        let seen = |n: usize, stage: DiskStage, measure: Option<(u64, u32)>| StageObservation {
            run: run(n),
            stage,
            measure,
        };
        let left = |n: usize, stage: DiskStage, peak_bytes: u64, measures: u32| StagePeak {
            run: run(n),
            stage,
            peak_bytes,
            measures,
        };
        use DiskStage::{Integrator, Waiting, Worker};
        let mut peaks = StagePeaks::default();
        for measure in [
            None,
            Some((8, 1)),
            Some((8, 1)),
            Some((12, 2)),
            Some((11, 3)),
        ] {
            assert_eq!(
                peaks.pass(&[seen(1, Worker, measure), seen(2, Worker, None)]),
                []
            );
        }
        assert_eq!(
            peaks.pass(&[seen(1, Waiting, Some((11, 3))), seen(2, Waiting, None)]),
            [left(1, Worker, 12, 3)]
        );
        // Back from the wait before a new walk: the same measure, not again.
        assert_eq!(peaks.pass(&[seen(1, Worker, Some((11, 3)))]), []);
        assert_eq!(peaks.pass(&[seen(1, Worker, Some((10, 4)))]), []);
        assert_eq!(
            peaks.pass(&[seen(1, Integrator, Some((10, 4)))]),
            [left(1, Worker, 12, 4)]
        );
        assert_eq!(peaks.pass(&[seen(1, Integrator, Some((14, 5)))]), []);
        assert_eq!(peaks.pass(&[]), [left(1, Integrator, 14, 2)]);
        assert_eq!(peaks.pass(&[]), []);
        // Measured only while waiting: no peak.
        assert_eq!(peaks.pass(&[seen(3, Waiting, Some((9, 1)))]), []);
        assert_eq!(peaks.pass(&[]), []);
    }
}
