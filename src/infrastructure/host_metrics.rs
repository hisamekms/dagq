//! The host's load read from its tools, and the files it is written to
//! (task 516, [`crate::domain::host_metrics`]): the load averages from
//! getloadavg(3), the CPU and memory per kind of process from `ps`, and
//! the memory, swap and pageouts from `vm_stat` and `sysctl` on macOS or
//! from `/proc` on Linux. Each tool that fails or is missing leaves its
//! columns empty; none fails the sample. The files are the copy a person
//! reads of the samples the NodeSampleStore keeps.

use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result};

use super::adapters::capture_bytes;
use crate::domain::host_metrics::{
    self, HostSample, HostSummary, darwin_memory, header, linux_memory, processes,
};
use crate::domain::toolchain::Toolchains;

/// The bytes on disk of the build outputs of the run worktree `worktree`
/// (task 1382): of each directory the profiles in use build into under it
/// ([`Toolchains::worktree_build_dirs`]), measured as the cleanup measures
/// what it removes (none there and a link in its place count 0, no link is
/// followed); `None` when one could not be measured.
pub fn build_outputs_bytes(worktree: &Path, toolchains: &Toolchains) -> Option<u64> {
    use crate::application::RunFiles;
    toolchains
        .worktree_build_dirs()
        .into_iter()
        .map(|dir| {
            super::run_files::LocalRunFiles
                .tree_size(&worktree.join(dir))
                .map(Option::unwrap_or_default)
        })
        .sum::<std::io::Result<u64>>()
        .ok()
}

/// How long each of the host's tools may run.
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// The stdout of a tool that ran and succeeded; a byte that is not UTF-8
/// (a process's name) is replaced, not a failure.
pub(super) fn tool(program: &str, arguments: &[&str]) -> Option<String> {
    match capture_bytes(Command::new(program).args(arguments), TOOL_TIMEOUT) {
        Ok((status, stdout, _)) if status.success() => {
            Some(String::from_utf8_lossy(&stdout).into_owned())
        }
        _ => None,
    }
}

/// `ps`'s CPU, memory and command of every process. Linux's `comm` is
/// the name cut to 15 bytes, never the path the kinds are told by, so
/// there the first word of `args` (the path the process was started by)
/// stands for it; macOS's `comm` is that path already, spaces included.
fn ps() -> Option<String> {
    if cfg!(target_os = "linux") {
        let text = tool("ps", &["-A", "-o", "pcpu=,rss=,args="])?;
        Some(
            text.lines()
                .map(|line| {
                    line.split_whitespace()
                        .take(3)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    } else {
        tool("ps", &["-A", "-o", "pcpu=,rss=,comm="])
    }
}

/// The load averages of 1, 5 and 15 minutes.
fn loads() -> [Option<f64>; 3] {
    let mut loads = [0f64; 3];
    // SAFETY: getloadavg writes at most `nelem` doubles into the buffer.
    let written = unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) };
    let mut out = [None; 3];
    for (index, load) in loads.iter().enumerate() {
        if i32::try_from(index).is_ok_and(|index| index < written) && load.is_finite() {
            out[index] = Some(*load);
        }
    }
    out
}

/// One sample of the host now, at the unix second `now`, its processes
/// split by the kinds of `toolchains`' profiles. The memory is read the
/// Linux way when `/proc/meminfo` exists, else the macOS way.
pub fn sample(now: i64, toolchains: &Toolchains) -> HostSample {
    let memory = if Path::new("/proc/meminfo").exists() {
        linux_memory(
            fs::read_to_string("/proc/meminfo").ok().as_deref(),
            fs::read_to_string("/proc/vmstat").ok().as_deref(),
        )
    } else {
        darwin_memory(
            tool("vm_stat", &[]).as_deref(),
            tool("sysctl", &["-n", "hw.memsize"]).as_deref(),
            tool("sysctl", &["-n", "vm.swapusage"]).as_deref(),
        )
    };
    let usage = ps().as_deref().and_then(|ps| processes(ps, toolchains));
    HostSample::new(now)
        .with_loads(loads())
        .with_processes(usage.as_ref())
        .with_memory(&memory)
}

/// What [`record`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// The file the sample went to; `None` when another process of the
    /// queue wrote one less than half an interval before.
    pub written: Option<PathBuf>,
    /// The files the retention removed.
    pub removed: Vec<PathBuf>,
}

