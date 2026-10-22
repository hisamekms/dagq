//! The verification of `integrate` in the container
//! (docs/design/container-runtime.md, ADR-t2113-3 decision 1, ADR-t2114-4
//! decision 4, ADR-t2173-1, ADR-t2257-1 decision 4 (b)): a landing whose
//! verification `dagq.toml` sets to the container runs each command
//! through `podman --connection dagq run`, hardened, on a clone of the
//! rebased commit in a directory of its own under the queue's verification
//! directory, with the materials the preparation command put in the run
//! directory, the verification's token and no cache; what it left goes at
//! its end. The podman here is a fake that records its arguments and runs
//! the container's command (`dagq relay verify` of the test's binary) on
//! the host; the machine is a fake too. The directory's own checks and
//! the guarded clone are tried with real git.
use crate::runtime_support;

use dagq::application::podman_machine::RunMachine;
use dagq::application::run_container::{PreparedVerification, VerificationContainers};
use dagq::domain::podman_machine::QueueKey;
use dagq::domain::run_container::ContainerLimits;
use dagq::domain::run_container::{MATERIALS_DIR, VERIFICATION_CLONE};
use dagq::infrastructure::podman_machine::PodmanCli;
use dagq::infrastructure::run_container::PodmanVerificationContainers;
use runtime_support::*;
use std::os::unix::fs::{PermissionsExt, symlink};

const QUEUE: &str = "cccccccccccccccc";

/// A `podman` that writes each command line to `state/calls`. Its `run`
/// writes its arguments one per line to `state/run-args-<n>`, and runs the
/// container's command on the host as the container would: in
/// `--workdir`, with the `--env` variables alone (none of the test's), and
/// `dagq` found in `state/bin`; with `state/refuse-run` there, its `run`
/// starts nothing and exits as podman does when it cannot make the
/// container (125). Its `rm` removes nothing but is recorded.
fn fake_podman(state: &Path) -> PathBuf {
    fs::create_dir_all(state.join("bin")).unwrap();
    let s = shell_path(state);
    let script = format!(
        r#"#!/bin/sh
S={s}
if [ -z "${{FAKE_CLEAN:-}}" ]; then
  printf '%s\n' "$*" >> "$S/calls"
  [ "$3" = run ] && exec env -i FAKE_CLEAN=1 /bin/sh "$0" "$@"
fi
[ "$1 $2" = "--connection dagq" ] || exit 125
shift 2
case "$1" in
rm) ;;
run)
  shift
  n=$(ls "$S" | grep -c '^run-args-')
  printf '%s\n' "$@" > "$S/run-args-$((n + 1))"
  if [ -e "$S/refuse-run" ]; then
    echo "Error: the container could not be started" >&2
    exit 125
  fi
  workdir=
  while [ $# -gt 0 ]; do
    case "$1" in
    --rm|--read-only) shift ;;
    --env) export "$2"; shift 2 ;;
    --workdir) workdir=$2; shift 2 ;;
    --*) shift 2 ;;
    *) break ;;
    esac
  done
  shift
  cd "$workdir" || exit 125
  PATH="$S/bin:/usr/bin:/bin" "$@"
  exit $? ;;
*) exit 125 ;;
esac
"#
    );
    let program = state.join("podman");
    fs::write(&program, script).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(env!("CARGO_BIN_EXE_dagq"), state.join("bin").join("dagq")).unwrap();
    program
}

/// Each `podman run`'s arguments, in order.
fn runs(state: &Path) -> Vec<Vec<String>> {
    (1..)
        .map(|n| state.join(format!("run-args-{n}")))
        .take_while(|path| path.exists())
        .map(|path| {
            fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        })
        .collect()
}

fn values<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
    args.iter()
        .zip(args.iter().skip(1))
        .filter(|(f, _)| *f == flag)
        .map(|(_, value)| value.as_str())
        .collect()
}

/// The machine's parts: none is asked; what was readied and removed is
/// recorded.
#[derive(Default)]
struct FakeMachine {
    ready: Mutex<Vec<(String, Option<String>)>>,
    removed: Mutex<Vec<String>>,
}

impl RunMachine for FakeMachine {
    fn ready(&self, run: &str, build_output: Option<&str>) -> Result<String> {
        self.ready
            .lock()
            .unwrap()
            .push((run.to_owned(), build_output.map(str::to_owned)));
        Ok("localhost/dagq-test-image:t".to_owned())
    }

    fn remove_build_output(&self, build_output: &str) -> Result<()> {
        self.removed.lock().unwrap().push(build_output.to_owned());
        Ok(())
    }

