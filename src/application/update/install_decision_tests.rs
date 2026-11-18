//! Install, rollback and successive runtime updates on memory ports.
use super::*;
use crate::application::install::test_support::{At, Handoff, InstallQueue, InstallSteps, OnlyOne};
use crate::application::memory_files::MemoryFiles;
use std::time::UNIX_EPOCH;

// Termination takes effect immediately, so recovery never waits on a host process.
struct Stops(Arc<std::sync::atomic::AtomicBool>);
impl ProcessControl for Stops {
    fn alive(&self, pid: u32) -> bool {
        pid == 1 && self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn terminate(&self, _: u32) -> anyhow::Result<()> {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn interrupt(&self, pid: u32) -> anyhow::Result<()> {
        self.terminate(pid)
    }
    fn kill(&self, pid: u32) -> anyhow::Result<()> {
        self.terminate(pid)
    }
}

fn recorded(events: &[(String, Value)]) -> Vec<RunEvent> {
    events
        .iter()
        .enumerate()
        .rev()
        .map(|(i, (kind, payload))| RunEvent {
            id: EventId::new(i as i64 + 1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.as_str().into(),
            payload: payload.clone(),
            created_at: String::new(),
            actor: None,
        })
        .collect()
}

#[test]
fn each_landing_installs_and_records_its_own_commit() {
    let steps = Arc::new(InstallSteps {
        heartbeats: true,
        gate: Some(install::E2eOutcome {
            passed: true,
            ..Default::default()
        }),
        ..Default::default()
    });
    steps.registrations.lock().unwrap()[0].auto_update = true;
    let files = MemoryFiles::default();
    files.put(Path::new("/queue"), UNIX_EPOCH, "queue");
    files.put(Path::new("/to"), UNIX_EPOCH, "old");
    let queues = |_: &Path| -> Arc<dyn QueueOpener> { Arc::new(InstallQueue(steps.clone())) };
    let ports = JobPorts {
        binaries: &*steps,
        files: &files,
        processes: &OnlyOne,
        clock: &At::new(10_000),
        queues: &queues,
        restart: &|_| bail!("no failed supervisor"),
        is_ancestor: &|_, _| bail!("no failures to settle"),
    };
    for commit in ["seed", "source", "restored-source"] {
        let report = run(
            &ports,
            Path::new("/queue"),
            &JobOptions {
                commit: commit.into(),
                token: LeaseToken::new("first"),
                target: "/to".into(),
                repository: "/repo".into(),
                paths: UpdatePaths::under(Path::new("/q")),
                log: "/log".into(),
                build_command: None,
                e2e: E2eSettings {
                    command: None,
                    timeout: Duration::ZERO,
                    run_env_root: None,
                    queue_dir: None,
                    scratch: "/scratch".into(),
                    log: "/gate.log".into(),
                    utc_offset_secs: 0,
                    lock: None,
                    git_env: vec![],
                    ended: None,
                },
                restart: vec![],
                handoff_timeout: Duration::ZERO,
                watch_timeout: Duration::from_secs(100),
                poll: Duration::ZERO,
                pid: 77,
            },
        )
        .unwrap();
        assert_eq!(report["outcome"], "installed", "{report}");
        assert_eq!(report["commit"], commit);
        assert_eq!(report["e2e"]["status"], "passed");
        assert_eq!(report["supervisors"][0]["token"], "first");
        assert_eq!(report["supervisors"][0]["pid"], 1);
    }
    let events = steps.events.lock().unwrap();
    let installed: Vec<_> = events
        .iter()
        .filter(|(kind, _)| kind == "update_installed")
        .map(|(_, payload)| payload["commit"].as_str().unwrap())
        .collect();
    assert_eq!(installed, ["seed", "source", "restored-source"]);
    for kind in ["update_built", "update_e2e_passed"] {
        let commits: Vec<_> = events
            .iter()
            .filter(|(k, _)| k == kind)
            .map(|(_, p)| p["commit"].as_str().unwrap())
            .collect();
        assert_eq!(commits, ["seed", "source", "restored-source"]);
    }
    assert!(!events.iter().any(|(kind, _)| kind == "update_failed"));
    let shown = status(
        &steps.registrations.lock().unwrap(),
        &recorded(&events),
        &OnlyOne,
        10_000,
    );
    assert_eq!(shown["enabled"], true);
    assert_eq!(shown["state"], "installed");
    assert_eq!(shown["commit"], "restored-source");
    let calls = steps.calls.lock().unwrap();
    for commit in ["seed", "source", "restored-source"] {
        assert!(calls.contains(&format!("checkout /q/update/checkout {commit}")));
    }
    assert_eq!(calls.iter().filter(|c| c.as_str() == "replace").count(), 3);
    assert_eq!(calls.iter().filter(|c| c.as_str() == "hand_off").count(), 3);
    assert!(!calls.iter().any(|c| c == "restore"));
    assert!(steps.asks.lock().unwrap().is_empty());
}

#[test]
fn a_failed_manual_handoff_restores_and_asks_without_changing_auto_update() {
    let processes = Stops(Arc::new(std::sync::atomic::AtomicBool::new(true)));
    let steps = Arc::new(InstallSteps {
        handoff: Handoff::Failed,
        dies_on_handoff: Some(processes.0.clone()),
        ..Default::default()
    });
    let files = MemoryFiles::default();
    files.put(Path::new("/queue"), UNIX_EPOCH, "queue");
    files.put(Path::new("/to"), UNIX_EPOCH, "old");
    let queues = |_: &Path| -> Arc<dyn QueueOpener> { Arc::new(InstallQueue(steps.clone())) };
    let error = install_watched(
        &JobPorts {
            binaries: &*steps,
            files: &files,
            processes: &processes,
            clock: &At::new(10_000),
            queues: &queues,
            restart: &|_| Ok(json!({"started": true})),
            is_ancestor: &|_, _| bail!("manual install has no commit"),
        },
        &|| bail!("no drain"),
        None,
        Some(Path::new("/queue")),
        &InstallOptions {
            source: Source::Binary("/source".into()),
            target: "/to".into(),
            allow_breaking: false,
            restart: vec![],
            handoff_timeout: Duration::ZERO,
            poll: Duration::ZERO,
            e2e: E2eGate::NotApplicable,
        },
        Duration::ZERO,
    )
    .unwrap_err();
    let failed = InstallFailed::of(&error).unwrap();
    assert!(failed.message.contains("update_failed"));
    let report = &failed.report;
    assert_eq!(report["source"], "install");
    assert_eq!(report["stage"], "install");
    assert_eq!(report["kept"], false);
    assert_eq!(report["restored"]["restored"], true);
    assert!(steps.calls.lock().unwrap().iter().any(|c| c == "restore"));
    let asks = steps.asks.lock().unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(report["ask_id"], asks[0].id.as_i64());
    assert!(asks[0].question.contains("A person's `dagq install`"));
    let events = steps.events.lock().unwrap();
    assert_eq!(
        events.iter().filter(|(k, _)| k == "update_failed").count(),
        1
    );
    assert!(!events.iter().any(|(k, _)| k == "update_restored"));
    let updates = recorded(&events);
    assert!(latest_job_step(&updates).is_none());
    assert!(!retry_requested(&updates));
    assert_eq!(status(&[], &updates, &OnlyOne, 10_000)["state"], "idle");
}

#[test]
fn rollback_refuses_a_missing_previous_and_installs_the_previous_when_present() {
    let steps = Arc::new(InstallSteps {
        heartbeats: true,
        ..Default::default()
    });
    let files = MemoryFiles::default();
    let queues = |_: &Path| -> Arc<dyn QueueOpener> { Arc::new(InstallQueue(steps.clone())) };
    let ports = install::Ports {
        binaries: &*steps,
        files: &files,
        processes: &OnlyOne,
        clock: &At::new(10_000),
        queues: &queues,
        down: &|| bail!("no drain"),
        stop_records: None,
    };
    let options = InstallOptions {
        source: Source::Rollback,
        target: "/to".into(),
        allow_breaking: false,
        restart: vec![],
        handoff_timeout: Duration::ZERO,
        poll: Duration::ZERO,
        e2e: E2eGate::NotApplicable,
    };
    let error = install::built(&ports, None, &options).err().unwrap();
    assert!(error.to_string().contains("no previous binary"));
    assert!(steps.calls.lock().unwrap().is_empty());
    files.put(Path::new("/to.previous"), UNIX_EPOCH, "previous");
    let built = install::built(&ports, None, &options).unwrap();
    assert_eq!(built.binary, Path::new("/to.previous"));
    let report = install::install_built(&ports, None, &options, built).unwrap();
    assert_eq!(report["outcome"], "installed");
    assert_eq!(report["previous"], "/to.previous");
    assert_eq!(
        *steps.replacement_sources.lock().unwrap(),
        [PathBuf::from("/to.previous")]
    );
    assert_eq!(
        steps
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.as_str() == "replace")
            .count(),
        1
    );
}

#[test]
fn a_broken_landing_restores_before_recording_failure_and_asks_the_inbox() {
    let processes = Stops(Arc::new(std::sync::atomic::AtomicBool::new(true)));
    let steps = Arc::new(InstallSteps {
        handoff: Handoff::Failed,
        dies_on_handoff: Some(processes.0.clone()),
        previous_version: Some("old".into()),
        ..Default::default()
    });
    let files = MemoryFiles::default();
    files.put(Path::new("/queue"), UNIX_EPOCH, "queue");
    files.put(Path::new("/to"), UNIX_EPOCH, "old");
    let queues = |_: &Path| -> Arc<dyn QueueOpener> { Arc::new(InstallQueue(steps.clone())) };
    let ports = JobPorts {
        binaries: &*steps,
        files: &files,
        processes: &processes,
        clock: &At::new(10_000),
        queues: &queues,
        restart: &|_| Ok(json!({"started": true})),
        is_ancestor: &|_, _| bail!("no settled failure"),
    };
    let token = LeaseToken::new("first");
    let job = Job {
        subject: Subject::Commit("broken"),
        token: Some(&token),
        target: Path::new("/to"),
        staged: Path::new("/staged"),
        log: None,
        restart: &[],
        handoff_timeout: Duration::ZERO,
        watch_timeout: Duration::ZERO,
        poll: Duration::ZERO,
        pid: 77,
    };
    let mut queue = queues(Path::new("/queue")).open().unwrap();
    let failure = put_in_place(
        &ports,
        &mut *queue,
        Path::new("/queue"),
        &job,
        Path::new("/source"),
        PluginStep::Untouched,
    )
    .unwrap();
    assert_eq!(failure["commit"], "broken");
    assert_eq!(failure["stage"], "install");
    assert_eq!(failure["kept"], false);
    assert_eq!(failure["restored"]["restored"], true);
    assert_eq!(
        failure["supervisors"][0]["supervisor"]["state"],
        "brought_back_after_this_record"
    );
    let asks = steps.asks.lock().unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(
        serde_json::to_value(&asks[0].options).unwrap(),
        json!(["retry", "skip"])
    );
    assert!(asks[0].subject.is_none());
    assert!(asks[0].question.contains("failed at its install"));
    let calls = steps.calls.lock().unwrap();
    let restore = calls.iter().position(|c| c == "restore").unwrap();
    let record = calls
        .iter()
        .position(|c| c == "record update_failed")
        .unwrap();
    assert!(restore < record);
    let events = steps.events.lock().unwrap();
    assert!(!events.iter().any(|(kind, _)| kind == "update_installed"));
    assert_eq!(
        events
            .iter()
            .filter(|(kind, _)| kind == "update_failed")
            .count(),
        1
    );
    let shown = status(&[], &recorded(&events), &OnlyOne, 10_000);
    assert_eq!(shown["state"], "failed");
    assert_eq!(shown["commit"], "broken");
}
