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
/// user, for each of them the role may write (task 733).
#[test]
fn the_records_keep_the_writer_they_had() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    let worker = [
        ("DAGQ_ROLE", "worker"),
        ("DAGQ_RUN_ID", "r1"),
        ("DAGQ_TASK_ID", "1"),
    ];
    let planner = [("DAGQ_ROLE", "planner")];
    let inbox = [("DAGQ_ROLE", "inbox")];
    let supervisor = [("DAGQ_ROLE", "supervisor")];
    // (env, writer, its ask's kind, marks, records findings, answerer)
    type Actor<'a> = (
        &'a [(&'a str, &'a str)],
        &'a str,
        &'a str,
        bool,
        bool,
        Option<&'a str>,
    );
    let actors: [Actor; 5] = [
        (&[], "human", "decide", true, true, Some("person")),
        (&inbox, "inbox", "decide", true, true, Some("inbox")),
        (&planner, "planner", "planner_question", true, false, None),
        (&worker, "worker", "worker_question", false, false, None),
        (&supervisor, "supervisor", "decide", false, true, None),
    ];
    for (env, writer, kind, marks, finds, answerer) in actors {
        let run = |args: &[&str]| {
            let output = invoke_with(env, &db, args);
            assert!(
                output.status.success(),
                "{env:?} {args:?}: {}",
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
        assert_eq!(note["payload"]["by"], writer, "{env:?}");

        if marks {
            let marked = run(&["mark", &format!("mark of {writer}")]);
            assert_eq!(marked["detail"]["by"], writer, "{env:?}");
        }
        if finds {
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
            assert_eq!(finding["recorded_by"], writer, "{env:?} {finding}");
        }

        let question = format!("asked by {writer}");
        let mut args = ask_args(&question);
        args[2] = kind;
        if kind == "worker_question" {
            args.extend(["--topic", "task_overlap"]);
        }
        let asked = run(&args);
        assert_eq!(asked["asked_by"], writer, "{env:?}");
        let id = asked["id"].to_string();
        let answered = match answerer {
            Some(_) => run(&["answer", &id, "--text", "done"]),
            None => ok(&db, &["answer", &id, "--text", "done"]),
        };
        assert_eq!(
            answered["answered_by"],
            answerer.unwrap_or("person"),
            "{env:?}"
        );
        ok(&db, &["ask", "close", &id]);
    }
}

/// Every event records the actor that wrote it (task 730): the user of a
/// plain terminal, a worker's ask, an observer's finding and a planner's
/// goal, each with its role and actor id, which `events --full` and `show`
/// print; a row written before the queue recorded actors reads without one.
#[test]
fn every_event_records_its_actor() {
    let (_dir, db) = queue();
    let run = |env: &[(&str, &str)], args: &[&str]| {
        let output = invoke_with(env, &db, args);
        assert!(
            output.status.success(),
            "{env:?} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&[], &["add", "first"]);
    let worker = [
        ("DAGQ_ROLE", "worker"),
        ("DAGQ_ACTOR_ID", "worker:r1"),
        ("DAGQ_RUN_ID", "r1"),
        ("DAGQ_TASK_ID", "1"),
    ];
    let mut question = ask_args("which way?");
    question[2] = "worker_question";
    question.extend(["--topic", "task_overlap"]);
    run(&worker, &question);
    let observer = [("DAGQ_ROLE", "observer"), ("DAGQ_ACTOR_ID", "observer:s1")];
    run(
        &observer,
        &[
            "finding",
            "record",
            "--kind",
            "stall",
            "--task",
            "1",
            "--summary",
            "waits",
        ],
    );
    // A session from before the actor id is named by its role.
    run(&[("DAGQ_ROLE", "planner")], &["goal", "add", "a goal"]);
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO run_events(kind,payload) VALUES ('older_event','{}')",
            [],
        )
        .unwrap();

    let events = ok(&db, &["events", "--after", "0", "--all", "--full"])["events"].clone();
    let actor_of = |kind: &str| {
        events
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["kind"] == kind)
            .unwrap_or_else(|| panic!("no {kind} in {events}"))["actor"]
            .clone()
    };
    assert_eq!(
        actor_of("task_created"),
        serde_json::json!({"role": "user", "id": "user"})
    );
    assert_eq!(
        actor_of("ask_opened"),
        serde_json::json!({"role": "worker", "id": "worker:r1"})
    );
    assert_eq!(
        actor_of("finding_recorded"),
        serde_json::json!({"role": "observer", "id": "observer:s1"})
    );
    assert_eq!(
        actor_of("goal_created"),
        serde_json::json!({"role": "planner", "id": "planner"})
    );
    assert!(actor_of("older_event").is_null());
    // The compact form stays as it was.
    let compact = ok(&db, &["events", "--after", "0", "--all"])["events"].clone();
    assert!(compact[0].get("actor").is_none(), "{compact}");
    // `show` prints the actor of the task's events.
    let shown = ok(&db, &["show", "1"]);
    let task_events = shown["events"].as_array().unwrap();
    assert_eq!(task_events[0]["kind"], "task_created");
    assert_eq!(task_events[0]["actor"]["role"], "user");
    assert!(
        task_events
            .iter()
            .any(|event| event["actor"]["id"] == "worker:r1"),
        "{shown}"
    );
}