    fn create_worker_cache(&self, dirs: &[String]) -> Result<()> {
        panic!("a verification made the worker cache {dirs:?} ready")
    }
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

/// A ready task whose verification commands are `commands`.
fn add_task(db: &Path, commands: &[String]) -> TaskId {
    let mut queue = SqliteQueue::open(db).unwrap();
    let task = queue
        .add(NewTask {
            title: "test task".into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: commands.to_vec(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    task.id()
}

/// A queue in `<dir>/data/dagq/<QUEUE>/`, as the queues of a host are laid
/// out, with its repository; the podman's state and the machine.
struct VerifyQueue {
    dir: TempDir,
    repo: PathBuf,
    db: PathBuf,
    state: PathBuf,
    machine: Arc<FakeMachine>,
    options: SuperviseOptions,
}

impl VerifyQueue {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        crate::common::template::repository(&repo, "fixture\n");
        let queue_dir = dir.path().join("data").join("dagq").join(QUEUE);
        fs::create_dir_all(&queue_dir).unwrap();
        // A path that needs quoting, as the runtime tests' queues have.
        let db = queue_dir.join("queue's data.db");
        drop(crate::common::template::queue(&db));
        let state = dir.path().join("podman");
        let machine = Arc::new(FakeMachine::default());
        let containers = PodmanVerificationContainers {
            podman: PodmanCli {
                program: fake_podman(&state),
            },
            db: db.clone(),
            main_checkout: Some(repo.clone()),
            host_wide: None,
            machine: machine.clone(),
        };
        let mut options = supervise_options(1, true);
        options.container = Some(runtime::ContainerRuntimePort(Arc::new(Usable)));
        options.verification_containers =
            Some(runtime::VerificationContainersPort(Arc::new(containers)));
        Self {
            dir,
            repo,
            db,
            state,
            machine,
            options,
        }
    }

    fn queue_dir(&self) -> PathBuf {
        self.db.parent().unwrap().to_path_buf()
    }

    fn set_config(&self, text: &str) {
        fs::write(self.repo.join("dagq.toml"), text).unwrap();
        git(&self.repo, &["add", "dagq.toml"]);
        git(
            &self.repo,
            &["commit", "-qm", "the verification's settings"],
        );
    }

    /// Land `task` through the supervisor, its worker on the host.
    fn land(&self, task: TaskId) -> TaskRun {
        let backend = TestWorkspace::new(&self.db, false, VALID_AGENT);
        let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
        let outcome =
            supervise_reviewed_with(&self.db, &self.repo, &backend, &reviewer, &self.options);
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        let detail = SqliteQueue::open(&self.db).unwrap().show(task).unwrap();
        let run = detail.runs.last().unwrap().clone();
        assert_eq!(run.status(), RunStatus::Integrated, "{outcome}");
        run
    }
}

/// `text` as a TOML basic string.
fn toml_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The commands of a task: the first records where it ran and what it
/// saw (the directory, the commit, the materials, the variables) and
/// leaves in its home what a module cache leaves (directories nobody may
/// write, one nobody may enter), the second is the fixture's own check.
fn commands(state: &Path) -> Vec<String> {
    let s = shell_path(state);
    vec![
        format!(
            "pwd > {s}/pwd; git rev-parse HEAD > {s}/head; \
             ls \"$DAGQ_VERIFICATION_MATERIALS\" > {s}/materials; env > {s}/env; \
             mkdir -p \"$HOME/mod/a\" \"$HOME/sealed\" && : > \"$HOME/mod/a/f\" && \
             chmod -R a-w \"$HOME/mod\" && chmod 000 \"$HOME/sealed\""
        ),
        "test -f seed.txt".to_owned(),
    ]
}

fn seen_env(state: &Path) -> HashMap<String, String> {
    fs::read_to_string(state.join("env"))
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// Acceptance (2) to (5) through the supervisor: with the build output
/// set, then without it, each landing's commands run in containers of
/// their own, hardened, mounting only the verification's directory, the
/// materials read-only, the run's socket and (when set) the
/// verification's own build output; on a clone of the rebased commit; with
/// the preparation command's materials (only when it is declared), the
/// verification's token in the proxies alone and no `RUSTC_WRAPPER`,
/// whose removal is recorded. Two runs' verifications are made one after
/// the other in the same verification directory, and each leaves nothing.
#[test]
fn a_verification_set_to_the_container_runs_its_commands_there_and_leaves_nothing() {
    let _test = crate::common::test();
    let queue = VerifyQueue::new();
    fs::create_dir(queue.dir.path().join("bin")).unwrap();
    let sccache = queue.dir.path().join("bin").join("sccache");
    crate::common::template::script(&sccache, "#!/bin/sh\nexit 0\n");
    // A file and a directory of the host that a worker, writing its run
    // directory while the preparation runs, points the run's materials at.
    let victim = queue.dir.path().join("victim");
    fs::create_dir(&victim).unwrap();
    let victim_file = queue.dir.path().join("victim-file");
    fs::write(&victim_file, "host").unwrap();
    symlink(&victim_file, victim.join("it.json")).unwrap();
    // The preparation command, outside the repository: it acts as that
    // worker first, then writes the table where it is told.
    let prepare = queue.dir.path().join("prepare.sh");
    fs::write(
        &prepare,
        format!(
            "for run in {runs}/*/; do rm -rf \"$run/verification-materials\"; \
             ln -s {victim} \"$run/verification-materials\"; done\n\
             echo table > \"$DAGQ_VERIFICATION_MATERIALS/it.json\"\npwd > {}\n",
            shell_path(queue.state.join("prepared-in")),
            runs = shell_path(queue.queue_dir().join("runs")),
            victim = shell_path(&victim),
        ),
    )
    .unwrap();
    queue.set_config(&format!(
        "[roles.verification]\nruntime = \"container\"\n\
         [container.build_output]\ndir = \"/build\"\n\
         [container.worker_cache]\ndirs = [\"/cache/registry\"]\n\
         [container.verification]\nprepare = {}\n\
         [run.env]\nRUSTC_WRAPPER = {}\nCARGO_BUILD_JOBS = '2'\n",
        toml_string(&format!("sh {}", shell_path(&prepare))),
        toml_string(&sccache.display().to_string())
    ));
    let task = add_task(&queue.db, &commands(&queue.state));
    let run = queue.land(task);
    let queue_dir = queue.queue_dir();
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let materials = run_dir.join(MATERIALS_DIR);

    // One container per command, hardened.
    let all = runs(&queue.state);
    assert_eq!(all.len(), 2, "{all:?}");
    let args = &all[0];
    let text = args.join(" ");
    let id = values(args, "--name")[0]
        .strip_prefix(&format!("dagq-{QUEUE}-verify-"))
        .unwrap_or_else(|| panic!("{text}"))
        .to_owned();
    assert!(id.starts_with(run.id().as_str()), "{id}");
    let dir = queue_dir.join("verification").join(&id);
    for part in [
        "--user 10001:10001",
        "--read-only",
        "--cap-drop all",
        "--security-opt no-new-privileges",
        "--memory 2048m --cpus 2 --pids-limit 2048",
        "--network none",
        &format!("--workdir {}", dir.join(VERIFICATION_CLONE).display()),
        &format!(
            "localhost/dagq-test-image:t dagq relay verify --token-file {} -- sh -c",
            dir.join("token").display()
        ),
    ] {
        assert!(text.contains(part), "{part}: {text}");
    }
    assert_eq!(
        values(args, "--volume"),
        [
            format!("{0}:{0}", dir.display()),
            format!("{0}:{0}:ro", materials.display()),
            format!("dagq-{QUEUE}-socket-{}:/run/dagq-broker:z", run.id()),
            format!("dagq-{QUEUE}-build-{id}:/build"),
        ]
    );
    // No cache, no other run's or the host's place, no secret of the run's.
    let home = std::env::var("HOME").unwrap();
    for absent in [
        format!("dagq-{QUEUE}-cache"),
        format!("{0}:{0}", run_dir.display()),
        run.worktree_path().unwrap().to_owned(),
        queue_dir.join("clones").display().to_string(),
        queue_dir.join("landing").display().to_string(),
        format!("{}:", queue_dir.join("verification").display()),
        format!("{home}:"),
        "RUSTC_WRAPPER".to_owned(),
        "ANTHROPIC".to_owned(),
        "DAGQ_SERVICE".to_owned(),
    ] {
        assert!(!text.contains(&absent), "{absent}: {text}");
    }
    assert!(text.contains("CARGO_BUILD_JOBS=2"), "{text}");

    // Inside: the clone of the rebased commit, the materials the
    // preparation command put there from the main checkout, and the
    // verification's token in the proxies.
    let pwd = fs::read_to_string(queue.state.join("pwd")).unwrap();
    assert_eq!(
        pwd.trim(),
        dir.join(VERIFICATION_CLONE).display().to_string()
    );
    let head = fs::read_to_string(queue.state.join("head")).unwrap();
    assert_eq!(
        head.trim(),
        git_out(
            &queue.repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", run.id())]
        )
    );
    assert_eq!(
        fs::read_to_string(queue.state.join("materials")).unwrap(),
        "it.json\n"
    );
    // What the worker pointed the materials at is as it was: the copy in
    // the run directory is a directory of its own with a file of its own.
    assert_eq!(fs::read_to_string(&victim_file).unwrap(), "host");
    assert_eq!(fs::read_dir(&victim).unwrap().count(), 1);
    assert!(fs::symlink_metadata(&materials).unwrap().is_dir());
    let copied = fs::symlink_metadata(materials.join("it.json")).unwrap();
    assert!(copied.is_file());
    assert_eq!(
        fs::read_to_string(materials.join("it.json")).unwrap(),
        "table\n"
    );
    let prepared_in = fs::read_to_string(queue.state.join("prepared-in")).unwrap();
    assert_eq!(
        Path::new(prepared_in.trim()).canonicalize().unwrap(),
        queue.repo.canonicalize().unwrap()
    );
    let seen = seen_env(&queue.state);
    let proxy = &seen["HTTPS_PROXY"];
    let token = proxy
        .strip_prefix(&format!("http://{}:", run.id()))
        .and_then(|rest| rest.strip_suffix("@127.0.0.1:47310"))
        .unwrap_or_else(|| panic!("{proxy}"));
    assert_eq!(token.len(), 64, "{proxy}");
    assert!(!text.contains(token), "{text}");
    assert_eq!(
        seen["DAGQ_VERIFICATION_MATERIALS"],
        materials.display().to_string()
    );
    assert!(!seen.contains_key("RUSTC_WRAPPER"), "{seen:?}");
    // Revoked at the verification's end.
    assert_eq!(
        dagq::infrastructure::queue_broker::verification_run(&queue_dir, token).unwrap(),
        None
    );
    // Without a protected cache, the wrapper is taken out, and that is
    // recorded.
    let detail = SqliteQueue::open(&queue.db).unwrap().show(task).unwrap();
    let removed = payloads(&detail, "sccache_wrapper_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["by"], "integrate");
    assert_eq!(removed[0]["job"], "verification");
    assert_eq!(removed[0]["runtime"], "container");
    assert_eq!(payloads(&detail, "verification_command").len(), 2);

    // What it left goes: its containers, its build output, its directory.
    let calls = fs::read_to_string(queue.state.join("calls")).unwrap();
    assert!(
        calls.contains(&format!(
            "--connection dagq rm --force --ignore --time 0 dagq-{QUEUE}-verify-{id}"
        )),
        "{calls}"
    );
    assert_eq!(
        *queue.machine.ready.lock().unwrap(),
        [(run.id().to_string(), Some(id.clone()))]
    );
    assert_eq!(
        *queue.machine.removed.lock().unwrap(),
        std::slice::from_ref(&id)
    );
    assert!(!dir.exists());
    assert!(run_dir.join("verification-prepare.log").exists());

    // Without the build output and the preparation command: no volume is
    // made or mounted, and nothing prepares the materials. The second
    // verification is made in the same verification directory.
    queue.set_config("[roles.verification]\nruntime = \"container\"\n");
    let next = add_task(&queue.db, &commands(&queue.state));
    let second = queue.land(next);
    let all = runs(&queue.state);
    assert_eq!(all.len(), 4, "{all:?}");
    let volumes = values(&all[2], "--volume");
    assert_eq!(volumes.len(), 3, "{volumes:?}");
    assert!(
        !volumes.iter().any(|volume| volume.contains("-build-")),
        "{volumes:?}"
    );
    let ready = queue.machine.ready.lock().unwrap().clone();
    assert_eq!(ready[1], (second.id().to_string(), None));
    assert_eq!(queue.machine.removed.lock().unwrap().len(), 1);
    assert_eq!(
        fs::read_to_string(queue.state.join("materials")).unwrap(),
        ""
    );
    let second_dir = PathBuf::from(second.run_dir().unwrap());
    assert!(!second_dir.join("verification-prepare.log").exists());
    assert_eq!(
        fs::read_dir(queue_dir.join("verification"))
            .unwrap()
            .count(),
        0
    );
}

/// A script that leaves a mark named by its first argument in `marks`,
/// passing a filter's input through, so a git that runs it goes on.
fn marker(dir: &Path, marks: &Path) -> PathBuf {
    let script = dir.join("mark");
    crate::common::template::script(
        &script,
        format!(
            "#!/bin/sh\nmkdir -p {marks}\n: > {marks}/\"${{1:-unnamed}}\"\n\
             case \"$1\" in\n*-clean|*-smudge) cat ;;\n*-textconv) cat \"$2\" ;;\nesac\nexit 0\n",
            marks = shell_path(marks),
        ),
    );
    script
}

fn marks(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

/// Acceptance (5): the verification's directory is made only where
/// nothing is, under a verification directory that is the control side's
/// (which is made when missing and taken as it is when it is there); its
/// clone of a commit carrying tracked hooks and `.gitattributes` that
/// select the repository's drivers runs none of them; and its removal
/// follows no symlink.
#[test]
fn a_verification_s_directory_is_made_only_where_nothing_is_and_runs_no_tracked_hook() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    crate::common::template::repository(&repo, "fixture\n");
    let marks_dir = dir.path().join("marks");
    let mark = marker(dir.path(), &marks_dir);
    let m = |key: &str| format!("{} repo-{key}", shell_path(&mark));
    let config = repo.join(".git/config");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "[core]\n\thooksPath = .githooks\n[filter \"repo\"]\n\tclean = {}\n\tsmudge = {}\n\tprocess = {}\n\
         [diff \"repo\"]\n\ttextconv = {}\n[merge \"repo\"]\n\tdriver = {}\n",
        m("clean"),
        m("smudge"),
        m("process"),
        m("textconv"),
        m("driver"),
    ));
    fs::write(&config, text).unwrap();
    // The worker's commit: every hook calls a tracked script, and
    // `.gitattributes` selects the repository's drivers.
    let unfiltered = [
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "filter.repo.clean=",
        "-c",
        "filter.repo.smudge=",
        "-c",
        "filter.repo.process=",
    ];
    fs::create_dir_all(repo.join(".githooks")).unwrap();
    fs::create_dir_all(repo.join("scripts")).unwrap();
    fs::write(
        repo.join("scripts/hook.sh"),
        format!("#!/bin/sh\n{} tracked-$1\n", shell_path(&mark)),
    )
    .unwrap();
    for hook in [
        "post-checkout",
        "post-merge",
        "reference-transaction",
        "post-commit",
    ] {
        let path = repo.join(".githooks").join(hook);
        fs::write(
            &path,
            format!("#!/bin/sh\nsh \"$(git rev-parse --show-toplevel)/scripts/hook.sh\" {hook}\n"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(
        repo.join(".gitattributes"),
        "*.r filter=repo diff=repo merge=repo\n",
    )
    .unwrap();
    fs::write(repo.join("a.r"), "repo\n").unwrap();
    let mut add = unfiltered.to_vec();
    add.extend(["add", "--all"]);
    git(&repo, &add);
    let mut commit = unfiltered.to_vec();
    commit.extend(["commit", "-qm", "hooks and drivers"]);
    git(&repo, &commit);
    let head = CommitSha::parse(git_out(&repo, &["rev-parse", "HEAD"]), "head").unwrap();
    let repository = GitRepository::inspect(&repo).unwrap();

    let queue_dir = dir.path().join("queue");
    fs::create_dir(&queue_dir).unwrap();
    let parent = queue_dir.join("verification");
    let first = parent.join("r1-a");
    let clone = repository.create_verification_clone(&first, &head).unwrap();
    assert_eq!(clone, first.join(VERIFICATION_CLONE));
    assert_eq!(git_out(&clone, &["rev-parse", "HEAD"]), head.as_str());
    assert_eq!(fs::read_to_string(clone.join("a.r")).unwrap(), "repo\n");
    assert_eq!(marks(&marks_dir), Vec::<String>::new());
    assert_eq!(
        fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
        0o700
    );
    // The verification directory there already is taken as it is.
    let second = parent.join("r2-b");
    repository
        .create_verification_clone(&second, &head)
        .unwrap();
    assert_eq!(marks(&marks_dir), Vec::<String>::new());

    // Whatever is at the path already is refused, and left as it was.
    let outside = dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("kept"), "host").unwrap();
    fs::write(parent.join("file"), "f").unwrap();
    symlink(&outside, parent.join("link")).unwrap();
    for taken in ["r1-a", "file", "link"] {
        let error = repository
            .create_verification_clone(&parent.join(taken), &head)
            .unwrap_err();
        assert!(format!("{error:#}").contains("there already"), "{error:#}");
    }
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    // A verification directory that is a symlink, a file, or another's to
    // write is refused.
    for (name, plant) in [("linked", 0), ("plain", 1), ("open", 2)] {
        let other = dir.path().join(name);
        fs::create_dir(&other).unwrap();
        let parent = other.join("verification");
        match plant {
            0 => symlink(&outside, &parent).unwrap(),
            1 => fs::write(&parent, "").unwrap(),
            _ => {
                fs::create_dir(&parent).unwrap();
                fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
            }
        }
        assert!(
            repository
                .create_verification_clone(&parent.join("r3-c"), &head)
                .is_err(),
            "{name}"
        );
    }
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);

    // The removal follows no symlink the commands left, runs no git, and
    // takes the directories they left read-only or closed.
    symlink(&outside, clone.join("escape")).unwrap();
    fs::create_dir_all(clone.join("ro/sub")).unwrap();
    fs::write(clone.join("ro/sub/f"), "f").unwrap();
    fs::create_dir(clone.join("sealed")).unwrap();
    // A link inside a read-only directory to a read-only directory of the
    // host: neither its mode nor what is in it is touched.
    let host_ro = dir.path().join("host-ro");
    fs::create_dir(&host_ro).unwrap();
    fs::write(host_ro.join("kept"), "host").unwrap();
    symlink(&host_ro, clone.join("ro/sub/link")).unwrap();
    for path in [host_ro.as_path(), &clone.join("ro/sub"), &clone.join("ro")] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o555)).unwrap();
    }
    fs::set_permissions(clone.join("sealed"), fs::Permissions::from_mode(0o000)).unwrap();
    GitRepository::remove_verification_dir(&first).unwrap();
    assert!(!first.exists());
    assert_eq!(fs::read_to_string(outside.join("kept")).unwrap(), "host");
    assert_eq!(fs::read_to_string(host_ro.join("kept")).unwrap(), "host");
    assert_eq!(
        fs::metadata(&host_ro).unwrap().permissions().mode() & 0o777,
        0o555
    );
    fs::set_permissions(&host_ro, fs::Permissions::from_mode(0o755)).unwrap();
    // Nothing there is nothing to remove.
    GitRepository::remove_verification_dir(&first).unwrap();
    assert_eq!(marks(&marks_dir), Vec::<String>::new());
    // The marker is live: the repository's own git runs the tracked hooks.
    git(
        &repo,
        &[
            "-c",
            "filter.repo.process=",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "proof",
        ],
    );
    assert!(marks(&marks_dir).contains(&"tracked-post-commit".to_owned()));
}

/// `dagq relay verify`, the process a verification's container runs:
/// the command after `--` runs in the current directory with the proxies
/// made from the token's file and `DAGQ_RUN_ID`, and its exit code is
/// the verification's; without a token no command runs.
#[test]
fn relay_verify_runs_the_command_with_the_token_in_its_proxies_and_exits_with_its_code() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    fs::write(&token, "tok\n").unwrap();
    let seen = dir.path().join("seen");
    let verify = || {
        Command::new(env!("CARGO_BIN_EXE_dagq"))
            .without_actor_env()
            .env("DAGQ_RUN_ID", "r1")
            .arg("relay")
            .arg("verify")
            .arg("--token-file")
            .arg(&token)
            .args(["--", "sh", "-c"])
            .arg(format!(
                "printf '%s %s' \"$HTTPS_PROXY\" \"$(pwd)\" > {}; exit 3",
                shell_path(&seen)
            ))
            .current_dir(dir.path())
            .bounded_output()
            .unwrap()
    };
    let output = verify();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let text = fs::read_to_string(&seen).unwrap();
    let (proxy, pwd) = text.split_once(' ').unwrap();
    assert_eq!(proxy, "http://r1:tok@127.0.0.1:47310");
    assert_eq!(
        Path::new(pwd).canonicalize().unwrap(),
        dir.path().canonicalize().unwrap()
    );
    fs::write(&token, "").unwrap();
    fs::remove_file(&seen).unwrap();
    let output = verify();
    assert!(!output.status.success(), "{output:?}");
    assert!(!seen.exists());
}

