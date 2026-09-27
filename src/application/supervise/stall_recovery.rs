//! The `stalled` alert of a live session's recovery job (ADR-0047
//! decisions 30, 31 and 39): the session's side of the idle without a
//! receipt after its nudge (the [`StallWatch`] detects it and hands it to
//! the job), and a text the supervisor typed that the session did not take
//! (`submit_unconfirmed` of a text, `submit_not_started`, reason
//! `send_unconfirmed`). Both go to the recovery job before any ask; only
//! an escalation, a verdict of low confidence, a repair whose preconditions
//! no longer hold, a failed job (task 442's acceptance; every live alert
//! does the same since ADR-t609-1) and an alert past its attempts open the
//! `stalled` ask,
//! which the [`StallWatch`] follows. A `resume` repair parks a run in its
//! first session for a session of its own ([`SessionWatch::park_for_resume`]).
//!
//! The send's detection is read from the run's events, so a supervisor
//! that adopts the run hands a send to no second job and asks no second
//! time: a send is handed once (`recovery_requested` names it as
//! `send_event`), and its end is recorded once (`stall_resolved` of
//! `detection: recovery` with the same `send_event`).

use super::*;
use crate::domain::RunEvent;
use crate::domain::recovery::{HEADLESS_STALLED_ACTIONS, SEND_UNCONFIRMED, STALLED_ACTIONS};

/// The events of `events` that start a new session: a send before one of
/// them is not this session's.
fn starts_session(event: &RunEvent) -> bool {
    event.kind == "resume_started"
}

/// The latest send of the session that was not taken: a text still in the
/// input box after its Enters (`submit_unconfirmed`, never a `/exit`,
/// whose timeout is the `stuck_exit` alert's), or one that showed no sign
/// of work (`submit_not_started`).
fn unconfirmed_send(events: &[RunEvent]) -> Option<&RunEvent> {
    events
        .iter()
        .rev()
        .take_while(|e| !starts_session(e))
        .find(|e| {
            e.kind == "submit_not_started"
                || (e.kind == "submit_unconfirmed" && e.payload["input"] != "exit")
        })
}

/// Whether `event` is about the send `send`.
fn of_send(event: &RunEvent, send: EventId) -> bool {
    event.payload["send_event"] == json!(send)
}

/// The latest recovery job requested for the send `send`, and its attempt.
fn latest_job(events: &[RunEvent], send: EventId) -> Option<(&RunEvent, u64)> {
    events
        .iter()
        .rfind(|e| e.kind == "recovery_requested" && of_send(e, send))
        .map(|e| (e, e.payload["attempt"].as_u64().unwrap_or(0)))
}

/// Whether the detection of the send's job `attempt` ended
/// (`stall_resolved` of `detection: recovery`).
fn job_ended(events: &[RunEvent], send: EventId, attempt: u64) -> bool {
    events.iter().any(|e| {
        e.kind == "stall_resolved"
            && e.payload["detection"] == "recovery"
            && of_send(e, send)
            && e.payload["attempt"].as_u64() == Some(attempt)
    })
}

/// Whether a job's `recovery_finished` applied a repair (a `wait` repairs
/// nothing).
fn repaired(finished: &RunEvent) -> bool {
    finished.payload["applied"]
        .as_array()
        .is_some_and(|applied| applied.iter().any(|a| a != "wait"))
}

/// When `event` was recorded, on the files' wall clock.
fn recorded_at(event: &RunEvent) -> SystemTime {
    crate::domain::stats::timestamp_millis(&event.created_at).map_or(UNIX_EPOCH, |ms| {
        UNIX_EPOCH + Duration::from_millis(u64::try_from(ms).unwrap_or(0))
    })
}

