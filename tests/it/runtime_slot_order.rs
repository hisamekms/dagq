//! Runtime tests: the one line a fill pass gives its slots in
//! (ADR-t1850-1): the resumes of `needs_session` runs, the recovery jobs of
//! ended runs and the claims of ready tasks by the effective priority of
//! their task, and on the same priority a resume, then a recovery job,
//! then a claim. Which order the line takes is the unit test
//! `domain::slot_order::tests`; these read it through the supervisor, with
//! one slot. The runs in flight have no worktree, so their resume or
//! recovery job records its start and fails at once; the first start of
//! each kind is what the order is read from.
use crate::runtime_support;
use dagq::domain::{ClaimOutcome, LeaseToken};

use runtime_support::*;

/// A task of `priority` claimed outside the supervisor and left as a run
/// of `status` that no supervisor leases.
fn in_flight(db: &Path, repo: &Path, title: &str, priority: Priority, status: &str) -> TaskId {
    let mut queue = SqliteQueue::open(db).unwrap();
    let task = add_ready_task(&mut queue, title, &[]);
    queue.set_priority(task, Some(priority)).unwrap();
    let base = CommitSha::try_from(git_out(repo, &["rev-parse", "main"]).as_str()).unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&base, &LeaseToken::new("gone"))
        .unwrap()
    else {
        panic!("{title} was not claimed")
    };
    assert_eq!(run.task_id(), task);
    let raw = Connection::open(db).unwrap();
    raw.execute(
        "UPDATE task_runs SET status=?2, last_error='set aside by the test' WHERE id=?1",
        rusqlite::params![run.id(), status],
    )
    .unwrap();
    raw.execute("DELETE FROM run_leases WHERE run_id=?1", [run.id()])
        .unwrap();
    task
}

/// A ready task of `priority`.
fn ready(db: &Path, title: &str, priority: Priority) -> TaskId {
    let mut queue = SqliteQueue::open(db).unwrap();
    let task = add_ready_task(&mut queue, title, &[]);
    queue.set_priority(task, Some(priority)).unwrap();
    task
}

/// The fixture without its ready task, so each test chooses its own.
fn empty_fixture() -> (Fixture, PathBuf, PathBuf) {
    let (dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    (dir, repo, db)
}

/// The ID of the first `kind` event of a run of `task`.
fn first(db: &Path, kind: &str, task: TaskId) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT MIN(e.id) FROM run_events e JOIN task_runs r ON r.id = e.run_id
             WHERE e.kind=?1 AND r.task_id=?2",
            rusqlite::params![kind, task.as_i64()],
            |row| row.get::<_, Option<i64>>(0),
        )
        .unwrap()
        .unwrap_or_else(|| panic!("no {kind} for task {task}"))
}

fn supervise_one_slot(db: &Path, repo: &Path) {
    let backend = TestWorkspace::new(db, false, VALID_AGENT);
    supervise_with(db, repo, &backend, &supervise_options(1, true)).unwrap();
}

/// (a) A ready task of a higher effective priority is claimed before the
/// resume of a lower task's `needs_session` run: the claim takes the one
/// slot, and the resume waits for it.
#[test]
fn a_higher_ready_task_is_claimed_before_a_lower_resume() {
    let (_dir, repo, db) = empty_fixture();
    let parked = in_flight(&db, &repo, "parked", Priority::Low, "needs_session");
    let claimed = ready(&db, "ready", Priority::Normal);
    supervise_one_slot(&db, &repo);
    assert!(first(&db, "run_claimed", claimed) < first(&db, "resume_started", parked));
}

/// (b) The resume of a higher task's `needs_session` run starts before a
/// lower ready task is claimed.
#[test]
fn a_higher_resume_starts_before_a_lower_claim() {
    let (_dir, repo, db) = empty_fixture();
    let parked = in_flight(&db, &repo, "parked", Priority::High, "needs_session");
    let claimed = ready(&db, "ready", Priority::Normal);
    supervise_one_slot(&db, &repo);
    assert!(first(&db, "resume_started", parked) < first(&db, "run_claimed", claimed));
}

/// (c) On the same effective priority the resume comes first.
#[test]
fn on_the_same_priority_the_resume_starts_before_the_claim() {
    let (_dir, repo, db) = empty_fixture();
    let parked = in_flight(&db, &repo, "parked", Priority::Normal, "needs_session");
    let claimed = ready(&db, "ready", Priority::Normal);
    supervise_one_slot(&db, &repo);
    assert!(first(&db, "resume_started", parked) < first(&db, "run_claimed", claimed));
}

/// (d) The recovery job of a higher task's failed run starts before the
/// resume of a lower task's `needs_session` run, which the fill pass used
/// to start first whatever the priorities.
#[test]
fn a_higher_recovery_job_starts_before_a_lower_resume() {
    let (_dir, repo, db) = empty_fixture();
    let parked = in_flight(&db, &repo, "parked", Priority::Normal, "needs_session");
    let failed = in_flight(&db, &repo, "failed", Priority::High, "failed");
    supervise_one_slot(&db, &repo);
    assert!(first(&db, "triage_started", failed) < first(&db, "resume_started", parked));
}

/// ADR-t1850-1 decision 7: a person puts the task of a `needs_session` run
/// behind with `set-priority low`, and the next pass claims a normal ready
/// task before the resume; `--inherit` takes the goal's priority again
/// (here none, so `normal`).
#[test]
fn a_parked_task_set_low_while_in_progress_is_resumed_after_a_normal_claim() {
    let (_dir, repo, db) = empty_fixture();
    let parked = in_flight(&db, &repo, "parked", Priority::Normal, "needs_session");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let low = queue.set_priority(parked, Some(Priority::Low)).unwrap();
    assert_eq!(low.status(), TaskStatus::InProgress);
    let claimed = ready(&db, "ready", Priority::Normal);
    supervise_one_slot(&db, &repo);
    assert!(first(&db, "run_claimed", claimed) < first(&db, "resume_started", parked));
    let inherited = queue.set_priority(parked, None).unwrap();
    assert_eq!(inherited.priority(), Priority::Normal);
    assert_eq!(inherited.own_priority(), None);
}
