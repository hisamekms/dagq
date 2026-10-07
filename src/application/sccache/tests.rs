use super::*;
use crate::application::{EndedRunWorkspace, EndedRunWorktree};
use crate::domain::{EventId, RunEvent, RunId, RunStatus, TaskRun};
use std::cell::RefCell;

#[derive(Default)]
struct MemoryLog {
    events: RefCell<Vec<RunEvent>>,
}
impl MemoryLog {
    fn payloads(&self, kind: &str) -> Vec<Value> {
        self.events
            .borrow()
            .iter()
            .filter(|event| event.kind == kind)
            .map(|event| event.payload.clone())
            .collect()
    }
}
#[allow(unused_variables)]
impl RunLog for MemoryLog {
    fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        unreachable!("sccache reads queue events only")
    }
    fn active_runs(&self) -> Result<Vec<TaskRun>> {
        unreachable!("sccache reads queue events only")
    }
    fn all_runs(&self) -> Result<Vec<TaskRun>> {
        unreachable!("sccache reads queue events only")
    }
    fn all_events(&self) -> Result<Vec<RunEvent>> {
        unreachable!("sccache reads queue events only")
    }
    fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
        unreachable!("sccache reads queue events only")
    }
    fn run(&self, id: &RunId) -> Result<TaskRun> {
        unreachable!("sccache reads queue events only")
    }
    fn runs_with_status(&self, status: RunStatus) -> Result<Vec<TaskRun>> {
        unreachable!("sccache reads queue events only")
    }
    fn next_awaiting_integration(&self) -> Result<Option<TaskRun>> {
        unreachable!("sccache reads queue events only")
    }
    fn run_events(&self, _id: &RunId) -> Result<Vec<RunEvent>> {
        unreachable!("sccache reads queue events only")
    }
    fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
        unreachable!("sccache reads queue events only")
    }
    fn record_runtime_event(
        &self,
        id: &RunId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()> {
        unreachable!("sccache reads queue events only")
    }
    fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>> {
        unreachable!("sccache reads queue events only")
    }
    fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>> {
        unreachable!("sccache reads queue events only")
    }
    fn ended_run_worktree(&self, _: &RunId) -> Result<Option<EndedRunWorktree>> {
        unreachable!("sccache reads queue events only")
    }
    fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
        unreachable!("sccache reads queue events only")
    }
    fn latest_event_id(&self) -> Result<EventId> {
        unreachable!("sccache reads queue events only")
    }
    fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>> {
        unreachable!("sccache reads queue events only")
    }
    fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>> {
        unreachable!("sccache reads queue events only")
    }
    fn run_in_workspace(&self, workspace_id: &str) -> Result<Option<RunId>> {
        unreachable!("sccache reads queue events only")
    }
    fn record_backend_failure(
        &self,
        run: Option<&RunId>,
        payload: serde_json::Value,
    ) -> Result<()> {
        unreachable!("sccache reads queue events only")
    }
    fn record_queue_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<EventId> {
        let mut events = self.events.borrow_mut();
        let id = EventId::new(events.len() as i64 + 1);
        events.push(RunEvent {
            id,
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.as_str().into(),
            payload,
            created_at: String::new(),
            actor: None,
        });
        Ok(id)
    }
    fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>> {
        unreachable!("sccache reads queue events only")
    }
    fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>> {
        unreachable!("sccache reads queue events only")
    }
    fn latest_events_by_supervisor(
        &self,
        kinds: &[&str],
        supervisors: &[&str],
    ) -> Result<Vec<RunEvent>> {
        unreachable!("sccache reads queue events only")
    }
    fn claim_inbox_nudge(&self, payload: serde_json::Value) -> Result<bool> {
        unreachable!("sccache reads queue events only")
    }
    fn record_inbox_watcher_change(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<bool> {
        unreachable!("sccache reads queue events only")
    }
    fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
        Ok(self
            .events
            .borrow()
            .iter()
            .rev()
            .find(|event| kinds.contains(&event.kind.as_str()))
            .cloned())
    }
    fn events_of_between(
        &self,
        kinds: &[&str],
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        unreachable!("sccache reads queue events only")
    }
}

