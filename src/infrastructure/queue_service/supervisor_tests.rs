//! The supervisor's use cases on the service's side (ADR-t3102-3
//! decisions 3 and 4): each is the queue's own operation in its one
//! transaction, under the lease the principal was issued for.

use super::*;
use crate::application::TaskStore;
use crate::domain::{EventId, NewTask};

pub(super) const RUN: &str = "11111111-1111-4111-8111-111111111111";

/// A queue with one task claimed as run [`RUN`], starting, under the lease `token`,
/// and the backend of its service.
fn claimed(token: &LeaseToken) -> (tempfile::TempDir, SqliteServiceBackend) {
    claimed_with(token, crate::infrastructure::clock::system())
}

/// [`claimed`], the task added and the service's times taken from
/// `generators`.
pub(super) fn claimed_with(
    token: &LeaseToken,
    generators: crate::application::Generators,
) -> (tempfile::TempDir, SqliteServiceBackend) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let (backend, _) = claimed_at_with(token, &dir, db, generators);
    (dir, backend)
}

/// As [`claimed`], with the queue in a memdb named after `name` and a new
/// UUID, shared by every connection of the process that opens it while
/// the returned queue (which keeps it) is open; the tokens' files stay in
/// the returned directory.
pub(super) fn claimed_in_memdb(
    token: &LeaseToken,
    name: &str,
) -> (tempfile::TempDir, SqliteQueue, SqliteServiceBackend) {
    let dir = tempfile::tempdir().unwrap();
    let db = PathBuf::from(format!("file:/{name}-{}?vfs=memdb", uuid::Uuid::new_v4()));
    let (backend, kept) = claimed_at(token, &dir, db);
    (dir, kept, backend)
}

/// The queue at `db` with the task claimed as [`claimed`] makes it, the
/// queue's connection, and the backend of its service in `dir`.
fn claimed_at(
    token: &LeaseToken,
    dir: &tempfile::TempDir,
    db: PathBuf,
) -> (SqliteServiceBackend, SqliteQueue) {
    claimed_at_with(token, dir, db, crate::infrastructure::clock::system())
}

