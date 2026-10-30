//! Runtime tests of a container run's facts-only turn (ADR-t2978-1): a run
//! whose facts are not of the end of its last turn has them written anew
//! by a turn with no agent before its release, its `ready --inherit` or
//! its triage reads its work, within the turn's deadline. The container is
//! faked as in `runtime_clone`.
use crate::runtime_support;

use crate::common::service::OwnedByTest;

use dagq::domain::run_clone;
use dagq::infrastructure::git_binary::git_executable;
use runtime_support::container_runs::*;
use runtime_support::*;
use std::os::unix::fs::PermissionsExt;

/// A queue laid out as a host's (its directory names the runs'
/// containers), with the fake podman's containers of
/// `runtime_support::containers`: a facts-only turn runs the container's
/// executor (`dagq relay turn --facts-only`) on the host, with real git.
fn container_queue() -> (
    tempfile::TempDir,
    PathBuf,
    PathBuf,
    dagq::infrastructure::run_container::PodmanRunContainers,
) {
    let dir = tempfile::tempdir().unwrap();
    let (repo, db) = containers::queue_fixture(dir.path());
    let fake = containers::containers(&dir.path().join("podman"), &db, &repo);
    (dir, repo, db, fake)
}

/// A turn of `run` that commits `text` to `feature.txt` and ends: its
/// executor's facts of the turn's end, as [`turn_end`] writes them.
fn ended_turn(run: &ContainerRun, text: &str) -> String {
    let head = commit(run, &[("feature.txt", text)], text);
    turn_end(run);
    head
}

/// A turn of `run` that commits `text` and is cut off with its container
/// before its executor writes the turn's end: the state an earlier turn's
/// end wrote is left marked as its start marks it (`turn_ended: false`,
/// what `relay turn` does, held by
/// `a_container_turn_marks_its_last_state_before_its_agent_runs_and_writes_it_anew_at_its_end`),
/// and none is written when there was none. The head it left.
fn cut_off_turn(run: &ContainerRun, text: &str) -> String {
    // The run token's copy goes only with a stop's removal of the
    // container: a turn cut off with it leaves the copy behind.
    fs::write(
        run.run_dir.join(dagq::domain::run_container::TOKEN_FILE),
        "run-token\n",
    )
    .unwrap();
    let state = run.run_dir.join(run_clone::RUN_STATE_FILE);
    if state.exists() {
        let marked = run_clone::RunState {
            turn_ended: Some(false),
            ..run_state(run)
        };
        fs::write(&state, serde_json::to_vec(&marked).unwrap()).unwrap();
    }
    commit(run, &[("feature.txt", text)], text)
}

/// Task 1's run in a container whose last turn committed and was cut off
/// with its container, then recovered (`interrupted`, no lease) and held:
/// with `first_ended`, a first turn's end recorded clean facts at its own
/// commit and the cut-off turn's start marked them (`turn_ended: false`);
/// else the cut-off turn was its first and no state is there. The head the
/// cut-off turn left.
fn held_cut_off_run(db: &Path, repo: &Path, first_ended: bool) -> (ContainerRun, String) {
    let run = claim_in_container(db, repo, TaskId::new(1));
    let state = run.run_dir.join(run_clone::RUN_STATE_FILE);
    if first_ended {
        let first = ended_turn(&run, "feature");
        let latest = cut_off_turn(&run, "more");
        let left = run_state(&run);
        assert_eq!(
            (left.head.as_str(), left.turn_ended),
            (first.as_str(), Some(false))
        );
        recover(db, &run);
        hold(db);
        (run, latest)
    } else {
        let latest = cut_off_turn(&run, "feature");
        assert!(!state.exists());
        recover(db, &run);
        hold(db);
        (run, latest)
    }
}

/// The run recovered as its dead session's supervisor recovers it:
/// `interrupted`, with no lease.
fn recover(db: &Path, run: &ContainerRun) {
    let mut queue = SqliteQueue::open(db).unwrap();
    queue.release_lease(run.run.id(), &run.token).unwrap();
    let recovered = queue.recover_run(run.run.id(), 0, json!({})).unwrap();
    assert_eq!(recovered.status(), RunStatus::Interrupted);
}

/// Task 1 held by a person, in this process.
fn hold(db: &Path) {
    let mut queue = SqliteQueue::open(db).unwrap();
    let actor = dagq::domain::ActorContext::user();
    dagq::application::commands::holds::Holds::new(&mut queue, &actor, &dagq::domain::StaticPolicy)
        .hold(
            &dagq::domain::task_hold::HoldTarget::Task(TaskId::new(1)),
            "the login expired",
            None,
        )
        .unwrap();
}

