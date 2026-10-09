//! Runtime tests: a headless worker's session wrapper started in the
//! background, without a cmux workspace (ADR-t1404-1), the only place it
//! starts (ADR-t1433-3). The supervisor starts the wrapper detached (the
//! test backend runs it on a thread), records its handle as the run's
//! session and its start (`wrapper_launched`), and the wrapper registers
//! once it finds that record with its own pid. The wrapper itself
//! (`session --background`) runs without a terminal.
use crate::common;
use crate::runtime_support;

use dagq::application::ProcessControl;
use dagq::domain::background_wrapper::BackgroundHandle;
use dagq::domain::{
    AskReason, EventKind, LeaseToken, background_wrapper::is_background, turn::exit_path,
};
use dagq::infrastructure::adapters::SystemProcesses;
use runtime_support::headless::*;
use runtime_support::*;

/// The kinds of `detail`'s events, in order.
fn kinds(detail: &dagq::domain::TaskDetail) -> Vec<&str> {
    detail.events.iter().map(|e| e.kind.as_str()).collect()
}

/// Whether the run `id` has an event of `kind`.
fn has(queue: &mut SqliteQueue, id: &RunId, kind: &str) -> bool {
    queue
        .run_events(id)
        .unwrap()
        .iter()
        .any(|event| event.kind == kind)
}

/// Acceptance: a headless run opens no workspace: the supervisor starts its
/// wrapper in the background in the run's worktree, with the worker's
/// environment and its output in `session.log`, records the handle and the
/// start, and the run lands; the wrapper that ended is stopped by its
/// handle. cmux is asked for no workspace group. `[headless] wrapper =
/// "workspace"` in `dagq.toml` is accepted and ignored, with a warning
/// (ADR-t1433-3 decision 2).
#[test]
fn a_background_headless_run_lands_without_a_workspace() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    workspace_wrapper_setting(&repo);
    set_turns(dir.path(), FINISH);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let options = supervise_options(4, true);
    let (telemetry, captured) = Telemetry::capture();
    let outcome =
        telemetry.in_scope(|| supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options));
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let log = captured.text();
    assert!(
        log.contains("[headless] wrapper = \\\"workspace\\\" of dagq.toml is ignored for workers"),
        "{log}"
    );
    let detail = detail(&db);
    let run = &detail.runs[0];
    let launched = backend.launched.lock().unwrap().clone();
    assert_eq!(launched.len(), 1, "{launched:?}");
    let launch = &launched[0];
    assert_eq!(launch.cwd, Path::new(run.worktree_path().unwrap()));
    assert_eq!(
        launch.log,
        Path::new(run.run_dir().unwrap()).join("session.log")
    );
    assert!(!launch.command.contains("'--resume'"), "{}", launch.command);
    for (name, value) in [
        ("DAGQ_ROLE", "worker".to_owned()),
        ("DAGQ_ACTOR_ID", format!("worker:{}", run.id())),
    ] {
        assert!(
            launch.env.iter().any(|(k, v)| k == name && *v == value),
            "{name}={value} in {:?}",
            launch.env
        );
    }
    assert!(!launch.env.iter().any(|(k, _)| k == "DAGQ_QUEUE"));
    let handle = run.workspace_id().unwrap();
    assert!(is_background(handle), "{handle}");
    let launches = payloads(&detail, "wrapper_launched");
    assert_eq!(launches.len(), 1, "{launches:?}");
    assert_eq!(
        launches[0]["pid"],
        json!(BackgroundHandle::parse(handle).unwrap().pid)
    );
    assert_eq!(launches[0]["workspace_id"], handle);
    // The worker's first turn records what its prompt takes (ADR-t2072-1).
    let prompt = fs::read(Path::new(run.run_dir().unwrap()).join("prompt.txt")).unwrap();
    let bytes = &launches[0]["prompt_bytes"];
    assert_eq!(bytes["total"], json!(prompt.len()), "{bytes}");
    assert_eq!(
        bytes["limit"],
        json!(dagq::application::prompt::WORKER_PROMPT_LIMIT)
    );
    assert!(bytes["sections"]["task"].as_u64().unwrap() > 0, "{bytes}");
    assert_eq!(bytes["over_limit"], Value::Null, "{bytes}");
    let kinds = kinds(&detail);
    let at = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(at("workspace_created") < at("wrapper_launched"));
    assert!(at("wrapper_launched") < at("wrapper_started"));
    assert!(at("wrapper_started") < at("turn_started"));
    assert!(backend.closed().contains(&handle.to_owned()));
}

