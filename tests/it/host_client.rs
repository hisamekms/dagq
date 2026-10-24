//! The host `dagq` of the user, the inbox and a planner on a served queue
//! (ADR-t3102-2 decisions 5 and 6 (2), ADR-t3102-1 decisions 1 to 3,
//! ADR-t3105-1): the commands the queue service answers, `watch` and
//! `status` among them, go to the service of the queue named, as the
//! principal of the caller, and a service that does not answer, or serves
//! another queue, is returned as that, the DB never opened instead.

use std::{
    fs,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use crate::common;
use common::cli::{invoke, invoke_with};
use common::service::{Queue, call, events, queue, start, token};
use common::{Bounded, WithoutActor};
use dagq::application::ProcessControl;
use dagq::domain::queue_service::{CREDENTIAL_FILE_ENV, Principal, SessionProcess, UseCase};
use dagq::infrastructure::{adapters::SystemProcesses, queue_service as service};

/// The JSON a command printed on stdout when it succeeded, else on stderr.
fn printed(output: &std::process::Output) -> Value {
    let bytes = if output.status.success() {
        &output.stdout
    } else {
        &output.stderr
    };
    serde_json::from_slice(bytes)
        .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(bytes)))
}

/// `dagq` of the user run on the queue at `db` as it is: no service
/// started for it first, no token handed to it
/// ([`common::service::host_client_env`]).
fn unserved(db: &std::path::Path, args: &[&str]) -> std::process::Output {
    as_is(&[], db, args)
}

/// [`unserved`] with `env`.
fn as_is(env: &[(&str, &str)], db: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .envs(env.iter().copied())
        .arg("--db")
        .arg(db)
        .args(args)
        .bounded_output()
        .unwrap()
}

/// The code a refused command printed.
fn code(output: &std::process::Output) -> Value {
    assert!(!output.status.success(), "{}", printed(output));
    printed(output)["queue_service"]["code"].clone()
}

/// The session of the process `pid`, as `dagq inbox` binds its token.
fn session_of(pid: u32) -> SessionProcess {
    SessionProcess {
        pid,
        start: SystemProcesses.session_start(pid).unwrap(),
    }
}

/// The file of the token of the inbox of `session`, issued as `dagq inbox`
/// issues it: the path its agent is handed.
fn inbox_credential(queue: &Queue, session: SessionProcess) -> String {
    let issued = service::issue(queue.dir(), &Principal::inbox(session), 1).unwrap();
    issued.file.to_str().unwrap().to_owned()
}

/// A process that stands in for an inbox's agent, the session its token is
/// bound to.
fn stand_in_agent() -> common::KillOnDrop {
    common::KillOnDrop::new(
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        "the inbox's stand-in agent",
    )
}

/// The reasons of the queue's `queue_service_unauthenticated`, in order.
fn unauthenticated(queue: &Queue) -> Vec<Value> {
    events(&queue.db, "queue_service_unauthenticated")
        .iter()
        .map(|event| event["payload"]["reason"].clone())
        .collect()
}

