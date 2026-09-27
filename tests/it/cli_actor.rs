//! The actor a command runs as (ADR-t728-1 decision 4): `DAGQ_ROLE` is
//! parsed, an unknown value is refused before anything changes, none is
//! the user, the headless jobs read only, and the writer the records keep
//! is what it was before the type.

use crate::common;

use common::cli::*;

use serde_json::Value;

/// The error `args` fails with under `role`.
fn refused_as(role: &str, db: &std::path::Path, args: &[&str]) -> String {
    let output = invoke_as(Some(role), db, args);
    assert!(!output.status.success(), "{role} {args:?} was allowed");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    error["error"].as_str().unwrap().to_owned()
}

fn ask_args(question: &str) -> Vec<&str> {
    vec![
        "ask",
        "--kind",
        "decide",
        "--because",
        "recovery_failed",
        "--question",
        question,
        "--task",
        "1",
    ]
}

#[test]
fn an_unknown_role_fails_closed_and_changes_nothing() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    let before = ok(&db, &["events", "--after", "0", "--all"])["cursor"].clone();
    for role in ["inboxes", "user", "Worker", "human", "reviewers"] {
        for args in [
            &["add", "second"][..],
            &["note", "--task", "1", "--text", "x"],
            &["cancel", "1"],
            &ask_args("q"),
            &["list"],
        ] {
            assert_eq!(
                refused_as(role, &db, args),
                format!("unknown DAGQ_ROLE: {role}"),
                "{role} {args:?}"
            );
        }
    }
    assert_eq!(
        ok(&db, &["events", "--after", "0", "--all"])["cursor"],
        before
    );
    assert_eq!(ok(&db, &["list"])["total"], 1);
    // An empty role is no role: the user.
    ok_as("", &db, &["note", "--task", "1", "--text", "by a person"]);
    assert_eq!(ok(&db, &["notes"])["notes"][0]["payload"]["by"], "human");
}

/// Every headless job, and the legacy `reviewer` an older binary started,
/// reads the queue and changes nothing (ADR-0027, ADR-t728-1 decision 2).
#[test]
fn every_headless_job_may_only_read_the_queue() {
    let (_dir, db) = queue();
    ok(&db, &["goal", "add", "open goal"]);
    ok(&db, &["add", "existing", "--goal", "1"]);
    let asked = ok(&db, &ask_args("which?"));
    let ask = asked["id"].to_string();
    for role in [
        "review-job",
        "recovery-job",
        "plan-review-job",
        "goal-review-job",
        "reviewer",
    ] {
        for args in [
            &["cancel", "1"][..],
            &["integrate", "1"],
            &["answer", &ask, "--text", "x"],
            &["ready", "1", "--bypass-review"],
            &["note", "--task", "1", "--text", "x"],
            &ask_args("q"),
            &["goal", "close", "1", "--verdict", "achieved"],
            &["mark", "label"],
        ] {
            assert_eq!(
                refused_as(role, &db, args),
                "reviewer may not change queue state",
                "{role} {args:?}"
            );
        }
        for args in [
            &["list"][..],
            &["show", "1"],
            &["status"],
            &["asks"],
            &["notes"],
            &["findings"],
            &["goal", "show", "1"],
        ] {
            ok_as(role, &db, args);
        }
    }
    // Nothing was answered or cancelled.
    assert!(ok(&db, &["asks", "--all"])["asks"][0]["answer"].is_null());
    assert_eq!(ok(&db, &["show", "1"])["task"]["status"], "draft");
}

/// The writer of a note, mark, finding and ask, and the answerer of an
/// answer, are the role's name as before, and `human` / `person` for the
/// user.
#[test]
fn the_records_keep_the_writer_they_had() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    for (role, writer, answerer) in [
        (None, "human", "person"),
        (Some("inbox"), "inbox", "inbox"),
        (Some("planner"), "planner", "planner"),
        (Some("worker"), "worker", "worker"),
        (Some("supervisor"), "supervisor", "supervisor"),
    ] {
        let run = |args: &[&str]| {
            let output = invoke_as(role, &db, args);
            assert!(
                output.status.success(),
                "{role:?} {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice::<Value>(&output.stdout).unwrap()
        };
        let text = format!("note of {writer}");
        run(&["note", "--task", "1", "--text", &text]);
        let notes = ok(&db, &["notes"])["notes"].clone();
        let note = notes
            .as_array()
            .unwrap()
            .iter()
            .find(|note| note["payload"]["text"] == text.as_str())
            .unwrap();
        assert_eq!(note["payload"]["by"], writer, "{role:?}");

        let marked = run(&["mark", &format!("mark of {writer}")]);
        assert_eq!(marked["detail"]["by"], writer, "{role:?}");

        let finding = run(&[
            "finding",
            "record",
            "--kind",
            "failure",
            "--task",
            "1",
            "--subject",
            writer,
            "--summary",
            "fails",
        ]);
        assert_eq!(finding["recorded_by"], writer, "{role:?} {finding}");

        let question = format!("asked by {writer}");
        let asked = run(&ask_args(&question));
        assert_eq!(asked["asked_by"], writer, "{role:?}");
        let answered = run(&["answer", &asked["id"].to_string(), "--text", "done"]);
        assert_eq!(answered["answered_by"], answerer, "{role:?}");
        ok(&db, &["ask", "close", &asked["id"].to_string()]);
    }
}
