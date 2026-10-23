//! What an executor sends of its sessions' steps (ADR-t1662-3 decisions 1,
//! 2 and 8, docs/design/measurement.md "集める口"): the steps its
//! provider's [`StepReader`] collected of the agent's output, or the
//! commands a verification ran, numbered in their session from
//! [`FIRST_SEQ`] and put on the session's scale (milliseconds from its
//! start, by the executor's clock, never a time of the day), and sent to
//! the target's [`TelemetrySink`] together every
//! [`REPORT_INTERVAL_SECS`], and once more at a turn's or a verification's
//! end. A report is at most [`MAX_STEPS_PER_REPORT`] steps within
//! [`crate::domain::telemetry::MAX_REPORT_BYTES`].
//!
//! What was not sent (no token for the target, a service that does not
//! answer, a session not registered yet) is kept and sent again the next
//! time, at most [`MAX_PENDING`] steps of a session, the oldest dropped
//! past it (a gap the store sees). The store keeps one of each
//! `(session_id, seq)`, so a step sent twice is added once. A wrapper keeps
//! its sessions' numbering and unsent steps in its `turns/`
//! ([`STEPS_FILE`]), so the wrapper of a resume goes on from them. Nothing
//! here changes a run's, a planner's or a job's outcome: a failure is
//! logged.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Instant,
};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::{RunFiles, StepReader, TelemetrySink, TelemetrySinks};
use crate::domain::command_shape::{Subcommands, shape_value_with};
use crate::domain::session_step::{FIRST_SEQ, Step, StepKind, StepTarget};
use crate::domain::telemetry::{
    MAX_NAME_BYTES, MAX_STEPS_PER_REPORT, REPORT_INTERVAL_SECS, TelemetryError, TelemetryReport,
};

/// The file of a wrapper's `turns/` that keeps its sessions' numbering
/// and unsent steps.
pub const STEPS_FILE: &str = "steps.json";

/// How many unsent steps of a session are kept.
pub const MAX_PENDING: usize = 4096;

/// [`REPORT_INTERVAL_SECS`] in milliseconds.
const INTERVAL_MS: i64 = REPORT_INTERVAL_SECS as i64 * 1000;

/// The tool name of a verification's command.
pub const VERIFICATION_TOOL: &str = "verification";

/// One session's numbering: the next seq, when it started on the
/// executor's clock (unix milliseconds, kept here only), and its unsent
/// steps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSteps {
    pub next_seq: i64,
    pub started_ms: i64,
    #[serde(default)]
    pub pending: Vec<Step>,
}

/// The sessions of one target an executor reports.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReporterState {
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionSteps>,
}

/// How one flush went.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flushed {
    /// Steps the sink took.
    pub sent: usize,
    /// Steps kept for the next time.
    pub kept: usize,
}

/// The steps of the sessions of one target, numbered and sent together.
#[derive(Debug, Clone)]
pub struct StepReporter {
    target: StepTarget,
    state: ReporterState,
    /// When the last flush was (or the reporter made), on the clock the
    /// caller gives.
    last_flush_ms: i64,
}

impl StepReporter {
    /// The reporter of `target` going on from `state`, made at `now_ms`.
    pub fn new(target: StepTarget, state: ReporterState, now_ms: i64) -> Self {
        Self {
            target,
            state,
            last_flush_ms: now_ms,
        }
    }

    pub fn target(&self) -> &StepTarget {
        &self.target
    }

    pub fn state(&self) -> &ReporterState {
        &self.state
    }

    /// How many steps wait to be sent.
    pub fn pending(&self) -> usize {
        self.state
            .sessions
            .values()
            .map(|session| session.pending.len())
            .sum()
    }

    /// Add `steps` of `session`, their offsets from `from_ms` (when the
    /// turn they are of started, on the clock the session's start is kept
    /// on): a session not seen yet starts there. Each is numbered, and its
    /// names are cut to what a report takes.
    pub fn push(&mut self, session: &str, from_ms: i64, steps: Vec<Step>) {
        if steps.is_empty() {
            return;
        }
        let kept = self
            .state
            .sessions
            .entry(session.to_owned())
            .or_insert(SessionSteps {
                next_seq: FIRST_SEQ,
                started_ms: from_ms,
                pending: Vec::new(),
            });
        let shift = from_ms.saturating_sub(kept.started_ms).max(0);
        for mut step in steps {
            step.seq = kept.next_seq;
            kept.next_seq += 1;
            step.offset_ms = step.offset_ms.saturating_add(shift).max(0);
            step.duration_ms = step.duration_ms.max(0);
            step.model = step.model.map(|name| cut(&name));
            step.tool = step.tool.map(|name| cut(&name));
            kept.pending.push(step);
        }
        if kept.pending.len() > MAX_PENDING {
            let dropped = kept.pending.len() - MAX_PENDING;
            kept.pending.drain(..dropped);
            warn!(
                session,
                "{dropped} unsent steps of session {session} were dropped past {MAX_PENDING}"
            );
        }
    }

    /// Whether [`REPORT_INTERVAL_SECS`] passed since the last flush at
    /// `now_ms`.
    pub fn due(&self, now_ms: i64) -> bool {
        now_ms.saturating_sub(self.last_flush_ms) >= INTERVAL_MS
    }

