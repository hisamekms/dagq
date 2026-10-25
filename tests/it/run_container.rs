//! A worker's turns in their container (docs/design/container-runtime.md,
//! ADR-t2113-3 decision 1): the session wrapper of a run in a clone starts
//! each turn through `podman --connection dagq run` with the hardening and
//! the run's mounts alone, and the container runs `dagq relay turn` of the
//! client, which fetches main's bundle into the clone, gives the agent the
//! run token from its file and the forwarder's destinations, and writes
//! the run branch's bundle and the run's state at the turn's end. The
//! podman here is a fake that records its arguments and runs the
//! container's command on the host (the test's `dagq` and a stub `claude`
//! in the clone), with real git; the real machine is not used. Whatever
//! stops a run removes its container, and the run's end removes its build
//! output.
use crate::runtime_support;

use dagq::application::broker_relay::RelayControl;
use dagq::application::run_container::RunContainers;
use dagq::domain::background_wrapper::BackgroundHandle;
use dagq::domain::podman_machine::QueueKey;
use dagq::domain::run_container::{ContainerLimits, TOKEN_FILE};
use dagq::domain::{
    ClaimOutcome, EventKind, LeaseToken, RunPlan, provider_switch::WorkerRoute, run_clone,
    worker::Worker,
};
use dagq::infrastructure::broker_relay::PodmanRelays;
use dagq::infrastructure::location::runs_dir;
use dagq::infrastructure::podman_machine::PodmanCli;
use runtime_support::containers::*;
use runtime_support::headless::Stand;
use runtime_support::*;

/// A run of task 1 claimed for a worker set to the container, its clone
/// made by the control side and its prompt written, as the supervisor
/// provisions one; its lease's token.
fn claim_in_container(db: &Path, repo: &Path, lease: &str) -> (TaskRun, LeaseToken) {
    let mut queue = SqliteQueue::open(db).unwrap();
    let common_dir = GitRepository::inspect(repo).unwrap().common_dir;
    queue.bind_repository(common_dir.to_str().unwrap()).unwrap();
    let base = git_out(repo, &["rev-parse", "main"]);
    let token = LeaseToken::new(lease);
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor_in_order(
            &sha(&base),
            &token,
            &[TaskId::new(1)],
            None,
            &Default::default(),
            &WorkerRoute::direct(&[Worker::CLAUDE_HEADLESS]),
        )
        .unwrap()
    else {
        panic!("task 1 was not claimed")
    };
    let runs = runs_dir(db);
    let run_dir = runs.join(run.id().as_str());
    let clone = run_clone::clone_path(&runs, run.id());
    fs::create_dir_all(&run_dir).unwrap();
    let text = |path: &Path| path.to_str().unwrap().to_owned();
    queue
        .plan_run(
            run.id(),
            &token,
            &RunPlan {
                repo_path: text(repo),
                run_dir: text(&run_dir),
                branch: format!("dagq/{}", run.id()),
                worktree_path: text(&clone),
                receipt_path: text(&run_dir.join("receipt.json")),
                log_path: text(&run_dir.join("log")),
            },
        )
        .unwrap();
    fs::write(run_dir.join("prompt.txt"), "do the task").unwrap();
    let run = queue.run(run.id()).unwrap();
    GitRepository::inspect(repo)
        .unwrap()
        .create_clone(&run, None)
        .unwrap();
    (run, token)
}

/// Acceptance (1) to (3): a run in a clone whose worker `dagq.toml` sets to
/// the container takes its turn in its container. The container is
/// hardened and mounts only the run's clone, directory, socket and build
/// output; its arguments hold no token. Inside, main's bundle is fetched
/// into the clone's main ref before the agent runs, the agent is given the
/// run token from its file and every destination on the forwarder, and the
/// turn's end leaves the run branch's bundle and the run's state, which
/// the control side reads as it reads any container run's. The turn's
/// output, receipt and records are read as for a turn on the host, and the
/// container is removed at the turn's end. A later request (a revise, an
/// answer) resumes the session in a new container of its own.
#[test]
fn a_worker_s_turn_runs_in_its_hardened_container_and_leaves_its_bundle_and_state() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (repo, db) = queue_fixture(dir.path());
    fs::write(
        repo.join("dagq.toml"),
        "[roles.worker]\nruntime = \"container\"\n[container.build_output]\ndir = \"/build\"\n\
         [container.worker_cache]\ndirs = [\"/cache/registry\", \"/cache/compiler\"]\n\
         [run.env]\nCARGO_TARGET_DIR = '/build/target'\nRUSTC_WRAPPER = 'sccache'\n",
    )
    .unwrap();
    let (run, token) = claim_in_container(&db, &repo, "container-run");
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let clone = PathBuf::from(run.worktree_path().unwrap());
    let base = run.base_commit().as_str().to_owned();
    // Main moves on, and the control side hands it over as a bundle.
    fs::write(repo.join("main.txt"), "main\n").unwrap();
    git(&repo, &["add", "main.txt"]);
    git(&repo, &["commit", "-qm", "main moves"]);
    let main = git_out(&repo, &["rev-parse", "main"]);
    git(
        &repo,
        &[
            "bundle",
            "create",
            "-q",
            run_dir.join(run_clone::MAIN_BUNDLE_FILE).to_str().unwrap(),
            "refs/heads/main",
            &format!("^{base}"),
        ],
    );
    // The supervisor's start of the background wrapper, which registers
    // once it finds it.
    let stand = Stand::start().unwrap();
    let start = BackgroundHandle::parse(&stand.handle).unwrap().start;
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        queue
            .workspace_created(run.id(), &token, &stand.handle)
            .unwrap();
        queue
            .record_runtime_event(
                run.id(),
                EventKind::WrapperLaunched,
                json!({"pid": stand.pid, "start": start, "workspace_id": stand.handle,
                       "log": run_dir.join("session.log").to_string_lossy()}),
            )
            .unwrap();
    }
    fs::write(run_dir.join("leave-notes"), "").unwrap();
    let state = dir.path().join("podman");
    let containers = containers(&state, &db, &repo);
    let provider = dagq::infrastructure::adapters::ClaudeCode {
        executable: PathBuf::from("/nonexistent/claude-on-the-host"),
    };
    // The task's turn runs; once it finished, the session is asked to
    // exit, as the supervisor asks it after the receipt.
    let ended = thread::scope(|scope| {
        let wrapper = scope.spawn(|| {
            runtime::session_in_background_with_containers(
                &db,
                run.id(),
                &token,
                &provider,
                &process::LocalSpawner,
                false,
                stand.pid,
                &dagq::infrastructure::adapters::SystemProcesses,
                &containers,
            )
        });
        let turns_finished = |count: usize| {
            wait_until(&db, crate::common::STEP_LIMIT, |queue| {
                queue
                    .show(TaskId::new(1))
                    .unwrap()
                    .events
                    .iter()
                    .filter(|event| event.kind == EventKind::TurnFinished)
                    .count()
                    >= count
            });
        };
        turns_finished(1);
        runtime_support::headless::write_turn_request(&run, "revise it", "revise");
        turns_finished(2);
        fs::write(dagq::domain::turn::exit_path(&run_dir), "").unwrap();
        wrapper.join().unwrap()
    })
    .unwrap();
    assert_eq!(ended["exit_code"], 0, "{ended}");

    // (1) The container's arguments.
    let args: Vec<String> = fs::read_to_string(state.join("run-args"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let text = args.join(" ");
    let container = format!("dagq-{QUEUE}-run-{}", run.id());
    for part in [
        format!("--rm --name {container}"),
        "--user 10001:10001".to_owned(),
        "--read-only".to_owned(),
        "--cap-drop all".to_owned(),
        "--security-opt no-new-privileges".to_owned(),
        "--memory 2048m --cpus 2 --pids-limit 2048".to_owned(),
        "--network none".to_owned(),
        format!("--workdir {}", clone.display()),
        "localhost/dagq-test-image:t dagq relay turn".to_owned(),
    ] {
        assert!(text.contains(&part), "{part}: {text}");
    }
    let volumes: Vec<&str> = args
        .iter()
        .zip(args.iter().skip(1))
        .filter(|(flag, _)| *flag == "--volume")
        .map(|(_, value)| value.as_str())
        .collect();
    let queue_key = QueueKey::new(QUEUE).unwrap();
    assert_eq!(
        volumes,
        [
            format!("{0}:{0}", clone.display()),
            format!("{0}:{0}", run_dir.display()),
            format!("dagq-{QUEUE}-socket-{}:/run/dagq-broker:z", run.id()),
            format!("dagq-{QUEUE}-build-{}:/build", run.id()),
            format!(
                "{}:/cache/registry",
                queue_key.worker_cache_volume_of("/cache/registry")
            ),
            format!(
                "{}:/cache/compiler",
                queue_key.worker_cache_volume_of("/cache/compiler")
            ),
        ]
    );
    let queue_dir = db.parent().unwrap();
    let home = std::env::var("HOME").unwrap();
    for absent in [
        "docker.sock".to_owned(),
        "podman.sock".to_owned(),
        run_clone::landing_path(&runs_dir(&db), run.id())
            .display()
            .to_string(),
        queue_dir.join("verification").display().to_string(),
        format!("{home}:"),
        // No compiler wrapper is given the container: only the image's is
        // unset.
        "RUSTC_WRAPPER=".to_owned(),
    ] {
        assert!(
            !volumes.iter().any(|volume| volume.contains(&absent)) && !text.contains(&absent),
            "{absent}: {text}"
        );
    }
    // The worker's token, which the container read from its copy in the
    // run directory; the copy goes with the container.
    let credential = dagq::application::queue_service::ServiceAccess::credential(
        &dagq::infrastructure::queue_service::SystemServiceAccess,
        &db,
        &format!("worker:{}", run.id()),
    )
    .unwrap();
    let token_value = fs::read_to_string(credential).unwrap();
    assert!(!token_value.trim().is_empty());
    assert!(!text.contains(token_value.trim()), "{text}");
    assert!(!run_dir.join(TOKEN_FILE).exists());

    // (2) What the agent was given inside: the task's prompt on its
    // standard input through podman's, never in podman's arguments.
    assert_eq!(
        fs::read_to_string(run_dir.join("stub-first-prompt")).unwrap(),
        "do the task"
    );
    assert!(!text.contains("do the task"), "{text}");
    let seen: HashMap<String, String> = fs::read_to_string(run_dir.join("stub-env"))
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    assert_eq!(seen["token"], token_value.trim());
    assert_eq!(seen["base"], "http://127.0.0.1:47310/anthropic");
    assert_eq!(seen["headers"], format!("x-dagq-run: {}", run.id()));
    assert_eq!(
        seen["proxy"],
        format!("http://{}:{}@127.0.0.1:47310", run.id(), token_value.trim())
    );
    assert_eq!(seen["service"], "http://127.0.0.1:47310");
    assert_eq!(
        seen["credential"],
        run_dir.join(TOKEN_FILE).display().to_string()
    );
    assert_eq!(
        seen["home"],
        run_dir.join("container-home").display().to_string()
    );
    assert_eq!(seen["target"], "/build/target");
    // The records' directory is in the run directory the container mounts
    // at its own path (ADR-t2589-1 decision 2).
    assert_eq!(
        seen["checks"],
        run_dir.join("local-checks").display().to_string()
    );
    assert_eq!(
        Path::new(&seen["pwd"]).canonicalize().unwrap(),
        clone.canonicalize().unwrap()
    );
    // (3) Main's bundle was fetched into the clone before the agent ran.
    assert_eq!(seen["main"], main);

    // The turn's end: the run branch's bundle and the run's state, as the
    // control side reads them.
    let state_file = fs::read_to_string(run_dir.join(run_clone::RUN_STATE_FILE)).unwrap();
    let state_read = run_clone::RunState::parse(&state_file).unwrap();
    let head = git_out(&clone, &["rev-parse", "HEAD"]);
    assert_eq!(state_read.head().as_str(), head);
    assert_eq!(
        state_read.branch.as_deref(),
        Some(format!("refs/heads/dagq/{}", run.id()).as_str())
    );
    assert_eq!(state_read.status, "?? notes.txt\n");
    assert!(!state_read.rebase_in_progress);
    let facts = GitRepository::inspect(&repo)
        .unwrap()
        .clone_facts(&run)
        .unwrap()
        .unwrap();
    assert_eq!(facts.head.as_str(), head);
    assert!(
        !run_dir
            .join(format!("{}.tmp", run_clone::RUN_BUNDLE_FILE))
            .exists()
    );

    // The turn's output and receipt are read as on the host, and the
    // container is removed at its end.
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let finished: Vec<&Value> = detail
        .events
        .iter()
        .filter(|event| event.kind == EventKind::TurnFinished)
        .map(|event| &event.payload)
        .collect();
    assert_eq!(finished.len(), 2, "{:?}", detail.events);
    for turn in &finished {
        assert_eq!(turn["outcome"], "succeeded", "{turn}");
    }
    let session = run.id().to_string();
    assert_eq!(
        fs::read_to_string(run_dir.join("stub-calls")).unwrap(),
        format!("start {session}\nresume {session}\n")
    );
    let output = fs::read_to_string(dagq::domain::turn::output_path(&run_dir, 1, "jsonl")).unwrap();
    assert!(output.contains("working in the container"), "{output}");
    let receipt: Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt["commit"], head);
    let removed = format!("--connection dagq rm --force --ignore --time 0 {container}");
    let calls = calls(&state);
    assert_eq!(
        calls.iter().filter(|call| **call == removed).count(),
        2,
        "{calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("--connection dagq run "))
            .count(),
        2,
        "{calls:?}"
    );
    assert!(
        fs::read_dir(state.join("containers"))
            .unwrap()
            .next()
            .is_none()
    );
}

