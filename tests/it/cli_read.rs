use crate::common;
use dagq::domain::EventKind;
use dagq::domain::LeaseToken;
use dagq::infrastructure::git_binary::git_executable;

use common::cli::*;

use std::path::Path;

use dagq::infrastructure::sqlite::SqliteQueue;
use serde_json::Value;

/// A claimed run of task 1 (goal 1) with the given events, their
/// `created_at` set to 2026-09-24 at the given `HH:MM:SS` (a kind this
/// binary does not know written directly); the run's ID.
fn run_with_events(db: &Path, events: &[(&str, serde_json::Value, &str)]) -> String {
    use dagq::domain::{ClaimOutcome, CommitSha};
    common::template::queue(db);
    ok(db, &["goal", "add", "measured"]);
    ok(db, &["add", "first", "--goal", "1"]);
    ok(db, &["ready", "1", "--bypass-review"]);
    let mut queue = SqliteQueue::open(db).unwrap();
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&base, &LeaseToken::new("t"))
        .unwrap()
    else {
        panic!("nothing to claim");
    };
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "UPDATE run_events SET created_at='2026-09-24T00:00:00.000Z' WHERE run_id=?1",
        [run.id().as_str()],
    )
    .unwrap();
    for (kind, payload, at) in events {
        let before: i64 = conn
            .query_row("SELECT MAX(id) FROM run_events", [], |r| r.get(0))
            .unwrap();
        match EventKind::from_name(kind) {
            Some(kind) => queue
                .record_runtime_event(run.id(), kind, payload.clone())
                .unwrap(),
            // A kind this binary does not know is written as a newer
            // binary would: the write port takes only the kinds it knows.
            None => {
                conn.execute(
                    "INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (1,?1,?2,?3)",
                    rusqlite::params![run.id().as_str(), kind, payload.to_string()],
                )
                .unwrap();
            }
        }
        // The event and the session spans written with it (ADR-0048).
        conn.execute(
            "UPDATE run_events SET created_at=?1 WHERE id>?2",
            rusqlite::params![format!("2026-09-24T{at}.000Z"), before],
        )
        .unwrap();
    }
    run.id().as_str().to_owned()
}

#[test]
fn events_and_watch_read_past_a_cursor() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    ok(&db, &["add", "second"]);
    let cursor = ok(&db, &["status"])["cursor"].as_i64().unwrap();
    assert!(cursor >= 2);
    // Registering tasks is no attention; --all shows every kind, oldest first.
    assert_eq!(
        ok(&db, &["events", "--after", "0"]),
        serde_json::json!({"events": [], "cursor": cursor})
    );
    let all = ok(&db, &["events", "--after", "0", "--all", "--limit", "1"]);
    assert_eq!(all["events"].as_array().unwrap().len(), 1);
    assert_eq!(all["events"][0]["kind"], "task_created");
    assert_eq!(all["events"][0]["task_id"], 1);
    let first = all["cursor"].as_i64().unwrap();
    assert_eq!(first, all["events"][0]["id"].as_i64().unwrap());
    let rest = ok(&db, &["events", "--after", &first.to_string(), "--all"]);
    assert_eq!(rest["events"][0]["task_id"], 2);
    assert_eq!(rest["cursor"], cursor);
    assert!(!invoke(&db, &["events", "--limit", "0"]).status.success());

    // With nothing to report, watch times out empty with the cursor unchanged.
    let started = std::time::Instant::now();
    let quiet = ok(
        &db,
        &["watch", "--after", "0", "--timeout", "1", "--interval", "1"],
    );
    assert!(started.elapsed() >= std::time::Duration::from_secs(1));
    assert_eq!(
        quiet,
        serde_json::json!({"events": [], "supervisors_changed": false, "supervisors": [], "cursor": 0})
    );
    let quiet = ok(&db, &["watch", "--timeout", "0"]);
    assert_eq!(quiet["cursor"], cursor);
    assert!(!invoke(&db, &["watch", "--interval", "0"]).status.success());
}

