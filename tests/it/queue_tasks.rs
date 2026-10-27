//! Queue tests: editing a task (paths, fields, priority), the priority order,
//! and `list`.
use crate::common;
use dagq::domain::EventKind;

use dagq::{
    application::{
        StatusFilter, TaskQuery, TaskReads, TaskStore,
        commands::planning::{self, Dependency},
        dependency_graph,
    },
    domain::{
        ClaimOutcome, EvidenceCheck, GoalId, Priority, PrioritySource, Provider, TaskAction,
        TaskAuthorization, TaskEdit, TaskId, TaskStatus, cancel_reason::NewCancel,
    },
    infrastructure::sqlite::SqliteQueue,
};
use rusqlite::Connection;

use common::queue::*;

/// An actor's cancel naming `other` as the task that holds the content.
fn duplicate_of(other: TaskId) -> NewCancel {
    NewCancel::by_hand(None, Some(other), None)
}

/// `add --paths` stores the globs once each; `set_paths` replaces them on a
/// draft or ready task only, records `task_paths_changed` when they change,
/// and an invalid glob is refused either way (ADR-0029).
#[test]
fn paths_are_stored_and_replaced_while_the_task_is_editable() {
    let (_dir, mut queue) = fixture();
    let globs = |values: &[&str]| values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>();
    let mut spec = new_task("absolute");
    spec.paths = globs(&["/docs/**"]);
    let error = queue.add(spec).unwrap_err().to_string();
    assert!(
        error.contains("invalid --paths glob \"/docs/**\""),
        "{error}"
    );
    let mut spec = new_task("docs only");
    spec.paths = globs(&["docs/**", "*.md", "docs/**"]);
    let task = queue.add(spec).unwrap();
    assert_eq!(task.paths(), globs(&["docs/**", "*.md"]));
    assert_eq!(queue.show(task.id()).unwrap().task.paths(), task.paths());
    // No change, no event.
    queue
        .set_paths(task.id(), globs(&["docs/**", "*.md"]))
        .unwrap();
    assert!(queue.set_paths(task.id(), globs(&["../x"])).is_err());
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let widened = queue
        .set_paths(task.id(), globs(&["docs/**", "src/**"]))
        .unwrap();
    assert_eq!(widened.paths(), globs(&["docs/**", "src/**"]));
    assert!(
        queue
            .set_paths(task.id(), Vec::new())
            .unwrap()
            .paths()
            .is_empty()
    );
    let changes: Vec<_> = queue
        .show(task.id())
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind == EventKind::TaskPathsChanged)
        .map(|e| (e.run_id, e.payload))
        .collect();
    assert_eq!(
        changes,
        [
            (
                None,
                serde_json::json!({"from": ["docs/**", "*.md"], "to": ["docs/**", "src/**"]})
            ),
            (
                None,
                serde_json::json!({"from": ["docs/**", "src/**"], "to": []})
            ),
        ]
    );
    // Once claimed, the run keeps the scope it started with.
    queue.claim(&base()).unwrap();
    let error = queue
        .set_paths(task.id(), globs(&["docs/**"]))
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "the paths can only be changed for draft, submitted or ready tasks"
    );
    assert!(queue.set_paths(TaskId::new(99), Vec::new()).is_err());
}

/// `edit_task` refuses a blank acceptance and leaves the task and its
/// `task_edited` events as they were, while a blank
/// context replaces the old one; `add` takes a blank description and
/// acceptance.
#[test]
fn edit_task_refuses_blank_plan_fields_and_changes_nothing() {
    let (_dir, mut queue) = fixture();
    let task = queue.add(new_task("kept")).unwrap();
    let blank = queue
        .add(dagq::domain::NewTask {
            description: String::new(),
            acceptance: " ".into(),
            ..new_task("blank plan")
        })
        .unwrap();
    assert_eq!((blank.description(), blank.acceptance()), ("", " "));
    let edits = |queue: &mut SqliteQueue| {
        queue
            .show(task.id())
            .unwrap()
            .events
            .into_iter()
            .filter(|e| e.kind == EventKind::TaskEdited)
            .count()
    };
    let before = queue.show(task.id()).unwrap().task;
    // One representative refusal; which values are blank is the domain's
    // unit test. The valid context beside it is not written either.
    assert_eq!(
        queue
            .edit_task(
                task.id(),
                TaskEdit {
                    acceptance: Some(" \n".into()),
                    context: Some("ignored".into()),
                    ..TaskEdit::default()
                },
                TaskStatus::Draft,
            )
            .unwrap_err()
            .to_string(),
        "task acceptance must not be blank"
    );
    let after = queue.show(task.id()).unwrap().task;
    assert_eq!(
        (
            after.title(),
            after.description(),
            after.acceptance(),
            after.context()
        ),
        (
            before.title(),
            before.description(),
            before.acceptance(),
            before.context()
        )
    );
    assert_eq!(edits(&mut queue), 0);

    queue
        .edit_task(
            task.id(),
            TaskEdit {
                context: Some("why".into()),
                ..TaskEdit::default()
            },
            TaskStatus::Draft,
        )
        .unwrap();
    let cleared = queue
        .edit_task(
            task.id(),
            TaskEdit {
                context: Some(String::new()),
                ..TaskEdit::default()
            },
            TaskStatus::Draft,
        )
        .unwrap();
    assert_eq!(cleared.context(), "");
    assert_eq!(edits(&mut queue), 2);
}

