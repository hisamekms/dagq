//! Runtime tests: a worker's `already_satisfied` receipt (ADR-t1818-1) is
//! no `commit_mismatch`: the runtime lands nothing, runs the task's
//! verification commands on a snapshot of main recorded with its head,
//! asks a person on that snapshot, and closes the task without a commit on
//! the answer, verifying again on main's new head when main moved since.
use crate::common;
use crate::runtime_support;

use runtime_support::*;

/// The agent of a task whose base already meets it: no commit, and an
/// `already_satisfied` receipt naming the base with the check it ran there.
const SATISFIED_AGENT: &str = r#"printf '{"run_id":"%s","result":"already_satisfied","commit":"%s","tests":{"status":"passed","evidence_or_reason":"test -f seed.txt on the base"},"e2e":{"status":"not_applicable","evidence_or_reason":"the runtime runs it"},"subagent_review":{"status":"not_applicable","evidence_or_reason":"no change to review"},"summary":"the base meets it","base_checks":[{"command":"test -f seed.txt","result":"passed"}]}' "$RUN_ID" "$(git rev-parse HEAD)" > "$RECEIPT.tmp"; mv "$RECEIPT.tmp" "$RECEIPT""#;

fn supervise_reviewed(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    reviewer: &TestReviewer,
    parallel: usize,
) -> Value {
    let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
    let outcome = runtime::supervise_with_reviewer(
        db,
        repo,
        backend,
        &claude_stub(db),
        reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &supervise_options(parallel, true),
    )
    .unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    outcome
}

/// The single open ask of the queue, which must be an `approve_close`.
fn close_ask(queue: &mut SqliteQueue) -> dagq::domain::Ask {
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, dagq::domain::AskKind::ApproveClose);
    asks[0].clone()
}

fn main_head(repo: &Path) -> String {
    git_out(repo, &["rev-parse", "main"]).trim().to_owned()
}

/// The verification commands of task 1: the one its worker's base_checks
/// ran, and one they leave out, which only the runtime runs (a gate a
/// worker does not run itself, such as a coverage gate, stands there).
const VERIFY: &str = r#"["test -f seed.txt","test ! -f broken.txt"]"#;

/// Supervise task 1 of a fresh fixture, verified by [`VERIFY`], with
/// [`SATISFIED_AGENT`] until its claim waits in its ask; task 2 depends on
/// it.
fn claimed() -> (Fixture, PathBuf, PathBuf, TestWorkspace, TestReviewer) {
    claimed_after(|_| {})
}

/// [`claimed`], with `before` given the main checkout before the
/// supervisor starts.
fn claimed_after(
    before: impl FnOnce(&Path),
) -> (Fixture, PathBuf, PathBuf, TestWorkspace, TestReviewer) {
    let (fixture, repo, db) = fixture();
    before(&repo);
    // A ready task's verification is not editable through the CLI.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands=?1 WHERE id=1",
            [VERIFY],
        )
        .unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "after it", &[TaskId::new(1)]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.script_for(1, SATISFIED_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
    supervise_reviewed(&db, &repo, &backend, &reviewer, 2);
    (fixture, repo, db, backend, reviewer)
}

