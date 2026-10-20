//! Runtime tests: a worker's `already_satisfied` receipt (ADR-t1818-1) is
//! no `commit_mismatch`: the runtime lands nothing, runs the task's
//! verification commands on a snapshot of main recorded with its head,
//! has the review judge the claim on that same head, closes the task
//! without a commit on a `high` pass while main is still at that head or
//! else asks a person with the judgment, and closes it on the answer,
//! verifying (and judging) again on main's new head when main moved since.
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
    claimed_with(true, before)
}

/// [`claimed_after`], with task 2 depending on task 1 only when
/// `dependent`.
fn claimed_with(
    dependent: bool,
    before: impl FnOnce(&Path),
) -> (Fixture, PathBuf, PathBuf, TestWorkspace, TestReviewer) {
    claimed_judged(dependent, before, &[verdict("pass", &[], "fine")])
}

/// [`claimed_with`], the review's jobs (the claim's judgment first) run
/// by `scripts` in order, the last one repeated.
fn claimed_judged(
    dependent: bool,
    before: impl FnOnce(&Path),
    scripts: &[String],
) -> (Fixture, PathBuf, PathBuf, TestWorkspace, TestReviewer) {
    claimed_judged_by(dependent, |repo| {
        before(repo);
        scripts.to_vec()
    })
}

/// [`claimed_judged`], the scripts made by `scripts` from the main
/// checkout, given it before the supervisor starts.
fn claimed_judged_by(
    dependent: bool,
    scripts: impl FnOnce(&Path) -> Vec<String>,
) -> (Fixture, PathBuf, PathBuf, TestWorkspace, TestReviewer) {
    let (fixture, repo, db) = fixture();
    let scripts = scripts(&repo);
    // A ready task's verification is not editable through the CLI.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands=?1 WHERE id=1",
            [VERIFY],
        )
        .unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    if dependent {
        add_ready_task(&mut queue, "after it", &[TaskId::new(1)]);
    }
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.script_for(1, SATISFIED_AGENT);
    let reviewer = TestReviewer::new(&scripts);
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
    // Only the first verification, which passed, was judged by the review.
    assert_eq!(payloads(&detail, "run_satisfied_review_started").len(), 1);
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
    // A claim whose first verification failed is not judged.
    let judged: Vec<_> = detail
        .events
        .iter()
        .filter(|e| e.kind == "run_satisfied_review_started")
        .map(|e| e.run_id.clone())
        .collect();
    assert_eq!(judged, [Some(run.id().clone())]);

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
    let canceled = payloads(&detail, "task_status_changed").pop().unwrap();
    assert_eq!(
        (&canceled["reason"], &canceled["by"], &canceled["ask_id"]),
        (
            &json!("answered_cancel"),
            &json!("approve_close"),
            &json!(again.id)
        )
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
    // Nor was a scratch worktree made on the host.
    let scratch = satisfied_scratch(&db, run.id());
    assert!(fs::symlink_metadata(&scratch).is_err(), "{scratch:?}");
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
    let scratch = satisfied_scratch(&db, run.id());
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
    assert_eq!(satisfied_worktrees(&repo, &db), Vec::<String>::new());
}

/// Where the verification of `run`'s claim checks its snapshot out: the
/// run's directory of the queue's `satisfied/`.
fn satisfied_scratch(db: &Path, run: &RunId) -> PathBuf {
    dagq::application::integrate::satisfied_scratch(
        &dagq::infrastructure::location::satisfied_dir(db),
        run,
    )
}

/// The worktrees Git records of the repository under the queue's
/// `satisfied/`.
fn satisfied_worktrees(repo: &Path, db: &Path) -> Vec<String> {
    let dir = dagq::infrastructure::location::satisfied_dir(db);
    let dir = fs::canonicalize(dir.parent().unwrap())
        .unwrap()
        .join(dir.file_name().unwrap());
    git_out(repo, &["worktree", "list", "--porcelain"])
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .filter(|path| {
            fs::canonicalize(path)
                .unwrap_or_else(|_| PathBuf::from(path))
                .starts_with(&dir)
                || Path::new(path).starts_with(&dir)
        })
        .map(str::to_owned)
        .collect()
}

