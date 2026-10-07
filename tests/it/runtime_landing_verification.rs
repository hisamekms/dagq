//! Runtime tests: `[landing_verification]` of `dagq.toml` makes
//! `integrate` run the repository's command in place of the task's
//! commands it names, with the landing's base, the known CI failures and
//! whether the run fixes one in its environment (ADR-t1925-1 decision 4);
//! the host's and the flaky tests' retries treat it as any other command.
use crate::runtime_support;

use dagq::domain::{FindingId, FindingStatus, FindingTarget, NewFinding, ci_watch::CiCheckRecord};
use runtime_support::*;

/// A run awaiting integration, its repository, and the queue.
fn awaiting() -> (Fixture, PathBuf, PathBuf) {
    let (dir, db, detail) = run_agent(
        "echo a > a.txt && git add a.txt && git commit -q -m a; receipt \"$(git rev-parse HEAD)\"",
    );
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    let repo = Path::new(&db).parent().unwrap().join("repo's directory");
    (dir, db, repo)
}

fn set_commands(db: &Path, commands: Value) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands=?1 WHERE id=1",
            [commands.to_string()],
        )
        .unwrap();
}

fn show(db: &Path) -> dagq::domain::TaskDetail {
    SqliteQueue::open(db).unwrap().show(TaskId::new(1)).unwrap()
}

/// Commit `[landing_verification]` with `command` replacing the commands
/// that contain `coverage-gate` to main.
fn configure(repo: &Path, command: &str) {
    fs::write(
        repo.join("dagq.toml"),
        format!(
            "[landing_verification]\nreplaces = ['coverage-gate']\ncommand = {}\n",
            json!(command)
        ),
    )
    .unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-q", "-m", "landing verification"]);
}

/// Record a red CI run adding `name`, with a `ci_failure` finding for it,
/// as the watch does, after the `ci_checked` event `previous`; the
/// finding's ID and the event's.
fn red(db: &Path, previous: Option<i64>, run_id: i64, name: &str) -> (FindingId, i64) {
    let recorded = SqliteQueue::open(db)
        .unwrap()
        .record_ci_check(CiCheckRecord {
            previous,
            checked: json!({
                "run_id": run_id,
                "sha": format!("{run_id:040}"),
                "url": format!("https://ci.example/{run_id}"),
                "state": "red",
                "conclusion": "failure",
                "added": [name],
                "removed": [],
            }),
            turned_red: None,
            turned_green: None,
            finding: Some(NewFinding {
                kind: "ci_failure".into(),
                target: FindingTarget::Queue,
                subject: name.into(),
                summary: format!("{name} fails on main"),
                detail: None,
                impact: None,
                evidence: Vec::new(),
                propose: None,
                by: "runtime".into(),
            }),
            resolve: Vec::new(),
        })
        .unwrap()
        .unwrap();
    (recorded.finding.unwrap(), recorded.event.as_i64())
}

/// The task's command that names the coverage gate is replaced by the
/// repository's; the others run as registered and without the landing's
/// variables. The replacing command gets the main it was rebased onto,
/// the known failures with the items of the finding the task fixes kept
/// apart, and that it fixes one; its event and log name the command it
/// replaced, and the task's registered commands stay.
#[test]
fn the_repository_command_runs_in_place_of_the_coverage_gate_with_the_landing_env() {
    let (_dir, db, repo) = awaiting();
    configure(
        &repo,
        r#"echo "base=$DAGQ_LANDING_BASE"; echo "fix=$DAGQ_CI_FIX_RUN"; echo "list=$DAGQ_CI_KNOWN_FAILURES"; cat "$DAGQ_CI_KNOWN_FAILURES""#,
    );
    let (fixed, event) = red(&db, None, 3, "dagq::it runtime_x::fixed");
    red(&db, Some(event), 4, "dagq::it runtime_x::other");
    SqliteQueue::open(&db)
        .unwrap()
        .set_finding_status_covered(
            fixed,
            FindingStatus::Dismissed,
            "task 1 fixes it",
            "planner",
            Some(TaskId::new(1)),
        )
        .unwrap();
    let registered = json!([
        r#"test -z "$DAGQ_LANDING_BASE""#,
        "cargo coverage-gate --workspace",
        r#"test -z "$DAGQ_CI_KNOWN_FAILURES""#,
    ]);
    set_commands(&db, registered.clone());
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let detail = show(&db);
    assert_eq!(json!(detail.task.verification_commands()), registered);
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 3, "{verifications:?}");
    assert_eq!(verifications[0]["command"], registered[0]);
    assert_eq!(verifications[0]["replaces"], Value::Null);
    assert_eq!(verifications[2]["replaces"], Value::Null);
    let replaced = verifications[1];
    assert!(
        replaced["command"]
            .as_str()
            .unwrap()
            .starts_with("echo \"base="),
        "{replaced}"
    );
    assert_eq!(
        replaced["replaces"],
        json!(["cargo coverage-gate --workspace"])
    );
    assert_eq!(replaced["exit_code"], 0);
    let log = fs::read_to_string(replaced["log_path"].as_str().unwrap()).unwrap();
    assert!(
        log.starts_with(
            "# dagq: [landing_verification] of dagq.toml runs this command in place of the task's \"cargo coverage-gate --workspace\""
        ),
        "{log}"
    );
    let main = payloads(&detail, "integration_rebased")[0]["main"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(log.contains(&format!("base={main}\n")), "{log}");
    assert!(log.contains("fix=1\n"), "{log}");
    let run_dir = PathBuf::from(detail.runs[0].run_dir().unwrap());
    let list = run_dir.join("ci-known-failures.json");
    assert!(log.contains(&format!("list={}\n", list.display())), "{log}");
    let failures: Value = serde_json::from_str(&fs::read_to_string(&list).unwrap()).unwrap();
    let names = |key: &str| -> Vec<&str> {
        failures[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["name"].as_str().unwrap())
            .collect()
    };
    assert_eq!(
        names("failures"),
        ["dagq::it runtime_x::other"],
        "{failures}"
    );
    assert_eq!(
        names("kept_for_task"),
        ["dagq::it runtime_x::fixed"],
        "{failures}"
    );
}