    /// Send what waits to `sink` at `now_ms`, each session's in reports of
    /// at most [`MAX_STEPS_PER_REPORT`] that fit their bound; what the sink
    /// did not take is kept for the next time. With no sink (no token for
    /// the target) nothing is sent and everything is kept. It never fails.
    pub fn flush(&mut self, sink: Option<&dyn TelemetrySink>, now_ms: i64) -> Flushed {
        self.last_flush_ms = now_ms;
        let mut flushed = Flushed::default();
        let Some(sink) = sink else {
            flushed.kept = self.pending();
            if flushed.kept > 0 {
                debug!(
                    target_kind = self.target.kind.as_str(),
                    target_id = %self.target.id,
                    "no telemetry token for {} {}: {} steps are kept",
                    self.target.kind.as_str(),
                    self.target.id,
                    flushed.kept
                );
            }
            return flushed;
        };
        'sessions: for (session, kept) in &mut self.state.sessions {
            while !kept.pending.is_empty() {
                let mut count = kept.pending.len().min(MAX_STEPS_PER_REPORT);
                let report = loop {
                    let report = TelemetryReport::new(
                        self.target.clone(),
                        session.as_str(),
                        kept.pending[..count].to_vec(),
                    );
                    match crate::domain::telemetry::check(&report) {
                        Err(TelemetryError::TooLarge(_)) if count > 1 => count /= 2,
                        checked => break checked.map(|()| report),
                    }
                };
                let report = match report {
                    Ok(report) => report,
                    Err(why) => {
                        // Steps no report takes are dropped, not sent
                        // again and again.
                        warn!(
                            session,
                            "{count} steps of session {session} were dropped: {why}"
                        );
                        kept.pending.drain(..count);
                        continue;
                    }
                };
                match sink.report(&report) {
                    Ok(_) => {
                        kept.pending.drain(..count);
                        flushed.sent += count;
                    }
                    Err(error) => {
                        warn!(
                            session,
                            "the steps of session {session} could not be sent; they are sent again later: {error:#}"
                        );
                        break 'sessions;
                    }
                }
            }
        }
        flushed.kept = self.pending();
        flushed
    }
}

/// `name` within [`MAX_NAME_BYTES`], on a character boundary.
fn cut(name: &str) -> String {
    let mut end = name.len().min(MAX_NAME_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_owned()
}

/// The turn a wrapper reads now: its provider's reader, when it started
/// (unix milliseconds on the executor's clock), the session it is of once
/// known, and the steps read before it was.
struct TurnSteps {
    reader: Box<dyn StepReader>,
    started_ms: i64,
    session: Option<String>,
    held: Vec<Step>,
}

/// What a session wrapper collects of its turns and sends (the module):
/// the reporter of its target, loaded from its `turns/` at its start, and
/// the turn it reads.
#[derive(Default)]
pub struct WrapperSteps {
    reporter: Option<(StepReporter, PathBuf)>,
    turn: Option<TurnSteps>,
}

impl WrapperSteps {
    /// Go on with what `turns_dir` keeps of `target`'s sessions at
    /// `now_ms`; a file that cannot be read starts afresh.
    pub fn load(
        &mut self,
        files: &dyn RunFiles,
        turns_dir: &Path,
        target: StepTarget,
        now_ms: i64,
    ) {
        let path = turns_dir.join(STEPS_FILE);
        let state = files
            .read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        self.reporter = Some((StepReporter::new(target, state, now_ms), path));
    }

    /// The turn started at `now_ms` read by `reader`, of `session` when it
    /// is known before its output names it.
    pub fn start_turn(
        &mut self,
        reader: Box<dyn StepReader>,
        now_ms: i64,
        session: Option<String>,
    ) {
        self.turn = Some(TurnSteps {
            reader,
            started_ms: now_ms,
            session,
            held: Vec::new(),
        });
    }

    /// The turn's output named its session `session`.
    pub fn identify(&mut self, session: &str) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.session = Some(session.to_owned());
        let held = std::mem::take(&mut turn.held);
        let started = turn.started_ms;
        if let Some((reporter, _)) = self.reporter.as_mut() {
            reporter.push(session, started, held);
        }
    }

    /// One line of the turn's output, read at `now_ms`.
    pub fn line(&mut self, line: &str, now_ms: i64) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        let steps = turn
            .reader
            .line(line, now_ms.saturating_sub(turn.started_ms));
        self.take(steps);
    }

    fn take(&mut self, steps: Vec<Step>) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        match (&turn.session, self.reporter.as_mut()) {
            (Some(session), Some((reporter, _))) => reporter.push(session, turn.started_ms, steps),
            _ => turn.held.extend(steps),
        }
    }

    /// At `now_ms`, send what waits once [`REPORT_INTERVAL_SECS`] passed,
    /// to the sink `sinks` gives for the target of the queue at `db`.
    pub fn tick(
        &mut self,
        files: &dyn RunFiles,
        sinks: Option<&dyn TelemetrySinks>,
        db: &Path,
        now_ms: i64,
    ) {
        if self
            .reporter
            .as_ref()
            .is_some_and(|(reporter, _)| reporter.due(now_ms))
        {
            self.flush(files, sinks, db, now_ms);
        }
    }

    /// The turn ended at `now_ms`: its open steps end there, and what waits
    /// is sent. The steps of a turn whose session never was known are
    /// dropped.
    pub fn end_turn(
        &mut self,
        files: &dyn RunFiles,
        sinks: Option<&dyn TelemetrySinks>,
        db: &Path,
        now_ms: i64,
    ) {
        if let Some(turn) = self.turn.as_mut() {
            let steps = turn.reader.finish(now_ms.saturating_sub(turn.started_ms));
            self.take(steps);
        }
        if let Some(turn) = self.turn.take()
            && !turn.held.is_empty()
        {
            debug!(
                "{} steps of a turn whose session was never named are dropped",
                turn.held.len()
            );
        }
        self.flush(files, sinks, db, now_ms);
    }

    fn flush(
        &mut self,
        files: &dyn RunFiles,
        sinks: Option<&dyn TelemetrySinks>,
        db: &Path,
        now_ms: i64,
    ) {
        let Some((reporter, path)) = self.reporter.as_mut() else {
            return;
        };
        let sink = if reporter.pending() > 0 {
            sinks.and_then(|sinks| sinks.sink(db, reporter.target()))
        } else {
            None
        };
        reporter.flush(sink.as_deref(), now_ms);
        save(files, path, reporter.state());
    }
}