/// `edit_task` replaces the given fields of a draft task, records
/// `task_edited` with the old and new value of each field that changed (no
/// event when nothing does), and refuses a task that is no longer a draft
/// (ADR-0041 decision 9).
#[test]
fn edit_task_replaces_draft_fields_and_records_the_change() {
    let (_dir, mut queue) = fixture();
    let task = queue.add(new_task("first title")).unwrap();
    assert_eq!(
        queue
            .edit_task(task.id(), TaskEdit::default(), TaskStatus::Draft)
            .unwrap_err()
            .to_string(),
        "task edit changes nothing"
    );
    let edited = queue
        .edit_task(
            task.id(),
            TaskEdit {
                title: Some("second title".into()),
                verification_commands: Some(vec!["cargo fmt --all --check".into()]),
                required_evidence: Some(vec![EvidenceCheck::E2e]),
                paths: Some(vec!["docs/**".into()]),
                context: Some("why".into()),
                ..TaskEdit::default()
            },
            TaskStatus::Draft,
        )
        .unwrap();
    assert_eq!(edited.title(), "second title");
    assert_eq!(edited.description(), "A small development task");
    let shown = queue.show(task.id()).unwrap().task;
    assert_eq!(shown.verification_commands(), ["cargo fmt --all --check"]);
    assert_eq!(shown.required_evidence(), [EvidenceCheck::E2e]);
    assert_eq!(shown.paths(), ["docs/**"]);
    assert_eq!(shown.context(), "why");
    // The same values again change nothing and record nothing.
    queue
        .edit_task(
            task.id(),
            TaskEdit {
                title: Some("second title".into()),
                ..TaskEdit::default()
            },
            TaskStatus::Draft,
        )
        .unwrap();
    let edits: Vec<_> = queue
        .show(task.id())
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind == EventKind::TaskEdited)
        .map(|e| (e.run_id, e.payload))
        .collect();
    assert_eq!(
        edits,
        [(
            None,
            serde_json::json!({
                "from": {
                    "title": "first title",
                    "verification_commands": ["cargo test"],
                    "required_evidence": [],
                    "paths": [],
                    "context": "",
                },
                "to": {
                    "title": "second title",
                    "verification_commands": ["cargo fmt --all --check"],
                    "required_evidence": ["e2e"],
                    "paths": ["docs/**"],
                    "context": "why",
                },
            })
        )]
    );
    // A bad value is refused before the task is read.
    assert!(
        queue
            .edit_task(
                TaskId::new(99),
                TaskEdit {
                    paths: Some(vec!["../x".into()]),
                    ..TaskEdit::default()
                },
                TaskStatus::Draft
            )
            .unwrap_err()
            .to_string()
            .contains("invalid --paths glob")
    );
    let change = || TaskEdit {
        description: Some("late".into()),
        ..TaskEdit::default()
    };
    assert!(
        queue
            .edit_task(TaskId::new(99), change(), TaskStatus::Draft)
            .is_err()
    );
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    assert_eq!(
        queue
            .edit_task(task.id(), change(), TaskStatus::Ready)
            .unwrap_err()
            .to_string(),
        format!(
            "task {} is ready; only a draft or submitted task can be edited freely; an in_progress task permits only user or inbox --verify/--no-verify after its latest run ended and no live run remains",
            task.id()
        )
    );
    queue.claim(&base()).unwrap();
    assert_eq!(
        queue
            .edit_task(task.id(), change(), TaskStatus::InProgress)
            .unwrap_err()
            .to_string(),
        format!(
            "task {} is in_progress; only --verify/--no-verify may be edited by user or inbox after its latest run has ended and no live run remains",
            task.id()
        )
    );
    assert_eq!(
        queue.show(task.id()).unwrap().task.description(),
        "A small development task"
    );
}

