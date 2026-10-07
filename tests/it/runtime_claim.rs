//! Runtime tests: Claim and provisioning, leases, supervisor registrations, backend
//! failures, stats and `recover`, the injected clock and IDs, migrations and
//! the adapters.
use crate::runtime_support;
use dagq::domain::LeaseToken;
use dagq::infrastructure::git_binary::git_executable;

use runtime_support::*;

/// A failed backend call carries the call, the error and the load it
/// failed under: the load average (or null), the supervisor's slots held
/// and its `--parallel` (task 109).
fn assert_backend_failure(
    event: &dagq::domain::RunEvent,
    op: &str,
    workspace: Option<&str>,
    error: &str,
    run_id: &RunId,
) {
    assert_eq!(event.run_id.as_ref(), Some(run_id));
    assert_eq!(event.payload["op"], op, "{:?}", event.payload);
    assert_eq!(event.payload["workspace_id"], json!(workspace));
    assert_eq!(event.payload["timeout_secs"], 30);
    assert!(
        event.payload["error"].as_str().unwrap().contains(error),
        "{:?}",
        event.payload
    );
    assert!(event.payload["load_avg"].is_f64() || event.payload["load_avg"].is_null());
    assert_eq!(event.payload["slots"], 1);
    assert_eq!(event.payload["parallel"], 4);
}

/// A session wrapper that cannot be started in the background leaves the
/// run `starting` and stops claiming. The failure is recorded as
/// `backend_call_failed` on the run before the supervisor's own
/// `runtime_error`, which carries the call's code and op (ADR-0034) —
/// moved from the interactive
/// `failed_backend_calls_are_recorded_with_the_load_and_counted_by_stats`
/// that task 1437 deleted, and from a workspace's `create` to the
/// background launch (ADR-t1433-3).
#[test]
fn provisioning_failure_retains_the_run_and_stops_claiming_other_tasks() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "untouched", &[]);
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    let options = supervise_options(4, true);
    let error = format!(
        "{:#}",
        supervise_with(&db, &repo, &backend, &options).unwrap_err()
    );
    assert!(error.contains("injected background launch"), "{error}");
    assert!(error.contains("claiming stopped"), "{error}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Starting);
    assert!(
        run.last_error()
            .unwrap()
            .contains("injected background launch")
    );
    assert!(Path::new(run.worktree_path().unwrap()).exists());
    let failures = backend_failures(&detail);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_backend_failure(
        failures[0],
        "launch_background",
        None,
        "injected background launch failure",
        run.id(),
    );
    assert_eq!(failures[0].payload["code"], "backend_failed");
    let abandoned = detail
        .events
        .iter()
        .find(|e| e.kind == "runtime_error")
        .unwrap();
    assert!(failures[0].id < abandoned.id);
    assert_eq!(abandoned.payload["code"], "backend_failed");
    assert_eq!(abandoned.payload["op"], "launch_background");
    // The environment is suspect: the second candidate was left alone.
    assert!(queue.show(TaskId::new(2)).unwrap().runs.is_empty());
    assert_eq!(queue.candidates().unwrap()[0].id(), TaskId::new(2));
    // The run is disowned, so nothing has to be stopped before recovering it;
    // the drained loop took its registration with it.
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(queue.supervisors().unwrap().is_empty());
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["supervisors"], json!([]));
    assert_eq!(report["runs"][0]["recoverable"], true);
    assert_eq!(
        runtime::recover(&db, run.id()).unwrap()["run"]["status"],
        "interrupted"
    );
    assert_eq!(queue.show(TaskId::new(1)).unwrap().runs.len(), 1);
}

/// A failed stop of an accepted run's background wrapper (`close` on its
/// handle) is recorded as `backend_call_failed` on the run before the
/// supervisor's `cleanup_failed`, which carries the call's code and op
/// beside its message and the handle — moved from the interactive
/// `failed_backend_calls_are_recorded_with_the_load_and_counted_by_stats`
/// that task 1437 deleted.
#[test]
fn a_failed_workspace_close_is_recorded_before_the_cleanup_failure_with_its_code_and_op() {
    let (_dir, _db, detail) = run_agent_with(VALID_AGENT, true);
    let run = &detail.runs[0];
    let failures = backend_failures(&detail);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_backend_failure(
        failures[0],
        "close",
        Some(background_session(run).as_str()),
        "injected session stop failure",
        run.id(),
    );
    assert_eq!(failures[0].payload["code"], "backend_failed");
    let cleanup = detail
        .events
        .iter()
        .find(|e| e.kind == "cleanup_failed")
        .unwrap();
    assert_eq!(
        cleanup
            .payload
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["code", "message", "op", "workspace_id"]
    );
    assert_eq!(cleanup.payload["code"], "backend_failed");
    assert_eq!(cleanup.payload["op"], "close");
    assert!(failures[0].id < cleanup.id);
}

/// A cmux whose first `capture_timeouts` screen reads and every `exists`
/// time out, as cmux does under load; nothing else is called.
struct TimingOutCmux {
    capture_timeouts: AtomicUsize,
}

impl WorkspaceBackend for TimingOutCmux {
    fn preflight(&self) -> Result<()> {
        unimplemented!()
    }
    fn preflight_detached(&self, _: &SupervisorEnvironment) -> Result<()> {
        unimplemented!()
    }
    fn send_text(&self, _: &str, _: &str) -> Result<()> {
        unimplemented!()
    }
    fn send_enter(&self, _: &str) -> Result<()> {
        unimplemented!()
    }
    fn capture(&self, _: &str) -> Result<String> {
        let left = &self.capture_timeouts;
        if left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            bail!("cmux read-screen failed: Command timed out");
        }
        Ok("ready".into())
    }
    fn close(&self, _: &str) -> Result<()> {
        unimplemented!()
    }
    fn set_color(&self, _: &str, _: &str) -> Result<()> {
        unimplemented!()
    }
    fn set_status(&self, _: &str, _: &str, _: &str, _: &str) -> Result<()> {
        unimplemented!()
    }
    fn pin(&self, _: &str) -> Result<()> {
        unimplemented!()
    }
    fn send_exit(&self, _: &str) -> Result<()> {
        unimplemented!()
    }
    fn exists(&self, _: &str) -> Result<bool> {
        bail!("cmux list-workspaces failed: Command timed out")
    }
    fn listed_workspace_ids(&self) -> Result<Vec<String>> {
        unimplemented!()
    }
    fn create_named(&self, _: &str, _: &Path, _: &str, _: &WorkspaceTags) -> Result<String> {
        unimplemented!()
    }
    fn ensure_group(&self, _: &str, _: &str) -> Result<String> {
        unimplemented!()
    }
    fn notify(&self, _: &str, _: &str, _: Option<&str>) -> Result<()> {
        unimplemented!()
    }
    fn retry_backoff(&self) -> Duration {
        Duration::from_millis(10)
    }
}

