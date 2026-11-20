//! Order the work that needs a new runtime planner (ADR-t5583-1).
use super::Priority;
use std::cmp::Reverse;

/// Declaration order is the tie break between different kinds of work.
/// Resuming parked work uses its original kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
    Revise,
    Answer,
    Request,
    CiRepair,
    AgentWatch,
    Draft,
    Improvement,
}

pub(crate) struct Candidate<T> {
    pub kind: Kind,
    pub priority: Priority,
    /// Unix milliseconds from when the work became eligible to wait for a slot.
    pub waiting_since: i64,
    pub work: T,
}

/// With room, return the entire order. A start may fail to take a slot
/// after rechecking the work, so the caller must stop on actual opens.
pub(crate) fn order<T>(free: usize, mut candidates: Vec<Candidate<T>>) -> Vec<T> {
    if free == 0 {
        return Vec::new();
    }
    candidates.sort_by_key(|candidate| {
        (
            Reverse(candidate.priority),
            candidate.kind,
            candidate.waiting_since,
        )
    });
    candidates
        .into_iter()
        .map(|candidate| candidate.work)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Kind::*, Priority::*, *};

    #[test]
    fn priority_then_kind_then_age_order_all_candidates_when_there_is_room() {
        let cases = [
            ("later improvement", Improvement, Normal, 20),
            ("oldest draft", Draft, Normal, 1),
            ("high finding", Improvement, High, 2),
            ("old improvement", Improvement, Normal, 10),
            ("urgent draft", Draft, Urgent, 30),
            ("watch", AgentWatch, Normal, 1),
            ("repair", CiRepair, Normal, 40),
            ("request", Request, Normal, 40),
            ("answer", Answer, Normal, 40),
            ("normal revise", Revise, Normal, 40),
            ("interrupt revise", Revise, Interrupt, 50),
        ];
        let candidates = || {
            cases
                .map(|(work, kind, priority, waiting_since)| Candidate {
                    work,
                    kind,
                    priority,
                    waiting_since,
                })
                .into()
        };
        assert!(order(0, candidates()).is_empty());
        assert_eq!(
            order(1, candidates()),
            [
                "interrupt revise",
                "urgent draft",
                "high finding",
                "normal revise",
                "answer",
                "request",
                "repair",
                "watch",
                "oldest draft",
                "old improvement",
                "later improvement",
            ]
        );
    }
}
