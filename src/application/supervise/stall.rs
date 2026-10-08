//! A worker's session idle without a receipt (ADR-0043 decision 1, ADR-0047
//! decision 30): the [`StallWatch`] of the first session nudges it once
//! and, if it stays idle, hands it to a recovery job (`stalled`, reason
//! `idle_without_receipt`); only when the job does not repair it is it
//! raised to the inbox as a `stalled` ask. How each detection ended is
//! recorded (`stall_resolved`, decision 32). The watch also follows the
//! `stalled` ask a recovery job of another alert escalated to.
//!
//! The input marker the agent's hook writes as the session takes an input
//! counts as its last input next to the texts the supervisor typed, and a
//! typed input no send of the supervisor's explains, taken while the session
//! was idle short of the threshold, is recorded as `stall_preempted`
//! (decision 3, task 409).
//!
//! Only the first session ([`SessionWatch`]) needs it. A resumed session
//! and one sent a revise or a conflict request already end their phase
//! when they go idle without the receipt they were asked for (`/exit`, or
//! the `approve_landing` ask), and at the resume timeout otherwise.

use super::*;
use crate::domain::EventKind;
use crate::domain::RunEvent;
use crate::domain::recovery::{
    IDLE_WITHOUT_RECEIPT, PERMISSION_DENIED, SEND_UNCONFIRMED, TURN_WITHOUT_RECEIPT,
};
use crate::domain::run::{
    AttemptOf, AutoRepaired, NewStallNudged, NewStallResolved, RecoveryRecord, StallNudged,
    StallResolved, restore_payload as restore,
};
use crate::domain::turn::{HEADLESS_NUDGES, TurnFailure};

/// The reasons of the `stalled` alert of a session idle without a receipt:
/// a headless session's turn that ended so after its nudges, or refused too
/// many permissions (ADR-t813-1 decision 9), and the retired interactive
/// session's idle after its nudge, which past records still carry (task
/// 1437).
pub(super) const IDLE_REASONS: [&str; 3] = [
    IDLE_WITHOUT_RECEIPT,
    TURN_WITHOUT_RECEIPT,
    PERMISSION_DENIED,
];

/// The notice of a `worker_question` closed without its answer, sent in
/// place of a nudge (task 1372).
pub(super) struct ClosedNotice {
    ask_id: i64,
    text: FittedPrompt,
}

/// How many times a notice of a closed question is sent before the
/// session gets the nudge instead.
const NOTICE_ATTEMPTS: u8 = 3;

/// The notice of a closed question whose send failed: it is recorded only
/// once sent, so it is tried again (after the backend's retry backoff),
/// up to [`NOTICE_ATTEMPTS`].
#[derive(Debug, Clone, Copy)]
pub(super) struct NoticeFailure {
    ask_id: i64,
    failures: u8,
    at: Instant,
}

/// What the watch does with a notice due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoticeStep {
    Send,
    /// Its last send failed a moment ago: nothing is sent this time.
    Wait,
    /// Its sends failed [`NOTICE_ATTEMPTS`] times: the nudge as before.
    GaveUp,
}

/// The step of the notice of the closed question `ask_id`, given its last
/// failed send (`failed`), the backend's retry backoff and the monotonic
/// time `now`.
fn notice_step(
    failed: Option<&NoticeFailure>,
    ask_id: i64,
    backoff: Duration,
    now: Instant,
) -> NoticeStep {
    match failed {
        Some(failed) if failed.ask_id == ask_id && failed.failures >= NOTICE_ATTEMPTS => {
            NoticeStep::GaveUp
        }
        Some(failed)
            if failed.ask_id == ask_id && now.saturating_duration_since(failed.at) < backoff =>
        {
            NoticeStep::Wait
        }
        _ => NoticeStep::Send,
    }
}

/// What one look at a session idle without a receipt found, read before
/// the watch decides ([`StallWatch::idle_step`]).
#[derive(Debug, Clone, Copy)]
struct IdleSeen {
    /// When the wrapper's idle marker, newer than the last request, was
    /// written.
    modified: SystemTime,
    /// The receipt is on disk: a turn that wrote it and ended after the
    /// caller looked for it is no stall (task 1328).
    receipt_written: bool,
    /// A `worker_question` of the run is open: the session waits on it.
    question_open: bool,
    /// A login hold (ADR-0047 decision 42), or an answered hold whose text
    /// to go on is not typed yet, holds the session.
    held: bool,
}

/// What the watch does with an idle session ([`StallWatch::idle_step`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleStep {
    Nothing,
    /// Open the `stalled` ask (after a person's `wait`).
    Ask,
    /// Judge the turn that ended ([`StallWatch::turn_step`]).
    Turn,
}

/// What the last turn of an idle headless session says, read before the
/// watch decides ([`StallWatch::turn_step`]).
#[derive(Debug, Clone, Copy, Default)]
struct TurnSeen {
    /// The turn failed or was stopped: the session ends.
    ended: bool,
    /// Its provider could not be used ([`provider_failure`]).
    wall: Option<TurnFailure>,
    /// Why it goes to its recovery job at once ([`alert_at_once`]).
    at_once: Option<&'static str>,
    /// The step of a notice of a closed question due, if one is.
    notice: Option<NoticeStep>,
}

/// What the watch does with a turn that ended ([`StallWatch::turn_step`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnStep {
    Nothing,
    /// The next call goes to the other provider, or the run waits in the
    /// hold ask (ADR-t813-2).
    Wall(TurnFailure),
    /// Open the `stalled` ask (after a person's `wait`).
    Ask,
    /// Send the notice of a closed question in place of the nudge.
    Notice,
    Nudge,
    /// Hand it to its recovery job, with the alert's reason.
    Recover(&'static str),
}

/// What the watch found of its `stalled` ask ([`StallWatch::ask_step`]).
#[derive(Debug, Clone, Copy)]
enum AskSeen<'a> {
    /// Still open, with its answer if it has one.
    Open(Option<&'a str>),
    /// Someone closed it, with the answer it had.
    Closed(Option<&'a str>),
}

/// What the watch does with its `stalled` ask ([`StallWatch::ask_step`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AskStep {
    Nothing,
    /// Someone closed it answered `intervene`: it is opened again.
    ClosedIntervene,
    /// Someone closed it answered `wait`: the count starts again.
    ClosedWait,
    /// Someone closed it otherwise: a person took the session over.
    ClosedTaken,
    /// The session moved on: it is closed, resolved by itself.
    Moved,
    /// Answered `wait`: closed, and the count starts again.
    Wait,
    /// Answered `intervene`: closed, and opened again.
    Intervene,
    /// Answered `stop`: the session's exit request.
    Stop,
    /// Answered an instruction: the session's next turn.
    Instruction,
}

/// The latest `worker_question` of the run closed without its answer
/// reaching the session and not told to it yet. The headless session has
/// no terminal anyone could type an answer into, and its ask may close
/// before the turn that asked ends, so no turn of it counts against the
/// notice.
fn closed_notice(
    sv: &Supervisor<'_>,
    run: &TaskRun,
    _idle: &IdleMarker,
) -> Result<Option<ClosedNotice>> {
    // Most runs never had a question closed: no events are read for them.
    if sv.queue.last_worker_question_closed(run.id())?.is_none() {
        return Ok(None);
    }
    let events = sv.queue.run_events(run.id())?;
    let Some(closed) = crate::domain::worker_question::closed_undelivered(&events)
        .into_iter()
        .next_back()
    else {
        return Ok(None);
    };
    let ask = sv.queue.read_ask(AskId::new(closed.ask_id))?;
    let closer = closed.closed.actor.as_ref().map(|actor| {
        if actor.id.is_empty() || actor.id == actor.role {
            actor.role.clone()
        } else {
            format!("{} ({})", actor.role, actor.id)
        }
    });
    let text =
        closed_question_notice(run, closed.ask_id, closer.as_deref(), ask.answer.as_deref())?;
    Ok(Some(ClosedNotice {
        ask_id: closed.ask_id,
        text,
    }))
}

/// The setting the idle detections are judged by.
pub(super) const IDLE_THRESHOLD: &str = "idle_without_receipt_secs";

/// Retained when reading historical background recovery events.
pub(super) const BACKGROUND_THRESHOLD: &str = "background_alert_secs";

/// Retained when reading historical worker send recovery events.
pub(super) const SEND_THRESHOLD: &str = "send_confirm_secs";

/// The options of a `stalled` ask: leave the session alone and ask again
/// if it stays idle, or stop its wrapper through the exit request file.
pub(super) const STALLED_OPTIONS: [&str; 2] = ["wait", "stop"];

/// The option a headless session's `stalled` ask has after `wait`, in place
/// of `intervene` ([`headless_options`]): the supervisor has the session end
/// (its exit request), and the run goes on as one that ended without a
/// receipt.
pub(super) const STOP_OPTION: &str = "stop";

/// The options of a `stalled` ask: [`STALLED_OPTIONS`], then those of the
/// recovery job's `note`, each once (ADR-0047 decision 40).
pub(super) fn stalled_options(note: Option<&Note>) -> Vec<String> {
    let mut options: Vec<String> = STALLED_OPTIONS.iter().map(|o| (*o).to_owned()).collect();
    for option in note.map(|note| note.options.as_slice()).unwrap_or_default() {
        if !options.contains(option) {
            options.push(option.clone());
        }
    }
    options
}

/// The answer of a `stalled` ask for a person to step in (ADR-0043), which
/// asks no longer offer: a headless session takes no keys (task 1179). Asks
/// opened before that (the retired interactive session's, and headless ones
/// opened before task 1179) offered it, and an `intervene` that still comes
/// opens the ask again.
const INTERVENE_OPTION: &str = "intervene";

/// Whether `answer` is `intervene` (or `intervene: <text>`).
fn answered_intervene(answer: &str) -> bool {
    answer.trim().starts_with(INTERVENE_OPTION)
}

/// The options of a headless session's `stalled` ask: `options` without
/// `intervene`, and [`STOP_OPTION`] right after `wait` (task 1179).
pub(super) fn headless_options(options: Vec<String>) -> Vec<String> {
    let mut options: Vec<String> = options
        .into_iter()
        .filter(|o| o != INTERVENE_OPTION)
        .collect();
    if !options.iter().any(|o| o == STOP_OPTION) {
        let at = options
            .iter()
            .position(|o| o == "wait")
            .map_or(0, |i| i + 1);
        options.insert(at, STOP_OPTION.to_owned());
    }
    options
}

/// The answers the runtime writes into a `stalled` ask it closes.
pub(super) const STALL_MOVED_CLOSED: &str = "the session moved on; closed by the runtime";

/// Whether a `stalled` ask's answer leaves the session alone
/// ([`crate::domain::stats::thresholds::answer_waits`]).
fn answered_wait(answer: Option<&str>) -> bool {
    answer.is_some_and(crate::domain::stats::thresholds::answer_waits)
}

/// Whether the supervisor delivers `answer` of a headless session's
/// `stalled` ask from the run's slot: `stop` (the exit request) or an
/// instruction (the next turn), not `wait`, `propose` or `intervene`.
pub(super) fn headless_delivers(answer: &str) -> bool {
    !answered_wait(Some(answer)) && !answered_intervene(answer)
}

pub(super) const STALL_EXITED_CLOSED: &str = "the session exited; closed by the runtime";

/// The phase whose idle the watch judges.
const PHASE: &str = "session";

/// The nudge sent in this phase (at most one).
#[derive(Debug, Clone, Copy)]
struct Nudge {
    /// When it was recorded, on the files' wall clock.
    at: SystemTime,
    /// How long the session had been idle then.
    detected_after_secs: i64,
    /// Its `stall_resolved` is recorded.
    settled: bool,
}

/// The recovery job of this phase's idle after its nudge (ADR-0047
/// decision 30) whose end is not recorded yet.
#[derive(Debug, Clone, Copy)]
struct Recovering {
    attempt: usize,
    /// The alert's reason: one of [`IDLE_REASONS`].
    reason: &'static str,
    /// When it was requested, on the files' wall clock.
    at: SystemTime,
    /// How long the session had been idle then.
    detected_after_secs: i64,
    /// When its repair was applied: the session moving after this is the
    /// repair's doing.
    repaired_at: Option<SystemTime>,
}

impl Recovering {
    /// Whether the session moved since the job started (or since its
    /// repair): an idle marker written in a later millisecond than the
    /// event (task 1050).
    fn moved(&self, marker: Option<SystemTime>) -> bool {
        let since = self.repaired_at.unwrap_or(self.at);
        marker.is_some_and(|m| written_after(m, since))
    }
}