/// A person's `release 1 --provider codex` in this process, the run's
/// facts-only turn run in `containers`.
fn release_with(
    db: &Path,
    containers: &dyn dagq::application::run_container::RunContainers,
) -> Result<Value> {
    use dagq::domain::task_hold::{HoldTarget, ReleaseChoice};
    let mut queue = SqliteQueue::open(db).unwrap();
    let actor = dagq::domain::ActorContext::user();
    let released = dagq::application::commands::holds::Holds::new(
        &mut queue,
        &actor,
        &dagq::domain::StaticPolicy,
    )
    .release(
        &HoldTarget::Task(TaskId::new(1)),
        ReleaseChoice {
            provider: Some(dagq::domain::Provider::Codex),
            model: None,
        },
        &dagq::infrastructure::adapters::RunRepositories { containers },
    )?;
    Ok(serde_json::to_value(released).unwrap())
}

/// Whether the ref that keeps the carried head of `run` is there.
fn kept_ref(repo: &Path, run: &ContainerRun) -> Option<String> {
    let kept = format!("refs/dagq/runs/{}", run.run.id());
    Command::new(git_executable().expect("git executable"))
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "-q", &kept])
        .bounded_status()
        .unwrap()
        .success()
        .then(|| git_out(repo, &["rev-parse", &kept]))
}

/// The release of the run of [`held_cut_off_run`] carries over the head
/// a facts-only turn found in its clone, the cut-off turn's commit, not
/// an older head: the executor wrote the facts anew (`turn_ended: true`)
/// in the one facts-only turn, the head is kept, and the task is ready
/// with its hold gone.
fn released_with_the_facts_only_turn_s_head(first_ended: bool) {
    let (dir, repo, db, fake) = container_queue();
    let (run, latest) = held_cut_off_run(&db, &repo, first_ended);
    let released = release_with(&db, &fake).unwrap();
    assert_eq!(released["decision"], "retry_inherit", "{released}");
    assert_eq!(released["inherit"]["head"], json!(latest), "{released}");
    assert!(released["inherit"].get("saved").is_none(), "{released}");
    assert_eq!(kept_ref(&repo, &run), Some(latest.clone()));
    let refreshed = run_state(&run);
    assert_eq!(
        (refreshed.head.as_str(), refreshed.turn_ended),
        (latest.as_str(), Some(true))
    );
    assert_eq!(facts_turns(dir.path()), 1);
    facts_turn_was_narrow(dir.path(), &run);
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.show(run.task).unwrap().task.status(),
        TaskStatus::Ready
    );
    assert!(queue.task_hold(run.task).unwrap().is_none());
}

