//! The queue service's HTTP entrance on a real TCP loopback
//! (`infrastructure::queue_service_http`, ADR-t3990-1 decisions 1 and 2).

use crate::common;
use common::service::OwnedByTest;
use common::service::{events, queue, token};
use common::{Bounded, WithoutActor};

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use dagq::{
    domain::queue_service::{API_VERSION, Principal, ServiceStart},
    infrastructure::queue_service as service,
};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);

/// The status and the body of `body` posted to the use case `name` at `to`.
fn post(to: SocketAddr, name: &str, token: Option<&str>, body: &Value) -> (u16, Value) {
    let body = body.to_string();
    let authorization = token.map_or_else(String::new, |token| {
        format!("Authorization: Bearer {token}\r\n")
    });
    let _waiting = common::within(TIMEOUT, format!("the answer to {name} over HTTP"));
    let mut stream = TcpStream::connect(to).unwrap();
    write!(
        stream,
        "POST /api/use-case/{name} HTTP/1.1\r\nHost: {to}\r\n{authorization}Content-Length: \
         {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let (head, body) = answer.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap())
}

/// The service on a thread of this process, stopped when this goes, before the
/// queue's fixture: its `service stop` signals the pid the service answers
/// with, which would end the test's process with a failing test's report.
struct Serving {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<anyhow::Result<Value>>>,
}

impl Serving {
    fn stopped(mut self) -> Value {
        self.stop.store(true, Ordering::SeqCst);
        let thread = self.thread.take().unwrap();
        let _waiting = common::within(TIMEOUT, "the service to stop");
        thread.join().unwrap().unwrap()
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _waiting = common::within(TIMEOUT, "the service to stop");
            let _ = thread.join();
        }
    }
}

/// Supplies only discovery and this test's public key; no issuer server or
/// production key injection flag is involved.
struct IdentityDocuments {
    discovery: Vec<u8>,
    jwks: Vec<u8>,
}
impl dagq::application::oidc::OidcDocumentSource for IdentityDocuments {
    fn get(
        &self,
        url: &str,
    ) -> Result<dagq::application::oidc::OidcDocument, dagq::domain::oidc::KeyFetchFailure> {
        let body = match url {
            "https://idp.example/.well-known/openid-configuration" => self.discovery.clone(),
            "https://idp.example/jwks" => self.jwks.clone(),
            _ => panic!("unexpected document URL"),
        };
        Ok(dagq::application::oidc::OidcDocument {
            body,
            cache_control: None,
        })
    }
}
fn signed_identity(identity: &dagq::domain::oidc::OidcIdentity) -> (String, IdentityDocuments) {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use ring::{
        rand::SystemRandom,
        signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair},
    };
    let rng = SystemRandom::new();
    let alg = &ECDSA_P256_SHA256_FIXED_SIGNING;
    let der = EcdsaKeyPair::generate_pkcs8(alg, &rng).unwrap();
    let key = EcdsaKeyPair::from_pkcs8(alg, der.as_ref(), &rng).unwrap();
    let point = key.public_key().as_ref();
    let jwks = json!({"keys": [{"kty": "EC", "crv": "P-256", "kid": "test",
        "x": URL_SAFE_NO_PAD.encode(&point[1..33]), "y": URL_SAFE_NO_PAD.encode(&point[33..])}]})
    .to_string()
    .into_bytes();
    let discovery = json!({"issuer": identity.issuer, "jwks_uri": "https://idp.example/jwks"})
        .to_string()
        .into_bytes();
    let header = json!({"alg": "ES256", "kid": "test"});
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let claims = json!({"iss": identity.issuer, "sub": identity.subject, "aud": "dagq", "exp": expires, "name": "Ignored IdP Name"});
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let signature = key.sign(&rng, input.as_bytes()).unwrap();
    (
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref())),
        IdentityDocuments { discovery, jwks },
    )
}

