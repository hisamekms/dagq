//! Queue tests: what each migration does to the rows of an older queue.
use crate::common;
use dagq::domain::EventKind;
use dagq::domain::LeaseToken;

use std::sync::Mutex;

use dagq::{
    VERSION,
    application::TaskStore,
    domain::search::{SearchKind, SearchQuery},
    domain::{
        EventId, EvidenceCheck, GoalId, GoalStatus, Priority, RunId, RunStatus, SupervisorMode,
        TaskAction, TaskChange, TaskId, TaskStatus,
    },
    infrastructure::{
        schema::{MIGRATIONS, floor_for},
        sqlite::SqliteQueue,
    },
};
use rusqlite::Connection;

use common::queue::*;

#[test]
fn migration_to_v5_rebuilds_runs_moves_the_lease_and_keeps_foreign_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v3.db");
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch(include_str!("../../migrations/0001_queue.sql"))
        .unwrap();
    raw.execute_batch(include_str!("../../migrations/0002_supervisor.sql"))
        .unwrap();
    raw.execute_batch(include_str!("../../migrations/0003_workspace_close.sql"))
        .unwrap();
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 3).unwrap();
    raw.execute_batch(&format!(
        "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('rebuilt','','','[]','in_progress');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,result_commit,last_error)
         VALUES ('run-failed',1,'failed','claude','claude','{BASE}',NULL,'rejected');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,result_commit,workspace_closed_at)
         VALUES ('run-awaiting',1,'awaiting_integration','claude','claude','{BASE}','{BASE}',1700000000);
         INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (1,'run-failed','validation_finished','{{}}');
         INSERT INTO run_processes(run_id,role,pid,exited_at,exit_code) VALUES ('run-awaiting','wrapper',1,1,0);
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('orphaned','','','[]','in_progress');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,supervisor_token)
         VALUES ('run-orphan',2,'running','claude','claude','{BASE}','old-token');
         INSERT INTO supervisor_leases(singleton,token,pid,heartbeat_at) VALUES (1,'old-token',4242,1700000000);"
    ))
    .unwrap();
    drop(raw);
    let mut queue = migrated(&path);
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    // The queue-wide lease became the orphaned run's lease; the slot index is gone.
    let leases = queue.run_leases().unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].run_id.as_str(), "run-orphan");
    assert_eq!(leases[0].pid, 4242);
    assert_eq!(leases[0].heartbeat_at, 1700000000);
    assert!(
        queue
            .run_lease(&RunId::new("run-awaiting").unwrap())
            .unwrap()
            .is_none()
    );
    let raw = Connection::open(&path).unwrap();
    let objects: Vec<String> = raw
        .prepare("SELECT name FROM sqlite_master WHERE name IN ('supervisor_leases','one_executing_run_per_queue','run_leases','one_unfinished_run_per_task') ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(objects, ["one_unfinished_run_per_task", "run_leases"]);
    drop(raw);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(
        detail
            .runs
            .iter()
            .map(|r| r.id().as_str())
            .collect::<Vec<_>>(),
        ["run-failed", "run-awaiting"]
    );
    assert_eq!(detail.runs[0].last_error(), Some("rejected"));
    assert_eq!(detail.runs[1].status(), RunStatus::AwaitingIntegration);
    assert_eq!(detail.runs[1].workspace_closed_at(), Some(1700000000));
    assert_eq!(
        detail.events[0].run_id.as_ref().map(RunId::as_str),
        Some("run-failed")
    );
    assert_eq!(detail.processes[0].exit_code, Some(0));
    assert!(queue.candidates().unwrap().is_empty());
    // Enforcement is back on and the rebuilt table is the referenced one.
    let raw = Connection::open(&path).unwrap();
    raw.pragma_update(None, "foreign_keys", true).unwrap();
    assert!(
        raw.execute(
            "INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (1,'ghost','x','{}')",
            [],
        )
        .is_err()
    );
    assert!(
        raw.execute("DELETE FROM task_runs WHERE id='run-failed'", [])
            .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE task_runs SET status='integrated' WHERE id='run-awaiting'",
            []
        )
        .is_ok()
    );
}

#[test]
fn migration_to_v6_adds_the_integration_statuses_and_the_single_slot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v5.db");
    let raw = Connection::open(&path).unwrap();
    for migration in [
        include_str!("../../migrations/0001_queue.sql"),
        include_str!("../../migrations/0002_supervisor.sql"),
        include_str!("../../migrations/0003_workspace_close.sql"),
        include_str!("../../migrations/0004_integration.sql"),
        include_str!("../../migrations/0005_run_leases.sql"),
    ] {
        raw.execute_batch(migration).unwrap();
    }
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 5).unwrap();
    raw.execute_batch(&format!(
        "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('awaiting','','','[]','in_progress');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,result_commit)
         VALUES ('run-awaiting',1,'awaiting_integration','claude','claude','{BASE}','{BASE}');
         INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (1,'run-awaiting','validation_finished','{{}}');
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('running','','','[]','in_progress');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,supervisor_token)
         VALUES ('run-running',2,'running','claude','claude','{BASE}','tok');
         INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES ('run-running','tok',4242,1700000000);
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('other','','','[]','in_progress');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,result_commit)
         VALUES ('run-other',3,'awaiting_integration','claude','claude','{BASE}','{BASE}');"
    ))
    .unwrap();
    drop(raw);
    let mut queue = migrated(&path);
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().runs[0].status(),
        RunStatus::AwaitingIntegration
    );
    assert_eq!(
        queue.show(TaskId::new(2)).unwrap().runs[0].status(),
        RunStatus::Running
    );
    let leases = queue.run_leases().unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].run_id.as_str(), "run-running");
    assert_eq!(
        queue
            .next_awaiting_integration()
            .unwrap()
            .unwrap()
            .id()
            .as_str(),
        "run-awaiting"
    );
    let raw = Connection::open(&path).unwrap();
    raw.pragma_update(None, "foreign_keys", true).unwrap();
    let objects: Vec<String> = raw
        .prepare(
            "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'one_%' ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        objects,
        [
            "one_integrated_run_per_task",
            "one_integrating_run_per_queue",
            "one_unfinished_run_per_task"
        ]
    );
    // The new statuses are accepted; only one run may integrate at a time.
    raw.execute(
        "UPDATE task_runs SET status='integrating' WHERE id='run-awaiting'",
        [],
    )
    .unwrap();
    assert!(
        raw.execute(
            "UPDATE task_runs SET status='integrating' WHERE id='run-other'",
            []
        )
        .is_err()
    );
    raw.execute(
        "UPDATE task_runs SET status='needs_session' WHERE id='run-other'",
        [],
    )
    .unwrap();
    // A parked run still owns its task.
    assert!(
        raw.execute(
            "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
             VALUES ('extra',3,'claimed','claude','claude',?1)",
            [BASE],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (1,'ghost','x','{}')",
            [],
        )
        .is_err()
    );
    assert!(queue.candidates().unwrap().is_empty());
    assert!(
        queue
            .transition(TaskId::new(3), TaskAction::Cancel)
            .is_err()
    );
}

