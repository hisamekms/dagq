//! The asks, answers, notes, marks and findings are authorized in the
//! application before they change the queue (ADR-t728-1 decision 5,
//! ADR-t728-3, task 733): a worker asks on its own run only, only the user
//! and the inbox answer, the answer records whose authority it carries
//! (the user's own or the inbox's delegated one) and whether it approves,
//! and the observer still records findings and raises blocked asks.

use crate::common;

use common::cli::*;

use serde_json::{Value, json};
use std::path::Path;

fn allowed_as(env: &[(&str, &str)], db: &Path, args: &[&str]) -> Value {
    let output = invoke_with(env, db, args);
    assert!(
        output.status.success(),
        "{env:?} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The error JSON `args` is refused with.
fn denied_as(env: &[(&str, &str)], db: &Path, args: &[&str]) -> Value {
    let output = invoke_with(env, db, args);
    assert!(!output.status.success(), "{env:?} {args:?} was allowed");
    serde_json::from_slice(&output.stderr).unwrap()
}

fn events(db: &Path, kind: &str) -> Vec<Value> {
    ok(
        db,
        &["events", "--after", "0", "--all", "--full", "--kind", kind],
    )["events"]
        .as_array()
        .unwrap()
        .clone()
}

fn ask(kind: &str, target: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "ask",
        "--kind",
        kind,
        "--because",
        "scope",
        "--question",
        "which?",
        "--option",
        "retry",
        "--option",
        "cancel",
    ]
    .map(str::to_owned)
    .to_vec();
    if kind == "worker_question" {
        args.extend(["--topic".to_owned(), "task_overlap".to_owned()]);
    }
    args.extend(target.iter().map(|arg| (*arg).to_owned()));
    args
}

fn strs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

const WORKER: [(&str, &str); 4] = [
    ("DAGQ_ROLE", "worker"),
    ("DAGQ_ACTOR_ID", "worker:r1"),
    ("DAGQ_RUN_ID", "r1"),
    ("DAGQ_TASK_ID", "1"),
];

#[test]
fn a_worker_asks_on_its_own_run_and_task_only() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    ok(&db, &["add", "second"]);
    for (args, reason) in [
        (
            ask("worker_question", &["--run", "r2"]),
            "not on this resource",
        ),
        (
            ask("worker_question", &["--task", "2"]),
            "not on this resource",
        ),
        (ask("decide", &["--run", "r1"]), "not of this kind"),
        (ask("approve_landing", &["--task", "1"]), "not of this kind"),
    ] {
        let error = denied_as(&WORKER, &db, &strs(&args));
        assert_eq!(
            error["error"],
            format!("worker may not ask.open ({reason})"),
            "{args:?}"
        );
        assert_eq!(error["denied"]["capability"], "ask.open");
    }
    assert!(
        ok(&db, &["asks", "--all"])["asks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let denied = events(&db, "authorization_denied");
    assert_eq!(denied.len(), 4);
    assert_eq!(
        denied[0]["payload"]["resource"],
        json!({"kind": "new_ask", "ask_kind": "worker_question", "run": "r2", "task": null})
    );
    assert_eq!(denied[0]["actor"]["id"], "worker:r1");
    // Its own task it asks about.
    let asked = allowed_as(
        &WORKER,
        &db,
        &strs(&ask("worker_question", &["--task", "1"])),
    );
    assert_eq!(asked["asked_by"], "worker");
}

#[test]
fn workers_jobs_the_observer_and_the_planner_neither_answer_nor_close() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    let open = ok(&db, &strs(&ask("decide", &["--task", "1"])))["id"].to_string();
    let answered = ok(&db, &strs(&ask("approve_landing", &["--task", "1"])))["id"].to_string();
    ok(&db, &["answer", &answered, "--text", "retry"]);
    let mut actors: Vec<Vec<(&str, &str)>> = vec![
        WORKER.to_vec(),
        vec![("DAGQ_ROLE", "planner"), ("DAGQ_ACTOR_ID", "planner:1")],
    ];
    for role in [
        "review-job",
        "recovery-job",
        "plan-review-job",
        "goal-review-job",
        "throughput-review-job",
        "observer",
    ] {
        actors.push(vec![("DAGQ_ROLE", role)]);
    }
    let mut refused = 0;
    for env in &actors {
        for (args, capability) in [
            (vec!["answer", &open, "--text", "retry"], "ask.answer"),
            (vec!["ask", "close", &answered], "ask.close"),
        ] {
            let error = denied_as(env, &db, &args);
            assert_eq!(error["denied"]["capability"], capability, "{env:?}");
            assert_eq!(error["denied"]["reason"], "not granted", "{env:?}");
            refused += 1;
        }
    }
    let asks = ok(&db, &["asks", "--all"])["asks"].clone();
    assert!(asks[0]["answer"].is_null(), "{asks}");
    assert!(asks[1]["closed_at"].is_null(), "{asks}");
    let denied = events(&db, "authorization_denied");
    assert_eq!(denied.len(), refused);
    // Refused before the ask was read.
    assert_eq!(
        denied[0]["payload"]["resource"],
        json!({"kind": "ask", "id": 1, "run": null})
    );
}

#[test]
fn the_user_and_the_inbox_answer_and_the_record_tells_them_apart() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    let inbox = [("DAGQ_ROLE", "inbox"), ("DAGQ_ACTOR_ID", "inbox")];
    let by_user = ok(&db, &strs(&ask("decide", &["--task", "1"])))["id"].to_string();
    let by_inbox = ok(&db, &strs(&ask("approve_landing", &["--task", "1"])))["id"].to_string();
    let question = allowed_as(
        &inbox,
        &db,
        &strs(&ask("worker_question", &["--task", "1"])),
    );
    let plain = question["id"].to_string();

    let user_answer = ok(&db, &["answer", &by_user, "--text", "retry"]);
    let inbox_answer = allowed_as(&inbox, &db, &["answer", &by_inbox, "--text", "cancel"]);
    let plain_answer = allowed_as(&inbox, &db, &["answer", &plain, "--text", "go on"]);
    // The row: `answered_by` and `asked_by` keep their values; the
    // authority and the approval are new.
    assert_eq!(user_answer["answered_by"], "person");
    assert_eq!(user_answer["answer_authority"], "user");
    assert_eq!(user_answer["answer_approval"], true);
    assert_eq!(user_answer["asked_by"], "human");
    assert_eq!(inbox_answer["answered_by"], "inbox");
    assert_eq!(inbox_answer["answer_authority"], "delegated");
    assert_eq!(inbox_answer["answer_approval"], true);
    assert_eq!(plain_answer["answer_authority"], "delegated");
    assert_eq!(plain_answer["answer_approval"], false);
    assert_eq!(question["asked_by"], "inbox");

    let answered = events(&db, "ask_answered");
    let of = |id: &str| {
        answered
            .iter()
            .find(|event| event["payload"]["ask_id"] == id.parse::<i64>().unwrap())
            .unwrap()
            .clone()
    };
    let user = of(&by_user);
    assert_eq!(user["payload"]["authority"], "user");
    assert_eq!(user["payload"]["approval"], true);
    assert_eq!(user["payload"]["answered_by"], "person");
    assert_eq!(user["actor"], json!({"role": "user", "id": "user"}));
    let delegated = of(&by_inbox);
    assert_eq!(delegated["payload"]["authority"], "delegated");
    assert_eq!(delegated["payload"]["approval"], true);
    assert_eq!(delegated["actor"], json!({"role": "inbox", "id": "inbox"}));
    assert_eq!(of(&plain)["payload"]["approval"], false);
    // The inbox closes what it read.
    allowed_as(&inbox, &db, &["ask", "close", &by_inbox]);
    assert!(events(&db, "authorization_denied").is_empty());
}

