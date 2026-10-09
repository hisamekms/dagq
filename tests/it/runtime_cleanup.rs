//! Runtime tests: the cleanup of ended runs' worktrees off the supervisor's
//! loop, the branches of worktrees already gone, and worktrees a rebind
//! left pointing at an old repository (task 405), the runners of ended
//! runs (task 696), the build outputs of the runs left waiting for an
//! answer, a landing or a resume (task 1289), and the temporary files
//! directories of the runs whose task is over (task 1290).
use crate::{common, runtime_support};
use dagq::infrastructure::git_binary::git_executable;

use dagq::domain::disk::DiskConfig;
use dagq::{application::ProcessControl, infrastructure::adapters::SystemProcesses};
use dagq::{application::RunFiles, runtime::RunFilesPort};
use runtime_support::*;
use std::{
    io,
    sync::Condvar,
    sync::atomic::{AtomicBool, AtomicUsize},
};

/// A worker script that commits, leaves build outputs in its worktree and
/// fails: its receipt names the base commit, which validation refuses
/// (a headless turn that writes its receipt and then exits non-zero may be
/// validated before its failure is seen).
const BUILDING_AGENT: &str = "commit work; mkdir -p target/debug; \
     head -c 65536 /dev/zero > target/debug/big; receipt \"$BASE\"";

/// Supervisor options that sweep the ended runs on every pass.
fn sweeping_options() -> SuperviseOptions {
    SuperviseOptions {
        sweep_interval: Duration::ZERO,
        ..supervise_options(4, true)
    }
}

/// The payloads of a run's events of `kind`.
fn payloads_of(queue: &SqliteQueue, run: &TaskRun, kind: &str) -> Vec<Value> {
    queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.payload)
        .collect()
}

/// Whether the branch exists in `repo`.
fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new(git_executable().expect("git executable"))
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .bounded_output()
        .unwrap()
        .status
        .success()
}

/// The first measure of a `target/` under a run worktree waits here until
/// the test opens the gate: a slow cleanup.
#[derive(Default)]
struct Gate {
    /// The directory whose measure waits (or waited).
    held: Option<PathBuf>,
    open: bool,
}

/// The local run files, but for the [`Gate`] on measuring build outputs:
/// the first one, or only `only`'s; `then` gates a later one the same way
/// (not the measure held here).
#[derive(Clone, Default)]
struct GatedFiles {
    gate: Arc<(Mutex<Gate>, Condvar)>,
    only: Option<PathBuf>,
    then: Option<Box<GatedFiles>>,
}

impl GatedFiles {
    /// Wait for a measure to be held; the directory.
    fn held(&self) -> PathBuf {
        let (lock, changed) = &*self.gate;
        let gate = lock.lock().unwrap();
        let (gate, _) = changed
            .wait_timeout_while(gate, common::STEP_LIMIT, |gate| gate.held.is_none())
            .unwrap();
        gate.held.clone().expect("no cleanup reached the gate")
    }
    fn open(&self) {
        let (lock, changed) = &*self.gate;
        lock.lock().unwrap().open = true;
        changed.notify_all();
    }
    fn wait(&self, dir: &Path) {
        if !self.wait_here(dir)
            && let Some(then) = &self.then
        {
            then.wait_here(dir);
        }
    }
    /// Whether this measure was the one held.
    fn wait_here(&self, dir: &Path) -> bool {
        if !dir.ends_with("worktree/target") || self.only.as_ref().is_some_and(|only| only != dir) {
            return false;
        }
        let (lock, changed) = &*self.gate;
        let mut gate = lock.lock().unwrap();
        if gate.held.is_some() {
            return false;
        }
        gate.held = Some(dir.to_owned());
        changed.notify_all();
        let _ = changed
            .wait_timeout_while(gate, common::STEP_LIMIT, |gate| !gate.open)
            .unwrap();
        true
    }
    fn options(&self, options: SuperviseOptions) -> SuperviseOptions {
        SuperviseOptions {
            files: Some(RunFilesPort(Arc::new(self.clone()))),
            ..options
        }
    }
}

impl RunFiles for GatedFiles {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.create_dir_all(dir)
    }
    fn create_new_dir(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.create_new_dir(dir)
    }
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        LocalRunFiles.write(path, contents)
    }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        LocalRunFiles.copy(from, to)
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        LocalRunFiles.read(path)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        LocalRunFiles.read_to_string(path)
    }
    fn modified(&self, path: &Path) -> io::Result<SystemTime> {
        LocalRunFiles.modified(path)
    }
    fn read_stamped(&self, path: &Path) -> Result<Option<(SystemTime, Vec<u8>)>> {
        LocalRunFiles.read_stamped(path)
    }
    fn is_file(&self, path: &Path) -> bool {
        LocalRunFiles.is_file(path)
    }
    fn is_dir(&self, path: &Path) -> bool {
        LocalRunFiles.is_dir(path)
    }
    fn exists(&self, path: &Path) -> bool {
        LocalRunFiles.exists(path)
    }
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        LocalRunFiles.read_dir(dir)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        LocalRunFiles.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        LocalRunFiles.remove_file(path)
    }
    fn tree_size(&self, dir: &Path) -> io::Result<Option<u64>> {
        self.wait(dir);
        LocalRunFiles.tree_size(dir)
    }
    fn remove_dir_all(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.remove_dir_all(dir)
    }
    fn append_line(&self, path: &Path, line: &str) -> io::Result<()> {
        LocalRunFiles.append_line(path, line)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        LocalRunFiles.canonicalize(path)
    }
    fn write_fenced(&self, path: &Path, text: &str, info: &str, body: &Path) -> Result<()> {
        LocalRunFiles.write_fenced(path, text, info, body)
    }
    fn now(&self) -> SystemTime {
        LocalRunFiles.now()
    }
}

/// Task 405: the build outputs of an ended run are measured and removed
/// off the loop. While that cleanup is held, the supervisor claims the
/// next task and watches its run to its end; the cleanup is recorded once
/// it is done, with the payload of task 376, and once only.
#[test]
fn a_slow_cleanup_does_not_hold_up_the_loop() {
    let (_dir, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "second task", &[]);
    }
    let backend = Arc::new(TestWorkspace::new(&db, false, BUILDING_AGENT));
    let files = GatedFiles::default();
    // One slot: task 1 is claimed first, task 2 once its run ended.
    let options = files.options(supervise_options(1, true));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    let held = files.held();
    let first = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    assert_eq!(
        held,
        Path::new(first.worktree_path().unwrap()).join("target")
    );
    // The loop goes on: task 2 is claimed and its run watched to its end
    // while the cleanup of task 1's run is held.
    wait_until(&db, Duration::from_secs(60), |queue| {
        queue
            .show(TaskId::new(2))
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| run.status() == RunStatus::Failed)
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(payloads_of(&queue, &first, "build_outputs_removed").is_empty());
    assert!(held.join("debug/big").is_file());

    files.open();
    let outcome = joined(supervisor, "the supervisor to finish").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(!held.exists());
    let removed = payloads_of(&queue, &first, "build_outputs_removed");
    assert_eq!(
        removed,
        [
            json!({"paths": [held.to_string_lossy()], "bytes": removed[0]["bytes"], "by": "supervisor", "reason": "run_ended"})
        ]
    );
    assert!(removed[0]["bytes"].as_u64().unwrap() >= 65536);
    let second = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(
        payloads_of(&queue, &second, "build_outputs_removed").len(),
        1
    );
    assert!(payloads_of(&queue, &first, "cleanup_failed").is_empty());
}

/// Task 405: a run leased while the cleanup is on another worktree (a
/// claim of it) is left alone when the cleanup reaches it.
#[test]
fn a_run_leased_during_the_cleanup_keeps_its_worktree() {
    let (_dir, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "second task", &[]);
    }
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let runs: Vec<TaskRun> = (1..=2)
        .map(|task| queue.show(TaskId::new(task)).unwrap().runs[0].clone())
        .collect();
    // Task 696: the runners of the ended runs went with their build outputs.
    let runner = |run: &TaskRun| Path::new(run.run_dir().unwrap()).join("runner");
    for run in &runs {
        assert!(!runner(run).exists());
        assert!(
            Path::new(run.run_dir().unwrap())
                .join("receipt.json")
                .is_file()
        );
    }
    // Both build again, and have a runner again.
    for run in &runs {
        let dir = Path::new(run.worktree_path().unwrap()).join("target/debug");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("again"), "x").unwrap();
        fs::write(runner(run), "binary").unwrap();
    }
    let files = GatedFiles::default();
    let options = files.options(sweeping_options());
    let supervisor = {
        let (db, repo) = (db.clone(), repo.clone());
        let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    let held = files.held();
    let (cleaned, leased) = if held.starts_with(runs[0].worktree_path().unwrap()) {
        (&runs[0], &runs[1])
    } else {
        (&runs[1], &runs[0])
    };
    // A live supervisor leases the other run meanwhile.
    Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,'live',?2,unixepoch()+100000)",
            rusqlite::params![leased.id(), std::process::id()],
        )
        .unwrap();
    files.open();
    joined(supervisor, "the supervisor to finish").unwrap();
    assert!(!held.exists());
    assert_eq!(
        payloads_of(&queue, cleaned, "build_outputs_removed").len(),
        2
    );
    let kept = Path::new(leased.worktree_path().unwrap()).join("target/debug/again");
    assert!(kept.is_file());
    // The leased run's runner is kept; the other's is gone.
    assert!(runner(leased).is_file());
    assert!(!runner(cleaned).exists());
    assert_eq!(
        payloads_of(&queue, leased, "build_outputs_removed").len(),
        1
    );
}

/// Task 405: once its task is canceled, a run whose worktree directory is
/// already gone loses the branch left behind, recorded as
/// `worktree_removed` with no bytes.
#[test]
fn a_canceled_task_loses_the_branch_of_a_worktree_already_gone() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let worktree = Path::new(run.worktree_path().unwrap());
    fs::remove_dir_all(worktree).unwrap();
    let branch = run.branch().unwrap();
    assert!(branch_exists(&repo, branch));
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();

    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    assert!(!branch_exists(&repo, branch));
    let removed = payloads_of(&queue, &run, "worktree_removed");
    assert_eq!(
        removed,
        [json!({
            "path": run.worktree_path().unwrap(),
            "branch": branch,
            "bytes": 0,
            "by": "supervisor",
            "reason": "task_canceled",
            "worktree_missing": true,
        })]
    );
    assert!(payloads_of(&queue, &run, "cleanup_failed").is_empty());
}

