//! Runtime tests: how a released hold goes on (ADR-t1879-1), at the
//! boundaries the wiring tests' fakes do not reach: a run released for a
//! new one keeps the head it carries over under its run's ref with the
//! real Git (its uncommitted work saved as a commit, its worktree's latest
//! head past its validated commit), the queue service's client reads and
//! keeps that work with its own Git, and the store's transaction refuses a
//! release whose hold or run moved after it read the work. The supervisor's
//! wiring of a release (the resume in the same or a new session, the new
//! run that carries the work over) is `compose::wiring_tests::task_release`'s;
//! the choice of the branch is `domain::task_hold::release_decision`'s and
//! the model's `domain::worker_model::check_model`'s, whose unit tests hold
//! every case.
use crate::common::cli::invoke_with;
use crate::runtime_support;
use dagq::application::commands::holds::Holds;
use dagq::application::inherit::{ReleasedBranches, ReleasedRead};
use dagq::domain::task_hold::{HoldTarget, ReleaseChoice, ReleaseRecorded};
use dagq::domain::{ActorContext, EventKind, Provider, StaticPolicy, resume};
use dagq::infrastructure::adapters::RunRepositories;
use dagq::infrastructure::git_binary::git_executable;

use runtime_support::headless::{TASK, detail as headless_detail, headless_fixture};
use runtime_support::*;

/// `dagq` as a person at a plain terminal.
fn as_user(db: &Path, args: &[&str]) -> std::process::Output {
    invoke_with(&[], db, args)
}