/// The compiler wrapper the image of [`one_container_turn`] sets (its
/// `ENV`), which reaches the agent only where nothing else decides it.
const IMAGE_WRAPPER: &str = "/usr/local/bin/sccache-of-the-image";

/// One turn of a run in a clone whose worker `dagq.toml` (`config`) sets to
/// the container, in an image that sets [`IMAGE_WRAPPER`], the fake
/// podman's `sccache` the script `sccache` when given, the run directory
/// made ready by `prepare` before the turn, until
/// the turn finished and the session exits: the queue, the run, its
/// directory and the fake podman's state. Meanwhile the requests of the
/// turn's container are answered as the supervisor that leases the run
/// answers them on its passes ([`answer_requests`]).
fn one_container_turn(
    dir: &Path,
    config: &str,
    sccache: Option<&str>,
    prepare: impl FnOnce(&Path),
) -> (PathBuf, TaskRun, PathBuf, PathBuf) {
    one_container_turn_changed(dir, config, sccache, prepare, |_| {})
}

/// [`one_container_turn`] with `change` run on the main checkout once the
/// turn's container runs and its launcher left its request, before the
/// supervisor answers it.
fn one_container_turn_changed(
    dir: &Path,
    config: &str,
    sccache: Option<&str>,
    prepare: impl FnOnce(&Path),
    change: impl FnOnce(&Path) + Send,
) -> (PathBuf, TaskRun, PathBuf, PathBuf) {
    let (repo, db) = queue_fixture(dir);
    fs::write(repo.join("dagq.toml"), config).unwrap();
    let (run, token) = claim_in_container(&db, &repo, "container-run");
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let stand = Stand::start().unwrap();
    let start = BackgroundHandle::parse(&stand.handle).unwrap().start;
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        queue
            .workspace_created(run.id(), &token, &stand.handle)
            .unwrap();
        queue
            .record_runtime_event(
                run.id(),
                EventKind::WrapperLaunched,
                json!({"pid": stand.pid, "start": start, "workspace_id": stand.handle,
                       "log": run_dir.join("session.log").to_string_lossy()}),
            )
            .unwrap();
    }
    let state = dir.join("podman");
    let containers = containers(&state, &db, &repo);
    fs::write(
        state.join("image-env"),
        format!("RUSTC_WRAPPER={IMAGE_WRAPPER}\n"),
    )
    .unwrap();
    if let Some(script) = sccache {
        crate::common::template::script(&state.join("bin").join("sccache"), script);
    }
    // A gate and a request an earlier turn left are not this turn's.
    fs::write(run_dir.join("sccache-gate.earlier"), "started").unwrap();
    fs::write(request(&run_dir, "earlier"), "").unwrap();
    prepare(&run_dir);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let provider = dagq::infrastructure::adapters::ClaudeCode {
        executable: PathBuf::from("/nonexistent/claude-on-the-host"),
    };
    let ended = thread::scope(|scope| {
        scope.spawn(|| {
            // Errors a later pass got over, kept in the test's output.
            let answers = answer_requests(&containers, &db, &run, &stop, || change(&repo));
            if !answers.errors.is_empty() {
                eprintln!("{} passes, failed: {:#?}", answers.passes, answers.errors);
            }
        });
        // A panic below ends the answers too, so that the scope ends.
        let _stop = StopOnDrop(&stop);
        let wrapper = scope.spawn(|| {
            runtime::session_in_background_with_containers(
                &db,
                run.id(),
                &token,
                &provider,
                &process::LocalSpawner,
                false,
                stand.pid,
                &dagq::infrastructure::adapters::SystemProcesses,
                &containers,
            )
        });
        wait_until(&db, crate::common::STEP_LIMIT, |queue| {
            queue
                .show(TaskId::new(1))
                .unwrap()
                .events
                .iter()
                .any(|event| event.kind == EventKind::TurnFinished)
        });
        fs::write(dagq::domain::turn::exit_path(&run_dir), "").unwrap();
        let _waiting =
            crate::common::within(crate::common::STEP_LIMIT, "the session wrapper to return");
        wrapper.join().unwrap()
    })
    .unwrap();
    assert_eq!(ended["exit_code"], 0, "{ended}");
    (db, run, run_dir, state)
}

/// Sets its flag when dropped.
struct StopOnDrop<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// What [`answer_requests`] met: its passes that found requests to answer,
/// and the error of each pass that failed, in order.
struct Answers {
    passes: usize,
    errors: Vec<String>,
}

