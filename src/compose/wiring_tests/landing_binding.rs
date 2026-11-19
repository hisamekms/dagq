//! The landing decision reaches each authorization path's events, asks,
//! status and audit records. The fake injects changed blobs to exercise
//! reapproval, and the domain tests cover the comparison rules.

use serde_json::{Value, json};

use super::{
    agent::{Script, succeeded, verdict},
    harness::Wiring,
};
use crate::{
    application::TaskReads,
    domain::{AskKind, RunStatus, TaskDetail, TaskId, TaskStatus},
};

/// The file every run of these tests changes.
const FILE: &str = "change.txt";

/// The payloads of `detail`'s events of `kind`, oldest first.
fn payloads<'a>(detail: &'a TaskDetail, kind: &str) -> Vec<&'a Value> {
    detail
        .events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| &event.payload)
        .collect()
}

fn kinds(detail: &TaskDetail) -> Vec<&str> {
    detail.events.iter().map(|e| e.kind.as_str()).collect()
}

/// The wiring `name` whose run changes [`FILE`] and whose reviews say
/// `verdicts`, and its ready task.
fn wired(name: &str, verdicts: Vec<Value>) -> (Wiring, TaskId) {
    let wiring = Wiring::new(
        name,
        Script {
            receipts: vec![succeeded; 3],
            verdicts,
        },
    );
    // No program review runs: the diff of the test's repository is empty.
    const CONFIG: &str = "[review.programs.\"builtin:secrets\"]\nenabled = false\n";
    wiring
        .git
        .commit_files(&[("dagq.toml", CONFIG), (FILE, "one\n")], &[FILE]);
    wiring.verifier.set_config(CONFIG);
    let task = wiring.ready_task(&["true"]);
    (wiring, task)
}

/// A concern whose job recommends landing with high confidence: the
/// runtime lands it (ADR-t451-1 decision 3).
fn ai_land() -> Value {
    json!({"verdict": "concern", "reasons": ["a judgment"], "summary": "lands",
           "recommendation": "land", "confidence": "high"})
}

/// A concern that asks a person.
fn concern() -> Value {
    json!({"verdict": "concern", "reasons": ["a judgment"], "summary": "unsure"})
}

/// The set [`FILE`] at `blob`, as a record or a payload holds it.
fn set(blob: &str) -> Value {
    json!({FILE: {"blob": blob}})
}

/// The audit records of `wiring` of `action`.
fn audited(wiring: &Wiring, action: &str) -> Vec<Value> {
    wiring
        .audit_records()
        .into_iter()
        .filter(|record| record["action"] == action)
        .collect()
}

/// `task`'s run landed.
fn assert_landed(wiring: &Wiring, task: TaskId) -> TaskDetail {
    let detail = wiring.queue().show(task).unwrap();
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        kinds(&detail)
    );
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    detail
}

/// Each comparison of `detail`: its path and whether it matched.
fn comparisons(detail: &TaskDetail) -> Vec<(Value, Value)> {
    payloads(detail, "integration_compared")
        .into_iter()
        .map(|p| (p["path"].clone(), p["matched"].clone()))
        .collect()
}

/// A passed run lands on the blobs its review judged: the review's
/// `landing.judged`, the comparison's `landing.compared` and the merge's
/// `landing.merged` hold the same set, each with a diff hash and a tree
/// hash; the merge names main before and after.
#[test]
fn a_passed_run_lands_on_the_blobs_its_review_judged_and_audits_each_step() {
    let (wiring, task) = wired(
        "a_passed_run_lands_on_the_blobs_its_review_judged_and_audits_each_step",
        vec![verdict("pass", "fine")],
    );
    wiring.supervise_once();
    let detail = assert_landed(&wiring, task);
    assert_eq!(comparisons(&detail), [(json!("review_pass"), json!(true))]);
    let judged = audited(&wiring, "landing.judged");
    let compared = audited(&wiring, "landing.compared");
    let merged = audited(&wiring, "landing.merged");
    assert_eq!((judged.len(), compared.len(), merged.len()), (1, 1, 1));
    for record in [&judged[0], &compared[0], &merged[0]] {
        assert_eq!(
            record["landing"]["approved_blobs"],
            set(&format!("blob-{FILE}")),
            "{record}"
        );
        assert!(record["landing"]["diff_hash"].is_string(), "{record}");
        assert!(record["landing"]["tree_hash"].is_string(), "{record}");
        assert_eq!(
            record["resource"],
            json!({"kind": "run", "id": detail.runs[0].id()})
        );
    }
    assert_eq!(judged[0]["change"]["after"]["path"], "review_pass");
    assert_eq!(judged[0]["sender_id"], "supervisor:control");
    assert_eq!(compared[0]["outcome"]["status"], "succeeded");
    let compared_change = &compared[0]["change"];
    assert_eq!(
        compared_change["after"]["own_change"],
        json!({"kind": "unchanged_base"})
    );
    assert!(compared_change["before"]["binding"]["run_change"]["before_blobs"][FILE].is_object());
    assert_eq!(
        compared_change["before"]["binding"]["run_change"],
        compared_change["after"]["binding"]["run_change"],
    );
    let landed = &payloads(&detail, "run_integrated")[0];
    assert_eq!(merged[0]["change"]["before"]["main"], landed["main_before"]);
    assert_eq!(merged[0]["change"]["after"]["main"], landed["commit"]);
}

