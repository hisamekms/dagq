use serde_json::{Value, json};

use super::*;
use crate::domain::marks::utc_text;
use crate::domain::{EventId, GoalTag};

/// The fold at `window_end` as the ledger's window gives it, which is the
/// fold of the events themselves: every test here holds of the rows.
fn fold(events: &[RunEvent], window_end: Option<i64>) -> Folded {
    let direct = super::fold(events, window_end);
    let window = crate::domain::ledger::window::through_ledger(events, window_end);
    let through = window.attributes();
    assert_eq!(through, direct);
    through
}

const T0: i64 = 1_900_000_000;
const RUN: &str = "11111111-1111-4111-8111-111111111111";

/// The events of a queue as a test lays them down, one second apart.
#[derive(Default)]
struct Log {
    events: Vec<RunEvent>,
}

impl Log {
    fn push(&mut self, task: Option<i64>, goal: Option<i64>, kind: &str, payload: Value) {
        let n = i64::try_from(self.events.len()).unwrap() + 1;
        self.events.push(RunEvent {
            id: EventId::new(n),
            task_id: task.map(TaskId::new),
            goal_id: goal.map(GoalId::new),
            run_id: (kind == event_kind::RUN_CLAIMED || kind == event_kind::RUN_UNFINISHED_CHANGED)
                .then(|| RunId::new(RUN).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: utc_text((T0 + n) * 1000),
            actor: None,
        });
    }

    fn task(&mut self, task: i64, kind: &str, payload: Value) -> &mut Self {
        self.push(Some(task), None, kind, payload);
        self
    }

    fn goal(&mut self, goal: i64, kind: &str, payload: Value) -> &mut Self {
        self.push(None, Some(goal), kind, payload);
        self
    }

    fn folded(&self) -> Attributes {
        match fold(&self.events, None) {
            Folded::Attributes(attributes) => attributes,
            Folded::BeforeBaseline => panic!("before the baseline"),
        }
    }
}

fn goal_json(title: &str, status: &str, priority: &str) -> Value {
    json!({"id": 1, "title": title, "status": status, "priority": priority,
        "priority_by": "ai", "tags": ["measure"], "verdict": null, "closed_at": null})
}

fn created(title: &str, goal: Option<i64>) -> Value {
    json!({"goal_id": goal, "status": "draft", "title": title, "change": "runtime",
        "execution_class": "implementation", "priority": null, "priority_by": null})
}

#[test]
fn creation_records_and_later_changes_fold_to_the_latest_values() {
    let mut log = Log::default();
    log.goal(
        1,
        event_kind::GOAL_CREATED,
        json!({"goal": goal_json("g", "open", "high")}),
    )
    .task(10, event_kind::TASK_CREATED, created("first", Some(1)))
    .task(11, event_kind::TASK_CREATED, created("second", None))
    .task(
        11,
        event_kind::DEPENDENCY_ADDED,
        json!({"predecessor_id": 10}),
    )
    .task(
        10,
        event_kind::TASK_EDITED,
        json!({"from": {}, "to": {"title": "renamed", "change": null, "execution_class": "spike"}}),
    )
    .task(
        10,
        event_kind::TASK_STATUS_CHANGED,
        json!({"from": "draft", "to": "ready"}),
    )
    .task(
        11,
        event_kind::DRAFT_ORIGIN_RECORDED,
        json!({"origin": "follow_up"}),
    )
    .task(
        11,
        event_kind::TASK_GOAL_CHANGED,
        json!({"from": null, "to": 1}),
    );
    let folded = log.folded();
    let first = &folded.tasks[&TaskId::new(10)];
    assert_eq!(first.title, "renamed");
    assert_eq!(first.change, None);
    assert_eq!(first.execution_class, ExecutionClass::Spike);
    assert_eq!(first.status, TaskStatus::Ready);
    let second = &folded.tasks[&TaskId::new(11)];
    assert_eq!(second.change, Some("runtime".parse().unwrap()));
    assert_eq!(second.goal_id, Some(GoalId::new(1)));
    assert_eq!(
        folded.draft_origins()[&TaskId::new(11)],
        DraftOrigin::FollowUp
    );
    assert_eq!(
        folded.task_goals(),
        HashMap::from([
            (TaskId::new(10), Some(GoalId::new(1))),
            (TaskId::new(11), Some(GoalId::new(1)))
        ])
    );
    // Both inherit the goal's priority.
    assert_eq!(
        folded.base_priority(second),
        (Priority::High, PrioritySource::Goal, PriorityBy::Ai)
    );
    let summaries = folded.goal_summaries();
    assert_eq!(summaries.len(), 1);
    assert_eq!(
        summaries[0].tags,
        vec!["measure".parse::<GoalTag>().unwrap()]
    );
    assert_eq!((summaries[0].tasks.ready, summaries[0].tasks.draft), (1, 1));
}

#[test]
fn the_candidates_follow_the_ready_query() {
    let mut log = Log::default();
    log.goal(
        1,
        event_kind::GOAL_CREATED,
        json!({"goal": goal_json("g", "open", "normal")}),
    )
    .goal(
        2,
        event_kind::GOAL_CREATED,
        json!({"goal": goal_json("d", "draft", "normal")}),
    )
    .task(10, event_kind::TASK_CREATED, created("a", None))
    .task(11, event_kind::TASK_CREATED, created("b", None))
    .task(12, event_kind::TASK_CREATED, created("c", Some(2)))
    .task(13, event_kind::TASK_CREATED, created("e", None));
    for task in [10, 11, 12, 13] {
        log.task(
            task,
            event_kind::TASK_STATUS_CHANGED,
            json!({"to": "ready"}),
        );
    }
    log.task(
        11,
        event_kind::DEPENDENCY_ADDED,
        json!({"predecessor_id": 10}),
    )
    .task(13, event_kind::GOAL_DEPENDENCY_ADDED, json!({"goal_id": 1}));
    // 11 waits for 10, 12 is in a draft goal and 13 waits for goal 1.
    assert_eq!(log.folded().candidates(), vec![TaskId::new(10)]);
    log.task(10, event_kind::RUN_CLAIMED, json!({}));
    assert!(log.folded().candidates().is_empty());
    log.task(
        10,
        event_kind::RUN_UNFINISHED_CHANGED,
        json!({"unfinished": false, "from": "integrating", "to": "integrated"}),
    )
    .task(
        10,
        event_kind::TASK_STATUS_CHANGED,
        json!({"to": "completed"}),
    )
    .goal(1, event_kind::GOAL_CLOSED, json!({"verdict": "achieved"}));
    assert_eq!(
        log.folded().candidates(),
        vec![TaskId::new(11), TaskId::new(13)]
    );
    // Only a goal closed as achieved is reopened (ADR-t1504-2 decision 9):
    // its dependents wait again.
    log.goal(1, event_kind::GOAL_REOPENED, json!({}));
    let reopened = log.folded();
    assert!(!reopened.goals[&GoalId::new(1)].closed);
    assert_eq!(reopened.goals[&GoalId::new(1)].verdict, None);
    assert_eq!(reopened.candidates(), vec![TaskId::new(11)]);
    log.goal(1, event_kind::GOAL_CLOSED, json!({"verdict": "achieved"}))
        .goal(
            2,
            event_kind::GOAL_STATUS_CHANGED,
            json!({"from": "draft", "to": "open"}),
        );
    assert_eq!(
        log.folded().candidates(),
        vec![TaskId::new(11), TaskId::new(12), TaskId::new(13)]
    );
}

#[test]
fn own_priorities_and_goal_priorities_resolve_as_the_store_does() {
    let mut log = Log::default();
    log.goal(
        1,
        event_kind::GOAL_CREATED,
        json!({"goal": goal_json("g", "open", "low")}),
    )
    .task(10, event_kind::TASK_CREATED, created("a", Some(1)))
    .task(
        10,
        event_kind::TASK_PRIORITY_CHANGED,
        json!({"from": "low", "to": "urgent", "from_source": "goal", "to_source": "task",
                "own": "urgent", "own_by": "ai"}),
    );
    let folded = log.folded();
    let task = &folded.tasks[&TaskId::new(10)];
    assert_eq!(
        folded.base_priority(task),
        (Priority::Urgent, PrioritySource::Task, PriorityBy::Ai)
    );
    // `new` keeps the setter from before; the stored one wins.
    let mut new = goal_json("g2", "open", "high");
    new["priority_by"] = json!("ai");
    log.goal(
        1,
        event_kind::GOAL_UPDATED,
        json!({"old": {}, "new": new, "priority_by": "human"}),
    )
    .task(
        10,
        event_kind::TASK_PRIORITY_CHANGED,
        json!({"from": "urgent", "to": "high", "from_source": "task", "to_source": "goal",
                "own": null, "own_by": null}),
    );
    let folded = log.folded();
    assert_eq!(folded.goals[&GoalId::new(1)].title, "g2");
    assert_eq!(
        folded.base_priority(&folded.tasks[&TaskId::new(10)]),
        (Priority::High, PrioritySource::Goal, PriorityBy::Human)
    );
    // A binary before the record: the own priority from `to`, the setter
    // unknown (read as a person's).
    log.task(
        10,
        event_kind::TASK_PRIORITY_CHANGED,
        json!({"from": "high", "to": "low", "from_source": "goal", "to_source": "task"}),
    );
    let folded = log.folded();
    assert_eq!(
        folded.base_priority(&folded.tasks[&TaskId::new(10)]),
        (Priority::Low, PrioritySource::Task, PriorityBy::Human)
    );
}

#[test]
fn a_task_without_its_attributes_is_known_from_its_baseline_and_earlier_windows_are_before_it() {
    let mut log = Log::default();
    // An older binary's creation record, and a change before the baseline.
    log.task(10, event_kind::TASK_CREATED, json!({"goal_id": null}))
        .task(10, event_kind::TASK_STATUS_CHANGED, json!({"to": "ready"}));
    assert!(log.folded().tasks.is_empty());
    log.goal(
        1,
        event_kind::ATTRIBUTES_BASELINE,
        json!({"goal": goal_json("g", "open", "normal")}),
    )
    .task(
        10,
        event_kind::ATTRIBUTES_BASELINE,
        json!({"status": "ready", "goal_id": null, "title": "old", "change": "nonsense label!",
                "execution_class": "unknown", "priority": "high", "priority_by": null,
                "origin": "goal_gap", "depends_on": [3], "goal_dependencies": [1],
                "unfinished_run": RUN}),
    );
    // The mark closes the group, after its records.
    log.push(
        None,
        None,
        event_kind::ATTRIBUTES_BASELINE,
        json!({"tasks": 1, "goals": 1}),
    );
    log.task(
        10,
        event_kind::DEPENDENCY_REMOVED,
        json!({"predecessor_id": 3}),
    );
    let folded = log.folded();
    let task = &folded.tasks[&TaskId::new(10)];
    assert_eq!(task.title, "old");
    assert_eq!(task.change, None);
    assert_eq!(task.execution_class, ExecutionClass::Implementation);
    assert!(task.depends_on.is_empty());
    assert_eq!(task.goal_dependencies, BTreeSet::from([GoalId::new(1)]));
    assert_eq!(task.unfinished_run, Some(RunId::new(RUN).unwrap()));
    assert_eq!(task.origin, Some(DraftOrigin::GoalGap));
    assert_eq!(
        folded.base_priority(task),
        (Priority::High, PrioritySource::Task, PriorityBy::Human)
    );
}

/// A baseline is taken whole or not at all: a window that holds its
/// records but not the mark that closes it does not hold the baseline (the
/// first is before the baseline, a later one keeps the values from before
/// it), and a window that holds the mark holds every record of the group,
/// even one stamped after the mark.
#[test]
fn a_window_holds_a_baseline_whole_or_not_at_all() {
    let task = |title: &str| {
        json!({"status": "ready", "goal_id": 1, "title": title, "change": null,
            "execution_class": "implementation", "priority": null, "priority_by": null,
            "origin": null, "depends_on": [], "goal_dependencies": [], "unfinished_run": null})
    };
    let mark = |log: &mut Log| {
        log.push(
            None,
            None,
            event_kind::ATTRIBUTES_BASELINE,
            json!({"tasks": 1, "goals": 1}),
        );
    };
    let mut log = Log::default();
    // The first group: events 1 and 2 at T0 + 1 and T0 + 2, the mark at
    // T0 + 3.
    log.goal(
        1,
        event_kind::ATTRIBUTES_BASELINE,
        json!({"goal": goal_json("goal", "open", "high")}),
    )
    .task(10, event_kind::ATTRIBUTES_BASELINE, task("first"));
    mark(&mut log);
    assert_eq!(fold(&log.events, Some(T0 + 2)), Folded::BeforeBaseline);
    let Folded::Attributes(first) = fold(&log.events, Some(T0 + 3)) else {
        panic!("the first group's mark is in the window")
    };
    assert_eq!(first.tasks[&TaskId::new(10)].title, "first");
    assert_eq!(first.goals[&GoalId::new(1)].title, "goal");
    assert_eq!(
        first.base_priority(&first.tasks[&TaskId::new(10)]),
        (Priority::High, PrioritySource::Goal, PriorityBy::Ai)
    );
    assert_eq!(first.candidates(), vec![TaskId::new(10)]);
    // A later group (events 4 and 5, the mark at T0 + 6): a window that
    // ends inside it keeps the values from before it.
    log.goal(
        1,
        event_kind::ATTRIBUTES_BASELINE,
        json!({"goal": goal_json("goal, renamed", "draft", "low")}),
    )
    .task(10, event_kind::ATTRIBUTES_BASELINE, task("second"));
    mark(&mut log);
    for end in [T0 + 4, T0 + 5] {
        let Folded::Attributes(inside) = fold(&log.events, Some(end)) else {
            panic!("after the first mark")
        };
        assert_eq!(inside, first, "window ending at T0 + {}", end - T0);
    }
    let Folded::Attributes(second) = fold(&log.events, Some(T0 + 6)) else {
        panic!("after the first mark")
    };
    assert_eq!(second.tasks[&TaskId::new(10)].title, "second");
    assert_eq!(second.goals[&GoalId::new(1)].title, "goal, renamed");
    assert!(second.candidates().is_empty());
    // A record stamped after its group's mark (a clock that moved back)
    // still comes with the mark.
    log.task(10, event_kind::ATTRIBUTES_BASELINE, task("third"));
    mark(&mut log);
    let late = log.events.len() - 2;
    log.events[late].created_at = utc_text((T0 + 20) * 1000);
    let Folded::Attributes(third) = fold(&log.events, Some(T0 + 8)) else {
        panic!("after the first mark")
    };
    assert_eq!(third.tasks[&TaskId::new(10)].title, "third");
}

/// The supervisor compares at once, then not until the interval has
/// passed: 1 ms before it is too early, the interval itself is due.
#[test]
fn a_comparison_is_due_first_and_then_once_the_interval_has_passed() {
    use std::time::Duration;
    let interval = Duration::from_secs(RECONCILE_INTERVAL_SECS);
    assert!(reconcile_due(None));
    assert!(!reconcile_due(Some(Duration::ZERO)));
    assert!(!reconcile_due(Some(interval - Duration::from_millis(1))));
    assert!(reconcile_due(Some(interval)));
    assert!(reconcile_due(Some(interval * 2)));
}