#[test]
fn the_observer_still_records_findings_and_raises_blocked_asks() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    let observer = [("DAGQ_ROLE", "observer"), ("DAGQ_ACTOR_ID", "observer:s1")];
    let finding = allowed_as(
        &observer,
        &db,
        &[
            "finding",
            "record",
            "--kind",
            "stall",
            "--queue",
            "--summary",
            "waits",
        ],
    );
    assert_eq!(finding["recorded_by"], "observer");
    let id = finding["id"].to_string();
    let blocked = allowed_as(&observer, &db, &strs(&ask("blocked", &["--finding", &id])));
    assert_eq!(blocked["asked_by"], "observer");
    assert_eq!(blocked["created"], true);
    allowed_as(
        &observer,
        &db,
        &["finding", "resolve", &id, "--reason", "gone"],
    );
    // Not another kind, a note, a mark or a dismissal.
    for args in [
        strs(&ask("decide", &["--task", "1"])),
        vec!["note", "--task", "1", "--text", "x"],
        vec!["mark", "label"],
        vec!["finding", "dismiss", &id, "--reason", "no"],
    ] {
        let error = denied_as(&observer, &db, &args);
        assert_eq!(
            error["error"], "observer may not change queue state",
            "{args:?}"
        );
    }
    // A person answers the observer's blocked ask with `dismiss`, an
    // approval that the runtime applies to the finding.
    let answered = ok(
        &db,
        &["answer", &blocked["id"].to_string(), "--text", "dismiss"],
    );
    assert_eq!(answered["answer_approval"], true);
    assert_eq!(answered["answer_authority"], "user");
}

