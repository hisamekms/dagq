//! The order a fill pass gives its free slots in (ADR-t1850-1): the
//! resumes of `needs_session` runs, the recovery jobs of ended runs and the
//! claims of ready tasks stand in one line, by the effective priority of
//! their task, highest first. On the same priority a resume comes first,
//! then a recovery job, then a claim; within a kind the order stays the one
//! the kind already had (resumes oldest first, recovery jobs in the order
//! the queue lists the runs, claims by [`super::ClaimRank`]).

use super::Priority;

/// What a candidate starts in a slot. The order of the variants is the
/// order on the same effective priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SlotKind {
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

/// `candidates` in the order they get a slot: effective priority, highest
/// first, then [`SlotKind`]'s order, then the order they were given in
/// (each kind's own order).
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
    fn on_the_same_priority_a_resume_comes_before_a_recovery_job_and_a_claim() {
        let candidates = vec![
            candidate(SlotKind::Claim, Priority::Normal, 1),
            candidate(SlotKind::Recovery, Priority::Normal, 2),
            candidate(SlotKind::Resume, Priority::Normal, 3),
        ];
        assert_eq!(order(candidates), [3, 2, 1]);
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
}
