//! The dagq podman machine through a fake `podman` (ADR-t2113-1 decisions 4
//! and 5, ADR-t2173-1): what the commands are given, the host-wide lock
//! that makes concurrent users create and start it once, and the named
//! volumes of each queue. The real `podman` is in the ignored test at the
//! end.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread;

use dagq::application::podman_machine::{MachineVolumes, ensure, release};
use dagq::domain::podman_machine::Prepare;
use dagq::infrastructure::podman_machine::{PodmanCli, QueueMachine, QueueVolumes, host_volumes};

use crate::common::{self, shell_path};

/// A `podman` that keeps its machine, default connection and volumes in
/// `state`, writes each command line to `state/calls`, and marks
/// `state/overlap` when a machine's init or start runs while another does.
/// Creating and starting the machine make its connection the default, as
/// some Podman versions do.
fn fake_podman(state: &Path) -> PathBuf {
    fs::create_dir_all(state.join("volumes")).unwrap();
    fs::write(state.join("default"), "podman-machine-default\n").unwrap();
    let s = shell_path(state);
    let script = format!(
        r#"#!/bin/sh
S={s}
printf '%s\n' "$*" >> "$S/calls"
busy() {{ mkdir "$S/busy" 2>/dev/null || echo "$1" >> "$S/overlap"; sleep 0.2; }}
case "$1 $2" in
"machine list")
  if [ -f "$S/machine" ]; then
    r=false; [ "$(cat "$S/machine")" = running ] && r=true
    printf '[{{"Name":"podman-machine-default","Running":false}},{{"Name":"dagq","Running":%s}}]' "$r"
  else
    printf '[{{"Name":"podman-machine-default","Running":false}}]'
  fi ;;
"machine inspect")
  printf '[{{"ConfigDir":{{"Path":"%s"}},"Name":"dagq"}}]' "$S" ;;
"machine init")
  busy init
  mounts=""; next=""
  for arg in "$@"; do
    if [ "$next" = 1 ]; then
      [ -n "$mounts" ] && mounts="$mounts,"
      mounts="$mounts{{\"Source\":\"${{arg%%:*}}\"}}"
    fi
    next=""; [ "$arg" = --volume ] && next=1
  done
  printf '{{"Mounts":[%s]}}' "$mounts" > "$S/dagq.json"
  echo stopped > "$S/machine"; echo dagq > "$S/default"
  rmdir "$S/busy" ;;
"machine start")
  busy start
  echo running > "$S/machine"; echo dagq > "$S/default"
  rmdir "$S/busy" ;;
"machine stop") echo stopped > "$S/machine" ;;
"system connection")
  case "$3" in
  list) printf '[{{"Name":"%s","Default":true}}]' "$(cat "$S/default")" ;;
  default) echo "$4" > "$S/default" ;;
  esac ;;
"--connection dagq")
  shift 3
  case "$1" in
  exists) [ -f "$S/volumes/$2" ] ;;
  create) eval "name=\${{$#}}"; touch "$S/volumes/$name" ;;
  rm) rm "$S/volumes/$2" ;;
  esac ;;
*) exit 125 ;;
esac
"#
    );
    let program = state.join("podman");
    fs::write(&program, script).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    program
}