/// Hold `task` as a person; the hold as recorded.
fn hold(db: &Path, task: &str) -> Value {
    let output = as_user(db, &["hold", task, "--reason", "the login expired"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Release `task` as a person with `args`; the release as recorded.
fn release(db: &Path, task: &str, args: &[&str]) -> Value {
    let output = as_user(db, &[&["release", task], args].concat());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// A release of `task` with `args` is refused with `why` and changes
/// nothing: the hold stays open as it was, the task keeps its provider, and
/// no `task_released` is recorded.
fn release_refused(db: &Path, task: TaskId, args: &[&str], why: &str) {
    let mut queue = SqliteQueue::open(db).unwrap();
    let before = (queue.task_hold(task).unwrap(), queue.show(task).unwrap());
    let output = as_user(db, &[&["release", &task.to_string()], args].concat());
    assert!(!output.status.success(), "the release passed: {args:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(why), "{why} in {stderr}");
    let after = (queue.task_hold(task).unwrap(), queue.show(task).unwrap());
    assert_eq!(after.0, before.0, "the hold changed");
    assert!(after.0.is_some(), "the hold stays open");
    assert_eq!(after.1.task.worker(), before.1.task.worker());
    let runs = |detail: &dagq::domain::TaskDetail| {
        detail
            .runs
            .iter()
            .map(|run| {
                (
                    run.id().clone(),
                    run.status(),
                    run.last_error().map(str::to_owned),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(runs(&after.1), runs(&before.1));
    assert!(payloads(&after.1, "task_released").is_empty());
}

/// The one `task_released` of `detail`.
fn released_event(detail: &dagq::domain::TaskDetail) -> Value {
    let released = payloads(detail, "task_released");
    let [released] = released.as_slice() else {
        panic!("{released:?}")
    };
    (*released).clone()
}

/// The commit `refs/dagq/runs/<run>` keeps.
fn kept(repo: &Path, run: &TaskRun) -> String {
    git_out(
        repo,
        &["rev-parse", &format!("refs/dagq/runs/{}", run.id())],
    )
}

/// Task 2's run parked for a resume by a conflict at its landing, held at
/// once (it rests with no lease): the queue's directory, the repository,
/// the queue, the sessions' backend and the run.
fn held_parked() -> (Fixture, PathBuf, PathBuf, TestWorkspace, TaskRun) {
    let (dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    assert_eq!(hold(&db, "2")["state"], "held");
    (dir, repo, db, backend, run)
}

/// A recovery job's verdict that escalates: the run waits for a person
/// with a `decide` ask.
fn escalate() -> String {
    recovery(json!({
        "verdict": "escalate",
        "confidence": "high",
        "diagnosis": "a person decides",
        "question": "Carry the work over?",
    }))
}

/// The `triage_finished` events of `run` with action `retry_inherit`.
fn inherit_retries<'a>(detail: &'a dagq::domain::TaskDetail, run: &TaskRun) -> Vec<&'a Value> {
    detail
        .events
        .iter()
        .filter(|e| {
            e.kind == EventKind::TriageFinished
                && e.run_id.as_ref() == Some(run.id())
                && e.payload["action"] == "retry_inherit"
        })
        .map(|e| &e.payload)
        .collect()
}

/// A release whose head cannot be kept as `refs/dagq/runs/<run-id>` (a ref
/// under that name is in the way) is refused and changes nothing: the
/// hold, the failed run, its recovery job's ask and the task stay as they
/// were, and the worktree keeps its uncommitted work. Once the ref can be
/// written, the release keeps the saved work on top of the run's commits
/// under it, closes the recovery job's ask and records its carry-over as
/// the person's, which leaves the task's automatic one unused.
#[test]
fn a_release_whose_carried_head_cannot_be_kept_is_refused_and_changes_nothing() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "commit work; exit 7");
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[escalate()]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let task = TaskId::new(1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let failed = queue.show(task).unwrap().runs[0].clone();
    assert_eq!(failed.status(), RunStatus::Failed);
    let worktree = Path::new(failed.worktree_path().unwrap());
    fs::write(worktree.join("notes.txt"), "notes\n").unwrap();
    let head = git_out(worktree, &["rev-parse", "HEAD"]);
    assert_eq!(hold(&db, "1")["state"], "held");
    let blocker = format!("refs/dagq/runs/{}/blocker", failed.id());
    git(&repo, &["update-ref", &blocker, &head]);

    release_refused(&db, task, &[], "could not be kept");
    assert_eq!(queue.unclosed_run_asks(failed.id()).unwrap().len(), 1);
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert!(inherit_retries(&detail, &failed).is_empty());
    assert!(git_out(worktree, &["status", "--porcelain"]).contains("?? notes.txt"));

    git(&repo, &["update-ref", "-d", &blocker]);
    let released = release(&db, "1", &[]);
    assert_eq!(released["decision"], "retry_inherit", "{released}");
    let saved = released["inherit"]["saved"].as_str().unwrap().to_owned();
    assert_eq!(released["inherit"]["head"], json!(saved));
    assert_eq!(kept(&repo, &failed), saved);
    assert_eq!(git_out(&repo, &["rev-parse", &format!("{saved}^")]), head);
    assert!(queue.unclosed_run_asks(failed.id()).unwrap().is_empty());
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Ready);
    // The carry-over is the person's: the task's one automatic
    // retry_inherit stays unused for the recovery job of the next run.
    let retries = inherit_retries(&detail, &failed);
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0]["by"], "user", "{}", retries[0]);
    assert!(!detail.events.iter().any(resume::uses_automatic_inherit));
}

/// A release that ends its run through the queue service (ADR-t3102-3
/// decision 5): the client reads the run's work, saves its uncommitted
/// work as a commit and keeps the head with Git before it sends the
/// release, and the service, whose Git logs each run, runs none; the
/// release then carries that work over as the command line did on the DB.
#[test]
fn a_release_through_the_service_reads_and_keeps_the_work_with_the_client_s_git() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "commit work; exit 7");
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[escalate()]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let task = TaskId::new(1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let failed = queue.show(task).unwrap().runs[0].clone();
    assert_eq!(failed.status(), RunStatus::Failed);
    let worktree = Path::new(failed.worktree_path().unwrap());
    fs::write(worktree.join("notes.txt"), "notes\n").unwrap();
    let head = git_out(worktree, &["rev-parse", "HEAD"]);
    // As `invoke_with` runs the person's `dagq`: its host-wide settings
    // beside the queue.
    let config = db.parent().unwrap().join("config");
    let git_log = crate::common::service::serve_logging_git(
        &db,
        &[("XDG_CONFIG_HOME", config.to_str().unwrap())],
    );
    assert_eq!(hold(&db, "1")["state"], "held");
    fs::write(&git_log, "").unwrap();

    let released = release(&db, "1", &[]);
    assert_eq!(released["decision"], "retry_inherit", "{released}");
    let saved = released["inherit"]["saved"].as_str().unwrap().to_owned();
    assert_eq!(kept(&repo, &failed), saved);
    assert_eq!(git_out(&repo, &["rev-parse", &format!("{saved}^")]), head);
    assert_eq!(
        fs::read_to_string(&git_log).unwrap(),
        "",
        "the service ran Git"
    );
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Ready);
    assert_eq!(inherit_retries(&detail, &failed).len(), 1);
}

/// A client-mode `dagq` (`DAGQ_SERVICE_SOCKET` names the service, no
/// `--db`), which knows no queue directory, releases a host run's hold
/// and carries its work over as the host's `dagq` does: it reads and keeps
/// the work with its own Git, and the service records the release as the
/// user's.
#[test]
fn a_client_mode_release_carries_a_host_run_s_work_over_with_no_queue_path() {
    use crate::common::{Bounded, WithoutActor, service::OwnedByTest};
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "commit work; exit 7");
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[escalate()]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let task = TaskId::new(1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let failed = queue.show(task).unwrap().runs[0].clone();
    let worktree = Path::new(failed.worktree_path().unwrap());
    fs::write(worktree.join("notes.txt"), "notes\n").unwrap();
    let head = git_out(worktree, &["rev-parse", "HEAD"]);
    let config = db.parent().unwrap().join("config");
    crate::common::service::serve_logging_git(
        &db,
        &[("XDG_CONFIG_HOME", config.to_str().unwrap())],
    );
    assert_eq!(hold(&db, "1")["state"], "held");
    let socket = dagq::infrastructure::queue_service::socket_path(
        fs::canonicalize(&db).unwrap().parent().unwrap(),
    );

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .owned_by_test()
        .current_dir(&repo)
        .env("XDG_CONFIG_HOME", &config)
        .env(dagq::domain::queue_service::SOCKET_ENV, &socket)
        .args(["release", "1"])
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let released: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(released["decision"], "retry_inherit", "{released}");
    let saved = released["inherit"]["saved"].as_str().unwrap().to_owned();
    assert_eq!(kept(&repo, &failed), saved);
    assert_eq!(git_out(&repo, &["rev-parse", &format!("{saved}^")]), head);
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Ready);
    assert_eq!(inherit_retries(&detail, &failed).len(), 1);
    let actors: Vec<Value> = crate::common::service::events(&db, "task_released")
        .into_iter()
        .map(|event| event["actor"]["role"].clone())
        .collect();
    assert_eq!(actors, [json!("user")], "{released}");
}

/// Acceptance (b) and (c): a release that ends the run carries over the
/// latest work its worktree has, not the commit its review passed. A
/// waiting run whose validated commit a commit made while it was held went
/// past, its worktree clean, is released to codex with that later commit
/// carried over, recorded for the next run and kept. While the worktree's
/// `HEAD` is off the run's branch, the release is refused and changes
/// nothing.
#[test]
fn a_release_carries_the_worktrees_head_past_the_runs_validated_commit() {
    let (_dir, repo, db, _backend, run) = held_parked();
    let validated = run.result_commit().expect("validated").to_string();
    let worktree = Path::new(run.worktree_path().unwrap()).to_owned();
    assert_eq!(git_out(&worktree, &["rev-parse", "HEAD"]), validated);
    fs::write(worktree.join("later.txt"), "later\n").unwrap();
    git(&worktree, &["add", "later.txt"]);
    git(&worktree, &["commit", "-qm", "later work"]);
    let later = git_out(&worktree, &["rev-parse", "HEAD"]);
    assert_eq!(git_out(&worktree, &["status", "--porcelain"]), "");
    git(&worktree, &["checkout", "-q", "--detach"]);
    release_refused(
        &db,
        TASK,
        &["--provider", "codex"],
        "on a detached HEAD instead of refs/heads/",
    );
    git(&worktree, &["checkout", "-q", run.branch().unwrap()]);

    let released = release(&db, "2", &["--provider", "codex"]);
    assert_eq!(released["decision"], "retry_inherit", "{released}");
    assert_eq!(released["inherit"]["head"], json!(later), "{released}");
    assert!(released["inherit"]["saved"].is_null(), "{released}");
    assert_eq!(kept(&repo, &run), later);
    let detail = SqliteQueue::open(&db).unwrap().show(TASK).unwrap();
    assert_eq!(released_event(&detail)["inherit"]["head"], json!(later));
    let retries = inherit_retries(&detail, &run);
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0]["inherit"]["head"], json!(later));
}

