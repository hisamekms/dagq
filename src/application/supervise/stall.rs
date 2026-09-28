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
/// an interactive session's idle after its nudge, and a headless session's
/// turn that ended so after its nudges, or refused too many permissions
/// (ADR-t813-1 decision 9).
pub(super) const IDLE_REASONS: [&str; 3] = [
    IDLE_WITHOUT_RECEIPT,
    TURN_WITHOUT_RECEIPT,
    PERMISSION_DENIED,
];

/// The setting the idle detections are judged by.
pub(super) const IDLE_THRESHOLD: &str = "idle_without_receipt_secs";

/// The setting the `long_background` alert is judged by.
pub(super) const BACKGROUND_THRESHOLD: &str = "background_alert_secs";

/// The setting the checks that a sent text was taken are judged by (the
/// `stalled` alert of reason `send_unconfirmed`).
pub(super) const SEND_THRESHOLD: &str = "send_confirm_secs";

/// The options of a `stalled` ask: leave the session alone and ask again
/// if it stays idle, or have a person step in.
pub(super) const STALLED_OPTIONS: [&str; 2] = ["wait", "intervene"];

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

/// The answers the runtime writes into a `stalled` ask it closes.
pub(super) const STALL_MOVED_CLOSED: &str = "the session moved on; closed by the runtime";