/// Task 405: a worktree whose `.git` still points at the repository's
/// common directory before the queue was rebound is repaired, then
/// removed with its branch.
#[test]
fn a_worktree_left_pointing_at_an_old_repository_is_repaired_and_removed() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let worktree = Path::new(run.worktree_path().unwrap());
    // As if the repository had moved: the same worktree record, under a
    // common directory that is gone.
    let gitfile = fs::read_to_string(worktree.join(".git")).unwrap();
    let admin = gitfile.trim().strip_prefix("gitdir: ").unwrap();
    let id = Path::new(admin).file_name().unwrap().to_string_lossy();
    fs::write(
        worktree.join(".git"),
        format!("gitdir: /nonexistent/old repository/.git/worktrees/{id}\n"),
    )
    .unwrap();
    let branch = run.branch().unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();

    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    assert!(!worktree.exists());
    assert!(!branch_exists(&repo, branch));
    let removed = payloads_of(&queue, &run, "worktree_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["reason"], "task_canceled");
    assert_eq!(removed[0]["repaired"], true);
    assert!(removed[0]["bytes"].as_u64().unwrap() > 0);
    assert!(payloads_of(&queue, &run, "cleanup_failed").is_empty());
}

/// Task 1587: a worktree Git takes for none (its record gone and its
/// `.git` broken, as 5 were on 2026-10-03) stays while its task goes on;
/// once the task is over, its directory and branch go, recorded as
/// `worktree_removed` with `broken_git`, and no `cleanup_failed`.
#[test]
fn a_worktree_with_a_broken_git_goes_as_a_directory_once_its_task_is_over() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let worktree = Path::new(run.worktree_path().unwrap());
    let gitfile = fs::read_to_string(worktree.join(".git")).unwrap();
    let admin = gitfile.trim().strip_prefix("gitdir: ").unwrap();
    fs::remove_dir_all(admin).unwrap();
    fs::write(worktree.join(".git"), "broken\n").unwrap();
    let branch = run.branch().unwrap();

    // The task goes on: the worktree stays.
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    assert!(worktree.is_dir());
    assert!(branch_exists(&repo, branch));
    assert!(payloads_of(&queue, &run, "worktree_removed").is_empty());

    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    assert!(!worktree.exists());
    assert!(!branch_exists(&repo, branch));
    let removed = payloads_of(&queue, &run, "worktree_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["reason"], "task_canceled");
    assert_eq!(removed[0]["broken_git"], true);
    assert_eq!(removed[0].get("repaired"), None);
    assert!(payloads_of(&queue, &run, "cleanup_failed").is_empty());
}

/// The system's processes, counting the listings of their executables
/// (task 1590).
struct ListingsCounted(Arc<AtomicUsize>);

impl ProcessControl for ListingsCounted {
    fn alive(&self, pid: u32) -> bool {
        SystemProcesses.alive(pid)
    }
    fn terminate(&self, pid: u32) -> Result<()> {
        SystemProcesses.terminate(pid)
    }
    fn interrupt(&self, pid: u32) -> Result<()> {
        SystemProcesses.interrupt(pid)
    }
    fn kill(&self, pid: u32) -> Result<()> {
        SystemProcesses.kill(pid)
    }
    fn kill_group(&self, leader: u32) -> Result<()> {
        SystemProcesses.kill_group(leader)
    }
    fn reap(&self, pid: u32) {
        SystemProcesses.reap(pid)
    }
    fn list(&self) -> Result<Vec<dagq::domain::recovery::ProcessInfo>> {
        SystemProcesses.list()
    }
    fn executables(&self) -> Result<Vec<dagq::domain::disk::ProcessExecutable>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        SystemProcesses.executables()
    }
    fn start_identity(&self, pid: u32) -> Option<String> {
        SystemProcesses.start_identity(pid)
    }
    fn started_at(&self, pid: u32) -> Option<i64> {
        SystemProcesses.started_at(pid)
    }
    fn descendants(&self, pid: u32) -> Vec<u32> {
        SystemProcesses.descendants(pid)
    }
}

/// Kills the pids it holds when the test ends: on its drop, and on a
/// timeout's exit, which skips the drop (`on_timeout`, as `KillOnDrop`).
#[derive(Default)]
struct Detached(Vec<(u32, common::Cleanup)>);

impl Detached {
    fn hold(&mut self, pid: u32) -> u32 {
        let cleanup = common::on_timeout(
            common::STEP_LIMIT,
            format!("kill the detached pid {pid}"),
            move || {
                let _ = SystemProcesses.kill(pid);
            },
        );
        self.0.push((pid, cleanup));
        pid
    }
}

impl Drop for Detached {
    fn drop(&mut self) {
        for (pid, _) in &self.0 {
            let _ = SystemProcesses.kill(*pid);
        }
    }
}

/// Start `command` detached (its parent gone, as a test's child left
/// behind), in `cwd`; its pid. It inherits no actor's or client mode's
/// variables: a `dagq --db` of a worker's environment would refuse its
/// queue and end at once, not run until it is stopped.
fn detached(command: &str, cwd: &Path, pid_file: &Path) -> u32 {
    let status = Command::new("sh")
        .without_actor_env()
        .arg("-c")
        .arg(format!(
            "{command} </dev/null >/dev/null 2>&1 & echo $! > {}",
            shell_path(pid_file)
        ))
        .current_dir(cwd)
        .bounded_status()
        .unwrap();
    assert!(status.success());
    fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// Task 1590: a process running an executable from under an ended run's
/// worktree (as a test's child `dagq` left running from `target/`) is
/// left alone while the task goes on, and stopped before the worktree
/// goes once it is over, recorded in `worktree_removed`; the worktree is
/// not made again after. One only started in the worktree, from an
/// executable elsewhere, runs on. The processes are listed only for the
/// removal, not on a sweep with nothing to remove.
#[test]
fn a_process_running_from_an_ended_runs_worktree_is_stopped_before_it_goes() {
    let (dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let worktree = Path::new(run.worktree_path().unwrap()).to_path_buf();
    // A copy of dagq in the worktree, waiting from elsewhere on a queue of
    // its own that nothing writes to, so it runs until stopped (a copy of
    // a system binary does not run on macOS).
    let idle = dir.path().join("idle.db");
    drop(common::template::queue(&idle));
    let executable = worktree.join("tools").join("dagq");
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_dagq"), &executable).unwrap();
    // Both end by themselves too: the watch after its 30 reads, the
    // sleeper once the test's directory is gone or after 60 seconds.
    let mut started = Detached::default();
    let left = started.hold(detached(
        &format!(
            "{} --db {} watch --timeout 30 --interval 1",
            shell_path(&executable),
            shell_path(&idle)
        ),
        dir.path(),
        &dir.path().join("left.pid"),
    ));
    let sleeper = started.hold(detached(
        &format!(
            "(i=0; while [ -d {} ] && [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done)",
            shell_path(dir.path())
        ),
        &worktree,
        &dir.path().join("sleeper.pid"),
    ));
    let listings = Arc::new(AtomicUsize::new(0));
    let options = SuperviseOptions {
        processes: Some(runtime::ProcessesPort(Arc::new(ListingsCounted(
            listings.clone(),
        )))),
        ..sweeping_options()
    };

    // The task goes on: nothing to remove, nothing listed or stopped.
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert!(worktree.is_dir());
    assert!(pid_alive(left));
    assert_eq!(listings.load(Ordering::SeqCst), 0);

    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert!(!worktree.exists());
    assert!(!pid_alive(left));
    assert!(pid_alive(sleeper));
    assert_eq!(listings.load(Ordering::SeqCst), 1);
    let removed = payloads_of(&queue, &run, "worktree_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    let stopped = removed[0]["stopped_processes"].as_array().unwrap();
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(stopped[0]["pid"], left);
    assert_eq!(
        Path::new(stopped[0]["executable"].as_str().unwrap())
            .canonicalize()
            .ok(),
        None,
        "the executable went with the worktree"
    );
    assert!(
        stopped[0]["executable"]
            .as_str()
            .unwrap()
            .ends_with("/tools/dagq")
    );
    assert!(payloads_of(&queue, &run, "cleanup_failed").is_empty());

    // Nothing is left to make the worktree again; the next sweep lists
    // nothing.
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert!(!worktree.exists());
    assert_eq!(listings.load(Ordering::SeqCst), 1);
}

/// Run files whose removal of a directory under a `raced` directory finds
/// it gone (another cleanup removed it first), and under a `denied` one
/// fails.
struct RacyFiles;

impl RunFiles for RacyFiles {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.create_dir_all(dir)
    }
    fn create_new_dir(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.create_new_dir(dir)
    }
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        LocalRunFiles.write(path, contents)
    }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        LocalRunFiles.copy(from, to)
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        LocalRunFiles.read(path)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        LocalRunFiles.read_to_string(path)
    }
    fn modified(&self, path: &Path) -> io::Result<SystemTime> {
        LocalRunFiles.modified(path)
    }
    fn read_stamped(&self, path: &Path) -> Result<Option<(SystemTime, Vec<u8>)>> {
        LocalRunFiles.read_stamped(path)
    }
    fn is_file(&self, path: &Path) -> bool {
        LocalRunFiles.is_file(path)
    }
    fn is_dir(&self, path: &Path) -> bool {
        LocalRunFiles.is_dir(path)
    }
    fn exists(&self, path: &Path) -> bool {
        LocalRunFiles.exists(path)
    }
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        LocalRunFiles.read_dir(dir)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        LocalRunFiles.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        LocalRunFiles.remove_file(path)
    }
    fn tree_size(&self, dir: &Path) -> io::Result<Option<u64>> {
        LocalRunFiles.tree_size(dir)
    }
    fn remove_dir_all(&self, dir: &Path) -> io::Result<()> {
        match dir
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
        {
            // Someone else removes it between its measure and its removal.
            Some("raced") => {
                LocalRunFiles.remove_dir_all(dir)?;
                Err(io::ErrorKind::NotFound.into())
            }
            Some("denied") => Err(io::ErrorKind::PermissionDenied.into()),
            _ => LocalRunFiles.remove_dir_all(dir),
        }
    }
    fn append_line(&self, path: &Path, line: &str) -> io::Result<()> {
        LocalRunFiles.append_line(path, line)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        LocalRunFiles.canonicalize(path)
    }
    fn write_fenced(&self, path: &Path, text: &str, info: &str, body: &Path) -> Result<()> {
        LocalRunFiles.write_fenced(path, text, info, body)
    }
    fn now(&self) -> SystemTime {
        LocalRunFiles.now()
    }
}

