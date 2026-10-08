//! `run log` and `planner log` (ADR-t1404-1 decision 6): the session
//! wrapper started in the background writes the `[dagq]` summary of its
//! turns to its log (the run dir's `session.log`, the planner dir's
//! `session.log`), and a person reads it, or follows it while the wrapper
//! runs, by the run's (or task's) or the planner's ID; the log of an ended
//! run or a closed planner is read too, and an unknown ID is refused.
//! `status` and `show` give a background run's wrapper pid and log,
//! `planners` a background planner's.
use crate::common::{self, actor::WithoutActor, cli};
use crate::plan_review::{PlanWorkspace, StubReviewer, add, submit};
use crate::planner_headless::{diagnose_planner, headless_fixture, queue_events, supervise_until};
use crate::runtime_background_process::{
    background_fixture, nothing_left, recorded, supervise_real, waiting_turn,
};
use crate::runtime_support;

use dagq::domain::{Priority, ProposalStatus, TaskId};
use dagq::infrastructure::location::planners_dir;
use runtime_support::headless::*;
use runtime_support::*;
use std::io::Read;

/// The text `dagq run log` / `planner log` prints for `args`, which must
/// succeed.
fn printed(db: &Path, args: &[&str]) -> String {
    let output = cli::invoke(db, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// The error `dagq` prints for `args`, which must fail.
fn failed(db: &Path, args: &[&str]) -> String {
    let output = cli::invoke(db, args);
    assert!(!output.status.success(), "{args:?} succeeded");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Acceptance: a background run's wrapper writes the `[dagq]` summary of
/// its turn to the run dir's `session.log`. While it runs, `status` and
/// `show` give its pid and log, and `run log RUN --follow` prints the log
/// and what the wrapper appends until the wrapper ends with the run's
/// landing. After the landing `run log` reads the whole log by the run ID
/// or the task ID, `--lines` its last lines; an unknown run or task is
/// refused.
#[test]
fn a_background_runs_log_is_followed_and_read_after_it_ended() {
    let (_dir, repo, db, claude, _leftovers) = background_fixture(&waiting_turn());
    let reviewer = Arc::new(TestReviewer::new(&[verdict("pass", &[], "fine")]));
    let supervisor = supervise_real(&db, &repo, &claude, reviewer, Default::default());
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), "turn_started").is_empty()
    });
    let wrapper = recorded(&db, "wrapper_launched").remove(0);
    let run = detail(&db).runs[0].clone();
    let run_id = run.id().to_string();
    let log = Path::new(run.run_dir().unwrap()).join("session.log");
    let expected = json!({
        "handle": wrapper.to_string(),
        "pid": wrapper.pid,
        "start": wrapper.start,
        "log": log.display().to_string(),
    });
    let status = cli::ok(&db, &["status"]);
    let entry = status["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["run_id"] == run_id.as_str())
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(entry["background"], expected);
    let show = cli::ok(&db, &["show", "2"]);
    assert_eq!(show["runs"][0]["background"], expected, "{show}");

    // Killed when the test ends, and by a timeout's exit, which skips the
    // drops.
    let mut follow = common::KillOnDrop::new(
        Command::new(env!("CARGO_BIN_EXE_dagq"))
            .without_actor_env()
            .args([
                "--db",
                db.to_str().unwrap(),
                "run",
                "log",
                &run_id,
                "--follow",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        "run log --follow",
    );
    let mut stdout = follow.child().stdout.take().unwrap();
    let reader = thread::spawn(move || {
        let mut text = String::new();
        stdout.read_to_string(&mut text).unwrap();
        text
    });
    fs::write(Path::new(run.run_dir().unwrap()).join("go"), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    nothing_left(&db);
    // The wrapper ended, so the follow ends by itself.
    let started = Instant::now();
    let status = loop {
        if let Some(status) = follow.child().try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(30) {
            panic!("run log --follow did not end with the wrapper");
        }
        thread::sleep(Duration::from_millis(50));
    };
    let mut stderr = String::new();
    follow
        .child()
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(status.success(), "{stderr}");
    let followed = joined(reader, "the reader of run log --follow");
    let written = fs::read_to_string(&log).unwrap();
    assert!(written.contains("[dagq] turn 1 started"), "{written}");
    assert!(written.contains("[dagq] finished"), "{written}");
    assert_eq!(followed, written);

    // The ended run's log, by the run and by the task.
    assert_eq!(detail(&db).runs[0].status(), RunStatus::Integrated);
    assert_eq!(printed(&db, &["run", "log", &run_id]), written);
    assert_eq!(printed(&db, &["run", "log", "2"]), written);
    let last = written.lines().last().unwrap();
    assert_eq!(
        printed(&db, &["run", "log", &run_id, "--lines", "1"]),
        format!("{last}\n")
    );
    // The run's background is no longer in `status`, which lists the
    // unfinished runs only.
    assert!(
        cli::ok(&db, &["status"])["runs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["run_id"] != run_id.as_str())
    );
    let unknown = failed(&db, &["run", "log", "no-such-run"]);
    assert!(unknown.contains("no-such-run"), "{unknown}");
    let no_task = failed(&db, &["run", "log", "99"]);
    assert!(no_task.contains("99"), "{no_task}");
    // Task 1 never ran.
    let no_run = failed(&db, &["run", "log", "1"]);
    assert!(no_run.contains("task 1 has no run"), "{no_run}");
}

/// Acceptance: a headless planner of the runtime's, started in the
/// background, writes the `[dagq]` summary of its turn to its directory's
/// `session.log`; once it closed, `planner log ID` reads it (with
/// `--follow` too, which ends at once as its wrapper is gone), `planners
/// --all` gives its wrapper's pid and log, and an unknown planner is
/// refused.
#[test]
fn a_closed_background_planners_log_is_read_by_its_id() {
    let fx = headless_fixture(
        "\"$DAGQ\" --db \"$DB\" submit --proposal 1 >> \"$RUN_DIR/submit.log\" 2>&1; say \"turn $TURN submitted\"",
    );
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "change", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], None);
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "revise", "reasons": ["split it"], "summary": "not yet"}),
        json!({"verdict": "pass", "reasons": [], "summary": "ok"}),
    ]);
    let backend = PlanWorkspace::running();
    supervise_until(
        &fx,
        &backend,
        &reviewer,
        || {
            !queue_events(&fx.db, "planner_closed").is_empty()
                && queue.show_proposal(proposal).unwrap().status() == ProposalStatus::Accepted
        },
        || diagnose_planner(&fx.db),
    );
    let planner = queue.planners(true).unwrap().remove(0);
    let handle = planner.workspace_id.clone().unwrap();
    let wrapper = dagq::domain::background_wrapper::BackgroundHandle::parse(&handle).unwrap();
    let log = planners_dir(&fx.db)
        .join(planner.id.to_string())
        .join("session.log");
    let written = fs::read_to_string(&log).unwrap();
    // The turn says it submitted just before it exits, so the line is read
    // after its exit as often as before.
    assert!(
        written.contains("[dagq] turn 1 started"),
        "{}",
        diagnose_planner(&fx.db)
    );
    assert!(
        written.contains("[dagq] turn 1 submitted"),
        "{}",
        diagnose_planner(&fx.db)
    );
    let id = planner.id.to_string();
    assert_eq!(printed(&fx.db, &["planner", "log", &id]), written);
    assert_eq!(
        printed(&fx.db, &["planner", "log", &id, "--follow"]),
        written
    );
    let planners = cli::ok(&fx.db, &["planners", "--all"]);
    assert_eq!(
        planners["planners"][0]["background"],
        json!({
            "handle": handle,
            "pid": wrapper.pid,
            "start": wrapper.start,
            "log": log.display().to_string(),
        }),
        "{planners}"
    );
    let unknown = failed(&fx.db, &["planner", "log", "99"]);
    assert!(unknown.contains("99"), "{unknown}");
}
