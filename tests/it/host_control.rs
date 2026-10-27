//! The control side's commands a person or the inbox runs on the host
//! through the queue service (goal 182's stage (5), ADR-t3102-3 decisions
//! 3 to 5): `integrate`, `review`, `observe`, `throughput-review` and
//! `setup-review` of the user (the host's `dagq`, `--db`) and of the inbox
//! (a client-mode `dagq`, its socket and token) run their Git and their
//! programs in their own process and send each operation of the queue as a
//! step of their command, print what the command printed on the DB and
//! record its events as the caller (the landing's as the integrator at the
//! caller's request); the service refuses a principal the command is not
//! for at its first step and records the refusal, runs no Git, and a
//! service that does not answer ends each with that error, the DB opened
//! by none.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use crate::common::{
    self, Bounded, WithoutActor,
    service::{Served, serve_logging_git},
};
use crate::runtime_support::{
    Fixture, IntegrateTarget, RunStatus, SqliteQueue, TaskId, TaskReads, TaskRun, TestWorkspace,
    VALID_AGENT, fixture, runtime, supervise,
};
use dagq::{
    application::ProcessControl,
    domain::queue_service::{
        CREDENTIAL_FILE_ENV, ControlCall, ControlCommand, Principal, SOCKET_ENV, SessionProcess,
        UseCase,
    },
    domain::session_step::TargetKind,
    infrastructure::{adapters::SystemProcesses, queue_service as service},
};
use serde_json::{Value, json};

/// A queue whose task 1 has a run awaiting integration, landed by none
/// yet, and its repository.
fn awaiting() -> (Fixture, PathBuf, PathBuf, TaskRun) {
    let (fixture, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let run = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    (fixture, repo, db, run)
}

/// The host's `dagq --db <db> args` of the user, in `cwd`: no service is
/// started for it.
fn as_user(db: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .current_dir(cwd)
        .env("XDG_CONFIG_HOME", db.parent().unwrap().join("config"))
        .arg("--db")
        .arg(db)
        .args(args)
        .bounded_output()
        .unwrap()
}

/// The session of this test's process, as `dagq inbox` binds its token.
fn this_session() -> SessionProcess {
    let pid = std::process::id();
    SessionProcess {
        pid,
        start: SystemProcesses.session_start(pid).unwrap(),
    }
}

/// A client-mode `dagq args` in `cwd` as `principal` claiming `role`: the
/// socket of the service of the queue at `db` and the file of a token
/// issued to it, no queue's path.
fn in_client_mode(
    db: &Path,
    cwd: &Path,
    principal: &Principal,
    role: &str,
    args: &[&str],
) -> Output {
    let dir = db.parent().unwrap();
    let credential = service::issue(dir, principal, 1).unwrap().file;
    Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .current_dir(cwd)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env(SOCKET_ENV, service::socket_path(dir))
        .env(CREDENTIAL_FILE_ENV, credential)
        .env("DAGQ_ROLE", role)
        .args(args)
        .bounded_output()
        .unwrap()
}

/// What a command that must have succeeded printed.
fn printed(output: &Output, args: &[&str]) -> Value {
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// What a command that must have failed printed.
fn refused(output: &Output, args: &[&str]) -> Value {
    assert!(!output.status.success(), "{args:?} succeeded");
    serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|_| panic!("{args:?}: {}", String::from_utf8_lossy(&output.stderr)))
}

/// What a command printed with only what differs from one queue to
/// another put aside: the values of ids, times, process ids and paths, the
/// commit a landing made at its time, and the sizes of prompts that name
/// the paths.
fn normalized(value: &Value) -> Value {
    let volatile = |key: &str| {
        key == "id"
            || key.ends_with("pid")
            || key.ends_with("_bytes")
            || key == "result_commit"
            || key.ends_with("_id")
            || key.ends_with("_at")
            || key.ends_with("_ms")
            || key.ends_with("_secs")
            || key == "since"
            || key == "until"
    };
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let value = if volatile(key) && !value.is_array() {
                        json!("<volatile>")
                    } else {
                        normalized(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(normalized).collect()),
        Value::String(text) if text.contains('/') => json!("<path>"),
        value => value.clone(),
    }
}

/// An event of a landing: its kind, its actor's role and the role of who
/// asked for it (`requested_by`'s actor id before its `:`).
type LandingActor = (String, String, Option<String>);

/// Each event of `run` from its landing's approval on, oldest first, and
/// its payload ([`landing_payload`]: its pids are those of the processes
/// that approved and landed, compared by [`landing_pids`]).
fn landing_events(db: &Path, run: &TaskRun) -> Vec<(LandingActor, Value)> {
    let events = SqliteQueue::open(db).unwrap().run_events(run.id()).unwrap();
    let from = events
        .iter()
        .position(|event| event.kind.as_str() == "integration_approved")
        .expect("no integration_approved");
    events[from..]
        .iter()
        .map(|event| {
            let actor = event.actor.clone().expect("an event without its actor");
            (
                (
                    event.kind.as_str().to_owned(),
                    actor.role,
                    actor
                        .requested_by
                        .map(|by| by.split(':').next().unwrap().to_owned()),
                ),
                landing_payload(&normalized(&event.payload), run.id().as_str()),
            )
        })
        .collect()
}

/// A landing's payload ([`normalized`]) with what differs between two
/// queues' landings of the same task also put aside: the commits the run
/// and the landing made at their own times, the run's id in a commit's
/// message and the host's load averages.
fn landing_payload(value: &Value, run: &str) -> Value {
    let commit = |text: &str| text.len() == 40 && text.bytes().all(|b| b.is_ascii_hexdigit());
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let value = if key.starts_with("load_avg") {
                        json!("<volatile>")
                    } else {
                        landing_payload(value, run)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| landing_payload(item, run))
                .collect(),
        ),
        Value::String(text) if commit(text) => json!("<commit>"),
        Value::String(text) => json!(text.replace(run, "<run>")),
        value => value.clone(),
    }
}