/// The run's work as a release reads it, with `meanwhile` done once it was
/// read: what comes between a release's reading and its store's
/// transaction.
struct ReadThen<F: Fn()>(F);

impl<F: Fn()> ReleasedBranches for ReadThen<F> {
    fn released_work(&self, run: &TaskRun) -> Result<ReleasedRead> {
        let read = RunRepositories {
            containers: &NoContainers,
        }
        .released_work(run)?;
        (self.0)();
        Ok(read)
    }

    fn keep(&self, run: &TaskRun, head: &CommitSha) -> Result<()> {
        RunRepositories {
            containers: &NoContainers,
        }
        .keep(run, head)
    }
}

/// A person's `release 2 --provider codex` in this process, with
/// `meanwhile` done after it read the run's work and before its store's
/// transaction.
fn release_to_codex_with(db: &Path, meanwhile: impl Fn()) -> Result<ReleaseRecorded> {
    let mut queue = SqliteQueue::open(db).unwrap();
    let actor = ActorContext::user();
    Holds::new(&mut queue, &actor, &StaticPolicy).release(
        &HoldTarget::Task(TASK),
        ReleaseChoice {
            provider: Some(Provider::Codex),
            model: None,
        },
        &ReadThen(meanwhile),
    )
}

/// A turn of `run` that went on: its `turn_started` and `turn_finished`.
fn went_on(db: &Path, run: &TaskRun, turn: u64) {
    let queue = SqliteQueue::open(db).unwrap();
    queue
        .record_runtime_event(run.id(), EventKind::TurnStarted, json!({"turn": turn}))
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::TurnFinished,
            json!({"turn": turn, "session_created": true, "outcome": "succeeded"}),
        )
        .unwrap();
}

