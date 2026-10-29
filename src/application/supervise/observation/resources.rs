//! 観測と分析's measure of the runs' resources from outside (ADR-t1662-1
//! decision 8 and (ii), ADR-t1662-3 decisions 1 and 4,
//! docs/design/measurement-environment.md "runの資源"): every
//! [`ResourcesPort::interval`] a job thread asks the environment what each
//! run of the supervisor's slots attached to it spent since the last
//! window, by the processes the run registered (its wrapper and agent)
//! and those the supervisor started for it in other trees (its headless
//! jobs, the landing's verification commands, [`RunChildren`]), each with
//! the processes under it, and records the window
//! under the run's last `environment_attached`. A run it measured that
//! left its slots (its session ended, it came to rest, it ended) gets one
//! more window, marked last, which the run's ledger row waits on to
//! settle. A run whose last attachment no live supervisor's lease holds
//! and no window closed (its supervisor died) is closed by any
//! supervisor's pass with a last window of no value. An environment that
//! cannot measure a run from outside gives no value, and the ledger folds
//! what the run's executor measured from inside instead. Telemetry only:
//! a failure is logged and changes no run.

use super::*;
use crate::application::{ExecutionEnvironment, ResourceQueue};
use crate::domain::environment::{Resources, RunChildren, RunProcesses, RunResource};
use crate::domain::stats::timestamp_millis;

/// How often the supervisor measures the runs' resources.
pub const INTERVAL_SECS: u64 = 30;

/// What the supervisor measures the runs' resources with.
#[derive(Clone)]
pub struct ResourcesPort {
    /// How often a pass runs.
    pub interval: Duration,
    /// The environment the runs execute in.
    pub environment: Arc<dyn ExecutionEnvironment>,
    /// Opens the queue on the job's thread.
    pub store: Arc<dyn QueueOpener<dyn ResourceQueue + Send>>,
    /// The processes the supervisor started for its runs outside their
    /// workers' trees, which it measures with them.
    pub children: RunChildren,
}

/// A run the pass measures: the `environment_attached` it measures it
/// under and when its last window ended (unix ms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Track {
    pub attachment: i64,
    pub since_ms: i64,
    /// A write of its window got no answer, so it may have been done: the
    /// next pass goes on from the windows it finds stored ([`settle`]).
    pub unsure: bool,
}

/// What one pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Measured {
    /// The windows recorded.
    pub windows: usize,
    /// The last windows recorded (of runs that left the slots, and of
    /// attachments no live supervisor holds).
    pub closed: usize,
    /// Why it stopped short, when it did.
    pub failure: Option<String>,
}

/// One pass on `store` at `now_ms` (unix ms): measure the runs of `held`
/// (the supervisor's slots) attached to `environment`, by the processes
/// they registered and those of `children`, close those of
/// `tracked` no longer held, and those whose last attachment is open with
/// no live lease (`alive` says whether a pid lives). `tracked` keeps what
/// the next pass goes on from.
pub fn measure_pass(
    store: &dyn ResourceQueue,
    environment: &dyn ExecutionEnvironment,
    children: &RunChildren,
    tracked: &mut BTreeMap<String, Track>,
    held: &[RunId],
    now_ms: i64,
    alive: &dyn Fn(u32) -> bool,
) -> Measured {
    let mut measured = Measured::default();
    if let Err(error) = pass(
        store,
        environment,
        children,
        tracked,
        held,
        now_ms,
        alive,
        &mut measured,
    ) {
        measured.failure = Some(format!("{error:#}"));
    }
    measured
}

