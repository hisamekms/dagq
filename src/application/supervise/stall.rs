//! A worker's session idle without a receipt (ADR-0043 decision 1): the
//! [`StallWatch`] of the first session nudges it once and, if it stays
//! idle, raises it to the inbox as a `stalled` ask, recording how each
//! detection ended (`stall_resolved`, decision 3).
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

/// The setting the idle detections are judged by.
pub(super) const IDLE_THRESHOLD: &str = "idle_without_receipt_secs";

/// The setting the `long_background` alert is judged by.
pub(super) const BACKGROUND_THRESHOLD: &str = "background_alert_secs";

/// The options of a `stalled` ask: leave the session alone and ask again
/// if it stays idle, or have a person step in.
pub(super) const STALLED_OPTIONS: [&str; 2] = ["wait", "intervene"];

/// The answers the runtime writes into a `stalled` ask it closes.
pub(super) const STALL_MOVED_CLOSED: &str = "the session moved on; closed by the runtime";

/// Whether a `stalled` ask's answer leaves the session alone: `wait`, or
/// `propose` (ADR-0044 decision 19), which hands the cause to a planner
/// of the runtime's instead of a person stepping in.
fn answered_wait(answer: Option<&str>) -> bool {
    answer.is_some_and(|answer| {
        answer.trim() == "wait"
            || matches!(
                crate::domain::FindingAnswer::parse(answer),
                Some(crate::domain::FindingAnswer::Propose(_))
            )
    })
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

/// Receipt-less idle of one session, judged each tick.
#[derive(Debug, Clone, Default)]
pub(super) struct StallWatch {
    nudge: Option<Nudge>,
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
        // An answer the previous supervisor typed is an input too.
        for at in events
            .iter()
            .filter(|e| e.kind == event_kind::ASK_DELIVERED)
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
                    Some(_) => BACKGROUND_THRESHOLD,
                    None => IDLE_THRESHOLD,
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
            event_kind::STALL_PREEMPTED,
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
    fn resolved(
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        detection: &str,
        ask: Option<(AskId, &'static str)>,
        detected_after_secs: i64,
        detected_at: SystemTime,
        outcome: &str,
    ) -> Result<()> {
        let threshold = ask.map_or(IDLE_THRESHOLD, |(_, threshold)| threshold);
        let mut payload = json!({
            "phase": PHASE,
            "detection": detection,
            "threshold": threshold,
            "threshold_secs": match threshold {
                BACKGROUND_THRESHOLD => sv.stall.background_alert_secs,
                IDLE_PROCESS_THRESHOLD => sv.stall.idle_process_secs,
                _ => sv.stall.idle_without_receipt_secs,
            },
            "detected_after_secs": detected_after_secs,
            "outcome": outcome,
            "resolved_after_secs": secs_between(detected_at, sv.files.now()),
        });
        if let Some((id, _)) = ask {
            payload["ask_id"] = json!(id);
        }
        sv.queue
            .record_runtime_event(run.id(), event_kind::STALL_RESOLVED, payload)?;
        info!(run_id = %run.id(), "stall of {} ({detection}) ended: {outcome}", run.id());
        Ok(())
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
        if self.nudge.is_none_or(|n| n.settled) && self.asked.is_none() {
            return Ok(());
        }
        if !receipt && !sv.queue.has_unclosed_worker_question(run.id())? {
            return Ok(());
        }
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
    ) -> Result<Option<StartCheck>> {
        self.observe(sv, run, workspace, idle_marker, dialog, false)
    }

    /// The same observation for a run that waits for a person outside
    /// its slot (ADR-0062 decision 6): the `stalled` ask is followed and
    /// its answer applied, and one more ask opens after a `wait`, but
    /// nothing is sent to the session (no nudge, no key to a dialog).
    pub(super) fn poll_quiet(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle_marker: &Path,
        dialog: bool,
    ) -> Result<()> {
        self.observe(sv, run, workspace, idle_marker, dialog, true)
            .map(|_| ())
    }

    fn observe(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle_marker: &Path,
        dialog: bool,
        quiet: bool,
    ) -> Result<Option<StartCheck>> {
        let now = sv.files.now();
        let idle = IdleMarker::read(&*sv.files, sv.signals, idle_marker)?;
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
        if self.asked.is_some() {
            self.watch_ask(sv, run, marker, now)?;
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
        let from = self.wait_from.map_or(start, |at| at.max(start));
        let threshold = sv.stall.idle_without_receipt_secs;
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
        // 42), not for a nudge or a stalled ask of its own.
        if sv.queue.hold_of(run.id())?.is_some() {
            return Ok(None);
        }
        let idle_secs = secs_between(start, now);
        if quiet {
            // Nothing is typed: a stall before its nudge waits for the slot.
            let Some(nudge) = self.nudge else {
                return Ok(None);
            };
            if open_input.is_some() {
                return Ok(None);
            }
            self.open_ask(sv, run, workspace, &idle, idle_secs, nudge, now)?;
            return Ok(None);
        }
        let screen = sv.cmux.capture(workspace);
        if open_input.is_some() && screen.as_ref().ok().is_none_or(|s| sv.signals.working(s)) {
            return Ok(None);
        }
        if let Ok(screen) = screen {
            if sv.signals.auth_required(&screen) && raise_auth(sv, run, workspace, &screen)? {
                return Ok(None);
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
            Some(nudge) => {
                self.open_ask(sv, run, workspace, &idle, idle_secs, nudge, now)?;
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
            event_kind::STALL_NUDGED,
            json!({
                "phase": PHASE,
                "idle_secs": idle_secs,
                "threshold_secs": sv.stall.idle_without_receipt_secs,
                "background_running": idle.background_running(),
                "background_tasks": background,
                "workspace_id": workspace,
            }),
        )?;
        self.nudge = Some(Nudge {
            at: now,
            detected_after_secs: idle_secs,
            settled: false,
        });
        let text = stall_nudge(run, idle_secs, background)?;
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
    /// a `stalled` ask to the inbox.
    #[allow(clippy::too_many_arguments)]
    fn open_ask(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle: &IdleMarker,
        idle_secs: i64,
        nudge: Nudge,
        now: SystemTime,
    ) -> Result<()> {
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
        let screen = match sv.cmux.capture(workspace) {
            Ok(screen) => sv.signals.screen_excerpt(&screen),
            Err(error) => format!("(the screen could not be read: {error:#})"),
        };
        let question = format!(
            "The session of run {run_id} (task {task_id}) in workspace {workspace} has been idle without a receipt for {idle_secs}s (reason: idle_without_receipt, phase: {PHASE}), although the supervisor nudged it {nudged}s ago to write its receipt, ask a worker_question or say what it waits for. Answer `wait` to leave the session alone (the supervisor asks again if it stays idle for another {threshold}s), or `intervene` to step in yourself (read the screen, stop or check its background work, type an instruction, or stop the run and recover it; see the dagq-recover skill). Answer `propose` (or `propose: <why>`) to have a planner of the runtime's propose a remedy for its cause; the session is then left alone as for `wait`. This ask closes itself once the session moves on.\n\nBackground tasks when it stopped:\n{background}\n\nLast lines of the screen:\n{screen}",
            run_id = run.id(),
            task_id = run.task_id(),
            nudged = secs_between(nudge.at, now),
            threshold = sv.stall.idle_without_receipt_secs,
        );
        let outcome = ask::ask(
            &mut *sv.queue,
            &sv.layout.repo_root,
            NewAsk {
                kind: AskKind::Stalled,
                task_id: Some(run.task_id()),
                run_id: Some(run.id().clone()),
                question,
                options: STALLED_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
                asked_by: SessionRole::Supervisor.as_str().into(),
                reason_category: AskReason::RecoveryFailed,
                finding_id: None,
            },
            sv.cmux,
        )?;
        let id = AskId::new(outcome["id"].as_i64().context("ask returned no id")?);
        warn!(ask_id = %id, run_id = %run.id(), "run {} stays idle without a receipt after its nudge; stalled ask {id} (notified: {})", run.id(), outcome["notified"]);
        self.asked = Some(Asked {
            id,
            at: now,
            detected_after_secs: idle_secs,
            applied: false,
            threshold: IDLE_THRESHOLD,
        });
        if !nudge.settled {
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
        Ok(())
    }

    /// Follow the `stalled` ask: close it when the session ended a turn
    /// since (by itself, or after a person stepped in), and apply its
    /// answer: `wait` restarts the count and closes it; anything else is a
    /// person stepping in, and no ask follows until the session ends a turn
    /// after it.
    fn watch_ask(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        marker: Option<SystemTime>,
        now: SystemTime,
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
                self.wait_from = Some(now);
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
                self.wait_from = Some(now);
                info!(ask_id = %ask.id, run_id = %run.id(), "stalled ask {} of {} answered wait; counting its idle again", ask.id, run.id());
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