#[test]
fn migration_to_v7_adds_the_supervisor_registry_and_keeps_leases() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v6.db");
    let raw = Connection::open(&path).unwrap();
    for migration in [
        include_str!("../../migrations/0001_queue.sql"),
        include_str!("../../migrations/0002_supervisor.sql"),
        include_str!("../../migrations/0003_workspace_close.sql"),
        include_str!("../../migrations/0004_integration.sql"),
        include_str!("../../migrations/0005_run_leases.sql"),
        include_str!("../../migrations/0006_merge_queue.sql"),
    ] {
        raw.execute_batch(migration).unwrap();
    }
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 6).unwrap();
    raw.execute_batch(&format!(
        "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('running','','','[]','in_progress');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,supervisor_token)
         VALUES ('run-running',1,'running','claude','claude','{BASE}','tok');
         INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES ('run-running','tok',4242,1700000000);"
    ))
    .unwrap();
    assert!(raw.execute("SELECT count(*) FROM supervisors", []).is_err());
    drop(raw);
    let mut queue = migrated(&path);
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    // A v6 supervisor that was running has no registration; its lease is intact.
    assert!(queue.supervisors().unwrap().is_empty());
    let leases = queue.run_leases().unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].run_id.as_str(), "run-running");
    assert_eq!(leases[0].token, "tok");
    assert_eq!(leases[0].pid, 4242);
    assert_eq!(leases[0].heartbeat_at, 1700000000);
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().runs[0].status(),
        RunStatus::Running
    );

    // Registration: one row per token, a parallel limit of at least one,
    // heartbeat refreshed with the leases, removed only by deregistration.
    let before = queue.heartbeat(&LeaseToken::new("tok")).unwrap();
    assert_eq!(before, 1);
    let registered = queue
        .register_supervisor(&LeaseToken::new("sv"), 4243, 2, VERSION)
        .unwrap();
    assert_eq!(registered.token, "sv");
    assert_eq!(registered.pid, 4243);
    assert_eq!(registered.parallel, 2);
    assert!(registered.started_at > 1700000000);
    assert_eq!(registered.heartbeat_at, registered.started_at);
    // The process records its own build; `up` reads it back to decide
    // whether that supervisor is one of its own (ADR-0014).
    assert_eq!(registered.binary_version.as_deref(), Some(VERSION));
    assert!(
        queue
            .register_supervisor(&LeaseToken::new("sv"), 4243, 2, VERSION)
            .is_err()
    );
    assert!(
        queue
            .register_supervisor(&LeaseToken::new("zero"), 4244, 0, VERSION)
            .is_err()
    );
    let raw = Connection::open(&path).unwrap();
    raw.execute("UPDATE supervisors SET heartbeat_at=0 WHERE token='sv'", [])
        .unwrap();
    drop(raw);
    assert_eq!(queue.heartbeat(&LeaseToken::new("sv")).unwrap(), 0); // No lease, still refreshed.
    let listed = queue.supervisors().unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].heartbeat_at >= registered.started_at);
    assert_eq!(listed[0].binary_version.as_deref(), Some(VERSION));
    assert_eq!(queue.heartbeat(&LeaseToken::new("nobody")).unwrap(), 0);

    // The mode is `up`'s to record once the process has registered; a
    // supervisor started by hand keeps none, and only the two modes fit.
    assert_eq!(listed[0].mode, None);
    assert_eq!(listed[0].workspace_id, None);
    queue
        .set_supervisor_mode(&LeaseToken::new("sv"), SupervisorMode::InCmux, Some("ws-1"))
        .unwrap();
    let listed = queue.supervisors().unwrap();
    assert_eq!(listed[0].mode, Some(SupervisorMode::InCmux));
    assert_eq!(listed[0].workspace_id.as_deref(), Some("ws-1"));
    queue
        .set_supervisor_mode(&LeaseToken::new("sv"), SupervisorMode::Launchd, None)
        .unwrap();
    let listed = queue.supervisors().unwrap();
    assert_eq!(listed[0].mode, Some(SupervisorMode::Launchd));
    assert_eq!(listed[0].workspace_id, None);
    assert!(
        queue
            .set_supervisor_mode(&LeaseToken::new("nobody"), SupervisorMode::Launchd, None)
            .is_err()
    );
    // A mode outside the domain's fails the read (ADR-t876-1).
    let raw = Connection::open(&path).unwrap();
    raw.execute("UPDATE supervisors SET mode='by-hand' WHERE token='sv'", [])
        .unwrap();
    assert!(queue.supervisors().is_err());
    raw.execute("UPDATE supervisors SET mode='launchd' WHERE token='sv'", [])
        .unwrap();
    drop(raw);

    // A registration a pre-0010 binary wrote has no version at all, which
    // is not this binary's version either, so `up` replaces it like any
    // other mismatch.
    let raw = Connection::open(&path).unwrap();
    raw.execute(
        "INSERT INTO supervisors(token,pid,parallel) VALUES ('old',4245,1)",
        [],
    )
    .unwrap();
    drop(raw);
    let old = queue
        .supervisors()
        .unwrap()
        .into_iter()
        .find(|registration| registration.token == "old")
        .unwrap();
    assert_eq!(old.binary_version, None);
    assert!(
        queue
            .deregister_supervisor(&LeaseToken::new("old"))
            .unwrap()
    );

    assert!(queue.deregister_supervisor(&LeaseToken::new("sv")).unwrap());
    assert!(!queue.deregister_supervisor(&LeaseToken::new("sv")).unwrap());
    assert!(queue.supervisors().unwrap().is_empty());
    assert_eq!(queue.run_leases().unwrap().len(), 1);
}

