//! Runtime tests: the supervisor's `candidates_sampled` (ADR-0051
//! decision 3), recorded when the claimable ready tasks, the free slots or
//! the ready tasks change, and `kpi` reading it.
use crate::runtime_support;

use runtime_support::*;
use std::sync::atomic::AtomicBool;

/// Above `--max-load`: every claim is held, so the samples depend on the
/// tasks alone.
fn high_load() -> Option<f64> {
    Some(40.0)
}

fn samples(db: &Path) -> Vec<Value> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "candidates_sampled")
        .map(|event| event.payload)
        .collect()
}

fn counts(sample: &Value) -> (i64, i64, i64) {
    (
        sample["candidates"].as_i64().unwrap(),
        sample["free_slots"].as_i64().unwrap(),
        sample["ready"].as_i64().unwrap(),
    )
}

/// A one-shot supervisor records the sample of its only pass; the next
/// supervisor records one on its first pass though nothing changed, none
/// on the passes after, and one more when a ready task is added. A task
/// waiting for another is ready but no candidate. `kpi` then reads the
/// samples: the candidates have a value and are no longer unavailable.
#[test]
fn the_supervisor_samples_the_candidates_when_they_change() {
    let (dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let once = SuperviseOptions {
        max_load: Some(16.0),
        load_average: high_load,
        ..supervise_options(2, true)
    };
    let outcome = supervise_with(&db, &repo, &backend, &once).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    let first = samples(&db);
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(counts(&first[0]), (1, 2, 1));
    assert!(first[0]["supervisor"].is_string());

    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        max_load: Some(16.0),
        load_average: high_load,
        stop: stop.clone(),
        ..supervise_options(2, false)
    };
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |_| samples(&db).len() == 2);
    // Several more passes with nothing changed record nothing.
    thread::sleep(TEST_TICK * 6);
    let restarted = samples(&db);
    assert_eq!(restarted.len(), 2, "{restarted:?}");
    assert_eq!(counts(&restarted[1]), (1, 2, 1));
    assert_ne!(restarted[0]["supervisor"], restarted[1]["supervisor"]);

    // A task that waits for the first is ready, not a candidate.
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "follower", &[TaskId::new(1)]);
    }
    wait_until(&db, Duration::from_secs(30), |_| samples(&db).len() == 3);
    thread::sleep(TEST_TICK * 4);
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to stop").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let all = samples(&db);
    assert_eq!(all.len(), 3, "{all:?}");
    assert_eq!(counts(&all[2]), (1, 2, 2));

    // `kpi` over the samples the supervisor wrote.
    let config = dir.path().join("config");
    fs::create_dir_all(&config).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .env("TZ", "UTC")
        .env("XDG_CONFIG_HOME", &config)
        .arg("--db")
        .arg(&db)
        .args(["kpi", "--last", "1"])
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let today = &report["periods"][0];
    let candidates = &today["kpis"]["candidates"]["all"];
    assert_eq!(candidates["value"], 1.0, "{today}");
    assert_eq!(candidates["max"], 1.0, "{today}");
    assert_eq!(today["details"]["candidates"]["starved_secs"], 0, "{today}");
    assert!(today["unavailable"].get("candidates").is_none(), "{today}");
}