/// [`claimed_at`], the task added and the service's times taken from
/// `generators`.
fn claimed_at_with(
    token: &LeaseToken,
    dir: &tempfile::TempDir,
    db: PathBuf,
    generators: crate::application::Generators,
) -> (SqliteServiceBackend, SqliteQueue) {
    let mut queue = SqliteQueue::init(&db)
        .unwrap()
        .with_generators(generators.clone());
    let task = queue
        .add(NewTask {
            title: "t".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            priority: Default::default(),
            goal_id: None,
            context: String::new(),
            change: None,
            provider: None,
            worker_mode: None,
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap()
        .id();
    queue
        .conn
        .execute_batch(&format!(
            "UPDATE tasks SET status='in_progress' WHERE id={task};
             INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,supervisor_token)
             VALUES ('{RUN}',{task},'starting','claude','claude','{}','{token}');
             INSERT INTO run_leases(run_id,token,pid) VALUES ('{RUN}','{token}',1);",
            "a".repeat(40),
            token = token.as_str(),
        ))
        .unwrap();
    let backend = SqliteServiceBackend {
        queue_dir: dir.path().to_path_buf(),
        db,
        generators,
        reads: Arc::new(|_, _, _| Ok(Value::Null)),
        writes: Arc::new(|_, _, _| Ok(Value::Null)),
        detail: |_, _| Value::Null,
        commands: Arc::new(|_, _, _| Ok(Value::Null)),
        at_stop: None,
        stands_for: None,
        doctor: None,
        write_line: None,
        heavy_reads: None,
    };
    (backend, queue)
}

/// A token of `principal`'s for the queue of `backend`.
pub(super) fn token_of(backend: &SqliteServiceBackend, principal: &Principal) -> String {
    let issued = issue(&backend.queue_dir, principal, 0).unwrap();
    read_token(&issued.file).unwrap()
}

/// `op` with `args` as the principal of `token`.
fn call(backend: &SqliteServiceBackend, token: &str, op: &str, args: Value) -> ServiceResponse {
    QueueService {
        backend,
        build: "test",
        pid: 1,
    }
    .handle(&ServiceRequest {
        api_version: API_VERSION,
        token: Some(token.to_owned()),
        env_role: None,
        queue: None,
        binding: None,
        use_case: UseCase::Supervisor,
        params: json!({"op": op, "args": args}),
    })
}

fn workspace(backend: &SqliteServiceBackend) -> Option<String> {
    RunReads::run(
        &SqliteQueue::open(&backend.db).unwrap(),
        &RunId::new(RUN).unwrap(),
    )
    .unwrap()
    .workspace_id()
    .map(str::to_owned)
}

fn events(backend: &SqliteServiceBackend, kind: &str) -> Vec<RunEvent> {
    EventStore::latest_events_of(&SqliteQueue::open(&backend.db).unwrap(), kind, 10).unwrap()
}

fn refused(response: &ServiceResponse) -> String {
    assert!(!response.ok, "{response:?}");
    response.error.as_ref().unwrap().message.clone()
}

/// A supervisor writes a run under its own registration's lease only: one
/// that names another supervisor's lease is refused before the queue is
/// touched, and one that names its own lease of a run it does not hold is
/// refused by the queue's transaction; neither writes anything. The
/// supervisor that holds the lease writes, as its own actor.
#[test]
fn a_supervisor_writes_a_run_only_under_the_lease_its_principal_holds() {
    let (owner, other) = (LeaseToken::new("owner"), LeaseToken::new("other"));
    let (_dir, backend) = claimed(&owner);
    let owner_token = token_of(&backend, &Principal::supervisor(&owner, 7));
    let other_token = token_of(&backend, &Principal::supervisor(&other, 7));
    // Two supervisors of one process: each keeps its own token.
    assert_ne!(owner_token, other_token);

    let named_another = call(
        &backend,
        &other_token,
        "workspace_created",
        json!([RUN, "owner", "ws-1"]),
    );
    assert!(
        refused(&named_another).contains("not the supervisor principal's registration"),
        "{named_another:?}"
    );
    let not_held = call(
        &backend,
        &other_token,
        "workspace_created",
        json!([RUN, "other", "ws-1"]),
    );
    refused(&not_held);
    assert_eq!(workspace(&backend), None);
    assert!(events(&backend, "workspace_created").is_empty());

    let held = call(
        &backend,
        &owner_token,
        "workspace_created",
        json!([RUN, "owner", "ws-1"]),
    );
    assert!(held.ok, "{held:?}");
    assert_eq!(workspace(&backend).as_deref(), Some("ws-1"));
    let created = events(&backend, "workspace_created");
    assert_eq!(created.len(), 1);
    assert_eq!(
        created[0].actor.as_ref().map(|actor| actor.id.as_str()),
        Some("supervisor:7")
    );
}

/// A supervisor prunes its own registration, and another's only once its
/// heartbeat expired (one a dead supervisor left behind): a live one's is
/// refused and kept.
#[test]
fn a_supervisor_prunes_another_registration_only_once_its_heartbeat_expired() {
    let owner = LeaseToken::new("owner");
    let (_dir, backend) = claimed(&owner);
    let mut queue = SqliteQueue::open(&backend.db).unwrap();
    for token in ["live", "left"] {
        queue
            .register_supervisor(&LeaseToken::new(token), 9, 1, "test")
            .unwrap();
    }
    queue
        .conn
        .execute(
            "UPDATE supervisors SET heartbeat_at = heartbeat_at - ?1 WHERE token = 'left'",
            [crate::domain::HEARTBEAT_TIMEOUT_SECS + 60],
        )
        .unwrap();
    let owner_token = token_of(&backend, &Principal::supervisor(&owner, 7));
    let prune = |token: &str| {
        call(
            &backend,
            &owner_token,
            "prune_supervisor",
            json!([token, "supervisor_stopped", {"outcome": "pruned"}]),
        )
    };
    refused(&prune("live"));
    let pruned = prune("left");
    assert!(pruned.ok, "{pruned:?}");
    let mut left: Vec<String> = queue
        .supervisors()
        .unwrap()
        .into_iter()
        .map(|registration| registration.token.to_string())
        .collect();
    left.sort();
    assert_eq!(left, ["live"]);
}

/// Only a supervisor's principal reaches the use case: a worker's token is
/// refused as the policy refuses it, recorded, and an operation the service
/// does not know is a bad request.
#[test]
fn only_the_supervisor_principal_reaches_its_operations() {
    let owner = LeaseToken::new("owner");
    let (_dir, backend) = claimed(&owner);
    let worker = Principal::worker(&RunId::new(RUN).unwrap(), crate::domain::TaskId::new(1));
    let worker_token = token_of(&backend, &worker);
    let denied = call(&backend, &worker_token, "active_runs", json!([]));
    assert_eq!(
        denied.error.as_ref().map(|error| error.code),
        Some(ServiceErrorCode::AuthorizationDenied),
        "{denied:?}"
    );
    assert_eq!(events(&backend, "authorization_denied").len(), 1);

    let owner_token = token_of(&backend, &Principal::supervisor(&owner, 7));
    let unknown = call(&backend, &owner_token, "drop_table", json!([]));
    assert_eq!(
        unknown.error.as_ref().map(|error| error.code),
        Some(ServiceErrorCode::BadRequest),
        "{unknown:?}"
    );
    // A run the queue does not have is told apart, as the queue tells it.
    let missing = call(
        &backend,
        &owner_token,
        "run",
        json!(["22222222-2222-4222-8222-222222222222"]),
    );
    assert!(matches!(
        missing.error.as_ref().and_then(|error| error.kind.as_ref()),
        Some(crate::domain::queue_service::FailureKind::RunNotFound(_))
    ));
}

/// A supervisor of the binary before a handoff starts a diagnosis job in
/// its form (its pid, and the pids it took as gone, its own always among
/// them): a job of a registered supervisor of another token with that pid
/// (another pid namespace's) is not taken as gone by it, and the start is
/// refused; once that registration is gone, the same start takes the
/// operation over.
#[test]
fn an_older_supervisors_start_takes_no_registered_owner_of_its_pid_as_gone() {
    use crate::domain::replan_detect::JobOwner;
    use crate::domain::task_replan::{NewReplan, ReplanOrigin};
    let (_dir, backend) = claimed(&LeaseToken::new("old"));
    let mut queue = SqliteQueue::open(&backend.db).unwrap();
    let run = RunId::new(RUN).unwrap();
    let op = queue
        .record_replan(
            &NewReplan {
                run: run.clone(),
                reason: "the reviews keep failing".into(),
                evidence: Vec::new(),
            },
            ReplanOrigin::Runtime,
            "supervisor",
            "supervisor",
        )
        .unwrap()
        .operation;
    let twin = LeaseToken::new("twin");
    queue.register_supervisor(&twin, 7, 1, "test").unwrap();
    let twins = JobOwner {
        pid: 7,
        token: Some("twin".into()),
        process_start: Some("Mon 10:00".into()),
    };
    let claimed = queue
        .start_replan_diagnosis(
            (op.id, op.generation),
            "its diagnosis 1 starts",
            &run,
            (json!({"attempt": 1}), &twins, &[]),
        )
        .unwrap();
    let old = token_of(&backend, &Principal::supervisor(&LeaseToken::new("old"), 7));
    let older_start = || {
        call(
            &backend,
            &old,
            "start_replan_diagnosis",
            json!([[op.id, claimed.generation], "its diagnosis 2 starts", RUN,
                   {"attempt": 2}, 7, [7]]),
        )
    };
    let refusal = refused(&older_start());
    assert!(
        refusal.contains("one runs at a time in the queue"),
        "{refusal}"
    );
    assert_eq!(
        queue.replan_operation(op.id).unwrap().generation,
        claimed.generation
    );
    assert!(queue.deregister_supervisor(&twin).unwrap());
    let taken = older_start();
    assert!(taken.ok, "{taken:?}");
    assert_eq!(
        queue.replan_operation(op.id).unwrap().generation,
        claimed.generation + 1
    );
}

/// The service of [`call`] in this process as a supervisor's caller with
/// the token `token`, every operation it is sent kept by its name.
struct InProcess {
    backend: SqliteServiceBackend,
    token: String,
    ops: std::sync::Mutex<Vec<String>>,
}

impl crate::application::supervisor_service::ServiceCaller for InProcess {
    fn call(&self, use_case: UseCase, params: Value) -> Result<Value> {
        assert_eq!(use_case, UseCase::Supervisor);
        let op = params["op"].as_str().unwrap().to_owned();
        self.ops.lock().unwrap().push(op.clone());
        let response = call(&self.backend, &self.token, &op, params["args"].clone());
        match response.error {
            Some(error) => Err(crate::application::supervisor_service::service_error(
                &error,
            )),
            None => Ok(response.result.unwrap_or(Value::Null)),
        }
    }
    fn with_token(
        &self,
        _: &Path,
    ) -> Result<Arc<dyn crate::application::supervisor_service::ServiceCaller>> {
        unreachable!("no landing here")
    }
    fn sign_in(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn take_back(&self, _: &LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn hand_over(&self) {}
    fn sign_out(&self) {}
    fn credential(&self) -> Option<PathBuf> {
        None
    }
}

/// The session rows of three planner sessions, each opened, turned and
/// closed, and a fourth out of any session, written on the queue of
/// `backend` itself; the records between the anchors of the first are
/// made to read whole no more.
fn session_rows(backend: &SqliteServiceBackend) {
    use crate::application::LedgerStore;
    let event = |id: i64, kind: &str, payload: Value| RunEvent {
        id: crate::domain::EventId::new(id),
        task_id: None,
        goal_id: None,
        run_id: None,
        kind: kind.into(),
        payload,
        created_at: crate::domain::marks::utc_text((1_900_000_000 + id) * 1000),
        actor: None,
    };
    let mut events = Vec::new();
    for planner in 0..3_i64 {
        let opened = 1 + planner * 10;
        events.extend([
            event(
                opened,
                "session_opened",
                json!({"kind": "runtime_planner", "planner_id": planner + 1}),
            ),
            event(
                opened + 1,
                "session_turns",
                json!({"opened_event_id": opened}),
            ),
            event(
                opened + 2,
                "session_closed",
                json!({"opened_event_id": opened, "session_id": format!("s{planner}"), "model": "m"}),
            ),
        ]);
    }
    events.push(event(40, "session_closed", json!({"session_id": "out"})));
    let queue = SqliteQueue::open(&backend.db).unwrap();
    let rows = crate::domain::ledger::session::session_rows(&events, "2030-01-01T00:00:00.000Z");
    assert_eq!(rows.len(), 4);
    for row in &rows {
        queue.write_ledger_row(row).unwrap();
    }
    queue
        .conn
        .execute(
            "UPDATE ledger_rows SET records=json_set(records, '$[1]', json('{\"id\":2}'))
              WHERE kind='session' AND json_extract(records, '$[0].id') = 1",
            [],
        )
        .unwrap();
}

/// What builds the ledger reads the session rows' heads and anchors
/// through the service by their own operations, which read them as the
/// queue's own adapter does, never by the rows whole: a row with a record
/// that reads whole no more gives its head and anchors through the
/// service as on the queue's own connection, while its rows whole fail.
#[test]
fn the_heads_and_anchors_cross_as_the_queues_own_without_the_rows_whole() {
    use crate::application::LedgerStore;
    use crate::application::supervisor_service::SupervisorQueues;
    use crate::domain::ledger::LedgerKind;
    let owner = LeaseToken::new("owner");
    let (_dir, backend) = claimed(&owner);
    session_rows(&backend);
    let token = token_of(&backend, &Principal::supervisor(&owner, 7));
    let direct = SqliteQueue::open(&backend.db).unwrap();
    assert!(direct.ledger_rows(LedgerKind::Session).is_err());
    let (heads, anchors) = (
        direct.ledger_heads(LedgerKind::Session).unwrap(),
        direct.ledger_session_anchors().unwrap(),
    );
    assert_eq!(heads.len(), 4);
    assert_eq!(anchors.len(), 7);
    let service = Arc::new(InProcess {
        backend,
        token,
        ops: std::sync::Mutex::default(),
    });
    let queue = SupervisorQueues::new(
        service.clone(),
        crate::domain::ActorContext::instance(crate::domain::ActorRole::Supervisor, 7),
    )
    .queue();
    assert_eq!(queue.ledger_heads(LedgerKind::Session).unwrap(), heads);
    assert_eq!(queue.ledger_session_anchors().unwrap(), anchors);
    assert!(queue.ledger_heads(LedgerKind::Run).unwrap().is_empty());
    assert_eq!(
        *service.ops.lock().unwrap(),
        ["ledger_heads", "ledger_session_anchors", "ledger_heads"]
    );
}

/// The waits settled a task's fold reads besides the book made again cross
/// through the service by their own operation, as the queue's own adapter
/// reads them, with no record: a row whose records read whole no more
/// gives them through the service too. A worker's token is refused it,
/// the refusal recorded. The queue's and the service's times are fixed.
#[test]
fn the_settled_waits_cross_as_the_queues_own_without_the_rows_whole() {
    use crate::application::LedgerStore;
    use crate::application::supervisor_service::SupervisorQueues;
    use crate::domain::ledger::LedgerKind;
    use crate::domain::ledger::testkit::{built_and_settled, ms, waits_across};
    use crate::domain::pre_claim::rows::Since;
    let owner = LeaseToken::new("owner");
    let (_dir, backend) = claimed_with(&owner, fixed_time());
    let direct = SqliteQueue::open(&backend.db)
        .unwrap()
        .with_generators(fixed_time());
    let (log, position, at) = waits_across();
    for row in built_and_settled(&log.events[..position], ms(at)) {
        direct
            .write_ledger_row(&crate::domain::ledger::LedgerRow {
                is_final: true,
                ..row
            })
            .unwrap();
    }
    direct
        .conn
        .execute(
            "UPDATE ledger_rows SET records='[{\"id\":5}]' WHERE kind='queue' AND key='5'",
            [],
        )
        .unwrap();
    assert!(direct.ledger_rows(LedgerKind::Queue).is_err());
    let from = Since {
        ms: i64::MIN,
        id: crate::domain::EventId::new(0),
    };
    let settled = direct
        .ledger_settled_waits(Since::NONE, from, i64::MAX)
        .unwrap();
    assert_eq!(settled.len(), 2);
    let token = token_of(&backend, &Principal::supervisor(&owner, 7));
    let worker = Principal::worker(&RunId::new(RUN).unwrap(), crate::domain::TaskId::new(1));
    let worker_token = token_of(&backend, &worker);
    let args = json!([[i64::MAX, i64::MAX], [i64::MIN, 0], i64::MAX]);
    let denied = call(&backend, &worker_token, "ledger_settled_waits", args);
    assert_eq!(
        denied.error.as_ref().map(|error| error.code),
        Some(ServiceErrorCode::AuthorizationDenied),
        "{denied:?}"
    );
    assert_eq!(events(&backend, "authorization_denied").len(), 1);
    let service = Arc::new(InProcess {
        backend,
        token,
        ops: std::sync::Mutex::default(),
    });
    let queue = SupervisorQueues::new(
        service.clone(),
        crate::domain::ActorContext::instance(crate::domain::ActorRole::Supervisor, 7),
    )
    .queue();
    assert_eq!(
        queue
            .ledger_settled_waits(Since::NONE, from, i64::MAX)
            .unwrap(),
        settled
    );
    assert_eq!(*service.ops.lock().unwrap(), ["ledger_settled_waits"]);
}

/// The heads and the anchors are the supervisor principal's reads alone:
/// a worker's token is refused them as the policy refuses it, each refusal
/// recorded.
#[test]
fn only_the_supervisor_principal_reads_the_heads_and_anchors() {
    let owner = LeaseToken::new("owner");
    let (_dir, backend) = claimed(&owner);
    let worker = Principal::worker(&RunId::new(RUN).unwrap(), crate::domain::TaskId::new(1));
    let worker_token = token_of(&backend, &worker);
    for (op, args) in [
        ("ledger_heads", json!(["session", null, 10])),
        ("ledger_session_anchors", json!([null, 10])),
    ] {
        let denied = call(&backend, &worker_token, op, args.clone());
        assert_eq!(
            denied.error.as_ref().map(|error| error.code),
            Some(ServiceErrorCode::AuthorizationDenied),
            "{op}: {denied:?}"
        );
        let owner_token = token_of(&backend, &Principal::supervisor(&owner, 7));
        let answered = call(&backend, &owner_token, op, args);
        assert!(answered.ok, "{op}: {answered:?}");
    }
    assert_eq!(events(&backend, "authorization_denied").len(), 2);
}

/// The service's clock fixed at one time, and random ids.
fn fixed_time() -> crate::application::Generators {
    struct At;
    impl crate::application::Clock for At {
        fn system_time(&self) -> std::time::SystemTime {
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_900_000_000)
        }
        fn monotonic(&self) -> std::time::Instant {
            std::time::Instant::now()
        }
    }
    crate::application::Generators {
        clock: Arc::new(At),
        ..crate::infrastructure::clock::system()
    }
}

/// What builds the ledger reads the start of the tasks not settled, the
/// rows the waits and the lives are made again from and the stops through
/// the service by their own operations, which answer as the queue's own
/// adapter does, never by the rows whole: over the waits across a
/// position (open, ended by a supervisor's life, ended since a task not
/// settled, a stream not replaced, a stop of its own row besides the one
/// ending a hold), with a record that reads whole no more, the rows cross
/// without their records and the stops alone of the records, in pages.
#[test]
fn the_wait_rows_and_stops_cross_as_the_queues_own_without_the_rows_whole() {
    use crate::application::LedgerStore;
    use crate::application::supervisor_service::SupervisorQueues;
    use crate::domain::ledger::LedgerKind;
    use crate::domain::ledger::testkit::{built_and_settled, ms, waits_across};
    let owner = LeaseToken::new("owner");
    let (_dir, mut backend) = claimed(&owner);
    backend.generators = fixed_time();
    let direct = SqliteQueue::open(&backend.db).unwrap();
    let (log, position, at) = waits_across();
    let mut events = log.events[..position].to_vec();
    let mut stop = events
        .iter()
        .find(|event| event.kind == crate::domain::EventKind::SupervisorStopped)
        .unwrap()
        .clone();
    stop.id = crate::domain::EventId::new(100);
    stop.payload = json!({"supervisor": "f"});
    events.push(stop);
    for row in built_and_settled(&events, ms(at)) {
        direct.write_ledger_row(&row).unwrap();
    }
    direct
        .conn
        .execute(
            "UPDATE ledger_rows SET records=json_set(records, '$[0]', json('{\"id\":1}'))
              WHERE kind='queue' AND key='1'",
            [],
        )
        .unwrap();
    assert!(direct.ledger_rows(LedgerKind::Queue).is_err());
    let since = direct.ledger_tasks_open_since().unwrap();
    let (rows, stops) = (
        direct.ledger_wait_rows(since).unwrap(),
        direct.ledger_supervisor_stops().unwrap(),
    );
    assert!(rows.len() > 5);
    assert!(rows.iter().all(|row| row.records.is_empty()));
    let ids: Vec<i64> = stops.iter().map(|stop| stop.id.as_i64()).collect();
    assert_eq!(ids, [9, 100]);
    let token = token_of(&backend, &Principal::supervisor(&owner, 7));
    let service = Arc::new(InProcess {
        backend,
        token,
        ops: std::sync::Mutex::default(),
    });
    let queue = SupervisorQueues::new(
        service.clone(),
        crate::domain::ActorContext::instance(crate::domain::ActorRole::Supervisor, 7),
    )
    .queue();
    assert_eq!(queue.ledger_tasks_open_since().unwrap(), since);
    assert_eq!(queue.ledger_wait_rows(since).unwrap(), rows);
    assert_eq!(queue.ledger_supervisor_stops().unwrap(), stops);
    assert_eq!(
        *service.ops.lock().unwrap(),
        [
            "ledger_tasks_open_since",
            "ledger_wait_rows",
            "ledger_supervisor_stops"
        ]
    );
    // A page after a place gives what comes after it alone.
    let first = queue.ledger_supervisor_stops_after(None, 1).unwrap();
    assert_eq!(first.len(), 1);
    let (key, index) = first[0].0.clone();
    let rest = queue
        .ledger_supervisor_stops_after(Some((&key, index)), 10)
        .unwrap();
    let mut paged: Vec<_> = first
        .into_iter()
        .chain(rest)
        .map(|(_, stop)| stop)
        .collect();
    paged.sort_by_key(|stop| stop.id);
    assert_eq!(paged, stops);
    let first = queue.ledger_wait_rows_after(since, None, 2).unwrap();
    let rest = queue
        .ledger_wait_rows_after(since, Some(&first[1].key), 1_000)
        .unwrap();
    assert_eq!([first, rest].concat(), rows);

    // Two stops of one row, read a stop a page: each once, in their place.
    let mut later = stops[0].clone();
    later.id = crate::domain::EventId::new(200);
    let records = serde_json::to_string(&json!([{"id": 1}, stops[0], {"id": 2}, later])).unwrap();
    direct
        .conn
        .execute(
            "UPDATE ledger_rows SET records=?1 WHERE kind='queue' AND key='1'",
            [records],
        )
        .unwrap();
    let mut paged = Vec::new();
    let mut after: Option<(String, i64)> = None;
    loop {
        let page = direct
            .ledger_supervisor_stops_after(after.as_ref().map(|(k, i)| (k.as_str(), *i)), 1)
            .unwrap();
        let Some((place, stop)) = page.into_iter().next() else {
            break;
        };
        after = Some(place.clone());
        paged.push((place, stop.id.as_i64()));
    }
    assert!(paged.contains(&(("1".to_owned(), 1), 9)), "{paged:?}");
    assert!(paged.contains(&(("1".to_owned(), 3), 200)), "{paged:?}");
    let mut ids: Vec<i64> = paged.iter().map(|(_, id)| *id).collect();
    ids.sort_unstable();
    let whole: Vec<i64> = direct
        .ledger_supervisor_stops()
        .unwrap()
        .iter()
        .map(|stop| stop.id.as_i64())
        .collect();
    assert_eq!(ids, whole);
    assert_eq!(queue.ledger_supervisor_stops().unwrap().len(), whole.len());
}

/// The records of a workflow's freshness cross the service as the queue's
/// own: a stale record of the same workflow and latest success is written
/// once (a second, as from another supervisor or after a restart, is
/// not), a fresh record closes only the open one it names, and the reads
/// through the service are the queue's on its own connection.
#[test]
fn a_workflows_freshness_crosses_the_service_and_is_started_once_per_success() {
    use crate::application::QueueRecords;
    use crate::application::supervisor_service::SupervisorQueues;
    use crate::domain::ci_watch::freshness::FreshnessRecord;
    let owner = LeaseToken::new("owner");
    let (_dir, direct, backend) = claimed_in_memdb(&owner, "freshness-crosses");
    let token = token_of(&backend, &Principal::supervisor(&owner, 7));
    let service = Arc::new(InProcess {
        backend,
        token,
        ops: std::sync::Mutex::default(),
    });
    let queue = SupervisorQueues::new(
        service.clone(),
        crate::domain::ActorContext::instance(crate::domain::ActorRole::Supervisor, 7),
    )
    .queue();
    let stale = |previous: Option<i64>, run_id: Value| FreshnessRecord::Stale {
        previous,
        payload: json!({"workflow": "table.yml", "run_id": run_id, "age_secs": 151_200}),
    };
    let first = queue.record_ci_freshness(stale(None, json!(5))).unwrap();
    assert!(first.is_some());
    // The same success again, as decided before the first was written or
    // after it.
    assert_eq!(
        queue.record_ci_freshness(stale(None, json!(5))).unwrap(),
        None
    );
    assert_eq!(
        queue
            .record_ci_freshness(stale(first.map(EventId::as_i64), json!(5)))
            .unwrap(),
        None
    );
    // Another workflow's, and none of a workflow with no success twice.
    let other = FreshnessRecord::Stale {
        previous: None,
        payload: json!({"workflow": "other.yml", "run_id": 5}),
    };
    assert!(queue.record_ci_freshness(other).unwrap().is_some());
    let none = FreshnessRecord::Stale {
        previous: None,
        payload: json!({"workflow": "none.yml", "run_id": null}),
    };
    assert!(queue.record_ci_freshness(none.clone()).unwrap().is_some());
    assert_eq!(queue.record_ci_freshness(none).unwrap(), None);
    // A fresh record that names no open record is not written; one that
    // names it is, once.
    let fresh = |closes: i64| FreshnessRecord::Fresh {
        payload: json!({"workflow": "table.yml", "closes": closes, "run_id": 9}),
    };
    assert_eq!(queue.record_ci_freshness(fresh(999)).unwrap(), None);
    let open = first.unwrap().as_i64();
    assert!(queue.record_ci_freshness(fresh(open)).unwrap().is_some());
    assert_eq!(queue.record_ci_freshness(fresh(open)).unwrap(), None);
    // The success recorded stale before is not recorded again.
    let closed = queue
        .ci_freshness_events()
        .unwrap()
        .last()
        .map(|e| e.id.as_i64());
    assert_eq!(
        queue.record_ci_freshness(stale(closed, json!(5))).unwrap(),
        None
    );

    let read = queue.ci_freshness_events().unwrap();
    assert_eq!(read, direct.ci_freshness_events().unwrap());
    assert_eq!(
        read.iter()
            .map(|event| event.kind.to_string())
            .collect::<Vec<_>>(),
        [
            "ci_workflow_stale",
            "ci_workflow_stale",
            "ci_workflow_stale",
            "ci_workflow_fresh"
        ]
    );
    let ops = service.ops.lock().unwrap();
    assert_eq!(ops.last().map(String::as_str), Some("ci_freshness_events"));
    assert_eq!(
        ops.iter().filter(|op| *op == "record_ci_freshness").count(),
        10
    );
}

/// Two supervisors that read the same records and then two different
/// stale successes: the one that writes second, decided before the
/// first's record, is refused, so a late write of an older success never
/// opens it over the newer one's record; the newer one's attention stays
/// open, a second look at it holds, and only a success newer than it
/// closes it.
#[test]
fn a_late_stale_record_of_an_older_success_does_not_open_it_over_the_newer() {
    use crate::domain::ci_watch::CiRun;
    use crate::domain::ci_watch::freshness::{
        FreshnessDecision, FreshnessRecord, FreshnessState, FreshnessWatch, decide_freshness,
    };
    let owner = LeaseToken::new("owner");
    let (_dir, direct, backend) = claimed_in_memdb(&owner, "freshness-late");
    let token = token_of(&backend, &Principal::supervisor(&owner, 7));
    let watch = FreshnessWatch {
        workflow: "table.yml".into(),
        max_age_secs: 48 * 3600,
        lead_secs: 6 * 3600,
    };
    let success = |id: i64, created_at: &str| CiRun {
        run_id: id,
        run_number: id,
        sha: format!("sha{id}"),
        conclusion: "success".into(),
        url: format!("https://github.com/o/n/actions/runs/{id}"),
        created_at: created_at.into(),
        attempt: 1,
    };
    let (older, newer) = (
        success(1, "2026-10-27T00:00:00Z"),
        success(2, "2026-10-28T00:00:00Z"),
    );
    let now = crate::domain::stats::rfc3339_millis("2026-11-01T00:00:00Z").unwrap();
    let record = |run: &CiRun, state: &FreshnessState| {
        let decision = decide_freshness(&watch, "main", Some(run), now, state);
        (decision.clone(), FreshnessRecord::of(&decision, state))
    };
    let send = |record: FreshnessRecord| {
        let response = call(
            &backend,
            &token,
            "record_ci_freshness",
            json!([serde_json::to_value(record).unwrap()]),
        );
        assert!(response.ok, "{response:?}");
        serde_json::from_value::<Option<EventId>>(response.result.unwrap()).unwrap()
    };
    // Both read the empty records; the newer success is written first.
    let empty = FreshnessState::default();
    let (_, of_newer) = record(&newer, &empty);
    let (_, of_older) = record(&older, &empty);
    assert!(send(of_newer.unwrap()).is_some());
    assert_eq!(send(of_older.unwrap()), None);
    let state = FreshnessState::fold(&direct.ci_freshness_events().unwrap(), "table.yml");
    assert_eq!(state.open.as_ref().and_then(|open| open.run_id), Some(2));
    // The newer success, still old, holds: nothing closes or reopens it.
    assert_eq!(record(&newer, &state), (FreshnessDecision::Hold, None));
    // A decision made before a close is not written after it either.
    let (close, of_close) = record(&success(3, "2026-10-31T23:00:00Z"), &state);
    assert!(matches!(close, FreshnessDecision::Close(_)));
    assert!(send(of_close.unwrap()).is_some());
    let (_, late) = record(&older, &FreshnessState::default());
    assert_eq!(send(late.unwrap()), None);
    assert_eq!(
        direct
            .ci_freshness_events()
            .unwrap()
            .iter()
            .map(|event| event.kind.to_string())
            .collect::<Vec<_>>(),
        ["ci_workflow_stale", "ci_workflow_fresh"]
    );
}

/// The freshness records are the supervisor principal's alone: a worker's
/// and an observer's tokens are refused them as the policy refuses them,
/// each refusal recorded, and nothing is written.
#[test]
fn only_the_supervisor_principal_reads_and_writes_a_workflows_freshness() {
    let owner = LeaseToken::new("owner");
    let (_dir, _kept, backend) = claimed_in_memdb(&owner, "freshness-principals");
    let others = [
        Principal::worker(&RunId::new(RUN).unwrap(), crate::domain::TaskId::new(1)),
        Principal::of(&crate::domain::ActorContext::instance(
            crate::domain::ActorRole::Observer,
            "s1",
        )),
    ];
    let record = json!([{"record": "stale", "previous": null, "payload": {"workflow": "table.yml", "run_id": 5}}]);
    let mut denials = 0;
    for principal in others {
        let token = token_of(&backend, &principal);
        for (op, args) in [
            ("ci_freshness_events", json!([])),
            ("record_ci_freshness", record.clone()),
        ] {
            let denied = call(&backend, &token, op, args);
            assert_eq!(
                denied.error.as_ref().map(|error| error.code),
                Some(ServiceErrorCode::AuthorizationDenied),
                "{op}: {denied:?}"
            );
            denials += 1;
        }
    }
    assert_eq!(events(&backend, "authorization_denied").len(), denials);
    assert!(events(&backend, "ci_workflow_stale").is_empty());
    let owner_token = token_of(&backend, &Principal::supervisor(&owner, 7));
    assert!(call(&backend, &owner_token, "record_ci_freshness", record).ok);
    assert_eq!(events(&backend, "ci_workflow_stale").len(), 1);
}

/// What builds the ledger writes the session rows of a pass and the mark
/// through the service in one operation, each row over the one it read,
/// and reads the mark back by its own: whole in one request, or staged in
/// parts first when past one request whole, either way written in one
/// transaction; one row another writer wrote since its read writes none
/// and leaves the mark. A worker's token is refused both.
#[test]
fn the_session_rows_and_the_mark_cross_in_one_write_whole_or_staged() {
    use crate::application::LedgerStore;
    use crate::application::supervisor_service::SupervisorQueues;
    use crate::domain::ledger::session::written::SessionsWritten;
    use crate::domain::ledger::{LedgerKind, LedgerRow};
    let owner = LeaseToken::new("owner");
    let (_dir, backend) = claimed_with(&owner, fixed_time());
    let token = token_of(&backend, &Principal::supervisor(&owner, 7));
    let worker = Principal::worker(&RunId::new(RUN).unwrap(), crate::domain::TaskId::new(1));
    let worker_token = token_of(&backend, &worker);
    for (op, args) in [
        ("write_session_rows_over", json!([[], null])),
        ("write_staged_session_rows_over", json!(["d"])),
        ("ledger_sessions_written", json!([])),
    ] {
        let denied = call(&backend, &worker_token, op, args);
        assert_eq!(
            denied.error.as_ref().map(|error| error.code),
            Some(ServiceErrorCode::AuthorizationDenied),
            "{op}: {denied:?}"
        );
    }
    let db = backend.db.clone();
    let service = Arc::new(InProcess {
        backend,
        token,
        ops: std::sync::Mutex::default(),
    });
    let queue = SupervisorQueues::new(
        service.clone(),
        crate::domain::ActorContext::instance(crate::domain::ActorRole::Supervisor, 7),
    )
    .queue();
    let (log, _) = crate::domain::ledger::testkit::sessions_across();
    let rows =
        crate::domain::ledger::session::session_rows(&log.events, "2030-01-01T00:00:00.000Z");
    let batch: Vec<(LedgerRow, Option<LedgerRow>)> =
        rows.iter().map(|row| (row.clone(), None)).collect();
    let mark = SessionsWritten::at(crate::domain::EventId::new(7));
    assert_eq!(queue.sessions_written().unwrap(), None);
    assert!(queue.write_session_rows_over(&batch, Some(&mark)).unwrap());
    assert_eq!(queue.sessions_written().unwrap(), Some(mark));
    let direct = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        direct.ledger_rows(LedgerKind::Session).unwrap().len(),
        rows.len()
    );
    assert_eq!(
        *service.ops.lock().unwrap(),
        [
            "ledger_sessions_written",
            "write_session_rows_over",
            "ledger_sessions_written"
        ]
    );

    // Past one request whole: staged, then written as one.
    let mut large = rows[0].clone();
    large.detail = json!({"text": "a".repeat(crate::domain::queue_service::MAX_REQUEST_BYTES)});
    let over = [(large.clone(), Some(rows[0].clone()))];
    let later = SessionsWritten::at(crate::domain::EventId::new(9));
    service.ops.lock().unwrap().clear();
    assert!(queue.write_session_rows_over(&over, Some(&later)).unwrap());
    let ops = service.ops.lock().unwrap().clone();
    assert_eq!(
        ops.last().map(String::as_str),
        Some("write_staged_session_rows_over")
    );
    assert!(
        ops[..ops.len() - 1]
            .iter()
            .all(|op| op == "stage_ledger_row"),
        "{ops:?}"
    );
    assert_eq!(direct.sessions_written().unwrap(), Some(later));
    assert_eq!(
        direct.ledger_row(LedgerKind::Session, &large.key).unwrap(),
        Some(large.clone())
    );

    // The second over a stale read (the first's row before it was
    // written large): none written, the mark as it was.
    let mut moved = rows[1].clone();
    moved.built_at = "2031-01-01T00:00:00.000Z".into();
    let stale = [
        (moved.clone(), Some(rows[1].clone())),
        (rows[0].clone(), Some(rows[0].clone())),
    ];
    let last = SessionsWritten::at(crate::domain::EventId::new(11));
    assert!(!queue.write_session_rows_over(&stale, Some(&last)).unwrap());
    assert_eq!(direct.sessions_written().unwrap(), Some(later));
    assert_eq!(
        direct.ledger_row(LedgerKind::Session, &moved.key).unwrap(),
        Some(rows[1].clone())
    );
}

/// R crosses beside the session rows' mark in the same write, whole or
/// staged, and reads back with it; a reader of the session rows' mark
/// alone reads that from the same answer, and the marks of a store that
/// holds the session rows' mark alone read with no R.
#[test]
fn r_crosses_with_the_session_rows_mark_in_one_write_whole_or_staged() {
    use crate::application::LedgerStore;
    use crate::application::supervisor_service::SupervisorQueues;
    use crate::domain::ledger::session::written::{Cursors, Marks, RowsWritten, SessionsWritten};
    use crate::domain::ledger::{LedgerKind, LedgerRow};
    let owner = LeaseToken::new("owner");
    let (_dir, backend) = claimed_with(&owner, fixed_time());
    let token = token_of(&backend, &Principal::supervisor(&owner, 7));
    let db = backend.db.clone();
    let service = Arc::new(InProcess {
        backend,
        token,
        ops: std::sync::Mutex::default(),
    });
    let queue = SupervisorQueues::new(
        service.clone(),
        crate::domain::ActorContext::instance(crate::domain::ActorRole::Supervisor, 7),
    )
    .queue();
    let (log, _) = crate::domain::ledger::testkit::sessions_across();
    let rows =
        crate::domain::ledger::session::session_rows(&log.events, "2030-01-01T00:00:00.000Z");
    let batch: Vec<(LedgerRow, Option<LedgerRow>)> =
        rows.iter().map(|row| (row.clone(), None)).collect();
    let alone = SessionsWritten::at(crate::domain::EventId::new(7));
    assert!(queue.write_session_rows_over(&batch, Some(&alone)).unwrap());
    assert_eq!(
        queue.ledger_marks().unwrap(),
        Some(Marks {
            sessions: alone,
            rows: None
        })
    );
    let r = |events: i64| {
        RowsWritten::at(
            Cursors {
                events: crate::domain::EventId::new(events),
                samples: 2,
                steps: 3,
                resources: 4,
                inside_resources: 5,
            },
            1_900_000_000_000,
        )
    };
    let marks = Marks {
        sessions: SessionsWritten::at(crate::domain::EventId::new(8)),
        rows: Some(r(6)),
    };
    assert!(queue.write_session_rows_marked(&[], Some(&marks)).unwrap());
    assert_eq!(queue.ledger_marks().unwrap(), Some(marks));
    assert_eq!(queue.sessions_written().unwrap(), Some(marks.sessions));
    let direct = SqliteQueue::open(&db).unwrap();
    assert_eq!(direct.ledger_marks().unwrap(), Some(marks));

    // Past one request whole: staged, then written as one.
    let mut large = rows[0].clone();
    large.detail = json!({"text": "a".repeat(crate::domain::queue_service::MAX_REQUEST_BYTES)});
    let over = [(large.clone(), Some(rows[0].clone()))];
    let later = Marks {
        sessions: SessionsWritten::at(crate::domain::EventId::new(9)),
        rows: Some(r(9)),
    };
    service.ops.lock().unwrap().clear();
    assert!(
        queue
            .write_session_rows_marked(&over, Some(&later))
            .unwrap()
    );
    assert_eq!(
        service.ops.lock().unwrap().last().map(String::as_str),
        Some("write_staged_session_rows_over")
    );
    assert_eq!(direct.ledger_marks().unwrap(), Some(later));
    // The marks alone, R moved with W as it was.
    let moved = Marks {
        rows: Some(r(9).through).map(|through| {
            RowsWritten::at(
                Cursors {
                    samples: 6,
                    ..through
                },
                i64::MIN,
            )
        }),
        ..later
    };
    queue.write_ledger_marks(&moved).unwrap();
    assert_eq!(direct.ledger_marks().unwrap(), Some(moved));
    assert_eq!(
        direct.ledger_rows(LedgerKind::Session).unwrap().len(),
        rows.len()
    );
    // An R that holds no time crosses as none.
    let no_time = Marks {
        rows: moved.rows.map(|rows| RowsWritten {
            read_ms: None,
            ..rows
        }),
        ..moved
    };
    queue.write_ledger_marks(&no_time).unwrap();
    assert_eq!(queue.ledger_marks().unwrap(), Some(no_time));
    assert_eq!(direct.ledger_marks().unwrap(), Some(no_time));
}

/// What builds the ledger cuts its rows to a position by reads the start
/// of the task rows for that position and the last event naming a
/// supervisor up to it through the service by their own operations, which
/// answer as the queue's own adapter does: the one aggregate and the one
/// event (or none), never the rows whole, a record of theirs or the other
/// events, so a row whose records read whole no more leaves them as they
/// are.
#[test]
fn the_reads_of_a_cut_to_a_position_cross_as_the_queues_own() {
    use crate::application::LedgerStore;
    use crate::application::supervisor_service::SupervisorQueues;
    use crate::domain::ledger::LedgerKind;
    use crate::domain::ledger::testkit::{built_and_settled, ms, waits_across};
    let owner = LeaseToken::new("owner");
    let (_dir, backend) = claimed_with(&owner, fixed_time());
    let direct = SqliteQueue::open(&backend.db).unwrap();
    let (log, position, at) = waits_across();
    let events = &log.events[..position];
    for row in built_and_settled(events, ms(at)) {
        direct.write_ledger_row(&row).unwrap();
    }
    direct
        .conn
        .execute("UPDATE ledger_rows SET records='[{\"id\":1}]'", [])
        .unwrap();
    assert!(direct.ledger_rows(LedgerKind::Queue).is_err());
    let first: i64 = direct
        .conn
        .query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM run_events", [], |r| {
            r.get(0)
        })
        .unwrap();
    for (n, supervisor) in ["d", "e", "d", "f"].into_iter().enumerate() {
        direct
            .conn
            .execute(
                "INSERT INTO run_events(id, kind, payload, actor_role, actor_id)
                 VALUES (?1, 'slots_full_started', ?2, 'supervisor', 'supervisor')",
                rusqlite::params![
                    first + i64::try_from(n).unwrap(),
                    json!({"supervisor": supervisor, "class": "heavy"}).to_string()
                ],
            )
            .unwrap();
    }
    let p = events.last().unwrap().id;
    let ats: Vec<EventId> = (0..=p.as_i64()).map(EventId::new).collect();
    let ups: Vec<EventId> = (0..first + 5).map(EventId::new).collect();
    let names = ["d", "e", "f", "g"];
    let since: Vec<_> = ats
        .iter()
        .map(|at| direct.ledger_tasks_open_since_at(*at).unwrap())
        .collect();
    let lasts: Vec<_> = names
        .iter()
        .flat_map(|name| ups.iter().map(move |upto| (*name, *upto)))
        .map(|(name, upto)| direct.last_event_naming_supervisor(name, upto).unwrap())
        .collect();
    assert!(lasts.iter().any(Option::is_some));
    assert!(lasts.iter().any(Option::is_none));
    let token = token_of(&backend, &Principal::supervisor(&owner, 7));
    let service = Arc::new(InProcess {
        backend,
        token,
        ops: std::sync::Mutex::default(),
    });
    let queue = SupervisorQueues::new(
        service.clone(),
        crate::domain::ActorContext::instance(crate::domain::ActorRole::Supervisor, 7),
    )
    .queue();
    let through: Vec<_> = ats
        .iter()
        .map(|at| queue.ledger_tasks_open_since_at(*at).unwrap())
        .collect();
    assert_eq!(through, since);
    let through: Vec<_> = names
        .iter()
        .flat_map(|name| ups.iter().map(move |upto| (*name, *upto)))
        .map(|(name, upto)| queue.last_event_naming_supervisor(name, upto).unwrap())
        .collect();
    assert_eq!(through, lasts);
    let ops = std::mem::take(&mut *service.ops.lock().unwrap());
    assert_eq!(ops.len(), ats.len() + names.len() * ups.len());
    assert!(
        ops[..ats.len()]
            .iter()
            .all(|op| op == "ledger_tasks_open_since_at")
    );
    assert!(
        ops[ats.len()..]
            .iter()
            .all(|op| op == "last_event_naming_supervisor")
    );

    // The answers hold the aggregate alone and one event alone.
    let answer = call(
        &service.backend,
        &service.token,
        "ledger_tasks_open_since_at",
        json!([p]),
    );
    let since = answer.result.unwrap();
    let mut fields: Vec<&String> = since.as_object().unwrap().keys().collect();
    fields.sort();
    assert_eq!(fields, ["id", "ms"]);
    let answer = call(
        &service.backend,
        &service.token,
        "last_event_naming_supervisor",
        json!(["d", first + 10]),
    );
    let event: crate::domain::RunEvent = serde_json::from_value(answer.result.unwrap()).unwrap();
    assert_eq!(event.id, EventId::new(first + 2));
}

