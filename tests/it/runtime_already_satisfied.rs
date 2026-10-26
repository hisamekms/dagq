//! Runtime tests: a worker's `already_satisfied` receipt (ADR-t1818-1) is
//! no `commit_mismatch`: the runtime lands nothing, runs the task's
//! verification commands on a snapshot of main recorded with its head,
//! has the review judge the claim on that same head, closes the task
//! without a commit on a `high` pass while main is still at that head or
//! else asks a person with the judgment, and closes it on the answer,
//! verifying (and judging) again on main's new head when main moved since.
use crate::common;
use crate::runtime_support;
use dagq::domain::EventKind;

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
    supervise_reviewed_by(
        db,
        repo,
        backend,
        reviewer,
        parallel,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
    )
}

/// [`supervise_reviewed`] given the `dagq` the test owns
/// ([`common::service::owned_executable`], in the queue's directory), so a
/// service the supervisor starts stops once the test's process is gone.
fn supervise_owned(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    reviewer: &TestReviewer,
    parallel: usize,
) -> Value {
    let dagq = common::service::owned_executable(db.parent().unwrap());
    supervise_reviewed_by(db, repo, backend, reviewer, parallel, &dagq)
}

/// [`supervise_reviewed`], the supervisor given `dagq` as the executable
/// it starts.
fn supervise_reviewed_by(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    reviewer: &TestReviewer,
    parallel: usize,
    dagq: &Path,
) -> Value {
    let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
    let outcome = runtime::supervise_with_reviewer(
        db,
        repo,
        backend,
        &claude_stub(db),
        reviewer,
        dagq,
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
    claimed_by(dependent, scripts, false)
}

/// [`claimed`] made by a supervisor given the `dagq` the test owns
/// ([`supervise_owned`]).
fn claimed_owned() -> (Fixture, PathBuf, PathBuf, TestWorkspace, TestReviewer) {
    claimed_by(true, |_| vec![verdict("pass", &[], "fine")], true)
}

/// [`claimed_judged_by`], supervised by [`supervise_owned`] when `owned`.
fn claimed_by(
    dependent: bool,
    scripts: impl FnOnce(&Path) -> Vec<String>,
    owned: bool,
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
    if owned {
        supervise_owned(&db, &repo, &backend, &reviewer, 2);
    } else {
        supervise_reviewed(&db, &repo, &backend, &reviewer, 2);
    }
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
        .filter(|e| e.kind == EventKind::RunSatisfiedReviewStarted)
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

/// `[landing_verification]` replacing [`VERIFY`]'s gate the worker does
/// not run by a command that prints the landing's variables and then runs
/// the gate's check.
const LANDING: &str = r#"[landing_verification]
replaces = ['test ! -f broken.txt']
command = 'echo "base=$DAGQ_LANDING_BASE"; echo "list=$DAGQ_CI_KNOWN_FAILURES"; echo "fix=$DAGQ_CI_FIX_RUN"; echo "tmp=$TMPDIR"; mkdir "$TMPDIR/fixture" && : > "$TMPDIR/fixture/data" && test ! -f broken.txt'
"#;

/// With `[landing_verification]` the snapshot of a claim runs the
/// repository's command in place of the task's command it names, as the
/// landing does (ADR-t2740-1): the command gets the snapshot's head as its
/// base, the known failures written to the run's directory and that the
/// task fixes no CI failure; the other command runs as registered. On the
/// host its `TMPDIR` is the run's short directory outside `runs/`, over the
/// one `[run.env]` names, and what it makes there stays there.
#[test]
fn a_snapshot_runs_the_landing_command_in_place_of_what_it_replaces() {
    let mut named = PathBuf::new();
    let (_fixture, repo, db, _backend, _reviewer) = claimed_after(|repo| {
        named = repo.parent().unwrap().join("named-tmp");
        fs::create_dir(&named).unwrap();
        let run_env = format!("[run.env]\nTMPDIR = '{}'\n", named.display());
        set_runtimes(repo, &format!("{LANDING}{run_env}"));
    });
    let head = main_head(&repo);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(
        payloads(&detail, "run_satisfied_verified"),
        [&json!({"head": head, "passed": true, "commands": 2})]
    );
    let run_dir = PathBuf::from(detail.runs[0].run_dir().unwrap());
    let log = |index: usize| {
        fs::read_to_string(run_dir.join(format!("satisfied-{}-{index}.log", &head[..12]))).unwrap()
    };
    assert!(!log(1).contains("base="), "{}", log(1));
    let replaced = log(2);
    let list = run_dir.join("ci-known-failures.json");
    assert!(replaced.contains(&format!("base={head}\n")), "{replaced}");
    assert!(
        replaced.contains(&format!("list={}\n", list.display())),
        "{replaced}"
    );
    assert!(replaced.contains("fix=0\n"), "{replaced}");
    let tmp = dagq::application::host_verification_tmp_path(detail.runs[0].id());
    // The task goes on, so no cleanup removes it.
    let _gone = RemoveOnDrop(tmp.clone());
    assert!(
        replaced.contains(&format!("tmp={}\n", tmp.display())),
        "{replaced}"
    );
    assert!(!tmp.starts_with(&run_dir));
    assert!(tmp.join("fixture/data").is_file());
    assert!(!named.join("fixture").exists());
    let failures: Value = serde_json::from_str(&fs::read_to_string(&list).unwrap()).unwrap();
    assert_eq!(failures["failures"], json!([]), "{failures}");
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
fn requested_by(
    detail: &dagq::domain::TaskDetail,
    run: &RunId,
    kind: EventKind,
) -> Vec<Option<String>> {
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
        requested_by(&detail, run.id(), EventKind::RunSatisfiedJudged),
        std::slice::from_ref(&job)
    );
    let opened: Vec<_> = detail
        .events
        .iter()
        .filter(|e| e.kind == EventKind::AskOpened && e.payload["kind"] == "approve_close")
        .map(|e| e.actor.as_ref().and_then(|a| a.requested_by.clone()))
        .collect();
    assert_eq!(opened, [job]);
    for kind in [
        EventKind::RunSatisfiedReviewStarted,
        EventKind::RunSatisfiedReviewJobFinished,
        EventKind::LeaseReleased,
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
        requested_by(&detail, run.id(), EventKind::RunSatisfiedJudged),
        [None]
    );
    assert!(
        detail
            .events
            .iter()
            .filter(|e| e.kind == EventKind::AskOpened)
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
        .find(|e| e.kind == EventKind::RunSatisfiedVerified)
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
    for kind in [EventKind::RunSatisfiedJudged, EventKind::RunSatisfiedClosed] {
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
        requested_by(&detail, run.id(), EventKind::RunSatisfiedSnapshot),
        [None, job(1)]
    );
    assert_eq!(
        requested_by(&detail, run.id(), EventKind::RunSatisfiedJudged),
        [job(1), job(2)]
    );
    assert_eq!(
        requested_by(&detail, run.id(), EventKind::RunSatisfiedClosed),
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
        .filter(|e| e.kind == EventKind::AskOpened && e.payload["kind"] == "approve_close")
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

/// A supervisor of [`judged_on_a_thread`] on its thread, with what it
/// runs on.
struct OnAThread {
    _fixture: Fixture,
    repo: PathBuf,
    db: PathBuf,
    /// The `dagq` its supervisors start the service with, owned by the
    /// test ([`common::service::owned_executable`]).
    dagq: PathBuf,
    backend: Arc<TestWorkspace>,
    reviewer: Arc<TestReviewer>,
    supervisor: thread::JoinHandle<Result<Value>>,
}

/// Task 1 of a fresh fixture verified by [`VERIFY`], its worker
/// [`SATISFIED_AGENT`], and a supervisor on a thread whose review's jobs
/// run `scripts` in order, under `options`.
fn judged_on_a_thread(scripts: &[String], options: SuperviseOptions) -> OnAThread {
    let (fixture, repo, db) = fixture();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands=?1 WHERE id=1",
            [VERIFY],
        )
        .unwrap();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    backend.script_for(1, SATISFIED_AGENT);
    let reviewer = Arc::new(TestReviewer::new(scripts));
    // The service the supervisor starts takes the test as its owner.
    let dagq = common::service::owned_executable(db.parent().unwrap());
    let supervisor = {
        let (db, repo, dagq) = (db.clone(), repo.clone(), dagq.clone());
        let (backend, reviewer) = (backend.clone(), reviewer.clone());
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                &dagq,
                &options,
            )
        })
    };
    OnAThread {
        _fixture: fixture,
        repo,
        db,
        dagq,
        backend,
        reviewer,
        supervisor,
    }
}

fn low_pass() -> Value {
    json!({"verdict": "pass", "reasons": ["seed.txt is there"], "summary": "main meets it",
        "confidence": "low", "recommendation": "close"})
}

/// The `outcome` of each `run_satisfied_review_job_finished` of `detail`.
fn job_outcomes(detail: &dagq::domain::TaskDetail) -> Vec<Value> {
    payloads(detail, "run_satisfied_review_job_finished")
        .into_iter()
        .map(|p| p["outcome"].clone())
        .collect()
}

/// Acceptance (review.md, the claim's judgment: a provider's failure): a
/// judgment job that stops at Claude's login is no failed job and no run
/// once more: its end is recorded `held`, it joins the authentication
/// `queue_hold` ask as the run's review job, and no `approve_close` ask
/// opens while the hold is open. Once a person answers `done`, the same
/// judgment's job starts again on the same head and its judgment goes to
/// the person as a judgment (reason `scope`), not as a failed job's.
#[test]
fn a_judgment_stopped_at_a_login_waits_for_the_hold_and_runs_again_on_its_head_after_done() {
    let OnAThread {
        _fixture,
        repo,
        db,
        dagq: _,
        backend,
        reviewer,
        supervisor,
    } = judged_on_a_thread(
        &[
            "printf 'Invalid API key · Please run /login\\n'; exit 1".to_owned(),
            format!("printf '%s\\n' '{}'", low_pass()),
        ],
        supervise_options(2, true),
    );
    let head = main_head(&repo);
    let hold = |queue: &mut SqliteQueue| {
        queue
            .asks(AskQuery {
                open: true,
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .find(|ask| ask.kind == AskKind::QueueHold)
    };
    wait_until(&db, common::STEP_LIMIT, |queue| hold(queue).is_some());
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = hold(&mut queue).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(ask.reason_category, dagq::domain::AskReason::Authentication);
    assert_eq!(
        ask.affected,
        [dagq::domain::queue_hold::HoldJob::Review(run.id().clone()).entry()]
    );
    // Held while the ask is open: no judgment and no ask of the claim.
    // The job's finish is written after its `auth_required`.
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !job_outcomes(&queue.show(TaskId::new(1)).unwrap()).is_empty()
    });
    let detail = queue.show(TaskId::new(1)).unwrap();
    let required = payloads(&detail, "auth_required");
    assert_eq!(required.len(), 1, "{required:?}");
    assert_eq!(required[0]["job"], "review");
    let held = payloads(&detail, "run_satisfied_review_job_finished");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["outcome"], "held");
    assert!(held[0]["runs_again"].is_null(), "{}", held[0]);
    assert!(payloads(&detail, "run_satisfied_judged").is_empty());
    assert!(
        queue
            .asks(Default::default())
            .unwrap()
            .iter()
            .all(|ask| ask.kind != AskKind::ApproveClose)
    );

    queue.answer(ask.id, "done").unwrap();
    finished(&db, &backend, supervisor);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(job_outcomes(&detail), [json!("held"), json!("answered")]);
    // The same judgment on the same head: one start of the judgment, its
    // job started twice, neither as its run once more.
    let begun = payloads(&detail, "run_satisfied_review_started");
    assert_eq!(begun.len(), 1, "{begun:?}");
    assert_eq!(begun[0]["head"], json!(head));
    let started: Vec<_> = payloads(&detail, "run_satisfied_review_job_started")
        .into_iter()
        .map(|p| (p["attempt"].clone(), p["again"].clone()))
        .collect();
    assert_eq!(
        started,
        [(json!(1), json!(false)), (json!(1), json!(false))]
    );
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    for prompt in &prompts {
        assert!(
            prompt.contains(&format!("main at {head} (the snapshot)")),
            "{prompt}"
        );
    }
    // `done` was applied to the judgment's job, as a run review's.
    let applied: Vec<_> = queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::QueueHoldApplied)
        .map(|e| e.payload)
        .collect();
    assert_eq!(applied.len(), 1, "{applied:?}");
    assert_eq!(applied[0]["jobs"], json!(ask.affected), "{applied:?}");
    assert_eq!(
        judged_steps(&detail),
        [(json!("ask"), json!(head))],
        "{:?}",
        event_kinds(&detail)
    );
    let close = close_ask(&mut queue);
    assert_eq!(close.reason_category, dagq::domain::AskReason::Scope);
    assert_eq!(close.recommendation.as_deref(), Some("close"));
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert_eq!(satisfied_worktrees(&repo, &db), Vec::<String>::new());
}

