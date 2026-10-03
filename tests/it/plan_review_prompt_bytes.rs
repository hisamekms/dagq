//! The plan review records what its prompt took, in all and section by
//! section, on the event that ends it: `prompt_bytes` on
//! `plan_review_finished` and on `plan_review_failed` (task 1561,
//! ADR-t1566-1 decision 6).

use crate::plan_review::{PlanWorkspace, StubReviewer, add, events, fixture, submit, supervise};
use dagq::{
    domain::{Priority, TaskId},
    infrastructure::{location::plan_reviews_dir, sqlite::SqliteQueue},
};
use serde_json::{Value, json};
use std::fs;

/// `prompt_bytes` adds up and matches the prompt the job was given.
fn assert_prompt_bytes(recorded: &Value, prompt: &str) {
    let bytes = &recorded["prompt_bytes"];
    assert_eq!(bytes["total"], json!(prompt.len()), "{recorded}");
    assert_eq!(bytes["limit"], 400_000);
    let sections = bytes["sections"].as_object().unwrap();
    for section in [
        "instructions",
        "tasks",
        "expected_files",
        "goals",
        "lint",
        "other_proposals",
        "summaries",
        "full_text",
        "precedents",
        "hotspots",
        "candidates",
        "language",
    ] {
        assert!(sections.contains_key(section), "{section} in {bytes}");
    }
    let sum: u64 = sections.values().map(|n| n.as_u64().unwrap()).sum();
    assert_eq!(json!(sum), bytes["total"]);
    assert!(sections["tasks"].as_u64().unwrap() > 0);
    assert_eq!(bytes["over_limit"], Value::Null);
}

#[test]
fn a_plan_review_records_its_prompts_bytes_when_it_ends() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "measured", &[TaskId::new(1)], Priority::Normal);
    submit(&mut queue, &[task], None);
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "pass", "reasons": [], "summary": "sound",
    })]);
    supervise(&fx, &PlanWorkspace::default(), &reviewer);
    let prompt = &reviewer.prompts()[0];
    let started = &events(&mut queue, task, "plan_review_started")[0];
    let kept = fs::read_to_string(
        plan_reviews_dir(&fx.db)
            .join(started["plan_review_id"].to_string())
            .join("prompt.txt"),
    )
    .unwrap();
    assert_eq!(&kept, prompt);
    assert_prompt_bytes(&events(&mut queue, task, "plan_review_finished")[0], prompt);
}

#[test]
fn a_failed_plan_review_records_its_prompts_bytes() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "unlucky", &[TaskId::new(1)], Priority::Normal);
    submit(&mut queue, &[task], None);
    let reviewer = StubReviewer::failing();
    supervise(&fx, &PlanWorkspace::default(), &reviewer);
    let prompt = &reviewer.prompts()[0];
    assert_prompt_bytes(&events(&mut queue, task, "plan_review_failed")[0], prompt);
}
