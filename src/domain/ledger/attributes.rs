//! The attributes of the tasks and goals that `stats`, `kpi` and `report`
//! read of the planning tables, folded from the EventStore alone
//! (ADR-t1662-2 decision 3, docs/design/attribute-records.md): each
//! task's status, goal, title, change, execution class, own priority and
//! who set it, draft origin, predecessors, goal dependencies and
//! unfinished run, and each goal's title, status, priority and who set it,
//! tags and verdict. A record carries the new values, a later one wins,
//! and a task or goal whose creation record does not carry them is known
//! from its `attributes_baseline` on. Measurement only: no decision of the
//! runtime reads it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use self::payload::{
    DependencyChanged, GoalChanged, GoalClosed, GoalCreated, GoalDependencyChanged, GoalRecord,
    GoalUpdated, OriginRecorded, PriorityChanged, StatusChanged, TaskEdited, TaskRecord,
    UnfinishedChanged, restore,
};
use super::super::stats::timestamp_millis;
use super::super::{
    DraftOrigin, ExecutionClass, GoalId, GoalStatus, GoalSummary, GoalTag, GoalVerdict, Priority,
    PrioritySource, RunEvent, RunId, TaskChange, TaskId, TaskStatus, TaskStatusCounts,
    base_priority, event_kind, plan_request::PriorityBy,
};

/// A task's attributes as its records leave them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskAttributes {
    pub status: TaskStatus,
    pub goal_id: Option<GoalId>,
    pub title: String,
    /// None for none, and for a value that is not a label (read as the
    /// store reads it).
    pub change: Option<TaskChange>,
    pub execution_class: ExecutionClass,
    /// Its own priority; none inherits its goal's.
    pub own_priority: Option<Priority>,
    /// Who set its own priority as stored (none read as a person's); none
    /// without an own priority, as the store reads it.
    pub own_priority_by: Option<PriorityBy>,
    /// Where the draft came from (`draft_origins`); none for an origin this
    /// binary does not know.
    pub origin: Option<DraftOrigin>,
    pub depends_on: BTreeSet<TaskId>,
    pub goal_dependencies: BTreeSet<GoalId>,
    /// Its run in an unfinished status ([`super::super::RunStatus::is_unfinished`]).
    pub unfinished_run: Option<RunId>,
}

/// A goal's attributes as its records leave them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalAttributes {
    pub title: String,
    pub status: GoalStatus,
    pub priority: Priority,
    pub priority_by: PriorityBy,
    pub tags: Vec<GoalTag>,
    pub verdict: Option<GoalVerdict>,
    pub closed: bool,
}

/// The folded attributes of every task and goal known from the records.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attributes {
    pub tasks: BTreeMap<TaskId, TaskAttributes>,
    pub goals: BTreeMap<GoalId, GoalAttributes>,
}

/// What [`fold`] returns: the attributes, or that the window ends before
/// the first baseline, when the records do not hold them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Folded {
    BeforeBaseline,
    Attributes(Attributes),
}

/// How often the supervisor compares the records with the state tables
/// and writes a baseline for what differs (a binary before the record
/// writes without them while it runs on).
pub const RECONCILE_INTERVAL_SECS: u64 = 600;

/// Whether the supervisor compares the records with the state now, given
/// how long ago it last did (none before its first comparison): on its
/// first pass and once [`RECONCILE_INTERVAL_SECS`] have passed, whatever
/// the last comparison's outcome.
pub fn reconcile_due(since_last: Option<std::time::Duration>) -> bool {
    since_last.is_none_or(|since| since.as_secs() >= RECONCILE_INTERVAL_SECS)
}

/// The kinds [`fold`] reads: a reader may give it only these.
pub const KINDS: &[&str] = &[
    event_kind::ATTRIBUTES_BASELINE,
    event_kind::TASK_CREATED,
    event_kind::TASK_EDITED,
    event_kind::TASK_STATUS_CHANGED,
    event_kind::RUN_CLAIMED,
    event_kind::RUN_UNFINISHED_CHANGED,
    event_kind::TASK_GOAL_CHANGED,
    event_kind::TASK_PRIORITY_CHANGED,
    event_kind::DEPENDENCY_ADDED,
    event_kind::DEPENDENCY_REMOVED,
    event_kind::GOAL_DEPENDENCY_ADDED,
    event_kind::GOAL_DEPENDENCY_REMOVED,
    event_kind::DRAFT_ORIGIN_RECORDED,
    event_kind::GOAL_CREATED,
    event_kind::GOAL_UPDATED,
    event_kind::GOAL_STATUS_CHANGED,
    event_kind::GOAL_CLOSED,
    event_kind::GOAL_REOPENED,
];