/// Acceptance (review.md, the claim's judgment; `--no-claude`): with no
/// provider left for the review (`ReviewRoute::Manual`), a claim whose
/// verification passed starts no judgment job: the reviewer is never
/// called, and the person gets the `approve_close` ask of a failed job
/// (`recovery_failed`), still offering close; the run rests unleased.
#[test]
fn without_claude_a_passed_claim_starts_no_judgment_job_and_asks_as_a_failed_job() {
    use crate::runtime_support::codex::{TASK, codex_fixture};
    let (dir, repo, db, backend, codex) = codex_fixture();
    // The Codex worker's turn: no commit, and an `already_satisfied`
    // receipt naming the base with the check it ran there.
    set_turns(
        dir.path(),
        r#"printf '{"run_id":"%s","result":"already_satisfied","commit":"%s","tests":{"status":"passed","evidence_or_reason":"test -f seed.txt on the base"},"e2e":{"status":"not_applicable","evidence_or_reason":"the runtime runs it"},"subagent_review":{"status":"not_applicable","evidence_or_reason":"no change to review"},"summary":"the base meets it","base_checks":[{"command":"test -f seed.txt","result":"passed"}]}' "$DAGQ_RUN_ID" "$(git rev-parse HEAD)" > "$RECEIPT.tmp"; mv "$RECEIPT.tmp" "$RECEIPT"; say finished"#,
    );
    let reviewer = TestReviewer::new(&[]);
    let options = SuperviseOptions {
        no_claude: true,
        codex,
        ..supervise_options(1, true)
    };
    let outcome = {
        let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
        runtime::supervise_with_reviewer(
            &db,
            &repo,
            &backend,
            &dir.path().join("missing-claude"),
            &reviewer,
            &common::service::owned_executable(db.parent().unwrap()),
            &options,
        )
        .unwrap()
    };
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let head = main_head(&repo);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TASK).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    let verified = payloads(&detail, "run_satisfied_verified");
    assert_eq!(verified.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(verified[0]["head"], json!(head));
    // No job: neither started nor finished, and the reviewer never called.
    assert!(reviewer.prompts().is_empty() && reviewer.launches().is_empty());
    for none in [
        "run_satisfied_review_job_started",
        "run_satisfied_review_job_finished",
        "review_started",
    ] {
        assert!(payloads(&detail, none).is_empty(), "{none}");
    }
    let judged = payloads(&detail, "run_satisfied_judged");
    assert_eq!(judged.len(), 1, "{judged:?}");
    assert!(
        judged[0]["error"]
            .as_str()
            .unwrap()
            .contains("disabled by --no-claude"),
        "{}",
        judged[0]
    );
    let ask = close_ask(&mut queue);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::RecoveryFailed);
    assert_eq!(ask.options, ["close", "retry", "cancel"]);
    // It says no provider judged the claim, not that a job and its run
    // once more gave no judgment.
    assert!(
        ask.question.contains("disabled by --no-claude")
            && ask.question.contains("did not judge the claim")
            && !ask.question.contains("gave no judgment")
            && !ask.question.contains("once more included"),
        "{}",
        ask.question
    );
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert_eq!(satisfied_worktrees(&repo, &db), Vec::<String>::new());
}