/// The claim validates without a `commit_mismatch`, rests `validating`
/// without a lease, its verification runs on main's head (recorded with
/// it, before and after), and the ask names that head; task 2 waits. The
/// `close` answer completes task 1 without a commit, and task 2 is
/// claimed and lands; `stats` counts the close apart from the landing.
#[test]
fn a_verified_claim_is_closed_without_a_commit_by_the_answer() {
    let (_fixture, repo, db, backend, reviewer) = claimed();
    let head = main_head(&repo);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Validating);
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 1);
    assert_eq!(validated[0]["already_satisfied"], true);
    assert_eq!(validated[0]["status"], "validating");
    assert_eq!(validated[0].get("code"), None);
    assert_eq!(
        payloads(&detail, "run_satisfied_snapshot"),
        [&json!({"head": head})]
    );
    let verified = payloads(&detail, "run_satisfied_verified");
    assert_eq!(
        verified,
        [&json!({"head": head, "passed": true, "commands": 2})]
    );
    let ask = close_ask(&mut queue);
    assert_eq!(ask.options, ["close", "retry", "cancel"]);
    assert!(ask.question.contains(&head), "{}", ask.question);
    assert!(queue.show(TaskId::new(2)).unwrap().runs.is_empty());
    // The ask is the attention, not the unleased validating run.
    let status = runtime::status(&db).unwrap();
    assert!(
        !status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["run_id"] == json!(run.id()) && a["ask_id"].is_null()),
        "{status}"
    );

    // Nor is it an orphan for a person's recover.
    let refused = runtime::recover(&db, run.id()).unwrap_err().to_string();
    assert!(refused.contains("approve_close"), "{refused}");

    queue.answer(ask.id, "close").unwrap();
    // The answer is the supervisor's to apply, not the inbox's to read.
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        ask_attention(&status, ask.id)[0]["next"],
        format!("applying the answer of ask {} (runtime)", ask.id),
        "{status}"
    );
    supervise_reviewed(&db, &repo, &backend, &reviewer, 2);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(detail.runs[0].status(), RunStatus::Succeeded);
    assert_eq!(detail.runs[0].result_commit(), None);
    let closed = payloads(&detail, "run_satisfied_closed");
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["head"], json!(head));
    assert_eq!(closed[0]["ask_id"], json!(ask.id));
    assert_eq!(closed[0]["rechecked"], false);
    // No second verification: main did not move.
    assert_eq!(payloads(&detail, "run_satisfied_verified").len(), 1);
    assert!(payloads(&detail, "run_integrated").is_empty());
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let after = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(after.runs[0].status(), RunStatus::Integrated);

    let stats = stats_full(&db);
    assert_eq!(
        stats["overall"]["already_satisfied"], 1,
        "{}",
        stats["overall"]
    );
    assert_eq!(stats["overall"]["runs"], 2);
}

/// Main moves after the ask, and the person answers while a supervisor
/// runs another task (which holds a slot): that running supervisor applies
/// the close, which does not close on the old snapshot but verifies again
/// on main's new head in the free slot, and closes on that.
#[test]
fn a_close_answered_after_main_moved_verifies_the_new_head_first() {
    let (fixture, repo, db, backend, _reviewer) = claimed();
    let first = main_head(&repo);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = close_ask(&mut queue);
    let marker = fixture.dir.path().join("go");
    let third = add_ready_task(&mut queue, "meanwhile", &[]);
    backend.script_for(
        third.as_i64(),
        // A file of its own, so that its landing and task 2's do not conflict.
        &format!(
            "await_file {}; printf 3 > f3.txt && git add f3.txt && git commit -q -m t3; receipt \"$(git rev-parse HEAD)\"",
            shell_path(&marker)
        ),
    );
    // Task 2 lands after the close, the third task once released.
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine"), verdict("pass", &[], "fine")]);
    let answerer = {
        let (db, repo) = (db.clone(), repo.clone());
        thread::spawn(move || {
            wait_until(&db, Duration::from_secs(20), |queue| {
                queue
                    .show(third)
                    .unwrap()
                    .runs
                    .first()
                    .is_some_and(|run| run.status() == RunStatus::Running)
            });
            git(
                &repo,
                &["commit", "-q", "--allow-empty", "-m", "main moves"],
            );
            let moved = main_head(&repo);
            SqliteQueue::open(&db)
                .unwrap()
                .answer(ask.id, "close")
                .unwrap();
            wait_until(&db, Duration::from_secs(20), |queue| {
                queue.read_ask(ask.id).unwrap().closed_at.is_some()
            });
            fs::write(&marker, "").unwrap();
            moved
        })
    };
    supervise_reviewed(&db, &repo, &backend, &reviewer, 2);
    let moved = joined(answerer, "the answerer");
    assert_ne!(moved, first);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let snapshots = payloads(&detail, "run_satisfied_snapshot");
    assert_eq!(
        snapshots,
        [
            &json!({"head": first}),
            &json!({"head": moved, "ask_id": ask.id})
        ]
    );
    let verified = payloads(&detail, "run_satisfied_verified");
    assert_eq!(verified.len(), 2);
    assert_eq!(verified[1]["head"], json!(moved));
    assert_eq!(verified[1]["passed"], true);
    let closed = payloads(&detail, "run_satisfied_closed");
    assert_eq!(closed[0]["head"], json!(moved));
    assert_eq!(closed[0]["rechecked"], true);
}

