//! The run transitions that come with the events they record (ADR-0032's
//! domain events): each returns the changed run and the `run_events` rows,
//! kind and payload, that the store writes in the same transaction. Which
//! event a transition records, and what its payload says, is decided here;
//! the store only saves them.

use crate::domain::EventKind;
use serde_json::{Value, json};

use super::{
    TaskRun, decide_landing, exhaust_resumes, finish_session, park_after_recheck, park_live,
};
use crate::domain::{
    DomainError, Reason, ReasonCode, RunEvent, RunStatus, event_kind, recheck,
    recovery::{self, RecoveryAlert},
    resume::{self, Exhaustion, ResumeCount},
    validation::Validation,
};

/// A `run_events` row a transition records for its run.
#[derive(Debug, Clone, PartialEq)]
pub struct NewRunEvent {
    pub kind: EventKind,
    pub payload: Value,
}

impl NewRunEvent {
    pub const fn new(kind: EventKind, payload: Value) -> Self {
        Self { kind, payload }
    }
}

/// A run after a transition, with the events it records.
pub type Recorded = (TaskRun, Vec<NewRunEvent>);

/// Put the run's new `status` and `reason` into a caller's `payload`.
fn with_status_and_reason(mut payload: Value, run: &TaskRun, reason: &str) -> Value {
    payload["status"] = json!(run.status());
    payload["reason"] = json!(reason);
    payload
}

/// [`finish_session`], recorded as `supervision_finished`: with the exit
/// code and, for a non-zero one, its reason code; for a session that went
/// idle after its receipt (`None`), `session_live: true` and no exit code.
pub fn end_session(run: TaskRun, exit_code: Option<i32>) -> Result<Recorded, DomainError> {
    let run = finish_session(run, exit_code)?;
    let payload = match exit_code {
        Some(code) => {
            let mut payload = json!({"status": run.status(), "exit_code": code});
            if code != 0 {
                Reason::of_exit_code(code).apply_to(&mut payload);
            }
            payload
        }
        None => json!({"status": run.status(), "exit_code": null, "session_live": true}),
    };
    Ok((
        run,
        vec![NewRunEvent::new(EventKind::SupervisionFinished, payload)],
    ))
}

/// Validation's verdict on the run: an accepted receipt makes it
/// `awaiting_integration` ([`super::accept`]); a rejection parks it
/// `needs_session` when only its evidence or its paths are wrong, and
/// fails it otherwise ([`super::reject`]). Recorded as
/// `validation_finished` with the run's status, and a park also as
/// `scope_violation` (which wins) or `evidence_missing`; those carry no
/// status, since `validation_finished` already reports the park and
/// `stats` counts it once.
pub fn finish_validation(run: TaskRun, validation: &Validation) -> Result<Recorded, DomainError> {
    let run = match (validation.accepted, validation.result_commit.clone()) {
        (true, Some(commit)) => super::accept(run, commit)?,
        (true, None) => {
            return Err(DomainError::InvalidCommit {
                field: "accepted result commit",
            });
        }
        (false, commit) => super::reject(
            run,
            commit,
            validation.reason.clone(),
            validation.resumable(),
        )?,
    };
    let status = run.status();
    // A validation is plain data with string keys, so it always serializes.
    let mut payload = serde_json::to_value(validation).expect("a validation serializes");
    payload["status"] = json!(status);
    let mut events = vec![NewRunEvent::new(EventKind::ValidationFinished, payload)];
    if status == RunStatus::NeedsSession && !validation.scope_violation.is_empty() {
        events.push(NewRunEvent::new(
            EventKind::ScopeViolation,
            json!({
                "code": ReasonCode::ScopeViolation,
                "paths": validation.scope_violation,
                "allowed": validation.allowed_paths,
                "reason": validation.reason,
            }),
        ));
    } else if status == RunStatus::NeedsSession {
        let mut payload = json!({
            "code": ReasonCode::EvidenceMissing,
            "checks": validation.evidence_missing,
            "reason": validation.reason,
        });
        // Why `e2e` was required (ADR-t963-1 decision 2), when it was.
        if let Some(e2e) = validation
            .e2e_requirement
            .as_ref()
            .filter(|e2e| e2e.required)
        {
            payload["e2e_requirement"] = json!(e2e);
        }
        events.push(NewRunEvent::new(EventKind::EvidenceMissing, payload));
    }
    Ok((run, events))
}

/// [`decide_landing`], recorded as `landing_decided` with the caller's
/// `payload`, the new status and the reason.
pub fn record_landing_decision(
    run: TaskRun,
    to: RunStatus,
    reason: &str,
    payload: Value,
) -> Result<Recorded, DomainError> {
    let run = decide_landing(run, to, reason.to_owned())?;
    let payload = with_status_and_reason(payload, &run, reason);
    Ok((
        run,
        vec![NewRunEvent::new(EventKind::LandingDecided, payload)],
    ))
}

