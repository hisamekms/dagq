//! Runtime tests: the supervisor's watch of the landing branch's CI
//! through `gh` (ADR-t1920-1), with a fake `gh` that reads the runs, the
//! jobs and the JUnit artifacts from files beside it. The decisions (what
//! a run adds and removes, the key, the range) are `domain::ci_watch`'s
//! unit tests; these hold the wiring: `[ci_watch]` read by the supervisor,
//! the events and the finding written to the queue, the list read back,
//! and the hold while `gh` is missing or logged out.
use crate::runtime_support;

use dagq::domain::{FindingId, FindingStatus};
use dagq::runtime::CiWatchOptions;
use runtime_support::*;

/// A fake `gh`: `auth status` passes once `authed` exists beside it, `run
/// list` prints `runs.json`, `run view ID` prints `jobs-ID.json` (fails
/// while `jobs-ID.fail` exists), and `run download ID --pattern P --dir D`
/// copies the artifacts `artifacts-ID/P` into `D` (fails when none
/// matches, as gh does). Each call is appended to `calls`.
const FAKE_GH: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo "$*" >> "$dir/calls"
case "$1 $2" in
"auth status")
    [ -f "$dir/authed" ] && exit 0
    echo "You are not logged into any GitHub hosts. To log in, run: gh auth login" >&2
    exit 1 ;;
"run list") cat "$dir/runs.json" ;;
"run view")
    [ -f "$dir/jobs-$3.fail" ] && { echo "HTTP 502" >&2; exit 1; }
    cat "$dir/jobs-$3.json" 2>/dev/null || echo '{"jobs":[]}' ;;
"run download")
    id=$3
    out=
    pattern=
    while [ $# -gt 0 ]; do
        [ "$1" = --dir ] && out=$2
        [ "$1" = --pattern ] && pattern=$2
        shift
    done
    found=
    for artifact in "$dir/artifacts-$id"/$pattern; do
        [ -d "$artifact" ] || continue
        found=1
        mkdir -p "$out" && cp -R "$artifact" "$out/" || exit 1
    done
    [ -n "$found" ] || { echo "no valid artifacts found to download" >&2; exit 1; } ;;
*) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#;

/// Write `[ci_watch]` to the repository's `dagq.toml` and name a GitHub
/// remote; the fake `gh` goes in `bin/` of the fixture, logged in.
fn watched(fixture: &Fixture, repo: &Path) -> PathBuf {
    watched_with(fixture, repo, "")
}

/// `watched` with `more` keys in `[ci_watch]`.
fn watched_with(fixture: &Fixture, repo: &Path, more: &str) -> PathBuf {
    fs::write(
        repo.join("dagq.toml"),
        // Overlapping globs and one that matches nothing: each into its
        // own directory, the others read all the same.
        format!("[ci_watch]\nworkflow = \"ci.yml\"\njunit_artifacts = [\"junit-*\", \"junit-mac*\", \"absent-*\"]\n{more}"),
    )
    .unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-m", "ci watch"]);
    git(
        repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/name.git",
        ],
    );
    let bin = fixture.dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    crate::common::template::script(&gh, FAKE_GH);
    gh
}

/// Asks the supervisor to stop when dropped: on a test's panic too.
struct StopOnDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn options(gh: &Path) -> SuperviseOptions {
    watching(supervise_options(4, true), gh)
}

/// `options` that watch the CI through `gh`.
fn watching(mut options: SuperviseOptions, gh: &Path) -> SuperviseOptions {
    options.ci_watch = Some(CiWatchOptions {
        program: gh.to_str().unwrap().to_owned(),
        // One check at each start: the interval never passes within a test.
        interval: None,
    });
    // No planner of the runtime's takes the finding in these tests.
    options.runtime_planners = Some(0);
    options
}

/// The runs `gh run list` returns: (id, conclusion, commit), in order,
/// each at its first attempt.
fn runs(gh: &Path, runs: &[(i64, &str, &str)]) {
    let first: Vec<(i64, i64, &str, &str)> = runs
        .iter()
        .map(|(id, conclusion, sha)| (*id, 1, *conclusion, *sha))
        .collect();
    attempts(gh, &first);
}

/// The runs `gh run list` returns: (id, attempt, conclusion, commit); a
/// run's creation time follows its ID whatever its attempt.
fn attempts(gh: &Path, runs: &[(i64, i64, &str, &str)]) {
    let list: Vec<Value> = runs
        .iter()
        .map(|(id, attempt, conclusion, sha)| {
            json!({
                "databaseId": id, "number": id, "attempt": attempt, "headSha": sha,
                "conclusion": conclusion,
                "url": format!("https://github.com/owner/name/actions/runs/{id}"),
                "createdAt": format!("2026-10-06T00:{id:02}:00Z"), "displayTitle": "x",
            })
        })
        .collect();
    fs::write(gh.with_file_name("runs.json"), json!(list).to_string()).unwrap();
}

