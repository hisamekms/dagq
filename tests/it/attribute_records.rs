//! Queue tests: the attributes `stats`, `kpi` and `report` read of the
//! planning tables are recorded in the transaction that changes them, and
//! the measurement's fold of the EventStore (`domain::ledger::attributes`)
//! gives what the store reads now (docs/design/attribute-records.md).
use crate::common;

use dagq::application::{GraphInput, TaskStore};
use dagq::domain::ledger::attributes::{Folded, fold};
use dagq::domain::{
    ClaimOutcome, DraftOrigin, ExecutionClass, GoalEdit, GoalId, GoalVerdict, NewGoal, Priority,
    TaskAction, TaskEdit, TaskId, TaskStatus, event_kind,
};
use dagq::infrastructure::sqlite::SqliteQueue;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use common::attributes::{assert_folded_matches_state, folded};
use common::queue::*;

/// The payload of the newest event of `kind` on `task`.
fn latest(queue: &mut SqliteQueue, task: TaskId, kind: &str) -> Value {
    queue
        .show(task)
        .unwrap()
        .events
        .iter()
        .rev()
        .find(|event| event.kind == kind)
        .unwrap_or_else(|| panic!("no {kind} on task {task}"))
        .payload
        .clone()
}

fn goal(queue: &mut SqliteQueue, title: &str, priority: Priority, draft: bool) -> GoalId {
    queue
        .add_goal(NewGoal {
            priority: Some(priority),
            draft,
            ..new_goal(title)
        })
        .unwrap()
        .id()
}

/// Every write path of the attributes records their new values in its own
/// transaction (the creation, edit, set-goal, priority, dependency, goal
/// dependency, draft origin, goal title, priority and status, close and
/// claim of a run, and the run leaving the unfinished statuses), and the
/// fold follows the store at every step.
#[test]
fn every_write_path_records_the_new_values_and_the_fold_follows_the_store() {
    let (_dir, mut q) = fixture();
    let open = goal(&mut q, "open goal", Priority::High, false);
    let draft = goal(&mut q, "draft goal", Priority::Normal, true);
    let waited = goal(&mut q, "waited goal", Priority::Low, false);
    let abandoned = goal(&mut q, "abandoned goal", Priority::Low, false);
    let first = q
        .add(dagq::domain::NewTask {
            goal_id: Some(open),
            change: Some("runtime".parse().unwrap()),
            priority: None,
            ..new_task("first")
        })
        .unwrap()
        .id();
    let created = latest(&mut q, first, event_kind::TASK_CREATED);
    assert_eq!(created["title"], "first");
    assert_eq!(created["change"], "runtime");
    assert_eq!(created["goal_id"], open.as_i64());
    assert!(created["priority"].is_null());
    let second = q
        .add(dagq::domain::NewTask {
            dependencies: vec![first],
            goal_dependencies: vec![waited, abandoned],
            priority: Some(Priority::Urgent),
            ..new_task("second")
        })
        .unwrap()
        .id();
    assert_eq!(
        latest(&mut q, second, event_kind::TASK_CREATED)["priority"],
        "urgent"
    );
    let in_draft = q
        .add(dagq::domain::NewTask {
            goal_id: Some(draft),
            priority: None,
            ..new_task("in a draft goal")
        })
        .unwrap()
        .id();
    assert_folded_matches_state(&mut q);

    q.edit_task(
        first,
        TaskEdit {
            title: Some("first, renamed".into()),
            change: Some("docs".parse().unwrap()),
            execution_class: Some(ExecutionClass::Spike),
            ..TaskEdit::default()
        },
        TaskStatus::Draft,
    )
    .unwrap();
    assert_folded_matches_state(&mut q);

    // An own priority given and taken away: it inherits the goal's again.
    q.set_goal(second, Some(open)).unwrap();
    q.set_priority(second, Some(Priority::Low)).unwrap();
    let changed = latest(&mut q, second, event_kind::TASK_PRIORITY_CHANGED);
    assert_eq!(
        (changed["own"].clone(), changed["own_by"].clone()),
        (json!("low"), json!("human"))
    );
    assert_folded_matches_state(&mut q);
    q.set_priority(second, None).unwrap();
    assert!(latest(&mut q, second, event_kind::TASK_PRIORITY_CHANGED)["own"].is_null());
    assert_folded_matches_state(&mut q);
    // The goal's priority, which both inherit now, and its title.
    q.edit_goal(
        open,
        GoalEdit {
            title: Some("open goal, renamed".into()),
            priority: Some(Priority::Interrupt),
            ..GoalEdit::default()
        },
    )
    .unwrap();
    assert_folded_matches_state(&mut q);
    q.set_goal(second, None).unwrap();
    assert_folded_matches_state(&mut q);

    // Dependencies taken away and given again.
    q.remove_dependency(second, first).unwrap();
    q.remove_goal_dependency(second, abandoned).unwrap();
    assert_folded_matches_state(&mut q);
    q.add_dependency(second, first).unwrap();
    q.add_goal_dependency(second, abandoned).unwrap();
    assert_folded_matches_state(&mut q);

    let origin = q.add(new_task("a runtime draft")).unwrap().id();
    q.record_draft_origin(origin, DraftOrigin::Revisit, &json!({}))
        .unwrap();
    assert_eq!(
        latest(&mut q, origin, event_kind::DRAFT_ORIGIN_RECORDED)["origin"],
        "revisit"
    );
    assert_folded_matches_state(&mut q);

    for task in [first, second, in_draft] {
        q.transition(task, TaskAction::BypassReview).unwrap();
    }
    // The task in the draft goal is not a candidate until its goal opens.
    assert_folded_matches_state(&mut q);
    q.ready_goal(draft).unwrap();
    assert_folded_matches_state(&mut q);

    // A run claimed, then out of the unfinished statuses.
    let ClaimOutcome::Claimed { run, .. } = q.claim(&base()).unwrap() else {
        panic!("claim")
    };
    assert_folded_matches_state(&mut q);
    let run = q.recover_run(run.id(), 0, json!({})).unwrap();
    let left = latest(&mut q, run.task_id(), event_kind::RUN_UNFINISHED_CHANGED);
    assert_eq!(left["unfinished"], false);
    assert_folded_matches_state(&mut q);

    // The goals the second task waits for closed as achieved and as
    // abandoned: only an achieved one releases it.
    q.close_goal(abandoned, GoalVerdict::Abandoned).unwrap();
    q.close_goal(waited, GoalVerdict::Achieved).unwrap();
    assert_folded_matches_state(&mut q);
    q.remove_goal_dependency(second, abandoned).unwrap();
    assert_folded_matches_state(&mut q);
}

