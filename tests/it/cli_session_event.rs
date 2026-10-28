//! `dagq session-event`, the hooks' record of an inbox's span: the hook
//! sends it to the queue service with the inbox's token, as the inbox
//! (ADR-t3102-1 decision 4), and is given no queue path, so it opens none.
//! SessionEnd latency is measured only against a disposable fixture
//! queue, and a Codex inbox's hooks open, mark and close its span.
use crate::common::{self, WithoutActor};
use common::service::{Served, events, queue, start};
use dagq::domain::queue_service::{CREDENTIAL_FILE_ENV, Principal, SessionProcess};
use dagq::infrastructure::queue_service as service;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
    time::Instant,
};

/// The file of the token of the inbox whose session is the process `pid`,
/// issued as `dagq inbox` issues it on the queue at `db`.
fn inbox_credential(db: &Path, pid: u32) -> String {
    let session = SessionProcess {
        pid,
        start: dagq::infrastructure::adapters::session_start(pid).unwrap(),
    };
    service::issue(db.parent().unwrap(), &Principal::inbox(session), 1)
        .unwrap()
        .file
        .to_str()
        .unwrap()
        .to_owned()
}

fn event(db: &Path, action: &str, input: &Value) -> Value {
    event_with(db, action, input, &[])
}

/// The hook `action` of the inbox of this test's process on the queue at
/// `db`, with `env`: what it printed, once it succeeded.
fn event_with(db: &Path, action: &str, input: &Value, env: &[(&str, &str)]) -> Value {
    let credential = inbox_credential(db, std::process::id());
    let mut env = env.to_vec();
    env.push((CREDENTIAL_FILE_ENV, &credential));
    let output = hook(db, action, input, &env);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// `dagq session-event action` of an inbox with `env` and `input` on its
/// stdin, run beside the queue at `db` but given no path of it: neither
/// `--db` nor a repository whose queue it is.
fn hook(db: &Path, action: &str, input: &Value, env: &[(&str, &str)]) -> Output {
    run_hook(None, db, action, input, env)
}

/// [`hook`] given `--db named`.
fn hook_on(named: &Path, db: &Path, action: &str, input: &Value, env: &[(&str, &str)]) -> Output {
    run_hook(Some(named), db, action, input, env)
}

fn run_hook(
    named: Option<&Path>,
    db: &Path,
    action: &str,
    input: &Value,
    env: &[(&str, &str)],
) -> Output {
    let _waiting = common::within(common::STEP_LIMIT, "session-event fixture command");
    let elsewhere = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command
        .without_actor_env()
        .current_dir(elsewhere.path())
        .env("DAGQ_ROLE", "inbox")
        .envs(env.iter().copied());
    if let Some(named) = named {
        command.arg("--db").arg(named);
    }
    command
        .args(["session-event", action])
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
    assert!(db.is_file());
    output
}

/// The `queue_service.code` of a hook that failed.
fn code(output: &Output) -> Value {
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)));
    error["queue_service"]["code"].clone()
}