/// A release that ends the run reads its work before its store's
/// transaction. When, in between, another release lets the run go and a
/// new hold settles, the older release is refused (`release_read_moved`)
/// and changes nothing: the newer hold stays open, the run waits as it
/// did, nothing is carried over or kept, and the task keeps its provider.
/// So it is when the run also went on with newer work in between, and
/// when the run went on under the same hold. A release that reads the run
/// as it is then carries the newer work over. (A waiting run's held phase
/// closes and opens again with the holds, so its version moves too; a
/// change of the hold alone is `task_hold::check_release_read`'s, whose
/// unit test holds it.)
#[test]
fn a_release_whose_hold_or_run_moved_after_it_read_the_work_is_refused_and_changes_nothing() {
    let (_dir, repo, db, _backend, run) = held_parked();
    let worktree = Path::new(run.worktree_path().unwrap()).to_owned();
    fs::write(worktree.join("notes.txt"), "notes\n").unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let unmoved =
        |queue: &mut SqliteQueue, before: &dagq::domain::task_hold::TaskHold, released| {
            let hold = queue.task_hold(TASK).unwrap().unwrap();
            assert_ne!(hold.id, before.id, "a new hold is open");
            let detail = queue.show(TASK).unwrap();
            assert_eq!(detail.runs.len(), 1);
            assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
            assert_eq!(detail.task.status(), TaskStatus::InProgress);
            assert_eq!(detail.task.worker().provider, Provider::Claude);
            assert_eq!(payloads(&detail, "task_released").len(), released);
            let kept = format!("refs/dagq/runs/{}", run.id());
            assert!(
                !Command::new(git_executable().unwrap())
                    .arg("-C")
                    .arg(&repo)
                    .args(["rev-parse", "--verify", "-q", &kept])
                    .bounded_output()
                    .unwrap()
                    .status
                    .success()
            );
            hold
        };
    let moved = |refused: anyhow::Error| {
        assert!(
            refused.to_string().contains("release_read_moved"),
            "{refused:#}"
        );
    };
    // Released and held again, the run not gone on yet.
    let first = queue.task_hold(TASK).unwrap().unwrap();
    moved(
        release_to_codex_with(&db, || {
            assert_eq!(release(&db, "2", &[])["decision"], "resume");
            assert_eq!(hold(&db, "2")["state"], "held");
        })
        .unwrap_err(),
    );
    let second = unmoved(&mut queue, &first, 1);
    // Released, resumed with newer work, and held again.
    moved(
        release_to_codex_with(&db, || {
            assert_eq!(release(&db, "2", &[])["decision"], "resume");
            went_on(&db, &run, 2);
            fs::write(worktree.join("newer.txt"), "newer\n").unwrap();
            git(&worktree, &["add", "newer.txt"]);
            git(&worktree, &["commit", "-qm", "newer work"]);
            assert_eq!(hold(&db, "2")["state"], "held");
        })
        .unwrap_err(),
    );
    let third = unmoved(&mut queue, &second, 2);
    // Gone on under the same hold.
    moved(release_to_codex_with(&db, || went_on(&db, &run, 3)).unwrap_err());
    assert_eq!(unmoved(&mut queue, &second, 2), third);

    let released = release(&db, "2", &["--provider", "codex"]);
    assert_eq!(released["decision"], "retry_inherit", "{released}");
    let head = released["inherit"]["head"].as_str().unwrap().to_owned();
    assert_eq!(kept(&repo, &run), head);
    for file in ["newer.txt", "notes.txt"] {
        git(&repo, &["cat-file", "-e", &format!("{head}:{file}")]);
    }
}