/// A timed-out effect-free call (`capture`, `exists`) is made again after
/// a backoff doubled each time, and every failed attempt is recorded as
/// `backend_call_failed` on the run whose workspace it was for, with its
/// number of the backend's attempts and the backoff that followed it (none
/// after the last) — moved from the interactive
/// `failed_backend_calls_are_recorded_with_the_load_and_counted_by_stats`
/// that task 1437 deleted.
#[test]
fn timed_out_effect_free_calls_are_retried_with_a_doubling_backoff_each_attempt_recorded() {
    let (_dir, repo, db) = fixture();
    let run = provision_under(&repo, &db, "owner");
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .workspace_created(run.id(), &LeaseToken::new("owner"), "timing-out-ws")
        .unwrap();
    let cmux = TimingOutCmux {
        capture_timeouts: AtomicUsize::new(2),
    };
    let recording = runtime::RecordingBackend::new(&cmux, db.clone(), None);
    assert_eq!(recording.capture("timing-out-ws").unwrap(), "ready");
    assert!(recording.exists("timing-out-ws").is_err());
    let detail = queue.show(TaskId::new(1)).unwrap();
    let attempts: Vec<_> = backend_failures(&detail)
        .into_iter()
        .map(|e| {
            assert_eq!(e.run_id.as_ref(), Some(run.id()));
            assert_eq!(e.payload["workspace_id"], "timing-out-ws");
            assert_eq!(e.payload["code"], "backend_timeout");
            (
                e.payload["op"].clone(),
                e.payload["attempt"].clone(),
                e.payload["max_attempts"].clone(),
                e.payload["retry_after_ms"].clone(),
            )
        })
        .collect();
    assert_eq!(
        attempts,
        [
            (json!("capture"), json!(1), json!(3), json!(10)),
            (json!("capture"), json!(2), json!(3), json!(20)),
            (json!("exists"), json!(1), json!(3), json!(10)),
            (json!("exists"), json!(2), json!(3), json!(20)),
            (json!("exists"), json!(3), json!(3), Value::Null),
        ]
    );
}

/// With one slot the supervisor claims the candidate whose completion
/// releases the most unfinished tasks before the older task 1 (ADR-0023),
/// and records no event for the reordering.
#[test]
fn supervisor_claims_the_candidate_that_releases_the_most_tasks_first() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let root = add_ready_task(&mut queue, "root", &[]);
    let middle = add_ready_task(&mut queue, "middle", &[root]);
    add_ready_task(&mut queue, "leaf", &[middle]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(1, true)).unwrap();
    let claimed: Vec<i64> = outcome["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["task_id"].as_i64().unwrap())
        .collect();
    assert_eq!(claimed, [root.as_i64(), 1]);
    let mut claim_event = |task: TaskId| {
        let detail = queue.show(task).unwrap();
        assert!(!event_kinds(&detail).contains(&"claim_reordered"));
        detail
            .events
            .iter()
            .find(|event| event.kind == "run_claimed")
            .unwrap()
            .id
    };
    assert!(claim_event(root) < claim_event(TaskId::new(1)));
    // The same order ties back to ID once nothing is released.
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(1, true)).unwrap();
    assert_eq!(outcome["runs"][0]["task_id"], 1);
    assert_eq!(outcome["runs"][1]["task_id"], 2);
}

/// A worker's claim records the versions of the instructions it reads
/// (goal 113) under their own keys: its prompt's template, the plugin its
/// Claude Code installs and the repository's instruction documents at the
/// run's base, each the hash of its content; the versions of the other
/// providers are not kept.
#[test]
fn the_claim_records_the_versions_of_the_workers_instructions() {
    use dagq::domain::instructions::{content_hash, template_hash};
    let (_dir, repo, db) = fixture();
    fs::write(repo.join("AGENTS.md"), "rules\n").unwrap();
    git(&repo, &["add", "AGENTS.md"]);
    git(&repo, &["commit", "-qm", "instructions"]);
    let config = tempfile::tempdir().unwrap();
    let plugin = config.path().join("plugin");
    fs::create_dir_all(plugin.join("skills/dagq")).unwrap();
    fs::write(plugin.join("skills/dagq/SKILL.md"), "skill").unwrap();
    fs::create_dir_all(config.path().join("plugins")).unwrap();
    fs::write(
        config.path().join("plugins/installed_plugins.json"),
        json!({"version": 2, "plugins": {
            "claude-dagq@dagq": [{"scope": "user", "installPath": plugin}]
        }})
        .to_string(),
    )
    .unwrap();
    let mut options = supervise_options(1, true);
    options.claude_config_dir = Some(config.path().to_owned());
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise_with(&db, &repo, &backend, &options).unwrap();

    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let claimed = &detail
        .events
        .iter()
        .find(|event| event.kind == "run_claimed")
        .unwrap()
        .payload;
    let blob = Command::new(git_executable().unwrap())
        .arg("-C")
        .arg(&repo)
        .args(["rev-parse", "HEAD:AGENTS.md"])
        .bounded_output()
        .unwrap();
    let blob = String::from_utf8(blob.stdout).unwrap().trim().to_owned();
    assert_eq!(
        claimed["instructions_prompt"],
        template_hash(
            &dagq::application::prompt::worker_template(dagq::domain::Provider::Claude).unwrap()
        ),
        "{claimed}"
    );
    assert_eq!(
        claimed["instructions_plugin"],
        content_hash([("skills/dagq/SKILL.md", "skill")]),
        "{claimed}"
    );
    assert_eq!(
        claimed["instructions_repo"],
        content_hash([("AGENTS.md", blob)]),
        "{claimed}"
    );
    assert!(
        claimed.get("instructions_by_provider").is_none(),
        "{claimed}"
    );
}

/// The supervisor claims in the order `candidates` and `graph` show: the
/// highest effective priority first, whatever the ID or unblocks, and a
/// candidate that an urgent ready task waits for inherits urgent
/// (ADR-0040 decision 4).
#[test]
fn supervisor_claims_by_effective_priority_like_candidates_and_graph() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let low = add_ready_task(&mut queue, "later", &[]);
    queue.set_priority(low, Some(Priority::Low)).unwrap();
    let base = add_ready_task(&mut queue, "base", &[]);
    let waiter = add_ready_task(&mut queue, "urgent waiter", &[base]);
    queue.set_priority(waiter, Some(Priority::Urgent)).unwrap();
    let high = add_ready_task(&mut queue, "high", &[]);
    queue.set_priority(high, Some(Priority::High)).unwrap();
    let expected = [base, high, TaskId::new(1), low];
    let candidates: Vec<TaskId> = queue.candidates().unwrap().iter().map(|t| t.id()).collect();
    assert_eq!(candidates, expected);
    assert_eq!(
        dependency_graph(queue.graph_input().unwrap(), None).candidates,
        expected
    );
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(1, true)).unwrap();
    let claimed: Vec<TaskId> = outcome["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| TaskId::new(run["task_id"].as_i64().unwrap()))
        .collect();
    assert_eq!(claimed, expected);
}

