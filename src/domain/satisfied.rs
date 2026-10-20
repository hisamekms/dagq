//! A worker's claim that the base already met its task's acceptance
//! (ADR-t1818-1): the main head the runtime verifies the claim on (the
//! snapshot), what that verification found, the person's answer, and
//! whether an answer to close applies now or the verification runs again
//! on a newer head. Everything is read from the run's events, so a
//! supervisor started later compares the same snapshot.

use crate::domain::EventKind;
use crate::domain::run::{AskOpened, restore_payload};
use serde_json::{Value, json};

use super::{
    AskId, AskKind, CommitSha, EventId, ReasonCode, RunEvent, RunStatus, cancel_reason::NewCancel,
};

/// The answer that closes the task as `completed` without a commit.
pub const CLOSE: &str = "close";
/// The answer that fails the run and puts the task back to `ready`.
pub const RETRY: &str = "retry";
/// The answer that fails the run and cancels the task.
pub const CANCEL: &str = "cancel";

/// The options of the `approve_close` ask of a claim verified on its
/// snapshot.
pub const CLOSE_OPTIONS: [&str; 3] = [CLOSE, RETRY, CANCEL];

/// The options of the `approve_close` ask of a claim whose verification
/// failed: it is not closed.
pub const FAILED_OPTIONS: [&str; 2] = [RETRY, CANCEL];

/// An answer to an `approve_close` ask the supervisor applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SatisfiedAnswer {
    Close,
    Retry,
    Cancel,
}

impl SatisfiedAnswer {
    /// One of the options, whatever the case and the spaces around it;
    /// any other answer is the inbox's.
    pub fn parse(answer: &str) -> Option<Self> {
        match answer.trim().to_ascii_lowercase().as_str() {
            CLOSE => Some(Self::Close),
            RETRY => Some(Self::Retry),
            CANCEL => Some(Self::Cancel),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Close => CLOSE,
            Self::Retry => RETRY,
            Self::Cancel => CANCEL,
        }
    }

    /// What this answer to ask `ask` does to the run and its task when it
    /// is not `close` (which [`close_step`] decides against main): `retry`
    /// fails the run and puts the task back to `ready`, `cancel` fails the
    /// run and cancels the task.
    pub fn decision(self, ask: AskId) -> Option<Decision> {
        match self {
            Self::Close => None,
            Self::Retry => Some(Decision {
                task: TaskMove::Ready,
                code: ReasonCode::Other,
                reason: format!("put back to ready by ask {ask}"),
                cause: "retried",
            }),
            Self::Cancel => Some(Decision {
                task: TaskMove::Cancel(NewCancel::answered(AskKind::ApproveClose, ask)),
                code: ReasonCode::Cancelled,
                reason: format!("canceled by ask {ask}"),
                cause: "canceled",
            }),
        }
    }
}

/// A `retry` or `cancel` answer applied: the run fails with `reason` (its
/// `last_error`) and `code`, recorded as `run_satisfied_decided`, its task
/// moves as `task` says in the same transaction, and the run's phase rests
/// with `cause`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub task: TaskMove,
    pub code: ReasonCode,
    pub reason: String,
    pub cause: &'static str,
}

/// How a decided answer moves the run's task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskMove {
    /// Back to `ready`.
    Ready,
    /// Canceled, recording the answer as its reason (ADR-t947-4 decision 3).
    Cancel(NewCancel),
}

/// The options of the `approve_close` ask after `verification`: a claim
/// that failed its verification is not offered `close`.
pub fn options(verification: &SnapshotVerification) -> &'static [&'static str] {
    if verification.passed() {
        &CLOSE_OPTIONS
    } else {
        &FAILED_OPTIONS
    }
}

/// One verification of the claim, as `run_satisfied_verified` records it:
/// the snapshot it ran on, whether the task's verification commands
/// passed there, and the answer to close it was run again for (none for
/// the first one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub head: CommitSha,
    pub passed: bool,
    pub ask_id: Option<AskId>,
    pub event: EventId,
}