/// The actors of [`landing_events`].
fn landing_actors(db: &Path, run: &TaskRun) -> Vec<LandingActor> {
    landing_events(db, run)
        .into_iter()
        .map(|(actor, _)| actor)
        .collect()
}

/// The pids `run`'s landing recorded: of its approval
/// (`integration_approved`), of its lease and of its start
/// (`integration_started`), read while the lease is held (`None` once it
/// went).
fn landing_pids(db: &Path, run: &TaskRun) -> (Value, Option<u32>, Value) {
    let queue = SqliteQueue::open(db).unwrap();
    let events = queue.run_events(run.id()).unwrap();
    let pid = |kind: &str| {
        events
            .iter()
            .rfind(|event| event.kind.as_str() == kind)
            .map(|event| event.payload["pid"].clone())
            .unwrap_or_else(|| panic!("no {kind}"))
    };
    let leased = rusqlite::Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT pid FROM run_leases WHERE run_id=?1",
            [run.id().as_str()],
            |row| row.get(0),
        )
        .ok();
    (
        pid("integration_approved"),
        leased,
        pid("integration_started"),
    )
}

/// The pid of the queue service of the queue at `db`, as its state names
/// it.
fn service_pid(db: &Path) -> u32 {
    use dagq::domain::queue_service::{SERVICE_DIR, STATE_FILE};
    let state: Value = serde_json::from_slice(
        &fs::read(db.parent().unwrap().join(SERVICE_DIR).join(STATE_FILE)).unwrap(),
    )
    .unwrap();
    u32::try_from(state["pid"].as_i64().unwrap()).unwrap()
}

/// The number of the queue's events.
fn event_count(db: &Path) -> usize {
    SqliteQueue::open(db).unwrap().all_events().unwrap().len()
}

/// Acceptance (1) and (4): a person's `integrate` through the service
/// lands as the command line on the DB lands a queue alike: the same
/// outcome, the same events with the same actors (the approval the
/// user's, the landing's the integrator's at the user's request); the
/// service runs no Git, and the integrator's token goes with the landing.
#[test]
fn a_persons_integrate_lands_through_the_service_as_on_the_db_and_the_service_runs_no_git() {
    let (_direct, direct_repo, direct_db, direct_run) = awaiting();
    let on_db = runtime::integrate(
        &direct_db,
        IntegrateTarget::Task(TaskId::new(1)),
        &direct_repo,
        None,
    )
    .unwrap();
    assert_eq!(on_db["outcome"], "integrated", "{on_db}");

    let (_served, repo, db, run) = awaiting();
    let git_log = serve_logging_git(&db, &[]);
    let args = [
        "integrate",
        "1",
        "--no-push",
        "--repo",
        repo.to_str().unwrap(),
    ];
    let through = printed(&as_user(&db, &repo, &args), &args);
    assert_eq!(through["outcome"], "integrated", "{through}");
    assert_eq!(normalized(&through), normalized(&on_db));
    assert_eq!(
        landing_events(&db, &run),
        landing_events(&direct_db, &direct_run)
    );
    // The command's process, not the service's, approved and landed it,
    // as on the DB the command's own process does.
    for (db, run) in [(&db, &run), (&direct_db, &direct_run)] {
        let (approved, _, started) = landing_pids(db, run);
        assert_eq!(started, approved, "{}", db.display());
    }
    let (_, _, started) = landing_pids(&db, &run);
    assert_ne!(started, json!(service_pid(&db)));
    assert_ne!(started, json!(std::process::id()));
    let landed = landing_actors(&db, &run);
    assert_eq!(
        landed[0],
        ("integration_approved".into(), "user".into(), None)
    );
    assert!(
        landed.contains(&(
            "run_integrated".into(),
            "integrator".into(),
            Some("user".into())
        )),
        "{landed:?}"
    );
    let queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Integrated);
    // The integrator's token went as the landing left `integrating`.
    assert!(
        service::principals(db.parent().unwrap())
            .unwrap()
            .iter()
            .all(|principal| principal.landing_run().is_none()),
    );
    assert_eq!(fs::read_to_string(&git_log).unwrap_or_default(), "");
}