#[test]
fn claim_creates_a_lease_that_only_its_owner_can_use_or_release() {
    use dagq::{domain::ClaimOutcome, infrastructure::runtime_store::RunPlan};
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.bind_repository("/repo/one/.git").unwrap();
    assert!(queue.bind_repository("/repo/two/.git").is_err());
    let base = "0123456789abcdef0123456789abcdef01234567";
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&sha(base), &LeaseToken::new("first"))
        .unwrap()
    else {
        panic!()
    };
    assert!(matches!(
        queue
            .claim_for_supervisor(&sha(base), &LeaseToken::new("first"))
            .unwrap(),
        ClaimOutcome::NoReadyTask
    ));
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_eq!(lease.pid, std::process::id());
    assert_eq!(queue.run_leases().unwrap().len(), 1);
    // An idle supervisor heartbeats nothing; the owner heartbeats its runs.
    assert_eq!(
        queue.heartbeat_leases(&LeaseToken::new("second")).unwrap(),
        0
    );
    assert_eq!(
        queue.heartbeat_leases(&LeaseToken::new("first")).unwrap(),
        1
    );
    let plan = RunPlan {
        repo_path: "/test".into(),
        run_dir: "/run".into(),
        branch: "dagq/test".into(),
        worktree_path: "/run/worktree".into(),
        receipt_path: "/run/receipt.json".into(),
        log_path: "/run/log".into(),
    };
    assert!(
        queue
            .plan_run(run.id(), &LeaseToken::new("second"), &plan)
            .is_err()
    );
    let raw = Connection::open(&db).unwrap();
    raw.execute("UPDATE run_leases SET heartbeat_at=0", [])
        .unwrap();
    assert!(
        queue
            .release_lease(run.id(), &LeaseToken::new("second"))
            .is_err()
    );
    assert_eq!(queue.run_lease(run.id()).unwrap().unwrap().heartbeat_at, 0);
    // A stale lease of the writer's own token is renewed, not refused
    // (ADR-0039 decision 7).
    queue
        .plan_run(run.id(), &LeaseToken::new("first"), &plan)
        .unwrap();
    assert!(queue.run_lease(run.id()).unwrap().unwrap().heartbeat_at > 0);
    queue
        .release_lease(run.id(), &LeaseToken::new("first"))
        .unwrap();
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert!(
        queue
            .release_lease(run.id(), &LeaseToken::new("first"))
            .is_err()
    );
    // The token stays on the run as a record of who executed it.
    let raw_token: String = raw
        .query_row(
            "SELECT supervisor_token FROM task_runs WHERE id=?1",
            [&run.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(raw_token, "first");
}

/// IDs handed out in order.
struct FixedIds(Mutex<Vec<&'static str>>);

impl IdGenerator for FixedIds {
    fn uuid(&self) -> String {
        self.0.lock().unwrap().remove(0).to_owned()
    }
}

/// The store takes its times and run IDs from the injected generators and
/// writes them to SQLite: the registration, the claim, the run's lease and
/// the heartbeats. Whether a lease is stale is `lease_is_stale`'s unit test
/// in `infrastructure::runtime_store` (task 1709).
#[test]
fn an_injected_clock_stamps_the_heartbeats_and_injected_ids_name_the_run() {
    use dagq::{
        domain::{ClaimOutcome, HEARTBEAT_TIMEOUT_SECS},
        infrastructure::runtime_store::RunPlan,
    };
    const T: i64 = 1_900_000_000;
    const RUN: &str = "11111111-1111-4111-8111-111111111111";
    let (_dir, _repo, db) = fixture();
    let clock = ManualClock::at(T);
    let mut queue = SqliteQueue::open(&db).unwrap().with_generators(Generators {
        clock: Arc::new(clock.clone()),
        ids: Arc::new(FixedIds(Mutex::new(vec![RUN]))),
    });
    let registration = queue
        .register_supervisor(&LeaseToken::new("first"), 1, 1, VERSION)
        .unwrap();
    assert_eq!((registration.started_at, registration.heartbeat_at), (T, T));
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(
            &sha("0123456789abcdef0123456789abcdef01234567"),
            &LeaseToken::new("first"),
        )
        .unwrap()
    else {
        panic!()
    };
    // The run ID, the claim time and the first heartbeat come from the
    // generators, the times in the form the columns always had.
    assert_eq!(run.id().as_str(), RUN);
    assert_eq!(run.created_at(), "2030-03-17T17:46:40.000Z");
    let task = queue.show(run.task_id()).unwrap().task;
    assert_eq!(task.updated_at(), "2030-03-17T17:46:40.000Z");
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_eq!(lease.heartbeat_at, T);
    let plan = RunPlan {
        repo_path: "/test".into(),
        run_dir: "/run".into(),
        branch: "dagq/test".into(),
        worktree_path: "/run/worktree".into(),
        receipt_path: "/run/receipt.json".into(),
        log_path: "/run/log".into(),
    };
    // The store stamps heartbeats by the same clock, both the process
    // heartbeat and the renewal of a lease-guarded write.
    clock.set(T + HEARTBEAT_TIMEOUT_SECS + 1);
    assert_eq!(
        queue.heartbeat(&LeaseToken::new("first")).unwrap().leases,
        1
    );
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_eq!(lease.heartbeat_at, T + HEARTBEAT_TIMEOUT_SECS + 1);
    assert_eq!(
        queue.supervisors().unwrap()[0].heartbeat_at,
        T + HEARTBEAT_TIMEOUT_SECS + 1
    );
    clock.set(T + HEARTBEAT_TIMEOUT_SECS + 5);
    queue
        .plan_run(run.id(), &LeaseToken::new("first"), &plan)
        .unwrap();
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_eq!(lease.heartbeat_at, T + HEARTBEAT_TIMEOUT_SECS + 5);
}

/// ADR-0039 decision 7: a host sleep jumps the wall clock 120 s past the
/// last heartbeat between two lease-guarded writes. While the lease row
/// still carries the supervisor's token, the next write renews it and goes
/// on, so no other supervisor adopts the run afterwards. A supervisor that
/// stays asleep until another one adopted its stale lease (the adoption of a
/// dead supervisor's lease works as before) is refused and writes nothing.
#[test]
fn a_lease_of_its_own_token_is_renewed_after_a_host_sleep_until_another_supervisor_adopts_it() {
    use dagq::{
        domain::ClaimOutcome,
        infrastructure::runtime_store::{RunPlan, lease_is_stale},
    };
    const T: i64 = 1_900_000_000;
    const SLEEP: i64 = 120;
    let (_dir, _repo, db) = fixture();
    let clock = ManualClock::at(T);
    let mut queue = SqliteQueue::open(&db).unwrap().with_generators(Generators {
        clock: Arc::new(clock.clone()),
        ids: Arc::new(FixedIds(Mutex::new(vec![
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
        ]))),
    });
    add_ready_task(&mut queue, "second", &[]);
    let plan = |run: &TaskRun| RunPlan {
        repo_path: "/test".into(),
        run_dir: format!("/run/{}", run.id()),
        branch: format!("dagq/{}", run.id()),
        worktree_path: format!("/run/{}/worktree", run.id()),
        receipt_path: format!("/run/{}/receipt.json", run.id()),
        log_path: format!("/run/{}/log", run.id()),
    };
    let base = sha("0123456789abcdef0123456789abcdef01234567");
    let start = |queue: &mut SqliteQueue, token: &str| {
        let ClaimOutcome::Claimed { run } = queue
            .claim_for_supervisor(&base, &LeaseToken::new(token))
            .unwrap()
        else {
            panic!()
        };
        queue
            .plan_run(run.id(), &LeaseToken::new(token), &plan(&run))
            .unwrap();
        run
    };

    // The sleeper claims and plans at T, then the host sleeps 120 s.
    let run = start(&mut queue, "sleeper");
    clock.set(T + SLEEP);
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert!(lease_is_stale(&lease, T + SLEEP));
    // Woken up, it goes on with the run: every write renews the lease.
    queue
        .workspace_created(run.id(), &LeaseToken::new("sleeper"), "ws-1")
        .unwrap();
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_eq!(
        (lease.token.as_str(), lease.heartbeat_at),
        ("sleeper", T + SLEEP)
    );
    queue
        .register_wrapper(run.id(), &LeaseToken::new("sleeper"), std::process::id())
        .unwrap();
    queue
        .register_agent(run.id(), std::process::id(), std::process::id())
        .unwrap();
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Running);
    // The renewed lease is fresh, so another supervisor does not adopt it.
    assert!(
        queue
            .adopt_run(
                run.id(),
                &LeaseToken::new("sleeper"),
                &LeaseToken::new("other"),
                2,
                json!({})
            )
            .unwrap()
            .is_none()
    );
    assert!(
        queue
            .holds_lease(run.id(), &LeaseToken::new("sleeper"))
            .unwrap()
    );
    assert_eq!(supervisor_token_of(&db, &run), "sleeper");
    assert!(!queue.has_run_event(run.id(), "run_adopted").unwrap());

    // A second run whose supervisor sleeps until another one adopts it.
    let taken = start(&mut queue, "late");
    queue
        .workspace_created(taken.id(), &LeaseToken::new("late"), "ws-2")
        .unwrap();
    queue
        .register_wrapper(taken.id(), &LeaseToken::new("late"), std::process::id())
        .unwrap();
    queue
        .register_agent(taken.id(), std::process::id(), std::process::id())
        .unwrap();
    clock.set(T + 2 * SLEEP);
    let adopted = queue
        .adopt_run(
            taken.id(),
            &LeaseToken::new("late"),
            &LeaseToken::new("adopter"),
            3,
            json!({}),
        )
        .unwrap()
        .unwrap();
    assert_eq!(adopted.status(), RunStatus::Running);
    let events = queue.show(taken.task_id()).unwrap().events.len();
    // Woken up after the adoption, the late supervisor is refused and
    // writes nothing: the lease, the run and its events stay the adopter's.
    let error = queue
        .finish_supervision(taken.id(), &LeaseToken::new("late"), false)
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "run lease is missing or held by another supervisor"
    );
    let lease = queue.run_lease(taken.id()).unwrap().unwrap();
    assert_eq!((lease.token.as_str(), lease.pid), ("adopter", 3));
    assert_eq!(supervisor_token_of(&db, &taken), "adopter");
    assert_eq!(queue.run(taken.id()).unwrap().status(), RunStatus::Running);
    assert_eq!(queue.show(taken.task_id()).unwrap().events.len(), events);
    // The adopter's own writes go on.
    queue
        .finish_supervision_live(taken.id(), &LeaseToken::new("adopter"))
        .unwrap();
    assert!(
        !queue
            .holds_lease(taken.id(), &LeaseToken::new("late"))
            .unwrap()
    );
}

