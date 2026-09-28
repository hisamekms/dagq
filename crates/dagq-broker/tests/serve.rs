//! `dagq-broker serve` as a host process (no podman) on `127.0.0.1:0`:
//! health, the refusals of default deny, the fs and process backends and
//! their audit lines.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dagq_broker_protocol::{
    BrokerCapability, BrokerRole, Committer, ErrorBody, ErrorCode, HealthResponse, SigningKey,
    TOKEN_VERSION, TokenClaims, sign,
};
use serde_json::Value;

/// The longest any one wait in these tests takes.
const LIMIT: Duration = Duration::from_secs(20);

const KEY: [u8; 32] = [7; 32];

const HOST_SECRET_NAME: &str = "DAGQ_BROKER_TEST_HOST_SECRET";
const HOST_SECRET_VALUE: &str = "host-secret-value-832";

struct Broker {
    child: Child,
    addr: SocketAddr,
    dir: tempfile::TempDir,
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Broker {
    fn start() -> Self {
        Self::start_with(&[])
    }

    /// Start with `extra` flags after the required ones.
    fn start_with(extra: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("key"), KEY).unwrap();
        fs::create_dir_all(root.join("active")).unwrap();
        fs::create_dir_all(root.join("runs/run-1/worktree")).unwrap();
        fs::create_dir_all(root.join("runs/run-2/worktree")).unwrap();
        fs::write(root.join("runs/run-2/worktree/theirs"), "theirs\n").unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_dagq-broker"))
            .arg("serve")
            .args(["--listen", "127.0.0.1:0"])
            .arg("--key")
            .arg(root.join("key"))
            .arg("--active")
            .arg(root.join("active"))
            .arg("--audit")
            .arg(root.join("audit"))
            .arg("--root")
            .arg(root.join("runs"))
            .args(extra)
            // A secret of the host's env, which no program run by
            // `process.exec` may see.
            .env(HOST_SECRET_NAME, HOST_SECRET_VALUE)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start dagq-broker serve");
        let stdout = child.stdout.take().unwrap();
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = send.send(line);
        });
        let line = receive
            .recv_timeout(LIMIT)
            .expect("dagq-broker serve prints where it listens");
        let listening: Value = serde_json::from_str(&line).expect(&line);
        let addr: SocketAddr = listening["listening"].as_str().unwrap().parse().unwrap();
        assert!(addr.ip().is_loopback(), "{addr}");
        assert_ne!(addr.port(), 0);
        assert_eq!(listening["build"], env!("CARGO_PKG_VERSION"));
        Self { child, addr, dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn workspace(&self) -> String {
        self.root()
            .join("runs/run-1/worktree")
            .to_string_lossy()
            .into_owned()
    }

    /// Claims for run-1 with `capabilities`, valid for an hour.
    fn claims(&self, jti: &str, capabilities: &[BrokerCapability]) -> TokenClaims {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        TokenClaims {
            v: TOKEN_VERSION,
            jti: jti.to_owned(),
            actor_id: "worker:run-1".to_owned(),
            role: BrokerRole::Worker,
            task_id: 830,
            run_id: "run-1".to_owned(),
            workspace: self.workspace(),
            branch: "dagq/run-1".to_owned(),
            committer: Committer {
                name: "A".to_owned(),
                email: "a@example.com".to_owned(),
            },
            capabilities: capabilities.iter().copied().collect(),
            iat: now,
            exp: now + 3600,
        }
    }

    /// A token for `claims`, with its active mark unless `active` is false.
    fn token(&self, claims: &TokenClaims, active: bool) -> String {
        if active {
            fs::write(self.root().join("active").join(&claims.jti), &claims.run_id).unwrap();
        }
        let key = SigningKey::from_bytes(&KEY).unwrap();
        sign(&key, claims).unwrap().expose().to_owned()
    }

    /// Send `raw` and read the whole answer.
    fn exchange(&self, raw: &[u8]) -> Answer {
        self.exchange_then(raw, |_| {})
    }

    /// Send `raw`, do `then` with the stream, and read the whole answer.
    fn exchange_then(&self, raw: &[u8], then: impl FnOnce(&TcpStream)) -> Answer {
        let mut stream = TcpStream::connect_timeout(&self.addr, LIMIT).unwrap();
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        stream.set_write_timeout(Some(LIMIT)).unwrap();
        stream.write_all(raw).unwrap();
        then(&stream);
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").expect(&text);
        let mut lines = head.lines();
        let status = lines.next().unwrap().split(' ').nth(1).unwrap();
        Answer {
            status: status.parse().unwrap(),
            headers: lines.map(|line| line.to_ascii_lowercase()).collect(),
            body: body.to_owned(),
        }
    }

    fn post(&self, path: &str, token: Option<&str>, body: &str) -> Answer {
        let auth = token
            .map(|token| format!("Authorization: Bearer {token}\r\n"))
            .unwrap_or_default();
        self.exchange(
            format!(
                "POST {path} HTTP/1.1\r\nHost: b\r\n{auth}X-Dagq-Broker-Protocol: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
    }

    /// Every audit line so far.
    fn audit(&self) -> Vec<Value> {
        let dir = self.root().join("audit");
        let mut files: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        files.sort();
        files
            .iter()
            .flat_map(|file| {
                fs::read_to_string(file)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect::<Vec<Value>>()
            })
            .collect()
    }

    fn audit_text(&self) -> String {
        let dir = self.root().join("audit");
        fs::read_dir(&dir)
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect()
    }
}

struct Answer {
    status: u16,
    headers: Vec<String>,
    body: String,
}

impl Answer {
    fn error(&self) -> ErrorBody {
        serde_json::from_str(&self.body).expect(&self.body)
    }

    /// The audit line of this answer's request.
    fn audit_line(&self, broker: &Broker) -> Value {
        let id = self.error().error.request_id.to_string();
        let lines: Vec<_> = broker
            .audit()
            .into_iter()
            .filter(|line| line["request_id"] == id.as_str())
            .collect();
        assert_eq!(lines.len(), 1, "{id}");
        lines.into_iter().next().unwrap()
    }

    fn assert_refused(&self, code: ErrorCode) {
        assert_eq!(self.status, code.http_status(), "{}", self.body);
        assert_eq!(self.error().error.code, code, "{}", self.body);
    }
}

#[test]
fn health_needs_no_token_and_is_audited() {
    let broker = Broker::start();
    let answer = broker.exchange(b"GET /v1/health HTTP/1.1\r\nHost: b\r\n\r\n");
    assert_eq!(answer.status, 200);
    let health: HealthResponse = serde_json::from_str(&answer.body).unwrap();
    assert_eq!(health, HealthResponse::ok(env!("CARGO_PKG_VERSION")));
    assert!(
        answer
            .headers
            .contains(&"x-dagq-broker-protocol: 1".to_owned())
    );
    assert!(answer.headers.contains(&format!(
        "x-dagq-broker-build: {}",
        env!("CARGO_PKG_VERSION")
    )));
    let audit = broker.audit();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["op"], "health");
    assert_eq!(audit[0]["result"], "ok");
    assert_eq!(audit[0]["run_id"], Value::Null);
    assert_eq!(audit[0]["bytes_out"], answer.body.len() as u64);
}

#[test]
fn refuses_without_a_valid_token_and_audits_without_claims() {
    let broker = Broker::start();
    let claims = broker.claims("jti-good", &BrokerCapability::ALL);
    let good = broker.token(&claims, true);
    let other_key = sign(&SigningKey::from_bytes(&[9; 32]).unwrap(), &claims)
        .unwrap()
        .expose()
        .to_owned();
    let mut expired = broker.claims("jti-expired", &BrokerCapability::ALL);
    expired.iat -= 7200;
    expired.exp -= 7200;
    let expired = broker.token(&expired, true);
    let body = r#"{"path":"a.txt"}"#;

    let cases: [(&str, Option<&str>); 5] = [
        ("no token", None),
        ("not a token", Some("hello")),
        ("another key", Some(&other_key)),
        ("tampered", Some(&good.replace("dagq1.", "dagq1.x"))),
        ("expired", Some(&expired)),
    ];
    for (name, token) in cases {
        let answer = broker.post("/v1/fs/read", token, body);
        answer.assert_refused(ErrorCode::Unauthorized);
        let line = answer.audit_line(&broker);
        assert_eq!(line["result"], "unauthorized", "{name}");
        for field in ["jti", "run_id", "task_id", "actor_id", "op", "path"] {
            assert_eq!(line[field], Value::Null, "{name}: {field}");
        }
    }

    // Not a bearer token at all.
    let answer = broker.exchange(
        b"POST /v1/fs/read HTTP/1.1\r\nAuthorization: Basic abc\r\nContent-Length: 0\r\n\r\n",
    );
    answer.assert_refused(ErrorCode::Unauthorized);

    // An unknown path says nothing without a token either.
    let answer = broker.post("/v1/git/push", None, "{}");
    answer.assert_refused(ErrorCode::Unauthorized);

    let text = broker.audit_text();
    for token in [&good, &other_key, &expired] {
        assert!(!text.contains(token.as_str()));
        let signature = token.rsplit('.').next().unwrap();
        assert!(!text.contains(signature));
    }
    assert!(!text.contains("Bearer"));
    assert!(!text.contains("hello"));
}

#[test]
fn a_signed_token_without_its_active_mark_is_refused_with_its_run_audited() {
    let broker = Broker::start();
    let claims = broker.claims("jti-revoked", &BrokerCapability::ALL);
    let token = broker.token(&claims, false);
    let answer = broker.post("/v1/fs/read", Some(&token), r#"{"path":"a"}"#);
    answer.assert_refused(ErrorCode::Unauthorized);
    assert_eq!(answer.error().error.message, "the token is not active");
    let line = answer.audit_line(&broker);
    assert_eq!(line["jti"], "jti-revoked");
    assert_eq!(line["run_id"], "run-1");
    assert_eq!(line["task_id"], 830);
    assert_eq!(line["actor_id"], "worker:run-1");
    assert!(!broker.audit_text().contains(&token));
}

#[test]
fn default_deny_refuses_unknown_routes_missing_capabilities_and_foreign_fields() {
    let broker = Broker::start();
    let read_only = broker.claims("jti-read", &[BrokerCapability::FsRead]);
    let token = broker.token(&read_only, true);

    for (method, path) in [
        ("POST", "/v1/git/push"),
        ("POST", "/v1/health"),
        ("GET", "/v1/fs/read"),
        ("POST", "/v1/fs/read/"),
        ("DELETE", "/v1/fs/read"),
    ] {
        let answer = broker.exchange(
            format!(
                "{method} {path} HTTP/1.1\r\nAuthorization: Bearer {token}\r\nX-Dagq-Broker-Protocol: 1\r\nContent-Length: 2\r\n\r\n{{}}"
            )
            .as_bytes(),
        );
        answer.assert_refused(ErrorCode::InvalidRequest);
        assert_eq!(answer.error().error.message, "no such operation");
        let line = answer.audit_line(&broker);
        assert_eq!(line["result"], "invalid_request");
        assert_eq!(line["op"], Value::Null, "{method} {path}");
        assert_eq!(line["run_id"], "run-1");
    }

    for (path, body, capability) in [
        ("/v1/fs/write", r#"{"path":"a","content":"x"}"#, "fs.write"),
        (
            "/v1/process/exec",
            r#"{"argv":["ls","-la"]}"#,
            "process.exec",
        ),
        ("/v1/git/status", "{}", "git.read"),
        ("/v1/git/commit", r#"{"message":"m"}"#, "git.write"),
    ] {
        let answer = broker.post(path, Some(&token), body);
        answer.assert_refused(ErrorCode::CapabilityDenied);
        let line = answer.audit_line(&broker);
        assert_eq!(line["result"], "capability_denied");
        assert_eq!(line["capability"], capability);
        assert_eq!(line["run_id"], "run-1");
    }

    // Who asks comes from the token: a run id in the body is an unknown field.
    let answer = broker.post(
        "/v1/fs/read",
        Some(&token),
        r#"{"path":"a","run_id":"run-2"}"#,
    );
    answer.assert_refused(ErrorCode::InvalidRequest);
    assert_eq!(answer.audit_line(&broker)["run_id"], "run-1");

    // Another protocol, or none.
    let answer = broker.exchange(
        format!(
            "POST /v1/fs/read HTTP/1.1\r\nAuthorization: Bearer {token}\r\nX-Dagq-Broker-Protocol: 2\r\nContent-Length: 2\r\n\r\n{{}}"
        )
        .as_bytes(),
    );
    answer.assert_refused(ErrorCode::InvalidRequest);
    let answer = broker.exchange(
        format!(
            "POST /v1/fs/read HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 2\r\n\r\n{{}}"
        )
        .as_bytes(),
    );
    answer.assert_refused(ErrorCode::InvalidRequest);

    // A request that is not HTTP.
    let answer = broker.exchange(b"HELLO\r\n\r\n");
    answer.assert_refused(ErrorCode::InvalidRequest);
    assert_eq!(answer.audit_line(&broker)["op"], Value::Null);

    assert!(!broker.audit_text().contains(&token));
}

#[test]
fn paths_are_confined_and_git_is_not_implemented_yet() {
    let broker = Broker::start();
    let claims = broker.claims("jti-all", &BrokerCapability::ALL);
    let token = broker.token(&claims, true);

    for path in ["../run-2/worktree/a", "/etc/passwd", "a/../../b"] {
        let body = serde_json::json!({ "path": path }).to_string();
        let answer = broker.post("/v1/fs/read", Some(&token), &body);
        answer.assert_refused(ErrorCode::WorkspaceViolation);
        let line = answer.audit_line(&broker);
        assert_eq!(line["result"], "workspace_violation");
        assert_eq!(line["path"], Value::Null);
    }
    let absolute = format!("{}/src/a.rs", broker.workspace());
    let answer = broker.post(
        "/v1/fs/read",
        Some(&token),
        &serde_json::json!({ "path": absolute }).to_string(),
    );
    answer.assert_refused(ErrorCode::BackendError);
    assert_eq!(
        answer.error().error.message,
        "src/a.rs: no such file or directory"
    );
    let line = answer.audit_line(&broker);
    assert_eq!(line["path"], "src/a.rs");
    assert_eq!(line["backend"], "fs");
    assert_eq!(line["op"], "fs.read");
    assert_eq!(line["capability"], "fs.read");
    assert_eq!(line["result"], "backend_error");
    assert_eq!(line["jti"], "jti-all");
    assert_eq!(line["task_id"], 830);

    let answer = broker.post(
        "/v1/process/exec",
        Some(&token),
        r#"{"argv":["/bin/ls","secret-arg"],"env":{"TOKEN":"env-value"}}"#,
    );
    // The default allowlist is empty.
    answer.assert_refused(ErrorCode::CapabilityDenied);
    let line = answer.audit_line(&broker);
    assert_eq!(line["backend"], "process");
    assert_eq!(line["program"], "ls");
    assert_eq!(line["argc"], 2);
    assert_eq!(line["argv_sha256"].as_str().unwrap().len(), 64);

    let answer = broker.post("/v1/git/add", Some(&token), r#"{"paths":["x","y"]}"#);
    answer.assert_refused(ErrorCode::BackendError);
    let line = answer.audit_line(&broker);
    assert_eq!(line["backend"], "git");
    assert_eq!(line["path"], Value::Null);

    let text = broker.audit_text();
    for secret in [token.as_str(), "secret-arg", "env-value", "TOKEN"] {
        assert!(!text.contains(secret), "{secret}");
    }
}

#[test]
fn a_workspace_outside_the_mounted_roots_is_refused() {
    let broker = Broker::start();
    let mut claims = broker.claims("jti-outside", &BrokerCapability::ALL);
    claims.workspace = broker
        .root()
        .join("elsewhere")
        .to_string_lossy()
        .into_owned();
    let token = broker.token(&claims, true);
    let answer = broker.post("/v1/git/status", Some(&token), "{}");
    answer.assert_refused(ErrorCode::WorkspaceViolation);

    // Under a root in its letters only.
    let mut claims = broker.claims("jti-dotdot", &BrokerCapability::ALL);
    claims.workspace = format!("{}/runs/run-1/../../elsewhere", broker.root().display());
    let token = broker.token(&claims, true);
    let answer = broker.post("/v1/git/status", Some(&token), "{}");
    answer.assert_refused(ErrorCode::WorkspaceViolation);
}

#[test]
fn an_incomplete_request_is_refused_and_audited() {
    let broker = Broker::start();
    let answer = broker.exchange_then(
        b"POST /v1/fs/read HTTP/1.1\r\nContent-Length: 10\r\n\r\nab",
        |stream| stream.shutdown(Shutdown::Write).unwrap(),
    );
    answer.assert_refused(ErrorCode::InvalidRequest);
    assert_eq!(answer.error().error.message, "the request is incomplete");
    assert_eq!(answer.audit_line(&broker)["result"], "invalid_request");

    // A connection that sends nothing is not a request.
    let stream = TcpStream::connect_timeout(&broker.addr, LIMIT).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    drop(stream);
    let health = broker.exchange(b"GET /v1/health HTTP/1.1\r\n\r\n");
    assert_eq!(health.status, 200);
    let ops: Vec<_> = broker
        .audit()
        .iter()
        .map(|line| line["op"].clone())
        .collect();
    assert_eq!(ops, [Value::Null, Value::from("health")]);
}

#[test]
fn nothing_is_answered_unaudited() {
    let broker = Broker::start();
    fs::rename(broker.root().join("audit"), broker.root().join("gone")).unwrap();
    let answer = broker.exchange(b"GET /v1/health HTTP/1.1\r\n\r\n");
    answer.assert_refused(ErrorCode::BackendError);
    assert_eq!(
        answer.error().error.message,
        "the audit could not be written"
    );
}

fn serve_output(args: &[&str], dir: &Path) -> std::process::Output {
    let key = dir.join("key");
    fs::write(&key, KEY).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq-broker"));
    command
        .arg("serve")
        .args(args)
        .arg("--key")
        .arg(&key)
        .arg("--active")
        .arg(dir.join("active"))
        .arg("--audit")
        .arg(dir.join("audit"))
        .arg("--root")
        .arg(dir.join("runs"));
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(command.output());
    });
    receive
        .recv_timeout(LIMIT)
        .expect("dagq-broker serve exits")
        .unwrap()
}

#[test]
fn refuses_to_listen_beyond_loopback_outside_the_container() {
    let dir = tempfile::tempdir().unwrap();
    for listen in ["0.0.0.0:0", "[::]:0", "192.168.0.1:0"] {
        let output = serve_output(&["--listen", listen], dir.path());
        assert_eq!(output.status.code(), Some(2), "{listen}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("not a loopback address"), "{stderr}");
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn fs_operations_work_in_the_workspace_and_every_one_is_audited() {
    let broker = Broker::start_with(&["--fs-limit-bytes", "64"]);
    let claims = broker.claims("jti-fs", &BrokerCapability::ALL);
    let token = broker.token(&claims, true);
    let workspace = PathBuf::from(broker.workspace());
    let ok = |path: &str, body: Value| {
        let answer = broker.post(path, Some(&token), &body.to_string());
        assert_eq!(answer.status, 200, "{path}: {}", answer.body);
        let line = broker.audit().pop().expect("an audit line for the request");
        assert_eq!(line["result"], "ok", "{path}");
        assert_eq!(line["run_id"], "run-1");
        assert_eq!(line["backend"], "fs");
        (serde_json::from_str::<Value>(&answer.body).unwrap(), line)
    };

    let (written, line) = ok(
        "/v1/fs/write",
        serde_json::json!({"path": "src/a.txt", "content": "alpha\nbeta\n", "create_dirs": true}),
    );
    assert_eq!(written, serde_json::json!({"bytes": 11}));
    assert_eq!(line["op"], "fs.write");
    assert_eq!(line["capability"], "fs.write");
    assert_eq!(line["path"], "src/a.txt");
    let (read, line) = ok("/v1/fs/read", serde_json::json!({"path": "src/a.txt"}));
    assert_eq!(
        read,
        serde_json::json!({"content": "alpha\nbeta\n", "lines": 2, "truncated": false})
    );
    assert_eq!(line["op"], "fs.read");
    let (edited, _) = ok(
        "/v1/fs/edit",
        serde_json::json!({"path": "src/a.txt", "old_string": "beta", "new_string": "gamma"}),
    );
    assert_eq!(edited, serde_json::json!({"replacements": 1}));
    assert_eq!(
        fs::read_to_string(workspace.join("src/a.txt")).unwrap(),
        "alpha\ngamma\n"
    );
    let (listed, line) = ok("/v1/fs/list", serde_json::json!({"path": "."}));
    assert_eq!(
        listed,
        serde_json::json!({"entries": [{"name": "src", "kind": "dir", "size": 0}]})
    );
    assert_eq!(line["path"], ".");

    std::os::unix::fs::symlink(
        broker.root().join("runs/run-2/worktree"),
        workspace.join("other"),
    )
    .unwrap();
    fs::write(workspace.join("dup.txt"), "x x\n").unwrap();
    let theirs = broker.root().join("runs/run-2/worktree/theirs");
    let refusals: [(&str, Value, ErrorCode); 7] = [
        (
            "/v1/fs/read",
            serde_json::json!({"path": "../run-2/worktree/theirs"}),
            ErrorCode::WorkspaceViolation,
        ),
        (
            "/v1/fs/read",
            serde_json::json!({"path": theirs}),
            ErrorCode::WorkspaceViolation,
        ),
        (
            "/v1/fs/write",
            serde_json::json!({"path": "/tmp/x", "content": "x"}),
            ErrorCode::WorkspaceViolation,
        ),
        (
            "/v1/fs/write",
            serde_json::json!({"path": "other/theirs", "content": "x"}),
            ErrorCode::WorkspaceViolation,
        ),
        (
            "/v1/fs/write",
            serde_json::json!({"path": "big", "content": "x".repeat(65)}),
            ErrorCode::OutputLimit,
        ),
        (
            "/v1/fs/edit",
            serde_json::json!({"path": "dup.txt", "old_string": "x", "new_string": "y"}),
            ErrorCode::InvalidRequest,
        ),
        (
            "/v1/fs/list",
            serde_json::json!({"path": ".git"}),
            ErrorCode::WorkspaceViolation,
        ),
    ];
    for (path, body, code) in refusals {
        let answer = broker.post(path, Some(&token), &body.to_string());
        answer.assert_refused(code);
        let line = answer.audit_line(&broker);
        assert_eq!(line["result"], code.as_str(), "{path} {body}");
        assert_eq!(line["jti"], "jti-fs");
        assert_eq!(line["backend"], "fs");
    }
    assert_eq!(fs::read_to_string(&theirs).unwrap(), "theirs\n");
    fs::write(workspace.join("big"), "y".repeat(65)).unwrap();
    let answer = broker.post("/v1/fs/read", Some(&token), r#"{"path":"big"}"#);
    answer.assert_refused(ErrorCode::OutputLimit);

    // One line per request, and no content in any of them.
    let audit = broker.audit();
    assert_eq!(audit.len(), 12);
    let text = broker.audit_text();
    for content in ["alpha", "gamma", "theirs\n", &token] {
        assert!(!text.contains(content), "{content}");
    }
}

#[test]
fn an_operation_that_cannot_be_audited_is_not_done() {
    let broker = Broker::start();
    let claims = broker.claims("jti-noaudit", &BrokerCapability::ALL);
    let token = broker.token(&claims, true);
    fs::rename(broker.root().join("audit"), broker.root().join("gone")).unwrap();
    let answer = broker.post(
        "/v1/fs/write",
        Some(&token),
        r#"{"path":"a.txt","content":"x"}"#,
    );
    answer.assert_refused(ErrorCode::BackendError);
    assert_eq!(
        answer.error().error.message,
        "the audit could not be written"
    );
    assert!(!PathBuf::from(broker.workspace()).join("a.txt").exists());
}

#[test]
fn process_exec_runs_argv_in_the_workspace_within_the_limits_and_is_audited() {
    let broker = Broker::start_with(&[
        "--exec-allow",
        "sh",
        "--exec-allow",
        "env",
        "--exec-allow",
        "git",
        "--exec-env",
        "ALLOWED",
        "--exec-max-timeout-secs",
        "3",
        "--exec-timeout-secs",
        "3",
        "--output-limit-bytes",
        "4096",
    ]);
    let claims = broker.claims("jti-exec", &BrokerCapability::ALL);
    let token = broker.token(&claims, true);
    let exec = |body: Value| broker.post("/v1/process/exec", Some(&token), &body.to_string());

    // argv runs in the workspace, with stdin, and the audit has its exit
    // status and bytes but none of its arguments, env or output.
    let body = serde_json::json!({
        "argv": ["sh", "-c", "pwd; cat; echo stderr-text >&2; exit 4", "secret-arg"],
        "stdin": "stdin-text\n",
        "env": {"ALLOWED": "allowed-value", "OTHER": "other-value"},
    });
    let answer = exec(body.clone());
    assert_eq!(answer.status, 200, "{}", answer.body);
    let response: Value = serde_json::from_str(&answer.body).unwrap();
    assert_eq!(response["exit_code"], 4);
    assert_eq!(
        response["stdout"],
        format!("{}\nstdin-text\n", broker.workspace())
    );
    assert_eq!(response["stderr"], "stderr-text\n");
    let line = broker.audit().pop().unwrap();
    assert_eq!(line["result"], "ok");
    assert_eq!(line["backend"], "process");
    assert_eq!(line["op"], "process.exec");
    assert_eq!(line["capability"], "process.exec");
    assert_eq!(line["run_id"], "run-1");
    assert_eq!(line["program"], "sh");
    assert_eq!(line["argc"], 4);
    assert_eq!(line["exit_code"], 4);
    assert_eq!(line["bytes_in"], body.to_string().len());
    assert_eq!(line["bytes_out"], answer.body.len());
    assert!(line["duration_ms"].is_u64());

    // The host's env does not reach the program; the allowed name does.
    let answer = exec(serde_json::json!({
        "argv": ["env"],
        "env": {"ALLOWED": "allowed-value", HOST_SECRET_NAME: "from-request"},
    }));
    assert_eq!(answer.status, 200, "{}", answer.body);
    let response: Value = serde_json::from_str(&answer.body).unwrap();
    let stdout = response["stdout"].as_str().unwrap();
    assert!(stdout.contains("ALLOWED=allowed-value"), "{stdout}");
    assert!(!stdout.contains(HOST_SECRET_NAME), "{stdout}");
    assert!(!stdout.contains(HOST_SECRET_VALUE), "{stdout}");
    assert!(
        stdout.contains("PATH=/usr/local/bin:/usr/bin:/bin"),
        "{stdout}"
    );

    // The server's maximum timeout wins over the request's, and the
    // program's group is stopped.
    let answer = exec(serde_json::json!({
        "argv": ["sh", "-c", "sleep 60 & echo $! > bg.pid; sleep 60"],
        "timeout_secs": 3600,
    }));
    answer.assert_refused(ErrorCode::Timeout);
    let line = answer.audit_line(&broker);
    assert_eq!(line["result"], "timeout");
    assert_eq!(line["exit_code"], Value::Null);
    let deadline = std::time::Instant::now() + LIMIT;
    let pid_file = PathBuf::from(broker.workspace()).join("bg.pid");
    let pid: i32 = loop {
        if let Ok(pid) = fs::read_to_string(&pid_file)
            .unwrap_or_default()
            .trim()
            .parse()
        {
            break pid;
        }
        assert!(std::time::Instant::now() < deadline, "no pid in bg.pid");
        std::thread::sleep(Duration::from_millis(50));
    };
    while Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "{pid} is still running"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Output past the limit.
    let answer = exec(serde_json::json!({"argv": ["sh", "-c", "while :; do echo out; done"]}));
    answer.assert_refused(ErrorCode::OutputLimit);
    assert_eq!(answer.audit_line(&broker)["result"], "output_limit");

    // git and what the allowlist does not hold are refused before running.
    for argv in [vec!["git", "push"], vec!["cat", "/etc/passwd"]] {
        let answer = exec(serde_json::json!({ "argv": argv }));
        answer.assert_refused(ErrorCode::CapabilityDenied);
        assert_eq!(answer.audit_line(&broker)["program"], argv[0]);
    }

    // A token without process.exec runs nothing.
    let fs_only = broker.claims("jti-fs-only", &[BrokerCapability::FsRead]);
    let fs_token = broker.token(&fs_only, true);
    let answer = broker.post(
        "/v1/process/exec",
        Some(&fs_token),
        r#"{"argv":["sh","-c","touch ran"]}"#,
    );
    answer.assert_refused(ErrorCode::CapabilityDenied);
    assert!(!PathBuf::from(broker.workspace()).join("ran").exists());

    let text = broker.audit_text();
    for secret in [
        token.as_str(),
        "secret-arg",
        "stdin-text",
        "stderr-text",
        "allowed-value",
        "other-value",
        "ALLOWED",
        HOST_SECRET_VALUE,
    ] {
        assert!(!text.contains(secret), "{secret}");
    }
}