/// Where [`verification_clone_child_process`] reads what to make.
const CLONE_FOR: &str = "DAGQ_TEST_VERIFICATION_CLONE_FOR";

/// Make the verification's clone of `commit` in `repo` at `dir` as a
/// control side whose global git configuration is `global` makes it: in
/// another process of this test binary that has that configuration
/// ([`verification_clone_child_process`]), since this one cannot change
/// its own. The child's error, if any.
fn clone_in_child(repo: &Path, dir: &Path, commit: &str, global: &Path) -> Result<(), String> {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "verification_container::verification_clone_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(
            CLONE_FOR,
            format!("{}\n{}\n{commit}", repo.display(), dir.display()),
        )
        .env("GIT_CONFIG_GLOBAL", global)
        .bounded_output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    match stdout.lines().find_map(|line| {
        line.split_once("verification-clone: ")
            .map(|(_, said)| said)
    }) {
        Some("ok") => Ok(()),
        Some(error) => Err(error.to_owned()),
        None => panic!("{stdout}{}", String::from_utf8_lossy(&output.stderr)),
    }
}

/// Not a test of its own: the other process of [`clone_in_child`], which
/// makes the clone when it is asked to and does nothing otherwise.
#[test]
fn verification_clone_child_process() {
    let Ok(asked) = std::env::var(CLONE_FOR) else {
        return;
    };
    let mut parts = asked.split('\n');
    let (repo, dir, commit) = (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    );
    let made = GitRepository::inspect(Path::new(repo))
        .unwrap()
        .create_verification_clone(Path::new(dir), &CommitSha::parse(commit, "commit").unwrap());
    match made {
        Ok(_) => println!("verification-clone: ok"),
        Err(error) => println!("verification-clone: {error:#}"),
    }
}