#[allow(clippy::too_many_arguments)]
fn pass(
    store: &dyn ResourceQueue,
    environment: &dyn ExecutionEnvironment,
    children: &RunChildren,
    tracked: &mut BTreeMap<String, Track>,
    held: &[RunId],
    now_ms: i64,
    alive: &dyn Fn(u32) -> bool,
    measured: &mut Measured,
) -> Result<()> {
    let node = environment.attached().node_id;
    settle(store, environment, tracked, held)?;
    let mut now_held = BTreeMap::new();
    for run in held {
        let events = store.run_events(run)?;
        let Some(attached) = events
            .iter()
            .rev()
            .find(|event| event.kind == EventKind::EnvironmentAttached)
        else {
            continue;
        };
        let attachment = attached.id.as_i64();
        let since = timestamp_millis(&attached.created_at).unwrap_or(now_ms);
        // A new attachment (a resume, an adoption) closes the one before.
        if let Some(track) = tracked.get(run.as_str())
            && track.attachment != attachment
        {
            let closed = close(store, &node, run, *track, Resources::default(), now_ms);
            unsure_unless_written(tracked, run, closed)?;
            measured.closed += 1;
            tracked.remove(run.as_str());
        }
        let track = match tracked.get(run.as_str()) {
            Some(track) => *track,
            None => {
                // Taken up again (its measure lost, or back in the slots):
                // from the end of the last window of the attachment, and
                // not at all once that attachment was closed.
                let Some(since) = stored_until(store, run, attachment, since)? else {
                    continue;
                };
                let track = Track {
                    attachment,
                    since_ms: since.min(now_ms),
                    unsure: false,
                };
                tracked.insert(run.to_string(), track);
                track
            }
        };
        now_held.insert(run.clone(), (track, pids_of(store, children, run)?));
    }
    let left: Vec<(RunId, Track)> = tracked
        .iter()
        .filter(|(run, _)| !now_held.keys().any(|held| held.as_str() == run.as_str()))
        .filter_map(|(run, track)| Some((RunId::new(run).ok()?, *track)))
        .collect();
    let ask = |run: &RunId, track: &Track, pids: Vec<u32>| RunProcesses {
        run: run.clone(),
        pids,
        #[allow(clippy::cast_precision_loss)]
        window_secs: (now_ms - track.since_ms).max(0) as f64 / 1000.0,
    };
    let mut asked: Vec<RunProcesses> = now_held
        .iter()
        .map(|(run, (track, pids))| ask(run, track, pids.clone()))
        .collect();
    for (run, track) in &left {
        asked.push(ask(run, track, pids_of(store, children, run)?));
    }
    let spent: BTreeMap<String, Resources> = environment
        .measure_runs(&asked)?
        .into_iter()
        .map(|(run, resources)| (run.to_string(), resources))
        .collect();
    for (run, (track, _)) in &now_held {
        let Some(resources) = spent.get(run.as_str()) else {
            continue;
        };
        let written = store
            .record_run_resource(&window(
                &node,
                run,
                *track,
                resources.clone(),
                now_ms,
                false,
            ))
            .map(drop);
        unsure_unless_written(tracked, run, written)?;
        measured.windows += 1;
        if let Some(track) = tracked.get_mut(run.as_str()) {
            track.since_ms = now_ms;
        }
    }
    for (run, track) in left {
        let resources = spent.get(run.as_str()).cloned().unwrap_or_default();
        let closed = close(store, &node, &run, track, resources, now_ms);
        unsure_unless_written(tracked, &run, closed)?;
        measured.closed += 1;
        tracked.remove(run.as_str());
        environment.forget_run(&run);
    }
    let now = now_ms.div_euclid(1000);
    for (run, attachment) in store.open_attachments()? {
        if tracked.contains_key(run.as_str()) || held.contains(&run) {
            continue;
        }
        let lease_lives = store.run_lease(&run)?.is_some_and(|lease| {
            !crate::domain::heartbeat_stale(alive(lease.pid), now - lease.heartbeat_at)
        });
        if lease_lives {
            continue;
        }
        let track = Track {
            attachment,
            since_ms: now_ms,
            unsure: false,
        };
        close(store, &node, &run, track, Resources::default(), now_ms)?;
        measured.closed += 1;
    }
    Ok(())
}

