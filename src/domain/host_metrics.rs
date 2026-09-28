//! The host's load the supervisor records continuously (task 516): one row
//! per sample in `<queue dir>/host/metrics-YYYYMMDD.csv` (the host's local
//! day), never an event (ADR-0040 decision 5). Here are the columns, the
//! parsers of what the host's tools print (macOS's `vm_stat` and `sysctl
//! vm.swapusage`, Linux's `/proc/meminfo` and `/proc/vmstat`, and `ps` on
//! both), the files the retention removes, and the summary `stats` (and
//! `kpi`) read of a window: the mean, the median, the maximum and the p90
//! of each column, and the CPU seconds the processes spent (goal 72). A
//! value the host could not give is an empty cell and counts in no
//! summary.

use std::collections::BTreeMap;

use serde::Serialize;

/// The directory under the queue's directory the files are written in.
pub const HOST_DIR: &str = "host";

/// How often the supervisor samples by default (`supervise
/// --host-metrics-interval`).
pub const DEFAULT_INTERVAL_SECS: u64 = 30;

/// How many local days of files are kept by default (`supervise
/// --host-metrics-retention-days`): today's and the days before it.
pub const DEFAULT_RETENTION_DAYS: u32 = 30;

/// The kinds of processes the CPU and the memory are split by, in the
/// order of the columns.
pub const PROCESS_KINDS: [&str; 5] = ["cargo", "rustc", "claude", "dagq", "other"];

/// The columns of a file, in order. `time` is the host's local time of
/// `unix` (seconds); `cpu_*` are percents of one core (`ps`'s `%cpu`,
/// summed per kind), `rss_*_mb` the resident memory per kind;
/// `mem_used_mb` is wired + active + the compressor's pages; `pageouts`
/// counts since the host booted (the summary reads it per minute).
pub const COLUMNS: [&str; 22] = [
    "time",
    "unix",
    "load1",
    "load5",
    "load15",
    "cpu_total",
    "cpu_cargo",
    "cpu_rustc",
    "cpu_claude",
    "cpu_dagq",
    "cpu_other",
    "rss_cargo_mb",
    "rss_rustc_mb",
    "rss_claude_mb",
    "rss_dagq_mb",
    "rss_other_mb",
    "mem_total_mb",
    "mem_used_mb",
    "mem_compressed_mb",
    "swap_total_mb",
    "swap_used_mb",
    "pageouts",
];

/// The cumulative column the summary reads as a rate.
const PAGEOUTS: &str = "pageouts";
/// The rate of [`PAGEOUTS`] in the summary.
const PAGEOUTS_PER_MIN: &str = "pageouts_per_min";
/// Two samples further apart than this give no pageout rate: the
/// supervisor was not running between them.
const MAX_RATE_GAP_SECS: i64 = 600;

/// One sample: the unix second it was taken at and each column it has
/// (named as in [`COLUMNS`], without `time` and `unix`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostSample {
    pub unix: i64,
    pub values: BTreeMap<&'static str, f64>,
}

impl HostSample {
    pub fn new(unix: i64) -> Self {
        Self {
            unix,
            values: BTreeMap::new(),
        }
    }

    /// Set `column` when there is a value.
    pub fn set(&mut self, column: &'static str, value: Option<f64>) {
        if let Some(value) = value.filter(|value| value.is_finite()) {
            self.values.insert(column, value);
        }
    }

    /// The load averages of 1, 5 and 15 minutes.
    pub fn with_loads(mut self, loads: [Option<f64>; 3]) -> Self {
        for (column, load) in ["load1", "load5", "load15"].into_iter().zip(loads) {
            self.set(column, load);
        }
        self
    }

    /// The CPU and memory per kind of process ([`processes`]).
    pub fn with_processes(mut self, usage: Option<&[KindUsage; 5]>) -> Self {
        let Some(usage) = usage else {
            return self;
        };
        self.set("cpu_total", Some(usage.iter().map(|kind| kind.cpu).sum()));
        for (index, kind) in usage.iter().enumerate() {
            self.set(column_of("cpu_", index, ""), Some(kind.cpu));
            self.set(column_of("rss_", index, "_mb"), Some(kind.rss_mb));
        }
        self
    }

