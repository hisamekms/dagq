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

/// A stub `name` in `bin` running `body`.
fn stub(bin: &Path, name: &str, body: &str) {
    let path = bin.join(name);
    crate::common::template::script(&path, format!("#!/bin/sh\n{body}\n"));
}

/// The reports carry the near-term dependency diagram d2 and TALA drew
/// (ADR-0077 decision 7), inline and with nothing that loads from outside
/// the page; the tools are found on the supervisor's PATH, and d2 runs once
/// for all the reports owed. Without TALA the section says why and the rest
/// of the report is written.
#[test]
fn the_reports_carry_the_dependency_diagram_or_why_not() {
    let (dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let draft = |queue: &mut SqliteQueue, title: &str, priority: &str, dependencies| {
        queue
            .add(NewTask {
                title: title.into(),
                description: "d".into(),
                acceptance: "a".into(),
                verification_commands: Vec::new(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
                priority: priority.parse().unwrap(),
                change: None,
                dependencies,
                goal_dependencies: Vec::new(),
                goal_id: None,
                context: String::new(),
                provider: None,
                worker_mode: None,
            })
            .unwrap()
            .id()
    };
    let groundwork = draft(&mut queue, "groundwork", "normal", Vec::new());
    let on_top = draft(&mut queue, "on top", "high", vec![groundwork]);
    draft(&mut queue, "someday", "normal", Vec::new());
    drop(queue);

    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let calls = dir.path().join("d2-calls");
    stub(&bin, "d2plugin-tala", "exit 0");
    // Echoes the source back inside an SVG that would load from outside.
    stub(
        &bin,
        "d2",
        "[ \"$1\" = --layout=tala ] || exit 9\necho call >> \"${0%/*}/../d2-calls\"\nprintf '<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\"><style>@font-face{src:url(\"data:font/woff;base64,AA==\")}</style><image href=\"https://example.com/i.png\"/><script>x()</script><text>'\nsed 's/</[/g'\nprintf '</text></svg>'",
    );
    let path =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = SuperviseOptions {
        report_daily: true,
        diagram_path: Some(path),
        ..supervise_options(1, true)
    };
    supervise_with(&db, &repo, &backend, &options).unwrap();
    let written = report_events(&db);
    assert_eq!(written.len(), 8, "{written:?}");
    assert_eq!(fs::read_to_string(&calls).unwrap(), "call\n");
    let reports = db.canonicalize().unwrap().parent().unwrap().join("reports");
    for event in [&written[0], &written[7]] {
        let html = fs::read_to_string(event["html"].as_str().unwrap()).unwrap();
        let start = html.find("<div class=\"scroll diagram\"><svg").unwrap();
        let end = start + html[start..].find("</svg></div>").unwrap();
        let svg = &html[start..end];
        assert!(
            svg.contains("on top") && svg.contains("groundwork"),
            "{svg}"
        );
        assert!(!svg.contains("someday"));
        for gone in ["<?xml", "xmlns", "https://", "<script"] {
            assert!(!svg.contains(gone), "{gone} in {svg}");
        }
        // The image keeps nothing to load; the font is carried along.
        assert!(svg.contains("<image/>"));
        assert!(svg.contains("url(\"data:font/woff;base64,AA==\")"));
        let outside = format!("{}{}", &html[..start], &html[end..]);
        for external in EXTERNAL {
            assert!(
                !outside.contains(external),
                "{external} outside the diagram"
            );
        }
        let json: Value =
            serde_json::from_slice(&fs::read(event["json"].as_str().unwrap()).unwrap()).unwrap();
        assert_eq!(
            json["diagram"],
            json!({"tasks": [groundwork.as_i64(), on_top.as_i64()], "d2_source": true})
        );
    }

    // Without TALA: the reports are written with the reason.
    fs::remove_file(bin.join("d2plugin-tala")).unwrap();
    fs::remove_dir_all(&reports).unwrap();
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_events WHERE kind='report_written'", [])
        .unwrap();
    supervise_with(&db, &repo, &backend, &options).unwrap();
    let written = report_events(&db);
    assert_eq!(written.len(), 8);
    assert_eq!(fs::read_to_string(&calls).unwrap(), "call\n");
    let html = fs::read_to_string(written[7]["html"].as_str().unwrap()).unwrap();
    assert!(
        html.contains(
            "Not drawn: cannot draw the dependency diagram: d2plugin-tala not found on PATH"
        ),
        "{html}"
    );
    assert!(html.contains("<h2>Open findings</h2>"));
    for external in EXTERNAL {
        assert!(!html.contains(external), "{external} in the page");
    }
    let json: Value =
        serde_json::from_slice(&fs::read(written[7]["json"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(json["diagram"]["d2_source"], true);
    assert!(
        json["diagram"]["reason"]
            .as_str()
            .unwrap()
            .contains("d2plugin-tala not found")
    );
}