#[test]
fn migration_from_v6_adds_goals_and_keeps_tasks_runs_and_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v6.db");
    let raw = Connection::open(&path).unwrap();
    for migration in [
        include_str!("../../migrations/0001_queue.sql"),
        include_str!("../../migrations/0002_supervisor.sql"),
        include_str!("../../migrations/0003_workspace_close.sql"),
        include_str!("../../migrations/0004_integration.sql"),
        include_str!("../../migrations/0005_run_leases.sql"),
        include_str!("../../migrations/0006_merge_queue.sql"),
    ] {
        raw.execute_batch(migration).unwrap();
    }
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 6).unwrap();
    raw.execute_batch(&format!(
        "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('landed','why','done','[\"cargo test\"]','completed');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,result_commit)
         VALUES ('run-landed',1,'integrated','claude','claude','{BASE}','{BASE}');
         INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (1,NULL,'task_created','{{}}');
         INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (1,'run-landed','run_claimed','{{}}');
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('waiting','','','[]','ready');
         INSERT INTO task_dependencies(task_id,predecessor_id) VALUES (2,1);
         INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (2,NULL,'task_created','{{}}');"
    ))
    .unwrap();
    drop(raw);
    let mut queue = migrated(&path);
    // Every migration from 0007 (supervisors) on is applied together, up to
    // the last one the binary lists (ADR-0067 decision 1).
    assert_eq!(SqliteQueue::SCHEMA_VERSION, MIGRATIONS.len() as i64);
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    assert_eq!(
        queue
            .session_workspace(dagq::domain::SessionRole::Inbox)
            .unwrap(),
        None
    );
    let landed = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(landed.task.title(), "landed");
    assert_eq!(landed.task.description(), "why");
    assert_eq!(landed.task.goal_id(), None);
    assert_eq!(landed.task.context(), "");
    assert!(landed.task.paths().is_empty());
    assert_eq!(landed.task.priority(), Priority::Normal);
    assert_eq!(landed.task.status(), TaskStatus::Completed);
    assert_eq!(landed.runs.len(), 1);
    assert_eq!(landed.runs[0].status(), RunStatus::Integrated);
    // Events keep their IDs, order and run reference across the rebuild.
    assert_eq!(
        landed
            .events
            .iter()
            .map(|e| (
                e.id.as_i64(),
                e.task_id.map(TaskId::as_i64),
                e.run_id.as_ref().map(RunId::as_str)
            ))
            .collect::<Vec<_>>(),
        [(1, Some(1), None), (2, Some(1), Some("run-landed"))]
    );
    let waiting = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(waiting.task.goal_id(), None);
    assert_eq!(waiting.task.context(), "");
    // The task dependency survives every migration; no goal dependency appears.
    assert_eq!(waiting.dependencies, [TaskId::new(1)]);
    assert!(waiting.goal_dependencies.is_empty());
    assert_eq!(waiting.events[0].id, EventId::new(3));
    assert_eq!(queue.candidates().unwrap()[0].id(), TaskId::new(2));
    assert!(queue.list_goals().unwrap().is_empty());
    // New goals and events continue the sequences; foreign keys are enforced.
    let goal = queue.add_goal(new_goal("after")).unwrap();
    assert_eq!(goal.id(), GoalId::new(1));
    assert_eq!(
        queue.show_goal(goal.id()).unwrap().events[0].id,
        EventId::new(4)
    );
    let raw = Connection::open(&path).unwrap();
    raw.pragma_update(None, "foreign_keys", true).unwrap();
    assert!(
        raw.execute("UPDATE tasks SET goal_id=99 WHERE id=2", [])
            .is_err()
    );
    // Which events may belong to neither a task nor a goal is the write
    // port's rule since the kinds were opened (ADR-0073 decision 22).
    raw.execute(
        "INSERT INTO run_events(kind,payload) VALUES ('orphan','{}')",
        [],
    )
    .unwrap();
    for kind in [
        "backend_call_failed",
        "observe_started",
        "observe_finished",
        "ask_opened",
        "ask_answered",
    ] {
        raw.execute(
            "INSERT INTO run_events(kind,payload) VALUES (?1,'{}')",
            [kind],
        )
        .unwrap();
    }
    // Which asks may belong to no task is the write port's rule too.
    raw.execute(
        "INSERT INTO asks(kind,question,asked_by,reason_category)
         VALUES ('blocked','slots idle','observer','scope')",
        [],
    )
    .unwrap();
    raw.execute(
        "INSERT INTO asks(kind,question,asked_by,reason_category)
         VALUES ('decide','x','observer','recovery_failed')",
        [],
    )
    .unwrap();
    // 0029: every ask has a reason; that authentication and cost are the
    // queue_hold asks' alone is the write port's rule now.
    raw.execute(
        "INSERT INTO asks(kind,question,asked_by,reason_category,affected)
         VALUES ('queue_hold','log in','supervisor','authentication','[\"run-landed\"]')",
        [],
    )
    .unwrap();
    for (kind, reason) in [("queue_hold", "scope"), ("blocked", "cost")] {
        raw.execute(
            "INSERT INTO asks(kind,question,asked_by,reason_category) VALUES (?1,'x','observer',?2)",
            [kind, reason],
        )
        .unwrap();
    }
    assert!(
        raw.execute(
            "INSERT INTO asks(kind,question,asked_by) VALUES ('blocked','x','observer')",
            []
        )
        .is_err()
    );
    // 0017: the supervisor's stuck_exit ask about a run.
    raw.execute(
        "INSERT INTO asks(kind,task_id,run_id,question,asked_by,reason_category)
         VALUES ('stuck_exit',1,'run-landed','session did not exit','supervisor','recovery_failed')",
        [],
    )
    .unwrap();
    // The reason's values, an event's run without its task and a verdict
    // without its close were CHECKs until ADR-t876-1; the domain and the
    // write port keep them now (docs/design/persistence.md).
}

#[test]
fn goals_of_a_version_12_queue_migrate_as_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v12.db");
    let raw = Connection::open(&path).unwrap();
    for migration in [
        include_str!("../../migrations/0001_queue.sql"),
        include_str!("../../migrations/0002_supervisor.sql"),
        include_str!("../../migrations/0003_workspace_close.sql"),
        include_str!("../../migrations/0004_integration.sql"),
        include_str!("../../migrations/0005_run_leases.sql"),
        include_str!("../../migrations/0006_merge_queue.sql"),
        include_str!("../../migrations/0007_supervisors.sql"),
        include_str!("../../migrations/0008_goals.sql"),
        include_str!("../../migrations/0009_supervisor_mode.sql"),
        include_str!("../../migrations/0010_supervisor_binary_version.sql"),
        include_str!("../../migrations/0011_session_workspaces.sql"),
        include_str!("../../migrations/0012_queue_events.sql"),
    ] {
        raw.execute_batch(migration).unwrap();
    }
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 12).unwrap();
    raw.execute_batch(
        "INSERT INTO goals(title) VALUES ('existing');
         INSERT INTO tasks(title,description,acceptance,verification_commands,status,goal_id)
         VALUES ('in goal','','','[]','ready',1);",
    )
    .unwrap();
    drop(raw);
    let mut queue = migrated(&path);
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    // 0015: an existing task requires no evidence.
    assert!(
        queue
            .show(TaskId::new(1))
            .unwrap()
            .task
            .required_evidence()
            .is_empty()
    );
    let goal = queue.show_goal(GoalId::new(1)).unwrap().goal;
    assert_eq!(goal.status(), GoalStatus::Open);
    assert_eq!(queue.list_goals().unwrap()[0].status, GoalStatus::Open);
    assert_eq!(queue.candidates().unwrap()[0].id(), TaskId::new(1));
    // A status outside the domain's fails the read (ADR-t876-1).
    let raw = Connection::open(&path).unwrap();
    raw.execute("UPDATE goals SET status='closed' WHERE id=1", [])
        .unwrap();
    assert!(queue.show_goal(GoalId::new(1)).is_err());
}