/// Answer the requests the launcher of `run`'s turn container leaves, as
/// the supervisor that leases the run does on each pass
/// ([`dagq::application::run_container::serve_sccache_request`]), until
/// `stop`, `before` run once before the first answer to a request of the
/// turn's (not the one an earlier turn left). As the supervisor's pass, a
/// pass that fails (a turn's container that started and was gone again
/// between its looks) leaves its requests for the next pass and is kept in
/// what it returns. Once stopped, a last pass that failed with a request
/// still there has one more pass (the supervisor's next), and when that
/// one fails too it panics with the passes and every error: errors that go
/// on to the end are not dropped.
fn answer_requests(
    containers: &dyn RunContainers,
    db: &Path,
    run: &TaskRun,
    stop: &std::sync::atomic::AtomicBool,
    before: impl FnOnce(),
) -> Answers {
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let earlier = request(&run_dir, "earlier");
    let earlier = earlier.file_name().unwrap().to_str().unwrap();
    let mut before = Some(before);
    let mut answers = Answers {
        passes: 0,
        errors: Vec::new(),
    };
    let mut failing = false;
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        let requests = containers.sccache_requests(&run_dir).unwrap_or_default();
        if !requests.is_empty() {
            if requests.iter().any(|name| name != earlier)
                && let Some(before) = before.take()
            {
                before();
            }
            answers.passes += 1;
            match dagq::application::run_container::serve_sccache_request(
                containers,
                &SqliteQueue::open(db).unwrap(),
                run,
                0,
            ) {
                Ok(()) => failing = false,
                Err(error) => {
                    answers.errors.push(format!("{error:#}"));
                    failing = true;
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    if failing
        && !containers
            .sccache_requests(&run_dir)
            .unwrap_or_default()
            .is_empty()
    {
        answers.passes += 1;
        match dagq::application::run_container::serve_sccache_request(
            containers,
            &SqliteQueue::open(db).unwrap(),
            run,
            0,
        ) {
            Ok(()) => failing = false,
            Err(error) => answers.errors.push(format!("{error:#}")),
        }
    }
    let left = containers.sccache_requests(&run_dir).unwrap_or_default();
    assert!(
        !failing || left.is_empty(),
        "the passes failed to the end with {left:?} left after {} passes: {:#?}",
        answers.passes,
        answers.errors
    );
    answers
}

/// What the stub agent of the fake podman was given.
fn agent_env(run_dir: &Path) -> HashMap<String, String> {
    fs::read_to_string(run_dir.join("stub-env"))
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// A `[run.env]` naming sccache whose server listens on `port` and keeps
/// its cache in the worker cache.
fn sccache_config(port: u16) -> String {
    sccache_config_naming("sccache", port)
}

/// [`sccache_config`] naming `program` as `RUSTC_WRAPPER`.
fn sccache_config_naming(program: &str, port: u16) -> String {
    format!(
        "[roles.worker]\nruntime = \"container\"\n\
         [container.worker_cache]\ndirs = [\"/cache/registry\", \"/cache/sccache\"]\n\
         [run.env]\nRUSTC_WRAPPER = '{program}'\nSCCACHE_SERVER_PORT = '{port}'\n\
         SCCACHE_DIR = '/cache/sccache/server'\nSCCACHE_ERROR_LOG = '/tmp/sccache.log'\n"
    )
}

/// Acceptance (1), (2) and (4) of task 2181: a `[run.env]` naming sccache
/// has the supervisor start the server inside the turn's container at its
/// launcher's request and before the agent runs (`podman exec`, no idle
/// timeout, not refused its start, the cache in the queue's worker cache
/// volume), make the guard there, and record the start on the run; the
/// agent is refused the server's start and compiles through the guard,
/// which finds the server on the loopback, never through
/// `RUSTC_WRAPPER = sccache` itself.
#[test]
fn the_container_s_sccache_server_is_started_before_the_agent_with_its_cache_in_the_worker_cache() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    // The container's server: what listens on the loopback, where the
    // guard looks. Started, the stub says how; given a compile, it says
    // so and runs the compiler.
    let server = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = server.local_addr().unwrap().port();
    let (db, run, run_dir, state) = one_container_turn(
        dir.path(),
        &sccache_config(port),
        Some(
            "#!/bin/sh\nif [ \"$1\" != --start-server ]; then \
             echo \"$*\" >> \"${0%/*}/../sccache-compiles\"; exec \"$@\"; fi\n\
             { echo \"args=$*\"; echo \"dir=$SCCACHE_DIR\"; \
             echo \"idle=$SCCACHE_IDLE_TIMEOUT\"; echo \"port=$SCCACHE_SERVER_PORT\"; \
             echo \"log=${SCCACHE_ERROR_LOG-unset}\"; echo \"wrapper=${RUSTC_WRAPPER-unset}\"; \
             } > \"${0%/*}/../sccache-started\"\n",
        ),
        |_| {},
    );
    let container = format!("dagq-{QUEUE}-run-{}", run.id());
    // The supervisor's calls in the running container: the server's
    // start, then the guard's making.
    let execs: Vec<String> = calls(&state)
        .into_iter()
        .filter(|call| call.starts_with("--connection dagq exec "))
        .collect();
    assert_eq!(execs.len(), 2, "{execs:?}");
    // By the container's ID, which no later turn's container has.
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let recorded = payloads(&detail, "sccache_container_server_started");
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    let id = recorded[0]["container_id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("id"), "{recorded:?}");
    assert!(
        execs[0].ends_with(&format!("{id} sccache --start-server")),
        "{execs:?}"
    );
    assert!(
        execs[1].ends_with(&format!(
            "{id} dagq relay guard --dir {}",
            run_dir.join("container-home").display()
        )),
        "{execs:?}"
    );
    let started = fs::read_to_string(state.join("sccache-started")).unwrap();
    for line in [
        "args=--start-server",
        "dir=/cache/sccache/server",
        "idle=0",
        &format!("port={port}"),
        "log=unset",
        "wrapper=unset",
    ] {
        assert!(
            started.lines().any(|seen| seen == line),
            "{line}: {started}"
        );
    }
    // The cache's directory is a mount of the queue's worker cache volume.
    let args = fs::read_to_string(state.join("run-args")).unwrap();
    let volume = QueueKey::new(QUEUE)
        .unwrap()
        .worker_cache_volume_of("/cache/sccache");
    assert!(
        args.lines()
            .any(|arg| arg == format!("{volume}:/cache/sccache")),
        "{args}"
    );
    // No wrapper is given the container, and the image's is unset: the
    // agent's is only the guard the launcher gives.
    assert!(!args.contains("RUSTC_WRAPPER="), "{args}");
    assert!(args.contains("--unsetenv\nRUSTC_WRAPPER\n"), "{args}");
    assert!(!args.contains("/tmp/sccache.log"), "{args}");
    // The agent ran after the start, refused its own start, behind the guard.
    let seen = agent_env(&run_dir);
    assert_eq!(seen["server"], "args=--start-server");
    let guard = run_dir.join("container-home").join("dagq-rustc-wrapper");
    assert_eq!(seen["wrapper"], guard.display().to_string());
    assert_eq!(seen["error_log"], dagq::domain::sccache::REFUSED_ERROR_LOG);
    assert_eq!(seen["sccache_program"], "sccache");
    // The guard found the server on the loopback and compiled through it.
    assert_eq!(seen["guarded"], "compiled");
    assert_eq!(
        fs::read_to_string(state.join("sccache-compiles")).unwrap(),
        "/bin/echo compiled\n"
    );
    drop(server);
    assert_eq!(turn_gate(&run_dir), "started");
    assert_eq!(requests(&run_dir), Vec::<String>::new());
    assert_eq!(recorded[0]["port"], port);
    assert_eq!(recorded[0]["container"], container);
    assert_eq!(recorded[0]["by"], "supervisor");
    assert!(payloads(&detail, "sccache_wrapper_removed").is_empty());
    assert!(payloads(&detail, "sccache_container_server_start_failed").is_empty());
}

/// The fake podman's `sccache` that says how it was started, and runs a
/// compile it is given.
const SAYS_HOW_STARTED: &str = "#!/bin/sh\nif [ \"$1\" != --start-server ]; then \
     echo \"$*\" >> \"${0%/*}/../sccache-compiles\"; exec \"$@\"; fi\n\
     { echo \"args=$*\"; echo \"dir=$SCCACHE_DIR\"; echo \"port=$SCCACHE_SERVER_PORT\"; \
     } > \"${0%/*}/../sccache-started\"\n";

/// Acceptance (1) and (2) of task 3282: `dagq.toml` changed after the
/// turn's container started and before the supervisor answers (another
/// port, a cache directory and a worker cache the container does not
/// mount) does not reach the server: it is started with the port and the
/// cache directory the container was started with, the port recorded is
/// the one the agent's guard sees, and the guard finds the server there.
#[test]
fn a_dagq_toml_changed_before_the_answer_starts_the_server_as_the_container_was_started() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let server = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = server.local_addr().unwrap().port();
    let changed = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let other = changed.local_addr().unwrap().port();
    drop(changed);
    let (db, _, run_dir, state) = one_container_turn_changed(
        dir.path(),
        &sccache_config(port),
        Some(SAYS_HOW_STARTED),
        |_| {},
        |repo| {
            fs::write(
                repo.join("dagq.toml"),
                format!(
                    "[roles.worker]\nruntime = \"container\"\n\
                     [container.worker_cache]\ndirs = [\"/tmp/new-cache\"]\n\
                     [run.env]\nRUSTC_WRAPPER = 'sccache'\nSCCACHE_SERVER_PORT = '{other}'\n\
                     SCCACHE_DIR = '/tmp/new-cache'\n"
                ),
            )
            .unwrap();
        },
    );
    let started = fs::read_to_string(state.join("sccache-started")).unwrap();
    for line in [
        "args=--start-server",
        "dir=/cache/sccache/server",
        &format!("port={port}"),
    ] {
        assert!(
            started.lines().any(|seen| seen == line),
            "{line}: {started}"
        );
    }
    let exec = fs::read_to_string(state.join("exec-args")).unwrap();
    assert!(!exec.contains("/tmp/new-cache"), "{exec}");
    assert!(!exec.contains(&other.to_string()), "{exec}");
    let seen = agent_env(&run_dir);
    assert_eq!(seen["port"], port.to_string());
    assert_eq!(seen["guarded"], "compiled");
    drop(server);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let recorded = payloads(&detail, "sccache_container_server_started");
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0]["port"], port);
    assert!(payloads(&detail, "sccache_wrapper_removed").is_empty());
}

/// Acceptance (3) of task 2181: a server that cannot be started in the
/// container leaves the agent without `RUSTC_WRAPPER`, still refused the
/// server's start, and the failure and the removal are recorded on the
/// run.
#[test]
fn a_container_s_sccache_server_that_cannot_start_leaves_the_agent_without_the_wrapper() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (db, _, run_dir, _) = one_container_turn(
        dir.path(),
        &sccache_config(4301),
        Some(
            "#!/bin/sh\necho 'sccache: error: Timed out waiting for server startup' >&2\nexit 2\n",
        ),
        |_| {},
    );
    let seen = agent_env(&run_dir);
    // Nor the image's: the container was started without it.
    assert_eq!(seen["wrapper"], "unset");
    assert_eq!(seen["error_log"], dagq::domain::sccache::REFUSED_ERROR_LOG);
    assert!(
        turn_gate(&run_dir).starts_with("failed: the container's sccache server was not started")
    );
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let failed = payloads(&detail, "sccache_container_server_start_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(
        failed[0]["reason"]
            .as_str()
            .unwrap()
            .contains("Timed out waiting for server startup"),
        "{failed:?}"
    );
    let removed = payloads(&detail, "sccache_wrapper_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["by"], "supervisor");
    assert_eq!(removed[0]["port"], 4301);
    assert_eq!(removed[0]["container_id"], failed[0]["container_id"]);
}

/// Acceptance (3) of task 2181: a server started whose guard the
/// supervisor cannot make in the container (a directory left where the
/// link goes) closes the gate: the agent runs without `RUSTC_WRAPPER`,
/// and the removal is recorded beside the start.
#[test]
fn a_guard_the_container_cannot_make_leaves_the_agent_without_the_wrapper_recorded() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (db, _, run_dir, _) = one_container_turn(
        dir.path(),
        &sccache_config(4301),
        Some("#!/bin/sh\nexit 0\n"),
        |run_dir| {
            let left = run_dir.join("container-home").join("dagq-rustc-wrapper");
            fs::create_dir_all(&left).unwrap();
            fs::write(left.join("kept"), "").unwrap();
        },
    );
    let seen = agent_env(&run_dir);
    // Nor the image's: the container was started without it.
    assert_eq!(seen["wrapper"], "unset");
    assert_eq!(seen["error_log"], dagq::domain::sccache::REFUSED_ERROR_LOG);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(
        payloads(&detail, "sccache_container_server_started").len(),
        1
    );
    let removed = payloads(&detail, "sccache_wrapper_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert!(
        removed[0]["reason"]
            .as_str()
            .unwrap()
            .contains("the guard was not made in the container"),
        "{removed:?}"
    );
    assert_eq!(removed[0]["port"], 4301);
}

