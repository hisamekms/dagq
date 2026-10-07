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

/// What the runtime applies of a verdict (ADR-0041 decision 11, ADR-t451-1
/// decision 4): its decision, why it is not the verdict's own when it is
/// not, and what became of a `concern`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanVerdictDecision {
    pub decision: PlanReviewDecision,
    /// Why a `revise` became a `concern`: the proposal was sent back
    /// [`MAX_PLAN_REVISES`] times already.
    pub overridden: Option<String>,
    pub concern: Option<PlanConcernDecision>,
}

/// What the runtime makes of `verdict` on `proposal`, sent back
/// `revise_count` times when its job started: a `revise` past
/// [`MAX_PLAN_REVISES`] is a `concern`, a sure `concern` is applied as its
/// recommendation ([`PlanReviewVerdict::decide_concern`]), and the rest is
/// the verdict's own.
pub fn decide_verdict(
    proposal: ProposalId,
    verdict: &PlanReviewVerdict,
    revise_count: u32,
) -> PlanVerdictDecision {
    let concern = verdict.decide_concern(revise_count);
    let (decision, overridden) = match verdict.verdict {
        PlanReviewDecision::Revise if revise_count >= MAX_PLAN_REVISES => (
            PlanReviewDecision::Concern,
            Some(format!(
                "proposal {proposal} was sent back {revise_count} times already (at most {MAX_PLAN_REVISES})"
            )),
        ),
        PlanReviewDecision::Concern => match concern.and_then(|decided| decided.applied) {
            Some(applied) => (applied, None),
            None => (PlanReviewDecision::Concern, None),
        },
        decision => (decision, None),
    };
    PlanVerdictDecision {
        decision,
        overridden,
        concern,
    }
}

/// Check a verdict against the proposal it is applied to before anything
/// is: on a `pass`, a task canceled as a duplicate is no original of
/// another; whatever the decision, a task to reopen is not one of the
/// proposal's `members`.
pub fn check_verdict(
    members: &[TaskId],
    decision: PlanReviewDecision,
    verdict: &PlanReviewVerdict,
) -> Result<(), String> {
    if decision == PlanReviewDecision::Pass {
        let canceled: Vec<TaskId> = verdict
            .actions
            .iter()
            .filter(|a| matches!(a, PlanReviewAction::CancelDuplicate { .. }))
            .map(PlanReviewAction::task_id)
            .collect();
        for action in &verdict.actions {
            if let PlanReviewAction::CancelDuplicate { duplicate_of, .. } = action
                && canceled.contains(duplicate_of)
            {
                return Err(format!(
                    "task {duplicate_of} is canceled as a duplicate itself; it is no original"
                ));
            }
        }
    }
    match verdict
        .reopen
        .iter()
        .find(|reopen| members.contains(&reopen.task_id))
    {
        Some(reopen) => Err(format!(
            "task {} is in the proposal under review, not a ready task to reopen",
            reopen.task_id
        )),
        None => Ok(()),
    }
}

/// Check one action of a `pass` against the proposal's `members` and its
/// task (`target`: its status and priority, read only for a member): it
/// changes a submitted task of the proposal, a dependency names another
/// task, and a priority only goes down. Whether the other task of a
/// dependency or a duplicate holds is the store's to read.
pub fn check_action(
    members: &[TaskId],
    action: &PlanReviewAction,
    target: Option<(super::TaskStatus, Priority)>,
) -> Result<(), String> {
    let task_id = action.task_id();
    if !members.contains(&task_id) {
        return Err(format!(
            "plan review may change only the tasks of the proposal, not task {task_id}"
        ));
    }
    let Some((status, priority)) = target else {
        return Err(format!("task {task_id} does not exist"));
    };
    if status != super::TaskStatus::Submitted {
        return Err(format!(
            "task {task_id} is {}, not submitted",
            status.as_str()
        ));
    }
    match action {
        PlanReviewAction::AddDependency { depends_on, .. } => {
            super::task::check_not_self(task_id, *depends_on).map_err(|error| error.to_string())
        }
        PlanReviewAction::LowerPriority { priority: to, .. } if *to >= priority => Err(format!(
            "plan review may only lower the priority of task {task_id} ({}), not set it to {}",
            priority.as_str(),
            to.as_str()
        )),
        PlanReviewAction::LowerPriority { .. } | PlanReviewAction::CancelDuplicate { .. } => Ok(()),
    }
}

