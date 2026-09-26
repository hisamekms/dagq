//! Runtime tests: the daily KPI report the supervisor writes (ADR-0051
//! decision 20).
use crate::runtime_support;

use runtime_support::*;

/// What a page may not contain if it loads nothing from anywhere else.
pub const EXTERNAL: &[&str] = &[
    "<script",
    "<link",
    "<img",
    "<iframe",
    "<object",
    "<embed",
    "@import",
    "url(",
    "src=",
    "http://",
    "https://",
    "@font-face",
];

fn report_events(db: &Path) -> Vec<Value> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT payload FROM run_events WHERE kind='report_written' ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|p| serde_json::from_str(&p.unwrap()).unwrap())
        .collect()
}

#[test]
fn supervisor_writes_the_reports_it_owes_once_and_prunes_old_ones() {
    let (_dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let reports = db.canonicalize().unwrap().parent().unwrap().join("reports");
    fs::create_dir_all(reports.join("daily")).unwrap();
    // Past the 90 days kept, and a file that is no report.
    fs::write(reports.join("daily/2000-01-01.html"), "old").unwrap();
    fs::write(reports.join("daily/notes.txt"), "mine").unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = SuperviseOptions {
        report_daily: true,
        ..supervise_options(1, true)
    };
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(outcome["runs"], json!([]));

    // The 7 days before today and the last ISO week, oldest first.
    let written = report_events(&db);
    assert_eq!(written.len(), 8, "{written:?}");
    assert!(written[..7].iter().all(|w| w["period"] == "day"));
    assert_eq!(written[7]["period"], "week");
    assert!(written[7]["label"].as_str().unwrap().contains("-W"));
    let token = written[0]["supervisor"].as_str().unwrap();
    assert!(!token.is_empty());
    for event in &written {
        let label = event["label"].as_str().unwrap();
        let dir = if event["period"] == "day" {
            "daily"
        } else {
            "weekly"
        };
        let html = fs::read_to_string(reports.join(format!("{dir}/{label}.html"))).unwrap();
        assert_eq!(
            event["html"],
            json!(reports.join(format!("{dir}/{label}.html")))
        );
        for external in EXTERNAL {
            assert!(!html.contains(external), "{external} in {label}");
        }
        assert!(html.contains(&format!("KPI report · {label}")));
        let json: Value =
            serde_json::from_slice(&fs::read(reports.join(format!("{dir}/{label}.json"))).unwrap())
                .unwrap();
        assert_eq!(json["report"]["label"], label);
        assert_eq!(json["report"]["partial"], false);
        assert_eq!(json["report"]["build"], dagq::VERSION);
        assert_eq!(json["periods"].as_array().unwrap().len(), 7);
    }
    let index = fs::read_to_string(reports.join("index.html")).unwrap();
    let newest = written[6]["label"].as_str().unwrap();
    let oldest = written[0]["label"].as_str().unwrap();
    assert!(
        index.find(&format!("daily/{newest}.html")).unwrap()
            < index.find(&format!("daily/{oldest}.html")).unwrap()
    );
    for external in EXTERNAL {
        assert!(!index.contains(external), "{external} in the index");
    }
    // The retention removed the old report and left the other file.
    assert!(!reports.join("daily/2000-01-01.html").exists());
    assert!(reports.join("daily/notes.txt").exists());
    // No temporary file is left.
    assert!(fs::read_dir(reports.join("daily")).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")
    }));

    // Written reports are not written again, by this or another supervisor.
    fs::remove_file(reports.join(format!("daily/{newest}.html"))).unwrap();
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(report_events(&db).len(), 8);
    assert!(!reports.join(format!("daily/{newest}.html")).exists());
    // Without the setting, no report is written.
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_events WHERE kind='report_written'", [])
        .unwrap();
    supervise_with(&db, &repo, &backend, &supervise_options(1, true)).unwrap();
    assert!(report_events(&db).is_empty());
}

/// Of two writers of the same report, one records it.
#[test]
fn a_report_is_recorded_once() {
    let (_dir, _repo, db) = fixture();
    let queue = SqliteQueue::open(&db).unwrap();
    let payload = json!({"period": "day", "label": "2026-09-26", "supervisor": "a"});
    assert!(queue.record_report_written(payload.clone()).unwrap());
    assert!(!queue.record_report_written(payload).unwrap());
    assert!(
        queue
            .record_report_written(json!({"period": "week", "label": "2026-09-26"}))
            .unwrap()
    );
    assert!(
        queue
            .record_report_written(json!({"period": "day"}))
            .is_err()
    );
    let written = queue.reports_written().unwrap();
    assert_eq!(written.len(), 2);
    assert!(written.contains(&("day".to_owned(), "2026-09-26".to_owned())));
}

/// A retention shorter than the backfill writes only the days it keeps, and
/// a temporary file an ended writer left is removed once it is old.
#[test]
fn the_backfill_stops_at_the_retention_and_old_temporary_files_go() {
    let (_dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let queue_dir = db.canonicalize().unwrap().parent().unwrap().to_path_buf();
    fs::write(
        queue_dir.join("host.toml"),
        "[report]\nkeep_daily_days = 2\n",
    )
    .unwrap();
    let daily = queue_dir.join("reports/daily");
    fs::create_dir_all(&daily).unwrap();
    let (old, fresh) = (daily.join(".x.html.1.tmp"), daily.join(".y.html.2.tmp"));
    fs::write(&old, "cut").unwrap();
    fs::write(&fresh, "writing").unwrap();
    fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(7200))
        .unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = SuperviseOptions {
        report_daily: true,
        ..supervise_options(1, true)
    };
    supervise_with(&db, &repo, &backend, &options).unwrap();
    let written = report_events(&db);
    let periods: Vec<&str> = written
        .iter()
        .map(|w| w["period"].as_str().unwrap())
        .collect();
    assert_eq!(periods, ["day", "day", "week"], "{written:?}");
    assert!(!old.exists());
    assert!(fresh.exists());
}