#[test]
fn shell_arguments_round_trip_without_expansion_and_cmux_handles_are_strict() {
    let value = "a'b $HOME $(echo injected) `echo injected`\nmore";
    let result = Command::new("/bin/sh")
        .arg("-c")
        .arg(shell_join(&["printf".into(), "%s".into(), value.into()]))
        .bounded_output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(String::from_utf8(result.stdout).unwrap(), value);
    assert_eq!(workspace_handle("OK workspace:7\n").unwrap(), "workspace:7");
    assert!(workspace_handle("OK workspace:7; touch file").is_err());
    assert!(workspace_handle("OK surface:7").is_err());
}

#[test]
fn migration_from_v1_preserves_task_and_initializes_runtime_tables() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("old.db");
    let raw = Connection::open(&db).unwrap();
    raw.execute_batch(include_str!("../../migrations/0001_queue.sql"))
        .unwrap();
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 1).unwrap();
    raw.execute("INSERT INTO tasks(title,description,acceptance,verification_commands) VALUES ('preserved','','','[]')", []).unwrap();
    assert!(SqliteQueue::open(&db).is_err());
    SqliteQueue::migrate(&db, None, 0).unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.title(),
        "preserved"
    );
    assert!(queue.run_leases().unwrap().is_empty());
}

#[test]
fn wrapper_registration_is_one_shot_and_rejects_other_owners() {
    use dagq::{
        domain::ClaimOutcome,
        infrastructure::runtime_store::{RunPlan, Validation},
    };
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(
            &sha("0123456789abcdef0123456789abcdef01234567"),
            &LeaseToken::new("owner"),
        )
        .unwrap()
    else {
        panic!()
    };
    queue
        .plan_run(
            run.id(),
            &LeaseToken::new("owner"),
            &RunPlan {
                repo_path: "/test".into(),
                run_dir: "/run".into(),
                branch: "dagq/test".into(),
                worktree_path: "/run/worktree".into(),
                receipt_path: "/run/receipt.json".into(),
                log_path: "/run/log".into(),
            },
        )
        .unwrap();
    assert!(
        queue
            .register_wrapper(run.id(), &LeaseToken::new("owner"), 10)
            .is_err()
    ); // Workspace not attached yet.
    queue
        .workspace_created(run.id(), &LeaseToken::new("owner"), "workspace")
        .unwrap();
    assert!(
        queue
            .register_wrapper(run.id(), &LeaseToken::new("other-owner"), 10)
            .is_err()
    );
    let raw = Connection::open(&db).unwrap();
    raw.execute("UPDATE run_leases SET heartbeat_at=0", [])
        .unwrap();
    // A stale lease of the owner's token is renewed, not refused (ADR-0039
    // decision 7).
    queue
        .register_wrapper(run.id(), &LeaseToken::new("owner"), 10)
        .unwrap();
    assert!(queue.run_lease(run.id()).unwrap().unwrap().heartbeat_at > 0);
    assert!(
        queue
            .register_wrapper(run.id(), &LeaseToken::new("owner"), 11)
            .is_err()
    );
    assert!(queue.register_agent(run.id(), 11, 12).is_err());
    queue.register_agent(run.id(), 10, 12).unwrap();
    assert!(
        queue
            .finish_supervision(run.id(), &LeaseToken::new("owner"), false)
            .is_err()
    ); // Still live.
    queue.wrapper_exited(run.id(), 10, 0).unwrap();
    assert!(queue.heartbeat_wrapper(run.id(), 10).is_err());
    assert_eq!(
        queue
            .finish_supervision(run.id(), &LeaseToken::new("owner"), false)
            .unwrap()
            .status(),
        RunStatus::Validating
    );
    let validation = Validation {
        accepted: false,
        result_commit: None,
        reason: Some("receipt was not submitted".into()),
        code: Some(ReasonCode::ReceiptMissing),
        receipt: Value::Null,
        evidence_missing: Vec::new(),
        scope_violation: Vec::new(),
        allowed_paths: Vec::new(),
        e2e_requirement: None,
        load: Default::default(),
    };
    assert!(
        queue
            .finish_validation(run.id(), &LeaseToken::new("other-owner"), &validation)
            .is_err()
    );
    let failed = queue
        .finish_validation(run.id(), &LeaseToken::new("owner"), &validation)
        .unwrap();
    assert_eq!(failed.status(), RunStatus::Failed);
    assert_eq!(failed.last_error(), Some("receipt was not submitted"));
    assert!(
        queue
            .finish_validation(run.id(), &LeaseToken::new("owner"), &validation)
            .is_err()
    ); // Terminal.
    // A failed run never records a workspace close or cleanup failure.
    assert!(
        queue
            .workspace_closed(run.id(), &LeaseToken::new("owner"))
            .is_err()
    );
    assert!(
        queue
            .cleanup_failed(
                run.id(),
                &LeaseToken::new("owner"),
                "late",
                &ReasonCode::BackendFailed.into()
            )
            .is_err()
    );
}