/// No supervisor runs after the ask: main moves and the person answers
/// close; a supervisor started afterwards reads the snapshot from the
/// events, finds main moved and verifies again before it closes. When that
/// verification fails on the command the worker's base_checks left out
/// (main gained what the task forbids), the task
/// is not closed: the answered ask closes and a new one says the
/// verification failed on the new head, offering only retry and cancel.
/// `retry` fails the run and puts the task back, so it is claimed again and
/// its new claim fails on main as it is; `cancel` then cancels the task.
#[test]
fn after_a_restart_a_close_verifies_main_again_and_a_failure_does_not_close() {
    let (_fixture, repo, db, backend, reviewer) = claimed();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = close_ask(&mut queue);
    fs::write(repo.join("broken.txt"), "broken\n").unwrap();
    git(&repo, &["add", "broken.txt"]);
    git(&repo, &["commit", "-q", "-m", "main breaks the task"]);
    let moved = main_head(&repo);
    queue.answer(ask.id, "close").unwrap();
    supervise_reviewed(&db, &repo, &backend, &reviewer, 2);

    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Validating);
    assert!(payloads(&detail, "run_satisfied_closed").is_empty());
    let verified = payloads(&detail, "run_satisfied_verified");
    assert_eq!(verified.len(), 2);
    assert_eq!(verified[1]["head"], json!(moved));
    assert_eq!(verified[1]["passed"], false);
    assert_eq!(verified[1]["ask_id"], json!(ask.id));
    assert_eq!(verified[1]["command"], "test ! -f broken.txt");
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let failed = close_ask(&mut queue);
    assert_eq!(failed.options, ["retry", "cancel"]);
    assert!(
        failed.question.contains(&moved) && failed.question.contains("not closed"),
        "{}",
        failed.question
    );
    assert!(queue.show(TaskId::new(2)).unwrap().runs.is_empty());

    queue.answer(failed.id, "retry").unwrap();
    supervise_reviewed(&db, &repo, &backend, &reviewer, 2);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2);
    let retried = detail.runs.iter().find(|r| r.id() == run.id()).unwrap();
    assert_eq!(retried.status(), RunStatus::Failed);
    assert_eq!(
        retried.last_error(),
        Some(format!("put back to ready by ask {}", failed.id).as_str())
    );
    assert!(queue.read_ask(failed.id).unwrap().closed_at.is_some());
    let again = close_ask(&mut queue);
    assert_eq!(again.options, ["retry", "cancel"]);
    assert_ne!(again.run_id.as_ref(), Some(run.id()));

    queue.answer(again.id, "cancel").unwrap();
    supervise_reviewed(&db, &repo, &backend, &reviewer, 2);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    assert!(detail.runs.iter().all(|r| r.status() == RunStatus::Failed));
    let decided: Vec<_> = payloads(&detail, "run_satisfied_decided")
        .into_iter()
        .map(|p| (p["answer"].clone(), p["code"].clone()))
        .collect();
    assert_eq!(
        decided,
        [
            (json!("retry"), json!("other")),
            (json!("cancel"), json!("cancelled"))
        ]
    );
    assert!(queue.read_ask(again.id).unwrap().closed_at.is_some());
}

/// A claim without its checks on the base is refused as `receipt_invalid`:
/// the run fails, and nothing is verified or asked.
#[test]
fn a_claim_without_its_base_checks_is_an_invalid_receipt() {
    let unbacked = SATISFIED_AGENT.replace(
        r#","base_checks":[{"command":"test -f seed.txt","result":"passed"}]"#,
        "",
    );
    let (_fixture, _repo, detail) = run_agent(&unbacked);
    let reason = rejection_reason(&detail);
    assert!(reason.contains("base_checks"), "{reason}");
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated[0]["code"], "receipt_invalid");
    assert!(payloads(&detail, "run_satisfied_snapshot").is_empty());
}

/// Commit `text` as the main checkout's `dagq.toml`.
fn set_runtimes(repo: &Path, text: &str) {
    fs::write(repo.join("dagq.toml"), text).unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-q", "-m", "runtimes"]);
}

const CONTAINER: &str = "[roles.verification]\nruntime = 'container'\n";
const HOST: &str = "[roles.verification]\nruntime = 'host'\n";

