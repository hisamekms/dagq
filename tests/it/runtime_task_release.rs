//! Runtime tests: how a released hold goes on (ADR-t1879-1). A held run
//! with a session on the same provider resumes through the resume's entry
//! (`begin_resume`), in the same session, or in a new one of the same run
//! past `[fresh_session]`'s threshold; a run released to another provider,
//! with no session, or stopped ends, and the next run carries its branch
//! and its uncommitted work (saved as a commit) over. The choice of the
//! branch is `domain::task_hold::release_decision`'s and the model's
//! `domain::worker_model::check_model`'s, whose unit tests hold every
//! case; these hold the wiring of the CLI, the store, Git and the
//! supervisor.
use crate::common::cli::invoke_with;
use crate::runtime_support;
use dagq::application::commands::holds::Holds;
use dagq::application::inherit::{ReleasedBranches, ReleasedRead};
use dagq::domain::task_hold::{HoldTarget, ReleaseChoice, ReleaseRecorded};
use dagq::domain::turn::session_name;
use dagq::domain::{ActorContext, EventKind, Provider, StaticPolicy, resume, worker_model};
use dagq::infrastructure::adapters::RunRepositories;
use dagq::infrastructure::git_binary::git_executable;

use runtime_support::headless::{
    FINISH, TASK, detail as headless_detail, headless_fixture, pause_once, wait_for_file,
};
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
/// once (it rests with no lease), with `dagq.toml` setting `threshold` and
/// its previous turn's context recorded as `peak` tokens; its resumed
/// session resolves the conflict and lands.
fn held_parked(
    threshold: u64,
    peak: u64,
) -> (Fixture, PathBuf, PathBuf, TestWorkspace, TaskRun, String) {
    let (dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    set_fresh_session_threshold(&repo, threshold);
    record_previous_peak(&db, &run, peak);
    let main = git_out(&repo, &["rev-parse", "main"]);
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    assert_eq!(hold(&db, "2")["state"], "held");
    (dir, repo, db, backend, run, main)
}

/// Acceptance (a) and (e): a held run waiting for a session, released on
/// its provider with its previous turn's context at `[fresh_session]`'s
/// threshold, resumes once through the resume's entry in the session it
/// had and lands (the representative of the cases that keep the session:
/// which peaks and thresholds do, unset and unmeasured among them, is
/// `domain::fresh_session::next_session`'s, whose unit tests hold each); a
/// `--model` the provider takes is the resumed session's, and a model it
/// does not take, or any model with codex, is refused with the hold, the
/// provider and the run as they were.
#[test]
fn a_held_run_released_on_its_provider_resumes_its_session_with_the_model_chosen() {
    let (_dir, repo, db, backend, run, main) = held_parked(500, 500);
    let task = TaskId::new(2);
    release_refused(&db, task, &["--model", "gpt-9"], "not one claude takes");
    release_refused(
        &db,
        task,
        &["--provider", "codex", "--model", worker_model::OPUS],
        "codex runs its own model",
    );
    let released = release(&db, "2", &["--model", worker_model::SONNET]);
    assert_eq!(
        (&released["decision"], &released["continuation"]),
        (&json!("resume"), &json!("continue")),
        "{released}"
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db).unwrap().show(task).unwrap();
    assert_eq!(detail.runs.len(), 1, "no new run");
    assert_landed(&repo, &detail.runs[0], "second", &main);
    let resumed = payloads(&detail, "resume_started");
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0]["model"], worker_model::SONNET);
    let recorded = released_event(&detail);
    assert_eq!(recorded["model"], worker_model::SONNET);
    assert_eq!(recorded["decision"], "resume");
    assert!(payloads(&detail, "session_renewed").is_empty());
    let calls = stub_calls(&detail.runs[0]);
    assert!(
        calls
            .last()
            .unwrap()
            .starts_with(&format!("resume {} ", run.id())),
        "{calls:?}"
    );
    assert_eq!(detail.task.worker().provider, Provider::Claude);
}