/// Another worktree of the repository, a person's or another run's, with
/// work of its own not committed: a tracked file changed and a file Git
/// does not track. What the verification must leave as it is.
struct Bystander {
    dir: PathBuf,
    git_dir: String,
    head: String,
    branch: String,
    status: String,
}

impl Bystander {
    fn new(repo: &Path, dir: PathBuf) -> Self {
        git(
            repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "bystander",
                dir.to_str().unwrap(),
                "main",
            ],
        );
        fs::write(dir.join("seed.txt"), "changed, not committed").unwrap();
        fs::write(dir.join("untracked.txt"), "not tracked").unwrap();
        let read = |args: &[&str]| git_out(&dir, args).trim().to_owned();
        Self {
            git_dir: read(&["rev-parse", "--absolute-git-dir"]),
            head: read(&["rev-parse", "HEAD"]),
            branch: read(&["symbolic-ref", "HEAD"]),
            status: read(&["status", "--porcelain", "--untracked-files=all"]),
            dir,
        }
    }

    /// Its HEAD, its branch and its work are as they were.
    fn assert_untouched(&self) {
        let read = |args: &[&str]| git_out(&self.dir, args).trim().to_owned();
        assert_eq!(read(&["rev-parse", "HEAD"]), self.head);
        assert_eq!(read(&["symbolic-ref", "HEAD"]), self.branch);
        assert_eq!(
            read(&["status", "--porcelain", "--untracked-files=all"]),
            self.status
        );
        assert_eq!(
            fs::read_to_string(self.dir.join("seed.txt")).unwrap(),
            "changed, not committed"
        );
        assert_eq!(
            fs::read_to_string(self.dir.join("untracked.txt")).unwrap(),
            "not tracked"
        );
    }
}

/// Plant with `plant` what points at a [`Bystander`] where the run's
/// scratch worktree is (the queue's `satisfied/`) and where it once was
/// (the run directory's `satisfied`, which its worker can write), then
/// move main and answer close: the verification on the new head makes its
/// scratch worktree again from nothing and the task closes, the bystander's
/// HEAD and work unchanged, what was planted in the run directory left
/// alone, and no scratch worktree left behind or recorded by Git. With
/// `cleaned`, once the run ended, the cleanup removes a scratch worktree a
/// dead verification left, with Git's record of it. No task depends on
/// the claim's.
fn a_planted_scratch_is_not_trusted(cleaned: bool, plant: impl FnOnce(&Path, &Path, &Bystander)) {
    let (fixture, repo, db, backend, reviewer) = claimed_with(false, |_| {});
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = close_ask(&mut queue);
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let bystander = Bystander::new(&repo, fixture.dir.path().join("bystander"));
    let scratch = satisfied_scratch(&db, run.id());
    let old = Path::new(run.run_dir().unwrap()).join("satisfied");
    plant(&scratch, &old, &bystander);
    let planted = fs::symlink_metadata(&old).unwrap().file_type();
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "main moves"],
    );
    queue.answer(ask.id, "close").unwrap();
    supervise_reviewed(&db, &repo, &backend, &reviewer, 2);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let verified = payloads(&detail, "run_satisfied_verified");
    assert_eq!(verified.len(), 2, "{:?}", event_kinds(&detail));
    assert_eq!(verified[1]["passed"], true, "{verified:?}");
    bystander.assert_untouched();
    assert_eq!(
        fs::symlink_metadata(&old).unwrap().file_type(),
        planted,
        "the run directory's satisfied is left alone"
    );
    assert!(fs::symlink_metadata(&scratch).is_err(), "{scratch:?}");
    assert_eq!(satisfied_worktrees(&repo, &db), Vec::<String>::new());
    if !cleaned {
        return;
    }

    // A scratch worktree a dead verification left, recorded by Git.
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            scratch.to_str().unwrap(),
            "main",
        ],
    );
    assert_eq!(satisfied_worktrees(&repo, &db).len(), 1);
    let sweeping = SuperviseOptions {
        sweep_interval: Duration::ZERO,
        ..supervise_options(2, true)
    };
    supervise_with(&db, &repo, &backend, &sweeping).unwrap();
    assert!(fs::symlink_metadata(&scratch).is_err(), "{scratch:?}");
    assert_eq!(satisfied_worktrees(&repo, &db), Vec::<String>::new());
    bystander.assert_untouched();
}