#[test]
fn a_jwt_over_loopback_is_exchanged_and_its_human_token_carries_a_use_case() {
    let _test = common::test();
    let mut queue = queue();
    // A DB-only symlink supplies no local lock proof. This actual service
    // starts as a healthy standby and cannot sweep by writing yet.
    let stored = queue.db.clone();
    let entrance = queue.dir().join("entrance");
    std::fs::create_dir(&entrance).unwrap();
    queue.db = entrance.join("queue.db");
    std::os::unix::fs::symlink(&stored, &queue.db).unwrap();
    let task = queue.run.task_id();
    use dagq::application::queue_service::MemberStore;
    use dagq::domain::{
        oidc::{OidcConfig, OidcIdentity},
        queue_service::{Member, TeamRole},
    };
    let identity = OidcIdentity {
        issuer: "https://idp.example".into(),
        subject: "person-1".into(),
    };
    dagq::infrastructure::sqlite::SqliteQueue::open(&queue.db)
        .unwrap()
        .add_member(&Member {
            identity: identity.clone(),
            role: TeamRole::Member,
            on_call: false,
            display_name: Some("Registered Person".into()),
            created_at: 0,
            disabled_at: None,
        })
        .unwrap();
    let (jwt, source) = signed_identity(&identity);
    let config = OidcConfig::new(&identity.issuer, "dagq").unwrap();
    let worker = token(&queue, &Principal::worker(queue.run.id(), task));
    let dead = common::lifecycle::dead_pid();
    let ended = Principal::supervisor(&dagq::domain::LeaseToken::new("ended"), dead);
    let stale = service::issue_by(
        queue.dir(),
        &ended,
        &dagq::domain::queue_service::TokenIssuer {
            parent: "user".into(),
            process: Some(dagq::domain::queue_service::SessionProcess {
                pid: dead,
                start: "ended".into(),
            }),
            registration: None,
        },
        1,
    )
    .unwrap();
    let stale_token = service::read_token(&stale.file).unwrap();
    let raw = rusqlite::Connection::open(&stored).unwrap();
    raw.execute(
        "UPDATE write_leader SET holder='elsewhere', lease_until_ms=9223372036854775807",
        [],
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let options = service::ServeOptions {
        db: queue.db.clone(),
        generators: dagq::infrastructure::clock::system(),
        reads: dagq::compose::service_reads(),
        writes: dagq::compose::service_writes(),
        detail: dagq::compose::service_detail(),
        commands: dagq::compose::service_commands(),
        doctor: dagq::compose::service_doctor(),
        stop: stop.clone(),
        poll: Duration::from_millis(20),
        owner: None,
        start: ServiceStart::ByHand,
        report: None,
        executable: std::env::current_exe().unwrap(),
        binding: None,
    };
    let serving = Serving {
        stop,
        thread: Some(std::thread::spawn(move || {
            service::serve_with_http_oidc(&options, listener, config, source)
        })),
    };

    let show = json!({"api_version": API_VERSION, "use_case": "show", "params": {"id": task}});
    let hello = json!({"api_version": API_VERSION, "use_case": "hello"});
    let (status, waiting) = post(address, "hello", Some(&worker), &hello);
    assert_eq!(status, 200, "{waiting}");
    assert_eq!(waiting["result"]["leader"], false, "{waiting}");
    let (_, refused_write) = post(
        address,
        "note",
        Some(&worker),
        &json!({
            "api_version": API_VERSION, "use_case": "note",
            "params": {"task": task, "text": "waiting"}
        }),
    );
    assert_eq!(
        refused_write["error"]["code"], "not_leader",
        "{refused_write}"
    );
    assert!(stale.file.exists(), "the waiting service has not swept");
    let (status, refused) = post(address, "show", Some(&stale_token), &show);
    assert_eq!(status, 401, "{refused}");
    assert_eq!(refused["error"]["code"], "unauthenticated", "{refused}");
    // A schema the service cannot read must fail closed too, before a
    // migrate can make the deferred startup sweep possible.
    raw.pragma_update(
        None,
        "user_version",
        dagq::infrastructure::sqlite::SqliteQueue::SCHEMA_VERSION - 1,
    )
    .unwrap();
    let (status, refused) = post(address, "show", Some(&stale_token), &show);
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused["ok"], false, "{refused}");
    raw.pragma_update(
        None,
        "user_version",
        dagq::infrastructure::sqlite::SqliteQueue::SCHEMA_VERSION,
    )
    .unwrap();
    raw.execute("UPDATE write_leader SET lease_until_ms=0", [])
        .unwrap();
    // The retry must wait for this transaction's write lock. Keep probing
    // the Unix entrance across the retry interval: a synchronous takeover
    // would block accept for SQLite's 5s busy timeout, beyond hello's 2s.
    raw.execute_batch("BEGIN IMMEDIATE").unwrap();
    {
        let _waiting = common::within(TIMEOUT, "Unix health while takeover waits for a DB lock");
        let began = std::time::Instant::now();
        let held = dagq::domain::write_leader::TAKEOVER_RETRY + Duration::from_secs(1);
        while began.elapsed() < held {
            let health = service::hello(&service::socket_path(queue.dir()), Duration::from_secs(2))
                .expect("DB lock wait must not block socket health");
            assert_eq!(health["leader"], false, "{health}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    raw.execute_batch("ROLLBACK").unwrap();
    {
        let _waiting = common::within(TIMEOUT, "standby to acquire the released lease");
        loop {
            let (_, health) = post(address, "hello", Some(&worker), &hello);
            assert_eq!(health["ok"], true, "{health}");
            if health["result"]["leader"] == true {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    // Acquisition does not give a leftover credential its access back,
    // even though it is still stored after the skipped startup sweep.
    assert!(stale.file.exists());
    let (status, refused) = post(address, "show", Some(&stale_token), &show);
    assert_eq!(status, 401, "{refused}");
    let (status, shown) = post(address, "show", Some(&worker), &show);
    assert_eq!(status, 200, "{shown}");
    assert_eq!(shown["ok"], true, "{shown}");
    let over_socket = common::service::call(
        &queue,
        Some(&worker),
        dagq::domain::queue_service::UseCase::Show,
        json!({"id": task}),
    );
    assert_eq!(shown["result"], over_socket["result"], "{shown}");
    // No header: refused, though this process is the service's own OS user.
    let (status, refused) = post(address, "show", None, &show);
    assert_eq!(status, 401, "{refused}");
    assert_eq!(refused["error"]["code"], "unauthenticated", "{refused}");
    let recorded = events(&queue.db, "queue_service_unauthenticated");
    assert_eq!(
        recorded.last().unwrap()["payload"]["reason"],
        "missing_token",
        "{recorded:?}"
    );

    // The real service has one write line: what a token's run is read on off
    // the line, and a write on it, both pass.
    let (status, answered) = post(address, "hello", Some(&worker), &hello);
    assert_eq!(status, 200, "{answered}");
    assert_eq!(answered["result"]["service"], "dagq-queue-service");
    let exchange =
        json!({"api_version": API_VERSION, "use_case": "exchange_token", "params": {"jwt": jwt}});
    let (status, exchanged) = post(address, "exchange_token", None, &exchange);
    assert_eq!(status, 200, "{exchanged}");
    let human = exchanged["result"]["token"].as_str().unwrap();
    let (status, human_shown) = post(address, "show", Some(human), &show);
    assert_eq!(status, 200, "{human_shown}");
    // The CLI fixture canonicalizes a DB symlink and would start another
    // service with local lock proof there. Compare the complete host-user
    // response through this service's Unix entrance instead.
    let as_user = common::service::call(
        &queue,
        None,
        dagq::domain::queue_service::UseCase::Show,
        json!({"id": task}),
    );
    assert_eq!(human_shown["result"], as_user["result"]);
    let note = json!({"api_version": API_VERSION, "use_case": "note",
        "params": {"task": task, "text": "over http"}});
    let (status, noted) = post(address, "note", Some(human), &note);
    assert_eq!(status, 200, "{noted}");
    assert_eq!(noted["result"]["actor"]["role"], "user");
    assert_eq!(
        noted["result"]["actor"]["id"],
        dagq::domain::queue_service::Principal::human(identity).actor_id
    );
    // The only server-side persistence of either credential is the hash-named
    // record. Check all service files (including the startup-read records).
    fn no_plaintext(path: &Path, secrets: &[&str]) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                no_plaintext(&path, secrets);
            } else if let Ok(bytes) = std::fs::read(&path) {
                for secret in secrets {
                    assert!(
                        !bytes
                            .windows(secret.len())
                            .any(|part| part == secret.as_bytes()),
                        "plaintext in {}",
                        path.display()
                    );
                }
            }
        }
    }
    no_plaintext(&queue.dir().join("service"), &[human, &jwt]);
    // The 100 response proves this connection was accepted before shutdown,
    // while its body is still pending. It must finish dispatch AND sending
    // after acceptance stops, rather than finding a closed write line.
    let body = note.to_string();
    let mut pending = TcpStream::connect(address).unwrap();
    pending.set_read_timeout(Some(TIMEOUT)).unwrap();
    pending.set_write_timeout(Some(TIMEOUT)).unwrap();
    write!(pending, "POST /api/use-case/note HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {human}\r\nExpect: 100-continue\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
    let mut interim = Vec::new();
    while !interim.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        pending.read_exact(&mut byte).unwrap();
        interim.push(byte[0]);
        assert!(interim.len() < 1024);
    }
    assert!(String::from_utf8(interim).unwrap().contains("100 Continue"));
    let stopped = std::thread::spawn(move || serving.stopped());
    let _waiting = common::within(TIMEOUT, "acceptance to stop before the HTTP body is sent");
    while std::os::unix::net::UnixStream::connect(service::socket_path(queue.dir())).is_ok() {
        std::thread::sleep(Duration::from_millis(10));
    }
    pending.write_all(body.as_bytes()).unwrap();
    let mut replied = String::new();
    pending.read_to_string(&mut replied).unwrap();
    let (_, answer) = replied
        .split_once("\r\n\r\n")
        .expect("a complete HTTP reply");
    assert_eq!(
        serde_json::from_str::<Value>(answer).unwrap()["ok"],
        true,
        "{replied}"
    );
    assert_eq!(stopped.join().unwrap()["outcome"], "stopped");
}

/// Exercise the CLI's actual process environment without changing the test's.
fn configured_service(
    queue: &common::service::Queue,
    address: SocketAddr,
    report: &Path,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command.without_actor_env().owned_by_test();
    for name in dagq::infrastructure::deployment_config::NAMES {
        command.env_remove(name).env_remove(format!("{name}_FILE"));
    }
    command
        .current_dir(queue.dir())
        .arg("--db")
        .arg(&queue.db)
        .args(["service", "serve", "--replacement"])
        .arg(report)
        .env("XDG_CONFIG_HOME", queue.dir())
        .env("DAGQ_STORE", "sqlite")
        .env("DAGQ_OIDC_ISSUER", "https://identity.example.com")
        .env("DAGQ_OIDC_AUDIENCE", "queue")
        .env("DAGQ_LISTEN", address.to_string())
        .env("DAGQ_TLS_CERT_FILE", queue.dir().join("cert.pem"))
        .env("DAGQ_TLS_KEY_FILE", queue.dir().join("key.pem"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[test]
fn deployment_binds_tls_before_accepting_and_continues_a_request_before_its_body() {
    let _test = common::test();
    let queue = queue();
    let task = queue.run.task_id();
    let worker = token(&queue, &Principal::worker(queue.run.id(), task));
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    std::fs::write(queue.dir().join("cert.pem"), generated.cert.pem()).unwrap();
    std::fs::write(
        queue.dir().join("key.pem"),
        generated.signing_key.serialize_pem(),
    )
    .unwrap();
    let report = queue.dir().join("start-report.json");
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = occupied.local_addr().unwrap();

    // The failed start must report refusal before publishing a socket or
    // service record; retrying with the same queue must remain possible.
    let failed = configured_service(&queue, address, &report)
        .bounded_output()
        .unwrap();
    assert!(!failed.status.success());
    let records = deployment_records(&failed);
    assert!(records.iter().any(|record| record["target"] == "dagq::telemetry::exit" && record["level"] == "ERROR"));
    assert!(!queue.dir().join("logs").exists());
    assert!(!service::log_path(queue.dir()).exists());
    let refused: service::ReplacementState =
        serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert!(
        matches!(refused, service::ReplacementState::Refused { ref error, .. } if error.contains("DAGQ_LISTEN")),
        "{refused:?}"
    );
    assert!(service::ServiceRecord::read(queue.dir()).is_none());
    assert!(!service::socket_path(queue.dir()).exists());
    drop(occupied);
    std::fs::remove_file(&report).unwrap();

    let mut child = common::KillOnDrop::new(
        configured_service(&queue, address, &report)
            .spawn()
            .unwrap(),
        "the configured TLS service",
    );
    {
        let _waiting = common::within(TIMEOUT, "the configured service to report Accepting");
        loop {
            let state = std::fs::read(&report)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<service::ReplacementState>(&bytes).ok());
            if matches!(state, Some(service::ReplacementState::Accepting { .. })) {
                break;
            }
            assert!(
                child.child().try_wait().unwrap().is_none(),
                "service exited: {state:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    let mut roots = rustls::RootCertStore::empty();
    roots.add(generated.cert.der().clone()).unwrap();
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connection =
        rustls::ClientConnection::new(Arc::new(tls), "localhost".try_into().unwrap()).unwrap();
    {
        let _waiting = common::within(TIMEOUT, "100 Continue, the TLS answer and close_notify");
        let tcp = TcpStream::connect(address).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        tcp.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut tls = rustls::StreamOwned::new(connection, tcp);
        let body = json!({"api_version": API_VERSION, "use_case": "show", "params": {"id": task}})
            .to_string();
        write!(tls, "POST /api/use-case/show HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {worker}\r\nContent-Length: {}\r\nExpect: 100-continue\r\n\r\n", body.len()).unwrap();
        tls.flush().unwrap();
        // Deliberately send no body until the interim response has arrived.
        let mut interim = [0; b"HTTP/1.1 100 Continue\r\n\r\n".len()];
        tls.read_exact(&mut interim).unwrap();
        assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
        tls.write_all(body.as_bytes()).unwrap();
        tls.flush().unwrap();
        let mut answer = String::new();
        // rustls reports UnexpectedEof here if the server omits close_notify.
        tls.read_to_string(&mut answer).unwrap();
        let (head, body) = answer.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
        let shown: Value = serde_json::from_str(body).unwrap();
        assert_eq!(shown["ok"], true, "{shown}");
        assert_eq!(shown["result"]["task"]["id"], task.as_i64());
    }

    let pid = i32::try_from(child.child().id()).unwrap();
    // SAFETY: signal the child this test started, by its exact PID.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let _waiting = common::within(TIMEOUT, "the configured TLS service to stop");
    let stopped = child.wait_with_output().unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let records = deployment_records(&stopped);
    let exit = records
        .iter()
        .find(|record| record["target"] == "dagq::telemetry::exit")
        .unwrap();
    let result: Value = serde_json::from_str(exit["fields"]["result"].as_str().unwrap()).unwrap();
    assert_eq!(result["outcome"], "stopped");
    assert!(result["pid"].is_number() && result["served"].is_number());
    assert!(!queue.dir().join("logs").exists());
    assert!(!service::log_path(queue.dir()).exists());
    assert!(service::ServiceRecord::read(queue.dir()).is_none());
    assert!(!service::socket_path(queue.dir()).exists());
}

/// Every actual process output line, including its exit, has the tracing
/// envelope; a pretty CLI response or a second error JSON fails parsing here.
fn deployment_records(output: &std::process::Output) -> Vec<Value> {
    let mut records = Vec::new();
    for bytes in [&output.stdout, &output.stderr] {
        for line in std::str::from_utf8(bytes).unwrap().lines() {
            let record: Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("non-record line: {line}: {error}"));
            assert_eq!(
                record
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                ["fields", "level", "message", "spans", "target", "timestamp"]
            );
            records.push(record);
        }
    }
    records
}