/// Acceptance (1) and (3) of the inbox: its client-mode `dagq` (the
/// service's socket and its token, no queue's path) reviews, observes,
/// reviews the throughput and lands through the service, each printing
/// what the command line prints on the DB, the landing recorded as the
/// inbox's at its request.
#[test]
fn the_inbox_reviews_observes_and_lands_in_client_mode() {
    let (_fixture, repo, db, run) = awaiting();
    let _service = Served::start(&db);
    let inbox = Principal::inbox(this_session());
    let review = ["review", "1"];
    let reviewed = printed(
        &in_client_mode(&db, &repo, &inbox, "inbox", &review),
        &review,
    );
    assert_eq!(reviewed, runtime::review(&db, TaskId::new(1)).unwrap());
    for args in [
        &["observe", "--dry-run"][..],
        &["throughput-review", "--dry-run"],
    ] {
        let looked = printed(&in_client_mode(&db, &repo, &inbox, "inbox", args), args);
        assert_eq!(looked["dry_run"], true, "{args:?}: {looked}");
    }
    let before = event_count(&db);
    let args = ["integrate", "1", "--no-push"];
    let landed = printed(&in_client_mode(&db, &repo, &inbox, "inbox", &args), &args);
    assert_eq!(landed["outcome"], "integrated", "{landed}");
    assert!(event_count(&db) > before);
    let events = landing_actors(&db, &run);
    assert_eq!(
        events[0],
        ("integration_approved".into(), "inbox".into(), None)
    );
    assert!(
        events.contains(&(
            "run_integrated".into(),
            "integrator".into(),
            Some("inbox".into())
        )),
        "{events:?}"
    );
}

/// Acceptance (1)'s refusals: the service refuses a worker's client-mode
/// `dagq` each command at its first step, as the command line refuses it,
/// records each refusal, and nothing else is done.
#[test]
fn each_command_is_refused_a_worker_and_the_refusal_is_recorded() {
    let queue = common::service::queue();
    common::service::start(&queue);
    let worker = Principal::worker(queue.run.id(), queue.run.task_id());
    let task = queue.run.task_id().to_string();
    let cwd = queue.dir().to_owned();
    for (args, capability) in [
        (vec!["integrate", task.as_str()], "landing.request"),
        (vec!["review", task.as_str()], "review.prepare"),
        (vec!["observe", "--dry-run"], "observe.run"),
        (vec!["throughput-review", "--dry-run"], "observe.run"),
        (vec!["setup-review"], "review.setup"),
    ] {
        let before = event_count(&queue.db);
        let output = in_client_mode(&queue.db, &cwd, &worker, "worker", &args);
        let error = refused(&output, &args);
        assert_eq!(
            error["queue_service"]["code"], "authorization_denied",
            "{args:?}: {error}"
        );
        assert_eq!(
            error["denied"]["capability"], capability,
            "{args:?}: {error}"
        );
        let events = SqliteQueue::open(&queue.db).unwrap().all_events().unwrap();
        assert_eq!(events.len(), before + 1, "{args:?}");
        let denied = events.last().unwrap();
        assert_eq!(denied.kind.as_str(), "authorization_denied", "{args:?}");
        assert_eq!(denied.payload["capability"], capability, "{args:?}");
    }
    assert_eq!(
        SqliteQueue::open(&queue.db)
            .unwrap()
            .run(queue.run.id())
            .unwrap()
            .status(),
        queue.run.status()
    );
}

