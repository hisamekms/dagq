//! The queue service's HTTP entrance on a real TCP loopback
//! (`infrastructure::queue_service_http`, ADR-t3990-1 decisions 1 and 2).

use crate::common;
use common::cli::ok;
use common::service::{events, queue, token};

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
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