#[test]
fn the_user_the_inbox_and_a_planner_go_through_the_service_and_never_open_the_db() {
    let queue = queue();
    let task = queue.run.task_id().to_string();
    // A queue no service ever ran for is not opened either: each command
    // says no service answers, `status` and `watch` too, and nothing is
    // written (ADR-t3105-1 decision 3).
    for args in [
        &["show", task.as_str()][..],
        &["status"],
        &["watch", "--timeout", "30"],
        &["note", "--task", &task, "--text", "unserved"],
    ] {
        let output = unserved(&queue.db, args);
        assert_eq!(code(&output), "unreachable", "{args:?}");
        assert!(
            printed(&output)["error"]
                .as_str()
                .unwrap()
                .contains("dagq up")
        );
    }
    assert!(events(&queue.db, "observation").is_empty());
    let missing = unserved(&queue.dir().join("missing.db"), &["list"]);
    assert!(!missing.status.success());
    assert!(!queue.dir().join("missing.db").exists());
    start(&queue);

    // The user: no token, no role, as the peer of the socket.
    let shown = invoke(&queue.db, &["show", &task]);
    assert!(shown.status.success(), "{}", printed(&shown));
    assert_eq!(
        printed(&shown),
        call(
            &queue,
            None,
            UseCase::Show,
            json!({"id": task.parse::<i64>().unwrap()})
        )["result"]
    );
    let noted = invoke(&queue.db, &["note", "--task", &task, "--text", "by hand"]);
    assert!(noted.status.success(), "{}", printed(&noted));

    // The inbox, with the token of its session.
    let credential = inbox_credential(&queue, session_of(std::process::id()));
    let as_inbox = [
        ("DAGQ_ROLE", "inbox"),
        (CREDENTIAL_FILE_ENV, credential.as_str()),
    ];
    let noted = invoke_with(
        &as_inbox,
        &queue.db,
        &["note", "--task", &task, "--text", "seen"],
    );
    assert!(noted.status.success(), "{}", printed(&noted));
    let status = invoke_with(&as_inbox, &queue.db, &["status", "--role", "inbox"]);
    assert!(status.status.success(), "{}", printed(&status));
    let notes: Vec<(Value, Value)> = events(&queue.db, "observation")
        .iter()
        .map(|event| {
            (
                event["actor"]["role"].clone(),
                event["payload"]["text"].clone(),
            )
        })
        .collect();
    assert_eq!(
        notes,
        [
            (json!("user"), json!("by hand")),
            (json!("inbox"), json!("seen"))
        ]
    );

    // A planner with no token is refused, never taken for the user nor
    // given the DB, and told what to do; one whose planner is not open is
    // refused too.
    let refused = as_is(&[("DAGQ_ROLE", "planner")], &queue.db, &["show", &task]);
    assert_eq!(code(&refused), "unauthenticated");
    assert!(
        printed(&refused)["error"]
            .as_str()
            .unwrap()
            .contains("wait for it to close")
    );
    let file = service::issue(queue.dir(), &Principal::planner(41), 1)
        .unwrap()
        .file;
    let refused = invoke_with(
        &[
            ("DAGQ_ROLE", "planner"),
            ("DAGQ_ACTOR_ID", "planner:41"),
            (CREDENTIAL_FILE_ENV, file.to_str().unwrap()),
        ],
        &queue.db,
        &["list"],
    );
    assert_eq!(code(&refused), "unauthenticated");
    assert_eq!(
        unauthenticated(&queue),
        [json!("missing_token"), json!("target_ended")]
    );

    // `watch` takes its looks from the service: an ask opened past the
    // cursor wakes it.
    let worker = token(
        &queue,
        &Principal::worker(queue.run.id(), queue.run.task_id()),
    );
    let cursor = events(&queue.db, "observation").last().unwrap()["id"].clone();
    let asked = call(
        &queue,
        Some(&worker),
        UseCase::Ask,
        json!({"kind": "worker_question", "because": "scope", "topics": ["design_choice"],
               "question": "which way?", "run_id": queue.run.id()}),
    );
    assert_eq!(asked["ok"], true, "{asked}");
    let watched = invoke(
        &queue.db,
        &[
            "watch",
            "--after",
            &cursor.to_string(),
            "--timeout",
            "30",
            "--interval",
            "1",
        ],
    );
    assert!(watched.status.success(), "{}", printed(&watched));
    let woken = printed(&watched);
    assert_eq!(woken["events"][0]["kind"], "ask_opened", "{woken}");

    // With the service stopped, nothing it answers opens the DB instead:
    // each says the service does not answer, and `watch` says so at once.
    assert!(invoke(&queue.db, &["service", "stop"]).status.success());
    for args in [
        &["show", task.as_str()][..],
        &["status"],
        &["list"],
        &["note", "--task", &task, "--text", "lost"],
    ] {
        let output = invoke(&queue.db, args);
        assert_eq!(code(&output), "unreachable", "{args:?}");
        assert!(
            printed(&output)["error"]
                .as_str()
                .unwrap()
                .contains("dagq up"),
            "{args:?}"
        );
    }
    let started = Instant::now();
    let watched = invoke_with(
        &as_inbox,
        &queue.db,
        &["watch", "--role", "inbox", "--until-attention"],
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(code(&watched), "unreachable");
    // Nothing reached the queue while the service was gone.
    assert!(
        events(&queue.db, "observation")
            .iter()
            .all(|event| event["payload"]["text"] != "lost")
    );
}

/// ADR-t3102-3 decision 2: with the service down and its outage recorded
/// beside the DB, `status` says the outage, a watch of a role returns it
/// once, and the next watch of that role waits it out to its timeout rather
/// than return it again; another outage is returned again.
#[test]
fn a_recorded_outage_is_in_status_and_returned_once_to_each_role_s_watch() {
    let queue = queue();
    start(&queue);
    assert!(invoke(&queue.db, &["service", "stop"]).status.success());
    let record = |since: i64| {
        let outage = json!({
            "since": since,
            "reason": "unreachable",
            "message": "the queue service did not answer",
        });
        fs::write(
            queue.dir().join(dagq::domain::queue_service::OUTAGE_FILE),
            outage.to_string(),
        )
        .unwrap();
    };
    record(1_790_000_000);
    let status = invoke(&queue.db, &["status"]);
    assert_eq!(code(&status), "unreachable");
    assert_eq!(
        printed(&status)["queue_service"]["outage"]["since"],
        1_790_000_000
    );
    let watch = |role: &str| invoke(&queue.db, &["watch", "--role", role, "--timeout", "2"]);

    let told = watch("inbox");
    assert_eq!(code(&told), "unreachable");
    assert_eq!(
        printed(&told)["queue_service"]["outage"]["since"],
        1_790_000_000
    );
    let started = Instant::now();
    let waited = watch("inbox");
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert!(waited.status.success(), "{}", printed(&waited));
    assert_eq!(printed(&waited)["outage_since"], 1_790_000_000);
    assert_eq!(printed(&waited)["events"], json!([]));
    // Another role is told of it once too, and `status` says it still.
    assert_eq!(code(&watch("planner")), "unreachable");
    assert!(watch("planner").status.success());
    assert_eq!(
        printed(&invoke(&queue.db, &["status"]))["queue_service"]["outage"]["since"],
        1_790_000_000
    );

    record(1_790_000_060);
    assert_eq!(code(&watch("inbox")), "unreachable");
}

/// A service that takes the call and never answers holds a watch no
/// longer than its timeout and the floor of a look: each look is given
/// what is left of it, and the watch ends as unreachable then rather than
/// wait on the call.
#[test]
fn a_watch_of_a_service_that_never_answers_is_unreachable_within_its_timeout() {
    let queue = queue();
    let socket = service::socket_path(queue.dir());
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    // Bound and never accepted: a connection is taken into its backlog and
    // its request is never read nor answered.
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let started = Instant::now();
    let watched = unserved(&queue.db, &["watch", "--timeout", "2"]);
    let took = started.elapsed();
    assert_eq!(code(&watched), "unreachable");
    assert!(took >= Duration::from_secs(1), "{took:?}");
    assert!(took < Duration::from_secs(20), "{took:?}");
    let _ = fs::remove_file(&socket);
}

/// A service that sends its answer a byte at a time holds a watch no
/// longer either: the look's limit is one deadline across the call, not a
/// wait for each read.
#[test]
fn a_watch_of_a_service_that_trickles_its_answer_is_unreachable_within_its_timeout() {
    use std::io::{BufRead, BufReader, Write};
    let queue = queue();
    let socket = service::socket_path(queue.dir());
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let trickle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        // Until the caller hangs up, or 20 s at most.
        for _ in 0..80 {
            if (&stream).write_all(b" ").is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
    let started = Instant::now();
    let watched = unserved(&queue.db, &["watch", "--timeout", "2"]);
    let took = started.elapsed();
    assert_eq!(code(&watched), "unreachable");
    assert!(took < Duration::from_secs(10), "{took:?}");
    let _waiting = common::within(common::STEP_LIMIT, "the trickling service to end");
    trickle.join().unwrap();
    let _ = fs::remove_file(&socket);
}

/// A listener whose backlog is full holds a watch no longer either: the
/// connection is tried until the look's deadline (where a blocking connect
/// waits for room), or refused at once.
#[test]
fn a_watch_of_a_service_whose_backlog_is_full_is_unreachable_within_its_timeout() {
    use std::os::fd::AsRawFd;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let queue = queue();
    let socket = service::socket_path(queue.dir());
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    // SAFETY: listen(2) on the listener's own descriptor, with no pointer:
    // the least backlog, never accepted.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let done = Arc::new(AtomicBool::new(false));
    // Connections that fill the backlog and hold it; one that finds it
    // full waits in its connect, or is refused, and ends with the test.
    for _ in 0..8 {
        let socket = socket.clone();
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let _held = std::os::unix::net::UnixStream::connect(&socket);
            while !done.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
        });
    }
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    let watched = unserved(&queue.db, &["watch", "--timeout", "2"]);
    let took = started.elapsed();
    done.store(true, Ordering::Relaxed);
    assert_eq!(code(&watched), "unreachable");
    assert!(took < Duration::from_secs(10), "{took:?}");
    drop(listener);
    let _ = fs::remove_file(&socket);
}

