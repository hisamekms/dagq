//! The supervisor's use cases on the service's side (ADR-t3102-3
//! decisions 3 and 4): each is the queue's own operation in its one
//! transaction, under the lease the principal was issued for.

use super::*;
use crate::application::TaskStore;
use crate::domain::NewTask;

const RUN: &str = "11111111-1111-4111-8111-111111111111";

/// A queue with one task claimed as run [`RUN`], starting, under the lease `token`,
/// and the backend of its service.
fn claimed(token: &LeaseToken) -> (tempfile::TempDir, SqliteServiceBackend) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let mut queue = SqliteQueue::init(&db).unwrap();
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
        generators: crate::infrastructure::clock::system(),
        reads: Arc::new(|_, _, _| Ok(Value::Null)),
        writes: Arc::new(|_, _, _| Ok(Value::Null)),
        detail: |_, _| Value::Null,
        commands: Arc::new(|_, _, _| Ok(Value::Null)),
        at_stop: None,
        stands_for: None,
        doctor: None,
    };
    (dir, backend)
}

/// A token of `principal`'s for the queue of `backend`.
fn token_of(backend: &SqliteServiceBackend, principal: &Principal) -> String {
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