#[test]
fn workspace_close_is_recorded_once_and_only_for_accepted_runs() {
    use dagq::{
        domain::ClaimOutcome,
        infrastructure::runtime_store::{RunPlan, Validation},
    };
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(
            &sha("0123456789abcdef0123456789abcdef01234567"),
            &LeaseToken::new("owner"),
        )
        .unwrap()
    else {
        panic!()
    };
    queue
        .plan_run(
            run.id(),
            &LeaseToken::new("owner"),
            &RunPlan {
                repo_path: "/test".into(),
                run_dir: "/run".into(),
                branch: "dagq/test".into(),
                worktree_path: "/run/worktree".into(),
                receipt_path: "/run/receipt.json".into(),
                log_path: "/run/log".into(),
            },
        )
        .unwrap();
    // The handle of the run's background wrapper, as the supervisor
    // records it (a run opens no workspace, ADR-t1433-3).
    queue
        .workspace_created(run.id(), &LeaseToken::new("owner"), "background:10:start")
        .unwrap();
    queue
        .register_wrapper(run.id(), &LeaseToken::new("owner"), 10)
        .unwrap();
    queue.register_agent(run.id(), 10, 12).unwrap();
    // Still running: neither close nor cleanup failure may be recorded.
    assert!(
        queue
            .workspace_closed(run.id(), &LeaseToken::new("owner"))
            .is_err()
    );
    assert!(
        queue
            .cleanup_failed(
                run.id(),
                &LeaseToken::new("owner"),
                "early",
                &ReasonCode::BackendFailed.into()
            )
            .is_err()
    );
    queue.wrapper_exited(run.id(), 10, 0).unwrap();
    queue
        .finish_supervision(run.id(), &LeaseToken::new("owner"), false)
        .unwrap();
    let accepted = queue
        .finish_validation(
            run.id(),
            &LeaseToken::new("owner"),
            &Validation {
                accepted: true,
                result_commit: Some(sha("89abcdef0123456789abcdef0123456789abcdef")),
                reason: None,
                code: None,
                receipt: Value::Null,
                evidence_missing: Vec::new(),
                scope_violation: Vec::new(),
                allowed_paths: Vec::new(),
                e2e_requirement: None,
                load: Default::default(),
            },
        )
        .unwrap();
    assert_eq!(accepted.status(), RunStatus::AwaitingIntegration);
    assert!(accepted.workspace_closed_at().is_none());
    assert!(
        queue
            .workspace_closed(run.id(), &LeaseToken::new("other-owner"))
            .is_err()
    );
    let failed = queue
        .cleanup_failed(
            run.id(),
            &LeaseToken::new("owner"),
            "cmux down",
            &ReasonCode::BackendFailed.into(),
        )
        .unwrap();
    assert_eq!(failed.status(), RunStatus::AwaitingIntegration);
    assert_eq!(failed.last_error(), Some("cmux down"));
    assert!(failed.workspace_closed_at().is_none());
    // A later successful close clears nothing but records the close once.
    let closed = queue
        .workspace_closed(run.id(), &LeaseToken::new("owner"))
        .unwrap();
    assert!(closed.workspace_closed_at().is_some());
    assert_eq!(closed.status(), RunStatus::AwaitingIntegration);
    assert!(
        queue
            .workspace_closed(run.id(), &LeaseToken::new("owner"))
            .is_err()
    );
    assert!(
        queue
            .cleanup_failed(
                run.id(),
                &LeaseToken::new("owner"),
                "late",
                &ReasonCode::BackendFailed.into()
            )
            .is_err()
    );
    let kinds: Vec<String> = queue
        .show(TaskId::new(1))
        .unwrap()
        .events
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert_eq!(kinds.iter().filter(|k| *k == "workspace_closed").count(), 1);
    assert_eq!(kinds.iter().filter(|k| *k == "cleanup_failed").count(), 1);
}

#[test]
fn no_ready_task_ends_a_once_pass_without_creating_a_run_or_lease() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(TaskId::new(1), TaskAction::Draft).unwrap();
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["runs"], json!([]));
    assert_eq!(outcome["errors"], json!([]));
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(queue.supervisors().unwrap().is_empty());
    assert!(queue.show(TaskId::new(1)).unwrap().runs.is_empty());
    assert!(supervise_with(&db, &repo, &backend, &supervise_options(0, true)).is_err());
    assert!(queue.supervisors().unwrap().is_empty());
}

/// Wait until the heartbeat of the one registration, `registered`, is
/// fresh again after a test set it to 0. Nothing in `supervise` removes or
/// replaces its own row while it runs (the heartbeat only updates it; `up`
/// and `down` prune), so an empty list is the gap between the supervisor
/// deregistering and its thread ending, waited through. A supervisor that
/// ended (a heartbeat failure keeps its row) fails with its own result
/// instead of an index panic or a wait for nothing (task 722), and a row
/// of another token or pid fails too.
#[track_caller]
fn wait_for_heartbeat(
    db: &Path,
    registered: &LeaseToken,
    supervisor: &mut Option<thread::JoinHandle<Result<Value>>>,
) {
    let mut ended = false;
    wait_until(db, Duration::from_secs(10), |queue| {
        let fresh = queue.supervisors().unwrap().first().is_some_and(|row| {
            assert_eq!(&row.token, registered, "another registration");
            assert_eq!(row.pid, std::process::id(), "another process");
            row.heartbeat_at > 0
        });
        ended = supervisor.as_ref().is_some_and(|s| s.is_finished());
        fresh || ended
    });
    if ended {
        let outcome = joined(supervisor.take().unwrap(), "the ended supervisor thread");
        panic!("the supervisor ended while it was expected to heartbeat: {outcome:?}");
    }
}

/// A resident supervisor that holds no run is still listed by `status` and
/// `doctor` through its registration, which its heartbeat refreshes and a
/// graceful stop removes.
#[test]
fn resident_supervisor_without_runs_is_listed_until_it_stops() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(TaskId::new(1), TaskAction::Draft).unwrap();
    assert_eq!(runtime::status(&db).unwrap()["supervisors"], json!([]));
    let backend = Arc::new(TestWorkspace::new(&db, true, VALID_AGENT));
    let options = supervise_options(3, false);
    let mut supervisor = Some({
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    });
    wait_until(&db, Duration::from_secs(10), |queue| {
        queue.supervisors().unwrap().len() == 1
    });
    let registered = queue.supervisors().unwrap().remove(0);
    assert_eq!(registered.pid, std::process::id());
    assert_eq!(registered.parallel, 3);
    for report in [
        runtime::status(&db).unwrap(),
        runtime::doctor(&db, true).unwrap(),
    ] {
        assert_eq!(report["runs"], json!([]));
        assert_eq!(report["supervisors"].as_array().unwrap().len(), 1);
        let entry = &report["supervisors"][0];
        assert_eq!(entry["pid"], json!(std::process::id()));
        assert_eq!(entry["alive"], true);
        assert_eq!(entry["registered"], true);
        assert_eq!(entry["parallel"], 3);
        assert_eq!(entry["started_at"], json!(registered.started_at));
        assert_eq!(entry["stale"], false);
        assert!(entry["heartbeat_age_secs"].as_i64().unwrap() <= 5);
        assert_eq!(entry["run_ids"], json!([]));
    }
    // The heartbeat keeps the registration fresh while nothing runs.
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE supervisors SET heartbeat_at=0", [])
        .unwrap();
    wait_for_heartbeat(&db, &registered.token, &mut supervisor);
    assert!(queue.run_leases().unwrap().is_empty());

    options.stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor.unwrap(), "the supervisor thread to return").unwrap();
    assert_eq!(outcome["outcome"], "stopped");
    assert_eq!(outcome["runs"], json!([]));
    assert!(queue.supervisors().unwrap().is_empty());
    assert_eq!(runtime::status(&db).unwrap()["supervisors"], json!([]));
    assert_eq!(
        runtime::doctor(&db, true).unwrap()["supervisors"],
        json!([])
    );

    // A landing branch that no longer resolves (here: main vanished)
    // holds the claims while the supervisor stays (ADR-t615-1). An error
    // out of the loop itself (here: main names no commit) ends the process
    // with nothing active, so it deregisters too. Main may vanish in the
    // middle of a pass here; the claim holds then too
    // (`main_vanishing_after_the_landing_branch_check_holds_the_claim`).
    let options = supervise_options(1, false);
    let mut supervisor = Some({
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    });
    wait_until(&db, Duration::from_secs(10), |queue| {
        queue.supervisors().unwrap().len() == 1
    });
    let registered = queue.supervisors().unwrap().remove(0).token;
    git(&repo, &["update-ref", "-d", "refs/heads/main"]);
    queue
        .transition(TaskId::new(1), TaskAction::BypassReview)
        .unwrap();
    // Two heartbeats: passes that found the task and claimed nothing.
    for _ in 0..2 {
        Connection::open(&db)
            .unwrap()
            .execute("UPDATE supervisors SET heartbeat_at=0", [])
            .unwrap();
        wait_for_heartbeat(&db, &registered, &mut supervisor);
    }
    assert!(queue.show(TaskId::new(1)).unwrap().runs.is_empty());
    let blob = Command::new(git_executable().expect("git executable"))
        .arg("-C")
        .arg(&repo)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(std::process::Stdio::null())
        .bounded_output()
        .unwrap();
    assert!(blob.status.success());
    let common_dir = git_out(&repo, &["rev-parse", "--git-common-dir"]);
    let common_dir = repo.join(common_dir.trim());
    fs::write(
        common_dir.join("refs/heads/main"),
        String::from_utf8_lossy(&blob.stdout).as_bytes(),
    )
    .unwrap();
    let error = format!(
        "{:#}",
        joined(supervisor.unwrap(), "the supervisor thread to return").unwrap_err()
    );
    assert!(error.contains("Needed a single revision"), "{error}");
    assert!(queue.supervisors().unwrap().is_empty());
    assert!(queue.show(TaskId::new(1)).unwrap().runs.is_empty());
}