/// A service that takes the call and never answers holds `status` no
/// longer than the probe's wait: the service has to answer `hello` before
/// the look is sent (ADR-t3102-3 decision 2).
#[test]
fn a_status_of_a_service_that_never_answers_is_unreachable_at_once() {
    let queue = queue();
    let socket = service::socket_path(queue.dir());
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    // Bound and never accepted, as in the watch's test above.
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let started = Instant::now();
    let status = unserved(&queue.db, &["status"]);
    let took = started.elapsed();
    assert_eq!(code(&status), "unreachable");
    assert!(took < Duration::from_secs(10), "{took:?}");
    let _ = fs::remove_file(&socket);
}

/// A queue at an older schema within the floor is read through its
/// service from a copy migrated in memory, as the command line read it:
/// the user's reads, the evals' and the case sets' among them, and a
/// planner's, whose planner is read on the same copy. What writes is
/// refused for the schema, and the file is left at its schema.
#[test]
fn an_older_queue_is_read_through_the_service_by_the_user_and_a_planner() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let raw = rusqlite::Connection::open(&db).unwrap();
    for migration in &dagq::infrastructure::schema::MIGRATIONS[..23] {
        raw.execute_batch(migration).unwrap();
    }
    raw.pragma_update(None, "application_id", 0x4354_5131)
        .unwrap();
    raw.pragma_update(None, "user_version", 23).unwrap();
    raw.execute(
        "INSERT INTO planners(origin, created_at) VALUES ('runtime', 0)",
        [],
    )
    .unwrap();
    for args in [
        &["list"][..],
        &["status"],
        &["agent", "results"],
        &["agent", "sets"],
    ] {
        let output = invoke(&db, args);
        assert!(output.status.success(), "{args:?}: {}", printed(&output));
    }
    let issued = service::issue(dir.path(), &Principal::planner(1), 1).unwrap();
    let as_planner = [
        ("DAGQ_ROLE", "planner"),
        ("DAGQ_ACTOR_ID", "planner:1"),
        (CREDENTIAL_FILE_ENV, issued.file.to_str().unwrap()),
    ];
    let listed = invoke_with(&as_planner, &db, &["list"]);
    assert!(listed.status.success(), "{}", printed(&listed));
    let noted = invoke_with(
        &as_planner,
        &db,
        &["note", "--task", "1", "--text", "older"],
    );
    assert_eq!(code(&noted), "failed");
    assert!(
        printed(&noted)["error"]
            .as_str()
            .unwrap()
            .contains("dagq migrate"),
        "{}",
        printed(&noted)
    );
    let version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 23);
    let _ = invoke(&db, &["service", "stop"]);
}