struct ObservedSccache {
    process: std::cell::RefCell<Option<crate::domain::sccache::ServerProcess>>,
    stats: std::cell::Cell<crate::domain::sccache::ServerStats>,
    calls: std::cell::RefCell<Vec<String>>,
    stats_programs: RefCell<Vec<String>>,
    fail_process: std::cell::Cell<bool>,
    fail_stats: std::cell::Cell<bool>,
    fail_stop: std::cell::Cell<bool>,
    fail_start: std::cell::Cell<bool>,
}

impl crate::application::SccacheServer for ObservedSccache {
    fn listening(&self, _: u16) -> anyhow::Result<bool> {
        Ok(self.process.borrow().is_some())
    }
    fn process(&self, _: u16) -> anyhow::Result<Option<crate::domain::sccache::ServerProcess>> {
        if self.fail_process.get() {
            anyhow::bail!("process refused");
        }
        Ok(self.process.borrow().clone())
    }
    fn stats(
        &self,
        program: &Path,
        _: &[(String, String)],
        _: u16,
    ) -> anyhow::Result<Option<crate::domain::sccache::ServerStats>> {
        assert!(
            self.process.borrow().is_some(),
            "stats must not start an absent server"
        );
        self.calls.borrow_mut().push("stats".into());
        self.stats_programs
            .borrow_mut()
            .push(program.to_string_lossy().into_owned());
        if self.fail_stats.get() {
            anyhow::bail!("stats refused");
        }
        Ok(Some(self.stats.get()))
    }
    fn stop(&self, _: &Path, env: &[(String, String)], _: u16) -> anyhow::Result<()> {
        self.calls.borrow_mut().push("stop".into());
        assert!(env.contains(&("SCCACHE_IDLE_TIMEOUT".into(), "0".into())));
        if self.fail_stop.get() {
            anyhow::bail!("stop refused");
        }
        *self.process.borrow_mut() = None;
        Ok(())
    }
    fn start(
        &self,
        _: &Path,
        env: &[(String, String)],
        _: u16,
    ) -> anyhow::Result<crate::application::ServerPid> {
        self.calls.borrow_mut().push("start".into());
        assert!(env.contains(&("SCCACHE_IDLE_TIMEOUT".into(), "0".into())));
        assert!(env.contains(&("PATH".into(), "/configured/bin".into())));
        if self.fail_start.get() {
            anyhow::bail!("start refused");
        }
        *self.process.borrow_mut() = Some(observed_process(43, false));
        Ok(Ok(43))
    }
}
fn observed_process(pid: u32, sandboxed: bool) -> crate::domain::sccache::ServerProcess {
    crate::domain::sccache::ServerProcess {
        pid,
        started_at: format!("Mon Oct 5 10:00:{pid} 2026"),
        parent_pid: 1,
        command: "sccache --internal-start-server".into(),
        sandboxed: sandboxed.then_some(true),
        started_unix: Some(STARTED_UNIX),
    }
}
/// The Unix start of [`observed_process`].
const STARTED_UNIX: i64 = 1_791_000_000;
fn observed_server(sandboxed: bool) -> ObservedSccache {
    ObservedSccache {
        process: std::cell::RefCell::new(Some(observed_process(42, sandboxed))),
        stats: std::cell::Cell::new(Default::default()),
        calls: Default::default(),
        stats_programs: Default::default(),
        fail_process: std::cell::Cell::new(false),
        fail_stats: std::cell::Cell::new(false),
        fail_stop: std::cell::Cell::new(false),
        fail_start: std::cell::Cell::new(false),
    }
}

fn target() -> SccacheTarget {
    SccacheTarget {
        program: "sccache".into(),
        port: 4226,
    }
}
fn env() -> Vec<(String, String)> {
    vec![
        ("PATH".into(), "/configured/bin".into()),
        (IDLE_TIMEOUT_VAR.into(), "600".into()),
    ]
}
fn started(queue: &MemoryLog, process: &ServerProcess, port: u16) {
    queue.record_queue_event(EventKind::SccacheServerStarted, json!({
        "pid": process.pid, "started_at": process.started_at, "port": port, "supervisor": "owner"
    })).unwrap();
}
fn observe_once(
    queue: &MemoryLog,
    server: &ObservedSccache,
    watch: &mut FailureWatch,
) -> Option<Value> {
    observe(
        queue,
        server,
        &target(),
        Path::new("/configured/bin/sccache"),
        &env(),
        watch,
        123,
    )
    .unwrap()
}

