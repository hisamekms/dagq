//! Deferring the claim of a task whose files meet a run in flight on a
//! conflict hotspot (ADR-0069); the judgement is
//! [`crate::domain::claim_defer`]. What the judgement reads is cached: the
//! hotspots for [`HOT_REFRESH_SECS`], with the files each task is expected
//! to touch, and the files of the runs in flight for
//! [`IN_FLIGHT_REFRESH_SECS`] or until the next claim. The `[conflicts]`
//! they are judged by is read again every pass (ADR-0080): a change drops
//! the cached hotspots. A run in flight that only waits for a person's
//! answer stops holding the claims back past
//! `[conflicts] waiting_owner_grace_secs` (ADR-t1484-1), and a failed run
//! with no commit of its own does not hold them back at all (ADR-t1634-1).
use crate::domain::EventKind;
use std::collections::{HashMap, HashSet};

use anyhow::Result;
use tracing::{info, warn};

use super::Supervisor;
use crate::application::DependencyGraph;
use crate::domain::{
    Priority, RunId, TaskId,
    claim_defer::{
        self, DEFERRAL_KINDS, Decision, Deferral, InFlight, RELATED_TASKS, deferrals_in_place,
        expected_files, failed_without_commit, owner_waiting_since, worker_deferral_ended,
        worker_deferrals_in_place, worker_deferred,
    },
    provider_switch::route_of,
    stats::{
        ConflictConfig, ConflictConfigReport,
        conflicts::{CONFLICTS_CONFIG_CHANGED, conflicts_at_start, conflicts_change},
    },
    worker::{PROVIDER_UNAVAILABLE, Worker, unavailable},
};

/// How long the hotspots (and the expected files of the tasks) are reused.
const HOT_REFRESH_SECS: i64 = 600;
/// How long the files of the runs in flight are reused, when no claim
/// comes first.
const IN_FLIGHT_REFRESH_SECS: i64 = 60;

/// What the supervisor keeps between passes.
#[derive(Default)]
pub(super) struct DeferWatch {
    /// A changed value read on the preceding pass, awaiting confirmation.
    pending_conflicts: Option<ConflictConfig>,
    /// Whether `[conflicts]` was read again since the start, the read the
    /// values started with are held against the latest
    /// `conflicts_config_changed` on (ADR-t775-1).
    start_checked: bool,
    /// The alerted hotspots, and when they were read.
    hot: Option<(i64, Vec<String>)>,
    /// The expected files of each task read since the hotspots were.
    expected: HashMap<TaskId, Vec<String>>,
    /// The runs in flight and their files, and when they were read.
    in_flight: Option<(i64, Vec<InFlight>)>,
    /// The deferrals in place; `None` until read from the queue.
    deferrals: Option<HashMap<TaskId, Deferral>>,
    /// The deferrals for a worker this supervisor cannot run (ADR-t813-2),
    /// with their reason and since when; `None` until read from the queue.
    worker_deferrals: Option<HashMap<TaskId, (String, i64)>>,
}

impl DeferWatch {
    /// Confirm a change only after two consecutive reads (ADR-t774-1).
    /// Missing or invalid input breaks the streak without touching the cache.
    fn confirm_conflicts(
        &mut self,
        config: Option<ConflictConfig>,
        current: ConflictConfig,
    ) -> bool {
        let Some(config) = config.filter(|config| *config != current) else {
            self.pending_conflicts = None;
            return false;
        };
        if self.pending_conflicts != Some(config) {
            self.pending_conflicts = Some(config);
            return false;
        }
        self.pending_conflicts = None;
        self.hot = None;
        true
    }

    /// A claim just made: the next judgement reads the runs in flight
    /// again, with the claimed one.
    pub(super) fn claimed(&mut self) {
        self.in_flight = None;
    }
}

/// The `[conflicts]` a supervisor starts with: `given` by the options, or
/// read by `load` from `dagq.toml`, the defaults when there is none or it
/// cannot be read (ADR-0080 decision 12). An error is warned of here and
/// returned, so the first read again does not warn of it once more
/// (ADR-t775-1).
pub fn read_conflicts_at_start(
    given: Option<ConflictConfig>,
    load: impl FnOnce() -> Result<Option<ConflictConfig>>,
) -> (ConflictConfigReport, Option<String>) {
    let mut error = None;
    let config = given.or_else(|| {
        load().unwrap_or_else(|failure| {
            let message = format!("{failure:#}");
            warn!(error = %message, "[conflicts] of dagq.toml not read: {message}; using the defaults");
            error = Some(message);
            None
        })
    });
    (crate::application::stats::conflict_config(config), error)
}