/// The case of a turns script for a run that carries an earlier run over:
/// it brings that run's commits onto its base, checks with `check` that
/// the work is all there and finishes.
fn carry_over(check: &str) -> String {
    format!(
        r#"*'Carried over from run'*) RANGE=$(printf '%s\n' "$PROMPT" | sed -n 's/.*its own commits are \([0-9a-f]*\.\.[0-9a-f]*\)\..*/\1/p' | head -n 1); git cherry-pick "$RANGE" >/dev/null 2>&1 || fail "no cherry-pick of $RANGE"; {check} || fail "the work is not here"; receipt "$(git rev-parse HEAD)"; say finished ;;"#
    )
}

/// The turns of task 2. Its first turn (Claude's) waits for the test's
/// `go`, leaves an unstaged change of `seed.txt` and an untracked
/// `notes.txt`, and writes no receipt. A run that carries it over
/// (Codex's) finds all that work ([`carry_over`]).
fn turns(dir: &Path) {
    set_turns(
        dir,
        &format!(
            r#"case "$PROMPT" in
{carried}
*'dagq task 2,'*) await_file "$RUN_DIR/go"; printf 'half\n' >> seed.txt; printf 'notes\n' > notes.txt; say paused ;;
*) fail "unexpected prompt" ;;
esac"#,
            carried = carry_over("test -f notes.txt && grep -q half seed.txt")
        ),
    );
}

