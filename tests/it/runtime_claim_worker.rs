//! Runtime tests: the worker a task asks for (ADR-t813-2) is written on its
//! run at the claim, a supervisor claims only the tasks whose worker it has
//! adapters for and records why it defers the others, and its providers'
//! executables are recorded on its registration.
use crate::runtime_support;

use dagq::domain::{
    ClaimOutcome, LeaseToken, Provider,
    worker::{Worker, WorkerMode},
};
use runtime_support::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};

fn add_task(
    queue: &mut SqliteQueue,
    title: &str,
    provider: Option<Provider>,
    mode: Option<WorkerMode>,
) -> TaskId {
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Priority::Normal,
            kind: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider,
            worker_mode: mode,
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

fn runs_of(db: &Path, task: TaskId) -> Vec<TaskRun> {
    SqliteQueue::open(db).unwrap().show(task).unwrap().runs
}

/// A claim writes the provider and mode of the task's worker on the run
/// (requested and actual alike) and on its `run_claimed`; a claim that
/// runs only some workers passes over the tasks of the others.
#[test]
fn a_claim_writes_the_worker_of_its_task_on_the_run() {
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let codex = add_task(&mut queue, "codex", Some(Provider::Codex), None);
    let headless = add_task(&mut queue, "headless", None, Some(WorkerMode::Headless));
    let base = CommitSha::parse("0123456789abcdef0123456789abcdef01234567", "base").unwrap();
    let claim = |queue: &mut SqliteQueue, order: &[TaskId], workers: &[Worker]| {
        queue
            .claim_for_supervisor_in_order(
                &base,
                &LeaseToken::new("t"),
                order,
                None,
                &Default::default(),
                workers,
            )
            .unwrap()
    };
    // Neither worker is run: the task of fixture() (interactive Claude)
    // is taken instead of the ones asked for first.
    let ClaimOutcome::Claimed { run } = claim(&mut queue, &[codex, headless], &[Worker::DEFAULT])
    else {
        panic!("the interactive task is claimed")
    };
    assert_eq!(run.task_id(), TaskId::new(1));
    assert_eq!(run.worker_mode(), WorkerMode::Interactive);
    assert!(matches!(
        claim(&mut queue, &[], &[Worker::DEFAULT]),
        ClaimOutcome::NoReadyTask
    ));
    let ClaimOutcome::Claimed { run } = claim(&mut queue, &[], &Worker::ALL[..2]) else {
        panic!("the headless Claude task is claimed")
    };
    assert_eq!(run.task_id(), headless);
    assert_eq!(run.requested_provider(), Provider::Claude);
    assert_eq!(run.actual_provider(), Provider::Claude);
    assert_eq!(run.worker_mode(), WorkerMode::Headless);
    let stored = queue.run(run.id()).unwrap();
    assert_eq!(stored.worker_mode(), WorkerMode::Headless);
    let claimed = events(&db, "run_claimed");
    let (_, payload) = claimed
        .iter()
        .find(|(task, _)| *task == Some(headless))
        .unwrap();
    assert_eq!(payload["provider"], "claude");
    assert_eq!(payload["worker_mode"], "headless");
    assert_eq!(runs_of(&db, codex).len(), 0);
}

/// A supervisor with the adapters of interactive Claude only (ADR-t813-2)
/// does not claim a Codex task (`provider_unavailable`) nor a headless
/// Claude one (`mode_unavailable`): each deferral is recorded once, with
/// the worker, and shown by `status`; the task of interactive Claude is
/// claimed beside them. A task that leaves the candidates ends its
/// deferral.
#[test]
fn a_task_of_a_worker_the_supervisor_cannot_run_is_deferred() {
    let (_dir, repo, db) = fixture();
    let (codex, headless) = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        (
            add_task(&mut queue, "codex", Some(Provider::Codex), None),
            add_task(&mut queue, "headless", None, Some(WorkerMode::Headless)),
        )
    };
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(3, true)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let claimed = runs_of(&db, TaskId::new(1));
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].worker_mode(), WorkerMode::Interactive);
    assert_eq!(claimed[0].requested_provider(), Provider::Claude);
    assert!(runs_of(&db, codex).is_empty());
    assert!(runs_of(&db, headless).is_empty());
    let deferred = events(&db, "claim_deferred");
    assert_eq!(deferred.len(), 2, "{deferred:?}");
    let of = |task: TaskId| {
        deferred
            .iter()
            .find(|(id, _)| *id == Some(task))
            .map(|(_, payload)| payload.clone())
            .unwrap()
    };
    let payload = of(codex);
    assert_eq!(payload["reason"], "provider_unavailable");
    assert_eq!(payload["provider"], "codex");
    assert_eq!(payload["worker_mode"], "headless");
    assert!(payload["message"].as_str().unwrap().contains("codex"));
    assert_eq!(of(headless)["reason"], "mode_unavailable");
    let status = runtime::status(&db).unwrap();
    let mut reasons: Vec<(Value, Value)> = status["claim_deferrals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|open| (open["task_id"].clone(), open["reason"].clone()))
        .collect();
    reasons.sort_by_key(|(task, _)| task.as_i64());
    assert_eq!(
        reasons,
        [
            (json!(codex), json!("provider_unavailable")),
            (json!(headless), json!("mode_unavailable"))
        ]
    );

    // Deferred still, not recorded again; the canceled one ends.
    SqliteQueue::open(&db)
        .unwrap()
        .transition(codex, TaskAction::Cancel)
        .unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(3, true)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(events(&db, "claim_deferred").len(), 2);
    assert!(runs_of(&db, headless).is_empty());
    let ended = events(&db, "claim_deferral_ended");
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0].0, Some(codex));
    assert_eq!(ended[0].1["reason"], "provider_unavailable");
    assert_eq!(ended[0].1["why"], "not_candidate");
}

