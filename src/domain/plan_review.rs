//! Plan review (ADR-0041 decisions 10, 11, 14, 15): the verdict the
//! headless job prints about one proposal, the fixes it may make itself,
//! the order the supervisor takes submitted proposals in, and the answers
//! a person gives to its `approve_plan` ask.

use serde::{Deserialize, Serialize};

use super::{
    AskConfidence, AskId, AskReason, DomainError, Priority, ProposalId, TaskId, parse_json_object,
};

// What plan review decided (ADR-0041 decision 11): `pass` makes the
// proposal's tasks ready, `revise` sends it back to its planner, `concern`
// waits for a person in an `approve_plan` ask.
string_enum!(PlanReviewDecision {
    Pass => "pass",
    Revise => "revise",
    Concern => "concern",
});

// How a plan review job's row ended (ADR-t876-1: the rule the
// `plan_reviews.outcome` CHECK held): its verdict's decision, or `failed` /
// `interrupted` when the job gave none.
string_enum!(PlanReviewOutcome {
    Pass => "pass",
    Revise => "revise",
    Concern => "concern",
    Failed => "failed",
    Interrupted => "interrupted",
});

impl From<PlanReviewDecision> for PlanReviewOutcome {
    fn from(decision: PlanReviewDecision) -> Self {
        match decision {
            PlanReviewDecision::Pass => Self::Pass,
            PlanReviewDecision::Revise => Self::Revise,
            PlanReviewDecision::Concern => Self::Concern,
        }
    }
}

// Why a submitted proposal waits for a person instead of plan review
// (ADR-t876-1: the rule the `proposals.review_hold` CHECK held): its job
// failed, or its verdict was a concern.
string_enum!(ReviewHold {
    Failed => "failed",
    Concern => "concern",
});

// What a plan review's `concern` recommends (ADR-t451-1 decision 4): make
// the tasks ready, send the proposal back to its planner, or cancel it (a
// `discard`, which a person always decides).
string_enum!(PlanRecommendation {
    Ready => "ready",
    SendBack => "send_back",
    Cancel => "cancel",
});

// Why a `concern` needs a person whatever its confidence (ADR-t451-1
// decision 4, ADR-0047 decision 41): `scope` lets a task through against a
// recorded decision, a goal's constraints or a person's precedent;
// `discard` cancels it.
string_enum!(PlanConcernReason {
    Scope => "scope",
    Discard => "discard",
});

// Why the runtime left a `concern` to a person in an `approve_plan` ask
// instead of applying its recommendation (`plan_concern_decided`'s
// `escalated_because`).
string_enum!(PlanConcernEscalation {
    NoRecommendation => "no_recommendation",
    Discard => "discard",
    Scope => "scope",
    LowConfidence => "low_confidence",
    ReviseLimit => "revise_limit",
});

/// What the runtime makes of a `concern` (ADR-t451-1 decision 4): the
/// decision it applies itself, or why a person decides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanConcernDecision {
    pub recommendation: Option<PlanRecommendation>,
    pub confidence: Option<AskConfidence>,
    pub reason_category: Option<PlanConcernReason>,
    /// `pass` for a sure `ready`, `revise` for a sure `send_back`; `None`
    /// when it opens the `approve_plan` ask.
    pub applied: Option<PlanReviewDecision>,
    pub escalated_because: Option<PlanConcernEscalation>,
}

impl PlanConcernDecision {
    /// The reason the `approve_plan` ask carries: `discard` for a
    /// cancel (even in a concern missing its confidence), `scope`
    /// otherwise (as every `approve_plan` ask did).
    pub fn ask_reason(&self) -> AskReason {
        if self.recommendation == Some(PlanRecommendation::Cancel)
            || self.reason_category == Some(PlanConcernReason::Discard)
        {
            AskReason::Discard
        } else {
            AskReason::Scope
        }
    }
}

/// A fix plan review makes itself (ADR-0041 decision 11). Nothing else is
/// the job's to change: rewriting a task, splitting it, removing a
/// dependency or raising a priority is the planner's, through `revise`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanReviewAction {
    /// `task_id` (a task of the proposal) waits for `depends_on`.
    AddDependency { task_id: TaskId, depends_on: TaskId },
    /// `task_id` (a task of the proposal) gets the lower `priority`.
    LowerPriority { task_id: TaskId, priority: Priority },
    /// `task_id` (a task of the proposal) repeats `duplicate_of`, another
    /// task, so it is canceled. Only an obvious duplicate: a doubtful one is
    /// a `concern`.
    CancelDuplicate {
        task_id: TaskId,
        duplicate_of: TaskId,
    },
}

