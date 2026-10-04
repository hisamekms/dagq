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
/// it; `keep_draft` leaves the draft as it is until a person has the inbox
/// record a planning request that names it (`request add --ref task:N`,
/// ADR-t1394-1 decision 8), and no planner of the runtime's is opened for it
/// again.
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

/// The field of a receipt's follow_up entry that carries the worker's
/// proposal of how it relates to the source goal's acceptance (ADR-t1504-2
/// decision 11).
pub const MEMBERSHIP_PROPOSAL_FIELD: &str = "membership_proposal";

/// The worker's membership proposal of a receipt's follow_up `entry` as the
/// runtime records it: the value as written, whatever its shape, or null
/// without one. It is a proposal for the planner, never a judgement, and
/// its absence or shape never refuses the receipt (ADR-t947-3 decision 3).
pub fn follow_up_membership_proposal(entry: &serde_json::Value) -> serde_json::Value {
    entry
        .get(MEMBERSHIP_PROPOSAL_FIELD)
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

/// Only a complete recorded/restored registration establishes an open source.
/// Missing or conflicting registration information requires a person.
pub fn source_goal_was_open(material: &serde_json::Value) -> bool {
    material["source_goal_state"].as_str() == Some("open")
        && material["source_goal_id"].as_i64().is_some_and(|id| id > 0)
        && matches!(
            material["source_goal_provenance"].as_str(),
            Some("recorded" | "restored")
        )
}

/// Where a follow_up draft stands when a planner of the runtime's submits
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FollowUpFacts {
    /// The draft belongs to a goal that is not closed.
    pub goal_open: bool,
    /// The immutable registration establishes that the source goal was open.
    pub source_goal_open: bool,
    /// The draft's `follow_up_depth`.
    pub depth: i64,
}

/// Why a planner of the runtime's may not submit a follow_up draft without
/// a person's `adopt` answer (ADR-0041 decision 16), or `None`: the draft's
/// source goal was missing, closed or unknown at registration, its current
/// goal is closed or missing, or the draft is [`FOLLOW_UP_ASK_DEPTH`] or
/// more follow-ups from a person.
pub fn adopt_needs_person(facts: FollowUpFacts) -> Option<String> {
    if !facts.source_goal_open {
        Some("its source goal was missing, closed or unknown at registration, so a person must adopt it".into())
    } else if !facts.goal_open {
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
    fn a_membership_proposal_is_kept_as_written_or_null() {
        use serde_json::json;
        let proposal = |entry| follow_up_membership_proposal(&entry);
        let written =
            json!({"classification": "out_of_scope", "acceptance_items": ["(2)"], "reason": "r"});
        assert_eq!(
            proposal(json!({"membership_proposal": written.clone()})),
            written
        );
        assert_eq!(
            proposal(json!({"membership_proposal": {"classification": "maybe"}})),
            json!({"classification": "maybe"})
        );
        assert_eq!(
            proposal(json!({"membership_proposal": "required"})),
            json!("required")
        );
        assert_eq!(proposal(json!({"title": "t"})), serde_json::Value::Null);
        assert_eq!(proposal(json!("text")), serde_json::Value::Null);
    }

    #[test]
    fn a_follow_up_needs_a_person_for_a_closed_goal_or_depth_three() {
        let open = FollowUpFacts {
            goal_open: true,
            source_goal_open: true,
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

string_enum!(MembershipClassification {
    Required => "required",
    OutOfScope => "out_of_scope",
    Undecided => "undecided",
});

/// The judge supplies evidence; the store snapshots the acceptance version.
#[derive(Debug, Clone)]
pub struct MembershipJudgement {
    pub classification: MembershipClassification,
    pub acceptance_items: Vec<String>,
    pub reason: String,
    pub evidence: Vec<String>,
    pub destination_goal_id: Option<super::GoalId>,
    /// Required when the registration history cannot name the source.
    pub source_goal_id: Option<super::GoalId>,
    pub corrects: Option<i64>,
}

impl MembershipJudgement {
    pub fn validate(
        &self,
        previous: Option<(i64, MembershipClassification)>,
        source: super::GoalId,
    ) -> Result<(), String> {
        use MembershipClassification::*;
        if self.reason.trim().is_empty()
            || self
                .acceptance_items
                .iter()
                .chain(&self.evidence)
                .any(|s| s.trim().is_empty())
        {
            return Err("reason and each supplied item/reference must be non-blank".into());
        }
        if self.classification == Required && self.acceptance_items.is_empty() {
            return Err("required needs at least one acceptance item".into());
        }
        if self.classification != Undecided && self.evidence.is_empty() {
            return Err("required and out_of_scope need evidence".into());
        }
        if self.classification == OutOfScope
            && (self.destination_goal_id.is_none() || self.destination_goal_id == Some(source))
        {
            return Err("out_of_scope needs a destination different from the source goal".into());
        }
        if self.classification == Required && self.destination_goal_id.is_some_and(|g| g != source)
        {
            return Err("required belongs to the source goal".into());
        }
        if let Some((id, class)) = previous {
            if class != Undecided && self.classification == Undecided {
                return Err("a decided judgement cannot return to undecided".into());
            }
            if class != Undecided && class != self.classification && self.corrects != Some(id) {
                return Err("a correction must name the previous judgement with --corrects".into());
            }
        }
        if self.corrects.is_some() && self.corrects != previous.map(|p| p.0) {
            return Err("--corrects must name the latest judgement".into());
        }
        Ok(())
    }
}

/// A follow_up whose source goal is the goal a review or a close looks at
/// (ADR-t1504-2 decision 8), wherever it belongs now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFollowUp {
    pub task: super::TaskId,
    pub status: super::TaskStatus,
    /// Whether it is a task of the source goal now.
    pub in_goal: bool,
    /// Its current judgement: the row's ID, the classification and whether
    /// the source goal's acceptance changed after it (needs a recheck).
    pub judgement: Option<(i64, MembershipClassification, bool)>,
}

impl SourceFollowUp {
    /// Why it keeps its source goal from closing as achieved, or `None`.
    /// A completed or canceled one never does; an out-of-scope one does
    /// not wait for its work; a required one in the goal waits as a task of
    /// the goal (the goal's own rule), outside the goal it waits here.
    pub fn unsettled(&self) -> Option<&'static str> {
        use MembershipClassification::*;
        if matches!(
            self.status,
            super::TaskStatus::Completed | super::TaskStatus::Canceled
        ) {
            return None;
        }
        match self.judgement {
            None => Some("not judged"),
            Some((_, Undecided, _)) => Some("undecided"),
            Some((_, _, true)) => Some("judged before the acceptance changed"),
            Some((_, Required, false)) if !self.in_goal => Some("required but outside the goal"),
            Some(_) => None,
        }
    }

    /// What a goal review saw of it: its ID and current judgement. Its
    /// status is left out, so the work of an out-of-scope one moving on
    /// does not void a review; whether it ended is checked at the close.
    pub fn fingerprint(&self) -> String {
        let judgement = self.judgement.map_or("-".to_owned(), |(id, class, _)| {
            format!("{id}-{}", class.as_str())
        });
        format!("{}:{judgement}", self.task)
    }
}

/// Each follow-up of `follow_ups` that keeps its source goal from closing
/// as achieved, with why, in task order.
pub fn unsettled_follow_ups(follow_ups: &[SourceFollowUp]) -> Vec<(super::TaskId, String)> {
    let mut unsettled: Vec<_> = follow_ups
        .iter()
        .filter_map(|f| f.unsettled().map(|why| (f.task, why.to_owned())))
        .collect();
    unsettled.sort_by_key(|(task, _)| *task);
    unsettled
}

// Why a follow_up draft has no membership judgement plan review can rely
// on (ADR-t1504-2 decision 7): none recorded, the latest is `undecided`,
// or it predates the source goal's current acceptance version.
string_enum!(MembershipGap {
    Missing => "missing",
    Undecided => "undecided",
    NeedsRecheck => "needs_recheck",
});

impl MembershipGap {
    /// What the gap means and what records it, for a refusal or a lint.
    pub fn explain(self) -> &'static str {
        match self {
            Self::Missing => "it has no membership judgement",
            Self::Undecided => "its latest membership judgement is undecided",
            Self::NeedsRecheck => {
                "its membership judgement predates the source goal's current acceptance version (needs recheck)"
            }
        }
    }
}

/// Where a follow_up draft's membership stands for submit and lint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MembershipFacts {
    /// Registration recorded that the follow_up had no source goal
    /// (`source_goal_state: none`): there is no acceptance to judge by.
    pub source_goal_none: bool,
    /// The source goal (registered, restored or named by a judge) closed
    /// as abandoned: it claims no achievement to judge against.
    pub source_goal_abandoned: bool,
    /// The latest judgement's classification and whether it needs a
    /// recheck (its acceptance version is older than the goal's).
    pub latest: Option<(MembershipClassification, bool)>,
}

/// The gap that keeps a follow_up draft from being submitted, or `None`
/// (ADR-t1504-2 decision 7). An unknown source goal is treated as one that
/// exists, so it needs a judgement too.
pub fn membership_gap(facts: MembershipFacts) -> Option<MembershipGap> {
    if facts.source_goal_none || facts.source_goal_abandoned {
        return None;
    }
    match facts.latest {
        None => Some(MembershipGap::Missing),
        Some((MembershipClassification::Undecided, _)) => Some(MembershipGap::Undecided),
        Some((_, true)) => Some(MembershipGap::NeedsRecheck),
        Some((_, false)) => None,
    }
}

/// Whether recording `classification` for a follow-up whose source goal
/// closed with `verdict` opens a `correct_goal` ask (ADR-t1504-2 decision
/// 9): a required judgement after the goal closed as achieved says the
/// goal did not meet its acceptance, which only a person settles. An
/// out-of-scope correction is recorded and moved without one, and an
/// abandoned goal claimed nothing. Recording required again over a
/// `previous` required judgement (a recheck) changes nothing the close
/// knew, so it asks nothing.
pub fn opens_correction(
    classification: MembershipClassification,
    previous: Option<MembershipClassification>,
    verdict: Option<super::GoalVerdict>,
) -> bool {
    classification == MembershipClassification::Required
        && previous != Some(MembershipClassification::Required)
        && verdict == Some(super::GoalVerdict::Achieved)
}

// A person's answer to a `correct_goal` ask (ADR-t1504-2 decision 9),
// which the supervisor applies: open the goal again and move the follow-up
// back into it, keep the goal closed and record that its achieved verdict
// was wrong (the fix goes to a fix goal), or keep it achieved (the
// acceptance was met; the follow-up is out of scope).
string_enum!(CorrectionAnswer {
    Reopen => "reopen",
    CorrectVerdict => "correct_verdict",
    KeepAchieved => "keep_achieved",
});

/// The options of every `correct_goal` ask, in [`CorrectionAnswer`]'s
/// order.
pub const CORRECTION_OPTIONS: &[&str] = &[
    CorrectionAnswer::Reopen.as_str(),
    CorrectionAnswer::CorrectVerdict.as_str(),
    CorrectionAnswer::KeepAchieved.as_str(),
];

/// Who opens the `correct_goal` asks: the runtime, as the judgement is
/// recorded.
pub const CORRECTION_ASKER: &str = "supervisor";

impl CorrectionAnswer {
    /// The option `text` names, or `None` for any other answer (left for
    /// the inbox to read).
    pub fn parse(text: &str) -> Option<Self> {
        text.trim().parse().ok()
    }
}

/// A task that waits on the closed goal (`--goal-dep`) and so was released
/// when it closed as achieved, with its runs (ID and status, oldest first):
/// what a `correct_goal` ask shows a person, since the runtime stops none
/// of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleasedDependent {
    pub task: super::TaskId,
    pub title: String,
    pub status: super::TaskStatus,
    pub runs: Vec<(String, String)>,
}

