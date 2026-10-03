//! Runtime tests: a headless worker's session wrapper started in the
//! background, without a cmux workspace (ADR-t1404-1). With `[headless]
//! wrapper = "background"` the supervisor starts the wrapper detached
//! (the test backend runs it on a thread), records its handle as the run's
//! workspace and its start (`wrapper_launched`), and the wrapper registers
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

/// Acceptance: under `[headless] wrapper = "background"` a headless run
/// opens no workspace: the supervisor starts its wrapper in the background
/// in the run's worktree, with the worker's environment and its output in
/// `session.log`, records the handle and the start, and the run lands; the
/// wrapper that ended is stopped by its handle like a closed workspace.
#[test]
fn a_background_headless_run_lands_without_a_workspace() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    wrappers_in_background(&repo);
    set_turns(dir.path(), FINISH);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert!(backend.tags.lock().unwrap().is_empty(), "no workspace");
    assert!(backend.groups.lock().unwrap().is_empty(), "no cmux group");
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
    assert_eq!(launches[0]["pid"], json!(std::process::id()));
    assert_eq!(launches[0]["workspace_id"], handle);
    let kinds = kinds(&detail);
    let at = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(at("workspace_created") < at("wrapper_launched"));
    assert!(at("wrapper_launched") < at("wrapper_started"));
    assert!(at("wrapper_started") < at("turn_started"));
    assert!(backend.closed().contains(&handle.to_owned()));
    assert!(backend.texts().is_empty());
    assert_eq!(backend.captures.load(Ordering::SeqCst), 0);
}

/// Acceptance: a worker question is answered as the next turn of the same
/// background session, and the run lands.
#[test]
fn a_background_session_takes_the_answer_of_its_question() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    wrappers_in_background(&repo);
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
    assert!(backend.texts().is_empty());
}

/// Acceptance: a review's revise goes to the same background session as its
/// next turn; the run lands without another wrapper.
#[test]
fn a_background_session_takes_a_revise() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    wrappers_in_background(&repo);
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
    assert!(backend.texts().is_empty());
}

/// Acceptance: a run parked `needs_session` is resumed by a new background
/// wrapper (`--resume`, its own log) whose start is recorded anew.
#[test]
fn a_parked_background_run_is_resumed_in_the_background() {
    let (dir, repo, db, backend) = headless_fixture(&[EvidenceCheck::E2e]);
    wrappers_in_background(&repo);
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
    assert!(backend.resumes.lock().unwrap().is_empty(), "no workspace");
    let launched = backend.launched.lock().unwrap().clone();
    assert_eq!(launched.len(), 2, "{launched:?}");
    assert!(launched[1].command.contains("'--resume'"));
    assert_eq!(
        launched[1].log,
        Path::new(run.run_dir().unwrap()).join("session-resume-1.log")
    );
    let launches = payloads(&detail, "wrapper_launched");
    assert_eq!(launches.len(), 2, "{launches:?}");
    // Both wrappers ran in this process, so their records name one pid;
    // the resume's is recorded anew, after its own workspace record.
    assert_eq!(launches[1]["pid"], json!(std::process::id()));
    let started = payloads(&detail, "turn_started");
    assert_eq!(started.len(), 2, "{started:?}");
}

/// Acceptance: a supervisor that stops leaves its background wrapper
/// running; the next one adopts the run by the wrapper's handle and its
/// heartbeat, and the turns go on to the landing.
#[test]
fn a_new_supervisor_adopts_a_background_session() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    wrappers_in_background(&repo);
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
                "pid": std::process::id(),
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
        application::recording::RecordingBackend,
        domain::turn::{TurnRequest, request_path},
        infrastructure::{adapters::Cmux, runtime_store::SqliteOpener},
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
    let cmux = Cmux {
        executable: dir.path().join("no-cmux"),
    };
    let lost_id = lost.to_string();
    assert!(!cmux.exists(&lost_id).unwrap(), "the wrapper is gone");
    let recording = RecordingBackend::over(
        &cmux,
        Arc::new(SqliteOpener {
            db: db.clone(),
            generators: clock::system(),
            actor: None,
        }),
        None,
        || None,
    )
    .stopping_left_turns(Arc::new(SystemProcesses));
    assert!(recording.exists(&lost_id).unwrap(), "the turn it left runs");
    recording.close(&lost_id).unwrap();
    assert!(!SystemProcesses.alive(turn) || zombie(turn));
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