/// The `stalled` ask of this phase that nobody closed.
#[derive(Debug, Clone, Copy)]
struct Asked {
    id: AskId,
    /// When it was opened: an idle marker newer than this is a turn the
    /// session took since.
    at: SystemTime,
    detected_after_secs: i64,
    /// Its answer is applied (its `stall_resolved` is recorded).
    applied: bool,
    /// The setting of the detection it came from: [`IDLE_THRESHOLD`], or
    /// `background_alert_secs` / `idle_process_secs` for a recovery job's
    /// escalation.
    threshold: &'static str,
}

/// A headless session's `stalled` ask answered `intervene` and closed,
/// whose ask is opened again (task 1179).
#[derive(Debug, Clone, Copy)]
struct Reopen {
    /// The ask answered `intervene`.
    previous: AskId,
    /// When it was opened.
    at: SystemTime,
    detected_after_secs: i64,
    threshold: &'static str,
}

/// Why the session's watch parks its run for a session of its own.
#[derive(Debug, Clone)]
enum Park {
    /// A recovery job's `resume`, with its instruction.
    Resume(String),
}

/// Receipt-less idle of one session, judged each tick.
#[derive(Debug, Clone, Default)]
pub(super) struct StallWatch {
    nudge: Option<Nudge>,
    /// How many turn nudges the phase sent, up to [`HEADLESS_NUDGES`].
    nudges: u8,
    /// The latest text the supervisor typed into the session: a marker no
    /// newer is not the end of a turn that answered it.
    last_input: Option<SystemTime>,
    asked: Option<Asked>,
    /// The answer `wait` restarts the count here.
    wait_from: Option<SystemTime>,
    /// After `intervene` (or an ask a person closed), no ask until the
    /// session ends a turn after this.
    held: Option<SystemTime>,
    /// A headless session's `stalled` ask answered `intervene`, closed, to
    /// be opened again with the headless options (task 1179).
    reopen: Option<Reopen>,
    /// The recovery job of the idle after the nudge, until its end is
    /// recorded.
    /// Boxed: rarely set, and the watch is part of every session's phase.
    recovering: Option<Box<Recovering>>,
    /// A recovery job's repair (other than `wait`) restarts the count here.
    recovered_from: Option<SystemTime>,
    /// A recovery job's `resume` repair (task 442), or a move to headless
    /// Codex (ADR-t813-2 decision 5), with its instruction: the session's
    /// watch parks the run as `needs_session`. Boxed: rarely set.
    park: Option<Box<Park>>,
    /// The last observation followed the idle's recovery job: one that
    /// did not (a dialog, a question, a hold or an input came first) leaves
    /// the job nobody to act on, so the session's watch stops it.
    followed: bool,
}

/// The idle the watch hands to a recovery job, and how the watch reaches
/// it: the session's recovery and what it offers the job.
pub(super) struct StallRecovery<'a, 'b> {
    pub(super) recovery: &'a mut RecoveryWatch,
    pub(super) live: &'a Live<'b>,
}

/// Seconds from `from` to `to`, zero when `to` is earlier: what events
/// record. The checks compare [`elapsed`] with their threshold, which a
/// test may set below a second (task 1045).
fn secs_between(from: SystemTime, to: SystemTime) -> i64 {
    i64::try_from(elapsed(from, to).as_secs()).unwrap_or(i64::MAX)
}

/// The time from `from` to `to`, zero when `to` is earlier.
pub(super) fn elapsed(from: SystemTime, to: SystemTime) -> Duration {
    to.duration_since(from).unwrap_or_default()
}

/// The run's `stalled` ask `id`, or its latest one, closed or not.
fn stalled_ask(
    queue: &dyn Queue,
    run: &RunId,
    id: Option<AskId>,
) -> Result<Option<crate::domain::Ask>> {
    Ok(queue
        .asks(crate::application::AskQuery {
            all: true,
            ..Default::default()
        })?
        .into_iter()
        .rfind(|ask| {
            ask.kind == AskKind::Stalled
                && ask.run_id.as_ref() == Some(run)
                && id.is_none_or(|id| ask.id == id)
        }))
}

/// Whether the end of the `stalled` ask `id` of the run is recorded.
fn ask_resolved(queue: &dyn Queue, run: &TaskRun, id: AskId) -> Result<bool> {
    Ok(ask_resolved_in(&queue.run_events(run.id())?, id))
}

/// Whether `events` record the end of the `stalled` ask `id`.
fn ask_resolved_in(events: &[RunEvent], id: AskId) -> bool {
    events
        .iter()
        .filter_map(resolved_of)
        .any(|r| r.detection == Some("ask") && r.ask_id == Some(id.as_i64()))
}

/// The `stall_resolved` of this watch's phase that `event` is, if it is one.
fn resolved_of(event: &RunEvent) -> Option<StallResolved<'_>> {
    (event.kind == event_kind::STALL_RESOLVED)
        .then(|| restore::<StallResolved>(&event.payload))
        .filter(|r| r.phase == Some(PHASE))
}

/// The `stall_nudged` of this watch's phase that `event` is, if it is one.
fn nudged_of(event: &RunEvent) -> Option<StallNudged<'_>> {
    (event.kind == event_kind::STALL_NUDGED)
        .then(|| restore::<StallNudged>(&event.payload))
        .filter(|n| n.phase == Some(PHASE))
}

/// The recovery record of the idle that `event` is (any kind), if it is
/// one: its alert is `stalled` and its reason one of [`IDLE_REASONS`].
fn idle_job(event: &RunEvent) -> Option<RecoveryRecord<'_>> {
    let record = restore::<RecoveryRecord>(&event.payload);
    (record.alert == Some(RecoveryAlert::Stalled.as_str())
        && IDLE_REASONS
            .iter()
            .any(|reason| record.reason == Some(*reason)))
    .then_some(record)
}

/// The setting the alert escalated to the `stalled` ask `id` was judged
/// by: a recovery job's escalation names its ask (ADR-0047), and its alert
/// the setting; [`IDLE_THRESHOLD`] for the watch's own ask.
fn threshold_of(events: &[RunEvent], id: AskId) -> &'static str {
    let recovery = events
        .iter()
        .filter(|e| e.kind == event_kind::RECOVERY_FINISHED)
        .map(|e| restore::<RecoveryRecord>(&e.payload))
        .find(|r| r.ask_id == Some(id.as_i64()));
    match recovery {
        Some(r) if r.alert == Some(RecoveryAlert::IdleProcess.as_str()) => IDLE_PROCESS_THRESHOLD,
        Some(r) if r.alert == Some(RecoveryAlert::LongBackground.as_str()) => BACKGROUND_THRESHOLD,
        Some(r) if r.reason == Some(SEND_UNCONFIRMED) => SEND_THRESHOLD,
        _ => IDLE_THRESHOLD,
    }
}

/// Open a `stalled` ask of the run to the inbox.
fn new_stalled_ask(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    question: String,
    options: Vec<String>,
    reason_category: AskReason,
) -> Result<AskId> {
    let outcome = ask::ask(
        &mut *sv.queue,
        NewAsk {
            recommendation: None,
            confidence: None,
            kind: AskKind::Stalled,
            task_id: Some(run.task_id()),
            run_id: Some(run.id().clone()),
            question,
            options,
            asked_by: SessionRole::Supervisor.as_str().into(),
            reason_category,
            topics: Vec::new(),
            finding_id: None,
            request_id: None,
        },
    )?;
    let id = AskId::new(outcome["id"].as_i64().context("ask returned no id")?);
    info!(ask_id = %id, run_id = %run.id(), "stalled ask {id} of {} opened", run.id());
    Ok(id)
}

/// What the `stalled` ask says of the nudge before it.
fn nudged_text(nudge: Option<Nudge>, now: SystemTime) -> String {
    nudge.map_or_else(
        || "although its turns were refused permissions too often to get on".to_owned(),
        |nudge| {
            format!(
                "although the supervisor nudged it {}s ago to write its receipt, ask a worker_question or run again in the foreground what it ended the turn to wait for",
                secs_between(nudge.at, now)
            )
        },
    )
}

/// What `wait` does to a headless session's ask of its idle: the ask
/// follows its next turn.
const WAIT_ASKS_AFTER_TURN: &str = " (the supervisor asks again after its next turn)";

/// Where the answers of a `stalled` ask's question start, after what it
/// says of the session: a headless one's, and one of the retired
/// interactive session (or opened before task 1179).
const ANSWERS_START: [&str; 2] = ["Before answering, read what its turns did", "Answer `wait`"];

/// The sentence of an ask opened again for an `intervene` answer, up to
/// its end.
const REOPENED_FROM: &str = " Its stalled ask ";
const REOPENED_TO: &str = "so it is asked again.";

/// The question of a headless session's `stalled` ask: `situation` (what
/// the session did, and a recovery job's diagnosis), then its answers,
/// then its last turns (`turns`). The session has no screen and takes no
/// keys (ADR-t813-1 decision 4): a person reads what its turns did before
/// answering, and an instruction is its next turn; there is no
/// `intervene` (task 1179). `wait` says what `wait` does besides leaving
/// the session alone.
pub(super) fn headless_ask_text(
    run_id: &RunId,
    situation: &str,
    wait: &str,
    turns: &str,
) -> String {
    format!(
        "{situation} Before answering, read what its turns did: their last lines are below, and the run's `turns/` directory and `dagq timeline {run_id}` have the rest (see the dagq-recover skill). Answer `wait` to leave the session alone{wait}, or `stop` to have the supervisor end the session (its exit request; the run then ends without a receipt and goes to its recovery job). Any other text is sent as the session's next turn: a headless session takes no keys, so an instruction is given this way. Answer `propose` (or `propose: <why>`) to have a planner of the runtime's propose a remedy for its cause; the session is then left alone as for `wait`. This ask closes itself once the session moves on.\n\n{turns}"
    )
}

/// What the question of a `stalled` ask says of the session, before its
/// answers, without the sentence of an earlier `intervene`: `None` when
/// it has no answers the watch knows.
fn situation_of(question: &str) -> Option<String> {
    let at = ANSWERS_START
        .iter()
        .filter_map(|start| question.find(start))
        .min()?;
    let mut situation = question[..at].trim_end().to_owned();
    if let Some(from) = situation.find(REOPENED_FROM)
        && let Some(len) = situation[from..].find(REOPENED_TO)
    {
        situation.replace_range(from..from + len + REOPENED_TO.len(), "");
    }
    Some(situation)
}

/// The question of a headless session's `stalled` ask of its idle.
fn headless_question(
    sv: &Supervisor<'_>,
    run: &TaskRun,
    idle_secs: i64,
    nudged: &str,
    recovered: &str,
) -> String {
    let situation = format!(
        "The headless session of run {run_id} (task {task_id}) ended its turn without a receipt or an open question ({idle_secs}s ago, phase: {PHASE}), {nudged}.{recovered}",
        run_id = run.id(),
        task_id = run.task_id(),
    );
    headless_ask_text(
        run.id(),
        &situation,
        WAIT_ASKS_AFTER_TURN,
        &turns_excerpt(sv, run),
    )
}

/// Whether the idle marker `marker` shows a turn the session took since
/// `since` (a `stalled` ask opened, a person stepped in): written in a later
/// millisecond (task 1050).
fn marker_moved(marker: Option<SystemTime>, since: SystemTime) -> bool {
    marker.is_some_and(|m| written_after(m, since))
}

fn at_unix(secs: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).unwrap_or(0))
}

use super::file_time::{event_time as at_event, written_after};

impl StallWatch {
    /// Rebuild the watch of an adopted run from its events and its
    /// `stalled` asks ([`Self::adopted`]).
    pub(super) fn adopt(queue: &dyn Queue, run: &TaskRun) -> Result<Self> {
        let events = queue.run_events(run.id())?;
        let latest = stalled_ask(queue, run.id(), None)?;
        let unclosed = queue.unclosed_stalled_ask(run.id())?;
        Ok(Self::adopted(&events, latest.as_ref(), unclosed.as_ref()))
    }

