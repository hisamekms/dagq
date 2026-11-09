//! Control-side selection for a runner pull. Candidates arrive in claim order,
//! after dependency and hold checks. Additional credential/budget predicates
//! compose with this filter; a runner never supplies a task or run to select.
use super::{oidc::OidcIdentity, runner_registration::Runner};
use serde::{Deserialize, Serialize};

/// Registry and declaration restrictions apply before any caller's extra
/// predicate. Missing owners fail closed for private runners.
pub fn eligible(runner: &Runner, owner: Option<&OidcIdentity>) -> bool {
    runner.revoked_at.is_none()
        && runner.declaration.as_ref().is_some_and(|d| d.container)
        && (runner.shared || owner == Some(&runner.owner))
}

pub fn select<'a, T>(
    runner: &Runner,
    candidates: &'a [T],
    owner: impl Fn(&T) -> Option<&OidcIdentity>,
    additional: impl Fn(&T) -> bool,
) -> Option<&'a T> {
    candidates
        .iter()
        .find(|c| eligible(runner, owner(c)) && additional(c))
}

/// Limits read from the control side's accepted repository rules, not runner
/// hints. `None` means the corresponding repository limit is unset. Counting
/// running Spike assignments happens inside the claim transaction so two pulls
/// cannot both use its last slot. Investigation accounting uses the existing
/// claim's live queue facts and waiting-owner grace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClaimLimits {
    pub max_spikes: Option<usize>,
    pub investigations: super::investigation::InvestigationLimit,
}
impl ClaimLimits {
    pub fn holds(self, running_spikes: usize) -> super::claim_order::ClaimHolds {
        super::claim_order::ClaimHolds {
            classes: super::claim_order::ClassLimits {
                max_spikes: self.max_spikes,
                running_spikes,
            }
            .held(),
            investigations: self.investigations,
        }
    }
}

/// Trusted inputs to a pull, obtained through the service backend's main-head
/// and repository-rules ports. Neither can be supplied in the HTTP body.
pub struct ClaimInputs {
    pub base: super::CommitSha,
    pub limits: ClaimLimits,
}

/// POST runner_poll has an empty params object, never a run selector or
/// runner time. Empty replies are immediate; a caller may poll again later.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Poll {}

/// All three reports carry the fencing generation, never runner clock time.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseReport {
    pub run_id: String,
    pub generation: i64,
}

/// A turn's state file and receipt are data, not filesystem paths or commands.
/// The head must equal the validated state file's head. An optional exit code
/// uses the existing session-end transition; no requested run status is trusted.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultReport {
    pub run_id: String,
    pub generation: i64,
    pub head: String,
    pub state: super::run_clone::RunState,
    pub receipt: Option<serde_json::Value>,
    pub exit_code: Option<i32>,
}

/// The only time given to a runner is a duration in seconds, measured from
/// the control side's now. Every accepted heartbeat renews the 90-second lease.
#[derive(Debug, Serialize)]
pub struct Assignment {
    pub run: super::TaskRun,
    pub generation: i64,
    pub remaining_seconds: i64,
}
pub fn remaining_seconds(expires_at_ms: i64, now_ms: i64) -> i64 {
    expires_at_ms.saturating_sub(now_ms).max(0) / 1000
}