/// [`park_live`], recorded as `recovery_parked` with the caller's
/// `payload`, the new status and the reason.
pub fn record_live_park(
    run: TaskRun,
    reason: &str,
    payload: Value,
) -> Result<Recorded, DomainError> {
    let run = park_live(run, reason.to_owned())?;
    let payload = with_status_and_reason(payload, &run, reason);
    Ok((
        run,
        vec![NewRunEvent::new(EventKind::RecoveryParked, payload)],
    ))
}

/// [`park_after_recheck`], recorded as `landing_recheck_failed` with the
/// caller's `payload`, `action: resumed`, the new status and the reason.
pub fn record_recheck_park(
    run: TaskRun,
    reason: &str,
    mut payload: Value,
) -> Result<Recorded, DomainError> {
    let run = park_after_recheck(run, reason.to_owned())?;
    payload["action"] = json!(recheck::RESUMED);
    let payload = with_status_and_reason(payload, &run, reason);
    Ok((
        run,
        vec![NewRunEvent::new(EventKind::LandingRecheckFailed, payload)],
    ))
}

/// A resumed session ended, recorded as `resume_finished` with the
/// caller's `payload` and the status after it: `to` when the resume moved
/// the run ([`super::finish_resume`]), `needs_session` when it did not. The
/// store adds the session's work and tokens it keeps.
pub fn resume_finished(to: Option<RunStatus>, mut payload: Value) -> NewRunEvent {
    payload["status"] = json!(to.unwrap_or(RunStatus::NeedsSession).as_str());
    NewRunEvent::new(EventKind::ResumeFinished, payload)
}

