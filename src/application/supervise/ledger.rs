//! 観測と分析's builder of the ledger (ADR-t1662-1 (ii) and D7,
//! docs/design/measurement.md "台帳を作る係"): every
//! [`LedgerPort::interval`], draining and handing off too, a job thread
//! reads the inputs that came in after the builder's cursors, folds what
//! they mark and writes the rows [`crate::domain::ledger::builder`] says,
//! at most [`WRITES_PER_PASS`] a pass. Once a local day it removes the
//! node rows past the host's load's retention and the session steps past
//! [`crate::domain::session_step::RETENTION_DAYS`]
//! ([`Retain`]). It reads the events of each run and
//! task it folds, and the builder keeps what the folds of the queue as a
//! whole read ([`crate::domain::ledger::builder::Kept`]): no pass reads
//! the whole EventStore. Nothing of the runtime's control waits for it or
//! reads it, and a landing does not build a row.
//!
//! A pass that cannot read or write is logged (once until a pass succeeds
//! again) and keeps its marks for the next one; after
//! [`FAILING_PASSES`] failed passes in a row it records one
//! `ledger_build_failed` for the run of failures, so a reader of the
//! events sees the ledger fall behind. A run's or a task's outcome never
//! depends on it.

use super::*;
use crate::application::LedgerQueue;
use crate::domain::EventFilter;
use crate::domain::host_metrics::{local_day, oldest_kept_day};
use crate::domain::ledger::builder::{
    Builder, CHECKS_PER_PASS, EVENTS_PER_READ, Inputs, Target, WRITES_PER_PASS, build, hour, writes,
};
use crate::domain::session_step;

/// How many passes in a row fail before one `ledger_build_failed` is
/// recorded.
pub const FAILING_PASSES: u32 = 10;

/// What the supervisor builds the ledger with.
#[derive(Clone)]
pub struct LedgerPort {
    /// How often a pass runs.
    pub interval: Duration,
    /// How many local days of node rows are kept, as the NodeSampleStore's
    /// samples (`None`: the host's load is not recorded, none removed).
    pub retention_days: Option<u32>,
    /// The host's offset from UTC (seconds) at a unix second.
    pub utc_offset: fn(i64) -> i64,
    /// Opens the stores on the job's thread.
    pub store: Arc<dyn QueueOpener<dyn LedgerQueue + Send>>,
}

/// What one pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Built {
    /// The rows written.
    pub written: usize,
    /// The marks left for the next pass.
    pub backlog: usize,
    /// The node rows the retention removed.
    pub removed: usize,
    /// The session steps the retention removed.
    pub steps_removed: usize,
    /// Why it stopped short, when it did.
    pub failure: Option<String>,
}

/// What one pass removes past its retention: the node rows of a local
/// day before `node_rows`, and the session steps that came in before
/// `steps` (unix ms).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Retain {
    pub node_rows: Option<i64>,
    pub steps: Option<i64>,
}

impl Retain {
    /// What a pass at `now` (unix seconds) on the local day `today`
    /// removes: nothing when the retention ran on `today` already
    /// (`retained`), else the steps past their retention and, when the
    /// host's load is kept `node_days` days, the node rows past them.
    pub fn due(now: i64, today: i64, retained: Option<i64>, node_days: Option<u32>) -> Self {
        if retained == Some(today) {
            return Self::default();
        }
        Self {
            node_rows: node_days.and_then(|days| oldest_kept_day(today, days)),
            steps: Some(session_step::retained_since(now.saturating_mul(1000))),
        }
    }

    /// Whether it removes anything.
    pub const fn any(self) -> bool {
        self.node_rows.is_some() || self.steps.is_some()
    }
}

/// One pass on `store` at `now_ms`, the rows built at `built_at`: read the
/// inputs after `builder`'s cursors, mark, fold and write what changed,
/// and drop what `retain` says. A failure stops the pass and keeps every
/// mark not written.
pub fn build_pass(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    now_ms: i64,
    built_at: &str,
    retain: Retain,
) -> Built {
    let mut built = Built::default();
    if let Err(error) = pass(store, builder, now_ms, built_at, &mut built) {
        built.failure = Some(format!("{error:#}"));
    }
    if built.failure.is_none()
        && let Some(before_day) = retain.node_rows
    {
        match store.remove_node_rows(before_day) {
            Ok(removed) => built.removed = removed,
            Err(error) => built.failure = Some(format!("the node rows' retention: {error:#}")),
        }
    }
    if built.failure.is_none()
        && let Some(before) = retain.steps
    {
        match store.remove_steps(before) {
            Ok(removed) => built.steps_removed = removed,
            Err(error) => built.failure = Some(format!("the steps' retention: {error:#}")),
        }
    }
    built.backlog = builder.backlog();
    built
}

fn pass(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    now_ms: i64,
    built_at: &str,
    built: &mut Built,
) -> Result<()> {
    let latest = store.latest_event_id()?;
    while latest > builder.events {
        let new = store.events_between(
            builder.events,
            latest,
            &EventFilter::default(),
            EVENTS_PER_READ,
        )?;
        if new.is_empty() {
            break;
        }
        builder.mark_events(&new);
    }
    let samples = store.node_samples_after(builder.samples)?;
    builder.mark_samples(&samples);
    builder.due(now_ms);
    if !builder.marked() {
        return Ok(());
    }
    let rows = builder.rows_to_build(builder.kept());
    let mut inputs = Inputs::of(builder.kept(), now_ms, built_at, &rows);
    builder.mark_rows(&inputs.rows);
    let mut targets = builder.take(CHECKS_PER_PASS).into_iter();
    while let Some(target) = targets.next() {
        if built.written >= WRITES_PER_PASS {
            builder.keep(target);
            continue;
        }
        if let Err(error) = write(store, builder, &target, &mut inputs, built) {
            builder.keep(target);
            for target in targets {
                builder.keep(target);
            }
            return Err(error);
        }
    }
    Ok(())
}