/// Acceptance (a): past `[fresh_session]`'s threshold, the released run's
/// resume goes through the same entry to a new session of the same run,
/// worktree, branch and provider (`session_renewed`), and lands.
#[test]
fn a_held_run_released_past_the_threshold_resumes_in_a_new_session_of_the_same_run() {
    let (_dir, repo, db, backend, run, main) = held_parked(100, 500);
    assert_eq!(release(&db, "2", &[])["decision"], "resume");
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    assert_eq!(detail.runs.len(), 1, "no new run");
    let landed = &detail.runs[0];
    assert_landed(&repo, landed, "second", &main);
    assert_eq!(
        (landed.worktree_path(), landed.branch()),
        (run.worktree_path(), run.branch())
    );
    assert_eq!(landed.actual_provider(), Provider::Claude);
    let renewed = payloads(&detail, "session_renewed");
    assert_eq!(renewed.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    let name = session_name(run.id().as_str(), 1);
    let calls = stub_calls(landed);
    assert!(
        calls.last().unwrap().starts_with(&format!("start {name} ")),
        "{calls:?}"
    );
}

/// Acceptance (c): task 2 is held after its run's wrapper started and
/// before its first turn. The release is refused while the wrapper may
/// start the turn; the wrapper's fence ends the session with no turn, and
/// the run waits with no session of its provider (a wrapper started opens
/// none). A person commits to its branch while it waits. Released, the run
/// ends with no resume, and a new run carries that commit over and lands
/// it.
#[test]
fn a_run_held_after_its_wrapper_started_and_before_its_first_turn_is_released_to_a_new_run() {
    let (dir, repo, db, mut backend) = headless_fixture(&[]);
    let codex = headless_codex(dir.path(), &db);
    backend.codex = Some(codex.clone());
    let gate = dir.path().join("launch-go");
    backend.launch_gate = Some(gate.clone());
    let backend = Arc::new(backend);
    set_turns(dir.path(), FINISH);
    let pass = || TestReviewer::new(&[verdict("pass", &[], "fine")]);
    let supervisor = supervise_both(&db, &repo, backend.clone(), &codex, pass(), None);
    wait_until(&db, crate::common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), "wrapper_launched").is_empty()
    });
    assert_eq!(hold(&db, "2")["state"], "settling");
    release_refused(&db, TASK, &[], "hold_not_settled");
    fs::write(&gate, "").unwrap();
    done(&backend, supervisor);
    let waiting = headless_detail(&db);
    assert_eq!(waiting.runs.len(), 1);
    let run = waiting.runs[0].clone();
    assert_eq!(run.status(), RunStatus::NeedsSession);
    assert_eq!(payloads(&waiting, "hold_parked").len(), 1);
    assert_eq!(payloads(&waiting, "turn_withheld").len(), 1);
    assert!(payloads(&waiting, "turn_started").is_empty());
    assert!(stub_calls(&run).is_empty(), "{:?}", stub_calls(&run));
    let worktree = Path::new(run.worktree_path().unwrap());
    fs::write(worktree.join("mended.txt"), "mended\n").unwrap();
    git(worktree, &["add", "mended.txt"]);
    git(worktree, &["commit", "-qm", "mend while held"]);
    let head = git_out(worktree, &["rev-parse", "HEAD"]);

    let released = release(&db, "2", &[]);
    assert_eq!(
        (&released["decision"], &released["continuation"]),
        (&json!("retry_inherit"), &json!("new_run")),
        "{released}"
    );
    assert_eq!(released["inherit"]["head"], json!(head));
    let ended = headless_detail(&db);
    assert_eq!(ended.task.status(), TaskStatus::Ready);
    assert_eq!(ended.runs[0].status(), RunStatus::Failed);
    assert!(
        ended.runs[0]
            .last_error()
            .unwrap()
            .starts_with("released: ")
    );
    assert_eq!(kept(&repo, &ended.runs[0]), head);

    set_turns(
        dir.path(),
        &format!(
            r#"case "$PROMPT" in
{}
*) fail "unexpected prompt" ;;
esac"#,
            carry_over("test -f mended.txt")
        ),
    );
    let supervisor = supervise_both(&db, &repo, backend.clone(), &codex, pass(), None);
    done(&backend, supervisor);
    let detail = headless_detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(detail.runs.len(), 2, "one new run");
    assert_eq!(detail.runs[1].status(), RunStatus::Integrated);
    assert!(payloads(&detail, "resume_started").is_empty(), "no resume");
    assert_eq!(payloads(&detail, "run_inherited")[0]["head"], json!(head));
    assert_eq!(released_event(&detail)["decision"], "retry_inherit");
    assert_eq!(git_out(&repo, &["show", "main:mended.txt"]), "mended");
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

