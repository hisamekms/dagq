//! The queue service's reads of the whole queue
//! (docs/design/queue-service.md, ADR-t1233-5 decisions 1 to 3): every
//! read a read role's job or a worker runs (`events`, `timeline`, `stats`,
//! `kpi`, `marks`, `forecast`, `search`, `related`, `findings`, `goal
//! show`, ...) is answered on the service's side for the principal of a
//! token, with the JSON the command line prints; the worker reads the
//! whole queue as the measure tasks do; and what is no read use case
//! (`watch`, a file to write, a program to run) is refused.

use crate::queue_service::{Queue, call, queue, start, token};

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use crate::common::cli::{invoke, ok};
use dagq::{
    domain::{
        ActorContext, ActorRole,
        queue_service::{API_VERSION, Principal, UseCase},
    },
    infrastructure::queue_service as service,
};
use serde_json::{Value, json};

/// A queue whose reads show something: a goal, a note on the claimed
/// task, a finding and a mark besides the claimed run.
fn seeded() -> (Queue, i64, i64) {
    let queue = queue();
    let task = queue.run.task_id().as_i64();
    let goal = ok(
        &queue.db,
        &["goal", "add", "Reads", "--description", "served reads"],
    )["id"]
        .as_i64()
        .unwrap();
    ok(
        &queue.db,
        &["note", "--task", &task.to_string(), "--text", "seen served"],
    );
    ok(
        &queue.db,
        &[
            "finding",
            "record",
            "--kind",
            "stall",
            "--queue",
            "--summary",
            "a stall",
        ],
    );
    ok(&queue.db, &["mark", "parallel 3→2", "--note", "a test"]);
    start(&queue);
    (queue, task, goal)
}

fn answer(queue: &Queue, token: &str, use_case: UseCase, params: &Value) -> Value {
    let response = call(queue, Some(token), use_case, params.clone());
    assert_eq!(response["ok"], true, "{use_case:?} {params}: {response}");
    response["result"].clone()
}

/// The service's answer is what the command line prints for `args`. A
/// read that tells the time now is read again when the clock moved
/// between the two service calls around the command line's. When it moves
/// on every try (a loaded host makes each call cross a second), the
/// answers are compared without the fields that only measure the clock.
fn same_as_cli(queue: &Queue, token: &str, use_case: UseCase, params: &Value, args: &[&str]) {
    for _ in 0..3 {
        let before = answer(queue, token, use_case, params);
        let printed = ok(&queue.db, args);
        let after = answer(queue, token, use_case, params);
        if before == after {
            assert_eq!(before, printed, "{use_case:?} {params} against {args:?}");
            return;
        }
        let before = without_clock(before);
        if before == without_clock(after) {
            assert_eq!(
                before,
                without_clock(printed),
                "{use_case:?} {params} against {args:?}"
            );
            return;
        }
    }
    panic!("{use_case:?} {params} kept changing");
}

