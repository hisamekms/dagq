//! The queue and its records for the queue tests (`tests/queue_*.rs`).

use std::path::Path;

use dagq::{
    application::{TaskReads, TaskStore},
    domain::{DraftOrigin, Priority, TaskId},
};
use dagq::{
    domain::search::{SearchQuery, SearchRef},
    domain::{CommitSha, NewGoal, NewTask},
    infrastructure::{schema::MIGRATIONS, sqlite::SqliteQueue},
};
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

pub const BASE: &str = "0123456789abcdef0123456789abcdef01234567";

pub fn base() -> CommitSha {
    CommitSha::try_from(BASE).unwrap()
}

pub fn new_task(title: &str) -> NewTask {
    NewTask {
        title: title.into(),
        description: "A small development task".into(),
        acceptance: "The regression test passes".into(),
        verification_commands: vec!["cargo test".into()],
        required_evidence: Vec::new(),
        paths: Vec::new(),
        priority: Default::default(),
        change: None,
        dependencies: vec![],
        goal_dependencies: Vec::new(),
        goal_id: None,
        context: String::new(),
        provider: None,
        worker_mode: Some(dagq::domain::worker::WorkerMode::Interactive),
        wait_for_build: false,
        execution_class: Default::default(),
    }
}

pub fn new_goal(title: &str) -> NewGoal {
    NewGoal {
        priority: Default::default(),
        title: title.into(),
        description: "One problem several tasks solve".into(),
        acceptance: "Every task landed and the feature works end to end".into(),
        constraints: "Keep the module boundary".into(),
        doc: Some("docs/adr/0009-goal-groups-tasks.md".into()),
        draft: false,
        tags: Vec::new(),
    }
}

/// Replace the queue at `db` with one made by only the first `version`
/// migrations, as a binary of that schema left it. The queue is built up
/// rather than cut down from the latest one, so a later migration does not
/// change what this makes.
pub fn queue_at_schema(db: &Path, version: usize) {
    for side in ["", "-wal", "-shm"] {
        let file = format!("{}{side}", db.display());
        if Path::new(&file).exists() {
            std::fs::remove_file(file).unwrap();
        }
    }
    let conn = Connection::open(db).unwrap();
    for migration in &MIGRATIONS[..version] {
        conn.execute_batch(migration).unwrap();
    }
    // What `migrate` writes with the migrations: the header and the floor.
    conn.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = {version};
         INSERT INTO schema_floor(singleton, floor) VALUES (1, {floor})
         ON CONFLICT(singleton) DO UPDATE SET floor = excluded.floor;",
        floor = dagq::infrastructure::schema::floor_for(version as i64),
    ))
    .unwrap();
}

/// The schema version the queue file at `db` records (`user_version`).
pub fn user_version(db: &Path) -> i64 {
    Connection::open(db)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

/// An old queue after `dagq migrate`: opening it alone is refused and
/// leaves its `user_version` as it was (ADR-0045 decision 5).
pub fn migrated(path: &Path) -> SqliteQueue {
    let version = |path: &Path| -> i64 {
        Connection::open(path)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    };
    let before = version(path);
    let error = SqliteQueue::open(path).err().unwrap().to_string();
    assert!(error.contains("run `dagq migrate`"), "{error}");
    assert_eq!(version(path), before);
    let report = SqliteQueue::migrate(path, None, 1_700_000_000).unwrap();
    assert_eq!(report.previous_version, before);
    assert_eq!(report.schema_version, SqliteQueue::SCHEMA_VERSION);
    // Every migration so far is breaking, so the old queue was copied first.
    let backup = report.backup.unwrap();
    assert!(backup.ends_with(format!("backups/queue-{before}-1700000000.sqlite3")));
    assert_eq!(version(&backup), before);
    SqliteQueue::open(path).unwrap()
}

/// A queue copied from the migrated template (task 1044).
pub fn fixture() -> (TempDir, SqliteQueue) {
    let dir = tempfile::tempdir().unwrap();
    let queue = super::template::queue(&dir.path().join("queue.db"));
    (dir, queue)
}

/// A queue `init` migrates from an empty file, for the tests of the schema
/// and the migrations themselves.
pub fn migrated_fixture() -> (TempDir, SqliteQueue) {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("queue.db")).unwrap();
    (dir, queue)
}

