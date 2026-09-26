//! The sample of what the supervisor could claim (ADR-0051 decision 3):
//! the claimable ready tasks, the free slots and the ready tasks, recorded
//! as [`CANDIDATES_SAMPLED`] only when one of them changes.
use serde_json::Value;

#[cfg(doc)]
use super::CANDIDATES_SAMPLED;

/// One pass's `candidates`, `free_slots` and `ready`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CandidatesSample {
    /// The ready tasks the supervisor could claim: `graph`'s `candidates`.
    pub candidates: usize,
    /// `--parallel` less the slots in use.
    pub free_slots: usize,
    /// The tasks in `ready`, claimable or not.
    pub ready: usize,
}

impl CandidatesSample {
    /// The payload of the [`CANDIDATES_SAMPLED`] event to record when this
    /// pass's sample differs from `last`, the one this process recorded
    /// last; `None` when nothing changed. A process that recorded none yet
    /// (just started or handed over to) records its first pass.
    pub fn transition(&self, last: Option<&Self>) -> Option<Value> {
        if last == Some(self) {
            return None;
        }
        Some(serde_json::json!({
            "candidates": self.candidates,
            "free_slots": self.free_slots,
            "ready": self.ready,
        }))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::kpi::CANDIDATES_SAMPLED;

    const SAMPLE: CandidatesSample = CandidatesSample {
        candidates: 2,
        free_slots: 1,
        ready: 3,
    };

    #[test]
    fn the_first_pass_of_a_process_records_its_sample() {
        assert_eq!(
            SAMPLE.transition(None),
            Some(json!({"candidates": 2, "free_slots": 1, "ready": 3}))
        );
        assert_eq!(CANDIDATES_SAMPLED, "candidates_sampled");
    }

    #[test]
    fn an_unchanged_sample_is_not_recorded_again() {
        assert_eq!(SAMPLE.transition(Some(&SAMPLE)), None);
    }

    #[test]
    fn a_change_of_any_value_is_recorded() {
        for changed in [
            CandidatesSample {
                candidates: 0,
                ..SAMPLE
            },
            CandidatesSample {
                free_slots: 0,
                ..SAMPLE
            },
            CandidatesSample { ready: 4, ..SAMPLE },
        ] {
            let payload = changed.transition(Some(&SAMPLE)).unwrap();
            assert_eq!(payload["candidates"], changed.candidates);
            assert_eq!(payload["free_slots"], changed.free_slots);
            assert_eq!(payload["ready"], changed.ready);
        }
    }
}
