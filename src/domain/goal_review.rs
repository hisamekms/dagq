//! Goal review (ADR-0047 decision 43): the verdict the headless job prints
//! about one goal whose tasks all ended, what the runtime makes of it, the
//! state of the goal's tasks a review saw, and the answers a person gives
//! to its `approve_goal` ask.

use serde::{Deserialize, Serialize};

use super::{
    AskReason, DomainError, GoalId, GoalVerdict, TaskId, TaskStatus, follow_up::SourceFollowUp,
    parse_json_object,
};

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
#[serde(deny_unknown_fields)]
pub struct GoalCriterion {
    pub criterion: String,
    pub met: bool,
    #[serde(default)]
    pub evidence: Vec<String>,
}

/// Something the goal still lacks, registered as a draft task of the goal
/// with the origin `goal_gap`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalGap {
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// The acceptance item it is missing for.
    #[serde(default)]
    pub criterion: String,
}

/// What the headless goal review prints on stdout: one JSON object. A
/// field it does not know is refused, as the other jobs' verdicts are
/// (ADR-t728-1): the output is data, and a shape the runtime does not
/// read fails the review closed rather than being half applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// The input a review saw (ADR-t1504-2 decision 8): the state of the
/// goal's tasks ([`fingerprint`]), then the acceptance version when it
/// changed from the first, and each follow-up whose source is the goal
/// with its current judgement. A goal with neither keeps the fingerprint
/// of its tasks alone; one with a follow-up (ended ones included) or an
/// acceptance version above 1 is reviewed once more after this changed,
/// as its input now has them.
pub fn review_fingerprint(
    tasks: &[(TaskId, TaskStatus)],
    acceptance_version: i64,
    follow_ups: &[SourceFollowUp],
) -> String {
    let mut seen = fingerprint(tasks);
    if acceptance_version != 1 {
        seen.push_str(&format!(";acceptance:{acceptance_version}"));
    }
    if !follow_ups.is_empty() {
        let mut follow_ups = follow_ups.to_vec();
        follow_ups.sort_by_key(|f| f.task);
        let follow_ups: Vec<String> = follow_ups.iter().map(SourceFollowUp::fingerprint).collect();
        seen.push_str(&format!(";follow_ups:{}", follow_ups.join(",")));
    }
    seen
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

/// How many of a goal's latest reviews that decided something were
/// `gaps`, given their outcomes newest first.
pub fn gaps_in_a_row(outcomes: &[&str]) -> usize {
    outcomes.iter().take_while(|o| **o == "gaps").count()
}

/// What a goal's latest review that ran to an end (not `interrupted`)
/// left: its outcome, the input it saw ([`review_fingerprint`]) and
/// whether a person rearmed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatestReview<'a> {
    pub outcome: &'a str,
    pub seen: &'a str,
    pub rearmed: bool,
}

/// Whether a review may take the goal now: it is open, its input
/// (`fingerprint`) may close it (`closable`: its tasks all ended, one
/// completed, no follow-up of it unsettled), no `approve_goal` ask of it is
/// open, and its latest review saw another input or a person rearmed it.
pub fn reviewable(
    open: bool,
    closable: bool,
    ask_open: bool,
    latest: Option<LatestReview<'_>>,
    fingerprint: &str,
) -> bool {
    open && closable
        && !ask_open
        && !latest.is_some_and(|latest| latest.seen == fingerprint && !latest.rearmed)
}

/// Whether an open goal waits for a person to review it by hand: its latest
/// review failed on the input it has now (`fingerprint`), and nobody
/// rearmed it.
pub fn waits_for_a_person(latest: Option<LatestReview<'_>>, fingerprint: &str) -> bool {
    latest.is_some_and(|latest| {
        latest.outcome == "failed" && !latest.rearmed && latest.seen == fingerprint
    })
}

/// Whether `goal` may be reviewed again by a person's rearm, given whether
/// it exists and is open (`open`, `None` when it does not exist).
pub fn rearmable(goal: GoalId, open: Option<bool>) -> Result<(), String> {
    match open {
        None => Err(format!("goal {goal} does not exist")),
        Some(false) => Err(format!(
            "goal {goal} is not open; only an open goal is reviewed"
        )),
        Some(true) => Ok(()),
    }
}