    /// The watch of an adopted run, from its `events` (oldest first), its
    /// latest `stalled` ask (`latest`, closed or not) and the one nobody
    /// closed (`unclosed`), so a nudge or an ask is never repeated.
    pub(super) fn adopted(
        events: &[RunEvent],
        latest: Option<&crate::domain::Ask>,
        unclosed: Option<&crate::domain::Ask>,
    ) -> Self {
        let resolved = |detection: &str, ask: Option<AskId>| {
            events.iter().filter_map(resolved_of).any(|r| {
                r.detection == Some(detection) && ask.is_none_or(|id| r.ask_id == Some(id.as_i64()))
            })
        };
        let mut watch = Self {
            nudges: events
                .iter()
                .filter_map(nudged_of)
                .count()
                .try_into()
                .unwrap_or(u8::MAX),
            ..Self::default()
        };
        if let Some((event, nudged)) = events
            .iter()
            .rev()
            .find_map(|e| nudged_of(e).map(|n| (e, n)))
        {
            let at = at_event(event).unwrap_or(UNIX_EPOCH);
            watch.nudge = Some(Nudge {
                at,
                detected_after_secs: nudged.idle_secs.unwrap_or(0),
                settled: resolved("nudge", None),
            });
            watch.input_sent(at, None);
        }
        // An answer the previous supervisor typed is an input too, and so
        // is every request it wrote for a headless session's next turn.
        for at in events
            .iter()
            .filter(|e| e.kind == event_kind::ASK_DELIVERED || e.kind == event_kind::TURN_REQUESTED)
            .filter_map(at_event)
        {
            watch.input_sent(at, None);
        }
        let latest_outcome = |outcome: &str| {
            events
                .iter()
                .rev()
                .find(|e| resolved_of(e).is_some_and(|r| r.outcome == Some(outcome)))
                .and_then(at_event)
        };
        // A person stepped in, or had the session stopped, on an ask closed
        // since. A headless session's `intervene` is no step in: its ask
        // is opened again instead (task 1179).
        watch.held = latest_outcome("answered_stop");
        watch.wait_from = latest_outcome("answered_wait");
        // An instruction a recovery job had typed is an input too, and its
        // repair restarted the count.
        for at in events
            .iter()
            .filter(|e| {
                e.kind == event_kind::AUTO_REPAIRED
                    && restore::<AutoRepaired>(&e.payload).repair == Some("send_instruction")
            })
            .filter_map(at_event)
        {
            watch.input_sent(at, None);
        }
        let idle_jobs = || {
            events
                .iter()
                .filter_map(|e| idle_job(e).map(|record| (e, record)))
        };
        watch.recovered_from = idle_jobs()
            .rfind(|(e, record)| e.kind == event_kind::RECOVERY_FINISHED && record.repaired())
            .and_then(|(e, _)| at_event(e));
        // The idle's recovery job the previous supervisor requested and
        // whose end is not recorded: a job it left running is gone (the
        // adopter starts another, counted as one more), and the session
        // moving on ends it as this watch's own would.
        if let Some((requested, record)) =
            idle_jobs().rfind(|(e, _)| e.kind == event_kind::RECOVERY_REQUESTED)
        {
            let attempt = record.attempt.unwrap_or(0) as usize;
            let of_attempt =
                |e: &RunEvent| restore::<AttemptOf>(&e.payload).attempt == Some(attempt as u64);
            let ended = events
                .iter()
                .filter(|e| of_attempt(e))
                .filter_map(resolved_of)
                .any(|r| r.detection == Some("recovery"));
            if !ended {
                let applied = idle_jobs()
                    .filter(|(e, _)| of_attempt(e))
                    .find(|(e, record)| {
                        e.kind == event_kind::RECOVERY_FINISHED && record.repaired()
                    });
                let reason = IDLE_REASONS
                    .into_iter()
                    .find(|reason| record.reason == Some(*reason))
                    .unwrap_or(IDLE_WITHOUT_RECEIPT);
                watch.recovering = Some(Box::new(Recovering {
                    attempt,
                    reason,
                    at: at_event(requested).unwrap_or(UNIX_EPOCH),
                    detected_after_secs: record.idle_secs.unwrap_or(0),
                    repaired_at: applied.and_then(|(e, _)| at_event(e)),
                }));
            }
            if let Some(nudge) = &mut watch.nudge {
                nudge.settled = true;
            }
        }
        // Times in the ask are whole seconds: a marker in the same second
        // is taken as older. The latest ask closed without its outcome
        // recorded was closed while no supervisor watched: its `wait`
        // counts again from its close, anything else holds.
        if let Some(ask) = latest
            && let Some(closed) = ask.closed_at
            && !resolved("ask", Some(ask.id))
        {
            if answered_wait(ask.answer.as_deref()) {
                watch.wait_from = watch.wait_from.max(Some(at_unix(closed)));
            } else if ask.answer.as_deref().is_some_and(answered_intervene) {
                // A headless session's `intervene` holds nothing: its ask
                // is opened again (task 1179).
                watch.reopen = Some(Reopen {
                    previous: ask.id,
                    at: at_unix(ask.created_at + 1),
                    detected_after_secs: 0,
                    threshold: threshold_of(events, ask.id),
                });
            } else {
                watch.held = watch.held.max(Some(at_unix(closed + 1)));
            }
            if let Some(nudge) = &mut watch.nudge {
                nudge.settled = true;
            }
        }
        if let Some(ask) = unclosed {
            // A headless session's `intervene` is never applied: its watch
            // closes the ask and opens it again, recording its outcome only
            // if the previous supervisor did not (task 1179).
            let reopens = ask.answer.as_deref().is_some_and(answered_intervene);
            let applied = !reopens && ask.answered_at.is_some() && resolved("ask", Some(ask.id));
            watch.asked = Some(Asked {
                id: ask.id,
                at: at_unix(ask.created_at + 1),
                detected_after_secs: 0,
                applied,
                threshold: threshold_of(events, ask.id),
            });
            if applied {
                watch.held = watch.held.max(ask.answered_at.map(|at| at_unix(at + 1)));
            }
            // An ask follows the nudge, which escalated to it.
            if let Some(nudge) = &mut watch.nudge {
                nudge.settled = true;
            }
        } else if let Some(at) = events.iter().rposition(|e| {
            resolved_of(e)
                .is_some_and(|r| r.detection == Some("ask") && r.reopened == Some(true))
        }) && let event = &events[at]
            && let Some(reopened) = resolved_of(event)
            // Nothing was sent to the session since: an ask opened again
            // then settled by a receipt or a question is not opened again.
            && !events[at..].iter().any(|e| {
                e.kind == event_kind::TURN_REQUESTED
                    || e.kind == event_kind::ASK_DELIVERED
                    || e.kind == event_kind::STALL_NUDGED
            })
            && let Some(previous) = reopened.ask_id.map(AskId::new)
            && latest.is_some_and(|ask| ask.id == previous && ask.closed_at.is_some())
        {
            // The previous supervisor closed an ask answered `intervene`
            // and stopped before it opened the next one.
            watch.reopen = Some(Reopen {
                previous,
                at: at_event(event).unwrap_or(UNIX_EPOCH),
                detected_after_secs: reopened.detected_after_secs.unwrap_or(0),
                threshold: threshold_of(events, previous),
            });
            if let Some(nudge) = &mut watch.nudge {
                nudge.settled = true;
            }
        }
        watch
    }

    /// The supervisor typed `text` (when known) into the session at `at`.
    pub(super) fn input_sent(&mut self, at: SystemTime, _text: Option<&str>) {
        self.last_input = Some(self.last_input.map_or(at, |last| last.max(at)));
    }

    /// Whether the idle marker written at `modified` ended a turn after the
    /// last text the supervisor typed and after a person stepped in (the
    /// times an adopter reads from events, to the millisecond: a marker of
    /// their millisecond is from before them, task 1050).
    fn ended_after_inputs(&self, modified: SystemTime) -> bool {
        self.last_input.is_none_or(|at| written_after(modified, at))
            && self.held.is_none_or(|at| written_after(modified, at))
    }

    /// After a person's `wait`, whether the idle marker written at
    /// `modified` is of a turn after the answer; `None` without one.
    fn after_wait(&self, modified: SystemTime) -> Option<bool> {
        self.wait_from.map(|wait| written_after(modified, wait))
    }

    /// A recovery job escalated the alert of `threshold` to the `stalled`
    /// ask `id` (ADR-0047 decision 40): the watch follows it like its own,
    /// closing it once the session moves on and applying its answer.
    pub(super) fn escalated(
        &mut self,
        id: AskId,
        at: SystemTime,
        detected_after_secs: i64,
        threshold: &'static str,
    ) {
        self.asked = Some(Asked {
            id,
            at,
            detected_after_secs,
            applied: false,
            threshold,
        });
    }

    /// Record how a detection ended, once.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn resolved(
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        detection: &str,
        ask: Option<(AskId, &'static str)>,
        detected_after_secs: i64,
        detected_at: SystemTime,
        outcome: &str,
    ) -> Result<()> {
        let threshold = ask.map_or(IDLE_THRESHOLD, |(_, threshold)| threshold);
        let payload = Self::resolved_payload(
            sv,
            detection,
            threshold,
            detected_after_secs,
            detected_at,
            outcome,
        );
        Self::record_resolved(sv, run, payload, ask.map(|(id, _)| id), detection, outcome)
    }

