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

/// Thresholds with the `long_background` alert after a fifth of a second.
fn background_alert() -> dagq::domain::stall::StallConfig {
    dagq::domain::stall::StallConfig::default().with_millis("background_alert_secs", 200)
}

/// Supervise the fixture's task with [`ORPHAN_AGENT`], a background alert
/// after a fifth of a second, and `recovery` as the recovery job's script,
/// on a thread.
fn supervise_long_background(
    db: &Path,
    repo: &Path,
    recovery: &str,
) -> (
    Arc<TestWorkspace>,
    Arc<TestReviewer>,
    thread::JoinHandle<Result<Value>>,
) {
    supervise_repair(db, repo, ORPHAN_AGENT, background_alert(), &[recovery])
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
    finished(&db, &backend, supervisor);
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

/// The worker of task 438's run (task 918): it commits, leaves a wait loop
/// in its worktree (a shell that forks `sleep`, its pid in `bg.pid` of the
/// run directory), writes its receipt and goes idle with that background
/// work running. It goes idle without it once the loop is gone, or after a
/// bounded wait so that a runtime that never stops the loop fails the test
/// instead of hanging it.
const LEFTOVER_LOOP_AGENT: &str = r#"
commit work
bg="$(dirname "$RECEIPT")/bg.pid"
( sh -c 'while :; do sleep 1; done' >/dev/null 2>&1 & echo $! > "$bg.tmp"; mv "$bg.tmp" "$bg" )
receipt "$(git rev-parse HEAD)"; idle_bg
pid=$(cat "$bg")
i=0; while kill -0 "$pid" 2>/dev/null && [ $i -lt 1200 ]; do sleep 0.05; i=$((i + 1)); done
kill "$pid" 2>/dev/null
idle_bg_done; await_exit
"#;

/// Task 918: background work the session left running after its receipt
/// (task 438's run held its slot 5.6 hours with a wait loop) is the
/// `long_background` alert past its threshold too, with `phase:
/// after_receipt`; the recovery job's `stop_processes` stops the loop, the
/// session goes idle, and the run goes on to validation and lands without
/// an ask, a person or the observer.
#[test]
fn background_work_left_after_the_receipt_is_a_long_background_alert_for_the_recovery_job() {
    let (_dir, repo, db) = fixture();
    let (backend, reviewer, supervisor) = supervise_repair(
        &db,
        &repo,
        LEFTOVER_LOOP_AGENT,
        background_alert(),
        &[&recovery_verdict(&json!({
            "verdict": "repair",
            "confidence": "high",
            "diagnosis": "a wait loop left after the receipt holds the session",
            "actions": [{"action": "stop_processes", "pids": ["PID"]}],
        }))],
    );
    finished(&db, &backend, supervisor);
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
    assert_eq!(requested[0]["alert"], "long_background");
    assert_eq!(requested[0]["phase"], "after_receipt");
    assert_eq!(requested[0]["threshold_secs"], 1);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "receipt_observed") < position(&kinds, "recovery_requested"));
    let prompts = reviewer.triage_prompts();
    assert_eq!(prompts.len(), 1);
    for part in [
        "long_background",
        "after_receipt",
        &format!("- pid {pid} (parent "),
    ] {
        assert!(prompts[0].0.contains(part), "{part}: {}", prompts[0].0);
    }
    let repaired = payloads(&detail, "auto_repaired");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["alert"], "long_background");
    assert_eq!(repaired[0]["repair"], "stop_processes");
    assert_eq!(repaired[0]["processes"][0]["pid"], pid);
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["applied"], json!(["stop_processes"]));
    let idle = payloads(&detail, "session_idle_observed");
    assert_eq!(idle.len(), 1, "{idle:?}");
    assert_eq!(idle[0]["background_running"], false);
    assert!(position(&kinds, "auto_repaired") < position(&kinds, "session_idle_observed"));
    assert!(stalled_asks(&queue).is_empty());
}