/// The facts-only turn of `run` (the last container the fake podman of
/// the queue in `dir` ran) had only the machine and the image made ready,
/// with no relay of the run towards the queue broker, the machine held by
/// the turn's own user around it (the run is not counted as its user);
/// mounted only the clone and the run directory, with no socket and no
/// variable; and found no run token's copy left in the run directory.
#[track_caller]
fn facts_turn_was_narrow(dir: &Path, run: &ContainerRun) {
    let state = dir.join("podman");
    let calls = containers::machine_calls(&state);
    let user = calls
        .first()
        .and_then(|call| call.strip_prefix("hold "))
        .unwrap_or_else(|| panic!("{calls:?}"))
        .to_owned();
    assert!(
        user.starts_with(&format!("facts-{}-", run.run.id())),
        "{calls:?}"
    );
    assert_eq!(
        calls,
        [
            format!("hold {user}"),
            format!("image {}", run.run.id()),
            format!("leave {user}")
        ]
    );
    let args: Vec<String> = fs::read_to_string(state.join("run-args"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let values = |flag: &str| {
        args.windows(2)
            .filter(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
            .collect::<Vec<_>>()
    };
    // The paths as the run records them, which may be the canonical ones.
    let id = run.run.id();
    let volumes = values("--volume");
    let mounted = |volume: &String, dir: &str| {
        volume
            .split_once(':')
            .is_some_and(|(from, to)| from == to && from.ends_with(&format!("/{dir}/{id}")))
    };
    assert!(
        volumes.len() == 2 && mounted(&volumes[0], "clones") && mounted(&volumes[1], "runs"),
        "{volumes:?}"
    );
    assert!(values("--env").is_empty(), "{args:?}");
    assert!(args.iter().any(|arg| arg == "--facts-only"), "{args:?}");
    assert!(
        !run.run_dir
            .join(dagq::domain::run_container::TOKEN_FILE)
            .exists()
    );
}

/// The facts-only turns the fake podman of the queue in `dir` ran.
fn facts_turns(dir: &Path) -> usize {
    containers::calls(&dir.join("podman"))
        .into_iter()
        .filter(|call| call.contains("relay turn --facts-only"))
        .count()
}

/// Acceptance (a): a held, interrupted container run whose last turn was
/// cut off with its container, its facts marked `turn_ended: false` at an
/// earlier turn's head, is released with the head of the cut-off turn.
#[test]
fn a_held_container_run_whose_last_turn_was_cut_off_is_released_with_the_head_its_facts_only_turn_found()
 {
    released_with_the_facts_only_turn_s_head(true);
}

/// Acceptance (a): so is one whose only turn was cut off before any state
/// was written.
#[test]
fn a_held_container_run_with_no_state_is_released_with_the_head_its_facts_only_turn_found() {
    released_with_the_facts_only_turn_s_head(false);
}

/// Acceptance (b): when the facts-only turn does not run (no machine, no
/// image), the release of such a run is refused with why and changes
/// nothing: the hold stays, the task keeps its worker and status, the run
/// its status, and no head is carried over or kept.
#[test]
fn a_cut_off_container_run_whose_facts_only_turn_does_not_run_is_not_released() {
    let (_dir, repo, db, _fake) = container_queue();
    let (run, _latest) = held_cut_off_run(&db, &repo, true);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let before = queue.show(run.task).unwrap();
    let why = format!("{:#}", release_with(&db, &NoContainers).unwrap_err());
    for part in [
        "the hold stays",
        "the facts-only turn that would write them anew did not run",
        "this test runs no facts-only turn",
    ] {
        assert!(why.contains(part), "{part}: {why}");
    }
    let detail = queue.show(run.task).unwrap();
    assert!(queue.task_hold(run.task).unwrap().is_some());
    assert_eq!(detail.task.worker(), before.task.worker());
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(detail.runs.len(), 1);
    assert_eq!(detail.runs[0].status(), RunStatus::Interrupted);
    assert!(payloads(&detail, "task_released").is_empty());
    assert_eq!(kept_ref(&repo, &run), None);
    assert_eq!(run_state(&run).turn_ended, Some(false));
}

/// A `podman` first on the `PATH` of the host's `dagq` of a test
/// (`<queue dir>/bin`, which [`crate::common::cli::invoke_with`] puts first)
/// whose dagq machine is made and started as Podman makes it, with the
/// volumes `machine init` is given, and in which no image can be listed:
/// a facts-only turn's machine is made ready and its image is not. Each
/// command line goes to the file returned.
fn machine_without_images(db: &Path) -> PathBuf {
    let bin = db.parent().unwrap().join("bin");
    let state = db.parent().unwrap().join("podman-machine");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&state).unwrap();
    let s = shell_path(&state);
    let script = format!(
        r#"#!/bin/sh
S={s}
printf '%s\n' "$*" >> "$S/calls"
case "$1 $2" in
"machine list")
  if [ -f "$S/dagq.json" ]; then
    r=false; [ -f "$S/running" ] && r=true
    printf '[{{"Name":"dagq","Running":%s}}]' "$r"
  else
    printf '[]'
  fi ;;
"machine inspect") printf '[{{"ConfigDir":{{"Path":"%s"}},"Name":"dagq"}}]' "$S" ;;
"machine init")
  mounts=""; next=""
  for arg in "$@"; do
    if [ "$next" = 1 ]; then
      [ -n "$mounts" ] && mounts="$mounts,"
      mounts="$mounts{{\"Source\":\"${{arg%%:*}}\"}}"
    fi
    next=""; [ "$arg" = --volume ] && next=1
  done
  printf '{{"Mounts":[%s]}}' "$mounts" > "$S/dagq.json" ;;
"machine start") touch "$S/running" ;;
"system connection") [ "$3" = list ] && printf '[{{"Name":"dagq","Default":true}}]' ;;
*) echo "Error: no image can be listed in this machine" >&2; exit 125 ;;
esac
"#
    );
    let program = bin.join("podman");
    fs::write(&program, script).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    state.join("calls")
}

