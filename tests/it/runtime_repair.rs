//! Runtime tests: Repairs of long background work.
use crate::runtime_support;

use runtime_support::*;

/// The worker of the `long_background` tests: it commits, leaves an orphan
/// `sleep` in its worktree (its pid in `bg.pid` of the run directory), goes
/// idle with background work running, and writes its receipt once the
/// orphan is gone.
const ORPHAN_AGENT: &str = r#"
commit work
bg="$(dirname "$RECEIPT")/bg.pid"
( sleep 300 >/dev/null 2>&1 & echo $! > "$bg.tmp"; mv "$bg.tmp" "$bg" )
idle_bg
pid=$(cat "$bg")
while kill -0 "$pid" 2>/dev/null; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#;

/// Supervise the fixture's task with [`ORPHAN_AGENT`], a background alert
/// after one second, and `recovery` as the recovery job's script, on a
/// thread.
fn supervise_long_background(
    db: &Path,
    repo: &Path,
    recovery: &str,
) -> (
    Arc<TestWorkspace>,
    Arc<TestReviewer>,
    thread::JoinHandle<Result<Value>>,
) {
    supervise_repair(
        db,
        repo,
        ORPHAN_AGENT,
        dagq::domain::stall::StallConfig {
            background_alert_secs: 1,
            ..Default::default()
        },
        &[recovery],
    )
}

/// Supervise the fixture's task with `agent`, the thresholds `stall`, and
/// `recoveries` as the recovery jobs' scripts, on a thread.
fn supervise_repair(
    db: &Path,
    repo: &Path,
    agent: &str,
    stall: dagq::domain::stall::StallConfig,
    recoveries: &[&str],
) -> (
    Arc<TestWorkspace>,
    Arc<TestReviewer>,
    thread::JoinHandle<Result<Value>>,
) {
    let backend = Arc::new(TestWorkspace::new(db, false, agent));
    let recoveries: Vec<String> = recoveries.iter().map(|r| (*r).to_owned()).collect();
    let reviewer =
        Arc::new(TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&recoveries));
    let options = SuperviseOptions {
        stall: Some(stall),
        ..supervise_options(4, true)
    };
    let supervisor = {
        let (db, repo, backend, reviewer) = (
            db.to_owned(),
            repo.to_owned(),
            backend.clone(),
            reviewer.clone(),
        );
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    (backend, reviewer, supervisor)
}

/// A recovery job's script that prints `verdict` with `PID` replaced by the
/// orphan's pid.
fn recovery_verdict(verdict: &Value) -> String {
    let text = verdict.to_string().replace("\"PID\"", "$(cat bg.pid)");
    format!("printf '%s\\n' \"{}\"", text.replace('"', "\\\""))
}

/// Task 360 (ADR-0047 decisions 39 and 40): background work past its
/// threshold starts the recovery job with the run's processes; its repair
/// stops only the orphan of the run's worktree, recorded as
/// `auto_repaired`, and the session goes on to its receipt without an ask.
#[test]
fn a_long_background_alert_is_repaired_by_stopping_the_orphan_of_the_worktree() {
    let (_dir, repo, db) = fixture();
    let (backend, reviewer, supervisor) = supervise_long_background(
        &db,
        &repo,
        &recovery_verdict(&json!({
            "verdict": "repair",
            "confidence": "high",
            "diagnosis": "an orphan sleep holds the session",
            "actions": [{"action": "stop_processes", "pids": ["PID"]}],
        })),
    );
    let outcome = supervisor.join().unwrap().unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    let pid: u32 = fs::read_to_string(Path::new(run.run_dir().unwrap()).join("bg.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!pid_alive(pid));
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["alert"], "long_background");
    assert_eq!(requested[0]["attempt"], 1);
    assert_eq!(requested[0]["threshold_secs"], 1);
    assert_eq!(requested[0]["background_tasks"][0]["command"], "cargo test");
    let prompts = reviewer.triage_prompts();
    assert_eq!(prompts.len(), 1);
    let (prompt, cwd) = &prompts[0];
    assert_eq!(cwd, Path::new(run.run_dir().unwrap()));
    for part in [
        "long_background",
        &format!("- pid {pid} (parent "),
        "stop_processes",
        "\"verdict\": \"repair\" | \"escalate\"",
    ] {
        assert!(prompt.contains(part), "{part}: {prompt}");
    }
    let repaired = payloads(&detail, "auto_repaired");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["layer"], "recovery");
    assert_eq!(repaired[0]["repair"], "stop_processes");
    assert_eq!(repaired[0]["processes"][0]["pid"], pid);
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["escalated"], false);
    assert_eq!(finished[0]["applied"], json!(["stop_processes"]));
    assert_eq!(finished[0]["confidence"], "high");
    assert!(stalled_asks(&queue).is_empty());
}