#[test]
fn migration_to_v21_keeps_drafts_and_the_task_id_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v20.db");
    let raw = Connection::open(&path).unwrap();
    for migration in [
        include_str!("../../migrations/0001_queue.sql"),
        include_str!("../../migrations/0002_supervisor.sql"),
        include_str!("../../migrations/0003_workspace_close.sql"),
        include_str!("../../migrations/0004_integration.sql"),
        include_str!("../../migrations/0005_run_leases.sql"),
        include_str!("../../migrations/0006_merge_queue.sql"),
        include_str!("../../migrations/0007_supervisors.sql"),
        include_str!("../../migrations/0008_goals.sql"),
        include_str!("../../migrations/0009_supervisor_mode.sql"),
        include_str!("../../migrations/0010_supervisor_binary_version.sql"),
        include_str!("../../migrations/0011_session_workspaces.sql"),
        include_str!("../../migrations/0012_queue_events.sql"),
        include_str!("../../migrations/0013_goal_draft.sql"),
        include_str!("../../migrations/0014_asks.sql"),
        include_str!("../../migrations/0015_task_required_evidence.sql"),
        include_str!("../../migrations/0016_observer.sql"),
        include_str!("../../migrations/0017_stuck_exit_ask.sql"),
        include_str!("../../migrations/0018_task_paths.sql"),
        include_str!("../../migrations/0019_task_goal_dependencies.sql"),
        include_str!("../../migrations/0020_task_priority.sql"),
    ] {
        raw.execute_batch(migration).unwrap();
    }
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 20).unwrap();
    raw.execute_batch(&format!(
        "INSERT INTO goals(title) VALUES ('existing');
         INSERT INTO tasks(title,description,acceptance,verification_commands,status,goal_id,
                           context,required_evidence,paths,priority)
         VALUES ('kept draft','d','a','[\"cargo test\"]','draft',1,'c','[\"e2e\"]','[\"src/**\"]',3);
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('landed','','','[]','completed');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('run-landed',2,'integrated','claude','claude','{BASE}');
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('waiting','','','[]','ready');
         INSERT INTO task_dependencies(task_id,predecessor_id) VALUES (3,2);
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('gone','','','[]','draft');
         DELETE FROM tasks WHERE id=4;"
    ))
    .unwrap();
    drop(raw);
    let mut queue = migrated(&path);
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    let kept = queue.show(TaskId::new(1)).unwrap().task;
    assert_eq!(kept.status(), TaskStatus::Draft);
    assert_eq!(
        (kept.context(), kept.paths(), kept.priority()),
        ("c", &["src/**".to_owned()][..], Priority::Urgent)
    );
    assert_eq!(kept.required_evidence(), [EvidenceCheck::E2e]);
    assert_eq!(kept.goal_id(), Some(GoalId::new(1)));
    assert_eq!(
        queue.show(TaskId::new(3)).unwrap().task.status(),
        TaskStatus::Ready
    );
    assert_eq!(queue.candidates().unwrap().len(), 1);
    assert!(queue.proposals(true).unwrap().is_empty());
    // The deleted task's ID is never handed out again.
    assert_eq!(queue.add(new_task("new")).unwrap().id(), TaskId::new(5));
    let raw = Connection::open(&path).unwrap();
    // A status outside the domain's fails the read (ADR-t876-1).
    raw.execute(
        "INSERT INTO tasks(id,title,description,acceptance,verification_commands,status)
         VALUES (6,'x','','','[]','bogus')",
        [],
    )
    .unwrap();
    assert!(queue.show(TaskId::new(6)).is_err());
    let violations: i64 = raw
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
}

#[test]
fn migration_to_v28_gives_the_follow_up_drafts_already_queued_their_origin() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..27] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = 27;
         INSERT INTO schema_floor(singleton, floor) VALUES (1, 27);
         INSERT INTO tasks(title,description,acceptance,verification_commands,status,updated_at)
         VALUES ('source','','a','[]','completed','2026-09-02T00:00:00.000Z'),
                ('follow','later','','[]','draft','2026-09-02T00:00:00.000Z'),
                ('mine','','','[]','draft','2026-09-02T00:00:00.000Z');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('run-1',1,'integrated','claude','claude','{BASE}');
         INSERT INTO task_leases(task_id,supervisor_token,reason,heartbeat_at)
         VALUES (2,'old','follow_up_triage',1);
         INSERT INTO run_events(task_id,run_id,kind,payload) VALUES
           (1,'run-1','follow_up_registered','{{\"task_id\":2,\"title\":\"follow\",\"index\":0}}'),
           (1,'run-1','follow_up_registered','{{\"task_id\":null,\"index\":1,\"skipped\":\"x\"}}');"
    ))
    .unwrap();
    SqliteQueue::migrate(&path, None, 0).unwrap();
    let queue = SqliteQueue::open(&path).unwrap();
    // The follow_up draft from before gets a planner like a new one; the
    // person's draft does not.
    let (origin, material) = queue.draft_origin(TaskId::new(2)).unwrap().unwrap();
    assert_eq!(origin, dagq::domain::DraftOrigin::FollowUp);
    assert_eq!(
        material,
        serde_json::json!({"source_task_id": 1, "source_run_id": "run-1", "index": 0})
    );
    assert!(queue.draft_origin(TaskId::new(3)).unwrap().is_none());
    let targets: Vec<TaskId> = queue
        .planner_drafts()
        .unwrap()
        .iter()
        .map(|t| t.task.id())
        .collect();
    assert_eq!(targets, [TaskId::new(2)]);
    // The follow-up triage's leases are gone with it.
    let leases: i64 = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='task_leases'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leases, 0);
}

#[test]
fn migration_indexes_the_existing_rows_and_landings_record_their_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..25] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = 25;
         INSERT INTO schema_floor(singleton, floor) VALUES (1, 25);
         INSERT INTO goals(title, constraints, updated_at)
         VALUES ('古い goal', '互換を宣言する', '2026-09-01T00:00:00.000Z');
         INSERT INTO tasks(title,description,acceptance,verification_commands,status,goal_id,
                           updated_at)
         VALUES ('古い task','着地済みの変更','','[]','completed',1,'2026-09-02T00:00:00.000Z'),
                ('second','','','[]','completed',NULL,'2026-09-02T00:00:00.000Z');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('run-1',1,'integrated','claude','claude','{BASE}'),
                ('run-2',2,'integrated','claude','claude','{BASE}');
         INSERT INTO run_events(task_id,goal_id,run_id,kind,payload) VALUES
           (1,NULL,'run-1','observation','{{\"text\":\"古いメモ\",\"kind\":\"note\",\"by\":\"human\"}}'),
           (1,NULL,'run-1','run_integrated',
            '{{\"result_commit\":\"aaaa\",\"message\":\"docs: 古い task\\n\\nsummary を書いた\",\"git_common_dir\":\"/repo/.git\"}}'),
           (2,NULL,'run-2','run_integrated','{{\"result_commit\":\"bbbb\"}}');"
    ))
    .unwrap();
    let report = SqliteQueue::migrate(&path, None, 0).unwrap();
    assert_eq!(
        report
            .applied
            .iter()
            .map(|m| (m.version, m.compatible))
            .collect::<Vec<_>>(),
        pending_after(25)
    );
    assert_eq!(
        report.applied[..6]
            .iter()
            .map(|m| (m.version, m.compatible))
            .collect::<Vec<_>>(),
        [
            (26, true),
            (27, false),
            (28, false),
            (29, false),
            (30, false),
            (31, true)
        ]
    );
    // 0027 (plan review), 0028 (draft planners), 0029 (ask reasons) and 0030
    // (findings) are applied with it and are breaking: a copy is taken and the
    // floor rises to the last breaking one. 0031 (the supervisor handoff) is
    // compatible.
    assert!(report.backup.is_some());
    assert_eq!(report.floor, floor_for(SqliteQueue::SCHEMA_VERSION));
    assert!(report.floor >= 30);
    let mut queue = SqliteQueue::open(&path).unwrap();
    assert_eq!(
        search(&queue, "古い", |_| {}),
        [
            "commit aaaa completed message: docs: «古い» task\n\nsummary を書いた",
            "note 1 completed text: «古い»メモ",
            "task 1 completed title: «古い» task",
            "goal 1 open title: «古い» goal",
        ]
    );
    let page = queue
        .search(&SearchQuery {
            terms: "summary".into(),
            kinds: vec![SearchKind::Commit],
            goal_id: Some(GoalId::new(1)),
            limit: 5,
            ..SearchQuery::default()
        })
        .unwrap();
    let hit = &page.hits[0];
    assert_eq!(
        (hit.task_id, hit.run_id.as_deref(), hit.title.as_str()),
        (Some(1), Some("run-1"), "docs: 古い task")
    );
    // The landing recorded without its message is filled from Git.
    let seen = Mutex::new(Vec::new());
    let filled = queue
        .fill_commit_messages(|dir, commit| {
            seen.lock()
                .unwrap()
                .push((dir.map(str::to_owned), commit.to_owned()));
            Some("fix: 読み直した message".into())
        })
        .unwrap();
    assert_eq!(filled, 1);
    assert_eq!(seen.into_inner().unwrap(), [(None, "bbbb".to_owned())]);
    assert_eq!(
        search(&queue, "読み直した", |_| {}),
        ["commit bbbb completed message: fix: «読み直した» message"]
    );
    assert_eq!(queue.fill_commit_messages(|_, _| None).unwrap(), 0);

    // A later landing is recorded with the event that records it.
    drop(queue);
    raw.execute_batch(&format!(
        "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('third','','','[]','completed');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('run-3',3,'integrated','claude','claude','{BASE}');
         INSERT INTO run_events(task_id,run_id,kind,payload) VALUES
           (3,'run-3','run_integrated','{{\"result_commit\":\"cccc\",\"message\":\"feat: 新しい着地\"}}');"
    ))
    .unwrap();
    let queue = SqliteQueue::open(&path).unwrap();
    assert_eq!(
        search(&queue, "新しい着地", |_| {}),
        ["commit cccc completed message: feat: «新しい着地»"]
    );
}