/// A link to another worktree where the scratch worktree is, and a `.git`
/// file naming that worktree's Git directory where it once was in the run
/// directory: neither is followed nor used, and the cleanup of the ended
/// run removes a scratch worktree left there.
#[test]
fn a_link_where_the_scratch_is_is_removed_not_followed() {
    a_planted_scratch_is_not_trusted(true, |scratch, old, bystander| {
        fs::create_dir_all(scratch.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&bystander.dir, scratch).unwrap();
        fs::create_dir_all(old).unwrap();
        fs::write(old.join(".git"), format!("gitdir: {}\n", bystander.git_dir)).unwrap();
    });
}

/// A `.git` file naming another worktree's Git directory where the scratch
/// worktree is, and a link to that worktree where it once was in the run
/// directory: the `.git` is not trusted as the scratch worktree's.
#[test]
fn a_git_file_where_the_scratch_is_is_not_trusted() {
    a_planted_scratch_is_not_trusted(false, |scratch, old, bystander| {
        fs::create_dir_all(scratch).unwrap();
        fs::write(
            scratch.join(".git"),
            format!("gitdir: {}\n", bystander.git_dir),
        )
        .unwrap();
        std::os::unix::fs::symlink(&bystander.dir, old).unwrap();
    });
}

/// Open a replanning operation on `run` and move it past the pause's fence,
/// as the trusted runtime's diagnosis would (ADR-t1521-2).
fn pause_replan(db: &Path, run: &RunId) -> dagq::domain::task_replan::ReplanOperation {
    use dagq::domain::task_replan::{NewReplan, ReplanMove, ReplanOrigin, ReplanStatus};
    let mut queue = SqliteQueue::open(db).unwrap();
    let op = queue
        .record_replan(
            &NewReplan {
                run: run.clone(),
                reason: "two independent parts".into(),
                evidence: Vec::new(),
            },
            ReplanOrigin::Runtime,
            "supervisor",
            "supervisor",
        )
        .unwrap()
        .operation;
    queue
        .move_replan(
            op.id,
            op.generation,
            &ReplanMove {
                to: ReplanStatus::PauseRequested,
                reason: "the test pauses it".into(),
                snapshot_id: None,
                diagnosis: None,
            },
        )
        .unwrap()
}

/// What a replanning's park leaves of task 1's claim: the operation
/// parked, the run waiting for a session, no close, the task in progress
/// and task 2 (which waits for it) not claimed.
fn assert_parked_unclosed(db: &Path, op: &dagq::domain::task_replan::ReplanOperation) {
    let mut queue = SqliteQueue::open(db).unwrap();
    assert_eq!(
        queue.replan_operation(op.id).unwrap().status,
        dagq::domain::task_replan::ReplanStatus::Parked
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    assert_eq!(payloads(&detail, "replan_parked").len(), 1);
    assert!(payloads(&detail, "run_satisfied_closed").is_empty());
    assert!(queue.run_lease(detail.runs[0].id()).unwrap().is_none());
    assert!(queue.show(TaskId::new(2)).unwrap().runs.is_empty());
}

/// Acceptance (ADR-t1521-2, the park): an operation opened while the
/// claim's verification runs lets the verification end, but its result
/// opens no `approve_close` ask (`replan_result_withheld`): the run is
/// parked out of its slot with its work saved, and its task stays in
/// progress.
#[test]
fn a_claim_whose_verification_runs_at_the_fence_is_parked_without_its_ask() {
    let (fixture, repo, db) = fixture();
    let gate = fixture.path().join("verification-gate");
    let verify = json!(["test -f seed.txt", common::await_path(&gate)]);
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands=?1 WHERE id=1",
            [verify.to_string()],
        )
        .unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "after it", &[TaskId::new(1)]);
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    backend.script_for(1, SATISFIED_AGENT);
    let (_reviewer, supervisor) = runtime_support::headless::supervise_thread(
        &db,
        &repo,
        backend.clone(),
        Default::default(),
        &[],
    );
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !payloads(
            &queue.show(TaskId::new(1)).unwrap(),
            "run_satisfied_snapshot",
        )
        .is_empty()
    });
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let op = pause_replan(&db, run.id());
    fs::write(&gate, "").unwrap();
    finished(&db, &backend, supervisor);
    assert_parked_unclosed(&db, &op);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let withheld = payloads(&detail, "replan_result_withheld");
    assert_eq!(withheld.len(), 1, "{withheld:?}");
    assert_eq!(withheld[0]["what"], "satisfied_verification");
    assert_eq!(withheld[0]["result"]["passed"], true);
    assert!(payloads(&detail, "run_satisfied_verified").is_empty());
    assert!(
        queue
            .asks(Default::default())
            .unwrap()
            .iter()
            .all(|ask| ask.kind != dagq::domain::AskKind::ApproveClose)
    );
}

