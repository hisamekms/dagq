//! What builds the ledger through the queue service (ADR-t3102-3
//! decisions 3 to 5, docs/design/supervisor-lifecycle/supervise.md "queue
//! serviceの経路", docs/design/measurement.md "台帳を作る係"): its reads of
//! the events, the samples, the steps and their owners and the runs'
//! resources, its writes of the rows and its retention reach the queue only
//! through the service as the supervisor principal's, while the test
//! refuses the queue to its own process ([`refuse_opening`]); the rows and
//! the retention are those the same passes make on the queue's own
//! connection, and a service that does not answer is no reason to open the
//! queue instead.

use crate::common;
use crate::runtime_support::{fixture, head_commit};

use std::sync::Arc;

use dagq::{
    application::{
        LedgerQueue, LedgerStore, NodeSampleStore, QueueOpener, SessionOwners,
        SessionResourceStore, SessionStepStore,
        supervise::{Built, LedgerPort, Retain, build_pass},
        supervisor_service::{ServiceCaller, SupervisorPorts, SupervisorQueues, unreachable},
    },
    domain::{
        ActorContext, ActorRole, ClaimOutcome, LeaseToken,
        ledger::{LedgerKind, LedgerRow, builder::Builder},
        queue_service::Principal,
        session_step::{ResourceSample, Step, StepKind, StepTarget},
        slot_limits::{SlotFlags, SlotLimits, SupervisorConfig},
    },
    infrastructure::{
        queue_service::SocketCaller,
        sqlite::{SqliteQueue, refuse_opening},
    },
};

const BUILT: &str = "2030-01-01T00:00:00.000Z";

/// Every row of every kind `store` holds.
fn rows(store: &dyn LedgerQueue) -> Vec<LedgerRow> {
    LedgerKind::ALL
        .into_iter()
        .flat_map(|kind| store.ledger_rows(kind).unwrap())
        .collect()
}

/// Two passes at `now_ms` and an hour later, the second with `retain`.
fn passes(store: &dyn LedgerQueue, now_ms: i64, retain: Retain) -> (Built, Built) {
    let mut builder = Builder::default();
    let first = build_pass(
        store,
        &mut builder,
        (now_ms, BUILT),
        Retain::default(),
        None,
    );
    let second = build_pass(
        store,
        &mut builder,
        (now_ms + 3_600_000, BUILT),
        retain,
        None,
    );
    (first, second)
}