#[test]
fn migration_to_v29_gives_every_ask_the_reason_of_its_kind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..28] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = 28;
         INSERT INTO schema_floor(singleton, floor) VALUES (1, 28);
         INSERT INTO tasks(title,description,acceptance,verification_commands,status,updated_at)
         VALUES ('t','','a','[]','in_progress','2026-09-02T00:00:00.000Z');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('run-1',1,'running','claude','claude','{BASE}');
         INSERT INTO asks(kind,task_id,run_id,question,asked_by) VALUES
           ('approve_landing',1,'run-1','land?','supervisor'),
           ('worker_question',1,'run-1','which?','worker'),
           ('stalled',1,'run-1','idle','supervisor'),
           ('decide',1,NULL,'retry?','supervisor');
         INSERT INTO asks(kind,question,asked_by) VALUES ('blocked','slots idle','observer');"
    ))
    .unwrap();
    SqliteQueue::migrate(&path, None, 0).unwrap();
    let queue = SqliteQueue::open(&path).unwrap();
    let reasons: Vec<(String, String)> = queue
        .asks(dagq::infrastructure::asks::AskQuery {
            all: true,
            ..Default::default()
        })
        .unwrap()
        .iter()
        .map(|a| {
            (
                a.kind.as_str().to_owned(),
                a.reason_category.as_str().to_owned(),
            )
        })
        .collect();
    let pairs: Vec<(&str, &str)> = reasons
        .iter()
        .map(|(k, r)| (k.as_str(), r.as_str()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("approve_landing", "scope"),
            ("worker_question", "scope"),
            ("stalled", "recovery_failed"),
            ("decide", "recovery_failed"),
            ("blocked", "scope"),
        ]
    );
}

/// ADR-t980-1 decision 1: the task's kind is gone. The migration that
/// drops `tasks.kind` is breaking (a copy is taken and the floor rises to
/// it); the tasks of an older queue keep their change, and the migrated
/// queue reads and writes tasks and derives `stats`, `kpi` and `forecast`.
#[test]
fn migration_dropping_the_task_kind_keeps_tasks_and_their_change() {
    // Found by its statement, not its number, which a landing may change.
    let at = MIGRATIONS
        .iter()
        .position(|migration| migration.contains("ALTER TABLE tasks DROP COLUMN kind"))
        .unwrap();
    let before = i64::try_from(at).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..at] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = {before};
         INSERT INTO schema_floor(singleton, floor) VALUES (1, {floor});
         INSERT INTO tasks(title,description,acceptance,verification_commands,status,kind,change,
                           updated_at)
         VALUES ('landed','','','[]','completed','runtime','fix','2026-09-28T00:00:00.000Z'),
                ('open','','','[]','draft','docs',NULL,'2026-09-28T00:00:00.000Z');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,
                               result_commit)
         VALUES ('run-1',1,'integrated','claude','claude','{BASE}','{BASE}');
         INSERT INTO run_events(task_id,run_id,kind,payload,created_at) VALUES
           (1,'run-1','run_claimed','{{}}','2026-09-28T01:00:00.000Z'),
           (1,'run-1','run_integrated','{{\"result_commit\":\"aaaa\"}}',
            '2026-09-28T02:00:00.000Z');",
        floor = floor_for(before),
    ))
    .unwrap();
    drop(raw);
    let report = SqliteQueue::migrate(&path, None, 0).unwrap();
    assert!(
        report
            .applied
            .iter()
            .any(|m| m.version == before + 1 && !m.compatible)
    );
    assert!(report.backup.is_some());
    assert!(report.floor > before);
    let raw = Connection::open(&path).unwrap();
    let columns: Vec<String> = raw
        .prepare("SELECT name FROM pragma_table_info('tasks')")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(!columns.iter().any(|name| name == "kind"), "{columns:?}");
    assert!(columns.iter().any(|name| name == "change"), "{columns:?}");
    drop(raw);

    let mut queue = SqliteQueue::open(&path).unwrap();
    let landed = queue.show(TaskId::new(1)).unwrap().task;
    assert_eq!(landed.change().map(TaskChange::as_str), Some("fix"));
    assert_eq!(queue.show(TaskId::new(2)).unwrap().task.change(), None);
    let mut declared = new_task("added");
    declared.change = Some("docs".parse::<TaskChange>().unwrap());
    let added = queue.add(declared).unwrap();
    assert_eq!(added.change().map(TaskChange::as_str), Some("docs"));
    drop(queue);

    // The CLI on the migrated queue: no kind anywhere, the change kept.
    let config = dir.path().join("config");
    let run = |args: &[&str]| {
        let output = common::cli::invoke_with(
            &[("XDG_CONFIG_HOME", config.to_str().unwrap()), ("TZ", "UTC")],
            &path,
            args,
        );
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let edited = run(&["edit", "2", "--change", "fix"]);
    assert_eq!(edited["change"], "fix");
    assert!(edited.get("kind").is_none(), "{edited}");
    let listed = run(&["list", "--all"]);
    assert_eq!(listed["total"], 3);
    assert!(
        listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task.get("kind").is_none())
    );
    let stats = run(&["stats", "--full"]);
    assert_eq!(stats["runs"][0]["change"], "fix");
    assert!(stats.get("kinds").is_none(), "{stats}");
    assert_eq!(stats["changes"][0]["change"], "fix");
    let kpi = run(&[
        "kpi",
        "--since",
        "2026-09-27T00:00:00Z",
        "--until",
        "2026-09-29T00:00:00Z",
    ]);
    let landings = &kpi["periods"][0]["kpis"]["landings"];
    assert_eq!(landings["change=fix"]["value"], 1.0, "{landings}");
    assert!(
        landings
            .as_object()
            .unwrap()
            .keys()
            .all(|stratum| !stratum.starts_with("kind=")),
        "{landings}"
    );
    let forecast = run(&["forecast"]);
    assert!(forecast.get("tasks").is_some(), "{forecast}");
}