thread_local! {
    /// The repository whose main [`load_deleting_main`] deletes, once, on
    /// the supervisor's thread only.
    static DELETE_MAIN_IN: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// A load average port that deletes `refs/heads/main` of the repository
/// armed in [`DELETE_MAIN_IN`] the first time it is read. A pass with no
/// run reads the load only when it judges the claim hold, after the check
/// of the landing branch at the top of the pass and before the claim reads
/// main.
fn load_deleting_main() -> Option<f64> {
    if let Some(repo) = DELETE_MAIN_IN.with(|armed| armed.borrow_mut().take()) {
        git(&repo, &["update-ref", "-d", "refs/heads/main"]);
    }
    None
}

/// Main vanishing between the check of the landing branch at the top of a
/// pass and the claim holds the claims like a landing branch that did not
/// resolve at the top (ADR-t615-1): the pass ends without a run and the
/// loop does not fail. Before the claim reread the landing branch, it
/// ended the loop with the error of `main_head`, which deregistered a
/// resident supervisor in the second stage of
/// `resident_supervisor_without_runs_is_listed_until_it_stops` (task 1018).
#[test]
fn main_vanishing_after_the_landing_branch_check_holds_the_claim() {
    let (dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    let options = SuperviseOptions {
        load_average: load_deleting_main,
        ..supervise_options(1, true)
    };
    DELETE_MAIN_IN.with(|armed| *armed.borrow_mut() = Some(repo.clone()));
    let telemetry = Telemetry::open(&dir.path().join("logs"), "supervise");
    let outcome = telemetry
        .in_scope(|| supervise_with(&db, &repo, &backend, &options))
        .unwrap();
    assert!(
        DELETE_MAIN_IN.with(|armed| armed.borrow().is_none()),
        "the pass never read the load"
    );
    assert_eq!(outcome["outcome"], "finished", "{outcome}");
    assert_eq!(outcome["runs"], json!([]));
    assert_eq!(outcome["errors"], json!([]));
    let status = Command::new(git_executable().expect("git executable"))
        .arg("-C")
        .arg(&repo)
        .args(["rev-parse", "--verify", "--quiet", "refs/heads/main"])
        .bounded_output()
        .unwrap()
        .status;
    assert!(!status.success(), "main survived the pass");
    let mut queue = SqliteQueue::open(&db).unwrap();
    // A failed backend call reads the load too; none deleted main before
    // the check at the top of the pass.
    assert!(
        queue
            .all_events()
            .unwrap()
            .iter()
            .all(|event| event.kind != "backend_call_failed")
    );
    let task = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(task.task.status(), TaskStatus::Ready);
    assert!(task.runs.is_empty());
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(queue.supervisors().unwrap().is_empty());

    // Queue state alone also passes if a new load read deletes main before
    // the top-of-pass check. Require the claim's mid-pass recheck itself.
    let log = fs::read_to_string(telemetry.path.as_ref().unwrap()).unwrap();
    let records: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        records.iter().any(|record| {
            record["fields"]["event"] == "claim_landing_branch_unresolved"
                && record["target"] == "dagq::application::supervise"
        }),
        "the claim did not recheck the landing branch during the pass: {log}"
    );
}

/// A supervisor's progress goes to its process's JSON Lines file in the
/// log directory (ADR-0033): one record per line, with the startup facts,
/// the progress messages that also go to stderr, their run and task IDs as
/// fields, and the final result.
#[test]
fn supervise_records_its_progress_as_json_lines() {
    let (dir, repo, db) = fixture();
    let log_dir = dir.path().join("logs").join("nested");
    let options = supervise_options(2, true);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let telemetry = Telemetry::open(&log_dir, "supervise");
    let outcome = telemetry
        .in_scope(|| supervise_with(&db, &repo, &backend, &options))
        .unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    let pid = std::process::id();
    let path = telemetry.path.clone().unwrap();
    let name = path.file_name().unwrap().to_str().unwrap();
    assert!(
        name.starts_with("supervise-") && name.ends_with(&format!("Z-{pid}.jsonl")),
        "{name}"
    );
    assert_eq!(path.parent().unwrap(), log_dir);
    let text = fs::read_to_string(&path).unwrap();
    let records: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records[0]["message"], "dagq supervise started");
    let messages: Vec<&str> = records
        .iter()
        .map(|r| r["message"].as_str().unwrap())
        .collect();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs.remove(0);
    let token: String = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT supervisor_token FROM task_runs WHERE id=?1",
            [&run.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        messages.contains(&format!(
            "supervisor {token} started: version {VERSION}, pid {pid}, parallel 2, db {}, repository {}",
            db.canonicalize().unwrap().display(),
            repo.canonicalize().unwrap().display()
        ).as_str()),
        "{text}"
    );
    let running = records
        .iter()
        .find(|r| {
            r["message"]
                == format!(
                    "task 1 running in the background as {}; run {}",
                    background_session(&run),
                    run.id()
                )
        })
        .unwrap();
    assert_eq!(running["fields"]["run_id"], run.id().as_str());
    assert_eq!(running["fields"]["task_id"], "1");
    assert_eq!(running["level"], "INFO");
    assert_eq!(running["target"], "dagq::application::supervise::session");
    assert!(text.contains(&format!("receipt received for {}", run.id())));
    assert!(text.contains(&format!("run {} is awaiting_integration", run.id())));
    assert!(messages.iter().any(|m| m.starts_with(&format!(
        "supervisor {token} exiting: {{\"errors\":[],\"outcome\":\"finished\""
    ))));
}

