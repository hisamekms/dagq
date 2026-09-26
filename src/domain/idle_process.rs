//! Processes of a run that stay alive without making progress (task 469):
//! the `idle_process` alert of the recovery job (ADR-0047 decision 39).
//! The supervisor samples the CPU time (`ps`'s `time`) of the run's
//! processes ([`super::recovery::run_processes`]) and feeds each sample to
//! a [`CpuWatch`]; a process whose CPU time, together with that of its
//! descendants, has grown by almost nothing for `[stall].idle_process_secs`
//! is idle. Only the elapsed time of background work (`long_background`)
//! cannot tell work that is slow from work that is stuck.
use std::collections::{HashMap, HashSet};

use serde::Serialize;

use super::recovery::ProcessInfo;

/// The CPU time a process has to use per second of wall time to count as
/// making progress, in thousandths: 1%. A test binary stuck in a poll of
/// `sleep 0.05` used 0.3% (task 328's run).
pub const PROGRESS_CPU_PER_MILLE: u64 = 10;

/// How soon after the agent a child of it counts as the session's own
/// helper (an MCP server Claude Code starts with the session), which
/// sleeps for the whole session and is never an idle process.
pub const SESSION_HELPER_SECS: u64 = 60;

/// `processes` without the session's helpers: the agent's children that
/// started within [`SESSION_HELPER_SECS`] of `agent`.
pub fn without_session_helpers(
    processes: Vec<ProcessInfo>,
    agent: Option<&ProcessInfo>,
) -> Vec<ProcessInfo> {
    let Some(agent) = agent else {
        return processes;
    };
    processes
        .into_iter()
        .filter(|p| {
            p.ppid != agent.pid || p.elapsed_secs + SESSION_HELPER_SECS < agent.elapsed_secs
        })
        .collect()
}

/// What the watch knows of one process: its command (a pid reused by
/// another command starts over) and when it last made progress.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Track {
    command: String,
    /// The last sample at which it made progress, or the first sample of it.
    active_ms: i64,
    /// Its CPU time then.
    active_cpu_ms: u64,
    /// Its CPU time at the latest sample.
    cpu_ms: u64,
}

/// A process that has made no progress for the threshold, with its
/// descendants: the top of an idle subtree of the run's processes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdleProcess {
    pub pid: u32,
    pub ppid: u32,
    pub command: String,
    pub elapsed_secs: u64,
    /// How long it and its descendants have made no progress.
    pub idle_secs: i64,
    /// Its own CPU time now, and how much it and its descendants used in
    /// the idle time.
    pub cpu_ms: u64,
    pub cpu_growth_ms: u64,
    /// The pids under it (all idle with it).
    pub descendants: Vec<u32>,
    /// When the subtree last made progress (unix milliseconds).
    pub active_ms: i64,
}

/// The CPU time of a run's processes across samples.
#[derive(Debug, Clone, Default)]
pub struct CpuWatch {
    tracks: HashMap<u32, Track>,
    /// Processes already handed to a recovery job, with the progress they
    /// had then: they are not idle again until they make progress.
    handed: HashMap<u32, i64>,
}

impl CpuWatch {
    /// Take the sample `processes` (the run's processes) read at `now_ms`.
    /// A process not in it is forgotten, a new one (or a pid now running
    /// another command) starts its idle time here, and one whose CPU time
    /// grew by more than [`PROGRESS_CPU_PER_MILLE`] of the time since it
    /// last made progress makes progress now. A process whose CPU time
    /// could not be read is left out.
    pub fn observe(&mut self, processes: &[ProcessInfo], now_ms: i64) {
        let mut tracks = HashMap::new();
        for process in processes {
            let Some(cpu_ms) = process.cpu_ms else {
                continue;
            };
            let fresh = Track {
                command: process.command.clone(),
                active_ms: now_ms,
                active_cpu_ms: cpu_ms,
                cpu_ms,
            };
            let track = match self.tracks.remove(&process.pid) {
                Some(track) if track.command == process.command && cpu_ms >= track.cpu_ms => {
                    let wall = u64::try_from(now_ms - track.active_ms).unwrap_or(0);
                    let used = cpu_ms - track.active_cpu_ms;
                    if used.saturating_mul(1000) > wall.saturating_mul(PROGRESS_CPU_PER_MILLE) {
                        fresh
                    } else {
                        Track { cpu_ms, ..track }
                    }
                }
                _ => fresh,
            };
            tracks.insert(process.pid, track);
        }
        self.handed.retain(|pid, _| tracks.contains_key(pid));
        self.tracks = tracks;
    }

