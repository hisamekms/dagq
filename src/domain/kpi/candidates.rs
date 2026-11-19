//! The sample of what the supervisor could claim (ADR-0051 decision 3):
//! the claimable ready tasks, the free slots, the ready tasks and whether
//! claims are held, recorded as [`CANDIDATES_SAMPLED`] on changes.
use serde_json::Value;

use crate::domain::admission::Blocked;

#[cfg(doc)]
use super::CANDIDATES_SAMPLED;

/// One pass's candidate counts and whether claims are held. How long a
/// recorded sample holds in `kpi`: [`CANDIDATES_SAMPLED`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CandidatesSample {
    /// The ready tasks the supervisor could claim by the rule's order
    /// (`crate::application::dependency_graph`'s `candidates`), every task
    /// whose claim it deferred included (for a conflict hotspot or a class
    /// limit): unlike the `candidates` that the `graph` and `candidates`
    /// commands show by default, which leave the deferred tasks out
    /// (`crate::application::claim_view`).
    pub candidates: usize,
    /// `--parallel` less the slots in use.
    pub free_slots: usize,
    /// The tasks in `ready`, claimable or not.
    pub ready: usize,
    pub end: ClaimPassEnd,
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
        let mut payload = serde_json::json!({
            "candidates": self.candidates,
            "free_slots": self.free_slots,
            "ready": self.ready,
            "claims_held": self.end.claims_held(),
            "admission_reason": self.end.reason.map(Blocked::name),
        });
        if let Some(secs) = self.end.spacing_secs {
            payload["claim_spacing"] = serde_json::json!(secs);
        }
        Some(payload)
    }
}

/// The result of the claim path in one supervisor pass. A full slot,
/// an empty line and the spacing between claims leave the claims open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClaimPassEnd {
    pub reason: Option<Blocked>,
    pub spacing_secs: Option<usize>,
}

impl ClaimPassEnd {
    pub fn blocked(reason: Blocked) -> Self {
        Self {
            reason: Some(reason),
            spacing_secs: None,
        }
    }

    pub fn claims_held(self) -> bool {
        self.reason.is_some_and(|reason| {
            !matches!(
                reason,
                Blocked::WorkerSlot
                    | Blocked::Returning
                    | Blocked::ClaimSpacing
                    | Blocked::NoCandidates
                    | Blocked::CandidatesDeferred
            )
        })
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
        end: ClaimPassEnd {
            reason: None,
            spacing_secs: None,
        },
    };

    #[test]
    fn the_first_pass_of_a_process_records_its_sample() {
        assert_eq!(
            SAMPLE.transition(None),
            Some(
                json!({"candidates": 2, "free_slots": 1, "ready": 3, "claims_held": false, "admission_reason": null})
            )
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
            CandidatesSample {
                end: ClaimPassEnd::blocked(Blocked::Load),
                ..SAMPLE
            },
        ] {
            let payload = changed.transition(Some(&SAMPLE)).unwrap();
            assert_eq!(payload["candidates"], changed.candidates);
            assert_eq!(payload["free_slots"], changed.free_slots);
            assert_eq!(payload["ready"], changed.ready);
            assert_eq!(payload["claims_held"], changed.end.claims_held());
        }
    }
}
