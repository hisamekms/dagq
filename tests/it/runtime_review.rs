//! The real process boundary of a review job stopped at its timeout.
use crate::runtime_support;
use runtime_support::*;

/// The headless worker's turns: its first commits, and each later one (a
/// revise request) appends `fix <n>`, commits and rewrites the receipt, up
/// to `revises` times.
pub(crate) fn revising_agent(revises: usize) -> String {
    format!(
        "if [ \"$TURN\" -eq 1 ]; then commit work; \
         elif [ \"$TURN\" -le {} ]; then n=$((TURN - 1)); \
           printf 'fix %s\\n' \"$n\" >> change.txt; git commit -q -am \"fix $n\"; \
         fi; receipt \"$(git rev-parse HEAD)\"",
        revises + 1
    )
}

/// A review job stopped at its timeout runs once more like any job that
/// gave no answer (ADR-t1895-1 decision 5); stopped again, the review
/// fails to the person with one `approve_landing` ask. Only the agent job
/// has the 1 second: the reviewer's timeout is also the program jobs' unless
/// `dagq.toml` says otherwise, and a program stopped at it would make this
/// review the retry of its programs, whose jobs do not run once more.
#[test]
fn a_timed_out_review_job_runs_once_more_then_fails() {
    let (_dir, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        "[review.jobs]\nprogram_timeout_secs = 60\n",
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-q", "-m", "review jobs"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let mut reviewer = TestReviewer::new(&["sleep 30".to_owned()]);
    reviewer.timeout = Duration::from_secs(1);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(reviewer.prompts().len(), 2);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert!(payloads(&detail, "review_retried").is_empty(), "{kinds:?}");
    let outcomes: Vec<_> = payloads(&detail, "review_job_finished")
        .iter()
        .map(|end| (end["outcome"].clone(), end["runs_again"].clone()))
        .collect();
    assert_eq!(
        outcomes,
        [
            (json!("timed_out"), json!(true)),
            (json!("timed_out"), json!(false))
        ]
    );
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["attempt"], 1);
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("did not finish within 1 seconds"),
        "{failed:?}"
    );
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::ApproveLanding);
    assert_eq!(failed[0]["ask_id"], json!(asks[0].id));
}
