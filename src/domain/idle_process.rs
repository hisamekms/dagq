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
    /// When the CPU time used since is measured from: `active_ms`, except
    /// for a quiet new child that took its parent's `active_ms` and is
    /// measured from when it was first seen.
    measured_ms: i64,
    /// Its CPU time then.
    active_cpu_ms: u64,
    /// Its CPU time at the latest sample.
    cpu_ms: u64,
}

impl Track {
    /// A process that makes progress at `now_ms`.
    fn fresh(process: &ProcessInfo, cpu_ms: u64, now_ms: i64) -> Self {
        Self {
            command: process.command.clone(),
            active_ms: now_ms,
            measured_ms: now_ms,
            active_cpu_ms: cpu_ms,
            cpu_ms,
        }
    }
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
    /// When each process of the last sample, with its descendants then,
    /// last made progress: what a quiet new child of it takes.
    subtree_ms: HashMap<u32, i64>,
}

impl CpuWatch {
    /// Take the sample `processes` (the run's processes) read at `now_ms`.
    /// A process not in it is forgotten, and one whose CPU time grew by
    /// more than [`PROGRESS_CPU_PER_MILLE`] of the time since it last made
    /// progress makes progress now. A new process (or a pid now running
    /// another command, or whose CPU time went down) starts its idle time
    /// here when it is a root of `processes` or used more than
    /// [`PROGRESS_CPU_PER_MILLE`] of its own elapsed time; a quiet new
    /// child takes the last progress of its nearest ancestor in
    /// `processes` instead, so a poll that forks a new `sleep` at every
    /// sample is not progress (task 646). The ancestor's last progress is
    /// that of its subtree in the previous sample, so the next step of a
    /// pipeline caught at birth after a busy step ended is not idle at once. Its own later progress is
    /// measured from when it was first seen, not from the ancestor's
    /// progress. A process whose CPU time could not be read is left out,
    /// and a quiet child of it starts over.
    pub fn observe(&mut self, processes: &[ProcessInfo], now_ms: i64) {
        let by_pid: HashMap<u32, &ProcessInfo> = processes.iter().map(|p| (p.pid, p)).collect();
        let mut tracks = HashMap::new();
        let mut new = Vec::new();
        for process in processes {
            let Some(cpu_ms) = process.cpu_ms else {
                continue;
            };
            match self.tracks.remove(&process.pid) {
                Some(track) if track.command == process.command && cpu_ms >= track.cpu_ms => {
                    let wall = u64::try_from(now_ms - track.measured_ms).unwrap_or(0);
                    let used = cpu_ms - track.active_cpu_ms;
                    let track = if used.saturating_mul(1000)
                        > wall.saturating_mul(PROGRESS_CPU_PER_MILLE)
                    {
                        Track::fresh(process, cpu_ms, now_ms)
                    } else {
                        Track { cpu_ms, ..track }
                    };
                    tracks.insert(process.pid, track);
                }
                _ => new.push(process),
            }
        }
        // Parents before children: a quiet new child may take the progress
        // of a new parent.
        let depth = |process: &ProcessInfo| {
            let mut depth = 0;
            let mut pid = process.ppid;
            while let Some(parent) = by_pid.get(&pid).filter(|p| p.pid != p.ppid) {
                depth += 1;
                if depth > by_pid.len() {
                    break;
                }
                pid = parent.ppid;
            }
            depth
        };
        new.sort_by_key(|p| depth(p));
        for process in new {
            let Some(cpu_ms) = process.cpu_ms else {
                continue;
            };
            let elapsed_ms = process.elapsed_secs.saturating_mul(1000);
            let quiet =
                cpu_ms.saturating_mul(1000) <= elapsed_ms.saturating_mul(PROGRESS_CPU_PER_MILLE);
            let parent = by_pid
                .get(&process.ppid)
                .filter(|parent| parent.pid != process.pid);
            let track = match parent {
                Some(parent) if quiet => match tracks.get(&parent.pid) {
                    Some(track) => Track {
                        command: process.command.clone(),
                        // The parent's subtree in the last sample: a child
                        // that made progress and ended since still counts.
                        active_ms: self
                            .subtree_ms
                            .get(&parent.pid)
                            .map_or(track.active_ms, |&ms| ms.max(track.active_ms)),
                        measured_ms: now_ms,
                        active_cpu_ms: cpu_ms,
                        cpu_ms,
                    },
                    None => Track::fresh(process, cpu_ms, now_ms),
                },
                _ => Track::fresh(process, cpu_ms, now_ms),
            };
            tracks.insert(process.pid, track);
        }
        self.handed.retain(|pid, _| tracks.contains_key(pid));
        let mut subtree_ms: HashMap<u32, i64> = HashMap::new();
        for (&pid, track) in &tracks {
            let mut member = pid;
            for _ in 0..=by_pid.len() {
                let ms = subtree_ms.entry(member).or_insert(i64::MIN);
                *ms = (*ms).max(track.active_ms);
                match by_pid.get(&member) {
                    Some(p) if p.ppid != p.pid && by_pid.contains_key(&p.ppid) => member = p.ppid,
                    _ => break,
                }
            }
        }
        self.subtree_ms = subtree_ms;
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
        // A child that used CPU when first seen makes progress.
        watch.observe(
            &[process(10, 1, Some(0)), process(11, 10, Some(1000))],
            5 * MIN,
        );
        let both = [process(10, 1, Some(0)), process(11, 10, Some(1000))];
        watch.observe(&both, 35 * MIN);
        let idle = watch.idle(&both, 35 * MIN, 30 * 60);
        assert_eq!(idle[0].active_ms, 5 * MIN);
        watch.hand(&idle);
        let parent = [process(10, 1, Some(0))];
        watch.observe(&parent, 36 * MIN);
        assert!(watch.idle(&parent, 36 * MIN, 30 * 60).is_empty());
    }

