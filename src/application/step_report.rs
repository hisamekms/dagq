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
//! past it (a gap the store sees). A session the service refuses as not
//! of the target is sent again at most [`MAX_DENIALS`] times in a row
//! ([`WRAPPER_MAX_DENIALS`] from a session wrapper), and then given up
//! with one warning ([`StepReporter::flush`]). The
//! supervisor's own jobs send nothing of a session until the control side
//! registered it ([`JobSteps::keep`]), so a refusal there is a
//! registration that failed. The store keeps one of each
//! `(session_id, seq)`, so a step sent twice is added once. A wrapper keeps
//! its sessions' numbering and unsent steps in the file of its target the
//! control side's [`TelemetrySinks::steps_file`] names, beside the tokens'
//! files and outside every worktree, run, planner and job directory, so the
//! wrapper of a resume goes on from them and no agent can put a step there
//! for the executor to send. The file names its target ([`KeptSteps`]),
//! and the supervisor's sweep removes it once the queue says that target
//! ended, whatever its token
//! ([`super::telemetry_tokens::sweep_kept_steps`]): a `failed` or
//! `interrupted` run's stays while recovery can put the run back under the
//! same ID, so its resume's wrapper numbers on past the seqs already sent
//! ([`crate::domain::telemetry::run_ended`]). Nothing
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
    MAX_NAME_BYTES, MAX_STEPS_PER_REPORT, REPORT_INTERVAL_SECS, ReportDenied, TelemetryError,
    TelemetryReport,
};

/// How many unsent steps of a session are kept.
pub const MAX_PENDING: usize = 4096;

/// How many reports of one session in a row the queue service may refuse
/// as not of the token's target ([`ReportDenied`]) before the reporter
/// stops sending that session, for the supervisor's own jobs and
/// verifications: the control side registers their sessions before it
/// sends them (and before it sends a job's kept steps again), so a session
/// still refused after this many is one it will not register (another
/// target's). A report that went through counts the session's refusals
/// from zero again.
pub const MAX_DENIALS: u32 = 3;

/// [`MAX_DENIALS`] for a session wrapper ([`WrapperSteps`]): a thread its
/// agent names is registered only once a pass of the supervisor reads it in
/// the run's events, and the wrapper sends at every
/// [`REPORT_INTERVAL_SECS`] and every turn's end, so it waits about ten
/// minutes of its sends (a supervisor slow or between processes) before
/// it gives a session up.
pub const WRAPPER_MAX_DENIALS: u32 = 20;

/// [`REPORT_INTERVAL_SECS`] in milliseconds.
const INTERVAL_MS: i64 = REPORT_INTERVAL_SECS as i64 * 1000;

/// The tool name of a verification's command.
pub const VERIFICATION_TOOL: &str = "verification";

/// One session's numbering: the next seq, when it started on the
/// executor's clock (unix milliseconds, kept here only), its unsent
/// steps, how many of its reports in a row the service refused as not of
/// the target ([`MAX_DENIALS`]), and whether the reporter gave the
/// session up for that, after which its steps are dropped as they come.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSteps {
    pub next_seq: i64,
    pub started_ms: i64,
    #[serde(default)]
    pub pending: Vec<Step>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub denied: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refused: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(count: &u32) -> bool {
    *count == 0
}

/// The sessions of one target an executor reports.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReporterState {
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionSteps>,
}

/// What a wrapper keeps of its target in the file the control side names
/// ([`TelemetrySinks::steps_file`]): the target, which the supervisor's
/// sweep reads to remove the file once the target ended, and its sessions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeptSteps {
    pub target: StepTarget,
    #[serde(flatten)]
    pub state: ReporterState,
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
    /// How many refusals in a row give a session up ([`MAX_DENIALS`]).
    max_denials: u32,
}

impl StepReporter {
    /// The reporter of `target` going on from `state`, made at `now_ms`.
    pub fn new(target: StepTarget, state: ReporterState, now_ms: i64) -> Self {
        Self {
            target,
            state,
            last_flush_ms: now_ms,
            max_denials: MAX_DENIALS,
        }
    }

