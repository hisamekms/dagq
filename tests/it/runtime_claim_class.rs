//! Runtime tests: the execution classes share the worker slots
//! (ADR-t1487-2). The supervisor's wiring of the cases: `[supervisor]
//! max_spikes` defers the Spikes past it into `claim_deferred`, which
//! `candidates` shows, and the slot goes to an implementation; without it
//! the Spikes may take every slot; `[supervisor] class_aging` lets the
//! class that waited past it go first within one effective priority, as
//! `candidates` and `graph` show it, and the claim records it. Which candidate a limit holds and
//! which one ages, with the interrupts, the deferrals and the threshold,
//! is judged by the unit tests of `domain::claim_order`.
use crate::runtime_support;
use dagq::domain::EventKind;

use dagq::domain::{
    ClaimOutcome, ExecutionClass, LeaseToken, Priority, provider_switch::WorkerRoute,
    worker::Worker,
};
use runtime_support::*;

/// Add a ready task of `class` and `priority`; its ID.
fn add_task(db: &Path, title: &str, class: ExecutionClass, priority: Priority) -> TaskId {
    let task = add_draft(db, title, class, priority);
    ready(db, task);
    task
}

/// Make the draft `task` ready.
fn ready(db: &Path, task: TaskId) {
    SqliteQueue::open(db)
        .unwrap()
        .transition(task, TaskAction::BypassReview)
        .unwrap();
}

/// Add a draft task of `class` and `priority`: its ID comes now, its wait
/// starts when it is made ready.
fn add_draft(db: &Path, title: &str, class: ExecutionClass, priority: Priority) -> TaskId {
    let mut queue = SqliteQueue::open(db).unwrap();
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Some(priority),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
            execution_class: class,
        })
        .unwrap();
    task.id()
}

/// A worker that writes a Spike's result too: an implementation's run
/// ignores it.
fn agent() -> String {
    format!("{RECEIPT_SPIKE}commit work; receipt_spike \"$(git rev-parse HEAD)\" holds")
}

/// [`agent`] once the test lets it go.
fn gated_agent() -> String {
    format!(
        "{RECEIPT_SPIKE}await_file \"$EXIT.go\"; commit work; receipt_spike \"$(git rev-parse HEAD)\" holds; idle; await_exit"
    )
}

fn cancel(db: &Path, task: TaskId) {
    SqliteQueue::open(db)
        .unwrap()
        .transition(task, TaskAction::Cancel)
        .unwrap();
}

/// Let the gated session of `task`'s first run go, once the run has its
/// directory: a run just claimed gets it when it is provisioned.
fn release(db: &Path, task: TaskId) {
    let mut dir = None;
    wait_until(db, Duration::from_secs(60), |queue| {
        dir = queue.show(task).unwrap().runs[0]
            .run_dir()
            .map(PathBuf::from);
        dir.is_some()
    });
    fs::write(dir.unwrap().join("exit-requested.go"), "").unwrap();
}

/// The queue's events of `kind`, oldest first: each one's task and payload.
fn events(db: &Path, kind: &str) -> Vec<(Option<TaskId>, Value)> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| (event.task_id, event.payload))
        .collect()
}

fn claimed_tasks(db: &Path) -> Vec<i64> {
    events(db, "run_claimed")
        .iter()
        .map(|(task, _)| task.unwrap().as_i64())
        .collect()
}

fn ids(view: &Value) -> Vec<i64> {
    view.as_array()
        .unwrap()
        .iter()
        .map(|task| task.get("id").unwrap_or(task).as_i64().unwrap())
        .collect()
}