/// Take up again the runs of `tracked` a write of whose got no answer
/// ([`Track::unsure`]), as the windows stored say: one whose attachment's
/// last window is stored was closed, and is tracked no more (nor kept by
/// `environment` once it left `held`); another goes on from the end of the
/// windows stored, so the next window overlaps none written.
fn settle(
    store: &dyn ResourceQueue,
    environment: &dyn ExecutionEnvironment,
    tracked: &mut BTreeMap<String, Track>,
    held: &[RunId],
) -> Result<()> {
    let unsure: Vec<(RunId, Track)> = tracked
        .iter()
        .filter(|(_, track)| track.unsure)
        .filter_map(|(run, track)| Some((RunId::new(run).ok()?, *track)))
        .collect();
    for (run, track) in unsure {
        match stored_until(store, &run, track.attachment, track.since_ms)? {
            Some(since_ms) => {
                tracked.insert(
                    run.to_string(),
                    Track {
                        since_ms,
                        unsure: false,
                        ..track
                    },
                );
            }
            None => {
                tracked.remove(run.as_str());
                if !held.contains(&run) {
                    environment.forget_run(&run);
                }
            }
        }
    }
    Ok(())
}

/// When `run`'s windows stored under `attachment` end, from `since` (unix
/// ms) on; none once one of them is its last.
fn stored_until(
    store: &dyn ResourceQueue,
    run: &RunId,
    attachment: i64,
    since: i64,
) -> Result<Option<i64>> {
    let earlier: Vec<RunResource> = store
        .run_resources(run)?
        .into_iter()
        .filter(|window| window.attachment == attachment)
        .collect();
    if earlier.iter().any(|window| window.last) {
        return Ok(None);
    }
    Ok(Some(
        earlier
            .iter()
            .map(|window| window.until_ms)
            .fold(since, i64::max),
    ))
}

/// `written`, a write of `run`'s window: when it got no answer it may have
/// been done, so `run`'s track is marked for the next pass to settle.
fn unsure_unless_written(
    tracked: &mut BTreeMap<String, Track>,
    run: &RunId,
    written: Result<()>,
) -> Result<()> {
    if written.is_err()
        && let Some(track) = tracked.get_mut(run.as_str())
    {
        track.unsure = true;
    }
    written
}

