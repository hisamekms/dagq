//! Runtime tests: a supervisor of a release build looks for a new release
//! in crates.io's sparse index as the host's `[update]` says (ADR-t618-1
//! decisions 1 to 3), through a stub `curl`.
use std::os::unix::fs::PermissionsExt;

use crate::runtime_support;

use dagq::infrastructure::release_update::CurlIndex;
use runtime_support::*;

const INDEX: &str = r#"{"name":"dagq","vers":"0.3.0","deps":[],"cksum":"a","features":{},"yanked":false}
{"name":"dagq","vers":"0.5.0","deps":[],"cksum":"b","features":{},"yanked":true}
{"name":"dagq","vers":"0.4.0","deps":[],"cksum":"c","features":{},"yanked":false}
{"name":"dagq","vers":"0.6.0-rc.1","deps":[],"cksum":"d","features":{},"yanked":false}"#;

fn events_of(db: &Path, kind: &str) -> Vec<Value> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT payload FROM run_events WHERE kind=?1 ORDER BY id")
        .unwrap()
        .query_map([kind], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|p| serde_json::from_str(&p.unwrap()).unwrap())
        .collect()
}

fn now_secs() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

/// A queue whose task is canceled (the supervisor claims nothing), the
/// queue's `host.toml` holding `host`, and a stub `curl` that writes its
/// arguments to `calls` (one file per call) and answers `304` to an
/// `If-None-Match`, else `200` with [`INDEX`] (or exits 22 when `fail`).
struct Setup {
    _fixture: Fixture,
    repo: PathBuf,
    db: PathBuf,
    calls: PathBuf,
    options: SuperviseOptions,
}