#[test]
fn events_full_and_filters_narrow_what_they_read() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let run = run_with_events(
        &db,
        &[
            ("agent_started", json!({"pid": 1}), "00:00:05"),
            (
                "receipt_observed",
                json!({"path": "/r/receipt.json"}),
                "10:00:00",
            ),
            (
                "validation_finished",
                json!({"accepted": true, "receipt": {"summary": "s"}}),
                "10:00:01",
            ),
        ],
    );
    ok(&db, &["add", "other"]);
    let kinds = |value: &Value| {
        value["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };

    // --full keeps every field and the whole payload; the compact form drops the payload.
    let full = ok(&db, &["events", "--all", "--full", "--run", &run]);
    let events = full["events"].as_array().unwrap();
    assert!(events.len() >= 4);
    assert!(
        events
            .iter()
            .all(|e| e["run_id"] == run.as_str() && e["payload"].is_object())
    );
    let validated = events.last().unwrap();
    assert_eq!(validated["payload"]["receipt"]["summary"], "s");
    assert_eq!(validated["task_id"], 1);
    let compact = ok(&db, &["events", "--all", "--run", &run]);
    assert!(compact["events"][0].get("payload").is_none());

    // --kind reads that kind, attention or not; repeated, several.
    assert_eq!(
        kinds(&ok(
            &db,
            &[
                "events",
                "--kind",
                "receipt_observed",
                "--kind",
                "agent_started"
            ]
        )),
        ["agent_started", "receipt_observed"]
    );
    // Without --all or --kind, the filters still keep attention only.
    assert_eq!(
        kinds(&ok(&db, &["events", "--run", &run])),
        Vec::<String>::new()
    );
    // --task and --goal.
    let other = kinds(&ok(&db, &["events", "--all", "--task", "2"]));
    assert_eq!(other, ["task_created"]);
    // --goal reads the goal's own events and those of its tasks and runs.
    let goal = ok(&db, &["events", "--all", "--goal", "1"]);
    let goal_kinds = kinds(&goal);
    assert!(goal_kinds.contains(&"goal_created".to_owned()));
    assert!(goal_kinds.contains(&"receipt_observed".to_owned()));
    assert!(
        goal["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["task_id"] != 2)
    );
    // --since is inclusive, --until exclusive; a date is its midnight.
    assert_eq!(
        kinds(&ok(
            &db,
            &[
                "events",
                "--all",
                "--run",
                &run,
                "--since",
                "2026-09-24T00:00:05Z",
                "--until",
                "2026-09-24T10:00:01",
            ]
        )),
        // The worker's span opens with its session (ADR-0048).
        ["agent_started", "session_opened", "receipt_observed"]
    );
    assert_eq!(
        kinds(&ok(
            &db,
            &["events", "--all", "--run", &run, "--until", "2026-09-24"]
        )),
        Vec::<String>::new()
    );
    for bad in [
        "yesterday",
        "2026-09-26T1:00:00Z",
        "2026-09-26T24:00:00",
        "2026-13-01",
        "2026-09-26T10:00:00.Z",
    ] {
        let output = invoke(&db, &["events", "--since", bad]);
        assert!(!output.status.success(), "{bad}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("not a UTC time"));
    }
}

/// Bind the queue at `db` to a new Git repository under `dir` whose
/// `Cargo.toml` names the package `package` (ADR-t614-1), as a repository
/// queue's `init` binds it.
fn bind(db: &Path, dir: &Path, package: &str) {
    let repo = dir.join(format!("repo-{package}"));
    std::fs::create_dir_all(&repo).unwrap();
    let init = std::process::Command::new(git_executable().expect("git executable"))
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(init.success());
    std::fs::write(
        repo.join("Cargo.toml"),
        format!("[package]\nname = \"{package}\"\n"),
    )
    .unwrap();
    let common_dir = dagq::infrastructure::adapters::git_common_dir(&repo).unwrap();
    rusqlite::Connection::open(db)
        .unwrap()
        .execute(
            "INSERT OR REPLACE INTO queue_repository(singleton,git_common_dir) VALUES (1,?1)",
            [common_dir.to_str().unwrap()],
        )
        .unwrap();
}

/// Task 514: the work breakdown a run's session closed with is in
/// `timeline`'s heavy commands and in `stats --full`. Its cargo-only
/// counts, the claim's `rustc` and the `toolchain` axis of `kpi` are shown
/// for dagq's source only (ADR-t614-1).
#[test]
fn timeline_and_stats_show_the_work_breakdown() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let run = run_with_events(
        &db,
        &[
            ("agent_started", json!({"session_id": "s"}), "00:00:05"),
            ("session_exited", json!({"exit_code": 0}), "00:30:00"),
            ("run_integrated", json!({}), "00:40:00"),
        ],
    );
    // The breakdown the span would have recorded from its transcript.
    let work = json!({
        "total_secs": 1795, "secs": {"model": 600, "llvm_cov": 900, "idle": 295},
        "commands": {"llvm_cov": {"runs": 1, "failed": 0}},
        "verification_repeats": 1, "full_tests": 0, "llvm_cov_runs": 1,
        "heavy": [{"category": "llvm_cov", "start": "2026-09-24T00:10:00.000Z",
                   "end": "2026-09-24T00:25:00.000Z", "secs": 900,
                   "background": true, "finished": true, "failed": false}],
    });
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE run_events SET payload=json_set(payload,'$.work',json(?1)) WHERE kind='session_closed'",
        [work.to_string()],
    )
    .unwrap();
    conn.execute(
        "UPDATE run_events SET payload=json_set(payload,'$.rustc_release','1.93.0',
           '$.rustc_host','aarch64-apple-darwin') WHERE kind='run_claimed'",
        [],
    )
    .unwrap();
    bind(&db, dir.path(), "dagq");
    let timeline = ok(&db, &["timeline", &run]);
    let commands = timeline["commands"].as_array().unwrap();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["session"], "worker");
    assert_eq!(commands[0]["category"], "llvm_cov");
    assert_eq!(commands[0]["secs"], 900);
    assert_eq!(commands[0]["background"], true);
    let stats = ok(&db, &["stats", "--full"]);
    let breakdown = &stats["runs"][0]["work_breakdown"];
    assert_eq!(breakdown["secs"]["llvm_cov"], 900);
    assert_eq!(breakdown["verification_repeats"], 1);
    let overall = &stats["overall"]["work_breakdown"];
    assert_eq!(overall["runs"], 1);
    assert_eq!(overall["categories"]["llvm_cov"]["share"], json!(0.501));
    assert_eq!(overall["verification_repeats"], 1);
    assert_eq!(stats["goals"][0]["work_breakdown"]["runs"], 1);
    assert_eq!(stats["runs"][0]["rustc_release"], "1.93.0");
    assert_eq!(
        stats["versions"]["rustc"][0]["version"],
        "1.93.0 aarch64-apple-darwin"
    );
    let kpi = |db: &Path| {
        ok(
            db,
            &[
                "kpi",
                "--since",
                "2026-09-24T00:00:00Z",
                "--until",
                "2026-09-24T01:00:00Z",
                "--by",
                "toolchain",
            ],
        )
    };
    let landings = |kpi: &Value| kpi["periods"][0]["kpis"]["landings"].clone();
    assert_eq!(
        landings(&kpi(&db))["toolchain=1.93.0 aarch64-apple-darwin"]["value"],
        1.0
    );

    // Outside dagq's source none of them is shown; the rest is as before.
    bind(&db, dir.path(), "other");
    let stats = ok(&db, &["stats", "--full"]);
    let breakdown = &stats["runs"][0]["work_breakdown"];
    assert_eq!(breakdown["secs"]["model"], 600);
    let run_stats = &stats["runs"][0];
    let groups = [
        breakdown,
        &stats["overall"]["work_breakdown"],
        &stats["goals"][0]["work_breakdown"],
        &stats["versions"]["dagq"][0]["work_breakdown"],
    ];
    for group in groups {
        assert!(group.is_object(), "{group}");
        for key in [
            "verification_repeats",
            "runs_with_repeats",
            "test_with_llvm_cov",
        ] {
            assert!(group.get(key).is_none(), "{key}: {group}");
        }
    }
    assert_eq!(stats["overall"]["work_breakdown"]["runs"], 1);
    for key in ["rustc_release", "rustc_host"] {
        assert!(run_stats.get(key).is_none(), "{key}: {run_stats}");
    }
    assert!(
        stats["versions"].get("rustc").is_none(),
        "{}",
        stats["versions"]
    );
    assert_eq!(stats["versions"]["dagq"][0]["runs"], 1);
    let landings = landings(&kpi(&db));
    assert!(
        landings
            .as_object()
            .unwrap()
            .keys()
            .all(|stratum| !stratum.starts_with("toolchain=")),
        "{landings}"
    );
    assert_eq!(landings["all"]["value"], 1.0);
}