/// Acceptance (ADR-t1521-2, the park): a claim waiting unleased for the
/// answer to its close question is parked where it rests once an
/// operation is opened on its run. A `close` answered after the fence does
/// not close the task nor free its dependent: the park closes the ask, and
/// the close's own transaction refuses a task being replanned.
#[test]
fn a_claim_waiting_for_its_close_answer_is_parked_and_the_answer_does_not_close_it() {
    let (_fixture, repo, db, backend, reviewer) = claimed();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let ask = close_ask(&mut queue);
    let op = pause_replan(&db, run.id());
    queue.answer(ask.id, "close").unwrap();
    supervise_reviewed(&db, &repo, &backend, &reviewer, 2);
    assert_parked_unclosed(&db, &op);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let refused = queue
        .close_satisfied(run.id(), None, json!({}))
        .unwrap_err()
        .to_string();
    assert!(refused.contains("being replanned"), "{refused}");
}

/// The actor's `requested_by` of every event of `kind` of `run`.
fn requested_by(detail: &dagq::domain::TaskDetail, run: &RunId, kind: &str) -> Vec<Option<String>> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == kind && e.run_id.as_ref() == Some(run))
        .map(|e| e.actor.as_ref().and_then(|a| a.requested_by.clone()))
        .collect()
}

/// A script of the claim's judgment: what `printed`, after it wrote the
/// head and the directory it runs in to `seen`.
fn judging(seen: &Path, printed: &Value) -> String {
    format!(
        "{{ git rev-parse HEAD; pwd -P; }} > {}; printf '%s\\n' '{printed}'",
        shell_path(seen)
    )
}

