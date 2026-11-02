//! The supervisor's record of main's history wired to the queue
//! (docs/design/main-history.md "記録" and "取り込み"): a pass reads main's
//! commits and their messages through the repository's port and records
//! the evidence of a revert, importing it once for the commits recorded
//! before the record carried it.

use serde_json::json;

use super::{agent::Script, harness::Wiring};
use crate::domain::{EventKind, stats::main_log::Reach};

/// A queue whose record of main reached its head before the commits
/// carried the evidence of a revert: the supervisor's pass imports the
/// evidence of every line recorded once (the rewrites read from the queue), then records a later revert with its
/// commit, and imports nothing again.
#[test]
fn the_record_of_main_imports_and_records_the_evidence_of_a_revert() {
    let wiring = Wiring::new(
        "the_record_of_main_imports_and_records_the_evidence_of_a_revert",
        Script {
            receipts: Vec::new(),
            verdicts: Vec::new(),
        },
    );
    let landed = wiring.git.land_with_message("feat: a change\n");
    let revert = wiring.git.land_with_message(&format!(
        "Revert \"feat: a change\"\n\nThis reverts commit {landed}.\n"
    ));
    let queue = wiring.queue();
    // An earlier rewrite took off a line whose head Git no longer has.
    let gone = "f".repeat(40);
    queue
        .record_queue_event(
            EventKind::MainRewritten,
            json!({"from": gone, "head": revert, "base": null, "at": 1}),
        )
        .unwrap();
    queue
        .record_queue_event(
            EventKind::MainObserved,
            json!({"head": revert, "at": 1, "since": 0}),
        )
        .unwrap();

    wiring.supervise_once();
    let latest = |kind: &str| wiring.queue().latest_queue_event(&[kind]).unwrap();
    let imported = latest("main_reverts_imported").expect("the import");
    // Every line recorded is imported: the reach's, read, and the one the
    // rewrite took off, named unread.
    assert_eq!(imported.payload["heads"], json!([revert]));
    assert_eq!(imported.payload["unread"], json!([gone]));
    assert_eq!(
        imported.payload["reverts"],
        json!([{"sha": revert, "at": 2, "reverts": landed}])
    );
    let reach = Reach::of(&latest("main_observed").unwrap()).unwrap();
    assert!(reach.reverts, "{reach:?}");
    assert_eq!(reach.head, revert.as_str());

    // A later revert is recorded with its commit; nothing is imported
    // again.
    let again = wiring.git.land_with_message(&format!(
        "Revert \"Revert\"\n\nThis reverts commit {revert}.\n"
    ));
    wiring.supervise_once();
    assert_eq!(latest("main_reverts_imported").unwrap().id, imported.id);
    let recorded = latest("main_commits_recorded").expect("the commit recorded");
    assert_eq!(recorded.payload["after"], json!(revert));
    assert_eq!(
        recorded.payload["commits"],
        json!([{"sha": again, "at": 3, "changes": [], "reverts": revert}])
    );
}
