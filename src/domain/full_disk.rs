//! A worker's session that ended on a full disk (goal 260): a failure of
//! the host, not of the work, which the runtime resumes by itself once
//! there is room again instead of handing the run to its recovery job.
//!
//! A run is one when it ended `failed` or `interrupted` and the last turn
//! of its last session said so: the turn's message (what the agent's
//! result, its errors or its stderr ended on) carries the OS's mark of a
//! full disk, the same one a failed verification's `disk_full` reads
//! ([`verify_failure::out_of_space`]). Only a run whose recovery has not
//! begun since its last session is one: a round taken, a request pending
//! (its resumes used up) or a person's answer leaves it to the recovery
//! job as before, so the runtime never loops on its own resume.
//!
//! The resume waits while the free space holds the claims (the claims'
//! threshold of [`crate::domain::disk`]): it starts no session, no job and
//! no ask then, and the pass that finds room makes the run
//! `needs_session`, whose resume counts against the run's resumes as any
//! other and, once they are used up, goes the way of `resume_exhausted`.

use serde_json::json;

use super::{
    DomainError, EventKind, ReasonCode, RunEvent, RunStatus, TaskRun, TriageState,
    recovery::{self, RecoveryAlert, current_alert},
    resume::ResumeCount,
    run::{NewRunEvent, Recorded, resume_after_triage},
    triage_state, verify_failure,
};

/// The `repair` of the `auto_repaired` a resume after a full disk records
/// (`layer: runtime`).
pub const FULL_DISK_RESUME: &str = "full_disk_resume";

/// The `cause` its `triage_finished` and `auto_repaired` name: the class a
/// verification that failed on a full disk is given too.
pub const DISK_FULL: &str = "disk_full";

/// What a run that ended takes now.
/// `resume_exhausted`: the runtime's resume would be one past the run's
/// limit, so the run goes the way of a run whose resumes ran out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FullDiskStep {
    /// Not a session that ended on a full disk, or one whose recovery
    /// began: its recovery job's, as before.
    Recover,
    /// Ended on a full disk while the free space holds the claims: it
    /// rests, with no job and no ask, until a pass finds room.
    WaitForRoom,
    /// Ended on a full disk and there is room: resumed by the runtime.
    /// `evidence` is the line of the turn's message with the mark.
    Resume { evidence: String },
    /// Ended on a full disk with its resumes used up: its recovery job is
    /// requested with the alert `resume_exhausted`, room or not, as for a
    /// run whose last resume did not resolve it.
    Exhausted { evidence: String },
}

/// The line that shows that the last session of a run that ended in
/// `status` stopped on a full disk, read from its `events`: the message
/// of its last `turn_finished` since its latest `resume_started` (its
/// session's last turn), trimmed, when that turn failed. `None` for a run
/// that did not end `failed` or `interrupted`, a session that ended no
/// turn, a last turn that did not fail (its message is the agent's own
/// account, which may quote the mark), or a message without the mark.
pub fn ended_on_full_disk(status: RunStatus, events: &[RunEvent]) -> Option<String> {
    if !matches!(status, RunStatus::Failed | RunStatus::Interrupted) {
        return None;
    }
    let last = &events
        .iter()
        .rev()
        .take_while(|e| e.kind != EventKind::ResumeStarted)
        .find(|e| e.kind == EventKind::TurnFinished)?
        .payload;
    if last["outcome"] != "failed" {
        return None;
    }
    let message = last["message"].as_str()?;
    message
        .lines()
        .find(|line| verify_failure::out_of_space(line))
        .map(verify_failure::shorten)
}

/// What the run that ended in `status` with `events` takes, the claims
/// being held for the free space (`short_of_room`) or not, its resumes
/// used up (`resumes_exhausted`) or not.
pub fn full_disk_step(
    status: RunStatus,
    events: &[RunEvent],
    short_of_room: bool,
    resumes_exhausted: bool,
) -> FullDiskStep {
    let untaken = triage_state(events) == TriageState::Pending && current_alert(events).is_none();
    match ended_on_full_disk(status, events) {
        Some(evidence) if untaken && resumes_exhausted => FullDiskStep::Exhausted { evidence },
        Some(evidence) if untaken && !short_of_room => FullDiskStep::Resume { evidence },
        Some(_) if untaken => FullDiskStep::WaitForRoom,
        _ => FullDiskStep::Recover,
    }
}