/// Task 1100: a scratchpad another cleanup removed between its measure and
/// its removal is no failure, and one that cannot be removed under a root
/// records `cleanup_failed` while what was removed under another root is
/// recorded as `scratchpad_removed`; a later sweep tries it again.
#[test]
fn a_scratchpad_gone_meanwhile_is_no_failure_and_one_failing_root_keeps_the_others() {
    let (dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let roots: Vec<PathBuf> = ["raced", "denied", "kept"]
        .iter()
        .map(|name| dir.path().join(name))
        .collect();
    let scratchpads: Vec<PathBuf> = roots
        .iter()
        .map(|root| scratchpad_of(root, run.worktree_path().unwrap()))
        .collect();
    for scratchpad in &scratchpads {
        fs::create_dir_all(scratchpad.join("session/scratchpad")).unwrap();
        fs::write(scratchpad.join("session/scratchpad/notes"), "x").unwrap();
    }
    let options = SuperviseOptions {
        files: Some(RunFilesPort(Arc::new(RacyFiles))),
        scratchpad_roots: Some(roots.clone()),
        ..sweeping_options()
    };
    supervise_with(&db, &repo, &backend, &options).unwrap();
    backend.join();

    assert!(!scratchpads[0].exists());
    assert!(scratchpads[1].is_dir());
    assert!(!scratchpads[2].exists());
    let removed = payloads_of(&queue, &run, "scratchpad_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(
        removed[0]["paths"],
        json!([scratchpads[2].to_string_lossy()])
    );
    let failed = payloads_of(&queue, &run, "cleanup_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["path"], scratchpads[1].to_string_lossy().as_ref());
    assert!(
        failed[0]["message"]
            .as_str()
            .unwrap()
            .starts_with("scratchpad "),
        "{failed:?}"
    );

    // Once it can be removed, the next sweep does.
    let options = SuperviseOptions {
        scratchpad_roots: Some(roots),
        ..sweeping_options()
    };
    supervise_with(&db, &repo, &backend, &options).unwrap();
    backend.join();
    assert!(!scratchpads[1].exists());
    let removed = payloads_of(&queue, &run, "scratchpad_removed");
    assert_eq!(removed.len(), 2, "{removed:?}");
    assert_eq!(
        removed[1]["paths"],
        json!([scratchpads[1].to_string_lossy()])
    );
}

/// A run whose review returned `concern`: `awaiting_integration` with no
/// lease and no live session, its `approve_landing` ask open. Its worktree
/// then gets build outputs, and an uncommitted source with `uncommitted`;
/// returns the run and the ask.
fn run_awaiting_an_answer(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    uncommitted: bool,
) -> (TaskRun, dagq::domain::Ask) {
    let reviewer = TestReviewer::new(&[verdict("concern", &["a finding"], "first")]);
    let outcome = supervise_reviewed(db, repo, backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let ask = queue.asks(Default::default()).unwrap()[0].clone();
    assert_eq!(ask.kind, AskKind::ApproveLanding);
    let worktree = Path::new(run.worktree_path().unwrap());
    for dir in ["target/debug", "llvm-cov-target/debug"] {
        fs::create_dir_all(worktree.join(dir)).unwrap();
        fs::write(worktree.join(dir).join("big"), vec![0u8; 65536]).unwrap();
    }
    if uncommitted {
        fs::write(worktree.join("uncommitted.rs"), "fn main() {}\n").unwrap();
    }
    (run, ask)
}

/// What a cleanup must leave of a run whose build outputs it removed.
fn assert_kept_but_the_build_outputs(repo: &Path, run: &TaskRun) {
    let worktree = Path::new(run.worktree_path().unwrap());
    assert!(!worktree.join("target").exists());
    assert!(!worktree.join("llvm-cov-target").exists());
    assert!(worktree.join("uncommitted.rs").is_file());
    assert!(worktree.join("change.txt").is_file());
    assert!(branch_exists(repo, run.branch().unwrap()));
    let run_dir = Path::new(run.run_dir().unwrap());
    assert!(run_dir.join("receipt.json").is_file());
}

/// Task 1289: a run waiting for a person's answer, with no live lease and
/// no live session, is a candidate of the cleanup's usual sweep
/// (`awaiting_answer` and its ask); not while a live lease (a supervisor
/// working on it, or a run waiting in its session, ADR-0071) or a live
/// session holds it. The job's check of the run alone (task 1586) reads
/// it as the list does through every case.
#[test]
fn a_run_waiting_for_an_answer_is_a_candidate_unless_a_lease_or_a_session_holds_it() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let (run, ask) = run_awaiting_an_answer(&db, &repo, &backend, true);
    let queue = SqliteQueue::open(&db).unwrap();
    let raw = Connection::open(&db).unwrap();
    let candidate = |queue: &SqliteQueue| {
        let listed = queue
            .ended_run_worktrees()
            .unwrap()
            .into_iter()
            .find(|w| w.run_id == *run.id());
        assert_eq!(queue.ended_run_worktree(run.id()).unwrap(), listed);
        listed.map(|w| w.cleanup)
    };
    let awaiting = Some(dagq::application::WorktreeCleanup::AwaitingAnswer(ask.id));
    assert_eq!(candidate(&queue), awaiting);
    raw.execute(
        "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,'live',?2,unixepoch()+100000)",
        rusqlite::params![run.id(), std::process::id()],
    )
    .unwrap();
    assert_eq!(candidate(&queue), None);
    raw.execute("DELETE FROM run_leases WHERE run_id=?1", [run.id()])
        .unwrap();
    raw.execute(
        "INSERT OR REPLACE INTO run_processes(run_id,role,pid,heartbeat_at,exited_at) VALUES (?1,'agent',?2,unixepoch()+100000,NULL)",
        rusqlite::params![run.id(), std::process::id()],
    )
    .unwrap();
    assert_eq!(candidate(&queue), None);
    raw.execute(
        "UPDATE run_processes SET exited_at=unixepoch() WHERE run_id=?1 AND role='agent'",
        [run.id()],
    )
    .unwrap();
    assert_eq!(candidate(&queue), awaiting);
}

/// Task 1289: an answer that comes while the cleanup clears the run's
/// build outputs is applied, but the landing waits until the job has
/// passed the run; then the run lands.
#[test]
fn a_landing_waits_for_the_cleanup_of_its_build_outputs() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    // Nothing uncommitted: the landing would defer a dirty worktree.
    let (run, ask) = run_awaiting_an_answer(&db, &repo, &backend, false);
    let files = GatedFiles::default();
    let stop = Arc::new(AtomicBool::new(false));
    let options = files.options(SuperviseOptions {
        stop: stop.clone(),
        ..SuperviseOptions {
            sweep_interval: Duration::ZERO,
            ..supervise_options(4, false)
        }
    });
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            supervise_reviewed_with(&db, &repo, &backend, &TestReviewer::new(&[]), &options)
        })
    };
    let held = files.held();
    assert_eq!(held, Path::new(run.worktree_path().unwrap()).join("target"));
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.answer(ask.id, "land").unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue.read_ask(ask.id).unwrap().closed_at.is_some()
    });
    await_passes(&passes, SOME_PASSES);
    assert!(payloads_of(&queue, &run, "landing_queued").len() == 1);
    assert!(payloads_of(&queue, &run, "integration_started").is_empty());
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
    assert!(queue.run_lease(run.id()).unwrap().is_none());

    files.open();
    wait_until(&db, Duration::from_secs(60), |queue| {
        queue.run(run.id()).unwrap().status() == RunStatus::Integrated
    });
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to finish");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    // The landing began once the job had passed the run (the event is
    // recorded when the loop joins the job, which may come after).
    assert!(!held.exists());
    let removed = payloads_of(&queue, &run, "build_outputs_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["reason"], "awaiting_answer");
}

const GIB: u64 = 1 << 30;

/// The build outputs whose presence makes the skip test's disk short.
static SKIP_TARGET: Mutex<Option<PathBuf>> = Mutex::new(None);

fn short_while_skip_target(_: &Path) -> Option<u64> {
    let target = SKIP_TARGET
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Some(if target.as_ref().is_some_and(|dir| dir.exists()) {
        1
    } else {
        4 * GIB
    })
}

/// The counts of the events that would show a parked run moved on.
fn moved_on_counts(queue: &SqliteQueue, run: &TaskRun) -> Vec<usize> {
    [
        "resume_skipped",
        "resume_started",
        "validation_finished",
        "landing_queued",
        "integration_started",
    ]
    .iter()
    .map(|kind| payloads_of(queue, run, kind).len())
    .collect()
}

/// Task 1289: a parked run an earlier resume already resolved (its clean
/// head rebased onto main, its receipt rewritten, its integrate approved)
/// is not moved on by `skip_resume` while a cleanup for disk space clears
/// its build outputs: it stays `needs_session`, unleased, with no resume,
/// validation or landing begun. Once the job has passed it, it is skipped
/// without a session. The build outputs are ignored, as a
/// repository ignores its `target/`, so the worktree is clean and the
/// guard met is the skip's, not the resume's. The guard is `skip_resume`'s
/// own call of `Cleaning::may_lease` in `supervise::resume`, which goal
/// 100's tasks 1557/1558 move; its decision moves to a unit test with them.
#[test]
fn a_skipped_resume_waits_for_the_cleanup_of_its_build_outputs() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    crate::runtime_resume::unresolved_attempt(&db, &run, &first_landed);
    let resolved = crate::runtime_resume::resolve_in_worktree(&run, &first_landed);
    write_receipt(&run, &resolved, "succeeded", "resolved");
    let info = repo.join(".git/info");
    fs::create_dir_all(&info).unwrap();
    fs::write(info.join("exclude"), "target/\nllvm-cov-target/\n").unwrap();
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let target = worktree.join("target");
    fs::create_dir_all(target.join("debug")).unwrap();
    fs::write(target.join("debug/big"), vec![0u8; 65536]).unwrap();
    assert_eq!(
        git_out(
            &worktree,
            &["status", "--porcelain", "--untracked-files=all"]
        ),
        ""
    );
    // The `approve_landing` ask of its failed review, which the
    // approving integrate left open, is closed: the run waits for no
    // answer, and only a cleanup for disk space clears it.
    let mut queue = SqliteQueue::open(&db).unwrap();
    for ask in queue.asks(Default::default()).unwrap() {
        queue.answer(ask.id, "withdrawn").unwrap();
        queue.close_ask(ask.id).unwrap();
    }
    assert_eq!(
        queue
            .ended_run_worktrees()
            .unwrap()
            .iter()
            .find(|w| w.run_id == *run.id())
            .map(|w| w.cleanup),
        Some(dagq::application::WorktreeCleanup::Idle)
    );
    let before = moved_on_counts(&queue, &run);
    *SKIP_TARGET.lock().unwrap() = Some(target.clone());
    let files = GatedFiles::default();
    let stop = Arc::new(AtomicBool::new(false));
    let options = files.options(SuperviseOptions {
        stop: stop.clone(),
        disk: Some(DiskConfig {
            min_free_bytes: Some(GIB),
            ..DiskConfig::default()
        }),
        free_space: short_while_skip_target,
        ..supervise_options(4, false)
    });
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    assert_eq!(files.held(), target);
    await_passes(&passes, SOME_PASSES);
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::NeedsSession
    );
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert_eq!(moved_on_counts(&queue, &run), before);
    assert!(target.join("debug/big").is_file());

    // Once the job passed it, it is skipped without a session; that the
    // skip then lands is runtime_resume's.
    files.open();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !payloads_of(queue, &run, "resume_skipped").is_empty()
            && !payloads_of(queue, &run, "build_outputs_removed").is_empty()
    });
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to finish").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(!target.exists());
    assert!(runtime_support::headless::resume_launches(&backend).is_empty());
    assert_eq!(
        payloads_of(&queue, &run, "resume_started").len(),
        before[1],
        "a resume began"
    );
    assert_eq!(
        payloads_of(&queue, &run, "resume_skipped"),
        [
            json!({"head": resolved, "main": first_landed, "approved": true, "status": "needs_session"})
        ]
    );
    let removed = payloads_of(&queue, &run, "build_outputs_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["reason"], "disk_space");
}

