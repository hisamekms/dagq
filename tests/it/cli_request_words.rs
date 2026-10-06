//! The person's words of `request add` (ADR-t1394-1) given as `--text`,
//! read from a file with `--text-file` or from stdin with `--text -`, and
//! the inbox's note the same three ways. What is recorded is what the file
//! or stdin held when read, as `--text` would record it: a later change of
//! the file changes nothing. Two of the inputs, or none, are refused, and so
//! are a file that cannot be read, words that are not UTF-8 and empty words;
//! a refusal records no request.

use crate::common::{self, WithoutActor, cli::*};

use serde_json::Value;
use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

/// Multi-line words with quotes, backticks, Japanese and a trailing line
/// break, which a shell's quoting breaks.
const WORDS: &str = "着地が減った。\"戻す\" 計画を立てて。\nIt's `dagq.toml`'s [run.env]; keep it.\n\n  - 2 行目の箇条書き\n";

/// `request add` as the inbox with `stdin` on its standard input.
fn add_with_stdin(db: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let _waiting = common::within(common::STEP_LIMIT, "request add with stdin");
    let mut child = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .env("DAGQ_ROLE", "inbox")
        .arg("--db")
        .arg(db)
        .args(["request", "add"])
        .args(args)
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

fn recorded(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn requests(db: &Path) -> Vec<Value> {
    ok_as("observer", db, &["requests", "--all"])["requests"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn words_from_a_file_or_stdin_are_recorded_as_text_gives_them_and_stay() {
    let (dir, db) = queue();
    let words_file = dir.path().join("words.txt");
    std::fs::write(&words_file, WORDS).unwrap();
    let note_file = dir.path().join("note.txt");
    std::fs::write(&note_file, "人は inbox で \"10:02\" に頼んだ\n").unwrap();
    let words_path = words_file.to_str().unwrap();

    let by_text = ok_as(
        "inbox",
        &db,
        &["request", "add", "--text", WORDS, "--note", "short"],
    );
    let by_file = ok_as(
        "inbox",
        &db,
        &[
            "request",
            "add",
            "--text-file",
            words_path,
            "--note-file",
            note_file.to_str().unwrap(),
        ],
    );
    let by_stdin = recorded(&add_with_stdin(
        &db,
        &["--text", "-", "--note", "short"],
        WORDS.as_bytes(),
    ));
    let note_by_stdin = recorded(&add_with_stdin(
        &db,
        &["--text-file", words_path, "--note", "-"],
        "人は inbox で \"10:02\" に頼んだ\n".as_bytes(),
    ));
    // `--file`, the spelling task 1395 landed, is the same input.
    let by_old_spelling = ok_as("inbox", &db, &["request", "add", "--file", words_path]);
    for request in [
        &by_text,
        &by_file,
        &by_stdin,
        &note_by_stdin,
        &by_old_spelling,
    ] {
        assert_eq!(request["text"], WORDS, "{request}");
    }
    assert_eq!(by_text["note"], "short");
    assert_eq!(by_stdin["note"], "short");
    assert_eq!(by_file["note"], "人は inbox で \"10:02\" に頼んだ\n");
    assert_eq!(note_by_stdin["note"], by_file["note"]);

    // The request keeps what the file held when read, not its path.
    std::fs::write(&words_file, "rewritten after the record").unwrap();
    std::fs::remove_file(&note_file).unwrap();
    let id = by_file["id"].as_i64().unwrap().to_string();
    let read = &ok_as("observer", &db, &["requests", &id])["requests"][0];
    assert_eq!(read["text"], WORDS);
    assert_eq!(read["note"], "人は inbox で \"10:02\" に頼んだ\n");
    assert!(!read.to_string().contains("words.txt"));
}

#[test]
fn two_of_the_inputs_or_none_are_refused() {
    let (dir, db) = queue();
    let file = dir.path().join("words.txt");
    std::fs::write(&file, WORDS).unwrap();
    let file = file.to_str().unwrap();
    for args in [
        &[][..],
        &["--note", "only a note"][..],
        &["--text", "x", "--text-file", file][..],
        &["--text", "-", "--text-file", file][..],
        &["--text", "x", "--text", "-"][..],
        &["--text", "x", "--note", "y", "--note-file", file][..],
    ] {
        let output = add_with_stdin(&db, args, WORDS.as_bytes());
        assert!(!output.status.success(), "{args:?}");
    }
    // Both from stdin is refused with the reason.
    let output = add_with_stdin(&db, &["--text", "-", "--note", "-"], WORDS.as_bytes());
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("both read stdin"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(requests(&db).is_empty());
}

#[test]
fn an_unreadable_file_words_not_utf8_and_empty_words_record_nothing() {
    let (dir, db) = queue();
    let missing = dir.path().join("missing.txt");
    let latin1 = dir.path().join("latin1.txt");
    std::fs::write(&latin1, b"caf\xe9 \xff\n").unwrap();
    let empty = dir.path().join("empty.txt");
    std::fs::write(&empty, "").unwrap();
    let blank = dir.path().join("blank.txt");
    std::fs::write(&blank, " \n\t\n").unwrap();
    let path = |file: &Path| file.to_str().unwrap().to_owned();
    let cases: Vec<(Vec<String>, &[u8], &str)> = vec![
        (
            vec!["--text-file".into(), path(&missing)],
            b"",
            "cannot read",
        ),
        (vec!["--text-file".into(), path(&latin1)], b"", "not UTF-8"),
        (vec!["--text-file".into(), path(&empty)], b"", "is empty"),
        (vec!["--text-file".into(), path(&blank)], b"", "is empty"),
        (vec!["--text".into(), "-".into()], b"", "is empty"),
        (vec!["--text".into(), "-".into()], b"\xff\xfe", "not UTF-8"),
        (
            vec![
                "--text".into(),
                "x".into(),
                "--note-file".into(),
                path(&missing),
            ],
            b"",
            "cannot read",
        ),
        (
            vec!["--text".into(), "x".into(), "--note".into(), "-".into()],
            b"\xe9",
            "not UTF-8",
        ),
        (
            vec![
                "--text".into(),
                "x".into(),
                "--note-file".into(),
                path(&blank),
            ],
            b"",
            "is empty",
        ),
        (
            vec!["--text".into(), "x".into(), "--note".into(), "-".into()],
            b"",
            "is empty",
        ),
    ];
    for (args, stdin, reason) in cases {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = add_with_stdin(&db, &args, stdin);
        assert!(!output.status.success(), "{args:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        let message = error["error"].as_str().unwrap();
        assert!(message.contains(reason), "{args:?}: {message}");
    }
    assert!(requests(&db).is_empty());
}

#[test]
fn an_ungranted_actor_is_refused_before_a_file_or_open_stdin_is_read() {
    let (dir, db) = queue();
    let missing = dir.path().join("missing.md");
    for (n, input) in [["--text-file", missing.to_str().unwrap()], ["--text", "-"]]
        .into_iter()
        .enumerate()
    {
        let child = Command::new(env!("CARGO_BIN_EXE_dagq"))
            .without_actor_env()
            .env("DAGQ_ROLE", "planner")
            .arg("--db")
            .arg(&db)
            .args(["request", "add"])
            .args(input)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = common::KillOnDrop::new(child, "refusal before reading open stdin");
        // Hold the writing end outside Child: wait_with_output must not
        // close it and supply EOF, which would hide a read before refusal.
        let stdin = child.child().stdin.take().unwrap();
        let output = child.wait_with_output().unwrap();
        drop(stdin);
        assert!(!output.status.success(), "{input:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["denied"]["capability"], "request.record");
        assert_eq!(error["denied"]["reason"], "not granted");
        let denied = ok(
            &db,
            &[
                "events",
                "--after",
                "0",
                "--all",
                "--full",
                "--kind",
                "authorization_denied",
            ],
        );
        let denied = denied["events"].as_array().unwrap();
        assert_eq!(denied.len(), n + 1, "one event per refusal");
        assert_eq!(denied[n]["actor"]["role"], "planner");
        assert_eq!(denied[n]["payload"]["capability"], "request.record");
    }
    assert!(requests(&db).is_empty());
}