/// The attributes at `window_end` (unix seconds; none for every event)
/// from `events` (ascending id) alone. A baseline is one group: the task's
/// and goal's `attributes_baseline` and, after them, the queue's mark that
/// closes it. A window takes a group's records only when it holds the
/// group's mark, so it never holds part of a baseline, whatever the
/// records' times. A window that ends before the first mark is
/// [`Folded::BeforeBaseline`]. Reads nothing but `events`.
pub fn fold(events: &[RunEvent], window_end: Option<i64>) -> Folded {
    let end_ms = window_end.map(|end| end * 1000);
    let within = |event: &RunEvent| {
        end_ms.is_none_or(|end| timestamp_millis(&event.created_at).is_some_and(|at| at <= end))
    };
    let is_mark = |event: &RunEvent| {
        event.kind == event_kind::ATTRIBUTES_BASELINE
            && event.task_id.is_none()
            && event.goal_id.is_none()
    };
    if events
        .iter()
        .find(|event| is_mark(event))
        .is_some_and(|mark| !within(mark))
    {
        return Folded::BeforeBaseline;
    }
    // Whether the mark that closes each baseline record's group is in the
    // window (a record with no mark after it stands alone).
    let mut taken = vec![true; events.len()];
    let mut closing: Option<bool> = None;
    for (index, event) in events.iter().enumerate().rev() {
        if is_mark(event) {
            closing = Some(within(event));
        } else if event.kind == event_kind::ATTRIBUTES_BASELINE {
            taken[index] = closing.unwrap_or_else(|| within(event));
        }
    }
    let mut attributes = Attributes::default();
    for (event, _) in events
        .iter()
        .zip(&taken)
        .filter(|(event, taken)| **taken && (within(event) || is_baseline(event)))
    {
        match event.task_id {
            Some(task) => attributes.apply_task(task, event),
            None => {
                if let Some(goal) = event.goal_id {
                    attributes.apply_goal(goal, event);
                }
            }
        }
    }
    Folded::Attributes(attributes)
}

/// A task's or goal's `attributes_baseline` (not the queue's mark).
fn is_baseline(event: &RunEvent) -> bool {
    event.kind == event_kind::ATTRIBUTES_BASELINE
        && (event.task_id.is_some() || event.goal_id.is_some())
}

impl Attributes {
    fn apply_task(&mut self, id: TaskId, event: &RunEvent) {
        let payload = &event.payload;
        let kind = event.kind.as_str();
        if kind == event_kind::TASK_CREATED || kind == event_kind::ATTRIBUTES_BASELINE {
            // A creation record of a binary before the record carries no
            // title: the task is known from its baseline on.
            if let Some(task) = created(restore(payload)) {
                self.tasks.insert(id, task);
            }
            return;
        }
        let Some(task) = self.tasks.get_mut(&id) else {
            return;
        };
        match kind {
            event_kind::TASK_EDITED => {
                let to = restore::<TaskEdited>(payload).to.unwrap_or_default();
                if let Some(title) = to.title {
                    task.title = title;
                }
                if let Some(change) = to.change {
                    task.change = change.as_deref().and_then(parsed);
                }
                if let Some(class) = to.execution_class {
                    task.execution_class = class_of(Some(&class));
                }
            }
            event_kind::TASK_STATUS_CHANGED => {
                if let Some(status) = restore::<StatusChanged>(payload)
                    .to
                    .as_deref()
                    .and_then(parsed)
                {
                    task.status = status;
                }
            }
            event_kind::RUN_CLAIMED => {
                task.status = TaskStatus::InProgress;
                task.unfinished_run = event.run_id.clone();
            }
            event_kind::RUN_UNFINISHED_CHANGED => {
                if restore::<UnfinishedChanged>(payload).unfinished == Some(true) {
                    task.unfinished_run = event.run_id.clone();
                } else if task.unfinished_run == event.run_id {
                    task.unfinished_run = None;
                }
            }
            event_kind::TASK_GOAL_CHANGED => {
                task.goal_id = restore::<GoalChanged>(payload).to.map(GoalId::new);
            }
            event_kind::TASK_PRIORITY_CHANGED => {
                let changed = restore::<PriorityChanged>(payload);
                match changed.own {
                    Some(own) => {
                        task.own_priority = own.as_deref().and_then(parsed);
                        task.own_priority_by = task
                            .own_priority
                            .and(changed.own_by.as_deref().and_then(parsed));
                    }
                    None => {
                        // A binary before the record: the own priority is
                        // the new one when it is the task's; who set it is
                        // unknown.
                        task.own_priority = (changed.to_source.as_deref()
                            == Some(PrioritySource::Task.as_str()))
                        .then(|| changed.to.as_deref().and_then(parsed))
                        .flatten();
                        task.own_priority_by = None;
                    }
                }
            }
            event_kind::DEPENDENCY_ADDED | event_kind::DEPENDENCY_REMOVED => {
                if let Some(predecessor) = restore::<DependencyChanged>(payload).predecessor_id {
                    let predecessor = TaskId::new(predecessor);
                    if kind == event_kind::DEPENDENCY_ADDED {
                        task.depends_on.insert(predecessor);
                    } else {
                        task.depends_on.remove(&predecessor);
                    }
                }
            }
            event_kind::GOAL_DEPENDENCY_ADDED | event_kind::GOAL_DEPENDENCY_REMOVED => {
                if let Some(goal) = restore::<GoalDependencyChanged>(payload).goal_id {
                    let goal = GoalId::new(goal);
                    if kind == event_kind::GOAL_DEPENDENCY_ADDED {
                        task.goal_dependencies.insert(goal);
                    } else {
                        task.goal_dependencies.remove(&goal);
                    }
                }
            }
            event_kind::DRAFT_ORIGIN_RECORDED => {
                task.origin = restore::<OriginRecorded>(payload)
                    .origin
                    .as_deref()
                    .and_then(parsed);
            }
            _ => {}
        }
    }

