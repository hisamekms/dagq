//! The environment a run executes in and the resources it spends
//! (ADR-t1662-1 decision 8, ADR-t1662-3 decisions 3 to 6,
//! docs/design/measurement.md "実行環境と資源"): what an environment says
//! it is when a run is attached to it ([`EnvironmentAttached`], the event
//! `environment_attached`), the resources in the common units every
//! environment measures in ([`Resources`]), a measured window of a run's
//! ([`RunResource`]) and the fold of the windows into the intervals of the
//! run's row ([`fold`]), the waits an environment adds
//! ([`EnvironmentWait`], intervals of `blocker: infra`), and what the
//! host's implementation reads of `ps` ([`processes`], [`ProcessMeter`]).
//! Pure: what measures is the port's adapter.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::host_metrics::HostSample;
use super::ledger::fold::Interval;
use super::session_step::{ResourceSample, SessionResource};
use super::{EventKind, RunEvent};

/// The kind of an environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvKind {
    /// The supervisor's host itself.
    Host,
    /// A container on a node (podman, docker).
    Container,
    /// A pod of a Kubernetes cluster.
    Kubernetes,
}

/// What an environment says it is as a run is attached to it (the payload
/// of `environment_attached`): its kind, the node it runs on (the
/// NodeSampleStore's `node_id` of the same node), the digest of its image
/// (`None` with no image), its CPU limit in cores and its memory limit in
/// bytes (the node's own on the host), and the node's architecture
/// (`std::env::consts::ARCH`'s names).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentAttached {
    pub env_kind: EnvKind,
    pub node_id: String,
    pub image_digest: Option<String>,
    pub cpu_limit: Option<f64>,
    pub mem_limit: Option<i64>,
    pub arch: String,
}

impl EnvironmentAttached {
    /// The payload of its `environment_attached`.
    pub fn payload(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// The attachment a recorded `environment_attached`'s payload says.
    pub fn of_payload(payload: &Value) -> Option<Self> {
        serde_json::from_value(payload.clone()).ok()
    }
}

/// A wait an environment adds to a run that the host does not have
/// (ADR-t1662-3 decision 5): each is recorded as a phase of `blocker:
/// infra` ([`super::run_phase::Phase::Environment`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentWait {
    /// The run waits to be placed on a node.
    Placement,
    /// The node pulls the run's image.
    ImagePull,
    /// The environment is made ready (volumes, the checkout copied in).
    Preparation,
    /// The run's results are brought back (its branch, its files).
    ResultReturn,
}

impl EnvironmentWait {
    /// The name of its phase in `run_phase_changed`.
    pub const fn phase_name(self) -> &'static str {
        match self {
            Self::Placement => "environment_placement",
            Self::ImagePull => "environment_image_pull",
            Self::Preparation => "environment_preparation",
            Self::ResultReturn => "environment_result_return",
        }
    }
}

/// What a run or a node spent over a window, in the common units every
/// environment's measure returns (ADR-t1662-3 decision 4), `None` where
/// the environment could not measure it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Resources {
    /// CPU time spent in the window, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_seconds: Option<f64>,
    /// The CPU limit, in cores (the node's cores where nothing limits it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_limit: Option<f64>,
    /// The most memory used, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_max_bytes: Option<i64>,
    /// Time the CPU limit throttled, in seconds; `None` where nothing
    /// limits the CPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttled_seconds: Option<f64>,
    /// The CPU pressure (PSI `some avg10`, a percentage), where the
    /// environment has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_pressure: Option<f64>,
    /// The load average over a minute, where the environment has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_average: Option<f64>,
}

impl Resources {
    /// Whether it holds no value.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// What an executor measured from inside, in the same units.
    pub fn of_sample(sample: &ResourceSample) -> Self {
        Self {
            cpu_seconds: sample.cpu_seconds,
            cpu_limit: sample.cpu_limit,
            memory_max_bytes: sample.memory_max_bytes,
            throttled_seconds: sample.throttled_seconds,
            cpu_pressure: sample.cpu_pressure,
            load_average: sample.load_average,
        }
    }