/// The pids of `run`'s processes: those it registered that have not
/// exited (a pid of one that exited may be another process's by now), and
/// those the supervisor started for it in other trees that still run.
fn pids_of(store: &dyn ResourceQueue, children: &RunChildren, run: &RunId) -> Result<Vec<u32>> {
    let mut pids: Vec<u32> = store
        .processes(run)?
        .into_iter()
        .filter(|process| process.exited_at.is_none())
        .map(|process| process.pid)
        .chain(children.of(run))
        .collect();
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

fn window(
    node: &str,
    run: &RunId,
    track: Track,
    resources: Resources,
    now_ms: i64,
    last: bool,
) -> RunResource {
    RunResource {
        id: 0,
        run_id: run.to_string(),
        attachment: track.attachment,
        node_id: node.to_owned(),
        from_ms: track.since_ms.min(now_ms),
        until_ms: now_ms,
        resources,
        last,
    }
}

/// Record the last window of `run`'s attachment `track`.
fn close(
    store: &dyn ResourceQueue,
    node: &str,
    run: &RunId,
    track: Track,
    resources: Resources,
    now_ms: i64,
) -> Result<()> {
    store.record_run_resource(&window(node, run, track, resources, now_ms, true))?;
    Ok(())
}

/// Whether a pass is due at `at`: at once when none started yet
/// (`started` none), then once `interval` passed since the last one
/// started (a clock that reads earlier than that start reads none passed).
fn pass_due(started: Option<Instant>, at: Instant, interval: Duration) -> bool {
    started.is_none_or(|started| at.saturating_duration_since(started) >= interval)
}

/// The runs measured, the pass running now and how the last ones went.
#[derive(Default)]
pub(in crate::application::supervise) struct ResourcesWatch {
    /// The runs measured while no pass runs (a pass takes them and gives
    /// them back).
    tracked: Option<BTreeMap<String, Track>>,
    job: Option<thread::JoinHandle<(BTreeMap<String, Track>, Measured)>>,
    started: Option<Instant>,
    failing: bool,
}

impl ResourcesWatch {
    /// Reap the pass once it ended; start one on `held` (the runs of the
    /// slots) when one is due ([`pass_due`]).
    pub(in crate::application::supervise) fn pass(
        &mut self,
        env: &mut ObservationEnv<'_>,
        port: Option<&ResourcesPort>,
        held: Vec<RunId>,
    ) {
        let Some(port) = port.cloned() else {
            return;
        };
        if let Some(job) = self.job.take() {
            if !job.is_finished() {
                self.job = Some(job);
                return;
            }
            self.reap(job);
        }
        let at = env.generators.clock.monotonic();
        if !pass_due(self.started, at, port.interval) {
            return;
        }
        self.started = Some(at);
        let now_ms = env.generators.clock.now().saturating_mul(1000);
        let processes = Arc::clone(env.processes);
        let mut tracked = self.tracked.take().unwrap_or_default();
        self.job = Some(spawn_traced(move || {
            let measured = match port.store.open() {
                Ok(store) => measure_pass(
                    &*store,
                    &*port.environment,
                    &port.children,
                    &mut tracked,
                    &held,
                    now_ms,
                    &|pid| processes.alive(pid),
                ),
                Err(error) => Measured {
                    failure: Some(format!("open the queue: {error:#}")),
                    ..Measured::default()
                },
            };
            (tracked, measured)
        }));
    }

    /// Wait for the pass still running, as the loop ends, until
    /// `deadline` when one is given; one still running then is left, and
    /// the next supervisor closes what it measured.
    pub(in crate::application::supervise) fn finish(
        &mut self,
        env: &mut ObservationEnv<'_>,
        deadline: &super::EndBy,
    ) {
        if let Some(job) = self.job.take() {
            if super::finished_by(&job, deadline, &*env.generators.clock) {
                self.reap(job);
            } else {
                tracing::warn!(
                    "the measure of the runs' resources still runs as the supervisor ends; it is left"
                );
            }
        }
    }

    fn reap(&mut self, job: thread::JoinHandle<(BTreeMap<String, Track>, Measured)>) {
        let failure = match job.join() {
            Ok((tracked, measured)) => {
                self.tracked = Some(tracked);
                measured.failure
            }
            // The runs measured are lost with the thread: their attachments
            // are closed once no live lease holds them.
            Err(_) => Some("the measure of the runs' resources panicked".to_owned()),
        };
        match failure {
            Some(failure) => {
                if !self.failing {
                    warn!(error = %failure, "the runs' resources could not be measured: {failure}");
                }
                self.failing = true;
            }
            None => {
                if self.failing {
                    info!("the runs' resources are measured again");
                }
                self.failing = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pass is due at once on the first, then not 1 ms before its
    /// interval since the last one started, and at it.
    #[test]
    fn a_pass_is_due_first_and_then_once_its_interval_passed() {
        let interval = Duration::from_secs(INTERVAL_SECS);
        let at = Instant::now();
        assert!(pass_due(None, at, interval));
        let started = at.checked_sub(interval).unwrap();
        assert!(pass_due(Some(started), at, interval));
        let started = at.checked_sub(interval - Duration::from_millis(1)).unwrap();
        assert!(!pass_due(Some(started), at, interval));
        // A clock that reads earlier than the last start reads none passed.
        assert!(!pass_due(Some(at + interval), at, interval));
        // An interval of none is a pass each time.
        assert!(pass_due(Some(at), at, Duration::ZERO));
    }
}
