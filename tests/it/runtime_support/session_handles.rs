//! The test backend's background handles judged as the production backend
//! judges them: open while the process the handle names shows the start
//! recorded in it, and one handle per run.
use super::*;

/// A handle is open while its process lives with the start it names: a
/// wrapper whose process ended is not open although nobody closed it, nor
/// is a handle whose pid is dead or shows another start; a stop stops the
/// process of a session the backend did not start.
#[test]
fn a_background_handle_is_open_while_its_process_lives_and_not_until_its_stop() {
    let (_dir, _repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "exit 0");
    let left = backend.stand_in();
    assert!(backend.exists(&left).unwrap());
    backend.stop_background(&left, StopRoute::Sweep).unwrap();
    assert!(!backend.exists(&left).unwrap());

    // A wrapper's process that ended, never closed.
    let ended = Stand::start().unwrap();
    let handle = ended.handle.clone();
    assert!(backend.exists(&handle).unwrap());
    drop(ended);
    assert!(!backend.exists(&handle).unwrap());
    assert!(!backend.closed().contains(&handle));

    // A dead pid, and a live pid with another start than the one recorded.
    let dead = BackgroundHandle::new(dead_pid(), "Thu Jan  1 00:00:00 1970");
    assert!(!backend.exists(&dead.to_string()).unwrap());
    let other = BackgroundHandle::new(std::process::id(), "Thu Jan  1 00:00:00 1970");
    assert!(!backend.exists(&other.to_string()).unwrap());
}

/// Two runs given the same wrapper pid by a test each get a handle of their
/// own when the pid is dead; with a live pid they would share one, and the
/// fixture stops the test.
#[test]
fn runs_given_one_pid_get_handles_of_their_own_or_the_fixture_stops() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    for title in ["second", "third"] {
        add_ready_task(&mut queue, title, &[]);
    }
    let pid = dead_pid();
    let first = orphan_run(&repo, &db, "owner", pid, pid);
    let second = orphan_run(&repo, &db, "owner", pid, pid);
    assert_ne!(background_session(&first), background_session(&second));

    let live = Stand::start().unwrap();
    orphan_run(&repo, &db, "owner", live.pid, live.pid);
    let shared = std::panic::catch_unwind(|| {
        let mut queue = SqliteQueue::open(&db).unwrap();
        let run = queue.run(first.id()).unwrap();
        record_background_start(&db, &mut queue, &run, "owner", &live.handle);
    });
    let message = shared.expect_err("a handle two runs record stops the test");
    let message = message
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_default();
    assert!(
        message.contains("is already the session of run"),
        "{message}"
    );
}
