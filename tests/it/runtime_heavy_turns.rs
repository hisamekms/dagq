//! Runtime tests: the automatic update's build and e2e gate and the
//! landing checks (a landing's verification, a direct `integrate`'s
//! included, and the landing recheck's command) take turns through the
//! queue's lock files (ADR-t2719-1). Who waits for whom is decided in
//! `dagq::domain::heavy_turn` and the locks are
//! `dagq::application::heavy_turn` (their unit tests); these tests keep the
//! update job's process, the stub build and e2e, the stub verification and
//! recheck commands, Git, SQLite and the supervisor's wiring.
use crate::runtime_support;
use dagq::domain::EventKind;

use runtime_support::*;

/// Make the fixture's repository dagq's source, which the automatic update
/// alone builds (ADR-t614-1), with `dagq_toml` as its dagq.toml.
fn dagq_source(repo: &Path, dagq_toml: &str) {
    fs::write(repo.join("Cargo.toml"), "[package]\nname = \"dagq\"\n").unwrap();
    fs::write(repo.join("dagq.toml"), dagq_toml).unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", "dagq's manifest"]);
}

/// A dagq.toml whose `[recheck] command` is `command`.
fn recheck_command(command: &str) -> String {
    format!("[recheck]\ncommand = {}\n", json!(command))
}

/// A shell command that appends a line to `marker`, then waits until
/// `gate` exists (and stops with the test).
fn marked_and_held(marker: &Path, gate: &Path) -> String {
    format!(
        "echo started >> {}; {}",
        shell_path(marker),
        crate::common::await_path(gate)
    )
}

/// A stub e2e that appends a line to `marker`, waits until `gate` exists,
/// then fails, so the job replaces nothing.
fn failing_e2e(marker: &Path, gate: &Path) -> String {
    format!("{}; exit 1", marked_and_held(marker, gate))
}

/// A stub build that appends a line to `marker` and leaves a binary.
fn passing_build(marker: &Path) -> String {
    format!(
        "echo built >> {}; mkdir -p \"$CARGO_TARGET_DIR/release\" && : > \"$CARGO_TARGET_DIR/release/dagq\"",
        shell_path(marker)
    )
}

/// Options of a supervisor with the automatic update on (`register`), its
/// job's build and e2e the stubs `build` and `e2e`, on the queue's service
/// ([`served`]), to which the job sends its steps as that supervisor.
fn update_options(register: bool, build: String, e2e: Option<String>) -> SuperviseOptions {
    let base = supervise_options(4, true);
    SuperviseOptions {
        update: dagq::application::supervise::UpdateSettings {
            register,
            interval: Duration::ZERO,
            build_command: Some(build),
            e2e_command: e2e,
            ..base.update.clone()
        },
        service_route: dagq::compose::ServiceRoute::Socket,
        ..base
    }
}

/// The fixture's queue with its service running until the test ends.
fn served() -> (Fixture, PathBuf, PathBuf, crate::common::service::Served) {
    let (fixture, repo, db) = fixture();
    let served = crate::common::service::Served::start(&db);
    (fixture, repo, db, served)
}

