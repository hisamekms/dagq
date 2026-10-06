//! Runtime tests of a headless worker's background wrapper (ADR-t1404-1)
//! as real processes: the supervisor runs the production cmux adapter
//! (over the test's stub `cmux`), which starts the real `dagq session
//! --background` detached from it, and the wrapper runs its turns with the
//! stub headless `claude`. The wrapper and its turn outlive a supervisor
//! killed without a drain (one in a process of its own) and are adopted by
//! the next; every way the runtime stops a session (a landing canceled by
//! a person, the `stop` answer of a `stalled` ask, the recovery job's
//! `stop_processes`) leaves no wrapper or turn process behind.
use crate::common;
use crate::runtime_support;

use dagq::application::ProcessControl;
use dagq::application::naming::shell_join;
use dagq::domain::{
    EventKind, LeaseToken,
    background_wrapper::{BackgroundHandle, StopSignal, WrapperStop},
};
use dagq::infrastructure::{
    adapters::{Cmux, SystemProcesses},
    background::BackgroundWrappers,
};
use runtime_support::headless::*;
use runtime_support::*;

/// The variable that makes [`child_supervisor`] supervise, with the queue,
/// the repository and the stub `claude` as JSON.
const CHILD: &str = "BACKGROUND_PROCESS_TEST_SUPERVISOR";

