//! Runtime tests: a held task's run at its turn's boundary (ADR-t1879-1
//! decisions 2 and 7). The hold goes through the replanning's park: the
//! step that goes when the task is held (a turn, a validation, a review)
//! ends, nothing after it starts, the wrapper ends between turns, and the
//! run waits for a session with its session and work in place, out of the
//! slots, until a person releases the hold. A release while the step still
//! goes is refused and changes nothing; once the run waits it passes, and
//! the run resumes its session where it stopped.
use crate::common;
use crate::common::cli::invoke_with;
use crate::runtime_support;

use dagq::domain::task_replan::{NewReplan, ReplanMove, ReplanOrigin, ReplanStatus};
use dagq::domain::worker::WorkerMode;
use runtime_support::headless::*;
use runtime_support::*;

/// The turns of task 2: its first turn waits for the test's `go`, then
/// leaves a commit, an unstaged change and an untracked file and runs
/// `first` (by default no receipt); a session resumed after its hold, or
/// after a replanning's park, checks that the work is still there, commits
/// what is not committed yet and finishes. Task 3's turns finish at once.
fn turns(dir: &Path, first: &str) {
    turns_with(dir, first, "", "fail \"no revise was asked for\"")
}

/// [`turns`] whose resumed turn runs `resumed` first and whose revise turn
/// runs `revised`.
fn turns_with(dir: &Path, first: &str, resumed: &str, revised: &str) {
    set_turns(
        dir,
        &format!(
            r#"case "$PROMPT" in
*'asks for changes'*) {revised} ;;
*'while a person held its task'*|*'considered replanning'*) {resumed} test -f notes.txt && grep -q half seed.txt || fail "the work is not here"; git add -A; git diff --cached --quiet || git commit -qm "the rest"; receipt "$(git rev-parse HEAD)"; say finished ;;
*'dagq task 2,'*) await_file "$RUN_DIR/go"; commit "first part"; printf 'half\n' >> seed.txt; printf 'notes\n' > notes.txt; {first} say paused ;;
*) printf 'other\n' > other.txt && git add other.txt && git commit -qm other && receipt "$(git rev-parse HEAD)"; say finished ;;
esac"#
        ),
    );
}

/// A first turn that commits all its work and writes its receipt.
const RECEIPT: &str =
    r#"git add -A && git commit -qm everything && receipt "$(git rev-parse HEAD)";"#;

/// Add a ready task for a headless Claude worker, titled `title`.
fn add_headless_task(db: &Path, title: &str) -> TaskId {
    let mut queue = SqliteQueue::open(db).unwrap();
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: Some(dagq::domain::Provider::Claude),
            worker_mode: Some(WorkerMode::Headless),
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    task.id()
}

/// `dagq` as a person at a plain terminal.
fn as_user(db: &Path, args: &[&str]) -> std::process::Output {
    invoke_with(&[], db, args)
}

