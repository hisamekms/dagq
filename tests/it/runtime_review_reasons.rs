//! Runtime tests: the review job's reasons carry reason codes that
//! `review_finished` records, a person's answer to the concern records
//! what it made of the findings (`review_outcome`), and `stats` tallies
//! them per code (ADR-t947-1).
use crate::runtime_support;

use runtime_support::*;

/// A review job that prints `reasons` as they are: items with codes, or
/// texts in the form before the codes.
fn coded(decision: &str, reasons: Value, summary: &str) -> String {
    let json = json!({"verdict": decision, "reasons": reasons, "summary": summary});
    format!("printf '%s\\n' '{json}'")
}

/// A concern whose first reason carries two codes and whose second is a
/// bare text, as a job before the codes printed it.
fn concern() -> String {
    coded(
        "concern",
        json!([
            {"text": "contradicts the decision record", "codes": ["adr_conflict", "docs_drift"]},
            "a note in the old form",
        ]),
        "adr",
    )
}

/// The review of the first run returns a coded concern; the person answers
/// `answer`, and the supervisor applies it.
fn answered(answer: &str, then: &[String]) -> (Fixture, PathBuf, PathBuf) {
    let (dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let mut verdicts = vec![concern()];
    verdicts.extend_from_slice(then);
    let reviewer = TestReviewer::new(&verdicts);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    // The job was told the codes and their definitions.
    let prompt = &reviewer.prompts()[0];
    assert!(prompt.contains("- adr_conflict: "), "{prompt}");
    assert!(prompt.contains("- other: "), "{prompt}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let finished = payloads(&detail, "review_finished");
    assert_eq!(
        finished[0]["reasons"],
        json!(["contradicts the decision record", "a note in the old form"])
    );
    assert_eq!(
        finished[0]["reason_codes"],
        json!([["adr_conflict", "docs_drift"], ["unlabeled"]])
    );
    assert_eq!(finished[0]["primary_code"], "adr_conflict");
    assert!(payloads(&detail, "review_outcome").is_empty());
    let ask = queue.asks(Default::default()).unwrap()[0].clone();
    queue.answer(ask.id, answer).unwrap();
    if answer == "send_back" {
        backend.resume_script_for(
            1,
            "await_message; printf 'narrowed\\n' > change.txt; unlocked git commit -q -am narrowed; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
        );
    }
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let outcomes = payloads(&detail, "review_outcome");
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(outcomes[0]["ask_id"], ask.id.as_i64());
    assert_eq!(outcomes[0]["attempt"], 1);
    assert_eq!(
        outcomes[0]["reason_codes"],
        json!([["adr_conflict", "docs_drift"], ["unlabeled"]])
    );
    assert_eq!(outcomes[0]["primary_code"], "adr_conflict");
    (dir, repo, db)
}

/// `send_back` carries the verdict's codes on as `deviation_rejected`;
/// the review of the resumed run passes with no codes, and `stats` counts
/// the sent-back run, its codes, its answer and its resume per code.
#[test]
fn a_concern_sent_back_records_its_codes_and_outcome_and_stats_tally_them() {
    let (_dir, _repo, db) = answered("send_back", &[verdict("pass", &[], "fixed")]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert_eq!(
        payloads(&detail, "review_outcome")[0]["outcome"],
        "deviation_rejected"
    );
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 2);
    assert_eq!(finished[1]["reason_codes"], json!([]));
    assert_eq!(finished[1]["primary_code"], Value::Null);

    let stats = runtime::stats(&db, &Default::default()).unwrap();
    let review = &stats["review_reasons"]["review"];
    assert_eq!(review["verdicts"], 2);
    assert_eq!(review["reviewed"], 1);
    assert_eq!(review["sent_back"], 1);
    assert_eq!(review["rate"], 1.0);
    let adr = &review["by_code"]["adr_conflict"];
    assert_eq!(adr["verdicts"], 1);
    assert_eq!(adr["concern"], 1);
    assert_eq!(adr["subjects"], 1);
    assert_eq!(
        adr["outcomes"],
        json!({"deviation_accepted": 0, "deviation_rejected": 1, "canceled": 0})
    );
    assert_eq!(adr["concern_wait_secs"]["count"], 1);
    assert_eq!(adr["send_back_resume_secs"]["count"], 1);
    assert_eq!(
        review["codes"],
        json!({"adr_conflict": 1, "docs_drift": 1, "unlabeled": 1})
    );
    // The fixture's task has no kind.
    assert_eq!(review["by_kind"][0]["kind"], Value::Null);
    assert_eq!(review["by_kind"][0]["by_code"], json!({"adr_conflict": 1}));
    let run = &stats["runs"][0]["review_reasons"];
    assert_eq!(run.as_array().unwrap().len(), 1);
    assert_eq!(run[0]["verdict"], "concern");
    assert_eq!(run[0]["outcome"], "deviation_rejected");
}

/// `land` records `deviation_accepted` once, and the run lands.
#[test]
fn a_concern_landed_records_the_departure_accepted() {
    let (_dir, _repo, db) = answered("land", &[]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert_eq!(
        payloads(&detail, "review_outcome")[0]["outcome"],
        "deviation_accepted"
    );
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(
        stats["review_reasons"]["review"]["by_code"]["adr_conflict"]["outcomes"]["deviation_accepted"],
        1
    );
}

/// `cancel` records `canceled`, and the task is canceled.
#[test]
fn a_concern_canceled_records_the_cancel() {
    let (_dir, _repo, db) = answered("cancel", &[]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    assert_eq!(
        payloads(&detail, "review_outcome")[0]["outcome"],
        "canceled"
    );
}
