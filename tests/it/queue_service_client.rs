//! A client-mode `dagq` (docs/design/queue-service.md, goal 82's stage
//! (3), ADR-t1233-1 decision 7): with the queue service's socket and a
//! token's file in its environment, `dagq` opens no queue and sends its
//! command to the service as the principal of the token, which the service
//! authorizes on its side whatever role the environment names. Its `ask`,
//! `show`, `note`, findings and reads print what the command line prints;
//! a command that is none of the service's use cases, one that names a
//! queue, and every command while the service does not answer are refused
//! with the reason (fail closed).

use crate::common::service::{Queue, events, queue, start};

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use crate::common::{
    Bounded, WithoutActor,
    cli::{invoke_with, ok},
};
use dagq::{
    domain::{
        ActorContext,
        queue_service::{CREDENTIAL_FILE_ENV, Principal, SOCKET_ENV},
    },
    infrastructure::queue_service as service,
};
use serde_json::{Value, json};

/// `dagq args` in client mode as the principal of the token in
/// `credential` (none: no token named), with `role` as the `DAGQ_ROLE` it
/// claims, from a directory that is no repository: no queue resolves.
fn client(
    queue: &Queue,
    socket: &Path,
    credential: Option<&Path>,
    role: &str,
    args: &[&str],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command
        .without_actor_env()
        .current_dir(queue.dir().join("elsewhere"))
        .env(SOCKET_ENV, socket)
        .env("DAGQ_ROLE", role)
        .args(args);
    if let Some(credential) = credential {
        command.env(CREDENTIAL_FILE_ENV, credential);
    }
    command.bounded_output().unwrap()
}