/// Hold task 2 as a person; the hold as recorded.
fn hold(db: &Path) -> Value {
    let output = as_user(db, &["hold", "2", "--reason", "the login expired"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Release task 2 as a person; the release as recorded.
fn release(db: &Path) -> Value {
    let output = as_user(db, &["release", "2"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// A release of task 2 is refused while its run settles, and leaves the
/// hold as it was: open, settling, its reason kept.
fn release_refused(db: &Path) {
    let output = as_user(db, &["release", "2"]);
    assert!(
        !output.status.success(),
        "the release passed while settling"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("hold_not_settled"), "{stderr}");
    let open = open_hold(db).expect("the hold stays");
    assert_eq!(open.settled_at, None);
    assert_eq!(open.reason, "the login expired");
}

fn open_hold(db: &Path) -> Option<dagq::domain::task_hold::TaskHold> {
    SqliteQueue::open(db).unwrap().task_hold(TASK).unwrap()
}

/// The phases task 2's run's `run_phase_changed` recorded, in order.
fn phases(detail: &dagq::domain::TaskDetail) -> Vec<String> {
    payloads(detail, "run_phase_changed")
        .iter()
        .map(|payload| payload["phase"].as_str().unwrap().to_owned())
        .collect()
}

/// The kinds that would say the wait was watched as a stall or an idle
/// session, or given to a recovery job.
fn watched(kinds: &[&str]) -> Vec<String> {
    kinds
        .iter()
        .filter(|kind| {
            ["stall_", "triage_", "recovery_", "idle_"]
                .iter()
                .any(|prefix| kind.starts_with(prefix))
        })
        .map(|kind| (*kind).to_owned())
        .collect()
}

/// Wait until task 2's run has an event of `kind`.
fn wait_for_kind(db: &Path, kind: &str) {
    wait_until(db, common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), kind).is_empty()
    });
}

/// Supervise on a thread with one slot, `reviewer` and `pauses`.
fn supervise_with_parts(
    db: &Path,
    repo: &Path,
    backend: Arc<TestWorkspace>,
    reviewer: TestReviewer,
    pauses: Option<runtime::PausesPort>,
) -> thread::JoinHandle<Result<Value>> {
    let options = SuperviseOptions {
        pauses,
        ..supervise_options(1, true)
    };
    let (db, repo) = (db.to_owned(), repo.to_owned());
    thread::spawn(move || {
        runtime::supervise_with_reviewer(
            &db,
            &repo,
            &*backend,
            &claude_stub(&db),
            &reviewer,
            Path::new(env!("CARGO_BIN_EXE_dagq")),
            &options,
        )
    })
}

/// What a run parked for its hold shows: `needs_session` with the hold's
/// reason, its lease given back, its session closed, its `held` phase entered once, its work in the worktree, and no review,
/// revise or landing begun past the hold, nor anything that watched the
/// wait.
fn assert_parked_for_the_hold(db: &Path) -> dagq::domain::TaskDetail {
    let detail = detail(db);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::NeedsSession);
    assert!(
        run.last_error().unwrap().contains("was held"),
        "{:?}",
        run.last_error()
    );
    let kinds = event_kinds(&detail);
    for kind in ["hold_parked", "lease_released", "workspace_closed"] {
        assert!(kinds.contains(&kind), "{kind} in {kinds:?}");
    }
    for kind in ["revise_requested", "integration_started"] {
        assert!(!kinds.contains(&kind), "{kind} in {kinds:?}");
    }
    // No resume but the one each release let start.
    assert_eq!(
        payloads(&detail, "resume_started").len(),
        payloads(&detail, "task_released").len(),
        "{kinds:?}"
    );
    assert_eq!(watched(&kinds), Vec::<String>::new());
    let held: Vec<String> = phases(&detail)
        .into_iter()
        .filter(|phase| phase == "held")
        .collect();
    // One wait per hold: each release closed the one before.
    assert_eq!(
        held.len(),
        payloads(&detail, "task_released").len() + 1,
        "{:?}",
        phases(&detail)
    );
    assert_eq!(phases(&detail).last().unwrap(), "held");
    let hold = open_hold(db).expect("the hold stays open");
    assert!(hold.settled_at.is_some(), "{hold:?}");
    let worktree = Path::new(run.worktree_path().unwrap());
    assert!(
        git_out(worktree, &["log", "--format=%s"]).contains("first part"),
        "the commit is kept"
    );
    detail
}

/// Leave in task 2's run's worktree, while its step goes, an unstaged
/// change of `seed.txt` and an untracked `late.txt`: uncommitted work the
/// wait must keep.
fn leave_uncommitted(run: &TaskRun) {
    let worktree = Path::new(run.worktree_path().unwrap());
    let seed = worktree.join("seed.txt");
    let mut text = fs::read_to_string(&seed).unwrap();
    text.push_str("late\n");
    fs::write(&seed, text).unwrap();
    fs::write(worktree.join("late.txt"), "late\n").unwrap();
}

/// What the wait keeps of task 2's run, before its release: the session
/// its first turn ran in (the resume goes on in it), its branch with the
/// commit that turn made, and the uncommitted `changes` (lines of `git
/// status --porcelain`) in its worktree.
fn assert_work_kept(db: &Path, changes: &[&str]) {
    let detail = detail(db);
    let run = &detail.runs[0];
    let worktree = Path::new(run.worktree_path().unwrap());
    assert!(
        git_out(worktree, &["log", "--format=%s", run.branch().unwrap()]).contains("first part"),
        "the branch keeps the commit"
    );
    let status = git_out(worktree, &["status", "--porcelain"]);
    for line in changes {
        assert!(status.contains(line), "{line} in {status}");
    }
    assert!(
        stub_calls(run)
            .iter()
            .any(|call| call.starts_with("start ")),
        "the session was opened"
    );
}

/// What a release of task 2's run leads to once a supervisor runs: one
/// resume of the session the run's first turn opened (no other run, no
/// second resume), whose work, the changes the wait kept included (the
/// untracked `file` and the line `seed` of `seed.txt`), lands once on
/// `repo`'s main.
fn assert_resumed_once_and_landed(db: &Path, repo: &Path, file: &str, seed: &str) {
    let detail = detail(db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(detail.runs.len(), 1, "no new run");
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Integrated);
    assert_eq!(payloads(&detail, "run_claimed").len(), 1);
    assert_eq!(
        payloads(&detail, "resume_started").len(),
        payloads(&detail, "task_released").len(),
        "one resume per release"
    );
    assert_eq!(payloads(&detail, "integration_started").len(), 1);
    assert_eq!(payloads(&detail, "run_integrated").len(), 1);
    let calls = stub_calls(run);
    let session = |call: &str| call.split(' ').nth(1).unwrap().to_owned();
    let started = calls.iter().find(|c| c.starts_with("start ")).unwrap();
    let resumed = calls.iter().rfind(|c| c.starts_with("resume ")).unwrap();
    assert_eq!(session(started), session(resumed), "{calls:?}");
    let landed = git_out(repo, &["ls-tree", "--name-only", "main"]);
    assert!(landed.contains(file), "{file} in {landed}");
    assert!(
        git_out(repo, &["show", "main:seed.txt"]).contains(seed),
        "the unstaged change landed"
    );
}

/// Acceptance: a hold while a turn runs lets the turn end and sends no
/// next turn: a release before the turn ends is refused with
/// `hold_not_settled` and leaves the hold; the wrapper ends between turns;
/// the run is not failed but waits for a session with its session, its
/// commit and its uncommitted change in place, and gives its slot to
/// another task; its `held` phase is recorded when it began to wait, after
/// the turn ended, not when it was held. A supervisor opened again keeps it
/// waiting, with no stall, idle session or recovery job; once released,
/// the run resumes the same session where it stopped and lands.
#[test]
fn a_hold_during_a_turn_lets_it_end_and_the_run_waits_with_its_session_until_the_release() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    turns(dir.path(), "");
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    wait_for_kind(&db, "turn_started");
    let other = add_headless_task(&db, "other task");
    let held = hold(&db);
    assert_eq!(held["state"], "settling", "{held}");
    release_refused(&db);
    let run = detail(&db).runs[0].clone();
    fs::write(Path::new(run.run_dir().unwrap()).join("go"), "").unwrap();
    finished(&db, &backend, supervisor);
    assert_eq!(
        SqliteQueue::open(&db)
            .unwrap()
            .show(other)
            .unwrap()
            .task
            .status(),
        TaskStatus::Completed,
        "the slot was given back"
    );
    let detail = assert_parked_for_the_hold(&db);
    let kinds = event_kinds(&detail);
    assert!(kinds.contains(&"turn_withheld"), "{kinds:?}");
    assert_eq!(payloads(&detail, "turn_started").len(), 1, "no next turn");
    // The session the wrapper ended at the fence is closed by the park.
    assert_eq!(payloads(&detail, "workspace_closed")[0]["by"], "hold");
    let worktree = Path::new(detail.runs[0].worktree_path().unwrap());
    let status = git_out(worktree, &["status", "--porcelain"]);
    for line in ["M seed.txt", "?? notes.txt"] {
        assert!(status.contains(line), "{line} in {status}");
    }
    // The wait began at the turn's end, not at the hold.
    let at = |kind: &str, phase: Option<&str>| {
        detail
            .events
            .iter()
            .position(|e| e.kind.as_str() == kind && phase.is_none_or(|p| e.payload["phase"] == p))
            .unwrap_or_else(|| panic!("no {kind} {phase:?}"))
    };
    assert!(at("task_held", None) < at("turn_finished", None));
    assert!(at("turn_finished", None) < at("run_phase_changed", Some("held")));
    let parked = payloads(&detail, "hold_parked")[0].clone();
    assert_eq!(parked["code"], "held");
    assert_eq!(parked["hold_id"], held["hold"]["id"]);

    // Opened again, the supervisor leaves the run waiting.
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    finished(&db, &backend, supervisor);
    assert_parked_for_the_hold(&db);

    let released = release(&db);
    assert_eq!(released["continuation"], "continue", "{released}");
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    finished(&db, &backend, supervisor);
    let detail = runtime_support::headless::detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let phases = phases(&detail);
    let held_at = phases.iter().position(|phase| phase == "held").unwrap();
    assert_eq!(phases[held_at + 1], "needs_session", "{phases:?}");
    let run = &detail.runs[0];
    let request =
        fs::read_to_string(Path::new(run.run_dir().unwrap()).join("resume-1.txt")).unwrap();
    assert!(
        request.contains("while a person held its task"),
        "{request}"
    );
    // The resumed turn is the same session's, and the run lands once.
    assert_resumed_once_and_landed(&db, &repo, "notes.txt", "half");
}

/// Boundary: a hold while a review runs lets the review end (its verdict
/// recorded) and lands nothing; a release while it runs is refused, and
/// once the run waits, its session, branch and uncommitted changes kept,
/// the release passes, after which the run resumes its session once, its
/// work is reviewed afresh and lands once.
#[test]
fn a_review_running_at_the_hold_ends_and_the_run_waits_without_landing() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    turns(dir.path(), RECEIPT);
    let backend = Arc::new(backend);
    let go = dir.path().join("review-go");
    let reviewer = TestReviewer::new(&[format!(
        "{}; {}",
        common::await_path(&go),
        verdict("pass", &[], "fine")
    )]);
    let supervisor = supervise_with_parts(&db, &repo, backend.clone(), reviewer, None);
    wait_for_kind(&db, "turn_started");
    let run = detail(&db).runs[0].clone();
    fs::write(Path::new(run.run_dir().unwrap()).join("go"), "").unwrap();
    wait_for_kind(&db, "review_started");
    assert_eq!(hold(&db)["state"], "settling");
    release_refused(&db);
    leave_uncommitted(&run);
    fs::write(&go, "").unwrap();
    finished(&db, &backend, supervisor);
    let detail = assert_parked_for_the_hold(&db);
    assert_eq!(payloads(&detail, "review_finished").len(), 1);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert_work_kept(&db, &["M seed.txt", "?? late.txt"]);
    assert_eq!(release(&db)["continuation"], "continue");
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    finished(&db, &backend, supervisor);
    let detail = runtime_support::headless::detail(&db);
    assert_eq!(
        payloads(&detail, "review_finished").len(),
        2,
        "reviewed afresh"
    );
    assert_resumed_once_and_landed(&db, &repo, "late.txt", "late");
}

