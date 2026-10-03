//! Runtime tests: while the load hold is on, a supervisor makes one new
//! claim per pass and spaces the next one from the queue's latest claim
//! (ADR-t1479-1); `[supervisor] claim_spacing` sets the seconds, read again
//! each pass, and `status` shows when the next claim may be made.
use crate::runtime_support;

use dagq::domain::stats::timestamp_millis;
use runtime_support::*;
use std::sync::atomic::{AtomicBool, AtomicU64};

/// The load average [`changing`] reads, as `f64` bits: only the test that
/// changes it reads it.
static LOAD: AtomicU64 = AtomicU64::new(0);

fn changing() -> Option<f64> {
    Some(f64::from_bits(LOAD.load(Ordering::SeqCst)))
}

fn set_load(value: f64) {
    LOAD.store(value.to_bits(), Ordering::SeqCst);
}

/// Below any `--max-load` the tests give.
fn low() -> Option<f64> {
    Some(1.0)
}

/// Write `dagq.toml` whole by a rename: the supervisor reads it every pass.
fn write_config(repo: &Path, text: &str) {
    let staged = repo.join("dagq.toml.new");
    fs::write(&staged, text).unwrap();
    fs::rename(&staged, repo.join("dagq.toml")).unwrap();
}

/// The queue's events of `kind`, oldest first: each one's time in Unix
/// milliseconds and its payload.
fn events(db: &Path, kind: &str) -> Vec<(i64, Value)> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| (timestamp_millis(&event.created_at).unwrap(), event.payload))
        .collect()
}

fn add_tasks(db: &Path, count: usize) {
    let mut queue = SqliteQueue::open(db).unwrap();
    for index in 0..count {
        add_ready_task(&mut queue, &format!("more {index}"), &[]);
    }
}

/// A `--once` supervisor with `parallel` slots, the load hold at 16 and
/// `claim_spacing` given; its outcome.
fn once(
    db: &Path,
    repo: &Path,
    load_average: fn() -> Option<f64>,
    claim_spacing: Option<usize>,
) -> Value {
    let backend = TestWorkspace::new(db, false, VALID_AGENT);
    let options = SuperviseOptions {
        max_load: Some(16.0),
        load_average,
        claim_spacing,
        ..supervise_options(2, true)
    };
    let outcome = supervise_with(db, repo, &backend, &options).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    outcome
}

/// With two free slots, two claimable tasks and the load below
/// `--max-load`, a pass claims one task; the claim records the spacing and
/// no hold is recorded for it. A supervisor started again within the
/// spacing claims nothing, the spacing counted from the queue's latest
/// claim. After the spacing the load is judged again before the next
/// claim: above `--max-load` it is held (`claim_held`), below it is made.
#[test]
fn one_claim_per_pass_spaced_from_the_queues_latest_claim() {
    let (_dir, repo, db) = fixture();
    add_tasks(&db, 1);
    set_load(1.0);

    let outcome = once(&db, &repo, changing, Some(600));
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 1, "{outcome}");
    let claimed = events(&db, "run_claimed");
    assert_eq!(claimed.len(), 1, "{claimed:?}");
    assert_eq!(claimed[0].1["claim_spacing"], 600, "{claimed:?}");
    assert_eq!(claimed[0].1["claim_spacing_wait_secs"], 0, "{claimed:?}");
    assert!(events(&db, "claim_held").is_empty());
    assert!(events(&db, "claim_resumed").is_empty());

    // Started again within the spacing: nothing is claimed, nothing held.
    let outcome = once(&db, &repo, changing, Some(600));
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert_eq!(events(&db, "run_claimed").len(), 1);
    assert!(events(&db, "claim_held").is_empty());
    assert_eq!(
        SqliteQueue::open(&db)
            .unwrap()
            .show(TaskId::new(2))
            .unwrap()
            .task
            .status(),
        TaskStatus::Ready
    );

    // After the spacing (1 s here), the load is judged again: above
    // `--max-load` the claim is held.
    thread::sleep(Duration::from_millis(1100));
    set_load(40.0);
    let outcome = once(&db, &repo, changing, Some(1));
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    let held = events(&db, "claim_held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].1["reason"], "load_average");
    assert_eq!(events(&db, "run_claimed").len(), 1);

    // Below it, the next task is claimed.
    set_load(1.0);
    let outcome = once(&db, &repo, changing, Some(1));
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 1, "{outcome}");
    let claimed = events(&db, "run_claimed");
    assert_eq!(claimed.len(), 2, "{claimed:?}");
    assert_eq!(claimed[1].1["claim_spacing"], 1, "{claimed:?}");
    assert!(claimed[1].0 - claimed[0].0 >= 1000, "{claimed:?}");
    assert_eq!(events(&db, "claim_resumed").len(), 1);
}

