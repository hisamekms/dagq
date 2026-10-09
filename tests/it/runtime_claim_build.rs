//! Runtime tests: a task that waits for a build containing its
//! dependencies' landings (ADR-t1632-1) is not claimed while the
//! supervisor's own build lacks one, and is claimed by a supervisor whose
//! build has them; a task that declares nothing is claimed as before.
use crate::common::cli::{ok, queue};
use crate::runtime_support;

use dagq::domain::{ClaimOutcome, LeaseToken, provider_switch::WorkerRoute, worker::Worker};
use runtime_support::*;

fn add_waiting_task(queue: &mut SqliteQueue, title: &str, dependencies: &[TaskId]) -> TaskId {
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Some(Priority::Interrupt),
            change: None,
            dependencies: dependencies.to_vec(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: true,
            execution_class: Default::default(),
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    task.id()
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

fn runs_of(db: &Path, task: TaskId) -> usize {
    SqliteQueue::open(db)
        .unwrap()
        .show(task)
        .unwrap()
        .runs
        .len()
}

/// Supervise once as a supervisor built from `commit`, passing the review
/// of every run so it lands.
fn supervise_built_from(db: &Path, repo: &Path, commit: &str) -> Value {
    let backend = TestWorkspace::new(db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&vec![verdict("pass", &[], "meets the acceptance"); 4]);
    let options = SuperviseOptions {
        build: Some(format!("0.0.0-dev+{commit}")),
        ..supervise_options(3, true)
    };
    supervise_reviewed_with(db, repo, &backend, &reviewer, &options)
}

/// The fixture's task 1 lands under a supervisor built from main before
/// it: the task that waits for the build of that landing is passed over
/// (even at interrupt priority) and `status` and `show` say why, while the
/// task that declares nothing is claimed. A supervisor built from the
/// landing (the build an automatic update hands over to) claims it on its
/// first pass and ends the wait.
#[test]
fn a_task_waits_for_a_build_that_contains_its_dependencies_landings() {
    let (_dir, repo, db) = fixture();
    let before = git_out(&repo, &["rev-parse", "main"]);
    let (waiting, plain) = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        let waiting = add_waiting_task(&mut queue, "needs the build", &[TaskId::new(1)]);
        let plain = add_ready_task(&mut queue, "needs the landing only", &[TaskId::new(1)]);
        (waiting, plain)
    };

    let outcome = supervise_built_from(&db, &repo, &before);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let landed = events(&db, "run_integrated")
        .into_iter()
        .find(|(task, _)| *task == Some(TaskId::new(1)))
        .map(|(_, payload)| payload["result_commit"].as_str().unwrap().to_owned())
        .expect("task 1 landed");
    assert_eq!(runs_of(&db, plain), 1, "a task that declares nothing");
    assert_eq!(runs_of(&db, waiting), 0, "the build lacks the landing");
    let deferred = events(&db, "claim_deferred");
    assert_eq!(deferred.len(), 1, "{deferred:?}");
    let (task, payload) = &deferred[0];
    assert_eq!(*task, Some(waiting));
    assert_eq!(payload["reason"], "not_in_build");
    assert_eq!(payload["build"], format!("0.0.0-dev+{before}"));
    assert_eq!(
        payload["missing"],
        json!([{"task_id": 1, "commit": landed}])
    );
    let status = runtime::status(&db).unwrap();
    let open = status["claim_deferrals"].as_array().unwrap();
    assert_eq!(open.len(), 1, "{status}");
    assert_eq!(open[0]["task_id"], json!(waiting));
    assert_eq!(open[0]["reason"], "not_in_build");
    assert_eq!(open[0]["missing"][0]["commit"], json!(landed));
    let shown = SqliteQueue::open(&db).unwrap().show(waiting).unwrap();
    assert!(shown.task.wait_for_build());
    assert!(
        shown.events.iter().any(
            |event| event.kind == "claim_deferred" && event.payload["reason"] == "not_in_build"
        ),
        "show carries the wait"
    );
    let shown = ok(&db, &["show", &waiting.to_string()]);
    assert_eq!(shown["claim_deferral"]["reason"], "not_in_build", "{shown}");
    assert_eq!(shown["claim_deferral"]["missing"][0]["task_id"], 1);

    // A build of the landing claims it and ends the wait.
    let outcome = supervise_built_from(&db, &repo, &landed);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(runs_of(&db, waiting), 1, "the build contains the landing");
    // Recorded once over the passes it waited.
    assert_eq!(events(&db, "claim_deferred").len(), 1);
    let ended = events(&db, "claim_deferral_ended");
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0].0, Some(waiting));
    assert_eq!(ended[0].1["reason"], "not_in_build");
    assert_eq!(ended[0].1["why"], "cleared");
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["claim_deferrals"], json!([]), "{status}");
}