/// ADR-t980-1: the task change is an addition. The tasks of an older queue
/// have none after the migration, which raises no floor; an older binary's
/// insert leaves it null, and a value that is not a label reads as none.
#[test]
fn migration_adding_the_task_change_keeps_older_tasks_without_one() {
    // Found by its statement, not its number, which a landing may change.
    let at = MIGRATIONS
        .iter()
        .position(|migration| migration.contains("ALTER TABLE tasks ADD COLUMN change"))
        .unwrap();
    let before = i64::try_from(at).unwrap();
    assert_eq!(floor_for(before + 1), floor_for(before));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..at] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = {before};
         INSERT INTO schema_floor(singleton, floor) VALUES (1, {floor});
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('older','','','[]','draft');",
        floor = floor_for(before),
    ))
    .unwrap();
    drop(raw);
    SqliteQueue::migrate(&path, None, 0).unwrap();
    let mut queue = SqliteQueue::open(&path).unwrap();
    assert_eq!(queue.show(TaskId::new(1)).unwrap().task.change(), None);
    let mut declared = new_task("fix");
    declared.change = Some("fix".parse::<TaskChange>().unwrap());
    let added = queue.add(declared).unwrap();
    assert_eq!(added.change().map(TaskChange::as_str), Some("fix"));
    let raw = Connection::open(&path).unwrap();
    raw.execute(
        "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('older binary','','','[]','draft')",
        [],
    )
    .unwrap();
    assert_eq!(queue.show(TaskId::new(3)).unwrap().task.change(), None);
    raw.execute("UPDATE tasks SET change='Not a label' WHERE id=3", [])
        .unwrap();
    assert_eq!(queue.show(TaskId::new(3)).unwrap().task.change(), None);
    assert_eq!(queue.task_changes().unwrap()[&TaskId::new(3)], None);
    assert_eq!(
        queue.task_changes().unwrap()[&TaskId::new(2)]
            .as_ref()
            .map(TaskChange::as_str),
        Some("fix")
    );
}

/// Task 325: the answerer and the chosen option of an ask are additions.
/// An ask answered before the migration reads with both null (unknown),
/// shown as null in its JSON, and an older binary's answer that names
/// neither leaves them null too.
#[test]
fn migration_adding_the_answerer_keeps_older_answers_unknown() {
    let at = MIGRATIONS
        .iter()
        .position(|migration| migration.contains("ALTER TABLE asks ADD COLUMN answered_by"))
        .unwrap();
    let before = i64::try_from(at).unwrap();
    assert_eq!(floor_for(before + 1), floor_for(before));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..at] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = {before};
         INSERT INTO schema_floor(singleton, floor) VALUES (1, {floor});
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('t','','','[]','draft');
         INSERT INTO asks(kind,task_id,question,options,asked_by,reason_category,answer,answered_at)
         VALUES ('decide',1,'retry?','[\"retry\",\"cancel\"]','supervisor','recovery_failed','retry',1),
                ('decide',1,'again?','[\"retry\"]','supervisor','recovery_failed',NULL,NULL);",
        floor = floor_for(before),
    ))
    .unwrap();
    drop(raw);
    SqliteQueue::migrate(&path, None, 0).unwrap();
    let mut queue = SqliteQueue::open(&path).unwrap();
    let older = queue.read_ask(dagq::domain::AskId::new(1)).unwrap();
    assert_eq!(older.answer.as_deref(), Some("retry"));
    assert_eq!((older.answered_by, older.option_index), (None, None));
    let json = serde_json::to_value(queue.read_ask(dagq::domain::AskId::new(1)).unwrap()).unwrap();
    assert!(
        json["answered_by"].is_null() && json["option_index"].is_null(),
        "{json}"
    );
    // An older binary answers without naming the columns.
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE asks SET answer='retry', answered_at=2 WHERE id=2",
            [],
        )
        .unwrap();
    let answered = queue.read_ask(dagq::domain::AskId::new(2)).unwrap();
    assert_eq!((answered.answered_by, answered.option_index), (None, None));
    // This binary records both.
    let asked = queue
        .ask(dagq::domain::NewAsk {
            topics: Vec::new(),
            kind: dagq::domain::AskKind::Decide,
            task_id: Some(TaskId::new(1)),
            run_id: None,
            question: "which?".into(),
            options: vec!["retry".into(), "cancel".into()],
            asked_by: "supervisor".into(),
            reason_category: dagq::domain::AskReason::RecoveryFailed,
            finding_id: None,
        })
        .unwrap()
        .ask;
    let answered = queue
        .answer_as(asked.id, " cancel ", dagq::domain::Answerer::INBOX)
        .unwrap();
    assert_eq!(answered.answered_by.as_deref(), Some("inbox"));
    assert_eq!(answered.option_index, Some(1));
    assert_eq!(
        answered.answer_authority,
        Some(dagq::domain::AnswerAuthority::Delegated)
    );
    assert_eq!(answered.answer_approval, Some(true));
}

#[test]
fn migration_opening_the_kinds_keeps_rows_and_moves_their_rules_to_the_write_port() {
    use dagq::domain::{AskKind, AskReason, NewAsk};
    // ADR-0073 decisions 19-23, found by what it creates so a renumbering
    // on landing does not move it.
    let open = MIGRATIONS
        .iter()
        .position(|m| m.contains("CREATE TABLE asks_v39"))
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..open] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = {open};
         UPDATE schema_floor SET floor = {floor};
         INSERT INTO tasks(title,description,acceptance,verification_commands,status,updated_at)
         VALUES ('t','','a','[]','in_progress','2026-09-02T00:00:00.000Z');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('run-1',1,'running','claude','claude','{BASE}');
         INSERT INTO asks(id,kind,task_id,run_id,question,asked_by,reason_category,
                          answer,answered_at,answered_by,option_index) VALUES
           (7,'worker_question',1,'run-1','which?','worker','scope','a',5,'inbox',0);
         INSERT INTO asks(id,kind,question,asked_by,reason_category,subject,affected) VALUES
           (9,'queue_hold','log in','supervisor','authentication','login','[\"run-1\"]');
         INSERT INTO run_events(id,kind,payload) VALUES (40,'mark_recorded','{{}}');
         INSERT INTO run_events(id,task_id,run_id,kind,payload)
         VALUES (41,1,'run-1','agent_started','{{}}');",
        floor = floor_for(open as i64),
    ))
    .unwrap();
    // Before it, the queue refuses a kind it does not list.
    assert!(
        raw.execute(
            "INSERT INTO run_events(kind,payload) VALUES ('later','{}')",
            []
        )
        .is_err()
    );
    SqliteQueue::migrate(&path, None, 0).unwrap();
    let mut queue = SqliteQueue::open(&path).unwrap();
    let kinds: Vec<(i64, String)> = raw
        .prepare("SELECT id, kind FROM asks ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        kinds,
        [(7, "worker_question".into()), (9, "queue_hold".into())]
    );
    let answered = queue.read_ask(dagq::domain::AskId::new(7)).unwrap();
    assert_eq!(
        (answered.answered_by.as_deref(), answered.option_index),
        (Some("inbox"), Some(0))
    );
    let events: Vec<i64> = raw
        .prepare("SELECT id FROM run_events WHERE id IN (40, 41) ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(events, [40, 41]);
    let checks: i64 = raw
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name IN ('asks','run_events')
             AND (sql LIKE '%kind IN%' OR sql LIKE '%kind =%')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(checks, 0);
    let one_open: i64 = raw
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'asks_open'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(one_open, 1);

    // The queue takes a kind a newer binary writes, and this one reads it.
    raw.execute_batch(
        "INSERT INTO asks(kind,task_id,question,asked_by,reason_category)
         VALUES ('later_kind',1,'what now?','supervisor','scope');
         INSERT INTO run_events(kind,payload) VALUES ('later_event','{}');",
    )
    .unwrap();
    let later = queue
        .asks(dagq::infrastructure::asks::AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|a| a.question == "what now?")
        .unwrap();
    assert_eq!(later.kind, AskKind::Other("later_kind".into()));
    assert!(!later.kind.is_known());

    // The write port keeps the rules the CHECKs held.
    let error = queue
        .record_queue_event(EventKind::TaskCreated, serde_json::json!({}))
        .unwrap_err();
    assert!(error.to_string().contains("needs a task, a goal or a run"));
    queue
        .record_queue_event(EventKind::MarkRecorded, serde_json::json!({}))
        .unwrap();
    let ask = |kind: AskKind, task: Option<i64>, run: Option<&str>| NewAsk {
        topics: Vec::new(),
        kind,
        task_id: task.map(TaskId::new),
        run_id: run.map(|r| RunId::try_from(r).unwrap()),
        question: "which?".into(),
        options: vec![],
        asked_by: "supervisor".into(),
        reason_category: AskReason::Scope,
        finding_id: None,
    };
    let refused = [
        ask(AskKind::Other("later_kind".into()), Some(1), None),
        ask(AskKind::QueueHold, None, None),
        ask(AskKind::Decide, None, None),
    ];
    for new in refused {
        assert!(queue.ask(new.clone()).is_err(), "{:?}", new.kind);
    }
    assert!(
        queue
            .ask(ask(AskKind::Blocked, None, None))
            .unwrap()
            .created
    );
    assert!(
        queue
            .ask(ask(AskKind::Decide, Some(1), None))
            .unwrap()
            .created
    );
}