/// Acceptance: a worker question is answered as the next turn of the same
/// background session, and the run lands.
#[test]
fn a_background_session_takes_the_answer_of_its_question() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) ask "which file"; say asked ;;
*) {FINISH} ;;
esac"#
        ),
    );
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(|ask| ask.kind == AskKind::WorkerQuestion)
    });
    let ask = SqliteQueue::open(&db)
        .unwrap()
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == AskKind::WorkerQuestion)
        .unwrap();
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "change.txt")
        .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Integrated);
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[1],
        format!("resume {} answer to ask {}: change.txt", run.id(), ask.id)
    );
    // One background wrapper took both turns.
    assert_eq!(backend.launched.lock().unwrap().len(), 1);
    assert_eq!(payloads(&detail, "wrapper_launched").len(), 1);
}

/// Acceptance: a review's revise goes to the same background session as its
/// next turn; the run lands without another wrapper.
#[test]
fn a_background_session_takes_a_revise() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$MODE" in
start) {FINISH} ;;
resume) printf 'fix\n' >> change.txt; git commit -q -am fix; receipt "$(git rev-parse HEAD)"; say fixed ;;
esac"#
        ),
    );
    let reviewer = TestReviewer::new(&[
        verdict("revise", &["name the file"], "almost"),
        verdict("pass", &[], "meets the acceptance"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Integrated);
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[1].starts_with(&format!("resume {} ", run.id())));
    assert_eq!(reviewer.prompts().len(), 2);
    assert_eq!(backend.launched.lock().unwrap().len(), 1);
}

/// Acceptance: a run parked `needs_session` is resumed by a new background
/// wrapper (`--resume`, its own log) whose start is recorded anew, the
/// ignored `[headless] wrapper = "workspace"` notwithstanding
/// (ADR-t1433-3).
#[test]
fn a_parked_background_run_is_resumed_in_the_background() {
    let (dir, repo, db, backend) = headless_fixture(&[EvidenceCheck::E2e]);
    workspace_wrapper_setting(&repo);
    set_turns(
        dir.path(),
        r#"case "$MODE" in
start) commit work; receipt "$(git rev-parse HEAD)"; say finished ;;
resume) printf 'fixed\n' > fixed.txt; git add fixed.txt; git commit -q -m fix; receipt "$(git rev-parse HEAD)"; say fixed ;;
esac"#,
    );
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let options = SuperviseOptions {
        run_e2e: runtime::RunE2eOptions {
            command: Some(
                "if [ -f fixed.txt ]; then echo 'test result: ok. 1 passed'; exit 0; fi; \
                 echo 'test a_test ... FAILED'; echo 'test result: FAILED. 0 passed; 1 failed'; exit 101"
                    .into(),
            ),
            ..Default::default()
        },
        ..supervise_options(4, true)
    };
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Integrated);
    let launched = backend.launched.lock().unwrap().clone();
    assert_eq!(launched.len(), 2, "{launched:?}");
    assert!(launched[1].command.contains("'--resume'"));
    assert_eq!(
        launched[1].log,
        Path::new(run.run_dir().unwrap()).join("session-resume-1.log")
    );
    let launches = payloads(&detail, "wrapper_launched");
    assert_eq!(launches.len(), 2, "{launches:?}");
    // Each wrapper goes by a process of its own: the resume's start is
    // recorded anew, with its own pid, after its own workspace record.
    let resumed = BackgroundHandle::parse(launches[1]["workspace_id"].as_str().unwrap()).unwrap();
    assert_eq!(launches[1]["pid"], json!(resumed.pid));
    assert_ne!(launches[0]["pid"], launches[1]["pid"]);
    let started = payloads(&detail, "turn_started");
    assert_eq!(started.len(), 2, "{started:?}");
}