/// A pass whose resulting blob changed without a main update does
/// not land: the comparison names the file, the run goes back awaiting
/// integration and is reviewed again on its current diff, whose pass lands
/// it.
#[test]
fn a_pass_whose_merged_blob_changed_is_reviewed_again_and_lands_on_the_new_pass() {
    let (wiring, task) = wired(
        "a_pass_whose_merged_blob_changed_is_reviewed_again_and_lands_on_the_new_pass",
        vec![verdict("pass", "fine"), verdict("pass", "still fine")],
    );
    let wiring = wiring.with_tick(std::time::Duration::from_millis(1));
    wiring.git.change_blob_at_next_rebase(FILE, "merged");
    wiring.supervise_once();
    wiring.supervise_once();
    let detail = assert_landed(&wiring, task);
    assert_eq!(
        comparisons(&detail),
        [
            (json!("review_pass"), json!(false)),
            (json!("review_pass"), json!(true))
        ]
    );
    let entered = payloads(&detail, "landing_queue_entered");
    let left = payloads(&detail, "landing_queue_left");
    assert_eq!((entered.len(), left.len()), (1, 1));
    assert_eq!(left[0]["entry_id"], entered[0]["entry_id"]);
    assert_eq!(left[0]["reason"], "landed");
    assert!(
        payloads(&detail, "landing_queue_waiting")
            .iter()
            .any(|wait| {
                wait["entry_id"] == entered[0]["entry_id"]
                    && wait["waiting_for"] == "awaiting_reapproval"
            })
    );
    let first = payloads(&detail, "integration_compared")[0];
    assert_eq!(first["differing"], json!([FILE]));
    assert_eq!(first["reapproval"], "review");
    // The rebase is recorded as its own change, with the binding of each
    // side: each head's tree hash and set.
    let rebase = payloads(&detail, "integration_rebased")[0];
    for (side, head) in [
        ("binding_before", "head_before"),
        ("binding_after", "head_after"),
    ] {
        assert_eq!(
            rebase[side]["tree_hash"],
            format!("tree of {}", rebase[head].as_str().unwrap()),
            "{rebase}"
        );
        assert!(rebase[side]["approved_blobs"][FILE].is_object(), "{rebase}");
        assert!(rebase[side]["diff_hash"].is_string(), "{rebase}");
    }
    assert_eq!(rebase["binding_after"]["approved_blobs"], set("merged"));
    // The audit keeps the rebase as its own change, apart from the
    // comparison: each side's head, tree hash and set.
    let rewritten = audited(&wiring, "landing.rewritten");
    assert!(!rewritten.is_empty(), "{:?}", wiring.audit_records());
    let first_rewrite = &rewritten[0];
    assert_eq!(first_rewrite["change"]["after"]["rewrite"], "rebase");
    for (side, head) in [("before", "head_before"), ("after", "head_after")] {
        assert_eq!(
            first_rewrite["change"][side]["head"], rebase[head],
            "{first_rewrite}"
        );
        assert_eq!(
            first_rewrite["change"][side]["binding"]["tree_hash"],
            format!("tree of {}", rebase[head].as_str().unwrap()),
            "{first_rewrite}"
        );
    }
    assert_eq!(first_rewrite["landing"]["approved_blobs"], set("merged"));
    assert_eq!(first_rewrite["sender_id"], "integrate:control");
    assert!(first["rebased"].is_boolean(), "{first}");
    assert_eq!(payloads(&detail, "review_finished").len(), 2);
    assert_eq!(
        payloads(&detail, "review_finished")[1]["binding"]["approved_blobs"],
        set("merged")
    );
    let error = payloads(&detail, "integration_error")[0];
    assert_eq!(error["code"], "landing_changed");
    assert_eq!(error["reapproval"], "review");
    let compared = audited(&wiring, "landing.compared");
    assert_eq!(compared[0]["outcome"]["status"], "denied");
    assert_eq!(compared[1]["outcome"]["status"], "succeeded");
}

/// A review is bound to the diff it read as it started: the run's branch
/// changing its file while the review runs does not move the pass to the
/// new blob. The landing finds the merged blob differ from the pass's and
/// does not land; the review again, started on the new blob, passes it,
/// and that pass lands.
#[test]
fn a_pass_is_bound_to_the_diff_its_review_read_as_it_started() {
    let (wiring, task) = wired(
        "a_pass_is_bound_to_the_diff_its_review_read_as_it_started",
        vec![verdict("pass", "fine"), verdict("pass", "still fine")],
    );
    wiring.git.change_blob_after_next_binding(FILE, "moved");
    wiring.supervise_once();
    wiring.supervise_once();
    let detail = assert_landed(&wiring, task);
    let started = payloads(&detail, "review_started");
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 2, "{:?}", kinds(&detail));
    assert_eq!(
        started[0]["binding"]["approved_blobs"],
        set(&format!("blob-{FILE}"))
    );
    // The first pass keeps what its review read, not the moved blob.
    assert_eq!(
        finished[0]["binding"]["approved_blobs"],
        set(&format!("blob-{FILE}"))
    );
    assert_eq!(
        comparisons(&detail),
        [
            (json!("review_pass"), json!(false)),
            (json!("review_pass"), json!(true))
        ]
    );
    assert_eq!(
        payloads(&detail, "integration_compared")[0]["differing"],
        json!([FILE])
    );
    assert_eq!(started[1]["binding"]["approved_blobs"], set("moved"));
    assert_eq!(finished[1]["binding"]["approved_blobs"], set("moved"));
}