/// The latest verification among a run's `events`; `None` before one
/// finished.
pub fn latest_verified(events: &[RunEvent]) -> Option<Verified> {
    events
        .iter()
        .rev()
        .filter(|e| e.kind == EventKind::RunSatisfiedVerified)
        .find_map(|e| {
            Some(Verified {
                head: CommitSha::parse(e.payload["head"].as_str()?, "snapshot head").ok()?,
                passed: e.payload["passed"].as_bool()?,
                ask_id: e.payload["ask_id"].as_i64().map(AskId::new),
                event: e.id,
            })
        })
}

/// Whether `event` is a step of a claim's course and, if so, whether the
/// claim [`rests`] after it: its acceptance (`validation_finished` with
/// `already_satisfied`, or a recovery round's hand-off of a failed run,
/// [`handed_off`]), a verification's end and the end of the review's
/// judgment start a rest, and the snapshot a verification starts on and
/// the start of a judgment end it. The KPI's slot occupancy leaves the
/// rests out by these steps, as the supervisor's count does.
pub fn rests_after(event: &RunEvent) -> Option<bool> {
    match event.kind.known() {
        Some(EventKind::RunSatisfiedVerified | EventKind::RunSatisfiedJudged) => Some(true),
        Some(EventKind::RunSatisfiedSnapshot | EventKind::RunSatisfiedReviewStarted) => Some(false),
        Some(EventKind::ValidationFinished) if event.payload["already_satisfied"] == true => {
            Some(true)
        }
        _ if handed_off(event) => Some(true),
        _ => None,
    }
}

/// Whether `event` is a recovery round's hand-off of a failed run to the
/// claim's course (`triage_finished` with action `hand_off`, ADR-t1818-2
/// decision 3): the run is `validating` from then on, accepted as a claim
/// whose verification has not started.
pub fn handed_off(event: &RunEvent) -> bool {
    event.kind == EventKind::TriageFinished
        && event.payload["action"] == super::recovery::HAND_OFF_KIND
        && event.payload["status"] == RunStatus::Validating.as_str()
}

/// How many judgments of the claim the review began among a run's
/// `events` (`run_satisfied_review_started`): the next one's attempt is one
/// more.
pub fn judgments(events: &[RunEvent]) -> usize {
    events
        .iter()
        .filter(|e| e.kind == EventKind::RunSatisfiedReviewStarted)
        .count()
}

/// The latest step of a claim's course among a run's `events` ([`rests_after`]).
fn last_step(events: &[RunEvent]) -> Option<&RunEvent> {
    events.iter().rev().find(|e| rests_after(e).is_some())
}

/// Whether a `validating` run with these `events` rests between the steps
/// of its claim, no verification running: accepted and waiting for its
/// verification to start (while the container its verification is set to
/// cannot be used), or verified (and judged by the review) and waiting for
/// the answer to its `approve_close` ask. Nobody leases it and no process of it lives, yet it
/// is no orphan: the supervisor or the answer moves it, so neither the
/// supervisor's recovery nor a person's `recover` takes it.
pub fn rests(events: &[RunEvent]) -> bool {
    last_step(events).and_then(rests_after) == Some(true)
}

/// Whether the run waits for a person's answer by its `events`: an
/// `approve_close` ask of it opened (`ask_opened` of that kind) and neither
/// answered nor closed since. An answered close that waits for a slot or a
/// container waits for no person.
pub fn answer_awaited(events: &[RunEvent]) -> bool {
    let mut open: Vec<i64> = Vec::new();
    for event in events {
        match event.kind.known() {
            Some(EventKind::AskOpened) => {
                let opened = restore_payload::<AskOpened>(&event.payload);
                if opened.kind == Some("approve_close") {
                    open.extend(opened.ask_id);
                }
            }
            Some(EventKind::AskAnswered | EventKind::AskClosed) => {
                open.retain(|id| Some(*id) != event.payload["ask_id"].as_i64());
            }
            _ => {}
        }
    }
    !open.is_empty()
}

/// Whether a recovery round handed the run off ([`handed_off`]) since its
/// latest resume: its claim is the round's, and validating it again (an
/// adopter's) accepts it as a claim without judging its receipt.
pub fn handed_off_since_resume(events: &[RunEvent]) -> bool {
    events
        .iter()
        .rev()
        .take_while(|e| e.kind != EventKind::ResumeStarted)
        .any(handed_off)
}