#[test]
fn migration_opening_the_run_providers_keeps_runs_and_takes_a_codex_run() {
    use dagq::domain::{Provider, worker::WorkerMode};
    // Task 816, found by what it creates so a renumbering on landing does
    // not move it.
    let open = MIGRATIONS
        .iter()
        .position(|m| m.contains("CREATE TABLE task_runs_v49"))
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..open] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = {open};
         UPDATE schema_floor SET floor = {floor};
         INSERT INTO tasks(title,description,acceptance,verification_commands,status,updated_at)
         VALUES ('t','','a','[]','in_progress','2026-09-02T00:00:00.000Z');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,worker_mode)
         VALUES ('0d8e3f1a-7c1b-4e35-9a11-3f6d2c9b8e47',1,'running','claude','claude','{BASE}','headless');
         INSERT INTO run_events(task_id,run_id,kind,payload)
         VALUES (1,'0d8e3f1a-7c1b-4e35-9a11-3f6d2c9b8e47','agent_started','{{}}');",
        floor = floor_for(open as i64),
    ))
    .unwrap();
    // Before it, the queue refuses a run of any provider but claude.
    let codex = format!(
        "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('run-2',1,'failed','codex','codex','{BASE}')"
    );
    assert!(raw.execute(&codex, []).is_err());
    SqliteQueue::migrate(&path, None, 0).unwrap();
    let mut queue = SqliteQueue::open(&path).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Running);
    assert_eq!(run.requested_provider(), Provider::Claude);
    assert_eq!(run.worker_mode(), WorkerMode::Headless);
    assert_eq!(detail.events.len(), 1);
    let indexes: i64 = raw
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE tbl_name = 'task_runs' AND type = 'index'
             AND name IN ('runs_by_task','one_unfinished_run_per_task',
                          'one_integrated_run_per_task','one_integrating_run_per_queue')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(indexes, 4);
    // No CHECK is left on task_runs (ADR-t876-1).
    let sql: String = raw
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'task_runs'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!sql.contains("CHECK"), "{sql}");
    // The queue takes a Codex run now, and reads it.
    raw.execute(&codex, []).unwrap();
    let runs = queue.show(TaskId::new(1)).unwrap().runs;
    assert_eq!(runs[1].actual_provider(), Provider::Codex);
    // The rules the CHECKs held are the read's: a value outside the typed
    // status, provider or mode fails the read of its task (fail closed).
    for (task, status, provider, mode) in [
        (2, "failed", "gemini", "headless"),
        (3, "", "claude", "headless"),
        (4, "lost", "claude", "headless"),
        (5, "failed", "claude", "screen"),
    ] {
        raw.execute_batch(&format!(
            "INSERT INTO tasks(id,title,description,acceptance,verification_commands,status,updated_at)
             VALUES ({task},'t','','a','[]','in_progress','2026-09-02T00:00:00.000Z');
             INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,worker_mode)
             VALUES ('r{task}',{task},'{status}','{provider}','{provider}','{BASE}','{mode}');"
        ))
        .unwrap();
        assert!(
            queue.show(TaskId::new(task)).is_err(),
            "{status} {provider} {mode}"
        );
    }
    let floor: i64 = raw
        .query_row("SELECT floor FROM schema_floor", [], |r| r.get(0))
        .unwrap();
    assert!(floor > open as i64, "breaking: the floor rises to {floor}");
}

/// Every row of every table (its rowid first where it has one, apart from
/// sqlite_sequence, whose rows move), sorted, as text: what a table rebuild
/// must keep.
fn all_rows(conn: &Connection) -> Vec<(String, Vec<String>)> {
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    tables
        .into_iter()
        .map(|table| {
            let all = format!("SELECT * FROM \"{table}\"");
            let mut statement = if table == "sqlite_sequence" {
                conn.prepare(&all)
            } else {
                conn.prepare(&format!("SELECT rowid, * FROM \"{table}\""))
                    .or_else(|_| conn.prepare(&all))
            }
            .unwrap();
            let width = statement.column_count();
            let mut rows: Vec<String> = statement
                .query_map([], |r| {
                    (0..width)
                        .map(|i| {
                            r.get::<_, rusqlite::types::Value>(i)
                                .map(|v| format!("{v:?}"))
                        })
                        .collect::<Result<Vec<_>, _>>()
                        .map(|values| values.join("|"))
                })
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            rows.sort();
            (table, rows)
        })
        .collect()
}