/// Whether a `stalled` ask's answer leaves the session alone
/// ([`crate::domain::stats::thresholds::answer_waits`]).
fn answered_wait(answer: Option<&str>) -> bool {
    answer.is_some_and(crate::domain::stats::thresholds::answer_waits)
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

/// Why the session's watch parks its run for a session of its own.
#[derive(Debug, Clone)]
enum Park {
    /// A recovery job's `resume`, with its instruction.
    Resume(String),
    /// The run moves to headless Codex.
    Switch(PendingSwitch),
}

/// Receipt-less idle of one session, judged each tick.
#[derive(Debug, Clone, Default)]
pub(super) struct StallWatch {
    nudge: Option<Nudge>,
    /// How many nudges the phase sent: one for an interactive session, up
    /// to [`HEADLESS_NUDGES`] for a headless one.
    nudges: u8,
    /// The latest text the supervisor typed into the session: a marker no
    /// newer is not the end of a turn that answered it.
    last_input: Option<SystemTime>,
    /// The latest texts the supervisor typed (at most [`SENDS_KEPT`]): when,
    /// and the fingerprint of the text when known. An input marker one of
    /// them explains is not a person's.
    sends: Vec<(SystemTime, Option<u64>)>,
    /// The latest input the session took, by its input marker: typed by a
    /// person or the supervisor, or a notice the agent put in by itself.
    /// An idle marker no newer is not the end of the turn it started.
    taken: Option<SystemTime>,
    /// The latest idle marker seen, for an input whose turn ended between
    /// two observations.
    seen_idle: Option<SystemTime>,
    /// The first observation only takes the input marker in (sets
    /// [`Self::taken`]): an adopted run's marker may be an input the
    /// previous supervisor judged.
    prime_input: bool,
    asked: Option<Asked>,
    /// The answer `wait` restarts the count here.
    wait_from: Option<SystemTime>,
    /// After `intervene` (or an ask a person closed), no ask until the
    /// session ends a turn after this.
    held: Option<SystemTime>,
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

/// Seconds from `from` to `to`, zero when `to` is earlier.
fn secs_between(from: SystemTime, to: SystemTime) -> i64 {
    to.duration_since(from)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
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

/// The sends of the supervisor's an input marker is matched against.
const SENDS_KEPT: usize = 16;

/// Slack past two [`confirm_wait`]s (the send, and the text sent again)
/// within which an input is taken to be the supervisor's send.
const SEND_SLACK_SECS: i64 = 10;

/// Whether a send of the supervisor's at `sent` explains an input taken at
/// `at`: taken no earlier than a second before it, and within two
/// `confirm_secs` (the text may be sent again once) and a slack after it.
fn send_explains(sent: SystemTime, at: SystemTime, confirm_secs: i64) -> bool {
    at + Duration::from_secs(1) >= sent
        && secs_between(sent, at) <= confirm_secs.saturating_mul(2) + SEND_SLACK_SECS
}

fn at_unix(secs: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).unwrap_or(0))
}

fn at_event(event: &crate::domain::RunEvent) -> Option<SystemTime> {
    crate::domain::stats::timestamp_millis(&event.created_at)
        .map(|ms| UNIX_EPOCH + Duration::from_millis(u64::try_from(ms).unwrap_or(0)))
}

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
            prime_input: true,
            ..Self::default()
        };
        watch.nudges = events
            .iter()
            .filter(|e| e.kind == event_kind::STALL_NUDGED && e.payload["phase"] == PHASE)
            .count()
            .try_into()
            .unwrap_or(u8::MAX);
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
        // A person stepped in on an ask closed since.
        watch.held = latest_outcome("answered_intervene");
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
            } else {
                watch.held = watch.held.max(Some(at_unix(closed + 1)));
            }
            if let Some(nudge) = &mut watch.nudge {
                nudge.settled = true;
            }
        }
        if let Some(ask) = queue.unclosed_stalled_ask(run.id())? {
            let applied = ask.answered_at.is_some() && resolved("ask", Some(ask.id));
            // A recovery job's escalation names its ask (ADR-0047), and its
            // alert the setting it was judged by.
            let recovery = events.iter().find(|e| {
                e.kind == event_kind::RECOVERY_FINISHED && e.payload["ask_id"] == json!(ask.id)
            });
            watch.asked = Some(Asked {
                id: ask.id,
                at: at_unix(ask.created_at + 1),
                detected_after_secs: 0,
                applied,
                threshold: match recovery {
                    Some(e) if e.payload["alert"] == RecoveryAlert::IdleProcess.as_str() => {
                        IDLE_PROCESS_THRESHOLD
                    }
                    Some(e) if e.payload["alert"] == RecoveryAlert::LongBackground.as_str() => {
                        BACKGROUND_THRESHOLD
                    }
                    Some(e) if e.payload["reason"] == SEND_UNCONFIRMED => SEND_THRESHOLD,
                    _ => IDLE_THRESHOLD,
                },
            });
            if applied {
                watch.held = watch.held.max(ask.answered_at.map(|at| at_unix(at + 1)));
            }
            // An ask follows the nudge, which escalated to it.
            if let Some(nudge) = &mut watch.nudge {
                nudge.settled = true;
            }
        }
        Ok(watch)
    }

    /// The supervisor typed `text` (when known) into the session at `at`.
    pub(super) fn input_sent(&mut self, at: SystemTime, text: Option<&str>) {
        self.last_input = Some(self.last_input.map_or(at, |last| last.max(at)));
        self.sends.push((at, text.map(text_fingerprint)));
        if self.sends.len() > SENDS_KEPT {
            self.sends.remove(0);
        }
    }

    /// When the supervisor last typed a text into the session.
    pub(super) fn last_send(&self) -> Option<SystemTime> {
        self.last_input
    }

    /// Whether a text the supervisor typed explains `input`: the same text
    /// typed no later than a second after it (an answer the session took
    /// only after its turn), or any text typed within the window of
    /// [`send_explains`] before it.
    fn sent_by_supervisor(&self, input: &InputMarker, confirm_secs: i64) -> bool {
        self.sends.iter().any(|(sent, text)| {
            send_explains(*sent, input.modified, confirm_secs)
                || (text.is_some()
                    && *text == input.text
                    && input.modified + Duration::from_secs(1) >= *sent)
        })
    }

    /// Whether the session took an input at `input` (its input marker) no
    /// earlier than the last text the supervisor typed: the text, if any,
    /// is not still waiting to be taken.
    pub(super) fn taken_after_sends(&self, input: SystemTime) -> bool {
        self.last_input.is_none_or(|at| input >= at)
    }

    /// Whether the session ended a turn (its idle marker `marker`) since the
    /// last text the supervisor typed and the last input it took.
    pub(super) fn turn_since_input(&self, marker: SystemTime) -> bool {
        self.last_input.is_none_or(|at| marker > at) && self.taken.is_none_or(|at| marker > at)
    }

    /// The session took `input` (its input marker), `idle` being its idle
    /// marker now: the input counts as its last one, like a text the
    /// supervisor typed, so a turn it started by itself (from a person's
    /// input, or a notice that its background work ended) is not taken
    /// for a stall. Returns the idle seconds to record as
    /// `stall_preempted` when the input is new, typed, explained by no
    /// send of the supervisor's, and taken while the session was idle
    /// (its idle marker older than it) short of `threshold_secs`, with no
    /// `stalled` ask open and no person stepped in since its last turn
    /// (ADR-0043 decision 3). A notice of the agent's, or an input the
    /// marker does not say the source of, is never counted.
    pub(super) fn input_taken(
        &mut self,
        input: InputMarker,
        idle: Option<SystemTime>,
        threshold_secs: i64,
        confirm_secs: i64,
    ) -> Option<i64> {
        let before = self.seen_idle;
        if idle.is_some() {
            self.seen_idle = idle;
        }
        if self.taken.is_some_and(|at| input.modified <= at) {
            return None;
        }
        self.taken = Some(input.modified);
        if std::mem::take(&mut self.prime_input)
            || input.source != InputSource::Typed
            || self.asked.is_some()
        {
            return None;
        }
        // The idle the input ended: the marker now, or the one seen before
        // when the turn it started has ended already.
        let idle = [idle, before]
            .into_iter()
            .flatten()
            .filter(|idle| *idle < input.modified)
            .max()?;
        if self.held.is_some_and(|at| idle <= at) || self.sent_by_supervisor(&input, confirm_secs) {
            return None;
        }
        let from = self.wait_from.map_or(idle, |at| at.max(idle));
        let idle_secs = secs_between(from, input.modified);
        (idle_secs < threshold_secs).then_some(idle_secs)
    }

    /// Record the input a person typed before the idle detection as
    /// `stall_preempted`, unless the session waits at a dialog or on a
    /// question to a person, which is not an idle.
    fn preempted(
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        idle_secs: i64,
        dialog: bool,
    ) -> Result<()> {
        if dialog
            || sv.queue.has_unclosed_worker_question(run.id())?
            || sv.queue.has_unclosed_ask(run.id(), AskKind::AnswerPrompt)?
        {
            return Ok(());
        }
        sv.queue.record_runtime_event(
            run.id(),
            EventKind::StallPreempted,
            json!({
                "phase": PHASE,
                "threshold": IDLE_THRESHOLD,
                "threshold_secs": sv.stall.idle_without_receipt_secs,
                "idle_secs": idle_secs,
            }),
        )?;
        info!(run_id = %run.id(), "run {} took an input the supervisor did not send after {idle_secs}s idle without a receipt", run.id());
        Ok(())
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

    /// A recovery job chose `resume` with `instruction`: the session's
    /// watch parks the run on its next step.
    pub(super) fn request_park(&mut self, instruction: String) {
        self.park = Some(Box::new(Park::Resume(instruction)));
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

    /// The interactive session stopped at a wall and its run moved to
    /// headless Codex (ADR-t813-2 decision 5): the session's watch parks
    /// the run for a session of Codex's, which `instruction` starts.
    pub(super) fn request_switch(&mut self, switch: PendingSwitch) {
        self.park = Some(Box::new(Park::Switch(switch)));
    }

    /// Whether the session's watch is to park the run on its next step.
    pub(super) fn parking(&self) -> bool {
        self.park.is_some()
    }

    /// The move to Codex, once.
    pub(super) fn take_switch(&mut self) -> Option<PendingSwitch> {
        match self.park.take().map(|park| *park) {
            Some(Park::Switch(switch)) => Some(switch),
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
        if self.nudge.is_none_or(|n| n.settled) && self.asked.is_none() && self.recovering.is_none()
        {
            return Ok(());
        }
        if !receipt && !sv.queue.has_unclosed_worker_question(run.id())? {
            return Ok(());
        }
        let outcome = self.moved_outcome();
        self.recovery_resolved(sv, run, outcome)?;
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
    /// `/exit` requested. `dialog` is a `prompt_waiting` not cleared.
    /// Returns what was typed into the session, for the check that it was
    /// taken ([`StartCheck`]).
    pub(super) fn poll(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle_marker: &Path,
        dialog: bool,
        recovery: StallRecovery<'_, '_>,
    ) -> Result<Option<StartCheck>> {
        self.observe(sv, run, workspace, idle_marker, dialog, Some(recovery))
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
        dialog: bool,
    ) -> Result<()> {
        self.observe(sv, run, workspace, idle_marker, dialog, None)
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
        dialog: bool,
        recovery: Option<StallRecovery<'_, '_>>,
    ) -> Result<Option<StartCheck>> {
        self.followed = false;
        let now = sv.files.now();
        // Without a marker newer than its last input, the session's screen
        // stands in for it (ADR-t803-1).
        let idle = sv.session_idle(
            run,
            workspace,
            idle_marker,
            self.last_input.unwrap_or(UNIX_EPOCH),
            PHASE,
        )?;
        let marker = idle.as_ref().map(IdleMarker::modified);
        match InputMarker::read(&*sv.files, sv.signals, idle_marker)? {
            Some(input) => {
                if let Some(idle_secs) = self.input_taken(
                    input,
                    marker,
                    sv.stall.idle_without_receipt_secs,
                    sv.stall.send_confirm_secs,
                ) {
                    Self::preempted(sv, run, idle_secs, dialog)?;
                }
            }
            None => self.prime_input = false,
        }
        // The session moved since the idle's recovery job started (or since
        // its repair): the job's detection ended, and a job still running
        // is stopped by the session's watch ([`Self::recovering`]).
        if let Some(recovering) = self.recovering.as_deref() {
            let since = recovering.repaired_at.unwrap_or(recovering.at);
            if marker.is_some_and(|m| m > since) || self.taken.is_some_and(|t| t > since) {
                let outcome = self.moved_outcome();
                self.recovery_resolved(sv, run, outcome)?;
            }
        }
        if self.asked.is_some() {
            self.watch_ask(sv, run, marker, now, recovery.is_some())?;
            return Ok(None);
        }
        let Some(idle) = idle else {
            return Ok(None);
        };
        let modified = idle.modified();
        // The session has not ended a turn since the last text it was sent,
        // or since a person stepped in.
        if self.last_input.is_some_and(|at| modified <= at)
            || self.held.is_some_and(|at| modified <= at)
        {
            return Ok(None);
        }
        // An input it took since its last turn ended started a turn of its
        // own: no stall while it runs. A turn a person interrupted (Esc)
        // ends with no idle marker, so past the threshold from the input
        // the screen decides: the idle counts from the input unless the
        // agent is at work.
        let open_input = self.taken.filter(|at| modified <= *at);
        let start = open_input.unwrap_or(modified);
        let from = [self.wait_from, self.recovered_from]
            .into_iter()
            .flatten()
            .fold(start, SystemTime::max);
        // A headless turn that ended is done for good: nothing to wait out
        // (ADR-t813-1 decision 9).
        let threshold = if headless(run) {
            0
        } else {
            sv.stall.idle_without_receipt_secs
        };
        if secs_between(from, now) < threshold {
            return Ok(None);
        }
        // A dialog or an ask the session waits at is not a stall.
        if dialog
            || sv.queue.has_unclosed_worker_question(run.id())?
            || sv.queue.has_unclosed_ask(run.id(), AskKind::AnswerPrompt)?
        {
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
            if open_input.is_some() || self.wait_from.is_none() {
                return Ok(None);
            }
            // A headless session takes no turn by itself: after `wait` the
            // ask follows its next turn.
            if headless(run) && self.wait_from.is_some_and(|wait| idle.modified() <= wait) {
                return Ok(None);
            }
            self.open_ask(sv, run, workspace, &idle, idle_secs, Some(nudge), now, None)?;
            return Ok(None);
        };
        if headless(run) {
            return self.observe_turn(
                sv,
                run,
                workspace,
                idle_marker,
                &idle,
                idle_secs,
                now,
                recovery,
            );
        }
        let screen = sv.cmux.capture(workspace);
        if open_input.is_some() && screen.as_ref().ok().is_none_or(|s| sv.signals.working(s)) {
            return Ok(None);
        }
        if let Ok(screen) = screen {
            if let Some(wall) = sv.signals.screen_wall(&screen) {
                // An interactive Claude worker moves to headless Codex
                // (ADR-t813-2 decision 5), else it joins the hold ask.
                if self.parking() {
                    return Ok(None);
                }
                if let Some(switch) = interactive_switch(sv, run, wall)? {
                    self.request_switch(switch);
                    return Ok(None);
                }
                if raise_wall(sv, run, workspace, &screen, wall)? {
                    return Ok(None);
                }
            }
            // A Settings panel left open would take the nudge: it is closed
            // first, and the nudge follows on a later tick (ADR-0047
            // decision 29).
            if answer_known_dialog(sv, run, workspace, &screen, false, None)? {
                return Ok(None);
            }
        }
        match self.nudge {
            None => self.send_nudge(sv, run, workspace, &idle, idle_secs, now),
            // A person answered `wait` to the ask the stall came to: asked
            // again, without another job (ADR-0047 decision 30).
            Some(nudge) if self.wait_from.is_some() => {
                self.open_ask(sv, run, workspace, &idle, idle_secs, Some(nudge), now, None)?;
                Ok(None)
            }
            Some(nudge) => self.recover(
                sv,
                run,
                workspace,
                &idle,
                idle_secs,
                Some(nudge),
                now,
                recovery,
                IDLE_WITHOUT_RECEIPT,
            ),
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
    ) -> Result<Option<StartCheck>> {
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
        if let Some(wait) = self.wait_from {
            if idle.modified() > wait {
                self.open_ask(sv, run, workspace, idle, idle_secs, self.nudge, now, None)?;
            }
            return Ok(None);
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
    ) -> Result<Option<StartCheck>> {
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
        if let Some(mark) = headless(run)
            .then(|| run.idle_marker_path().ok())
            .flatten()
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
                Ok(applied.sent.map(|(text, sent_at, submission)| {
                    self.input_sent(sent_at, Some(&text));
                    StartCheck::new("recovery instruction", &text, sent_at, &submission)
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

    /// Record `stall_nudged` and type the nudge, once in the phase. A nudge
    /// that could not be typed is noted; the ask follows on the next tick.
    fn send_nudge(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle: &IdleMarker,
        idle_secs: i64,
        now: SystemTime,
    ) -> Result<Option<StartCheck>> {
        let background = idle.background_tasks();
        sv.queue.record_runtime_event(
            run.id(),
            EventKind::StallNudged,
            json!({
                "phase": PHASE,
                "idle_secs": idle_secs,
                "threshold_secs": sv.stall.idle_without_receipt_secs,
                "background_running": idle.background_running_evidence(),
                "background_tasks": background,
                "workspace_id": workspace,
            }),
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
        let text = stall_nudge(run, idle_secs, background, idle.background_running())?;
        let sent_at = sv.files.now();
        match submit(sv, run, workspace, Input::Text(&text), "nudge") {
            Ok(submission) => {
                self.input_sent(sent_at, Some(&text));
                info!(run_id = %run.id(), "run {} was idle without a receipt for {idle_secs}s; nudged it in workspace {workspace}", run.id());
                Ok(Some(StartCheck::new("nudge", &text, sent_at, &submission)))
            }
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "the nudge of {} could not be typed into workspace {workspace}: {error:#}; asking the inbox instead", run.id());
                Ok(None)
            }
        }
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
        workspace: &str,
        idle: &IdleMarker,
        idle_secs: i64,
        nudge: Option<Nudge>,
        now: SystemTime,
        note: Option<&Note>,
    ) -> Result<AskId> {
        let background = match idle.background_tasks() {
            [] if idle.background_running() => {
                "background work was running (not listed)".to_owned()
            }
            [] => "no background task was running".to_owned(),
            tasks => tasks
                .iter()
                .map(|t| format!("- {} ({}): {}", t.description, t.id, t.command))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        let recovered = note.map_or_else(String::new, |note| {
            format!(" And {}.\n{}\n", note.why, note.text)
        });
        let nudged = nudge.map_or_else(
            || "although its turns were refused permissions too often to get on".to_owned(),
            |nudge| {
                format!(
                    "although the supervisor nudged it {}s ago to write its receipt, ask a worker_question or say what it waits for",
                    secs_between(nudge.at, now)
                )
            },
        );
        let question = if headless(run) {
            // A headless session has no screen and takes no keys: a
            // person steps in through the run (ADR-t813-1 decision 4).
            format!(
                "The headless session of run {run_id} (task {task_id}) ended its turn without a receipt or an open question ({idle_secs}s ago, phase: {PHASE}), {nudged}.{recovered} Answer `wait` to leave the session alone (the supervisor asks again after its next turn), or `intervene` to step in yourself (a headless session takes no keys: stop the run and recover it, see the dagq-recover skill). Answer `propose` (or `propose: <why>`) to have a planner of the runtime's propose a remedy for its cause; the session is then left alone as for `wait`. This ask closes itself once the session moves on.\n\n{turns}",
                run_id = run.id(),
                task_id = run.task_id(),
                turns = turns_excerpt(sv, run),
            )
        } else {
            let screen = match sv.cmux.capture(workspace) {
                Ok(screen) => sv.signals.screen_excerpt(&screen),
                Err(error) => format!("(the screen could not be read: {error:#})"),
            };
            format!(
                "The session of run {run_id} (task {task_id}) in workspace {workspace} has been idle without a receipt for {idle_secs}s (reason: idle_without_receipt, phase: {PHASE}), {nudged}.{recovered} Answer `wait` to leave the session alone (the supervisor asks again if it stays idle for another {threshold}s), or `intervene` to step in yourself (read the screen, stop or check its background work, type an instruction, or stop the run and recover it; see the dagq-recover skill). Answer `propose` (or `propose: <why>`) to have a planner of the runtime's propose a remedy for its cause; the session is then left alone as for `wait`. This ask closes itself once the session moves on.\n\nBackground tasks when it stopped:\n{background}\n\nLast lines of the screen:\n{screen}",
                run_id = run.id(),
                task_id = run.task_id(),
                threshold = sv.stall.idle_without_receipt_secs,
            )
        };
        let options = stalled_options(note);
        let outcome = ask::ask(
            &mut *sv.queue,
            &sv.layout.main_checkout,
            NewAsk {
                kind: AskKind::Stalled,
                task_id: Some(run.task_id()),
                run_id: Some(run.id().clone()),
                question,
                options,
                asked_by: SessionRole::Supervisor.as_str().into(),
                reason_category: note.map_or(AskReason::RecoveryFailed, |note| note.category),
                topics: Vec::new(),
                finding_id: None,
            },
            sv.cmux,
        )?;
        let id = AskId::new(outcome["id"].as_i64().context("ask returned no id")?);
        warn!(ask_id = %id, run_id = %run.id(), "run {} stays idle without a receipt after its nudge; stalled ask {id} (notified: {})", run.id(), outcome["notified"]);
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

    /// Follow the `stalled` ask a recovery job of a session in a revise or
    /// a resume escalated to (their watch runs no idle detection):
    /// close it once the session moves on and apply its answer.
    pub(super) fn follow_ask(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        idle_marker: &Path,
    ) -> Result<()> {
        if self.asked.is_none() {
            return Ok(());
        }
        let marker = sv.files.modified(idle_marker).ok();
        let now = sv.files.now();
        self.watch_ask(sv, run, marker, now, true)
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
        let Some(asked) = self.asked else {
            return Ok(());
        };
        let Some(ask) = sv.queue.unclosed_stalled_ask(run.id())? else {
            // Someone closed it: its answer `wait` counts again, anything
            // else (or none) is a person who took the session over.
            self.asked = None;
            let wait = stalled_ask(&*sv.queue, run.id(), Some(asked.id))?
                .is_some_and(|ask| answered_wait(ask.answer.as_deref()));
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
        let moved = |since: SystemTime| marker.is_some_and(|m| m > since);
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
            // A headless session takes no keys: a person's answer other
            // than `intervene` is its next turn's prompt (ADR-t813-1
            // decision 6).
            Some(answer) if headless(run) && !answer.starts_with("intervene") => {
                // Sent once the run is back in its slot.
                if !send {
                    return Ok(());
                }
                let text = answer_text(run, ask.id, answer);
                let workspace = run.workspace_id().unwrap_or_default().to_owned();
                let sent_at = sv.files.now();
                submit(
                    sv,
                    run,
                    &workspace,
                    Input::Text(&text),
                    "answer of the stalled ask",
                )?;
                self.input_sent(sent_at, Some(&text));
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

    const THRESHOLD: i64 = 1200;
    const CONFIRM: i64 = 60;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000_000 + secs)
    }

    fn typed(secs: u64) -> InputMarker {
        InputMarker {
            modified: at(secs),
            source: InputSource::Typed,
            text: None,
        }
    }

    /// Task 380: an input the session took after its idle marker (a person's,
    /// or a notice of its background work) starts a turn: no turn has ended
    /// since, so there is no idle to nudge until its idle marker follows.
    #[test]
    fn an_input_newer_than_the_idle_marker_holds_the_nudge_off() {
        for source in [InputSource::Typed, InputSource::Agent, InputSource::Unknown] {
            let mut watch = StallWatch::default();
            assert!(watch.turn_since_input(at(100)));
            watch.input_taken(
                InputMarker {
                    modified: at(200),
                    source,
                    text: None,
                },
                Some(at(100)),
                THRESHOLD,
                CONFIRM,
            );
            // The idle marker is older than the input: still the turn it
            // started, however long it takes.
            assert!(!watch.turn_since_input(at(100)), "{source:?}");
            assert!(!watch.turn_since_input(at(200)), "{source:?}");
            // Its end is a turn since the input.
            assert!(watch.turn_since_input(at(201)), "{source:?}");
        }
        // The supervisor's own texts count as before.
        let mut watch = StallWatch::default();
        watch.input_sent(at(300), None);
        watch.input_taken(typed(301), Some(at(100)), THRESHOLD, CONFIRM);
        assert!(!watch.turn_since_input(at(301)));
        assert!(watch.turn_since_input(at(302)));
    }

    /// ADR-0043 decision 3: an input typed while the session was idle short
    /// of the threshold, which no send of the supervisor's explains, is a
    /// person who stepped in before the detection: once, with how long the
    /// session had been idle.
    #[test]
    fn a_typed_input_short_of_the_threshold_is_preempted_once() {
        let mut watch = StallWatch::default();
        assert_eq!(
            watch.input_taken(typed(700), Some(at(100)), THRESHOLD, CONFIRM),
            Some(600)
        );
        // The same marker seen again records nothing more.
        assert_eq!(
            watch.input_taken(typed(700), Some(at(100)), THRESHOLD, CONFIRM),
            None
        );
        // Its turn ended between two looks: the idle seen before counts.
        let mut watch = StallWatch::default();
        assert_eq!(
            watch.input_taken(typed(10), Some(at(5)), THRESHOLD, CONFIRM),
            Some(5)
        );
        assert_eq!(
            watch.input_taken(typed(300), Some(at(100)), THRESHOLD, CONFIRM),
            Some(200)
        );
        assert_eq!(
            watch.input_taken(typed(900), Some(at(950)), THRESHOLD, CONFIRM),
            Some(800)
        );
        // After `wait`, the idle counts from the answer.
        let mut watch = StallWatch {
            wait_from: Some(at(500)),
            ..StallWatch::default()
        };
        assert_eq!(
            watch.input_taken(typed(700), Some(at(100)), THRESHOLD, CONFIRM),
            Some(200)
        );
    }

    #[test]
    fn no_preemption_past_the_threshold_or_after_a_send_of_the_supervisor() {
        // Past the threshold, the detection was due: no preemption.
        let mut watch = StallWatch::default();
        assert_eq!(
            watch.input_taken(typed(1300), Some(at(100)), THRESHOLD, CONFIRM),
            None
        );
        // Right after a send of the supervisor's (the text, or the text
        // sent again after the confirm wait), the input is that send.
        for taken in [100, 101, 160, 230] {
            let mut watch = StallWatch::default();
            watch.input_sent(at(100), None);
            assert_eq!(
                watch.input_taken(typed(taken), Some(at(50)), THRESHOLD, CONFIRM),
                None,
                "{taken}"
            );
        }
        // Long after it, it is a person's again.
        let mut watch = StallWatch::default();
        watch.input_sent(at(100), None);
        assert_eq!(
            watch.input_taken(typed(400), Some(at(300)), THRESHOLD, CONFIRM),
            Some(100)
        );
    }

    /// Two answers typed back to back: the session takes the first at once
    /// and the second only after the first one's turn, long after it was
    /// typed. Each is the supervisor's, by its time or by its text.
    #[test]
    fn every_recent_send_of_the_supervisor_explains_its_input() {
        let mut watch = StallWatch::default();
        watch.input_sent(at(100), Some("answer to ask 1: blue"));
        watch.input_sent(at(103), Some("answer to ask 2: red"));
        let first = InputMarker {
            text: Some(text_fingerprint("answer to ask 1: blue")),
            ..typed(100)
        };
        assert_eq!(
            watch.input_taken(first, Some(at(50)), THRESHOLD, CONFIRM),
            None
        );
        // Queued behind a 10-minute turn, and matched by its text.
        let second = InputMarker {
            text: Some(text_fingerprint("answer to ask 2: red\n")),
            ..typed(700)
        };
        assert_eq!(
            watch.input_taken(second, Some(at(650)), THRESHOLD, CONFIRM),
            None
        );
        // Another text at that time is a person's.
        let other = InputMarker {
            text: Some(text_fingerprint("please stop")),
            ..typed(800)
        };
        assert_eq!(
            watch.input_taken(other, Some(at(750)), THRESHOLD, CONFIRM),
            Some(50)
        );
        // A text typed before the send is not explained by it.
        let mut watch = StallWatch::default();
        watch.input_sent(at(500), Some("hello"));
        let early = InputMarker {
            text: Some(text_fingerprint("hello")),
            ..typed(300)
        };
        assert_eq!(
            watch.input_taken(early, Some(at(100)), THRESHOLD, CONFIRM),
            Some(200)
        );
        // Only the latest sends are kept.
        let mut watch = StallWatch::default();
        for n in 0..=SENDS_KEPT as u64 {
            watch.input_sent(at(n), None);
        }
        assert_eq!(watch.sends.len(), SENDS_KEPT);
        assert_eq!(watch.sends[0].0, at(1));
    }

    #[test]
    fn only_a_typed_input_ending_an_idle_is_preempted() {
        // A notice of the agent's own, or an input the marker does not say
        // the source of, is not counted.
        for source in [InputSource::Agent, InputSource::Unknown] {
            let mut watch = StallWatch::default();
            let input = InputMarker {
                modified: at(700),
                source,
                text: None,
            };
            assert_eq!(
                watch.input_taken(input, Some(at(100)), THRESHOLD, CONFIRM),
                None
            );
        }
        // Before any idle (the first prompt), there was no idle to end.
        let mut watch = StallWatch::default();
        assert_eq!(watch.input_taken(typed(10), None, THRESHOLD, CONFIRM), None);
        assert_eq!(
            watch.input_taken(typed(20), Some(at(30)), THRESHOLD, CONFIRM),
            None
        );
        // An adopted run's first marker is only taken in.
        let mut watch = StallWatch {
            prime_input: true,
            ..StallWatch::default()
        };
        assert_eq!(
            watch.input_taken(typed(700), Some(at(100)), THRESHOLD, CONFIRM),
            None
        );
        assert_eq!(
            watch.input_taken(typed(900), Some(at(800)), THRESHOLD, CONFIRM),
            Some(100)
        );
        // While a stalled ask is open, or after a person stepped in with no
        // turn ended since, typing is the answer, not a preemption.
        let mut watch = StallWatch {
            asked: Some(Asked {
                id: AskId::new(1),
                at: at(50),
                detected_after_secs: 0,
                applied: false,
                threshold: IDLE_THRESHOLD,
            }),
            ..StallWatch::default()
        };
        assert_eq!(
            watch.input_taken(typed(700), Some(at(100)), THRESHOLD, CONFIRM),
            None
        );
        let mut watch = StallWatch {
            held: Some(at(150)),
            ..StallWatch::default()
        };
        assert_eq!(
            watch.input_taken(typed(700), Some(at(100)), THRESHOLD, CONFIRM),
            None
        );
    }
}
