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
    Builder, CHECKS_PER_PASS, EVENTS_PER_READ, Inputs, KeptSize, RESOURCES_PER_READ, ReadTo,
    RunResources, RunSteps, SESSION_BYTES_PER_WRITE, SESSION_ROWS_PER_WRITE, STEPS_PER_READ,
    Target, WRITES_PER_PASS, behind, build, hour, keeps_settled, writes,
};
use crate::domain::ledger::queue::{QueueBook, QueueHead, cut};
use crate::domain::ledger::session::SessionsHeld;
use crate::domain::ledger::session::written::{Marks, rows_to_go_on_from, written_through};
use crate::domain::ledger::{LedgerKind, LedgerRow};
use crate::domain::pre_claim::{self, PreClaimBook};
use crate::domain::session_step::{self, StepTarget};
use crate::domain::supervisor_life::Lives;

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
    /// The run, task, queue and node rows written, at most
    /// [`WRITES_PER_PASS`].
    pub written: usize,
    /// The session rows written, all at once ([`write_sessions`]).
    pub sessions: usize,
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
    let mut built = Built::default();
    let read = (classifier, per_read, session_bytes);
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
    (classifier, per_read, session_bytes): (
        Option<&crate::domain::command_rules::CommandClassifier>,
        usize,
        usize,
    ),
    built: &mut Built,
) -> Result<()> {
    let latest = store.latest_event_id()?;
    while latest > builder.events {
        let new =
            store.events_between(builder.events, latest, &EventFilter::default(), per_read)?;
        if new.is_empty() {
            break;
        }
        builder.mark_events(&new);
        drop(new);
        if builder.sweeps_pages() {
            sweep_page(store, builder, (builder.folds_at(now_ms), built_at))?;
        }
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
    // What it read came by then.
    let now_ms = builder.folds_at(now_ms);
    builder.due(now_ms);
    if !builder.marked() {
        // No row changed: the mark alone moves on.
        built.sessions = write_sessions(store, builder, (now_ms, built_at), session_bytes)?;
        return Ok(());
    }
    let rows = builder.rows_to_build(builder.kept());
    restore(store, builder, &rows)?;
    let after = builder.take_rows_after();
    fold_rows(store, builder, (now_ms, built_at), &rows, after, false)?;
    // The session rows wait aside for the write of them all after the
    // other rows, so that none is written alone, a failure of theirs stops
    // no other row, and the marks written with the last of them hold back
    // only for the rows this pass left marked.
    let aside = builder.take_sessions();
    let taken = write_taken(
        store,
        builder,
        (now_ms, built_at),
        classifier,
        built,
        &rows,
        aside,
    );
    let sessions = write_sessions(store, builder, (now_ms, built_at), session_bytes);
    if let Ok(written) = sessions {
        built.sessions = written;
    }
    // The other rows' failure first, as they were written first.
    taken?;
    sessions.map(drop)
}

/// Write the marks a pass takes ([`Builder::take`], [`CHECKS_PER_PASS`]
/// of them) at most [`WRITES_PER_PASS`] rows, the session rows aside
/// (written after, [`write_sessions`]), then sweep the session and queue rows of `rows`
/// left marked by a builder that has not swept ([`fold_rows`]). The
/// session rows taken `aside` are marked again before the sweep, so that
/// it keeps their records.
fn write_taken(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    (now_ms, built_at): (i64, &str),
    classifier: Option<&crate::domain::command_rules::CommandClassifier>,
    built: &mut Built,
    rows: &BTreeSet<(LedgerKind, String)>,
    aside: Vec<Target>,
) -> Result<()> {
    let taken = builder.take(CHECKS_PER_PASS);
    let rows_taken: BTreeSet<(LedgerKind, String)> = taken
        .iter()
        .filter(|target| matches!(target, Target::Row(..)))
        .map(Target::row_key)
        .collect();
    let mut inputs = Inputs::of(builder.kept(), now_ms, built_at, &rows_taken);
    let mut targets = taken.into_iter();
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
            aside.into_iter().for_each(|target| builder.keep(target));
            return Err(error);
        }
    }
    drop(inputs);
    aside.into_iter().for_each(|target| builder.keep(target));
    let left: BTreeSet<(LedgerKind, String)> = builder
        .rows_to_sweep(rows)
        .iter()
        .map(Target::row_key)
        .collect();
    fold_rows(store, builder, (now_ms, built_at), &left, None, true)?;
    builder.swept();
    Ok(())
}