/// The queue as a binary before the record leaves it: rows written with no
/// event, or with a creation record that carries no attributes.
fn older_rows(path: &std::path::Path) {
    let raw = Connection::open(path).unwrap();
    raw.execute_batch(
        "INSERT INTO goals(id, title, description, acceptance, constraints, status, priority,
                           priority_by, tags, closed_at, verdict)
         VALUES (1, 'older open', '', '', '', 'open', 3, 'human', '[\"measure\"]', NULL, NULL),
                (2, 'older achieved', '', '', '', 'open', 1, 'ai', '[]', '2026-10-01T00:00:00Z',
                 'achieved'),
                (3, 'older abandoned', '', '', '', 'open', 1, 'ai', '[]', '2026-10-01T00:00:00Z',
                 'abandoned');
         INSERT INTO run_events(goal_id, kind, payload)
         VALUES (1, 'goal_created', '{\"goal\": {\"id\": 1, \"title\": \"older open\"}}');
         INSERT INTO tasks(id, title, description, acceptance, verification_commands, status,
                           goal_id, priority, priority_by, change, execution_class)
         VALUES (1, 'older done', '', '', '[]', 'completed', 2, NULL, NULL, 'runtime',
                 'implementation'),
                (2, 'older ready', '', '', '[]', 'ready', 1, 4, 'ai', 'docs', 'spike'),
                (3, 'older running', '', '', '[]', 'in_progress', NULL, NULL, NULL, NULL,
                 'implementation'),
                (4, 'older waiting', '', '', '[]', 'ready', 1, NULL, NULL, 'not a label!',
                 'implementation'),
                (5, 'older draft', '', '', '[]', 'draft', NULL, 2, NULL, NULL,
                 'implementation');
         INSERT INTO run_events(task_id, kind, payload)
         VALUES (1, 'task_created', '{\"goal_id\": 2}'), (2, 'task_created', '{\"goal_id\": 1}');
         INSERT INTO task_dependencies(task_id, predecessor_id) VALUES (2, 1), (4, 3);
         INSERT INTO task_goal_dependencies(task_id, goal_id) VALUES (2, 2), (4, 3);
         INSERT INTO draft_origins(task_id, origin, material, created_at)
         VALUES (5, 'goal_gap', '{}', 0);",
    )
    .unwrap();
    raw.execute(
        "INSERT INTO task_runs(id, task_id, status, requested_provider, actual_provider,
                               base_commit)
         VALUES ('33333333-3333-4333-8333-333333333333', 3, 'running', 'claude', 'claude', ?1)",
        params![BASE],
    )
    .unwrap();
}