/// Acceptance (2): with no service, each command of the user's and of the
/// inbox's says the service does not answer and what to do next, and opens
/// no DB: the queue's files are as they were.
#[test]
fn with_no_service_each_command_says_so_and_opens_no_queue() {
    let queue = common::service::queue();
    let task = queue.run.task_id().to_string();
    let cwd = queue.dir().to_owned();
    let files = |dir: &Path| {
        let mut files: Vec<(String, Vec<u8>)> = fs::read_dir(dir)
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().unwrap().is_file())
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect();
        files.sort();
        files
    };
    let before = files(queue.dir());
    let inbox = Principal::inbox(this_session());
    for args in [
        vec!["integrate", task.as_str(), "--no-push"],
        vec!["review", task.as_str()],
        vec!["observe", "--dry-run"],
        vec!["throughput-review", "--dry-run"],
        vec!["setup-review"],
    ] {
        // The host's `dagq` says how to start the service, a client-mode
        // one where to look from the control side.
        for (output, next) in [
            (as_user(&queue.db, &cwd, &args), "dagq up"),
            (
                in_client_mode(&queue.db, &cwd, &inbox, "inbox", &args),
                "dagq service status",
            ),
        ] {
            let error = refused(&output, &args);
            assert_eq!(
                error["queue_service"]["code"], "unreachable",
                "{args:?}: {error}"
            );
            let message = error["error"].as_str().unwrap();
            assert!(message.contains(next), "{args:?}: {message}");
            assert!(
                message.contains("does not open the queue"),
                "{args:?}: {message}"
            );
        }
    }
    // The tokens issued are the service's files, beside the DB's.
    assert_eq!(files(queue.dir()), before);
}

/// A fake `claude` in `dir` that reads its prompt and answers as a
/// headless Claude job does.
fn stub_claude(dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("claude-stub");
    fs::write(
        &path,
        "#!/bin/sh\ncat >/dev/null\necho '{\"type\":\"result\",\"result\":\"done\"}'\n",
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path.to_str().unwrap().to_owned()
}

/// An observation of the queue at `db` on the DB, as the user's `dagq`
/// ran it before the service (the library's, its records the user's),
/// its agent `claude`: what the command line passes by default.
fn observe_on_db(db: &Path, claude: &str) -> anyhow::Result<Value> {
    let provider = dagq::infrastructure::adapters::ClaudeCode {
        executable: claude.into(),
    };
    dagq::compose::observe(
        db,
        &provider,
        Some(&provider),
        &dagq::application::observer::ObserveOptions {
            mode: dagq::application::observer::ObserveMode::Hourly,
            since: None,
            dry_run: false,
            timeout: std::time::Duration::from_secs(1800),
            dagq: PathBuf::from(env!("CARGO_BIN_EXE_dagq")),
            user_config: None,
            prompt_limit: dagq::application::observer::PROMPT_LIMIT,
            launch: None,
            switchable: false,
            fallback: true,
            unavailable: None,
        },
    )
}

/// The actor's role of each queue event of `kind`, oldest first.
fn queue_event_roles(db: &Path, kind: &str) -> Vec<String> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| event.actor.expect("an event without its actor").role)
        .collect()
}

/// Acceptance (1)'s records of the jobs: an observation and a throughput
/// review the user and the inbox start by hand through the service print
/// what the command prints on the DB and record their start and their end
/// as the caller who started them, as on the DB the user's are.
#[test]
fn an_observation_and_a_throughput_review_record_as_their_caller() {
    use dagq::infrastructure::adapters::ClaudeCode;
    // Each caller's on a queue of its own: a second observation of the
    // same queue with nothing new since is not started.
    let (on_db, by_user, by_inbox) = (
        common::service::queue(),
        common::service::queue(),
        common::service::queue(),
    );
    for queue in [&on_db, &by_user, &by_inbox] {
        common::service::start(queue);
    }
    let inbox = Principal::inbox(this_session());
    for (job, started, finished) in [
        ("observe", "observe_started", "observe_finished"),
        (
            "throughput-review",
            "throughput_review_started",
            "throughput_review_finished",
        ),
    ] {
        // The command on the DB, as the user's `dagq` ran it before the
        // service: the library's, its records the user's.
        let claude = stub_claude(on_db.dir());
        let provider = ClaudeCode {
            executable: claude.clone().into(),
        };
        let direct = if job == "observe" {
            observe_on_db(&on_db.db, &claude)
        } else {
            dagq::compose::throughput_review(
                &on_db.db,
                &provider,
                &dagq::application::throughput_review::ReviewOptions {
                    mode: "hourly".parse().unwrap(),
                    at: None,
                    dry_run: false,
                    timeout: std::time::Duration::from_secs(1800),
                    dagq: PathBuf::from(env!("CARGO_BIN_EXE_dagq")),
                    user_config: None,
                    utc_offset: None,
                    launch: None,
                    switchable: false,
                    fallback: true,
                    unavailable: None,
                },
            )
        }
        .unwrap();
        let direct_roles: Vec<Vec<String>> = [started, finished]
            .iter()
            .map(|kind| queue_event_roles(&on_db.db, kind))
            .collect();
        assert_eq!(direct_roles, [["user"], ["user"]], "{job} on the DB");
        for (queue, role) in [(&by_user, "user"), (&by_inbox, "inbox")] {
            let claude = stub_claude(queue.dir());
            let args = [job, "--claude", claude.as_str()];
            let cwd = queue.dir().to_owned();
            let output = if role == "user" {
                as_user(&queue.db, &cwd, &args)
            } else {
                in_client_mode(&queue.db, &cwd, &inbox, role, &args)
            };
            let through = printed(&output, &args);
            assert_eq!(normalized(&through), normalized(&direct), "{args:?}");
            for kind in [started, finished] {
                assert_eq!(
                    queue_event_roles(&queue.db, kind),
                    [role],
                    "{args:?} {kind}"
                );
            }
            // The job's telemetry token, issued and revoked by the service
            // at the caller's step for the caller's own job, as on the DB.
            let target = if job == "observe" {
                TargetKind::Observer
            } else {
                TargetKind::ThroughputReview
            };
            let tokens = |db: &Path| -> Vec<String> {
                common::service::target_token_events(db, target)
                    .iter()
                    .map(|(_, recorded, _)| (*recorded).to_owned())
                    .collect()
            };
            assert_eq!(tokens(&queue.db), ["issued", "revoked"], "{args:?}");
            assert_eq!(tokens(&queue.db), tokens(&on_db.db), "{args:?}");
        }
    }
}

