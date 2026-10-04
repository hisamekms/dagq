//! The free disk space a run needs (ADR-0047 decision 44, task 377): the
//! supervisor claims no new run, and starts no landing's verification,
//! while the free space of the queue's directory (where the run worktrees
//! are) is below what one needs. What one needs follows from what the
//! recent runs built: the largest `bytes` of the latest
//! `build_outputs_removed` (the `target/` an ended run left, task 376) and
//! of the latest `scratchpad_removed` (the Claude Code scratchpad a run's
//! session wrote to, task 1100) and `run_tmp_removed` (the `TMPDIR` the
//! runtime gave a Codex worker's turns, task 1290) together
//! ([`run_size`]), times a
//! factor per step, never below `min_free_bytes`. The `[disk]` table of
//! `dagq.toml` sets them.

use serde::Serialize;
use std::path::Path;

use super::views::RunEvent;

/// The run event whose `bytes` measure what a run built.
pub const BUILD_OUTPUTS_REMOVED: &str = super::event_kind::BUILD_OUTPUTS_REMOVED;
/// The run event whose `bytes` measure what a run's session left in its
/// Claude Code scratchpad (task 1100).
pub const SCRATCHPAD_REMOVED: &str = super::event_kind::SCRATCHPAD_REMOVED;
/// The run event whose `bytes` measure what a run's turns left in the
/// `TMPDIR` the runtime gave them (task 1290).
pub const RUN_TMP_REMOVED: &str = super::event_kind::RUN_TMP_REMOVED;
/// The kinds of the events a run's size is read from.
pub const RUN_SIZE_EVENTS: [&str; 3] = [BUILD_OUTPUTS_REMOVED, SCRATCHPAD_REMOVED, RUN_TMP_REMOVED];
/// The `subject` of the `cost` ask about the disk (ADR-0047 decision 42).
pub const DISK_SUBJECT: &str = "disk";
/// The options of the disk ask (ADR-0047 decision 44): `done` once a
/// person freed the disk, `wait` to leave the queue waiting.
pub const DISK_OPTIONS: &[&str] = &["done", "wait"];
/// The `repair` of the `auto_repaired` a cleanup for the disk records.
pub const DISK_CLEANUP: &str = "disk_cleanup";

/// Default number of the latest `build_outputs_removed` read.
pub const DEFAULT_SAMPLE_RUNS: i64 = 20;
/// Default factor of the recent run size (the largest build outputs plus
/// the largest scratchpad and run `TMPDIR`) a claim needs free.
pub const DEFAULT_CLAIM_FACTOR: f64 = 2.0;
/// Default factor of the recent run size a landing's verification needs
/// free.
pub const DEFAULT_INTEGRATE_FACTOR: f64 = 1.5;

/// The `[disk]` table of `dagq.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DiskConfig {
    /// How many of the latest `build_outputs_removed`, of the latest
    /// `scratchpad_removed` and of the latest `run_tmp_removed` the runs'
    /// sizes are read from.
    pub sample_runs: i64,
    /// A claim needs the recent run size (the largest build outputs plus
    /// the largest scratchpad and run `TMPDIR`) times this free.
    pub claim_factor: f64,
    /// A landing's verification needs the recent run size times this free.
    pub integrate_factor: f64,
    /// The least free bytes either needs, also before any build was
    /// measured; `None` checks nothing until one was.
    pub min_free_bytes: Option<u64>,
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            sample_runs: DEFAULT_SAMPLE_RUNS,
            claim_factor: DEFAULT_CLAIM_FACTOR,
            integrate_factor: DEFAULT_INTEGRATE_FACTOR,
            min_free_bytes: None,
        }
    }
}

impl DiskConfig {
    /// The setting names of the `[disk]` table.
    pub const KEYS: [&str; 4] = [
        "sample_runs",
        "claim_factor",
        "integrate_factor",
        "min_free_bytes",
    ];
    /// The settings that are factors (positive numbers, not whole ones).
    pub const FACTORS: [&str; 2] = ["claim_factor", "integrate_factor"];

    /// The whole-number setting `key` set to `value`; `None` for a key
    /// that is not one.
    pub fn set_whole(&mut self, key: &str, value: i64) -> Option<()> {
        match key {
            "sample_runs" => self.sample_runs = value,
            "min_free_bytes" => self.min_free_bytes = Some(u64::try_from(value).ok()?),
            _ => return None,
        }
        Some(())
    }

    /// The factor `key` set to `value`; `None` for a key that is not one.
    pub fn set_factor(&mut self, key: &str, value: f64) -> Option<()> {
        match key {
            "claim_factor" => self.claim_factor = value,
            "integrate_factor" => self.integrate_factor = value,
            _ => return None,
        }
        Some(())
    }

    /// What a claim and a landing need free, given the sizes of the recent
    /// runs (the largest counts; [`run_size`] gives one).
    pub fn needs(&self, builds: &[u64]) -> DiskNeeds {
        let largest = builds.iter().copied().max();
        let need = |factor: f64| {
            let scaled = largest.map(|bytes| (bytes as f64 * factor).ceil() as u64);
            match (scaled, self.min_free_bytes) {
                (Some(scaled), Some(least)) => Some(scaled.max(least)),
                (scaled, least) => scaled.or(least),
            }
        };
        DiskNeeds {
            largest_build: largest,
            claim: need(self.claim_factor),
            landing: need(self.integrate_factor),
        }
    }
}