/// Acceptance (c) and (d-2): a failed run its recovery job left to a
/// person is released (its `decide` ask closed with it) to a new run that
/// carries its branch over (`run_inherited`). The release used no
/// automatic `retry_inherit`, so when that run fails too, its recovery
/// job's `retry_inherit` is applied, once.
#[test]
fn a_failed_run_released_to_a_new_run_leaves_the_recovery_jobs_retry_inherit_unused() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "commit work; exit 7");
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[escalate()]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let failed = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(failed.status(), RunStatus::Failed);
    assert_eq!(queue.unclosed_run_asks(failed.id()).unwrap().len(), 1);
    assert_eq!(hold(&db, "1")["state"], "held");
    let released = release(&db, "1", &[]);
    assert_eq!(released["decision"], "retry_inherit", "{released}");
    assert!(queue.unclosed_run_asks(failed.id()).unwrap().is_empty());

    // The next run fails too; its job's retry_inherit is applied. The run
    // after that fails with no verdict left (its job cannot start).
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[repair(
        json!({"action": "retry_inherit"}),
        "the work is done",
    )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(detail.runs.len() >= 3, "{:?}", event_kinds(&detail));
    let inherited = payloads(&detail, "run_inherited");
    assert_eq!(inherited[0]["head"], json!(kept(&repo, &failed)));
    assert_eq!(inherit_retries(&detail, &detail.runs[0])[0]["by"], "user");
    let automatic = inherit_retries(&detail, &detail.runs[1]);
    assert_eq!(automatic.len(), 1, "{:?}", event_kinds(&detail));
    assert!(resume::uses_automatic_inherit(
        detail
            .events
            .iter()
            .find(|e| e.run_id.as_ref() == Some(detail.runs[1].id())
                && e.payload["action"] == "retry_inherit")
            .unwrap()
    ));
}

/// A release whose head cannot be kept as `refs/dagq/runs/<run-id>` (a ref
/// under that name is in the way) is refused and changes nothing: the
/// hold, the failed run, its recovery job's ask and the task stay as they
/// were, and the worktree keeps its uncommitted work. Once the ref can be
/// written, the release keeps the saved work on top of the run's commits
/// under it.
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
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Ready);
    assert_eq!(inherit_retries(&detail, &failed).len(), 1);
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
    let (_dir, repo, db, _backend, run, _main) = held_parked(500, 500);
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
    let (_dir, repo, db, _backend, run, _main) = held_parked(500, 500);
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

