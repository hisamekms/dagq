//! Runtime tests: a run left `integrating` by a supervisor that died gives
//! the integration slot back on the next supervisor's pass, lands again
//! without a person when it was approved or passed, is reviewed again when
//! not, lands once a person's `recover` put it back, and is never landed
//! twice when its landing already moved main (task 1118). Processes the
//! dead landing left in its worktree are waited for, stopped by pid past
//! their grace, and the inbox's once when they cannot be (task 1129).
use crate::runtime_support;
use dagq::application::ProcessControl;
use dagq::domain::landing_release::{GRACE_SECS, UNLISTED_SECS};
use dagq::domain::{EventKind, LeaseToken};
use dagq::infrastructure::adapters::SystemProcesses;
use std::sync::atomic::Ordering;
use std::time::{Instant, UNIX_EPOCH};

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
    headless_workers();
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
    headless_workers();
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
    headless_workers();
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
    let pid: u32 = verifying.parse().unwrap();
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
            // A fixed pause, not supervisor passes: while the landing's
            // process lives, each pass looks for processes in the worktree
            // and takes 0.35-0.6 s, so five passes took 1.7-2.9 s against
            // this 0.3 s (task 1075).
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
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(repairs(&detail, "landing_released").len(), 1);
    // Within its grace the process is only waited for (task 1129): the
    // wait is recorded once, from the first pass that saw it, and nothing
    // was stopped (the test's own kill above succeeded).
    let waiting = payloads(&detail, "landing_release_waiting");
    assert_eq!(waiting.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(waiting[0]["pids"], json!([pid]));
    assert!(repairs(&detail, "landing_processes_stopped").is_empty());
    assert!(payloads(&detail, "landing_release_stuck").is_empty());
}

