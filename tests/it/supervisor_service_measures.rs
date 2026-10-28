//! The supervisor's measures through the queue service (ADR-t3102-3
//! decisions 3 to 5, docs/design/supervisor-lifecycle/supervise.md "queue
//! serviceの経路"): the record of the host's load and the reads of its
//! samples, and the measure of the runs' resources, reach the queue only
//! through the service as the supervisor principal's, while each test
//! refuses the queue to its own process ([`refuse_opening`]), and a
//! service that does not answer is no reason to open the queue instead.

use crate::common;
use crate::runtime_support::losing_answers::LosingAnswers;
use crate::runtime_support::{fixture, head_commit};

use std::{
    path::Path,
    sync::{Arc, atomic::Ordering},
};

use dagq::{
    application::{
        QueueOpener,
        supervisor_service::{ServiceCaller, ServiceUnreachable, SupervisorQueues, unreachable},
    },
    domain::{
        ActorContext, ActorRole, ClaimOutcome, LeaseToken,
        queue_service::Principal,
        slot_limits::{SlotFlags, SlotLimits, SupervisorConfig},
    },
    infrastructure::{queue_service::SocketCaller, sqlite::refuse_opening},
};

/// A sample of the host's load at `unix`: one and a half cores busy, 8 GiB
/// used.
fn host_load(unix: i64) -> dagq::domain::host_metrics::HostSample {
    let mut sample =
        dagq::domain::host_metrics::HostSample::new(unix).with_loads([Some(1.5), Some(2.0), None]);
    sample.set("cpu_total", Some(150.0));
    sample.set("mem_used_mb", Some(8192.0));
    sample
}