/// Acceptance (d-1): after the recovery job's automatic `retry_inherit`, a
/// release still carries the next failed run over, but gives the used
/// retry back: when the run after it fails, its job's `retry_inherit` is
/// refused (once per task) and goes to a person.
#[test]
fn a_release_after_the_automatic_retry_inherit_leaves_it_used() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "commit work; exit 7");
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[
        repair(json!({"action": "retry_inherit"}), "the work is done"),
        escalate(),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.runs.len(), 2, "{:?}", event_kinds(&detail));
    assert_eq!(detail.runs[1].status(), RunStatus::Failed);
    assert_eq!(hold(&db, "1")["state"], "held");
    let released = release(&db, "1", &[]);
    assert_eq!(released["decision"], "retry_inherit", "{released}");

    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[repair(
        json!({"action": "retry_inherit"}),
        "the work is done",
    )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.runs.len(), 3, "{:?}", event_kinds(&detail));
    assert_eq!(inherit_retries(&detail, &detail.runs[1])[0]["by"], "user");
    let third = &detail.runs[2];
    assert_eq!(third.status(), RunStatus::Failed);
    assert!(inherit_retries(&detail, third).is_empty());
    let refused = detail
        .events
        .iter()
        .rfind(|e| e.kind == EventKind::TriageFinished && e.run_id.as_ref() == Some(third.id()))
        .unwrap();
    assert_eq!(refused.payload["action"], "ask", "{}", refused.payload);
    assert!(
        refused.payload["reason"]
            .as_str()
            .unwrap()
            .contains("once per task"),
        "{}",
        refused.payload
    );
    assert_eq!(
        detail
            .events
            .iter()
            .filter(|e| resume::uses_automatic_inherit(e))
            .count(),
        1
    );
}

/// Acceptance (e): a run with no commit of its own and nothing
/// uncommitted is released to a new run from the current main, which
/// inherits nothing, though an earlier run of the task was carried over:
/// the fresh release bounds the search for a carried-over run.
#[test]
fn a_fresh_release_after_an_earlier_carry_over_starts_from_main_with_nothing_inherited() {
    let (dir, repo, db) = fixture();
    let (first, second) = (dir.path().join("first"), dir.path().join("second"));
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "if [ ! -f {first} ]; then : > {first}; commit work; exit 7; fi; if [ ! -f {second} ]; then : > {second}; exit 7; fi; {VALID_AGENT}",
            first = shell_path(&first),
            second = shell_path(&second)
        ),
    );
    // Run 1 commits and fails, and its job carries it over; run 2 fails
    // before it commits anything, and its job leaves it to a person.
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[
        repair(json!({"action": "retry_inherit"}), "the work is done"),
        escalate(),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.runs.len(), 2, "{:?}", event_kinds(&detail));
    assert_eq!(payloads(&detail, "run_inherited").len(), 1);
    let worktree = Path::new(detail.runs[1].worktree_path().unwrap());
    assert_eq!(git_out(worktree, &["status", "--porcelain"]), "");
    assert_eq!(hold(&db, "1")["state"], "held");
    let released = release(&db, "1", &[]);
    assert_eq!(released["decision"], "retry_fresh", "{released}");
    assert!(released.get("inherit").is_none(), "{released}");

    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(detail.runs.len(), 3);
    let third = &detail.runs[2];
    assert!(
        !detail
            .events
            .iter()
            .any(|e| e.kind == EventKind::RunInherited && e.run_id.as_ref() == Some(third.id())),
        "{:?}",
        event_kinds(&detail)
    );
    let prompt =
        fs::read_to_string(Path::new(third.run_dir().unwrap()).join("prompt.txt")).unwrap();
    assert!(!prompt.contains("Carried over from run"), "{prompt}");
}

/// A run a release ended for a new one is no failure toward the recovery
/// job's retry limit (`TRIAGE_RETRY_FAILURES`): when the next run fails,
/// its job's `retry` is applied, though the task then has two failed runs.
#[test]
fn a_run_a_release_ended_is_no_failure_toward_the_recovery_jobs_retry() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "exit 7");
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[escalate()]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(hold(&db, "1")["state"], "held");
    assert_eq!(release(&db, "1", &[])["decision"], "retry_fresh");

    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")])
        .with_triages(&[repair(json!({"action": "retry"}), "try once more")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert!(detail.runs.len() >= 3, "{:?}", event_kinds(&detail));
    let second = &detail.runs[1];
    let finished = detail
        .events
        .iter()
        .find(|e| e.kind == EventKind::TriageFinished && e.run_id.as_ref() == Some(second.id()))
        .unwrap();
    assert_eq!(finished.payload["action"], "retry", "{}", finished.payload);
}

/// Where a test holds task 2's Claude run: in its first turn, at its
/// validation's end, or while its review runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Turn,
    Validation,
    Review,
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
/// `go`, commits `first part` when `commits`, leaves an unstaged change of
/// `seed.txt` and an untracked `notes.txt`, and writes a receipt when the
/// test holds the run after its turn. A run that carries it over (Codex's)
/// finds all that work ([`carry_over`]).
fn turns(dir: &Path, commits: bool, receipt: bool) {
    let commit = if commits {
        r#"commit "first part";"#
    } else {
        ""
    };
    let receipt = if receipt {
        r#"git add -A && git commit -qm everything && receipt "$(git rev-parse HEAD)";"#
    } else {
        ""
    };
    set_turns(
        dir,
        &format!(
            r#"case "$PROMPT" in
{carried}
*'dagq task 2,'*) await_file "$RUN_DIR/go"; {commit} printf 'half\n' >> seed.txt; printf 'notes\n' > notes.txt; {receipt} say paused ;;
*) fail "unexpected prompt" ;;
esac"#,
            carried = carry_over("test -f notes.txt && grep -q half seed.txt")
        ),
    );
}

