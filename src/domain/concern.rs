//! What the runtime makes of a review's `concern` (ADR-t451-1 decision 3,
//! amending ADR-0027 decisions 1 and 2): the review job's recommendation
//! (`land` / `send_back`), its confidence and the reason a person is needed
//! (`scope` / `discard`) decide whether the runtime applies the
//! recommendation or asks a person in `approve_landing`, and
//! `concern_decided` records which.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{AskConfidence, DomainError};

// What the review job recommends for its `concern`: the options of the
// `approve_landing` ask it stands in for, `cancel` aside (a `discard`).
string_enum!(LandingRecommendation {
    Land => "land",
    SendBack => "send_back",
});

// Why a person is needed for a `concern` (ADR-0047 decision 41): landing
// would accept a departure from the acceptance, an ADR or the goal
// (`scope`), or the judgement is to cancel or throw the work away
// (`discard`). `null` in the verdict when neither.
string_enum!(ConcernReason {
    Scope => "scope",
    Discard => "discard",
});

// Why a `concern` went to a person rather than being applied
// (`concern_decided`'s `escalated_because`).
string_enum!(EscalatedBecause {
    // The verdict carries no recommendation (the form before ADR-t451-1).
    NoRecommendation => "no_recommendation",
    LowConfidence => "low_confidence",
    Scope => "scope",
    Discard => "discard",
    // A `send_back` past the revises of the round (`MAX_REVISE_ATTEMPTS`).
    ReviseLimit => "revise_limit",
    // A `send_back` the live session could not be sent: it had ended, the
    // request could not be typed, or its model switch was left unsettled.
    Unsent => "unsent",
});

/// What the runtime does with a `concern`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConcernDecision {
    /// Land it on the path of a `pass`.
    Land,
    /// Send it back to the live session as a `revise`.
    SendBack,
    /// Ask a person in `approve_landing`.
    Ask(EscalatedBecause),
}

/// Decide a `concern` from the job's `recommendation`, `confidence` and
/// `reason_category`. Only a `high` confidence with no reason a person is
/// needed is applied; a `send_back` also needs a revise left in the round
/// (`revise_left`). A `land` needs none, since it asks the session for no
/// further revise.
pub fn decide(
    recommendation: Option<LandingRecommendation>,
    confidence: Option<AskConfidence>,
    reason_category: Option<ConcernReason>,
    revise_left: bool,
) -> ConcernDecision {
    let Some(recommendation) = recommendation else {
        return ConcernDecision::Ask(EscalatedBecause::NoRecommendation);
    };
    match reason_category {
        Some(ConcernReason::Scope) => return ConcernDecision::Ask(EscalatedBecause::Scope),
        Some(ConcernReason::Discard) => return ConcernDecision::Ask(EscalatedBecause::Discard),
        None => {}
    }
    if confidence != Some(AskConfidence::High) {
        return ConcernDecision::Ask(EscalatedBecause::LowConfidence);
    }
    match recommendation {
        LandingRecommendation::Land => ConcernDecision::Land,
        LandingRecommendation::SendBack if revise_left => ConcernDecision::SendBack,
        LandingRecommendation::SendBack => ConcernDecision::Ask(EscalatedBecause::ReviseLimit),
    }
}

/// Whether the review a `review_finished` payload records lets the run
/// land without a person: a `pass`, or a `concern` the job recommends to
/// `land` with `high` confidence and no reason a person is needed, which
/// the runtime lands as it does a pass (ADR-t451-1 decision 3). Read from
/// the verdict alone, so every reader of a passed review agrees with
/// [`decide`], whose `land` needs no revise left.
pub fn lets_land(review: &Value) -> bool {
    match review["verdict"].as_str() {
        Some("pass") => true,
        Some("concern") => {
            decide(
                known(review["recommendation"].as_str()),
                known(review["confidence"].as_str()),
                reason(review["reason_category"].as_str()),
                true,
            ) == ConcernDecision::Land
        }
        _ => false,
    }
}

/// The payload of `concern_decided`: the review `attempt`, what the job
/// gave, whether it was applied, and why not when it was not.
pub fn decided_payload(
    attempt: usize,
    recommendation: Option<LandingRecommendation>,
    confidence: Option<AskConfidence>,
    reason_category: Option<ConcernReason>,
    escalated_because: Option<EscalatedBecause>,
) -> Value {
    json!({
        "attempt": attempt,
        "recommendation": recommendation,
        "confidence": confidence,
        "reason_category": reason_category,
        "applied": escalated_because.is_none(),
        "escalated_because": escalated_because,
    })
}

/// The verdict's text of a field as one of `T`'s values: `None` when
/// missing, null, blank or unknown.
pub(crate) fn known<T: std::str::FromStr>(value: Option<&str>) -> Option<T> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse().ok())
}

/// `reason_category` as the verdict gives it: null (or missing, or blank)
/// is no reason, and a value this binary does not know reads as `scope`,
/// so that a person decides it.
pub(crate) fn reason(value: Option<&str>) -> Option<ConcernReason> {
    let value = value.map(str::trim).filter(|v| !v.is_empty())?;
    Some(value.parse().unwrap_or(ConcernReason::Scope))
}