/// Whether `message` is an error other than `last`, the one warned of
/// last, which it then becomes.
fn new_error(last: &mut Option<String>, message: String) -> bool {
    if last.as_ref() == Some(&message) {
        return false;
    }
    *last = Some(message);
    true
}

impl Supervisor<'_> {
    /// Read `[conflicts]` of the main checkout's `dagq.toml` again
    /// (ADR-0080, amended by ADR-t774-1). Two consecutive reads of the
    /// same changed values replace those in use, drop
    /// the cached hotspots and are recorded as `conflicts_config_changed`,
    /// once for the queue. A file that cannot be read or holds invalid
    /// values keeps those in use, warned of once per error; so does a
    /// missing file, which may only be a checkout rewriting it. A first
    /// read of the values started with records them when the latest change
    /// on the queue moved elsewhere (ADR-t775-1).
    pub(super) fn reread_conflicts(&mut self) -> Result<()> {
        let Some(read) = self.conflicts_file.clone() else {
            return Ok(());
        };
        let first = !std::mem::replace(&mut self.defer.start_checked, true);
        let config = match read() {
            Ok(config) => {
                self.conflicts_error = None;
                match config {
                    Some(config) => config,
                    None => {
                        self.defer.confirm_conflicts(None, self.conflicts.config);
                        return Ok(());
                    }
                }
            }
            Err(error) => {
                self.defer.confirm_conflicts(None, self.conflicts.config);
                let message = format!("{error:#}");
                if new_error(&mut self.conflicts_error, message.clone()) {
                    warn!(error = %message, "[conflicts] of dagq.toml not read: {message}; keeping the values in use");
                }
                return Ok(());
            }
        };
        let from = self.conflicts.config;
        // Only while the file still holds them: values changed since the
        // start may already be another supervisor's latest `to`.
        if first && config == from {
            self.record_conflicts_at_start()?;
        }
        let confirmed = self.defer.confirm_conflicts(Some(config), from);
        if from == config {
            self.conflicts.source = "file";
        }
        if !confirmed {
            return Ok(());
        }
        self.conflicts = ConflictConfigReport {
            config,
            source: "file",
        };
        let last = self.queue.latest_queue_event(&[CONFLICTS_CONFIG_CHANGED])?;
        if let Some(mut payload) =
            conflicts_change(from, config, last.as_ref().map(|event| &event.payload))
        {
            info!(
                "[conflicts] of dagq.toml changed ({} -> {}): the hotspots and the deferred claims are judged by the new values",
                payload["from"], payload["to"]
            );
            payload["supervisor"] = serde_json::json!(self.token);
            self.queue
                .record_queue_event(EventKind::ConflictsConfigChanged, payload)?;
        }
        Ok(())
    }

    /// Record the values started with from `dagq.toml` as
    /// `conflicts_config_changed` when the latest one on the queue moved to
    /// others, so its `to` is the values in use and a later change back to
    /// them is recorded (ADR-t775-1). The defaults started with for want
    /// of a readable file are not recorded.
    fn record_conflicts_at_start(&mut self) -> Result<()> {
        if self.conflicts.source != "file" {
            return Ok(());
        }
        let last = self.queue.latest_queue_event(&[CONFLICTS_CONFIG_CHANGED])?;
        if let Some(mut payload) = conflicts_at_start(
            self.conflicts.config,
            last.as_ref().map(|event| &event.payload),
        ) {
            info!(
                "[conflicts] of dagq.toml at the start ({}) differs from the latest change recorded ({}): recorded as a change",
                payload["to"], payload["from"]
            );
            payload["supervisor"] = serde_json::json!(self.token);
            self.queue
                .record_queue_event(EventKind::ConflictsConfigChanged, payload)?;
        }
        Ok(())
    }

    /// The candidates of `graph` this pass may claim, in its order: those
    /// whose worker this supervisor cannot run (ADR-t813-2) and those whose
    /// files meet a run in flight on a hotspot are passed over (ADR-0069),
    /// and the start and end of each deferral are recorded on its task.
    pub(super) fn claimable(&mut self, graph: &DependencyGraph) -> Result<Vec<TaskId>> {
        let now = self.generators.clock.now();
        let max_secs = self.conflicts.config.defer_max_secs;
        let (hot, in_flight) = self.hot_in_flight(now)?;
        // The runs that only wait for a person past the grace (ADR-t1484-1)
        // and the failed runs with no commit of their own (ADR-t1634-1) are
        // left out.
        let counted = claim_defer::counted(
            &in_flight,
            now,
            self.conflicts.config.waiting_owner_grace_secs,
        );
        let latest = if self.defer.deferrals.is_none() || self.defer.worker_deferrals.is_none() {
            self.queue.latest_task_events(&DEFERRAL_KINDS)?
        } else {
            Vec::new()
        };
        let mut deferrals = match self.defer.deferrals.take() {
            Some(deferrals) => deferrals,
            None => deferrals_in_place(&latest),
        };
        let mut worker_deferrals = match self.defer.worker_deferrals.take() {
            Some(deferrals) => deferrals,
            None => worker_deferrals_in_place(&latest),
        };
        // Every worker runs here, on its own provider or the other one: no
        // candidate is read for its worker (ADR-t813-2).
        let routes = self.routes();
        let workers: HashMap<TaskId, Worker> = if Worker::ALL
            .iter()
            .all(|worker| route_of(&routes, *worker).is_some())
        {
            HashMap::new()
        } else {
            self.queue
                .candidates()?
                .into_iter()
                .map(|task| (task.id(), task.worker()))
                .collect()
        };
        let mut order = Vec::new();
        let mut events = Vec::new();
        for &id in &graph.candidates {
            let reason = workers.get(&id).and_then(|&worker| {
                route_of(&routes, worker).is_none().then(|| {
                    // Held rather than missing, it is its provider that
                    // cannot be used.
                    let why = unavailable(worker, &self.workers).unwrap_or(PROVIDER_UNAVAILABLE);
                    (worker, why)
                })
            });
            match (reason, worker_deferrals.get(&id)) {
                (Some(_), Some(_)) => continue,
                (Some((worker, why)), None) => {
                    events.push((id, worker_deferred(why, worker, &self.token)));
                    worker_deferrals.insert(id, (why.to_owned(), now));
                    continue;
                }
                (None, Some(_)) => {
                    if let Some((why, since)) = worker_deferrals.remove(&id) {
                        events.push((
                            id,
                            worker_deferral_ended(&why, "cleared", since, now, &self.token),
                        ));
                    }
                }
                (None, None) => {}
            }
            let interrupt = graph
                .tasks
                .iter()
                .find(|node| node.id == id)
                .is_some_and(|node| node.effective_priority == Priority::Interrupt);
            let (overlap, left_out) = if hot.is_empty() || interrupt {
                (None, None)
            } else {
                let expected = self.expected(id)?;
                let overlap = claim_defer::overlap(&hot, &expected, &counted);
                let left_out = if counted.len() < in_flight.len() {
                    claim_defer::left_out(&hot, &expected, &in_flight, &counted)
                } else {
                    None
                };
                (overlap, left_out)
            };
            let mut deferral = deferrals.get(&id).copied();
            let decision = claim_defer::decide(
                interrupt,
                overlap,
                left_out,
                &mut deferral,
                now,
                max_secs,
                &self.token,
            );
            match deferral {
                Some(deferral) => deferrals.insert(id, deferral),
                None => deferrals.remove(&id),
            };
            match decision {
                Decision::Claim { event } => {
                    events.extend(event.map(|event| (id, event)));
                    order.push(id);
                }
                Decision::Defer { event } => events.extend(event.map(|event| (id, event))),
            }
        }
        let candidates: HashSet<TaskId> = graph.candidates.iter().copied().collect();
        worker_deferrals.retain(|id, (why, since)| {
            if candidates.contains(id) {
                return true;
            }
            events.push((
                *id,
                worker_deferral_ended(why, "not_candidate", *since, now, &self.token),
            ));
            false
        });
        self.defer.worker_deferrals = Some(worker_deferrals);
        deferrals.retain(|id, deferral| {
            if candidates.contains(id) {
                return true;
            }
            events.extend(claim_defer::left(*deferral, now, &self.token).map(|event| (*id, event)));
            false
        });
        self.defer.deferrals = Some(deferrals);
        for (id, (kind, payload)) in events {
            match kind {
                EventKind::ClaimDeferred => warn!(
                    task_id = %id,
                    "claim of task {id} deferred: {}",
                    payload["message"].as_str().unwrap_or_default()
                ),
                _ => info!(
                    task_id = %id,
                    "claim of task {id} no longer deferred ({})",
                    payload["why"].as_str().unwrap_or_default()
                ),
            }
            self.queue.record_task_event(id, kind, payload)?;
        }
        Ok(order)
    }

    /// The hotspots some run in flight touches, and the runs in flight;
    /// both empty when no hotspot is alerted.
    fn hot_in_flight(&mut self, now: i64) -> Result<(Vec<String>, Vec<InFlight>)> {
        if self
            .defer
            .hot
            .as_ref()
            .is_none_or(|(at, _)| now - at >= HOT_REFRESH_SECS)
        {
            let hot = match self.conflict_hotspot_files() {
                Ok(files) => files
                    .into_iter()
                    .filter(|file| file.alert)
                    .map(|file| file.renamed_to.unwrap_or(file.path))
                    .collect(),
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "the conflict hotspots could not be read: {error:#}; no claim is deferred on them");
                    Vec::new()
                }
            };
            self.defer.hot = Some((now, hot));
            self.defer.expected.clear();
            self.defer.in_flight = None;
        }
        let hot = self.defer.hot.as_ref().map(|(_, hot)| hot.clone());
        let hot = hot.unwrap_or_default();
        if hot.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        if self
            .defer
            .in_flight
            .as_ref()
            .is_none_or(|(at, _)| now - at >= IN_FLIGHT_REFRESH_SECS)
        {
            let in_flight = self.runs_in_flight()?;
            self.defer.in_flight = Some((now, in_flight));
        }
        let in_flight = self
            .defer
            .in_flight
            .as_ref()
            .map(|(_, runs)| runs.clone())
            .unwrap_or_default();
        let touched = hot
            .into_iter()
            .filter(|path| {
                in_flight
                    .iter()
                    .any(|run| claim_defer::touches(&run.files, path))
            })
            .collect();
        Ok((touched, in_flight))
    }

    /// The runs in flight (the latest run of each in-progress task) and
    /// the files each is expected to touch: its diff from its base to its
    /// head and the expected files of its task (ADR-0069 decision 2), with
    /// since when each only waits for a person (ADR-t1484-1) and whether it
    /// failed with no commit of its own (ADR-t1634-1).
    pub(super) fn runs_in_flight(&mut self) -> Result<Vec<InFlight>> {
        let mut in_flight = Vec::new();
        for run in self.queue.latest_runs_in_progress()? {
            let mut files = self.expected(run.task_id())?;
            let head = run
                .result_commit()
                .map(|commit| commit.as_str().to_owned())
                .or_else(|| run.branch().map(str::to_owned));
            let mut changed = None;
            // A head that cannot be read may hold a change: the run counts.
            let mut readable = true;
            if let Some(head) = head {
                match self
                    .repository
                    .changed_paths(run.base_commit().as_str(), &head)
                {
                    Ok(paths) => changed = Some(paths),
                    Err(error) => {
                        readable = false;
                        warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "the files run {} changed could not be read: {error:#}", run.id())
                    }
                }
            }
            files.extend(changed.iter().flatten().cloned());
            in_flight.push(InFlight {
                run_id: run.id().as_str().to_owned(),
                task_id: run.task_id(),
                files,
                owner_waiting_since: self.owner_waiting_since(run.id())?,
                no_commit: readable && failed_without_commit(run.status(), changed.as_deref()),
            });
        }
        Ok(in_flight)
    }

    /// Since when `run` only waits for a person's answer: its open asks,
    /// its lease and its events, read only when it has an open ask.
    fn owner_waiting_since(&self, run: &RunId) -> Result<Option<i64>> {
        let asks = self.queue.unclosed_run_asks(run)?;
        if !asks
            .iter()
            .any(|ask| ask.answered_at.is_none() && claim_defer::waits_for_owner(&ask.kind))
        {
            return Ok(None);
        }
        let leased = self.queue.run_lease(run)?.is_some();
        let events = self.queue.run_events(run)?;
        Ok(owner_waiting_since(&asks, leased, &events))
    }

    /// The files `task` is expected to touch as it is now, not as cached:
    /// for a task whose paths may have changed since (one under plan
    /// review).
    pub(super) fn expected_now(&mut self, task: TaskId) -> Result<Vec<String>> {
        self.defer.expected.remove(&task);
        self.expected(task)
    }

    /// The files `task` is expected to touch: its declared paths, or the
    /// files its most related landed tasks changed.
    pub(super) fn expected(&mut self, task: TaskId) -> Result<Vec<String>> {
        if let Some(files) = self.defer.expected.get(&task) {
            return Ok(files.clone());
        }
        let declared = self.queue.show(task)?.task.paths().to_vec();
        let mut changed = Vec::new();
        if declared.is_empty() {
            match self.queue.related_landed_commits(task, RELATED_TASKS) {
                Ok(commits) => {
                    for commit in commits {
                        match self
                            .repository
                            .changed_paths(&format!("{commit}^"), &commit)
                        {
                            Ok(paths) => changed.extend(paths),
                            Err(error) => {
                                warn!(error = %format_args!("{error:#}"), "the files commit {commit} changed could not be read: {error:#}")
                            }
                        }
                    }
                }
                Err(error) => {
                    warn!(task_id = %task, error = %format_args!("{error:#}"), "the tasks related to task {task} could not be read: {error:#}")
                }
            }
        }
        let files = expected_files(&declared, &changed);
        self.defer.expected.insert(task, files.clone());
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_consecutive_changed_values_invalidate_the_hotspot_cache() {
        let current = ConflictConfig {
            defer_max_secs: 7200,
            ..ConflictConfig::default()
        };
        let changed = ConflictConfig::default();
        let partial = ConflictConfig {
            hotspot_conflicts: 4,
            ..changed
        };
        let cached = Some((123, vec!["cached.md".to_owned()]));
        let mut watch = DeferWatch {
            hot: cached.clone(),
            ..DeferWatch::default()
        };
        // A restored file, a missing/invalid file, or a different partial
        // table breaks confirmation. None may discard the cached hotspots.
        for interruption in [Some(current), None, Some(partial)] {
            assert!(!watch.confirm_conflicts(Some(changed), current));
            assert!(!watch.confirm_conflicts(interruption, current));
            assert!(!watch.confirm_conflicts(Some(changed), current));
            assert_eq!(watch.hot, cached);
            assert!(!watch.confirm_conflicts(Some(current), current));
        }
        assert!(!watch.confirm_conflicts(Some(changed), current));
        assert!(watch.confirm_conflicts(Some(changed), current));
        assert!(watch.hot.is_none());
        watch.hot = cached.clone();
        assert!(!watch.confirm_conflicts(Some(changed), changed));
        assert_eq!(watch.hot, cached);
    }

    #[test]
    fn an_error_at_the_start_is_not_warned_of_again_by_the_first_read() {
        let given = ConflictConfig {
            defer_max_secs: 60,
            ..ConflictConfig::default()
        };
        let (report, error) = read_conflicts_at_start(Some(given), || unreachable!());
        assert_eq!((report.config, report.source, error), (given, "file", None));
        let (report, error) = read_conflicts_at_start(None, || Ok(None));
        assert_eq!((report, error), (ConflictConfigReport::default(), None));
        let (report, mut last) =
            read_conflicts_at_start(None, || Err(anyhow::anyhow!("dagq.toml:2: invalid")));
        assert_eq!(report, ConflictConfigReport::default());
        assert_eq!(last.as_deref(), Some("dagq.toml:2: invalid"));
        // The first read again meets the same error: no second warn.
        assert!(!new_error(&mut last, "dagq.toml:2: invalid".to_owned()));
        assert!(new_error(&mut last, "dagq.toml:3: invalid".to_owned()));
        assert!(!new_error(&mut last, "dagq.toml:3: invalid".to_owned()));
        let mut none = None;
        assert!(new_error(&mut none, "dagq.toml:2: invalid".to_owned()));
    }
}