/// Acceptance (review.md, the claim's judgment: the handoff): a handoff
/// while the claim's judgment job runs stops the job (one
/// `headless_job_stopped`) and applies nothing of it. The process that
/// continues under the same token validates the run again, verifies the
/// claim on the snapshot's head and judges it there with a job of its own,
/// whose judgment goes to the person; the stopped job's Execution is not
/// counted again (no second `headless_job_stopped`, no finish of it).
#[test]
fn a_judgment_running_at_a_handoff_is_stopped_and_the_next_process_verifies_and_judges_again() {
    let gate_dir = tempfile::tempdir().unwrap();
    let gate = gate_dir.path().join("judgment-gate");
    // What the gated job leaves once the gate lets it go, if it still runs.
    let outlived = gate_dir.path().join("outlived");
    let OnAThread {
        _fixture,
        repo,
        db,
        dagq,
        backend,
        reviewer,
        supervisor,
    } = judged_on_a_thread(
        &[
            format!(
                "{}; : > {}; printf '%s\\n' '{}'",
                common::await_path(&gate),
                shell_path(&outlived),
                high_pass()
            ),
            format!("printf '%s\\n' '{}'", low_pass()),
        ],
        supervise_options(2, true),
    );
    let head = main_head(&repo);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !payloads(
            &queue.show(TaskId::new(1)).unwrap(),
            "run_satisfied_review_job_started",
        )
        .is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let token = queue.supervisors().unwrap().remove(0).token;
    assert!(queue.request_handoff(&token, "/next/dagq").unwrap());
    let outcome = joined(supervisor, "the supervisor handing off").unwrap();
    assert_eq!(outcome["outcome"], "handoff", "{outcome}");
    // Nothing of the stopped job is applied, and the run stays leased.
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    let stopped = payloads(&detail, "headless_job_stopped");
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(
        (
            &stopped[0]["kind"],
            &stopped[0]["label"],
            &stopped[0]["attempt"]
        ),
        (&json!("review"), &json!("already_satisfied"), &json!(1)),
        "{}",
        stopped[0]
    );
    assert!(job_outcomes(&detail).is_empty());
    assert!(payloads(&detail, "run_satisfied_judged").is_empty());
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    assert!(queue.run_lease(run.id()).unwrap().is_some());
    // A job that outlived its stop goes on from here.
    fs::write(&gate, "").unwrap();

    let next = SuperviseOptions {
        handoff_token: Some(token),
        ..supervise_options(2, true)
    };
    let outcome = {
        let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
        runtime::supervise_with_reviewer(
            &db,
            &repo,
            &*backend,
            &claude_stub(&db),
            &*reviewer,
            &dagq,
            &next,
        )
        .unwrap()
    };
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    // Validated, verified and judged again on the snapshot's head.
    assert_eq!(payloads(&detail, "validation_finished").len(), 2);
    let snapshots: Vec<_> = payloads(&detail, "run_satisfied_verified")
        .into_iter()
        .map(|p| p["head"].clone())
        .collect();
    assert_eq!(snapshots, [json!(head), json!(head)]);
    let begun: Vec<_> = payloads(&detail, "run_satisfied_review_started")
        .into_iter()
        .map(|p| (p["attempt"].clone(), p["head"].clone()))
        .collect();
    assert_eq!(begun, [(json!(1), json!(head)), (json!(2), json!(head))]);
    assert_eq!(reviewer.prompts().len(), 2);
    assert!(!outlived.exists(), "the stopped judgment job still ran");
    // The stopped job's Execution is counted once: by its stop alone.
    assert_eq!(payloads(&detail, "headless_job_stopped").len(), 1);
    assert_eq!(job_outcomes(&detail), [json!("answered")]);
    assert_eq!(
        payloads(&detail, "run_satisfied_review_job_finished")[0]["attempt"],
        2
    );
    // The stopped job's high pass closed nothing: the new judgment asks.
    assert_eq!(judged_steps(&detail), [(json!("ask"), json!(head))]);
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert!(payloads(&detail, "run_satisfied_closed").is_empty());
    let ask = close_ask(&mut queue);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::Scope);
    assert_eq!(ask.confidence, Some(dagq::domain::AskConfidence::Low));
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert_eq!(satisfied_worktrees(&repo, &db), Vec::<String>::new());
}