/// What the person is told the session did with the send.
fn situation(send: &RunEvent) -> String {
    let what = send.payload["what"].as_str().unwrap_or("text");
    if send.kind == "submit_unconfirmed" {
        let enters = send.payload["retries"].as_u64().unwrap_or(0) + 1;
        return format!(
            "the {what} the supervisor typed stays in the input box after {enters} Enters, not submitted"
        );
    }
    if let Some(dialog) = send.payload["dialog"].as_str() {
        return format!("a {dialog} dialog came up after the supervisor sent the {what}");
    }
    format!(
        "the session showed no sign of work within {}s of the {what} the supervisor sent{}",
        send.payload["waited_secs"].as_u64().unwrap_or(0),
        if send.payload["resent"] == true {
            " twice"
        } else {
            ""
        }
    )
}

impl SessionWatch {
    /// Whether the session is idle at its prompt, for `send_instruction`:
    /// no dialog recorded, and a turn ended since the last input (the last
    /// text the supervisor typed, and for a session asked to fix its run
    /// the last input of its stage).
    pub(super) fn at_prompt(&self, sv: &Supervisor<'_>) -> bool {
        self.prompt_hash.is_none()
            && sv.files.modified(&self.idle_marker).is_ok_and(|marker| {
                self.stall.turn_since_input(marker) && self.input_at.is_none_or(|at| marker > at)
            })
    }

