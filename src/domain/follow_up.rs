//! Drafts the runtime opens a planner for (ADR-0041 decision 16, which
//! replaces the follow-up triage job of ADR-0037): where such a draft came
//! from, how many planners one draft gets, and the runtime's rule on which
//! drafts a planner of the runtime's may not submit without a person.
//!
//! `integrate` registers a draft from each landed receipt's `follow_ups`
//! (origin `follow_up`); a job that judges a goal may register one for a gap
//! it found (origin `goal_gap`). A ready task plan review reopened into a
//! proposal of its own (ADR-0044 decision 14) that returns to `draft` when
//! that proposal is withdrawn gets origin `reopened` (task 418). A draft a
//! person registers with `add` has no origin, and no planner is opened for
//! it.

use serde::{Deserialize, Serialize};

use super::DomainError;

// Where a draft the runtime or a job registered came from.
string_enum!(DraftOrigin {
    FollowUp => "follow_up",
    GoalGap => "goal_gap",
    Reopened => "reopened",
});

/// The material of a `reopened` draft: why plan review reopened the task
/// (the verdict's reopen reason), the proposal it was reopened into and
/// that was withdrawn, and the proposal whose plan review reopened it.
pub fn reopened_material(
    reason: &str,
    proposal_id: super::ProposalId,
    reviewed_proposal_id: Option<super::ProposalId>,
) -> serde_json::Value {
    serde_json::json!({
        "reason": reason,
        "proposal_id": proposal_id,
        "reviewed_proposal_id": reviewed_proposal_id,
    })
}

/// The options of the `planner_question` ask a planner of the runtime's
/// opens about its draft when it cannot decide (ADR-0041 decision 16). The
/// answer is typed into that planner's workspace and the planner applies
/// it; `keep_draft` leaves the draft for a person's planner, and no planner
/// of the runtime's is opened for it again.
pub const PLANNER_QUESTION_OPTIONS: &[&str] = &["adopt", "cancel", "keep_draft"];

/// Planners the runtime opens for one draft that none of them decided
/// (its session ended with the draft still waiting); the next time the
/// draft is recorded as exhausted instead (`draft_planner_exhausted`, the
/// inbox's attention).
pub const MAX_DRAFT_PLANNERS: usize = 3;

/// A follow-up this many steps from a person's judgement is not submitted
/// by a planner of the runtime's without one (ADR-0037 decision 6, kept by
/// ADR-0041 decision 16; raised from 2 to 3 by ADR-t808-1).
pub const FOLLOW_UP_ASK_DEPTH: i64 = 3;

/// The categories a worker gives each entry of its receipt's `follow_ups`
/// (`category`, ADR-t947-3), with what each means; the design's list
/// (receipt-and-session-exit) is the one of record. The runtime records a
/// value it does not know as it is, and never rejects a receipt for one.
pub const FOLLOW_UP_CATEGORIES: &[(&str, &str)] = &[
    (
        "defect",
        "the runtime (or a script) does not do what is decided",
    ),
    (
        "flaky_test",
        "an existing test fails or times out now and then (name it as <module>::<name>)",
    ),
    (
        "test_gap",
        "a path has no test or a weak one, not failing now (name the test)",
    ),
    (
        "docs_drift",
        "a document (design, the ADR index, a skill, AGENTS.md) disagrees with the code or an accepted ADR",
    ),
    (
        "remaining_scope",
        "what an accepted ADR or the goal decided that this task did not do",
    ),
    (
        "improvement",
        "not a defect but better: observation, stats, refactoring, speed, ease of use",
    ),
    (
        "measurement",
        "count and check something after a landing or a period",
    ),
    (
        "decision",
        "a question a person or a planner has to decide; the work is not settled",
    ),
    (
        "ops",
        "work a person or the inbox does on the host, the queue or a service, not a change to the repository",
    ),
    ("other", "none of these; say what it is in the description"),
];

/// The category of a follow_up entry that names none (or a blank or
/// non-text one), and of a draft registered before categories were kept.
pub const UNLABELED_CATEGORY: &str = "unlabeled";

/// The category of a receipt's follow_up `entry` as the runtime records
/// it: its trimmed `category`, one of [`FOLLOW_UP_CATEGORIES`] or not, or
/// [`UNLABELED_CATEGORY`] without a non-blank text one.
pub fn follow_up_category(entry: &serde_json::Value) -> String {
    entry
        .get("category")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|category| !category.is_empty())
        .unwrap_or(UNLABELED_CATEGORY)
        .to_owned()
}

/// Where a follow_up draft stands when a planner of the runtime's submits
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FollowUpFacts {
    /// The draft belongs to a goal that is not closed.
    pub goal_open: bool,
    /// The draft's `follow_up_depth`.
    pub depth: i64,
}