/// A commit the worker adds after the review started, which the receipt
/// does not name, changes the file the pass was bound to: the landing
/// records the comparison that differed, spending the pass, and sends the
/// run back to its session for a receipt of its head. The pass does not
/// land it; the review of the new receipt binds the new blob, and its pass
/// lands.
#[test]
fn a_commit_after_the_pass_the_receipt_does_not_name_spends_the_pass() {
    let (wiring, task) = wired(
        "a_commit_after_the_pass_the_receipt_does_not_name_spends_the_pass",
        vec![verdict("pass", "fine"), verdict("pass", "still fine")],
    );
    wiring.git.change_blob_after_next_binding(FILE, "unnamed");
    wiring.git.commit_after_next_binding();
    wiring.supervise_once();
    wiring.supervise_once();
    let detail = assert_landed(&wiring, task);
    assert_eq!(
        comparisons(&detail),
        [
            (json!("review_pass"), json!(false)),
            (json!("review_pass"), json!(true))
        ],
        "{:?}",
        kinds(&detail)
    );
    let compared = payloads(&detail, "integration_compared")[0];
    assert_eq!(compared["unnamed_commit"], true);
    assert_eq!(compared["differing"], json!([FILE]));
    assert_eq!(compared["reapproval"], "review");
    assert_eq!(compared["landed"]["approved_blobs"], set("unnamed"));
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 2);
    assert_eq!(
        finished[0]["binding"]["approved_blobs"],
        set(&format!("blob-{FILE}"))
    );
    assert_eq!(finished[1]["binding"]["approved_blobs"], set("unnamed"));
    assert_eq!(
        audited(&wiring, "landing.compared")[0]["outcome"]["status"],
        "denied"
    );
}

/// A binding whose diff hash or tree hash cannot be read is an error, not
/// a binding short of them: no judgment or comparison is recorded with an
/// audit record missing its hashes.
#[test]
fn a_binding_whose_hashes_cannot_be_read_is_an_error() {
    let (wiring, _task) = wired("a_binding_whose_hashes_cannot_be_read_is_an_error", vec![]);
    let main = crate::application::RepositoryReads::main_head(&wiring.git).unwrap();
    let bind =
        |to: &str| crate::application::integrate::binding::bind(&wiring.git, main.as_str(), to);
    let bound = bind(main.as_str()).unwrap();
    assert!(bound.diff_hash.is_some() && bound.tree_hash.is_some());
    // The tree of a commit the repository does not have.
    let no_tree = bind(&"f".repeat(40)).unwrap_err();
    assert!(format!("{no_tree:#}").contains("tree hash"), "{no_tree:#}");
    wiring.git.fail_diff_text();
    let no_diff = bind(main.as_str()).unwrap_err();
    assert!(format!("{no_diff:#}").contains("diff"), "{no_diff:#}");
}

/// An AI's `land` lands on its judgment's blobs, and goes back to a review
/// when the merged blob changed.
#[test]
fn an_ai_land_lands_on_its_blobs_and_a_changed_blob_is_reviewed_again() {
    let (wiring, task) = wired(
        "an_ai_land_lands_on_its_blobs_and_a_changed_blob_is_reviewed_again",
        vec![ai_land(), ai_land()],
    );
    wiring.git.change_blob_at_next_rebase(FILE, "merged");
    wiring.supervise_once();
    wiring.supervise_once();
    let detail = assert_landed(&wiring, task);
    assert_eq!(
        comparisons(&detail),
        [
            (json!("ai_land"), json!(false)),
            (json!("ai_land"), json!(true))
        ]
    );
    assert_eq!(payloads(&detail, "review_finished").len(), 2);
}

/// A commit the worker adds after an AI's `land` (after the judgment, as
/// the conflict precheck before the landing runs), which the receipt does
/// not name, spends the judgment: the comparison differs and names the AI's
/// path, the run goes back to its session and is reviewed again, and the
/// new `land`, bound to the new blob, lands.
#[test]
fn a_commit_after_an_ai_land_spends_it_and_the_run_is_reviewed_again() {
    let (wiring, task) = wired(
        "a_commit_after_an_ai_land_spends_it_and_the_run_is_reviewed_again",
        vec![ai_land(), ai_land()],
    );
    wiring.git.commit_after_next_merge_check(FILE, "after");
    wiring.supervise_once();
    wiring.supervise_once();
    let detail = assert_landed(&wiring, task);
    assert_unnamed_commit_spent(&detail, "ai_land", "review");
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 2, "{:?}", kinds(&detail));
    assert_eq!(
        finished[0]["binding"]["approved_blobs"],
        set(&format!("blob-{FILE}"))
    );
    assert_eq!(finished[1]["binding"]["approved_blobs"], set("after"));
}

/// The first comparison of `detail` is the differing one an unnamed commit
/// after the judgment of `path` recorded, sending the run to
/// `reapproval`, and a later one matched.
fn assert_unnamed_commit_spent(detail: &TaskDetail, path: &str, reapproval: &str) {
    let compared = payloads(detail, "integration_compared");
    assert!(compared.len() >= 2, "{:?}", kinds(detail));
    assert_eq!(compared[0]["path"], path, "{}", compared[0]);
    assert_eq!(compared[0]["matched"], false, "{}", compared[0]);
    assert_eq!(compared[0]["unnamed_commit"], true, "{}", compared[0]);
    assert_eq!(compared[0]["differing"], json!([FILE]));
    assert_eq!(compared[0]["reapproval"], reapproval);
    assert_eq!(compared[0]["landed"]["approved_blobs"], set("after"));
    assert_eq!(compared.last().unwrap()["matched"], true);
}

