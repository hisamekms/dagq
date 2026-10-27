//! A `revise` verdict or a conflict sent to the live session
//! ([`ReviseWatch`], ADR-0027 decision 2).

use super::*;
use crate::domain::AskConfidence;

/// A `revise` verdict, or a conflict the precheck found, sent to the live
/// session: it is waited for until the session rewrites its receipt and
/// goes idle.
pub(super) struct ReviseWatch {
    pub(super) session: SessionRef,
    pub(super) attempt: usize,
    pub(super) fix: Fix,
    /// A receipt or idle marker no newer than this predates the request.
    pub(super) sent_at: SystemTime,
    /// When the request, or the last input that restarted its resume
    /// timeout, was sent, on the injected monotonic clock.
    pub(super) sent: Instant,
    /// Whether the session took the request, or the last answer typed
    /// (task 285).
    pub(super) start: Option<SystemTime>,
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
    /// The oldest of its own `worker_question`s whose answer the session
    /// waits for in its slot, as the last poll or the adoption found it:
    /// the run is in the phase `waiting` meanwhile, not `revise`. Another
    /// question than the one before is an answer and the next question
    /// ([`super::stages::question_moves`]).
    pub(super) awaits_answer: Option<AskId>,
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

/// The reasons review `attempt` sends the run back with
/// ([`ReviewVerdict::sent_back_findings`]): what decides, once the run's
/// revises are used up, whether it lands or a person is asked
/// (ADR-t2599-1).
pub(super) struct SentBack {
    pub(super) attempt: usize,
    pub(super) findings: Vec<crate::domain::review_reason::Finding>,
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
/// requested (`input_at`, in a later millisecond, task 1050).
fn idle_ends_turn(idle: &IdleMarker, input_at: SystemTime) -> bool {
    super::file_time::written_after(idle.modified(), input_at)
}

/// Whether a receipt written at `modified` was rewritten after the request
/// sent at `sent_at`, compared to the millisecond (task 1197): an adopter
/// reads `sent_at` back to the millisecond it was recorded with, and a
/// receipt of that millisecond, or of an earlier one in its second, is the
/// one from before the request.
pub(super) fn rewritten_after(modified: SystemTime, sent_at: SystemTime) -> bool {
    super::file_time::written_after(modified, sent_at)
}

/// What a revise that has neither a rewritten receipt nor an idle after
/// its last input does at `now`: it waits until the resume `timeout` since
/// `sent` (the request, or the last input that restarted the timeout), and
/// ends at it.
pub(super) fn revise_deadline(
    now: Instant,
    sent: Instant,
    timeout: Duration,
) -> Option<ReviseOutcome> {
    passed(now, sent, timeout).then(|| {
        ReviseOutcome::Ended(format!(
            "did not rewrite the receipt within {} seconds",
            timeout.as_secs()
        ))
    })
}

/// The first second of the `worker_question`s a revise or conflict
/// request sent at `sent_at` follows ([`SessionWatch::asks_from`]): a
/// question from before it (asked during validation or the review, or left
/// open before the receipt) is the inbox's to deliver by hand, neither
/// typed here nor holding the revise (task 582).
pub(super) fn revise_asks_from(sent_at: SystemTime) -> i64 {
    unix_seconds(sent_at)
}

/// The first second of the `worker_question`s that hold a revise: those it
/// follows from the request (`asks_from`) and not passed by a receipt
/// rewritten after them (`questions_from`, task 583).
pub(super) fn questions_held_from(asks_from: i64, questions_from: i64) -> i64 {
    asks_from.max(questions_from)
}

/// What the `worker_question`s open since [`questions_held_from`] do to a
/// revise at one poll.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum QuestionHold {
    /// The session stopped at its own question waits for the answer,
    /// however long a person takes: it neither went idle without
    /// rewriting the receipt nor ran out of time.
    Waits,
    /// The session rewrote its receipt since the request after its
    /// questions: the ones created before this second hold it no longer,
    /// and its time runs from now.
    PassedFrom(i64),
    /// A receipt rewritten after them was seen before: they hold no longer.
    Passed,
}

/// What open `worker_question`s do to a revise whose receipt was last
/// written at `receipt_modified` (`None` when it cannot be read), the
/// request sent at `sent_at` and the questions passed up to
/// `questions_from`: a receipt rewritten since the request is judged all
/// the same, as a worker's own session's is (task 583).
pub(super) fn open_question_hold(
    receipt_modified: Option<SystemTime>,
    sent_at: SystemTime,
    questions_from: i64,
) -> QuestionHold {
    let Some(modified) = receipt_modified.filter(|m| rewritten_after(*m, sent_at)) else {
        return QuestionHold::Waits;
    };
    let from = unix_seconds(modified) + 1;
    if from > questions_from {
        QuestionHold::PassedFrom(from)
    } else {
        QuestionHold::Passed
    }
}

/// What a revise held by no question does once it read its turn
/// ([`turn_outcome`]).
pub(super) enum TurnOutcome {
    /// The rewritten receipt is judged ([`judge_receipt`]).
    JudgeReceipt,
    /// The revise ends.
    Ends(ReviseOutcome),
    /// Nothing ended the turn: its resume timeout decides
    /// ([`revise_deadline`]). The only outcome the clock is read for.
    CheckDeadline,
}

/// A receipt `rewritten` since the request with an idle after it
/// (`idle_after_receipt`) is judged; no rewritten receipt and an idle since
/// the last input (`idle_since_input`) ends the revise; otherwise its
/// deadline is checked.
pub(super) fn turn_outcome(
    rewritten: bool,
    idle_after_receipt: bool,
    idle_since_input: bool,
) -> TurnOutcome {
    if rewritten && idle_after_receipt {
        return TurnOutcome::JudgeReceipt;
    }
    if !rewritten && idle_since_input {
        return TurnOutcome::Ends(ReviseOutcome::Ended(
            "went idle without rewriting the receipt".to_owned(),
        ));
    }
    TurnOutcome::CheckDeadline
}

/// What a rewritten receipt (`parsed`, or why it cannot be read) says about
/// a revise whose worktree HEAD is `head`, `clean` or not: the head, or why
/// validation would fail the run and its work and the session is asked to
/// fix it.
pub(super) fn judge_receipt(
    parsed: Result<Receipt>,
    head: CommitSha,
    clean: bool,
) -> ReviseOutcome {
    match parsed {
        // A session that gives the change up is validation's to fail.
        Ok(receipt) if receipt.result() == ReceiptResult::Failed => ReviseOutcome::Rewritten(head),
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
    }
}

/// The question a revise waits for in its slot: the oldest that holds it
/// (`held`, [`ReviseWatch::held_question`]) while its receipt, last
/// written at `receipt_modified`, was not rewritten since the request sent
/// at `sent_at`; a rewritten receipt passed its questions
/// ([`open_question_hold`]).
pub(super) fn awaited_question(
    held: Option<AskId>,
    receipt_modified: Option<SystemTime>,
    sent_at: SystemTime,
    questions_from: i64,
) -> Option<AskId> {
    held.filter(|_| {
        open_question_hold(receipt_modified, sent_at, questions_from) == QuestionHold::Waits
    })
}

impl ReviseWatch {
    /// `now` is the supervisor's monotonic clock: the resume timeout runs
    /// from it.
    pub(super) fn new(
        run: &TaskRun,
        session: SessionRef,
        attempt: usize,
        fix: Fix,
        sent_at: SystemTime,
        start: Option<SystemTime>,
        now: Instant,
    ) -> Result<Self> {
        let mut live = Box::new(SessionWatch::fixing(run, &session.workspace, sent_at, now)?);
        live.asks_from = revise_asks_from(sent_at);
        Ok(ReviseWatch {
            session,
            attempt,
            fix,
            sent_at,
            sent: now,
            start,
            live,
            questions_from: 0,
            delivered_closed: None,
            awaits_answer: None,
        })
    }