/// State belongs to the single matrix test below, including under cargo test.
struct DrainDisk {
    db: PathBuf,
    outputs: Vec<PathBuf>,
    enough: bool,
    stop: Arc<AtomicBool>,
    stop_requested: bool,
    stopping_passes: usize,
}

static DRAIN_DISK: Mutex<Option<DrainDisk>> = Mutex::new(None);
static DRAIN_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn drain_free_space(_: &Path) -> Option<u64> {
    DRAIN_READS.fetch_add(1, Ordering::SeqCst);
    let mut state = DRAIN_DISK.lock().unwrap();
    let state = state.as_mut()?;
    // Request stop from a known point in a pass. The next reading follows
    // poll_cleanup(ending=true), including when an empty loop then exits.
    if state.stop_requested {
        state.stop.store(true, Ordering::SeqCst);
        state.stopping_passes += 1;
    }
    let queued = SqliteQueue::open(&state.db)
        .unwrap()
        .latest_event_of("landing_queued")
        .unwrap()
        .is_some();
    Some(
        if queued && (!state.enough || state.outputs.iter().any(|path| path.exists())) {
            1
        } else {
            1 << 40
        },
    )
}

/// Task 648: stop and handoff both finish a disk cleanup, retain a landing
/// lease until the fresh reading, then either land or return it. That an
/// ordinary job stops at its current worktree on a stop or a handoff is
/// `supervise::cleanup`'s unit tests (`only_a_stop_or_a_handoff_ends_the_cleanup`,
/// `ending_keeps_only_the_rest_of_a_cleanup_for_room`) and, through the
/// supervisor, `a_withdrawn_handoff_resumes_the_cleanup`'s replaced handoff.
#[test]
fn draining_finishes_disk_cleanup_before_deciding_a_landing() {
    // A stop and a handoff, each with a disk cleanup: the matrix's
    // decisions (which job ends after its current worktree, and whether the
    // drained landing lands or gives its lease back) are the unit tests' of
    // `supervise::cleanup` and `domain::landing_hold`.
    for (handoff, disk) in [(false, Some(false)), (true, Some(true))] {
        let (_dir, repo, db) = fixture();
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "second cleanup", &[]);
        add_ready_task(&mut queue, "third cleanup", &[]);
        let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
        supervise(&db, &repo, &backend).unwrap();
        backend.join();
        let runs: Vec<_> = (1..=3)
            .map(|id| queue.show(TaskId::new(id)).unwrap().runs[0].clone())
            .collect();
        let outputs: Vec<_> = runs
            .iter()
            .map(|run| {
                let target = Path::new(run.worktree_path().unwrap()).join("target");
                fs::create_dir_all(&target).unwrap();
                fs::write(target.join("left"), vec![0; 4096]).unwrap();
                target
            })
            .collect();
        if disk.is_some() {
            queue
                .transition(TaskId::new(3), TaskAction::Cancel)
                .unwrap();
            add_ready_task(&mut queue, "landing", &[]);
        }
        let files = GatedFiles::default();
        let stop = Arc::new(AtomicBool::new(false));
        *DRAIN_DISK.lock().unwrap() = Some(DrainDisk {
            db: db.clone(),
            outputs: outputs.clone(),
            enough: disk == Some(true),
            stop: stop.clone(),
            stop_requested: false,
            stopping_passes: 0,
        });
        let options = files.options(SuperviseOptions {
            stop: stop.clone(),
            disk: Some(dagq::domain::disk::DiskConfig {
                min_free_bytes: Some(1 << 30),
                ..Default::default()
            }),
            free_space: drain_free_space,
            ..supervise_options(1, false)
        });
        let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
        let supervisor = {
            let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
            thread::spawn(move || {
                let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets acceptance")]);
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
        };
        let held = files.held();
        let landing = disk.map(|_| {
            wait_until(&db, common::STEP_LIMIT, |q| {
                q.latest_event_of("landing_queued").unwrap().is_some()
            });
            queue.show(TaskId::new(4)).unwrap().runs[0].clone()
        });
        // Let check_disk promote the running job before asking to end.
        let reads = DRAIN_READS.load(Ordering::SeqCst);
        wait_until(&db, common::STEP_LIMIT, |_| {
            DRAIN_READS.load(Ordering::SeqCst) >= reads + 3
        });
        if handoff {
            let registration = queue.supervisors().unwrap().pop().unwrap();
            assert!(
                queue
                    .request_handoff(&registration.token, "/next/dagq")
                    .unwrap()
            );
        } else {
            DRAIN_DISK.lock().unwrap().as_mut().unwrap().stop_requested = true;
        }
        // Multiple full passes after the request, while cleanup is gated.
        let reads = DRAIN_READS.load(Ordering::SeqCst);
        if disk.is_some() || handoff {
            wait_until(&db, common::STEP_LIMIT, |_| {
                DRAIN_READS.load(Ordering::SeqCst) >= reads + 4
            });
        } else {
            // With no slots a stop leaves the loop and joins the job.
            wait_until(&db, common::STEP_LIMIT, |_| {
                DRAIN_DISK.lock().unwrap().as_ref().unwrap().stopping_passes >= 2
            });
        }
        assert!(!supervisor.is_finished());
        if let Some(run) = &landing {
            assert!(queue.run_lease(run.id()).unwrap().is_some());
            assert_eq!(
                queue.run(run.id()).unwrap().status(),
                RunStatus::AwaitingIntegration
            );
            assert!(payloads_of(&queue, run, "integration_started").is_empty());
        }
        files.open();
        let outcome = joined(supervisor, "the cleanup and drain to finish").unwrap();
        backend.join();
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        for (run, output) in runs.iter().zip(&outputs) {
            let cleaned = disk.is_some() || *output == held;
            assert_eq!(
                !output.exists(),
                cleaned,
                "handoff={handoff}, disk={disk:?}"
            );
            if disk.is_some() && run.task_id() == TaskId::new(3) {
                assert_eq!(payloads_of(&queue, run, "worktree_removed").len(), 1);
            } else {
                assert_eq!(
                    payloads_of(&queue, run, "build_outputs_removed").len(),
                    if cleaned { 2 } else { 1 }
                );
            }
        }
        if let Some(run) = &landing {
            assert!(queue.run_lease(run.id()).unwrap().is_none());
            assert_eq!(
                queue.run(run.id()).unwrap().status(),
                if disk == Some(true) {
                    RunStatus::Integrated
                } else {
                    RunStatus::AwaitingIntegration
                }
            );
            let repair = queue.latest_event_of("auto_repaired").unwrap().unwrap();
            assert_eq!(repair.payload["repair"], "disk_cleanup");
            assert_eq!(
                repair.payload["detail"]["runs"].as_array().unwrap().len(),
                3
            );
        }
    }
}

/// State of the drain test below of the rest of a cleanup for room.
struct RestDrain {
    db: PathBuf,
    idle_target: PathBuf,
    /// Ask for a handoff on the pass the rest of the cleanup starts, after
    /// the cleanup was polled and before the handoff is read: the reading
    /// once the first job's `auto_repaired` is recorded waits for the rest
    /// to reach its gate, then asks.
    handoff_at_rest: Option<AtRest>,
}

/// [`RestDrain::handoff_at_rest`].
struct AtRest {
    rest: GatedFiles,
    /// With a worktree entry the rest's `git worktree prune`, its last
    /// step, removes: the reading lets the rest go through and asks once
    /// its job finished, after the disk was read short.
    finished: Option<PathBuf>,
}

static REST_DRAIN: Mutex<Option<RestDrain>> = Mutex::new(None);
static REST_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Short once a run waits to land, until the idle run's build outputs go.
fn rest_free_space(_: &Path) -> Option<u64> {
    REST_READS.fetch_add(1, Ordering::SeqCst);
    let state = REST_DRAIN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut state = state;
    let state = state.as_mut()?;
    let queue = SqliteQueue::open(&state.db).unwrap();
    let queued = queue.latest_event_of("landing_queued").unwrap().is_some();
    let short = queued && state.idle_target.exists();
    if state.handoff_at_rest.is_some() && queue.latest_event_of("auto_repaired").unwrap().is_some()
    {
        let at_rest = state.handoff_at_rest.take().unwrap();
        at_rest.rest.held();
        if let Some(entry) = &at_rest.finished {
            at_rest.rest.open();
            let deadline = Instant::now() + common::STEP_LIMIT;
            while entry.exists() {
                assert!(Instant::now() < deadline, "the rest was not pruned");
                thread::sleep(Duration::from_millis(10));
            }
            // The prune is the job's last step: let its thread return.
            // Passing does not depend on it; it makes sure the handoff is
            // read with the job finished.
            thread::sleep(Duration::from_millis(200));
        }
        let registration = queue.supervisors().unwrap().pop().unwrap();
        assert!(
            queue
                .request_handoff(&registration.token, "/next/dagq")
                .unwrap()
        );
    }
    Some(if short { 1 } else { 1 << 40 })
}

/// When the supervisor of the test below is asked to hand off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drain {
    /// While the first job is held.
    Handoff,
    /// Read first on the pass the rest of the cleanup starts.
    HandoffAtRest,
    /// Read first on that pass too, once the rest's job finished and freed
    /// enough after the disk was read short.
    HandoffAfterRest,
}

