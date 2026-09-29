//! Runtime tests: a run left `integrating` by a supervisor that died gives
//! the integration slot back on the next supervisor's pass, lands again
//! without a person when it was approved or passed, is reviewed again when
//! not, lands once a person's `recover` put it back, and is never landed
//! twice when its landing already moved main (task 1118).
use crate::runtime_support;
use dagq::domain::{EventKind, LeaseToken};
use std::time::Instant;

use runtime_support::*;

/// The agent of task `n`: a change of its own, so the landings of the
/// tasks do not conflict, and its receipt.
fn own_change(n: i64) -> String {
    format!(
        "printf '{n}\\n' > f{n}.txt && git add f{n}.txt && git commit -q -m t{n}; receipt \"$(git rev-parse HEAD)\""
    )
}

/// Task 1's run, validated and awaiting integration (its review raised a
/// concern whose ask a person closed), then given `verdict` by a later
/// review when there is one, and taken into the integration slot by a
/// supervisor that died: `integrating`, leased to a dead pid whose
/// heartbeat is old. Task 2, independent of it, is ready.
fn dead_landing(verdict: Option<&str>) -> (Fixture, PathBuf, PathBuf, TaskRun, TestWorkspace) {
    let (fixture, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.script_for(1, &own_change(1));
    backend.script_for(2, &own_change(2));
    let reviewer = TestReviewer::new(&[verdict_json("concern")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    for ask in queue.asks(Default::default()).unwrap() {
        queue.answer(ask.id, "withdrawn").unwrap();
        queue.close_ask(ask.id).unwrap();
    }
    if let Some(verdict) = verdict {
        queue
            .record_runtime_event(
                run.id(),
                EventKind::ReviewFinished,
                json!({"attempt": 2, "verdict": verdict, "reasons": [], "summary": "reviewed again"}),
            )
            .unwrap();
    }
    add_ready_task(&mut queue, "second", &[]);
    let main = git_out(&repo, &["rev-parse", "main"]);
    queue
        .begin_integration(run.id(), &LeaseToken::new("crashed"), &sha(&main))
        .unwrap();
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE run_leases SET heartbeat_at=0, pid=?1", [dead_pid()])
        .unwrap();
    (fixture, repo, db, run, backend)
}

fn verdict_json(decision: &str) -> String {
    verdict(decision, &[], decision)
}

/// The `auto_repaired` records of `repair`.
fn repairs<'a>(detail: &'a dagq::domain::TaskDetail, repair: &str) -> Vec<&'a Value> {
    payloads(detail, "auto_repaired")
        .into_iter()
        .filter(|p| p["repair"] == repair)
        .collect()
}

/// The dead landing of a passed run gives the slot back and lands again
/// without a review or a person, and the other task's landing follows.
#[test]
fn a_passed_run_whose_landing_died_is_released_and_lands_again() {
    let (_dir, repo, db, run, backend) = dead_landing(Some("pass"));
    let mut queue = SqliteQueue::open(&db).unwrap();
    // Only task 2 is reviewed.
    let reviewer = TestReviewer::new(&[verdict_json("pass")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(reviewer.prompts().len(), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1);
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let recovered = payloads(&detail, "run_recovered");
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0]["by"], "supervisor");
    assert_eq!(recovered[0]["previous_status"], "integrating");
    assert_eq!(recovered[0]["status"], "awaiting_integration");
    let released = repairs(&detail, "landing_released");
    assert_eq!(released.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(released[0]["conditions"]["lease"], "stale");
    assert_eq!(released[0]["conditions"]["review_passed"], true);
    assert_eq!(released[0]["detail"]["then"], "land");
    assert_eq!(
        payloads(&detail, "landing_queued"),
        [&json!({"via": "recover"})]
    );
    assert_eq!(payloads(&detail, "integration_started").len(), 2);
    assert!(payloads(&detail, "review_started").len() == 1);
    // Task 2 landed after it, its slot free again.
    let second = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(second.runs[0].status(), RunStatus::Integrated);
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(
        git_out(&repo, &["log", "--format=%s", "main"])
            .lines()
            .filter(|s| *s == "test task")
            .count(),
        1
    );
    let _ = run;
}

/// A run whose landing died before it was approved or passed is reviewed
/// again, as one just validated, and lands on the review's pass.
#[test]
fn a_run_neither_approved_nor_passed_is_reviewed_again_once_its_landing_died() {
    let (_dir, repo, db, _run, backend) = dead_landing(None);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let reviewer = TestReviewer::new(&[verdict_json("pass"), verdict_json("pass")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let released = repairs(&detail, "landing_released");
    assert_eq!(released.len(), 1);
    assert_eq!(released[0]["detail"]["then"], "review");
    assert!(
        payloads(&detail, "landing_queued")
            .iter()
            .all(|p| p["via"] != "recover")
    );
    let acquired = payloads(&detail, "lease_acquired");
    assert!(
        acquired.iter().any(|p| p["reason"] == "review"),
        "{acquired:?}"
    );
    // The concern's review, then the one after the release.
    assert_eq!(payloads(&detail, "review_started").len(), 2);
    assert_eq!(reviewer.prompts().len(), 2);
    assert_eq!(
        queue.show(TaskId::new(2)).unwrap().runs[0].status(),
        RunStatus::Integrated
    );
}

/// Nothing is released while a process still works in the run's worktree
/// (a verification command the dead landing left running): the run stays
/// integrating, and the next pass after it ends releases it.
#[test]
fn a_dead_landing_is_not_released_while_a_process_works_in_its_worktree() {
    let (_dir, repo, db, run, backend) = dead_landing(Some("pass"));
    let mut queue = SqliteQueue::open(&db).unwrap();
    // Not a child of this process, which is the supervisor's: the dead
    // landing's command outlives its parent.
    let verifying = Command::new("sh")
        .args(["-c", "sleep 120 >/dev/null 2>&1 & echo $!"])
        .current_dir(run.worktree_path().unwrap())
        .bounded_output()
        .unwrap();
    let verifying = String::from_utf8(verifying.stdout)
        .unwrap()
        .trim()
        .to_owned();
    // Task 2 cannot land either while task 1 holds the slot; its review
    // does not come.
    let reviewer = TestReviewer::new(&[verdict_json("pass"), verdict_json("pass")]);
    let stopper = {
        let db = db.clone();
        thread::spawn(move || {
            let mut queue = SqliteQueue::open(&db).unwrap();
            let deadline = Instant::now() + Duration::from_secs(60);
            // Once task 2 waits for the slot, nothing else can happen.
            loop {
                let runs = queue.show(TaskId::new(2)).unwrap().runs;
                if runs
                    .first()
                    .is_some_and(|r| r.status() == RunStatus::AwaitingIntegration)
                    && payloads(&queue.show(TaskId::new(2)).unwrap(), "review_finished").len() == 1
                {
                    break;
                }
                assert!(Instant::now() < deadline, "task 2 never waited to land");
                thread::sleep(TEST_TICK);
            }
            thread::sleep(Duration::from_millis(300));
            let still = queue.run(run.id()).unwrap().status();
            let killed = Command::new("kill").arg(&verifying).bounded_output();
            assert!(killed.unwrap().status.success());
            still
        })
    };
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(stopper.join().unwrap(), RunStatus::Integrating);
    for task in [1, 2] {
        let detail = queue.show(TaskId::new(task)).unwrap();
        assert_eq!(detail.runs[0].status(), RunStatus::Integrated, "{task}");
    }
    assert_eq!(
        repairs(&queue.show(TaskId::new(1)).unwrap(), "landing_released").len(),
        1
    );
}

/// A person's `recover` of a passed run whose landing died puts it back
/// awaiting integration; it waits to land without a person (not `review
/// and integrate`) and the supervisor lands it without reviewing it again.
#[test]
fn a_passed_run_a_person_recovered_from_its_landing_is_queued_to_land() {
    let (_dir, repo, db, run, backend) = dead_landing(Some("pass"));
    let recovered = runtime::recover(&db, run.id()).unwrap();
    assert_eq!(recovered["run"]["status"], "awaiting_integration");
    let status = runtime::status(&db).unwrap();
    let entry = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["task_id"] == 1)
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(entry["next"], "queued to land (runtime)", "{status}");
    let reviewer = TestReviewer::new(&[verdict_json("pass")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert!(repairs(&detail, "landing_released").is_empty());
    assert_eq!(reviewer.prompts().len(), 1);
    assert!(queue.asks(Default::default()).unwrap().is_empty());
}

/// A landing that moved main before its supervisor died is not landed a
/// second time: the run is integrated with the commit already on main.
#[test]
fn a_landing_that_reached_main_before_its_supervisor_died_is_not_landed_twice() {
    let (_dir, repo, db, run, backend) = dead_landing(Some("pass"));
    // What the dead landing did: the squash of the run on main, with its
    // trailers, before it recorded anything.
    let head = git_out(&repo, &["rev-parse", run.branch().unwrap()]);
    git(&repo, &["merge", "-q", "--squash", &head]);
    git(
        &repo,
        &[
            "commit",
            "-q",
            "-m",
            "test task",
            "-m",
            &format!("Dagq-Task: 1\nDagq-Run: {}", run.id()),
        ],
    );
    let landed = git_out(&repo, &["rev-parse", "main"]);
    let reviewer = TestReviewer::new(&[verdict_json("pass")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let integrated = payloads(&detail, "run_integrated");
    assert_eq!(integrated[0]["result_commit"], landed);
    let found = repairs(&detail, "landing_found_on_main");
    assert_eq!(found.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(found[0]["conditions"]["commit"], landed);
    let log = git_out(&repo, &["log", "--format=%s", "main"]);
    assert_eq!(
        log.lines().filter(|s| *s == "test task").count(),
        1,
        "{log}"
    );
    assert_eq!(
        queue.show(TaskId::new(2)).unwrap().runs[0].status(),
        RunStatus::Integrated
    );
}

/// A landing that died after it rebased the run onto a main that had
/// moved left the worktree on the rebased head, which the receipt does not
/// name: the landing again takes it as its own rebase of the receipt's
/// commit and lands it, instead of parking the run for a session.
#[test]
fn a_landing_that_died_after_its_rebase_lands_the_rebased_head() {
    let (_dir, repo, db, run, backend) = dead_landing(Some("pass"));
    fs::write(repo.join("other.txt"), "moved\n").unwrap();
    git(&repo, &["add", "other.txt"]);
    git(&repo, &["commit", "-q", "-m", "main moves"]);
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let before = git_out(&worktree, &["rev-parse", "HEAD"]);
    git(&worktree, &["rebase", "-q", "main"]);
    let after = git_out(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(before, after);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::IntegrationRebased,
            json!({"main": git_out(&repo, &["rev-parse", "main"]), "head_before": before, "head_after": after}),
        )
        .unwrap();
    let reviewer = TestReviewer::new(&[verdict_json("pass")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert!(payloads(&detail, "resume_started").is_empty());
    assert!(payloads(&detail, "integration_deferred").is_empty());
    assert!(
        git_out(&repo, &["ls-tree", "--name-only", "main"])
            .lines()
            .any(|name| name == "f1.txt")
    );
}

/// A supervisor that drains frees the slot too, so that nothing it waits
/// for waits on a dead landing; it lands nothing, and the run stays queued
/// for the next supervisor.
#[test]
fn a_draining_supervisor_releases_a_dead_landing_and_leaves_it_queued() {
    let (_dir, repo, db, run, backend) = dead_landing(Some("pass"));
    let options = supervise_options(4, true);
    options
        .stop
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let reviewer = TestReviewer::new(&[]);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(repairs(&detail, "landing_released").len(), 1);
    assert_eq!(payloads(&detail, "integration_started").len(), 1);
    assert!(queue.show(TaskId::new(2)).unwrap().runs.is_empty());
    let status = runtime::status(&db).unwrap();
    let entry = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["task_id"] == 1)
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(entry["next"], "queued to land (runtime)", "{status}");
}
