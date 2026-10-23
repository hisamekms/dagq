//! The fixture of the runtime tests of a worker in its container
//! (`run_container`, `runtime_replan_park`): a fake `podman` that runs the
//! container's command (`dagq relay turn` and a stub `claude`) on the host
//! with real git, the queue laid out as a host's, and a queue whose task 1's
//! worker `dagq.toml` sets to the container.
use super::*;
use dagq::application::podman_machine::RunMachine;
use dagq::infrastructure::podman_machine::PodmanCli;
use dagq::infrastructure::run_container::PodmanRunContainers;
use std::os::unix::fs::PermissionsExt;

pub const QUEUE: &str = "bbbbbbbbbbbbbbbb";

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
pub fn fake_podman(state: &Path) -> PathBuf {
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
    --read-only|--interactive) shift ;;
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
    // untracked file. It names the test's process as `STUB_TEST_PID`, the
    // container's environment passing none of the test's, so that a
    // `turn.sh` waiting with `await_file` stops once the test is gone,
    // its executor living on.
    let claude = state.join("bin").join("claude");
    fs::write(
        &claude,
        format!("#!/bin/sh\nSTUB_TEST_PID={}\n", std::process::id())
            + r#"SESSION=; PERMISSION=; RUN_DIR=; MODE=
while [ $# -gt 0 ]; do
  case "$1" in
    --session-id) MODE=start; SESSION=$2; shift 2 ;;
    --resume) MODE=resume; SESSION=$2; shift 2 ;;
    --permission-mode) PERMISSION=$2; shift 2 ;;
    --add-dir) RUN_DIR=$2; shift 2 ;;
    --) printf 'stub: a turn prompt in argv\n' >&2; exit 64 ;;
    *) shift ;;
  esac
done
printf '{"type":"system","subtype":"init","session_id":"%s","model":"stub","permissionMode":"%s"}\n' "$SESSION" "$PERMISSION"
# The turn's prompt comes on stdin through podman's `--interactive`; the
# first turn's is kept.
[ -f "$RUN_DIR/stub-first-prompt" ] || cat > "$RUN_DIR/stub-first-prompt"
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

pub fn calls(state: &Path) -> Vec<String> {
    fs::read_to_string(state.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A queue in `<dir>/data/dagq/<QUEUE>/`, as the queues of a host are laid
/// out, with a repository whose task 1 is ready.
pub fn queue_fixture(dir: &Path) -> (PathBuf, PathBuf) {
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

/// The machine's parts of the test's turns: none is asked, and every turn
/// runs the test's image. Each readying is written to `<state>/machine`
/// (`ready <run>` for a turn's parts with its relay, `image <run>` for the
/// machine and the image alone).
pub struct FakeMachine(PathBuf);

impl FakeMachine {
    fn note(&self, line: String) {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.0.join("machine"))
            .unwrap();
        std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes()).unwrap();
    }
}

/// What [`FakeMachine`] of `state` was asked to make ready.
pub fn machine_calls(state: &Path) -> Vec<String> {
    fs::read_to_string(state.join("machine"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

impl RunMachine for FakeMachine {
    fn ready(&self, run: &str, _: Option<&str>) -> Result<String> {
        self.note(format!("ready {run}"));
        Ok("localhost/dagq-test-image:t".to_owned())
    }

    fn ready_image(&self, run: &str) -> Result<String> {
        self.note(format!("image {run}"));
        Ok("localhost/dagq-test-image:t".to_owned())
    }

    fn remove_build_output(&self, _: &str) -> Result<()> {
        Ok(())
    }

    fn create_worker_cache(&self, _: &[String]) -> Result<()> {
        Ok(())
    }
}

pub fn containers(state: &Path, db: &Path, repo: &Path) -> PodmanRunContainers {
    PodmanRunContainers::of_queue(
        PodmanCli {
            program: fake_podman(state),
        },
        db,
        Some(repo.to_path_buf()),
        None,
        Arc::new(FakeMachine(state.to_path_buf())),
    )
}

/// A container runtime whose every part answers.
pub struct Usable;

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
/// the supervisor the same. A timeout, whose `process::exit` skips the
/// drop of `dir`, removes it ([`common::on_timeout`]), which ends a turn's
/// wait of [`common::await_file`] too.
pub struct ContainerQueue {
    pub dir: TempDir,
    pub repo: PathBuf,
    pub db: PathBuf,
    pub state: PathBuf,
    pub backend: Arc<TestWorkspace>,
    pub options: SuperviseOptions,
    pub _on_timeout: common::Cleanup,
}

impl ContainerQueue {
    pub fn new(turn: Option<&str>) -> Self {
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
        let on_timeout = {
            let path = dir.path().to_owned();
            common::on_timeout(
                Duration::from_secs(20),
                format!("remove {}", path.display()),
                move || {
                    let _ = fs::remove_dir_all(path);
                },
            )
        };
        Self {
            dir,
            repo,
            db,
            state,
            backend: Arc::new(backend),
            options,
            _on_timeout: on_timeout,
        }
    }

    /// Supervise on a thread with `reviewer` until nothing is left to do.
    pub fn supervise(&self, reviewer: TestReviewer) -> thread::JoinHandle<Result<Value>> {
        self.supervise_with(reviewer, |_| {})
    }

    /// [`Self::supervise`] with the options `change` makes of the queue's.
    /// The service the supervisor starts takes the test as its owner.
    pub fn supervise_with(
        &self,
        reviewer: TestReviewer,
        change: impl FnOnce(&mut SuperviseOptions),
    ) -> thread::JoinHandle<Result<Value>> {
        let mut options = self.options.clone();
        change(&mut options);
        let dagq = common::service::owned_executable(self.db.parent().unwrap());
        let (db, repo, backend) = (self.db.clone(), self.repo.clone(), self.backend.clone());
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &reviewer,
                &dagq,
                &options,
            )
        })
    }

    pub fn detail(&self) -> dagq::domain::TaskDetail {
        SqliteQueue::open(&self.db)
            .unwrap()
            .show(TaskId::new(1))
            .unwrap()
    }

    /// The fake podman's removals of the container of `run`, and the
    /// turns `run` took: each turn's end removes its container once, so
    /// any more removals are a stop's.
    pub fn removals(&self, run: &TaskRun) -> (usize, usize) {
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
    pub fn none_left(&self) {
        let left: Vec<_> = fs::read_dir(self.state.join("containers"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }
}