#[test]
fn ownership_requires_pid_port_start_and_a_recorded_supervisor() {
    let queue = MemoryLog::default();
    let process = observed_process(42, false);
    assert_eq!(owner(&queue, &process, 4226).unwrap(), "unknown");
    started(&queue, &process, 4226);
    assert_eq!(owner(&queue, &process, 4226).unwrap(), "owner");
    assert_eq!(owner(&queue, &process, 4227).unwrap(), "unknown");
    let mut changed = process.clone();
    changed.pid += 1;
    assert_eq!(owner(&queue, &changed, 4226).unwrap(), "unknown");
    changed = process.clone();
    changed.started_at = "reused PID".into();
    assert_eq!(owner(&queue, &changed, 4226).unwrap(), "unknown");
    queue.events.borrow_mut().last_mut().unwrap().payload["supervisor"] = Value::Null;
    assert_eq!(owner(&queue, &process, 4226).unwrap(), "unknown");
}

/// A start record from before `started_at` was recorded, or whose process
/// could not be read just after the start: no `started_at`, only `at`.
fn started_without_start_time(queue: &MemoryLog, pid: Value, port: u16, at: i64) {
    queue
        .record_queue_event(
            EventKind::SccacheServerStarted,
            json!({"pid": pid, "port": port, "at": at, "supervisor": "owner"}),
        )
        .unwrap();
}

#[test]
fn a_record_without_a_start_time_owns_a_server_started_near_its_at() {
    let process = observed_process(42, false);
    let window = crate::domain::sccache::START_RECORD_WINDOW_SECS;
    for at in [STARTED_UNIX, STARTED_UNIX - window, STARTED_UNIX + window] {
        let queue = MemoryLog::default();
        started_without_start_time(&queue, json!(42), 4226, at);
        assert_eq!(owner(&queue, &process, 4226).unwrap(), "owner", "{at}");
    }
    let queue = MemoryLog::default();
    started_without_start_time(&queue, json!(42), 4226, STARTED_UNIX);
    queue.events.borrow_mut().last_mut().unwrap().payload["started_at"] = Value::Null;
    assert_eq!(owner(&queue, &process, 4226).unwrap(), "owner");
}

#[test]
fn a_record_without_a_start_time_stays_unknown_off_its_at_pid_or_port() {
    let process = observed_process(42, false);
    let window = crate::domain::sccache::START_RECORD_WINDOW_SECS;
    for at in [STARTED_UNIX - window - 1, STARTED_UNIX + window + 1] {
        let queue = MemoryLog::default();
        started_without_start_time(&queue, json!(42), 4226, at);
        assert_eq!(owner(&queue, &process, 4226).unwrap(), "unknown", "{at}");
    }
    let queue = MemoryLog::default();
    started_without_start_time(&queue, json!(43), 4226, STARTED_UNIX);
    assert_eq!(owner(&queue, &process, 4226).unwrap(), "unknown");
    let queue = MemoryLog::default();
    started_without_start_time(&queue, json!(42), 4227, STARTED_UNIX);
    assert_eq!(owner(&queue, &process, 4226).unwrap(), "unknown");
    // A start whose pid could not be read (`pid_error`) owns nothing.
    let queue = MemoryLog::default();
    started_without_start_time(&queue, Value::Null, 4226, STARTED_UNIX);
    queue.events.borrow_mut().last_mut().unwrap().payload["pid_error"] = json!("no pid");
    assert_eq!(owner(&queue, &process, 4226).unwrap(), "unknown");
    // Without the process's start time, nothing is compared.
    let queue = MemoryLog::default();
    started_without_start_time(&queue, json!(42), 4226, STARTED_UNIX);
    let mut unread = process.clone();
    unread.started_unix = None;
    assert_eq!(owner(&queue, &unread, 4226).unwrap(), "unknown");
    // A record without `at` cannot be compared either.
    queue.events.borrow_mut().last_mut().unwrap().payload["at"] = Value::Null;
    assert_eq!(owner(&queue, &process, 4226).unwrap(), "unknown");
}

