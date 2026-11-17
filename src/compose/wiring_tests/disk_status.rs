//! Measurement jobs through the shared disk decision and the recorded status.
use super::{
    agent::Script,
    harness::{Wiring, connect},
};
use crate::{
    application::{
        self, ExecutionEnvironment, HostMetricsFiles, NodeSampleStore, QueueOpener, TaskReads,
        TaskStore,
    },
    domain::{
        AskKind, AskReason, ClaimOutcome, EventKind, LeaseToken, NewAsk, RunId,
        environment::{EnvironmentAttached, EnvironmentWait, Resources, RunProcesses},
        host_metrics::HostSample,
    },
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime},
};

#[derive(Default)]
struct Metrics {
    held_seen: AtomicBool,
}
impl HostMetricsFiles for Metrics {
    fn record(
        &self,
        sample: &HostSample,
        _: i64,
        _: Duration,
        _: u32,
        _: Option<bool>,
    ) -> Result<Vec<PathBuf>> {
        if sample.values.get("held_target_bytes") == Some(&19_327_352_832.0) {
            self.held_seen.store(true, Ordering::SeqCst);
        }
        Ok(vec![])
    }
    fn files(&self) -> Result<Vec<(String, u64)>> {
        Ok(vec![])
    }
    fn read_file(&self, _: &str) -> Result<(Vec<HostSample>, u64)> {
        unreachable!()
    }
}
impl QueueOpener<dyn NodeSampleStore + Send> for Metrics {
    fn open(&self) -> Result<Box<dyn NodeSampleStore + Send>> {
        anyhow::bail!("no sample store in this wiring")
    }
}
impl ExecutionEnvironment for Metrics {
    fn attached(&self) -> EnvironmentAttached {
        unreachable!()
    }
    fn measures_from_outside(&self) -> bool {
        true
    }
    fn waits(&self) -> &'static [EnvironmentWait] {
        &[]
    }
    fn measure_runs(&self, _: &[RunProcesses]) -> Result<Vec<(RunId, Resources)>> {
        Ok(vec![])
    }
    fn forget_run(&self, _: &RunId) {}
    fn node_resources(&self, _: &HostSample, _: Duration) -> Resources {
        Resources::default()
    }
}

/// Move only measurement time; queue heartbeats keep their stopped wall clock.
struct MeasureClock {
    base: Arc<dyn application::Clock>,
    seconds: Arc<AtomicU64>,
}
impl application::Clock for MeasureClock {
    fn system_time(&self) -> SystemTime {
        self.base.system_time()
    }
    fn monotonic(&self) -> Instant {
        self.base.monotonic() + Duration::from_secs(self.seconds.load(Ordering::SeqCst))
    }
}

fn row(status: &Value, task: crate::domain::TaskId) -> &Value {
    status["claim_hold"]["disk_status"]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["task"] == json!(task))
        .unwrap()
}