    /// Add `share` (0 to 1) of `other`: its time spent is added in that
    /// share, its limits, its memory, its pressure and its load are the
    /// larger of the two.
    fn add(&mut self, other: &Self, share: f64) {
        let sum = |mine: Option<f64>, theirs: Option<f64>| match (mine, theirs) {
            (mine, Some(theirs)) => Some(mine.unwrap_or(0.0) + theirs * share),
            (mine, None) => mine,
        };
        let most = |mine: Option<f64>, theirs: Option<f64>| match (mine, theirs) {
            (Some(mine), Some(theirs)) => Some(mine.max(theirs)),
            (mine, theirs) => mine.or(theirs),
        };
        self.cpu_seconds = sum(self.cpu_seconds, other.cpu_seconds);
        self.throttled_seconds = sum(self.throttled_seconds, other.throttled_seconds);
        self.cpu_limit = most(self.cpu_limit, other.cpu_limit);
        self.cpu_pressure = most(self.cpu_pressure, other.cpu_pressure);
        self.load_average = most(self.load_average, other.load_average);
        self.memory_max_bytes = match (self.memory_max_bytes, other.memory_max_bytes) {
            (Some(mine), Some(theirs)) => Some(mine.max(theirs)),
            (mine, theirs) => mine.or(theirs),
        };
    }
}

/// The resources of the host over a window of `interval_secs` from one
/// sample of the host's load (its `cpu_total`, a percent of one core, and
/// its `mem_used_mb`), with the host's `cores` as its CPU limit and its
/// `cpu_pressure` where the host has PSI: the host's implementation of the
/// measure of a node. Nothing limits the host's CPU, so nothing throttles
/// it.
pub fn host_node_resources(
    sample: &HostSample,
    interval_secs: f64,
    cores: Option<f64>,
    cpu_pressure: Option<f64>,
) -> Resources {
    let value = |column: &str| sample.values.get(column).copied();
    #[allow(clippy::cast_possible_truncation)]
    let bytes = value("mem_used_mb").map(|mb| (mb * 1_048_576.0).round() as i64);
    Resources {
        cpu_seconds: value("cpu_total").map(|percent| percent / 100.0 * interval_secs),
        cpu_limit: cores,
        memory_max_bytes: bytes,
        throttled_seconds: None,
        cpu_pressure,
        load_average: value("load1"),
    }
}

/// How long a sample of a node's load at `unix` stands for, as the host's
/// summary counts its CPU seconds: the time since the node's row before
/// it (`previous`, whichever supervisor wrote it), so a sample taken late
/// counts the time it was late and two supervisors sampling apart each
/// count their share, up to twice `interval`, so a stretch no supervisor
/// recorded is not counted as if the load of the sample had lasted all
/// along. The node's first sample stands for `interval`.
pub fn sample_stands_for(
    previous: Option<i64>,
    unix: i64,
    interval: std::time::Duration,
) -> std::time::Duration {
    let most = interval.saturating_mul(2);
    previous.map_or(interval, |previous| {
        let gap = u64::try_from(unix.saturating_sub(previous)).unwrap_or(0);
        std::time::Duration::from_secs(gap).min(most)
    })
}

/// The CPU pressure of `/proc/pressure/cpu`'s text: its `some` line's
/// `avg10`.
pub fn cpu_pressure(text: &str) -> Option<f64> {
    text.lines()
        .find(|line| line.starts_with("some "))?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse()
        .ok()
        .filter(|value: &f64| value.is_finite() && *value >= 0.0)
}

/// One window of a run's resources the supervisor measured from outside,
/// as the store holds it: its id in the order the rows came in (a cursor
/// reads past it), its run, the `environment_attached` it was measured
/// under (its event id), the node, the window (unix ms, its end the time
/// of the measure) and what it spent. `last` marks the measure the
/// supervisor takes after the run left the environment it was attached
/// to (its session ended, it came to rest, it ended): what the run's
/// `final` waits on ([`resources_recorded`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunResource {
    pub id: i64,
    pub run_id: String,
    pub attachment: i64,
    pub node_id: String,
    pub from_ms: i64,
    pub until_ms: i64,
    pub resources: Resources,
    pub last: bool,
}

/// A window of resources on the supervisor's clock: one of a run's
/// measured from outside, or one an executor measured from inside placed
/// by its session's start.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub from_ms: i64,
    pub until_ms: i64,
    pub resources: Resources,
}

impl Window {
    pub fn of_run(row: &RunResource) -> Self {
        Self {
            from_ms: row.from_ms,
            until_ms: row.until_ms,
            resources: row.resources.clone(),
        }
    }

    /// An executor's `sample` placed from its session's start, `start`
    /// (unix ms, when the control side registered the session).
    pub fn of_sample(sample: &SessionResource, start: i64) -> Self {
        let from = start + sample.sample.offset_ms;
        Self {
            from_ms: from,
            until_ms: from + sample.sample.duration_ms,
            resources: Resources::of_sample(&sample.sample),
        }
    }
}