/// Whether the claim was accepted (or a failed run handed off to its
/// course) and its verification has not started since: the supervisor starts it once it may (a free slot, the
/// verification's runtime usable), unless an `approve_close` ask of the run
/// is still open.
pub fn awaits_verification(events: &[RunEvent]) -> bool {
    last_step(events).is_some_and(|e| e.kind == EventKind::ValidationFinished || handed_off(e))
}

/// Whether a run in `status`, `leased` or not, with these `events` is
/// kept from recovery (the supervisor's and a person's `recover`, and
/// `doctor`'s `recoverable`): an unleased `validating` run that
/// [`rests`]. A leased one is its supervisor's, whose death the usual
/// recovery (adoption) handles, and a running verification has a lease.
pub fn blocks_recovery(status: RunStatus, leased: bool, events: &[RunEvent]) -> bool {
    status == RunStatus::Validating && !leased && rests(events)
}

/// What an answer to close does now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseStep {
    /// Close on this verification.
    Close(Verified),
    /// Run the verification again on main as it is now.
    Verify,
}

/// What the answer to close of ask `ask` does with main at `main`, given
/// the latest verification `latest` (ADR-t1818-1 decision 3): it closes on
/// a verification that passed on that very head, or on the one run again
/// for this answer, even if main moved while it ran (it is not run a
/// second time). Otherwise, main having moved since the snapshot (before
/// the answer, or while no supervisor ran) or no verification having
/// passed, the verification runs again on `main`; the person's answer
/// stands and is not asked again.
pub fn close_step(latest: Option<&Verified>, ask: AskId, main: &CommitSha) -> CloseStep {
    match latest {
        Some(verified)
            if verified.passed && (verified.head == *main || verified.ask_id == Some(ask)) =>
        {
            CloseStep::Close(verified.clone())
        }
        _ => CloseStep::Verify,
    }
}

/// How the verification of a snapshot ended: the commands passed, or the
/// first one that failed with its exit, log and the end of its output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotVerification {
    pub head: CommitSha,
    /// How many of the task's verification commands ran.
    pub commands: usize,
    pub failure: Option<SnapshotFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotFailure {
    pub command: String,
    pub exit_code: i32,
    pub log_path: String,
    pub output_tail: String,
}

impl SnapshotVerification {
    pub fn passed(&self) -> bool {
        self.failure.is_none()
    }

    /// The payload of `run_satisfied_verified`, for the answer `ask` when
    /// it was run again for one.
    pub fn payload(&self, ask: Option<AskId>) -> Value {
        let mut payload = json!({
            "head": self.head,
            "passed": self.passed(),
            "commands": self.commands,
        });
        if let Some(ask) = ask {
            payload["ask_id"] = json!(ask);
        }
        if let Some(failure) = &self.failure {
            payload["command"] = json!(failure.command);
            payload["exit_code"] = json!(failure.exit_code);
            payload["log_path"] = json!(failure.log_path);
            payload["output_tail"] = json!(failure.output_tail);
        }
        payload
    }
}

/// The payload of `run_satisfied_snapshot`: the head the verification
/// runs on, and the answer it runs again for.
pub fn snapshot_payload(head: &CommitSha, ask: Option<AskId>) -> Value {
    let mut payload = json!({"head": head});
    if let Some(ask) = ask {
        payload["ask_id"] = json!(ask);
    }
    payload
}

/// The payload of `run_satisfied_closed` for a person's answer
/// (`closed_by` `answer`): the head the task was verified on, the
/// verification and the answer it was closed on. `rechecked` says the
/// verification ran again for the answer after main moved.
pub fn closed_payload(verified: &Verified, ask: AskId) -> Value {
    json!({
        "head": verified.head,
        "verified_event": verified.event,
        "closed_by": "answer",
        "ask_id": ask,
        "answer": CLOSE,
        "rechecked": verified.ask_id == Some(ask),
    })
}