/// ADR-t3102-1 decisions 2 and 3: the inbox's token ends when the inbox is
/// opened again (the issue revokes the one before) and when its session's
/// process ends, and the refusal says to open the inbox again.
#[test]
fn an_inbox_s_token_ends_when_it_is_opened_again_and_with_its_session() {
    let queue = queue();
    let task = queue.run.task_id().to_string();
    start(&queue);
    let mut earlier_agent = stand_in_agent();
    let mut agent = stand_in_agent();
    let as_inbox = |credential: &str| {
        invoke_with(
            &[("DAGQ_ROLE", "inbox"), (CREDENTIAL_FILE_ENV, credential)],
            &queue.db,
            &["show", &task],
        )
    };
    let earlier = inbox_credential(&queue, session_of(earlier_agent.child().id()));
    assert!(as_inbox(&earlier).status.success());
    let earlier_token = fs::read_to_string(&earlier).unwrap();
    // Opened again by another session: the path the earlier session was
    // handed holds no token, neither the new one nor its own.
    let again = inbox_credential(&queue, session_of(agent.child().id()));
    assert_ne!(earlier, again);
    assert_eq!(code(&as_inbox(&earlier)), "no_credential");
    assert_eq!(
        call(
            &queue,
            Some(earlier_token.trim()),
            UseCase::Show,
            json!({"id": task.parse::<i64>().unwrap()})
        )["error"]["code"],
        "unauthenticated"
    );
    assert!(as_inbox(&again).status.success());
    // The session ends: so does its token.
    agent.child().kill().unwrap();
    {
        let _waiting = common::within(common::STEP_LIMIT, "the inbox's stand-in agent to exit");
        agent.child().wait().unwrap();
    }
    let ended = as_inbox(&again);
    assert_eq!(code(&ended), "unauthenticated");
    assert!(
        printed(&ended)["error"]
            .as_str()
            .unwrap()
            .contains("dagq inbox")
    );
    assert_eq!(
        unauthenticated(&queue),
        [json!("unknown_token"), json!("session_ended")]
    );
}