/// A sample from inside a fold counted, placed on the supervisor's clock:
/// its session and seq, so it is counted once, and its window, so a
/// later fold keeps what it spent once the retention removed it from the
/// store ([`counted_inside`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CountedSample {
    pub session_id: String,
    pub seq: i64,
    pub from_ms: i64,
    pub until_ms: i64,
    pub resources: Resources,
}

impl CountedSample {
    /// An executor's `sample` placed from its session's start, `start`
    /// ([`Window::of_sample`]).
    pub fn of_sample(sample: &SessionResource, start: i64) -> Self {
        let window = Window::of_sample(sample, start);
        Self {
            session_id: sample.session_id.clone(),
            seq: sample.sample.seq,
            from_ms: window.from_ms,
            until_ms: window.until_ms,
            resources: window.resources,
        }
    }

    pub fn window(&self) -> Window {
        Window {
            from_ms: self.from_ms,
            until_ms: self.until_ms,
            resources: self.resources.clone(),
        }
    }
}

/// The samples from inside a fold counts: those an earlier fold counted
/// (`stored`), whether the store still holds them or not, and those of
/// `placed` it did not, each session's seq once.
pub fn counted_inside(stored: &[CountedSample], placed: &[CountedSample]) -> Vec<CountedSample> {
    let mut seen: BTreeSet<(&str, i64)> = BTreeSet::new();
    stored
        .iter()
        .chain(placed)
        .filter(|sample| seen.insert((sample.session_id.as_str(), sample.seq)))
        .cloned()
        .collect()
}

/// Where a run's resources, or an interval's, were measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The supervisor, from outside, through the environment's API.
    Outside,
    /// The executor, from inside, where the environment let the
    /// supervisor measure nothing.
    Inside,
    /// A run's only: some of its intervals from outside, the others, where
    /// the supervisor measured nothing, from inside.
    Mixed,
}

/// The key of a run row's detail its resources are under.
pub const DETAIL_KEY: &str = "resources";

/// A run's resources folded into its intervals: where they were measured
/// (the run's and each interval's), the whole run's and each interval's
/// (in the order of the row's intervals; an interval no window reaches
/// has none), the samples from inside it counted ([`counted_inside`])
/// and the starts of their sessions on the supervisor's clock, so a later
/// sample is placed once the retention removed the session's owner.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Folded {
    pub source: Option<Source>,
    pub total: Resources,
    pub intervals: Vec<Resources>,
    /// Where each interval's were measured, `None` for one with none.
    #[serde(default)]
    pub sources: Vec<Option<Source>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inside: Vec<CountedSample>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub starts: BTreeMap<String, i64>,
}

/// `windows` spread over `intervals`: the whole of them and each
/// interval's. A window's time spent goes to each interval in the share
/// of the window it overlaps (a window of no length to the interval it
/// falls in), its limits, its memory, its pressure and its load to each
/// interval it overlaps as the most of them.
fn spread(intervals: &[Interval], windows: &[Window]) -> (Resources, Vec<Resources>) {
    let mut total = Resources::default();
    let mut each = vec![Resources::default(); intervals.len()];
    for window in windows {
        total.add(&window.resources, 1.0);
        let length = window.until_ms - window.from_ms;
        for (index, interval) in intervals.iter().enumerate() {
            let share = if length <= 0 {
                let at = window.until_ms;
                if interval.start <= at && at < interval.end.max(interval.start + 1) {
                    1.0
                } else {
                    continue;
                }
            } else {
                let overlap =
                    window.until_ms.min(interval.end) - window.from_ms.max(interval.start);
                if overlap <= 0 {
                    continue;
                }
                #[allow(clippy::cast_precision_loss)]
                let share = overlap as f64 / length as f64;
                share
            };
            each[index].add(&window.resources, share);
        }
    }
    (total, each)
}