impl PlanReviewAction {
    /// The task of the proposal the action changes.
    pub fn task_id(&self) -> TaskId {
        match self {
            Self::AddDependency { task_id, .. }
            | Self::LowerPriority { task_id, .. }
            | Self::CancelDuplicate { task_id, .. } => *task_id,
        }
    }
}

/// A task already `ready`, outside the proposal, that has to change for
/// the proposal to hold (ADR-0041 decision 14): the runtime takes it out
/// of `ready` and a planner of its own fixes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reopen {
    pub task_id: TaskId,
    pub reason: String,
}

/// What the headless plan review prints on stdout: one JSON object.
/// `actions` are applied on `pass` only; `reopen` whatever the verdict;
/// `precedents` name asks a person answered before about the same kind of
/// finding, which the runtime quotes to the planner or the person;
/// `predictions` are recorded, whatever their shape, apart from the rest.
/// Each item of `reasons` is a text, or a text with its reason codes
/// (ADR-t947-1), as the run review's: `reasons` keeps the texts and
/// `reason_codes` each item's codes, empty for an item without them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "PrintedPlanReviewVerdict")]
pub struct PlanReviewVerdict {
    pub verdict: PlanReviewDecision,
    pub reasons: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<Vec<String>>,
    pub summary: String,
    pub actions: Vec<PlanReviewAction>,
    pub reopen: Vec<Reopen>,
    pub precedents: Vec<AskId>,
    /// The weight of each submitted task of the proposal (ADR-0079
    /// decision 2), kept raw: a shape that does not hold is checked by
    /// [`super::prediction::parse_predictions`] and only leaves the
    /// predictions unrecorded, never the verdict failed. It is not typed
    /// here on purpose: a typed field would fail the whole verdict, and so
    /// the review, on a malformed estimate that ADR-0079 decision 2 says
    /// changes nothing. The raw value maps to no transition: the runtime
    /// reads it only through `parse_predictions`, into typed
    /// predictions, to record the estimates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predictions: Option<serde_json::Value>,
    /// A `concern`'s recommendation, confidence and reason a person is
    /// needed (ADR-t451-1 decision 4); read on a `concern` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<PlanRecommendation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<AskConfidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_category: Option<PlanConcernReason>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrintedPlanReviewVerdict {
    verdict: PlanReviewDecision,
    reasons: Vec<super::review_reason::PrintedReason>,
    summary: String,
    #[serde(default)]
    actions: Vec<PlanReviewAction>,
    #[serde(default)]
    reopen: Vec<Reopen>,
    #[serde(default)]
    precedents: Vec<AskId>,
    #[serde(default)]
    predictions: Option<serde_json::Value>,
    #[serde(default)]
    recommendation: Option<PlanRecommendation>,
    #[serde(default)]
    confidence: Option<AskConfidence>,
    #[serde(default)]
    reason_category: Option<PlanConcernReason>,
}

impl From<PrintedPlanReviewVerdict> for PlanReviewVerdict {
    fn from(printed: PrintedPlanReviewVerdict) -> Self {
        let (reasons, reason_codes) = super::review_reason::split(printed.reasons);
        Self {
            verdict: printed.verdict,
            reasons,
            reason_codes,
            summary: printed.summary,
            actions: printed.actions,
            reopen: printed.reopen,
            precedents: printed.precedents,
            predictions: printed.predictions,
            recommendation: printed.recommendation,
            confidence: printed.confidence,
            reason_category: printed.reason_category,
        }
    }
}

impl PlanReviewVerdict {
    /// The codes `plan_review_finished` records for each reason,
    /// `unlabeled` for one without them.
    pub fn recorded_codes(&self) -> Vec<Vec<String>> {
        super::review_reason::recorded(&self.reason_codes, self.reasons.len())
    }