/// The baseline is written once for the tasks and goals whose records do
/// not carry their attributes, and from it and the later records the fold
/// gives what the store reads: through an edit, a set-goal, a dependency
/// removed, own priorities given and taken away, a goal's priority, goal
/// dependencies added and removed, an older run that ends, a goal closed as
/// achieved and as abandoned, and a run claimed after the baseline until
/// it ends and its task is retried. A window before the baseline is told
/// apart.
#[test]
fn the_baseline_is_idempotent_and_the_fold_from_it_follows_the_store() {
    let (dir, mut q) = fixture();
    older_rows(&dir.path().join("queue.db"));
    // Before the baseline the older rows are not in the records.
    assert!(folded(&q).tasks.is_empty());
    assert_eq!(q.record_attributes_baseline().unwrap(), Some((5, 3)));
    assert_eq!(q.record_attributes_baseline().unwrap(), None);
    assert_eq!(
        fold(&q.all_events().unwrap(), Some(0)),
        Folded::BeforeBaseline
    );
    assert_folded_matches_state(&mut q);
    // A task the queue's own binary made needs none.
    q.add(new_task("newer")).unwrap();
    assert_eq!(q.record_attributes_baseline().unwrap(), None);
    assert_folded_matches_state(&mut q);

    let (task, goal) = (TaskId::new(5), GoalId::new(1));
    q.edit_task(
        task,
        TaskEdit {
            title: Some("older draft, edited".into()),
            ..TaskEdit::default()
        },
        TaskStatus::Draft,
    )
    .unwrap();
    assert_folded_matches_state(&mut q);
    q.set_goal(task, Some(goal)).unwrap();
    assert_folded_matches_state(&mut q);
    q.set_priority(task, None).unwrap();
    assert_folded_matches_state(&mut q);
    q.set_priority(TaskId::new(4), Some(Priority::High))
        .unwrap();
    assert_folded_matches_state(&mut q);
    q.edit_goal(
        goal,
        GoalEdit {
            priority: Some(Priority::Low),
            ..GoalEdit::default()
        },
    )
    .unwrap();
    assert_folded_matches_state(&mut q);
    q.remove_dependency(TaskId::new(2), TaskId::new(1)).unwrap();
    assert_folded_matches_state(&mut q);
    q.add_goal_dependency(task, GoalId::new(2)).unwrap();
    q.remove_goal_dependency(TaskId::new(4), GoalId::new(3))
        .unwrap();
    assert_folded_matches_state(&mut q);
    // The older running run ends: the waiting task still waits for its
    // predecessor, which is in progress.
    let run = dagq::domain::RunId::new("33333333-3333-4333-8333-333333333333").unwrap();
    q.recover_run(&run, 0, json!({})).unwrap();
    assert_folded_matches_state(&mut q);
    let other = q.add_goal(new_goal("closing")).unwrap().id();
    q.add_goal_dependency(TaskId::new(4), other).unwrap();
    q.close_goal(other, GoalVerdict::Abandoned).unwrap();
    assert_folded_matches_state(&mut q);
    // After the baseline, a run claimed on the older ready task leaves the
    // candidates while it is unfinished, and its task is a candidate again
    // once the run ended and the task is retried.
    let ready = TaskId::new(2);
    assert_eq!(q.graph_input().unwrap().candidates, vec![ready]);
    let ClaimOutcome::Claimed { run, .. } = q.claim(&base()).unwrap() else {
        panic!("claim")
    };
    assert_eq!(run.task_id(), ready);
    assert_eq!(
        folded(&q).tasks[&ready].unfinished_run.as_ref(),
        Some(run.id())
    );
    assert!(q.graph_input().unwrap().candidates.is_empty());
    assert_folded_matches_state(&mut q);
    q.recover_run(run.id(), 0, json!({})).unwrap();
    assert_eq!(folded(&q).tasks[&ready].unfinished_run, None);
    assert_folded_matches_state(&mut q);
    q.transition(ready, TaskAction::Ready).unwrap();
    assert_eq!(q.graph_input().unwrap().candidates, vec![ready]);
    assert_folded_matches_state(&mut q);
}