#[test]
fn ended_run_allows_only_verify_correction_before_inherited_retry() {
    let (dir, mut queue) = fixture();
    let task = queue.add(new_task("verify correction")).unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let run = match queue.claim(&base()).unwrap() {
        ClaimOutcome::Claimed { run } => run,
        other => panic!("unexpected claim: {other:?}"),
    };
    let edit = || TaskEdit {
        verification_commands: Some(vec!["python3.11 check.py".into()]),
        ..TaskEdit::default()
    };
    assert!(
        queue
            .edit_task(task.id(), edit(), TaskStatus::InProgress)
            .unwrap_err()
            .to_string()
            .contains("no live run remains")
    );
    let conn = Connection::open(dir.path().join("queue.db")).unwrap();
    conn.execute(
        "UPDATE task_runs SET status='failed' WHERE id=?1",
        [run.id().as_str()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO run_leases(run_id,token,pid) VALUES (?1,'test',1)",
        [run.id().as_str()],
    )
    .unwrap();
    assert!(
        queue
            .edit_task(task.id(), edit(), TaskStatus::InProgress)
            .is_err(),
        "the ended run still has a lease"
    );
    conn.execute(
        "DELETE FROM run_leases WHERE run_id=?1",
        [run.id().as_str()],
    )
    .unwrap();
    for status in [
        "running",
        "awaiting_integration",
        "integrating",
        "needs_session",
    ] {
        conn.execute(
            "UPDATE task_runs SET status=?1 WHERE id=?2",
            rusqlite::params![status, run.id().as_str()],
        )
        .unwrap();
        assert!(
            queue
                .edit_task(task.id(), edit(), TaskStatus::InProgress)
                .is_err(),
            "{status}"
        );
    }
    conn.execute(
        "UPDATE task_runs SET status='failed' WHERE id=?1",
        [run.id().as_str()],
    )
    .unwrap();
    assert!(
        queue
            .edit_task(
                task.id(),
                TaskEdit {
                    paths: Some(vec!["src/**".into()]),
                    ..TaskEdit::default()
                },
                TaskStatus::InProgress
            )
            .is_err()
    );
    let edited = queue
        .edit_task(task.id(), edit(), TaskStatus::InProgress)
        .unwrap();
    assert_eq!(edited.verification_commands(), ["python3.11 check.py"]);
    let event = queue
        .show(task.id())
        .unwrap()
        .events
        .into_iter()
        .find(|e| e.kind == EventKind::TaskEdited)
        .unwrap();
    assert_eq!(
        event.payload["from"]["verification_commands"],
        serde_json::json!(["cargo test"])
    );
    assert_eq!(
        event.payload["to"]["verification_commands"],
        serde_json::json!(["python3.11 check.py"])
    );
    queue
        .edit_task(
            task.id(),
            TaskEdit {
                verification_commands: Some(vec![]),
                ..TaskEdit::default()
            },
            TaskStatus::InProgress,
        )
        .unwrap();
    assert!(
        queue
            .show(task.id())
            .unwrap()
            .task
            .verification_commands()
            .is_empty()
    );
}

/// ADR-t883-1: an edit authorized while the task was `ready` (`task.write`)
/// is refused when the store finds it `in_progress` with a failed run, where
/// only `task.verify_edit` would let it through; the task and its events stay
/// as they were. Authorized with the status it has, the same edit goes on.
#[test]
fn an_edit_authorized_on_another_status_changes_nothing() {
    let (dir, mut queue) = fixture();
    let task = queue.add(new_task("claimed in between")).unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let run = match queue.claim(&base()).unwrap() {
        ClaimOutcome::Claimed { run } => run,
        other => panic!("unexpected claim: {other:?}"),
    };
    Connection::open(dir.path().join("queue.db"))
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='failed' WHERE id=?1",
            [run.id().as_str()],
        )
        .unwrap();
    let edit = || TaskEdit {
        verification_commands: Some(vec!["python3.11 check.py".into()]),
        ..TaskEdit::default()
    };
    assert_eq!(
        queue
            .edit_task(task.id(), edit(), TaskStatus::Ready)
            .unwrap_err()
            .to_string(),
        format!(
            "task {} is in_progress now, not ready as when this command was authorized; nothing was changed, run it again",
            task.id()
        )
    );
    let edited_events = |queue: &mut dagq::infrastructure::sqlite::SqliteQueue| {
        queue
            .show(task.id())
            .unwrap()
            .events
            .into_iter()
            .filter(|e| e.kind == EventKind::TaskEdited)
            .count()
    };
    assert_eq!(edited_events(&mut queue), 0);
    assert_eq!(
        queue.show(task.id()).unwrap().task.verification_commands(),
        ["cargo test"]
    );
    let edited = queue
        .edit_task(task.id(), edit(), TaskStatus::InProgress)
        .unwrap();
    assert_eq!(edited.verification_commands(), ["python3.11 check.py"]);
    assert_eq!(edited_events(&mut queue), 1);
}