/// Acceptance: `run close-workspaces` of an ended background run whose
/// wrapper was killed in the middle of a turn is not refused for the turn
/// it left (the run's agent, alive): the turn is the one the session
/// recorded (pid and start), so the dry run lists the session and the
/// close stops the turn and records the close.
#[test]
fn close_workspaces_stops_the_turn_a_killed_background_wrapper_left() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), "say slow; sleep 120");
    let run = provision_under(&repo, &db, "owner");
    let claude = backend.headless.clone().unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let mut wrapper = Wrapper(
        wrapper_command(&db, &run, &claude, true, false)
            .spawn()
            .unwrap(),
    );
    let handle = handle_of(wrapper.0.id());
    let handle_id = handle.to_string();
    queue
        .workspace_created(run.id(), &LeaseToken::new("owner"), &handle_id)
        .unwrap();
    record_launch(&mut queue, &run, &handle);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        has(queue, run.id(), "turn_started")
    });
    let started = payloads(&detail(&db), "turn_started")[0].clone();
    let turn = u32::try_from(started["pid"].as_u64().unwrap()).unwrap();
    let _turn = TurnGuard::of(turn);
    wrapper.0.kill().unwrap();
    exited(&mut wrapper, "the killed wrapper to be reaped");
    // The run ended with no supervisor behind it; its agent, the turn,
    // is alive.
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='interrupted' WHERE id=?1",
            [run.id().as_str()],
        )
        .unwrap();
    age_lease(&db, &run, 31);
    assert!(SystemProcesses.alive(turn));
    let cmux = db.parent().unwrap().join("bin/cmux");
    let cleanup = |apply: bool| {
        let mut args = vec![
            "run",
            "close-workspaces",
            "--cmux",
            cmux.to_str().unwrap(),
            run.id().as_str(),
        ];
        if apply {
            args.push("--apply");
        }
        let output = common::cli::invoke_with(&[], &db, &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let listed = cleanup(false);
    assert_eq!(
        listed["workspaces"][0]["workspace_id"],
        json!(handle_id),
        "{listed}"
    );
    assert_eq!(
        listed["workspaces"][0]["outcome"], "would_close",
        "{listed}"
    );
    assert!(SystemProcesses.alive(turn) && !zombie(turn));
    let closed = cleanup(true);
    assert_eq!(closed["workspaces"][0]["outcome"], "closed", "{closed}");
    assert!(!SystemProcesses.alive(turn) || zombie(turn));
    let closes = payloads(&detail(&db), "workspace_closed")
        .into_iter()
        .filter(|p| p["workspace_id"] == json!(handle_id))
        .count();
    assert_eq!(closes, 1);
    // Nothing is left to close.
    assert_eq!(cleanup(true)["workspaces"], json!([]));
}

/// A stub `cmux` under `dir` that cannot list the workspaces (its
/// `list-windows` fails) and records every call in `calls` next to it.
fn unlisting_cmux(dir: &Path) -> PathBuf {
    let stub = dir.join("unlisting-cmux").join("cmux");
    fs::create_dir_all(stub.parent().unwrap()).unwrap();
    common::template::script(
        &stub,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${0%/*}/calls\"\ncase \"$*\" in *list-windows*) echo 'cmux: cannot list' >&2; exit 1 ;; esac\n",
    );
    stub
}

/// Acceptance: `run close-workspaces` of an ended run whose earlier
/// session was a workspace and whose last one is a background session
/// whose wrapper was killed in the middle of a turn, while cmux cannot list
/// the workspaces: the background session is judged by its processes, not
/// by cmux's list, so the close stops the turn it left and records the
/// close of its handle, and only then does the command fail for the
/// workspace it could not judge, of which nothing is closed or recorded.
#[test]
fn close_workspaces_stops_a_background_session_while_cmux_cannot_list_the_workspaces() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), "say slow; sleep 120");
    let run = provision_under(&repo, &db, "owner");
    let claude = backend.headless.clone().unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    // The run's earlier session, in a workspace.
    queue
        .record_runtime_event(
            run.id(),
            EventKind::WorkspaceCreated,
            json!({"workspace_id": "WS-EARLIER"}),
        )
        .unwrap();
    let mut wrapper = Wrapper(
        wrapper_command(&db, &run, &claude, true, false)
            .spawn()
            .unwrap(),
    );
    let handle = handle_of(wrapper.0.id());
    let handle_id = handle.to_string();
    queue
        .workspace_created(run.id(), &LeaseToken::new("owner"), &handle_id)
        .unwrap();
    record_launch(&mut queue, &run, &handle);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        has(queue, run.id(), "turn_started")
    });
    let started = payloads(&detail(&db), "turn_started")[0].clone();
    let turn = u32::try_from(started["pid"].as_u64().unwrap()).unwrap();
    let _turn = TurnGuard::of(turn);
    wrapper.0.kill().unwrap();
    exited(&mut wrapper, "the killed wrapper to be reaped");
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='interrupted' WHERE id=?1",
            [run.id().as_str()],
        )
        .unwrap();
    age_lease(&db, &run, 31);
    let cmux = unlisting_cmux(dir.path());
    let cleanup = |apply: bool| {
        let mut args = vec![
            "run",
            "close-workspaces",
            "--cmux",
            cmux.to_str().unwrap(),
            run.id().as_str(),
        ];
        if apply {
            args.push("--apply");
        }
        let output = common::cli::invoke_with(&[], &db, &args);
        assert!(
            !output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    };
    // The dry run fails as well, and closes nothing.
    let error = cleanup(false);
    assert!(
        error.contains("cmux could not list the workspaces of the ended runs"),
        "{error}"
    );
    assert!(SystemProcesses.alive(turn) && !zombie(turn));
    assert!(payloads(&detail(&db), "workspace_closed").is_empty());
    let error = cleanup(true);
    assert!(
        error.contains("only their background sessions were closed"),
        "{error}"
    );
    assert!(!SystemProcesses.alive(turn) || zombie(turn));
    let detail = detail(&db);
    let closes = payloads(&detail, "workspace_closed");
    assert_eq!(closes.len(), 1, "{closes:?}");
    assert_eq!(closes[0]["workspace_id"], json!(handle_id));
    assert_eq!(closes[0]["reason"], "cleanup");
    let calls = fs::read_to_string(cmux.with_file_name("calls")).unwrap();
    assert!(calls.contains("list-windows"), "{calls}");
    assert!(!calls.contains("WS-EARLIER"), "{calls}");
}

/// Acceptance: a run resumed in a workspace after a background session,
/// whose live wrapper and agent took the pids that session's wrapper and
/// turn had: `run close-workspaces` judges them as a workspace session's,
/// by their pids, and refuses while they live, as it did before the
/// background wrappers. The earlier session's records (another start) do
/// not make the wrapper dead, nor the agent a turn left to stop.
#[test]
fn close_workspaces_refuses_a_live_workspace_session_with_a_background_sessions_pids() {
    let (_dir, repo, db, _backend) = headless_fixture(&[]);
    let run = provision_under(&repo, &db, "owner");
    let owner = LeaseToken::new("owner");
    // The resume's wrapper and agent: live processes.
    let wrapper = Wrapper(Command::new("sleep").arg("30").spawn().unwrap());
    let mut agent = Wrapper(Command::new("sleep").arg("30").spawn().unwrap());
    let (wrapper_pid, agent_pid) = (wrapper.0.id(), agent.0.id());
    let mut queue = SqliteQueue::open(&db).unwrap();
    // The earlier background session, whose wrapper and turn had the pids.
    let earlier = BackgroundHandle {
        pid: wrapper_pid,
        start: "Thu_Jan_1_00:00:00_1970".into(),
    };
    queue
        .workspace_created(run.id(), &owner, &earlier.to_string())
        .unwrap();
    record_launch(&mut queue, &run, &earlier);
    queue
        .record_runtime_event(
            run.id(),
            EventKind::TurnStarted,
            json!({"turn": 1, "pid": agent_pid, "start": earlier.start}),
        )
        .unwrap();
    // The resume, in a workspace.
    queue
        .record_runtime_event(
            run.id(),
            EventKind::WorkspaceCreated,
            json!({"workspace_id": "WS-RESUME", "resume_attempt": 1}),
        )
        .unwrap();
    queue
        .register_wrapper(run.id(), &owner, wrapper_pid)
        .unwrap();
    queue
        .register_agent(run.id(), wrapper_pid, agent_pid)
        .unwrap();
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='interrupted' WHERE id=?1",
            [run.id().as_str()],
        )
        .unwrap();
    age_lease(&db, &run, 31);
    let refused = || {
        let cmux = db.parent().unwrap().join("bin/cmux");
        let output = common::cli::invoke_with(
            &[],
            &db,
            &[
                "run",
                "close-workspaces",
                "--cmux",
                cmux.to_str().unwrap(),
                run.id().as_str(),
                "--apply",
            ],
        );
        assert!(!output.status.success());
        String::from_utf8_lossy(&output.stderr).into_owned()
    };
    // The run's processes are checked agent first: the agent is no turn
    // left by the earlier session.
    let error = refused();
    assert!(
        error.contains(&format!(
            "the agent of run {} (pid {agent_pid}) is alive",
            run.id()
        )),
        "{error}"
    );
    // With the agent gone, the wrapper is alive by its pid.
    agent.0.kill().unwrap();
    agent.0.wait().unwrap();
    let error = refused();
    assert!(
        error.contains(&format!(
            "the wrapper of run {} (pid {wrapper_pid}) is alive",
            run.id()
        )),
        "{error}"
    );
    assert!(payloads(&detail(&db), "workspace_closed").is_empty());
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

/// Acceptance: a wrapper started in a workspace still waits for the run's
/// workspace, whatever start the run records, and registers once the
/// workspace is recorded.
#[test]
fn a_workspace_wrapper_still_waits_for_its_workspace() {
    let (_dir, repo, db, backend) = headless_fixture(&[]);
    let run = provision_under(&repo, &db, "owner");
    let (provider, other) =
        headless_provider(&run, backend.headless.as_deref(), backend.codex.as_deref()).unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::WrapperLaunched,
            json!({"pid": std::process::id()}),
        )
        .unwrap();
    let wrapper = {
        let (db, id) = (db.clone(), run.id().clone());
        thread::spawn(move || {
            runtime::session_with_providers(
                &db,
                &id,
                &LeaseToken::new("owner"),
                &provider,
                Some(&other),
                &StubSpawner { db: db.clone() },
                false,
            )
        })
    };
    thread::sleep(Duration::from_millis(500));
    assert!(!has(&mut queue, run.id(), "wrapper_started"));
    queue
        .workspace_created(run.id(), &LeaseToken::new("owner"), "w1")
        .unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        has(queue, run.id(), "turn_finished")
    });
    fs::write(exit_path(Path::new(run.run_dir().unwrap())), "").unwrap();
    joined(wrapper, "the workspace wrapper to exit").unwrap();
}