    /// The processes of `processes` (the sample last observed) that, with
    /// all their descendants, have made no progress for `threshold_secs`
    /// at `now_ms`: only the top of each idle subtree, and not one handed
    /// to a job since its last progress ([`Self::hand`]). A process whose
    /// CPU time could not be read counts as making progress, and so does
    /// the subtree it is in.
    pub fn idle(
        &self,
        processes: &[ProcessInfo],
        now_ms: i64,
        threshold_secs: i64,
    ) -> Vec<IdleProcess> {
        let pids: HashSet<u32> = processes.iter().map(|p| p.pid).collect();
        let children = |pid: u32| {
            processes
                .iter()
                .filter(move |p| p.ppid == pid && p.pid != pid)
                .map(|p| p.pid)
        };
        let subtree = |pid: u32| {
            let mut all = vec![pid];
            let mut next = 0;
            while next < all.len() {
                let more: Vec<u32> = children(all[next]).filter(|c| !all.contains(c)).collect();
                all.extend(more);
                next += 1;
            }
            all
        };
        // When the subtree of `pid` last made progress, and the CPU time it
        // used since.
        let progress = |pid: u32| {
            let mut active = i64::MIN;
            let mut used = 0;
            for member in subtree(pid) {
                match self.tracks.get(&member) {
                    Some(track) => {
                        active = active.max(track.active_ms);
                        used += track.cpu_ms - track.active_cpu_ms;
                    }
                    None => active = now_ms,
                }
            }
            (active, used)
        };
        let is_idle = |pid: u32| {
            let (active, _) = progress(pid);
            now_ms.saturating_sub(active) >= threshold_secs.saturating_mul(1000)
        };
        processes
            .iter()
            .filter(|p| is_idle(p.pid))
            .filter(|p| !(pids.contains(&p.ppid) && p.ppid != p.pid && is_idle(p.ppid)))
            .filter_map(|p| {
                let (active, used) = progress(p.pid);
                // Handed, and nothing in it made progress since (a child that
                // ended can only move its last progress back).
                if self
                    .handed
                    .get(&p.pid)
                    .is_some_and(|&handed| active <= handed)
                {
                    return None;
                }
                Some(IdleProcess {
                    pid: p.pid,
                    ppid: p.ppid,
                    command: p.command.clone(),
                    elapsed_secs: p.elapsed_secs,
                    idle_secs: (now_ms - active) / 1000,
                    cpu_ms: p.cpu_ms.unwrap_or(0),
                    cpu_growth_ms: used,
                    descendants: subtree(p.pid).into_iter().skip(1).collect(),
                    active_ms: active,
                })
            })
            .collect()
    }

    /// The watch with no process handed to a job: those it handed are
    /// idle again at once (the job's `wait` ended).
    #[must_use]
    pub fn released(self) -> Self {
        Self {
            handed: HashMap::new(),
            ..self
        }
    }

    /// `idle` was handed to a recovery job: those processes are not idle
    /// again until they (or a descendant) make progress.
    pub fn hand(&mut self, idle: &[IdleProcess]) {
        for process in idle {
            self.handed.insert(process.pid, process.active_ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, ppid: u32, cpu_ms: Option<u64>) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid,
            elapsed_secs: 5,
            command: format!("cmd {pid}"),
            cwd: None,
            cpu_ms,
        }
    }

    const MIN: i64 = 60_000;

    #[test]
    fn a_process_without_progress_for_the_threshold_is_idle() {
        let mut watch = CpuWatch::default();
        watch.observe(&[process(10, 1, Some(500))], 0);
        // 0.3% of the wall time is no progress.
        let sample = [process(10, 1, Some(500 + 540))];
        watch.observe(&sample, 3 * MIN);
        assert!(watch.idle(&sample, 3 * MIN, 30 * 60).is_empty());
        let sample = [process(10, 1, Some(500 + 5400))];
        watch.observe(&sample, 30 * MIN);
        let idle = watch.idle(&sample, 30 * MIN, 30 * 60);
        assert_eq!(idle.len(), 1, "{idle:?}");
        assert_eq!(idle[0].pid, 10);
        assert_eq!(idle[0].idle_secs, 30 * 60);
        assert_eq!(idle[0].cpu_ms, 5900);
        assert_eq!(idle[0].cpu_growth_ms, 5400);
        assert!(idle[0].descendants.is_empty());
    }

    #[test]
    fn a_long_process_that_uses_cpu_is_not_idle() {
        let mut watch = CpuWatch::default();
        for minute in 0..=40 {
            let sample = [process(10, 1, Some(minute as u64 * 30_000))];
            watch.observe(&sample, minute * MIN);
            assert!(watch.idle(&sample, minute * MIN, 30 * 60).is_empty());
        }
        // Progress restarts the idle time: a burst, then quiet.
        let sample = [process(10, 1, Some(40 * 30_000))];
        watch.observe(&sample, 69 * MIN);
        assert!(watch.idle(&sample, 69 * MIN, 30 * 60).is_empty());
        watch.observe(&sample, 70 * MIN);
        assert_eq!(watch.idle(&sample, 70 * MIN, 30 * 60).len(), 1);
    }