/// ADR-t1639-1 decision 2: setting or clearing a task's own priority is
/// recorded even when its base value stays the same, with where the value
/// comes from before and after; asking again for what it has records nothing.
#[test]
fn clearing_an_own_priority_records_the_change_of_its_source() {
    let (_dir, mut queue) = fixture();
    let goal = queue.add_goal(new_goal("normal goal")).unwrap().id();
    let mut spec = new_task("own normal");
    spec.priority = Some(Priority::Normal);
    spec.goal_id = Some(goal);
    let task = queue.add(spec).unwrap().id();
    let inherited = queue.set_priority(task, None).unwrap();
    assert_eq!(
        (inherited.priority(), inherited.priority_source()),
        (Priority::Normal, PrioritySource::Goal)
    );
    queue.set_priority(task, None).unwrap();
    queue.set_priority(task, Some(Priority::Normal)).unwrap();
    let changes: Vec<_> = queue
        .show(task)
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind == EventKind::TaskPriorityChanged)
        .map(|e| e.payload)
        .collect();
    assert_eq!(
        changes,
        [
            serde_json::json!({"from": "normal", "to": "normal", "from_source": "task",
                "to_source": "goal", "own": null, "own_by": null}),
            serde_json::json!({"from": "normal", "to": "normal", "from_source": "goal",
                "to_source": "task", "own": "normal", "own_by": "human"}),
        ]
    );
}

/// `task_created` carries the worker the task is registered with, resolved,
/// and whether the registrant named its provider and its mode: a provider
/// named without a mode takes the mode its default gives (ADR-t1340-1).
#[test]
fn task_created_records_the_worker_and_whether_each_part_was_named() {
    let (_dir, mut queue) = fixture();
    let mut spec = new_task("codex by name");
    spec.provider = Some(Provider::Codex);
    spec.worker_mode = None;
    let task = queue.add(spec).unwrap().id();
    let events = queue.show(task).unwrap().events;
    let created = events
        .iter()
        .find(|e| e.kind == EventKind::TaskCreated)
        .unwrap();
    let worker = [
        "provider",
        "provider_named",
        "worker_mode",
        "worker_mode_named",
    ]
    .map(|field| created.payload[field].clone());
    assert_eq!(
        serde_json::Value::from(worker.to_vec()),
        serde_json::json!(["codex", true, "headless", false])
    );
}

/// `add` stores the priority and `set_priority` changes it on a draft,
/// ready or in-progress task only, recording `task_priority_changed` when it changes; the
/// column refuses anything outside 0..=4 (ADR-0040 decision 4).
#[test]
fn priority_is_stored_changed_while_editable_and_checked_on_read() {
    let (dir, mut queue) = fixture();
    let mut spec = new_task("urgent");
    spec.priority = Some(Priority::Urgent);
    let task = queue.add(spec).unwrap();
    assert_eq!(task.priority(), Priority::Urgent);
    assert_eq!(
        queue.show(task.id()).unwrap().task.priority(),
        Priority::Urgent
    );
    // No change, no event.
    queue
        .set_priority(task.id(), Some(Priority::Urgent))
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let low = queue.set_priority(task.id(), Some(Priority::Low)).unwrap();
    assert_eq!(low.priority(), Priority::Low);
    let changes: Vec<_> = queue
        .show(task.id())
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind == EventKind::TaskPriorityChanged)
        .map(|e| e.payload)
        .collect();
    assert_eq!(
        changes,
        [
            serde_json::json!({"from": "urgent", "to": "low", "from_source": "task",
            "to_source": "task", "own": "low", "own_by": "human"})
        ]
    );
    // In progress, it orders the next resume and recovery job of its run
    // (ADR-t1850-1 decision 7); a finished task keeps it.
    queue.claim(&base()).unwrap();
    let interrupt = queue
        .set_priority(task.id(), Some(Priority::Interrupt))
        .unwrap();
    assert_eq!(interrupt.priority(), Priority::Interrupt);
    assert_eq!(interrupt.status(), TaskStatus::InProgress);
    queue.set_priority(task.id(), Some(Priority::Low)).unwrap();
    Connection::open(dir.path().join("queue.db"))
        .unwrap()
        .execute("UPDATE tasks SET status='completed' WHERE id=1", [])
        .unwrap();
    assert_eq!(
        queue
            .set_priority(task.id(), Some(Priority::Interrupt))
            .unwrap_err()
            .to_string(),
        "task 1 is completed; the priority can only be changed for draft, submitted, ready or in_progress tasks"
    );
    assert!(
        queue
            .set_priority(TaskId::new(99), Some(Priority::Low))
            .is_err()
    );

    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    // A priority outside the domain's fails the read (ADR-t876-1).
    for value in [-1, 5] {
        raw.execute("UPDATE tasks SET priority=?1 WHERE id=1", [value])
            .unwrap();
        assert!(queue.show(task.id()).is_err(), "{value}");
    }
    raw.execute("UPDATE tasks SET priority=4 WHERE id=1", [])
        .unwrap();
    assert_eq!(
        queue.show(task.id()).unwrap().task.priority(),
        Priority::Interrupt
    );
}