/// A red run's failed job and its JUnit, with `failed` failing and
/// `passed` passing.
fn red(gh: &Path, id: i64, failed: &[&str], passed: &[&str]) {
    fs::write(
        gh.with_file_name(format!("jobs-{id}.json")),
        json!({"jobs": [
            {"name": "test", "conclusion": "failure",
             "steps": [{"name": "nextest", "conclusion": "failure"}]},
            {"name": "lint", "conclusion": "success", "steps": []},
        ]})
        .to_string(),
    )
    .unwrap();
    let cases: String = failed
        .iter()
        .map(|name| {
            format!("<testcase classname=\"dagq::it\" name=\"{name}\"><failure/></testcase>")
        })
        .chain(
            passed
                .iter()
                .map(|name| format!("<testcase classname=\"dagq::it\" name=\"{name}\"/>")),
        )
        .collect();
    let dir = gh
        .with_file_name(format!("artifacts-{id}"))
        .join("junit-macos");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("junit.xml"),
        format!("<testsuites><testsuite name=\"dagq::it\">{cases}</testsuite></testsuites>"),
    )
    .unwrap();
}

/// The queue events of the watch, oldest first: (kind, payload).
/// The check's records: the supervisor's hold for the watch apart
/// ([`hold_events`]).
fn watch_events(db: &Path) -> Vec<(String, Value)> {
    events_where(
        db,
        "kind LIKE 'ci_%' AND kind NOT IN ('ci_watch_held', 'ci_watch_resumed')",
    )
}

/// The supervisors' `ci_watch_held` / `ci_watch_resumed`.
fn hold_events(db: &Path) -> Vec<(String, Value)> {
    events_where(db, "kind IN ('ci_watch_held', 'ci_watch_resumed')")
}