/// Supervise with one slot, the stub `codex` too and `reviewer`, on a
/// thread, the `dagq` it starts owned by the test
/// ([`crate::common::service::owned_executable`], in the queue's directory).
fn supervise_both(
    db: &Path,
    repo: &Path,
    backend: Arc<TestWorkspace>,
    codex: &Path,
    reviewer: TestReviewer,
) -> thread::JoinHandle<Result<Value>> {
    let options = SuperviseOptions {
        codex: codex.to_owned(),
        ..supervise_options(1, true)
    };
    let dagq = crate::common::service::owned_executable(db.parent().unwrap());
    let (db, repo) = (db.to_owned(), repo.to_owned());
    thread::spawn(move || {
        runtime::supervise_with_reviewer(
            &db,
            &repo,
            &*backend,
            &claude_stub(&db),
            &reviewer,
            &dagq,
            &options,
        )
    })
}

/// Wait for the supervisor thread and its sessions, with no error.
fn done(backend: &TestWorkspace, supervisor: thread::JoinHandle<Result<Value>>) {
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}

/// Acceptance (e): a run with no commit of its own, held while its turn
/// goes and released to Codex once it waits, starts from the current main
/// with its uncommitted work saved as a commit on top of its head (the
/// worktree left as it was), kept under its run's ref and carried over to
/// the next run on Codex, which lands it.
#[test]
fn a_run_with_no_commit_of_its_own_is_released_to_a_new_run_with_its_uncommitted_work() {
    let (dir, repo, db, mut backend) = headless_fixture(&[]);
    let codex = headless_codex(dir.path(), &db);
    backend.codex = Some(codex.clone());
    let backend = Arc::new(backend);
    turns(dir.path());
    let pass = || TestReviewer::new(&[verdict("pass", &[], "fine")]);
    let supervisor = supervise_both(&db, &repo, backend.clone(), &codex, pass());
    wait_until(&db, crate::common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), "turn_started").is_empty()
    });
    let run = headless_detail(&db).runs[0].clone();
    let run_dir = Path::new(run.run_dir().unwrap()).to_owned();
    assert_eq!(hold(&db, "2")["state"], "settling");
    fs::write(run_dir.join("go"), "").unwrap();
    done(&backend, supervisor);
    let waiting = headless_detail(&db);
    assert_eq!(waiting.runs[0].status(), RunStatus::NeedsSession);
    let worktree = Path::new(run.worktree_path().unwrap());
    let status = git_out(worktree, &["status", "--porcelain"]);
    for line in ["M seed.txt", "?? notes.txt"] {
        assert!(status.contains(line), "{line} in {status}");
    }
    let head = git_out(worktree, &["rev-parse", "HEAD"]);

    let released = release(&db, "2", &["--provider", "codex"]);
    assert_eq!(released["decision"], "retry_fresh", "{released}");
    let carried = released["inherit"]["head"].as_str().unwrap().to_owned();
    assert_eq!(kept(&repo, &run), carried);
    // The uncommitted work is saved on top of the head, the worktree left
    // as it was.
    assert_eq!(released["inherit"]["saved"], json!(carried));
    assert_eq!(git_out(&repo, &["rev-parse", &format!("{carried}^")]), head);
    let saved = git_out(&repo, &["ls-tree", "--name-only", &carried]);
    assert!(saved.contains("notes.txt"), "{saved}");
    assert!(git_out(worktree, &["status", "--porcelain"]).contains("?? notes.txt"));

    let supervisor = supervise_both(&db, &repo, backend.clone(), &codex, pass());
    done(&backend, supervisor);
    let detail = headless_detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(detail.runs.len(), 2, "one new run");
    assert_eq!(detail.runs[1].actual_provider(), Provider::Codex);
    assert_eq!(detail.runs[1].status(), RunStatus::Integrated);
    assert_eq!(payloads(&detail, "run_inherited").len(), 1);
    assert_eq!(released_event(&detail)["decision"], "retry_fresh");
    let landed = git_out(&repo, &["ls-tree", "--name-only", "main"]);
    assert!(landed.contains("notes.txt"), "{landed}");
    assert!(git_out(&repo, &["show", "main:seed.txt"]).contains("half"));
}
