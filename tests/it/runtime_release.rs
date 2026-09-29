//! Runtime tests: a supervisor of a release build looks for a new release
//! in crates.io's sparse index as the host's `[update]` says (ADR-t618-1
//! decisions 1 to 3), through a stub `curl`, asks about it, and starts the
//! job that installs it on the answer or without asking (decisions 4, 5),
//! through a stub `cargo` that fails, so nothing is ever replaced.
use dagq::domain::EventKind;
use std::os::unix::fs::PermissionsExt;

use crate::{common, runtime_support};

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
    cargo_calls: PathBuf,
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
    // A cargo that cannot install: it notes its arguments and fails.
    let cargo_calls = queue_dir.join("cargo-calls");
    fs::create_dir(&cargo_calls).unwrap();
    let cargo = queue_dir.join("cargo");
    fs::write(
        &cargo,
        format!(
            "#!/bin/sh\nn=$(ls '{calls}' | wc -l | tr -d ' ')\nprintf '%s\\n' \"$@\" > \"{calls}/$n\"\necho 'error: could not compile dagq' >&2\nexit 101\n",
            calls = cargo_calls.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
    let base = supervise_options(1, true);
    let options = SuperviseOptions {
        update: dagq::application::supervise::UpdateSettings {
            interval: Duration::ZERO,
            cargo: Some(cargo),
            ..base.update.clone()
        },
        host_config: Some(queue_dir.join("no host-wide file.toml")),
        release_index: Some(runtime::ReleaseIndexPort(Arc::new(CurlIndex {
            program: curl,
            url: "https://index.example/da/gq/dagq".to_owned(),
        }))),
        release_current: Some(current.to_owned()),
        ..base
    };
    Setup {
        _fixture: fixture,
        repo,
        db,
        calls,
        cargo_calls,
        options,
    }
}

impl Setup {
    fn supervise(&self) {
        let backend = TestWorkspace::new(&self.db, false, VALID_AGENT);
        let outcome = supervise_with(&self.db, &self.repo, &backend, &self.options).unwrap();
        assert_eq!(outcome["runs"], json!([]), "{outcome}");
    }

    /// [`Self::supervise`] with `claude` as the supervisor's Claude Code.
    fn supervise_with_claude(&self, claude: &Path) {
        let backend = TestWorkspace::new(&self.db, false, VALID_AGENT);
        let outcome = {
            let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
            runtime::supervise(
                &self.db,
                &self.repo,
                &backend,
                claude,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &self.options,
            )
            .unwrap()
        };
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

    fn record(&self, kind: EventKind, payload: Value) {
        SqliteQueue::open(&self.db)
            .unwrap()
            .record_queue_event(kind, payload)
            .unwrap();
    }

    fn asks(&self, all: bool) -> Vec<dagq::domain::Ask> {
        SqliteQueue::open(&self.db)
            .unwrap()
            .asks(AskQuery {
                all,
                ..AskQuery::default()
            })
            .unwrap()
    }

    /// The `approve_release` asks nobody closed.
    fn release_asks(&self) -> Vec<dagq::domain::Ask> {
        self.asks(false)
            .into_iter()
            .filter(|ask| ask.kind == AskKind::ApproveRelease)
            .collect()
    }

    fn answer(&self, ask: &dagq::domain::Ask, text: &str) {
        SqliteQueue::open(&self.db)
            .unwrap()
            .answer(ask.id, text)
            .unwrap();
    }

    /// The release update's steps of `kind` about `release`.
    fn steps(&self, kind: &str, release: &str) -> Vec<Value> {
        events_of(&self.db, kind)
            .into_iter()
            .filter(|p| p["source"] == "release" && p["release"] == release)
            .collect()
    }

    /// Wait for the `times`th `update_failed` of `release`'s job.
    fn wait_failed(&self, release: &str, times: usize) -> Value {
        let started = Instant::now();
        loop {
            let failed = self.steps("update_failed", release);
            if failed.len() >= times {
                return failed[times - 1].clone();
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the job for {release} did not fail: {:?}",
                events_of(&self.db, "update_started")
            );
            thread::sleep(Duration::from_millis(100));
        }
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
fn a_release_build_records_the_latest_release_and_asks_about_it() {
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
    // The release is asked about; the look itself is no attention.
    assert_eq!(s.release_asks().len(), 1);

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
    let attention = status["attention"].as_array().unwrap();
    assert!(
        attention
            .iter()
            .all(|a| a["kind"] != "release_checked" && a["kind"] != "release_check_failed"),
        "{attention:?}"
    );
    assert!(
        attention.iter().any(|a| a["kind"] == "ask_opened"),
        "{attention:?}"
    );
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
        EventKind::ReleaseChecked,
        json!({"latest": "0.4.0", "current": "0.3.0", "checked_at": now_secs() - 60, "etag": "\"e1\"", "supervisor": "another"}),
    );
    s.supervise();
    assert!(s.calls().is_empty(), "{:?}", s.calls());
    assert_eq!(events_of(&s.db, "release_checked").len(), 1);

    // A failed look counts as a look too.
    let failed = setup("", "0.3.0", false);
    failed.record(
        EventKind::ReleaseCheckFailed,
        json!({"error": "timeout", "current": "0.3.0", "checked_at": now_secs() - 60}),
    );
    failed.supervise();
    assert!(failed.calls().is_empty(), "{:?}", failed.calls());
}

#[test]
fn past_the_interval_the_etag_is_sent_and_a_304_keeps_the_last_result() {
    let s = setup("[update]\ncheck_interval_secs = 60\n", "0.3.0", false);
    s.record(
        EventKind::ReleaseChecked,
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

fn record_latest(s: &Setup, latest: &str) {
    s.record(
        EventKind::ReleaseChecked,
        json!({"latest": latest, "current": "0.3.0", "checked_at": now_secs(), "etag": "\"e2\"", "supervisor": "another"}),
    );
}

/// A new release opens one `approve_release` ask about it (ADR-t618-1
/// decision 4), which a later look does not repeat; a newer release closes
/// it as `superseded` and asks about itself.
#[test]
fn a_new_release_opens_one_approve_release_and_a_newer_one_supersedes_it() {
    let s = setup("", "0.3.0", false);
    s.supervise();
    let asks = s.release_asks();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let first = &asks[0];
    assert_eq!(first.subject.as_deref(), Some("0.4.0"));
    assert_eq!(first.options, ["install", "skip"]);
    assert_eq!(first.task_id, None);
    assert_eq!(first.run_id, None);
    assert_eq!(first.asked_by, "supervisor");
    assert_eq!(first.reason_category.as_str(), "scope");
    assert!(
        first.question.contains("dagq 0.4.0 is released")
            && first.question.contains("runs 0.3.0")
            && first.question.contains("cargo install --locked dagq@0.4.0"),
        "{}",
        first.question
    );
    s.supervise();
    assert_eq!(s.release_asks().len(), 1);

    record_latest(&s, "0.5.0");
    s.supervise();
    let asks = s.release_asks();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].subject.as_deref(), Some("0.5.0"));
    let older = s
        .asks(true)
        .into_iter()
        .find(|ask| ask.id == first.id)
        .unwrap();
    assert_eq!(older.answer.as_deref(), Some("superseded"));
    assert!(older.closed_at.is_some());
    assert!(events_of(&s.db, "update_started").is_empty());
}

/// The `install` answer starts the job (ADR-t618-1 decision 5): it runs
/// `cargo install` under the queue's `update/release` and, as cargo fails,
/// replaces nothing and opens `update_failed`; `retry` starts it again and
/// `skip` leaves the release, which is not asked about again.
#[test]
fn the_install_answer_starts_the_job_and_retry_and_skip_follow_its_failure() {
    let s = setup("", "0.3.0", false);
    s.supervise();
    let ask = s.release_asks().remove(0);
    s.answer(&ask, "install");
    s.supervise();
    let answered = s.steps("update_answered", "0.4.0");
    assert_eq!(answered.len(), 1, "{answered:?}");
    assert_eq!(answered[0]["answer"], "install");
    assert!(s.release_asks().is_empty());
    let started = s.steps("update_started", "0.4.0");
    assert_eq!(started.len(), 1, "{:?}", events_of(&s.db, "update_started"));
    assert_eq!(started[0]["version"], "0.3.0");
    let failed = s.wait_failed("0.4.0", 1);
    assert_eq!(failed["stage"], "build", "{failed}");
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .contains("install --locked dagq@0.4.0"),
        "{failed}"
    );
    let queue_dir = s.db.canonicalize().unwrap().parent().unwrap().to_path_buf();
    let args = fs::read_to_string(s.cargo_calls.join("0")).unwrap();
    assert_eq!(
        args,
        format!(
            "install\n--locked\ndagq@0.4.0\n--root\n{}\n--target-dir\n{}\n",
            queue_dir.join("update/release").display(),
            queue_dir.join("update/target").display()
        )
    );
    assert!(events_of(&s.db, "update_installed").is_empty());

    // `retry` starts it again, once.
    let failed_ask = s
        .asks(false)
        .into_iter()
        .find(|ask| ask.kind == AskKind::UpdateFailed)
        .unwrap();
    assert!(
        failed_ask.question.contains("release 0.4.0"),
        "{}",
        failed_ask.question
    );
    s.answer(&failed_ask, "retry");
    s.supervise();
    assert_eq!(s.steps("update_retry", "0.4.0").len(), 1);
    assert_eq!(s.steps("update_started", "0.4.0").len(), 2);
    s.wait_failed("0.4.0", 2);

    // `skip` leaves it: no job, no ask about it any more.
    let failed_ask = s
        .asks(false)
        .into_iter()
        .find(|ask| ask.kind == AskKind::UpdateFailed)
        .unwrap();
    s.answer(&failed_ask, "skip");
    s.supervise();
    s.supervise();
    let skipped: Vec<Value> = s
        .steps("update_answered", "0.4.0")
        .into_iter()
        .filter(|p| p["answer"] == "skip")
        .collect();
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert_eq!(s.steps("update_started", "0.4.0").len(), 2);
    assert!(s.release_asks().is_empty());
    assert!(s.asks(false).is_empty(), "{:?}", s.asks(false));
}

/// A skipped release is not asked about again; the next one is.
#[test]
fn a_skipped_release_is_not_asked_about_again() {
    let s = setup("", "0.3.0", false);
    s.supervise();
    let ask = s.release_asks().remove(0);
    s.answer(&ask, "skip");
    s.supervise();
    assert_eq!(s.steps("update_answered", "0.4.0").len(), 1);
    s.supervise();
    assert!(s.release_asks().is_empty());
    assert!(events_of(&s.db, "update_started").is_empty());
    record_latest(&s, "0.5.0");
    s.supervise();
    assert_eq!(s.release_asks()[0].subject.as_deref(), Some("0.5.0"));
}

/// `release = "auto"` starts the job without an ask, once per release.
#[test]
fn release_auto_installs_without_asking() {
    let s = setup("[update]\nrelease = \"auto\"\n", "0.3.0", false);
    s.supervise();
    assert!(s.release_asks().is_empty());
    assert_eq!(s.steps("update_started", "0.4.0").len(), 1);
    s.wait_failed("0.4.0", 1);
    s.supervise();
    assert_eq!(s.steps("update_started", "0.4.0").len(), 1);
    assert!(s.release_asks().is_empty());
}

/// A supervisor of a development build applies no `approve_release`
/// answer (ADR-t618-1 decision 1): the ask stays for a release build or a
/// person.
#[test]
fn a_development_build_applies_no_release_answer() {
    let s = setup("", "0.4.0-dev+abc", false);
    let mut queue = SqliteQueue::open(&s.db).unwrap();
    let ask = queue
        .open_update_ask(
            AskKind::ApproveRelease,
            "dagq 0.5.0 is released",
            &["install", "skip"],
            "supervisor",
            Some("0.5.0"),
            Value::Null,
        )
        .unwrap();
    queue.answer(ask.id, "install").unwrap();
    s.supervise();
    let ask = queue.read_ask(ask.id).unwrap();
    assert!(ask.closed_at.is_none(), "{ask:?}");
    assert!(events_of(&s.db, "update_answered").is_empty());
    assert!(events_of(&s.db, "update_started").is_empty());
}

/// A job of a release that died without recording how it ended is
/// reported through `update_failed` (stage `interrupted`) even when a
/// step of the automatic update came after it, and not started again on
/// its own.
#[test]
fn an_interrupted_release_job_is_reported_not_retried() {
    let s = setup("", "0.3.0", false);
    record_latest(&s, "0.4.0");
    s.record(
        EventKind::UpdateStarted,
        json!({"pid": 999_999_999u32, "source": "release", "release": "0.4.0"}),
    );
    s.record(
        EventKind::UpdateFailed,
        json!({"stage": "build", "commit": "abc", "ask_id": 0}),
    );
    s.supervise();
    let failed = s.steps("update_failed", "0.4.0");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["stage"], "interrupted");
    assert_eq!(failed[0]["after"], "update_started");
    let ask = s
        .asks(false)
        .into_iter()
        .find(|ask| ask.kind == AskKind::UpdateFailed)
        .unwrap();
    assert!(ask.question.contains("release 0.4.0"), "{}", ask.question);
    assert_eq!(s.steps("update_started", "0.4.0").len(), 1);
    assert!(s.release_asks().is_empty());
    s.supervise();
    assert_eq!(s.steps("update_failed", "0.4.0").len(), 1);
}

/// A `claude` whose installed claude-dagq is `version` and that notes each
/// call's arguments in `<claude>.calls`.
fn plugin_claude(s: &Setup, version: &str) -> PathBuf {
    let claude = s.db.canonicalize().unwrap().with_file_name("plugin-claude");
    fs::write(
        &claude,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{calls}'\ncase \"$*\" in\n\
'plugin list --json') printf '%s' '[{{\"id\":\"claude-dagq@dagq\",\"version\":\"{version}\",\"enabled\":true}}]' ;;\n\
*) echo ok ;;\nesac\n",
            calls = claude.with_extension("calls").display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    claude
}

/// Only the plugin is older than the binary, which is the latest release
/// (ADR-t618-2 decision 4): the look records its version, `approve_release`
/// asks about the plugin alone, and `install` starts the job that runs the
/// two update commands with the supervisor's `claude` and nothing else.
#[test]
fn a_plugin_older_than_the_latest_binary_is_asked_about_and_updated_alone() {
    let s = setup("", "0.4.0", false);
    let claude = plugin_claude(&s, "0.3.0");
    s.supervise_with_claude(&claude);
    let checked = events_of(&s.db, "release_checked");
    assert_eq!(checked[0]["plugin"], "0.3.0", "{checked:?}");
    let asks = s.release_asks();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].subject.as_deref(), Some("0.4.0"));
    assert!(
        asks[0]
            .question
            .contains("plugin installed in Claude Code is 0.3.0")
            && asks[0].question.contains("the binary is not touched"),
        "{}",
        asks[0].question
    );
    s.supervise_with_claude(&claude);
    assert_eq!(s.release_asks().len(), 1);

    s.answer(&asks[0], "install");
    s.supervise_with_claude(&claude);
    let answered = s.steps("update_answered", "0.4.0");
    assert_eq!(answered[0]["plugin_only"], true, "{answered:?}");
    let started = s.steps("update_started", "0.4.0");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["plugin_only"], true);
    let installed = {
        let begun = Instant::now();
        loop {
            if let Some(installed) = s.steps("update_installed", "0.4.0").pop() {
                break installed;
            }
            assert!(
                begun.elapsed() < Duration::from_secs(60),
                "the plugin job did not end: {:?}",
                events_of(&s.db, "update_failed")
            );
            thread::sleep(Duration::from_millis(100));
        }
    };
    assert_eq!(installed["plugin_only"], true, "{installed}");
    assert_eq!(installed["plugin"]["updated"], true, "{installed}");
    let calls = fs::read_to_string(claude.with_extension("calls")).unwrap();
    assert!(
        calls.contains("plugin marketplace update dagq\nplugin update claude-dagq@dagq\n"),
        "{calls}"
    );
    assert!(fs::read_dir(&s.cargo_calls).unwrap().next().is_none());
    // Done: not asked or started again.
    s.supervise_with_claude(&claude);
    assert!(s.release_asks().is_empty());
    assert_eq!(s.steps("update_started", "0.4.0").len(), 1);
}

/// A supervisor on `--plugin-dir` neither reads nor asks about the
/// installed plugin (ADR-t618-2 decision 3).
#[test]
fn a_plugin_dir_supervisor_leaves_the_installed_plugin() {
    let mut s = setup("", "0.4.0", false);
    s.options.plugin_dir = Some(s.repo.clone());
    let claude = plugin_claude(&s, "0.3.0");
    s.supervise_with_claude(&claude);
    let checked = events_of(&s.db, "release_checked");
    assert_eq!(checked[0]["plugin"], Value::Null, "{checked:?}");
    assert!(s.release_asks().is_empty());
    let calls = fs::read_to_string(claude.with_extension("calls")).unwrap_or_default();
    assert!(!calls.contains("plugin"), "{calls}");
}

/// Record the look of a supervisor of build `current` that found `latest`.
fn record_look(s: &Setup, current: &str, latest: &str) {
    s.record(
        EventKind::ReleaseChecked,
        json!({"latest": latest, "current": current, "checked_at": now_secs(), "etag": "\"e2\"", "supervisor": "another"}),
    );
}

/// Open an `approve_release` about `release` as a supervisor opens it for
/// the plugin alone (`Some(true)`) or the binary (`Some(false)`), or as an
/// older runtime did, without saying (`None`), and answer it `answer`.
fn answered_release_ask(s: &Setup, release: &str, plugin_only: Option<bool>, answer: &str) {
    let mut queue = SqliteQueue::open(&s.db).unwrap();
    let details = plugin_only.map_or(
        Value::Null,
        |plugin_only| json!({"plugin_only": plugin_only}),
    );
    let ask = queue
        .open_update_ask(
            AskKind::ApproveRelease,
            &format!("about {release}"),
            &["install", "skip"],
            "supervisor",
            Some(release),
            details,
        )
        .unwrap();
    queue.answer(ask.id, answer).unwrap();
}

/// A plugin-only answer stays about the plugin when a supervisor of an
/// older build applies it: it is recorded `plugin_only` and starts no job
/// of the binary, nor is it dropped (its release is the latest).
#[test]
fn a_plugin_answer_applied_by_another_build_stays_about_the_plugin() {
    let s = setup("", "0.3.0", false);
    record_look(&s, "0.3.0", "0.4.0");
    answered_release_ask(&s, "0.4.0", Some(true), "install");
    s.supervise();
    let answered = s.steps("update_answered", "0.4.0");
    assert_eq!(answered.len(), 1, "{answered:?}");
    assert_eq!(answered[0]["plugin_only"], true, "{answered:?}");
    assert!(events_of(&s.db, "update_started").is_empty());
    assert!(events_of(&s.db, "update_dropped").is_empty());
}

/// An answer about the binary stays about the binary when a supervisor
/// already of its release applies it: not `plugin_only`, and no plugin job.
#[test]
fn a_binary_answer_applied_by_its_own_build_is_not_about_the_plugin() {
    let s = setup("", "0.4.0", false);
    record_look(&s, "0.4.0", "0.4.0");
    answered_release_ask(&s, "0.4.0", Some(false), "install");
    s.supervise();
    let answered = s.steps("update_answered", "0.4.0");
    assert_eq!(answered.len(), 1, "{answered:?}");
    assert_eq!(answered[0]["plugin_only"], false, "{answered:?}");
    assert!(events_of(&s.db, "update_started").is_empty());
}

/// An ask opened before its purpose was recorded is about the plugin when
/// its release is the build that applies the answer, else the binary's.
#[test]
fn an_ask_without_its_purpose_is_about_the_plugin_only_on_its_own_build() {
    for (current, plugin_only) in [("0.4.0", true), ("0.3.0", false)] {
        let s = setup("", current, false);
        record_look(&s, current, "0.4.0");
        answered_release_ask(&s, "0.4.0", None, "skip");
        s.supervise();
        let answered = s.steps("update_answered", "0.4.0");
        assert_eq!(answered.len(), 1, "{answered:?}");
        assert_eq!(answered[0]["plugin_only"], plugin_only, "{current}");
    }
}

/// A plugin-only `install` applied after a newer release is out is not
/// run: `update_dropped` says so once, with the release that supersedes
/// it, and the newer release is asked about.
#[test]
fn a_plugin_install_behind_a_newer_release_is_dropped_with_its_reason() {
    let s = setup("", "0.4.0", false);
    record_look(&s, "0.4.0", "0.5.0");
    answered_release_ask(&s, "0.4.0", Some(true), "install");
    s.supervise();
    let answered = s.steps("update_answered", "0.4.0");
    assert_eq!(answered[0]["plugin_only"], true, "{answered:?}");
    let dropped = s.steps("update_dropped", "0.4.0");
    assert_eq!(dropped.len(), 1, "{dropped:?}");
    assert_eq!(dropped[0]["ask_id"], answered[0]["ask_id"]);
    assert_eq!(dropped[0]["answer"], "install");
    assert_eq!(dropped[0]["plugin_only"], true);
    assert_eq!(dropped[0]["reason"], "newer_release");
    assert_eq!(dropped[0]["newer"], "0.5.0");
    assert!(dropped[0]["supervisor"].is_string(), "{dropped:?}");
    assert!(events_of(&s.db, "update_started").is_empty());
    assert_eq!(s.release_asks()[0].subject.as_deref(), Some("0.5.0"));
    s.supervise();
    assert_eq!(s.steps("update_dropped", "0.4.0").len(), 1);
    assert!(events_of(&s.db, "update_started").is_empty());
}