/// `candidates`, `graph` and successive claims follow one order: highest
/// effective priority, then most unblocks, then lowest ID. A ready task
/// passes its priority to what it waits for; a draft task and a task of a
/// draft goal do not.
#[test]
fn candidates_graph_and_claims_share_the_priority_order() {
    let (_dir, mut queue) = fixture();
    let mut draft_goal = new_goal("parked goal");
    draft_goal.draft = true;
    let draft_goal = queue.add_goal(draft_goal).unwrap().id();
    let mut add = |title: &str, priority: Priority, depends_on: &[TaskId], goal: Option<GoalId>| {
        let mut spec = new_task(title);
        spec.priority = Some(priority);
        spec.dependencies = depends_on.to_vec();
        spec.goal_id = goal;
        queue.add(spec).unwrap().id()
    };
    let plain = add("plain", Priority::Normal, &[], None);
    let releasing = add("releasing", Priority::Normal, &[], None);
    let parked = add("parked", Priority::Interrupt, &[releasing], None);
    let low = add("low", Priority::Low, &[], None);
    let lifted = add("lifted", Priority::Normal, &[], None);
    let waiter = add("waiter", Priority::Urgent, &[lifted], None);
    let high = add("high", Priority::High, &[], None);
    let in_draft_goal = add(
        "in draft goal",
        Priority::Interrupt,
        &[low],
        Some(draft_goal),
    );
    for id in [plain, releasing, low, lifted, waiter, high, in_draft_goal] {
        queue.transition(id, TaskAction::BypassReview).unwrap();
    }
    assert_eq!(queue.show(parked).unwrap().task.status(), TaskStatus::Draft);

    let expected = [lifted, high, releasing, plain, low];
    let candidates: Vec<TaskId> = queue.candidates().unwrap().iter().map(|t| t.id()).collect();
    assert_eq!(candidates, expected);
    let graph = dependency_graph(queue.graph_input().unwrap(), None);
    assert_eq!(graph.candidates, expected);
    let node = |id: TaskId| graph.tasks.iter().find(|n| n.id == id).unwrap();
    assert_eq!(node(lifted).priority, Priority::Normal);
    assert_eq!(node(lifted).effective_priority, Priority::Urgent);
    assert_eq!(node(releasing).effective_priority, Priority::Normal);
    assert_eq!(node(low).effective_priority, Priority::Low);
    assert_eq!(node(parked).effective_priority, Priority::Interrupt);

    let mut claimed = Vec::new();
    while let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() {
        claimed.push(run.task_id());
    }
    assert_eq!(claimed, expected);
}

fn listed_ids(queue: &SqliteQueue, query: &TaskQuery) -> Vec<TaskId> {
    let page = queue.list(query).unwrap();
    page.tasks.iter().map(|task| task.id).collect()
}

#[test]
fn list_defaults_to_unfinished_tasks_newest_first_with_compact_items() {
    let (dir, mut queue) = fixture();
    let goal = queue.add_goal(new_goal("grouped")).unwrap().id();
    let a = queue.add(new_task("landed")).unwrap().id();
    let b = queue.add(new_task("dropped")).unwrap().id();
    let mut spec = new_task("claimed");
    spec.dependencies = vec![a];
    spec.goal_id = Some(goal);
    let c = queue.add(spec).unwrap().id();
    let d = queue.add(new_task("waiting")).unwrap().id();
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    raw.execute("UPDATE tasks SET status='completed' WHERE id=?1", [a])
        .unwrap();
    queue.transition(b, TaskAction::Cancel).unwrap();
    queue.transition(c, TaskAction::BypassReview).unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!()
    };
    queue.transition(d, TaskAction::BypassReview).unwrap();

    let page = queue.list(&TaskQuery::default()).unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.next, None);
    assert_eq!(
        serde_json::to_value(&page).unwrap(),
        serde_json::json!({
            "tasks": [
                {"id": d, "status": "ready", "priority": "normal", "priority_source": "default",
                 "priority_by": "ai", "change": null, "execution_class": "implementation",
                 "title": "waiting",
                 "provider": "claude", "worker_mode": "interactive",
                 "goal_id": null,
                 "dependencies": [], "goal_dependencies": [], "latest_run": null},
                {"id": c, "status": "in_progress", "priority": "normal", "priority_source": "goal",
                 "priority_by": "human", "change": null, "execution_class": "implementation",
                 "title": "claimed", "provider": "claude", "worker_mode": "interactive",
                 "goal_id": goal,
                 "dependencies": [a], "goal_dependencies": [],
                 "latest_run": {"id": run.id(), "status": "claimed"}},
            ],
            "next": null,
            "total": 2,
        })
    );

    let all = TaskQuery {
        status: StatusFilter::Any,
        ..TaskQuery::default()
    };
    assert_eq!(listed_ids(&queue, &all), vec![d, c, b, a]);
    let only = TaskQuery {
        status: StatusFilter::Only(vec![TaskStatus::Ready, TaskStatus::Canceled]),
        ..TaskQuery::default()
    };
    assert_eq!(listed_ids(&queue, &only), vec![d, b]);
    let of_goal = TaskQuery {
        goal_id: Some(goal),
        ..TaskQuery::default()
    };
    assert_eq!(listed_ids(&queue, &of_goal), vec![c]);
    // Status and goal combine with AND.
    let ready_of_goal = TaskQuery {
        status: StatusFilter::Only(vec![TaskStatus::Ready]),
        goal_id: Some(goal),
        ..TaskQuery::default()
    };
    let page = queue.list(&ready_of_goal).unwrap();
    assert!(page.tasks.is_empty());
    assert_eq!(page.total, 0);
}