/// Write `state` to `path` through a temporary file; a failure is logged:
/// the next wrapper then numbers from what it finds.
fn save(files: &dyn RunFiles, path: &Path, state: &ReporterState) {
    let tmp = path.with_extension("json.tmp");
    let written = serde_json::to_vec(state)
        .map_err(std::io::Error::from)
        .and_then(|bytes| files.write(&tmp, &bytes))
        .and_then(|()| files.rename(&tmp, path));
    if let Err(error) = written {
        warn!(
            "the steps of the session could not be kept in {}: {error:#}",
            path.display()
        );
    }
}

/// The step of a verification's command `command` (integrate's or the
/// e2e's, ADR-t1662-3 decision 1): a tool's call at `offset_ms` from the
/// verification's start lasting `duration_ms`, with its shape, its exit
/// (none when it was killed or its code is not known) and its output's
/// bytes, never its text; its shape keeps `known`'s subcommands in plain
/// and digests its other words under `known`'s key (none without one).
pub fn verification_step(
    (command, known): (&str, &Subcommands),
    offset_ms: i64,
    duration_ms: i64,
    exit_code: Option<i64>,
    output_bytes: Option<i64>,
) -> Step {
    let mut step = Step::new(0, StepKind::ToolCall, offset_ms, duration_ms);
    step.tool = Some(VERIFICATION_TOOL.to_owned());
    step.command_shape = shape_value_with(command, known);
    step.exit_code = exit_code;
    step.background = Some(false);
    step.output_bytes = output_bytes;
    step
}

/// The steps of one session an executor of the supervisor's process sends
/// (a verification's, a headless job's), on the session's scale
/// (milliseconds from its start, given as values): sent to the target's
/// sink every [`REPORT_INTERVAL_SECS`] ([`Self::tick`]) and at its end
/// ([`Self::finish`]). Steps read before the session is known are held
/// until it is ([`Self::name_session`]).
pub struct VerificationSteps {
    sinks: std::sync::Arc<dyn TelemetrySinks>,
    db: PathBuf,
    session: Option<String>,
    held: Vec<Step>,
    reporter: StepReporter,
}

impl VerificationSteps {
    /// The session `session` of `target` of the queue at `db`; `None`
    /// until its agent names it.
    pub fn new(
        sinks: std::sync::Arc<dyn TelemetrySinks>,
        db: PathBuf,
        target: StepTarget,
        session: Option<String>,
    ) -> Self {
        Self {
            sinks,
            db,
            session,
            held: Vec::new(),
            reporter: StepReporter::new(target, ReporterState::default(), 0),
        }
    }

    /// A verification of `run` of the queue at `db` as session `session`,
    /// which the control side registers to the run here before the
    /// verification runs (a registration that failed is logged, and its
    /// reports are refused).
    pub fn of_run(
        tokens: &dyn super::queue_service::TelemetryTokens,
        sinks: std::sync::Arc<dyn TelemetrySinks>,
        db: &Path,
        run: &crate::domain::RunId,
        session: String,
    ) -> Self {
        let target = StepTarget::run(run);
        super::telemetry_tokens::register_session(tokens, db, &target, &session);
        Self::new(sinks, db.to_path_buf(), target, Some(session))
    }

    /// The session is `session` from now: what was held is its.
    pub fn name_session(&mut self, session: &str) {
        if self.session.is_some() {
            return;
        }
        self.session = Some(session.to_owned());
        let held = std::mem::take(&mut self.held);
        self.reporter.push(session, 0, held);
    }

    /// `steps` of the session, then a [`Self::tick`] at `now_ms`.
    pub fn push(&mut self, steps: Vec<Step>, now_ms: i64) {
        self.add(steps);
        self.tick(now_ms);
    }

    /// `steps` of the session, held until it is named; nothing is sent.
    fn add(&mut self, steps: Vec<Step>) {
        match &self.session {
            Some(session) => self.reporter.push(session, 0, steps),
            None => self.held.extend(steps),
        }
    }

    /// Its command `command` (its shape keeping the known subcommands it
    /// comes with) from `from_ms` to `to_ms`, with `exit_code` and
    /// `output_bytes`: its step, sent with the others once
    /// [`REPORT_INTERVAL_SECS`] passed.
    pub fn command(
        &mut self,
        command: (&str, &Subcommands),
        (from_ms, to_ms): (i64, i64),
        exit_code: Option<i64>,
        output_bytes: Option<i64>,
    ) {
        // Its words are digested under the queue's key.
        let (command, known) = command;
        let known = known.clone().keyed(self.sinks.digest_key(&self.db));
        let step = verification_step(
            (command, &known),
            from_ms,
            to_ms.saturating_sub(from_ms),
            exit_code,
            output_bytes,
        );
        self.push(vec![step], to_ms);
    }

    /// At `now_ms`, send what waits once [`REPORT_INTERVAL_SECS`] passed
    /// since the last time.
    pub fn tick(&mut self, now_ms: i64) {
        if self.reporter.due(now_ms) {
            self.send(now_ms);
        }
    }

    /// The session ended at `now_ms`: what waits is sent. Whether nothing
    /// is left unsent.
    pub fn finish(&mut self, now_ms: i64) -> bool {
        self.send(now_ms);
        self.reporter.pending() == 0
    }

    /// Send what waits; whether its target still has a token to send with.
    fn send(&mut self, now_ms: i64) -> bool {
        if self.reporter.pending() == 0 {
            return true;
        }
        let sink = self.sinks.sink(&self.db, self.reporter.target());
        self.reporter.flush(sink.as_deref(), now_ms);
        sink.is_some()
    }

    /// The steps that wait to be sent.
    pub fn pending(&self) -> usize {
        self.reporter.pending()
    }
}

/// Milliseconds from `origin` to `at`.
pub fn millis_since(origin: Instant, at: Instant) -> i64 {
    i64::try_from(at.saturating_duration_since(origin).as_millis()).unwrap_or(i64::MAX)
}