/// Acceptance: a supervisor that stops leaves its background wrapper
/// running; the next one adopts the run by the wrapper's handle and its
/// heartbeat, and the turns go on to the landing.
#[test]
fn a_new_supervisor_adopts_a_background_session() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) say working ;;
*) {FINISH} ;;
esac"#
        ),
    );
    let backend = Arc::new(backend);
    let run = provision_under(&repo, &db, "dead-supervisor");
    let launched = backend.launch_background(
        Path::new(run.worktree_path().unwrap()),
        &shell_join(&[
            "runner".into(),
            "session".into(),
            "--run".into(),
            run.id().to_string(),
            "--lease".into(),
            "dead-supervisor".into(),
            "--background".into(),
        ]),
        &[],
        &Path::new(run.run_dir().unwrap()).join("session.log"),
    );
    let handle = launched.unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .workspace_created(run.id(), &LeaseToken::new("dead-supervisor"), &handle)
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::WrapperLaunched,
            json!({
                "pid": BackgroundHandle::parse(&handle).unwrap().pid,
                "start": BackgroundHandle::parse(&handle).unwrap().start,
                "workspace_id": handle,
            }),
        )
        .unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), "turn_finished").is_empty()
    });
    age_lease(&db, &run, 31);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::Stalled,
            task_id: None,
            run_id: Some(run.id().clone()),
            question: "the headless session ended its turns without a receipt".into(),
            options: vec!["wait".into(), "intervene".into()],
            asked_by: "supervisor".into(),
            reason_category: AskReason::RecoveryFailed,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    queue.answer(ask.id, "go on").unwrap();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    assert!(
        kinds(&detail).contains(&"run_adopted"),
        "{:?}",
        kinds(&detail)
    );
    // The adopter started no wrapper of its own.
    assert_eq!(backend.launched.lock().unwrap().len(), 1);
    assert_eq!(stub_calls(run).len(), 2);
}

/// Kills the wrapper process of a test when the test ends.
struct Wrapper(Child);