#[test]
fn doctor_status_and_detection_follow_a_record_without_a_start_time() {
    let queue = MemoryLog::default();
    let server = observed_server(false);
    started_without_start_time(&queue, json!(42), 4226, STARTED_UNIX + 3);
    let read = |stats| {
        report(
            &queue,
            &server,
            &target(),
            Path::new("sccache"),
            &env(),
            stats,
        )
        .unwrap()
    };
    assert_eq!(read(false)["health"], "running");
    assert_eq!(read(true)["started_by"], "owner");
    assert!(observe_once(&queue, &server, &mut FailureWatch::default()).is_none());
    assert!(queue.payloads(DETECTED).is_empty());
    // Off the window, the same server is of unknown origin and detected.
    let queue = MemoryLog::default();
    started_without_start_time(&queue, json!(42), 4226, STARTED_UNIX + 3600);
    let status = report(
        &queue,
        &server,
        &target(),
        Path::new("sccache"),
        &env(),
        false,
    )
    .unwrap();
    assert_eq!(status["health"], "unknown_origin");
    assert_eq!(status["started_by"], "unknown");
    observe_once(&queue, &server, &mut FailureWatch::default());
    assert_eq!(queue.payloads(DETECTED).len(), 1);
}

#[test]
fn a_foreign_identity_is_recorded_once_across_fresh_watches() {
    let queue = MemoryLog::default();
    let server = observed_server(false);
    for _ in 0..2 {
        assert!(observe_once(&queue, &server, &mut FailureWatch::default()).is_none());
    }
    let events = queue.payloads(DETECTED);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["pid"], 42);
    assert_eq!(
        events[0]["started_at"],
        observed_process(42, false).started_at
    );
    assert_eq!(events[0]["parent_pid"], 1);
    assert_eq!(events[0]["command"], "sccache --internal-start-server");
    server.process.borrow_mut().as_mut().unwrap().started_at = "reused PID".into();
    observe_once(&queue, &server, &mut FailureWatch::default());
    assert_eq!(queue.payloads(DETECTED).len(), 2);
    started(&queue, server.process.borrow().as_ref().unwrap(), 4226);
    observe_once(&queue, &server, &mut FailureWatch::default());
    assert_eq!(queue.payloads(DETECTED).len(), 2);
}

#[test]
fn failure_only_deltas_and_sandbox_evidence_mark_the_current_identity() {
    let queue = MemoryLog::default();
    let server = observed_server(false);
    let mut watch = FailureWatch::default();
    assert!(observe_once(&queue, &server, &mut watch).is_none());
    server.stats.set(ServerStats {
        requests: 7,
        failures: 7,
        compilations: 0,
    });
    let unhealthy = observe_once(&queue, &server, &mut watch).unwrap();
    assert_eq!(unhealthy["reason"], "failure_bias");
    assert_eq!(unhealthy["stats"]["failures"], 7);
    let sandboxed =
        observe_once(&queue, &observed_server(true), &mut FailureWatch::default()).unwrap();
    assert_eq!(sandboxed["reason"], "sandboxed");
}