/// Three slots and `max_spikes = 1`: task 1, the first Spike and the
/// implementation after the second Spike take the slots in one pass, the
/// second Spike deferred with `spike_limit`, which `candidates` shows and
/// `status` names the limit of. Once the first Spike leaves its slot the
/// second is claimed and its deferral ends; no run is stopped. The claims
/// record their class, and `stats` gives each class's waits.
#[test]
fn the_spike_limit_defers_a_spike_and_gives_its_slot_to_an_implementation() {
    let (_dir, repo, db) = fixture();
    fs::write(repo.join("dagq.toml"), "[supervisor]\nmax_spikes = 1\n").unwrap();
    let first = add_task(&db, "spike one", ExecutionClass::Spike, Priority::Normal);
    let second = add_task(&db, "spike two", ExecutionClass::Spike, Priority::Normal);
    let after = add_task(
        &db,
        "implementation",
        ExecutionClass::Implementation,
        Priority::Normal,
    );
    let backend = Arc::new(TestWorkspace::new(&db, false, &agent()));
    backend.script_for(first.as_i64(), &gated_agent());
    let options = supervise_options(3, true);
    let supervisor = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(60), |_| {
        claimed_tasks(&db).len() >= 3 && !events(&db, "claim_deferred").is_empty()
    });
    let claimed = events(&db, "run_claimed");
    assert_eq!(
        claimed_tasks(&db)[..3],
        [1, first.as_i64(), after.as_i64()],
        "{claimed:?}"
    );
    let slots: Vec<_> = claimed[..3]
        .iter()
        .map(|(_, p)| p["slots"].clone())
        .collect();
    assert_eq!(slots, [json!(0), json!(1), json!(2)], "{claimed:?}");
    assert_eq!(claimed[1].1["execution_class"], "spike", "{claimed:?}");
    assert_eq!(claimed[1].1["aged"], false, "{claimed:?}");
    assert_eq!(claimed[2].1["execution_class"], "implementation");
    let deferred = events(&db, "claim_deferred");
    assert_eq!(deferred.len(), 1, "{deferred:?}");
    assert_eq!(deferred[0].0, Some(second));
    assert_eq!(deferred[0].1["reason"], "spike_limit");
    assert_eq!(
        (
            deferred[0].1["limit"].clone(),
            deferred[0].1["running"].clone()
        ),
        (json!(1), json!(1))
    );

    // While the first Spike holds its slot, the second is shown deferred
    // for the limit.
    let view = crate::common::cli::ok(&db, &["candidates"]);
    assert_eq!(ids(&view["candidates"]), Vec::<i64>::new(), "{view}");
    assert_eq!(view["deferred"][0]["task_id"], second.as_i64(), "{view}");
    assert_eq!(view["deferred"][0]["reason"], "spike_limit", "{view}");
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        status["supervisors"][0]["claim_classes"],
        json!({"max_spikes": 1, "max_spikes_source": "dagq.toml",
               "class_aging": 3600, "class_aging_source": "default"}),
        "{status}"
    );

    release(&db, first);
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(claimed_tasks(&db).len(), 4);
    assert_eq!(claimed_tasks(&db)[3], second.as_i64());
    let ended = events(&db, "claim_deferral_ended");
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(
        (
            ended[0].0,
            ended[0].1["reason"].clone(),
            ended[0].1["why"].clone()
        ),
        (Some(second), json!("spike_limit"), json!("cleared"))
    );
    // Every run went on to await its landing: none was stopped for the
    // limit.
    let mut queue = SqliteQueue::open(&db).unwrap();
    for task in [TaskId::new(1), first, second, after] {
        let runs = queue.show(task).unwrap().runs;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status(), RunStatus::AwaitingIntegration);
    }

    let stats = crate::common::cli::ok(&db, &["stats", "--full"]);
    assert_eq!(stats["claim_waits"]["spike"]["count"], 2, "{stats}");
    assert_eq!(
        stats["claim_waits"]["implementation"]["count"], 2,
        "{stats}"
    );
}