/// The production cmux adapter over a stub `cmux` next to the queue at
/// `db`, which answers the supervisor's `ping`, cannot list the workspaces
/// (`list-windows` fails), and records every call in `calls`: a background
/// handle is served by the real `BackgroundWrappers` and never reaches it.
fn cmux(db: &Path) -> Cmux {
    let stub = cmux_stub(db);
    if !stub.exists() {
        fs::create_dir_all(stub.parent().unwrap()).unwrap();
        crate::common::template::script(
            &stub,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${0%/*}/calls\"\ncase \"$1\" in ping) echo PONG ;; esac\ncase \"$*\" in *list-windows*) echo 'cmux: cannot list' >&2; exit 1 ;; esac\n",
        );
    }
    Cmux { executable: stub }
}

/// The stub `cmux` of [`cmux`].
fn cmux_stub(db: &Path) -> PathBuf {
    db.parent().unwrap().join("background-cmux").join("cmux")
}

/// The handles of `kind`'s records of the run of task 2: the wrapper
/// starts (`wrapper_launched`) or the turns (`turn_started`), each with
/// its pid and the start recorded.
pub(crate) fn recorded(db: &Path, kind: &str) -> Vec<BackgroundHandle> {
    payloads(&detail(db), kind)
        .into_iter()
        .filter_map(|p| {
            Some(BackgroundHandle {
                pid: u32::try_from(p["pid"].as_u64()?).ok()?,
                start: p["start"].as_str()?.to_owned(),
            })
        })
        .collect()
}

/// Whether the process `handle` names still runs: its pid shows its start.
pub(crate) fn running(handle: &BackgroundHandle) -> bool {
    handle.is(
        handle.pid,
        SystemProcesses.start_identity(handle.pid).as_deref(),
    )
}

/// Wait until no wrapper and no turn the run recorded runs.
#[track_caller]
pub(crate) fn nothing_left(db: &Path) {
    let started = Instant::now();
    loop {
        let left: Vec<BackgroundHandle> = recorded(db, "wrapper_launched")
            .into_iter()
            .chain(recorded(db, "turn_started"))
            .filter(running)
            .collect();
        if left.is_empty() {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "still running: {left:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// Stops, when the test ends (a failing one included), every wrapper and
/// turn of the queue at its path that still runs: they are no children of
/// the test, which the fixture's cleanup would reach.
pub(crate) struct Leftovers(PathBuf);

impl Drop for Leftovers {
    fn drop(&mut self) {
        let Ok(mut queue) = SqliteQueue::open(&self.0) else {
            return;
        };
        let Ok(detail) = queue.show(TASK) else {
            return;
        };
        let handles = |kind: &str| -> Vec<BackgroundHandle> {
            payloads(&detail, kind)
                .into_iter()
                .filter_map(|p| {
                    Some(BackgroundHandle {
                        pid: u32::try_from(p["pid"].as_u64()?).ok()?,
                        start: p["start"].as_str()?.to_owned(),
                    })
                })
                .collect()
        };
        let wrappers = BackgroundWrappers {
            processes: &SystemProcesses,
        };
        for wrapper in handles("wrapper_launched") {
            let _ = wrappers.stop(&wrapper);
        }
        for turn in handles("turn_started").into_iter().filter(running) {
            let _ = SystemProcesses.kill_group(turn.pid);
        }
    }
}

/// Kills the supervisor's process when the test ends.
struct SupervisorProcess(Child);

impl Drop for SupervisorProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Supervise the queue at `db` on a thread through the production cmux
/// adapter, the wrappers' turns run by `claude`, with `reviewer` and
/// `stall`, until nothing is left to do.
pub(crate) fn supervise_real(
    db: &Path,
    repo: &Path,
    claude: &Path,
    reviewer: Arc<TestReviewer>,
    stall: dagq::domain::stall::StallConfig,
) -> thread::JoinHandle<Result<Value>> {
    let options = SuperviseOptions {
        stall: Some(stall),
        ..supervise_options(4, true)
    };
    let (db, repo, claude) = (db.to_owned(), repo.to_owned(), claude.to_owned());
    thread::spawn(move || {
        runtime::supervise_with_reviewer(
            &db,
            &repo,
            &cmux(&db),
            &claude,
            &*reviewer,
            Path::new(env!("CARGO_BIN_EXE_dagq")),
            &options,
        )
    })
}

/// The fixture of these tests: task 2 for a headless Claude worker whose
/// wrapper starts in the background, the stub `claude` its turns run, and
/// the guard that stops what the run leaves.
pub(crate) fn background_fixture(turns: &str) -> (Fixture, PathBuf, PathBuf, PathBuf, Leftovers) {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), turns);
    let claude = turns_only(dir.path(), &backend.headless.clone().unwrap());
    // Made before any supervisor (a child's too) looks for it.
    cmux(&db);
    let leftovers = Leftovers(db.clone());
    (dir, repo, db, claude, leftovers)
}

/// The `claude` the supervisor runs, which runs the stub `headless` only
/// for a turn (`--add-dir` the run dir): the supervisor also runs it for
/// its version, outside any run, where a turn's script (a commit, an
/// orphan) must never run.
fn turns_only(dir: &Path, headless: &Path) -> PathBuf {
    let guard = dir.join("claude-turns-only");
    crate::common::template::script(
        &guard,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do [ \"$arg\" = --add-dir ] && exec {} \"$@\"; done\necho '2.0.0 (Claude Code)'\n",
            shell_join(&[headless.to_str().unwrap().to_owned()])
        ),
    );
    guard
}

/// A turn that waits (at most 60 s) for the run dir's `go` file, then
/// commits and writes its receipt; later turns only say so.
pub(crate) fn waiting_turn() -> String {
    format!(
        r#"case "$TURN" in
1) i=0; while [ ! -f "$RUN_DIR/go" ] && [ $i -lt 1200 ]; do sleep 0.05; i=$((i + 1)); done; {FINISH} ;;
*) say again ;;
esac"#
    )
}

/// The supervisor of [`a_detached_wrapper_and_its_turn_outlive_a_killed_supervisor`]
/// in a process of its own, which that test kills: it does nothing unless
/// [`CHILD`] names its queue. A child whose test died stops itself after
/// two minutes.
#[test]
#[ignore = "run by a_detached_wrapper_and_its_turn_outlive_a_killed_supervisor"]
fn child_supervisor() {
    let Ok(spec) = std::env::var(CHILD) else {
        return;
    };
    let spec: Value = serde_json::from_str(&spec).unwrap();
    let path = |key: &str| PathBuf::from(spec[key].as_str().unwrap());
    let (db, repo, claude) = (path("db"), path("repo"), path("claude"));
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
    let options = supervise_options(4, false);
    let stop = options.stop.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(120));
        stop.store(true, Ordering::SeqCst);
    });
    runtime::supervise_with_reviewer(
        &db,
        &repo,
        &cmux(&db),
        &claude,
        &reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap();
}

/// Acceptance: a supervisor in a process of its own starts the run's
/// wrapper in the background and is killed (SIGKILL, no drain) while the
/// wrapper's turn runs. The wrapper, which is not its child, and the turn
/// go on; the next supervisor adopts the run by the wrapper's recorded pid,
/// start and heartbeat, starts no wrapper of its own, and the turn ends
/// with the receipt that lands. Nothing of the session runs afterwards.
#[test]
fn a_detached_wrapper_and_its_turn_outlive_a_killed_supervisor() {
    let (dir, repo, db, claude, _leftovers) = background_fixture(&waiting_turn());
    let base = git_out(&repo, &["rev-parse", "main"]);
    let spec = json!({"db": db, "repo": repo, "claude": claude}).to_string();
    let log = fs::File::create(dir.path().join("child-supervisor.log")).unwrap();
    let mut child = SupervisorProcess(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "runtime_background_process::child_supervisor",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, spec)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), "turn_started").is_empty()
    });
    let wrapper = recorded(&db, "wrapper_launched").remove(0);
    let turn = recorded(&db, "turn_started").remove(0);
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(running(&wrapper), "the wrapper outlives its supervisor");
    assert!(running(&turn), "the turn outlives its supervisor");
    let parent = Command::new("ps")
        .args(["-o", "ppid=", "-p", &wrapper.pid.to_string()])
        .output()
        .unwrap();
    assert_ne!(
        String::from_utf8_lossy(&parent.stdout).trim(),
        child.0.id().to_string(),
        "the wrapper was no child of the supervisor"
    );
    let reviewer = Arc::new(TestReviewer::new(&[verdict("pass", &[], "fine")]));
    let supervisor = supervise_real(&db, &repo, &claude, reviewer, Default::default());
    wait_until(&db, common::STEP_LIMIT, |queue| {
        payloads(&queue.show(TASK).unwrap(), "run_adopted").len() == 1
    });
    assert!(running(&turn), "the adopted turn still runs");
    let run_dir = PathBuf::from(detail(&db).runs[0].run_dir().unwrap());
    fs::write(run_dir.join("go"), "").unwrap();
    let outcome = joined(supervisor, "the adopting supervisor to return").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    // The adopter started no wrapper: the one turn was the first one's.
    assert_eq!(recorded(&db, "wrapper_launched"), [wrapper]);
    assert_eq!(stub_calls(run).len(), 1);
    nothing_left(&db);
}

/// Acceptance: a person cancels the landing of a run whose review raised
/// a concern. The runtime asks only once the background session has ended
/// (its exit request, then the close of its handle), so the cancel finds
/// no session to stop: the run fails, the task is canceled, the close of
/// the handle is recorded once, and nothing of the session runs.
#[test]
fn a_canceled_landing_leaves_nothing_of_the_background_session() {
    let (_dir, repo, db, claude, _leftovers) = background_fixture(FINISH);
    let reviewer = Arc::new(TestReviewer::new(&[verdict(
        "concern",
        &["needs a person"],
        "unsure",
    )]));
    let supervisor = supervise_real(&db, &repo, &claude, reviewer, Default::default());
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(|ask| ask.kind == AskKind::ApproveLanding)
    });
    let wrapper = recorded(&db, "wrapper_launched").remove(0);
    // The concern ended the session before it asked: its exit request,
    // then the close of its handle, whose wrapper is gone.
    let kinds: Vec<String> = detail(&db).events.iter().map(|e| e.kind.clone()).collect();
    let at = |kind: &str| kinds.iter().position(|k| k == kind).unwrap();
    assert!(at("exit_requested") < at("workspace_closed"), "{kinds:?}");
    assert!(at("workspace_closed") < at("ask_opened"), "{kinds:?}");
    assert!(!running(&wrapper), "the session ended before the ask");
    let ask = SqliteQueue::open(&db)
        .unwrap()
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == AskKind::ApproveLanding)
        .unwrap();
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "cancel")
        .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    // The answer is applied by the next supervisor.
    let reviewer = Arc::new(TestReviewer::new(&[]));
    let supervisor = supervise_real(&db, &repo, &claude, reviewer, Default::default());
    let outcome = joined(supervisor, "the supervisor that applies the answer").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    let closes = payloads(&detail, "workspace_closed");
    assert_eq!(closes.len(), 1, "{closes:?}");
    assert_eq!(closes[0]["workspace_id"], json!(wrapper.to_string()));
    // The stop of the handle after the review is recorded with its route.
    let stops = payloads(&detail, "wrapper_stopped");
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert_eq!(stops[0]["route"], "after_review");
    assert_eq!(stops[0]["workspace_id"], json!(wrapper.to_string()));
    assert_eq!(stops[0]["pid"], json!(wrapper.pid));
    nothing_left(&db);
}

/// Acceptance: a person answers `stop` to the `stalled` ask of a
/// background session whose turns end without a receipt: the session is
/// ended and the run goes to its recovery job, with nothing left running.
#[test]
fn a_stop_answer_stops_the_background_session() {
    let (_dir, repo, db, claude, _leftovers) = background_fixture("denied; say refused");
    let reviewer = Arc::new(TestReviewer::new(&[verdict("pass", &[], "fine")]));
    let supervisor = supervise_real(&db, &repo, &claude, reviewer, Default::default());
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !stalled_asks(queue).is_empty()
    });
    let wrapper = recorded(&db, "wrapper_launched").remove(0);
    assert!(running(&wrapper), "the session waits for its next request");
    let ask = stalled_asks(&SqliteQueue::open(&db).unwrap()).remove(0);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "stop")
        .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    let resolved = payloads(&detail, "stall_resolved");
    assert!(
        resolved.iter().any(|p| p["outcome"] == "answered_stop"),
        "{resolved:?}"
    );
    nothing_left(&db);
}

/// Acceptance: a background session's turn leaves an orphan in the
/// worktree that does nothing (`idle_process`); the recovery job's
/// `stop_processes` stops it, the turn goes on to the receipt and the run
/// lands, with nothing of the session (nor the orphan) left running.
#[test]
fn stop_processes_stops_a_background_sessions_orphan() {
    let turns = format!(
        r#"( sleep 600 >/dev/null 2>&1 & echo $! > "$RUN_DIR/bg.pid.tmp"; mv "$RUN_DIR/bg.pid.tmp" "$RUN_DIR/bg.pid" )
pid=$(cat "$RUN_DIR/bg.pid")
i=0; while kill -0 "$pid" 2>/dev/null && [ $i -lt 1200 ]; do sleep 0.05; i=$((i + 1)); done
kill "$pid" 2>/dev/null
{FINISH}"#
    );
    let (_dir, repo, db, claude, _leftovers) = background_fixture(&turns);
    let base = git_out(&repo, &["rev-parse", "main"]);
    let reviewer = Arc::new(
        TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[
            crate::runtime_repair::recovery_verdict(&json!({
                "verdict": "repair",
                "confidence": "high",
                "diagnosis": "an orphan sleep holds the session",
                "actions": [{"action": "stop_processes", "pids": ["PID"]}],
            })),
        ]),
    );
    let stall = dagq::domain::stall::StallConfig::default().with_millis("idle_process_secs", 1000);
    let supervisor = supervise_real(&db, &repo, &claude, reviewer, stall);
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    let orphan: u32 = fs::read_to_string(Path::new(run.run_dir().unwrap()).join("bg.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!pid_alive(orphan));
    let repaired = payloads(&detail, "auto_repaired");
    let stops: Vec<&&Value> = repaired
        .iter()
        .filter(|p| p["repair"] == "stop_processes")
        .collect();
    assert_eq!(stops.len(), 1, "{repaired:?}");
    assert_eq!(stops[0]["processes"][0]["pid"], json!(orphan));
    nothing_left(&db);
}

/// Acceptance: the supervisor's sweep stops the background session of an
/// ended run without asking cmux (whose stub cannot list the workspaces).
/// The run opened a workspace before it, which the sweep's candidates keep
/// (every workspace a run opened, closed or not): a workspace from before
/// ADR-t1433-3 is left to a person, and cmux is not listed for it, while
/// the background session, judged by its wrapper's process, is stopped:
/// its wrapper and turn are stopped and the stop of its handle is
/// recorded, with nothing recorded or asked of cmux for the workspace.
#[test]
fn the_sweep_stops_an_ended_background_session_while_cmux_cannot_list() {
    let (_dir, repo, db, claude, _leftovers) = background_fixture("say slow; sleep 120");
    let run = provision_under(&repo, &db, "owner");
    let mut queue = SqliteQueue::open(&db).unwrap();
    // The run's earlier session, in a workspace.
    queue
        .record_runtime_event(
            run.id(),
            EventKind::WorkspaceCreated,
            json!({"workspace_id": "WS-EARLIER"}),
        )
        .unwrap();
    // The background session's wrapper, started as the supervisor starts it.
    let command = shell_join(&[
        env!("CARGO_BIN_EXE_dagq").to_owned(),
        "--db".into(),
        db.to_str().unwrap().into(),
        "session".into(),
        "--run".into(),
        run.id().to_string(),
        "--lease".into(),
        "owner".into(),
        "--claude".into(),
        claude.to_str().unwrap().into(),
        "--codex".into(),
        "/nonexistent/codex".into(),
        "--background".into(),
    ]);
    let handle = BackgroundWrappers {
        processes: &SystemProcesses,
    }
    .launch(
        Path::new(run.worktree_path().unwrap()),
        &command,
        &[],
        &Path::new(run.run_dir().unwrap()).join("session.log"),
    )
    .unwrap();
    let wrapper = BackgroundHandle::parse(&handle).unwrap();
    queue
        .workspace_created(run.id(), &LeaseToken::new("owner"), &handle)
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::WrapperLaunched,
            json!({"pid": wrapper.pid, "start": wrapper.start, "workspace_id": handle}),
        )
        .unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), "turn_started").is_empty()
    });
    let turn = recorded(&db, "turn_started").remove(0);
    // The run ended with no supervisor behind it and its task canceled:
    // the sweep's, not the triage's.
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='interrupted' WHERE id=?1",
            [run.id().as_str()],
        )
        .unwrap();
    queue.transition(TASK, TaskAction::Cancel).unwrap();
    age_lease(&db, &run, 31);
    assert!(running(&wrapper) && running(&turn));
    let reviewer = Arc::new(TestReviewer::new(&[]));
    let supervisor = supervise_real(&db, &repo, &claude, reviewer, Default::default());
    let outcome = joined(supervisor, "the sweeping supervisor to return").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    nothing_left(&db);
    let detail = detail(&db);
    let closes = payloads(&detail, "workspace_closed");
    assert_eq!(
        closes,
        [&json!({"workspace_id": handle, "by": "supervisor", "reason": "superseded"})]
    );
    // The wrapper ran when the sweep stopped it: by SIGTERM, or SIGKILL
    // after the grace, never `gone` (task 1657).
    let stops = payloads(&detail, "wrapper_stopped");
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert_eq!(stops[0]["route"], "sweep");
    assert_eq!(stops[0]["workspace_id"], json!(handle));
    assert_eq!(stops[0]["pid"], json!(wrapper.pid));
    assert!(
        ["sigterm", "sigkill"].contains(&stops[0]["signal"].as_str().unwrap()),
        "{stops:?}"
    );
    assert!(stops[0]["children_killed"].is_u64(), "{stops:?}");
    let calls = fs::read_to_string(cmux_stub(&db).with_file_name("calls")).unwrap();
    assert!(!calls.contains("list-windows"), "{calls}");
    assert!(!calls.contains("WS-EARLIER"), "{calls}");
}

/// Stops, when a test ends (a failing one included), the wrapper and the
/// turn of [`wrapper_with_turn`] while they show the start they had.
struct StopsWrapper(BackgroundHandle, u32, Option<String>);

impl Drop for StopsWrapper {
    fn drop(&mut self) {
        let _ = BackgroundWrappers {
            processes: &SystemProcesses,
        }
        .stop(&self.0);
        if self.2.is_some() && SystemProcesses.start_identity(self.1) == self.2 {
            let _ = SystemProcesses.kill_group(self.1);
            let _ = SystemProcesses.kill(self.1);
        }
    }
}

/// A wrapper started as the runtime starts one, in `dir`, that runs
/// `script` (`$TURN` names the file it writes the pid of its turn to);
/// its handle and the turn's pid, once it started, and the guard that
/// stops both.
fn wrapper_with_turn(
    dir: &Path,
    script: &str,
) -> (
    BackgroundWrappers<'static>,
    BackgroundHandle,
    u32,
    StopsWrapper,
) {
    let turn = dir.join("turn.pid");
    let script = script.replace("$TURN", &shell_join(&[turn.to_str().unwrap().to_owned()]));
    let wrappers = BackgroundWrappers {
        processes: &SystemProcesses,
    };
    let handle = wrappers
        .launch(
            dir,
            &shell_join(&["sh".into(), "-c".into(), script]),
            &[],
            &dir.join("session.log"),
        )
        .unwrap();
    let handle = BackgroundHandle::parse(&handle).unwrap();
    let started = Instant::now();
    let pid = loop {
        if let Some(pid) = fs::read_to_string(&turn)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            break pid;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the turn did not start"
        );
        thread::sleep(Duration::from_millis(20));
    };
    let guard = StopsWrapper(handle.clone(), pid, SystemProcesses.start_identity(pid));
    (wrappers, handle, pid, guard)
}

/// Acceptance (task 1657): a wrapper that takes SIGTERM (stopping its
/// turn, as the session wrapper does) ends by it, and its stop says so;
/// a second stop finds it gone. Nothing is left. (Its turn may still be
/// dying when the stop looks, so the SIGKILLs sent to it are not fixed.)
#[test]
fn a_wrapper_that_takes_sigterm_is_stopped_by_it() {
    let dir = tempfile::tempdir().unwrap();
    let (wrappers, handle, turn, _guard) = wrapper_with_turn(
        dir.path(),
        "trap 'kill $!; exit 0' TERM; sleep 30 & echo $! > $TURN.tmp; mv $TURN.tmp $TURN; wait",
    );
    let stop = wrappers.stop(&handle).unwrap();
    assert_eq!(stop.signal, StopSignal::Term, "{stop:?}");
    assert!(!running(&handle));
    let started = Instant::now();
    while pid_alive(turn) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the turn is left"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        wrappers.stop(&handle).unwrap(),
        WrapperStop {
            signal: StopSignal::Gone,
            children_killed: 0
        }
    );
}

/// Acceptance (task 1657): a wrapper that ignores SIGTERM is killed after
/// the grace, and its turn, in a process group of its own, is sent SIGKILL
/// too; the stop says `sigkill` and counts the turn. Nothing is left.
#[test]
fn a_wrapper_that_ignores_sigterm_is_killed_with_its_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (wrappers, handle, turn, _guard) = wrapper_with_turn(
        dir.path(),
        "trap '' TERM; perl -e 'use POSIX; setsid(); exec qw(sleep 30)' & echo $! > $TURN.tmp; mv $TURN.tmp $TURN; sleep 30; wait",
    );
    let stop = wrappers.stop(&handle).unwrap();
    assert_eq!(stop.signal, StopSignal::Kill, "{stop:?}");
    assert!(stop.children_killed >= 1, "{stop:?}");
    assert!(!running(&handle));
    let started = Instant::now();
    while pid_alive(turn) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the turn is left"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Acceptance (task 1657): the stop of a background wrapper no run
/// records (a planner's, one whose start was never recorded) goes through
/// the supervisor's recording backend like a run's, and is recorded as the
/// queue's own `wrapper_stopped`, on no run, with its handle, pid, signal
/// and route.
#[test]
fn the_stop_of_a_wrapper_no_run_records_is_the_queues_event() {
    use dagq::{
        application::{WorkspaceBackend, recording::RecordingBackend},
        domain::background_wrapper::StopRoute,
        infrastructure::runtime_store::SqliteOpener,
    };
    let (dir, _repo, db) = fixture();
    let (_, handle, _, _guard) = wrapper_with_turn(
        dir.path(),
        "trap 'kill $!; exit 0' TERM; sleep 30 & echo $! > $TURN.tmp; mv $TURN.tmp $TURN; wait",
    );
    let cmux = Cmux {
        executable: dir.path().join("no-cmux"),
    };
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
    let stop = recording
        .stop_background(&handle.to_string(), StopRoute::Planner)
        .unwrap()
        .expect("the cmux adapter tells how the stop ended");
    assert_ne!(stop.signal, StopSignal::Gone, "{stop:?}");
    assert!(!running(&handle));
    let recorded: Vec<(Option<String>, Value)> = Connection::open(&db)
        .unwrap()
        .prepare("SELECT run_id, payload FROM run_events WHERE kind = 'wrapper_stopped'")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                serde_json::from_str(&row.get::<_, String>(1)?).unwrap(),
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        recorded,
        [(
            None,
            json!({
                "workspace_id": handle.to_string(),
                "pid": handle.pid,
                "signal": stop.signal.as_str(),
                "children_killed": stop.children_killed,
                "left_turn_killed": false,
                "route": "planner",
            })
        )]
    );
}