/// Supervise with one slot, the stub `codex` too, `reviewer` and `pauses`,
/// on a thread, the `dagq` it starts owned by the test
/// ([`crate::common::service::owned_executable`], in the queue's directory).
fn supervise_both(
    db: &Path,
    repo: &Path,
    backend: Arc<TestWorkspace>,
    codex: &Path,
    reviewer: TestReviewer,
    pauses: Option<runtime::PausesPort>,
) -> thread::JoinHandle<Result<Value>> {
    let options = SuperviseOptions {
        codex: codex.to_owned(),
        pauses,
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

/// Acceptance (b), (e) and (f): task 2's Claude run is held while `step`
/// goes. A release then, `--provider codex --model M` among them, is
/// refused (`hold_not_settled`) and changes nothing: the step goes on to
/// its end (no preemption) and the run waits with its session, its branch
/// and its uncommitted work. Released then with `--provider codex`, the run
/// ends and a new run on Codex carries its commits (`commits`) and its
/// uncommitted work, saved as a commit, over and lands once: no resume,
/// and one new run.
fn released_to_codex_after(step: Step, commits: bool) {
    let (dir, repo, db, mut backend) = headless_fixture(&[]);
    let codex = headless_codex(dir.path(), &db);
    backend.codex = Some(codex.clone());
    let backend = Arc::new(backend);
    turns(dir.path(), commits, step != Step::Turn);
    let (paused, go) = (dir.path().join("paused"), dir.path().join("step-go"));
    let pauses = (step == Step::Validation).then(|| {
        pause_once(
            runtime::PausePoint::ValidationEnd,
            paused.clone(),
            go.clone(),
        )
    });
    let review = match step {
        Step::Review => format!(
            ": > {}; {}; {}",
            shell_path(&paused),
            crate::common::await_path(&go),
            verdict("pass", &[], "fine")
        ),
        Step::Turn | Step::Validation => verdict("pass", &[], "fine"),
    };
    let supervisor = supervise_both(
        &db,
        &repo,
        backend.clone(),
        &codex,
        TestReviewer::new(&[review]),
        pauses,
    );
    wait_until(&db, crate::common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), "turn_started").is_empty()
    });
    let run = headless_detail(&db).runs[0].clone();
    let run_dir = Path::new(run.run_dir().unwrap()).to_owned();
    if step != Step::Turn {
        fs::write(run_dir.join("go"), "").unwrap();
        wait_for_file(&paused);
    }
    // Held while the step goes: the release comes first and is refused.
    assert_eq!(hold(&db, "2")["state"], "settling");
    release_refused(
        &db,
        TASK,
        &["--provider", "codex", "--model", worker_model::SONNET],
        "hold_not_settled",
    );
    release_refused(&db, TASK, &["--provider", "codex"], "hold_not_settled");
    fs::write(
        if step == Step::Turn {
            run_dir.join("go")
        } else {
            go
        },
        "",
    )
    .unwrap();
    done(&backend, supervisor);
    // The step went to its end and the run waits with its work.
    let waiting = headless_detail(&db);
    assert_eq!(waiting.runs.len(), 1);
    assert_eq!(waiting.runs[0].status(), RunStatus::NeedsSession);
    assert_eq!(payloads(&waiting, "hold_parked").len(), 1);
    match step {
        Step::Turn => assert_eq!(payloads(&waiting, "turn_finished").len(), 1),
        Step::Validation => assert_eq!(payloads(&waiting, "validation_finished").len(), 1),
        Step::Review => assert_eq!(payloads(&waiting, "review_finished").len(), 1),
    }
    let worktree = Path::new(run.worktree_path().unwrap());
    if step == Step::Turn {
        let status = git_out(worktree, &["status", "--porcelain"]);
        for line in ["M seed.txt", "?? notes.txt"] {
            assert!(status.contains(line), "{line} in {status}");
        }
    }
    let head = git_out(worktree, &["rev-parse", "HEAD"]);
    assert_eq!(waiting.task.worker().provider, Provider::Claude);

    // The wait came first: the release passes and ends the run.
    let released = release(&db, "2", &["--provider", "codex"]);
    let decision = if commits || step != Step::Turn {
        "retry_inherit"
    } else {
        "retry_fresh"
    };
    assert_eq!(released["decision"], decision, "{released}");
    assert_eq!(released["provider"], "codex");
    let carried = released["inherit"]["head"].as_str().unwrap().to_owned();
    assert_eq!(kept(&repo, &run), carried);
    if step == Step::Turn {
        // The uncommitted work is saved on top of the head, the worktree
        // left as it was.
        assert_eq!(released["inherit"]["saved"], json!(carried));
        assert_eq!(git_out(&repo, &["rev-parse", &format!("{carried}^")]), head);
        let saved = git_out(&repo, &["ls-tree", "--name-only", &carried]);
        assert!(saved.contains("notes.txt"), "{saved}");
        assert!(git_out(worktree, &["status", "--porcelain"]).contains("?? notes.txt"));
    } else {
        assert_eq!(carried, head);
    }
    let detail = headless_detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Ready);
    assert_eq!(detail.task.worker().provider, Provider::Codex);

    let supervisor = supervise_both(
        &db,
        &repo,
        backend.clone(),
        &codex,
        TestReviewer::new(&[verdict("pass", &[], "fine")]),
        None,
    );
    done(&backend, supervisor);
    let detail = headless_detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(detail.runs.len(), 2, "one new run");
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert_eq!(detail.runs[1].actual_provider(), Provider::Codex);
    assert_eq!(detail.runs[1].status(), RunStatus::Integrated);
    assert!(payloads(&detail, "resume_started").is_empty(), "no resume");
    assert_eq!(payloads(&detail, "run_claimed").len(), 2);
    assert_eq!(payloads(&detail, "run_inherited").len(), 1);
    assert_eq!(released_event(&detail)["decision"], decision);
    let landed = git_out(&repo, &["ls-tree", "--name-only", "main"]);
    assert!(landed.contains("notes.txt"), "{landed}");
    assert!(git_out(&repo, &["show", "main:seed.txt"]).contains("half"));
}

#[test]
fn a_release_racing_a_turn_is_refused_until_the_wait_and_then_moves_the_work_to_codex() {
    released_to_codex_after(Step::Turn, true);
}

#[test]
fn a_release_racing_a_validation_is_refused_until_the_wait_and_then_moves_the_work_to_codex() {
    released_to_codex_after(Step::Validation, true);
}

#[test]
fn a_release_racing_a_review_is_refused_until_the_wait_and_then_moves_the_work_to_codex() {
    released_to_codex_after(Step::Review, true);
}

/// Acceptance (e): a run with no commit of its own, released to a new run,
/// starts from the current main with its uncommitted work saved and
/// carried over to the next run, which lands it.
#[test]
fn a_run_with_no_commit_of_its_own_is_released_to_a_new_run_with_its_uncommitted_work() {
    released_to_codex_after(Step::Turn, false);
}
