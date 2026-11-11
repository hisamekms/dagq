//! The free disk space a run needs (ADR-0047 decision 44 as ADR-t4473-1
//! amends it): the supervisor claims no new run, and starts no landing's
//! verification, while the free space of the queue's directory (where the
//! run worktrees are) is short of what one needs. What one needs follows
//! from a reserve of the volume ([`reserve_bytes`]) and the size a new run
//! is expected to take from the recent runs' samples ([`run_samples`],
//! [`expected_run_size`]), with no factor; [`DiskState::needs`] and
//! [`DiskState::claims`] give them for a pass, and the queue always moves
//! on above the reserve with no run moving. The `[disk]` table of
//! `dagq.toml` sets the reserve's share.

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

/// The `[disk]` table of `dagq.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DiskConfig {
    /// `reserve` (a share of the volume written as `"5%"`, default
    /// [`DEFAULT_RESERVE_SHARE`]): the free space no claim nor landing goes
    /// below is this share of the volume's size, at least
    /// [`MIN_RESERVE_BYTES`] ([`reserve_bytes`]).
    pub reserve: f64,
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            reserve: DEFAULT_RESERVE_SHARE,
        }
    }
}

impl DiskConfig {
    /// The setting names of the `[disk]` table.
    pub const KEYS: [&str; 1] = ["reserve"];
    /// The settings of an older `[disk]` that are read no more: the
    /// thresholds follow the reserve and the recent runs' samples with no
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

/// What the disk says of a claim and a landing now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskState {
    /// The free bytes of the volume of the queue's directory; `None` when
    /// they could not be read.
    pub free: Option<u64>,
    /// The size of that volume.
    pub total: u64,
    /// What a new run is expected to take ([`expected_run_size`]).
    pub expected: Option<u64>,
    /// How many runs move now (a session open or a landing's verification
    /// running).
    pub moving: usize,
    /// What the moving runs are expected to take yet, together.
    pub remaining: u64,
    /// Whether a run waits for its landing.
    pub landing_waiting: bool,
    /// The relaxation of the needs in effect now ([`relax_state`]); `None`
    /// outside one.
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
    /// Samples within the expectation's age and count window.
    pub sample_count: usize,
    /// Runs moving at this reading, including landing verifications.
    pub moving: usize,
    /// Sum of the moving runs' remaining estimates, before the idle rule.
    pub remaining: u64,
    /// Moving runs counted from a trusted measured size, including measured zero.
    pub measured: usize,
    /// Moving runs counted at the whole expectation because no size is trusted.
    pub unmeasured: usize,
    /// Whether a landing waits, even when the idle rule adds no expectation.
    pub landing_waiting: bool,
    /// Bytes added for the new claim; zero with no moving run or no expectation.
    pub claim_expected: u64,
    /// Remaining bytes added to the claim; zero under the idle/no-sample rule.
    pub claim_remaining: u64,
    /// Bytes added for a waiting landing; at most one expectation.
    pub landing_expected: u64,
    /// Why claims wait regardless of free space; None when space alone decides.
    pub claims_wait_reason: Option<ClaimsWaitReason>,
    /// The reserve ([`reserve_bytes`]).
    pub reserve: u64,
    /// What a new run is expected to take ([`expected_run_size`]).
    pub expected: Option<u64>,
    /// What a claim needs free: the reserve with no run moving or no
    /// expectation, else the reserve, the expectation, the moving runs'
    /// remaining and, with a landing waiting, the expectation once more.
    pub claim: u64,
    /// What a landing's verification needs free: the reserve with no run
    /// moving or no expectation, else the reserve and the expectation.
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
            "reserve {} (volume {}, share {:.2}%); expected {} (samples {}); moving {}, remaining {} (measured {}, unmeasured {}); landing_waiting {}, landing_expected {}; claim additions: expected {}, remaining {}; claims_wait {}",
            show(self.reserve),
            show(self.volume),
            self.reserve_share * 100.0,
            self.expected.map_or_else(|| "none yet".into(), show),
            self.sample_count,
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
        ) + &self
            .relaxed
            .map_or_else(String::new, |relaxed| format!("; {}", relaxed.message()))
    }
}

