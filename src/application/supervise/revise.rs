//! A `revise` verdict or a conflict sent to the live session
//! ([`ReviseWatch`], ADR-0027 decision 2).

use super::*;
use crate::domain::AskConfidence;

/// The phase an idle the screen showed during a revise or a conflict
/// request is recorded with (`idle_inferred`).
pub(super) const REVISE_PHASE: &str = "revise";

/// A `revise` verdict, or a conflict the precheck found, sent to the live
/// session: it is waited for until the session rewrites its receipt and
/// goes idle.
pub(super) struct ReviseWatch {
    pub(super) session: SessionRef,
    pub(super) attempt: usize,
    pub(super) fix: Fix,
    /// A receipt or idle marker no newer than this predates the request.
    pub(super) sent_at: SystemTime,
    pub(super) sent: Instant,
    /// Whether the session took the request, or the last answer typed
    /// (task 285).
    pub(super) start: Option<StartCheck>,
    /// The answers of the session's `worker_question`s and the dialogs it
    /// stops at, followed as a worker's own session's are (task 238).
    pub(super) live: Box<SessionWatch>,
    /// The first second of the `worker_question`s that hold the revise
    /// besides [`SessionWatch::asks_from`]: a session that rewrote its
    /// receipt after its questions went on past them (task 583).
    pub(super) questions_from: i64,
    /// The second this watch's own delivery of an answer closed the last
    /// `worker_question` in: that close is the answer typed at `input_at`,
    /// not one delivered by hand, and moves no clock (task 971, as a
    /// resume's since task 931).
    pub(super) delivered_closed: Option<i64>,
}

/// What the live session was asked to fix (ADR-0027 decisions 2 and 4).
pub(super) enum Fix {
    /// A `revise` verdict's findings, or those of a `concern` whose
    /// `send_back` the runtime applied (`concern`, ADR-t451-1 decision 3).
    Revise {
        reasons: Vec<String>,
        concern: Option<SentBackConcern>,
    },
    /// A conflict with main found after a `pass`; the passed verdict, for
    /// the ask if the session does not resolve it.
    Conflict(ReviewVerdict),
}

impl Fix {
    /// How the request is named in logs and texts: `revise N` or
    /// `conflict request N`.
    pub(super) fn label(&self, attempt: usize) -> String {
        match self {
            Fix::Revise { .. } => format!("revise {attempt}"),
            Fix::Conflict(_) => format!("conflict request {attempt}"),
        }
    }

    /// The `approve_landing` ask when the session cannot fix it: a revise
    /// asks with `summary`; a conflict asks with its passed verdict. A
    /// `send_back` the runtime applied on a concern asks as that concern,
    /// saying the runtime applied it and `why` the session did not fix it,
    /// and records the escalation (task 1392). Neither carries a
    /// recommendation: the one given was applied, and the ask is about
    /// what the session did after it. `requested_by` is the review job the
    /// ask acts on, if any ([`AfterExit::Ask`]).
    pub(super) fn ask(
        &self,
        summary: String,
        why: String,
        requested_by: Option<ActorContext>,
    ) -> AfterExit {
        match self {
            Fix::Revise {
                reasons,
                concern: Some(concern),
            } => AfterExit::Ask {
                decision: ReviewDecision::Concern,
                reasons: reasons.clone(),
                summary: concern.summary.clone(),
                why: Some(concern.why(&why)),
                recommendation: None,
                confidence: None,
                reason_category: None,
                requested_by,
                sent_back: Some(SentBackEscalation {
                    attempt: concern.attempt,
                    why,
                }),
            },
            Fix::Revise {
                reasons,
                concern: None,
            } => AfterExit::Ask {
                decision: ReviewDecision::Revise,
                reasons: reasons.clone(),
                summary,
                why: Some(why),
                recommendation: None,
                confidence: None,
                reason_category: None,
                requested_by,
                sent_back: None,
            },
            // The ask is about the conflict, not a concern's recommendation.
            Fix::Conflict(verdict) => AfterExit::Ask {
                decision: verdict.verdict,
                reasons: verdict.reasons.clone(),
                summary: verdict.summary.clone(),
                why: Some(why),
                recommendation: None,
                confidence: None,
                reason_category: None,
                requested_by,
                sent_back: None,
            },
        }
    }
}