/// A verification's steps shared with what sends them every
/// [`REPORT_INTERVAL_SECS`] while its commands run ([`Ticking`]), its scale
/// from `origin`.
pub struct SharedVerification {
    origin: Instant,
    steps: std::sync::Mutex<VerificationSteps>,
}

impl SharedVerification {
    pub fn new(steps: VerificationSteps, origin: Instant) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            origin,
            steps: std::sync::Mutex::new(steps),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VerificationSteps> {
        self.steps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// [`VerificationSteps::command`], from `started` to `ended`.
    pub fn command(
        &self,
        command: (&str, &Subcommands),
        (started, ended): (Instant, Instant),
        exit_code: Option<i64>,
        output_bytes: Option<i64>,
    ) {
        let span = (
            millis_since(self.origin, started),
            millis_since(self.origin, ended),
        );
        self.lock().command(command, span, exit_code, output_bytes);
    }

    /// [`VerificationSteps::tick`] at `at`.
    pub fn tick(&self, at: Instant) {
        let now = millis_since(self.origin, at);
        self.lock().tick(now);
    }

    /// [`VerificationSteps::finish`] at `at`: what is left unsent goes to
    /// `unsent`, sent again at the next sweeps.
    pub fn finish(&self, at: Instant, unsent: &UnsentSteps) -> bool {
        let now = millis_since(self.origin, at);
        let mut steps = self.lock();
        if steps.finish(now) {
            return true;
        }
        let empty = VerificationSteps::new(
            steps.sinks.clone(),
            steps.db.clone(),
            steps.reporter.target().clone(),
            steps.session.clone(),
        );
        unsent.keep(std::mem::replace(&mut *steps, empty));
        false
    }
}

/// A landing's verification's steps ([`SharedVerification`]) and where
/// what its end could not send goes ([`UnsentSteps`]).
pub struct LandingSteps {
    pub shared: std::sync::Arc<SharedVerification>,
    pub unsent: std::sync::Arc<UnsentSteps>,
    /// The clock its [`Ticking`] looks at.
    pub clock: std::sync::Arc<dyn super::Clock>,
}

/// Sends a [`SharedVerification`]'s steps once their interval passed, looked
/// at twice a second while it lives from a thread of its own; stopped when
/// dropped.
pub struct Ticking {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Ticking {
    pub fn start(
        shared: std::sync::Arc<SharedVerification>,
        clock: std::sync::Arc<dyn super::Clock>,
    ) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        let handle = std::thread::Builder::new()
            .name("verification-steps".to_owned())
            .spawn(move || {
                while !stopped.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    shared.tick(clock.monotonic());
                }
            })
            .map_err(|error| {
                warn!("the steps of a verification are sent only at its end: {error:#}")
            })
            .ok();
        Self { stop, handle }
    }
}

impl Drop for Ticking {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The steps the supervisor's executors (its verifications, its jobs)
/// could not send by their end: sent again at each sweep while their
/// target has its token, dropped once it has none (the target ended).
#[derive(Default)]
pub struct UnsentSteps(std::sync::Mutex<Vec<VerificationSteps>>);

impl UnsentSteps {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<VerificationSteps>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Keep `steps` for the next sweep.
    pub fn keep(&self, steps: VerificationSteps) {
        if steps.pending() > 0 {
            self.lock().push(steps);
        }
    }

    /// The sessions of the kept steps of targets of `kind`, each with its
    /// target: what the control side registers before it sends them again
    /// (an eval's job, whose session only its output names, named at its
    /// end).
    pub fn sessions_of(
        &self,
        kind: crate::domain::session_step::TargetKind,
    ) -> Vec<(StepTarget, String)> {
        self.lock()
            .iter()
            .filter(|steps| steps.reporter.target().kind == kind)
            .filter_map(|steps| {
                let session = steps.session.clone()?;
                Some((steps.reporter.target().clone(), session))
            })
            .collect()
    }

    /// Send again what is kept; how many steps are still kept.
    pub fn resend(&self) -> usize {
        let kept = std::mem::take(&mut *self.lock());
        let mut left = Vec::new();
        for mut steps in kept {
            let now = steps.reporter.last_flush_ms;
            if !steps.send(now) {
                warn!(
                    target_kind = steps.reporter.target().kind.as_str(),
                    target_id = %steps.reporter.target().id,
                    "{} steps were not sent before their target's token ended",
                    steps.pending()
                );
                continue;
            }
            if steps.pending() > 0 {
                left.push(steps);
            }
        }
        let count = left.iter().map(VerificationSteps::pending).sum();
        self.lock().extend(left);
        count
    }
}

/// What a headless job's executor (the supervisor that started it) reads
/// of its agent's stdout as it runs, by its provider's [`StepReader`], and
/// sends as the steps of its session ([`VerificationSteps`]): each look is
/// at its time from the job's start, given as a value.
pub struct JobSteps {
    reader: Box<dyn StepReader>,
    read: u64,
    rest: Vec<u8>,
    steps: VerificationSteps,
    unsent: std::sync::Arc<UnsentSteps>,
}

impl JobSteps {
    pub fn new(
        reader: Box<dyn StepReader>,
        steps: VerificationSteps,
        unsent: std::sync::Arc<UnsentSteps>,
    ) -> Self {
        Self {
            reader,
            read: 0,
            rest: Vec::new(),
            steps,
            unsent,
        }
    }

    /// The session its agent's output named, once it did (or the one named
    /// ahead).
    pub fn session(&self) -> Option<String> {
        self.steps.session.clone()
    }

    /// Read what `stdout` got since the last look, `now_ms` after the
    /// job's start, and send once [`REPORT_INTERVAL_SECS`] passed.
    pub fn look(&mut self, files: &dyn RunFiles, stdout: &Path, now_ms: i64) {
        self.read_on(files, stdout, now_ms);
        self.steps.tick(now_ms);
    }