/// Why the ready task `task_id` is not reopened (ADR-0041 decision 14),
/// `None` when it is: `found` is its status and the active proposal it is
/// in, `None` when it does not exist.
pub fn reopen_refusal(
    task_id: TaskId,
    found: Option<(super::TaskStatus, Option<ProposalId>)>,
) -> Option<String> {
    let Some((status, active)) = found else {
        return Some(format!("task {task_id} does not exist"));
    };
    if status != super::TaskStatus::Ready {
        return Some(format!(
            "task {task_id} is {}, not ready: it is not changed now",
            status.as_str()
        ));
    }
    active.map(|proposal| {
        format!(
            "task {task_id} is in proposal {proposal}, still under plan review or revise: it is not moved"
        )
    })
}

/// The revise reason a reopened task's own proposal waits for a planner
/// with.
pub fn reopen_reason(reviewed: ProposalId, task_id: TaskId, reason: &str) -> String {
    format!(
        "plan review of proposal {reviewed} found that ready task {task_id} has to change: {reason}"
    )
}

/// The tasks of the proposal edited while its job ran (ADR-0041 decision
/// 9), once each, in order: those of `edits` (each `task_edited` of a task
/// of the proposal, by event id) after the job's `plan_review_started`
/// (`started`, none when it is not recorded).
pub fn edited_during(started: Option<i64>, edits: &[(i64, TaskId)]) -> Vec<TaskId> {
    let Some(started) = started else {
        return Vec::new();
    };
    let mut edited: Vec<TaskId> = edits
        .iter()
        .filter(|(event, _)| *event > started)
        .map(|(_, task)| *task)
        .collect();
    edited.sort();
    edited.dedup();
    edited
}