/// Acceptance (1) of task 2181 through the supervisor (ADR-t2114-4
/// decision 1): the supervisor that leases a run in a clone answers its
/// turn container's request on its pass, starting the server and making
/// the guard there before the agent runs, which compiles through the
/// guard; the run lands as any other. The supervisor claims nothing while
/// a program of `[run.env]` is not found on the host, so `RUSTC_WRAPPER`
/// names the fake podman's sccache by its path: the host the test runs on
/// need have no sccache of its own.
#[test]
fn the_supervisor_starts_the_sccache_server_of_a_turn_s_container_on_its_pass() {
    let _test = crate::common::test();
    let queue = ContainerQueue::configured(None, |state| {
        let program = state.join("bin").join("sccache");
        sccache_config_naming(program.to_str().unwrap(), 4301)
    });
    crate::common::template::script(
        &queue.state.join("bin").join("sccache"),
        "#!/bin/sh\n[ \"$1\" = --start-server ] && echo started > \"${0%/*}/../sccache-started\"\nexit 0\n",
    );
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
    let outcome = supervise_reviewed_with(
        &queue.db,
        &queue.repo,
        &queue.backend,
        &reviewer,
        &queue.options,
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let detail = queue.detail();
    let run_dir = PathBuf::from(detail.runs[0].run_dir().unwrap());
    let seen = agent_env(&run_dir);
    assert_eq!(seen["server"], "started");
    assert_eq!(
        seen["wrapper"],
        run_dir
            .join("container-home")
            .join("dagq-rustc-wrapper")
            .display()
            .to_string()
    );
    let started = payloads(&detail, "sccache_container_server_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["by"], "supervisor");
    assert!(payloads(&detail, "sccache_wrapper_removed").is_empty());
    queue.none_left();
}

/// A run in a clone whose `dagq.toml` names sccache with its cache in the
/// worker cache, its turn's container running in the fake machine with
/// the ID `id` (the fake podman's `sccache` exits 0), and the variables
/// its launcher reads: the queue, the run, its directory, the fake
/// podman's state and its containers.
fn running_container(
    dir: &Path,
    id: &str,
) -> (
    PathBuf,
    TaskRun,
    PathBuf,
    PathBuf,
    dagq::infrastructure::run_container::PodmanRunContainers,
) {
    let (repo, db) = queue_fixture(dir);
    fs::write(repo.join("dagq.toml"), sccache_config(4301)).unwrap();
    let (run, _) = claim_in_container(&db, &repo, "container-run");
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    fs::create_dir_all(run_dir.join("container-home")).unwrap();
    let state = dir.join("podman");
    let containers = containers(&state, &db, &repo);
    crate::common::template::script(&state.join("bin").join("sccache"), "#!/bin/sh\nexit 0\n");
    fs::write(
        state
            .join("containers")
            .join(format!("dagq-{QUEUE}-run-{}", run.id())),
        container_line(id, &run_dir, TURN, 4301, "/cache/sccache") + "\n",
    )
    .unwrap();
    (db, run, run_dir, state, containers)
}

/// The fake machine's line of a running turn container of the ID `id` and
/// the token `turn` in `run_dir` (what `podman container inspect` prints):
/// started to have its server listen on `port` with its cache in
/// `/cache/sccache/server`, its worker cache volume mounted at `mounted`.
fn container_line(id: &str, run_dir: &Path, turn: &str, port: u16, mounted: &str) -> String {
    format!(
        "{id}\ntrue\n[\"DAGQ_SCCACHE_GATE={}\",\"DAGQ_SCCACHE_TURN={turn}\",\
         \"DAGQ_SCCACHE_PROGRAM=sccache\",\"SCCACHE_SERVER_PORT={port}\",\
         \"SCCACHE_DIR=/cache/sccache/server\"]\n\
         [{{\"Type\":\"volume\",\"Name\":\"{}\",\"Destination\":\"{mounted}\"}}]",
        gate_file(run_dir, turn).display(),
        QueueKey::new(QUEUE)
            .unwrap()
            .worker_cache_volume_of(mounted),
    )
}

/// The token of the turn whose container [`running_container`] makes.
const TURN: &str = "turn1";

/// The request the launcher of the turn of the token `turn` leaves in
/// `run_dir`.
fn request(run_dir: &Path, turn: &str) -> PathBuf {
    run_dir.join(format!("sccache-request.{turn}"))
}

/// The gate of the turn of the token `turn` in `run_dir`.
fn gate_file(run_dir: &Path, turn: &str) -> PathBuf {
    run_dir.join(format!("sccache-gate.{turn}"))
}

/// The names in `run_dir` that start with `prefix`, sorted.
fn named(run_dir: &Path, prefix: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(run_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with(prefix))
        .collect();
    names.sort();
    names
}

/// The names of the turns' requests in `run_dir`.
fn requests(run_dir: &Path) -> Vec<String> {
    named(run_dir, "sccache-request")
}

/// The text of the one gate in `run_dir`: the last turn's, the earlier
/// turns' cleared before it.
fn turn_gate(run_dir: &Path) -> String {
    let gates = named(run_dir, "sccache-gate");
    assert_eq!(gates.len(), 1, "{gates:?}");
    fs::read_to_string(run_dir.join(&gates[0])).unwrap()
}

/// The variables of the launcher of a turn in `run_dir` whose server the
/// supervisor starts.
fn launcher_env(run_dir: &Path) -> impl Fn(&str) -> Option<String> {
    let gate = gate_file(run_dir, TURN).display().to_string();
    move |key| match key {
        "DAGQ_SCCACHE_GATE" => Some(gate.clone()),
        "DAGQ_SCCACHE_TURN" => Some(TURN.to_owned()),
        "DAGQ_SCCACHE_PROGRAM" => Some("sccache".to_owned()),
        _ => None,
    }
}

/// Acceptance (3) of task 2181: a launcher whose wait for the gate ends
/// before the supervisor answers writes its own gate and runs the agent
/// without `RUSTC_WRAPPER`; the supervisor answering late starts nothing,
/// leaves that gate, and records the removal once for the container,
/// however often a request is left again.
#[test]
fn a_launcher_whose_gate_comes_late_runs_without_the_wrapper_as_recorded() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (db, run, run_dir, state, containers) = running_container(dir.path(), "idlate");
    let vars = dagq::infrastructure::container_turn::sccache_env(
        &run_dir,
        launcher_env(&run_dir),
        Duration::ZERO,
    )
    .unwrap();
    assert_eq!(
        vars,
        [(
            "SCCACHE_ERROR_LOG".to_owned(),
            dagq::domain::sccache::REFUSED_ERROR_LOG.to_owned()
        )]
    );
    let gate = fs::read_to_string(gate_file(&run_dir, TURN)).unwrap();
    assert!(gate.starts_with("timed out: "), "{gate}");
    assert!(request(&run_dir, TURN).exists());
    for _ in 0..2 {
        dagq::application::run_container::serve_sccache_request(
            &containers,
            &SqliteQueue::open(&db).unwrap(),
            &run,
            0,
        )
        .unwrap();
        assert!(!request(&run_dir, TURN).exists());
        // The worker leaves another.
        fs::write(request(&run_dir, TURN), "").unwrap();
    }
    assert_eq!(fs::read_to_string(gate_file(&run_dir, TURN)).unwrap(), gate);
    assert!(
        !calls(&state)
            .iter()
            .any(|call| call.starts_with("--connection dagq exec ")),
        "{:?}",
        calls(&state)
    );
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let removed = payloads(&detail, "sccache_wrapper_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["by"], "supervisor");
    assert_eq!(removed[0]["container_id"], "idlate");
    assert!(
        removed[0]["reason"]
            .as_str()
            .unwrap()
            .contains("found no sccache gate"),
        "{removed:?}"
    );
    assert!(payloads(&detail, "sccache_container_server_started").is_empty());
}

/// Acceptance (3) of task 2181: a gate nothing recorded yet is recorded
/// once as it says. The supervisor's own an earlier process wrote and did
/// not record (it stopped in between) is recorded for its container; a
/// launcher's timed out one whose container ended before any answer is
/// recorded by the gate's own token. A request left again records nothing
/// more.
#[test]
fn a_gate_nothing_recorded_is_recorded_once_as_it_says() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (db, run, run_dir, state, containers) = running_container(dir.path(), "idkept");
    let serve = |turn: &str| {
        fs::write(request(&run_dir, turn), "").unwrap();
        dagq::application::run_container::serve_sccache_request(
            &containers,
            &SqliteQueue::open(&db).unwrap(),
            &run,
            0,
        )
        .unwrap();
        assert!(!request(&run_dir, turn).exists());
    };
    let detail = || {
        SqliteQueue::open(&db)
            .unwrap()
            .show(TaskId::new(1))
            .unwrap()
    };
    fs::write(gate_file(&run_dir, TURN), "started").unwrap();
    serve(TURN);
    serve(TURN);
    let first = detail();
    let started = payloads(&first, "sccache_container_server_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["container_id"], "idkept");
    // The next turn's launcher waited out its gate, and its container
    // ended before the supervisor looked.
    fs::remove_file(
        state
            .join("containers")
            .join(format!("dagq-{QUEUE}-run-{}", run.id())),
    )
    .unwrap();
    let gate = "timed out: no sccache gate within 180s (gate 0123abcd)";
    fs::write(gate_file(&run_dir, "turn2"), gate).unwrap();
    serve("turn2");
    serve("turn2");
    let then = detail();
    let removed = payloads(&then, "sccache_wrapper_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["gate"], gate);
    assert_eq!(removed[0]["container_id"], Value::Null);
    assert!(
        !calls(&state)
            .iter()
            .any(|call| call.starts_with("--connection dagq exec ")),
        "{:?}",
        calls(&state)
    );
}

/// A request in a running container its wrapper gave no sccache gate (a
/// worker's, not a launcher's) starts nothing and records nothing: it is
/// only taken away.
#[test]
fn a_request_of_a_container_given_no_gate_is_not_served() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (db, run, run_dir, state, containers) = running_container(dir.path(), "idforged");
    fs::write(
        state
            .join("containers")
            .join(format!("dagq-{QUEUE}-run-{}", run.id())),
        "idforged\ntrue\n[]\n[]\n",
    )
    .unwrap();
    fs::write(request(&run_dir, TURN), "").unwrap();
    dagq::application::run_container::serve_sccache_request(
        &containers,
        &SqliteQueue::open(&db).unwrap(),
        &run,
        0,
    )
    .unwrap();
    assert!(!request(&run_dir, TURN).exists());
    assert!(!gate_file(&run_dir, TURN).exists());
    assert!(
        !calls(&state)
            .iter()
            .any(|call| call.starts_with("--connection dagq exec ")),
        "{:?}",
        calls(&state)
    );
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    for kind in [
        "sccache_wrapper_removed",
        "sccache_container_server_started",
        "sccache_container_server_start_failed",
    ] {
        assert!(payloads(&detail, kind).is_empty(), "{kind}");
    }
}

/// Acceptance (2) and (3) of task 3282: the server's plan is the running
/// container's, never `dagq.toml`'s by now nor one the worker leaves in the
/// run directory. With `dagq.toml` and a planted plan naming another port
/// and a cache the container does not mount, the server starts with the
/// container's port and cache; a container whose cache directory is not in
/// its worker cache mounts starts none, and the agent runs without
/// `RUSTC_WRAPPER` as recorded.
#[test]
fn the_server_s_plan_is_the_running_container_s_whatever_dagq_toml_or_the_run_directory_say() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (db, run, run_dir, state, containers) = running_container(dir.path(), "idplan");
    let repo = dir.path().join("repo");
    assert!(repo.join("dagq.toml").exists());
    fs::write(
        repo.join("dagq.toml"),
        "[roles.worker]\nruntime = \"container\"\n\
         [container.worker_cache]\ndirs = [\"/tmp/new-cache\"]\n\
         [run.env]\nRUSTC_WRAPPER = 'sccache'\nSCCACHE_SERVER_PORT = '4999'\n\
         SCCACHE_DIR = '/tmp/new-cache'\n",
    )
    .unwrap();
    for name in ["sccache-plan", "sccache-plan.json", "container-env"] {
        fs::write(
            run_dir.join(name),
            "SCCACHE_SERVER_PORT=4998\nSCCACHE_DIR=/tmp/forged\n",
        )
        .unwrap();
    }
    crate::common::template::script(&state.join("bin").join("sccache"), SAYS_HOW_STARTED);
    fs::write(request(&run_dir, TURN), "").unwrap();
    dagq::application::run_container::serve_sccache_request(
        &containers,
        &SqliteQueue::open(&db).unwrap(),
        &run,
        0,
    )
    .unwrap();
    let started = fs::read_to_string(state.join("sccache-started")).unwrap();
    assert!(started.contains("port=4301\n"), "{started}");
    assert!(started.contains("dir=/cache/sccache/server\n"), "{started}");
    assert_eq!(turn_gate(&run_dir), "started");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let recorded = payloads(&detail, "sccache_container_server_started");
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0]["port"], 4301);

    // The next turn's container mounts its worker cache elsewhere than its
    // cache directory: its server is not started.
    let container = state
        .join("containers")
        .join(format!("dagq-{QUEUE}-run-{}", run.id()));
    fs::write(
        &container,
        container_line("idbare", &run_dir, "turn2", 4301, "/cache/registry") + "\n",
    )
    .unwrap();
    fs::remove_file(state.join("sccache-started")).unwrap();
    fs::write(request(&run_dir, "turn2"), "").unwrap();
    dagq::application::run_container::serve_sccache_request(
        &containers,
        &SqliteQueue::open(&db).unwrap(),
        &run,
        0,
    )
    .unwrap();
    assert!(!state.join("sccache-started").exists());
    let gate = fs::read_to_string(gate_file(&run_dir, "turn2")).unwrap();
    assert!(gate.starts_with("failed: "), "{gate}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let failed = payloads(&detail, "sccache_container_server_start_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["container_id"], "idbare");
    let removed = payloads(&detail, "sccache_wrapper_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["container_id"], "idbare");
    assert_eq!(removed[0]["port"], 4301);
    assert!(
        removed[0]["reason"]
            .as_str()
            .unwrap()
            .contains("/cache/sccache/server is not in a directory"),
        "{removed:?}"
    );
}

/// Leave in `run_dir` the next turn's container, of the ID `id` and the
/// token `turn`, in place of the run's container `container` (the fake
/// machine's file of it), and its launcher's request: what the fake
/// machine shows once one turn's container ended and the next one's runs.
fn next_turn(container: &Path, run_dir: &Path, id: &str, turn: &str) {
    fs::write(
        container,
        container_line(id, run_dir, turn, 4301, "/cache/sccache") + "\n",
    )
    .unwrap();
    fs::write(request(run_dir, turn), "").unwrap();
}

/// An answer whose container ended while it started the server (the next
/// turn's container runs and its launcher left its request) takes away only
/// the request it answered and writes no gate: the next turn's request is
/// left for the next pass, which starts that container's server.
#[test]
fn an_answer_that_outlives_its_turn_leaves_the_next_turn_s_request() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (db, run, run_dir, state, containers) = running_container(dir.path(), "idold");
    let container = state
        .join("containers")
        .join(format!("dagq-{QUEUE}-run-{}", run.id()));
    // While the old container's server starts, that container ends, the
    // next turn's starts and its launcher leaves its request.
    let line = container_line("idnew", &run_dir, "turn2", 4301, "/cache/sccache");
    crate::common::template::script(
        &state.join("bin").join("sccache"),
        format!(
            "#!/bin/sh\n[ -f {switched} ] && exit 0\n: > {switched}\nprintf '%s\\n' {line} > {container}\n: > {next}\n",
            switched = crate::common::shell_path(state.join("switched")),
            line = dagq::infrastructure::adapters::shell_quote(&line),
            container = crate::common::shell_path(&container),
            next = crate::common::shell_path(request(&run_dir, "turn2")),
        ),
    );
    fs::write(request(&run_dir, TURN), "").unwrap();
    let serve = || {
        dagq::application::run_container::serve_sccache_request(
            &containers,
            &SqliteQueue::open(&db).unwrap(),
            &run,
            0,
        )
        .unwrap();
    };
    serve();
    assert_eq!(requests(&run_dir), ["sccache-request.turn2"]);
    assert_eq!(named(&run_dir, "sccache-gate"), Vec::<String>::new());
    let detail = || {
        SqliteQueue::open(&db)
            .unwrap()
            .show(TaskId::new(1))
            .unwrap()
    };
    assert!(payloads(&detail(), "sccache_container_server_started").is_empty());
    serve();
    assert_eq!(requests(&run_dir), Vec::<String>::new());
    assert_eq!(
        fs::read_to_string(gate_file(&run_dir, "turn2")).unwrap(),
        "started"
    );
    let then = detail();
    let started = payloads(&then, "sccache_container_server_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["container_id"], "idnew");
    assert!(payloads(&then, "sccache_wrapper_removed").is_empty());
}