/// The instruction the resumed session is given, and the run's
/// `last_error` until it resumes.
pub fn resume_instruction(evidence: &str) -> String {
    format!(
        "the session ended on a full disk ({evidence}); the runtime resumed the run once there was room again: go on with the task from where it stopped"
    )
}

/// The resume by the runtime of `run`, which ended on a full disk as
/// `evidence` shows: `needs_session` with [`resume_instruction`] as
/// `last_error`, recorded as `auto_repaired` (`layer: runtime`, `repair:`
/// [`FULL_DISK_RESUME`]) and `triage_finished` (`by: runtime`, action
/// `resume`, `cause:` [`DISK_FULL`]) with the instruction, which the
/// resumed session's request carries as a return from its recovery, so
/// that no round takes the run.
pub fn record_full_disk_resume(run: TaskRun, evidence: &str) -> Result<Recorded, DomainError> {
    let previous = run.status();
    let instruction = resume_instruction(evidence);
    let run = resume_after_triage(run, instruction.clone())?;
    let events = vec![
        NewRunEvent::new(
            EventKind::AutoRepaired,
            json!({
                "layer": "runtime",
                "repair": FULL_DISK_RESUME,
                "alert": previous.as_str(),
                "conditions": {"cause": DISK_FULL, "evidence": evidence, "room": true},
                "detail": "the worker's session ended on a full disk; the run is resumed now that the free space no longer holds the claims",
            }),
        ),
        NewRunEvent::new(
            EventKind::TriageFinished,
            json!({
                "by": "runtime",
                "action": "resume",
                "cause": DISK_FULL,
                "evidence": evidence,
                "instruction": instruction,
                "code": ReasonCode::TriageResume,
                "previous_status": previous.as_str(),
                "status": run.status().as_str(),
            }),
        ),
    ];
    Ok((run, events))
}