/// A run of a task that fixes no CI failure is told so, and gets every
/// known failure to leave out, none kept for it.
#[test]
fn a_run_that_fixes_no_ci_failure_gets_the_whole_list() {
    let (_dir, db, repo) = awaiting();
    configure(&repo, r#"echo "fix=$DAGQ_CI_FIX_RUN""#);
    red(&db, None, 3, "dagq::it runtime_x::broken");
    set_commands(&db, json!(["cargo coverage-gate"]));
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let detail = show(&db);
    let verifications = integration_verifications(&detail);
    let log = fs::read_to_string(verifications[0]["log_path"].as_str().unwrap()).unwrap();
    assert!(log.contains("fix=0\n"), "{log}");
    let run_dir = PathBuf::from(detail.runs[0].run_dir().unwrap());
    let failures: Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("ci-known-failures.json")).unwrap())
            .unwrap();
    assert_eq!(
        failures["failures"][0]["name"], "dagq::it runtime_x::broken",
        "{failures}"
    );
    assert_eq!(failures["failures"].as_array().unwrap().len(), 1);
    assert_eq!(failures["kept_for_task"], json!([]), "{failures}");
}

/// The replacing command that failed on a full disk is retried once in the
/// same attempt like a task's (ADR-t639-1), its retry naming what it
/// replaced too.
#[test]
fn a_host_failure_of_the_replacing_command_is_retried_once() {
    let (dir, db, repo) = awaiting();
    let marker = dir.path().join("full once");
    configure(
        &repo,
        &format!(
            "if [ -f {0} ]; then exit 0; fi; touch {0}; echo 'error: failed to write: No space left on device (os error 28)'; exit 1",
            shell_path(&marker)
        ),
    );
    set_commands(&db, json!(["cargo coverage-gate"]));
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let detail = show(&db);
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 2, "{verifications:?}");
    assert_eq!(verifications[0]["failure"]["class"], "disk_full");
    assert_eq!(verifications[1]["retry"], true);
    assert_eq!(verifications[1]["exit_code"], 0);
    for verification in verifications {
        assert_eq!(verification["replaces"], json!(["cargo coverage-gate"]));
    }
    assert!(payloads(&detail, "integration_deferred").is_empty());
}

/// The replacing command whose only failed tests passed on nextest's retry
/// is run once more with flakes allowed (ADR-t768-1), and the run lands.
#[test]
fn a_flaky_only_failure_of_the_replacing_command_is_verified_once_more() {
    let (dir, db, repo) = awaiting();
    let output = dir.path().join("flaky.log");
    fs::write(
        &output,
        [
            "  TRY 1 FAIL [   0.010s] (───) dagq::it runtime_x::flaky",
            "  TRY 2 PASS [   0.007s] (1/1) dagq::it runtime_x::flaky",
            "────────────",
            "     Summary [   0.017s] 1 test run: 1 passed, 0 skipped",
            " FLKY-FL 2/2 [   0.007s] (1/1) dagq::it runtime_x::flaky",
            "error: test run failed",
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    configure(
        &repo,
        &format!(
            "if [ \"$NEXTEST_FLAKY_RESULT\" = pass ]; then exit 0; fi; cat {}; exit 100",
            shell_path(&output)
        ),
    );
    set_commands(&db, json!(["true", "cargo coverage-gate"]));
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let detail = show(&db);
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 4, "{verifications:?}");
    assert_eq!(verifications[1]["failure"]["class"], "flaky");
    assert_eq!(verifications[3]["attempt"], 2);
    assert_eq!(verifications[3]["exit_code"], 0);
    assert_eq!(verifications[3]["replaces"], json!(["cargo coverage-gate"]));
    let retried = payloads(&detail, "integration_retried");
    assert_eq!(retried.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(retried[0]["index"], 2);
    assert!(payloads(&detail, "integration_deferred").is_empty());
}
