//! `planner request` and the headless planner's `planner screen` /
//! `planner send` (ADR-t1533-1): a person or the inbox hands a follow-up
//! request to an open headless planner of the runtime's by its planner id.
//! The words are written under the planner's directory and the sentence
//! pointing at them becomes its next turn request, after a turn at work;
//! an interactive planner and one closed, lost, exited or asked to exit
//! are refused, and nothing reaches cmux. Other roles are refused and
//! recorded. A headless planner has no screen and takes no keys. The
//! words come as `--text`, from a file with `--text-file` (`--file`) or
//! from stdin with `--text -`, handed as read; two of them or none, and a
//! file or stdin that cannot be read, is not UTF-8 or holds nothing, are
//! refused and hand nothing. The planner's background handle is this test
//! process, alive throughout.

use crate::common::{self, WithoutActor};

use common::cli::*;

use dagq::{
    application::{ProcessControl, planner_idle_marker},
    domain::{
        PlannerId, PlannerOrigin, PlannerRoute,
        background_wrapper::BackgroundHandle,
        turn::{exit_path, request_path, taken_path},
    },
    infrastructure::{adapters::SystemProcesses, location::planners_dir, sqlite::SqliteQueue},
};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

/// A stub `cmux` beside `db` that only keeps its calls; returns its path.
fn stub_cmux(db: &Path) -> PathBuf {
    let dir = db.parent().unwrap().join("request-cmux");
    fs::create_dir_all(&dir).unwrap();
    let stub = dir.join("cmux");
    common::template::script(
        &stub,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${0%/*}/calls\"\n",
    );
    stub
}

fn calls(cmux: &Path) -> String {
    fs::read_to_string(cmux.parent().unwrap().join("calls")).unwrap_or_default()
}

/// The handle of `pid` as the supervisor records a background wrapper.
fn handle_of(pid: u32) -> String {
    BackgroundHandle::new(pid, &SystemProcesses.start_identity(pid).unwrap()).to_string()
}

/// The handle of this process: alive while the test runs.
fn live_handle() -> String {
    handle_of(std::process::id())
}

/// A process alive until the guard drops, and its handle: a planner's
/// workspace id is its own.
fn live_process() -> (common::KillOnDrop, String) {
    let child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let handle = handle_of(child.id());
    (
        common::KillOnDrop::new(child, "a planner's stand-in wrapper"),
        handle,
    )
}

/// A planner of the runtime's on `route`, its session in `workspace`, its
/// wrapper `wrapper` and its agent registered.
fn planner(db: &Path, route: PlannerRoute, workspace: &str, wrapper: u32) -> PlannerId {
    let queue = SqliteQueue::open(db).unwrap();
    let planner = queue.open_planner(PlannerOrigin::Runtime, None).unwrap();
    queue.set_planner_route(planner.id, route).unwrap();
    queue
        .planner_workspace_created(planner.id, workspace)
        .unwrap();
    queue.register_planner_wrapper(planner.id, wrapper).unwrap();
    queue
        .register_planner_agent(planner.id, wrapper, wrapper)
        .unwrap();
    planner.id
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

fn refusal(env: &[(&str, &str)], db: &Path, args: &[&str]) -> Value {
    let output = invoke_with(env, db, args);
    assert!(!output.status.success(), "{env:?} {args:?} succeeded");
    serde_json::from_slice(&output.stderr).unwrap()
}

/// `planner request ID --text TEXT` with the stub `cmux`.
fn request(planner: PlannerId, text: &str, cmux: &str) -> Vec<String> {
    let id = planner.to_string();
    ["planner", "request", &id, "--text", text, "--cmux", cmux]
        .map(str::to_owned)
        .to_vec()
}

fn args(argv: &[String]) -> Vec<&str> {
    argv.iter().map(String::as_str).collect()
}

#[test]
fn a_follow_up_reaches_a_live_headless_planner_as_its_next_turn_with_its_actor() {
    let (_dir, db) = queue();
    let cmux = stub_cmux(&db);
    let cmux = cmux.to_str().unwrap();
    let id = planner(
        &db,
        PlannerRoute::Headless,
        &live_handle(),
        std::process::id(),
    );
    let dir = planners_dir(&db).join(id.to_string());
    // A turn at work: its request taken, no idle marker after it.
    fs::create_dir_all(dir.join("turns")).unwrap();
    fs::write(taken_path(&dir, 1), "{}").unwrap();

    let handed = ok_as(
        "inbox",
        &db,
        &args(&request(id, "also split the docs", cmux)),
    );
    assert_eq!(handed["state"], "working");
    assert_eq!(handed["seq"], 2, "after the turn at work: {handed}");
    let file = dir.join("requests").join("followup-1.md");
    assert_eq!(handed["file"], file.display().to_string());
    assert_eq!(fs::read_to_string(&file).unwrap(), "also split the docs");
    let turn: Value = serde_json::from_slice(&fs::read(request_path(&dir, 2)).unwrap()).unwrap();
    assert_eq!(
        turn["prompt"],
        format!(
            "dagq: a request for you is in the file {}: read it and work on it as it says.",
            file.display()
        )
    );
    assert_eq!(turn["what"], "follow-up request followup-1");
    // The turn at work ends: its idle marker is no idle while the
    // follow-up waits to be taken.
    fs::write(
        planner_idle_marker(&dir),
        r#"{"dagq_turn":{"turn":1,"outcome":"succeeded"}}"#,
    )
    .unwrap();
    let listed = ok(&db, &["planners", "--cmux", cmux]);
    assert_eq!(listed["planners"][0]["state"], "working", "{listed}");

    // A person at a plain terminal, from a file; the next one is taken
    // after the first.
    let words = db.parent().unwrap().join("words.md");
    fs::write(&words, "and keep the tests").unwrap();
    let second = ok(
        &db,
        &[
            "planner",
            "request",
            &id.to_string(),
            "--file",
            words.to_str().unwrap(),
            "--cmux",
            cmux,
        ],
    );
    assert_eq!(second["seq"], 3);
    assert_eq!(second["name"], "followup-2");

    let recorded = events(&db, "planner_request_handed");
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0]["actor"]["role"], "inbox");
    assert_eq!(
        recorded[0]["payload"]["planner_id"],
        id.to_string().parse::<i64>().unwrap()
    );
    assert_eq!(recorded[0]["payload"]["seq"], 2);
    assert_eq!(recorded[1]["actor"]["role"], "user");
    let requested = events(&db, "turn_requested");
    assert_eq!(requested.len(), 2);
    assert_eq!(
        requested[0]["payload"]["what"],
        "follow-up request followup-1"
    );
    assert_eq!(calls(Path::new(cmux)), "", "nothing reached cmux");
}

#[test]
fn an_interactive_closed_lost_exited_exiting_or_walled_planner_is_refused_with_the_reason() {
    let (_dir, db) = queue();
    let cmux = stub_cmux(&db);
    let cmux = cmux.to_str().unwrap();
    let pid = std::process::id();
    let (_alive, handles) = (0..4)
        .map(|_| live_process())
        .unzip::<_, _, Vec<_>, Vec<_>>();
    let interactive = planner(&db, PlannerRoute::Interactive, "PW-1", pid);
    let closed = planner(&db, PlannerRoute::Headless, "background:1:closed", pid);
    let exited = planner(&db, PlannerRoute::Headless, &handles[0], pid);
    let exiting = planner(&db, PlannerRoute::Headless, &handles[1], pid);
    // A wrapper that is gone behind a handle still alive.
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    gone.wait().unwrap();
    let lost = planner(&db, PlannerRoute::Headless, &handles[2], gone.id());
    // Idle after a turn that failed at the usage limit: it waits for Claude.
    let walled = planner(&db, PlannerRoute::Headless, &handles[3], pid);
    let walled_dir = planners_dir(&db).join(walled.to_string());
    fs::create_dir_all(&walled_dir).unwrap();
    fs::write(
        planner_idle_marker(&walled_dir),
        r#"{"dagq_turn":{"turn":1,"outcome":"failed","failure":"usage_limit"}}"#,
    )
    .unwrap();
    {
        let queue = SqliteQueue::open(&db).unwrap();
        queue.close_planner(closed, None).unwrap();
        queue.planner_exited(exited, pid, 0).unwrap();
    }
    let exit = exit_path(&planners_dir(&db).join(exiting.to_string()));
    fs::create_dir_all(exit.parent().unwrap()).unwrap();
    fs::write(&exit, "").unwrap();

    for (id, reason) in [
        (interactive, "is interactive"),
        (closed, "is closed"),
        (lost, "is lost"),
        (exited, "is exited"),
        (exiting, "is asked to exit"),
        (walled, "waits for Claude"),
    ] {
        let error = refusal(
            &[("DAGQ_ROLE", "inbox")],
            &db,
            &args(&request(id, "more", cmux)),
        );
        assert!(
            error["error"].as_str().unwrap().contains(reason),
            "{id}: {error}"
        );
        assert!(
            !planners_dir(&db)
                .join(id.to_string())
                .join("requests")
                .exists(),
            "{id}: nothing handed"
        );
    }
    assert_eq!(
        calls(Path::new(cmux)),
        "",
        "nothing typed or probed by cmux"
    );
    assert!(events(&db, "planner_request_handed").is_empty());
    assert!(events(&db, "turn_requested").is_empty());
}

#[test]
fn only_the_inbox_and_a_person_hand_a_request_and_other_roles_are_recorded() {
    let (_dir, db) = queue();
    let cmux = stub_cmux(&db);
    let cmux = cmux.to_str().unwrap();
    let id = planner(
        &db,
        PlannerRoute::Headless,
        &live_handle(),
        std::process::id(),
    );
    let own = id.to_string();
    let planner_actor = format!("planner:{own}");
    let actors: Vec<Vec<(&str, &str)>> = vec![
        vec![
            ("DAGQ_ROLE", "planner"),
            ("DAGQ_ACTOR_ID", &planner_actor),
            ("DAGQ_PLANNER_ID", &own),
        ],
        vec![
            ("DAGQ_ROLE", "worker"),
            ("DAGQ_ACTOR_ID", "worker:x"),
            ("DAGQ_RUN_ID", "r-1"),
            ("DAGQ_TASK_ID", "1"),
        ],
        vec![("DAGQ_ROLE", "observer")],
        vec![("DAGQ_ROLE", "recovery-job")],
        vec![("DAGQ_ROLE", "plan-review-job")],
        vec![("DAGQ_ROLE", "supervisor")],
    ];
    for env in &actors {
        let error = refusal(env, &db, &args(&request(id, "more", cmux)));
        assert_eq!(error["denied"]["capability"], "planner.request", "{env:?}");
        assert_eq!(error["denied"]["reason"], "not granted");
    }
    let denied = events(&db, "authorization_denied");
    assert_eq!(denied.len(), actors.len());
    assert_eq!(denied[0]["actor"]["role"], "planner");
    assert_eq!(denied[0]["payload"]["capability"], "planner.request");
    assert!(events(&db, "planner_request_handed").is_empty());
    assert!(
        !planners_dir(&db).join(&own).join("requests").exists(),
        "nothing handed"
    );
    ok_as("inbox", &db, &args(&request(id, "more", cmux)));
}

#[test]
fn a_headless_planner_has_no_screen_and_takes_no_keys_or_answer() {
    let (_dir, db) = queue();
    let cmux = stub_cmux(&db);
    let cmux = cmux.to_str().unwrap();
    let id = planner(
        &db,
        PlannerRoute::Headless,
        &live_handle(),
        std::process::id(),
    );
    let own = id.to_string();

    let read = ok_as("inbox", &db, &["planner", "screen", &own, "--cmux", cmux]);
    assert_eq!(read["screen"], Value::Null);
    assert_eq!(read["route"], "headless");
    assert_eq!(
        read["turns"],
        planners_dir(&db)
            .join(&own)
            .join("turns")
            .display()
            .to_string()
    );
    for send in [
        vec!["planner", "send", &own, "--key", "enter"],
        vec!["planner", "send", &own, "--key", "exit"],
        vec!["planner", "send", &own, "--answer", "1"],
    ] {
        let mut argv = send.clone();
        argv.extend(["--cmux", cmux]);
        let error = refusal(&[("DAGQ_ROLE", "inbox")], &db, &argv);
        assert!(
            error["error"].as_str().unwrap().contains("is headless"),
            "{argv:?}: {error}"
        );
    }
    assert_eq!(calls(Path::new(cmux)), "", "nothing read or typed");
    assert!(events(&db, "screen_read").is_empty());
    assert!(events(&db, "screen_input_sent").is_empty());
}

/// Multi-line words with quotes, backticks, Japanese and a trailing line
/// break, which a shell's quoting breaks.
const WORDS: &str = "残りの task も分けて。\"戻す\" 計画は後で。\nIt's `dagq.toml`'s [run.env]; keep it.\n\n  - 2 行目の箇条書き\n";

/// `planner request ID ARGS --cmux CMUX` as the inbox with `stdin` on its
/// standard input.
fn request_with_stdin(
    db: &Path,
    planner: PlannerId,
    args: &[&str],
    cmux: &str,
    stdin: &[u8],
) -> Output {
    let _waiting = common::within(common::STEP_LIMIT, "planner request with stdin");
    let mut child = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .env("DAGQ_ROLE", "inbox")
        .arg("--db")
        .arg(db)
        .args(["planner", "request", &planner.to_string()])
        .args(args)
        .args(["--cmux", cmux])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A refusal before stdin is read (a conflict clap rejects) closes the
    // pipe early; its exit is what the test reads, not the write.
    let _ = child.stdin.take().unwrap().write_all(stdin);
    child.wait_with_output().unwrap()
}

#[test]
fn words_from_a_file_or_stdin_are_handed_as_text_gives_them() {
    let (dir, db) = queue();
    let cmux = stub_cmux(&db);
    let cmux = cmux.to_str().unwrap();
    let id = planner(
        &db,
        PlannerRoute::Headless,
        &live_handle(),
        std::process::id(),
    );
    let words_file = dir.path().join("words.md");
    fs::write(&words_file, WORDS).unwrap();
    let words_path = words_file.to_str().unwrap();

    let mut handed = Vec::new();
    for args in [
        &["--text", WORDS][..],
        &["--text-file", words_path][..],
        // `--file`, the spelling task 1533 landed, is the same input.
        &["--file", words_path][..],
        &["--text", "-"][..],
    ] {
        let output = request_with_stdin(&db, id, args, cmux, WORDS.as_bytes());
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let request: Value = serde_json::from_slice(&output.stdout).unwrap();
        handed.push(request["file"].as_str().unwrap().to_owned());
    }
    let requests = planners_dir(&db).join(id.to_string()).join("requests");
    for (n, file) in handed.iter().enumerate() {
        assert_eq!(
            *file,
            requests
                .join(format!("followup-{}.md", n + 1))
                .display()
                .to_string()
        );
        assert_eq!(fs::read_to_string(file).unwrap(), WORDS, "{file}");
    }
    assert_eq!(events(&db, "turn_requested").len(), 4);
    // What is handed is what the file held when read, not its path.
    fs::write(&words_file, "rewritten after the hand-over").unwrap();
    assert_eq!(fs::read_to_string(&handed[1]).unwrap(), WORDS);
}

#[test]
fn two_inputs_none_or_unreadable_words_are_refused_and_hand_nothing() {
    let (dir, db) = queue();
    let cmux = stub_cmux(&db);
    let cmux = cmux.to_str().unwrap();
    let id = planner(
        &db,
        PlannerRoute::Headless,
        &live_handle(),
        std::process::id(),
    );
    let words = dir.path().join("words.md");
    fs::write(&words, WORDS).unwrap();
    let missing = dir.path().join("missing.md");
    let latin1 = dir.path().join("latin1.md");
    fs::write(&latin1, b"caf\xe9 \xff\n").unwrap();
    let empty = dir.path().join("empty.md");
    fs::write(&empty, "").unwrap();
    let blank = dir.path().join("blank.md");
    fs::write(&blank, " \n\t\n").unwrap();
    let path = |file: &Path| file.to_str().unwrap().to_owned();

    // Two of the inputs, or none, are refused by clap.
    for args in [
        vec![],
        vec![
            "--text".into(),
            "x".into(),
            "--text-file".into(),
            path(&words),
        ],
        vec!["--text".into(), "-".into(), "--file".into(), path(&words)],
        vec![
            "--text-file".into(),
            path(&words),
            "--file".into(),
            path(&words),
        ],
        vec!["--text".into(), "x".into(), "--text".into(), "-".into()],
    ] {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = request_with_stdin(&db, id, &args, cmux, WORDS.as_bytes());
        assert!(!output.status.success(), "{args:?}");
    }
    // Words that cannot be read, are not UTF-8 or hold nothing are
    // refused with the input and the reason.
    let cases: Vec<(Vec<String>, &[u8], &str)> = vec![
        (
            vec!["--text-file".into(), path(&missing)],
            b"",
            "--text-file",
        ),
        (
            vec!["--text-file".into(), path(&missing)],
            b"",
            "cannot read",
        ),
        (vec!["--file".into(), path(&latin1)], b"", "not UTF-8"),
        (vec!["--text-file".into(), path(&empty)], b"", "is empty"),
        (vec!["--text-file".into(), path(&blank)], b"", "is empty"),
        (
            vec!["--text".into(), "-".into()],
            b"",
            "--text - (stdin) is empty",
        ),
        (vec!["--text".into(), "-".into()], b" \n", "is empty"),
        (vec!["--text".into(), "-".into()], b"\xff\xfe", "not UTF-8"),
    ];
    for (args, stdin, reason) in cases {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = request_with_stdin(&db, id, &args, cmux, stdin);
        assert!(!output.status.success(), "{args:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        let message = error["error"].as_str().unwrap();
        assert!(message.contains(reason), "{args:?}: {message}");
    }
    assert!(
        !planners_dir(&db)
            .join(id.to_string())
            .join("requests")
            .exists(),
        "no followup-N.md"
    );
    assert!(events(&db, "planner_request_handed").is_empty());
    assert!(events(&db, "turn_requested").is_empty());
    assert_eq!(calls(Path::new(cmux)), "", "nothing reached cmux");
}
