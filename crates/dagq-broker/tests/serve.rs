//! `dagq-broker serve` as a host process (no podman) on `127.0.0.1:0`:
//! health, the refusals of default deny and their audit lines.

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
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("key"), KEY).unwrap();
        fs::create_dir_all(root.join("active")).unwrap();
        fs::create_dir_all(root.join("runs/run-1/worktree")).unwrap();
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
fn paths_are_confined_and_the_backends_are_not_implemented_yet() {
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
        "fs.read is not implemented yet"
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
    answer.assert_refused(ErrorCode::BackendError);
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