fn events_where(db: &Path, filter: &str) -> Vec<(String, Value)> {
    Connection::open(db)
        .unwrap()
        .prepare(&format!(
            "SELECT kind, payload FROM run_events WHERE {filter} ORDER BY id"
        ))
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                serde_json::from_str(&row.get::<_, String>(1)?).unwrap(),
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn head(repo: &Path) -> String {
    git_out(repo, &["rev-parse", "HEAD"]).trim().to_owned()
}

fn commit(repo: &Path, name: &str) -> String {
    fs::write(repo.join(name), name).unwrap();
    git(repo, &["add", name]);
    git(repo, &["commit", "-m", name]);
    head(repo)
}

/// AC 1–4: the supervisor checks at its interval and records each settled
/// run; green to red records the turn and one `ci_failure` finding with
/// the tests, the range over the cancelled run, the URL and whether the
/// build contains it; the same failures again add none; the list is read
/// back (with a fix task's own items kept apart) and empties on green,
/// which resolves the finding.
#[test]
fn the_watch_records_runs_files_one_finding_per_failure_set_and_keeps_the_list() {
    let (fixture, repo, db) = fixture();
    let gh = watched(&fixture, &repo);
    fs::write(gh.with_file_name("authed"), "").unwrap();
    // Nothing to claim: only the watch works.
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    drop(queue);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = options(&gh);
    let green = head(&repo);
    let cancelled = commit(&repo, "two");
    let first_red = commit(&repo, "three");

    // The first check takes the newest run only.
    runs(&gh, &[(1, "success", &green)]);
    supervise_with(&db, &repo, &backend, &options).unwrap();
    let events = watch_events(&db);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].0, "ci_checked");
    assert_eq!(events[0].1["state"], "green");
    assert_eq!(events[0].1["workflow"], "ci.yml");
    assert_eq!(events[0].1["branch"], "main");
    assert_eq!(events[0].1["junit"], "missing");

    // A cancelled run, then a red one: the red takes the cancelled one's
    // range.
    runs(
        &gh,
        &[
            (1, "success", &green),
            (2, "cancelled", &cancelled),
            (3, "failure", &first_red),
        ],
    );
    red(
        &gh,
        3,
        &["runtime_claim::fails"],
        &["runtime_claim::passes"],
    );
    supervise_with(&db, &repo, &backend, &options).unwrap();
    let events = watch_events(&db);
    let kinds: Vec<&str> = events.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(kinds, ["ci_checked", "ci_checked", "ci_turned_red"]);
    let checked = &events[1].1;
    assert_eq!(checked["run_id"], 3);
    assert_eq!(checked["skipped_runs"], json!([2]));
    assert_eq!(checked["junit"], "read");
    assert_eq!(checked["added"], json!(["dagq::it runtime_claim::fails"]));
    assert_eq!(
        checked["failed_jobs"],
        json!([{"job": "test", "steps": ["nextest"]}])
    );
    let finding_id = checked["finding_id"].as_i64().unwrap();
    assert_eq!(events[2].1["last_green"]["sha"], green.as_str());
    assert_eq!(events[2].1["finding_ids"], json!([finding_id]));
    let queue = SqliteQueue::open(&db).unwrap();
    let findings = queue
        .findings(&dagq::domain::FindingQuery::default())
        .unwrap();
    assert_eq!(findings.len(), 1);
    let finding = &findings[0].finding;
    assert_eq!(finding.kind, "ci_failure");
    assert!(finding.subject.starts_with("ci_failure:"));
    assert!(finding.propose_reason.is_some());
    let detail: Value = serde_json::from_str(&finding.detail).unwrap();
    assert_eq!(detail["tests"], json!(["dagq::it runtime_claim::fails"]));
    assert_eq!(
        detail["range"],
        json!({"from": green, "to": first_red, "commits": 2})
    );
    assert_eq!(
        detail["url"],
        "https://github.com/owner/name/actions/runs/3"
    );
    // The test binary names a commit of dagq's own, not of this fixture.
    assert_eq!(detail["binary_contains"], "unknown");
    drop(queue);

    // The same failure again records no new finding.
    let again = commit(&repo, "four");
    runs(
        &gh,
        &[
            (1, "success", &green),
            (2, "cancelled", &cancelled),
            (3, "failure", &first_red),
            (4, "failure", &again),
        ],
    );
    red(&gh, 4, &["runtime_claim::fails"], &[]);
    // Its jobs cannot be read: the run is taken without them, and the
    // watch goes on.
    fs::write(gh.with_file_name("jobs-4.fail"), "").unwrap();
    supervise_with(&db, &repo, &backend, &options).unwrap();
    let events = watch_events(&db);
    assert_eq!(events.len(), 4, "{events:?}");
    assert_eq!(events[3].1["run_id"], 4);
    assert_eq!(events[3].1["failed_jobs"], json!([]));
    assert_eq!(events[3].1["junit"], "read");
    assert_eq!(events[3].1["added"], json!([]));
    assert_eq!(events[3].1["known_failures"], 1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue
            .findings(&dagq::domain::FindingQuery::default())
            .unwrap()
            .len(),
        1
    );

    // The list, as `ci failures` and `status` read it.
    let list = runtime::ci_failures(&db, None).unwrap();
    assert_eq!(list["enabled"], true);
    assert_eq!(list["state"], "red");
    assert_eq!(list["watch"], "available");
    assert_eq!(list["workflow"], "ci.yml");
    assert_eq!(list["latest_run"]["run_id"], 4);
    assert_eq!(list["failures"][0]["name"], "dagq::it runtime_claim::fails");
    assert_eq!(list["failures"][0]["added"]["run_id"], 3);
    assert_eq!(list["failures"][0]["finding_id"], finding_id);
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["ci"]["state"], "red", "{status}");
    assert_eq!(status["ci"]["failures"], 1);

    // A task that fixes it covers the finding; its runs keep the test.
    let fix = queue
        .add(NewTask {
            title: "fix the test".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: vec![],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: None,
            wait_for_build: false,
        })
        .unwrap();
    // A closed task covers nothing (task 1 was canceled above).
    let refused = queue
        .set_finding_status_covered(
            FindingId::new(finding_id),
            FindingStatus::Dismissed,
            "task fixes it",
            "planner",
            Some(TaskId::new(1)),
        )
        .unwrap_err();
    assert!(
        format!("{refused:#}").contains("completed or canceled"),
        "{refused:#}"
    );
    let dismissed = queue
        .set_finding_status_covered(
            FindingId::new(finding_id),
            FindingStatus::Dismissed,
            "task fixes it",
            "planner",
            Some(fix.id()),
        )
        .unwrap();
    assert_eq!(dismissed.covered_by_task, Some(fix.id()));
    drop(queue);
    let kept = runtime::ci_failures(&db, Some(fix.id())).unwrap();
    assert_eq!(kept["failures"], json!([]));
    assert_eq!(
        kept["kept_for_task"][0]["name"],
        "dagq::it runtime_claim::fails"
    );

    // Green: the list empties, the turn is recorded.
    let fixed = commit(&repo, "five");
    runs(
        &gh,
        &[
            (1, "success", &green),
            (2, "cancelled", &cancelled),
            (3, "failure", &first_red),
            (4, "failure", &again),
            (5, "success", &fixed),
        ],
    );
    supervise_with(&db, &repo, &backend, &options).unwrap();
    let events = watch_events(&db);
    let last: Vec<&str> = events[4..].iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(last, ["ci_checked", "ci_turned_green"]);
    assert_eq!(
        events[4].1["removed"],
        json!([{"name": "dagq::it runtime_claim::fails", "reason": "green"}])
    );
    assert_eq!(events[5].1["red_since"]["run_id"], 3);
    assert_eq!(events[5].1["red_secs"], 120);
    let list = runtime::ci_failures(&db, None).unwrap();
    assert_eq!(list["state"], "green");
    assert_eq!(list["failures"], json!([]));
    backend.join();
}

