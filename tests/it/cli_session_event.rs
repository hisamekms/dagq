//! SessionEnd latency is measured only against a disposable fixture queue.
use crate::common::{self, WithoutActor};
use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Stdio},
    time::Instant,
};

fn event(db: &std::path::Path, action: &str, input: &Value) -> Value {
    let _waiting = common::within(common::STEP_LIMIT, "session-event fixture command");
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command
        .without_actor_env()
        .env("DAGQ_ROLE", "inbox")
        .env_remove("CMUX_WORKSPACE_ID")
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
