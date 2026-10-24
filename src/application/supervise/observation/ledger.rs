//! 観測と分析's builder of the ledger (ADR-t1662-1 (ii) and D7,
//! docs/design/measurement.md "台帳を作る係"): every
//! [`LedgerPort::interval`], draining and handing off too, a job thread
//! reads the inputs that came in after the builder's cursors (the
//! events, the node samples, the session steps and the runs' resources),
//! folds what they mark, a run's steps and resources into its row, and
//! writes the rows
//! [`crate::domain::ledger::builder`] says,
//! at most [`WRITES_PER_PASS`] a pass. Once a local day it removes the
//! node rows past the host's load's retention and the session steps past
//! [`crate::domain::session_step::RETENTION_DAYS`]
//! ([`Retain`]). It reads the events of each run and
//! task it folds, and the builder keeps what the folds of the queue as a
//! whole read ([`crate::domain::ledger::builder::Kept`]): no pass reads
//! the whole EventStore. Nothing of the runtime's control waits for it or
//! reads it, and a landing does not build a row. A settled row a later
//! event reaches, whose records the builder dropped, gets them back from
//! the row the store holds before it is folded. The log says how much the
//! builder keeps ([`KeptSize`]) after the first pass, then hourly or when
//! its bytes change by half ([`logs_kept`]).
//!
//! A pass that cannot read or write is logged (once until a pass succeeds
//! again) and keeps its marks for the next one; after
//! [`FAILING_PASSES`] failed passes in a row it records one
//! `ledger_build_failed` for the run of failures, so a reader of the
//! events sees the ledger fall behind. A run's or a task's outcome never
//! depends on it.

use std::collections::BTreeSet;

use super::*;
use crate::application::LedgerQueue;
use crate::domain::EventFilter;
use crate::domain::host_metrics::{local_day, oldest_kept_day};
use crate::domain::ledger::LedgerKind;
use crate::domain::ledger::builder::{
    Builder, CHECKS_PER_PASS, EVENTS_PER_READ, Inputs, KeptSize, RESOURCES_PER_READ, RunResources,
    RunSteps, STEPS_PER_READ, Target, WRITES_PER_PASS, behind, build, hour, keeps_settled, writes,
};
use crate::domain::session_step::{self, StepTarget};

/// How many passes in a row fail before one `ledger_build_failed` is
/// recorded.
pub const FAILING_PASSES: u32 = 10;

/// How often the supervisor's log says how much the builder keeps between
/// passes ([`KeptSize`]) while it changes little.
pub const KEPT_LOG_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// By how many percent of the bytes last logged the builder's kept bytes
/// change before the log says them again within [`KEPT_LOG_INTERVAL`].
pub const KEPT_LOG_CHANGE_PERCENT: usize = 50;

/// Whether a pass logs the builder's kept `bytes`, the last log (how long
/// ago and its bytes) being `last`: the first one, one
/// [`KEPT_LOG_INTERVAL`] after the last, and one whose bytes changed by
/// [`KEPT_LOG_CHANGE_PERCENT`] of the last's or more.
pub fn logs_kept(last: Option<(Duration, usize)>, bytes: usize) -> bool {
    let Some((since, logged)) = last else {
        return true;
    };
    since >= KEPT_LOG_INTERVAL
        || bytes != logged
            && bytes.abs_diff(logged).saturating_mul(100)
                >= logged.saturating_mul(KEPT_LOG_CHANGE_PERCENT)
}

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
    /// The rules that classify the runs' command steps now, read on the
    /// job's thread at each pass (the `dagq.toml` of the main checkout
    /// and the queue's key of command digests); `None` classifies none.
    pub classifier: Classifier,
}