#[test]
fn list_full_items_carry_every_task_field() {
    let (_dir, mut queue) = fixture();
    let mut spec = new_task("detailed");
    spec.context = "why it exists".into();
    let task = queue.add(spec).unwrap();
    let full = TaskQuery {
        full: true,
        ..TaskQuery::default()
    };
    let item = serde_json::to_value(&queue.list(&full).unwrap().tasks[0]).unwrap();
    let mut expected = serde_json::to_value(&task).unwrap();
    expected["dependencies"] = serde_json::json!([]);
    expected["goal_dependencies"] = serde_json::json!([]);
    expected["latest_run"] = serde_json::Value::Null;
    assert_eq!(item, expected);
}

#[test]
fn list_pages_by_limit_with_next_as_the_following_before() {
    let (_dir, mut queue) = fixture();
    let ids: Vec<TaskId> = (0..21)
        .map(|n| queue.add(new_task(&format!("task {n}"))).unwrap().id())
        .collect();
    let newest_first: Vec<TaskId> = ids.iter().rev().copied().collect();

    // limit + 1 tasks: the extra one is the next page.
    let first = queue.list(&TaskQuery::default()).unwrap();
    assert_eq!(first.total, 21);
    assert_eq!(first.tasks.len(), 20);
    assert_eq!(
        first.tasks.iter().map(|t| t.id).collect::<Vec<_>>(),
        newest_first[..20]
    );
    assert_eq!(first.next, Some(ids[0]));
    let second = queue
        .list(&TaskQuery {
            before: first.next,
            ..TaskQuery::default()
        })
        .unwrap();
    assert_eq!(
        second.tasks.iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![ids[0]]
    );
    assert_eq!(second.next, None);
    assert_eq!(second.total, 21, "total ignores the page");

    // Exactly limit tasks: no next page.
    queue.transition(ids[0], TaskAction::Cancel).unwrap();
    let exact = queue.list(&TaskQuery::default()).unwrap();
    assert_eq!(exact.tasks.len(), 20);
    assert_eq!(exact.next, None);
    assert_eq!(exact.total, 20);

    let small = TaskQuery {
        limit: 3,
        before: Some(ids[10]),
        ..TaskQuery::default()
    };
    let page = queue.list(&small).unwrap();
    assert_eq!(
        page.tasks.iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![ids[10], ids[9], ids[8]]
    );
    assert_eq!(page.next, Some(ids[7]));
    let zero = TaskQuery {
        limit: 0,
        ..TaskQuery::default()
    };
    assert!(queue.list(&zero).is_err());
}

/// A ready task claimed in between and left `in_progress` by a failed run,
/// as a planner's `cancel` or `draft` authorized on `ready` would find it.
fn claimed_and_failed(dir: &tempfile::TempDir, queue: &mut SqliteQueue, title: &str) -> TaskId {
    let task = queue.add(new_task(title)).unwrap().id();
    queue.transition(task, TaskAction::BypassReview).unwrap();
    let run = match queue.claim(&base()).unwrap() {
        ClaimOutcome::Claimed { run } => run,
        other => panic!("unexpected claim: {other:?}"),
    };
    assert_eq!(run.task_id(), task);
    Connection::open(dir.path().join("queue.db"))
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='failed' WHERE id=?1",
            [run.id().as_str()],
        )
        .unwrap();
    task
}

