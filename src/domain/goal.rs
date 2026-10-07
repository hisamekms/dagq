//! The goal aggregate: its state, the rules that create and restore it, and
//! the commands and queries on it. The fields are private, so a goal changes
//! only through the functions here; the store saves what they return.

use serde::Serialize;

use super::{
    DomainError, GoalEdit, GoalId, GoalRecord, GoalStatus, GoalSummary, GoalTag, GoalVerdict,
    NewGoal, Priority, TaskId, TaskStatus, TaskStatusCounts,
    plan_request::{PriorityBy, RecordedOrigin},
    require,
};

impl GoalVerdict {
    /// `achieved` needs every task finished; `abandoned` only needs nothing
    /// executing, so unstarted tasks stay in the queue as they are.
    pub fn allows(self, task: TaskStatus) -> bool {
        match self {
            Self::Achieved => task.is_terminal(),
            Self::Abandoned => task != TaskStatus::InProgress,
        }
    }
}

/// A higher-level problem that several tasks solve together (ADR-0009). The
/// description, acceptance and constraints align the judgement of sibling
/// tasks; `doc` is a path inside the repository. There are no verification
/// commands: machine checks belong to a task that depends on the others.
/// `Serialize` is the JSON the CLI prints; there is no `Deserialize`: a goal
/// is built by [`Goal::new`] or [`Goal::restore`] only.
#[derive(Debug, Clone, Serialize)]
pub struct Goal {
    id: GoalId,
    title: String,
    description: String,
    acceptance: String,
    constraints: String,
    doc: Option<String>,
    /// What the goal's tasks without a priority of their own inherit
    /// (ADR-t1639-1 decisions 1 and 2).
    priority: Priority,
    /// Who set `priority` (ADR-t1975-1 decision 2): `human` for a person's,
    /// the priority of a request the runtime attached included.
    priority_by: PriorityBy,
    /// Where it comes from, recorded at its creation (ADR-t1975-1
    /// decision 5): shown as `origin`, `origin_kind`, `origin_request_id`.
    #[serde(flatten)]
    origin: RecordedOrigin,
    /// What the goal is about, each once, in the order given (ADR-t1639-1
    /// decision 6).
    tags: Vec<GoalTag>,
    /// `draft` or `open` (ADR-0044 decision 5): a draft goal's tasks are
    /// not candidates and are never claimed. Closing does not change it.
    status: GoalStatus,
    /// Set together with `verdict` by the one close.
    closed_at: Option<String>,
    /// Recorded once per close. Only a person's `reopen` of a goal closed
    /// as achieved clears it ([`reopen`]); the `goal_closed` event stays.
    verdict: Option<GoalVerdict>,
    created_at: String,
    updated_at: String,
}

const GOAL_TITLE_BLANK: DomainError = DomainError::Blank {
    field: "goal title",
};

impl Goal {
    /// A goal registered now as `id`: open, or a draft when `new.draft`
    /// (ADR-0024 decision 5), and not closed. A blank `doc` is no reference.
    pub fn new(id: GoalId, new: NewGoal, created_at: String) -> Result<Self, DomainError> {
        new.validate()?;
        require_positive(id)?;
        Ok(Self {
            id,
            title: new.title,
            description: new.description,
            acceptance: new.acceptance,
            constraints: new.constraints,
            doc: new.doc.filter(|d| !d.trim().is_empty()),
            priority: new.priority.unwrap_or_default(),
            priority_by: PriorityBy::Ai,
            origin: RecordedOrigin::UNKNOWN,
            tags: new.tags,
            status: if new.draft {
                GoalStatus::Draft
            } else {
                GoalStatus::Open
            },
            closed_at: None,
            verdict: None,
            updated_at: created_at.clone(),
            created_at,
        })
    }