/// Boundary: a hold that reaches a receipt's validation as it ends lets
/// the validation's result be acted on, starts no review and lands
/// nothing; a release before the run waits is refused, and once it waits,
/// its session, branch and uncommitted changes kept, the release passes,
/// after which the run resumes its session once and lands once.
#[test]
fn a_validation_ending_at_the_hold_is_acted_on_and_starts_no_review() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    turns(dir.path(), RECEIPT);
    let backend = Arc::new(backend);
    let (paused, go) = (dir.path().join("paused"), dir.path().join("validated"));
    let pauses = pause_once(
        runtime::PausePoint::ValidationEnd,
        paused.clone(),
        go.clone(),
    );
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
    let supervisor = supervise_with_parts(&db, &repo, backend.clone(), reviewer, Some(pauses));
    wait_for_kind(&db, "turn_started");
    let run = detail(&db).runs[0].clone();
    fs::write(Path::new(run.run_dir().unwrap()).join("go"), "").unwrap();
    wait_for_file(&paused);
    assert_eq!(hold(&db)["state"], "settling");
    release_refused(&db);
    leave_uncommitted(&run);
    fs::write(&go, "").unwrap();
    finished(&db, &backend, supervisor);
    let detail = assert_parked_for_the_hold(&db);
    assert_eq!(payloads(&detail, "validation_finished").len(), 1);
    assert!(payloads(&detail, "review_started").is_empty());
    assert_work_kept(&db, &["M seed.txt", "?? late.txt"]);
    assert_eq!(release(&db)["continuation"], "continue");
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    finished(&db, &backend, supervisor);
    assert_resumed_once_and_landed(&db, &repo, "late.txt", "late");
}