#[test]
fn timeline_names_the_long_gap_before_the_receipt() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    // Task 182's shape: the session starts, then nothing until the receipt
    // ten hours later; the run waits on a question for an hour after it.
    let run = run_with_events(
        &db,
        &[
            ("agent_started", json!({}), "00:00:05"),
            ("receipt_observed", json!({}), "10:00:00"),
            (
                "ask_opened",
                json!({"ask_id": 9, "kind": "worker_question"}),
                "10:00:10",
            ),
            (
                "ask_answered",
                json!({"ask_id": 9, "kind": "worker_question"}),
                "11:00:10",
            ),
        ],
    );
    let timeline = ok(&db, &["timeline", &run]);
    assert_eq!(timeline["run_id"], run.as_str());
    assert_eq!(timeline["task_id"], 1);
    assert_eq!(timeline["status"], "claimed");
    assert_eq!(timeline["gap_secs"], 300);
    let gaps = timeline["gaps"].as_array().unwrap();
    assert_eq!(gaps[0]["reason"], "idle");
    assert_eq!(gaps[0]["phase"], "session");
    assert_eq!(gaps[0]["confirmed"], false);
    assert_eq!(gaps[0]["secs"], 10 * 3600 - 5);
    assert_eq!(gaps[0]["from"], "2026-09-24T00:00:05.000Z");
    assert_eq!(gaps[0]["until"], "2026-09-24T10:00:00.000Z");
    assert_eq!(gaps[1]["reason"], "waiting_ask");
    assert_eq!(gaps[1]["ask_ids"], json!([9]));
    // The run is not finished: the time since its last event is a gap too.
    let last = gaps.last().unwrap();
    assert!(last["before_event"].is_null() && last["until"].is_null());
    assert_eq!(last["reason"], "after_receipt");
    assert!(timeline["events"][0].get("payload").is_none());
    let full = ok(&db, &["timeline", &run, "--full", "--gap", "7200"]);
    assert!(full["events"][0]["payload"].is_object());
    assert_eq!(full["gaps"][0]["secs"], 10 * 3600 - 5);
    assert!(!invoke(&db, &["timeline", "no-such-run"]).status.success());
    assert!(
        !invoke(&db, &["timeline", &run, "--gap", "0"])
            .status
            .success()
    );
}

