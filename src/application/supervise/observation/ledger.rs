//! 観測と分析's builder of the ledger (ADR-t1662-1 (ii) and D7,
//! docs/design/measurement.md "台帳を作る係"): every
//! [`LedgerPort::interval`], draining and handing off too, a job thread
//! reads the inputs that came in after the builder's cursors (the
//! events, the node samples, the session steps and the runs' resources),
//! folds what they mark, a run's steps and resources into its row, and
//! writes the rows
//! [`crate::domain::ledger::builder`] says,
//! until the pass has taken [`PASS_TIME`](crate::domain::ledger::builder::PASS_TIME). While a pass leaves R behind
//! the events, stopped at a bound with work left, the next starts at once
//! ([`goes_on`]). Once a local day it removes the
//! node rows past the host's load's retention and the session steps past
//! [`crate::domain::session_step::RETENTION_DAYS`]
//! ([`Retain`]). It reads the events of each run and
//! task it folds, and the builder keeps what the folds of the queue as a
//! whole read ([`crate::domain::ledger::builder::Kept`]): no pass reads
//! the whole EventStore. Nothing of the runtime's control waits for it or
//! reads it, and a landing does not build a row. A settled row a later
//! event reaches, whose records the builder dropped, gets them back from
//! the row the store holds before it is folded. A new process's builder
//! starts where the store's marks say ([`start_builder`]): from the
//! position R with every book made again from the rows, or from its
//! cursors' start. Its first pass sweeps the rows a page at a time as it
//! reads the events ([`pass`]), so that its catch-up holds a page's events
//! and the rows open, not every event. The log says how much the
//! builder keeps ([`KeptSize`]) after the first pass, then hourly or when
//! its bytes change by half ([`logs_kept`]).
//!
//! A pass that cannot read or write is logged (once until a pass succeeds
//! again) and keeps its marks for the next one; after
//! [`FAILING_PASSES`] failed passes in a row it records one
//! `ledger_build_failed` for the run of failures, so a reader of the
//! events sees the ledger fall behind. A run's or a task's outcome never
//! depends on it.
//!
//! The builder's state is 観測と分析's ([`super::ObservationState`]), put
//! together in `compose::observation`, and is not added to the body of
//! the `Supervisor`.

use std::collections::BTreeSet;
use std::collections::btree_map::Entry;

use super::*;
use crate::application::LedgerQueue;
use crate::domain::EventFilter;
use crate::domain::host_metrics::{local_day, oldest_kept_day};
use crate::domain::ledger::builder::pushes::{self, PushesHeld};
use crate::domain::ledger::builder::{
    Builder, CHECKS_PER_PASS, EVENTS_PER_READ, Inputs, KeptSize, MarkObservation,
    RESOURCES_PER_READ, ReadTo, RunResources, RunSteps, SESSION_BYTES_PER_WRITE,
    SESSION_ROWS_PER_WRITE, STEPS_PER_READ, SweepCounts, Target, behind, build, hour,
    keeps_settled, marking_spent, pass_spent, row_groups, writes,
};
use crate::domain::ledger::queue::{QueueBook, QueueHead, cut};
use crate::domain::ledger::session::SessionsHeld;
use crate::domain::ledger::session::written::{
    Marks, RowPosition, marks_with_time_work, rows_to_resume, written_through,
};
use crate::domain::ledger::{LedgerKind, LedgerRow};
use crate::domain::pre_claim::{self, PreClaimBook};
use crate::domain::session_step::{self, StepTarget};
use crate::domain::supervisor_life::Lives;

/// How many passes in a row fail before one `ledger_build_failed` is
/// recorded.
pub const FAILING_PASSES: u32 = 10;

/// Bound an event catch-up pass so missing rows get their write turn and
/// checkpoint even when its session marks prevent page checkpoints.
const EVENTS_PER_PASS: usize = 20 * EVENTS_PER_READ;

/// Let a pass finish for at most thirty seconds after loop shutdown starts
/// waiting for it. Catch-up can outlive an update or service restart; it
/// must not hold that handoff indefinitely. Earlier EndBy deadlines win,
/// including a heartbeat deadline set while waiting. Committed checkpoints
/// survive leaving the thread; exec discards it, and store transactions
/// keep unfinished row/mark writes from exposing a partial certificate.
const FINISH_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq, Eq)]
enum FinishWait {
    Reap,
    Leave,
    Wait(Duration),
}

/// Decide from supplied monotonic elapsed time and the current EndBy
/// remainder. Reap an already finished pass even at the boundary; otherwise
/// sleep no longer than the earlier deadline, polling for completion.
fn finish_wait(finished: bool, elapsed: Duration, end_left: Option<Duration>) -> FinishWait {
    if finished {
        return FinishWait::Reap;
    }
    let left = FINISH_WAIT.saturating_sub(elapsed);
    let left = end_left.map_or(left, |end| left.min(end));
    if left.is_zero() {
        FinishWait::Leave
    } else {
        FinishWait::Wait(left.min(Duration::from_millis(20)))
    }
}

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
    /// How often a pass runs while the ledger is caught up ([`goes_on`]).
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
    /// The run, task, queue and node rows written, until the pass took
    /// [`PASS_TIME`](crate::domain::ledger::builder::PASS_TIME).
    pub written: usize,
    /// The session rows written, all at once ([`write_sessions`]).
    pub sessions: usize,
    /// The marks left for the next pass.
    pub backlog: usize,
    /// Whether it stopped at one of its bounds with work left: its time
    /// ([`PASS_TIME`](crate::domain::ledger::builder::PASS_TIME)), the marks it looks at ([`CHECKS_PER_PASS`]) or the
    /// events it reads ([`goes_on`]).
    pub cut: bool,
    /// The node rows the retention removed.
    pub removed: usize,
    /// The session steps the retention removed.
    pub steps_removed: usize,
    /// Why it stopped short, when it did.
    pub failure: Option<String>,
    /// How much the builder keeps after it.
    pub kept: KeptSize,
    /// Remaining marks at the final session write, with session counts and
    /// acknowledged marks refreshed afterwards. Checkpoints observe these
    /// too; a pass ending before its final write observes its current marks
    /// when it ends.
    pub marks: MarkObservation,
    marks_observed: bool,
    row_conflict: bool,
    /// Reserve the final queue group prepared within the pass time.
    /// If no session row changes, R must cross with these rows instead
    /// of in a later mark-only transaction.
    pending_queue: SessionChunk,
    pub sweep: SweepCounts,
    pub work: PassWork,
}

/// Work measured by stage. Page totals include all page sweeps; final
/// sweep reads count the stored rows fold_rows read, not restoration.
/// Durations are monotonic elapsed time and do not affect folding.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PassWork {
    pub events: usize,
    pub pages: usize,
    pub event_read: Duration,
    pub page_sweep: Duration,
    pub final_reads: usize,
    pub final_sweep: Duration,
    pub elapsed: Duration,
    pub latest: i64,
    pub marks_written: Option<Marks>,
    pub no_marks: &'static str,
    /// The session and queue rows folded to mark them before the writes
    /// ([`Builder::rows_to_mark`]) and to write them, one at a time as the
    /// writes come to them ([`Inputs::build_row`], [`fold_sessions`]): none of the rows a pass
    /// leaves marked is folded again for a write it does not make.
    pub folded: usize,
}

/// Timing is supplied by the runtime's Clock port. Direct fold callers
/// without a clock report zero durations and emit no periodic progress.
#[derive(Clone, Copy, Default)]
struct Timing<'a>(Option<&'a dyn crate::application::Clock>);

impl Timing<'_> {
    fn start(self) -> Option<Instant> {
        self.0.map(|clock| clock.monotonic())
    }

    fn since(self, start: Option<Instant>) -> Duration {
        self.0
            .zip(start)
            .map_or(Duration::ZERO, |(clock, start)| clock.since(start))
    }

    /// How long since `start` on the clock; none without a clock.
    fn elapsed(self, start: Option<Instant>) -> Option<Duration> {
        self.0.is_some().then(|| self.since(start))
    }
}

/// One minute exposes a stalled first catch-up before its pass finishes,
/// while bounding progress logs independently of the number of pages.
const FIRST_PASS_PROGRESS: Duration = Duration::from_secs(60);

fn progress_due(last: Duration, now: Duration) -> bool {
    now.saturating_sub(last) >= FIRST_PASS_PROGRESS
}

/// Elapsed monotonic clock values are supplied so the boundary can be
/// checked without waiting or reading a real clock in tests.
fn log_progress(last: &mut Duration, now: Duration, events: i64, pages: usize) {
    if progress_due(*last, now) {
        info!(events, pages, "the ledger first pass is reading");
        *last = now;
    }
}

/// Capture only marks actually acknowledged by this pass, including an
/// R-only write. An unchanged mark is distinct from an incomplete session
/// book (the latter cannot certify W or R).
fn write_sessions_observed(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    at: (i64, &str),
    bytes: usize,
    built: &mut Built,
) -> Result<usize> {
    built.marks = builder.observe_marks();
    built.marks_observed = true;
    let before = builder.observe_written_marks();
    let queue = std::mem::take(&mut built.pending_queue).rows;
    let mut queue_failure = None;
    let result = write_sessions(
        store,
        builder,
        at,
        bytes,
        built.marks.rows,
        (queue, &mut built.written, &mut queue_failure),
    );
    if let Some(error) = queue_failure {
        built.failure.get_or_insert_with(|| format!("{error:#}"));
        // A failed deferred queue write changed the blockers after the
        // initial observation; its new R is the one the session writer used.
        built.marks = builder.observe_marks();
    }
    builder.refresh_session_observation(&mut built.marks);
    let after = builder.observe_written_marks();
    if before != after {
        built.work.no_marks = if after.as_ref().is_some_and(|marks| marks.rows.is_none()) {
            "rows_not_whole"
        } else {
            ""
        };
        built.work.marks_written = after;
    } else {
        built.work.no_marks = if result.is_err() {
            "session_write_failed"
        } else {
            ""
        };
    }
    result
}