/// The free bytes a claim and a landing need; `None` checks nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DiskNeeds {
    /// The recent run size they follow from: the largest build outputs
    /// plus the largest Claude Code scratchpad and run `TMPDIR` of the recent runs (the name
    /// is kept from before scratchpads were counted).
    pub largest_build: Option<u64>,
    pub claim: Option<u64>,
    pub landing: Option<u64>,
}

/// The size of a recent run the thresholds follow, from `events` (the
/// latest `build_outputs_removed`, `scratchpad_removed` and
/// `run_tmp_removed`), for [`DiskConfig::needs`]: the largest build
/// outputs, the largest scratchpad and the largest run `TMPDIR` together,
/// as a run keeps them on the disk at once. They are
/// taken over the runs apart, not per run: a run whose task goes on
/// records its build outputs, and its scratchpad only once the task is
/// over (a landed run's build goes with its whole worktree), so the two
/// seldom fall on the same run. `None` without either; events of other
/// kinds or without `bytes` are left out.
pub fn run_size(events: &[RunEvent]) -> Option<u64> {
    let mut largest: [Option<u64>; RUN_SIZE_EVENTS.len()] = [None; RUN_SIZE_EVENTS.len()];
    for event in events {
        let Some(kind) = RUN_SIZE_EVENTS.iter().position(|kind| *kind == event.kind) else {
            continue;
        };
        let Some(bytes) = event
            .payload
            .get("bytes")
            .and_then(serde_json::Value::as_u64)
        else {
            continue;
        };
        largest[kind] = Some(largest[kind].map_or(bytes, |seen| seen.max(bytes)));
    }
    largest
        .iter()
        .any(Option::is_some)
        .then(|| largest.iter().flatten().sum())
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
    use super::*;

    #[test]
    fn the_needs_follow_the_largest_recent_build() {
        let config = DiskConfig::default();
        assert_eq!(config.needs(&[]), DiskNeeds::default());
        let needs = config.needs(&[100, 400, 250]);
        assert_eq!(needs.largest_build, Some(400));
        assert_eq!(needs.claim, Some(800));
        assert_eq!(needs.landing, Some(600));
        // The least free bytes apply before any build and as a floor.
        let floor = DiskConfig {
            min_free_bytes: Some(700),
            ..config
        };
        assert_eq!(floor.needs(&[]).claim, Some(700));
        assert_eq!(floor.needs(&[]).landing, Some(700));
        assert_eq!(floor.needs(&[400]).claim, Some(800));
        assert_eq!(floor.needs(&[400]).landing, Some(700));
        let odd = DiskConfig {
            integrate_factor: 1.5,
            ..config
        };
        assert_eq!(odd.needs(&[3]).landing, Some(5));
    }

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
    fn a_run_size_adds_the_largest_scratchpad_to_the_largest_build() {
        const A: &str = "00000000-0000-4000-8000-00000000000a";
        const B: &str = "00000000-0000-4000-8000-00000000000b";
        let size = run_size(&[
            event(BUILD_OUTPUTS_REMOVED, Some(A), Some(300)),
            event(BUILD_OUTPUTS_REMOVED, Some(A), Some(500)),
            event(SCRATCHPAD_REMOVED, Some(B), Some(1_000)),
            event(SCRATCHPAD_REMOVED, None, Some(40)),
            // Not counted: no bytes, another kind.
            event(BUILD_OUTPUTS_REMOVED, Some(B), None),
            event(
                super::super::event_kind::WORKTREE_REMOVED,
                Some(B),
                Some(9_000),
            ),
        ]);
        assert_eq!(size, Some(1_500));
        assert_eq!(
            run_size(&[event(SCRATCHPAD_REMOVED, Some(A), Some(7))]),
            Some(7)
        );
        assert_eq!(
            run_size(&[event(BUILD_OUTPUTS_REMOVED, Some(A), Some(9))]),
            Some(9)
        );
        assert_eq!(run_size(&[]), None);
        // A run's `TMPDIR` (task 1290) adds its largest too.
        assert_eq!(
            run_size(&[
                event(BUILD_OUTPUTS_REMOVED, Some(A), Some(500)),
                event(SCRATCHPAD_REMOVED, Some(B), Some(1_000)),
                event(RUN_TMP_REMOVED, Some(A), Some(20)),
                event(RUN_TMP_REMOVED, Some(B), Some(70)),
            ]),
            Some(1_570)
        );
        assert_eq!(
            run_size(&[event(RUN_TMP_REMOVED, Some(A), Some(3))]),
            Some(3)
        );
    }

    #[test]
    fn the_settings_are_set_by_name() {
        let mut config = DiskConfig::default();
        assert_eq!(config.set_whole("sample_runs", 5), Some(()));
        assert_eq!(config.set_whole("min_free_bytes", 9), Some(()));
        assert_eq!(config.set_whole("claim_factor", 9), None);
        assert_eq!(config.set_factor("claim_factor", 3.0), Some(()));
        assert_eq!(config.set_factor("integrate_factor", 2.5), Some(()));
        assert_eq!(config.set_factor("sample_runs", 2.5), None);
        assert_eq!(
            config,
            DiskConfig {
                sample_runs: 5,
                claim_factor: 3.0,
                integrate_factor: 2.5,
                min_free_bytes: Some(9),
            }
        );
        assert_eq!(gib(1.5 * f64::from(1u32 << 30)), "1.5 GiB");
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