#[test]
fn unhealthy_observations_keep_retrying_without_repeating_the_same_diagnosis() {
    let queue = MemoryLog::default();
    let server = observed_server(true);
    for failures in [0, 7, 12] {
        server.stats.set(ServerStats {
            requests: failures,
            failures,
            compilations: 0,
        });
        assert!(observe_once(&queue, &server, &mut FailureWatch::default()).is_some());
        // A failure notice between observations must not reset the diagnosis.
        record_restart_failure(&queue, Path::new("sccache"), fault()).unwrap();
    }
    assert_eq!(queue.payloads(UNHEALTHY).len(), 1);
    assert_eq!(queue.payloads(RESTART_FAILED).len(), 1);
    // A reused PID is a new identity, even with the same reason.
    server.process.borrow_mut().as_mut().unwrap().started_at = "later".into();
    observe_once(&queue, &server, &mut FailureWatch::default()).unwrap();
    assert_eq!(queue.payloads(UNHEALTHY).len(), 2);
    started(&queue, server.process.borrow().as_ref().unwrap(), 4226);
    observe_once(&queue, &server, &mut FailureWatch::default()).unwrap();
    assert_eq!(queue.payloads(UNHEALTHY).len(), 3);
    // The same identity can acquire a different diagnosis.
    server.process.borrow_mut().as_mut().unwrap().sandboxed = None;
    let mut watch = FailureWatch::default();
    assert!(observe_once(&queue, &server, &mut watch).is_none());
    server.stats.set(ServerStats {
        requests: 15,
        failures: 15,
        compilations: 0,
    });
    assert_eq!(
        observe_once(&queue, &server, &mut watch).unwrap()["reason"],
        "failure_bias"
    );
    assert_eq!(queue.payloads(UNHEALTHY).len(), 4);
}

#[test]
fn reports_classify_health_and_read_stats_only_for_an_existing_server() {
    let queue = MemoryLog::default();
    let server = observed_server(false);
    let read = |stats| {
        report(
            &queue,
            &server,
            &target(),
            Path::new("sccache"),
            &env(),
            stats,
        )
        .unwrap()
    };
    assert_eq!(read(false)["health"], "unknown_origin");
    assert!(server.calls.borrow().is_empty());
    started(&queue, server.process.borrow().as_ref().unwrap(), 4226);
    assert_eq!(read(false)["health"], "running");
    for kind in [
        EventKind::SccacheServerUnhealthy,
        EventKind::SccacheServerRestartFailed,
    ] {
        queue.record_queue_event(kind, json!({"pid": 42, "started_at": observed_process(42, false).started_at, "port": 4226})).unwrap();
        assert_eq!(read(false)["health"], "unhealthy");
    }
    queue
        .record_queue_event(
            EventKind::SccacheServerUnhealthy,
            json!({"pid": 42, "started_at": observed_process(42, false).started_at, "port": 4227}),
        )
        .unwrap();
    assert_eq!(read(false)["health"], "running");
    server.stats.set(ServerStats {
        requests: 7,
        failures: 7,
        compilations: 0,
    });
    let doctor = read(true);
    assert_eq!(doctor["started_by"], "owner");
    assert_eq!(doctor["failure_ratio"], 1.0);
    assert_eq!(doctor["stats"]["requests"], 7);
    server.process.borrow_mut().as_mut().unwrap().started_at = "different".into();
    assert_eq!(read(false)["health"], "unknown_origin");
    *server.process.borrow_mut() = None;
    let calls = server.calls.borrow().len();
    assert_eq!(read(true)["health"], "absent");
    assert!(observe_once(&queue, &server, &mut FailureWatch::default()).is_none());
    assert_eq!(server.calls.borrow().len(), calls);
}

#[test]
fn pending_restarts_keep_the_same_or_absent_identity_and_drop_replacements() {
    let queue = MemoryLog::default();
    let server = observed_server(false);
    assert!(pending_restart(&queue, &server, 4226).unwrap().is_none());
    queue
        .record_queue_event(
            EventKind::SccacheServerRestartFailed,
            json!({"pid": 42, "started_at": observed_process(42, false).started_at, "port": 4226}),
        )
        .unwrap();
    assert!(pending_restart(&queue, &server, 4226).unwrap().is_some());
    assert!(pending_restart(&queue, &server, 4227).unwrap().is_none());
    server.process.borrow_mut().as_mut().unwrap().pid = 99;
    assert!(pending_restart(&queue, &server, 4226).unwrap().is_none());
    server.process.borrow_mut().as_mut().unwrap().pid = 42;
    server.process.borrow_mut().as_mut().unwrap().started_at = "different".into();
    assert!(pending_restart(&queue, &server, 4226).unwrap().is_none());
    *server.process.borrow_mut() = None;
    assert!(pending_restart(&queue, &server, 4226).unwrap().is_some());
    started(&queue, &observed_process(43, false), 4226);
    assert!(pending_restart(&queue, &server, 4226).unwrap().is_none());
}