/// The claim falls back to the claim order when none of the tasks the
/// supervisor chose is ready any more (claimed elsewhere, canceled), and
/// that fallback never takes a task that waits for the build: only the
/// supervisor that judged its build may claim it.
#[test]
fn the_claims_fallback_never_takes_a_task_that_waits_for_the_build() {
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let waiting = add_waiting_task(&mut queue, "needs the build", &[]);
    let base = CommitSha::parse("0123456789abcdef0123456789abcdef01234567", "base").unwrap();
    let claim = |queue: &mut SqliteQueue, order: &[TaskId]| {
        queue
            .claim_for_supervisor_in_order(
                &base,
                &LeaseToken::new("t"),
                order,
                None,
                &Default::default(),
                &WorkerRoute::direct(&Worker::ALL),
            )
            .unwrap()
    };
    // An order whose task is gone: the fallback takes task 1, though the
    // waiting task comes first by its interrupt priority.
    let ClaimOutcome::Claimed { run } = claim(&mut queue, &[TaskId::new(99)]) else {
        panic!("the task that declares nothing is claimed")
    };
    assert_eq!(run.task_id(), TaskId::new(1));
    assert!(matches!(
        claim(&mut queue, &[TaskId::new(99)]),
        ClaimOutcome::NoReadyTask
    ));
    // Named in the order, as a supervisor whose build serves it names it.
    let ClaimOutcome::Claimed { run } = claim(&mut queue, &[waiting]) else {
        panic!("the waiting task is claimed when ordered")
    };
    assert_eq!(run.task_id(), waiting);
}

/// `add --wait-for-build` declares the wait and `show` prints it;
/// `edit --no-wait-for-build` withdraws it, recorded as a change of
/// `wait_for_build`, and a task that declares nothing shows no such field.
#[test]
fn the_wait_for_the_build_is_declared_shown_and_withdrawn() {
    let (_dir, db) = queue();
    let sound = ["--acceptance", "works", "--verify", "cargo test"];
    let added = ok(
        &db,
        &[&["add", "needs the build", "--wait-for-build"][..], &sound].concat(),
    );
    assert_eq!(added["wait_for_build"], true);
    let id = added["id"].to_string();
    assert_eq!(ok(&db, &["show", &id])["task"]["wait_for_build"], true);
    assert!(ok(&db, &["show", &id]).get("claim_deferral").is_none());
    let edited = ok(&db, &["edit", &id, "--no-wait-for-build"]);
    assert!(edited.get("wait_for_build").is_none(), "{edited}");
    let event = ok(&db, &["show", &id, "--full"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["kind"] == "task_edited")
        .cloned()
        .unwrap();
    assert_eq!(event["payload"]["from"], json!({"wait_for_build": true}));
    assert_eq!(event["payload"]["to"], json!({"wait_for_build": false}));
    let edited = ok(&db, &["edit", &id, "--wait-for-build"]);
    assert_eq!(edited["wait_for_build"], true);
    let plain = ok(&db, &[&["add", "needs nothing"][..], &sound].concat());
    assert!(plain.get("wait_for_build").is_none(), "{plain}");
}