impl Drop for Wrapper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The real session wrapper of `run` under the lease `owner`, started the
/// way the supervisor starts one in the background (`--background`, and
/// `--resume` for a resume) but with its stdin and stdout no terminal and
/// no `CMUX_WORKSPACE_ID`; without `background` it is the wrapper of a
/// workspace.
fn wrapper_command(
    db: &Path,
    run: &TaskRun,
    claude: &Path,
    background: bool,
    resume: bool,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command.without_actor_env();
    command
        .env_remove("CMUX_WORKSPACE_ID")
        .arg("--db")
        .arg(db)
        .args(["session", "--run", run.id().as_str(), "--lease", "owner"])
        .arg("--claude")
        .arg(claude)
        .args(["--codex", "/nonexistent/codex"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if resume {
        command.arg("--resume");
    }
    if background {
        command.arg("--background");
    }
    command
}

/// The handle of the process `pid` as the supervisor records it, with its
/// start as the system shows it now.
fn handle_of(pid: u32) -> BackgroundHandle {
    BackgroundHandle::new(pid, &SystemProcesses.start_identity(pid).unwrap())
}

/// Record the supervisor's start of the background wrapper `handle` of
/// `run`.
fn record_launch(queue: &mut SqliteQueue, run: &TaskRun, handle: &BackgroundHandle) {
    queue
        .record_runtime_event(
            run.id(),
            EventKind::WrapperLaunched,
            json!({"pid": handle.pid, "start": handle.start, "workspace_id": handle.to_string()}),
        )
        .unwrap();
}

/// The number of the run's events of `kind`.
fn count(db: &Path, run: &TaskRun, kind: &str) -> usize {
    SqliteQueue::open(db)
        .unwrap()
        .run_events(run.id())
        .unwrap()
        .iter()
        .filter(|event| event.kind == kind)
        .count()
}

/// Wait for the wrapper `child` to exit; its status.
fn exited(child: &mut Wrapper, what: &str) -> std::process::ExitStatus {
    let _waiting = common::within(common::STEP_LIMIT, what);
    child.0.wait().unwrap()
}

/// Acceptance: the wrapper started with `--background` (the real binary,
/// its stdin and stdout no terminal and no `CMUX_WORKSPACE_ID`) is not
/// refused for want of a terminal; it waits for the supervisor's record of
/// its start, not for a workspace, and a record of another pid, or of its
/// pid with another start (a process that took the pid of a wrapper that
/// died), is not its own. Then it registers, starts its first turn, and
/// ends at the exit request. Without the flag the same wrapper is refused
/// for want of a terminal, as before.
#[test]
fn a_wrapper_without_a_terminal_or_workspace_registers_once_its_start_is_recorded() {
    let (_dir, repo, db, backend) = headless_fixture(&[]);
    let run = provision_under(&repo, &db, "owner");
    let claude = backend.headless.clone().unwrap();
    let refused = wrapper_command(&db, &run, &claude, false, false)
        .bounded_output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("interactive Claude wrapper requires a terminal"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let mut queue = SqliteQueue::open(&db).unwrap();
    let mut child = Wrapper(
        wrapper_command(&db, &run, &claude, true, false)
            .spawn()
            .unwrap(),
    );
    let handle = handle_of(child.0.id());
    queue
        .workspace_created(run.id(), &LeaseToken::new("owner"), &handle.to_string())
        .unwrap();
    for other in [
        BackgroundHandle {
            pid: handle.pid + 1,
            start: handle.start.clone(),
        },
        BackgroundHandle {
            pid: handle.pid,
            start: "Thu_Jan_1_00:00:00_1970".into(),
        },
    ] {
        record_launch(&mut queue, &run, &other);
        thread::sleep(Duration::from_millis(500));
        assert!(!has(&mut queue, run.id(), "wrapper_started"), "{other}");
    }
    record_launch(&mut queue, &run, &handle);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        has(queue, run.id(), "turn_finished")
    });
    let detail = detail(&db);
    assert_eq!(
        payloads(&detail, "wrapper_started")[0]["pid"],
        json!(handle.pid)
    );
    assert_eq!(detail.runs[0].status(), RunStatus::Running);
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    fs::write(exit_path(&run_dir), "").unwrap();
    let status = exited(&mut child, "the background wrapper to exit");
    assert!(status.success(), "{status}");
}

/// Acceptance: a background wrapper killed in the middle of a turn leaves
/// the turn running in its own group. The supervisor's backend tells the
/// session open by the turn it recorded (its pid and start), and closing
/// it stops the turn. The lost session is then opened again with the real
/// wrapper started `--background --resume` (no terminal, no
/// `CMUX_WORKSPACE_ID`): it registers as the resume's wrapper
/// (`register_resume_wrapper`) once its own start is recorded, and runs
/// the next request as a resumed turn.
#[test]
fn a_turn_left_by_a_killed_background_wrapper_is_stopped_and_a_resumed_wrapper_registers() {
    use dagq::{
        application::{RecordingQueue, SessionWrappers, recording::RecordingSessions},
        domain::{
            background_wrapper::StopRoute,
            turn::{TurnRequest, request_path},
        },
        infrastructure::{
            adapters::BackgroundSessions,
            runtime_store::{SqliteOpener, SqlitePorts},
        },
    };
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        r#"case "$TURN" in
1) say slow; sleep 120 ;;
*) say resumed ;;
esac"#,
    );
    let run = provision_under(&repo, &db, "owner");
    let claude = backend.headless.clone().unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let mut first = Wrapper(
        wrapper_command(&db, &run, &claude, true, false)
            .spawn()
            .unwrap(),
    );
    let lost = handle_of(first.0.id());
    queue
        .workspace_created(run.id(), &LeaseToken::new("owner"), &lost.to_string())
        .unwrap();
    record_launch(&mut queue, &run, &lost);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        has(queue, run.id(), "turn_started")
    });
    let first_turn = detail(&db);
    let started = payloads(&first_turn, "turn_started")[0].clone();
    let turn = u32::try_from(started["pid"].as_u64().unwrap()).unwrap();
    let _turn = TurnGuard::of(turn);
    assert!(started["start"].is_string(), "{started}");
    // SIGKILL: the wrapper stops no turn of its own.
    first.0.kill().unwrap();
    exited(&mut first, "the killed wrapper to be reaped");
    assert!(SystemProcesses.alive(turn));
    let lost_id = lost.to_string();
    assert!(
        !BackgroundSessions.exists(&lost_id).unwrap(),
        "the wrapper is gone"
    );
    let recording = RecordingSessions::over(
        &BackgroundSessions,
        Arc::new(SqlitePorts {
            opener: SqliteOpener {
                db: db.clone(),
                generators: clock::system(),
                actor: None,
            },
            keep: |queue| -> Box<dyn RecordingQueue + Send> { Box::new(queue) },
        }),
        None,
        || None,
    )
    .stopping_left_turns(Arc::new(SystemProcesses));
    assert!(recording.exists(&lost_id).unwrap(), "the turn it left runs");
    recording
        .stop_background(&lost_id, StopRoute::Close)
        .unwrap();
    assert!(!SystemProcesses.alive(turn) || zombie(turn));
    // The close names no route of its own; the wrapper was gone and the
    // turn it left was killed (task 1657).
    let stops = payloads(&detail(&db), "wrapper_stopped")
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert_eq!(stops[0]["route"], "close");
    assert_eq!(stops[0]["signal"], "gone");
    assert_eq!(stops[0]["left_turn_killed"], true);
    assert!(!recording.exists(&lost_id).unwrap());
    // The lost session opened again in the background.
    queue
        .clear_lost_session(run.id(), &LeaseToken::new("owner"), Some(lost.pid))
        .unwrap();
    let mut second = Wrapper(
        wrapper_command(&db, &run, &claude, true, true)
            .spawn()
            .unwrap(),
    );
    let reopened = handle_of(second.0.id());
    queue
        .session_reopened(
            run.id(),
            &LeaseToken::new("owner"),
            &reopened.to_string(),
            1,
            json!({"layer": "runtime", "repair": "test"}),
        )
        .unwrap();
    record_launch(
        &mut queue,
        &run,
        &BackgroundHandle {
            pid: reopened.pid,
            start: "Thu_Jan_1_00:00:00_1970".into(),
        },
    );
    thread::sleep(Duration::from_millis(500));
    assert_eq!(count(&db, &run, "wrapper_started"), 1);
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let request = TurnRequest {
        seq: 1,
        what: "answer".into(),
        prompt: "go on".into(),
    };
    let path = request_path(&run_dir, 1);
    fs::write(
        path.with_extension("json.tmp"),
        serde_json::to_string(&request).unwrap(),
    )
    .unwrap();
    fs::rename(path.with_extension("json.tmp"), &path).unwrap();
    record_launch(&mut queue, &run, &reopened);
    wait_until(&db, common::STEP_LIMIT, |_| {
        count(&db, &run, "turn_finished") >= 1 && count(&db, &run, "turn_started") == 2
    });
    let detail = detail(&db);
    let wrappers = payloads(&detail, "wrapper_started");
    assert_eq!(wrappers.len(), 2, "{wrappers:?}");
    assert_eq!(wrappers[1]["pid"], json!(reopened.pid));
    // Registered while the run is running: only a resume's wrapper may
    // (`register_resume_wrapper`); the first session's refuses it.
    assert_eq!(detail.runs[0].status(), RunStatus::Running);
    let started = payloads(&detail, "turn_started");
    assert_eq!(started[1]["request"], json!(1), "{started:?}");
    fs::write(exit_path(&run_dir), "").unwrap();
    let status = exited(&mut second, "the resumed background wrapper to exit");
    assert!(status.success(), "{status}");
}

/// Kills the process group of a turn a test left running when the test
/// ends, a failing one included, while its pid shows the start it had.
struct TurnGuard(u32, Option<String>);

impl TurnGuard {
    fn of(pid: u32) -> Self {
        Self(pid, SystemProcesses.start_identity(pid))
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if self.1.is_some() && SystemProcesses.start_identity(self.0) == self.1 {
            let _ = SystemProcesses.kill_group(self.0);
        }
    }
}

/// Whether `pid` is a zombie nobody has reaped yet: gone for this purpose.
fn zombie(pid: u32) -> bool {
    let stat = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&stat.stdout)
        .trim()
        .starts_with('Z')
}