/// What makes the summaries of the pushes again reads the pushes of no run
/// through the service by their own operation, which answers as the
/// queue's own adapter does: those of the task rows and of the queue rows
/// of a push alone, cut, a page after a push at a time, and no other queue
/// row (one whose records no longer parse leaves the read as it is).
#[test]
fn the_pushes_of_no_run_cross_as_the_queues_own_and_no_other_queue_row() {
    use crate::application::LedgerStore;
    use crate::application::supervisor_service::SupervisorQueues;
    use crate::domain::ledger::{self, LedgerKind};
    let owner = LeaseToken::new("owner");
    let (_dir, backend) = claimed_with(&owner, fixed_time());
    let direct = SqliteQueue::open(&backend.db).unwrap();
    let (mut log, now) = ledger::testkit::pushes_across(1);
    let pushed = json!({"remote": "origin", "branch": "late", "commit": "c"});
    log.add(Some(2), None, "push_finished", pushed.clone(), now)
        .add(None, None, "push_finished", pushed, now + 1);
    for row in ledger::build(&log.events, ledger::testkit::ms(now + 2), "t") {
        direct.write_ledger_row(&row).unwrap();
    }
    let expected = direct.ledger_runless_pushes().unwrap();
    assert_eq!(expected.len(), 2);
    let others = direct
        .conn
        .execute(
            "UPDATE ledger_rows SET records='[{\"id\":1}]'
              WHERE kind='queue' AND (subject_kind IS NULL OR subject_kind<>'push_finished')",
            [],
        )
        .unwrap();
    assert!(others > 0);
    assert!(direct.ledger_rows(LedgerKind::Queue).is_err());
    let token = token_of(&backend, &Principal::supervisor(&owner, 7));
    let service = Arc::new(InProcess {
        backend,
        token,
        ops: std::sync::Mutex::default(),
    });
    let queue = SupervisorQueues::new(
        service.clone(),
        crate::domain::ActorContext::instance(crate::domain::ActorRole::Supervisor, 7),
    )
    .queue();
    assert_eq!(queue.ledger_runless_pushes().unwrap(), expected);
    let ops = std::mem::take(&mut *service.ops.lock().unwrap());
    assert_eq!(ops, ["ledger_runless_pushes"]);
    let answer = call(
        &service.backend,
        &service.token,
        "ledger_runless_pushes",
        json!([expected[0].id, 1]),
    );
    let page: Vec<crate::domain::RunEvent> =
        serde_json::from_value(answer.result.unwrap()).unwrap();
    assert_eq!(page, expected[1..]);
}

