//! The service's write line on a real SQLite queue: which thread and
//! connection a request runs on, and what a fenced write answers. What the
//! line itself decides is [`super::write_line`]'s tests.

use super::supervisor_tests::{RUN, claimed_with, token_of};
use super::*;
use crate::application::{Clock, WriteLeaderStore};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::SystemTime;

const BOUND: Duration = Duration::from_secs(30);

/// A clock that does not move: the tests read no real time.
struct Still;

impl Clock for Still {
    fn system_time(&self) -> SystemTime {
        std::time::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    fn monotonic(&self) -> Instant {
        static AT: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *AT.get_or_init(Instant::now)
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    lease: LeaseToken,
    backend: Arc<SqliteServiceBackend>,
}

impl Fixture {
    fn line(&self) -> &Arc<ServiceWriteLine> {
        self.backend.write_line.as_ref().unwrap()
    }

    /// How many events of `kind` the queue holds.
    fn count(&self, kind: EventKind) -> i64 {
        open(&self.backend.db)
            .conn
            .query_row(
                "SELECT count(*) FROM run_events WHERE kind = ?1",
                [kind.as_str()],
                |row| row.get(0),
            )
            .unwrap()
    }
}

fn open(db: &Path) -> SqliteQueue {
    SqliteQueue::open(db).unwrap()
}

/// A queue with one run claimed, and the backend of a service with a write
/// line whose [`ServiceCommands`] `commands` makes.
fn service(commands: impl FnOnce(Arc<ServiceWriteLine>) -> ServiceCommands) -> Fixture {
    let lease = LeaseToken::new("owner");
    let (dir, mut backend) = claimed_with(&lease, still());
    let line = Arc::new(ServiceWriteLine::start(&backend.db).unwrap());
    backend.commands = commands(line.clone());
    backend.write_line = Some(line);
    Fixture {
        _dir: dir,
        lease,
        backend: Arc::new(backend),
    }
}

/// The read before each command's write, as the application's three stores
/// authorize it: what the composition root runs them with.
fn read_steps() -> ServiceCommands {
    use crate::application::commands::{holds::Holds, planning::Planning, replans::Replans};
    use crate::application::queue_commands::QueueCommand;
    use crate::domain::task_hold::{HoldTarget, ReleaseChoice};
    use crate::domain::{ReplanId, StaticPolicy, TaskId};
    Arc::new(|queue, _, command| {
        let actor = queue.actor();
        match command {
            QueueCommand::ReleaseRead(_) => Holds::new(queue, &actor, &StaticPolicy)
                .release_read(
                    &HoldTarget::Task(TaskId::new(1)),
                    &ReleaseChoice {
                        provider: None,
                        model: None,
                    },
                )
                .map(drop),
            QueueCommand::ReplanResolveRead(_) => Replans::new(queue, &actor, &StaticPolicy)
                .to_resolve(ReplanId::new(1))
                .map(drop),
            QueueCommand::ReadyInheritRead(_) => Planning::new(queue, &actor, &StaticPolicy)
                .ready_inherit_read(TaskId::new(1), "r")
                .map(drop),
            _ => bail!("no such step of the test's"),
        }?;
        Ok(Value::Null)
    })
}

/// [`service`] with [`read_steps`], whose records off the line wait for it up
/// to `wait`.
fn service_recording_within(wait: Duration) -> Fixture {
    let lease = LeaseToken::new("owner");
    let (dir, mut backend) = claimed_with(&lease, still());
    let mut line = ServiceWriteLine::start(&backend.db).unwrap();
    line.record_wait = wait;
    backend.commands = read_steps();
    backend.write_line = Some(Arc::new(line));
    Fixture {
        _dir: dir,
        lease,
        backend: Arc::new(backend),
    }
}

fn still() -> Generators {
    Generators {
        clock: Arc::new(Still),
        ids: Arc::new(crate::infrastructure::clock::UuidGenerator),
    }
}

fn no_commands(_: Arc<ServiceWriteLine>) -> ServiceCommands {
    Arc::new(|_, _, _| Ok(Value::Null))
}

fn request(token: Option<String>, use_case: UseCase, params: Value) -> ServiceRequest {
    ServiceRequest {
        api_version: API_VERSION,
        token,
        env_role: None,
        queue: None,
        binding: None,
        use_case,
        params,
    }
}

/// A request of the user's (the service's own OS user), which reaches
/// [`ServiceCommands`] for a write.
fn write() -> ServiceRequest {
    request(None, UseCase::AskClose, json!({"id": 1}))
}

/// `request` answered from `peer` on a thread of its own, as a connection's
/// is: where its answer comes.
fn sent_from(
    backend: &Arc<SqliteServiceBackend>,
    request: ServiceRequest,
    peer: Peer,
) -> Receiver<Result<LineAnswer>> {
    let (answer, answered) = channel();
    let backend = backend.clone();
    thread::spawn(move || {
        let _ = answer.send(handled(&backend, 7, request, peer, None));
    });
    answered
}

/// `request` from the service's own OS user, answered within [`BOUND`].
fn answered(backend: &Arc<SqliteServiceBackend>, request: ServiceRequest) -> ServiceResponse {
    answered_from(backend, request, Peer::Owner)
}

fn answered_from(
    backend: &Arc<SqliteServiceBackend>,
    request: ServiceRequest,
    peer: Peer,
) -> ServiceResponse {
    sent_from(backend, request, peer)
        .recv_timeout(BOUND)
        .unwrap()
        .unwrap()
        .response
}

fn record_event(queue: &SqliteQueue) -> Result<()> {
    EventStore::record_queue_event(queue, EventKind::QueueServiceUnauthenticated, json!({}))
        .map(drop)
}

/// Two writes sent at once run on the line's one thread, each to its commit
/// while the other waits, and a read is answered while a write holds the line
/// and its transaction.
#[test]
fn writes_run_one_at_a_time_on_the_line_and_a_read_does_not_wait() {
    let (began, begun) = channel::<bool>();
    let (go, gone): (Sender<()>, Receiver<()>) = channel();
    let gone = Mutex::new(gone);
    let fixture = service(|line| {
        let began = Mutex::new(began);
        Arc::new(move |queue, _, _| {
            record_event(queue)?;
            let tx = queue.write_tx("held_by_the_test")?;
            began.lock().unwrap().send(line.line.here()).unwrap();
            gone.lock().unwrap().recv_timeout(BOUND)?;
            tx.commit()?;
            Ok(Value::Null)
        })
    });
    let before = fixture.count(EventKind::QueueServiceUnauthenticated);
    let first = sent_from(&fixture.backend, write(), Peer::Owner);
    assert!(begun.recv_timeout(BOUND).unwrap(), "off the line's thread");
    let second = sent_from(&fixture.backend, write(), Peer::Owner);
    // The first holds the line and the write lock; the read is answered.
    let read = answered(
        &fixture.backend,
        request(None, UseCase::ProposalList, json!({})),
    );
    assert!(read.ok, "{read:?}");
    // The second has not begun, whenever its thread got to the line.
    assert!(begun.try_recv().is_err());
    go.send(()).unwrap();
    assert!(first.recv_timeout(BOUND).unwrap().unwrap().response.ok);
    assert!(begun.recv_timeout(BOUND).unwrap(), "off the line's thread");
    go.send(()).unwrap();
    assert!(second.recv_timeout(BOUND).unwrap().unwrap().response.ok);
    assert_eq!(
        fixture.count(EventKind::QueueServiceUnauthenticated),
        before + 2
    );
}

/// Holds the line until it is let go or dropped: what waits for the line
/// waits for this.
struct HeldLine {
    go: Sender<()>,
    held: Receiver<LineAnswer>,
}

impl HeldLine {
    fn of(fixture: &Fixture) -> Self {
        let (go, gone) = channel::<()>();
        let held = fixture
            .line()
            .line
            .enqueue(move || {
                let _ = gone.recv_timeout(BOUND);
                LineAnswer {
                    response: ServiceResponse::success(Value::Null),
                    role: None,
                }
            })
            .unwrap();
        Self { go, held }
    }

    fn let_go(self) {
        self.go.send(()).unwrap();
        assert!(self.held.recv_timeout(BOUND).unwrap().response.ok);
    }
}

/// The reads of the supervisor, of a control step and of a session wrapper
/// are answered while the line is held, as a queue read is, and a write of the
/// supervisor's waits for it.
#[test]
fn every_kind_of_read_is_answered_while_the_line_is_held() {
    let fixture = service(no_commands);
    let held = HeldLine::of(&fixture);
    let run = RunId::new(RUN).unwrap();
    let supervisor = token_of(&fixture.backend, &Principal::supervisor(&fixture.lease, 7));
    let wrapper = token_of(
        &fixture.backend,
        &Principal::wrapper(&StepTarget::run(&run), Some(crate::domain::TaskId::new(1))),
    );
    let job_read =
        serde_json::to_value(crate::application::queue_reads::JobRead::StatsInputs).unwrap();
    let as_supervisor = |op: &str, args: Value| {
        request(
            Some(supervisor.clone()),
            UseCase::Supervisor,
            json!({"op": op, "args": args}),
        )
    };
    let as_wrapper = |op: &crate::application::session::SessionOp| {
        request(
            Some(wrapper.clone()),
            UseCase::SessionRun,
            serde_json::to_value(op).unwrap(),
        )
    };
    let as_control = |command: &str, op: &str, args: Value| {
        request(
            None,
            UseCase::Control,
            json!({
                "control": {"command": command, "task": 1, "pid": std::process::id()},
                "call": {"op": op, "args": args},
            }),
        )
    };
    let reads = [
        as_supervisor("active_runs", json!([])),
        as_supervisor("job_read", json!([job_read])),
        as_supervisor("ci_watch_events", json!([])),
        as_supervisor("assert_repository", json!(["a repository"])),
        as_supervisor("planner_handover", json!([1])),
        as_control("integrate", "run", json!([RUN])),
        as_control("observe", "job_read", json!([job_read])),
        as_wrapper(&crate::application::session::SessionOp::RunEvents { run: run.clone() }),
        as_wrapper(&crate::application::session::SessionOp::MachineUserRun {
            owner: StepTarget::run(&run),
            run: RUN.to_owned(),
        }),
    ];
    for read in reads {
        let said = read.params.clone();
        let peer = if read.token.is_some() {
            Peer::Other
        } else {
            Peer::Owner
        };
        let response = answered_from(&fixture.backend, read, peer);
        assert!(response.ok, "{said}: {response:?}");
    }
    let waiting = sent_from(
        &fixture.backend,
        as_supervisor("workspace_created", json!([RUN, "owner", "ws-1"])),
        Peer::Other,
    );
    held.let_go();
    let wrote = waiting.recv_timeout(BOUND).unwrap().unwrap().response;
    assert!(wrote.ok, "{wrote:?}");
}

/// Every operation sent to a read-only connection is answered on one with the
/// line held: none of them writes or waits for the line, in so far as the
/// arguments below read as its own. A write there fails as the first case
/// shows.
#[test]
fn an_op_that_only_reads_writes_nothing_on_its_read_only_connection() {
    let fixture = service(no_commands);
    let refused = crate::application::TaskStore::investigations(
        &mut SqliteQueue::open_read_only(&fixture.backend.db).unwrap(),
    )
    .expect_err("a write on a read-only connection");
    assert!(format!("{refused:#}").contains("readonly"), "{refused:#}");
    let held = HeldLine::of(&fixture);
    let supervisor = token_of(&fixture.backend, &Principal::supervisor(&fixture.lease, 7));
    let args = [
        json!([]),
        json!([RUN]),
        json!([1]),
        json!(["owner"]),
        json!([RUN, "owner"]),
        json!([1, 1]),
        json!([RUN, 1]),
        json!([1, "owner"]),
        json!(["owner", 1]),
        json!(["owner", "owner"]),
        json!([[1]]),
        json!([["owner"]]),
        json!([1, 1, 1]),
        json!(["owner", 1, 1]),
        json!([RUN, 1, 1]),
    ];
    for op in crate::application::supervisor_service::ops_that_only_read() {
        for args in &args {
            let response = answered_from(
                &fixture.backend,
                request(
                    Some(supervisor.clone()),
                    UseCase::Supervisor,
                    json!({"op": op, "args": args}),
                ),
                Peer::Other,
            );
            let said = format!("{:?}", response.error);
            assert!(!said.contains("readonly"), "{op} {args}: {said}");
            if response.ok {
                break;
            }
        }
    }
    held.let_go();
}

/// What the commands that read before they write refuse is recorded as their
/// caller, on the line, from the read-only connection they run on: each of the
/// application's stores gives it the same way.
#[test]
fn a_read_step_s_refusal_is_recorded_as_its_caller() {
    let fixture = service_recording_within(BOUND);
    let run = RunId::new(RUN).unwrap();
    let principal = Principal::worker(&run, crate::domain::TaskId::new(1));
    let worker = token_of(&fixture.backend, &principal);
    for (use_case, params) in [
        (UseCase::ReleaseRead, json!({"target": "1"})),
        (
            UseCase::ReplanResolveRead,
            json!({
                "id": 1,
                "generation": 1,
                "resolution": crate::domain::task_replan::ReplanResolution::Withdraw,
                "reason": "r",
            }),
        ),
        (UseCase::ReadyInheritRead, json!({"id": 1, "reason": "r"})),
    ] {
        assert!(!on_write_line(use_case, &params), "{use_case:?}");
        let before = fixture.count(EventKind::AuthorizationDenied);
        let refused = answered_from(
            &fixture.backend,
            request(Some(worker.clone()), use_case, params),
            Peer::Other,
        );
        assert_eq!(
            refused.error.as_ref().map(|error| error.code),
            Some(ServiceErrorCode::AuthorizationDenied),
            "{use_case:?}: {refused:?}"
        );
        assert_eq!(
            fixture.count(EventKind::AuthorizationDenied),
            before + 1,
            "{use_case:?}"
        );
        let recorded: String = open(&fixture.backend.db)
            .conn
            .query_row(
                "SELECT actor_id FROM run_events WHERE kind = ?1 ORDER BY id DESC LIMIT 1",
                [EventKind::AuthorizationDenied.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recorded, principal.actor().actor_id(), "{use_case:?}");
    }
}

/// A read refused while the line is held answers all the same, and its record
/// is written in its turn: an unauthenticated request's and the policy's.
#[test]
fn a_refused_read_does_not_wait_for_a_held_line_and_its_record_follows() {
    let fixture = service_recording_within(Duration::from_millis(1));
    let worker = token_of(
        &fixture.backend,
        &Principal::worker(&RunId::new(RUN).unwrap(), crate::domain::TaskId::new(1)),
    );
    let unauthenticated = fixture.count(EventKind::QueueServiceUnauthenticated);
    let denied = fixture.count(EventKind::AuthorizationDenied);
    let held = HeldLine::of(&fixture);
    let unknown = answered_from(
        &fixture.backend,
        request(
            Some("a token of no one's".to_owned()),
            UseCase::ProposalList,
            json!({}),
        ),
        Peer::Other,
    );
    assert_eq!(
        unknown.error.map(|error| error.code),
        Some(ServiceErrorCode::Unauthenticated)
    );
    let refused = answered_from(
        &fixture.backend,
        request(
            Some(worker),
            UseCase::ReadyInheritRead,
            json!({"id": 1, "reason": "r"}),
        ),
        Peer::Other,
    );
    assert_eq!(
        refused.error.map(|error| error.code),
        Some(ServiceErrorCode::AuthorizationDenied)
    );
    assert_eq!(fixture.count(EventKind::AuthorizationDenied), denied);
    held.let_go();
    // The line goes in order: what comes after the records finds them done.
    fixture
        .line()
        .line
        .enqueue(|| LineAnswer {
            response: ServiceResponse::success(Value::Null),
            role: None,
        })
        .unwrap()
        .recv_timeout(BOUND)
        .unwrap();
    assert_eq!(
        fixture.count(EventKind::QueueServiceUnauthenticated),
        unauthenticated + 1
    );
    assert_eq!(fixture.count(EventKind::AuthorizationDenied), denied + 1);
}

/// A takeover after the service kept its identity fences its next write: the
/// answer is `not_leader`, and nothing of it is in the queue.
#[test]
fn a_fenced_write_answers_not_leader_and_writes_nothing() {
    let fixture = service(|_| {
        Arc::new(move |queue, _, _| {
            record_event(queue)?;
            Ok(Value::Null)
        })
    });
    let ran = answered(&fixture.backend, write());
    assert!(ran.ok, "{ran:?}");
    open(&fixture.backend.db)
        .take_write_leader("another service", Duration::from_secs(60), true)
        .unwrap();
    let before = fixture.count(EventKind::QueueServiceUnauthenticated);
    let refused = answered(&fixture.backend, write());
    assert_eq!(
        refused.error.map(|error| error.code),
        Some(ServiceErrorCode::NotLeader)
    );
    assert_eq!(
        fixture.count(EventKind::QueueServiceUnauthenticated),
        before
    );
}

/// `mark` the line's connection (a temporary table is only its own), and
/// whether it is marked, each in the line's turn.
fn marked_on_the_line(line: &Arc<ServiceWriteLine>, mark: bool) -> bool {
    let on = line.clone();
    let (answer, answered) = channel();
    line.line
        .enqueue(move || {
            let queue = on.queue().unwrap();
            if mark {
                queue
                    .conn
                    .execute_batch("CREATE TEMP TABLE marked (x)")
                    .unwrap();
            }
            let _ = answer.send(
                queue
                    .conn
                    .query_row("SELECT count(*) FROM marked", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .is_ok(),
            );
            LineAnswer {
                response: ServiceResponse::success(Value::Null),
                role: None,
            }
        })
        .unwrap();
    answered.recv_timeout(BOUND).unwrap()
}

/// A token with a run is read on the connection its request writes on: an
/// integrator's and a worker's requests stay on the one connection, the
/// integrator's opening the queue again after the token's read.
#[test]
fn a_token_with_a_run_keeps_the_request_on_the_one_connection() {
    let fixture = service(no_commands);
    assert!(marked_on_the_line(fixture.line(), true));
    let run = RunId::new(RUN).unwrap();
    let integrator = token_of(
        &fixture.backend,
        &Principal::integrator(&run, &fixture.lease, 7),
    );
    let landing = answered_from(
        &fixture.backend,
        request(
            Some(integrator),
            UseCase::Supervisor,
            json!({"op": "workspace_created", "args": [RUN, "owner", "ws-1"]}),
        ),
        Peer::Other,
    );
    let refusal = format!("{:?}", landing.error);
    assert!(!refusal.contains("another loan"), "{refusal}");
    assert!(marked_on_the_line(fixture.line(), false));
    let worker = token_of(
        &fixture.backend,
        &Principal::worker(&run, crate::domain::TaskId::new(1)),
    );
    let note = answered_from(
        &fixture.backend,
        request(
            Some(worker),
            UseCase::Note,
            json!({"task": 1, "text": "on the one connection"}),
        ),
        Peer::Other,
    );
    assert!(note.ok, "{note:?}");
    assert!(marked_on_the_line(fixture.line(), false));
}

/// A token's run that ended is refused on the line, and the refusal's record
/// gets the connection the token's read gave back.
#[test]
fn a_token_refused_on_the_line_is_recorded() {
    let fixture = service(no_commands);
    let run = RunId::new(RUN).unwrap();
    let worker = token_of(
        &fixture.backend,
        &Principal::worker(&run, crate::domain::TaskId::new(1)),
    );
    open(&fixture.backend.db)
        .conn
        .execute_batch("UPDATE task_runs SET status = 'failed'")
        .unwrap();
    let before = fixture.count(EventKind::QueueServiceUnauthenticated);
    let refused = answered_from(
        &fixture.backend,
        request(Some(worker), UseCase::Note, json!({"task": 1, "text": "x"})),
        Peer::Other,
    );
    assert_eq!(
        refused.error.map(|error| error.code),
        Some(ServiceErrorCode::Unauthenticated)
    );
    assert_eq!(
        fixture.count(EventKind::QueueServiceUnauthenticated),
        before + 1
    );
}

/// A loan is on the same connection as the last until a loan left in a
/// transaction or a DB file replaced ends it, and there is no second loan
/// while one is out; a loan that is refused leaves the connection there.
#[test]
fn the_line_s_loans_share_one_connection_while_it_is_sound() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    SqliteQueue::init(&db).unwrap();
    let connection = SqliteQueue::write_connection(&db);
    let on_it = |queue: &SqliteQueue| {
        queue
            .conn
            .query_row("SELECT count(*) FROM marked", [], |row| {
                row.get::<_, i64>(0)
            })
            .is_ok()
    };
    let mark = |queue: &SqliteQueue| {
        queue
            .conn
            .execute_batch("CREATE TEMP TABLE marked (x)")
            .unwrap();
    };
    let queue = connection.lend().unwrap();
    mark(&queue);
    let wal_limit: i64 = queue
        .conn
        .query_row("PRAGMA journal_size_limit", [], |row| row.get(0))
        .unwrap();
    assert!(wal_limit > 0);
    let second = connection.lend().err().expect("a second loan");
    assert!(format!("{second:#}").contains("another loan"), "{second:#}");
    drop(queue);
    let queue = connection.lend().unwrap();
    assert!(on_it(&queue));
    // What a request left open is not the next one's.
    queue.conn.execute_batch("BEGIN").unwrap();
    drop(queue);
    let queue = connection.lend().unwrap();
    assert!(!on_it(&queue));
    mark(&queue);
    drop(queue);
    // Another queue at the path is not written through the old file.
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", db.display()));
    }
    SqliteQueue::init(&db).unwrap();
    let queue = connection.lend().unwrap();
    assert!(!on_it(&queue));
    record_event(&queue).unwrap();
}

/// A write the line stopped before leaves no answer: the caller's connection
/// closes, which it reads as maybe done and sends again.
#[test]
fn a_write_after_the_line_stopped_is_not_answered() {
    let fixture = service(no_commands);
    fixture.line().line.finish(BOUND);
    let answer = sent_from(&fixture.backend, write(), Peer::Owner)
        .recv_timeout(BOUND)
        .unwrap();
    assert!(answer.is_err());
}

/// A planner's token names a target, read on the connection the use case runs
/// on: `migrate --check` is off the line, so that is no write connection.
#[test]
fn an_administration_that_reads_authenticates_off_the_line() {
    let fixture = service(no_commands);
    let planner = token_of(&fixture.backend, &Principal::planner(1));
    let answered = answered_from(
        &fixture.backend,
        request(Some(planner), UseCase::Migrate, json!({"check": true})),
        Peer::Other,
    );
    let message = format!("{:?}", answered.error);
    assert!(!message.contains("off its write line"), "{message}");
}

/// Nothing writes beside the line: its connection is lent on its thread alone.
#[test]
fn the_line_s_queue_is_refused_off_its_thread() {
    let fixture = service(no_commands);
    let line = fixture.line().clone();
    assert!(line.queue().is_err());
    let (answer, recorded) = channel();
    thread::spawn(move || {
        let on = line.clone();
        let _ = answer.send(on_line(Some(&line), move || on.queue().map(drop)));
    });
    recorded.recv_timeout(BOUND).unwrap().unwrap();
}