/// The question of the `correct_goal` ask about follow-up `task` of
/// `goal`, judged required by the row `judgement_id` with `judgement`, and
/// the tasks the goal released.
pub fn correction_question(
    goal: super::GoalId,
    task: super::TaskId,
    judgement_id: i64,
    judgement: &MembershipJudgement,
    released: &[ReleasedDependent],
) -> String {
    let mut question = format!(
        "Goal {goal} was closed as achieved, but its follow-up task {task} is now judged required for its acceptance (judgement {judgement_id}). Was the acceptance met?"
    );
    question.push_str(&format!(
        "\nAcceptance items: {}",
        judgement.acceptance_items.join("; ")
    ));
    question.push_str(&format!("\nReason: {}", judgement.reason.trim()));
    if !judgement.evidence.is_empty() {
        question.push_str(&format!("\nEvidence: {}", judgement.evidence.join(", ")));
    }
    question.push_str("\nTasks released by the goal (the runtime stops none of them):");
    if released.is_empty() {
        question.push_str("\n- none");
    }
    for dependent in released {
        let runs = if dependent.runs.is_empty() {
            "no runs".to_owned()
        } else {
            let runs: Vec<String> = dependent
                .runs
                .iter()
                .map(|(id, status)| format!("{id} {status}"))
                .collect();
            format!("runs {}", runs.join(", "))
        };
        question.push_str(&format!(
            "\n- task {} ({}): {}; {runs}",
            dependent.task,
            dependent.status.as_str(),
            dependent.title
        ));
    }
    question.push_str(
        "\nAnswer reopen (open the goal again and move the follow-up back into it; its released tasks not yet claimed wait for it again), correct_verdict (keep it closed and record that achieved was wrong; the fix goes to a fix goal), or keep_achieved (the acceptance was met; the follow-up is out of scope).",
    );
    question
}