/// A worker_question carries what it left undecided (ADR-t947-2): the
/// primary topic first, kept in the row, `ask_opened` and `status`' asks;
/// one is required, other kinds carry none, and a code outside the list is
/// kept as given.
#[test]
fn a_worker_question_records_its_topics_primary_first() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    let mut args = ask("decide", &["--task", "1"]);
    args[2] = "worker_question".to_owned();
    let missing = denied_as(&WORKER, &db, &strs(&args));
    assert!(
        missing["error"]
            .as_str()
            .unwrap()
            .contains("a worker_question needs --topic"),
        "{missing}"
    );
    let mut on_decide = ask("decide", &["--task", "1"]);
    on_decide.extend(["--topic".to_owned(), "task_overlap".to_owned()]);
    let refused = invoke(&db, &strs(&on_decide));
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("only a worker_question carries --topic, not decide"),
    );

    args.extend(
        [
            "--topic",
            " adr_conflict ",
            "--topic",
            "new_code",
            "--topic",
            "adr_conflict",
        ]
        .map(str::to_owned),
    );
    let asked = allowed_as(&WORKER, &db, &strs(&args));
    assert_eq!(asked["topics"], json!(["adr_conflict", "new_code"]));
    let decided = ok(&db, &strs(&ask("decide", &["--task", "1"])));
    assert!(decided.get("topics").is_none(), "{decided}");

    let opened = events(&db, "ask_opened");
    assert_eq!(
        opened[0]["payload"]["topics"],
        json!(["adr_conflict", "new_code"])
    );
    assert_eq!(opened[0]["payload"]["reason_category"], "scope");
    assert!(
        opened[1]["payload"].get("topics").is_none(),
        "{}",
        opened[1]
    );
    let status = ok(&db, &["status"]);
    let asks = status["asks"].as_array().unwrap();
    assert_eq!(
        asks[0]["topics"],
        json!(["adr_conflict", "new_code"]),
        "{status}"
    );
    assert!(asks[1].get("topics").is_none(), "{status}");
    let all = ok(&db, &["asks", "--all"]);
    assert_eq!(
        all["asks"][0]["topics"],
        json!(["adr_conflict", "new_code"])
    );
}