    /// What the runtime makes of a `concern` after `revise_count` revises
    /// (ADR-t451-1 decision 4); `None` for any other verdict. A sure
    /// (`high`) `ready` or `send_back` with no reason a person is needed
    /// is applied as a `pass` or a `revise`; a `send_back` past
    /// [`MAX_PLAN_REVISES`], a `low` confidence, a `scope` or a `discard`
    /// (a `cancel`), and a concern without both a recommendation and a
    /// confidence (the shape before ADR-t451-1) open the `approve_plan`
    /// ask.
    pub fn decide_concern(&self, revise_count: u32) -> Option<PlanConcernDecision> {
        if self.verdict != PlanReviewDecision::Concern {
            return None;
        }
        let escalated_because = if self.recommendation.is_none() || self.confidence.is_none() {
            Some(PlanConcernEscalation::NoRecommendation)
        } else if self.recommendation == Some(PlanRecommendation::Cancel)
            || self.reason_category == Some(PlanConcernReason::Discard)
        {
            Some(PlanConcernEscalation::Discard)
        } else if self.reason_category == Some(PlanConcernReason::Scope) {
            Some(PlanConcernEscalation::Scope)
        } else if self.confidence == Some(AskConfidence::Low) {
            Some(PlanConcernEscalation::LowConfidence)
        } else if self.recommendation == Some(PlanRecommendation::SendBack)
            && revise_count >= MAX_PLAN_REVISES
        {
            Some(PlanConcernEscalation::ReviseLimit)
        } else {
            None
        };
        let applied = match (escalated_because, self.recommendation) {
            (None, Some(PlanRecommendation::Ready)) => Some(PlanReviewDecision::Pass),
            (None, Some(PlanRecommendation::SendBack)) => Some(PlanReviewDecision::Revise),
            _ => None,
        };
        Some(PlanConcernDecision {
            recommendation: self.recommendation,
            confidence: self.confidence,
            reason_category: self.reason_category,
            applied,
            escalated_because,
        })
    }

    /// The verdict in the job's stdout, found the way the run review's is.
    pub fn parse(stdout: &str) -> Result<Self, String> {
        parse_json_object(stdout)
            .map_err(|error| format!("the plan review printed no verdict JSON: {error}"))
    }
}

/// How many times plan review sends one proposal back to its planner; a
/// later review that does not pass is a `concern` (ADR-0041 decision 11,
/// the same bound as a run's review).
pub const MAX_PLAN_REVISES: u32 = super::MAX_REVISE_ATTEMPTS as u32;

/// The options of the `approve_plan` ask a `concern` opens, which the
/// supervisor applies once answered (ADR-0041 decision 11).
pub const PLAN_OPTIONS: &[&str] = &["ready", "send_back", "cancel"];

/// Who opens the `approve_plan` asks.
pub const PLAN_REVIEW_ASKER: &str = "plan_review";

/// A person's answer to an `approve_plan` ask the supervisor applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAnswer {
    /// Make the proposal's tasks ready as they are.
    Ready,
    /// Send the proposal back to its planner, with the person's reason
    /// when they gave one (`send_back: <reason>`).
    SendBack(Option<String>),
    /// Cancel the proposal's tasks.
    Cancel,
}

impl PlanAnswer {
    /// One of [`PLAN_OPTIONS`], `send_back` optionally followed by `:` and
    /// the person's reason; anything else is a person's to read.
    pub fn parse(answer: &str) -> Option<Self> {
        match answer.trim() {
            "ready" => Some(Self::Ready),
            "cancel" => Some(Self::Cancel),
            answer => super::send_back_reason(answer).map(Self::SendBack),
        }
    }
}

/// A proposal waiting for plan review, as the supervisor orders them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanReviewCandidate {
    pub proposal_id: ProposalId,
    pub submitted_at: String,
    /// One of its submitted tasks has the `interrupt` priority.
    pub interrupt: bool,
}