/// Acceptance (1) of an observation that stops at a wall: the user's and
/// the inbox's `observe` through the service open the queue's
/// authentication ask as the observer job and record `auth_required` as
/// the command on the DB does, as their caller, and print the ask.
#[test]
fn an_observation_at_a_login_that_ran_out_holds_as_on_the_db() {
    use crate::runtime_support::{AskKind, AskQuery};
    use std::os::unix::fs::PermissionsExt;
    let logged_out = |dir: &Path| {
        let path = dir.join("claude-logged-out");
        fs::write(
            &path,
            "#!/bin/sh\ncat >/dev/null\nprintf 'Invalid API key \\302\\267 Please run /login\\n'\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path.to_str().unwrap().to_owned()
    };
    let (on_db, by_user, by_inbox) = (
        common::service::queue(),
        common::service::queue(),
        common::service::queue(),
    );
    for queue in [&on_db, &by_user, &by_inbox] {
        common::service::start(queue);
    }
    let hold = |db: &Path, printed: &Value| {
        let asks = SqliteQueue::open(db)
            .unwrap()
            .asks(AskQuery {
                open: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(asks.len(), 1, "{asks:?}");
        assert_eq!(asks[0].kind, AskKind::QueueHold);
        assert_eq!(
            asks[0].reason_category,
            dagq::domain::AskReason::Authentication
        );
        assert_eq!(asks[0].affected, ["observer job"]);
        assert_eq!(printed["hold_ask_id"], json!(asks[0].id), "{printed}");
        assert_eq!(printed["wall"], "authentication", "{printed}");
    };
    let direct = observe_on_db(&on_db.db, &logged_out(on_db.dir())).unwrap();
    hold(&on_db.db, &direct);
    assert_eq!(queue_event_roles(&on_db.db, "auth_required"), ["user"]);
    let inbox = Principal::inbox(this_session());
    for (queue, role) in [(&by_user, "user"), (&by_inbox, "inbox")] {
        let claude = logged_out(queue.dir());
        let args = ["observe", "--claude", claude.as_str()];
        let cwd = queue.dir().to_owned();
        let output = if role == "user" {
            as_user(&queue.db, &cwd, &args)
        } else {
            in_client_mode(&queue.db, &cwd, &inbox, role, &args)
        };
        let through = printed(&output, &args);
        assert_eq!(normalized(&through), normalized(&direct), "{args:?}");
        hold(&queue.db, &through);
        assert_eq!(
            queue_event_roles(&queue.db, "auth_required"),
            [role],
            "{args:?}"
        );
    }
}

/// Where [`setup_review_on_db_child_process`] reads what to do: the
/// queue's DB, the checkout and the agent, one a line.
const SETUP_REVIEW_ON_DB_FOR: &str = "DAGQ_TEST_SETUP_REVIEW_ON_DB_FOR";

/// `setup-review` on the DB of the queue at `db` in `repo` with the agent
/// `claude`, as the user's `dagq` ran it before the service: in another
/// process of this test binary ([`setup_review_on_db_child_process`]),
/// since the command sets its process's signal handlers while it runs.
fn setup_review_on_db(db: &Path, repo: &Path, claude: &str) -> Value {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "host_control::setup_review_on_db_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(
            SETUP_REVIEW_ON_DB_FOR,
            format!("{}\n{}\n{claude}", db.display(), repo.display()),
        )
        .bounded_output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find_map(|line| {
            line.split_once("setup-review-on-db: ")
                .map(|(_, on_db)| on_db)
        })
        .unwrap_or_else(|| panic!("{stdout}{}", String::from_utf8_lossy(&output.stderr)));
    serde_json::from_str(line).unwrap()
}

/// Not a test of its own: the other process of [`setup_review_on_db`],
/// which runs `setup-review` on the DB and prints what it printed when it
/// is asked to, and does nothing otherwise.
#[test]
fn setup_review_on_db_child_process() {
    let Ok(asked) = std::env::var(SETUP_REVIEW_ON_DB_FOR) else {
        return;
    };
    let [db, repo, claude] = <[&str; 3]>::try_from(asked.lines().collect::<Vec<_>>()).unwrap();
    let on_db = dagq::compose::setup_review(
        Path::new(db),
        Path::new(repo),
        &dagq::compose::SetupReviewOptions {
            profiles: &dagq::domain::toolchain::BUILTIN,
            claude: claude.into(),
            codex: "codex".into(),
            codex_home: None,
            timeout: std::time::Duration::from_secs(1800),
            approval: dagq::application::setup_review::Approval::Ask,
            dagq: PathBuf::from(env!("CARGO_BIN_EXE_dagq")),
        },
    )
    .unwrap();
    println!(
        "setup-review-on-db: {}",
        serde_json::to_string(&on_db).unwrap()
    );
}

/// Acceptance (1) of `setup-review`: the user's host `dagq` and the
/// inbox's client-mode `dagq` show through the service the settings their
/// job generated as the command on the DB shows them, and none changes
/// the queue.
#[test]
fn the_inbox_sets_up_the_review_in_client_mode_as_the_user_does() {
    let fixture = common::setup_review::Fixture::new();
    let _service = Served::start(&fixture.db);
    let agent = fixture.agent(
        "claude-generate",
        "printf '\\n[review.subagents.\"builtin:tests\"]\\nenabled = true\\n' >> dagq.toml",
    );
    let events = fixture.events();
    let args = ["setup-review", "--claude", agent.as_str()];
    let on_db = setup_review_on_db(&fixture.db, &fixture.repo, &agent);
    assert_eq!(fixture.events(), events);
    let by_user = fixture.ok(&[], &args);
    assert_eq!(normalized(&by_user), normalized(&on_db), "{by_user}");
    let inbox = Principal::inbox(this_session());
    let by_inbox = printed(
        &in_client_mode(&fixture.db, &fixture.repo, &inbox, "inbox", &args),
        &args,
    );
    assert_eq!(by_inbox["approved"], false, "{by_inbox}");
    assert_eq!(by_inbox["changes"], by_user["changes"], "{by_inbox}");
    assert_eq!(by_inbox["diff"], by_user["diff"], "{by_inbox}");
    assert_eq!(normalized(&by_inbox), normalized(&by_user));
    assert_eq!(fixture.events(), events);
    assert!(fixture.places_gone());
}

/// A person's landing through the service whose command died is the
/// supervisor's to release, as a dead supervisor's is: its lease and its
/// `integration_started` name the command's process, not the service's
/// (which still runs), so once that process is gone and the lease's
/// heartbeat is old, the next pass gives the slot back and lands the
/// approved run again.
#[test]
fn a_landing_whose_command_died_is_released_by_the_next_pass() {
    let (_fixture, repo, db, run) = awaiting();
    // The service lives on: the landing is released for its command's
    // death, not the service's.
    let _service = Served::start(&db);
    let pid = crate::runtime_support::dead_pid();
    let step = |op: &str, args: Value| {
        let response = integrate_step(&db, 1, pid, op, args);
        assert_eq!(response["ok"], true, "{op}: {response}");
    };
    // The steps of `integrate` the command sent before it died: its
    // approval and its slot.
    step(
        "approve_integration",
        json!([run.id(), {"status": "awaiting_integration", "pid": pid, "push": false}]),
    );
    step(
        "begin_landing",
        json!([run.id(), "landing-of-a-dead-command", main_of(&repo), false]),
    );
    assert_eq!(landing_pids(&db, &run), (json!(pid), Some(pid), json!(pid)));
    // Its heartbeat stopped with it.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("UPDATE run_leases SET heartbeat_at=0", [])
        .unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let queue = SqliteQueue::open(&db).unwrap();
    let events = queue.run_events(run.id()).unwrap();
    let released: Vec<&Value> = events
        .iter()
        .filter(|event| {
            event.kind.as_str() == "auto_repaired" && event.payload["repair"] == "landing_released"
        })
        .map(|event| &event.payload)
        .collect();
    assert_eq!(released.len(), 1, "{events:?}");
    assert_eq!(released[0]["detail"]["then"], "land", "{released:?}");
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Integrated);
}

/// The step `op` with `args` of the user's `dagq integrate <task>` of
/// process `pid`, sent to the service of the queue at `db`: its answer.
fn integrate_step(db: &Path, task: i64, pid: u32, op: &str, args: Value) -> Value {
    let control = ControlCall {
        command: ControlCommand::Integrate,
        task: Some(TaskId::new(task)),
        yes: false,
        pid,
    };
    let response = service::call(
        &service::socket_path(db.parent().unwrap()),
        &dagq::domain::queue_service::ServiceRequest {
            api_version: dagq::domain::queue_service::API_VERSION,
            token: None,
            env_role: None,
            queue: None,
            binding: None,
            use_case: UseCase::Control,
            params: json!({"control": control, "call": {"op": op, "args": args}}),
        },
        std::time::Duration::from_secs(30),
    )
    .unwrap();
    serde_json::to_value(response).unwrap()
}

/// The commit `main` of `repo` is at.
fn main_of(repo: &Path) -> String {
    let main = Command::new("git")
        .args(["rev-parse", "main"])
        .current_dir(repo)
        .output()
        .unwrap();
    String::from_utf8(main.stdout).unwrap().trim().to_owned()
}

/// A landing's steps are only of a run of the task its command names:
/// `integrate 2`'s approval, slot and token's end on task 1's run are
/// refused, and the run's status, its lease, the queue's events and the
/// landing's integrator token stay as they were.
#[test]
fn a_landing_of_another_tasks_run_is_neither_begun_nor_ended() {
    let (_fixture, repo, db, run) = awaiting();
    let _service = Served::start(&db);
    let pid = std::process::id();
    let main = main_of(&repo);
    let landings = || {
        service::principals(db.parent().unwrap())
            .unwrap()
            .iter()
            .filter(|principal| principal.landing_run().as_ref() == Some(run.id()))
            .count()
    };
    let leases = || -> i64 {
        rusqlite::Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM run_leases WHERE run_id = ?1",
                [run.id().as_str()],
                |row| row.get(0),
            )
            .unwrap()
    };
    let status = || {
        SqliteQueue::open(&db)
            .unwrap()
            .run(run.id())
            .unwrap()
            .status()
    };
    let approval = json!([run.id(), {"status": "awaiting_integration", "pid": pid, "push": false}]);
    let refused = |op: &str, args: Value| {
        let answer = integrate_step(&db, 2, pid, op, args);
        assert_eq!(answer["ok"], false, "{op}: {answer}");
        assert_eq!(answer["error"]["code"], "bad_request", "{op}: {answer}");
    };
    let before = event_count(&db);
    refused("approve_integration", approval.clone());
    refused(
        "begin_landing",
        json!([run.id(), "landing-of-another-task", main, false]),
    );
    refused("end_landing", json!([run.id()]));
    assert_eq!(event_count(&db), before);
    assert_eq!(status(), RunStatus::AwaitingIntegration);
    assert_eq!((leases(), landings()), (0, 0));

    // Its own task's landing begins, and its integrator defers it to a
    // session: the run left `integrating` with the landing's token, which
    // only its own task's end revokes.
    let ok = |op: &str, args: Value| {
        let answer = integrate_step(&db, 1, pid, op, args);
        assert_eq!(answer["ok"], true, "{op}: {answer}");
        answer["result"].clone()
    };
    ok("approve_integration", approval);
    let begun = ok(
        "begin_landing",
        json!([run.id(), "landing-of-its-task", main, false]),
    );
    let token = service::read_token(Path::new(begun["integrator"].as_str().unwrap())).unwrap();
    let deferred = service::call(
        &service::socket_path(db.parent().unwrap()),
        &dagq::domain::queue_service::ServiceRequest {
            api_version: dagq::domain::queue_service::API_VERSION,
            token: Some(token),
            env_role: None,
            queue: None,
            binding: None,
            use_case: UseCase::Supervisor,
            params: json!({"op": "defer_integration", "args": [run.id(), "landing-of-its-task", "a test", {}]}),
        },
        std::time::Duration::from_secs(30),
    )
    .unwrap();
    assert!(deferred.ok, "{deferred:?}");
    assert_eq!(status(), RunStatus::NeedsSession);
    assert_eq!(landings(), 1);
    let before = event_count(&db);
    refused("end_landing", json!([run.id()]));
    assert_eq!((landings(), event_count(&db)), (1, before));
    ok("end_landing", json!([run.id()]));
    assert_eq!(landings(), 0);
}