/// Acceptance (ADR-t1818-1 decision 4): once the claim's verification
/// passed on main's head, one review job judges it in a checkout of that
/// head of the queue's `satisfied/` (not the run's worktree or directory),
/// its prompt carrying the task's acceptance and verification, the
/// receipt's grounds and the head with what passed there; no review
/// subagent and no review program run. A `low` pass goes to the person:
/// the `approve_close` ask names the head and the review's verdict,
/// confidence, recommendation and reasons, and the judgment's event and the
/// ask's are recorded at the job's request; the tree goes.
#[test]
fn a_passed_claim_is_judged_on_its_head_and_asks_the_person_with_the_judgment() {
    let seen_dir = tempfile::tempdir().unwrap();
    let seen = seen_dir.path().join("seen");
    let printed = json!({"verdict": "pass", "reasons": ["seed.txt is there"],
        "summary": "main meets it", "confidence": "low", "recommendation": "close"});
    let (_fixture, repo, db, _backend, reviewer) =
        claimed_judged(false, |_| {}, &[judging(&seen, &printed)]);
    let head = main_head(&repo);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Validating);
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert!(queue.run_lease(run.id()).unwrap().is_none());

    // The judgment ran on the snapshot's head, in the queue's scratch.
    let seen = fs::read_to_string(&seen).unwrap();
    let mut seen = seen.lines();
    assert_eq!(seen.next(), Some(head.as_str()));
    let tree = PathBuf::from(seen.next().unwrap());
    let scratch = satisfied_scratch(&db, run.id());
    let scratch = fs::canonicalize(scratch.parent().unwrap())
        .unwrap()
        .join(scratch.file_name().unwrap());
    assert_eq!(tree, scratch);
    assert!(!tree.starts_with(fs::canonicalize(run.run_dir().unwrap()).unwrap()));
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let task = &detail.task;
    for said in [
        &format!("main at {head} (the snapshot)"),
        &format!(
            "{} is a checkout of main at {head}",
            satisfied_scratch(&db, run.id()).display()
        ),
        task.acceptance(),
        "test ! -f broken.txt",
        "the base meets it",
        "- test -f seed.txt: passed",
        "2 verification commands",
    ] {
        assert!(prompts[0].contains(said), "{said}: {}", prompts[0]);
    }
    // No subagent and no program: there is no diff for them.
    assert!(reviewer.launches().is_empty());
    let kinds = event_kinds(&detail);
    for none in [
        "review_started",
        "review_job_started",
        "review_program_finished",
    ] {
        assert!(!kinds.contains(&none), "{none}: {kinds:?}");
    }
    assert_eq!(
        payloads(&detail, "run_satisfied_review_started")[0]["head"],
        json!(head)
    );
    let finished = payloads(&detail, "run_satisfied_review_job_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["outcome"], "answered");
    let judged = payloads(&detail, "run_satisfied_judged");
    assert_eq!(
        (
            &judged[0]["verdict"],
            &judged[0]["confidence"],
            &judged[0]["head"]
        ),
        (&json!("pass"), &json!("low"), &json!(head))
    );
    assert_eq!(
        (&judged[0]["step"], &judged[0]["because"]),
        (&json!("ask"), &json!("not_high_pass"))
    );

    let ask = close_ask(&mut queue);
    assert_eq!(ask.options, ["close", "retry", "cancel"]);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::Scope);
    assert_eq!(ask.recommendation.as_deref(), Some("close"));
    assert_eq!(ask.confidence, Some(dagq::domain::AskConfidence::Low));
    for said in [
        head.as_str(),
        "pass with low confidence, recommending close",
        "main meets it",
        "1. seed.txt is there",
    ] {
        assert!(ask.question.contains(said), "{said}: {}", ask.question);
    }
    // At the judgment job's request: its verdict and the ask it opened,
    // and nothing before it or after the ask.
    let job = Some(format!("review-job:{}:satisfied-1", run.id()));
    assert_eq!(
        requested_by(&detail, run.id(), "run_satisfied_judged"),
        std::slice::from_ref(&job)
    );
    let opened: Vec<_> = detail
        .events
        .iter()
        .filter(|e| e.kind == "ask_opened" && e.payload["kind"] == "approve_close")
        .map(|e| e.actor.as_ref().and_then(|a| a.requested_by.clone()))
        .collect();
    assert_eq!(opened, [job]);
    for kind in [
        "run_satisfied_review_started",
        "run_satisfied_review_job_finished",
        "lease_released",
    ] {
        assert!(
            requested_by(&detail, run.id(), kind)
                .iter()
                .all(Option::is_none),
            "{kind}"
        );
    }
    assert_eq!(satisfied_worktrees(&repo, &db), Vec::<String>::new());
}

/// A judgment the job does not print in the claim's form runs once more,
/// as a run review's job does; a second one that gives none fails the job
/// to the person: the `approve_close` ask (still offering close, the
/// verification passed) says no judgment came, as a failed job's
/// (`recovery_failed`), recorded at nobody's request.
#[test]
fn an_unreadable_judgment_runs_once_more_and_then_asks_as_a_failed_job() {
    let (_fixture, repo, db, _backend, reviewer) = claimed_judged(
        false,
        |_| {},
        &[
            "printf 'no judgment here'".to_owned(),
            verdict("maybe", &[], "still none"),
        ],
    );
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(reviewer.prompts().len(), 2);
    let finished: Vec<_> = payloads(&detail, "run_satisfied_review_job_finished")
        .into_iter()
        .map(|p| {
            (
                p["outcome"].clone(),
                p["again"].clone(),
                p["runs_again"].clone(),
            )
        })
        .collect();
    assert_eq!(
        finished,
        [
            (json!("unreadable"), json!(false), json!(true)),
            (json!("unreadable"), json!(true), json!(false)),
        ]
    );
    let judged = payloads(&detail, "run_satisfied_judged");
    assert_eq!(judged.len(), 1);
    assert!(
        judged[0]["error"]
            .as_str()
            .unwrap()
            .contains("satisfied-review-1-again.out"),
        "{}",
        judged[0]
    );
    let ask = close_ask(&mut queue);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::RecoveryFailed);
    assert_eq!(ask.options, ["close", "retry", "cancel"]);
    assert!(
        ask.question.contains("gave no judgment"),
        "{}",
        ask.question
    );
    assert_eq!(
        requested_by(&detail, run.id(), "run_satisfied_judged"),
        [None]
    );
    assert!(
        detail
            .events
            .iter()
            .filter(|e| e.kind == "ask_opened")
            .all(|e| e.actor.as_ref().is_none_or(|a| a.requested_by.is_none()))
    );
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert_eq!(satisfied_worktrees(&repo, &db), Vec::<String>::new());
}