/// A host's `dagq` (the inbox's here) whose release reads a held container
/// run's work through the queue service runs the run's facts-only turn
/// itself, and opens no queue for its image's records (ADR-t3102-2
/// decision 7): the machine is made ready by the client, the image's
/// failure is recorded through the service as the caller's, not as the
/// process's user it would be on the DB, and the release is refused and
/// changes nothing.
#[test]
fn a_host_release_records_its_facts_only_turn_s_image_through_the_service() {
    let (_dir, repo, db, _fake) = container_queue();
    let (run, _latest) = held_cut_off_run(&db, &repo, true);
    let calls = machine_without_images(&db);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let before = queue.show(run.task).unwrap();
    let output = crate::common::cli::invoke_with(
        &[("DAGQ_ROLE", "inbox")],
        &db,
        &["release", "1", "--provider", "codex"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("make the machine and the image ready"),
        "{stderr}"
    );
    let calls = fs::read_to_string(calls).unwrap();
    assert!(
        calls.contains("machine init") && calls.contains("machine start"),
        "{calls}"
    );
    let failed = crate::common::service::events(&db, "container_image_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["actor"]["role"], "inbox", "{failed:?}");
    let detail = queue.show(run.task).unwrap();
    assert!(queue.task_hold(run.task).unwrap().is_some());
    assert_eq!(detail.task.status(), before.task.status());
    assert_eq!(detail.runs[0].status(), RunStatus::Interrupted);
    assert!(payloads(&detail, "task_released").is_empty());
    assert_eq!(kept_ref(&repo, &run), None);
}

/// Acceptance (f): a person's `ready --inherit` of an interrupted container
/// run whose last turn was cut off carries over the head a facts-only turn
/// found, as the release does: through the entry both the carried head and
/// a release's read of the run's work go through
/// (`inherit::container_head`), whichever of them `ready --inherit` reads.
#[test]
fn ready_inherit_of_a_cut_off_container_run_carries_the_head_its_facts_only_turn_found() {
    let (_dir, repo, db, fake) = container_queue();
    let run = claim_in_container(&db, &repo, TaskId::new(1));
    ended_turn(&run, "feature");
    let latest = cut_off_turn(&run, "more");
    recover(&db, &run);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let inbox = dagq::domain::ActorContext::instance(dagq::domain::ActorRole::Inbox, 1);
    let branches = dagq::infrastructure::adapters::RunRepositories { containers: &fake };
    use dagq::application::commands::planning::Planning;
    let reason = "the turn's work is in its clone";
    let read = Planning::new(&mut queue, &inbox, &dagq::domain::StaticPolicy)
        .ready_inherit_read(TaskId::new(1), reason)
        .unwrap();
    let kept = dagq::application::inherit::keep_head(read.run.as_ref(), &branches).unwrap();
    let recorded = Planning::new(&mut queue, &inbox, &dagq::domain::StaticPolicy)
        .ready_inherit_by_caller(TaskId::new(1), reason, read.at, kept.head.clone());
    let inherited = kept.settle(&branches, recorded, |_| true).unwrap();
    assert_eq!(inherited.head.as_str(), latest);
    assert_eq!(kept_ref(&repo, &run), Some(latest.clone()));
    assert_eq!(run_state(&run).turn_ended, Some(true));
    let detail = SqliteQueue::open(&db).unwrap().show(run.task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Ready);
}

/// The inbox's client-mode `dagq` (the service's socket and its token, no
/// queue's path) carrying a cut-off container run over runs the run's
/// facts-only turn as the command line on the DB does, in the containers
/// of the queue the service answered for: it makes that queue's machine
/// and starts it, counts the turn's process and the run among the
/// machine's users (under the queue's data root), and readies the image,
/// which this machine
/// cannot list. The image's failure reaches the queue only through the
/// service, as the inbox's record, and the carry-over is refused with
/// nothing else recorded, the ref untouched and no Git of the service's.
#[test]
fn a_client_mode_ready_inherit_runs_a_cut_off_container_run_s_facts_only_turn_in_its_queue() {
    use crate::common::{Bounded, WithoutActor};
    use dagq::domain::queue_service::{CREDENTIAL_FILE_ENV, Principal, SOCKET_ENV, SessionProcess};
    use dagq::infrastructure::queue_service as service;
    let (dir, repo, db, _fake) = container_queue();
    let run = claim_in_container(&db, &repo, TaskId::new(1));
    ended_turn(&run, "feature");
    cut_off_turn(&run, "more");
    recover(&db, &run);
    let config = dir.path().join("config");
    let git_log = crate::common::service::serve_logging_git(
        &db,
        &[("XDG_CONFIG_HOME", config.to_str().unwrap())],
    );
    fs::write(&git_log, "").unwrap();
    let queue_dir = fs::canonicalize(&db).unwrap().parent().unwrap().to_owned();
    let pid = std::process::id();
    let inbox = Principal::inbox(SessionProcess {
        pid,
        start: dagq::application::ProcessControl::session_start(
            &dagq::infrastructure::adapters::SystemProcesses,
            pid,
        )
        .unwrap(),
    });
    let credential = service::issue(&queue_dir, &inbox, 1).unwrap().file;
    let calls = machine_without_images(&db);
    let path = std::env::join_paths(
        std::iter::once(db.parent().unwrap().join("bin"))
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let elsewhere = dir.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .owned_by_test()
        .current_dir(&elsewhere)
        .env("PATH", path)
        .env("XDG_CONFIG_HOME", &config)
        .env(SOCKET_ENV, service::socket_path(&queue_dir))
        .env(CREDENTIAL_FILE_ENV, credential)
        .env("DAGQ_ROLE", "inbox")
        .args([
            "ready",
            "1",
            "--inherit",
            "--reason",
            "the turn's work is in its clone",
        ])
        .bounded_output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("facts-only turn"), "{error}");
    assert!(
        error.contains("make the machine and the image ready"),
        "{error}"
    );
    assert!(!error.contains("list the podman machines"), "{error}");
    assert!(!error.contains("knows no queue directory"), "{error}");
    let calls = fs::read_to_string(calls).unwrap();
    assert!(
        calls.contains("machine init") && calls.contains("machine start"),
        "{calls}"
    );
    // The users were counted under the served queue's data root; the
    // turn's own hold left, and the run, cut off, is no longer held.
    let users = queue_dir.parent().unwrap().join("machine").join("users");
    assert!(
        users.is_dir(),
        "no user of the served queue's machine was counted"
    );
    assert!(!elsewhere.join("machine").exists());
    let failed = crate::common::service::events(&db, "container_image_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["actor"]["role"], "inbox", "{failed:?}");
    assert_eq!(kept_ref(&repo, &run), None);
    assert_eq!(
        fs::read_to_string(&git_log).unwrap(),
        "",
        "the service ran Git"
    );
    let detail = SqliteQueue::open(&db).unwrap().show(run.task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
}

/// A container run of task 1 whose first turn ended at its base and whose
/// second committed and was cut off with its container, its session gone
/// (a dead wrapper, no lease), triaged by a supervisor whose recovery job
/// repairs it with `action`. Its facts say the base with no bundle until
/// a facts-only turn writes them anew. The run's events, its task, the
/// cut-off turn's head and the queue's directory.
fn triaged_cut_off_run(
    action: &str,
) -> (
    Vec<dagq::domain::RunEvent>,
    dagq::domain::TaskDetail,
    String,
    ContainerRun,
    tempfile::TempDir,
    PathBuf,
    PathBuf,
) {
    let triaged = triage_cut_off_run(action, |_, _, fake| fake);
    let (events, _, _, run, dir, _, _) = &triaged;
    let started = events
        .iter()
        .find(|event| event.kind.as_str() == "triage_started")
        .unwrap();
    assert_eq!(started.payload["status"], "interrupted");
    assert_eq!(run_state(run).turn_ended, Some(true));
    assert_eq!(facts_turns(dir.path()), 1, "{action}");
    facts_turn_was_narrow(dir.path(), run);
    triaged
}

/// The run of [`triaged_cut_off_run`] triaged with the containers
/// `containers` makes of the queue's database, its main checkout and the
/// fake podman's containers, as they are when its supervisor's pass ends.
fn triage_cut_off_run(
    action: &str,
    containers: impl FnOnce(
        &Path,
        &Path,
        dagq::infrastructure::run_container::PodmanRunContainers,
    ) -> dagq::infrastructure::run_container::PodmanRunContainers,
) -> (
    Vec<dagq::domain::RunEvent>,
    dagq::domain::TaskDetail,
    String,
    ContainerRun,
    tempfile::TempDir,
    PathBuf,
    PathBuf,
) {
    let (dir, repo, db, fake) = container_queue();
    let fake = containers(&db, &repo, fake);
    // The next run's worker is in a container that cannot be used: the
    // task a repair readies is not claimed again.
    fs::write(
        repo.join("dagq.toml"),
        "[roles.worker]\nruntime = \"container\"\n",
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-qm", "the worker in the container"]);
    let mut run = plan_in_container(&db, &repo, TaskId::new(1));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let token = run.token.as_str().to_owned();
    let dead = dead_pid();
    let handle = dagq::domain::background_wrapper::BackgroundHandle::new(
        dead,
        &format!("gone {}", run.run.id()),
    );
    record_background_start(&db, &mut queue, &run.run, &token, &handle.to_string());
    run.reload(&db);
    GitRepository::inspect(&repo)
        .unwrap()
        .create_clone(&run.run, None)
        .unwrap();
    turn_end(&run);
    assert_eq!(run_state(&run).head, run.base);
    let latest = cut_off_turn(&run, "feature");
    assert!(!run.run_dir.join(run_clone::RUN_BUNDLE_FILE).exists());
    queue
        .register_wrapper(run.run.id(), &run.token, dead)
        .unwrap();
    queue.register_agent(run.run.id(), dead, dead).unwrap();
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases", [])
        .unwrap();
    let backend = TestWorkspace::new(&db, false, "exit 7");
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "unused")])
        .with_triages(&[repair(json!({"action": action}), "the machine restarted")]);
    let mut options = supervise_options(1, true);
    options.run_containers = Some(runtime::RunContainersPort(Arc::new(fake)));
    options.container = Some(runtime::ContainerRuntimePort(Arc::new(Unusable)));
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].id(), run.run.id());
    let events = queue.run_events(run.run.id()).unwrap();
    (events, detail, latest, run, dir, repo, db)
}