    /// The payload of a `stall_resolved`.
    pub(super) fn resolved_payload<'a>(
        sv: &Supervisor<'_>,
        detection: &'a str,
        threshold: &'static str,
        detected_after_secs: i64,
        detected_at: SystemTime,
        outcome: &'a str,
    ) -> NewStallResolved<'a> {
        NewStallResolved {
            phase: PHASE,
            detection,
            threshold,
            threshold_secs: match threshold {
                BACKGROUND_THRESHOLD => sv.stall.background_alert_secs,
                IDLE_PROCESS_THRESHOLD => sv.stall.idle_process_secs,
                SEND_THRESHOLD => sv.stall.send_confirm_secs,
                _ => sv.stall.idle_without_receipt_secs,
            },
            detected_after_secs,
            outcome,
            resolved_after_secs: secs_between(detected_at, sv.files.now()),
            ask_id: None,
            attempt: None,
            reason: None,
            reopened: None,
        }
    }

    /// Record `payload` as `stall_resolved`, naming the ask when there is
    /// one.
    pub(super) fn record_resolved(
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        payload: NewStallResolved<'_>,
        ask: Option<AskId>,
        detection: &str,
        outcome: &str,
    ) -> Result<()> {
        let payload = NewStallResolved {
            ask_id: ask.map(AskId::as_i64),
            ..payload
        };
        sv.queue.record_runtime_event(
            run.id(),
            EventKind::StallResolved,
            serde_json::to_value(payload)?,
        )?;
        info!(run_id = %run.id(), "stall of {} ({detection}) ended: {outcome}", run.id());
        Ok(())
    }

    /// Whether a recovery job of the idle waits for its end to be recorded:
    /// the job may run only then.
    pub(super) fn recovering(&self) -> bool {
        self.recovering.is_some()
    }

    /// Whether the last observation followed the idle's recovery job.
    pub(super) fn followed(&self) -> bool {
        self.followed
    }

    /// The `resume` a recovery job chose, once.
    pub(super) fn take_park(&mut self) -> Option<String> {
        match self.park.take().map(|park| *park) {
            Some(Park::Resume(instruction)) => Some(instruction),
            other => {
                self.park = other.map(Box::new);
                None
            }
        }
    }

    /// The run is parked for a session of its own by the idle's recovery
    /// job: the job's detection ended with its repair.
    pub(super) fn parked(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        self.recovery_resolved(sv, run, "resolved_by_recovery")
    }

    /// The recovery job `attempt` of the idle started: a job before it, and
    /// the nudge, went on to it.
    fn recovery_started(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        attempt: usize,
        reason: &'static str,
        idle_secs: i64,
        now: SystemTime,
    ) -> Result<()> {
        self.recovery_resolved(sv, run, "escalated")?;
        self.nudge_escalated(sv, run)?;
        self.recovering = Some(Box::new(Recovering {
            attempt,
            reason,
            at: now,
            detected_after_secs: idle_secs,
            repaired_at: None,
        }));
        Ok(())
    }

    /// The nudge not settled yet went on to the next detection.
    fn nudge_escalated(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        if let Some(nudge) = &mut self.nudge
            && !nudge.settled
        {
            nudge.settled = true;
            let nudge = *nudge;
            Self::resolved(
                sv,
                run,
                "nudge",
                None,
                nudge.detected_after_secs,
                nudge.at,
                "escalated",
            )?;
        }
        Ok(())
    }

    /// Record how the idle's recovery job ended (`detection: recovery`),
    /// once.
    fn recovery_resolved(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        outcome: &str,
    ) -> Result<()> {
        let Some(recovering) = self.recovering.take() else {
            return Ok(());
        };
        let payload = NewStallResolved {
            attempt: Some(recovering.attempt),
            reason: Some(recovering.reason),
            ..Self::resolved_payload(
                sv,
                "recovery",
                IDLE_THRESHOLD,
                recovering.detected_after_secs,
                recovering.at,
                outcome,
            )
        };
        Self::record_resolved(sv, run, payload, None, "recovery", outcome)
    }

    /// The outcome of the idle's recovery job once the session moved: the
    /// repair's doing when one was applied.
    fn moved_outcome(&self) -> &'static str {
        if self
            .recovering
            .as_ref()
            .is_some_and(|r| r.repaired_at.is_some())
        {
            "resolved_by_recovery"
        } else {
            "resolved_by_itself"
        }
    }

    /// The session wrote its receipt (`receipt`) or asked a
    /// `worker_question`: a nudge not settled yet resolved it, and a
    /// `stalled` ask is closed (resolved by itself if nobody answered it).
    pub(super) fn settle(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        receipt: bool,
    ) -> Result<()> {
        if self.nudge.is_none_or(|n| n.settled)
            && self.asked.is_none()
            && self.reopen.is_none()
            && self.recovering.is_none()
        {
            return Ok(());
        }
        if !receipt && !sv.queue.has_unclosed_worker_question(run.id())? {
            return Ok(());
        }
        let outcome = self.moved_outcome();
        self.recovery_resolved(sv, run, outcome)?;
        self.reopen = None;
        if let Some(nudge) = &mut self.nudge
            && !nudge.settled
        {
            nudge.settled = true;
            let nudge = *nudge;
            Self::resolved(
                sv,
                run,
                "nudge",
                None,
                nudge.detected_after_secs,
                nudge.at,
                "resolved_by_nudge",
            )?;
        }
        self.close(sv, run, STALL_MOVED_CLOSED, "resolved_by_itself")
    }

    /// The session ended: whatever is not settled ends with the run.
    pub(super) fn ended(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        if let Some(nudge) = &mut self.nudge
            && !nudge.settled
        {
            nudge.settled = true;
            let nudge = *nudge;
            Self::resolved(
                sv,
                run,
                "nudge",
                None,
                nudge.detected_after_secs,
                nudge.at,
                "run_ended",
            )?;
        }
        self.recovery_resolved(sv, run, "run_ended")?;
        self.reopen = None;
        self.close(sv, run, STALL_EXITED_CLOSED, "run_ended")
    }

    /// Close the run's `stalled` ask, recording `outcome` for one whose
    /// answer was not applied.
    fn close(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        answer: &str,
        outcome: &str,
    ) -> Result<()> {
        let Some(asked) = self.asked.take() else {
            return Ok(());
        };
        for ask in sv.queue.close_stalled_asks(run.id(), answer)? {
            info!(ask_id = %ask.id, run_id = %run.id(), "closed the stalled ask {} of {}: {answer}", ask.id, run.id());
        }
        if !asked.applied {
            Self::resolved(
                sv,
                run,
                "ask",
                Some((asked.id, asked.threshold)),
                asked.detected_after_secs,
                asked.at,
                outcome,
            )?;
        }
        Ok(())
    }

    /// One observation of a session with a fresh wrapper, no receipt and no
    /// exit requested. Returns the queued request time.
    pub(super) fn poll(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle_marker: &Path,
        recovery: StallRecovery<'_, '_>,
    ) -> Result<Option<SystemTime>> {
        self.observe(sv, run, workspace, idle_marker, Some(recovery))
    }

    /// The same observation for a run that waits for a person outside
    /// its slot (ADR-0062 decision 6): the `stalled` ask is followed and
    /// its answer applied, and one more ask opens after a `wait`, but
    /// nothing is sent to the session (no nudge, no key to a dialog) and no
    /// recovery job starts.
    pub(super) fn poll_quiet(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle_marker: &Path,
    ) -> Result<()> {
        self.observe(sv, run, workspace, idle_marker, None)
            .map(|_| ())
    }

    /// One observation; `recovery` is `None` for a run out of its slot,
    /// which is sent nothing.
    fn observe(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle_marker: &Path,
        recovery: Option<StallRecovery<'_, '_>>,
    ) -> Result<Option<SystemTime>> {
        self.followed = false;
        let now = sv.files.now();
        // Only a wrapper idle marker newer than the last request counts.
        let idle = sv.session_idle(idle_marker)?;
        let marker = idle.as_ref().map(IdleMarker::modified);
        // The session moved since the idle's recovery job started (or since
        // its repair): the job's detection ended, and a job still running
        // is stopped by the session's watch ([`Self::recovering`]).
        if self
            .recovering
            .as_deref()
            .is_some_and(|recovering| recovering.moved(marker))
        {
            let outcome = self.moved_outcome();
            self.recovery_resolved(sv, run, outcome)?;
        }
        if self.asked.is_some() || self.reopen.is_some() {
            self.watch_ask(sv, run, marker, now, recovery.is_some())?;
            return Ok(None);
        }
        let Some(idle) = idle else {
            return Ok(None);
        };
        let mut seen = IdleSeen {
            modified: idle.modified(),
            // The caller looked for the receipt before the idle was read.
            receipt_written: run
                .receipt_path()
                .is_some_and(|receipt| sv.files.is_file(Path::new(receipt))),
            question_open: false,
            held: false,
        };
        // The question and the hold are read only when they could stop the
        // step: they only ever make it nothing.
        let mut step = self.idle_step(&seen, recovery.is_some());
        if step != IdleStep::Nothing {
            seen.question_open = sv.queue.has_unclosed_worker_question(run.id())?;
            if !seen.question_open {
                seen.held =
                    sv.queue.hold_unclosed(run.id())? || sv.hold_continue.contains_key(run.id());
            }
            step = self.idle_step(&seen, recovery.is_some());
        }
        let idle_secs = secs_between(seen.modified, now);
        match (step, recovery) {
            (IdleStep::Ask, _) => {
                self.open_ask(sv, run, workspace, &idle, idle_secs, self.nudge, now, None)?;
                Ok(None)
            }
            (IdleStep::Turn, Some(recovery)) => self.observe_turn(
                sv,
                run,
                workspace,
                idle_marker,
                &idle,
                idle_secs,
                now,
                recovery,
            ),
            (IdleStep::Nothing | IdleStep::Turn, _) => Ok(None),
        }
    }

    /// What the watch does with a session idle without a receipt, as
    /// `seen`, when no `stalled` ask is followed: nothing while the
    /// receipt is written, the session has not ended a turn since the last
    /// text it was sent or since a person stepped in, it waits on its
    /// question or a hold holds it. Out of its slot (`in_slot` false)
    /// nothing is typed and no job starts: a stall before its nudge, or
    /// one for a recovery job, waits for the slot, and after a person's
    /// `wait` the ask follows the session's next turn. In its slot the
    /// turn is judged.
    fn idle_step(&self, seen: &IdleSeen, in_slot: bool) -> IdleStep {
        if seen.receipt_written
            || !self.ended_after_inputs(seen.modified)
            || seen.question_open
            || seen.held
        {
            return IdleStep::Nothing;
        }
        if in_slot {
            return IdleStep::Turn;
        }
        if self.nudge.is_none()
            || self.wait_from.is_none()
            || self.after_wait(seen.modified) == Some(false)
        {
            return IdleStep::Nothing;
        }
        IdleStep::Ask
    }

    /// What the watch does with a headless session's turn that ended, at
    /// `modified`, with neither a receipt nor an open question (ADR-t813-1
    /// decision 9), as `seen`: nothing for a turn that failed or was
    /// stopped (the session ends); a provider that cannot be used goes to
    /// its wall; after a person's `wait` the ask follows the next turn; a
    /// question closed without its answer is told in place of the nudge
    /// (task 1372) unless a recovery job looks at the idle; a turn refused
    /// too many permissions goes to its recovery job at once; otherwise it
    /// is nudged up to [`HEADLESS_NUDGES`] times, and then goes to its
    /// recovery job (`turn_without_receipt`).
    fn turn_step(&self, modified: SystemTime, seen: &TurnSeen) -> TurnStep {
        if seen.ended {
            return TurnStep::Nothing;
        }
        if let Some(failure) = seen.wall {
            return TurnStep::Wall(failure);
        }
        if let Some(after) = self.after_wait(modified) {
            return if after {
                TurnStep::Ask
            } else {
                TurnStep::Nothing
            };
        }
        if self.recovering.is_none() {
            match seen.notice {
                Some(NoticeStep::Send) => return TurnStep::Notice,
                Some(NoticeStep::Wait) => return TurnStep::Nothing,
                Some(NoticeStep::GaveUp) | None => {}
            }
        }
        if let Some(reason) = seen.at_once {
            return TurnStep::Recover(reason);
        }
        if usize::from(self.nudges) < HEADLESS_NUDGES && self.recovering.is_none() {
            return TurnStep::Nudge;
        }
        TurnStep::Recover(TURN_WITHOUT_RECEIPT)
    }

    /// What the watch does with its `stalled` ask `asked`, as `seen`, the
    /// idle marker written at `marker` and the run in its slot or not
    /// (`send`). An ask someone closed: its `intervene` opens it again, its
    /// `wait` counts again, anything else (or none) is a person who took
    /// the session over. An open ask: closed once the session ended a turn
    /// since it opened (or since a person stepped in, for an answer
    /// applied); `wait` restarts the count; `intervene`, which a headless
    /// session cannot take, opens it again (task 1179); `stop` and an
    /// instruction are sent only from the run's slot.
    fn ask_step(
        &self,
        asked: Asked,
        seen: AskSeen<'_>,
        marker: Option<SystemTime>,
        send: bool,
    ) -> AskStep {
        let moved = |since: SystemTime| marker_moved(marker, since);
        let answer = match seen {
            AskSeen::Closed(answer) => {
                return if answer.is_some_and(answered_intervene) {
                    AskStep::ClosedIntervene
                } else if answered_wait(answer) {
                    AskStep::ClosedWait
                } else {
                    AskStep::ClosedTaken
                };
            }
            AskSeen::Open(answer) => answer.map(str::trim),
        };
        match answer {
            None if moved(asked.at) => AskStep::Moved,
            None => AskStep::Nothing,
            Some(_) if asked.applied => {
                if self.held.is_none_or(moved) {
                    AskStep::Moved
                } else {
                    AskStep::Nothing
                }
            }
            Some(answer) if answered_wait(Some(answer)) => AskStep::Wait,
            Some(answer) if answered_intervene(answer) => AskStep::Intervene,
            Some(_) if !send => AskStep::Nothing,
            Some(STOP_OPTION) => AskStep::Stop,
            Some(_) => AskStep::Instruction,
        }
    }

    /// A headless session's turn ended with neither a receipt nor an open
    /// question (ADR-t813-1 decision 9): a login or a usage limit of its
    /// provider holds the queue for a person; a turn refused too many
    /// permissions goes to its recovery job at once (`permission_denied`);
    /// otherwise it is nudged, one resume per nudge, up to
    /// [`HEADLESS_NUDGES`] times, and then goes to its recovery job
    /// (`turn_without_receipt`). After a person's `wait` the ask follows.
    #[allow(clippy::too_many_arguments)]
    fn observe_turn(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle_marker: &Path,
        idle: &IdleMarker,
        idle_secs: i64,
        now: SystemTime,
        recovery: StallRecovery<'_, '_>,
    ) -> Result<Option<SystemTime>> {
        let mark = last_turn(sv, idle_marker);
        let mut seen = TurnSeen {
            ended: mark.is_some_and(|mark| !mark.outcome.goes_on(mark.failure)),
            wall: provider_failure(mark),
            at_once: alert_at_once(mark),
            notice: None,
        };
        let mut step = self.turn_step(idle.modified(), &seen);
        // A question closed without its answer is read only when its notice
        // could take the place of the nudge or the recovery job.
        let mut closed = None;
        if self.recovering.is_none() && matches!(step, TurnStep::Nudge | TurnStep::Recover(_)) {
            closed = closed_notice(sv, run, idle)?;
            seen.notice = closed.as_ref().map(|closed| {
                notice_step(
                    sv.notice_failures.get(run.id()),
                    closed.ask_id,
                    sv.sessions.retry_backoff(),
                    sv.generators.clock.monotonic(),
                )
            });
            step = self.turn_step(idle.modified(), &seen);
        }
        match step {
            TurnStep::Nothing => Ok(None),
            TurnStep::Wall(failure) => {
                sv.turn_at_wall(run, workspace, failure)?;
                Ok(None)
            }
            TurnStep::Ask => {
                self.open_ask(sv, run, workspace, idle, idle_secs, self.nudge, now, None)?;
                Ok(None)
            }
            TurnStep::Notice => match closed {
                Some(closed) => self.send_notice(sv, run, workspace, idle, idle_secs, now, closed),
                None => Ok(None),
            },
            TurnStep::Nudge => self.send_nudge(sv, run, workspace, idle, idle_secs, now),
            TurnStep::Recover(reason) => self.recover(
                sv, run, workspace, idle, idle_secs, self.nudge, now, recovery, reason,
            ),
        }
    }

    /// The session stays idle without a receipt after its nudge: its
    /// recovery job (`stalled`, reason `idle_without_receipt`, ADR-0047
    /// decision 30), and the `stalled` ask once the job does not repair
    /// it: an escalation, low confidence, a repair whose preconditions no
    /// longer hold, a failed job (with `reason_category: recovery_failed`)
    /// or the alert past its attempts. A `resume` repair asks the session's
    /// watch to park the run ([`Self::take_park`]). Returns an instruction
    /// the repair typed, for the check that it was taken.
    #[allow(clippy::too_many_arguments)]
    fn recover(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle: &IdleMarker,
        idle_secs: i64,
        nudge: Option<Nudge>,
        now: SystemTime,
        stall: StallRecovery<'_, '_>,
        why: &'static str,
    ) -> Result<Option<SystemTime>> {
        self.followed = true;
        let reason = Some(why);
        let StallRecovery { recovery, live } = stall;
        // The nudge is the evidence of the alert.
        let evidence: Vec<EventId> = if recovery
            .running_for(RecoveryAlert::Stalled, reason)
            .is_none()
        {
            sv.queue
                .run_events(run.id())?
                .iter()
                .rev()
                .find(|e| nudged_of(e).is_some())
                .map(|e| e.id)
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
        let facts = json!({
            "phase": PHASE,
            "idle_secs": idle_secs,
            "threshold": IDLE_THRESHOLD,
            "threshold_secs": sv.stall.idle_without_receipt_secs,
            "nudged_secs_ago": nudge.map(|nudge| secs_between(nudge.at, now)),
            "nudges": self.nudges,
            "background_running": idle.background_running_evidence(),
            "background_tasks": idle.background_tasks(),
            "evidence": evidence,
        });
        // A headless session's last turn, as its idle marker says it
        // (ADR-t813-1): its turns are in the job's material too.
        let mut facts = facts;
        if let Some(mark) = run
            .idle_marker_path()
            .ok()
            .and_then(|marker| last_turn(sv, &marker))
        {
            facts["turn"] = json!({
                "turn": mark.turn,
                "outcome": mark.outcome,
                "failure": mark.failure,
                "permission_denials": mark.permission_denials,
            });
        }
        let step = recovery.follow_for(sv, run, live, RecoveryAlert::Stalled, reason, || facts)?;
        if let Some(attempt) = recovery.running_for(RecoveryAlert::Stalled, reason)
            && self
                .recovering
                .as_ref()
                .is_none_or(|r| r.attempt != attempt)
        {
            info!(run_id = %run.id(), "run {} stays idle without a receipt {idle_secs}s after its nudge ({why}); recovery job {attempt} looks at it", run.id());
            self.recovery_started(sv, run, attempt, why, idle_secs, now)?;
        }
        match step {
            LiveStep::Pending => Ok(None),
            LiveStep::Repaired(applied) => {
                // A `wait` repairs nothing: the session moving after it
                // moved by itself.
                if applied.names.iter().any(|name| *name != "wait") {
                    if let Some(recovering) = &mut self.recovering {
                        recovering.repaired_at = Some(now);
                    }
                    self.recovered_from = Some(now);
                }
                if let Some(instruction) = applied.resume {
                    self.park = Some(Box::new(Park::Resume(instruction)));
                }
                Ok(applied.sent.map(|(text, sent_at, _submission)| {
                    self.input_sent(sent_at, Some(&text));
                    sent_at
                }))
            }
            LiveStep::Escalate(attempt, escalation) => {
                let alert = RecoveryAlert::Stalled;
                let note = escalation.note(run, alert, attempt);
                let extra = json!({"reason": why});
                sv.for_escalation(run, alert, attempt, &escalation, |sv| {
                    // A `stalled` ask another alert opened meanwhile
                    // already has a person looking.
                    let id = if sv.queue.has_unclosed_ask(run.id(), AskKind::Stalled)? {
                        None
                    } else {
                        Some(self.open_ask(
                            sv,
                            run,
                            workspace,
                            idle,
                            idle_secs,
                            nudge,
                            now,
                            Some(&note),
                        )?)
                    };
                    escalation.record(sv, run, alert, attempt, &note, id, extra)
                })?;
                Ok(None)
            }
        }
    }

    /// Record `stall_nudged` and send the nudge as the next turn, once in the
    /// phase. A nudge that could not be sent is noted; the ask follows on the next tick.
    fn send_nudge(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle: &IdleMarker,
        idle_secs: i64,
        now: SystemTime,
    ) -> Result<Option<SystemTime>> {
        self.nudged(sv, run, workspace, idle, idle_secs, now, None)?;
        let text = stall_nudge(run)?;
        let sent_at = sv.files.now();
        match submit(sv, run, workspace, Input::from(&text), "nudge") {
            Ok(_submission) => {
                self.input_sent(sent_at, Some(&text.text));
                info!(run_id = %run.id(), "run {} was idle without a receipt for {idle_secs}s; nudged it in workspace {workspace}", run.id());
                Ok(Some(sent_at))
            }
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "the nudge of {} could not be typed into workspace {workspace}: {error:#}; asking the inbox instead", run.id());
                Ok(None)
            }
        }
    }

    /// Send the notice of a question closed without its answer in place of
    /// the nudge (task 1372), and only once it is sent record it as the
    /// phase's `stall_nudged` with its `closed_ask`: a send that failed is
    /// tried again on a later observation ([`notice_step`]), so the notice
    /// is not taken as told when it never reached the session.
    #[allow(clippy::too_many_arguments)]
    fn send_notice(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle: &IdleMarker,
        idle_secs: i64,
        now: SystemTime,
        closed: ClosedNotice,
    ) -> Result<Option<SystemTime>> {
        let sent_at = sv.files.now();
        let what = "notice of a closed question";
        match submit(sv, run, workspace, Input::from(&closed.text), what) {
            Ok(_submission) => {
                sv.notice_failures.remove(run.id());
                self.nudged(
                    sv,
                    run,
                    workspace,
                    idle,
                    idle_secs,
                    now,
                    Some(closed.ask_id),
                )?;
                self.input_sent(sent_at, Some(&closed.text.text));
                info!(run_id = %run.id(), ask_id = closed.ask_id, "run {} was told in workspace {workspace} that its question {} was closed without an answer", run.id(), closed.ask_id);
                Ok(Some(sent_at))
            }
            Err(error) => {
                let failures = match sv.notice_failures.get(run.id()) {
                    Some(failed) if failed.ask_id == closed.ask_id => failed.failures + 1,
                    _ => 1,
                };
                sv.notice_failures.insert(
                    run.id().clone(),
                    NoticeFailure {
                        ask_id: closed.ask_id,
                        failures,
                        at: sv.generators.clock.monotonic(),
                    },
                );
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "the notice of closed question {} could not be sent to {} in workspace {workspace} (failure {failures} of {NOTICE_ATTEMPTS}): {error:#}", closed.ask_id, run.id());
                Ok(None)
            }
        }
    }

    /// Record `stall_nudged` (with `closed_ask` for the notice of a closed
    /// question) and count it as the phase's nudge.
    #[allow(clippy::too_many_arguments)]
    fn nudged(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle: &IdleMarker,
        idle_secs: i64,
        now: SystemTime,
        closed_ask: Option<i64>,
    ) -> Result<()> {
        let payload = NewStallNudged {
            phase: PHASE,
            idle_secs,
            threshold_secs: sv.stall.idle_without_receipt_secs,
            background_running: idle.background_running(),
            background_tasks: idle.background_tasks(),
            workspace_id: workspace,
            closed_ask,
        };
        sv.queue.record_runtime_event(
            run.id(),
            EventKind::StallNudged,
            serde_json::to_value(payload)?,
        )?;
        // A headless session's next nudge follows one that did not move
        // it on.
        if let Some(nudge) = &mut self.nudge
            && !nudge.settled
        {
            nudge.settled = true;
            let nudge = *nudge;
            Self::resolved(
                sv,
                run,
                "nudge",
                None,
                nudge.detected_after_secs,
                nudge.at,
                "nudged_again",
            )?;
        }
        self.nudge = Some(Nudge {
            at: now,
            detected_after_secs: idle_secs,
            settled: false,
        });
        self.nudges = self.nudges.saturating_add(1);
        Ok(())
    }

    /// Raise the session, still idle without a receipt after its nudge, as
    /// a `stalled` ask to the inbox: with the recovery job's `note` when
    /// the job did not repair it (its options added, its reason category),
    /// without after a person's `wait`.
    #[allow(clippy::too_many_arguments)]
    fn open_ask(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        _workspace: &str,
        _idle: &IdleMarker,
        idle_secs: i64,
        nudge: Option<Nudge>,
        now: SystemTime,
        note: Option<&Note>,
    ) -> Result<AskId> {
        let recovered = note.map_or_else(String::new, |note| {
            format!(" And {}.\n{}\n", note.why, note.text)
        });
        let nudged = nudged_text(nudge, now);
        let question = { headless_question(sv, run, idle_secs, &nudged, &recovered) };
        let mut options = stalled_options(note);
        {
            options = headless_options(options);
        }
        let category = note.map_or(AskReason::RecoveryFailed, |note| note.category);
        let id = new_stalled_ask(sv, run, question, options, category)?;
        warn!(ask_id = %id, run_id = %run.id(), "run {} stays idle without a receipt after its nudge; stalled ask {id}", run.id());
        self.recovery_resolved(sv, run, "escalated")?;
        self.asked = Some(Asked {
            id,
            at: now,
            detected_after_secs: idle_secs,
            applied: false,
            threshold: IDLE_THRESHOLD,
        });
        if let Some(nudge) = nudge
            && !nudge.settled
        {
            self.nudge = Some(Nudge {
                settled: true,
                ..nudge
            });
            Self::resolved(
                sv,
                run,
                "nudge",
                None,
                nudge.detected_after_secs,
                nudge.at,
                "escalated",
            )?;
        }
        Ok(id)
    }

    /// Open again the `stalled` ask of a headless session whose previous
    /// ask was answered `intervene` and closed: the same options without
    /// `intervene` and the same reason, at once, with no nudge and no
    /// recovery job (task 1179).
    fn reopen_ask(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        reopen: Reopen,
        now: SystemTime,
    ) -> Result<()> {
        // An ask closed by someone before the watch applied its answer
        // has no outcome yet.
        if !ask_resolved(&*sv.queue, run, reopen.previous)? {
            let payload = NewStallResolved {
                reopened: Some(true),
                ..Self::resolved_payload(
                    sv,
                    "ask",
                    reopen.threshold,
                    reopen.detected_after_secs,
                    reopen.at,
                    "answered_intervene",
                )
            };
            Self::record_resolved(
                sv,
                run,
                payload,
                Some(reopen.previous),
                "ask",
                "answered_intervene",
            )?;
        }
        let previous = stalled_ask(&*sv.queue, run.id(), Some(reopen.previous))?;
        let (options, category) = previous.as_ref().map_or_else(
            || (stalled_options(None), AskReason::RecoveryFailed),
            |ask| (ask.options.clone(), ask.reason_category),
        );
        let reopened = format!(
            "{REOPENED_FROM}{} was answered `intervene`, which a headless session cannot take (it has no screen and takes no keys), {REOPENED_TO}",
            reopen.previous
        );
        // What the previous ask said of the session (its alert, its
        // diagnosis) stands; only its answers are the headless ones.
        let question = match previous
            .as_ref()
            .and_then(|ask| situation_of(&ask.question))
        {
            Some(situation) => headless_ask_text(
                run.id(),
                &format!("{situation}{reopened}"),
                if reopen.threshold == IDLE_THRESHOLD {
                    WAIT_ASKS_AFTER_TURN
                } else {
                    ""
                },
                &turns_excerpt(sv, run),
            ),
            None => headless_question(
                sv,
                run,
                reopen.detected_after_secs,
                &nudged_text(self.nudge, now),
                &reopened,
            ),
        };
        let id = new_stalled_ask(sv, run, question, headless_options(options), category)?;
        warn!(ask_id = %id, run_id = %run.id(), "stalled ask {} of {} was answered intervene, which its headless session cannot take; stalled ask {id} asks again", reopen.previous, run.id());
        self.asked = Some(Asked {
            id,
            at: now,
            detected_after_secs: reopen.detected_after_secs,
            applied: false,
            threshold: reopen.threshold,
        });
        Ok(())
    }

    /// Follow the `stalled` ask: close it when the session ended a turn
    /// since (by itself, or after a person stepped in), and apply its
    /// answer: `wait` restarts the count and closes it; anything else is a
    /// person stepping in, and no ask follows until the session ends a turn
    /// after it; for a headless session any other answer is its next turn,
    /// sent only when `send` (the run is in its slot).
    fn watch_ask(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        marker: Option<SystemTime>,
        now: SystemTime,
        send: bool,
    ) -> Result<()> {
        // Kept until the ask opens: a failure tries again on the next look.
        if let Some(reopen) = self.reopen {
            self.reopen_ask(sv, run, reopen, now)?;
            self.reopen = None;
            return Ok(());
        }
        let Some(asked) = self.asked else {
            return Ok(());
        };
        let open = sv.queue.unclosed_stalled_ask(run.id())?;
        // Someone closed it: it is no longer followed, whatever comes next.
        if open.is_none() {
            self.asked = None;
        }
        let closed = match &open {
            Some(_) => None,
            None => stalled_ask(&*sv.queue, run.id(), Some(asked.id))?,
        };
        let seen = match &open {
            Some(ask) => AskSeen::Open(ask.answer.as_deref()),
            None => AskSeen::Closed(closed.as_ref().and_then(|ask| ask.answer.as_deref())),
        };
        let step = self.ask_step(asked, seen, marker, send);
        let answer = seen_answer(seen);
        let id = open.as_ref().map_or(asked.id, |ask| ask.id);
        match step {
            AskStep::Nothing => {}
            // Someone closed it: its answer `wait` counts again, anything
            // else (or none) is a person who took the session over. A
            // headless session's `intervene` holds nothing: its ask is
            // opened again (task 1179).
            AskStep::ClosedIntervene => {
                self.asked = None;
                self.reopen_after(sv, run, asked, asked.id, now)?;
            }
            AskStep::ClosedWait | AskStep::ClosedTaken => {
                self.asked = None;
                let wait = step == AskStep::ClosedWait;
                if wait {
                    // A `wait` on a send not taken does not stand for the idle.
                    if asked.threshold != SEND_THRESHOLD {
                        self.wait_from = Some(now);
                    }
                } else {
                    self.held = Some(now);
                }
                if !asked.applied {
                    Self::resolved(
                        sv,
                        run,
                        "ask",
                        Some((asked.id, asked.threshold)),
                        asked.detected_after_secs,
                        asked.at,
                        if wait {
                            "answered_wait"
                        } else {
                            "answered_intervene"
                        },
                    )?;
                }
            }
            AskStep::Moved => {
                self.close(sv, run, STALL_MOVED_CLOSED, "resolved_by_itself")?;
            }
            AskStep::Wait => {
                // Closed first: a supervisor that stops in between leaves
                // a closed `wait` its adopter reads as one.
                sv.queue.close_ask(id)?;
                Self::resolved(
                    sv,
                    run,
                    "ask",
                    Some((id, asked.threshold)),
                    asked.detected_after_secs,
                    asked.at,
                    "answered_wait",
                )?;
                self.asked = None;
                if asked.threshold != SEND_THRESHOLD {
                    self.wait_from = Some(now);
                }
                info!(ask_id = %id, run_id = %run.id(), "stalled ask {id} of {} answered wait; counting its idle again", run.id());
            }
            // A headless session takes no keys, so a person has no way in:
            // `intervene` (answered to an ask opened before it was taken
            // off its options, or typed as text) closes the ask and opens
            // it again with the headless options, and nothing is sent
            // (task 1179). Its outcome is recorded before the ask is
            // closed, and marked `reopened`: an adopter of a supervisor
            // that stopped in between opens the ask, once, without
            // recording it again.
            AskStep::Intervene => {
                if !ask_resolved(&*sv.queue, run, id)? {
                    let payload = NewStallResolved {
                        reopened: Some(true),
                        ..Self::resolved_payload(
                            sv,
                            "ask",
                            asked.threshold,
                            asked.detected_after_secs,
                            asked.at,
                            "answered_intervene",
                        )
                    };
                    Self::record_resolved(sv, run, payload, Some(id), "ask", "answered_intervene")?;
                }
                sv.queue.close_ask(id)?;
                self.asked = None;
                self.reopen_after(sv, run, asked, id, now)?;
            }
            // `stop` has a headless session end: its exit request, sent
            // once (the wrapper stops a running turn and exits), never as
            // a turn. The run then ends without a receipt: validating fails
            // it and its recovery job takes it (task 1104).
            AskStep::Stop => {
                // Written before the ask is closed: an adopter of a
                // supervisor that stopped in between finds it and closes
                // the ask without writing it again.
                if exit_requested(sv, run) {
                    info!(ask_id = %id, run_id = %run.id(), "the exit of {} was already requested; stalled ask {id} not sent again", run.id());
                } else {
                    let workspace = run.workspace_id().unwrap_or_default().to_owned();
                    submit(sv, run, &workspace, Input::Exit, "/exit")?;
                }
                sv.queue.close_ask(id)?;
                Self::resolved(
                    sv,
                    run,
                    "ask",
                    Some((id, asked.threshold)),
                    asked.detected_after_secs,
                    asked.at,
                    "answered_stop",
                )?;
                self.asked = None;
                self.held = Some(now);
                warn!(ask_id = %id, run_id = %run.id(), "stalled ask {id} of {} answered stop; the headless session is asked to exit, and the run goes to its recovery job without a receipt", run.id());
            }
            // A headless session takes no keys: a person's answer other
            // than `intervene` is its next turn's prompt (ADR-t813-1
            // decision 6).
            AskStep::Instruction => {
                // The request names the ask and is written before the ask
                // is closed: a supervisor that stopped in between left it,
                // and its adopter closes the ask without writing another
                // (task 863).
                let what = stalled_answer_what(id);
                if let Some(seq) = requested(sv, run, &what)? {
                    info!(ask_id = %id, run_id = %run.id(), "the answer of stalled ask {id} of {} was already requested (request {seq}); not sent again", run.id());
                } else {
                    let text = answer_text(run, id, answer.unwrap_or_default().trim());
                    let workspace = run.workspace_id().unwrap_or_default().to_owned();
                    let sent_at = sv.files.now();
                    submit(sv, run, &workspace, Input::from(&text), &what)?;
                    self.input_sent(sent_at, Some(&text.text));
                }
                sv.queue.close_ask(id)?;
                Self::resolved(
                    sv,
                    run,
                    "ask",
                    Some((id, asked.threshold)),
                    asked.detected_after_secs,
                    asked.at,
                    "answered_instruction",
                )?;
                self.asked = None;
                info!(ask_id = %id, run_id = %run.id(), "stalled ask {id} of {} answered; its answer is the headless session's next turn", run.id());
            }
        }
        Ok(())
    }

    /// Open again the ask `previous`, of the detection of `asked`, answered
    /// `intervene` and closed.
    fn reopen_after(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        asked: Asked,
        previous: AskId,
        now: SystemTime,
    ) -> Result<()> {
        let reopen = Reopen {
            previous,
            at: asked.at,
            detected_after_secs: asked.detected_after_secs,
            threshold: asked.threshold,
        };
        self.reopen = Some(reopen);
        self.reopen_ask(sv, run, reopen, now)?;
        self.reopen = None;
        Ok(())
    }
}

