//! Runtime tests: the worker, its `needs_session` resume and the headless
//! jobs are given the queue service's socket and a token instead of the
//! queue's path (goal 82's stage (3), ADR-t1233-4 decision 4): neither the
//! environment nor the command line of their processes names the queue
//! database, and their `dagq` reads and writes through the service as their
//! own principal.
use crate::runtime_support;
use dagq::domain::EventKind;

use runtime_support::*;

/// Shell lines that write the process's environment and command line to
/// `<dir>/<name>.env` and `<dir>/<name>.args`.
fn dump(dir: &str, name: &str) -> String {
    format!(r#"env > "{dir}/{name}.env"; ps -o args= -p $$ > "{dir}/{name}.args"; "#)
}

/// The names of the queue database a process could be given: as the test
/// named it and as the runtime resolved it.
fn queue_paths(db: &Path) -> Vec<String> {
    vec![
        db.to_str().unwrap().to_owned(),
        db.canonicalize().unwrap().to_str().unwrap().to_owned(),
    ]
}

/// What `<dir>/<name>` dumped names no queue database, and its environment
/// names the service and the token's file instead. The test double's own
/// `DB` (the tests' look at the queue, not the runtime's) is left out.
fn names_no_queue(db: &Path, dir: &Path, name: &str) {
    let env = fs::read_to_string(dir.join(format!("{name}.env")))
        .unwrap_or_else(|error| panic!("{name}.env: {error}"));
    let args = fs::read_to_string(dir.join(format!("{name}.args"))).unwrap();
    for path in queue_paths(db) {
        for line in env.lines().filter(|line| !line.starts_with("DB=")) {
            assert!(!line.contains(&path), "{name}: {line}");
        }
        assert!(!args.contains(&path), "{name}: {args}");
    }
    assert!(
        !env.lines().any(|line| line.starts_with("DAGQ_QUEUE=")),
        "{name}: {env}"
    );
    // The socket of this queue, as its directory is named or resolved.
    let sockets: Vec<String> = [
        db.parent().unwrap().to_owned(),
        db.canonicalize().unwrap().parent().unwrap().to_owned(),
    ]
    .iter()
    .map(|dir| {
        format!(
            "DAGQ_SERVICE_SOCKET={}",
            dagq::infrastructure::queue_service::socket_path(dir).display()
        )
    })
    .collect();
    assert!(
        env.lines()
            .any(|line| sockets.iter().any(|socket| line == socket)),
        "{name}: {sockets:?} in {env}"
    );
    assert!(
        env.lines()
            .any(|line| line.starts_with("DAGQ_SERVICE_CREDENTIAL_FILE=")),
        "{name}: {env}"
    );
}

/// The worker and the session the supervisor reopens for its run's
/// `needs_session` are given no queue path; each `show`s and `note`s
/// through the service as the run's worker.
#[test]
fn the_worker_and_its_resume_reach_the_queue_only_through_the_service() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let worker = format!(
        r#"{dump}"$DAGQ" show 2 > "$(dirname "$RECEIPT")/worker-show.json" || exit 71
"$DAGQ" note --task 2 --text 'noted by the worker' > /dev/null || exit 72
{VALID_AGENT}"#,
        dump = dump("$(dirname \"$RECEIPT\")", "worker"),
    );
    backend.script_for(2, &worker);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        &format!(
            r#"{dump}"$DAGQ" show 2 > "$(dirname "$RECEIPT")/resume-show.json" || exit 73
"$DAGQ" note --run "$RUN_ID" --text 'noted by the resume' > /dev/null || exit 74
await_message; resolve; receipt "$(git rev-parse HEAD)"; idle; await_exit"#,
            dump = dump("$(dirname \"$RECEIPT\")", "resume"),
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    let run_dir = Path::new(run.run_dir().unwrap());
    for name in ["worker", "resume"] {
        names_no_queue(&db, run_dir, name);
        let shown: Value = serde_json::from_str(
            &fs::read_to_string(run_dir.join(format!("{name}-show.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(shown["task"]["id"], 2, "{name}: {shown}");
    }
    // Both notes are the run's worker's: the service wrote them as the
    // principal of the token the claim and the resume issued.
    let notes: Vec<_> = detail
        .events
        .iter()
        .filter(|event| event.kind == EventKind::Observation)
        .collect();
    assert_eq!(notes.len(), 2, "{notes:?}");
    for note in notes {
        let actor = note.actor.clone().unwrap();
        assert_eq!(
            (actor.role.as_str(), actor.id),
            ("worker", format!("worker:{}", run.id()))
        );
        assert_eq!(note.payload["by"], "worker");
    }
    // The runs ended (both tasks landed), and their workers' tokens left
    // the disk: the one the resume issued again with the run's landing, the
    // first task's (landed by `integrate`, not by the supervisor) with the
    // supervisor's sweep.
    assert_no_tokens(db.parent().unwrap());
}

/// The review and the recovery job of a run are given no queue path, and
/// each job's token ends with it.
#[test]
fn the_review_and_the_recovery_job_are_given_the_service_not_the_queue() {
    let (_dir, repo, db) = fixture();
    let dir = db.parent().unwrap().to_owned();
    let mark = dir.join("failed-once");
    let worker = format!(
        "if [ ! -f {mark} ]; then : > {mark}; exit 7; fi; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
        mark = shell_path(&mark)
    );
    let backend = TestWorkspace::new(&db, false, &worker);
    let dir_text = dir.to_str().unwrap().replace('\'', "'\\''");
    let reviewer = TestReviewer::new(&[format!(
        "{}{}",
        dump(&dir_text, "review"),
        verdict("pass", &[], "meets the acceptance")
    )])
    .with_triages(&[format!(
        "{}{}",
        dump(&dir_text, "recovery"),
        repair(json!({"action": "retry"}), "the session died on its own")
    )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    for name in ["review", "recovery"] {
        names_no_queue(&db, &dir, name);
    }
    // Each job's token was revoked when it ended, and each run's worker's
    // with its run: the first failed, its retry landed.
    assert_no_tokens(&dir);
}

/// The files of the tokens the control side issued on the queue in
/// `queue_dir`: their values and their records.
fn token_files(queue_dir: &Path) -> Vec<PathBuf> {
    let service = dagq::infrastructure::queue_service::service_dir(queue_dir);
    ["credentials", "tokens"]
        .iter()
        .filter_map(|dir| fs::read_dir(service.join(dir)).ok())
        .flatten()
        .map(|entry| entry.unwrap().path())
        .collect()
}

/// No token's value or record is left on the queue in `queue_dir`.
fn assert_no_tokens(queue_dir: &Path) {
    let left = token_files(queue_dir);
    assert!(left.is_empty(), "{left:?}");
}

/// Whether the worker of `run` holds a token: its value's file.
fn worker_holds_token(db: &Path, run: &TaskRun) -> bool {
    use dagq::application::queue_service::ServiceAccess;
    let actor = dagq::domain::ActorContext::worker(run.id(), run.task_id());
    dagq::infrastructure::queue_service::SystemServiceAccess
        .credential(db, actor.actor_id())
        .is_some()
}

/// The supervisor's sweep revokes the tokens the ends of their runs left
/// (a run `integrate` landed, a canceled task's run, a run the queue does
/// not have) and keeps the token of a run that waits for its resume
/// (`needs_session`).
#[test]
fn the_sweep_revokes_the_tokens_of_ended_runs_and_keeps_a_waiting_runs() {
    let (_dir, repo, db) = fixture();
    let queue_dir = db.parent().unwrap().to_owned();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (parked, _) = parked_conflict(&repo, &db, &backend);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let landed = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(landed.status(), RunStatus::Integrated);
    // `integrate` landed the first task with no supervisor to revoke its
    // worker's token; one is left of a run no longer in the queue.
    assert!(worker_holds_token(&db, &landed));
    assert!(worker_holds_token(&db, &parked));
    let gone = dagq::domain::queue_service::Principal::worker(
        &RunId::new("gone").unwrap(),
        TaskId::new(9),
    );
    dagq::infrastructure::queue_service::issue(&queue_dir, &gone, 0).unwrap();
    let sweeping = SuperviseOptions {
        sweep_interval: Duration::ZERO,
        ..supervise_options(1, true)
    };
    // A person holds the parked run's task, which keeps it waiting; the
    // supervisor's sweep takes the ended runs' tokens and keeps its.
    let held =
        crate::common::cli::invoke_with(&[], &db, &["hold", "2", "--reason", "a look first"]);
    assert!(
        held.status.success(),
        "{}",
        String::from_utf8_lossy(&held.stderr)
    );
    let outcome = supervise_with(&db, &repo, &backend, &sweeping).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(
        queue.run(parked.id()).unwrap().status(),
        RunStatus::NeedsSession
    );
    assert!(worker_holds_token(&db, &parked));
    assert!(!worker_holds_token(&db, &landed));
    assert_eq!(
        token_files(&queue_dir).len(),
        2,
        "the parked run's value and record"
    );
    // The run ends while no supervisor looks (as `recover` ends one whose
    // process is gone) and the person cancels its task; the supervisor
    // that comes next sweeps the run's token.
    let released = crate::common::cli::invoke_with(&[], &db, &["release", "2"]);
    assert!(
        released.status.success(),
        "{}",
        String::from_utf8_lossy(&released.stderr)
    );
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='interrupted' WHERE id=?1",
            [parked.id().as_str()],
        )
        .unwrap();
    queue
        .transition(TaskId::new(2), TaskAction::Cancel)
        .unwrap();
    let ended = queue.run(parked.id()).unwrap().status();
    assert!(
        !dagq::domain::queue_service::run_holds_token(ended),
        "{ended:?}"
    );
    let outcome = supervise_with(&db, &repo, &backend, &sweeping).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_no_tokens(&queue_dir);
}