/// What `client` printed, which must have succeeded.
fn answered(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The code of a refused `client`'s error.
fn refused(output: &Output) -> String {
    assert!(!output.status.success(), "it succeeded");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    error["queue_service"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("no code in {error}"))
        .to_owned()
}

/// The refusal a refused command printed, without the service's code:
/// its message and what it refused (`denied`).
fn denial(output: &Output) -> Value {
    assert!(!output.status.success(), "it succeeded");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    json!({"error": error["error"], "denied": error["denied"]})
}

fn issued(queue: &Queue, principal: &Principal) -> PathBuf {
    service::issue(queue.dir(), principal, 1).unwrap().file
}

/// A started queue, a directory beside it that is no repository, and the
/// service's socket.
fn served() -> (Queue, PathBuf) {
    let queue = queue();
    std::fs::create_dir(queue.dir().join("elsewhere")).unwrap();
    start(&queue);
    let socket = service::socket_path(queue.dir());
    (queue, socket)
}

/// Acceptance: a worker's client-mode `dagq` shows, notes, asks and reads
/// what a measure task reads through the service, as the command line
/// prints it, recorded as the worker; and is refused on the service's side
/// what its role may not do, whatever role its environment claims.
#[test]
fn a_worker_s_dagq_runs_its_commands_through_the_service_as_the_worker() {
    let (queue, socket) = served();
    let task = queue.run.task_id().to_string();
    let run = queue.run.id().to_string();
    let worker = issued(
        &queue,
        &Principal::worker(queue.run.id(), queue.run.task_id()),
    );
    let as_worker = |args: &[&str]| client(&queue, &socket, Some(&worker), "worker", args);

    for args in [
        vec!["show", task.as_str()],
        vec!["show", task.as_str(), "--full"],
        vec!["show", task.as_str(), "--events", "2"],
    ] {
        assert_eq!(
            answered(&as_worker(&args)),
            ok(&queue.db, &args),
            "{args:?}"
        );
    }
    let noted = answered(&as_worker(&["note", "--task", &task, "--text", "seen"]));
    assert_eq!(noted["payload"]["by"], "worker", "{noted}");
    let asked = answered(&as_worker(&[
        "ask",
        "--run",
        &run,
        "--kind",
        "worker_question",
        "--because",
        "scope",
        "--topic",
        "design_choice",
        "--question",
        "which way?",
        "--option",
        "a",
        "--cmux",
        "/nonexistent/cmux",
    ]));
    let asks = ok(&queue.db, &["asks", "--all"])["asks"].clone();
    assert_eq!(asks[0]["id"], asked["id"], "{asks}");
    assert_eq!(asks[0]["asked_by"], "worker", "{asks}");
    assert_eq!(asks[0]["options"], json!(["a"]));
    // `show` lists the ask of the run as the command line does.
    for args in [
        vec!["show", task.as_str()],
        vec!["show", task.as_str(), "--full"],
    ] {
        let shown = answered(&as_worker(&args));
        assert_eq!(shown["asks"][0]["id"], asked["id"], "{args:?}: {shown}");
        assert_eq!(shown["asks"][0]["run_id"], run.as_str(), "{shown}");
        assert_eq!(shown, ok(&queue.db, &args), "{args:?}");
    }
    // The worker is the actor of what it wrote.
    for kind in ["observation", "ask_opened"] {
        let written = events(&queue.db, kind);
        assert_eq!(written[0]["actor"]["role"], "worker", "{kind}: {written:?}");
        assert_eq!(written[0]["actor"]["id"], format!("worker:{run}"));
    }

    // What a measure task reads (ADR-t1233-5 decision 3), as printed.
    for args in [
        vec!["events", "--full", "--all"],
        vec!["timeline", run.as_str()],
        vec!["marks"],
        vec!["findings", "--all"],
        vec!["notes", "--task", task.as_str()],
        vec!["search", "served"],
        vec!["goal", "list"],
        vec!["list", "--all", "--full"],
        vec!["graph"],
    ] {
        assert_eq!(
            answered(&as_worker(&args)),
            ok(&queue.db, &args),
            "{args:?}"
        );
    }
    // The reads that tell the time now are read twice around the command
    // line's, until the clock did not move between them (a few tries under
    // load, each try being three processes).
    for args in [
        vec!["stats", "--full"],
        vec!["kpi"],
        vec![
            "kpi", "--period", "week", "--last", "4", "--area", "runtime", "--by", "provider",
        ],
        vec!["forecast"],
    ] {
        let mut same = false;
        for _ in 0..20 {
            let before = answered(&as_worker(&args));
            let printed = ok(&queue.db, &args);
            let after = answered(&as_worker(&args));
            if before == after {
                assert_eq!(before, printed, "{args:?}");
                same = true;
                break;
            }
        }
        assert!(same, "{args:?} kept changing");
    }
    // `graph --format d2` prints the source as the command line does.
    let d2 = as_worker(&["graph", "--format", "d2"]);
    assert!(d2.status.success());
    let printed = crate::common::cli::invoke(&queue.db, &["graph", "--format", "d2"]);
    assert_eq!(d2.stdout, printed.stdout);

    // Another task is not the worker's to note, and the observer's
    // finding is not its to record, whatever role it claims: the service
    // refuses both for the worker's principal and records the refusals as
    // the worker's.
    let other = ok(&queue.db, &["add", "another"])["id"].to_string();
    assert_eq!(
        refused(&as_worker(&["note", "--task", &other, "--text", "x"])),
        "authorization_denied"
    );
    let posing = client(
        &queue,
        &socket,
        Some(&worker),
        "observer",
        &[
            "finding",
            "record",
            "--kind",
            "stall",
            "--queue",
            "--summary",
            "s",
        ],
    );
    assert_eq!(refused(&posing), "authorization_denied");
    let denials: Vec<(Value, Value)> = events(&queue.db, "authorization_denied")
        .iter()
        .map(|event| {
            (
                event["actor"]["id"].clone(),
                event["payload"]["capability"].clone(),
            )
        })
        .collect();
    assert_eq!(
        denials,
        [
            (json!(format!("worker:{run}")), json!("note.write")),
            (json!(format!("worker:{run}")), json!("finding.record")),
        ]
    );
    // No refused write landed.
    assert!(
        ok(&queue.db, &["findings", "--all"])["findings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// Each refusal of a worker's dagq by the service prints, beside the
/// service's code, what the command line prints when it refuses the
/// worker the same (whatever role the environment claims): the message,
/// and the role, the capability and the reason its event records.
#[test]
fn a_worker_s_refusal_by_the_service_prints_as_the_command_line_s() {
    let (queue, socket) = served();
    let task = queue.run.task_id().to_string();
    let run = queue.run.id().to_string();
    let worker = issued(
        &queue,
        &Principal::worker(queue.run.id(), queue.run.task_id()),
    );
    let other = ok(&queue.db, &["add", "another"])["id"].to_string();
    let local_worker = [
        ("DAGQ_ROLE", "worker"),
        ("DAGQ_ACTOR_ID", &*format!("worker:{run}")),
        ("DAGQ_RUN_ID", &*run),
        ("DAGQ_TASK_ID", &*task),
    ];
    let note = ["note", "--task", other.as_str(), "--text", "x"];
    let ask = [
        "ask",
        "--task",
        other.as_str(),
        "--kind",
        "worker_question",
        "--because",
        "scope",
        "--topic",
        "design_choice",
        "--question",
        "q?",
    ];
    let finding = [
        "finding",
        "record",
        "--kind",
        "stall",
        "--queue",
        "--summary",
        "s",
    ];
    for (role, args, capability, reason) in [
        ("worker", &note[..], "note.write", "not on this resource"),
        ("worker", &ask[..], "ask.open", "not on this resource"),
        ("observer", &finding[..], "finding.record", "not granted"),
    ] {
        let served = client(&queue, &socket, Some(&worker), role, args);
        assert_eq!(refused(&served), "authorization_denied", "{args:?}");
        let printed = denial(&served);
        assert_eq!(
            printed["denied"],
            json!({"role": "worker", "capability": capability, "reason": reason}),
            "{args:?}"
        );
        let recorded = events(&queue.db, "authorization_denied");
        let last = &recorded.last().unwrap()["payload"];
        assert_eq!(
            json!({"role": last["role"], "capability": last["capability"], "reason": last["reason"]}),
            printed["denied"],
            "{args:?}"
        );
        let local = invoke_with(&local_worker, &queue.db, args);
        assert_eq!(printed, denial(&local), "{args:?}");
    }
}

/// A job refused on the service's side prints the refusal the command line
/// prints for the job: the jobs' message and what was refused.
#[test]
fn a_job_s_refusal_by_the_service_prints_as_the_command_line_s() {
    let (queue, socket) = served();
    let task = queue.run.task_id().to_string();
    let job = issued(&queue, &Principal::of(&ActorContext::plan_review_job(1, 1)));
    let args = ["note", "--task", task.as_str(), "--text", "x"];
    let served = client(&queue, &socket, Some(&job), "plan-review-job", &args);
    assert_eq!(refused(&served), "authorization_denied");
    let printed = denial(&served);
    assert_eq!(
        printed["denied"],
        json!({"role": "plan-review-job", "capability": "note.write", "reason": "not granted"}),
        "{printed}"
    );
    let recorded = events(&queue.db, "authorization_denied");
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    for field in ["role", "capability", "reason"] {
        assert_eq!(recorded[0]["payload"][field], printed["denied"][field]);
    }
    let local = invoke_with(
        &[
            ("DAGQ_ROLE", "plan-review-job"),
            ("DAGQ_ACTOR_ID", "plan-review-job:1:1"),
        ],
        &queue.db,
        &args,
    );
    assert_eq!(printed, denial(&local));
}

/// What the service has no use case for, a command that names a queue, a
/// call without a token and a service that does not answer are refused
/// with their reason, and no queue is opened instead.
#[test]
fn a_client_mode_dagq_opens_no_queue_and_says_why_it_refused() {
    let (queue, socket) = served();
    let task = queue.run.task_id().to_string();
    let worker = issued(
        &queue,
        &Principal::worker(queue.run.id(), queue.run.task_id()),
    );
    let db = queue.db.to_str().unwrap();
    // The planning commands are the service's, which refuses a worker's.
    for args in [
        vec!["ready", task.as_str()],
        vec!["add", "x"],
        vec!["mark", "m"],
    ] {
        let output = client(&queue, &socket, Some(&worker), "worker", &args);
        assert_eq!(refused(&output), "authorization_denied", "{args:?}");
    }
    for args in [
        vec!["ready", task.as_str(), "--inherit", "--reason", "r"],
        vec!["answer", "1", "--text", "a"],
        vec!["ask", "close", "1"],
        vec!["watch", "--timeout", "1"],
        vec!["report"],
        vec!["graph", "--out", "g.d2", "--format", "d2"],
        vec!["init"],
        vec!["observe"],
        vec!["integrate", task.as_str()],
    ] {
        let output = client(&queue, &socket, Some(&worker), "worker", &args);
        assert_eq!(refused(&output), "no_use_case", "{args:?}");
    }
    // The control side's commands are refused as such, by name.
    for (args, name) in [
        (vec!["doctor"], "dagq doctor"),
        (vec!["service", "status"], "dagq service status"),
        (vec!["planners"], "dagq planners"),
    ] {
        let output = client(&queue, &socket, Some(&worker), "worker", &args);
        assert_eq!(refused(&output), "control_side_command", "{args:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        let message = error["error"].as_str().unwrap();
        assert!(message.contains(name), "{message}");
        assert!(message.contains("control-side command"), "{message}");
        assert!(message.contains("a person or the inbox"), "{message}");
    }
    // `locate` says where the commands go and that the service answers
    // there with its API versions, and names no queue.
    let located = answered(&client(
        &queue,
        &socket,
        Some(&worker),
        "worker",
        &["locate"],
    ));
    assert_eq!(
        located,
        json!({"client_mode": true, "socket": socket, "db": null, "db_exists": null,
               "note": located["note"], "reachable": true,
               "api_version": dagq::domain::queue_service::API_VERSION,
               "min_api_version": dagq::domain::queue_service::MIN_API_VERSION,
               "api_compatible": true})
    );
    assert!(!located.to_string().contains(db), "{located}");
    let named = client(
        &queue,
        &socket,
        Some(&worker),
        "worker",
        &["--db", db, "show", &task],
    );
    assert_eq!(refused(&named), "queue_named");
    assert_eq!(
        refused(&client(&queue, &socket, None, "worker", &["show", &task])),
        "unauthenticated"
    );
    assert_eq!(
        refused(&client(
            &queue,
            &socket,
            Some(&queue.dir().join("no-such-token")),
            "worker",
            &["show", &task],
        )),
        "no_credential"
    );
    // A read the service does not take as a read (the host's d2 draws an
    // SVG) is the service's bad request.
    assert_eq!(
        refused(&client(
            &queue,
            &socket,
            Some(&worker),
            "worker",
            &["graph", "--format", "svg"],
        )),
        "bad_request"
    );
    // The queue is not touched by any of it: the task is still claimed.
    assert_eq!(
        ok(&queue.db, &["show", &task])["task"]["status"],
        "in_progress"
    );
    ok(&queue.db, &["service", "stop"]);
    let unreachable = client(&queue, &socket, Some(&worker), "worker", &["show", &task]);
    assert_eq!(refused(&unreachable), "unreachable");
    assert!(
        String::from_utf8_lossy(&unreachable.stderr).contains("does not open the queue itself")
    );
    // With no service on the socket, `locate` still answers, with why it
    // does not reach one, and asks for no `init`.
    let located = answered(&client(
        &queue,
        &socket,
        Some(&worker),
        "worker",
        &["locate"],
    ));
    assert_eq!(located["reachable"], false, "{located}");
    assert!(
        located["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty())
    );
    assert_eq!(located["db_exists"], Value::Null);
    assert_eq!(located["db"], Value::Null);
    assert!(located.get("api_version").is_none(), "{located}");
    assert!(!located.to_string().contains(db), "{located}");
    // The control side's commands touch neither the queue nor the service.
    assert_eq!(
        refused(&client(
            &queue,
            &socket,
            Some(&worker),
            "worker",
            &["doctor"]
        )),
        "control_side_command"
    );
}

/// The observer's client-mode `dagq` records, updates and resolves its
/// findings and opens the blocked ask on one through the service, as the
/// observer (goal 80's Codex observer's path, ADR-t1222-1), and may not
/// dismiss a finding.
#[test]
fn the_observer_s_dagq_writes_its_findings_through_the_service() {
    let (queue, socket) = served();
    let observer = issued(
        &queue,
        &Principal::of(&ActorContext::new(
            dagq::domain::ActorRole::Observer,
            "observer:s1",
        )),
    );
    let as_observer = |args: &[&str]| client(&queue, &socket, Some(&observer), "observer", args);
    let recorded = answered(&as_observer(&[
        "finding",
        "record",
        "--kind",
        "stall",
        "--queue",
        "--subject",
        "slots",
        "--summary",
        "idle slots",
    ]));
    assert_eq!(recorded["created"], true, "{recorded}");
    let id = recorded["id"].to_string();
    let asked = answered(&as_observer(&[
        "ask",
        "--kind",
        "blocked",
        "--because",
        "recovery_failed",
        "--finding",
        &id,
        "--question",
        "slots idle",
        "--recommend",
        "propose",
    ]));
    assert_eq!(asked["kind"], "blocked", "{asked}");
    assert_eq!(asked["asked_by"], "observer", "{asked}");
    assert_eq!(
        refused(&as_observer(&["finding", "dismiss", &id, "--reason", "no"])),
        "authorization_denied"
    );
    let resolved = answered(&as_observer(&[
        "finding",
        "resolve",
        &id,
        "--reason",
        "busy again",
    ]));
    assert_eq!(resolved["status"], "resolved", "{resolved}");
    assert_eq!(
        answered(&as_observer(&["findings", "--all", "--full"])),
        ok(&queue.db, &["findings", "--all", "--full"])
    );
    let recorded = events(&queue.db, "finding_recorded");
    assert_eq!(recorded[0]["actor"]["id"], "observer:s1", "{recorded:?}");
    assert_eq!(recorded[0]["payload"]["by"], "observer");
}

/// The plan review job reads a proposal, lints it and reads the goal
/// through the service as the command line prints them.
#[test]
fn a_plan_review_job_s_dagq_reads_through_the_service() {
    let (queue, socket) = served();
    let goal = ok(&queue.db, &["goal", "add", "Planned"])["id"].to_string();
    let draft = ok(
        &queue.db,
        &["add", "drafted", "--goal", &goal, "--verify", "true"],
    )["id"]
        .to_string();
    let job = issued(&queue, &Principal::of(&ActorContext::plan_review_job(1, 1)));
    for args in [
        vec!["proposal", "list", "--all"],
        vec!["goal", "show", goal.as_str(), "--full"],
        vec!["lint", draft.as_str()],
        vec!["related", draft.as_str()],
        vec!["candidates"],
        vec!["observe", "--history"],
    ] {
        assert_eq!(
            answered(&client(
                &queue,
                &socket,
                Some(&job),
                "plan-review-job",
                &args
            )),
            ok(&queue.db, &args),
            "{args:?}"
        );
    }
    // `status` tells when it was checked, to the second.
    let status = |mut value: Value| {
        value.as_object_mut().unwrap().remove("checked_at");
        value
    };
    assert_eq!(
        status(answered(&client(
            &queue,
            &socket,
            Some(&job),
            "plan-review-job",
            &["status"]
        ))),
        status(ok(&queue.db, &["status"]))
    );
}

/// The production sets read through the service as the command line
/// prints them, by an actor with `eval.read` (the observer); one without
/// it (the plan review job) is refused them as the command line refuses
/// it, not told the service has no use case for them.
#[test]
fn the_observer_s_dagq_reads_the_production_sets_through_the_service() {
    let (queue, socket) = served();
    let requested = ok(&queue.db, &["agent", "cases", "adr-rules"]);
    let set = requested["set_id"].to_string();
    let observer = issued(
        &queue,
        &Principal::of(&ActorContext::new(
            dagq::domain::ActorRole::Observer,
            "observer:s1",
        )),
    );
    let job = issued(&queue, &Principal::of(&ActorContext::plan_review_job(1, 1)));
    let sets = ["agent", "sets", "--agent", "adr-rules"];
    let one = ["agent", "set", set.as_str()];
    for args in [&sets[..], &one[..]] {
        let served = answered(&client(&queue, &socket, Some(&observer), "observer", args));
        assert_eq!(served, ok(&queue.db, args), "{args:?}");
        let refusal = client(&queue, &socket, Some(&job), "plan-review-job", args);
        assert_eq!(refused(&refusal), "authorization_denied", "{args:?}");
        let printed = denial(&refusal);
        assert_eq!(
            printed["denied"],
            json!({"role": "plan-review-job", "capability": "eval.read", "reason": "not granted"}),
            "{args:?}"
        );
        let local = invoke_with(
            &[
                ("DAGQ_ROLE", "plan-review-job"),
                ("DAGQ_ACTOR_ID", "plan-review-job:1:1"),
            ],
            &queue.db,
            args,
        );
        assert_eq!(printed, denial(&local), "{args:?}");
    }
    let listed = answered(&client(&queue, &socket, Some(&observer), "observer", &sets));
    assert_eq!(listed["sets"].as_array().unwrap().len(), 1, "{listed}");
}

/// Acceptance (task 1464): a read whose answer is past the service's limit
/// is refused as `response_too_large`, with the limit and how to narrow
/// that read, not as `unreachable`; the same read narrowed under the limit
/// answers as the command line prints it.
#[test]
fn a_read_past_the_answer_s_limit_is_refused_with_how_to_narrow_it() {
    use dagq::domain::queue_service::MAX_ANSWER_BYTES;
    let (queue, socket) = served();
    let worker = issued(
        &queue,
        &Principal::worker(queue.run.id(), queue.run.task_id()),
    );
    // One event longer than the limit, written as it is: a note as long goes
    // through the search index and takes seconds.
    rusqlite::Connection::open(&queue.db)
        .unwrap()
        .execute(
            "INSERT INTO run_events(task_id,kind,payload) VALUES (?1,'session_opened',?2)",
            rusqlite::params![
                queue.run.task_id().as_i64(),
                json!({"text": "x".repeat(MAX_ANSWER_BYTES)}).to_string()
            ],
        )
        .unwrap();
    let read = |args: &[&str]| client(&queue, &socket, Some(&worker), "worker", args);

    let past = read(&["events", "--full", "--all"]);
    assert_eq!(refused(&past), "response_too_large");
    assert_eq!(past.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&past.stderr).unwrap();
    let message = error["error"].as_str().unwrap();
    assert!(message.contains(&MAX_ANSWER_BYTES.to_string()), "{message}");
    for option in ["--since / --until", "--limit", "--after <cursor>"] {
        assert!(message.contains(option), "{message}");
    }

    let within = ["events", "--full", "--all", "--limit", "2"];
    assert_eq!(answered(&read(&within)), ok(&queue.db, &within));
}

/// Acceptance (task 1464): a client-mode `dagq` that reads past the limit
/// on the socket (a service of another limit) refuses it as
/// `response_too_large` with how to narrow the read, not as `unreachable`:
/// the service answered.
#[test]
fn an_answer_past_the_client_s_limit_on_the_socket_is_refused_as_too_large() {
    use crate::common::{STEP_LIMIT, within};
    use dagq::domain::queue_service::MAX_ANSWER_BYTES;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("s.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    // A service that answers one request with a line past the limit.
    let service = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(STEP_LIMIT)).unwrap();
        stream.set_write_timeout(Some(STEP_LIMIT)).unwrap();
        let mut request = Vec::new();
        let _ = BufReader::new(&stream).read_until(b'\n', &mut request);
        let _ = stream.write_all(&vec![b' '; MAX_ANSWER_BYTES + 1]);
    });

    let past = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .current_dir(dir.path())
        .env(SOCKET_ENV, &socket)
        .env("DAGQ_ROLE", "worker")
        .args(["notes"])
        .bounded_output()
        .unwrap();
    // A client that never connected leaves the service waiting: this ends it.
    let _ = UnixStream::connect(&socket);
    {
        let _waiting = within(STEP_LIMIT, "the stub service to end");
        service.join().unwrap();
    }
    assert_eq!(refused(&past), "response_too_large");
    assert_eq!(past.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&past.stderr).unwrap();
    let message = error["error"].as_str().unwrap();
    assert!(message.contains(&MAX_ANSWER_BYTES.to_string()), "{message}");
    assert!(message.contains("--limit"), "{message}");
}

/// A client-mode `dagq` whose broker answers past the limit refuses the
/// answer as `response_too_large` with how to narrow the read, not as
/// `unreachable`: the broker answered.
#[test]
fn an_answer_past_the_client_s_limit_from_the_broker_is_refused_as_too_large() {
    use crate::common::{STEP_LIMIT, within};
    use dagq::domain::queue_service::{MAX_ANSWER_BYTES, URL_ENV};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let credential = dir.path().join("token");
    std::fs::write(&credential, "a-token\n").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    // A broker that answers one request with 200 and a body past the limit.
    let broker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(STEP_LIMIT)).unwrap();
        stream.set_write_timeout(Some(STEP_LIMIT)).unwrap();
        let mut reader = BufReader::new(&stream);
        let mut length = 0;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let _ = reader.take(length).read_to_end(&mut Vec::new());
        let body = vec![b' '; MAX_ANSWER_BYTES + 1];
        // The client stops reading at the limit: what is left unwritten does
        // not matter.
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(&body);
    });

    let past = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .current_dir(dir.path())
        .env(URL_ENV, format!("http://{address}"))
        .env(CREDENTIAL_FILE_ENV, &credential)
        .env("DAGQ_RUN_ID", "r1")
        .env("DAGQ_ROLE", "worker")
        .args(["events"])
        .bounded_output()
        .unwrap();
    // A client that never connected leaves the broker waiting: this ends it.
    let _ = TcpStream::connect(address);
    {
        let _waiting = within(STEP_LIMIT, "the stub broker to end");
        broker.join().unwrap();
    }
    assert_eq!(refused(&past), "response_too_large");
    assert_eq!(past.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&past.stderr).unwrap();
    let message = error["error"].as_str().unwrap();
    assert!(message.contains(&MAX_ANSWER_BYTES.to_string()), "{message}");
    for option in ["--since / --until", "--limit", "--after <cursor>"] {
        assert!(message.contains(option), "{message}");
    }
}
