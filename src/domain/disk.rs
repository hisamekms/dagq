//! The free disk space a run needs (ADR-0047 decision 44, task 377): the
//! supervisor claims no new run, and starts no landing's verification,
//! while the free space of the queue's directory (where the run worktrees
//! are) is below what one needs. What one needs follows from what the
//! recent runs built: the largest `bytes` of the latest
//! `build_outputs_removed` (the `target/` an ended run left, task 376) and
//! of the latest `scratchpad_removed` (the Claude Code scratchpad a run's
//! session wrote to, task 1100) together ([`run_size`]), times a
//! factor per step, never below `min_free_bytes`. The `[disk]` table of
//! `dagq.toml` sets them.

use serde::Serialize;

use super::views::RunEvent;

/// The run event whose `bytes` measure what a run built.
pub const BUILD_OUTPUTS_REMOVED: &str = super::event_kind::BUILD_OUTPUTS_REMOVED;
/// The run event whose `bytes` measure what a run's session left in its
/// Claude Code scratchpad (task 1100).
pub const SCRATCHPAD_REMOVED: &str = super::event_kind::SCRATCHPAD_REMOVED;
/// The kinds of the events a run's size is read from.
pub const RUN_SIZE_EVENTS: [&str; 2] = [BUILD_OUTPUTS_REMOVED, SCRATCHPAD_REMOVED];
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
/// the largest scratchpad) a claim needs free.
pub const DEFAULT_CLAIM_FACTOR: f64 = 2.0;
/// Default factor of the recent run size a landing's verification needs
/// free.
pub const DEFAULT_INTEGRATE_FACTOR: f64 = 1.5;

/// The `[disk]` table of `dagq.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DiskConfig {
    /// How many of the latest `build_outputs_removed` and of the latest
    /// `scratchpad_removed` the runs' sizes are read from.
    pub sample_runs: i64,
    /// A claim needs the recent run size (the largest build outputs plus
    /// the largest scratchpad) times this free.
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
    /// plus the largest Claude Code scratchpad of the recent runs (the name
    /// is kept from before scratchpads were counted).
    pub largest_build: Option<u64>,
    pub claim: Option<u64>,
    pub landing: Option<u64>,
}

/// The size of a recent run the thresholds follow, from `events` (the
/// latest `build_outputs_removed` and `scratchpad_removed`), for
/// [`DiskConfig::needs`]: the largest build outputs and the largest
/// scratchpad together, as a run keeps both on the disk at once. They are
/// taken over the runs apart, not per run: a run whose task goes on
/// records its build outputs, and its scratchpad only once the task is
/// over (a landed run's build goes with its whole worktree), so the two
/// seldom fall on the same run. `None` without either; events of other
/// kinds or without `bytes` are left out.
pub fn run_size(events: &[RunEvent]) -> Option<u64> {
    let mut largest: [Option<u64>; 2] = [None, None];
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
    match largest {
        [None, None] => None,
        [built, scratch] => Some(built.unwrap_or(0) + scratch.unwrap_or(0)),
    }
}

/// `bytes` in GiB with one decimal, for messages.
pub fn gib(bytes: f64) -> String {
    format!("{:.1} GiB", bytes / f64::from(1u32 << 30))
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
}
