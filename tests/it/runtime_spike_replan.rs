//! Runtime tests: a Spike's landing leaves its replanning request
//! (ADR-t1487-1 decision 5) and the investigations its claims open are
//! limited (decision 7). The supervisor's wiring of the cases: the landing
//! records one request per Spike in its own transaction, whatever the
//! verdict; a supervisor started again, a second one, and the passes that
//! reconcile record no second and catch one a landing left out. Which
//! investigation a limit holds, with the waits for a person and the Spikes
//! an investigation waits for, is judged by the unit tests of
//! `domain::investigation`.
use crate::runtime_support;

use dagq::domain::{ExecutionClass, GoalId, NewGoal, Priority};
use runtime_support::*;

/// Add a ready task of `class` in `goal`; its ID.
fn add_task(db: &Path, title: &str, class: ExecutionClass, goal: Option<GoalId>) -> TaskId {
    let mut queue = SqliteQueue::open(db).unwrap();
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "measure it, at most one hour".into(),
            acceptance: "a verdict with grounds".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Some(Priority::Normal),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: goal,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
            execution_class: class,
        })
        .unwrap()
        .id();
    queue.transition(task, TaskAction::BypassReview).unwrap();
    task
}

fn open_goal(db: &Path, title: &str) -> GoalId {
    SqliteQueue::open(db)
        .unwrap()
        .add_goal(NewGoal {
            title: title.into(),
            ..Default::default()
        })
        .unwrap()
        .id()
}

/// A worker that commits a file of its own run and reports its Spike with
/// `verdict`: runs of it in parallel land one after another.
fn spike_agent(verdict: &str) -> String {
    format!(
        "{RECEIPT_SPIKE}printf x > \"notes-$RUN_ID.txt\" && git add \"notes-$RUN_ID.txt\" && git commit -q -m spike; receipt_spike \"$(git rev-parse HEAD)\" {verdict}"
    )
}

/// Each Spike's requests: `(spike_task_id, status)` by request ID.
fn spike_requests(db: &Path) -> Vec<(i64, String)> {
    let connection = rusqlite::Connection::open(db).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT spike_task_id, status FROM plan_requests WHERE spike_task_id IS NOT NULL
             ORDER BY id",
        )
        .unwrap();
    statement
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn events(db: &Path, kind: &str) -> Vec<(Option<TaskId>, Value)> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| (event.task_id, event.payload))
        .collect()
}