/// Task 1609: `cancel` (with `--duplicate-of` too), `draft`, `set-goal`,
/// `set-paths`, `set-priority`, `ready` and `dependency add` / `remove`,
/// authorized while the task was `ready`, are refused when the store finds
/// it `in_progress` with a failed run; the task, its dependencies and its
/// events stay as they were. Authorized with the status the task has, the
/// same commands go on as before.
#[test]
fn changes_authorized_on_another_status_change_nothing() {
    let (dir, mut queue) = fixture();
    let refused = |id: TaskId| {
        format!(
            "task {id} is in_progress now, not ready as when this command was authorized; nothing was changed, run it again"
        )
    };
    let other = queue.add(new_task("predecessor")).unwrap().id();
    let goal = queue.add_goal(new_goal("grouped")).unwrap().id();
    let task = claimed_and_failed(&dir, &mut queue, "claimed in between");
    let before = queue.show(task).unwrap();
    let ready = || TaskAuthorization::outside_proposals(TaskStatus::Ready);
    let errors = [
        planning::PlanningStore::transition(&mut queue, task, TaskAction::Cancel, ready()),
        planning::PlanningStore::transition(&mut queue, task, TaskAction::Draft, ready()),
        planning::PlanningStore::transition(&mut queue, task, TaskAction::BypassReview, ready()),
        planning::PlanningStore::cancel(&mut queue, task, duplicate_of(other), ready()),
        planning::PlanningStore::set_goal(&mut queue, task, Some(goal), ready()),
        planning::PlanningStore::set_paths(&mut queue, task, vec!["docs/**".into()], ready()),
        planning::PlanningStore::set_priority(&mut queue, task, Some(Priority::High), ready()),
    ]
    .into_iter()
    .map(|result| result.unwrap_err().to_string())
    .chain(
        [
            planning::PlanningStore::add_dependency(
                &mut queue,
                task,
                Dependency::Task(other),
                ready(),
            ),
            planning::PlanningStore::add_dependency(
                &mut queue,
                task,
                Dependency::Goal(goal),
                ready(),
            ),
            planning::PlanningStore::remove_dependency(
                &mut queue,
                task,
                Dependency::Task(other),
                ready(),
            ),
            planning::PlanningStore::remove_dependency(
                &mut queue,
                task,
                Dependency::Goal(goal),
                ready(),
            ),
        ]
        .into_iter()
        .map(|result| result.unwrap_err().to_string()),
    )
    .collect::<Vec<_>>();
    assert_eq!(errors, vec![refused(task); 11]);
    let after = queue.show(task).unwrap();
    assert_eq!(after.task.status(), TaskStatus::InProgress);
    assert_eq!(
        serde_json::to_value(&after.task).unwrap(),
        serde_json::to_value(&before.task).unwrap()
    );
    assert_eq!(after.events.len(), before.events.len());
    assert!(after.dependencies.is_empty());
    assert!(after.goal_dependencies.is_empty());

    // Authorized with the status the task has, `cancel` and `draft` of an
    // `in_progress` task whose run ended go on as before.
    let in_progress = || TaskAuthorization::outside_proposals(TaskStatus::InProgress);
    let canceled =
        planning::PlanningStore::transition(&mut queue, task, TaskAction::Cancel, in_progress())
            .unwrap();
    assert_eq!(canceled.status(), TaskStatus::Canceled);
    let task = claimed_and_failed(&dir, &mut queue, "drafted again");
    let drafted =
        planning::PlanningStore::transition(&mut queue, task, TaskAction::Draft, in_progress())
            .unwrap();
    assert_eq!(drafted.status(), TaskStatus::Draft);
    let task = claimed_and_failed(&dir, &mut queue, "a duplicate");
    let duplicate =
        planning::PlanningStore::cancel(&mut queue, task, duplicate_of(other), in_progress())
            .unwrap();
    assert_eq!(duplicate.status(), TaskStatus::Canceled);

    // And on a `ready` task authorized as `ready`, the rest go on too.
    let task = queue.add(new_task("still ready")).unwrap().id();
    planning::PlanningStore::transition(
        &mut queue,
        task,
        TaskAction::BypassReview,
        TaskAuthorization::outside_proposals(TaskStatus::Draft),
    )
    .unwrap();
    let moved = planning::PlanningStore::set_goal(&mut queue, task, Some(goal), ready()).unwrap();
    assert_eq!(moved.goal_id(), Some(goal));
    planning::PlanningStore::set_paths(&mut queue, task, vec!["docs/**".into()], ready()).unwrap();
    planning::PlanningStore::set_priority(&mut queue, task, Some(Priority::High), ready()).unwrap();
    planning::PlanningStore::add_dependency(&mut queue, task, Dependency::Task(other), ready())
        .unwrap();
    assert_eq!(queue.show(task).unwrap().dependencies.len(), 1);
    planning::PlanningStore::remove_dependency(&mut queue, task, Dependency::Task(other), ready())
        .unwrap();
    assert!(queue.show(task).unwrap().dependencies.is_empty());
    let task = queue.show(task).unwrap().task;
    assert_eq!(task.paths(), ["docs/**"]);
    assert_eq!(task.priority(), Priority::High);
    // A status the store does not find is refused on a `ready` task too.
    assert!(
        planning::PlanningStore::transition(
            &mut queue,
            task.id(),
            TaskAction::Cancel,
            TaskAuthorization::outside_proposals(TaskStatus::Draft)
        )
        .is_err()
    );
    assert_eq!(
        queue.show(task.id()).unwrap().task.status(),
        TaskStatus::Ready
    );
}