/// The supervisor's record of the host's load (its samples, the import of
/// the CSV files and the retention) and the reads of the samples go through
/// the service as the supervisor's, its process never opening the queue,
/// with the values, the marks of the files imported and the rows the
/// retention removes as the store keeps them; once the service stops, the
/// record fails as out of reach and the CSV file is written all the same,
/// still without opening the queue.
#[test]
fn the_hosts_load_is_recorded_and_read_only_through_the_service() {
    use dagq::application::{
        HostMetricsFiles, NodeSampleStore, supervise::HostMetricsPort,
        supervisor_service::SupervisorPorts,
    };
    use dagq::domain::host_metrics::{HOST_NODE, SampleSource, file_name};
    use dagq::infrastructure::{environment::HostEnvironment, host_metrics::HostMetricsDir};
    use std::time::Duration;
    const DAY: i64 = 86_400;
    const NOW: i64 = 20_500 * DAY + DAY / 2;
    let (_fixture, _repo, db) = fixture();
    let served = common::service::Served::start(&db);
    let pid = 4243;
    let caller: Arc<dyn ServiceCaller> = Arc::new(SocketCaller::new(&db));
    let queues = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    );
    let token = LeaseToken::new("supervisor-of-the-host");
    caller.sign_in(&token, pid).unwrap();
    let refused = refuse_opening(&db);
    let host = db.canonicalize().unwrap().parent().unwrap().join("host");
    let interval = Duration::from_secs(30);
    // A row of yesterday's file a supervisor kept without the store.
    HostMetricsDir(host.clone())
        .record(&host_load(NOW - DAY), 0, interval, 30, None)
        .unwrap();
    let port = |retention_days| HostMetricsPort {
        interval,
        retention_days,
        sample: Arc::new(host_load),
        utc_offset: |_| 0,
        held_targets: None,
        files: Arc::new(HostMetricsDir(host.clone())),
        store: Arc::new(SupervisorPorts {
            queues: queues.clone(),
            keep: |queue| -> Box<dyn NodeSampleStore + Send> { Box::new(queue) },
        }),
        environment: Arc::new(HostEnvironment::with(
            Some(8.0),
            Some(1 << 34),
            || None,
            || None,
        )),
    };
    let first = port(30).record(NOW, true);
    assert!(first.failures.is_empty(), "{first:?}");
    assert_eq!(first.imported, Some(1));
    let next = port(30).record(NOW + 40, false);
    assert!(next.failures.is_empty(), "{next:?}");

    let queue = queues.queue();
    let rows = queue.node_samples(HOST_NODE, 0, i64::MAX).unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| (row.sample.unix, row.local_day, row.source))
            .collect::<Vec<_>>(),
        [
            (NOW - DAY, NOW / DAY - 1, SampleSource::CsvImport),
            (NOW, NOW / DAY, SampleSource::Sampled),
            (NOW + 40, NOW / DAY, SampleSource::Sampled),
        ]
    );
    assert_eq!(rows[1].sample, host_load(NOW).as_written());
    // Each stands for the time since the row before, up to twice the
    // interval: the first sampled 60 s after yesterday's, the next 40 s.
    assert_eq!(rows[0].resources.cpu_seconds, None);
    assert_eq!(rows[1].resources.cpu_seconds, Some(90.0));
    assert_eq!(rows[2].resources.cpu_seconds, Some(60.0));
    assert_eq!(rows[1].resources.cpu_limit, Some(8.0));
    assert_eq!(rows[1].resources.memory_max_bytes, Some(8192 << 20));
    assert_eq!(queue.node_samples_after(0).unwrap(), rows);
    assert_eq!(queue.node_samples_after(rows[1].id).unwrap(), rows[2..]);
    assert_eq!(queue.node_samples(HOST_NODE, NOW, NOW).unwrap(), rows[1..2]);
    // The service answers a page of a read at most.
    let page = caller
        .call(
            dagq::domain::queue_service::UseCase::Supervisor,
            serde_json::json!({"op": "node_samples_after", "args": [0, 2]}),
        )
        .unwrap();
    assert_eq!(page.as_array().map(Vec::len), Some(2), "{page}");
    let marks = queue.imported_files(HOST_NODE).unwrap();
    assert_eq!(
        marks.keys().cloned().collect::<Vec<_>>(),
        [file_name(NOW / DAY - 1)]
    );
    // A retention of today alone removes yesterday's row and its mark.
    let kept = port(1).record(NOW + 80, false);
    assert!(kept.failures.is_empty(), "{kept:?}");
    let left = queue.node_samples(HOST_NODE, 0, i64::MAX).unwrap();
    assert_eq!(
        left.iter().map(|row| row.sample.unix).collect::<Vec<_>>(),
        [NOW, NOW + 40, NOW + 80]
    );
    assert!(queue.imported_files(HOST_NODE).unwrap().is_empty());

    drop(served);
    let lost = port(30).record(NOW + 120, false);
    assert!(
        lost.failures.iter().any(|failure| {
            failure.starts_with("record the sample in the store")
                && failure.contains("the queue service did not answer")
        }),
        "{lost:?}"
    );
    assert!(
        std::fs::read_to_string(host.join(file_name(NOW / DAY)))
            .unwrap()
            .contains(&(NOW + 120).to_string())
    );
    let error = queue.node_samples(HOST_NODE, 0, i64::MAX).unwrap_err();
    assert!(unreachable(&error), "{error:#}");
    drop(refused);
    caller.sign_out();
}