/// Ownership is checked before generation: another runner gets
/// authorization_denied, while a former generation of the same runner gets
/// lease_lost. No supplied id can establish ownership.
pub fn owns(runner_id: &str, lease: Option<&super::runner_lease::RunnerLease>) -> bool {
    use super::authorization::{Authorizer, Capability, Resource, StaticPolicy};
    let Some(lease) = lease else {
        return false;
    };
    let Ok(id) = super::RunId::new(lease.run_id.clone()) else {
        return false;
    };
    StaticPolicy
        .authorize(
            &super::ActorContext::instance(super::ActorRole::Runner, runner_id),
            Capability::RunnerReport,
            &Resource::RunnerRun {
                id,
                runner_id: Some(lease.runner_id.clone()),
            },
        )
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runner_registration::Declaration;
    fn identity(subject: &str) -> OidcIdentity {
        OidcIdentity {
            issuer: "issuer".into(),
            subject: subject.into(),
        }
    }
    fn runner(shared: bool) -> Runner {
        Runner {
            id: "a".into(),
            owner: identity("a"),
            created_at: 0,
            revoked_at: None,
            shared,
            approved_by: None,
            declaration: Some(Declaration {
                os: "linux".into(),
                arch: "aarch64".into(),
                container: true,
                credentials: vec![],
            }),
        }
    }
    #[test]
    fn sharing_and_owner_filter_claim_order_with_composable_restrictions() {
        let candidates = [identity("b"), identity("a")];
        assert_eq!(
            select(&runner(true), &candidates, |o| Some(o), |_| true),
            Some(&candidates[0])
        );
        assert_eq!(
            select(&runner(false), &candidates, |o| Some(o), |_| true),
            Some(&candidates[1])
        );
        assert!(select(&runner(false), &candidates[..1], |o| Some(o), |_| true).is_none());
        assert!(select(&runner(true), &candidates, |o| Some(o), |_| false).is_none());
        assert!(!eligible(&runner(false), None));
        let mut revoked = runner(true);
        revoked.revoked_at = Some(1);
        assert!(!eligible(&revoked, Some(&candidates[0])));
    }
    #[test]
    fn no_declaration_or_no_container_never_receives_even_own_work() {
        for shared in [false, true] {
            let mut r = runner(shared);
            r.declaration.as_mut().unwrap().container = false;
            assert!(!eligible(&r, Some(&r.owner)));
            r.declaration = None;
            assert!(!eligible(&r, Some(&r.owner)));
        }
    }
    #[test]
    fn configured_limits_hold_spikes_and_preserve_investigation_settings() {
        let limits = ClaimLimits {
            max_spikes: Some(1),
            investigations: super::super::investigation::InvestigationLimit {
                max: Some(2),
                grace_secs: 60,
            },
        };
        assert!(limits.holds(0).classes.is_empty());
        assert_eq!(
            limits.holds(1).classes,
            vec![super::super::ExecutionClass::Spike]
        );
        assert_eq!(limits.holds(2).investigations, limits.investigations);
        assert!(ClaimLimits::default().holds(100).classes.is_empty());
    }

    #[test]
    fn requests_exclude_poll_selectors_and_runner_clocks() {
        assert!(serde_json::from_str::<Poll>("{}").is_ok());
        for body in [
            r#"{"run_id":"r"}"#,
            r#"{"now_ms":1}"#,
            r#"{"max_spikes":null}"#,
        ] {
            assert!(serde_json::from_str::<Poll>(body).is_err());
        }
        assert!(
            serde_json::from_str::<LeaseReport>(r#"{"run_id":"r","generation":1,"now_ms":0}"#)
                .is_err()
        );
        assert_eq!(remaining_seconds(91_000, 1_000), 90);
        assert_eq!(remaining_seconds(1, 2), 0);
    }
}

/// One authenticated pull or report. Poll has no target. Report targets are
/// authorized against the lease read in the same transaction as the mutation.
pub enum Operation {
    Poll(Poll),
    Heartbeat(LeaseReport),
    Report(ResultReport),
    Release(LeaseReport),
}
pub enum Outcome {
    Assignment(Option<Box<Assignment>>),
    Renewed { remaining_seconds: i64 },
    Released,
    Denied,
    LeaseLost,
    Unauthenticated,
}

/// The remote run uses logical paths within its runner clone; the control side
/// never opens them. They are chosen by the control side, not by report data.
pub fn start(run: super::TaskRun, generation: i64) -> Result<super::TaskRun, super::DomainError> {
    if run.status() != super::RunStatus::Claimed {
        return super::run::assign_runner_branch(run, generation);
    }
    let id = run.id().to_string();
    let run = super::run::start_provisioning(
        run,
        &super::RunPlan {
            repo_path: ".".into(),
            run_dir: format!("runs/{id}"),
            branch: format!("dagq/{id}/{generation}"),
            worktree_path: ".".into(),
            receipt_path: format!("runs/{id}/receipt.json"),
            log_path: format!("runs/{id}/session.log"),
        },
    )?;
    super::run::assign_runner_branch(run, generation)
}

/// A result cannot set the aggregate's status directly. The existing run
/// command ends the session; the accompanying state and receipt are retained
/// in that transition's event, without reading or executing either of them.
pub fn finish(
    run: super::TaskRun,
    report: &ResultReport,
) -> Result<super::run::Recorded, super::DomainError> {
    super::CommitSha::parse(report.state.head.to_ascii_lowercase(), "head")?;
    if report.state.head != report.head {
        return Err(super::DomainError::RunInconsistent {
            run_id: run.id().clone(),
            reason: "reported head differs from state head",
        });
    }
    let (run, mut events) =
        super::run::end_session(run, report.exit_code, report.receipt.is_some())?;
    let payload = &mut events[0].payload;
    payload["state"] = serde_json::json!(&report.state);
    payload["runner_receipt"] = serde_json::json!(&report.receipt);
    payload["generation"] = serde_json::json!(report.generation);
    payload["head"] = serde_json::json!(report.head);
    Ok((run, events))
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReportRefusal {
    AuthorizationDenied,
    LeaseLost,
}
pub fn check_report(
    runner: &str,
    lease: Option<&super::runner_lease::RunnerLease>,
    generation: i64,
    now_ms: i64,
) -> Result<i64, ReportRefusal> {
    if !owns(runner, lease) {
        return Err(ReportRefusal::AuthorizationDenied);
    }
    match super::runner_lease::judge(
        lease,
        super::runner_lease::Report {
            runner_id: runner,
            generation,
        },
        now_ms,
    ) {
        super::runner_lease::Verdict::Accept { expires_at_ms } => Ok(expires_at_ms),
        super::runner_lease::Verdict::LeaseLost => Err(ReportRefusal::LeaseLost),
    }
}

#[cfg(test)]
mod report_tests {
    use super::*;
    use crate::domain::{
        CommitSha, Provider, RunId, RunRecord, RunStatus, TaskId,
        runner_lease::{Grant, LeaseState, RunnerLease, grant},
        worker::WorkerMode,
    };
    fn current() -> RunnerLease {
        RunnerLease {
            run_id: "run".into(),
            generation: 3,
            runner_id: "a".into(),
            expires_at_ms: 90_000,
            state: LeaseState::Held,
        }
    }
    fn run() -> crate::domain::TaskRun {
        crate::domain::TaskRun::restore(RunRecord {
            id: RunId::new("run").unwrap(),
            task_id: TaskId::new(1),
            status: RunStatus::Starting,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            worker_mode: WorkerMode::Headless,
            base_commit: CommitSha::parse("a".repeat(40), "base").unwrap(),
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
            created_at: "now".into(),
        })
        .unwrap()
    }
    #[test]
    fn current_heartbeat_renews_and_stale_reports_or_foreign_runs_are_refused() {
        let lease = current();
        assert_eq!(check_report("a", Some(&lease), 3, 20_000), Ok(110_000));
        for generation in [2, 4] {
            assert_eq!(
                check_report("a", Some(&lease), generation, 20_000),
                Err(ReportRefusal::LeaseLost)
            );
        }
        assert_eq!(
            check_report("b", Some(&lease), 3, 20_000),
            Err(ReportRefusal::AuthorizationDenied)
        );
        assert_eq!(
            check_report("a", None, 3, 20_000),
            Err(ReportRefusal::AuthorizationDenied)
        );
        assert_eq!(
            check_report("a", Some(&lease), 3, 90_000),
            Err(ReportRefusal::LeaseLost)
        );
        let returned = RunnerLease {
            state: LeaseState::Released,
            ..lease
        };
        assert_eq!(
            check_report("a", Some(&returned), 3, 20_000),
            Err(ReportRefusal::LeaseLost)
        );
        assert!(matches!(
            grant(Some(&returned), 20_000),
            Grant::Grant { generation: 4, .. }
        ));
        assert!(reassignable(&returned, &[], 20_000));
        let mut records = vec![crate::domain::runner_lease::GenerationRecord {
            generation: 3,
            accepted_head: Some("head".into()),
            verified_ref_head: None,
        }];
        assert!(!reassignable(&returned, &records, 20_000));
        records.push(crate::domain::runner_lease::GenerationRecord {
            generation: 4,
            accepted_head: None,
            verified_ref_head: Some("head".into()),
        });
        assert!(reassignable(&returned, &records, 20_000));
        assert!(!reassignable(&current(), &records, 20_000));
    }
    #[test]
    fn assignment_branch_names_follow_the_lease_generation() {
        let first = start(run(), 1).unwrap();
        assert_eq!(first.branch(), Some("dagq/run/1"));
        let next = start(first, 2).unwrap();
        assert_eq!(next.branch(), Some("dagq/run/2"));
        assert_eq!(next.status(), RunStatus::Starting);
    }

    #[test]
    fn result_uses_existing_transition_and_preserves_state_and_receipt_as_data() {
        let report: ResultReport = serde_json::from_value(serde_json::json!({
            "run_id":"run", "generation":3, "head":"b".repeat(40),
            "state":{"head":"b".repeat(40),"branch":"branch","status":"arbitrary data"},
            "receipt":{"result":"succeeded"}, "exit_code":0,
        }))
        .unwrap();
        let (next, events) = finish(run(), &report).unwrap();
        assert_eq!(next.status(), RunStatus::Validating);
        assert_eq!(
            events[0].payload["runner_receipt"],
            serde_json::json!({"result":"succeeded"})
        );
        assert_eq!(events[0].payload["head"], report.head);
        let mut inconsistent = report;
        inconsistent.head = "c".repeat(40);
        assert!(finish(run(), &inconsistent).is_err());
    }
}

/// A released/expired run can be offered immediately, provided an accepted
/// head is reachable from a verified successor generation's ref. Ref creation
/// belongs to the hand-over port; selection never trusts a remote branch tip.
pub fn reassignable(
    lease: &super::runner_lease::RunnerLease,
    records: &[super::runner_lease::GenerationRecord],
    now_ms: i64,
) -> bool {
    use super::runner_lease::{Grant, grant, newest_accepted_head};
    if !matches!(grant(Some(lease), now_ms), Grant::Grant { .. }) {
        return false;
    }
    newest_accepted_head(records).is_none_or(|accepted| {
        records.iter().any(|r| {
            r.generation == lease.generation.saturating_add(1)
                && r.verified_ref_head.as_deref() == Some(&accepted.head)
        })
    })
}