/// Acceptance: a replanning and a hold on the same run each keep it
/// waiting: the replanning's park saves the run's work and the hold settles
/// as the run rests; the replanning's continuation does not resume it while
/// the hold is open, and once the hold is released it resumes and lands.
#[test]
fn a_replanning_and_a_hold_each_keep_the_run_waiting_until_both_end() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    turns(dir.path(), "");
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    wait_for_kind(&db, "turn_started");
    let run = detail(&db).runs[0].clone();
    hold(&db);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let op = queue
        .record_replan(
            &NewReplan {
                run: run.id().clone(),
                reason: "two independent parts".into(),
                evidence: Vec::new(),
            },
            ReplanOrigin::Runtime,
            "supervisor",
            "supervisor",
        )
        .unwrap()
        .operation;
    let mv = |op: &dagq::domain::task_replan::ReplanOperation, to: ReplanStatus| {
        SqliteQueue::open(&db)
            .unwrap()
            .move_replan(
                op.id,
                op.generation,
                &ReplanMove {
                    to,
                    reason: format!("the test moves it to {}", to.as_str()),
                    snapshot_id: None,
                    diagnosis: None,
                },
            )
            .unwrap()
    };
    mv(&op, ReplanStatus::PauseRequested);
    fs::write(Path::new(run.run_dir().unwrap()).join("go"), "").unwrap();
    finished(&db, &backend, supervisor);
    let parked = queue.replan_operation(op.id).unwrap();
    assert_eq!(parked.status, ReplanStatus::Parked, "{parked:?}");
    let detail = runtime_support::headless::detail(&db);
    assert!(event_kinds(&detail).contains(&"replan_parked"));
    assert!(!event_kinds(&detail).contains(&"hold_parked"));
    assert!(open_hold(&db).unwrap().settled_at.is_some());
    assert_eq!(phases(&detail).last().unwrap(), "held");

    // The replanning ends: the hold still keeps the run waiting.
    mv(&parked, ReplanStatus::ContinuingAfterPark);
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    finished(&db, &backend, supervisor);
    let detail = runtime_support::headless::detail(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    assert!(payloads(&detail, "resume_started").is_empty());

    release(&db);
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    finished(&db, &backend, supervisor);
    let detail = runtime_support::headless::detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(payloads(&detail, "replan_restored").len(), 1);
}

