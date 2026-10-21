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
use dagq::application::podman_machine::RunMachine;
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
use dagq::infrastructure::run_container::PodmanRunContainers;
use runtime_support::headless::Stand;
use runtime_support::*;
use std::os::unix::fs::PermissionsExt;

const QUEUE: &str = "bbbbbbbbbbbbbbbb";

/// A `podman` that writes each command line to `state/calls`. Its `run`
/// writes its arguments one per line to `state/run-args`, keeps the
/// container in `state/containers` while it runs (and after, without
/// `--rm`), and runs the container's command on the host as the container
/// would: in `--workdir`, with the `--env` variables, and `dagq` and
/// `claude` found in `state/bin`. Its `rm` removes the container named
/// last; its volumes are files of `state/volumes`. Under coverage it also
/// gives the command the test's `LLVM_PROFILE_FILE`, which the runtime
/// keeps out of the container's env: else the instrumented `dagq` writes
/// its profile into the run's clone and the clone is not clean to land.
fn fake_podman(state: &Path) -> PathBuf {
    for dir in ["containers", "volumes", "bin"] {
        fs::create_dir_all(state.join(dir)).unwrap();
    }
    let s = shell_path(state);
    let profile = std::env::var("LLVM_PROFILE_FILE")
        .map(|file| format!("export LLVM_PROFILE_FILE={}", shell_path(file)))
        .unwrap_or_default();
    let script = format!(
        r#"#!/bin/sh
S={s}
printf '%s\n' "$*" >> "$S/calls"
[ "$1 $2" = "--connection dagq" ] || exit 125
shift 2
case "$1" in
rm) eval "name=\${{$#}}"; rm -f "$S/containers/$name" ;;
volume)
  case "$2" in
  create) eval "name=\${{$#}}"; touch "$S/volumes/$name" ;;
  exists) [ -f "$S/volumes/$3" ] ;;
  rm) eval "name=\${{$#}}"; rm "$S/volumes/$name" ;;
  esac ;;
run)
  shift
  printf '%s\n' "$@" > "$S/run-args"
  workdir=; name=; keep=1
  while [ $# -gt 0 ]; do
    case "$1" in
    --rm) keep=; shift ;;
    --read-only) shift ;;
    --env) export "$2"; shift 2 ;;
    --workdir) workdir=$2; shift 2 ;;
    --name) name=$2; shift 2 ;;
    --*) shift 2 ;;
    *) break ;;
    esac
  done
  shift
  touch "$S/containers/$name"
  cd "$workdir" || exit 125
  {profile}
  PATH="$S/bin:$PATH" "$@"
  code=$?
  [ -n "$keep" ] || rm -f "$S/containers/$name"
  exit $code ;;
*) exit 125 ;;
esac
"#
    );
    let program = state.join("podman");
    fs::write(&program, script).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_dagq"), state.join("bin").join("dagq")).unwrap();
    // The agent of the turn: it says how it was started, then runs
    // `state/turn.sh` when there is one (with `say`, `denied` and
    // `finish`), else `finish`es: commits in the clone, writes the
    // receipt and, when the run directory has `leave-notes`, leaves an
    // untracked file.
    let claude = state.join("bin").join("claude");
    fs::write(
        &claude,
        r#"#!/bin/sh
SESSION=; PERMISSION=; RUN_DIR=; MODE=
while [ $# -gt 0 ]; do
  case "$1" in
    --session-id) MODE=start; SESSION=$2; shift 2 ;;
    --resume) MODE=resume; SESSION=$2; shift 2 ;;
    --permission-mode) PERMISSION=$2; shift 2 ;;
    --add-dir) RUN_DIR=$2; shift 2 ;;
    --) shift; break ;;
    *) shift ;;
  esac