/// The verification set to a container that cannot be used: the claim
/// rests accepted, nothing is verified on the host (no snapshot, no log),
/// it is no orphan for `recover`, and the queue's attention says why. Back
/// on the host, the next supervisor verifies it and asks. An answer to
/// close given while the container is set again waits too (no verification
/// starts), and is applied once the host is back.
#[test]
fn a_verification_set_to_a_held_container_runs_nothing_on_the_host() {
    let (_fixture, repo, db, backend, reviewer) =
        claimed_after(|repo| set_runtimes(repo, CONTAINER));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Validating);
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert!(payloads(&detail, "run_satisfied_snapshot").is_empty());
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    let logs: Vec<_> = fs::read_dir(run.run_dir().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("satisfied"))
        .collect();
    assert!(logs.is_empty(), "{logs:?}");
    let refused = runtime::recover(&db, run.id()).unwrap_err().to_string();
    assert!(refused.contains("already_satisfied"), "{refused}");
    let status = runtime::status(&db).unwrap();
    let attention = status["attention"].as_array().unwrap();
    assert!(
        attention
            .iter()
            .any(|a| a["kind"] == "container_runtime_unavailable"),
        "{status}"
    );
    assert!(
        !attention.iter().any(|a| a["run_id"] == json!(run.id())),
        "{status}"
    );

    set_runtimes(&repo, HOST);
    supervise_reviewed(&db, &repo, &backend, &reviewer, 2);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "run_satisfied_snapshot").len(), 1);
    let ask = close_ask(&mut queue);

    set_runtimes(&repo, CONTAINER);
    queue.answer(ask.id, "close").unwrap();
    supervise_reviewed(&db, &repo, &backend, &reviewer, 2);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "run_satisfied_snapshot").len(), 1);
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_none());

    set_runtimes(&repo, HOST);
    supervise_reviewed(&db, &repo, &backend, &reviewer, 2);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(
        payloads(&detail, "run_satisfied_closed")[0]["rechecked"],
        true
    );
}

/// A process an earlier verification left working in the run's scratch
/// worktree (its supervisor died while it ran) is stopped before the
/// worktree is checked out again, recorded as `auto_repaired`; the
/// verification then runs on the new snapshot and the task closes. A
/// process elsewhere is left alone.
#[test]
fn a_verification_a_dead_supervisor_left_running_is_stopped_before_the_next() {
    use dagq::application::ProcessControl;
    use dagq::infrastructure::adapters::SystemProcesses;
    let (_fixture, repo, db, backend, reviewer) = claimed();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = close_ask(&mut queue);
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let scratch = Path::new(run.run_dir().unwrap()).join("satisfied");
    fs::create_dir_all(&scratch).unwrap();
    // A stand-in the test orphans in `dir`: it ends on its own once its
    // directory or the test's process is gone, or after two minutes,
    // whatever ends the test. Its `sleep`s work in `dir` too, so the stop
    // may name them beside it.
    let orphan = |dir: &Path| -> u32 {
        let script = format!(
            "i=0; while [ $i -lt 120 ] && [ -d \"$PWD\" ] && kill -0 {} 2>/dev/null; \
             do sleep 1; i=$((i + 1)); done >/dev/null 2>&1 & echo $!",
            std::process::id()
        );
        let out = std::process::Command::new("sh")
            .args(["-c", &script])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    let left = orphan(&scratch);
    let outside = orphan(&repo);
    let stop = |pid: u32| {
        if SystemProcesses.alive(pid) {
            let _ = SystemProcesses.kill(pid);
        }
    };
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "main moves"],
    );
    queue.answer(ask.id, "close").unwrap();
    supervise_reviewed(&db, &repo, &backend, &reviewer, 2);
    let alive = (SystemProcesses.alive(left), SystemProcesses.alive(outside));
    stop(left);
    stop(outside);
    assert_eq!(alive, (false, true));
    let detail = queue.show(TaskId::new(1)).unwrap();
    let stopped: Vec<_> = payloads(&detail, "auto_repaired")
        .into_iter()
        .filter(|p| p["repair"] == "satisfied_verification_stopped")
        .collect();
    assert_eq!(stopped.len(), 1, "{:?}", event_kinds(&detail));
    let pids = stopped[0]["conditions"]["pids"].as_array().unwrap();
    assert!(
        pids.contains(&json!(left)) && !pids.contains(&json!(outside)),
        "{pids:?}"
    );
    assert_eq!(detail.task.status(), TaskStatus::Completed);
}