/// What [`fold_sessions`] made of the session rows marked.
#[derive(Default)]
struct SessionsFolded {
    /// The rows to write, each with the row the store held as it was read.
    write: Vec<(Target, LedgerRow, Option<LedgerRow>)>,
    /// The rows the store holds as folded, which are tracked as they are.
    held: Vec<(Target, LedgerRow)>,
    /// The rows whose fold is [`behind`] the store's, left marked.
    behind: Vec<Target>,
}

/// Fold the session rows of `targets` ([`Inputs::of`]), each over the row
/// the store holds of it, and sort them by what [`writes`] and [`behind`]
/// say.
fn fold_sessions(
    store: &dyn LedgerQueue,
    builder: &Builder,
    (now_ms, built_at): (i64, &str),
    targets: &[Target],
) -> Result<SessionsFolded> {
    let mut folded = SessionsFolded::default();
    let keys: BTreeSet<(LedgerKind, String)> = targets.iter().map(Target::row_key).collect();
    let inputs = Inputs::of(builder.kept(), now_ms, built_at, &keys);
    for target in targets {
        let (kind, key) = target.row_key();
        let stored = store.ledger_row(kind, &key)?;
        let Some(row) = build(target, &inputs, &[]) else {
            continue;
        };
        if !writes(stored.as_ref(), &row) {
            if let Some(stored) = stored {
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
/// [`WRITES_PER_PASS`], [`SESSION_ROWS_PER_WRITE`] at a time, and write
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
/// and writes no mark. The number of rows written.
fn write_sessions(
    store: &dyn LedgerQueue,
    builder: &mut Builder,
    at: (i64, &str),
    bytes: usize,
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
            let size = serde_json::to_vec(&row.1).map_or(0, |text| text.len());
            if !pending.takes(size, bytes) {
                match write_chunk(store, builder, std::mem::take(&mut pending).rows, None) {
                    Ok(count) => written += count,
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
    let mark = builder.sessions_mark();
    if pending.rows.is_empty() {
        match mark {
            None => return Ok(written),
            // R alone moved: the session rows' mark as the store holds it
            // (a builder gone on from R before W reads past W before it).
            Some(marks) if builder.sessions_mark_written() == Some(marks.sessions) => {
                store.write_ledger_marks(&marks)?;
                builder.wrote_sessions_mark(marks);
                return Ok(written);
            }
            Some(_) => {}
        }
    }
    Ok(written + write_chunk(store, builder, pending.rows, mark)?)
}

/// The session rows of one write ([`write_sessions`]), each with its target
/// and the row the store held as it was read, and the bytes of their text.
#[derive(Default)]
struct SessionChunk {
    rows: Vec<(Target, LedgerRow, Option<LedgerRow>)>,
    bytes: usize,
}

impl SessionChunk {
    /// Whether a row of `size` bytes goes in without taking it past
    /// [`SESSION_ROWS_PER_WRITE`] rows or `bytes`: always into an empty one.
    fn takes(&self, size: usize, bytes: usize) -> bool {
        self.rows.is_empty()
            || (self.rows.len() < SESSION_ROWS_PER_WRITE
                && self.bytes.saturating_add(size) <= bytes)
    }

    fn push(&mut self, row: (Target, LedgerRow, Option<LedgerRow>), size: usize) {
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
    chunk: Vec<(Target, LedgerRow, Option<LedgerRow>)>,
    mark: Option<Marks>,
) -> Result<usize> {
    let (targets, rows): (Vec<Target>, Vec<(LedgerRow, Option<LedgerRow>)>) = chunk
        .into_iter()
        .map(|(target, row, stored)| (target, (row, stored)))
        .unzip();
    let written = store.write_session_rows_marked(&rows, mark.as_ref());
    if !matches!(written, Ok(true)) {
        // Folded again next pass.
        keep_all(builder, targets);
        return written.map(|_| 0);
    }
    for (target, (row, _)) in targets.iter().zip(&rows) {
        builder.track(target, row);
    }
    if let Some(mark) = mark {
        builder.wrote_sessions_mark(mark);
    }
    Ok(rows.len())
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
fn sweep_page(store: &dyn LedgerQueue, builder: &mut Builder, at: (i64, &str)) -> Result<()> {
    let rows = builder.page_rows(builder.kept());
    restore(store, builder, &rows)?;
    let after = builder.take_rows_after();
    fold_rows(store, builder, at, &rows, after, true)?;
    builder.page_swept();
    Ok(())
}

/// Fold the session and queue rows of `rows` [`CHECKS_PER_PASS`] at a
/// time ([`Inputs::of`]), so that no pass holds the folds of every row:
/// mark each share's rows that read an input after `after`
/// ([`Builder::mark_rows_after`]), every share before any read of the
/// store, so that a read that fails leaves every mark; then with `sweeps`
/// sweep those still marked: those the store holds settled and up to date
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
) -> Result<()> {
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
        for target in builder.rows_to_sweep(inputs.rows.keys()) {
            let (kind, key) = target.row_key();
            let stored = store.ledger_row(kind, &key)?;
            builder.sweep(
                &target,
                stored.as_ref(),
                build(&target, &inputs, &[]).as_ref(),
            );
        }
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
    books_of_heads(store, &store.ledger_queue_heads()?, at, closed_to)
}

/// [`books_at`] of `heads`, the queue rows' heads the store holds, of which
/// those it forgets anyway at `at` may be left out
/// ([`crate::domain::ledger::queue::QueueHead::forgotten_at`]).
fn books_of_heads(
    store: &dyn LedgerQueue,
    heads: &[QueueHead],
    at: EventId,
    closed_to: i64,
) -> Result<(QueueBook, Lives, PreClaimBook)> {
    let needs = cut::Needs::of(heads, at);
    let since = store.ledger_tasks_open_since_at(at)?;
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
    let mut read = Vec::new();
    for id in ids {
        let before = EventId::new(id.as_i64() - 1);
        read.extend(store.events_between(before, *id, &EventFilter::default(), 1)?);
    }
    Ok(read)
}

/// The builder of a new process on `store`, at `now_ms`. Where the store's
/// marks give the position R ([`rows_to_go_on_from`]) and it holds no row folded
/// by an older version ([`crate::application::LedgerStore::ledger_older_rows`]),
/// it goes on from R: its cursors at R, each its own
/// ([`Builder::go_on_from_rows`]), and every book of [`Kept`] made again
/// from the rows as they were at R's events, the one position they all
/// share ([`books_of_heads`], [`restore_sessions`], [`restore_pushes`]).
/// R is no later than W, so the session rows hold every input up to it; a
/// book made at any later position would read the events after R twice.
/// The rows the store holds not settled are marked (session rows by
/// [`restore_sessions`]): their grace, the waits they read and a time past
/// a silence fold them again with no input, as a builder from its cursors'
/// start finds them. Rows settled with no input after R are not looked at:
/// they are the fold of every input as they are. So it reads the inputs
/// after R only; the rows a mark left unwritten are among what those inputs
/// mark ([`Builder::rows_written`]). The latest time of an event read up
/// to R it takes from the mark ([`rows_to_go_on_from`],
/// [`Builder::read_up_to`]), reading no event. Besides the heads, what it reads by
/// target bears on the rows not settled and the lives open only
/// ([`cut::Needs`], [`restore_pushes`]).
///
/// With no R (a store with no mark, or W alone), with an R that does not
/// hold that time at its position, or with a row older than this version,
/// it starts at its cursors' start, as [`Builder::default`]: it reads every
/// input, and its writes fill what the rows lack and write the marks, R
/// with its time. An R next to older rows moved on rows of this version alone,
/// and those of no input after it would stay old.
///
/// A read that fails gives no builder: the next pass starts again.
///
/// [`Kept`]: crate::domain::ledger::builder::Kept
pub fn start_builder(store: &dyn LedgerQueue, now_ms: i64) -> Result<Builder> {
    let mut builder = Builder::default();
    let heads = store.ledger_heads(LedgerKind::Session)?;
    let Some((r, read_ms)) = rows_to_go_on_from(store.ledger_marks()?.as_ref(), &heads) else {
        return Ok(builder);
    };
    if store.ledger_older_rows()? {
        return Ok(builder);
    }
    builder.go_on_from_rows(r);
    builder.read_up_to(read_ms);
    let now_ms = builder.folds_at(now_ms);
    let heads = store.ledger_queue_heads_at(r.events)?;
    let (queue, lives, pre_claim) = books_of_heads(store, &heads, r.events, now_ms)?;
    builder.restore_pre_claim(lives, pre_claim);
    // Taken while nothing is marked.
    builder.restore_queue_rows(queue);
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
        if let Some(target) = node_target(&head.key).filter(|_| !head.is_final) {
            builder.keep(target);
        }
    }
    Ok(builder)
}

/// The mark of the node row of `key` ([`crate::domain::ledger::node::key`]).
fn node_target(key: &str) -> Option<Target> {
    let (node, start) = key.rsplit_once('/')?;
    Some(Target::Node(node.to_owned(), start.parse().ok()?))
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

/// One pass of [`LedgerWatch`] on `store` as it opened: the builder it held,
/// started on the store when it holds none ([`start_builder`]). A store that
/// did not open gives back the builder as it was, and a start that failed
/// gives none, so that the next pass starts it again: neither falls to a
/// builder from its cursors' start. The builder to hold, and what it did.
fn pass_of_watch(
    store: Result<&dyn LedgerQueue, &anyhow::Error>,
    builder: Option<Builder>,
    at: (i64, &str),
    retain: Retain,
    classifier: Option<&crate::domain::command_rules::CommandClassifier>,
) -> (Option<Builder>, Built) {
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
        None => match start_builder(store, at.0) {
            Ok(builder) => builder,
            Err(error) => return (None, failed("start the builder", &error)),
        },
    };
    let built = build_pass(store, &mut builder, at, retain, classifier);
    (Some(builder), built)
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
        let builder = self.builder.take();
        self.job = Some(spawn_traced(move || {
            let classifier = (port.classifier)();
            let opened = port.store.open();
            let store = match &opened {
                Ok(store) => Ok(&**store as &dyn LedgerQueue),
                Err(error) => Err(error),
            };
            pass_of_watch(
                store,
                builder,
                (now * 1000, &built_at),
                retain,
                classifier.as_ref(),
            )
        }));
    }

    /// Wait for the pass still running, as the loop ends, until
    /// `deadline` when one is given ([`super::finished_by`]); one still
    /// running then is left, and the next supervisor's ledger catches up.
    pub(in crate::application::supervise) fn finish(
        &mut self,
        env: &mut ObservationEnv<'_>,
        deadline: &super::EndBy,
    ) {
        if let Some(job) = self.job.take() {
            if super::finished_by(&job, deadline, &*env.generators.clock) {
                self.reap(env, job);
            } else {
                tracing::warn!("the ledger's pass still runs as the supervisor ends; it is left");
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