/// The ledger's builder on a [`LedgerPort`] over the service as the
/// supervisor, given no queue's path and no connection of its own, writes
/// the run's (its session's steps and samples folded), the task's, the
/// queue's and the node's rows and removes the node rows and the steps past their retention as the
/// same passes on the queue's own connection do, while this process
/// refuses to open the queue. A write over a row another writer wrote
/// since it was read writes nothing. Once the service stops, the pass fails
/// as out of reach, still without opening the queue, and writes nothing.
#[test]
fn the_ledger_is_built_only_through_the_service_as_on_the_queues_own_connection() {
    let (fixture, repo, db) = fixture();
    let served = common::service::Served::start(&db);
    let pid = 4244;
    let caller: Arc<dyn ServiceCaller> = Arc::new(SocketCaller::new(&db));
    let queues = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    );
    let token = LeaseToken::new("supervisor-building-the-ledger");
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
    for phase in ["worker", "ended"] {
        queue
            .record_runtime_event(
                &id,
                dagq::domain::EventKind::RunPhaseChanged,
                serde_json::json!({"phase": phase, "blocker": "ai", "holds": "worker_slot",
                       "attempt": {"kind": "first", "n": 1}, "cause": "x", "v": 1}),
            )
            .unwrap();
    }
    // The run's session, its steps and samples, and the host's load: the
    // records of the executor's and the host's, which the supervisor only
    // reads.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let at = now_ms - 600_000;
    {
        let direct = SqliteQueue::open(&db).unwrap();
        let target = StepTarget::run(&id);
        direct.register_session(&target, "s1", at).unwrap();
        direct
            .record_steps(
                &target,
                "s1",
                &[
                    Step::new(1, StepKind::ModelTurn, 0, 60_000),
                    Step::new(2, StepKind::ToolCall, 60_000, 30_000),
                ],
                at + 120_000,
            )
            .unwrap();
        direct
            .record_resources(
                &target,
                "s1",
                &[ResourceSample::new(1, 0, 30_000)],
                at + 120_000,
            )
            .unwrap();
        let unix = at / 1000 - 7_200;
        direct
            .record_node_sample(
                dagq::domain::host_metrics::HOST_NODE,
                unix / 86_400,
                &dagq::domain::host_metrics::HostSample::new(unix),
                &dagq::domain::environment::NodeRate::default(),
                dagq::domain::host_metrics::SampleSource::Sampled,
                0,
            )
            .unwrap();
    }
    // The same queue, for the passes on its own connection.
    let copy = fixture.dir.path().join("copy.db");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("VACUUM INTO ?1", [copy.to_str().unwrap()])
        .unwrap();
    let refused = refuse_opening(&db);
    let retain = Retain {
        node_rows: Some(now_ms / 86_400_000 + 1),
        steps: Some(now_ms + 3_600_000),
    };

    let port = LedgerPort {
        interval: std::time::Duration::from_secs(60),
        retention_days: Some(1),
        utc_offset: |_| 0,
        store: Arc::new(SupervisorPorts {
            queues: queues.clone(),
            keep: |queue| -> Box<dyn LedgerQueue + Send> { Box::new(queue) },
        }),
        classifier: Arc::new(|| None),
    };
    let store = port.store.open().unwrap();
    let (first, second) = passes(&*store, now_ms, retain);
    let own = SqliteQueue::open(&copy).unwrap();
    let (own_first, own_second) = passes(&own, now_ms, retain);
    assert_eq!(first, own_first);
    assert_eq!(second, own_second);
    assert_eq!(first.failure, None, "{first:?}");
    assert!(first.written > 0, "{first:?}");
    assert_eq!(second.failure, None, "{second:?}");
    assert_eq!((second.removed, second.steps_removed), (1, 2), "{second:?}");
    let built = rows(&*store);
    assert_eq!(built, rows(&own));
    for kind in [LedgerKind::Run, LedgerKind::Task, LedgerKind::Queue] {
        assert!(
            built.iter().any(|row| row.kind == kind),
            "{kind:?}: {built:?}"
        );
    }
    let run_row = store.ledger_row(LedgerKind::Run, id.as_str()).unwrap();
    assert_eq!(
        run_row,
        own.ledger_row(LedgerKind::Run, id.as_str()).unwrap()
    );
    // The run's row folded its session's steps and samples, placed by its
    // owner's registration.
    let folded = run_row.as_ref().expect("the run's row");
    assert_eq!(
        folded.detail[dagq::domain::ledger::steps::DETAIL_KEY]["fold"]["counts"],
        serde_json::json!({"model_turn": 1, "tool_call": 1}),
        "{folded:?}"
    );
    assert!(
        folded.inputs_through.inside_resources.is_some(),
        "{folded:?}"
    );
    assert!(
        store
            .target_steps(&StepTarget::run(&id))
            .unwrap()
            .is_empty()
    );
    assert!(store.steps_after(0, 10).unwrap().is_empty());
    assert!(store.resources_after(0, 10).unwrap().is_empty());
    assert_eq!(store.session_owner("s1").unwrap(), None);

    // A write over the row read: written once, and not again over the
    // read another write has replaced.
    let read = run_row.unwrap();
    let mut over = read.clone();
    over.built_at = "2030-01-02T00:00:00.000Z".to_owned();
    assert_eq!(
        store.write_ledger_row_over(&over, Some(&read)).unwrap(),
        Some(true)
    );
    let mut stale = read.clone();
    stale.built_at = "2030-01-03T00:00:00.000Z".to_owned();
    assert_eq!(
        store.write_ledger_row_over(&stale, Some(&read)).unwrap(),
        None
    );
    assert_eq!(store.write_ledger_row_over(&stale, None).unwrap(), None);
    assert_eq!(
        store.ledger_row(LedgerKind::Run, id.as_str()).unwrap(),
        Some(over.clone())
    );
    // A row whose records are past the request's limit whole is written
    // in parts.
    let from = store.latest_event_id().unwrap();
    for _ in 0..2 {
        let events = (0..200)
            .map(|n| {
                (
                    dagq::domain::EventKind::LedgerBuildFailed,
                    serde_json::json!({"n": n, "error": "x".repeat(2_000)}),
                )
            })
            .collect();
        queue.record_queue_events(events).unwrap();
    }
    let mut big = over.clone();
    big.records = store
        .events_between(
            from,
            store.latest_event_id().unwrap(),
            &dagq::domain::EventFilter::default(),
            1_000,
        )
        .unwrap();
    assert_eq!(big.records.len(), 400);
    assert!(
        serde_json::to_vec(&big).unwrap().len()
            > dagq::domain::queue_service::MAX_REQUEST_BYTES / 2
    );
    assert_eq!(
        store.write_ledger_row_over(&big, Some(&over)).unwrap(),
        Some(true)
    );
    assert_eq!(
        store.ledger_row(LedgerKind::Run, id.as_str()).unwrap(),
        Some(big.clone())
    );
    let over = big;
    // The service answers a page of the rows at most, after the key named.
    let page = caller
        .call(
            dagq::domain::queue_service::UseCase::Supervisor,
            serde_json::json!({"op": "ledger_rows", "args": ["queue", null, 1]}),
        )
        .unwrap();
    assert_eq!(page.as_array().map(Vec::len), Some(1), "{page}");
    // What the supervisor does not send is refused on its side.
    let error = store.write_ledger_row(&over).unwrap_err();
    assert!(format!("{error:#}").contains("no operation"), "{error:#}");

    drop(served);
    let mut builder = Builder::default();
    let lost = build_pass(
        &*store,
        &mut builder,
        (now_ms + 7_200_000, BUILT),
        retain,
        None,
    );
    let failure = lost.failure.expect("a pass the service does not answer");
    assert!(failure.contains("did not answer"), "{failure}");
    let error = store.ledger_rows(LedgerKind::Run).unwrap_err();
    assert!(unreachable(&error), "{error:#}");
    drop(refused);
    let direct = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        direct.ledger_row(LedgerKind::Run, id.as_str()).unwrap(),
        Some(over)
    );
    caller.sign_out();
}