/// Acceptance (5): a filter driver a global configuration includes only
/// for the verification's clone (on its gitdir, on its branch), selected
/// by the worker's `.gitattributes`, does not run when the control side
/// makes the clone; the same configuration runs it for a git that is not
/// guarded.
#[test]
fn a_driver_only_the_verification_s_clone_includes_does_not_run() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    crate::common::template::repository(&repo, "fixture\n");
    let marks_dir = dir.path().join("marks");
    let mark = marker(dir.path(), &marks_dir);
    let queue_dir = dir.path().canonicalize().unwrap().join("queue");
    fs::create_dir(&queue_dir).unwrap();
    let drivers = |name: &str| {
        let m = |key: &str| format!("{} {name}-{key}", shell_path(&mark));
        format!(
            "[filter \"{name}\"]\n\tclean = {}\n\tsmudge = {}\n",
            m("clean"),
            m("smudge")
        )
    };
    let on_gitdir = dir.path().join("gitdir.gitconfig");
    fs::write(&on_gitdir, drivers("cond")).unwrap();
    let on_branch = dir.path().join("branch.gitconfig");
    fs::write(&on_branch, drivers("branch")).unwrap();
    let global = dir.path().join("global.gitconfig");
    fs::write(
        &global,
        format!(
            "[includeIf \"gitdir:{}/\"]\n\tpath = {}\n[includeIf \"onbranch:verification\"]\n\tpath = {}\n",
            queue_dir.join("verification").display(),
            on_gitdir.display(),
            on_branch.display()
        ),
    )
    .unwrap();
    // The worker's commit selects them; the test's own git has none of
    // them defined.
    fs::write(
        repo.join(".gitattributes"),
        "*.c filter=cond\n*.b filter=branch\n",
    )
    .unwrap();
    fs::write(repo.join("a.c"), "c\n").unwrap();
    fs::write(repo.join("a.b"), "b\n").unwrap();
    git(&repo, &["add", "--all"]);
    git(&repo, &["commit", "-qm", "conditional drivers"]);
    let head = git_out(&repo, &["rev-parse", "HEAD"]);

    let verification = queue_dir.join("verification").join("r1-a");
    clone_in_child(&repo, &verification, &head, &global).unwrap();
    let clone = verification.join(VERIFICATION_CLONE);
    assert_eq!(fs::read_to_string(clone.join("a.c")).unwrap(), "c\n");
    assert_eq!(fs::read_to_string(clone.join("a.b")).unwrap(), "b\n");
    assert_eq!(marks(&marks_dir), Vec::<String>::new());

    // The drivers are live in the clone: a git that is not guarded runs
    // both.
    for file in ["a.c", "a.b"] {
        fs::remove_file(clone.join(file)).unwrap();
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(&clone)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "checkout",
            "-f",
            "--",
            ".",
        ])
        .env("GIT_CONFIG_GLOBAL", &global)
        .bounded_output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(marks(&marks_dir), ["branch-smudge", "cond-smudge"]);
    GitRepository::remove_verification_dir(&verification).unwrap();
}