/// A commit the worker adds after a person's `land` answer, before the
/// supervisor applies it, which the receipt does not name, does not land
/// on the answer: the comparison differs and names the person's path, and
/// the run goes back to its session. Its review again passes, but a pass
/// does not stand for the person: the landing goes back to a new
/// approve_landing ask, whose `land` lands the new blob.
#[test]
fn a_commit_after_a_persons_land_spends_it_and_the_ask_is_opened_again() {
    let (wiring, task) = wired(
        "a_commit_after_a_persons_land_spends_it_and_the_ask_is_opened_again",
        vec![concern(), verdict("pass", "fine")],
    );
    wiring.supervise_once();
    let open = |wiring: &Wiring| {
        wiring
            .queue()
            .asks(Default::default())
            .unwrap()
            .into_iter()
            .filter(|ask| ask.kind == AskKind::ApproveLanding && ask.closed_at.is_none())
            .collect::<Vec<_>>()
    };
    let first = open(&wiring);
    assert_eq!(first.len(), 1);
    wiring.queue().answer(first[0].id, "land").unwrap();
    // After the answer, before it is applied: the worker's commit.
    let detail = wiring.queue().show(task).unwrap();
    let worktree = detail.runs[0].worktree_path().unwrap().to_owned();
    wiring
        .git
        .commit_in(std::path::Path::new(&worktree))
        .unwrap();
    wiring.git.set_changed_blob(FILE, "after");
    wiring.supervise_once();
    wiring.supervise_once();
    let again = open(&wiring);
    assert_eq!(
        again.len(),
        1,
        "{:?}",
        kinds(&wiring.queue().show(task).unwrap())
    );
    assert_ne!(again[0].id, first[0].id);
    wiring.queue().answer(again[0].id, "land").unwrap();
    wiring.supervise_once();
    let detail = assert_landed(&wiring, task);
    assert_unnamed_commit_spent(&detail, "approve_landing", "approve_landing");
    // The pass after the session's new receipt did not land: its landing
    // went back to the person.
    assert!(
        payloads(&detail, "integration_compared")
            .iter()
            .any(|c| c["path"] == "review_pass"
                && c["matched"] == false
                && c["reapproval"] == "approve_landing"),
        "{:?}",
        payloads(&detail, "integration_compared")
    );
    let approvals = payloads(&detail, "integration_approved");
    assert_eq!(
        approvals[0]["binding"]["approved_blobs"],
        set(&format!("blob-{FILE}"))
    );
    assert_eq!(
        approvals.last().unwrap()["binding"]["approved_blobs"],
        set("after")
    );
}

/// A person's `land` lands on the blobs it was bound to; when the merged
/// blob changed, its approve_landing ask is opened again, and the next
/// `land` lands the current diff.
#[test]
fn a_persons_land_whose_merged_blob_changed_is_asked_again_and_lands_on_the_new_answer() {
    let (wiring, task) = wired(
        "a_persons_land_whose_merged_blob_changed_is_asked_again_and_lands_on_the_new_answer",
        vec![concern()],
    );
    wiring.supervise_once();
    let open = |wiring: &Wiring| {
        wiring
            .queue()
            .asks(Default::default())
            .unwrap()
            .into_iter()
            .filter(|ask| ask.kind == AskKind::ApproveLanding && ask.closed_at.is_none())
            .collect::<Vec<_>>()
    };
    let first = open(&wiring);
    assert_eq!(first.len(), 1);
    wiring.git.change_blob_at_next_rebase(FILE, "merged");
    wiring.queue().answer(first[0].id, "land").unwrap();
    wiring.supervise_once();
    wiring.supervise_once();
    let again = open(&wiring);
    assert_eq!(
        again.len(),
        1,
        "{:?}",
        kinds(&wiring.queue().show(task).unwrap())
    );
    assert_ne!(again[0].id, first[0].id);
    let detail = wiring.queue().show(task).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert_eq!(
        comparisons(&detail),
        [(json!("approve_landing"), json!(false))]
    );
    wiring.queue().answer(again[0].id, "land").unwrap();
    wiring.supervise_once();
    let detail = assert_landed(&wiring, task);
    assert_eq!(
        comparisons(&detail),
        [
            (json!("approve_landing"), json!(false)),
            (json!("approve_landing"), json!(true))
        ]
    );
    let approved = audited(&wiring, "landing.approved");
    assert_eq!(approved.len(), 2);
    assert_eq!(
        approved[0]["landing"]["approved_blobs"],
        set(&format!("blob-{FILE}"))
    );
    assert_eq!(approved[1]["landing"]["approved_blobs"], set("merged"));
    assert_eq!(
        approved[1]["change"]["before"]["approved_blobs"],
        set(&format!("blob-{FILE}"))
    );
}

/// A person's `land` is bound to the diff its ask was opened on: a change
/// after the answer, before the supervisor applies it, is not approved by
/// it. The landing finds the merged blob differ, does not land and asks
/// again. The approval's audit names the person who answered, directly,
/// besides the supervisor that wrote it.
#[test]
fn a_persons_land_is_bound_to_the_diff_its_ask_showed_and_audits_the_answerer() {
    let (wiring, task) = wired(
        "a_persons_land_is_bound_to_the_diff_its_ask_showed_and_audits_the_answerer",
        vec![concern()],
    );
    wiring.supervise_once();
    let asks = wiring.queue().asks(Default::default()).unwrap();
    let ask = asks
        .iter()
        .find(|ask| ask.kind == AskKind::ApproveLanding)
        .expect("an approve_landing ask");
    wiring.queue().answer(ask.id, "land").unwrap();
    // The run's file changed after the answer, before it is applied.
    wiring.git.set_changed_blob(FILE, "after");
    wiring.supervise_once();
    let detail = wiring.queue().show(task).unwrap();
    let approvals = payloads(&detail, "integration_approved");
    assert_eq!(
        approvals[0]["binding"]["approved_blobs"],
        set(&format!("blob-{FILE}")),
        "{:?}",
        kinds(&detail)
    );
    assert_eq!(
        comparisons(&detail),
        [(json!("approve_landing"), json!(false))]
    );
    assert_ne!(detail.runs[0].status(), RunStatus::Integrated);
    let approved = audited(&wiring, "landing.approved");
    assert_eq!(
        approved[0]["principal"]["role"], "supervisor",
        "{}",
        approved[0]
    );
    assert_eq!(
        approved[0]["on_behalf_of"][0]["id"], "person",
        "{}",
        approved[0]
    );
    assert_eq!(
        approved[0]["answered_via"], "human_attested",
        "{}",
        approved[0]
    );
}