/// The `recovery_requested` of a run that ended on a full disk as
/// `evidence` shows with its `resumes` used up: alert `resume_exhausted`
/// (`by: runtime`, its next attempt, the last `resume_finished` of its
/// `events` as evidence, `cause:` [`DISK_FULL`] and the line), which the
/// run's next recovery round takes as the round of a run whose resumes
/// ran out; the run stays where it is.
pub fn exhausted_request(
    status: RunStatus,
    events: &[RunEvent],
    resumes: ResumeCount,
    evidence: &str,
) -> NewRunEvent {
    let resumed = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::ResumeFinished)
        .map(|e| e.id);
    NewRunEvent::new(
        EventKind::RecoveryRequested,
        json!({
            "alert": RecoveryAlert::ResumeExhausted,
            "attempt": recovery::attempts(events, RecoveryAlert::ResumeExhausted) + 1,
            "code": ReasonCode::ResumeExhausted,
            "by": "runtime",
            "status": status.as_str(),
            "evidence": resumed.into_iter().collect::<Vec<_>>(),
            "resumes": resumes.total(),
            "counted_resumes": resumes.counted,
            "cause": DISK_FULL,
            "line": evidence,
            "reason": format!("the session ended on a full disk ({evidence}) with its resumes used up"),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CommitSha, EventId, Provider, RunId, RunRecord, TaskId};
    use serde_json::Value;

    fn event(id: i64, kind: EventKind, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(3)),
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    fn turn(id: i64, message: &str) -> RunEvent {
        event(
            id,
            EventKind::TurnFinished,
            json!({"turn": id, "outcome": "failed", "message": message}),
        )
    }

    const FULL: &str =
        "error: failed to write target/debug/deps/x.rlib: No space left on device (os error 28)";

    fn run(status: RunStatus) -> TaskRun {
        TaskRun::restore(RunRecord {
            id: RunId::new("r1").unwrap(),
            task_id: TaskId::new(3),
            status,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            worker_mode: crate::domain::worker::WorkerMode::Interactive,
            base_commit: CommitSha::parse("0123456789abcdef0123456789abcdef01234567", "commit")
                .unwrap(),
            branch: None,
            worktree_path: None,
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: None,
            last_error: Some("session exited with code 1".into()),
            workspace_closed_at: None,
            created_at: "c".into(),
        })
        .unwrap()
    }

    /// Each of the errno's three marks, on any line of the last turn's
    /// message, makes a failed or interrupted run one that ended on a
    /// full disk, with that line as the evidence.
    #[test]
    fn each_mark_of_a_full_disk_in_the_last_turn_counts_for_a_failed_or_interrupted_run() {
        for line in [
            "cp: cannot create 'x': No space left on device",
            "write failed: ENOSPC",
            "Error: Os { code: 28 } (os error 28)",
        ] {
            let events = [
                turn(1, "done"),
                turn(2, &format!("tests ran\n  {line}  \nstopped")),
            ];
            for status in [RunStatus::Failed, RunStatus::Interrupted] {
                assert_eq!(
                    ended_on_full_disk(status, &events).as_deref(),
                    Some(line),
                    "{status:?}"
                );
            }
        }
    }

    /// A failure whose last turn says nothing of the disk, a session that
    /// ended no turn, and a run that did not end failed or interrupted (a
    /// session that ended well) are not one.
    #[test]
    fn other_failures_and_runs_that_did_not_fail_are_not_full_disks() {
        // A last turn that did not fail, quoting the mark in its account.
        let quoted = [event(
            1,
            EventKind::TurnFinished,
            json!({"turn": 1, "outcome": "succeeded", "message": FULL}),
        )];
        assert_eq!(ended_on_full_disk(RunStatus::Failed, &quoted), None);
        let other = [turn(1, "error: could not compile `dagq`")];
        assert_eq!(ended_on_full_disk(RunStatus::Failed, &other), None);
        assert_eq!(ended_on_full_disk(RunStatus::Failed, &[]), None);
        let without_message = [event(
            1,
            EventKind::TurnFinished,
            json!({"turn": 1, "outcome": "failed"}),
        )];
        assert_eq!(
            ended_on_full_disk(RunStatus::Failed, &without_message),
            None
        );
        let full = [turn(1, FULL)];
        for status in [
            RunStatus::Validating,
            RunStatus::AwaitingIntegration,
            RunStatus::NeedsSession,
            RunStatus::Running,
            RunStatus::Integrated,
        ] {
            assert_eq!(ended_on_full_disk(status, &full), None, "{status:?}");
        }
    }

    /// Only the last turn of the last session is read: a full disk an
    /// earlier session met, or an earlier turn the session got past, is
    /// not why it ended.
    #[test]
    fn only_the_last_turn_of_the_last_session_is_read() {
        let past_turn = [turn(1, FULL), turn(2, "error: could not compile")];
        assert_eq!(ended_on_full_disk(RunStatus::Failed, &past_turn), None);
        let past_session = [turn(1, FULL), event(2, EventKind::ResumeStarted, json!({}))];
        assert_eq!(ended_on_full_disk(RunStatus::Failed, &past_session), None);
        let this_session = [
            turn(1, "done"),
            event(2, EventKind::ResumeStarted, json!({})),
            turn(3, FULL),
        ];
        assert!(ended_on_full_disk(RunStatus::Failed, &this_session).is_some());
    }

    /// A run that ended on a full disk resumes when there is room and
    /// rests while the free space holds the claims; any other goes to its
    /// recovery job whatever the room.
    #[test]
    fn a_full_disk_resumes_with_room_and_rests_without() {
        let full = [turn(1, FULL)];
        let evidence = verify_failure::shorten(FULL);
        assert_eq!(
            full_disk_step(RunStatus::Failed, &full, false, false),
            FullDiskStep::Resume { evidence }
        );
        assert_eq!(
            full_disk_step(RunStatus::Interrupted, &full, true, false),
            FullDiskStep::WaitForRoom
        );
        let other = [turn(1, "error: could not compile")];
        for short in [false, true] {
            assert_eq!(
                full_disk_step(RunStatus::Failed, &other, short, false),
                FullDiskStep::Recover
            );
        }
    }

    /// A run that ended on a full disk with its resumes used up goes the
    /// way of `resume_exhausted`, room or not, and the request it records
    /// hands it to its recovery job (no second request after it).
    #[test]
    fn a_full_disk_with_its_resumes_used_up_requests_the_recovery_job() {
        let full = [
            turn(1, FULL),
            event(2, EventKind::ResumeFinished, json!({})),
        ];
        let full = [
            event(0, EventKind::ResumeStarted, json!({})),
            full[1].clone(),
            full[0].clone(),
        ];
        let evidence = verify_failure::shorten(FULL);
        for short in [false, true] {
            assert_eq!(
                full_disk_step(RunStatus::Failed, &full, short, true),
                FullDiskStep::Exhausted {
                    evidence: evidence.clone()
                }
            );
        }
        let request =
            exhausted_request(RunStatus::Failed, &full, ResumeCount::default(), &evidence);
        assert_eq!(request.kind, EventKind::RecoveryRequested);
        assert_eq!(request.payload["alert"], "resume_exhausted");
        assert_eq!(request.payload["attempt"], 1);
        assert_eq!(request.payload["cause"], DISK_FULL);
        assert_eq!(request.payload["evidence"], json!([2]));
        let mut after = full.to_vec();
        after.push(event(9, request.kind, request.payload));
        assert_eq!(current_alert(&after), Some(RecoveryAlert::ResumeExhausted));
        assert_eq!(triage_state(&after), TriageState::Pending);
        assert_eq!(
            full_disk_step(RunStatus::Failed, &after, false, true),
            FullDiskStep::Recover
        );
    }

    /// A run whose recovery began since its last session (its resumes used
    /// up and requested of the job, a round finished or failed, a person's
    /// answer back to the job) is the recovery job's: the runtime does not
    /// resume it again.
    #[test]
    fn a_run_whose_recovery_began_is_the_recovery_jobs() {
        let taken = [
            (
                EventKind::RecoveryRequested,
                json!({"alert": "resume_exhausted"}),
            ),
            (EventKind::TriageFinished, json!({"action": "ask"})),
            (EventKind::TriageFailed, json!({})),
        ];
        for (kind, payload) in taken {
            let events = [turn(1, FULL), event(2, kind, payload)];
            for short in [false, true] {
                assert_eq!(
                    full_disk_step(RunStatus::Failed, &events, short, true),
                    FullDiskStep::Recover,
                    "{kind:?}"
                );
            }
        }
    }

    /// The resume makes the run `needs_session` with the instruction as
    /// its last error, and records the repair and the `triage_finished`
    /// whose payload says the disk was full and shows the line.
    #[test]
    fn the_resume_records_that_the_disk_was_full() {
        let (resumed, events) = record_full_disk_resume(run(RunStatus::Failed), "ENOSPC").unwrap();
        assert_eq!(resumed.status(), RunStatus::NeedsSession);
        assert_eq!(
            resumed.last_error(),
            Some(resume_instruction("ENOSPC").as_str())
        );
        assert_eq!(
            events.iter().map(|e| e.kind).collect::<Vec<_>>(),
            [EventKind::AutoRepaired, EventKind::TriageFinished]
        );
        let repaired = &events[0].payload;
        assert_eq!(repaired["layer"], "runtime");
        assert_eq!(repaired["repair"], FULL_DISK_RESUME);
        assert_eq!(repaired["alert"], "failed");
        assert_eq!(repaired["conditions"]["cause"], DISK_FULL);
        assert_eq!(repaired["conditions"]["evidence"], "ENOSPC");
        let finished = &events[1].payload;
        assert_eq!(finished["by"], "runtime");
        assert_eq!(finished["action"], "resume");
        assert_eq!(finished["cause"], DISK_FULL);
        assert_eq!(finished["evidence"], "ENOSPC");
        assert_eq!(finished["code"], "triage_resume");
        assert_eq!(finished["previous_status"], "failed");
        assert_eq!(finished["status"], "needs_session");
        assert!(
            finished["instruction"]
                .as_str()
                .unwrap()
                .contains("ended on a full disk (ENOSPC)")
        );
        // The round is over for the triage: none takes the run.
        let mut after = vec![turn(1, FULL)];
        after.extend(
            events
                .iter()
                .enumerate()
                .map(|(at, e)| event(2 + i64::try_from(at).unwrap(), e.kind, e.payload.clone())),
        );
        assert_eq!(triage_state(&after), TriageState::Finished);
        assert!(record_full_disk_resume(run(RunStatus::Validating), "x").is_err());
    }
}