/// Persist clock work before writing any prefix row. The position remains
/// the store's previously certified one (zero if it has no usable R), so
/// a later failure writing sessions/marks cannot strand an open row at its
/// prefix time after a checkpoint. This reads no ledger row.
fn preserve_time_work(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    until: i64,
    built: &mut Built,
) -> Result<()> {
    builder.defer_time(until);
    let stored = store.ledger_marks()?;
    let heads = store.ledger_heads(LedgerKind::Session)?;
    let marks = marks_with_time_work(stored.as_ref(), &heads, until);
    // The coverage is not the time work's: compared without it.
    let unchanged = stored.as_ref().is_some_and(|stored| {
        Marks {
            omitted: None,
            ..stored.clone()
        } == marks
    });
    if !unchanged {
        store.write_ledger_marks(&marks)?;
        built.work.marks_written = Some(marks.clone());
    }
    builder.defer_time(
        marks
            .rows
            .and_then(|rows| rows.time_pending)
            .expect("clock work attached"),
    );
    builder.wrote_sessions_mark(marks);
    Ok(())
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
    at: (i64, &str),
    retain: Retain,
    classifier: Option<&crate::domain::command_rules::CommandClassifier>,
) -> Built {
    build_pass_reading(store, builder, at, retain, classifier, EVENTS_PER_READ)
}

/// [`build_pass`], reading the events `per_read` at a time.
fn build_pass_reading(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    at: (i64, &str),
    retain: Retain,
    classifier: Option<&crate::domain::command_rules::CommandClassifier>,
    per_read: usize,
) -> Built {
    build_pass_bounded(
        store,
        builder,
        at,
        retain,
        classifier,
        (per_read, SESSION_BYTES_PER_WRITE),
    )
}

/// [`build_pass_reading`], a write of the session rows holding at most
/// `session_bytes` of them ([`write_sessions`]).
fn build_pass_bounded(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    (now_ms, built_at): (i64, &str),
    retain: Retain,
    classifier: Option<&crate::domain::command_rules::CommandClassifier>,
    (per_read, session_bytes): (usize, usize),
) -> Built {
    build_pass_timed(
        store,
        builder,
        (now_ms, built_at),
        retain,
        classifier,
        (per_read, session_bytes),
        Timing::default(),
    )
}

fn build_pass_timed(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    (now_ms, built_at): (i64, &str),
    retain: Retain,
    classifier: Option<&crate::domain::command_rules::CommandClassifier>,
    (per_read, session_bytes): (usize, usize),
    timing: Timing<'_>,
) -> Built {
    build_pass_limited(
        store,
        builder,
        (now_ms, built_at),
        retain,
        classifier,
        (per_read, EVENTS_PER_PASS, session_bytes),
        timing,
    )
}

