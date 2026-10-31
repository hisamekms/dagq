//! Forecast snapshots (ADR-0070 decision 3): at most once every
//! [`ForecastPort::check`], a supervisor at work looks for the triggers
//! since the latest `forecast_recorded` (a settled plan, a change mark or
//! a change of an open task's priority or dependencies, a landing, and the
//! first look of a local day with no snapshot yet), and a job thread
//! computes the forecast of every open task and goal and records it as one
//! event, unless the only triggers are landings that moved no p50 past the
//! thresholds. The triggers come from the queue's events, so what a drain
//! or a stop leaves is taken up by the next look, of this process or of
//! another one, and of two supervisors that looked at once one records.
//! It takes no run slot and uses no LLM; a failure is logged and looked at
//! again after [`RETRY`], and stops no claim nor landing.
//!
//! The snapshots are stopped now ([`STOPPED`]): the forecast reads every
//! event of the queue.

use super::*;
use crate::application::forecast::{self, Pending, SnapshotOutcome};
use crate::domain::LeaseToken;

/// How long after a failed look or job the triggers are looked for again.
const RETRY: Duration = Duration::from_secs(600);
/// The snapshots are stopped (request 190, decision (1) of ADR-t3745-1):
/// the forecast they record ([`forecast::record_snapshot`]) reads every
/// event of the queue. No look reads the triggers nor starts a job, so no
/// failure nor retry follows; the log says so once a process. The
/// `forecast_recorded` recorded stay as they are. Task 1670 brings the
/// snapshots back from the ledger and removes this with the branch it
/// stops.
const STOPPED: bool = true;
/// How often the triggers are looked for without `--forecast-check`.
pub const FORECAST_CHECK: Duration = Duration::from_secs(60);

/// What the supervisor takes the snapshots with.
#[derive(Clone)]
pub struct ForecastPort {
    /// The host's time zone at a unix second, seconds east of UTC.
    pub utc_offset: fn(i64) -> i64,
    /// The `[kpi]` `min_samples` at a unix second, read again each time.
    pub min_samples: Arc<dyn Fn(i64) -> Result<usize> + Send + Sync>,
    /// How often the triggers are looked for.
    pub check: Duration,
}

/// The snapshot job running now, and where the last look left off.
#[derive(Default)]
pub(in crate::application::supervise) struct ForecastWatch {
    job: Option<thread::JoinHandle<Result<(i64, SnapshotOutcome)>>>,
    /// The trigger events a job that recorded nothing read through: the
    /// next look starts after them.
    checked: Option<i64>,
    looked: Option<Instant>,
    failed: Option<Instant>,
    /// The log said this process takes no snapshot ([`STOPPED`]).
    told_stopped: bool,
}

impl ForecastWatch {
    pub(in crate::application::supervise) const fn running(&self) -> bool {
        self.job.is_some()
    }

    /// Look at `at` (the monotonic clock's) for the triggers `pending`
    /// reads and start `job` on them, unless [`STOPPED`]: then neither is
    /// run, and the first time the log says why.
    fn look(
        &mut self,
        at: Instant,
        pending: impl FnOnce() -> Result<Option<Pending>>,
        job: impl FnOnce(Pending) -> Result<(i64, SnapshotOutcome)> + Send + 'static,
    ) {
        if STOPPED {
            if !self.told_stopped {
                info!(
                    "the forecast snapshots are stopped: the forecast reads every event of the queue, and they come back once it reads the ledger"
                );
                self.told_stopped = true;
            }
            return;
        }
        self.looked = Some(at);
        let pending = match pending() {
            Ok(Some(pending)) => pending,
            Ok(None) => return,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the forecast triggers could not be read: {error:#}");
                self.failed = Some(at);
                return;
            }
        };
        self.job = Some(spawn_traced(move || job(pending)));
    }
}

