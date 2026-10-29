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