/// Fold a run's windows into its `intervals` (ADR-t1662-3 decision 1):
/// each interval takes those the supervisor measured from outside
/// (`outside`) where they give it a value, else those its executors
/// measured from inside (`inside`), where the environment let the
/// supervisor measure nothing ([`spread`] for how a window is shared).
/// The whole run's are the windows from outside with the intervals that
/// took the samples from inside, or all the samples from inside where
/// nothing was measured from outside. `None` with no window of a value.
pub fn fold(intervals: &[Interval], outside: &[Window], inside: &[Window]) -> Option<Folded> {
    let measured = |windows: &[Window]| windows.iter().any(|w| !w.resources.is_empty());
    let (any_outside, any_inside) = (measured(outside), measured(inside));
    if !any_outside && !any_inside {
        return None;
    }
    let (outside_total, outside_each) = spread(intervals, outside);
    let (inside_total, inside_each) = spread(intervals, inside);
    let mut total = if any_outside {
        outside_total
    } else {
        inside_total
    };
    let mut each = Vec::with_capacity(intervals.len());
    let mut sources = Vec::with_capacity(intervals.len());
    for (from_outside, from_inside) in outside_each.into_iter().zip(inside_each) {
        if !from_outside.is_empty() {
            each.push(from_outside);
            sources.push(Some(Source::Outside));
        } else if !from_inside.is_empty() {
            if any_outside {
                total.add(&from_inside, 1.0);
            }
            each.push(from_inside);
            sources.push(Some(Source::Inside));
        } else {
            each.push(Resources::default());
            sources.push(None);
        }
    }
    let took = |source| sources.contains(&Some(source));
    let source = match (any_outside, took(Source::Inside)) {
        (true, true) => Source::Mixed,
        (true, false) => Source::Outside,
        (false, _) => Source::Inside,
    };
    Some(Folded {
        source: Some(source),
        total,
        intervals: each,
        sources,
        inside: Vec::new(),
        starts: BTreeMap::new(),
    })
}

impl Folded {
    /// The detail it is kept as under [`DETAIL_KEY`].
    pub fn detail(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// What a row's `detail` holds under [`DETAIL_KEY`].
    pub fn of_detail(detail: &Value) -> Option<Self> {
        serde_json::from_value(detail.get(DETAIL_KEY)?.clone()).ok()
    }
}

/// The id of the last `environment_attached` among a run's `events`.
pub fn last_attachment(events: &[RunEvent]) -> Option<i64> {
    events
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::EnvironmentAttached)
        .map(|event| event.id.as_i64())
}

/// Whether the supervisor's last measure of a run whose events are
/// `events` and whose windows measured from outside are `rows` is
/// recorded, as the run's `final` waits on (ADR-t1662-1 (ii)): no run
/// attached to an environment (one from before the record) waits on none,
/// and one attached waits on a `last` window of its last attachment. An
/// attachment before the last is closed by the next, whatever was
/// recorded of it.
pub fn resources_recorded(events: &[RunEvent], rows: &[RunResource]) -> bool {
    last_attachment(events).is_none_or(|attached| {
        rows.iter()
            .any(|row| row.last && row.attachment == attached)
    })
}

/// A run to measure from outside: its id, the pids of its processes
/// that have not exited (those it registered, its wrapper and agent, and
/// those the control side started for it in other trees, [`RunChildren`]),
/// each counted with every process under it, and how long ago its window
/// started (seconds): a process that started before it counts only what
/// it spends from its first measure ([`ProcessMeter::window`]).
#[derive(Debug, Clone, PartialEq)]
pub struct RunProcesses {
    pub run: super::RunId,
    pub pids: Vec<u32>,
    pub window_secs: f64,
}

/// The processes the control side started for runs outside their
/// workers' trees (a review's or a recovery's headless job, a
/// verification command of the landing), shared by what starts them and
/// the measure of the runs' resources from outside, which counts each as
/// its run's ([`RunProcesses::pids`]). A process is its run's from its
/// start until the [`RunChild`] its start returned is dropped.
#[derive(Debug, Clone, Default)]
pub struct RunChildren(std::sync::Arc<std::sync::Mutex<BTreeMap<String, Vec<u32>>>>);

impl RunChildren {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u32>>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `pid` started for `run`: its until the guard is dropped.
    pub fn started(&self, run: &super::RunId, pid: u32) -> RunChild {
        self.lock().entry(run.to_string()).or_default().push(pid);
        RunChild {
            children: self.clone(),
            run: run.to_string(),
            pid,
        }
    }

    /// The pids started for `run` that are still its, ascending.
    pub fn of(&self, run: &super::RunId) -> Vec<u32> {
        let mut pids = self.lock().get(run.as_str()).cloned().unwrap_or_default();
        pids.sort_unstable();
        pids.dedup();
        pids
    }
}

/// A process of [`RunChildren`], its run's until this is dropped.
#[derive(Debug)]
#[must_use = "the process is its run's only until this is dropped"]
pub struct RunChild {
    children: RunChildren,
    run: String,
    pid: u32,
}

