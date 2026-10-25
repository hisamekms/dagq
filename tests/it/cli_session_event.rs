//! `dagq session-event`, the hooks' record of an inbox's span:
//! SessionEnd latency is measured only against a disposable fixture
//! queue, and a Codex inbox's hooks open, mark and close its span.
use crate::common::{self, WithoutActor};
use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Stdio},
    time::Instant,
};

fn event(db: &std::path::Path, action: &str, input: &Value) -> Value {
    event_with(db, action, input, &[])
}

fn event_with(db: &std::path::Path, action: &str, input: &Value, env: &[(&str, &str)]) -> Value {
    let _waiting = common::within(common::STEP_LIMIT, "session-event fixture command");
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command
        .without_actor_env()
        .env("DAGQ_ROLE", "inbox")
        .envs(env.iter().copied())
        .args(["--db", db.to_str().unwrap(), "session-event", action])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn close_with_a_production_sized_transcript() {
    let (dir, db) = common::cli::queue();
    // Same size as the long main-checkout inbox transcript observed for
    // task 655 (2026-09-30); generated data, never private transcript text.
    const BYTES: usize = 9_249_259;
    let path = dir.path().join("long.jsonl");
    let line = json!({"type":"assistant", "sessionId":"long",
        "version":"2.1.283", "timestamp":"2026-09-30T00:00:00Z",
        "message":{"content":[{"type":"text","text":"fixture"}]}})
    .to_string()
        + "\n";
    let mut transcript = line.repeat(BYTES / line.len());
    transcript.extend(std::iter::repeat_n(' ', BYTES - transcript.len()));
    std::fs::write(&path, transcript).unwrap();
    let input = json!({"session_id":"long", "transcript_path":path,
        "cwd":dir.path(), "reason":"prompt_input_exit", "source":"startup"});
    let opened = event(&db, "open", &input)["opened"].clone();
    let start = Instant::now();
    let closed = event(&db, "close", &input);
    let elapsed = start.elapsed();
    eprintln!(
        "session-event close: {BYTES} bytes, {:.6} seconds",
        elapsed.as_secs_f64()
    );
    assert_eq!(closed["closed"], json!([opened]));
    let conn = rusqlite::Connection::open(&db).unwrap();
    let payload: String = conn
        .query_row(
            "SELECT payload FROM run_events WHERE kind='session_closed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&payload).unwrap()["active_unavailable"],
        "hook_intake_pending"
    );
    let turns: i64 = conn
        .query_row(
            "SELECT count(*) FROM run_events WHERE kind='session_turns'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(turns, 0);
    assert_eq!(event(&db, "close", &input)["closed"], json!([]));
}

/// A Codex inbox's hooks through `dagq session-event` (ADR-t1181-1
/// decision 5): its SessionStart opens the span with the launch's
/// provider and the hook's model, the compaction's hooks and the
/// SessionStart of `source` `compact` mark it without a new span, the
/// guardrail's hook is recorded once a session, and its SessionEnd closes
/// it.
#[test]
fn a_codex_inboxs_hooks_open_mark_and_close_its_span() {
    let (_dir, db) = common::cli::queue();
    let env = [
        ("DAGQ_SESSION_KIND", "inbox"),
        (
            "DAGQ_LAUNCH",
            r#"{"role":"inbox","provider":"codex","model":null,"effort":null,"source":"argument"}"#,
        ),
    ];
    let input = |name: &str, extra: Value| {
        let mut input = json!({"session_id": "01a16fd0-ea53", "cwd": "/repo",
            "transcript_path": "/codex/rollout-01a16fd0.jsonl", "hook_event_name": name,
            "model": "gpt-6.1-sol"});
        for (key, value) in extra.as_object().unwrap() {
            input[key] = value.clone();
        }
        input
    };
    let opened = event_with(
        &db,
        "open",
        &input("SessionStart", json!({"source": "startup"})),
        &env,
    )["opened"]
        .clone();
    assert!(opened.is_i64(), "{opened}");
    for name in ["PreCompact", "PostCompact"] {
        let marked = event_with(
            &db,
            "compact",
            &input(name, json!({"trigger": "auto"})),
            &env,
        );
        assert_eq!(marked["marked"], "session_compacted");
        assert_eq!(marked["opened"], Value::Null);
    }
    let again = event_with(
        &db,
        "open",
        &input("SessionStart", json!({"source": "compact"})),
        &env,
    );
    assert_eq!(again["opened"], Value::Null);
    assert_eq!(again["closed"], json!([]));
    let tool = input("PreToolUse", json!({"tool_name": "Bash"}));
    assert_eq!(
        event_with(&db, "guard", &tool, &env)["marked"],
        "inbox_guardrail_reached"
    );
    assert_eq!(event_with(&db, "guard", &tool, &env)["marked"], Value::Null);
    let mut end = input("SessionEnd", json!({"reason": "other"}));
    end.as_object_mut().unwrap().remove("model");
    assert_eq!(
        event_with(&db, "close", &end, &env)["closed"],
        json!([opened])
    );

    let conn = rusqlite::Connection::open(&db).unwrap();
    let payloads = |kind: &str| -> Vec<Value> {
        conn.prepare("SELECT payload FROM run_events WHERE kind=?1 ORDER BY id")
            .unwrap()
            .query_map([kind], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|text| serde_json::from_str(&text.unwrap()).unwrap())
            .collect()
    };
    let spans = payloads("session_opened");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0]["provider"], "codex");
    assert_eq!(spans[0]["model"], "gpt-6.1-sol");
    assert_eq!(spans[0]["session_id"], "01a16fd0-ea53");
    assert_eq!(spans[0]["launch"]["source"], "argument");
    let marks: Vec<(String, String)> = payloads("session_compacted")
        .iter()
        .map(|mark| {
            assert_eq!(mark["opened_event_id"], opened);
            (
                mark["hook"].as_str().unwrap().to_owned(),
                mark["trigger"]
                    .as_str()
                    .or(mark["source"].as_str())
                    .unwrap()
                    .to_owned(),
            )
        })
        .collect();
    assert_eq!(
        marks,
        [
            ("pre_compact".to_owned(), "auto".to_owned()),
            ("post_compact".to_owned(), "auto".to_owned()),
            ("session_start".to_owned(), "compact".to_owned()),
        ]
    );
    assert_eq!(payloads("inbox_guardrail_reached").len(), 1);
    assert_eq!(payloads("session_closed")[0]["reason"], "exited");
}