#[test]
fn close_with_a_production_sized_transcript() {
    let (dir, db) = common::cli::queue();
    let _served = Served::start(&db);
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
    let _served = Served::start(&db);
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
    // Every record is the inbox's, as its token's principal.
    let roles: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT actor_role FROM run_events WHERE kind IN \
             ('session_opened','session_compacted','inbox_guardrail_reached','session_closed')",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(roles, ["inbox"]);
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

/// Acceptance (3): a principal not allowed `session.record` on the queue
/// (a worker's token) is refused the inbox's span as
/// `authorization_denied`, recorded as that principal, and the token of an
/// inbox whose session ended is refused as `session_ended`; neither opens
/// a span.
#[test]
fn a_hook_of_another_principal_or_of_an_ended_session_is_refused() {
    let queue = queue();
    start(&queue);
    let input = json!({"session_id": "s1", "source": "startup"});
    let worker = service::issue(
        queue.dir(),
        &Principal::worker(queue.run.id(), queue.run.task_id()),
        1,
    )
    .unwrap()
    .file;
    let refused = hook(
        &queue.db,
        "open",
        &input,
        &[
            ("DAGQ_SESSION_KIND", "inbox"),
            (CREDENTIAL_FILE_ENV, worker.to_str().unwrap()),
        ],
    );
    assert_eq!(code(&refused), "authorization_denied");
    let denied = events(&queue.db, "authorization_denied");
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert_eq!(denied[0]["actor"]["role"], "worker");
    assert_eq!(denied[0]["payload"]["capability"], "session.record");

    let mut agent = common::KillOnDrop::new(
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        "the inbox's stand-in agent",
    );
    let credential = inbox_credential(&queue.db, agent.child().id());
    agent.child().kill().unwrap();
    {
        let _waiting = common::within(common::STEP_LIMIT, "the stand-in agent to exit");
        agent.child().wait().unwrap();
    }
    let ended = hook(
        &queue.db,
        "open",
        &input,
        &[(CREDENTIAL_FILE_ENV, credential.as_str())],
    );
    assert_eq!(code(&ended), "unauthenticated");
    let reasons: Vec<Value> = events(&queue.db, "queue_service_unauthenticated")
        .iter()
        .map(|event| event["payload"]["reason"].clone())
        .collect();
    assert_eq!(reasons, [json!("session_ended")]);
    assert!(events(&queue.db, "session_opened").is_empty());
}

/// A worker's environment that names its session an inbox's, with no
/// token, is refused by the service as unauthenticated, and an inbox's
/// token with another queue named (elsewhere, or beside its own) is
/// refused as `wrong_queue`: none records a span, on either queue. The
/// token's own queue named records it.
#[test]
fn a_hook_with_no_token_or_another_queue_records_nothing() {
    let queue = queue();
    start(&queue);
    let input = json!({"session_id": "s1", "source": "startup"});
    let refused = hook_on(
        &queue.db,
        &queue.db,
        "open",
        &input,
        &[("DAGQ_ROLE", "worker"), ("DAGQ_SESSION_KIND", "inbox")],
    );
    assert_eq!(code(&refused), "unauthenticated");
    let unauthenticated: Vec<Value> = events(&queue.db, "queue_service_unauthenticated")
        .iter()
        .map(|event| json!([event["payload"]["use_case"], event["payload"]["reason"]]))
        .collect();
    assert_eq!(
        unauthenticated,
        [json!(["session_record", "missing_token"])]
    );
    let elsewhere = tempfile::tempdir().unwrap();
    let other = elsewhere.path().join("queue.db");
    std::fs::write(&other, "").unwrap();
    let credential = inbox_credential(&queue.db, std::process::id());
    let crossed = hook_on(
        &other,
        &queue.db,
        "open",
        &input,
        &[(CREDENTIAL_FILE_ENV, credential.as_str())],
    );
    assert_eq!(code(&crossed), "wrong_queue");
    // Another queue beside the token's, in the same directory, is not the
    // service's DB either: the service compares the file named, and
    // neither queue gets the span.
    let beside = queue.dir().join("other.db");
    drop(common::template::queue(&beside));
    let crossed = hook_on(
        &beside,
        &queue.db,
        "open",
        &input,
        &[(CREDENTIAL_FILE_ENV, credential.as_str())],
    );
    assert_eq!(code(&crossed), "wrong_queue");
    assert!(events(&queue.db, "session_opened").is_empty());
    assert!(events(&beside, "session_opened").is_empty());
    // The token's own queue named is answered, as the inbox.
    let opened = hook_on(
        &queue.db,
        &queue.db,
        "open",
        &input,
        &[(CREDENTIAL_FILE_ENV, credential.as_str())],
    );
    assert!(
        opened.status.success(),
        "{}",
        String::from_utf8_lossy(&opened.stderr)
    );
    let spans = events(&queue.db, "session_opened");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0]["actor"]["role"], "inbox");
    assert!(events(&beside, "session_opened").is_empty());
}

/// Acceptance (4): with the service stopped the hook fails as
/// `unreachable`, and nothing is written to the queue instead.
#[test]
fn a_hook_whose_service_does_not_answer_writes_nothing() {
    let queue = queue();
    start(&queue);
    let credential = inbox_credential(&queue.db, std::process::id());
    assert!(
        common::cli::invoke(&queue.db, &["service", "stop"])
            .status
            .success()
    );
    let before = dagq::infrastructure::sqlite::SqliteQueue::open(&queue.db)
        .unwrap()
        .all_events()
        .unwrap()
        .len();
    let output = hook(
        &queue.db,
        "open",
        &json!({"session_id": "s1", "source": "startup"}),
        &[(CREDENTIAL_FILE_ENV, credential.as_str())],
    );
    assert_eq!(code(&output), "unreachable");
    let after = dagq::infrastructure::sqlite::SqliteQueue::open(&queue.db)
        .unwrap()
        .all_events()
        .unwrap()
        .len();
    assert_eq!(after, before);
}
