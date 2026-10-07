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

/// The service's answer is what the command line prints for `args`, read
/// between two service calls. The fields that tell the time now
/// ([`clock_fields`]) are compared apart: one the clock did not move
/// between the two answers is printed as they serve it, and one it moved
/// that grows with it is printed between the two answers' values, so a
/// loaded host that makes every call cross a second fails nothing. The
/// rest is compared whole; it is read again only when it changed between
/// the two service calls (a period that ended in between).
fn same_as_cli(queue: &Queue, token: &str, use_case: UseCase, params: &Value, args: &[&str]) {
    let clock = clock_fields(use_case);
    for _ in 0..3 {
        let mut before = answer(queue, token, use_case, params);
        let mut printed = ok(&queue.db, args);
        let mut after = answer(queue, token, use_case, params);
        let told = [&mut before, &mut printed, &mut after].map(|value| take(value, &clock));
        if before != after {
            continue;
        }
        assert_eq!(before, printed, "{use_case:?} {params} against {args:?}");
        let [before, printed, after] = told;
        let paths = |told: &Told| told.keys().cloned().collect::<Vec<_>>();
        assert_eq!(paths(&before), paths(&printed), "{use_case:?} {params}");
        assert_eq!(paths(&before), paths(&after), "{use_case:?} {params}");
        for (path, (grows, then)) in &before {
            let (now, later) = (&printed[path].1, &after[path].1);
            let between = if then == later {
                now == then
            } else {
                // A field derived from the clock otherwise is whatever the
                // second the command line read made it.
                !*grows
                    || order(then, now).is_some_and(std::cmp::Ordering::is_le)
                        && order(now, later).is_some_and(std::cmp::Ordering::is_le)
            };
            assert!(
                between,
                "{use_case:?} {params} {path}: printed {now}, served {then} then {later}"
            );
        }
        return;
    }
    panic!("{use_case:?} {params} kept changing beside the clock");
}

/// The fields of `use_case`'s answer that tell the time now, as paths
/// (`*` is any index or key), and whether each only grows with the clock
/// (a time, or the length of what has not ended yet) or is derived from
/// it otherwise (`forecast`'s seed of the unix second).
/// These are the ones [`seeded`]'s queue shows: a lease, a supervisor, an
/// ask or a landed run would add their ages here.
fn clock_fields(use_case: UseCase) -> Vec<(&'static [&'static str], bool)> {
    match use_case {
        UseCase::Stats => vec![
            (&["host", "until"][..], true),
            (&["landing_utilization", "window_secs"], true),
        ],
        // Every period of `kpi`: only the one not over yet moves.
        UseCase::Kpi => vec![(
            &[
                "periods",
                "*",
                "details",
                "landing_utilization",
                "window_secs",
            ][..],
            true,
        )],
        UseCase::Status => vec![(&["checked_at"][..], true)],
        UseCase::Forecast => vec![(&["at"][..], true), (&["seed"], false)],
        // A gap still open ends now.
        UseCase::Timeline => vec![
            (&["gap_total_secs"][..], true),
            (&["gaps", "*", "secs"], true),
        ],
        _ => Vec::new(),
    }
}

/// The fields taken out of an answer, keyed by where each was, with
/// whether it grows with the clock.
type Told = std::collections::BTreeMap<String, (bool, Value)>;

