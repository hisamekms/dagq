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
        return sizes.last().copied();
    }
    let rank = (n * QUANTILE_TENTHS).div_ceil(10);
    sizes.get(rank.max(1) - 1).copied()
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
}

/// Whether a claim and a landing may go now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskVerdict {
    pub claim: bool,
    pub landing: bool,
}

/// The free bytes a claim and a landing need now ([`DiskState::needs`]),
/// for the holds, the cleanup and the disk ask.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DiskNeeds {
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
}

impl DiskState {
    /// What a claim and a landing need free now, with the reserve `share`
    /// of the volume ([`reserve_bytes`]). No factor is applied: a run
    /// larger than expected may fail for want of space and is retried.
    pub fn needs(&self, share: f64) -> DiskNeeds {
        let reserve = reserve_bytes(self.total, share);
        let moving = self.moving > 0;
        let landing = match (moving, self.expected) {
            (true, Some(expected)) => reserve.saturating_add(expected),
            _ => reserve,
        };
        let claim = match (moving, self.expected) {
            (true, Some(expected)) => reserve
                .saturating_add(expected)
                .saturating_add(self.remaining)
                .saturating_add(if self.landing_waiting { expected } else { 0 }),
            _ => reserve,
        };
        DiskNeeds {
            reserve,
            expected: self.expected,
            claim,
            landing,
            claims_wait: if moving {
                self.expected.is_none()
            } else {
                self.landing_waiting
            },
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
        let Some(expected) = self.expected else {
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