fn fault() -> Value {
    let mut payload = json!(observed_process(42, true));
    payload["port"] = json!(4226);
    payload["reason"] = json!("sandboxed");
    payload["supervisor"] = json!("supervisor-test");
    payload["supervisor_pid"] = json!(123);
    payload["at"] = json!(456);
    payload["error"] = json!("old error");
    payload["pid_error"] = json!("old pid error");
    payload["stats"] = json!({"failures": 7});
    payload
}
#[test]
fn replacement_payload_keeps_the_supervisor_and_replaces_only_the_old_identity() {
    let queue = MemoryLog::default();
    let server = observed_server(true);
    restart(
        &queue,
        &server,
        Path::new("/configured/bin/sccache"),
        &env(),
        fault(),
    )
    .unwrap();
    assert_eq!(*server.calls.borrow(), ["stop", "start"]);
    let event = queue.payloads(SCCACHE_SERVER_STARTED).pop().unwrap();
    assert_eq!(event["reason"], "restart");
    assert_eq!(event["idle_timeout"], "0");
    assert_eq!(event["program"], "/configured/bin/sccache");
    assert_eq!(event["supervisor"], "supervisor-test");
    assert_eq!(event["supervisor_pid"], 123);
    assert_eq!(event["at"], 456);
    assert_eq!(
        event["replaced"],
        json!({"pid":42, "started_at": observed_process(42, true).started_at, "reason":"sandboxed"})
    );
    assert_eq!(event["pid"], 43);
    assert_eq!(event["started_at"], observed_process(43, false).started_at);
    assert_eq!(event["parent_pid"], 1);
    assert_eq!(event["command"], "sccache --internal-start-server");
    assert_eq!(event["sandboxed"], Value::Null);
    for key in ["error", "pid_error", "stats"] {
        assert!(event.get(key).is_none());
    }
    assert!(pending_restart(&queue, &server, 4226).unwrap().is_none());
}
fn failed_restart(stop_fails: bool) {
    let queue = MemoryLog::default();
    let server = observed_server(true);
    server.fail_stop.set(stop_fails);
    server.fail_start.set(!stop_fails);
    for _ in 0..3 {
        assert!(restart(&queue, &server, Path::new("sccache"), &env(), fault()).is_err());
    }
    assert_eq!(queue.payloads(RESTART_FAILED).len(), 1);
    let failure = queue.payloads(RESTART_FAILED).pop().unwrap();
    assert_eq!(failure["pid"], 42);
    assert_eq!(failure["supervisor"], "supervisor-test");
    assert_eq!(failure["program"], "sccache");
    assert_eq!(
        failure["error"],
        if stop_fails {
            "stop refused"
        } else {
            "start refused"
        }
    );
    assert!(pending_restart(&queue, &server, 4226).unwrap().is_some());
    assert_eq!(
        crate::domain::event_attention(RESTART_FAILED, &failure)
            .unwrap()
            .to_string(),
        "stop sccache on the host; the supervisor starts it"
    );
    assert!(crate::domain::wakes_inbox(RESTART_FAILED, &failure));
    assert_eq!(server.calls.borrow().len(), if stop_fails { 3 } else { 6 });
}
#[test]
fn a_failed_stop_preserves_the_fault_and_emits_attention() {
    failed_restart(true);
}
#[test]
fn a_failed_start_preserves_the_fault_and_emits_attention() {
    failed_restart(false);
}