impl ObservationState {
    /// Reap the snapshot job once it ended; with `start`, look for the
    /// triggers when it is time and start a job when there are any.
    pub(in crate::application::supervise) fn forecast_pass(
        &mut self,
        env: &mut ObservationEnv<'_>,
        start: bool,
    ) {
        let Some(port) = self.forecasts.clone() else {
            return;
        };
        if let Some(job) = self.forecast.job.take() {
            if !job.is_finished() {
                self.forecast.job = Some(job);
                return;
            }
            match job.join() {
                Ok(Ok((through, outcome))) => {
                    match &outcome {
                        SnapshotOutcome::Recorded {
                            event,
                            tasks,
                            goals,
                        } => info!(
                            "forecast snapshot {event} recorded: {tasks} task(s), {goals} goal(s)"
                        ),
                        SnapshotOutcome::Unmoved => {
                            info!("forecast snapshot skipped: no trigger held")
                        }
                        SnapshotOutcome::Taken => {
                            info!("forecast snapshot skipped: another supervisor recorded one")
                        }
                    }
                    // What another supervisor recorded says itself how far
                    // it looked: the triggers after that are not passed.
                    if outcome != SnapshotOutcome::Taken {
                        self.forecast.checked = Some(through);
                    }
                    self.forecast.failed = None;
                }
                Ok(Err(error)) => {
                    warn!(error = %format_args!("{error:#}"), "the forecast snapshot could not be recorded: {error:#}");
                    self.forecast.failed = Some(env.generators.clock.monotonic());
                }
                Err(_) => {
                    warn!("the forecast snapshot job panicked");
                    self.forecast.failed = Some(env.generators.clock.monotonic());
                }
            }
        }
        let clock = &env.generators.clock;
        if !look_due(
            start,
            self.forecast.failed.map(|at| clock.since(at)),
            self.forecast.looked.map(|at| clock.since(at)),
            port.check,
        ) {
            return;
        }
        let now = env.generators.clock.now();
        let at = env.generators.clock.monotonic();
        let queue = &*env.queue;
        let checked = self.forecast.checked;
        let utc_offset = port.utc_offset;
        let queues = self.queues.clone();
        let processes = env.processes.clone();
        let token = env.token.clone();
        self.forecast.look(
            at,
            || forecast::pending(queue, now, utc_offset, checked),
            move |pending| snapshot(&*queues, &*processes, &port, &token, now, &pending),
        );
    }
}

/// The snapshot job: the forecast at `now` recorded for `pending`, with
/// the event ID its triggers were read through.
fn snapshot(
    queues: &dyn QueueOpener<dyn crate::application::ObservationQueue + Send>,
    processes: &dyn ProcessControl,
    port: &ForecastPort,
    token: &LeaseToken,
    now: i64,
    pending: &Pending,
) -> Result<(i64, SnapshotOutcome)> {
    let min_samples = (port.min_samples)(now)?;
    let queue = queues.open()?;
    let outcome = forecast::record_snapshot(&*queue, processes, token, now, min_samples, pending)?;
    Ok((pending.through, outcome))
}

/// Whether the triggers are looked for now, with `start`: [`RETRY`] after a
/// failure and `check` after the last look, each given as the time since.
fn look_due(
    start: bool,
    since_failed: Option<Duration>,
    since_looked: Option<Duration>,
    check: Duration,
) -> bool {
    start
        && since_failed.is_none_or(|since| since >= RETRY)
        && since_looked.is_none_or(|since| since >= check)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The triggers are looked for at the retry after a failure and at the
    /// check after a look, not 1 ms before; never without `start`.
    #[test]
    fn the_triggers_are_looked_for_once_the_retry_and_the_check_passed() {
        let check = FORECAST_CHECK;
        let ms = Duration::from_millis(1);
        assert!(look_due(true, None, None, check));
        assert!(!look_due(false, None, None, check));
        assert!(!look_due(true, Some(RETRY - ms), None, check));
        assert!(look_due(true, Some(RETRY), None, check));
        assert!(!look_due(true, None, Some(check - ms), check));
        assert!(look_due(true, None, Some(check), check));
        assert!(!look_due(true, Some(RETRY), Some(check - ms), check));
    }

    /// While stopped, no look reads the triggers nor runs the job whose
    /// forecast reads every event: nothing runs, none is due again after a
    /// failure, and the log is told once.
    #[test]
    fn a_stopped_snapshot_reads_nothing() {
        use std::cell::Cell;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let read = Cell::new(0);
        let ran = Arc::new(AtomicUsize::new(0));
        let mut watch = ForecastWatch::default();
        for _ in 0..3 {
            let ran = ran.clone();
            watch.look(
                Instant::now(),
                || {
                    read.set(read.get() + 1);
                    Err(anyhow::anyhow!("the triggers are read"))
                },
                move |_| {
                    ran.fetch_add(1, Ordering::SeqCst);
                    panic!("the snapshot reads every event")
                },
            );
            assert!(!watch.running());
        }
        assert_eq!(read.get(), 0);
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert_eq!(watch.failed, None);
        assert_eq!(watch.looked, None);
        assert!(watch.told_stopped);
    }
}