/// A container runtime that cannot be used.
struct Unusable;

impl dagq::application::ContainerRuntime for Unusable {
    fn availability(
        &self,
        _: dagq::domain::container_runtime::RuntimeTarget,
    ) -> dagq::domain::container_runtime::Availability {
        dagq::domain::container_runtime::Availability::Unavailable("not in this test".to_owned())
    }
}

/// The payloads of the events of `kind` among `events`.
fn of_kind(events: &[dagq::domain::RunEvent], kind: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| event.payload.clone())
        .collect()
}

/// Acceptance (c): the triage of a cut-off container run reads its own
/// commits from facts a facts-only turn wrote anew, so a `retry` from
/// scratch is refused (its branch holds the cut-off turn's commit) and
/// asked of a person instead.
#[test]
fn the_triage_of_a_cut_off_container_run_refuses_a_retry_from_scratch() {
    let (events, detail, _latest, _run, _dir, _repo, db) = triaged_cut_off_run("retry");
    assert_eq!(detail.runs.len(), 1);
    let finished = of_kind(&events, "triage_finished");
    assert_eq!(finished[0]["action"], "ask", "{finished:?}");
    let ask = SqliteQueue::open(&db)
        .unwrap()
        .read_ask(AskId::new(finished[0]["ask_id"].as_i64().unwrap()))
        .unwrap();
    assert!(
        ask.question.contains("its branch holds commits of its own"),
        "{}",
        ask.question
    );
}

