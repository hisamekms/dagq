//! A headless job that could not be started (task 1560): arguments or an
//! environment past the system's limit (`E2BIG`, os error 7) are the job's
//! own input, so the job fails alone and no provider is held; a provider
//! whose executable is not there is held as before.

use crate::goal_review_codex::{codex_home, queue_events, roles, stub_codex, stub_lines};
use crate::plan_review::{
    Fixture, MISSING, PlanWorkspace, StubReviewer, TOO_LONG, add, events, fixture, options, status,
    submit, supervise_with,
};

use dagq::{
    domain::{Priority, ProposalId, TaskId, TaskStatus},
    infrastructure::sqlite::SqliteQueue,
};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

/// Supervise once with `codex` as the Codex CLI, and no runtime planner.
fn supervise(fx: &Fixture, reviewer: &StubReviewer, codex: &Path) {
    let mut options = options(0, Duration::from_secs(3600));
    options.codex = codex.to_owned();
    options.codex_home = Some(codex_home(fx));
    supervise_with(fx, &PlanWorkspace::default(), reviewer, &options);
}

fn pass() -> Value {
    json!({"verdict": "pass", "reasons": [], "summary": "pass it"})
}

/// A submitted proposal of one new task, waiting for the draft blocker.
fn proposal(fx: &Fixture, title: &str) -> (TaskId, ProposalId) {
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, title, &[TaskId::new(1)], Priority::Normal);
    (task, submit(&mut queue, &[task], None))
}

/// The plan review of one proposal fails to start with `E2BIG`: that job
/// fails as any failed job does, no provider is held, and the next
/// proposal's job starts on the same provider and passes. Supervised
/// again, the failed proposal is not started again, so no hold comes back.
#[test]
fn a_job_past_the_argument_limit_fails_alone_and_holds_no_provider() {
    let fx = fixture();
    roles(&fx, "[roles.plan_review]\nprovider = \"claude\"\n");
    // Claude's role, with a Codex the job could move to: a provider that
    // cannot be used would be held and the job moved.
    let codex = stub_codex(&fx, "ok", &pass());
    let (too_long, _) = proposal(&fx, "too long");
    let (next, _) = proposal(&fx, "next");
    let claude = StubReviewer::new(&[json!(TOO_LONG), pass()]);
    supervise(&fx, &claude, &codex);
    supervise(&fx, &claude, &codex);

    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let failed = events(&mut queue, too_long, "plan_review_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    let error = failed[0]["error"].as_str().unwrap();
    assert!(error.contains("os error 7"), "{error}");
    assert!(failed[0]["provider_unusable"].is_null(), "{failed:?}");
    assert_eq!(status(&mut queue, too_long), TaskStatus::Submitted);
    assert!(queue_events(&fx, "provider_held").is_empty());
    // The other proposal's job ran on Claude all the same.
    assert_eq!(status(&mut queue, next), TaskStatus::Ready);
    let started = events(&mut queue, next, "plan_review_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["launch"]["provider"], "claude");
    assert!(
        started[0]["launch"]["switched_from"].is_null(),
        "{started:?}"
    );
    assert_eq!(claude.prompts().len(), 2, "each proposal started once");
    assert!(
        stub_lines(&fx, "codex-args.txt").is_empty(),
        "nothing moved"
    );
}

/// A provider whose executable is not there when its job starts is held
/// (`executable_missing`) and the job moves to the other provider, as
/// before task 1560.
#[test]
fn a_missing_executable_still_holds_its_provider() {
    let fx = fixture();
    roles(&fx, "[roles.plan_review]\nprovider = \"claude\"\n");
    let codex = stub_codex(&fx, "ok", &pass());
    let (task, _) = proposal(&fx, "moved");
    let claude = StubReviewer::new(&[json!(MISSING), pass()]);
    supervise(&fx, &claude, &codex);
    supervise(&fx, &claude, &codex);
    let held = queue_events(&fx, "provider_held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(
        (&held[0]["provider"], &held[0]["reason"]),
        (&json!("claude"), &json!("executable_missing"))
    );
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let started = events(&mut queue, task, "plan_review_started");
    assert_eq!(started.len(), 2, "{started:?}");
    assert_eq!(started[1]["launch"]["provider"], "codex");
    assert_eq!(started[1]["launch"]["switch_reason"], "executable_missing");
    assert_eq!(status(&mut queue, task), TaskStatus::Ready);
}