/// `show` lists the asks of the task and of its runs (ADR-t451-1
/// decision 1), not the asks about no task: the latest ten oldest first
/// with their question and answer cut, all of them whole with `--full`.
#[test]
fn show_lists_the_asks_of_the_task_and_its_runs() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let run = run_with_events(&db, &[]);
    assert_eq!(ok(&db, &["show", "1"])["asks"], serde_json::json!([]));
    assert_eq!(ok(&db, &["show", "1"])["asks_total"], 0);
    let long = "q".repeat(301);
    let ask = |target: &[&str], question: &str, extra: &[&str]| {
        let mut args = vec![
            "ask",
            "--kind",
            "decide",
            "--because",
            "scope",
            "--question",
            question,
            "--option",
            "retry",
            "--option",
            "cancel",
        ];
        args.extend(target);
        args.extend(extra);
        ok(&db, &args)
    };
    // An open ask of the same kind is asked again only once answered.
    for _ in 0..11 {
        let id = ask(&["--task", "1"], &long, &[])["id"].to_string();
        ok(&db, &["answer", &id, "--text", "retry"]);
    }
    let last = ask(
        &["--run", &run],
        "which?",
        &["--recommend", "retry", "--confidence", "low"],
    );
    ok(&db, &["answer", &last["id"].to_string(), "--text", &long]);
    // A finding's blocked ask is about no task.
    let finding = ok(
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
    ok(
        &db,
        &[
            "ask",
            "--kind",
            "blocked",
            "--because",
            "scope",
            "--question",
            "stuck?",
            "--option",
            "retry",
            "--recommend",
            "retry",
            "--confidence",
            "high",
            "--finding",
            &finding,
        ],
    );

    let full = ok(&db, &["show", "1", "--full"]);
    let all = full["asks"].as_array().unwrap();
    assert_eq!(all.len(), 12, "{full}");
    assert_eq!(all[0]["task_id"], 1);
    assert_eq!(all[0]["question"], long.as_str());
    assert_eq!(all[0]["recommendation"], Value::Null);
    assert_eq!(all[0]["confidence"], Value::Null);
    assert_eq!(all[0]["answer"], "retry");
    assert_eq!(all[11]["id"], last["id"]);
    assert_eq!(all[11]["run_id"], run.as_str());
    assert_eq!(all[11]["recommendation"], "retry");
    assert_eq!(all[11]["confidence"], "low");
    assert_eq!(all[11]["answer"], long.as_str());
    assert!(all.iter().all(|ask| ask.get("truncated").is_none()));

    let shown = ok(&db, &["show", "1"]);
    assert_eq!(shown["asks_total"], 12);
    let latest = shown["asks"].as_array().unwrap();
    assert_eq!(latest.len(), 10);
    assert_eq!(latest[0]["id"], all[2]["id"]);
    let question = latest[0]["question"].as_str().unwrap();
    assert!(question.ends_with('…'), "{question}");
    assert_eq!(question.chars().count(), 301);
    assert_eq!(latest[0]["truncated"], true);
    assert_eq!(latest[9]["id"], last["id"]);
    assert_eq!(latest[9]["question"], "which?");
    assert_eq!(latest[9]["recommendation"], "retry");
    assert_eq!(latest[9]["confidence"], "low");
    let answer = latest[9]["answer"].as_str().unwrap();
    assert!(answer.ends_with('…'), "{answer}");
    assert_eq!(answer.chars().count(), 301);
    assert_eq!(latest[9]["truncated"], true);
}