#[test]
fn restart_failure_notices_follow_identity_program_error_and_a_recorded_start() {
    let queue = MemoryLog::default();
    let program = Path::new("/configured/bin/sccache");
    let mut failure = fault();
    record_restart_failure(&queue, program, failure.clone()).unwrap();
    // Neither another supervisor nor observations and changing counters turn
    // the persisted fault into a fresh notification.
    failure["supervisor"] = json!("next-supervisor");
    failure["supervisor_pid"] = json!(999);
    failure["at"] = json!(789);
    failure["stats"] = json!({"failures": 15});
    failure["reason"] = json!("failure_bias");
    queue
        .record_queue_event(EventKind::SccacheServerDetected, failure.clone())
        .unwrap();
    queue
        .record_queue_event(EventKind::SccacheServerUnhealthy, failure.clone())
        .unwrap();
    record_restart_failure(&queue, program, failure.clone()).unwrap();
    assert_eq!(queue.payloads(RESTART_FAILED).len(), 1);
    // Each part of the identity and the error can make a new failure notice.
    for (index, (key, value)) in [
        ("pid", json!(43)),
        ("started_at", json!("later")),
        ("port", json!(4227)),
        ("error", json!("different error")),
    ]
    .into_iter()
    .enumerate()
    {
        failure[key] = value;
        record_restart_failure(&queue, program, failure.clone()).unwrap();
        record_restart_failure(&queue, program, failure.clone()).unwrap();
        assert_eq!(queue.payloads(RESTART_FAILED).len(), index + 2);
    }
    let other_program = Path::new("/other/bin/sccache");
    record_restart_failure(&queue, other_program, failure.clone()).unwrap();
    record_restart_failure(&queue, other_program, failure.clone()).unwrap();
    assert_eq!(queue.payloads(RESTART_FAILED).len(), 6);
    started(&queue, &observed_process(43, false), 4227);
    record_restart_failure(&queue, other_program, failure.clone()).unwrap();
    record_restart_failure(&queue, other_program, failure).unwrap();
    assert_eq!(queue.payloads(RESTART_FAILED).len(), 7);
}

#[test]
fn unreadable_processes_are_unknown_and_unreadable_stats_preserve_identity() {
    let queue = MemoryLog::default();
    let server = observed_server(false);
    server.fail_process.set(true);
    let report = super::report(
        &queue,
        &server,
        &target(),
        Path::new("sccache"),
        &env(),
        true,
    )
    .unwrap();
    assert_eq!(report["health"], "unknown");
    assert_eq!(report["error"], "process refused");
    assert!(server.calls.borrow().is_empty());
    server.fail_process.set(false);
    server.fail_stats.set(true);
    let report = super::report(
        &queue,
        &server,
        &target(),
        Path::new("sccache"),
        &env(),
        true,
    )
    .unwrap();
    assert_eq!(report["health"], "unknown_origin");
    assert_eq!(report["server"]["pid"], 42);
    assert_eq!(report["stats_error"], "stats refused");
    assert_eq!(report["stats"], Value::Null);
    server.process.borrow_mut().as_mut().unwrap().sandboxed = Some(true);
    assert_eq!(
        observe_once(&queue, &server, &mut FailureWatch::default()).unwrap()["reason"],
        "sandboxed"
    );
}

struct DiagnosticVerifier {
    wrapper: Option<&'static str>,
    unreadable: bool,
    resolved: Option<&'static str>,
    resolution_fails: bool,
}

impl Verifier for DiagnosticVerifier {
    fn run_env(&self, run_dir: &Path) -> Result<Vec<(String, String)>> {
        assert_eq!(run_dir, Path::new("queue"));
        if self.unreadable {
            anyhow::bail!("configuration cannot be read");
        }
        Ok(self
            .wrapper
            .map(|wrapper| vec![(WRAPPER_VAR.into(), wrapper.into())])
            .unwrap_or_default())
    }
    fn run_env_programs(
        &self,
        run_dir: Option<&Path>,
    ) -> Result<crate::domain::run_env::RunEnvCheck> {
        assert_eq!(run_dir, None);
        if self.resolution_fails {
            anyhow::bail!("program resolution refused");
        }
        use crate::domain::run_env::{RunEnvCheck, RunEnvProgram};
        Ok(RunEnvCheck {
            programs: vec![
                RunEnvProgram {
                    variable: "RUSTC".into(),
                    value: "rustc".into(),
                    resolved: Some("/resolved/rustc".into()),
                },
                RunEnvProgram {
                    variable: WRAPPER_VAR.into(),
                    value: self.wrapper.unwrap().into(),
                    resolved: self.resolved.map(str::to_owned),
                },
            ],
            ..Default::default()
        })
    }
    fn run_env_table(&self) -> Result<Option<Vec<(String, String)>>> {
        unreachable!("diagnostics only read environment and programs")
    }
    fn run_env_salt(&self) -> Result<String> {
        unreachable!("diagnostics only read environment and programs")
    }
    fn run_to_log(
        &self,
        _: &str,
        _: &Path,
        _: &[(String, String)],
        _: &Path,
    ) -> Result<crate::application::Exit> {
        unreachable!("diagnostics must not run verification")
    }
}