/// Three Spikes, one per verdict, land: each landing records its request
/// in its own transaction (its `request_recorded` is the event right after
/// the task's completion), by the runtime, with the Spike and its goal. Two
/// supervisors' reconciling passes record no second; a request a landing
/// left out (an older binary's) is recorded by the next pass, once.
#[test]
fn a_spike_s_landing_records_one_request_whatever_the_verdict_and_passes_add_none() {
    let (_dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let goal = open_goal(&db, "does the premise hold");
    let holds = add_task(&db, "spike holds", ExecutionClass::Spike, Some(goal));
    let not = add_task(&db, "spike does not", ExecutionClass::Spike, Some(goal));
    let open = add_task(&db, "spike unresolved", ExecutionClass::Spike, None);
    let backend = TestWorkspace::new(&db, false, &spike_agent("holds"));
    backend.script_for(not.as_i64(), &spike_agent("does_not_hold"));
    backend.script_for(open.as_i64(), &spike_agent("unresolved"));
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(3, true)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(spike_requests(&db).is_empty());
    for spike in [holds, not, open] {
        assert_eq!(
            integrate(&db, spike.as_i64(), &repo).unwrap()["outcome"],
            "integrated"
        );
    }
    let requests = spike_requests(&db);
    assert_eq!(
        requests,
        [
            (holds.as_i64(), "open".to_owned()),
            (not.as_i64(), "open".to_owned()),
            (open.as_i64(), "open".to_owned())
        ],
    );
    let all = SqliteQueue::open(&db).unwrap().all_events().unwrap();
    for spike in [holds, not, open] {
        let recorded = all
            .iter()
            .position(|event| {
                event.kind == "request_recorded" && event.payload["spike_task_id"] == spike.as_i64()
            })
            .unwrap_or_else(|| panic!("no request of {spike}"));
        assert_eq!(all[recorded].payload["requested_by"], "runtime");
        let completed = all
            .iter()
            .position(|event| {
                event.task_id == Some(spike)
                    && event.kind == "task_status_changed"
                    && event.payload["to"] == "completed"
            })
            .unwrap();
        assert_eq!(completed + 1, recorded, "{spike}");
    }
    let shown = crate::common::cli::ok(&db, &["requests", "--all"]);
    let of_holds = shown["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|request| request["spike_task_id"] == holds.as_i64())
        .unwrap()
        .clone();
    assert_eq!(
        of_holds["refs"],
        json!([{"kind": "task", "id": holds.as_i64()}, {"kind": "goal", "id": goal.as_i64()}]),
        "{of_holds}"
    );

    // Two supervisors' passes reconcile: no second request.
    let (mut one, mut two) = (
        SqliteQueue::open(&db).unwrap(),
        SqliteQueue::open(&db).unwrap(),
    );
    for _ in 0..2 {
        one.investigations().unwrap();
        two.investigations().unwrap();
    }
    assert_eq!(spike_requests(&db).len(), 3);

    // A landing that left its request out (an older binary's): the next
    // pass records it, once.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM plan_requests WHERE spike_task_id=?1",
            [open.as_i64()],
        )
        .unwrap();
    one.investigations().unwrap();
    two.investigations().unwrap();
    let requests = spike_requests(&db);
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert_eq!(
        requests
            .iter()
            .filter(|(task, _)| *task == open.as_i64())
            .count(),
        1
    );
    assert_eq!(events(&db, "request_recorded").len(), 4);
}

fn claimed_tasks(db: &Path) -> Vec<TaskId> {
    events(db, "run_claimed")
        .into_iter()
        .map(|(task, _)| task.unwrap())
        .collect()
}

/// `kind` events of `task`: their reasons.
fn reasons_of(db: &Path, task: TaskId, kind: &str) -> Vec<String> {
    events(db, kind)
        .into_iter()
        .filter(|(of, _)| *of == Some(task))
        .map(|(_, payload)| payload["reason"].as_str().unwrap().to_owned())
        .collect()
}

fn ids(view: &Value) -> Vec<i64> {
    view.as_array()
        .unwrap()
        .iter()
        .map(|task| task.get("id").unwrap_or(task).as_i64().unwrap())
        .collect()
}

/// Decline the Spike `spike`'s request, as the planner `planner` open
/// for it (none when none is).
fn decline(db: &Path, spike: TaskId) {
    let mut queue = SqliteQueue::open(db).unwrap();
    let request = queue
        .plan_requests(false)
        .unwrap()
        .into_iter()
        .find(|request| request.spike_task == Some(spike))
        .unwrap()
        .id;
    let planner = queue.request_planner(request).unwrap();
    queue
        .decline_request(request, "nothing to plan on", "planner", planner)
        .unwrap();
    if let Some(planner) = planner {
        queue.register_planner_wrapper(planner, 1).unwrap();
        queue.register_planner_agent(planner, 1, 1).unwrap();
        queue.planner_exited(planner, 1, 0).unwrap();
    }
}

/// `max_spikes = 1` and `max_investigations = 1`, three slots: goal A's
/// Spikes `a1` and `a2`, goal B's Spike `b1` and two implementations. The
/// Spike limit holds both other Spikes first, with its reason; once `a1`
/// leaves its slot, `a2` (of goal A, open) is claimed and `b1` stays held,
/// now for the investigation limit, which `candidates` and `graph` show;
/// the implementations take the other slots. A submit of another goal's
/// Spike is not refused. Goal A stays open while a Spike of it is
/// unfinished and while its request is `open`, beside the planner its
/// request opens and an implementation claimed meanwhile; once both
/// requests are declined, the next pass claims `b1` and ends its deferral.
#[test]
fn the_investigation_limit_holds_a_new_goal_s_spike_until_an_investigation_ends() {
    let (_dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    fs::write(
        repo.join("dagq.toml"),
        "[supervisor]\nmax_spikes = 1\nmax_investigations = 1\n",
    )
    .unwrap();
    let (a, b) = (open_goal(&db, "goal A"), open_goal(&db, "goal B"));
    let a1 = add_task(&db, "a1", ExecutionClass::Spike, Some(a));
    let a2 = add_task(&db, "a2", ExecutionClass::Spike, Some(a));
    let b1 = add_task(&db, "b1", ExecutionClass::Spike, Some(b));
    let i1 = add_task(&db, "i1", ExecutionClass::Implementation, None);
    let i2 = add_task(&db, "i2", ExecutionClass::Implementation, None);
    let backend = TestWorkspace::new(&db, false, &spike_agent("holds"));
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(3, true)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let claimed = claimed_tasks(&db);
    assert_eq!(claimed[..3], [a1, i1, i2], "{claimed:?}");
    assert_eq!(claimed[3..], [a2], "{claimed:?}");
    let opened: Vec<bool> = events(&db, "run_claimed")
        .iter()
        .map(|(_, payload)| payload["investigation_opened"] == true)
        .collect();
    assert_eq!(opened, [true, false, false, false]);
    assert_eq!(
        reasons_of(&db, a2, "claim_deferred"),
        ["spike_limit"],
        "a2 waited for the Spike limit only"
    );
    // While a Spike holds the slot, the Spike limit's reason; then the
    // investigation limit's, which each change of reason ends and starts.
    let deferred = reasons_of(&db, b1, "claim_deferred");
    assert_eq!(deferred.first().map(String::as_str), Some("spike_limit"));
    assert_eq!(
        deferred.last().map(String::as_str),
        Some("investigation_limit")
    );
    assert!(
        deferred.windows(2).all(|pair| pair[0] != pair[1]
            && pair
                .iter()
                .all(|r| r == "spike_limit" || r == "investigation_limit")),
        "{deferred:?}"
    );
    assert_eq!(
        reasons_of(&db, b1, "claim_deferral_ended"),
        deferred[..deferred.len() - 1]
    );
    let held = events(&db, "claim_deferred")
        .into_iter()
        .rfind(|(task, _)| *task == Some(b1))
        .unwrap()
        .1;
    assert_eq!(
        (held["limit"].clone(), held["running"].clone()),
        (json!(1), json!(1))
    );
    let view = crate::common::cli::ok(&db, &["candidates"]);
    assert_eq!(ids(&view["candidates"]), Vec::<i64>::new(), "{view}");
    assert_eq!(view["deferred"][0]["task_id"], b1.as_i64(), "{view}");
    assert_eq!(
        view["deferred"][0]["reason"], "investigation_limit",
        "{view}"
    );
    let graph = crate::common::cli::ok(&db, &["graph"]);
    assert_eq!(graph["candidates"], view["candidates"], "{graph}");
    assert_eq!(graph["deferred"], view["deferred"], "{graph}");

    // At the limit a submit is not refused.
    let c = open_goal(&db, "goal C");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let c1 = queue
        .add(NewTask {
            title: "c1".into(),
            description: "measure".into(),
            acceptance: "a verdict".into(),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: None,
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: Some(c),
            context: String::new(),
            provider: None,
            worker_mode: None,
            wait_for_build: false,
            execution_class: ExecutionClass::Spike,
        })
        .unwrap()
        .id();
    crate::common::cli::ok(&db, &["submit", &c1.to_string()]);
    queue.transition(c1, TaskAction::Cancel).unwrap();

    // Goal A's Spikes land; their requests are open, so it stays open: the
    // planner a request opens and an implementation claimed meanwhile
    // leave `b1` held.
    for spike in [a1, a2] {
        assert_eq!(
            integrate(&db, spike.as_i64(), &repo).unwrap()["outcome"],
            "integrated"
        );
    }
    decline(&db, a2);
    let i3 = add_task(&db, "i3", ExecutionClass::Implementation, None);
    let backend = TestWorkspace::new(&db, false, &spike_agent("holds"));
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(3, true)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(claimed_tasks(&db)[4..], [i3]);
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners.len(), 1, "{planners:?}");
    assert!(planners[0].request_id.is_some());

    // Its last request declined, goal A's investigation ends: `b1` goes.
    decline(&db, a1);
    let backend = TestWorkspace::new(&db, false, &spike_agent("holds"));
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(3, true)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(claimed_tasks(&db)[5..], [b1]);
    assert_eq!(reasons_of(&db, b1, "claim_deferral_ended"), deferred);
}

/// Claim `task` as a supervisor would, in a claim of its own.
fn claim(db: &Path, task: TaskId) -> dagq::domain::TaskRun {
    let base = CommitSha::parse("0123456789abcdef0123456789abcdef01234567", "base").unwrap();
    match SqliteQueue::open(db)
        .unwrap()
        .claim_for_supervisor_in_order(
            &base,
            &dagq::domain::LeaseToken::new("t"),
            &[task],
            None,
            &Default::default(),
            &dagq::domain::provider_switch::WorkerRoute::direct(&dagq::domain::worker::Worker::ALL),
        )
        .unwrap()
    {
        dagq::domain::ClaimOutcome::Claimed { run } => *run,
        other => panic!("{task} not claimed: {other:?}"),
    }
}

fn open_keys(db: &Path) -> Vec<dagq::domain::investigation::InvestigationKey> {
    SqliteQueue::open(db)
        .unwrap()
        .investigations()
        .unwrap()
        .open
        .into_iter()
        .map(|investigation| investigation.key)
        .collect()
}

/// A Spike's claim opens its goal's investigation, and a goalless Spike's
/// its own; abandoning the goal (its request still `open`), or canceling
/// the last unfinished Spike (no request: a canceled Spike gets none),
/// ends it. The claim's own
/// transaction holds a Spike past the limit, whatever the order named; an
/// investigation whose Spike only waits for a person's answer past the
/// grace stops counting and lets it go.
#[test]
fn abandoning_the_goal_or_canceling_its_spikes_ends_an_investigation() {
    use dagq::domain::investigation::InvestigationKey;
    let (_dir, _repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let (a, b) = (open_goal(&db, "goal A"), open_goal(&db, "goal B"));
    let a1 = add_task(&db, "a1", ExecutionClass::Spike, Some(a));
    let b1 = add_task(&db, "b1", ExecutionClass::Spike, Some(b));
    let loose = add_task(&db, "goalless", ExecutionClass::Spike, None);
    claim(&db, a1);
    let run = claim(&db, loose);
    assert_eq!(
        open_keys(&db),
        [InvestigationKey::Goal(a), InvestigationKey::Spike(loose)]
    );

    // The claim's transaction holds `b1` at the limit, named or not.
    let base = CommitSha::parse("0123456789abcdef0123456789abcdef01234567", "base").unwrap();
    let held = dagq::domain::claim_order::ClaimHolds {
        classes: Vec::new(),
        investigations: dagq::domain::investigation::InvestigationLimit {
            max: Some(2),
            grace_secs: 600,
        },
    };
    let mut queue = SqliteQueue::open(&db).unwrap();
    let outcome = queue
        .claim_for_supervisor_holding(
            &base,
            &dagq::domain::LeaseToken::new("t"),
            &[b1],
            &held,
            None,
            &Default::default(),
            &dagq::domain::provider_switch::WorkerRoute::direct(&dagq::domain::worker::Worker::ALL),
        )
        .unwrap();
    assert!(
        matches!(outcome, dagq::domain::ClaimOutcome::NoReadyTask),
        "{outcome:?}"
    );

    // The goalless Spike's run only waits for a person, past the grace: it
    // stops counting, and `b1` goes.
    Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO asks(kind, task_id, run_id, question, asked_by, reason_category,
               created_at)
             VALUES ('worker_question', ?1, ?2, 'which way?', 'worker', 'scope', unixepoch() - 3600)",
            rusqlite::params![loose.as_i64(), run.id().as_str()],
        )
        .unwrap();
    let counted = queue.investigations().unwrap();
    assert_eq!(counted.open.len(), 2);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert_eq!(counted.counted(now, 600), 1);
    let outcome = queue
        .claim_for_supervisor_holding(
            &base,
            &dagq::domain::LeaseToken::new("t"),
            &[b1],
            &held,
            None,
            &Default::default(),
            &dagq::domain::provider_switch::WorkerRoute::direct(&dagq::domain::worker::Worker::ALL),
        )
        .unwrap();
    assert!(
        matches!(&outcome, dagq::domain::ClaimOutcome::Claimed { run } if run.task_id() == b1),
        "{outcome:?}"
    );

    // `b1` completed (its request open) and goal B abandoned: its
    // investigation ends with the request still open.
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET status='completed' WHERE id=?1",
            [b1.as_i64()],
        )
        .unwrap();
    assert_eq!(
        open_keys(&db),
        [
            InvestigationKey::Goal(a),
            InvestigationKey::Spike(loose),
            InvestigationKey::Goal(b)
        ]
    );
    queue
        .close_goal(b, dagq::domain::GoalVerdict::Abandoned)
        .unwrap();
    assert_eq!(
        open_keys(&db),
        [InvestigationKey::Goal(a), InvestigationKey::Spike(loose)]
    );
    // Goal A's only Spike canceled after its run failed ends goal A's,
    // with no request.
    let a_run = queue.show(a1).unwrap().runs[0].id().clone();
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='failed' WHERE id=?1",
            [a_run.as_str()],
        )
        .unwrap();
    queue.transition(a1, TaskAction::Cancel).unwrap();
    assert_eq!(open_keys(&db), [InvestigationKey::Spike(loose)]);
    let requests = queue.plan_requests(true).unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.spike_task)
            .collect::<Vec<_>>(),
        [Some(b1)]
    );
}