/// The run containers of the fake machine (`inner`), with what a test
/// makes happen meanwhile: `on_look` right after each look at the
/// container that runs returned (given its count, from 1), and `on_guard`
/// right after the guard was made.
struct Hooked<'a> {
    inner: &'a dagq::infrastructure::run_container::PodmanRunContainers,
    looks: std::sync::atomic::AtomicUsize,
    on_look: Box<dyn Fn(usize) + Send + Sync + 'a>,
    on_guard: Box<dyn Fn() + Send + Sync + 'a>,
}

impl<'a> Hooked<'a> {
    fn new(inner: &'a dagq::infrastructure::run_container::PodmanRunContainers) -> Self {
        Self {
            inner,
            looks: std::sync::atomic::AtomicUsize::new(0),
            on_look: Box::new(|_| {}),
            on_guard: Box::new(|| {}),
        }
    }
}

impl RunContainers for Hooked<'_> {
    fn prepare(
        &self,
        run: &TaskRun,
    ) -> anyhow::Result<dagq::application::run_container::PreparedRun> {
        self.inner.prepare(run)
    }
    fn prepare_run_dir(&self, run_dir: &Path, credential: &Path) -> anyhow::Result<PathBuf> {
        self.inner.prepare_run_dir(run_dir, credential)
    }
    fn command(&self, args: &[String]) -> dagq::application::CommandSpec {
        self.inner.command(args)
    }
    fn remove(&self, run: &dagq::domain::RunId) -> anyhow::Result<()> {
        self.inner.remove(run)
    }
    fn reserve_memory(
        &self,
        run: &TaskRun,
        prepared: &dagq::application::run_container::PreparedRun,
        stop: &dyn Fn() -> Option<String>,
        notes: &mut dyn FnMut(
            &dagq::domain::machine_memory::MemoryAsk,
            dagq::domain::machine_memory::MemoryNote<'_>,
        ),
    ) -> anyhow::Result<()> {
        self.inner.reserve_memory(run, prepared, stop, notes)
    }
    fn release_memory(&self, run: &dagq::domain::RunId) -> anyhow::Result<()> {
        self.inner.release_memory(run)
    }
    fn memory_started(&self, run: &dagq::domain::RunId) -> anyhow::Result<()> {
        self.inner.memory_started(run)
    }
    fn facts_turn(&self, run: &TaskRun) -> anyhow::Result<()> {
        self.inner.facts_turn(run)
    }
    fn sccache_requests(&self, run_dir: &Path) -> anyhow::Result<Vec<String>> {
        self.inner.sccache_requests(run_dir)
    }
    fn take_sccache_requests(&self, run_dir: &Path, names: &[String]) -> anyhow::Result<()> {
        self.inner.take_sccache_requests(run_dir, names)
    }
    fn running_container(
        &self,
        run: &dagq::domain::RunId,
    ) -> anyhow::Result<Option<dagq::application::run_container::RunningContainer>> {
        let found = self.inner.running_container(run)?;
        (self.on_look)(self.looks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1);
        Ok(found)
    }
    fn start_sccache(
        &self,
        container: &str,
        program: &str,
        env: &[(String, String)],
    ) -> anyhow::Result<()> {
        self.inner.start_sccache(container, program, env)
    }
    fn make_sccache_guard(&self, container: &str, run_dir: &Path) -> anyhow::Result<()> {
        let made = self.inner.make_sccache_guard(container, run_dir);
        (self.on_guard)();
        made
    }
    fn claim_sccache_gate(
        &self,
        run_dir: &Path,
        name: &str,
        gate: &dagq::domain::sccache::ContainerGate,
    ) -> dagq::domain::sccache::GateClaim {
        self.inner.claim_sccache_gate(run_dir, name, gate)
    }
    fn read_sccache_gate(&self, run_dir: &Path, name: &str) -> anyhow::Result<Option<String>> {
        self.inner.read_sccache_gate(run_dir, name)
    }
}

/// An answer whose container is replaced by the next turn's after the
/// supervisor saw it still run and before it writes its verdict writes
/// only the gate of the turn it answered: the next turn's launcher, which
/// waits for its own turn's gate, finds none, and the next pass starts that
/// container's server and opens its gate.
#[test]
fn an_answer_whose_container_is_replaced_before_its_gate_opens_no_later_turn_s_gate() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (db, run, run_dir, state, podman) = running_container(dir.path(), "idold");
    let container = state
        .join("containers")
        .join(format!("dagq-{QUEUE}-run-{}", run.id()));
    let mut containers = Hooked::new(&podman);
    // The first look finds the container to answer; the second is the one
    // before the gate is written.
    containers.on_look = Box::new(|look| {
        if look == 2 {
            next_turn(&container, &run_dir, "idnew", "turn2");
        }
    });
    fs::write(request(&run_dir, TURN), "").unwrap();
    let serve = || {
        dagq::application::run_container::serve_sccache_request(
            &containers,
            &SqliteQueue::open(&db).unwrap(),
            &run,
            0,
        )
        .unwrap();
    };
    serve();
    assert_eq!(
        fs::read_to_string(gate_file(&run_dir, TURN)).unwrap(),
        "started"
    );
    assert!(!gate_file(&run_dir, "turn2").exists());
    assert_eq!(requests(&run_dir), ["sccache-request.turn2"]);
    serve();
    assert_eq!(
        fs::read_to_string(gate_file(&run_dir, "turn2")).unwrap(),
        "started"
    );
    assert_eq!(requests(&run_dir), Vec::<String>::new());
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let started = payloads(&detail, "sccache_container_server_started");
    let ids: Vec<_> = started.iter().map(|event| &event["container_id"]).collect();
    assert_eq!(ids, ["idold", "idnew"], "{started:?}");
    assert!(payloads(&detail, "sccache_wrapper_removed").is_empty());
    // Each container's server was started in that container, by its ID.
    let starts: Vec<_> = calls(&state)
        .into_iter()
        .filter(|call| {
            call.starts_with("--connection dagq exec ") && call.ends_with("--start-server")
        })
        .collect();
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert!(starts[0].contains(" idold "), "{starts:?}");
    assert!(starts[1].contains(" idnew "), "{starts:?}");
}

/// Requests and timed out gates of other turns the worker leaves while its
/// container runs record nothing and are only taken away; once the turn
/// ended, what is left records one turn's removal, not one for each file.
#[test]
fn a_worker_s_requests_and_gates_record_nothing_while_its_container_runs() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (db, run, run_dir, state, containers) = running_container(dir.path(), "idrun");
    let leave = || {
        for turn in ["forged1", "forged2"] {
            fs::write(request(&run_dir, turn), "").unwrap();
            fs::write(gate_file(&run_dir, turn), format!("timed out: {turn}")).unwrap();
        }
    };
    let serve = || {
        dagq::application::run_container::serve_sccache_request(
            &containers,
            &SqliteQueue::open(&db).unwrap(),
            &run,
            0,
        )
        .unwrap();
        assert_eq!(requests(&run_dir), Vec::<String>::new());
    };
    let detail = || {
        SqliteQueue::open(&db)
            .unwrap()
            .show(TaskId::new(1))
            .unwrap()
    };
    for _ in 0..3 {
        leave();
        serve();
    }
    assert!(payloads(&detail(), "sccache_wrapper_removed").is_empty());
    leave();
    fs::remove_file(
        state
            .join("containers")
            .join(format!("dagq-{QUEUE}-run-{}", run.id())),
    )
    .unwrap();
    serve();
    let then = detail();
    let removed = payloads(&then, "sccache_wrapper_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["gate"], "timed out: forged1");
    assert_eq!(removed[0]["container_id"], Value::Null);
}

/// An inspect that fails for another reason than no container (the machine
/// cannot be asked) is an error that leaves the request for the next pass,
/// not a turn that ended; the next pass, the machine answering again,
/// starts the server.
#[test]
fn an_inspect_that_fails_leaves_the_request_for_the_next_pass() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (db, run, run_dir, state, containers) = running_container(dir.path(), "idup");
    fs::write(request(&run_dir, TURN), "").unwrap();
    let serve = || {
        dagq::application::run_container::serve_sccache_request(
            &containers,
            &SqliteQueue::open(&db).unwrap(),
            &run,
            0,
        )
    };
    let detail = || {
        SqliteQueue::open(&db)
            .unwrap()
            .show(TaskId::new(1))
            .unwrap()
    };
    fs::write(state.join("machine-down"), "").unwrap();
    let error = format!("{:#}", serve().unwrap_err());
    assert!(error.contains("unable to connect"), "{error}");
    assert_eq!(requests(&run_dir), ["sccache-request.turn1"]);
    assert!(!gate_file(&run_dir, TURN).exists());
    assert!(
        detail()
            .events
            .iter()
            .all(|event| !event.kind.as_str().starts_with("sccache"))
    );
    fs::remove_file(state.join("machine-down")).unwrap();
    serve().unwrap();
    assert_eq!(requests(&run_dir), Vec::<String>::new());
    assert_eq!(
        fs::read_to_string(gate_file(&run_dir, TURN)).unwrap(),
        "started"
    );
    let then = detail();
    let started = payloads(&then, "sccache_container_server_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["container_id"], "idup");
}

/// A turn's container that starts and is gone again between the looks of
/// one pass (an inspect that finds none, an exists that finds one, and the
/// inspect after it that finds none again) makes that pass an error of
/// podman's "no such container", not a container that runs not; the
/// requests are answered as the supervisor answers them
/// ([`answer_requests`]): the request is left for the next pass, which finds
/// no container and takes the request away.
#[test]
fn a_container_gone_between_the_looks_of_a_pass_leaves_the_request_for_the_next_pass() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (db, run, run_dir, state, containers) = running_container(dir.path(), "idgone");
    fs::remove_file(
        state
            .join("containers")
            .join(format!("dagq-{QUEUE}-run-{}", run.id())),
    )
    .unwrap();
    fs::write(state.join("exists-gone"), "").unwrap();
    let error = format!("{:#}", containers.running_container(run.id()).unwrap_err());
    assert!(error.contains("no such container"), "{error}");
    let looks: Vec<String> = calls(&state)
        .iter()
        .filter_map(|call| call.strip_prefix("--connection dagq container "))
        .filter_map(|call| call.split_whitespace().next().map(str::to_owned))
        .collect();
    assert_eq!(looks, ["inspect", "exists", "inspect"]);
    fs::write(request(&run_dir, TURN), "").unwrap();
    fs::write(state.join("exists-gone"), "").unwrap();
    let stop = std::sync::atomic::AtomicBool::new(false);
    let answers = thread::scope(|scope| {
        let answering = scope.spawn(|| answer_requests(&containers, &db, &run, &stop, || {}));
        let _stop = StopOnDrop(&stop);
        let until = std::time::Instant::now() + crate::common::STEP_LIMIT;
        while !requests(&run_dir).is_empty()
            && !answering.is_finished()
            && std::time::Instant::now() < until
        {
            thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        answering.join().unwrap()
    });
    assert_eq!(requests(&run_dir), Vec::<String>::new());
    assert!(!state.join("exists-gone").exists());
    assert_eq!(answers.errors.len(), 1, "{:?}", answers.errors);
    assert!(
        answers.errors[0].contains("no such container"),
        "{:?}",
        answers.errors
    );
    assert!(answers.passes >= 2, "{}", answers.passes);
}