fn calls(state: &Path) -> Vec<String> {
    fs::read_to_string(state.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A data root with the queues `aaaa…` and `bbbb…` and a fake `podman`.
struct Host {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    podman: PodmanCli,
}

const QUEUE_A: &str = "aaaaaaaaaaaaaaaa";
const QUEUE_B: &str = "bbbbbbbbbbbbbbbb";

impl Host {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data").join("dagq");
        for queue in [QUEUE_A, QUEUE_B] {
            fs::create_dir_all(root.join(queue)).unwrap();
            fs::write(root.join(queue).join("queue.db"), "").unwrap();
        }
        let state = dir.path().join("podman");
        let podman = PodmanCli {
            program: fake_podman(&state),
        };
        Self {
            _dir: dir,
            root,
            state,
            podman,
        }
    }

    fn machine(&self, queue: &str) -> QueueMachine {
        QueueMachine::new(
            self.podman.clone(),
            &self.root,
            &self.root.join(queue),
            None,
        )
    }

    fn volumes(&self, queue: &str) -> QueueVolumes {
        QueueVolumes::of_queue(self.podman.clone(), &self.root.join(queue)).unwrap()
    }
}

#[test]
fn concurrent_users_create_and_start_the_machine_once_and_the_last_one_stops_it() {
    // The fake podman's children and the host-wide lock have no limit
    // of their own.
    let _waiting = common::test();
    let host = Host::new();
    let users: Vec<String> = (0..4).map(|n| format!("run-{n}")).collect();
    let joins = common::within(common::STEP_LIMIT, "the users' ensure to return");
    let ensured: Vec<Result<Prepare, String>> = thread::scope(|scope| {
        let handles: Vec<_> = users
            .iter()
            .enumerate()
            .map(|(n, user)| {
                let machine = host.machine(if n % 2 == 0 { QUEUE_A } else { QUEUE_B });
                scope.spawn(move || ensure(&machine.machine(), user))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    drop(joins);
    // One created it and started it; the others found it running.
    let mut steps: Vec<Prepare> = ensured.into_iter().map(Result::unwrap).collect();
    steps.sort_by_key(|step| *step as u8);
    assert_eq!(
        steps,
        [
            Prepare::Ready,
            Prepare::Ready,
            Prepare::Ready,
            Prepare::Init
        ]
    );
    let all = calls(&host.state);
    let count = |prefix: &str| all.iter().filter(|c| c.starts_with(prefix)).count();
    assert_eq!(count("machine init"), 1, "{all:#?}");
    assert_eq!(count("machine start"), 1, "{all:#?}");
    assert!(
        !host.state.join("overlap").exists(),
        "init or start ran beside another: {all:#?}"
    );

    // Only the last user to leave stops it.
    let machine = host.machine(QUEUE_A);
    for (n, user) in users.iter().enumerate() {
        let stopped = release(&machine.machine(), user).unwrap();
        assert_eq!(stopped, n == users.len() - 1, "{user}");
    }
    assert_eq!(count_in(&host.state, "machine stop"), 1);
    assert_eq!(
        fs::read_to_string(host.state.join("machine")).unwrap(),
        "stopped\n"
    );
}

fn count_in(state: &Path, prefix: &str) -> usize {
    calls(state)
        .iter()
        .filter(|c| c.starts_with(prefix))
        .count()
}

#[test]
fn every_command_names_the_dagq_machine_and_the_default_connection_is_kept() {
    // The fake podman's children and the host-wide lock have no limit
    // of their own.
    let _waiting = common::test();
    let host = Host::new();
    let machine = host.machine(QUEUE_A);
    ensure(&machine.machine(), "r1").unwrap();
    let volumes = host.volumes(QUEUE_A);
    volumes.create_build_output("r1").unwrap();
    volumes.remove_build_output("r1").unwrap();
    release(&machine.machine(), "r1").unwrap();

    let all = calls(&host.state);
    for call in &all {
        let names_ours = (call.starts_with("machine ") && call.ends_with(" dagq"))
            || call.starts_with("--connection dagq volume ");
        let reads_only = matches!(
            call.as_str(),
            "machine list --format json" | "system connection list --format json"
        );
        // Setting the person's default back is the one change of a
        // connection.
        let restores = call == "system connection default podman-machine-default";
        assert!(names_ours || reads_only || restores, "{call}: {all:#?}");
    }
    assert!(all.iter().any(|c| c.starts_with("machine init")));
    assert!(all.iter().any(|c| c == "machine stop dagq"));
    assert_eq!(
        fs::read_to_string(host.state.join("default")).unwrap(),
        "podman-machine-default\n"
    );
}

#[test]
fn init_mounts_only_the_run_clone_and_verification_dirs_and_a_mismatch_is_unusable() {
    // The fake podman's children and the host-wide lock have no limit
    // of their own.
    let _waiting = common::test();
    let host = Host::new();
    ensure(&host.machine(QUEUE_A).machine(), "r1").unwrap();
    let all = calls(&host.state);
    let init = all.iter().find(|c| c.starts_with("machine init")).unwrap();
    let mounted: Vec<&str> = init
        .split(' ')
        .collect::<Vec<_>>()
        .windows(2)
        .filter(|pair| pair[0] == "--volume")
        .map(|pair| pair[1])
        .collect();
    let expected: Vec<String> = host_volumes(&host.root, &host.root.join(QUEUE_A))
        .iter()
        .map(|dir| format!("{0}:{0}", dir.display()))
        .collect();
    assert_eq!(mounted, expected, "{init}");
    // Each is a run, clone or verification directory of a queue: not
    // $HOME, the worker's cache, the build output or a landing worktree.
    for volume in &mounted {
        let dir = Path::new(volume.split(':').next().unwrap());
        let queue = dir.parent().unwrap();
        assert!(
            [QUEUE_A, QUEUE_B]
                .iter()
                .any(|q| queue == host.root.join(q)),
            "{volume}"
        );
        assert!(
            ["runs", "clones", "verification"]
                .iter()
                .any(|name| dir.ends_with(name)),
            "{volume}"
        );
    }
    assert!(
        init.contains("--cpus 2 --memory 4096 --disk-size 60"),
        "{init}"
    );

    // A machine created with $HOME does not serve the queue: it is not
    // used, and the answer says how to create it again.
    release(&host.machine(QUEUE_A).machine(), "r1").unwrap();
    fs::write(
        host.state.join("dagq.json"),
        r#"{"Mounts":[{"Source":"/Users/someone"}]}"#,
    )
    .unwrap();
    let why = ensure(&host.machine(QUEUE_A).machine(), "r2").unwrap_err();
    assert!(why.contains("/Users/someone"), "{why}");
    assert!(why.contains("podman machine rm dagq"), "{why}");
    assert_eq!(count_in(&host.state, "machine start"), 1);
}

#[test]
fn volumes_are_named_per_queue_and_run_and_one_queue_never_removes_another_s() {
    // The fake podman's children and the host-wide lock have no limit
    // of their own.
    let _waiting = common::test();
    let host = Host::new();
    let (a, b) = (host.volumes(QUEUE_A), host.volumes(QUEUE_B));
    for volumes in [&a, &b] {
        volumes.create_build_output("r1").unwrap();
        volumes.create_build_output("r1").unwrap();
        volumes.create_worker_cache().unwrap();
    }
    assert!(a.build_output_exists("r1").unwrap());
    assert!(!a.build_output_exists("r2").unwrap());

    a.remove_build_output("r1").unwrap();
    a.remove_build_output("r1").unwrap();
    assert!(!a.build_output_exists("r1").unwrap());
    assert!(b.build_output_exists("r1").unwrap());
    assert!(a.worker_cache_exists().unwrap() && b.worker_cache_exists().unwrap());
    a.remove_worker_cache().unwrap();
    assert!(!a.worker_cache_exists().unwrap());
    assert!(b.worker_cache_exists().unwrap());

    let mut left: Vec<String> = fs::read_dir(host.state.join("volumes"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(
        left,
        [
            format!("dagq-{QUEUE_B}-build-r1"),
            format!("dagq-{QUEUE_B}-cache")
        ]
    );
    // Every removal named a volume of queue A.
    for call in calls(&host.state) {
        if let Some(name) = call.strip_prefix("--connection dagq volume rm ") {
            assert!(name.starts_with(&format!("dagq-{QUEUE_A}-")), "{call}");
        }
    }
    assert!(a.create_build_output("../r").is_err());
}

/// The real dagq machine with the real `podman` on `PATH`: it is created
/// if missing (mounting this host's queues' directories), started, given a
/// build output volume and stopped when this test leaves it. Run by hand,
/// with no other Podman machine running; not part of the gate.
#[test]
#[ignore = "runs the real podman and the host's dagq machine"]
fn the_real_dagq_machine_starts_holds_a_volume_and_stops() {
    // Creating a machine downloads its image.
    let _waiting = common::within(
        std::time::Duration::from_secs(1800),
        "the real podman machine",
    );
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/share"));
    let root = data_home.join("dagq");
    let queue = root
        .read_dir()
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|dir| dir.join("queue.db").is_file())
        .expect("a queue on this host");
    let machine = QueueMachine::new(
        PodmanCli::default(),
        &root,
        &queue,
        dagq::infrastructure::kpi_config::host_wide_file().as_deref(),
    );
    let user = format!("it-{}", std::process::id());
    ensure(&machine.machine(), &user).unwrap();
    let volumes = QueueVolumes::of_queue(PodmanCli::default(), &queue).unwrap();
    volumes.create_build_output(&user).unwrap();
    assert!(volumes.build_output_exists(&user).unwrap());
    volumes.remove_build_output(&user).unwrap();
    assert!(!volumes.build_output_exists(&user).unwrap());
    release(&machine.machine(), &user).unwrap();
}