/// Acceptance (c): its `retry_inherit` carries the cut-off turn's commit
/// over and keeps it, its own commits read from the facts written anew.
#[test]
fn the_triage_of_a_cut_off_container_run_carries_the_cut_off_turn_s_commit_over() {
    let (events, detail, latest, run, _dir, repo, _db) = triaged_cut_off_run("retry_inherit");
    let finished = of_kind(&events, "triage_finished");
    assert_eq!(finished[0]["action"], "retry_inherit", "{finished:?}");
    assert_eq!(finished[0]["inherit"]["head"], json!(latest));
    assert_eq!(
        of_kind(&events, "auto_repaired")[0]["conditions"]["own_commits"],
        true
    );
    assert_eq!(kept_ref(&repo, &run), Some(latest.clone()));
    assert_eq!(detail.task.status(), TaskStatus::Ready);
}

/// Where the readying of a facts-only turn hangs in [`stalling_podman`]:
/// the machine's start, or nowhere (the machine starts and no image is
/// made). A build that hangs is not reached here: a test's dagq is built
/// with uncommitted changes, whose image has no client to build, and the
/// lease a build cut off leaves is the unit test
/// `a_readying_cut_off_by_its_deadline_leaves_the_machine_it_joined`'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stall {
    Nothing,
    MachineStart,
}

/// How long the facts-only turns of [`stalling_containers`] may take
/// before their cleanup.
const FACTS_LIMIT: Duration = Duration::from_secs(2);

/// How much later than its deadline a facts-only turn may return in these
/// tests: what ends it after the deadline (a killed command reaped, the
/// error read back) on a loaded host. The hanging commands wait for far
/// longer, and each wait of these tests is timed with this besides
/// ([`crate::common::within`]).
const LATE: Duration = Duration::from_secs(20);

/// The host-wide directory of the dagq machine of the queue at `db`: its
/// lock and its users.
fn machine_dir(db: &Path) -> PathBuf {
    db.parent().unwrap().parent().unwrap().join("machine")
}

/// A `podman` whose dagq machine is made and started as
/// [`machine_without_images`]'s, in which no image is listed and a build
/// fails, but which hangs (far past [`FACTS_LIMIT`], while the test lives:
/// [`containers::HANG_WHILE_PARENT`]) in the machine's start when `stall`
/// says so. Each command line goes to
/// `<state>/calls`.
fn stalling_podman(state: &Path, stall: Stall) -> PathBuf {
    fs::create_dir_all(state).unwrap();
    let s = shell_path(state);
    let start = if stall == Stall::MachineStart {
        containers::HANG_WHILE_PARENT
    } else {
        r#"touch "$S/running""#
    };
    let script = format!(
        r#"#!/bin/sh
S={s}
printf '%s\n' "$*" >> "$S/calls"
case "$1 $2" in
"machine list")
  if [ -f "$S/dagq.json" ]; then
    r=false; [ -f "$S/running" ] && r=true
    printf '[{{"Name":"dagq","Running":%s}}]' "$r"
  else
    printf '[]'
  fi ;;
"machine inspect") printf '[{{"ConfigDir":{{"Path":"%s"}},"Name":"dagq"}}]' "$S" ;;
"machine init")
  mounts=""; next=""
  for arg in "$@"; do
    if [ "$next" = 1 ]; then
      [ -n "$mounts" ] && mounts="$mounts,"
      mounts="$mounts{{\"Source\":\"${{arg%%:*}}\"}}"
    fi
    next=""; [ "$arg" = --volume ] && next=1
  done
  printf '{{"Mounts":[%s]}}' "$mounts" > "$S/dagq.json" ;;
"machine start") {start} ;;
"system connection") [ "$3" = list ] && printf '[{{"Name":"dagq","Default":true}}]' ;;
"--connection dagq")
  case "$3" in
  images) printf '[]' ;;
  build) echo "Error: no build in this test" >&2; exit 125 ;;
  *) echo "Error: not in this test" >&2; exit 125 ;;
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

/// The containers of the queue at `db` bound to `repo` whose machine is
/// readied as the host's (`QueueRunMachine`) through the podman of
/// [`stalling_podman`], their facts-only turns given [`FACTS_LIMIT`]; the
/// file its command lines go to.
fn stalling_containers(
    db: &Path,
    repo: &Path,
    stall: Stall,
) -> (
    dagq::infrastructure::run_container::PodmanRunContainers,
    PathBuf,
) {
    use dagq::infrastructure::podman_machine::{PodmanCli, QueueRunMachine};
    let state = db.parent().unwrap().join("podman-stalling");
    let podman = PodmanCli {
        program: stalling_podman(&state, stall),
    };
    let mut containers = dagq::infrastructure::run_container::PodmanRunContainers::of_queue(
        podman.clone(),
        db,
        Some(repo.to_path_buf()),
        None,
        Arc::new(QueueRunMachine {
            podman,
            db: db.to_path_buf(),
            main_checkout: Some(repo.to_path_buf()),
            host_wide: None,
            kept_records: None,
            owner_runs: None,
            deadline: None,
        }),
    );
    containers.facts_limit = FACTS_LIMIT;
    containers.memory_poll = Duration::from_millis(20);
    (containers, state.join("calls"))
}