/// A process started in `dir` that outlives its parent (as a dead
/// landing's verification command does): not a child of the test process,
/// which is the supervisor's.
fn orphan_in(dir: &Path) -> u32 {
    let out = Command::new("sh")
        .args(["-c", "sleep 120 >/dev/null 2>&1 & echo $!"])
        .current_dir(dir)
        .bounded_output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// What a previous supervisor recorded when it first found `pids` in the
/// worktree of `run`'s dead landing, `ago` seconds back.
fn waited_since(db: &Path, run: &TaskRun, pids: &[u32], ago: i64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    SqliteQueue::open(db)
        .unwrap()
        .record_runtime_event(
            run.id(),
            EventKind::LandingReleaseWaiting,
            json!({"pids": pids, "seen_at": now - ago}),
        )
        .unwrap();
}

/// Stops a pid the test started, if it still runs.
struct Reaped(u32);

impl Drop for Reaped {
    fn drop(&mut self) {
        if SystemProcesses.alive(self.0) {
            let _ = SystemProcesses.kill(self.0);
        }
    }
}

/// A process the dead landing left in its worktree past its grace, counted
/// from what an earlier supervisor saw (the supervisor that replaced it
/// does not count again), is stopped by pid on the next pass: the run is
/// released and lands, and the other task's landing follows. A process
/// outside the worktree is left alone.
#[test]
fn a_process_past_its_grace_is_stopped_and_the_dead_landing_released() {
    headless_workers();
    let (_dir, repo, db, run, backend) = dead_landing(Some("pass"));
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let hung = orphan_in(&worktree);
    let _hung = Reaped(hung);
    let outside = orphan_in(&repo);
    let _outside = Reaped(outside);
    waited_since(&db, &run, &[hung], GRACE_SECS + 5);
    let reviewer = TestReviewer::new(&[verdict_json("pass")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(!SystemProcesses.alive(hung));
    assert!(SystemProcesses.alive(outside));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let stopped = repairs(&detail, "landing_processes_stopped");
    assert_eq!(stopped.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(stopped[0]["layer"], "runtime");
    assert_eq!(stopped[0]["conditions"]["pids"], json!([hung]));
    assert!(stopped[0]["conditions"]["waited_secs"].as_i64().unwrap() >= GRACE_SECS);
    assert_eq!(stopped[0]["detail"]["stopped"][0]["pid"], hung);
    let released = repairs(&detail, "landing_released");
    assert_eq!(released.len(), 1);
    assert_eq!(released[0]["conditions"]["stopped_processes"], 1);
    assert_eq!(released[0]["detail"]["then"], "land");
    // The wait of the earlier supervisor is the only one.
    assert_eq!(payloads(&detail, "landing_release_waiting").len(), 1);
    assert!(payloads(&detail, "landing_release_stuck").is_empty());
    assert_eq!(
        queue.show(TaskId::new(2)).unwrap().runs[0].status(),
        RunStatus::Integrated
    );
}

/// The host's processes, whose signals reach none of them: a process that
/// outlives its SIGKILL.
struct Unstoppable;

impl ProcessControl for Unstoppable {
    fn alive(&self, pid: u32) -> bool {
        SystemProcesses.alive(pid)
    }
    fn terminate(&self, _: u32) -> Result<()> {
        Ok(())
    }
    fn interrupt(&self, _: u32) -> Result<()> {
        Ok(())
    }
    fn kill(&self, _: u32) -> Result<()> {
        Ok(())
    }
    fn list(&self) -> Result<Vec<dagq::domain::recovery::ProcessInfo>> {
        SystemProcesses.list()
    }
}

/// A process that outlives its stop leaves the run integrating and is the
/// inbox's attention, once however many passes go by; once a person
/// stopped it, the next pass releases the run and both tasks land.
#[test]
fn a_process_that_outlives_its_stop_is_the_inboxs_once() {
    headless_workers();
    let (_dir, repo, db, run, backend) = dead_landing(Some("pass"));
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let hung = orphan_in(&worktree);
    let _hung = Reaped(hung);
    waited_since(&db, &run, &[hung], GRACE_SECS + 5);
    let options = SuperviseOptions {
        processes: Some(runtime::ProcessesPort(Arc::new(Unstoppable))),
        ..supervise_options(4, true)
    };
    let passes = options.passes.clone();
    let person = {
        let (db, run) = (db.clone(), run.clone());
        thread::spawn(move || {
            let mut queue = SqliteQueue::open(&db).unwrap();
            let deadline = Instant::now() + Duration::from_secs(60);
            let seen = loop {
                let detail = queue.show(TaskId::new(1)).unwrap();
                if !payloads(&detail, "landing_release_stuck").is_empty() {
                    break passes.load(Ordering::SeqCst);
                }
                assert!(Instant::now() < deadline, "the stop was never given up");
                thread::sleep(TEST_TICK);
            };
            // Passes go by without another attention.
            while passes.load(Ordering::SeqCst) < seen + 3 {
                assert!(Instant::now() < deadline, "no pass went by");
                thread::sleep(TEST_TICK);
            }
            let status = runtime::status(&db).unwrap();
            let still = queue.run(run.id()).unwrap().status();
            let killed = SystemProcesses.kill(hung);
            (status, still, killed.is_ok())
        })
    };
    let reviewer = TestReviewer::new(&[verdict_json("pass")]);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let (status, still, killed) = person.join().unwrap();
    assert!(killed);
    assert_eq!(still, RunStatus::Integrating);
    let entry = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["task_id"] == 1)
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(entry["kind"], "landing_release_stuck", "{status}");
    assert_eq!(
        entry["next"], "stop the dead landing's processes",
        "{status}"
    );
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let stuck = payloads(&detail, "landing_release_stuck");
    assert_eq!(stuck.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(stuck[0]["cause"], "survived_stop");
    assert_eq!(stuck[0]["pids"], json!([hung]));
    assert!(repairs(&detail, "landing_processes_stopped").is_empty());
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert_eq!(repairs(&detail, "landing_released").len(), 1);
    assert_eq!(
        queue.show(TaskId::new(2)).unwrap().runs[0].status(),
        RunStatus::Integrated
    );
}

/// A person's `recover` of a passed run whose landing died puts it back
/// awaiting integration; it waits to land without a person (not `review
/// and integrate`) and the supervisor lands it without reviewing it again.
#[test]
fn a_passed_run_a_person_recovered_from_its_landing_is_queued_to_land() {
    headless_workers();
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
    headless_workers();
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
    headless_workers();
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
    headless_workers();
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

/// The host's processes, which cannot be listed while `failing` holds.
struct Unlisted {
    failing: Arc<std::sync::atomic::AtomicBool>,
}

impl ProcessControl for Unlisted {
    fn alive(&self, pid: u32) -> bool {
        SystemProcesses.alive(pid)
    }
    fn terminate(&self, pid: u32) -> Result<()> {
        SystemProcesses.terminate(pid)
    }
    fn interrupt(&self, pid: u32) -> Result<()> {
        SystemProcesses.interrupt(pid)
    }
    fn kill(&self, pid: u32) -> Result<()> {
        SystemProcesses.kill(pid)
    }
    fn list(&self) -> Result<Vec<dagq::domain::recovery::ProcessInfo>> {
        if self.failing.load(Ordering::SeqCst) {
            bail!("ps failed");
        }
        SystemProcesses.list()
    }
}

/// Processes that cannot be listed for twice the grace since the first
/// pass that could not list them leave the run integrating and are the
/// inbox's attention, once however many passes go by; once they can be
/// listed again (and none is left), the run is released and both tasks
/// land.
#[test]
fn processes_unlisted_past_twice_the_grace_are_the_inboxs_once() {
    headless_workers();
    let (_dir, repo, db, run, backend) = dead_landing(Some("pass"));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    // What an earlier supervisor recorded when it first could not list.
    SqliteQueue::open(&db)
        .unwrap()
        .record_runtime_event(
            run.id(),
            EventKind::LandingReleaseWaiting,
            json!({"pids": null, "seen_at": now - UNLISTED_SECS - 5, "error": "ps failed"}),
        )
        .unwrap();
    let failing = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let options = SuperviseOptions {
        processes: Some(runtime::ProcessesPort(Arc::new(Unlisted {
            failing: failing.clone(),
        }))),
        ..supervise_options(4, true)
    };
    let passes = options.passes.clone();
    let person = {
        let (db, run) = (db.clone(), run.clone());
        thread::spawn(move || {
            let mut queue = SqliteQueue::open(&db).unwrap();
            let deadline = Instant::now() + Duration::from_secs(60);
            let seen = loop {
                let detail = queue.show(TaskId::new(1)).unwrap();
                if !payloads(&detail, "landing_release_stuck").is_empty() {
                    break passes.load(Ordering::SeqCst);
                }
                assert!(
                    Instant::now() < deadline,
                    "the unlisted wait was never given up"
                );
                thread::sleep(TEST_TICK);
            };
            while passes.load(Ordering::SeqCst) < seen + 3 {
                assert!(Instant::now() < deadline, "no pass went by");
                thread::sleep(TEST_TICK);
            }
            let status = runtime::status(&db).unwrap();
            let still = queue.run(run.id()).unwrap().status();
            let stuck = payloads(
                &queue.show(TaskId::new(1)).unwrap(),
                "landing_release_stuck",
            )
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
            failing.store(false, Ordering::SeqCst);
            (status, still, stuck)
        })
    };
    let reviewer = TestReviewer::new(&[verdict_json("pass")]);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let (status, still, stuck) = person.join().unwrap();
    assert_eq!(still, RunStatus::Integrating);
    assert_eq!(stuck.len(), 1, "{stuck:?}");
    assert_eq!(stuck[0]["cause"], "unlisted");
    assert!(stuck[0]["waited_secs"].as_i64().unwrap() >= UNLISTED_SECS);
    let entry = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["task_id"] == 1)
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(entry["kind"], "landing_release_stuck", "{status}");
    assert_eq!(
        entry["next"], "stop the dead landing's processes",
        "{status}"
    );
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "landing_release_stuck").len(), 1);
    // The earlier supervisor's wait is the only one.
    assert_eq!(payloads(&detail, "landing_release_waiting").len(), 1);
    assert!(repairs(&detail, "landing_processes_stopped").is_empty());
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert_eq!(repairs(&detail, "landing_released").len(), 1);
    assert_eq!(
        queue.show(TaskId::new(2)).unwrap().runs[0].status(),
        RunStatus::Integrated
    );
}

/// An `integrating` run whose landing's supervisor lives (its pid alive,
/// its heartbeat fresh) is not the release's: a process in its worktree,
/// even one seen past the grace, is neither stopped nor waited for, and
/// the run is not released. Once that supervisor is gone (and its process
/// with it), the run is released and lands.
#[test]
fn a_live_supervisors_landing_and_its_processes_are_left_alone() {
    headless_workers();
    let (_dir, repo, db, run, backend) = dead_landing(Some("pass"));
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let hung = orphan_in(&worktree);
    let _hung = Reaped(hung);
    waited_since(&db, &run, &[hung], GRACE_SECS + 5);
    // The landing's supervisor: a live process of the test's.
    let mut landing = Command::new("sleep").arg("120").spawn().unwrap();
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET heartbeat_at=unixepoch(), pid=?1",
            [landing.id()],
        )
        .unwrap();
    let options = supervise_options(4, true);
    let passes = options.passes.clone();
    let person = {
        let (db, run) = (db.clone(), run.clone());
        thread::spawn(move || {
            let mut queue = SqliteQueue::open(&db).unwrap();
            let deadline = Instant::now() + Duration::from_secs(60);
            let start = passes.load(Ordering::SeqCst);
            while passes.load(Ordering::SeqCst) < start + 5 {
                assert!(Instant::now() < deadline, "no pass went by");
                Connection::open(&db)
                    .unwrap()
                    .execute("UPDATE run_leases SET heartbeat_at=unixepoch()", [])
                    .unwrap();
                thread::sleep(TEST_TICK);
            }
            let still = queue.run(run.id()).unwrap().status();
            let alive = SystemProcesses.alive(hung);
            let detail = queue.show(TaskId::new(1)).unwrap();
            let records = [
                payloads(&detail, "landing_release_waiting").len(),
                repairs(&detail, "landing_processes_stopped").len(),
                repairs(&detail, "landing_released").len(),
                payloads(&detail, "landing_release_stuck").len(),
            ];
            // The landing's supervisor dies, and its process with it.
            SystemProcesses.kill(hung).unwrap();
            landing.kill().unwrap();
            landing.wait().unwrap();
            Connection::open(&db)
                .unwrap()
                .execute("UPDATE run_leases SET heartbeat_at=0", [])
                .unwrap();
            (still, alive, records)
        })
    };
    let reviewer = TestReviewer::new(&[verdict_json("pass")]);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let (still, alive, records) = person.join().unwrap();
    assert_eq!(still, RunStatus::Integrating);
    assert!(alive, "the live landing's process was stopped");
    // Only the wait the test recorded; nothing stopped, released or stuck.
    assert_eq!(records, [1, 0, 0, 0]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert!(repairs(&detail, "landing_processes_stopped").is_empty());
    assert_eq!(repairs(&detail, "landing_released").len(), 1);
}
