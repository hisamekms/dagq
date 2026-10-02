//! The host's load read from its tools, and the files it is written to
//! (task 516, [`crate::domain::host_metrics`]): the load averages from
//! getloadavg(3), the CPU and memory per kind of process from `ps`, and
//! the memory, swap and pageouts from `vm_stat` and `sysctl` on macOS or
//! from `/proc` on Linux. Each tool that fails or is missing leaves its
//! columns empty; none fails the sample.

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

/// How long each of the host's tools may run.
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// The stdout of a tool that ran and succeeded; a byte that is not UTF-8
/// (a process's name) is replaced, not a failure.
fn tool(program: &str, arguments: &[&str]) -> Option<String> {
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

/// One sample of the host now, at the unix second `now`.
pub fn sample(now: i64) -> HostSample {
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
    let usage = ps().as_deref().and_then(processes);
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
/// missing, with the header when new), unless the file's last row is less
/// than half of `interval` old (another supervisor of the queue sampled),
/// then remove the files past `retention_days`.
pub fn record(
    dir: &Path,
    sample: &HostSample,
    utc_offset_secs: i64,
    interval: Duration,
    retention_days: u32,
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
    let half = i64::try_from(interval.as_secs() / 2).unwrap_or(i64::MAX);
    let (recent, needs_header) = if length == 0 {
        (false, true)
    } else {
        // The last row is in the file's tail.
        file.seek(SeekFrom::Start(length.saturating_sub(4096)))?;
        let mut tail = String::new();
        file.read_to_string(&mut tail)
            .with_context(|| format!("read {}", path.display()))?;
        let recent =
            host_metrics::last_unix(&tail).is_some_and(|last| (sample.unix - last).abs() < half);
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
            let path = dir.join(&name);
            let text =
                fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            samples.extend(
                host_metrics::parse_file(&text)
                    .into_iter()
                    .filter(|sample| (from..=until).contains(&sample.unix)),
            );
        }
    }
    Ok(samples)
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
    fn a_sample_of_this_host_has_the_load_and_the_processes() {
        let sample = sample(1_790_000_000);
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
        let first = record(&host, &at(1_790_000_000, 1.0), 0, interval, 2).unwrap();
        let path = host.join("metrics-20260921.csv");
        assert_eq!(first.written.as_deref(), Some(path.as_path()));
        // Another supervisor 10 s later writes nothing; 30 s later, a row.
        assert_eq!(
            record(&host, &at(1_790_000_010, 2.0), 0, interval, 2)
                .unwrap()
                .written,
            None
        );
        record(&host, &at(1_790_000_030, 3.0), 0, interval, 2).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3, "{text}");
        assert_eq!(text.lines().next(), Some(header().as_str()));

        // Past the retention, and a file that is no metrics file.
        fs::write(host.join("metrics-20260918.csv"), "old").unwrap();
        fs::write(host.join("notes.txt"), "mine").unwrap();
        let later = record(&host, &at(1_790_000_060, 4.0), 0, interval, 2).unwrap();
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
        assert!(record(&file, &at(1, 1.0), 0, interval, 2).is_err());
        let broken = host.join("metrics-20260921.csv");
        fs::remove_file(&broken).unwrap();
        fs::create_dir(&broken).unwrap();
        let failed = super::summary(&host, 1_790_000_000, 1_790_000_100);
        assert!(failed.error.is_some());
        assert_eq!(failed.samples, 0);
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
            record(&host, &disk(1_790_000_000 + 30 * index), 0, interval, 0).unwrap();
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
