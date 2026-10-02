//! A client-mode `dagq` (docs/design/queue-service.md, goal 82's stage
//! (3), ADR-t1233-1 decision 7): with the queue service's socket and a
//! token's file in its environment, `dagq` opens no queue and sends its
//! command to the service as the principal of the token, which the service
//! authorizes on its side whatever role the environment names. Its `ask`,
//! `show`, `note`, findings and reads print what the command line prints;
//! a command that is none of the service's use cases, one that names a
//! queue, and every command while the service does not answer are refused
//! with the reason (fail closed).

use crate::queue_service::{Queue, events, queue, start};

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use crate::common::{Bounded, WithoutActor, cli::ok};
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
    for args in [
        vec!["ready", task.as_str()],
        vec!["add", "x"],
        vec!["mark", "m"],
        vec!["answer", "1", "--text", "a"],
        vec!["ask", "close", "1"],
        vec!["watch", "--timeout", "1"],
        vec!["report"],
        vec!["graph", "--out", "g.d2", "--format", "d2"],
        vec!["doctor"],
        vec!["init"],
        vec!["observe"],
        vec!["integrate", task.as_str()],
        vec!["service", "status"],
    ] {
        let output = client(&queue, &socket, Some(&worker), "worker", &args);
        assert_eq!(refused(&output), "no_use_case", "{args:?}");
    }
    // `locate` says where the commands go, and names no queue.
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
               "note": located["note"]})
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
