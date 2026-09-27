//! The model and effort of the sessions other than the worker's (ADR-0079
//! decision 7): without `[roles.<role>]` every job starts as before (no
//! model or effort given); with one, its job is given them. Either way the
//! start event and the session's span record what it started with and
//! where that came from.
use crate::runtime_support;

use runtime_support::*;

/// A worker script that fails its first run without a commit and lands
/// every later one: a recovery job, then a review.
fn fails_once(dir: &Path) -> String {
    let mark = dir.join("failed-once");
    format!(
        "if [ ! -f '{mark}' ]; then : > '{mark}'; exit 7; fi; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
        mark = mark.display()
    )
}

/// The `launch` of each event of `kind` of the task's runs, in order.
fn launches(queue: &mut SqliteQueue, kind: &str) -> Vec<Value> {
    queue
        .show(TaskId::new(1))
        .unwrap()
        .events
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.payload["launch"].clone())
        .collect()
}

/// The `launch` of each span of `span_kind` the task's runs opened.
fn span_launches(queue: &mut SqliteQueue, span_kind: &str) -> Vec<Value> {
    queue
        .show(TaskId::new(1))
        .unwrap()
        .events
        .iter()
        .filter(|e| e.kind == "session_opened" && e.payload["kind"] == span_kind)
        .map(|e| e.payload["launch"].clone())
        .collect()
}

fn run_recovered_and_reviewed(repo: &Path, db: &Path) -> TestReviewer {
    let backend = TestWorkspace::new(db, false, &fails_once(db.parent().unwrap()));
    let reviewer =
        TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]).with_triages(&[repair(
            json!({"action": "retry"}),
            "the session died on its own",
        )]);
    let outcome = supervise_reviewed(db, repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    reviewer
}

/// No `[roles.*]`: the recovery job and the review are given no model or
/// effort, as before, and record that they started with the default.
#[test]
fn jobs_without_a_role_table_start_as_before() {
    let (_dir, repo, db) = fixture();
    let reviewer = run_recovered_and_reviewed(&repo, &db);
    assert_eq!(reviewer.models(), []);
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        launches(&mut queue, "triage_started"),
        [json!({"role": "recovery", "model": null, "effort": null, "source": "default"})]
    );
    assert_eq!(
        launches(&mut queue, "review_started"),
        [json!({"role": "review", "model": null, "effort": null, "source": "default"})]
    );
    assert_eq!(
        span_launches(&mut queue, "review"),
        launches(&mut queue, "review_started")
    );
    assert_eq!(
        span_launches(&mut queue, "triage"),
        launches(&mut queue, "triage_started")
    );
}

/// `[roles.review]` and `[roles.recovery]` of the main checkout's
/// `dagq.toml`: each job is given its role's model and effort (the default
/// for the key left out), and records them with `dagq.toml` as the source.
#[test]
fn jobs_with_a_role_table_are_given_its_model_and_effort() {
    let (_dir, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        "[roles.review]\neffort = \"high\"\n\n[roles.recovery]\nmodel = \"claude-sonnet-5\"\n",
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-m", "roles"]);
    let reviewer = run_recovered_and_reviewed(&repo, &db);
    assert_eq!(
        reviewer.models(),
        [
            ("claude-sonnet-5".to_owned(), "medium".to_owned()),
            ("claude-opus-5-5".to_owned(), "high".to_owned()),
        ]
    );
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        launches(&mut queue, "triage_started"),
        [
            json!({"role": "recovery", "model": "claude-sonnet-5", "effort": "medium", "source": "dagq.toml"})
        ]
    );
    let review = json!({"role": "review", "model": "claude-opus-5-5", "effort": "high", "source": "dagq.toml"});
    assert_eq!(
        launches(&mut queue, "review_started"),
        std::slice::from_ref(&review)
    );
    assert_eq!(span_launches(&mut queue, "review"), [review]);
}