impl Drop for RunChild {
    fn drop(&mut self) {
        let mut runs = self.children.lock();
        if let Some(pids) = runs.get_mut(&self.run) {
            if let Some(at) = pids.iter().position(|pid| *pid == self.pid) {
                pids.swap_remove(at);
            }
            if pids.is_empty() {
                runs.remove(&self.run);
            }
        }
    }
}

/// One process of `ps`: its pid, its parent, how long ago it started and
/// the CPU it spent since (seconds), and its resident memory (bytes).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Process {
    pub pid: u32,
    pub ppid: u32,
    pub elapsed_secs: f64,
    pub cpu_seconds: f64,
    pub rss_bytes: i64,
}

/// The processes of `ps -A -o pid=,ppid=,etime=,time=,rss=` (`rss` in KiB,
/// `etime` and `time` as `[[dd-]hh:]mm:ss[.ff]`, macOS's minutes past 59
/// too); a line it cannot read is left out.
pub fn processes(ps: &str) -> Vec<Process> {
    ps.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let elapsed_secs = cpu_time(fields.next()?)?;
            let cpu_seconds = cpu_time(fields.next()?)?;
            let rss_kib: i64 = fields.next()?.parse().ok()?;
            Some(Process {
                pid,
                ppid,
                elapsed_secs,
                cpu_seconds,
                rss_bytes: rss_kib.saturating_mul(1024),
            })
        })
        .collect()
}

/// The seconds of a `ps` `time` or `etime`: `[[dd-]hh:]mm:ss[.ff]`.
fn cpu_time(text: &str) -> Option<f64> {
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<f64>().ok()?, clock),
        None => (0.0, text),
    };
    let mut seconds = 0.0;
    let mut unit = 1.0;
    for part in clock.rsplit(':') {
        let value: f64 = part.parse().ok()?;
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        seconds += value * unit;
        unit *= 60.0;
        if unit > 3_600.0 * 60.0 {
            return None;
        }
    }
    Some(days * 86_400.0 + seconds)
}

/// The pids of `roots` and of every process under them.
pub fn tree(processes: &[Process], roots: &[u32]) -> BTreeSet<u32> {
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for process in processes {
        children.entry(process.ppid).or_default().push(process.pid);
    }
    let alive: BTreeSet<u32> = processes.iter().map(|process| process.pid).collect();
    let mut found = BTreeSet::new();
    let mut next: Vec<u32> = roots
        .iter()
        .copied()
        .filter(|pid| alive.contains(pid))
        .collect();
    while let Some(pid) = next.pop() {
        if found.insert(pid) {
            next.extend(children.get(&pid).into_iter().flatten().copied());
        }
    }
    found
}

/// What the host's measure of one run keeps between its windows: the CPU
/// each process of the run's tree had spent at the last one, so a window
/// counts only what was spent since. A process gone between two windows
/// takes with it what it spent since the last (`ps` reads the living
/// only), and a pid whose CPU went down is another process. A process
/// first seen that started before the window (a run adopted from another
/// supervisor, measured again after its measure was lost) counts from its
/// first measure, so what an earlier window may have counted is not
/// counted again.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessMeter {
    seen: BTreeMap<u32, f64>,
}

impl ProcessMeter {
    /// The CPU seconds the tree of `roots` spent in the window of the last
    /// `window_secs` and the resident memory it holds now, of
    /// `processes`; nothing measured (a tree with no process) is `None`
    /// for both.
    pub fn window(
        &mut self,
        processes: &[Process],
        roots: &[u32],
        window_secs: f64,
    ) -> (Option<f64>, Option<i64>) {
        let pids = tree(processes, roots);
        if pids.is_empty() {
            self.seen.clear();
            return (None, None);
        }
        let mut cpu = 0.0;
        let mut rss = 0;
        let mut seen = BTreeMap::new();
        for process in processes.iter().filter(|p| pids.contains(&p.pid)) {
            // A second of slack for `etime`'s whole seconds.
            let started_before = process.elapsed_secs > window_secs + 1.0;
            let before = match self.seen.get(&process.pid).copied() {
                Some(before) if before <= process.cpu_seconds => before,
                _ if started_before => process.cpu_seconds,
                _ => 0.0,
            };
            cpu += process.cpu_seconds - before;
            rss += process.rss_bytes;
            seen.insert(process.pid, process.cpu_seconds);
        }
        self.seen = seen;
        (Some(cpu), Some(rss))
    }
}

#[cfg(test)]
mod tests;