impl DiskState {
    /// What a claim and a landing need free now, with the reserve `share`
    /// of the volume ([`reserve_bytes`]). No factor is applied: a run
    /// larger than expected may fail for want of space and is retried.
    ///
    /// Under a relaxation ([`Self::relaxed`], ADR-t4727-1) only the terms
    /// it names are loosened: `expected` stands for the samples'
    /// expectation, of a new run and of a landing, while there is one (a
    /// project with no sample still runs one at a time); the waiting
    /// landing's expectation is not added to a claim; the moving runs'
    /// remaining counts 0. The reserve, a run with none moving and the
    /// landing first with none moving are kept.
    pub fn needs(&self, share: f64) -> DiskNeeds {
        let reserve = reserve_bytes(self.total, share);
        let moving = self.moving > 0;
        let relax = self
            .relaxed
            .map(|relaxed| relaxed.relax)
            .unwrap_or_default();
        let expected = self
            .expected
            .map(|sampled| relax.expected.unwrap_or(sampled));
        let remaining = if relax.no_remaining {
            0
        } else {
            self.remaining
        };
        let adds_landing = self.landing_waiting && !relax.no_landing_expectation;
        let landing = match (moving, expected) {
            (true, Some(expected)) => reserve.saturating_add(expected),
            _ => reserve,
        };
        let claim = match (moving, expected) {
            (true, Some(expected)) => reserve
                .saturating_add(expected)
                .saturating_add(remaining)
                .saturating_add(if adds_landing { expected } else { 0 }),
            _ => reserve,
        };
        DiskNeeds {
            volume: self.total,
            reserve_share: share,
            moving: self.moving,
            remaining: self.remaining,
            landing_waiting: self.landing_waiting,
            claim_expected: if moving { expected.unwrap_or(0) } else { 0 },
            claim_remaining: if moving && expected.is_some() {
                remaining
            } else {
                0
            },
            landing_expected: if moving && adds_landing {
                expected.unwrap_or(0)
            } else {
                0
            },
            claims_wait_reason: if moving && expected.is_none() {
                Some(ClaimsWaitReason::NoSamples)
            } else if !moving && self.landing_waiting {
                Some(ClaimsWaitReason::LandingFirst)
            } else {
                None
            },
            reserve,
            expected,
            claim,
            landing,
            claims_wait: if moving {
                expected.is_none()
            } else {
                self.landing_waiting
            },
            relaxed: self.relaxed,
            ..DiskNeeds::default()
        }
    }

    /// Whether a claim and a landing may go ([`Self::needs`]). Unread free
    /// space holds nothing; below the reserve both are held. Above it, the
    /// queue always moves on: a landing goes with no run moving or no
    /// expectation, and otherwise when what is free above the reserve
    /// holds the expectation. A claim with no run moving goes unless a
    /// landing waits (the landing goes first, and frees the worktree; the
    /// next pass decides the claim); with runs moving and no expectation
    /// it is held (a project with no records runs one at a time);
    /// otherwise it goes when what is free above the reserve holds the
    /// expectation, the moving runs' remaining and, with a landing
    /// waiting, the expectation once more.
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