/// The approval to land is refused, on the DB and through the service
/// alike, to a run a live lease holds (the supervisor's review or resume),
/// its prerequisites read in the approval's own write: no
/// `integration_approved` is recorded and the run still awaits
/// integration.
#[test]
fn a_held_run_is_approved_by_no_integrate_and_records_no_approval() {
    let (_fixture, repo, db, run) = awaiting();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .cast_signed();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO run_leases (run_id, token, pid, heartbeat_at) VALUES (?1, 'held', ?2, ?3)",
            rusqlite::params![run.id().as_str(), std::process::id(), now],
        )
        .unwrap();
    let approvals = || {
        SqliteQueue::open(&db)
            .unwrap()
            .run_events(run.id())
            .unwrap()
            .iter()
            .filter(|event| event.kind.as_str() == "integration_approved")
            .count()
    };
    let error =
        runtime::integrate(&db, IntegrateTarget::Task(TaskId::new(1)), &repo, None).unwrap_err();
    assert!(
        format!("{error:#}").contains("is held by the supervisor"),
        "{error:#}"
    );
    let _service = Served::start(&db);
    let inbox = Principal::inbox(this_session());
    let args = ["integrate", "1", "--no-push"];
    let error = refused(&in_client_mode(&db, &repo, &inbox, "inbox", &args), &args);
    assert!(
        error.to_string().contains("is held by the supervisor"),
        "{error}"
    );
    assert_eq!(approvals(), 0);
    let queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
}

