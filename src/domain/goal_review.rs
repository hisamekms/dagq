//! Goal review (ADR-0047 decision 43): the verdict the headless job prints
//! about one goal whose tasks all ended, what the runtime makes of it, the
//! state of the goal's tasks a review saw, and the answers a person gives
//! to its `approve_goal` ask.

use serde::{Deserialize, Serialize};

use super::{AskReason, DomainError, TaskId, TaskStatus, parse_json_object};

// What goal review decided: `achieved` closes the goal, `gaps` registers
// what is missing as drafts of the goal, `ask` waits for a person in an
// `approve_goal` ask.
string_enum!(GoalReviewDecision {
    Achieved => "achieved",
    Gaps => "gaps",
    Ask => "ask",
});

/// One item of the goal's acceptance and whether it is met, with where the
/// job saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalCriterion {
    pub criterion: String,
    pub met: bool,
    #[serde(default)]
    pub evidence: Vec<String>,
}

/// Something the goal still lacks, registered as a draft task of the goal
/// with the origin `goal_gap`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalGap {
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// The acceptance item it is missing for.
    #[serde(default)]
    pub criterion: String,
}

/// What the headless goal review prints on stdout: one JSON object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalReviewVerdict {
    pub verdict: GoalReviewDecision,
    #[serde(default)]
    pub criteria: Vec<GoalCriterion>,
    #[serde(default)]
    pub gaps: Vec<GoalGap>,
    #[serde(default)]
    pub summary: String,
    /// The question of an `ask`; its summary when blank.
    #[serde(default)]
    pub question: String,
    /// Options an `ask` offers besides [`GOAL_OPTIONS`].
    #[serde(default)]
    pub options: Vec<String>,
    /// Why an `ask` needs a person: `scope` or `discard`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_category: Option<AskReason>,
}

impl GoalReviewVerdict {
    /// The verdict in the job's stdout, found the way the run review's is.
    /// `gaps` needs a gap, each with a title.
    pub fn parse(stdout: &str) -> Result<Self, String> {
        let verdict: Self = parse_json_object(stdout)
            .map_err(|error| format!("the goal review printed no verdict JSON: {error}"))?;
        if verdict.verdict == GoalReviewDecision::Gaps && verdict.gaps.is_empty() {
            return Err("the goal review's gaps verdict lists no gap".into());
        }
        if verdict.gaps.iter().any(|gap| gap.title.trim().is_empty()) {
            return Err("a gap of the goal review has a blank title".into());
        }
        Ok(verdict)
    }
}

/// How many `gaps` verdicts in a row a goal gets before a review that does
/// not find it achieved asks a person instead (the guard against filling
/// gaps forever).
pub const MAX_GOAL_GAPS: usize = 3;

/// What the runtime acts on: the verdict's decision, except a `gaps` after
/// [`MAX_GOAL_GAPS`] of them in a row, which is an `ask` (`overridden`
/// says why).
pub fn decide(
    verdict: GoalReviewDecision,
    gaps_in_a_row: usize,
) -> (GoalReviewDecision, Option<String>) {
    match verdict {
        GoalReviewDecision::Gaps if gaps_in_a_row >= MAX_GOAL_GAPS => (
            GoalReviewDecision::Ask,
            Some(format!(
                "goal review found gaps {gaps_in_a_row} times in a row already (at most {MAX_GOAL_GAPS})"
            )),
        ),
        decision => (decision, None),
    }
}

/// The options of every `approve_goal` ask, which the supervisor applies
/// once answered; `gaps` may be followed by `:` and what is missing.
pub const GOAL_OPTIONS: &[&str] = &["achieved", "abandoned", "gaps", "keep_open"];

/// Who opens the `approve_goal` asks.
pub const GOAL_REVIEW_ASKER: &str = "goal_review";

/// A person's answer to an `approve_goal` ask the supervisor applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalAnswer {
    /// Close the goal as achieved.
    Achieved,
    /// Close the goal as abandoned.
    Abandoned,
    /// Register drafts for what is missing: the person's text as one gap
    /// (`gaps: <what>`), else the gaps of the review.
    Gaps(Option<String>),
    /// Leave the goal open; no review until its tasks change.
    KeepOpen,
}

impl GoalAnswer {
    /// One of [`GOAL_OPTIONS`], `gaps` optionally followed by `:` and what
    /// is missing; anything else is a person's to read.
    pub fn parse(answer: &str) -> Option<Self> {
        let answer = answer.trim();
        match answer {
            "achieved" => return Some(Self::Achieved),
            "abandoned" => return Some(Self::Abandoned),
            "keep_open" => return Some(Self::KeepOpen),
            "gaps" => return Some(Self::Gaps(None)),
            _ => {}
        }
        let what = answer.strip_prefix("gaps")?.trim_start();
        let what = what.strip_prefix(':')?.trim();
        Some(Self::Gaps((!what.is_empty()).then(|| what.to_owned())))
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Achieved => "achieved",
            Self::Abandoned => "abandoned",
            Self::Gaps(_) => "gaps",
            Self::KeepOpen => "keep_open",
        }
    }
}