/// The record and the reads of the host's load samples are the supervisor
/// principal's alone: a worker, a review job, a landing's integrator and the
/// user are refused each of them as `authorization_denied`, each refusal
/// recorded with the refused principal as its actor (the integrator's by
/// the policy's `scheduler.supervise`, which it does not hold; the user's,
/// which the policy grants it, for holding no supervisor's registration).
/// The user and the inbox are refused them as the steps of a control-side
/// command too, though they hold `queue.read`, each refusal recorded, and
/// nothing is written.
#[test]
fn only_a_supervisor_records_and_reads_the_hosts_load_samples() {
    use common::service::{call, queue, start, token};
    use dagq::domain::queue_service::UseCase;
    use serde_json::json;
    let queue = queue();
    start(&queue);
    let columns = dagq::domain::host_metrics::value_columns().len();
    let sample = json!([1_700_000_000, vec![serde_json::Value::Null; columns]]);
    let rate = serde_json::to_value(dagq::domain::environment::NodeRate::default()).unwrap();
    let ops = [
        (
            "record_node_sample",
            json!(["host", 1, sample, rate, "sampled", 0]),
        ),
        (
            "import_node_sample_part",
            json!(["host", "metrics-x.csv", 1, [sample], {"bytes": 1, "rows": 1}]),
        ),
        ("imported_files", json!(["host"])),
        ("remove_node_samples", json!([2])),
        ("node_samples", json!(["host", 0, 10, 100])),
        ("node_samples_after", json!([0, 100])),
    ];
    let worker = token(
        &queue,
        &Principal::worker(queue.run.id(), queue.run.task_id()),
    );
    let review = token(
        &queue,
        &Principal::of(&ActorContext::review_job(queue.run.id(), 1)),
    );
    // The integrator of the run's landing, refused by the policy too.
    let integrator = token(
        &queue,
        &Principal::integrator(
            queue.run.id(),
            &LeaseToken::new("a-landing"),
            std::process::id(),
        ),
    );
    let before = common::service::events(&queue.db, "authorization_denied").len();
    let who = [
        Some(worker.as_str()),
        Some(review.as_str()),
        Some(integrator.as_str()),
        None,
    ];
    for who in who {
        for (op, args) in &ops {
            let refused = call(
                &queue,
                who,
                UseCase::Supervisor,
                json!({"op": op, "args": args}),
            );
            assert_eq!(
                refused["error"]["code"], "authorization_denied",
                "{op}: {refused}"
            );
            assert_eq!(
                refused["error"]["denied"]["capability"], "scheduler.supervise",
                "{op}: {refused}"
            );
        }
    }
    let denials = common::service::events(&queue.db, "authorization_denied");
    // Each refusal is recorded as the refused principal's own.
    assert_eq!(denials.len() - before, who.len() * ops.len());
    for role in ["integrator", "user"] {
        let of_role = denials[before..]
            .iter()
            .filter(|denial| denial["actor"]["role"] == role)
            .collect::<Vec<_>>();
        assert_eq!(of_role.len(), ops.len(), "{role}: {denials:?}");
        assert!(
            of_role
                .iter()
                .all(|denial| denial["payload"]["capability"] == "scheduler.supervise"),
            "{role}: {denials:?}"
        );
    }

    // No control-side command reads or records them either, whatever its
    // principal holds (the user's `queue.read` and `scheduler.supervise`).
    use dagq::domain::queue_service::{ControlCall, ControlCommand, SessionProcess};
    let pid = std::process::id();
    let inbox = Principal::inbox(SessionProcess {
        pid,
        start: dagq::infrastructure::adapters::session_start(pid).unwrap(),
    });
    let inbox = token(&queue, &inbox);
    let before = common::service::events(&queue.db, "authorization_denied").len();
    let commands = [
        (ControlCommand::Observe, None),
        (ControlCommand::Review, Some(queue.run.task_id())),
    ];
    for who in [None, Some(inbox.as_str())] {
        for (command, task) in commands {
            let control = ControlCall {
                command,
                task,
                yes: false,
                pid,
            };
            for (op, args) in &ops {
                let refused = call(
                    &queue,
                    who,
                    UseCase::Control,
                    json!({"control": control, "call": {"op": op, "args": args}}),
                );
                assert_eq!(
                    refused["error"]["code"], "authorization_denied",
                    "{command:?} {op}: {refused}"
                );
            }
        }
    }
    let denials = common::service::events(&queue.db, "authorization_denied");
    assert_eq!(denials.len() - before, 2 * commands.len() * ops.len());
    for role in ["inbox", "user"] {
        assert_eq!(
            denials[before..]
                .iter()
                .filter(|denial| denial["actor"]["role"] == role)
                .count(),
            commands.len() * ops.len(),
            "{role}: {denials:?}"
        );
    }
    assert!(
        denials[before..].iter().all(|denial| {
            denial["payload"]["why"]
                .as_str()
                .is_some_and(|why| why.contains("the host's load samples are the supervisor's"))
        }),
        "{denials:?}"
    );
    use dagq::application::NodeSampleStore;
    let direct = dagq::infrastructure::sqlite::SqliteQueue::open(&queue.db).unwrap();
    assert!(direct.node_samples_after(0).unwrap().is_empty());
    assert!(direct.imported_files("host").unwrap().is_empty());
}