/// Why a planner of the runtime's may not submit a follow_up draft without
/// a person's `adopt` answer (ADR-0041 decision 16), or `None`: the draft's
/// goal is closed or missing, or the draft is [`FOLLOW_UP_ASK_DEPTH`] or
/// more follow-ups from a person.
pub fn adopt_needs_person(facts: FollowUpFacts) -> Option<String> {
    if !facts.goal_open {
        Some("its goal is closed (or it has none), so a person decides whether it belongs to a new goal".to_owned())
    } else if facts.depth >= FOLLOW_UP_ASK_DEPTH {
        Some(format!(
            "it is a follow-up {} steps from a person's judgement (at most {} is submitted without one)",
            facts.depth,
            FOLLOW_UP_ASK_DEPTH - 1
        ))
    } else {
        None
    }
}

/// A draft the runtime opens a planner for: the task, where it came from
/// and the material its origin recorded, and how many planners the runtime
/// opened for it already.
#[derive(Debug, Clone, Serialize)]
pub struct DraftTarget {
    pub task: super::Task,
    pub origin: DraftOrigin,
    pub material: serde_json::Value,
    pub planners: usize,
}

impl DraftTarget {
    /// The bundle the draft is planned in (ADR-t807-1).
    pub fn bundle_key(&self) -> BundleKey {
        BundleKey::of(self.origin, &self.material, self.task.id())
    }
}

// What makes drafts one bundle (ADR-t807-1): the one piece of work that made
// them together. A draft whose material names none is a bundle of its own
// (`task_id`).
string_enum!(BundleKeyKind {
    SourceRun => "source_run_id",
    GoalReview => "goal_review_id",
    ReviewedProposal => "reviewed_proposal_id",
    Task => "task_id",
});

/// The key of a bundle of drafts (ADR-t807-1): the drafts of one origin
/// that the same run's `integrate` (`follow_up`), the same goal review
/// (`goal_gap`) or the same withdrawal of a proposal plan review reopened
/// tasks into (`reopened`) made. One planner of the runtime's takes the
/// drafts of one key that wait at once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleKey {
    pub kind: BundleKeyKind,
    pub value: String,
}

impl BundleKey {
    /// The key of the draft `task` of `origin` with `material`: its
    /// origin's field, or the task itself when the material has none.
    pub fn of(origin: DraftOrigin, material: &serde_json::Value, task: super::TaskId) -> Self {
        let kind = match origin {
            DraftOrigin::FollowUp => BundleKeyKind::SourceRun,
            DraftOrigin::GoalGap => BundleKeyKind::GoalReview,
            DraftOrigin::Reopened => BundleKeyKind::ReviewedProposal,
        };
        match material.get(kind.as_str()) {
            Some(serde_json::Value::String(value)) if !value.is_empty() => Self {
                kind,
                value: value.clone(),
            },
            Some(value @ serde_json::Value::Number(_)) => Self {
                kind,
                value: value.to_string(),
            },
            _ => Self {
                kind: BundleKeyKind::Task,
                value: task.to_string(),
            },
        }
    }
}

/// The drafts waiting for a planner grouped by their [`BundleKey`], in the
/// order of each bundle's oldest draft (the targets come by ID).
pub fn bundles(targets: Vec<DraftTarget>) -> Vec<Vec<DraftTarget>> {
    let mut bundles: Vec<(BundleKey, Vec<DraftTarget>)> = Vec::new();
    for target in targets {
        let key = target.bundle_key();
        match bundles.iter_mut().find(|(k, _)| *k == key) {
            Some((_, members)) => members.push(target),
            None => bundles.push((key, vec![target])),
        }
    }
    bundles.into_iter().map(|(_, members)| members).collect()
}

// What became of one draft of a bundle when its planner ended
// (ADR-t807-1): submitted into a proposal, canceled (as a duplicate of
// another task, or not), kept as a draft by a person's answer, or left
// undecided (a later planner takes it again).
string_enum!(DraftOutcome {
    Submitted => "submitted",
    Canceled => "canceled",
    Duplicate => "duplicate",
    KeepDraft => "keep_draft",
    Undecided => "undecided",
});

#[cfg(test)]
mod tests {
    use super::*;

    /// A known category, an unknown one kept as it is, and entries without
    /// a text one (ADR-t947-3 decision 3).
    #[test]
    fn a_follow_up_category_is_kept_as_written_or_unlabeled() {
        use serde_json::json;
        let category = |entry| follow_up_category(&entry);
        assert_eq!(category(json!({"category": " defect "})), "defect");
        assert_eq!(category(json!({"category": "typo_fix"})), "typo_fix");
        assert_eq!(category(json!({"title": "t"})), UNLABELED_CATEGORY);
        assert_eq!(category(json!({"category": " "})), UNLABELED_CATEGORY);
        assert_eq!(category(json!({"category": 3})), UNLABELED_CATEGORY);
        assert_eq!(category(json!("text")), UNLABELED_CATEGORY);
        assert!(
            FOLLOW_UP_CATEGORIES
                .iter()
                .any(|(code, _)| *code == "other")
        );
    }