    fn apply_goal(&mut self, id: GoalId, event: &RunEvent) {
        let payload = &event.payload;
        match event.kind.as_str() {
            event_kind::GOAL_CREATED | event_kind::ATTRIBUTES_BASELINE => {
                if let Some(goal) = restore::<GoalCreated>(payload).goal.and_then(goal_of) {
                    self.goals.insert(id, goal);
                }
            }
            event_kind::GOAL_UPDATED => {
                let updated = restore::<GoalUpdated>(payload);
                if let Some(mut goal) = updated.new.and_then(goal_of) {
                    if let Some(by) = updated.priority_by.as_deref().and_then(parsed) {
                        goal.priority_by = by;
                    }
                    self.goals.insert(id, goal);
                }
            }
            event_kind::GOAL_STATUS_CHANGED => {
                let status = restore::<StatusChanged>(payload)
                    .to
                    .as_deref()
                    .and_then(parsed);
                if let (Some(goal), Some(status)) = (self.goals.get_mut(&id), status) {
                    goal.status = status;
                }
            }
            event_kind::GOAL_CLOSED => {
                if let Some(goal) = self.goals.get_mut(&id) {
                    goal.verdict = restore::<GoalClosed>(payload)
                        .verdict
                        .as_deref()
                        .and_then(parsed);
                    goal.closed = true;
                }
            }
            event_kind::GOAL_REOPENED => {
                if let Some(goal) = self.goals.get_mut(&id) {
                    goal.status = GoalStatus::Open;
                    goal.verdict = None;
                    goal.closed = false;
                }
            }
            _ => {}
        }
    }

    /// The goal of every task, as `PlanningRecords::task_goals` gives it.
    pub fn task_goals(&self) -> HashMap<TaskId, Option<GoalId>> {
        self.tasks
            .iter()
            .map(|(&id, task)| (id, task.goal_id))
            .collect()
    }

    /// The title of every task, as `PlanningRecords::task_titles`.
    pub fn task_titles(&self) -> HashMap<TaskId, String> {
        self.tasks
            .iter()
            .map(|(&id, task)| (id, task.title.clone()))
            .collect()
    }

    /// The change of every task, as `PlanningRecords::task_changes`.
    pub fn task_changes(&self) -> HashMap<TaskId, Option<TaskChange>> {
        self.tasks
            .iter()
            .map(|(&id, task)| (id, task.change.clone()))
            .collect()
    }

    /// Where every draft with a known origin came from, as
    /// `PlanningRecords::draft_origins`.
    pub fn draft_origins(&self) -> HashMap<TaskId, DraftOrigin> {
        self.tasks
            .iter()
            .filter_map(|(&id, task)| Some((id, task.origin?)))
            .collect()
    }