/// A review's `concern` whose `send_back` the runtime applied as a revise
/// (`concern_decided` with `applied: true`, ADR-t451-1 decision 3).
pub(super) struct SentBackConcern {
    /// The review attempt that returned it.
    pub(super) attempt: usize,
    pub(super) summary: String,
    pub(super) confidence: Option<AskConfidence>,
}

impl SentBackConcern {
    /// The concern of review `attempt`, when `verdict` is one.
    pub(super) fn of(verdict: &ReviewVerdict, attempt: usize) -> Option<Self> {
        (verdict.verdict == ReviewDecision::Concern).then(|| SentBackConcern {
            attempt,
            summary: verdict.summary.clone(),
            confidence: verdict.confidence,
        })
    }

    /// Why the ask is a person's: the runtime applied the review's
    /// `send_back`, and the session did not fix it (`why`).
    pub(super) fn why(&self, why: &str) -> String {
        format!(
            "the review recommended send_back ({} confidence), which the runtime applied, but {why}",
            self.confidence.map_or("no", AskConfidence::as_str)
        )
    }
}

/// The escalation an `approve_landing` ask records after a `send_back` the
/// runtime applied (`concern_send_back_escalated`, task 1392): the review
/// `attempt` of its `concern_decided` and why the session did not fix it.
pub(super) struct SentBackEscalation {
    pub(super) attempt: usize,
    pub(super) why: String,
}

pub(super) enum ReviseOutcome {
    /// The receipt was rewritten after the request, names the clean
    /// worktree HEAD (or reports `failed`), and the session went idle after
    /// it; the worktree HEAD at that time. Validation judges the receipt.
    Rewritten(CommitSha),
    /// The session rewrote the receipt and went idle, but the receipt does
    /// not name the clean worktree HEAD (an old commit, a commit after the
    /// receipt, uncommitted changes) or cannot be read: validation would
    /// fail the run and its work, so the session is asked to fix it.
    Mismatch(ReasonCode, String),
    /// The session will not rewrite it: why.
    Ended(String),
}

/// Whether `idle` ends the turn of the revise: written after the last input
/// typed (`input_at`, in a later millisecond, task 1050), with no input the
/// session took since (`input`) still running a turn (task 672).
fn idle_ends_turn(idle: &IdleMarker, input_at: SystemTime, input: Option<&InputMarker>) -> bool {
    super::file_time::written_after(idle.modified(), input_at) && !idle.turn_open_after(input)
}

/// Whether a receipt written at `modified` was rewritten after the request
/// sent at `sent_at`, compared to the millisecond (task 1197): an adopter
/// reads `sent_at` back to the millisecond it was recorded with, and a
/// receipt of that millisecond, or of an earlier one in its second, is the
/// one from before the request.
pub(super) fn rewritten_after(modified: SystemTime, sent_at: SystemTime) -> bool {
    super::file_time::written_after(modified, sent_at)
}

impl ReviseWatch {
    pub(super) fn new(
        run: &TaskRun,
        session: SessionRef,
        attempt: usize,
        fix: Fix,
        sent_at: SystemTime,
        start: Option<StartCheck>,
    ) -> Result<Self> {
        let mut live = Box::new(SessionWatch::fixing(
            run,
            &session.workspace,
            sent_at,
            Stage::Revise,
        )?);
        // A question from before the request (asked during validation or
        // the review, or left open before the receipt) is the inbox's to
        // deliver by hand: it neither gets its answer typed here nor holds
        // the revise (task 582).
        live.asks_from = unix_seconds(sent_at);
        Ok(ReviseWatch {
            session,
            attempt,
            fix,
            sent_at,
            sent: Instant::now(),
            start,
            live,
            questions_from: 0,
            delivered_closed: None,
        })
    }