/// The record and the reads of the runs' resources are the supervisor
/// principal's alone, as the host's load samples are: a worker, a review
/// job, a landing's integrator and the user are refused each of them as
/// `authorization_denied`, each refusal recorded with the refused
/// principal as its actor (the integrator's by the policy's
/// `scheduler.supervise`, the user's for holding no supervisor's
/// registration). The
/// user and the inbox are refused them as the steps of a control-side
/// command too, though they hold `queue.read`, each refusal recorded, and
/// nothing is written. The supervisor's own records and reads are the
/// measure's above.
#[test]
fn only_a_supervisor_records_and_reads_the_runs_resources() {
    use common::service::{call, queue, start, token};
    use dagq::domain::queue_service::UseCase;
    use serde_json::json;
    let queue = queue();
    start(&queue);
    let run = queue.run.id();
    let window = dagq::domain::environment::RunResource {
        id: 0,
        run_id: run.to_string(),
        attachment: 1,
        node_id: "host".to_owned(),
        from_ms: 1_000,
        until_ms: 31_000,
        resources: dagq::domain::environment::Resources::default(),
        last: true,
    };
    let ops = [
        ("record_run_resource", json!([window])),
        ("run_resources", json!([run, 0, 10])),
        ("run_resources_after", json!([0, 10])),
        ("open_attachments", json!([])),
    ];
    let worker = token(&queue, &Principal::worker(run, queue.run.task_id()));
    let review = token(&queue, &Principal::of(&ActorContext::review_job(run, 1)));
    let integrator = token(
        &queue,
        &Principal::integrator(run, &LeaseToken::new("a-landing"), std::process::id()),
    );
    let before = common::service::events(&queue.db, "authorization_denied").len();
    let who = [
        Some(worker.as_str()),
        Some(review.as_str()),
        Some(integrator.as_str()),
        None,
    ];
    for who in who {
        for (op, args) in &ops {
            let refused = call(
                &queue,
                who,
                UseCase::Supervisor,
                json!({"op": op, "args": args}),
            );
            assert_eq!(
                refused["error"]["code"], "authorization_denied",
                "{op}: {refused}"
            );
            assert_eq!(
                refused["error"]["denied"]["capability"], "scheduler.supervise",
                "{op}: {refused}"
            );
        }
    }
    let denials = common::service::events(&queue.db, "authorization_denied");
    assert_eq!(denials.len() - before, who.len() * ops.len());
    for role in ["integrator", "user"] {
        let of_role: Vec<_> = denials[before..]
            .iter()
            .filter(|denial| denial["actor"]["role"] == role)
            .collect();
        assert_eq!(of_role.len(), ops.len(), "{role}: {denials:?}");
        assert!(
            of_role
                .iter()
                .all(|denial| denial["payload"]["capability"] == "scheduler.supervise"),
            "{role}: {denials:?}"
        );
    }

    use dagq::domain::queue_service::{ControlCall, ControlCommand, SessionProcess};
    let pid = std::process::id();
    let inbox = Principal::inbox(SessionProcess {
        pid,
        start: dagq::infrastructure::adapters::session_start(pid).unwrap(),
    });
    let inbox = token(&queue, &inbox);
    let before = common::service::events(&queue.db, "authorization_denied").len();
    let commands = [
        (ControlCommand::Observe, None),
        (ControlCommand::Review, Some(queue.run.task_id())),
    ];
    for who in [None, Some(inbox.as_str())] {
        for (command, task) in commands {
            let control = ControlCall {
                command,
                task,
                yes: false,
                pid,
            };
            for (op, args) in &ops {
                let refused = call(
                    &queue,
                    who,
                    UseCase::Control,
                    json!({"control": control, "call": {"op": op, "args": args}}),
                );
                assert_eq!(
                    refused["error"]["code"], "authorization_denied",
                    "{command:?} {op}: {refused}"
                );
            }
        }
    }
    let denials = common::service::events(&queue.db, "authorization_denied");
    assert_eq!(denials.len() - before, 2 * commands.len() * ops.len());
    assert!(
        denials[before..].iter().all(|denial| {
            denial["payload"]["why"]
                .as_str()
                .is_some_and(|why| why.contains("the runs' resources are the supervisor's"))
        }),
        "{denials:?}"
    );
    use dagq::application::RunResourceStore;
    let direct = dagq::infrastructure::sqlite::SqliteQueue::open(&queue.db).unwrap();
    assert!(direct.run_resources_after(0, 10).unwrap().is_empty());
}