/// An approve_landing ask whose diff is not on record (here, opened after
/// a review that failed before it started; an older version's ask is the
/// same) approves nothing by its `land`: the current diff is recorded as a
/// comparison that differed, the ask is opened again on it, and the
/// `land` of that ask lands the run on its set.
#[test]
fn a_land_of_an_ask_with_no_diff_on_record_is_asked_again_on_the_current_diff() {
    let (wiring, task) = wired(
        "a_land_of_an_ask_with_no_diff_on_record_is_asked_again_on_the_current_diff",
        vec![],
    );
    wiring.git.fail_diff_text();
    wiring.supervise_once();
    let open = |wiring: &Wiring| {
        wiring
            .queue()
            .asks(Default::default())
            .unwrap()
            .into_iter()
            .filter(|ask| ask.kind == AskKind::ApproveLanding && ask.closed_at.is_none())
            .collect::<Vec<_>>()
    };
    let first = open(&wiring);
    assert_eq!(
        first.len(),
        1,
        "{:?}",
        kinds(&wiring.queue().show(task).unwrap())
    );
    wiring.git.restore_diff_text();
    wiring.queue().answer(first[0].id, "land").unwrap();
    wiring.supervise_once();
    wiring.supervise_once();
    let detail = wiring.queue().show(task).unwrap();
    assert!(
        payloads(&detail, "integration_approved").is_empty(),
        "{:?}",
        kinds(&detail)
    );
    let compared = payloads(&detail, "integration_compared");
    assert_eq!(compared.len(), 1, "{:?}", kinds(&detail));
    assert_eq!(compared[0]["ask_unbound"], true);
    assert_eq!(compared[0]["reapproval"], "approve_landing");
    let again = open(&wiring);
    assert_eq!(again.len(), 1, "{:?}", kinds(&detail));
    assert_ne!(again[0].id, first[0].id);
    wiring.queue().answer(again[0].id, "land").unwrap();
    wiring.supervise_once();
    let detail = assert_landed(&wiring, task);
    assert_eq!(
        payloads(&detail, "integration_approved")[0]["binding"]["approved_blobs"],
        set(&format!("blob-{FILE}"))
    );
    assert_eq!(
        comparisons(&detail),
        [
            (json!("approve_landing"), json!(false)),
            (json!("approve_landing"), json!(true))
        ]
    );
}

/// A person's `integrate` of a run with no pass and no answer lands as
/// before, its approval audited with its actor and the set it was bound
/// to. When the merged blob changed after the approval, nothing lands:
/// the slot goes back, the run awaits integration with the mismatch as the
/// attention's error, and `integrate` again approves the current diff and
/// lands it.
#[test]
fn a_direct_integrate_lands_on_its_approval_and_a_changed_blob_needs_integrate_again() {
    let (wiring, task) = wired(
        "a_direct_integrate_lands_on_its_approval_and_a_changed_blob_needs_integrate_again",
        vec![concern()],
    );
    wiring.supervise_once();
    wiring.git.change_blob_at_next_rebase(FILE, "merged");
    let outcome = wiring.integrate_outcome(task);
    assert_eq!(outcome["outcome"], "changed", "{outcome}");
    assert_eq!(outcome["reapproval"], "integrate");
    assert_eq!(outcome["differing"], json!([FILE]));
    let detail = wiring.queue().show(task).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(wiring.queue().run_leases().unwrap().is_empty());
    let status = wiring.status();
    let entry = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| {
            entry["run_id"] == json!(detail.runs[0].id())
                && entry["kind"] == "integration_compared"
                && entry["last_error_code"] == "landing_changed"
        })
        .unwrap_or_else(|| panic!("{status}"))
        .clone();
    assert!(
        entry["last_error"]
            .as_str()
            .unwrap()
            .contains("dagq integrate"),
        "{entry}"
    );
    wiring.integrate(task);
    let detail = assert_landed(&wiring, task);
    assert_eq!(
        comparisons(&detail),
        [
            (json!("integrate"), json!(false)),
            (json!("integrate"), json!(true))
        ]
    );
    let approved = audited(&wiring, "landing.approved");
    assert_eq!(approved.len(), 2);
    for record in &approved {
        assert_eq!(record["principal"]["role"], "user", "{record}");
        assert!(record["landing"]["diff_hash"].is_string());
        assert!(record["landing"]["tree_hash"].is_string());
    }
    assert_eq!(
        approved[0]["landing"]["approved_blobs"],
        set(&format!("blob-{FILE}"))
    );
    assert_eq!(approved[1]["landing"]["approved_blobs"], set("merged"));
}

/// A `revise` verdict whose reasons carry codes: each `(text, codes)`.
fn coded_revise(reasons: &[(&str, &[&str])]) -> Value {
    let reasons: Vec<Value> = reasons
        .iter()
        .map(|(text, codes)| json!({"text": text, "codes": codes}))
        .collect();
    json!({"verdict": "revise", "reasons": reasons, "summary": "not yet"})
}