/// AC 5: without `gh`, or with one that is logged out, the supervisor
/// claims nothing and tells the inbox once per answer; `up` refuses to
/// start; once `gh` reads, the claims resume.
#[test]
fn a_missing_or_logged_out_gh_holds_the_claims_and_tells_the_inbox() {
    let (fixture, repo, db) = fixture();
    let gh = watched(&fixture, &repo);
    // Not there yet.
    let missing = gh.with_file_name("missing-gh");
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    for _ in 0..2 {
        let outcome = supervise_with(&db, &repo, &backend, &options(&missing)).unwrap();
        assert_eq!(outcome["runs"], json!([]), "{outcome}");
    }
    assert!(backend.launched.lock().unwrap().is_empty());
    let events = watch_events(&db);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].0, "ci_watch_unavailable");
    assert_eq!(events[0].1["reason"], "gh_missing");
    let attention = |db: &Path| {
        runtime::status(db).unwrap()["attention"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["kind"] == "ci_watch_unavailable")
            .cloned()
    };
    let install = attention(&db).unwrap();
    assert_eq!(install["next"], "install tool");
    assert_eq!(install["status"], "unavailable");
    assert!(
        install["last_error"]
            .as_str()
            .unwrap()
            .contains("[ci_watch]"),
        "{install}"
    );
    assert_eq!(
        runtime::ci_failures(&db, None).unwrap()["watch"],
        "unavailable"
    );
    // `doctor` shows the table and the supervisor's last answer.
    let doctor = runtime::doctor(&db, false).unwrap();
    assert_eq!(
        doctor["ci_watch"]["config"]["workflow"], "ci.yml",
        "{doctor}"
    );
    assert_eq!(doctor["ci_watch"]["repo"], "owner/name");
    assert_eq!(
        doctor["ci_watch"]["supervisor_last"]["kind"],
        "ci_watch_unavailable"
    );

    // Logged out: the new reason is recorded and the person logs in.
    let outcome = supervise_with(&db, &repo, &backend, &options(&gh)).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert!(backend.launched.lock().unwrap().is_empty());
    let events = watch_events(&db);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[1].1["reason"], "gh_unauthenticated");
    assert_eq!(attention(&db).unwrap()["next"], "log in to gh");
    // `up`'s preflight says the same before any supervisor starts.
    let path = format!("{}:/usr/bin:/bin", gh.parent().unwrap().display());
    let message = dagq::infrastructure::ci_watch::preflight(&repo, Some(path.clone().into()))
        .unwrap()
        .unwrap();
    assert!(message.contains("gh auth login"), "{message}");
    assert_eq!(
        dagq::infrastructure::ci_watch::preflight(&repo, Some("/nonexistent".into()))
            .unwrap()
            .map(|message| message.contains("is not found")),
        Some(true)
    );

    // Logged in: the return is recorded, the attention ends and the task
    // is claimed.
    fs::write(gh.with_file_name("authed"), "").unwrap();
    runs(&gh, &[]);
    let outcome = supervise_with(&db, &repo, &backend, &options(&gh)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let events = watch_events(&db);
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(events[2].0, "ci_watch_available");
    assert_eq!(events[2].1["resolved"], gh.to_str().unwrap());
    assert!(attention(&db).is_none());
    // Each supervisor recorded its hold where it changed, not per pass:
    // waiting for its first answer, then for the means while they were
    // missing, and its end once they were there.
    let holds: Vec<(String, String, String)> = hold_events(&db)
        .into_iter()
        .map(|(kind, payload)| {
            (
                payload["supervisor"].as_str().unwrap().to_owned(),
                kind,
                payload["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let mut supervisors: Vec<&str> = holds.iter().map(|(token, ..)| token.as_str()).collect();
    supervisors.dedup();
    assert_eq!(supervisors.len(), 4, "{holds:?}");
    let of = |token: &str| -> Vec<(&str, &str)> {
        holds
            .iter()
            .filter(|(holder, ..)| holder == token)
            .map(|(_, kind, reason)| (kind.as_str(), reason.as_str()))
            .collect()
    };
    for token in &supervisors[..3] {
        assert_eq!(
            of(token),
            [
                ("ci_watch_held", "pending"),
                ("ci_watch_held", "unreadable")
            ],
            "{holds:?}"
        );
    }
    assert_eq!(
        of(supervisors[3]),
        [
            ("ci_watch_held", "pending"),
            ("ci_watch_resumed", "pending")
        ],
        "{holds:?}"
    );
    assert_eq!(
        dagq::infrastructure::ci_watch::preflight(&repo, Some(path.clone().into())).unwrap(),
        None
    );
    let run = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
}

/// A supervisor that continues under its token after a handoff reads its
/// own latest hold however many records of the same kinds the others wrote
/// since, and does not record the hold it already holds again.
#[test]
fn a_handed_off_supervisor_reads_its_own_hold_past_the_others_records() {
    use dagq::domain::{EventKind, LeaseToken};
    let (fixture, repo, db) = fixture();
    let gh = watched(&fixture, &repo);
    let missing = gh.with_file_name("missing-gh");
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .register_supervisor(&LeaseToken::new("handed"), std::process::id(), 4, "0.0.1")
        .unwrap();
    queue
        .record_queue_event(
            EventKind::CiWatchHeld,
            json!({"reason": "pending", "workflow": "ci.yml", "supervisor": "handed"}),
        )
        .unwrap();
    for _ in 0..70 {
        for kind in [EventKind::CiWatchHeld, EventKind::CiWatchResumed] {
            queue
                .record_queue_event(kind, json!({"reason": "pending", "supervisor": "other"}))
                .unwrap();
        }
    }
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = SuperviseOptions {
        handoff_token: Some(LeaseToken::new("handed")),
        ..options(&missing)
    };
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    let handed: Vec<(String, String)> = hold_events(&db)
        .into_iter()
        .filter(|(_, payload)| payload["supervisor"] == "handed")
        .map(|(kind, payload)| (kind, payload["reason"].as_str().unwrap().to_owned()))
        .collect();
    assert_eq!(
        handed,
        [
            ("ci_watch_held".to_owned(), "pending".to_owned()),
            ("ci_watch_held".to_owned(), "unreadable".to_owned()),
        ],
        "{handed:?}"
    );
}

/// AC 1: a supervisor that may watch the CI but whose `dagq.toml` has no
/// `[ci_watch]` reads nothing, records nothing and holds nothing.
#[test]
fn without_the_table_nothing_is_watched_nor_held() {
    let (fixture, repo, db) = fixture();
    let bin = fixture.dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    crate::common::template::script(&gh, FAKE_GH);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &options(&gh)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(watch_events(&db).is_empty());
    assert!(hold_events(&db).is_empty());
    assert_eq!(runtime::status(&db).unwrap()["ci"], Value::Null);
    assert_eq!(
        runtime::ci_failures(&db, None).unwrap(),
        json!({"enabled": false, "state": "unknown", "watch": "disabled", "failures": []})
    );
    let run = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
}

/// AC 1: a supervisor at work checks at its start, not again before the
/// interval has passed on its clock, and again once it has.
#[test]
fn a_supervisor_at_work_checks_again_once_the_interval_passed() {
    let (fixture, repo, db) = fixture();
    let gh = watched(&fixture, &repo);
    fs::write(gh.with_file_name("authed"), "").unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    drop(queue);
    let first = head(&repo);
    let second = commit(&repo, "two");
    runs(&gh, &[(1, "success", &first)]);
    // The supervisor's monotonic clock, which its interval is measured
    // on, moved on by hand.
    let (options, ahead) = supervise_options_ahead(4, false);
    let mut options = watching(options, &gh);
    if let Some(watch) = &mut options.ci_watch {
        watch.interval = Some(Duration::from_secs(600));
    }
    let stop = options.stop.clone();
    let passes = options.passes.clone();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let checked = |db: &Path| {
        watch_events(db)
            .iter()
            .filter(|(kind, _)| kind == "ci_checked")
            .count()
    };
    let wait_for = |what: &str, done: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "waited for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    std::thread::scope(|scope| {
        let supervisor = scope.spawn(|| supervise_with(&db, &repo, &backend, &options));
        // A failed assertion below still stops the supervisor, so the
        // scope's join ends.
        let _stop = StopOnDrop(stop.clone());
        wait_for("the first check", &|| checked(&db) == 1);
        // A new run before the interval passed: not read.
        runs(&gh, &[(1, "success", &first), (2, "success", &second)]);
        let seen = passes.load(std::sync::atomic::Ordering::SeqCst);
        wait_for("more passes", &|| {
            passes.load(std::sync::atomic::Ordering::SeqCst) >= seen + 10
        });
        assert_eq!(checked(&db), 1);
        // The interval passes on the supervisor's clock.
        ahead.by(Duration::from_secs(600));
        wait_for("the second check", &|| checked(&db) == 2);
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _waiting =
            crate::common::within(crate::common::STEP_LIMIT, "the supervisor at work to stop");
        supervisor.join().unwrap().unwrap();
    });
    assert_eq!(watch_events(&db)[1].1["run_id"], 2);
}

/// AC 2–4: a run created before a later one but ended after it is read at
/// the next check and recorded `late`, leaving the list and the state to
/// the later run; a green re-run of the red run (a new attempt of the
/// same ID) is read and empties the list; neither is read twice.
#[test]
fn a_run_that_ends_late_and_a_green_re_run_are_each_recorded_once() {
    let (fixture, repo, db) = fixture();
    let gh = watched(&fixture, &repo);
    fs::write(gh.with_file_name("authed"), "").unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    drop(queue);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = options(&gh);
    let green = head(&repo);
    let slow = commit(&repo, "two");
    let flaky = commit(&repo, "three");

    runs(&gh, &[(1, "success", &green)]);
    supervise_with(&db, &repo, &backend, &options).unwrap();
    // Run 3 ends red while run 2, created before it, still runs.
    runs(&gh, &[(1, "success", &green), (3, "failure", &flaky)]);
    red(&gh, 3, &["runtime_claim::flaky"], &[]);
    supervise_with(&db, &repo, &backend, &options).unwrap();
    // Run 2 ends red with another failure: recorded late, nothing changes.
    runs(
        &gh,
        &[
            (1, "success", &green),
            (2, "failure", &slow),
            (3, "failure", &flaky),
        ],
    );
    red(&gh, 2, &["runtime_claim::other"], &[]);
    supervise_with(&db, &repo, &backend, &options).unwrap();
    let checked = |db: &Path| -> Vec<Value> {
        watch_events(db)
            .into_iter()
            .filter(|(kind, _)| kind == "ci_checked")
            .map(|(_, payload)| payload)
            .collect()
    };
    let records = checked(&db);
    assert_eq!(
        records
            .iter()
            .map(|r| (
                r["run_id"].as_i64().unwrap(),
                r["attempt"].as_i64().unwrap()
            ))
            .collect::<Vec<_>>(),
        [(1, 1), (3, 1), (2, 1)]
    );
    assert_eq!(records[2]["late"], true);
    assert_eq!(
        records[2]["failed_tests"],
        json!(["dagq::it runtime_claim::other"])
    );
    assert_eq!(records[2]["added"], json!([]));
    assert_eq!(
        SqliteQueue::open(&db)
            .unwrap()
            .findings(&dagq::domain::FindingQuery::default())
            .unwrap()
            .len(),
        1
    );
    let list = runtime::ci_failures(&db, None).unwrap();
    assert_eq!(list["state"], "red");
    assert_eq!(list["latest_run"]["run_id"], 3);
    assert_eq!(list["latest_run"]["attempt"], 1);
    assert_eq!(list["failures"][0]["name"], "dagq::it runtime_claim::flaky");
    assert_eq!(list["failures"][0]["added"]["attempt"], 1);
    assert_eq!(list["failures"].as_array().unwrap().len(), 1);

    // The red run 3 is re-run and passes: its new attempt empties the list.
    attempts(
        &gh,
        &[
            (1, 1, "success", &green),
            (2, 1, "failure", &slow),
            (3, 2, "success", &flaky),
        ],
    );
    supervise_with(&db, &repo, &backend, &options).unwrap();
    // A check with nothing new records nothing: no run or attempt twice.
    supervise_with(&db, &repo, &backend, &options).unwrap();
    let events = watch_events(&db);
    let kinds: Vec<&str> = events.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "ci_checked",
            "ci_checked",
            "ci_turned_red",
            "ci_checked",
            "ci_checked",
            "ci_turned_green"
        ]
    );
    let rerun = &events[4].1;
    assert_eq!(
        (rerun["run_id"].as_i64(), rerun["attempt"].as_i64()),
        (Some(3), Some(2))
    );
    assert_eq!(
        rerun["removed"],
        json!([{"name": "dagq::it runtime_claim::flaky", "reason": "green"}])
    );
    let list = runtime::ci_failures(&db, None).unwrap();
    assert_eq!(list["state"], "green");
    assert_eq!(list["failures"], json!([]));
    assert_eq!(list["latest_run"]["attempt"], 2);
    // Its finding is resolved: none is left open.
    let open = SqliteQueue::open(&db)
        .unwrap()
        .findings(&dagq::domain::FindingQuery::default())
        .unwrap();
    assert!(open.is_empty(), "{open:?}");
    backend.join();
}