/// A registration whose process died, or whose heartbeat stopped, is
/// reported as stale by `status` and `doctor` and left for a person;
/// neither a later supervisor nor `recover` removes it, and an `integrate`
/// or orphaned lease holder is listed next to it without a registration.
#[test]
fn killed_supervisor_registration_is_reported_stale_and_never_deleted() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let dead = dead_pid();
    let killed = queue
        .register_supervisor(&LeaseToken::new("killed"), dead, 4, VERSION)
        .unwrap();
    // Killed a moment ago: the heartbeat is fresh, the pid is gone.
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["runs"], json!([]));
    let entry = &status["supervisors"][0];
    assert_eq!(entry["pid"], json!(dead));
    assert_eq!(entry["alive"], false);
    assert_eq!(entry["stale"], true);
    assert_eq!(entry["registered"], true);
    assert_eq!(entry["parallel"], 4);
    // The build the process ran, which `up` compares against its own.
    assert_eq!(entry["binary_version"], VERSION);
    assert_eq!(entry["heartbeat_at"], json!(killed.heartbeat_at));
    assert!(entry["heartbeat_age_secs"].as_i64().unwrap() <= 5);
    // Alive but silent: stale by heartbeat age alone.
    queue
        .register_supervisor(&LeaseToken::new("hung"), std::process::id(), 1, VERSION)
        .unwrap();
    let raw = Connection::open(&db).unwrap();
    raw.execute(
        "UPDATE supervisors SET heartbeat_at=1700000000 WHERE token='hung'",
        [],
    )
    .unwrap();
    drop(raw);
    let doctor = runtime::doctor(&db, true).unwrap();
    assert_eq!(doctor["supervisors"].as_array().unwrap().len(), 2);
    let hung = &doctor["supervisors"][1];
    assert_eq!(hung["alive"], true);
    assert_eq!(hung["stale"], true);
    assert!(hung["heartbeat_age_secs"].as_i64().unwrap() > 30);
    assert_eq!(hung["run_ids"], json!([]));
    let compact = runtime::doctor(&db, false).unwrap();
    let keys: Vec<&String> = compact["supervisors"][1]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(
        keys,
        [
            "alive",
            "auto_update",
            "binary_version",
            "heartbeat_age_secs",
            "max_waiting",
            "max_waiting_source",
            "mode",
            "parallel",
            "parallel_source",
            "pid",
            "providers",
            "registered",
            "run_ids",
            "runtime_planners",
            "runtime_planners_source",
            "stale",
            "workspace_id"
        ]
    );
    assert_eq!(compact["supervisors"][1]["stale"], true);

    // A run owned without a registration (an `integrate` process, or a
    // supervisor from before the registry) is still attributed to its lease.
    let wrapper = Stand::start().unwrap();
    let orphan = orphan_run(&repo, &db, "owner", wrapper.pid, wrapper.pid);
    let status = runtime::status(&db).unwrap();
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 3);
    assert_eq!(supervisors[2]["registered"], false);
    assert_eq!(supervisors[2]["parallel"], Value::Null);
    // A lease holder without a registration recorded no version either.
    assert_eq!(supervisors[2]["binary_version"], Value::Null);
    assert_eq!(supervisors[2]["started_at"], Value::Null);
    assert_eq!(supervisors[2]["pid"], json!(std::process::id()));
    assert_eq!(supervisors[2]["alive"], true);
    assert_eq!(supervisors[2]["stale"], false);
    assert_eq!(supervisors[2]["run_ids"], json!([orphan.id()]));
    assert_eq!(status["runs"][0]["lease"]["pid"], json!(std::process::id()));
    // A registered supervisor's leases join it by token rather than by pid.
    queue
        .register_supervisor(&LeaseToken::new("owner"), std::process::id(), 2, VERSION)
        .unwrap();
    let status = runtime::status(&db).unwrap();
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 3);
    assert_eq!(supervisors[2]["registered"], true);
    assert_eq!(supervisors[2]["parallel"], 2);
    assert_eq!(supervisors[2]["run_ids"], json!([orphan.id()]));

    // Recovery of the run and a later supervisor's own registration and
    // deregistration leave the stale rows alone.
    queue.wrapper_exited(orphan.id(), wrapper.pid, 0).unwrap();
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases", [])
        .unwrap();
    assert_eq!(
        runtime::recover(&db, orphan.id()).unwrap()["run"]["status"],
        "interrupted"
    );
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    assert_eq!(
        supervise(&db, &repo, &backend).unwrap()["outcome"],
        "finished"
    );
    let tokens: Vec<String> = queue
        .supervisors()
        .unwrap()
        .into_iter()
        .map(|s| s.token.into_string())
        .collect();
    assert_eq!(tokens, ["killed", "hung", "owner"]);
    assert_eq!(
        runtime::doctor(&db, true).unwrap()["supervisors"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn recover_requires_dead_processes_and_stale_lease_then_allows_a_new_run() {
    let (_dir, repo, db) = fixture();
    let mut wrapper = sleeper();
    let mut agent = sleeper();
    let run = orphan_run(&repo, &db, "owner", wrapper.id(), agent.id());
    let mut queue = SqliteQueue::open(&db).unwrap();

    // Everything is alive: doctor says so and recover refuses.
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["supervisors"][0]["pid"], json!(std::process::id()));
    assert_eq!(report["supervisors"][0]["stale"], false);
    assert_eq!(report["supervisors"][0]["alive"], true);
    assert_eq!(report["supervisors"][0]["run_ids"], json!([run.id()]));
    let health = &report["runs"][0];
    assert_eq!(health["run_id"], json!(run.id()));
    assert_eq!(health["status"], "running");
    assert_eq!(health["workspace_id"], json!(background_session(&run)));
    assert_eq!(health["lease"]["stale"], false);
    assert_eq!(health["lease"]["alive"], true);
    assert_eq!(health["worktree_exists"], true);
    assert_eq!(health["run_dir_exists"], true);
    assert_eq!(health["receipt_exists"], false);
    assert_eq!(health["recoverable"], false);
    let processes = health["processes"].as_array().unwrap();
    assert_eq!(processes.len(), 2);
    assert!(processes.iter().all(|p| p["alive"] == true));
    let error = format!("{:#}", runtime::recover(&db, run.id()).unwrap_err());
    assert!(
        error.contains("wrapper pid") && error.contains("lease heartbeat"),
        "{error}"
    );
    assert!(
        queue
            .transition(TaskId::new(1), TaskAction::BypassReview)
            .is_err()
    );

    // The supervisor is gone (stale heartbeat, dead PID) but the session is not.
    let raw = Connection::open(&db).unwrap();
    raw.execute("UPDATE run_leases SET heartbeat_at=0, pid=?1", [dead_pid()])
        .unwrap();
    raw.execute("UPDATE run_processes SET heartbeat_at=0", [])
        .unwrap();
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["supervisors"][0]["stale"], true);
    assert_eq!(report["supervisors"][0]["alive"], false);
    assert_eq!(report["runs"][0]["lease"]["stale"], true);
    assert_eq!(report["runs"][0]["processes"][0]["heartbeat_stale"], true);
    let error = format!("{:#}", runtime::recover(&db, run.id()).unwrap_err());
    assert!(
        error.contains("agent pid") && !error.contains("supervisor"),
        "{error}"
    );
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Running);
    assert!(queue.run_lease(run.id()).unwrap().is_some());

    // The session processes are gone too: recovery is allowed and explicit.
    agent.kill().unwrap();
    agent.wait().unwrap();
    wrapper.kill().unwrap();
    wrapper.wait().unwrap();
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["runs"][0]["recoverable"], true);
    assert_eq!(report["runs"][0]["blockers"], json!([]));
    let outcome = runtime::recover(&db, run.id()).unwrap();
    assert_eq!(outcome["outcome"], "recovered");
    assert_eq!(outcome["run"]["status"], "interrupted");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(detail.runs[0].status(), RunStatus::Interrupted);
    assert!(queue.run_leases().unwrap().is_empty());
    let recovered = detail
        .events
        .iter()
        .find(|e| e.kind == "run_recovered")
        .unwrap();
    assert_eq!(recovered.payload["previous_status"], "running");
    assert_eq!(recovered.payload["lease_deleted"], true);
    assert_eq!(recovered.payload["run"]["processes"][0]["alive"], false);
    assert_eq!(recovered.payload["run"]["lease"]["stale"], true);
    // Registrations and resources are left as observed.
    assert!(detail.processes.iter().all(|p| p.exited_at.is_none()));
    assert!(Path::new(run.worktree_path().unwrap()).exists());
    assert!(runtime::recover(&db, run.id()).is_err()); // No longer unfinished.
    assert_eq!(runtime::doctor(&db, true).unwrap()["runs"], json!([]));
    assert!(queue.candidates().unwrap().is_empty());

    // Retry is a separate decision: ready again, then a second run with new paths.
    queue.transition(TaskId::new(1), TaskAction::Ready).unwrap();
    assert_eq!(queue.candidates().unwrap().len(), 1);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2);
    assert_eq!(detail.runs[0].status(), RunStatus::Interrupted);
    assert_eq!(detail.runs[0].worktree_path(), run.worktree_path());
    assert!(Path::new(run.worktree_path().unwrap()).exists());
    assert_ne!(detail.runs[1].worktree_path(), run.worktree_path());
    assert!(
        queue
            .transition(TaskId::new(1), TaskAction::BypassReview)
            .is_err()
    ); // Awaiting integration still owns the task.
}