/// A `--db` beside the served queue's is not answered by that queue's
/// service: the service refuses it as `wrong_queue` before it reads
/// anything, one that is not there is no queue, and neither is changed.
#[test]
fn a_queue_named_beside_the_served_one_is_refused_and_neither_is_changed() {
    let queue = queue();
    let task = queue.run.task_id().to_string();
    let other = queue.dir().join("other.db");
    drop(common::template::queue(&other));
    start(&queue);
    let missing = queue.dir().join("nonexistent.db");
    for args in [
        &["show", task.as_str()][..],
        &["note", "--task", &task, "--text", "elsewhere"],
    ] {
        let output = invoke(&other, args);
        assert_eq!(code(&output), "wrong_queue", "{args:?}");
        // A DB that is not there is no queue, said before anything is sent.
        let output = invoke(&missing, args);
        assert!(!output.status.success());
        assert!(
            printed(&output)["error"]
                .as_str()
                .unwrap()
                .starts_with("no queue at"),
            "{args:?}"
        );
    }
    assert!(!missing.exists());
    for db in [&queue.db, &other] {
        assert!(events(db, "observation").is_empty(), "{}", db.display());
    }
    // The queue the service serves is answered, named by a path relative
    // to the caller's directory too.
    assert!(invoke(&queue.db, &["show", &task]).status.success());
    let relative = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .current_dir(queue.dir())
        .args([
            "--db",
            queue.db.file_name().unwrap().to_str().unwrap(),
            "show",
            &task,
        ])
        .bounded_output()
        .unwrap();
    assert!(relative.status.success(), "{}", printed(&relative));
    // A link elsewhere names the served queue's DB: its service answers
    // it, and once stopped the link is no way around it.
    let elsewhere = tempfile::tempdir().unwrap();
    let link = elsewhere.path().join("link.db");
    std::os::unix::fs::symlink(&queue.db, &link).unwrap();
    assert!(invoke(&link, &["show", &task]).status.success());
    assert!(invoke(&queue.db, &["service", "stop"]).status.success());
    assert_eq!(code(&invoke(&link, &["show", &task])), "unreachable");
}