/// Acceptance (ADR-t1521-2, the park): an operation opened while the
/// claim's judgment job runs lets the job end (it is not abandoned), but
/// what it judged opens no `approve_close` ask (`replan_result_withheld`):
/// the run is parked out of its slot, and its task stays in progress.
#[test]
fn a_claim_whose_judgment_runs_at_the_fence_is_parked_without_its_ask() {
    let (fixture, repo, db) = fixture();
    let gate = fixture.path().join("judgment-gate");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands=?1 WHERE id=1",
            [VERIFY],
        )
        .unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "after it", &[TaskId::new(1)]);
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    backend.script_for(1, SATISFIED_AGENT);
    let printed = json!({"verdict": "pass", "reasons": [], "summary": "met", "confidence": "high"});
    let reviewer = Arc::new(TestReviewer::new(&[format!(
        "{}; printf '%s\\n' '{printed}'",
        common::await_path(&gate)
    )]));
    // The service the supervisor starts takes the test as its owner.
    let dagq = common::service::owned_executable(db.parent().unwrap());
    let supervisor = {
        let (db, repo, reviewer) = (db.clone(), repo.clone(), reviewer.clone());
        let backend = backend.clone();
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                &dagq,
                &supervise_options(2, true),
            )
        })
    };
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !payloads(
            &queue.show(TaskId::new(1)).unwrap(),
            "run_satisfied_review_job_started",
        )
        .is_empty()
    });
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let op = pause_replan(&db, run.id());
    fs::write(&gate, "").unwrap();
    finished(&db, &backend, supervisor);
    assert_parked_unclosed(&db, &op);
    let detail = queue.show(TaskId::new(1)).unwrap();
    // The job ended and was reaped, not stopped.
    let ended = payloads(&detail, "run_satisfied_review_job_finished");
    assert_eq!(ended.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(ended[0]["outcome"], "answered");
    assert!(payloads(&detail, "headless_job_stopped").is_empty());
    let withheld = payloads(&detail, "replan_result_withheld");
    assert_eq!(withheld.len(), 1, "{withheld:?}");
    assert_eq!(withheld[0]["what"], "satisfied_judgment");
    assert_eq!(withheld[0]["operation_id"], json!(op.id));
    assert_eq!(withheld[0]["result"]["verdict"], "pass");
    assert!(payloads(&detail, "run_satisfied_judged").is_empty());
    assert!(
        queue
            .asks(Default::default())
            .unwrap()
            .iter()
            .all(|ask| ask.kind != dagq::domain::AskKind::ApproveClose)
    );
    assert_eq!(satisfied_worktrees(&repo, &db), Vec::<String>::new());
}

/// The judgment's script that commits to main in `repo` (main moves while
/// the review judges), then prints `printed`.
fn moving_main(repo: &Path, printed: &Value) -> String {
    format!(
        "git -C {} commit -q --allow-empty -m 'main moves' && printf '%s\\n' '{printed}'",
        shell_path(repo)
    )
}

fn high_pass() -> Value {
    json!({"verdict": "pass", "reasons": ["seed.txt is there"], "summary": "main meets it",
        "confidence": "high", "recommendation": "close"})
}

/// The `step` of each `run_satisfied_judged` of task 1, with its `head`.
fn judged_steps(detail: &dagq::domain::TaskDetail) -> Vec<(Value, Value)> {
    payloads(detail, "run_satisfied_judged")
        .into_iter()
        .map(|p| (p["step"].clone(), p["head"].clone()))
        .collect()
}