#[test]
fn recover_ignores_exited_processes_and_tolerates_a_missing_lease() {
    let (_dir, repo, db) = fixture();
    // The wrapper reported its exit before the supervisor died; its live PID
    // (a process standing for it) must not block recovery.
    let wrapper = Stand::start().unwrap();
    let pid = wrapper.pid;
    let run = orphan_run(&repo, &db, "owner", pid, pid);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.wrapper_exited(run.id(), pid, 0).unwrap();
    let error = format!("{:#}", runtime::recover(&db, run.id()).unwrap_err());
    assert!(
        error.contains("supervisor pid") && !error.contains("wrapper pid"),
        "{error}"
    );
    let report = runtime::doctor(&db, true).unwrap();
    assert!(
        report["runs"][0]["processes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["alive"].is_null() && p["heartbeat_stale"] == false)
    );
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases", [])
        .unwrap();
    assert_eq!(
        runtime::doctor(&db, true).unwrap()["supervisors"],
        json!([])
    );
    let outcome = runtime::recover(&db, run.id()).unwrap();
    assert_eq!(outcome["run"]["status"], "interrupted");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let recovered = detail
        .events
        .iter()
        .find(|e| e.kind == "run_recovered")
        .unwrap();
    assert_eq!(recovered.payload["lease_deleted"], false);
    assert_eq!(recovered.payload["run"]["lease"], Value::Null);
    // The task can be edited again before a retry.
    assert!(
        queue
            .add_dependency(TaskId::new(1), TaskId::new(1))
            .is_err()
    );
    queue.transition(TaskId::new(1), TaskAction::Draft).unwrap();
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Draft
    );
    assert!(runtime::recover(&db, &RunId::new("no-such-run").unwrap()).is_err());
}

#[test]
fn a_refused_run_transition_keeps_the_domain_reason_beside_the_old_error() {
    use dagq::{domain::ClaimOutcome, infrastructure::runtime_store::REFUSALS_LOG};
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let base = "0123456789abcdef0123456789abcdef01234567";
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&sha(base), &LeaseToken::new("owner"))
        .unwrap()
    else {
        panic!()
    };
    let run_dir = dagq::infrastructure::location::runs_dir(&db.canonicalize().unwrap())
        .join(run.id().as_str());
    fs::create_dir_all(&run_dir).unwrap();
    // A claimed run is not awaiting integration: the error keeps the
    // store's message, and the domain's reason goes to the run's log.
    let error = queue
        .restart_validation(run.id(), &LeaseToken::new("owner"))
        .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "run is not awaiting integration under this supervisor"
    );
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Claimed);
    let log = fs::read_to_string(run_dir.join(REFUSALS_LOG)).unwrap();
    let line = log.lines().next().unwrap();
    assert!(line.starts_with('['), "{line}");
    assert!(
        line.ends_with(
            "] run is not awaiting integration under this supervisor: \
             cannot validate again a run in claimed state"
        ),
        "{line}"
    );
    // A refusal whose run directory is gone still fails the same way.
    fs::remove_dir_all(&run_dir).unwrap();
    let error = queue
        .restart_validation(run.id(), &LeaseToken::new("owner"))
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "run is not awaiting integration under this supervisor"
    );
    assert!(!run_dir.exists());
}

/// `integrate` reads its time and its token from the generators `main`
/// hands it, not from the queue's own clock and UUIDs: a lease heartbeat
/// fresh to the injected clock holds the run, a stale one lets it through,
/// and only the injected token takes over a lease stored under it.
#[test]
fn integrate_takes_its_time_and_token_from_the_injected_generators() {
    let (_dir, db, detail) = run_agent(
        "git rm -q seed.txt && git commit -q -m 'drop seed'; receipt \"$(git rev-parse HEAD)\"",
    );
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let repo = Path::new(&db).parent().unwrap().join("repo's directory");
    Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,'held',?2,1000)",
            rusqlite::params![run.id(), std::process::id()],
        )
        .unwrap();
    let clock = ManualClock::at(1_005);
    let one_shot = |ids: Vec<&'static str>| {
        runtime::OneShot::new(Generators {
            clock: Arc::new(clock.clone()),
            ids: Arc::new(FixedIds(Mutex::new(ids))),
        })
    };
    let target = || IntegrateTarget::Task(TaskId::new(1));

    // Five seconds after the heartbeat by the injected clock: still held.
    let held = one_shot(vec![])
        .integrate(&db, target(), &repo, None)
        .unwrap_err();
    assert!(
        format!("{held:#}").contains("is held by the supervisor"),
        "{held:#}"
    );

    // Long after it the lease is stale, but a lease is only taken over
    // under its own token.
    clock.set(1_000 + 10 * dagq::domain::HEARTBEAT_TIMEOUT_SECS);
    let leased = one_shot(vec!["other"])
        .integrate(&db, target(), &repo, None)
        .unwrap_err();
    assert!(
        format!("{leased:#}").contains("run is still leased"),
        "{leased:#}"
    );
    let outcome = one_shot(vec!["held"])
        .integrate(&db, target(), &repo, None)
        .unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let started: i64 = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM run_events WHERE run_id=?1 AND kind='integration_started'",
            [run.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(started, 1);
}

/// `status` and `doctor` measure everything to the injected clock's now,
/// read once per call.
#[test]
fn status_and_doctor_measure_to_the_injected_clock() {
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::Blocked,
            task_id: Some(TaskId::new(1)),
            run_id: None,
            question: "which way?".into(),
            options: vec![],
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE asks SET created_at=1000", [])
        .unwrap();
    let one_shot = runtime::OneShot::new(Generators {
        clock: Arc::new(ManualClock::at(1_042)),
        ids: Arc::new(FixedIds(Mutex::new(vec![]))),
    });
    let status = one_shot.status_for(&db, None).unwrap();
    assert_eq!(status["checked_at"], 1_042, "{status}");
    assert_eq!(status["asks"][0]["id"], ask.id.as_i64(), "{status}");
    assert_eq!(status["asks"][0]["age_secs"], 42, "{status}");
    let doctor = one_shot.doctor(&db, false, None).unwrap();
    assert_eq!(doctor["checked_at"], 1_042, "{doctor}");
}