/// An ask carries the asking AI's recommendation and confidence
/// (ADR-t451-1 decision 1): `--recommend` must be one of its options (the
/// runtime's `propose` / `dismiss` on a finding's blocked ask too), both
/// are optional on every kind, and both reach the ask, `ask_opened`,
/// `asks`, `show`, `status --role inbox` and `watch --role inbox` (null
/// without them); `stats` counts how often the answers chose it.
#[test]
fn a_recommendation_reaches_the_ask_status_and_stats() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    let with = |kind: &str, target: &[&str], extra: &[&str]| {
        let mut args = ask(kind, target);
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        args
    };
    let refused = invoke(
        &db,
        &strs(&with("decide", &["--task", "1"], &["--recommend", "land"])),
    );
    assert!(!refused.status.success());
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(
        error.contains(r#"--recommend \"land\" is none of the ask's options"#)
            && error.contains("retry"),
        "{error}"
    );
    let bad = invoke(
        &db,
        &strs(&with(
            "decide",
            &["--task", "1"],
            &["--confidence", "medium"],
        )),
    );
    assert!(!bad.status.success());

    let cursor = ok(&db, &["status"])["cursor"].as_i64().unwrap();
    let recommended = ok(
        &db,
        &strs(&with(
            "decide",
            &["--task", "1"],
            &["--recommend", " retry ", "--confidence", "low"],
        )),
    );
    assert_eq!(recommended["recommendation"], "retry");
    assert_eq!(recommended["confidence"], "low");
    // A worker_question needs neither; one alone is kept.
    let question = allowed_as(
        &WORKER,
        &db,
        &strs(&with("worker_question", &["--task", "1"], &[])),
    );
    assert_eq!(question["recommendation"], Value::Null);
    assert_eq!(question["confidence"], Value::Null);
    let landing = ok(
        &db,
        &strs(&with(
            "approve_landing",
            &["--task", "1"],
            &["--confidence", "high"],
        )),
    );
    assert_eq!(landing["recommendation"], Value::Null);
    assert_eq!(landing["confidence"], "high");
    // The observer may recommend the option the runtime adds.
    let observer = [("DAGQ_ROLE", "observer"), ("DAGQ_ACTOR_ID", "observer:s1")];
    let finding = allowed_as(
        &observer,
        &db,
        &[
            "finding",
            "record",
            "--kind",
            "stall",
            "--queue",
            "--summary",
            "waits",
        ],
    )["id"]
        .to_string();
    let blocked = allowed_as(
        &observer,
        &db,
        &strs(&with(
            "blocked",
            &["--finding", &finding],
            &["--recommend", "dismiss", "--confidence", "high"],
        )),
    );
    assert_eq!(blocked["recommendation"], "dismiss");

    let opened = events(&db, "ask_opened");
    assert_eq!(opened[0]["payload"]["recommendation"], "retry");
    assert_eq!(opened[0]["payload"]["confidence"], "low");
    assert_eq!(opened[1]["payload"]["recommendation"], Value::Null);
    assert_eq!(opened[1]["payload"]["confidence"], Value::Null);
    let status = ok(&db, &["status", "--role", "inbox"]);
    let asks = status["asks"].as_array().unwrap();
    assert_eq!(asks[0]["recommendation"], "retry", "{status}");
    assert_eq!(asks[0]["confidence"], "low", "{status}");
    assert_eq!(asks[1]["recommendation"], Value::Null, "{status}");
    assert_eq!(asks[1]["confidence"], Value::Null, "{status}");
    let listed = ok(&db, &["asks", "--open", "--role", "inbox"]);
    assert_eq!(listed["asks"][0]["recommendation"], "retry");
    assert_eq!(listed["asks"][2]["confidence"], "high");
    let watch = ok(
        &db,
        &[
            "watch",
            "--role",
            "inbox",
            "--after",
            &cursor.to_string(),
            "--timeout",
            "0",
        ],
    );
    let watched: Vec<&Value> = watch["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "ask_opened")
        .collect();
    assert_eq!(watched[0]["recommendation"], "retry", "{watch}");
    assert_eq!(watched[0]["confidence"], "low", "{watch}");
    assert_eq!(watched[1]["recommendation"], Value::Null, "{watch}");
    let show = ok(&db, &["show", "1", "--full"]);
    assert!(
        show["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "ask_opened"
                && event["payload"]["recommendation"] == "retry"
                && event["payload"]["confidence"] == "low"),
        "{show}"
    );

    // One answer chooses the recommendation and one does not.
    ok(
        &db,
        &["answer", &recommended["id"].to_string(), "--text", "retry"],
    );
    ok(
        &db,
        &["answer", &blocked["id"].to_string(), "--text", "propose"],
    );
    let stats = ok(&db, &["stats", "--full"]);
    assert_eq!(
        stats["recommendations"]["by_kind"],
        json!({
            "decide": {"answered": 1, "matched": 1, "rate": 1.0},
            "blocked": {"answered": 1, "matched": 0, "rate": 0.0},
        }),
        "{}",
        stats["recommendations"]
    );
    assert_eq!(stats["recommendations"]["decided_without_ask"], json!({}));
}

/// `send_back: <reason>` to an approve_landing recommending `send_back`
/// counts as choosing it (task 1389): `answer` records the option before
/// the reason as `reasoned_option`, leaving `option_index` and `option` to
/// an exact choice, and `stats` matches it. The same form of a `decide`
/// answer stays free.
#[test]
fn a_send_back_with_a_reason_matches_the_recommendation_in_stats() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    let mut landing = ask("approve_landing", &["--task", "1"]);
    landing.extend(["--option", "send_back", "--recommend", "send_back"].map(str::to_owned));
    let landing = ok(&db, &strs(&landing))["id"].to_string();
    let mut decide = ask("decide", &["--task", "1"]);
    decide.extend(["--recommend", "retry"].map(str::to_owned));
    let decide = ok(&db, &strs(&decide))["id"].to_string();
    ok(
        &db,
        &["answer", &landing, "--text", " send_back : split it"],
    );
    ok(&db, &["answer", &decide, "--text", "retry: once more"]);

    let answered = events(&db, "ask_answered");
    assert_eq!(answered[0]["payload"]["reasoned_option"], "send_back");
    assert_eq!(answered[0]["payload"]["option_index"], Value::Null);
    assert_eq!(answered[0]["payload"]["option"], Value::Null);
    assert_eq!(answered[1]["payload"]["reasoned_option"], Value::Null);
    let stats = ok(&db, &["stats", "--full"]);
    assert_eq!(
        stats["recommendations"]["by_kind"],
        json!({
            "approve_landing": {"answered": 1, "matched": 1, "rate": 1.0},
            "decide": {"answered": 1, "matched": 0, "rate": 0.0},
        }),
        "{}",
        stats["recommendations"]
    );
}