/// Reads the rules that classify the runs' command steps
/// ([`LedgerPort::classifier`]).
pub type Classifier =
    Arc<dyn Fn() -> Option<crate::domain::command_rules::CommandClassifier> + Send + Sync>;

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
    /// How much the builder keeps after it.
    pub kept: KeptSize,
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
/// inputs after `builder`'s cursors, mark, fold and write what changed (at
/// no earlier time than an event read, [`Builder::folds_at`]),
/// and drop what `retain` says, the steps only up to those a run's row
/// does not hold yet ([`Builder::steps_held_since`]). A run's command
/// steps are classified by `classifier` ([`RunSteps::classified_by`]);
/// when its rules are of another version than those the builder last
/// marked by, every run row the store holds classified by others is marked
/// ([`Builder::mark_rules`]), so the rules alone fold the runs again. A
/// failure stops the pass and keeps every mark not written.
pub fn build_pass(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    (now_ms, built_at): (i64, &str),
    retain: Retain,
    classifier: Option<&crate::domain::command_rules::CommandClassifier>,
) -> Built {
    let mut built = Built::default();
    if let Err(error) = pass(store, builder, (now_ms, built_at), classifier, &mut built) {
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
    // A run's steps go only once its row holds them.
    if built.failure.is_none()
        && let Some(before) = retain.steps
    {
        let before = builder
            .steps_held_since()
            .map_or(before, |since| before.min(since));
        match store.remove_steps(before) {
            Ok(removed) => built.steps_removed = removed,
            Err(error) => built.failure = Some(format!("the steps' retention: {error:#}")),
        }
    }
    built.backlog = builder.backlog();
    built.kept = builder.kept().size();
    built
}

fn pass(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    (now_ms, built_at): (i64, &str),
    classifier: Option<&crate::domain::command_rules::CommandClassifier>,
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
    loop {
        let new = store.steps_after(builder.steps, STEPS_PER_READ)?;
        builder.mark_steps(&new);
        if new.len() < STEPS_PER_READ {
            break;
        }
    }
    loop {
        let outside = store.run_resources_after(builder.resources, RESOURCES_PER_READ)?;
        let inside = store.resources_after(builder.inside_resources, RESOURCES_PER_READ)?;
        builder.mark_resources(&outside, &inside);
        if outside.len() < RESOURCES_PER_READ && inside.len() < RESOURCES_PER_READ {
            break;
        }
    }
    // Rules of another version classify the runs held settled again.
    let rules = classifier.map(|classifier| classifier.rules().version());
    if builder.rules_moved(rules.as_deref()) {
        let rows = store.ledger_rows(LedgerKind::Run)?;
        builder.mark_rules(rules.as_deref(), &rows);
    }
    // What it read came by then.
    let now_ms = builder.folds_at(now_ms);
    builder.due(now_ms);
    if !builder.marked() {
        return Ok(());
    }
    let rows = builder.rows_to_build(builder.kept());
    restore(store, builder, &rows)?;
    let mut inputs = Inputs::of(builder.kept(), now_ms, built_at, &rows);
    builder.mark_rows(&inputs.rows);
    let mut targets = builder.take(CHECKS_PER_PASS).into_iter();
    while let Some(target) = targets.next() {
        // The next pass starts at what this one did not look at.
        if built.written >= WRITES_PER_PASS {
            builder.put_back(target, targets);
            break;
        }
        if let Err(error) = write(store, builder, &target, (&mut inputs, classifier), built) {
            // Its turn comes again after the rest: one failing each pass
            // stops no other.
            builder.keep(target);
            if let Some(next) = targets.next() {
                builder.put_back(next, targets);
            }
            return Err(error);
        }
    }
    sweep(store, builder, &inputs)?;
    drop(inputs);
    builder.swept();
    Ok(())
}

/// Sweep the session and queue rows the pass built that are still marked
/// while the builder has not swept ([`Builder::rows_to_sweep`]): those the
/// store holds settled and up to date are settled now, past
/// [`CHECKS_PER_PASS`], so that the records [`Kept`] read of every event
/// from its cursors' start (a new process's) do not wait for the marks of
/// every run and task before them.
///
/// [`Kept`]: crate::domain::ledger::builder::Kept
fn sweep(store: &dyn LedgerQueue, builder: &mut Builder, inputs: &Inputs) -> Result<()> {
    for target in builder.rows_to_sweep(&inputs.rows) {
        let (kind, key) = target.row_key();
        let stored = store.ledger_row(kind, &key)?;
        builder.sweep(
            &target,
            stored.as_ref(),
            build(&target, inputs, &[]).as_ref(),
        );
    }
    Ok(())
}

/// Give each row of `rows` whose records the builder dropped once it was
/// settled (a later event reached it) its records back before it is
/// folded: those of the row the store holds of it, and any it lacks read
/// by id.
fn restore(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    rows: &BTreeSet<(LedgerKind, String)>,
) -> Result<()> {
    for (kind, key) in builder.to_restore(rows) {
        let records = store
            .ledger_row(kind, &key)?
            .map(|row| row.records)
            .unwrap_or_default();
        let read = read_by_id(store, &builder.restore(kind, &key, records))?;
        if !read.is_empty() {
            builder.restore(kind, &key, read);
        }
    }
    Ok(())
}

/// The events of `ids`, each read by its id.
fn read_by_id(store: &dyn LedgerQueue, ids: &[EventId]) -> Result<Vec<RunEvent>> {
    let mut read = Vec::new();
    for id in ids {
        let before = EventId::new(id.as_i64() - 1);
        read.extend(store.events_between(before, *id, &EventFilter::default(), 1)?);
    }
    Ok(read)
}

/// How many times a pass folds a target whose row another writer wrote
/// between its read and its write before it leaves the target marked for
/// the next pass.
pub const WRITE_TRIES: usize = 3;

/// Fold `target` and write it when [`writes`] says so ([`fold_and_write`]),
/// folding it again over the row held now while another writer (another
/// supervisor's builder) wrote the row since it was read; after
/// [`WRITE_TRIES`] the target stays marked for the next pass.
fn write(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    target: &Target,
    (inputs, classifier): (
        &mut Inputs,
        Option<&crate::domain::command_rules::CommandClassifier>,
    ),
    built: &mut Built,
) -> Result<()> {
    for _ in 0..WRITE_TRIES {
        if fold_and_write(store, builder, target, (inputs, classifier), built)? {
            return Ok(());
        }
    }
    builder.keep(target.clone());
    Ok(())
}

/// Fold `target`, a run's or a task's on its events up to the builder's
/// cursor (a run's on its steps up to the step cursor too, over what the
/// stored row folded of them, and on its resources up to the resource
/// cursors), and write it when [`writes`] says so and it
/// is not [`behind`] the stored row (then it stays marked), only over the
/// row it read
/// ([`crate::application::LedgerStore::write_ledger_row_over`]). False
/// when another writer wrote the row since: nothing written.
fn fold_and_write(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    target: &Target,
    (inputs, classifier): (
        &mut Inputs,
        Option<&crate::domain::command_rules::CommandClassifier>,
    ),
    built: &mut Built,
) -> Result<bool> {
    let (kind, key) = target.row_key();
    let stored = store.ledger_row(kind, &key)?;
    let mut samples = Vec::new();
    let events = match target {
        Target::Run(run) => {
            let id = RunId::new(run)?;
            let through = builder.steps;
            let steps = store
                .target_steps(&StepTarget::run(&id))?
                .into_iter()
                .filter(|step| step.id <= through)
                .collect();
            let mut steps =
                RunSteps::new(steps, stored.as_ref()).classified_by(classifier.cloned());
            for session in steps.sessions() {
                if let Some(owner) = store.session_owner(&session)? {
                    steps.starts.insert(session, owner.registered_at);
                }
            }
            inputs.read_steps(run, steps);
            let (outside_through, inside_through) = (builder.resources, builder.inside_resources);
            let outside = store
                .run_resources(&id)?
                .into_iter()
                .filter(|window| window.id <= outside_through)
                .collect();
            let inside = store
                .target_resources(&StepTarget::run(&id))?
                .into_iter()
                .filter(|sample| sample.id <= inside_through)
                .collect();
            let mut resources = RunResources::new(outside, inside, stored.as_ref());
            for session in resources.sessions() {
                if let Some(owner) = store.session_owner(&session)? {
                    resources.starts.insert(session, owner.registered_at);
                }
            }
            inputs.read_resources(run, resources);
            store.run_events(&id)?
        }
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
    let events: Vec<RunEvent> = events
        .into_iter()
        .filter(|event| event.id <= through)
        .collect();
    // A task's settled row no event of it reached since stays as it is.
    if let (Target::Task(_), Some(stored)) = (target, &stored)
        && keeps_settled(stored, &events)
    {
        builder.track(target, stored);
        return Ok(true);
    }
    // The pushes it reads that the builder dropped once its runs settled.
    let pushes = read_by_id(store, &builder.pushes_to_read(&events))?;
    inputs.read_pushes(target, pushes);
    inputs.read(target, events);
    let row = build(target, inputs, &samples);
    inputs.forget(target);
    let Some(row) = row else {
        return Ok(true);
    };
    if writes(stored.as_ref(), &row) {
        // Its cursors or its time are behind the stored row's: they pass
        // it in a later pass.
        if stored.as_ref().is_some_and(|stored| behind(stored, &row)) {
            builder.keep(target.clone());
            return Ok(true);
        }
        if store
            .write_ledger_row_over(&row, stored.as_ref())?
            .is_none()
        {
            return Ok(false);
        }
        built.written += 1;
        builder.track(target, &row);
    } else if let Some(stored) = stored {
        builder.track(target, &stored);
    }
    Ok(true)
}

/// The builder, the pass running now and how the last ones went.
#[derive(Default)]
pub(in crate::application::supervise) struct LedgerWatch {
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
    /// When the log last said how much the builder keeps, and its bytes
    /// then ([`logs_kept`]).
    kept_logged: Option<(Instant, usize)>,
}

impl LedgerWatch {
    /// Reap the pass once it ended; start one when the interval passed
    /// since the last one started.
    pub(in crate::application::supervise) fn pass(
        &mut self,
        env: &mut ObservationEnv<'_>,
        port: Option<&LedgerPort>,
    ) {
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
            let classifier = (port.classifier)();
            let built = match port.store.open() {
                Ok(store) => build_pass(
                    &*store,
                    &mut builder,
                    (now * 1000, &built_at),
                    retain,
                    classifier.as_ref(),
                ),
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
    pub(in crate::application::supervise) fn finish(
        &mut self,
        env: &mut ObservationEnv<'_>,
        deadline: Option<Instant>,
    ) {
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
                let at = env.generators.clock.monotonic();
                // A failed pass's size is not the builder's.
                if built.failure.is_none()
                    && logs_kept(
                        self.kept_logged.map(|(logged_at, bytes)| {
                            (at.saturating_duration_since(logged_at), bytes)
                        }),
                        built.kept.bytes,
                    )
                {
                    let kept = built.kept;
                    info!(
                        events = kept.events(),
                        sessions = kept.sessions,
                        queue = kept.queue,
                        pushes = kept.pushes,
                        lives = kept.lives,
                        pre_claim = kept.pre_claim,
                        session_rows = kept.session_rows,
                        queue_rows = kept.queue_rows,
                        dropped_rows = kept.dropped_rows,
                        push_runs = kept.push_runs,
                        bytes = kept.bytes,
                        "the ledger's builder keeps {} events, about {} bytes",
                        kept.events(),
                        kept.bytes
                    );
                    self.kept_logged = Some((at, kept.bytes));
                }
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