done
printf '{"type":"system","subtype":"init","session_id":"%s","model":"stub","permissionMode":"%s"}\n' "$SESSION" "$PERMISSION"
{
  echo "token=$ANTHROPIC_AUTH_TOKEN"
  echo "base=$ANTHROPIC_BASE_URL"
  echo "headers=$ANTHROPIC_CUSTOM_HEADERS"
  echo "proxy=$HTTPS_PROXY"
  echo "service=$DAGQ_SERVICE_URL"
  echo "credential=$DAGQ_SERVICE_CREDENTIAL_FILE"
  echo "home=$HOME"
  echo "checks=$DAGQ_LOCAL_CHECKS_DIR"
  echo "target=$CARGO_TARGET_DIR"
  echo "pwd=$(pwd)"
  echo "main=$(git rev-parse refs/remotes/dagq/main)"
} > "$RUN_DIR/stub-env"
echo "$MODE $SESSION" >> "$RUN_DIR/stub-calls"
DENIALS=
say() { printf '{"type":"assistant","message":{"model":"stub","content":[{"type":"text","text":"%s"}]}}\n' "$1"; }
denied() { DENIALS='{"tool_name":"Bash","tool_use_id":"t1","tool_input":{}},{"tool_name":"Bash","tool_use_id":"t2","tool_input":{}},{"tool_name":"Edit","tool_use_id":"t3","tool_input":{}}'; }
finish() {
  echo "change $(wc -l < "$RUN_DIR/stub-calls" | tr -d ' ')" >> change.txt
  git add change.txt
  git -c user.name=worker -c user.email=worker@example.invalid commit -q -m work
  printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"passed","evidence_or_reason":"ran"},"e2e":{"status":"not_applicable","evidence_or_reason":"stub"},"subagent_review":{"status":"not_applicable","evidence_or_reason":"stub"},"summary":"in the container"}' "$DAGQ_RUN_ID" "$(git rev-parse HEAD)" > "$RUN_DIR/receipt.json"
  [ -f "$RUN_DIR/leave-notes" ] && echo notes > notes.txt
  say "working in the container"
}
if [ -f "${0%/*}/../turn.sh" ]; then . "${0%/*}/../turn.sh"; else finish; fi
printf '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"duration_ms":5,"total_cost_usd":0.01,"session_id":"%s","result":"done","usage":{"input_tokens":7,"output_tokens":3},"permission_denials":[%s]}\n' "$SESSION" "$DENIALS"
"#,
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    program
}