impl GoalAnswer {
    /// Whether the supervisor can apply the answer to an open goal whose
    /// tasks are `tasks`: `achieved` needs every task ended and no
    /// follow-up of the goal unsettled (`follow_ups_settled`), `abandoned`
    /// no task in progress, `gaps` without a text the review's gaps
    /// (`review_has_gaps`); `gaps: <what>` and `keep_open` always apply.
    pub fn fits(
        &self,
        tasks: &[(TaskId, TaskStatus)],
        follow_ups_settled: bool,
        review_has_gaps: bool,
    ) -> bool {
        match self {
            Self::Achieved => {
                tasks
                    .iter()
                    .all(|(_, status)| GoalVerdict::Achieved.allows(*status))
                    && follow_ups_settled
            }
            Self::Abandoned => tasks
                .iter()
                .all(|(_, status)| GoalVerdict::Abandoned.allows(*status)),
            Self::Gaps(Some(_)) | Self::KeepOpen => true,
            Self::Gaps(None) => review_has_gaps,
        }
    }

    /// The verdict the goal closes with on the answer; `None` for one that
    /// leaves it open (`gaps` registers drafts, `keep_open` waits for its
    /// tasks to change).
    pub fn closes(&self) -> Option<GoalVerdict> {
        match self {
            Self::Achieved => Some(GoalVerdict::Achieved),
            Self::Abandoned => Some(GoalVerdict::Abandoned),
            Self::Gaps(_) | Self::KeepOpen => None,
        }
    }
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
    fn a_verdict_with_an_unknown_field_is_refused() {
        for stdout in [
            r#"{"verdict":"achieved","summary":"done","close":true}"#,
            r#"{"verdict":"achieved","criteria":[{"criterion":"(1)","met":true,"override":1}]}"#,
            r#"{"verdict":"gaps","gaps":[{"title":"docs","priority":"high"}]}"#,
        ] {
            let error = GoalReviewVerdict::parse(stdout).unwrap_err();
            assert!(error.contains("unknown field"), "{stdout}: {error}");
        }
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

    /// A review's input changes with the acceptance version and with each
    /// follow-up's registration and judgement (not its status); a goal with
    /// neither keeps the fingerprint of its tasks (ADR-t1504-2 decision 8).
    #[test]
    fn the_review_fingerprint_covers_acceptance_and_follow_up_judgements() {
        use crate::domain::follow_up::MembershipClassification::*;
        let tasks = [(TaskId::new(2), TaskStatus::Completed)];
        assert_eq!(review_fingerprint(&tasks, 1, &[]), "2:completed");
        assert_eq!(
            review_fingerprint(&tasks, 2, &[]),
            "2:completed;acceptance:2"
        );
        let follow_up = |task, judgement| SourceFollowUp {
            task: TaskId::new(task),
            status: TaskStatus::Draft,
            in_goal: false,
            judgement,
        };
        let judged = review_fingerprint(
            &tasks,
            1,
            &[
                follow_up(9, Some((4, OutOfScope, false))),
                follow_up(8, None),
            ],
        );
        assert_eq!(judged, "2:completed;follow_ups:8:-,9:4-out_of_scope");
        let rejudged = review_fingerprint(
            &tasks,
            1,
            &[
                follow_up(9, Some((6, OutOfScope, false))),
                follow_up(8, None),
            ],
        );
        assert_ne!(judged, rejudged);
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

    #[test]
    fn gaps_are_counted_back_from_the_newest_decision() {
        assert_eq!(gaps_in_a_row(&[]), 0);
        assert_eq!(gaps_in_a_row(&["gaps", "gaps", "gaps"]), 3);
        assert_eq!(gaps_in_a_row(&["gaps", "ask", "gaps"]), 1);
        assert_eq!(gaps_in_a_row(&["achieved", "gaps"]), 0);
        // Three in a row make the fourth gaps an ask.
        let (decision, why) = decide(
            GoalReviewDecision::Gaps,
            gaps_in_a_row(&["gaps", "gaps", "gaps"]),
        );
        assert_eq!(decision, GoalReviewDecision::Ask);
        assert_eq!(
            why.as_deref(),
            Some("goal review found gaps 3 times in a row already (at most 3)")
        );
    }

    /// Only an open goal whose tasks all ended (one completed), with no
    /// open ask, is reviewed, and only once per input unless a person
    /// rearms it; a failed review holds it for a person until then.
    #[test]
    fn a_goal_is_reviewed_once_per_input_unless_rearmed() {
        let done = [
            (TaskId::new(1), TaskStatus::Completed),
            (TaskId::new(2), TaskStatus::Canceled),
        ];
        let seen = fingerprint(&done);
        let latest = |outcome, seen, rearmed| {
            Some(LatestReview {
                outcome,
                seen,
                rearmed,
            })
        };
        assert!(reviewable(true, tasks_done(&done), false, None, &seen));
        // No task, only canceled tasks, a task still draft.
        for tasks in [
            &[][..],
            &[(TaskId::new(1), TaskStatus::Canceled)][..],
            &[
                (TaskId::new(1), TaskStatus::Completed),
                (TaskId::new(2), TaskStatus::Draft),
            ][..],
        ] {
            assert!(!reviewable(
                true,
                tasks_done(tasks),
                false,
                None,
                &fingerprint(tasks)
            ));
        }
        // A closed goal; an open approve_goal ask.
        assert!(!reviewable(false, true, false, None, &seen));
        assert!(!reviewable(true, true, true, None, &seen));
        // Seen already: not again, unless rearmed or the input changed (a
        // gap's draft that ended).
        assert!(!reviewable(
            true,
            true,
            false,
            latest("gaps", &seen, false),
            &seen
        ));
        assert!(reviewable(
            true,
            true,
            false,
            latest("gaps", &seen, true),
            &seen
        ));
        let after_gap = fingerprint(&[
            (TaskId::new(1), TaskStatus::Completed),
            (TaskId::new(2), TaskStatus::Canceled),
            (TaskId::new(3), TaskStatus::Completed),
        ]);
        assert!(reviewable(
            true,
            true,
            false,
            latest("gaps", &seen, false),
            &after_gap
        ));
        // A failed review: it waits for a person, not reviewed again by
        // itself, until rearmed or its input changes.
        let failed = latest("failed", &seen, false);
        assert!(waits_for_a_person(failed, &seen));
        assert!(!reviewable(true, true, false, failed, &seen));
        assert!(!waits_for_a_person(latest("failed", &seen, true), &seen));
        assert!(!waits_for_a_person(failed, &after_gap));
        assert!(!waits_for_a_person(latest("ask", &seen, false), &seen));
        assert!(!waits_for_a_person(None, &seen));
        // Only an open goal is rearmed.
        assert_eq!(rearmable(GoalId::new(4), Some(true)), Ok(()));
        assert_eq!(
            rearmable(GoalId::new(4), Some(false)).unwrap_err(),
            "goal 4 is not open; only an open goal is reviewed"
        );
        assert_eq!(
            rearmable(GoalId::new(99), None).unwrap_err(),
            "goal 99 does not exist"
        );
    }

    /// The answers of an `approve_goal` ask the supervisor applies, and
    /// when: `achieved` and `abandoned` close the goal, `gaps` registers
    /// the review's gaps or the person's, `keep_open` leaves it open; an
    /// option the runtime does not know (the job's own) is the inbox's.
    #[test]
    fn an_answer_applies_when_the_goal_allows_it() {
        let ended = [
            (TaskId::new(1), TaskStatus::Completed),
            (TaskId::new(2), TaskStatus::Canceled),
        ];
        let pending = [
            (TaskId::new(1), TaskStatus::Completed),
            (TaskId::new(2), TaskStatus::Ready),
        ];
        let running = [(TaskId::new(1), TaskStatus::InProgress)];
        let parse = |text| GoalAnswer::parse(text).unwrap();
        assert!(parse("achieved").fits(&ended, true, false));
        assert!(!parse("achieved").fits(&pending, true, false));
        assert!(!parse("achieved").fits(&ended, false, false));
        assert!(parse("abandoned").fits(&pending, false, false));
        assert!(!parse("abandoned").fits(&running, true, true));
        assert!(parse("keep_open").fits(&running, false, false));
        assert!(parse("gaps: docs").fits(&running, false, false));
        assert!(parse("gaps").fits(&ended, true, true));
        assert!(!parse("gaps").fits(&ended, true, false));
        assert_eq!(GoalAnswer::parse("lower_target"), None);

        assert_eq!(parse("achieved").closes(), Some(GoalVerdict::Achieved));
        assert_eq!(parse("abandoned").closes(), Some(GoalVerdict::Abandoned));
        assert_eq!(parse("keep_open").closes(), None);
        assert_eq!(parse("gaps").closes(), None);
    }
}