/// A resident supervisor takes `claim_spacing` from `[supervisor]`, claims
/// one of two tasks, and claims the other once the spacing after the first
/// claim has passed, recording how long it waited; `status` shows the
/// seconds, where they come from and when the next claim may be made.
/// Neither wait records a hold. A changed table takes effect without a
/// restart: 0 spaces no claim.
#[test]
fn a_resident_supervisor_waits_for_the_spacing_and_status_shows_it() {
    let (_dir, repo, db) = fixture();
    add_tasks(&db, 1);
    write_config(&repo, "[supervisor]\nclaim_spacing = 8\n");
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        max_load: Some(16.0),
        load_average: low,
        stop: stop.clone(),
        ..supervise_options(2, false)
    };
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(60), |_| {
        !events(&db, "run_claimed").is_empty()
    });
    let status = runtime::status(&db).unwrap();
    let spacing = &status["supervisors"][0]["claim_spacing"];
    assert_eq!(spacing["secs"], 8, "{status}");
    assert_eq!(spacing["source"], "dagq.toml", "{status}");
    assert_eq!(spacing["max_load"], 16.0, "{status}");
    assert_eq!(spacing["in_effect"], true, "{status}");
    let last = timestamp_millis(spacing["last_claim_at"].as_str().unwrap()).unwrap();
    let next = timestamp_millis(spacing["next_claim_at"].as_str().unwrap()).unwrap();
    assert_eq!(next - last, 8000, "{status}");
    if events(&db, "run_claimed").len() == 1 {
        assert_eq!(spacing["waiting"], true, "{status}");
    }

    wait_until(&db, Duration::from_secs(120), |_| {
        events(&db, "run_claimed").len() == 2
    });
    let claimed = events(&db, "run_claimed");
    assert!(claimed[1].0 - claimed[0].0 >= 8000, "{claimed:?}");
    assert_eq!(claimed[1].1["claim_spacing"], 8, "{claimed:?}");
    let waited = claimed[1].1["claim_spacing_wait_secs"].as_u64().unwrap();
    // From the pass that claimed the first task to the one that claimed
    // the second: about the spacing, more on a loaded host.
    assert!((1..=120).contains(&waited), "{claimed:?}");
    assert!(events(&db, "claim_held").is_empty());
    assert!(events(&db, "claim_resumed").is_empty());

    // Changed to 0: in use from the next pass, recorded, out of effect.
    write_config(&repo, "[supervisor]\nclaim_spacing = 0\n");
    wait_until(&db, Duration::from_secs(30), |_| {
        events(&db, "supervisor_config_changed").len() == 1
    });
    let changed = &events(&db, "supervisor_config_changed")[0].1;
    assert_eq!(changed["from"]["claim_spacing"], 8, "{changed}");
    assert_eq!(changed["to"]["claim_spacing"], 0, "{changed}");
    await_passes(&passes, SOME_PASSES);
    let status = runtime::status(&db).unwrap();
    let spacing = &status["supervisors"][0]["claim_spacing"];
    assert_eq!(spacing["secs"], 0, "{status}");
    assert_eq!(spacing["in_effect"], false, "{status}");
    assert_eq!(spacing["next_claim_at"], Value::Null, "{status}");
    assert_eq!(spacing["waiting"], false, "{status}");

    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}

/// Without the load hold (the library's default, as `--max-load 0`), or
/// with a spacing of 0, a pass fills every free slot as before, and the
/// claims record no spacing. Without the table the default spacing is in
/// effect.
#[test]
fn no_load_hold_or_no_spacing_fills_the_free_slots_in_one_pass() {
    let (_dir, repo, db) = fixture();
    add_tasks(&db, 2);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = SuperviseOptions {
        load_average: low,
        ..supervise_options(3, true)
    };
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    backend.join();
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 3, "{outcome}");
    let claimed = events(&db, "run_claimed");
    // Each claim made while the ones before it held their slots: one pass.
    let slots: Vec<_> = claimed.iter().map(|(_, p)| p["slots"].clone()).collect();
    assert_eq!(slots, [json!(0), json!(1), json!(2)], "{claimed:?}");
    assert!(
        claimed
            .iter()
            .all(|(_, payload)| payload.get("claim_spacing").is_none()),
        "{claimed:?}"
    );

    add_tasks(&db, 2);
    let outcome = once(&db, &repo, low, Some(0));
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2, "{outcome}");
    let claimed = events(&db, "run_claimed");
    assert_eq!(claimed.len(), 5, "{claimed:?}");
    assert_eq!(claimed[4].1["slots"], 1, "{claimed:?}");
    assert!(
        claimed
            .iter()
            .all(|(_, payload)| payload.get("claim_spacing").is_none()),
        "{claimed:?}"
    );

    // No table and no value given: the default spacing (180 s) is in
    // effect, so the claims just made space the next one.
    add_tasks(&db, 1);
    let outcome = once(&db, &repo, low, None);
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert_eq!(events(&db, "run_claimed").len(), 5);
    assert!(events(&db, "claim_held").is_empty());
}