/// Cancel the fixture's task and add task 2, whose verification is
/// `verify`.
fn task_verified_by(db: &Path, verify: String) {
    let mut queue = SqliteQueue::open(db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let task = queue
        .add(NewTask {
            title: "verified by a stub".into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec![verify],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
}

fn updates(db: &Path) -> Vec<dagq::domain::RunEvent> {
    SqliteQueue::open(db).unwrap().update_events(100).unwrap()
}

/// The id of the newest automatic update's step `kind`.
fn update_step(db: &Path, kind: &str) -> Option<EventId> {
    updates(db)
        .into_iter()
        .find(|update| update.kind.as_str() == kind)
        .map(|update| update.id)
}

/// The id of the first `kind` of `run`.
fn run_step(db: &Path, run: &RunId, kind: &str) -> Option<EventId> {
    SqliteQueue::open(db)
        .unwrap()
        .run_events(run)
        .unwrap()
        .into_iter()
        .find(|event| event.kind.as_str() == kind)
        .map(|event| event.id)
}

/// Wait for `holds`, failing with the update's steps after a minute.
#[track_caller]
fn await_that(db: &Path, what: &str, mut holds: impl FnMut() -> bool) {
    let started = Instant::now();
    while !holds() {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "{what} never came: {:?}",
            updates(db)
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// The update's job failed (its stub e2e or build), which ends it.
fn await_update_failed(db: &Path) {
    await_that(db, "update_failed", || {
        update_step(db, "update_failed").is_some()
    });
}

/// The update's job ended: its log holds its outcome or its error. Its own
/// end goes unrecorded when the pass of the supervisor that started it
/// ended first: the job's steps go under that registration's token, which
/// ends with it.
fn await_job_ended(db: &Path) {
    await_that(db, "the update job's end", || {
        let log = job_log(db);
        log.contains("\"outcome\"") || log.contains("\"error\"")
    });
}

/// What the update's job logged so far: its log, named by its
/// `update_started`.
fn job_log(db: &Path) -> String {
    updates(db)
        .into_iter()
        .find(|update| update.kind == EventKind::UpdateStarted)
        .and_then(|started| started.payload["log"].as_str().map(PathBuf::from))
        .and_then(|log| fs::read_to_string(log).ok())
        .unwrap_or_default()
}

/// The supervisor's outcome once it returned.
fn returned(supervisor: thread::ScopedJoinHandle<'_, Value>) -> Value {
    let _waiting = crate::common::within(crate::common::STEP_LIMIT, "the supervisor to return");
    supervisor
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

fn first_run(queue: &mut SqliteQueue, task: TaskId) -> Option<TaskRun> {
    queue.show(task).unwrap().runs.first().cloned()
}

/// Task 2 waits for a person on a `concern`, after one supervisor's pass.
fn waiting_on_a_concern(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    options: &SuperviseOptions,
) -> (TaskRun, TestReviewer) {
    let reviewer = TestReviewer::new(&[verdict("concern", &["a person should look"], "unsure")]);
    let outcome = supervise_reviewed_with(db, repo, backend, &reviewer, options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(db).unwrap();
    let waiting = first_run(&mut queue, TaskId::new(2)).unwrap();
    assert_eq!(waiting.status(), RunStatus::AwaitingIntegration);
    (waiting, reviewer)
}

/// Move main outside dagq, in a file the waiting run does not touch.
fn move_main(repo: &Path) {
    fs::write(repo.join("other.txt"), "moved by hand\n").unwrap();
    git(repo, &["add", "other.txt"]);
    git(repo, &["commit", "-q", "-m", "by hand"]);
}

/// While the update's job runs its e2e (its stub held until the test
/// writes the gate), the run whose review passed does not start its
/// landing: no `integration_started`, its verification command not run.
/// Once the e2e ended (failed, so nothing is replaced), it lands.
#[test]
fn a_landing_waits_for_the_update_jobs_e2e_and_verifies_once_it_ended() {
    let (fixture, repo, db, _served) = served();
    dagq_source(&repo, "");
    let dir = fixture.dir.path();
    let (built, e2e, gate, verified) = (
        dir.join("built"),
        dir.join("e2e"),
        dir.join("e2e gate"),
        dir.join("verified"),
    );
    task_verified_by(&db, format!("echo verified >> {}", shell_path(&verified)));
    let options = update_options(true, passing_build(&built), Some(failing_e2e(&e2e, &gate)));
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    thread::scope(|scope| {
        let supervisor =
            scope.spawn(|| supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options));
        wait_until(&db, Duration::from_secs(60), |queue| {
            first_run(queue, TaskId::new(2))
                .is_some_and(|run| !events_of(&db, run.id(), "review_finished").is_empty())
        });
        await_that(&db, "the e2e", || e2e.exists());
        await_passes(&options.passes, SOME_PASSES);
        let mut queue = SqliteQueue::open(&db).unwrap();
        let run = first_run(&mut queue, TaskId::new(2)).unwrap();
        assert_eq!(run.status(), RunStatus::AwaitingIntegration);
        assert_eq!(run_step(&db, run.id(), "integration_started"), None);
        assert!(!verified.exists());
        assert!(update_step(&db, "update_built").is_some());
        assert!(!supervisor.is_finished());

        fs::write(&gate, "go").unwrap();
        let outcome = returned(supervisor);
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        let run = first_run(&mut queue, TaskId::new(2)).unwrap();
        assert_eq!(run.status(), RunStatus::Integrated);
        assert!(verified.exists());
        let failed = update_step(&db, "update_failed").unwrap();
        let landing = run_step(&db, run.id(), "integration_started").unwrap();
        assert!(failed < landing, "{failed:?} {landing:?}");
    });
    backend.join();
}

/// A person's `integrate` waits for the update's e2e as the supervisor's
/// landing does: its verification command runs only once the e2e ended.
#[test]
fn a_direct_integrate_waits_for_the_update_jobs_e2e() {
    let (fixture, repo, db, _served) = served();
    dagq_source(&repo, "");
    let dir = fixture.dir.path();
    let (built, e2e, gate, verified) = (
        dir.join("built"),
        dir.join("e2e"),
        dir.join("e2e gate"),
        dir.join("verified"),
    );
    task_verified_by(&db, format!("echo verified >> {}", shell_path(&verified)));
    let options = update_options(true, passing_build(&built), Some(failing_e2e(&e2e, &gate)));
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let (waiting, _reviewer) = waiting_on_a_concern(&db, &repo, &backend, &options);
    await_that(&db, "the e2e", || e2e.exists());

    thread::scope(|scope| {
        let integrate = scope
            .spawn(|| runtime::integrate(&db, IntegrateTarget::Task(TaskId::new(2)), &repo, None));
        wait_until(&db, Duration::from_secs(60), |queue| {
            queue.run(waiting.id()).unwrap().status() == RunStatus::Integrating
        });
        thread::sleep(Duration::from_millis(500));
        assert!(!verified.exists());
        assert!(!integrate.is_finished());
        fs::write(&gate, "go").unwrap();
        let _waiting = crate::common::within(crate::common::STEP_LIMIT, "integrate");
        let outcome = integrate
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            .unwrap();
        assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    });
    assert!(verified.exists());
    await_job_ended(&db);
    backend.join();
}

/// A landing recheck due (main moved while a run waits for a person) does
/// not start while the update's job runs its e2e; once the e2e ended, the
/// next supervisor rechecks the run and runs the recheck's command.
#[test]
fn a_landing_recheck_waits_for_the_update_jobs_e2e_and_runs_its_command_once_it_ended() {
    let (fixture, repo, db, _served) = served();
    let dir = fixture.dir.path();
    let (built, e2e, gate, rechecked) = (
        dir.join("built"),
        dir.join("e2e"),
        dir.join("e2e gate"),
        dir.join("rechecked"),
    );
    dagq_source(
        &repo,
        &recheck_command(&format!("echo rechecked >> {}", shell_path(&rechecked))),
    );
    task_verified_by(&db, "test -f seed.txt".into());
    // The run waits on its ask, and the job starts, its e2e held.
    let options = update_options(true, passing_build(&built), Some(failing_e2e(&e2e, &gate)));
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let (waiting, reviewer) = waiting_on_a_concern(&db, &repo, &backend, &options);
    await_that(&db, "the e2e", || e2e.exists());

    move_main(&repo);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(events_of(&db, waiting.id(), "landing_recheck_finished").is_empty());
    assert!(!rechecked.exists());
    assert_eq!(update_step(&db, "update_failed"), None);

    fs::write(&gate, "go").unwrap();
    await_job_ended(&db);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(rechecked.exists());
    run_step(&db, waiting.id(), "landing_recheck_clean").expect("rechecked clean");
    backend.join();
}

/// While a landing's verification runs (its command held until the test
/// writes the gate), the job of an automatic update just turned on does
/// not build: it logs what it waits for, and builds once the landing
/// ended, main's head with the landing in it.
#[test]
fn the_update_jobs_build_waits_for_a_landing_verification() {
    let (fixture, repo, db, _served) = served();
    dagq_source(&repo, "");
    let dir = fixture.dir.path();
    let (built, verifying, gate) = (dir.join("built"), dir.join("verifying"), dir.join("gate"));
    task_verified_by(&db, marked_and_held(&verifying, &gate));
    let options = update_options(false, format!("{}; exit 1", passing_build(&built)), None);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    thread::scope(|scope| {
        let supervisor =
            scope.spawn(|| supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options));
        await_that(&db, "the verification", || verifying.exists());
        let queue = SqliteQueue::open(&db).unwrap();
        let token = queue.supervisors().unwrap().remove(0).token;
        queue.set_auto_update(&token, true).unwrap();
        await_that(&db, "the job's wait", || {
            job_log(&db).contains("waits for the landing checks")
        });
        assert!(!built.exists());
        assert_eq!(update_step(&db, "update_failed"), None);

        fs::write(&gate, "go").unwrap();
        let outcome = returned(supervisor);
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
    });
    await_update_failed(&db);
    assert!(built.exists());
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = first_run(&mut queue, TaskId::new(2)).unwrap();
    assert_eq!(run.status(), RunStatus::Integrated);
    let landed = run_step(&db, run.id(), "run_integrated").unwrap();
    let failed = update_step(&db, "update_failed").unwrap();
    assert!(landed < failed, "{landed:?} {failed:?}");
    backend.join();
}

/// While the landing recheck's command runs (held until the test writes
/// the gate), the update's job does not build; it builds once the recheck
/// ended.
#[test]
fn the_update_jobs_build_waits_for_a_landing_recheck_command() {
    let (fixture, repo, db, _served) = served();
    let dir = fixture.dir.path();
    let (built, rechecking, gate) = (dir.join("built"), dir.join("rechecking"), dir.join("gate"));
    dagq_source(
        &repo,
        &recheck_command(&marked_and_held(&rechecking, &gate)),
    );
    task_verified_by(&db, "test -f seed.txt".into());
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let (waiting, reviewer) = waiting_on_a_concern(
        &db,
        &repo,
        &backend,
        &update_options(false, "exit 1".into(), None),
    );
    assert!(!rechecking.exists());

    move_main(&repo);
    let options = update_options(true, format!("{}; exit 1", passing_build(&built)), None);
    thread::scope(|scope| {
        let supervisor =
            scope.spawn(|| supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options));
        await_that(&db, "the recheck's command", || rechecking.exists());
        await_that(&db, "the job's wait", || {
            job_log(&db).contains("waits for the landing checks")
        });
        assert!(!built.exists());

        fs::write(&gate, "go").unwrap();
        let outcome = returned(supervisor);
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
    });
    await_update_failed(&db);
    assert!(built.exists());
    let finished = run_step(&db, waiting.id(), "landing_recheck_finished").unwrap();
    let failed = update_step(&db, "update_failed").unwrap();
    assert!(finished < failed, "{finished:?} {failed:?}");
    backend.join();
}