/// The answer of the ask as seen.
fn seen_answer(seen: AskSeen<'_>) -> Option<&str> {
    match seen {
        AskSeen::Open(answer) | AskSeen::Closed(answer) => answer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Task 1179: an ask opened again keeps what the previous one said of
    /// the session, whatever its answers were, and names only the latest
    /// `intervene`.
    #[test]
    fn the_situation_of_a_stalled_question_stops_at_its_answers() {
        let run = RunId::new("r").unwrap();
        let asked = headless_ask_text(
            &run,
            "The headless session has had background work running (alert: long_background), and the job escalated.\nDiagnosis: stuck\n",
            "",
            "turn 1: success",
        );
        assert!(!asked.contains("intervene"), "{asked}");
        assert!(asked.contains("`stop`") && asked.ends_with("\n\nturn 1: success"));
        assert_eq!(
            situation_of(&asked).as_deref(),
            Some(
                "The headless session has had background work running (alert: long_background), and the job escalated.\nDiagnosis: stuck"
            )
        );
        // A retired interactive session's ask, or one opened before task 1179.
        let old = "pid 7 is idle (alert: idle_process). And the job gave up.\nAnswer `wait` to leave the session alone, or `intervene` to step in yourself (read the screen).";
        assert_eq!(
            situation_of(old).as_deref(),
            Some("pid 7 is idle (alert: idle_process). And the job gave up.")
        );
        // The sentence of an earlier reopening goes.
        let again = format!(
            "It ended its turn.{REOPENED_FROM}3 was answered `intervene`, which a headless session cannot take (it has no screen and takes no keys), {REOPENED_TO} Before answering, read what its turns did: ..."
        );
        assert_eq!(situation_of(&again).as_deref(), Some("It ended its turn."));
        assert_eq!(situation_of("no answers here"), None);
    }

    use super::super::file_time::at_ns;

    /// A recorded event `id` of `kind` with `payload`, recorded at second
    /// `secs` and millisecond `ms` of a fixed minute.
    fn event(id: i64, kind: &str, payload: Value, secs: u32, ms: u32) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: format!("2026-09-30T00:00:{secs:02}.{ms:03}Z"),
            actor: None,
        }
    }

    /// The time [`event`] records at `secs` and `ms`.
    fn event_at(secs: u32, ms: u32) -> SystemTime {
        at_event(&event(0, "x", json!({}), secs, ms)).unwrap()
    }

    /// A `stalled` ask `id` of the run, answered `answer`, closed at
    /// `closed`, opened at unix second 1000.
    fn stalled(id: i64, answer: Option<&str>, closed: Option<i64>) -> crate::domain::Ask {
        serde_json::from_value(json!({
            "id": id, "kind": "stalled", "task_id": 1, "run_id": "r", "question": "q",
            "options": ["wait", "stop"], "answer": answer, "asked_by": "supervisor",
            "reason_category": "recovery_failed", "created_at": 1000,
            "answered_at": answer.map(|_| 1001), "closed_at": closed,
        }))
        .unwrap()
    }

    /// The `stall_resolved` of the ask `id` with `outcome`.
    fn ask_end(id: i64, ask: i64, outcome: &str, extra: Value) -> RunEvent {
        let mut payload =
            json!({"phase": PHASE, "detection": "ask", "outcome": outcome, "ask_id": ask});
        for (key, value) in extra.as_object().into_iter().flatten() {
            payload[key] = value.clone();
        }
        event(id, event_kind::STALL_RESOLVED, payload, 10, 0)
    }

    /// Task 1558: an adopter counts the phase's nudges and takes the last
    /// one's time and idle from `stall_nudged`; a nudge of another phase,
    /// or without its phase (a payload not of this watch), does not count,
    /// and a missing or mistyped `idle_secs` reads as 0.
    #[test]
    fn an_adopter_reads_the_nudges_of_the_phase_and_tolerates_missing_fields() {
        let nudged = |id, payload| event(id, event_kind::STALL_NUDGED, payload, 5, id as u32);
        let events = [
            nudged(1, json!({"phase": PHASE, "idle_secs": 40})),
            nudged(2, json!({"phase": "revise", "idle_secs": 50})),
            nudged(3, json!({"idle_secs": 60})),
            nudged(4, json!({"phase": PHASE, "idle_secs": "70"})),
            event(
                5,
                event_kind::STALL_RESOLVED,
                json!({"phase": PHASE, "detection": "nudge"}),
                6,
                0,
            ),
        ];
        let watch = StallWatch::adopted(&events, None, None);
        assert_eq!(watch.nudges, 2);
        let nudge = watch.nudge.unwrap();
        assert_eq!(nudge.at, event_at(5, 4));
        assert_eq!(nudge.detected_after_secs, 0);
        assert!(nudge.settled);
        assert_eq!(watch.last_input, Some(event_at(5, 4)));
        // Nothing recorded: nothing rebuilt.
        let empty = StallWatch::adopted(&[], None, None);
        assert!(empty.nudge.is_none() && empty.asked.is_none() && empty.recovering.is_none());
        // A payload that is no object reads as one with no fields.
        let broken = [event(1, event_kind::STALL_NUDGED, json!("session"), 5, 0)];
        assert_eq!(StallWatch::adopted(&broken, None, None).nudges, 0);
    }

    /// Task 1558: the idle's recovery job an adopter takes over is the last
    /// one requested whose end was not recorded, with its attempt, reason
    /// and idle; a `recovery_finished` that applied a repair other than
    /// `wait` restarts the count and marks the repair. A job of another
    /// alert or reason is not the idle's, and a missing attempt reads as 0.
    #[test]
    fn an_adopter_takes_over_the_idles_recovery_job_whose_end_is_not_recorded() {
        let job = |id, kind, payload| event(id, kind, payload, 20, id as u32);
        let requested = json!({"alert": "stalled", "reason": TURN_WITHOUT_RECEIPT, "attempt": 2, "idle_secs": 90});
        let mut events = vec![
            job(
                1,
                event_kind::RECOVERY_REQUESTED,
                json!({"alert": "idle_process", "reason": TURN_WITHOUT_RECEIPT, "attempt": 1}),
            ),
            job(2, event_kind::RECOVERY_REQUESTED, requested.clone()),
        ];
        let watch = StallWatch::adopted(&events, None, None);
        let recovering = watch.recovering.as_deref().unwrap();
        assert_eq!(recovering.attempt, 2);
        assert_eq!(recovering.reason, TURN_WITHOUT_RECEIPT);
        assert_eq!(recovering.detected_after_secs, 90);
        assert_eq!(recovering.at, event_at(20, 2));
        assert!(recovering.repaired_at.is_none() && watch.recovered_from.is_none());
        // `wait` repairs nothing; a non-text item is no `wait`.
        let finished = |id, applied: Value| {
            job(
                id,
                event_kind::RECOVERY_FINISHED,
                json!({"alert": "stalled", "reason": TURN_WITHOUT_RECEIPT, "attempt": 2, "applied": applied}),
            )
        };
        events.push(finished(3, json!(["wait"])));
        assert!(
            StallWatch::adopted(&events, None, None)
                .recovered_from
                .is_none()
        );
        events.push(finished(4, json!([7])));
        let watch = StallWatch::adopted(&events, None, None);
        assert_eq!(watch.recovered_from, Some(event_at(20, 4)));
        assert_eq!(
            watch.recovering.as_deref().unwrap().repaired_at,
            Some(event_at(20, 4))
        );
        // Its end recorded: nothing to take over.
        events.push(job(
            5,
            event_kind::STALL_RESOLVED,
            json!({"phase": PHASE, "detection": "recovery", "attempt": 2}),
        ));
        assert!(
            StallWatch::adopted(&events, None, None)
                .recovering
                .is_none()
        );
        // A request recorded before the attempt was: attempt 0.
        let old = [job(
            1,
            event_kind::RECOVERY_REQUESTED,
            json!({"alert": "stalled", "reason": IDLE_WITHOUT_RECEIPT}),
        )];
        let watch = StallWatch::adopted(&old, None, None);
        assert_eq!(watch.recovering.as_deref().unwrap().attempt, 0);
        assert_eq!(watch.recovering.as_deref().unwrap().detected_after_secs, 0);
    }

    /// Task 1558, moved from
    /// `runtime_headless_stall::an_adopter_reopens_a_headless_stalled_ask_left_answered_intervene`
    /// (the cases `::an_adopter_opens_the_next_ask_of_an_intervene_a_person_closed` and
    /// `::an_adopter_opens_the_next_ask_of_a_closed_intervene_once` run end to end):
    /// an ask left open answered `intervene` is followed (to be opened
    /// again, never applied), and one a person closed answered `intervene`
    /// with no outcome recorded is opened again; one the previous
    /// supervisor closed and marked `reopened` is opened again once, not
    /// after a turn was sent since.
    #[test]
    fn an_adopter_opens_again_an_ask_answered_intervene_wherever_its_supervisor_stopped() {
        // Held: the outcome recorded and the ask left open.
        let held = stalled(7, Some("intervene"), None);
        let events = [ask_end(1, 7, "answered_intervene", json!({}))];
        let watch = StallWatch::adopted(&events, Some(&held), Some(&held));
        let asked = watch.asked.unwrap();
        assert_eq!(asked.id, AskId::new(7));
        assert!(!asked.applied);
        assert_eq!(asked.at, at_unix(1001));
        assert_eq!(asked.threshold, IDLE_THRESHOLD);
        assert!(watch.held.is_none() && watch.reopen.is_none());
        // Closed by a person while no supervisor watched.
        let closed = stalled(7, Some("intervene"), Some(1005));
        let watch = StallWatch::adopted(&[], Some(&closed), None);
        let reopen = watch.reopen.unwrap();
        assert_eq!(reopen.previous, AskId::new(7));
        assert_eq!(reopen.at, at_unix(1001));
        assert!(watch.held.is_none() && watch.asked.is_none());
        // Closed and marked by the previous supervisor, the next not open.
        let marked = [ask_end(
            1,
            7,
            "answered_intervene",
            json!({"reopened": true, "detected_after_secs": 12}),
        )];
        let watch = StallWatch::adopted(&marked, Some(&closed), None);
        let reopen = watch.reopen.unwrap();
        assert_eq!(reopen.previous, AskId::new(7));
        assert_eq!(reopen.detected_after_secs, 12);
        // A `reopened` of another type, or a turn sent since: not again.
        let mistyped = [ask_end(
            1,
            7,
            "answered_intervene",
            json!({"reopened": "true"}),
        )];
        assert!(
            StallWatch::adopted(&mistyped, Some(&closed), None)
                .reopen
                .is_none()
        );
        let sent = [
            marked[0].clone(),
            event(2, event_kind::TURN_REQUESTED, json!({"seq": 3}), 11, 0),
        ];
        assert!(
            StallWatch::adopted(&sent, Some(&closed), None)
                .reopen
                .is_none()
        );
        // An `ask_id` of another type names no ask: the closed ask has no
        // outcome recorded, and is opened again from the ask itself.
        let other = [ask_end(
            1,
            7,
            "answered_intervene",
            json!({"reopened": true, "ask_id": "7", "detected_after_secs": 12}),
        )];
        let reopen = StallWatch::adopted(&other, Some(&closed), None)
            .reopen
            .unwrap();
        assert_eq!((reopen.at, reopen.detected_after_secs), (at_unix(1001), 0));
    }

    /// Task 1558: a closed ask with no outcome recorded: its `wait` counts
    /// again from its close, anything else holds from the next second; an
    /// open ask answered and applied holds from the next second after its
    /// answer. The outcomes recorded give `wait_from` and `held`.
    #[test]
    fn an_adopter_reads_waits_and_holds_from_closed_asks_and_outcomes() {
        let waited = stalled(3, Some("wait"), Some(1010));
        let watch = StallWatch::adopted(&[], Some(&waited), None);
        assert_eq!(watch.wait_from, Some(at_unix(1010)));
        let taken = stalled(3, Some("I'll look"), Some(1010));
        assert_eq!(
            StallWatch::adopted(&[], Some(&taken), None).held,
            Some(at_unix(1011))
        );
        // Its outcome recorded: read from the events instead.
        let events = [ask_end(1, 3, "answered_wait", json!({}))];
        let watch = StallWatch::adopted(&events, Some(&taken), None);
        assert_eq!(watch.wait_from, Some(event_at(10, 0)));
        assert!(watch.held.is_none());
        let stopped = [ask_end(1, 3, "answered_stop", json!({}))];
        assert_eq!(
            StallWatch::adopted(&stopped, None, None).held,
            Some(event_at(10, 0))
        );
        // Open, answered and applied.
        let open = stalled(4, Some("go on"), None);
        let applied = [ask_end(1, 4, "answered_intervene", json!({}))];
        let watch = StallWatch::adopted(&applied, Some(&open), Some(&open));
        assert!(watch.asked.unwrap().applied);
        assert_eq!(watch.held, Some(at_unix(1002)));
    }

    /// Task 1558: the setting an escalated ask was judged by comes from the
    /// `recovery_finished` that names it; one that names no ask, or an
    /// `ask_id` of another type, leaves the idle's own.
    #[test]
    fn the_threshold_of_an_ask_is_its_escalations() {
        let finished = |payload| [event(1, event_kind::RECOVERY_FINISHED, payload, 0, 0)];
        let of = |payload| threshold_of(&finished(payload), AskId::new(5));
        assert_eq!(
            of(json!({"alert": "idle_process", "ask_id": 5})),
            IDLE_PROCESS_THRESHOLD
        );
        assert_eq!(
            of(json!({"alert": "long_background", "ask_id": 5})),
            BACKGROUND_THRESHOLD
        );
        assert_eq!(
            of(json!({"alert": "stalled", "reason": SEND_UNCONFIRMED, "ask_id": 5})),
            SEND_THRESHOLD
        );
        assert_eq!(
            of(json!({"alert": "idle_process", "ask_id": "5"})),
            IDLE_THRESHOLD
        );
        assert_eq!(of(json!({"alert": "idle_process"})), IDLE_THRESHOLD);
        assert_eq!(threshold_of(&[], AskId::new(5)), IDLE_THRESHOLD);
    }

    /// Task 1558: the end of an ask is recorded only by a `stall_resolved`
    /// of the phase's `ask` detection that names it as an integer.
    #[test]
    fn an_asks_end_is_its_own_ask_detection() {
        let end = |payload| [event(1, event_kind::STALL_RESOLVED, payload, 0, 0)];
        let id = AskId::new(9);
        assert!(ask_resolved_in(
            &end(json!({"phase": PHASE, "detection": "ask", "ask_id": 9})),
            id
        ));
        assert!(!ask_resolved_in(
            &end(json!({"phase": PHASE, "detection": "ask", "ask_id": 8})),
            id
        ));
        assert!(!ask_resolved_in(
            &end(json!({"phase": PHASE, "detection": "nudge", "ask_id": 9})),
            id
        ));
        assert!(!ask_resolved_in(
            &end(json!({"detection": "ask", "ask_id": 9})),
            id
        ));
        assert!(!ask_resolved_in(
            &end(json!({"phase": PHASE, "detection": "ask", "ask_id": 9.0})),
            id
        ));
    }

    /// Task 1558: the records the watch writes keep the keys it wrote
    /// before they were typed, and an ask's id, a job's attempt and reason
    /// and an ask opened again only when they are there.
    #[test]
    fn the_records_of_the_watch_keep_their_keys() {
        let resolved = NewStallResolved {
            phase: PHASE,
            detection: "recovery",
            threshold: IDLE_THRESHOLD,
            threshold_secs: 1200,
            detected_after_secs: 30,
            outcome: "escalated",
            resolved_after_secs: 4,
            ask_id: None,
            attempt: Some(2),
            reason: Some(TURN_WITHOUT_RECEIPT),
            reopened: None,
        };
        assert_eq!(
            serde_json::to_value(&resolved).unwrap(),
            json!({"phase": "session", "detection": "recovery", "threshold": IDLE_THRESHOLD,
                "threshold_secs": 1200, "detected_after_secs": 30, "outcome": "escalated",
                "resolved_after_secs": 4, "attempt": 2, "reason": TURN_WITHOUT_RECEIPT})
        );
        let asked = NewStallResolved {
            detection: "ask",
            ask_id: Some(7),
            attempt: None,
            reason: None,
            reopened: Some(true),
            ..resolved
        };
        let value = serde_json::to_value(&asked).unwrap();
        assert_eq!(value["ask_id"], 7);
        assert_eq!(value["reopened"], true);
        assert!(value.get("attempt").is_none() && value.get("reason").is_none());
        let nudged = NewStallNudged {
            phase: PHASE,
            idle_secs: 30,
            threshold_secs: 1200,
            background_running: false,
            background_tasks: Vec::<crate::domain::stall::BackgroundTask>::new(),
            workspace_id: "w",
            closed_ask: None,
        };
        assert_eq!(
            serde_json::to_value(&nudged).unwrap(),
            json!({"phase": "session", "idle_secs": 30, "threshold_secs": 1200,
                "background_running": false, "background_tasks": [], "workspace_id": "w"})
        );
        let notice = NewStallNudged {
            closed_ask: Some(4),
            ..nudged
        };
        assert_eq!(serde_json::to_value(&notice).unwrap()["closed_ask"], 4);
    }

    fn seen(modified: SystemTime) -> IdleSeen {
        IdleSeen {
            modified,
            receipt_written: false,
            question_open: false,
            held: false,
        }
    }

    /// Task 1558, moved from `runtime_stall::a_worker_idle_at_its_question_is_not_nudged`:
    /// a session idle at its own question, with its receipt written, held
    /// by a hold, or not idle since the last text it was sent (a marker
    /// of the same millisecond) is left alone; in its slot its turn is
    /// judged.
    #[test]
    fn an_idle_session_is_judged_only_past_its_inputs_question_receipt_and_hold() {
        let watch = StallWatch::default();
        let idle = seen(at_ns(300, 0));
        assert_eq!(watch.idle_step(&idle, true), IdleStep::Turn);
        for waiting in [
            IdleSeen {
                question_open: true,
                ..idle
            },
            IdleSeen {
                receipt_written: true,
                ..idle
            },
            IdleSeen { held: true, ..idle },
        ] {
            assert_eq!(watch.idle_step(&waiting, true), IdleStep::Nothing);
            assert_eq!(watch.idle_step(&waiting, false), IdleStep::Nothing);
        }
        let mut sent = StallWatch::default();
        sent.input_sent(at_ns(300, 0), None);
        assert_eq!(
            sent.idle_step(&seen(at_ns(300, 900_000)), true),
            IdleStep::Nothing
        );
        assert_eq!(sent.idle_step(&seen(at_ns(301, 0)), true), IdleStep::Turn);
    }

    /// Task 1558: out of its slot nothing is sent: only after a nudge and a
    /// person's `wait` is the ask opened, and only for a turn after the
    /// `wait` (the millisecond of the `wait` is before it).
    #[test]
    fn out_of_its_slot_only_a_wait_followed_by_a_turn_asks() {
        let nudge = Nudge {
            at: at_ns(100, 0),
            detected_after_secs: 0,
            settled: true,
        };
        let waited = StallWatch {
            nudge: Some(nudge),
            wait_from: Some(at_ns(200, 0)),
            ..StallWatch::default()
        };
        assert_eq!(waited.idle_step(&seen(at_ns(201, 0)), false), IdleStep::Ask);
        assert_eq!(
            waited.idle_step(&seen(at_ns(200, 500_000)), false),
            IdleStep::Nothing
        );
        let unwaited = StallWatch {
            wait_from: None,
            ..waited.clone()
        };
        assert_eq!(
            unwaited.idle_step(&seen(at_ns(201, 0)), false),
            IdleStep::Nothing
        );
        let unnudged = StallWatch {
            nudge: None,
            ..waited
        };
        assert_eq!(
            unnudged.idle_step(&seen(at_ns(201, 0)), false),
            IdleStep::Nothing
        );
    }

    /// Task 1558: a turn that failed ends the session; a provider that
    /// cannot be used goes to its wall; after `wait` only a later turn
    /// asks; a closed question's notice comes before the nudge unless its
    /// send waits or gave up; a turn refused too often goes to its job at
    /// once; [`HEADLESS_NUDGES`] nudges, then the job.
    #[test]
    fn a_turn_is_nudged_up_to_its_limit_and_then_recovered() {
        let at = at_ns(500, 0);
        let fresh = StallWatch::default();
        let turn = TurnSeen::default();
        assert_eq!(fresh.turn_step(at, &turn), TurnStep::Nudge);
        let ended = TurnSeen {
            ended: true,
            wall: Some(TurnFailure::UsageLimit),
            ..turn
        };
        assert_eq!(fresh.turn_step(at, &ended), TurnStep::Nothing);
        let wall = TurnSeen {
            wall: Some(TurnFailure::UsageLimit),
            ..turn
        };
        assert_eq!(
            fresh.turn_step(at, &wall),
            TurnStep::Wall(TurnFailure::UsageLimit)
        );
        let waited = StallWatch {
            wait_from: Some(at),
            ..StallWatch::default()
        };
        assert_eq!(waited.turn_step(at, &turn), TurnStep::Nothing);
        assert_eq!(waited.turn_step(at_ns(501, 0), &turn), TurnStep::Ask);
        for (notice, step) in [
            (NoticeStep::Send, TurnStep::Notice),
            (NoticeStep::Wait, TurnStep::Nothing),
            (NoticeStep::GaveUp, TurnStep::Nudge),
        ] {
            let seen = TurnSeen {
                notice: Some(notice),
                ..turn
            };
            assert_eq!(fresh.turn_step(at, &seen), step, "{notice:?}");
        }
        let denied = TurnSeen {
            at_once: Some(PERMISSION_DENIED),
            ..turn
        };
        assert_eq!(
            fresh.turn_step(at, &denied),
            TurnStep::Recover(PERMISSION_DENIED)
        );
        let nudged = StallWatch {
            nudges: u8::try_from(HEADLESS_NUDGES).unwrap(),
            ..StallWatch::default()
        };
        assert_eq!(
            nudged.turn_step(at, &turn),
            TurnStep::Recover(TURN_WITHOUT_RECEIPT)
        );
        let below = StallWatch {
            nudges: u8::try_from(HEADLESS_NUDGES - 1).unwrap(),
            ..StallWatch::default()
        };
        assert_eq!(below.turn_step(at, &turn), TurnStep::Nudge);
        // A job that looks at the idle: no notice, no nudge.
        let recovering = StallWatch {
            recovering: Some(Box::new(Recovering {
                attempt: 1,
                reason: TURN_WITHOUT_RECEIPT,
                at,
                detected_after_secs: 0,
                repaired_at: None,
            })),
            ..StallWatch::default()
        };
        let notice = TurnSeen {
            notice: Some(NoticeStep::Send),
            ..turn
        };
        assert_eq!(
            recovering.turn_step(at, &notice),
            TurnStep::Recover(TURN_WITHOUT_RECEIPT)
        );
    }

    /// Task 1558: a notice whose send failed waits out the backoff (not at
    /// its end) and gives up after [`NOTICE_ATTEMPTS`]; a failure of
    /// another question's notice does not hold it.
    #[test]
    fn a_failed_notice_waits_its_backoff_and_gives_up() {
        let at = Instant::now();
        let backoff = Duration::from_secs(2);
        let failed = |failures| NoticeFailure {
            ask_id: 3,
            failures,
            at,
        };
        assert_eq!(notice_step(None, 3, backoff, at), NoticeStep::Send);
        assert_eq!(
            notice_step(Some(&failed(1)), 3, backoff, at),
            NoticeStep::Wait
        );
        let just_before = at + backoff - Duration::from_millis(1);
        assert_eq!(
            notice_step(Some(&failed(1)), 3, backoff, just_before),
            NoticeStep::Wait
        );
        assert_eq!(
            notice_step(Some(&failed(1)), 3, backoff, at + backoff),
            NoticeStep::Send
        );
        assert_eq!(
            notice_step(Some(&failed(NOTICE_ATTEMPTS)), 3, backoff, at),
            NoticeStep::GaveUp
        );
        assert_eq!(
            notice_step(Some(&failed(NOTICE_ATTEMPTS)), 4, backoff, at),
            NoticeStep::Send
        );
    }

    /// Task 1558, moved from
    /// `runtime_headless_stall::an_intervene_answer_closes_the_headless_stalled_ask_and_opens_another`
    /// (the ask closed by the one who answered `wait` runs end to end in
    /// `::a_headless_stalled_ask_closed_after_wait_is_asked_again_after_the_next_turn`):
    /// the answers of a `stalled` ask, open or closed by someone, and the
    /// session moving on.
    #[test]
    fn the_answers_of_a_stalled_ask_are_applied_by_kind() {
        let asked = Asked {
            id: AskId::new(1),
            at: at_ns(400, 0),
            detected_after_secs: 0,
            applied: false,
            threshold: IDLE_THRESHOLD,
        };
        let watch = StallWatch::default();
        let step = |seen, marker, send| watch.ask_step(asked, seen, marker, send);
        // Closed by someone.
        assert_eq!(
            step(AskSeen::Closed(Some("intervene")), None, true),
            AskStep::ClosedIntervene
        );
        assert_eq!(
            step(AskSeen::Closed(Some("wait")), None, true),
            AskStep::ClosedWait
        );
        assert_eq!(
            step(AskSeen::Closed(Some("go on")), None, false),
            AskStep::ClosedTaken
        );
        assert_eq!(
            step(AskSeen::Closed(None), None, true),
            AskStep::ClosedTaken
        );
        // Open and unanswered: closed once the session took a turn since
        // it opened (not one of its millisecond).
        assert_eq!(
            step(AskSeen::Open(None), Some(at_ns(400, 900_000)), true),
            AskStep::Nothing
        );
        assert_eq!(
            step(AskSeen::Open(None), Some(at_ns(401, 0)), true),
            AskStep::Moved
        );
        assert_eq!(step(AskSeen::Open(None), None, true), AskStep::Nothing);
        // Answered.
        assert_eq!(
            step(AskSeen::Open(Some(" wait ")), None, false),
            AskStep::Wait
        );
        assert_eq!(
            step(AskSeen::Open(Some("propose: why")), None, false),
            AskStep::Wait
        );
        assert_eq!(
            step(AskSeen::Open(Some("intervene")), None, false),
            AskStep::Intervene
        );
        assert_eq!(
            step(AskSeen::Open(Some("intervene: I look")), None, true),
            AskStep::Intervene
        );
        assert_eq!(step(AskSeen::Open(Some("stop")), None, true), AskStep::Stop);
        assert_eq!(
            step(AskSeen::Open(Some("stop")), None, false),
            AskStep::Nothing
        );
        assert_eq!(
            step(AskSeen::Open(Some("go on")), None, true),
            AskStep::Instruction
        );
        assert_eq!(
            step(AskSeen::Open(Some("go on")), None, false),
            AskStep::Nothing
        );
        // An answer applied (a person stepped in): closed once the session
        // took a turn after the step in.
        let applied = Asked {
            applied: true,
            ..asked
        };
        let held = StallWatch {
            held: Some(at_ns(450, 0)),
            ..StallWatch::default()
        };
        let marker = |ms| Some(at_ns(ms, 0));
        assert_eq!(
            held.ask_step(applied, AskSeen::Open(Some("x")), marker(450), true),
            AskStep::Nothing
        );
        assert_eq!(
            held.ask_step(applied, AskSeen::Open(Some("x")), marker(451), true),
            AskStep::Moved
        );
        assert_eq!(
            watch.ask_step(applied, AskSeen::Open(Some("x")), None, true),
            AskStep::Moved
        );
        // The headless options: no `intervene`, `stop` after `wait`.
        assert_eq!(
            headless_options(vec!["wait".into(), "intervene".into(), "go on".into()]),
            ["wait", "stop", "go on"]
        );
    }

    /// Task 1050: a marker of the same millisecond as the event of the last
    /// text typed, a person's step in, a `wait` or a recovery job's request
    /// is from before it; one of the next millisecond is after it.
    #[test]
    fn a_marker_of_the_events_millisecond_is_from_before_it() {
        let event = at_ns(250, 0);
        let same = at_ns(250, 700_000);
        let next = at_ns(251, 0);
        // The last text typed (an adopted `stall_nudged`).
        let mut watch = StallWatch::default();
        watch.input_sent(event, None);
        assert!(!watch.ended_after_inputs(same));
        assert!(watch.ended_after_inputs(next));
        // A person who stepped in (`answered_intervene`).
        let held = StallWatch {
            held: Some(event),
            ..StallWatch::default()
        };
        assert!(!held.ended_after_inputs(same));
        assert!(held.ended_after_inputs(next));
        // `wait` (`answered_wait`).
        let waited = StallWatch {
            wait_from: Some(event),
            ..StallWatch::default()
        };
        assert_eq!(waited.after_wait(same), Some(false));
        assert_eq!(waited.after_wait(next), Some(true));
        assert_eq!(StallWatch::default().after_wait(next), None);
        // A recovery job's request, then its repair.
        let mut recovering = Recovering {
            attempt: 1,
            reason: IDLE_WITHOUT_RECEIPT,
            at: event,
            detected_after_secs: 0,
            repaired_at: None,
        };
        assert!(!recovering.moved(Some(same)));
        assert!(recovering.moved(Some(next)));
        recovering.repaired_at = Some(at_ns(900, 0));
        assert!(!recovering.moved(Some(at_ns(900, 999_999))));
        assert!(recovering.moved(Some(at_ns(901, 0))));
    }

    /// Task 1050: a marker of the millisecond of a `stalled` ask's opening
    /// or of a person's step in (an adopter's, from an event) is not a turn
    /// the session took since, for `watch_ask`.
    #[test]
    fn a_marker_of_the_asks_millisecond_has_not_moved() {
        let since = at_ns(250, 0);
        assert!(!marker_moved(None, since));
        assert!(!marker_moved(Some(at_ns(250, 700_000)), since));
        assert!(!marker_moved(Some(at_ns(250, 0)), since));
        assert!(marker_moved(Some(at_ns(251, 0)), since));
        // An ask an adopter reads in whole seconds is taken from the next
        // second (`at_unix(created_at + 1)`), at_ns(1_000, 0).
        let asked = at_unix(1_000_000 + 1);
        assert!(!marker_moved(Some(at_ns(999, 999_999)), asked));
        assert!(!marker_moved(Some(at_ns(1_000, 700_000)), asked));
        assert!(marker_moved(Some(at_ns(1_001, 0)), asked));
    }
}