/// The token of an inbox or a planner goes to no service but its own
/// queue's: a `--db` naming another queue is refused before anything of
/// the token is sent, so the other queue's service never sees it.
#[test]
fn a_token_is_sent_to_no_service_but_its_queue_s() {
    let (own, other) = (queue(), queue());
    let task = own.run.task_id().to_string();
    let inbox = inbox_credential(&own, session_of(std::process::id()));
    let planner = service::issue(own.dir(), &Principal::planner(1), 1)
        .unwrap()
        .file;
    let planner = planner.to_str().unwrap();
    for env in [
        &[
            ("DAGQ_ROLE", "inbox"),
            (CREDENTIAL_FILE_ENV, inbox.as_str()),
        ][..],
        &[
            ("DAGQ_ROLE", "planner"),
            ("DAGQ_ACTOR_ID", "planner:1"),
            (CREDENTIAL_FILE_ENV, planner),
        ],
    ] {
        for args in [
            &["show", task.as_str()][..],
            &["status"],
            &["watch", "--timeout", "5"],
        ] {
            let output = invoke_with(env, &other.db, args);
            assert_eq!(code(&output), "wrong_queue", "{env:?} {args:?}");
        }
    }
    assert!(unauthenticated(&other).is_empty());
    // Said before anything is sent, a `hello` neither: the other queue's
    // service being down changes nothing.
    assert!(invoke(&other.db, &["service", "stop"]).status.success());
    let status = as_is(
        &[
            ("DAGQ_ROLE", "inbox"),
            (CREDENTIAL_FILE_ENV, inbox.as_str()),
        ],
        &other.db,
        &["status"],
    );
    assert_eq!(code(&status), "wrong_queue");
    // Its own queue's service takes it.
    let shown = invoke_with(
        &[
            ("DAGQ_ROLE", "inbox"),
            (CREDENTIAL_FILE_ENV, inbox.as_str()),
        ],
        &own.db,
        &["show", &task],
    );
    assert!(shown.status.success(), "{}", printed(&shown));
}

/// Acceptance (1): `requests`, `replans` and `ci failures` are reads of the
/// service, answered as the command line answered them, and refused as
/// unreachable when the service is stopped.
#[test]
fn requests_replans_ci_failures_and_planners_are_read_through_the_service() {
    let queue = queue();
    let request = dagq::infrastructure::sqlite::SqliteQueue::open(&queue.db)
        .unwrap()
        .record_plan_request(
            &dagq::domain::plan_request::NewPlanRequest {
                text: "plan it".into(),
                note: None,
                refs: Vec::new(),
                priority: None,
            },
            "inbox",
            "inbox",
        )
        .unwrap();
    // As the command line reads them before a service ran.
    let before: Vec<Value> = [
        &["requests"][..],
        &["requests", &request.id.to_string()],
        &["replans", "--all"],
        &["ci", "failures"],
        &["planners", "--all"],
    ]
    .iter()
    .map(|args| printed(&invoke(&queue.db, args)))
    .collect();
    assert_eq!(before[0]["requests"][0]["id"], request.id.as_i64());
    start(&queue);
    for (args, answered) in [
        &["requests"][..],
        &["requests", &request.id.to_string()],
        &["replans", "--all"],
        &["ci", "failures"],
        &["planners", "--all"],
    ]
    .iter()
    .zip(&before)
    {
        let output = invoke(&queue.db, args);
        assert!(output.status.success(), "{args:?}: {}", printed(&output));
        assert_eq!(&printed(&output), answered, "{args:?}");
    }
    assert!(invoke(&queue.db, &["service", "stop"]).status.success());
    assert!(before[4]["planners"].is_array());
    for args in [
        &["requests"][..],
        &["replans"],
        &["ci", "failures"],
        &["planners"],
    ] {
        assert_eq!(code(&invoke(&queue.db, args)), "unreachable", "{args:?}");
    }
}