/// The supervisor resolves `claude` and `codex` to their executables and
/// records them on its registration with whether each was found and the
/// modes it runs it in (ADR-t813-2); `status` and `doctor` show them.
#[test]
fn the_providers_are_recorded_on_the_registration() {
    let (dir, repo, db) = fixture();
    let codex = dir.path().join("codex-stub");
    fs::write(&codex, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        codex: codex.clone(),
        ..supervise_options(1, false)
    };
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(60), |queue| {
        queue
            .supervisors()
            .unwrap()
            .first()
            .is_some_and(|registration| registration.providers.is_some())
    });
    let status = runtime::status(&db).unwrap();
    let providers = status["supervisors"][0]["providers"].clone();
    let doctor = runtime::doctor(&db, false).unwrap();
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(providers[0]["provider"], "claude", "{providers}");
    assert_eq!(providers[0]["found"], true);
    assert_eq!(providers[0]["modes"], json!(["interactive"]));
    assert_eq!(providers[1]["provider"], "codex");
    assert_eq!(
        providers[1]["executable"],
        json!(codex.canonicalize().unwrap())
    );
    assert_eq!(providers[1]["found"], true);
    assert_eq!(providers[1]["modes"], json!([]));
    assert_eq!(doctor["supervisors"][0]["providers"], providers);
}

/// A `codex` that is not found does not stop the supervisor: it is
/// recorded as not found, with why.
#[test]
fn a_missing_codex_is_recorded_as_not_found() {
    let (dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let token = LeaseToken::new("t");
    queue
        .register_supervisor(&token, std::process::id(), 1, "v")
        .unwrap();
    let missing = dir.path().join("no-codex");
    let checks =
        dagq::compose::provider_checks(Path::new("/bin/sh"), &missing, &Default::default());
    assert_eq!(checks[1].provider, Provider::Codex);
    assert!(!checks[1].found);
    assert!(checks[1].error.is_some());
    assert_eq!(checks[1].executable, missing.display().to_string());
    assert!(!checks[0].usable(), "no adapters, no mode");
    queue.set_supervisor_providers(&token, &checks).unwrap();
    let registration = queue.supervisors().unwrap().remove(0);
    assert_eq!(registration.providers, Some(checks));
}