/// The jobs `gh run view ID` prints: (name, conclusion).
fn jobs(gh: &Path, id: i64, jobs: &[(&str, &str)]) {
    let jobs: Vec<Value> = jobs
        .iter()
        .map(|(name, conclusion)| json!({"name": name, "conclusion": conclusion, "steps": []}))
        .collect();
    fs::write(
        gh.with_file_name(format!("jobs-{id}.json")),
        json!({ "jobs": jobs }).to_string(),
    )
    .unwrap();
}

/// ADR-t2034-1 decisions 4 and 5: with
/// `required_jobs`, a success run whose named job was skipped, one that
/// lacks a named job (told to the inbox once) and one whose jobs stay
/// unreadable (read again at each check, then given up at the limit) are
/// not read green: the list stays and the next settled run takes them in
/// `skipped_runs`.
#[test]
fn success_runs_without_their_named_jobs_are_not_read_green() {
    let (fixture, repo, db) = fixture();
    let gh = watched_with(&fixture, &repo, "required_jobs = [\"rust\", \"linux\"]\n");
    fs::write(gh.with_file_name("authed"), "").unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    drop(queue);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = options(&gh);
    let red_sha = head(&repo);
    let docs = commit(&repo, "docs");
    let renamed = commit(&repo, "renamed");
    let unread = commit(&repo, "unread");
    let unread_too = commit(&repo, "unread too");
    let fixed = commit(&repo, "fixed");
    let kinds =
        |db: &Path| -> Vec<String> { watch_events(db).into_iter().map(|(kind, _)| kind).collect() };
    let attention = |db: &Path| {
        runtime::status(db).unwrap()["attention"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["kind"] == "ci_jobs_missing")
            .cloned()
    };

    runs(&gh, &[(1, "failure", &red_sha)]);
    red(&gh, 1, &["runtime_claim::fails"], &[]);
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(kinds(&db), ["ci_checked", "ci_turned_red"]);

    // A docs-only push: the Rust jobs skipped, the run a success. Nothing
    // is recorded and the list stays.
    runs(&gh, &[(1, "failure", &red_sha), (2, "success", &docs)]);
    jobs(
        &gh,
        2,
        &[
            ("docs", "success"),
            ("rust", "skipped"),
            ("linux", "skipped"),
        ],
    );
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(kinds(&db), ["ci_checked", "ci_turned_red"]);
    let list = runtime::ci_failures(&db, None).unwrap();
    assert_eq!(list["state"], "red");
    assert_eq!(list["failures"][0]["name"], "dagq::it runtime_claim::fails");
    let calls = fs::read_to_string(gh.with_file_name("calls")).unwrap();
    assert!(
        calls.contains("run view 2 --attempt 1 --json jobs"),
        "{calls}"
    );

    // A run without the job `linux`: told once, an attention for the
    // inbox, and still not green.
    runs(
        &gh,
        &[
            (1, "failure", &red_sha),
            (2, "success", &docs),
            (3, "success", &renamed),
        ],
    );
    jobs(
        &gh,
        3,
        &[("rust", "success"), ("linux (renamed)", "success")],
    );
    for _ in 0..2 {
        supervise_with(&db, &repo, &backend, &options).unwrap();
    }
    assert_eq!(
        kinds(&db),
        ["ci_checked", "ci_turned_red", "ci_jobs_missing"]
    );
    let told = &watch_events(&db)[2].1;
    assert_eq!(told["jobs"], json!(["linux"]));
    assert_eq!(told["run_id"], 3);
    let fix = attention(&db).unwrap();
    assert_eq!(fix["next"], "fix dagq.toml");
    assert_eq!(fix["status"], "missing");
    assert_eq!(runtime::ci_failures(&db, None).unwrap()["state"], "red");

    // Jobs that cannot be read stop the check there (the later runs wait
    // too) and are read again at each check; at the limit the run is
    // given up, each of two in a row on its own count.
    runs(
        &gh,
        &[
            (1, "failure", &red_sha),
            (2, "success", &docs),
            (3, "success", &renamed),
            (4, "success", &unread),
            (5, "success", &unread_too),
            (6, "success", &fixed),
        ],
    );
    fs::write(gh.with_file_name("jobs-4.fail"), "").unwrap();
    fs::write(gh.with_file_name("jobs-5.fail"), "").unwrap();
    jobs(&gh, 6, &[("rust", "success"), ("linux", "success")]);
    let (options, ahead) = supervise_options_ahead(4, false);
    let mut options = watching(options, &gh);
    if let Some(watch) = &mut options.ci_watch {
        watch.interval = Some(Duration::from_secs(600));
    }
    let stop = options.stop.clone();
    let reads_of = |id: i64| {
        fs::read_to_string(gh.with_file_name("calls"))
            .unwrap()
            .lines()
            .filter(|line| line.starts_with(&format!("run view {id} ")))
            .count()
    };
    let wait_for = |what: &str, done: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "waited for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let told = |db: &Path, id: i64| {
        watch_events(db)
            .iter()
            .filter(|(kind, payload)| kind == "ci_check_failed" && payload["run_id"] == id)
            .count()
    };
    std::thread::scope(|scope| {
        let supervisor = scope.spawn(|| supervise_with(&db, &repo, &backend, &options));
        let _stop = StopOnDrop(stop.clone());
        // (reads of run 4, reads of run 5) after each check: 4 is given up
        // at the third, 5 at the fifth, and run 6 is recorded then.
        for (check, reads) in [(1, (1, 0)), (2, (2, 0)), (3, (3, 1)), (4, (4, 2))] {
            if check > 1 {
                ahead.by(Duration::from_secs(600));
            }
            wait_for("a check", &|| (reads_of(4), reads_of(5)) == reads);
            let given_up = usize::from(check >= 3);
            wait_for("run 4 told", &|| told(&db, 4) == given_up);
            assert_eq!(kinds(&db).len(), 3 + given_up);
        }
        ahead.by(Duration::from_secs(600));
        wait_for("run 6", &|| kinds(&db).len() == 7);
        assert_eq!((told(&db, 4), told(&db, 5)), (1, 1));
        // The newest run's jobs stay unreadable: given up at the limit, it
        // is given up again at once at the next check, told once.
        let mut list: Vec<Value> =
            serde_json::from_str(&fs::read_to_string(gh.with_file_name("runs.json")).unwrap())
                .unwrap();
        let mut seven = list[5].clone();
        seven["databaseId"] = json!(7);
        seven["createdAt"] = json!("2026-10-06T00:07:00Z");
        list.push(seven);
        fs::write(gh.with_file_name("runs.json"), json!(list).to_string()).unwrap();
        fs::write(gh.with_file_name("jobs-7.fail"), "").unwrap();
        for read in 1..=4 {
            ahead.by(Duration::from_secs(600));
            wait_for("a read of run 7", &|| reads_of(7) == read);
        }
        wait_for("run 7 given up", &|| told(&db, 7) == 1);
        assert_eq!(kinds(&db).len(), 8);
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _waiting =
            crate::common::within(crate::common::STEP_LIMIT, "the supervisor at work to stop");
        supervisor.join().unwrap().unwrap();
    });
    let events = watch_events(&db);
    let kinds: Vec<&str> = events.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "ci_checked",
            "ci_turned_red",
            "ci_jobs_missing",
            "ci_check_failed",
            "ci_check_failed",
            "ci_checked",
            "ci_turned_green",
            "ci_check_failed"
        ]
    );
    assert_eq!(events[3].1["run_id"], 4);
    assert_eq!(events[3].1["failures"], 3);
    assert_eq!(events[4].1["run_id"], 5);
    let checked = &events[5].1;
    assert_eq!(checked["run_id"], 6);
    assert_eq!(checked["skipped_runs"], json!([2, 3, 4, 5]));
    let reasons: Vec<&str> = checked["undecided"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["reason"].as_str().unwrap())
        .collect();
    assert_eq!(
        reasons,
        [
            "jobs_not_passed",
            "jobs_missing",
            "jobs_unreadable",
            "jobs_unreadable"
        ]
    );
    assert_eq!(
        checked["removed"],
        json!([{"name": "dagq::it runtime_claim::fails", "reason": "green"}])
    );
    assert_eq!(events[6].1["red_since"]["run_id"], 1);
    // The green run with every named job ends the attention.
    assert!(attention(&db).is_none());
    assert_eq!(
        runtime::ci_failures(&db, None).unwrap()["failures"],
        json!([])
    );
    backend.join();
}