fn calls(state: &Path) -> Vec<String> {
    fs::read_to_string(state.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A queue in `<dir>/data/dagq/<QUEUE>/`, as the queues of a host are laid
/// out, with a repository whose task 1 is ready.
fn queue_fixture(dir: &Path) -> (PathBuf, PathBuf) {
    let repo = dir.join("repo");
    fs::create_dir(&repo).unwrap();
    crate::common::template::repository(&repo, "fixture\n");
    let queue_dir = dir.join("data").join("dagq").join(QUEUE);
    fs::create_dir_all(&queue_dir).unwrap();
    // A path that needs quoting, as the runtime tests' queues have.
    let db = queue_dir.join("queue's data.db");
    let mut queue = crate::common::template::queue(&db);
    add_ready_task(&mut queue, "test task", &[]);
    (repo, db)
}

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

/// The machine's parts of the test's turns: none is asked, and every turn
/// runs the test's image.
struct FakeMachine;

impl RunMachine for FakeMachine {
    fn ready(&self, _: &str, _: Option<&str>) -> Result<String> {
        Ok("localhost/dagq-test-image:t".to_owned())
    }

    fn remove_build_output(&self, _: &str) -> Result<()> {
        Ok(())
    }
}

fn containers(state: &Path, db: &Path, repo: &Path) -> PodmanRunContainers {
    PodmanRunContainers::of_queue(
        PodmanCli {
            program: fake_podman(state),
        },
        db,
        Some(repo.to_path_buf()),
        None,
        Arc::new(FakeMachine),
    )
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
    assert_eq!(
        volumes,
        [
            format!("{0}:{0}", clone.display()),
            format!("{0}:{0}", run_dir.display()),
            format!("dagq-{QUEUE}-socket-{}:/run/dagq-broker:z", run.id()),
            format!("dagq-{QUEUE}-build-{}:/build", run.id()),
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
        "RUSTC_WRAPPER".to_owned(),
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

    // (2) What the agent was given inside.
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

/// A container runtime whose every part answers.
struct Usable;

impl dagq::application::ContainerRuntime for Usable {
    fn availability(
        &self,
        _: dagq::domain::container_runtime::RuntimeTarget,
    ) -> dagq::domain::container_runtime::Availability {
        dagq::domain::container_runtime::Availability::Available
    }
}

/// A queue whose task 1's worker `dagq.toml` (committed) sets to the
/// container, every part of which answers; the turns' agent runs `turn`
/// (see [`fake_podman`]) when given. The backend starts each run's wrapper
/// on a thread with the fake podman's containers, and the options give
/// the supervisor the same.
struct ContainerQueue {
    dir: TempDir,
    repo: PathBuf,
    db: PathBuf,
    state: PathBuf,
    backend: Arc<TestWorkspace>,
    options: SuperviseOptions,
}

impl ContainerQueue {
    fn new(turn: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (repo, db) = queue_fixture(dir.path());
        fs::write(
            repo.join("dagq.toml"),
            "[roles.worker]\nruntime = \"container\"\n",
        )
        .unwrap();
        git(&repo, &["add", "dagq.toml"]);
        git(&repo, &["commit", "-qm", "the worker in the container"]);
        let state = dir.path().join("podman");
        let containers = Arc::new(containers(&state, &db, &repo));
        if let Some(turn) = turn {
            fs::write(state.join("turn.sh"), turn).unwrap();
        }
        let mut backend = TestWorkspace::new(&db, false, "exit 99");
        backend.headless = Some(PathBuf::from("/nonexistent/claude-on-the-host"));
        backend.containers = Some(containers.clone());
        let mut options = supervise_options(1, true);
        options.container = Some(runtime::ContainerRuntimePort(Arc::new(Usable)));
        options.run_containers = Some(runtime::RunContainersPort(containers));
        Self {
            dir,
            repo,
            db,
            state,
            backend: Arc::new(backend),
            options,
        }
    }

    /// Supervise on a thread with `reviewer` until nothing is left to do.
    fn supervise(&self, reviewer: TestReviewer) -> thread::JoinHandle<Result<Value>> {
        let (db, repo, backend, options) = (
            self.db.clone(),
            self.repo.clone(),
            self.backend.clone(),
            self.options.clone(),
        );
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    }

    fn detail(&self) -> dagq::domain::TaskDetail {
        SqliteQueue::open(&self.db)
            .unwrap()
            .show(TaskId::new(1))
            .unwrap()
    }

    /// The fake podman's removals of the container of `run`, and the
    /// turns `run` took: each turn's end removes its container once, so
    /// any more removals are a stop's.
    fn removals(&self, run: &TaskRun) -> (usize, usize) {
        let removed = format!(
            "--connection dagq rm --force --ignore --time 0 dagq-{QUEUE}-run-{}",
            run.id()
        );
        let removals = calls(&self.state)
            .iter()
            .filter(|call| **call == removed)
            .count();
        let turns = payloads(&self.detail(), "turn_finished").len();
        (removals, turns)
    }

    /// No container of the queue is left in the fake machine.
    #[track_caller]
    fn none_left(&self) {
        let left: Vec<_> = fs::read_dir(self.state.join("containers"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }
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
    let work = &spans[0]["work"];
    assert_eq!(
        work["commands"]["build"],
        json!({"runs": 1, "failed": 0}),
        "{}",
        spans[0]
    );
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
/// directories, which writes the clone and the run directory and nothing
/// else, with no network. Needs podman, the dagq machine, a queue
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
    let script = format!(
        "test \"$(id -u)\" = 10001 && ! touch /probe 2>/dev/null && touch {clone} {run} \
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
            limits: ContainerLimits::default(),
            env: &env,
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
    let wrote = clone.join("probe").is_file() && run_dir.join("probe").is_file();
    let _ = fs::remove_dir_all(&clone);
    let _ = fs::remove_dir_all(&run_dir);
    assert!(status.success(), "{status}");
    assert!(wrote);
}
