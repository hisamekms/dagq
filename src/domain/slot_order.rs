//! Worker slots go to returning runs, resumes, recovery jobs and claims
//! in descending effective priority (ADR-t5292-1 decision 3).
//! On the same priority a return comes first, then a resume, a recovery
//! job and a claim. Returns use the oldest wait end first. Other kinds
//! keep their input order, including claim aging within a priority.

use std::collections::HashMap;

use super::{Priority, TaskId};

/// What a candidate starts in a slot. The order of the variants is the
/// order on the same effective priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SlotKind {
    /// A human wait that ended at this timestamp in Unix milliseconds.
    Return { ended_at: i64 },
    /// A session resumed for a `needs_session` run.
    Resume,
    /// A recovery job for a `failed` or `interrupted` run.
    Recovery,
    /// The claim of a ready task.
    Claim,
}

/// One candidate for a slot: its kind, the effective priority of its task
/// and what the caller starts for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotCandidate<T> {
    pub kind: SlotKind,
    pub priority: Priority,
    pub item: T,
}

/// The claims of `order` (the claim's order, aging included) as candidates
/// of the line, each at its own effective priority from `effective`; a
/// task it has none of is left out. Aging lends no task another one's
/// priority (ADR-t1850-1 decisions 1 and 6, ADR-t1489-1).
pub fn claims(
    order: &[TaskId],
    effective: &HashMap<TaskId, Priority>,
) -> Vec<SlotCandidate<TaskId>> {
    order
        .iter()
        .filter_map(|id| {
            Some(SlotCandidate {
                kind: SlotKind::Claim,
                priority: *effective.get(id)?,
                item: *id,
            })
        })
        .collect()
}

/// `candidates` in the order they get a slot: effective priority, highest
/// first, then [`SlotKind`]'s order (including the wait end for returns),
/// then the order they were given in.
pub fn slot_order<T>(mut candidates: Vec<SlotCandidate<T>>) -> Vec<SlotCandidate<T>> {
    // Stable: a kind's own order survives within the same priority.
    candidates.sort_by_key(|candidate| (std::cmp::Reverse(candidate.priority), candidate.kind));
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(kind: SlotKind, priority: Priority, item: u32) -> SlotCandidate<u32> {
        SlotCandidate {
            kind,
            priority,
            item,
        }
    }

    fn order(candidates: Vec<SlotCandidate<u32>>) -> Vec<u32> {
        slot_order(candidates)
            .into_iter()
            .map(|candidate| candidate.item)
            .collect()
    }

    #[test]
    fn a_higher_effective_priority_comes_first_whatever_the_kind() {
        let candidates = vec![
            candidate(SlotKind::Resume, Priority::Low, 1),
            candidate(SlotKind::Recovery, Priority::Normal, 2),
            candidate(SlotKind::Claim, Priority::High, 3),
            candidate(SlotKind::Resume, Priority::Interrupt, 4),
            candidate(SlotKind::Claim, Priority::Low, 5),
            candidate(SlotKind::Recovery, Priority::Urgent, 6),
        ];
        assert_eq!(order(candidates), [4, 6, 3, 2, 1, 5]);
    }

    #[test]
    fn at_equal_priority_returns_precede_resumes_recoveries_and_claims() {
        let candidates = vec![
            candidate(SlotKind::Claim, Priority::Normal, 1),
            candidate(SlotKind::Recovery, Priority::Normal, 2),
            candidate(SlotKind::Resume, Priority::Normal, 3),
            candidate(SlotKind::Return { ended_at: 20 }, Priority::Normal, 4),
            candidate(SlotKind::Return { ended_at: 10 }, Priority::Normal, 5),
            candidate(SlotKind::Return { ended_at: 0 }, Priority::Low, 6),
        ];
        assert_eq!(order(candidates), [5, 4, 3, 2, 1, 6]);
    }

    #[test]
    fn within_a_kind_and_priority_the_given_order_stays() {
        let candidates = vec![
            candidate(SlotKind::Resume, Priority::Normal, 3),
            candidate(SlotKind::Claim, Priority::Normal, 9),
            candidate(SlotKind::Resume, Priority::Normal, 1),
            candidate(SlotKind::Claim, Priority::Normal, 7),
            candidate(SlotKind::Recovery, Priority::Normal, 5),
            candidate(SlotKind::Recovery, Priority::Normal, 4),
            candidate(SlotKind::Resume, Priority::Normal, 2),
        ];
        assert_eq!(order(candidates), [3, 1, 2, 5, 4, 9, 7]);
    }

    #[test]
    fn no_candidate_gives_no_order() {
        assert!(order(Vec::new()).is_empty());
    }

    /// A claim aging moved ahead competes at its own effective priority: a
    /// low Spike first among the low claims stays behind a normal resume
    /// and recovery job, and a normal one behind a normal resume.
    #[test]
    fn an_aged_claim_competes_with_resumes_at_its_own_priority() {
        use crate::domain::ExecutionClass;
        use crate::domain::claim_order::{Candidate, ClassLimits, arrange};
        let candidate = |id: i64, class, priority, waited_min: i64| Candidate {
            id: TaskId::new(id),
            class,
            priority,
            ready_at_ms: Some(100 * 60_000 - waited_min * 60_000),
        };
        let lined = [
            candidate(1, ExecutionClass::Implementation, Priority::High, 1),
            candidate(2, ExecutionClass::Implementation, Priority::Low, 1),
            candidate(3, ExecutionClass::Spike, Priority::Low, 60),
            candidate(4, ExecutionClass::Implementation, Priority::Normal, 1),
            candidate(5, ExecutionClass::Spike, Priority::Normal, 60),
        ];
        // The rule's order: by priority, then ID here.
        let ruled = [&lined[0], &lined[3], &lined[4], &lined[1], &lined[2]].map(|c| *c);
        let arranged = arrange(
            &ruled,
            &Default::default(),
            ClassLimits::default(),
            100 * 60_000,
            600,
        );
        assert_eq!(
            arranged.aged,
            [TaskId::new(5), TaskId::new(3)],
            "each Spike moved within its priority"
        );
        let effective: HashMap<TaskId, Priority> =
            lined.iter().map(|c| (c.id, c.priority)).collect();
        let mut line: Vec<SlotCandidate<u32>> = claims(&arranged.order, &effective)
            .into_iter()
            .map(|claim| SlotCandidate {
                kind: claim.kind,
                priority: claim.priority,
                item: u32::try_from(claim.item.as_i64()).unwrap(),
            })
            .collect();
        line.push(candidate_of(SlotKind::Resume, Priority::Normal, 10));
        line.push(candidate_of(SlotKind::Recovery, Priority::Normal, 11));
        assert_eq!(order(line), [1, 10, 11, 5, 4, 3, 2]);
    }

    fn candidate_of(kind: SlotKind, priority: Priority, item: u32) -> SlotCandidate<u32> {
        candidate(kind, priority, item)
    }
}