fn diagnostic_verifier() -> DiagnosticVerifier {
    DiagnosticVerifier {
        wrapper: Some("/configured/sccache"),
        unreadable: false,
        resolved: Some("/resolved/sccache"),
        resolution_fails: false,
    }
}

fn diagnostic_output(verifier: Option<&dyn Verifier>, stats: bool) -> (Value, ObservedSccache) {
    let queue = MemoryLog::default();
    let server = observed_server(false);
    let mut output = json!({"attention": [{"kind": RESTART_FAILED}, {"kind": "other"}]});
    add_diagnostics(
        &queue,
        &server,
        verifier,
        Path::new("queue"),
        stats,
        &mut output,
    )
    .unwrap();
    assert!(
        queue.events.borrow().is_empty(),
        "diagnostics are read-only"
    );
    (output, server)
}

#[test]
fn diagnostics_hide_the_field_and_restart_attention_without_a_checkout() {
    let (output, server) = diagnostic_output(None, true);
    assert!(output.get("sccache").is_none());
    assert_eq!(output["attention"], json!([{"kind": "other"}]));
    assert!(server.calls.borrow().is_empty());
    let mut doctor = json!({"checked_at": 123});
    add_diagnostics(
        &MemoryLog::default(),
        &server,
        None,
        Path::new("queue"),
        true,
        &mut doctor,
    )
    .unwrap();
    assert_eq!(doctor, json!({"checked_at": 123}));
}

#[test]
fn diagnostics_hide_the_field_and_restart_attention_for_unreadable_configuration() {
    let mut verifier = diagnostic_verifier();
    verifier.unreadable = true;
    let (output, server) = diagnostic_output(Some(&verifier), true);
    assert!(output.get("sccache").is_none());
    assert_eq!(output["attention"], json!([{"kind": "other"}]));
    assert!(server.calls.borrow().is_empty());
}

#[test]
fn diagnostics_hide_the_field_and_restart_attention_for_other_or_missing_wrappers() {
    for wrapper in [Some("other"), None] {
        let mut verifier = diagnostic_verifier();
        verifier.wrapper = wrapper;
        let (output, server) = diagnostic_output(Some(&verifier), true);
        assert!(output.get("sccache").is_none());
        assert_eq!(output["attention"], json!([{"kind": "other"}]));
        assert!(server.calls.borrow().is_empty());
    }
}

#[test]
fn diagnostics_keep_sccache_attention_and_resolve_its_stats_program() {
    let verifier = diagnostic_verifier();
    let (status, server) = diagnostic_output(Some(&verifier), false);
    assert_eq!(status["sccache"]["health"], "unknown_origin");
    assert_eq!(status["attention"][0]["kind"], RESTART_FAILED);
    assert!(server.calls.borrow().is_empty(), "status skips stats");
    let (doctor, server) = diagnostic_output(Some(&verifier), true);
    assert_eq!(doctor["sccache"]["stats"]["requests"], 0);
    assert_eq!(*server.stats_programs.borrow(), ["/resolved/sccache"]);
}

#[test]
fn diagnostics_use_the_configured_program_when_resolution_fails_or_is_missing() {
    for resolution_fails in [false, true] {
        let mut verifier = diagnostic_verifier();
        verifier.resolved = None;
        verifier.resolution_fails = resolution_fails;
        let (output, server) = diagnostic_output(Some(&verifier), true);
        assert_eq!(output["sccache"]["health"], "unknown_origin");
        assert_eq!(*server.stats_programs.borrow(), ["/configured/sccache"]);
    }
}