/// Whether `why` says a podman command of the machine was cut off by the
/// facts-only turn's deadline: killed while it ran, or not started as the
/// deadline had passed (a loaded host reaches the hanging start late).
fn cut_off(why: &str) -> bool {
    why.contains("did not end within") || why.contains("its deadline passed")
}

/// The release of the run of [`held_cut_off_run`] through `containers`
/// is refused by the deadline of its facts-only turn, with why (each of
/// `why`) and nothing changed: the hold stays, the task keeps its worker
/// (its provider and its model) and its status, the run its status, and
/// no head is carried over or kept. The run is no user of the machine
/// after, and no container was started.
#[track_caller]
fn refused_by_the_deadline(
    db: &Path,
    repo: &Path,
    run: &ContainerRun,
    containers: &dagq::infrastructure::run_container::PodmanRunContainers,
    why: &[&str],
) {
    let mut queue = SqliteQueue::open(db).unwrap();
    let before = queue.show(run.task).unwrap();
    let started = Instant::now();
    let waiting = crate::common::within(
        FACTS_LIMIT + LATE,
        "the release refused by the facts-only turn's deadline",
    );
    let refused = format!("{:#}", release_with(db, containers).unwrap_err());
    drop(waiting);
    let took = started.elapsed();
    assert!(took < FACTS_LIMIT + LATE, "{took:?}: {refused}");
    for part in [
        "the hold stays",
        "the facts-only turn that would write them anew did not run",
        "make the machine and the image ready",
    ]
    .iter()
    .chain(why)
    {
        assert!(refused.contains(part), "{part}: {refused}");
    }
    assert!(
        cut_off(&refused) || refused.contains("was held by another until the deadline"),
        "{refused}"
    );
    let detail = queue.show(run.task).unwrap();
    assert!(queue.task_hold(run.task).unwrap().is_some());
    assert_eq!(detail.task.worker(), before.task.worker());
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(detail.runs.len(), 1);
    assert_eq!(detail.runs[0].status(), RunStatus::Interrupted);
    assert!(payloads(&detail, "task_released").is_empty());
    assert_eq!(kept_ref(repo, run), None);
    assert_eq!(run_state(run).turn_ended, Some(false));
    assert!(
        !machine_dir(db)
            .join("users")
            .join(run.run.id().as_str())
            .exists(),
        "the run is left among the machine's users"
    );
}

/// Acceptance (a) and (d): a cut-off container run whose facts-only turn
/// hangs in its readying (the machine's start) is refused its release by
/// the turn's deadline, the hanging command killed: it leaves no lease of
/// the machine and started no container.
#[test]
fn a_facts_only_turn_that_hangs_in_its_readying_is_refused_by_its_deadline() {
    let (_dir, repo, db, _fake) = container_queue();
    let (run, _latest) = held_cut_off_run(&db, &repo, true);
    let (containers, calls) = stalling_containers(&db, &repo, Stall::MachineStart);
    refused_by_the_deadline(
        &db,
        &repo,
        &run,
        &containers,
        &["make the dagq machine run"],
    );
    let calls = fs::read_to_string(calls).unwrap();
    assert!(!calls.contains("dagq run"), "{calls}");
}

/// Acceptance (c): while another holds the dagq machine's host-wide lock
/// and does not let it go, a facts-only turn does not wait on for it: the
/// release is refused by the turn's deadline, no podman command of the
/// machine runs, and the holder keeps the lock all along.
#[test]
fn a_facts_only_turn_does_not_wait_on_for_a_machine_lock_another_holds() {
    use dagq::application::podman_machine::MachineUsers;
    let (_dir, repo, db, _fake) = container_queue();
    let (run, _latest) = held_cut_off_run(&db, &repo, true);
    let users = dagq::infrastructure::podman_machine::HostMachineUsers {
        dir: machine_dir(&db),
        runs: None,
    };
    let held = users.lock().unwrap();
    let (containers, calls) = stalling_containers(&db, &repo, Stall::Nothing);
    refused_by_the_deadline(
        &db,
        &repo,
        &run,
        &containers,
        &["was held by another until the deadline"],
    );
    assert!(!calls.exists(), "{:?}", fs::read_to_string(&calls));
    assert!(users.try_lock().unwrap().is_none(), "the holder lost it");
    drop(held);
    assert!(users.try_lock().unwrap().is_some());
}