#[test]
fn show_goal_show_and_doctor_are_compact_unless_full() {
    let (_dir, db) = queue();
    let long = "長".repeat(400);
    ok(
        &db,
        &[
            "goal",
            "add",
            "compact",
            "--description",
            &long,
            "--acceptance",
            "short",
        ],
    );
    ok(
        &db,
        &[
            "add",
            "task",
            "--goal",
            "1",
            "--context",
            &long,
            "--verify",
            "true",
        ],
    );
    // Draft and ready back and forth: one event each, twelve in all with `task_created`.
    for _ in 0..6 {
        ok(&db, &["ready", "1", "--bypass-review"]);
        ok(&db, &["draft", "1"]);
    }
    ok(&db, &["goal", "edit", "1", "--constraints", &long]);

    let full = ok(&db, &["show", "1", "--full"]);
    assert_eq!(full["task"]["context"], long.as_str());
    assert!(full["task"].get("truncated").is_none());
    let all_events = full["events"].as_array().unwrap().len();
    assert!(all_events > 10);
    assert!(full["events"][0]["payload"].is_object());
    assert!(full.get("events_total").is_none());

    let shown = ok(&db, &["show", "1"]);
    let context = shown["task"]["context"].as_str().unwrap();
    assert!(context.ends_with('…'), "{context}");
    assert_eq!(context.chars().count(), 301);
    assert_eq!(shown["task"]["truncated"], true);
    assert_eq!(
        shown["task"]["verification_commands"],
        serde_json::json!(["true"])
    );
    assert_eq!(shown["runs"], serde_json::json!([]));
    assert_eq!(shown["events_total"], all_events);
    let events = shown["events"].as_array().unwrap();
    assert_eq!(events.len(), 10);
    assert_eq!(events[9]["id"], full["events"][all_events - 1]["id"]);
    assert_eq!(
        events[9]["payload"],
        serde_json::json!({"from": "ready", "to": "draft"})
    );
    let three = ok(&db, &["show", "1", "--events", "3"]);
    assert_eq!(three["events"].as_array().unwrap().len(), 3);
    assert!(
        !invoke(&db, &["show", "1", "--full", "--events", "3"])
            .status
            .success()
    );

    let full = ok(&db, &["goal", "show", "1", "--full"]);
    assert_eq!(full["goal"]["description"], long.as_str());
    assert!(full["events"][0]["payload"].is_object());
    let goal = ok(&db, &["goal", "show", "1"]);
    assert_eq!(goal["goal"]["truncated"], true);
    assert_eq!(goal["goal"]["acceptance"], "short");
    assert!(goal["goal"]["description"].as_str().unwrap().ends_with('…'));
    assert!(goal["goal"]["constraints"].as_str().unwrap().ends_with('…'));
    assert_eq!(
        goal["tasks"],
        serde_json::json!([{"id": 1, "title": "task", "status": "draft", "priority": "normal",
            "priority_source": "goal"}])
    );
    assert_eq!(
        goal["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["goal_created", "goal_updated"]
    );
    assert!(goal["events"][0].get("payload").is_none());

    for args in [&["doctor"][..], &["doctor", "--full"]] {
        let report = ok(&db, args);
        assert_eq!(report["runs"], serde_json::json!([]), "{args:?}");
        assert_eq!(report["supervisors"], serde_json::json!([]), "{args:?}");
    }
}

#[test]
fn search_prints_hits_with_excerpts_and_checks_its_filters() {
    let (_dir, db) = queue();
    ok(
        &db,
        &[
            "goal",
            "add",
            "重複を見つける",
            "--acceptance",
            "上位 5 件に出る",
        ],
    );
    ok(
        &db,
        &[
            "add",
            "runtime: 全文検索",
            "--goal",
            "1",
            "--description",
            "FTS5 の trigram",
        ],
    );
    ok(&db, &["add", "unrelated"]);
    ok(&db, &["cancel", "2"]);
    ok(&db, &["note", "--task", "1", "--text", "全文検索のメモ"]);
    let found = ok(&db, &["search", "全文検索"]);
    assert_eq!(found["total"], 2, "{found}");
    let kinds: Vec<&str> = found["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds.len(), 2);
    assert!(kinds.contains(&"task") && kinds.contains(&"note"));
    let goal = ok(
        &db,
        &["search", "上位", "--kind", "goal", "--status", "open"],
    );
    assert_eq!(
        goal["hits"][0],
        serde_json::json!({
            "kind": "goal", "id": 1, "status": "open", "title": "重複を見つける",
            "field": "acceptance", "excerpt": "«上位» 5 件に出る", "tags": [],
        })
    );
    let full = ok(
        &db,
        &[
            "search", "trigram", "--goal", "1", "--kind", "task", "--full", "--limit", "1",
        ],
    );
    assert_eq!(full["hits"][0]["fields"]["description"], "FTS5 の trigram");
    assert!(full["hits"][0]["score"].is_number(), "{full}");
    let canceled = ok(
        &db,
        &["search", "unrelated", "--status", "canceled,completed"],
    );
    assert_eq!(canceled["hits"][0]["id"], 2);
    assert_eq!(
        refused(&db, &["search", "x", "--status", "closed"]),
        "unknown status: closed"
    );
    assert!(refused(&db, &["search", "AND"]).contains("the query has no term"));
    assert!(refused(&db, &["search", "trigram OR"]).starts_with("invalid search query: fts5"));
    // Reading, so the observer and the headless reviewer may search.
    ok_as("observer", &db, &["search", "全文検索"]);
    ok_as("reviewer", &db, &["search", "全文検索"]);
}

#[test]
fn related_prints_candidates_with_their_clues() {
    let (_dir, db) = queue();
    ok(
        &db,
        &[
            "add",
            "test: a_run_parked_again_after_a_skip が負荷下で落ちる",
            "--description",
            "tests/runtime.rs の assert が落ちた",
        ],
    );
    ok(
        &db,
        &[
            "add",
            "test: a_run_parked_again_after_a_skip fails under load",
            "--description",
            "tests/runtime.rs again",
        ],
    );
    ok(&db, &["add", "unrelated"]);
    // A title with FTS5's syntax in it still makes a valid query.
    ok(&db, &["add", "fix: (a*b) ^c \"d\" NOT x OR y:z"]);
    assert_eq!(ok(&db, &["related", "4"])["total"], 0);
    let related = ok(&db, &["related", "1"]);
    assert_eq!(related["task_id"], 1);
    assert_eq!(related["total"], 1, "{related}");
    let candidate = &related["related"][0];
    assert_eq!(candidate["id"], 2);
    assert_eq!(candidate["status"], "draft");
    assert!(candidate["score"].as_f64().unwrap() > 0.0);
    let clues: Vec<(&str, &str)> = candidate["clues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["clue"].as_str().unwrap(), c["value"].as_str().unwrap()))
        .collect();
    assert!(
        clues.contains(&("test", "a_run_parked_again_after_a_skip")),
        "{clues:?}"
    );
    assert!(clues.contains(&("file", "tests/runtime.rs")), "{clues:?}");
    let none = ok(
        &db,
        &[
            "related",
            "1",
            "--status",
            "ready,completed",
            "--limit",
            "1",
        ],
    );
    assert_eq!(none["related"], serde_json::json!([]));
    assert!(
        !invoke(&db, &["related", "1", "--status", "closed"])
            .status
            .success()
    );
    assert_eq!(refused(&db, &["related", "9"]), "task 9 does not exist");
    ok_as("observer", &db, &["related", "1"]);
    ok_as("reviewer", &db, &["related", "1"]);
}

#[test]
fn status_watch_and_show_read_kinds_a_newer_binary_wrote() {
    // ADR-0073 decision 21: a kind this binary does not know is shown, not
    // an error; its ask is a generic one a person answers.
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    let cursor = ok(&db, &["status"])["cursor"].as_i64().unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "INSERT INTO asks(kind,task_id,question,options,asked_by,reason_category)
         VALUES ('future_kind',1,'Which way?','[\"left\",\"right\"]','supervisor','scope');
         INSERT INTO run_events(task_id,kind,payload)
         VALUES (1,'ask_opened','{\"ask_id\":1,\"kind\":\"future_kind\"}');
         INSERT INTO run_events(task_id,kind,payload) VALUES (1,'future_task_event','{}');
         INSERT INTO run_events(kind,payload) VALUES ('future_queue_event','{}');",
    )
    .unwrap();
    let status = ok(&db, &["status"]);
    assert_eq!(status["asks"][0]["kind"], "future_kind");
    assert_eq!(status["asks"][0]["question"], "Which way?");
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["ask_id"] == 1 && a["next"] == "answer ask 1")
    );
    let watch = ok(
        &db,
        &["watch", "--after", &cursor.to_string(), "--timeout", "0"],
    );
    assert_eq!(watch["events"][0]["ask_id"], 1);
    assert_eq!(watch["cursor"], cursor + 3);
    let show = ok(&db, &["show", "1", "--full"]);
    let kinds: Vec<&str> = show["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["task_created", "ask_opened", "future_task_event"]);
    let events = ok(&db, &["events", "--after", &cursor.to_string(), "--all"]);
    assert_eq!(events["events"][2]["kind"], "future_queue_event");
    // Its answer is recorded for a person to read; nothing applies it.
    ok(&db, &["answer", "1", "--text", "left"]);
    let answered = ok(&db, &["status"]);
    assert!(
        answered["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| { a["ask_id"] == 1 && a["next"] == "read the answer of ask 1 and close it" })
    );

    // A run's timeline and the stats pass an unknown event by.
    let timeline_dir = tempfile::tempdir().unwrap();
    let db = timeline_dir.path().join("queue.db");
    let run = run_with_events(
        &db,
        &[
            (
                "agent_started",
                serde_json::json!({"session_id": "s"}),
                "00:00:05",
            ),
            ("future_run_event", serde_json::json!({"x": 1}), "00:10:00"),
        ],
    );
    ok(&db, &["timeline", &run]);
    ok(&db, &["stats", "--full"]);
}

/// `status` and `doctor` name each AI actor's backend and enforcement: on
/// the host, advisory and not sandboxed (goal 55); the control plane and
/// the user are not AI actors and are not listed.
#[test]
fn status_and_doctor_show_every_actor_on_the_host_advisory() {
    let (_dir, db) = queue();
    for args in [&["status"][..], &["doctor"], &["doctor", "--full"]] {
        let report = ok(&db, args);
        let actors = report["actors"].as_array().unwrap();
        let roles: Vec<&str> = actors
            .iter()
            .map(|actor| actor["role"].as_str().unwrap())
            .collect();
        assert_eq!(
            roles,
            [
                "inbox",
                "planner",
                "worker",
                "review-job",
                "recovery-job",
                "plan-review-job",
                "goal-review-job",
                "throughput-review-job",
                "observer",
            ],
            "{args:?}"
        );
        for actor in actors {
            assert_eq!(actor["backend"], "host", "{args:?}");
            assert_eq!(actor["enforcement"], "advisory", "{args:?}");
            assert_eq!(actor["sandboxed"], false, "{args:?}");
        }
    }
}

/// With a supervisor that can run a Codex worker, the worker's row lists
/// each provider: Claude advisory, Codex `confined` by its OS sandbox and
/// not isolated (ADR-t813-3 decision 7); the row itself and the other
/// roles stay advisory. A supervisor whose `codex` is missing leaves the
/// rows without `providers`, as with none, and so does a supervisor that
/// is not alive.
#[test]
fn status_and_doctor_show_the_codex_worker_confined() {
    use dagq::domain::{
        Provider,
        worker::{ProviderCheck, WorkerMode},
    };
    let (_dir, db) = queue();
    let check = |provider, found, modes: &[WorkerMode]| ProviderCheck {
        provider,
        executable: format!("/bin/{}", Provider::as_str(provider)),
        found,
        error: None,
        modes: modes.to_vec(),
    };
    let token = LeaseToken::new("t");
    let mut store = SqliteQueue::open(&db).unwrap();
    store
        .register_supervisor(&token, std::process::id(), 1, "v")
        .unwrap();
    let before = ok(&db, &["status"])["actors"].clone();
    // A registration a dead supervisor left behind does not count.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let stale = LeaseToken::new("stale");
    store.register_supervisor(&stale, dead, 1, "v").unwrap();
    store
        .set_supervisor_providers(
            &stale,
            &[check(Provider::Codex, true, &[WorkerMode::Headless])],
        )
        .unwrap();
    assert_eq!(ok(&db, &["status"])["actors"], before, "dead supervisor");
    store
        .set_supervisor_providers(
            &token,
            &[
                check(
                    Provider::Claude,
                    true,
                    &[WorkerMode::Interactive, WorkerMode::Headless],
                ),
                check(Provider::Codex, false, &[WorkerMode::Headless]),
            ],
        )
        .unwrap();
    assert_eq!(ok(&db, &["status"])["actors"], before, "codex missing");
    store
        .set_supervisor_providers(
            &token,
            &[
                check(
                    Provider::Claude,
                    true,
                    &[WorkerMode::Interactive, WorkerMode::Headless],
                ),
                check(Provider::Codex, true, &[WorkerMode::Headless]),
            ],
        )
        .unwrap();
    for args in [&["status"][..], &["doctor"], &["doctor", "--full"]] {
        let report = ok(&db, args);
        for actor in report["actors"].as_array().unwrap() {
            assert_eq!(actor["enforcement"], "advisory", "{args:?} {actor}");
            assert_eq!(actor["sandboxed"], false, "{args:?} {actor}");
            if actor["role"] != "worker" {
                assert!(actor.get("providers").is_none(), "{args:?} {actor}");
                continue;
            }
            assert_eq!(
                actor["providers"],
                serde_json::json!([
                    {"provider": "claude", "modes": ["interactive", "headless"],
                     "enforcement": "advisory", "sandboxed": false},
                    {"provider": "codex", "modes": ["headless"],
                     "enforcement": "confined", "sandboxed": false},
                ]),
                "{args:?}"
            );
        }
    }
}

/// `ask_request_taken` is no longer written (ADR-t1233-5 decision 5), but
/// a queue that has such events from the run directory's ask requests of
/// earlier Codex turns is still read by `events`, `show` and `stats`.
#[test]
fn past_ask_request_taken_events_are_still_read() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let run = run_with_events(
        &db,
        &[
            (
                "ask_request_taken",
                serde_json::json!({"request": "r1", "outcome": "refused", "reason": "malformed"}),
                "00:00:05",
            ),
            (
                "ask_request_taken",
                serde_json::json!({"request": "r2", "outcome": "opened", "ask_id": 7, "created": true}),
                "00:00:06",
            ),
        ],
    );
    let events = ok(&db, &["events", "--after", "0", "--all", "--run", &run]);
    let taken: Vec<&Value> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "ask_request_taken")
        .collect();
    assert_eq!(taken.len(), 2, "{events}");
    let show = ok(&db, &["show", "1", "--full"]);
    let shown = show["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "ask_request_taken")
        .count();
    assert_eq!(shown, 2, "{show}");
    ok(&db, &["stats", "--full"]);
}
