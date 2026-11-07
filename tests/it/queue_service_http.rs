//! The queue service's HTTP entrance on a real TCP loopback
//! (`infrastructure::queue_service_http`, ADR-t3990-1 decisions 1 and 2).

use crate::common;
use common::cli::ok;
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

#[test]
fn a_request_over_loopback_reaches_the_service_and_passes_by_its_token() {
    let _test = common::test();
    let queue = queue();
    let task = queue.run.task_id();
    let worker = token(&queue, &Principal::worker(queue.run.id(), task));
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
            service::serve_with_http(&options, listener)
        })),
    };

    let show = json!({"api_version": API_VERSION, "use_case": "show", "params": {"id": task}});
    let (status, shown) = post(address, "show", Some(&worker), &show);
    assert_eq!(status, 200, "{shown}");
    assert_eq!(shown["ok"], true, "{shown}");
    assert_eq!(shown["result"], ok(&queue.db, &["show", &task.to_string()]));
    // No header: refused, though this process is the service's own OS user.
    let (status, refused) = post(address, "show", None, &show);
    assert_eq!(status, 401, "{refused}");
    assert_eq!(refused["error"]["code"], "unauthenticated", "{refused}");
    let recorded = events(&queue.db, "queue_service_unauthenticated");
    assert_eq!(
        recorded[0]["payload"]["reason"], "missing_token",
        "{recorded:?}"
    );

    // The real service has one write line: what a token's run is read on off
    // the line, and a write on it, both pass.
    let hello = json!({"api_version": API_VERSION, "use_case": "hello"});
    let (status, answered) = post(address, "hello", Some(&worker), &hello);
    assert_eq!(status, 200, "{answered}");
    assert_eq!(answered["result"]["service"], "dagq-queue-service");
    let note = json!({"api_version": API_VERSION, "use_case": "note",
        "params": {"task": task, "text": "over http"}});
    let (status, noted) = post(address, "note", Some(&worker), &note);
    assert_eq!(status, 200, "{noted}");
    assert_eq!(noted["result"]["actor"]["role"], "worker", "{noted}");

    assert_eq!(serving.stopped()["outcome"], "stopped");
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
    assert!(service::ServiceRecord::read(queue.dir()).is_none());
    assert!(!service::socket_path(queue.dir()).exists());
}