/// Wait for the `stalled` ask of a `long_background` test, check that the
/// orphan still runs, then stop it as a person would and let the run end.
fn escalated_long_background(
    db: &Path,
    backend: &TestWorkspace,
    supervisor: thread::JoinHandle<Result<Value>>,
) -> (dagq::domain::Ask, dagq::domain::TaskDetail) {
    wait_until(db, Duration::from_secs(30), |queue| {
        !stalled_asks(queue).is_empty()
    });
    let mut queue = SqliteQueue::open(db).unwrap();
    let ask = stalled_asks(&queue).remove(0);
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let pid: u32 = fs::read_to_string(Path::new(run.run_dir().unwrap()).join("bg.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid_alive(pid), "the orphan was stopped");
    assert!(payloads(&queue.show(TaskId::new(1)).unwrap(), "auto_repaired").is_empty());
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .unwrap();
    let outcome = supervisor.join().unwrap().unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    (ask, queue.show(TaskId::new(1)).unwrap())
}

/// A repair that names a process outside the run is not applied at all:
/// the runtime refuses the verdict and asks the inbox (`recovery_failed`);
/// the ask closes itself once the session moves on.
#[test]
fn a_recovery_repair_of_a_process_outside_the_run_becomes_an_ask() {
    let (_dir, repo, db) = fixture();
    let mut outsider = std::process::Command::new("/bin/sleep")
        .arg("300")
        .current_dir(repo.parent().unwrap())
        .spawn()
        .unwrap();
    let (backend, _reviewer, supervisor) = supervise_long_background(
        &db,
        &repo,
        &recovery_verdict(&json!({
            "verdict": "repair",
            "confidence": "high",
            "diagnosis": "two sleeps",
            "actions": [{"action": "stop_processes", "pids": ["PID", outsider.id()]}],
        })),
    );
    let (ask, detail) = escalated_long_background(&db, &backend, supervisor);
    assert!(pid_alive(outsider.id()));
    let _ = outsider.kill();
    let _ = outsider.wait();
    assert_eq!(ask.options, ["wait", "intervene", "propose"]);
    for part in [
        "alert: long_background",
        "Why a person: recovery_failed",
        &format!(
            "pid {} is not one of the run's own processes",
            outsider.id()
        ),
        "Diagnosis: two sleeps",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["escalated"], true);
    assert_eq!(finished[0]["reason_category"], "recovery_failed");
    assert_eq!(finished[0]["ask_id"], json!(ask.id));
    assert_eq!(ask.reason_category, dagq::domain::AskReason::RecoveryFailed);
    let queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let resolved = payloads(&detail, "stall_resolved");
    assert_eq!(resolved.len(), 1, "{resolved:?}");
    assert_eq!(resolved[0]["threshold"], "background_alert_secs");
    assert_eq!(resolved[0]["outcome"], "resolved_by_itself");
}

/// A repair the job is not sure of is not applied: it becomes an ask with
/// the job's actions as the recommendation and its options added.
#[test]
fn a_recovery_repair_of_low_confidence_becomes_an_ask() {
    let (_dir, repo, db) = fixture();
    let (backend, _reviewer, supervisor) = supervise_long_background(
        &db,
        &repo,
        &recovery_verdict(&json!({
            "verdict": "repair",
            "confidence": "low",
            "diagnosis": "maybe a slow test",
            "actions": [{"action": "stop_processes", "pids": ["PID"]}],
            "options": ["stop it"],
        })),
    );
    let (ask, detail) = escalated_long_background(&db, &backend, supervisor);
    assert_eq!(ask.options, ["wait", "intervene", "stop it", "propose"]);
    for part in [
        "confidence low",
        "Recommended: [{\"action\":\"stop_processes\"",
        "Why a person: recovery_failed",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished[0]["escalated"], true);
    assert_eq!(finished[0]["confidence"], "low");
}

/// The worker of the `idle_process` tests: it commits, leaves an orphan in
/// its worktree (`child`, its pid in `bg.pid` of the run directory), polls
/// for it without going idle (short `sleep`s, never one process that
/// lives long), and writes its receipt once the orphan is gone. `wait` is
/// how the session waits: until the orphan ends, or a bounded wait after
/// which it stops the orphan itself.
fn idle_agent(child: &str, wait: &str) -> String {
    format!(
        r#"
commit work
bg="$(dirname "$RECEIPT")/bg.pid"
( {child} >/dev/null 2>&1 & echo $! > "$bg.tmp"; mv "$bg.tmp" "$bg" )
pid=$(cat "$bg")
{wait}
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#
    )
}

/// Thresholds with the `idle_process` alert after two seconds.
fn idle_process_stall() -> dagq::domain::stall::StallConfig {
    dagq::domain::stall::StallConfig {
        idle_process_secs: 2,
        ..Default::default()
    }
}

/// Task 469: an orphan of the worktree that stays alive without using CPU
/// time is the `idle_process` alert, whose recovery job gets the idle
/// processes with their CPU time and stops the orphan; the session then
/// writes its receipt and the run lands without an ask.
#[test]
fn a_process_without_cpu_progress_is_an_idle_process_alert_for_the_recovery_job() {
    let (_dir, repo, db) = fixture();
    let (backend, reviewer, supervisor) = supervise_repair(
        &db,
        &repo,
        &idle_agent(
            "sleep 300",
            r#"while kill -0 "$pid" 2>/dev/null; do sleep 0.05; done"#,
        ),
        idle_process_stall(),
        &[&recovery_verdict(&json!({
            "verdict": "repair",
            "confidence": "high",
            "diagnosis": "an orphan sleep uses no CPU and holds the session",
            "actions": [{"action": "stop_processes", "pids": ["PID"]}],
        }))],
    );
    let outcome = supervisor.join().unwrap().unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let run = &detail.runs[0];
    let pid: u32 = fs::read_to_string(Path::new(run.run_dir().unwrap()).join("bg.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!pid_alive(pid));
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    let requested = requested[0];
    assert_eq!(requested["alert"], "idle_process");
    assert_eq!(requested["threshold"], "idle_process_secs");
    assert_eq!(requested["threshold_secs"], 2);
    assert_eq!(requested["phase"], "session");
    let idle = requested["idle_processes"].as_array().unwrap();
    assert_eq!(idle.len(), 1, "{idle:?}");
    assert_eq!(idle[0]["pid"], pid);
    assert!(idle[0]["command"].as_str().unwrap().contains("sleep 300"));
    assert!(idle[0]["idle_secs"].as_i64().unwrap() >= 2, "{idle:?}");
    let prompts = reviewer.triage_prompts();
    assert_eq!(prompts.len(), 1);
    for part in ["idle_process", &format!("- pid {pid} (parent "), ", cpu 0."] {
        assert!(prompts[0].0.contains(part), "{part}: {}", prompts[0].0);
    }
    let repaired = payloads(&detail, "auto_repaired");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["alert"], "idle_process");
    assert_eq!(repaired[0]["repair"], "stop_processes");
    assert_eq!(repaired[0]["processes"][0]["pid"], pid);
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["alert"], "idle_process");
    assert_eq!(finished[0]["applied"], json!(["stop_processes"]));
    assert!(stalled_asks(&queue).is_empty());
}

/// A process that lives past the threshold but keeps using CPU time is
/// making progress: no `idle_process` alert and no recovery job.
#[test]
fn a_long_process_that_uses_cpu_time_is_not_an_idle_process_alert() {
    let (_dir, repo, db) = fixture();
    let (backend, reviewer, supervisor) = supervise_repair(
        &db,
        &repo,
        &idle_agent(
            "yes",
            r#"i=0; while [ $i -lt 120 ]; do sleep 0.05; i=$((i + 1)); done; kill "$pid""#,
        ),
        idle_process_stall(),
        &[],
    );
    let outcome = supervisor.join().unwrap().unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert!(payloads(&detail, "recovery_requested").is_empty());
    assert!(reviewer.triage_prompts().is_empty());
    assert!(stalled_asks(&queue).is_empty());
}