/// The run of task 2 parked for its hold during its first turn, which left
/// its work without a receipt: the run waits, its hold settled.
fn parked_in_its_first_turn(db: &Path, repo: &Path, backend: &Arc<TestWorkspace>) -> TaskRun {
    let (_reviewer, supervisor) =
        supervise_thread_in(db, repo, backend.clone(), Default::default(), &[], 1);
    wait_for_kind(db, "turn_started");
    hold(db);
    let run = detail(db).runs[0].clone();
    fs::write(Path::new(run.run_dir().unwrap()).join("go"), "").unwrap();
    finished(db, backend, supervisor);
    assert_parked_for_the_hold(db);
    run
}

/// Boundary: a hold during a resumed session's turn lets the turn end, and
/// the receipt it rewrote is judged and validated as before; no review
/// starts, the run waits again, and a release before that is refused.
#[test]
fn a_receipt_a_resumed_turn_rewrote_at_the_hold_is_validated_and_starts_no_review() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    turns_with(dir.path(), "", r#"await_file "$RUN_DIR/go2";"#, "");
    let backend = Arc::new(backend);
    let run = parked_in_its_first_turn(&db, &repo, &backend);
    release(&db);
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        payloads(&queue.show(TASK).unwrap(), "turn_started").len() == 2
    });
    assert_eq!(hold(&db)["state"], "settling");
    release_refused(&db);
    fs::write(Path::new(run.run_dir().unwrap()).join("go2"), "").unwrap();
    finished(&db, &backend, supervisor);
    let detail = assert_parked_for_the_hold(&db);
    assert_eq!(payloads(&detail, "hold_parked").len(), 2);
    assert_eq!(payloads(&detail, "resume_finished").len(), 1);
    assert_eq!(payloads(&detail, "validation_finished").len(), 1);
    assert!(payloads(&detail, "review_started").is_empty());
}

/// Boundary: a hold during a revise's turn lets the turn end, and the
/// receipt it rewrote is judged and validated as before; the next review
/// does not start, the run waits, and a release before that is refused.
#[test]
fn a_receipt_a_revise_rewrote_at_the_hold_is_validated_and_starts_no_review() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    turns_with(
        dir.path(),
        RECEIPT,
        "",
        r#"await_file "$RUN_DIR/go2"; printf 'fixed\n' > fixed.txt && git add fixed.txt && git commit -qm fixed && receipt "$(git rev-parse HEAD)"; say fixed"#,
    );
    let backend = Arc::new(backend);
    let reviewer = TestReviewer::new(&[
        verdict("revise", &["add fixed.txt"], "needs a fix"),
        verdict("pass", &[], "fine"),
    ]);
    let supervisor = supervise_with_parts(&db, &repo, backend.clone(), reviewer, None);
    wait_for_kind(&db, "turn_started");
    let run = detail(&db).runs[0].clone();
    fs::write(Path::new(run.run_dir().unwrap()).join("go"), "").unwrap();
    // The revise's turn runs.
    wait_until(&db, common::STEP_LIMIT, |queue| {
        payloads(&queue.show(TASK).unwrap(), "turn_started").len() == 2
    });
    assert_eq!(hold(&db)["state"], "settling");
    release_refused(&db);
    fs::write(Path::new(run.run_dir().unwrap()).join("go2"), "").unwrap();
    finished(&db, &backend, supervisor);
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_eq!(
        run.status(),
        RunStatus::NeedsSession,
        "{:?}",
        event_kinds(&detail)
    );
    let kinds = event_kinds(&detail);
    for kind in ["revise_finished", "hold_parked"] {
        assert!(kinds.contains(&kind), "{kind} in {kinds:?}");
    }
    assert_eq!(payloads(&detail, "validation_finished").len(), 2);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert!(!kinds.contains(&"integration_started"), "{kinds:?}");
    assert_eq!(watched(&kinds), Vec::<String>::new());
    assert!(open_hold(&db).unwrap().settled_at.is_some());
    assert_eq!(phases(&detail).last().unwrap(), "held");
}

/// Stops the helper whose pid is in the file by its pid when the test
/// ends, passed or not.
struct Helper(PathBuf);
impl Drop for Helper {
    fn drop(&mut self) {
        if let Ok(pid) = fs::read_to_string(&self.0) {
            let _ = common::Bounded::bounded_status(
                std::process::Command::new("kill").args(["-9", pid.trim()]),
            );
        }
    }
}