#[cfg(test)]
mod membership_tests {
    use super::*;
    use crate::domain::GoalId;
    fn required() -> MembershipJudgement {
        MembershipJudgement {
            classification: MembershipClassification::Required,
            acceptance_items: vec!["(1)".into()],
            reason: "cannot satisfy (1) without it".into(),
            evidence: vec!["receipt:r".into()],
            destination_goal_id: None,
            source_goal_id: None,
            corrects: None,
        }
    }
    #[test]
    fn membership_requires_fields_and_allows_only_documented_transitions() {
        use MembershipClassification::*;
        let source = GoalId::new(1);
        let valid = required();
        assert!(valid.validate(None, source).is_ok());
        for invalid in [
            MembershipJudgement {
                reason: " ".into(),
                ..valid.clone()
            },
            MembershipJudgement {
                acceptance_items: vec![],
                ..valid.clone()
            },
            MembershipJudgement {
                evidence: vec![],
                ..valid.clone()
            },
            MembershipJudgement {
                evidence: vec![" ".into()],
                ..valid.clone()
            },
            MembershipJudgement {
                classification: OutOfScope,
                ..valid.clone()
            },
            MembershipJudgement {
                classification: OutOfScope,
                destination_goal_id: Some(source),
                ..valid.clone()
            },
        ] {
            assert!(invalid.validate(None, source).is_err());
        }
        assert!("tiny".parse::<MembershipClassification>().is_err());
        let undecided = MembershipJudgement {
            classification: Undecided,
            acceptance_items: vec![],
            evidence: vec![],
            ..valid.clone()
        };
        assert!(undecided.validate(None, source).is_ok());
        assert!(valid.validate(Some((1, Undecided)), source).is_ok());
        assert!(valid.validate(Some((1, Required)), source).is_ok());
        assert!(undecided.validate(Some((1, Required)), source).is_err());
        assert!(undecided.validate(Some((1, OutOfScope)), source).is_err());
        let outside = MembershipJudgement {
            classification: OutOfScope,
            destination_goal_id: Some(GoalId::new(2)),
            ..valid.clone()
        };
        assert!(outside.validate(Some((1, Required)), source).is_err());
        assert!(
            MembershipJudgement {
                corrects: Some(1),
                ..outside.clone()
            }
            .validate(Some((1, Required)), source)
            .is_ok()
        );
        assert!(
            MembershipJudgement {
                corrects: Some(2),
                ..outside
            }
            .validate(Some((1, Required)), source)
            .is_err()
        );
        assert!(
            MembershipJudgement {
                corrects: Some(1),
                ..valid
            }
            .validate(Some((1, OutOfScope)), source)
            .is_ok()
        );
    }
    /// Which follow-ups keep their source goal from closing as achieved
    /// (ADR-t1504-2 decision 8).
    #[test]
    fn only_unjudged_undecided_stale_or_outside_required_follow_ups_block_the_close() {
        use super::super::{TaskId, TaskStatus};
        use MembershipClassification::*;
        let follow_up = |status, in_goal, judgement| SourceFollowUp {
            task: TaskId::new(5),
            status,
            in_goal,
            judgement,
        };
        let open = TaskStatus::Draft;
        for (case, why) in [
            (follow_up(open, true, None), Some("not judged")),
            (
                follow_up(open, false, Some((1, Undecided, false))),
                Some("undecided"),
            ),
            (
                follow_up(open, false, Some((1, OutOfScope, true))),
                Some("judged before the acceptance changed"),
            ),
            (
                follow_up(TaskStatus::InProgress, false, Some((1, Required, false))),
                Some("required but outside the goal"),
            ),
            // Out of scope does not wait for its work, wherever it is.
            (follow_up(open, false, Some((1, OutOfScope, false))), None),
            // A required one in the goal waits as a task of the goal.
            (follow_up(open, true, Some((1, Required, false))), None),
            // An ended one never waits.
            (follow_up(TaskStatus::Completed, false, None), None),
            (
                follow_up(TaskStatus::Canceled, false, Some((1, Undecided, true))),
                None,
            ),
        ] {
            assert_eq!(case.unsettled(), why, "{case:?}");
        }
        assert_eq!(
            unsettled_follow_ups(&[
                follow_up(open, false, Some((1, OutOfScope, false))),
                follow_up(open, true, None),
            ]),
            [(TaskId::new(5), "not judged".to_owned())]
        );
        assert_eq!(
            follow_up(open, false, Some((3, OutOfScope, false))).fingerprint(),
            "5:3-out_of_scope"
        );
        assert_eq!(follow_up(open, true, None).fingerprint(), "5:-");
    }

