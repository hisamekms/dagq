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
/// last; its volumes are files of `state/volumes`. Its `container inspect`
/// prints what its `run` wrote for the container in `state/containers`
/// (an ID, `true`, its `--env` variables and its `--volume` mounts, a
/// line each, as the runtime's inspect format asks); its `container
/// exists` says a container is there once when `state/exists-gone` is
/// (taking that file away), as if one started and was gone again; and its
/// `exec` of a container's ID writes its arguments to `state/exec-args`
/// and runs the command on the host as `run` does, with the container's
/// variables. Its `run` hangs ([`HANG_WHILE_PARENT`]), its container
/// kept, while `state/run-hangs` is there. Under coverage it also
/// gives the command the test's `LLVM_PROFILE_FILE`, which the runtime
/// keeps out of the container's env: else the instrumented `dagq` writes
/// its profile into the run's clone and the clone is not clean to land.
/// A stub's command that does not end while the test lives: it waits a
/// tenth of a second at a time while its parent (the test's process, whose
/// runtime started it) is there, for two minutes at most, so a test that
/// ends however it ends (a wait's timeout, a panic, a kill) leaves no
/// process of it behind. A runtime that kills it leaves at most the last
/// tenth of a second's `sleep`.
pub const HANG_WHILE_PARENT: &str = r#"i=0; while [ "$i" -lt 1200 ] && kill -0 "$PPID" 2>/dev/null; do sleep 0.1; i=$((i + 1)); done; exit 125"#;

pub fn fake_podman(state: &Path) -> PathBuf {
    for dir in ["containers", "volumes", "bin"] {
        fs::create_dir_all(state.join(dir)).unwrap();
    }
    let s = shell_path(state);
    let hang = HANG_WHILE_PARENT;
    let profile = std::env::var("LLVM_PROFILE_FILE")
        .map(|file| format!("export LLVM_PROFILE_FILE={}", shell_path(file)))
        .unwrap_or_default();
    let script = format!(
        r#"#!/bin/sh
S={s}
printf '%s\n' "$*" >> "$S/calls"
[ "$1 $2" = "--connection dagq" ] || exit 125
shift 2
# A value as the text of a JSON string.
json_text() {{ printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }}
# A container is given only its own variables: none of sccache's the
# test's process may have (a worker's `[run.env]`).
unset RUSTC_WRAPPER SCCACHE_ERROR_LOG SCCACHE_DIR SCCACHE_SERVER_PORT SCCACHE_IDLE_TIMEOUT DAGQ_SCCACHE_PROGRAM DAGQ_SCCACHE_GATE DAGQ_SCCACHE_TURN
case "$1" in
rm) eval "name=\${{$#}}"; rm -f "$S/containers/$name" ;;
volume)
  case "$2" in
  create) eval "name=\${{$#}}"; touch "$S/volumes/$name" ;;
  exists) [ -f "$S/volumes/$3" ] ;;
  rm) eval "name=\${{$#}}"; rm "$S/volumes/$name" ;;
  esac ;;
container)
  # A machine the test took down answers nothing of its containers.
  [ -f "$S/machine-down" ] && {{ echo "Error: unable to connect to Podman socket" >&2; exit 125; }}
  eval "name=\${{$#}}"
  case "$2" in
  inspect) if [ -f "$S/containers/$name" ]; then cat "$S/containers/$name"; else echo "Error: no such container $name" >&2; exit 125; fi ;;
  # A container the test has start and go between this exists and the
  # inspect after it: there once, and gone again.
  exists) if [ -f "$S/exists-gone" ]; then rm -f "$S/exists-gone"; exit 0; fi
    [ -f "$S/containers/$name" ] || exit 1 ;;
  *) exit 125 ;;
  esac ;;