/// Wait for the `stalled` ask of a `long_background` test, check that the
/// orphan still runs, then stop it as a person would and let the run end.
fn escalated_long_background(
    db: &Path,
    backend: &TestWorkspace,
    supervisor: thread::JoinHandle<Result<Value>>,
) -> (dagq::domain::Ask, dagq::domain::TaskDetail) {
    // Up to a step's limit: under the coverage gate's load the recovery job
    // and its escalation take longer than a fixed 30 s (task 433's integrate).
    wait_until(db, crate::common::STEP_LIMIT, |queue| {
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
    finished(db, backend, supervisor);
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

/// Task 782: the escalation of a live run that its recovery job's verdict
/// asked for (here `escalate`) is recorded at the job's request: its
/// `recovery_finished` and the `stalled` ask's `ask_opened` are the
/// supervisor's with the job as `requested_by`. A job that failed asked
/// for nothing: the same events are the supervisor's own.
#[test]
fn a_live_escalation_is_recorded_at_the_recovery_jobs_request_unless_the_job_failed() {
    let escalate = recovery_verdict(&json!({
        "verdict": "escalate",
        "confidence": "high",
        "diagnosis": "a person should look",
        "actions": [],
    }));
    for (script, by_job) in [
        (escalate.as_str(), true),
        ("echo broken >&2; exit 3", false),
    ] {
        let (_dir, repo, db) = fixture();
        let (backend, _reviewer, supervisor) = supervise_long_background(&db, &repo, script);
        let (ask, detail) = escalated_long_background(&db, &backend, supervisor);
        let run = &detail.runs[0];
        let finished = payloads(&detail, "recovery_finished");
        assert_eq!(finished.len(), 1, "{finished:?}");
        assert_eq!(finished[0]["ask_id"], json!(ask.id));
        assert_eq!(
            finished[0]["outcome"] == "job_failed",
            !by_job,
            "{finished:?}"
        );
        let expected = (
            "supervisor".to_owned(),
            format!("supervisor:{}", std::process::id()),
            by_job.then(|| format!("recovery-job:{}:long_background:1", run.id())),
        );
        for kind in ["recovery_finished", "ask_opened"] {
            let events: Vec<_> = detail
                .events
                .iter()
                .filter(|e| e.kind == kind && e.run_id.as_ref() == Some(run.id()))
                .collect();
            assert_eq!(events.len(), 1, "{kind}: {events:?}");
            let actor = events[0].actor.clone().expect("an actor");
            assert_eq!(
                (actor.role, actor.id, actor.requested_by),
                expected,
                "{kind}"
            );
        }
    }
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

/// How long a process makes no progress before the `idle_process` alert in
/// these tests, in milliseconds.
const IDLE_PROCESS_MS: u64 = 200;

/// Thresholds with the `idle_process` alert after [`IDLE_PROCESS_MS`].
fn idle_process_stall() -> dagq::domain::stall::StallConfig {
    dagq::domain::stall::StallConfig::default().with_millis("idle_process_secs", IDLE_PROCESS_MS)
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
    finished(&db, &backend, supervisor);
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
    assert_eq!(requested["threshold_secs"], 1);
    assert_eq!(requested["phase"], "session");
    let idle = requested["idle_processes"].as_array().unwrap();
    assert_eq!(idle.len(), 1, "{idle:?}");
    assert_eq!(idle[0]["pid"], pid);
    assert!(idle[0]["command"].as_str().unwrap().contains("sleep 300"));
    // Quiet for the threshold when the job was requested.
    let requested_ms = detail
        .events
        .iter()
        .find(|e| e.kind == "recovery_requested")
        .and_then(|e| dagq::domain::stats::timestamp_millis(&e.created_at))
        .unwrap();
    assert!(
        requested_ms - idle[0]["active_ms"].as_i64().unwrap() >= IDLE_PROCESS_MS as i64,
        "{idle:?}"
    );
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
            r#"i=0; while [ $i -lt 20 ]; do sleep 0.05; i=$((i + 1)); done; kill "$pid""#,
        ),
        idle_process_stall(),
        &[],
    );
    finished(&db, &backend, supervisor);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert!(payloads(&detail, "recovery_requested").is_empty());
    assert!(reviewer.triage_prompts().is_empty());
    assert!(stalled_asks(&queue).is_empty());
}