/// A launcher whose wait ends while the supervisor starts the server
/// writes its timed out gate first: the supervisor's verdict loses to it,
/// the removal is recorded by that gate's token, and the same gate is not
/// recorded again once the turn ended with its request left again.
#[test]
fn a_launcher_s_gate_that_wins_over_the_answer_is_recorded_once() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (db, run, run_dir, state, podman) = running_container(dir.path(), "idslow");
    let gate = "timed out: the container's launcher found no sccache gate within 180s (gate 0a1b)";
    let mut containers = Hooked::new(&podman);
    containers.on_guard = Box::new(|| fs::write(gate_file(&run_dir, TURN), gate).unwrap());
    fs::write(request(&run_dir, TURN), "").unwrap();
    dagq::application::run_container::serve_sccache_request(
        &containers,
        &SqliteQueue::open(&db).unwrap(),
        &run,
        0,
    )
    .unwrap();
    assert_eq!(fs::read_to_string(gate_file(&run_dir, TURN)).unwrap(), gate);
    // The worker leaves the request again and the turn ends.
    fs::write(request(&run_dir, TURN), "").unwrap();
    fs::remove_file(
        state
            .join("containers")
            .join(format!("dagq-{QUEUE}-run-{}", run.id())),
    )
    .unwrap();
    dagq::application::run_container::serve_sccache_request(
        &podman,
        &SqliteQueue::open(&db).unwrap(),
        &run,
        0,
    )
    .unwrap();
    assert_eq!(requests(&run_dir), Vec::<String>::new());
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let removed = payloads(&detail, "sccache_wrapper_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["gate"], gate);
    assert_eq!(removed[0]["container_id"], "idslow");
}

/// Before a turn whose server is to be started, the wrapper clears every
/// gate and request earlier turns left (a link there removed, its target
/// kept), leaves the run directory's other files, and gives the new turn a
/// token of its own that names a gate and a request.
#[test]
fn a_new_turn_clears_every_earlier_turn_s_gate_and_request() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (_, _, run_dir, _, containers) = running_container(dir.path(), "idclear");
    for turn in ["turn1", "turn2"] {
        fs::write(request(&run_dir, turn), "").unwrap();
        fs::write(gate_file(&run_dir, turn), "started").unwrap();
    }
    let outside = dir.path().join("outside");
    fs::write(&outside, "kept").unwrap();
    std::os::unix::fs::symlink(&outside, request(&run_dir, "linked")).unwrap();
    fs::write(run_dir.join("sccache-gate"), "not a turn's").unwrap();
    let first = containers.clear_sccache(&run_dir).unwrap();
    assert_eq!(requests(&run_dir), Vec::<String>::new());
    assert_eq!(named(&run_dir, "sccache-gate"), ["sccache-gate"]);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "kept");
    let second = containers.clear_sccache(&run_dir).unwrap();
    assert_ne!(first, second);
    for turn in [&first, &second] {
        assert!(
            dagq::domain::sccache::container_gate(turn).is_some(),
            "{turn}"
        );
        assert!(
            dagq::domain::sccache::container_request(turn).is_some(),
            "{turn}"
        );
    }
}

/// Acceptance (3) of task 2181: a gate the supervisor cannot write is
/// recorded as the agent's run without `RUSTC_WRAPPER` beside the server's
/// start, and its launcher, which cannot write its own either, runs no
/// agent: no agent compiles through a guard the records deny.
#[test]
fn a_gate_that_cannot_be_written_leaves_no_agent_with_the_wrapper() {
    use std::os::unix::fs::PermissionsExt;
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (db, run, run_dir, _, containers) = running_container(dir.path(), "idro");
    fs::write(request(&run_dir, TURN), "").unwrap();
    fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o500)).unwrap();
    let served = dagq::application::run_container::serve_sccache_request(
        &containers,
        &SqliteQueue::open(&db).unwrap(),
        &run,
        0,
    );
    let launched = dagq::infrastructure::container_turn::sccache_env(
        &run_dir,
        launcher_env(&run_dir),
        Duration::ZERO,
    );
    fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700)).unwrap();
    // The request could not be taken away either.
    assert!(served.is_err());
    let error = format!("{:#}", launched.unwrap_err());
    assert!(error.contains("no agent runs"), "{error}");
    assert!(!gate_file(&run_dir, TURN).exists());
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(
        payloads(&detail, "sccache_container_server_started").len(),
        1
    );
    let removed = payloads(&detail, "sccache_wrapper_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert!(
        removed[0]["reason"]
            .as_str()
            .unwrap()
            .contains("could not be written"),
        "{removed:?}"
    );
}

/// Acceptance (3) and (4) of task 2181: a cache directory outside the
/// queue's worker cache starts no server, and the agent runs without
/// `RUSTC_WRAPPER`, recorded.
#[test]
fn a_cache_directory_outside_the_worker_cache_starts_no_container_server() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (db, _, run_dir, state) = one_container_turn(
        dir.path(),
        &sccache_config(4301).replace("/cache/sccache/server", "/elsewhere"),
        Some("#!/bin/sh\nexit 0\n"),
        |_| {},
    );
    assert!(!state.join("exec-args").exists());
    let seen = agent_env(&run_dir);
    // Nor the image's: the container was started without it.
    assert_eq!(seen["wrapper"], "unset");
    assert_eq!(seen["sccache_program"], "unset");
    assert_eq!(seen["error_log"], dagq::domain::sccache::REFUSED_ERROR_LOG);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let removed = payloads(&detail, "sccache_wrapper_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert!(
        removed[0]["reason"]
            .as_str()
            .unwrap()
            .contains("[container.worker_cache]"),
        "{removed:?}"
    );
    assert!(payloads(&detail, "sccache_container_server_started").is_empty());
}

/// Acceptance (1) of task 2181: a `[run.env]` naming no sccache starts no
/// server and gives the container its environment as it is, its own
/// compiler wrapper and log too.
#[test]
fn a_run_env_naming_no_sccache_starts_no_server_and_keeps_its_environment() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (db, _, run_dir, state) = one_container_turn(
        dir.path(),
        "[roles.worker]\nruntime = \"container\"\n\
         [container.worker_cache]\ndirs = [\"/cache/sccache\"]\n\
         [run.env]\nRUSTC_WRAPPER = 'cachepot'\nSCCACHE_ERROR_LOG = '/tmp/own.log'\n\
         SCCACHE_DIR = '/cache/sccache/server'\n",
        Some("#!/bin/sh\nexit 0\n"),
        |_| {},
    );
    assert!(!state.join("exec-args").exists());
    // Nothing the image sets is unset: no wrapper is handled.
    assert!(
        !fs::read_to_string(state.join("run-args"))
            .unwrap()
            .lines()
            .any(|arg| arg == "--unsetenv")
    );
    let seen = agent_env(&run_dir);
    assert_eq!(seen["wrapper"], "cachepot");
    assert_eq!(seen["error_log"], "/tmp/own.log");
    assert_eq!(seen["sccache_program"], "unset");
    assert_eq!(seen["server"], "");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    for kind in [
        "sccache_wrapper_removed",
        "sccache_container_server_started",
        "sccache_container_server_start_failed",
    ] {
        assert!(payloads(&detail, kind).is_empty(), "{kind}");
    }
}

/// A run in a clone is never started on the host once `dagq.toml` sets the
/// worker back to the host: its turn is refused, and no podman and no
/// agent runs.
#[test]
fn a_clone_s_turn_is_refused_when_the_worker_is_set_back_to_the_host() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (repo, db) = queue_fixture(dir.path());
    let (run, token) = claim_in_container(&db, &repo, "container-run");
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let stand = Stand::start().unwrap();
    let start = BackgroundHandle::parse(&stand.handle).unwrap().start;
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        queue
            .workspace_created(run.id(), &token, &stand.handle)
            .unwrap();
        queue
            .record_runtime_event(
                run.id(),
                EventKind::WrapperLaunched,
                json!({"pid": stand.pid, "start": start, "workspace_id": stand.handle,
                       "log": run_dir.join("session.log").to_string_lossy()}),
            )
            .unwrap();
    }
    let state = dir.path().join("podman");
    let containers = containers(&state, &db, &repo);
    let provider = dagq::infrastructure::adapters::ClaudeCode {
        executable: PathBuf::from("/nonexistent/claude-on-the-host"),
    };
    let error = runtime::session_in_background_with_containers(
        &db,
        run.id(),
        &token,
        &provider,
        &process::LocalSpawner,
        false,
        stand.pid,
        &dagq::infrastructure::adapters::SystemProcesses,
        &containers,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("refuses to start its turn on the host"),
        "{error:#}"
    );
    assert!(!state.join("run-args").exists());
    assert!(!run_dir.join("stub-env").exists());
}