/// The proposal plan review takes next (ADR-0041 decision 15): one with an
/// `interrupt` task first, then the oldest submission, then the lowest ID.
pub fn next_to_review(candidates: &[PlanReviewCandidate]) -> Option<ProposalId> {
    candidates
        .iter()
        .min_by(|a, b| {
            (!a.interrupt, &a.submitted_at, a.proposal_id).cmp(&(
                !b.interrupt,
                &b.submitted_at,
                b.proposal_id,
            ))
        })
        .map(|candidate| candidate.proposal_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verdict_parses_with_its_actions_reopen_and_precedents() {
        let verdict = PlanReviewVerdict::parse(
            r#"Here it is: {"verdict": "pass", "reasons": [], "summary": "fine",
               "actions": [{"action": "add_dependency", "task_id": 3, "depends_on": 1},
                           {"action": "lower_priority", "task_id": 3, "priority": "low"},
                           {"action": "cancel_duplicate", "task_id": 4, "duplicate_of": 2}],
               "reopen": [{"task_id": 9, "reason": "clashes"}], "precedents": [30]}"#,
        )
        .unwrap();
        assert_eq!(verdict.verdict, PlanReviewDecision::Pass);
        assert_eq!(
            verdict.actions,
            [
                PlanReviewAction::AddDependency {
                    task_id: TaskId::new(3),
                    depends_on: TaskId::new(1)
                },
                PlanReviewAction::LowerPriority {
                    task_id: TaskId::new(3),
                    priority: Priority::Low
                },
                PlanReviewAction::CancelDuplicate {
                    task_id: TaskId::new(4),
                    duplicate_of: TaskId::new(2)
                },
            ]
        );
        assert_eq!(
            verdict
                .actions
                .iter()
                .map(PlanReviewAction::task_id)
                .collect::<Vec<_>>(),
            [TaskId::new(3), TaskId::new(3), TaskId::new(4)]
        );
        assert_eq!(verdict.reopen[0].task_id, TaskId::new(9));
        assert_eq!(verdict.precedents, [AskId::new(30)]);
        let bare =
            PlanReviewVerdict::parse(r#"{"verdict":"revise","reasons":["x"],"summary":"s"}"#)
                .unwrap();
        assert!(bare.actions.is_empty() && bare.reopen.is_empty() && bare.precedents.is_empty());
        assert_eq!(bare.predictions, None);
        // Predictions of any shape leave the verdict whole.
        let odd = PlanReviewVerdict::parse(
            r#"{"verdict":"pass","reasons":[],"summary":"s","predictions":"soon"}"#,
        )
        .unwrap();
        assert_eq!(odd.predictions, Some(serde_json::json!("soon")));
        for broken in [
            "no json",
            r#"{"verdict":"maybe","reasons":[],"summary":""}"#,
            r#"{"verdict":"pass","reasons":[],"summary":"","extra":1}"#,
            r#"{"verdict":"pass","reasons":[],"summary":"","actions":[{"action":"rewrite","task_id":1}]}"#,
        ] {
            let error = PlanReviewVerdict::parse(broken).unwrap_err();
            assert!(error.starts_with("the plan review printed no verdict JSON"));
        }
    }

    #[test]
    fn a_verdict_with_an_unknown_field_or_action_is_refused_but_its_predictions_are_raw() {
        for stdout in [
            r#"{"verdict":"pass","reasons":[],"summary":"ok","ready":true}"#,
            r#"{"verdict":"pass","reasons":[],"summary":"ok","actions":[{"action":"land","task_id":1}]}"#,
            r#"{"verdict":"pass","reasons":[],"summary":"ok","reopen":[{"task_id":1,"reason":"x","now":true}]}"#,
            r#"{"verdict":"pass","reasons":[],"summary":"ok","actions":[{"action":"lower_priority","task_id":1,"priority":"low","why":"x"}]}"#,
            "no json",
        ] {
            assert!(PlanReviewVerdict::parse(stdout).is_err(), "{stdout}");
        }
        // Predictions of any shape leave the verdict readable: they are
        // checked apart from it (ADR-0079 decision 2).
        let verdict = PlanReviewVerdict::parse(
            r#"{"verdict":"pass","reasons":[],"summary":"ok","predictions":"S"}"#,
        )
        .unwrap();
        assert_eq!(verdict.verdict, PlanReviewDecision::Pass);
        assert_eq!(verdict.predictions, Some(serde_json::json!("S")));
    }

    #[test]
    fn a_sure_concern_is_applied_and_the_rest_wait_for_a_person() {
        let concern = |extra: &str| {
            PlanReviewVerdict::parse(&format!(
                r#"{{"verdict":"concern","reasons":["x"],"summary":"s"{extra}}}"#
            ))
            .unwrap()
        };
        let decide = |extra: &str, revises: u32| {
            let decided = concern(extra).decide_concern(revises).unwrap();
            (decided.applied, decided.escalated_because)
        };
        use PlanConcernEscalation as E;
        use PlanReviewDecision as D;
        let sure = r#","confidence":"high","reason_category":null"#;
        assert_eq!(
            decide(&format!(r#","recommendation":"ready"{sure}"#), 0),
            (Some(D::Pass), None)
        );
        assert_eq!(
            decide(&format!(r#","recommendation":"send_back"{sure}"#), 1),
            (Some(D::Revise), None)
        );
        assert_eq!(
            decide(
                &format!(r#","recommendation":"send_back"{sure}"#),
                MAX_PLAN_REVISES
            ),
            (None, Some(E::ReviseLimit))
        );
        // The shape before ADR-t451-1, or half of it.
        assert_eq!(decide("", 0), (None, Some(E::NoRecommendation)));
        assert_eq!(
            decide(r#","recommendation":"ready""#, 0),
            (None, Some(E::NoRecommendation))
        );
        assert_eq!(
            decide(r#","confidence":"high""#, 0),
            (None, Some(E::NoRecommendation))
        );
        assert_eq!(
            decide(r#","recommendation":"ready","confidence":"low""#, 0),
            (None, Some(E::LowConfidence))
        );
        assert_eq!(
            decide(
                r#","recommendation":"ready","confidence":"high","reason_category":"scope""#,
                0
            ),
            (None, Some(E::Scope))
        );
        assert_eq!(
            decide(
                r#","recommendation":"send_back","confidence":"high","reason_category":"discard""#,
                0
            ),
            (None, Some(E::Discard))
        );
        let cancel = concern(r#","recommendation":"cancel","confidence":"high""#)
            .decide_concern(0)
            .unwrap();
        assert_eq!(cancel.escalated_because, Some(E::Discard));
        assert_eq!(cancel.ask_reason(), AskReason::Discard);
        let low = concern(r#","recommendation":"ready","confidence":"low""#)
            .decide_concern(0)
            .unwrap();
        assert_eq!(low.ask_reason(), AskReason::Scope);
        assert_eq!(low.recommendation, Some(PlanRecommendation::Ready));
        assert_eq!(low.confidence, Some(AskConfidence::Low));
        let half = concern(r#","reason_category":"discard""#)
            .decide_concern(0)
            .unwrap();
        assert_eq!(half.escalated_because, Some(E::NoRecommendation));
        assert_eq!(half.ask_reason(), AskReason::Discard);
        // Only a concern is decided; an unknown value is refused.
        assert_eq!(
            PlanReviewVerdict::parse(
                r#"{"verdict":"pass","reasons":[],"summary":"s","recommendation":"ready","confidence":"high"}"#
            )
            .unwrap()
            .decide_concern(0),
            None
        );
        assert!(
            PlanReviewVerdict::parse(
                r#"{"verdict":"concern","reasons":[],"summary":"s","recommendation":"land"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn an_answer_is_one_of_the_options_and_send_back_may_carry_a_reason() {
        assert_eq!(PlanAnswer::parse(" ready "), Some(PlanAnswer::Ready));
        assert_eq!(PlanAnswer::parse("cancel"), Some(PlanAnswer::Cancel));
        assert_eq!(
            PlanAnswer::parse("send_back"),
            Some(PlanAnswer::SendBack(None))
        );
        assert_eq!(
            PlanAnswer::parse("send_back: split it in two"),
            Some(PlanAnswer::SendBack(Some("split it in two".into())))
        );
        assert_eq!(
            PlanAnswer::parse("send_back:  "),
            Some(PlanAnswer::SendBack(None))
        );
        for other in [
            "",
            "land",
            "send_backwards",
            "ready now",
            "send_back reason",
        ] {
            assert_eq!(PlanAnswer::parse(other), None, "{other}");
        }
    }

    #[test]
    fn interrupt_proposals_come_first_then_the_oldest_submission() {
        let candidate = |id: i64, at: &str, interrupt: bool| PlanReviewCandidate {
            proposal_id: ProposalId::new(id),
            submitted_at: at.into(),
            interrupt,
        };
        assert_eq!(next_to_review(&[]), None);
        let plain = [candidate(3, "t2", false), candidate(2, "t1", false)];
        assert_eq!(next_to_review(&plain), Some(ProposalId::new(2)));
        let urgent = [
            candidate(1, "t0", false),
            candidate(5, "t9", true),
            candidate(4, "t9", true),
        ];
        assert_eq!(next_to_review(&urgent), Some(ProposalId::new(4)));
    }
}