/// The operations of what builds the ledger are the supervisor
/// principal's alone, as the runs' resources are: a worker, a review job,
/// a landing's integrator and the user are refused each of them as
/// `authorization_denied` on the supervisor's use case, each refusal
/// recorded (the integrator's by the policy's `scheduler.supervise`). The user and the inbox are refused them as the
/// steps of a control-side command too, each refusal recorded, and nothing
/// is written. The supervisor's own are the builder's above.
#[test]
fn only_a_supervisor_reads_and_writes_what_builds_the_ledger() {
    use common::service::{call, queue, start, token};
    use dagq::domain::queue_service::UseCase;
    use serde_json::json;
    let queue = queue();
    start(&queue);
    let run = queue.run.id();
    let target = StepTarget::run(run);
    let row = json!({"kind": "run", "key": run.as_str()});
    let ops = [
        ("ledger_row", json!(["run", run.as_str()])),
        ("ledger_rows", json!(["run", null, 10])),
        ("write_ledger_row_over", json!([row, null])),
        ("stage_ledger_row", json!(["d", 0, 1, "{}"])),
        ("write_staged_ledger_row_over", json!(["d", null])),
        ("ledger_row_part", json!(["run", run.as_str(), 0, 10])),
        ("ledger_row_keys", json!(["run", null, 1])),
        ("remove_node_rows", json!([30_000])),
        ("steps_after", json!([0, 10])),
        ("target_steps", json!([target, 0, 10])),
        ("remove_steps", json!([i64::MAX])),
        ("target_resources", json!([target, 0, 10])),
        ("resources_after", json!([0, 10])),
        ("session_owner", json!(["s1"])),
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
        }
    }
    let denials = common::service::events(&queue.db, "authorization_denied");
    assert_eq!(denials.len() - before, who.len() * ops.len());
    let integrators: Vec<_> = denials[before..]
        .iter()
        .filter(|denial| denial["actor"]["role"] == "integrator")
        .collect();
    assert_eq!(integrators.len(), ops.len(), "{denials:?}");
    assert!(
        integrators
            .iter()
            .all(|denial| denial["payload"]["capability"] == "scheduler.supervise"),
        "{denials:?}"
    );

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
            denial["payload"]["why"].as_str().is_some_and(|why| {
                why.contains("the ledger builder's reads and writes are the supervisor's")
            })
        }),
        "{denials:?}"
    );
    let direct = SqliteQueue::open(&queue.db).unwrap();
    assert!(direct.ledger_rows(LedgerKind::Run).unwrap().is_empty());
}

/// A row of `key` of the kind `run` with `detail`, and nothing else.
fn run_row(key: &str, detail: serde_json::Value) -> LedgerRow {
    serde_json::from_value(serde_json::json!({
        "kind": "run", "key": key, "task_id": null, "ledger_v": 6,
        "built_at": BUILT, "inputs_from": 0, "inputs_through": {"events": 0},
        "is_final": false, "started_at": null, "ended_at": null, "outcome": null,
        "totals": dagq::domain::ledger::Totals::default(), "intervals": [],
        "attributes": {}, "after_claim": null, "detail": detail, "legacy": null,
        "subject": null, "records": [],
    }))
    .unwrap()
}