/// A turn's part that leaves a helper running in the worktree in its own
/// process group, its pid in `$RUN_DIR/helper.pid`. The helper ends at the
/// test's `$RUN_DIR/stop`, once this test process is gone or its run
/// directory is, and after two minutes at most.
fn helper_script() -> String {
    format!(
        r#"printf '%s\n' 'i=0; while [ $i -lt 1200 ] && [ ! -f "$1/stop" ] && kill -0 {test} 2>/dev/null && [ -d "$1" ]; do sleep 0.1; i=$((i+1)); done' > "$RUN_DIR/helper.sh"; perl -e 'setpgrp(0, 0) or die "setpgrp: $!"; open(my $p, ">", "$ARGV[0]/helper.tmp") or die $!; print $p "$$\n"; close($p) or die $!; rename "$ARGV[0]/helper.tmp", "$ARGV[0]/helper.pid" or die $!; exec "sh", "$ARGV[0]/helper.sh", "$ARGV[0]"' "$RUN_DIR" >/dev/null 2>&1 & await_file "$RUN_DIR/helper.pid";"#,
        test = std::process::id()
    )
}

/// Boundary: a process a turn left running in the worktree keeps the hold
/// from settling: once the turn ended and its session closed, the run keeps
/// its slot and a release is refused while the helper runs; the run waits
/// only once it ended.
#[test]
fn a_helper_left_running_in_the_worktree_keeps_the_hold_settling() {
    let helper = helper_script();
    let (dir, repo, db, backend) = headless_fixture(&[]);
    turns(dir.path(), &helper);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    wait_for_kind(&db, "turn_started");
    hold(&db);
    let run = detail(&db).runs[0].clone();
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let _helper = Helper(run_dir.join("helper.pid"));
    fs::write(run_dir.join("go"), "").unwrap();
    // The session the wrapper ended at the fence is closed; the helper
    // still runs.
    wait_for_kind(&db, "workspace_closed");
    for _ in 0..3 {
        release_refused(&db);
        let detail = detail(&db);
        assert_eq!(detail.runs[0].status(), RunStatus::Running);
        assert!(!event_kinds(&detail).contains(&"hold_parked"));
        thread::sleep(Duration::from_millis(300));
    }
    fs::write(run_dir.join("stop"), "").unwrap();
    finished(&db, &backend, supervisor);
    assert_parked_for_the_hold(&db);
    assert_eq!(release(&db)["continuation"], "continue");
}

/// Boundary: a hold during a resumed turn whose supervisor dies before the
/// turn ends (it stands still at the park's first look, and its lease goes
/// stale as a killed process's would). The wrapper ends the turn and then
/// itself at the fence, unwatched; no adopter takes a resume whose session
/// ended, and the hold refuses the resume. Another supervisor takes the run
/// over for the park (`lease_acquired`, `hold_park`), closes the ended
/// session and parks it, so the hold settles and its release passes; the
/// supervisor that stood still stops and records nothing of the park when
/// it comes back, only that it lost the run's lease. Once released, the
/// run resumes its session and lands once.
#[test]
fn a_hold_whose_resuming_supervisor_died_is_settled_by_another_supervisor() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    turns_with(dir.path(), "", r#"await_file "$RUN_DIR/go2";"#, "");
    let backend = Arc::new(backend);
    let run = parked_in_its_first_turn(&db, &repo, &backend);
    release(&db);
    let (paused, go) = (dir.path().join("paused"), dir.path().join("back"));
    let pauses = pause_once(runtime::PausePoint::ParkLook, paused.clone(), go.clone());
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
    let stood_still = supervise_with_parts(&db, &repo, backend.clone(), reviewer, Some(pauses));
    wait_until(&db, common::STEP_LIMIT, |queue| {
        payloads(&queue.show(TASK).unwrap(), "turn_started").len() == 2
    });
    assert_eq!(hold(&db)["state"], "settling");
    wait_for_file(&paused);
    // The supervisor stands still in the park: its lease goes stale, as a
    // killed process's.
    let moved = Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET token='dead', heartbeat_at=0, pid=?2 WHERE run_id=?1",
            rusqlite::params![run.id(), dead_pid()],
        )
        .unwrap();
    assert_eq!(moved, 1);
    let run_dir = Path::new(run.run_dir().unwrap());
    fs::write(run_dir.join("go2"), "").unwrap();
    // The turn ends and the wrapper ends itself at the fence, unwatched.
    wait_until(&db, common::STEP_LIMIT, |queue| {
        payloads(&queue.show(TASK).unwrap(), "turn_withheld").len() == 2
    });
    release_refused(&db);
    let (_reviewer, other) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    finished(&db, &backend, other);
    let detail = detail(&db);
    let took_over: Vec<&Value> = payloads(&detail, "lease_acquired")
        .into_iter()
        .filter(|payload| payload["reason"] == "hold_park")
        .collect();
    assert_eq!(took_over.len(), 1, "{took_over:?}");
    assert_eq!(took_over[0]["previous_token"], "dead");
    assert_eq!(payloads(&detail, "hold_parked").len(), 2);
    assert_eq!(
        payloads(&detail, "resume_started").len(),
        1,
        "no resume while held"
    );
    assert!(
        SqliteQueue::open(&db)
            .unwrap()
            .run_lease(run.id())
            .unwrap()
            .is_none()
    );
    // The supervisor that stood still comes back: the park is no longer
    // its own, and it stops and records nothing of it.
    let parked = detail.events.len();
    fs::write(&go, "").unwrap();
    let outcome = joined(stood_still, "the supervisor that stood still to return").unwrap();
    backend.join();
    let errors = outcome["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{outcome}");
    assert!(
        errors[0]["message"]
            .as_str()
            .unwrap()
            .contains("held by another process"),
        "{outcome}"
    );
    let detail = assert_parked_for_the_hold(&db);
    assert!(
        detail.events[parked..].iter().all(|e| ![
            "hold_parked",
            "workspace_closed",
            "resume_started"
        ]
        .contains(&e.kind.as_str())),
        "{:?}",
        &detail.events[parked..]
    );
    assert_eq!(release(&db)["continuation"], "continue");
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    finished(&db, &backend, supervisor);
    assert_resumed_once_and_landed(&db, &repo, "notes.txt", "half");
}