/// Without `max_spikes` nothing limits the Spikes: three of them take the
/// three slots in one pass.
#[test]
fn without_the_limit_spikes_may_take_every_slot() {
    let (_dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    for index in 0..3 {
        add_task(
            &db,
            &format!("spike {index}"),
            ExecutionClass::Spike,
            Priority::Normal,
        );
    }
    let backend = TestWorkspace::new(&db, false, &agent());
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(3, true)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let claimed = events(&db, "run_claimed");
    let slots: Vec<_> = claimed.iter().map(|(_, p)| p["slots"].clone()).collect();
    assert_eq!(slots, [json!(0), json!(1), json!(2)], "{claimed:?}");
    assert!(
        claimed
            .iter()
            .all(|(_, payload)| payload["execution_class"] == "spike"),
        "{claimed:?}"
    );
    assert!(events(&db, "claim_deferred").is_empty());
}

/// `max_spikes = 1` keeps no slot for Spikes: with no Spike among the
/// candidates, three implementations take the three slots in one pass.
#[test]
fn with_the_limit_and_no_spike_implementations_take_every_slot() {
    let (_dir, repo, db) = fixture();
    fs::write(repo.join("dagq.toml"), "[supervisor]\nmax_spikes = 1\n").unwrap();
    for index in 0..2 {
        add_task(
            &db,
            &format!("implementation {index}"),
            ExecutionClass::Implementation,
            Priority::Normal,
        );
    }
    let backend = TestWorkspace::new(&db, false, &agent());
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(3, true)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let claimed = events(&db, "run_claimed");
    let slots: Vec<_> = claimed.iter().map(|(_, p)| p["slots"].clone()).collect();
    assert_eq!(slots, [json!(0), json!(1), json!(2)], "{claimed:?}");
    assert!(
        claimed
            .iter()
            .all(|(_, payload)| payload["execution_class"] == "implementation"),
        "{claimed:?}"
    );
    assert!(events(&db, "claim_deferred").is_empty());
}

/// Start a `--once` supervisor with one slot and gated sessions that stops
/// claiming once `stop` is set; its thread and its backend.
fn one_slot(
    db: &Path,
    repo: &Path,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> (
    thread::JoinHandle<anyhow::Result<Value>>,
    Arc<TestWorkspace>,
) {
    slots(db, repo, stop, 1)
}

/// [`one_slot`] with `parallel` slots.
fn slots(
    db: &Path,
    repo: &Path,
    stop: &Arc<std::sync::atomic::AtomicBool>,
    parallel: usize,
) -> (
    thread::JoinHandle<anyhow::Result<Value>>,
    Arc<TestWorkspace>,
) {
    let backend = Arc::new(TestWorkspace::new(db, false, &gated_agent()));
    let options = SuperviseOptions {
        stop: stop.clone(),
        ..supervise_options(parallel, true)
    };
    let supervisor = {
        let (db, repo, backend) = (db.to_owned(), repo.to_owned(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    (supervisor, backend)
}

/// One pass, six slots, `max_spikes = 2`, `class_aging = 1`: the
/// interrupts go first in the rule's order, an overdue interrupt Spike not
/// aged ahead of the interrupt implementation; at `high`, an implementation
/// whose priority a waiting `high` task lends it, and which unblocks that
/// task, comes by the rule before a `high` implementation of a lower ID,
/// yet the overdue `high` Spike goes before both, within the limit; at
/// `normal`, the overdue Spike is held by the limit the two Spikes reached
/// and the implementation takes the slot. Each claim records its own
/// effective priority and whether it aged.
#[test]
fn aging_keeps_to_each_priority_with_interrupts_inheritance_unblocks_and_the_limit() {
    let (_dir, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        "[supervisor]\nmax_spikes = 2\nclass_aging = 1\n",
    )
    .unwrap();
    cancel(&db, TaskId::new(1));
    let implementation =
        |title: &str, priority| add_draft(&db, title, ExecutionClass::Implementation, priority);
    let interrupt = implementation("interrupt implementation", Priority::Interrupt);
    let plain_high = implementation("plain high", Priority::High);
    let lent = implementation("lent high", Priority::Normal);
    let waiter = implementation("high waiter", Priority::High);
    let normal = implementation("normal implementation", Priority::Normal);
    SqliteQueue::open(&db)
        .unwrap()
        .add_dependency(waiter, lent)
        .unwrap();
    let spike = |title: &str, priority| add_task(&db, title, ExecutionClass::Spike, priority);
    let interrupt_spike = spike("interrupt spike", Priority::Interrupt);
    let high_spike = spike("high spike", Priority::High);
    let normal_spike = spike("normal spike", Priority::Normal);
    thread::sleep(Duration::from_millis(1100));
    for task in [interrupt, plain_high, lent, waiter, normal] {
        ready(&db, task);
    }
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (supervisor, backend) = slots(&db, &repo, &stop, 6);
    wait_until(&db, Duration::from_secs(60), |_| {
        claimed_tasks(&db).len() == 6
    });
    stop.store(true, Ordering::SeqCst);
    let claimed = events(&db, "run_claimed");
    let first: Vec<(i64, Value, Value, Value)> = claimed
        .iter()
        .map(|(task, payload)| {
            (
                task.unwrap().as_i64(),
                payload["slots"].clone(),
                payload["effective_priority"].clone(),
                payload["aged"].clone(),
            )
        })
        .collect();
    assert_eq!(
        first,
        [
            (
                interrupt.as_i64(),
                json!(0),
                json!("interrupt"),
                json!(false)
            ),
            (
                interrupt_spike.as_i64(),
                json!(1),
                json!("interrupt"),
                json!(false)
            ),
            (high_spike.as_i64(), json!(2), json!("high"), json!(true)),
            (lent.as_i64(), json!(3), json!("high"), json!(false)),
            (plain_high.as_i64(), json!(4), json!("high"), json!(false)),
            (normal.as_i64(), json!(5), json!("normal"), json!(false)),
        ],
        "{claimed:?}"
    );
    assert_eq!(claimed[3].1["priority"], "normal", "lent, not its own");
    let deferred = events(&db, "claim_deferred");
    assert_eq!(deferred[0].0, Some(normal_spike), "{deferred:?}");
    assert_eq!(deferred[0].1["reason"], "spike_limit");
    for task in [
        interrupt,
        interrupt_spike,
        high_spike,
        lent,
        plain_high,
        normal,
    ] {
        release(&db, task);
    }
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}

/// Two slots, `max_spikes = 1` and `class_aging = 1`, every task
/// `normal`: implementations made ready since come first in the rule's
/// order, yet the first Spike past the threshold is claimed, within the
/// limit, and the second is held by it while the slot it would take goes
/// to an implementation. `candidates` shows the held Spike as
/// `spike_limit` and the implementation left.
#[test]
fn a_spike_past_the_threshold_is_claimed_within_the_limit() {
    let (_dir, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        "[supervisor]\nmax_spikes = 1\nclass_aging = 1\n",
    )
    .unwrap();
    cancel(&db, TaskId::new(1));
    let implementations: Vec<TaskId> = (0..2)
        .map(|index| {
            add_draft(
                &db,
                &format!("implementation {index}"),
                ExecutionClass::Implementation,
                Priority::Normal,
            )
        })
        .collect();
    let spikes: Vec<TaskId> = (0..2)
        .map(|index| {
            add_task(
                &db,
                &format!("spike {index}"),
                ExecutionClass::Spike,
                Priority::Normal,
            )
        })
        .collect();
    thread::sleep(Duration::from_millis(1100));
    for task in &implementations {
        ready(&db, *task);
    }
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (supervisor, backend) = slots(&db, &repo, &stop, 2);
    wait_until(&db, Duration::from_secs(60), |_| {
        claimed_tasks(&db).len() == 2
    });
    stop.store(true, Ordering::SeqCst);
    assert_eq!(
        claimed_tasks(&db),
        [spikes[0].as_i64(), implementations[0].as_i64()]
    );
    let claimed = events(&db, "run_claimed");
    assert_eq!(claimed[0].1["aged"], true, "{claimed:?}");
    let deferred = events(&db, "claim_deferred");
    assert_eq!(deferred.len(), 1, "{deferred:?}");
    assert_eq!(deferred[0].0, Some(spikes[1]));
    assert_eq!(deferred[0].1["reason"], "spike_limit");
    let view = crate::common::cli::ok(&db, &["candidates"]);
    assert_eq!(
        ids(&view["candidates"]),
        [implementations[1].as_i64()],
        "{view}"
    );
    assert_eq!(view["deferred"][0]["task_id"], spikes[1].as_i64(), "{view}");
    assert_eq!(view["deferred"][0]["reason"], "spike_limit", "{view}");
    release(&db, spikes[0]);
    release(&db, implementations[0]);
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}

/// One slot, no `max_spikes` and `class_aging = 1`, every task `normal`:
/// two Spikes that waited past the second go ahead of an implementation
/// made ready since, which comes first in the rule's order, one claim at a
/// time and in their own order, each claim recording that it aged. While
/// the first runs, `candidates` and `graph` show the second first with the
/// mark, and the next claim takes it; the supervisor stops after it.
#[test]
fn the_aging_puts_a_spike_ahead_as_candidates_show_and_the_claim_records() {
    let (_dir, repo, db) = fixture();
    fs::write(repo.join("dagq.toml"), "[supervisor]\nclass_aging = 1\n").unwrap();
    cancel(&db, TaskId::new(1));
    let fresh = add_draft(
        &db,
        "implementation",
        ExecutionClass::Implementation,
        Priority::Normal,
    );
    let first = add_task(&db, "spike one", ExecutionClass::Spike, Priority::Normal);
    let second = add_task(&db, "spike two", ExecutionClass::Spike, Priority::Normal);
    thread::sleep(Duration::from_millis(1100));
    ready(&db, fresh);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (supervisor, backend) = one_slot(&db, &repo, &stop);
    wait_until(&db, Duration::from_secs(60), |_| {
        !claimed_tasks(&db).is_empty()
    });
    assert_eq!(claimed_tasks(&db), [first.as_i64()]);

    let view = crate::common::cli::ok(&db, &["candidates"]);
    assert_eq!(
        ids(&view["candidates"]),
        [second.as_i64(), fresh.as_i64()],
        "{view}"
    );
    assert_eq!(view["aged"], json!([second.as_i64()]), "{view}");
    let graph = crate::common::cli::ok(&db, &["graph"]);
    assert_eq!(
        ids(&graph["candidates"]),
        [second.as_i64(), fresh.as_i64()],
        "{graph}"
    );
    assert_eq!(graph["aged"], view["aged"], "{graph}");
    // The rule's order shows no aging.
    let rule = crate::common::cli::ok(&db, &["candidates", "--ignore-deferrals"]);
    assert_eq!(ids(&rule["candidates"]), [fresh.as_i64(), second.as_i64()]);
    assert!(rule.get("aged").is_none(), "{rule}");

    release(&db, first);
    wait_until(&db, Duration::from_secs(60), |_| {
        claimed_tasks(&db).len() == 2
    });
    stop.store(true, Ordering::SeqCst);
    release(&db, second);
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(claimed_tasks(&db), [first.as_i64(), second.as_i64()]);
    let aged: Vec<_> = events(&db, "run_claimed")
        .iter()
        .map(|(_, payload)| payload["aged"].clone())
        .collect();
    assert_eq!(aged, [json!(true), json!(true)]);
}

/// Spikes keep arriving ahead of an implementation in the rule's order,
/// within one priority: once the implementation waited past the threshold
/// and longer than any of them, its class goes first and it is claimed
/// (no `max_spikes`), the claim recording that it aged.
#[test]
fn spikes_that_keep_arriving_do_not_starve_an_implementation() {
    let (_dir, repo, db) = fixture();
    fs::write(repo.join("dagq.toml"), "[supervisor]\nclass_aging = 1\n").unwrap();
    cancel(&db, TaskId::new(1));
    let spikes: Vec<TaskId> = (0..2)
        .map(|index| {
            add_draft(
                &db,
                &format!("spike {index}"),
                ExecutionClass::Spike,
                Priority::Normal,
            )
        })
        .collect();
    let waiting = add_task(
        &db,
        "implementation",
        ExecutionClass::Implementation,
        Priority::Normal,
    );
    thread::sleep(Duration::from_millis(1100));
    for spike in &spikes {
        ready(&db, *spike);
    }
    thread::sleep(Duration::from_millis(1100));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (supervisor, backend) = one_slot(&db, &repo, &stop);
    wait_until(&db, Duration::from_secs(60), |_| {
        !claimed_tasks(&db).is_empty()
    });
    stop.store(true, Ordering::SeqCst);
    assert_eq!(claimed_tasks(&db), [waiting.as_i64()]);
    release(&db, waiting);
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(claimed_tasks(&db), [waiting.as_i64()]);
    let claimed = events(&db, "run_claimed");
    assert_eq!(claimed[0].1["aged"], true, "{claimed:?}");
    assert_eq!(claimed[0].1["execution_class"], "implementation");
}

/// The claim never takes a task of a class the limit holds: not a Spike
/// that became ready after the supervisor made its order (so it is in no
/// list the supervisor read), and not when none of the order is claimable
/// any more (claimed elsewhere, canceled) and the claim falls back to the
/// claim order, where that Spike comes first.
#[test]
fn the_claim_never_takes_a_held_class_not_one_read_after_the_order() {
    let (_dir, _repo, db) = fixture();
    let gone = add_task(
        &db,
        "implementation",
        ExecutionClass::Implementation,
        Priority::Normal,
    );
    cancel(&db, TaskId::new(1));
    // The supervisor's order named `gone` only; then it was canceled and a
    // Spike became ready before the claim's transaction.
    cancel(&db, gone);
    let spike = add_task(&db, "spike", ExecutionClass::Spike, Priority::High);
    let base = CommitSha::parse("0123456789abcdef0123456789abcdef01234567", "base").unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let mut claim = |held: &[ExecutionClass]| {
        queue
            .claim_for_supervisor_holding(
                &base,
                &LeaseToken::new("t"),
                &[gone],
                &dagq::domain::claim_order::ClaimHolds {
                    classes: held.to_vec(),
                    ..Default::default()
                },
                None,
                &Default::default(),
                &WorkerRoute::direct(&Worker::ALL),
            )
            .unwrap()
    };
    assert!(matches!(
        claim(&[ExecutionClass::Spike]),
        ClaimOutcome::NoReadyTask
    ));
    // Not held, the fallback takes it.
    let ClaimOutcome::Claimed { run } = claim(&[]) else {
        panic!("the Spike is claimed when not held")
    };
    assert_eq!(run.task_id(), spike);
}

/// The aging's clock read from the queue: a ready task of a draft goal
/// that waits for a task becomes claimable when the goal opens, after the
/// task it waited for completed; removing that met dependency then keeps
/// the time. The SQLite read gathers the task's own events, the status
/// changes of the task it depends on and its goal's; each case of the
/// judgement is a unit test of `domain::claim_facts::claimable_since`.
#[test]
fn the_aging_s_clock_is_read_from_the_task_s_its_dependencies_and_its_goal_s_events() {
    use dagq::application::DraftPlannerStore;
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let goal = queue
        .add_goal(dagq::domain::NewGoal {
            draft: true,
            ..crate::common::queue::new_goal("opens later")
        })
        .unwrap()
        .id();
    let predecessor = add_task(
        &db,
        "predecessor",
        ExecutionClass::Implementation,
        Priority::Normal,
    );
    let task = queue
        .add(NewTask {
            goal_id: Some(goal),
            dependencies: vec![predecessor],
            ..crate::common::queue::new_task("in a draft goal")
        })
        .unwrap()
        .id();
    queue.transition(task, TaskAction::BypassReview).unwrap();
    let ready_at = |queue: &SqliteQueue| queue.ready_at(&[task]).unwrap().get(&task).copied();
    assert_eq!(ready_at(&queue), None, "not a candidate yet");
    thread::sleep(Duration::from_millis(30));
    queue
        .record_task_event(
            predecessor,
            dagq::domain::EventKind::TaskStatusChanged,
            json!({"from": "in_progress", "to": "completed"}),
        )
        .unwrap();
    assert_eq!(ready_at(&queue), None, "its goal is a draft");
    thread::sleep(Duration::from_millis(30));
    queue.ready_goal(goal).unwrap();
    let opened = queue
        .all_events()
        .unwrap()
        .into_iter()
        .rfind(|event| event.kind == EventKind::GoalStatusChanged)
        .and_then(|event| dagq::domain::stats::timestamp_millis(&event.created_at))
        .unwrap();
    assert_eq!(ready_at(&queue), Some(opened));
    thread::sleep(Duration::from_millis(30));
    queue.remove_dependency(task, predecessor).unwrap();
    assert_eq!(
        ready_at(&queue),
        Some(opened),
        "the met dependency's removal keeps it"
    );
}

fn high_load() -> Option<f64> {
    Some(40.0)
}

/// The load hold holds a Spike's claim like any other, past the aging
/// threshold too: a Spike does not pass it.
#[test]
fn the_load_hold_holds_a_spike_too() {
    let (_dir, repo, db) = fixture();
    fs::write(repo.join("dagq.toml"), "[supervisor]\nclass_aging = 1\n").unwrap();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let spike = add_task(&db, "spike", ExecutionClass::Spike, Priority::Normal);
    thread::sleep(Duration::from_millis(1100));
    let backend = TestWorkspace::new(&db, false, &agent());
    let options = SuperviseOptions {
        max_load: Some(16.0),
        load_average: high_load,
        ..supervise_options(3, true)
    };
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    backend.join();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert!(claimed_tasks(&db).is_empty());
    assert_eq!(events(&db, "claim_held")[0].1["reason"], "load_average");
    assert_eq!(
        SqliteQueue::open(&db)
            .unwrap()
            .show(spike)
            .unwrap()
            .task
            .status(),
        TaskStatus::Ready
    );
}