/// Wait for `count` more readings of the free space: whole passes.
fn rest_passes(db: &Path, count: usize) {
    let reads = REST_READS.load(Ordering::SeqCst);
    wait_until(db, common::STEP_LIMIT, |_| {
        REST_READS.load(Ordering::SeqCst) >= reads + count
    });
}

/// Task 1426: a cleanup for room asked for while an ordinary cleanup runs
/// is taken on by that job, and its rest (`Request::counted`) is not
/// dropped by a handoff: it runs after the job, to its last candidate,
/// removing the build outputs of a run nobody works on that waits for no
/// answer (`build_outputs_removed`, `disk_space`), counted in
/// `auto_repaired` (`disk_cleanup`). The run short of room to land keeps
/// its lease until that rest is done, then lands on the reading after it;
/// so too when the handoff is read first on the pass the rest starts, and
/// when the rest finished between that pass's reading and the handoff:
/// the landing is decided on the next pass's reading, not the one before.
/// The other cases (a stop, still short) differ only in the decisions of
/// `supervise::cleanup::tests::ending_keeps_only_the_rest_of_a_cleanup_for_room`
/// and `domain::landing_hold`'s unit tests.
#[test]
fn draining_runs_the_rest_of_a_cleanup_for_room_before_deciding_a_landing() {
    for drain in [
        Drain::Handoff,
        Drain::HandoffAtRest,
        Drain::HandoffAfterRest,
    ] {
        let (_dir, repo, db) = fixture();
        let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
        let (idle, ask) = run_awaiting_an_answer(&db, &repo, &backend, true);
        let mut queue = SqliteQueue::open(&db).unwrap();
        queue.answer(ask.id, "withdrawn").unwrap();
        queue.close_ask(ask.id).unwrap();
        // An ended run whose task goes on: the ordinary sweep's.
        add_ready_task(&mut queue, "ended", &[]);
        let building = TestWorkspace::new(&db, false, BUILDING_AGENT);
        supervise(&db, &repo, &building).unwrap();
        building.join();
        let ended = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
        assert_eq!(ended.status(), RunStatus::Failed);
        let ended_target = Path::new(ended.worktree_path().unwrap()).join("target");
        fs::create_dir_all(ended_target.join("debug")).unwrap();
        fs::write(ended_target.join("debug/again"), vec![0u8; 4096]).unwrap();
        let idle_target = Path::new(idle.worktree_path().unwrap()).join("target");
        assert!(idle_target.join("debug/big").is_file());
        let idle_head = git_out(&repo, &["rev-parse", idle.branch().unwrap()]);
        assert_eq!(
            queue
                .ended_run_worktree(idle.id())
                .unwrap()
                .unwrap()
                .cleanup,
            dagq::application::WorktreeCleanup::Idle,
        );
        add_ready_task(&mut queue, "landing", &[]);
        let rest = GatedFiles {
            only: Some(idle_target.clone()),
            ..GatedFiles::default()
        };
        // A worktree entry only a cleanup for room prunes.
        let finished = (drain == Drain::HandoffAfterRest).then(|| {
            let stale = repo.with_file_name("stale-worktree");
            git_out(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "stale-worktree",
                    stale.to_str().unwrap(),
                ],
            );
            fs::remove_dir_all(&stale).unwrap();
            let entry = repo.join(".git/worktrees/stale-worktree");
            assert!(entry.is_dir());
            entry
        });
        *REST_DRAIN.lock().unwrap() = Some(RestDrain {
            db: db.clone(),
            idle_target: idle_target.clone(),
            handoff_at_rest: (drain != Drain::Handoff).then(|| AtRest {
                rest: rest.clone(),
                finished,
            }),
        });
        let files = GatedFiles {
            then: Some(Box::new(rest.clone())),
            ..GatedFiles::default()
        };
        let options = files.options(SuperviseOptions {
            disk: Some(DiskConfig {
                min_free_bytes: Some(GIB),
                ..DiskConfig::default()
            }),
            free_space: rest_free_space,
            ..supervise_options(1, false)
        });
        let supervisor = {
            let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
            thread::spawn(move || {
                let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets acceptance")]);
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
        };
        // The sweep's job, with room: the ended run only.
        assert_eq!(files.held(), ended_target);
        wait_until(&db, common::STEP_LIMIT, |q| {
            q.latest_event_of("landing_queued").unwrap().is_some()
        });
        let landing = queue.show(TaskId::new(3)).unwrap().runs[0].clone();
        // Short now: the held job takes on the cleanup for room.
        rest_passes(&db, 3);
        if drain == Drain::Handoff {
            let registration = queue.supervisors().unwrap().pop().unwrap();
            assert!(
                queue
                    .request_handoff(&registration.token, "/next/dagq")
                    .unwrap()
            );
        }
        rest_passes(&db, 4);
        let waits = |queue: &SqliteQueue| {
            assert!(!supervisor.is_finished(), "{drain:?}");
            assert!(queue.run_lease(landing.id()).unwrap().is_some());
            assert_eq!(
                queue.run(landing.id()).unwrap().status(),
                RunStatus::AwaitingIntegration
            );
            assert!(payloads_of(queue, &landing, "integration_started").is_empty());
        };
        waits(&queue);

        // The job ends; the rest starts though the supervisor ends, and
        // the landing waits for it too.
        files.open();
        assert_eq!(rest.held(), idle_target);
        if drain != Drain::Handoff {
            wait_until(&db, common::STEP_LIMIT, |_| {
                REST_DRAIN
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .handoff_at_rest
                    .is_none()
            });
        }
        if drain != Drain::HandoffAfterRest {
            rest_passes(&db, 4);
            waits(&queue);
            assert_eq!(
                payloads_of(&queue, &ended, "build_outputs_removed").len(),
                2
            );
            assert!(idle_target.join("debug/big").is_file());
        }

        rest.open();
        let outcome = joined(supervisor, "the rest of the cleanup and the drain").unwrap();
        backend.join();
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        assert_kept_but_the_build_outputs(&repo, &idle);
        let removed = payloads_of(&queue, &idle, "build_outputs_removed");
        assert_eq!(removed.len(), 1, "{removed:?}");
        assert_eq!(removed[0]["reason"], "disk_space");
        assert_eq!(removed[0]["paths"].as_array().unwrap().len(), 2);
        assert!(removed[0]["bytes"].as_u64().unwrap() >= 2 * 65536);
        assert_eq!(
            git_out(&repo, &["rev-parse", idle.branch().unwrap()]),
            idle_head
        );
        assert!(Path::new(idle.run_dir().unwrap()).is_dir());
        assert_eq!(
            queue.run(idle.id()).unwrap().status(),
            RunStatus::AwaitingIntegration
        );
        let repaired: Vec<Value> = queue
            .all_events()
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "auto_repaired")
            .map(|event| event.payload)
            .collect();
        assert_eq!(repaired.len(), 2, "{repaired:?}");
        assert_eq!(repaired[0]["repair"], "disk_cleanup");
        assert_eq!(repaired[0]["detail"]["runs"], json!([ended.id().as_str()]));
        assert_eq!(repaired[1]["repair"], "disk_cleanup");
        assert_eq!(repaired[1]["bytes"], removed[0]["bytes"]);
        assert_eq!(repaired[1]["detail"]["runs"], json!([idle.id().as_str()]));
        assert!(queue.run_lease(landing.id()).unwrap().is_none());
        assert_eq!(
            queue.run(landing.id()).unwrap().status(),
            RunStatus::Integrated,
            "{drain:?}"
        );
        assert_eq!(outcome["outcome"], "handoff", "{outcome}");
    }
}

/// State of the test below of a handoff withdrawn while draining.
struct Withdrawn {
    db: PathBuf,
    idle_target: PathBuf,
    /// The test made the disk short once a run waits to land.
    short: bool,
    enough: bool,
}

static WITHDRAWN: Mutex<Option<Withdrawn>> = Mutex::new(None);
static WITHDRAWN_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Room until the test makes the disk short: then short once a run waits
/// to land, until the idle run's build outputs go (with `enough`; for ever
/// without).
fn withdrawn_free_space(_: &Path) -> Option<u64> {
    WITHDRAWN_READS.fetch_add(1, Ordering::SeqCst);
    let state = WITHDRAWN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = state.as_ref()?;
    let queued = SqliteQueue::open(&state.db)
        .unwrap()
        .latest_event_of("landing_queued")
        .unwrap()
        .is_some();
    let short = state.short && queued && (!state.enough || state.idle_target.exists());
    Some(if short { 1 } else { 1 << 40 })
}

/// Wait for `count` more readings of the free space: whole passes.
fn withdrawn_passes(db: &Path, count: usize) {
    let reads = WITHDRAWN_READS.load(Ordering::SeqCst);
    wait_until(db, common::STEP_LIMIT, |_| {
        WITHDRAWN_READS.load(Ordering::SeqCst) >= reads + count
    });
}

/// How the handoff of the test below ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Withdrawal {
    /// Withdrawn while the ordinary job is held on its first worktree.
    WhileHeld,
    /// Withdrawn as the supervisor takes it, once the job stopped after its
    /// first worktree.
    AtTake,
    /// Replaced by another binary while the job is held (task 1286).
    Replaced,
}

