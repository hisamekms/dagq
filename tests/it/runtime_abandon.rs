//! Runtime tests: a run the supervisor gives up on while it keeps the
//! session open through the review, a revise or the `/exit` (task 237).
use crate::common;
use crate::runtime_support;

use runtime_support::*;

/// A `revise` verdict whose request cannot be written (a directory is in
/// the way of `revise-1.txt`): the step fails in the review phase, with the
/// worker's session open.
fn unwritable_revise() -> TestReviewer {
    TestReviewer::new(&[format!(
        "mkdir ../revise-1.txt; {}",
        verdict("revise", &["add a line"], "one gap")
    )])
}

/// The supervisor asks the session of a run it gives up on to `/exit`, and
/// records so on the `runtime_error`: nothing is left open with nobody
/// watching it, and the run waits for a person's review and integrate.
#[test]
fn a_run_given_up_in_its_review_asks_its_session_to_exit() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = unwritable_revise();
    // Returns once the stub session exited on the `/exit`.
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"].as_array().unwrap().len(), 1, "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let errors = payloads(&detail, "runtime_error");
    assert_eq!(errors.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(errors[0]["lease_released"], true);
    assert_eq!(errors[0]["session"]["exit"], "sent", "{}", errors[0]);
    assert_eq!(
        errors[0]["session"]["workspace_id"],
        json!(run.workspace_id())
    );
    assert!(backend.exits_sent.load(Ordering::SeqCst) >= 1);
    // The session was open through the review and ended at the `/exit`
    // (the idle stub exits at nothing else). The `/exit` is typed before
    // `runtime_error` is recorded, and the stub's wrapper records
    // `session_exited` as soon as it sees it, so the two may come in either
    // order.
    let kinds = event_kinds(&detail);
    assert!(
        position(&kinds, "review_finished") < position(&kinds, "session_exited"),
        "{kinds:?}"
    );
    let status = runtime::status(&db).unwrap();
    let attention = run_attention_of(&status, run.id()).unwrap();
    assert_eq!(attention["next"], "review and integrate", "{status}");
}

/// A `/exit` that cannot be sent leaves the session to a person: the
/// `runtime_error` says so, and the run's attention is `exit the session`
/// until the session exits, when `review and integrate` comes back.
#[test]
fn a_session_that_cannot_be_asked_to_exit_is_a_persons_to_end() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(usize::MAX, Ordering::SeqCst);
    let reviewer = unwritable_revise();
    let outcome = {
        let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
        runtime::supervise_with_reviewer(
            &db,
            &repo,
            &backend,
            &claude_stub(&db),
            &reviewer,
            Path::new(env!("CARGO_BIN_EXE_dagq")),
            &supervise_options(4, true),
        )
        .unwrap()
    };
    assert_eq!(outcome["errors"].as_array().unwrap().len(), 1, "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let errors = payloads(&detail, "runtime_error");
    assert_eq!(errors[0]["session"]["exit"], "failed", "{}", errors[0]);
    assert!(
        !errors[0]["session"]["error"].as_str().unwrap().is_empty(),
        "{}",
        errors[0]
    );
    let status = runtime::status(&db).unwrap();
    let attention = run_attention_of(&status, run.id()).unwrap();
    assert_eq!(attention["next"], "exit the session", "{status}");
    assert_eq!(attention["kind"], "runtime_error");

    // A person ends the session: the run waits for its review again.
    backend.exit_unsent.store(0, Ordering::SeqCst);
    let workspace = run.workspace_id().unwrap().to_owned();
    backend.send_exit(&workspace).unwrap();
    backend.join();
    let status = runtime::status(&db).unwrap();
    let attention = run_attention_of(&status, run.id()).unwrap();
    assert_eq!(attention["next"], "review and integrate", "{status}");
}