exec)
  shift
  printf '%s\n' "$@" > "$S/exec-args"
  # The container's own variables, as a real exec inherits them: the
  # image's, less those it was started without, then its own.
  [ -f "$S/image-env" ] && while IFS= read -r pair; do export "$pair"; done < "$S/image-env"
  prev=
  [ -f "$S/run-args" ] && while IFS= read -r arg; do
    [ "$prev" = --unsetenv ] && unset "$arg"
    [ "$prev" = --env ] && export "$arg"
    prev=$arg
  done < "$S/run-args"
  while [ $# -gt 0 ]; do
    case "$1" in
    --env) export "$2"; shift 2 ;;
    --*) shift 2 ;;
    *) break ;;
    esac
  done
  # By its ID: the container whose line starts with it.
  found=
  for file in "$S"/containers/*; do
    [ -f "$file" ] && [ "$(head -n 1 < "$file")" = "$1" ] && found=$file
  done
  shift
  [ -n "$found" ] || exit 125
  PATH="$S/bin:$PATH" "$@" ;;
run)
  shift
  printf '%s\n' "$@" > "$S/run-args"
  workdir=; name=; keep=1; envs=; mounts=
  # What the image sets (`ENV`), which `--unsetenv` drops.
  [ -f "$S/image-env" ] && while IFS= read -r pair; do export "$pair"; done < "$S/image-env"
  while [ $# -gt 0 ]; do
    case "$1" in
    --rm) keep=; shift ;;
    --read-only|--interactive) shift ;;
    --unsetenv) unset "$2"; shift 2 ;;
    --env) export "$2"; envs="$envs${{envs:+,}}\"$(json_text "$2")\""; shift 2 ;;
    --volume)
      source=${{2%%:*}}; destination=${{2#*:}}
      case "$source" in
      /*) mount="{{\"Type\":\"bind\",\"Source\":\"$source\",\"Destination\":\"$destination\"}}" ;;
      *) mount="{{\"Type\":\"volume\",\"Name\":\"$source\",\"Destination\":\"$destination\"}}" ;;
      esac
      mounts="$mounts${{mounts:+,}}$mount"; shift 2 ;;
    --workdir) workdir=$2; shift 2 ;;
    --name) name=$2; shift 2 ;;
    --*) shift 2 ;;
    *) break ;;
    esac
  done
  shift
  printf 'id%s\ntrue\n[%s]\n[%s]\n' "$$" "$envs" "$mounts" > "$S/containers/$name"
  # A container whose command does not end while `state/run-hangs` is there.
  [ -f "$S/run-hangs" ] && {{ {hang}; }}
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
  echo "wrapper=${RUSTC_WRAPPER-unset}"
  echo "error_log=${SCCACHE_ERROR_LOG-unset}"
  echo "sccache_program=${DAGQ_SCCACHE_PROGRAM-unset}"
  echo "port=${SCCACHE_SERVER_PORT-unset}"
  echo "server=$(head -n 1 "${0%/*}/../sccache-started" 2>/dev/null)"
  [ "${RUSTC_WRAPPER##*/}" = dagq-rustc-wrapper ] && echo "guarded=$("$RUSTC_WRAPPER" /bin/echo compiled 2>&1)"
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

    /// The machine's memory is short while `<state>/memory-full` is there,
    /// and, while `<state>/memory-ordered` ([`MEMORY_ORDERED`]) is, for an
    /// ask behind the standing wait of another container; each try and
    /// release is written to `<state>/memory`.
    fn reserve(
        &self,
        ask: &dagq::domain::machine_memory::MemoryAsk,
        _: i64,
    ) -> Result<dagq::domain::machine_memory::MemoryStep> {
        use dagq::domain::machine_memory::MemoryStep;
        let behind = self.0.join(MEMORY_ORDERED).exists()
            && standing_waits(&self.0)
                .iter()
                .any(|waiting| *waiting != ask.container);
        let full = self.0.join(MEMORY_FULL).exists() || behind;
        self.memory_note(format!(
            "{} {}",
            if full { "wait" } else { "reserve" },
            ask.container
        ));
        Ok(if full {
            MemoryStep::Wait("the test's machine is full".to_owned())
        } else {
            MemoryStep::Reserved
        })
    }

    fn release_memory(&self, container: &str) -> Result<()> {
        self.memory_note(format!("release {container}"));
        Ok(())
    }

    /// Written apart from [`Self::release_memory`]: what a container's
    /// removal forgets is told from what its turn forgot itself.
    fn release_memory_if_free(&self, container: &str) -> Result<()> {
        self.memory_note(format!("release-if-free {container}"));
        Ok(())
    }
}

/// The file whose presence makes [`FakeMachine`]'s memory short, and
/// [`MemoryGated`] tell a claim to wait.
pub const MEMORY_FULL: &str = "memory-full";

/// The file whose presence makes [`FakeMachine`] keep the host-wide order
/// of the waits: an ask waits while another container's wait stands.
pub const MEMORY_ORDERED: &str = "memory-ordered";

/// The containers whose wait for [`FakeMachine`]'s memory of `state`
/// stands: their last try waited, and nothing reserved or forgot it since.
pub fn standing_waits(state: &Path) -> Vec<String> {
    let mut last: Vec<(String, String)> = Vec::new();
    for line in memory_calls(state) {
        let Some((call, container)) = line.split_once(' ') else {
            continue;
        };
        last.retain(|(seen, _)| seen != container);
        last.push((container.to_owned(), call.to_owned()));
    }
    last.into_iter()
        .filter(|(_, call)| call == "wait")
        .map(|(container, _)| container)
        .collect()
}