/// Acceptance (4), the copy of the materials: what a worker left in its
/// run directory where the materials go (a symlink in their place, a
/// directory of theirs holding symlinks to the host's file and directory)
/// is replaced, never followed, and the staged materials are copied as
/// files and directories of the run directory's own; a symlink staged is
/// not copied.
#[test]
fn the_materials_are_copied_into_the_run_directory_following_no_symlink() {
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let staged = dir.path().join("staged");
    fs::create_dir_all(staged.join("sub")).unwrap();
    fs::write(staged.join("it.json"), "table").unwrap();
    fs::write(staged.join("sub").join("known.json"), "known").unwrap();
    symlink(dir.path(), staged.join("link")).unwrap();
    let run_dir = dir.path().join("q").join("runs").join("r1");
    fs::create_dir_all(&run_dir).unwrap();
    let materials = run_dir.join(MATERIALS_DIR);
    let victim = dir.path().join("victim");
    fs::create_dir(&victim).unwrap();
    let victim_file = dir.path().join("victim-file");
    fs::write(&victim_file, "host").unwrap();
    let containers = PodmanVerificationContainers {
        podman: PodmanCli::default(),
        db: dir.path().join("q").join("queue.db"),
        main_checkout: None,
        host_wide: None,
        machine: Arc::new(FakeMachine::default()),
    };
    let verification = PreparedVerification {
        id: "r1-a".to_owned(),
        queue: QueueKey::new(QUEUE).unwrap(),
        image: "i".to_owned(),
        dir: dir.path().join("v"),
        staged: staged.clone(),
        materials: materials.clone(),
        build_output: None,
        limits: ContainerLimits::default(),
    };
    let copied = || {
        assert_eq!(fs::read_to_string(&victim_file).unwrap(), "host");
        assert_eq!(fs::read_dir(&victim).unwrap().count(), 0);
        assert!(fs::symlink_metadata(&materials).unwrap().is_dir());
        for (path, text) in [("it.json", "table"), ("sub/known.json", "known")] {
            assert!(
                fs::symlink_metadata(materials.join(path))
                    .unwrap()
                    .is_file()
            );
            assert_eq!(fs::read_to_string(materials.join(path)).unwrap(), text);
        }
        assert!(fs::symlink_metadata(materials.join("link")).is_err());
    };
    // The materials' place is a symlink to a directory of the host.
    symlink(&victim, &materials).unwrap();
    containers.publish_materials(&verification).unwrap();
    copied();
    // The materials are a directory whose entries point at the host.
    fs::remove_dir_all(&materials).unwrap();
    fs::create_dir(&materials).unwrap();
    symlink(&victim_file, materials.join("it.json")).unwrap();
    symlink(&victim, materials.join("sub")).unwrap();
    containers.publish_materials(&verification).unwrap();
    copied();
}