    /// The memory, swap and pageouts ([`Memory`]).
    pub fn with_memory(mut self, memory: &Memory) -> Self {
        self.set("mem_total_mb", memory.total_mb);
        self.set("mem_used_mb", memory.used_mb);
        self.set("mem_compressed_mb", memory.compressed_mb);
        self.set("swap_total_mb", memory.swap_total_mb);
        self.set("swap_used_mb", memory.swap_used_mb);
        self.set(PAGEOUTS, memory.pageouts);
        self
    }

    /// The row of a file: the host's local time at `utc_offset_secs`, then
    /// each column, empty when the sample has no value.
    pub fn row(&self, utc_offset_secs: i64) -> String {
        let mut cells = vec![
            local_time(self.unix, utc_offset_secs),
            self.unix.to_string(),
        ];
        for column in &COLUMNS[2..] {
            cells.push(self.values.get(column).map_or_else(String::new, |value| {
                if value.fract() == 0.0 {
                    format!("{value:.0}")
                } else {
                    format!("{value:.2}")
                }
            }));
        }
        cells.join(",")
    }
}

/// The name of the column of the process kind at `index`.
fn column_of(prefix: &str, index: usize, suffix: &str) -> &'static str {
    COLUMNS
        .iter()
        .find(|column| {
            column
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(suffix))
                == Some(PROCESS_KINDS[index])
        })
        .copied()
        .unwrap_or("cpu_other")
}

/// The header line of a file.
pub fn header() -> String {
    COLUMNS.join(",")
}

/// The samples of a file's text, read by its header (so a file with other
/// or more columns is read too); rows without a unix second are skipped.
pub fn parse_file(text: &str) -> Vec<HostSample> {
    let mut lines = text.lines();
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let names: Vec<Option<&'static str>> = header
        .split(',')
        .map(|name| {
            COLUMNS
                .iter()
                .find(|column| **column == name.trim())
                .copied()
        })
        .collect();
    let mut samples = Vec::new();
    for line in lines {
        let cells: Vec<&str> = line.split(',').collect();
        let unix = names
            .iter()
            .position(|name| *name == Some("unix"))
            .and_then(|index| cells.get(index))
            .and_then(|cell| cell.trim().parse::<i64>().ok());
        let Some(unix) = unix else {
            continue;
        };
        let mut sample = HostSample::new(unix);
        for (name, cell) in names.iter().zip(&cells) {
            if let Some(name) = name.filter(|name| !matches!(*name, "time" | "unix")) {
                sample.set(name, cell.trim().parse().ok());
            }
        }
        samples.push(sample);
    }
    samples
}

/// The unix second of a file's last row, to tell whether another
/// supervisor of the queue sampled just now.
pub fn last_unix(text: &str) -> Option<i64> {
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let index = COLUMNS.iter().position(|column| *column == "unix")?;
    line.split(',').nth(index)?.trim().parse().ok()
}

/// The CPU (percent of one core) and resident memory (MB) of one kind of
/// process.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct KindUsage {
    pub cpu: f64,
    pub rss_mb: f64,
}

/// The kind of a process by its command (`ps -o comm`, a path or a
/// name): `dagq` (the name `dagq`, the fixed binary's `dagq.previous`
/// too), `claude` (the name `claude`, or Claude Code's
/// `<...>/claude/versions/<version>`), `rustc` (`rustc`, `clippy-driver`,
/// `sccache`), `cargo` (`cargo`, `cargo-*`, and what runs from a `target/`
/// directory: build scripts and test binaries), else `other`.
pub fn process_kind(command: &str) -> usize {
    let command = command.trim().trim_start_matches('(').trim_end_matches(')');
    let name = command.rsplit('/').next().unwrap_or(command);
    let kind = if name == "dagq" || name.starts_with("dagq.") {
        "dagq"
    } else if name == "claude" || command.contains("/claude/versions/") {
        "claude"
    } else if matches!(name, "rustc" | "clippy-driver" | "sccache") {
        "rustc"
    } else if name == "cargo" || name.starts_with("cargo-") || command.contains("/target/") {
        "cargo"
    } else {
        "other"
    };
    PROCESS_KINDS
        .iter()
        .position(|candidate| *candidate == kind)
        .unwrap_or(PROCESS_KINDS.len() - 1)
}