/// What builds the ledger reads and writes past the service's limits as it
/// did on the queue's own connection, while this process refuses to open
/// the queue: a node row past one request whole (an hour of the host's load
/// a second at a time) is built and written in parts as on the queue's own
/// connection and removed by its retention; a run's and its task's events
/// past one answer (which the service refuses whole) are read in pages; and
/// a row past one answer is written in parts and read back whole, alone and
/// among the rows of its kind.
#[test]
fn what_builds_the_ledger_crosses_the_services_limits_in_parts() {
    use dagq::application::EventStore;
    use dagq::application::supervisor_service::too_large;
    use dagq::domain::queue_service::{MAX_ANSWER_BYTES, MAX_REQUEST_BYTES, UseCase};
    let (fixture, repo, db) = fixture();
    let served = common::service::Served::start(&db);
    let pid = 4245;
    let caller: Arc<dyn ServiceCaller> = Arc::new(SocketCaller::new(&db));
    let queues = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    );
    let token = LeaseToken::new("supervisor-past-the-limits");
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
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let hour = (now_ms / 1000 - 7_200) / 3_600 * 3_600;
    let samples: Vec<_> = (hour..hour + 3_600)
        .map(|unix| {
            let mut sample = dagq::domain::host_metrics::HostSample::new(unix);
            for column in dagq::domain::host_metrics::value_columns() {
                sample.set(column, Some(1.0));
            }
            sample
        })
        .collect();
    SqliteQueue::open(&db)
        .unwrap()
        .import_node_samples(
            dagq::domain::host_metrics::HOST_NODE,
            "metrics-of-the-test.csv",
            hour / 86_400,
            &samples,
            1,
        )
        .unwrap();
    let copy = fixture.dir.path().join("copy.db");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("VACUUM INTO ?1", [copy.to_str().unwrap()])
        .unwrap();
    let refused = refuse_opening(&db);
    let store = queues.queue();
    let own = SqliteQueue::open(&copy).unwrap();
    let nodes = |store: &dyn LedgerQueue| store.ledger_rows(LedgerKind::Node).unwrap();

    let (mut builder, mut own_builder) = (Builder::default(), Builder::default());
    let built = build_pass(
        &store,
        &mut builder,
        (now_ms, BUILT),
        Retain::default(),
        None,
    );
    let own_built = build_pass(
        &own,
        &mut own_builder,
        (now_ms, BUILT),
        Retain::default(),
        None,
    );
    assert_eq!(built.failure, None, "{built:?}");
    assert_eq!(built, own_built);
    let node = nodes(&store);
    assert_eq!(node.len(), 1);
    assert_eq!(node, nodes(&own));
    let text = serde_json::to_vec(&node[0]).unwrap().len();
    assert!(text > MAX_REQUEST_BYTES, "{text}");
    let retain = Retain {
        node_rows: Some(now_ms / 86_400_000 + 1),
        steps: None,
    };
    let at = (now_ms + 3_600_000, BUILT);
    let removed = build_pass(&store, &mut builder, at, retain, None);
    assert_eq!(
        removed,
        build_pass(&own, &mut own_builder, at, retain, None)
    );
    assert_eq!((removed.failure, removed.removed), (None, 1));
    assert!(nodes(&store).is_empty());

    // Each event within a request, all of them past an answer.
    let pad = "x".repeat(MAX_REQUEST_BYTES - 100_000);
    let count = MAX_ANSWER_BYTES / pad.len() + 1;
    for n in 0..count {
        queue
            .record_runtime_event(
                &id,
                dagq::domain::EventKind::RunPhaseChanged,
                serde_json::json!({"n": n, "pad": pad}),
            )
            .unwrap();
    }
    let whole = caller
        .call(
            UseCase::Supervisor,
            serde_json::json!({"op": "run_events", "args": [id]}),
        )
        .unwrap_err();
    assert!(too_large(&whole), "{whole:#}");
    let padded = |events: &[dagq::domain::RunEvent]| {
        assert!(events.windows(2).all(|two| two[0].id < two[1].id));
        events
            .iter()
            .filter(|event| event.payload["pad"] == pad.as_str())
            .count()
    };
    assert_eq!(padded(&store.run_events(&id).unwrap()), count);
    assert_eq!(padded(&store.task_events(run.task_id()).unwrap()), count);

    // A row past an answer, and one after it by key.
    let big = run_row(
        "a-big-row",
        serde_json::json!({"pad": "é\"".repeat(MAX_ANSWER_BYTES / 4 + 1)}),
    );
    assert!(serde_json::to_vec(&big).unwrap().len() > MAX_ANSWER_BYTES);
    let small = run_row("b-small-row", serde_json::json!({}));
    assert_eq!(store.write_ledger_row_over(&big, None).unwrap(), Some(true));
    assert_eq!(
        store.write_ledger_row_over(&small, None).unwrap(),
        Some(true)
    );
    assert_eq!(
        store.ledger_row(LedgerKind::Run, "a-big-row").unwrap(),
        Some(big.clone())
    );
    let runs = store.ledger_rows(LedgerKind::Run).unwrap();
    let at = runs.iter().position(|row| row.key == "a-big-row").unwrap();
    assert_eq!(runs[at..], [big, small]);
    drop(served);
    drop(refused);
    caller.sign_out();
}