    /// Another request typed at `sent_at` (a receipt to fix): only what the
    /// session does after it counts.
    pub(super) fn requested(&mut self, sent_at: SystemTime, start: StartCheck) {
        self.sent_at = sent_at;
        self.live.input_at = Some(sent_at);
        self.start = Some(start);
    }

    pub(super) fn poll(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<Option<ReviseOutcome>> {
        let outcome = self.observe(sv, run)?;
        if matches!(
            outcome,
            Some(ReviseOutcome::Rewritten(_) | ReviseOutcome::Ended(_))
        ) {
            // The revise ends here: a dialog recorded during it is no
            // attention any more.
            self.live.clear_prompt(sv, run)?;
            self.live.recovery.stop(sv, run);
            // A `stalled` ask of a send it did not take ends with it.
            match outcome {
                Some(ReviseOutcome::Rewritten(_)) => self.live.stall.settle(sv, run, true)?,
                _ => self.live.stall.ended(sv, run)?,
            }
            self.live.end_sends(sv, run)?;
        }
        Ok(outcome)
    }

    /// The first second of the `worker_question`s that hold the revise.
    pub(super) fn holds_questions_from(&self) -> i64 {
        self.live.asks_from.max(self.questions_from)
    }

    /// Whether the session rewrote `receipt` since the request.
    fn rewritten(&self, sv: &Supervisor<'_>, receipt: &Path) -> bool {
        sv.files
            .modified(receipt)
            .is_ok_and(|modified| rewritten_after(modified, self.sent_at))
    }

    fn observe(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<Option<ReviseOutcome>> {
        let processes = sv.queue.processes(run.id())?;
        let Some(wrapper) = processes
            .iter()
            .find(|p| p.role == "wrapper" && p.exited_at.is_none())
        else {
            return Ok(Some(ReviseOutcome::Ended(
                "ended before it rewrote the receipt".to_owned(),
            )));
        };
        if sv.generators.clock.now() - wrapper.heartbeat_at > HEARTBEAT_TIMEOUT_SECS {
            // A session that died will not rewrite it either (task 236).
            if !sv.wrapper_lives(wrapper) {
                return Ok(Some(ReviseOutcome::Ended(
                    "died without recording its exit".to_owned(),
                )));
            }
            // The exit that follows records `wrapper_heartbeat_expired` and
            // sends the /exit.
            return Ok(Some(ReviseOutcome::Ended(
                "went silent (its wrapper stopped heartbeating while its process lives on)"
                    .to_owned(),
            )));
        }
        // A silence its wait recorded ended before the revise did: the
        // session may wait again, and a later silence is recorded again
        // (task 606).
        self.live.silent = false;
        // An answer to a question the session asked during the revise is
        // typed once it went idle at it: the session works again, and gets
        // the resume timeout again (task 238).
        // A login or usage limit a person fixed (task 437): the session is
        // told to go on, like an answer typed into it.
        if let Some(typed) = self.live.continue_after_hold(sv, run)? {
            self.live.input_at = Some(typed);
            self.sent = Instant::now();
            self.start = self.live.answer_start.take();
        }
        if let Some(typed) = self.live.deliver_answers(sv, run)? {
            self.live.input_at = Some(typed);
            self.sent = Instant::now();
            self.delivered_closed = sv.queue.last_worker_question_closed(run.id())?;
            self.start = self.live.answer_start.take();
        }
        if let Some(agent) = processes
            .iter()
            .find(|p| p.role == "agent" && p.exited_at.is_none())
        {
            self.live.watch_prompt(sv, run, agent)?;
        }
        if let Some(start) = &mut self.start {
            let workspace = self.session.workspace.clone();
            start.poll(sv, run, &workspace, &run.idle_marker_path()?)?;
        }
        // A request or an answer it did not take goes to its recovery job
        // (ADR-0047 decision 31); an instruction the job typed is input.
        if let Some((typed, start)) = self.live.watch_sends(sv, run)? {
            self.live.input_at = Some(typed);
            self.sent = Instant::now();
            self.start = Some(start);
        }
        // A session stopped at its own question, asked since the request,
        // waits for its answer, however long a person takes: it neither went
        // idle without rewriting the receipt nor ran out of time. A receipt
        // it rewrote since the request is judged all the same, as a
        // worker's own session is (task 583): the session went on past the
        // questions it asked before the receipt, which hold it no longer
        // (nor after a request to fix the receipt), and its time runs from
        // there.
        let receipt = Path::new(run.receipt_path().context("missing receipt path")?);
        if sv
            .queue
            .has_unclosed_worker_question_since(run.id(), self.holds_questions_from())?
        {
            let Ok(modified) = sv.files.modified(receipt) else {
                return Ok(None);
            };
            if !rewritten_after(modified, self.sent_at) {
                return Ok(None);
            }
            let from = unix_seconds(modified) + 1;
            if from > self.questions_from {
                self.questions_from = from;
                self.sent = Instant::now();
            }
        }
        // A turn at its provider's wall (ADR-t813-2): the call went to the
        // other provider, or the run waits in the hold ask.
        match self.live.provider_wall(sv, run)? {
            WallGate::Held => return Ok(None),
            WallGate::Moved(_) => {
                self.sent = Instant::now();
                self.start = None;
            }
            WallGate::Open => (),
        }
        // An answer delivered by hand (or by the supervisor this one
        // adopted the run from) is input too: the idle marker of the stop at
        // the question is older than it. Its close is known to the second: a
        // close in a later second than the last input moves it there. The
        // close of an answer this watch typed is recorded after the send,
        // often in a later second: it is that input, and moving the last
        // input to it would leave unseen an idle the session wrote right
        // after the answer (task 971).
        let input_at = self.live.input_at.unwrap_or(self.sent_at);
        if let Some(closed) = sv.queue.last_worker_question_closed(run.id())?
            && closed > unix_seconds(input_at)
            && self.delivered_closed.is_none_or(|own| closed > own)
        {
            self.live.input_at = Some(UNIX_EPOCH + Duration::from_secs(closed.max(0) as u64));
            self.sent = Instant::now();
        }
        // The idle marker is read before the receipt: a receipt rewritten
        // after this read is judged at the next poll, never as idle without
        // it. A marker from before the last input typed is not this turn's.
        // Without such a marker (or one newer than the receipt it
        // rewrote), the screen stands in for it (ADR-t803-1).
        let input_at = self.live.input_at.unwrap_or(self.sent_at);
        let after = sv
            .files
            .modified(receipt)
            .map_or(input_at, |modified| modified.max(input_at));
        let idle_marker = run.idle_marker_path()?;
        let idle = sv.session_idle(
            run,
            &self.session.workspace,
            &idle_marker,
            after,
            REVISE_PHASE,
        )?;
        // An input the session took since its marker (read after it: a
        // notice that its background work ended, or a prompt) started a
        // turn that is still running: only the idle that ends it ends the
        // revise (task 672).
        let input = InputMarker::read(&*sv.files, sv.signals, &idle_marker)?;
        let idle = idle.filter(|idle| idle_ends_turn(idle, input_at, input.as_ref()));
        let rewritten = self.rewritten(sv, receipt);
        let idle_after_receipt = match &idle {
            Some(idle) if rewritten => idle.idle_after_receipt(&*sv.files, receipt)?.is_some(),
            _ => false,
        };
        if idle_after_receipt {
            let worktree = Path::new(run.worktree_path().context("missing worktree")?);
            let head = sv.repository.head(worktree)?;
            let clean = sv.repository.status(worktree)?.trim().is_empty();
            let parsed = sv
                .files
                .read_to_string(receipt)
                .map_err(anyhow::Error::from)
                .and_then(|text| Ok(Receipt::parse(&text)?));
            return Ok(Some(match parsed {
                // A session that gives the change up is validation's to fail.
                Ok(receipt) if receipt.result() == ReceiptResult::Failed => {
                    ReviseOutcome::Rewritten(head)
                }
                Ok(receipt) if receipt.names_commit(head.as_str()) && clean => {
                    ReviseOutcome::Rewritten(head)
                }
                Ok(receipt) if !receipt.names_commit(head.as_str()) => ReviseOutcome::Mismatch(
                    ReasonCode::CommitMismatch,
                    format!(
                        "the rewritten receipt names commit {} but the worktree HEAD is {head}",
                        receipt.commit()
                    ),
                ),
                Ok(_) => ReviseOutcome::Mismatch(
                    ReasonCode::WorktreeDirty,
                    format!("the worktree has uncommitted changes on top of HEAD {head}"),
                ),
                Err(error) => ReviseOutcome::Mismatch(
                    ReasonCode::ReceiptInvalid,
                    format!("the rewritten receipt is invalid: {error:#}"),
                ),
            }));
        }
        if !rewritten && idle.is_some_and(|idle| idle.idle_since(input_at)) {
            return Ok(Some(ReviseOutcome::Ended(
                "went idle without rewriting the receipt".to_owned(),
            )));
        }
        if self.sent.elapsed() >= sv.cmux.resume_timeout() {
            return Ok(Some(ReviseOutcome::Ended(format!(
                "did not rewrite the receipt within {} seconds",
                sv.cmux.resume_timeout().as_secs()
            ))));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::super::file_time::at_ns;
    use super::*;

    /// Task 1197: an adopted request's `sent_at` is read back to the
    /// millisecond: a receipt written before it, in the same millisecond or
    /// earlier in its second, is not rewritten; one a millisecond later is.
    /// A request recorded in whole seconds reads as that second.
    #[test]
    fn a_receipt_of_the_adopted_requests_millisecond_is_not_rewritten() {
        use super::super::file_time::{request_sent_at, request_sent_at_of};
        let sent = at_ns(250, 600_000);
        let adopted = request_sent_at_of(&json!({"sent_at": request_sent_at(sent)}));
        for sent_at in [sent, adopted] {
            assert!(!rewritten_after(at_ns(250, 100_000), sent_at));
            assert!(!rewritten_after(at_ns(0, 0), sent_at));
            assert!(rewritten_after(at_ns(251, 0), sent_at));
        }
        let whole = request_sent_at_of(&json!({"sent_at": 1_000_000}));
        assert_eq!(whole, at_ns(0, 0));
        assert!(!rewritten_after(at_ns(0, 999_999), whole));
        assert!(rewritten_after(at_ns(1, 0), whole));
    }

    /// Task 1050: an idle marker of the millisecond of the revise's last
    /// input (an adopter's, from an event) does not end its turn.
    #[test]
    fn a_marker_of_the_inputs_millisecond_does_not_end_the_revise() {
        let path = Path::new("/run/idle.json");
        let input_at = at_ns(250, 0);
        let idle = |at| IdleMarker::written_at(path, at);
        assert!(!idle_ends_turn(&idle(at_ns(250, 700_000)), input_at, None));
        assert!(idle_ends_turn(&idle(at_ns(251, 0)), input_at, None));
        // An input taken after the marker still runs its turn.
        let input = InputMarker {
            modified: at_ns(252, 0),
            source: InputSource::Typed,
            text: None,
        };
        assert!(!idle_ends_turn(
            &idle(at_ns(251, 0)),
            input_at,
            Some(&input)
        ));
    }
}