/// The CPU and memory per kind of `ps -A -o pcpu=,rss=,comm=` (rss in
/// KiB); `None` when no line could be read.
pub fn processes(ps: &str) -> Option<[KindUsage; 5]> {
    let mut usage = [KindUsage::default(); 5];
    let mut read = false;
    for line in ps.lines() {
        let mut fields = line.split_whitespace();
        let (Some(cpu), Some(rss)) = (fields.next(), fields.next()) else {
            continue;
        };
        let (Ok(cpu), Ok(rss)) = (cpu.parse::<f64>(), rss.parse::<f64>()) else {
            continue;
        };
        let command = fields.collect::<Vec<_>>().join(" ");
        let kind = &mut usage[process_kind(&command)];
        kind.cpu += cpu;
        kind.rss_mb += rss / 1024.0;
        read = true;
    }
    read.then(|| {
        usage.map(|kind| KindUsage {
            cpu: round2(kind.cpu),
            rss_mb: kind.rss_mb.round(),
        })
    })
}

/// The host's memory, swap and pageouts; each `None` when the host did
/// not give it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Memory {
    pub total_mb: Option<f64>,
    pub used_mb: Option<f64>,
    pub compressed_mb: Option<f64>,
    pub swap_total_mb: Option<f64>,
    pub swap_used_mb: Option<f64>,
    pub pageouts: Option<f64>,
}

/// macOS: the used memory and the pageouts of `vm_stat`, the physical
/// memory of `sysctl -n hw.memsize` (bytes) and the swap of `sysctl -n
/// vm.swapusage`.
pub fn darwin_memory(vm_stat: Option<&str>, memsize: Option<&str>, swap: Option<&str>) -> Memory {
    let mut memory = Memory {
        total_mb: memsize
            .and_then(|text| text.trim().parse::<f64>().ok())
            .map(|bytes| (bytes / MIB).round()),
        ..Memory::default()
    };
    if let Some(text) = vm_stat {
        let page = text
            .lines()
            .next()
            .and_then(|line| line.split("page size of ").nth(1))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|size| size.parse::<f64>().ok());
        let count = |label: &str| {
            text.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                (name.trim().trim_matches('"') == label)
                    .then(|| value.trim().trim_end_matches('.').parse::<f64>().ok())
                    .flatten()
            })
        };
        let pages = |labels: &[&str]| {
            labels
                .iter()
                .map(|label| count(label))
                .sum::<Option<f64>>()
                .zip(page)
                .map(|(pages, page)| (pages * page / MIB).round())
        };
        memory.used_mb = pages(&[
            "Pages wired down",
            "Pages active",
            "Pages occupied by compressor",
        ]);
        memory.compressed_mb = pages(&["Pages occupied by compressor"]);
        memory.pageouts = count("Pageouts");
    }
    if let Some((total, used)) = swap.and_then(swap_usage) {
        memory.swap_total_mb = Some(total);
        memory.swap_used_mb = Some(used);
    }
    memory
}

/// The total and used swap (MB) of `sysctl -n vm.swapusage`:
/// `total = 3072.00M  used = 1530.81M  free = 1541.19M  (encrypted)`.
pub fn swap_usage(text: &str) -> Option<(f64, f64)> {
    let value = |label: &str| {
        let rest = text.split(&format!("{label} = ")).nth(1)?;
        let token = rest.split_whitespace().next()?;
        let (number, unit) = token.split_at(token.len().checked_sub(1)?);
        let number: f64 = number.parse().ok()?;
        let factor = match unit {
            "K" => 1.0 / 1024.0,
            "M" => 1.0,
            "G" => 1024.0,
            _ => return None,
        };
        Some(round2(number * factor))
    };
    Some((value("total")?, value("used")?))
}