/// What makes the summaries of the pushes again reads the runs a legacy
/// task claimed with no row and the runs past their rows through the
/// service by their own operations: their ids alone cross, as the queue's
/// own adapter gives them.
#[test]
fn the_runs_the_summaries_of_the_pushes_read_whole_cross_as_the_queues_own() {
    use crate::application::LedgerStore;
    use crate::application::supervisor_service::SupervisorQueues;
    use crate::domain::ledger::{self, LedgerKind};
    let owner = LeaseToken::new("owner");
    let (_dir, backend) = claimed_with(&owner, fixed_time());
    let direct = SqliteQueue::open(&backend.db).unwrap();
    let (old, later) = (ledger::testkit::run_id(1), ledger::testkit::run_id(2));
    let mut log = ledger::testkit::Log::default();
    log.legacy(1, &old, 0);
    let rows = ledger::build(&log.events, ledger::testkit::ms(2000), "t");
    for row in rows
        .iter()
        .filter(|row| matches!(row.kind, LedgerKind::Task | LedgerKind::Run))
    {
        direct.write_ledger_row(row).unwrap();
    }
    log.add(Some(1), Some(&later), "run_claimed", json!({}), 2000);
    direct
        .conn
        .pragma_update(None, "foreign_keys", false)
        .unwrap();
    for event in &log.events {
        direct
            .conn
            .execute(
                "INSERT INTO run_events(id, task_id, run_id, kind, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    event.id.as_i64() + 1_000_000,
                    event.task_id.map(|task| task.as_i64()),
                    event.run_id.as_ref().map(|run| run.as_str()),
                    event.kind.as_str(),
                    event.payload.to_string()
                ],
            )
            .unwrap();
    }
    let expected = [crate::domain::RunId::new(&later).unwrap()];
    assert_eq!(direct.ledger_legacy_runs_without_row().unwrap(), expected);
    let task = crate::domain::TaskId::new(1);
    let claimed = direct.ledger_runs_claimed(task).unwrap();
    assert_eq!(claimed.len(), 2);
    let (through, past) = (
        EventId::new(log.last().as_i64() + 1_000_000),
        [crate::domain::RunId::new(&old).unwrap()],
    );
    assert_eq!(direct.ledger_runs_past_their_rows(through).unwrap(), past);
    let records = direct.ledger_push_records().unwrap();
    assert_eq!(records.len(), 1);
    let heads = direct.ledger_heads_open_at(through, 0).unwrap();
    assert_eq!(heads.len(), 2);
    assert!(direct.ledger_runs_without_row().unwrap().is_empty());
    let token = token_of(&backend, &Principal::supervisor(&owner, 7));
    let service = Arc::new(InProcess {
        backend,
        token,
        ops: std::sync::Mutex::default(),
    });
    let queue = SupervisorQueues::new(
        service.clone(),
        crate::domain::ActorContext::instance(crate::domain::ActorRole::Supervisor, 7),
    )
    .queue();
    assert_eq!(queue.ledger_legacy_runs_without_row().unwrap(), expected);
    assert_eq!(queue.ledger_runs_claimed(task).unwrap(), claimed);
    assert_eq!(queue.ledger_runs_past_their_rows(through).unwrap(), past);
    assert_eq!(queue.ledger_push_records().unwrap(), records);
    assert_eq!(queue.ledger_heads_open_at(through, 0).unwrap(), heads);
    assert!(queue.ledger_runs_without_row().unwrap().is_empty());
    let ops = std::mem::take(&mut *service.ops.lock().unwrap());
    assert_eq!(
        ops,
        [
            "ledger_legacy_runs_without_row",
            "ledger_runs_claimed",
            "ledger_runs_past_their_rows",
            "ledger_push_records",
            "ledger_heads_open_at",
            "ledger_heads_open_at",
            "ledger_runs_without_row",
        ]
    );
    let page = |args: Value| {
        let answer = call(
            &service.backend,
            &service.token,
            "ledger_heads_open_at",
            args,
        );
        let heads: Vec<ledger::LedgerHead> =
            serde_json::from_value(answer.result.unwrap()).unwrap();
        heads.into_iter().map(|head| head.key).collect::<Vec<_>>()
    };
    assert_eq!(page(json!(["task", through, 0, null, 1])), ["1"]);
    assert!(page(json!(["task", through, 0, "1", 1])).is_empty());
    assert_eq!(page(json!(["run", through, 0, null, 1])), [old.as_str()]);
}