/// Fold `target`, a run's or a task's on its events up to the builder's
/// cursor, and write it when [`writes`] says so.
fn write(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    target: &Target,
    inputs: &mut Inputs,
    built: &mut Built,
) -> Result<()> {
    let mut samples = Vec::new();
    let events = match target {
        Target::Run(run) => store.run_events(&RunId::new(run)?)?,
        Target::Task(task) => store.task_events(*task)?,
        Target::Node(node, start) => {
            let (from, until) = hour(*start);
            samples = store.node_samples(node, from, until)?;
            Vec::new()
        }
        Target::Row(..) => Vec::new(),
    };
    // The kept pushes and waits reach the cursor; a later event marks it again.
    let through = builder.events;
    inputs.read(
        target,
        events
            .into_iter()
            .filter(|event| event.id <= through)
            .collect(),
    );
    let row = build(target, inputs, &samples);
    inputs.forget(target);
    let Some(row) = row else {
        return Ok(());
    };
    let (kind, key) = target.row_key();
    let stored = store.ledger_row(kind, &key)?;
    if writes(stored.as_ref(), &row) {
        store.write_ledger_row(&row)?;
        built.written += 1;
        builder.track(target, &row);
    } else if let Some(stored) = stored {
        builder.track(target, &stored);
    }
    Ok(())
}

/// The builder, the pass running now and how the last ones went.
#[derive(Default)]
pub(super) struct LedgerWatch {
    /// The builder while no pass runs (a pass takes it and gives it back).
    builder: Option<Builder>,
    job: Option<thread::JoinHandle<(Builder, Built)>>,
    started: Option<Instant>,
    /// The failed passes in a row, and whether that run was recorded.
    failing: u32,
    recorded: bool,
    /// The local day the retention (of the node rows and of the steps)
    /// last ran on, and the one the pass running now runs it on. Kept in
    /// the process only: a process started again that day runs it once
    /// more, which finds nothing to remove and writes nothing
    /// ([`crate::application::LedgerStore::remove_node_rows`],
    /// [`crate::application::SessionStepStore::remove_steps`]).
    retained: Option<i64>,
    retaining: Option<i64>,
}

impl LedgerWatch {
    /// Reap the pass once it ended; start one when the interval passed
    /// since the last one started.
    pub(super) fn pass(&mut self, env: &mut ObservationEnv<'_>, port: Option<&LedgerPort>) {
        let Some(port) = port.cloned() else {
            return;
        };
        if let Some(job) = self.job.take() {
            if !job.is_finished() {
                self.job = Some(job);
                return;
            }
            self.reap(env, job);
        }
        let at = env.generators.clock.monotonic();
        if self
            .started
            .is_some_and(|started| at.saturating_duration_since(started) < port.interval)
        {
            return;
        }
        self.started = Some(at);
        let now = env.generators.clock.now();
        let built_at = env.generators.clock.timestamp();
        let today = local_day(now, (port.utc_offset)(now));
        let retain = Retain::due(now, today, self.retained, port.retention_days);
        self.retaining = retain.any().then_some(today);
        let mut builder = self.builder.take().unwrap_or_default();
        self.job = Some(spawn_traced(move || {
            let built = match port.store.open() {
                Ok(store) => build_pass(&*store, &mut builder, now * 1000, &built_at, retain),
                Err(error) => Built {
                    failure: Some(format!("open the queue: {error:#}")),
                    ..Built::default()
                },
            };
            (builder, built)
        }));
    }

    /// Wait for the pass still running, as the loop ends, until
    /// `deadline` when one is given ([`super::finished_by`]); one still
    /// running then is left, and the next supervisor's ledger catches up.
    pub(super) fn finish(&mut self, env: &mut ObservationEnv<'_>, deadline: Option<Instant>) {
        if let Some(job) = self.job.take() {
            if super::finished_by(&job, deadline, &*env.generators.clock) {
                self.reap(env, job);
            } else {
                tracing::warn!("the ledger's pass still runs as the supervisor ends; it is left");
            }
        }
    }

    fn reap(&mut self, env: &mut ObservationEnv<'_>, job: thread::JoinHandle<(Builder, Built)>) {
        let failure = match job.join() {
            Ok((builder, built)) => {
                self.builder = Some(builder);
                if built.written > 0 || built.removed > 0 || built.steps_removed > 0 {
                    tracing::debug!(
                        written = built.written,
                        backlog = built.backlog,
                        removed = built.removed,
                        steps_removed = built.steps_removed,
                        "the ledger is built"
                    );
                }
                built.failure
            }
            // The builder is lost with the thread: the next starts at the
            // cursors' start and catches up.
            Err(_) => Some("the ledger's pass panicked".to_owned()),
        };
        let retaining = self.retaining.take();
        let Some(failure) = failure else {
            // A failed pass leaves the retention to the next one.
            if retaining.is_some() {
                self.retained = retaining;
            }
            if self.failing > 0 {
                info!("the ledger is built again");
            }
            self.failing = 0;
            self.recorded = false;
            return;
        };
        if self.failing == 0 {
            warn!(error = %failure, "the ledger could not be built: {failure}");
        }
        self.failing += 1;
        if self.failing >= FAILING_PASSES && !self.recorded {
            let payload = json!({
                "supervisor": env.token,
                "passes": self.failing,
                "error": failure,
            });
            match env
                .queue
                .record_queue_event(EventKind::LedgerBuildFailed, payload)
            {
                Ok(_) => self.recorded = true,
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "ledger_build_failed could not be recorded: {error:#}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