/// `[goals] max_spike_cycles = 1`: a goal's investigation opened once, a
/// planner of the runtime's submit of a further Spike of it is refused and
/// says a person decides; a person's submit of it goes, and an
/// implementation of the goal goes either way.
#[test]
fn a_goal_at_its_spike_cycles_takes_a_further_spike_only_from_a_person() {
    use dagq::domain::{PlannerOrigin, PlannerOwner, Submission};
    let (_dir, _repo, db) = fixture();
    let a = open_goal(&db, "goal A");
    let first = add_task(&db, "first spike", ExecutionClass::Spike, Some(a));
    claim(&db, first);
    let mut queue = SqliteQueue::open(&db)
        .unwrap()
        .with_max_spike_cycles(Some(1));
    let draft = |queue: &mut SqliteQueue, class| {
        queue
            .add(NewTask {
                title: format!("next {class:?}"),
                description: "measure again".into(),
                acceptance: "a verdict".into(),
                verification_commands: vec!["true".into()],
                required_evidence: Vec::new(),
                paths: Vec::new(),
                priority: None,
                change: None,
                dependencies: Vec::new(),
                goal_dependencies: Vec::new(),
                goal_id: Some(a),
                context: String::new(),
                provider: None,
                worker_mode: None,
                wait_for_build: false,
                execution_class: class,
            })
            .unwrap()
            .id()
    };
    let submit = |queue: &mut SqliteQueue, task, origin| {
        queue.submit_linking(
            Submission {
                tasks: vec![task],
                goals: Vec::new(),
                proposal: None,
                owner: PlannerOwner {
                    origin,
                    workspace_id: None,
                },
            },
            &[],
        )
    };
    let further = draft(&mut queue, ExecutionClass::Spike);
    let refused = submit(&mut queue, further, PlannerOrigin::Runtime)
        .unwrap_err()
        .to_string();
    assert!(refused.contains("max_spike_cycles (1)"), "{refused}");
    assert!(refused.contains("planner_question"), "{refused}");
    let implementation = draft(&mut queue, ExecutionClass::Implementation);
    submit(&mut queue, implementation, PlannerOrigin::Runtime).unwrap();
    submit(&mut queue, further, PlannerOrigin::Person).unwrap();
}