/// Task 1427: a handoff withdrawn while the supervisor drains brings the
/// cleanup back to normal with the claims. An ordinary job held when the
/// handoff came, whether it went on or stopped after its current worktree,
/// is followed to its last candidate (`build_outputs_removed`,
/// `worktree_removed`). A shortage of room after that runs a cleanup for
/// room before any disk ask: with enough, no ask is opened and the run
/// lands; still short, the ask is opened as before. A handoff replaced by
/// another binary goes on draining: the job stops after its current
/// worktree and nothing more is cleaned before the exec.
#[test]
fn a_withdrawn_handoff_resumes_the_cleanup() {
    // Each way the handoff ends once; whether the room after it is enough
    // is the disk's decision
    // (`supervise::disk::tests::the_rest_of_a_cleanup_for_room_holds_and_asks_nothing_until_it_is_done`),
    // and what a withdrawal does to the cleanup is
    // `supervise::cleanup::tests::a_withdrawn_end_takes_requests_again_and_asks_for_every_ended_run`.
    for (withdrawal, enough) in [
        (Withdrawal::WhileHeld, false),
        (Withdrawal::AtTake, true),
        (Withdrawal::Replaced, true),
    ] {
        let case = format!("{withdrawal:?}, enough={enough}");
        let (_dir, repo, db) = fixture();
        let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
        // A run nobody works on that waits for no answer: only a cleanup
        // for room removes its build outputs.
        let (idle, ask) = run_awaiting_an_answer(&db, &repo, &backend, true);
        let mut queue = SqliteQueue::open(&db).unwrap();
        queue.answer(ask.id, "withdrawn").unwrap();
        queue.close_ask(ask.id).unwrap();
        // Ended runs of the ordinary sweep: one whose task goes on, one
        // whose task is canceled.
        add_ready_task(&mut queue, "ended", &[]);
        add_ready_task(&mut queue, "canceled", &[]);
        let building = TestWorkspace::new(&db, false, BUILDING_AGENT);
        supervise(&db, &repo, &building).unwrap();
        building.join();
        let ended = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
        let canceled = queue.show(TaskId::new(3)).unwrap().runs[0].clone();
        let targets: Vec<PathBuf> = [&ended, &canceled]
            .iter()
            .map(|run| {
                assert_eq!(run.status(), RunStatus::Failed);
                let target = Path::new(run.worktree_path().unwrap()).join("target");
                fs::create_dir_all(target.join("debug")).unwrap();
                fs::write(target.join("debug/again"), vec![0u8; 4096]).unwrap();
                target
            })
            .collect();
        queue
            .transition(TaskId::new(3), TaskAction::Cancel)
            .unwrap();
        let canceled_worktree = PathBuf::from(canceled.worktree_path().unwrap());
        let idle_target = Path::new(idle.worktree_path().unwrap()).join("target");
        assert!(idle_target.join("debug/big").is_file());
        if withdrawal == Withdrawal::AtTake {
            // The take of `/next/dagq` meets its withdrawal: the trigger
            // withdraws the request and skips the take's own update.
            Connection::open(&db)
                .unwrap()
                .execute_batch(
                    "CREATE TRIGGER withdraw_at_take BEFORE UPDATE OF handoff_accepted ON supervisors
                     WHEN NEW.handoff_accepted = 0 AND OLD.handoff_binary = '/next/dagq'
                     BEGIN
                         UPDATE supervisors SET handoff_binary = NULL, handoff_requested_at = NULL
                         WHERE token = OLD.token;
                         SELECT RAISE(IGNORE);
                     END;",
                )
                .unwrap();
        }
        *WITHDRAWN.lock().unwrap() = Some(Withdrawn {
            db: db.clone(),
            idle_target: idle_target.clone(),
            short: false,
            enough,
        });
        let files = GatedFiles::default();
        let stop = Arc::new(AtomicBool::new(false));
        let options = files.options(SuperviseOptions {
            stop: stop.clone(),
            disk: Some(DiskConfig {
                min_free_bytes: Some(GIB),
                ..DiskConfig::default()
            }),
            free_space: withdrawn_free_space,
            ..supervise_options(1, false)
        });
        let supervisor = {
            let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
            thread::spawn(move || {
                let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets acceptance")]);
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
        };
        // The first sweep's ordinary job, held on its first worktree.
        assert_eq!(files.held(), targets[0], "{case}");
        let token = queue.supervisors().unwrap().pop().unwrap().token;
        assert!(queue.request_handoff(&token, "/next/dagq").unwrap());
        // The drain stops the job after its current worktree.
        withdrawn_passes(&db, 4);
        match withdrawal {
            Withdrawal::WhileHeld => {
                assert!(queue.cancel_handoff(&token, "/next/dagq").unwrap());
                withdrawn_passes(&db, 4);
            }
            Withdrawal::Replaced => {
                assert!(queue.request_handoff(&token, "/other/dagq").unwrap());
                withdrawn_passes(&db, 4);
            }
            Withdrawal::AtTake => {}
        }
        assert!(!supervisor.is_finished(), "{case}");
        files.open();

        if withdrawal == Withdrawal::Replaced {
            let outcome = joined(supervisor, "the drain of the replaced handoff").unwrap();
            backend.join();
            assert_eq!(outcome["errors"], json!([]), "{outcome}");
            assert_eq!(outcome["outcome"], "handoff", "{outcome}");
            assert_eq!(outcome["binary"], "/other/dagq", "{outcome}");
            // Still ending: the job stopped after its current worktree and
            // nothing cleaned the rest before the exec.
            assert!(!targets[0].exists());
            assert!(targets[1].is_dir());
            assert!(canceled_worktree.is_dir());
            assert!(payloads_of(&queue, &canceled, "worktree_removed").is_empty());
            assert!(idle_target.join("debug/big").is_file());
            continue;
        }

        // Back to normal: the rest of the ended runs is cleaned.
        wait_until(&db, common::STEP_LIMIT, |queue| {
            payloads_of(queue, &ended, "build_outputs_removed").len() == 2
                && payloads_of(queue, &canceled, "worktree_removed").len() == 1
        });
        assert!(!targets[0].exists(), "{case}");
        assert!(!canceled_worktree.exists(), "{case}");
        assert!(queue.handoff_request(&token).unwrap().is_none(), "{case}");
        assert!(idle_target.join("debug/big").is_file(), "{case}");
        // The job the withdrawal asked for, which finds nothing left, ends
        // before the disk runs short: a cleanup for room asked while an
        // ordinary job runs would ride on it instead.
        withdrawn_passes(&db, 4);

        // Short of room once a run waits to land, after the withdrawal.
        WITHDRAWN.lock().unwrap().as_mut().unwrap().short = true;
        add_ready_task(&mut queue, "landing", &[]);
        let disk_asks = |queue: &SqliteQueue| -> Vec<dagq::domain::Ask> {
            queue
                .asks(dagq::application::AskQuery {
                    all: true,
                    ..Default::default()
                })
                .unwrap()
                .into_iter()
                .filter(|ask| ask.subject.as_deref() == Some("disk"))
                .collect()
        };
        if enough {
            // The claims resumed, and the cleanup for room made room for
            // the landing: no disk ask.
            wait_until(&db, common::STEP_LIMIT, |queue| {
                queue
                    .show(TaskId::new(4))
                    .unwrap()
                    .runs
                    .first()
                    .is_some_and(|run| run.status() == RunStatus::Integrated)
            });
            assert!(disk_asks(&queue).is_empty(), "{case}");
        } else {
            wait_until(&db, common::STEP_LIMIT, |queue| {
                !disk_asks(queue).is_empty()
            });
            let landing = queue.show(TaskId::new(4)).unwrap().runs[0].clone();
            assert_eq!(
                queue.run(landing.id()).unwrap().status(),
                RunStatus::AwaitingIntegration,
                "{case}"
            );
        }
        // Either way the cleanup for room ran first.
        assert_kept_but_the_build_outputs(&repo, &idle);
        let removed = payloads_of(&queue, &idle, "build_outputs_removed");
        assert_eq!(removed.len(), 1, "{case}: {removed:?}");
        assert_eq!(removed[0]["reason"], "disk_space");
        let repair = queue.latest_event_of("auto_repaired").unwrap().unwrap();
        assert_eq!(repair.payload["repair"], "disk_cleanup", "{case}");
        assert_eq!(
            repair.payload["detail"]["runs"],
            json!([idle.id().as_str()]),
            "{case}"
        );
        stop.store(true, Ordering::SeqCst);
        let outcome = joined(supervisor, "the supervisor to stop").unwrap();
        backend.join();
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
    }
}

/// State of the test below of the rest of a cleanup for room in normal
/// operation.
struct Ride {
    db: PathBuf,
    idle_target: PathBuf,
    /// The free bytes once a run waits to land, while the idle run's build
    /// outputs are there.
    during: u64,
    /// The free bytes once they went.
    after: u64,
}

static RIDE: Mutex<Option<Ride>> = Mutex::new(None);
static RIDE_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn ride_free_space(_: &Path) -> Option<u64> {
    RIDE_READS.fetch_add(1, Ordering::SeqCst);
    let state = RIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = state.as_ref()?;
    let queued = SqliteQueue::open(&state.db)
        .unwrap()
        .latest_event_of("landing_queued")
        .unwrap()
        .is_some();
    Some(if !queued {
        1 << 40
    } else if state.idle_target.exists() {
        state.during
    } else {
        state.after
    })
}

/// Wait for `count` more readings of the free space: whole passes.
fn ride_passes(db: &Path, count: usize) {
    let reads = RIDE_READS.load(Ordering::SeqCst);
    wait_until(db, common::STEP_LIMIT, |_| {
        RIDE_READS.load(Ordering::SeqCst) >= reads + count
    });
}

/// Task 1478: while the rest of a cleanup for room another job took on
/// (`Request::counted`) runs, a disk with room for a landing but short of
/// what a claim needs opens no disk ask and records no `claim_held` or
/// `landing_held`: the claim waits for the rest, and the landing, there
/// being room for it, goes on without waiting. Once the rest recorded what
/// it removed, the next reading, still short of a claim, opens the one
/// disk ask and records the claims' hold, and no landing's. The other
/// readings (short of both, room for both, enough after the rest) differ
/// only in
/// `supervise::disk::tests::the_rest_of_a_cleanup_for_room_holds_and_asks_nothing_until_it_is_done`.
#[test]
fn the_rest_of_a_cleanup_for_room_holds_and_asks_nothing_until_it_is_done() {
    // Room for a landing (a gibibyte), short of a claim.
    const LANDING_ROOM: u64 = 4 * GIB;
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let (idle, ask) = run_awaiting_an_answer(&db, &repo, &backend, true);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.answer(ask.id, "withdrawn").unwrap();
    queue.close_ask(ask.id).unwrap();
    // An ended run whose task goes on: the ordinary sweep's.
    add_ready_task(&mut queue, "ended", &[]);
    let building = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &building).unwrap();
    building.join();
    let ended = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(ended.status(), RunStatus::Failed);
    // A measured build: the claim's need follows it (far above
    // LANDING_ROOM), the landing's stays the least (a gibibyte).
    assert!(!payloads_of(&queue, &ended, "build_outputs_removed").is_empty());
    let ended_target = Path::new(ended.worktree_path().unwrap()).join("target");
    fs::create_dir_all(ended_target.join("debug")).unwrap();
    fs::write(ended_target.join("debug/again"), vec![0u8; 4096]).unwrap();
    let idle_target = Path::new(idle.worktree_path().unwrap()).join("target");
    assert!(idle_target.join("debug/big").is_file());
    // The landing's and the claim's changes do not meet the idle run's or
    // each other's: no landing recheck resumes a run.
    let worker = |file: &str| {
        format!(
            "printf '{file}\\n' > {file} && git add {file} && git commit -q -m {file}; \
             receipt \"$(git rev-parse HEAD)\"; idle; await_exit"
        )
    };
    backend.script_for(3, &worker("landing.txt"));
    backend.script_for(4, &worker("claimed.txt"));
    add_ready_task(&mut queue, "landing", &[]);
    // Short of both once the landing is queued; from the rest on, and
    // after it, room for a landing only.
    *RIDE.lock().unwrap() = Some(Ride {
        db: db.clone(),
        idle_target: idle_target.clone(),
        during: 1,
        after: LANDING_ROOM,
    });
    let rest = GatedFiles {
        only: Some(idle_target.clone()),
        ..GatedFiles::default()
    };
    let files = GatedFiles {
        then: Some(Box::new(rest.clone())),
        ..GatedFiles::default()
    };
    let stop = Arc::new(AtomicBool::new(false));
    let options = files.options(SuperviseOptions {
        stop: stop.clone(),
        disk: Some(DiskConfig {
            min_free_bytes: Some(GIB),
            claim_factor: 100_000.0,
            integrate_factor: 1.0,
            ..DiskConfig::default()
        }),
        free_space: ride_free_space,
        ..supervise_options(2, false)
    });
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets acceptance")]);
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
    };
    // The sweep's job, with room: the ended run only.
    assert_eq!(files.held(), ended_target);
    wait_until(&db, common::STEP_LIMIT, |q| {
        q.latest_event_of("landing_queued").unwrap().is_some()
    });
    // Short of both now: the held job takes on the cleanup for room.
    ride_passes(&db, 3);
    files.open();
    // The job ends, and its rest starts and is held on the idle run.
    assert_eq!(rest.held(), idle_target);
    add_ready_task(&mut queue, "claim", &[]);
    RIDE.lock().unwrap().as_mut().unwrap().during = LANDING_ROOM;
    let disk_asks = |queue: &SqliteQueue| -> Vec<dagq::domain::Ask> {
        queue
            .asks(dagq::application::AskQuery {
                all: true,
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .filter(|ask| ask.kind == AskKind::QueueHold && ask.subject.as_deref() == Some("disk"))
            .collect()
    };
    let events = |queue: &SqliteQueue, kind: &str| -> Vec<Value> {
        queue
            .all_events()
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == kind)
            .map(|event| event.payload)
            .filter(|payload| kind != "auto_repaired" || payload["repair"] == "disk_cleanup")
            .collect()
    };
    let integrated = |_: &SqliteQueue, task: i64| {
        SqliteQueue::open(&db)
            .unwrap()
            .show(TaskId::new(task))
            .unwrap()
            .runs
            .iter()
            .any(|run| run.status() == RunStatus::Integrated)
    };
    let claimed = |_: &SqliteQueue| {
        !SqliteQueue::open(&db)
            .unwrap()
            .show(TaskId::new(4))
            .unwrap()
            .runs
            .is_empty()
    };
    // The landing does not wait for the rest; the claim does.
    wait_until(&db, common::STEP_LIMIT, |queue| integrated(queue, 3));
    ride_passes(&db, 4);
    // The rest is still held: nothing is asked for or held.
    assert!(idle_target.join("debug/big").is_file());
    assert!(disk_asks(&queue).is_empty());
    assert!(events(&queue, "claim_held").is_empty());
    assert!(events(&queue, "landing_held").is_empty());
    assert!(!claimed(&queue));

    rest.open();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        events(queue, "auto_repaired").len() >= 2
    });
    let removed = payloads_of(&queue, &idle, "build_outputs_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["reason"], "disk_space");
    let repaired = events(&queue, "auto_repaired");
    assert_eq!(repaired[1]["repair"], "disk_cleanup");
    assert_eq!(repaired[1]["detail"]["runs"], json!([idle.id().as_str()]));
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !disk_asks(queue).is_empty() && !events(queue, "claim_held").is_empty()
    });
    ride_passes(&db, 4);
    let asks = disk_asks(&queue);
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert!(asks[0].is_open());
    let held = events(&queue, "claim_held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["reason"], "disk_space");
    assert!(!claimed(&queue));
    let landings = events(&queue, "landing_held");
    assert!(landings.is_empty(), "{landings:?}");
    assert!(integrated(&queue, 3));
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to stop").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}

/// Task 1636: a drain on a provisioning failure (claiming stops, neither a
/// stop nor a handoff) does not end the cleanup. The ordinary job running
/// when the drain starts is not stopped and goes to its last candidate;
/// a cleanup asked for during the drain (a run that failed) is taken,
/// waits for that job and then runs, while the drain still waits for a
/// run. A drain that ended the cleanup, as a stop or a handoff does, would
/// stop the job after its current worktree and refuse the request.
#[test]
fn a_drain_on_a_provisioning_failure_takes_cleanup_requests_and_lets_the_job_finish() {
    let (_dir, repo, db) = fixture();
    // Two ended runs whose tasks go on (the fixture's task and one more):
    // the ordinary sweep's two candidates.
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "ended", &[]);
    let building = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &building).unwrap();
    building.join();
    let ended: Vec<TaskRun> = [1, 2]
        .map(|task| queue.show(TaskId::new(task)).unwrap().runs[0].clone())
        .into();
    for run in &ended {
        assert_eq!(run.status(), RunStatus::Failed);
        let target = Path::new(run.worktree_path().unwrap()).join("target");
        fs::create_dir_all(target.join("debug")).unwrap();
        fs::write(target.join("debug/again"), vec![0u8; 4096]).unwrap();
    }
    // Two runs that work until the test lets them finish: the first then
    // fails with build outputs, the second lands.
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    backend.script_for(3, &format!("await_file \"$EXIT.go\"; {BUILDING_AGENT}"));
    backend.script_for(4, GATED_AGENT);
    add_ready_task(&mut queue, "failing", &[]);
    add_ready_task(&mut queue, "landing", &[]);
    let files = GatedFiles::default();
    let options = files.options(SuperviseOptions {
        // The first pass's sweep only: a drain sweeps no more anyway.
        sweep_interval: Duration::from_secs(3600),
        ..supervise_options(3, false)
    });
    let options_passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets acceptance")]);
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
    };
    // The sweep's ordinary job is held on its first candidate.
    let held = files.held();
    let target_of = |run: &TaskRun| Path::new(run.worktree_path().unwrap()).join("target");
    assert!(ended.iter().any(|run| target_of(run) == held), "{held:?}");
    let claimed = |queue: &mut SqliteQueue, task: i64| {
        queue.show(TaskId::new(task)).unwrap().runs.first().cloned()
    };
    wait_until(&db, common::STEP_LIMIT, |q| {
        claimed(q, 3).is_some() && claimed(q, 4).is_some()
    });
    // The next claim fails to provision: claiming stops and the supervisor
    // drains while the job is held.
    backend.fail_tasks.lock().unwrap().push(TaskId::new(5));
    add_ready_task(&mut queue, "unprovisioned", &[]);
    wait_until(&db, common::STEP_LIMIT, |q| {
        claimed(q, 5).is_some_and(|run| run.last_error().is_some())
    });
    // The first run fails in the drain (which triages none): a cleanup of
    // its task is asked for as it ends, while the job is held, and waits
    // for it.
    let first = claimed(&mut queue, 3).unwrap();
    fs::write(
        Path::new(first.run_dir().unwrap()).join("exit-requested.go"),
        "",
    )
    .unwrap();
    wait_until(&db, common::STEP_LIMIT, |q| {
        q.run(first.id()).unwrap().status() == RunStatus::Failed
            && q.run_lease(first.id()).unwrap().is_none()
    });
    let passes = options_passes.load(Ordering::SeqCst);
    wait_until(&db, common::STEP_LIMIT, |_| {
        options_passes.load(Ordering::SeqCst) >= passes + 3
    });
    let first_target_dir = Path::new(first.worktree_path().unwrap()).join("target");
    assert!(first_target_dir.join("debug/big").is_file());
    assert!(payloads_of(&queue, &first, "build_outputs_removed").is_empty());

    files.open();
    // The job goes to its last candidate, and the request taken in the
    // drain then runs, while the drain still waits for the second run.
    wait_until(&db, common::STEP_LIMIT, |q| {
        ended
            .iter()
            .all(|run| payloads_of(q, run, "build_outputs_removed").len() == 2)
            && !payloads_of(q, &first, "build_outputs_removed").is_empty()
    });
    assert!(!supervisor.is_finished());
    for run in &ended {
        assert!(!target_of(run).exists(), "{}", run.id());
    }
    // The request's job came after the ordinary one.
    let order = |run: &TaskRun| {
        queue
            .run_events(run.id())
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "build_outputs_removed")
            .map(|e| e.id)
            .max()
            .unwrap()
    };
    assert!(ended.iter().all(|run| order(run) < order(&first)));
    let removed = payloads_of(&queue, &first, "build_outputs_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["reason"], "run_ended", "{removed:?}");
    assert!(!first_target_dir.exists());

    let second = claimed(&mut queue, 4).unwrap();
    fs::write(
        Path::new(second.run_dir().unwrap()).join("exit-requested.go"),
        "",
    )
    .unwrap();
    let error = format!(
        "{:#}",
        joined(supervisor, "the drain on the provisioning failure").unwrap_err()
    );
    backend.join();
    assert!(
        error.contains("injected background launch failure"),
        "{error}"
    );
    assert!(error.contains("claiming stopped"), "{error}");
    assert_eq!(
        queue.run(second.id()).unwrap().status(),
        RunStatus::Integrated
    );
    let failed: Vec<Value> = queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "cleanup_failed")
        .map(|event| event.payload)
        .collect();
    assert!(failed.is_empty(), "{failed:?}");
}