/// The service answers a control-side command's step only within its
/// command: no requester but the caller, no record on a run its landing
/// does not land, and no telemetry of a job that is not the caller's own;
/// each refused, nothing written.
#[test]
fn a_step_outside_its_command_is_refused_and_writes_nothing() {
    let queue = common::service::queue();
    common::service::start(&queue);
    let step = |command, task: Option<i64>, op: &str, args: Value, requested_by: Option<&str>| {
        let control = ControlCall {
            command,
            task: task.map(TaskId::new),
            yes: false,
            pid: std::process::id(),
        };
        let mut call = json!({"op": op, "args": args});
        if let Some(by) = requested_by {
            call["requested_by"] = json!(by);
        }
        common::service::call(
            &queue,
            None,
            UseCase::Control,
            json!({"control": control, "call": call}),
        )
    };
    let run = queue.run.id().as_str().to_owned();
    let before = event_count(&queue.db);
    for (answer, what) in [
        (
            step(
                ControlCommand::Observe,
                None,
                "record_queue_event",
                json!(["observe_started", {}]),
                Some("inbox:1"),
            ),
            "another's request",
        ),
        (
            step(
                ControlCommand::Integrate,
                Some(1),
                "record_runtime_event",
                json!([run, "integration_approved", {}]),
                None,
            ),
            "a run in progress",
        ),
        (
            step(
                ControlCommand::Integrate,
                Some(9),
                "record_runtime_event",
                json!([run, "run_phase_changed", {}]),
                None,
            ),
            "another task's run",
        ),
        (
            step(
                ControlCommand::Observe,
                None,
                "issue_telemetry",
                json!([{"kind": "planner", "id": "1"}, null, null]),
                None,
            ),
            "a planner's telemetry",
        ),
        (
            step(
                ControlCommand::Observe,
                None,
                "register_telemetry_session",
                json!([{"kind": "observer", "id": "1"}, "s"]),
                None,
            ),
            "an observation it did not start",
        ),
    ] {
        assert_eq!(answer["ok"], false, "{what}: {answer}");
        assert_eq!(answer["error"]["code"], "bad_request", "{what}: {answer}");
    }
    assert_eq!(event_count(&queue.db), before);
    // A landing's container verification reads and records its image on
    // the service's queue as the caller.
    let image = step(
        ControlCommand::Integrate,
        None,
        "record_queue_event",
        json!([
            "container_image_failed",
            dagq::domain::container_image::failure_payload(None, "no podman", 1)
        ]),
        None,
    );
    assert_eq!(image["ok"], true, "{image}");
    assert_eq!(
        queue_event_roles(&queue.db, "container_image_failed"),
        ["user"]
    );
    let latest = step(
        ControlCommand::Integrate,
        None,
        "latest_queue_event",
        json!([dagq::domain::container_image::CONTAINER_IMAGE_KINDS]),
        None,
    );
    assert_eq!(latest["ok"], true, "{latest}");
    let before = event_count(&queue.db);
    // The machine's users' runs a landing's container verification reads
    // on the queue it lands on.
    for (op, args) in [
        ("latest_runs_in_progress", json!([])),
        ("run", json!([run])),
    ] {
        let read = step(ControlCommand::Integrate, None, op, args, None);
        assert_eq!(read["ok"], true, "{op}: {read}");
    }
    // The approval's own step reads the run in its write: one in progress
    // is not approved.
    let approved = step(
        ControlCommand::Integrate,
        queue.run.task_id().to_string().parse().ok(),
        "approve_integration",
        json!([run, {"status": queue.run.status().as_str(), "pid": 1, "push": false}]),
        None,
    );
    assert_eq!(approved["ok"], false, "{approved}");
    assert_eq!(event_count(&queue.db), before);
}
