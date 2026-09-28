//! Plan review's reasons carry reason codes that `plan_review_finished`
//! records, a person's answer to the `approve_plan` ask of a concern
//! records what it made of the findings (`plan_review_outcome`), and
//! `stats` and `kpi` tally them per code (ADR-t947-1).

use crate::plan_review::{PlanWorkspace, StubReviewer, add, events, fixture, submit, supervise};
use dagq::{
    domain::{Priority, TaskId},
    infrastructure::sqlite::SqliteQueue,
    runtime,
};
use serde_json::{Value, json};

#[test]
fn a_concerns_codes_and_the_answers_outcomes_are_recorded_and_tallied() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let tasks = ["kept", "dropped", "returned"]
        .map(|title| add(&mut queue, title, &[blocker], Priority::Normal));
    for task in tasks {
        submit(&mut queue, &[task], None);
    }
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "concern",
        "reasons": [
            {"text": "repeats task 1", "codes": ["task_overlap"]},
            {"text": "odd", "codes": ["someday_code"]},
            "a note in the old form",
        ],
        "summary": "maybe done",
    })]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    let prompt = &reviewer.prompts()[0];
    assert!(prompt.contains("- missing_dependency: "), "{prompt}");
    for task in tasks {
        let finished = &events(&mut queue, task, "plan_review_finished")[0];
        assert_eq!(
            finished["reasons"],
            json!(["repeats task 1", "odd", "a note in the old form"])
        );
        // A code outside the list is kept as printed.
        assert_eq!(
            finished["reason_codes"],
            json!([["task_overlap"], ["someday_code"], ["unlabeled"]])
        );
        assert_eq!(finished["primary_code"], "task_overlap");
    }
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 3);
    for (ask, answer) in asks.iter().zip(["ready", "cancel", "send_back: split it"]) {
        queue.answer(ask.id, answer).unwrap();
    }
    supervise(&fx, &backend, &reviewer);
    for (task, outcome) in
        tasks
            .iter()
            .zip(["deviation_accepted", "canceled", "deviation_rejected"])
    {
        let outcomes = events(&mut queue, *task, "plan_review_outcome");
        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        assert_eq!(outcomes[0]["outcome"], outcome);
        assert_eq!(outcomes[0]["primary_code"], "task_overlap");
        assert_eq!(
            outcomes[0]["reason_codes"],
            json!([["task_overlap"], ["someday_code"], ["unlabeled"]])
        );
        let finished = &events(&mut queue, *task, "plan_review_finished")[0];
        assert_eq!(outcomes[0]["plan_review_id"], finished["plan_review_id"]);
    }

    let stats = runtime::stats(&fx.db, &Default::default()).unwrap();
    let plan = &stats["review_reasons"]["plan_review"];
    assert_eq!(plan["reviewed"], 3);
    assert_eq!(plan["sent_back"], 3);
    let overlap = &plan["by_code"]["task_overlap"];
    assert_eq!(overlap["concern"], 3);
    assert_eq!(overlap["subjects"], 3);
    assert_eq!(
        overlap["outcomes"],
        json!({"deviation_accepted": 1, "deviation_rejected": 1, "canceled": 1})
    );
    assert_eq!(overlap["concern_wait_secs"]["count"], 3);
    assert_eq!(
        plan["codes"],
        json!({"someday_code": 3, "task_overlap": 3, "unlabeled": 3})
    );
    assert_eq!(stats["review_reasons"]["review"]["reviewed"], 0);

    // `kpi` reads them per code in the day's window.
    let kpi = crate::common::cli::ok(&fx.db, &["kpi", "--last", "1"]);
    let today = kpi["periods"].as_array().unwrap().last().unwrap().clone();
    // Concerns, not revises: 0 of the 3 reviews.
    assert_eq!(
        today["kpis"]["plan.revise_rate"]["code=task_overlap"],
        json!({"n": 3, "value": 0.0}),
        "{today}"
    );
    assert_eq!(
        today["details"]["review_reasons"]["plan_review"]["sent_back"],
        3
    );
    assert_eq!(
        today["kpis"]["review.sendback_rate"]["all"]["value"],
        Value::Null
    );
}
