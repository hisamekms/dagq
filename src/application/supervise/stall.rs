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
use crate::domain::recovery::{
    IDLE_WITHOUT_RECEIPT, PERMISSION_DENIED, SEND_UNCONFIRMED, TURN_WITHOUT_RECEIPT,
};
use crate::domain::turn::HEADLESS_NUDGES;

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
    text: String,
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
enum NoticeStep {
    Send,
    /// Its last send failed a moment ago: nothing is sent this time.
    Wait,
    /// Its sends failed [`NOTICE_ATTEMPTS`] times: the nudge as before.
    GaveUp,
}

fn notice_step(sv: &Supervisor<'_>, run: &TaskRun, ask_id: i64) -> NoticeStep {
    match sv.notice_failures.get(run.id()) {
        Some(failed) if failed.ask_id == ask_id && failed.failures >= NOTICE_ATTEMPTS => {
            NoticeStep::GaveUp
        }
        Some(failed)
            if failed.ask_id == ask_id && failed.at.elapsed() < sv.cmux.retry_backoff() =>
        {
            NoticeStep::Wait
        }
        _ => NoticeStep::Send,
    }
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
    Ok(queue.run_events(run.id())?.iter().any(|e| {
        e.kind == event_kind::STALL_RESOLVED
            && e.payload["phase"] == PHASE
            && e.payload["detection"] == "ask"
            && e.payload["ask_id"] == json!(id)
    }))
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
        &sv.layout.main_checkout,
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
        sv.cmux,
    )?;
    let id = AskId::new(outcome["id"].as_i64().context("ask returned no id")?);
    info!(ask_id = %id, run_id = %run.id(), "stalled ask {id} of {} opened (notified: {})", run.id(), outcome["notified"]);
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
    /// `stalled` ask, so a nudge or an ask is never repeated.
    pub(super) fn adopt(queue: &dyn Queue, run: &TaskRun) -> Result<Self> {
        let events = queue.run_events(run.id())?;
        let resolved = |detection: &str, ask: Option<AskId>| {
            events.iter().any(|e| {
                e.kind == event_kind::STALL_RESOLVED
                    && e.payload["phase"] == PHASE
                    && e.payload["detection"] == detection
                    && ask.is_none_or(|id| e.payload["ask_id"] == json!(id))
            })
        };
        let mut watch = Self {
            nudges: events
                .iter()
                .filter(|e| e.kind == event_kind::STALL_NUDGED && e.payload["phase"] == PHASE)
                .count()
                .try_into()
                .unwrap_or(u8::MAX),
            ..Self::default()
        };
        if let Some(event) = events
            .iter()
            .rev()
            .find(|e| e.kind == event_kind::STALL_NUDGED && e.payload["phase"] == PHASE)
        {
            let at = at_event(event).unwrap_or(UNIX_EPOCH);
            watch.nudge = Some(Nudge {
                at,
                detected_after_secs: event.payload["idle_secs"].as_i64().unwrap_or(0),
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
                .find(|e| {
                    e.kind == event_kind::STALL_RESOLVED
                        && e.payload["phase"] == PHASE
                        && e.payload["outcome"] == outcome
                })
                .and_then(at_event)
        };
        // A person stepped in, or had the session stopped, on an ask closed
        // since. A headless session's `intervene` is no step in: its ask
        // is opened again instead (task 1179).
        watch.held = latest_outcome("answered_stop");
        // A recovery job's escalation names its ask (ADR-0047), and its
        // alert the setting it was judged by.
        let threshold_of = |id: AskId| {
            let recovery = events.iter().find(|e| {
                e.kind == event_kind::RECOVERY_FINISHED && e.payload["ask_id"] == json!(id)
            });
            match recovery {
                Some(e) if e.payload["alert"] == RecoveryAlert::IdleProcess.as_str() => {
                    IDLE_PROCESS_THRESHOLD
                }
                Some(e) if e.payload["alert"] == RecoveryAlert::LongBackground.as_str() => {
                    BACKGROUND_THRESHOLD
                }
                Some(e) if e.payload["reason"] == SEND_UNCONFIRMED => SEND_THRESHOLD,
                _ => IDLE_THRESHOLD,
            }
        };
        watch.wait_from = latest_outcome("answered_wait");
        // An instruction a recovery job had typed is an input too, and its
        // repair restarted the count.
        let idle_job = |e: &&crate::domain::RunEvent| {
            e.payload["alert"] == RecoveryAlert::Stalled.as_str()
                && IDLE_REASONS
                    .iter()
                    .any(|reason| e.payload["reason"] == *reason)
        };
        for at in events
            .iter()
            .filter(|e| e.kind == "auto_repaired" && e.payload["repair"] == "send_instruction")
            .filter_map(at_event)
        {
            watch.input_sent(at, None);
        }
        let repaired = |e: &crate::domain::RunEvent| {
            e.payload["applied"]
                .as_array()
                .is_some_and(|applied| applied.iter().any(|a| a != "wait"))
        };
        watch.recovered_from = events
            .iter()
            .filter(idle_job)
            .rfind(|e| e.kind == "recovery_finished" && repaired(e))
            .and_then(at_event);
        // The idle's recovery job the previous supervisor requested and
        // whose end is not recorded: a job it left running is gone (the
        // adopter starts another, counted as one more), and the session
        // moving on ends it as this watch's own would.
        if let Some(requested) = events
            .iter()
            .filter(idle_job)
            .rfind(|e| e.kind == "recovery_requested")
        {
            let attempt = requested.payload["attempt"].as_u64().unwrap_or(0) as usize;
            let of_attempt = |e: &&crate::domain::RunEvent| {
                e.payload["attempt"].as_u64() == Some(attempt as u64)
            };
            let ended = events.iter().filter(of_attempt).any(|e| {
                e.kind == "stall_resolved"
                    && e.payload["phase"] == PHASE
                    && e.payload["detection"] == "recovery"
            });
            if !ended {
                let applied = events.iter().filter(idle_job).filter(of_attempt).find(|e| {
                    e.kind == "recovery_finished"
                        && e.payload["applied"]
                            .as_array()
                            .is_some_and(|a| a.iter().any(|a| a != "wait"))
                });
                let reason = IDLE_REASONS
                    .into_iter()
                    .find(|reason| requested.payload["reason"] == *reason)
                    .unwrap_or(IDLE_WITHOUT_RECEIPT);
                watch.recovering = Some(Box::new(Recovering {
                    attempt,
                    reason,
                    at: at_event(requested).unwrap_or(UNIX_EPOCH),
                    detected_after_secs: requested.payload["idle_secs"].as_i64().unwrap_or(0),
                    repaired_at: applied.and_then(at_event),
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
        if let Some(ask) = stalled_ask(queue, run.id(), None)?
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
                    threshold: threshold_of(ask.id),
                });
            } else {
                watch.held = watch.held.max(Some(at_unix(closed + 1)));
            }
            if let Some(nudge) = &mut watch.nudge {
                nudge.settled = true;
            }
        }
        if let Some(ask) = queue.unclosed_stalled_ask(run.id())? {
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
                threshold: threshold_of(ask.id),
            });
            if applied {
                watch.held = watch.held.max(ask.answered_at.map(|at| at_unix(at + 1)));
            }
            // An ask follows the nudge, which escalated to it.
            if let Some(nudge) = &mut watch.nudge {
                nudge.settled = true;
            }
        } else if let Some(at) = events.iter().rposition(|e| {
                e.kind == event_kind::STALL_RESOLVED
                    && e.payload["phase"] == PHASE
                    && e.payload["detection"] == "ask"
                    && e.payload["reopened"] == true
            })
            && let event = &events[at]
            // Nothing was sent to the session since: an ask opened again
            // then settled by a receipt or a question is not opened again.
            && !events[at..].iter().any(|e| {
                e.kind == event_kind::TURN_REQUESTED
                    || e.kind == event_kind::ASK_DELIVERED
                    || e.kind == event_kind::STALL_NUDGED
            })
            && let Some(previous) = event.payload["ask_id"].as_i64().map(AskId::new)
            && stalled_ask(queue, run.id(), None)?
                .is_some_and(|ask| ask.id == previous && ask.closed_at.is_some())
        {
            // The previous supervisor closed an ask answered `intervene`
            // and stopped before it opened the next one.
            watch.reopen = Some(Reopen {
                previous,
                at: at_event(event).unwrap_or(UNIX_EPOCH),
                detected_after_secs: event.payload["detected_after_secs"].as_i64().unwrap_or(0),
                threshold: threshold_of(previous),
            });
            if let Some(nudge) = &mut watch.nudge {
                nudge.settled = true;
            }
        }
        Ok(watch)
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
    pub(super) fn resolved_payload(
        sv: &Supervisor<'_>,
        detection: &str,
        threshold: &'static str,
        detected_after_secs: i64,
        detected_at: SystemTime,
        outcome: &str,
    ) -> Value {
        json!({
            "phase": PHASE,
            "detection": detection,
            "threshold": threshold,
            "threshold_secs": match threshold {
                BACKGROUND_THRESHOLD => sv.stall.background_alert_secs,
                IDLE_PROCESS_THRESHOLD => sv.stall.idle_process_secs,
                SEND_THRESHOLD => sv.stall.send_confirm_secs,
                _ => sv.stall.idle_without_receipt_secs,
            },
            "detected_after_secs": detected_after_secs,
            "outcome": outcome,
            "resolved_after_secs": secs_between(detected_at, sv.files.now()),
        })
    }

    /// Record `payload` as `stall_resolved`, naming the ask when there is
    /// one.
    pub(super) fn record_resolved(
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        mut payload: Value,
        ask: Option<AskId>,
        detection: &str,
        outcome: &str,
    ) -> Result<()> {
        if let Some(id) = ask {
            payload["ask_id"] = json!(id);
        }
        sv.queue
            .record_runtime_event(run.id(), EventKind::StallResolved, payload)?;
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
        let mut payload = Self::resolved_payload(
            sv,
            "recovery",
            IDLE_THRESHOLD,
            recovering.detected_after_secs,
            recovering.at,
            outcome,
        );
        payload["attempt"] = json!(recovering.attempt);
        payload["reason"] = json!(recovering.reason);
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
        // The caller looked for the receipt before the idle was read: a
        // turn that wrote its receipt and ended in between is not a stall,
        // and a nudge would start another turn under the receipt's
        // validation (task 1328). The next pass sees the receipt.
        if let Some(receipt) = run.receipt_path()
            && sv.files.is_file(Path::new(receipt))
        {
            return Ok(None);
        }
        let modified = idle.modified();
        // The session has not ended a turn since the last text it was sent,
        // or since a person stepped in.
        if !self.ended_after_inputs(modified) {
            return Ok(None);
        }
        let start = modified;
        // A question the session waits on is not a stall.
        if sv.queue.has_unclosed_worker_question(run.id())? {
            return Ok(None);
        }
        // A session stopped at a login that ran out waits for a person to
        // log in, in the queue's one authentication ask (ADR-0047 decision
        // 42), not for a nudge or a stalled ask of its own. An answered
        // hold not applied yet, or a `done` whose text to go on is not
        // typed yet, still holds it: the text follows, not a nudge.
        if sv.queue.hold_unclosed(run.id())? || sv.hold_continue.contains_key(run.id()) {
            return Ok(None);
        }
        let idle_secs = secs_between(start, now);
        let Some(recovery) = recovery else {
            // Nothing is typed and no job starts: a stall before its
            // nudge, or one for a recovery job, waits for the slot. After a
            // person's `wait` the ask follows straight away.
            let Some(nudge) = self.nudge else {
                return Ok(None);
            };
            if self.wait_from.is_none() {
                return Ok(None);
            }
            // A headless session takes no turn by itself: after `wait` the
            // ask follows its next turn.
            if self.after_wait(idle.modified()) == Some(false) {
                return Ok(None);
            }
            self.open_ask(sv, run, workspace, &idle, idle_secs, Some(nudge), now, None)?;
            return Ok(None);
        };
        {
            self.observe_turn(
                sv,
                run,
                workspace,
                idle_marker,
                &idle,
                idle_secs,
                now,
                recovery,
            )
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
        // A turn that failed or was stopped ends the session: its run goes
        // to its recovery job once the wrapper exited, and nothing is sent.
        if mark.is_some_and(|mark| !mark.outcome.goes_on(mark.failure)) {
            return Ok(None);
        }
        // Its provider cannot be used: the next call goes to the other one,
        // or the run waits in the hold ask (ADR-t813-2).
        if let Some(failure) = provider_failure(mark) {
            sv.turn_at_wall(run, workspace, failure)?;
            return Ok(None);
        }
        // After a person's `wait` the ask follows the next turn: a headless
        // session takes none by itself.
        if let Some(after) = self.after_wait(idle.modified()) {
            if after {
                self.open_ask(sv, run, workspace, idle, idle_secs, self.nudge, now, None)?;
            }
            return Ok(None);
        }
        // A question closed without its answer is told as the next turn,
        // in place of the nudge or the recovery job (task 1372).
        if self.recovering.is_none()
            && let Some(closed) = closed_notice(sv, run, idle)?
        {
            match notice_step(sv, run, closed.ask_id) {
                NoticeStep::Send => {
                    return self.send_notice(sv, run, workspace, idle, idle_secs, now, closed);
                }
                NoticeStep::Wait => return Ok(None),
                NoticeStep::GaveUp => {}
            }
        }
        if let Some(reason) = alert_at_once(mark) {
            return self.recover(
                sv, run, workspace, idle, idle_secs, self.nudge, now, recovery, reason,
            );
        }
        if usize::from(self.nudges) < HEADLESS_NUDGES && self.recovering.is_none() {
            return self.send_nudge(sv, run, workspace, idle, idle_secs, now);
        }
        self.recover(
            sv,
            run,
            workspace,
            idle,
            idle_secs,
            self.nudge,
            now,
            recovery,
            TURN_WITHOUT_RECEIPT,
        )
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
                .find(|e| e.kind == "stall_nudged" && e.payload["phase"] == PHASE)
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
        match submit(sv, run, workspace, Input::Text(&text), "nudge") {
            Ok(_submission) => {
                self.input_sent(sent_at, Some(&text));
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
        match submit(sv, run, workspace, Input::Text(&closed.text), what) {
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
                self.input_sent(sent_at, Some(&closed.text));
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
                        at: Instant::now(),
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
        let mut payload = json!({
            "phase": PHASE,
            "idle_secs": idle_secs,
            "threshold_secs": sv.stall.idle_without_receipt_secs,
            "background_running": idle.background_running_evidence(),
            "background_tasks": idle.background_tasks(),
            "workspace_id": workspace,
        });
        if let Some(ask) = closed_ask {
            payload["closed_ask"] = json!(ask);
        }
        sv.queue
            .record_runtime_event(run.id(), EventKind::StallNudged, payload)?;
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
            let mut payload = Self::resolved_payload(
                sv,
                "ask",
                reopen.threshold,
                reopen.detected_after_secs,
                reopen.at,
                "answered_intervene",
            );
            payload["reopened"] = json!(true);
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
        let Some(ask) = sv.queue.unclosed_stalled_ask(run.id())? else {
            // Someone closed it: its answer `wait` counts again, anything
            // else (or none) is a person who took the session over.
            self.asked = None;
            let answer =
                stalled_ask(&*sv.queue, run.id(), Some(asked.id))?.and_then(|ask| ask.answer);
            let wait = answered_wait(answer.as_deref());
            // A headless session's `intervene` holds nothing: its ask is
            // opened again (task 1179).
            if answer.as_deref().is_some_and(answered_intervene) {
                let reopen = Reopen {
                    previous: asked.id,
                    at: asked.at,
                    detected_after_secs: asked.detected_after_secs,
                    threshold: asked.threshold,
                };
                self.reopen = Some(reopen);
                self.reopen_ask(sv, run, reopen, now)?;
                self.reopen = None;
                return Ok(());
            }
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
            return Ok(());
        };
        let moved = |since: SystemTime| marker_moved(marker, since);
        match ask.answer.as_deref().map(str::trim) {
            None if moved(asked.at) => {
                self.close(sv, run, STALL_MOVED_CLOSED, "resolved_by_itself")?;
            }
            None => (),
            Some(_) if asked.applied => {
                if self.held.is_none_or(moved) {
                    self.close(sv, run, STALL_MOVED_CLOSED, "resolved_by_itself")?;
                }
            }
            Some(answer) if answered_wait(Some(answer)) => {
                // Closed first: a supervisor that stops in between leaves
                // a closed `wait` its adopter reads as one.
                sv.queue.close_ask(ask.id)?;
                Self::resolved(
                    sv,
                    run,
                    "ask",
                    Some((ask.id, asked.threshold)),
                    asked.detected_after_secs,
                    asked.at,
                    "answered_wait",
                )?;
                self.asked = None;
                if asked.threshold != SEND_THRESHOLD {
                    self.wait_from = Some(now);
                }
                info!(ask_id = %ask.id, run_id = %run.id(), "stalled ask {} of {} answered wait; counting its idle again", ask.id, run.id());
            }
            // A headless session takes no keys, so a person has no way in:
            // `intervene` (answered to an ask opened before it was taken
            // off its options, or typed as text) closes the ask and opens
            // it again with the headless options, and nothing is sent
            // (task 1179). Its outcome is recorded before the ask is
            // closed, and marked `reopened`: an adopter of a supervisor
            // that stopped in between opens the ask, once, without
            // recording it again.
            Some(answer) if answered_intervene(answer) => {
                if !ask_resolved(&*sv.queue, run, ask.id)? {
                    let mut payload = Self::resolved_payload(
                        sv,
                        "ask",
                        asked.threshold,
                        asked.detected_after_secs,
                        asked.at,
                        "answered_intervene",
                    );
                    payload["reopened"] = json!(true);
                    Self::record_resolved(
                        sv,
                        run,
                        payload,
                        Some(ask.id),
                        "ask",
                        "answered_intervene",
                    )?;
                }
                sv.queue.close_ask(ask.id)?;
                self.asked = None;
                let reopen = Reopen {
                    previous: ask.id,
                    at: asked.at,
                    detected_after_secs: asked.detected_after_secs,
                    threshold: asked.threshold,
                };
                self.reopen = Some(reopen);
                self.reopen_ask(sv, run, reopen, now)?;
                self.reopen = None;
            }
            // `stop` has a headless session end: its exit request, sent
            // once (the wrapper stops a running turn and exits), never as
            // a turn. The run then ends without a receipt: validating fails
            // it and its recovery job takes it (task 1104).
            Some(answer) if answer == STOP_OPTION => {
                // Sent once the run is back in its slot.
                if !send {
                    return Ok(());
                }
                // Written before the ask is closed: an adopter of a
                // supervisor that stopped in between finds it and closes
                // the ask without writing it again.
                if exit_requested(sv, run) {
                    info!(ask_id = %ask.id, run_id = %run.id(), "the exit of {} was already requested; stalled ask {} not sent again", run.id(), ask.id);
                } else {
                    let workspace = run.workspace_id().unwrap_or_default().to_owned();
                    submit(sv, run, &workspace, Input::Exit, "/exit")?;
                }
                sv.queue.close_ask(ask.id)?;
                Self::resolved(
                    sv,
                    run,
                    "ask",
                    Some((ask.id, asked.threshold)),
                    asked.detected_after_secs,
                    asked.at,
                    "answered_stop",
                )?;
                self.asked = None;
                self.held = Some(now);
                warn!(ask_id = %ask.id, run_id = %run.id(), "stalled ask {} of {} answered stop; the headless session is asked to exit, and the run goes to its recovery job without a receipt", ask.id, run.id());
            }
            // A headless session takes no keys: a person's answer other
            // than `intervene` is its next turn's prompt (ADR-t813-1
            // decision 6).
            Some(answer) if headless_delivers(answer) => {
                // Sent once the run is back in its slot.
                if !send {
                    return Ok(());
                }
                // The request names the ask and is written before the ask
                // is closed: a supervisor that stopped in between left it,
                // and its adopter closes the ask without writing another
                // (task 863).
                let what = stalled_answer_what(ask.id);
                if let Some(seq) = requested(sv, run, &what)? {
                    info!(ask_id = %ask.id, run_id = %run.id(), "the answer of stalled ask {} of {} was already requested (request {seq}); not sent again", ask.id, run.id());
                } else {
                    let text = answer_text(run, ask.id, answer);
                    let workspace = run.workspace_id().unwrap_or_default().to_owned();
                    let sent_at = sv.files.now();
                    submit(sv, run, &workspace, Input::Text(&text), &what)?;
                    self.input_sent(sent_at, Some(&text));
                }
                sv.queue.close_ask(ask.id)?;
                Self::resolved(
                    sv,
                    run,
                    "ask",
                    Some((ask.id, asked.threshold)),
                    asked.detected_after_secs,
                    asked.at,
                    "answered_instruction",
                )?;
                self.asked = None;
                info!(ask_id = %ask.id, run_id = %run.id(), "stalled ask {} of {} answered; its answer is the headless session's next turn", ask.id, run.id());
            }
            Some(_) => {
                Self::resolved(
                    sv,
                    run,
                    "ask",
                    Some((ask.id, asked.threshold)),
                    asked.detected_after_secs,
                    asked.at,
                    "answered_intervene",
                )?;
                self.asked = Some(Asked {
                    applied: true,
                    ..asked
                });
                self.held = Some(now);
                info!(ask_id = %ask.id, run_id = %run.id(), "stalled ask {} of {} answered for a person to step in; no ask until the session moves", ask.id, run.id());
            }
        }
        Ok(())
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