/// Boundary: a hold during a turn whose receipt its validation sends back
/// to a session (`evidence_missing`): the validation's result is recorded
/// and the run rests `needs_session` with that reason, but while a helper
/// the turn left runs in the worktree the exit's park keeps the lease and
/// the hold settling, and a release is refused; once the helper ended, the
/// run waits with its own reason (no `hold_parked`) and the release passes.
#[test]
fn a_helper_left_by_a_turn_its_validation_sent_back_keeps_the_hold_settling() {
    const RECEIPT_TESTS: &str = r#"receipt_tests() { printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"%s","evidence_or_reason":"%s"},"e2e":{"status":"not_applicable","evidence_or_reason":"the runtime runs the e2e"},"subagent_review":{"status":"passed","evidence_or_reason":"reviewed"},"summary":"done"}' "$DAGQ_RUN_ID" "$1" "$2" "$3" > "$RECEIPT.tmp"; mv "$RECEIPT.tmp" "$RECEIPT"; };"#;
    let (dir, repo, db, backend) = headless_fixture(&[EvidenceCheck::Tests]);
    turns(
        dir.path(),
        &format!(
            r#"{} {RECEIPT_TESTS} git add -A && git commit -qm everything && receipt_tests "$(git rev-parse HEAD)" passed ' ';"#,
            helper_script()
        ),
    );
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    wait_for_kind(&db, "turn_started");
    assert_eq!(hold(&db)["state"], "settling");
    let run = detail(&db).runs[0].clone();
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let _helper = Helper(run_dir.join("helper.pid"));
    fs::write(run_dir.join("go"), "").unwrap();
    // The validation sent the run back, and the session it ran in is
    // closed; the helper still runs.
    wait_for_kind(&db, "evidence_missing");
    wait_for_kind(&db, "workspace_closed");
    for _ in 0..3 {
        release_refused(&db);
        let detail = detail(&db);
        assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
        assert!(
            SqliteQueue::open(&db)
                .unwrap()
                .run_lease(run.id())
                .unwrap()
                .is_some(),
            "the slot keeps the lease"
        );
        thread::sleep(Duration::from_millis(300));
    }
    fs::write(run_dir.join("stop"), "").unwrap();
    finished(&db, &backend, supervisor);
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::NeedsSession);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"hold_parked"), "{kinds:?}");
    assert!(!kinds.contains(&"review_started"), "{kinds:?}");
    assert_eq!(watched(&kinds), Vec::<String>::new());
    // The run rests with its validation's reason, which its resume after
    // the release takes up.
    let last_park = kinds
        .iter()
        .rev()
        .find(|kind| ["evidence_missing", "hold_parked"].contains(kind));
    assert_eq!(last_park, Some(&"evidence_missing"));
    assert!(open_hold(&db).unwrap().settled_at.is_some());
    assert_eq!(phases(&detail).last().unwrap(), "held");
    assert!(
        SqliteQueue::open(&db)
            .unwrap()
            .run_lease(run.id())
            .unwrap()
            .is_none()
    );
    assert_eq!(release(&db)["continuation"], "continue");
}

