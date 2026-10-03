//! Runtime tests: the host's load the supervisor records continuously
//! under the queue's `host/` (task 516).
use crate::runtime_support;

use dagq::{
    compose::HostMetricsSettings,
    domain::host_metrics::{DiskSpace, HostSample},
};
use runtime_support::*;
use std::sync::atomic::AtomicBool;

/// A sample of a host with a fixed load and memory.
fn steady_host(now: i64) -> HostSample {
    let mut sample = HostSample::new(now).with_loads([Some(1.5), Some(2.0), None]);
    sample.set("mem_used_mb", Some(8192.0));
    sample
}

fn broken_host(_now: i64) -> HostSample {
    panic!("the host's tools could not be read");
}

fn settings(interval: Duration, sample: fn(i64) -> HostSample) -> Option<HostMetricsSettings> {
    Some(HostMetricsSettings {
        sample,
        // 10 GiB free of 100.
        disk: |_| {
            Some(DiskSpace {
                free_bytes: 10 << 30,
                total_bytes: 100 << 30,
            })
        },
        ..HostMetricsSettings::new(interval, 30)
    })
}

fn host_dir(db: &Path) -> PathBuf {
    db.canonicalize().unwrap().parent().unwrap().join("host")
}

/// The rows of every metrics file under `host`, without the headers.
fn rows(host: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(host) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for entry in entries {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with("metrics-") {
            let text = fs::read_to_string(&path).unwrap();
            let mut lines = text.lines();
            assert_eq!(
                lines.next(),
                Some(dagq::domain::host_metrics::header().as_str())
            );
            rows.extend(lines.map(str::to_owned));
        }
    }
    rows
}

/// While it runs, the supervisor appends a row every interval to the local
/// day's file, with the columns the header names; the files past the
/// retention go and any other file stays.
#[test]
fn supervisor_records_the_host_load_while_it_runs_and_prunes_old_files() {
    headless_workers();
    let (dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let host = host_dir(&db);
    fs::create_dir_all(&host).unwrap();
    fs::write(
        host.join("metrics-20000101.csv"),
        format!("{}\n", dagq::domain::host_metrics::header()),
    )
    .unwrap();
    fs::write(host.join("notes.txt"), "mine").unwrap();
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        host_metrics: settings(Duration::from_millis(100), steady_host),
        ..supervise_options(1, false)
    };
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |_| rows(&host).len() >= 3);
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to stop").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let rows = rows(&host);
    let columns = dagq::domain::host_metrics::COLUMNS.len();
    let mut last = 0;
    for row in &rows {
        let cells: Vec<&str> = row.split(',').collect();
        assert_eq!(cells.len(), columns, "{row}");
        let unix: i64 = cells[1].parse().unwrap();
        assert!(unix >= last, "{rows:?}");
        last = unix;
        assert_eq!(&cells[2..5], ["1.50", "2", ""], "{row}");
    }
    assert!(!host.join("metrics-20000101.csv").exists());
    assert!(host.join("notes.txt").exists());
    // `stats` summarizes them.
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert!(stats["host"]["samples"].as_u64().unwrap() >= 3, "{stats}");
    assert_eq!(stats["host"]["metrics"]["mem_used_mb"]["max"], 8192.0);
    // The free space of the runs' filesystem, its least and median too
    // (task 1371).
    let disk = &stats["host"]["metrics"]["disk_free_bytes"];
    assert_eq!(
        (&disk["min"], &disk["median"]),
        (&json!(10737418240.0), &json!(10737418240.0)),
        "{stats}"
    );
    assert_eq!(stats["host"]["metrics"]["disk_free_pct"]["min"], 10.0);
    let config = dir.path().join("config");
    fs::create_dir_all(&config).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
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
    let kpi: Value = serde_json::from_slice(&output.stdout).unwrap();
    let today = &kpi["periods"][0];
    assert_eq!(
        today["health"]["disk"]["min_free_bytes"], 10737418240.0,
        "{today}"
    );
    assert_eq!(today["health"]["disk"]["median_free_pct"], 10.0, "{today}");
    // Without the setting, nothing is recorded.
    fs::remove_dir_all(&host).unwrap();
    supervise_with(&db, &repo, &backend, &supervise_options(1, true)).unwrap();
    assert!(!host.exists());
}

/// A sample that panics, or a directory that cannot be written, stops no
/// claim nor landing: the run lands as without the recording.
#[test]
fn a_failed_sample_stops_nothing() {
    headless_workers();
    let (_dir, repo, db) = fixture();
    let host = host_dir(&db);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = SuperviseOptions {
        host_metrics: settings(Duration::from_millis(50), broken_host),
        ..supervise_options(1, true)
    };
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(outcome["runs"][0]["task_id"], 1, "{outcome}");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(rows(&host).is_empty());

    // `host` is a file: nothing can be written under it.
    fs::write(&host, "in the way").unwrap();
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second", &[]);
    let options = SuperviseOptions {
        host_metrics: settings(Duration::from_millis(50), steady_host),
        ..supervise_options(1, true)
    };
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(outcome["runs"][0]["task_id"], 2, "{outcome}");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(fs::read_to_string(&host).unwrap(), "in the way");
}