    /// A stored goal as it was saved. Checked: a positive ID, a title that
    /// is not blank, tags given once, and a close time exactly when there
    /// is a verdict.
    pub fn restore(record: GoalRecord) -> Result<Self, DomainError> {
        require_positive(record.id)?;
        require(!record.title.trim().is_empty(), || GOAL_TITLE_BLANK)?;
        super::goal_tag::check_distinct(&record.tags)?;
        require(
            record.closed_at.is_some() == record.verdict.is_some(),
            || DomainError::GoalCloseInconsistent { goal_id: record.id },
        )?;
        Ok(Self {
            id: record.id,
            title: record.title,
            description: record.description,
            acceptance: record.acceptance,
            constraints: record.constraints,
            doc: record.doc,
            priority: record.priority,
            priority_by: PriorityBy::Human,
            origin: RecordedOrigin::UNKNOWN,
            tags: record.tags,
            status: record.status,
            closed_at: record.closed_at,
            verdict: record.verdict,
            created_at: record.created_at,
            updated_at: record.updated_at,
        })
    }

    pub fn id(&self) -> GoalId {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn acceptance(&self) -> &str {
        &self.acceptance
    }

    pub fn constraints(&self) -> &str {
        &self.constraints
    }

    pub fn doc(&self) -> Option<&str> {
        self.doc.as_deref()
    }

    pub fn priority(&self) -> Priority {
        self.priority
    }

    /// Who set [`Self::priority`] (ADR-t1975-1 decision 2).
    pub fn priority_by(&self) -> PriorityBy {
        self.priority_by
    }

    /// Where it comes from (ADR-t1975-1 decision 5).
    pub fn origin(&self) -> RecordedOrigin {
        self.origin
    }

    /// The goal with what the queue records beside it: who set its
    /// priority, and its origin.
    pub fn with_record(mut self, priority_by: PriorityBy, origin: RecordedOrigin) -> Self {
        self.priority_by = priority_by;
        self.origin = origin;
        self
    }

    pub fn tags(&self) -> &[GoalTag] {
        &self.tags
    }

    pub fn status(&self) -> GoalStatus {
        self.status
    }

    pub fn closed_at(&self) -> Option<&str> {
        self.closed_at.as_deref()
    }

    pub fn verdict(&self) -> Option<GoalVerdict> {
        self.verdict
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    pub fn updated_at(&self) -> &str {
        &self.updated_at
    }

    pub fn is_closed(&self) -> bool {
        self.closed_at.is_some()
    }

    /// An unclosed draft: the only goal `goal ready` opens.
    pub fn is_draft(&self) -> bool {
        self.status == GoalStatus::Draft && !self.is_closed()
    }

    /// The title, consuming the goal.
    pub fn into_title(self) -> String {
        self.title
    }
}

fn require_positive(id: GoalId) -> Result<(), DomainError> {
    require(id.as_i64() > 0, || DomainError::NonPositiveId {
        field: "goal ID",
    })
}

fn require_open(goal: &Goal) -> Result<(), DomainError> {
    require(!goal.is_closed(), || DomainError::GoalAlreadyClosed {
        goal_id: goal.id,
        verdict: goal.verdict,
    })
}

/// Tasks join and move between open goals only; a closed goal is a record.
pub fn check_accepts_tasks(goal: &Goal) -> Result<(), DomainError> {
    require(!goal.is_closed(), || DomainError::GoalClosed {
        goal_id: goal.id,
        verdict: goal.verdict,
    })
}

/// A task may depend on an open goal or one closed as achieved; one closed
/// as abandoned never releases the task, so it would never be claimed.
pub fn check_accepts_dependents(goal: &Goal) -> Result<(), DomainError> {
    require(goal.verdict != Some(GoalVerdict::Abandoned), || {
        DomainError::AbandonedGoalDependency { goal_id: goal.id }
    })
}

/// `goal` with the fields of `edit` replaced; a title must stay non-blank
/// and an empty `doc` clears the reference. The priority changes only while
/// the goal is a draft or open (ADR-t1639-1 decision 1): a closed goal has
/// no tasks left to claim. The tags replace the whole list, each once; an
/// empty list removes them, and a closed goal's tags change too, so `goal
/// list --tag` still finds it.
pub fn edit(mut goal: Goal, edit: GoalEdit) -> Result<Goal, DomainError> {
    if let Some(priority) = edit.priority {
        check_accepts_tasks(&goal)?;
        goal.priority = priority;
    }
    if let Some(tags) = edit.tags {
        super::goal_tag::check_distinct(&tags)?;
        goal.tags = tags;
    }
    if let Some(title) = edit.title {
        require(!title.trim().is_empty(), || GOAL_TITLE_BLANK)?;
        goal.title = title;
    }
    if let Some(description) = edit.description {
        goal.description = description;
    }
    if let Some(acceptance) = edit.acceptance {
        goal.acceptance = acceptance;
    }
    if let Some(constraints) = edit.constraints {
        goal.constraints = constraints;
    }
    if let Some(doc) = edit.doc {
        goal.doc = Some(doc).filter(|d| !d.trim().is_empty());
    }
    Ok(goal)
}

/// `goal list` (ADR-t1639-1 decision 7): the goals that carry one of
/// `tags` (all of them when `tags` is empty), the unclosed ones (open and
/// draft, a draft without tasks included) before the closed ones, each part
/// by priority, highest first, then by ID.
pub fn list(mut goals: Vec<GoalSummary>, tags: &[GoalTag]) -> Vec<GoalSummary> {
    if !tags.is_empty() {
        goals.retain(|goal| goal.tags.iter().any(|tag| tags.contains(tag)));
    }
    goals.sort_by_key(|goal| (goal.closed, std::cmp::Reverse(goal.priority), goal.id));
    goals
}

/// `goal ready`: a draft that is not closed opens, so its tasks become
/// candidates.
pub fn ready(mut goal: Goal) -> Result<Goal, DomainError> {
    require_open(&goal)?;
    require(goal.status == GoalStatus::Draft, || {
        DomainError::GoalNotDraft { goal_id: goal.id }
    })?;
    goal.status = GoalStatus::Open;
    Ok(goal)
}

/// A person's `reopen` answer to a `correct_goal` ask (ADR-t1504-2
/// decision 9, the one exception to ADR-0009's closed goal): a goal closed
/// as achieved opens again, so it can be closed again. Its `goal_closed`
/// event stays; the store adds the reopening one. A goal that is open, or
/// was abandoned, is not reopened.
pub fn reopen(mut goal: Goal, reopened_at: String) -> Result<Goal, DomainError> {
    require(goal.verdict == Some(GoalVerdict::Achieved), || {
        DomainError::GoalNotReopenable {
            goal_id: goal.id,
            verdict: goal.verdict,
        }
    })?;
    goal.status = GoalStatus::Open;
    goal.verdict = None;
    goal.closed_at = None;
    goal.updated_at = reopened_at;
    Ok(goal)
}

/// Whether the follow-ups whose source goal is `goal_id` let it close with
/// `verdict` (ADR-t1504-2 decision 8): `achieved` needs every one that is
/// not completed or canceled judged, rechecked after the last change of the
/// acceptance, and, when required, a task of the goal; `abandoned` claims
/// nothing and waits for none.
pub fn check_follow_ups(
    goal_id: GoalId,
    verdict: GoalVerdict,
    follow_ups: &[super::follow_up::SourceFollowUp],
) -> Result<(), DomainError> {
    if verdict != GoalVerdict::Achieved {
        return Ok(());
    }
    let unsettled = super::follow_up::unsettled_follow_ups(follow_ups);
    require(unsettled.is_empty(), || {
        DomainError::GoalFollowUpsUnsettled {
            goal_id,
            follow_ups: unsettled,
        }
    })
}

/// Close `goal` with `verdict` at `closed_at`, its tasks numbering `counts`
/// by status. A goal is closed once; the rejection names the statuses that
/// do not allow the verdict.
pub fn close(
    mut goal: Goal,
    verdict: GoalVerdict,
    counts: &TaskStatusCounts,
    closed_at: String,
) -> Result<Goal, DomainError> {
    require_open(&goal)?;
    let blocking: Vec<(TaskStatus, usize)> = [
        (TaskStatus::Draft, counts.draft),
        (TaskStatus::Submitted, counts.submitted),
        (TaskStatus::Ready, counts.ready),
        (TaskStatus::InProgress, counts.in_progress),
    ]
    .into_iter()
    .filter(|(status, n)| *n > 0 && !verdict.allows(*status))
    .collect();
    require(blocking.is_empty(), || DomainError::GoalCloseBlocked {
        goal_id: goal.id,
        verdict,
        blocking,
    })?;
    goal.verdict = Some(verdict);
    goal.updated_at = closed_at.clone();
    goal.closed_at = Some(closed_at);
    Ok(goal)
}

/// A task of a closed goal that is left unfinished (`draft`, `submitted`
/// or `ready`: approve withheld it, or the goal was abandoned) while other
/// tasks wait on it, directly or through other unfinished tasks (task
/// 421). It never completes, so its dependents are never claimed. The
/// runtime tells the inbox and does not hold the dependents: removing the
/// dependency, cancelling or taking the task up again is the plan's call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StrandedDependency {
    pub task_id: TaskId,
    pub goal_id: GoalId,
    pub verdict: GoalVerdict,
    /// The `ready` and `submitted` tasks of open goals (or of none) that
    /// wait on it, by ID.
    pub waiting: Vec<TaskId>,
}

impl StrandedDependency {
    /// What the inbox shows: the task, its goal and who waits.
    pub fn summary(&self) -> String {
        let waiting: Vec<String> = self.waiting.iter().map(ToString::to_string).collect();
        format!(
            "task {} of goal {} (closed {}) will not complete; tasks {} wait on it",
            self.task_id,
            self.goal_id,
            self.verdict.as_str(),
            waiting.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(status: GoalStatus, verdict: Option<GoalVerdict>) -> GoalRecord {
        GoalRecord {
            id: GoalId::new(7),
            title: "g".into(),
            description: String::new(),
            acceptance: String::new(),
            constraints: String::new(),
            doc: None,
            priority: Priority::Normal,
            tags: Vec::new(),
            status,
            closed_at: verdict.map(|_| "2026-09-23T00:00:00Z".into()),
            verdict,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn goal(verdict: Option<GoalVerdict>) -> Goal {
        Goal::restore(record(GoalStatus::Open, verdict)).unwrap()
    }

    /// ADR-t1504-2 decision 9: only an achieved goal opens again, without
    /// its verdict, and it can be closed again.
    #[test]
    fn only_an_achieved_goal_is_reopened() {
        let reopened = reopen(goal(Some(GoalVerdict::Achieved)), "later".into()).unwrap();
        assert!(!reopened.is_closed());
        assert_eq!(
            (reopened.status(), reopened.verdict(), reopened.updated_at()),
            (GoalStatus::Open, None, "later")
        );
        let counts = TaskStatusCounts::default();
        assert!(close(reopened, GoalVerdict::Achieved, &counts, "again".into()).is_ok());
        for verdict in [None, Some(GoalVerdict::Abandoned)] {
            let refused = reopen(goal(verdict), "later".into()).unwrap_err();
            assert_eq!(
                refused,
                DomainError::GoalNotReopenable {
                    goal_id: GoalId::new(7),
                    verdict
                }
            );
        }
        assert_eq!(
            reopen(goal(Some(GoalVerdict::Abandoned)), "later".into())
                .unwrap_err()
                .to_string(),
            "goal 7 is closed as abandoned; only a goal closed as achieved is opened again"
        );
    }

    #[test]
    fn a_stranded_dependency_names_its_task_goal_and_waiting_tasks() {
        let stranded = StrandedDependency {
            task_id: TaskId::new(3),
            goal_id: GoalId::new(7),
            verdict: GoalVerdict::Abandoned,
            waiting: vec![TaskId::new(5), TaskId::new(8)],
        };
        assert_eq!(
            stranded.summary(),
            "task 3 of goal 7 (closed abandoned) will not complete; tasks 5, 8 wait on it"
        );
    }

    #[test]
    fn a_new_goal_is_open_or_a_draft_and_not_closed() {
        let new = NewGoal {
            title: "g".into(),
            doc: Some(" ".into()),
            ..NewGoal::default()
        };
        let goal = Goal::new(GoalId::new(1), new.clone(), "now".into()).unwrap();
        assert_eq!(goal.status(), GoalStatus::Open);
        assert!(!goal.is_closed());
        assert_eq!(goal.doc(), None);
        assert_eq!((goal.created_at(), goal.updated_at()), ("now", "now"));
        let draft = Goal::new(
            GoalId::new(1),
            NewGoal {
                draft: true,
                doc: Some("docs/g.md".into()),
                ..new
            },
            "now".into(),
        )
        .unwrap();
        assert!(draft.is_draft());
        assert_eq!(draft.doc(), Some("docs/g.md"));
        assert_eq!(
            Goal::new(GoalId::new(1), NewGoal::default(), "now".into())
                .unwrap_err()
                .to_string(),
            "goal title must not be blank"
        );
        assert_eq!(
            serde_json::to_value(&draft).unwrap(),
            serde_json::json!({
                "id": 1, "title": "g", "description": "", "acceptance": "",
                "constraints": "", "doc": "docs/g.md", "priority": "normal", "tags": [],
                "priority_by": "ai", "origin": "unknown", "origin_kind": null,
                "origin_request_id": null, "status": "draft",
                "closed_at": null, "verdict": null,
                "created_at": "now", "updated_at": "now"
            })
        );
    }

    #[test]
    fn restore_checks_the_id_title_and_close() {
        let closed = goal(Some(GoalVerdict::Achieved));
        assert_eq!(closed.closed_at(), Some("2026-09-23T00:00:00Z"));
        assert_eq!(closed.verdict(), Some(GoalVerdict::Achieved));
        assert_eq!(
            Goal::restore(GoalRecord {
                verdict: None,
                ..record(GoalStatus::Open, Some(GoalVerdict::Achieved))
            })
            .unwrap_err()
            .to_string(),
            "goal 7 has a close time without a verdict or a verdict without a close time"
        );
        assert_eq!(
            Goal::restore(GoalRecord {
                title: " ".into(),
                ..record(GoalStatus::Open, None)
            })
            .unwrap_err(),
            GOAL_TITLE_BLANK
        );
        assert_eq!(
            Goal::restore(GoalRecord {
                id: GoalId::new(0),
                ..record(GoalStatus::Open, None)
            })
            .unwrap_err()
            .to_string(),
            "goal ID must be positive"
        );
    }

    fn tag(value: &str) -> GoalTag {
        value.parse().unwrap()
    }

    /// ADR-t1639-1 decision 6: the tags replace the whole list, each once,
    /// and change on a closed goal too.
    #[test]
    fn an_edit_replaces_the_tags_given_once() {
        let tagged = edit(
            goal(None),
            GoalEdit {
                tags: Some(vec![tag("codex"), tag("cmux")]),
                ..GoalEdit::default()
            },
        )
        .unwrap();
        assert_eq!(tagged.tags(), [tag("codex"), tag("cmux")]);
        let cleared = edit(
            tagged,
            GoalEdit {
                tags: Some(Vec::new()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
        assert!(cleared.tags().is_empty());
        assert_eq!(
            edit(
                cleared,
                GoalEdit {
                    tags: Some(vec![tag("a"), tag("a")]),
                    ..GoalEdit::default()
                },
            )
            .unwrap_err(),
            DomainError::GoalTagRepeated { tag: "a".into() }
        );
        let closed = edit(
            goal(Some(GoalVerdict::Abandoned)),
            GoalEdit {
                tags: Some(vec![tag("codex")]),
                ..GoalEdit::default()
            },
        )
        .unwrap();
        assert_eq!(closed.tags(), [tag("codex")]);
        assert!(
            Goal::restore(GoalRecord {
                tags: vec![tag("a"), tag("a")],
                ..record(GoalStatus::Open, None)
            })
            .is_err()
        );
        let new = NewGoal {
            title: "g".into(),
            description: String::new(),
            acceptance: String::new(),
            constraints: String::new(),
            doc: None,
            draft: false,
            priority: Some(Priority::Normal),
            tags: vec![tag("b"), tag("b")],
        };
        assert!(Goal::new(GoalId::new(1), new, "now".into()).is_err());
    }

    /// ADR-t1639-1 decision 7: unclosed goals first, then priority
    /// descending, then ID ascending; `--tag` keeps the goals with any.
    #[test]
    fn the_list_puts_unclosed_goals_first_by_priority_then_id() {
        let summary = |id: i64, priority: Priority, closed: bool, tags: &[&str]| GoalSummary {
            id: GoalId::new(id),
            title: format!("g{id}"),
            status: GoalStatus::Open,
            priority,
            tags: tags.iter().map(|t| tag(t)).collect(),
            closed,
            verdict: closed.then_some(GoalVerdict::Achieved),
            tasks: TaskStatusCounts::default(),
        };
        let goals = vec![
            summary(1, Priority::Low, false, &["codex"]),
            summary(2, Priority::Interrupt, true, &["codex"]),
            summary(3, Priority::High, false, &[]),
            summary(4, Priority::Normal, false, &["cmux", "throughput"]),
            summary(5, Priority::High, false, &["throughput"]),
            summary(6, Priority::Low, true, &[]),
        ];
        let ids = |listed: Vec<GoalSummary>| -> Vec<i64> {
            listed.iter().map(|goal| goal.id.as_i64()).collect()
        };
        assert_eq!(ids(list(goals.clone(), &[])), [3, 5, 4, 1, 2, 6]);
        assert_eq!(ids(list(goals.clone(), &[tag("codex")])), [1, 2]);
        assert_eq!(
            ids(list(goals.clone(), &[tag("throughput"), tag("codex")])),
            [5, 4, 1, 2]
        );
        assert!(list(goals, &[tag("enterprise")]).is_empty());
    }

    #[test]
    fn only_an_unclosed_draft_goal_becomes_ready() {
        let draft = Goal::restore(record(GoalStatus::Draft, None)).unwrap();
        assert!(draft.is_draft());
        assert_eq!(ready(draft).unwrap().status(), GoalStatus::Open);
        assert_eq!(
            ready(goal(None)).unwrap_err().to_string(),
            "goal 7 is not a draft"
        );
        let closed =
            Goal::restore(record(GoalStatus::Draft, Some(GoalVerdict::Abandoned))).unwrap();
        assert!(!closed.is_draft());
        assert_eq!(
            ready(closed).unwrap_err().to_string(),
            "goal 7 is already closed as abandoned"
        );
    }

    #[test]
    fn a_closed_goal_takes_no_tasks() {
        check_accepts_tasks(&goal(None)).unwrap();
        assert_eq!(
            check_accepts_tasks(&goal(Some(GoalVerdict::Abandoned)))
                .unwrap_err()
                .to_string(),
            "goal 7 is closed as abandoned; create a new goal for further work"
        );
    }

    #[test]
    fn only_an_abandoned_goal_takes_no_dependents() {
        check_accepts_dependents(&goal(None)).unwrap();
        check_accepts_dependents(&goal(Some(GoalVerdict::Achieved))).unwrap();
        assert_eq!(
            check_accepts_dependents(&goal(Some(GoalVerdict::Abandoned)))
                .unwrap_err()
                .to_string(),
            "goal 7 is closed as abandoned and never releases a task that depends on it"
        );
    }

    #[test]
    fn an_edit_replaces_the_given_fields() {
        let edited = edit(
            goal(None),
            GoalEdit {
                title: Some("new".into()),
                description: Some("d".into()),
                acceptance: Some("a".into()),
                constraints: Some("c".into()),
                doc: Some("x.md".into()),
                priority: None,
                tags: None,
            },
        )
        .unwrap();
        assert_eq!(
            (
                edited.title(),
                edited.description(),
                edited.acceptance(),
                edited.constraints(),
                edited.doc()
            ),
            ("new", "d", "a", "c", Some("x.md"))
        );
        let cleared = edit(
            edited,
            GoalEdit {
                doc: Some(String::new()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
        assert_eq!(cleared.doc(), None);
        assert_eq!(cleared.into_title(), "new");
        assert_eq!(
            edit(
                goal(None),
                GoalEdit {
                    title: Some(" ".into()),
                    ..GoalEdit::default()
                }
            )
            .unwrap_err()
            .to_string(),
            "goal title must not be blank"
        );
    }

    /// ADR-t1639-1 decision 1: a draft or open goal takes another
    /// priority; a closed one refuses it but keeps its other edits.
    #[test]
    fn only_an_unclosed_goal_changes_its_priority() {
        let high = GoalEdit {
            priority: Some(Priority::High),
            ..GoalEdit::default()
        };
        assert_eq!(goal(None).priority(), Priority::Normal);
        assert_eq!(
            edit(goal(None), high.clone()).unwrap().priority(),
            Priority::High
        );
        let draft = Goal::restore(record(GoalStatus::Draft, None)).unwrap();
        assert_eq!(
            edit(draft, high.clone()).unwrap().priority(),
            Priority::High
        );
        assert_eq!(
            edit(goal(Some(GoalVerdict::Achieved)), high)
                .unwrap_err()
                .to_string(),
            "goal 7 is closed as achieved; create a new goal for further work"
        );
        let renamed = edit(
            goal(Some(GoalVerdict::Abandoned)),
            GoalEdit {
                title: Some("t".into()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
        assert_eq!(renamed.title(), "t");
    }

    #[test]
    fn only_achieved_waits_for_the_follow_ups_membership() {
        use crate::domain::follow_up::{MembershipClassification, SourceFollowUp};
        let unjudged = SourceFollowUp {
            task: crate::domain::TaskId::new(9),
            status: TaskStatus::Draft,
            in_goal: false,
            judgement: None,
        };
        let goal = GoalId::new(7);
        assert_eq!(
            check_follow_ups(goal, GoalVerdict::Achieved, std::slice::from_ref(&unjudged))
                .unwrap_err()
                .to_string(),
            "goal 7 cannot be closed as achieved: its follow-up(s) 9 not judged; \
             record their membership with judge-follow-up"
        );
        assert!(
            check_follow_ups(
                goal,
                GoalVerdict::Abandoned,
                std::slice::from_ref(&unjudged)
            )
            .is_ok()
        );
        let out_of_scope = SourceFollowUp {
            judgement: Some((1, MembershipClassification::OutOfScope, false)),
            ..unjudged
        };
        assert!(check_follow_ups(goal, GoalVerdict::Achieved, &[out_of_scope]).is_ok());
    }

    #[test]
    fn a_goal_closes_once_and_only_when_its_tasks_allow_the_verdict() {
        let mut counts = TaskStatusCounts::default();
        counts.count(TaskStatus::Submitted, 1);
        counts.count(TaskStatus::Ready, 2);
        counts.count(TaskStatus::InProgress, 1);
        assert_eq!((counts.total, counts.submitted), (4, 1));
        let at = || "2026-09-24T00:00:00Z".to_owned();
        assert_eq!(
            close(goal(None), GoalVerdict::Achieved, &counts, at())
                .unwrap_err()
                .to_string(),
            "goal 7 cannot be closed as achieved: 1 task(s) submitted, 2 task(s) ready, \
             1 task(s) in_progress"
        );
        assert_eq!(
            close(goal(None), GoalVerdict::Abandoned, &counts, at()).unwrap_err(),
            DomainError::GoalCloseBlocked {
                goal_id: GoalId::new(7),
                verdict: GoalVerdict::Abandoned,
                blocking: vec![(TaskStatus::InProgress, 1)]
            }
        );
        counts.in_progress = 0;
        let closed = close(goal(None), GoalVerdict::Abandoned, &counts, at()).unwrap();
        assert!(closed.is_closed());
        assert_eq!(closed.verdict(), Some(GoalVerdict::Abandoned));
        assert_eq!(closed.closed_at(), Some("2026-09-24T00:00:00Z"));
        assert_eq!(closed.updated_at(), "2026-09-24T00:00:00Z");
        assert_eq!(
            close(closed, GoalVerdict::Achieved, &counts, at())
                .unwrap_err()
                .to_string(),
            "goal 7 is already closed as abandoned"
        );
    }
}