#[test]
fn measurements_reach_the_recorded_human_and_json_status() {
    const G: u64 = 1 << 30;
    let mut w = Wiring::new(
        "measurements_reach_the_recorded_human_and_json_status",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    )
    .with_parallel(3)
    .with_tick(Duration::from_millis(1))
    .with_repository_startup_file(Arc::new(|| {
        let config = crate::infrastructure::run_env::parse_config(
            "[disk]\nestimate = \"fixed\"\nrun_size = \"20GiB\"\nwaiting_build_cleanup = \"never\"\n",
        )?;
        Ok(application::supervise::repository_config::StartupRules {
            stall: config.stall,
            disk: config.disk,
            resume: config.resume,
            review_jobs: config.review_jobs,
        })
    }));
    let seconds = Arc::new(AtomicU64::new(0));
    let clock = Arc::new(MeasureClock {
        base: w.generators().clock,
        seconds: seconds.clone(),
    });
    w = w.with_clock(clock);
    // Restored preconditions: one run awaiting approval and one worker of
    // another live supervisor. This test exercises the measurement/status
    // pass, without running workers, reviews or a landing to prepare it.
    let waiting = w.ready_task(&[]);
    let mut queue = w.queue();
    let ClaimOutcome::Claimed { run: waiting_run } = queue.claim(&w.git.main()).unwrap() else {
        panic!("claim waiting run")
    };
    let connection = connect(&w.db);
    connection.execute("UPDATE task_runs SET status='awaiting_integration', worktree_path='/waiting/worktree' WHERE id=?1", [waiting_run.id().as_str()]).unwrap();
    queue
        .record_runtime_event(
            waiting_run.id(),
            EventKind::RunPhaseChanged,
            json!({"phase":"landing_answer"}),
        )
        .unwrap();
    queue
        .ask(NewAsk {
            kind: AskKind::ApproveLanding,
            task_id: Some(waiting),
            run_id: Some(waiting_run.id().clone()),
            question: "approve this waiting run".into(),
            options: vec!["land".into()],
            asked_by: "test".into(),
            reason_category: AskReason::Scope,
            topics: vec![],
            recommendation: None,
            confidence: None,
            finding_id: None,
            request_id: None,
        })
        .unwrap();
    let waiting_run = queue.show(waiting).unwrap().runs[0].clone();
    let worker = w.ready_task(&[]);
    let owner = LeaseToken::new("measurement-owner");
    queue
        .register_supervisor(&owner, std::process::id(), 1, crate::VERSION)
        .unwrap();
    let ClaimOutcome::Claimed { run: worker_run } =
        queue.claim_for_supervisor(&w.git.main(), &owner).unwrap()
    else {
        panic!("claim worker run")
    };
    connection
        .execute(
            "UPDATE task_runs SET status='running', worktree_path='/worker/worktree' WHERE id=?1",
            [worker_run.id().as_str()],
        )
        .unwrap();
    queue
        .record_runtime_event(
            worker_run.id(),
            EventKind::RunPhaseChanged,
            json!({"phase":"worker"}),
        )
        .unwrap();
    // A ready third task and an available slot exercise hold_claims even
    // while the two restored runs occupy the other lanes.
    let candidate = w.ready_task(&[]);
    w.free.store(40 * G, Ordering::SeqCst);
    let metrics = Arc::new(Metrics::default());
    let clock = w.generators().clock;
    let measurement_at = clock.monotonic();
    let timestamp = i64::try_from(
        clock
            .system_time()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let waiting_path = PathBuf::from(waiting_run.worktree_path().unwrap());
    let measured_paths = Arc::new(Mutex::new(Vec::new()));
    let paths = measured_paths.clone();
    let (allow_build, build_ready) = mpsc::channel();
    let build_ready = Mutex::new(build_ready);
    let (allow_worker, worker_ready) = mpsc::channel();
    let worker_ready = Mutex::new(Some(worker_ready));
    let worker_bytes = Arc::new(AtomicU64::new(12 * G));
    let measured_bytes = worker_bytes.clone();
    let worker_path = PathBuf::from(queue.run(worker_run.id()).unwrap().worktree_path().unwrap());
    w.host_metrics = Some(application::supervise::HostMetricsPort {
        interval: Duration::ZERO,
        retention_days: 1,
        sample: Arc::new(HostSample::new),
        utc_offset: |_| 0,
        files: metrics.clone(),
        store: metrics.clone(),
        environment: metrics.clone(),
        held_targets: Some(application::supervise::HeldTargetsPort {
            // Only moving-measure time advances (61 seconds); the held
            // interval ensures this sole job is joined before stop().
            interval: Duration::from_secs(600),
            clock: clock.clone(),
            measure: Arc::new(move |path| {
                build_ready
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(60))
                    .ok()?;
                paths.lock().unwrap().push(path.to_path_buf());
                Some(18 * G)
            }),
        }),
    });
    w.moving_sizes = Arc::new(move |path: &Path| {
        if path == worker_path {
            if let Some(gate) = worker_ready.lock().unwrap().take()
                && gate.recv_timeout(Duration::from_secs(60)).is_err()
            {
                return Ok(None);
            }
            Ok(Some(measured_bytes.load(Ordering::SeqCst)))
        } else {
            Ok(None)
        }
    });
    let mut first = None;
    let mut measured = None;
    let mut grown = None;
    let mut output = None;
    w.supervise_once_while_stoppable(|queue, stop| {
        let status = w.status();
        if status["claim_hold"]["disk_status"].is_null() {
            return false;
        }
        if first.is_none() {
            // The hold begins before either off-loop job may finish.
            assert!(row(&status, waiting)["current"].is_null());
            assert!(row(&status, worker)["current"].is_null());
            first = Some(status);
            allow_build.send(()).unwrap();
            allow_worker.send(()).unwrap();
        } else if measured.is_none() {
            // A sample carrying this total proves the sole held job was
            // joined by HostMetricsWatch::pass, before any stop is requested.
            if !metrics.held_seen.load(Ordering::SeqCst)
                || row(&status, worker)["current"] != 12 * G
                || row(&status, waiting)["current"] != 18 * G
            {
                return false;
            }
            assert_eq!(row(&status, worker)["unused"], 8 * G);
            assert_eq!(status["claim_hold"]["disk_status"]["deficit"], 13 * G);
            measured = Some(status);
            worker_bytes.store(18 * G, Ordering::SeqCst);
            seconds.store(61, Ordering::SeqCst);
        } else if grown.is_none() {
            if row(&status, worker)["current"] != 18 * G {
                return false;
            }
            assert_eq!(row(&status, worker)["unused"], 2 * G);
            assert_eq!(status["claim_hold"]["disk_status"]["deficit"], 7 * G);
            grown = Some(status);
            queue
                .record_runtime_event(
                    worker_run.id(),
                    EventKind::RunPhaseChanged,
                    json!({"phase":"review"}),
                )
                .unwrap();
        } else {
            if row(&status, worker)["stage"] != "review" {
                return false;
            }
            assert_eq!(row(&status, worker)["target"], "build");
            assert_eq!(row(&status, worker)["unused"], 0);
            assert_eq!(status["claim_hold"]["disk_status"]["needs"]["remaining"], 0);
            assert_eq!(status["claim_hold"]["disk_status"]["deficit"], 5 * G);
            output = Some(status);
            stop();
            return true;
        }
        false
    });
    assert_eq!(clock.monotonic(), measurement_at + Duration::from_secs(61));
    assert_eq!(*measured_paths.lock().unwrap(), vec![waiting_path]);
    let latest = output.expect("a current breakdown after a stage change");
    assert_eq!(latest["claim_hold"]["disk_status"]["claims"], 0);
    assert_eq!(
        latest["claim_hold"]["since"],
        first.as_ref().unwrap()["claim_hold"]["since"]
    );
    let output = measured;
    assert!(w.queue().show(candidate).unwrap().runs.is_empty());
    let status = output.expect("a disk hold with both measurements");
    let hold = &status["claim_hold"];
    let events = w.queue().all_events().unwrap();
    let held: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::ClaimHeld)
        .collect();
    assert_eq!(
        held.len(),
        1,
        "refreshing status must not append hold events"
    );
    let recorded = &held[0].payload;
    assert_eq!(
        recorded["disk_status"],
        first.unwrap()["claim_hold"]["disk_status"]
    );
    assert_ne!(hold["disk_status"], recorded["disk_status"]);
    assert_eq!(
        latest["claim_hold"]["disk"],
        latest["claim_hold"]["disk_status"]["needs"]
    );
    // The ordinary human output and --json share this status value. No
    // formatter or status read measures either target again.
    let text = hold["message"].as_str().unwrap();
    let encoded: Value = serde_json::from_str(&serde_json::to_string(&status).unwrap()).unwrap();
    assert_eq!(
        encoded["claim_hold"]["disk_status"]["worker_source"],
        "configuration"
    );
    assert_eq!(
        encoded["claim_hold"]["disk_status"]["integrator_source"],
        "configuration"
    );
    let rows = encoded["claim_hold"]["disk_status"]["runs"]
        .as_array()
        .unwrap();
    for (task, bytes, target) in [(waiting, 18 * G, "build"), (worker, 12 * G, "worktree+tmp")] {
        let row = rows.iter().find(|r| r["task"] == json!(task)).unwrap();
        assert_eq!(row["current"], bytes);
        assert_eq!(row["unused"], if task == worker { 8 * G } else { 0 });
        if target == "build" {
            assert_eq!(row["stage"], "landing_answer");
            assert_eq!(row["reservation"], 18 * G);
        } else {
            assert_eq!(row["stage"], "WORKER");
            assert_eq!(row["reservation"], 20 * G);
        }
        assert_eq!(row["target"], target);
        assert_eq!(row["measured_at_ms"], timestamp);
        assert_eq!(row["stale"], false);
        assert!(text.contains(&format!("task {task}")));
        assert!(text.contains(&format!("[{target}] at {timestamp}")));
    }
    assert!(text.contains("current 18.0 GiB [build]"));
    assert!(text.contains("current 12.0 GiB [worktree+tmp]"));
    assert_eq!(hold["disk_status"]["needs"], hold["disk"]);
}