/// The migrations after schema `version` as `migrate` reports them, each
/// with whether it declares itself compatible: what a test expects without
/// naming the latest schema version (ADR-0067 decision 4).
pub fn pending_after(version: i64) -> Vec<(i64, bool)> {
    MIGRATIONS
        .iter()
        .enumerate()
        .skip(usize::try_from(version).unwrap())
        .map(|(index, migration)| {
            (
                index as i64 + 1,
                dagq::infrastructure::schema::is_compatible(migration),
            )
        })
        .collect()
}

pub fn search(
    queue: &SqliteQueue,
    terms: &str,
    adjust: impl FnOnce(&mut SearchQuery),
) -> Vec<String> {
    let mut query = SearchQuery {
        terms: terms.into(),
        limit: 20,
        ..SearchQuery::default()
    };
    adjust(&mut query);
    let page = queue.search(&query).unwrap();
    assert_eq!(page.total, page.hits.len());
    page.hits
        .iter()
        .map(|hit| {
            let id = match &hit.id {
                SearchRef::Id(id) => id.to_string(),
                SearchRef::Commit(sha) => sha.clone(),
            };
            format!(
                "{} {id} {} {}: {}",
                hit.kind.as_str(),
                hit.status.as_deref().unwrap_or("-"),
                hit.field,
                hit.excerpt
            )
        })
        .collect()
}

/// A draft as the runtime or a job registers it: `origin` with `material`.
pub fn runtime_draft(
    queue: &mut SqliteQueue,
    title: &str,
    goal: Option<dagq::domain::GoalId>,
    origin: DraftOrigin,
    mut material: Value,
) -> TaskId {
    let id = queue
        .add(NewTask {
            title: title.into(),
            description: format!("{title}: found outside the task"),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Some(Priority::Normal),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: goal,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Interactive),
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap()
        .id();
    if origin == DraftOrigin::FollowUp {
        material["source_goal_id"] = json!(goal);
        material["source_goal_state"] = json!(match goal {
            None => "none",
            Some(goal) if queue.show_goal(goal).unwrap().closed => "closed",
            Some(_) => "open",
        });
        material["source_goal_provenance"] = json!("recorded");
    }
    queue.record_draft_origin(id, origin, &material).unwrap();
    id
}

/// A connection to the queue at `db` for [`beat`], opened before the job
/// it writes beside starts, so the beat itself is only its statement.
pub fn heart(db: &Path) -> Connection {
    let conn = Connection::open(db).unwrap();
    conn.busy_timeout(dagq::application::QUEUE_BUSY_TIMEOUT)
        .unwrap();
    conn
}

/// A heartbeat's write on `conn` ([`heart`]) while another connection's
/// job writes the queue: waiting [`dagq::application::QUEUE_BUSY_TIMEOUT`]
/// for the lock, it adds a row to `beats` (created first by [`beats`])
/// holding what `seen` (a count) reads under its lock. Returns whether it
/// was written, how long it took and what it saw.
pub fn beat(conn: &Connection, seen: &str) -> (bool, std::time::Duration, i64) {
    let started = std::time::Instant::now();
    let written = conn
        .execute(&format!("INSERT INTO beats(seen) {seen}"), [])
        .is_ok();
    let took = started.elapsed();
    let seen = conn
        .query_row("SELECT coalesce(max(seen), -1) FROM beats", [], |r| {
            r.get(0)
        })
        .unwrap();
    (written, took, seen)
}

/// The table [`beat`] writes, in the queue at `db`.
pub fn beats(db: &Path) {
    Connection::open(db)
        .unwrap()
        .execute_batch("CREATE TABLE beats(seen INTEGER)")
        .unwrap();
}

/// Wait until `count` (a count) reads more than none, or `less` than
/// `from` when given, in the queue at `db`: a job's first transaction
/// committed.
pub fn until_committed(db: &Path, count: &str, from: Option<i64>) {
    let conn = Connection::open(db).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let now: i64 = conn.query_row(count, [], |r| r.get(0)).unwrap();
        if from.map_or(now > 0, |from| now < from) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "nothing was committed"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