    /// The reporter giving a session up at its `max_denials`th refusal in
    /// a row, not at [`MAX_DENIALS`].
    #[must_use]
    pub const fn giving_up_at(mut self, max_denials: u32) -> Self {
        self.max_denials = max_denials;
        self
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
                denied: 0,
                refused: false,
            });
        if kept.refused {
            debug!(
                session,
                "{} steps of session {session}, which the queue service refuses, are dropped",
                steps.len()
            );
            return;
        }
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
    ///
    /// A report the service refused as not of the target ([`ReportDenied`]:
    /// a session not registered to it, or another target's) is that
    /// session's alone: its steps are kept, each refusal is warned of once,
    /// and the other sessions are still sent. At the [`MAX_DENIALS`]th
    /// refusal in a row ([`WRAPPER_MAX_DENIALS`] for a session wrapper's)
    /// the session is given up: one warning names it and
    /// how many of its steps are dropped, and its later steps are dropped
    /// as they come, never sent (a gap the store sees). Any other failure
    /// (a service that did not answer) stops this flush and keeps
    /// everything for the next time, as it may pass then.
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
                        kept.denied = 0;
                        flushed.sent += count;
                    }
                    Err(error) if error.downcast_ref::<ReportDenied>().is_some() => {
                        kept.denied += 1;
                        if kept.denied >= self.max_denials {
                            warn!(
                                session,
                                "the queue service refused the steps of session {session} {} times as not of {} {}: its {} unsent steps are dropped and its later ones are not sent: {error:#}",
                                kept.denied,
                                self.target.kind.as_str(),
                                self.target.id,
                                kept.pending.len()
                            );
                            kept.pending.clear();
                            kept.refused = true;
                        } else {
                            warn!(
                                session,
                                "the steps of session {session} were refused ({} of {}); they are sent again once the session is registered: {error:#}",
                                kept.denied,
                                self.max_denials
                            );
                        }
                        continue 'sessions;
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
/// the reporter of its target, loaded at its start from the file the
/// control side names ([`TelemetrySinks::steps_file`]), and the turn it
/// reads.
#[derive(Default)]
pub struct WrapperSteps {
    reporter: Option<(StepReporter, Option<PathBuf>)>,
    turn: Option<TurnSteps>,
}

impl WrapperSteps {
    /// Go on at `now_ms` with what `kept` keeps of `target`'s sessions,
    /// the file the control side names for it, never one in the run's or
    /// the planner's directory; a file that cannot be read, or that is of
    /// another target, starts afresh, and `None` keeps the steps in the
    /// wrapper alone.
    pub fn load(
        &mut self,
        files: &dyn RunFiles,
        kept: Option<PathBuf>,
        target: StepTarget,
        now_ms: i64,
    ) {
        let state = kept
            .as_deref()
            .and_then(|path| files.read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<KeptSteps>(&text).ok())
            .filter(|kept| kept.target == target)
            .map(|kept| kept.state)
            .unwrap_or_default();
        self.reporter = Some((
            StepReporter::new(target, state, now_ms).giving_up_at(WRAPPER_MAX_DENIALS),
            kept,
        ));
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
        if let Some(path) = path {
            save(files, path, reporter.target(), reporter.state());
        }
    }
}

/// Write `state` of `target` to `path` through a temporary file; a
/// failure is logged: the next wrapper then numbers from what it finds.
fn save(files: &dyn RunFiles, path: &Path, target: &StepTarget, state: &ReporterState) {
    let tmp = path.with_extension("json.tmp");
    let kept = KeptSteps {
        target: target.clone(),
        state: state.clone(),
    };
    let written = serde_json::to_vec(&kept)
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
/// and digests its other words under `known`'s key (none without one), at
/// `known`'s detail (`[telemetry] command_detail`: at `redacted`, its kind,
/// its tool and its time alone).
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
    known.detail().redact(&mut step);
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

    /// The sessions of the kept steps, each with its target: what the
    /// control side registers before it sends them again (a job's whose
    /// session only its output names, read at its end, or whose
    /// registration failed while it ran).
    pub fn sessions(&self) -> Vec<(StepTarget, String)> {
        self.lock()
            .iter()
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
        self.read_rest(now_ms);
        if !self.steps.finish(now_ms) {
            self.unsent.keep(self.steps);
        }
    }

    /// The job ended `now_ms` after its start before the control side
    /// registered its session: the rest of its output is read and its open
    /// steps end as at [`Self::finish`], and nothing is sent; all of it is
    /// kept for the sweep, which registers the session to its target before
    /// it sends the kept steps again
    /// ([`UnsentSteps::sessions`]).
    pub fn keep(mut self, files: &dyn RunFiles, stdout: &Path, now_ms: i64) {
        self.read_on(files, stdout, now_ms);
        self.read_rest(now_ms);
        self.unsent.keep(self.steps);
    }

    /// The last line its output left unended, and the steps its reader
    /// still holds open, ended at `now_ms`; nothing is sent.
    fn read_rest(&mut self, now_ms: i64) {
        if !self.rest.is_empty() {
            let line = std::mem::take(&mut self.rest);
            let steps = self
                .reader
                .line(String::from_utf8_lossy(&line).trim_end(), now_ms);
            self.steps.add(steps);
        }
        let steps = self.reader.finish(now_ms);
        self.steps.add(steps);
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
    /// one the output names) of the queue at `db`, from `clock`'s now. A
    /// session named ahead, which the command registered before its agent
    /// started, is sent every [`REPORT_INTERVAL_SECS`], what failed again
    /// the next time; one only the output names (a Codex thread) is read
    /// and held while the agent runs, as the command registers it only
    /// once its agent ended, and is sent at [`WatchedSteps::finish`], after
    /// that registration: nothing of it is refused before it is the
    /// target's. Its value, and the steps to end.
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
        let registered = session.is_some();
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
                        let now = millis_since(started, clock.monotonic());
                        if registered {
                            steps.look(files, stdout, now);
                        } else {
                            steps.read_on(files, stdout, now);
                        }
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

    /// A sink that records the reports it takes, failing while `down`,
    /// and refusing as the service does (`authorization_denied`) the
    /// sessions in `unregistered`, each refusal counted in `denials`.
    #[derive(Default)]
    struct Sink {
        reports: Mutex<Vec<TelemetryReport>>,
        down: Mutex<bool>,
        unregistered: Mutex<Vec<String>>,
        denials: Mutex<Vec<String>>,
    }

    impl TelemetrySink for Sink {
        fn report(&self, report: &TelemetryReport) -> anyhow::Result<ReportRecorded> {
            if *self.down.lock().unwrap() {
                anyhow::bail!("the queue service at /s is unreachable");
            }
            if self
                .unregistered
                .lock()
                .unwrap()
                .contains(&report.session_id)
            {
                self.denials.lock().unwrap().push(report.session_id.clone());
                return Err(anyhow::Error::new(ReportDenied(
                    "executor-telemetry may not telemetry.report (not on this resource)".into(),
                ))
                .context("the report was not sent"));
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

        fn steps_file(&self, _: &Path, _: &StepTarget) -> Option<PathBuf> {
            Some(PathBuf::from(KEPT))
        }
    }

    /// Where the control side keeps a wrapper's steps in these tests.
    const KEPT: &str = "/q/service/steps/planner-3.json";

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
    /// could not send in the file the control side names for its target,
    /// for the wrapper of a resume.
    #[test]
    fn a_wrappers_steps_go_on_across_turns_and_wrappers() {
        let files = MemoryRunFiles::default();
        let sink = Arc::new(Sink::default());
        let sinks: Arc<Sink> = sink.clone();
        let db = Path::new("/q/queue.db");
        let kept = sinks.steps_file(db, &target());
        let mut steps = WrapperSteps::default();
        steps.load(&files, kept.clone(), target(), 0);
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
        resumed.load(&files, kept, target(), 100_000);
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
        let kept: KeptSteps =
            serde_json::from_str(&files.read_to_string(Path::new(KEPT)).unwrap()).unwrap();
        // The file names its target, for the sweep that removes it.
        assert_eq!(kept.target, target());
        assert_eq!(kept.state.sessions["thread-1"].next_seq, 6);
        assert!(kept.state.sessions["thread-1"].pending.is_empty());
    }

    /// The wrapper of the resume of a run that was interrupted, its kept
    /// file left by the sweep, goes on numbering the same session past the
    /// seqs sent before, so the store takes the new steps rather than
    /// dropping them as repeats, and sends first what was not sent.
    #[test]
    fn the_resume_of_an_interrupted_run_numbers_on_and_resends() {
        let files = MemoryRunFiles::default();
        let sink = Arc::new(Sink::default());
        let sinks: Arc<Sink> = sink.clone();
        let db = Path::new("/q/queue.db");
        let run = StepTarget::run(&crate::domain::RunId::new("r1").unwrap());
        let kept = sinks.steps_file(db, &run);
        let mut steps = WrapperSteps::default();
        steps.load(&files, kept.clone(), run.clone(), 0);
        steps.start_turn(Box::new(Lines), 0, Some("s".to_owned()));
        steps.line("Read", 100);
        steps.tick(&files, Some(&sinks), db, 31_000);
        // The run is interrupted with its last steps unsent.
        *sink.down.lock().unwrap() = true;
        steps.line("Bash", 32_000);
        steps.end_turn(&files, Some(&sinks), db, 33_000);
        let before = sent(&sink);
        let sent_max = before.iter().flat_map(|(_, seqs, _)| seqs).max().copied();
        assert_eq!(sent_max, Some(1));
        *sink.down.lock().unwrap() = false;
        let mut resumed = WrapperSteps::default();
        resumed.load(&files, kept, run, 100_000);
        resumed.start_turn(Box::new(Lines), 100_000, Some("s".to_owned()));
        resumed.line("Edit", 100_100);
        resumed.end_turn(&files, Some(&sinks), db, 100_200);
        let after = &sent(&sink)[before.len()..];
        assert_eq!(
            after,
            [(
                "s".to_owned(),
                vec![2, 3, 4, 5],
                vec![32_000, 33_000, 100_100, 100_200]
            )]
        );
        assert!(after[0].1.iter().all(|seq| Some(*seq) > sent_max));
    }

    /// With no sink the wrapper's turn ends as any other, its steps kept.
    #[test]
    fn a_wrapper_with_no_token_keeps_its_steps() {
        let files = MemoryRunFiles::default();
        let mut steps = WrapperSteps::default();
        steps.load(&files, Some(PathBuf::from(KEPT)), target(), 0);
        steps.start_turn(Box::new(Lines), 0, Some("s".to_owned()));
        steps.line("Read", 10);
        steps.end_turn(&files, None, Path::new("/q/queue.db"), 20);
        let kept: KeptSteps =
            serde_json::from_str(&files.read_to_string(Path::new(KEPT)).unwrap()).unwrap();
        assert_eq!(kept.state.sessions["s"].pending.len(), 2);
    }

    /// Steps an agent put in the run's directory, where a wrapper once
    /// kept them (`turns/steps.json`), are never read or sent with the
    /// executor's token: the wrapper reads only the file the control side
    /// names, and numbers its session afresh.
    #[test]
    fn steps_put_in_the_runs_directory_are_never_sent() {
        let files = MemoryRunFiles::default();
        let sink = Arc::new(Sink::default());
        let sinks: Arc<Sink> = sink.clone();
        let mut planted = Step::new(1, StepKind::ToolCall, 0, 1);
        planted.tool = Some("planted".to_owned());
        let forged = ReporterState {
            sessions: [(
                "forged".to_owned(),
                SessionSteps {
                    next_seq: 2,
                    started_ms: 0,
                    pending: vec![planted],
                    denied: 0,
                    refused: false,
                },
            )]
            .into(),
        };
        files
            .write(
                Path::new("/runs/r1/turns/steps.json"),
                &serde_json::to_vec(&forged).unwrap(),
            )
            .unwrap();
        let db = Path::new("/q/queue.db");
        let mut steps = WrapperSteps::default();
        steps.load(&files, sinks.steps_file(db, &target()), target(), 0);
        steps.start_turn(Box::new(Lines), 0, Some("s".to_owned()));
        steps.line("Read", 10);
        steps.end_turn(&files, Some(&sinks), db, 20);
        assert_eq!(
            sent(&sink),
            vec![("s".to_owned(), vec![1, 2], vec![10, 20])]
        );
        let text = serde_json::to_string(&*sink.reports.lock().unwrap()).unwrap();
        assert!(
            !text.contains("planted") && !text.contains("forged"),
            "{text}"
        );
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

        fn steps_file(&self, _: &Path, _: &StepTarget) -> Option<PathBuf> {
            None
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

    /// The kept steps name, with their targets, the sessions the control
    /// side registers before it sends them again: of every kind of target
    /// (an eval's job's, a run's), and none of steps whose session is not
    /// named.
    #[test]
    fn the_sessions_of_kept_steps_are_listed_with_their_targets() {
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
            unsent.sessions(),
            vec![
                (eval, "eval-session".to_owned()),
                (target(), "planner-session".to_owned())
            ]
        );
    }

    fn step(at: i64) -> Step {
        Step::new(0, StepKind::ToolCall, at * 10, 1)
    }

    /// A session wrapper waits longer for the registration of a thread its
    /// agent named ([`WRAPPER_MAX_DENIALS`]), sending at every interval and
    /// turn's end, and gives it up then all the same; the count goes on
    /// in the file its next wrapper loads.
    #[test]
    fn a_wrapper_gives_a_refused_session_up_at_its_own_bound() {
        let files = MemoryRunFiles::default();
        let sink = Arc::new(Sink::default());
        sink.unregistered.lock().unwrap().push("thread".to_owned());
        let sinks: Arc<Sink> = sink.clone();
        let db = Path::new("/q/queue.db");
        for turn in 0..30_i64 {
            let mut steps = WrapperSteps::default();
            steps.load(
                &files,
                Some(PathBuf::from(KEPT)),
                target(),
                turn * INTERVAL_MS,
            );
            steps.start_turn(
                Box::new(Lines),
                turn * INTERVAL_MS,
                Some("thread".to_owned()),
            );
            steps.line("Read", turn * INTERVAL_MS + 10);
            steps.end_turn(&files, Some(&sinks), db, turn * INTERVAL_MS + 20);
            if turn + 1 < i64::from(WRAPPER_MAX_DENIALS) {
                assert_eq!(denials_of(&sink, "thread"), turn as usize + 1);
            }
        }
        assert_eq!(denials_of(&sink, "thread"), WRAPPER_MAX_DENIALS as usize);
        let kept: KeptSteps =
            serde_json::from_str(&files.read_to_string(Path::new(KEPT)).unwrap()).unwrap();
        assert!(kept.state.sessions["thread"].refused);
        assert!(kept.state.sessions["thread"].pending.is_empty());
    }

    /// An agent that names its own session (`thread-9`), as [`Named`]
    /// reads it.
    struct NamingAgent;

    impl super::super::AgentProvider for NamingAgent {
        fn preflight(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn review_command(
            &self,
            _: &crate::domain::TaskRun,
            _: &Path,
            _: &str,
            _: crate::domain::headless_job::JobAccess,
        ) -> anyhow::Result<crate::application::CommandSpec> {
            unreachable!()
        }
        fn step_reader(&self, _: Option<&str>, _: Subcommands) -> Box<dyn StepReader> {
            Box::new(Named)
        }
    }

    /// A clock whose every look is [`REPORT_INTERVAL_SECS`] past the one
    /// before, so each look of a watch is past its interval, and that tells
    /// `looked` of each look.
    struct Racing {
        now: Mutex<Instant>,
        looked: Mutex<std::sync::mpsc::Sender<()>>,
    }

    impl crate::application::Clock for Racing {
        fn system_time(&self) -> std::time::SystemTime {
            std::time::UNIX_EPOCH
        }
        fn monotonic(&self) -> Instant {
            let mut now = self.now.lock().unwrap();
            *now += std::time::Duration::from_secs(REPORT_INTERVAL_SECS + 1);
            // The test may have stopped waiting for looks.
            let _ = self.looked.lock().unwrap().send(());
            *now
        }
    }

    /// A command's agent that names its own session and runs past many
    /// intervals sends nothing of it while it runs, though its steps came
    /// early: the command registers the session only once the agent ended,
    /// and the steps then go whole at the finish, none refused, none given
    /// up.
    #[test]
    fn a_long_command_sends_its_agents_named_session_only_once_registered() {
        let files = MemoryRunFiles::default();
        let tokens = Arc::new(Tokens::default());
        tokens
            .sink
            .unregistered
            .lock()
            .unwrap()
            .push("thread-9".to_owned());
        let stdout = Path::new("/obs/output.out");
        files.write(stdout, b"Read\nBash\n").unwrap();
        let (looked, looks) = std::sync::mpsc::channel();
        let clock = Racing {
            now: Mutex::new(Instant::now()),
            looked: Mutex::new(looked),
        };
        let watch = CommandSteps {
            files: &files,
            sinks: tokens.clone(),
        };
        let ((), watched) = watch.watch(
            &NamingAgent,
            None,
            (Path::new("/q/queue.db"), target(), None),
            stdout,
            &clock,
            || {
                // The agent runs on until its watch's start and four looks
                // read the clock, each past its interval.
                for _ in 0..5 {
                    looks
                        .recv_timeout(std::time::Duration::from_secs(60))
                        .expect("the watch looks at the agent's stdout");
                }
            },
        );
        assert_eq!(denials_of(&tokens.sink, "thread-9"), 0);
        assert_eq!(sink_count(&tokens.sink), 0);
        // The command registered the thread once its agent ended.
        tokens.sink.unregistered.lock().unwrap().clear();
        watched.finish();
        assert_eq!(denials_of(&tokens.sink, "thread-9"), 0);
        let sent: usize = tokens
            .sink
            .reports
            .lock()
            .unwrap()
            .iter()
            .inspect(|report| assert_eq!(report.session_id, "thread-9"))
            .map(|report| report.steps.len())
            .sum();
        assert_eq!(sent, 3);
    }

    fn denials_of(sink: &Sink, session: &str) -> usize {
        sink.denials
            .lock()
            .unwrap()
            .iter()
            .filter(|denied| *denied == session)
            .count()
    }

    /// A session the service refuses as not of the target (never
    /// registered, or another target's) is sent again at most
    /// [`MAX_DENIALS`] times, each refusal warned of, and then given up:
    /// its steps dropped, its later ones never sent, so the refusals and
    /// their warnings stop. The other sessions of the target are sent all
    /// along.
    #[test]
    fn a_session_the_service_refuses_is_given_up_after_max_denials() {
        let sink = Sink::default();
        sink.unregistered.lock().unwrap().push("foreign".to_owned());
        let mut reporter = StepReporter::new(target(), ReporterState::default(), 0);
        for round in 0..10_i64 {
            reporter.push("foreign", 0, vec![step(round), step(round)]);
            reporter.push("own", 0, vec![step(round)]);
            reporter.flush(Some(&sink), round * INTERVAL_MS);
        }
        assert_eq!(denials_of(&sink, "foreign"), MAX_DENIALS as usize);
        let foreign = &reporter.state().sessions["foreign"];
        assert!(foreign.refused);
        assert!(foreign.pending.is_empty());
        assert_eq!(reporter.pending(), 0);
        let own: usize = sink
            .reports
            .lock()
            .unwrap()
            .iter()
            .inspect(|report| assert_eq!(report.session_id, "own"))
            .map(|report| report.steps.len())
            .sum();
        assert_eq!(own, 10);
        // The give-up outlives the wrapper: a resume's goes on from it.
        let kept: ReporterState =
            serde_json::from_value(serde_json::to_value(reporter.state()).unwrap()).unwrap();
        assert!(kept.sessions["foreign"].refused);
    }

    /// A session registered before its refusals reach [`MAX_DENIALS`] is
    /// sent whole, the steps refused before included, and its count of
    /// refusals starts again.
    #[test]
    fn a_session_registered_after_a_refusal_is_sent_and_its_count_reset() {
        let sink = Sink::default();
        sink.unregistered.lock().unwrap().push("s".to_owned());
        let mut reporter = StepReporter::new(target(), ReporterState::default(), 0);
        reporter.push("s", 0, vec![step(0), step(1)]);
        reporter.flush(Some(&sink), 0);
        assert_eq!(reporter.state().sessions["s"].denied, 1);
        assert_eq!(reporter.pending(), 2);
        sink.unregistered.lock().unwrap().clear();
        reporter.push("s", 0, vec![step(2)]);
        let flushed = reporter.flush(Some(&sink), INTERVAL_MS);
        assert_eq!(flushed, Flushed { sent: 3, kept: 0 });
        let session = &reporter.state().sessions["s"];
        assert_eq!((session.denied, session.refused), (0, false));
    }

    /// Steps a job kept of a session the service refuses are sent again by
    /// the sweeps at most [`MAX_DENIALS`] times, and then go: the sweeps of
    /// every pass do not send them on while the target's token holds.
    #[test]
    fn kept_steps_of_a_refused_session_stop_being_sent_again() {
        let tokens = Arc::new(Tokens::default());
        tokens
            .sink
            .unregistered
            .lock()
            .unwrap()
            .push("review-agent".to_owned());
        let unsent = UnsentSteps::default();
        let mut steps = VerificationSteps::new(
            tokens.clone(),
            PathBuf::from("/q/queue.db"),
            target(),
            Some("review-agent".to_owned()),
        );
        steps.command(
            ("cargo build", &Subcommands::common()),
            (0, 10),
            Some(0),
            None,
        );
        assert!(!steps.finish(20));
        unsent.keep(steps);
        for _ in 0..20 {
            unsent.resend();
        }
        assert_eq!(
            denials_of(&tokens.sink, "review-agent"),
            MAX_DENIALS as usize
        );
        assert_eq!(unsent.resend(), 0);
        assert!(unsent.sessions().is_empty());
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