/// The heads a new process takes at a position cross a page at a time as
/// the queue's own adapter gives them, and those left out are in no page.
#[test]
fn the_queue_heads_at_a_position_cross_in_pages_as_the_queues_own() {
    use crate::application::LedgerStore;
    use crate::application::supervisor_service::SupervisorQueues;
    use crate::domain::ledger;
    let owner = LeaseToken::new("owner");
    let (_dir, backend) = claimed_with(&owner, fixed_time());
    let direct = SqliteQueue::open(&backend.db).unwrap();
    let log = ledger::testkit::forgotten_log(2);
    let rows = ledger::testkit::built_and_settled(&log.events, ledger::testkit::ms(100_000));
    for row in &rows {
        direct.write_ledger_row(row).unwrap();
    }
    let at = log.last();
    let heads = direct.ledger_queue_heads_at(at).unwrap();
    assert!(heads.len() >= 2);
    assert!(heads.len() < direct.ledger_queue_heads().unwrap().len());
    let token = token_of(&backend, &Principal::supervisor(&owner, 7));
    let service = Arc::new(InProcess {
        backend,
        token,
        ops: std::sync::Mutex::default(),
    });
    let queue = SupervisorQueues::new(
        service.clone(),
        crate::domain::ActorContext::instance(crate::domain::ActorRole::Supervisor, 7),
    )
    .queue();
    assert_eq!(queue.ledger_queue_heads_at(at).unwrap(), heads);
    assert_eq!(
        std::mem::take(&mut *service.ops.lock().unwrap()),
        ["ledger_queue_heads_at"]
    );
    let page = |args: Value| {
        let answer = call(
            &service.backend,
            &service.token,
            "ledger_queue_heads_at",
            args,
        );
        let page: Vec<ledger::queue::QueueHead> =
            serde_json::from_value(answer.result.unwrap()).unwrap();
        page
    };
    assert_eq!(page(json!([at, null, 1])), heads[..1]);
    assert_eq!(page(json!([at, heads[0].head.key, 1])), heads[1..2]);
}