/// [`exhaust_resumes`] by the runtime, with the run's `events` and its
/// `resumes`: [`Exhaustion::Recover`] records `recovery_requested` (`alert:
/// resume_exhausted`, the next attempt, the last `resume_finished` as
/// evidence) for the recovery job; [`Exhaustion::Inherit`] records
/// `auto_repaired` (`repair: inherit_retry`) and `triage_finished` (action
/// `retry_inherit`) with the branch carried over.
pub fn record_exhausted_resumes(
    run: TaskRun,
    reason: &str,
    exhaustion: &Exhaustion,
    resumes: ResumeCount,
    events: &[RunEvent],
) -> Result<Recorded, DomainError> {
    let previous = run.status();
    let run = exhaust_resumes(run, reason.to_owned())?;
    let mut payload = json!({
        "code": ReasonCode::ResumeExhausted,
        "by": "runtime",
        "reason": reason,
        "resumes": resumes.total(),
        "counted_resumes": resumes.counted,
        "conflict_only_resumes": resumes.conflict_only,
        "kill_only_resumes": resumes.kill_only,
        "conflict_requests": resumes.conflict_requests,
        "previous_status": previous.as_str(),
        "status": run.status().as_str(),
    });
    let recorded = match exhaustion {
        Exhaustion::Recover => {
            let resumed = events
                .iter()
                .rev()
                .find(|e| e.kind == event_kind::RESUME_FINISHED)
                .map(|e| e.id);
            payload["alert"] = json!(RecoveryAlert::ResumeExhausted);
            payload["attempt"] =
                json!(recovery::attempts(events, RecoveryAlert::ResumeExhausted) + 1);
            payload["evidence"] = json!(resumed.into_iter().collect::<Vec<_>>());
            vec![NewRunEvent::new(EventKind::RecoveryRequested, payload)]
        }
        Exhaustion::Inherit { branch, head } => {
            payload["verdict"] = json!(resume::RETRY_INHERIT);
            payload["action"] = json!(resume::RETRY_INHERIT);
            payload["inherit"] = json!({"branch": branch, "head": head});
            vec![
                NewRunEvent::new(
                    EventKind::AutoRepaired,
                    json!({
                        "layer": "runtime",
                        "repair": "inherit_retry",
                        "conditions": {
                            "review": "pass",
                            "parked": ReasonCode::RebaseConflict,
                            "counted_resumes": resumes.counted,
                            "conflict_only_resumes": resumes.conflict_only,
                            "conflict_requests": resumes.conflict_requests,
                            "branch": branch,
                            "head": head,
                        },
                        "detail": "the resumes were used up on conflicts with main; the task is ready again for a run that carries this run's branch over",
                    }),
                ),
                NewRunEvent::new(EventKind::TriageFinished, payload),
            ]
        }
    };
    Ok((run, recorded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CommitSha, EventId, EvidenceCheck, Provider, RunId, RunRecord, TaskId, measure::LoadSummary,
    };

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn sha() -> CommitSha {
        CommitSha::parse(SHA, "commit").unwrap()
    }

    fn run(status: RunStatus) -> TaskRun {
        TaskRun::restore(RunRecord {
            id: RunId::new("r1").unwrap(),
            task_id: TaskId::new(3),
            status,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            worker_mode: crate::domain::worker::WorkerMode::Interactive,
            base_commit: sha(),
            branch: None,
            worktree_path: None,
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: None,
            last_error: None,
            workspace_closed_at: None,
            created_at: "c".into(),
        })
        .unwrap()
    }

    fn kinds(events: &[NewRunEvent]) -> Vec<&'static str> {
        events.iter().map(|e| e.kind.as_str()).collect()
    }

    fn validation(
        accepted: bool,
        evidence_missing: Vec<EvidenceCheck>,
        scope_violation: Vec<String>,
    ) -> Validation {
        Validation {
            accepted,
            result_commit: Some(sha()),
            reason: (!accepted).then(|| "why".to_owned()),
            code: None,
            receipt: Value::Null,
            evidence_missing,
            allowed_paths: if scope_violation.is_empty() {
                Vec::new()
            } else {
                vec!["src/**".into()]
            },
            scope_violation,
            e2e_requirement: None,
            load: LoadSummary::default(),
        }
    }

    #[test]
    fn a_session_end_records_its_exit() {
        let (run_, events) = end_session(run(RunStatus::Running), Some(0)).unwrap();
        assert_eq!(run_.status(), RunStatus::Validating);
        assert_eq!(kinds(&events), [event_kind::SUPERVISION_FINISHED]);
        assert_eq!(
            events[0].payload,
            json!({"status": "validating", "exit_code": 0})
        );

        let (run_, events) = end_session(run(RunStatus::Running), Some(137)).unwrap();
        assert_eq!(run_.status(), RunStatus::Failed);
        assert_eq!(events[0].payload["code"], "session_killed");
        assert_eq!(events[0].payload["signal"], 9);

        let (_, events) = end_session(run(RunStatus::Running), None).unwrap();
        assert_eq!(
            events[0].payload,
            json!({"status": "validating", "exit_code": null, "session_live": true})
        );
        assert!(end_session(run(RunStatus::Failed), None).is_err());
    }

    #[test]
    fn an_accepted_validation_records_only_its_finish() {
        let (run_, events) = finish_validation(
            run(RunStatus::Validating),
            &validation(true, vec![], vec![]),
        )
        .unwrap();
        assert_eq!(run_.status(), RunStatus::AwaitingIntegration);
        assert_eq!(kinds(&events), [event_kind::VALIDATION_FINISHED]);
        assert_eq!(events[0].payload["status"], "awaiting_integration");
        assert_eq!(events[0].payload["accepted"], true);
    }

    #[test]
    fn an_accepted_validation_needs_its_commit() {
        let mut accepted = validation(true, vec![], vec![]);
        accepted.result_commit = None;
        assert!(matches!(
            finish_validation(run(RunStatus::Validating), &accepted),
            Err(DomainError::InvalidCommit { .. })
        ));
    }

    #[test]
    fn a_failed_validation_records_no_park() {
        let (run_, events) = finish_validation(
            run(RunStatus::Validating),
            &validation(false, vec![], vec![]),
        )
        .unwrap();
        assert_eq!(run_.status(), RunStatus::Failed);
        assert_eq!(run_.last_error(), Some("why"));
        assert_eq!(kinds(&events), [event_kind::VALIDATION_FINISHED]);
        assert_eq!(events[0].payload["status"], "failed");
    }

    #[test]
    fn a_park_for_evidence_records_evidence_missing() {
        let (run_, events) = finish_validation(
            run(RunStatus::Validating),
            &validation(false, vec![EvidenceCheck::E2e], vec![]),
        )
        .unwrap();
        assert_eq!(run_.status(), RunStatus::NeedsSession);
        assert_eq!(
            kinds(&events),
            [
                event_kind::VALIDATION_FINISHED,
                event_kind::EVIDENCE_MISSING
            ]
        );
        assert_eq!(
            events[1].payload,
            json!({"code": "evidence_missing", "checks": ["e2e"], "reason": "why"})
        );
    }

    #[test]
    fn a_park_for_scope_records_scope_violation_even_with_evidence_missing() {
        let (_, events) = finish_validation(
            run(RunStatus::Validating),
            &validation(false, vec![EvidenceCheck::E2e], vec!["docs/x.md".into()]),
        )
        .unwrap();
        assert_eq!(
            kinds(&events),
            [event_kind::VALIDATION_FINISHED, event_kind::SCOPE_VIOLATION]
        );
        assert_eq!(
            events[1].payload,
            json!({"code": "scope_violation", "paths": ["docs/x.md"], "allowed": ["src/**"], "reason": "why"})
        );
    }

    #[test]
    fn parks_and_landing_decisions_add_the_status_and_reason() {
        let (run_, events) = record_landing_decision(
            run(RunStatus::AwaitingIntegration),
            RunStatus::Failed,
            "canceled",
            json!({"ask_id": 4}),
        )
        .unwrap();
        assert_eq!(run_.status(), RunStatus::Failed);
        assert_eq!(kinds(&events), [event_kind::LANDING_DECIDED]);
        assert_eq!(
            events[0].payload,
            json!({"ask_id": 4, "status": "failed", "reason": "canceled"})
        );

        let (_, events) =
            record_live_park(run(RunStatus::Running), "resume", json!({"job": 1})).unwrap();
        assert_eq!(kinds(&events), [event_kind::RECOVERY_PARKED]);
        assert_eq!(
            events[0].payload,
            json!({"job": 1, "status": "needs_session", "reason": "resume"})
        );

        let (_, events) = record_recheck_park(
            run(RunStatus::AwaitingIntegration),
            "conflict",
            json!({"main": "m"}),
        )
        .unwrap();
        assert_eq!(kinds(&events), [recheck::LANDING_RECHECK_FAILED]);
        assert_eq!(
            events[0].payload,
            json!({"main": "m", "action": "resumed", "status": "needs_session", "reason": "conflict"})
        );
        assert!(record_recheck_park(run(RunStatus::Running), "x", json!({})).is_err());
    }

    #[test]
    fn a_finished_resume_reports_the_status_after_it() {
        let event = resume_finished(None, json!({"head": "h"}));
        assert_eq!(event.kind, EventKind::ResumeFinished);
        assert_eq!(
            event.payload,
            json!({"head": "h", "status": "needs_session"})
        );
        let event = resume_finished(Some(RunStatus::Failed), json!({}));
        assert_eq!(event.payload["status"], "failed");
    }

    fn finished_resume(id: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: event_kind::RESUME_FINISHED.to_owned(),
            payload: json!({}),
            created_at: String::new(),
            actor: None,
        }
    }

    #[test]
    fn exhausted_resumes_go_to_the_recovery_job() {
        let resumes = ResumeCount {
            counted: 3,
            conflict_only: 1,
            ..Default::default()
        };
        let events = [finished_resume(7), finished_resume(9)];
        let (run_, recorded) = record_exhausted_resumes(
            run(RunStatus::NeedsSession),
            "used up",
            &Exhaustion::Recover,
            resumes,
            &events,
        )
        .unwrap();
        assert_eq!(run_.status(), RunStatus::Failed);
        assert_eq!(run_.last_error(), Some("used up"));
        assert_eq!(kinds(&recorded), [event_kind::RECOVERY_REQUESTED]);
        let payload = &recorded[0].payload;
        assert_eq!(payload["code"], "resume_exhausted");
        assert_eq!(payload["alert"], "resume_exhausted");
        assert_eq!(payload["attempt"], 1);
        assert_eq!(payload["evidence"], json!([9]));
        assert_eq!(payload["resumes"], 4);
        assert_eq!(payload["previous_status"], "needs_session");
        assert_eq!(payload["status"], "failed");
    }

    #[test]
    fn exhausted_resumes_on_conflicts_retry_with_the_branch() {
        let resumes = ResumeCount {
            counted: 0,
            conflict_only: 5,
            kill_only: 0,
            conflict_requests: 0,
            parked_for_conflict: true,
        };
        let (_, recorded) = record_exhausted_resumes(
            run(RunStatus::NeedsSession),
            "conflicts",
            &Exhaustion::Inherit {
                branch: Some("dagq/r1".into()),
                head: sha(),
            },
            resumes,
            &[],
        )
        .unwrap();
        assert_eq!(
            kinds(&recorded),
            [event_kind::AUTO_REPAIRED, event_kind::TRIAGE_FINISHED]
        );
        assert_eq!(recorded[0].payload["repair"], "inherit_retry");
        assert_eq!(recorded[0].payload["conditions"]["branch"], "dagq/r1");
        assert_eq!(recorded[1].payload["action"], resume::RETRY_INHERIT);
        assert_eq!(
            recorded[1].payload["inherit"],
            json!({"branch": "dagq/r1", "head": SHA})
        );
        assert!(
            record_exhausted_resumes(
                run(RunStatus::Failed),
                "x",
                &Exhaustion::Recover,
                resumes,
                &[]
            )
            .is_err()
        );
    }
}