/// The question of the `approve_close` ask after the verification of
/// `verification`: the claim, the snapshot it was verified on, and what
/// each option does. A failed verification offers no close.
pub fn question(
    task: &str,
    summary: &str,
    verification: &SnapshotVerification,
    after: Option<AskId>,
) -> String {
    let claim = format!(
        "The worker of task {task} reported already_satisfied: the base already met the acceptance, and it made no commit. Its summary: {}",
        if summary.trim().is_empty() {
            "(none)"
        } else {
            summary.trim()
        }
    );
    match &verification.failure {
        None => format!(
            "{claim}\nThe task's {} verification commands passed on main {} (the snapshot). Answer close to complete the task without a commit (its dependents go on; if main moved by then, the commands run once more on the new head and the task closes only if they pass), retry to fail the run and put the task back to ready, or cancel to cancel the task.",
            verification.commands, verification.head
        ),
        Some(failure) => format!(
            "{claim}\nThe task's verification {}failed on main {} (the snapshot): {:?} exited with {}; see {}. The task is not closed. Answer retry to fail the run and put the task back to ready, or cancel to cancel the task.",
            after.map_or(String::new(), |ask| format!(
                "run again for the close answer of ask {ask} "
            )),
            verification.head,
            failure.command,
            failure.exit_code,
            failure.log_path
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::event_kind;

    const S1: &str = "0000000000000000000000000000000000000001";
    const S2: &str = "0000000000000000000000000000000000000002";

    fn sha(text: &str) -> CommitSha {
        CommitSha::parse(text, "commit").unwrap()
    }

    fn verified(head: &str, passed: bool, ask: Option<i64>) -> Verified {
        Verified {
            head: sha(head),
            passed,
            ask_id: ask.map(AskId::new),
            event: EventId::new(7),
        }
    }

    /// The snapshot against the head at the answer (ADR-t1818-1 decision
    /// 3): the same head closes; a moved head, or none verified, runs the
    /// verification again; the one run again for this answer closes even
    /// if main moved once more, but not for another answer or if it
    /// failed.
    #[test]
    fn an_answer_to_close_closes_on_the_snapshot_or_verifies_the_new_head() {
        let ask = AskId::new(3);
        let first = verified(S1, true, None);
        assert_eq!(
            close_step(Some(&first), ask, &sha(S1)),
            CloseStep::Close(first.clone())
        );
        assert_eq!(close_step(Some(&first), ask, &sha(S2)), CloseStep::Verify);
        assert_eq!(close_step(None, ask, &sha(S1)), CloseStep::Verify);
        let again = verified(S2, true, Some(3));
        assert_eq!(
            close_step(Some(&again), ask, &sha(S1)),
            CloseStep::Close(again.clone())
        );
        assert_eq!(
            close_step(Some(&again), AskId::new(4), &sha(S1)),
            CloseStep::Verify
        );
        let failed = verified(S1, false, Some(3));
        assert_eq!(close_step(Some(&failed), ask, &sha(S1)), CloseStep::Verify);
    }

    #[test]
    fn the_latest_verification_is_read_back_from_the_events() {
        let event = |id: i64, payload: Value| RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: event_kind::RUN_SATISFIED_VERIFIED.into(),
            payload,
            created_at: String::new(),
            actor: None,
        };
        let first = SnapshotVerification {
            head: sha(S1),
            commands: 2,
            failure: None,
        };
        let again = SnapshotVerification {
            head: sha(S2),
            commands: 2,
            failure: Some(SnapshotFailure {
                command: "cargo test".into(),
                exit_code: 101,
                log_path: "/runs/r/satisfied-1.log".into(),
                output_tail: "failed".into(),
            }),
        };
        let events = [
            event(1, first.payload(None)),
            event(2, again.payload(Some(AskId::new(5)))),
        ];
        assert_eq!(latest_verified(&[]), None);
        assert!(!rests(&[]));
        assert!(rests(&events));
        assert!(!awaits_verification(&events));
        let mut again = events.to_vec();
        again.push(RunEvent {
            kind: event_kind::RUN_SATISFIED_SNAPSHOT.into(),
            ..event(3, json!({"head": S2}))
        });
        assert!(!rests(&again));
        // Accepted, before any snapshot: it rests and awaits its
        // verification; an acceptance that is no claim is no step.
        let accepted = |claim: bool| RunEvent {
            kind: event_kind::VALIDATION_FINISHED.into(),
            ..event(4, json!({"accepted": true, "already_satisfied": claim}))
        };
        assert!(rests(&[accepted(true)]) && awaits_verification(&[accepted(true)]));
        assert!(!rests(&[accepted(false)]) && !awaits_verification(&[accepted(false)]));
        // An adopter's validation again after the ask still rests.
        again.push(accepted(true));
        assert!(rests(&again) && awaits_verification(&again));
        assert_eq!(
            latest_verified(&events[..1]),
            Some(Verified {
                head: sha(S1),
                passed: true,
                ask_id: None,
                event: EventId::new(1)
            })
        );
        assert_eq!(
            latest_verified(&events),
            Some(Verified {
                head: sha(S2),
                passed: false,
                ask_id: Some(AskId::new(5)),
                event: EventId::new(2)
            })
        );
        assert_eq!(events[1].payload["exit_code"], 101);
        let closed = closed_payload(&verified(S2, true, Some(5)), AskId::new(5));
        assert_eq!(closed["rechecked"], true);
        assert_eq!(closed["head"], S2);
        assert_eq!(closed["closed_by"], "answer");
        assert_eq!(snapshot_payload(&sha(S1), None), json!({"head": S1}));
    }

    #[test]
    fn a_retry_or_a_cancel_fails_the_run_and_moves_the_task_and_close_is_not_theirs() {
        let ask = AskId::new(8);
        assert_eq!(SatisfiedAnswer::Close.decision(ask), None);
        assert_eq!(
            SatisfiedAnswer::Retry.decision(ask),
            Some(Decision {
                task: TaskMove::Ready,
                code: ReasonCode::Other,
                reason: "put back to ready by ask 8".into(),
                cause: "retried",
            })
        );
        assert_eq!(
            SatisfiedAnswer::Cancel.decision(ask),
            Some(Decision {
                task: TaskMove::Cancel(NewCancel::answered(AskKind::ApproveClose, ask)),
                code: ReasonCode::Cancelled,
                reason: "canceled by ask 8".into(),
                cause: "canceled",
            })
        );
    }

    /// Only an unleased `validating` run resting on its ask is kept from
    /// recovery: not a leased one, not one of another status, and not one
    /// whose verification runs (or that is no claim).
    #[test]
    fn only_an_unleased_claim_at_rest_is_kept_from_recovery() {
        let event = |id: i64, kind: &str| RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload: json!({"head": S1, "passed": true}),
            created_at: String::new(),
            actor: None,
        };
        let resting = [
            event(1, event_kind::RUN_SATISFIED_SNAPSHOT),
            event(2, event_kind::RUN_SATISFIED_VERIFIED),
        ];
        assert!(blocks_recovery(RunStatus::Validating, false, &resting));
        assert!(!blocks_recovery(RunStatus::Validating, true, &resting));
        assert!(!blocks_recovery(RunStatus::Running, false, &resting));
        let again = [
            event(1, event_kind::RUN_SATISFIED_SNAPSHOT),
            event(2, event_kind::RUN_SATISFIED_VERIFIED),
            event(3, event_kind::RUN_SATISFIED_SNAPSHOT),
        ];
        assert!(!blocks_recovery(RunStatus::Validating, false, &again));
        assert!(!blocks_recovery(
            RunStatus::Validating,
            false,
            &resting[..1]
        ));
        assert!(!blocks_recovery(RunStatus::Validating, false, &[]));
        // Accepted and waiting for its verification to start (a container
        // holds it): kept from recovery too.
        let accepted = RunEvent {
            payload: json!({"already_satisfied": true}),
            ..event(1, event_kind::VALIDATION_FINISHED)
        };
        assert!(blocks_recovery(RunStatus::Validating, false, &[accepted]));
        // A failed run a recovery round handed off (ADR-t1818-2 decision
        // 3) rests the same way until its verification starts; a round
        // whose hand-off a hold kept (the run stays failed) does not.
        let handed = RunEvent {
            payload: json!({"action": "hand_off", "status": "validating"}),
            ..event(1, event_kind::TRIAGE_FINISHED)
        };
        assert!(handed_off(&handed));
        assert!(awaits_verification(std::slice::from_ref(&handed)));
        assert!(blocks_recovery(
            RunStatus::Validating,
            false,
            std::slice::from_ref(&handed)
        ));
        let kept = RunEvent {
            payload: json!({"action": "held", "kept_action": "hand_off", "status": "failed"}),
            ..event(1, event_kind::TRIAGE_FINISHED)
        };
        assert!(!handed_off(&kept) && !awaits_verification(&[kept]));
        let started = [handed, event(2, event_kind::RUN_SATISFIED_SNAPSHOT)];
        assert!(!awaits_verification(&started));
        assert!(handed_off_since_resume(&started));
        let resumed = [started[0].clone(), event(3, event_kind::RESUME_STARTED)];
        assert!(!handed_off_since_resume(&resumed));
    }

    /// The review's judgment after a passed verification ends the rest the
    /// verification began, and its end begins one again; the judgments are
    /// counted by their starts.
    #[test]
    fn a_claim_under_judgment_does_not_rest_and_rests_once_judged() {
        let event = |id: i64, kind: &str| RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload: json!({"head": S1, "passed": true, "attempt": 1}),
            created_at: String::new(),
            actor: None,
        };
        let judging = [
            event(1, event_kind::RUN_SATISFIED_SNAPSHOT),
            event(2, event_kind::RUN_SATISFIED_VERIFIED),
            event(3, event_kind::RUN_SATISFIED_REVIEW_STARTED),
            event(4, event_kind::RUN_SATISFIED_REVIEW_JOB_STARTED),
            event(5, event_kind::RUN_SATISFIED_REVIEW_JOB_FINISHED),
        ];
        assert!(!rests(&judging));
        assert!(!blocks_recovery(RunStatus::Validating, false, &judging));
        assert_eq!(judgments(&judging), 1);
        let judged = [
            judging.to_vec(),
            vec![event(6, event_kind::RUN_SATISFIED_JUDGED)],
        ]
        .concat();
        assert!(rests(&judged));
        assert!(blocks_recovery(RunStatus::Validating, false, &judged));
        assert_eq!(judgments(&judged), 1);
        assert_eq!(judgments(&judging[..2]), 0);
    }

    #[test]
    fn a_failed_verification_is_not_offered_close() {
        let passed = SnapshotVerification {
            head: sha(S1),
            commands: 2,
            failure: None,
        };
        assert_eq!(options(&passed), ["close", "retry", "cancel"]);
        let failed = SnapshotVerification {
            failure: Some(SnapshotFailure {
                command: "test ! -f broken.txt".into(),
                exit_code: 1,
                log_path: "/l".into(),
                output_tail: String::new(),
            }),
            ..passed
        };
        assert_eq!(options(&failed), ["retry", "cancel"]);
    }

    #[test]
    fn the_answers_and_the_questions_name_the_snapshot() {
        assert_eq!(
            SatisfiedAnswer::parse(" Close "),
            Some(SatisfiedAnswer::Close)
        );
        assert_eq!(
            SatisfiedAnswer::parse("retry"),
            Some(SatisfiedAnswer::Retry)
        );
        assert_eq!(
            SatisfiedAnswer::parse("cancel"),
            Some(SatisfiedAnswer::Cancel)
        );
        assert_eq!(SatisfiedAnswer::parse("land"), None);
        let passed = SnapshotVerification {
            head: sha(S1),
            commands: 4,
            failure: None,
        };
        let text = question("12", "nothing to do", &passed, None);
        assert!(text.contains(S1) && text.contains("Answer close"), "{text}");
        let failed = SnapshotVerification {
            head: sha(S2),
            commands: 4,
            failure: Some(SnapshotFailure {
                command: "cargo test".into(),
                exit_code: 1,
                log_path: "/l".into(),
                output_tail: String::new(),
            }),
        };
        let text = question("12", "", &failed, Some(AskId::new(9)));
        assert!(
            text.contains(S2) && text.contains("not closed") && text.contains("ask 9"),
            "{text}"
        );
        assert!(!text.contains("Answer close"), "{text}");
    }
}