/// Takes out of `value` the fields at `clock`'s paths.
fn take(value: &mut Value, clock: &[(&[&str], bool)]) -> Told {
    fn walk(value: &mut Value, path: &[&str], at: String, grows: bool, out: &mut Told) {
        let [first, rest @ ..] = path else { return };
        let children: Vec<(String, &mut Value)> = match (value, *first) {
            (Value::Object(object), _) if rest.is_empty() => {
                if let Some(taken) = object.remove(*first) {
                    out.insert(format!("{at}/{first}"), (grows, taken));
                }
                return;
            }
            (Value::Object(object), "*") => object
                .iter_mut()
                .map(|(key, child)| (key.clone(), child))
                .collect(),
            (Value::Object(object), key) => object
                .get_mut(key)
                .map(|child| (key.to_owned(), child))
                .into_iter()
                .collect(),
            (Value::Array(items), "*") => items
                .iter_mut()
                .enumerate()
                .map(|(index, child)| (index.to_string(), child))
                .collect(),
            _ => return,
        };
        for (key, child) in children {
            walk(child, rest, format!("{at}/{key}"), grows, out);
        }
    }
    let mut out = Told::new();
    for (path, grows) in clock {
        walk(value, path, String::new(), *grows, &mut out);
    }
    out
}

/// The order of two times or lengths of the same kind: numbers, or the
/// RFC 3339 times of one format, which sort as text.
fn order(first: &Value, second: &Value) -> Option<std::cmp::Ordering> {
    match (first, second) {
        (Value::Number(first), Value::Number(second)) => {
            first.as_f64()?.partial_cmp(&second.as_f64()?)
        }
        (Value::String(first), Value::String(second)) => Some(first.cmp(second)),
        _ => None,
    }
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
    // An observation's whole input, which `observe --input` reads.
    let observation = queue
        .db
        .parent()
        .unwrap()
        .join("observer")
        .join("1791005872");
    std::fs::create_dir_all(&observation).unwrap();
    std::fs::write(
        observation.join("input.json"),
        json!({"stats": {"next_cursor": 4}, "findings": [{"id": 1}, {"id": 2}]}).to_string(),
    )
    .unwrap();
    let reads: Vec<(UseCase, Value, Vec<&str>)> = vec![
        (UseCase::List, json!({}), vec!["list"]),
        (
            UseCase::List,
            json!({"all": true, "full": true, "limit": 5}),
            vec!["list", "--all", "--full", "--limit", "5"],
        ),
        (UseCase::Candidates, Value::Null, vec!["candidates"]),
        (
            UseCase::Candidates,
            json!({"ignore_deferrals": true}),
            vec!["candidates", "--ignore-deferrals"],
        ),
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
        (
            UseCase::ObserveInput,
            json!({"observation": "1791005872"}),
            vec!["observe", "--input", "1791005872"],
        ),
        (
            UseCase::ObserveInput,
            json!({"observation": "1791005872", "section": "findings", "offset": 1, "limit": 1}),
            vec![
                "observe",
                "--input",
                "1791005872",
                "--section",
                "findings",
                "--offset",
                "1",
                "--limit",
                "1",
            ],
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

/// A live supervisor's hold for the CI watch and for a landing branch
/// that does not resolve stand in `candidates`' `held` as the command line
/// prints it, with their records, and leave the order alone.
#[test]
fn the_supervisors_own_holds_are_served_in_held_as_printed() {
    use dagq::domain::{EventKind, LeaseToken};
    use dagq::infrastructure::sqlite::SqliteQueue;
    let (queue, _, _) = seeded();
    let mut sqlite = SqliteQueue::open(&queue.db).unwrap();
    sqlite
        .register_supervisor(&LeaseToken::new("live"), std::process::id(), 2, "0.0.1")
        .unwrap();
    sqlite
        .record_queue_event(
            EventKind::CiWatchHeld,
            json!({"reason": "unreadable", "workflow": "ci.yml", "supervisor": "live"}),
        )
        .unwrap();
    sqlite
        .record_queue_event(
            EventKind::LandingBranchUnresolved,
            json!({"reason": "unresolved", "error": "no landing branch", "supervisor": "live"}),
        )
        .unwrap();
    let plan_review = token(&queue, &Principal::of(&ActorContext::plan_review_job(1, 1)));
    let served = answer(&queue, &plan_review, UseCase::Candidates, &Value::Null);
    let reasons: Vec<&str> = served["held"]
        .as_array()
        .unwrap()
        .iter()
        .map(|held| held["reason"].as_str().unwrap())
        .collect();
    assert_eq!(
        reasons,
        ["ci_watch_held", "landing_branch_unresolved"],
        "{served}"
    );
    assert_eq!(served["held"][0]["record"]["reason"], "unreadable");
    assert_eq!(served["held"][1]["record"]["error"], "no landing branch");
    same_as_cli(
        &queue,
        &plan_review,
        UseCase::Candidates,
        &Value::Null,
        &["candidates"],
    );
}

/// A live supervisor's own holds are served from its latest record however
/// many records of the same kinds another supervisor wrote after it, in
/// `candidates`' `held` and `status`' supervisors as the command line
/// prints them, until it records their end.
#[test]
fn a_supervisor_s_own_holds_are_served_past_another_s_records() {
    use dagq::domain::{EventKind, LeaseToken};
    use dagq::infrastructure::sqlite::SqliteQueue;
    let (queue, _, _) = seeded();
    let mut sqlite = SqliteQueue::open(&queue.db).unwrap();
    for supervisor in ["live", "other"] {
        sqlite
            .register_supervisor(&LeaseToken::new(supervisor), std::process::id(), 2, "0.0.1")
            .unwrap();
    }
    let record = |kind: EventKind, payload: Value| {
        sqlite.record_queue_event(kind, payload).unwrap();
    };
    record(
        EventKind::CiWatchHeld,
        json!({"reason": "unreadable", "workflow": "ci.yml", "supervisor": "live"}),
    );
    record(
        EventKind::LandingBranchUnresolved,
        json!({"reason": "unresolved", "error": "no landing branch", "supervisor": "live"}),
    );
    for _ in 0..70 {
        record(
            EventKind::CiWatchHeld,
            json!({"reason": "pending", "supervisor": "other"}),
        );
        record(
            EventKind::CiWatchResumed,
            json!({"reason": "pending", "supervisor": "other"}),
        );
        record(
            EventKind::LandingBranchUnresolved,
            json!({"reason": "unresolved", "error": "flaky", "supervisor": "other"}),
        );
        record(
            EventKind::LandingBranchResolved,
            json!({"reason": "unresolved", "supervisor": "other"}),
        );
    }
    let plan_review = token(&queue, &Principal::of(&ActorContext::plan_review_job(1, 1)));
    let reasons = |served: &Value| -> Vec<String> {
        served["held"]
            .as_array()
            .unwrap()
            .iter()
            .map(|held| held["reason"].as_str().unwrap().to_owned())
            .collect()
    };
    let holds = |status: &Value, key: &str| -> Vec<Value> {
        status["supervisors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|supervisor| supervisor.get(key).cloned())
            .collect()
    };
    let served = answer(&queue, &plan_review, UseCase::Candidates, &Value::Null);
    assert_eq!(
        reasons(&served),
        ["ci_watch_held", "landing_branch_unresolved"],
        "{served}"
    );
    assert_eq!(served["held"][0]["record"]["supervisor"], "live");
    assert_eq!(served["held"][1]["record"]["supervisor"], "live");
    same_as_cli(
        &queue,
        &plan_review,
        UseCase::Candidates,
        &Value::Null,
        &["candidates"],
    );
    for status in [
        answer(&queue, &plan_review, UseCase::Status, &json!({})),
        ok(&queue.db, &["status"]),
    ] {
        for key in ["ci_watch_hold", "landing_branch_hold"] {
            let held = holds(&status, key);
            assert_eq!(held.len(), 1, "{key}: {status}");
            assert_eq!(held[0]["supervisor"], "live", "{key}: {status}");
        }
    }

    record(
        EventKind::CiWatchResumed,
        json!({"reason": "unreadable", "supervisor": "live"}),
    );
    record(
        EventKind::LandingBranchResolved,
        json!({"reason": "unresolved", "supervisor": "live"}),
    );
    let served = answer(&queue, &plan_review, UseCase::Candidates, &Value::Null);
    assert!(reasons(&served).is_empty(), "{served}");
    same_as_cli(
        &queue,
        &plan_review,
        UseCase::Candidates,
        &Value::Null,
        &["candidates"],
    );
    for status in [
        answer(&queue, &plan_review, UseCase::Status, &json!({})),
        ok(&queue.db, &["status"]),
    ] {
        for key in ["ci_watch_hold", "landing_branch_hold"] {
            assert!(holds(&status, key).is_empty(), "{key}: {status}");
        }
    }
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

/// ADR-t1404-1 decisions 2 and 10, ADR-t1433-1: `stats`' `workspace_mismatch`
/// judges a running run's background wrapper by its handle's pid and the
/// start it recorded alone, without cmux, on the command line (whose
/// `--cmux` names none that runs) and in the service alike. A live wrapper
/// whose heartbeat is far past its timeout is no alert; one whose process
/// is gone is `run_without_wrapper`.
#[test]
fn stats_judges_a_background_wrapper_by_its_pid_and_start_on_both_paths() {
    use dagq::{
        application::ProcessControl,
        domain::{EventKind, background_wrapper::BackgroundHandle},
        infrastructure::{adapters::SystemProcesses, sqlite::SqliteQueue},
    };
    let queue = queue();
    // The wrapper's stand-in is killed when the test ends, however it ends.
    let mut wrapper = crate::common::KillOnDrop::new(
        std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .unwrap(),
        "a background wrapper's stand-in",
    );
    let pid = wrapper.child().id();
    let start_identity = SystemProcesses.start_identity(pid).unwrap();
    let handle = BackgroundHandle::new(pid, &start_identity).to_string();
    let recorded = SqliteQueue::open(&queue.db).unwrap();
    let run = queue.run.id();
    recorded
        .record_runtime_event(
            run,
            EventKind::WorkspaceCreated,
            json!({"workspace_id": handle}),
        )
        .unwrap();
    recorded
        .record_runtime_event(run, EventKind::AgentStarted, json!({}))
        .unwrap();
    let conn = rusqlite::Connection::open(&queue.db).unwrap();
    conn.execute(
        "UPDATE task_runs SET status='running', workspace_id=?2 WHERE id=?1",
        rusqlite::params![run.as_str(), handle],
    )
    .unwrap();
    // The wrapper's row, its heartbeat far older than the timeout.
    conn.execute(
        "INSERT INTO run_processes(run_id, role, pid, heartbeat_at) VALUES (?1, 'wrapper', ?2, unixepoch() - 100000)",
        rusqlite::params![run.as_str(), pid],
    )
    .unwrap();
    start(&queue);
    let worker = token(&queue, &Principal::worker(run, queue.run.task_id()));
    let mismatches = |stats: &Value| -> Vec<Value> {
        stats["running_alerts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|alert| alert["kind"] == "workspace_mismatch")
            .cloned()
            .collect()
    };
    let both = || {
        let cli = ok(&queue.db, &["stats", "--cmux", "/nonexistent/cmux"]);
        let served = answer(&queue, &worker, UseCase::Stats, &json!({}));
        assert_eq!(cli["workspace_check"], served["workspace_check"]);
        (
            mismatches(&cli),
            mismatches(&served),
            cli["workspace_check"].clone(),
        )
    };
    let (cli, served, check) = both();
    assert!(cli.is_empty(), "{cli:?}");
    assert!(served.is_empty(), "{served:?}");
    assert_eq!(
        check,
        json!({"status": "wrappers", "judged": 1, "unjudged": 0})
    );
    wrapper.child().kill().unwrap();
    wrapper.child().wait().unwrap();
    let expected = vec![json!({
        "kind": "workspace_mismatch",
        "task_id": queue.run.task_id().as_i64(),
        "run_id": run.as_str(),
        "reason": "run_without_wrapper",
        "workspace_id": handle,
    })];
    let (cli, served, _) = both();
    assert_eq!(cli, expected);
    assert_eq!(served, expected);
}