/// Append `sample` to its local day's file under `dir` (made when
/// missing, with the header when new) as `admitted` decides: `Some` is the
/// NodeSampleStore's decision, which the files follow; `None` (the store
/// could not decide) leaves out a sample less than
/// [`host_metrics::apart_secs`] from the file's last row (another
/// supervisor of the queue sampled). Then remove the files past
/// `retention_days`.
pub fn record(
    dir: &Path,
    sample: &HostSample,
    utc_offset_secs: i64,
    interval: Duration,
    retention_days: u32,
    admitted: Option<bool>,
) -> Result<Recorded> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let today = host_metrics::local_day(sample.unix, utc_offset_secs);
    let path = dir.join(host_metrics::file_name(today));
    let mut file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let length = file.metadata()?.len();
    let half = host_metrics::apart_secs(interval);
    let (recent, needs_header) = if length == 0 {
        (admitted == Some(false), true)
    } else {
        // The last row is in the file's tail.
        file.seek(SeekFrom::Start(length.saturating_sub(4096)))?;
        let mut tail = String::new();
        file.read_to_string(&mut tail)
            .with_context(|| format!("read {}", path.display()))?;
        let recent = admitted.map_or_else(
            || host_metrics::last_unix(&tail).is_some_and(|last| (sample.unix - last).abs() < half),
            |admitted| !admitted,
        );
        // The header in force: the first line, unless another binary began
        // the file (task 1371). Only then is the file read whole, for the
        // last header in it, however far back it is.
        file.seek(SeekFrom::Start(0))?;
        let mut first = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(&file), &mut first)
            .with_context(|| format!("read {}", path.display()))?;
        let needs_header = first.trim() != header() && {
            file.seek(SeekFrom::Start(0))?;
            let mut whole = String::new();
            file.read_to_string(&mut whole)
                .with_context(|| format!("read {}", path.display()))?;
            host_metrics::needs_header(&whole)
        };
        (recent, needs_header)
    };
    let written = if recent {
        None
    } else {
        let mut text = String::new();
        // A new file, or one an older binary began with other columns
        // (task 1371): the rows after this header are read by it.
        if needs_header {
            text.push_str(&header());
            text.push('\n');
        }
        text.push_str(&sample.row(utc_offset_secs));
        text.push('\n');
        file.write_all(text.as_bytes())
            .with_context(|| format!("append to {}", path.display()))?;
        Some(path)
    };
    let names = file_names(dir)?;
    let mut removed = Vec::new();
    for name in host_metrics::expired(&names, today, retention_days) {
        let path = dir.join(name);
        fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        removed.push(path);
    }
    Ok(Recorded { written, removed })
}

fn file_names(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        if let Some(name) = entry?.file_name().to_str() {
            names.push(name.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// The samples of the files under `dir` taken in `[from, until]` (unix
/// seconds); none when there is no directory.
pub fn read(dir: &Path, from: i64, until: i64) -> Result<Vec<HostSample>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut samples = Vec::new();
    for name in file_names(dir)? {
        if host_metrics::file_day(&name)
            .is_some_and(|day| host_metrics::file_may_hold(day, from, until))
        {
            samples.extend(
                read_file(dir, &name)?
                    .into_iter()
                    .filter(|sample| (from..=until).contains(&sample.unix)),
            );
        }
    }
    Ok(samples)
}

/// The samples of one file under `dir`.
pub fn read_file(dir: &Path, name: &str) -> Result<Vec<HostSample>> {
    read_text(dir, name).map(|text| host_metrics::parse_file(&text))
}

fn read_text(dir: &Path, name: &str) -> Result<String> {
    let path = dir.join(name);
    fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))
}

/// The files of the host's load under a directory (`<queue dir>/host`).
#[derive(Debug, Clone)]
pub struct HostMetricsDir(pub PathBuf);