/// A caller of the service that answers none of the parts of an import
/// after the first `answered` of them, as a service gone part way.
struct ImportStops {
    inner: Arc<dyn ServiceCaller>,
    answered: usize,
    parts: std::sync::atomic::AtomicUsize,
}

impl ServiceCaller for ImportStops {
    fn call(
        &self,
        use_case: dagq::domain::queue_service::UseCase,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        if params["op"] == "import_node_sample_part" {
            let request = serde_json::to_vec(&params).unwrap().len();
            assert!(
                request < dagq::domain::queue_service::MAX_REQUEST_BYTES,
                "{request}"
            );
            if self.parts.fetch_add(1, Ordering::SeqCst) >= self.answered {
                return Err(ServiceUnreachable::not_done("stopped for the test").into());
            }
        }
        self.inner.call(use_case, params)
    }
    fn with_token(&self, file: &Path) -> anyhow::Result<Arc<dyn ServiceCaller>> {
        self.inner.with_token(file)
    }
    fn sign_in(&self, registration: &LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.sign_in(registration, pid)
    }
    fn take_back(&self, registration: &LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.take_back(registration, pid)
    }
    fn hand_over(&self) {
        self.inner.hand_over();
    }
    fn sign_out(&self) {
        self.inner.sign_out();
    }
    fn credential(&self) -> Option<std::path::PathBuf> {
        self.inner.credential()
    }
}