/// Linux: the memory and swap of `/proc/meminfo` (used is total less
/// available) and the pages swapped out (`pswpout`) of `/proc/vmstat`.
/// Linux has no compressor: that column stays empty.
pub fn linux_memory(meminfo: Option<&str>, vmstat: Option<&str>) -> Memory {
    let mut memory = Memory::default();
    if let Some(text) = meminfo {
        let kib = |label: &str| {
            text.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                (name.trim() == label)
                    .then(|| value.split_whitespace().next()?.parse::<f64>().ok())
                    .flatten()
            })
        };
        let mb = |kib: f64| (kib / 1024.0).round();
        memory.total_mb = kib("MemTotal").map(mb);
        memory.used_mb = kib("MemTotal")
            .zip(kib("MemAvailable"))
            .map(|(total, available)| mb(total - available));
        memory.swap_total_mb = kib("SwapTotal").map(mb);
        memory.swap_used_mb = kib("SwapTotal")
            .zip(kib("SwapFree"))
            .map(|(total, free)| mb(total - free));
    }
    memory.pageouts = vmstat.and_then(|text| {
        text.lines().find_map(|line| {
            let (name, value) = line.split_once(' ')?;
            (name == "pswpout")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
    });
    memory
}

const MIB: f64 = 1024.0 * 1024.0;

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// The file of the local day `day` (days since the epoch):
/// `metrics-YYYYMMDD.csv`.
pub fn file_name(day: i64) -> String {
    let (year, month, date) = civil(day);
    format!("metrics-{year:04}{month:02}{date:02}.csv")
}

/// The local day (days since the epoch) a file of [`file_name`] is of.
pub fn file_day(name: &str) -> Option<i64> {
    let digits = name.strip_prefix("metrics-")?.strip_suffix(".csv")?;
    if digits.len() != 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year: i64 = digits[..4].parse().ok()?;
    let month: i64 = digits[4..6].parse().ok()?;
    let date: i64 = digits[6..].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&date) {
        return None;
    }
    Some(days_from_civil(year, month, date))
}

/// The host's local day (days since the epoch) of the unix second `unix`.
pub const fn local_day(unix: i64, utc_offset_secs: i64) -> i64 {
    (unix + utc_offset_secs).div_euclid(86_400)
}

/// Of `names` (a directory's file names), the files the retention removes
/// on the local day `today`: those of a day before the `retention_days`
/// latest (today included). A name that is no such file is kept; zero
/// days removes nothing.
pub fn expired(names: &[String], today: i64, retention_days: u32) -> Vec<&str> {
    if retention_days == 0 {
        return Vec::new();
    }
    let oldest = today - i64::from(retention_days) + 1;
    names
        .iter()
        .filter(|name| file_day(name).is_some_and(|day| day < oldest))
        .map(String::as_str)
        .collect()
}

/// Whether the file of `day` may hold a sample of `[from, until]` (unix
/// seconds) whatever the host's offset was: a day of margin on each side.
pub fn file_may_hold(day: i64, from: i64, until: i64) -> bool {
    day >= from.div_euclid(86_400) - 1 && day <= until.div_euclid(86_400) + 1
}

/// The mean, the median, the maximum and the p90 (the median and the p90
/// by nearest rank) of a column's values.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Summary {
    pub samples: usize,
    pub mean: f64,
    pub median: f64,
    pub max: f64,
    pub p90: f64,
}

impl Summary {
    pub fn of(values: &mut [f64]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        values.sort_by(f64::total_cmp);
        let count = values.len();
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let rank = |share: f64| ((count as f64) * share).ceil() as usize;
        let at = |share: f64| round2(values[rank(share).clamp(1, count) - 1]);
        #[allow(clippy::cast_precision_loss)]
        let mean = values.iter().sum::<f64>() / count as f64;
        Some(Self {
            samples: count,
            mean: round2(mean),
            median: at(0.5),
            max: round2(values[count - 1]),
            p90: at(0.9),
        })
    }
}

