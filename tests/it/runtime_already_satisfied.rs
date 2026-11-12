//! Runtime tests: the boundaries of a worker's `already_satisfied` receipt
//! (ADR-t1818-1) that only a real Git, real processes and a supervisor's
//! restart show: the scratch worktree a snapshot of main is verified in,
//! the processes a dead verification left there, and a claim's course read
//! back by the next supervisor. Its wiring through the supervisor is the
//! in-process wiring tests' (`compose::wiring_tests::already_satisfied`),
//! its decisions `domain::satisfied` and `domain::satisfied_judgment`.
use crate::common;
use crate::runtime_support;
use dagq::domain::EventKind;

use runtime_support::*;

/// The agent of a task whose base already meets it: no commit, and an
/// `already_satisfied` receipt naming the base with the check it ran there.
const SATISFIED_AGENT: &str = r#"printf '{"run_id":"%s","result":"already_satisfied","commit":"%s","tests":{"status":"passed","evidence_or_reason":"test -f seed.txt on the base"},"e2e":{"status":"not_applicable","evidence_or_reason":"the runtime runs it"},"subagent_review":{"status":"not_applicable","evidence_or_reason":"no change to review"},"acceptance_items":[{"item":1,"status":"met","evidence":"stub implementation and tests"}],"summary":"the base meets it","base_checks":[{"command":"test -f seed.txt","result":"passed"}]}' "$RUN_ID" "$(git rev-parse HEAD)" > "$RECEIPT.tmp"; mv "$RECEIPT.tmp" "$RECEIPT""#;

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

/// Commit `text` as the main checkout's `dagq.toml`.
fn set_runtimes(repo: &Path, text: &str) {
    fs::write(repo.join("dagq.toml"), text).unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-q", "-m", "runtimes"]);
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