/// A run that converged past its revise limit lands on the blobs of the
/// convergence; when the merged blob changed, it is reviewed again, and
/// the new review's pass lands it.
#[test]
fn a_convergence_whose_merged_blob_changed_is_reviewed_again_and_lands_on_the_new_pass() {
    let wiring = Wiring::new(
        "a_convergence_whose_merged_blob_changed_is_reviewed_again_and_lands_on_the_new_pass",
        Script {
            receipts: vec![succeeded; 4],
            verdicts: vec![
                coded_revise(&[("add a line", &["acceptance_unmet"])]),
                coded_revise(&[("add a test", &["test_gap"])]),
                coded_revise(&[("a stale sentence in docs/a.md", &["docs_drift"])]),
                verdict("pass", "fine"),
            ],
        },
    );
    const CONFIG: &str = "[review.programs.\"builtin:secrets\"]\nenabled = false\n";
    wiring
        .git
        .commit_files(&[("dagq.toml", CONFIG), (FILE, "one\n")], &[FILE]);
    wiring.verifier.set_config(CONFIG);
    let task = wiring.ready_task(&["true"]);
    wiring.git.change_blob_at_next_rebase(FILE, "merged");
    wiring.supervise_once();
    wiring.supervise_once();
    let detail = assert_landed(&wiring, task);
    assert_eq!(
        comparisons(&detail),
        [
            (json!("converged"), json!(false)),
            (json!("review_pass"), json!(true))
        ]
    );
    assert_eq!(payloads(&detail, "review_finished").len(), 4);
    let judged = audited(&wiring, "landing.judged");
    assert_eq!(judged.len(), 2);
    assert_eq!(judged[0]["change"]["after"]["path"], "converged");
    assert_eq!(judged[1]["landing"]["approved_blobs"], set("merged"));
    // Each revise of the run's branch after a review is audited as its own
    // change, with the binding the review read before it and the one
    // after it.
    let revises: Vec<Value> = audited(&wiring, "landing.rewritten")
        .into_iter()
        .filter(|record| record["change"]["after"]["rewrite"] == "revise")
        .collect();
    assert_eq!(revises.len(), 2, "{revises:?}");
    for record in &revises {
        assert!(
            record["change"]["before"]["binding"]["tree_hash"].is_string(),
            "{record}"
        );
        assert!(
            record["change"]["after"]["binding"]["tree_hash"].is_string(),
            "{record}"
        );
        assert!(
            record["change"]["after"]["binding"]["approved_blobs"][FILE].is_object(),
            "{record}"
        );
    }
}

/// A commit the worker adds after the convergence past the revise limit
/// (after the judgment, as the conflict precheck before the landing
/// runs), which the receipt does not name, spends it: the comparison
/// differs and names the convergence, and the run goes back to its session
/// and is reviewed again, whose pass lands the new blob.
#[test]
fn a_commit_after_a_convergence_spends_it_and_the_run_is_reviewed_again() {
    let wiring = Wiring::new(
        "a_commit_after_a_convergence_spends_it_and_the_run_is_reviewed_again",
        Script {
            receipts: vec![succeeded; 4],
            verdicts: vec![
                coded_revise(&[("add a line", &["acceptance_unmet"])]),
                coded_revise(&[("add a test", &["test_gap"])]),
                coded_revise(&[("a stale sentence in docs/a.md", &["docs_drift"])]),
                verdict("pass", "fine"),
            ],
        },
    );
    const CONFIG: &str = "[review.programs.\"builtin:secrets\"]\nenabled = false\n";
    wiring
        .git
        .commit_files(&[("dagq.toml", CONFIG), (FILE, "one\n")], &[FILE]);
    wiring.verifier.set_config(CONFIG);
    let task = wiring.ready_task(&["true"]);
    wiring.git.commit_after_next_merge_check(FILE, "after");
    wiring.supervise_once();
    wiring.supervise_once();
    let detail = assert_landed(&wiring, task);
    assert_unnamed_commit_spent(&detail, "converged", "review");
    let judged = audited(&wiring, "landing.judged");
    assert_eq!(judged[0]["change"]["after"]["path"], "converged");
    assert_eq!(
        judged.last().unwrap()["landing"]["approved_blobs"],
        set("after")
    );
}

/// A later waiting run whose landing differed from a person's `land` gets
/// its approve_landing ask again behind an earlier waiting run (the order
/// whatever the slots is `split_reasks`'s unit test). The ask opened again
/// is returned, not replaced, when another supervisor opens it again: the
/// ask's own write keeps the open one.
#[test]
fn a_reask_opens_for_a_later_run_and_is_kept_when_asked_again() {
    let (wiring, first) = wired(
        "a_reask_opens_for_a_later_run_and_is_kept_when_asked_again",
        vec![concern(), concern()],
    );
    let second = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let open = |wiring: &Wiring, run: &crate::domain::RunId| {
        wiring
            .queue()
            .asks(Default::default())
            .unwrap()
            .into_iter()
            .filter(|ask| {
                ask.kind == AskKind::ApproveLanding
                    && ask.closed_at.is_none()
                    && ask.run_id.as_ref() == Some(run)
            })
            .collect::<Vec<_>>()
    };
    let first_run = wiring.queue().show(first).unwrap().runs[0].id().clone();
    let second_run = wiring.queue().show(second).unwrap().runs[0].id().clone();
    assert_eq!(open(&wiring, &first_run).len(), 1);
    let asked = open(&wiring, &second_run);
    assert_eq!(asked.len(), 1);
    // The second run's landing differed from a person's land: its ask was
    // applied and closed, and the comparison sent it to the ask again.
    let mut queue = wiring.queue();
    queue.answer(asked[0].id, "withdrawn").unwrap();
    queue.close_ask(asked[0].id).unwrap();
    crate::application::EventStore::record_runtime_event(
        &queue,
        &second_run,
        crate::domain::EventKind::IntegrationCompared,
        json!({"path": "approve_landing", "matched": false, "reapproval": "approve_landing",
               "landed": {"approved_blobs": set(&format!("blob-{FILE}"))}, "differing": [FILE]}),
    )
    .unwrap();
    wiring.supervise_once();
    let again = open(&wiring, &second_run);
    assert_eq!(
        again.len(),
        1,
        "{:?}",
        kinds(&wiring.queue().show(second).unwrap())
    );
    assert_ne!(again[0].id, asked[0].id);
    // Another supervisor asking again gets the open ask back, unclosed.
    let outcome = crate::application::ask::ask(
        &mut wiring.queue(),
        crate::domain::NewAsk {
            recommendation: None,
            confidence: None,
            kind: AskKind::ApproveLanding,
            task_id: None,
            run_id: Some(second_run.clone()),
            question: "Land its current diff?".into(),
            options: vec!["land".into(), "send_back".into(), "cancel".into()],
            asked_by: "supervisor".into(),
            reason_category: crate::domain::AskReason::Scope,
            topics: Vec::new(),
            finding_id: None,
            request_id: None,
        },
    )
    .unwrap();
    assert_eq!(outcome["created"], false, "{outcome}");
    assert_eq!(outcome["id"], json!(again[0].id), "{outcome}");
    assert_eq!(open(&wiring, &second_run).len(), 1);
}