    /// Another request typed at `sent_at` (a receipt to fix): only what the
    /// session does after it counts. It does not restart the resume
    /// timeout (`sent`), which still runs from the request or the last
    /// input that restarted it.
    pub(super) fn requested(&mut self, sent_at: SystemTime, start: SystemTime) {
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

            self.live.recovery.stop(sv, run);
            // A `stalled` ask of a send it did not take ends with it.
            match outcome {
                Some(ReviseOutcome::Rewritten(_)) => self.live.stall.settle(sv, run, true)?,
                _ => self.live.stall.ended(sv, run)?,
            }
        }
        Ok(outcome)
    }

    /// The oldest unclosed `worker_question` of `run` that holds the
    /// revise ([`Self::holds_questions_from`]).
    pub(super) fn held_question(
        &self,
        sv: &Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<Option<AskId>> {
        sv.oldest_unclosed_question(run, self.holds_questions_from())
    }

    /// The question an adopted revise of `run` waits for in its slot, as
    /// its poll would judge it ([`awaited_question`]).
    pub(super) fn adopted_question(
        &self,
        sv: &Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<Option<AskId>> {
        let held = self.held_question(sv, run)?;
        let modified = run
            .receipt_path()
            .and_then(|receipt| sv.files.modified(Path::new(receipt)).ok());
        Ok(awaited_question(
            held,
            modified,
            self.sent_at,
            self.questions_from,
        ))
    }

    /// The first second of the `worker_question`s that hold the revise.
    pub(super) fn holds_questions_from(&self) -> i64 {
        questions_held_from(self.live.asks_from, self.questions_from)
    }

    /// Whether the session rewrote `run`'s receipt since the request.
    pub(super) fn receipt_rewritten(&self, sv: &Supervisor<'_>, run: &TaskRun) -> bool {
        run.receipt_path()
            .is_some_and(|receipt| self.rewritten(sv, Path::new(receipt)))
    }

    /// Whether the session rewrote `receipt` since the request.
    fn rewritten(&self, sv: &Supervisor<'_>, receipt: &Path) -> bool {
        sv.files
            .modified(receipt)
            .is_ok_and(|modified| rewritten_after(modified, self.sent_at))
    }

    /// What the rewritten `receipt` of `run` says about the revise: the
    /// head it names, or why it does not do.
    fn judge_rewritten(
        &self,
        sv: &Supervisor<'_>,
        run: &TaskRun,
        receipt: &Path,
    ) -> Result<ReviseOutcome> {
        let crate::application::run_git::SessionHead { head, clean, .. } =
            crate::application::run_git::session_head(&*sv.repository, run)?;
        let parsed = sv
            .files
            .read_to_string(receipt)
            .map_err(anyhow::Error::from)
            .and_then(|text| Ok(Receipt::parse(&text)?));
        Ok(judge_receipt(parsed, head, clean))
    }

    fn observe(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<Option<ReviseOutcome>> {
        self.awaits_answer = None;
        let processes = sv.queue.processes(run.id())?;
        let Some(wrapper) = processes
            .iter()
            .find(|p| p.role == "wrapper" && p.exited_at.is_none())
        else {
            // A session that ended once it rewrote the receipt (a wrapper
            // that ended between turns at a hold's fence, ADR-t1879-1) is
            // judged by that receipt.
            let receipt = Path::new(run.receipt_path().context("missing receipt path")?);
            if self.rewritten(sv, receipt) {
                return self.judge_rewritten(sv, run, receipt).map(Some);
            }
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
            self.sent = sv.generators.clock.monotonic();
            self.start = self.live.answer_start.take();
        }
        if let Some(typed) = self.live.deliver_answers(sv, run)? {
            self.live.input_at = Some(typed);
            self.sent = sv.generators.clock.monotonic();
            self.delivered_closed = sv.queue.last_worker_question_closed(run.id())?;
            self.start = self.live.answer_start.take();
        }

        // A request or an answer it did not take goes to its recovery job
        // (ADR-0047 decision 31); an instruction the job typed is input.
        // A session stopped at its own question, asked since the request,
        // waits for its answer, however long a person takes: it neither went
        // idle without rewriting the receipt nor ran out of time. A receipt
        // it rewrote since the request is judged all the same, as a
        // worker's own session is (task 583): the session went on past the
        // questions it asked before the receipt, which hold it no longer
        // (nor after a request to fix the receipt), and its time runs from
        // there.
        let receipt = Path::new(run.receipt_path().context("missing receipt path")?);
        // The idle marker is read before the questions: a session asks
        // before it stops, so a stop this read sees has its question seen
        // below, never as idle without one, however fast the turn after an
        // answer typed above asked and stopped.
        let idle_marker = run.idle_marker_path()?;
        let idle = sv.session_idle(&idle_marker)?;
        if sv
            .queue
            .has_unclosed_worker_question_since(run.id(), self.holds_questions_from())?
        {
            let modified = sv.files.modified(receipt).ok();
            match open_question_hold(modified, self.sent_at, self.questions_from) {
                QuestionHold::Waits => {
                    self.awaits_answer = self.held_question(sv, run)?;
                    return Ok(None);
                }
                QuestionHold::PassedFrom(from) => {
                    self.questions_from = from;
                    self.sent = sv.generators.clock.monotonic();
                }
                QuestionHold::Passed => (),
            }
        }
        // A turn at its provider's wall (ADR-t813-2): the call went to the
        // other provider, or the run waits in the hold ask.
        match self.live.provider_wall(sv, run)? {
            WallGate::Held => return Ok(None),
            WallGate::Moved(_) => {
                self.sent = sv.generators.clock.monotonic();
                self.start = None;
            }
            WallGate::Open => (),
        }
        // A turn that waits for the dagq machine's memory has not started:
        // the request's clock starts once it does (ADR-t3163-1 decision 4).
        if super::recovery::waits_for_memory(sv, run)? {
            self.sent = sv.generators.clock.monotonic();
            return Ok(None);
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
            self.sent = sv.generators.clock.monotonic();
        }
        // The idle marker was read above, before the receipt: a receipt
        // rewritten after that read is judged at the next poll, never as idle
        // without it. A marker from before the last request is not this turn's.
        let input_at = self.live.input_at.unwrap_or(self.sent_at);
        // Only an idle marker newer than the queued request ends the revise.
        let idle = idle.filter(|idle| idle_ends_turn(idle, input_at));
        let rewritten = self.rewritten(sv, receipt);
        let idle_after_receipt = match &idle {
            Some(idle) if rewritten => idle.idle_after_receipt(&*sv.files, receipt)?.is_some(),
            _ => false,
        };
        let idle_since_input = idle.is_some_and(|idle| idle.idle_since(input_at));
        match turn_outcome(rewritten, idle_after_receipt, idle_since_input) {
            TurnOutcome::JudgeReceipt => self.judge_rewritten(sv, run, receipt).map(Some),
            TurnOutcome::Ends(outcome) => Ok(Some(outcome)),
            TurnOutcome::CheckDeadline => Ok(revise_deadline(
                sv.generators.clock.monotonic(),
                self.sent,
                sv.sessions.resume_timeout(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::file_time::at_ns;
    use super::*;

    /// An adopted revise waits in its slot for its question only while its
    /// receipt was not rewritten since the request: a receipt rewritten
    /// after the request passed the question, as its poll judges it.
    #[test]
    fn an_adopted_revise_waits_for_its_question_only_before_a_rewritten_receipt() {
        let sent_at = at(1_000, 500);
        let one = Some(AskId::new(1));
        assert_eq!(awaited_question(one, None, sent_at, 0), one);
        assert_eq!(awaited_question(one, Some(at(900, 0)), sent_at, 0), one);
        assert_eq!(awaited_question(one, Some(at(1_000, 100)), sent_at, 0), one);
        assert_eq!(awaited_question(one, Some(at(1_200, 0)), sent_at, 0), None);
        assert_eq!(
            awaited_question(one, Some(at(1_200, 0)), sent_at, 5_000),
            None
        );
        assert_eq!(awaited_question(None, None, sent_at, 0), None);
    }

    /// A revise ends at its resume timeout since its last send, not a
    /// millisecond before; an input that restarted the timeout (a later
    /// `sent`) gives it the whole timeout again. The moment is given as a
    /// value: only offsets from the origin matter.
    #[test]
    fn a_revise_ends_at_its_resume_timeout_since_the_last_send() {
        let ms = Duration::from_millis(1);
        let timeout = Duration::from_secs(1800);
        let sent = Instant::now();
        assert!(revise_deadline(sent, sent, timeout).is_none());
        assert!(revise_deadline(sent + timeout - ms, sent, timeout).is_none());
        let Some(ReviseOutcome::Ended(why)) = revise_deadline(sent + timeout, sent, timeout) else {
            panic!("a revise at its timeout ends");
        };
        assert_eq!(why, "did not rewrite the receipt within 1800 seconds");
        let restarted = sent + timeout - ms;
        assert!(revise_deadline(sent + timeout, restarted, timeout).is_none());
        assert!(revise_deadline(restarted + timeout, restarted, timeout).is_some());
    }

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

    /// The time `secs` and `ms` after the epoch: asks are created to the
    /// second, files written to the millisecond.
    fn at(secs: u64, ms: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(ms)
    }

    fn sha(c: char) -> CommitSha {
        CommitSha::parse(c.to_string().repeat(40), "commit").unwrap()
    }

    fn receipt(result: &str, commit: &CommitSha) -> Receipt {
        let check = |status: &str| json!({"status": status, "evidence_or_reason": "why"});
        Receipt::parse(
            &json!({
                "run_id": "r",
                "result": result,
                "commit": commit,
                "tests": check("passed"),
                "e2e": check("not_applicable"),
                "subagent_review": check("not_applicable"),
                "summary": "fixed",
            })
            .to_string(),
        )
        .unwrap()
    }

    /// Whether a revise sent at `sent_at` follows a `worker_question`
    /// created at `created_at`, with the questions passed up to
    /// `questions_from`: its answer typed, and it holding the revise.
    fn follows(created_at: i64, sent_at: SystemTime, questions_from: i64) -> bool {
        let held = questions_held_from(revise_asks_from(sent_at), questions_from);
        super::super::session::follows_ask(created_at, held)
    }

    /// Task 582: a question created in an earlier second than the revise
    /// request (asked during the review) is the inbox's: the revise neither
    /// types its answer nor waits for it, open or answered, so an idle
    /// session or the resume timeout ends the revise as without it. One
    /// asked in the request's second or later is the revise's. A run
    /// awaiting integration takes the answer of the same ones only
    /// (`domain::tests::answers_are_the_sessions_while_it_runs_or_fixes_a_request`).
    #[test]
    fn a_question_from_before_the_revise_neither_gets_its_answer_typed_nor_holds_it() {
        let sent_at = at(100, 400);
        assert!(!follows(99, sent_at, 0));
        assert!(follows(100, sent_at, 0));
        assert!(follows(101, sent_at, 0));
        // Idle since the last input without a rewritten receipt, it ends
        // with no deadline to check (no clock read); otherwise the resume
        // timeout decides (`a_revise_ends_at_its_resume_timeout_since_the_last_send`).
        let TurnOutcome::Ends(ReviseOutcome::Ended(why)) = turn_outcome(false, false, true) else {
            panic!("an idle session without a rewritten receipt ends");
        };
        assert_eq!(why, "went idle without rewriting the receipt");
        assert!(matches!(
            turn_outcome(false, false, false),
            TurnOutcome::CheckDeadline
        ));
    }

    /// Task 238: a session with a question it asked since the request open,
    /// its receipt not rewritten since the request (or not readable), waits
    /// for the answer: nothing else is judged, past the resume timeout too.
    /// A receipt rewritten since then passes the questions created before
    /// the second after it, and its time runs from there; seen again, it
    /// passes nothing new (task 583).
    #[test]
    fn an_open_question_holds_the_revise_until_a_receipt_rewritten_after_the_request() {
        let sent_at = at(100, 400);
        assert_eq!(open_question_hold(None, sent_at, 0), QuestionHold::Waits);
        let before = at(100, 300);
        assert_eq!(
            open_question_hold(Some(before), sent_at, 0),
            QuestionHold::Waits
        );
        let rewritten = at(102, 200);
        assert_eq!(
            open_question_hold(Some(rewritten), sent_at, 0),
            QuestionHold::PassedFrom(103)
        );
        assert_eq!(
            open_question_hold(Some(rewritten), sent_at, 103),
            QuestionHold::Passed
        );
        // A question asked after the rewritten receipt holds the revise
        // again until a later one.
        assert!(follows(103, sent_at, 103));
        assert!(!follows(102, sent_at, 103));
    }

    /// Task 583: a receipt rewritten since the request with an idle after
    /// it is judged whatever questions it went on past; it names the clean
    /// HEAD, or reports `failed`, or is a mismatch the session is asked to
    /// fix: another commit, uncommitted changes, an unreadable receipt.
    #[test]
    fn a_receipt_rewritten_past_its_questions_is_judged_by_the_head_it_names() {
        // Judged with no deadline to check (no clock read), however long
        // the revise took.
        for idle_since_input in [false, true] {
            assert!(matches!(
                turn_outcome(true, true, idle_since_input),
                TurnOutcome::JudgeReceipt
            ));
        }
        // Rewritten without an idle after it: only the deadline can end it.
        assert!(matches!(
            turn_outcome(true, false, true),
            TurnOutcome::CheckDeadline
        ));
        let (head, other) = (sha('b'), sha('a'));
        let judged = |parsed, clean| judge_receipt(parsed, head.clone(), clean);
        assert!(matches!(
            judged(Ok(receipt("succeeded", &head)), true),
            ReviseOutcome::Rewritten(h) if h == head
        ));
        assert!(matches!(
            judged(Ok(receipt("failed", &other)), false),
            ReviseOutcome::Rewritten(h) if h == head
        ));
        let ReviseOutcome::Mismatch(code, why) = judged(Ok(receipt("succeeded", &other)), true)
        else {
            panic!("a receipt for another commit is a mismatch");
        };
        assert_eq!(code, ReasonCode::CommitMismatch);
        assert_eq!(
            why,
            format!("the rewritten receipt names commit {other} but the worktree HEAD is {head}")
        );
        let ReviseOutcome::Mismatch(code, why) = judged(Ok(receipt("succeeded", &head)), false)
        else {
            panic!("a dirty worktree is a mismatch");
        };
        assert_eq!(code, ReasonCode::WorktreeDirty);
        assert_eq!(
            why,
            format!("the worktree has uncommitted changes on top of HEAD {head}")
        );
        let ReviseOutcome::Mismatch(code, why) = judged(Err(anyhow::anyhow!("not json")), true)
        else {
            panic!("an unreadable receipt is a mismatch");
        };
        assert_eq!(code, ReasonCode::ReceiptInvalid);
        assert_eq!(why, "the rewritten receipt is invalid: not json");
    }

    /// Task 582 and 583: a revise follows the questions from its request's
    /// second. An open question with no receipt rewritten since the request
    /// keeps it waiting (no clock read). A receipt rewritten after a
    /// question passes it: the questions before the second after it hold no
    /// more and the resume timeout restarts (the clock read). Seen again, it
    /// passes nothing new (no clock read). Once that receipt is sent back
    /// to be fixed (a request at a later `sent_at`, which
    /// [`ReviseWatch::requested`] records without touching the bounds), the
    /// question still open holds the fix no longer, while one asked after
    /// the rewritten receipt still does; the session idle without rewriting
    /// it again is judged so.
    #[test]
    fn a_question_passed_by_a_rewritten_receipt_does_not_hold_its_fix() {
        let sent_at = at(100, 400);
        let asks_from = revise_asks_from(sent_at);
        assert_eq!(asks_from, 100);
        assert_eq!(questions_held_from(asks_from, 0), 100);
        assert_eq!(open_question_hold(None, sent_at, 0), QuestionHold::Waits);

        let rewritten = at(102, 200);
        let QuestionHold::PassedFrom(questions_from) =
            open_question_hold(Some(rewritten), sent_at, 0)
        else {
            panic!("the rewritten receipt passes the question");
        };
        assert_eq!(questions_from, 103);
        assert_eq!(
            open_question_hold(Some(rewritten), sent_at, questions_from),
            QuestionHold::Passed
        );

        let fix_sent_at = at(104, 0);
        let held = questions_held_from(asks_from, questions_from);
        assert_eq!(held, 103);
        assert!(!super::super::session::follows_ask(101, held));
        assert!(super::super::session::follows_ask(103, held));
        // The old receipt is not rewritten since the fix request: only a
        // question still held would make the session wait for it.
        assert!(!rewritten_after(rewritten, fix_sent_at));
        assert_eq!(
            open_question_hold(Some(rewritten), fix_sent_at, questions_from),
            QuestionHold::Waits
        );
        assert!(matches!(
            turn_outcome(false, false, true),
            TurnOutcome::Ends(ReviseOutcome::Ended(why))
                if why == "went idle without rewriting the receipt"
        ));
    }

    /// Task 1050: an idle marker of the millisecond of the revise's last
    /// input (an adopter's, from an event) does not end its turn.
    #[test]
    fn a_marker_of_the_inputs_millisecond_does_not_end_the_revise() {
        let path = Path::new("/run/idle.json");
        let input_at = at_ns(250, 0);
        let idle = |at| IdleMarker::written_at(path, at);
        assert!(!idle_ends_turn(&idle(at_ns(250, 700_000)), input_at));
        assert!(idle_ends_turn(&idle(at_ns(251, 0)), input_at));
    }
}