/// `value` without the fields that grow with the clock alone: the length
/// of a window that ends now (`landing_utilization`'s `window_secs` of
/// the period not over yet, in `stats` and every period of `kpi`).
fn without_clock(mut value: Value) -> Value {
    fn strip(value: &mut Value) {
        match value {
            Value::Object(object) => {
                object.remove("window_secs");
                object.values_mut().for_each(strip);
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(&mut value);
    value
}

#[test]
fn a_worker_reads_what_a_measure_task_reads_as_the_command_line_prints_it() {
    let (queue, _, _) = seeded();
    let run = queue.run.id().to_string();
    let worker = token(
        &queue,
        &Principal::worker(queue.run.id(), queue.run.task_id()),
    );
    for (use_case, params, args) in [
        (
            UseCase::Stats,
            json!({"full": true}),
            vec!["stats", "--full"],
        ),
        (
            UseCase::Events,
            json!({"full": true, "all": true}),
            vec!["events", "--full", "--all"],
        ),
        (
            UseCase::Events,
            json!({"full": true}),
            vec!["events", "--full"],
        ),
        (
            UseCase::Timeline,
            json!({"run": run}),
            vec!["timeline", &run],
        ),
        (UseCase::Kpi, json!({}), vec!["kpi"]),
        (
            UseCase::Kpi,
            json!({"period": "week", "last": 4, "area": ["runtime"], "by": ["provider"]}),
            vec![
                "kpi", "--period", "week", "--last", "4", "--area", "runtime", "--by", "provider",
            ],
        ),
        (UseCase::Marks, json!({}), vec!["marks"]),
        (UseCase::Forecast, json!({}), vec!["forecast"]),
    ] {
        same_as_cli(&queue, &worker, use_case, &params, &args);
    }
    // The worker reads the whole queue, not only its own run (ADR-t1233-5
    // decision 3), and no read is recorded as a refusal.
    assert!(
        ok(
            &queue.db,
            &["events", "--all", "--kind", "authorization_denied"]
        )["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn the_read_roles_read_what_their_prompts_name_as_the_command_line_prints_it() {
    let (queue, task, goal) = seeded();
    let run = queue.run.id().to_string();
    let (task_text, goal_text) = (task.to_string(), goal.to_string());
    let plan_review = token(&queue, &Principal::of(&ActorContext::plan_review_job(1, 1)));
    let reads: Vec<(UseCase, Value, Vec<&str>)> = vec![
        (UseCase::List, json!({}), vec!["list"]),
        (
            UseCase::List,
            json!({"all": true, "full": true, "limit": 5}),
            vec!["list", "--all", "--full", "--limit", "5"],
        ),
        (UseCase::Candidates, Value::Null, vec!["candidates"]),
        (UseCase::Graph, json!({}), vec!["graph"]),
        (UseCase::Status, json!({}), vec!["status"]),
        (
            UseCase::Status,
            json!({"role": "inbox"}),
            vec!["status", "--role", "inbox"],
        ),
        (UseCase::Asks, json!({"all": true}), vec!["asks", "--all"]),
        (
            UseCase::Events,
            json!({"full": true, "all": true, "task": task, "limit": 3}),
            vec![
                "events", "--full", "--all", "--task", &task_text, "--limit", "3",
            ],
        ),
        (
            UseCase::Events,
            json!({"kind": ["observation"], "since": "2000-01-01"}),
            vec!["events", "--kind", "observation", "--since", "2000-01-01"],
        ),
        (
            UseCase::Timeline,
            json!({"run": run, "full": true, "gap": 1}),
            vec!["timeline", &run, "--full", "--gap", "1"],
        ),
        (UseCase::Stats, json!({}), vec!["stats"]),
        (
            UseCase::Stats,
            json!({"since": "1", "goal": goal}),
            vec!["stats", "--since", "1", "--goal", &goal_text],
        ),
        (
            UseCase::Kpi,
            json!({"compare": "@1790000000", "window": 3}),
            vec!["kpi", "--compare", "@1790000000", "--window", "3"],
        ),
        (
            UseCase::Forecast,
            json!({"goal": goal, "trials": 50}),
            vec!["forecast", "--goal", &goal_text, "--trials", "50"],
        ),
        (
            UseCase::Notes,
            json!({"task": task}),
            vec!["notes", "--task", &task_text],
        ),
        (
            UseCase::Marks,
            json!({"since": "1"}),
            vec!["marks", "--since", "1"],
        ),
        (UseCase::Findings, json!({}), vec!["findings"]),
        (
            UseCase::Findings,
            json!({"all": true, "full": true, "queue": true, "kind": ["stall"]}),
            vec!["findings", "--all", "--full", "--queue", "--kind", "stall"],
        ),
        (
            UseCase::Search,
            json!({"query": "served"}),
            vec!["search", "served"],
        ),
        (
            UseCase::Search,
            json!({"query": "served", "kind": ["task"], "full": true}),
            vec!["search", "served", "--kind", "task", "--full"],
        ),
        (
            UseCase::Related,
            json!({"task": task, "status": ["in_progress"]}),
            vec!["related", &task_text, "--status", "in_progress"],
        ),
        (UseCase::GoalList, json!({}), vec!["goal", "list"]),
        (
            UseCase::GoalShow,
            json!({"id": goal}),
            vec!["goal", "show", &goal_text],
        ),
        (
            UseCase::GoalShow,
            json!({"id": goal, "full": true}),
            vec!["goal", "show", &goal_text, "--full"],
        ),
        (
            UseCase::Lint,
            json!({"tasks": [task]}),
            vec!["lint", &task_text],
        ),
        (
            UseCase::ObserveHistory,
            json!({}),
            vec!["observe", "--history"],
        ),
    ];
    for (use_case, params, args) in &reads {
        same_as_cli(&queue, &plan_review, *use_case, params, args);
    }
    // `graph --format d2` prints its source as is.
    let d2 = answer(
        &queue,
        &plan_review,
        UseCase::Graph,
        &json!({"format": "d2"}),
    );
    let printed = invoke(&queue.db, &["graph", "--format", "d2"]);
    assert!(printed.status.success());
    assert_eq!(
        d2[dagq::view::RAW_STDOUT],
        String::from_utf8(printed.stdout).unwrap()
    );

    // Every read role and the worker reads every one of them on the
    // service's side (ADR-t728-1's `queue.read`, ADR-t1233-5 decision 3).
    let principals = [
        Principal::of(&ActorContext::review_job(queue.run.id(), 1)),
        Principal::of(&ActorContext::recovery_job(queue.run.id(), "failed", 1)),
        Principal::of(&ActorContext::goal_review_job(goal, 1)),
        Principal::of(&ActorContext::throughput_review_job(
            "hourly",
            "2026-10-02T09",
        )),
        Principal::of(&ActorContext::instance(ActorRole::Observer, "s1")),
        Principal::worker(queue.run.id(), queue.run.task_id()),
    ];
    for principal in &principals {
        let token = token(&queue, principal);
        for (use_case, params, _) in &reads {
            answer(&queue, &token, *use_case, params);
        }
    }
    assert!(
        ok(
            &queue.db,
            &["events", "--all", "--kind", "authorization_denied"]
        )["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// One raw request line, as a client of the API sends it.
fn raw(queue: &Queue, request: &Value) -> Value {
    let mut stream = UnixStream::connect(service::socket_path(queue.dir())).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    writeln!(stream, "{request}").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn what_is_no_read_use_case_is_refused_on_the_service_s_side() {
    let (queue, task, _) = seeded();
    let worker = token(
        &queue,
        &Principal::worker(queue.run.id(), queue.run.task_id()),
    );
    // No use case watches, writes a report or runs another program.
    for use_case in ["watch", "report", "locate", "doctor", "goal_add", "mark"] {
        let response = raw(
            &queue,
            &json!({"api_version": API_VERSION, "token": worker, "use_case": use_case}),
        );
        assert_eq!(response["ok"], false, "{use_case}: {response}");
        assert_eq!(response["error"]["code"], "bad_request", "{use_case}");
    }
    for (use_case, params) in [
        (
            UseCase::Graph,
            json!({"format": "svg", "out": "/tmp/graph.svg"}),
        ),
        (UseCase::Stats, json!({"cmux": "/bin/sh"})),
        (UseCase::Events, json!({"limit": 0})),
        (UseCase::Findings, json!({"task": task, "queue": true})),
        (UseCase::Timeline, json!({})),
    ] {
        let response = call(&queue, Some(&worker), use_case, params.clone());
        assert_eq!(
            response["error"]["code"], "bad_request",
            "{params}: {response}"
        );
    }
    // A read of what is not there fails as the command line does.
    let missing = call(&queue, Some(&worker), UseCase::GoalShow, json!({"id": 999}));
    assert_eq!(missing["error"]["code"], "failed", "{missing}");
    assert!(!invoke(&queue.db, &["goal", "show", "999"]).status.success());
    // Without a principal nothing is read.
    let anonymous = call(&queue, None, UseCase::Stats, json!({}));
    assert_eq!(anonymous["error"]["code"], "unauthenticated");
}