/// A gap a person wrote in a `gaps: <what>` answer: its first line (cut to
/// 100 characters) is the title, the whole text the description.
pub fn person_gap(what: &str) -> GoalGap {
    let first = what.lines().next().unwrap_or(what).trim();
    let title: String = first.chars().take(100).collect();
    GoalGap {
        title,
        description: what.to_owned(),
        criterion: String::new(),
    }
}

/// The state of a goal's tasks a review saw, which the next review is
/// started only after it changes: each task's ID and status, in ID order.
pub fn fingerprint(tasks: &[(TaskId, TaskStatus)]) -> String {
    let mut tasks = tasks.to_vec();
    tasks.sort_by_key(|(id, _)| *id);
    tasks
        .iter()
        .map(|(id, status)| format!("{id}:{}", status.as_str()))
        .collect::<Vec<_>>()
        .join(",")
}

/// Whether the tasks of an open goal let a review start: at least one,
/// each completed or canceled, and one completed.
pub fn tasks_done(tasks: &[(TaskId, TaskStatus)]) -> bool {
    tasks
        .iter()
        .all(|(_, status)| matches!(status, TaskStatus::Completed | TaskStatus::Canceled))
        && tasks
            .iter()
            .any(|(_, status)| *status == TaskStatus::Completed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verdict_is_parsed_from_the_last_json_object() {
        let verdict = GoalReviewVerdict::parse(
            "thinking\n{\"verdict\":\"achieved\",\"criteria\":[{\"criterion\":\"(1)\",\"met\":true,\"evidence\":[\"task 3\"]}],\"summary\":\"done\"}\n",
        )
        .unwrap();
        assert_eq!(verdict.verdict, GoalReviewDecision::Achieved);
        assert_eq!(verdict.criteria[0].evidence, ["task 3"]);
        assert!(GoalReviewVerdict::parse("no json").is_err());
    }

    #[test]
    fn gaps_need_a_titled_gap() {
        assert!(GoalReviewVerdict::parse(r#"{"verdict":"gaps","summary":"x"}"#).is_err());
        assert!(GoalReviewVerdict::parse(r#"{"verdict":"gaps","gaps":[{"title":" "}]}"#).is_err());
        let verdict =
            GoalReviewVerdict::parse(r#"{"verdict":"gaps","gaps":[{"title":"docs"}]}"#).unwrap();
        assert_eq!(verdict.gaps[0].title, "docs");
    }

    #[test]
    fn a_fourth_gaps_in_a_row_asks() {
        assert_eq!(
            decide(GoalReviewDecision::Gaps, 2),
            (GoalReviewDecision::Gaps, None)
        );
        let (decision, why) = decide(GoalReviewDecision::Gaps, 3);
        assert_eq!(decision, GoalReviewDecision::Ask);
        assert!(why.unwrap().contains("3 times"));
        assert_eq!(
            decide(GoalReviewDecision::Achieved, 5).0,
            GoalReviewDecision::Achieved
        );
    }

    #[test]
    fn answers_parse() {
        assert_eq!(GoalAnswer::parse(" achieved "), Some(GoalAnswer::Achieved));
        assert_eq!(GoalAnswer::parse("abandoned"), Some(GoalAnswer::Abandoned));
        assert_eq!(GoalAnswer::parse("keep_open"), Some(GoalAnswer::KeepOpen));
        assert_eq!(GoalAnswer::parse("gaps"), Some(GoalAnswer::Gaps(None)));
        assert_eq!(
            GoalAnswer::parse("gaps: write the docs"),
            Some(GoalAnswer::Gaps(Some("write the docs".into())))
        );
        assert_eq!(GoalAnswer::parse("gaps:"), Some(GoalAnswer::Gaps(None)));
        assert_eq!(GoalAnswer::parse("gapsx"), None);
        assert_eq!(GoalAnswer::parse("maybe"), None);
        assert_eq!(GoalAnswer::Gaps(None).as_str(), "gaps");
        assert_eq!(GoalAnswer::KeepOpen.as_str(), "keep_open");
    }

    #[test]
    fn a_person_gap_takes_its_first_line_as_title() {
        let gap = person_gap("write the docs\nfor the CLI");
        assert_eq!(gap.title, "write the docs");
        assert_eq!(gap.description, "write the docs\nfor the CLI");
    }

    #[test]
    fn fingerprints_and_done_tasks() {
        let tasks = [
            (TaskId::new(3), TaskStatus::Canceled),
            (TaskId::new(2), TaskStatus::Completed),
        ];
        assert_eq!(fingerprint(&tasks), "2:completed,3:canceled");
        assert!(tasks_done(&tasks));
        assert!(!tasks_done(&[(TaskId::new(1), TaskStatus::Canceled)]));
        assert!(!tasks_done(&[]));
        assert!(!tasks_done(&[
            (TaskId::new(1), TaskStatus::Completed),
            (TaskId::new(2), TaskStatus::Draft)
        ]));
    }
}