/// A CSV file whose samples are past the request's limit is imported
/// through the service in parts, each within the limit, the file marked
/// with the last only: an import stopped part way leaves the file
/// unmarked with the parts before kept, and the next import reads it again
/// whole and adds the rest, none twice, and marks it with its bytes and
/// rows.
#[test]
fn a_file_past_the_requests_limit_is_imported_in_parts_and_one_stopped_adds_the_rest() {
    use dagq::application::{
        NodeSampleStore, supervise::HostMetricsPort, supervisor_service::SupervisorPorts,
    };
    use dagq::domain::host_metrics::{
        HOST_NODE, HostSample, SampleSource, file_name, header, value_columns,
    };
    use dagq::infrastructure::{environment::HostEnvironment, host_metrics::HostMetricsDir};
    use std::time::Duration;
    const DAY: i64 = 86_400;
    const NOW: i64 = 20_500 * DAY + DAY / 2;
    const ROWS: i64 = 2500;
    let (_fixture, _repo, db) = fixture();
    let _served = common::service::Served::start(&db);
    let pid = 4244;
    let socket: Arc<dyn ServiceCaller> = Arc::new(SocketCaller::new(&db));
    let token = LeaseToken::new("supervisor-importing");
    socket.sign_in(&token, pid).unwrap();
    let refused = refuse_opening(&db);
    // Yesterday's file, every 10 s, each value wide.
    let host = db.canonicalize().unwrap().parent().unwrap().join("host");
    std::fs::create_dir_all(&host).unwrap();
    let start = (NOW / DAY - 1) * DAY;
    let mut text = header();
    for index in 0..ROWS {
        let mut sample = HostSample::new(start + 10 * index);
        for column in value_columns() {
            sample.set(column, Some(1_234_567_890_123.25));
        }
        text.push('\n');
        text.push_str(&sample.row(0));
    }
    text.push('\n');
    let path = host.join(file_name(NOW / DAY - 1));
    std::fs::write(&path, &text).unwrap();
    let length = u64::try_from(text.len()).unwrap();
    // Even as the file keeps them, past one request.
    assert!(text.len() > dagq::domain::queue_service::MAX_REQUEST_BYTES);
    let port = |caller: Arc<dyn ServiceCaller>| HostMetricsPort {
        interval: Duration::from_secs(10),
        retention_days: 30,
        sample: Arc::new(host_load),
        utc_offset: |_| 0,
        held_targets: None,
        files: Arc::new(HostMetricsDir(host.clone())),
        store: Arc::new(SupervisorPorts {
            queues: SupervisorQueues::new(
                caller,
                ActorContext::instance(ActorRole::Supervisor, pid),
            ),
            keep: |queue| -> Box<dyn NodeSampleStore + Send> { Box::new(queue) },
        }),
        environment: Arc::new(HostEnvironment::with(
            Some(8.0),
            Some(1 << 34),
            || None,
            || None,
        )),
    };
    let queue = SupervisorQueues::new(
        socket.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    )
    .queue();
    let imported = || {
        queue
            .node_samples(HOST_NODE, 0, i64::MAX)
            .unwrap()
            .into_iter()
            .filter(|row| row.source == SampleSource::CsvImport)
            .count()
    };

    let stopping = Arc::new(ImportStops {
        inner: socket.clone(),
        answered: 1,
        parts: Default::default(),
    });
    let stopped = port(stopping.clone()).record(NOW, true);
    assert_eq!(stopped.imported, None);
    assert!(
        stopped
            .failures
            .iter()
            .any(|failure| failure.starts_with("import the CSV files")),
        "{stopped:?}"
    );
    assert_eq!(stopping.parts.load(Ordering::SeqCst), 2);
    let kept = imported();
    assert!(kept > 0 && kept < ROWS as usize, "{kept}");
    assert!(queue.imported_files(HOST_NODE).unwrap().is_empty());

    let whole = Arc::new(ImportStops {
        inner: socket.clone(),
        answered: usize::MAX,
        parts: Default::default(),
    });
    let again = port(whole.clone()).record(NOW + 10, true);
    assert!(again.failures.is_empty(), "{again:?}");
    assert_eq!(again.imported, Some(ROWS as usize - kept));
    assert!(whole.parts.load(Ordering::SeqCst) > 2);
    assert_eq!(imported(), ROWS as usize);
    assert_eq!(
        queue.imported_files(HOST_NODE).unwrap()[&file_name(NOW / DAY - 1)],
        length
    );
    drop(refused);
    let rows: (i64, i64) = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT bytes, rows FROM node_sample_imports WHERE file=?1",
            [file_name(NOW / DAY - 1)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(rows, (i64::try_from(length).unwrap(), ROWS));
    socket.sign_out();
}

/// The measure of the runs' resources goes through the service on its
/// socket, as the supervisor principal's, with no path of the queue and no
/// connection of its own: a pass on a run of the slots, attached and
/// running its wrapper, records its window, the next one after it left the
/// slots its last, and the attachments it reads open are the queue's
/// ([`dagq::application::RunResourceStore`]), while this process refuses
/// to open the queue. A write whose answer is lost after the service did
/// it is taken up from the windows stored: the last window goes on from
/// the one written, and once written it is not written again. Once the
/// service stops, the pass fails as out of reach and records nothing,
/// still without opening the queue.
#[test]
fn the_measure_of_the_runs_resources_reaches_the_queue_only_through_the_service() {
    use dagq::application::supervise::{Measured, ResourcesPort, measure_pass};
    use dagq::application::supervisor_service::SupervisorPorts;
    use dagq::application::{ExecutionEnvironment, ResourceQueue};
    use dagq::domain::environment::RunChildren;

    let (_fixture, repo, db) = fixture();
    let served = common::service::Served::start(&db);
    let pid = 4243;
    // The answers of the first two windows and of the last are lost after
    // the service wrote them.
    let losing = Arc::new(LosingAnswers {
        inner: Arc::new(SocketCaller::new(&db)),
        lose: std::sync::Mutex::new(vec![&["record_run_resource"]; 3]),
        lost: std::sync::Mutex::new(Vec::new()),
    });
    let caller: Arc<dyn ServiceCaller> = losing.clone();
    let queues = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    );
    let refused = refuse_opening(&db);
    let token = LeaseToken::new("supervisor-measuring");
    caller.sign_in(&token, pid).unwrap();
    let mut queue = queues.open().unwrap();
    let limits = SlotLimits::resolve(SlotFlags::default(), &SupervisorConfig::default());
    queue
        .register_supervisor(&token, pid, limits, &[], "0.0.0-test")
        .unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&head_commit(&repo), &token)
        .unwrap()
    else {
        panic!("no task was claimed");
    };
    let id = run.id().clone();
    queue
        .plan_run(
            &id,
            &token,
            &dagq::domain::RunPlan {
                repo_path: "/test".into(),
                run_dir: format!("/run/{id}"),
                branch: format!("dagq/{id}"),
                worktree_path: format!("/run/{id}/worktree"),
                receipt_path: format!("/run/{id}/receipt.json"),
                log_path: format!("/run/{id}/log"),
            },
        )
        .unwrap();
    queue.workspace_created(&id, &token, "ws").unwrap();
    queue
        .register_wrapper(&id, &token, std::process::id())
        .unwrap();
    let host = dagq::infrastructure::environment::HostEnvironment::new();
    queue
        .record_runtime_event(
            &id,
            dagq::domain::EventKind::EnvironmentAttached,
            host.attached().payload(),
        )
        .unwrap();

    let port = ResourcesPort {
        interval: std::time::Duration::from_secs(30),
        environment: Arc::new(host),
        store: Arc::new(SupervisorPorts {
            queues: queues.clone(),
            keep: |queue| -> Box<dyn ResourceQueue + Send> { Box::new(queue) },
        }),
        children: RunChildren::default(),
    };
    let store = port.store.open().unwrap();
    let attachment = store.open_attachments().unwrap();
    assert_eq!(attachment.len(), 1, "{attachment:?}");
    assert_eq!(attachment[0].0, id);
    let mut tracked = std::collections::BTreeMap::new();
    let now = 1_900_000_000_000;
    let pass = |tracked: &mut _, held: &[dagq::domain::RunId], at: i64| {
        measure_pass(
            &*store,
            &*port.environment,
            &port.children,
            tracked,
            held,
            at,
            &|_| true,
        )
    };
    // Written, its answer lost: the next pass, the run still in the slots,
    // goes on from the window stored, the one after the run left them
    // closes it from the window stored, and that answer lost too, the one
    // after it writes no other last.
    let lost = |measured: Measured| {
        assert_eq!((measured.windows, measured.closed), (0, 0));
        assert!(
            measured
                .failure
                .is_some_and(|failure| failure.contains("was lost"))
        );
    };
    lost(pass(&mut tracked, std::slice::from_ref(&id), now));
    lost(pass(&mut tracked, std::slice::from_ref(&id), now + 500));
    lost(pass(&mut tracked, &[], now + 1_000));
    assert_eq!(*losing.lost.lock().unwrap(), ["record_run_resource"; 3]);
    let after = pass(&mut tracked, &[], now + 1_500);
    assert_eq!((after.windows, after.closed, after.failure), (0, 0, None));
    assert!(tracked.is_empty(), "{tracked:?}");
    let windows = store.run_resources(&id).unwrap();
    assert_eq!(windows.len(), 3, "{windows:?}");
    assert!(
        windows
            .iter()
            .all(|window| window.attachment == attachment[0].1)
    );
    assert!(
        windows
            .windows(2)
            .all(|pair| pair[1].from_ms >= pair[0].until_ms),
        "{windows:?}"
    );
    assert_eq!(
        [windows[1].from_ms, windows[1].until_ms, windows[2].from_ms],
        [now, now + 500, now + 500],
        "{windows:?}"
    );
    assert!(
        !windows[0].last && !windows[1].last && windows[2].last,
        "{windows:?}"
    );
    // All measured, the last one too.
    assert!(
        windows.iter().all(|window| window
            .resources
            .memory_max_bytes
            .is_some_and(|bytes| bytes > 0)),
        "{windows:?}"
    );
    assert_eq!(
        store.run_resources_after(windows[0].id, 10).unwrap(),
        windows[1..]
    );
    // The service answers a page of the run's windows after the one named.
    let page = caller
        .call(
            dagq::domain::queue_service::UseCase::Supervisor,
            serde_json::json!({"op": "run_resources", "args": [id, windows[0].id, 1]}),
        )
        .unwrap();
    assert_eq!(page, serde_json::to_value(&windows[1..2]).unwrap());
    assert!(store.open_attachments().unwrap().is_empty());

    drop(served);
    let unanswered = pass(&mut tracked, std::slice::from_ref(&id), now + 2_000);
    assert_eq!((unanswered.windows, unanswered.closed), (0, 0));
    let failure = unanswered.failure.unwrap();
    assert!(failure.contains("did not answer"), "{failure}");
    let error = store.open_attachments().unwrap_err();
    assert!(unreachable(&error), "{error:#}");
    drop(refused);
    caller.sign_out();
}