/// Acceptance (ADR-t1879-1, a task's end closes its hold): a held claim
/// is not closed. Its `close` answer waits through the supervisor's pass,
/// and the close's own transaction refuses it. A `cancel` decided on the
/// claim cancels the task and closes its hold in the same transaction
/// (`task_hold_ended`, `canceled`).
#[test]
fn a_held_claim_is_not_closed_and_its_cancel_closes_the_hold() {
    let (_fixture, repo, db, backend, reviewer) = claimed_owned();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let ask = close_ask(&mut queue);
    let output =
        crate::common::cli::invoke_with(&[], &db, &["hold", "1", "--reason", "the login expired"]);
    assert!(output.status.success());
    queue.answer(ask.id, "close").unwrap();
    supervise_owned(&db, &repo, &backend, &reviewer, 2);
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::InProgress
    );
    let refused = queue
        .close_satisfied(run.id(), None, json!({}))
        .unwrap_err()
        .to_string();
    assert!(refused.contains("held"), "{refused}");
    assert!(queue.task_hold(TaskId::new(1)).unwrap().is_some());

    queue
        .decide_satisfied(
            run.id(),
            "a person canceled it",
            json!({"ask_id": ask.id, "answer": "cancel"}),
            &dagq::domain::satisfied::TaskMove::Cancel(
                dagq::domain::cancel_reason::NewCancel::answered(
                    dagq::domain::AskKind::ApproveClose,
                    ask.id,
                ),
            ),
        )
        .unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    assert!(queue.task_hold(TaskId::new(1)).unwrap().is_none());
    let ended = detail
        .events
        .iter()
        .position(|e| e.kind == EventKind::TaskHoldEnded)
        .expect("no task_hold_ended");
    assert_eq!(detail.events[ended].payload["reason"], "canceled");
    assert_eq!(detail.events[ended - 1].kind, EventKind::TaskStatusChanged);
}

/// A supervisor that drains for a stop does not apply a `close` answer to
/// a claim's ask: no verification on main's new head starts, the ask stays
/// open and the run validating for the next supervisor (ADR-t3330-1).
#[test]
fn a_draining_supervisor_verifies_no_claim_for_its_close_answer() {
    let (_fixture, repo, db, backend, reviewer) = claimed_owned();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = close_ask(&mut queue);
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "main moves"],
    );
    queue.answer(ask.id, "close").unwrap();
    let options = supervise_options(2, true);
    options.stop.store(true, Ordering::SeqCst);
    let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
    let outcome = runtime::supervise_with_reviewer(
        &db,
        &repo,
        &backend,
        &claude_stub(&db),
        &reviewer,
        &common::service::owned_executable(db.parent().unwrap()),
        &options,
    )
    .unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Validating);
    assert_eq!(payloads(&detail, "run_satisfied_verified").len(), 1);
    assert!(payloads(&detail, "run_satisfied_closed").is_empty());
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_none());
}