/// Acceptance (4): a stop of a run's session or processes removes its
/// container (only a run that had one asks podman), and the run's end in
/// the machine removes its container, relay, socket and build output.
#[test]
fn a_stopped_or_ended_run_leaves_no_container_and_no_build_output() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (repo, db) = queue_fixture(dir.path());
    let state = dir.path().join("podman");
    let containers = containers(&state, &db, &repo);
    // A run on the host has no container: podman is not asked.
    containers.remove(&RunId::new("host-run").unwrap()).unwrap();
    assert!(calls(&state).is_empty(), "{:?}", calls(&state));
    let (run, _) = claim_in_container(&db, &repo, "container-run");
    let container = format!("dagq-{QUEUE}-run-{}", run.id());
    fs::write(state.join("containers").join(&container), "").unwrap();
    containers.remove(run.id()).unwrap();
    assert!(!state.join("containers").join(&container).exists());
    assert_eq!(
        calls(&state),
        [format!(
            "--connection dagq rm --force --ignore --time 0 {container}"
        )]
    );

    // The run's end: its relay's close takes its container and volumes.
    let relays = PodmanRelays::of_queue(
        PodmanCli {
            program: state.join("podman"),
        },
        &db,
        None,
    );
    fs::write(state.join("containers").join(&container), "").unwrap();
    for volume in [
        format!("dagq-{QUEUE}-socket-{}", run.id()),
        format!("dagq-{QUEUE}-build-{}", run.id()),
    ] {
        fs::write(state.join("volumes").join(volume), "").unwrap();
    }
    relays.close(run.id().as_str()).unwrap();
    assert!(
        fs::read_dir(state.join("containers"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(
        fs::read_dir(state.join("volumes"))
            .unwrap()
            .next()
            .is_none()
    );
}

/// The lines of a turn's script (see [`fake_podman`]) that add `lines`
/// (each a JSON object of the transcript, which may use `$NOW` and
/// `$SESSION`) to the session's transcript where Claude Code in the
/// container writes it: under `$HOME` (the run directory's agent home), in
/// the project directory named after the container's working directory
/// (the clone). `$NOW` is the time to the millisecond, so that a line is
/// inside the session's span.
fn write_transcript(lines: &[&str]) -> String {
    let lines: String = lines
        .iter()
        .map(|line| format!("printf '%s\\n' \"{}\"\n", line.replace('"', "\\\"")))
        .collect();
    format!(
        r#"PROJECT="$HOME/.claude/projects/$(printf '%s' "$PWD" | sed 's/[^A-Za-z0-9]/-/g')"
mkdir -p "$PROJECT"
NOW=$(perl -MTime::HiRes=time -MPOSIX=strftime -e '$t = time; printf "%s.%03dZ", strftime("%Y-%m-%dT%H:%M:%S", gmtime($t)), ($t - int($t)) * 1000')
{{
{lines}}} >> "$PROJECT/$SESSION.jsonl"
"#
    )
}

/// A line of the session's transcript (see [`write_transcript`]) of
/// `kind` with `content`, said by the model `claude-in-the-container`.
fn transcript_line(kind: &str, content: &str) -> String {
    format!(
        r#"{{"type":"{kind}","timestamp":"$NOW","sessionId":"$SESSION","version":"2.1.283","message":{{"model":"claude-in-the-container","content":{content}}}}}"#
    )
}

/// Acceptance (1) and (2) of task 2470: a run in a container whose turn
/// ended before its model answered (a login that ran out, before any
/// `result`) left its session in the run directory's agent home, not in
/// the host's `~/.claude`; the next turn finds it there and resumes it
/// rather than starting the same name again, which Claude Code would
/// refuse. The worker span's close takes its model and its commands from
/// that transcript, as a host run's from the host's.
#[test]
fn a_container_turn_ended_before_its_result_is_resumed_and_measured_from_the_agent_home() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let (repo, db) = queue_fixture(dir.path());
    fs::write(
        repo.join("dagq.toml"),
        "[roles.worker]\nruntime = \"container\"\n",
    )
    .unwrap();
    let (run, token) = claim_in_container(&db, &repo, "container-run");
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let stand = Stand::start().unwrap();
    let start = BackgroundHandle::parse(&stand.handle).unwrap().start;
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        queue
            .workspace_created(run.id(), &token, &stand.handle)
            .unwrap();
        queue
            .record_runtime_event(
                run.id(),
                EventKind::WrapperLaunched,
                json!({"pid": stand.pid, "start": start, "workspace_id": stand.handle,
                       "log": run_dir.join("session.log").to_string_lossy()}),
            )
            .unwrap();
    }
    let state = dir.path().join("podman");
    let containers = containers(&state, &db, &repo);
    // The first turn writes its transcript and ends before its `result`
    // with Claude Code's words for a login that ran out; the next one
    // finishes.
    let go = transcript_line("user", r#""go""#);
    let build = transcript_line(
        "assistant",
        r#"[{"type":"tool_use","id":"a","name":"Bash","input":{"command":"cargo build"}}]"#,
    );
    let built = transcript_line(
        "user",
        r#"[{"type":"tool_result","tool_use_id":"a","content":"ok"}]"#,
    );
    let done = transcript_line("assistant", r#"[{"type":"text","text":"done"}]"#);
    fs::write(
        state.join("turn.sh"),
        format!(
            "if [ ! -f \"$RUN_DIR/ended\" ]; then\ntouch \"$RUN_DIR/ended\"\n{}echo 'Invalid API key · Please run /login' >&2\nexit 1\nfi\n{}finish\n",
            write_transcript(&[&go]),
            write_transcript(&[&build, &built, &done]),
        ),
    )
    .unwrap();
    let provider = dagq::infrastructure::adapters::ClaudeCode {
        executable: PathBuf::from("/nonexistent/claude-on-the-host"),
    };
    let session = run.id().to_string();
    let turns_finished = |count: usize| {
        wait_until(&db, crate::common::STEP_LIMIT, |queue| {
            queue
                .show(TaskId::new(1))
                .unwrap()
                .events
                .iter()
                .filter(|event| event.kind == EventKind::TurnFinished)
                .count()
                >= count
        });
    };
    // The scope's join of the wrapper, implicit on a panic too, is bounded.
    let waiting = crate::common::within(
        crate::common::STEP_LIMIT,
        "the wrapper of the container's turns to return",
    );
    let ended = thread::scope(|scope| {
        let wrapper = scope.spawn(|| {
            runtime::session_in_background_with_containers(
                &db,
                run.id(),
                &token,
                &provider,
                &process::LocalSpawner,
                false,
                stand.pid,
                &dagq::infrastructure::adapters::SystemProcesses,
                &containers,
            )
        });
        turns_finished(1);
        // The session was left in the agent home, never in the host's.
        let project = run_dir.join("container-home/.claude/projects").join(
            dagq::infrastructure::transcripts::encode_cwd(run.worktree_path().unwrap()),
        );
        assert!(project.join(format!("{session}.jsonl")).is_file());
        // The provider's hold ended: the same session is asked again.
        runtime_support::headless::write_turn_request(
            &run,
            "go on",
            dagq::domain::turn::PROVIDER_RETRY,
        );
        turns_finished(2);
        fs::write(dagq::domain::turn::exit_path(&run_dir), "").unwrap();
        wrapper.join().unwrap()
    })
    .unwrap();
    drop(waiting);
    assert_eq!(ended["exit_code"], 0, "{ended}");
    let finished = payloads(
        &SqliteQueue::open(&db)
            .unwrap()
            .show(TaskId::new(1))
            .unwrap(),
        "turn_finished",
    )
    .into_iter()
    .cloned()
    .collect::<Vec<_>>();
    assert_eq!(finished[0]["failure"], "authentication", "{finished:?}");
    // The first turn did not say it made the session: only its transcript
    // in the agent home makes the next one resume.
    assert_eq!(finished[0]["session_created"], false, "{finished:?}");
    assert_eq!(finished[1]["outcome"], "succeeded", "{finished:?}");
    assert_eq!(
        fs::read_to_string(run_dir.join("stub-calls")).unwrap(),
        format!("start {session}\nresume {session}\n")
    );
    // The worker span's close read the agent home's transcript.
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let spans: Vec<&Value> = payloads(&detail, "session_closed")
        .into_iter()
        .filter(|span| span["kind"] == "worker")
        .collect();
    assert_eq!(spans.len(), 1, "{spans:?}");
    assert_eq!(spans[0]["model"], "claude-in-the-container", "{}", spans[0]);
    assert!(spans[0]["work"].is_object(), "{}", spans[0]);
    // Its command was read from that transcript; its repository declares no
    // profile and has no project rule, so no rule classifies it.
    let worktime = fs::read_to_string(run_dir.join("worktime.jsonl")).unwrap();
    let line: Value = serde_json::from_str(worktime.lines().next().unwrap()).unwrap();
    assert_eq!(line["command"], "cargo build", "{worktime}");
    assert_eq!(line["category"], "other_command", "{worktime}");
}

/// Acceptance (3) to (5) through the supervisor: once every part of the
/// container answers, a task whose worker `dagq.toml` sets to the
/// container is claimed into a clone, its turn runs in its container, its
/// receipt is checked from the bundle and state the turn left, its review
/// passes, and it lands; the session's stop after the review removes the
/// container, and none is left.
#[test]
fn a_task_set_to_the_container_lands_through_the_supervisor_and_leaves_no_container() {
    let _test = crate::common::test();
    let queue = ContainerQueue::new(None);
    let base = git_out(&queue.repo, &["rev-parse", "main"]);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
    let outcome = supervise_reviewed_with(
        &queue.db,
        &queue.repo,
        &queue.backend,
        &reviewer,
        &queue.options,
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let detail = queue.detail();
    let run = &detail.runs[0];
    assert!(run.in_container());
    // The worker's commit is on main, on top of the base.
    let landed = git_out(&queue.repo, &["rev-parse", "main"]);
    assert_ne!(landed, base);
    assert_eq!(git_out(&queue.repo, &["rev-parse", "main~1"]), base);
    assert_eq!(
        fs::read_to_string(queue.repo.join("change.txt")).unwrap(),
        "change 1\n"
    );
    // Removed at the turn's end and again by the stop after the review.
    assert_eq!(queue.removals(run), (2, 1));
    queue.none_left();
}

/// Acceptance (4), the triage's stop: a person answers `stop` to the
/// `stalled` ask of a run whose turns in the container end without a
/// receipt. The session waits between turns, with no container, and ends
/// on the answer: each turn's container was removed at the turn's end, the
/// stop finds no container to remove, and none is left.
#[test]
fn a_stop_answer_leaves_no_container_of_a_stalled_run() {
    let _test = crate::common::test();
    let queue = ContainerQueue::new(Some("denied; say refused\n"));
    let supervisor = queue.supervise(TestReviewer::new(&[verdict("pass", &[], "fine")]));
    wait_until(&queue.db, crate::common::STEP_LIMIT, |sqlite| {
        !stalled_asks(sqlite).is_empty()
    });
    let ask = stalled_asks(&SqliteQueue::open(&queue.db).unwrap()).remove(0);
    SqliteQueue::open(&queue.db)
        .unwrap()
        .answer(ask.id, "stop")
        .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    queue.backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.detail();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Failed);
    assert!(
        payloads(&detail, "stall_resolved")
            .iter()
            .any(|p| p["outcome"] == "answered_stop")
    );
    let (removals, turns) = queue.removals(run);
    assert!(turns >= 1);
    assert_eq!(removals, turns);
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    let last = |kind: &str| kinds.iter().rposition(|k| *k == kind).unwrap();
    assert!(last("turn_finished") < last("ask_answered"), "{kinds:?}");
    assert!(last("ask_answered") < last("session_exited"), "{kinds:?}");
    queue.none_left();
}

/// Acceptance (4), the cancel: a person cancels the landing of a run in
/// the container whose review raised a concern. The session was stopped
/// before the ask (its container removed then), so the cancel finds no
/// session; the run fails, the task is canceled, and no container is left.
#[test]
fn a_canceled_landing_leaves_no_container() {
    let _test = crate::common::test();
    let queue = ContainerQueue::new(None);
    let supervisor = queue.supervise(TestReviewer::new(&[verdict(
        "concern",
        &["needs a person"],
        "unsure",
    )]));
    wait_until(&queue.db, crate::common::STEP_LIMIT, |sqlite| {
        sqlite
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(|ask| ask.kind == AskKind::ApproveLanding)
    });
    let run = queue.detail().runs[0].clone();
    // Stopped after the review, before the ask.
    assert_eq!(queue.removals(&run), (2, 1));
    let ask = SqliteQueue::open(&queue.db)
        .unwrap()
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == AskKind::ApproveLanding)
        .unwrap();
    SqliteQueue::open(&queue.db)
        .unwrap()
        .answer(ask.id, "cancel")
        .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    // The answer is applied by the next supervisor.
    let outcome = joined(
        queue.supervise(TestReviewer::new(&[])),
        "the supervisor that applies the answer",
    )
    .unwrap();
    queue.backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.detail();
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    let (removals, turns) = queue.removals(&run);
    assert!(removals > turns, "{removals} removals for {turns} turns");
    queue.none_left();
}

/// Acceptance (4), the recovery job's `stop_processes`: a turn in the
/// container leaves an orphan in the clone that does nothing; the job
/// stops it and removes the run's container (whose processes no pid of the
/// host names), the turn's next try finishes, and the run lands with no
/// container left.
#[test]
fn stop_processes_removes_the_run_s_container() {
    let _test = crate::common::test();
    let mut queue = ContainerQueue::new(Some(
        r#"if [ ! -f "$RUN_DIR/bg.pid" ]; then
( sleep 600 >/dev/null 2>&1 & echo $! > "$RUN_DIR/bg.pid.tmp"; mv "$RUN_DIR/bg.pid.tmp" "$RUN_DIR/bg.pid" )
pid=$(cat "$RUN_DIR/bg.pid")
i=0; while kill -0 "$pid" 2>/dev/null && [ $i -lt 1200 ]; do sleep 0.05; i=$((i + 1)); done
kill "$pid" 2>/dev/null
fi
finish
"#,
    ));
    queue.options.stall =
        Some(dagq::domain::stall::StallConfig::default().with_millis("idle_process_secs", 1000));
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[
        crate::runtime_repair::recovery_verdict(&json!({
            "verdict": "repair",
            "confidence": "high",
            "diagnosis": "an orphan sleep holds the session",
            "actions": [{"action": "stop_processes", "pids": ["PID"]}],
        })),
    ]);
    let outcome = supervise_reviewed_with(
        &queue.db,
        &queue.repo,
        &queue.backend,
        &reviewer,
        &queue.options,
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.detail();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Integrated);
    let repaired = payloads(&detail, "auto_repaired");
    let stops: Vec<&&Value> = repaired
        .iter()
        .filter(|p| p["repair"] == "stop_processes")
        .collect();
    assert_eq!(stops.len(), 1, "{repaired:?}");
    let processes = stops[0]["processes"].as_array().unwrap();
    assert!(
        processes.iter().any(|p| p["container"] == true),
        "{processes:?}"
    );
    // Each turn's end, the stop of the processes and the stop after the
    // review.
    let (removals, turns) = queue.removals(run);
    assert!(
        removals >= turns + 2,
        "{removals} removals for {turns} turns"
    );
    queue.none_left();
}