/// With `config` committed (its `[recheck]` and the verification's
/// runtime), task 1's run waits for a person's answer, made a run of a
/// worker in a container (its stored workplace a clone's path, as such a
/// run's is); then main moves outside dagq in another file, and the next
/// supervisor, whose container runtime is `container`, rechecks task 1's
/// run against it. Task 1's run, and the count of `podman run` before the
/// recheck.
fn rechecked_in_a_container(
    queue: &VerifyQueue,
    config: &str,
    container: Arc<dyn dagq::application::ContainerRuntime + Send + Sync>,
) -> (TaskRun, usize) {
    queue.set_config(config);
    let first = add_task(&queue.db, &[]);
    let backend = TestWorkspace::new(&queue.db, false, IDLE_AGENT);
    backend.script_for(
        1,
        "printf 'one\\n' > one.txt && git add one.txt && git commit -q -m one; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let reviewer = TestReviewer::new(&[verdict("concern", &["a person should look"], "unsure")]);
    let outcome =
        supervise_reviewed_with(&queue.db, &queue.repo, &backend, &reviewer, &queue.options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let waiting = SqliteQueue::open(&queue.db)
        .unwrap()
        .show(first)
        .unwrap()
        .runs[0]
        .clone();
    assert_eq!(waiting.status(), RunStatus::AwaitingIntegration);
    let clone = queue.queue_dir().join("clones").join(waiting.id().as_str());
    Connection::open(&queue.db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET worktree_path=?1 WHERE id=?2",
            [clone.display().to_string(), waiting.id().to_string()],
        )
        .unwrap();
    let before = runs(&queue.state).len();
    fs::write(queue.repo.join("two.txt"), "two\n").unwrap();
    git(&queue.repo, &["add", "two.txt"]);
    git(&queue.repo, &["commit", "-qm", "main moves"]);
    let mut options = queue.options.clone();
    options.container = Some(runtime::ContainerRuntimePort(container));
    let outcome = supervise_reviewed_with(&queue.db, &queue.repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let waiting = SqliteQueue::open(&queue.db)
        .unwrap()
        .show(first)
        .unwrap()
        .runs[0]
        .clone();
    assert!(waiting.in_container());
    (waiting, before)
}

/// The landing recheck's command of a run in a container runs in a
/// verification's container (ADR-t2424-1): `podman run`, hardened as
/// `integrate`'s verification, on a clone of main with the run merged in,
/// mounting no cache, the run directory or the scratch worktree, with a
/// token of its own in the proxies, no materials and no sccache wrapper
/// (whose removal is recorded); never as a process of
/// the host's in the scratch worktree. What it left and its token go, and
/// the run is found still landing.
#[test]
fn a_waiting_run_in_a_container_has_its_recheck_command_run_in_a_verification_s_container() {
    let _test = crate::common::test();
    let queue = VerifyQueue::new();
    let m = shell_path(&queue.state);
    let command = format!(
        "pwd > {m}/recheck-pwd; git log --format=%s -1 > {m}/recheck-head; \
         ls -A \"$DAGQ_VERIFICATION_MATERIALS\" > {m}/recheck-materials; env > {m}/env"
    );
    // An sccache of the test's own, named by its path: the supervisor
    // claims nothing while `[run.env]` names a program its PATH does not
    // have, and the host need not have sccache.
    let sccache = queue.dir.path().join("bin").join("sccache");
    fs::create_dir(sccache.parent().unwrap()).unwrap();
    crate::common::template::script(&sccache, "#!/bin/sh\nexit 0\n");
    let (waiting, before) = rechecked_in_a_container(
        &queue,
        &format!(
            "[roles.verification]\nruntime = \"container\"\n[recheck]\ncommand = {}\n\
             [run.env]\nRUSTC_WRAPPER = {}\n",
            toml_string(&command),
            toml_string(&sccache.display().to_string())
        ),
        Arc::new(Usable),
    );
    let queue_dir = queue.queue_dir();
    let all = runs(&queue.state);
    assert_eq!(all.len(), before + 1, "{all:?}");
    let args = all.last().unwrap();
    let text = args.join(" ");
    let id = values(args, "--name")[0]
        .strip_prefix(&format!("dagq-{QUEUE}-verify-"))
        .unwrap_or_else(|| panic!("{text}"))
        .to_owned();
    assert!(id.starts_with(waiting.id().as_str()), "{id}");
    let dir = queue_dir.join("verification").join(&id);
    for part in [
        "--read-only",
        "--cap-drop all",
        "--network none",
        &format!("--workdir {}", dir.join(VERIFICATION_CLONE).display()),
        &format!(
            "dagq relay verify --token-file {} -- sh -c {command}",
            dir.join("token").display()
        ),
    ] {
        assert!(text.contains(part), "{part}: {text}");
    }
    assert_eq!(
        values(args, "--volume"),
        [
            format!("{0}:{0}", dir.display()),
            format!("{0}:{0}:ro", dir.join("materials").display()),
            format!("dagq-{QUEUE}-socket-{}:/run/dagq-broker:z", waiting.id()),
        ]
    );
    let run_dir = PathBuf::from(waiting.run_dir().unwrap());
    for absent in [
        format!("dagq-{QUEUE}-cache"),
        format!("{0}:{0}", run_dir.display()),
        queue_dir.join("recheck").display().to_string(),
        "RUSTC_WRAPPER".to_owned(),
    ] {
        assert!(!text.contains(&absent), "{absent}: {text}");
    }
    // Inside: the clone of main with the run merged in, no materials, the
    // recheck's own token in the proxies.
    assert_eq!(
        fs::read_to_string(queue.state.join("recheck-pwd"))
            .unwrap()
            .trim(),
        dir.join(VERIFICATION_CLONE).display().to_string()
    );
    assert!(
        fs::read_to_string(queue.state.join("recheck-head"))
            .unwrap()
            .starts_with(&format!("dagq landing recheck of run {}", waiting.id()))
    );
    assert_eq!(
        fs::read_to_string(queue.state.join("recheck-materials")).unwrap(),
        ""
    );
    let seen = seen_env(&queue.state);
    let proxy = &seen["HTTPS_PROXY"];
    let token = proxy
        .strip_prefix(&format!("http://{}:", waiting.id()))
        .and_then(|rest| rest.strip_suffix("@127.0.0.1:47310"))
        .unwrap_or_else(|| panic!("{proxy}"));
    assert!(!text.contains(token), "{text}");
    assert!(!seen.contains_key("RUSTC_WRAPPER"), "{seen:?}");
    let removed = events_of(&queue.db, waiting.id(), "sccache_wrapper_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["job"], "recheck");
    assert_eq!(removed[0]["runtime"], "container");
    assert_eq!(
        dagq::infrastructure::queue_broker::verification_run(&queue_dir, token).unwrap(),
        None
    );
    // Not on the host: no scratch worktree, nothing in the run directory's
    // materials.
    assert!(!recheck_scratch(&queue_dir.join("recheck")).exists());
    assert!(!run_dir.join(MATERIALS_DIR).exists());
    // What it left goes.
    let calls = fs::read_to_string(queue.state.join("calls")).unwrap();
    assert!(
        calls.contains(&format!(
            "--connection dagq rm --force --ignore --time 0 dagq-{QUEUE}-verify-{id}"
        )),
        "{calls}"
    );
    assert!(!dir.exists());
    let clean = events_of(&queue.db, waiting.id(), "landing_recheck_clean");
    assert_eq!(clean.len(), 1, "{clean:?}");
    assert_eq!(clean[0]["command"], command.as_str());
    assert!(events_of(&queue.db, waiting.id(), "landing_recheck_failed").is_empty());
}

/// When podman starts no container for it (it exits 125, as when it
/// cannot make the container), the landing recheck's command of a run in a
/// container did not run, on the host or anywhere (ADR-t2424-1): the
/// finding judges nothing, as a command that failed on the host. The run
/// keeps waiting to land, neither resumed nor held, and the event and its
/// ask say why; what the container left and its token go. Which runtimes
/// and availabilities refuse it before any container is made is decided
/// in `supervise::recheck`'s unit test.
#[test]
fn a_waiting_run_in_a_container_whose_recheck_container_does_not_start_is_not_judged() {
    let _test = crate::common::test();
    let queue = VerifyQueue::new();
    fs::write(queue.state.join("refuse-run"), "").unwrap();
    let mark = queue.dir.path().join("ran");
    let command = format!(": > {}", shell_path(&mark));
    let (waiting, before) = rechecked_in_a_container(
        &queue,
        &format!(
            "[roles.verification]\nruntime = \"container\"\n[recheck]\ncommand = {}\n",
            toml_string(&command)
        ),
        Arc::new(Usable),
    );
    // podman was asked, and the command ran neither there nor on the host.
    let all = runs(&queue.state);
    assert_eq!(all.len(), before + 1, "{all:?}");
    assert!(all.last().unwrap().join(" ").contains(&command));
    assert!(!mark.exists());
    let queue_dir = queue.queue_dir();
    assert!(!recheck_scratch(&queue_dir.join("recheck")).exists());
    assert_eq!(
        fs::read_dir(queue_dir.join("verification"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(waiting.status(), RunStatus::AwaitingIntegration);
    assert!(events_of(&queue.db, waiting.id(), "resume_started").is_empty());
    let found = events_of(&queue.db, waiting.id(), "landing_recheck_failed");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["action"], "skipped");
    assert_eq!(found[0]["command"], command.as_str());
    let refused = found[0]["container_refused"].as_str().unwrap();
    assert!(
        refused.starts_with("podman did not start the container"),
        "{refused}"
    );
    assert!(
        found[0]["reason"]
            .as_str()
            .unwrap()
            .contains("it did not run on the host instead")
    );
    let counts = &runtime::status(&queue.db).unwrap()["landing_recheck"];
    assert_eq!(counts["environment"], 1, "{counts}");
    assert_eq!(counts["resumed"], 0);
    assert_eq!(counts["held"], 0);
    assert_eq!(counts["check_failed"], 0);
    let ask = SqliteQueue::open(&queue.db)
        .unwrap()
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.run_id.as_ref() == Some(waiting.id()))
        .unwrap();
    assert!(
        ask.question.contains("Nothing judged the code"),
        "{}",
        ask.question
    );
}

/// The agent of a task whose base already meets it: no commit, and an
/// `already_satisfied` receipt naming the base with the check it ran there.
const SATISFIED_AGENT: &str = r#"printf '{"run_id":"%s","result":"already_satisfied","commit":"%s","tests":{"status":"passed","evidence_or_reason":"test -f seed.txt on the base"},"e2e":{"status":"not_applicable","evidence_or_reason":"the runtime runs it"},"subagent_review":{"status":"not_applicable","evidence_or_reason":"no change to review"},"summary":"the base meets it","base_checks":[{"command":"test -f seed.txt","result":"passed"}]}' "$RUN_ID" "$(git rev-parse HEAD)" > "$RECEIPT.tmp"; mv "$RECEIPT.tmp" "$RECEIPT""#;

impl VerifyQueue {
    /// Supervise once, the workers on the host claiming their tasks are
    /// met already.
    fn supervise_satisfied(&self) {
        let backend = TestWorkspace::new(&self.db, false, SATISFIED_AGENT);
        let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
        let outcome =
            supervise_reviewed_with(&self.db, &self.repo, &backend, &reviewer, &self.options);
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
    }

    /// The single open ask, an `approve_close`.
    fn close_ask(&self) -> dagq::domain::Ask {
        let asks = SqliteQueue::open(&self.db)
            .unwrap()
            .asks(Default::default())
            .unwrap();
        let open: Vec<_> = asks.into_iter().filter(|a| a.closed_at.is_none()).collect();
        assert_eq!(open.len(), 1, "{open:?}");
        assert_eq!(open[0].kind, dagq::domain::AskKind::ApproveClose);
        open[0].clone()
    }
}

/// Acceptance (1) to (4) of the snapshot's verification in the container:
/// an `already_satisfied` claim's commands run on main's head in
/// containers of their own, as a landing's do (hardened, mounting only the
/// verification's directory, the materials read-only, the run's socket and
/// its own build output; on a clone of the head; the token in the proxies
/// alone, revoked at the end; no `RUSTC_WRAPPER`, whose removal is
/// recorded), its result is `run_satisfied_verified`, and the ask offers
/// the close. What an earlier one left (a dead supervisor's) is removed
/// before the next; the close answered after main moved verifies the new
/// head in the container again and closes; nothing of the verifications
/// is left.
#[test]
fn an_already_satisfied_claim_is_verified_in_the_container_and_closes() {
    let _test = crate::common::test();
    let queue = VerifyQueue::new();
    fs::create_dir(queue.dir.path().join("bin")).unwrap();
    let sccache = queue.dir.path().join("bin").join("sccache");
    crate::common::template::script(&sccache, "#!/bin/sh\nexit 0\n");
    let prepare = queue.dir.path().join("prepare.sh");
    fs::write(
        &prepare,
        "echo table > \"$DAGQ_VERIFICATION_MATERIALS/it.json\"\n",
    )
    .unwrap();
    queue.set_config(&format!(
        "[roles.verification]\nruntime = \"container\"\n\
         [container.build_output]\ndir = \"/build\"\n\
         [container.worker_cache]\ndirs = [\"/cache/registry\"]\n\
         [container.verification]\nprepare = {}\n\
         [run.env]\nRUSTC_WRAPPER = {}\n",
        toml_string(&format!("sh {}", shell_path(&prepare))),
        toml_string(&sccache.display().to_string())
    ));
    let task = add_task(&queue.db, &commands(&queue.state));
    queue.supervise_satisfied();
    let queue_dir = queue.queue_dir();
    let head = git_out(&queue.repo, &["rev-parse", "main"]);
    let detail = SqliteQueue::open(&queue.db).unwrap().show(task).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Validating);
    assert_eq!(
        payloads(&detail, "run_satisfied_verified"),
        [&json!({"head": head, "passed": true, "commands": 2})]
    );
    assert_eq!(queue.close_ask().options, ["close", "retry", "cancel"]);

    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let materials = run_dir.join(MATERIALS_DIR);
    let all = runs(&queue.state);
    assert_eq!(all.len(), 2, "{all:?}");
    let args = &all[0];
    let text = args.join(" ");
    let id = values(args, "--name")[0]
        .strip_prefix(&format!("dagq-{QUEUE}-verify-"))
        .unwrap_or_else(|| panic!("{text}"))
        .to_owned();
    assert!(id.starts_with(run.id().as_str()), "{id}");
    let dir = queue_dir.join("verification").join(&id);
    for part in [
        "--read-only",
        "--cap-drop all",
        "--security-opt no-new-privileges",
        "--network none",
        &format!("--workdir {}", dir.join(VERIFICATION_CLONE).display()),
        &format!(
            "dagq relay verify --token-file {}",
            dir.join("token").display()
        ),
    ] {
        assert!(text.contains(part), "{part}: {text}");
    }
    assert_eq!(
        values(args, "--volume"),
        [
            format!("{0}:{0}", dir.display()),
            format!("{0}:{0}:ro", materials.display()),
            format!("dagq-{QUEUE}-socket-{}:/run/dagq-broker:z", run.id()),
            format!("dagq-{QUEUE}-build-{id}:/build"),
        ]
    );
    let home = std::env::var("HOME").unwrap();
    for absent in [
        format!("dagq-{QUEUE}-cache"),
        format!("{0}:{0}", run_dir.display()),
        run.worktree_path().unwrap().to_owned(),
        format!("{home}:"),
        "RUSTC_WRAPPER".to_owned(),
    ] {
        assert!(!text.contains(&absent), "{absent}: {text}");
    }
    // Inside: the clone of main's head, the preparation's materials and
    // the snapshot's token, revoked at its end.
    assert_eq!(
        fs::read_to_string(queue.state.join("pwd")).unwrap().trim(),
        dir.join(VERIFICATION_CLONE).display().to_string()
    );
    assert_eq!(
        fs::read_to_string(queue.state.join("head")).unwrap().trim(),
        head
    );
    assert_eq!(
        fs::read_to_string(queue.state.join("materials")).unwrap(),
        "it.json\n"
    );
    let seen = seen_env(&queue.state);
    let proxy = &seen["HTTPS_PROXY"];
    let token = proxy
        .strip_prefix(&format!("http://{}:", run.id()))
        .and_then(|rest| rest.strip_suffix("@127.0.0.1:47310"))
        .unwrap_or_else(|| panic!("{proxy}"));
    assert!(!text.contains(token), "{text}");
    assert!(!seen.contains_key("RUSTC_WRAPPER"), "{seen:?}");
    assert_eq!(
        dagq::infrastructure::queue_broker::verification_run(&queue_dir, token).unwrap(),
        None
    );
    let removed = payloads(&detail, "sccache_wrapper_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["job"], "satisfied");
    assert_eq!(removed[0]["runtime"], "container");
    // What it left goes.
    assert!(!dir.exists());
    assert_eq!(
        *queue.machine.removed.lock().unwrap(),
        std::slice::from_ref(&id)
    );
    let calls = fs::read_to_string(queue.state.join("calls")).unwrap();
    assert!(
        calls.contains(&format!(
            "rm --force --ignore --time 0 dagq-{QUEUE}-verify-{id}"
        )),
        "{calls}"
    );

    // A verification a dead supervisor left, and another run's, which
    // stays; main moves, and the close is answered.
    let left = format!("{}-0123456789ab", run.id());
    let others = "00000000-0000-4000-8000-0000000000ff-0123456789ab".to_owned();
    for name in [&left, &others] {
        fs::create_dir_all(queue_dir.join("verification").join(name).join("clone")).unwrap();
    }
    git(
        &queue.repo,
        &["commit", "-q", "--allow-empty", "-m", "main moves"],
    );
    let moved = git_out(&queue.repo, &["rev-parse", "main"]);
    let ask = queue.close_ask();
    SqliteQueue::open(&queue.db)
        .unwrap()
        .answer(ask.id, "close")
        .unwrap();
    queue.supervise_satisfied();
    let detail = SqliteQueue::open(&queue.db).unwrap().show(task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let verified = payloads(&detail, "run_satisfied_verified");
    assert_eq!(verified.len(), 2);
    assert_eq!(verified[1]["head"], json!(moved));
    assert_eq!(verified[1]["passed"], true);
    assert_eq!(
        payloads(&detail, "run_satisfied_closed")[0]["rechecked"],
        true
    );
    assert_eq!(runs(&queue.state).len(), 4);
    assert_eq!(
        fs::read_to_string(queue.state.join("head")).unwrap().trim(),
        moved
    );
    let stopped: Vec<_> = payloads(&detail, "auto_repaired")
        .into_iter()
        .filter(|p| p["repair"] == "satisfied_verification_stopped")
        .collect();
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(stopped[0]["conditions"]["verifications"], json!([left]));
    let calls = fs::read_to_string(queue.state.join("calls")).unwrap();
    assert!(
        calls.contains(&format!(
            "rm --force --ignore --time 0 dagq-{QUEUE}-verify-{left}"
        )),
        "{calls}"
    );
    assert!(queue.machine.removed.lock().unwrap().contains(&left));
    let remaining: Vec<_> = fs::read_dir(queue_dir.join("verification"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(remaining, [others]);
}

/// Acceptance (2): a claim whose verification fails in the container is
/// asked about without the close, and its container leaves nothing.
#[test]
fn an_already_satisfied_claim_failing_in_the_container_is_not_offered_the_close() {
    let _test = crate::common::test();
    let queue = VerifyQueue::new();
    queue.set_config("[roles.verification]\nruntime = \"container\"\n");
    let task = add_task(&queue.db, &["test -f absent.txt".to_owned()]);
    queue.supervise_satisfied();
    let detail = SqliteQueue::open(&queue.db).unwrap().show(task).unwrap();
    let verified = payloads(&detail, "run_satisfied_verified");
    assert_eq!(verified.len(), 1);
    assert_eq!(verified[0]["passed"], false);
    assert_eq!(verified[0]["command"], "test -f absent.txt");
    assert_eq!(queue.close_ask().options, ["retry", "cancel"]);
    assert_eq!(runs(&queue.state).len(), 1);
    assert_eq!(
        fs::read_dir(queue.queue_dir().join("verification"))
            .unwrap()
            .count(),
        0
    );
}

/// When podman starts no container for a landing's verification command
/// (it exits 125, as when it cannot make the container), the command did
/// not run, in the container or on the host: integrate fails as when the
/// verification's container could not be made ready, judging nothing. No
/// `verification_command` is recorded, the later command is not started,
/// the run is neither resumed nor sent to a session and keeps waiting to
/// land, and the error says podman started no container.
#[test]
fn a_verification_whose_container_podman_does_not_start_fails_the_landing_judging_nothing() {
    let _test = crate::common::test();
    let queue = VerifyQueue::new();
    fs::write(queue.state.join("refuse-run"), "").unwrap();
    queue.set_config("[roles.verification]\nruntime = \"container\"\n");
    let mark = queue.dir.path().join("ran");
    let task = add_task(
        &queue.db,
        &[
            format!(": > {}", shell_path(&mark)),
            "test -f seed.txt".to_owned(),
        ],
    );
    let backend = TestWorkspace::new(&queue.db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
    let outcome =
        supervise_reviewed_with(&queue.db, &queue.repo, &backend, &reviewer, &queue.options);
    let errors = outcome["errors"].to_string();
    assert!(
        errors.contains("podman did not start the verification's container"),
        "{outcome}"
    );
    // podman was asked once, and the command ran neither there nor on the
    // host; the second command was not started.
    let all = runs(&queue.state);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(!mark.exists());
    let detail = SqliteQueue::open(&queue.db).unwrap().show(task).unwrap();
    let run = detail.runs.last().unwrap().clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration, "{outcome}");
    for kind in [
        "verification_command",
        "resume_started",
        "integration_deferred",
    ] {
        assert!(payloads(&detail, kind).is_empty(), "{kind}: {outcome}");
    }
    // Recorded as an error of integrate's, as a container not made ready.
    let errors = payloads(&detail, "integration_error");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0]
            .to_string()
            .contains("podman did not start the verification's container"),
        "{errors:?}"
    );
    // What the verification left goes.
    assert_eq!(
        fs::read_dir(queue.queue_dir().join("verification"))
            .unwrap()
            .count(),
        0
    );
}