/// Acceptance (ADR-t1818-1 decisions 3 and 5): a `high` pass on the
/// snapshot main is still at closes the task without a commit and with no
/// ask, its dependent then claimed and landed. `run_satisfied_closed`
/// names the head, the verification, the verdict and confidence and the
/// receipt's grounds, and that the judgment closed it; it and the
/// judgment's event are recorded at the job's request.
#[test]
fn a_high_pass_on_the_snapshot_main_is_still_at_closes_the_claim_with_no_ask() {
    let (_fixture, repo, db, _backend, _reviewer) = claimed_judged(
        true,
        |_| {},
        &[
            format!("printf '%s\\n' '{}'", high_pass()),
            verdict("pass", &[], "fine"),
        ],
    );
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    let detail = queue.show(TaskId::new(1)).unwrap();
    let snapshots = payloads(&detail, "run_satisfied_snapshot");
    assert_eq!(snapshots.len(), 1);
    let head = snapshots[0]["head"].as_str().unwrap().to_owned();
    let run = detail.runs[0].clone();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(run.status(), RunStatus::Succeeded);
    assert_eq!(run.result_commit(), None);
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert_eq!(judged_steps(&detail), [(json!("close"), json!(head))]);
    let verified = &detail
        .events
        .iter()
        .find(|e| e.kind == "run_satisfied_verified")
        .unwrap();
    let closed = payloads(&detail, "run_satisfied_closed");
    assert_eq!(
        closed,
        [
            &json!({"head": head, "verified_event": verified.id, "closed_by": "judgment",
            "attempt": 1, "verdict": "pass", "confidence": "high",
            "receipt_summary": "the base meets it",
            "base_checks": [{"command": "test -f seed.txt", "result": "passed"}]})
        ]
    );
    let job = Some(format!("review-job:{}:satisfied-1", run.id()));
    for kind in ["run_satisfied_judged", "run_satisfied_closed"] {
        assert_eq!(
            requested_by(&detail, run.id(), kind),
            std::slice::from_ref(&job),
            "{kind}"
        );
    }
    assert!(event_kinds(&detail).iter().all(|k| *k != "ask_opened"));
    let after = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(after.runs[0].status(), RunStatus::Integrated);
    assert_eq!(satisfied_worktrees(&repo, &db), Vec::<String>::new());
    let stats = stats_full(&db);
    assert_eq!(
        stats["overall"]["already_satisfied"], 1,
        "{}",
        stats["overall"]
    );
}

/// Acceptance (ADR-t1818-1 decision 3): main moves while the review judges
/// the claim, so its `high` pass does not close the task: the claim is
/// verified on main's new head, at the judgment job's request, and judged
/// again there, and that judgment's `high` pass closes it on the new head.
#[test]
fn a_high_pass_main_moved_past_during_the_review_is_verified_and_judged_again_on_the_new_head() {
    let (_fixture, repo, db, _backend, reviewer) = claimed_judged_by(false, |repo| {
        vec![
            moving_main(repo, &high_pass()),
            format!("printf '%s\\n' '{}'", high_pass()),
        ]
    });
    let moved = main_head(&repo);
    let first = git_out(&repo, &["rev-parse", "main~1"]).trim().to_owned();
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(
        payloads(&detail, "run_satisfied_snapshot"),
        [&json!({"head": first}), &json!({"head": moved})]
    );
    let judged = payloads(&detail, "run_satisfied_judged");
    assert_eq!(judged[0]["main"], json!(moved));
    assert_eq!(
        judged_steps(&detail),
        [
            (json!("rejudge"), json!(first)),
            (json!("close"), json!(moved))
        ]
    );
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert!(
        prompts[1].contains(&format!("main at {moved} (the snapshot)")),
        "{}",
        prompts[1]
    );
    let closed = payloads(&detail, "run_satisfied_closed");
    assert_eq!(
        (
            &closed[0]["head"],
            &closed[0]["attempt"],
            &closed[0]["closed_by"]
        ),
        (&json!(moved), &json!(2), &json!("judgment"))
    );
    // The first judgment's job asked for the verification again, the
    // second for the close.
    let job = |attempt: usize| Some(format!("review-job:{}:satisfied-{attempt}", run.id()));
    assert_eq!(
        requested_by(&detail, run.id(), "run_satisfied_snapshot"),
        [None, job(1)]
    );
    assert_eq!(
        requested_by(&detail, run.id(), "run_satisfied_judged"),
        [job(1), job(2)]
    );
    assert_eq!(
        requested_by(&detail, run.id(), "run_satisfied_closed"),
        [job(2)]
    );
}