/// The records of `action` `landing.rewritten` whose rewrite is `rewrite`.
fn rewrites(wiring: &Wiring, rewrite: &str) -> Vec<Value> {
    audited(wiring, "landing.rewritten")
        .into_iter()
        .filter(|record| record["change"]["after"]["rewrite"] == rewrite)
        .collect()
}

/// The landing's rebase after the pass conflicts, which parks the run for
/// a resume, and the resume that resolves it rewrites the branch after its
/// judgment: its `resume_finished` holds the head and binding before it
/// (the head the conflict parked) and after it (the resolved head), audited
/// as its own change. The resolution changed the judged file: the run is
/// reviewed again, and the landing compares with the new pass. The
/// resolution's binding could not be read once: nothing of the finish was
/// recorded until the next pass read it.
#[test]
fn a_conflict_a_resume_resolved_is_audited_with_both_bindings_and_reviewed_again() {
    let (wiring, task) = wired(
        "a_conflict_a_resume_resolved_is_audited_with_both_bindings_and_reviewed_again",
        vec![verdict("pass", "fine"), verdict("pass", "fine")],
    );
    wiring.git.conflict_at_next_rebase(FILE, "resolved");
    // The first commit is the worker's, the second the resume's.
    wiring.git.fail_first_binding_of_commit(2);
    for _ in 0..3 {
        wiring.supervise_once();
    }
    let detail = assert_landed(&wiring, task);
    assert_eq!(wiring.git.failed_bindings(), 1, "{:?}", kinds(&detail));
    let deferred = payloads(&detail, "integration_deferred");
    assert_eq!(
        deferred[0]["code"],
        "rebase_conflict",
        "{:?}",
        kinds(&detail)
    );
    let resumed = payloads(&detail, "resume_finished");
    assert_eq!(resumed.len(), 1, "{:?}", kinds(&detail));
    let resumed = resumed[0];
    assert_eq!(resumed["outcome"], "resolved", "{resumed}");
    assert_eq!(resumed["rewrite"], "conflict_resolution", "{resumed}");
    assert_eq!(resumed["head_before"], deferred[0]["head"], "{resumed}");
    assert_eq!(resumed["head_after"], resumed["head"], "{resumed}");
    assert_ne!(resumed["head_before"], resumed["head_after"], "{resumed}");
    let before = set(&format!("blob-{FILE}"));
    assert_eq!(resumed["binding_before"]["approved_blobs"], before);
    assert_eq!(resumed["binding_after"]["approved_blobs"], set("resolved"));
    let resolutions = rewrites(&wiring, "conflict_resolution");
    assert_eq!(resolutions.len(), 1, "{resolutions:?}");
    let change = &resolutions[0]["change"];
    assert_eq!(change["before"]["head"], resumed["head_before"]);
    assert_eq!(change["before"]["binding"]["approved_blobs"], before);
    assert!(
        change["before"]["binding"]["tree_hash"].is_string(),
        "{change}"
    );
    assert_eq!(change["after"]["head"], resumed["head_after"]);
    assert_eq!(
        change["after"]["binding"]["approved_blobs"],
        set("resolved")
    );
    assert!(
        change["after"]["binding"]["tree_hash"].is_string(),
        "{change}"
    );
    assert_eq!(
        resolutions[0]["landing"]["approved_blobs"],
        set("resolved"),
        "{}",
        resolutions[0]
    );
    // The resolution is approved again by the review of the resolved
    // head, not carried over from the pass before it: the landing compares
    // with the new pass, bound to the resolved blob.
    assert_eq!(payloads(&detail, "review_finished").len(), 2);
    let judged = audited(&wiring, "landing.judged");
    assert_eq!(judged.len(), 2, "{judged:?}");
    assert_eq!(judged[0]["landing"]["approved_blobs"], before);
    assert_eq!(judged[1]["landing"]["approved_blobs"], set("resolved"));
    assert_eq!(comparisons(&detail), [(json!("review_pass"), json!(true))]);
    assert_eq!(
        payloads(&detail, "integration_compared")[0]["judged"]["approved_blobs"],
        set("resolved")
    );
}

