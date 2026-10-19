//! The sample of what the supervisor could claim (ADR-0051 decision 3):
//! the claimable ready tasks, the free slots, the ready tasks and whether
//! claims are held, recorded as [`CANDIDATES_SAMPLED`] on changes.
use serde_json::Value;

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
    /// Claims are deliberately held or this supervisor is draining.
    pub claims_held: bool,
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
            "claims_held": self.claims_held,
        }))
    }
}

/// How a supervisor's claim pass ended, for
/// [`CandidatesSample::claims_held`]. Running out of room or waiting for
/// the claim spacing ends the claims of a pass too, but holds nothing: the
/// supervisor would claim the next candidate as soon as a slot frees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimPassEnd {
    /// The pass set its claims up: nothing held them (load, disk, a queue
    /// hold), no `[run.env]` program was missing, the CI could be read,
    /// the landing branch resolved and the queue service was up.
    pub may_claim: bool,
    /// The supervisor still claims after the pass: a worker that could not
    /// be provisioned stops it.
    pub claiming: bool,
    /// The landing branch still resolves after the pass.
    pub landing_resolved: bool,
    /// A provider is left to take a worker.
    pub route_left: bool,
}

impl ClaimPassEnd {
    /// Whether the pass held the claims: it could not set them up, or they
    /// stopped in the middle of it for a reason that outlives the pass.
    pub fn claims_held(self) -> bool {
        !(self.may_claim && self.claiming && self.landing_resolved && self.route_left)
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
        claims_held: false,
    };

    const CLAIMED: ClaimPassEnd = ClaimPassEnd {
        may_claim: true,
        claiming: true,
        landing_resolved: true,
        route_left: true,
    };

    /// A pass whose claims ran out of room or waited for the spacing leaves
    /// every input as it found it: it holds nothing.
    #[test]
    fn a_pass_out_of_room_or_spacing_does_not_hold_the_claims() {
        assert!(!CLAIMED.claims_held());
    }

    #[test]
    fn a_held_pass_or_one_stopped_in_the_middle_holds_the_claims() {
        for (case, end) in [
            (
                "held before the claims",
                ClaimPassEnd {
                    may_claim: false,
                    ..CLAIMED
                },
            ),
            (
                "a worker could not be provisioned",
                ClaimPassEnd {
                    claiming: false,
                    ..CLAIMED
                },
            ),
            (
                "the landing branch stopped resolving",
                ClaimPassEnd {
                    landing_resolved: false,
                    ..CLAIMED
                },
            ),
            (
                "no provider is left",
                ClaimPassEnd {
                    route_left: false,
                    ..CLAIMED
                },
            ),
        ] {
            assert!(end.claims_held(), "{case}");
        }
    }

    #[test]
    fn the_first_pass_of_a_process_records_its_sample() {
        assert_eq!(
            SAMPLE.transition(None),
            Some(json!({"candidates": 2, "free_slots": 1, "ready": 3, "claims_held": false}))
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
                claims_held: true,
                ..SAMPLE
            },
        ] {
            let payload = changed.transition(Some(&SAMPLE)).unwrap();
            assert_eq!(payload["candidates"], changed.candidates);
            assert_eq!(payload["free_slots"], changed.free_slots);
            assert_eq!(payload["ready"], changed.ready);
            assert_eq!(payload["claims_held"], changed.claims_held);
        }
    }
}