/// The summary of the host's load over a window (`stats`'s `host`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HostSummary {
    /// The window, unix seconds.
    pub from: i64,
    pub until: i64,
    /// The samples in it.
    pub samples: usize,
    /// The first and the last sample's unix second.
    pub first: Option<i64>,
    pub last: Option<i64>,
    /// Per column (without `time`, `unix` and the cumulative `pageouts`,
    /// with `pageouts_per_min` between two samples less than 10 minutes
    /// apart); null when no sample had it.
    pub metrics: BTreeMap<&'static str, Option<Summary>>,
    /// The CPU seconds the processes spent in the window ([`cpu_secs`]);
    /// null when no sample had `cpu_total`.
    pub cpu_secs: Option<CpuSecs>,
    /// Why the files could not be read, when they could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The summary of the samples in `[from, until]` (unix seconds).
pub fn summarize(samples: &[HostSample], from: i64, until: i64) -> HostSummary {
    let mut inside: Vec<&HostSample> = samples
        .iter()
        .filter(|sample| (from..=until).contains(&sample.unix))
        .collect();
    inside.sort_by_key(|sample| sample.unix);
    let mut metrics = BTreeMap::new();
    for column in COLUMNS[2..].iter().filter(|column| **column != PAGEOUTS) {
        let mut values: Vec<f64> = inside
            .iter()
            .filter_map(|sample| sample.values.get(column).copied())
            .collect();
        metrics.insert(*column, Summary::of(&mut values));
    }
    let mut rates: Vec<f64> = inside
        .windows(2)
        .filter_map(|pair| {
            let gap = pair[1].unix - pair[0].unix;
            let delta = pair[1].values.get(PAGEOUTS)? - pair[0].values.get(PAGEOUTS)?;
            #[allow(clippy::cast_precision_loss)]
            ((1..=MAX_RATE_GAP_SECS).contains(&gap) && delta >= 0.0)
                .then(|| delta * 60.0 / gap as f64)
        })
        .collect();
    metrics.insert(PAGEOUTS_PER_MIN, Summary::of(&mut rates));
    HostSummary {
        from,
        until,
        samples: inside.len(),
        first: inside.first().map(|sample| sample.unix),
        last: inside.last().map(|sample| sample.unix),
        metrics,
        cpu_secs: cpu_secs(&inside, from),
        error: None,
    }
}

/// The CPU seconds the processes spent over a window (goal 72): each
/// sample's `%cpu` (100 is one core) times the seconds it stands for.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CpuSecs {
    /// The seconds the samples with `cpu_total` stand for.
    pub covered_secs: i64,
    /// The seconds a sample stands for at most: twice the typical gap
    /// (the shortest) between two samples of the window.
    pub max_gap_secs: i64,
    /// The CPU seconds of all the processes.
    pub total: f64,
    /// Per kind of process ([`PROCESS_KINDS`]).
    pub by_kind: BTreeMap<&'static str, f64>,
}

/// The CPU seconds of `samples` (in the window from `from`, ascending):
/// a sample stands for the seconds since the one before it, and the
/// first for the typical gap (never before `from`). The typical gap is
/// the shortest gap between two samples of the window, the interval they
/// were taken at (the default interval without one), so that a window
/// mostly of gaps does not raise it; a gap is counted up to twice that, so a
/// stretch the supervisor did not record (it was stopped, or the host
/// slept) is not counted as if the load of its next sample had lasted
/// all along. macOS's `%cpu` is a decaying average: an approximation.
fn cpu_secs(samples: &[&HostSample], from: i64) -> Option<CpuSecs> {
    #[allow(clippy::cast_possible_wrap)]
    let typical = samples
        .windows(2)
        .map(|pair| pair[1].unix - pair[0].unix)
        .filter(|gap| *gap > 0)
        .min()
        .unwrap_or(DEFAULT_INTERVAL_SECS as i64);
    let max_gap = typical * 2;
    let mut total = CpuSecs {
        covered_secs: 0,
        max_gap_secs: max_gap,
        total: 0.0,
        by_kind: PROCESS_KINDS.iter().map(|kind| (*kind, 0.0)).collect(),
    };
    let mut any = false;
    let mut previous: Option<i64> = None;
    for sample in samples {
        let stands_for = previous.map_or_else(
            || typical.min(sample.unix - from + 1),
            |previous| sample.unix - previous,
        );
        previous = Some(sample.unix);
        let stands_for = stands_for.clamp(0, max_gap);
        let Some(cpu) = sample.values.get("cpu_total") else {
            continue;
        };
        any = true;
        #[allow(clippy::cast_precision_loss)]
        let secs = stands_for as f64 / 100.0;
        total.covered_secs += stands_for;
        total.total += cpu * secs;
        for (index, kind) in PROCESS_KINDS.iter().enumerate() {
            if let Some(cpu) = sample.values.get(column_of("cpu_", index, "")) {
                *total.by_kind.entry(kind).or_default() += cpu * secs;
            }
        }
    }
    any.then(|| {
        total.total = round2(total.total);
        for secs in total.by_kind.values_mut() {
            *secs = round2(*secs);
        }
        total
    })
}