/// The columns, foreign keys and indexes of every table, and the SQL of
/// every index and trigger: the schema apart from each table's own SQL.
fn structure(conn: &Connection) -> Vec<String> {
    let text = |sql: &str| -> Vec<String> {
        let mut statement = conn.prepare(sql).unwrap();
        let width = statement.column_count();
        statement
            .query_map([], |r| {
                (0..width)
                    .map(|i| {
                        r.get::<_, rusqlite::types::Value>(i)
                            .map(|v| format!("{v:?}"))
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(|values| values.join("|"))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let mut out = text(
        "SELECT type, name, tbl_name, sql FROM sqlite_master
         WHERE type IN ('index', 'trigger', 'view') ORDER BY name",
    );
    for table in text("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name") {
        let table = table.trim_start_matches("Text(\"").trim_end_matches("\")");
        out.push(format!("== {table}"));
        out.extend(text(&format!(
            "SELECT * FROM pragma_table_info('{table}') ORDER BY cid"
        )));
        out.extend(text(&format!(
            "SELECT * FROM pragma_foreign_key_list('{table}') ORDER BY id, seq"
        )));
        out.extend(text(&format!(
            "SELECT name, \"unique\", origin, partial FROM pragma_index_list('{table}')
             ORDER BY name"
        )));
    }
    out
}

#[test]
fn migration_dropping_every_check_keeps_rows_ids_indexes_triggers_and_keys() {
    // ADR-t876-1, found by what it creates so a renumbering on landing does
    // not move it.
    let open = MIGRATIONS
        .iter()
        .position(|m| m.contains("CREATE TABLE asks_v50"))
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..open] {
        raw.execute_batch(migration).unwrap();
    }
    // A row in every table that had a CHECK, with ids that are not 1, and a
    // deleted ask so the sequence runs ahead of the rows.
    raw.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = {open};
         UPDATE schema_floor SET floor = {floor};
         INSERT INTO queue_repository(singleton, git_common_dir) VALUES (1, '/repo/.git');
         INSERT INTO proposals(id,status,owner_origin,submitted_at,created_at,updated_at,review_hold)
         VALUES (2,'submitted','person','2026-09-02','2026-09-02','2026-09-02','concern');
         INSERT INTO goals(id,title,status,proposal_id,closed_at,verdict)
         VALUES (5,'g','open',2,'2026-09-03','achieved');
         INSERT INTO tasks(id,title,description,acceptance,verification_commands,status,updated_at,
                           goal_id,priority,worker_provider,worker_mode)
         VALUES (1,'first','d','a','[\"cargo test\"]','in_progress','2026-09-02T00:00:00.000Z',
                 5,3,'codex','headless'),
                (3,'third','d','a','[]','draft','2026-09-02T00:00:00.000Z',NULL,1,NULL,NULL);
         INSERT INTO task_dependencies(task_id, predecessor_id) VALUES (3, 1);
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('run-1',1,'running','claude','claude','{BASE}');
         INSERT INTO run_processes(run_id,role,pid,heartbeat_at) VALUES ('run-1','agent',42,7);
         INSERT INTO supervisors(token,pid,parallel,mode,providers)
         VALUES ('tok',11,3,'in_cmux','[\"claude\"]');
         INSERT INTO planners(id,origin,proposal_id,workspace_id,created_at,draft_task_id)
         VALUES (4,'person',2,'ws-1',1,3);
         INSERT INTO plan_reviews(id,proposal_id,attempt,supervisor_token,started_at,finished_at,
                                  outcome,verdict)
         VALUES (3,2,1,'tok',1,2,'pass','{{}}');
         INSERT INTO draft_origins(task_id,origin,material,created_at)
         VALUES (3,'follow_up','{{\"from\":1}}',1);
         INSERT INTO findings(id,kind,target,task_id,run_id,summary,first_seen_at,last_seen_at,
                              evidence,recorded_by,updated_at)
         VALUES (6,'stall','run',1,'run-1','stuck',1,2,'[40]','observer',2);
         INSERT INTO binary_updates(id,kind,payload) VALUES (2,'installed','{{}}');
         INSERT INTO asks(id,kind,task_id,run_id,question,asked_by,reason_category,
                          answer,answered_at,answered_by,option_index,finding_id)
         VALUES (7,'worker_question',1,'run-1','which?','worker','scope','a',5,'inbox',0,6);
         INSERT INTO asks(id,kind,question,asked_by,reason_category)
         VALUES (12,'blocked','gone','observer','scope');
         DELETE FROM asks WHERE id = 12;
         INSERT INTO run_events(id,task_id,run_id,kind,payload)
         VALUES (40,1,'run-1','observation','{{\"text\":\"a note to find\"}}');
         INSERT INTO run_events(id,task_id,run_id,kind,payload)
         VALUES (41,1,'run-1','run_integrated','{{\"result_commit\":\"{BASE}\",\"message\":\"landed\"}}');
         INSERT INTO draft_reopens(task_id,material,created_at) VALUES (3,'{{}}',1);
         INSERT INTO goal_reviews(id,goal_id,attempt,supervisor_token,fingerprint,started_at)
         VALUES (2,5,1,'tok','fp',1);
         INSERT INTO headless_jobs(id,kind,run_id,attempt,pid,supervisor_token,started_at)
         VALUES (8,'review','run-1',0,99,'tok',1);",
        floor = floor_for(open as i64),
    ))
    .unwrap();
    let checked = |conn: &Connection| -> i64 {
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE sql LIKE '%CHECK%'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert!(checked(&raw) > 0);
    let rows = all_rows(&raw);
    let before = structure(&raw);
    // Every table that had a CHECK holds a row.
    for table in [
        "tasks",
        "task_dependencies",
        "queue_repository",
        "run_processes",
        "supervisors",
        "goals",
        "proposals",
        "planners",
        "plan_reviews",
        "draft_origins",
        "findings",
        "binary_updates",
        "asks",
        "run_events",
        "draft_reopens",
        "goal_reviews",
        "headless_jobs",
        "landed_commits",
    ] {
        let (_, table_rows) = rows.iter().find(|(name, _)| name == table).unwrap();
        assert!(!table_rows.is_empty(), "{table}");
    }

    // This migration alone, as `migrate` applies each (foreign keys off
    // around one transaction), so a later migration that adds a column
    // does not change what is compared.
    raw.pragma_update(None, "foreign_keys", false).unwrap();
    raw.execute_batch(&format!(
        "BEGIN IMMEDIATE; {} PRAGMA user_version = {}; COMMIT;",
        MIGRATIONS[open],
        open + 1
    ))
    .unwrap();
    raw.pragma_update(None, "foreign_keys", true).unwrap();
    assert_eq!(checked(&raw), 0);
    // Rows, ids, rowids and the AUTOINCREMENT sequences are kept.
    assert_eq!(all_rows(&raw), rows);
    // Columns, NOT NULL, DEFAULT, keys, indexes and triggers are the same.
    assert_eq!(structure(&raw), before);
    let violations: i64 = raw
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
    // The rest through `migrate`: the floor rises past it (breaking).
    SqliteQueue::migrate(&path, None, 0).unwrap();
    let floor: i64 = raw
        .query_row("SELECT floor FROM schema_floor", [], |r| r.get(0))
        .unwrap();
    assert!(floor > open as i64, "breaking: the floor rises to {floor}");

    // The triggers still index, and the next id follows the deleted ask.
    let mut queue = SqliteQueue::open(&path).unwrap();
    assert_eq!(queue.show(TaskId::new(1)).unwrap().runs.len(), 1);
    raw.execute("UPDATE tasks SET title = 'renamed first' WHERE id = 1", [])
        .unwrap();
    let title: String = raw
        .query_row("SELECT title FROM search_index WHERE rowid = 4", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(title, "renamed first");
    raw.execute(
        "INSERT INTO asks(kind,task_id,question,asked_by,reason_category)
         VALUES ('decide',1,'next?','supervisor','scope')",
        [],
    )
    .unwrap();
    assert_eq!(raw.last_insert_rowid(), 13);
    // The foreign keys still hold once enforced.
    raw.pragma_update(None, "foreign_keys", true).unwrap();
    assert!(
        raw.execute(
            "INSERT INTO task_dependencies(task_id, predecessor_id) VALUES (3, 99)",
            []
        )
        .is_err()
    );
}