    /// Read what `stdout` got since the last look, `now_ms` after the
    /// job's start, into its steps and the session it names, and send
    /// nothing: what the control side reads to learn the session it
    /// registers before the steps are sent ([`Self::session`]).
    pub fn read_on(&mut self, files: &dyn RunFiles, stdout: &Path, now_ms: i64) {
        let bytes = files.read_from(stdout, self.read).unwrap_or_default();
        self.read += bytes.len() as u64;
        self.rest.extend_from_slice(&bytes);
        let mut steps = Vec::new();
        while let Some(at) = self.rest.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.rest.drain(..=at).collect();
            steps.extend(
                self.reader
                    .line(String::from_utf8_lossy(&line).trim_end(), now_ms),
            );
        }
        if let Some(session) = self.reader.session() {
            self.steps.name_session(&session);
        }
        self.steps.add(steps);
    }

    /// The job ended `now_ms` after its start: the rest of its output is
    /// read, its open steps end, and what waits is sent; what could not be
    /// is sent again at the next sweeps.
    pub fn finish(mut self, files: &dyn RunFiles, stdout: &Path, now_ms: i64) {
        self.look(files, stdout, now_ms);
        if !self.rest.is_empty() {
            let line = std::mem::take(&mut self.rest);
            let steps = self
                .reader
                .line(String::from_utf8_lossy(&line).trim_end(), now_ms);
            self.steps.push(steps, now_ms);
        }
        let steps = self.reader.finish(now_ms);
        self.steps.push(steps, now_ms);
        if !self.steps.finish(now_ms) {
            self.unsent.keep(self.steps);
        }
    }
}

/// How often a command that waits for its agent looks at its stdout
/// ([`CommandSteps::watch`]).
const LOOK_MS: u64 = 500;

/// What a command that waits for its agent (an observation, a throughput
/// review: a headless job its own process runs, ADR-t1662-3 decisions 1
/// and 10) sends its agent's session's steps with as their executor: the
/// files its agent's stdout is read through and the sinks of the targets'
/// tokens.
#[derive(Clone)]
pub struct CommandSteps<'a> {
    pub files: &'a dyn RunFiles,
    pub sinks: std::sync::Arc<dyn TelemetrySinks>,
}

/// The steps a [`CommandSteps::watch`] read, to end once its agent's
/// session is the target's ([`Self::finish`]).
pub struct WatchedSteps<'a> {
    files: &'a dyn RunFiles,
    stdout: PathBuf,
    started: Instant,
    /// When the wait for the agent returned: its steps end there.
    ended: Instant,
    steps: JobSteps,
    unsent: std::sync::Arc<UnsentSteps>,
}

impl<'a> CommandSteps<'a> {
    /// Run `run` (the wait for the agent `agent` starts, of `model`, whose
    /// stdout goes to `stdout`) while a thread of its own reads that
    /// stdout every [`LOOK_MS`] by `agent`'s [`StepReader`] into the steps
    /// of `target`'s session (`session` when it is named ahead, else the
    /// one the output names) of the queue at `db`, from `clock`'s now:
    /// sent every [`REPORT_INTERVAL_SECS`], what failed again the next
    /// time. Its value, and the steps to end.
    pub fn watch<T>(
        &self,
        agent: &dyn super::AgentProvider,
        model: Option<&str>,
        (db, target, session): (&Path, StepTarget, Option<&str>),
        stdout: &Path,
        clock: &dyn super::Clock,
        run: impl FnOnce() -> T,
    ) -> (T, WatchedSteps<'a>) {
        let unsent = std::sync::Arc::new(UnsentSteps::default());
        let mut steps = JobSteps::new(
            agent.step_reader(
                model,
                Subcommands::common().keyed(self.sinks.digest_key(db)),
            ),
            VerificationSteps::new(
                self.sinks.clone(),
                db.to_path_buf(),
                target,
                session.map(str::to_owned),
            ),
            unsent.clone(),
        );
        let started = clock.monotonic();
        let files = self.files;
        let (value, ended) = std::thread::scope(|scope| {
            let (stop, stopped) = std::sync::mpsc::channel::<()>();
            let steps = &mut steps;
            // A thread that cannot start, or a look that panics, only
            // leaves the steps to the end's read: the wait goes on.
            let watcher = std::thread::Builder::new()
                .name("job-steps".to_owned())
                .spawn_scoped(scope, move || {
                    // Until `run` returns and drops `stop`.
                    while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                        stopped.recv_timeout(std::time::Duration::from_millis(LOOK_MS))
                    {
                        steps.look(files, stdout, millis_since(started, clock.monotonic()));
                    }
                })
                .inspect_err(|error| {
                    warn!("the agent's steps are read only at its end: {error:#}");
                })
                .ok();
            let value = run();
            let ended = clock.monotonic();
            drop(stop);
            if let Some(watcher) = watcher
                && watcher.join().is_err()
            {
                warn!("the reading of the agent's steps stopped: the rest is read at its end");
            }
            (value, ended)
        });
        (
            value,
            WatchedSteps {
                files,
                stdout: stdout.to_path_buf(),
                started,
                ended,
                steps,
                unsent,
            },
        )
    }
}