    /// One look at the first session's idle without a receipt: the nudge,
    /// then its recovery job and the `stalled` ask ([`StallWatch::poll`]).
    /// A job whose detection ended (the session moved on) is stopped.
    /// Returns what was typed into the session, for the check that it was
    /// taken.
    pub(super) fn watch_stall(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<Option<StartCheck>> {
        let live = Live {
            workspace: &self.workspace,
            run_dir: &self.run_dir,
            // A headless session has no dialog to answer (ADR-t813-1).
            allowed: if headless(run) {
                &HEADLESS_STALLED_ACTIONS
            } else {
                &STALLED_ACTIONS
            },
            exit_typed: false,
            at_prompt: self.at_prompt(sv),
            lands: false,
            park: self.input_at.is_none(),
        };
        let start = self.stall.poll(
            sv,
            run,
            &self.workspace,
            &self.idle_marker,
            self.prompt_hash.is_some(),
            StallRecovery {
                recovery: &mut self.recovery,
                live: &live,
            },
        )?;
        let followed = self.stall.followed();
        self.stop_idle_job(sv, run, followed);
        Ok(start)
    }

    /// Stop the recovery job of the idle once its detection ended (the
    /// session wrote its receipt, asked a question or moved on), or when the
    /// last observation did not follow it (`followed` false: a dialog, a
    /// question, a hold or an input came first), which leaves nobody to act
    /// on its verdict while it holds the one job of the session.
    pub(super) fn stop_idle_job(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun, followed: bool) {
        let outcome = if !self.stall.recovering() {
            "session_moved"
        } else if !followed {
            "alert_cleared"
        } else {
            return;
        };
        for reason in IDLE_REASONS {
            self.recovery
                .stop_reason(sv, run, RecoveryAlert::Stalled, reason, outcome);
        }
    }

    /// Whether the session showed it took something after `at`: its idle
    /// marker, its input marker (unless a notice of the agent's own) or its
    /// receipt written since.
    fn moved_since(&self, sv: &Supervisor<'_>, at: SystemTime) -> Result<bool> {
        let newer = |path: &Path| sv.files.modified(path).is_ok_and(|m| m > at);
        let input = InputMarker::read(&*sv.files, sv.signals, &self.idle_marker)?;
        Ok(newer(&self.idle_marker)
            || newer(&self.receipt_path)
            || input.is_some_and(|input| input.source != InputSource::Agent && input.modified > at))
    }

    /// One look at the sends the session did not take (ADR-0047 decision
    /// 31): the latest such send of the session is handed to a recovery job
    /// (`stalled`, reason `send_unconfirmed`) unless the session moved on
    /// since, a person already looks at it (a `stalled` ask, a dialog the
    /// `prompt_waiting` alert follows, or a question of the worker's; a job
    /// running then is stopped), or a job already repaired or escalated it.
    /// Each job's detection ends once (`stall_resolved`): the session moved,
    /// the next job started, the ask opened, or the stage ended
    /// ([`Self::end_sends`]). An escalation opens the `stalled` ask the
    /// [`StallWatch`] follows; for a session asked to fix its run (whose
    /// watch runs no idle detection) it is followed here. Returns when an
    /// instruction a repair typed was sent, and its check.
    pub(super) fn watch_sends(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<Option<(SystemTime, StartCheck)>> {
        if self.input_at.is_some() {
            self.stall.follow_ask(sv, run, &self.idle_marker)?;
        }
        let reason = Some(SEND_UNCONFIRMED);
        let events = sv.queue.run_events(run.id())?;
        let Some(send) = unconfirmed_send(&events) else {
            return Ok(None);
        };
        let id = send.id;
        let latest = latest_job(&events, id);
        let open = latest.filter(|(_, attempt)| !job_ended(&events, id, *attempt));
        // Its last job's detection ended (the session moved, or a person
        // was asked): nothing more for this send.
        if latest.is_some() && open.is_none() {
            return Ok(None);
        }
        let attempt = open.map(|(_, attempt)| attempt);
        // The end of its job, or of an escalation that named it (an alert
        // past its attempts starts no job).
        let finished = events.iter().rfind(|e| {
            e.kind == "recovery_finished"
                && (of_send(e, id)
                    || (e.payload["alert"] == RecoveryAlert::Stalled.as_str()
                        && attempt.is_some()
                        && e.payload["attempt"].as_u64() == attempt))
        });
        if self.moved_since(sv, recorded_at(send))? {
            self.recovery.stop_reason(
                sv,
                run,
                RecoveryAlert::Stalled,
                SEND_UNCONFIRMED,
                "session_moved",
            );
            // A send handed to a job ends here: the job's repair moved the
            // session, or it moved by itself.
            if let Some((requested, attempt)) = open {
                let outcome = if finished.is_some_and(repaired) {
                    "resolved_by_recovery"
                } else {
                    "resolved_by_itself"
                };
                self.send_resolved(sv, run, id, attempt, requested, outcome)?;
            }
            return Ok(None);
        }
        if sv.queue.has_unclosed_ask(run.id(), AskKind::Stalled)?
            || self.prompt_hash.is_some()
            || self.waits_for_question(sv, run)?
        {
            self.recovery.stop_reason(
                sv,
                run,
                RecoveryAlert::Stalled,
                SEND_UNCONFIRMED,
                "alert_cleared",
            );
            return Ok(None);
        }
        // A job of this send that ended: its repair waits for the session
        // to move (only a `wait` asks for another job), and its escalation
        // or failure is with a person. A job whose end is not recorded went
        // with the supervisor that ran it, and one stopped while a person
        // looked starts again.
        let running = self
            .recovery
            .running_for(RecoveryAlert::Stalled, reason)
            .map(|attempt| attempt as u64);
        if let Some(finished) = finished.filter(|_| running != attempt) {
            let only_wait = finished.payload["applied"]
                .as_array()
                .is_some_and(|applied| !applied.is_empty() && applied.iter().all(|a| a == "wait"));
            let gone = matches!(
                finished.payload["outcome"].as_str(),
                Some("session_ended" | "session_moved" | "alert_cleared")
            );
            if !only_wait && !gone {
                return Ok(None);
            }
        }
        let facts = json!({
            "send_event": id,
            "evidence": [id],
            "send": send.payload["what"],
            "event": send.kind,
            "dialog": send.payload["dialog"],
            "waited_secs": send.payload["waited_secs"],
            "excerpt": send.payload["excerpt"],
            "threshold": SEND_THRESHOLD,
            "threshold_secs": sv.stall.send_confirm_secs,
        });
        let live = Live {
            workspace: &self.workspace,
            run_dir: &self.run_dir,
            allowed: &STALLED_ACTIONS,
            exit_typed: false,
            at_prompt: self.at_prompt(sv),
            lands: false,
            park: self.input_at.is_none(),
        };
        let step =
            self.recovery
                .follow_for(sv, run, &live, RecoveryAlert::Stalled, reason, || facts)?;
        // Another job of the send started: the one before went on to it.
        if let Some((requested, before)) = open
            && self
                .recovery
                .running_for(RecoveryAlert::Stalled, reason)
                .is_some_and(|now| now as u64 != before)
        {
            self.send_resolved(sv, run, id, before, requested, "escalated")?;
        }
        match step {
            LiveStep::Pending => Ok(None),
            LiveStep::Repaired(applied) => {
                if let Some(instruction) = applied.resume {
                    self.stall.request_park(instruction);
                }
                Ok(applied.sent.map(|(text, sent_at, submission)| {
                    self.stall.input_sent(sent_at, Some(&text));
                    (
                        sent_at,
                        StartCheck::new("recovery instruction", &text, sent_at, &submission),
                    )
                }))
            }
            LiveStep::Escalate(attempt, escalation) => {
                self.escalate_send(sv, run, send, attempt, &escalation)?;
                Ok(None)
            }
        }
    }

    /// The stage of the session ended (it exited, or its revise or resume
    /// is over): the job of a send not taken is stopped, and its detection
    /// ends with the run (`run_ended`).
    pub(super) fn end_sends(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        self.end_sends_with(sv, run, "run_ended")
    }

    /// [`Self::end_sends`] with the outcome of the send's detection.
    fn end_sends_with(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        outcome: &str,
    ) -> Result<()> {
        self.recovery.stop_reason(
            sv,
            run,
            RecoveryAlert::Stalled,
            SEND_UNCONFIRMED,
            "session_ended",
        );
        let events = sv.queue.run_events(run.id())?;
        let Some(send) = unconfirmed_send(&events) else {
            return Ok(());
        };
        if let Some((requested, attempt)) = latest_job(&events, send.id)
            && !job_ended(&events, send.id, attempt)
        {
            self.send_resolved(sv, run, send.id, attempt, requested, outcome)?;
        }
        Ok(())
    }

    /// Park the run for a session of its own, as a recovery job's `resume`
    /// asked (ADR-0047 decision 40, task 442): the stalled detections end
    /// with the repair, their asks close, the jobs stop, and the run
    /// becomes `needs_session` with the instruction, which the resume's
    /// request carries (`recovery_parked`). The phase then asks the session
    /// to exit and lets the lease go; the supervisor resumes the run.
    pub(super) fn park_for_resume(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        instruction: &str,
    ) -> Result<TaskRun> {
        self.stall.parked(sv, run)?;
        self.end_sends_with(sv, run, "resolved_by_recovery")?;
        self.stall.ended(sv, run)?;
        self.recovery.stop(sv, run);
        let reason = format!(
            "the supervisor's recovery job sent the stalled session back to a session of its own: {instruction}"
        );
        let parked = sv.queue.park_live(
            run.id(),
            &sv.token,
            &reason,
            json!({
                "instruction": instruction,
                "alert": RecoveryAlert::Stalled,
                "workspace_id": self.workspace,
            }),
        )?;
        info!(run_id = %run.id(), "run {} is parked for a resume by its recovery job; its session is asked to exit", run.id());
        Ok(parked)
    }

    /// Record how the recovery job `attempt` of the send `send` ended
    /// (`stall_resolved`, `detection: recovery`), once.
    fn send_resolved(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        send: EventId,
        attempt: u64,
        requested: &RunEvent,
        outcome: &str,
    ) -> Result<()> {
        let mut payload = StallWatch::resolved_payload(
            sv,
            "recovery",
            SEND_THRESHOLD,
            requested.payload["waited_secs"].as_i64().unwrap_or(0),
            recorded_at(requested),
            outcome,
        );
        payload["attempt"] = json!(attempt);
        payload["send_event"] = json!(send);
        payload["reason"] = json!(SEND_UNCONFIRMED);
        StallWatch::record_resolved(sv, run, payload, None, "recovery", outcome)
    }

    /// Raise the send the recovery job did not repair as the `stalled` ask
    /// (reason `send_unconfirmed`), with the job's diagnosis, its options
    /// added and its reason category, and hand the ask to the
    /// [`StallWatch`]. An ask already open keeps a person looking: the
    /// diagnosis is only recorded.
    fn escalate_send(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        send: &RunEvent,
        attempt: usize,
        escalation: &Escalation,
    ) -> Result<()> {
        let alert = RecoveryAlert::Stalled;
        let note = escalation.note(run, alert, attempt);
        let extra = json!({"reason": SEND_UNCONFIRMED, "send_event": send.id});
        let asked = if sv.queue.has_unclosed_ask(run.id(), AskKind::Stalled)? {
            None
        } else {
            let excerpt = send.payload["excerpt"].as_str().unwrap_or("(not read)");
            let question = format!(
                "The session of run {run_id} (task {task_id}) in workspace {workspace}: {situation} (reason: send_unconfirmed), and {why}.\n{text}\nAnswer `wait` to leave the session alone, or `intervene` to step in yourself (read the screen, press Enter or type the text again in that workspace, or stop the run and recover it; see the dagq-recover skill). This ask closes itself once the session moves on.\n\nLast lines of the screen:\n{excerpt}",
                run_id = run.id(),
                task_id = run.task_id(),
                workspace = self.workspace,
                situation = situation(send),
                why = note.why,
                text = note.text,
            );
            let outcome = ask::ask(
                &mut *sv.queue,
                &sv.layout.main_checkout,
                NewAsk {
                    kind: AskKind::Stalled,
                    task_id: Some(run.task_id()),
                    run_id: Some(run.id().clone()),
                    question,
                    options: stalled_options(Some(&note)),
                    asked_by: SessionRole::Supervisor.as_str().into(),
                    reason_category: note.category,
                    finding_id: None,
                },
                sv.cmux,
            )?;
            let id = AskId::new(outcome["id"].as_i64().context("ask returned no id")?);
            warn!(ask_id = %id, run_id = %run.id(), "run {}: {}; {}; stalled ask {id} (notified: {})", run.id(), situation(send), note.why, outcome["notified"]);
            let waited = send.payload["waited_secs"].as_i64().unwrap_or(0);
            self.stall
                .escalated(id, sv.files.now(), waited, SEND_THRESHOLD);
            Some(id)
        };
        escalation.record(sv, run, alert, attempt, &note, asked, extra)?;
        // The detection of its last job went on to the ask (for an alert
        // past its attempts, the job before, whose `wait` led here).
        let events = sv.queue.run_events(run.id())?;
        if let Some((requested, attempt)) = latest_job(&events, send.id)
            && !job_ended(&events, send.id, attempt)
        {
            self.send_resolved(sv, run, send.id, attempt, requested, "escalated")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    #[test]
    fn the_latest_send_of_the_session_not_taken_is_found() {
        let events = vec![
            event(1, "submit_not_started", json!({"what": "answer"})),
            event(2, "submit_unconfirmed", json!({"input": "exit"})),
        ];
        assert_eq!(
            unconfirmed_send(&events).map(|e| e.id),
            Some(EventId::new(1))
        );
        let mut resumed = events.clone();
        resumed.push(event(3, "resume_started", json!({})));
        assert!(unconfirmed_send(&resumed).is_none());
        resumed.push(event(4, "submit_unconfirmed", json!({"input": "text"})));
        assert_eq!(
            unconfirmed_send(&resumed).map(|e| e.id),
            Some(EventId::new(4))
        );
    }

    #[test]
    fn the_situation_says_what_happened_to_the_send() {
        let stuck = event(
            1,
            "submit_unconfirmed",
            json!({"what": "resolution request", "retries": 3}),
        );
        assert_eq!(
            situation(&stuck),
            "the resolution request the supervisor typed stays in the input box after 4 Enters, not submitted"
        );
        let lost = event(
            2,
            "submit_not_started",
            json!({"what": "nudge", "waited_secs": 60, "resent": true}),
        );
        assert_eq!(
            situation(&lost),
            "the session showed no sign of work within 60s of the nudge the supervisor sent twice"
        );
        let dialog = event(
            3,
            "submit_not_started",
            json!({"what": "answer of ask 4", "dialog": "permission"}),
        );
        assert_eq!(
            situation(&dialog),
            "a permission dialog came up after the supervisor sent the answer of ask 4"
        );
    }
}