/// `YYYY-MM-DDTHH:MM:SS+HH:MM` of the unix second `unix` at the offset.
pub fn local_time(unix: i64, utc_offset_secs: i64) -> String {
    let local = unix + utc_offset_secs;
    let (year, month, date) = civil(local.div_euclid(86_400));
    let rest = local.rem_euclid(86_400);
    let sign = if utc_offset_secs < 0 { '-' } else { '+' };
    let offset = utc_offset_secs.abs();
    format!(
        "{year:04}-{month:02}-{date:02}T{:02}:{:02}:{:02}{sign}{:02}:{:02}",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60,
        offset / 3_600,
        offset % 3_600 / 60
    )
}

/// The civil date of a day count (Howard Hinnant's days-to-civil).
const fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let date = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, date)
}

/// The day count of a civil date (Howard Hinnant's days-from-civil).
const fn days_from_civil(year: i64, month: i64, date: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + date - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    const VM_STAT: &str = "Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                               89859.
Pages active:                            186146.
Pages inactive:                          238288.
Pages wired down:                        155732.
\"Translation faults\":               55771604623.
Pages occupied by compressor:            229637.
Pageouts:                               2622788.
Swapouts:                               1047356.
";

    #[test]
    fn darwin_memory_reads_vm_stat_memsize_and_swapusage() {
        let memory = darwin_memory(
            Some(VM_STAT),
            Some("17179869184\n"),
            Some("total = 3072.00M  used = 1530.81M  free = 1541.19M  (encrypted)\n"),
        );
        assert_eq!(memory.total_mb, Some(16384.0));
        // (155732 + 186146 + 229637) pages of 16 KiB.
        assert_eq!(memory.used_mb, Some(8930.0));
        assert_eq!(memory.compressed_mb, Some(3588.0));
        assert_eq!(memory.pageouts, Some(2_622_788.0));
        assert_eq!(memory.swap_total_mb, Some(3072.0));
        assert_eq!(memory.swap_used_mb, Some(1530.81));
        // What the host did not give stays empty.
        assert_eq!(darwin_memory(None, Some("x"), Some("?")), Memory::default());
        assert_eq!(
            swap_usage("total = 1.50G  used = 512.00K"),
            Some((1536.0, 0.5))
        );
        assert_eq!(swap_usage("total = 1.50T  used = 0M"), None);
        // A vm_stat without the page size gives the pageouts only.
        let memory = darwin_memory(Some("Pageouts: 7.\n"), None, None);
        assert_eq!((memory.used_mb, memory.pageouts), (None, Some(7.0)));
    }

    #[test]
    fn linux_memory_reads_meminfo_and_vmstat() {
        let meminfo = "MemTotal:       16384000 kB\nMemFree: 1 kB\nMemAvailable:    8192000 kB\nSwapTotal:       2048000 kB\nSwapFree:        1024000 kB\n";
        let memory = linux_memory(Some(meminfo), Some("pgpgout 5\npswpout 42\n"));
        assert_eq!(memory.total_mb, Some(16000.0));
        assert_eq!(memory.used_mb, Some(8000.0));
        assert_eq!(memory.compressed_mb, None);
        assert_eq!(memory.swap_total_mb, Some(2000.0));
        assert_eq!(memory.swap_used_mb, Some(1000.0));
        assert_eq!(memory.pageouts, Some(42.0));
        assert_eq!(linux_memory(None, None), Memory::default());
    }

    #[test]
    fn processes_are_split_by_kind() {
        let ps = " 12.5  2048 /Users/me/.cargo/bin/cargo
 100.0 10240 /usr/local/bin/rustc
 50.0  1024 sccache
 20.0 409600 /Users/me/.local/share/claude/versions/2.1.283
 3.0  4096 /Users/me/.local/bin/dagq
 1.0  1024 (dagq)
 7.0  2048 /w/target/llvm-cov-target/debug/deps/it-4f327bb8
 2.0  1024 cargo-nextest
 5.5  2048 /usr/libexec/logd
garbage line
";
        let usage = processes(ps).unwrap();
        assert_eq!(usage[0].cpu, 21.5);
        assert_eq!(usage[0].rss_mb, 5.0);
        assert_eq!(usage[1].cpu, 150.0);
        assert_eq!(usage[1].rss_mb, 11.0);
        assert_eq!(usage[2].cpu, 20.0);
        assert_eq!(usage[2].rss_mb, 400.0);
        assert_eq!(usage[3].cpu, 4.0);
        assert_eq!(usage[4].cpu, 5.5);
        assert_eq!(processes("nothing here\n"), None);
        assert_eq!(PROCESS_KINDS[process_kind("claude")], "claude");
        assert_eq!(PROCESS_KINDS[process_kind("dagq.previous")], "dagq");
        assert_eq!(PROCESS_KINDS[process_kind("clippy-driver")], "rustc");
    }

    #[test]
    fn a_sample_is_written_and_read_back_by_its_header() {
        let usage = processes(" 10.0 2048 cargo\n 5.0 1024 zsh\n").unwrap();
        let memory = Memory {
            total_mb: Some(16384.0),
            used_mb: Some(9000.0),
            pageouts: Some(12.0),
            ..Memory::default()
        };
        let sample = HostSample::new(1_790_000_000)
            .with_loads([Some(1.25), None, Some(f64::NAN)])
            .with_processes(Some(&usage))
            .with_memory(&memory);
        let row = sample.row(9 * 3600);
        assert!(row.starts_with("2026-09-21T23:13:20+09:00,1790000000,1.25,,,15,10,"));
        assert_eq!(row.split(',').count(), COLUMNS.len());
        let text = format!("{}\n{row}\nno,unix\n", header());
        let read = parse_file(&text);
        assert_eq!(read, vec![sample]);
        assert_eq!(last_unix(&text), None);
        assert_eq!(
            last_unix(&format!("{}\n{row}\n\n", header())),
            Some(1_790_000_000)
        );
        assert!(parse_file("").is_empty());
        // A file of other columns is read by its header.
        let other = parse_file("unix,extra,load1\n5,x,2.5\n");
        assert_eq!(other[0].unix, 5);
        assert_eq!(other[0].values.get("load1"), Some(&2.5));
        assert_eq!(local_time(0, -5400), "1969-12-31T22:30:00-01:30");
        assert_eq!(HostSample::new(0).with_processes(None), HostSample::new(0));
    }

    #[test]
    fn files_are_named_by_the_local_day_and_expire_after_the_retention() {
        let day = local_day(1_790_000_000, 9 * 3600);
        assert_eq!(file_name(day), "metrics-20260921.csv");
        assert_eq!(file_day("metrics-20260921.csv"), Some(day));
        assert_eq!(file_day("metrics-20261321.csv"), None);
        assert_eq!(file_day("metrics-2026092.csv"), None);
        assert_eq!(file_day("notes.csv"), None);
        let names: Vec<String> = [
            "metrics-20260921.csv",
            "metrics-20260920.csv",
            "metrics-20260919.csv",
            "metrics-20000101.csv",
            "notes.txt",
        ]
        .map(str::to_owned)
        .to_vec();
        assert_eq!(
            expired(&names, day, 2),
            vec!["metrics-20260919.csv", "metrics-20000101.csv"]
        );
        assert!(expired(&names, day, 0).is_empty());
        assert!(file_may_hold(day, 1_790_000_000, 1_790_000_000));
        assert!(!file_may_hold(day - 3, 1_790_000_000, 1_790_000_100));
    }

    #[test]
    fn the_summary_reads_mean_max_p90_and_the_pageout_rate() {
        let samples: Vec<HostSample> = (0..10_i32)
            .map(|index| {
                let mut sample = HostSample::new(1000 + i64::from(index) * 30);
                sample.set("load1", Some(f64::from(index + 1)));
                // The host rebooted before the sixth sample.
                let pageouts = if index < 5 { index } else { index - 5 };
                sample.set(PAGEOUTS, Some(100.0 * f64::from(pageouts)));
                sample
            })
            .chain([HostSample::new(5000)])
            .collect();
        let summary = summarize(&samples, 1000, 1270);
        assert_eq!(summary.samples, 10);
        assert_eq!((summary.first, summary.last), (Some(1000), Some(1270)));
        let load = summary.metrics["load1"].unwrap();
        assert_eq!(
            (load.samples, load.mean, load.median, load.max, load.p90),
            (10, 5.5, 5.0, 10.0, 9.0)
        );
        // No sample had the CPU: no CPU seconds.
        assert_eq!(summary.cpu_secs, None);
        assert_eq!(summary.metrics["mem_used_mb"], None);
        assert!(!summary.metrics.contains_key(PAGEOUTS));
        // 100 per 30 s, the reboot (a counter going back) skipped.
        let rate = summary.metrics[PAGEOUTS_PER_MIN].unwrap();
        assert_eq!((rate.samples, rate.max), (8, 200.0));
        let empty = summarize(&[], 0, 10);
        assert_eq!(empty.samples, 0);
        assert_eq!(empty.metrics[PAGEOUTS_PER_MIN], None);
        let json = serde_json::to_value(&empty).unwrap();
        assert!(json.get("error").is_none());
    }

    /// Each sample stands for the seconds since the one before it, the
    /// first for the usual gap but never before the window; a gap is
    /// counted up to twice the usual one, so records that stopped for
    /// hours do not count the next record's load as spent all along.
    #[test]
    fn cpu_seconds_count_each_sample_for_its_gap_up_to_twice_the_usual() {
        let at = |unix: i64, cpu: Option<f64>| {
            let mut sample = HostSample::new(unix);
            sample.set("cpu_total", cpu);
            sample.set("cpu_rustc", cpu.map(|cpu| cpu / 2.0));
            sample
        };
        let samples = vec![
            at(1000, Some(100.0)),
            at(1030, Some(100.0)),
            at(1060, Some(100.0)),
            // Stopped for ten hours.
            at(37_060, Some(400.0)),
            // Without the CPU: stands for nothing, but ends a gap.
            at(37_090, None),
            at(37_120, Some(100.0)),
        ];
        let cpu = summarize(&samples, 990, 40_000).cpu_secs.unwrap();
        // 11 s (from 990), 30 s, 30 s, 60 s (not 36,000 s), 30 s.
        assert_eq!(cpu.max_gap_secs, 60);
        assert_eq!(cpu.covered_secs, 11 + 30 + 30 + 60 + 30);
        assert_eq!(cpu.total, 11.0 + 30.0 + 30.0 + 240.0 + 30.0);
        assert_eq!(cpu.by_kind["rustc"], 170.5);
        assert_eq!(cpu.by_kind["cargo"], 0.0);
        // One sample: the default interval.
        // A window mostly of gaps still counts each for twice the
        // interval the records were taken at.
        let sparse = [
            at(0, Some(100.0)),
            at(30, Some(100.0)),
            at(36_030, Some(100.0)),
            at(72_030, Some(100.0)),
        ];
        let cpu = summarize(&sparse, 0, 80_000).cpu_secs.unwrap();
        assert_eq!(cpu.covered_secs, 1 + 30 + 60 + 60);
        let one = summarize(&samples[..1], 0, 2000).cpu_secs.unwrap();
        assert_eq!((one.covered_secs, one.total), (30, 30.0));
        let json = serde_json::to_value(summarize(&[], 0, 1)).unwrap();
        assert!(json["cpu_secs"].is_null());
    }
}