#[cfg(test)]
mod tests {
    use super::*;
    use AskConfidence::{High, Low};
    use LandingRecommendation::{Land, SendBack};

    #[test]
    fn only_a_high_confidence_without_a_reason_is_applied() {
        assert_eq!(
            decide(Some(Land), Some(High), None, true),
            ConcernDecision::Land
        );
        assert_eq!(
            decide(Some(Land), Some(High), None, false),
            ConcernDecision::Land
        );
        assert_eq!(
            decide(Some(SendBack), Some(High), None, true),
            ConcernDecision::SendBack
        );
        let ask = |why| ConcernDecision::Ask(why);
        assert_eq!(
            decide(Some(SendBack), Some(High), None, false),
            ask(EscalatedBecause::ReviseLimit)
        );
        assert_eq!(
            decide(Some(Land), Some(Low), None, true),
            ask(EscalatedBecause::LowConfidence)
        );
        assert_eq!(
            decide(Some(Land), None, None, true),
            ask(EscalatedBecause::LowConfidence)
        );
        assert_eq!(
            decide(Some(Land), Some(High), Some(ConcernReason::Scope), true),
            ask(EscalatedBecause::Scope)
        );
        assert_eq!(
            decide(
                Some(SendBack),
                Some(Low),
                Some(ConcernReason::Discard),
                true
            ),
            ask(EscalatedBecause::Discard)
        );
        assert_eq!(
            decide(None, Some(High), None, true),
            ask(EscalatedBecause::NoRecommendation)
        );
    }

    #[test]
    fn the_payload_says_whether_it_was_applied() {
        assert_eq!(
            decided_payload(2, Some(Land), Some(High), None, None),
            json!({"attempt": 2, "recommendation": "land", "confidence": "high",
                   "reason_category": null, "applied": true, "escalated_because": null})
        );
        assert_eq!(
            decided_payload(
                1,
                Some(SendBack),
                Some(High),
                Some(ConcernReason::Scope),
                Some(EscalatedBecause::Scope)
            ),
            json!({"attempt": 1, "recommendation": "send_back", "confidence": "high",
                   "reason_category": "scope", "applied": false, "escalated_because": "scope"})
        );
    }

    #[test]
    fn unknown_values_read_as_none_and_an_unknown_reason_as_scope() {
        assert_eq!(known::<LandingRecommendation>(Some(" land ")), Some(Land));
        assert_eq!(known::<LandingRecommendation>(Some("cancel")), None);
        assert_eq!(known::<AskConfidence>(Some("medium")), None);
        assert_eq!(known::<AskConfidence>(None), None);
        assert_eq!(reason(None), None);
        assert_eq!(reason(Some(" ")), None);
        assert_eq!(reason(Some("discard")), Some(ConcernReason::Discard));
        assert_eq!(reason(Some("cost")), Some(ConcernReason::Scope));
    }

    #[test]
    fn a_concern_verdict_carries_its_recommendation_and_others_do_not() {
        use crate::domain::ReviewVerdict;
        let concern = ReviewVerdict::parse(
            r#"{"verdict":"concern","reasons":["x"],"summary":"s","recommendation":"send_back","confidence":"high","reason_category":null}"#,
        )
        .unwrap();
        assert_eq!(concern.recommendation, Some(SendBack));
        assert_eq!(concern.confidence, Some(High));
        assert_eq!(concern.reason_category, None);
        assert_eq!(concern.concern_decision(true), ConcernDecision::SendBack);
        // The form before ADR-t451-1 asks a person.
        let plain =
            ReviewVerdict::parse(r#"{"verdict":"concern","reasons":["x"],"summary":"s"}"#).unwrap();
        assert_eq!(
            plain.concern_decision(true),
            ConcernDecision::Ask(EscalatedBecause::NoRecommendation)
        );
        // A pass or a revise needs none, and any it gives is not read.
        let pass = ReviewVerdict::parse(
            r#"{"verdict":"pass","reasons":[],"summary":"s","recommendation":"land","confidence":"high","reason_category":"scope"}"#,
        )
        .unwrap();
        assert_eq!(
            (pass.recommendation, pass.confidence, pass.reason_category),
            (None, None, None)
        );
    }

    #[test]
    fn a_pass_and_a_high_land_let_the_run_land() {
        assert!(lets_land(&json!({"verdict": "pass"})));
        assert!(lets_land(
            &json!({"verdict": "concern", "recommendation": "land",
                                  "confidence": "high", "reason_category": null})
        ));
        for review in [
            json!({"verdict": "concern"}),
            json!({"verdict": "concern", "recommendation": "land", "confidence": "low"}),
            json!({"verdict": "concern", "recommendation": "land", "confidence": "high",
                   "reason_category": "scope"}),
            json!({"verdict": "concern", "recommendation": "send_back", "confidence": "high"}),
            json!({"verdict": "revise", "recommendation": "land", "confidence": "high"}),
        ] {
            assert!(!lets_land(&review), "{review}");
        }
    }
}