/// What a planner's change of a task was authorized with includes the
/// proposal the task is in, its status and its owner: a change authorized
/// before the draft moved into another planner's revising proposal, or
/// before its revising proposal changed owner, is refused by the store,
/// and the task, its dependencies and its events stay as they were.
/// Authorized with what the store holds, the same change goes on.
#[test]
fn changes_authorized_before_the_tasks_proposal_changed_change_nothing() {
    use dagq::domain::{
        PlannerOrigin, PlannerOwner, ProposalStatus, Submission, TaskProposal,
        follow_up::RevisitChange,
    };
    let (dir, mut queue) = fixture();
    let other = queue.add(new_task("predecessor")).unwrap().id();
    let goal = queue.add_goal(new_goal("grouped")).unwrap().id();
    let task = queue.add(new_task("moved in between")).unwrap().id();
    let refused = format!(
        "task {task} is in another proposal now, or its proposal's status or owner changed, since this command was authorized; nothing was changed, run it again"
    );
    let proposal = queue
        .submit(Submission {
            tasks: vec![task],
            goals: Vec::new(),
            proposal: None,
            owner: PlannerOwner {
                origin: PlannerOrigin::Runtime,
                workspace_id: None,
            },
        })
        .unwrap()
        .id();
    let conn = Connection::open(dir.path().join("queue.db")).unwrap();
    let send_back_to = |owner: &str| {
        conn.execute(
            "UPDATE proposals SET status='revising', owner_actor_id=?1, revise_planner_id=NULL
             WHERE id=?2",
            rusqlite::params![owner, proposal],
        )
        .unwrap();
        conn.execute("UPDATE tasks SET status='draft' WHERE id=?1", [task])
            .unwrap();
    };
    let revising = |owner: &str| TaskAuthorization {
        status: TaskStatus::Draft,
        proposal: Some(TaskProposal {
            id: proposal,
            status: ProposalStatus::Revising,
            owner: Some(owner.into()),
        }),
    };
    send_back_to("planner:5");
    // Authorized while the draft was in no proposal, and while its revising
    // proposal was planner 1's.
    for stale in [
        TaskAuthorization::outside_proposals(TaskStatus::Draft),
        revising("planner:1"),
    ] {
        let before = queue.show(task).unwrap();
        let s = || stale.clone();
        let mut errors = [
            planning::PlanningStore::edit_task(
                &mut queue,
                task,
                TaskEdit {
                    description: Some("replaced".into()),
                    ..TaskEdit::default()
                },
                s(),
            ),
            planning::PlanningStore::transition(&mut queue, task, TaskAction::Draft, s()),
            planning::PlanningStore::cancel(&mut queue, task, duplicate_of(other), s()),
            planning::PlanningStore::set_goal(&mut queue, task, Some(goal), s()),
            planning::PlanningStore::set_paths(&mut queue, task, vec!["docs/**".into()], s()),
            planning::PlanningStore::set_priority(&mut queue, task, Some(Priority::High), s()),
        ]
        .into_iter()
        .map(|result| result.unwrap_err().to_string())
        .collect::<Vec<_>>();
        for on in [Dependency::Task(other), Dependency::Goal(goal)] {
            for result in [
                planning::PlanningStore::add_dependency(&mut queue, task, on, s()),
                planning::PlanningStore::remove_dependency(&mut queue, task, on, s()),
            ] {
                errors.push(result.unwrap_err().to_string());
            }
        }
        errors.push(
            planning::PlanningStore::revisit_draft(
                &mut queue,
                task,
                RevisitChange::Set { at: 1, note: None },
                "planner",
                "planner:1",
                s(),
            )
            .unwrap_err()
            .to_string(),
        );
        assert_eq!(errors, vec![refused.clone(); 11], "{stale:?}");
        let after = queue.show(task).unwrap();
        assert_eq!(
            serde_json::to_value(&after.task).unwrap(),
            serde_json::to_value(&before.task).unwrap()
        );
        assert_eq!(after.events.len(), before.events.len());
        assert!(after.dependencies.is_empty());
        assert!(after.goal_dependencies.is_empty());
    }

    // Authorized with the proposal and owner the store holds, it goes on.
    let paths = planning::PlanningStore::set_paths(
        &mut queue,
        task,
        vec!["docs/**".into()],
        revising("planner:5"),
    )
    .unwrap();
    assert_eq!(paths.paths(), ["docs/**"]);
}