impl crate::application::HostMetricsFiles for HostMetricsDir {
    fn record(
        &self,
        sample: &HostSample,
        utc_offset_secs: i64,
        interval: Duration,
        retention_days: u32,
        admitted: Option<bool>,
    ) -> Result<Vec<PathBuf>> {
        record(
            &self.0,
            sample,
            utc_offset_secs,
            interval,
            retention_days,
            admitted,
        )
        .map(|recorded| recorded.removed)
    }

    fn files(&self) -> Result<Vec<(String, u64)>> {
        if !self.0.is_dir() {
            return Ok(Vec::new());
        }
        file_names(&self.0)?
            .into_iter()
            .map(|name| {
                let path = self.0.join(&name);
                let bytes = fs::metadata(&path)
                    .with_context(|| format!("read {}", path.display()))?
                    .len();
                Ok((name, bytes))
            })
            .collect()
    }

    fn read_file(&self, name: &str) -> Result<(Vec<HostSample>, u64)> {
        let text = read_text(&self.0, name)?;
        Ok((host_metrics::parse_file(&text), text.len() as u64))
    }
}

/// The summary of the host's load over `[from, until]` from the files
/// under `dir`; a directory that cannot be read gives an empty summary
/// with the reason.
pub fn summary(dir: &Path, from: i64, until: i64) -> HostSummary {
    match read(dir, from, until) {
        Ok(samples) => host_metrics::summarize(&samples, from, until),
        Err(error) => HostSummary {
            error: Some(format!("{error:#}")),
            ..host_metrics::summarize(&[], from, until)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_outputs_of_a_worktree_are_its_profiles_directories_on_disk() {
        let worktree = tempfile::tempdir().unwrap();
        let rust = Toolchains::this_repository();
        // None built yet is 0 bytes, as is a worktree of no profile.
        assert_eq!(build_outputs_bytes(worktree.path(), &rust), Some(0));
        let target = worktree.path().join("target/debug");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("dagq"), vec![7u8; 64 * 1024]).unwrap();
        fs::write(worktree.path().join("src.rs"), vec![7u8; 64 * 1024]).unwrap();
        let bytes = build_outputs_bytes(worktree.path(), &rust).unwrap();
        // The blocks the file takes, and its directories', not the
        // worktree's other files.
        assert!((64 * 1024..128 * 1024).contains(&bytes), "{bytes}");
        assert_eq!(
            build_outputs_bytes(worktree.path(), &Toolchains::none()),
            Some(0)
        );
    }

    #[test]
    fn a_sample_of_this_host_has_the_load_and_the_processes() {
        let sample = sample(1_790_000_000, &Toolchains::this_repository());
        assert_eq!(sample.unix, 1_790_000_000);
        assert!(sample.values.contains_key("load1"), "{sample:?}");
        assert!(sample.values.contains_key("cpu_total"), "{sample:?}");
        assert!(sample.values.contains_key("mem_total_mb"), "{sample:?}");
        assert_eq!(tool("/nonexistent/tool", &[]), None);
    }

    #[test]
    fn records_append_once_per_interval_prune_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("host");
        let at = |unix: i64, load: f64| {
            let mut sample = HostSample::new(unix);
            sample.set("load1", Some(load));
            sample
        };
        let interval = Duration::from_secs(30);
        let first = record(&host, &at(1_790_000_000, 1.0), 0, interval, 2, None).unwrap();
        let path = host.join("metrics-20260921.csv");
        assert_eq!(first.written.as_deref(), Some(path.as_path()));
        // Another supervisor 10 s later writes nothing; 30 s later, a row.
        assert_eq!(
            record(&host, &at(1_790_000_010, 2.0), 0, interval, 2, None)
                .unwrap()
                .written,
            None
        );
        record(&host, &at(1_790_000_030, 3.0), 0, interval, 2, None).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3, "{text}");
        assert_eq!(text.lines().next(), Some(header().as_str()));

        // Past the retention, and a file that is no metrics file.
        fs::write(host.join("metrics-20260918.csv"), "old").unwrap();
        fs::write(host.join("notes.txt"), "mine").unwrap();
        let later = record(&host, &at(1_790_000_060, 4.0), 0, interval, 2, None).unwrap();
        assert_eq!(later.removed, vec![host.join("metrics-20260918.csv")]);
        assert!(host.join("notes.txt").exists());

        let samples = read(&host, 1_790_000_000, 1_790_000_040).unwrap();
        assert_eq!(samples.len(), 2);
        let summary = summary(&host, 1_790_000_000, 1_790_000_100);
        assert_eq!(summary.samples, 3);
        assert_eq!(summary.metrics["load1"].unwrap().max, 4.0);
        assert!(read(&dir.path().join("none"), 0, 1).unwrap().is_empty());
        // A directory that cannot be read: the summary says why.
        let file = dir.path().join("file");
        fs::write(&file, "x").unwrap();
        assert!(record(&file, &at(1, 1.0), 0, interval, 2, None).is_err());
        let broken = host.join("metrics-20260921.csv");
        fs::remove_file(&broken).unwrap();
        fs::create_dir(&broken).unwrap();
        let failed = super::summary(&host, 1_790_000_000, 1_790_000_100);
        assert!(failed.error.is_some());
        assert_eq!(failed.samples, 0);
    }

    /// The store's decision overrides the file's last row: admitted, a
    /// sample 10 s after the last row is appended; kept out, a sample far
    /// from it is not, nor is a header written to a new day's file.
    #[test]
    fn the_stores_decision_overrides_the_last_row() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("host");
        let interval = Duration::from_secs(30);
        let at = |unix: i64| {
            let mut sample = HostSample::new(unix);
            sample.set("load1", Some(1.5));
            sample
        };
        record(&host, &at(1_790_000_000), 0, interval, 0, None).unwrap();
        let near = record(&host, &at(1_790_000_010), 0, interval, 0, Some(true)).unwrap();
        assert!(near.written.is_some());
        let far = record(&host, &at(1_790_000_100), 0, interval, 0, Some(false)).unwrap();
        assert_eq!(far.written, None);
        let next_day = record(&host, &at(1_790_086_400), 0, interval, 0, Some(false)).unwrap();
        assert_eq!(next_day.written, None);
        assert_eq!(
            read(&host, 0, i64::MAX).unwrap(),
            [at(1_790_000_000), at(1_790_000_010)]
        );
    }

    /// A day's file an older binary began with fewer columns gets this
    /// binary's header before its first row, once (task 1371), and the
    /// disk columns read back.
    #[test]
    fn a_file_of_older_columns_gets_this_header_once() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("host");
        fs::create_dir_all(&host).unwrap();
        let path = host.join("metrics-20260921.csv");
        fs::write(&path, "time,unix,load1\nx,1790000000,1.5\n").unwrap();
        let disk = |unix: i64| {
            HostSample::new(unix).with_disk(Some(host_metrics::DiskSpace {
                free_bytes: 3 * 1024 * 1024,
                total_bytes: 12 * 1024 * 1024,
            }))
        };
        let interval = Duration::from_secs(30);
        // More rows than the tail read for the last row: the header stays
        // the one written first.
        for index in 1..=100 {
            record(
                &host,
                &disk(1_790_000_000 + 30 * index),
                0,
                interval,
                0,
                None,
            )
            .unwrap();
        }
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.len() > 4096, "{}", text.len());
        assert_eq!(
            text.lines().filter(|line| *line == header()).count(),
            1,
            "{text}"
        );
        let summary = summary(&host, 1_790_000_000, 1_790_003_100);
        assert_eq!(summary.samples, 101);
        assert_eq!(summary.metrics["load1"].unwrap().max, 1.5);
        let disk = host_metrics::DiskFree::of(&summary).unwrap();
        assert_eq!((disk.samples, disk.min_free_bytes), (100, 3_145_728.0));
        assert_eq!(disk.min_free_pct, Some(25.0));
    }

    #[test]
    fn the_disk_space_of_a_directory_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let disk = crate::infrastructure::adapters::disk_space(dir.path()).unwrap();
        assert!(
            disk.total_bytes > 0 && disk.free_bytes <= disk.total_bytes,
            "{disk:?}"
        );
        assert_eq!(
            crate::infrastructure::adapters::disk_space(&dir.path().join("none")),
            None
        );
    }
}