    fn child(pid: u32, ppid: u32, cpu_ms: u64, elapsed_secs: u64) -> ProcessInfo {
        ProcessInfo {
            elapsed_secs,
            command: "sleep 5".to_owned(),
            ..process(pid, ppid, Some(cpu_ms))
        }
    }

    #[test]
    fn a_poll_that_forks_a_quiet_child_at_every_sample_is_idle() {
        let mut watch = CpuWatch::default();
        let mut last = Vec::new();
        for minute in 0..=31 {
            // `while ! cond; do sleep 5; done`: a new `sleep` at every sample,
            // under a quiet shell under a quiet agent's tool call.
            last = vec![
                process(10, 1, Some(100)),
                process(11, 10, Some(20)),
                child(100 + minute as u32, 11, 1, 2),
            ];
            watch.observe(&last, minute * MIN);
            if minute < 30 {
                assert!(watch.idle(&last, minute * MIN, 30 * 60).is_empty());
            }
        }
        let idle = watch.idle(&last, 31 * MIN, 30 * 60);
        assert_eq!(idle.len(), 1, "{idle:?}");
        assert_eq!(idle[0].pid, 10);
        assert_eq!(idle[0].descendants, [11, 131]);
        assert_eq!(idle[0].idle_secs, 31 * 60);
        assert_eq!(idle[0].active_ms, 0);
        // A new quiet child of a new quiet child takes the same progress.
        let deeper = [
            process(10, 1, Some(100)),
            process(11, 10, Some(20)),
            child(201, 200, 0, 0),
            child(200, 11, 0, 0),
        ];
        watch.observe(&deeper, 32 * MIN);
        let idle = watch.idle(&deeper, 32 * MIN, 30 * 60);
        assert_eq!(idle.len(), 1, "{idle:?}");
        assert_eq!(idle[0].active_ms, 0);
        // A quiet new root starts over.
        let root = [child(300, 1, 0, 0)];
        watch.observe(&root, 33 * MIN);
        assert!(watch.idle(&root, 33 * MIN, 30 * 60).is_empty());
    }

    #[test]
    fn a_new_child_that_uses_cpu_is_progress_after_a_long_quiet_parent() {
        let mut watch = CpuWatch::default();
        let quiet = [process(10, 1, Some(100))];
        watch.observe(&quiet, 0);
        watch.observe(&quiet, 29 * MIN);
        // A build started by the poll: busy when first seen.
        let build = |cpu_ms: u64, elapsed_secs: u64| {
            [
                process(10, 1, Some(100)),
                child(50, 10, cpu_ms, elapsed_secs),
            ]
        };
        let sample = build(20_000, 30);
        watch.observe(&sample, 29 * MIN + 30_000);
        watch.observe(&sample, 31 * MIN);
        assert!(watch.idle(&sample, 31 * MIN, 30 * 60).is_empty());
        // Started just before a sample, quiet when first seen, then busy: its
        // progress is measured from when it was seen, not from the parent's
        // old progress, so 2% of a minute counts.
        let mut watch = CpuWatch::default();
        watch.observe(&quiet, 0);
        let sample = build(0, 0);
        watch.observe(&sample, 29 * MIN);
        assert_eq!(watch.tracks[&50].active_ms, 0);
        let sample = build(1200, 60);
        watch.observe(&sample, 30 * MIN);
        assert_eq!(watch.tracks[&50].active_ms, 30 * MIN);
        watch.observe(&sample, 31 * MIN);
        assert!(watch.idle(&sample, 31 * MIN, 30 * 60).is_empty());
        // A reused pid is new too: busy, it is progress.
        let mut reused = child(50, 10, 60_000, 60);
        reused.command = "cargo test".to_owned();
        let sample = [process(10, 1, Some(100)), reused];
        watch.observe(&sample, 70 * MIN);
        assert_eq!(watch.tracks[&50].active_ms, 70 * MIN);
    }

    #[test]
    fn a_quiet_new_child_takes_the_progress_of_a_child_that_ended() {
        let mut watch = CpuWatch::default();
        // cargo waits while rustc A works, then A ends and B is caught at
        // birth.
        let cargo = process(10, 1, Some(100));
        watch.observe(&[cargo.clone(), child(11, 10, 0, 0)], 0);
        for minute in 1..=40 {
            let sample = [cargo.clone(), child(11, 10, minute as u64 * 30_000, 60)];
            watch.observe(&sample, minute * MIN);
        }
        let sample = [cargo.clone(), child(12, 10, 0, 0)];
        watch.observe(&sample, 41 * MIN);
        assert!(watch.idle(&sample, 41 * MIN, 30 * 60).is_empty());
        assert_eq!(watch.tracks[&12].active_ms, 40 * MIN);
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
