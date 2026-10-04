//! Runtime tests: the work breakdown of a headless Codex worker's session
//! (task 1354). Codex has no transcript: the session wrapper stamps when it
//! reads each `command_execution` item's start and end, writes them to the
//! turn's commands file, and the span's close makes the same `work` and
//! `worktime.jsonl` lines a Claude span's transcript gives.
use crate::common;
use crate::runtime_codex::{FINISH, codex_fixture, detail, finished, supervise_thread};
use crate::runtime_support;

use runtime_support::*;

/// A Codex turn that runs `cargo build` (heavy whatever the repository),
/// a command whose start the output does not show, and a tool, then
/// finishes: its worker span's `session_closed` carries the work, the run
/// directory's `worktime.jsonl` its commands, and `stats --full` and
/// `timeline` show them.
#[test]
fn a_codex_workers_commands_are_its_spans_work_and_worktime_lines() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    set_turns(
        dir.path(),
        &format!(
            r#"printf '{{"type":"item.started","item":{{"id":"c1","type":"command_execution","command":"cargo build","status":"in_progress"}}}}\n'
printf '{{"type":"item.completed","item":{{"id":"c1","type":"command_execution","command":"cargo build","exit_code":0,"aggregated_output":"ok","status":"completed"}}}}\n'
printf '{{"type":"item.completed","item":{{"id":"c2","type":"command_execution","command":"git status","exit_code":1,"aggregated_output":"no","status":"failed"}}}}\n'
printf '{{"type":"item.started","item":{{"id":"t1","type":"mcp_tool_call","server":"s","tool":"t","status":"in_progress"}}}}\n'
printf '{{"type":"item.completed","item":{{"id":"t1","type":"mcp_tool_call","server":"s","tool":"t","status":"completed"}}}}\n'
{FINISH}"#
        ),
    );
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        &codex,
        &[verdict("pass", &[], "meets the acceptance")],
    );
    let outcome = finished(&backend, supervisor);
    assert_eq!(
        outcome["runs"].as_array().unwrap().last().unwrap()["status"],
        "integrated",
        "{outcome}"
    );
    let detail = detail(&db);
    let run = &detail.runs[0];
    let run_dir = Path::new(run.run_dir().unwrap());

    // The turn's commands, as the wrapper read them.
    let file = fs::read_to_string(run_dir.join("turns/turn-000001.commands.jsonl")).unwrap();
    let items: Vec<Value> = file
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(items.len(), 3, "{file}");
    assert_eq!(items[0]["command"], "cargo build");
    assert!(items[0]["started"].is_i64() && items[0]["ended"].is_i64());
    assert!(items[0]["ended"].as_i64() >= items[0]["started"].as_i64());
    assert_eq!(items[1]["started"], Value::Null, "{file}");
    assert_eq!(items[1]["exit_code"], 1);
    assert_eq!(items[2]["tool"], "mcp_tool_call");

    // The worker span's work, of the same fields as a Claude span's.
    let spans: Vec<&Value> = payloads(&detail, "session_closed")
        .into_iter()
        .filter(|span| span["kind"] == "worker")
        .collect();
    assert_eq!(spans.len(), 1, "{spans:?}");
    let work = &spans[0]["work"];
    assert!(work.is_object(), "{}", spans[0]);
    assert_eq!(spans[0].get("work_unavailable"), None);
    assert_eq!(work["commands"]["build"], json!({"runs": 1, "failed": 0}));
    let heavy = work["heavy"].as_array().unwrap();
    assert_eq!(heavy.len(), 1, "{work}");
    assert_eq!(heavy[0]["category"], "build");
    assert_eq!(heavy[0]["background"], false);
    assert_eq!(heavy[0]["finished"], true);
    assert_eq!(heavy[0]["failed"], false);
    assert!(work["total_secs"].as_i64().unwrap() >= work["secs"]["build"].as_i64().unwrap_or(0));
    // Not dagq's source: no cargo-only counts.
    assert_eq!(work.get("verification_repeats"), None, "{work}");

    // The run directory's lines, one per command.
    let lines: Vec<Value> = fs::read_to_string(run_dir.join("worktime.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let categories: Vec<(&str, &str, &str)> = lines
        .iter()
        .map(|line| {
            assert_eq!(line["kind"], "worker", "{line}");
            assert_eq!(line["background"], false, "{line}");
            (
                line["tool"].as_str().unwrap(),
                line["category"].as_str().unwrap(),
                line["time_source"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        categories,
        [
            ("command_execution", "build", "read"),
            ("command_execution", "git", "completed_only"),
            ("mcp_tool_call", "tool", "read"),
        ],
        "{lines:?}"
    );
    assert_eq!(lines[1]["failed"], true);
    assert_eq!(lines[1]["secs"], 0);

    // `stats --full` and `timeline` take the span's work.
    let stats = common::cli::ok(&db, &["stats", "--full"]);
    let breakdown = &stats["runs"][0]["work_breakdown"];
    assert_eq!(breakdown["sessions"], 1, "{breakdown}");
    assert_eq!(
        breakdown["commands"]["build"],
        json!({"runs": 1, "failed": 0}),
        "{breakdown}"
    );
    let timeline = common::cli::ok(&db, &["timeline", run.id().as_str()]);
    let commands = timeline["commands"].as_array().unwrap();
    assert_eq!(commands.len(), 1, "{timeline}");
    assert_eq!(commands[0]["category"], "build");
    assert_eq!(commands[0]["session"], "worker");
}
