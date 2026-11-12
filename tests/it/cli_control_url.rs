//! Boundary: a real CLI whose environment names a control side
//! (`DAGQ_CONTROL_URL`) sends its command over loopback TCP to that side's
//! HTTP entrance, with the token its terminal's login kept as the bearer, and
//! prints the answer as it came. The control side is a fake in the test; the
//! credential store is the file one in a temporary config directory, so no
//! OS store is opened. The route's and the token's decisions are the unit
//! tests' (`infrastructure::queue_service::control_tests`).
use crate::common::{self, Bounded, STEP_LIMIT, actor::WithoutActor};
use serde_json::{Value, json};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    thread,
    time::{Duration, Instant},
};

const TOKEN: &str = "dagq-terminal-secret";

/// One request as the fake control side read it.
struct Received {
    head: String,
    body: Value,
}

#[test]
fn a_command_goes_to_the_control_url_with_the_terminal_s_bearer() {
    let _test = common::test();
    // Under no run directory: the file store refuses one, and a worker's
    // `TMPDIR` is in its run directory.
    let root = common::short_tempdir();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());

    // The terminal's one credential set, kept for this control side, as
    // `dagq login` keeps it in the file store.
    let directory = root.path().join("dagq/credentials");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .unwrap();
    let kept = json!({
        "destination": {"control_url": base, "issuer": base, "client_id": "test-cli"},
        "token": TOKEN,
        "refresh_token": null,
        "identity": {"issuer": base, "subject": "subject", "role": "member",
                     "expires_at": i64::MAX},
    });
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join("login.json"))
        .unwrap()
        .write_all(kept.to_string().as_bytes())
        .unwrap();

    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(30);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < end, "no request came");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("{e}"),
            }
        };
        // macOS inherits O_NONBLOCK from the listener on accept.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut head = vec![];
        while !head.ends_with(b"\r\n\r\n") && head.len() < 16384 {
            assert!(Instant::now() < end, "fixture request timed out");
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 1);
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        let length = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        assert!(length <= 1 << 20, "fixture request too large");
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        let version = body["api_version"].clone();
        let reply = json!({
            "api_version": version,
            "min_api_version": version,
            "ok": true,
            "result": {"id": 7, "title": "from the control side"},
        })
        .to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{reply}",
            reply.len()
        )
        .unwrap();
        Received { head, body }
    });

    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command
        .without_actor_env()
        .env_remove("DAGQ_SERVICE_SOCKET")
        .env_remove("DAGQ_SERVICE_URL")
        .env_remove("DAGQ_SERVICE_CREDENTIAL_FILE")
        .env("DAGQ_CONTROL_URL", &base)
        .env("DAGQ_CREDENTIAL_STORE", "file")
        .env("XDG_CONFIG_HOME", root.path())
        .current_dir(root.path())
        .args(["show", "7"]);
    let output = command.bounded_output().unwrap();
    let received = {
        let _waiting = common::within(STEP_LIMIT, "the fake control side's thread to return");
        server.join().unwrap()
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}{stderr}");

    let request_line = received.head.lines().next().unwrap();
    assert_eq!(request_line, "POST /api/use-case/show HTTP/1.1");
    let bearer = received
        .head
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .map(|(_, value)| value.trim().to_owned())
        })
        .unwrap();
    assert_eq!(bearer, format!("Bearer {TOKEN}"));
    assert_eq!(received.body["use_case"], "show");
    for field in ["token", "env_role", "queue", "binding"] {
        assert!(received.body.get(field).is_none(), "{field}");
    }

    let printed: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(printed, json!({"id": 7, "title": "from the control side"}));
    assert!(!stdout.contains(TOKEN) && !stderr.contains(TOKEN));
    // No queue was opened or made.
    assert!(!root.path().join("data.db").exists());
}