    #[test]
    fn a_follow_up_needs_a_person_for_a_closed_goal_or_depth_three() {
        let open = FollowUpFacts {
            goal_open: true,
            depth: 1,
        };
        assert_eq!(adopt_needs_person(open), None);
        assert!(
            adopt_needs_person(FollowUpFacts {
                goal_open: false,
                ..open
            })
            .unwrap()
            .contains("closed")
        );
        assert_eq!(adopt_needs_person(FollowUpFacts { depth: 2, ..open }), None);
        let why = adopt_needs_person(FollowUpFacts { depth: 3, ..open }).unwrap();
        assert!(why.contains("3 steps"), "{why}");
        assert!(why.contains("at most 2"), "{why}");
    }

    fn target(id: i64, origin: DraftOrigin, material: serde_json::Value) -> DraftTarget {
        let task = crate::domain::Task::new(
            crate::domain::TaskId::new(id),
            crate::domain::NewTask {
                title: format!("t{id}"),
                description: String::new(),
                acceptance: String::new(),
                verification_commands: Vec::new(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
                dependencies: Vec::new(),
                goal_dependencies: Vec::new(),
                priority: Default::default(),
                change: None,
                goal_id: None,
                context: String::new(),
                provider: None,
                worker_mode: None,
            },
            "2026-09-28T00:00:00.000Z".into(),
        )
        .unwrap();
        DraftTarget {
            task,
            origin,
            material,
            planners: 0,
        }
    }

    /// The drafts one run's integrate, one goal review or one withdrawal
    /// made are one bundle each; a draft whose material names none is a
    /// bundle of its own (ADR-t807-1).
    #[test]
    fn drafts_are_bundled_by_what_made_them() {
        use serde_json::json;
        let targets = vec![
            target(1, DraftOrigin::FollowUp, json!({"source_run_id": "r1"})),
            target(2, DraftOrigin::GoalGap, json!({"goal_review_id": 4})),
            target(3, DraftOrigin::FollowUp, json!({"source_run_id": "r2"})),
            target(4, DraftOrigin::FollowUp, json!({"source_run_id": "r1"})),
            target(5, DraftOrigin::FollowUp, json!({"source_run_id": null})),
            target(6, DraftOrigin::FollowUp, json!({})),
            target(7, DraftOrigin::Reopened, json!({"reviewed_proposal_id": 4})),
            target(8, DraftOrigin::GoalGap, json!({"goal_review_id": 4})),
        ];
        let ids: Vec<Vec<i64>> = bundles(targets)
            .iter()
            .map(|b| b.iter().map(|t| t.task.id().as_i64()).collect())
            .collect();
        assert_eq!(
            ids,
            [vec![1, 4], vec![2, 8], vec![3], vec![5], vec![6], vec![7]]
        );
        let key = target(9, DraftOrigin::Reopened, json!({"reviewed_proposal_id": 4})).bundle_key();
        assert_eq!(
            (key.kind.as_str(), key.value.as_str()),
            ("reviewed_proposal_id", "4")
        );
        let key = target(9, DraftOrigin::FollowUp, json!({"source_run_id": ""})).bundle_key();
        assert_eq!((key.kind, key.value.as_str()), (BundleKeyKind::Task, "9"));
        assert_eq!(
            "duplicate".parse::<DraftOutcome>().unwrap(),
            DraftOutcome::Duplicate
        );
    }

    #[test]
    fn origins_parse() {
        assert_eq!(
            "goal_gap".parse::<DraftOrigin>().unwrap(),
            DraftOrigin::GoalGap
        );
        assert_eq!(DraftOrigin::FollowUp.as_str(), "follow_up");
        assert_eq!(
            "reopened".parse::<DraftOrigin>().unwrap(),
            DraftOrigin::Reopened
        );
        assert!("person".parse::<DraftOrigin>().is_err());
    }

    #[test]
    fn a_reopened_draft_keeps_the_reason_and_both_proposals() {
        let material = reopened_material(
            "clashes with task 3",
            crate::domain::ProposalId::new(7),
            Some(crate::domain::ProposalId::new(5)),
        );
        assert_eq!(
            material,
            serde_json::json!({"reason": "clashes with task 3", "proposal_id": 7, "reviewed_proposal_id": 5})
        );
        assert!(
            reopened_material("r", crate::domain::ProposalId::new(7), None)["reviewed_proposal_id"]
                .is_null()
        );
    }
}