    #[test]
    fn a_parent_that_waits_for_a_busy_child_is_not_idle() {
        let mut watch = CpuWatch::default();
        let at = |child_cpu: u64| {
            [
                process(10, 1, Some(10)),
                process(11, 10, Some(20)),
                process(12, 11, Some(child_cpu)),
            ]
        };
        watch.observe(&at(0), 0);
        let sample = at(31 * 30_000);
        watch.observe(&sample, 31 * MIN);
        assert!(watch.idle(&sample, 31 * MIN, 30 * 60).is_empty());
        // Once the child stops too, only the top of the subtree is idle.
        let sample = at(31 * 30_000);
        watch.observe(&sample, 62 * MIN);
        let idle = watch.idle(&sample, 62 * MIN, 30 * 60);
        assert_eq!(idle.len(), 1, "{idle:?}");
        assert_eq!(idle[0].pid, 10);
        assert_eq!(idle[0].descendants, [11, 12]);
        assert_eq!(idle[0].idle_secs, 31 * 60);
    }

    #[test]
    fn a_new_or_reused_pid_and_an_unreadable_cpu_time_start_over() {
        let mut watch = CpuWatch::default();
        watch.observe(&[process(10, 1, Some(100))], 0);
        let mut reused = process(10, 1, Some(100));
        reused.command = "other".to_owned();
        watch.observe(&[reused.clone()], 31 * MIN);
        assert!(watch.idle(&[reused.clone()], 31 * MIN, 30 * 60).is_empty());
        // CPU time that went down is another process under the same pid.
        let mut restarted = reused.clone();
        restarted.cpu_ms = Some(5);
        watch.observe(&[restarted.clone()], 62 * MIN);
        assert!(watch.idle(&[restarted], 62 * MIN, 30 * 60).is_empty());
        // A child whose CPU time is unknown keeps its parent busy.
        let sample = [process(20, 1, Some(0)), process(21, 20, None)];
        watch.observe(&sample, 0);
        watch.observe(&sample, 31 * MIN);
        assert!(watch.idle(&sample, 31 * MIN, 30 * 60).is_empty());
    }

    #[test]
    fn a_handed_process_is_idle_again_only_after_progress() {
        let mut watch = CpuWatch::default();
        let quiet = [process(10, 1, Some(0))];
        watch.observe(&quiet, 0);
        watch.observe(&quiet, 31 * MIN);
        let idle = watch.idle(&quiet, 31 * MIN, 30 * 60);
        watch.hand(&idle);
        watch.observe(&quiet, 90 * MIN);
        assert!(watch.idle(&quiet, 90 * MIN, 30 * 60).is_empty());
        assert_eq!(
            watch
                .clone()
                .released()
                .idle(&quiet, 90 * MIN, 30 * 60)
                .len(),
            1
        );
        let busy = [process(10, 1, Some(60_000))];
        watch.observe(&busy, 91 * MIN);
        watch.observe(&busy, 122 * MIN);
        assert_eq!(watch.idle(&busy, 122 * MIN, 30 * 60).len(), 1);
        // A process that went away is forgotten, handed or not.
        watch.hand(&watch.idle(&busy, 122 * MIN, 30 * 60));
        watch.observe(&[], 123 * MIN);
        assert!(watch.handed.is_empty() && watch.tracks.is_empty());
    }

    #[test]
    fn a_handed_process_whose_quiet_child_ended_is_not_idle_again() {
        let mut watch = CpuWatch::default();
        watch.observe(&[process(10, 1, Some(0))], 0);
        let both = [process(10, 1, Some(0)), process(11, 10, Some(0))];
        watch.observe(&both, 5 * MIN);
        watch.observe(&both, 35 * MIN);
        let idle = watch.idle(&both, 35 * MIN, 30 * 60);
        assert_eq!(idle[0].active_ms, 5 * MIN);
        watch.hand(&idle);
        let parent = [process(10, 1, Some(0))];
        watch.observe(&parent, 36 * MIN);
        assert!(watch.idle(&parent, 36 * MIN, 30 * 60).is_empty());
    }

    #[test]
    fn the_agents_early_children_are_session_helpers() {
        let agent = ProcessInfo {
            elapsed_secs: 3600,
            ..process(5, 4, Some(0))
        };
        let helper = ProcessInfo {
            elapsed_secs: 3590,
            ..process(6, 5, Some(0))
        };
        let work = ProcessInfo {
            elapsed_secs: 1800,
            ..process(7, 5, Some(0))
        };
        let orphan = ProcessInfo {
            elapsed_secs: 3590,
            ..process(8, 1, Some(0))
        };
        let all = vec![helper, work.clone(), orphan.clone()];
        assert_eq!(
            without_session_helpers(all.clone(), Some(&agent)),
            [work, orphan]
        );
        assert_eq!(without_session_helpers(all.clone(), None), all);
    }
}