fn half_a_gibibyte(_: &Path) -> Option<u64> {
    Some(GIB / 2)
}

/// Task 1627: a cleanup for room that runs longer than the interval between
/// two of them asks for no other while it runs, and the pass after it ends
/// judges the reading after it: still short of what a claim needs, the one
/// disk ask opens and `claim_held` (`disk_space`) is recorded, with no
/// other cleanup for room in between: the next measure of the run's build
/// outputs (the ordinary sweep's) is held too, which would hold the disk
/// ask were it another cleanup for room. The interval is shortened here; the
/// decisions are the unit tests' of `asks_for_cleanup` and
/// `add_disk_request`.
#[test]
fn the_pass_after_a_long_cleanup_for_room_asks_and_holds() {
    const INTERVAL: Duration = Duration::from_millis(200);
    let (_dir, repo, db) = fixture();
    let building = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &building).unwrap();
    building.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ended = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(ended.status(), RunStatus::Failed);
    let target = Path::new(ended.worktree_path().unwrap()).join("target");
    fs::create_dir_all(target.join("debug")).unwrap();
    fs::write(target.join("debug/again"), vec![0u8; 4096]).unwrap();
    add_ready_task(&mut queue, "second task", &[]);
    let next = GatedFiles::default();
    let files = GatedFiles {
        then: Some(Box::new(next.clone())),
        ..GatedFiles::default()
    };
    let stop = Arc::new(AtomicBool::new(false));
    let options = files.options(SuperviseOptions {
        stop: stop.clone(),
        disk: Some(DiskConfig {
            min_free_bytes: Some(GIB),
            ..DiskConfig::default()
        }),
        free_space: half_a_gibibyte,
        disk_cleanup_interval: INTERVAL,
        ..supervise_options(1, false)
    });
    let passes = options.passes.clone();
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    let disk_asks = |queue: &SqliteQueue| -> Vec<dagq::domain::Ask> {
        queue
            .asks(dagq::application::AskQuery {
                all: true,
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .filter(|ask| ask.kind == AskKind::QueueHold && ask.subject.as_deref() == Some("disk"))
            .collect()
    };
    let events = |queue: &SqliteQueue, kind: &str| -> Vec<Value> {
        queue
            .all_events()
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == kind)
            .map(|event| event.payload)
            .filter(|payload| kind != "auto_repaired" || payload["repair"] == "disk_cleanup")
            .collect()
    };
    // The cleanup for room of the first pass is held well past the
    // interval: nothing is asked for or held meanwhile.
    assert_eq!(files.held(), target);
    let held_at = Instant::now();
    wait_until(&db, common::STEP_LIMIT, |_| {
        held_at.elapsed() > 4 * INTERVAL
    });
    await_passes(&passes, SOME_PASSES);
    assert!(disk_asks(&queue).is_empty());
    assert!(events(&queue, "claim_held").is_empty());
    assert!(queue.show(TaskId::new(2)).unwrap().runs.is_empty());

    files.open();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !disk_asks(queue).is_empty() && !events(queue, "claim_held").is_empty()
    });
    await_passes(&passes, SOME_PASSES);
    let repaired = events(&queue, "auto_repaired");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["detail"]["runs"], json!([ended.id().as_str()]));
    assert!(!target.exists());
    let asks = disk_asks(&queue);
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert!(asks[0].is_open());
    let held = events(&queue, "claim_held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["reason"], "disk_space");
    assert!(events(&queue, "landing_held").is_empty());
    assert!(queue.show(TaskId::new(2)).unwrap().runs.is_empty());
    next.open();
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to stop").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}

/// A worker that builds, asks a `worker_question` and ends its turn; the
/// turn of the answer writes whether its build outputs were still there,
/// builds again and commits. Git ignores `target/`, as the repositories
/// that build do.
const BUILDING_ASKING_AGENT: &str = r#"
mkdir -p "$(git rev-parse --git-common-dir)/info"
printf 'target/\n' >> "$(git rev-parse --git-common-dir)/info/exclude"
case "$PROMPT" in
"answer to ask "*)
  if [ -e target/debug/big ]; then echo kept; else echo rebuilt; fi > built.txt
  mkdir -p target/debug; head -c 65536 /dev/zero > target/debug/big
  git add built.txt
  git commit -q -m built
  receipt "$(git rev-parse HEAD)" ;;