/// Boundary: a replanning's park that cannot stop what the turn left in
/// the worktree ends `blocked` and lets the run rest `needs_session` with
/// its lease given back. That rest is no safe wait for a hold on the same
/// task: while the helper runs the hold stays settling and its release is
/// refused, through a supervisor's pass too; once the helper ended, the
/// next pass finds no writer and settles the hold, and the release passes.
#[test]
fn a_hold_under_a_replanning_park_a_helper_blocked_settles_only_once_it_ended() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    turns(dir.path(), &helper_script());
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    wait_for_kind(&db, "turn_started");
    let run = detail(&db).runs[0].clone();
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let _helper = Helper(run_dir.join("helper.pid"));
    assert_eq!(hold(&db)["state"], "settling");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let op = queue
        .record_replan(
            &NewReplan {
                run: run.id().clone(),
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
                reason: "the test moves it to pause_requested".into(),
                snapshot_id: None,
                diagnosis: None,
            },
        )
        .unwrap();
    fs::write(run_dir.join("go"), "").unwrap();
    finished(&db, &backend, supervisor);
    let blocked = queue.replan_operation(op.id).unwrap();
    assert_eq!(blocked.status, ReplanStatus::Blocked, "{blocked:?}");
    assert_eq!(detail(&db).runs[0].status(), RunStatus::NeedsSession);
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    release_refused(&db);
    // A supervisor's pass finds the helper still running and settles
    // nothing.
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    finished(&db, &backend, supervisor);
    release_refused(&db);
    assert!(!phases(&detail(&db)).iter().any(|phase| phase == "held"));
    // The helper ends; the next pass settles the hold.
    let pid = fs::read_to_string(run_dir.join("helper.pid")).unwrap();
    fs::write(run_dir.join("stop"), "").unwrap();
    let waited = std::time::Instant::now();
    while common::Bounded::bounded_status(
        std::process::Command::new("kill").args(["-0", pid.trim()]),
    )
    .unwrap()
    .success()
    {
        assert!(
            waited.elapsed() < common::STEP_LIMIT,
            "the helper never ended"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    finished(&db, &backend, supervisor);
    assert!(open_hold(&db).unwrap().settled_at.is_some());
    assert_eq!(phases(&detail(&db)).last().unwrap(), "held");
    assert_eq!(release(&db)["continuation"], "continue");
}

/// Boundary: a hold during a resumed turn whose resume is given up while
/// the hold settles (as `give_up_resume` does on an error of its watch: the
/// lease is given back and the session, its wrapper still running the
/// turn, is kept). The lease given back settles nothing: the release is
/// refused while the turn runs. The supervisor takes the run for the
/// hold's park, waits for the wrapper to end the turn and itself at the
/// fence, and then the run waits with the hold settled; the release passes,
/// and the run resumes its session and lands once.
#[test]
fn a_hold_whose_resume_was_given_up_with_its_wrapper_running_settles_once_it_ended() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    turns_with(dir.path(), "", r#"await_file "$RUN_DIR/go2";"#, "");
    let backend = Arc::new(backend);
    let run = parked_in_its_first_turn(&db, &repo, &backend);
    release(&db);
    let (paused, go) = (dir.path().join("paused"), dir.path().join("back"));
    let pauses = pause_once(runtime::PausePoint::ParkLook, paused.clone(), go.clone());
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
    let supervisor = supervise_with_parts(&db, &repo, backend.clone(), reviewer, Some(pauses));
    wait_until(&db, common::STEP_LIMIT, |queue| {
        payloads(&queue.show(TASK).unwrap(), "turn_started").len() == 2
    });
    assert_eq!(hold(&db)["state"], "settling");
    wait_for_file(&paused);
    // The resume is given up as its watch's error would: the lease goes,
    // the session and its running wrapper stay.
    let mut queue = SqliteQueue::open(&db).unwrap();
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    queue
        .finish_resume(
            run.id(),
            &lease.token,
            None,
            None,
            false,
            json!({"attempt": 1, "outcome": "error", "error": "the test gives the resume up"}),
        )
        .unwrap();
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert!(open_hold(&db).unwrap().settled_at.is_none());
    release_refused(&db);
    // The supervisor goes on: it takes the run for the park and waits for
    // the turn, which still runs.
    fs::write(&go, "").unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        payloads(&queue.show(TASK).unwrap(), "lease_acquired")
            .iter()
            .any(|payload| payload["reason"] == "hold_park")
    });
    release_refused(&db);
    fs::write(Path::new(run.run_dir().unwrap()).join("go2"), "").unwrap();
    let outcome = joined(supervisor, "the supervisor to return").unwrap();
    backend.join();
    let errors = outcome["errors"].as_array().unwrap();
    assert!(errors.len() <= 1, "{outcome}");
    let detail = detail(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    assert!(open_hold(&db).unwrap().settled_at.is_some());
    assert_eq!(phases(&detail).last().unwrap(), "held");
    // The turn ended before the wait: its end is recorded before the hold
    // settled.
    let at = |kind: &str, phase: Option<&str>| {
        detail
            .events
            .iter()
            .rposition(|e| e.kind.as_str() == kind && phase.is_none_or(|p| e.payload["phase"] == p))
            .unwrap_or_else(|| panic!("no {kind} {phase:?}"))
    };
    assert!(at("turn_finished", None) < at("run_phase_changed", Some("held")));
    assert_eq!(
        payloads(&detail, "hold_parked").len(),
        1,
        "it rests as it is"
    );
    assert_eq!(release(&db)["continuation"], "continue");
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    finished(&db, &backend, supervisor);
    assert_resumed_once_and_landed(&db, &repo, "notes.txt", "half");
}