/// The run of [`triage_cut_off_run`] triaged with `action` through the
/// containers of [`stalling_containers`] hanging at `stall`, the machine's
/// lock held by another throughout when `lock_held`: the triage's pass
/// ends long before the hanging command would, the facts written by no
/// turn, and repairs nothing (no new run, the task not readied); the
/// triage's finish.
fn triaged_by_the_deadline(action: &str, stall: Stall, lock_held: bool) -> Value {
    use dagq::application::podman_machine::MachineUsers;
    let held = std::cell::RefCell::new(None);
    let started = Instant::now();
    // The hanging command waits far longer, and a retry_inherit reads the
    // run's work twice.
    let limit = 2 * FACTS_LIMIT + LATE * 2;
    let waiting = crate::common::within(
        limit,
        "the triage's supervisor pass to end by the facts-only turns' deadlines",
    );
    let (events, detail, _latest, run, _dir, _repo, db) =
        triage_cut_off_run(action, |db, repo, _| {
            if lock_held {
                let users = dagq::infrastructure::podman_machine::HostMachineUsers {
                    dir: machine_dir(db),
                    runs: None,
                };
                *held.borrow_mut() = Some((users.lock().unwrap(), users));
            }
            stalling_containers(db, repo, stall).0
        });
    drop(waiting);
    let took = started.elapsed();
    assert!(took < limit, "{took:?}");
    if let Some((_held, users)) = held.borrow().as_ref() {
        assert!(users.try_lock().unwrap().is_none(), "the holder lost it");
    }
    assert_eq!(detail.runs.len(), 1);
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(run_state(&run).turn_ended, Some(false));
    assert!(
        !machine_dir(&db)
            .join("users")
            .join(run.run.id().as_str())
            .exists()
    );
    let finished = of_kind(&events, "triage_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_ne!(finished[0]["action"], action, "{finished:?}");
    finished[0].clone()
}

/// Acceptance (b): the triage of a cut-off container run whose facts-only
/// turn hangs in the machine's start ends by the turn's deadline and does
/// not retry it from scratch: whether its branch holds commits of its own
/// cannot be read, and the cut-off turn's commit is not thrown away.
#[test]
fn the_triage_of_a_cut_off_container_run_whose_facts_only_turn_hangs_retries_nothing() {
    let finished = triaged_by_the_deadline("retry", Stall::MachineStart, false).to_string();
    for part in [
        "whether its branch holds commits could not be read",
        "make the dagq machine run",
    ] {
        assert!(finished.contains(part), "{part}: {finished}");
    }
    assert!(cut_off(&finished), "{finished}");
}

/// Acceptance (c): so does the triage whose `retry_inherit` would carry
/// the cut-off turn over while another holds the machine's lock: neither
/// its own commits nor its carried head (`inherited_head`) are read, and
/// nothing is carried over.
#[test]
fn the_triage_of_a_cut_off_container_run_carries_nothing_over_while_another_holds_the_machine_lock()
{
    let finished = triaged_by_the_deadline("retry_inherit", Stall::Nothing, true);
    assert!(
        finished
            .to_string()
            .contains("was held by another until the deadline"),
        "{finished}"
    );
}

/// A facts-only turn of the run of [`held_cut_off_run`] in the fake
/// podman's containers, given [`FACTS_LIMIT`], while `blocker` is in the
/// fake's state: it is stopped by its deadline, its container removed and
/// its memory released within the cleanup's limit, and the release is
/// refused as for any facts-only turn that did not run.
fn stopped_past_its_deadline(blocker: &str) {
    let (dir, repo, db, mut fake) = container_queue();
    fake.facts_limit = FACTS_LIMIT;
    let (run, _latest) = held_cut_off_run(&db, &repo, true);
    let state = dir.path().join("podman");
    fs::write(state.join(blocker), "").unwrap();
    let started = Instant::now();
    let waiting = crate::common::within(
        FACTS_LIMIT + LATE,
        "the release refused by the facts-only turn's deadline",
    );
    let refused = format!("{:#}", release_with(&db, &fake).unwrap_err());
    drop(waiting);
    let took = started.elapsed();
    assert!(took < FACTS_LIMIT + LATE, "{took:?}: {refused}");
    assert!(
        refused.contains("the facts-only turn that would write them anew did not run"),
        "{refused}"
    );
    let left: Vec<_> = fs::read_dir(state.join("containers"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert!(left.is_empty(), "{left:?}");
    let memory = containers::memory_calls(&state);
    assert!(
        memory
            .last()
            .is_some_and(|call| call.starts_with("release ")),
        "{memory:?}"
    );
    assert!(
        SqliteQueue::open(&db)
            .unwrap()
            .task_hold(run.task)
            .unwrap()
            .is_some()
    );
}

/// Acceptance (d): a facts-only turn whose container does not end (its
/// `podman run` hangs) leaves no container and no reservation.
#[test]
fn a_facts_only_turn_whose_container_hangs_past_its_deadline_leaves_nothing() {
    stopped_past_its_deadline("run-hangs");
}

/// Acceptance (d): nor does one whose memory never fits.
#[test]
fn a_facts_only_turn_whose_memory_waits_past_its_deadline_leaves_nothing() {
    stopped_past_its_deadline(containers::MEMORY_FULL);
}
