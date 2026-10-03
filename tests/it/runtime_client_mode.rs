//! Runtime tests: the worker, its `needs_session` resume and the headless
//! jobs are given the queue service's socket and a token instead of the
//! queue's path (goal 82's stage (3), ADR-t1233-4 decision 4): neither the
//! environment nor the command line of their processes names the queue
//! database, and their `dagq` reads and writes through the service as their
//! own principal.
use crate::runtime_support;

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
    headless_workers();
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
        .filter(|event| event.kind == "observation")
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
    // The runs ended (both tasks landed), and so did their workers' tokens.
    let credentials =
        dagq::infrastructure::queue_service::service_dir(db.parent().unwrap()).join("credentials");
    let tokens: Vec<PathBuf> = fs::read_dir(&credentials)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(tokens.len(), 2, "{tokens:?}");
    for token in tokens {
        let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
            .without_actor_env()
            .env(
                "DAGQ_SERVICE_SOCKET",
                dagq::infrastructure::queue_service::socket_path(db.parent().unwrap()),
            )
            .env("DAGQ_SERVICE_CREDENTIAL_FILE", &token)
            .args(["show", "2"])
            .bounded_output()
            .unwrap();
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["queue_service"]["code"], "unauthenticated", "{error}");
    }
}

/// The review and the recovery job of a run are given no queue path, and
/// each job's token ends with it.
#[test]
fn the_review_and_the_recovery_job_are_given_the_service_not_the_queue() {
    headless_workers();
    let (_dir, repo, db) = fixture();
    let dir = db.parent().unwrap().to_owned();
    let mark = dir.join("failed-once");
    let worker = format!(
        "if [ ! -f '{mark}' ]; then : > '{mark}'; exit 7; fi; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
        mark = mark.display()
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
    // Each job's token was revoked when it ended; only the runs' remain.
    let credentials = dagq::infrastructure::queue_service::service_dir(&dir).join("credentials");
    assert_eq!(
        fs::read_dir(&credentials).unwrap().count(),
        2,
        "the two runs' workers' tokens only"
    );
}