/// The same production pass with explicit read bounds, so tests can cross
/// a catch-up boundary without manufacturing a full production batch.
fn build_pass_limited(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    (now_ms, built_at): (i64, &str),
    retain: Retain,
    classifier: Option<&crate::domain::command_rules::CommandClassifier>,
    (per_read, per_pass, session_bytes): (usize, usize, usize),
    timing: Timing<'_>,
) -> Built {
    const CALLER: &str = "ledger.build_pass_timed";
    let _caller = crate::application::supervisor_service::caller::scope(CALLER);
    let started = timing.start();
    let mut built = Built::default();
    let read = (
        classifier,
        per_read,
        per_pass,
        session_bytes,
        (timing, started),
    );
    if let Err(error) = pass(store, builder, (now_ms, built_at), read, &mut built) {
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
        && let Err(error) = store.remove_omitted_queue_rows()
    {
        built.failure = Some(format!("the omitted queue rows' removal: {error:#}"));
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
    if !built.marks_observed {
        built.marks = builder.observe_marks();
    }
    built.work.elapsed = timing.since(started);
    if built.work.marks_written.is_none() && built.work.no_marks.is_empty() {
        built.work.no_marks = if built.failure.is_some() {
            "pass_failed"
        } else {
            builder.mark_skip_reason(&built.marks)
        };
    }
    info!(marks_written = ?built.work.marks_written,
        no_marks = built.work.no_marks,
        latest = built.work.latest,
        lag = built.marks.rows.map(|r| built.work.latest.saturating_sub(r.through.events.as_i64())),
        marks = ?built.marks, sweep = ?built.sweep, work = ?built.work,
        written = built.written, sessions = built.sessions, failure = ?built.failure,
        "the ledger pass finished");
    built
}

/// Read again the events whose rule of no row the pass's start changed
/// ([`Builder::regain_after`]: after the store's R alone), `per_read` at a
/// time as the pass's events ([`within_pass`]), before the pass reads on
/// or writes anything: those the builder left without rows that the rule
/// it holds now copies get their rows, so that no input is left of neither
/// a row nor the coverage. Whether it read them all: a pass that did not
/// writes nothing (R stays before the events not read again) and the next
/// goes on. A read short of where it had read fails the pass.
fn regain_rows(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    bounds: (usize, usize),
    timing: (Timing<'_>, Option<Instant>),
    built: &mut Built,
) -> Result<bool> {
    while let Some((after, through)) = builder.regain_after() {
        let Some(events) = within_pass(store, (after, through), bounds, timing, built)? else {
            return Ok(false);
        };
        anyhow::ensure!(
            !events.is_empty(),
            "the events after {after} up to {through} to give their rows are not read"
        );
        builder.regain(&events);
    }
    Ok(true)
}

/// Read again the events from the S of the coverage the builder took from
/// the store up to its cursor ([`Builder::coverage_behind`]), `per_read` at
/// a time as the pass's events ([`within_pass`]), before the pass reads on
/// or writes anything: so that the S it writes covers every event it read.
/// Whether it read them all: a pass that did not writes nothing (no mark
/// moves while the coverage is behind) and the next goes on from the S it
/// folded up to. A read short of the cursor fails the pass.
fn catch_up_coverage(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    bounds: (usize, usize),
    timing: (Timing<'_>, Option<Instant>),
    built: &mut Built,
) -> Result<bool> {
    while let Some(after) = builder.coverage_behind() {
        let Some(events) = within_pass(store, (after, builder.events), bounds, timing, built)?
        else {
            return Ok(false);
        };
        anyhow::ensure!(
            !events.is_empty(),
            "the events after the coverage's S {after} up to {} are not read",
            builder.events
        );
        builder.catch_up_coverage(&events);
    }
    Ok(true)
}

/// The next page of the events after the first of `range` up to the
/// second, `per_read` at most, read as the pass's events (`built`'s, its
/// events and pages): a page at least, then none once the pass read
/// `per_pass` events or took
/// [`MARK_TIME`](crate::domain::ledger::builder::MARK_TIME) since
/// `started`, reserving the rest of the pass for writes. None: the pass
/// is cut at its bound behind the latest event,
/// so that the next starts at once ([`goes_on`]).
fn within_pass(
    store: &dyn LedgerQueue,
    (after, through): (EventId, EventId),
    (per_read, per_pass): (usize, usize),
    (timing, started): (Timing<'_>, Option<Instant>),
    built: &mut Built,
) -> Result<Option<Vec<RunEvent>>> {
    if built.work.events > 0
        && (built.work.events >= per_pass || marking_spent(timing.since(started)))
    {
        built.cut = true;
        built.work.latest = store.latest_event_id()?.as_i64();
        return Ok(None);
    }
    let read_started = timing.start();
    let events = store.events_between(
        after,
        through,
        &EventFilter::default(),
        per_read
            .min(per_pass.saturating_sub(built.work.events))
            .max(1),
    );
    built.work.event_read += timing.since(read_started);
    let events = events?;
    built.work.events += events.len();
    built.work.pages += 1;
    Ok(Some(events))
}

/// One pass's reads, folds and writes. A builder that has not swept
/// ([`Builder::sweeps_pages`]: a new process's catching up) builds and
/// sweeps the session and queue rows each page
/// of events changed as it reads it ([`sweep_page`]), so that it keeps of
/// the rows the store holds settled no more than a page's events: their
/// records dropped, a later event that changes one gives them back from
/// the store's row first ([`restore`]). Every pass folds those rows a
/// share of [`CHECKS_PER_PASS`] at a time, and of the rows marked only
/// those it takes.
fn pass(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    (now_ms, built_at): (i64, &str),
    (classifier, per_read, per_pass, session_bytes, (timing, started)): (
        Option<&crate::domain::command_rules::CommandClassifier>,
        usize,
        usize,
        usize,
        (Timing<'_>, Option<Instant>),
    ),
    built: &mut Built,
) -> Result<()> {
    // Every builder, one that omits no kind too, so that a store of a
    // coverage of other kinds says it stops.
    let marks = store.ledger_marks()?;
    let coverage = if marks.is_none() {
        store.ledger_omitted_coverage()?
    } else {
        None
    };
    builder.begin_coverage_from(marks.as_ref(), coverage.as_ref());
    fill_open(store);
    let bounds = (per_read, per_pass);
    if !regain_rows(store, builder, bounds, (timing, started), built)?
        || !catch_up_coverage(store, builder, bounds, (timing, started), built)?
    {
        return Ok(());
    }
    // The events it read again are the pass's own: it reads on within the
    // events and the time they left it. Each page is read only while the
    // pass has marking time left, reserving the rest for writes, a page
    // at least, so that a pass makes way; one stopped short of the latest
    // is cut, and the next starts at once from where it stopped.
    let latest = store.latest_event_id()?;
    built.work.latest = latest.as_i64();
    let mut progress = Duration::ZERO;
    let reading_started = timing.start();
    let mut checkpoint_events = 0;
    while latest > builder.events && built.work.events < per_pass {
        if built.work.events > 0 && marking_spent(timing.since(started)) {
            break;
        }
        let started = timing.start();
        let read = store.events_between(
            builder.events,
            latest,
            &EventFilter::default(),
            per_read.min(per_pass - built.work.events),
        );
        built.work.event_read += timing.since(started);
        let new = read?;
        if new.is_empty() {
            break;
        }
        built.work.events += new.len();
        checkpoint_events += new.len();
        built.work.pages += 1;
        builder.mark_events(&new);
        drop(new);
        if builder.sweeps_pages() {
            let started = timing.start();
            let result = sweep_page(
                store,
                builder,
                (builder.folds_at_snapshot(now_ms, latest), built_at),
                &mut built.sweep,
            );
            built.work.page_sweep += timing.since(started);
            result?;
            // Commit covered inputs during catch-up, before the next read
            // can fail or the process can stop. The session writer certifies
            // W and R together. With pending sessions, defer to the pass's
            // bounded end instead of adding row reads for a checkpoint.
            if checkpoint_events >= EVENTS_PER_READ && builder.sessions_mark().is_some() {
                built.sessions += write_sessions_observed(
                    store,
                    builder,
                    (builder.folds_at_snapshot(now_ms, latest), built_at),
                    session_bytes,
                    built,
                )?;
                // A later page can change marks and fail before the final
                // write; its failure log must observe that state too.
                built.marks_observed = false;
                checkpoint_events = 0;
            }
            let elapsed = timing.since(reading_started);
            log_progress(
                &mut progress,
                elapsed,
                builder.events.as_i64(),
                built.work.pages,
            );
        }
        // Bound catch-up reads even when every page leaves missing rows.
        // The normal end of the pass writes their folds and checkpoints.
        if built.work.events >= per_pass {
            break;
        }
    }
    if builder.events < latest {
        built.cut = true;
        preserve_time_work(store, builder, builder.folds_at(now_ms), built)?;
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
        let runs = match &rules {
            Some(version) => store.ledger_runs_classified_otherwise(version)?,
            None => Vec::new(),
        };
        builder.mark_rules(rules.as_deref(), runs);
    }
    // Unread heartbeats are not silence: time follows the read prefix
    // until the snapshot is complete, including a bounded pass's writes.
    let now_ms = builder.folds_at_snapshot(now_ms, latest);
    builder.due(now_ms);
    if let Some((after, limit)) = builder.older_rows_to_read() {
        let older = store.ledger_older_rows_after(after, limit)?;
        builder.mark_older_rows(older, limit);
    }
    if !builder.marked() {
        // No row changed: the mark alone moves on.
        if builder.events >= latest {
            builder.finished_time();
        }
        let rebuilding = rebuild_omitted(store, builder);
        built.sessions +=
            write_sessions_observed(store, builder, (now_ms, built_at), session_bytes, built)?;
        return rebuilding;
    }
    let rows = builder.rows_to_build(builder.kept());
    restore(store, builder, &rows)?;
    let after = builder.take_rows_after();
    if let Some(after) = after {
        let marking = builder.rows_to_mark(&rows);
        mark_rows_timed(
            builder,
            (now_ms, built_at),
            marking,
            after,
            (timing, started),
            built,
        );
    }
    // The session rows wait aside for the write of them all after the
    // other rows. Hold the final queue group until just before the first
    // changed session row is written; if none changes, that group carries
    // W and R instead of the last session write. The marks hold back only
    // for rows this pass left marked. See row_groups for group bounds and
    // the oversized singleton path.
    let aside = builder.take_sessions();
    let taken = write_taken(
        store,
        builder,
        (now_ms, built_at),
        (classifier, timing, started),
        built,
        &rows,
        aside,
    );
    if taken.is_ok() && !builder.marked() && builder.events >= latest {
        builder.finished_time();
    }
    if built.row_conflict {
        return taken;
    }
    let rebuilding = rebuild_omitted(store, builder);
    let sessions =
        write_sessions_observed(store, builder, (now_ms, built_at), session_bytes, built);
    if let Ok(written) = sessions {
        built.sessions += written;
    }
    // The other rows' failure first, as they were written first.
    taken?;
    sessions?;
    rebuilding
}

/// One pass of the filling of what the run rows leave open at their end
/// ([`crate::application::LedgerStore::fill_ledger_open`]), a bounded part
/// of the rows a pass. A failure is logged and the pass goes on: the table
/// is a view no write of the ledger waits for, and until it is filled its
/// readers read every run row's records as before.
fn fill_open(store: &dyn LedgerQueue) {
    if let Err(error) = store.fill_ledger_open() {
        warn!(error = %format_args!("{error:#}"), "the ledger's open runs could not be filled: {error:#}");
    }
    if let Err(error) = store.fill_ledger_queue_views() {
        warn!(error = %format_args!("{error:#}"), "the ledger's queue views could not be filled: {error:#}");
    }
}

/// Once after the normal read and row writes: rebuilding cannot spend
/// the normal writer's time budget or change which queue rows it writes.
/// Its checkpoint is carried by the final W/R transaction, never a page
/// checkpoint. A read error leaves the private fold unchanged; the normal
/// W/R/S write is still attempted, and its errors take priority. The kind
/// reads use EventStore's kind/id index; a full final page needs only a
/// bounded tail probe to certify the empty tail.
fn rebuild_omitted(store: &dyn LedgerQueue, builder: &mut Builder) -> Result<()> {
    builder.rebuild_omitted(|kind, after, upto, limit| {
        let events = store.events_of_between(&[kind], after, upto, limit)?;
        let complete = events.len() < limit
            || !store.ledger_kind_has_after(kind, events.last().expect("full page").id, upto)?;
        Ok::<_, anyhow::Error>((events, complete))
    })
}

/// Mark those of `rows` that read an input after `after`
/// ([`Builder::mark_rows_after`]), each built as the pass that started at
/// `started` comes to it, until it has taken
/// [`MARK_TIME`](crate::domain::ledger::builder::MARK_TIME)
/// ([`marking_spent`]); those it has no time left to build are marked as
/// if each read one ([`Builder::mark_rows_unread`]), so that the writes
/// keep the rest of the pass and the next pass does not build them again.
fn mark_rows_timed(
    builder: &mut Builder,
    (now_ms, built_at): (i64, &str),
    rows: BTreeSet<(LedgerKind, String)>,
    after: ReadTo,
    (timing, started): (Timing<'_>, Option<Instant>),
    built: &mut Built,
) {
    let mut rows = rows.into_iter();
    while let Some(row) = rows.next() {
        if timing.elapsed(started).is_some_and(marking_spent) {
            builder.mark_rows_unread(after, std::iter::once(row).chain(rows));
            break;
        }
        let inputs = Inputs::of(builder.kept(), now_ms, built_at, &BTreeSet::from([row]));
        builder.mark_rows_after(Some(after), &inputs.rows);
        built.work.folded += 1;
    }
}

/// Write the marks a pass takes ([`Builder::take`], [`CHECKS_PER_PASS`]
/// of them, those holding R first) until the pass that started at
/// `started` has taken [`PASS_TIME`](crate::domain::ledger::builder::PASS_TIME) ([`pass_spent`]; a pass without a
/// clock writes every one), and put back the rest for the next pass; the
/// session rows aside
/// (written after, [`write_sessions`]), then sweep the session and queue rows of `rows`
/// left marked by a builder that has not swept ([`fold_rows`]). The
/// session rows taken `aside` are marked again before the sweep, so that
/// it keeps their records.
fn write_taken(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    (now_ms, built_at): (i64, &str),
    (classifier, timing, started): (
        Option<&crate::domain::command_rules::CommandClassifier>,
        Timing<'_>,
        Option<Instant>,
    ),
    built: &mut Built,
    rows: &BTreeSet<(LedgerKind, String)>,
    aside: Vec<Target>,
) -> Result<()> {
    let taken = builder.take(CHECKS_PER_PASS);
    // Marks past those it looks at may be left.
    built.cut |= taken.len() >= CHECKS_PER_PASS;
    // Each session or queue row is built as its write comes, after the
    // time is looked at: what the pass leaves is not folded for nothing.
    let mut inputs = Inputs::of(builder.kept(), now_ms, built_at, &BTreeSet::new());
    let mut first = builder.taken_first();
    // Defer the first error while continuing past failed priority targets
    // and isolated queue errors, so healthy work still gets its turn.
    let mut failed_first = None;
    let mut targets = taken.into_iter();
    while let Some(target) = targets.next() {
        let taken_first = first > 0;
        first = first.saturating_sub(1);
        // The next pass starts at what this one did not look at.
        if timing.elapsed(started).is_some_and(pass_spent) {
            built.cut = true;
            builder.put_back(target, targets);
            break;
        }
        let queue_target = matches!(target, Target::Row(LedgerKind::Queue, _));
        // The baseline test exercises the actual pre-grouping application
        // writer below, including WRITE_TRIES, rather than emulating it
        // by changing how a fake applies group requests.
        #[cfg(test)]
        let queue_target = queue_target && !store.uses_singleton_queue_writer();
        if queue_target {
            // Prepare one row per clock check. The byte-sized pending
            // group crosses whole when full, or with the final marks;
            // grouping must not eagerly fold the rest of a timed pass.
            let share = vec![target];
            if !write_queue_groups(
                store,
                builder,
                (now_ms, built_at),
                &share,
                built,
                &mut failed_first,
            ) {
                keep_all(builder, share);
                keep_all(builder, targets);
                keep_all(builder, aside);
                keep_all(builder, std::mem::take(&mut built.pending_queue).targets());
                built.row_conflict = true;
                return failed_first.map_or(Ok(()), Err);
            }
            continue;
        }
        if inputs.build_row(&target) {
            built.work.folded += 1;
        }
        let written = write(store, builder, &target, (&mut inputs, classifier), built);
        inputs.drop_row(&target);
        if let Err(error) = written {
            // Its turn comes again after the rest: one failing each pass
            // stops no other, nor is it taken first again.
            builder.keep_failed(target);
            if taken_first {
                failed_first.get_or_insert(error);
                continue;
            }
            if let Some(next) = targets.next() {
                builder.put_back(next, targets);
            }
            aside.into_iter().for_each(|target| builder.keep(target));
            return Err(error);
        }
    }
    drop(inputs);
    aside.into_iter().for_each(|target| builder.keep(target));
    if let Some(error) = failed_first {
        return Err(error);
    }
    let left: BTreeSet<(LedgerKind, String)> = builder
        .rows_to_sweep(rows)
        .iter()
        .map(Target::row_key)
        .collect();
    let started = timing.start();
    let reads_before = built.sweep.reads;
    let result = fold_rows(
        store,
        builder,
        (now_ms, built_at),
        &left,
        None,
        true,
        &mut built.sweep,
    );
    built.work.final_sweep += timing.since(started);
    built.work.final_reads += built.sweep.reads - reads_before;
    result?;
    // A bounded read can leave startup catch-up to another pass. Keep
    // sweeping pages until that catch-up reaches its snapshot of latest.
    if builder.events.as_i64() >= built.work.latest {
        builder.swept();
    }
    Ok(())
}

/// A folded row, its target, and the value read for compare-and-write.
type FoldedRow = (Target, LedgerRow, Option<LedgerRow>);

/// What [`fold_sessions`] made of the session or queue rows marked.
#[derive(Default)]
struct SessionsFolded {
    /// The rows to write, each with the row the store held as it was read.
    write: Vec<FoldedRow>,
    /// Rows whose inputs the store already records, tracked even when a
    /// queue row still needs a version or settlement rewrite.
    held: Vec<(Target, LedgerRow)>,
    /// The rows whose fold is [`behind`] the store's, left marked.
    behind: Vec<Target>,
}

/// Fold the session or queue rows of `targets` ([`Inputs::of`]), each over the row
/// the store holds of it, and sort them by what [`writes`] and [`behind`]
/// say.
fn fold_sessions(
    store: &dyn LedgerQueue,
    builder: &Builder,
    (now_ms, built_at): (i64, &str),
    targets: &[Target],
) -> Result<SessionsFolded> {
    const CALLER: &str = "ledger.fold_sessions";
    let _caller = crate::application::supervisor_service::caller::scope(CALLER);
    let mut folded = SessionsFolded::default();
    let keys: BTreeSet<(LedgerKind, String)> = targets.iter().map(Target::row_key).collect();
    let inputs = Inputs::of(builder.kept(), now_ms, built_at, &keys);
    let row_keys: Vec<_> = targets.iter().map(Target::row_key).collect();
    let mut held: BTreeMap<_, _> = store
        .ledger_rows_of(&row_keys)?
        .into_iter()
        .map(|row| ((row.kind, row.key.clone()), row))
        .collect();
    for target in targets {
        let stored = held.remove(&target.row_key());
        let Some(row) = build(target, &inputs, &[]) else {
            continue;
        };
        // A queue rewrite for version or settlement may be behind or
        // refused later. Inputs already recorded must not hold its R.
        let recorded = matches!(target, Target::Row(LedgerKind::Queue, _))
            && stored
                .as_ref()
                .is_some_and(|stored| Builder::inputs_recorded(stored, &row));
        if recorded {
            folded.held.push((
                target.clone(),
                stored.as_ref().expect("recorded row").clone(),
            ));
        }
        if !writes(stored.as_ref(), &row) {
            if !recorded && let Some(stored) = stored {
                folded.held.push((target.clone(), stored));
            }
        } else if stored.as_ref().is_some_and(|stored| behind(stored, &row)) {
            folded.behind.push(target.clone());
        } else {
            folded.write.push((target.clone(), row, stored));
        }
    }
    Ok(folded)
}

/// Fold every session row marked ([`Builder::take_sessions`]), whatever
/// [`PASS_TIME`](crate::domain::ledger::builder::PASS_TIME), [`SESSION_ROWS_PER_WRITE`] at a time, and write
/// those that change in chunks of at most as many rows and `bytes` of them
/// (each row's text, as it crosses to the queue service; a row past
/// `bytes` alone is a chunk of its own), each in one transaction
/// ([`crate::application::LedgerStore::write_session_rows_marked`]); the
/// pass's last chunk (none of rows when nothing is left to write) carries
/// the marks of the builder's cursors when it gives them
/// ([`Builder::sessions_mark`]: its session book is the fold of every
/// session event, and no session row is left marked; with them R, where
/// the other rows the pass left marked hold it back,
/// [`Builder::rows_written`]), or the marks alone when only R moved
/// ([`crate::application::LedgerStore::write_ledger_marks`]). It runs
/// after the pass wrote the other rows. So the mark is
/// written only once every session row the pass changed is written before
/// it or with it, and the store's session rows record every session event
/// up to the position it gives. A chunk written before a later one fails
/// only adds records to its rows: the mark the store holds from before
/// stays true. A fold [`behind`] the stored row, and a chunk another writer
/// wrote a row of since its read, stay marked, and so no mark is written;
/// a read or a write that fails leaves its chunk and those after it marked
/// and writes no mark. The held final queue group crosses before the
/// first changed session row, or with the marks if no session row changes.
/// The number of session rows written; queue writes update `queue_written`.
fn write_sessions(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    at: (i64, &str),
    bytes: usize,
    mut rows_mark: Option<crate::domain::ledger::session::written::RowsWritten>,
    (mut queue, queue_written, queue_failure): (
        Vec<FoldedRow>,
        &mut usize,
        &mut Option<anyhow::Error>,
    ),
) -> Result<usize> {
    let targets = builder.take_sessions();
    let mut written = 0;
    // The chunk filled last, written once the next row would take it past
    // its bounds, or with the mark at the end.
    let mut pending = SessionChunk::default();
    for (index, share) in targets.chunks(SESSION_ROWS_PER_WRITE).enumerate() {
        let after = (index + 1) * SESSION_ROWS_PER_WRITE;
        let folded = match fold_sessions(store, builder, at, share) {
            Ok(folded) => folded,
            Err(error) => {
                keep_all(builder, queue.into_iter().map(|(target, ..)| target));
                keep_all(builder, pending.targets());
                keep_all(builder, targets[index * SESSION_ROWS_PER_WRITE..].to_vec());
                return Err(error);
            }
        };
        for (target, stored) in &folded.held {
            builder.track(target, stored);
        }
        keep_all(builder, folded.behind);
        let mut rows = folded.write.into_iter();
        while let Some(row) = rows.next() {
            if !queue.is_empty() {
                if !write_queue_chunk(
                    store,
                    builder,
                    std::mem::take(&mut queue),
                    None,
                    queue_written,
                    queue_failure,
                ) {
                    keep_all(builder, [row.0]);
                    keep_all(builder, rows.map(|(target, ..)| target));
                    keep_all(builder, targets.get(after..).unwrap_or_default().to_vec());
                    return Ok(written);
                }
                // Error isolation may have put only the failed queue
                // targets back. Certify sessions with that conservative R.
                if queue_failure.is_some() {
                    rows_mark = builder.observe_marks().rows;
                }
            }
            let size = serde_json::to_vec(&row.1).map_or(0, |text| text.len());
            if !pending.takes(size, bytes) {
                match write_chunk(store, builder, std::mem::take(&mut pending).rows, None) {
                    Ok(Some(count)) => written += count,
                    Ok(None) => {
                        keep_all(builder, [row.0]);
                        keep_all(builder, rows.map(|(target, ..)| target));
                        keep_all(builder, targets.get(after..).unwrap_or_default().to_vec());
                        return Ok(written);
                    }
                    Err(error) => {
                        keep_all(builder, [row.0]);
                        keep_all(builder, rows.map(|(target, ..)| target));
                        keep_all(builder, targets.get(after..).unwrap_or_default().to_vec());
                        return Err(error);
                    }
                }
            }
            pending.push(row, size);
        }
    }
    let mark = builder.sessions_mark_from(rows_mark);
    if !queue.is_empty() {
        write_queue_chunk(store, builder, queue, mark, queue_written, queue_failure);
        return Ok(written);
    }
    if pending.rows.is_empty() {
        match mark {
            None => return Ok(written),
            // R alone moved: the session rows' mark as the store holds it
            // (a builder gone on from R before W reads past W before it).
            // A coverage the store refused writes neither: the next pass
            // takes the store's and writes them again.
            Some(marks) if builder.sessions_mark_written() == Some(marks.sessions) => {
                if store.write_ledger_marks(&marks)? {
                    builder.wrote_sessions_mark(marks);
                }
                return Ok(written);
            }
            Some(_) => {}
        }
    }
    Ok(written + write_chunk(store, builder, pending.rows, mark)?.unwrap_or(0))
}

/// The session rows of one write ([`write_sessions`]), each with its target
/// and the row the store held as it was read, and the bytes of their text.
#[derive(Debug, Default, PartialEq, Eq)]
struct SessionChunk {
    rows: Vec<FoldedRow>,
    bytes: usize,
}

impl SessionChunk {
    /// Whether a row of `size` bytes goes in without taking it past
    /// [`SESSION_ROWS_PER_WRITE`] rows or `bytes`: always into an empty one.
    fn takes(&self, size: usize, bytes: usize) -> bool {
        self.rows.is_empty()
            || (self.rows.len() < SESSION_ROWS_PER_WRITE
                && row_groups(&[self.bytes, size], bytes, None::<()>).len() == 1)
    }

    fn push(&mut self, row: FoldedRow, size: usize) {
        self.rows.push(row);
        self.bytes = self.bytes.saturating_add(size);
    }

    fn targets(self) -> impl Iterator<Item = Target> {
        self.rows.into_iter().map(|(target, ..)| target)
    }
}

/// Write `chunk` (each target with its row and the row the store held as
/// it was read) in one transaction, with `mark` when given, and track its
/// rows; another writer that wrote one of them since its read, or a
/// failure, writes none of them and leaves them marked. The rows written.
fn write_chunk(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    chunk: Vec<FoldedRow>,
    mark: Option<Marks>,
) -> Result<Option<usize>> {
    let (targets, rows): (Vec<Target>, Vec<(LedgerRow, Option<LedgerRow>)>) = chunk
        .into_iter()
        .map(|(target, row, stored)| (target, (row, stored)))
        .unzip();
    let written = store.write_session_rows_marked(&rows, mark.as_ref());
    if !matches!(written, Ok(true)) {
        // Folded again next pass.
        keep_all(builder, targets);
        return written.map(|_| None);
    }
    for (target, (row, _)) in targets.iter().zip(&rows) {
        if matches!(target, Target::Row(LedgerKind::Queue, _)) {
            // An earlier failed group may have re-marked this row.
            // Its successful singleton retry acknowledges those inputs.
            builder.sweep(target, Some(row), Some(row));
        } else {
            builder.track(target, row);
        }
    }
    if let Some(mark) = mark {
        builder.wrote_sessions_mark(mark);
    }
    Ok(Some(rows.len()))
}

/// Write queue rows through the same atomic whole/staged operation as
/// sessions. A refused group leaves all later groups for a fresh fold.
/// Its final group waits for the session folds: R crosses with the last
/// session group, or with this queue group when no session row changes.
fn write_queue_groups(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    at: (i64, &str),
    targets: &[Target],
    built: &mut Built,
    failure: &mut Option<anyhow::Error>,
) -> bool {
    if let Ok(written) = try_write_queue_groups(store, builder, at, targets, built, failure) {
        return written;
    }
    // Save the healthy pending group before isolating read/fold errors;
    // a current target's read error cannot mark its predecessor failed.
    let pending = std::mem::take(&mut built.pending_queue).rows;
    if !pending.is_empty()
        && !write_queue_chunk(store, builder, pending, None, &mut built.written, failure)
    {
        return false;
    }
    for (index, target) in targets.iter().enumerate() {
        match try_write_queue_groups(
            store,
            builder,
            at,
            std::slice::from_ref(target),
            built,
            failure,
        ) {
            Ok(true) => {}
            Ok(false) => {
                keep_all(builder, targets[index..].iter().cloned());
                return false;
            }
            Err(error) => {
                builder.keep_failed(target.clone());
                failure.get_or_insert(error);
            }
        }
    }
    true
}

/// Attempt a group without error isolation. False means a compare-and-write
/// refusal; errors are retried by [`write_queue_groups`] one row at a time.
fn try_write_queue_groups(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    at: (i64, &str),
    targets: &[Target],
    built: &mut Built,
    failure: &mut Option<anyhow::Error>,
) -> Result<bool> {
    built.work.folded += targets.len();
    let folded = fold_sessions(store, builder, at, targets)?;
    for (target, stored) in folded.held {
        builder.track(&target, &stored);
    }
    keep_all(builder, folded.behind);
    for row in folded.write {
        let size = serde_json::to_vec(&row.1)?.len();
        if !built.pending_queue.rows.is_empty()
            && row_groups(
                &[built.pending_queue.bytes, size],
                SESSION_BYTES_PER_WRITE,
                None::<()>,
            )
            .len()
                > 1
        {
            let pending = std::mem::take(&mut built.pending_queue).rows;
            if !write_queue_chunk(store, builder, pending, None, &mut built.written, failure) {
                return Ok(false);
            }
        }
        built.pending_queue.push(row, size);
    }
    Ok(true)
}

/// Write a queue group, including the deferred final group. A compare
/// refusal stops later work; a store error isolates singletons, keeps
/// only failed targets failed, and lets sessions and the other R proceed.
/// If this was the last group, recompute its certificate after isolation
/// and pair it with the last successful singleton (or no row if all failed).
fn write_queue_chunk(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    chunk: Vec<FoldedRow>,
    mark: Option<Marks>,
    written: &mut usize,
    failure: &mut Option<anyhow::Error>,
) -> bool {
    match write_chunk(store, builder, chunk.clone(), mark.clone()) {
        Ok(Some(count)) => {
            *written += count;
            return true;
        }
        Ok(None) => return false,
        Err(_) => {}
    }
    let mut last_written = None;
    for (index, (target, row, _)) in chunk.iter().enumerate() {
        match write_chunk(store, builder, vec![chunk[index].clone()], None) {
            Ok(Some(count)) => {
                *written += count;
                last_written = Some((target.clone(), row.clone(), Some(row.clone())));
            }
            Ok(None) => {
                keep_all(
                    builder,
                    chunk[index + 1..].iter().map(|(target, ..)| target.clone()),
                );
                return false;
            }
            Err(error) => {
                builder.keep_failed(target.clone());
                failure.get_or_insert(error);
            }
        }
    }
    if mark.is_some()
        && let Some(mark) = builder.sessions_mark()
    {
        // Every failure is now marked. Rewriting the last successful row
        // with its current read pairs the safe R with its last covered
        // group, without counting that row twice against the pass budget.
        let last: Vec<_> = last_written.into_iter().collect();
        let targets: Vec<_> = last.iter().map(|(target, ..)| target.clone()).collect();
        match write_chunk(store, builder, last, Some(mark)) {
            Ok(Some(_)) => {}
            Ok(None) => return false,
            Err(error) => {
                for target in targets {
                    builder.keep_failed(target);
                }
                failure.get_or_insert(error);
            }
        }
    }
    true
}

/// Mark `targets` again for the next pass ([`Builder::keep`]).
fn keep_all(builder: &mut Builder, targets: impl IntoIterator<Item = Target>) {
    targets.into_iter().for_each(|target| builder.keep(target));
}

/// Build the session and queue rows the page of events just read changed
/// ([`Builder::page_rows`]), their records given back first when the
/// builder dropped them ([`restore`]); mark those that read an input of the
/// page and sweep them ([`fold_rows`]); then drop the records of those the
/// store holds settled, before the next page is read
/// ([`Builder::page_swept`]). A row a later page changes is built again
/// then, so the marks it leaves are those of a sweep after every page.
fn sweep_page(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    at: (i64, &str),
    counts: &mut SweepCounts,
) -> Result<()> {
    const CALLER: &str = "ledger.sweep_page";
    let _caller = crate::application::supervisor_service::caller::scope(CALLER);
    let rows = builder.page_rows(builder.kept());
    restore(store, builder, &rows)?;
    let after = builder.take_rows_after();
    fold_rows(store, builder, at, &rows, after, true, counts)?;
    builder.page_swept();
    Ok(())
}

/// Fold the session and queue rows of `rows` [`CHECKS_PER_PASS`] at a
/// time ([`Inputs::of`]), so that no pass holds the folds of every row:
/// mark each share's rows that read an input after `after`
/// ([`Builder::mark_rows_after`]), every share before any read of the
/// store, so that a read that fails leaves every mark; then with `sweeps`
/// sweep those still marked, the stored rows of a share's in one read
/// ([`crate::application::LedgerStore::ledger_rows_of`]), so that a sweep
/// asks as many reads as it has shares, not rows: those the store holds settled and up to date
/// are settled now, past [`CHECKS_PER_PASS`], so that the records [`Kept`]
/// read of every event from its cursors' start do not
/// wait for the marks of every run and task before them.
///
/// [`Kept`]: crate::domain::ledger::builder::Kept
fn fold_rows(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    (now_ms, built_at): (i64, &str),
    rows: &BTreeSet<(LedgerKind, String)>,
    after: Option<ReadTo>,
    sweeps: bool,
    counts: &mut SweepCounts,
) -> Result<()> {
    const CALLER: &str = "ledger.fold_rows";
    let _caller = crate::application::supervisor_service::caller::scope(CALLER);
    let rows: Vec<&(LedgerKind, String)> = rows.iter().collect();
    let share_of = |share: &[&(LedgerKind, String)]| -> BTreeSet<(LedgerKind, String)> {
        share.iter().map(|row| (*row).clone()).collect()
    };
    if after.is_some() {
        for share in rows.chunks(CHECKS_PER_PASS) {
            let inputs = Inputs::of(builder.kept(), now_ms, built_at, &share_of(share));
            builder.mark_rows_after(after, &inputs.rows);
        }
    }
    if !sweeps {
        return Ok(());
    }
    for share in rows.chunks(CHECKS_PER_PASS) {
        let inputs = Inputs::of(builder.kept(), now_ms, built_at, &share_of(share));
        let targets = builder.rows_to_sweep(inputs.rows.keys());
        if targets.is_empty() {
            continue;
        }
        let keys: Vec<(LedgerKind, String)> = targets.iter().map(Target::row_key).collect();
        let mut stored: BTreeMap<(LedgerKind, String), LedgerRow> = store
            .ledger_rows_of(&keys)?
            .into_iter()
            .map(|row| ((row.kind, row.key.clone()), row))
            .collect();
        for (target, key) in targets.iter().zip(keys) {
            counts.reads += 1;
            let stored = stored.remove(&key);
            counts.record(builder.sweep(
                target,
                stored.as_ref(),
                build(target, &inputs, &[]).as_ref(),
            ));
        }
    }
    Ok(())
}

/// Give each row of `rows` whose records the builder dropped once it was
/// settled (a later event reached it) its records back before it is
/// folded: those of the row the store holds of it, and any it lacks read
/// by id. A queue row of an older version among them is then folded on its
/// own inputs again ([`Builder::fold_older_again`]).
fn restore(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    rows: &BTreeSet<(LedgerKind, String)>,
) -> Result<()> {
    const CALLER: &str = "ledger.restore";
    let _caller = crate::application::supervisor_service::caller::scope(CALLER);
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
    for key in builder.older_to_fold_again(rows) {
        let stored = store.ledger_row(LedgerKind::Queue, &key)?;
        let mut read = read_by_id(store, &builder.older_inputs(&key, stored.as_ref()))?;
        if let Some(run) = builder.older_slot_run(&key, &read) {
            read.extend(store.ledger_run_events(&run)?);
        }
        builder.fold_older_again(&key, read);
    }
    Ok(())
}

/// Fold on its own inputs again each older wait left
/// ([`Builder::older_waits`]), one row read at a time and none of its records
/// kept, before any task's fold reads it.
fn fold_older_waits(store: &dyn LedgerQueue, builder: &mut Builder) -> Result<()> {
    const CALLER: &str = "ledger.fold_older_waits";
    let _caller = crate::application::supervisor_service::caller::scope(CALLER);
    for key in builder.older_waits() {
        let stored = store.ledger_row(LedgerKind::Queue, &key)?;
        let mut read = read_by_id(store, &builder.older_inputs(&key, stored.as_ref()))?;
        read.extend(stored.map(|row| row.records).unwrap_or_default());
        builder.fold_older_again(&key, read);
    }
    Ok(())
}

/// The session book made again from the session rows `store` holds
/// ([`SessionBook::from_heads`]): their heads and anchors only, no event of
/// the EventStore and no other record of theirs, so that a builder going on
/// from the store's rows ([`Builder::restore_session_rows`]) does not read
/// the events of the sessions settled to know their rows and rounds.
#[cfg(test)]
pub fn session_book(
    store: &dyn LedgerQueue,
) -> Result<crate::domain::ledger::session::SessionBook> {
    Ok(crate::domain::ledger::session::SessionBook::from_heads(
        &store.ledger_heads(LedgerKind::Session)?,
        &store.ledger_session_anchors()?,
    ))
}

/// The queue book made again from the queue rows `store` holds
/// ([`QueueBook::from_heads`]): their heads with their subjects, times,
/// outcomes and details, no event of the EventStore and no record of
/// theirs, read against the waits before a claim `builder` keeps and up to
/// its cursor, so that a builder going on from the store's rows
/// ([`Builder::restore_queue_rows`]) does not read the events of the queue
/// rows settled to know their findings, alerts, streams, waits and landing
/// slots.
#[cfg(test)]
pub fn queue_book(store: &dyn LedgerQueue, builder: &Builder) -> Result<QueueBook> {
    Ok(QueueBook::from_heads(
        &store.ledger_queue_heads()?,
        builder.pre_claim(),
        builder.events,
    ))
}

/// The supervisors' lives and the waits before a claim made again from the
/// queue rows `store` holds ([`pre_claim::rows::books`]), folded by a pass
/// at `closed_to`: the waits open or a life ended, those that bear on a
/// task not settled, the streams not replaced and the stops, and of the
/// EventStore only the last event of each life that goes on
/// ([`pre_claim::rows::living`]), so that a builder going on from the
/// store's rows ([`Builder::restore_pre_claim`]) reads no event of a wait
/// or a life settled.
#[cfg(test)]
pub fn pre_claim_books(store: &dyn LedgerQueue, closed_to: i64) -> Result<(Lives, PreClaimBook)> {
    let since = store.ledger_tasks_open_since()?;
    let rows = store.ledger_wait_rows(since)?;
    let stops = store.ledger_supervisor_stops()?;
    let last = read_by_id(store, &pre_claim::rows::living(&rows, &stops))?;
    let (lives, mut book) =
        pre_claim::rows::books(&rows, &stops, &last, closed_to, pre_claim::rows::AS_WRITTEN);
    book.held_from(since);
    Ok((lives, book))
}

/// The books a builder going on from the position `at` takes, made again
/// from the queue rows `store` holds (written at a position no earlier)
/// as they were at `at` ([`crate::domain::ledger::queue::cut`]), folded by
/// a pass at `closed_to`: the supervisors' lives and the waits before a
/// claim ([`pre_claim::rows::books`], of the rows
/// [`pre_claim::rows::holds_at`] keeps for the position's
/// [`pre_claim::rows::Since`],
/// [`crate::application::LedgerStore::ledger_tasks_open_since_at`]), and the queue book
/// ([`QueueBook::from_heads_at`]). Besides the rows' heads, the stops and
/// that [`pre_claim::rows::Since`], it reads what the cut names
/// ([`cut::Needs`]): the last event naming each supervisor whose life
/// reaches a row by `at`, the records of a finding or a stream with one
/// past `at`, and the events of the run of an attempt whose end the heads
/// do not tell; as many however many rows settled before. The task rows
/// the store holds were folded past `at`, so they give the exact
/// [`pre_claim::rows::Since`] of `at` only as a bound no later than it:
/// held from that bound, the books then hold besides waits the exact one
/// would not, each ended before the exact one (and some may have ended
/// after the bound). A landing slot's attempt ended by `at` reads up to
/// its row's through until the events after `at` are read again.
#[cfg(test)]
pub fn books_at(
    store: &dyn LedgerQueue,
    at: EventId,
    closed_to: i64,
) -> Result<(QueueBook, Lives, PreClaimBook)> {
    let since = store.ledger_tasks_open_since_at(at)?;
    books_of_heads(store, &store.ledger_queue_heads()?, since, at, closed_to)
}

/// [`books_at`] of `heads`, the queue rows' heads the store holds, of which
/// those it forgets anyway at `at` and `since` (the position's
/// [`pre_claim::rows::Since`]) may be left out
/// ([`crate::domain::ledger::queue::QueueHead::forgotten_at`]).
fn books_of_heads(
    store: &dyn LedgerQueue,
    heads: &[QueueHead],
    since: pre_claim::rows::Since,
    at: EventId,
    closed_to: i64,
) -> Result<(QueueBook, Lives, PreClaimBook)> {
    const CALLER: &str = "ledger.books_of_heads";
    let _caller = crate::application::supervisor_service::caller::scope(CALLER);
    let needs = cut::Needs::of(heads, at);
    let stops = store.ledger_supervisor_stops()?;
    let mut last = Vec::new();
    for supervisor in &needs.supervisors {
        last.extend(store.last_event_naming_supervisor(supervisor, at)?);
    }
    let mut reads = cut::Reads::default();
    for key in &needs.rows {
        if let Some(row) = store.ledger_row(LedgerKind::Queue, key)? {
            reads.records.insert(key.clone(), row.records);
        }
    }
    for run in &needs.runs {
        if let Ok(id) = RunId::new(run.clone()) {
            reads
                .runs
                .insert(run.clone(), store.ledger_run_events(&id)?);
        }
    }
    let rows: Vec<LedgerRow> = heads
        .iter()
        .map(QueueHead::row)
        .filter(|row| pre_claim::rows::holds_at(row, since, at))
        .collect();
    let (lives, mut book) = pre_claim::rows::books(&rows, &stops, &last, closed_to, at);
    book.held_from(since);
    let cut_lives = cut::lives_at(&last, &stops, at);
    let queue = QueueBook::from_heads_at(
        heads,
        &book,
        &cut::Cut {
            at,
            now_ms: closed_to,
            lives: &cut_lives,
            reads: &reads,
        },
    );
    Ok((queue, lives, book))
}

/// The position W the session rows `store` holds record every session
/// event up to, by its mark and the rows' heads ([`written_through`]).
/// `None` for a store with no mark (its rows written one by one): a
/// builder then does not go on from the rows, and folds the session
/// events from its cursors' start (cursor 0), whose first pass's writes
/// fill what the rows lack and write the mark.
#[cfg(test)]
pub fn sessions_written_through(store: &dyn LedgerQueue) -> Result<Option<EventId>> {
    Ok(written_through(
        store.sessions_written()?.as_ref(),
        &store.ledger_heads(LedgerKind::Session)?,
    ))
}

/// The position R each of whose cursors the run, task, queue and node rows
/// `store` holds record every input up to, by its marks and the session
/// rows' heads ([`rows_written_through`]): its events no later than W
/// ([`sessions_written_through`]). `None` for a store with no mark or with
/// the session rows' alone: a builder then folds from its cursors' start.
#[cfg(test)]
pub fn rows_written(
    store: &dyn LedgerQueue,
) -> Result<Option<crate::domain::ledger::session::written::Cursors>> {
    Ok(
        crate::domain::ledger::session::written::rows_written_through(
            store.ledger_marks()?.as_ref(),
            &store.ledger_heads(LedgerKind::Session)?,
        ),
    )
}

/// Make `builder` go on from the session rows `store` holds as they were
/// at its cursor ([`SessionsHeld::at`], [`Builder::restore_sessions`]):
/// the heads and anchors of every session row, and the records of only
/// those not settled at the cursor, so that what it keeps of the sessions
/// settled is their records' ids. The rows give the fold of every event
/// only when the store's session rows record every session event up to
/// the cursor: when the cursor is no later than the position W the
/// store's mark gives ([`written_through`]), W itself or the R it went on
/// from ([`Builder::go_on_from_rows`]), they are held as the fold of every
/// session event up to it ([`SessionsHeld::within`]) and the builder
/// writes the marks again once it has caught up; at a cursor past W, or
/// with no W, it writes none.
pub fn restore_sessions(store: &dyn LedgerQueue, builder: &mut Builder) -> Result<()> {
    const CALLER: &str = "ledger.restore_sessions";
    let _caller = crate::application::supervisor_service::caller::scope(CALLER);
    let heads = store.ledger_heads(LedgerKind::Session)?;
    let anchors = store.ledger_session_anchors()?;
    let w = written_through(store.sessions_written()?.as_ref(), &heads);
    let held = SessionsHeld::within(&heads, &anchors, builder.events, w);
    let mut records = Vec::new();
    for key in &held.open {
        if let Some(row) = store.ledger_row(LedgerKind::Session, key)? {
            records.extend(row.records);
        }
    }
    builder.restore_sessions(held, records);
    Ok(())
}

/// Make `builder` go on from the summaries of the pushes the run and task
/// rows `store` holds as they were at its cursor ([`PushesHeld::at`],
/// [`Builder::restore_pushes`]), with a pass at `now_ms`. It reads the
/// push records of the run rows
/// ([`crate::application::LedgerStore::ledger_push_records`]) and the
/// pushes of no run, of the task rows and of the queue rows of a push alone
/// only ([`crate::application::LedgerStore::ledger_runless_pushes`]),
/// and the events of the runs whose rows do not record every event of
/// theirs up to the cursor: those a task row names that have no row
/// ([`crate::application::LedgerStore::ledger_runs_without_row`]: on their
/// way where the rows were written, whatever their tasks), those a legacy
/// task claimed that have none, whom no row names
/// ([`crate::application::LedgerStore::ledger_legacy_runs_without_row`]),
/// and those with an event up to it after the inputs of their rows
/// ([`crate::application::LedgerStore::ledger_runs_past_their_rows`]: they
/// went on their way since their row was written, from a pending push,
/// closed or not). Their events stand for their rows wherever they are
/// folded; another run is read of its row's records only. The pushes of all
/// of them are folded first, so that whether a run or task had ended and
/// settled at the cursor is folded on every push up to it. Whether a run
/// read whole that reads a push was settled at the cursor is its fold up to
/// it ([`PushesHeld::events_settled`]). It reads the heads of the rows not
/// settled at the cursor and of the legacy ones, whose mark the store keeps
/// as first written, and of the other readers among them their rows cut to
/// it (a task's, with its runs' rows or the events of those read whole) to
/// fold whether they were settled at it, a row written before the push that
/// closed its pending push being behind though it records every event of
/// its run ([`PushesHeld::settled_folded`]), a legacy task's runs by its
/// claims ([`crate::application::LedgerStore::ledger_runs_claimed`]); of a run whose
/// settling it folds, the windows of its resources up to the builder's
/// cursor of them, not what its row holds of them; and by id only the
/// pushes the readers not settled read. Of the events it reads the runs'
/// above and the pushes by id, and their number grows with the runs and
/// tasks not settled where the rows were written, not with those settled;
/// of a settled run's row it reads the push records, and whether it has an
/// event after them, no other record.
///
/// It holds only when the store's rows are those a build of every event
/// wrote at a position at or after the builder's cursors (each of them,
/// the one of the runs' windows too; none written late): which position
/// the rows record every input up to they do not say, and finding it is
/// for the caller, not for this.
pub fn restore_pushes(store: &dyn LedgerQueue, builder: &mut Builder, now_ms: i64) -> Result<()> {
    const CALLER: &str = "ledger.restore_pushes";
    let _caller = crate::application::supervisor_service::caller::scope(CALLER);
    let (through, resources) = (builder.events, builder.resources);
    let now_ms = builder.folds_at(now_ms);
    let heads = store.ledger_heads_open_at(through, resources)?;
    let mut open: BTreeSet<Target> = heads.iter().filter_map(pushes::target_of).collect();
    let mut records = store.ledger_push_records()?;
    records.extend(store.ledger_runless_pushes()?);
    let mut runs = store.ledger_runs_without_row()?;
    runs.extend(store.ledger_legacy_runs_without_row()?);
    runs.extend(store.ledger_runs_past_their_rows(through)?);
    let mut whole: BTreeMap<RunId, Vec<RunEvent>> = BTreeMap::new();
    for run in runs {
        if let Entry::Vacant(entry) = whole.entry(run) {
            let events = store.ledger_run_events(entry.key())?;
            records.extend(events.iter().cloned());
            entry.insert(events);
        }
    }
    let mut held = PushesHeld::at(records, through);
    let readers = held.readers();
    for (run, events) in &whole {
        let target = Target::Run(run.to_string());
        if !readers.contains(&target) {
            continue;
        }
        let windows = windows_until(store, run, resources)?;
        if held.events_settled(run.as_str(), events, &windows, now_ms) {
            open.remove(&target);
        } else {
            open.insert(target);
        }
    }
    for head in &heads {
        let Some(target) = pushes::target_of(head).filter(|target| readers.contains(target)) else {
            continue;
        };
        let windows = match &target {
            Target::Run(run) => match RunId::new(run) {
                // Folded on its events above.
                Ok(run) if whole.contains_key(&run) => continue,
                Ok(run) => windows_until(store, &run, resources)?,
                Err(_) => Vec::new(),
            },
            _ => Vec::new(),
        };
        let Some(row) = store.ledger_row(head.kind, &head.key)? else {
            continue;
        };
        let events = run_and_task_records(store, &row, &whole)?;
        if held.settled_folded(&row, events, &windows, now_ms) {
            open.remove(&target);
        }
    }
    held.settle(&open);
    let pushes = read_by_id(store, &held.to_read())?;
    builder.restore_pushes(held, pushes);
    Ok(())
}

/// The windows of `run`'s resources measured from outside up to the cursor
/// `resources`: those a build up to that cursor had read.
fn windows_until(
    store: &dyn LedgerQueue,
    run: &RunId,
    resources: i64,
) -> Result<Vec<crate::domain::environment::RunResource>> {
    Ok(store
        .run_resources(run)?
        .into_iter()
        .filter(|window| window.id <= resources)
        .collect())
}

/// The records of the run or task row `row`, and of a task's its runs'
/// (their rows' records, or the events of those read whole, `whole`, in
/// their stead): those it names, or those its claims name for a legacy row,
/// which keeps the runs it named as first written.
fn run_and_task_records(
    store: &dyn LedgerQueue,
    row: &LedgerRow,
    whole: &BTreeMap<RunId, Vec<RunEvent>>,
) -> Result<Vec<RunEvent>> {
    const CALLER: &str = "ledger.run_and_task_records";
    let _caller = crate::application::supervisor_service::caller::scope(CALLER);
    let mut events = row.records.clone();
    if row.kind == LedgerKind::Task {
        let runs: Vec<String> = match row.task_id.filter(|_| row.is_legacy()) {
            Some(task) => store
                .ledger_runs_claimed(task)?
                .iter()
                .map(|run| run.as_str().to_owned())
                .collect(),
            None => pushes::task_runs(row),
        };
        for run in runs {
            match RunId::new(&run).ok().and_then(|id| whole.get(&id)) {
                Some(read) => events.extend(read.iter().cloned()),
                None => {
                    if let Some(run) = store.ledger_row(LedgerKind::Run, &run)? {
                        events.extend(run.records);
                    }
                }
            }
        }
    }
    Ok(events)
}

/// The events of `ids`, each read by its id.
fn read_by_id(store: &dyn LedgerQueue, ids: &[EventId]) -> Result<Vec<RunEvent>> {
    const CALLER: &str = "ledger.read_by_id";
    let _caller = crate::application::supervisor_service::caller::scope(CALLER);
    let mut read = Vec::new();
    for id in ids {
        let before = EventId::new(id.as_i64() - 1);
        read.extend(store.events_between(before, *id, &EventFilter::default(), 1)?);
    }
    Ok(read)
}

/// The builder of a new process, restoring each kind at its own certified
/// position and event time. Queue replay cannot move core restoration back,
/// and core replay cannot move the queue's head read back. The lagging
/// stream alone replays to the later event cursor before the books join
/// ([`Builder::join_queue`]); normal incremental scans then share a cursor.
/// Open rows and older versions are marked as usual; settled core rows
/// need no replay when only queue R lags. A missing certificate starts only
/// that stream at zero, and a legacy single R seeds both streams there.
/// Neither R exceeds W, so session restoration never trusts an unrecorded
/// session prefix. A read that fails gives no builder: the next pass retries.
pub fn start_builder(store: &dyn LedgerQueue, now_ms: i64) -> Result<Builder> {
    start_builder_timed(store, now_ms, Timing::default())
}

fn start_builder_timed(
    store: &dyn LedgerQueue,
    now_ms: i64,
    timing: Timing<'_>,
) -> Result<Builder> {
    let _caller =
        crate::application::supervisor_service::caller::scope("ledger.start_builder_timed");
    let started = timing.start();
    let marks = store.ledger_marks()?;
    let heads = store.ledger_heads(LedgerKind::Session)?;
    let zero = RowPosition {
        through: Default::default(),
        read_ms: i64::MIN,
    };
    let core = rows_to_resume(marks.as_ref(), &heads, false);
    let queue = rows_to_resume(marks.as_ref(), &heads, true);
    let latest = store.latest_event_id()?;
    let lag_from = core
        .map_or(EventId::new(0), |at| at.through.events)
        .min(queue.map_or(EventId::new(0), |at| at.through.events));
    let (builder, reason) = if core.is_none() && queue.is_none() {
        (
            Builder::default(),
            crate::domain::ledger::session::written::restart_reason(marks.as_ref(), &heads),
        )
    } else {
        (
            resume_builder(
                store,
                now_ms,
                (core.unwrap_or(zero), queue.unwrap_or(zero)),
                marks.as_ref(),
            )?,
            None,
        )
    };
    // Observe the complete startup, including the independent replay and
    // joined queue book, once. The lag still describes the stored marks.
    info!(marks = ?marks, latest = latest.as_i64(),
        lag = latest.as_i64().saturating_sub(lag_from.as_i64()), reason,
        restored_rows = builder.book_rows(),
        elapsed_ms = timing.since(started).as_millis(), "the ledger builder started");
    Ok(builder)
}

fn resume_builder(
    store: &dyn LedgerQueue,
    now_ms: i64,
    (core, queue): (RowPosition, RowPosition),
    marks: Option<&Marks>,
) -> Result<Builder> {
    if core == queue {
        return start_builder_at(store, now_ms, core, marks, None, false);
    }

    // The streams replay only their own missing prefix, to their join at
    // the later event cursor. Queue replay supplies routing when core is
    // ahead: no core target read or session reconstruction moves back to Q.
    let mut queue_builder = Builder::default();
    queue_builder.go_on_from_rows(queue.through);
    queue_builder.read_up_to(queue.read_ms);
    queue_builder.begin_stored_coverage(marks);
    let since = store.ledger_tasks_open_since_at(queue.through.events)?;
    let heads = store.ledger_queue_heads_at(queue.through.events, since)?;
    let latest = store.latest_event_id()?;
    let queue_time = queue_builder.folds_at_snapshot(now_ms, latest);
    let (book, lives, pre_claim) =
        books_of_heads(store, &heads, since, queue.through.events, queue_time)?;
    queue_builder.restore_pre_claim(lives, pre_claim);
    queue_builder.restore_queue_rows(book);
    for head in &heads {
        if !head.head.is_final {
            queue_builder.keep(Target::Row(LedgerKind::Queue, head.head.key.clone()));
        }
    }
    let reached = queue_builder.older_reached_later();
    restore(store, &mut queue_builder, &reached)?;
    fold_older_waits(store, &mut queue_builder)?;
    replay_startup(store, &mut queue_builder, core.through.events, now_ms, true)?;
    let routing = (queue.through.events < core.through.events).then_some(&queue_builder);
    let mut builder = start_builder_at(store, now_ms, core, marks, routing, true)?;
    replay_startup(store, &mut builder, queue.through.events, now_ms, false)?;
    builder.join_queue(queue_builder);
    Ok(builder)
}

/// A restart's lagging stream alone reads its missing inputs. Page folds
/// name dirty rows before joining; they neither write rows nor certificates.
fn replay_startup(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    until: EventId,
    now_ms: i64,
    queue: bool,
) -> Result<()> {
    let _caller = crate::application::supervisor_service::caller::scope(if queue {
        "ledger.replay_queue"
    } else {
        "ledger.replay_core"
    });
    while builder.events < until {
        let events = store.events_between(
            builder.events,
            until,
            &EventFilter::default(),
            EVENTS_PER_READ,
        )?;
        anyhow::ensure!(
            !events.is_empty(),
            "missing ledger restart inputs before {until}"
        );
        if queue {
            builder.replay_queue_events(&events);
        } else {
            builder.mark_events(&events);
        }
        let rows = builder
            .page_rows(builder.kept())
            .into_iter()
            .filter(|(kind, _)| {
                *kind
                    == if queue {
                        LedgerKind::Queue
                    } else {
                        LedgerKind::Session
                    }
            })
            .collect();
        restore(store, builder, &rows)?;
        let after = builder.take_rows_after();
        fold_rows(
            store,
            builder,
            (
                builder.folds_at_snapshot(now_ms, store.latest_event_id()?),
                "",
            ),
            &rows,
            after,
            true,
            &mut SweepCounts::default(),
        )?;
        builder.page_swept();
    }
    Ok(())
}

fn start_builder_at(
    store: &dyn LedgerQueue,
    now_ms: i64,
    position: RowPosition,
    marks: Option<&Marks>,
    routing: Option<&Builder>,
    wait_only: bool,
) -> Result<Builder> {
    let mut builder = Builder::default();
    let latest = store.latest_event_id()?;
    let (r, read_ms) = (position.through, position.read_ms);
    builder.go_on_from_rows(r);
    builder.resume_taking(
        marks
            .and_then(|marks| marks.rows)
            .map_or(0, |rows| rows.take_offset),
    );
    builder.restore_time(
        marks
            .and_then(|marks| marks.rows)
            .and_then(|rows| rows.time_pending),
    );
    builder.read_up_to(read_ms);
    // After the time read up to R: where it reads by the rule of no row from.
    builder.begin_stored_coverage(marks);
    let now_ms = builder.folds_at_snapshot(now_ms, latest);
    let heads = if let Some(routing) = routing {
        builder.restore_routing_from(routing);
        Vec::new()
    } else {
        let since = store.ledger_tasks_open_since_at(r.events)?;
        // Core replay needs only waits/lives at its R, not the queue's
        // finding, stream and slot head reconstruction at that older R.
        let heads = if wait_only {
            let read_since = pre_claim::rows::Since {
                // A wait settled now may still have been open at core R,
                // even with no task ready there. Include its later end.
                id: since.id.min(r.events),
                ms: since.ms.min(read_ms),
            };
            store
                .ledger_wait_rows(read_since)?
                .iter()
                .filter(|row| pre_claim::rows::holds_at(row, since, r.events))
                .map(QueueHead::of)
                .collect()
        } else {
            store.ledger_queue_heads_at(r.events, since)?
        };
        let (queue, lives, pre_claim) = books_of_heads(store, &heads, since, r.events, now_ms)?;
        builder.restore_pre_claim(lives, pre_claim);
        builder.restore_queue_rows(queue);
        heads
    };
    restore_sessions(store, &mut builder)?;
    restore_pushes(store, &mut builder, now_ms)?;
    for head in store.ledger_heads_open_at(r.events, r.resources)? {
        if let Some(target) = pushes::target_of(&head).filter(|_| !head.is_final) {
            builder.keep(target);
        }
    }
    for head in heads.iter().map(|head| &head.head) {
        if !head.is_final {
            builder.keep(Target::Row(LedgerKind::Queue, head.key.clone()));
        }
    }
    for head in store.ledger_heads(LedgerKind::Node)? {
        if let Some(target) = Target::of_row(head.kind, &head.key).filter(|_| !head.is_final) {
            builder.keep(target);
        }
    }
    let reached = builder.older_reached_later();
    restore(store, &mut builder, &reached)?;
    fold_older_waits(store, &mut builder)?;
    builder.walk_older_rows();
    Ok(builder)
}

/// How many times a pass folds a target whose row another writer wrote
/// between its read and its write before it leaves the target marked for
/// the next pass.
pub const WRITE_TRIES: usize = 3;

/// Fold `target` and write it when [`writes`] or an open row's advancing
/// clock input says so ([`Builder::time_advanced`], [`fold_and_write`]),
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
/// cursors), and write it when records or pending clock work changed and it
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
    const CALLER: &str = "ledger.fold_and_write";
    let _caller = crate::application::supervisor_service::caller::scope(CALLER);
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
    let waits = match target {
        Target::Task(task) => builder.waits_to_read(&events).map(|since| (*task, since)),
        _ => None,
    };
    inputs.read_pushes(target, pushes);
    inputs.read(target, events);
    // A task that starts before where the book made again from the rows
    // holds the waits from reads those settled over its time from the
    // store's rows: up to its end, which no wait moves, or the time folded to.
    if let Some((task, (held, from))) = waits {
        let until = build(target, inputs, &samples).map_or(inputs.now_ms, |row| {
            row.ended_at
                .unwrap_or(inputs.now_ms)
                .max(row.started_at.unwrap_or(i64::MIN))
        });
        inputs.read_waits(task, &store.ledger_settled_waits(held, from, until)?);
    }
    let row = build(target, inputs, &samples);
    inputs.forget(target);
    let Some(row) = row else {
        return Ok(true);
    };
    // A required version/settlement write may fail or be behind another
    // writer, but inputs already held by that writer do not hold R.
    if let Some(stored) = &stored
        && Builder::inputs_recorded(stored, &row)
    {
        builder.track(target, stored);
    }
    if writes(stored.as_ref(), &row)
        || stored
            .as_ref()
            .is_some_and(|stored| builder.time_advanced(stored, &row))
    {
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
    if builder.events.as_i64() < built.work.latest
        && !row.is_final
        && matches!(target, Target::Run(_) | Target::Task(_))
    {
        // The row has its records, so track released its input position.
        // Keep clock work until the snapshot catches up; startup restores
        // the same work from the unsettled heads if the process stops.
        builder.keep(target.clone());
    }
    Ok(true)
}

/// One pass of [`LedgerWatch`] on `store` as it opened: the builder it held,
/// started on the store when it holds none ([`start_builder`]). A store that
/// did not open gives back the builder as it was, and a start that failed
/// gives none, so that the next pass starts it again: neither falls to a
/// builder from its cursors' start. The builder to hold, and what it did.
#[cfg(test)]
fn pass_of_watch(
    store: Result<&dyn LedgerQueue, &anyhow::Error>,
    builder: Option<Builder>,
    at: (i64, &str),
    retain: Retain,
    classifier: Option<&crate::domain::command_rules::CommandClassifier>,
) -> (Option<Builder>, Built) {
    pass_of_watch_timed(store, builder, at, retain, classifier, Timing::default())
}

fn pass_of_watch_timed(
    store: Result<&dyn LedgerQueue, &anyhow::Error>,
    builder: Option<Builder>,
    at: (i64, &str),
    retain: Retain,
    classifier: Option<&crate::domain::command_rules::CommandClassifier>,
    timing: Timing<'_>,
) -> (Option<Builder>, Built) {
    const CALLER: &str = "ledger.pass_of_watch_timed";
    let _caller = crate::application::supervisor_service::caller::scope(CALLER);
    let failed = |what: &str, error: &anyhow::Error| Built {
        failure: Some(format!("{what}: {error:#}")),
        ..Built::default()
    };
    let store = match store {
        Ok(store) => store,
        Err(error) => return (builder, failed("open the queue", error)),
    };
    let mut builder = match builder {
        Some(builder) => builder,
        None => match start_builder_timed(store, at.0, timing) {
            Ok(builder) => builder,
            Err(error) => return (None, failed("start the builder", &error)),
        },
    };
    let built = build_pass_timed(
        store,
        &mut builder,
        at,
        retain,
        classifier,
        (EVENTS_PER_READ, SESSION_BYTES_PER_WRITE),
        timing,
    );
    (Some(builder), built)
}

/// Whether the pass after `built` starts at once rather than after the
/// interval ([`LedgerPort::interval`]): `built` stopped at one of its
/// bounds with work left ([`Built::cut`]) and left the ledger behind, R
/// (its cursor of the events, the earlier of the kinds' R: either kind
/// behind goes on; a builder of no R is taken as behind) before the
/// latest event it saw ([`PassWork::latest`]). R need not move:
/// the rows of one page of events hold it at the same place until the
/// last of them is written. The passes so go on back to back until R
/// reaches the events and then come every interval again. A pass that
/// failed, stopped at no bound (what holds R waits on a write that
/// failed or a fold behind the stored row), or wrote nothing and read no
/// event, waits for the interval, so that such a pass does not run passes
/// back to back.
fn goes_on(built: &Built) -> bool {
    // A pass that wrote nothing and read no event stopped before any
    // progress: the next waits for the interval rather than spin.
    let moved = built.written > 0 || built.sessions > 0 || built.work.events > 0;
    built.failure.is_none()
        && built.cut
        && moved
        && built
            .marks
            .rows
            .is_none_or(|rows| rows.through.events.as_i64() < built.work.latest)
}

/// Whether the watch starts a pass at `at`, the last one having started
/// at `started` (none: no pass yet): at once when the last one reaped
/// [`goes_on`], else once `interval` passed since it started.
fn pass_due(started: Option<Instant>, at: Instant, interval: Duration, goes_on: bool) -> bool {
    goes_on || started.is_none_or(|started| at.saturating_duration_since(started) >= interval)
}

/// The builder (none before the first pass that started one), the pass
/// running now and how the last ones went.
#[derive(Default)]
pub(in crate::application::supervise) struct LedgerWatch {
    /// The builder while no pass runs (a pass takes it and gives it back).
    builder: Option<Builder>,
    job: Option<thread::JoinHandle<(Option<Builder>, Built)>>,
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
    /// Whether the next pass starts at once ([`goes_on`]).
    goes_on: bool,
}

impl LedgerWatch {
    /// Reap the pass once it ended, never waiting for one still running
    /// (it looks whether it finished); start one when the interval passed
    /// since the last one started, or at once while the last one left the
    /// ledger behind ([`pass_due`]).
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
        if !self.starts(env.generators.clock.monotonic(), port.interval) {
            return;
        }
        let now = env.generators.clock.now();
        let built_at = env.generators.clock.timestamp();
        let today = local_day(now, (port.utc_offset)(now));
        let retain = Retain::due(now, today, self.retained, port.retention_days);
        self.retaining = retain.any().then_some(today);
        let builder = self.builder.take();
        let clock = env.generators.clock.clone();
        self.job = Some(spawn_traced(move || {
            let classifier = (port.classifier)();
            let opened = port.store.open();
            let store = match &opened {
                Ok(store) => Ok(&**store as &dyn LedgerQueue),
                Err(error) => Err(error),
            };
            pass_of_watch_timed(
                store,
                builder,
                (now * 1000, &built_at),
                retain,
                classifier.as_ref(),
                Timing(Some(&*clock)),
            )
        }));
    }

    /// Whether a pass starts at `at` ([`pass_due`]), taken as started then
    /// when it does: the next one waits for the interval unless this one,
    /// reaped, goes on ([`Self::reaped`]).
    fn starts(&mut self, at: Instant, interval: Duration) -> bool {
        if !pass_due(self.started, at, interval, self.goes_on) {
            return false;
        }
        self.goes_on = false;
        self.started = Some(at);
        true
    }

    /// Whether the pass after `built` goes on at once ([`goes_on`]).
    fn reaped(&mut self, built: &Built) {
        self.goes_on = goes_on(built);
    }

    /// Wait for the pass still running, as the loop ends, until
    /// at most [`FINISH_WAIT`] or an earlier `deadline`. One still running
    /// then is left, and the next supervisor resumes from committed R.
    pub(in crate::application::supervise) fn finish(
        &mut self,
        env: &mut ObservationEnv<'_>,
        deadline: &super::EndBy,
    ) {
        if let Some(job) = self.job.take() {
            let clock = &*env.generators.clock;
            let started = clock.monotonic();
            loop {
                let now = clock.monotonic();
                match finish_wait(
                    job.is_finished(),
                    now.saturating_duration_since(started),
                    deadline.left(now),
                ) {
                    FinishWait::Reap => {
                        self.reap(env, job);
                        break;
                    }
                    FinishWait::Leave => {
                        tracing::warn!(
                            "the ledger's pass still runs as the supervisor ends; it is left"
                        );
                        break;
                    }
                    FinishWait::Wait(left) => thread::sleep(left),
                }
            }
        }
    }

    fn reap(
        &mut self,
        env: &mut ObservationEnv<'_>,
        job: thread::JoinHandle<(Option<Builder>, Built)>,
    ) {
        let failure = match job.join() {
            Ok((builder, built)) => {
                self.builder = builder;
                self.reaped(&built);
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
                if built.written > 0
                    || built.sessions > 0
                    || built.removed > 0
                    || built.steps_removed > 0
                {
                    tracing::debug!(
                        written = built.written,
                        sessions = built.sessions,
                        backlog = built.backlog,
                        removed = built.removed,
                        steps_removed = built.steps_removed,
                        "the ledger is built"
                    );
                }
                built.failure
            }
            // The builder is lost with the thread: the next starts anew
            // and catches up ([`start_builder`]).
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
                // Kept to be sent, it is recorded.
                Err(error) if crate::application::supervisor_service::resend::kept(&error) => {
                    self.recorded = true
                }
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "ledger_build_failed could not be recorded: {error:#}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