/// Acceptance (ADR-t1818-1 decision 3, the limit in
/// `satisfied_judgment::MAX_JUDGMENT_RETRIES`): main moves during every
/// judgment, so each `high` pass is verified and judged again on the new
/// head until the limit; past it the claim is not closed but asked of the
/// person, the ask saying main kept moving, at the last job's request.
#[test]
fn a_high_pass_main_keeps_moving_past_is_asked_of_the_person_past_the_limit() {
    use dagq::domain::satisfied_judgment::MAX_JUDGMENT_RETRIES;
    let (_fixture, _repo, db, _backend, reviewer) =
        claimed_judged_by(false, |repo| vec![moving_main(repo, &high_pass())]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert!(payloads(&detail, "run_satisfied_closed").is_empty());
    let judgments = MAX_JUDGMENT_RETRIES + 1;
    assert_eq!(reviewer.prompts().len(), judgments);
    assert_eq!(payloads(&detail, "run_satisfied_verified").len(), judgments);
    let steps: Vec<_> = judged_steps(&detail).into_iter().map(|(s, _)| s).collect();
    let mut expected = vec![json!("rejudge"); MAX_JUDGMENT_RETRIES];
    expected.push(json!("ask"));
    assert_eq!(steps, expected);
    assert_eq!(
        payloads(&detail, "run_satisfied_judged")[MAX_JUDGMENT_RETRIES]["because"],
        "retry_limit"
    );
    let ask = close_ask(&mut queue);
    assert_eq!(ask.options, ["close", "retry", "cancel"]);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::Scope);
    assert!(
        ask.question.contains("Main moved past the snapshot"),
        "{}",
        ask.question
    );
    let opened: Vec<_> = detail
        .events
        .iter()
        .filter(|e| e.kind == "ask_opened" && e.payload["kind"] == "approve_close")
        .map(|e| e.actor.as_ref().and_then(|a| a.requested_by.clone()))
        .collect();
    assert_eq!(
        opened,
        [Some(format!(
            "review-job:{}:satisfied-{judgments}",
            run.id()
        ))]
    );
    assert!(queue.run_lease(run.id()).unwrap().is_none());
}

/// Acceptance: a supervisor recorded a `high` pass on main's head and
/// stopped before it closed the task, its lease left behind; main moved
/// before the next one started. That one adopts the run and does not close
/// it on the old judgment: it validates the claim again, verifies it on
/// main's new head and judges it there, and closes it on that judgment.
#[test]
fn a_high_pass_a_stopped_supervisor_recorded_does_not_close_once_main_moved() {
    let (_fixture, repo, db, backend, _reviewer) = claimed_with(false, |_| {});
    let first = main_head(&repo);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    // What the stopped supervisor left: its ask never opened, the judgment
    // of a high pass recorded to close on, and its lease.
    let ask = close_ask(&mut queue);
    queue.answer(ask.id, "withdrawn").unwrap();
    queue.close_ask(ask.id).unwrap();
    let judged = json!({"attempt": 2, "head": first, "verdict": "pass", "confidence": "high",
        "recommendation": "close", "reasons": [], "summary": "met", "step": "close"});
    queue
        .record_runtime_event(
            run.id(),
            dagq::domain::EventKind::RunSatisfiedJudged,
            judged,
        )
        .unwrap();
    let dead = {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    };
    Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,'stopped',?2,unixepoch())",
            rusqlite::params![run.id().to_string(), dead],
        )
        .unwrap();
    age_lease(&db, &run, 31);
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "main moves"],
    );
    let moved = main_head(&repo);
    let reviewer = TestReviewer::new(&[format!("printf '%s\\n' '{}'", high_pass())]);
    supervise_reviewed(&db, &repo, &backend, &reviewer, 2);

    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(
        !adoption_events(&detail).is_empty(),
        "{:?}",
        event_kinds(&detail)
    );
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let closed = payloads(&detail, "run_satisfied_closed");
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["head"], json!(moved));
    assert_eq!(closed[0]["closed_by"], "judgment");
    assert_eq!(
        payloads(&detail, "run_satisfied_snapshot").last().unwrap()["head"],
        json!(moved)
    );
    assert_eq!(
        judged_steps(&detail).last().unwrap(),
        &(json!("close"), json!(moved))
    );
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 1);
    assert!(
        prompts[0].contains(&format!("main at {moved} (the snapshot)")),
        "{}",
        prompts[0]
    );
}