impl WatchedSteps<'_> {
    /// The agent ended (and its session, a Codex thread named only by its
    /// output, is registered to the target): the rest of its stdout is
    /// read, its open steps end when its wait returned, and what waits is
    /// sent, before the target's token is revoked.
    /// What still cannot be is tried once more, and then is a gap the
    /// store sees.
    pub fn finish(self) {
        let now = millis_since(self.started, self.ended);
        self.steps.finish(self.files, &self.stdout, now);
        let left = self.unsent.resend();
        if left > 0 {
            warn!("{left} steps of the session were not sent before its target ended");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::memory_files::MemoryFiles as MemoryRunFiles;
    use crate::domain::telemetry::ReportRecorded;
    use std::sync::{Arc, Mutex};

    /// A sink that records the reports it takes, refusing while `down`.
    #[derive(Default)]
    struct Sink {
        reports: Mutex<Vec<TelemetryReport>>,
        down: Mutex<bool>,
    }

    impl TelemetrySink for Sink {
        fn report(&self, report: &TelemetryReport) -> anyhow::Result<ReportRecorded> {
            if *self.down.lock().unwrap() {
                anyhow::bail!("the queue service at /s is unreachable");
            }
            self.reports.lock().unwrap().push(report.clone());
            Ok(ReportRecorded::default())
        }
    }

    impl TelemetrySinks for Arc<Sink> {
        fn sink(&self, _: &Path, _: &StepTarget) -> Option<Box<dyn TelemetrySink>> {
            Some(Box::new(Forward(self.clone())))
        }

        fn digest_key(&self, _: &Path) -> Option<crate::domain::command_shape::DigestKey> {
            Some(crate::domain::command_shape::DigestKey::test())
        }
    }

    struct Forward(Arc<Sink>);

    impl TelemetrySink for Forward {
        fn report(&self, report: &TelemetryReport) -> anyhow::Result<ReportRecorded> {
            self.0.report(report)
        }
    }

    fn target() -> StepTarget {
        StepTarget::of(crate::domain::session_step::TargetKind::Planner, 3)
    }

    fn sent(sink: &Sink) -> Vec<(String, Vec<i64>, Vec<i64>)> {
        sink.reports
            .lock()
            .unwrap()
            .iter()
            .map(|report| {
                (
                    report.session_id.clone(),
                    report.steps.iter().map(|step| step.seq).collect(),
                    report.steps.iter().map(|step| step.offset_ms).collect(),
                )
            })
            .collect()
    }

    fn wait(offset: i64) -> Step {
        Step::new(0, StepKind::Wait, offset, 10)
    }

    /// Steps are numbered in their session, put on its scale, and sent
    /// together once the interval passed at the times given; a sink that
    /// fails or no token keeps them, and they are sent again the next
    /// time, each once.
    #[test]
    fn steps_are_sent_together_every_interval_and_again_after_a_failure() {
        let sink = Sink::default();
        let mut reporter = StepReporter::new(target(), ReporterState::default(), 1_000);
        reporter.push("s", 1_000, vec![wait(0), wait(5)]);
        // The session's second turn started 60 s after its first.
        reporter.push("s", 61_000, vec![wait(7)]);
        assert!(!reporter.due(30_999));
        assert!(reporter.due(31_000));
        // No token: nothing is sent, all is kept.
        assert_eq!(reporter.flush(None, 31_000), Flushed { sent: 0, kept: 3 });
        assert!(!reporter.due(60_999));
        // A service that does not answer: kept again.
        *sink.down.lock().unwrap() = true;
        assert_eq!(
            reporter.flush(Some(&sink), 61_000),
            Flushed { sent: 0, kept: 3 }
        );
        assert!(sent(&sink).is_empty());
        *sink.down.lock().unwrap() = false;
        reporter.push("s", 61_000, vec![wait(9)]);
        assert_eq!(
            reporter.flush(Some(&sink), 91_000),
            Flushed { sent: 4, kept: 0 }
        );
        assert_eq!(
            sent(&sink),
            vec![("s".to_owned(), vec![1, 2, 3, 4], vec![0, 5, 60_007, 60_009])]
        );
        // Nothing waits: nothing more is sent.
        assert_eq!(reporter.flush(Some(&sink), 121_000), Flushed::default());
        assert_eq!(sink.reports.lock().unwrap().len(), 1);
    }

    /// A flush past a report's bound sends several reports, each within
    /// it; a name too long for a report is cut.
    #[test]
    fn many_steps_go_in_reports_within_their_bounds() {
        let sink = Sink::default();
        let mut reporter = StepReporter::new(target(), ReporterState::default(), 0);
        let mut long = Step::new(0, StepKind::ToolCall, 0, 1);
        long.tool = Some("t".repeat(300));
        reporter.push("s", 0, vec![long]);
        reporter.push(
            "s",
            0,
            (0..MAX_STEPS_PER_REPORT as i64 + 10).map(wait).collect(),
        );
        assert_eq!(
            reporter.flush(Some(&sink), 0),
            Flushed {
                sent: MAX_STEPS_PER_REPORT + 11,
                kept: 0
            }
        );
        let reports = sink.reports.lock().unwrap();
        assert_eq!(reports.len(), 2);
        assert!(
            reports
                .iter()
                .all(|r| crate::domain::telemetry::check(r).is_ok())
        );
        assert_eq!(
            reports[0].steps[0].tool.as_ref().unwrap().len(),
            MAX_NAME_BYTES
        );
    }

    /// Past [`MAX_PENDING`] the oldest unsent steps are dropped: their
    /// seqs are the store's gap.
    #[test]
    fn unsent_steps_past_the_limit_drop_the_oldest() {
        let mut reporter = StepReporter::new(target(), ReporterState::default(), 0);
        reporter.push("s", 0, (0..MAX_PENDING as i64 + 3).map(wait).collect());
        let kept = &reporter.state().sessions["s"];
        assert_eq!(kept.pending.len(), MAX_PENDING);
        assert_eq!(kept.pending[0].seq, 4);
        assert_eq!(kept.next_seq, MAX_PENDING as i64 + 4);
    }

    /// A reader that makes one step per line, at the line's offset.
    struct Lines;

    impl StepReader for Lines {
        fn line(&mut self, line: &str, at_ms: i64) -> Vec<Step> {
            let mut step = Step::new(0, StepKind::ToolCall, at_ms, 1);
            step.tool = Some(line.to_owned());
            vec![step]
        }
        fn finish(&mut self, at_ms: i64) -> Vec<Step> {
            vec![Step::new(0, StepKind::ModelTurn, at_ms, 0)]
        }
    }

    /// A wrapper holds a turn's steps until its output names its session,
    /// sends them at the turn's end, and keeps its numbering and what it
    /// could not send in its `turns/` for the wrapper of a resume.
    #[test]
    fn a_wrappers_steps_go_on_across_turns_and_wrappers() {
        let files = MemoryRunFiles::default();
        let sink = Arc::new(Sink::default());
        let sinks: Arc<Sink> = sink.clone();
        let dir = Path::new("/run/turns");
        let db = Path::new("/q/queue.db");
        let mut steps = WrapperSteps::default();
        steps.load(&files, dir, target(), 0);
        steps.start_turn(Box::new(Lines), 1_000, None);
        steps.line("Read", 1_200);
        steps.identify("thread-1");
        steps.line("Bash", 1_500);
        // Before the interval nothing is sent.
        steps.tick(&files, Some(&sinks), db, 20_000);
        assert!(sent(&sink).is_empty());
        steps.tick(&files, Some(&sinks), db, 31_000);
        assert_eq!(
            sent(&sink),
            vec![("thread-1".to_owned(), vec![1, 2], vec![200, 500])]
        );
        *sink.down.lock().unwrap() = true;
        steps.end_turn(&files, Some(&sinks), db, 40_000);
        // The wrapper of a resume goes on from its numbering and resends.
        *sink.down.lock().unwrap() = false;
        let mut resumed = WrapperSteps::default();
        resumed.load(&files, dir, target(), 100_000);
        resumed.start_turn(Box::new(Lines), 100_000, Some("thread-1".to_owned()));
        resumed.line("Edit", 100_100);
        resumed.end_turn(&files, Some(&sinks), db, 100_300);
        assert_eq!(
            sent(&sink)[1..],
            [(
                "thread-1".to_owned(),
                vec![3, 4, 5],
                vec![39_000, 99_100, 99_300]
            )]
        );
        // A turn whose session is never named keeps nothing of it.
        resumed.start_turn(Box::new(Lines), 200_000, None);
        resumed.line("Read", 200_100);
        resumed.end_turn(&files, Some(&sinks), db, 200_200);
        assert_eq!(sink.reports.lock().unwrap().len(), 2);
        let kept: ReporterState =
            serde_json::from_str(&files.read_to_string(&dir.join(STEPS_FILE)).unwrap()).unwrap();
        assert_eq!(kept.sessions["thread-1"].next_seq, 6);
        assert!(kept.sessions["thread-1"].pending.is_empty());
    }

    /// With no sink the wrapper's turn ends as any other, its steps kept.
    #[test]
    fn a_wrapper_with_no_token_keeps_its_steps() {
        let files = MemoryRunFiles::default();
        let dir = Path::new("/run/turns");
        let mut steps = WrapperSteps::default();
        steps.load(&files, dir, target(), 0);
        steps.start_turn(Box::new(Lines), 0, Some("s".to_owned()));
        steps.line("Read", 10);
        steps.end_turn(&files, None, Path::new("/q/queue.db"), 20);
        let kept: ReporterState =
            serde_json::from_str(&files.read_to_string(&dir.join(STEPS_FILE)).unwrap()).unwrap();
        assert_eq!(kept.sessions["s"].pending.len(), 2);
    }

    /// A verification's command is a tool's call with its shape, exit,
    /// output's bytes and times, never its text; its steps are sent every
    /// interval, also while a command runs, and at its end.
    #[test]
    fn a_verifications_commands_are_tool_calls_of_its_session() {
        let sink = Arc::new(Sink::default());
        let mut steps = VerificationSteps::new(
            Arc::new(sink.clone()),
            PathBuf::from("/q/queue.db"),
            StepTarget::run(
                &crate::domain::RunId::new("0af1cbdd-b587-4db4-aac3-631ec0f2fbea").unwrap(),
            ),
            Some("landing-1".to_owned()),
        );
        let command = "cargo test --locked --test it runtime_claim::";
        let known = Subcommands::common();
        steps.command((command, &known), (100, 2_100), Some(101), Some(42));
        steps.command(
            ("cargo fmt --all --check", &known),
            (2_100, 2_200),
            Some(0),
            Some(0),
        );
        assert!(sent(&sink).is_empty());
        assert!(steps.finish(2_300));
        let reports = sink.reports.lock().unwrap();
        let step = &reports[0].steps[0];
        assert_eq!(
            (
                step.seq,
                step.kind,
                step.offset_ms,
                step.duration_ms,
                step.exit_code,
                step.output_bytes
            ),
            (1, StepKind::ToolCall, 100, 2_000, Some(101), Some(42))
        );
        assert_eq!(step.tool.as_deref(), Some(VERIFICATION_TOOL));
        assert_eq!(
            step.command_shape,
            Some(crate::domain::command_shape::shape_value(command))
        );
        assert_eq!(reports[0].steps[1].seq, 2);
        let text = serde_json::to_string(&reports[0]).unwrap();
        assert!(!text.contains("runtime_claim"), "{text}");
        assert!(!text.contains("token"), "{text}");
        drop(reports);
        // A first command of 5 s, then one of minutes: the first is sent
        // 30 s on, while the second still runs.
        let mut long = VerificationSteps::new(
            Arc::new(sink.clone()),
            PathBuf::from("/q/queue.db"),
            target(),
            Some("e2e-1".to_owned()),
        );
        long.command(("cargo build", &known), (0, 5_000), Some(0), None);
        long.tick(29_999);
        assert_eq!(sink.reports.lock().unwrap().len(), 1);
        long.tick(30_000);
        assert_eq!(sink.reports.lock().unwrap().len(), 2);
        *sink.down.lock().unwrap() = true;
        long.command(("cargo test", &known), (5_000, 200_000), Some(0), None);
        assert!(!long.finish(200_000));
        assert_eq!(long.pending(), 1);
        *sink.down.lock().unwrap() = false;
        assert!(long.finish(210_000));
        assert_eq!(sink.reports.lock().unwrap().len(), 3);
    }

    /// Sinks found by target: none once its token is gone.
    #[derive(Default)]
    struct Tokens {
        sink: Arc<Sink>,
        gone: Mutex<bool>,
    }

    impl TelemetrySinks for Tokens {
        fn sink(&self, _: &Path, _: &StepTarget) -> Option<Box<dyn TelemetrySink>> {
            (!*self.gone.lock().unwrap()).then(|| Box::new(Forward(self.sink.clone())) as _)
        }

        fn digest_key(&self, _: &Path) -> Option<crate::domain::command_shape::DigestKey> {
            Some(crate::domain::command_shape::DigestKey::test())
        }
    }

    /// What a verification or a job could not send by its end is sent
    /// again at a later sweep, as the same steps, and dropped once its
    /// target's token is gone.
    #[test]
    fn unsent_steps_are_sent_again_until_their_targets_token_ends() {
        let tokens = Arc::new(Tokens::default());
        let unsent = UnsentSteps::default();
        let mut steps = VerificationSteps::new(
            tokens.clone(),
            PathBuf::from("/q/queue.db"),
            target(),
            Some("landing-1".to_owned()),
        );
        *tokens.sink.down.lock().unwrap() = true;
        steps.command(
            ("cargo build", &Subcommands::common()),
            (0, 10),
            Some(0),
            None,
        );
        assert!(!steps.finish(20));
        unsent.keep(steps);
        assert_eq!(unsent.resend(), 1);
        *tokens.sink.down.lock().unwrap() = false;
        assert_eq!(unsent.resend(), 0);
        assert_eq!(
            sent(&tokens.sink),
            vec![("landing-1".to_owned(), vec![1], vec![0])]
        );
        // A target whose token ended keeps nothing.
        let mut steps = VerificationSteps::new(
            tokens.clone(),
            PathBuf::from("/q/queue.db"),
            target(),
            Some("landing-2".to_owned()),
        );
        *tokens.sink.down.lock().unwrap() = true;
        steps.command(
            ("cargo build", &Subcommands::common()),
            (0, 10),
            Some(0),
            None,
        );
        assert!(!steps.finish(20));
        unsent.keep(steps);
        *tokens.gone.lock().unwrap() = true;
        assert_eq!(unsent.resend(), 0);
        *tokens.gone.lock().unwrap() = false;
        *tokens.sink.down.lock().unwrap() = false;
        assert_eq!(unsent.resend(), 0);
        assert_eq!(sink_count(&tokens.sink), 1);
    }

    /// The kept steps name, by their target's kind, the sessions the
    /// control side registers before it sends them again: an eval's job's,
    /// not another target's, and none of steps whose session is not named.
    #[test]
    fn the_sessions_of_kept_steps_are_listed_by_their_targets_kind() {
        use crate::domain::session_step::TargetKind;
        let tokens = Arc::new(Tokens::default());
        *tokens.sink.down.lock().unwrap() = true;
        let unsent = UnsentSteps::default();
        let eval = StepTarget::of(TargetKind::EvalJob, 7);
        for (target, session) in [
            (eval.clone(), Some("eval-session")),
            (target(), Some("planner-session")),
            (StepTarget::of(TargetKind::EvalJob, 8), None),
        ] {
            let mut steps = VerificationSteps::new(
                tokens.clone(),
                PathBuf::from("/q/queue.db"),
                target,
                session.map(str::to_owned),
            );
            steps.command(
                ("cargo build", &Subcommands::common()),
                (0, 10),
                Some(0),
                None,
            );
            steps.finish(20);
            unsent.keep(steps);
        }
        assert_eq!(
            unsent.sessions_of(TargetKind::EvalJob),
            vec![(eval, "eval-session".to_owned())]
        );
    }

    fn sink_count(sink: &Sink) -> usize {
        sink.reports.lock().unwrap().len()
    }

    /// A reader that makes one step per line and names its session at the
    /// line `session`.
    struct Named;

    impl StepReader for Named {
        fn line(&mut self, line: &str, at_ms: i64) -> Vec<Step> {
            Lines.line(line, at_ms)
        }
        fn finish(&mut self, at_ms: i64) -> Vec<Step> {
            Lines.finish(at_ms)
        }
        fn session(&self) -> Option<String> {
            Some("thread-9".to_owned())
        }
    }

    /// A job's stdout is read as it grows, whole lines only, its steps
    /// held until its agent names its session and sent every interval at
    /// the times given; at its end the rest is read and sent, what could
    /// not be kept for a sweep.
    #[test]
    fn a_jobs_output_is_read_as_it_runs_and_sent_as_its_sessions_steps() {
        let files = MemoryRunFiles::default();
        let out = Path::new("/run/review-1.out");
        let tokens = Arc::new(Tokens::default());
        let unsent = Arc::new(UnsentSteps::default());
        let mut job = JobSteps::new(
            Box::new(Lines),
            VerificationSteps::new(tokens.clone(), PathBuf::from("/q/queue.db"), target(), None),
            unsent.clone(),
        );
        files.put(out, std::time::SystemTime::UNIX_EPOCH, "Read\nBa");
        job.look(&files, out, 1_000);
        assert_eq!(job.steps.held.len(), 1);
        job.reader = Box::new(Named);
        files.put(out, std::time::SystemTime::UNIX_EPOCH, "Read\nBash\n");
        job.look(&files, out, 2_000);
        assert_eq!(job.steps.pending(), 2);
        assert_eq!(sink_count(&tokens.sink), 0);
        job.look(&files, out, 31_000);
        assert_eq!(
            sent(&tokens.sink),
            vec![("thread-9".to_owned(), vec![1, 2], vec![1_000, 2_000])]
        );
        files.put(out, std::time::SystemTime::UNIX_EPOCH, "Read\nBash\nEdit");
        *tokens.sink.down.lock().unwrap() = true;
        job.finish(&files, out, 40_000);
        *tokens.sink.down.lock().unwrap() = false;
        assert_eq!(unsent.resend(), 0);
        assert_eq!(
            sent(&tokens.sink)[1..],
            [("thread-9".to_owned(), vec![3, 4], vec![40_000, 40_000])]
        );
    }
}