    /// ADR-t1504-2 decision 9: only a required judgement after an achieved
    /// close asks a person; the ask names the judgement and every released
    /// task with its runs, and offers the three answers.
    #[test]
    fn a_required_judgement_after_achieved_asks_with_the_released_tasks() {
        use super::super::{GoalVerdict, TaskId, TaskStatus};
        use MembershipClassification::*;
        let achieved = Some(GoalVerdict::Achieved);
        for (class, previous, verdict, opens) in [
            (Required, None, achieved, true),
            (Required, Some(OutOfScope), achieved, true),
            (Required, Some(Undecided), achieved, true),
            // A recheck of a required judgement settles nothing new.
            (Required, Some(Required), achieved, false),
            (Required, None, Some(GoalVerdict::Abandoned), false),
            (Required, None, None, false),
            (OutOfScope, Some(Required), achieved, false),
            (Undecided, None, achieved, false),
        ] {
            assert_eq!(
                opens_correction(class, previous, verdict),
                opens,
                "{class:?} {previous:?} {verdict:?}"
            );
        }
        assert_eq!(
            CORRECTION_OPTIONS,
            ["reopen", "correct_verdict", "keep_achieved"]
        );
        assert_eq!(
            CorrectionAnswer::parse(" reopen "),
            Some(CorrectionAnswer::Reopen)
        );
        assert_eq!(CorrectionAnswer::parse("reopen it"), None);
        let released = [
            ReleasedDependent {
                task: TaskId::new(8),
                title: "next step".into(),
                status: TaskStatus::InProgress,
                runs: vec![
                    ("r1".into(), "failed".into()),
                    ("r2".into(), "running".into()),
                ],
            },
            ReleasedDependent {
                task: TaskId::new(9),
                title: "later".into(),
                status: TaskStatus::Ready,
                runs: Vec::new(),
            },
        ];
        let question =
            correction_question(GoalId::new(3), TaskId::new(5), 4, &required(), &released);
        assert!(
            question.starts_with("Goal 3 was closed as achieved, but its follow-up task 5"),
            "{question}"
        );
        for part in [
            "(judgement 4)",
            "Acceptance items: (1)",
            "Reason: cannot satisfy (1) without it",
            "Evidence: receipt:r",
            "- task 8 (in_progress): next step; runs r1 failed, r2 running",
            "- task 9 (ready): later; no runs",
            "reopen (",
            "correct_verdict (",
            "keep_achieved (",
        ] {
            assert!(question.contains(part), "{part}: {question}");
        }
        assert!(
            correction_question(GoalId::new(3), TaskId::new(5), 4, &required(), &[])
                .contains("\n- none")
        );
    }

