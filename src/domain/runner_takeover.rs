//! Restartable runner hand-over. Only accepted heads and verified generation
//! marks are authority; an earlier runner's remote tip, session and build files
//! are never inputs. Ref creation is idempotent and never overwrites a ref.
use super::{RunStatus, runner_lease::*};

/// Persistent facts read together before a control pass. Registry records are
/// supplied separately to `runner_lease::loss` so expiry never uses runner time.
#[derive(Clone, Debug)]
pub struct Takeover {
    pub lease: RunnerLease,
    pub status: RunStatus,
    pub records: Vec<GenerationRecord>,
}

/// The observation of the successor ref, never an old generation's tip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefLook {
    Missing,
    Present,
    ReadBack(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Recover,
    WaitRecovery,
    Create { generation: i64, head: String },
    Read { generation: i64 },
    Mark { generation: i64, head: String },
    Assign,
    Delete { generation: i64 },
    Wait,
    RefMismatch,
}

/// Select one next effect. A lost unfinished run first follows existing
/// recovery (including its triage); it enters selection again once resumable.
/// A deliberately returned, unfinished assignment keeps its immediate path.
/// Once granted, cleanup may delete older refs only while the granted ref's
/// durable mark matches the accepted head. Each retry uses the highest verified
/// generation, including a successor verified before a crash at assignment.
/// `old_refs` contains existence only, not their (possibly unaccepted) tips.
pub fn next(facts: &Takeover, look: &RefLook, old_refs: &[i64]) -> Step {
    let held = facts.lease.state == LeaseState::Held;
    if facts.lease.state == LeaseState::Lost
        && matches!(
            facts.status,
            RunStatus::Claimed
                | RunStatus::Starting
                | RunStatus::Running
                | RunStatus::Validating
                | RunStatus::Integrating
        )
    {
        return Step::Recover;
    }
    let returned = facts.lease.state == LeaseState::Released
        && matches!(
            facts.status,
            RunStatus::Claimed | RunStatus::Starting | RunStatus::Running
        );
    if !held && facts.status != RunStatus::NeedsSession && !returned {
        return Step::WaitRecovery;
    }
    let Some(accepted) = newest_accepted_head(&facts.records) else {
        return if held { Step::Wait } else { Step::Assign };
    };
    let generation = if held {
        facts.lease.generation
    } else {
        let Some(next) = facts.lease.generation.checked_add(1) else {
            return Step::Wait;
        };
        next
    };
    let verified = newest_verified_ref(&facts.records);
    if verified
        .as_ref()
        .is_some_and(|r| r.generation == generation && r.head == accepted.head)
    {
        if !held {
            return Step::Assign;
        }
        return old_refs
            .iter()
            .copied()
            .filter(|g| *g < generation)
            .min()
            .map_or(Step::Wait, |generation| Step::Delete { generation });
    }
    if held {
        return Step::Wait;
    }
    match look {
        RefLook::Missing => Step::Create {
            generation,
            head: accepted.head,
        },
        RefLook::Present => Step::Read { generation },
        RefLook::ReadBack(head) if *head == accepted.head => Step::Mark {
            generation,
            head: head.clone(),
        },
        RefLook::ReadBack(_) => Step::RefMismatch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn facts(head: bool) -> Takeover {
        Takeover {
            lease: RunnerLease {
                run_id: "r".into(),
                generation: 1,
                runner_id: "a".into(),
                expires_at_ms: 90_000,
                state: LeaseState::Lost,
            },
            status: RunStatus::NeedsSession,
            records: vec![GenerationRecord {
                generation: 1,
                accepted_head: head.then(|| "H".into()),
                verified_ref_head: head.then(|| "H".into()),
            }],
        }
    }
    #[test]
    fn every_interrupted_stage_retries_before_assignment_and_deletion() {
        let mut f = facts(true);
        let old = [1];
        assert_eq!(
            next(&f, &RefLook::Missing, &old),
            Step::Create {
                generation: 2,
                head: "H".into()
            }
        );
        assert_eq!(
            next(&f, &RefLook::Present, &old),
            Step::Read { generation: 2 }
        );
        assert_eq!(
            next(&f, &RefLook::ReadBack("H".into()), &old),
            Step::Mark {
                generation: 2,
                head: "H".into()
            }
        );
        // A crash before the durable mark always restarts idempotent creation.
        assert!(matches!(
            next(&f, &RefLook::Missing, &old),
            Step::Create { .. }
        ));
        f.records.push(GenerationRecord {
            generation: 2,
            accepted_head: None,
            verified_ref_head: Some("H".into()),
        });
        assert_eq!(next(&f, &RefLook::Missing, &old), Step::Assign);
        f.lease.generation = 2;
        f.lease.state = LeaseState::Held;
        f.status = RunStatus::Running;
        assert_eq!(
            next(&f, &RefLook::Missing, &old),
            Step::Delete { generation: 1 }
        );
        assert_eq!(next(&f, &RefLook::Missing, &[]), Step::Wait);
        // A second loss starts from the newest verified ref, preserving H.
        f.lease.state = LeaseState::Lost;
        f.status = RunStatus::NeedsSession;
        assert_eq!(
            next(&f, &RefLook::Missing, &[2]),
            Step::Create {
                generation: 3,
                head: "H".into()
            }
        );
        assert_eq!(newest_verified_ref(&f.records).unwrap().generation, 2);
    }
    #[test]
    fn a_mismatch_or_missing_mark_never_assigns_or_deletes() {
        let mut f = facts(true);
        assert_eq!(
            next(&f, &RefLook::ReadBack("U".into()), &[1]),
            Step::RefMismatch
        );
        assert!(!super::super::runner_assignment::reassignable(
            &f.lease, &f.records, 90_000
        ));
        f.records.push(GenerationRecord {
            generation: 2,
            accepted_head: None,
            verified_ref_head: Some("U".into()),
        });
        assert!(!super::super::runner_assignment::reassignable(
            &f.lease, &f.records, 90_000
        ));
        assert_eq!(
            next(&f, &RefLook::Present, &[1]),
            Step::Read { generation: 2 }
        );
        f.records[1].verified_ref_head = Some("H".into());
        assert!(super::super::runner_assignment::reassignable(
            &f.lease, &f.records, 90_000
        ));
    }
    #[test]
    fn a_run_without_an_accepted_head_needs_no_ref_but_still_needs_recovery() {
        let mut f = facts(false);
        assert_eq!(next(&f, &RefLook::Missing, &[]), Step::Assign);
        assert!(super::super::runner_assignment::reassignable(
            &f.lease, &f.records, 90_000
        ));
        f.status = RunStatus::Running;
        assert_eq!(next(&f, &RefLook::Missing, &[]), Step::Recover);
        f.status = RunStatus::Interrupted;
        assert_eq!(next(&f, &RefLook::Missing, &[]), Step::WaitRecovery);
        f.status = RunStatus::Failed;
        assert_eq!(next(&f, &RefLook::Missing, &[]), Step::WaitRecovery);
        // A deliberately returned, unstarted assignment keeps the pull's
        // existing immediate hand-back path, without orphan recovery.
        f.lease.state = LeaseState::Released;
        f.status = RunStatus::Starting;
        assert_eq!(next(&f, &RefLook::Missing, &[]), Step::Assign);
    }
}