/// The `run_unfinished_changed` of `run`, oldest first.
fn unfinished_changes(queue: &SqliteQueue, run: &dagq::domain::RunId) -> Vec<Value> {
    queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| {
            event.kind == event_kind::RUN_UNFINISHED_CHANGED && event.run_id.as_ref() == Some(run)
        })
        .map(|event| event.payload)
        .collect()
}

fn attributes_baselines(queue: &SqliteQueue) -> usize {
    queue
        .all_events()
        .unwrap()
        .iter()
        .filter(|event| event.kind == event_kind::ATTRIBUTES_BASELINE)
        .count()
}

/// What a binary before the record changes without its records (a copied
/// runner ending a run, a priority, an origin) leaves the records apart
/// from the state until the next comparison, which writes a baseline of
/// those tasks only; then they agree again, and the next one writes none.
#[test]
fn a_change_an_older_binary_makes_without_its_record_is_baselined_again() {
    let (dir, mut q) = fixture();
    let task = q.add(new_task("claimed")).unwrap().id();
    q.transition(task, TaskAction::BypassReview).unwrap();
    let ClaimOutcome::Claimed { run, .. } = q.claim(&base()).unwrap() else {
        panic!("claim")
    };
    let draft = q.add(new_task("a draft")).unwrap().id();
    assert_eq!(q.record_attributes_baseline().unwrap(), None);
    assert_folded_matches_state(&mut q);
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    raw.execute(
        "UPDATE task_runs SET status='failed' WHERE id=?1",
        [run.id().as_str()],
    )
    .unwrap();
    raw.execute(
        "UPDATE tasks SET priority=3, priority_by='ai' WHERE id=?1",
        [draft],
    )
    .unwrap();
    raw.execute(
        "INSERT INTO draft_origins(task_id, origin, material, created_at)
         VALUES (?1, 'revisit', '{}', 0)",
        [draft],
    )
    .unwrap();
    // The failed run's task is retried: the store has it as a candidate,
    // the records still hold the run unfinished.
    q.transition(task, TaskAction::Ready).unwrap();
    assert_eq!(q.graph_input().unwrap().candidates, vec![task]);
    let stale = folded(&q);
    assert_eq!(stale.tasks[&task].unfinished_run.as_ref(), Some(run.id()));
    assert!(GraphInput::folded(&stale).candidates.is_empty());
    assert_eq!(stale.tasks[&draft].origin, None);
    assert_eq!(q.record_attributes_baseline().unwrap(), Some((2, 0)));
    assert_folded_matches_state(&mut q);
    assert_eq!(q.record_attributes_baseline().unwrap(), None);
}

/// A supervised run that lands records its leaving the unfinished statuses
/// once, as `integrated`, in the landing's transaction; the supervisor's
/// comparison found nothing to write.
#[test]
fn a_landing_records_the_run_leaving_the_unfinished_statuses() {
    let (_fixture, repo, db, run) = crate::runtime_support::awaiting_run();
    let mut q = SqliteQueue::open(&db).unwrap();
    assert!(unfinished_changes(&q, run.id()).is_empty());
    assert_folded_matches_state(&mut q);
    assert_eq!(
        crate::runtime_support::integrate(&db, 1, &repo).unwrap()["outcome"],
        "integrated"
    );
    assert_eq!(
        unfinished_changes(&q, run.id()),
        [json!({"unfinished": false, "from": "integrating", "to": "integrated"})]
    );
    assert_eq!(attributes_baselines(&q), 0);
    assert_folded_matches_state(&mut q);
}

/// A session that fails under the supervisor records the run's end.
#[test]
fn a_failed_supervision_records_the_run_leaving_the_unfinished_statuses() {
    let (_fixture, db, detail) = crate::runtime_support::run_agent("commit work; exit 7");
    let run = detail.runs[0].clone();
    let mut q = SqliteQueue::open(&db).unwrap();
    assert_eq!(run.status(), dagq::domain::RunStatus::Failed);
    let changes = unfinished_changes(&q, run.id());
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0]["unfinished"], false);
    assert_eq!(changes[0]["to"], "failed");
    assert_eq!(attributes_baselines(&q), 0);
    assert_folded_matches_state(&mut q);
}