/// One try of a turn's container `container` of another run, owned by
/// this process, at [`FakeMachine`]'s memory of `state`, written to its
/// memory calls as any try is.
pub fn try_memory_of(state: &Path, container: &str) -> dagq::domain::machine_memory::MemoryStep {
    use dagq::domain::machine_memory::{MemoryAsk, Purpose};
    let ask = MemoryAsk {
        container: container.to_owned(),
        purpose: Purpose::Turn,
        memory_mib: 1024,
        owner: dagq::domain::podman_machine::LeaseOwner::Process {
            pid: std::process::id(),
        },
        pid: std::process::id(),
        since: 0,
        seq: 0,
        started: None,
    };
    FakeMachine(state.to_path_buf()).reserve(&ask, 0).unwrap()
}

impl FakeMachine {
    fn memory_note(&self, line: String) {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.0.join("memory"))
            .unwrap();
        std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes()).unwrap();
    }
}

/// What [`FakeMachine`] of `state` was asked of the machine's memory.
pub fn memory_calls(state: &Path) -> Vec<String> {
    fs::read_to_string(state.join("memory"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

pub fn containers(state: &Path, db: &Path, repo: &Path) -> PodmanRunContainers {
    let mut containers = PodmanRunContainers::of_queue(
        PodmanCli {
            program: fake_podman(state),
        },
        db,
        Some(repo.to_path_buf()),
        None,
        Arc::new(FakeMachine(state.to_path_buf())),
    );
    // A turn that waits for the memory tries again soon.
    containers.memory_poll = Duration::from_millis(20);
    containers
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

/// A container runtime whose every part answers, and whose machine's
/// memory leaves a new worker's container no room while
/// `<state>/memory-full` ([`MEMORY_FULL`]) is there.
pub struct MemoryGated(pub PathBuf);

impl dagq::application::ContainerRuntime for MemoryGated {
    fn availability(
        &self,
        _: dagq::domain::container_runtime::RuntimeTarget,
    ) -> dagq::domain::container_runtime::Availability {
        dagq::domain::container_runtime::Availability::Available
    }

    fn memory_wait(&self) -> Option<String> {
        self.0
            .join(MEMORY_FULL)
            .exists()
            .then(|| "the test's machine is full".to_owned())
    }
}

/// A queue whose task 1's worker `dagq.toml` (committed) sets to the
/// container, every part of which answers; the turns' agent runs `turn`
/// (see [`fake_podman`]) when given. The backend starts each run's wrapper
/// on a thread with the fake podman's containers, and the options give
/// the supervisor the same. Its drop ends the wrappers as [`Fixture`]'s
/// does before `dir` goes. A timeout, whose `process::exit` skips the
/// drop, removes it as [`Fixture`]'s does ([`remove_at_timeout`]), which
/// ends a turn's wait of [`common::await_file`] too, and a kill leaves it
/// to the sweep of a later fixture ([`common::cleanup::owned_temp_dir_in`]).
pub struct ContainerQueue {
    pub dir: TempDir,
    pub _owner: fs::File,
    pub repo: PathBuf,
    pub db: PathBuf,
    pub state: PathBuf,
    pub backend: Arc<TestWorkspace>,
    pub options: SuperviseOptions,
    pub _on_timeout: common::Cleanup,
}

impl Drop for ContainerQueue {
    fn drop(&mut self) {
        kill_stubs(&self.db);
        end_wrappers(&self.db, common::STEP_LIMIT);
        common::cleanup::remove_fixture_dir(self.dir.path());
    }
}

impl ContainerQueue {
    pub fn new(turn: Option<&str>) -> Self {
        Self::configured(turn, |_| {
            "[roles.worker]\nruntime = \"container\"\n".to_owned()
        })
    }

    /// [`Self::new`] with what `config` makes of the fake podman's state
    /// directory (its `bin` holds the container's programs) as its
    /// committed `dagq.toml`.
    pub fn configured(turn: Option<&str>, config: impl FnOnce(&Path) -> String) -> Self {
        let (dir, owner) = common::cleanup::owned_temp_dir();
        let (repo, db) = queue_fixture(dir.path());
        let state = dir.path().join("podman");
        fs::write(repo.join("dagq.toml"), config(&state)).unwrap();
        git(&repo, &["add", "dagq.toml"]);
        git(&repo, &["commit", "-qm", "the worker in the container"]);
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
            let (db, path) = (db.clone(), dir.path().to_owned());
            common::on_timeout(
                Duration::from_secs(20),
                format!("remove {}", path.display()),
                move || remove_at_timeout(&db, &path),
            )
        };
        Self {
            dir,
            _owner: owner,
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