/// Acceptance (4), the adopt: a run in the container whose supervisor
/// died is adopted by a supervisor started afresh, which knows nothing of
/// the run's container but its name; its stop after the review removes
/// the container, and none is left.
#[test]
fn an_adopted_run_s_container_is_removed_by_its_stop() {
    let _test = crate::common::test();
    let queue = ContainerQueue::new(None);
    let (run, _) = claim_in_container(&queue.db, &queue.repo, "dead-supervisor");
    let handle = queue
        .backend
        .launch_background(
            Path::new(run.worktree_path().unwrap()),
            &shell_join(&[
                "runner".into(),
                "session".into(),
                "--run".into(),
                run.id().to_string(),
                "--lease".into(),
                "dead-supervisor".into(),
                "--background".into(),
            ]),
            &[],
            &Path::new(run.run_dir().unwrap()).join("session.log"),
        )
        .unwrap();
    let mut sqlite = SqliteQueue::open(&queue.db).unwrap();
    record_background_start(&queue.db, &mut sqlite, &run, "dead-supervisor", &handle);
    wait_until(&queue.db, crate::common::STEP_LIMIT, |sqlite| {
        !payloads(&sqlite.show(TaskId::new(1)).unwrap(), "turn_finished").is_empty()
    });
    age_lease(&queue.db, &run, 31);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
    let outcome = supervise_reviewed_with(
        &queue.db,
        &queue.repo,
        &queue.backend,
        &reviewer,
        &queue.options,
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.detail();
    assert_eq!(adoption_events(&detail).len(), 1);
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert_eq!(queue.removals(&run), (2, 1));
    queue.none_left();
    drop(queue.dir);
}

/// Acceptance (1) on the real machine: the arguments of a turn's container
/// run there as a non-root user mapped to the owner of the mounted
/// directories, which writes the clone, the run directory and the queue's
/// worker cache and nothing else, with no network. Needs podman, the dagq machine, a queue
/// directory it mounts (`DAGQ_WORKER_QUEUE_DIR`, under the data home) and
/// the queue's image there (`DAGQ_WORKER_IMAGE`).
#[test]
#[ignore = "needs podman, the dagq machine, DAGQ_WORKER_QUEUE_DIR and DAGQ_WORKER_IMAGE"]
fn a_turn_s_container_runs_hardened_in_the_real_machine() {
    let _waiting = crate::common::within(Duration::from_secs(600), "the real container");
    let queue_dir = PathBuf::from(std::env::var("DAGQ_WORKER_QUEUE_DIR").unwrap());
    let image = std::env::var("DAGQ_WORKER_IMAGE").unwrap();
    let queue = QueueKey::new(queue_dir.file_name().unwrap().to_str().unwrap()).unwrap();
    let run = "realworkertest";
    let clone = queue_dir.join(run_clone::CLONES_DIR).join(run);
    let run_dir = queue_dir.join("runs").join(run);
    for dir in [&clone, &run_dir] {
        fs::create_dir_all(dir).unwrap();
    }
    let cache = vec!["/dagq-test-cache".to_owned()];
    let script = format!(
        "test \"$(id -u)\" = 10001 && ! touch /probe 2>/dev/null && touch {clone} {run} \
         && touch /dagq-test-cache/probe \
         && ! dagq relay probe --socket /nonexistent 2>/dev/null && touch /tmp/probe",
        clone = shell_path(clone.join("probe")),
        run = shell_path(run_dir.join("probe")),
    );
    let command = ["sh".to_owned(), "-c".to_owned(), script];
    let env = Vec::new();
    let args = dagq::domain::run_container::worker_run_args(
        &dagq::domain::run_container::WorkerContainer {
            queue: &queue,
            run,
            image: &image,
            clone: &clone,
            run_dir: &run_dir,
            build_output: None,
            worker_cache: &cache,
            limits: ContainerLimits::default(),
            env: &env,
            unset_env: &[],
            command: &command,
        },
    )
    .unwrap();
    // No relay runs for the test: its socket volume is made empty.
    let socket = queue.socket_volume(run).unwrap();
    let machine = ["--connection", dagq::domain::podman_machine::MACHINE_NAME];
    let podman = |args: &[&str]| {
        Command::new("podman")
            .args(machine)
            .args(args)
            .bounded_status()
            .unwrap()
    };
    assert!(podman(&["volume", "create", "--ignore", &socket]).success());
    let status = Command::new("podman")
        .args(machine)
        .args(&args)
        .bounded_status()
        .unwrap();
    let _ = podman(&["volume", "rm", "--force", &socket]);
    let _ = podman(&[
        "volume",
        "rm",
        "--force",
        &queue.worker_cache_volume_of(&cache[0]),
    ]);
    let wrote = clone.join("probe").is_file() && run_dir.join("probe").is_file();
    let _ = fs::remove_dir_all(&clone);
    let _ = fs::remove_dir_all(&run_dir);
    assert!(status.success(), "{status}");
    assert!(wrote);
}

/// Acceptance (d) of task 3391 (ADR-t3163-1 decision 4): while a worker's
/// container would wait for the machine's memory, a task whose worker is
/// set to the container is not claimed, its deferral saying why; once the
/// memory fits, it is claimed, runs in its container and lands.
#[test]
fn a_claim_waits_while_the_machine_s_memory_is_short_and_goes_on_once_it_fits() {
    let _test = crate::common::test();
    let mut queue = ContainerQueue::new(None);
    queue.options.container = Some(runtime::ContainerRuntimePort(Arc::new(MemoryGated(
        queue.state.clone(),
    ))));
    fs::write(queue.state.join(MEMORY_FULL), "").unwrap();
    let outcome = supervise_reviewed_owned_with(
        &queue.db,
        &queue.repo,
        &queue.backend,
        &TestReviewer::new(&[]),
        &queue.options,
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    let detail = queue.detail();
    assert!(detail.runs.is_empty());
    let deferred = payloads(&detail, "claim_deferred");
    assert_eq!(deferred.len(), 1, "{deferred:?}");
    assert_eq!(deferred[0]["reason"], "container_memory_wait");
    let message = deferred[0]["message"].as_str().unwrap();
    assert!(
        message.contains("dagq machine's memory") && message.contains("never on the host"),
        "{message}"
    );
    assert!(memory_calls(&queue.state).is_empty());

    fs::remove_file(queue.state.join(MEMORY_FULL)).unwrap();
    let outcome = supervise_reviewed_owned_with(
        &queue.db,
        &queue.repo,
        &queue.backend,
        &TestReviewer::new(&[verdict("pass", &[], "fine")]),
        &queue.options,
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let detail = queue.detail();
    assert_eq!(
        payloads(&detail, "claim_deferral_ended")[0]["why"],
        "cleared"
    );
    // Its turn reserved its memory, and released it with its container.
    let container = format!("dagq-{QUEUE}-run-{}", detail.runs[0].id());
    let memory = memory_calls(&queue.state);
    assert_eq!(
        memory.first(),
        Some(&format!("reserve {container}")),
        "{memory:?}"
    );
    assert!(
        memory.contains(&format!("release {container}")),
        "{memory:?}"
    );
    queue.none_left();
}

/// The turns of [`a_running_run_s_next_turn_waits_for_the_machine_s_memory_outside_its_time`]:
/// the first ends without a receipt and fills the machine, the next one
/// finishes.
const TURN_THEN_FULL: &str = r#"if [ -f "$RUN_DIR/stub-turn-one" ]; then
  finish
else
  : > "$RUN_DIR/stub-turn-one"
  : > "${0%/*}/../memory-full"
  say "not done yet"
fi
"#;

/// Acceptance (e) of task 3391 (ADR-t3163-1 decision 4): the next turn of
/// a running run (here the nudge of a turn that ended without a receipt)
/// waits for the machine's memory before its container starts, without an
/// error. The wait is the run's record and shows in `status`; it is
/// outside the turn: the turn starts after it, and neither the stall's,
/// the idle processes' nor the request's watch counts it. Once the memory
/// fits, the turn runs and the run lands.
#[test]
fn a_running_run_s_next_turn_waits_for_the_machine_s_memory_outside_its_time() {
    let _test = crate::common::test();
    let queue = ContainerQueue::new(Some(TURN_THEN_FULL));
    let supervisor = queue.supervise(TestReviewer::new(&[verdict("pass", &[], "fine")]));
    wait_until(&queue.db, crate::common::STEP_LIMIT, |sqlite| {
        sqlite
            .show(TaskId::new(1))
            .unwrap()
            .events
            .iter()
            .any(|event| event.kind == EventKind::ContainerMemoryWaiting)
    });
    let status = runtime::status(&queue.db).unwrap();
    let waiting = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "container_memory_waiting")
        .cloned()
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(
        waiting["next"],
        "waiting for the dagq podman machine's memory (runtime)"
    );
    assert_eq!(waiting["last_error"], "the test's machine is full");
    // A wait past its long mark (recorded here as the wrapper records it
    // after `LONG_WAIT_SECS`) is the person's.
    let run_id = queue.detail().runs[0].id().clone();
    SqliteQueue::open(&queue.db)
        .unwrap()
        .record_runtime_event(
            &run_id,
            EventKind::ContainerMemoryWaitLong,
            json!({"purpose": "turn", "turn": 2, "why": "still full"}),
        )
        .unwrap();
    let status = runtime::status(&queue.db).unwrap();
    let long = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "container_memory_wait_long")
        .cloned()
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(long["next"], "look at the dagq podman machine's memory");
    assert_eq!(long["last_error"], "still full");
    // Held a while, the turn has not started and nothing took the wait for
    // a stall.
    thread::sleep(Duration::from_millis(200));
    assert_eq!(payloads(&queue.detail(), "turn_started").len(), 1);
    fs::remove_file(queue.state.join(MEMORY_FULL)).unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    queue.backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.detail();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Integrated, "{outcome}");
    let kinds = event_kinds(&detail);
    let at = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    let last = |kind: &str| kinds.iter().rposition(|k| *k == kind).unwrap();
    assert!(at("container_memory_waiting") < at("container_memory_wait_ended"));
    assert!(
        at("container_memory_wait_ended") < last("turn_started"),
        "{kinds:?}"
    );
    let waiting = payloads(&detail, "container_memory_waiting");
    assert_eq!(waiting.len(), 1, "{waiting:?}");
    assert_eq!(waiting[0]["purpose"], "turn");
    assert_eq!(waiting[0]["turn"], 2);
    let ended = payloads(&detail, "container_memory_wait_ended");
    assert_eq!(ended[0]["outcome"], "reserved");
    // Whole seconds of the wall clock: a wait shorter than one may read 0.
    assert!(ended[0]["waited_secs"].as_i64().unwrap() >= 0, "{ended:?}");
    // The wait was no stall nor idle: one nudge, for the first turn, no
    // recovery job and no ask.
    assert_eq!(payloads(&detail, "stall_nudged").len(), 1, "{kinds:?}");
    assert!(
        payloads(&detail, "recovery_requested").is_empty(),
        "{kinds:?}"
    );
    assert!(stalled_asks(&SqliteQueue::open(&queue.db).unwrap()).is_empty());
    let container = format!("dagq-{QUEUE}-run-{}", run.id());
    let memory = memory_calls(&queue.state);
    assert!(memory.contains(&format!("wait {container}")), "{memory:?}");
    assert_eq!(
        memory
            .iter()
            .filter(|line| **line == format!("reserve {container}"))
            .count(),
        2,
        "{memory:?}"
    );
    let status = runtime::status(&queue.db).unwrap();
    assert!(
        !status.to_string().contains("container_memory_wait"),
        "{status}"
    );
    queue.none_left();
}