/// An abandon leaves the status, so records nothing.
#[test]
fn an_abandon_records_no_end_of_the_run() {
    use dagq::domain::{LeaseToken, Reason, ReasonCode, RunStatus};
    let (_dir, mut q) = fixture();
    let task = q.add(new_task("claimed")).unwrap().id();
    q.transition(task, TaskAction::BypassReview).unwrap();
    let token = LeaseToken::new("supervisor");
    let ClaimOutcome::Claimed { run, .. } = q.claim_for_supervisor(&base(), &token).unwrap() else {
        panic!("claim")
    };
    let abandoned = q
        .abandon_run(
            run.id(),
            &token,
            "lost",
            &Reason::new(ReasonCode::WorkerFailed),
            None,
        )
        .unwrap();
    assert_eq!(abandoned.status(), RunStatus::Claimed);
    assert!(unfinished_changes(&q, run.id()).is_empty());
    assert_folded_matches_state(&mut q);
}

/// A failed landing records the run's end in its transaction: a write of
/// the record that fails leaves the run integrating and writes nothing.
#[test]
fn a_failed_landing_records_its_end_and_a_failed_record_rolls_it_back() {
    use dagq::domain::{LeaseToken, RunStatus};
    let (_fixture, repo, db, run) = crate::runtime_support::awaiting_run();
    let mut q = SqliteQueue::open(&db).unwrap();
    let main = crate::runtime_support::git_out(&repo, &["rev-parse", "main"]);
    let token = LeaseToken::new("integrator");
    q.begin_integration(run.id(), &token, &crate::runtime_support::sha(&main))
        .unwrap();
    let raw = Connection::open(&db).unwrap();
    raw.execute_batch(
        "CREATE TRIGGER refuse_the_record BEFORE INSERT ON run_events
         WHEN new.kind = 'run_unfinished_changed' BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    )
    .unwrap();
    let before = q.all_events().unwrap().len();
    assert!(
        q.fail_integration(run.id(), &token, "worker failed", json!({}))
            .is_err()
    );
    assert_eq!(q.all_events().unwrap().len(), before);
    assert_eq!(
        q.show(TaskId::new(1)).unwrap().runs[0].status(),
        RunStatus::Integrating
    );
    raw.execute_batch("DROP TRIGGER refuse_the_record;")
        .unwrap();

    let failed = q
        .fail_integration(run.id(), &token, "worker failed", json!({}))
        .unwrap();
    assert_eq!(failed.status(), RunStatus::Failed);
    assert_eq!(
        unfinished_changes(&q, run.id()),
        [json!({"unfinished": false, "from": "integrating", "to": "failed"})]
    );
    assert_folded_matches_state(&mut q);
}

/// The supervisor's observation pass compares the records with the state
/// and writes the baseline of what an older binary changed without its
/// records (a task it made, a priority it changed), so the fold agrees
/// with the store through the run that follows.
#[test]
fn a_supervisors_pass_writes_the_baseline_of_what_an_older_binary_changed() {
    use crate::runtime_support::{TestWorkspace, VALID_AGENT};
    let (_fixture, repo, db) = crate::runtime_support::fixture();
    Connection::open(&db)
        .unwrap()
        .execute_batch(
            "INSERT INTO tasks(title, description, acceptance, verification_commands, status)
             VALUES ('older draft', '', '', '[]', 'draft');
             UPDATE tasks SET priority=3, priority_by='ai' WHERE id=1;",
        )
        .unwrap();
    let older = TaskId::new(2);
    assert!(
        !folded(&SqliteQueue::open(&db).unwrap())
            .tasks
            .contains_key(&older)
    );
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    crate::runtime_support::supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut q = SqliteQueue::open(&db).unwrap();
    let baselines: Vec<Option<TaskId>> = q
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == event_kind::ATTRIBUTES_BASELINE && event.goal_id.is_none())
        .map(|event| event.task_id)
        .collect();
    // The two tasks that differed, then the mark that closes the group.
    assert_eq!(baselines, [Some(TaskId::new(1)), Some(older), None]);
    assert_eq!(
        q.show(TaskId::new(1)).unwrap().runs[0].status(),
        dagq::domain::RunStatus::AwaitingIntegration
    );
    assert_folded_matches_state(&mut q);
}