/// The error a job interrupted by edits of its tasks closes with.
pub fn edited_error(edited: &[TaskId]) -> String {
    format!(
        "task {} of the proposal was edited during its review",
        edited
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// How the end of a plan review job is taken, by what became of its
/// proposal while it ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanReviewEnd {
    /// The proposal is no longer the submitted, unheld one the job took:
    /// the job's row is interrupted with `error`, and nothing else changes.
    MovedOn { error: String },
    /// Tasks of the proposal were edited during the job: its row is
    /// interrupted with `error` and `plan_review_discarded` says why; the
    /// proposal stays submitted and unheld, so the next pass reviews it
    /// again instead of applying a verdict on the old contents or holding
    /// it for a person.
    Discarded { edited: Vec<TaskId>, error: String },
    /// The verdict is applied, or the failure recorded.
    Ends,
}

/// How the end of a job is taken: `reviewable` is whether its proposal is
/// still the one it took, `edited` its tasks edited during it
/// ([`edited_during`]), `failure` the job's error when it failed.
pub fn plan_review_end(
    reviewable: bool,
    edited: Vec<TaskId>,
    failure: Option<&str>,
) -> PlanReviewEnd {
    if !reviewable {
        return PlanReviewEnd::MovedOn {
            error: failure
                .unwrap_or("the proposal moved on during its review")
                .to_owned(),
        };
    }
    if edited.is_empty() {
        return PlanReviewEnd::Ends;
    }
    let error = match failure {
        Some(failure) => format!("{failure}; {}", edited_error(&edited)),
        None => edited_error(&edited),
    };
    PlanReviewEnd::Discarded { edited, error }
}

/// How a failed job's row ends and whether its proposal is held for a
/// person (`plan_review_failed`): a provider that could not be used
/// (`unusable`) leaves it interrupted and unheld, to be reviewed again at
/// once on the other provider (ADR-t1063-1 decision 4); any other failure
/// holds it.
pub fn failed_end(unusable: bool) -> (PlanReviewOutcome, Option<ReviewHold>) {
    if unusable {
        (PlanReviewOutcome::Interrupted, None)
    } else {
        (PlanReviewOutcome::Failed, Some(ReviewHold::Failed))
    }
}

/// Whether an answer `text` to an ask of `kind` is one the supervisor
/// applies: one of [`PLAN_OPTIONS`] to an `approve_plan` ask, while the
/// proposal of the ask's task (`proposal`: its status and hold, `None`
/// without one) is still submitted and held for this concern.
pub fn plan_answer_applies(
    kind: &super::AskKind,
    text: &str,
    proposal: Option<(super::ProposalStatus, Option<ReviewHold>)>,
) -> bool {
    *kind == super::AskKind::ApprovePlan
        && PlanAnswer::parse(text).is_some()
        && proposal == Some((super::ProposalStatus::Submitted, Some(ReviewHold::Concern)))
}

/// The revise reason a person's `send_back` answer to ask `ask` puts
/// first, with the person's `reason` when they gave one.
pub fn person_send_back_reason(ask: AskId, reason: Option<&str>) -> String {
    match reason {
        Some(reason) => format!("a person sent the proposal back in ask {ask}: {reason}"),
        None => {
            format!("a person sent the proposal back in ask {ask} for plan review's findings")
        }
    }
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

    fn verdict(text: &str) -> PlanReviewVerdict {
        PlanReviewVerdict::parse(text).unwrap()
    }

    /// A revise past the limit is a concern saying why; a sure concern is
    /// applied as its recommendation, counted toward the limit like any
    /// revise; the rest is the verdict's own (moved from the integration
    /// tests a_revise_past_the_limit_is_a_concern,
    /// a_sure_send_back_is_a_revise_counted_toward_the_limit_then_a_person_decides
    /// and a_sure_ready_with_an_action_that_does_not_hold_fails_as_a_pass_would).
    #[test]
    fn the_runtime_decides_a_verdict_by_the_revise_limit_and_the_concerns_confidence() {
        use PlanReviewDecision as D;
        let proposal = ProposalId::new(1);
        let revise = verdict(r#"{"verdict":"revise","reasons":["still vague"],"summary":"vague"}"#);
        for count in 0..MAX_PLAN_REVISES {
            let decided = decide_verdict(proposal, &revise, count);
            assert_eq!((decided.decision, decided.overridden), (D::Revise, None));
            assert_eq!(decided.concern, None);
        }
        let past = decide_verdict(proposal, &revise, MAX_PLAN_REVISES);
        assert_eq!(past.decision, D::Concern);
        assert_eq!(
            past.overridden.as_deref(),
            Some("proposal 1 was sent back 2 times already (at most 2)")
        );
        assert_eq!(past.concern, None);
        let pass = verdict(r#"{"verdict":"pass","reasons":[],"summary":"ok"}"#);
        assert_eq!(
            decide_verdict(proposal, &pass, MAX_PLAN_REVISES).decision,
            D::Pass
        );
        // A sure send_back is a revise until the limit, then a person's.
        let send_back = verdict(
            r#"{"verdict":"concern","reasons":["looks already implemented"],"summary":"s",
                "recommendation":"send_back","confidence":"high","reason_category":null}"#,
        );
        for count in 0..MAX_PLAN_REVISES {
            let decided = decide_verdict(proposal, &send_back, count);
            assert_eq!(decided.decision, D::Revise, "{count}");
            assert_eq!(decided.concern.unwrap().applied, Some(D::Revise));
        }
        let limited = decide_verdict(proposal, &send_back, MAX_PLAN_REVISES);
        assert_eq!(limited.decision, D::Concern);
        assert_eq!(limited.overridden, None);
        assert_eq!(
            limited.concern.unwrap().escalated_because,
            Some(PlanConcernEscalation::ReviseLimit)
        );
        // A sure ready is a pass, whose actions are checked as a pass's.
        let ready = verdict(
            r#"{"verdict":"concern","reasons":["x"],"summary":"s","recommendation":"ready",
                "confidence":"high","actions":[{"action":"cancel_duplicate","task_id":2,"duplicate_of":2}]}"#,
        );
        let decided = decide_verdict(proposal, &ready, 0);
        assert_eq!(decided.decision, D::Pass);
        assert_eq!(decided.concern.unwrap().applied, Some(D::Pass));
        // The old shape and a low confidence stay a concern.
        for text in [
            r#"{"verdict":"concern","reasons":["x"],"summary":"s"}"#,
            r#"{"verdict":"concern","reasons":["x"],"summary":"s","recommendation":"ready","confidence":"low"}"#,
        ] {
            let decided = decide_verdict(proposal, &verdict(text), 0);
            assert_eq!(decided.decision, D::Concern, "{text}");
            assert_eq!(decided.concern.unwrap().applied, None, "{text}");
        }
    }

    /// A pass's actions are checked against the proposal before any is
    /// applied (moved from the integration tests
    /// a_failed_plan_review_waits_for_a_person_and_is_not_retried and
    /// a_ready_task_the_review_reopens_leaves_the_claim_for_a_planner).
    #[test]
    fn a_pass_changes_only_submitted_tasks_of_the_proposal_and_only_lowers_priorities() {
        use super::super::TaskStatus as S;
        let members = [TaskId::new(2), TaskId::new(3)];
        let submitted = Some((S::Submitted, Priority::Normal));
        let lower = |task: i64, to: Priority| PlanReviewAction::LowerPriority {
            task_id: TaskId::new(task),
            priority: to,
        };
        assert_eq!(
            check_action(&members, &lower(2, Priority::Low), submitted),
            Ok(())
        );
        assert_eq!(
            check_action(&members, &lower(2, Priority::Urgent), submitted),
            Err(
                "plan review may only lower the priority of task 2 (normal), not set it to urgent"
                    .into()
            )
        );
        assert_eq!(
            check_action(&members, &lower(2, Priority::Normal), submitted),
            Err(
                "plan review may only lower the priority of task 2 (normal), not set it to normal"
                    .into()
            )
        );
        assert_eq!(
            check_action(&members, &lower(9, Priority::Low), None),
            Err("plan review may change only the tasks of the proposal, not task 9".into())
        );
        assert_eq!(
            check_action(
                &members,
                &lower(3, Priority::Low),
                Some((S::Ready, Priority::Normal))
            ),
            Err("task 3 is ready, not submitted".into())
        );
        assert_eq!(
            check_action(&members, &lower(3, Priority::Low), None),
            Err("task 3 does not exist".into())
        );
        let depend = |on: i64| PlanReviewAction::AddDependency {
            task_id: TaskId::new(3),
            depends_on: TaskId::new(on),
        };
        assert_eq!(check_action(&members, &depend(2), submitted), Ok(()));
        assert_eq!(
            check_action(&members, &depend(3), submitted),
            Err("a task cannot depend on itself".into())
        );
        let cancel = |task: i64, of: i64| PlanReviewAction::CancelDuplicate {
            task_id: TaskId::new(task),
            duplicate_of: TaskId::new(of),
        };
        assert_eq!(check_action(&members, &cancel(3, 1), submitted), Ok(()));

        // A task canceled as a duplicate is no original; a reopen is not of
        // the proposal's own tasks.
        let mut pass = verdict(r#"{"verdict":"pass","reasons":[],"summary":"ok"}"#);
        pass.actions = vec![cancel(3, 1), cancel(2, 3)];
        assert_eq!(
            check_verdict(&members, PlanReviewDecision::Pass, &pass),
            Err("task 3 is canceled as a duplicate itself; it is no original".into())
        );
        // Actions are applied on a pass only.
        assert_eq!(
            check_verdict(&members, PlanReviewDecision::Revise, &pass),
            Ok(())
        );
        pass.actions = vec![cancel(3, 1)];
        assert_eq!(
            check_verdict(&members, PlanReviewDecision::Pass, &pass),
            Ok(())
        );
        pass.reopen = vec![Reopen {
            task_id: TaskId::new(2),
            reason: "x".into(),
        }];
        for decision in [PlanReviewDecision::Pass, PlanReviewDecision::Concern] {
            assert_eq!(
                check_verdict(&members, decision, &pass),
                Err("task 2 is in the proposal under review, not a ready task to reopen".into())
            );
        }
        pass.reopen[0].task_id = TaskId::new(7);
        assert_eq!(
            check_verdict(&members, PlanReviewDecision::Pass, &pass),
            Ok(())
        );
    }

    /// Only a ready task outside an active proposal is reopened; the rest
    /// is skipped with why.
    #[test]
    fn a_reopen_takes_only_a_ready_task_no_active_proposal_holds() {
        use super::super::TaskStatus as S;
        let task = TaskId::new(4);
        assert_eq!(reopen_refusal(task, Some((S::Ready, None))), None);
        assert_eq!(
            reopen_refusal(task, None).as_deref(),
            Some("task 4 does not exist")
        );
        assert_eq!(
            reopen_refusal(task, Some((S::Draft, None))).as_deref(),
            Some("task 4 is draft, not ready: it is not changed now")
        );
        assert_eq!(
            reopen_refusal(task, Some((S::Ready, Some(ProposalId::new(2))))).as_deref(),
            Some("task 4 is in proposal 2, still under plan review or revise: it is not moved")
        );
        assert_eq!(
            reopen_reason(ProposalId::new(1), task, "it must use the new API"),
            "plan review of proposal 1 found that ready task 4 has to change: it must use the new API"
        );
    }

    /// Edits of the proposal's tasks after the job started discard its
    /// end, a verdict's or a failure's, and the proposal is reviewed again
    /// unheld; edits before it leave the end applied, and a failure then
    /// holds the proposal unless its provider could not be used (moved
    /// from the integration tests
    /// a_job_failing_after_an_edit_of_its_task_is_not_held_and_the_review_runs_again,
    /// a_job_failing_after_edits_before_it_or_to_another_proposal_is_held
    /// and edits_before_a_review_or_to_another_proposal_leave_its_verdict_applied).
    #[test]
    fn edits_during_a_job_discard_its_end_and_a_failure_otherwise_holds_the_proposal() {
        let (two, three) = (TaskId::new(2), TaskId::new(3));
        assert_eq!(edited_during(Some(10), &[(4, two), (9, three)]), []);
        assert_eq!(
            edited_during(Some(10), &[(4, two), (12, three), (11, three), (15, two)]),
            [two, three]
        );
        assert_eq!(edited_during(None, &[(12, three)]), []);
        assert_eq!(
            edited_error(&[two, three]),
            "task 2, 3 of the proposal was edited during its review"
        );
        assert_eq!(plan_review_end(true, Vec::new(), None), PlanReviewEnd::Ends);
        assert_eq!(
            plan_review_end(true, Vec::new(), Some("model unavailable")),
            PlanReviewEnd::Ends
        );
        assert_eq!(
            plan_review_end(true, vec![three], None),
            PlanReviewEnd::Discarded {
                edited: vec![three],
                error: "task 3 of the proposal was edited during its review".into(),
            }
        );
        assert_eq!(
            plan_review_end(true, vec![three], Some("model unavailable")),
            PlanReviewEnd::Discarded {
                edited: vec![three],
                error: "model unavailable; task 3 of the proposal was edited during its review"
                    .into(),
            }
        );
        assert_eq!(
            plan_review_end(false, Vec::new(), None),
            PlanReviewEnd::MovedOn {
                error: "the proposal moved on during its review".into()
            }
        );
        assert_eq!(
            plan_review_end(false, Vec::new(), Some("model unavailable")),
            PlanReviewEnd::MovedOn {
                error: "model unavailable".into()
            }
        );
        assert_eq!(
            failed_end(false),
            (PlanReviewOutcome::Failed, Some(ReviewHold::Failed))
        );
        assert_eq!(failed_end(true), (PlanReviewOutcome::Interrupted, None));
    }

    /// An answer applies while its proposal is submitted and held for the
    /// concern; a withdrawn or released one is closed unapplied (moved from
    /// the integration test an_answered_plan_ask_closed_unapplied_records_ask_closed).
    #[test]
    fn an_answer_applies_only_while_its_proposal_waits_for_it() {
        use super::super::{AskKind, ProposalStatus as P};
        let held = Some((P::Submitted, Some(ReviewHold::Concern)));
        for answer in ["ready", "cancel", "send_back: split it"] {
            assert!(
                plan_answer_applies(&AskKind::ApprovePlan, answer, held),
                "{answer}"
            );
        }
        assert!(!plan_answer_applies(&AskKind::ApprovePlan, "maybe", held));
        assert!(!plan_answer_applies(&AskKind::Blocked, "ready", held));
        for proposal in [
            None,
            Some((P::Submitted, None)),
            Some((P::Submitted, Some(ReviewHold::Failed))),
            Some((P::Revising, None)),
            Some((P::Canceled, Some(ReviewHold::Concern))),
        ] {
            assert!(
                !plan_answer_applies(&AskKind::ApprovePlan, "ready", proposal),
                "{proposal:?}"
            );
        }
        assert_eq!(
            person_send_back_reason(AskId::new(7), Some("split the parser out first")),
            "a person sent the proposal back in ask 7: split the parser out first"
        );
        assert_eq!(
            person_send_back_reason(AskId::new(7), None),
            "a person sent the proposal back in ask 7 for plan review's findings"
        );
    }
}