fn setup(host: &str, current: &str, fail: bool) -> Setup {
    let (fixture, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let queue_dir = db.canonicalize().unwrap().parent().unwrap().to_path_buf();
    fs::write(queue_dir.join("host.toml"), host).unwrap();
    let calls = queue_dir.join("curl-calls");
    fs::create_dir(&calls).unwrap();
    let index = queue_dir.join("index.txt");
    fs::write(&index, format!("{INDEX}\n")).unwrap();
    let curl = queue_dir.join("curl");
    let answer = if fail {
        "echo 'curl: (6) Could not resolve host: index.crates.io' >&2; exit 6".to_owned()
    } else {
        format!(
            "case \"$*\" in *If-None-Match*) printf 'HTTP/2 304\\r\\netag: \"e1\"\\r\\n\\r\\n';; *) printf 'HTTP/2 200\\r\\netag: \"e1\"\\r\\n\\r\\n'; cat '{}';; esac",
            index.display()
        )
    };
    fs::write(
        &curl,
        format!(
            "#!/bin/sh\nn=$(ls '{calls}' | wc -l | tr -d ' ')\nprintf '%s\\n' \"$@\" > \"{calls}/$n\"\n{answer}\n",
            calls = calls.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();
    let options = SuperviseOptions {
        host_config: Some(queue_dir.join("no host-wide file.toml")),
        release_index: Some(runtime::ReleaseIndexPort(Arc::new(CurlIndex {
            program: curl,
            url: "https://index.example/da/gq/dagq".to_owned(),
        }))),
        release_current: Some(current.to_owned()),
        ..supervise_options(1, true)
    };
    Setup {
        _fixture: fixture,
        repo,
        db,
        calls,
        options,
    }
}

impl Setup {
    fn supervise(&self) {
        let backend = TestWorkspace::new(&self.db, false, VALID_AGENT);
        let outcome = supervise_with(&self.db, &self.repo, &backend, &self.options).unwrap();
        assert_eq!(outcome["runs"], json!([]), "{outcome}");
    }

    /// The arguments of each call of the stub `curl`, in order.
    fn calls(&self) -> Vec<String> {
        let mut calls = Vec::new();
        for n in 0.. {
            let Ok(args) = fs::read_to_string(self.calls.join(n.to_string())) else {
                break;
            };
            calls.push(args);
        }
        calls
    }

    fn record(&self, kind: &str, payload: Value) {
        SqliteQueue::open(&self.db)
            .unwrap()
            .record_queue_event(kind, payload)
            .unwrap();
    }

    fn no_asks(&self) {
        let asks = SqliteQueue::open(&self.db)
            .unwrap()
            .asks(AskQuery {
                all: true,
                ..AskQuery::default()
            })
            .unwrap();
        assert!(asks.is_empty(), "{asks:?}");
    }
}

#[test]
fn a_release_build_records_the_latest_release_and_asks_nothing() {
    let s = setup("", "0.3.0", false);
    s.supervise();

    let calls = s.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(
        calls[0].starts_with("-fsS\n--max-time\n10\n") && !calls[0].contains("If-None-Match"),
        "{calls:?}"
    );
    let checked = events_of(&s.db, "release_checked");
    assert_eq!(checked.len(), 1, "{checked:?}");
    assert_eq!(checked[0]["latest"], "0.4.0");
    assert_eq!(checked[0]["current"], "0.3.0");
    assert_eq!(checked[0]["plugin"], Value::Null);
    assert_eq!(checked[0]["etag"], "\"e1\"");
    assert_eq!(checked[0]["not_modified"], false);
    s.no_asks();

    let status = runtime::status(&s.db).unwrap();
    let release = &status["release_update"];
    assert_eq!(release["mode"], "ask", "{status}");
    assert_eq!(release["latest"], "0.4.0");
    assert_eq!(release["checked_at"], checked[0]["checked_at"]);
    // No supervisor is live: `status` judges by its own build.
    let expected = if VERSION.contains(['-', '+']) {
        "not_release"
    } else {
        "update_available"
    };
    assert_eq!(release["state"], expected);
    let attention = status["attention"].to_string();
    assert!(!attention.contains("release"), "{attention}");
}

#[test]
fn a_development_build_and_release_off_do_not_look() {
    let dev = setup("", "0.4.0-dev+abc", false);
    dev.supervise();
    let off = setup("[update]\nrelease = \"off\"\n", "0.3.0", false);
    off.supervise();
    for s in [&dev, &off] {
        assert!(s.calls().is_empty(), "{:?}", s.calls());
        assert!(events_of(&s.db, "release_checked").is_empty());
        assert!(events_of(&s.db, "release_check_failed").is_empty());
    }
    let status = runtime::status(&off.db).unwrap();
    assert_eq!(status["release_update"]["state"], "off", "{status}");
    assert_eq!(status["release_update"]["mode"], "off", "{status}");
}

#[test]
fn a_look_by_another_supervisor_within_the_interval_is_not_repeated() {
    let s = setup("[update]\ncheck_interval_secs = 3600\n", "0.3.0", false);
    s.record(
        "release_checked",
        json!({"latest": "0.4.0", "current": "0.3.0", "checked_at": now_secs() - 60, "etag": "\"e1\"", "supervisor": "another"}),
    );
    s.supervise();
    assert!(s.calls().is_empty(), "{:?}", s.calls());
    assert_eq!(events_of(&s.db, "release_checked").len(), 1);

    // A failed look counts as a look too.
    let failed = setup("", "0.3.0", false);
    failed.record(
        "release_check_failed",
        json!({"error": "timeout", "current": "0.3.0", "checked_at": now_secs() - 60}),
    );
    failed.supervise();
    assert!(failed.calls().is_empty(), "{:?}", failed.calls());
}

#[test]
fn past_the_interval_the_etag_is_sent_and_a_304_keeps_the_last_result() {
    let s = setup("[update]\ncheck_interval_secs = 60\n", "0.3.0", false);
    s.record(
        "release_checked",
        json!({"latest": "0.4.0", "current": "0.3.0", "checked_at": now_secs() - 120, "etag": "\"e1\"", "supervisor": "another"}),
    );
    s.supervise();
    let calls = s.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(
        calls[0].contains("-H\nIf-None-Match: \"e1\"\n"),
        "{calls:?}"
    );
    let checked = events_of(&s.db, "release_checked");
    assert_eq!(checked.len(), 2, "{checked:?}");
    assert_eq!(checked[1]["latest"], "0.4.0");
    assert_eq!(checked[1]["not_modified"], true);
    assert_eq!(checked[1]["etag"], "\"e1\"");
}

#[test]
fn an_index_that_cannot_be_read_is_recorded_and_asks_nothing() {
    let s = setup("", "0.3.0", true);
    s.supervise();
    assert_eq!(s.calls().len(), 1);
    assert!(events_of(&s.db, "release_checked").is_empty());
    let failed = events_of(&s.db, "release_check_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    let error = failed[0]["error"].as_str().unwrap();
    assert!(error.contains("Could not resolve host"), "{error}");
    assert_eq!(failed[0]["current"], "0.3.0");
    s.no_asks();
    let status = runtime::status(&s.db).unwrap();
    let attention = status["attention"].to_string();
    assert!(!attention.contains("release"), "{attention}");
}

#[test]
fn doctor_warns_of_a_wrong_update_value_taken_as_its_default() {
    let s = setup(
        "[update]\nrelease = \"sometimes\"\ncheck_interval_secs = 0\n",
        "0.3.0",
        false,
    );
    let report = runtime::doctor(&s.db, false).unwrap();
    let release = &report["release_update"];
    assert_eq!(release["release"], "ask", "{report}");
    assert_eq!(release["check_interval_secs"], 86_400);
    assert!(
        release["source"]
            .as_str()
            .is_some_and(|source| source.ends_with("host.toml")),
        "{release}"
    );
    let warnings = release["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings[0].as_str().unwrap().contains("release"));
    assert!(
        warnings[1]
            .as_str()
            .unwrap()
            .contains("check_interval_secs")
    );
    // The wrong values are the defaults: the supervisor still looks.
    s.supervise();
    assert_eq!(events_of(&s.db, "release_checked").len(), 1);
}