/// A Codex worker's `dagq ask` (ADR-t813-3 decision 3): with its turn's
/// `DAGQ_ASK_REQUESTS`, an ask the queue's path would open is written there
/// as a request and no queue is opened; one it would refuse (another run, a
/// reason of the queue's, no topic, a finding, a kind that is not the
/// worker's) is refused at once and writes nothing.
#[test]
fn a_codex_workers_ask_is_checked_then_written_to_its_run_directory() {
    let dir = tempfile::tempdir().unwrap();
    let requests = dir.path().join("run/ask-requests");
    let db = dir.path().join("no-queue/queue.db");
    let env = [
        ("DAGQ_ROLE", "worker"),
        ("DAGQ_ACTOR_ID", "worker:r1"),
        ("DAGQ_RUN_ID", "r1"),
        ("DAGQ_TASK_ID", "3"),
        (
            dagq::domain::ask_request::ASK_REQUESTS_ENV,
            requests.to_str().unwrap(),
        ),
    ];
    let question = |run: &str, kind: &str, because: &str, more: &[&'static str]| {
        let mut args = vec![
            "ask".to_owned(),
            "--run".to_owned(),
            run.to_owned(),
            "--kind".to_owned(),
            kind.to_owned(),
            "--because".to_owned(),
            because.to_owned(),
            "--question".to_owned(),
            "which?".to_owned(),
        ];
        args.extend(more.iter().map(|arg| (*arg).to_owned()));
        args
    };
    let asked = allowed_as(
        &env,
        &db,
        &strs(&question(
            "r1",
            "worker_question",
            "scope",
            &["--topic", "other"],
        )),
    );
    assert_eq!(asked["requested"], true, "{asked}");
    let id = asked["request"].as_str().unwrap();
    let written: Value =
        serde_json::from_slice(&std::fs::read(requests.join(format!("{id}.json"))).unwrap())
            .unwrap();
    assert_eq!(written["run_id"], "r1");
    assert_eq!(written["question"], "which?");
    for (args, says) in [
        (
            question("r2", "worker_question", "scope", &["--topic", "other"]),
            "worker may not ask.open",
        ),
        (
            question("r1", "approve_landing", "scope", &[]),
            "worker may not ask.open",
        ),
        (
            question("r1", "worker_question", "cost", &["--topic", "other"]),
            "cost",
        ),
        (question("r1", "worker_question", "scope", &[]), "--topic"),
        (
            question(
                "r1",
                "worker_question",
                "scope",
                &["--topic", "other", "--finding", "1"],
            ),
            "finding",
        ),
    ] {
        let error = denied_as(&env, &db, &strs(&args));
        let text = error["error"].as_str().unwrap_or_default();
        assert!(text.contains(says), "{args:?}: {error}");
    }
    assert_eq!(std::fs::read_dir(&requests).unwrap().count(), 1);
    assert!(!db.exists());
}