    #[test]
    fn submit_needs_a_current_decided_judgement_unless_no_acceptance_applies() {
        use MembershipClassification::*;
        let facts = |latest| MembershipFacts {
            source_goal_none: false,
            source_goal_abandoned: false,
            latest,
        };
        assert_eq!(membership_gap(facts(None)), Some(MembershipGap::Missing));
        assert_eq!(
            membership_gap(facts(Some((Undecided, false)))),
            Some(MembershipGap::Undecided)
        );
        for class in [Required, OutOfScope] {
            assert_eq!(membership_gap(facts(Some((class, false)))), None);
            assert_eq!(
                membership_gap(facts(Some((class, true)))),
                Some(MembershipGap::NeedsRecheck)
            );
        }
        for latest in [None, Some((Undecided, true))] {
            assert_eq!(
                membership_gap(MembershipFacts {
                    source_goal_none: true,
                    ..facts(latest)
                }),
                None
            );
            assert_eq!(
                membership_gap(MembershipFacts {
                    source_goal_abandoned: true,
                    ..facts(latest)
                }),
                None
            );
        }
        assert!(MembershipGap::NeedsRecheck.explain().contains("recheck"));
    }
    #[test]
    fn registration_facts_cannot_be_overridden_by_current_membership() {
        for provenance in ["recorded", "restored", "unknown"] {
            for goal in [None, Some(1), Some(-1)] {
                let material = serde_json::json!({"source_goal_state":"open", "source_goal_id":goal, "source_goal_provenance":provenance});
                assert_eq!(
                    source_goal_was_open(&material),
                    goal == Some(1) && provenance != "unknown"
                );
            }
        }
        assert!(!source_goal_was_open(&serde_json::json!({})));
        for depth in [0, 1, 2, 3, 4] {
            for current in [false, true] {
                for source in [false, true] {
                    let needs = adopt_needs_person(FollowUpFacts {
                        goal_open: current,
                        source_goal_open: source,
                        depth,
                    })
                    .is_some();
                    assert_eq!(needs, !source || !current || depth >= 3);
                }
            }
        }
    }
}