*) mkdir -p target/debug; head -c 65536 /dev/zero > target/debug/big
  "$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Which word?' --cmux /usr/bin/true > /dev/null || exit 70 ;;
esac
"#;

/// The system's processes, but for what runs under the stub session's
/// wrapper (its watchdog's `sleep`), which a real wrapper waiting between
/// turns does not run.
#[derive(Clone, Default)]
struct StubWrapperHidden(Arc<Mutex<Option<u32>>>);

impl ProcessControl for StubWrapperHidden {
    fn alive(&self, pid: u32) -> bool {
        SystemProcesses.alive(pid)
    }
    fn terminate(&self, pid: u32) -> Result<()> {
        SystemProcesses.terminate(pid)
    }
    fn interrupt(&self, pid: u32) -> Result<()> {
        SystemProcesses.interrupt(pid)
    }
    fn kill(&self, pid: u32) -> Result<()> {
        SystemProcesses.kill(pid)
    }
    fn kill_group(&self, leader: u32) -> Result<()> {
        SystemProcesses.kill_group(leader)
    }
    fn reap(&self, pid: u32) {
        SystemProcesses.reap(pid)
    }
    fn list(&self) -> Result<Vec<dagq::domain::recovery::ProcessInfo>> {
        let all = SystemProcesses.list()?;
        let Some(wrapper) = *self.0.lock().unwrap() else {
            return Ok(all);
        };
        let under = dagq::domain::headless_job::descendants(&all, wrapper);
        Ok(all
            .into_iter()
            .filter(|p| !under.contains(&p.pid))
            .collect())
    }
    fn executables(&self) -> Result<Vec<dagq::domain::disk::ProcessExecutable>> {
        SystemProcesses.executables()
    }
    fn start_identity(&self, pid: u32) -> Option<String> {
        SystemProcesses.start_identity(pid)
    }
    fn started_at(&self, pid: u32) -> Option<i64> {
        SystemProcesses.started_at(pid)
    }
    fn descendants(&self, pid: u32) -> Vec<u32> {
        SystemProcesses.descendants(pid)
    }
}

/// Whether the waiting run's test reads the free space as short.
static WAITING_SHORT: AtomicBool = AtomicBool::new(false);

fn short_while_waiting(_: &Path) -> Option<u64> {
    Some(if WAITING_SHORT.load(Ordering::SeqCst) {
        1
    } else {
        4 << 30
    })
}

/// A run the supervisor leases that waits outside its slot for the answer
/// to its `worker_question`, its turn over, loses its build outputs in a
/// cleanup for room: `build_outputs_removed` (`reason: disk_space`, its
/// `ask_id`) on the run, its bytes and the run in `auto_repaired`
/// (`disk_cleanup`); its sources, receipt path and runner stay. The answer
/// that arrives while the job has the run reserved ends no wait, and
/// nothing is delivered nor any turn started, until the job passed it; the
/// worker then builds again and goes on. Which waiting runs a cleanup
/// takes (a turn running or asked for, no ask open, something running for
/// it, an ordinary cleanup) is the unit tests' of `supervise::cleanup`.
#[test]
fn a_waiting_runs_build_outputs_go_for_room_and_its_answer_waits_for_the_cleanup() {
    let (_dir, repo, db) = fixture();
    WAITING_SHORT.store(false, Ordering::SeqCst);
    let backend = Arc::new(TestWorkspace::new(&db, false, BUILDING_ASKING_AGENT));
    let files = GatedFiles::default();
    let processes = StubWrapperHidden::default();
    let options = files.options(SuperviseOptions {
        disk: Some(DiskConfig {
            min_free_bytes: Some(1 << 30),
            ..DiskConfig::default()
        }),
        free_space: short_while_waiting,
        processes: Some(runtime::ProcessesPort(Arc::new(processes.clone()))),
        ..supervise_options(1, true)
    });
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    // The run waits, and the turn that asked is over.
    wait_until(&db, Duration::from_secs(60), |queue| {
        queue
            .show(TaskId::new(1))
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| {
                let kinds: Vec<String> = queue
                    .run_events(run.id())
                    .unwrap()
                    .into_iter()
                    .map(|e| e.kind)
                    .collect();
                kinds.iter().any(|k| k == "run_waiting_started")
                    && kinds
                        .iter()
                        .rfind(|k| {
                            ["turn_requested", "turn_started", "turn_finished"]
                                .contains(&k.as_str())
                        })
                        .is_some_and(|k| k == "turn_finished")
            })
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let ask = queue
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.run_id.as_ref() == Some(run.id()) && ask.kind == AskKind::WorkerQuestion)
        .unwrap()
        .id;
    let target = Path::new(run.worktree_path().unwrap()).join("target");
    let runner = Path::new(run.run_dir().unwrap()).join("runner");
    assert!(target.join("debug/big").is_file());
    *processes.0.lock().unwrap() = queue
        .processes(run.id())
        .unwrap()
        .into_iter()
        .find(|p| p.role == "wrapper")
        .map(|p| p.pid);
    WAITING_SHORT.store(true, Ordering::SeqCst);
    assert_eq!(files.held(), target);
    // Answered while the job holds the run: the wait holds still.
    queue.answer(ask, "blue").unwrap();
    await_passes(&passes, 2);
    assert!(payloads_of(&queue, &run, "run_waiting_ended").is_empty());
    assert!(payloads_of(&queue, &run, "ask_delivered").is_empty());
    assert!(payloads_of(&queue, &run, "turn_requested").is_empty());
    assert!(payloads_of(&queue, &run, "build_outputs_removed").is_empty());

    WAITING_SHORT.store(false, Ordering::SeqCst);
    files.open();
    let outcome = joined(supervisor, "the supervisor to finish").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    // The first; a later cleanup may remove what it built again.
    let removed = payloads_of(&queue, &run, "build_outputs_removed");
    assert!(!removed.is_empty(), "{removed:?}");
    let bytes = removed[0]["bytes"].as_u64().unwrap();
    assert!(bytes >= 65536, "{bytes}");
    assert_eq!(
        removed[0],
        json!({"paths": [target.to_string_lossy()], "bytes": bytes, "by": "supervisor", "reason": "disk_space", "ask_id": ask})
    );
    let repaired: Vec<Value> = queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "auto_repaired" && e.payload["repair"] == "disk_cleanup")
        .map(|e| e.payload)
        .collect();
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["bytes"].as_u64(), Some(bytes));
    assert_eq!(repaired[0]["detail"]["runs"], json!([run.id().as_str()]));
    // The answer went once the build outputs were gone (the job records
    // them once joined, maybe after the wait ended), and the worker built
    // again and went on.
    assert_eq!(payloads_of(&queue, &run, "ask_delivered").len(), 1);
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let worktree = Path::new(run.worktree_path().unwrap());
    assert_eq!(
        fs::read_to_string(worktree.join("built.txt")).unwrap(),
        "rebuilt\n"
    );
    assert!(runner.is_file());
    assert!(
        Path::new(run.run_dir().unwrap())
            .join("receipt.json")
            .is_file()
    );
}