    /// Every goal in ID order with its tasks counted by status, as
    /// `TaskStore::list_goals` gives it.
    pub fn goal_summaries(&self) -> Vec<GoalSummary> {
        self.goals
            .iter()
            .map(|(&id, goal)| {
                let mut tasks = TaskStatusCounts::default();
                for task in self.tasks.values().filter(|task| task.goal_id == Some(id)) {
                    tasks.count(task.status, 1);
                }
                GoalSummary {
                    id,
                    title: goal.title.clone(),
                    status: goal.status,
                    priority: goal.priority,
                    tags: goal.tags.clone(),
                    closed: goal.closed,
                    verdict: goal.verdict,
                    tasks,
                }
            })
            .collect()
    }

    /// The task's base priority, where it comes from and who set it: its
    /// own, else its goal's, else `normal` (the store's rule, ADR-t1639-1
    /// decision 2 and ADR-t1975-1 decision 2).
    pub fn base_priority(&self, task: &TaskAttributes) -> (Priority, PrioritySource, PriorityBy) {
        let goal = task.goal_id.and_then(|id| self.goals.get(&id));
        let (priority, source) = base_priority(task.own_priority, goal.map(|goal| goal.priority));
        let by = PriorityBy::effective(
            task.own_priority,
            task.own_priority_by,
            goal.map(|goal| goal.priority_by),
        );
        (priority, source, by)
    }

    /// The claimable tasks in ID order, by the store's ready query: ready,
    /// not in a draft goal, every predecessor completed, every goal
    /// depended on closed as achieved, and no unfinished run.
    pub fn candidates(&self) -> Vec<TaskId> {
        self.tasks
            .iter()
            .filter(|(_, task)| {
                task.status == TaskStatus::Ready
                    && task
                        .goal_id
                        .and_then(|id| self.goals.get(&id))
                        .is_none_or(|goal| goal.status != GoalStatus::Draft)
                    && task.depends_on.iter().all(|predecessor| {
                        self.tasks
                            .get(predecessor)
                            .is_some_and(|p| p.status == TaskStatus::Completed)
                    })
                    && task.goal_dependencies.iter().all(|goal| {
                        self.goals.get(goal).is_some_and(|goal| {
                            goal.closed && goal.verdict == Some(GoalVerdict::Achieved)
                        })
                    })
                    && task.unfinished_run.is_none()
            })
            .map(|(&id, _)| id)
            .collect()
    }
}

/// A task from its creation record or its baseline; none without a title.
fn created(record: TaskRecord) -> Option<TaskAttributes> {
    let own_priority = record.priority.as_deref().and_then(parsed);
    Some(TaskAttributes {
        status: record
            .status
            .as_deref()
            .and_then(parsed)
            .unwrap_or(TaskStatus::Draft),
        goal_id: record.goal_id.map(GoalId::new),
        title: record.title?,
        change: record.change.as_deref().and_then(parsed),
        execution_class: class_of(record.execution_class.as_deref()),
        own_priority,
        own_priority_by: own_priority.and(record.priority_by.as_deref().and_then(parsed)),
        origin: record.origin.as_deref().and_then(parsed),
        depends_on: record
            .depends_on
            .into_iter()
            .flatten()
            .map(TaskId::new)
            .collect(),
        goal_dependencies: record
            .goal_dependencies
            .into_iter()
            .flatten()
            .map(GoalId::new)
            .collect(),
        unfinished_run: record.unfinished_run.and_then(|id| RunId::new(id).ok()),
    })
}

/// A goal from its JSON; none without a title or a known status.
fn goal_of(goal: GoalRecord) -> Option<GoalAttributes> {
    Some(GoalAttributes {
        title: goal.title?,
        status: goal.status.as_deref().and_then(parsed)?,
        priority: goal
            .priority
            .as_deref()
            .and_then(parsed)
            .unwrap_or_default(),
        priority_by: goal
            .priority_by
            .as_deref()
            .and_then(parsed)
            .unwrap_or(PriorityBy::Ai),
        tags: goal
            .tags
            .into_iter()
            .flatten()
            .filter_map(|tag| tag.parse().ok())
            .collect(),
        verdict: goal.verdict.as_deref().and_then(parsed),
        closed: goal.closed_at.is_some(),
    })
}

/// A recorded name read as `T`; one this binary does not know is absent.
fn parsed<T: std::str::FromStr>(text: &str) -> Option<T> {
    text.parse().ok()
}

/// Read as the store reads the column: an unknown class is the default.
fn class_of(class: Option<&str>) -> ExecutionClass {
    class.and_then(parsed).unwrap_or_default()
}

mod payload;

#[cfg(test)]
mod tests;
