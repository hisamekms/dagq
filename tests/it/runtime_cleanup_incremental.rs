//! Runtime tests: the cleanup's removal, on its usual sweep, of the
//! caches no build reads any more (rustc's older incremental sessions)
//! from the worktrees of the runs that build nothing now.
use crate::common;
use crate::runtime_support::*;
use std::sync::atomic::AtomicBool;

/// The payloads of a run's events of `kind`.
fn payloads_of(queue: &SqliteQueue, run: &TaskRun, kind: &str) -> Vec<Value> {
    queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .filter(|e| e.kind.as_str() == kind)
        .map(|e| e.payload)
        .collect()
}

/// Write in `crate_dir` the session `name` of rustc's incremental
/// compilation, with a file in it, and its lock (`s-<T>-<R>.lock`).
fn write_session(crate_dir: &Path, name: &str) {
    fs::create_dir_all(crate_dir.join(name)).unwrap();
    fs::write(
        crate_dir.join(name).join("query-cache.bin"),
        vec![0u8; 65536],
    )
    .unwrap();
    let lock = &name[..name.rfind('-').unwrap()];
    fs::write(crate_dir.join(format!("{lock}.lock")), "").unwrap();
}

/// The entries of `dir`, sorted.
fn entries_of(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A run that builds nothing (here one with no lease and no session, its
/// ask closed) loses, on the cleanup's usual sweep whatever the free space,
/// the finished incremental sessions older than the newest of each crate
/// with their locks, recorded as `incremental_sessions_removed`; the
/// newest, a working session and their locks, and the rest of its build
/// outputs stay. While its session runs nothing is touched; once a build
/// of it left an older session again and the session ended, that one goes
/// too. The runs a sweep takes (one waiting for the landing slot or for an
/// answer in its slot, none working) and how long one is left out once
/// pruned are the unit tests' of `supervise::cleanup`.
#[test]
fn a_run_that_builds_nothing_loses_its_older_incremental_sessions_after_each_build() {
    let (_dir, repo, db) = fixture();
    declare_rust_profile(&repo);
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let (run, ask) = run_awaiting_an_answer(&db, &repo, &backend, false);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.answer(ask.id, "withdrawn").unwrap();
    queue.close_ask(ask.id).unwrap();
    assert_eq!(
        queue
            .ended_run_worktree(run.id())
            .unwrap()
            .map(|w| w.cleanup),
        Some(dagq::application::WorktreeCleanup::Idle)
    );
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let crate_dir = worktree.join("target/debug/incremental/dagq-1a2b");
    for session in ["s-00a-r1-svh1", "s-00b-r2-svh2", "s-00c-r3-working"] {
        write_session(&crate_dir, session);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        sweep_interval: Duration::ZERO,
        ..supervise_options(4, false)
    };
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            supervise_reviewed_owned_with(&db, &repo, &backend, &TestReviewer::new(&[]), &options)
        })
    };
    let removed = |queue: &SqliteQueue| payloads_of(queue, &run, "incremental_sessions_removed");
    wait_until(&db, common::STEP_LIMIT, |queue| removed(queue).len() == 1);
    assert_eq!(
        entries_of(&crate_dir),
        [
            "s-00b-r2-svh2",
            "s-00b-r2.lock",
            "s-00c-r3-working",
            "s-00c-r3.lock"
        ]
    );
    assert!(worktree.join("target/debug/big").is_file());
    let first = removed(&queue)[0].clone();
    let bytes = first["bytes"].as_u64().unwrap();
    assert!(bytes >= 65536, "{first}");
    assert_eq!(
        first,
        json!({"paths": [crate_dir.to_string_lossy()], "entries": 2, "bytes": bytes, "by": "supervisor", "reason": "idle"})
    );
    assert!(payloads_of(&queue, &run, "build_outputs_removed").is_empty());

    // Its session runs and builds: nothing goes meanwhile.
    let raw = Connection::open(&db).unwrap();
    raw.execute(
        "INSERT OR REPLACE INTO run_processes(run_id,role,pid,heartbeat_at,exited_at) VALUES (?1,'agent',?2,unixepoch()+100000,NULL)",
        rusqlite::params![run.id(), std::process::id()],
    )
    .unwrap();
    write_session(&crate_dir, "s-00d-r4-svh4");
    await_passes(&passes, SOME_PASSES);
    assert_eq!(removed(&queue).len(), 1);
    assert!(crate_dir.join("s-00b-r2-svh2").is_dir());

    // Its session ended: the session before its build goes.
    raw.execute(
        "UPDATE run_processes SET exited_at=unixepoch() WHERE run_id=?1 AND role='agent'",
        [run.id()],
    )
    .unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| removed(queue).len() == 2);
    assert_eq!(
        entries_of(&crate_dir),
        [
            "s-00c-r3-working",
            "s-00c-r3.lock",
            "s-00d-r4-svh4",
            "s-00d-r4.lock"
        ]
    );
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to finish");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(removed(&queue).len(), 2);
}