/// Acceptance (1): a planner's `ask --request` goes through the service as
/// that planner, as the command line lets it: on the request it was opened
/// for, and refused and recorded on another planner's; with the service
/// stopped it is unreachable, the DB not opened instead.
#[test]
fn a_planner_s_ask_on_its_request_goes_through_the_service_as_that_planner() {
    use dagq::application::{PlanRequestStore, RequestPlannerStart};
    use dagq::domain::{PlannerOrigin, plan_request::NewPlanRequest};
    let queue = queue();
    let mut sqlite = dagq::infrastructure::sqlite::SqliteQueue::open(&queue.db).unwrap();
    let request = sqlite
        .record_plan_request(
            &NewPlanRequest {
                text: "plan it".into(),
                note: None,
                refs: Vec::new(),
                priority: None,
            },
            "inbox",
            "inbox",
        )
        .unwrap();
    let RequestPlannerStart::Opened { planner, .. } =
        sqlite.open_request_planner(request.id, None).unwrap()
    else {
        panic!("no planner was opened for the request");
    };
    let other = sqlite.open_planner(PlannerOrigin::Runtime, None).unwrap();
    start(&queue);
    let ask = |id: i64| {
        let issued = service::issue(queue.dir(), &Principal::planner(id), 1).unwrap();
        invoke_with(
            &[
                ("DAGQ_ROLE", "planner"),
                ("DAGQ_ACTOR_ID", &format!("planner:{id}")),
                (CREDENTIAL_FILE_ENV, issued.file.to_str().unwrap()),
            ],
            &queue.db,
            &[
                "ask",
                "--kind",
                "planner_question",
                "--request",
                &request.id.to_string(),
                "--because",
                "scope",
                "--question",
                "a new goal for this?",
            ],
        )
    };
    assert_eq!(code(&ask(other.id.as_i64())), "authorization_denied");
    let denied = events(&queue.db, "authorization_denied");
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert_eq!(denied[0]["actor"]["id"], format!("planner:{}", other.id));
    assert_eq!(
        denied[0]["payload"]["resource"]["planner"],
        planner.id.as_i64()
    );
    let asked = ask(planner.id.as_i64());
    assert!(asked.status.success(), "{}", printed(&asked));
    let opened = events(&queue.db, "ask_opened");
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert_eq!(opened[0]["actor"]["id"], format!("planner:{}", planner.id));
    assert!(invoke(&queue.db, &["service", "stop"]).status.success());
    assert_eq!(code(&ask(planner.id.as_i64())), "unreachable");
    assert_eq!(events(&queue.db, "ask_opened").len(), 1);
}

/// `graph --format svg` of a served queue: the service reads the d2 source
/// and the host's d2 draws it, as the command line drew it.
#[test]
fn a_served_queue_s_graph_svg_is_drawn_by_the_host_s_d2_from_the_service_s_source() {
    let queue = queue();
    let bin = queue.dir().join("bin");
    fs::create_dir_all(&bin).unwrap();
    for (name, body) in [
        ("d2plugin-tala", "exit 0"),
        (
            "d2",
            "[ \"$1\" = --layout=tala ] || exit 9\nprintf '<svg>'; cat; printf '</svg>'",
        ),
    ] {
        common::template::script(&bin.join(name), format!("#!/bin/sh\n{body}\n"));
    }
    let drawn = invoke(&queue.db, &["graph", "--format", "svg"]);
    assert!(drawn.status.success());
    start(&queue);
    let source = invoke(&queue.db, &["graph", "--format", "d2"]);
    assert!(source.status.success());
    let svg = invoke(&queue.db, &["graph", "--format", "svg"]);
    assert!(
        svg.status.success(),
        "{}",
        String::from_utf8_lossy(&svg.stderr)
    );
    assert_eq!(svg.stdout, drawn.stdout);
    assert_eq!(
        String::from_utf8(svg.stdout).unwrap(),
        format!("<svg>{}</svg>", String::from_utf8(source.stdout).unwrap())
    );
}