/// A revise whose binding after it cannot be read is not recorded while it
/// cannot: the next pass reads it, and the run's one `revise_finished`
/// holds the binding after it, audited as its own change, before the new
/// review's pass lands the run.
#[test]
fn a_revise_whose_binding_cannot_be_read_yet_is_recorded_once_it_can() {
    let (wiring, task) = wired(
        "a_revise_whose_binding_cannot_be_read_yet_is_recorded_once_it_can",
        vec![
            coded_revise(&[("add a line", &["acceptance_unmet"])]),
            verdict("pass", "fine"),
        ],
    );
    // The first commit is the worker's, the second the revise's.
    wiring.git.fail_first_binding_of_commit(2);
    for _ in 0..2 {
        wiring.supervise_once();
    }
    let detail = assert_landed(&wiring, task);
    assert_eq!(wiring.git.failed_bindings(), 1, "{:?}", kinds(&detail));
    let revised = payloads(&detail, "revise_finished");
    assert_eq!(revised.len(), 1, "{:?}", kinds(&detail));
    assert!(
        revised[0]["binding_after"]["tree_hash"].is_string(),
        "{}",
        revised[0]
    );
    assert_eq!(
        revised[0]["binding_after"]["approved_blobs"],
        set(&format!("blob-{FILE}"))
    );
    let revises = rewrites(&wiring, "revise");
    assert_eq!(revises.len(), 1, "{revises:?}");
    assert_eq!(
        revises[0]["change"]["after"]["head"],
        revised[0]["head_after"]
    );
    assert_eq!(comparisons(&detail), [(json!("review_pass"), json!(true))]);
}

/// A run parked by a rebase conflict whose earlier resume resolved
/// nothing, and whose conflict was then resolved on its branch with its
/// receipt rewritten for it (as a later session or a person does), is
/// moved on without a session: its `resume_skipped` holds the head and
/// binding before it (the conflicted head) and after it (the resolved
/// head), audited as its own change with the skip. The resolution changed
/// the judged file: the run is approved again by its review, whose new
/// pass lands it. The resolution's binding could not be read once:
/// nothing was skipped until the next pass read it.
#[test]
fn a_conflict_resolved_after_an_unresolved_resume_is_skipped_with_both_bindings() {
    use crate::application::RunFiles;
    let (wiring, task) = wired(
        "a_conflict_resolved_after_an_unresolved_resume_is_skipped_with_both_bindings",
        vec![verdict("pass", "fine"), verdict("pass", "fine")],
    );
    wiring.git.conflict_at_next_rebase(FILE, "resolved");
    // No session resolves it: a resume's wrapper cannot start.
    wiring.fail_resume_launches();
    let outcome = wiring.supervise_once_reporting();
    assert_eq!(
        outcome["errors"].as_array().map(Vec::len),
        Some(1),
        "{outcome}"
    );
    let detail = wiring.queue().show(task).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(
        run.status(),
        RunStatus::NeedsSession,
        "{:?}",
        kinds(&detail)
    );
    let deferred = payloads(&detail, "integration_deferred");
    assert_eq!(
        deferred[0]["code"],
        "rebase_conflict",
        "{:?}",
        kinds(&detail)
    );
    let conflicted = deferred[0]["head"].clone();
    // An earlier resume that resolved nothing.
    let mut queue = wiring.queue();
    let earlier = crate::domain::LeaseToken::new("earlier");
    queue
        .begin_resume(
            run.id(),
            &earlier,
            &wiring.git.main(),
            None,
            Default::default(),
        )
        .unwrap()
        .expect("the parked run resumes");
    queue
        .finish_resume(
            run.id(),
            &earlier,
            None,
            None,
            false,
            json!({"attempt": 1, "outcome": "unresolved", "exhausted": false}),
        )
        .unwrap();
    // The conflict resolved on the branch, and its receipt rewritten for it.
    wiring.git.fail_first_binding_of_commit(1);
    let worktree = std::path::PathBuf::from(run.worktree_path().unwrap());
    let resolved = wiring.git.commit_in(&worktree).unwrap();
    let path = std::path::PathBuf::from(run.receipt_path().unwrap());
    let mut receipt: Value =
        serde_json::from_str(&wiring.files.read_to_string(&path).unwrap()).unwrap();
    receipt["commit"] = json!(resolved);
    wiring
        .files
        .write(&path, receipt.to_string().as_bytes())
        .unwrap();
    for _ in 0..4 {
        wiring.supervise_once();
    }
    let detail = assert_landed(&wiring, task);
    assert_eq!(wiring.git.failed_bindings(), 1, "{:?}", kinds(&detail));
    let skipped = payloads(&detail, "resume_skipped");
    assert_eq!(skipped.len(), 1, "{:?}", kinds(&detail));
    let skipped = skipped[0];
    assert_eq!(skipped["head"], json!(resolved), "{skipped}");
    assert_eq!(skipped["rewrite"], "conflict_resolution", "{skipped}");
    assert_eq!(skipped["head_before"], conflicted, "{skipped}");
    assert_eq!(skipped["head_after"], json!(resolved), "{skipped}");
    let before = set(&format!("blob-{FILE}"));
    assert_eq!(skipped["binding_before"]["approved_blobs"], before);
    assert_eq!(skipped["binding_after"]["approved_blobs"], set("resolved"));
    let resolutions = rewrites(&wiring, "conflict_resolution");
    assert_eq!(resolutions.len(), 1, "{resolutions:?}");
    let change = &resolutions[0]["change"];
    assert_eq!(change["before"]["head"], conflicted);
    assert_eq!(change["before"]["binding"]["approved_blobs"], before);
    assert!(
        change["before"]["binding"]["tree_hash"].is_string(),
        "{change}"
    );
    assert_eq!(change["after"]["head"], json!(resolved));
    assert_eq!(
        change["after"]["binding"]["approved_blobs"],
        set("resolved")
    );
    assert!(
        change["after"]["binding"]["tree_hash"].is_string(),
        "{change}"
    );
    // Approved again by its review: the landing compares with the new pass.
    assert_eq!(payloads(&detail, "review_finished").len(), 2);
    let judged = audited(&wiring, "landing.judged");
    assert_eq!(
        judged.last().unwrap()["landing"]["approved_blobs"],
        set("resolved")
    );
    let compared = payloads(&detail, "integration_compared");
    let last = compared.last().expect("a comparison");
    assert_eq!(last["matched"], true, "{compared:?}");
    assert_eq!(last["judged"]["approved_blobs"], set("resolved"));
}