    /// How many new runs may be claimed this pass ([`Self::verdict`]);
    /// `None` for no limit (unread free space, or an expectation of 0).
    /// With no run moving the first goes whatever it is expected to take,
    /// and more only as the room above the reserve holds the expectation
    /// for each; with runs moving, each claim needs the expectation above
    /// what the moving runs and a waiting landing need.
    pub fn claims(&self, share: f64) -> Option<usize> {
        let free = self.free?;
        if !self.verdict(share).claim {
            return Some(0);
        }
        let needs = self.needs(share);
        let Some(expected) = needs.expected else {
            return Some(1);
        };
        if expected == 0 {
            return None;
        }
        let each = |room: u64| usize::try_from(room / expected).unwrap_or(usize::MAX);
        if self.moving == 0 {
            return Some(each(free - needs.reserve).max(1));
        }
        // What the moving runs and a waiting landing need: what a claim
        // needs less the expectation of the first new run.
        Some(each(free - (needs.claim - expected)))
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

/// What the moving runs are expected to take yet, together
/// ([`DiskState::remaining`]): for each, the expectation less its size now
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
    /// The expectation of a new run, and of a landing, in place of the
    /// samples'; `None` keeps the samples'.
    #[serde(rename = "expected_bytes")]
    pub expected: Option<u64>,
    /// A claim adds no expectation for a waiting landing.
    pub no_landing_expectation: bool,
    /// The moving runs' remaining counts 0.
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
    fn breakdown_additions_match_both_thresholds_under_the_existing_rules() {
        for (expected, moving, waiting, remaining) in [
            (Some(10 * GIB), 0, false, 0),
            (None, 2, false, 0),
            (None, 0, true, 0),
            (Some(10 * GIB), 0, true, 0),
            (
                Some(10 * GIB),
                2,
                true,
                moving_remaining(Some(10 * GIB), [Some(4 * GIB), None]),
            ),
        ] {
            let state = DiskState {
                free: Some(1),
                total: 200 * GIB,
                expected,
                moving,
                remaining,
                landing_waiting: waiting,
                relaxed: None,
            };
            let mut needs = state.needs(0.05);
            needs.measured = usize::from(remaining > 0);
            needs.unmeasured = moving - needs.measured;
            assert_eq!(
                needs.claim,
                needs
                    .reserve
                    .saturating_add(needs.claim_expected)
                    .saturating_add(needs.claim_remaining)
                    .saturating_add(needs.landing_expected)
            );
            assert_eq!(
                needs.landing,
                needs.reserve.saturating_add(needs.claim_expected)
            );
            assert_eq!(needs.volume, 200 * GIB);
            assert_eq!(needs.reserve, 10 * GIB);
            assert_eq!(needs.measured + needs.unmeasured, moving);
            assert_eq!(needs.claims_wait, needs.claims_wait_reason.is_some());
            if remaining > 0 {
                assert_eq!(needs.claim, 46 * GIB);
                assert_eq!(needs.landing_expected, 10 * GIB);
                assert_eq!(needs.unmeasured, 1);
            }
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
    fn the_settings_are_set_by_name() {
        assert_eq!(DiskConfig::KEYS, ["reserve"]);
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

    /// Acceptance 1, the stretch of 2026-11-03: one 45.6 GiB build among
    /// 20 of 8 to 14 GiB does not move the expectation above 14 GiB, and
    /// with 71 GiB free of 228 GiB a claim goes with no run moving or one.
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
                remaining: 0,
                landing_waiting: false,
                relaxed: None,
            };
            assert!(state.verdict(DEFAULT_RESERVE_SHARE).claim, "{moving}");
        }
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

    /// Acceptance 4 of the task: with no run moving and room above the
    /// reserve one run is claimed and a landing goes, whatever the
    /// remaining.
    #[test]
    fn with_no_run_moving_a_claim_and_a_landing_go_whatever_the_remaining() {
        for remaining in [0, 10 * GIB, u64::MAX] {
            let state = DiskState {
                free: Some(6 * GIB),
                total: 0,
                expected: Some(10 * GIB),
                moving: 0,
                remaining,
                landing_waiting: false,
                relaxed: None,
            };
            assert_eq!(
                state.verdict(DEFAULT_RESERVE_SHARE),
                DiskVerdict {
                    claim: true,
                    landing: true
                },
                "{remaining}"
            );
            assert_eq!(state.claims(DEFAULT_RESERVE_SHARE), Some(1), "{remaining}");
        }
    }

    /// Acceptance 2: once every sample is older than 14 days there is no
    /// expectation, and with no run moving a claim goes above the reserve.
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
            remaining: 0,
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

    /// Acceptance 3: a project with no sample runs one at a time, with no
    /// setting.
    #[test]
    fn no_sample_runs_one_at_a_time() {
        let state = |moving| DiskState {
            free: Some(100 * GIB),
            total: 228 * GIB,
            expected: expected_run_size(&[], NOW),
            moving,
            remaining: 0,
            landing_waiting: false,
            relaxed: None,
        };
        assert!(state(0).verdict(DEFAULT_RESERVE_SHARE).claim);
        assert!(!state(1).verdict(DEFAULT_RESERVE_SHARE).claim);
        // A landing goes with no expectation, whatever moves.
        assert!(state(1).verdict(DEFAULT_RESERVE_SHARE).landing);
    }

    #[test]
    fn the_reserve_is_5_gib_or_5_percent() {
        let reserve = reserve_bytes(gib_f(228.3), DEFAULT_RESERVE_SHARE);
        assert_eq!(gib(reserve as f64), "11.4 GiB");
        assert_eq!(reserve_bytes(50 * GIB, DEFAULT_RESERVE_SHARE), 5 * GIB);
        assert_eq!(reserve_bytes(0, DEFAULT_RESERVE_SHARE), MIN_RESERVE_BYTES);
        assert_eq!(reserve_bytes(1_000 * GIB, 0.10), 100 * GIB);
    }

    /// Acceptance 6 and 7: the reserve, landings first with no run moving,
    /// and what a claim needs with runs moving, to the byte.
    #[test]
    fn the_verdict_holds_below_the_reserve_and_otherwise_moves_on() {
        let total = 50 * GIB; // the reserve is 5 GiB
        let reserve = 5 * GIB;
        let base = DiskState {
            free: Some(reserve),
            total,
            expected: Some(10 * GIB),
            moving: 0,
            remaining: 0,
            landing_waiting: false,
            relaxed: None,
        };
        let verdict = |state: DiskState| state.verdict(DEFAULT_RESERVE_SHARE);
        let both = |claim, landing| DiskVerdict { claim, landing };
        // Below the reserve both are held, whatever moves.
        for moving in [0, 2] {
            let below = DiskState {
                free: Some(reserve - 1),
                moving,
                expected: None,
                ..base
            };
            assert_eq!(verdict(below), both(false, false), "{moving}");
        }
        // At the reserve with no run moving both go, the claim only with
        // no landing waiting.
        assert_eq!(verdict(base), both(true, true));
        let waiting = DiskState {
            landing_waiting: true,
            ..base
        };
        assert_eq!(verdict(waiting), both(false, true));
        // Runs moving with no expectation: the claim is held, the
        // landing goes.
        let unknown = DiskState {
            free: Some(100 * GIB),
            expected: None,
            moving: 1,
            landing_waiting: true,
            ..base
        };
        assert_eq!(verdict(unknown), both(false, true));
        // Unread free space holds nothing.
        for moving in [0, 3] {
            let unread = DiskState {
                free: None,
                moving,
                landing_waiting: true,
                ..base
            };
            assert_eq!(verdict(unread), both(true, true), "{moving}");
        }
        // With runs moving: a claim needs the expectation, the remaining
        // and, with a landing waiting, the expectation again; a landing
        // the expectation.
        for landing_waiting in [false, true] {
            let need = 10 * GIB + 3 * GIB + if landing_waiting { 10 * GIB } else { 0 };
            let moving = DiskState {
                free: Some(reserve + need),
                moving: 2,
                remaining: 3 * GIB,
                landing_waiting,
                ..base
            };
            assert_eq!(verdict(moving), both(true, true), "{landing_waiting}");
            let short = DiskState {
                free: Some(reserve + need - 1),
                ..moving
            };
            assert!(!verdict(short).claim, "{landing_waiting}");
            assert!(verdict(short).landing, "{landing_waiting}");
        }
        // A landing with runs moving needs the expectation free above the
        // reserve.
        let landing = DiskState {
            free: Some(reserve + 10 * GIB),
            moving: 1,
            ..base
        };
        assert!(verdict(landing).landing);
        assert!(
            !verdict(DiskState {
                free: Some(reserve + 10 * GIB - 1),
                ..landing
            })
            .landing
        );
    }

    /// The needs give the verdict's thresholds to the byte, and the claims
    /// of a pass: one at least with no run moving, then one per expectation
    /// the room holds.
    #[test]
    fn the_needs_give_the_verdict_and_bound_the_claims_of_a_pass() {
        let reserve = 5 * GIB;
        let state = |free: u64, expected: Option<u64>, moving, landing_waiting| DiskState {
            free: Some(free),
            total: 50 * GIB,
            expected,
            moving,
            remaining: 0,
            landing_waiting,
            relaxed: None,
        };
        let share = DEFAULT_RESERVE_SHARE;
        let e = Some(10 * GIB);
        assert_eq!(
            state(0, e, 2, true).needs(share),
            DiskNeeds {
                reserve,
                expected: e,
                claim: reserve + 20 * GIB,
                landing: reserve + 10 * GIB,
                claims_wait: false,
                volume: 50 * GIB,
                reserve_share: share,
                moving: 2,
                landing_waiting: true,
                claim_expected: 10 * GIB,
                landing_expected: 10 * GIB,
                ..DiskNeeds::default()
            }
        );
        assert_eq!(state(0, e, 0, false).needs(share).claim, reserve);
        assert_eq!(state(0, e, 0, false).needs(share).landing, reserve);
        assert!(state(0, e, 0, true).needs(share).claims_wait);
        assert!(state(0, None, 1, false).needs(share).claims_wait);
        assert!(!state(0, None, 0, false).needs(share).claims_wait);
        // A claim goes exactly when the claims of the pass are not 0.
        for free in [
            0,
            reserve - 1,
            reserve,
            14 * GIB,
            15 * GIB,
            26 * GIB,
            60 * GIB,
        ] {
            for expected in [None, Some(0), e] {
                for moving in [0, 1, 3] {
                    for waiting in [false, true] {
                        let state = state(free, expected, moving, waiting);
                        assert_eq!(
                            state.verdict(share).claim,
                            state.claims(share) != Some(0),
                            "{state:?}"
                        );
                    }
                }
            }
        }
        // No run moving: one whatever the expectation, more as the room
        // above the reserve holds it; none while a landing waits.
        assert_eq!(state(reserve, e, 0, false).claims(share), Some(1));
        assert_eq!(state(reserve, None, 0, false).claims(share), Some(1));
        assert_eq!(state(26 * GIB, e, 0, false).claims(share), Some(2));
        assert_eq!(state(26 * GIB, e, 0, true).claims(share), Some(0));
        // Runs moving: one per expectation above a waiting landing's.
        assert_eq!(state(35 * GIB, e, 2, false).claims(share), Some(3));
        assert_eq!(state(35 * GIB, e, 2, true).claims(share), Some(2));
        assert_eq!(state(24 * GIB, e, 2, true).claims(share), Some(0));
        assert_eq!(state(60 * GIB, None, 2, false).claims(share), Some(0));
        // No limit: an expectation of 0, or unread free space.
        assert_eq!(state(60 * GIB, Some(0), 2, true).claims(share), None);
        let unread = DiskState {
            free: None,
            ..state(0, e, 2, true)
        };
        assert_eq!(unread.claims(share), None);
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

    /// Acceptance 1 and 3: each term loosens its part of the needs alone,
    /// out of a relaxation the needs are as before, and the reserve is
    /// never loosened: below it both the claim and the landing are held.
    #[test]
    fn each_term_loosens_its_part_and_the_reserve_holds() {
        let total = 200 * GIB; // the reserve is 10 GiB
        let base = DiskState {
            free: Some(40 * GIB),
            total,
            expected: Some(10 * GIB),
            moving: 2,
            remaining: 15 * GIB,
            landing_waiting: true,
            relaxed: None,
        };
        let needs = |relax: Option<DiskRelax>| {
            DiskState {
                relaxed: relax.map(relaxed),
                ..base
            }
            .needs(DEFAULT_RESERVE_SHARE)
        };
        // reserve 10 + expected 10 + remaining 15 + landing 10.
        let before = needs(None);
        assert_eq!((before.claim, before.landing), (45 * GIB, 20 * GIB));
        assert_eq!(before.relaxed, None);
        assert!(!base.verdict(DEFAULT_RESERVE_SHARE).claim);
        let expected = needs(Some(DiskRelax {
            expected: Some(4 * GIB),
            ..DiskRelax::default()
        }));
        assert_eq!((expected.claim, expected.landing), (33 * GIB, 14 * GIB));
        assert_eq!(expected.expected, Some(4 * GIB));
        assert!(expected.relaxed.is_some());
        assert!(expected.message().contains("relaxed until"));
        let landing = needs(Some(DiskRelax {
            no_landing_expectation: true,
            ..DiskRelax::default()
        }));
        assert_eq!((landing.claim, landing.landing), (35 * GIB, 20 * GIB));
        assert_eq!(landing.landing_expected, 0);
        let remaining = needs(Some(DiskRelax {
            no_remaining: true,
            ..DiskRelax::default()
        }));
        assert_eq!((remaining.claim, remaining.landing), (30 * GIB, 20 * GIB));
        assert_eq!(remaining.claim_remaining, 0);
        let all = DiskRelax {
            expected: Some(4 * GIB),
            no_landing_expectation: true,
            no_remaining: true,
        };
        let loosest = needs(Some(all));
        assert_eq!((loosest.claim, loosest.landing), (14 * GIB, 14 * GIB));
        assert_eq!(loosest.reserve, before.reserve);
        let relaxed_state = DiskState {
            relaxed: Some(relaxed(all)),
            ..base
        };
        assert!(relaxed_state.verdict(DEFAULT_RESERVE_SHARE).claim);
        // 40 free less the reserve's 10 (the moving runs' remaining and the
        // waiting landing count nothing): 7 claims of 4 GiB each.
        assert_eq!(relaxed_state.claims(DEFAULT_RESERVE_SHARE), Some(7));
        // Below the reserve nothing goes, relaxed or not.
        let below = DiskState {
            free: Some(10 * GIB - 1),
            ..relaxed_state
        };
        assert_eq!(
            below.verdict(DEFAULT_RESERVE_SHARE),
            DiskVerdict {
                claim: false,
                landing: false
            }
        );
        assert_eq!(below.claims(DEFAULT_RESERVE_SHARE), Some(0));
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

    /// Acceptance 3: under a relaxation one run still goes with none
    /// moving, a waiting landing still goes before a claim with none
    /// moving, and a project with no sample still runs one at a time.
    #[test]
    fn a_relaxation_keeps_the_progress_and_the_landing_first() {
        let all = relaxed(DiskRelax {
            expected: Some(GIB),
            no_landing_expectation: true,
            no_remaining: true,
        });
        let idle = DiskState {
            free: Some(11 * GIB),
            total: 200 * GIB,
            expected: Some(50 * GIB),
            moving: 0,
            remaining: 0,
            landing_waiting: true,
            relaxed: Some(all),
        };
        let needs = idle.needs(DEFAULT_RESERVE_SHARE);
        assert_eq!(
            needs.claims_wait_reason,
            Some(ClaimsWaitReason::LandingFirst)
        );
        assert_eq!(
            idle.verdict(DEFAULT_RESERVE_SHARE),
            DiskVerdict {
                claim: false,
                landing: true
            }
        );
        let alone = DiskState {
            landing_waiting: false,
            ..idle
        };
        assert!(alone.verdict(DEFAULT_RESERVE_SHARE).claim);
        assert_eq!(alone.claims(DEFAULT_RESERVE_SHARE), Some(1));
        let unsampled = DiskState {
            free: Some(100 * GIB),
            expected: None,
            moving: 1,
            landing_waiting: false,
            ..idle
        };
        let needs = unsampled.needs(DEFAULT_RESERVE_SHARE);
        assert_eq!(needs.claims_wait_reason, Some(ClaimsWaitReason::NoSamples));
        assert!(!unsampled.verdict(DEFAULT_RESERVE_SHARE).claim);
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
}
